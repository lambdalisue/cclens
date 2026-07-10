# Storage Specification

The store is the SQLite database `analyze` writes and every consumer reads
(`architecture.md`). It holds three tables that mirror the catalog×usage model —
a shared `sessions` dimension, the `surfaces` catalog, the `events` spine — plus
`ingested_files` for incremental rebuilds. This spec defines the schema and the
ingest contract; the *meaning* of the columns lives in the specs that own them
(`events.md`, `surfaces.md`, `config-format.md`).

## Schema

```sql
-- Shared dimension. One row per analyzed transcript.
CREATE TABLE sessions (
    id          TEXT PRIMARY KEY,   -- sessionId
    project     TEXT NOT NULL,      -- normalized (worktree folded; see below)
    slug        TEXT NOT NULL,      -- raw cwd-slug
    root        TEXT NOT NULL,      -- real start directory (records' cwd, worktree folded); '' when unknown
    source_path TEXT NOT NULL,      -- the main transcript file
    started_at  TEXT NOT NULL,      -- RFC3339 UTC
    version     TEXT                -- Claude Code version, when present
);

-- Catalog: everything installed, with its static cost. Read from live config.
CREATE TABLE surfaces (
    kind          TEXT NOT NULL,    -- skill | rule | mcp_server | mcp_tool | hook | claude_md | permission | agent
    id            TEXT NOT NULL,    -- stable identity within the kind
    scope         TEXT NOT NULL,    -- global | project
    project       TEXT NOT NULL,    -- owning project's normalized slug; '' for global rows
    config_path   TEXT,
    static_tokens INTEGER,          -- token weight of the injected definition; NULL if unknown (e.g. mcp_tool)
    load_mode     TEXT NOT NULL,    -- startup_full | startup_description | path_conditional | on_demand | tool_schema
    attrs_json    TEXT,             -- kind-specific extras (paths glob, hook matcher, …)
    PRIMARY KEY (kind, id, scope, project)
);

-- Usage spine. One row per extracted event; a skill span is one kind.
CREATE TABLE events (
    id                   INTEGER PRIMARY KEY,
    session_id           TEXT NOT NULL REFERENCES sessions(id),
    source_path          TEXT NOT NULL,   -- file this event came from (ingest delete key)
    source_line          INTEGER,         -- 0-based line index in source_path; recovers the raw record (prompt text for goal-3 clustering) without storing it
    kind                 TEXT NOT NULL,   -- skill_invocation | tool_use | agent_spawn | prompt | tool_error | compaction | permission_prompt | …
    surface_kind         TEXT,            -- join key into surfaces (NULL for surfaceless kinds)
    surface_id           TEXT,            -- for tool_error: the friction category (no surface join, surface_kind NULL)
    source               TEXT,            -- kind-specific detail string: slash|tool (skill path); behavior class (prompt); a short error-text excerpt (tool_error); NULL otherwise
    started_at           TEXT NOT NULL,   -- RFC3339 UTC
    started_epoch        INTEGER NOT NULL,-- UTC unix seconds (bucketing)
    duration_sec         REAL NOT NULL,
    is_trailing          INTEGER NOT NULL,-- 1 when closed only by session end (duration is a lower bound)
    out_tokens           INTEGER NOT NULL,
    ctx_growth           INTEGER NOT NULL,
    ctx_start            INTEGER NOT NULL,
    ctx_peak             INTEGER NOT NULL,
    sub_tokens           INTEGER NOT NULL,
    sub_agent_count      INTEGER NOT NULL,
    sub_tokens_estimated INTEGER NOT NULL,
    model                TEXT,            -- representative model (skill_invocation); the originating tool name (tool_error)
    target               TEXT,            -- the failed call's subject: file_path edited / command run (tool_error)
    attrs_json           TEXT
);

CREATE INDEX events_by_surface ON events(surface_kind, surface_id);
CREATE INDEX events_by_time    ON events(started_epoch);

-- Incremental-ingest fingerprints.
CREATE TABLE ingested_files (
    path  TEXT PRIMARY KEY,
    mtime INTEGER NOT NULL,
    size  INTEGER NOT NULL
);

-- Analyze-run metadata: analyzed_at (RFC3339 UTC), projects_dir, config_dir.
-- Freshness reporting reads analyzed_at; auto-analyze on read commands re-runs
-- the analysis against the same recorded roots (cli.md).
CREATE TABLE meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

-- A clean read view over tool_error events for ad-hoc SQL (the `sql` command).
-- The friction signal is overloaded onto generic event columns; the view names
-- it and joins the project so a query need not know the encoding.
CREATE VIEW tool_errors AS
SELECT e.session_id, s.project,
       e.surface_id AS category, e.source AS excerpt, e.model AS tool,
       e.target, e.started_epoch
FROM events e JOIN sessions s ON e.session_id = s.id
WHERE e.kind = 'tool_error';
```

