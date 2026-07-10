---
name: optimize
description: Investigate cclens findings about the user's Claude Code usage to root cause and propose concrete config fixes — recurring tool failures, unused or heavy config, always-on context cost. Use when the user wants to optimize their Claude Code configuration, cut token waste or friction, or act on cclens doctor findings.
---

# cclens optimize — act on the findings in this session

`cclens optimize` normally launches a *new* `claude` session seeded with an
advisor prompt. This skill runs the same analysis but adopts that prompt in the
**current** session instead, so the user stays where they are.

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

## 2. Analyze and fetch the advisor prompt

```sh
cclens analyze --db "$DB"
PROMPT="$(mktemp)"
cclens optimize --frozen --print --db "$DB" > "$PROMPT"
```

If the user asked to optimize their global setup or one specific project, add
`--scope global` or `--scope project:<slug>` to the `optimize` command — the
prompt then pins the work to that config layer.

`--print` writes the full advisor prompt — the prescribed instructions plus the
complete findings briefing — to stdout. It goes through a temp file (`mktemp`
creates it private, `0600`), not your tool-output stream, because the briefing
carries real paths and error excerpts from the user's transcripts.

## 3. Follow it

Read the temp file and **adopt its contents as your instructions for the rest
of the task**: investigate each finding to a root cause yourself (using
`cclens sql` against the store, and the user's actual config files), conclude
with a prioritized fix-plan naming specific file edits, and pause only for the
user's approval of that plan before editing anything.

Delete the temp file (`rm -f "$PROMPT"`) as soon as you have read it. Do not
paste the briefing wholesale back to the user — deliver the conclusions.
