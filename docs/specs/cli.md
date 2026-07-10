# CLI Specification

The CLI is the user's surface onto the stages (`architecture.md`): `analyze`
populates the store, the **views** and `sql` read it, and `optimize` hands the
findings to an interactive `claude` session. This spec defines the command
contract and the reporting model; exact flag spellings live with the
implementation and stay in sync via `.claude/rules/spec-sync.md`.

## `analyze` — populate the store

```
cclens analyze [--projects <dir>] [--config <dir>] [--db <path>]
```

Reads both Claude Code inputs — session transcripts (default
`~/.claude/projects/`) and live config (default `~/.claude/` plus the relevant
project `.claude/`) — and writes the SQLite store (`storage.md`). Incremental and
idempotent: unchanged transcripts are skipped, changed ones replaced, the surface
catalog rebuilt from current config (`storage.md`). The verb is `analyze`, not
`build` — it analyzes raw input into facts.

`analyze` reads everything **read-only** and never copies input into the repo or
the store beyond the derived facts (`.claude/rules/session-data-privacy.md`).

## Views — read the store

```
cclens <view> [--scope global|project[:<slug>]] [--by <bucket>] [--frozen]
              [--format table|markdown|json] [--db <path>]
```

Each view is its own top-level command; their queries only read the store, never
raw input. The set is deliberately curated — a view earns a command by carrying
logic a one-line query cannot (a classification, an algorithm, a suggestion). Any
*other* slice is a `sql` query (below), so thin "just count a column" views are
not commands.

### Freshness — reads auto-refresh

A stale store the user cannot notice was a real failure mode, so a read command
**runs the incremental `analyze` stage first by default** — the same composition
`optimize` always had (`architecture.md`). With `ingested_files` skipping
unchanged transcripts (`storage.md`), an up-to-date store costs one stat per
transcript. The refresh reuses the roots recorded in `meta` (`projects_dir`),
and a missing db is simply created — "absent db" is an error only for `sql`.
`--frozen` suppresses the refresh and reads the store as-is.

Every read prints a **one-line freshness header on stderr** (stdout stays clean
for pipes): what was refreshed, or — under `--frozen` / `sql` — how old the
store is, with a re-analyze hint once it is older than a day.

### Scope — route each finding to the config layer that owns the fix

"Optimize my global setup" and "optimize this project" are different tasks, and
mixing them lets one busy project drown the global picture. Every finding is
routed (`core::scope`):

- **Config wedges** route by the surface's own scope column (`storage.md`).
- **Friction categories** route by concentration: a strict-majority project owns
  the category (fix in that project's config); a spread category is global (a
  cross-project habit).
- **Thrash** routes to the project it happened in; **cd% / prompting / token
  totals** are behavioral, hence global.

`--scope` narrows a report to one layer: `global`, `project` (all projects), or
`project:<slug>` (one). `doctor`, the optimize briefing, and (by default) the
scoped views show **both layers split into sections** rather than a merged
ranking.

| View | Answers |
| --- | --- |
| `doctor` | The entry point: a one-screen health check **written for someone who has never seen cclens's vocabulary** — every item says what happened, why it costs, and what to do. Sections: `WHAT TO FIX FIRST` (recurring problems as a prioritized to-do list, each routed to the owning config layer with the follow-up command), `COST` (the session-start context and output totals in plain words, humanized units), `CONFIG WORTH PRUNING` (dead or heavy config per owner), `LOOKS HEALTHY` (what explicitly needs no action). `--scope` narrows to one layer; the text form leads with actions, never a stats dump. |
| `inventory` | The catalog×usage join per surface **row** (scope and owning project shown; usage attributed under per-project shadowing — `surfaces.md`): static cost, load mode, usage. `--scope` filters to one layer; orphaned usage (no scope to filter on) appears in the unfiltered view only. |
| `usage` | Skill event rollups: per skill, or per time bucket (`--by`) — frequency, tokens, `ctx_growth`, duration. Leads with a token-destination line (main-thread skill output vs subagent total) so the reader sees where tokens actually go before the table. |
| `waste` | Just the flagged opportunities (unused, costly+rare, always-on heavy, …) with their evidence and owning scope; `--scope` filters to one layer. |
| `overhead` | Reconcile the empirical always-on floor (min observed `ctx_start`) against the readable always-on config; the residual is the system prompt + built-in tools + MCP schemas the catalog cannot weigh (`surfaces.md`). Includes a per-project floor table (confounded by session depth — read the global figure as authoritative). |
| `prompts` | How the user steers the session: the mix of steer / correct / question / instruct prompts (`core::prompt`, lexical heuristics), with a verdict — heavy steering suggests more autonomy, frequent correction suggests clearer upfront specs. This is a behavioral signal, not a config metric; embeddings showed prompt *topics* do not map to reusable skills, so the value is in *how* you prompt, not *what about*. |
| `failures` | Where the work stumbles: recurring tool failures by category (`core::friction` — edit-precondition, path-not-found, blocked-by-hook, …), ranked, each with what it suggests fixing and the originating-tool split. This analyses the *work*, not the config — where the real cost is. The classifier separates fixable friction from non-actionable noise (cancelled, transient). `--scope` follows the routing above: `global` = spread categories, `project` = each project's majority-owned categories, `project:<slug>` = everything in that one project (this subsumes the old `--project` flag). Lexical heuristics. |
| `stuck` | Bursts of rapid re-edits to one file (`core::thrash` — N+ edits within a few minutes), ranked. This isolates *where Claude got stuck and kept retrying* from healthy spread-out editing — an algorithmic signal a flat edit count cannot give. Observed: a file edited 25× in under 8 minutes. |

