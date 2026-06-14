# Session Format Specification

This spec catalogues what the **adapter layer** knows about the Claude Code
**transcript** format — the usage half of the catalog×usage model
(`architecture.md`). Its sibling `config-format.md` covers the config half. Only
the adapter may use the field names below; everything downstream sees the
internal domain model (`.claude/rules/format-isolation.md`). This is one of the
two specs most likely to change when Claude Code ships a new release.

> Observed against a real `~/.claude/projects/` tree (≈460 transcript files)
> on 2026-06-14, Claude Code transcript version `2.1.x`. Counts drift as
> sessions accumulate; the **shapes** are what matter.

## File layout

```
~/.claude/projects/
  <cwd-slug>/
    <sessionId>.jsonl                          ← main session transcript
    <sessionId>/subagents/
      agent-<agentId>.jsonl                    ← one transcript per spawned subagent
```

- `<cwd-slug>` encodes the working directory (path separators → `-`). Worktree
  directories appear as their own slugs (e.g. a `...--wt-feat-x` suffix); the
  store folds these to a parent project — see `storage.md`.
- A **main** transcript and its **subagent** transcripts are separate files.
  This separation is load-bearing: a main transcript's records describe only the
  main thread's context, so main-thread metrics are computed from the main file
  alone, uncontaminated by subagent work.

Each file is JSONL — one JSON object per line, appended over the session's life.

## Records the adapter reads

Records carry a `type`. The adapter takes the listed fields and ignores the
rest; unknown types are skipped.

### `assistant` — the source of all runtime cost

- `timestamp` — ISO-8601, millisecond precision, **UTC** (`Z` suffix, e.g.
  `2026-05-14T22:40:06.133Z`). Parsed to a UTC instant; timezone presentation
  happens later (`cli.md`).
- `message.model` — e.g. `claude-opus-4-7`. May be the sentinel `<synthetic>`
  for locally-generated turns; excluded when choosing an event's representative
  model (`events.md`).
- `message.usage` — token accounting: `input_tokens`,
  `cache_read_input_tokens`, `cache_creation_input_tokens`, `output_tokens`.
  - **Prompt size** at a request (the full context handed to the model) is
    `input_tokens + cache_read_input_tokens + cache_creation_input_tokens`. It
    is **not monotonic**: at the context limit Claude Code compacts and the next
    request's prompt size drops sharply (observed ~1,000,000 → ~49,000). The
    context metric is defined to survive this (`events.md`).
- `message.content[]` — blocks; the adapter inspects `tool_use` blocks for skill
  invocations, subagent spawns, and tool usage (below).

### `user` — human turns and slash invocations

- A **human turn** is a `user` record that is not `isMeta` and whose content is
  real input (not solely a `tool_result`). Human turns delimit event ends
  (`events.md`).
- A **slash-command invocation** appears as `<command-name>/NAME</command-name>`
  in the content.

### `system` — structural signals

`system` records carry a `subtype`. Several are directly useful:

| `subtype` | Use |
| --- | --- |
| `compact_boundary` | Marks a context compaction **structurally** — so the context metric detects compaction from the boundary record, not by guessing from a token drop. |
| `turn_duration` | A measured turn wall-clock, usable for duration cross-checks. |
| `local_command` | A slash command run locally (carries its own `<command-name>`). |
| `away_summary`, `compact_boundary`, `scheduled_task_fire`, … | Other lifecycle markers; read as needed. |

## Usage signals the adapter extracts

The transcript yields **events** of several kinds (see `events.md` for how each
becomes an event). The adapter recognises at least:

| Signal | Where in the transcript |
| --- | --- |
| Skill invocation — `slash` | `<command-name>/NAME</command-name>` in a `user` (or `system` `local_command`) record |
| Skill invocation — `tool` | `assistant` `tool_use` with `name == "Skill"`, `input.skill == NAME` |
| Subagent spawn | `assistant` `tool_use` with `name == "Agent"` |
| Tool use | `assistant` `tool_use` blocks (built-in and MCP tools, by `name`) |
| User prompt | `user` **human turns** (not `last-prompt`, which is a single leaf-pointer to the latest prompt — one per region, not per turn) |
| Permission denial | denial text inside a `tool_result` error block (e.g. `"Permission for this action was denied"` / `<tool_use_error>`). **No structured record exists** — this is a low-confidence heuristic; see note below |
| Compaction | `system` `subtype == compact_boundary` |

These map to configuration surfaces (`surfaces.md`): a `Skill` invocation to a
`skill` surface, an MCP `tool_use` to an `mcp_tool`/`mcp_server` surface, and so
on. New surface kinds are added by recognising new signals here — additively,
without disturbing existing extraction.

**Detection is structural, not substring.** A skill invocation is a real
`tool_use` block (`name == "Skill"`) or a `<command-name>` in an actual
`user`/`local_command` command field — never a `<command-name>` or `Skill`
string appearing anywhere in content. Quoted text (a transcript that discusses
skills, a prompt containing the literal tags) must not be mis-read as an
invocation. The adapter matches on record structure and field position.

**Not every surface emits a usage signal.** `rule`, `hook`, and `claude_md` have
no discrete invocation in the transcript — they are injected context, not
events. They appear in the catalog (`config-format.md`) but acquire no events;
`surfaces.md` classifies surface kinds as usage-measurable vs catalog-only so the
join does not mistake "no event possible" for "unused". The permission-denial
signal above is the one heuristically-extracted (not structured) signal, and is
flagged as lower-confidence wherever it is used.

### Skill invocation has two distinct paths

`slash` (the human typed it) and `tool` (the model invoked it) are **not
duplicates of one event** — empirically a `slash` is not shadowed by a matching
`tool` call. The adapter emits both, tagged with their `source`; the core does
**not** deduplicate them.

## Subagent linkage

Subagent cost is attributed back to the event that spawned the work, but a
subagent's tokens live in a separate file. The structural join is **`promptId`**:

- A subagent transcript's records carry `agentId`, `sessionId`, and `promptId`;
  the file is `agent-<agentId>.jsonl`.
- The same `promptId` appears on the parent session's records for the spawning
  turn.
- There is **no** direct `Agent` `tool_use.id` → `agentId` link, so `promptId`
  is the only reliable join. Several subagents can share one `promptId` (parallel
  spawn), making per-event attribution approximate — see `events.md`.

## Subagents do not invoke skills

Empirically, subagent transcripts contain no `Skill` `tool_use` and no
`<command-name>` — subagents are not given the Skill tool. So every skill
invocation originates in a main transcript; subagent files are read only to
**attribute tokens** to the main-thread event that spawned them. If a future
release gives subagents skills, this assumption is the first thing to revisit.
