//! CLI: the `analyze` and `report` commands. This is the thin shell that walks
//! files and renders tables; the analysis itself lives in the pure core. See
//! `docs/specs/cli.md`.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use crate::adapter::config::{
    claude_config_dir, read_agent_surfaces, read_claude_md_surfaces, read_hook_surfaces,
    read_mcp_server_surfaces, read_project_surfaces, read_rule_surfaces, read_skill_surfaces,
};
use crate::adapter::history::config_changes;
use crate::adapter::prompt_cache::extract_prompt_cache;
use crate::adapter::request_shape::extract_tool_context;
use crate::adapter::transcript::{
    HookActivity, count_permission_denials, count_tool_calls, extract_context_loads,
    extract_hook_activity, extract_prompt_pointers, extract_tool_errors, extract_work_events,
    parse_session, parse_subagent_sidecar, prompt_tokens, session_cwd, subagent_id_from_file_name,
    subagent_prompt_id, subagent_sidecar_path, subagents_dir,
};
use crate::core::bucket::{Bucket, JST_OFFSET_SECS, bucket_label};
use crate::core::cache::{DEFAULT_MIN_REUSE, observe_cache};
use crate::core::context_load::{
    ContextLoad, LoadTuning, LoadVerdict, load_verdict, load_wedge, tokens_paid,
};
use crate::core::effect::{
    Effect, Flag, Params as EffectParams, Verdict, compare as compare_effect,
};
use crate::core::friction::ErrorCategory;
use crate::core::hook::{
    CallResult, CatalogHook, HookDef, HookOutcome, HookTrace, HookUsage, Step, Unattributed,
    retry_calls, rollup, standing_context, trigger_count,
};
use crate::core::optimize as optimize_mod;
use crate::core::scope::{ScopeFilter, split_friction};
use crate::core::span::{DEFAULT_IDLE_GAP_MS, SessionStart, extract_spans, session_start};
use crate::core::subagent::{
    AgentModelChoice, ModelChoice, SplitCoverage, SubagentRun, caller_models, link_runs,
    model_choice_by_agent,
};
use crate::core::surface::{
    LoadMode, Scope, StartupSavings, Surface, Wedge, classify_wedge, on_demand_tokens,
    startup_savings,
};
use crate::core::thrash::{
    BashOutcome, check_key, check_outcome, count_unsandboxed_retries, detect_check_loops,
    detect_thrash,
};
use crate::core::tool_load::{
    TOOL_LOAD_THRESHOLDS, ToolLoadWedge, ToolLoadWedgeKind, tool_load_wedges,
};
use crate::core::usage::{attribute_subagents, extract_usage_events, output_tokens, run_model};
use crate::store::{
    AgentModelCost, Decomposition, McpServerCost, SessionMeta, Store, WorkRow, canonical_path,
};

mod cache_view;

#[derive(Parser)]
#[command(
    name = "cclens",
    // Cargo.toml holds a placeholder that the release workflow and the flake
    // both stamp from the tag before building.
    version,
    about = "See where your Claude Code time and tokens go, and which config to fix.\n\
             Start with `cclens doctor`; every report explains itself and names the\n\
             follow-up command."
)]
pub struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Read your session logs and config into the local store. Runs
    /// automatically before every report — call it directly only to refresh
    /// without reading anything.
    Analyze {
        /// Transcript root (default: $CLAUDE_CONFIG_DIR/projects, or
        /// ~/.claude/projects).
        #[arg(long)]
        projects: Option<PathBuf>,
        /// Output format: table | json (the run's counters).
        #[arg(long)]
        format: Option<String>,
        /// Store path (default: $XDG_STATE_HOME/cclens/cclens.db).
        #[arg(long)]
        db: Option<PathBuf>,
    },
    /// How often each skill ran and what it cost — per skill, or over time
    /// with --by.
    Usage {
        /// Bucket usage by time: year | month | week | day | hour (JST).
        #[arg(long)]
        by: Option<String>,
        /// Output format: table | markdown.
        #[arg(long)]
        format: Option<String>,
        /// Read the store as-is; skip the automatic refresh.
        #[arg(long)]
        frozen: bool,
        /// Store path (default: $XDG_STATE_HOME/cclens/cclens.db).
        #[arg(long)]
        db: Option<PathBuf>,
    },
    /// Everything installed in your config (skills, rules, agents, MCP
    /// servers, CLAUDE.md), what it weighs, and whether it was ever used.
    Inventory {
        /// Show one config layer only: global (~/.claude) | project (all
        /// projects' own config) | project:<slug> (one project's).
        #[arg(long)]
        scope: Option<String>,
        /// Output format: table | markdown.
        #[arg(long)]
        format: Option<String>,
        /// Read the store as-is; skip the automatic refresh.
        #[arg(long)]
        frozen: bool,
        /// Store path (default: $XDG_STATE_HOME/cclens/cclens.db).
        #[arg(long)]
        db: Option<PathBuf>,
    },
    /// Cleanup opportunities: config that is never used, loaded every session
    /// while heavy, or rarely worth its cost — ranked by what removing it
    /// actually saves.
    Waste {
        /// Show one config layer only: global (~/.claude) | project (all
        /// projects' own config) | project:<slug> (one project's).
        #[arg(long)]
        scope: Option<String>,
        /// Output format: table | markdown.
        #[arg(long)]
        format: Option<String>,
        /// Read the store as-is; skip the automatic refresh.
        #[arg(long)]
        frozen: bool,
        /// Store path (default: $XDG_STATE_HOME/cclens/cclens.db).
        #[arg(long)]
        db: Option<PathBuf>,
    },
    /// How often each rule (and CLAUDE.md) was actually loaded into context —
    /// and which path-conditional rules never fire or fire in almost every
    /// session.
    Rules {
        /// Show one config layer only: global (~/.claude) | project (all
        /// projects' own config) | project:<slug> (one project's).
        #[arg(long)]
        scope: Option<String>,
        /// Output format: table | markdown | json.
        #[arg(long)]
        format: Option<String>,
        /// Read the store as-is; skip the automatic refresh.
        #[arg(long)]
        frozen: bool,
        /// Store path (default: $XDG_STATE_HOME/cclens/cclens.db).
        #[arg(long)]
        db: Option<PathBuf>,
    },
    /// What every session pays in context before any work starts, and how
    /// much of that comes from your own config files vs Claude Code itself.
    Overhead {
        /// Output format: table | markdown.
        #[arg(long)]
        format: Option<String>,
        /// Read the store as-is; skip the automatic refresh.
        #[arg(long)]
        frozen: bool,
        /// Store path (default: $XDG_STATE_HOME/cclens/cclens.db).
        #[arg(long)]
        db: Option<PathBuf>,
    },
    /// How you steer Claude: the mix of instructing, steering ("go ahead"),
    /// correcting ("no, instead…"), and questioning prompts — and what that
    /// mix suggests changing.
    Prompts {
        /// Output format: table | markdown.
        #[arg(long)]
        format: Option<String>,
        /// Read the store as-is; skip the automatic refresh.
        #[arg(long)]
        frozen: bool,
        /// Store path (default: $XDG_STATE_HOME/cclens/cclens.db).
        #[arg(long)]
        db: Option<PathBuf>,
    },
    /// Tool calls that kept failing the same way — recurring failures that
    /// waste turns and tokens, each with a suggested fix.
    Failures {
        /// Show one config layer only: global (failures spread across
        /// projects — habits), project (each project's own failures), or
        /// project:<slug> (everything in that one project).
        #[arg(long)]
        scope: Option<String>,
        /// Output format: table | markdown.
        #[arg(long)]
        format: Option<String>,
        /// Read the store as-is; skip the automatic refresh.
        #[arg(long)]
        frozen: bool,
        /// Store path (default: $XDG_STATE_HOME/cclens/cclens.db).
        #[arg(long)]
        db: Option<PathBuf>,
    },
    /// What each configured hook costs and catches: how often its matcher
    /// selected a call, how often it blocked, its run time, the context it
    /// injected, and how many calls each block cost before the work moved on.
    Hooks {
        /// Show one config layer only: global, project, or project:<slug>.
        #[arg(long)]
        scope: Option<String>,
        /// Output format: table | markdown | json.
        #[arg(long)]
        format: Option<String>,
        /// Read the store as-is; skip the automatic refresh.
        #[arg(long)]
        frozen: bool,
        /// Store path (default: $XDG_STATE_HOME/cclens/cclens.db).
        #[arg(long)]
        db: Option<PathBuf>,
    },
    /// How well the prompt cache held: hit rate, the tokens re-written after the
    /// cache broke, why it broke, and which MCP servers or tools keep breaking it.
    Cache {
        /// Rows of the main table: session | project | year | month | week | day |
        /// hour (JST). Default: project.
        #[arg(long)]
        by: Option<String>,
        /// Output format: table | markdown | json.
        #[arg(long)]
        format: Option<String>,
        /// Read the store as-is; skip the automatic refresh.
        #[arg(long)]
        frozen: bool,
        /// Store path (default: $XDG_STATE_HOME/cclens/cclens.db).
        #[arg(long)]
        db: Option<PathBuf>,
    },
    /// Files Claude kept re-editing in rapid bursts — where it got stuck
    /// retrying instead of making progress.
    Stuck {
        /// Output format: table | markdown.
        #[arg(long)]
        format: Option<String>,
        /// Read the store as-is; skip the automatic refresh.
        #[arg(long)]
        frozen: bool,
        /// Store path (default: $XDG_STATE_HOME/cclens/cclens.db).
        #[arg(long)]
        db: Option<PathBuf>,
    },
    /// Did a config change reduce the failures it was written for? Friction
    /// rates in the sessions before vs after each rule's last change.
    Effect {
        /// Surface kind to compare: rule (default), skill, claude_md, or
        /// all.
        #[arg(long, default_value = "rule")]
        kind: String,
        /// Longest span compared on each side of the change, in days.
        #[arg(long, default_value_t = 28)]
        window: i64,
        /// Output format: table | markdown | json.
        #[arg(long)]
        format: Option<String>,
        /// Read the store as-is; skip the automatic refresh.
        #[arg(long)]
        frozen: bool,
        /// Store path (default: $XDG_STATE_HOME/cclens/cclens.db).
        #[arg(long)]
        db: Option<PathBuf>,
    },
    /// The health check — what to fix first, what your sessions cost, and
    /// what is fine as-is. Start here.
    Doctor {
        /// Show one config layer only: global (~/.claude) | project (all
        /// projects' own config) | project:<slug> (one project's).
        #[arg(long)]
        scope: Option<String>,
        /// Output format: table (human text) | json (the routed findings).
        #[arg(long)]
        format: Option<String>,
        /// Read the store as-is; skip the automatic refresh.
        #[arg(long)]
        frozen: bool,
        /// Store path (default: $XDG_STATE_HOME/cclens/cclens.db).
        #[arg(long)]
        db: Option<PathBuf>,
    },
    /// Ask anything the fixed reports don't cover: run a read-only SQL query
    /// against the analyzed store (as an argument, or piped via stdin).
    /// `SELECT sql FROM sqlite_master` shows the schema.
    Sql {
        /// The SQL to run. If omitted, the query is read from stdin.
        query: Option<String>,
        /// Output format: table | markdown.
        #[arg(long)]
        format: Option<String>,
        /// Store path (default: $XDG_STATE_HOME/cclens/cclens.db).
        #[arg(long)]
        db: Option<PathBuf>,
    },
    /// Act on the findings: opens an interactive `claude` session pre-loaded
    /// with the full analysis, which investigates each problem and proposes
    /// concrete config edits for your approval.
    Optimize {
        /// Transcript root (default: $CLAUDE_CONFIG_DIR/projects, or
        /// ~/.claude/projects).
        #[arg(long)]
        projects: Option<PathBuf>,
        /// Store to analyze into / read from (default:
        /// $XDG_STATE_HOME/cclens/cclens.db).
        #[arg(long)]
        db: Option<PathBuf>,
        /// Optimize one config layer only: global (~/.claude) | project |
        /// project:<slug>.
        #[arg(long)]
        scope: Option<String>,
        /// Use the existing store as-is; skip the analyze step.
        #[arg(long)]
        frozen: bool,
        /// Print the composed prompt instead of launching `claude`.
        #[arg(long)]
        print: bool,
    },
}

pub fn run() -> Result<()> {
    match Cli::parse().command {
        Command::Analyze {
            projects,
            format,
            db,
        } => analyze(projects, parse_format(format.as_deref())?, &resolve_db(db)?),
        Command::Usage {
            by,
            format,
            frozen,
            db,
        } => usage(
            by.as_deref(),
            parse_format(format.as_deref())?,
            frozen,
            &resolve_db(db)?,
        ),
        Command::Inventory {
            scope,
            format,
            frozen,
            db,
        } => inventory(
            &parse_scope(scope.as_deref())?,
            parse_format(format.as_deref())?,
            frozen,
            &resolve_db(db)?,
        ),
        Command::Waste {
            scope,
            format,
            frozen,
            db,
        } => waste(
            &parse_scope(scope.as_deref())?,
            parse_format(format.as_deref())?,
            frozen,
            &resolve_db(db)?,
        ),
        Command::Rules {
            scope,
            format,
            frozen,
            db,
        } => rules(
            &parse_scope(scope.as_deref())?,
            parse_format(format.as_deref())?,
            frozen,
            &resolve_db(db)?,
        ),
        Command::Overhead { format, frozen, db } => {
            overhead(parse_format(format.as_deref())?, frozen, &resolve_db(db)?)
        }
        Command::Prompts { format, frozen, db } => {
            prompts(parse_format(format.as_deref())?, frozen, &resolve_db(db)?)
        }
        Command::Failures {
            scope,
            format,
            frozen,
            db,
        } => failures(
            &parse_scope(scope.as_deref())?,
            parse_format(format.as_deref())?,
            frozen,
            &resolve_db(db)?,
        ),
        Command::Hooks {
            scope,
            format,
            frozen,
            db,
        } => hooks(
            &parse_scope(scope.as_deref())?,
            parse_format(format.as_deref())?,
            frozen,
            &resolve_db(db)?,
        ),
        Command::Cache {
            by,
            format,
            frozen,
            db,
        } => cache_view::cache(
            by.as_deref(),
            parse_format(format.as_deref())?,
            frozen,
            &resolve_db(db)?,
        ),
        Command::Stuck { format, frozen, db } => {
            stuck(parse_format(format.as_deref())?, frozen, &resolve_db(db)?)
        }
        Command::Effect {
            kind,
            window,
            format,
            frozen,
            db,
        } => effect(
            &kind,
            window,
            parse_format(format.as_deref())?,
            frozen,
            &resolve_db(db)?,
        ),
        Command::Doctor {
            scope,
            format,
            frozen,
            db,
        } => doctor(
            &parse_scope(scope.as_deref())?,
            parse_format(format.as_deref())?,
            frozen,
            &resolve_db(db)?,
        ),
        Command::Sql { query, format, db } => sql(
            query.as_deref(),
            parse_format(format.as_deref())?,
            &resolve_db(db)?,
        ),
        Command::Optimize {
            projects,
            db,
            scope,
            frozen,
            print,
        } => optimize(
            projects,
            &resolve_db(db)?,
            &parse_scope(scope.as_deref())?,
            frozen,
            print,
        ),
    }
}

fn parse_scope(value: Option<&str>) -> Result<ScopeFilter> {
    match value {
        None => Ok(ScopeFilter::All),
        Some(v) => ScopeFilter::parse(v)
            .with_context(|| format!("unknown --scope '{v}' (global | project | project:<slug>)")),
    }
}

/// Analyze, compose the advisor prompt from the findings, and hand it to an
/// interactive `claude` session — the AI-proposal layer's entry point. The pure
/// composition lives in `core::optimize`; this shell does the I/O: run the
/// analysis, read the store into `Findings`, then launch `claude`.
///
/// The briefing carries concrete paths and error excerpts that may be sensitive,
/// so it is written to a private (`0600`) temp file and only a pointer is passed
/// on argv — never the data, which `ps` would otherwise expose. The file is
/// removed as soon as the session ends. `--print` instead writes the full prompt
/// (briefing inline) to stdout for piping / inspection.
fn optimize(
    projects: Option<PathBuf>,
    db: &Path,
    filter: &ScopeFilter,
    frozen: bool,
    print: bool,
) -> Result<()> {
    if !frozen {
        let stats = run_analyze(projects, db)?;
        eprintln!(
            "{}",
            Style::stderr().dim(&format!(
                "store: refreshed just now ({} transcript(s) re-analyzed, {} unchanged)",
                stats.sessions, stats.skipped
            ))
        );
    }
    let store = Store::open(db).context("open store")?;
    let findings = collect_findings(&store)?;

    // The absolute store path so the session's `cclens sql --db …` hits the
    // store this run built, wherever it is launched from.
    let db_display = std::fs::canonicalize(db)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| db.to_string_lossy().into_owned());

    if print {
        println!(
            "{}",
            optimize_mod::compose_prompt(&findings, &db_display, filter)
        );
        return Ok(());
    }

    // Sensitive briefing → private temp file; only a data-free pointer on argv.
    let briefing = optimize_mod::render_briefing(&findings, filter);
    let briefing_path = write_private_tempfile(&briefing).context("write briefing temp file")?;
    let prompt = optimize_mod::launch_prompt(&briefing_path.to_string_lossy(), &db_display, filter);

    // Hand over the terminal: `claude <prompt>` starts an interactive session,
    // inheriting our stdio so the user takes over. Clean up the briefing file
    // the moment the session ends, whatever the outcome.
    let result = std::process::Command::new("claude").arg(&prompt).status();
    let _ = fs::remove_file(&briefing_path);

    let status = result.context("launch `claude` — is Claude Code installed and on PATH?")?;
    if !status.success() {
        anyhow::bail!("claude exited with {status}");
    }
    Ok(())
}

/// Write `contents` to a fresh file in the temp dir. Used for the optimization
/// briefing, which may hold sensitive paths/excerpts and must not sit on argv or
/// be world-readable.
fn write_private_tempfile(contents: &str) -> Result<PathBuf> {
    // The clock is here to avoid a collision with a leftover file, not to be
    // unguessable — exclusive creation is what makes guessing the name useless.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    let mut path = std::env::temp_dir();
    path.push(format!(
        "cclens-briefing-{}-{nanos:x}.md",
        std::process::id()
    ));
    write_private_file(&path, contents)?;
    Ok(path)
}

/// Create `path` and write `contents` to it, for the current user only.
///
/// The temp dir is shared on unix, so the file has to be **created
/// exclusively**: a same-user process that pre-creates the path — as a symlink
/// to somewhere it can read — would otherwise capture the briefing, and no
/// permission bits set afterwards would help. Losing that race now fails the
/// open instead of following what is already there.
///
/// `0600` narrows it further on unix. Windows has no mode to set here; the
/// default per-user `%LOCALAPPDATA%\Temp` is what keeps other unprivileged
/// users out, so the guarantee there rests on the directory rather than the
/// file. Setting an explicit ACL would mean a new dependency for a second lock
/// on a door that is already locked.
fn write_private_file(path: &Path, contents: &str) -> Result<()> {
    use std::io::Write;

    let mut opts = fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)?.write_all(contents.as_bytes())?;
    Ok(())
}

