//! Compose the optimization briefing: turn the analysis findings into the
//! prompt that seeds an interactive `claude` session. This is the AI-proposal
//! layer's pure half — it knows nothing about the store or the process launch,
//! only how to render findings into a prompt. Keeping it pure makes both the
//! prescribed instructions and the briefing format directly testable.

use crate::core::friction::ErrorCategory;
use crate::core::scope::ScopeFilter;

/// The complete analysis the briefing renders — every view's detail as owned
/// data, so the seeded session has the full picture in hand and need not re-run
/// the tool. Findings are pre-routed to the config layer that owns each fix
/// (`core::scope`): the flat fields are the **global** picture, `projects`
/// holds each project's own findings. The renderer stays pure and testable.
#[derive(Debug, Clone, Default)]
pub struct Findings {
    pub main_out: i64,
    pub sub_tokens: i64,
    pub sub_agents: i64,
    /// Empirical always-on floor; 0 means unknown (section omitted).
    pub floor: i64,
    /// Global always-on config tokens (project config is per-project).
    pub config_tokens: i64,
    /// Friction no single project owns (spread across projects) — a
    /// cross-project habit whose fix belongs in global config.
    pub friction_global: Vec<FrictionCat>,
    /// `cd` as a percentage of Bash calls, if any Bash calls were seen.
    pub cd_pct: Option<i64>,
    /// Top Bash leading words by frequency.
    pub top_commands: Vec<(String, i64)>,
    /// Most-edited files (hotspots) by edit count.
    pub hotspots: Vec<(String, i64)>,
    /// Global-scope surfaces installed but never used in the window.
    pub unused: Vec<SurfaceRef>,
    /// Global-scope always-on surfaces heavy enough to be worth slimming.
    pub always_on_heavy: Vec<SurfaceRef>,
    /// Prompt-mix shares, if any prompts were classified.
    pub steer_pct: Option<i64>,
    pub correct_pct: Option<i64>,
    /// Per-project findings, busiest first.
    pub projects: Vec<ProjectFindings>,
}

/// One project's own findings: the friction it owns (strict-majority routing,
/// `core::scope`), where the work got stuck, and its project-scoped config
/// wedges — everything whose fix belongs in that project's config.
#[derive(Debug, Clone, Default)]
pub struct ProjectFindings {
    pub project: String,
    /// The project's real root directory (empty when no session recorded one)
    /// — far more readable than the folded slug, and what the fix session
    /// actually opens.
    pub root: String,
    pub friction: Vec<FrictionCat>,
    pub thrash: Vec<ThrashLine>,
    pub unused: Vec<SurfaceRef>,
    pub always_on_heavy: Vec<SurfaceRef>,
}

impl ProjectFindings {
    pub fn total_friction(&self) -> i64 {
        self.friction.iter().map(|c| c.count).sum()
    }
}

/// One failure category: its count, how many projects it spreads across (1 for
/// a project-owned category), the split across the tools that produced it (so
/// file friction is told apart from, say, a Playwright locator miss), and a few
/// concrete example excerpts (the actual failing paths/files) — enough that the
/// fix is obvious from the briefing alone.
#[derive(Debug, Clone)]
pub struct FrictionCat {
    pub label: String,
    pub count: i64,
    /// How many projects the count spreads across (1 when project-owned).
    pub projects: usize,
    pub by_tool: Vec<(String, i64)>,
    pub examples: Vec<String>,
}

/// A configuration surface referenced in the config sections.
#[derive(Debug, Clone)]
pub struct SurfaceRef {
    pub kind: String,
    pub id: String,
    pub static_tokens: Option<i64>,
}

/// A thrash burst — a file re-edited many times in a short window.
#[derive(Debug, Clone)]
pub struct ThrashLine {
    pub file: String,
    pub edits: u32,
    pub span_secs: i64,
}

