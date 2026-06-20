# CLI Specification

The CLI is the user's surface onto the two stages (`architecture.md`). It is
deliberately small: one command to populate the store, one to read it. This spec
defines the command contract and the reporting model; exact flag spellings live
with the implementation and stay in sync via `.claude/rules/spec-sync.md`.

## `analyze` — populate the store

```
ccoptimizer analyze [--projects <dir>] [--config <dir>] [--db <path>]
```

Reads both Claude Code inputs — session transcripts (default
`~/.claude/projects/`) and live config (default `~/.claude/` plus the relevant
project `.claude/`) — and writes the SQLite store (`storage.md`). Incremental and
idempotent: unchanged transcripts are skipped, changed ones replaced, the surface
catalog rebuilt from current config (`storage.md`). The verb is `analyze`, not
`build` — it analyzes raw input into facts.

`analyze` reads everything **read-only** and never copies input into the repo or
the store beyond the derived facts (`.claude/rules/session-data-privacy.md`).

## `report` — read the store

```
ccoptimizer report [<view>] [--by <bucket>] [--since <t>] [--until <t>]
                   [--tz <zone>] [--kind <surface-kind>] [--project <name>]
                   [--format table|markdown] [--db <path>]
```

`report` only queries the store; it never touches raw input. Views answer the
tool's core questions:

| View | Answers |
| --- | --- |
| `summary` | The entry point: a one-screen health check that pulls the few most actionable findings from every view (token destinations, always-on cost, top fixable friction — annotated with the project a category concentrates in — cd overhead + worst thrash, unused config, prompting) into one prioritised report — so the tool answers "what should I do" without running ten commands. |
| `surfaces` (default) | The catalog×usage join per surface: static cost, load mode, usage, cost — the optimization wedges (`surfaces.md`), ranked. |
| `usage` | Event rollups: per surface and/or per time bucket — frequency, tokens, `ctx_growth`, duration. The default per-skill view leads with a token-destination line (main-thread skill output vs subagent total) so the reader sees where tokens actually go before the table. |
| `wedges` | Just the flagged opportunities (unused, costly+rare, always-on heavy, …) with their evidence. |
| `baseline` | Reconcile the empirical always-on floor (min observed `ctx_start`) against the readable always-on config; the residual is the system prompt + built-in tools + MCP schemas the catalog cannot weigh (`surfaces.md`). Includes a per-project floor table (confounded by session depth — read the global figure as authoritative). |
| `prompts` | How the user steers the session: the mix of steer / correct / question / instruct prompts (`core::prompt`, lexical heuristics), with a verdict — heavy steering suggests more autonomy, frequent correction suggests clearer upfront specs. This is a behavioral signal, not a config metric; embeddings showed prompt *topics* do not map to reusable skills, so the value is in *how* you prompt, not *what about*. |
| `friction` | Where the work stumbles: recurring tool failures by category (`core::friction` — edit-precondition, path-not-found, blocked-by-hook, …), ranked, each with what it suggests fixing. This analyses the *work*, not the config — where the real cost is. Recurring failures are fixable friction (e.g. many path-not-found → a file map in CLAUDE.md). The classifier separates fixable friction from non-actionable noise (cancelled, transient). `--project <slug>` restricts the view to one project so the fix lands in the right config — the same per-project breakdown lets `summary` name the project a category concentrates in. Lexical heuristics. |
| `hotspots` | Files Claude edits most (from `Edit`/`Write` targets) — where effort and churn concentrate; a very high count can flag a file it keeps struggling with. |
| `commands` | The Bash command mix and the `cd` overhead. Observed: ~half of all Bash calls were `cd` — a working-dir convention would cut that churn. |
| `thrash` | Bursts of rapid re-edits to one file (`core::thrash` — N+ edits within a few minutes), ranked. Unlike a flat hotspot count, this isolates *where Claude got stuck and kept retrying* from healthy spread-out editing. Observed: a file edited 25× in under 8 minutes. |

Filters (`--kind`, `--project`, `--since/--until`) narrow any view. Output is a
terminal **table** by default or **Markdown** for pasting into notes/PRs.

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

## Not in the CLI (yet)

There is no `recommend` / AI-proposal command. That consumer reads the same store
but its surface is undesigned until decided (`architecture.md`), and is
deliberately absent here rather than stubbed.