/// Gather the COMPLETE analysis the optimization briefing carries — every view's
/// detail as owned data for `core::optimize`, so the seeded session works from
/// the briefing and need not re-run the tool. Lists are capped where a long tail
/// adds nothing for ranking.
fn collect_findings(store: &Store) -> Result<optimize_mod::Findings> {
    // The briefing's "unknown floor" sentinel is 0 — no session start observed
    // means the always-on section is omitted rather than guessed at.
    let floor = store.baseline_floor()?.unwrap_or(0);

    // A few concrete examples per (project, category) — the actual failing
    // paths/files behind each count, so the fix is obvious from the briefing.
    const EXAMPLES_PER_CATEGORY: u32 = 3;
    let mut examples: std::collections::HashMap<(String, String), Vec<String>> =
        std::collections::HashMap::new();
    for (project, category, excerpt) in store.error_examples(EXAMPLES_PER_CATEGORY)? {
        examples
            .entry((project, category))
            .or_default()
            .push(excerpt);
    }

    // The split of each (project, category) across the tools that produced it —
    // so the briefing carries the attribution the agent otherwise re-derives.
    let mut by_tool: std::collections::HashMap<(String, String), Vec<(String, i64)>> =
        std::collections::HashMap::new();
    for (project, category, tool, n) in store.error_tool_breakdown()? {
        by_tool
            .entry((project, category))
            .or_default()
            .push((tool, n));
    }

    // Route actionable friction to the layer that owns each fix (core::scope):
    // majority-owned categories to their project, the spread rest to global.
    let cells: Vec<(String, String, i64)> = store
        .error_counts_by_project()?
        .into_iter()
        .filter(|(_, label, _)| ErrorCategory::from_label(label).is_actionable())
        .collect();
    let split = split_friction(&cells);

    // A global category aggregates its tool split across projects and pools a
    // few examples; a project-owned one uses its own (project, category) detail.
    const EXAMPLES_PER_GLOBAL_CATEGORY: usize = 3;
    let friction_global: Vec<optimize_mod::FrictionCat> = split
        .global
        .iter()
        .map(|g| {
            let mut tools: std::collections::HashMap<String, i64> =
                std::collections::HashMap::new();
            let mut pooled = Vec::new();
            for (project, label, _) in &cells {
                if label != &g.category {
                    continue;
                }
                let key = (project.clone(), label.clone());
                for (tool, n) in by_tool.get(&key).into_iter().flatten() {
                    *tools.entry(tool.clone()).or_default() += n;
                }
                pooled.extend(examples.get(&key).into_iter().flatten().cloned());
            }
            let mut by_tool: Vec<(String, i64)> = tools.into_iter().collect();
            by_tool.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            pooled.truncate(EXAMPLES_PER_GLOBAL_CATEGORY);
            optimize_mod::FrictionCat {
                label: g.category.clone(),
                count: g.total,
                projects: g.projects,
                by_tool,
                examples: pooled,
            }
        })
        .collect();

    // Workflow: Bash mix + cd share, most-edited files (global signals).
    let bash = store.work_counts("bash_cmd")?;
    let bash_total: i64 = bash.iter().map(|(_, n)| n).sum();
    let cd_pct = (bash_total > 0).then(|| {
        let cd = bash.iter().find(|(c, _)| c == "cd").map_or(0, |(_, n)| *n);
        (cd as f64 * 100.0 / bash_total as f64).round() as i64
    });
    let top_commands: Vec<(String, i64)> = bash.into_iter().take(15).collect();
    let hotspots: Vec<(String, i64)> = store
        .work_counts("file_edit")?
        .into_iter()
        .take(15)
        .collect();

    // Thrash per project — a burst belongs to the project it happened in. Each
    // episode already knows its project, so detection runs once over everything.
    let mut thrash_by_project: std::collections::HashMap<String, Vec<optimize_mod::ThrashLine>> =
        std::collections::HashMap::new();
    for episode in detect_thrash(&store.file_edits()?, 5 * 60, 4) {
        let lines = thrash_by_project
            .entry(episode.project.clone())
            .or_default();
        if lines.len() < 5 {
            lines.push(optimize_mod::ThrashLine {
                span_secs: episode.span_secs(),
                file: episode.file().to_string(),
                edits: episode.edits,
            });
        }
    }

    // Failing-check loops, like thrash, belong to the session's project.
    let mut loops_by_project: std::collections::HashMap<String, Vec<optimize_mod::CheckLoopLine>> =
        std::collections::HashMap::new();
    for found in detect_check_loops(&store.work_steps()?, MIN_CHECK_ROUNDS) {
        let lines = loops_by_project.entry(found.project.clone()).or_default();
        if lines.len() < 5 {
            lines.push(optimize_mod::CheckLoopLine {
                check: found.check,
                rounds: found.rounds,
                failures: found.failures,
                resolved: found.resolved,
            });
        }
    }

    // Unsandboxed retries route by concentration, like a friction category.
    let retries = count_unsandboxed_retries(&store.sandboxed_runs()?);
    let retry_line = |filter: &ScopeFilter| {
        let (total, after_sandbox_denial) = retries_in_scope(&retries, filter);
        (total > 0).then_some(optimize_mod::RetryLine {
            total,
            after_sandbox_denial,
        })
    };
    let retries_global = retry_line(&ScopeFilter::Global);
    let mut retries_by_project: std::collections::HashMap<String, optimize_mod::RetryLine> =
        retry_owner(&retries)
            .and_then(|owner| Some((owner, retry_line(&ScopeFilter::Project(None))?)))
            .into_iter()
            .collect();

    // Config wedges, split by the surface's own scope.
    let scoped = config_wedges(store)?;

    // Assemble per-project findings: any project owning friction, thrash, or
    // project-scoped wedges gets a section, busiest first.
    let roots: std::collections::HashMap<String, String> = store
        .session_roots()?
        .into_iter()
        .map(|(root, project)| (project, root))
        .collect();
    let mut owned_friction: std::collections::HashMap<String, Vec<(String, i64)>> =
        split.per_project.into_iter().collect();
    let mut project_names: Vec<String> = owned_friction
        .keys()
        .chain(thrash_by_project.keys())
        .chain(loops_by_project.keys())
        .chain(retries_by_project.keys())
        .chain(scoped.projects.keys())
        .cloned()
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    let mut projects: Vec<optimize_mod::ProjectFindings> = project_names
        .drain(..)
        .map(|project| {
            let friction = owned_friction
                .remove(&project)
                .unwrap_or_default()
                .into_iter()
                .map(|(label, count)| {
                    let key = (project.clone(), label.clone());
                    optimize_mod::FrictionCat {
                        by_tool: by_tool.get(&key).cloned().unwrap_or_default(),
                        examples: examples.get(&key).cloned().unwrap_or_default(),
                        label,
                        count,
                        projects: 1,
                    }
                })
                .collect();
            let wedges = scoped.projects.get(&project);
            optimize_mod::ProjectFindings {
                friction,
                thrash: thrash_by_project.remove(&project).unwrap_or_default(),
                check_loops: loops_by_project.remove(&project).unwrap_or_default(),
                unsandboxed_retries: retries_by_project.remove(&project),
                unused: wedges.map(|w| w.unused.clone()).unwrap_or_default(),
                always_on_heavy: wedges
                    .map(|w| w.always_on_heavy.clone())
                    .unwrap_or_default(),
                root: roots.get(&project).cloned().unwrap_or_default(),
                project,
            }
        })
        .collect();
    // Busiest first: owned friction, then config wedges, then thrash — so a
    // friction-free but config-heavy project still outranks thrash-only noise.
    projects.sort_by(|a, b| {
        let weight = |p: &optimize_mod::ProjectFindings| {
            (
                p.total_friction(),
                p.unused.len() + p.always_on_heavy.len(),
                p.thrash.len() + p.check_loops.len(),
            )
        };
        weight(b)
            .cmp(&weight(a))
            .then_with(|| a.project.cmp(&b.project))
    });

    let pcounts = store.prompt_behavior_counts()?;
    let ptotal: i64 = pcounts.iter().map(|(_, n)| n).sum();
    let share = |name: &str| {
        let n = pcounts
            .iter()
            .find(|(b, _)| b == name)
            .map_or(0, |(_, n)| *n);
        (n as f64 * 100.0 / ptotal as f64).round() as i64
    };
    let (steer_pct, correct_pct) = if ptotal > 0 {
        (Some(share("steer")), Some(share("correct")))
    } else {
        (None, None)
    };

    let (sub_tokens, sub_agents) = store.subagent_totals()?;
    let agents: Vec<optimize_mod::AgentCost> = store
        .subagent_by_agent()?
        .into_iter()
        .map(|row| optimize_mod::AgentCost {
            agent: row.agent,
            runs: row.runs,
            out_tokens: row.out_tokens,
        })
        .collect();
    Ok(optimize_mod::Findings {
        main_out: store.skill_usage()?.iter().map(|r| r.out_tokens).sum(),
        sub_tokens,
        sub_agents,
        agent_split: SplitCoverage::new(
            sub_tokens,
            sub_agents,
            agents.iter().map(|a| (a.runs, a.out_tokens)),
        ),
        agents,
        floor,
        config_tokens: if floor > 0 {
            store.always_on_config_tokens()?
        } else {
            0
        },
        friction_global,
        unsandboxed_retries: retries_global,
        cd_pct,
        top_commands,
        hotspots,
        unused: scoped.global.unused,
        always_on_heavy: scoped.global.always_on_heavy,
        steer_pct,
        correct_pct,
        projects,
    })
}

/// Wedge-flagged surfaces (unused-but-measurable, always-on heavy) grouped by
/// the config layer that owns them — the routing key for summary and briefing.
#[derive(Default)]
struct WedgeRefs {
    unused: Vec<optimize_mod::SurfaceRef>,
    always_on_heavy: Vec<optimize_mod::SurfaceRef>,
}

struct ScopedWedges {
    global: WedgeRefs,
    projects: std::collections::HashMap<String, WedgeRefs>,
}

fn config_wedges(store: &Store) -> Result<ScopedWedges> {
    // Carry the cost split, not one figure: a skill's file weight is its
    // invocation cost, and rendering it as the trim saving promised startup
    // context that deleting it never frees (`docs/specs/surfaces.md`).
    let surface_ref =
        |e: &crate::store::CatalogEntry, load_mode: LoadMode| optimize_mod::SurfaceRef {
            kind: e.kind.clone(),
            id: e.id.clone(),
            startup_tokens: e.startup_tokens,
            on_demand_tokens: on_demand_tokens(load_mode, e.static_tokens.map(|t| t as u64))
                .map(|t| t as i64),
        };
    let mut scoped = ScopedWedges {
        global: WedgeRefs::default(),
        projects: std::collections::HashMap::new(),
    };
    for entry in catalog_with_hook_uses(store)? {
        let refs = if entry.scope == "global" {
            &mut scoped.global
        } else {
            scoped.projects.entry(entry.project.clone()).or_default()
        };
        let load_mode = LoadMode::from_label(&entry.load_mode).unwrap_or(LoadMode::OnDemand);
        let static_tokens = entry.static_tokens.map(|t| t as u64);
        if entry.measurable && entry.uses == 0 {
            refs.unused.push(surface_ref(&entry, load_mode));
        }
        if load_mode.is_always_on() && static_tokens.is_some_and(|t| t >= HEAVY_TOKENS) {
            refs.always_on_heavy.push(surface_ref(&entry, load_mode));
        }
    }
    Ok(scoped)
}

/// Run a read-only SQL query against the store and print the result. The query
/// comes from the argument or, when absent, from stdin — so both
/// `cclens sql '…'` and `echo '…' | cclens sql` work. The store is
/// opened read-only so an ad-hoc query can never mutate the derived data.
fn sql(query: Option<&str>, format: Format, db: &Path) -> Result<()> {
    let query = match query {
        Some(q) => q.to_string(),
        None => {
            use std::io::Read;
            let mut buf = String::new();
            std::io::stdin()
                .read_to_string(&mut buf)
                .context("read SQL from stdin")?;
            buf
        }
    };
    let query = query.trim();
    if query.is_empty() {
        anyhow::bail!("no SQL provided — pass it as an argument or on stdin");
    }

    let store = Store::open_readonly(db)
        .with_context(|| format!("open store {} (run `analyze` first?)", db.display()))?;
    // `sql` never mutates the store, so it cannot auto-refresh — surface the
    // store's age instead so a stale read is at least visible. An old db whose
    // schema predates `meta` just skips the header.
    if let Ok(analyzed_at) = store.meta("analyzed_at") {
        eprint_freshness(&freshness_line(analyzed_at.as_deref()));
    }
    if format == Format::Json {
        // Typed rows as objects — SQLite integers/reals/NULLs survive as JSON
        // numbers/null. Duplicate column names keep the last value.
        let (columns, rows) = store.query_json(query)?;
        let objects: Vec<serde_json::Value> = rows
            .into_iter()
            .map(|row| serde_json::Value::Object(columns.iter().cloned().zip(row).collect()))
            .collect();
        return emit_json(&serde_json::Value::Array(objects));
    }
    let (columns, rows) = store.query(query)?;
    let headers: Vec<&str> = columns.iter().map(String::as_str).collect();
    let aligns = vec![Align::Left; columns.len()];
    render(&headers, &aligns, &rows, format);
    note(&format!("{} row(s)", rows.len()));
    Ok(())
}

