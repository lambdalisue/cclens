# Events Specification

An **event** is one timestamped occurrence the tool extracts from a transcript,
carrying a runtime cost and pointing at the configuration surface it exercises.
Events are the usage half of the catalog×usage model (`architecture.md`). This
spec defines the event kinds, how a *span* (an event with a duration window) is
bounded, how each cost metric is computed, and how two hard cases — context
compaction and meta-skills that drive other skills — are handled. All of it is
**pure** logic over the domain model the adapter produces; it is the tool's
primary test surface (`.claude/rules/tdd.md`).

## Event kinds

Every event has a `kind` and, where applicable, a `surface_kind` / `surface_id`
identifying the configuration surface it exercises (the join key into
`surfaces.md`).

| `kind` | Surface exercised | Span? |
| --- | --- | --- |
| `skill_invocation` | `skill` | yes — work window until the skill's activity ends |
| `agent_spawn` | `agent` | no — point event; its cost is carried by the run it started (below) |
| `tool_use` | `mcp_tool` / `mcp_server` / built-in tool | point event |
| `prompt` | — (user input; raw material for skill extraction) | point event; text referenced, not stored |
| `tool_error` | — (a failed `tool_result`) | point event; friction category in `surface_id`, a short error-text excerpt in `source`, the originating tool name in `model`, the call's target (file_path / command) in `target` |
| `file_edit` | — (an Edit/Write target) | point event; the file's **basename** in `surface_id`, its **full path** in `target` |
| `bash_cmd` | — (a Bash invocation) | point event; the command's leading word in `surface_id` |
| `compaction` | — (a `compact_boundary`) | point marker, used by the context metric |
| `permission_prompt` | `permission` | point event (friction signal) — partly heuristic source |

New kinds are added additively as the adapter learns to recognise new signals
(`session-format.md`); the schema's loose `kind` / `surface_*` columns
(`storage.md`) absorb them without migration.

Two kinds carry caveats that downstream reports must honour:

- **`prompt`** stores a pointer `(source_path, source_line)` to the raw record,
  not the prompt text — the text is recovered on demand by the future
  skill-extraction layer, keeping personal data out of the store (`storage.md`,
  `.claude/rules/session-data-privacy.md`). This reserves the
  prompt-clustering capability now so it survives transcript rotation.
- **`permission_prompt`** is counted from the denial marker a denying entry
  carries, falling back to denial text where there is none (`session-format.md`).
  The marked half is as structural as any other kind; the fallback half is not,
  so the friction wedge built on it (`surfaces.md`) stays labelled
  lower-confidence — a transcript written before the marker existed is still
  counted by an English phrase alone.
- **`tool_error`** keeps a short, bounded **excerpt** of the error text — unlike
  `prompt`, which stores only a pointer. The asymmetry is deliberate: prompt text
  is large and wholesale-sensitive and is needed in full only by a later layer,
  so a pointer defers the cost; an error excerpt is small and its *value is the
  concrete instance* (the actual failing path), which a report shows directly.
  It lives only in the local store (gitignored, never committed), so the privacy
  rule — no real data in the repo — still holds.

- **`file_edit`** stores the basename *and* the full path because the two answer
  different questions. A hotspot ranking wants the basename — it is the name a
  human recognises, and collapsing every `route.ts` into one row is the point.
  The name is taken on **either separator** (`core::path::basename`): a
  transcript spells paths the way the machine that wrote it does, and the same
  file arrives both ways within one session. Cutting on `/` alone left a
  backslash path standing in for the name and let one file compete against
  itself for a hotspot slot, so the list ranked spellings rather than files.
  Thrash detection wants the opposite: an episode claims *one agent kept retrying
  this one file*, so it must distinguish two same-named files, and two sessions
  editing one file in parallel. Keeping only the basename made those
  indistinguishable and let `stuck` report parallel work as an agent stuck in a
  loop. Detection therefore groups on `(session_id, path)` — the session implies
  the project, and the path is unique within it (`core::thrash`). The deliberate
  cost: thrash continuing across a resume or `/clear` splits into one episode per
  session. A split under-reports a real burst; a merge invents one that never
  happened, and the merge is what makes the report untrustworthy.

Surfaces that emit no event at all — `rule`, `hook`, `claude_md` — never produce
rows here. That is expected, not missing data; `surfaces.md` classifies them as
catalog-only so their absence of events is never read as disuse.

A **span** is an event with a duration window — currently `skill_invocation`.
The rest of this spec is mostly about spans, since that is where boundary and
attribution subtlety lives. Point events carry only an instant and their own
cost.

## Span boundaries

A span starts at its invocation. It ends at the **earliest** of:

1. the next **human turn** in the session;
2. the next **sibling** invocation (an invocation that is not a child of this
   span — see meta-skills below);
3. a record following an **idle gap** longer than `IDLE_GAP` from the previous
   record (active work has ended);