### Output formats — human vs machine

Every command has both a human form and a machine form, strictly separated:

- **Human** (default `table`, plus `--format markdown` for pasting into
  notes/PRs; `doctor` and `analyze` have their own text layouts instead of
  markdown). The output must explain itself: every table opens with a dimmed
  **framing block** saying what the view shows and how to read it (table
  format only — markdown stays a bare table), headers are words rather than
  abbreviations, and interpretation notes are dimmed footnotes. ANSI styling —
  headings, dimmed context, highlighted counts and staleness warnings — turns
  on only when stdout (stderr for the freshness line) is a terminal,
  `NO_COLOR` is unset, and `TERM` is not `dumb`; pipes and captures always get
  plain bytes. Styling is applied after column padding so it never skews table
  alignment.
- **Machine** (`--format json`, on every command): a typed JSON document alone
  on stdout — SQLite integers/reals/NULLs stay JSON numbers/null
  (`Store::query_json`), `doctor` emits the routed findings structure
  (`core::optimize::Findings`, `projects` filtered by `--scope`), and each view
  emits its rows as named objects. All freshness/refresh chatter goes to
  **stderr** (see above), so `cclens <cmd> --format json | jq` needs no
  filtering. Human-only footnotes (row counts, heuristic caveats) are either
  folded into the JSON (`total`, …) or dropped, never mixed into it.

Slices not covered here — the Bash command mix, the most-edited files, an error
category broken down any way — are a `sql` one-liner; those once had thin
`commands`/`hotspots` views, dropped once `sql` existed.

## Time bucketing

`--by` groups by `year | month | week | day | hour`. Because the store keeps UTC
(`storage.md`), bucketing converts to `--tz` first (**default JST**) so day/hour
buckets mean the user's local day/hour — bucketing UTC would smear a local day
across two buckets.

A span is assigned **whole to its start-time bucket**; its cost is not split
across boundaries. This is correct for coarse buckets (year/month) and for
counting, but **distorts fine buckets** for long spans: a `loop` span lasting
hours (observed ~10,000s) lands entirely in one hour even though its work spanned
many. So at `--by hour` / `--by day`, the report **flags long and trailing spans**
(`events.is_trailing`, and a duration threshold) rather than letting them build a
silent spike. Duration-weighted distribution is a later option; v1 assigns to the
start bucket and is honest about the limitation.

## Reporting honesty

The report never hides an approximation; it labels it (these mirror the metric
caveats in `events.md` / `surfaces.md`):

- `sub_tokens_estimated` rows are marked so estimated subagent cost is not read
  as exact.
- Trailing spans (closed only by session end) are marked; their duration is a
  lower bound.
- Surfaces with unknown `static_tokens` (e.g. `mcp_tool` without an available
  schema) are marked unknown, never reported as zero cost.
- "Unused" always carries the window it was evaluated over, so it is not mistaken
  for "never installed".

Meta-skills are not nested (`events.md`): a driver and the skills it ran are
each their own span, so a child's cost lands on the child and no figure is
silently double-counted.

When **several caveats apply to one row** — e.g. a long `loop` span that is
trailing, holds equally-split subagent tokens, and is bucketed whole into one
hour — the row is marked **low-confidence overall**, not merely tagged three
times. Stacked approximations compound into exactly the kind of time-concentrated
spike a reader over-trusts; one combined flag says "read this number loosely".

## `sql` — query the store directly

```
cclens sql [<query>] [--format table|markdown|json] [--db <path>]
```

