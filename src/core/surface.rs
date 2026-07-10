//! The configuration-surface catalog: the domain model for "what is installed
//! and what it costs" — the other half of the catalog×usage model
//! (`docs/specs/surfaces.md`). The adapter fills these from live config; the
//! join against events lives in the store.

/// Where a surface is installed: the shared `~/.claude`, or one specific
/// project's config. The project scope names its project (the normalized slug)
/// because many projects coexist in one catalog and shadowing is per-project
/// (`docs/specs/surfaces.md`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    Global,
    Project(String),
}

impl Scope {
    pub fn label(&self) -> &'static str {
        match self {
            Scope::Global => "global",
            Scope::Project(_) => "project",
        }
    }

    /// The owning project's normalized slug — empty for the global scope.
    pub fn project(&self) -> &str {
        match self {
            Scope::Global => "",
            Scope::Project(project) => project,
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

    /// Parse a stored load-mode label back into the enum.
    pub fn from_label(label: &str) -> Option<LoadMode> {
        match label {
            "startup_full" => Some(LoadMode::StartupFull),
            "startup_description" => Some(LoadMode::StartupDescription),
            "path_conditional" => Some(LoadMode::PathConditional),
            "on_demand" => Some(LoadMode::OnDemand),
            "tool_schema" => Some(LoadMode::ToolSchema),
            _ => None,
        }
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

/// An optimization opportunity for a surface (`docs/specs/surfaces.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wedge {
    /// Installed, usage-measurable, never invoked — a delete candidate.
    Unused,
    /// Always-on context with a large static cost — slim or make conditional.
    AlwaysOnHeavy,
    /// Costly but rarely used — trim or make on-demand.
    CostlyRare,
}

impl Wedge {
    pub fn label(self) -> &'static str {
        match self {
            Wedge::Unused => "UNUSED",
            Wedge::AlwaysOnHeavy => "ALWAYS-ON HEAVY",
            Wedge::CostlyRare => "COSTLY+RARE",
        }
    }

    pub fn suggestion(self) -> &'static str {
        match self {
            Wedge::Unused => "delete / disable",
            Wedge::AlwaysOnHeavy => "slim, or make path-conditional / on-demand",
            Wedge::CostlyRare => "trim, or make on-demand",
        }
    }

    /// Ranking priority (lower sorts first).
    pub fn priority(self) -> u8 {
        match self {
            Wedge::AlwaysOnHeavy => 0,
            Wedge::Unused => 1,
            Wedge::CostlyRare => 2,
        }
    }
}

/// What removing a surface actually saves from the context loaded at session
/// start. The distinction matters because skills and agents load only their
/// description at startup (their body is on-demand), so deleting an unused one
/// is decluttering — not a token win — whereas always-on config and tool schemas
/// are paid every session. See `docs/specs/surfaces.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartupSavings {
    /// Real, measured: removing it saves this many tokens every session.
    Tokens(u64),
    /// Real but unmeasured: an MCP tool schema loaded every session, weight unknown.
    UnknownSchema,
    /// ~No startup cost (on-demand / path-conditional body) — removal is
    /// decluttering only.
    Declutter,
}

/// The startup token saving from removing a surface, by load mode.
pub fn startup_savings(load_mode: LoadMode, static_tokens: Option<u64>) -> StartupSavings {
    match load_mode {
        LoadMode::StartupFull => StartupSavings::Tokens(static_tokens.unwrap_or(0)),
        LoadMode::ToolSchema => StartupSavings::UnknownSchema,
        LoadMode::StartupDescription | LoadMode::PathConditional | LoadMode::OnDemand => {
            StartupSavings::Declutter
        }
    }
}

/// Classify a surface into its optimization wedge, if any. `uses` is the
/// invocation count (meaningful only for `measurable` kinds); `heavy_tokens`
/// is the injected threshold for "large" static cost. The "unused" verdict is
/// gated on measurability so a rule or CLAUDE.md is never called unused for
/// emitting no events (`docs/specs/surfaces.md`).
pub fn classify_wedge(
    measurable: bool,
    load_mode: LoadMode,
    static_tokens: Option<u64>,
    uses: i64,
    heavy_tokens: u64,
) -> Option<Wedge> {
    let heavy = static_tokens.is_some_and(|tokens| tokens >= heavy_tokens);
    if load_mode.is_always_on() && heavy {
        return Some(Wedge::AlwaysOnHeavy);
    }
    if measurable && uses == 0 {
        return Some(Wedge::Unused);
    }
    if measurable && (1..=2).contains(&uses) && heavy {
        return Some(Wedge::CostlyRare);
    }
    None
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

    #[test]
    fn an_unused_measurable_surface_is_a_delete_wedge() {
        let wedge = classify_wedge(true, LoadMode::StartupDescription, Some(500), 0, 1000);
        assert_eq!(wedge, Some(Wedge::Unused));
    }

    #[test]
    fn a_catalog_only_surface_with_no_uses_is_not_unused() {
        // A rule emits no events; zero uses must not read as unused.
        let wedge = classify_wedge(false, LoadMode::PathConditional, Some(500), 0, 1000);
        assert_eq!(wedge, None);
    }

    #[test]
    fn a_big_always_on_surface_is_always_on_heavy() {
        let wedge = classify_wedge(false, LoadMode::StartupFull, Some(1500), 0, 1000);
        assert_eq!(wedge, Some(Wedge::AlwaysOnHeavy));
    }

    #[test]
    fn a_small_always_on_surface_is_not_flagged() {
        let wedge = classify_wedge(false, LoadMode::StartupFull, Some(200), 0, 1000);
        assert_eq!(wedge, None);
    }

    #[test]
    fn a_costly_rarely_used_surface_is_costly_rare() {
        let wedge = classify_wedge(true, LoadMode::StartupDescription, Some(2000), 1, 1000);
        assert_eq!(wedge, Some(Wedge::CostlyRare));
    }

    #[test]
    fn a_well_used_surface_has_no_wedge() {
        let wedge = classify_wedge(true, LoadMode::StartupDescription, Some(2000), 50, 1000);
        assert_eq!(wedge, None);
    }

    #[test]
    fn startup_savings_reflects_what_is_actually_paid_at_startup() {
        // An always-on rule: removing it saves real tokens every session.
        assert_eq!(
            startup_savings(LoadMode::StartupFull, Some(900)),
            StartupSavings::Tokens(900)
        );
        // An MCP server: real but unmeasured.
        assert_eq!(
            startup_savings(LoadMode::ToolSchema, None),
            StartupSavings::UnknownSchema
        );
        // A skill: body is on-demand, so removal is decluttering, not a token win.
        assert_eq!(
            startup_savings(LoadMode::StartupDescription, Some(2000)),
            StartupSavings::Declutter
        );
    }
}
