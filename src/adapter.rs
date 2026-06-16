//! Adapter layer: the only place that knows Claude Code's on-disk formats.
//! It maps raw transcripts and config into the tool's internal domain model so
//! the rest of the crate never sees a Claude Code field name. See
//! `docs/specs/architecture.md` and `.claude/rules/format-isolation.md`.

pub mod config;
pub mod transcript;