4. the **end of the session**.

Rules 2–4 exist because rule 1 alone is insufficient. The naive definition
(end = next human turn only) let a span run to the end of the session whenever no
human turn followed, sweeping in unrelated tokens — observed: a `doc-check` span
absorbing ~580k output tokens of later, unrelated work. Rules 2 and 3 close the
span at the next real boundary; rule 4 bounds the trailing case. A span closed
only by rule 4 has a `duration_sec` that is a lower bound (the session may have
ended mid-work), so reports flag trailing spans rather than trusting their
wall-clock.

`IDLE_GAP` is a tuning constant injected into the core (not hard-wired), so
tests pin it.

## Meta-skills and nesting — intentionally flat

Some skills drive other skills (`loop` repeatedly invokes `git-commit`,
`code-review`, …). It is tempting to treat those inner calls as **children** of
the driver and nest them under a `parent_id`. We deliberately do **not**, because
the transcript gives no signal to do it correctly.

Empirically, a meta-skill's "children" are recorded exactly like sequential
top-level commands: each appears as its own `slash` invocation with its **own
distinct `promptId`** — not the driver's, not a tool-path call, not a sidechain.
There is no structural marker (`source`, `promptId`, nesting depth) that
distinguishes "a skill `loop` ran" from "a skill the user ran next". Inferring
nesting would require a fragile time-window or a hard-coded list of meta-skills.

So the model stays flat: each invocation is its own span, and the boundary rules
(next human turn / next skill / idle gap) apply uniformly. This is not a loss of
accuracy — a child's cost is correctly attributed to the child (`git-commit`'s
tokens land on `git-commit`); the driver's own small span reflects that it is an
orchestrator, not the place the work happened. Reports therefore never
double-count, and need no parent-inclusive/exclusive distinction.

The one thing that *is* attributable across the boundary is subagent cost, which
carries a real join key — see below.

## Metrics

Each metric is computed from the records strictly within the event/span.

| Metric | Definition |
| --- | --- |
| `out_tokens` | Sum of `output_tokens` over the span's `assistant` records. |
| `ctx_growth` | **Compaction-safe** context consumption: the sum of *positive* differences in prompt size between consecutive `assistant` records. Decreases (compaction, cache eviction) are clipped to zero. |
| `ctx_start` | Prompt size at the span's first `assistant` record — *where this skill ran*, not where the session began. See "The session-start context is its own event" below. |
| `ctx_peak` | Maximum prompt size across the span's `assistant` records. |
| `duration_sec` | Last minus first record `timestamp` within the span. Zero when fewer than two timestamped records. |
| `sub_tokens` | Tokens from subagents attributed to this span (below). |
| `sub_agent_count` | Number of subagents attributed. |
| `sub_tokens_estimated` | True when any attributed subagent was equally split (below). |
| `model` | Representative model: the first `assistant` record whose model is **not** `<synthetic>`. NULL if none qualifies. |

### Why `ctx_growth`, not max-minus-start

The intuitive "context consumed" is `ctx_peak − ctx_start`, but prompt size is
not monotonic: when a session compacts (observed ~1,000,000 → ~49,000 within one
file, marked by a `compact_boundary` record — `session-format.md`), `ctx_peak`
keeps crediting the pre-compaction peak long after that context was released, and
a span opening just after a peak can show a near-zero or negative figure. Summing
only the positive step-to-step increments counts what the span actually *added*
to the running context and is robust to mid-span compaction. `ctx_start` and
`ctx_peak` are stored alongside so the raw shape stays inspectable and
`ctx_growth` is auditable rather than opaque. The `compact_boundary` markers may
additionally be used to split or annotate a span; at minimum the metric must not
assume monotonic prompt size.

### The session-start context is its own event

A span's `ctx_start` answers "how much context was loaded when this skill ran",
which is a *span* property. The always-on floor (`surfaces.md`) needs a different
question answered — "how much context did this session begin with" — and the two
diverge sharply: a session that only invokes a skill after a long conversation
has a span `ctx_start` in the hundreds of thousands while having started lean.
Minimising span `ctx_start` therefore does not converge on the floor; it converges
on whichever session happened to invoke a skill early.

So the session start is extracted separately, as one `session_start` event per
session carrying the prompt size of the session's **first** `assistant` record
(`core::span::session_start_ctx`). A transcript with no `assistant` record yields
no event — the session contributes no observation rather than a zero that would
sink every minimum it entered.

This remains a *lower bound* per observation: a resumed session's first record
already carries the restored context, so its start overstates a fresh one. Taking
the minimum across a project's sessions is what filters those out, which is why
the session count travels with the floor in reports — one observation cannot be
distinguished from one resumed session.

## Subagent runs

