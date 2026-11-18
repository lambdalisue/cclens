# Surfaces Specification

A **surface** is one configurable thing in a Claude Code setup — a skill, a
rule, an MCP server, a hook, a permission, an `CLAUDE.md`, an agent. This spec
defines the surface catalog and the **catalog × usage join** that is the whole
point of the tool (`architecture.md`): relating *what is installed and what it
costs* (surfaces, from `config-format.md`) to *what was actually used and at what
runtime cost* (events, from `events.md`) to expose optimization wedges.

## The catalog

The adapter reads config into one `surface` per configurable thing, each with:

- a stable `(kind, id)` identity — the join key events also carry;
- two **costs**, because one figure cannot answer both questions: a **static
  cost** (the whole definition — for a skill or agent, what invoking it costs)
  and a **startup cost** (the slice loaded into every session, which is all that
  removing the surface reclaims), plus its **load mode** (startup-full /
  startup-description / path-conditional / on-demand / tool-schema — see
  `config-format.md`). Load mode is what separates always-on tax from per-use
  cost, and which slice of the text each cost covers;
- its scope (global vs project) and `config_path`;
- kind-specific **attributes** (`attrs_json`) — settings that do not change what
  the surface weighs but do change what *using* it costs. An agent carries the
  frontmatter keys that shape its runs (`config-format.md`): the `model` tier it
  runs on, whether it `omitClaudeMd` (its runs start lighter), its
  `autoCompactWindow` and its prompt-cache TTL. They are what explains a cost
  row rather than a cost of their own: an agent whose runs show up `inherited`
  in the spawn model-choice rollup (`events.md`) and whose definition has no
  `model` is the configuration to pin.

The catalog is the denominator of every optimization question: you cannot call a
surface "unused" without knowing it was installed in the first place. A surface
exercised in a transcript but absent from the catalog (e.g. a skill that was
since deleted) is also meaningful — it is reported as **orphaned usage**, the
mirror of unused config.

