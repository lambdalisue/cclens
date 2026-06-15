# ccoptimizer

Analyze Claude Code usage to see **where your configuration can be optimized**.
It reads your session transcripts, measures how each skill is actually used —
invocation count, output tokens, context growth, wall-clock — and reports it so
heavy or unused configuration stands out.

The design intent lives in [`docs/specs/`](docs/specs/); this README is just how
to run it.

## Build

```sh
cargo build
```

## Use

```sh
# 1. Analyze your transcripts into a local SQLite store (default ~/.claude/projects)
cargo run -- analyze --db ccoptimizer.db

# 2. Report per-skill usage, most-invoked first
cargo run -- report --db ccoptimizer.db
```

```
skill                       count     out_tok    ctx_grow       sec
-------------------------------------------------------------------
git-commit                     87     1356304     1129590    131324
deal-review                    74     2410182     1327027     16026
pr-review                      46      546725      238026      3568
...
```

`analyze` is read-only over your transcripts and incremental (re-running
re-ingests only changed sessions). Nothing is sent anywhere; the store is a local
file.

## Scope today

This is an early vertical slice. It covers **skill** usage from main-session
transcripts. Not yet implemented (see [`docs/specs/`](docs/specs/) for the full
design):

- Other configuration surfaces (rules, hooks, MCP servers, `CLAUDE.md`,
  permissions) and the **catalog × usage** join that flags unused / costly
  config.
- Subagent cost attribution, meta-skill (`loop`) nesting, and the idle-gap span
  rule — so a meta-skill's cost currently overlaps its children's.
- Time-bucketed reports and the optimization "wedge" views.

Counts are usage signals for ranking, not a billing ledger.
