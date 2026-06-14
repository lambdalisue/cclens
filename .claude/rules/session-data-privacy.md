---
paths:
  - "tests/**"
  - "**/fixtures/**"
  - "src/**/*.rs"
---

# Session Data Privacy — Synthetic Fixtures Only

Real Claude Code transcripts under `~/.claude/projects/` contain personal and
proprietary data: prompts, file contents, paths, project names, tool output.
Real Claude Code config is just as sensitive — `settings.json` can hold API
tokens and secrets, and `CLAUDE.md` / rules can carry private project context.
None of it — transcript or config — may enter the repository.

## Hard rules

- **Test fixtures are synthetic.** Author them by hand to exercise a specific
  rule. Never paste a real transcript — not even "lightly edited." If you need
  a realistic shape, construct a minimal record with invented ids, neutral
  paths (`/tmp/example`), and placeholder text.
- **Never commit harvested data or generated output.** Real `*.jsonl`
  transcripts, copied real config (`settings.json`, `CLAUDE.md`, skills, rules),
  the generated SQLite database, and any scratch export stay out of git.
  `.gitignore` enforces this, but do not defeat it with `git add -f`.
- **No real ids or paths in code or tests.** Session ids, agent ids, prompt
  ids, cwd slugs, and project names in fixtures must be fabricated.
- **Exploration happens against the live directory, read-only.** When verifying
  behavior against real data, read from `~/.claude/projects/` directly at
  runtime; never copy those files into the tree.

## Why

A privacy leak here is irreversible once pushed — transcripts may contain
secrets, customer data, or unpublished work. The cost of fabricating a fixture
is minutes; the cost of leaking one transcript is unbounded.
