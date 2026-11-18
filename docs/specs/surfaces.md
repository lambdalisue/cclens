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

### Usage-measurable, load-measurable, and catalog-only surfaces

Not every surface kind emits a usage signal, and conflating the two is the
fastest way to a wrong recommendation. Each kind is classified:

| Surface kind | Class | Usage signal |
| --- | --- | --- |
| `skill` | usage-measurable | `skill_invocation` |
| `agent` | usage-measurable | `agent_spawn` |
| `mcp_server` / `mcp_tool` | usage-measurable | `tool_use` |
| `permission` | usage-measurable (partly heuristic) | `permission_prompt` from the denial marker, falling back to denial text — lower confidence |
| `hook` | usage-measurable **where its trigger is countable** (estimated) | `tool_call` (or prompt / session) counts its matcher selects; `hook_runs` traces for what it did |
| `rule` | **load-measurable** (gated) | `context_load`, joined by canonical path — only in sessions that carry load records |
| `claude_md` | **load-measurable** (gated) | `context_load` (always-on; usage is not the question — static cost is, so the load confirms the cost rather than the use) |

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

A **load-measurable** surface sits in between. A load says the file entered the
context, not that the model used it, so a load is never an invocation: these
kinds stay out of **Unused** / **Costly + rare**, whose `uses` count would read
"loaded every session" as heavy use. And the signal is only as old as the
records behind it, so every load metric is measured against the **observed
sessions** — the ones that could have shown a load (below) — never against the
whole store.

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
| **Never-fires rule** | path-conditional rule with **zero** loads across enough observed sessions | `context_load` + observed sessions (below) | narrow the globs, or delete |
| **Effectively always-on** | path-conditional rule loaded in nearly every observed session | `context_load` + observed sessions (below) | drop `paths:` (load once at start), or narrow the globs |
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
  An MCP server (`tool_schema`) whose startup cost was measured from the request
  shape (`config-format.md`) ranks here too, by that measured figure.
- **unknown schema** — an MCP server (`tool_schema`) never seen in a recorded
  request shape: a real every-session cost nothing could weigh; flagged, ranked
  next.
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
to a `hook` only when its trigger is countable (above).

### Rule loads: never-fires and effectively always-on

A `paths:`-conditional rule loads only when a matching file is touched, and each
load is a `context_load` record (`session-format.md`). Re-implementing Claude
Code's glob matching against `tool_use` targets was the earlier plan; the load
record makes it unnecessary and is strictly better evidence — it is the matcher's
own verdict, it catches `Bash` touches a target scan would miss, and it needs no
copy of the glob semantics that could drift from upstream. Unconditional rules
and `CLAUDE.md` load at every start, so their records confirm the always-on cost
but cannot flag anything; both wedges apply to conditional rules only.

Per rule (`core::context_load`) the metrics are: **loads**, **sessions** loaded
in, **tokens paid** (static weight × loads — a load re-pays the whole file),
**last loaded**, and the **observed sessions** they are measured against.

**Observed sessions are the gate.** A session can show a load only if its
Claude Code version writes the records. A session counts as observed when it
carries at least one load of any file (every session of a recording version
loads its always-on files) **and** the newest version that wrote its loads is
at or above the oldest version seen writing an on-demand load
(`core::context_load::recording_floor`). The start-time records shipped a few
versions before the on-demand ones, so a session from that gap shows startup
loads it could not have paired with conditional ones. The gate is a version,
not a date: an older install can keep writing transcripts after a newer one
has (another machine sharing the config root, say), and a time cutoff would
admit those sessions as observed. A session with no readable version is not
observed.

A row's reach follows the same attribution as its loads (`storage.md`
`context_loads`), so its share is one population over one population: a
project rule counts its own project's observed sessions; a global rule counts
every observed session except those of a project that owns a row for the same
file, whose loads land on that project's row instead. Two rows of one scope naming one
file (a symlinked alias) can take a load on one row only, so the others are
reported unmeasured rather than as never firing. A transcript written
before the records existed therefore never contributes "zero loads" — it
contributes nothing — and a rule with too few observed sessions is reported
**unmeasured**, never never-fired.

- **Never-fires rule**: zero loads over at least `MIN_OBSERVED_SESSIONS`
  observed sessions. The glob matched nothing the work touched; narrow it to
  what the rule is really about, or delete the rule. Ranked as declutter —
  a conditional rule costs nothing until it loads — so this is hygiene and
  a sign of a mis-targeted glob, not a token saving.
