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
    config_path   TEXT,
    static_tokens INTEGER,
    load_mode     TEXT NOT NULL,
    PRIMARY KEY (kind, id, scope)
);
";

/// Identity and provenance of one analyzed session.
pub struct SessionMeta {
    pub id: String,
    pub project: String,
    pub slug: String,
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
#[derive(Debug, PartialEq)]
pub struct SkillUsage {
    pub skill: String,
    pub invocations: i64,
    pub out_tokens: i64,
    pub ctx_growth: i64,
    pub duration_sec: f64,
}

/// One catalogued surface (effective scope already resolved).
#[derive(Debug, PartialEq)]
pub struct CatalogEntry {
    pub kind: String,
    pub id: String,
    pub static_tokens: Option<i64>,
    pub load_mode: String,
}

/// One skill event's cost, with its UTC start, for time bucketing.
#[derive(Debug, PartialEq)]
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

    /// An ephemeral in-memory store, for tests.
    pub fn in_memory() -> Result<Self> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(conn: Connection) -> Result<Self> {
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
               (id, project, slug, source_path, started_at, sub_tokens, sub_agent_count)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            (
                &session.id,
                &session.project,
                &session.slug,
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

    /// Insert tool-failure events for a session: `(epoch_ms, category)`. The
    /// category rides in `surface_id`. Call after `ingest_session`, whose
    /// delete-by-`source_path` already cleared prior rows for this file.
    pub fn ingest_tool_errors(
        &mut self,
        session_id: &str,
        source_path: &str,
        errors: &[(i64, &str)],
    ) -> Result<()> {
        let tx = self.conn.transaction()?;
        for (epoch_ms, category) in errors {
            // surface_kind stays NULL so these do not enter the surface catalog
            // join; the category rides in surface_id for error_counts only.
            tx.execute(
                "INSERT INTO events
                   (session_id, source_path, kind, surface_id,
                    started_at, started_epoch, duration_sec, out_tokens, ctx_growth,
                    ctx_start, ctx_peak)
                 VALUES (?1, ?2, 'tool_error', ?3, '', ?4, 0, 0, 0, 0, 0)",
                (session_id, source_path, *category, epoch_ms / 1000),
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

    /// Total tokens of always-on (startup_full) config read from files — what a
    /// session pays unconditionally from CLAUDE.md and non-conditional rules.
    pub fn always_on_config_tokens(&self) -> Result<i64> {
        let total = self.conn.query_row(
            "SELECT COALESCE(SUM(static_tokens), 0) FROM surfaces WHERE load_mode = 'startup_full'",
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

    /// Rebuild the surface catalog wholesale — it is a snapshot of current
    /// config, not an accumulation (see `docs/specs/storage.md`).
    pub fn replace_surfaces(&mut self, surfaces: &[Surface]) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM surfaces", ())?;
        for surface in surfaces {
            tx.execute(
                "INSERT OR REPLACE INTO surfaces
                   (kind, id, scope, config_path, static_tokens, load_mode)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                (
                    &surface.kind,
                    &surface.id,
                    surface.scope.label(),
                    &surface.config_path,
                    surface.static_tokens,
                    surface.load_mode.label(),
                ),
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// The whole catalog, one row per `(kind, id)` with the **effective** scope
    /// resolved — a project surface shadows a global one of the same id (see
    /// `docs/specs/storage.md`).
    pub fn catalog(&self) -> Result<Vec<CatalogEntry>> {
        // MAX(scope): 'project' > 'global' lexically, so the project row wins.
        // SQLite gives the bare columns (static_tokens, load_mode) the values
        // from the same row the MAX picked.
        let mut stmt = self.conn.prepare(
            "SELECT kind, id, static_tokens, load_mode, MAX(scope)
             FROM surfaces
             GROUP BY kind, id
             ORDER BY kind, id",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok(CatalogEntry {
                    kind: row.get(0)?,
                    id: row.get(1)?,
                    static_tokens: row.get(2)?,
                    load_mode: row.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
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
    fn surface_catalog_resolves_project_over_global() {
        let mut store = Store::in_memory().unwrap();
        store
            .replace_surfaces(&[
                surface("git-commit", Scope::Global, 100),
                surface("git-commit", Scope::Project, 250), // shadows global
                surface("pr-create", Scope::Global, 40),
            ])
            .unwrap();

        let catalog = store.catalog().unwrap();

        assert_eq!(catalog.len(), 2);
        let git = catalog.iter().find(|e| e.id == "git-commit").unwrap();
        assert_eq!(git.static_tokens, Some(250)); // the project row won
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
            ])
            .unwrap();

        assert_eq!(store.always_on_config_tokens().unwrap(), 1500);
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

        let catalog = store.catalog().unwrap();
        assert_eq!(catalog.len(), 1);
        assert_eq!(catalog[0].id, "new");
    }
}
