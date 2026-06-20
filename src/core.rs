//! Pure analysis core: records → events → aggregates. No I/O, no clock, no SQL.
//! This layer is the tool's primary test surface (see `.claude/rules/tdd.md`).

pub mod bucket;
pub mod friction;
pub mod metrics;
pub mod prompt;
pub mod span;
pub mod surface;
pub mod thrash;
pub mod usage;
