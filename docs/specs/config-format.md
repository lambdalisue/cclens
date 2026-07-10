# Config Format Specification

This spec catalogues what the **adapter layer** knows about Claude Code **config
files** — the catalog half of the catalog×usage model (`architecture.md`). Its
sibling `session-format.md` covers the usage half. Reading config answers *what
is installed and what does it cost to have installed*; the transcript answers
*what was actually used*. Only the adapter uses the paths and shapes below
(`.claude/rules/format-isolation.md`). Like `session-format.md`, this is a spec
that tracks an upstream format and will drift.

> Observed against a real `~/.claude/` tree on 2026-06-14, Claude Code `2.1.x`.

## Where config lives

Config exists at two scopes; project overrides/extends global. The adapter scans
both and records the scope on each surface — `Scope::Project` carries the
owning project's normalized slug, since many projects coexist in one catalog
(`storage.md`). Which project roots get scanned is driven by the *sessions*:
`analyze` collects the distinct real session roots (`session-format.md`, the
records' `cwd`, worktree folded onto the parent checkout) and scans each root
that still exists on disk (`read_project_surfaces` in `config.rs`); a root that
was deleted since its sessions ran simply contributes no project surfaces.

| Surface kind | Global | Project-local |
| --- | --- | --- |
| `skill` | `~/.claude/skills/<name>/SKILL.md` (+ plugin skills) | `<project>/.claude/skills/<name>/SKILL.md` |
| `rule` | `~/.claude/rules/**/*.md` | `<project>/.claude/rules/**/*.md` |
| `agent` | `~/.claude/agents/<name>.md` | `<project>/.claude/agents/<name>.md` |
| `claude_md` | `~/.claude/CLAUDE.md` | `<project>/CLAUDE.md`, `AGENTS.md`, nested |
| `hook`, `permission` | `~/.claude/settings.json` | `<project>/.claude/settings.json`, `settings.local.json` |
| `mcp_server` | `~/.claude/mcp.json` (top-level `mcpServers`) | `<project>/.mcp.json` |
| `mcp_tool` | the MCP server's advertised tool schemas (dynamic — see below) | — |

`settings.json` carries two surface kinds at once: `permissions` (allow/deny) and
`hooks` (matcher → command); the adapter splits one file into several surfaces.
MCP servers are **not** in `settings.json` — they live in a separate `mcp.json`
(verified: `~/.claude/settings.json` has no `mcpServers` key; `~/.claude/mcp.json`
holds them). If a future Claude Code version also accepts inline `mcpServers` in
`settings.json`, the adapter reads both — but `mcp.json` is the source of record.

## Static cost

A surface's **static cost** is the token weight of the text it injects into the
model's context. It is computed by the adapter from the file content; it is an
**approximation** (a ranking signal, not a billing figure — `architecture.md`),
so a tokenizer that is *close* and *consistent* is sufficient. The exact
tokenizer is a tuning choice (see open question) injected into the weighing
function, not hard-wired.

**Static cost is a config-side estimate, not a measured runtime component.** The
transcript exposes only a *total* prompt size, never a per-surface breakdown
(`session-format.md`), so `static_tokens` is never validated, on its own, as the
exact number of tokens that surface contributes at runtime. Two things are
therefore inherently uncertain and must not be presented as fact: the tool's
tokenizer differs from Claude Code's, and whether the text is actually injected
(and when) depends on `load_mode`, which is itself an assumption (below). The one
honest empirical check the tool *can* make is reconciliation against observed
context — see `surfaces.md` "always-on reconciliation". Reports present
`static_tokens` as an estimate, labelled as such.

### Load mode is an assumption table, versioned against the release

`load_mode` is **not read from anything** — it is the adapter's model of how
Claude Code loads each surface kind, maintained as a table pinned to the observed
release (`2.1.x`). It drives the most consequential wedge ("always-on heavy") and
its suggested moves, so when the table is wrong every wedge built on it is wrong.
It belongs in the "will drift" caveat alongside field names: a release that, say,
lazy-loads `CLAUDE.md` invalidates the `startup_full` assumption silently. The
stored enum uses underscores (`startup_full`, `path_conditional`, …); the prose
labels below use hyphens for readability — same values.

What counts as "the injected text" differs by surface and is the crucial
distinction for optimization, because **always-on cost is the expensive kind**:

| Load mode | Surfaces | Cost character |
| --- | --- | --- |
| **startup-full** | `CLAUDE.md` / `AGENTS.md`, memory index | Paid every session, unconditionally. The heaviest tax. |
| **startup-description** | `skill`, `agent` | Only the frontmatter `description` is loaded at startup; the body is on-demand. Cheap to keep, unless many accumulate. |
| **path-conditional** | `rule` with `paths:` frontmatter | Loaded only when a matching file is in play; full body when it fires. |
| **on-demand** | `skill` / `agent` body, when invoked | Paid per use — this is what `events` measures at runtime. |
| **tool-schema** | `mcp_tool`, built-in tools | The tool definition injected into the system/tool context. MCP schemas can be very large (the deferred-tool catalog). |

So a surface records both its **full static cost** (whole definition) and its
**load mode**, and the report distinguishes "always-on" tax from "per-use" cost.
A skill whose body is huge but is invoked daily is a *body-trim* candidate; a
`CLAUDE.md` that is huge is an *always-on* candidate; a rule that never fires is
a *delete* candidate.

## The MCP tool-schema hard case

`mcp_tool` static cost is the one surface whose definition is not a file on disk
— the schema is advertised by the running MCP server. Reading it statically
requires either connecting to the server or finding a cached schema. The adapter
treats this as best-effort: when a schema source is available it is weighed like
any other surface; when it is not, the surface is recorded with an unknown
static cost and the report flags it rather than reporting a false zero. The
runtime side is unaffected — MCP `tool_use` still shows up in `events`.

**Investigated and found unmeasurable from local data (the honest limit).** None
of the would-be sources exist: `mcp.json` holds only launch config
(`command`/`args`), no schema; there is no on-disk cache of advertised tool
definitions (`mcp-needs-auth-cache.json` is auth state only); the transcript
exposes only the tools that were *used*, so a server's full tool count is unknown
and an *unused* server — exactly the one worth flagging — yields no signal at
all. A differential (baseline with vs without a server) is also unavailable
because every project shares the global `mcp.json`. A "tool count × average
schema size" estimate would therefore be a fabrication for the cases that matter,
so the tool does **not** invent one. The honest substitutes it does provide:
per-server **call frequency** (`surfaces` / `wedges` — a used vs dead-weight
signal), and the **residual** in `baseline` (an upper bound on system + built-in
tools + all MCP schemas combined). Isolating one server's cost would need runtime
schema access this tool does not have.

## Frontmatter the adapter reads

- `skill`: `name`, `description` (the startup-loaded text), `argument-hint`,
  `allowed-tools`. Body length feeds the on-demand cost.
- `rule`: `paths:` (the load condition) and, when present, `description:`.
  Presence of `paths:` sets the load mode to path-conditional; its absence means
  the rule is always loaded. `description:` is frequently absent on real rules —
  read it defensively, do not rely on it.
- `agent`: `name`, `description`, `tools`, `model`.

Parsing is defensive: unknown frontmatter keys are ignored, missing optional
keys tolerated. A surface the adapter cannot fully parse is still catalogued
(with what it could read) rather than dropped — an unparseable surface is itself
worth surfacing.

## Identity and the join

Each surface has a stable `(kind, id)` — e.g. `(skill, git-commit)`,
`(mcp_server, playwright)`, `(rule, spec-sync)`. The same identity is what
`events` carries in `surface_kind` / `surface_id`, so the catalog×usage join
(`surfaces.md`) is a key match. The adapter is responsible for deriving the same
id from both sides: the `skill` surface id from the directory/`name`, and the
matching event id from the invoked skill name in the transcript.

A surface can exist at both global and project scope under one `(kind, id)`. The
catalog stores both rows (keyed with `scope`), but the join resolves to the
**effective surface** — project shadows global, mirroring Claude Code's own
override — so one event never matches two surface rows. The full contract is in
`storage.md` "Surface identity, scope, and the effective join".
