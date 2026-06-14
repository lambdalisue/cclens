---
description: Conventional Commits format and scope convention for this repo
---

# Commit Convention — Conventional Commits

All commits follow [Conventional Commits](https://www.conventionalcommits.org/).

## Format

```
<type>(<scope>): <subject>

<body>

<footer>
```

- `subject`: imperative mood, lower-case start, no trailing period, ≤ 70 chars.
- `body`: focus on **why**, not what — the diff already shows what.
- Breaking changes: `BREAKING CHANGE:` in the footer, or `type!`.

## Allowed types

| type       | When to use                                              |
| ---------- | -------------------------------------------------------- |
| `feat`     | New user- or developer-facing capability                 |
| `fix`      | Bug fix                                                   |
| `refactor` | Internal change that does not alter behavior             |
| `perf`     | Performance improvement                                  |
| `test`     | Test-only changes                                        |
| `docs`     | Docs-only changes (`SPEC.md`, `AGENTS.md`, comments)     |
| `build`    | Build system or dependency changes (`Cargo.toml`)        |
| `ci`       | CI configuration changes                                 |
| `chore`    | Anything else (config, ignore files, housekeeping)       |

## Scope convention

Scope is **derived from the area actually touched**, not chosen from a frozen
list — the module layout is still emerging, so this doc does not pin it down
prematurely. Rules:

- Use the lowercase name of the module or surface the change centers on (for
  example the Cargo module under `src/`, or `spec` for `SPEC.md`).
- A change spanning several areas with one purpose may omit the scope:
  `refactor: ...`.
- Keep scopes stable once they appear — reuse an existing scope rather than
  coining a synonym. When a genuinely new top-level area lands, that is the
  signal to add it here in the same change.

Do not invent a scope for a stage that does not exist yet. If a change does not
fit any existing scope and is not introducing a new area, omit the scope.
