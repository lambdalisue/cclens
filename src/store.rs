//! SQLite store: persist extracted spans as events and query them back. This
//! layer knows SQL but nothing about raw Claude Code formats. See
//! `docs/specs/storage.md`.
//!
//! The schema tracks the spec's `events` table; `attrs_json` is the one column
//! deferred until a report needs it.

use anyhow::Result;
use rusqlite::Connection;

use crate::core::span::{Source, Span};
use crate::core::surface::Surface;
use crate::core::usage::UsageEvent;

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
CREATE INDEX IF NOT EXISTS events_by_surface ON events(surface_kind, surface_id);
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
-- commands can report freshness and re-run the analysis with the same roots.
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
-- A clean read view over tool_error events: the friction columns are overloaded
-- onto generic event columns (category in surface_id, excerpt in source, tool in
-- model), so this view names them and joins the project — letting an ad-hoc SQL
-- query (e.g. from the optimize session) ask for any slice without knowing the
-- encoding. `project LIKE '%--wt%'` distinguishes a worktree from the main checkout.
CREATE VIEW IF NOT EXISTS tool_errors AS
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

    fn from_connection(conn: Connection) -> Result<Self> {
        // A db written by an older cclens lacks columns the queries below rely
        // on (`CREATE TABLE IF NOT EXISTS` will not add them). The store is a
        // regenerable cache, so refuse it with the fix instead of failing on
        // some later query.
        for (table, column) in [("sessions", "root"), ("surfaces", "project")] {
            if table_exists(&conn, table)? && !column_exists(&conn, table, column)? {
                anyhow::bail!(
                    "the store's schema is from an older cclens — delete the db file \
                     and re-run `cclens analyze` to regenerate it"
                );
            }
        }
        conn.execute_batch(SCHEMA)?;
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
        let started_at = spans
            .iter()
            .map(|span| span.started_epoch_ms)
            .min()
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
    pub fn ingest_work_events(
        &mut self,
        session_id: &str,
        source_path: &str,
        events: &[(i64, &str, String)],
    ) -> Result<()> {
        let tx = self.conn.transaction()?;
        for (epoch_ms, kind, id) in events {
            tx.execute(
                "INSERT INTO events
                   (session_id, source_path, kind, surface_id,
                    started_at, started_epoch, duration_sec, out_tokens, ctx_growth,
                    ctx_start, ctx_peak)
                 VALUES (?1, ?2, ?3, ?4, '', ?5, 0, 0, 0, 0, 0)",
                (session_id, source_path, *kind, id, epoch_ms / 1000),
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Raw work events of a kind as `(project, id, started_epoch)`, time-ordered
    /// — burst/thrash detection per project, so a burst is reported under the
    /// project it happened in.
    pub fn work_event_rows_by_project(&self, kind: &str) -> Result<Vec<(String, String, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT s.project, e.surface_id, e.started_epoch
             FROM events e JOIN sessions s ON e.session_id = s.id
             WHERE e.kind = ?1 ORDER BY e.started_epoch",
        )?;
        let rows = stmt
            .query_map([kind], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Raw work events of a kind as `(id, started_epoch)`, time-ordered — for
    /// burst/thrash detection where individual timestamps matter.
    pub fn work_event_rows(&self, kind: &str) -> Result<Vec<(String, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT surface_id, started_epoch FROM events WHERE kind = ?1 ORDER BY started_epoch",
        )?;
        let rows = stmt
            .query_map([kind], |row| Ok((row.get(0)?, row.get(1)?)))?
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

    /// Empirical always-on context floor per project: the smallest non-trivial
    /// prompt size any skill started with. The leanest such moment is closest to
    /// a fresh session's baseline (system prompt + tool/MCP schemas + always-on
    /// config), so it is a lower bound on what every session in that project
    /// actually loads — including the MCP schema cost the catalog cannot read.
    pub fn baseline_floor_per_project(&self) -> Result<Vec<(String, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT s.project, MIN(e.ctx_start)
             FROM events e JOIN sessions s ON e.session_id = s.id
             WHERE e.ctx_start > 0
             GROUP BY s.project
             ORDER BY MIN(e.ctx_start) DESC",
        )?;
        let rows = stmt
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// The global empirical always-on floor — the smallest non-trivial prompt
    /// size observed anywhere.
    pub fn baseline_floor(&self) -> Result<i64> {
        let floor = self.conn.query_row(
            "SELECT COALESCE(MIN(ctx_start), 0) FROM events WHERE ctx_start > 0",
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

fn table_exists(conn: &Connection, table: &str) -> Result<bool> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [table],
        |row| row.get(0),
    )?;
    Ok(n > 0)
}

fn column_exists(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let names = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(names.iter().any(|name| name == column))
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

    #[test]
    fn an_outdated_store_schema_is_rejected_with_guidance() {
        // A db created by an older cclens (no sessions.root / surfaces.project)
        // must be refused with a re-analyze hint, not fail mid-query.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE sessions (id TEXT PRIMARY KEY, project TEXT NOT NULL);")
            .unwrap();
        let err = Store::from_connection(conn).err().expect("must be refused");
        assert!(err.to_string().contains("cclens analyze"));
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
    fn baseline_floor_is_the_smallest_nonzero_ctx_start_per_project() {
        let mut store = Store::in_memory().unwrap();
        // Project "alpha": floor 12000; project "beta": floor 40000.
        let mut alpha = session("a1");
        alpha.project = "alpha".to_string();
        store
            .ingest_session(
                &alpha,
                &[
                    span_at_ctx("git-commit", 30000),
                    span_at_ctx("pr-create", 12000),
                ],
                &[],
            )
            .unwrap();
        let mut beta = session("b1");
        beta.project = "beta".to_string();
        store
            .ingest_session(&beta, &[span_at_ctx("git-commit", 40000)], &[])
            .unwrap();

        assert_eq!(store.baseline_floor().unwrap(), 12000);
        assert_eq!(
            store.baseline_floor_per_project().unwrap(),
            vec![("beta".to_string(), 40000), ("alpha".to_string(), 12000)]
        );
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
    fn work_event_rows_carry_their_project() {
        let mut store = Store::in_memory().unwrap();
        let mut alpha = session("a1");
        alpha.project = "alpha".to_string();
        store.ingest_session(&alpha, &[], &[]).unwrap();
        store
            .ingest_work_events(
                "a1",
                &alpha.source_path,
                &[(1_000, "file_edit", "x.rs".to_string())],
            )
            .unwrap();

        assert_eq!(
            store.work_event_rows_by_project("file_edit").unwrap(),
            vec![("alpha".to_string(), "x.rs".to_string(), 1)]
        );
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