/// One-screen health check: the few most actionable findings from every view,
/// split by the config layer that owns each fix — the global picture first,
/// then each project's own findings ("optimize my global setup" and "optimize
/// this project" are different tasks). `--scope` narrows to one layer.
fn doctor(filter: &ScopeFilter, format: Format, frozen: bool, db: &Path) -> Result<()> {
    if format == Format::Markdown {
        anyhow::bail!("summary has no markdown form — use the default text or --format json");
    }
    let store = open_for_read(db, frozen)?;
    let f = collect_findings(&store)?;
    let cache = cache_view::CacheSummary::read(&store)?;

    if format == Format::Json {
        // The routed findings, filtered the same way the text form is: the
        // global picture (minus the projects list) and each visible project.
        let global = filter.includes_global().then(|| {
            let mut value = serde_json::to_value(&f).expect("findings serialize");
            value.as_object_mut().expect("object").remove("projects");
            value
        });
        let projects: Vec<&optimize_mod::ProjectFindings> = f
            .projects
            .iter()
            .filter(|p| filter.includes_project(&p.project))
            .collect();
        return emit_json(&serde_json::json!({
            "global": global,
            "projects": projects,
            "prompt_cache": filter.includes_global().then(|| cache.json()),
        }));
    }

    // The text form is written for someone who has never seen cclens's
    // vocabulary: every item says what happened, why it costs, and what to do
    // — prioritized, so the report reads top to bottom as a to-do list.
    let style = Style::stdout();
    let (session_count, project_count) = store.session_stats()?;
    if session_count == 0 {
        println!("nothing analyzed yet — run `cclens analyze` first");
        return Ok(());
    }
    println!(
        "{}",
        style.heading(&format!(
            "Claude Code usage health check — {session_count} sessions across \
             {project_count} projects"
        ))
    );

    let visible: Vec<_> = f
        .projects
        .iter()
        .filter(|p| filter.includes_project(&p.project))
        .collect();
    let display = |p: &optimize_mod::ProjectFindings| {
        if p.root.is_empty() {
            p.project.clone()
        } else {
            tilde_path(&p.root)
        }
    };

    // ---- WHAT TO FIX FIRST: recurring problems, biggest first. -------------
    struct Item {
        weight: i64,
        lines: Vec<String>,
        /// Where the rest of this kind of problem is listed, for the note that
        /// names what the truncation below dropped.
        more: &'static str,
    }
    let mut items: Vec<Item> = Vec::new();
    for p in &visible {
        let stuck_on_checks = !p.check_loops.is_empty() || p.unsandboxed_retries.is_some();
        if p.total_friction() == 0 && !stuck_on_checks {
            continue;
        }
        let mut lines = vec![if p.total_friction() > 0 {
            format!(
                "In {}, {} tool calls failed in recurring ways:",
                style.bold(&display(p)),
                style.warn(&p.total_friction().to_string())
            )
        } else {
            format!("In {}, the work kept retrying:", style.bold(&display(p)))
        }];
        for cat in p.friction.iter().take(3) {
            lines.push(format!(
                "  {:>4}× {} {}",
                cat.count,
                cat.label,
                style.dim(&format!(
                    "— {}",
                    ErrorCategory::from_label(&cat.label).suggestion()
                ))
            ));
        }
        if let Some(worst) = p.thrash.first() {
            lines.push(format!(
                "  …and Claude got stuck re-editing {} ({} edits in {}m).",
                worst.file,
                worst.edits,
                worst.span_secs.div_euclid(60).max(1)
            ));
        }
        if let Some(worst) = p.check_loops.first() {
            lines.push(format!(
                "  …and `{}` kept failing for {} fix rounds without stopping to ask{}.",
                worst.check,
                worst.rounds,
                if worst.resolved { "" } else { " (never fixed)" }
            ));
        }
        if let Some(retries) = p.unsandboxed_retries {
            lines.push(format!(
                "  …and {} commands were re-run with the sandbox off after failing in it \
                 ({} after a sandbox refusal — allow those in the sandbox settings).",
                retries.total, retries.after_sandbox_denial
            ));
        }
        lines.push(format!(
            "  Fix in this project's own CLAUDE.md / .claude — details: {}",
            style.command(&format!("cclens failures --scope project:{}", p.project))
        ));
        items.push(Item {
            // Each failed fix round and each unsandboxed retry is a wasted turn,
            // the same unit a recurring failure is weighed in.
            weight: p.total_friction()
                + p.check_loops
                    .iter()
                    .map(|c| i64::from(c.rounds))
                    .sum::<i64>()
                + p.unsandboxed_retries.map_or(0, |r| r.total),
            lines,
            more: "cclens failures --scope project",
        });
    }
    if filter.includes_global() {
        for cat in &f.friction_global {
            items.push(Item {
                weight: cat.count,
                lines: vec![
                    format!(
                        "The same failure hit {} different projects ({}× total): {} {}",
                        cat.projects,
                        style.warn(&cat.count.to_string()),
                        style.bold(&cat.label),
                        style.dim(&format!(
                            "— {}",
                            ErrorCategory::from_label(&cat.label).suggestion()
                        ))
                    ),
                    format!(
                        "  No single project owns it, so the fix belongs in ~/.claude — \
                         details: {}",
                        style.command("cclens failures --scope global")
                    ),
                ],
                more: "cclens failures --scope global",
            });
        }
        // Weighed by the tokens it re-wrote, not by its break count: one break
        // re-pays a whole prompt, so a count would rank it like a single failed
        // tool call and let it fall off the list.
        for surface in cache.recurring_surfaces() {
            items.push(Item {
                weight: surface.rebuilt_tokens,
                lines: vec![
                    format!(
                        "{} changed the tool list mid-session and broke the prompt cache in {} \
                         sessions ({} re-written tokens)",
                        style.bold(&surface.surface),
                        style.warn(&surface.sessions.to_string()),
                        fmt_tokens(surface.rebuilt_tokens)
                    ),
                    format!(
                        "  Make it available from the session's start — details: {}",
                        style.command("cclens cache")
                    ),
                ],
                more: "cclens cache",
            });
        }
        if let Some(retries) = f.unsandboxed_retries {
            items.push(Item {
                weight: retries.total,
                lines: vec![
                    format!(
                        "{} commands failed inside the Bash sandbox and were re-run with it \
                         turned off — a turn each, and the re-run is unprotected.",
                        style.warn(&retries.total.to_string())
                    ),
                    format!(
                        "  {} of them followed a sandbox refusal; allowing those hosts/paths \
                         in ~/.claude/settings.json `sandbox` removes them — details: {}",
                        retries.after_sandbox_denial,
                        style.command("cclens failures --scope global")
                    ),
                ],
                more: "cclens failures --scope global",
            });
        }
    }
    items.sort_by_key(|item| std::cmp::Reverse(item.weight));
    let dropped: Vec<Item> = items.split_off(items.len().min(4));
    if filter.includes_global() {
        if let Some(cd_pct) = f.cd_pct.filter(|pct| *pct >= 25) {
            items.push(Item {
                weight: 0,
                lines: vec![format!(
                    "{} of all Bash commands are just `cd` — a working-directory \
                     convention in your global CLAUDE.md would cut the churn.",
                    style.warn(&format!("{cd_pct}%"))
                )],
                more: "",
            });
        }
        if let (Some(steer), Some(correct)) = (f.steer_pct, f.correct_pct)
            && (correct >= 10 || steer >= 25)
        {
            items.push(Item {
                weight: 0,
                lines: vec![format!(
                    "{steer}% of your prompts steer and {correct}% correct — rework that \
                     clearer upfront specs could cut. Details: {}",
                    style.command("cclens prompts")
                )],
                more: "",
            });
        }
    }

    println!("\n{}", style.heading("WHAT TO FIX FIRST"));
    if items.is_empty() {
        println!("  {}", style.good("nothing recurring found — looking good"));
    }
    for (i, item) in items.iter().enumerate() {
        let (first, rest) = item.lines.split_first().expect("item has lines");
        println!("  {}. {first}", i + 1);
        for line in rest {
            println!("   {line}");
        }
    }
    // Never truncate silently (reporting honesty, cli.md).
    if !dropped.is_empty() {
        let mut more: Vec<&str> = dropped.iter().map(|item| item.more).collect();
        more.sort_unstable();
        more.dedup();
        println!(
            "  {}",
            style.dim(&format!(
                "… and {} smaller recurring problem(s) — see {}",
                dropped.len(),
                more.iter()
                    .map(|command| format!("`{command}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        );
    }

    // ---- COST: what a session pays, in plain words. -------------------------
    if filter.includes_global() {
        println!("\n{}", style.heading("COST (token estimates, not billing)"));
        if f.floor > 0 {
            let residual = (f.floor - f.config_tokens).max(0);
            println!(
                "  Every session starts with ~{} tokens of context before any work:",
                style.bold(&fmt_tokens(f.floor))
            );
            println!(
                "  {} comes from your own global config files; the other {} is Claude Code",
                fmt_tokens(f.config_tokens),
                fmt_tokens(residual)
            );
            println!(
                "  {}",
                style.dim("itself (system prompt, built-in tools, MCP schemas) — not trimmable from files.")
            );
        }
        println!(
            "  Output written so far: {} tokens by skills on the main thread, {} by {} subagent runs.",
            fmt_tokens(f.main_out),
            fmt_tokens(f.sub_tokens),
            f.sub_agents
        );
        for line in cache.doctor_lines(&style) {
            println!("  {line}");
        }
        warn_missing_agent_split(&f.agent_split);
        for line in doctor_agent_lines(&f, &style.command("cclens usage")) {
            if line.starts_with('…') {
                println!("  {}", style.dim(&line));
            } else {
                println!("  {line}");
            }
        }
    }

    // ---- CONFIG WORTH PRUNING: installed-but-dead weight, by owner. ---------
    let mut pruning: Vec<String> = Vec::new();
    if filter.includes_global() && (!f.unused.is_empty() || !f.always_on_heavy.is_empty()) {
        pruning.push(format!(
            "  ~/.claude: {} → {}",
            prune_phrase(&f.unused, &f.always_on_heavy),
            style.command("cclens waste --scope global")
        ));
    }
    let mut prunable: Vec<_> = visible
        .iter()
        .filter(|p| !p.unused.is_empty() || !p.always_on_heavy.is_empty())
        .collect();
    prunable.sort_by_key(|p| std::cmp::Reverse(p.unused.len() + p.always_on_heavy.len()));
    for p in prunable.iter().take(3) {
        pruning.push(format!(
            "  {}: {} → {}",
            display(p),
            prune_phrase(&p.unused, &p.always_on_heavy),
            style.command(&format!("cclens waste --scope project:{}", p.project))
        ));
    }
    if !pruning.is_empty() {
        println!("\n{}", style.heading("CONFIG WORTH PRUNING"));
        println!(
            "  {}",
            style.dim("Installed config that costs attention (or context) without earning it:")
        );
        for line in pruning {
            println!("{line}");
        }
        if prunable.len() > 3 {
            println!(
                "  {}",
                style.dim(&format!(
                    "… and {} more project(s) with prunable config — see `cclens waste --scope project`",
                    prunable.len() - 3
                ))
            );
        }
    }

    // ---- LOOKS HEALTHY: say explicitly what needs no action. ----------------
    if filter.includes_global() {
        let mut healthy: Vec<String> = Vec::new();
        if let (Some(steer), Some(correct)) = (f.steer_pct, f.correct_pct)
            && correct < 10
            && steer < 25
        {
            healthy.push(format!(
                "prompting mix ({steer}% steering / {correct}% corrections) — good autonomy balance"
            ));
        }
        if f.friction_global.is_empty() {
            healthy.push("no failure habit spans multiple projects".to_string());
        }
        if f.always_on_heavy.is_empty() && f.floor > 0 {
            healthy.push(format!(
                "your global config is lean ({} of the ~{} startup tokens)",
                fmt_tokens(f.config_tokens),
                fmt_tokens(f.floor)
            ));
        }
        if !healthy.is_empty() {
            println!("\n{}", style.heading("LOOKS HEALTHY"));
            for line in healthy {
                println!("  {}", style.good(&line));
            }
        }
    }
    Ok(())
}

/// `"3 unused surfaces, 4 always-on files costing ~8.5k tokens/session"` — the
/// pruning line's plain-language phrase, zero parts dropped.
fn prune_phrase(
    unused: &[optimize_mod::SurfaceRef],
    always_on_heavy: &[optimize_mod::SurfaceRef],
) -> String {
    let mut parts = Vec::new();
    if !unused.is_empty() {
        parts.push(format!(
            "{} installed but never used",
            plural(unused.len(), "surface")
        ));
    }
    if !always_on_heavy.is_empty() {
        let tokens: i64 = always_on_heavy
            .iter()
            .filter_map(|s| s.startup_tokens)
            .sum();
        parts.push(format!(
            "{} loaded every session (~{} tokens each time)",
            plural(always_on_heavy.len(), "heavy file"),
            fmt_tokens(tokens)
        ));
    }
    parts.join(", ")
}

fn plural(n: usize, noun: &str) -> String {
    if n == 1 {
        format!("1 {noun}")
    } else {
        format!("{n} {noun}s")
    }
}

/// The doctor's lines naming the agents behind the subagent figure — "which
/// agent" is the actionable half of it. The top rows are introduced as "most
/// of" the output only when they actually exceed half of it; when the rows
/// are a partial split (`SplitCoverage`), the gap is stated first so the
/// names are not read as the whole bill. `full_split` is the styled command
/// that shows every row. An unavailable split yields nothing: that case is
/// reported on stderr with its refresh hint.
fn doctor_agent_lines(f: &optimize_mod::Findings, full_split: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let top: Vec<String> = f
        .agents
        .iter()
        .take(3)
        .map(|a| {
            format!(
                "{} {} ({} run{})",
                a.label(),
                fmt_tokens(a.out_tokens),
                a.runs,
                if a.runs == 1 { "" } else { "s" }
            )
        })
        .collect();
    if top.is_empty() {
        return lines;
    }
    let coverage = &f.agent_split;
    let shown: i64 = f.agents.iter().take(top.len()).map(|a| a.out_tokens).sum();
    let intro = if coverage.is_partial() {
        lines.push(format!(
            "Only {} of those tokens ({} of {} runs) have a per-agent split — the rest were \
             analyzed before runs were extracted and their transcripts are gone.",
            fmt_tokens(coverage.known_tokens),
            coverage.known_runs,
            coverage.total_runs
        ));
        "Among the runs that have one"
    } else if coverage.is_majority(shown) {
        "Most of that subagent output"
    } else {
        "The largest agent types"
    };
    lines.push(format!(
        "{intro}: {}. Full split: {full_split}",
        top.join(", ")
    ));
    // Never truncate silently (reporting honesty, cli.md).
    if f.agents.len() > top.len() {
        lines.push(format!(
            "… and {} more agent type(s) not shown",
            f.agents.len() - top.len()
        ));
    }
    lines
}

/// Humanize a token count: `6467406` → `6.5M`, `32139` → `32.1k`.
fn fmt_tokens(n: i64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1e6)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1e3)
    } else {
        n.to_string()
    }
}

/// One dimmed framing block above a table: what this view shows and how to
/// read it, so the output explains itself. Table format only — markdown is for
/// pasting the table, json is the machine contract.
fn preamble(format: Format, text: &str) {
    if matches!(format, Format::Table) {
        println!("{}\n", Style::stdout().dim(text));
    }
}

/// Shorten an absolute path under the home directory to `~/…` for display.
fn tilde_path(path: &str) -> String {
    shorten_to_tilde(path, home_dir())
}

/// The shortening itself, separated from the environment lookup so it stays
/// pure and testable. A separator must follow the prefix, so `/home/metadata`
/// is not mistaken for something under `/home/me`; either separator counts, so
/// a Windows path shortens against a Windows home.
fn shorten_to_tilde(path: &str, home: Option<&str>) -> String {
    let Some(home) = home.filter(|home| !home.is_empty()) else {
        return path.to_string();
    };
    // The two sides come from different places — `home` from the environment,
    // `path` from the cwd a transcript recorded — so on Windows they can spell
    // the same directory differently; the OS accepts either. Only the
    // comparison is folded. The suffix is sliced from the original at the same
    // offset, which stays valid because folding swaps one ASCII byte for
    // another, so the display keeps whatever spelling the path really had.
    let fold = |s: &str| s.replace('\\', "/");
    match fold(path).strip_prefix(&fold(home)) {
        Some("") => "~".to_string(),
        Some(rest) if rest.starts_with('/') => format!("~{}", &path[home.len()..]),
        _ => path.to_string(),
    }
}

/// Thrash bursts: a file edited many times in a short window — where Claude got
/// stuck and kept retrying, as opposed to a hotspot's healthy spread-out edits.
/// An episode is one session's edits to one path, so parallel work on a
/// same-named file is never presented as one agent looping (`core::thrash`).
fn stuck(format: Format, frozen: bool, db: &Path) -> Result<()> {
    const GAP_SECS: i64 = 5 * 60;
    const MIN_EDITS: u32 = 4;
    let store = open_for_read(db, frozen)?;
    let episodes = detect_thrash(&store.file_edits()?, GAP_SECS, MIN_EDITS);
    let checks = detect_check_loops(&store.work_steps()?, MIN_CHECK_ROUNDS);
    if format == Format::Json {
        let episodes: Vec<serde_json::Value> = episodes
            .iter()
            .map(|e| {
                serde_json::json!({
                    "project": e.project,
                    "session_id": e.session_id,
                    "file": e.file(),
                    "path": e.path,
                    "edits": e.edits,
                    "span_secs": e.span_secs(),
                })
            })
            .collect();
        let checks: Vec<serde_json::Value> = checks
            .iter()
            .map(|c| {
                serde_json::json!({
                    "project": c.project,
                    "session_id": c.session_id,
                    "check": c.check,
                    "rounds": c.rounds,
                    "failures": c.failures,
                    "resolved": c.resolved,
                    "span_secs": c.span_secs(),
                })
            })
            .collect();
        return emit_json(&serde_json::json!({ "episodes": episodes, "checks": checks }));
    }
    if episodes.is_empty() && checks.is_empty() {
        println!(
            "no thrash episodes (>= {MIN_EDITS} rapid re-edits, or a check failing \
             >= {MIN_CHECK_ROUNDS} rounds) — run analyze first"
        );
        return Ok(());
    }
    if !episodes.is_empty() {
        stuck_files(&episodes, format, GAP_SECS, MIN_EDITS);
    }
    if !checks.is_empty() {
        if !episodes.is_empty() {
            println!();
        }
        stuck_checks(&checks, format);
    }
    Ok(())
}

/// A check still failing after this many fix rounds is where the user's
/// "three rounds, then ask" rule says to stop (`docs/specs/events.md`).
const MIN_CHECK_ROUNDS: u32 = 3;

fn stuck_checks(checks: &[crate::core::thrash::CheckLoop], format: Format) {
    preamble(
        format,
        "Checks that kept failing round after round in one session, with edits in\n\
         between — Claude kept trying fixes instead of stopping to ask. Worth asking\n\
         what it was missing (a failing test it could not read? a wrong assumption?).",
    );
    let rows: Vec<Vec<String>> = checks
        .iter()
        .take(20)
        .map(|c| {
            let span = c.span_secs();
            vec![
                c.project.clone(),
                c.check.clone(),
                c.rounds.to_string(),
                c.failures.to_string(),
                if c.resolved { "yes" } else { "no" }.to_string(),
                format!("{}m{}s", span / 60, span % 60),
            ]
        })
        .collect();
    render(
        &[
            "project",
            "check",
            "rounds",
            "failed runs",
            "resolved",
            "within",
        ],
        &[
            Align::Left,
            Align::Left,
            Align::Right,
            Align::Right,
            Align::Left,
            Align::Right,
        ],
        &rows,
        format,
    );
    note(&format!(
        "a round is a failing run after an edit, the first failure aside; >= {MIN_CHECK_ROUNDS} \
         rounds in one session\nis where \"three rounds, then ask\" says to stop.\n\
         The check is the runner + subcommand (a curated list — a heuristic). `--format json`\n\
         carries the session id to investigate with."
    ));
}

fn stuck_files(
    episodes: &[crate::core::thrash::ThrashEpisode],
    format: Format,
    gap_secs: i64,
    min_edits: u32,
) {
    preamble(
        format,
        "Files Claude kept re-editing in rapid bursts — the signature of getting stuck\n\
         and retrying. Worth checking what blocked it there (a rule? missing context?).",
    );
    let rows: Vec<Vec<String>> = episodes
        .iter()
        .take(20)
        .map(|e| {
            let span = e.span_secs();
            vec![
                e.project.clone(),
                e.file().to_string(),
                e.edits.to_string(),
                format!("{}m{}s", span / 60, span % 60),
            ]
        })
        .collect();
    render(
        &["project", "file", "edits", "within"],
        &[Align::Left, Align::Left, Align::Right, Align::Right],
        &rows,
        format,
    );
    note(&format!(
        "bursts of >= {min_edits} edits to one file within {}m by a single session —\n\
         likely where Claude got stuck. `--format json` carries the session id and\n\
         full path to investigate with.",
        gap_secs / 60
    ));
}

/// The unsandboxed retries a scope owns, as `(retries, after a sandbox
/// denial)`. Routed like a friction category (`docs/specs/cli.md`): a strict
/// majority project owns them all, and with no owner they are global.
fn retries_in_scope(
    retries: &[crate::core::thrash::UnsandboxedRetry],
    filter: &ScopeFilter,
) -> (i64, i64) {
    let owner = retry_owner(retries);
    let included = |project: &str| match filter {
        ScopeFilter::All => true,
        ScopeFilter::Global => owner.is_none(),
        ScopeFilter::Project(None) => owner.as_deref() == Some(project),
        ScopeFilter::Project(Some(slug)) => slug == project,
    };
    retries
        .iter()
        .filter(|retry| included(&retry.project))
        .fold((0, 0), |(all, marked), retry| {
            (all + 1, marked + i64::from(retry.after_sandbox_denial))
        })
}

/// The project holding a strict majority of the unsandboxed retries, if any —
/// the same ownership rule a friction category follows (`core::scope`).
fn retry_owner(retries: &[crate::core::thrash::UnsandboxedRetry]) -> Option<String> {
    const LABEL: &str = "unsandboxed-retry";
    let mut per_project: std::collections::BTreeMap<&str, i64> = Default::default();
    for retry in retries {
        *per_project.entry(retry.project.as_str()).or_default() += 1;
    }
    let cells: Vec<(String, String, i64)> = per_project
        .into_iter()
        .map(|(project, n)| (project.to_string(), LABEL.to_string(), n))
        .collect();
    split_friction(&cells)
        .per_project
        .into_iter()
        .next()
        .map(|(project, _)| project)
}

/// What a block cost, when the trace is a block of a known call.
fn block_cost(trace: &HookTrace, steps: &[Step]) -> Option<u32> {
    if trace.outcome != HookOutcome::Blocked {
        return None;
    }
    retry_calls(steps, trace.tool_use_id.as_deref()?, trace.epoch_ms)
}

/// Every catalogued hook row with its usage, and the traces no catalogued hook
/// claimed (`core::hook::rollup`). Rows are the whole effective catalog's hook
/// rows, unfiltered, so a project hook never loses its traces to orphans just
/// because a report is scoped elsewhere.
struct HookReport {
    hooks: Vec<(crate::store::CatalogEntry, HookUsage)>,
    orphans: Vec<Unattributed>,
}

fn hook_report(store: &Store) -> Result<HookReport> {
    let mut triggers_by_scope: std::collections::HashMap<String, crate::core::hook::Triggers> =
        std::collections::HashMap::new();
    let mut entries = Vec::new();
    let mut catalog = Vec::new();
    for entry in store.effective_catalog()? {
        if entry.kind != "hook" {
            continue;
        }
        let Some(def) = HookDef::parse(&entry.id) else {
            continue;
        };
        let project = (entry.scope == "project").then(|| entry.project.clone());
        let key = project.clone().unwrap_or_default();
        if !triggers_by_scope.contains_key(&key) {
            let triggers = store.hook_triggers(project.as_deref())?;
            triggers_by_scope.insert(key.clone(), triggers);
        }
        let triggers = trigger_count(&def, &triggers_by_scope[&key]);
        catalog.push(CatalogHook {
            def,
            project,
            triggers,
        });
        entries.push(entry);
    }
    let traces = store.hook_traces()?;
    let refs: Vec<_> = traces.iter().collect();
    let (usages, orphans) = rollup(&catalog, &refs);
    Ok(HookReport {
        hooks: entries.into_iter().zip(usages).collect(),
        orphans,
    })
}

/// The effective catalog with each hook row's uses set to its trigger count —
/// measurable only once that count is known (`docs/specs/surfaces.md`).
fn catalog_with_hook_uses(store: &Store) -> Result<Vec<crate::store::CatalogEntry>> {
    let report = hook_report(store)?;
    let mut catalog = store.effective_catalog()?;
    for entry in catalog.iter_mut().filter(|e| e.kind == "hook") {
        let usage = report.hooks.iter().find(|(hook, _)| {
            hook.id == entry.id && hook.scope == entry.scope && hook.project == entry.project
        });
        let triggers = usage.and_then(|(_, usage)| usage.triggers);
        entry.uses = triggers.map_or(0, |t| t as i64);
        entry.measurable = triggers.is_some();
    }
    Ok(catalog)
}

/// A millisecond figure in the largest unit that keeps it readable.
fn fmt_ms(ms: i64) -> String {
    match ms {
        ms if ms >= 60_000 => format!("{:.1}m", ms as f64 / 60_000.0),
        ms if ms >= 1_000 => format!("{:.1}s", ms as f64 / 1_000.0),
        ms => format!("{ms}ms"),
    }
}

/// Per-hook cost and catch: uses (the triggers its matcher selected), blocks,
/// its own errors, run time, injected context and what each block cost. The
/// figures other than uses come from the traces Claude Code recorded, so they
/// cover the runs that did something visible (`docs/specs/events.md`).
fn hooks(filter: &ScopeFilter, format: Format, frozen: bool, db: &Path) -> Result<()> {
    let store = open_for_read(db, frozen)?;
    let report = hook_report(&store)?;
    let mut rows: Vec<_> = report
        .hooks
        .iter()
        .filter(|(entry, _)| in_scope(filter, &entry.scope, &entry.project))
        .collect();
    // What the hook costs or catches first; then how much it is exercised.
    rows.sort_by_key(|(entry, usage)| {
        (
            std::cmp::Reverse(usage.blocks),
            std::cmp::Reverse(usage.context_tokens),
            std::cmp::Reverse(usage.duration_total_ms),
            std::cmp::Reverse(usage.triggers.unwrap_or(0)),
            entry.id.clone(),
        )
    });
    let orphans = if *filter == ScopeFilter::All {
        report.orphans.as_slice()
    } else {
        &[]
    };
    let status = |event: &str, usage: &HookUsage| -> &'static str {
        if usage.never_matched() {
            "NEVER MATCHED"
        } else if standing_context(event).is_some() && usage.context_injections > 0 {
            "STANDING CONTEXT"
        } else {
            ""
        }
    };

    if format == Format::Json {
        let hooks: Vec<serde_json::Value> = rows
            .iter()
            .map(|(entry, usage)| {
                let def = HookDef::parse(&entry.id);
                serde_json::json!({
                    "id": entry.id,
                    "scope": entry.scope,
                    "project": if entry.project.is_empty() { None } else { Some(&entry.project) },
                    "event": def.as_ref().map(|d| &d.event),
                    "matcher": def.as_ref().and_then(|d| d.matcher.as_ref()),
                    "command": def.as_ref().map(|d| &d.command),
                    "usage": usage,
                    "block_rate": usage.block_rate(),
                    "mean_retry_calls": usage.mean_retry_calls(),
                    "status": def.as_ref().map_or("", |d| status(&d.event, usage)),
                })
            })
            .collect();
        let orphans: Vec<serde_json::Value> = orphans
            .iter()
            .map(|o| {
                serde_json::json!({
                    "trace": o.key,
                    "ambiguous": o.ambiguous,
                    "usage": o.usage,
                })
            })
            .collect();
        return emit_json(&serde_json::json!({ "hooks": hooks, "orphans": orphans }));
    }
    if rows.is_empty() && orphans.is_empty() {
        println!("no hooks configured or traced — run `cclens analyze` first");
        return Ok(());
    }

    preamble(
        format,
        "Every hook in your settings, with what it costs and what it catches. \"uses\" is\n\
         how many calls (or prompts / sessions) its matcher selected — an estimate against\n\
         today's config, \"?\" when its trigger cannot be counted. A hook that passes\n\
         silently leaves no trace, so blocks, errors, time and context cover only the runs\n\
         Claude Code recorded. NEVER MATCHED marks a hook that cannot have fired.",
    );
    let cells = |event: &str, usage: &HookUsage| -> Vec<String> {
        let blocked = match (usage.blocks, usage.block_rate()) {
            (0, _) => "-".to_string(),
            (n, Some(rate)) if rate < 0.001 => format!("{n} (<0.1%)"),
            (n, Some(rate)) => format!("{n} ({:.1}%)", rate * 100.0),
            (n, None) => n.to_string(),
        };
        let time = match (usage.duration_p50_ms, usage.duration_p95_ms) {
            (Some(p50), Some(p95)) => format!(
                "{} (p50 {} / p95 {}, n={})",
                fmt_ms(usage.duration_total_ms),
                fmt_ms(p50),
                fmt_ms(p95),
                usage.measured_runs
            ),
            _ => "-".to_string(),
        };
        let context = match usage.mean_context_tokens() {
            Some(mean) => {
                let per = standing_context(event).unwrap_or("injection");
                format!(
                    "{} tok (~{mean}/{per}, {}x)",
                    fmt_tokens(usage.context_tokens as i64),
                    usage.context_injections
                )
            }
            None => "-".to_string(),
        };
        let retries = match (
            usage.mean_retry_calls(),
            usage.blocks - usage.resolved_blocks,
        ) {
            (None, 0) => "-".to_string(),
            (None, open) => format!("{open} unresolved"),
            (Some(mean), 0) => format!("{mean:.1} calls"),
            (Some(mean), open) => format!("{mean:.1} calls, {open} unresolved"),
        };
        let errors = match usage.errors {
            0 => "-".to_string(),
            n => n.to_string(),
        };
        vec![blocked, errors, time, context, retries]
    };
    let mut table: Vec<Vec<String>> = Vec::new();
    for (entry, usage) in &rows {
        let event = HookDef::parse(&entry.id)
            .map(|d| d.event)
            .unwrap_or_default();
        let mut row = vec![
            entry.id.clone(),
            scope_cell(&entry.scope, &entry.project),
            usage
                .triggers
                .map_or_else(|| "?".to_string(), |t| t.to_string()),
        ];
        row.extend(cells(&event, usage));
        row.push(status(&event, usage).to_string());
        table.push(row);
    }
    for orphan in orphans {
        let event = orphan.key.split([':', ' ']).next().unwrap_or_default();
        let mut row = vec![orphan.key.clone(), "-".to_string(), "-".to_string()];
        row.extend(cells(event, &orphan.usage));
        row.push(
            if orphan.ambiguous {
                "UNATTRIBUTED"
            } else {
                "ORPHANED"
            }
            .to_string(),
        );
        table.push(row);
    }
    render(
        &[
            "hook",
            "scope",
            "uses",
            "blocked",
            "errors",
            "time",
            "context",
            "cost per block",
            "status",
        ],
        &[
            Align::Left,
            Align::Left,
            Align::Right,
            Align::Right,
            Align::Right,
            Align::Left,
            Align::Left,
            Align::Left,
            Align::Left,
        ],
        &table,
        format,
    );
    note(
        "cost per block: tool calls after a block until the same tool next succeeded —\n\
         a heuristic for \"until the same intent passed\"; a block followed by a human\n\
         turn or the session's end is unresolved. UNATTRIBUTED: Claude Code recorded no\n\
         command, and several of your hooks could have left it. ORPHANED: traces of hooks\n\
         not in your settings (a plugin, or a wrapper passing --settings).",
    );
    Ok(())
}

/// Before/after each config change: the friction rates in the sessions before
/// vs after the surface last changed (`core::effect`). Declared targets get a
/// verdict and, when they did not improve, a hook-candidate flag; undeclared
/// surfaces show every category that occurred and are left to the reader.
fn effect(kind: &str, window_days: i64, format: Format, frozen: bool, db: &Path) -> Result<()> {
    const MAX_UNDECLARED_ROWS: usize = 5;
    const MAX_CONFOUNDERS: usize = 5;
    anyhow::ensure!(
        window_days > 0,
        "--window must be a positive number of days"
    );
    let store = open_for_read(db, frozen)?;
    let changes = store.surface_changes()?;
    let samples = store.session_samples()?;
    let params = EffectParams {
        window_secs: window_days * 86_400,
        ..EffectParams::default()
    };
    let now = chrono::Utc::now().timestamp();
    let mut effects: Vec<Effect> = changes
        .iter()
        .filter(|c| kind == "all" || c.kind == kind)
        .map(|c| compare_effect(c, &changes, &samples, now, &params))
        .filter(|e| !e.change.targets.is_empty() || !e.categories.is_empty())
        .collect();
    // Declared surfaces first — they carry verdicts — then the newest change.
    effects.sort_by_key(|e| {
        (
            e.change.targets.is_empty(),
            std::cmp::Reverse(e.change.changed_epoch),
        )
    });

    if format == Format::Json {
        let rows: Vec<serde_json::Value> = effects.iter().map(effect_json).collect();
        return emit_json(&serde_json::json!({
            "window_days": window_days,
            "min_sessions": params.min_sessions,
            "alpha": params.alpha,
            "effects": rows,
        }));
    }
    if effects.is_empty() {
        println!(
            "no {kind} change has friction around it to compare — run analyze, or declare \
             `cclens-targets:` in a rule's frontmatter"
        );
        return Ok(());
    }
    preamble(
        format,
        "Did a config change reduce the failures it was written for? For each surface,\n\
         the failure rates in the sessions that started before its last change vs the\n\
         sessions after it, over equal windows. A rule that declares `cclens-targets:`\n\
         gets a verdict on those categories; one that does not shows every category\n\
         that occurred, for you to judge.",
    );
    let style = Style::stdout();
    for e in &effects {
        let date = |epoch: i64| local_time(epoch, "%Y-%m-%d %H:%M");
        let day = |epoch: i64| local_time(epoch, "%m-%d");
        let calls = |w: &crate::core::effect::Window| {
            w.tool_calls
                .map_or_else(|| "calls unknown".to_string(), |c| format!("{c} calls"))
        };
        // Two projects can each have their own `rule/x`; the scope tells the
        // reader which copy a verdict belongs to.
        let owner = match &e.change.scope {
            Scope::Global => "global".to_string(),
            Scope::Project(project) => format!("project {project}"),
        };
        println!(
            "{} [{owner}]  changed {} ({}; introduced {})",
            style.bold(&e.change.name()),
            date(e.change.changed_epoch),
            e.change.date_source.label(),
            local_time(e.change.introduced_epoch, "%Y-%m-%d"),
        );
        println!(
            "  before {}–{}: {} session(s), {}   after {}–{}: {} session(s), {}",
            day(e.before.start),
            day(e.before.end),
            e.before.sessions,
            calls(&e.before),
            day(e.after.start),
            day(e.after.end),
            e.after.sessions,
            calls(&e.after),
        );
        if e.change.targets.is_empty() {
            println!(
                "  {}",
                style.dim("targets: undeclared — every category shown")
            );
        } else {
            println!("  targets: {}", e.change.targets.join(", "));
        }
        if !e.change.always_on {
            println!(
                "  {}",
                style.dim(
                    "loads only on demand: sessions it never reached dilute the rate, \
                     so no verdict here raises a flag"
                )
            );
        }
        if !e.unknown_targets.is_empty() {
            println!(
                "  {}",
                style.warn(&format!(
                    "unknown target categories (not compared): {}",
                    e.unknown_targets.join(", ")
                ))
            );
        }
        if !e.concurrent.is_empty() {
            println!(
                "  {}",
                style.warn(&format!(
                    "also changed inside this span ({}, nearest first): {}{}",
                    e.concurrent.len(),
                    e.concurrent[..e.concurrent.len().min(MAX_CONFOUNDERS)].join(", "),
                    if e.concurrent.len() > MAX_CONFOUNDERS {
                        ", …"
                    } else {
                        ""
                    }
                ))
            );
        }
        let shown = if e.change.targets.is_empty() {
            MAX_UNDECLARED_ROWS
        } else {
            usize::MAX
        };
        let fmt_rate = |r: Option<f64>| r.map_or_else(|| "-".to_string(), |r| format!("{r:.2}"));
        let rows: Vec<Vec<String>> = e
            .categories
            .iter()
            .take(shown)
            .map(|c| {
                let verdict = if let Some(flag) = c.flag {
                    style.warn(match flag {
                        Flag::ReplaceWithHook => "no-improvement → replace the rule with a hook?",
                        Flag::HookAlreadyBlocks => {
                            "no-improvement → the hook blocks it; the rule doesn't prevent it"
                        }
                    })
                } else if c.verdict == Verdict::Improved {
                    style.good(c.verdict.label())
                } else {
                    c.verdict.label().to_string()
                };
                vec![
                    c.category.clone(),
                    format!("{} → {}", c.before, c.after),
                    format!(
                        "{} → {}",
                        fmt_rate(c.before_per_100_calls),
                        fmt_rate(c.after_per_100_calls)
                    ),
                    format!(
                        "{} → {}",
                        fmt_rate(c.before_per_session),
                        fmt_rate(c.after_per_session)
                    ),
                    c.p_value
                        .map_or_else(|| "-".to_string(), |p| format!("{p:.3}")),
                    verdict,
                ]
            })
            .collect();
        println!();
        render(
            &[
                "category",
                "failures",
                "per 100 calls",
                "per session",
                "p",
                "verdict",
            ],
            &[
                Align::Left,
                Align::Right,
                Align::Right,
                Align::Right,
                Align::Right,
                Align::Left,
            ],
            &rows,
            format,
        );
        if e.categories.len() > shown {
            println!(
                "{}",
                style.dim(&format!(
                    "  … {} more categories (`--format json` has them all)",
                    e.categories.len() - shown
                ))
            );
        }
        println!();
    }
    note(&format!(
        "Sessions are assigned whole to the side they started on (config loads at\n\
         session start). The verdict uses the per-call rate when every session has a\n\
         call count, else per session; `p` is the one-sided exact chance of seeing this\n\
         few failures after the change if the rate were unchanged. `too-few` means fewer\n\
         than {} sessions on a side. Before/after is not causation: the model, the\n\
         Claude Code version and the kind of work also change over time, and changes\n\
         listed as \"also changed\" share whatever effect is seen.",
        params.min_sessions
    ));
    Ok(())
}

fn effect_json(e: &Effect) -> serde_json::Value {
    let window = |w: &crate::core::effect::Window| {
        serde_json::json!({
            "start_epoch": w.start,
            "end_epoch": w.end,
            "sessions": w.sessions,
            "tool_calls": w.tool_calls,
        })
    };
    let categories: Vec<serde_json::Value> = e
        .categories
        .iter()
        .map(|c| {
            serde_json::json!({
                "category": c.category,
                "before": c.before,
                "after": c.after,
                "before_per_session": c.before_per_session,
                "after_per_session": c.after_per_session,
                "before_per_100_calls": c.before_per_100_calls,
                "after_per_100_calls": c.after_per_100_calls,
                "p_value": c.p_value,
                "verdict": c.verdict.label(),
                "declared": c.declared,
                "flag": c.flag.map(Flag::label),
            })
        })
        .collect();
    serde_json::json!({
        "kind": e.change.kind,
        "id": e.change.id,
        "scope": e.change.scope.label(),
        "project": e.change.scope.project(),
        "changed_epoch": e.change.changed_epoch,
        "introduced_epoch": e.change.introduced_epoch,
        "date_source": e.change.date_source.label(),
        "targets": e.change.targets,
        "always_on": e.change.always_on,
        "unknown_targets": e.unknown_targets,
        "concurrent_changes": e.concurrent,
        "basis": e.basis.label(),
        "before": window(&e.before),
        "after": window(&e.after),
        "categories": categories,
    })
}

/// An epoch shown in the presentation zone (JST, as bucketing uses).
fn local_time(epoch: i64, pattern: &str) -> String {
    use chrono::TimeZone;
    chrono::FixedOffset::east_opt(JST_OFFSET_SECS)
        .and_then(|tz| tz.timestamp_opt(epoch, 0).single())
        .map_or_else(|| epoch.to_string(), |t| t.format(pattern).to_string())
}

/// A dimmed footnote under a table — context, not data.
fn note(text: &str) {
    println!("\n{}", Style::stdout().dim(text));
}

/// Where the work stumbles: recurring tool failures by category, ranked, each
/// with what it suggests fixing. This is about the work, not the config —
/// recurring failures are fixable friction that wastes turns and tokens.
/// `--scope` routes by ownership (`core::scope`): `global` shows the failures
/// no project owns, `project` each project's majority-owned failures, and
/// `project:<slug>` everything that happened in that one project.
fn failures(filter: &ScopeFilter, format: Format, frozen: bool, db: &Path) -> Result<()> {
    struct FrictionRow {
        project: Option<String>,
        category: String,
        count: i64,
        /// How many projects the count spreads across (global scope only).
        spread: Option<usize>,
    }

    let store = open_for_read(db, frozen)?;
    let cells = store.error_counts_by_project()?;

    let counts: Vec<FrictionRow> = match filter {
        ScopeFilter::All => store
            .error_counts()?
            .into_iter()
            .map(|(category, count)| FrictionRow {
                project: None,
                category,
                count,
                spread: None,
            })
            .collect(),
        ScopeFilter::Global => split_friction(&cells)
            .global
            .into_iter()
            .map(|g| FrictionRow {
                project: None,
                category: g.category,
                count: g.total,
                spread: Some(g.projects),
            })
            .collect(),
        ScopeFilter::Project(None) => split_friction(&cells)
            .per_project
            .into_iter()
            .flat_map(|(project, cats)| {
                cats.into_iter().map(move |(category, count)| FrictionRow {
                    project: Some(project.clone()),
                    category,
                    count,
                    spread: None,
                })
            })
            .collect(),
        ScopeFilter::Project(Some(slug)) => cells
            .into_iter()
            .filter(|(project, _, _)| project == slug)
            .map(|(project, category, count)| FrictionRow {
                project: Some(project),
                category,
                count,
                spread: None,
            })
            .collect(),
    };
    let total: i64 = counts.iter().map(|row| row.count).sum();
    let (retries, retries_after_denial) =
        retries_in_scope(&count_unsandboxed_retries(&store.sandboxed_runs()?), filter);

    if format == Format::Json {
        let failures: Vec<serde_json::Value> = counts
            .iter()
            .map(|row| {
                serde_json::json!({
                    "project": row.project,
                    "category": row.category,
                    "count": row.count,
                    "spread_projects": row.spread,
                    "suggestion": ErrorCategory::from_label(&row.category).suggestion(),
                })
            })
            .collect();
        return emit_json(&serde_json::json!({
            "total": total,
            "failures": failures,
            "unsandboxed_retries": {
                "total": retries,
                "after_sandbox_denial": retries_after_denial,
            },
        }));
    }
    let print_retries = || {
        if retries > 0 {
            println!(
                "\n{retries} Bash runs were retried with the sandbox disabled right after a \
                 sandboxed failure;\n{retries_after_denial} of them followed a failure classified \
                 as a sandbox refusal — those are what an\nallowlist entry in the sandbox settings \
                 would remove (the rest may have failed for other reasons)."
            );
        }
    };
    if total == 0 {
        // A check's failure can be read from its output alone, so retries can
        // exist in a scope that recorded no tool failure.
        println!("no tool failures found for this scope — run `cclens analyze` first?");
        print_retries();
        return Ok(());
    }

    preamble(
        format,
        match filter {
            ScopeFilter::All => {
                "Tool calls that kept failing the same way, grouped by kind of failure.\n\
                 Every row wastes turns and tokens; the suggestion says what usually fixes it."
            }
            ScopeFilter::Global => {
                "Failures spread across several projects — no single project owns them, so\n\
                 they read as habits or global-config problems. Fix these in ~/.claude."
            }
            ScopeFilter::Project(None) => {
                "Failures each project owns (it holds the majority of that kind). Fix each\n\
                 in that project's own CLAUDE.md / .claude, not globally."
            }
            ScopeFilter::Project(Some(_)) => {
                "Every recurring failure inside this one project, most frequent first.\n\
                 Fix them in this project's own CLAUDE.md / .claude."
            }
        },
    );
    let with_project = matches!(filter, ScopeFilter::Project(None));
    let rows: Vec<Vec<String>> = counts
        .iter()
        .map(|row| {
            let mut cells = Vec::new();
            if with_project {
                cells.push(row.project.clone().unwrap_or_default());
            }
            let error = match row.spread {
                Some(spread) => format!("{} (across {spread} projects)", row.category),
                None => row.category.clone(),
            };
            cells.extend([
                error,
                row.count.to_string(),
                ErrorCategory::from_label(&row.category)
                    .suggestion()
                    .to_string(),
            ]);
            cells
        })
        .collect();
    let (headers, aligns): (Vec<&str>, Vec<Align>) = if with_project {
        (
            vec!["project", "error", "count", "suggestion"],
            vec![Align::Left, Align::Left, Align::Right, Align::Left],
        )
    } else {
        (
            vec!["error", "count", "suggestion"],
            vec![Align::Left, Align::Right, Align::Left],
        )
    };
    render(&headers, &aligns, &rows, format);
    note(&format!(
        "{total} tool failures (denials from the transcript's own marker where present, \
         sandbox-network from the\nsandbox's own report; everything else — sandbox-write \
         included — is a lexical heuristic)"
    ));
    print_retries();
    Ok(())
}

/// Show the mix of how the user steers the session, and what it suggests. Heavy
/// steering points to room for more autonomy; frequent corrections point to
/// clearer upfront specs. The classes are lexical heuristics (`core::prompt`).
fn prompts(format: Format, frozen: bool, db: &Path) -> Result<()> {
    let store = open_for_read(db, frozen)?;
    let counts = store.prompt_behavior_counts()?;
    let total: i64 = counts.iter().map(|(_, n)| n).sum();
    if format == Format::Json {
        let behaviors: Vec<serde_json::Value> = counts
            .iter()
            .map(|(behavior, n)| {
                serde_json::json!({
                    "behavior": behavior,
                    "count": n,
                    "share_pct": if total > 0 {
                        (*n as f64 * 100.0 / total as f64).round() as i64
                    } else {
                        0
                    },
                })
            })
            .collect();
        return emit_json(&serde_json::json!({ "total": total, "behaviors": behaviors }));
    }
    if total == 0 {
        println!("no prompts found — run `cclens analyze` first");
        return Ok(());
    }

    preamble(
        format,
        "How you steer Claude, classified per prompt: instruct (substantive asks),\n\
         steer (\"go ahead\" / \"yes\"), correct (\"no, instead…\"), question. Lots of\n\
         steering = room for more autonomy; lots of correcting = unclear upfront specs.",
    );
    let pct = |n: i64| (n as f64 * 100.0 / total as f64).round() as i64;
    let get = |name: &str| {
        counts
            .iter()
            .find(|(b, _)| b == name)
            .map_or(0, |(_, n)| *n)
    };

    let rows: Vec<Vec<String>> = counts
        .iter()
        .map(|(behavior, n)| vec![behavior.clone(), n.to_string(), format!("{}%", pct(*n))])
        .collect();
    render(
        &["behavior", "count", "share"],
        &[Align::Left, Align::Right, Align::Right],
        &rows,
        format,
    );

    let style = Style::stdout();
    note(&format!(
        "{total} prompts (behavioral classes are lexical heuristics)"
    ));
    let steer = pct(get("steer"));
    let correct = pct(get("correct"));
    let mut flagged = false;
    if steer >= 25 {
        flagged = true;
        println!(
            "  - {}",
            style.warn(&format!(
                "{steer}% steering (\"go ahead\" / \"yes\" / \"next\"): you approve in small \
                 steps — room for more autonomy (clearer upfront scope, /loop, longer leash)."
            ))
        );
    }
    if correct >= 10 {
        flagged = true;
        println!(
            "  - {}",
            style.warn(&format!(
                "{correct}% corrections (\"no\" / \"instead\" / \"戻して\"): rework after a \
                 wrong turn — tighter initial specs or rules could cut it."
            ))
        );
    }
    if !flagged {
        println!(
            "  - {}",
            style.good(&format!(
                "healthy mix: mostly substantive instructions, low correction ({correct}%) — \
                 good alignment, no obvious babysitting or rework problem."
            ))
        );
    }
    Ok(())
}

/// Reconcile the empirical always-on floor against readable config. The floor is
/// what every session actually starts with; subtracting the config we can read
/// leaves the residual — system prompt, built-in tools, and MCP tool schemas the
/// catalog cannot weigh. The per-project floors let you infer an MCP server's
/// real cost by comparing a project that enables it against one that does not.
fn overhead(format: Format, frozen: bool, db: &Path) -> Result<()> {
    let store = open_for_read(db, frozen)?;
    let floor = store.baseline_floor()?;
    let config = store.always_on_config_tokens()?;
    let residual = floor.map(|floor| (floor - config).max(0));
    let per_project = store.baseline_floor_per_project()?;
    let decomposition = store.latest_decomposition()?;
    let mcp_servers = store.measured_mcp_startup()?;
    let tool_wedges = tool_load_wedges(&store.tool_load_stats()?, TOOL_LOAD_THRESHOLDS);

    if format == Format::Json {
        let per_project: Vec<serde_json::Value> = per_project
            .iter()
            .map(|(project, floor, sessions)| {
                serde_json::json!({
                    "project": project,
                    "floor": floor,
                    "sessions": sessions,
                })
            })
            .collect();
        return emit_json(&serde_json::json!({
            "floor": floor,
            "config_tokens": config,
            "residual": residual,
            "per_project": per_project,
            "decomposition": decomposition,
            "mcp_servers": mcp_servers,
            "tool_wedges": tool_wedges_json(&tool_wedges),
        }));
    }
    // No session start observed — say so rather than substituting a mid-session
    // context that would read as a startup cost.
    let Some(floor) = floor else {
        let (sessions, _) = store.session_stats()?;
        if sessions == 0 {
            println!("no data — run `cclens analyze` first");
        } else {
            println!(
                "always-on floor unavailable — none of the {sessions} analyzed session(s)\n\
                 carries an observable session start. A store written before session-start\n\
                 tracking keeps its old rows (analyze never re-reads a closed session), so\n\
                 the floor appears as newer sessions are analyzed. Discarding the store\n\
                 would backfill it from the transcripts still on disk, at the cost of every\n\
                 session whose transcript Claude Code has already pruned."
            );
        }
        return Ok(());
    };
    let residual = (floor - config).max(0);

    preamble(
        format,
        "What every session pays in context before any work starts, and how much of\n\
         it your own config files account for.",
    );
    let style = Style::stdout();
    println!(
        "Observed always-on floor (leanest session-start context): {}",
        style.bold(&format!("{floor} tokens"))
    );
    println!(
        "  readable config (CLAUDE.md, always-on rules, skill/agent descriptions): {config} tokens"
    );
    println!(
        "  residual (system prompt + built-in tools + MCP schemas):                {residual} tokens"
    );
    println!(
        "{}",
        style.dim(
            "  -> the residual is what you cannot read from files; compare projects below\n\
             \x20    to see an MCP server's marginal cost (a project that enables it starts higher)."
        )
    );
    println!();

    let rows: Vec<Vec<String>> = per_project
        .into_iter()
        .map(|(project, floor, sessions)| vec![project, floor.to_string(), sessions.to_string()])
        .collect();
    render(
        &["project", "floor_tokens", "sessions"],
        &[Align::Left, Align::Right, Align::Right],
        &rows,
        format,
    );
    note(
        "each floor is the leanest start across that project's sessions — a project with\n\
         few sessions may not have caught a fresh one, so compare like counts.",
    );
    print_decomposition(decomposition.as_ref());
    print_mcp_servers(&mcp_servers, format);
    print_tool_wedges(&tool_wedges, format);
    Ok(())
}

/// The residual split into what the latest recorded request shape says it is
/// made of, beside that session's observed start (`docs/specs/surfaces.md`).
fn print_decomposition(decomposition: Option<&Decomposition>) {
    let style = Style::stdout();
    println!();
    let Some(d) = decomposition else {
        println!(
            "{}",
            style.dim(
                "residual breakdown unavailable — no analyzed session recorded its request\n\
                 shape (Claude Code releases before the shape records). It appears as newer\n\
                 sessions are analyzed."
            )
        );
        return;
    };
    println!(
        "{}",
        style.heading(&format!(
            "Latest session start, broken down from its request shape ({}, {})",
            d.started_at, d.project
        ))
    );
    let components = [
        ("system prompt", d.system_tokens),
        ("built-in tools (eager schemas)", d.builtin_tool_tokens),
        ("MCP tools (eager schemas)", d.mcp_tool_tokens),
        ("deferred tool listing", d.deferred_listing_tokens),
        ("MCP server instructions", d.mcp_instruction_tokens),
        ("readable config", d.config_tokens),
    ];
    for (label, tokens) in components {
        println!("  {label:<46} {tokens:>8} tokens");
    }
    if let Some(observed) = d.observed_ctx {
        let measured: i64 = components.iter().map(|(_, tokens)| tokens).sum();
        println!("  {:<46} {:>8} tokens", "observed start", observed);
        println!(
            "  {:<46} {:>8} tokens",
            "remainder (opening prompt, reminders, estimate)",
            observed - measured
        );
    }
    println!(
        "{}",
        style.dim(
            "  -> estimates (~4 chars/token), counted once per session; listings and\n\
             \x20    instructions of servers that connect later are included."
        )
    );
}

/// Each MCP server's measured startup cost against how much it was used.
fn print_mcp_servers(servers: &[McpServerCost], format: Format) {
    if servers.is_empty() {
        return;
    }
    println!();
    println!(
        "{}",
        Style::stdout().heading("MCP servers: what each costs at startup vs how often it is used")
    );
    let rows: Vec<Vec<String>> = servers
        .iter()
        .map(|server| {
            vec![
                server.server.clone(),
                server.startup_tokens.to_string(),
                server.sessions.to_string(),
                server.calls.to_string(),
            ]
        })
        .collect();
    render(
        &["server", "startup_tokens", "sessions", "calls"],
        &[Align::Left, Align::Right, Align::Right, Align::Right],
        &rows,
        format,
    );
    note(
        "startup_tokens is the latest session's eager schemas + listing lines + instructions;\n\
         a server with many sessions and few calls is paid for far more than it is used.",
    );
}

fn tool_wedge_evidence(wedge: &ToolLoadWedge) -> (String, &'static str) {
    let stat = &wedge.stat;
    match wedge.kind {
        ToolLoadWedgeKind::DeferCandidate => (
            format!(
                "eager in {} sessions, called in {} (~{} tok/session)",
                stat.eager_sessions, stat.eager_sessions_called, stat.eager_tokens
            ),
            "defer it: its schema is then paid only when a tool search loads it",
        ),
        ToolLoadWedgeKind::EagerCandidate => (
            format!(
                "deferred in {} sessions, loaded in {}",
                stat.deferred_sessions, stat.deferred_sessions_loaded
            ),
            "load it eagerly: a tool-search turn is paid in nearly every session",
        ),
    }
}

fn tool_wedges_json(wedges: &[ToolLoadWedge]) -> Vec<serde_json::Value> {
    wedges
        .iter()
        .map(|wedge| {
            let (_, suggestion) = tool_wedge_evidence(wedge);
            serde_json::json!({
                "wedge": wedge.kind.label(),
                "tool": wedge.stat,
                "suggestion": suggestion,
            })
        })
        .collect()
}

/// Tools whose loading mode fights their use (`core::tool_load`).
fn print_tool_wedges(wedges: &[ToolLoadWedge], format: Format) {
    if wedges.is_empty() {
        return;
    }
    println!();
    println!(
        "{}",
        Style::stdout().heading("Tools loaded the wrong way for how they are used")
    );
    let rows: Vec<Vec<String>> = wedges
        .iter()
        .map(|wedge| {
            let (evidence, suggestion) = tool_wedge_evidence(wedge);
            vec![
                wedge.kind.label().to_string(),
                wedge.stat.name.clone(),
                evidence,
                suggestion.to_string(),
            ]
        })
        .collect();
    render(
        &["problem", "tool", "evidence", "suggestion"],
        &[Align::Left, Align::Left, Align::Left, Align::Left],
        &rows,
        format,
    );
}

/// Static-token threshold above which an always-on or rarely-used surface counts
/// as "heavy" — a tuning knob, not a hard rule.
const HEAVY_TOKENS: u64 = 800;

/// Counters from one analyze run, for the command's summary line and the
/// freshness header on auto-refreshing reads.
struct AnalyzeStats {
    sessions: usize,
    skipped: usize,
    spans_total: usize,
    surface_count: usize,
    denials: usize,
}

fn analyze(projects: Option<PathBuf>, format: Format, db: &Path) -> Result<()> {
    let stats = run_analyze(projects, db)?;
    let store = Store::open(db).context("open store")?;
    let (sub_tokens, sub_agents) = store.subagent_totals()?;
    if format == Format::Json {
        return emit_json(&serde_json::json!({
            "sessions": stats.sessions,
            "unchanged": stats.skipped,
            "skill_invocations": stats.spans_total,
            "surfaces": stats.surface_count,
            "subagent_tokens": sub_tokens,
            "subagents": sub_agents,
            "permission_denials": stats.denials,
            "db": db.display().to_string(),
        }));
    }
    println!(
        "analyzed {} session(s) ({} unchanged), {} skill invocation(s), \
         {} surface(s) catalogued, \
         {sub_tokens} subagent tokens across {sub_agents} subagent(s), \
         {} permission denial(s) -> {}",
        stats.sessions,
        stats.skipped,
        stats.spans_total,
        stats.surface_count,
        stats.denials,
        db.display()
    );
    Ok(())
}

fn run_analyze(projects: Option<PathBuf>, db: &Path) -> Result<AnalyzeStats> {
    let projects = projects.map(Ok).unwrap_or_else(default_projects_dir)?;
    let mut store = Store::open(db).context("open store")?;

    // Rows produced by older analysis semantics are rebuilt once: the
    // fingerprint skip below would otherwise pin them forever, because the
    // transcripts they came from will never change again.
    let reuse_ingested = store.is_analyzer_current()?;

    let mut sessions = 0;
    let mut skipped = 0;
    let mut spans_total = 0;
    let mut denials = 0;
    for transcript in main_transcripts(&projects)? {
        let path_str = transcript.display().to_string();
        // Incremental skip: an unchanged (mtime, size) was already ingested.
        // The stat is taken before the read so a file that grows concurrently
        // is guaranteed a mismatch — and a re-ingest — next run
        // (`docs/specs/storage.md`).
        let fingerprint = file_fingerprint(&transcript);
        if reuse_ingested
            && let Some((mtime, size)) = fingerprint
            && store.is_ingested(&path_str, mtime, size)?
        {
            skipped += 1;
            continue;
        }
        let text = fs::read_to_string(&transcript)
            .with_context(|| format!("read {}", transcript.display()))?;
        denials += count_permission_denials(&text);
        let records = parse_session(&text);
        let mut spans = extract_spans(&records, DEFAULT_IDLE_GAP_MS);
        let usage = extract_usage_events(&records);
        let (runs, subagent_hooks) = subagent_runs(&transcript);
        attribute_subagents(&mut spans, &runs);
        let sub_tokens: i64 = runs.iter().map(|run| run.out_tokens as i64).sum();
        let root = session_cwd(&text).map(|cwd| normalize_root(&cwd));
        let meta = session_meta(
            &transcript,
            root.unwrap_or_default(),
            sub_tokens,
            runs.len() as i64,
            session_start(&records),
        );
        store.ingest_session(&meta, &spans, &usage)?;
        store.set_session_tool_calls(&meta.id, count_tool_calls(&text))?;
        store.ingest_subagent_runs(&meta.id, &meta.source_path, &runs)?;
        let prompts: Vec<(usize, i64, &str)> = extract_prompt_pointers(&text)
            .into_iter()
            .map(|(line, ts, behavior)| (line, ts, behavior.label()))
            .collect();
        store.ingest_prompts(&meta.id, &meta.source_path, &prompts)?;
        // `raw_errors` owns the strings; `error_rows` borrows them.
        let raw_errors = extract_tool_errors(&text);
        let error_rows: Vec<(i64, &str, &str, &str, &str)> = raw_errors
            .iter()
            .map(|e| {
                (
                    e.epoch_ms,
                    e.category.label(),
                    e.excerpt.as_str(),
                    e.tool.as_str(),
                    e.target.as_str(),
                )
            })
            .collect();
        store.ingest_tool_errors(&meta.id, &meta.source_path, &error_rows)?;
        // `work` owns the strings; the rows borrow them.
        let work = extract_work_events(&text);
        let checks: Vec<Option<String>> = work
            .iter()
            .map(|e| e.bash.as_ref().and_then(|run| check_key(&run.command)))
            .collect();
        let work_rows: Vec<WorkRow> = work
            .iter()
            .zip(&checks)
            .map(|(e, check)| match &e.bash {
                Some(run) => {
                    let row = WorkRow::bash(
                        e.epoch_ms,
                        &e.id,
                        run.outcome.map(BashOutcome::label),
                        run.unsandboxed,
                    );
                    match check {
                        Some(key) => row.checked(
                            key,
                            check_outcome(&run.command, run.outcome, &run.output)
                                .map(BashOutcome::label),
                        ),
                        None => row,
                    }
                }
                None => WorkRow::file_edit(e.epoch_ms, &e.id, e.path.as_deref()),
            })
            .collect();
        store.ingest_work_events(&meta.id, &meta.source_path, &work_rows)?;
        // The main thread and every subagent: hooks fire in both. A block's cost
        // is measured within the transcript it happened in.
        let activities: Vec<HookActivity> = std::iter::once(extract_hook_activity(&text))
            .chain(subagent_hooks)
            .collect();
        let hook_rows: Vec<(HookTrace, Option<u32>)> = activities
            .iter()
            .flat_map(|activity| {
                activity
                    .traces
                    .iter()
                    .map(|trace| (trace.clone(), block_cost(trace, &activity.steps)))
            })
            .collect();
        let calls: Vec<(i64, &str, CallResult)> = activities
            .iter()
            .flat_map(|activity| &activity.steps)
            .filter_map(|step| match step {
                Step::Call {
                    epoch_ms,
                    tool,
                    result,
                    ..
                } => Some((*epoch_ms, tool.as_str(), *result)),
                Step::HumanTurn => None,
            })
            .collect();
        store.ingest_hook_activity(&meta.id, &meta.source_path, &hook_rows, &calls)?;
        store.ingest_context_loads(
            &meta.id,
            &meta.source_path,
            &session_context_loads(&transcript, &text),
        )?;
        store.ingest_tool_context(&meta.id, &meta.source_path, &extract_tool_context(&text))?;
        let prompt_cache = extract_prompt_cache(&text);
        store.ingest_prompt_cache(
            &meta.id,
            &meta.source_path,
            &observe_cache(&prompt_cache.trace, DEFAULT_MIN_REUSE),
            prompt_cache.reported_cost_usd,
        )?;
        if let Some((mtime, _)) = fingerprint {
            // Record the size actually consumed, so a tail appended after the
            // read shows a mismatch next run instead of being skipped forever.
            store.record_ingested_file(&path_str, mtime, text.len() as i64)?;
        }
        sessions += 1;
        spans_total += spans.len();
    }

    // Catalog the installed config — global scope plus every known project root
    // that still exists on disk — so usage joins against what is installed.
    let config_dir = claude_home()?;
    let mut surfaces = read_global_surfaces()?;
    for (root, project) in store.session_roots()? {
        if Path::new(&root).is_dir() {
            surfaces.extend(read_project_surfaces(
                Path::new(&root),
                &project,
                home_dir().map(Path::new),
                Some(&config_dir),
            ));
        }
    }
    let surface_count = surfaces.len();
    store.replace_surfaces(&surfaces)?;
    store.replace_surface_changes(&config_changes(&surfaces))?;

    store.record_analyzer_version()?;
    store.set_meta("analyzed_at", &chrono::Utc::now().to_rfc3339())?;
    store.set_meta("projects_dir", &projects.display().to_string())?;
    store.set_meta("config_dir", &config_dir.display().to_string())?;

    Ok(AnalyzeStats {
        sessions,
        skipped,
        spans_total,
        surface_count,
        denials,
    })
}

/// Open the store for a read command. Unless `--frozen`, the analysis is
/// refreshed first — incremental, so an up-to-date store costs one stat per
/// transcript — which makes stale reads structurally impossible (`optimize` has
/// always worked this way; the views now compose the same stage). A one-line
/// freshness header goes to **stderr** so piped stdout stays clean.
fn open_for_read(db: &Path, frozen: bool) -> Result<Store> {
    if !frozen {
        // Re-analyze against the roots the store was built from (recorded in
        // meta); a fresh db falls back to the defaults.
        let projects = match db.exists() {
            true => Store::open(db)
                .context("open store")?
                .meta("projects_dir")?
                .map(PathBuf::from),
            false => None,
        };
        let stats = run_analyze(projects, db)?;
        eprintln!(
            "{}",
            Style::stderr().dim(&format!(
                "store: refreshed just now ({} transcript(s) re-analyzed, {} unchanged)",
                stats.sessions, stats.skipped
            ))
        );
        return Store::open(db).context("open store");
    }

    let store = Store::open(db).context("open store")?;
    eprint_freshness(&freshness_line(store.meta("analyzed_at")?.as_deref()));
    Ok(store)
}

/// Warn when a report knows a subagent total but not its per-agent split.
///
/// A store written before runs were extracted keeps its session totals, and
/// `--frozen` skips the refresh that would fill the split in — so for that one
/// read the per-agent rows sum to zero against a nonzero total. Reporting the
/// gap is the store's contract; printing the total as if nothing were missing
/// is the substitution it forbids (`docs/specs/storage.md`). It goes to stderr
/// with the other staleness signals, so piped stdout and `--format json` stay
/// clean.
///
/// A *partial* split is not warned about here: it is a property of the data
/// that no refresh can change, so each report labels it in-band beside the
/// rows it qualifies (`SplitCoverage::partial_note`).
fn warn_missing_agent_split(coverage: &SplitCoverage) {
    if coverage.is_unavailable() {
        eprintln!(
            "{}",
            Style::stderr().warn(
                "store: per-agent subagent split unavailable (store predates it) \
                 — run `cclens analyze` (or drop --frozen) to fill it in"
            )
        );
    }
}

/// Print a freshness line to stderr — dimmed context normally, highlighted
/// when it carries a staleness hint the user should act on.
fn eprint_freshness(line: &str) {
    let style = Style::stderr();
    if line.contains("cclens analyze") {
        eprintln!("{}", style.warn(line));
    } else {
        eprintln!("{}", style.dim(line));
    }
}

/// The `--frozen` freshness header: how old the store is, with a refresh hint
/// once it is older than a day. This is what lets the user *notice* staleness.
fn freshness_line(analyzed_at: Option<&str>) -> String {
    let Some(parsed) = analyzed_at.and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok())
    else {
        return "store: freshness unknown (no analyze recorded) — run `cclens analyze`".to_string();
    };
    let age_secs = (chrono::Utc::now() - parsed.with_timezone(&chrono::Utc)).num_seconds();
    let hint = if age_secs >= 24 * 3600 {
        " — run `cclens analyze` (or drop --frozen) to refresh"
    } else {
        ""
    };
    format!(
        "store: analyzed {} ago (--frozen){hint}",
        humanize_age(age_secs)
    )
}

