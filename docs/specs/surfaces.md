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
- a **static cost**: the token weight of the text it injects, plus its **load
  mode** (startup-full / startup-description / path-conditional / on-demand /
  tool-schema — see `config-format.md`). Load mode is what separates always-on
  tax from per-use cost;
- its scope (global vs project) and `config_path`.

The catalog is the denominator of every optimization question: you cannot call a
surface "unused" without knowing it was installed in the first place. A surface
exercised in a transcript but absent from the catalog (e.g. a skill that was
since deleted) is also meaningful — it is reported as **orphaned usage**, the
mirror of unused config.

A surface exists at one or both scopes (global, project). The catalog keeps both
rows, but the join resolves to the **effective surface** — project shadows global
— so one event never double-counts across scopes (`storage.md` "Surface
identity, scope, and the effective join").

### Usage-measurable vs catalog-only surfaces

Not every surface kind emits a usage signal, and conflating the two is the
fastest way to a wrong recommendation. Each kind is classified:

| Surface kind | Class | Usage signal |
| --- | --- | --- |
| `skill` | usage-measurable | `skill_invocation` |
| `agent` | usage-measurable | `agent_spawn` |
| `mcp_server` / `mcp_tool` | usage-measurable | `tool_use` |
| `permission` | usage-measurable (heuristic) | `permission_prompt` from denial text — lower confidence |
| `rule` | **catalog-only** | none (injected context; no invocation event) |
| `hook` | **catalog-only** | none (no structured transcript trace) |
| `claude_md` | **catalog-only** | none (always-on; usage is not the question — static cost is) |

For a **catalog-only** surface, absence of events is *not* evidence of disuse —
no event could ever appear. The join still lists it (with zero usage), but the
wedges below gate on this class so a rule or hook is never flagged "unused →
delete" merely for emitting nothing.

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
| **Unused** | installed, **zero** events (over the window) | usage-measurable kind only | delete / disable |
| **Costly + rare** | high static or runtime cost, low usage | usage-measurable kind | trim, or make on-demand |
| **Always-on heavy** | startup-full load mode, large static cost, confirmed against observed context (below) | static cost + observed `ctx_start` | slim, or move to a path-conditional rule / on-demand skill |
| **Never-fires rule** | path-conditional rule whose `paths:` never matched session activity | rule paths × activity file-paths (see below) | narrow or delete |
| **Redundant** | surfaces with overlapping purpose, usage splitting between them | a similarity signal (not in v1 schema — below) | merge |
| **Recurring friction** | repeated permission denials for the same operation | `permission_prompt` (heuristic) | add an allow rule |
| **Orphaned usage** | events with no matching surface | any usage event | re-add config, or ignore if intentional |

The "Requires" column makes the coverage explicit: **Unused** and **Costly +
rare** apply only to usage-measurable kinds — never to `rule` / `hook` /
`claude_md`. A few wedges depend on signals the tool does not yet fully have:

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
honestly, reconcile it against observed context: the **floor of `ctx_start`
across a project's sessions** is an empirical lower bound on the always-on
context every session starts with. Comparing `sum(static_tokens WHERE
load_mode = startup_full)` against that observed floor is the only evidence-backed
form of the always-on claim. The report shows both numbers — the config-side
estimate and the observed floor — rather than presenting the estimate as if it
were the measured tax. When they diverge badly, that divergence is itself a
signal (a stale load-mode assumption, or untracked context).

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
the report, or to the future AI proposal layer that consumes this same joined
data.