- **Effectively always-on**: loaded in at least `ALWAYS_ON_SHARE` of its
  observed sessions (with the same minimum). The condition buys nothing: the
  rule is paid in nearly every session anyway, and mid-session, so it is
  re-paid after each compaction. Either drop `paths:` so it loads once at
  start, or narrow the globs if the rule was meant for a corner of the tree.

Both thresholds are tuning constants injected into the core, like the other
wedges' "high cost" / "low usage" figures. Window and scope semantics are the
same as every wedge: the window is the store's history, and a project rule is
that project's finding.

A few wedges depend on signals the tool does not yet fully have:

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
system prompt, built-in tools, and MCP tool schemas. Observed against a real
tree, the readable config was a small fraction (~6%) of a much larger always-on
floor, redirecting "what to slim" from rules toward the tool/MCP surface.
Per-project floors are also shown, each with the session count behind it, since
a project with few sessions may never have caught a fresh start; the **global**
floor is the trustworthy baseline.

### Decomposing the residual from the request shape

Where transcripts record the request shape (`session-format.md`), the residual
stops being one opaque number. `overhead` weighs the components of the **latest
session's first main-loop shape** — the latest, because the question is what the
current setup costs, and the floor is a minimum over history that an old,
leaner setup may have set:

| Component | Measured from |
| --- | --- |
| system prompt | the shape's system blocks |
| built-in tools | eager (non-`defer_loading`) tool definitions whose name is not `mcp__…` |
| MCP eager tools | eager `mcp__<server>__…` definitions, also split per server |
| deferred tool listing | the deferred listing's lines, each tool counted once per session |
| MCP instructions | each server's instructions block, once per session |

The figures use the config estimator (`config-format.md`), so they are estimates
on the same scale as `startup_tokens`, not billing figures. They are reported
beside that session's observed start and the readable config, and what neither
covers — the opening prompt, harness reminders, the estimator's error — is shown
as the remainder rather than folded into a component. The listings arrive as
messages, and MCP servers that connect after the first request add theirs later,
so these components are counted over the whole session, not only before its
first request: they are paid every session either way. Sessions whose transcript
records no shape (older releases), or not the shape of their first request,
contribute nothing to the decomposition, and
`overhead` says so instead of reporting zeros.

### Eager vs deferred tools

Per tool, per session, the shape and listings say whether it was **eager** (its
definition in the first main-loop shape) or **deferred** (announced in the
listing, loadable through `ToolSearch`), and the transcript says whether it was
**loaded** (a `ToolSearch` record names it) and how often it was **called**
(`tool_use`). Aggregated across sessions (`core::tool_load`), that yields two
wedges in opposite directions:

| Wedge | Condition | Suggested move |
| --- | --- | --- |
| **Defer candidate** | an eager MCP tool called in few of the sessions that paid for it | defer it (tool search), so its schema is paid only when loaded |
| **Eager candidate** | a deferred tool loaded in most of the sessions that listed it | load it eagerly: each `ToolSearch` load costs a turn, paid in nearly every session anyway |

Both wedges are limited to MCP tools: a built-in tool's loading is upstream's
choice, not config the user can change, so naming one would be a finding without
a fix. A tool is judged only in the mode it is in now (its latest session):
history from before a switch would otherwise flag it both ways at once. Both wedges need a minimum number of sessions before they
fire, and both carry their evidence (sessions, calls or loads, schema weight) so
the reader can judge the trade: deferring a schema saves its tokens every session
but costs a turn whenever it is needed.

## Time and trend

Because events carry a timestamp, every wedge can be evaluated over a window
(`cli.md` bucketing). "Unused" means unused *in the window*; a surface used
heavily last quarter but never this month is a different signal from one never
used at all. The catalog is current (read now); usage is historical — the report
states the window so "unused" is never mistaken for "never installed".

### Before / after a config change

Many rules are written in answer to a recurring failure. Whether one worked is a
question the friction spine can answer, as long as the surface carries **when it
last changed**: compare the rate of each friction category (`core::friction`) in
the sessions before that date with the sessions after it. This is the only
usage-like signal a catalog-only surface (a rule, a `CLAUDE.md`) can get — it has
no invocation events, but its *effect on failures* is observable. Implemented in
`core::effect`; shown by the `effect` view (`cli.md`).

