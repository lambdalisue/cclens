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
`~/.claude` throughout this document means `$CLAUDE_CONFIG_DIR` when that
variable names a non-empty path, because Claude Code relocates the whole tree
under it (`claude_config_dir` in `config.rs`); an empty value is treated as unset.

A session started in the home directory has `$HOME` as its project root, and
`$HOME/.claude` is then the global config root itself. Reading it again as that
project's `<project>/.claude` would catalogue every global skill, rule and agent
a second time as a project row, which shadows the global row for that project's
sessions (`storage.md`): usage splits across two copies of one file and both
come out looking unused. So `read_project_surfaces` skips a project's `.claude`
when it resolves to the same directory as `~/.claude` — compared after
resolving symlinks and path separators, so `$CLAUDE_CONFIG_DIR` pointing at it,
or a symlinked `~/.claude`, counts as the same root, while a `$CLAUDE_CONFIG_DIR`
elsewhere leaves `$HOME/.claude` an ordinary project directory, as it is to
Claude Code. The project itself stays: its sessions are real and still route
usage, they just resolve to the global rows. The in-repo files outside `.claude`
are still read, since they are not global config: `$HOME/CLAUDE.md` is a
distinct file Claude Code loads in addition to `~/.claude/CLAUDE.md`, and
`$HOME/.mcp.json` is a project MCP file. What the global `CLAUDE.md` already
weighs — the file itself and everything it imports — is not counted again
under any project, whether the project reaches it through a symlinked
top-level `CLAUDE.md` or through an import of its own. Such a file's imports
are still followed from the project, though, since a relative import resolves
against the path it was reached by and can lead to a file the global side never
reads.

| Surface kind | Global | Project-local |
| --- | --- | --- |
| `skill` | `~/.claude/skills/<name>/SKILL.md` (+ plugin skills) | `<project>/.claude/skills/<name>/SKILL.md` |
| `rule` | `~/.claude/rules/**/*.md` | `<project>/.claude/rules/**/*.md` |
| `agent` | `~/.claude/agents/<name>.md` | `<project>/.claude/agents/<name>.md` |
| `claude_md` | `~/.claude/CLAUDE.md` | `<project>/CLAUDE.md`, `AGENTS.md`, nested |
| `hook`, `permission` | `~/.claude/settings.json` | `<project>/.claude/settings.json`, `settings.local.json` |
| `mcp_server` | `~/.claude/mcp.json` (top-level `mcpServers`) | `<project>/.mcp.json` |
| `mcp_tool` | the MCP server's advertised tool schemas (dynamic — see below) | — |

`CLAUDE.md` and `AGENTS.md` are two names for the same always-on context, and a
repo serving both agent conventions typically ships `AGENTS.md` as a symlink to
`CLAUDE.md`. Claude Code injects that content once, so the adapter weighs the
file once: `read_claude_md_surfaces` resolves each candidate and keeps the first
surface per resolved file, which reports the pair under `CLAUDE.md`. The
deduplication key is the resolved path, not the content — two independently
maintained files are two real always-on surfaces even when their text coincides.
Getting this wrong doubles the project's `startup_full` total, which is what the
always-on floor is reconciled against (`surfaces.md`).