fn humanize_age(secs: i64) -> String {
    let secs = secs.max(0);
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m", secs / 60),
        3600..86400 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86400),
    }
}

/// A file's `(mtime epoch secs, size)` — the cheap change detector for
/// incremental ingest. `None` when the file cannot be stat'ed (it is then
/// ingested unconditionally and not recorded).
fn file_fingerprint(path: &Path) -> Option<(i64, i64)> {
    let meta = fs::metadata(path).ok()?;
    let mtime = meta
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs() as i64;
    Some((mtime, meta.len() as i64))
}

/// Fold a worktree checkout's path onto its parent repository, mirroring
/// `normalize_project`'s slug rule at path level: `/repo/.wt/feat-x` → `/repo`.
///
/// Either separator delimits the segment, so a cwd recorded on Windows
/// (`C:\repo\.wt\feat-x`) folds like a unix one. The slug rule keys on `--wt-`
/// and is separator-free, so without this the two halves of the same
/// normalization disagreed and a project folded by slug stayed split by root.
fn normalize_root(cwd: &str) -> String {
    const SEGMENT: &str = ".wt";
    for (idx, _) in cwd.match_indices(SEGMENT) {
        let has_parent = cwd[..idx].ends_with(['/', '\\']);
        let has_child = cwd[idx + SEGMENT.len()..].starts_with(['/', '\\']);
        if has_parent && has_child {
            // `idx` is at least 1 here, and the separator before it is one byte.
            let parent = &cwd[..idx - 1];
            // Directly under a filesystem root there is no parent to fold onto:
            // the separator *is* the root, so dropping it would turn `/.wt/x`
            // into `""` and `C:\.wt\x` into a drive-relative `C:`. A drive
            // prefix is exactly two bytes, which keeps an ordinary directory
            // whose name ends in a colon out of this branch.
            if parent.is_empty() || (parent.len() == 2 && parent.ends_with(':')) {
                return cwd[..idx].to_string();
            }
            return parent.to_string();
        }
    }
    cwd.to_string()
}