/// The prescribed instructions prepended to every briefing. This is the role
/// and method the seeded `claude` session adopts: an optimization advisor that
/// investigates the findings to a conclusion *on its own* — drilling in with the
/// tool and reading the actual config — rather than handing the analysis back to
/// the user, and pauses only to get the concrete fix-plan approved before editing.
pub const INSTRUCTIONS: &str = "\
You are acting as a Claude Code optimization advisor. The user ran `cclens`, \
a tool that analyzed their Claude Code session transcripts and configuration to find \
where time, tokens, and effort are wasted. Its headline findings are below. Your job is \
to investigate them to a conclusion and propose concrete fixes — not to hand the analysis \
back to the user.

Reading the data — caveats:
- The briefing routes every finding to the config layer that owns the fix: the global \
sections are cross-project signals (fix them in ~/.claude), and each per-project section \
is that project's own (fix it in that project's .claude / CLAUDE.md, never globally).
- Counts are usage signals for ranking, not a billing ledger. Token figures are \
output-token sums and estimates; static config costs are token estimates, not measured \
runtime cost.
- \"Fixable friction\" is recurring tool failures during the actual work. This is usually \
where the real cost is — far more than the size of the config.
- \"Always-on context\" is what every session loads before any work starts. Most of it \
(the system prompt, built-in tools, MCP schemas) cannot be trimmed from files; only the \
\"your config\" portion is yours to slim.

Investigate autonomously — this is your work, not the user's:
- Do NOT ask the user which area to start with, and do NOT ask them to run commands or \
gather data. Drive the investigation yourself, end to end.
- The briefing below is the headline analysis. For ANY deeper slice — the full list of \
failing paths, a worktree-vs-main split, counts grouped however you like — query the \
analyzed store with `cclens sql`. cclens has already extracted every session \
into a SQLite store; querying it is the tool's job, so do NOT re-parse the raw \
~/.claude/projects transcripts in Python — that reinvents what cclens is. Examples:
    cclens sql \"SELECT category, tool, COUNT(*) n FROM tool_errors GROUP BY 1,2 ORDER BY n DESC\"
    cclens sql \"SELECT target, COUNT(*) n FROM tool_errors WHERE category='edit-precondition' AND target<>'' GROUP BY target ORDER BY n DESC\"
    echo \"SELECT excerpt FROM tool_errors WHERE category='path-not-found'\" | cclens sql
  If `cclens` is not on your PATH — e.g. this session was started via \
`nix run github:lambdalisue/cclens` — run it through Nix wherever these instructions say \
`cclens`, with the same subcommands and flags: \
`nix run github:lambdalisue/cclens -- sql --db <path> \"SELECT …\"`.
  Re-running `cclens analyze` is unnecessary — the store is already current. Schema crib:
    - tool_errors(session_id, project, category, excerpt, tool, target, started_epoch): one row \
