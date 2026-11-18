//! Pure analysis core: records → events → aggregates. No I/O, no clock, no SQL.
//! This layer is the tool's primary test surface (see `.claude/rules/tdd.md`).

pub mod bucket;
pub mod cache;
pub mod context_load;
pub mod friction;
pub mod hook;
pub mod metrics;
pub mod optimize;
pub mod path;
pub mod prompt;
pub mod scope;
pub mod span;
pub mod subagent;
pub mod surface;
pub mod thrash;
pub mod tool_load;
pub mod usage;
