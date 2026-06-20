//! Compose the optimization briefing: turn the analysis findings into the
//! prompt that seeds an interactive `claude` session. This is the AI-proposal
//! layer's pure half — it knows nothing about the store or the process launch,
//! only how to render findings into a prompt. Keeping it pure makes both the
//! prescribed instructions and the briefing format directly testable.

use crate::core::friction::ErrorCategory;

/// The findings the briefing renders — the same headline signals the `summary`
/// view shows, as owned data so the renderer stays pure and testable.
#[derive(Debug, Clone, Default)]
pub struct Findings {
    pub main_out: i64,
    pub sub_tokens: i64,
    pub sub_agents: i64,
    /// Empirical always-on floor; 0 means unknown (section omitted).
    pub floor: i64,
    pub config_tokens: i64,
    pub friction: Vec<FrictionLine>,
    /// `cd` as a percentage of Bash calls, if any Bash calls were seen.
    pub cd_pct: Option<i64>,
    pub worst_thrash: Option<ThrashLine>,
    pub unused_count: i64,
    pub always_on_heavy: i64,
    /// Prompt-mix shares, if any prompts were classified.
    pub steer_pct: Option<i64>,
    pub correct_pct: Option<i64>,
}

/// One recurring-failure category, with the project it concentrates in when one
/// owns the clear majority (so the fix lands in the right config).
#[derive(Debug, Clone)]
pub struct FrictionLine {
    pub label: String,
    pub count: i64,
    pub dominant_project: Option<String>,
}

/// The worst thrash burst — a file re-edited many times in a short window.
#[derive(Debug, Clone)]
pub struct ThrashLine {
    pub file: String,
    pub edits: u32,
    pub span_secs: i64,
}

/// The prescribed instructions prepended to every briefing. This is the role
/// and method the seeded `claude` session adopts: an optimization advisor that
/// prioritises work friction over config size, verifies before recommending
/// removal, and changes nothing without the user's agreement.
pub const INSTRUCTIONS: &str = "\
You are acting as a Claude Code optimization advisor. The user ran `ccoptimizer`, \
a tool that analyzed their Claude Code session transcripts and configuration to find \
where time, tokens, and effort are wasted. Its headline findings are below. Work with \
the user interactively to act on them.

Reading the data — caveats:
- Counts are usage signals for ranking, not a billing ledger. Token figures are \
output-token sums and estimates; static config costs are token estimates, not measured \
runtime cost.
- \"Fixable friction\" is recurring tool failures during the actual work. This is usually \
where the real cost is — far more than the size of the config.
- \"Always-on context\" is what every session loads before any work starts. Most of it \
(the system prompt, built-in tools, MCP schemas) cannot be trimmed from files; only the \
\"your config\" portion is yours to slim.

You can run the tool yourself for detail (it is on PATH as `ccoptimizer`):
- `ccoptimizer wedges` — the ranked list of unused / always-on-heavy / costly-but-rare \
surfaces, each with a suggested action.
- `ccoptimizer friction --project=<slug>` — one project's failures, so you fix the right config.
- `ccoptimizer thrash`, `ccoptimizer hotspots`, `ccoptimizer commands`, `ccoptimizer prompts` \
— drill into workflow, churn, and prompting.