The store is also a **read surface for arbitrary queries** (`cli.md`: `sql`).
Because it is plain SQLite holding already-extracted facts, the session-analysis
slices a consumer might want — the `optimize` agent chasing a root cause, say —
are a `SELECT` away, which is cheaper and less error-prone than re-parsing the
raw transcripts. `sql` opens the db **read-only** (`Store::open_readonly`) so an
ad-hoc query cannot mutate the derived store; views like `tool_errors` keep those
queries clean despite the column overloading.

`attrs_json` is the additive escape hatch: a new event kind or surface attribute
lands there without a migration, and graduates to a column only if reports query
it often. This is what keeps "add a new surface" a non-migrating change.

### Prompt text is referenced, never copied

A `prompt` event stores `(source_path, source_line)`, not the prompt string. The
future skill-extraction layer (clustering recurring prompts into candidate
skills — `architecture.md`) recovers the text by re-reading that line on demand.
This keeps personal data out of the store (`.claude/rules/session-data-privacy.md`)
while reserving the capability now, so it survives transcript rotation: the
pointer is cheap to keep and the alternative — discovering later that the text is
gone — is unrecoverable. (If the source file is rotated away, the pointer simply
resolves to nothing; the event's counts remain.)

## Surface identity, scope, and the effective join

`surfaces` is keyed `(kind, id, scope, project)` because the same logical
surface can be installed globally and in **any number of projects** (e.g. a
`git-commit` skill in `~/.claude/skills/` and in two projects'
`.claude/skills/`). Events carry only `(surface_kind, surface_id)` — the
transcript does not reveal which copy was loaded — but they *do* join a session
whose `project` is known, and Claude Code's own resolution makes a project's
copy shadow the global one **inside that project only**.

So the catalog×usage join is defined per session project: an event from project
P joins P's project row for that `(kind, id)` when one exists, else the global
row. The join stays strictly 1:N-safe — one event never matches two surface
rows, and one project's usage never inflates another project's copy
(`Store::effective_catalog`). Scope and project are retained on every catalog
row so reports can route a finding to the config layer that owns the fix
(`cli.md` `--scope`). `surfaces.md` and `config-format.md` describe the same
contract.

## Timestamps are stored in UTC

`started_at` / `started_epoch` are UTC; the transcript gives UTC and the store
keeps it. Timezone is a **presentation** concern: the report converts to the
target zone (default JST) when bucketing and displaying (`cli.md`). Storing local
time would corrupt sorting and break portability across machines.

## Project normalization

A worktree directory has its own `cwd-slug` (a `...--wt-feat-x` suffix), so the
same logical project is otherwise scattered across slugs. `sessions.project` is
the **normalized** project (worktree suffix folded to the parent); `sessions.slug`
keeps the raw slug. Reports group by `project` so a project's usage is not split,
while the raw slug stays available for drill-down. The exact folding rule is an
adapter concern documented where it is implemented, but it must preserve a
testable invariant: **one logical project maps to exactly one `project` value,
and folding is idempotent** (folding an already-folded value is a no-op). Getting
this wrong splits or merges a project's usage, which is a correctness bug for
every per-project wedge, not a presentation detail.

## Incremental ingest

Transcripts are append-only and **active sessions keep growing**, so re-running
`analyze` must be cheap and idempotent.

- Before ingesting a file, compare `(mtime, size)` against `ingested_files`.
  Unchanged → skip. Changed or new → re-ingest.
- Re-ingest is **replace, not append**: delete all `events` whose `source_path`
  equals this file, then re-extract from the whole file and insert. This is why
  `events.source_path` exists. Replacing avoids duplicate rows when a still-open
  session is analyzed twice (the second pass simply supersedes the first).
- `surfaces` is rebuilt wholesale on each run from current config — the catalog
  is a snapshot of *now*, not an accumulation. (Usage is historical; catalog is
  current — `surfaces.md`.)

`(mtime, size)` is a cheap change detector, not a content hash; a touch that
changes mtime without changing bytes triggers a harmless idempotent replace, and
any byte change moves size or mtime. A content hash is a later hardening option
if the cheap detector proves insufficient.

### Reading files that grow mid-run

Active sessions are appended to while `analyze` runs, so the reader must tolerate
a **torn read**: a partial final line is skipped (JSONL append is not atomic at
line granularity), never parsed as a broken record. The fingerprint stored in
`ingested_files` is captured **after** the read completes — recording the size
actually consumed. A file that grew during the read therefore shows a changed
`(mtime, size)` on the next run and is re-ingested, picking up the tail. Stamping
the fingerprint before the read would record the post-growth size against
pre-growth events and permanently skip the tail.

## Why these three tables, not one span table

An earlier design had a single skill-centric `spans` table. It could not hold
rules, hooks, MCP, or prompts without contortion, and it conflated the catalog
(what is installed) with usage (what ran). Splitting into `sessions` (dimension)
+ `surfaces` (catalog) + `events` (usage) lets every configuration surface share
one usage spine and one catalog shape, and makes the optimization analysis a
join rather than a special case per surface — see `surfaces.md`.
