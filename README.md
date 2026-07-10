# cclens

A lens onto your Claude Code usage. `cclens` reads your session transcripts and
live configuration, extracts them into a local SQLite store, and shows **where
time, tokens, and effort are being wasted — and how to fix it**: unused or heavy
config, recurring tool failures, where the work gets stuck, and the cost picture
behind it all.

The design intent lives in [`docs/specs/`](docs/specs/); this README is how to
run it.

## Install / build

With Nix (no toolchain to set up):

```sh
nix run github:lambdalisue/cclens -- summary   # run without installing
nix profile install github:lambdalisue/cclens  # install `cclens` onto PATH
```

From source — dev tools are pinned by the Nix flake, tasks run via `just`:

```sh
nix develop          # enter the dev shell (pinned rust, just, sqlite, …)
just                 # list tasks
just check           # rustfmt --check + clippy (the CI gate)
just test            # cargo test
just build           # release binary at target/release/cclens
```

Plain Cargo also works (`cargo build --release`) if you have a recent stable
Rust; the flake just pins it. CI runs `nix develop -c just check` / `just test`,
and tagged releases (`vX.Y.Z`) build native binaries for Linux/macOS/Windows.

## Claude Code plugin

The repository is also a Claude Code plugin marketplace
([`.claude-plugin/`](.claude-plugin/) + [`plugins/cclens/`](plugins/cclens/)),
so you can drive cclens from inside a Claude Code session:

```
/plugin marketplace add lambdalisue/cclens
/plugin install cclens@cclens
```

| Skill | What it does |
| --- | --- |
| `/cclens:summary` | Analyze and present the one-screen health check in the session. |
| `/cclens:optimize` | The `cclens optimize` advisor, in the **current** session: investigate the findings to root cause and propose concrete config fixes. |
| `/cclens:query` | Answer an ad-hoc usage question with read-only SQL over the store. |

The skills use the `cclens` binary when it is on PATH and fall back to
`nix run github:lambdalisue/cclens` otherwise. Outside this repository they
keep the store at `~/.cache/cclens/cclens.db` instead of dropping a
`cclens.db` into your project.

## Quick start

```sh
# 1. Analyze your transcripts + config into a local SQLite store.
cclens analyze

# 2. Start here — a one-screen health check of the most actionable findings.
cclens summary

# 3. Act on them: hand the findings to an interactive `claude` session that
#    investigates each root cause and proposes concrete fixes.
cclens optimize
```

`analyze` reads your transcripts (`~/.claude/projects`) and live config
(`~/.claude/{skills,rules,agents,mcp.json,CLAUDE.md}`) **read-only** and
incrementally, writing `cclens.db`. Nothing is sent anywhere; the store is a
local file. Every command takes `--db <path>` (default `cclens.db`).

## Commands

### Pipeline

| Command | What it does |
| --- | --- |
| `cclens analyze` | Extract transcripts + config into the store. Run it to refresh. |
| `cclens summary` | One-screen health check across every view — **start here**. |
| `cclens sql '<query>'` | Run an arbitrary read-only SQL query against the store (see below). |
| `cclens optimize` | Hand the findings to an interactive `claude` session to act on. |

### Views

Each is a curated lens that carries logic a raw query cannot (a classification,
an algorithm, a suggestion). Add `--format markdown` to paste into a PR or note.

```sh
cclens wedges                 # ranked optimization opportunities, with a suggested action
cclens surfaces               # every installed surface × its actual usage
cclens baseline               # always-on context per session, reconciled vs your config
cclens usage                  # per-skill usage, most-invoked first
cclens usage --by month       # usage per time bucket (JST: year|month|week|day|hour)
cclens friction               # recurring tool failures by category, ranked, with fixes
cclens friction --project=<slug>   # one project's failures (fix lands in the right config)
cclens prompts                # how you steer the session (steer/correct/question/instruct)
cclens thrash                 # files Claude got stuck re-editing in rapid bursts
```

### `sql` — query the store directly

Anything the curated views don't cover is a SQL query — the store is the whole
point. The query is an argument, or read from **stdin**:

```sh
# Which tool produces each friction category?
cclens sql "SELECT category, tool, COUNT(*) n FROM tool_errors GROUP BY 1,2 ORDER BY n DESC"

# The actual failing paths behind path-not-found (stdin form):
echo "SELECT excerpt FROM tool_errors WHERE category='path-not-found'" | cclens sql

# Discover the schema:
cclens sql "SELECT sql FROM sqlite_master"
```

The `tool_errors` view names the friction columns (`category`, `excerpt`,
`tool`, `project`); the db is opened read-only, so a query can never mutate the
derived store.

## What it covers

- **Every configuration surface** — skills, rules, agents, MCP servers,
  `CLAUDE.md` — catalogued with its static token cost and load mode, joined
  against actual usage to flag unused (delete), always-on heavy (slim), and
  costly+rare (trim).
- **Where the work stumbles** — recurring tool failures by category and
  originating tool (`friction`), files Claude got stuck re-editing (`thrash`),
  each with concrete examples and the project it concentrates in.
- **Where tokens go** — main-thread skill output vs. subagent cost, and the
  always-on context floor reconciled against your readable config (the residual
  is system + tools + MCP you cannot trim from files).
- **How you prompt** — the steer / correct / question / instruct mix.

## Notes

- Counts are usage signals for ranking, **not a billing ledger**; static cost is
  a token estimate, not a measured runtime figure.
- `optimize` launches `claude` (Claude Code) seeded with the findings; the
  briefing is written to a private temp file, never passed on the command line.
- Not yet implemented (see [`docs/specs/`](docs/specs/)): meta-skill (`loop`)
  nesting — a pending design decision on detecting parent/child from the
  transcript.
