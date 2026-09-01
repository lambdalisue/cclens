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
    kind                 TEXT NOT NULL,   -- skill_invocation | session_start | tool_use | agent_spawn | prompt | tool_error | compaction | permission_prompt | …
    surface_kind         TEXT,            -- join key into surfaces (NULL for surfaceless kinds)
    surface_id           TEXT,            -- for tool_error: the friction category (no surface join, surface_kind NULL); for file_edit: the basename hotspots group on (the full path is in target)
    source               TEXT,            -- kind-specific detail string: slash|tool (skill path); behavior class (prompt); a short error-text excerpt (tool_error); NULL otherwise
    started_at           TEXT NOT NULL,   -- RFC3339 UTC
    started_epoch        INTEGER NOT NULL,-- UTC unix seconds (bucketing)
    duration_sec         REAL NOT NULL,
    is_trailing          INTEGER NOT NULL,-- 1 when closed only by session end (duration is a lower bound)
    out_tokens           INTEGER NOT NULL,
    ctx_growth           INTEGER NOT NULL,
    ctx_start            INTEGER NOT NULL,-- for session_start: the context the session began with (the always-on floor)
    ctx_peak             INTEGER NOT NULL,
    sub_tokens           INTEGER NOT NULL,
    sub_agent_count      INTEGER NOT NULL,
    sub_tokens_estimated INTEGER NOT NULL,
    model                TEXT,            -- representative model (skill_invocation); the originating tool name (tool_error)
    target               TEXT,            -- the failed call's subject: file_path edited / command run (tool_error); the edited file's full path (file_edit)
    attrs_json           TEXT
);

CREATE INDEX events_by_surface ON events(surface_kind, surface_id);

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

`sessions.root` folds by the same rule one level down, on the real path rather
than the slug. The two must agree: the slug form keys on a `--wt-` infix and is
separator-free, so the path form accepts **either separator** — otherwise a
worktree recorded on Windows folds by `project` while `root` stays split, and
per-root config scanning (`config-format.md`) sees two roots where there is one
checkout.

## Schema evolution is non-destructive

The store outlives its inputs. Claude Code prunes transcripts on its own
retention schedule, and once a transcript is gone the rows extracted from it are
the only surviving record of that session — nothing on disk can rebuild them.
So an upgrade may never resolve schema drift by asking for the file to be
deleted; `Store::open` migrates in place instead (`migrate` in `store.rs`).

The declared schema is the single source of truth for the target shape: it is
applied to a scratch in-memory database and read back through
`PRAGMA table_info`, and the live store is reconciled against that. Adding a
column to the declaration is therefore the whole of a routine migration — there
is no parallel list of ALTER statements to keep in step, which is what made the
previous "detect a missing column, refuse the file" check drift into a
delete-and-lose-history instruction.

Drift is reconciled per object by what that object costs to lose:

- `sessions`, `events`, `ingested_files`, `meta` hold history, so a missing
  column is appended with `ALTER TABLE ADD COLUMN`. Existing rows take the
  column's default, which the "report the gap, never substitute" contract below
  already covers. A column that is already present is **compared, not skipped** —
  matching on name alone would pass silently over a retype or a changed default,
  leaving the store permanently diverged from the declaration it is meant to
  converge on.
- `surfaces` is a snapshot of live config rebuilt on every analyze, so a shape
  change drops and recreates it. The shape is compared first — an unchanged
  catalog must survive reopening, or a `--frozen` read would report an empty one.
  When it *is* rebuilt, `analyzed_at` is cleared, so a `--frozen` read is routed
  to the freshness warning instead of printing an ordinary header over an empty
  catalog.
- Indexes and views carry no data, so a drifted one is simply dropped and
  recreated. Only a *drifted* one: `CREATE INDEX / VIEW IF NOT EXISTS` would keep
  a stale definition — a reader would then silently query yesterday's column
  mapping — but reindexing `events` on every open is not free either, so the
  stored `CREATE` statement is compared against the declared one and matched
  objects are left untouched. Objects the declaration does not name are also left
  untouched: the store is meant to be explored with ad-hoc SQL, so a helper view
  someone saved into the file is theirs to keep.

Indexes and views are **declared apart from the tables**, in their own constants,
and applied only after column reconciliation. Declared alongside the table they
would be replayed by the same `execute_batch` that creates missing tables, before
any column was added — so an index over a column a future release introduces
would hit `no such column` on a legacy store and take the whole migration down,
which is the destructive refusal this design exists to remove. Deriving the names
to reconcile from the declaration is also what removes a hand-kept list: such a
list drifts, and a declared object missing from it would collide with its own
`CREATE` on the store's second open.

The whole migration runs in one transaction, as every other multi-statement write
here does: reconciliation can bail partway through, and a store left
half-migrated is the outcome in-place upgrading exists to avoid.

`PRAGMA user_version` carries `SCHEMA_VERSION`. Reconciliation only ever adds, so
routine additive changes do not bump it; it exists to catch what reconciliation
cannot express. A store stamped **newer** than this build is refused rather than
written to — this build's shape would overwrite changes it does not understand,
in a file the newer binary still reads correctly. The version is also the seam
for a future change that reconciliation cannot cover (a column removed, retyped,
or given a new meaning). Reconciliation now *fails* on such drift rather than
ignoring it, so the seam cannot be skipped by accident; the migration written to
fill it must state what it discards, because the answer is no longer "nothing".

`Store::open_readonly` (the `sql` command) cannot migrate. An ad-hoc query
against a store older than this build surfaces SQLite's own missing-column
error; any read command opens read-write and migrates first.

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

The fingerprint is keyed on the *file*, not on what cclens knew how to extract
from it, so a store built by an older cclens keeps skipping transcripts that have
since stopped changing — a closed session never re-ingests, so any event kind
added after it was written stays missing for that session, and any field added
after it stays NULL. There is no ingest-level invalidation.

The contract is that a reader **reports the gap or drops the row**, never
substitutes something plausible:

- `overhead` reports the always-on floor as unavailable when no `session_start`
  was observed, rather than falling back to a skill span's `ctx_start`
  (`surfaces.md`).
- Thrash detection excludes `file_edit` rows with no `target`, rather than
  falling back to the basename in `surface_id` (`events.md`).

Both fallbacks would have looked like working output while silently restoring the
bug the field exists to prevent, which is worse than an empty report. An empty or
unavailable result names a gap in what was extracted at the time — the transcript
it came from may be gone, so re-analysis cannot always close it.

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
