---
name: summary
description: One-screen health check of the user's Claude Code usage via cclens — where tokens go, always-on context cost, recurring tool friction, unused config. Use when the user asks how their Claude Code setup is doing, where time or tokens are being spent or wasted, or for a cclens summary / health check.
---

# cclens summary — usage health check

Run cclens over the user's Claude Code transcripts and config, then present the
one-screen health check. Everything runs locally and read-only over
`~/.claude`; nothing is sent anywhere.

## 1. Resolve the binary and the store

- **Binary**: use `cclens` if `command -v cclens` finds it. Otherwise run every
  command below through Nix instead: `nix run github:lambdalisue/cclens -- <subcommand …>`.
  If neither `cclens` nor `nix` is available, stop and tell the user how to
  install it (`nix profile install github:lambdalisue/cclens`, or
  `cargo install --git https://github.com/lambdalisue/cclens`).
- **Store**: if `./cclens.db` exists, use it and omit `--db`. Otherwise use the
  per-user store so no file is dropped into the current project:

  ```sh
  DB="${XDG_CACHE_HOME:-$HOME/.cache}/cclens/cclens.db"
  mkdir -p "$(dirname "$DB")"
  ```

  and pass `--db "$DB"` to every command below.

## 2. Refresh and report

```sh
cclens analyze --db "$DB"       # incremental; fast when current
cclens summary --db "$DB"
```

(`summary` has no `--format` flag; the individual views do.)

## 3. Present

Relay the report's findings in the conversation language, keeping its ranking —
it is already ordered by what is most actionable. Preserve the caveats it
prints (estimated tokens, evaluation windows); they exist so numbers are not
over-trusted. Do not pad it with generic advice.

Close by naming the follow-ups: `/cclens:optimize` to investigate and fix the
findings, `/cclens:query` for any specific slice the report does not show.
