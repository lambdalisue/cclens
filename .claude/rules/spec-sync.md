---
paths:
  - "src/**/*.rs"
  - "docs/specs/**/*.md"
---

# Spec Sync — Update `docs/specs/` Together With Code

The specs under `docs/specs/` capture **design intent** (the *why* and the
contract). When code drifts and the spec is not updated in the same change, the
spec turns into misinformation that future you (or a coding agent) will rely on.
Treat the spec as part of the source — not as documentation that "someone will
fix later."

## Code → Spec Map

The mapping is convention-based, by **concern**, not by an enumerated module
list (the module layout is still emerging — do not pin it down here
prematurely). A change to code maps to the spec that owns its concern:

| Concern in code | Owning spec |
| --- | --- |
| Raw transcript parsing (record types, field names, file layout) | `docs/specs/session-format.md` |
| Config parsing (skills, rules, `settings.json`, MCP schemas, `CLAUDE.md`) and static-cost weighing | `docs/specs/config-format.md` |
| Event extraction, metric computation, span/nesting, subagent attribution (the pure core) | `docs/specs/events.md` |
| The configuration-surface catalog and the cost×usage join (optimization wedges) | `docs/specs/surfaces.md` |
| SQLite schema, incremental ingest, idempotency (the store layer) | `docs/specs/storage.md` |
| CLI commands, flags, time bucketing, output formats | `docs/specs/cli.md` |
| Layer boundaries, two-stage flow, resilience to upstream format changes (cross-cutting) | `docs/specs/architecture.md` |

If a change spans concerns, update each owning spec. If you can't find an owning
spec, list `docs/specs/` and pick the closest topic before assuming none fits.

## When the spec MUST change in the same task

A code edit requires a spec edit when it changes anything the spec already
documents:

- **Public surface**: CLI commands/flags, the SQLite schema (table/column names
  and types), public function/type/enum names the spec refers to by name.
- **Analysis semantics**: event/span start-end rules, how a metric is computed
  (`out_tokens`, `ctx_growth`, `duration_sec`, `sub_tokens`, `sub_agent_count`,
  surface `static_tokens`), subagent attribution, surface detection and the
  cost×usage join, time bucketing, timezone.
- **Architecture**: the layer split, what crosses a layer boundary, the
  two-stage flow.
- **Diagrams**: any Mermaid node that names a function, type, table, or file
  you just renamed or removed.

## When the spec does NOT need to change

Spec-clean edits: bug fixes preserving the documented contract, internal
refactors invisible from outside the module, test additions, comment/formatting,
performance work with no behavior or surface change. If unsure, default to
updating — a small unnecessary edit costs far less than stale docs.

## How to update the spec

Edit the relevant existing section in place; do not append a new section at the
end. Lead with **why** — for "what" details, link into the source
(`see {file}.rs`). When a documented name is renamed or removed, grep
`docs/specs/` for the old name before finishing — Mermaid diagrams embed names
as plain strings and slip past `cargo check`.

## Anti-patterns

- No "being updated, see #1234" stubs. Either update the spec or revert the code.
- No TODO comments parked in the spec — spec TODOs become permanent.
- Don't duplicate a doc comment verbatim; the spec adds the context the comment
  cannot (why this design beat alternatives, what it deliberately omits).
