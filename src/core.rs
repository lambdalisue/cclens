//! Pure analysis core: records → events → aggregates. No I/O, no clock, no SQL.
//! This layer is the tool's primary test surface (see `.claude/rules/tdd.md`).

pub mod metrics;