per failed tool call. `category` = friction class, `excerpt` = the actual error text (carries \
the failing path/file — including any worktree segment like `/.wt/`, so a worktree-vs-main \
split is a `WHERE excerpt LIKE …` away), `tool` = the tool that produced it, `target` = the \
file_path it edited or command it ran (use this when the error text omits the path, e.g. \
edit-precondition's \"File has not been read yet\"). `project` = the session's cwd slug.
    - sessions(id, project, slug, source_path, started_at, …) and events(session_id, kind, \
surface_id, source, model, started_epoch, …) hold everything else (run `cclens sql \
\"SELECT sql FROM sqlite_master\"` to see all of it). Confirm an encoding by sampling the \
data before relying on it.
- The store cannot hold the config itself, so still open the actual CLAUDE.md, rules, hooks, \
skills, and settings to pin down each root cause. Keep going until you can name the cause and \
the fix — never stop at \"this category is high.\"

Reach concrete conclusions:
1. Prioritize fixing work friction over shrinking config — stopping a recurring failure \
(e.g. adding a file map to a project's CLAUDE.md to end path-not-found errors) saves more \
than deleting an unused skill.
2. Respect the briefing's routing: a project section's finding is fixed in that project's \
config, a global section's in ~/.claude — do not promote a one-project fix to global.
3. Before proposing to delete or disable anything, verify it by inspecting — an \"unused\" \
skill may still be invoked by subagents, or be a deliberate safety net. Confirm it yourself; \
do not punt the check to the user.
4. For each top opportunity, conclude with a specific fix: which file, what change, and the \
effect you expect.
5. Be honest about what the data cannot tell you. If a step is genuinely blocked on a \
judgement only the user can make (a preference, a secret, an external fact), name that one \
decision specifically and keep going on everything else — do not hand the whole analysis back.

Deliver a concrete, prioritized action plan with the specific edits you propose, having done \
the investigation yourself. Apply file changes only after the user approves the plan — that \
approval is the one and only thing you pause for, never direction on what to investigate.";

/// Render the findings as a complete Markdown briefing, restricted to the
/// requested scope: the global picture, each project's own findings, or both
/// (the default). Any section with no data is omitted so the prompt never
/// carries an empty heading.
pub fn render_briefing(f: &Findings, filter: &ScopeFilter) -> String {
    let mut out = String::from("# cclens analysis\n");

    if filter.includes_global() {
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
                "~{} tokens — your global config {}; the remaining {} is system + tools + MCP \
                 (not trimmable from files).\n",
                f.floor, f.config_tokens, residual
            ));
        }

        if !f.friction_global.is_empty() {
            out.push_str(
                "\n## Global friction — spread across projects\n\
                 Recurring tool failures no single project owns: a cross-project habit, \
                 so the fix belongs in global config (~/.claude). Counts exclude \
                 non-actionable noise (user stops, infra blips).\n",
            );
            for cat in &f.friction_global {
                render_friction_cat(&mut out, cat, true);
            }
        }

        let has_workflow =
            f.cd_pct.is_some() || !f.top_commands.is_empty() || !f.hotspots.is_empty();
        if has_workflow {
            out.push_str("\n## Workflow\n");
            if let Some(pct) = f.cd_pct {
                out.push_str(&format!("- cd is {pct}% of Bash calls\n"));
            }
            if !f.top_commands.is_empty() {
                out.push_str("\n### Bash command mix (top)\n");
                for (cmd, n) in &f.top_commands {
                    out.push_str(&format!("- {cmd}: {n}\n"));
                }
            }
            if !f.hotspots.is_empty() {
                out.push_str("\n### Most-edited files\n");
                for (file, n) in &f.hotspots {
                    out.push_str(&format!("- {file}: {n}\n"));
                }
            }
        }

        render_config_to_trim(
            &mut out,
            "Global config to trim",
            &f.unused,
            &f.always_on_heavy,
        );

        if let (Some(steer), Some(correct)) = (f.steer_pct, f.correct_pct) {
            out.push_str("\n## Prompting\n");
            out.push_str(&format!("- {steer}% steering, {correct}% corrections\n"));
        }
    }

    for p in &f.projects {
        if !filter.includes_project(&p.project) {
            continue;
        }
        let root = if p.root.is_empty() {
            String::new()
        } else {
            format!(" (root: {})", p.root)
        };
        out.push_str(&format!(
            "\n## Project `{}`{root} — {} failures it owns\n\
             Fix these in this project's own config (.claude/, CLAUDE.md), not globally.\n",
            p.project,
            p.total_friction()
        ));
        for cat in &p.friction {
            render_friction_cat(&mut out, cat, false);
        }
        if !p.thrash.is_empty() {
            out.push_str("\n### Thrash episodes (rapid re-edits to one file)\n");
            for t in &p.thrash {
                out.push_str(&format!(
                    "- {} edited {}x within {}m{}s\n",
                    t.file,
                    t.edits,
                    t.span_secs / 60,
                    t.span_secs % 60
                ));
            }
        }
        render_config_to_trim(
            &mut out,
            "Project config to trim",
            &p.unused,
            &p.always_on_heavy,
        );
    }

    out
}

