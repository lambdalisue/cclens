# cclens

A CLI that analyzes Claude Code usage to show **where your Claude Code
configuration can be optimized, and how**. It measures the **cost** (context
tokens, wall-clock time, friction) and **actual usage** of every configuration
surface — skills, rules, hooks, MCP servers and their tool schemas, agents,
`CLAUDE.md` / `AGENTS.md` / memory, permissions — and surfaces the optimization
wedges: high cost with low use (prune), always-on cost used only rarely
(defer/split), redundancy (merge), recurring friction (configure).

Skills are just one surface among many; the tool is configuration-wide.

AI-assisted optimization *proposals* are a planned future layer on top of this
analysis core — the core's job is to measure and visualize, not to advise.

## How it is built

This project is **spec-driven and test-driven**.

- **Spec first.** `docs/specs/` holds the design intent (the *why* and the
  contract), split by concern. Read the owning spec before changing behavior;
  update it in the same change. See `.claude/rules/spec-sync.md`.
- **Test first.** Red → green → refactor, t-wada style. Keep the analysis core
  (records → events → aggregation) pure and I/O-free so it is unit-testable
  against small synthetic fixtures. See `.claude/rules/tdd.md`.
- **Privacy is a hard constraint.** Real transcripts and real config can contain
  personal data and secrets. Test fixtures are synthetic; harvested transcripts,
  captured config, and generated databases are never committed. See
  `.claude/rules/session-data-privacy.md`.

## Architecture

Two stages, decoupled by an intermediate SQLite store:

```
transcripts + config ──analyze──▶ SQLite (sessions · surfaces · events) ──▶ consumers
```

`analyze` reads two kinds of Claude Code input — **session transcripts** (what
was actually used and at what cost) and **live config** (what is installed and
its static cost) — and writes a normalized store. The first consumer is CLI
reporting (tables / Markdown); a later consumer feeds the same store to AI for
optimization proposals, a surface left undesigned until decided. The store
exists so every consumer reads pre-computed data instead of re-parsing the
input. Implementation language is **Rust**; the store is **SQLite**.

The core idea is a join: a **surfaces** catalog (everything configured, with its
static cost) against an **events** spine (everything actually exercised, with its
measured cost). What is configured but absent from events is unused; what is
costly but rarely used is a prune candidate.

Internally the code is layered — an **adapter** that owns all knowledge of the
Claude Code input formats (transcript JSONL *and* config files), a **pure core**
(records → events → aggregates, and the surface/usage joins), a **store**, and
**reporting**. Those input formats are upstream and will change; isolating them
in the adapter keeps the rest stable across Claude Code releases. See
`docs/specs/architecture.md` and `.claude/rules/format-isolation.md`.

Project-local guardrails live in `.claude/rules/`.
