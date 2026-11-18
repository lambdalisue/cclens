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
      agent-<agentId>.meta.json                ← that subagent's sidecar (below)
```

- `<cwd-slug>` encodes the working directory (path separators → `-`). Worktree
  directories appear as their own slugs (e.g. a `...--wt-feat-x` suffix); the
  store folds these to a parent project — see `storage.md`.
- The slug is **lossy** (`-` in a path component is indistinguishable from a
  separator), so the real project root comes from the records themselves: most
  record types carry a `cwd` field with the absolute working directory. A
  session that changes directory stamps later records with the child path, so
  the adapter takes the **first** recorded `cwd` as the session root
  (`session_cwd` in `transcript.rs`) — the directory the session started in,
  which is what the slug encodes. Sessions whose records carry no `cwd` have an
  unknown root; project-config scanning simply skips them (`config-format.md`).
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
- `message.id` — the API response id. One response is written as **several
  lines, one per content block** (`thinking`, `text`, `tool_use`), and every
  line repeats `message.model` and carries a `message.usage`; the id is what
  ties them back to one response. The lines take two shapes: on the main
  thread each line's `output_tokens` is the response's final value repeated;
  in a subagent transcript it is a running total that only grows, the last
  line carrying the whole response. The adapter reconciles both at parse time
  by emitting, per line, only the output not yet reported under that id — the
  whole value on the first line and zero on a repeat, or each step of a
  running total (`adapter::transcript::assistant_records`). That is the
  per-id maximum, spread over the lines that first observed it, so the core
  can sum records and count each response once (`events.md`). Keeping the
  *first* line's value instead would under-count subagents badly, and
  `stop_reason` is no substitute: every main-thread line carries one, and some
  subagent responses carry it on no line. A line with no id reports its value
  as is.
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
| Tool failure | a `tool_result` block flagged `is_error` or wrapping `<tool_use_error>`. Categorised from the entry's `toolDenialKind` where that marker can be attributed to it (below), else from the text (`core::friction`); a short readable excerpt is kept. Linking the result's `tool_use_id` to the assistant `tool_use` block recovers both the **originating tool** and the call's **target** (its `input.file_path` / `command`) — so a report shows which tool produced each failure and which file/command it hit, even when the error text omits the path (e.g. edit-precondition names no file) |
| Permission denial | `toolDenialKind` on the entry that carries the denied `tool_result`, else the denial text inside it (`"Permission for this action was denied"`). Structural where the marker is present; a low-confidence heuristic where it is not — see note below |
| Compaction | `system` `subtype == compact_boundary` |
| Hook run | see "Hook traces" below |

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

**Not every surface emits a usage signal.** `rule` and `claude_md` have no
discrete invocation in the transcript — they are injected context, not events.
They appear in the catalog (`config-format.md`) but acquire no events;
`surfaces.md` classifies surface kinds as usage-measurable vs catalog-only so the
join does not mistake "no event possible" for "unused". A `hook` leaves traces
only when it does something visible (below), which is why its uses are counted
from its trigger instead. The permission-denial signal above is the one partly
heuristic signal, and is flagged as lower-confidence wherever it is used.

### Hook traces

> Observed against a real tree on 2026-11-18, Claude Code `2.1.302`–`2.1.325`.

A hook that runs and prints nothing leaves **no record**. What is written, in
the main transcript and in every subagent transcript alike — hooks run in both
(`extract_hook_activity` in `transcript.rs`):

| Record | Fields read | Outcome |
| --- | --- | --- |
| `attachment`, `attachment.type == hook_non_blocking_error` | `hookEvent`, `hookName`, `command`, `exitCode`, `durationMs` | `error` |
| `attachment`, `attachment.type == hook_success` | same | `ok` |
| `attachment`, `attachment.type == hook_additional_context` | `hookEvent`, `hookName`, `content[]` (no command, no duration) | `context` |
| `system`, `subtype == stop_hook_summary` | `hookInfos[].command`, `hookInfos[].durationMs`, `preventedContinuation` | `ok` per hook; event `Stop`. `preventedContinuation` belongs to the stop, not to a hook: it makes the sole hook's run `blocked`, and with several hooks becomes one extra unnamed `blocked` trace |
| a `user` entry's failed `tool_result` whose text opens `<Event>:<subject> hook error: [<command>]` | the prefix; the result's `tool_use_id` | `blocked` |

`hookName` is `<Event>:<subject>` — the subject being what the matcher was tested
against (the tool name for tool events, the source such as `clear` for
`SessionStart`) — or the bare event name when there is none. A record whose
`hookName` does not begin with its `hookEvent` is not a user hook but Claude
Code's own injection (observed: `tool.call`, which adds a worktree's `AGENTS.md`
after a tool call) and is skipped, as is the internal `callback` command in a
stop summary.

The command is not always the user's text. Claude Code sometimes writes an
anonymous placeholder (`command hook`, `command hook 2`); that is read as *no
command*, so the trace resolves by event and matcher alone (`surfaces.md`). A
successful hook that also injected context writes both a `hook_success` and a
`hook_additional_context` for the one run; they are kept as two traces, since
one is a run and the other its context, and no figure counts traces as fires.

The block prefix is upstream's fixed wording, written ahead of whatever the
hook printed, so reading it is structural even though it sits in text — the
hook's own message after it is never parsed. It appears with
`toolDenialKind == permission-rule` where that marker exists, but is matched on
its own so older transcripts are read the same way.

Each tool call's result is recorded with it (`ok` / `failed` / `denied` /
`pending`), because the post-execution hook events fire on different results
(`surfaces.md`). A call is `denied` — refused before it ran — when its result is
a hook block, its entry carries `toolDenialKind`, or, in a transcript older than
that marker, its text is upstream's fixed refusal wording (the fallback the
permission-denial signal already uses). A user entry with no `tool_result`
block is a human turn by structure alone; a prompt that merely mentions a tool
result in its text is still one.

### Why a denial is read from its marker, not its words

A denied call carries `toolDenialKind` — `permission-rule`, `user-rejected`,
`automode-blocked` — on the entry, one level above the `tool_result` block it
explains. Reading it is not an optimization but the only reliable route: the text
below a hook denial is written by the **user's own hook**, so its wording and its
*language* are arbitrary, and no keyword list can converge on it. The marker is
upstream's fixed vocabulary, so it classifies a denial identically whatever the
hook chose to say. `permission-rule` and `automode-blocked` are blocks the user's
config could relax; `user-rejected` is the user answering no.

The field is optional in both directions: older transcripts predate it, and not
every refusal carries one (a built-in guard may simply return an error). An
unrecognised kind falls through to the text classifier rather than overriding it,
so a kind added upstream degrades to today's behavior instead of erasing it.

The marker is scoped to the **entry**, not to a block, so it is only attributed
when the entry holds exactly one failed `tool_result` — every transcript observed
writes one per entry. Were upstream to batch several, a shared marker could not
say which of them it denied, so it is dropped and the text classifier decides for
all. Both fallbacks follow the same rule: where the marker cannot be attributed
with certainty, it does not get to assert anything.

### Skill invocation has two distinct paths

`slash` (the human typed it) and `tool` (the model invoked it) are **not
duplicates of one event** — empirically a `slash` is not shadowed by a matching
`tool` call. The adapter emits both, tagged with their `source`; the core does
**not** deduplicate them.

## Subagent linkage

Subagent cost is attributed back to the event that spawned the work, but a
subagent's tokens live in a separate file, and its own transcript never names
the **agent type** that ran. Both facts come from the sidecar; `promptId` is the
fallback when there is none.

Each `agent-<agentId>.jsonl` has a sibling `agent-<agentId>.meta.json`:

| Field | Use |
| --- | --- |
| `agentType` | The agent that ran (`Explore`, a custom agent's name) — the only place it appears. Without it a run's cost cannot be attributed to a type at all. |
| `toolUseId` | The `id` of the `Agent` `tool_use` block that spawned this run — an **exact** join to one spawn. |
| `parentAgentId` | For a subagent spawned by a subagent, the parent's `agentId`. Absent at depth 1. |
| `spawnDepth` | 1 for a main-thread spawn, 2+ for a nested one. |
| `model` | The model the spawning call **asked for**, as written (`opus`, `sonnet`, `inherit`) — absent when the call named none. It is not the model the run used: that is the `message.model` of the run's own assistant records. Observed to equal the `Agent` call's `model` input on every matched spawn. |

The transcript side of the same join:

- A subagent transcript's records carry `agentId`, `sessionId`, and `promptId`;
  the file is `agent-<agentId>.jsonl`.
- The same `promptId` appears on the parent session's records for the spawning
  turn. It is inherited by the **whole** subagent tree, so a nested run's
  `promptId` is still the top-level turn's — it identifies the turn, never the
  spawn.
- Several subagents share one `promptId` (parallel spawn, and every nested run),
  so `promptId` alone makes per-event attribution approximate. `toolUseId` does
  not: it names one call. The adapter therefore keeps the `Agent` block's `id`
  on the spawn record and prefers that join, falling back to `promptId` only
  when a sidecar is missing — see `events.md`.

**Nested runs exist only here.** An agent spawned by an agent has no `Agent`
`tool_use` anywhere in the main transcript, so the main thread's spawn count is
not the number of subagent runs; the `subagents/` directory is. Observed on a
real tree: roughly half of all runs were nested, and one agent type ran seven
times more often than the main transcript alone suggested.

> The sidecar was observed on transcript versions `2.1.220`–`2.1.246`, present
> for every subagent transcript in the tree. Earlier releases may not write it,
> so every field is read as optional and an absent sidecar degrades to the
> `promptId` join with an unknown agent type — never to a guessed one.

## Subagents do not invoke skills

Empirically, subagent transcripts contain no `Skill` `tool_use` and no
`<command-name>` — subagents are not given the Skill tool. So every skill
invocation originates in a main transcript; subagent files are read only to
**attribute tokens** to the main-thread event that spawned them. If a future
release gives subagents skills, this assumption is the first thing to revisit.