A `CLAUDE.md` can pull other files in with `@path` imports, and Claude Code
injects the imported text every session just like the file's own. Weighing only
the importing file would book a config made of import lines as nearly free and
push the real always-on cost into the unattributed residual — exactly inverting
the "always-on heavy" wedge for such a layout. So `read_claude_md_surfaces`
follows the imports and records **each imported file as its own `claude_md`
surface** (`startup_full`, id `@<path>`, relative to the importing top-level
file's directory when inside it), rather than folding the text into the
importer: the cost lands on the file that carries it, and each one can be pruned
on its own. The import rules mirror Claude Code's documented behavior: an import
is an `@` opening a whitespace-separated word (an e-mail address is not one),
ignored inside code spans and fenced blocks; it resolves relative to the
importing file, `~/` to the home directory, or as an absolute path; recursion
stops after five hops; an import naming no readable file is skipped. The same
resolved-path deduplication as the `AGENTS.md` symlink applies across the whole
import graph — a file imported twice, imported in a cycle, or imported *and*
read as a top-level `AGENTS.md` is weighed once, with the top-level name
winning.

`settings.json` carries two surface kinds at once: `permissions` (allow/deny) and
`hooks` (matcher → command); the adapter splits one file into several surfaces.

Hooks are read from `~/.claude/settings.json` and, per project, from
`.claude/settings.json` and `.claude/settings.local.json` (`read_hook_surfaces`).
The shape is `hooks.<Event>[].{matcher, hooks[].{type, command}}`; every entry
of `hooks[]` is one surface, so two commands under one matcher are two hooks.
Its id is `<Event>[<matcher>] <command>` (`core::hook::HookDef::id`) — an
absent matcher is written `*`, and an entry with no `command` (a `prompt` or
`http` hook) is named by its type and what it runs, `(prompt) <prompt text>` —
whitespace-collapsed, and past 80 characters cut and suffixed with a hash of the
whole text — so two of them in one place stay two surfaces. The hash need not
survive a release: ids are rebuilt from config on every analyze and nothing
stored is keyed by them. Event, matcher and command together are the
identity because each alone repeats: one script is commonly wired to several
events, and one matcher to several scripts. The same hook declared in both a
project's shared and local settings is one surface. A hook injects no text of
its own at startup, so its static cost is unknown and its startup cost zero, and
its load mode is `on_demand` (paid when it fires — `events.md` "Hook runs").
Hooks a plugin or a `--settings` wrapper adds are not read; they surface as
orphaned usage when they leave traces (`surfaces.md`).
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

A surface is weighed **twice**, because one number cannot answer both questions
a reader has. `static_tokens` is the whole definition — for a skill or agent,
what its body costs *when invoked*. `startup_tokens` is the part every session
pays for merely having it installed: the whole text for an always-on file, the
frontmatter `description` alone for a skill or agent, nothing for a body that
waits (a path-conditional rule), and *unknown* for an MCP server. Only the second
is what deleting the surface reclaims, and reporting the first as if it were the
saving is a promise the deletion cannot keep (`surfaces.md`, "Ranking by what
removal actually saves"). Only the description text is weighed for the startup
half — the listing scaffolding Claude Code wraps it in is upstream's, not the
config's, so counting it would be inventing a figure.

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

So a surface records its **full static cost** (whole definition), its
**startup cost** (the slice of that text the load mode actually injects at
session start, per the table above), and its **load mode**, and the report
distinguishes "always-on" tax from "per-use" cost. A skill whose body is huge but
is invoked daily is a *body-trim* candidate; a `CLAUDE.md` that is huge is an
*always-on* candidate; a rule that never fires is a *delete* candidate.

## The MCP tool-schema hard case

`mcp_tool` static cost is the one surface whose definition is not a file on disk
— the schema is advertised by the running MCP server. Reading it statically
requires either connecting to the server or finding a cached schema. The adapter
treats this as best-effort: when a schema source is available it is weighed like
any other surface; when it is not, the surface is recorded with an unknown
static cost and the report flags it rather than reporting a false zero. The
runtime side is unaffected — MCP `tool_use` still shows up in `events`.

**Unmeasurable from config, measurable from the request shape.** No config-side
source exists: `mcp.json` holds only launch config (`command`/`args`), no
schema; there is no on-disk cache of advertised tool definitions
(`mcp-needs-auth-cache.json` is auth state only). So the config adapter still
records `mcp_server` / `mcp_tool` with an unknown static cost — it never invents a
"tool count × average schema size" figure.

Transcripts from releases that record the request shape (`session-format.md`)
close the gap from the usage side instead: every tool the server advertised
reaches the transcript, eager ones as full definitions in the shape and deferred
ones as a line in the deferred listing, plus the server's instructions block.
The transcript adapter weighs those with the same estimator as config text
(`approx_tokens`), and a server's **measured startup cost** is the sum over the
latest session that saw it (`Store::measured_mcp_startup`): eager definitions +
listing lines + instructions. Only a session whose startup shape was recorded counts, since
without it the eager definitions are unknown rather than zero. Where that measurement exists it stands in for
the unknown `startup_tokens` of the server's catalog row in the effective
catalog (`inventory`, `waste`) — a project-scoped row reads only its own
project's sessions, since two projects may configure one server name with
different tools, and a global row reads the latest session anywhere — so the server ranks among measured always-on
costs instead of as "schema?"; a server never seen in a recorded shape — an
older transcript tree, or a server that never connected — stays unknown.

## Frontmatter the adapter reads

- `skill`: `name`, `description` (the startup-loaded text — its weight *is*
  `startup_tokens`), `argument-hint`, `allowed-tools`. Body length feeds the
  on-demand cost. The description is read defensively: a plain or quoted scalar,
  or a block scalar folded from its indented continuation lines; a skill
  declaring none simply costs nothing at startup.
- `rule`: `paths:` (the load condition) and, when present, `description:`.
  Presence of `paths:` sets the load mode to path-conditional; its absence means
  the rule is always loaded. `description:` is frequently absent on real rules —
  read it defensively, do not rely on it.
- `agent`: `name`, `description` (weighed like a skill's), `tools`, and the
  keys that change what a run of it costs, kept as surface attributes
  (`surfaces.md`): `model` (the tier it runs on unless the call overrides it),
  `omitClaudeMd` (its runs start without the `CLAUDE.md` context — a smaller
  floor; Claude Code 2.1.271+), `autoCompactWindow` (when its context compacts;
  2.1.296+) and `experimental.cacheTtl` (its prompt-cache lifetime; 2.1.248+).
  Each is read as a plain scalar and kept verbatim; an absent key is simply not
  recorded, so an older definition carries no attributes rather than defaults.
- any markdown surface: `cclens-targets:` — the friction categories
  (`core::friction` labels, e.g. `edit-precondition`) the surface was written to
  reduce, as an inline `[a, b]` list, a block list, or a single scalar. This key
  is cclens's own, not Claude Code's; it is namespaced so it can never collide
  with a key Claude Code later gives meaning to. It turns the before/after
  comparison from "every category around the date" into a verdict on the
  declared ones (`surfaces.md` "Before / after a config change").

### Change dates

The before/after comparison needs when each compared surface (skill, rule,
`CLAUDE.md`) last changed. An `mcp_server` has no date of its own: its
`mcp.json` is shared by every server in it, so its date would belong to all of
them at once. An `agent` is dated by nothing either, for a different reason:
it acts in its own subagent transcript, and the failures the comparison reads
are the main thread's — a rate that could neither credit nor blame it.

The file's **mtime is not that date**. Config under `~/.claude` is routinely a
symlink chain into a dotfiles checkout (often through a Nix store, whose files
carry epoch-1 mtimes), and a checkout, rebase, or copy rewrites mtimes without
changing content — observed on a real tree, a rule's mtime trailed its commit by
two days. So the adapter canonicalizes the path first, then asks git:

1. `git log --follow -p -- <file>` in the file's real directory. The newest
   commit's author time is the **last change**, the oldest commit's the
   **introduction** (`--follow` keeps a rename from reading as a new file).
   Author time, because that is when the user wrote the text in place;
   committer time moves on every rebase. The commits are taken in history
   order, not by the largest and smallest time: a cherry-pick keeps its
   original author time, so the latest commit can carry an earlier time than
   one before it.
2. A commit whose diff only adds, edits or removes the `cclens-targets:`
   declaration — the key, its list items, and the frontmatter fences and blank
   lines around it — is **not** a change. The declaration is metadata about the
   rule, and counting it would make declaring a rule erase its own
   before-window: the boundary would jump to the day the user wrote the key.
3. When the file has uncommitted modifications beyond the declaration
   (`git diff HEAD`, filtered the same way), the installed text is newer than
   any commit, so the last change is the mtime and the provenance says
   `uncommitted`.
4. Outside a repository, or untracked, both dates fall back to the mtime, with
   provenance `mtime` so a report can flag the weaker evidence.

This is the only place cclens runs a subprocess during `analyze`; it lives in
the adapter (`adapter::history`) and the core sees plain epochs.

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

Rules and `CLAUDE.md` files are also identified by **where they are**, because
their usage signal (`context_load`, `events.md`) names a loaded file, and Claude
Code records it with symlinks resolved. The catalog records each surface's
`config_path` as found under the config root and, beside it, its
`resolved_path` — the same file canonicalized. The two differ exactly when the
config is symlinked (a `~/.claude/rules` link into a dotfiles checkout, or a
`$CLAUDE_CONFIG_DIR` reached through a link), which is when a string match on
`config_path` would silently miss every load. A path that cannot be
canonicalized (gone since it was read) keeps no resolved path, and joins
nothing rather than a guess.

A surface can exist at both global and project scope under one `(kind, id)`. The
catalog stores both rows (keyed with `scope`), but the join resolves to the
**effective surface** — project shadows global, mirroring Claude Code's own
override — so one event never matches two surface rows. The full contract is in
`storage.md` "Surface identity, scope, and the effective join".