fn render_friction_cat(out: &mut String, cat: &FrictionCat, show_spread: bool) {
    let suggestion = ErrorCategory::from_label(&cat.label).suggestion();
    let spread = if show_spread {
        format!(" (across {} projects)", cat.projects)
    } else {
        String::new()
    };
    out.push_str(&format!(
        "- {} × {}{spread} — {}\n",
        cat.count, cat.label, suggestion
    ));
    if !cat.by_tool.is_empty() {
        let split = cat
            .by_tool
            .iter()
            .map(|(tool, n)| format!("{tool} {n}"))
            .collect::<Vec<_>>()
            .join(", ");
        out.push_str(&format!("    by tool: {split}\n"));
    }
    for example in &cat.examples {
        out.push_str(&format!("    e.g. {example}\n"));
    }
}

fn render_config_to_trim(
    out: &mut String,
    heading: &str,
    unused: &[SurfaceRef],
    always_on_heavy: &[SurfaceRef],
) {
    if unused.is_empty() && always_on_heavy.is_empty() {
        return;
    }
    out.push_str(&format!("\n## {heading}\n"));
    if !unused.is_empty() {
        out.push_str(&format!(
            "\n### Unused surfaces ({}) — installed but never used in the window\n",
            unused.len()
        ));
        for s in unused {
            out.push_str(&format!("- {}\n", render_surface(s)));
        }
    }
    if !always_on_heavy.is_empty() {
        out.push_str(&format!(
            "\n### Always-on heavy ({}) — loaded every session; slim or make on-demand\n",
            always_on_heavy.len()
        ));
        for s in always_on_heavy {
            out.push_str(&format!("- {}\n", render_surface(s)));
        }
    }
}

fn render_surface(s: &SurfaceRef) -> String {
    match s.static_tokens {
        Some(t) => format!("{}/{} ({} tok)", s.kind, s.id, t),
        None => format!("{}/{} (unknown tok)", s.kind, s.id),
    }
}

/// How to reach the analyzed store for `cclens sql` — appended so the
/// `--db` the agent should query is the one this run actually built.
fn store_pointer(db_path: &str) -> String {
    format!(
        "The analyzed store is at `{db_path}`; pass `--db {db_path}` to `cclens sql` \
         (or run from a directory where it is `cclens.db`)."
    )
}

/// The task statement a `--scope` run prepends: it pins the session to one
/// config layer so the other's findings are context, not work items.
fn scope_statement(filter: &ScopeFilter) -> Option<String> {
    match filter {
        ScopeFilter::All => None,
        ScopeFilter::Global => Some(
            "THIS RUN'S SCOPE: optimize the GLOBAL configuration (~/.claude) only. \
             The briefing is restricted to global findings; every fix you propose \
             must land in global config."
                .to_string(),
        ),
        ScopeFilter::Project(None) => Some(
            "THIS RUN'S SCOPE: optimize project-local configuration only. Every fix \
             you propose must land in a project's own config (.claude/, CLAUDE.md), \
             never in ~/.claude."
                .to_string(),
        ),
        ScopeFilter::Project(Some(project)) => Some(format!(
            "THIS RUN'S SCOPE: optimize project `{project}`'s configuration only. \
             Every fix you propose must land in that project's own config \
             (.claude/, CLAUDE.md), never in ~/.claude."
        )),
    }
}

fn preamble(db_path: &str, filter: &ScopeFilter) -> String {
    match scope_statement(filter) {
        Some(statement) => format!(
            "{INSTRUCTIONS}\n\n{statement}\n\n{}",
            store_pointer(db_path)
        ),
        None => format!("{INSTRUCTIONS}\n\n{}", store_pointer(db_path)),
    }
}

/// The full prompt with the briefing inline — used for `--print` (so the reader
/// sees everything in one stream) and for tests.
pub fn compose_prompt(f: &Findings, db_path: &str, filter: &ScopeFilter) -> String {
    format!(
        "{}\n\n{}",
        preamble(db_path, filter),
        render_briefing(f, filter)
    )
}

