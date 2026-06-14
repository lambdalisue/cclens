---
paths:
  - "src/**/*.rs"
  - "tests/**/*.rs"
---

# TDD — Test-First, t-wada Style

Every behavior change starts as a failing test. Follow the red → green →
refactor loop one behavior at a time; do not write production code without a
test that currently fails for the reason you are about to fix.

## The loop

1. **Red** — write one small test that names the behavior and fails. Run it;
   confirm it fails for the expected reason (not a compile error in unrelated
   code).
2. **Green** — write the minimum code to pass. Resist adding unrequested
   generality.
3. **Refactor** — clean up with the test green. Re-run after each step.

`cargo test` MUST be green before a change is considered done. `cargo fmt` and
`cargo clippy` must also pass.

## Keep the analysis core pure

The value of this tool is in pure transforms, so isolate them from I/O and make
them directly unit-testable:

- `records → events` (event/span extraction), the `surfaces × events` cost×usage
  join, and `events → aggregates` (bucketing / rollups) take owned/borrowed data
  and return data — no filesystem, no DB, no clock reads inside them.
- File walking, JSONL reading, config reading, static-token weighing, SQLite
  writing, and the current time are thin shells around the pure core. Test them
  sparingly; test the core thoroughly.
- Inject anything non-deterministic (now, timezone, tuning constants) as a
  parameter so tests stay deterministic.

## Test shape

- Unit tests for pure functions live in `#[cfg(test)] mod tests` next to the
  code; cross-module / CLI behavior lives under `tests/`.
- Drive event, join, and aggregation logic with **small synthetic fixtures** —
  a handful of transcript records or config snippets that isolate one rule (a
  span that ends at the next human turn, one that ends at the next sibling skill
  call, a `compact_boundary` mid-span, a subagent linked by `promptId`, a
  configured-but-unused surface, a UTC→JST boundary case). Fixtures are synthetic
  by mandate; see `.claude/rules/session-data-privacy.md`.
- One assertion focus per test. The test name states the *what*; the body shows
  the example.

## Anti-patterns

- No production code ahead of a failing test "because we'll need it."
- No tests that assert on a real `~/.claude/projects` file — they are
  non-deterministic and leak private data.
- Don't weaken an assertion to make a flaky test pass; fix the determinism.
