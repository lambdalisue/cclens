//! SQLite store: persist extracted spans as events and query them back. This
//! layer knows SQL but nothing about raw Claude Code formats. See
//! `docs/specs/storage.md`.
//!
//! The schema tracks the spec's `events` table; `attrs_json` is the one column
//! deferred until a report needs it.

use std::collections::BTreeMap;

use anyhow::Result;
use rusqlite::Connection;

use crate::core::span::{SessionStart, Source, Span};
use crate::core::surface::Surface;
use crate::core::thrash::FileEdit;
use crate::core::usage::UsageEvent;

/// The `meta` key holding the analysis semantics a store's rows were built with.
const ANALYZER_META_KEY: &str = "analyzer_version";

/// The analysis semantics this build produces. **Bump it whenever extraction or
/// classification changes what an unchanged transcript yields** — that is the
/// only way rows behind the incremental-ingest skip get rebuilt. It is not the
/// crate version: a release that changes no analysis should not force everyone
/// through a full re-analyze.
const ANALYZER_VERSION: &str = "1";

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS sessions (
    id              TEXT PRIMARY KEY,
    project         TEXT NOT NULL,
    slug            TEXT NOT NULL,
    root            TEXT NOT NULL DEFAULT '',
    source_path     TEXT NOT NULL,
    started_at      TEXT NOT NULL,
    sub_tokens      INTEGER NOT NULL DEFAULT 0,
    sub_agent_count INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS events (
    id            INTEGER PRIMARY KEY,
    session_id    TEXT NOT NULL,
    source_path   TEXT NOT NULL,
    source_line   INTEGER,
    kind          TEXT NOT NULL,
    surface_kind  TEXT,
    surface_id    TEXT,
    source        TEXT,
    started_at    TEXT NOT NULL,
    started_epoch INTEGER NOT NULL,
    duration_sec  REAL NOT NULL,
    out_tokens    INTEGER NOT NULL,
    ctx_growth    INTEGER NOT NULL,
    ctx_start     INTEGER NOT NULL,
    ctx_peak      INTEGER NOT NULL,
    model         TEXT,
    target        TEXT,
    sub_tokens           INTEGER NOT NULL DEFAULT 0,
    sub_agent_count      INTEGER NOT NULL DEFAULT 0,
    sub_tokens_estimated INTEGER NOT NULL DEFAULT 0,
    is_trailing          INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS surfaces (
    kind          TEXT NOT NULL,
    id            TEXT NOT NULL,
    scope         TEXT NOT NULL,
    project       TEXT NOT NULL DEFAULT '',
    config_path   TEXT,
    static_tokens INTEGER,
    load_mode     TEXT NOT NULL,
    PRIMARY KEY (kind, id, scope, project)
);
-- Analyze-run metadata (analyzed_at, projects_dir, config_dir) so read
-- commands can report freshness and re-run the analysis with the same roots,
-- plus analyzer_version, which invalidates the ingest fingerprints above.
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
-- Incremental-ingest fingerprints: a transcript whose (mtime, size) matches is
-- skipped on re-analyze (see docs/specs/storage.md).
CREATE TABLE IF NOT EXISTS ingested_files (
    path  TEXT PRIMARY KEY,
    mtime INTEGER NOT NULL,
    size  INTEGER NOT NULL
);
";

/// Indexes are declared apart from the tables so `migrate` can apply them only
/// after reconciling columns. Declared alongside the table, an index over a
/// column a future release adds would be replayed against a legacy shape, fail on
/// the missing column, and take the whole migration down with it.
const INDEX_SCHEMA: &str = "
CREATE INDEX events_by_surface ON events(surface_kind, surface_id);
";

/// Views are pure definitions over the tables, declared apart from them for the
/// same reason as indexes plus one of their own: `CREATE VIEW IF NOT EXISTS`
/// would leave an older store's stale definition in place, and a reader would
/// silently query yesterday's column mapping.
const VIEW_SCHEMA: &str = "
-- A clean read view over tool_error events: the friction columns are overloaded
-- onto generic event columns (category in surface_id, excerpt in source, tool in
-- model), so this view names them and joins the project — letting an ad-hoc SQL
-- query (e.g. from the optimize session) ask for any slice without knowing the
-- encoding. `project LIKE '%--wt%'` distinguishes a worktree from the main checkout.
CREATE VIEW tool_errors AS
SELECT e.session_id        AS session_id,
       s.project           AS project,
       e.surface_id        AS category,
       e.source            AS excerpt,
       e.model             AS tool,
       e.target            AS target,
       e.started_epoch      AS started_epoch
FROM events e JOIN sessions s ON e.session_id = s.id
WHERE e.kind = 'tool_error';
";

/// Stamped into `PRAGMA user_version`. Bumped only for a change `migrate` cannot
/// reconcile — an existing column removed, retyped, or given a new meaning.
/// Additive drift (a new column, table, or view) needs no bump: `migrate`
/// converges any older store onto the declared schema in place.
const SCHEMA_VERSION: i64 = 1;

/// Tables holding analysis history. Claude Code prunes transcripts on its own
/// retention schedule, after which these rows are the only surviving record of
/// those sessions — so a shape change adds columns in place, never rebuilds.
const MIGRATED_TABLES: [&str; 4] = ["sessions", "events", "ingested_files", "meta"];

/// Tables regenerated wholesale from live config on every analyze
/// (`replace_surfaces`), so a shape change costs nothing to drop and recreate.
const REBUILT_TABLES: [&str; 1] = ["surfaces"];

/// Identity and provenance of one analyzed session.
pub struct SessionMeta {
    pub id: String,
    pub project: String,
    pub slug: String,
    /// The real directory the session started in (from the transcript's `cwd`,
    /// worktree folded) — empty when no record carried one. This is what
    /// project-config scanning walks; the slug is too lossy to reconstruct it.
    pub root: String,
    pub source_path: String,
    /// Total output tokens across this session's subagent transcripts, and how
    /// many subagents it spawned.
    pub sub_tokens: i64,
    pub sub_agent_count: i64,
    /// When this session started and the context it started with, when the
    /// transcript carried an assistant record to read it from — the only honest
    /// always-on-floor observation (`core::span::session_start`). Stored as a
    /// `session_start` event so `overhead` can distinguish "no observation" from
    /// a fabricated zero.
    pub start: Option<SessionStart>,
}

/// One row of the per-skill usage rollup. Subagent cost is deliberately not
/// rolled up per skill: under the flat-span model a skill's window can absorb
/// subagents spawned by later same-window work, so a per-skill figure
/// over-counts for skills that do not themselves spawn agents. The exact figure
/// is the session-level total (`subagent_totals`). See `docs/specs/events.md`.
#[derive(Debug, PartialEq, serde::Serialize)]
pub struct SkillUsage {
    pub skill: String,
    pub invocations: i64,
    pub out_tokens: i64,
    pub ctx_growth: i64,
    pub duration_sec: f64,
}

/// One catalogued surface row with its **effective** usage: the invocations
/// attributed to this row under per-project shadowing (a project row absorbs
/// its own project's events; the global row keeps everyone else's). See
/// `docs/specs/surfaces.md`.
#[derive(Debug, PartialEq, serde::Serialize)]
pub struct CatalogEntry {
    pub kind: String,
    pub id: String,
    /// `global` | `project`.
    pub scope: String,
    /// The owning project's normalized slug; empty for global rows.
    pub project: String,
    pub static_tokens: Option<i64>,
    pub load_mode: String,
    pub uses: i64,
}

/// One skill event's cost, with its UTC start, for time bucketing.
#[derive(Debug, PartialEq, serde::Serialize)]
pub struct EventCost {
    pub started_epoch: i64,
    pub out_tokens: i64,
    pub ctx_growth: i64,
    pub duration_sec: f64,
}

pub struct Store {
    conn: Connection,
}

impl Store {
    /// Open (creating if needed) a store at `path`, ensuring the schema exists.
    pub fn open(path: impl AsRef<std::path::Path>) -> Result<Self> {
        Self::from_connection(Connection::open(path)?)
    }

    /// Open an existing store **read-only**, without touching the schema. For
    /// the `sql` command, where an ad-hoc query must never mutate the derived
    /// store (and an absent db is an error, not a fresh empty one).
    pub fn open_readonly(path: impl AsRef<std::path::Path>) -> Result<Self> {
        let conn = Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        Ok(Self { conn })
    }

    /// An ephemeral in-memory store, for tests.
    pub fn in_memory() -> Result<Self> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    /// Run an arbitrary read query, returning `(column names, rows)` with every
    /// cell stringified. Powers the `sql` command — the store's own query
    /// surface, so the analyzed data is reachable for any slice without
    /// re-parsing transcripts. Opened read-only by the caller; a non-read
    /// statement simply errors at SQLite.
    pub fn query(&self, sql: &str) -> Result<(Vec<String>, Vec<Vec<String>>)> {
        let mut stmt = self.conn.prepare(sql)?;
        let columns: Vec<String> = stmt.column_names().iter().map(|c| c.to_string()).collect();
        let ncol = columns.len();
        let rows = stmt
            .query_map([], |row| {
                (0..ncol)
                    .map(|i| Ok(value_to_string(row.get_ref(i)?)))
                    .collect::<rusqlite::Result<Vec<_>>>()
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok((columns, rows))
    }

    /// Like `query`, but with SQLite's types preserved as JSON values — the
    /// machine half of the output contract (`--format json`): integers and
    /// reals stay numbers, NULL stays null, so a consumer never re-parses
    /// strings.
    pub fn query_json(&self, sql: &str) -> Result<(Vec<String>, Vec<Vec<serde_json::Value>>)> {
        let mut stmt = self.conn.prepare(sql)?;
        let columns: Vec<String> = stmt.column_names().iter().map(|c| c.to_string()).collect();
        let ncol = columns.len();
        let rows = stmt
            .query_map([], |row| {
                (0..ncol)
                    .map(|i| Ok(value_to_json(row.get_ref(i)?)))
                    .collect::<rusqlite::Result<Vec<_>>>()
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok((columns, rows))
    }

    fn from_connection(mut conn: Connection) -> Result<Self> {
        migrate(&mut conn)?;
        Ok(Self { conn })
    }

    /// Replace a session's events with a freshly-extracted set (idempotent
    /// re-ingest keyed on `source_path`; see `docs/specs/storage.md`). `spans`
    /// are skill executions with cost; `usage` are point events (agent spawns,
    /// MCP tool calls) counted for the catalog join.
    pub fn ingest_session(
        &mut self,
        session: &SessionMeta,
        spans: &[Span],
        usage: &[UsageEvent],
    ) -> Result<()> {
        // The session's first assistant record precedes any span, so it is the
        // better start when present; the earliest span is the fallback for a
        // transcript that carried no assistant record.
        let started_epoch_ms = session
            .start
            .map(|start| start.timestamp_ms)
            .into_iter()
            .chain(spans.iter().map(|span| span.started_epoch_ms))
            .min();
        let started_at = started_epoch_ms
            .map(epoch_ms_to_rfc3339)
            .unwrap_or_default();

        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT OR REPLACE INTO sessions
               (id, project, slug, root, source_path, started_at, sub_tokens, sub_agent_count)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            (
                &session.id,
                &session.project,
                &session.slug,
                &session.root,
                &session.source_path,
                &started_at,
                session.sub_tokens,
                session.sub_agent_count,
            ),
        )?;
        tx.execute(
            "DELETE FROM events WHERE source_path = ?1",
            (&session.source_path,),
        )?;
        // One `session_start` event per session that carried an observable
        // start context — the always-on floor `overhead` reports. It is stamped
        // with when the session began, not left at the epoch: `sql` exposes raw
        // event rows, and a 1970 timestamp there is worse than no row at all.
        if let Some(start) = session.start {
            tx.execute(
                "INSERT INTO events
                   (session_id, source_path, kind, started_at, started_epoch,
                    duration_sec, out_tokens, ctx_growth, ctx_start, ctx_peak)
                 VALUES (?1, ?2, 'session_start', ?3, ?4, 0, 0, 0, ?5, ?5)",
                (
                    &session.id,
                    &session.source_path,
                    epoch_ms_to_rfc3339(start.timestamp_ms),
                    start.timestamp_ms / 1000,
                    start.ctx,
                ),
            )?;
        }
        for span in spans {
            tx.execute(
                "INSERT INTO events
                   (session_id, source_path, kind, surface_kind, surface_id, source,
                    started_at, started_epoch, duration_sec, out_tokens, ctx_growth,
                    ctx_start, ctx_peak, model,
                    sub_tokens, sub_agent_count, sub_tokens_estimated, is_trailing)
                 VALUES (?1, ?2, 'skill_invocation', 'skill', ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
                (
                    &session.id,
                    &session.source_path,
                    &span.skill,
                    source_label(span.source),
                    epoch_ms_to_rfc3339(span.started_epoch_ms),
                    span.started_epoch_ms / 1000,
                    span.duration_sec,
                    span.out_tokens,
                    span.ctx_growth,
                    span.ctx_start,
                    span.ctx_peak,
                    &span.model,
                    span.sub_tokens,
                    span.sub_agent_count,
                    span.sub_tokens_estimated as i64,
                    span.is_trailing as i64,
                ),
            )?;
        }
        for event in usage {
            let kind = if event.surface_kind == "agent" {
                "agent_spawn"
            } else {
                "tool_use"
            };
            tx.execute(
                "INSERT INTO events
                   (session_id, source_path, kind, surface_kind, surface_id, source,
                    started_at, started_epoch, duration_sec, out_tokens, ctx_growth,
                    ctx_start, ctx_peak, model)
                 VALUES (?1, ?2, ?3, ?4, ?5, NULL, ?6, ?7, 0, 0, 0, 0, 0, NULL)",
                (
                    &session.id,
                    &session.source_path,
                    kind,
                    &event.surface_kind,
                    &event.surface_id,
                    epoch_ms_to_rfc3339(event.started_epoch_ms),
                    event.started_epoch_ms / 1000,
                ),
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Insert prompt-pointer events for a session: `(source_line, epoch_ms)`
    /// per user prompt. The text is not stored — only the pointer
    /// (`source_path` with `source_line`) so it can be re-read later
    /// (`docs/specs/storage.md`). Call after `ingest_session`, whose
    /// delete-by-`source_path` already cleared any prior prompt rows for this file.
    pub fn ingest_prompts(
        &mut self,
        session_id: &str,
        source_path: &str,
        prompts: &[(usize, i64, &str)],
    ) -> Result<()> {
        let tx = self.conn.transaction()?;
        for (line_no, epoch_ms, behavior) in prompts {
            // The prompt's behavioral class rides in the unused `source` column.
            tx.execute(
                "INSERT INTO events
                   (session_id, source_path, source_line, kind, source,
                    started_at, started_epoch, duration_sec, out_tokens, ctx_growth,
                    ctx_start, ctx_peak)
                 VALUES (?1, ?2, ?3, 'prompt', ?4, '', ?5, 0, 0, 0, 0, 0)",
                (
                    session_id,
                    source_path,
                    *line_no as i64,
                    *behavior,
                    epoch_ms / 1000,
                ),
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Insert tool-failure events for a session: `(epoch_ms, category, excerpt,
    /// tool, target)`. For a `tool_error` row the otherwise-unused detail columns
    /// carry the friction signal: `surface_id` = category, `source` = a short
    /// excerpt of the error text, `model` = the originating tool, `target` = the
    /// call's subject (file_path / command). `surface_kind` stays NULL so these
    /// never enter the surface-catalog join. The excerpt, tool, and target let a
    /// report show concrete instances, which tool produced them, and which file
    /// or command they hit — without re-reading the transcript. Call after
    /// `ingest_session`, whose delete-by-`source_path` already cleared prior rows.
    pub fn ingest_tool_errors(
        &mut self,
        session_id: &str,
        source_path: &str,
        errors: &[(i64, &str, &str, &str, &str)],
    ) -> Result<()> {
        let tx = self.conn.transaction()?;
        for (epoch_ms, category, excerpt, tool, target) in errors {
            tx.execute(
                "INSERT INTO events
                   (session_id, source_path, kind, surface_id, source, model, target,
                    started_at, started_epoch, duration_sec, out_tokens, ctx_growth,
                    ctx_start, ctx_peak)
                 VALUES (?1, ?2, 'tool_error', ?3, ?4, ?5, ?6, '', ?7, 0, 0, 0, 0, 0)",
                (
                    session_id,
                    source_path,
                    *category,
                    *excerpt,
                    *tool,
                    *target,
                    epoch_ms / 1000,
                ),
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Insert work events `(epoch_ms, kind, id)` — Bash leading words and edited
    /// file basenames — kept out of the catalog join (`surface_kind` NULL).
    /// Persist work events as `(epoch_ms, kind, id, path)`. A file edit's full
    /// path goes to `target` — the generic detail column
    /// (`docs/specs/storage.md`) — while `surface_id` keeps the basename that
    /// hotspot rankings group on.
    pub fn ingest_work_events(
        &mut self,
        session_id: &str,
        source_path: &str,
        events: &[(i64, &str, &str, Option<&str>)],
    ) -> Result<()> {
        let tx = self.conn.transaction()?;
        for (epoch_ms, kind, id, path) in events {
            tx.execute(
                "INSERT INTO events
                   (session_id, source_path, kind, surface_id, target,
                    started_at, started_epoch, duration_sec, out_tokens, ctx_growth,
                    ctx_start, ctx_peak)
                 VALUES (?1, ?2, ?3, ?4, ?5, '', ?6, 0, 0, 0, 0, 0)",
                (session_id, source_path, kind, id, path, epoch_ms / 1000),
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Every file edit as a `FileEdit`, time-ordered — the input to thrash
    /// detection, which needs the session and full path to tell one agent's
    /// retries apart from parallel work on a same-named file (`core::thrash`).
    ///
    /// Rows without a recorded path are **excluded**, not fallen back to the
    /// basename in `surface_id`: grouping same-named files together is the bug
    /// this query exists to avoid, and a silent fallback would reproduce it for
    /// every store written before paths were recorded — invisibly, since the
    /// output would look normal. An empty result surfaces as "no thrash
    /// episodes", which tells the user to re-analyze.
    pub fn file_edits(&self) -> Result<Vec<FileEdit>> {
        let mut stmt = self.conn.prepare(
            "SELECT s.project, e.session_id, e.target, e.started_epoch
             FROM events e JOIN sessions s ON e.session_id = s.id
             WHERE e.kind = 'file_edit' AND e.target IS NOT NULL
             ORDER BY e.started_epoch",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok(FileEdit {
                    project: row.get(0)?,
                    session_id: row.get(1)?,
                    path: row.get(2)?,
                    epoch: row.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Counts of a work-event kind by id (e.g. Bash leading word, edited file),
    /// most frequent first.
    pub fn work_counts(&self, kind: &str) -> Result<Vec<(String, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT surface_id, COUNT(*) FROM events WHERE kind = ?1
             GROUP BY surface_id ORDER BY COUNT(*) DESC",
        )?;
        let rows = stmt
            .query_map([kind], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Counts of tool failures by category, most frequent first.
    pub fn error_counts(&self) -> Result<Vec<(String, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT surface_id, COUNT(*) FROM events WHERE kind = 'tool_error'
             GROUP BY surface_id ORDER BY COUNT(*) DESC",
        )?;
        let rows = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Tool-failure counts split by project and category, densest pair first.
    /// Joins each error event back to its session's project so a friction
    /// category can be attributed to the project whose config should carry the
    /// fix — backing a `--project` filter and the dominant-project line in the
    /// summary.
    pub fn error_counts_by_project(&self) -> Result<Vec<(String, String, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT s.project, e.surface_id, COUNT(*)
             FROM events e JOIN sessions s ON e.session_id = s.id
             WHERE e.kind = 'tool_error'
             GROUP BY s.project, e.surface_id
             ORDER BY COUNT(*) DESC, s.project, e.surface_id",
        )?;
        let rows = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Up to `per_group` example error excerpts for each `(project, category)`,
    /// as `(project, category, excerpt)`. Gives a report the concrete instances
    /// behind a friction count — the actual failing paths/files — so the reader
    /// (or a seeded agent) need not re-mine the transcripts. Earliest examples
    /// first within a group; empty excerpts are skipped.
    pub fn error_examples(&self, per_group: u32) -> Result<Vec<(String, String, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT project, category, excerpt FROM (
                 SELECT s.project AS project, e.surface_id AS category, e.source AS excerpt,
                        ROW_NUMBER() OVER (
                            PARTITION BY s.project, e.surface_id ORDER BY e.id
                        ) AS rn
                 FROM events e JOIN sessions s ON e.session_id = s.id
                 WHERE e.kind = 'tool_error' AND e.source IS NOT NULL AND e.source <> ''
             )
             WHERE rn <= ?1
             ORDER BY project, category, rn",
        )?;
        let rows = stmt
            .query_map([per_group], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Tool-failure counts split by project, category, and originating tool, as
    /// `(project, category, tool, count)`, densest first. Answers "which tool
    /// produced these failures" — e.g. path-not-found split across Read / Bash /
    /// Edit / a Playwright locator — so a report separates file friction from a
    /// browser miss that merely reads as "not found", and the seeded agent need
    /// not re-derive the attribution from the transcripts.
    pub fn error_tool_breakdown(&self) -> Result<Vec<(String, String, String, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT s.project, e.surface_id, COALESCE(e.model, 'unknown'), COUNT(*)
             FROM events e JOIN sessions s ON e.session_id = s.id
             WHERE e.kind = 'tool_error'
             GROUP BY s.project, e.surface_id, e.model
             ORDER BY COUNT(*) DESC, s.project, e.surface_id",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Counts of prompts by behavioral class (`source` column on prompt events),
    /// most frequent first.
    pub fn prompt_behavior_counts(&self) -> Result<Vec<(String, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT source, COUNT(*) FROM events WHERE kind = 'prompt' AND source IS NOT NULL
             GROUP BY source ORDER BY COUNT(*) DESC",
        )?;
        let rows = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Per-skill usage rollup, most-invoked first.
    pub fn skill_usage(&self) -> Result<Vec<SkillUsage>> {
        let mut stmt = self.conn.prepare(
            "SELECT surface_id,
                    COUNT(*),
                    SUM(out_tokens),
                    SUM(ctx_growth),
                    SUM(duration_sec)
             FROM events
             WHERE surface_kind = 'skill'
             GROUP BY surface_id
             ORDER BY COUNT(*) DESC, surface_id",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok(SkillUsage {
                    skill: row.get(0)?,
                    invocations: row.get(1)?,
                    out_tokens: row.get(2)?,
                    ctx_growth: row.get(3)?,
                    duration_sec: row.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Total tokens of **global** always-on (startup_full) config — what every
    /// session pays unconditionally from `~/.claude` regardless of project.
    /// Project config is always-on only for its own sessions
    /// (`always_on_config_tokens_for`).
    pub fn always_on_config_tokens(&self) -> Result<i64> {
        let total = self.conn.query_row(
            "SELECT COALESCE(SUM(static_tokens), 0) FROM surfaces
             WHERE load_mode = 'startup_full' AND scope = 'global'",
            [],
            |row| row.get(0),
        )?;
        Ok(total)
    }

    /// Empirical always-on context floor per project as `(project, floor,
    /// sessions)`: the leanest context any of that project's sessions *started*
    /// with, and how many starts back the figure.
    ///
    /// Only `session_start` events qualify. A skill span's `ctx_start` is the
    /// prompt size wherever that skill ran, which in a long or resumed session
    /// sits far above the session's own start — reading floors from spans
    /// reported startup costs that were really mid-session sizes. The session
    /// count is part of the answer because the floor is a minimum over
    /// observations: one resumed session alone still overstates it, and the
    /// count is what makes that visible.
    pub fn baseline_floor_per_project(&self) -> Result<Vec<(String, i64, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT s.project, MIN(e.ctx_start), COUNT(*)
             FROM events e JOIN sessions s ON e.session_id = s.id
             WHERE e.kind = 'session_start' AND e.ctx_start > 0
             GROUP BY s.project
             ORDER BY MIN(e.ctx_start) DESC",
        )?;
        let rows = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// The global empirical always-on floor — the leanest session start observed
    /// anywhere. `None` when no session start was observed at all, so callers
    /// report the metric as unavailable instead of inventing a zero.
    pub fn baseline_floor(&self) -> Result<Option<i64>> {
        let floor = self.conn.query_row(
            "SELECT MIN(ctx_start) FROM events
             WHERE kind = 'session_start' AND ctx_start > 0",
            [],
            |row| row.get(0),
        )?;
        Ok(floor)
    }

    /// How much data the store holds: `(sessions, distinct projects)` — the
    /// summary's "what was analyzed" context line.
    pub fn session_stats(&self) -> Result<(i64, i64)> {
        let row = self.conn.query_row(
            "SELECT COUNT(*), COUNT(DISTINCT project) FROM sessions",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        Ok(row)
    }

    /// Total subagent output tokens and subagent count across all sessions.
    pub fn subagent_totals(&self) -> Result<(i64, i64)> {
        let row = self.conn.query_row(
            "SELECT COALESCE(SUM(sub_tokens), 0), COALESCE(SUM(sub_agent_count), 0) FROM sessions",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        Ok(row)
    }

    /// Invocation counts per surface `(kind, id)` across all event kinds — the
    /// usage side of the catalog join for every surface, not just skills.
    pub fn usage_counts(&self) -> Result<Vec<(String, String, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT surface_kind, surface_id, COUNT(*)
             FROM events
             WHERE surface_kind IS NOT NULL
             GROUP BY surface_kind, surface_id",
        )?;
        let rows = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Per-event costs for skill invocations, for time bucketing in the report.
    pub fn skill_event_costs(&self) -> Result<Vec<EventCost>> {
        let mut stmt = self.conn.prepare(
            "SELECT started_epoch, out_tokens, ctx_growth, duration_sec
             FROM events
             WHERE surface_kind = 'skill'",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok(EventCost {
                    started_epoch: row.get(0)?,
                    out_tokens: row.get(1)?,
                    ctx_growth: row.get(2)?,
                    duration_sec: row.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// An analyze-run metadata value, if recorded.
    pub fn meta(&self, key: &str) -> Result<Option<String>> {
        let mut stmt = self.conn.prepare("SELECT value FROM meta WHERE key = ?1")?;
        let mut rows = stmt.query_map([key], |row| row.get(0))?;
        Ok(rows.next().transpose()?)
    }

    /// Record an analyze-run metadata value, superseding any previous one.
    pub fn set_meta(&mut self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)",
            (key, value),
        )?;
        Ok(())
    }

    /// Whether the store's rows were produced by the current analysis semantics.
    /// The `(mtime, size)` skip below never expires, so an improved extractor or
    /// classifier would otherwise leave a transcript that never changes stamped
    /// with its old results forever; a mismatch here tells `analyze` to re-ingest
    /// everything once (`docs/specs/storage.md`). "Everything" means every
    /// transcript still on disk — rows whose source file is gone can never be
    /// re-derived, so they stay as last extracted.
    pub fn is_analyzer_current(&self) -> Result<bool> {
        Ok(self.meta(ANALYZER_META_KEY)?.as_deref() == Some(ANALYZER_VERSION))
    }

    /// Stamp the store with the analysis semantics its rows were produced by.
    /// Call once a full analyze run has completed.
    pub fn record_analyzer_version(&mut self) -> Result<()> {
        self.set_meta(ANALYZER_META_KEY, ANALYZER_VERSION)
    }

    /// Whether a source file's `(mtime, size)` matches its recorded ingest
    /// fingerprint — if so, re-analyzing may skip it (`docs/specs/storage.md`).
    pub fn is_ingested(&self, path: &str, mtime: i64, size: i64) -> Result<bool> {
        let matched = self.conn.query_row(
            "SELECT COUNT(*) FROM ingested_files WHERE path = ?1 AND mtime = ?2 AND size = ?3",
            (path, mtime, size),
            |row| row.get::<_, i64>(0),
        )?;
        Ok(matched > 0)
    }

    /// Record a source file's ingest fingerprint. Call **after** the read
    /// completes, with the pre-read stat — a file that grew mid-read then shows
    /// a changed fingerprint next run and is re-ingested (`docs/specs/storage.md`).
    pub fn record_ingested_file(&mut self, path: &str, mtime: i64, size: i64) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO ingested_files (path, mtime, size) VALUES (?1, ?2, ?3)",
            (path, mtime, size),
        )?;
        Ok(())
    }

    /// Rebuild the surface catalog wholesale — it is a snapshot of current
    /// config, not an accumulation (see `docs/specs/storage.md`).
    pub fn replace_surfaces(&mut self, surfaces: &[Surface]) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM surfaces", ())?;
        for surface in surfaces {
            tx.execute(
                "INSERT OR REPLACE INTO surfaces
                   (kind, id, scope, project, config_path, static_tokens, load_mode)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                (
                    &surface.kind,
                    &surface.id,
                    surface.scope.label(),
                    surface.scope.project(),
                    &surface.config_path,
                    surface.static_tokens,
                    surface.load_mode.label(),
                ),
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// The whole catalog — every `(kind, id, scope, project)` row — with each
    /// row's **effective** usage. Shadowing is per project: an event from
    /// project P joins P's project row when one exists for that `(kind, id)`,
    /// else the global row. One event therefore counts on exactly one row
    /// (`docs/specs/surfaces.md`).
    pub fn effective_catalog(&self) -> Result<Vec<CatalogEntry>> {
        let mut stmt = self.conn.prepare(
            "SELECT f.kind, f.id, f.scope, f.project, f.static_tokens, f.load_mode,
                    COUNT(u.event_id)
             FROM surfaces f
             LEFT JOIN (SELECT e.id AS event_id, e.surface_kind, e.surface_id,
                               s.project AS session_project
                        FROM events e JOIN sessions s ON e.session_id = s.id
                        WHERE e.surface_kind IS NOT NULL) u
               ON u.surface_kind = f.kind AND u.surface_id = f.id
              AND ((f.scope = 'project' AND u.session_project = f.project)
                OR (f.scope = 'global' AND u.session_project NOT IN (
                      SELECT p.project FROM surfaces p
                      WHERE p.kind = f.kind AND p.id = f.id AND p.scope = 'project')))
             GROUP BY f.kind, f.id, f.scope, f.project
             ORDER BY f.kind, f.id, f.scope, f.project",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok(CatalogEntry {
                    kind: row.get(0)?,
                    id: row.get(1)?,
                    scope: row.get(2)?,
                    project: row.get(3)?,
                    static_tokens: row.get(4)?,
                    load_mode: row.get(5)?,
                    uses: row.get(6)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Distinct known `(root, project)` pairs — the directories project-config
    /// scanning walks, each with the normalized project slug its surfaces get
    /// scoped to. Sessions whose transcript carried no cwd are skipped.
    pub fn session_roots(&self) -> Result<Vec<(String, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT root, project FROM sessions WHERE root <> '' ORDER BY root",
        )?;
        let rows = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Total always-on config tokens for a session in `project`: the global
    /// figure plus that project's own startup-full config.
    pub fn always_on_config_tokens_for(&self, project: &str) -> Result<i64> {
        let total = self.conn.query_row(
            "SELECT COALESCE(SUM(static_tokens), 0) FROM surfaces
             WHERE load_mode = 'startup_full'
               AND (scope = 'global' OR (scope = 'project' AND project = ?1))",
            [project],
            |row| row.get(0),
        )?;
        Ok(total)
    }
}

/// Bring an existing store onto the current schema **in place**. The store
/// outlives the transcripts it was built from, so an upgrade may not resolve
/// schema drift by asking for the file to be deleted — that would discard
/// history no re-analysis can reconstruct (`docs/specs/storage.md`).
///
/// Drift is reconciled per table by what the table costs to lose, and the
/// declared schema is the single source of truth for the target shape: it is
/// applied to a scratch database and read back, so adding a column to `SCHEMA`
/// is all a future change needs.
fn migrate(conn: &mut Connection) -> Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version > SCHEMA_VERSION {
        // Reconciliation only ever adds; it cannot undo a newer cclens's
        // changes, and writing this build's shape over them would corrupt a
        // store that binary still reads correctly.
        anyhow::bail!(
            "the store was written by a newer cclens (schema v{version}; this build knows \
             v{SCHEMA_VERSION}) — upgrade cclens to read it"
        );
    }

    let desired = Connection::open_in_memory()?;
    desired.execute_batch(SCHEMA)?;
    desired.execute_batch(INDEX_SCHEMA)?;
    desired.execute_batch(VIEW_SCHEMA)?;

    // One transaction for the whole migration, as every other multi-statement
    // write in this file does: reconciliation may bail partway through, and a
    // store left half-migrated is exactly the outcome in-place upgrading exists
    // to avoid. SQLite rolls DDL back like any other statement.
    let tx = conn.transaction()?;

    let mut rebuilt = false;
    for table in REBUILT_TABLES {
        if table_exists(&tx, table)? && table_shape(&tx, table)? != table_shape(&desired, table)? {
            tx.execute_batch(&format!("DROP TABLE {table}"))?;
            rebuilt = true;
        }
    }
    tx.execute_batch(SCHEMA)?;
    for table in MIGRATED_TABLES {
        reconcile_columns(&tx, &desired, table)?;
    }
    // Indexes and views come last: both reference columns the step above may
    // have just added.
    reconcile_derived(&tx, &desired, "index")?;
    reconcile_derived(&tx, &desired, "view")?;
    if rebuilt {
        // The catalog is empty until the next analyze refills it. Without this a
        // `--frozen` read would print an ordinary freshness line over an empty
        // catalog, reporting "nothing installed" as though it were a finding.
        tx.execute("DELETE FROM meta WHERE key = 'analyzed_at'", ())?;
    }

    tx.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION}"))?;
    tx.commit()?;
    Ok(())
}

/// Reconcile `table` against the columns `desired` declares for it. Only
/// additive drift is reconcilable in place — SQLite can append a column but not
/// retype or re-key one, and a NOT NULL column needs a default to fill the rows
/// already stored — so anything else is reported rather than half-applied.
///
/// A column already present is compared, not skipped: reconciliation would
/// otherwise pass silently over a retype or a changed default, leaving the store
/// permanently diverged from the declaration it is supposed to converge on.
fn reconcile_columns(conn: &Connection, desired: &Connection, table: &str) -> Result<()> {
    let present = table_shape(conn, table)?;
    for column in table_shape(desired, table)? {
        if let Some(existing) = present.iter().find(|c| c.name == column.name) {
            if *existing != column {
                anyhow::bail!(
                    "`{table}.{}` is declared as {column:?} but the store holds {existing:?}: \
                     SQLite cannot change a column in place, so this needs an explicit \
                     migration that states what it discards",
                    column.name
                );
            }
            continue;
        }
        let mut ddl = format!(
            "ALTER TABLE {table} ADD COLUMN {} {}",
            column.name, column.decl_type
        );
        match &column.default {
            Some(default) => {
                if column.notnull {
                    ddl.push_str(" NOT NULL");
                }
                ddl.push_str(&format!(" DEFAULT {default}"));
            }
            None if column.notnull => anyhow::bail!(
                "cannot add `{table}.{}` to an existing store: a NOT NULL column needs a \
                 DEFAULT to fill the rows already stored",
                column.name
            ),
            None => {}
        }
        conn.execute_batch(&ddl)?;
    }
    Ok(())
}

/// Recreate every index or view of `kind` whose stored definition differs from
/// the declared one. Both carry no data, so replacing one costs nothing but the
/// rebuild — which is why only a *drifted* object is touched: reindexing a large
/// `events` table on every open would not be free. Objects the declaration does
/// not name are left alone; the store is meant to be explored with ad-hoc SQL, so
/// a helper view someone saved into the file is theirs to keep.
///
/// Deriving the names from the declaration is also what removes the hand-kept
/// list this replaced: such a list drifts, and a declared object missing from it
/// would collide with its own `CREATE` on the store's second open.
fn reconcile_derived(conn: &Connection, desired: &Connection, kind: &str) -> Result<()> {
    let stored = declared_sql(conn, kind)?;
    for (name, sql) in declared_sql(desired, kind)? {
        if stored.get(&name).is_some_and(|existing| *existing == sql) {
            continue;
        }
        conn.execute_batch(&format!("DROP {kind} IF EXISTS {name}"))?;
        conn.execute_batch(&sql)?;
    }
    Ok(())
}

/// `name -> CREATE statement` for every object of `kind` that carries one (an
/// implicit index has none). SQLite stores the statement as written minus
/// `IF NOT EXISTS`, so definitions built from the same declaration compare equal.
fn declared_sql(conn: &Connection, kind: &str) -> Result<BTreeMap<String, String>> {
    let mut stmt =
        conn.prepare("SELECT name, sql FROM sqlite_master WHERE type = ?1 AND sql IS NOT NULL")?;
    let rows = stmt
        .query_map([kind], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<BTreeMap<String, String>>>()?;
    Ok(rows)
}

/// One column as SQLite reports it (`PRAGMA table_info`).
#[derive(Debug, PartialEq)]
struct ColumnDef {
    name: String,
    decl_type: String,
    notnull: bool,
    default: Option<String>,
    pk: i64,
}

/// A table's columns in declaration order; empty when the table does not exist.
fn table_shape(conn: &Connection, table: &str) -> Result<Vec<ColumnDef>> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let columns = stmt
        .query_map([], |row| {
            Ok(ColumnDef {
                name: row.get(1)?,
                decl_type: row.get(2)?,
                notnull: row.get::<_, i64>(3)? != 0,
                default: row.get(4)?,
                pk: row.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(columns)
}

fn table_exists(conn: &Connection, table: &str) -> Result<bool> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [table],
        |row| row.get(0),
    )?;
    Ok(n > 0)
}

fn source_label(source: Source) -> &'static str {
    match source {
        Source::Slash => "slash",
        Source::Tool => "tool",
    }
}

fn epoch_ms_to_rfc3339(epoch_ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(epoch_ms)
        .map(|dt| dt.to_rfc3339())
        .unwrap_or_default()
}

/// A SQLite cell as a typed JSON value, for the `--format json` surface.
fn value_to_json(v: rusqlite::types::ValueRef<'_>) -> serde_json::Value {
    use rusqlite::types::ValueRef;
    match v {
        ValueRef::Null => serde_json::Value::Null,
        ValueRef::Integer(i) => serde_json::json!(i),
        ValueRef::Real(f) => serde_json::json!(f),
        ValueRef::Text(t) => serde_json::json!(String::from_utf8_lossy(t)),
        ValueRef::Blob(_) => serde_json::json!("<blob>"),
    }
}

/// Stringify a SQLite cell for the generic `query` surface.
fn value_to_string(v: rusqlite::types::ValueRef<'_>) -> String {
    use rusqlite::types::ValueRef;
    match v {
        ValueRef::Null => String::new(),
        ValueRef::Integer(i) => i.to_string(),
        ValueRef::Real(f) => f.to_string(),
        ValueRef::Text(t) => String::from_utf8_lossy(t).into_owned(),
        ValueRef::Blob(_) => "<blob>".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::surface::{LoadMode, Scope};

    fn span(skill: &str, out_tokens: u64, ctx_growth: u64, duration_sec: f64) -> Span {
        Span {
            skill: skill.to_string(),
            source: Source::Slash,
            started_epoch_ms: 1_700_000_000_000,
            duration_sec,
            out_tokens,
            ctx_growth,
            ctx_start: 0,
            ctx_peak: ctx_growth,
            model: Some("claude-opus-4-7".to_string()),
            is_trailing: false,
            agent_prompt_ids: Vec::new(),
            sub_tokens: 0,
            sub_agent_count: 0,
            sub_tokens_estimated: false,
        }
    }

    fn session(id: &str) -> SessionMeta {
        SessionMeta {
            id: id.to_string(),
            project: "demo".to_string(),
            slug: "demo".to_string(),
            root: String::new(),
            source_path: format!("/tmp/{id}.jsonl"),
            sub_tokens: 0,
            sub_agent_count: 0,
            start: None,
        }
    }

    #[test]
    fn rolls_up_usage_per_skill_across_sessions() {
        let mut store = Store::in_memory().unwrap();
        store
            .ingest_session(
                &session("s1"),
                &[
                    span("git-commit", 100, 50, 2.0),
                    span("git-commit", 200, 30, 1.0),
                ],
                &[],
            )
            .unwrap();
        store
            .ingest_session(&session("s2"), &[span("pr-create", 10, 5, 0.5)], &[])
            .unwrap();

        let usage = store.skill_usage().unwrap();

        assert_eq!(
            usage,
            vec![
                SkillUsage {
                    skill: "git-commit".to_string(),
                    invocations: 2,
                    out_tokens: 300,
                    ctx_growth: 80,
                    duration_sec: 3.0,
                },
                SkillUsage {
                    skill: "pr-create".to_string(),
                    invocations: 1,
                    out_tokens: 10,
                    ctx_growth: 5,
                    duration_sec: 0.5,
                },
            ]
        );
    }

    #[test]
    fn re_ingesting_a_session_replaces_its_events() {
        let mut store = Store::in_memory().unwrap();
        store
            .ingest_session(&session("s1"), &[span("git-commit", 100, 50, 2.0)], &[])
            .unwrap();
        // Same source_path, different content — must supersede, not accumulate.
        store
            .ingest_session(&session("s1"), &[span("git-commit", 999, 999, 9.0)], &[])
            .unwrap();

        let usage = store.skill_usage().unwrap();
        assert_eq!(usage.len(), 1);
        assert_eq!(usage[0].invocations, 1);
        assert_eq!(usage[0].out_tokens, 999);
    }

    fn surface(id: &str, scope: Scope, static_tokens: u64) -> Surface {
        Surface {
            kind: "skill".to_string(),
            id: id.to_string(),
            scope,
            config_path: format!("/cfg/{id}"),
            static_tokens: Some(static_tokens),
            load_mode: LoadMode::StartupDescription,
        }
    }

    #[test]
    fn a_project_surface_shadows_global_only_for_its_own_sessions() {
        // skill/git-commit is installed globally AND in project alpha; alpha and
        // beta each invoke it once. Alpha's use lands on alpha's project row,
        // beta's on the global row — two uses total, never double-counted, and
        // beta's usage never inflates alpha's copy.
        let mut store = Store::in_memory().unwrap();
        let mut alpha = session("a1");
        alpha.project = "alpha".to_string();
        let mut beta = session("b1");
        beta.project = "beta".to_string();
        store
            .ingest_session(&alpha, &[span("git-commit", 1, 1, 1.0)], &[])
            .unwrap();
        store
            .ingest_session(&beta, &[span("git-commit", 1, 1, 1.0)], &[])
            .unwrap();
        store
            .replace_surfaces(&[
                surface("git-commit", Scope::Global, 100),
                surface("git-commit", Scope::Project("alpha".to_string()), 250),
            ])
            .unwrap();

        let catalog = store.effective_catalog().unwrap();

        assert_eq!(catalog.len(), 2);
        let global = catalog.iter().find(|e| e.scope == "global").unwrap();
        let project = catalog.iter().find(|e| e.scope == "project").unwrap();
        assert_eq!(project.project, "alpha");
        assert_eq!(project.static_tokens, Some(250));
        assert_eq!(project.uses, 1); // alpha's invocation only
        assert_eq!(global.uses, 1); // beta's invocation only
    }

    #[test]
    fn an_unshadowed_global_surface_counts_usage_from_every_project() {
        let mut store = Store::in_memory().unwrap();
        let mut alpha = session("a1");
        alpha.project = "alpha".to_string();
        let mut beta = session("b1");
        beta.project = "beta".to_string();
        store
            .ingest_session(&alpha, &[span("pr-create", 1, 1, 1.0)], &[])
            .unwrap();
        store
            .ingest_session(&beta, &[span("pr-create", 1, 1, 1.0)], &[])
            .unwrap();
        store
            .replace_surfaces(&[surface("pr-create", Scope::Global, 40)])
            .unwrap();

        let catalog = store.effective_catalog().unwrap();
        assert_eq!(catalog.len(), 1);
        assert_eq!(catalog[0].uses, 2);
    }

    #[test]
    fn session_roots_are_distinct_pairs_and_skip_unknown() {
        let mut store = Store::in_memory().unwrap();
        let mut a = session("a1");
        a.root = "/tmp/example/app".to_string();
        a.project = "alpha".to_string();
        let mut b = session("b1");
        b.root = "/tmp/example/app".to_string(); // duplicate root
        b.project = "alpha".to_string();
        let c = session("c1"); // root unknown (empty)
        store.ingest_session(&a, &[], &[]).unwrap();
        store.ingest_session(&b, &[], &[]).unwrap();
        store.ingest_session(&c, &[], &[]).unwrap();

        assert_eq!(
            store.session_roots().unwrap(),
            vec![("/tmp/example/app".to_string(), "alpha".to_string())]
        );
    }

    /// A store as an older cclens left it: `sessions` without the columns later
    /// releases added, and `surfaces` still keyed without `project`.
    fn legacy_store() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE sessions (
                 id          TEXT PRIMARY KEY,
                 project     TEXT NOT NULL,
                 slug        TEXT NOT NULL,
                 source_path TEXT NOT NULL,
                 started_at  TEXT NOT NULL
             );
             CREATE TABLE surfaces (
                 kind          TEXT NOT NULL,
                 id            TEXT NOT NULL,
                 scope         TEXT NOT NULL,
                 config_path   TEXT,
                 static_tokens INTEGER,
                 load_mode     TEXT NOT NULL,
                 PRIMARY KEY (kind, id, scope)
             );",
        )
        .unwrap();
        conn
    }

    #[test]
    fn a_legacy_store_gains_missing_columns_without_losing_its_history() {
        // The rows are irreplaceable once Claude Code has pruned the source
        // transcripts, so upgrading must add the columns in place.
        let conn = legacy_store();
        conn.execute(
            "INSERT INTO sessions (id, project, slug, source_path, started_at)
             VALUES ('s1', 'demo', 'demo', '/tmp/example/s1.jsonl', '2026-01-01T00:00:00Z')",
            (),
        )
        .unwrap();

        let store = Store::from_connection(conn).expect("must migrate, not refuse");

        let (_, rows) = store
            .query("SELECT id, root, sub_tokens FROM sessions")
            .unwrap();
        assert_eq!(
            rows,
            vec![vec!["s1".to_string(), String::new(), "0".to_string()]],
            "the pre-existing session must survive, with the added columns defaulted"
        );
    }

    #[test]
    fn a_legacy_catalog_table_is_rebuilt_when_its_shape_drifted() {
        // `surfaces` is a snapshot of live config, rebuilt on every analyze, so
        // a shape change is recreated rather than patched column by column.
        let conn = legacy_store();

        let store = Store::from_connection(conn).expect("must migrate, not refuse");

        let names: Vec<String> = table_shape(&store.conn, "surfaces")
            .unwrap()
            .into_iter()
            .map(|column| column.name)
            .collect();
        assert!(names.contains(&"project".to_string()), "{names:?}");
    }

    #[test]
    fn an_up_to_date_catalog_survives_reopening() {
        // Reopening must not drop `surfaces` merely because the store predates
        // version stamping — a `--frozen` read would then report an empty catalog.
        let mut store = Store::in_memory().unwrap();
        store
            .replace_surfaces(&[surface("git-commit", Scope::Global, 1)])
            .unwrap();
        store
            .conn
            .execute_batch("PRAGMA user_version = 0;")
            .unwrap();

        let store = Store::from_connection(store.conn).unwrap();

        assert_eq!(store.effective_catalog().unwrap().len(), 1);
    }

    #[test]
    fn a_stale_view_definition_is_replaced_on_open() {
        // `CREATE VIEW IF NOT EXISTS` leaves an older definition in place, so a
        // reader would silently query yesterday's column mapping.
        let conn = legacy_store();
        conn.execute_batch("CREATE VIEW tool_errors AS SELECT id AS session_id FROM sessions;")
            .unwrap();

        let store = Store::from_connection(conn).expect("must migrate, not refuse");

        let sql: String = store
            .conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'view' AND name = 'tool_errors'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(sql.contains("kind = 'tool_error'"), "stale view: {sql}");
    }

    #[test]
    fn opening_stamps_the_current_schema_version() {
        let store = Store::from_connection(legacy_store()).unwrap();

        let version: i64 = store
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }

    #[test]
    fn a_store_written_by_a_newer_cclens_is_refused_without_advising_deletion() {
        // Writing to it with this binary's schema would corrupt history the
        // newer cclens can still read; deleting it would throw that history away.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(&format!("PRAGMA user_version = {};", SCHEMA_VERSION + 1))
            .unwrap();

        let err = Store::from_connection(conn).err().expect("must be refused");

        let msg = err.to_string();
        assert!(msg.contains("upgrade"), "{msg}");
        assert!(!msg.contains("delete"), "must not advise deletion: {msg}");
    }

    #[test]
    fn a_column_that_cannot_be_added_in_place_is_reported() {
        // SQLite cannot add a NOT NULL column without a default; failing loudly
        // beats leaving the store half-migrated.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE t (id TEXT PRIMARY KEY);")
            .unwrap();
        let desired = Connection::open_in_memory().unwrap();
        desired
            .execute_batch("CREATE TABLE t (id TEXT PRIMARY KEY, tag TEXT NOT NULL);")
            .unwrap();

        let err = reconcile_columns(&conn, &desired, "t").expect_err("must be reported");

        assert!(err.to_string().contains("tag"), "{err}");
    }

    #[test]
    fn a_column_whose_declaration_drifted_is_reported() {
        // Only additive drift is reconcilable: SQLite cannot retype or re-key a
        // column in place, so a declaration change that reconciliation would
        // silently skip has to fail while it is still a development mistake.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE t (id TEXT PRIMARY KEY, n TEXT NOT NULL DEFAULT '');")
            .unwrap();
        let desired = Connection::open_in_memory().unwrap();
        desired
            .execute_batch("CREATE TABLE t (id TEXT PRIMARY KEY, n INTEGER NOT NULL DEFAULT 0);")
            .unwrap();

        let err = reconcile_columns(&conn, &desired, "t").expect_err("must be reported");

        assert!(err.to_string().contains('n'), "{err}");
    }

    #[test]
    fn a_failed_migration_leaves_the_store_untouched() {
        // The whole point of migrating in place is that the file is never left
        // worse than it was found, so a bail partway through must roll back the
        // columns already added.
        let mut conn = legacy_store();
        conn.execute_batch(
            "CREATE TABLE events (
                 id            INTEGER PRIMARY KEY,
                 session_id    TEXT NOT NULL,
                 source_path   TEXT NOT NULL,
                 kind          TEXT NOT NULL,
                 started_at    TEXT NOT NULL,
                 started_epoch INTEGER NOT NULL,
                 duration_sec  REAL NOT NULL,
                 out_tokens    TEXT NOT NULL,
                 ctx_growth    INTEGER NOT NULL,
                 ctx_start     INTEGER NOT NULL,
                 ctx_peak      INTEGER NOT NULL
             );",
        )
        .unwrap();

        migrate(&mut conn).expect_err("the drifted events.out_tokens must be reported");

        let names: Vec<String> = table_shape(&conn, "sessions")
            .unwrap()
            .into_iter()
            .map(|column| column.name)
            .collect();
        assert!(
            !names.contains(&"root".to_string()),
            "sessions was altered before the bail and not rolled back: {names:?}"
        );
    }

    #[test]
    fn a_drifted_index_is_recreated_from_the_declaration() {
        // `CREATE INDEX IF NOT EXISTS` leaves an older definition in place, so
        // the store would keep an index the declaration no longer describes.
        let store = Store::in_memory().unwrap();
        store
            .conn
            .execute_batch(
                "DROP INDEX events_by_surface;
                 CREATE INDEX events_by_surface ON events(surface_id);",
            )
            .unwrap();

        let store = Store::from_connection(store.conn).unwrap();

        let sql: String = store
            .conn
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'index' AND name = 'events_by_surface'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(sql.contains("surface_kind"), "stale index: {sql}");
    }

    #[test]
    fn rebuilding_the_catalog_clears_the_analyze_timestamp() {
        // The rebuilt catalog is empty until the next analyze repopulates it, and
        // a `--frozen` read would otherwise report that emptiness as a finished
        // analysis. Dropping the timestamp routes it to the freshness warning.
        let mut store = Store::in_memory().unwrap();
        store
            .set_meta("analyzed_at", "2026-01-01T00:00:00Z")
            .unwrap();
        store
            .conn
            .execute_batch(
                "DROP TABLE surfaces;
                 CREATE TABLE surfaces (
                     kind TEXT NOT NULL, id TEXT NOT NULL, scope TEXT NOT NULL,
                     config_path TEXT, static_tokens INTEGER, load_mode TEXT NOT NULL,
                     PRIMARY KEY (kind, id, scope)
                 );",
            )
            .unwrap();

        let store = Store::from_connection(store.conn).unwrap();

        assert_eq!(store.meta("analyzed_at").unwrap(), None);
    }

    #[test]
    fn a_view_the_declaration_does_not_name_is_left_alone() {
        // The store is meant to be explored with ad-hoc SQL, so a helper view
        // someone saved into the file is theirs — reconciliation replaces what the
        // declaration names and touches nothing else.
        let store = Store::from_connection(Connection::open_in_memory().unwrap()).unwrap();
        store
            .conn
            .execute_batch("CREATE VIEW stray AS SELECT id FROM sessions;")
            .unwrap();

        let store = Store::from_connection(store.conn).expect("a second open must not collide");

        let views: Vec<String> = declared_sql(&store.conn, "view")
            .unwrap()
            .into_keys()
            .collect();
        assert_eq!(views, vec!["stray".to_string(), "tool_errors".to_string()]);
    }

    #[test]
    fn an_index_is_created_after_the_columns_it_covers() {
        // Indexes are declared apart from the tables so they are applied only
        // after column reconciliation. Declaring one alongside the table would
        // run it against a legacy shape and fail the whole migration — the exact
        // destructive refusal in-place upgrading exists to remove.
        let conn = legacy_store();
        conn.execute_batch(
            "CREATE TABLE events (
                 id            INTEGER PRIMARY KEY,
                 session_id    TEXT NOT NULL,
                 source_path   TEXT NOT NULL,
                 kind          TEXT NOT NULL,
                 surface_id    TEXT,
                 started_at    TEXT NOT NULL,
                 started_epoch INTEGER NOT NULL,
                 duration_sec  REAL NOT NULL,
                 out_tokens    INTEGER NOT NULL,
                 ctx_growth    INTEGER NOT NULL,
                 ctx_start     INTEGER NOT NULL,
                 ctx_peak      INTEGER NOT NULL
             );",
        )
        .unwrap();

        let store = Store::from_connection(conn).expect("must migrate, not refuse");

        let sql = declared_sql(&store.conn, "index").unwrap();
        assert!(sql["events_by_surface"].contains("surface_kind"), "{sql:?}");
    }

    fn span_at_ctx(skill: &str, ctx_start: u64) -> Span {
        Span {
            skill: skill.to_string(),
            source: Source::Slash,
            started_epoch_ms: 1_700_000_000_000,
            duration_sec: 1.0,
            out_tokens: 10,
            ctx_growth: 5,
            ctx_start,
            ctx_peak: ctx_start,
            model: None,
            is_trailing: false,
            agent_prompt_ids: Vec::new(),
            sub_tokens: 0,
            sub_agent_count: 0,
            sub_tokens_estimated: false,
        }
    }

    #[test]
    fn baseline_floor_is_the_leanest_session_start_per_project() {
        let mut store = Store::in_memory().unwrap();
        // Project "alpha" started two sessions at 30000 and 12000; "beta" one
        // at 40000.
        let mut alpha1 = session("a1");
        alpha1.project = "alpha".to_string();
        alpha1.start = Some(SessionStart {
            timestamp_ms: 1_700_000_000_000,
            ctx: 30000,
        });
        store.ingest_session(&alpha1, &[], &[]).unwrap();
        let mut alpha2 = session("a2");
        alpha2.project = "alpha".to_string();
        alpha2.start = Some(SessionStart {
            timestamp_ms: 1_700_000_000_000,
            ctx: 12000,
        });
        store.ingest_session(&alpha2, &[], &[]).unwrap();
        let mut beta = session("b1");
        beta.project = "beta".to_string();
        beta.start = Some(SessionStart {
            timestamp_ms: 1_700_000_000_000,
            ctx: 40000,
        });
        store.ingest_session(&beta, &[], &[]).unwrap();

        assert_eq!(store.baseline_floor().unwrap(), Some(12000));
        assert_eq!(
            store.baseline_floor_per_project().unwrap(),
            vec![
                ("beta".to_string(), 40000, 1),
                ("alpha".to_string(), 12000, 2)
            ]
        );
    }

    #[test]
    fn baseline_floor_ignores_the_context_a_skill_happened_to_run_at() {
        let mut store = Store::in_memory().unwrap();
        // A session that started lean but only invoked a skill deep into a long
        // conversation. The span's ctx_start is not a floor observation.
        let mut meta = session("s1");
        meta.start = Some(SessionStart {
            timestamp_ms: 1_700_000_000_000,
            ctx: 50000,
        });
        store
            .ingest_session(&meta, &[span_at_ctx("git-commit", 430000)], &[])
            .unwrap();

        assert_eq!(store.baseline_floor().unwrap(), Some(50000));
    }

    #[test]
    fn a_session_start_event_is_stamped_with_when_the_session_began() {
        // `sql` exposes raw event rows, so an unstamped row would place every
        // session start in 1970.
        let mut store = Store::in_memory().unwrap();
        let mut meta = session("s1");
        meta.start = Some(SessionStart {
            timestamp_ms: 1_700_000_000_000,
            ctx: 50000,
        });
        store.ingest_session(&meta, &[], &[]).unwrap();

        let (_, rows) = store
            .query("SELECT started_epoch, started_at FROM events WHERE kind = 'session_start'")
            .unwrap();
        assert_eq!(rows[0][0], "1700000000");
        assert!(!rows[0][1].is_empty());
    }

    #[test]
    fn a_session_without_spans_still_records_when_it_started() {
        // started_at was derived only from spans, so a session that invoked no
        // skill was stored with an empty start.
        let mut store = Store::in_memory().unwrap();
        let mut meta = session("s1");
        meta.start = Some(SessionStart {
            timestamp_ms: 1_700_000_000_000,
            ctx: 50000,
        });
        store.ingest_session(&meta, &[], &[]).unwrap();

        let (_, rows) = store
            .query("SELECT started_at FROM sessions WHERE id = 's1'")
            .unwrap();
        assert!(!rows[0][0].is_empty());
    }

    #[test]
    fn baseline_floor_is_absent_when_no_session_start_was_observed() {
        let mut store = Store::in_memory().unwrap();
        let meta = session("s1");
        store
            .ingest_session(&meta, &[span_at_ctx("git-commit", 430000)], &[])
            .unwrap();

        assert_eq!(store.baseline_floor().unwrap(), None);
        assert_eq!(store.baseline_floor_per_project().unwrap(), vec![]);
    }

    #[test]
    fn always_on_config_sums_only_startup_full_surfaces() {
        let mut store = Store::in_memory().unwrap();
        store
            .replace_surfaces(&[
                Surface {
                    kind: "claude_md".to_string(),
                    id: "global".to_string(),
                    scope: Scope::Global,
                    config_path: "/c/CLAUDE.md".to_string(),
                    static_tokens: Some(600),
                    load_mode: LoadMode::StartupFull,
                },
                Surface {
                    kind: "rule".to_string(),
                    id: "git/safety".to_string(),
                    scope: Scope::Global,
                    config_path: "/c/safety.md".to_string(),
                    static_tokens: Some(900),
                    load_mode: LoadMode::StartupFull,
                },
                // A skill is startup_description — must NOT count.
                surface("git-commit", Scope::Global, 1000),
                // Another project's CLAUDE.md is always-on *there*, not globally.
                Surface {
                    kind: "claude_md".to_string(),
                    id: "project".to_string(),
                    scope: Scope::Project("alpha".to_string()),
                    config_path: "/tmp/example/CLAUDE.md".to_string(),
                    static_tokens: Some(400),
                    load_mode: LoadMode::StartupFull,
                },
            ])
            .unwrap();

        // The global figure excludes project config; a session in alpha pays
        // the global floor plus alpha's own always-on config.
        assert_eq!(store.always_on_config_tokens().unwrap(), 1500);
        assert_eq!(store.always_on_config_tokens_for("alpha").unwrap(), 1900);
    }

    #[test]
    fn replace_surfaces_rebuilds_wholesale() {
        let mut store = Store::in_memory().unwrap();
        store
            .replace_surfaces(&[surface("old", Scope::Global, 1)])
            .unwrap();
        store
            .replace_surfaces(&[surface("new", Scope::Global, 1)])
            .unwrap();

        let catalog = store.effective_catalog().unwrap();
        assert_eq!(catalog.len(), 1);
        assert_eq!(catalog[0].id, "new");
    }

    #[test]
    fn meta_round_trips_and_overwrites() {
        let mut store = Store::in_memory().unwrap();
        assert_eq!(store.meta("analyzed_at").unwrap(), None);
        store
            .set_meta("analyzed_at", "2026-01-01T00:00:00Z")
            .unwrap();
        store
            .set_meta("analyzed_at", "2026-01-02T00:00:00Z")
            .unwrap();
        assert_eq!(
            store.meta("analyzed_at").unwrap().as_deref(),
            Some("2026-01-02T00:00:00Z")
        );
    }

    #[test]
    fn an_unchanged_fingerprint_marks_a_file_ingested() {
        let mut store = Store::in_memory().unwrap();
        // Never seen: needs ingest.
        assert!(!store.is_ingested("/tmp/a.jsonl", 100, 5).unwrap());
        store.record_ingested_file("/tmp/a.jsonl", 100, 5).unwrap();
        // Same (mtime, size): skip. Any change: re-ingest.
        assert!(store.is_ingested("/tmp/a.jsonl", 100, 5).unwrap());
        assert!(!store.is_ingested("/tmp/a.jsonl", 101, 5).unwrap());
        assert!(!store.is_ingested("/tmp/a.jsonl", 100, 6).unwrap());
        // A new fingerprint supersedes the old one.
        store.record_ingested_file("/tmp/a.jsonl", 101, 6).unwrap();
        assert!(store.is_ingested("/tmp/a.jsonl", 101, 6).unwrap());
    }

    #[test]
    fn a_store_is_stale_until_stamped_with_the_current_analyzer() {
        let mut store = Store::in_memory().unwrap();
        // A store written by an older analyzer (or none at all) is stale, so
        // `analyze` re-ingests even the transcripts whose fingerprint matches.
        assert!(!store.is_analyzer_current().unwrap());
        store.set_meta(ANALYZER_META_KEY, "0").unwrap();
        assert!(!store.is_analyzer_current().unwrap());
        store.record_analyzer_version().unwrap();
        assert!(store.is_analyzer_current().unwrap());
    }

    #[test]
    fn error_counts_break_down_by_project_and_category() {
        let mut store = Store::in_memory().unwrap();
        // Two projects, the same category concentrated in one of them.
        let mut alpha = session("a1");
        alpha.project = "alpha".to_string();
        let mut beta = session("b1");
        beta.project = "beta".to_string();
        store.ingest_session(&alpha, &[], &[]).unwrap();
        store.ingest_session(&beta, &[], &[]).unwrap();
        store
            .ingest_tool_errors(
                "a1",
                &alpha.source_path,
                &[
                    (100, "edit-precondition", "x", "Edit", "f"),
                    (200, "edit-precondition", "y", "Edit", "f"),
                ],
            )
            .unwrap();
        store
            .ingest_tool_errors(
                "b1",
                &beta.source_path,
                &[(300, "edit-precondition", "z", "Write", "f")],
            )
            .unwrap();

        let rows = store.error_counts_by_project().unwrap();

        // Densest (project, category) pair first — alpha owns the friction.
        assert_eq!(
            rows,
            vec![
                ("alpha".to_string(), "edit-precondition".to_string(), 2),
                ("beta".to_string(), "edit-precondition".to_string(), 1),
            ]
        );
    }

    #[test]
    fn error_examples_are_capped_per_project_and_category() {
        let mut store = Store::in_memory().unwrap();
        let mut alpha = session("a1");
        alpha.project = "alpha".to_string();
        store.ingest_session(&alpha, &[], &[]).unwrap();
        store
            .ingest_tool_errors(
                "a1",
                &alpha.source_path,
                &[
                    (100, "path-not-found", "missing /a", "Read", "f"),
                    (200, "path-not-found", "missing /b", "Read", "f"),
                    (300, "path-not-found", "missing /c", "Bash", "f"),
                ],
            )
            .unwrap();

        // Two examples per group, earliest first — the third is dropped.
        let examples = store.error_examples(2).unwrap();
        assert_eq!(
            examples,
            vec![
                (
                    "alpha".to_string(),
                    "path-not-found".to_string(),
                    "missing /a".to_string()
                ),
                (
                    "alpha".to_string(),
                    "path-not-found".to_string(),
                    "missing /b".to_string()
                ),
            ]
        );
    }

    #[test]
    fn session_stats_count_sessions_and_distinct_projects() {
        let mut store = Store::in_memory().unwrap();
        let mut a = session("a1");
        a.project = "alpha".to_string();
        let mut b = session("b1");
        b.project = "alpha".to_string();
        let mut c = session("c1");
        c.project = "beta".to_string();
        store.ingest_session(&a, &[], &[]).unwrap();
        store.ingest_session(&b, &[], &[]).unwrap();
        store.ingest_session(&c, &[], &[]).unwrap();

        assert_eq!(store.session_stats().unwrap(), (3, 2));
    }

    #[test]
    fn file_edits_carry_their_project_session_and_full_path() {
        let mut store = Store::in_memory().unwrap();
        let mut alpha = session("a1");
        alpha.project = "alpha".to_string();
        store.ingest_session(&alpha, &[], &[]).unwrap();
        store
            .ingest_work_events(
                "a1",
                &alpha.source_path,
                &[(1_000, "file_edit", "x.rs", Some("/repo/src/x.rs"))],
            )
            .unwrap();

        assert_eq!(
            store.file_edits().unwrap(),
            vec![FileEdit {
                project: "alpha".to_string(),
                session_id: "a1".to_string(),
                path: "/repo/src/x.rs".to_string(),
                epoch: 1,
            }]
        );
    }

    #[test]
    fn a_file_edit_without_a_recorded_path_is_excluded() {
        // Falling back to the basename would silently regroup same-named files —
        // the very merge this query exists to prevent — for every store written
        // before paths were recorded.
        let mut store = Store::in_memory().unwrap();
        let alpha = session("a1");
        store.ingest_session(&alpha, &[], &[]).unwrap();
        store
            .ingest_work_events(
                "a1",
                &alpha.source_path,
                &[(1_000, "file_edit", "x.rs", None)],
            )
            .unwrap();

        assert_eq!(store.file_edits().unwrap(), vec![]);
    }

    #[test]
    fn query_json_preserves_sqlite_types() {
        let mut store = Store::in_memory().unwrap();
        let mut alpha = session("a1");
        alpha.project = "alpha".to_string();
        store.ingest_session(&alpha, &[], &[]).unwrap();

        let (cols, rows) = store
            .query_json("SELECT project, COUNT(*) AS n, NULL AS absent, 1.5 AS ratio FROM sessions")
            .unwrap();

        assert_eq!(cols, vec!["project", "n", "absent", "ratio"]);
        // Numbers stay numbers, NULL stays null — a `--format json` consumer
        // must not need to re-parse strings.
        assert_eq!(
            rows,
            vec![vec![
                serde_json::json!("alpha"),
                serde_json::json!(1),
                serde_json::Value::Null,
                serde_json::json!(1.5),
            ]]
        );
    }

    #[test]
    fn query_returns_columns_and_stringified_rows() {
        let mut store = Store::in_memory().unwrap();
        let mut alpha = session("a1");
        alpha.project = "alpha".to_string();
        store.ingest_session(&alpha, &[], &[]).unwrap();
        store
            .ingest_tool_errors(
                "a1",
                &alpha.source_path,
                &[(100, "path-not-found", "missing /a", "Read", "f")],
            )
            .unwrap();

        let (cols, rows) = store
            .query("SELECT category, tool, COUNT(*) AS n FROM tool_errors GROUP BY category, tool")
            .unwrap();
        assert_eq!(cols, vec!["category", "tool", "n"]);
        assert_eq!(
            rows,
            vec![vec![
                "path-not-found".to_string(),
                "Read".to_string(),
                "1".to_string()
            ]]
        );
    }

    #[test]
    fn tool_errors_view_exposes_named_columns_and_project() {
        let mut store = Store::in_memory().unwrap();
        let mut alpha = session("a1");
        alpha.project = "demo--wt-feature".to_string();
        store.ingest_session(&alpha, &[], &[]).unwrap();
        store
            .ingest_tool_errors(
                "a1",
                &alpha.source_path,
                &[(100, "path-not-found", "missing /a", "Read", "f")],
            )
            .unwrap();

        // An ad-hoc query against the view sees clean names, not the overloaded
        // event columns, and the worktree filter works off `project`.
        let row: (String, String, String, String) = store
            .conn
            .query_row(
                "SELECT project, category, excerpt, tool FROM tool_errors \
                 WHERE project LIKE '%--wt%'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(
            row,
            (
                "demo--wt-feature".to_string(),
                "path-not-found".to_string(),
                "missing /a".to_string(),
                "Read".to_string(),
            )
        );
    }

    #[test]
    fn error_tool_breakdown_splits_a_category_across_tools() {
        let mut store = Store::in_memory().unwrap();
        let mut alpha = session("a1");
        alpha.project = "alpha".to_string();
        store.ingest_session(&alpha, &[], &[]).unwrap();
        store
            .ingest_tool_errors(
                "a1",
                &alpha.source_path,
                &[
                    (100, "path-not-found", "missing /a", "Read", "f"),
                    (200, "path-not-found", "missing /b", "Read", "f"),
                    (300, "path-not-found", "missing /c", "Bash", "f"),
                ],
            )
            .unwrap();

        // path-not-found splits Read 2 / Bash 1, densest tool first.
        let rows = store.error_tool_breakdown().unwrap();
        assert_eq!(
            rows,
            vec![
                (
                    "alpha".to_string(),
                    "path-not-found".to_string(),
                    "Read".to_string(),
                    2
                ),
                (
                    "alpha".to_string(),
                    "path-not-found".to_string(),
                    "Bash".to_string(),
                    1
                ),
            ]
        );
    }
}
