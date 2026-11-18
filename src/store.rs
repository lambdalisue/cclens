//! SQLite store: persist extracted spans as events and query them back. This
//! layer knows SQL but nothing about raw Claude Code formats. See
//! `docs/specs/storage.md`.
//!
//! The schema tracks the spec's `events` table; its `attrs_json` is the one
//! column deferred until a report needs it (surfaces already carry theirs).

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension};

use crate::core::cache::{BreakCause, CacheObservation, CacheRow};
use crate::core::context_load::{
    ContextLoad, LoadHistory, is_observed, recording_floor, version_key,
};
use crate::core::effect::{ConfigChange, DateSource, SessionSample};
use crate::core::hook::{CallResult, HookOutcome, HookTrace, StoredTrace, ToolCalls, Triggers};
use crate::core::span::{SessionStart, Source, Span};
use crate::core::subagent::{SpawnModels, SubagentRun};
use crate::core::surface::{Scope, Surface, is_usage_measurable};
use crate::core::thrash::{BashOutcome, FileEdit, SandboxedRun, StepKind, WorkStep};
use crate::core::tool_load::{ToolContext, ToolLoadStat};
use crate::core::usage::UsageEvent;

/// The `meta` key holding the analysis semantics a store's rows were built with.
const ANALYZER_META_KEY: &str = "analyzer_version";

