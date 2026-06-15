//! SQLite store: persist extracted spans as events and query them back. This
//! layer knows SQL but nothing about raw Claude Code formats. See
//! `docs/specs/storage.md`.
//!
//! The schema is a subset of the spec's `events` table — the columns the
//! current pipeline populates. Deferred columns (`parent_id`, `source_line`,
//! `is_trailing`, `sub_*`, `attrs_json`) and the `surfaces` catalog are added
//! as their features land.

use anyhow::Result;
use rusqlite::Connection;

use crate::core::span::{Source, Span};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS sessions (
    id          TEXT PRIMARY KEY,
    project     TEXT NOT NULL,
    slug        TEXT NOT NULL,
    source_path TEXT NOT NULL,
    started_at  TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS events (
    id            INTEGER PRIMARY KEY,
    session_id    TEXT NOT NULL,
    source_path   TEXT NOT NULL,
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
    model         TEXT
);
CREATE INDEX IF NOT EXISTS events_by_surface ON events(surface_kind, surface_id);
";

/// Identity and provenance of one analyzed session.
pub struct SessionMeta {
    pub id: String,
    pub project: String,
    pub slug: String,
    pub source_path: String,
}

/// One row of the per-skill usage rollup.
#[derive(Debug, PartialEq)]
pub struct SkillUsage {
    pub skill: String,
    pub invocations: i64,
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
    /// re-ingest keyed on `source_path`; see `docs/specs/storage.md`).
    pub fn ingest_session(&mut self, session: &SessionMeta, spans: &[Span]) -> Result<()> {
        let started_at = spans
            .iter()
            .map(|span| span.started_epoch_ms)
            .min()
            .map(epoch_ms_to_rfc3339)
            .unwrap_or_default();

        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT OR REPLACE INTO sessions (id, project, slug, source_path, started_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            (
                &session.id,
                &session.project,
                &session.slug,
                &session.source_path,
                &started_at,
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
                    ctx_start, ctx_peak, model)
                 VALUES (?1, ?2, 'skill_invocation', 'skill', ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
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
                ),
            )?;
        }
        tx.commit()?;
        Ok(())
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
        }
    }

    fn session(id: &str) -> SessionMeta {
        SessionMeta {
            id: id.to_string(),
            project: "demo".to_string(),
            slug: "demo".to_string(),
            source_path: format!("/tmp/{id}.jsonl"),
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
            )
            .unwrap();
        store
            .ingest_session(&session("s2"), &[span("pr-create", 10, 5, 0.5)])
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
            .ingest_session(&session("s1"), &[span("git-commit", 100, 50, 2.0)])
            .unwrap();
        // Same source_path, different content — must supersede, not accumulate.
        store
            .ingest_session(&session("s1"), &[span("git-commit", 999, 999, 9.0)])
            .unwrap();

        let usage = store.skill_usage().unwrap();
        assert_eq!(usage.len(), 1);
        assert_eq!(usage[0].invocations, 1);
        assert_eq!(usage[0].out_tokens, 999);
    }
}