/// Every subagent run of a session, read from `<sessionId>/subagents/` beside
/// the main transcript: `agent-<agentId>.jsonl` holds the run's cost, and its
/// `.meta.json` sidecar names the agent type and the call that spawned it.
///
/// Runs come back linked (`link_runs`), so a nested run already knows the
/// main-thread spawn its tree started at. A run whose sidecar is missing keeps
/// an unknown agent type and only the turn-level join key — it still counts,
/// with the gap visible, rather than being guessed at or dropped.
///
/// Each run's hook activity comes back beside the runs: hooks fire inside
/// subagents too, so their calls are triggers and their traces are uses of the
/// same configured hooks (`docs/specs/surfaces.md`).
fn subagent_runs(transcript: &Path) -> (Vec<SubagentRun>, Vec<HookActivity>) {
    let Ok(entries) = fs::read_dir(subagents_dir(transcript)) else {
        return (Vec::new(), Vec::new());
    };
    let mut runs = Vec::new();
    let mut hooks = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "jsonl") {
            continue;
        }
        let Ok(text) = fs::read_to_string(&path) else {
            continue;
        };
        let sidecar = fs::read_to_string(subagent_sidecar_path(&path))
            .ok()
            .and_then(|json| parse_subagent_sidecar(&json))
            .unwrap_or_default();
        hooks.push(extract_hook_activity(&text));
        let records = parse_session(&text);
        runs.push(SubagentRun {
            agent: sidecar.agent_type,
            agent_id: path
                .file_stem()
                .map(|stem| subagent_id_from_file_name(&stem.to_string_lossy()))
                .unwrap_or_default(),
            run_path: path.display().to_string(),
            tool_use_id: sidecar.tool_use_id,
            parent_agent_id: sidecar.parent_agent_id,
            spawn_depth: sidecar.spawn_depth.unwrap_or(1),
            model: run_model(&records),
            requested_model: sidecar.requested_model,
            prompt_id: subagent_prompt_id(&text),
            out_tokens: output_tokens(&records),
            prompt_tokens: prompt_tokens(&text),
            started_epoch_ms: records.first().map(|r| r.timestamp_ms).unwrap_or_default(),
            root_tool_use_id: None,
        });
    }
    // Directory order is filesystem-dependent; spawn order is not. Sorting keeps
    // one session's ingest byte-identical across machines and re-runs.
    runs.sort_by(|a, b| {
        a.started_epoch_ms
            .cmp(&b.started_epoch_ms)
            .then_with(|| a.agent_id.cmp(&b.agent_id))
    });
    link_runs(&mut runs);
    (runs, hooks)
}

