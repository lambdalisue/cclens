---
name: query
description: Answer ad-hoc questions about the user's Claude Code usage with read-only SQL over the cclens store (cclens sql) — which files keep failing to edit, which tool errors most, usage per month, any slice the fixed views don't cover. Use when the user asks a specific question about their Claude Code sessions, tool errors, or skill usage.
---

# cclens query — ad-hoc SQL over the usage store

cclens extracts every Claude Code session into a local SQLite store; any slice
its fixed views don't cover is one read-only query away. Answer the user's
question by querying the store — never by re-parsing the raw
`~/.claude/projects` transcripts yourself.

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

  and pass `--db "$DB"` to every command below. `cclens sql` errors on an
  absent store — run `cclens analyze --db "$DB"` first in that case (also run
  it when the user wants current data; it is incremental and fast).

## 2. Query

The query is an argument, or stdin (which sidesteps shell quoting):

```sh
cclens sql --db "$DB" "SELECT category, tool, COUNT(*) n FROM tool_errors GROUP BY 1,2 ORDER BY n DESC"
echo "SELECT excerpt FROM tool_errors WHERE category='path-not-found'" | cclens sql --db "$DB"
```

The store is opened read-only, so a query can never mutate it.

Schema crib — confirm with `cclens sql "SELECT sql FROM sqlite_master"` and
sample the data before relying on an encoding:

- `tool_errors` (view): one row per failed tool call —
  `session_id, project, category, excerpt, tool, target, started_epoch`.
  `category` is the friction class (`edit-precondition`, `path-not-found`,
  `blocked-by-hook`, …), `excerpt` the actual error text (carries the failing
  path), `target` the file/command when the text omits it, `project` the
  session's cwd slug.
- `sessions` and `events` hold everything else (skill spans, tokens, models,
  timestamps); `surfaces` is the config catalog with static token costs.

## 3. Present

Answer the user's actual question, with the query you ran shown so it can be
tweaked. Timestamps in the store are UTC epoch — convert before presenting.
For "how is my setup doing overall", point at `/cclens:summary` instead of
assembling it by hand.
