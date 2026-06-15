//! ccoptimizer — analyze Claude Code usage to find configuration optimizations.
//!
//! The crate is layered (see `docs/specs/architecture.md`): an `adapter` owns the
//! upstream input formats, a pure `core` derives events and aggregates, a `store`
//! persists them, and `report` renders. Modules are added as their contracts gain
//! tests — nothing is scaffolded ahead of a failing test.

pub mod adapter;
pub mod core;
pub mod store;