/// Fold a file loaded inside a worktree checkout onto the same file in the
/// parent checkout, as `normalize_root` folds the session itself: the session
/// is the parent project's, its catalog is scanned at the parent's root, and a
/// worktree's copy of a rule is that rule. Left unfolded, every load of it
/// would miss the catalog while the session still counted as observed, and the
/// rule would read as never firing. A path outside the worktree, or a cwd in
/// no worktree, is returned as is.
fn fold_worktree_path(path: &str, cwd: &str) -> String {
    let root = normalize_root(cwd);
    if root == cwd || root.is_empty() {
        return path.to_string();
    }
    // The checkout is the root plus two whole segments: `.wt` and the
    // worktree's own name. Cut at segment boundaries — a substring search for
    // the name would match inside `.wt` itself for a worktree named `wt`.
    let after_root = &cwd[root.len()..];
    let mut end = 0;
    for _ in 0..2 {
        let rest = &after_root[end..];
        let start = rest.len() - rest.trim_start_matches(['/', '\\']).len();
        let len = rest[start..]
            .find(['/', '\\'])
            .unwrap_or(rest.len() - start);
        if len == 0 {
            return path.to_string();
        }
        end += start + len;
    }
    let checkout = &cwd[..root.len() + end];
    match path.strip_prefix(checkout) {
        Some(rest) if rest.is_empty() || rest.starts_with(['/', '\\']) => format!("{root}{rest}"),
        _ => path.to_string(),
    }
}

/// Every config-file load of a session: its own transcript's, plus every
/// subagent transcript's — a rule can fire only inside a subagent, and that is
/// still this session's work (`docs/specs/events.md`). A load inside a
/// worktree is folded onto the parent checkout (`fold_worktree_path`), then
/// canonicalized like the catalog's, so the join holds even if a
/// transcript recorded a linked path; a file that no longer exists keeps the
/// path as recorded.
fn session_context_loads(transcript: &Path, text: &str) -> Vec<ContextLoad> {
    let mut loads = own_context_loads(text);
    if let Ok(entries) = fs::read_dir(subagents_dir(transcript)) {
        let mut runs: Vec<PathBuf> = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
            .collect();
        runs.sort();
        for run in runs {
            if let Ok(run_text) = fs::read_to_string(&run) {
                loads.extend(own_context_loads(&run_text));
            }
        }
    }
    for load in &mut loads {
        if let Some(resolved) = canonical_path(&load.path) {
            load.path = resolved;
        }
    }
    loads
}

/// One transcript's loads, folded against that transcript's own cwd: a
/// subagent can run in a worktree of its own, apart from its parent's.
fn own_context_loads(text: &str) -> Vec<ContextLoad> {
    let mut loads = extract_context_loads(text);
    if let Some(cwd) = session_cwd(text) {
        for load in &mut loads {
            load.path = fold_worktree_path(&load.path, &cwd);
        }
    }
    loads
}

fn usage(by: Option<&str>, format: Format, frozen: bool, db: &Path) -> Result<()> {
    let store = open_for_read(db, frozen)?;

    if let Some(by) = by {
        let bucket = Bucket::parse(by)
            .with_context(|| format!("unknown --by value '{by}' (year|month|week|day|hour)"))?;
        return usage_by_time(&store, bucket, format);
    }

    let skills = store.skill_usage()?;
    // Where the tokens actually go: main-thread skill output vs subagents. The
    // subagent figure is usually the larger by far — worth seeing before reading
    // the per-skill table.
    let main_out: i64 = skills.iter().map(|row| row.out_tokens).sum();
    let (sub_tokens, sub_agents) = store.subagent_totals()?;
    let agents = store.subagent_by_agent()?;
    // How much of the total the rows actually account for: a store that kept
    // totals for sessions whose transcripts were pruned before their runs were
    // extracted has rows for only part of it, and the rows must not pass for
    // the whole. It travels with the rows in every format.
    let agent_split = SplitCoverage::new(
        sub_tokens,
        sub_agents,
        agents.iter().map(|a| (a.runs, a.out_tokens)),
    );
    // Ahead of the format branches: a machine consumer reading an empty `agents`
    // beside a nonzero total needs the reason as much as a human does, and it
    // goes to stderr precisely so it can be given to both.
    warn_missing_agent_split(&agent_split);
    // Which tier the subagent tokens were spent on, and who picked it: the
    // per-agent split says whose cost it is, these say what a model choice
    // would change (`docs/specs/events.md` "Model choice at spawn").
    let agent_models = store.subagent_by_agent_model()?;
    let spawns = store.spawn_models()?;
    let spawn_models = model_choice_by_agent(&spawns);
    let main_models = caller_models(&spawns);
    if format == Format::Json {
        return emit_json(&serde_json::json!({
            "tokens": {
                "main_out": main_out,
                "sub_tokens": sub_tokens,
                "sub_agents": sub_agents,
            },
            "agent_split": agent_split,
            "agents": agents,
            "agent_models": agent_models,
            "spawn_models": spawn_models,
            "main_models": main_models,
            "skills": skills,
        }));
    }
    // A subagent total with neither table behind it is not "nothing happened" —
    // it is a store whose per-agent split has not been extracted yet, and saying
    // "no usage" there would be false.
    // Spawns count too: a spawn whose run was pruned still says who chose its model.
    if skills.is_empty() && agents.is_empty() && sub_tokens == 0 && spawn_models.is_empty() {
        println!("no skill usage found — run `cclens analyze` first");
        return Ok(());
    }
    preamble(
        format,
        "What ran and what it cost. First the subagent bill per agent type\n\
         (costliest first), then how often each skill ran — output tokens written,\n\
         context growth while it ran, and wall-clock seconds (most-invoked first).\n\
         A section with nothing to show is omitted.",
    );
    println!(
        "tokens: main-thread skill output {main_out}, subagents {sub_tokens} ({sub_agents} agents)"
    );
    // Stated between the total and the table, so the table below is read as
    // the part it is.
    if let Some(note) = agent_split.partial_note() {
        println!("{note}");
    }
    println!();

    // The subagent figure is usually the larger one, so break it down by agent
    // type before the per-skill table: a spawn count alone cannot tell an
    // expensive agent from a cheap one that runs often.
    if !agents.is_empty() {
        let rows: Vec<Vec<String>> = agents
            .iter()
            .map(|row| {
                vec![
                    row.agent.clone().unwrap_or_else(|| "(unknown)".to_string()),
                    row.runs.to_string(),
                    row.out_tokens.to_string(),
                ]
            })
            .collect();
        render(
            &["agent", "runs", "out tokens"],
            &[Align::Left, Align::Right, Align::Right],
            &rows,
            format,
        );
        println!();
    }
    print_model_mix(&agent_models, &spawn_models, &main_models, format);

    if skills.is_empty() {
        return Ok(());
    }
    let rows: Vec<Vec<String>> = skills
        .iter()
        .map(|row| {
            vec![
                row.skill.clone(),
                row.invocations.to_string(),
                row.out_tokens.to_string(),
                row.ctx_growth.to_string(),
                format!("{:.0}", row.duration_sec),
            ]
        })
        .collect();
    render(
        &["skill", "runs", "out tokens", "ctx growth", "seconds"],
        &[
            Align::Left,
            Align::Right,
            Align::Right,
            Align::Right,
            Align::Right,
        ],
        &rows,
        format,
    );
    Ok(())
}

/// The subagent model tables of `usage`: what each agent type ran on, then who
/// chose that model at spawn, read against the main loop's own models.
fn print_model_mix(
    agent_models: &[AgentModelCost],
    spawn_models: &[AgentModelChoice],
    main_models: &[(String, i64)],
    format: Format,
) {
    let unknown = |value: &Option<String>| value.clone().unwrap_or_else(|| "(unknown)".to_string());
    if !main_models.is_empty() {
        println!(
            "main loop models (per subagent spawn): {}",
            tally_list(main_models)
        );
        println!();
    }
    if !agent_models.is_empty() {
        let rows: Vec<Vec<String>> = agent_models
            .iter()
            .map(|row| {
                vec![
                    unknown(&row.agent),
                    unknown(&row.model),
                    row.runs.to_string(),
                    row.out_tokens.to_string(),
                    row.prompt_tokens
                        .map_or_else(|| "-".to_string(), |tokens| tokens.to_string()),
                ]
            })
            .collect();
        render(
            &["agent", "ran on", "runs", "out tokens", "prompt tokens"],
            &[
                Align::Left,
                Align::Left,
                Align::Right,
                Align::Right,
                Align::Right,
            ],
            &rows,
            format,
        );
        println!();
    }
    if !spawn_models.is_empty() {
        let rows: Vec<Vec<String>> = spawn_models
            .iter()
            .map(|row| {
                vec![
                    row.agent.clone(),
                    row.spawns.to_string(),
                    row.explicit.to_string(),
                    row.inherited.to_string(),
                    row.agent_default.to_string(),
                    row.unknown.to_string(),
                    tally_list(&row.requested),
                ]
            })
            .collect();
        render(
            &[
                "agent",
                "spawns",
                ModelChoice::Explicit.label(),
                ModelChoice::Inherited.label(),
                ModelChoice::AgentDefault.label(),
                ModelChoice::Unknown.label(),
                "requested",
            ],
            &[
                Align::Left,
                Align::Right,
                Align::Right,
                Align::Right,
                Align::Right,
                Align::Right,
                Align::Left,
            ],
            &rows,
            format,
        );
        println!();
    }
}