/// The analysis semantics this build produces. **Bump it whenever extraction or
/// classification changes what an unchanged transcript yields** — that is the
/// only way rows behind the incremental-ingest skip get rebuilt. It is not the
/// crate version: a release that changes no analysis should not force everyone
/// through a full re-analyze.
const ANALYZER_VERSION: &str = "10";

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS sessions (
    id              TEXT PRIMARY KEY,
    project         TEXT NOT NULL,
    slug            TEXT NOT NULL,
    root            TEXT NOT NULL DEFAULT '',
    source_path     TEXT NOT NULL,
    started_at      TEXT NOT NULL,
    sub_tokens      INTEGER NOT NULL DEFAULT 0,
    sub_agent_count INTEGER NOT NULL DEFAULT 0,
    -- 1 once this session's hook activity (hook_runs, tool_call events) was
    -- extracted; a session stored earlier holds none and must not count as
    -- one where no hook fired.
    hook_scanned    INTEGER NOT NULL DEFAULT 0,
    tool_calls      INTEGER
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
    is_trailing          INTEGER NOT NULL DEFAULT 0,
    check_outcome        TEXT
);
-- One row per subagent execution: which agent type ran, and what its own output
-- cost. The session total says how much subagent work cost; this says *whose*.
-- `source_path` is the parent session's transcript — the ingest delete key, as
-- for events — while `run_path` points at the subagent's own transcript.
CREATE TABLE IF NOT EXISTS subagent_runs (
    id                INTEGER PRIMARY KEY,
    session_id        TEXT NOT NULL,
    source_path       TEXT NOT NULL,
    run_path          TEXT NOT NULL,
    agent             TEXT,
    agent_id          TEXT NOT NULL,
    tool_use_id       TEXT,
    root_tool_use_id  TEXT,
    parent_agent_id   TEXT,
    spawn_depth       INTEGER NOT NULL DEFAULT 1,
    model             TEXT,
    requested_model   TEXT,
    prompt_id         TEXT,
    out_tokens        INTEGER NOT NULL DEFAULT 0,
    prompt_tokens     INTEGER,
    started_at        TEXT NOT NULL,
    started_epoch     INTEGER NOT NULL
);
-- One row per main-thread API request: the prompt split into cache read,
-- cache write by lifetime, and uncached input; on a request that broke the
-- cache, the cause and the surfaces it implicates (a JSON array of kind:id).
-- `source_path` is the ingest delete key, as for events.
CREATE TABLE IF NOT EXISTS cache_requests (
    id              INTEGER PRIMARY KEY,
    session_id      TEXT NOT NULL,
    source_path     TEXT NOT NULL,
    started_epoch   INTEGER NOT NULL,
    model           TEXT NOT NULL,
    uncached        INTEGER NOT NULL DEFAULT 0,
    cache_read      INTEGER NOT NULL DEFAULT 0,
    cache_write_5m  INTEGER NOT NULL DEFAULT 0,
    cache_write_1h  INTEGER NOT NULL DEFAULT 0,
    gap_sec         REAL,
    ttl_sec         INTEGER NOT NULL DEFAULT 300,
    break_cause     TEXT,
    rebuilt_tokens  INTEGER NOT NULL DEFAULT 0,
    break_surfaces  TEXT
);
-- The session cost Claude Code itself reported. A session absent here has no
-- reported cost: unknown, not free.
CREATE TABLE IF NOT EXISTS session_costs (
    session_id     TEXT PRIMARY KEY,
    source_path    TEXT NOT NULL,
    total_cost_usd REAL NOT NULL
);
CREATE TABLE IF NOT EXISTS surfaces (
    kind          TEXT NOT NULL,
    id            TEXT NOT NULL,
    scope         TEXT NOT NULL,
    project       TEXT NOT NULL DEFAULT '',
    config_path   TEXT,
    -- config_path with symlinks resolved; NULL when it cannot be. Transcripts
    -- record a loaded file by its real path, so context_load events join here.
    resolved_path TEXT,
    static_tokens INTEGER,
    startup_tokens INTEGER,
    load_mode     TEXT NOT NULL,
    attrs_json    TEXT,
    PRIMARY KEY (kind, id, scope, project)
);
-- One row per hook trace. Not a fire count: a hook that passes silently leaves
-- no row, so a hook's uses come from its trigger (`tool_call` events and the
-- like) instead. Traces carry no catalog id — which catalogued hook a trace
-- belongs to depends on the matchers installed now — so the core resolves them
-- at read time (docs/specs/surfaces.md; events.md, Hook runs).
CREATE TABLE IF NOT EXISTS hook_runs (
    id             INTEGER PRIMARY KEY,
    session_id     TEXT NOT NULL,
    source_path    TEXT NOT NULL,
    started_at     TEXT NOT NULL,
    started_epoch  INTEGER NOT NULL,
    event          TEXT NOT NULL,
    subject        TEXT,
    command        TEXT,
    outcome        TEXT NOT NULL,
    exit_code      INTEGER,
    duration_ms    INTEGER,
    context_tokens INTEGER NOT NULL DEFAULT 0,
    retry_calls    INTEGER
);
-- Request shapes, content-addressed by the transcript's shape hash: weights,
-- counts and tool names only, never text. A hash names the same shape wherever
-- it appears, so a re-ingest never deletes these rows.
CREATE TABLE IF NOT EXISTS request_shapes (
    hash          TEXT PRIMARY KEY,
    system_tokens INTEGER NOT NULL,
    system_blocks INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS request_shape_tools (
    hash     TEXT NOT NULL,
    name     TEXT NOT NULL,
    server   TEXT,
    deferred INTEGER NOT NULL,
    tokens   INTEGER NOT NULL,
    PRIMARY KEY (hash, name)
);
CREATE TABLE IF NOT EXISTS session_shapes (
    session_id  TEXT NOT NULL,
    source_path TEXT NOT NULL,
    hash        TEXT NOT NULL,
    first_epoch INTEGER NOT NULL,
    requests    INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS session_tools (
    session_id     TEXT NOT NULL,
    source_path    TEXT NOT NULL,
    name           TEXT NOT NULL,
    server         TEXT,
    eager_tokens   INTEGER,
    listing_tokens INTEGER,
    loaded         INTEGER NOT NULL,
    calls          INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS session_mcp_instructions (
    session_id  TEXT NOT NULL,
    source_path TEXT NOT NULL,
    server      TEXT NOT NULL,
    tokens      INTEGER NOT NULL
);
-- When each file-backed surface last changed and the friction it declares it
-- targets: the boundary of the before/after comparison (core::effect).
CREATE TABLE IF NOT EXISTS surface_changes (
    kind             TEXT NOT NULL,
    id               TEXT NOT NULL,
    scope            TEXT NOT NULL,
    project          TEXT NOT NULL DEFAULT '',
    changed_epoch    INTEGER NOT NULL,
    introduced_epoch INTEGER NOT NULL,
    date_source      TEXT NOT NULL,
    targets          TEXT NOT NULL DEFAULT '',
    always_on        INTEGER NOT NULL DEFAULT 1,
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
CREATE INDEX subagent_runs_by_agent ON subagent_runs(agent);
CREATE INDEX subagent_runs_by_source ON subagent_runs(source_path);
CREATE INDEX hook_runs_by_source ON hook_runs(source_path);
CREATE INDEX session_shapes_by_source ON session_shapes(source_path);
CREATE INDEX session_shapes_by_session ON session_shapes(session_id, first_epoch);
CREATE INDEX session_tools_by_source ON session_tools(source_path);
CREATE INDEX session_tools_by_name ON session_tools(name);
CREATE INDEX cache_requests_by_source ON cache_requests(source_path);
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
-- The same for agent spawns: the requested model rides in `source` and the
-- calling turn's model in `model`. Joined to the run the spawn started, it is one
-- query to ask which spawns ran on the main loop's model by default.
CREATE VIEW agent_spawns AS
SELECT e.session_id, s.project, e.surface_id AS agent,
       e.source AS requested_model, e.model AS caller_model,
       r.model AS run_model, e.target AS tool_use_id, e.started_epoch
FROM events e JOIN sessions s ON e.session_id = s.id
LEFT JOIN subagent_runs r ON r.source_path = e.source_path AND r.tool_use_id = e.target
WHERE e.kind = 'agent_spawn';

-- Every config-file load, resolved against the catalog by canonical path. One
-- file can sit under several catalog rows (a home-directory session scans
-- ~/.claude as a project; a worktree session can scan the main checkout), so
-- each load is attributed to exactly one: the row of the loading session's own
-- project, else the global row, else the first. An unmatched load (a file since
-- deleted, or one in a root no longer catalogued) keeps NULL surface columns
-- rather than being dropped.
CREATE VIEW context_loads AS
WITH candidates AS (
    SELECT e.id AS event_id, e.session_id, s.project, e.target, e.surface_id AS file_class,
           e.source AS load_trigger, e.model AS cc_version, e.started_epoch,
           f.kind, f.id AS surface_id, f.scope, f.project AS surface_project,
           f.load_mode, f.static_tokens,
           ROW_NUMBER() OVER (
               PARTITION BY e.id
               ORDER BY (f.scope = 'project' AND f.project = s.project) DESC,
                        (f.scope = 'global') DESC, f.kind, f.project, f.id
           ) AS rn
    FROM events e JOIN sessions s ON e.session_id = s.id
    LEFT JOIN surfaces f ON f.resolved_path = e.target
    WHERE e.kind = 'context_load'
)
SELECT event_id,
       session_id,
       project,
       target          AS path,
       file_class,
       load_trigger,
       cc_version,
       kind            AS surface_kind,
       surface_id,
       scope           AS surface_scope,
       surface_project,
       load_mode,
       static_tokens,
       started_epoch
FROM candidates
WHERE rn = 1;
-- The same for bash_cmd events: the check key, outcome and sandbox flag the
-- retry-loop and unsandboxed-retry detectors read (docs/specs/events.md).
CREATE VIEW bash_commands AS
SELECT e.session_id               AS session_id,
       s.project                  AS project,
       e.surface_id               AS command,
       e.target                   AS check_key,
       e.source                   AS outcome,
       e.check_outcome            AS check_outcome,
       e.model IS 'unsandboxed'   AS unsandboxed,
       e.started_epoch            AS started_epoch
FROM events e JOIN sessions s ON e.session_id = s.id
WHERE e.kind = 'bash_cmd';
";

/// One work event as stored. The detail columns carry a different fact per
/// kind (`docs/specs/storage.md`): a file edit's full path in `target`; a Bash
/// run's check key in `target`, its outcome in `source`, and the sandbox flag
/// in `model`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkRow<'a> {
    pub epoch_ms: i64,
    pub kind: &'a str,
    /// The ranking identity: a command's leading word, or a file's basename.
    pub id: &'a str,
    pub target: Option<&'a str>,
    pub source: Option<&'a str>,
    pub model: Option<&'a str>,
    /// What a check run said, apart from what the run did (`source`): a run
    /// the sandbox refused failed, but the check it carried never reported.
    pub check_outcome: Option<&'a str>,
}

impl<'a> WorkRow<'a> {
    pub fn file_edit(epoch_ms: i64, basename: &'a str, path: Option<&'a str>) -> Self {
        WorkRow {
            epoch_ms,
            kind: "file_edit",
            id: basename,
            target: path,
            source: None,
            model: None,
            check_outcome: None,
        }
    }

    pub fn bash(
        epoch_ms: i64,
        leading: &'a str,
        outcome: Option<&'a str>,
        unsandboxed: bool,
    ) -> Self {
        WorkRow {
            epoch_ms,
            kind: "bash_cmd",
            id: leading,
            target: None,
            source: outcome,
            model: unsandboxed.then_some("unsandboxed"),
            check_outcome: None,
        }
    }

    /// Mark the run as a check: its key and what the check said.
    pub fn checked(self, key: &'a str, outcome: Option<&'a str>) -> Self {
        WorkRow {
            target: Some(key),
            check_outcome: outcome,
            ..self
        }
    }
}

/// Stamped into `PRAGMA user_version`. Bumped only for a change `migrate` cannot
/// reconcile — an existing column removed, retyped, or given a new meaning.
/// Additive drift (a new column, table, or view) needs no bump: `migrate`
/// converges any older store onto the declared schema in place.
///
/// v2: `subagent_runs.model` changed from the model a spawn *requested* to the
/// model its run *ran on*; the requested alias moved to `requested_model`. A v1
/// build would keep writing the alias into `model`, so it must refuse the store.
const SCHEMA_VERSION: i64 = 2;

/// Tables holding analysis history. Claude Code prunes transcripts on its own
/// retention schedule, after which these rows are the only surviving record of
/// those sessions — so a shape change adds columns in place, never rebuilds.
const MIGRATED_TABLES: [&str; 8] = [
    "sessions",
    "events",
    "subagent_runs",
    "hook_runs",
    "cache_requests",
    "session_costs",
    "ingested_files",
    "meta",
];

/// Tables regenerated wholesale from live config on every analyze
/// (`replace_surfaces`), so a shape change costs nothing to drop and recreate.
const REBUILT_TABLES: [&str; 2] = ["surfaces", "surface_changes"];

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
    /// The whole definition's weight — for a skill or agent, what its body
    /// costs when invoked, not what having it installed costs.
    pub static_tokens: Option<i64>,
    /// What this surface adds to every session's startup context; `None` when
    /// unknown (an MCP server's tool schema).
    pub startup_tokens: Option<i64>,
    pub load_mode: String,
    pub uses: i64,
    /// Whether `uses` can be read as evidence: true for usage-measurable kinds,
    /// and for a hook only once its trigger has been counted
    /// (`docs/specs/surfaces.md`). Zero uses of an unmeasurable row means nothing.
    pub measurable: bool,
}

/// One catalogued rule or `CLAUDE.md` row with its load history.
#[derive(Debug, PartialEq, serde::Serialize)]
pub struct RuleLoadStat {
    pub kind: String,
    pub id: String,
    pub scope: String,
    pub project: String,
    pub load_mode: String,
    pub static_tokens: Option<u64>,
    pub loads: u64,
    /// Distinct observed sessions in its reach it loaded in — the numerator of
    /// `observed_sessions`, so the share stays within one population. Loads and
    /// their cost count every session.
    pub sessions: u64,
    /// UTC unix seconds of the latest load; `None` when it never loaded.
    pub last_loaded_epoch: Option<i64>,
    /// Sessions in its reach that could have recorded a load.
    pub observed_sessions: u64,
}

impl RuleLoadStat {
    pub fn history(&self) -> LoadHistory {
        LoadHistory {
            loads: self.loads,
            sessions: self.sessions,
            observed_sessions: self.observed_sessions,
        }
    }
}

/// What one agent type cost: how many times it ran and the output tokens those
/// runs wrote. `agent` is `None` for runs whose type could not be read.
#[derive(Debug, PartialEq, serde::Serialize)]
pub struct AgentCost {
    pub agent: Option<String>,
    pub runs: i64,
    pub out_tokens: i64,
}

/// What one agent type cost on one model: its runs, the output they wrote and
/// the prompt they re-read. `model` is the model the runs ran on.
#[derive(Debug, PartialEq, serde::Serialize)]
pub struct AgentModelCost {
    pub agent: Option<String>,
    pub model: Option<String>,
    pub runs: i64,
    pub out_tokens: i64,
    /// `None` when any of the runs was stored before prompt tokens were
    /// measured: a partial sum would pass for the whole.
    pub prompt_tokens: Option<i64>,
}

/// What one session paid at start besides the conversation, component by
/// component, measured from its request shape (`docs/specs/surfaces.md`
/// "Decomposing the residual"). Token figures are estimates on the config
/// estimator's scale.
#[derive(Debug, PartialEq, serde::Serialize)]
pub struct Decomposition {
    pub session_id: String,
    pub project: String,
    pub started_at: String,
    /// The context the session was observed to start with; `None` when its
    /// transcript carried no assistant record to read it from.
    pub observed_ctx: Option<i64>,
    pub system_tokens: i64,
    pub builtin_tool_tokens: i64,
    pub mcp_tool_tokens: i64,
    pub deferred_listing_tokens: i64,
    pub mcp_instruction_tokens: i64,
    /// The readable always-on config for that session's project.
    pub config_tokens: i64,
}

/// One MCP server's measured startup cost and how much it was used.
#[derive(Debug, PartialEq, serde::Serialize)]
pub struct McpServerCost {
    pub server: String,
    pub startup_tokens: i64,
    /// Sessions whose transcript showed the server's tools or instructions.
    pub sessions: i64,
    pub calls: i64,
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

/// `create_dir_all` with owner-only permissions on every directory it creates —
/// the mode the XDG Base Directory spec prescribes for a directory a program
/// creates on the user's behalf. Directories that already exist keep theirs.
fn create_private_dir_all(dir: &std::path::Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
    }
    #[cfg(not(unix))]
    std::fs::create_dir_all(dir)
}

/// Make sure the store file is readable only by its owner: create it `0600`
/// when absent, and narrow it when it is already there with group/other bits
/// set. Creating it here rather than letting SQLite create it under the umask
/// is what removes the window — SQLite would otherwise write the schema into a
/// world-readable file and only then have it chmod'ed — and it lets SQLite
/// inherit the mode for the rollback journal it writes beside the database.
///
/// An existing store is narrowed rather than left alone: a store an older
/// cclens created under the umask is the realistic case, and nobody
/// deliberately shares this store. Only a regular file is touched, so pointing
/// `--db` at a directory fails at SQLite instead of silently re-moding it, and
/// a store owned by someone else is left as it is — re-moding it is not this
/// process's call, and refusing to run over it would break a store legitimately
/// created under another account.
fn ensure_owner_only(path: &std::path::Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::fs::{OpenOptions, Permissions};
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
        {
            Ok(_) => return Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e).with_context(|| format!("create store {}", path.display())),
        }
        let meta = std::fs::metadata(path)
            .with_context(|| format!("read store permissions {}", path.display()))?;
        let mode = meta.permissions().mode();
        if meta.is_file() && mode & 0o077 != 0 {
            // 0600 outright rather than masking the existing mode: masking would
            // carry over a stray execute bit, and could leave the owner without
            // the read/write the store needs to be opened at all.
            match std::fs::set_permissions(path, Permissions::from_mode(0o600)) {
                Ok(()) => {}
                // Not our file to re-mode. Everything else is a real failure.
                Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {}
                Err(e) => {
                    return Err(e)
                        .with_context(|| format!("restrict store permissions {}", path.display()));
                }
            }
        }
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

impl Store {
    /// Open (creating if needed) a store at `path`, ensuring the schema exists.
    /// The directory is created too: the default store lives under the user
    /// state directory, which need not exist yet, and SQLite will not create it.
    ///
    /// Both are **owner-only**. The store aggregates transcript-derived data —
    /// file paths, error excerpts, project names — for every project at one
    /// well-known user-level path, so the umask default (commonly a
    /// group/world-readable `0755` directory and `0644` file) would expose it to
    /// other local accounts.
    pub fn open(path: impl AsRef<std::path::Path>) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            create_private_dir_all(parent)
                .with_context(|| format!("create store directory {}", parent.display()))?;
        }
        ensure_owner_only(path)?;
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
            // An agent spawn keeps its call id in `target` — the join key to the
            // subagent run it started (`ingest_subagent_runs`) — the model its
            // call requested in `source`, and the calling turn's model in `model`.
            tx.execute(
                "INSERT INTO events
                   (session_id, source_path, kind, surface_kind, surface_id, source,
                    started_at, started_epoch, duration_sec, out_tokens, ctx_growth,
                    ctx_start, ctx_peak, model, target)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 0, 0, 0, 0, 0, ?9, ?10)",
                (
                    &session.id,
                    &session.source_path,
                    kind,
                    &event.surface_kind,
                    &event.surface_id,
                    &event.requested_model,
                    epoch_ms_to_rfc3339(event.started_epoch_ms),
                    event.started_epoch_ms / 1000,
                    &event.caller_model,
                    &event.tool_use_id,
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

    /// Persist a session's subagent runs, and settle the cost of the spawns that
    /// started them.
    ///
    /// Two rows describe one subagent from opposite sides: the `agent_spawn`
    /// event is what the main thread *did*, the run is what it *cost*. They are
    /// joined by the spawn call's id — kept in the event's `target` — so the
    /// spawn's own `out_tokens` becomes the run's output and its `sub_tokens`
    /// the output of everything that run spawned in turn. A spawn whose run is
    /// missing keeps its zeros rather than borrowing another's figures.
    ///
    /// Call after `ingest_session`, which inserted those spawn events.
    pub fn ingest_subagent_runs(
        &mut self,
        session_id: &str,
        source_path: &str,
        runs: &[SubagentRun],
    ) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "DELETE FROM subagent_runs WHERE source_path = ?1",
            (source_path,),
        )?;
        for run in runs {
            tx.execute(
                "INSERT INTO subagent_runs
                   (session_id, source_path, run_path, agent, agent_id, tool_use_id,
                    root_tool_use_id, parent_agent_id, spawn_depth, model, requested_model,
                    prompt_id, out_tokens, prompt_tokens, started_at, started_epoch)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
                (
                    session_id,
                    source_path,
                    &run.run_path,
                    &run.agent,
                    &run.agent_id,
                    &run.tool_use_id,
                    &run.root_tool_use_id,
                    &run.parent_agent_id,
                    run.spawn_depth,
                    &run.model,
                    &run.requested_model,
                    &run.prompt_id,
                    run.out_tokens,
                    run.prompt_tokens,
                    epoch_ms_to_rfc3339(run.started_epoch_ms),
                    run.started_epoch_ms / 1000,
                ),
            )?;
        }
        tx.execute(
            // Every subquery is scoped to this transcript. A call id is unique
            // within a session but nothing guarantees it across sessions, and an
            // unscoped scalar subquery would silently pick an arbitrary matching
            // row while the sums added another session's runs on top.
            "UPDATE events
                SET out_tokens = COALESCE((SELECT r.out_tokens FROM subagent_runs r
                                            WHERE r.source_path = events.source_path
                                              AND r.tool_use_id = events.target), 0),
                    sub_tokens = COALESCE((SELECT SUM(r.out_tokens) FROM subagent_runs r
                                            WHERE r.source_path = events.source_path
                                              AND r.root_tool_use_id = events.target
                                              AND r.tool_use_id IS NOT events.target), 0),
                    sub_agent_count = (SELECT COUNT(*) FROM subagent_runs r
                                        WHERE r.source_path = events.source_path
                                          AND r.root_tool_use_id = events.target
                                          AND r.tool_use_id IS NOT events.target)
              WHERE kind = 'agent_spawn' AND source_path = ?1 AND target IS NOT NULL",
            (source_path,),
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Replace a session's prompt-cache requests and its reported cost. The
    /// whole sequence is replaced, never appended: a break is defined against
    /// the request before it, so a grown session is re-measured from its start.
    pub fn ingest_prompt_cache(
        &mut self,
        session_id: &str,
        source_path: &str,
        observations: &[CacheObservation],
        reported_cost_usd: Option<f64>,
    ) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "DELETE FROM cache_requests WHERE source_path = ?1",
            (source_path,),
        )?;
        tx.execute(
            "DELETE FROM session_costs WHERE source_path = ?1",
            (source_path,),
        )?;
        for obs in observations {
            let request = &obs.request;
            let (cause, rebuilt, surfaces) = match &obs.cache_break {
                Some(b) => (
                    Some(b.cause.label()),
                    b.rebuilt_tokens as i64,
                    (!b.surfaces.is_empty())
                        .then(|| serde_json::to_string(&b.surfaces))
                        .transpose()?,
                ),
                None => (None, 0, None),
            };
            tx.execute(
                "INSERT INTO cache_requests
                   (session_id, source_path, started_epoch, model, uncached, cache_read,
                    cache_write_5m, cache_write_1h, gap_sec, ttl_sec, break_cause,
                    rebuilt_tokens, break_surfaces)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                rusqlite::params![
                    session_id,
                    source_path,
                    request.sent_epoch_ms / 1000,
                    &request.model,
                    request.uncached as i64,
                    request.cache_read as i64,
                    request.cache_write_5m as i64,
                    request.cache_write_1h as i64,
                    obs.gap_ms.map(|ms| ms as f64 / 1000.0),
                    obs.ttl.millis() / 1000,
                    cause,
                    rebuilt,
                    surfaces,
                ],
            )?;
        }
        if let Some(cost) = reported_cost_usd {
            tx.execute(
                "INSERT OR REPLACE INTO session_costs (session_id, source_path, total_cost_usd)
                 VALUES (?1, ?2, ?3)",
                (session_id, source_path, cost),
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Every stored prompt-cache request with its session's project, in time
    /// order.
    pub fn cache_rows(&self) -> Result<Vec<CacheRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT c.session_id, s.project, c.started_epoch, c.uncached, c.cache_read,
                    c.cache_write_5m, c.cache_write_1h, c.break_cause, c.rebuilt_tokens,
                    c.break_surfaces
             FROM cache_requests c JOIN sessions s ON c.session_id = s.id
             ORDER BY c.started_epoch, c.id",
        )?;
        let rows = stmt
            .query_map([], |row| {
                let cause: Option<String> = row.get(7)?;
                let surfaces: Option<String> = row.get(9)?;
                Ok(CacheRow {
                    session_id: row.get(0)?,
                    project: row.get(1)?,
                    started_epoch: row.get(2)?,
                    uncached: row.get(3)?,
                    cache_read: row.get(4)?,
                    cache_write_5m: row.get(5)?,
                    cache_write_1h: row.get(6)?,
                    // A label this build does not know still marks a break.
                    break_cause: cause
                        .map(|label| BreakCause::from_label(&label).unwrap_or(BreakCause::Unknown)),
                    rebuilt_tokens: row.get(8)?,
                    surfaces: surfaces
                        .and_then(|json| serde_json::from_str(&json).ok())
                        .unwrap_or_default(),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// `(sum, sessions)` of the cost Claude Code reported, over the sessions
    /// that reported one.
    pub fn reported_cost(&self) -> Result<(f64, i64)> {
        let totals = self.conn.query_row(
            "SELECT COALESCE(SUM(total_cost_usd), 0.0), COUNT(*) FROM session_costs",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        Ok(totals)
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

    /// Persist work events — Bash runs and file edits — kept out of the catalog
    /// join (`surface_kind` NULL), in the order given. That order is the
    /// transcript's, and it is what `work_steps` reads back: an edit and the
    /// check runs around it often share one second, so the row id is the only
    /// thing that says which came first.
    pub fn ingest_work_events(
        &mut self,
        session_id: &str,
        source_path: &str,
        events: &[WorkRow<'_>],
    ) -> Result<()> {
        let tx = self.conn.transaction()?;
        for row in events {
            tx.execute(
                "INSERT INTO events
                   (session_id, source_path, kind, surface_id, target, source, model,
                    check_outcome, started_at, started_epoch, duration_sec, out_tokens,
                    ctx_growth, ctx_start, ctx_peak)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, '', ?9, 0, 0, 0, 0, 0)",
                (
                    session_id,
                    source_path,
                    row.kind,
                    row.id,
                    row.target,
                    row.source,
                    row.model,
                    row.check_outcome,
                    row.epoch_ms / 1000,
                ),
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Persist a session's hook activity: its traces, each with the retry cost
    /// of a block (`core::hook::retry_calls`), and a `tool_call` event per tool
    /// call `(epoch_ms, tool)` — the trigger a tool-event hook is counted
    /// against. Traces replace on `source_path` like `subagent_runs`; the
    /// `tool_call` rows rely on `ingest_session`'s delete having run first.
    pub fn ingest_hook_activity(
        &mut self,
        session_id: &str,
        source_path: &str,
        traces: &[(HookTrace, Option<u32>)],
        calls: &[(i64, &str, CallResult)],
    ) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "DELETE FROM hook_runs WHERE source_path = ?1",
            (source_path,),
        )?;
        tx.execute(
            "UPDATE sessions SET hook_scanned = 1 WHERE id = ?1",
            (session_id,),
        )?;
        for (trace, retry_calls) in traces {
            tx.execute(
                "INSERT INTO hook_runs
                   (session_id, source_path, started_at, started_epoch, event, subject,
                    command, outcome, exit_code, duration_ms, context_tokens, retry_calls)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                rusqlite::params![
                    session_id,
                    source_path,
                    epoch_ms_to_rfc3339(trace.epoch_ms),
                    trace.epoch_ms / 1000,
                    &trace.event,
                    &trace.subject,
                    &trace.command,
                    trace.outcome.label(),
                    trace.exit_code,
                    trace.duration_ms,
                    trace.context_tokens as i64,
                    retry_calls,
                ],
            )?;
        }
        for (epoch_ms, tool, result) in calls {
            tx.execute(
                "INSERT INTO events
                   (session_id, source_path, kind, surface_id, source,
                    started_at, started_epoch, duration_sec, out_tokens, ctx_growth,
                    ctx_start, ctx_peak)
                 VALUES (?1, ?2, 'tool_call', ?3, ?4, '', ?5, 0, 0, 0, 0, 0)",
                (
                    session_id,
                    source_path,
                    *tool,
                    result.label(),
                    epoch_ms / 1000,
                ),
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Every hook trace with its session's project, oldest first. The trace's
    /// `epoch_ms` comes back at the stored second's resolution.
    pub fn hook_traces(&self) -> Result<Vec<StoredTrace>> {
        let mut stmt = self.conn.prepare(
            "SELECT h.started_epoch, h.event, h.subject, h.command, h.outcome, h.exit_code,
                    h.duration_ms, h.context_tokens, h.retry_calls, s.project
             FROM hook_runs h JOIN sessions s ON h.session_id = s.id
             ORDER BY h.started_epoch, h.id",
        )?;
        let rows = stmt
            .query_map([], |row| {
                let outcome: String = row.get(4)?;
                Ok(StoredTrace {
                    trace: HookTrace {
                        epoch_ms: row.get::<_, i64>(0)? * 1000,
                        event: row.get(1)?,
                        subject: row.get(2)?,
                        command: row.get(3)?,
                        outcome: HookOutcome::from_label(&outcome).unwrap_or(HookOutcome::Ok),
                        exit_code: row.get(5)?,
                        duration_ms: row.get(6)?,
                        context_tokens: row.get::<_, i64>(7)? as u64,
                        tool_use_id: None,
                    },
                    project: row.get(9)?,
                    retry_calls: row.get(8)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// What hooks installed in `project` (`None` = globally, so every session)
    /// can be triggered by: tool calls per tool, prompts, and sessions.
    pub fn hook_triggers(&self, project: Option<&str>) -> Result<Triggers> {
        let in_scope = "s.hook_scanned = 1 AND (?1 IS NULL OR s.project = ?1)";
        let mut stmt = self.conn.prepare(&format!(
            "SELECT e.surface_id, e.source, COUNT(*) FROM events e JOIN sessions s ON e.session_id = s.id
             WHERE e.kind = 'tool_call' AND {in_scope}
             GROUP BY e.surface_id, e.source ORDER BY e.surface_id"
        ))?;
        let rows = stmt
            .query_map([project], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, i64>(2)? as u64,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut tool_calls: Vec<ToolCalls> = Vec::new();
        for (tool, result, count) in rows {
            if tool_calls.last().is_none_or(|calls| calls.tool != tool) {
                tool_calls.push(ToolCalls {
                    tool,
                    ..ToolCalls::default()
                });
            }
            let calls = tool_calls.last_mut().expect("pushed above");
            match result.as_deref().and_then(CallResult::from_label) {
                Some(CallResult::Succeeded) => calls.succeeded += count,
                Some(CallResult::Failed) => calls.failed += count,
                Some(CallResult::Denied) => calls.denied += count,
                Some(CallResult::Pending) | None => calls.pending += count,
            }
        }
        let prompts: i64 = self.conn.query_row(
            &format!(
                "SELECT COUNT(*) FROM events e JOIN sessions s ON e.session_id = s.id
                 WHERE e.kind = 'prompt' AND {in_scope}"
            ),
            [project],
            |row| row.get(0),
        )?;
        let sessions: i64 = self.conn.query_row(
            &format!("SELECT COUNT(*) FROM sessions s WHERE {in_scope}"),
            [project],
            |row| row.get(0),
        )?;
        Ok(Triggers {
            tool_calls,
            prompts: prompts as u64,
            sessions: sessions as u64,
        })
    }

    /// Persist a session's config-file loads as `context_load` events: the
    /// path in `target` (the join key into `surfaces.resolved_path`), the file
    /// class in `surface_id`, the trigger in `source`, the writing Claude Code
    /// version in `model`. `surface_kind` stays
    /// NULL — which surface a path is gets decided at read time, against the
    /// current catalog (`docs/specs/events.md`).
    pub fn ingest_context_loads(
        &mut self,
        session_id: &str,
        source_path: &str,
        loads: &[ContextLoad],
    ) -> Result<()> {
        let tx = self.conn.transaction()?;
        for load in loads {
            tx.execute(
                "INSERT INTO events
                   (session_id, source_path, kind, surface_id, source, target, model,
                    started_at, started_epoch, duration_sec, out_tokens, ctx_growth,
                    ctx_start, ctx_peak)
                 VALUES (?1, ?2, 'context_load', ?3, ?4, ?5, ?6, ?7, ?8, 0, 0, 0, 0, 0)",
                (
                    session_id,
                    source_path,
                    load.class.label(),
                    load.trigger.label(),
                    &load.path,
                    &load.version,
                    epoch_ms_to_rfc3339(load.epoch_ms),
                    load.epoch_ms.div_euclid(1000),
                ),
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Load history of every catalogued rule and `CLAUDE.md` row
    /// (`docs/specs/surfaces.md` "Rule loads"). Loads join by resolved path and
    /// are attributed to one row each (the `context_loads` view). The observed
    /// sessions are the gate, judged per Claude Code version rather than per
    /// timestamp: a session counts when the newest version that wrote its loads
    /// is at or above the oldest version seen writing an on-demand load
    /// (`core::context_load::recording_floor`), so a transcript from an install
    /// that cannot record loads contributes nothing rather than a zero. A row's
    /// reach follows the attribution rule, so its numerator and denominator are
    /// one population: a project row reaches its own project's sessions, a
    /// global row every session whose project has no row of its own for the
    /// same file.
    pub fn rule_load_stats(&self) -> Result<Vec<RuleLoadStat>> {
        // Each session's newest load version, and whether it loaded on demand.
        let mut session_versions: BTreeMap<String, (String, Option<String>)> = BTreeMap::new();
        let mut on_demand_versions: Vec<String> = Vec::new();
        {
            let mut stmt = self.conn.prepare(
                "SELECT session_id, project, cc_version, load_trigger FROM context_loads",
            )?;
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                let session: String = row.get(0)?;
                let project: String = row.get(1)?;
                let version: Option<String> = row.get(2)?;
                let trigger: Option<String> = row.get(3)?;
                if trigger.as_deref() == Some("on_demand")
                    && let Some(version) = &version
                {
                    on_demand_versions.push(version.clone());
                }
                let entry = session_versions
                    .entry(session)
                    .or_insert_with(|| (project, None));
                let newer = match (&entry.1, &version) {
                    (_, None) => false,
                    (None, Some(_)) => true,
                    (Some(held), Some(seen)) => version_key(seen) > version_key(held),
                };
                if newer {
                    entry.1 = version;
                }
            }
        }
        let floor = recording_floor(on_demand_versions.iter().map(String::as_str));
        let observed: BTreeMap<&str, &str> = session_versions
            .iter()
            .filter(|(_, (_, version))| is_observed(version.as_deref(), floor.as_deref()))
            .map(|(session, (project, _))| (session.as_str(), project.as_str()))
            .collect();

        struct Row {
            stat: RuleLoadStat,
            resolved_path: Option<String>,
        }
        let mut catalog: Vec<Row> = {
            let mut stmt = self.conn.prepare(
                "SELECT kind, id, scope, project, load_mode, static_tokens, resolved_path
                 FROM surfaces WHERE kind IN ('rule', 'claude_md')
                 ORDER BY kind DESC, scope, project, id",
            )?;
            stmt.query_map([], |row| {
                Ok(Row {
                    stat: RuleLoadStat {
                        kind: row.get(0)?,
                        id: row.get(1)?,
                        scope: row.get(2)?,
                        project: row.get(3)?,
                        load_mode: row.get(4)?,
                        static_tokens: row.get::<_, Option<i64>>(5)?.map(|n| n.max(0) as u64),
                        loads: 0,
                        sessions: 0,
                        last_loaded_epoch: None,
                        observed_sessions: 0,
                    },
                    resolved_path: row.get(6)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
        };

        // Projects that own a row for a file: their sessions are that row's,
        // not the global row's.
        let mut owners: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for row in &catalog {
            if row.stat.scope == "project"
                && let Some(path) = &row.resolved_path
            {
                owners
                    .entry(path.clone())
                    .or_default()
                    .push(row.stat.project.clone());
            }
        }
        let in_reach = |scope: &str, own: &str, path: Option<&String>, session_project: &str| {
            if scope == "project" {
                return own == session_project;
            }
            path.and_then(|path| owners.get(path))
                .is_none_or(|owned| !owned.iter().any(|p| p == session_project))
        };

        // Two rows of one scope naming one file (a symlinked alias) take loads
        // on the first only, as the view orders them; the others could never
        // load, so they are left unmeasured rather than flagged as never firing.
        let mut attribution_target: BTreeMap<(&str, &str, &str), (&str, &str)> = BTreeMap::new();
        for row in &catalog {
            if let Some(path) = &row.resolved_path {
                let candidate = (row.stat.kind.as_str(), row.stat.id.as_str());
                attribution_target
                    .entry((&row.stat.scope, &row.stat.project, path))
                    .and_modify(|target| *target = (*target).min(candidate))
                    .or_insert(candidate);
            }
        }
        let aliases: std::collections::BTreeSet<(String, String, String, String)> = catalog
            .iter()
            .filter(|row| {
                row.resolved_path.as_deref().is_some_and(|path| {
                    attribution_target.get(&(
                        row.stat.scope.as_str(),
                        row.stat.project.as_str(),
                        path,
                    )) != Some(&(row.stat.kind.as_str(), row.stat.id.as_str()))
                })
            })
            .map(|row| {
                (
                    row.stat.kind.clone(),
                    row.stat.id.clone(),
                    row.stat.scope.clone(),
                    row.stat.project.clone(),
                )
            })
            .collect();
        drop(attribution_target);

        // (kind, id, scope, project) → that row's (session, loads, latest load).
        type RowKey = (String, String, String, String);
        let mut attributed: BTreeMap<RowKey, Vec<(String, u64, i64)>> = BTreeMap::new();
        {
            let mut stmt = self.conn.prepare(
                "SELECT surface_kind, surface_id, surface_scope, surface_project, session_id,
                        COUNT(*), MAX(started_epoch)
                 FROM context_loads WHERE surface_kind IS NOT NULL
                 GROUP BY surface_kind, surface_id, surface_scope, surface_project, session_id",
            )?;
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                attributed
                    .entry((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
                    .or_default()
                    .push((
                        row.get(4)?,
                        row.get::<_, i64>(5)?.max(0) as u64,
                        row.get(6)?,
                    ));
            }
        }

        for row in &mut catalog {
            let key = (
                row.stat.kind.clone(),
                row.stat.id.clone(),
                row.stat.scope.clone(),
                row.stat.project.clone(),
            );
            let reaches = |session_project: &str| {
                in_reach(
                    &row.stat.scope,
                    &row.stat.project,
                    row.resolved_path.as_ref(),
                    session_project,
                )
            };
            let (mut loads, mut sessions, mut last_loaded) = (0, 0, None);
            for (session, count, last) in attributed.get(&key).into_iter().flatten() {
                loads += count;
                last_loaded = last_loaded.max(Some(*last));
                if observed
                    .get(session.as_str())
                    .is_some_and(|project| reaches(project))
                {
                    sessions += 1;
                }
            }
            let observed_sessions = if aliases.contains(&key) {
                0
            } else {
                observed.values().filter(|p| reaches(p)).count() as u64
            };
            row.stat.loads = loads;
            row.stat.sessions = sessions;
            row.stat.last_loaded_epoch = last_loaded;
            row.stat.observed_sessions = observed_sessions;
        }
        Ok(catalog.into_iter().map(|row| row.stat).collect())
    }

    /// Every file edit and check run as a `WorkStep`, per session in transcript
    /// order — the input to check-loop detection (`core::thrash`). A Bash run
    /// that is not a check, or whose outcome was never seen, is no step.
    pub fn work_steps(&self) -> Result<Vec<WorkStep>> {
        let mut stmt = self.conn.prepare(
            "SELECT s.project, e.session_id, e.started_epoch, e.kind, e.target, e.check_outcome
             FROM events e JOIN sessions s ON e.session_id = s.id
             WHERE (e.kind = 'file_edit')
                OR (e.kind = 'bash_cmd' AND e.target IS NOT NULL AND e.check_outcome IS NOT NULL)
             ORDER BY e.session_id, e.id",
        )?;
        let rows = stmt
            .query_map([], |row| {
                let kind: String = row.get(3)?;
                let kind = if kind == "file_edit" {
                    StepKind::Edit
                } else {
                    let outcome: String = row.get(5)?;
                    StepKind::Check {
                        key: row.get(4)?,
                        failed: BashOutcome::from_label(&outcome)
                            .is_some_and(BashOutcome::is_failure),
                    }
                };
                Ok(WorkStep {
                    project: row.get(0)?,
                    session_id: row.get(1)?,
                    epoch: row.get(2)?,
                    kind,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Every Bash run with its outcome and sandbox flag, per session in
    /// transcript order — the input to the unsandboxed-retry count.
    pub fn sandboxed_runs(&self) -> Result<Vec<SandboxedRun>> {
        let mut stmt = self.conn.prepare(
            "SELECT s.project, e.session_id, e.source, e.model IS 'unsandboxed'
             FROM events e JOIN sessions s ON e.session_id = s.id
             WHERE e.kind = 'bash_cmd'
             ORDER BY e.session_id, e.id",
        )?;
        let rows = stmt
            .query_map([], |row| {
                let outcome: Option<String> = row.get(2)?;
                Ok(SandboxedRun {
                    project: row.get(0)?,
                    session_id: row.get(1)?,
                    outcome: outcome.as_deref().and_then(BashOutcome::from_label),
                    unsandboxed: row.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
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

    /// Total startup tokens of **global** config — what every session pays
    /// unconditionally from `~/.claude` regardless of project: always-on files
    /// in full, plus the descriptions skills and agents contribute to the
    /// startup listing. Bodies that wait to be invoked weigh nothing here, and a
    /// surface whose startup weight is unknown (an MCP schema) is left out of
    /// the sum rather than counted as zero. Project config is always-on only for
    /// its own sessions (`always_on_config_tokens_for`).
    pub fn always_on_config_tokens(&self) -> Result<i64> {
        let total = self.conn.query_row(
            "SELECT COALESCE(SUM(startup_tokens), 0) FROM surfaces
             WHERE scope = 'global'",
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

    /// Subagent output tokens and run count per agent type, costliest first —
    /// the breakdown behind the session-level subagent total.
    ///
    /// Every run counts once, at its own cost, whatever its depth: a nested run
    /// is the agent that produced those tokens, and rolling it into its parent's
    /// type would credit the cost to an agent that only delegated. Runs whose
    /// type is unknown (no sidecar beside the transcript) group under `None`
    /// rather than being dropped, so the rows sum to the total of every run
    /// that was extracted. Sessions whose transcripts were pruned before their
    /// runs could be extracted keep their totals but have no rows here; a
    /// report reconciles the two with `core::subagent::SplitCoverage` rather
    /// than presenting these rows as the whole breakdown.
    pub fn subagent_by_agent(&self) -> Result<Vec<AgentCost>> {
        let mut stmt = self.conn.prepare(
            "SELECT agent, COUNT(*), COALESCE(SUM(out_tokens), 0)
             FROM subagent_runs
             GROUP BY agent
             ORDER BY COALESCE(SUM(out_tokens), 0) DESC, COUNT(*) DESC",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok(AgentCost {
                    agent: row.get(0)?,
                    runs: row.get(1)?,
                    out_tokens: row.get(2)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Subagent runs per agent type × the model they ran on, costliest first.
    /// Built from runs like `subagent_by_agent`, nested runs included, so the
    /// rows sum the same way.
    pub fn subagent_by_agent_model(&self) -> Result<Vec<AgentModelCost>> {
        let mut stmt = self.conn.prepare(
            "SELECT agent, model, COUNT(*), COALESCE(SUM(out_tokens), 0),
                    CASE WHEN COUNT(prompt_tokens) = COUNT(*) THEN SUM(prompt_tokens) END
             FROM subagent_runs
             GROUP BY agent, model
             ORDER BY COALESCE(SUM(out_tokens), 0) DESC, COUNT(*) DESC, agent, model",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok(AgentModelCost {
                    agent: row.get(0)?,
                    model: row.get(1)?,
                    runs: row.get(2)?,
                    out_tokens: row.get(3)?,
                    prompt_tokens: row.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Every main-thread spawn with the models around it — what its call
    /// requested, what its agent's definition pins, what the calling turn ran
    /// on, and what its run ran on — in spawn order, for
    /// `core::subagent::model_choice_by_agent`.
    ///
    /// The pin is read from the **effective** definition for the spawn's
    /// project, as the catalog join resolves it: a project's own copy shadows
    /// the global one even when it pins nothing. It is today's definition, not
    /// the one in force at spawn time — the catalog keeps no history.
    pub fn spawn_models(&self) -> Result<Vec<SpawnModels>> {
        let mut stmt = self.conn.prepare(
            "SELECT a.agent, a.requested_model,
                    CASE WHEN EXISTS (SELECT 1 FROM surfaces f
                                       WHERE f.kind = 'agent' AND f.id = a.agent
                                         AND f.scope = 'project' AND f.project = a.project)
                         THEN (SELECT json_extract(f.attrs_json, '$.model') FROM surfaces f
                                WHERE f.kind = 'agent' AND f.id = a.agent
                                  AND f.scope = 'project' AND f.project = a.project)
                         ELSE (SELECT json_extract(f.attrs_json, '$.model') FROM surfaces f
                                WHERE f.kind = 'agent' AND f.id = a.agent
                                  AND f.scope = 'global')
                    END,
                    a.caller_model, a.run_model
             FROM agent_spawns a
             ORDER BY a.started_epoch, a.tool_use_id",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok(SpawnModels {
                    agent: row.get(0)?,
                    requested: row.get(1)?,
                    pinned: row.get(2)?,
                    caller: row.get(3)?,
                    ran: row.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
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
                   (kind, id, scope, project, config_path, resolved_path,
                    static_tokens, startup_tokens, load_mode, attrs_json)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                (
                    &surface.kind,
                    &surface.id,
                    surface.scope.label(),
                    surface.scope.project(),
                    &surface.config_path,
                    canonical_path(&surface.config_path),
                    surface.static_tokens,
                    surface.startup_tokens,
                    surface.load_mode.label(),
                    attrs_json(&surface.attrs),
                ),
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Replace the change dates wholesale, like the catalog they describe —
    /// both are read from live config on every analyze.
    pub fn replace_surface_changes(&mut self, changes: &[ConfigChange]) -> Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM surface_changes", ())?;
        for change in changes {
            tx.execute(
                "INSERT OR REPLACE INTO surface_changes
                   (kind, id, scope, project, changed_epoch, introduced_epoch,
                    date_source, targets, always_on)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                (
                    &change.kind,
                    &change.id,
                    change.scope.label(),
                    change.scope.project(),
                    change.changed_epoch,
                    change.introduced_epoch,
                    change.date_source.label(),
                    change.targets.join(","),
                    change.always_on,
                ),
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Every recorded surface change, in catalog order.
    pub fn surface_changes(&self) -> Result<Vec<ConfigChange>> {
        let mut stmt = self.conn.prepare(
            "SELECT kind, id, scope, project, changed_epoch, introduced_epoch,
                    date_source, targets, always_on
             FROM surface_changes ORDER BY kind, id, scope, project",
        )?;
        let rows = stmt
            .query_map([], |row| {
                let scope: String = row.get(2)?;
                let project: String = row.get(3)?;
                let source: String = row.get(6)?;
                let targets: String = row.get(7)?;
                Ok(ConfigChange {
                    kind: row.get(0)?,
                    id: row.get(1)?,
                    scope: if scope == "project" {
                        Scope::Project(project)
                    } else {
                        Scope::Global
                    },
                    changed_epoch: row.get(4)?,
                    introduced_epoch: row.get(5)?,
                    date_source: DateSource::from_label(&source),
                    targets: targets
                        .split(',')
                        .filter(|t| !t.is_empty())
                        .map(str::to_string)
                        .collect(),
                    always_on: row.get(8)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Record how many main-thread tool calls a session made. Call after
    /// `ingest_session`, whose row replacement resets it to unknown.
    pub fn set_session_tool_calls(&mut self, session_id: &str, tool_calls: u64) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET tool_calls = ?2 WHERE id = ?1",
            (session_id, tool_calls as i64),
        )?;
        Ok(())
    }

    /// Every session with a known start, as the before/after comparison sees
    /// it: project, start, tool calls, and failures per category.
    pub fn session_samples(&self) -> Result<Vec<SessionSample>> {
        let mut samples: BTreeMap<String, SessionSample> = BTreeMap::new();
        let mut stmt = self.conn.prepare(
            "SELECT id, project, CAST(strftime('%s', started_at) AS INTEGER), tool_calls
             FROM sessions WHERE strftime('%s', started_at) IS NOT NULL",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, Option<i64>>(3)?,
            ))
        })?;
        for row in rows {
            let (id, project, start_epoch, tool_calls) = row?;
            samples.insert(
                id,
                SessionSample {
                    project,
                    start_epoch,
                    tool_calls: tool_calls.map(|n| n as u64),
                    errors: BTreeMap::new(),
                },
            );
        }
        let mut stmt = self.conn.prepare(
            "SELECT session_id, surface_id, COUNT(*) FROM events
             WHERE kind = 'tool_error' GROUP BY session_id, surface_id",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?;
        for row in rows {
            let (session_id, category, count) = row?;
            if let Some(sample) = samples.get_mut(&session_id) {
                sample.errors.insert(category, count as u64);
            }
        }
        Ok(samples.into_values().collect())
    }

    /// The whole catalog — every `(kind, id, scope, project)` row — with each
    /// row's **effective** usage. Shadowing is per project: an event from
    /// project P joins P's project row when one exists for that `(kind, id)`,
    /// else the global row. One event therefore counts on exactly one row
    /// (`docs/specs/surfaces.md`).
    pub fn effective_catalog(&self) -> Result<Vec<CatalogEntry>> {
        // An MCP server's startup weight is unknown to config; a measurement from
        // the request shape stands in for it where one exists.
        let mut stmt = self.conn.prepare(&format!(
            "WITH {MEASURED_MCP_CTE}
             SELECT f.kind, f.id, f.scope, f.project, f.static_tokens,
                    COALESCE(f.startup_tokens,
                             CASE WHEN f.kind = 'mcp_server' AND f.scope = 'project' THEN
                               (SELECT m.startup_tokens FROM measured_mcp_project m
                                 WHERE m.server = f.id AND m.project = f.project)
                                  WHEN f.kind = 'mcp_server' THEN
                               (SELECT m.startup_tokens FROM measured_mcp m WHERE m.server = f.id)
                             END),
                    f.load_mode, COUNT(u.event_id)
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
             ORDER BY f.kind, f.id, f.scope, f.project"
        ))?;
        let rows = stmt
            .query_map([], |row| {
                Ok(CatalogEntry {
                    kind: row.get(0)?,
                    id: row.get(1)?,
                    scope: row.get(2)?,
                    project: row.get(3)?,
                    static_tokens: row.get(4)?,
                    startup_tokens: row.get(5)?,
                    load_mode: row.get(6)?,
                    uses: row.get(7)?,
                    measurable: is_usage_measurable(&row.get::<_, String>(0)?),
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

    /// Total startup config tokens for a session in `project`: the global
    /// figure plus that project's own startup cost.
    ///
    /// A surface installed at both scopes under the same `(kind, id)` is counted
    /// twice here. Resolving that would mean asserting which copy Claude Code
    /// actually loads, and the shadowing model this store implements is defined
    /// for the **usage join** (`effective_catalog`), not for load order — a
    /// project rule and its global namesake genuinely both reach the context, so
    /// excluding one would understate the figure instead.
    pub fn always_on_config_tokens_for(&self, project: &str) -> Result<i64> {
        let total = self.conn.query_row(
            "SELECT COALESCE(SUM(startup_tokens), 0) FROM surfaces
             WHERE scope = 'global' OR (scope = 'project' AND project = ?1)",
            [project],
            |row| row.get(0),
        )?;
        Ok(total)
    }

    /// Replace a session's tool context: which shapes it sent, which tools it
    /// had and how it used them, and its MCP instructions. Shapes themselves are
    /// content-addressed and only ever added.
    pub fn ingest_tool_context(
        &mut self,
        session_id: &str,
        source_path: &str,
        context: &ToolContext,
    ) -> Result<()> {
        let tx = self.conn.transaction()?;
        for table in [
            "session_shapes",
            "session_tools",
            "session_mcp_instructions",
        ] {
            tx.execute(
                &format!("DELETE FROM {table} WHERE source_path = ?1"),
                (source_path,),
            )?;
        }
        for shape in &context.shapes {
            tx.execute(
                "INSERT OR IGNORE INTO request_shapes (hash, system_tokens, system_blocks)
                 VALUES (?1, ?2, ?3)",
                (&shape.hash, shape.system_tokens, shape.system_blocks),
            )?;
            for tool in &shape.tools {
                tx.execute(
                    "INSERT OR IGNORE INTO request_shape_tools (hash, name, server, deferred, tokens)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    (&shape.hash, &tool.name, &tool.server, tool.deferred, tool.tokens),
                )?;
            }
        }
        for use_ in &context.uses {
            tx.execute(
                "INSERT INTO session_shapes (session_id, source_path, hash, first_epoch, requests)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                (
                    session_id,
                    source_path,
                    &use_.hash,
                    use_.first_epoch_ms / 1000,
                    use_.requests,
                ),
            )?;
        }
        for tool in &context.tools {
            tx.execute(
                "INSERT INTO session_tools
                   (session_id, source_path, name, server, eager_tokens, listing_tokens, loaded, calls)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                (
                    session_id,
                    source_path,
                    &tool.name,
                    &tool.server,
                    tool.eager_tokens,
                    tool.listing_tokens,
                    tool.loaded,
                    tool.calls,
                ),
            )?;
        }
        for (server, tokens) in &context.mcp_instructions {
            tx.execute(
                "INSERT INTO session_mcp_instructions (session_id, source_path, server, tokens)
                 VALUES (?1, ?2, ?3, ?4)",
                (session_id, source_path, server, tokens),
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// The always-on components of the **latest** session that recorded its
    /// request shape — the current setup, where the floor is a minimum over
    /// history (`docs/specs/surfaces.md` "Decomposing the residual"). `None`
    /// when no analyzed session recorded one.
    pub fn latest_decomposition(&self) -> Result<Option<Decomposition>> {
        let latest = self
            .conn
            .query_row(
                // A session whose startup shape was never recorded has nothing to
                // decompose; a later shape of it would be read as its start.
                "SELECT s.id, s.project, s.started_at, startup.hash
                 FROM sessions s
                 JOIN (SELECT h.session_id, h.hash,
                              ROW_NUMBER() OVER (PARTITION BY h.session_id
                                                 ORDER BY h.first_epoch) AS rank
                       FROM session_shapes h) startup
                   ON startup.session_id = s.id AND startup.rank = 1
                 WHERE EXISTS (SELECT 1 FROM request_shapes r WHERE r.hash = startup.hash)
                 ORDER BY s.started_at DESC, s.id DESC LIMIT 1",
                [],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                },
            )
            .optional()?;
        let Some((session_id, project, started_at, hash)) = latest else {
            return Ok(None);
        };
        let sum = |sql: &str, key: &str| -> Result<i64> {
            Ok(self.conn.query_row(sql, [key], |row| row.get(0))?)
        };
        Ok(Some(Decomposition {
            observed_ctx: self
                .conn
                .query_row(
                    "SELECT MIN(ctx_start) FROM events
                     WHERE kind = 'session_start' AND session_id = ?1",
                    [&session_id],
                    |row| row.get(0),
                )
                .optional()?
                .flatten(),
            system_tokens: sum(
                "SELECT COALESCE(SUM(system_tokens), 0) FROM request_shapes WHERE hash = ?1",
                &hash,
            )?,
            builtin_tool_tokens: sum(
                "SELECT COALESCE(SUM(tokens), 0) FROM request_shape_tools
                 WHERE hash = ?1 AND deferred = 0 AND server IS NULL",
                &hash,
            )?,
            mcp_tool_tokens: sum(
                "SELECT COALESCE(SUM(tokens), 0) FROM request_shape_tools
                 WHERE hash = ?1 AND deferred = 0 AND server IS NOT NULL",
                &hash,
            )?,
            deferred_listing_tokens: sum(
                "SELECT COALESCE(SUM(listing_tokens), 0) FROM session_tools WHERE session_id = ?1",
                &session_id,
            )?,
            mcp_instruction_tokens: sum(
                "SELECT COALESCE(SUM(tokens), 0) FROM session_mcp_instructions WHERE session_id = ?1",
                &session_id,
            )?,
            config_tokens: self.always_on_config_tokens_for(&project)?,
            session_id,
            project,
            started_at,
        }))
    }

    /// Every tool across the sessions that had it, for the eager↔deferred
    /// wedges (`core::tool_load`).
    pub fn tool_load_stats(&self) -> Result<Vec<ToolLoadStat>> {
        let mut stmt = self.conn.prepare(
            "SELECT t.name, MAX(t.server),
                    SUM(t.eager_tokens IS NOT NULL),
                    SUM(t.eager_tokens IS NOT NULL AND t.calls > 0),
                    SUM(t.listing_tokens IS NOT NULL),
                    SUM(t.listing_tokens IS NOT NULL AND t.loaded),
                    SUM(t.calls),
                    COALESCE((SELECT l.eager_tokens FROM session_tools l
                               JOIN sessions s ON s.id = l.session_id
                              WHERE l.name = t.name AND l.eager_tokens IS NOT NULL
                              ORDER BY s.started_at DESC, s.id DESC LIMIT 1), 0),
                    COALESCE((SELECT l.listing_tokens IS NOT NULL FROM session_tools l
                               JOIN sessions s ON s.id = l.session_id
                              WHERE l.name = t.name
                                AND (l.eager_tokens IS NOT NULL OR l.listing_tokens IS NOT NULL)
                              ORDER BY s.started_at DESC, s.id DESC LIMIT 1), 0)
             FROM session_tools t
             GROUP BY t.name
             ORDER BY t.name",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok(ToolLoadStat {
                    name: row.get(0)?,
                    server: row.get(1)?,
                    eager_sessions: row.get(2)?,
                    eager_sessions_called: row.get(3)?,
                    deferred_sessions: row.get(4)?,
                    deferred_sessions_loaded: row.get(5)?,
                    calls: row.get(6)?,
                    eager_tokens: row.get(7)?,
                    currently_deferred: row.get(8)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Each MCP server's startup cost measured from the latest session that saw
    /// it — eager definitions, listing lines, and instructions — with how many
    /// sessions saw it and how often its tools were called
    /// (`docs/specs/config-format.md` "The MCP tool-schema hard case").
    pub fn measured_mcp_startup(&self) -> Result<Vec<McpServerCost>> {
        let mut stmt = self.conn.prepare(&format!(
            "WITH {MEASURED_MCP_CTE}
             SELECT server, startup_tokens, sessions, calls FROM measured_mcp
             ORDER BY startup_tokens DESC, server"
        ))?;
        let rows = stmt
            .query_map([], |row| {
                Ok(McpServerCost {
                    server: row.get(0)?,
                    startup_tokens: row.get(1)?,
                    sessions: row.get(2)?,
                    calls: row.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
}

/// Defines `measured_mcp(server, startup_tokens, sessions, calls)`: each MCP
/// server's startup cost in the latest session that carried any of its tools
/// or its instructions — and `measured_mcp_project`, the same per project, which
/// a project-scoped catalog row reads so one project's server is never priced
/// by another's. Only sessions whose startup shape was recorded measure
/// anything: without it the eager schemas are unknown, not zero. Shared by the per-server report and the effective
/// catalog, so both read one measurement.
const MEASURED_MCP_CTE: &str = "
    shaped_sessions AS (
        SELECT h.session_id FROM session_shapes h
        WHERE h.first_epoch = (SELECT MIN(f.first_epoch) FROM session_shapes f
                                WHERE f.session_id = h.session_id)
          AND EXISTS (SELECT 1 FROM request_shapes r WHERE r.hash = h.hash)),
    mcp_tools AS (
        SELECT session_id, server,
               SUM(COALESCE(eager_tokens, 0) + COALESCE(listing_tokens, 0)) AS tokens
        FROM session_tools
        WHERE server IS NOT NULL AND (eager_tokens IS NOT NULL OR listing_tokens IS NOT NULL)
          AND session_id IN (SELECT session_id FROM shaped_sessions)
        GROUP BY session_id, server),
    mcp_instructions AS (
        SELECT session_id, server, SUM(tokens) AS tokens
        FROM session_mcp_instructions
        WHERE session_id IN (SELECT session_id FROM shaped_sessions)
        GROUP BY session_id, server),
    mcp_seen AS (
        SELECT session_id, server FROM mcp_tools
        UNION SELECT session_id, server FROM mcp_instructions),
    mcp_latest AS (
        SELECT m.server, m.session_id,
               ROW_NUMBER() OVER (PARTITION BY m.server
                                  ORDER BY s.started_at DESC, s.id DESC) AS rank
        FROM mcp_seen m JOIN sessions s ON s.id = m.session_id),
    measured_mcp AS (
        SELECT l.server,
               COALESCE((SELECT t.tokens FROM mcp_tools t
                          WHERE t.server = l.server AND t.session_id = l.session_id), 0)
             + COALESCE((SELECT i.tokens FROM mcp_instructions i
                          WHERE i.server = l.server AND i.session_id = l.session_id), 0)
               AS startup_tokens,
               (SELECT COUNT(*) FROM mcp_seen m WHERE m.server = l.server) AS sessions,
               COALESCE((SELECT SUM(c.calls) FROM session_tools c WHERE c.server = l.server), 0)
               AS calls
        FROM mcp_latest l WHERE l.rank = 1),
    mcp_latest_project AS (
        SELECT m.server, s.project, m.session_id,
               ROW_NUMBER() OVER (PARTITION BY m.server, s.project
                                  ORDER BY s.started_at DESC, s.id DESC) AS rank
        FROM mcp_seen m JOIN sessions s ON s.id = m.session_id),
    measured_mcp_project AS (
        SELECT l.server, l.project,
               COALESCE((SELECT t.tokens FROM mcp_tools t
                          WHERE t.server = l.server AND t.session_id = l.session_id), 0)
             + COALESCE((SELECT i.tokens FROM mcp_instructions i
                          WHERE i.server = l.server AND i.session_id = l.session_id), 0)
               AS startup_tokens
        FROM mcp_latest_project l WHERE l.rank = 1)
";

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
    // Before v2, `subagent_runs.model` held the sidecar's requested alias.
    let runs_predate_requested_model = version < 2 && table_exists(&tx, "subagent_runs")?;
    tx.execute_batch(SCHEMA)?;
    for table in MIGRATED_TABLES {
        reconcile_columns(&tx, &desired, table)?;
    }
    if runs_predate_requested_model {
        // A run whose transcript is gone is never re-extracted, so its alias
        // would otherwise be reported as the model it ran on — which that store
        // never recorded.
        tx.execute_batch("UPDATE subagent_runs SET requested_model = model, model = NULL")?;
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

/// A path with every symlink resolved, or `None` when it cannot be (the file
/// is gone). Both join sides go through this, so a config reached through a
/// link matches the real path a transcript records (`docs/specs/config-format.md`).
pub fn canonical_path(path: &str) -> Option<String> {
    std::fs::canonicalize(path)
        .ok()
        .map(|resolved| resolved.display().to_string())
}

fn epoch_ms_to_rfc3339(epoch_ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(epoch_ms)
        .map(|dt| dt.to_rfc3339())
        .unwrap_or_default()
}

/// A surface's attributes as one JSON object, or NULL when it has none.
fn attrs_json(attrs: &[(String, String)]) -> Option<String> {
    if attrs.is_empty() {
        return None;
    }
    let object: BTreeMap<&str, &str> = attrs
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect();
    serde_json::to_string(&object).ok()
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
    use crate::core::cache::{CacheBreak, CacheRequest, CacheTtl};
    use crate::core::context_load::{FileClass, LoadTrigger};
    use crate::core::surface::{LoadMode, Scope};
    use crate::core::tool_load::{SessionTool, ShapeTool, ShapeUse, WeighedShape};

    /// Permissions are the reason this test and the next one work on a real
    /// temp directory instead of `Store::in_memory`: the default store path
    /// lives under a user state directory that may not exist yet, and both it
    /// and the store must come out owner-only.
    #[test]
    fn open_creates_a_missing_directory_and_keeps_both_owner_only() {
        let dir = std::env::temp_dir().join(format!("cclens-open-test-{}", std::process::id()));
        let path = dir.join("cclens").join("cclens.db");
        let _ = std::fs::remove_dir_all(&dir);

        drop(Store::open(&path).unwrap());

        assert!(path.exists(), "the store was not created at {path:?}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode =
                |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(path.parent().unwrap()), 0o700);
            assert_eq!(mode(&path), 0o600);
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The store an older cclens left under the umask is the case that actually
    /// exists in the wild, so opening it has to narrow it — otherwise the
    /// protection never reaches anyone who already ran the tool.
    #[cfg(unix)]
    #[test]
    fn open_narrows_a_store_an_earlier_run_left_readable() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!("cclens-mode-test-{}", std::process::id()));
        let path = dir.join("cclens.db");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        drop(Store::open(&path).unwrap());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        drop(Store::open(&path).unwrap());

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        std::fs::remove_dir_all(&dir).unwrap();
    }

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
            spawns: Vec::new(),
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

    fn cache_observation(
        at_ms: i64,
        read: u64,
        cache_break: Option<CacheBreak>,
    ) -> CacheObservation {
        CacheObservation {
            request: CacheRequest {
                sent_epoch_ms: at_ms,
                model: "model-a".into(),
                uncached: 2,
                cache_read: read,
                cache_write_5m: 1,
                cache_write_1h: 7,
                requested_ttl: None,
                shape: None,
            },
            gap_ms: None,
            ttl: CacheTtl::OneHour,
            cache_break,
        }
    }

    #[test]
    fn cache_requests_round_trip_with_their_break_and_session_project() {
        let mut store = Store::in_memory().unwrap();
        let meta = session("s1");
        store.ingest_session(&meta, &[], &[]).unwrap();
        let tools_break = CacheBreak {
            cause: BreakCause::ToolsChanged,
            rebuilt_tokens: 900,
            surfaces: vec!["mcp_server:late".into(), "tool:Monitor".into()],
        };
        store
            .ingest_prompt_cache(
                &meta.id,
                &meta.source_path,
                &[
                    cache_observation(1_700_000_000_000, 0, None),
                    cache_observation(1_700_000_060_000, 5, Some(tools_break)),
                ],
                None,
            )
            .unwrap();

        let rows = store.cache_rows().unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].break_cause, None);
        assert_eq!(
            rows[1],
            CacheRow {
                session_id: "s1".into(),
                project: "demo".into(),
                started_epoch: 1_700_000_060,
                uncached: 2,
                cache_read: 5,
                cache_write_5m: 1,
                cache_write_1h: 7,
                break_cause: Some(BreakCause::ToolsChanged),
                rebuilt_tokens: 900,
                surfaces: vec!["mcp_server:late".into(), "tool:Monitor".into()],
            }
        );
    }

    #[test]
    fn re_ingesting_prompt_cache_replaces_the_sessions_rows_and_cost() {
        let mut store = Store::in_memory().unwrap();
        let meta = session("s1");
        store.ingest_session(&meta, &[], &[]).unwrap();
        let first = [cache_observation(1_700_000_000_000, 0, None)];
        store
            .ingest_prompt_cache(&meta.id, &meta.source_path, &first, Some(1.0))
            .unwrap();
        let grown = [
            cache_observation(1_700_000_000_000, 0, None),
            cache_observation(1_700_000_001_000, 8, None),
        ];
        store
            .ingest_prompt_cache(&meta.id, &meta.source_path, &grown, Some(2.5))
            .unwrap();

        assert_eq!(store.cache_rows().unwrap().len(), 2);
        assert_eq!(store.reported_cost().unwrap(), (2.5, 1));
    }

    #[test]
    fn a_session_without_a_reported_cost_is_not_counted_as_free() {
        let mut store = Store::in_memory().unwrap();
        for (id, cost) in [("s1", Some(3.0)), ("s2", None)] {
            let meta = session(id);
            store.ingest_session(&meta, &[], &[]).unwrap();
            store
                .ingest_prompt_cache(&meta.id, &meta.source_path, &[], cost)
                .unwrap();
        }
        assert_eq!(store.reported_cost().unwrap(), (3.0, 1));
    }

    fn agent_spawn(surface_id: &str, tool_use_id: &str) -> UsageEvent {
        UsageEvent {
            surface_kind: "agent".to_string(),
            surface_id: surface_id.to_string(),
            started_epoch_ms: 1_700_000_000_000,
            tool_use_id: Some(tool_use_id.to_string()),
            requested_model: None,
            caller_model: None,
        }
    }

    fn subagent_run(agent: &str, agent_id: &str, root: &str, out_tokens: u64) -> SubagentRun {
        SubagentRun {
            agent: Some(agent.to_string()),
            agent_id: agent_id.to_string(),
            run_path: format!("/tmp/example/subagents/agent-{agent_id}.jsonl"),
            tool_use_id: Some(format!("toolu_{agent_id}")),
            parent_agent_id: None,
            spawn_depth: 1,
            model: Some("sonnet".to_string()),
            requested_model: None,
            prompt_id: Some("p1".to_string()),
            out_tokens,
            prompt_tokens: 0,
            started_epoch_ms: 1_700_000_000_000,
            root_tool_use_id: Some(root.to_string()),
        }
    }

    #[test]
    fn subagent_cost_rolls_up_per_agent_type() {
        let mut store = Store::in_memory().unwrap();
        let s1 = session("s1");
        store.ingest_session(&s1, &[], &[]).unwrap();
        store
            .ingest_subagent_runs(
                &s1.id,
                &s1.source_path,
                &[
                    subagent_run("Explore", "a1", "toolu_a1", 400),
                    subagent_run("Explore", "a2", "toolu_a2", 100),
                    subagent_run("code-writer", "a3", "toolu_a3", 900),
                ],
            )
            .unwrap();

        assert_eq!(
            store.subagent_by_agent().unwrap(),
            vec![
                AgentCost {
                    agent: Some("code-writer".to_string()),
                    runs: 1,
                    out_tokens: 900,
                },
                AgentCost {
                    agent: Some("Explore".to_string()),
                    runs: 2,
                    out_tokens: 500,
                },
            ]
        );
    }

    #[test]
    fn subagent_cost_splits_per_agent_type_and_the_model_it_ran_on() {
        let mut store = Store::in_memory().unwrap();
        let s1 = session("s1");
        store.ingest_session(&s1, &[], &[]).unwrap();
        let on = |model: &str, mut run: SubagentRun| {
            run.model = Some(model.to_string());
            run.prompt_tokens = run.out_tokens * 10;
            run
        };
        store
            .ingest_subagent_runs(
                &s1.id,
                &s1.source_path,
                &[
                    on("m-big", subagent_run("Explore", "a1", "toolu_a1", 400)),
                    on("m-small", subagent_run("Explore", "a2", "toolu_a2", 100)),
                    on("m-big", subagent_run("Explore", "a3", "toolu_a3", 200)),
                ],
            )
            .unwrap();

        assert_eq!(
            store.subagent_by_agent_model().unwrap(),
            vec![
                AgentModelCost {
                    agent: Some("Explore".to_string()),
                    model: Some("m-big".to_string()),
                    runs: 2,
                    out_tokens: 600,
                    prompt_tokens: Some(6000),
                },
                AgentModelCost {
                    agent: Some("Explore".to_string()),
                    model: Some("m-small".to_string()),
                    runs: 1,
                    out_tokens: 100,
                    prompt_tokens: Some(1000),
                },
            ]
        );
    }

    #[test]
    fn a_group_holding_an_unmeasured_run_has_no_prompt_total() {
        let mut store = Store::in_memory().unwrap();
        let s1 = session("s1");
        store.ingest_session(&s1, &[], &[]).unwrap();
        store
            .ingest_subagent_runs(
                &s1.id,
                &s1.source_path,
                &[
                    subagent_run("Explore", "a1", "toolu_a1", 400),
                    subagent_run("Explore", "a2", "toolu_a2", 100),
                ],
            )
            .unwrap();
        store
            .conn
            .execute_batch("UPDATE subagent_runs SET prompt_tokens = NULL WHERE agent_id = 'a2'")
            .unwrap();

        let rows = store.subagent_by_agent_model().unwrap();
        assert_eq!((rows[0].out_tokens, rows[0].prompt_tokens), (500, None));
    }

    #[test]
    fn a_spawn_keeps_the_model_its_call_requested_beside_its_callers_and_its_runs() {
        let mut store = Store::in_memory().unwrap();
        let s1 = session("s1");
        let mut pinned = agent_spawn("Explore", "toolu_a1");
        pinned.requested_model = Some("small".to_string());
        pinned.caller_model = Some("m-big".to_string());
        let mut unpinned = agent_spawn("Plan", "toolu_a2");
        unpinned.caller_model = Some("m-big".to_string());
        store.ingest_session(&s1, &[], &[pinned, unpinned]).unwrap();
        let mut run = subagent_run("Explore", "a1", "toolu_a1", 10);
        run.model = Some("m-small".to_string());
        store
            .ingest_subagent_runs(&s1.id, &s1.source_path, &[run])
            .unwrap();

        assert_eq!(
            store.spawn_models().unwrap(),
            vec![
                SpawnModels {
                    agent: "Explore".to_string(),
                    requested: Some("small".to_string()),
                    pinned: None,
                    caller: Some("m-big".to_string()),
                    ran: Some("m-small".to_string()),
                },
                SpawnModels {
                    agent: "Plan".to_string(),
                    requested: None,
                    pinned: None,
                    caller: Some("m-big".to_string()),
                    ran: None,
                },
            ]
        );
    }

    fn agent_pinned(id: &str, scope: Scope, model: Option<&str>) -> Surface {
        let mut agent = surface(id, scope, 1);
        agent.kind = "agent".to_string();
        agent.attrs = model
            .map(|model| vec![("model".to_string(), model.to_string())])
            .unwrap_or_default();
        agent
    }

    #[test]
    fn a_spawn_reads_its_agents_pin_from_the_effective_definition() {
        let mut store = Store::in_memory().unwrap();
        let s1 = session("s1");
        store
            .ingest_session(
                &s1,
                &[],
                &[
                    agent_spawn("writer", "toolu_a1"),
                    agent_spawn("reviewer", "toolu_a2"),
                ],
            )
            .unwrap();
        store
            .replace_surfaces(&[
                agent_pinned("writer", Scope::Global, Some("big")),
                // The project's own copy shadows the global pin with none.
                agent_pinned("reviewer", Scope::Global, Some("big")),
                agent_pinned("reviewer", Scope::Project("demo".to_string()), None),
            ])
            .unwrap();

        let pins: Vec<(String, Option<String>)> = store
            .spawn_models()
            .unwrap()
            .into_iter()
            .map(|spawn| (spawn.agent, spawn.pinned))
            .collect();
        assert_eq!(
            pins,
            vec![
                ("writer".to_string(), Some("big".to_string())),
                ("reviewer".to_string(), None),
            ]
        );
    }

    #[test]
    fn the_agent_spawns_view_names_the_overloaded_model_columns() {
        let mut store = Store::in_memory().unwrap();
        let s1 = session("s1");
        let mut spawn = agent_spawn("Explore", "toolu_a1");
        spawn.requested_model = Some("small".to_string());
        spawn.caller_model = Some("m-big".to_string());
        store.ingest_session(&s1, &[], &[spawn]).unwrap();

        let (_, rows) = store
            .query("SELECT agent, requested_model, caller_model, run_model FROM agent_spawns")
            .unwrap();
        assert_eq!(rows, vec![vec!["Explore", "small", "m-big", ""]]);
    }

    #[test]
    fn a_runs_table_predating_requested_model_moves_its_model_there() {
        // Before the split, `model` held the sidecar's *requested* alias. A run
        // whose transcript is gone is never re-extracted, so left in place that
        // alias would be reported as the model the run ran on.
        let mut store = Store::in_memory().unwrap();
        let s1 = session("s1");
        store.ingest_session(&s1, &[], &[]).unwrap();
        store
            .ingest_subagent_runs(
                &s1.id,
                &s1.source_path,
                &[subagent_run("Explore", "a1", "toolu_a1", 400)],
            )
            .unwrap();
        store
            .conn
            .execute_batch(
                "ALTER TABLE subagent_runs DROP COLUMN requested_model;
                 PRAGMA user_version = 1;",
            )
            .unwrap();

        let store = Store::from_connection(store.conn).unwrap();

        let (_, rows) = store
            .query("SELECT model, requested_model FROM subagent_runs")
            .unwrap();
        assert_eq!(rows, vec![vec!["", "sonnet"]]);
    }

    #[test]
    fn a_runs_table_already_holding_requested_model_keeps_both_on_reopen() {
        let mut store = Store::in_memory().unwrap();
        let s1 = session("s1");
        store.ingest_session(&s1, &[], &[]).unwrap();
        let mut run = subagent_run("Explore", "a1", "toolu_a1", 400);
        run.requested_model = Some("small".to_string());
        store
            .ingest_subagent_runs(&s1.id, &s1.source_path, &[run])
            .unwrap();

        let store = Store::from_connection(store.conn).unwrap();

        let (_, rows) = store
            .query("SELECT model, requested_model FROM subagent_runs")
            .unwrap();
        assert_eq!(rows, vec![vec!["sonnet", "small"]]);
    }

    #[test]
    fn a_runs_table_predating_the_model_columns_gains_them_without_losing_rows() {
        let mut store = Store::in_memory().unwrap();
        let s1 = session("s1");
        store.ingest_session(&s1, &[], &[]).unwrap();
        store
            .ingest_subagent_runs(
                &s1.id,
                &s1.source_path,
                &[subagent_run("Explore", "a1", "toolu_a1", 400)],
            )
            .unwrap();
        store
            .conn
            .execute_batch(
                "ALTER TABLE subagent_runs DROP COLUMN requested_model;
                 ALTER TABLE subagent_runs DROP COLUMN prompt_tokens;",
            )
            .unwrap();

        let store = Store::from_connection(store.conn).expect("must migrate, not refuse");

        let (_, rows) = store
            .query("SELECT agent, out_tokens, prompt_tokens FROM subagent_runs")
            .unwrap();
        // Never measured, so unknown — not a measured zero.
        assert_eq!(rows, vec![vec!["Explore", "400", ""]]);
    }

    #[test]
    fn a_spawn_event_takes_the_cost_of_the_run_it_started() {
        let mut store = Store::in_memory().unwrap();
        let s1 = session("s1");
        store
            .ingest_session(&s1, &[], &[agent_spawn("Explore", "toolu_a1")])
            .unwrap();
        let mut nested = subagent_run("code-writer", "a2", "toolu_a1", 700);
        nested.parent_agent_id = Some("a1".to_string());
        nested.spawn_depth = 2;
        store
            .ingest_subagent_runs(
                &s1.id,
                &s1.source_path,
                &[subagent_run("Explore", "a1", "toolu_a1", 400), nested],
            )
            .unwrap();

        let (out_tokens, sub_tokens, sub_agents) = store
            .query(
                "SELECT out_tokens, sub_tokens, sub_agent_count FROM events \
                 WHERE kind = 'agent_spawn'",
            )
            .map(|(_, rows)| (rows[0][0].clone(), rows[0][1].clone(), rows[0][2].clone()))
            .unwrap();

        // The spawn's own run wrote 400; the run it spawned in turn wrote 700.
        assert_eq!((out_tokens.as_str(), sub_tokens.as_str()), ("400", "700"));
        assert_eq!(sub_agents, "1");
    }

    #[test]
    fn a_spawn_whose_run_is_missing_keeps_its_zeros() {
        let mut store = Store::in_memory().unwrap();
        let s1 = session("s1");
        store
            .ingest_session(&s1, &[], &[agent_spawn("Explore", "toolu_gone")])
            .unwrap();
        store
            .ingest_subagent_runs(
                &s1.id,
                &s1.source_path,
                &[subagent_run("Explore", "a1", "toolu_a1", 400)],
            )
            .unwrap();

        let (_, rows) = store
            .query("SELECT out_tokens, sub_tokens FROM events WHERE target = 'toolu_gone'")
            .unwrap();
        assert_eq!(rows, vec![vec!["0".to_string(), "0".to_string()]]);
    }

    #[test]
    fn a_spawn_takes_only_its_own_sessions_run() {
        // Call ids are unique within a session, not across the store — a resumed
        // or copied transcript can carry the same id into a second session.
        let mut store = Store::in_memory().unwrap();
        let (s1, s2) = (session("s1"), session("s2"));
        store
            .ingest_session(&s1, &[], &[agent_spawn("Explore", "toolu_a1")])
            .unwrap();
        store
            .ingest_session(&s2, &[], &[agent_spawn("Explore", "toolu_a1")])
            .unwrap();
        store
            .ingest_subagent_runs(
                &s1.id,
                &s1.source_path,
                &[subagent_run("Explore", "a1", "toolu_a1", 400)],
            )
            .unwrap();
        store
            .ingest_subagent_runs(
                &s2.id,
                &s2.source_path,
                &[subagent_run("Explore", "a1", "toolu_a1", 900)],
            )
            .unwrap();

        let (_, rows) = store
            .query(
                "SELECT session_id, out_tokens FROM events \
                 WHERE kind = 'agent_spawn' ORDER BY session_id",
            )
            .unwrap();
        assert_eq!(
            rows,
            vec![
                vec!["s1".to_string(), "400".to_string()],
                vec!["s2".to_string(), "900".to_string()],
            ]
        );
    }

    #[test]
    fn re_ingesting_a_session_replaces_its_subagent_runs() {
        let mut store = Store::in_memory().unwrap();
        let s1 = session("s1");
        store.ingest_session(&s1, &[], &[]).unwrap();
        let runs = [subagent_run("Explore", "a1", "toolu_a1", 400)];
        store
            .ingest_subagent_runs(&s1.id, &s1.source_path, &runs)
            .unwrap();
        store
            .ingest_subagent_runs(&s1.id, &s1.source_path, &runs)
            .unwrap();

        assert_eq!(
            store.subagent_by_agent().unwrap(),
            vec![AgentCost {
                agent: Some("Explore".to_string()),
                runs: 1,
                out_tokens: 400,
            }]
        );
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

    /// A skill surface whose body weighs `static_tokens` and whose description
    /// (the only part loaded at startup) weighs `startup_tokens`.
    fn skill_surface(id: &str, scope: Scope, static_tokens: u64, startup_tokens: u64) -> Surface {
        Surface {
            kind: "skill".to_string(),
            id: id.to_string(),
            scope,
            config_path: format!("/cfg/{id}"),
            static_tokens: Some(static_tokens),
            startup_tokens: Some(startup_tokens),
            load_mode: LoadMode::StartupDescription,
            attrs: Vec::new(),
        }
    }

    fn surface(id: &str, scope: Scope, static_tokens: u64) -> Surface {
        skill_surface(id, scope, static_tokens, 0)
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
        assert_eq!(
            views,
            [
                "agent_spawns",
                "bash_commands",
                "context_loads",
                "stray",
                "tool_errors"
            ]
        );
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

    #[test]
    fn a_catalog_without_the_startup_cost_column_is_rebuilt() {
        // A catalog weighed before startup cost was split out would silently
        // answer every "what does removing this save" question with nothing, so
        // it must not survive the upgrade as it is. `surfaces` is regenerated
        // from live config on every analyze, so reconciling it costs nothing to
        // drop — and clearing `analyzed_at` is what routes a `--frozen` read to
        // the freshness warning instead of reporting the empty catalog as a
        // finding.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE surfaces (kind TEXT NOT NULL, id TEXT NOT NULL,
                                    scope TEXT NOT NULL, project TEXT NOT NULL,
                                    static_tokens INTEGER, load_mode TEXT NOT NULL);
             CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             INSERT INTO meta (key, value) VALUES ('analyzed_at', '2026-01-01T00:00:00Z');",
        )
        .unwrap();

        let store = Store::from_connection(conn).expect("must migrate, not refuse");

        let names: Vec<String> = table_shape(&store.conn, "surfaces")
            .unwrap()
            .into_iter()
            .map(|column| column.name)
            .collect();
        assert!(names.contains(&"startup_tokens".to_string()), "{names:?}");
        assert_eq!(store.meta("analyzed_at").unwrap(), None);
    }

    #[test]
    fn a_store_without_subagent_runs_gains_the_table_and_is_re_extracted() {
        // An existing store holds the events but not the runs, and its
        // transcripts have stopped changing — so nothing would ever populate the
        // new table, silently, since every query still succeeds and returns
        // nothing. Two mechanisms cover it, and neither alone would: `migrate`
        // supplies the table, and the analyzer version is what gets the rows
        // extracted into it.
        let mut store = Store::in_memory().unwrap();
        store
            .record_ingested_file("/tmp/example/s1.jsonl", 1, 2)
            .unwrap();
        store
            .conn
            .execute_batch("DROP TABLE subagent_runs")
            .unwrap();
        // Stamped by a build that predates subagent extraction.
        store.set_meta(ANALYZER_META_KEY, "1").unwrap();

        let store = Store::from_connection(store.conn).unwrap();

        assert!(table_exists(&store.conn, "subagent_runs").unwrap());
        assert!(!store.is_analyzer_current().unwrap());
    }

    #[test]
    fn reopening_a_current_store_keeps_its_fingerprints() {
        // Migration reconciles shape only. Re-running analyze against a store
        // the current analyzer already wrote must stay cheap.
        let mut store = Store::in_memory().unwrap();
        store
            .record_ingested_file("/tmp/example/s1.jsonl", 1, 2)
            .unwrap();
        store.record_analyzer_version().unwrap();
        let store = Store::from_connection(store.conn).unwrap();

        assert!(store.is_analyzer_current().unwrap());
        assert!(store.is_ingested("/tmp/example/s1.jsonl", 1, 2).unwrap());
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
            spawns: Vec::new(),
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
    fn always_on_config_sums_every_surface_startup_cost() {
        let mut store = Store::in_memory().unwrap();
        store
            .replace_surfaces(&[
                Surface {
                    kind: "claude_md".to_string(),
                    id: "global".to_string(),
                    scope: Scope::Global,
                    config_path: "/c/CLAUDE.md".to_string(),
                    static_tokens: Some(600),
                    startup_tokens: Some(600),
                    load_mode: LoadMode::StartupFull,
                    attrs: Vec::new(),
                },
                Surface {
                    kind: "rule".to_string(),
                    id: "git/safety".to_string(),
                    scope: Scope::Global,
                    config_path: "/c/safety.md".to_string(),
                    static_tokens: Some(900),
                    startup_tokens: Some(900),
                    load_mode: LoadMode::StartupFull,
                    attrs: Vec::new(),
                },
                // A skill contributes its description, never its 1000-token body.
                skill_surface("git-commit", Scope::Global, 1000, 8),
                // An MCP server's schema is real but unweighable — it must not
                // land in the sum as a false zero-cost row either way.
                Surface {
                    kind: "mcp_server".to_string(),
                    id: "playwright".to_string(),
                    scope: Scope::Global,
                    config_path: "/c/.mcp.json".to_string(),
                    static_tokens: None,
                    startup_tokens: None,
                    load_mode: LoadMode::ToolSchema,
                    attrs: Vec::new(),
                },
                // Another project's CLAUDE.md is always-on *there*, not globally.
                Surface {
                    kind: "claude_md".to_string(),
                    id: "project".to_string(),
                    scope: Scope::Project("alpha".to_string()),
                    config_path: "/tmp/example/CLAUDE.md".to_string(),
                    static_tokens: Some(400),
                    startup_tokens: Some(400),
                    load_mode: LoadMode::StartupFull,
                    attrs: Vec::new(),
                },
            ])
            .unwrap();

        // The global figure excludes project config; a session in alpha pays
        // the global floor plus alpha's own always-on config.
        assert_eq!(store.always_on_config_tokens().unwrap(), 1508);
        assert_eq!(store.always_on_config_tokens_for("alpha").unwrap(), 1908);
    }

    #[test]
    fn the_catalog_carries_startup_and_body_cost_apart() {
        let mut store = Store::in_memory().unwrap();
        store
            .replace_surfaces(&[skill_surface("repro", Scope::Global, 1015, 7)])
            .unwrap();

        let catalog = store.effective_catalog().unwrap();
        assert_eq!(catalog[0].static_tokens, Some(1015));
        assert_eq!(catalog[0].startup_tokens, Some(7));
    }

    #[test]
    fn a_surfaces_attributes_are_stored_as_a_json_object() {
        let mut store = Store::in_memory().unwrap();
        let mut agent = surface("explorer", Scope::Global, 1);
        agent.kind = "agent".to_string();
        agent.attrs = vec![
            ("model".to_string(), "sonnet".to_string()),
            ("omitClaudeMd".to_string(), "true".to_string()),
        ];
        store
            .replace_surfaces(&[agent, surface("plain", Scope::Global, 1)])
            .unwrap();

        let (_, rows) = store
            .query("SELECT id, attrs_json FROM surfaces ORDER BY id")
            .unwrap();
        assert_eq!(
            rows,
            vec![
                vec!["explorer", r#"{"model":"sonnet","omitClaudeMd":"true"}"#],
                vec!["plain", ""],
            ]
        );
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
                &[WorkRow::file_edit(1_000, "x.rs", Some("/repo/src/x.rs"))],
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
                &[WorkRow::file_edit(1_000, "x.rs", None)],
            )
            .unwrap();

        assert_eq!(store.file_edits().unwrap(), vec![]);
    }

    #[test]
    fn bash_runs_and_edits_read_back_as_work_steps_in_transcript_order() {
        let mut store = Store::in_memory().unwrap();
        let mut alpha = session("a1");
        alpha.project = "alpha".to_string();
        store.ingest_session(&alpha, &[], &[]).unwrap();
        // All in one second: only insertion order says the edit came between.
        store
            .ingest_work_events(
                "a1",
                &alpha.source_path,
                &[
                    // Refused by the sandbox: the run failed, the check never said.
                    WorkRow::bash(1_000, "cargo", Some("sandbox-write"), false)
                        .checked("cargo test", None),
                    // Exited clean, but the runner printed a failure.
                    WorkRow::bash(1_050, "cargo", Some("ok"), true)
                        .checked("cargo test", Some("test-failure")),
                    WorkRow::file_edit(1_100, "x.rs", Some("/repo/src/x.rs")),
                    WorkRow::bash(1_200, "cargo", Some("ok"), false)
                        .checked("cargo test", Some("ok")),
                    // Not a check: no step, but still a Bash run.
                    WorkRow::bash(1_300, "rg", Some("ok"), true),
                ],
            )
            .unwrap();

        let steps: Vec<StepKind> = store
            .work_steps()
            .unwrap()
            .into_iter()
            .map(|s| s.kind)
            .collect();
        assert_eq!(
            steps,
            vec![
                StepKind::Check {
                    key: "cargo test".to_string(),
                    failed: true
                },
                StepKind::Edit,
                StepKind::Check {
                    key: "cargo test".to_string(),
                    failed: false
                },
            ]
        );
        // The retry count reads what the run did, not what the check said.
        let runs = store.sandboxed_runs().unwrap();
        assert_eq!(runs.len(), 4);
        assert_eq!(runs[0].project, "alpha");
        assert_eq!(
            runs[0].outcome,
            Some(BashOutcome::Failed(
                crate::core::friction::ErrorCategory::SandboxWrite
            ))
        );
        assert_eq!(runs[1].outcome, Some(BashOutcome::Ok));
        assert!(runs[1].unsandboxed);

        let (cols, rows) = store
            .query(
                "SELECT command, check_key, outcome, check_outcome, unsandboxed FROM bash_commands",
            )
            .unwrap();
        assert_eq!(
            cols,
            vec![
                "command",
                "check_key",
                "outcome",
                "check_outcome",
                "unsandboxed"
            ]
        );
        assert_eq!(
            rows[0],
            vec!["cargo", "cargo test", "sandbox-write", "", "0"]
        );
        assert_eq!(rows[3], vec!["rg", "", "ok", "", "1"]);
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

    fn hook_trace(outcome: HookOutcome, command: Option<&str>) -> HookTrace {
        HookTrace {
            epoch_ms: 1_700_000_000_000,
            event: "PreToolUse".to_string(),
            subject: Some("Bash".to_string()),
            command: command.map(String::from),
            outcome,
            exit_code: Some(2),
            duration_ms: Some(30),
            context_tokens: 5,
            tool_use_id: Some("t1".to_string()),
        }
    }

    #[test]
    fn hook_runs_round_trip_with_their_project_and_block_cost() {
        let mut store = Store::in_memory().unwrap();
        let mut alpha = session("a1");
        alpha.project = "alpha".to_string();
        store.ingest_session(&alpha, &[], &[]).unwrap();
        let blocked = hook_trace(HookOutcome::Blocked, Some("/tmp/example/guard.sh"));
        store
            .ingest_hook_activity("a1", &alpha.source_path, &[(blocked.clone(), Some(3))], &[])
            .unwrap();

        // The call id only matters while the block cost is measured at ingest.
        let blocked = HookTrace {
            tool_use_id: None,
            ..blocked
        };
        let traces = store.hook_traces().unwrap();

        assert_eq!(
            traces,
            vec![StoredTrace {
                trace: blocked,
                project: "alpha".to_string(),
                retry_calls: Some(3),
            }]
        );
    }

    #[test]
    fn re_ingesting_a_session_replaces_its_hook_runs() {
        let mut store = Store::in_memory().unwrap();
        let meta = session("s1");
        store.ingest_session(&meta, &[], &[]).unwrap();
        let ok = hook_trace(HookOutcome::Ok, None);
        store
            .ingest_hook_activity("s1", &meta.source_path, &[(ok.clone(), None)], &[])
            .unwrap();
        store
            .ingest_hook_activity("s1", &meta.source_path, &[(ok, None)], &[])
            .unwrap();

        assert_eq!(store.hook_traces().unwrap().len(), 1);
    }

    #[test]
    fn hook_triggers_count_tool_calls_prompts_and_sessions_per_project() {
        let mut store = Store::in_memory().unwrap();
        let mut alpha = session("a1");
        alpha.project = "alpha".to_string();
        let mut beta = session("b1");
        beta.project = "beta".to_string();
        store.ingest_session(&alpha, &[], &[]).unwrap();
        store.ingest_session(&beta, &[], &[]).unwrap();
        store
            .ingest_hook_activity(
                "a1",
                &alpha.source_path,
                &[],
                &[
                    (100, "Bash", CallResult::Succeeded),
                    (200, "Bash", CallResult::Failed),
                    (300, "Edit", CallResult::Pending),
                ],
            )
            .unwrap();
        store
            .ingest_hook_activity(
                "b1",
                &beta.source_path,
                &[],
                &[(400, "Bash", CallResult::Succeeded)],
            )
            .unwrap();
        store
            .ingest_prompts("a1", &alpha.source_path, &[(0, 100, "instruct")])
            .unwrap();

        let alpha_only = store.hook_triggers(Some("alpha")).unwrap();
        let everywhere = store.hook_triggers(None).unwrap();

        assert_eq!(
            alpha_only.tool_calls,
            vec![
                ToolCalls {
                    tool: "Bash".to_string(),
                    succeeded: 1,
                    failed: 1,
                    denied: 0,
                    pending: 0,
                },
                ToolCalls {
                    tool: "Edit".to_string(),
                    succeeded: 0,
                    failed: 0,
                    denied: 0,
                    pending: 1,
                },
            ]
        );
        assert_eq!(alpha_only.prompts, 1);
        assert_eq!(alpha_only.sessions, 1);
        assert_eq!(everywhere.tool_calls[0].succeeded, 2);
        assert_eq!(everywhere.sessions, 2);
    }
    #[test]
    fn a_session_whose_hook_activity_was_never_extracted_is_not_a_trigger_session() {
        let mut store = Store::in_memory().unwrap();
        // Stored by an analyzer that predates hook extraction, transcript gone.
        store.ingest_session(&session("old"), &[], &[]).unwrap();
        let scanned = session("new");
        store.ingest_session(&scanned, &[], &[]).unwrap();
        store
            .ingest_hook_activity("new", &scanned.source_path, &[], &[])
            .unwrap();

        assert_eq!(store.hook_triggers(None).unwrap().sessions, 1);
    }
    /// A scratch config tree with one real rule file reached through a
    /// symlinked rules directory — the layout a dotfiles-managed `~/.claude`
    /// has, and the reason the catalog joins on the resolved path.
    struct LinkedRules {
        dir: std::path::PathBuf,
    }

    impl LinkedRules {
        fn new(tag: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("cclens-rules-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(dir.join("dotfiles/rules")).unwrap();
            std::fs::write(dir.join("dotfiles/rules/rust.md"), "x").unwrap();
            std::fs::write(dir.join("dotfiles/rules/ts.md"), "x").unwrap();
            std::fs::create_dir_all(dir.join("config")).unwrap();
            #[cfg(unix)]
            std::os::unix::fs::symlink(dir.join("dotfiles/rules"), dir.join("config/rules"))
                .unwrap();
            LinkedRules { dir }
        }

        /// Where the catalog finds a rule: under the (symlinked) config root.
        fn config_path(&self, name: &str) -> String {
            self.dir
                .join("config/rules")
                .join(name)
                .display()
                .to_string()
        }

        /// Where a transcript records it: the real file.
        fn real_path(&self, name: &str) -> String {
            std::fs::canonicalize(self.dir.join("dotfiles/rules").join(name))
                .unwrap()
                .display()
                .to_string()
        }
    }

    impl Drop for LinkedRules {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn rule(id: &str, config_path: String, load_mode: LoadMode) -> Surface {
        Surface {
            kind: "rule".to_string(),
            id: id.to_string(),
            scope: Scope::Global,
            config_path,
            static_tokens: Some(100),
            startup_tokens: Some(0),
            load_mode,
            attrs: Vec::new(),
        }
    }

    fn load(epoch_ms: i64, path: &str, trigger: LoadTrigger) -> ContextLoad {
        load_v(epoch_ms, path, trigger, "2.1.400")
    }

    fn load_v(epoch_ms: i64, path: &str, trigger: LoadTrigger, version: &str) -> ContextLoad {
        ContextLoad {
            epoch_ms,
            path: path.to_string(),
            class: FileClass::Rule,
            trigger,
            version: Some(version.to_string()),
        }
    }

    fn ingest_loads(store: &mut Store, id: &str, loads: &[ContextLoad]) {
        let meta = session(id);
        store.ingest_session(&meta, &[], &[]).unwrap();
        store
            .ingest_context_loads(&meta.id, &meta.source_path, loads)
            .unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_load_recorded_under_the_real_path_joins_a_rule_catalogued_through_a_symlink() {
        let tree = LinkedRules::new("join");
        let mut store = Store::in_memory().unwrap();
        store
            .replace_surfaces(&[rule(
                "rust",
                tree.config_path("rust.md"),
                LoadMode::PathConditional,
            )])
            .unwrap();
        let rust = tree.real_path("rust.md");
        ingest_loads(
            &mut store,
            "s1",
            &[
                load(1_000, &rust, LoadTrigger::OnDemand),
                load(5_000, &rust, LoadTrigger::OnDemand),
            ],
        );

        let stats = store.rule_load_stats().unwrap();
        assert_eq!(stats.len(), 1);
        assert_eq!(
            (
                stats[0].loads,
                stats[0].sessions,
                stats[0].last_loaded_epoch
            ),
            (2, 1, Some(5))
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_conditional_rule_counts_only_sessions_that_could_have_recorded_a_load() {
        // s0 predates load records (none at all); s1 is an older install that
        // records only startup loads — written *after* the newer sessions, so a
        // timestamp gate would let it in; s2 and s3 are fully recording
        // sessions. Only s2 and s3 are observed.
        let tree = LinkedRules::new("observed");
        let mut store = Store::in_memory().unwrap();
        store
            .replace_surfaces(&[rule(
                "ts",
                tree.config_path("ts.md"),
                LoadMode::PathConditional,
            )])
            .unwrap();
        let (rust, other) = (tree.real_path("rust.md"), "/tmp/example/CLAUDE.md");
        ingest_loads(&mut store, "s0", &[]);
        ingest_loads(
            &mut store,
            "s1",
            &[load_v(40_000, other, LoadTrigger::Startup, "2.1.299")],
        );
        ingest_loads(
            &mut store,
            "s2",
            &[
                load(20_000, other, LoadTrigger::Startup),
                load(25_000, &rust, LoadTrigger::OnDemand),
            ],
        );
        ingest_loads(
            &mut store,
            "s3",
            &[load(30_000, other, LoadTrigger::Startup)],
        );

        let stats = store.rule_load_stats().unwrap();
        assert_eq!((stats[0].loads, stats[0].observed_sessions), (0, 2));
    }

    #[test]
    fn a_store_with_no_on_demand_load_observes_no_session() {
        let mut store = Store::in_memory().unwrap();
        store
            .replace_surfaces(&[rule(
                "ts",
                "/tmp/example/missing/ts.md".to_string(),
                LoadMode::PathConditional,
            )])
            .unwrap();
        ingest_loads(
            &mut store,
            "s1",
            &[load(1_000, "/tmp/example/CLAUDE.md", LoadTrigger::Startup)],
        );

        assert_eq!(store.rule_load_stats().unwrap()[0].observed_sessions, 0);
    }

    #[test]
    fn a_project_rule_is_observed_only_by_its_own_projects_sessions() {
        let mut store = Store::in_memory().unwrap();
        let mut project_rule = rule(
            "local",
            "/tmp/example/missing/local.md".to_string(),
            LoadMode::PathConditional,
        );
        project_rule.scope = Scope::Project("alpha".to_string());
        store.replace_surfaces(&[project_rule]).unwrap();
        for (id, project) in [("a1", "alpha"), ("b1", "beta"), ("b2", "beta")] {
            let mut meta = session(id);
            meta.project = project.to_string();
            store.ingest_session(&meta, &[], &[]).unwrap();
            store
                .ingest_context_loads(
                    &meta.id,
                    &meta.source_path,
                    &[load(1_000, "/tmp/example/other.md", LoadTrigger::OnDemand)],
                )
                .unwrap();
        }

        assert_eq!(store.rule_load_stats().unwrap()[0].observed_sessions, 1);
    }

    #[test]
    fn re_ingesting_a_session_replaces_its_context_loads() {
        let mut store = Store::in_memory().unwrap();
        let loads = [load(1_000, "/tmp/example/a.md", LoadTrigger::OnDemand)];
        ingest_loads(&mut store, "s1", &loads);
        ingest_loads(&mut store, "s1", &loads);

        let (_, rows) = store.query("SELECT COUNT(*) FROM context_loads").unwrap();
        assert_eq!(rows, vec![vec!["1".to_string()]]);
    }
    #[cfg(unix)]
    #[test]
    fn a_file_catalogued_under_two_rows_counts_each_load_once() {
        // The same rule file is catalogued globally and as project alpha's (a
        // session rooted where the global config lives). Alpha's load lands on
        // alpha's row, beta's on the global row — never on both.
        let tree = LinkedRules::new("dup");
        let mut store = Store::in_memory().unwrap();
        let global = rule(
            "rust",
            tree.config_path("rust.md"),
            LoadMode::PathConditional,
        );
        let mut local = global.clone();
        local.scope = Scope::Project("alpha".to_string());
        store.replace_surfaces(&[global, local]).unwrap();
        let rust = tree.real_path("rust.md");
        for (id, project) in [("a1", "alpha"), ("b1", "beta")] {
            let mut meta = session(id);
            meta.project = project.to_string();
            store.ingest_session(&meta, &[], &[]).unwrap();
            store
                .ingest_context_loads(
                    &meta.id,
                    &meta.source_path,
                    &[load(1_000, &rust, LoadTrigger::OnDemand)],
                )
                .unwrap();
        }

        let loads: Vec<(String, u64)> = store
            .rule_load_stats()
            .unwrap()
            .into_iter()
            .map(|s| (format!("{}:{}", s.scope, s.project), s.loads))
            .collect();
        assert_eq!(
            loads,
            vec![("global:".to_string(), 1), ("project:alpha".to_string(), 1)]
        );
    }
    #[cfg(unix)]
    #[test]
    fn loaded_sessions_count_only_observed_ones_while_loads_count_all() {
        // An always-on rule loads in s1, written by a version that records no
        // on-demand loads, and in s2. Both loads were paid; only s2 is observed, so the
        // share is 1 of 1, never 2 of 1.
        let tree = LinkedRules::new("share");
        let mut store = Store::in_memory().unwrap();
        store
            .replace_surfaces(&[rule("ts", tree.config_path("ts.md"), LoadMode::StartupFull)])
            .unwrap();
        let (ts, rust) = (tree.real_path("ts.md"), tree.real_path("rust.md"));
        ingest_loads(
            &mut store,
            "s1",
            &[load_v(10_000, &ts, LoadTrigger::Startup, "2.1.299")],
        );
        ingest_loads(
            &mut store,
            "s2",
            &[
                load(20_000, &ts, LoadTrigger::Startup),
                load(25_000, &rust, LoadTrigger::OnDemand),
            ],
        );

        let stat = &store.rule_load_stats().unwrap()[0];
        assert_eq!(
            (stat.loads, stat.sessions, stat.observed_sessions),
            (2, 1, 1)
        );
    }
    #[cfg(unix)]
    #[test]
    fn a_global_row_does_not_count_sessions_of_a_project_owning_the_same_file() {
        // The file is catalogued globally and as alpha's. Alpha's sessions load
        // it onto alpha's row, so they are alpha's population too: the global
        // row's share is over beta alone.
        let tree = LinkedRules::new("reach");
        let mut store = Store::in_memory().unwrap();
        let global = rule(
            "rust",
            tree.config_path("rust.md"),
            LoadMode::PathConditional,
        );
        let mut local = global.clone();
        local.scope = Scope::Project("alpha".to_string());
        store.replace_surfaces(&[global, local]).unwrap();
        let rust = tree.real_path("rust.md");
        for (id, project) in [("a1", "alpha"), ("a2", "alpha"), ("b1", "beta")] {
            let mut meta = session(id);
            meta.project = project.to_string();
            store.ingest_session(&meta, &[], &[]).unwrap();
            store
                .ingest_context_loads(
                    &meta.id,
                    &meta.source_path,
                    &[load(1_000, &rust, LoadTrigger::OnDemand)],
                )
                .unwrap();
        }

        let shares: Vec<(String, u64, u64)> = store
            .rule_load_stats()
            .unwrap()
            .into_iter()
            .map(|s| (s.scope, s.sessions, s.observed_sessions))
            .collect();
        assert_eq!(
            shares,
            vec![("global".to_string(), 1, 1), ("project".to_string(), 2, 2)]
        );
    }
    #[cfg(unix)]
    #[test]
    fn a_symlinked_alias_of_a_rule_is_unmeasured_not_never_firing() {
        // `alias.md` links to `rust.md` in the same rules dir: both rows name
        // one file, loads land on one of them, and the other must not read as
        // a rule that never fires.
        let tree = LinkedRules::new("alias");
        std::os::unix::fs::symlink(
            tree.dir.join("dotfiles/rules/rust.md"),
            tree.dir.join("dotfiles/rules/alias.md"),
        )
        .unwrap();
        let mut store = Store::in_memory().unwrap();
        store
            .replace_surfaces(&[
                rule(
                    "rust",
                    tree.config_path("rust.md"),
                    LoadMode::PathConditional,
                ),
                rule(
                    "alias",
                    tree.config_path("alias.md"),
                    LoadMode::PathConditional,
                ),
            ])
            .unwrap();
        let rust = tree.real_path("rust.md");
        for id in ["s1", "s2", "s3", "s4", "s5"] {
            ingest_loads(&mut store, id, &[load(1_000, &rust, LoadTrigger::OnDemand)]);
        }

        let rows: Vec<(String, u64, u64)> = store
            .rule_load_stats()
            .unwrap()
            .into_iter()
            .map(|s| (s.id, s.loads, s.observed_sessions))
            .collect();
        assert_eq!(
            rows,
            vec![("alias".to_string(), 5, 5), ("rust".to_string(), 0, 0)]
        );
    }

    fn shape_tool(name: &str, deferred: bool, tokens: u64) -> ShapeTool {
        ShapeTool {
            name: name.into(),
            server: crate::core::usage::mcp_server_of(name),
            deferred,
            tokens,
        }
    }

    fn session_tool(
        name: &str,
        eager: Option<u64>,
        listing: Option<u64>,
        loaded: bool,
        calls: u64,
    ) -> SessionTool {
        SessionTool {
            name: name.into(),
            server: crate::core::usage::mcp_server_of(name),
            eager_tokens: eager,
            listing_tokens: listing,
            loaded,
            calls,
        }
    }

    /// A session whose startup shape is `hash`: a 40-token system prompt, one
    /// built-in and one MCP tool eager, plus a stub that is not counted.
    fn tool_context(hash: &str, epoch_ms: i64, tools: Vec<SessionTool>) -> ToolContext {
        ToolContext {
            shapes: vec![WeighedShape {
                hash: hash.into(),
                system_tokens: 40,
                system_blocks: 2,
                tools: vec![
                    shape_tool("Bash", false, 300),
                    shape_tool("mcp__srv__q", false, 50),
                    shape_tool("DeferredToolPlaceholder", true, 9),
                ],
            }],
            uses: vec![ShapeUse {
                hash: hash.into(),
                first_epoch_ms: epoch_ms,
                requests: 3,
            }],
            tools,
            mcp_instructions: vec![("srv".into(), 7)],
        }
    }

    fn ingest_with_context(
        store: &mut Store,
        id: &str,
        epoch_ms: i64,
        ctx: u64,
        context: &ToolContext,
    ) {
        let mut meta = session(id);
        meta.start = Some(SessionStart {
            timestamp_ms: epoch_ms,
            ctx,
        });
        store.ingest_session(&meta, &[], &[]).unwrap();
        store
            .ingest_tool_context(&meta.id, &meta.source_path, context)
            .unwrap();
    }

    #[test]
    fn the_latest_session_start_is_decomposed_from_its_shape() {
        let mut store = Store::in_memory().unwrap();
        let tools = vec![
            session_tool("Bash", Some(300), None, false, 2),
            session_tool("mcp__srv__q", Some(50), None, false, 0),
            session_tool("mcp__srv__late", None, Some(4), true, 1),
            session_tool("WebSearch", None, Some(3), false, 0),
        ];
        ingest_with_context(
            &mut store,
            "old",
            1_000,
            999,
            &tool_context("h_old", 1_000, vec![]),
        );
        ingest_with_context(
            &mut store,
            "new",
            2_000,
            900,
            &tool_context("h_new", 2_000, tools),
        );

        let decomposition = store.latest_decomposition().unwrap().unwrap();
        assert_eq!(
            decomposition,
            Decomposition {
                session_id: "new".into(),
                project: "demo".into(),
                started_at: epoch_ms_to_rfc3339(2_000),
                observed_ctx: Some(900),
                system_tokens: 40,
                builtin_tool_tokens: 300,
                mcp_tool_tokens: 50,
                deferred_listing_tokens: 7,
                mcp_instruction_tokens: 7,
                config_tokens: 0,
            }
        );
    }

    #[test]
    fn no_recorded_shape_means_no_decomposition() {
        let mut store = Store::in_memory().unwrap();
        store.ingest_session(&session("s"), &[], &[]).unwrap();
        assert_eq!(store.latest_decomposition().unwrap(), None);
    }

    #[test]
    fn re_ingesting_a_session_replaces_its_tool_context() {
        let mut store = Store::in_memory().unwrap();
        let context = tool_context(
            "h",
            1_000,
            vec![session_tool("Bash", Some(300), None, false, 1)],
        );
        ingest_with_context(&mut store, "s", 1_000, 900, &context);
        ingest_with_context(&mut store, "s", 1_000, 900, &context);
        let stats = store.tool_load_stats().unwrap();
        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].eager_sessions, 1);
        assert_eq!(stats[0].calls, 1);
    }

    #[test]
    fn tool_load_stats_count_sessions_by_mode_and_use() {
        let mut store = Store::in_memory().unwrap();
        let first = vec![
            session_tool("mcp__srv__q", Some(50), None, false, 3),
            session_tool("mcp__srv__late", None, Some(4), true, 1),
        ];
        let second = vec![
            session_tool("mcp__srv__q", Some(60), None, false, 0),
            session_tool("mcp__srv__late", None, Some(4), false, 0),
        ];
        ingest_with_context(
            &mut store,
            "a",
            1_000,
            900,
            &tool_context("h", 1_000, first),
        );
        ingest_with_context(
            &mut store,
            "b",
            2_000,
            900,
            &tool_context("h", 2_000, second),
        );

        let stats = store.tool_load_stats().unwrap();
        assert_eq!(
            stats,
            vec![
                ToolLoadStat {
                    name: "mcp__srv__late".into(),
                    server: Some("srv".into()),
                    eager_sessions: 0,
                    eager_sessions_called: 0,
                    deferred_sessions: 2,
                    deferred_sessions_loaded: 1,
                    calls: 1,
                    eager_tokens: 0,
                    currently_deferred: true,
                },
                ToolLoadStat {
                    name: "mcp__srv__q".into(),
                    server: Some("srv".into()),
                    eager_sessions: 2,
                    eager_sessions_called: 1,
                    deferred_sessions: 0,
                    deferred_sessions_loaded: 0,
                    calls: 3,
                    eager_tokens: 60,
                    currently_deferred: false,
                },
            ]
        );
    }

    #[test]
    fn an_mcp_server_startup_cost_is_measured_from_its_latest_session() {
        let mut store = Store::in_memory().unwrap();
        let old = vec![session_tool("mcp__srv__q", Some(500), None, false, 2)];
        let new = vec![
            session_tool("mcp__srv__q", Some(50), None, false, 1),
            session_tool("mcp__srv__late", None, Some(4), false, 0),
        ];
        ingest_with_context(
            &mut store,
            "old",
            1_000,
            900,
            &tool_context("h", 1_000, old),
        );
        ingest_with_context(
            &mut store,
            "new",
            2_000,
            900,
            &tool_context("h", 2_000, new),
        );

        assert_eq!(
            store.measured_mcp_startup().unwrap(),
            vec![McpServerCost {
                server: "srv".into(),
                startup_tokens: 50 + 4 + 7,
                sessions: 2,
                calls: 3,
            }]
        );
    }

    #[test]
    fn a_measured_mcp_startup_cost_fills_the_catalogs_unknown() {
        let mut store = Store::in_memory().unwrap();
        let tools = vec![session_tool("mcp__srv__q", Some(50), None, false, 1)];
        ingest_with_context(
            &mut store,
            "s",
            1_000,
            900,
            &tool_context("h", 1_000, tools),
        );
        let unmeasured = |id: &str| Surface {
            kind: "mcp_server".into(),
            id: id.into(),
            scope: Scope::Global,
            config_path: "/tmp/mcp.json".into(),
            static_tokens: None,
            startup_tokens: None,
            load_mode: LoadMode::ToolSchema,
            attrs: Vec::new(),
        };
        store
            .replace_surfaces(&[unmeasured("srv"), unmeasured("never_seen")])
            .unwrap();

        let startup: Vec<(String, Option<i64>)> = store
            .effective_catalog()
            .unwrap()
            .into_iter()
            .map(|entry| (entry.id, entry.startup_tokens))
            .collect();
        assert_eq!(
            startup,
            vec![("never_seen".into(), None), ("srv".into(), Some(57))]
        );
    }
    #[test]
    fn a_session_whose_startup_shape_was_never_recorded_is_not_decomposed() {
        let mut store = Store::in_memory().unwrap();
        let mut context = tool_context("h", 2_000, vec![]);
        context.uses.insert(
            0,
            ShapeUse {
                hash: "h_missing".into(),
                first_epoch_ms: 1_000,
                requests: 1,
            },
        );
        ingest_with_context(&mut store, "s", 1_000, 900, &context);
        assert_eq!(store.latest_decomposition().unwrap(), None);
    }

    #[test]
    fn a_project_mcp_server_takes_only_its_own_projects_measurement() {
        let mut store = Store::in_memory().unwrap();
        for (id, project, tokens, epoch_ms) in
            [("a", "proj-a", 100, 1_000), ("b", "proj-b", 9_000, 2_000)]
        {
            let mut meta = session(id);
            meta.project = project.into();
            store.ingest_session(&meta, &[], &[]).unwrap();
            let mut context = tool_context(
                "h",
                epoch_ms,
                vec![session_tool("mcp__srv__q", Some(tokens), None, false, 0)],
            );
            context.mcp_instructions.clear();
            store
                .ingest_tool_context(&meta.id, &meta.source_path, &context)
                .unwrap();
        }
        let surface = |project: &str| Surface {
            kind: "mcp_server".into(),
            id: "srv".into(),
            scope: Scope::Project(project.into()),
            config_path: "/tmp/.mcp.json".into(),
            static_tokens: None,
            startup_tokens: None,
            load_mode: LoadMode::ToolSchema,
            attrs: Vec::new(),
        };
        store
            .replace_surfaces(&[surface("proj-a"), surface("proj-b"), surface("proj-c")])
            .unwrap();

        let startup: Vec<(String, Option<i64>)> = store
            .effective_catalog()
            .unwrap()
            .into_iter()
            .map(|entry| (entry.project, entry.startup_tokens))
            .collect();
        assert_eq!(
            startup,
            vec![
                ("proj-a".into(), Some(100)),
                ("proj-b".into(), Some(9_000)),
                ("proj-c".into(), None),
            ]
        );
    }
    #[test]
    fn a_session_without_its_startup_shape_does_not_measure_an_mcp_server() {
        let mut store = Store::in_memory().unwrap();
        let tools = vec![session_tool("mcp__srv__q", Some(50), None, false, 0)];
        ingest_with_context(
            &mut store,
            "old",
            1_000,
            900,
            &tool_context("h", 1_000, tools),
        );
        // Newer, but its startup shape was never recorded: its instructions alone
        // would pass for the server's whole startup cost.
        let mut partial = tool_context("h", 3_000, vec![]);
        partial.uses.insert(
            0,
            ShapeUse {
                hash: "h_missing".into(),
                first_epoch_ms: 2_000,
                requests: 1,
            },
        );
        ingest_with_context(&mut store, "new", 2_000, 900, &partial);

        assert_eq!(
            store.measured_mcp_startup().unwrap(),
            vec![McpServerCost {
                server: "srv".into(),
                startup_tokens: 57,
                sessions: 1,
                calls: 0,
            }]
        );
    }

    fn started(id: &str, epoch_secs: i64) -> SessionMeta {
        SessionMeta {
            start: Some(SessionStart {
                timestamp_ms: epoch_secs * 1000,
                ctx: 1,
            }),
            ..session(id)
        }
    }

    #[test]
    fn a_session_sample_carries_its_start_calls_and_failures_per_category() {
        let mut store = Store::in_memory().unwrap();
        let s1 = started("s1", 1_700_000_000);
        store.ingest_session(&s1, &[], &[]).unwrap();
        store.set_session_tool_calls("s1", 40).unwrap();
        store
            .ingest_tool_errors(
                "s1",
                &s1.source_path,
                &[
                    (1, "timeout", "", "Bash", ""),
                    (2, "timeout", "", "Bash", ""),
                    (3, "path-not-found", "", "Read", ""),
                ],
            )
            .unwrap();
        let samples = store.session_samples().unwrap();
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].start_epoch, 1_700_000_000);
        assert_eq!(samples[0].tool_calls, Some(40));
        assert_eq!(samples[0].errors.get("timeout"), Some(&2));
        assert_eq!(samples[0].errors.get("path-not-found"), Some(&1));
    }

    #[test]
    fn a_session_whose_calls_were_never_counted_has_no_count_not_zero() {
        let mut store = Store::in_memory().unwrap();
        store
            .ingest_session(&started("s1", 1_700_000_000), &[], &[])
            .unwrap();
        assert_eq!(store.session_samples().unwrap()[0].tool_calls, None);
    }

    #[test]
    fn a_session_with_no_known_start_is_not_sampled() {
        let mut store = Store::in_memory().unwrap();
        store.ingest_session(&session("s1"), &[], &[]).unwrap();
        assert!(store.session_samples().unwrap().is_empty());
    }

    #[test]
    fn surface_changes_round_trip_and_are_replaced_wholesale() {
        let mut store = Store::in_memory().unwrap();
        let change = ConfigChange {
            kind: "rule".to_string(),
            id: "tools/example".to_string(),
            scope: Scope::Project("demo".to_string()),
            changed_epoch: 2000,
            introduced_epoch: 1000,
            date_source: DateSource::Uncommitted,
            targets: vec!["timeout".to_string(), "path-not-found".to_string()],
            always_on: false,
        };
        store
            .replace_surface_changes(std::slice::from_ref(&change))
            .unwrap();
        assert_eq!(store.surface_changes().unwrap(), vec![change]);
        store.replace_surface_changes(&[]).unwrap();
        assert!(store.surface_changes().unwrap().is_empty());
    }
}
