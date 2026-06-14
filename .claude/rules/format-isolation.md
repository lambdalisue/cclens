---
paths:
  - "src/**/*.rs"
  - "docs/specs/session-format.md"
  - "docs/specs/config-format.md"
---

# Format Isolation — Keep Claude Code Format Knowledge in the Adapter Layer

The tool reads two kinds of Claude Code input, and **both are upstream formats
it does not own**:

1. **Session transcripts** — the JSONL under `~/.claude/projects/` (record
   types, field names like `message.usage.cache_read_input_tokens`,
   `<command-name>` tags, the `promptId` join, the `subagents/` layout).
2. **Live configuration** — skills (`SKILL.md` + frontmatter), rules (`paths`
   frontmatter), `settings.json` (hooks, permissions, MCP servers), `CLAUDE.md`
   / `AGENTS.md`, memory, and MCP tool schemas.

Both will change between Claude Code releases. Contain that knowledge in one
place so an upstream change touches one layer, not the whole tool.

## The boundary

- **Adapter layer only** parses raw input: it knows the on-disk shapes and
  Claude Code's field/file conventions for *both* transcripts and config, and
  maps them into the tool's own internal domain model (sessions, surfaces,
  events).
- **Core / store / report layers never name a raw Claude Code field or config
  path convention.** They operate on the internal domain model. If you find
  yourself writing `cache_read_input_tokens`, `command-name`, `tool_use`,
  `isSidechain`, `promptId`, `compact_boundary`, a `~/.claude/...` path, or
  parsing skill/rule frontmatter outside the adapter, that code belongs in the
  adapter instead.

## Rules

- Deserialize defensively: take only the fields the tool needs, ignore unknown
  fields, tolerate missing optional ones (forward compatibility). A new upstream
  field or a new config key must not break `analyze`.
- The internal domain model is the contract every other layer codes against.
  Renaming a Claude Code field or moving a config file changes the adapter's
  mapping, not the domain model — keep domain names stable and tool-centric, not
  mirror copies of upstream names.
- When an upstream format changes, the diff should be confined to the adapter
  module and the owning format spec (`session-format.md` for transcripts,
  `config-format.md` for config). If a format change forces edits in
  core/store/report, that leak is the bug — fix the boundary, not the call site.

## Why

This tool's entire input is formats it does not own. Isolating them is what lets
the analysis logic, schema, and reports stay stable across Claude Code releases.
See `docs/specs/architecture.md` for the layer rationale.