`sql` is the one read that never auto-refreshes — it opens the store strictly
read-only, so an ad-hoc query is reproducible and can never mutate anything.
It prints the freshness header (store age) on stderr instead, so a stale read
is at least visible.

The store's own query surface: run an arbitrary read query and print the result.
The query is the argument, or — when omitted — read from **stdin**, so both
`cclens sql "SELECT …"` and `echo "SELECT …" | cclens sql` work (stdin
sidesteps shell quoting for complex queries). This exists because the tool is a
*session-analysis* tool: anyone wanting a slice the fixed views do not cover —
notably the `optimize` session chasing a root cause — should query the analyzed
store, not re-parse the raw transcripts. The store is opened **read-only** so an
ad-hoc query can never mutate the derived data, and an absent db is an error
(run `analyze` first), not a silently-created empty one.

A `tool_errors` view (`storage.md`) names the friction columns that are
otherwise overloaded onto generic event columns (`category`, `excerpt`, `tool`,
`target`, `project`), so a friction query reads cleanly without knowing the
encoding — e.g. `SELECT target, COUNT(*) FROM tool_errors WHERE
category='edit-precondition' GROUP BY target` answers "which files keep failing
to edit", which the error text alone cannot. `SELECT sql FROM sqlite_master`
lists the full schema.

## `optimize` — act on the findings with an interactive `claude` session

```
cclens optimize [--projects <dir>] [--db <path>] [--scope global|project[:<slug>]]
                [--frozen] [--print]
```

The AI-proposal consumer (`architecture.md`): rather than emit static
"recommendations" the tool cannot safely act on, `optimize` analyzes (unless
`--frozen`, the same flag the views use), composes the findings with a prescribed advisor prompt, and
launches `claude` seeded with it — so the user optimizes *interactively*, with an
agent that can read their config and apply edits on agreement. The prompt makes
the session **investigate autonomously to a conclusion**: it pins down each root
cause and proposes a concrete fix, rather than handing the analysis back with a
"which area do you want to start with?". It prioritises work friction over config
size, verifies before recommending removal (an "unused" skill may be invoked by
subagents) by inspecting rather than asking, and pauses for exactly one thing —
approval of the concrete fix-plan before any file is edited. This "drive to a
plan, gate only on applying it" shape is the design; if it changes, update
`INSTRUCTIONS` in `core::optimize` and this paragraph together.

Crucially the briefing is the **complete** analysis, not a headline — every
project's friction breakdown, the full Bash/hotspot/thrash detail, and the actual
unused / always-on-heavy surface lists with token costs — **routed the same way
the views are** (see "Scope"): global sections first, then one section per
project, so the session fixes each finding in the layer that owns it. `--scope`
restricts the briefing to one layer and prepends an explicit scope statement
(`core::optimize::scope_statement`), turning the session into "optimize my
global setup" or "optimize this project" specifically. Each friction category
also carries its **split across the originating tools** (`path-not-found` →
`Bash 33, Read 29, Edit 9, playwright 7`) and a few **concrete example excerpts**
— the actual failing paths/files behind the count (`events.md`: `tool_error`
keeps the tool name and a short error-text excerpt) — so the fix is obvious from
the briefing and the session need not re-mine the transcripts to attribute or
locate the failures. The tool split also separates true file friction from a
browser-automation miss that merely reads as "not found". The briefing is the
*headline*; for any deeper slice (the full failing-path list, a worktree split,
arbitrary groupings) the prompt directs the session to **query the store with
`cclens sql`** rather than re-parse the raw transcripts in Python — the
store is exactly the analysis surface cclens exists to provide. It still
spends its effort on the evidence the store cannot hold (the real CLAUDE.md,
rules, hooks). Embedding the headline plus pointing at the queryable store is
what stops the seeded session from re-deriving what `analyze` already computed.

**The briefing is never passed on argv.** It holds concrete paths and error
excerpts that may be sensitive, and a process argument is world-visible (`ps`).
So the launch writes the briefing to a private (`0600`) temp file and passes
`claude` only a data-free pointer prompt (the prescribed instructions plus "read
this file"); the file is removed the instant the session ends, whatever the
outcome. `--print` is the exception — it writes the full prompt (briefing inline)
to stdout for piping / inspection, where the user has opted into seeing it.

The split mirrors the rest of the tool: prompt composition is a pure transform
(`core::optimize` — `render_briefing` for the Markdown report with empty sections
omitted and long lists capped, `INSTRUCTIONS` for the role, `launch_prompt` for
the argv pointer), and only the temp-file write and process launch are I/O.