A **run** is one execution of one agent type, extracted from its own transcript
plus the sidecar beside it (`session-format.md`). It is a first-class persisted
row (`storage.md`), not an intermediate, because the session-level total answers
only *how much* subagent work cost — never *whose*. Two agents with similar
spawn counts can differ by an order of magnitude in output, and the spawn count
alone cannot tell them apart; without runs, the largest figure in the report is
the one nothing can be done about.

A run carries the agent type, its own output tokens, its model, its depth in the
spawn tree, and the ids that link it back: its own `tool_use_id`, its parent
run, and the **root** `tool_use_id` its tree started at (`core::subagent`).

- **Every run counts once, at its own cost, whatever its depth.** A nested run
  is the agent that wrote those tokens; folding it into its parent's type would
  bill an agent that only delegated. So the per-agent rows sum exactly to the
  session-level total.
- **A run whose agent type is unknown** (no sidecar) is kept, grouped under
  "unknown". Dropping it would silently break that sum; guessing a type would
  be worse.
- **The root link is what crosses the boundary.** A nested run's own
  `tool_use_id` names a call inside its *parent's* transcript, which no
  main-thread span contains. Walking `parent_agent_id` to the tree's root gives
  the one call the main thread actually made, so a subtree's cost lands on the
  main-thread work that caused it. A broken chain (missing parent, or a cycle in
  malformed input) leaves the run unrooted and unattributed rather than charged
  to an arbitrary spawn.

## Subagent attribution

A span's `sub_tokens` is the cost of the runs it spawned. The join is tried at
two precisions, and falls back only when it must:

1. **By spawn call.** A run rooted at a known `Agent` call is attributed to the
   one span whose window contains that call. Spans do not overlap, so this
   claims at most one span: the attribution is exact and is **never** flagged
   estimated.
2. **By turn.** Used **only when the run carries no call id at all** — an older
   transcript with no sidecar. Then only `promptId` is left, and it identifies
   the turn rather than the spawn: the run is claimed by every span that spawned
   anything in that turn, and when more than one competes they **split its tokens
   equally** and are each marked `sub_tokens_estimated`. A single claimant is not
   flagged: the flag means *this figure was split*, which is what the metrics
   table above defines it as.

The precisions are selected by what the run *knows*, never by whether the exact
join happened to hit. Once a run carries a call id the turn join is never used
for it, in either failing case:

- **Its rooted call sits in no span.** The root already says which call spawned
  it, so no span containing that call means it was spawned outside every span.
- **Its parent chain is broken** (a missing parent, or a cycle in malformed
  input), so it has no root. The call id still shows it was spawned by another
  subagent rather than from the main thread.

Retrying by turn in either case would charge the run to whichever unrelated span
happened to spawn something in the same turn — the exact error the call id
exists to prevent.

A run matching no span (spawned outside any skill, or unrooted) is not
attributed per-span; it still counts in the session-level total, which is exact
and needs no join.

Equal-split is a deliberate approximation — this tool ranks, it does not bill
(`architecture.md`) — and its worst-case error is bounded by the spread of the
competing runs' sizes. The flag, not false precision, is the mitigation, and it
now marks only the runs that genuinely could not be resolved.

### The spawn event carries the run's cost

An `agent_spawn` event and a run describe one subagent from opposite sides: the
event is what the main thread *did*, the run is what it *cost*. They are joined
by the spawn call's id, so the event's `out_tokens` is the run's own output and
its `sub_tokens` the output of everything that run spawned in turn. A spawn
whose run is missing keeps zeros rather than borrowing another's figures.

This is why the spawn stays a point event rather than growing a window: its cost
is measured in the subagent's transcript, not inferred from main-thread records
inside a guessed boundary.

### Per-span attribution is window-bounded, not authoritative

Attribution joins a subagent to the span whose window contains its `Agent`
spawn. Under the flat-span model a span's window can stretch past the skill's
own work (until the next human turn / skill / idle gap), so it can absorb
subagents spawned by *later, same-window* activity. Observed in real data: a
`git-commit` span — `git-commit` spawns no agents — picked up several agents
that ran after the commit in the same turn. There is no transcript marker for
"the skill returned", so this cannot be tightened structurally.

Joining by the spawn call instead of the turn does not fix this: the call is
inside the window either way, so the window is still the thing being trusted.
What it fixes is the *other* half — a span that spawned nothing in that turn no
longer takes a share.

Therefore `events.sub_tokens` is recorded per span (queryable, and exact for
skills that genuinely spawn their own agents) but is **not rolled up into the
per-skill report**, where it would over-count. The authoritative subagent figure
is the **session-level total** (`sessions.sub_tokens`), which is exact — every
subagent run counted once, no window guess involved — and the per-agent split of
that total (`subagent_runs`) is exact for the same reason.

## Determinism

The core takes the session's records plus tuning constants (`IDLE_GAP`) and
returns events with no hidden inputs — no clock reads, no randomness. Identical
records in always produce identical events out, so every rule above is pinned by
a fixture test.