How to help:
1. Lead with the 2-3 highest-impact opportunities, in plain language, and ask the user \
which to tackle first.
2. Prioritize fixing work friction over shrinking config — stopping a recurring failure \
(e.g. adding a file map to a project's CLAUDE.md to end path-not-found errors) saves more \
than deleting an unused skill.
3. When friction concentrates in one project, make the fix in that project's config, not globally.
4. Before recommending you delete or disable anything, verify it: an \"unused\" skill may still \
be invoked by subagents, or be a deliberate safety net. Inspect the config or ask first.
5. Propose concrete, minimal edits — which file, what change, and why — and apply them only \
after the user agrees.
6. Be honest about what the data cannot tell you; do not invent a cause for a number.

Do not change anything yet. Begin by summarizing what stands out and asking the user where \
they want to start.";

/// Render the findings as a Markdown briefing, omitting any section with no
/// data so the prompt never carries empty headings.
pub fn render_briefing(f: &Findings) -> String {
    let mut out = String::from("# ccoptimizer analysis\n");

    out.push_str("\n## Where tokens go\n");
    out.push_str(&format!("- Main-thread skill output: {}\n", f.main_out));
    out.push_str(&format!(
        "- Subagents: {} ({} agents)\n",
        f.sub_tokens, f.sub_agents
    ));

    if f.floor > 0 {
        let residual = (f.floor - f.config_tokens).max(0);
        out.push_str("\n## Always-on context per session\n");
        out.push_str(&format!(
            "~{} tokens — your config {}; the remaining {} is system + tools + MCP \
             (not trimmable from files).\n",
            f.floor, f.config_tokens, residual
        ));
    }

    if !f.friction.is_empty() {
        out.push_str("\n## Top fixable friction\n");
        for line in &f.friction {
            let suggestion = ErrorCategory::from_label(&line.label).suggestion();
            let where_ = match &line.dominant_project {
                Some(proj) => format!(" (mostly in `{proj}`)"),
                None => String::new(),
            };
            out.push_str(&format!(
                "- {} × {} — {}{}\n",
                line.count, line.label, suggestion, where_
            ));
        }
    }

    if f.cd_pct.is_some() || f.worst_thrash.is_some() {
        out.push_str("\n## Workflow\n");
        if let Some(pct) = f.cd_pct {
            out.push_str(&format!("- cd is {pct}% of Bash calls\n"));
        }
        if let Some(t) = &f.worst_thrash {
            out.push_str(&format!(
                "- Worst thrash: {} edited {}x within {}m{}s\n",
                t.file,
                t.edits,
                t.span_secs / 60,
                t.span_secs % 60
            ));
        }
    }

    out.push_str("\n## Config\n");
    out.push_str(&format!(
        "- {} unused surface(s), {} always-on heavy (run `ccoptimizer wedges` for the list)\n",
        f.unused_count, f.always_on_heavy
    ));

    if let (Some(steer), Some(correct)) = (f.steer_pct, f.correct_pct) {
        out.push_str("\n## Prompting\n");
        out.push_str(&format!("- {steer}% steering, {correct}% corrections\n"));
    }

    out
}

/// The full prompt that seeds the `claude` session: the prescribed instructions
/// followed by the rendered briefing.
pub fn compose_prompt(f: &Findings) -> String {
    format!("{INSTRUCTIONS}\n\n{}", render_briefing(f))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn findings() -> Findings {
        Findings {
            main_out: 8_000_000,
            sub_tokens: 1_800_000,
            sub_agents: 300,
            floor: 35_000,
            config_tokens: 2_000,
            friction: vec![FrictionLine {
                label: "path-not-found".to_string(),
                count: 92,
                dominant_project: Some("alpha".to_string()),
            }],
            cd_pct: Some(53),
            worst_thrash: Some(ThrashLine {
                file: "SKILL.md".to_string(),
                edits: 25,
                span_secs: 460,
            }),
            unused_count: 20,
            always_on_heavy: 1,
            steer_pct: Some(13),
            correct_pct: Some(6),
        }
    }

    #[test]
    fn compose_prepends_instructions_to_the_briefing() {
        let prompt = compose_prompt(&findings());
        // The advisor role and its safest rule must both reach the session.
        assert!(prompt.contains("Claude Code optimization advisor"));
        assert!(prompt.contains("Do not change anything yet"));
        assert!(prompt.contains("# ccoptimizer analysis"));
    }

    #[test]
    fn friction_line_names_the_dominant_project_when_present() {
        let brief = render_briefing(&findings());
        assert!(brief.contains("92 × path-not-found"));
        assert!(brief.contains("(mostly in `alpha`)"));
    }

    #[test]
    fn friction_line_omits_project_when_not_concentrated() {
        let mut f = findings();
        f.friction[0].dominant_project = None;
        let brief = render_briefing(&f);
        assert!(brief.contains("92 × path-not-found"));
        assert!(!brief.contains("mostly in"));
    }

    #[test]
    fn unknown_floor_omits_the_always_on_section() {
        let mut f = findings();
        f.floor = 0;
        let brief = render_briefing(&f);
        assert!(!brief.contains("Always-on context"));
    }

    #[test]
    fn no_workflow_signals_omits_the_workflow_section() {
        let mut f = findings();
        f.cd_pct = None;
        f.worst_thrash = None;
        let brief = render_briefing(&f);
        assert!(!brief.contains("## Workflow"));
    }
}