/// `name ×count` pairs, comma-separated; `-` for none.
fn tally_list(counts: &[(String, i64)]) -> String {
    if counts.is_empty() {
        return "-".to_string();
    }
    counts
        .iter()
        .map(|(name, count)| format!("{name} ×{count}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Skill usage rolled up per time bucket (JST). A long span is assigned whole to
/// its start bucket (`docs/specs/cli.md`).
fn usage_by_time(store: &Store, bucket: Bucket, format: Format) -> Result<()> {
    let events = store.skill_event_costs()?;

    let mut totals: std::collections::BTreeMap<String, (i64, i64, i64, f64)> =
        std::collections::BTreeMap::new();
    for event in &events {
        let label = bucket_label(event.started_epoch, bucket, JST_OFFSET_SECS);
        let row = totals.entry(label).or_default();
        row.0 += 1;
        row.1 += event.out_tokens;
        row.2 += event.ctx_growth;
        row.3 += event.duration_sec;
    }

    if format == Format::Json {
        let buckets: Vec<serde_json::Value> = totals
            .iter()
            .map(|(label, (count, out_tokens, ctx_growth, duration_sec))| {
                serde_json::json!({
                    "bucket": label,
                    "count": count,
                    "out_tokens": out_tokens,
                    "ctx_growth": ctx_growth,
                    "duration_sec": duration_sec,
                })
            })
            .collect();
        return emit_json(&serde_json::json!({ "buckets": buckets }));
    }
    if events.is_empty() {
        println!("no skill usage found — run `cclens analyze` first");
        return Ok(());
    }

    preamble(
        format,
        "Skill activity per time bucket (times in JST). A long-running skill counts\n\
         whole in the bucket it started in, so fine buckets can show spikes.",
    );
    let rows: Vec<Vec<String>> = totals
        .into_iter()
        .map(|(label, (count, out_tokens, ctx_growth, duration_sec))| {
            vec![
                label,
                count.to_string(),
                out_tokens.to_string(),
                ctx_growth.to_string(),
                format!("{duration_sec:.0}"),
            ]
        })
        .collect();
    render(
        &["bucket", "runs", "out tokens", "ctx growth", "seconds"],
        &[
            Align::Left,
            Align::Right,
            Align::Right,
            Align::Right,
            Align::Right,
        ],
        &rows,
        format,
    );
    Ok(())
}

/// Read all installed config (global scope) into one surface list.
fn read_global_surfaces() -> Result<Vec<Surface>> {
    let config_dir = claude_home()?;
    let scope = Scope::Global;
    let mut surfaces = read_skill_surfaces(&config_dir.join("skills"), &scope);
    surfaces.extend(read_rule_surfaces(&config_dir.join("rules"), &scope));
    surfaces.extend(read_agent_surfaces(&config_dir.join("agents"), &scope));
    surfaces.extend(read_mcp_server_surfaces(
        &config_dir.join("mcp.json"),
        &scope,
    ));
    surfaces.extend(read_hook_surfaces(
        &[config_dir.join("settings.json")],
        &scope,
    ));
    surfaces.extend(read_claude_md_surfaces(
        &[(&config_dir.join("CLAUDE.md"), "global")],
        &scope,
        home_dir().map(Path::new),
    ));
    Ok(surfaces)
}

/// Join the catalogued surfaces against usage: each installed surface with its
/// static cost and (for usage-measurable kinds) invocation count, unused ones
/// flagged. Usage with no matching surface is shown as orphaned.
fn inventory(filter: &ScopeFilter, format: Format, frozen: bool, db: &Path) -> Result<()> {
    let store = open_for_read(db, frozen)?;
    let catalog: Vec<_> = catalog_with_hook_uses(&store)?
        .into_iter()
        .filter(|e| in_scope(filter, &e.scope, &e.project))
        .collect();
    let usage = store.usage_counts()?;

    if format == Format::Json {
        let catalogued: std::collections::HashSet<(&str, &str)> = catalog
            .iter()
            .map(|e| (e.kind.as_str(), e.id.as_str()))
            .collect();
        let mut items: Vec<serde_json::Value> = catalog
            .iter()
            .map(|entry| {
                let measurable = entry.measurable;
                serde_json::json!({
                    "kind": entry.kind,
                    "id": entry.id,
                    "scope": entry.scope,
                    "project": if entry.project.is_empty() { None } else { Some(&entry.project) },
                    "static_tokens": entry.static_tokens,
                    "startup_tokens": entry.startup_tokens,
                    "uses": if measurable { Some(entry.uses) } else { None },
                    "load_mode": entry.load_mode,
                    "status": if measurable && entry.uses == 0 { "unused" }
                              else if !measurable { "catalog-only" } else { "" },
                })
            })
            .collect();
        if *filter == ScopeFilter::All {
            for (kind, id, count) in &usage {
                if !catalogued.contains(&(kind.as_str(), id.as_str())) {
                    items.push(serde_json::json!({
                        "kind": kind, "id": id, "scope": null, "project": null,
                        "static_tokens": null, "startup_tokens": null,
                        "uses": count, "load_mode": null,
                        "status": "orphaned",
                    }));
                }
            }
        }
        return emit_json(&serde_json::json!({ "surfaces": items }));
    }
    if catalog.is_empty() && usage.is_empty() {
        println!("nothing catalogued — run `cclens analyze` first");
        return Ok(());
    }

    preamble(
        format,
        "Everything installed in your config — skills, rules, agents, hooks, MCP servers,\n\
         CLAUDE.md — with where it is installed (scope) and whether it was ever used.\n\
         \"start tok\" is what every session pays for having it installed; \"static tok\"\n\
         is the whole definition, which for a skill or agent is paid only on invocation.\n\
         UNUSED marks delete candidates. A load is not a use, so rules and CLAUDE.md show\n\
         no uses here — `cclens rules` shows how often each one was loaded. A hook's\n\
         uses are the calls its matcher selected (see `cclens hooks`).",
    );
    let mut rows: Vec<Vec<String>> = Vec::new();
    for entry in &catalog {
        let measurable = entry.measurable;
        // "unused" is only meaningful for a measurable row: catalog-only kinds
        // (rules, CLAUDE.md) emit no events, and a hook whose trigger cannot be
        // counted has no uses figure, so 0 means nothing for either.
        let (uses_cell, status) = if measurable {
            (
                entry.uses.to_string(),
                if entry.uses == 0 { "UNUSED" } else { "" },
            )
        } else {
            ("-".to_string(), "(catalog-only)")
        };
        let tokens = |value: Option<i64>| value.map_or_else(|| "?".to_string(), |t| t.to_string());
        rows.push(vec![
            entry.kind.clone(),
            entry.id.clone(),
            scope_cell(&entry.scope, &entry.project),
            tokens(entry.startup_tokens),
            tokens(entry.static_tokens),
            uses_cell,
            entry.load_mode.clone(),
            status.to_string(),
        ]);
    }

    // Orphaned usage has no scope to filter on; it appears in the full view only.
    let catalogued: std::collections::HashSet<(&str, &str)> = catalog
        .iter()
        .map(|e| (e.kind.as_str(), e.id.as_str()))
        .collect();
    for (kind, id, count) in &usage {
        if *filter != ScopeFilter::All {
            break;
        }
        if !catalogued.contains(&(kind.as_str(), id.as_str())) {
            rows.push(vec![
                kind.clone(),
                id.clone(),
                "-".to_string(),
                "-".to_string(),
                "-".to_string(),
                count.to_string(),
                "-".to_string(),
                "ORPHANED".to_string(),
            ]);
        }
    }

    render(
        &[
            "kind",
            "id",
            "scope",
            "start tok",
            "static tok",
            "uses",
            "load mode",
            "status",
        ],
        &[
            Align::Left,
            Align::Left,
            Align::Left,
            Align::Right,
            Align::Right,
            Align::Right,
            Align::Left,
            Align::Left,
        ],
        &rows,
        format,
    );
    Ok(())
}

/// Whether a catalog row's `(scope, project)` passes the `--scope` filter.
fn in_scope(filter: &ScopeFilter, scope: &str, project: &str) -> bool {
    if scope == "global" {
        filter.includes_global()
    } else {
        filter.includes_project(project)
    }
}

/// A human-readable scope cell: `global`, or `project:<slug>`.
fn scope_cell(scope: &str, project: &str) -> String {
    if scope == "project" {
        format!("project:{project}")
    } else {
        scope.to_string()
    }
}

/// List optimization wedges, ranked by what removing each surface actually saves
/// at session start — real always-on tokens first, then MCP schemas, then
/// declutter-only (skills/agents, whose body is on-demand). This is the "where
/// and how to optimize" view, honest about which removals save context.
fn waste(filter: &ScopeFilter, format: Format, frozen: bool, db: &Path) -> Result<()> {
    let store = open_for_read(db, frozen)?;
    let catalog: Vec<_> = catalog_with_hook_uses(&store)?
        .into_iter()
        .filter(|e| in_scope(filter, &e.scope, &e.project))
        .collect();

    struct Row<'a> {
        wedge: Wedge,
        kind: &'a str,
        id: &'a str,
        scope: &'a str,
        project: &'a str,
        uses: i64,
        measurable: bool,
        savings: StartupSavings,
        /// What every session pays for having it installed.
        startup_tokens: Option<u64>,
        /// What its body costs when loaded — an invocation cost, never a saving.
        on_demand_tokens: Option<u64>,
    }

    let mut found: Vec<Row> = Vec::new();
    for entry in &catalog {
        let measurable = entry.measurable;
        let load_mode = LoadMode::from_label(&entry.load_mode).unwrap_or(LoadMode::OnDemand);
        let static_tokens = entry.static_tokens.map(|tokens| tokens as u64);
        let startup_tokens = entry.startup_tokens.map(|tokens| tokens as u64);
        if let Some(wedge) = classify_wedge(
            measurable,
            load_mode,
            static_tokens,
            entry.uses,
            HEAVY_TOKENS,
        ) {
            found.push(Row {
                wedge,
                kind: &entry.kind,
                id: &entry.id,
                scope: &entry.scope,
                project: &entry.project,
                uses: entry.uses,
                measurable,
                savings: startup_savings(load_mode, startup_tokens),
                startup_tokens,
                on_demand_tokens: on_demand_tokens(load_mode, static_tokens),
            });
        }
    }

    // Rule loads raise their own wedges; a load is not a use, so the row's
    // evidence is its load history rather than a `uses` count.
    let rule_stats = store.rule_load_stats()?;
    let mut load_evidence: std::collections::HashMap<(&str, &str, &str, &str), String> =
        std::collections::HashMap::new();
    for stat in rule_stats
        .iter()
        .filter(|s| in_scope(filter, &s.scope, &s.project))
    {
        let load_mode = LoadMode::from_label(&stat.load_mode).unwrap_or(LoadMode::OnDemand);
        let verdict = load_verdict(load_mode, stat.history(), LoadTuning::default());
        let Some(wedge) = load_wedge(verdict) else {
            continue;
        };
        load_evidence.insert(
            (&stat.kind, &stat.id, &stat.scope, &stat.project),
            format!("{}/{} sess", stat.sessions, stat.observed_sessions),
        );
        found.push(Row {
            wedge,
            kind: &stat.kind,
            id: &stat.id,
            scope: &stat.scope,
            project: &stat.project,
            uses: stat.loads as i64,
            measurable: false,
            savings: startup_savings(load_mode, Some(0)),
            startup_tokens: Some(0),
            on_demand_tokens: on_demand_tokens(load_mode, stat.static_tokens),
        });
    }
    let evidence = |row: &Row| {
        load_evidence
            .get(&(row.kind, row.id, row.scope, row.project))
            .cloned()
    };

    // Rank by real startup savings: measured tokens (desc), then unmeasured MCP
    // schemas, then declutter-only.
    found.sort_by_key(|row| savings_rank(row.savings));

    if format == Format::Json {
        let wedges: Vec<serde_json::Value> = found
            .iter()
            .map(|row| {
                let (savings, savings_tokens) = match row.savings {
                    StartupSavings::Tokens(n) => ("tokens", Some(n)),
                    StartupSavings::UnknownSchema => ("unknown_schema", None),
                    StartupSavings::Declutter => ("declutter", None),
                };
                serde_json::json!({
                    "wedge": row.wedge.label(),
                    "kind": row.kind,
                    "id": row.id,
                    "scope": row.scope,
                    "project": if row.project.is_empty() { None } else { Some(row.project) },
                    "savings": savings,
                    "savings_tokens": savings_tokens,
                    "startup_tokens": row.startup_tokens,
                    "on_demand_tokens": row.on_demand_tokens,
                    "uses": if row.measurable { Some(row.uses) } else { None },
                    "load_evidence": evidence(row),
                    "suggestion": waste_suggestion(row.kind, row.wedge, row.savings, row.on_demand_tokens),
                })
            })
            .collect();
        return emit_json(&serde_json::json!({ "wedges": wedges }));
    }
    if found.is_empty() {
        println!("nothing wasteful found — run `cclens analyze` first");
        return Ok(());
    }

    preamble(
        format,
        "Concrete cleanup opportunities in your config, ranked by what removing each\n\
         one actually saves at session start. A \"~\" figure is declutter-only — just\n\
         the description is loaded at startup; the body costs nothing until invoked.",
    );
    let rows: Vec<Vec<String>> = found
        .iter()
        .map(|row| {
            let uses_cell = if row.measurable {
                row.uses.to_string()
            } else {
                evidence(row).unwrap_or_else(|| "-".to_string())
            };
            vec![
                row.wedge.label().to_string(),
                format!("{}/{}", row.kind, row.id),
                scope_cell(row.scope, row.project),
                savings_cell(row.savings, row.startup_tokens),
                uses_cell,
                waste_suggestion(row.kind, row.wedge, row.savings, row.on_demand_tokens),
            ]
        })
        .collect();
    render(
        &[
            "problem",
            "surface",
            "scope",
            "saves@start",
            "uses",
            "suggestion",
        ],
        &[
            Align::Left,
            Align::Left,
            Align::Left,
            Align::Right,
            Align::Right,
            Align::Left,
        ],
        &rows,
        format,
    );
    Ok(())
}

/// Every catalogued rule and `CLAUDE.md` with its load history and the verdict
/// on its `paths:` condition (`docs/specs/surfaces.md` "Rule loads"). The
/// observed-session count rides on every row because it is the gate: below the
/// minimum a rule is unmeasured, never "never loaded".
fn rules(filter: &ScopeFilter, format: Format, frozen: bool, db: &Path) -> Result<()> {
    let store = open_for_read(db, frozen)?;
    let tuning = LoadTuning::default();
    let mut rows: Vec<_> = store
        .rule_load_stats()?
        .into_iter()
        .filter(|s| in_scope(filter, &s.scope, &s.project))
        .map(|stat| {
            let load_mode = LoadMode::from_label(&stat.load_mode).unwrap_or(LoadMode::OnDemand);
            let verdict = load_verdict(load_mode, stat.history(), tuning);
            (stat, verdict)
        })
        .collect();
    // Only a conditional rule can be flagged, so those lead; within a verdict
    // the costliest history first.
    rows.sort_by_key(|(stat, verdict)| {
        (
            verdict_rank(*verdict),
            std::cmp::Reverse(tokens_paid(stat.static_tokens, stat.loads).unwrap_or(0)),
            stat.id.clone(),
        )
    });

    if format == Format::Json {
        let items: Vec<serde_json::Value> = rows
            .iter()
            .map(|(stat, verdict)| {
                serde_json::json!({
                    "kind": stat.kind,
                    "id": stat.id,
                    "scope": stat.scope,
                    "project": if stat.project.is_empty() { None } else { Some(&stat.project) },
                    "load_mode": stat.load_mode,
                    "static_tokens": stat.static_tokens,
                    "loads": stat.loads,
                    "sessions": stat.sessions,
                    "observed_sessions": stat.observed_sessions,
                    "tokens_paid": tokens_paid(stat.static_tokens, stat.loads),
                    "last_loaded_epoch": stat.last_loaded_epoch,
                    "verdict": verdict.label(),
                })
            })
            .collect();
        return emit_json(&serde_json::json!({
            "min_observed_sessions": tuning.min_observed_sessions,
            "always_on_share": tuning.always_on_share,
            "rules": items,
        }));
    }
    if rows.is_empty() {
        println!("no rules catalogued — run `cclens analyze` first");
        return Ok(());
    }

    preamble(
        format,
        &format!(
            "How often each rule and CLAUDE.md actually entered the context. \"sessions\" is\n\
             observed sessions it loaded in / observed sessions — those new enough to record\n\
             loads (Claude Code 2.1.30x+); older transcripts count for nothing, not zero.\n\
             A path-conditional rule is NEVER LOADED with 0 loads over {min}+ observed\n\
             sessions, ALWAYS LOADED in {share:.0}%+ of them; with fewer than {min} it is\n\
             unmeasured. \"tok paid\" is the file's weight times its loads.",
            min = tuning.min_observed_sessions,
            share = tuning.always_on_share * 100.0,
        ),
    );
    let table: Vec<Vec<String>> = rows
        .iter()
        .map(|(stat, verdict)| {
            vec![
                format!("{}/{}", stat.kind, stat.id),
                scope_cell(&stat.scope, &stat.project),
                stat.load_mode.clone(),
                stat.loads.to_string(),
                format!("{}/{}", stat.sessions, stat.observed_sessions),
                tokens_paid(stat.static_tokens, stat.loads)
                    .map_or_else(|| "?".to_string(), |n| n.to_string()),
                stat.last_loaded_epoch.map_or_else(
                    || "-".to_string(),
                    |epoch| bucket_label(epoch, Bucket::Day, JST_OFFSET_SECS),
                ),
                verdict_cell(*verdict).to_string(),
            ]
        })
        .collect();
    render(
        &[
            "surface",
            "scope",
            "load mode",
            "loads",
            "sessions",
            "tok paid",
            "last loaded",
            "status",
        ],
        &[
            Align::Left,
            Align::Left,
            Align::Left,
            Align::Right,
            Align::Right,
            Align::Right,
            Align::Left,
            Align::Left,
        ],
        &table,
        format,
    );
    Ok(())
}

fn verdict_rank(verdict: LoadVerdict) -> u8 {
    match verdict {
        LoadVerdict::NeverLoaded => 0,
        LoadVerdict::AlwaysLoaded => 1,
        LoadVerdict::Selective => 2,
        LoadVerdict::Unmeasured => 3,
        LoadVerdict::Unconditional => 4,
    }
}

fn verdict_cell(verdict: LoadVerdict) -> &'static str {
    match verdict {
        LoadVerdict::NeverLoaded => "NEVER LOADED",
        LoadVerdict::AlwaysLoaded => "ALWAYS LOADED",
        LoadVerdict::Selective => "",
        LoadVerdict::Unmeasured => "(unmeasured)",
        LoadVerdict::Unconditional => "(always-on)",
    }
}

/// Sort key: real measured savings first (largest first), then unmeasured MCP
/// schemas, then declutter-only.
fn savings_rank(savings: StartupSavings) -> (u8, std::cmp::Reverse<u64>) {
    match savings {
        StartupSavings::Tokens(n) => (0, std::cmp::Reverse(n)),
        StartupSavings::UnknownSchema => (1, std::cmp::Reverse(0)),
        StartupSavings::Declutter => (2, std::cmp::Reverse(0)),
    }
}

fn savings_cell(savings: StartupSavings, startup_tokens: Option<u64>) -> String {
    match savings {
        StartupSavings::Tokens(n) => format!("{n}/sess"),
        StartupSavings::UnknownSchema => "schema?".to_string(),
        // Negligible, but measured: a description-loaded surface still costs its
        // description every session, and that is all removing it reclaims.
        StartupSavings::Declutter => format!("~{}/sess", startup_tokens.unwrap_or(0)),
    }
}

/// The suggestion for one wedge row. An unused hook is not clutter but a
/// matcher that selected nothing, which is what the reader has to fix.
fn waste_suggestion(
    kind: &str,
    wedge: Wedge,
    savings: StartupSavings,
    on_demand: Option<u64>,
) -> String {
    if kind == "hook" && wedge == Wedge::Unused {
        return "its matcher never matched a call: fix the matcher, or drop the hook".to_string();
    }
    savings_suggestion(wedge, savings, on_demand)
}

/// A suggestion honest about whether removal saves context or only declutters —
/// naming the body cost as an invocation cost so it is not read as a saving.
fn savings_suggestion(wedge: Wedge, savings: StartupSavings, on_demand: Option<u64>) -> String {
    let body = match on_demand {
        Some(n) => format!("body is {n} tok when invoked"),
        None => "body is on-demand".to_string(),
    };
    let per_load = on_demand.map_or_else(String::new, |n| format!(", {n} tok per load"));
    match wedge {
        Wedge::NeverFires => {
            return format!(
                "never loaded: narrow the paths: globs, or delete (declutter only{per_load})"
            );
        }
        Wedge::AlwaysLoaded => {
            return format!(
                "loads in almost every session{per_load}: drop paths: to load once at \
                 start, or narrow the globs"
            );
        }
        Wedge::Unused | Wedge::AlwaysOnHeavy | Wedge::CostlyRare => {}
    }
    match savings {
        StartupSavings::Tokens(n) => format!("removing saves ~{n} tokens every session"),
        StartupSavings::UnknownSchema => {
            "disable: drops its tool schema from every session (real, unmeasured)".to_string()
        }
        StartupSavings::Declutter => match wedge {
            Wedge::Unused => format!("declutter only: {body}, ~no startup saving"),
            _ => format!("heavy only when invoked ({body}); rarely used"),
        },
    }
}

/// Main session transcripts: the `<sessionId>.jsonl` files directly under each
/// project slug directory (subagent transcripts live one level deeper and are
/// not main-thread usage).
fn main_transcripts(projects: &Path) -> Result<Vec<PathBuf>> {
    let mut transcripts = Vec::new();
    let slugs = fs::read_dir(projects)
        .with_context(|| format!("read projects dir {}", projects.display()))?;
    for slug in slugs.flatten() {
        if !slug.path().is_dir() {
            continue;
        }
        for entry in fs::read_dir(slug.path()).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "jsonl") {
                transcripts.push(path);
            }
        }
    }
    Ok(transcripts)
}

fn session_meta(
    transcript: &Path,
    root: String,
    sub_tokens: i64,
    sub_agent_count: i64,
    start: Option<SessionStart>,
) -> SessionMeta {
    let id = transcript
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("unknown")
        .to_string();
    let slug = transcript
        .parent()
        .and_then(|dir| dir.file_name())
        .and_then(|name| name.to_str())
        .unwrap_or("unknown")
        .to_string();
    SessionMeta {
        project: normalize_project(&slug),
        slug,
        id,
        root,
        source_path: transcript.display().to_string(),
        sub_tokens,
        sub_agent_count,
        start,
    }
}

/// Fold a worktree slug onto its parent project so usage is not split across
/// `...--wt-feat-x` directories (see `docs/specs/storage.md`).
fn normalize_project(slug: &str) -> String {
    match slug.split_once("--wt-") {
        Some((parent, _)) => parent.to_string(),
        None => slug.to_string(),
    }
}

fn default_projects_dir() -> Result<PathBuf> {
    Ok(claude_home()?.join("projects"))
}

/// The user's home directory, resolved once per run.
///
/// `HOME` is preferred, so unix behaves exactly as before; Windows does not set
/// it, hence `USERPROFILE`. The wrinkle is that a Windows shell like Git Bash
/// *does* export `HOME` — as a POSIX path (`/c/Users/…`) that a native binary
/// cannot resolve. Taking it at face value would send every lookup to a
/// directory that does not exist and report "no transcripts" instead of an
/// error, so on Windows a candidate has to name a real directory to win. The
/// check stays off on unix, where a home that does not exist is worth surfacing
/// as itself rather than silently falling through to another variable.
fn home_dir() -> Option<&'static str> {
    static HOME: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    HOME.get_or_init(|| {
        ["HOME", "USERPROFILE"]
            .into_iter()
            .filter_map(|key| std::env::var(key).ok())
            .filter(|value| !value.is_empty())
            .find(|value| !cfg!(windows) || Path::new(value).is_dir())
    })
    .as_deref()
}