/// The argv prompt for launching `claude`: the prescribed instructions, where to
/// query the store, and a pointer to the briefing file. The briefing — which
/// carries concrete paths and error excerpts that may be sensitive — is written
/// to that file rather than passed on argv (where `ps` would expose it); only
/// this generic, data-free prompt reaches the process table.
pub fn launch_prompt(briefing_path: &str, db_path: &str, filter: &ScopeFilter) -> String {
    format!(
        "{}\n\nThe briefing — the complete analysis — is in the file \
         `{briefing_path}`. Read that file in full before doing anything else, then proceed.",
        preamble(db_path, filter)
    )
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
            friction_global: vec![FrictionCat {
                label: "blocked-by-hook".to_string(),
                count: 40,
                projects: 5,
                by_tool: vec![("Bash".to_string(), 40)],
                examples: vec!["blocked by enforce-perl.sh".to_string()],
            }],
            cd_pct: Some(53),
            top_commands: vec![("cd".to_string(), 200), ("cargo".to_string(), 80)],
            hotspots: vec![("lib.rs".to_string(), 40)],
            unused: vec![SurfaceRef {
                kind: "skill".to_string(),
                id: "code-review".to_string(),
                static_tokens: Some(1345),
            }],
            always_on_heavy: vec![SurfaceRef {
                kind: "rule".to_string(),
                id: "git/safety".to_string(),
                static_tokens: Some(922),
            }],
            steer_pct: Some(13),
            correct_pct: Some(6),
            projects: vec![ProjectFindings {
                project: "alpha".to_string(),
                root: "/tmp/example/alpha".to_string(),
                friction: vec![
                    FrictionCat {
                        label: "path-not-found".to_string(),
                        count: 92,
                        projects: 1,
                        by_tool: vec![("Read".to_string(), 60), ("Bash".to_string(), 32)],
                        examples: vec!["File does not exist: src/components/Foo.tsx".to_string()],
                    },
                    FrictionCat {
                        label: "timeout".to_string(),
                        count: 12,
                        projects: 1,
                        by_tool: vec![],
                        examples: vec![],
                    },
                ],
                thrash: vec![ThrashLine {
                    file: "SKILL.md".to_string(),
                    edits: 25,
                    span_secs: 460,
                }],
                unused: vec![SurfaceRef {
                    kind: "skill".to_string(),
                    id: "deploy".to_string(),
                    static_tokens: Some(500),
                }],
                always_on_heavy: vec![],
            }],
        }
    }

    #[test]
    fn compose_prepends_instructions_to_the_briefing() {
        let prompt = compose_prompt(&findings(), "/tmp/cc.db", &ScopeFilter::All);
        // The advisor role, the autonomy mandate (investigate without handing
        // the analysis back), the store pointer, and the briefing must all reach
        // the session.
        assert!(prompt.contains("Claude Code optimization advisor"));
        assert!(prompt.contains("Do NOT ask the user which area"));
        assert!(prompt.contains("Apply file changes only after the user approves"));
        assert!(prompt.contains("cclens sql"));
        assert!(prompt.contains("--db /tmp/cc.db"));
        assert!(prompt.contains("# cclens analysis"));
        // The session may have been launched via `nix run` (cclens not on PATH),
        // so the prompt must document the Nix invocation as a substitution rule.
        assert!(prompt.contains("nix run github:lambdalisue/cclens"));
    }

    #[test]
    fn briefing_routes_friction_to_its_owning_layer() {
        let brief = render_briefing(&findings(), &ScopeFilter::All);
        // The spread category is a global finding with its spread named...
        assert!(brief.contains("## Global friction"));
        assert!(brief.contains("40 × blocked-by-hook (across 5 projects)"));
        // ...and the project-owned categories live under the project's heading,
        // which names the real root the fix session will open.
        assert!(
            brief.contains("## Project `alpha` (root: /tmp/example/alpha) — 104 failures it owns")
        );
        assert!(brief.contains("92 × path-not-found"));
        assert!(brief.contains("12 × timeout"));
    }

    #[test]
    fn a_global_scope_briefing_omits_project_sections() {
        let brief = render_briefing(&findings(), &ScopeFilter::Global);
        assert!(brief.contains("## Global friction"));
        assert!(!brief.contains("## Project `alpha`"));
        assert!(!brief.contains("skill/deploy"));
    }

    #[test]
    fn a_project_scope_briefing_carries_only_that_project() {
        let filter = ScopeFilter::Project(Some("alpha".to_string()));
        let brief = render_briefing(&findings(), &filter);
        assert!(brief.contains("## Project `alpha`"));
        // No global sections — the session is pinned to the project layer.
        assert!(!brief.contains("## Global friction"));
        assert!(!brief.contains("## Prompting"));
    }

    #[test]
    fn a_scoped_prompt_states_the_run_scope() {
        let filter = ScopeFilter::Global;
        let prompt = compose_prompt(&findings(), "/tmp/cc.db", &filter);
        assert!(prompt.contains("THIS RUN'S SCOPE: optimize the GLOBAL configuration"));
        let unscoped = compose_prompt(&findings(), "/tmp/cc.db", &ScopeFilter::All);
        assert!(!unscoped.contains("THIS RUN'S SCOPE"));
    }

    #[test]
    fn launch_prompt_points_at_the_file_and_carries_no_briefing_data() {
        let prompt = launch_prompt(
            "/tmp/cclens-briefing-123.md",
            "/tmp/cc.db",
            &ScopeFilter::All,
        );
        // The instructions, the store pointer, and the file pointer must be present...
        assert!(prompt.contains("Claude Code optimization advisor"));
        assert!(prompt.contains("--db /tmp/cc.db"));
        assert!(prompt.contains("/tmp/cclens-briefing-123.md"));
        assert!(prompt.contains("Read that file in full"));
        assert!(prompt.contains("nix run github:lambdalisue/cclens"));
        // ...but none of the analysis (which goes to the file) leaks onto argv.
        assert!(!prompt.contains("# cclens analysis"));
    }

    #[test]
    fn briefing_shows_concrete_error_examples_under_a_category() {
        let brief = render_briefing(&findings(), &ScopeFilter::All);
        // The actual failing path must appear, so the fix is obvious without
        // re-mining the transcripts.
        assert!(brief.contains("e.g. File does not exist: src/components/Foo.tsx"));
    }

    #[test]
    fn briefing_splits_a_category_by_originating_tool() {
        let brief = render_briefing(&findings(), &ScopeFilter::All);
        // The per-tool attribution the agent otherwise re-derives from the raw
        // transcripts must be present.
        assert!(brief.contains("by tool: Read 60, Bash 32"));
    }

    #[test]
    fn briefing_lists_concrete_config_surfaces_not_just_counts() {
        let brief = render_briefing(&findings(), &ScopeFilter::All);
        assert!(brief.contains("skill/code-review (1345 tok)"));
        assert!(brief.contains("rule/git/safety (922 tok)"));
        // The project's own wedge sits in the project section.
        assert!(brief.contains("skill/deploy (500 tok)"));
    }

    #[test]
    fn briefing_includes_command_mix_and_thrash_detail() {
        let brief = render_briefing(&findings(), &ScopeFilter::All);
        assert!(brief.contains("cd is 53% of Bash calls"));
        assert!(brief.contains("cargo: 80"));
        assert!(brief.contains("SKILL.md edited 25x within 7m40s"));
    }

    #[test]
    fn unknown_floor_omits_the_always_on_section() {
        let mut f = findings();
        f.floor = 0;
        let brief = render_briefing(&f, &ScopeFilter::All);
        assert!(!brief.contains("Always-on context"));
    }

    #[test]
    fn no_workflow_signals_omits_the_workflow_section() {
        let mut f = findings();
        f.cd_pct = None;
        f.top_commands.clear();
        f.hotspots.clear();
        let brief = render_briefing(&f, &ScopeFilter::All);
        assert!(!brief.contains("## Workflow"));
    }
}
