//! The configuration-surface catalog: the domain model for "what is installed
//! and what it costs" — the other half of the catalog×usage model
//! (`docs/specs/surfaces.md`). The adapter fills these from live config; the
//! join against events lives in the store.

/// Where a surface is installed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Global,
    Project,
}

impl Scope {
    pub fn label(self) -> &'static str {
        match self {
            Scope::Global => "global",
            Scope::Project => "project",
        }
    }
}

/// How a surface's text reaches the model's context — the distinction that
/// separates always-on tax from per-use cost (`docs/specs/config-format.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadMode {
    /// Paid every session, unconditionally (CLAUDE.md, memory).
    StartupFull,
    /// Only the description is loaded at startup; the body is on-demand (skills, agents).
    StartupDescription,
    /// Loaded only when a `paths:` glob matches (rules with `paths:`).
    PathConditional,
    /// Paid per use when invoked (skill/agent bodies).
    OnDemand,
    /// A tool definition injected into the system/tool context (MCP, built-ins).
    ToolSchema,
}

impl LoadMode {
    pub fn label(self) -> &'static str {
        match self {
            LoadMode::StartupFull => "startup_full",
            LoadMode::StartupDescription => "startup_description",
            LoadMode::PathConditional => "path_conditional",
            LoadMode::OnDemand => "on_demand",
            LoadMode::ToolSchema => "tool_schema",
        }
    }

    /// Whether this surface is paid on every session start regardless of use —
    /// the load modes the "always-on heavy" wedge cares about.
    pub fn is_always_on(self) -> bool {
        matches!(self, LoadMode::StartupFull)
    }
}

/// One configurable thing, with its static cost and how it loads. `(kind, id)`
/// is the join key into the events spine (`docs/specs/surfaces.md`).
#[derive(Debug, Clone, PartialEq)]
pub struct Surface {
    /// `skill` | `rule` | `mcp_server` | `mcp_tool` | `hook` | `claude_md` | `permission` | `agent`.
    pub kind: String,
    pub id: String,
    pub scope: Scope,
    pub config_path: String,
    /// Token weight of the injected definition; `None` when it cannot be weighed
    /// (e.g. an MCP tool schema with no available source).
    pub static_tokens: Option<u64>,
    pub load_mode: LoadMode,
}

/// Whether a surface kind can ever acquire usage events. Absence of events is
/// only evidence of disuse for usage-measurable kinds; for catalog-only kinds
/// (rules, hooks, CLAUDE.md) it means nothing (`docs/specs/surfaces.md`).
pub fn is_usage_measurable(kind: &str) -> bool {
    matches!(
        kind,
        "skill" | "agent" | "mcp_server" | "mcp_tool" | "permission"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_startup_full_is_always_on() {
        assert!(LoadMode::StartupFull.is_always_on());
        assert!(!LoadMode::StartupDescription.is_always_on());
        assert!(!LoadMode::OnDemand.is_always_on());
    }

    #[test]
    fn rules_and_claude_md_are_catalog_only() {
        assert!(is_usage_measurable("skill"));
        assert!(is_usage_measurable("mcp_tool"));
        assert!(!is_usage_measurable("rule"));
        assert!(!is_usage_measurable("hook"));
        assert!(!is_usage_measurable("claude_md"));
    }
}