/// The Claude Code config root for this run: the environment read that
/// `adapter::config::claude_config_dir` deliberately does not do, plus the
/// advice to show when neither variable names one.
fn claude_home() -> Result<PathBuf> {
    let config_dir = std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from);
    // What can go wrong differs by platform, so the advice does too: only
    // Windows requires the value to name a real directory, and telling a unix
    // user otherwise sends them looking for a problem they do not have.
    claude_config_dir(config_dir.as_deref(), home_dir().map(Path::new)).with_context(|| {
        if cfg!(windows) {
            "set CLAUDE_CONFIG_DIR, or HOME/USERPROFILE to an existing directory"
        } else {
            "set CLAUDE_CONFIG_DIR or HOME"
        }
    })
}

/// Resolve `--db`, defaulting to a **user-level** store rather than a
/// cwd-relative file. What gets analyzed is every transcript under
/// `~/.claude/projects` — one cross-project dataset — so a per-directory
/// default would build a separate copy of the same store per directory cclens
/// happens to run from, and leave a database inside unrelated Git working
/// trees.
///
/// State, not cache. The store accumulates rows whose source transcripts Claude
/// Code later prunes, at which point it is the only surviving record of those
/// sessions and nothing on disk can rebuild it (`storage.md`). A cache
/// directory is by definition somewhere a cleaner may empty without asking —
/// exactly the one thing this file cannot survive.
///
/// Pure over its inputs (the environment is read by the caller) so the
/// resolution is testable.
fn store_path(
    db: Option<PathBuf>,
    state_home: Option<&Path>,
    home: Option<&Path>,
) -> Result<PathBuf> {
    if let Some(db) = db {
        return Ok(db);
    }
    // Both roots must be absolute, for the same reason: honoring a relative one
    // would reintroduce the cwd-relative store this default exists to avoid.
    // (For `XDG_STATE_HOME` the XDG spec says as much — a non-absolute value is
    // invalid and must be ignored.)
    let base = match state_home.filter(|dir| dir.is_absolute()) {
        Some(dir) => dir.to_path_buf(),
        None => home
            .filter(|dir| dir.is_absolute())
            .context("HOME is not set to an absolute path")?
            .join(".local")
            .join("state"),
    };
    Ok(base.join("cclens").join("cclens.db"))
}

/// `store_path` against the live environment.
fn resolve_db(db: Option<PathBuf>) -> Result<PathBuf> {
    let state_home = std::env::var_os("XDG_STATE_HOME").map(PathBuf::from);
    store_path(db, state_home.as_deref(), home_dir().map(Path::new))
}

/// Column alignment for the table renderer.
#[derive(Clone, Copy)]
enum Align {
    Left,
    Right,
}

/// Output format for reports: `table` and `markdown` are for humans, `json`
/// is the machine contract — typed values, nothing but JSON on stdout (all
/// meta/freshness chatter goes to stderr).
#[derive(Clone, Copy, PartialEq)]
enum Format {
    Table,
    Markdown,
    Json,
}

fn parse_format(value: Option<&str>) -> Result<Format> {
    match value.unwrap_or("table") {
        "table" => Ok(Format::Table),
        "markdown" | "md" => Ok(Format::Markdown),
        "json" => Ok(Format::Json),
        other => anyhow::bail!("unknown --format '{other}' (table|markdown|json)"),
    }
}

/// Print a machine-readable payload: pretty JSON, alone on stdout.
fn emit_json(value: &serde_json::Value) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

/// ANSI styling for the human formats. Colors turn on only when the target is
/// a terminal, `NO_COLOR` is unset, and `TERM` is not `dumb` — so pipes,
/// captures, and `--format markdown|json` always get plain bytes.
#[derive(Clone, Copy)]
struct Style {
    on: bool,
}

impl Style {
    fn stdout() -> Self {
        use std::io::IsTerminal;
        Self {
            on: use_color(std::io::stdout().is_terminal()),
        }
    }

    fn stderr() -> Self {
        use std::io::IsTerminal;
        Self {
            on: use_color(std::io::stderr().is_terminal()),
        }
    }

    fn paint(&self, code: &str, text: &str) -> String {
        if self.on {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }

    fn bold(&self, text: &str) -> String {
        self.paint("1", text)
    }

    fn dim(&self, text: &str) -> String {
        self.paint("2", text)
    }

    /// Section headings (`== … ==`, table headers).
    fn heading(&self, text: &str) -> String {
        self.paint("1;36", text)
    }

    /// Pointers to follow-up commands.
    fn command(&self, text: &str) -> String {
        self.paint("36", text)
    }

    /// Attention: a finding's count, a wedge label, a staleness warning.
    fn warn(&self, text: &str) -> String {
        self.paint("33", text)
    }

    fn good(&self, text: &str) -> String {
        self.paint("32", text)
    }
}

fn use_color(tty: bool) -> bool {
    tty && std::env::var_os("NO_COLOR").is_none()
        && std::env::var("TERM").map(|t| t != "dumb").unwrap_or(true)
}

/// Render a table as aligned text or GitHub-flavored markdown. Auto-sizes
/// columns; `aligns` controls per-column alignment in table mode. `json` is
/// each command's own payload, emitted before reaching here.
fn render(headers: &[&str], aligns: &[Align], rows: &[Vec<String>], format: Format) {
    match format {
        Format::Json => unreachable!("json is emitted by the command, not the table renderer"),
        Format::Markdown => {
            println!("| {} |", headers.join(" | "));
            let sep: Vec<&str> = headers.iter().map(|_| "---").collect();
            println!("| {} |", sep.join(" | "));
            for row in rows {
                println!("| {} |", row.join(" | "));
            }
        }
        Format::Table => {
            // Styling wraps a cell only after padding, so ANSI codes never
            // enter the width math.
            let style = Style::stdout();
            let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
            for row in rows {
                for (col, cell) in row.iter().enumerate() {
                    widths[col] = widths[col].max(cell.chars().count());
                }
            }
            let format_row = |cells: &[String]| -> String {
                cells
                    .iter()
                    .enumerate()
                    .map(|(col, cell)| pad(cell, widths[col], aligns[col]))
                    .collect::<Vec<_>>()
                    .join("  ")
            };
            let header_cells: Vec<String> = headers.iter().map(|h| (*h).to_string()).collect();
            println!("{}", style.heading(&format_row(&header_cells)));
            let total: usize = widths.iter().sum::<usize>() + 2 * widths.len().saturating_sub(1);
            println!("{}", style.dim(&"-".repeat(total)));
            for row in rows {
                println!("{}", format_row(row));
            }
        }
    }
}

fn pad(text: &str, width: usize, align: Align) -> String {
    let fill = " ".repeat(width.saturating_sub(text.chars().count()));
    match align {
        Align::Left => format!("{text}{fill}"),
        Align::Right => format!("{fill}{text}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_flag_prints_the_crate_version() {
        use clap::CommandFactory;
        let err = Cli::command()
            .try_get_matches_from(["cclens", "--version"])
            .unwrap_err();
        assert_eq!(err.kind(), clap::error::ErrorKind::DisplayVersion);
        assert_eq!(
            err.to_string(),
            format!("cclens {}\n", env!("CARGO_PKG_VERSION"))
        );
    }

    fn agent(name: &str, runs: i64, out_tokens: i64) -> optimize_mod::AgentCost {
        optimize_mod::AgentCost {
            agent: Some(name.to_string()),
            runs,
            out_tokens,
        }
    }

    fn findings_with_agents(
        sub_tokens: i64,
        sub_agents: i64,
        agents: Vec<optimize_mod::AgentCost>,
    ) -> optimize_mod::Findings {
        optimize_mod::Findings {
            sub_tokens,
            sub_agents,
            agent_split: SplitCoverage::new(
                sub_tokens,
                sub_agents,
                agents.iter().map(|a| (a.runs, a.out_tokens)),
            ),
            agents,
            ..Default::default()
        }
    }

    #[test]
    fn doctor_calls_the_top_rows_most_of_the_output_only_when_they_are() {
        let f = findings_with_agents(
            1_000_000,
            10,
            vec![agent("a", 4, 700_000), agent("b", 6, 300_000)],
        );
        let lines = doctor_agent_lines(&f, "cclens usage");
        assert_eq!(
            lines,
            vec![
                "Most of that subagent output: a 700.0k (4 runs), b 300.0k (6 runs). \
                 Full split: cclens usage"
            ]
        );
    }

    #[test]
    fn doctor_does_not_call_a_minority_of_the_output_most_of_it() {
        // A long tail: ten types at a tenth each, so the three shown cover 30%
        // of the output, and "most" would be false even with a complete split.
        let agents: Vec<_> = (0..10)
            .map(|i| agent(&format!("t{i}"), 1, 100_000))
            .collect();
        let f = findings_with_agents(1_000_000, 10, agents);
        let lines = doctor_agent_lines(&f, "cclens usage");
        assert_eq!(
            lines,
            vec![
                "The largest agent types: t0 100.0k (1 run), t1 100.0k (1 run), t2 100.0k \
                 (1 run). Full split: cclens usage",
                "… and 7 more agent type(s) not shown",
            ]
        );
    }

    #[test]
    fn doctor_labels_a_partial_split_before_naming_its_agents() {
        // The rows explain 12% of the total: saying "most of that output" here
        // would blame the listed types for tokens no row accounts for.
        let f = findings_with_agents(
            3_093_466,
            375,
            vec![
                agent("general-purpose", 11, 315_663),
                agent("codex:codex-rescue", 14, 62_067),
            ],
        );
        let lines = doctor_agent_lines(&f, "cclens usage");
        assert_eq!(
            lines,
            vec![
                "Only 377.7k of those tokens (25 of 375 runs) have a per-agent split — the \
                 rest were analyzed before runs were extracted and their transcripts are gone.",
                "Among the runs that have one: general-purpose 315.7k (11 runs), \
                 codex:codex-rescue 62.1k (14 runs). Full split: cclens usage",
            ]
        );
    }

    #[test]
    fn doctor_names_no_agents_when_the_split_is_unavailable() {
        // The gap is reported on stderr with its refresh hint; there is
        // nothing to list in-band.
        let f = findings_with_agents(3_093_466, 375, vec![]);
        assert!(doctor_agent_lines(&f, "cclens usage").is_empty());
    }

    /// A path in the temp dir that no other test uses, removed on drop so a
    /// failure does not leak a file into the runner's temp dir.
    struct TempPath(PathBuf);

    impl TempPath {
        fn new(tag: &str) -> Self {
            let mut path = std::env::temp_dir();
            path.push(format!("cclens-test-{}-{tag}", std::process::id()));
            let _ = fs::remove_file(&path);
            Self(path)
        }
    }

    impl Drop for TempPath {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }

    #[test]
    fn write_private_file_refuses_a_path_that_already_exists() {
        // The temp dir is shared, so a same-user process can pre-create the
        // path — as a symlink to somewhere it can read. Writing the briefing
        // into whatever is already there is the leak this prevents.
        let path = TempPath::new("preexisting.md");
        fs::write(&path.0, "squatted").unwrap();
        assert!(write_private_file(&path.0, "briefing").is_err());
        assert_eq!(fs::read_to_string(&path.0).unwrap(), "squatted");
    }

    #[test]
    fn write_private_file_writes_the_contents_to_a_fresh_path() {
        let path = TempPath::new("fresh.md");
        write_private_file(&path.0, "briefing").unwrap();
        assert_eq!(fs::read_to_string(&path.0).unwrap(), "briefing");
    }

    #[cfg(unix)]
    #[test]
    fn write_private_file_is_readable_only_by_its_owner() {
        use std::os::unix::fs::PermissionsExt;

        let path = TempPath::new("mode.md");
        write_private_file(&path.0, "briefing").unwrap();
        let mode = fs::metadata(&path.0).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn shorten_to_tilde_replaces_the_home_prefix() {
        assert_eq!(
            shorten_to_tilde("/tmp/example/me/.claude", Some("/tmp/example/me")),
            "~/.claude"
        );
        // Home itself collapses to a bare tilde.
        assert_eq!(
            shorten_to_tilde("/tmp/example/me", Some("/tmp/example/me")),
            "~"
        );
    }

    #[test]
    fn shorten_to_tilde_accepts_a_backslash_separator() {
        assert_eq!(
            shorten_to_tilde(r"C:\example\me\.claude", Some(r"C:\example\me")),
            r"~\.claude"
        );
    }

    #[test]
    fn shorten_to_tilde_matches_a_home_spelled_with_the_other_separator() {
        // Windows takes either separator, and the home comes from the
        // environment while the path comes from a recorded cwd — so the two can
        // disagree on spelling while naming the same directory.
        assert_eq!(
            shorten_to_tilde("C:/example/me/.claude", Some(r"C:\example\me")),
            "~/.claude"
        );
        assert_eq!(
            shorten_to_tilde(r"C:\example\me\.claude", Some("C:/example/me")),
            r"~\.claude"
        );
        // The suffix keeps the spelling the path actually had, not the folded
        // one used for the comparison.
        assert_eq!(
            shorten_to_tilde(r"C:\example\me\a/b\c", Some("C:/example/me")),
            r"~\a/b\c"
        );
    }

    #[test]
    fn shorten_to_tilde_leaves_a_path_outside_home_alone() {
        assert_eq!(
            shorten_to_tilde("/tmp/other", Some("/tmp/example/me")),
            "/tmp/other"
        );
        // A sibling that merely shares the prefix is not under home.
        assert_eq!(
            shorten_to_tilde("/tmp/example/metadata", Some("/tmp/example/me")),
            "/tmp/example/metadata"
        );
    }

    #[test]
    fn shorten_to_tilde_passes_the_path_through_without_a_home() {
        assert_eq!(shorten_to_tilde("/tmp/example/me", None), "/tmp/example/me");
        assert_eq!(
            shorten_to_tilde("/tmp/example/me", Some("")),
            "/tmp/example/me"
        );
    }

    #[test]
    fn a_load_inside_a_worktree_folds_onto_the_parent_checkout() {
        assert_eq!(
            fold_worktree_path(
                "/tmp/example/repo/.wt/feat-x/.claude/rules/rust.md",
                "/tmp/example/repo/.wt/feat-x/src"
            ),
            "/tmp/example/repo/.claude/rules/rust.md"
        );
    }

    #[test]
    fn a_load_outside_the_worktree_keeps_its_path() {
        assert_eq!(
            fold_worktree_path(
                "/tmp/example/home/.claude/rules/rust.md",
                "/tmp/example/repo/.wt/feat-x"
            ),
            "/tmp/example/home/.claude/rules/rust.md"
        );
    }

    #[test]
    fn a_load_in_a_plain_checkout_keeps_its_path() {
        assert_eq!(
            fold_worktree_path(
                "/tmp/example/repo/.claude/rules/rust.md",
                "/tmp/example/repo"
            ),
            "/tmp/example/repo/.claude/rules/rust.md"
        );
    }

    #[test]
    fn a_worktree_named_like_its_marker_folds_at_segment_boundaries() {
        assert_eq!(
            fold_worktree_path(
                "/tmp/example/repo/.wt/wt/.claude/rules/rust.md",
                "/tmp/example/repo/.wt/wt"
            ),
            "/tmp/example/repo/.claude/rules/rust.md"
        );
    }

    #[test]
    fn a_backslash_worktree_load_folds_too() {
        assert_eq!(
            fold_worktree_path(
                r"C:\repo\.wt\feat-x\.claude\rules\rust.md",
                r"C:\repo\.wt\feat-x"
            ),
            r"C:\repo\.claude\rules\rust.md"
        );
    }

    #[test]
    fn normalize_root_folds_a_worktree_path_onto_its_repo() {
        assert_eq!(
            normalize_root("/tmp/example/repo/.wt/feat-x"),
            "/tmp/example/repo"
        );
        // Idempotent: an already-folded root stays put.
        assert_eq!(normalize_root("/tmp/example/repo"), "/tmp/example/repo");
    }

    #[test]
    fn normalize_root_folds_a_backslash_worktree_path() {
        // A cwd recorded on Windows spells the same layout natively; leaving it
        // unfolded splits a project whose slug already folded.
        assert_eq!(
            normalize_root(r"C:\example\repo\.wt\feat-x"),
            r"C:\example\repo"
        );
        assert_eq!(normalize_root(r"C:\example\repo"), r"C:\example\repo");
    }

    #[test]
    fn normalize_root_keeps_the_filesystem_root_separator() {
        // Folding onto the root leaves the separator standing: it *is* the
        // parent. Dropping it would hand back a relative path — `""` on unix,
        // the drive-relative `C:` on Windows — and the root then names a
        // different directory than the one the session ran in.
        assert_eq!(normalize_root("/.wt/feat-x"), "/");
        assert_eq!(normalize_root(r"C:\.wt\feat-x"), r"C:\");
        // A directory whose name merely ends in a colon is not a drive.
        assert_eq!(normalize_root("/tmp/odd:/.wt/feat-x"), "/tmp/odd:");
    }

    #[test]
    fn normalize_root_leaves_a_dot_wt_that_is_not_a_segment_alone() {
        // Only a whole `.wt` directory is a worktree parent.
        assert_eq!(
            normalize_root("/tmp/example/repo/x.wt/feat-x"),
            "/tmp/example/repo/x.wt/feat-x"
        );
        // A trailing `.wt` names no checkout to fold onto.
        assert_eq!(
            normalize_root("/tmp/example/repo/.wt"),
            "/tmp/example/repo/.wt"
        );
    }

    #[test]
    fn a_declutter_row_shows_the_description_it_would_reclaim() {
        // The measured startup cost, not the body — deleting an unused skill
        // frees its description, and that is what the column promises.
        assert_eq!(
            savings_cell(StartupSavings::Declutter, Some(7)),
            "~7/sess".to_string()
        );
        assert_eq!(
            savings_cell(StartupSavings::Tokens(922), Some(922)),
            "922/sess".to_string()
        );
    }

    #[test]
    fn a_never_firing_rule_is_suggested_as_declutter_with_its_per_load_cost() {
        let suggestion = savings_suggestion(Wedge::NeverFires, StartupSavings::Declutter, Some(80));
        assert_eq!(
            suggestion,
            "never loaded: narrow the paths: globs, or delete (declutter only, 80 tok per load)"
        );
    }

    #[test]
    fn an_unused_body_is_suggested_as_an_invocation_cost_not_a_saving() {
        let suggestion = savings_suggestion(Wedge::Unused, StartupSavings::Declutter, Some(1015));
        assert!(suggestion.contains("1015 tok when invoked"));
        assert!(suggestion.contains("declutter only"));
    }

    #[test]
    fn store_path_defaults_under_the_user_state_dir() {
        assert_eq!(
            store_path(None, Some(Path::new("/tmp/example/state")), None).unwrap(),
            PathBuf::from("/tmp/example/state/cclens/cclens.db")
        );
        assert_eq!(
            store_path(None, None, Some(Path::new("/tmp/example/home"))).unwrap(),
            PathBuf::from("/tmp/example/home/.local/state/cclens/cclens.db")
        );
    }

    #[test]
    fn store_path_ignores_a_relative_xdg_state_home() {
        // XDG: a non-absolute XDG_STATE_HOME is invalid and must be ignored —
        // honoring it would put the store back under the current directory.
        assert_eq!(
            store_path(
                None,
                Some(Path::new("relative/state")),
                Some(Path::new("/tmp/example/home"))
            )
            .unwrap(),
            PathBuf::from("/tmp/example/home/.local/state/cclens/cclens.db")
        );
    }

    #[test]
    fn store_path_without_a_home_is_an_error() {
        assert!(store_path(None, None, None).is_err());
    }

    #[test]
    fn store_path_refuses_a_relative_home() {
        // The fallback root is held to the same bar as XDG_STATE_HOME: a
        // relative one would put the store back under the current directory.
        assert!(store_path(None, None, Some(Path::new("relative/home"))).is_err());
    }

    #[test]
    fn store_path_takes_an_explicit_db_as_is() {
        // The escape hatch for a deliberately project-local store.
        assert_eq!(
            store_path(
                Some(PathBuf::from("./cclens.db")),
                Some(Path::new("/tmp/example/state")),
                Some(Path::new("/tmp/example/home"))
            )
            .unwrap(),
            PathBuf::from("./cclens.db")
        );
    }
}