A surface exists at the global scope, in any number of projects, or both. The
catalog keeps every row (scope + owning project), and the join resolves each
event against the **effective surface for its session's project** — that
project's copy shadows global there, while other projects' events still land on
the global row — so one event never double-counts across scopes and one
project's usage never inflates another's copy (`storage.md` "Surface identity,
scope, and the effective join"). Scope is also the routing key for reports: a
wedge on a project-scoped surface is that project's finding, not a global one
(`cli.md` `--scope`).

### Usage-measurable vs catalog-only surfaces

Not every surface kind emits a usage signal, and conflating the two is the
fastest way to a wrong recommendation. Each kind is classified:

| Surface kind | Class | Usage signal |
| --- | --- | --- |
| `skill` | usage-measurable | `skill_invocation` |
| `agent` | usage-measurable | `agent_spawn` |
| `mcp_server` / `mcp_tool` | usage-measurable | `tool_use` |
| `permission` | usage-measurable (partly heuristic) | `permission_prompt` from the denial marker, falling back to denial text — lower confidence |
| `hook` | usage-measurable **where its trigger is countable** (estimated) | `tool_call` (or prompt / session) counts its matcher selects; `hook_runs` traces for what it did |
| `rule` | **catalog-only** | none (injected context; no invocation event) |
| `claude_md` | **catalog-only** | none (always-on; usage is not the question — static cost is) |

For a **catalog-only** surface, absence of events is *not* evidence of disuse —
no event could ever appear. The join still lists it (with zero usage), but the
wedges below gate on this class so a rule or `CLAUDE.md` is never flagged
"unused → delete" merely for emitting nothing.

### Hooks are measured by their trigger, not their trace

A hook's own trace cannot count how often it ran: Claude Code records a hook
only when it *did* something visible — blocked a call, failed, or injected
context (`hook_runs`, `session-format.md`). A guard that passes silently, which
is what a healthy `PreToolUse` guard does almost every time, leaves nothing.
Counting traces would call the most-exercised guard "unused".

What *is* countable is the trigger. A hook fires on every occurrence of its event
that its matcher selects, so its **uses** are those occurrences in the window
(`core::hook::trigger_count`):

- a tool event — the `tool_call` events whose tool name the matcher selects,
  among the calls that event fires on: every call for `PreToolUse`, the
  successful ones for `PostToolUse`, the ones that ran and failed for
  `PostToolUseFailure` (a call refused before it ran — by a hook, a permission
  rule, or the user — reaches no post-execution hook).
  Calls made inside subagents count too, because hooks run there as well;
- `UserPromptSubmit` — the prompts; `SessionStart` — the sessions.

A matcher is read the way Claude Code reads it: names and `|` only are an exact
list; any other character makes the whole matcher a regex searched anywhere in
the tool name, so `Edit.*` also selects `NotebookEdit`.

Every other event (`Stop`, `Notification`, `PermissionRequest` — which fires only
on calls that raised a prompt, a fact no call records — …), and a matcher too intricate to
evaluate without guessing, leaves the trigger **unknown**: the row is listed but
never judged, exactly like a catalog-only kind. The figure is an estimate
because the catalog is read *now*: a hook added last week is credited with the
matching calls of the whole window. That direction is safe for the one verdict
built on it — zero matching calls means the hook could not have fired whenever
it was added — so **Unused** for a hook reads "its matcher never matched", the
same signal Claude Code's own `/status` reports, but across history.

Triggers are counted only over sessions whose hook activity was extracted. A
session stored by an older analyzer whose transcript Claude Code has since
pruned holds no calls; counting it would turn "not observed" into "never
fired", so it is left out, and a scope with no such session at all leaves every
trigger unknown rather than zero (the store's report-the-gap rule,
`storage.md`).

A hook is also not shadowed like a skill. Global and project hooks all run, so
each row's trigger counts the sessions its own scope covers — every session for
a global hook, that project's sessions for a project hook.

Traces are joined to the catalog in the core, not by id (`core::hook::resolve`):
a trace names its event, the value its matcher was tested against (the tool
name), and — when Claude Code recorded it — the command. It resolves only when
exactly one catalogued hook qualifies: the same event, a matcher that selects
that value, and the same command when one is named. A trace that several hooks
qualify for is **unattributed** — one of the user's hooks left it, but nothing
says which, and splitting it would invent a precision the transcript does not
hold. That covers an unnamed command, and also one command wired under two
matchers that both select the call: Claude Code runs identical handlers once, so
the single trace belongs to neither entry more than the other. A trace no
catalogued hook could have left is **orphaned usage**: a hook injected from
outside the user's settings (a plugin, a wrapper that passes `--settings`) is
real usage the catalog cannot see.

## The join

```mermaid
flowchart LR
    S["surfaces<br/>(kind,id, static_cost, load_mode)"] -->|left join on (kind,id)| J{{join}}
    E["events rollup<br/>(kind,id → count, tokens, ctx, duration, friction)"] -->|right side| J
    J --> W["wedges"]
```

A `LEFT JOIN` from surfaces to an events rollup (grouped by `surface_kind` /
`surface_id`, optionally per time bucket) yields, for every installed surface:
its static cost and load mode, and its realised usage (invocation count, summed
runtime cost, recency, friction). The two halves together are what make a
recommendation possible; neither alone does.

## Optimization wedges

A wedge is a surface-shaped opportunity. The core computes these from the joined
rows; the report ranks them. None is a verdict — the tool surfaces the wedge with
its evidence; acting (or the future AI layer proposing) is downstream.

| Wedge | Condition | Requires | Suggested move |
| --- | --- | --- | --- |
| **Unused** | installed, **zero** events (over the window); for a hook, zero triggering occurrences | usage-measurable kind only (a hook only when its trigger is countable) | delete / disable (a hook: fix or drop the matcher) |
| **Costly + rare** | high static or runtime cost, low usage | usage-measurable kind | trim, or make on-demand |
| **Always-on heavy** | startup-full load mode, large static cost, confirmed against observed context (below) | static cost + observed `ctx_start` | slim, or move to a path-conditional rule / on-demand skill |
| **Never-fires rule** | path-conditional rule whose `paths:` never matched session activity | rule paths × activity file-paths (see below) | narrow or delete |
| **Redundant** | surfaces with overlapping purpose, usage splitting between them | a similarity signal (not in v1 schema — below) | merge |
| **Recurring friction** | repeated permission denials for the same operation | `permission_prompt` (partly heuristic) | add an allow rule |
| **Orphaned usage** | events with no matching surface | any usage event | re-add config, or ignore if intentional |

### Ranking by what removal actually saves

A wedge is only useful if it says how much *removing the surface saves at
session start* — and that is not its static token count. Skills and agents load
only their **description** at startup (their body is on-demand), so deleting an
unused one is **decluttering**, not a token win, even if its body tokenizes
large. Every view that names a surface therefore ranks by `startup_savings` and
reports the two costs **apart** — `startup_tokens` (what removal reclaims) beside
the on-demand body (what invoking it costs), never one figure standing for both.
The ranking (see `core::surface`):

- **measured tokens** — always-on config (`startup_full`): removing it saves that
  many tokens every session. Ranked first, largest first.
- **unknown schema** — an MCP server (`tool_schema`): a real every-session cost
  the catalog cannot weigh; flagged, ranked next.
- **declutter** — on-demand / path-conditional bodies (skills, agents,
  conditional rules): ~no startup cost, so an unused one is namespace hygiene,
  not savings. Ranked last and labelled as such.

This stops a view from headlining an unused skill's body size as if deleting it
freed that context, and lifts the genuinely paid items (heavy always-on rules,
unused MCP servers) to the top. The rule binds the `optimize` briefing as much as
the tables: a briefing line reads `skill/x (~7 tok at startup / 1015 tok when
invoked)`, because the seeded session acts on those figures and a single
unlabelled number there becomes a fix proposal that frees nothing.

The "Requires" column makes the coverage explicit: **Unused** and **Costly +
rare** apply only to usage-measurable kinds — never to `rule` / `claude_md`, and
to a `hook` only when its trigger is countable (above). A few wedges depend on signals the tool does not yet fully have:

- **Never-fires rule** needs evidence a rule's `paths:` glob *could* have fired.
  The only honest source is session activity — did any file matching the glob
  appear as a `tool_use` target (Read/Edit/Write) in the window? That requires
  extracting tool-use file targets, a usage signal **not in v1**; until then this
  wedge is aspirational and labelled so, not computed from "zero events" (a rule
  has no events regardless).
- **Redundant** needs a purpose/similarity signal (e.g. derived from each
  surface's `description`), which the v1 schema does not carry; it lands in
  `attrs_json` or is scoped out of v1 like the AI layer.

**Load mode** is a first-class part of a surface because the same static-token
figure means something very different for a `CLAUDE.md` paid every session than
for a skill body paid only when invoked — but load mode is an assumption, so the
always-on wedge is confirmed empirically, not asserted:

### Always-on reconciliation

`static_tokens` is a config-side estimate, never a measured runtime component
(`config-format.md`). To claim "your always-on config costs ~X every session"
honestly, reconcile it against observed context: the **floor of the session-start
context across sessions** is an empirical lower bound on the always-on context
every session starts with. Comparing `sum(startup_tokens)` against that observed
floor is the only evidence-backed form of the always-on claim. The sum spans
every surface, not just the always-on files: a skill's description is paid at
every session start too, so leaving descriptions out understated the config's
share of the floor by however many skills had accumulated. Surfaces whose startup
weight is unknown (an MCP schema) stay out of the sum rather than entering it as
zero — they land in the unattributed residual, where the report already says the
figure is an upper bound on system + tools + MCP.

The floor must come from an actual session start (the `session_start` event —
`events.md`), never from a skill span's `ctx_start`. Spans were the original
source and are a rejected alternative, not a supported one: a span's `ctx_start`
is the prompt size wherever that skill happened to run, which in a long or
resumed session is far above what the session started with, so a project whose
only skill invocations came late reported a mid-session size as its startup cost
— inverting the advice this section exists to give.

What still bounds the figure is the observation set, not the source: a start is a
lower bound only for a *fresh* session, since a resumed one begins with its
context already restored. The minimum across a project's sessions filters those
out when there are enough of them, so reports carry the session count alongside
the floor, and a project with no observed session start contributes no floor
rather than a substitute.

This is implemented as the `overhead` command (`cli.md`): it reports the observed
floor, the readable config that accounts for part of it, and the **residual** —
the floor minus the config — which is everything not in the user's files: the
system prompt, built-in tools, and **MCP tool schemas** the catalog cannot weigh
(`config-format.md` marks `mcp_tool` static cost unknown). The residual is how
the tool gives an empirical handle on MCP cost despite not reading the schema:
observed against a real tree, the readable config was a small fraction (~6%) of a
much larger always-on floor, redirecting "what to slim" from rules toward the
tool/MCP surface. Per-project floors are also shown, but they are confounded by
how early in a session a skill was first invoked (a project whose skills only ran
deep in long sessions shows an inflated floor), so the **global** floor is the
trustworthy baseline; the per-project view is suggestive, not a clean per-server
measurement.

## Time and trend

Because events carry a timestamp, every wedge can be evaluated over a window
(`cli.md` bucketing). "Unused" means unused *in the window*; a surface used
heavily last quarter but never this month is a different signal from one never
used at all. The catalog is current (read now); usage is historical — the report
states the window so "unused" is never mistaken for "never installed".

## What the core computes vs. what it leaves open

The core computes the join and the wedge classifications above from thresholds
injected as tuning constants (what counts as "high cost", "low usage"). It does
**not** rank surfaces against each other by importance, infer intent, or decide
that a wedge should be acted on — those judgements belong to the human reading
the report, or to the `optimize` AI-proposal session that consumes this same
joined data.
