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
# 1. Analyze transcripts + config into a local SQLite store
cargo run -- analyze --db ccoptimizer.db

# 2. Start here: one-screen health check — the few most actionable findings
cargo run -- summary --db ccoptimizer.db

# Act on the findings interactively — hands them to a `claude` session
cargo run -- optimize --db ccoptimizer.db
cargo run -- optimize --print --db ccoptimizer.db   # just print the prompt

# 3. See where to optimize — ranked opportunities with a suggested action
cargo run -- wedges --db ccoptimizer.db

# 4. Other views
cargo run -- surfaces --db ccoptimizer.db          # every installed surface × its usage
cargo run -- report   --db ccoptimizer.db          # per-skill usage, most-invoked first
cargo run -- report --by month --db ccoptimizer.db # usage per time bucket (JST)
cargo run -- friction --db ccoptimizer.db          # where the work stumbles, ranked
cargo run -- thrash   --db ccoptimizer.db          # files Claude got stuck re-editing

# any view takes --format markdown to paste into a PR or note
cargo run -- wedges --format markdown --db ccoptimizer.db
```

```
wedge            surface                        static  uses  suggestion
------------------------------------------------------------------------
ALWAYS-ON HEAVY  rule/git/safety                   922     0  slim, or make path-conditional / on-demand
UNUSED           skill/code-review                1345     0  delete / disable
UNUSED           skill/style-review              1055     0  delete / disable
...
```

`analyze` reads your transcripts (`~/.claude/projects`) and live config
(`~/.claude/{skills,rules,agents,mcp.json,CLAUDE.md}`) read-only and
incrementally. Nothing is sent anywhere; the store is a local file.

## What it covers

- **Every configuration surface**: skills, rules, agents, MCP servers,
  `CLAUDE.md` — each catalogued with its static token cost and load mode.
- **The catalog × usage join**: what is installed vs. what is actually used,
  flagging unused (delete), always-on heavy (slim), and costly+rare (trim).
- **Where tokens actually go**: main-thread skill output vs. subagent cost, and
  the always-on context floor reconciled against your readable config (the
  residual is system + tools + MCP you cannot trim from files).
- **Where the work stumbles**: recurring tool failures by category (`friction`),
  files Claude got stuck re-editing (`thrash`), the `cd`/Bash command mix, and
  how you steer the session (`prompts`).
- **Time-bucketed usage** (`--by year|month|week|day|hour`, JST) and markdown
  output.

Not yet implemented (see [`docs/specs/`](docs/specs/) for the full design):
meta-skill (`loop`) nesting (a design decision on detecting parent/child from
the transcript).

Counts are usage signals for ranking, not a billing ledger; static cost is a
token estimate, not a measured runtime figure.