**The boundary is the surface's last change**, not its introduction: the
question is whether the text now installed improved on what came before it, and
a rule reworked last week was not in force in its current form before then. Both
dates are carried, so a later cosmetic edit that moved the boundary stays
visible. Where the dates come from is adapter I/O (`config-format.md`
"Change dates"); the core receives `(surface, changed, introduced, provenance,
targets, always-on)` and the per-session samples, and reads no clock — `now` is
injected.

**The unit is a session, assigned whole by its start time.** Config is read at
session start, so a session that began before the change ran without it even if
it lasted past the boundary; splitting it by event time would credit the rule
with failures it never saw. A project-scoped surface is compared over its own
project's sessions; a global one over all sessions.

**Only an always-on surface can be flagged.** A `paths:` rule reaches only the
sessions that touched its files, and a skill only those that invoked it; the
sessions it never reached sit in both windows and pull its rate toward the
background, so "it did not help" is not something the data shows. Such a
surface still gets its rates and verdict, labelled as diluted, but never a
flag. Restricting it to the sessions that actually loaded it needs per-session
load evidence (rule-load and skill-invocation events), which is the refinement
this leaves open.

**The windows are equal and adjacent**: `[b − L, b)` and `[b, b + L)`, where `L`
is the requested window (default 28 days) shortened to what both sides can
actually supply — no later than now, no earlier than the first stored session.
Unequal windows would turn a change in how much the user worked into a change in
the rate. A change made today therefore has an after-window of hours, and says so.

**Two rates, with their counts.** Each category reports its raw counts beside
`per session` and `per 100 tool calls` (main-thread calls, the same thread the
failures are read from — `storage.md`). Sessions vary in length by orders of
magnitude, so the per-call rate is the one the verdict uses whenever every
session in both windows has a recorded call count; a store holding sessions
analysed before counts were recorded falls back to per session, and the report
names the basis used.

**The verdict is deliberately weak.** Given `k = before + after` failures and the
two windows' exposures, an unchanged rate would split `k` binomially in
proportion to exposure; the one-sided exact p-value of seeing *this few* after
the change is what separates "lower" from "lower by chance":

| Verdict | Condition |
| --- | --- |
| `too-few` | either window has fewer than the minimum sessions (a tuning constant), or its sessions are known to have made no tool calls — nothing was attempted that could fail |
| `no-signal` | the category occurred in neither window |
| `improved` | the after rate is lower and `p < 0.05` |
| `unclear` | the after rate is lower, but not significantly |
| `no-improvement` | the after rate is not lower |

`no-improvement` on a category the surface **declares it targets** is flagged as
a candidate to replace the prose rule with a deterministic hook — a rule that did
not change the behaviour it was written for is costing context for nothing, and
a hook enforces what prose only asks for. The exception is `blocked-by-hook`:
those failures *are* a hook's blocks, so enforcement already exists and "replace
it with a hook" would be circular — there the flag says instead that the hook
blocks the behaviour but the rule is not preventing the attempts (steer from the
hook's own message, or drop the rule). A rise there can also mean the hook was
tightened in the same change, which is exactly the observed case of a sed/awk
rule reworked together with its hook. Neither flag fires on an undeclared
category: without a declaration every category is shown around the date and the
reader judges which one the rule was about.

**Mapping a surface to its category is declared, not guessed.** A keyword
heuristic over rule text would misattribute (a rule about `sed` mentions "hook"
because a hook enforces it), and the misattribution would land as a confident
verdict. So a surface states its targets in its own frontmatter
(`config-format.md`); without them the report shows every actionable category
that occurred in either window and flags nothing.

**What it does not claim.** Before/after is not causation. Other things change on
the same dates — the model, the Claude Code version, the kind of work, and other
config edits made in the same commit. The report carries what it can measure of
that: the session count of each window (a volume shift), and the other surfaces
whose change date falls inside the compared span, named, because a commit that
changed three rules at once cannot attribute its effect to any one of them.
Model and version shifts are not measured and the report says so.

## What the core computes vs. what it leaves open

The core computes the join and the wedge classifications above from thresholds
injected as tuning constants (what counts as "high cost", "low usage"). It does
**not** rank surfaces against each other by importance, infer intent, or decide
that a wedge should be acted on — those judgements belong to the human reading
the report, or to the `optimize` AI-proposal session that consumes this same
joined data.
