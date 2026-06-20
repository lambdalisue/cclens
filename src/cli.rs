//! CLI: the `analyze` and `report` commands. This is the thin shell that walks
//! files and renders tables; the analysis itself lives in the pure core. See
//! `docs/specs/cli.md`.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use crate::adapter::config::{
    read_agent_surfaces, read_claude_md_surface, read_mcp_server_surfaces, read_rule_surfaces,
    read_skill_surfaces,
};
use crate::adapter::transcript::{
    count_permission_denials, extract_prompt_pointers, extract_tool_errors, extract_work_events,
    parse_session, subagent_prompt_id,
};
use crate::core::bucket::{Bucket, JST_OFFSET_SECS, bucket_label};
use crate::core::friction::ErrorCategory;
use crate::core::optimize as optimize_mod;
use crate::core::span::{DEFAULT_IDLE_GAP_MS, extract_spans};
use crate::core::surface::{
    LoadMode, Scope, StartupSavings, Surface, Wedge, classify_wedge, is_usage_measurable,
    startup_savings,
};
use crate::core::thrash::detect_thrash;
use crate::core::usage::{attribute_subagents, extract_usage_events, output_tokens};
use crate::store::{SessionMeta, Store};

#[derive(Parser)]
#[command(
    name = "ccoptimizer",
    about = "Analyze your Claude Code sessions — usage, cost, and where the work stumbles"
)]
pub struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Analyze session transcripts into the store.
    Analyze {
        /// Transcript root (default: ~/.claude/projects).
        #[arg(long)]
        projects: Option<PathBuf>,
        /// Output store path.
        #[arg(long, default_value = "ccoptimizer.db")]
        db: PathBuf,
    },
    /// Report skill usage from the store — per skill, or per time bucket.
    Report {
        /// Bucket usage by time: year | month | week | day | hour (JST).
        #[arg(long)]
        by: Option<String>,
        /// Output format: table | markdown.
        #[arg(long)]
        format: Option<String>,
        #[arg(long, default_value = "ccoptimizer.db")]
        db: PathBuf,
    },
    /// Join the skill catalog against usage — installed skills, their cost, and
    /// what is unused.
    Surfaces {
        /// Output format: table | markdown.
        #[arg(long)]
        format: Option<String>,
        #[arg(long, default_value = "ccoptimizer.db")]
        db: PathBuf,
    },
    /// List optimization opportunities (unused, always-on heavy, costly+rare),
    /// ranked, with a suggested action.
    Wedges {
        /// Output format: table | markdown.
        #[arg(long)]
        format: Option<String>,
        #[arg(long, default_value = "ccoptimizer.db")]
        db: PathBuf,
    },
    /// Reconcile the measured always-on context against your readable config —
    /// what every session actually loads, and how much is MCP/system you cannot
    /// read from files.
    Baseline {
        /// Output format: table | markdown.
        #[arg(long)]
        format: Option<String>,
        #[arg(long, default_value = "ccoptimizer.db")]
        db: PathBuf,
    },
    /// Show how you steer the session — the mix of steering / correcting /
    /// questioning / instructing prompts, with what it suggests.
    Prompts {
        /// Output format: table | markdown.
        #[arg(long)]
        format: Option<String>,
        #[arg(long, default_value = "ccoptimizer.db")]
        db: PathBuf,
    },
    /// Where the work stumbles: recurring tool failures by category, ranked,
    /// with what each suggests fixing.
    Friction {
        /// Restrict to one project (its cwd slug) — see which project owns the
        /// friction so its config can carry the fix.
        #[arg(long)]
        project: Option<String>,
        /// Output format: table | markdown.
        #[arg(long)]
        format: Option<String>,
        #[arg(long, default_value = "ccoptimizer.db")]
        db: PathBuf,
    },
    /// Files Claude edits most — where effort and churn concentrate.
    Hotspots {
        /// Output format: table | markdown.
        #[arg(long)]
        format: Option<String>,
        #[arg(long, default_value = "ccoptimizer.db")]
        db: PathBuf,
    },
    /// The Bash command mix — what Claude runs most, and the `cd` overhead.
    Commands {
        /// Output format: table | markdown.
        #[arg(long)]
        format: Option<String>,
        #[arg(long, default_value = "ccoptimizer.db")]
        db: PathBuf,
    },
    /// Thrash episodes — bursts of rapid re-edits to one file, where Claude got
    /// stuck (distinct from a healthy hotspot's spread-out edits).
    Thrash {
        /// Output format: table | markdown.
        #[arg(long)]
        format: Option<String>,
        #[arg(long, default_value = "ccoptimizer.db")]
        db: PathBuf,
    },
    /// One-screen health check: the few most actionable findings across every
    /// view, prioritised. Start here.
    Summary {
        #[arg(long, default_value = "ccoptimizer.db")]
        db: PathBuf,
    },
    /// Analyze, then hand the findings to an interactive `claude` session so you
    /// can act on them together. Composes the analysis with a prescribed advisor
    /// prompt and launches `claude` seeded with it.
    Optimize {
        /// Transcript root (default: ~/.claude/projects).
        #[arg(long)]
        projects: Option<PathBuf>,
        /// Store to analyze into / read from.
        #[arg(long, default_value = "ccoptimizer.db")]
        db: PathBuf,
        /// Use the existing store as-is; skip the analyze step.
        #[arg(long)]
        skip_analyze: bool,
        /// Print the composed prompt instead of launching `claude`.
        #[arg(long)]
        print: bool,
    },
}

pub fn run() -> Result<()> {
    match Cli::parse().command {
        Command::Analyze { projects, db } => analyze(projects, &db),
        Command::Report { by, format, db } => {
            report(by.as_deref(), parse_format(format.as_deref())?, &db)
        }
        Command::Surfaces { format, db } => surfaces(parse_format(format.as_deref())?, &db),
        Command::Wedges { format, db } => wedges(parse_format(format.as_deref())?, &db),
        Command::Baseline { format, db } => baseline(parse_format(format.as_deref())?, &db),
        Command::Prompts { format, db } => prompts(parse_format(format.as_deref())?, &db),
        Command::Friction {
            project,
            format,
            db,
        } => friction(project.as_deref(), parse_format(format.as_deref())?, &db),
        Command::Hotspots { format, db } => hotspots(parse_format(format.as_deref())?, &db),
        Command::Commands { format, db } => commands(parse_format(format.as_deref())?, &db),
        Command::Thrash { format, db } => thrash(parse_format(format.as_deref())?, &db),
        Command::Summary { db } => summary(&db),
        Command::Optimize {
            projects,
            db,
            skip_analyze,
            print,
        } => optimize(projects, &db, skip_analyze, print),
    }
}

/// Analyze, compose the advisor prompt from the findings, and hand it to an
/// interactive `claude` session — the AI-proposal layer's entry point. The pure
/// composition lives in `core::optimize`; this shell does the I/O: run the
/// analysis, read the store into `Findings`, then launch `claude` seeded with
/// the prompt (or print it with `--print` for piping / inspection).
fn optimize(projects: Option<PathBuf>, db: &Path, skip_analyze: bool, print: bool) -> Result<()> {
    if !skip_analyze {
        analyze(projects, db)?;
    }
    let store = Store::open(db).context("open store")?;
    let prompt = optimize_mod::compose_prompt(&collect_findings(&store)?);

    if print {
        println!("{prompt}");
        return Ok(());
    }

    // Hand over the terminal: `claude <prompt>` starts an interactive session
    // seeded with the briefing, inheriting our stdio so the user takes over.
    let status = std::process::Command::new("claude")
        .arg(&prompt)
        .status()
        .context("launch `claude` — is Claude Code installed and on PATH?")?;
    if !status.success() {
        anyhow::bail!("claude exited with {status}");
    }
    Ok(())
}

/// Gather the COMPLETE analysis the optimization briefing carries — every view's
/// detail as owned data for `core::optimize`, so the seeded session works from
/// the briefing and need not re-run the tool. Lists are capped where a long tail
/// adds nothing for ranking.
fn collect_findings(store: &Store) -> Result<optimize_mod::Findings> {
    let floor = store.baseline_floor()?;

    // Actionable friction grouped by project, busiest project first — the
    // session fixes each in that project's own config.
    let mut by_project: std::collections::HashMap<String, Vec<(String, i64)>> =
        std::collections::HashMap::new();
    for (proj, label, n) in store.error_counts_by_project()? {
        if ErrorCategory::from_label(&label).is_actionable() {
            by_project.entry(proj).or_default().push((label, n));
        }
    }
    let mut friction_by_project: Vec<optimize_mod::ProjectFriction> = by_project
        .into_iter()
        .map(|(project, mut categories)| {
            categories.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            optimize_mod::ProjectFriction {
                project,
                categories,
            }
        })
        .collect();
    friction_by_project.sort_by(|a, b| {
        b.total()
            .cmp(&a.total())
            .then_with(|| a.project.cmp(&b.project))
    });

    // Workflow: Bash mix + cd share, most-edited files, thrash bursts.
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
    let edits = store.work_event_rows("file_edit")?;
    let thrash: Vec<optimize_mod::ThrashLine> = detect_thrash(&edits, 5 * 60, 4)
        .into_iter()
        .take(10)
        .map(|w| optimize_mod::ThrashLine {
            span_secs: w.span_secs(),
            file: w.file,
            edits: w.edits,
        })
        .collect();

    let (unused, always_on_heavy) = config_wedges(store)?;

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
    Ok(optimize_mod::Findings {
        main_out: store.skill_usage()?.iter().map(|r| r.out_tokens).sum(),
        sub_tokens,
        sub_agents,
        floor,
        config_tokens: if floor > 0 {
            store.always_on_config_tokens()?
        } else {
            0
        },
        friction_by_project,
        cd_pct,
        top_commands,
        hotspots,
        thrash,
        unused,
        always_on_heavy,
        steer_pct,
        correct_pct,
    })
}

/// The surfaces `wedges` would flag: unused-but-measurable and always-on heavy,
/// as concrete lists. `summary` reads their lengths; `optimize` embeds the items.
fn config_wedges(
    store: &Store,
) -> Result<(Vec<optimize_mod::SurfaceRef>, Vec<optimize_mod::SurfaceRef>)> {
    let counts: std::collections::HashMap<(String, String), i64> = store
        .usage_counts()?
        .into_iter()
        .map(|(k, i, c)| ((k, i), c))
        .collect();
    let surface_ref = |e: &crate::store::CatalogEntry| optimize_mod::SurfaceRef {
        kind: e.kind.clone(),
        id: e.id.clone(),
        static_tokens: e.static_tokens,
    };
    let mut unused = Vec::new();
    let mut always_on_heavy = Vec::new();
    for entry in store.catalog()? {
        let uses = counts
            .get(&(entry.kind.clone(), entry.id.clone()))
            .copied()
            .unwrap_or(0);
        let load_mode = LoadMode::from_label(&entry.load_mode).unwrap_or(LoadMode::OnDemand);
        let static_tokens = entry.static_tokens.map(|t| t as u64);
        if is_usage_measurable(&entry.kind) && uses == 0 {
            unused.push(surface_ref(&entry));
        }
        if load_mode.is_always_on() && static_tokens.is_some_and(|t| t >= HEAVY_TOKENS) {
            always_on_heavy.push(surface_ref(&entry));
        }
    }
    Ok((unused, always_on_heavy))
}

/// One-screen health check: pull the few most actionable findings from every
/// view into one prioritised report, so the tool answers "what should I do"
/// without running ten commands.
fn summary(db: &Path) -> Result<()> {
    let store = Store::open(db).context("open store")?;

    // Where tokens go.
    let main_out: i64 = store.skill_usage()?.iter().map(|r| r.out_tokens).sum();
    let (sub_tokens, sub_agents) = store.subagent_totals()?;
    println!("== tokens ==");
    println!("  main-thread skill output  {main_out}");
    println!("  subagents                 {sub_tokens} ({sub_agents} agents)");

    // Always-on cost picture.
    let floor = store.baseline_floor()?;
    if floor > 0 {
        let config = store.always_on_config_tokens()?;
        let residual = (floor - config).max(0);
        println!("\n== always-on context per session ==");
        println!(
            "  ~{floor} tokens (your config {config}, the rest {residual} is system+tools+MCP)"
        );
    }

    // Top fixable friction (exclude non-actionable noise).
    let friction: Vec<_> = store
        .error_counts()?
        .into_iter()
        .filter(|(label, _)| ErrorCategory::from_label(label).is_actionable())
        .collect();
    if !friction.is_empty() {
        // Per-category project breakdown, so each line can name the project that
        // owns the friction when it concentrates there.
        let mut by_cat: std::collections::HashMap<String, Vec<(String, i64)>> =
            std::collections::HashMap::new();
        for (proj, label, n) in store.error_counts_by_project()? {
            by_cat.entry(label).or_default().push((proj, n));
        }
        let dominant = |label: &str, total: i64| -> Option<String> {
            let (proj, n) = by_cat.get(label)?.iter().max_by_key(|(_, n)| *n)?;
            // Only call it out when one project owns the clear majority.
            (*n * 2 > total).then(|| format!(" — mostly in `{proj}`"))
        };
        println!("\n== top fixable friction ==");
        for (label, n) in friction.iter().take(3) {
            println!(
                "  {n:>4}  {label} — {}{}",
                ErrorCategory::from_label(label).suggestion(),
                dominant(label, *n).unwrap_or_default()
            );
        }
    }

    // Workflow inefficiency: cd overhead + worst thrash.
    println!("\n== workflow ==");
    let bash = store.work_counts("bash_cmd")?;
    let bash_total: i64 = bash.iter().map(|(_, n)| n).sum();
    if bash_total > 0 {
        let cd = bash.iter().find(|(c, _)| c == "cd").map_or(0, |(_, n)| *n);
        let cd_pct = (cd as f64 * 100.0 / bash_total as f64).round() as i64;
        println!("  cd is {cd_pct}% of Bash calls — a working-dir convention would cut the churn");
    }
    let edits = store.work_event_rows("file_edit")?;
    if let Some(worst) = detect_thrash(&edits, 5 * 60, 4).first() {
        let span = worst.span_secs();
        println!(
            "  worst thrash: {} edited {}x within {}m{}s — likely got stuck there",
            worst.file,
            worst.edits,
            span / 60,
            span % 60
        );
    }

    // Config worth cutting: count what `wedges` would flag with real savings.
    let (unused, always_on_heavy_list) = config_wedges(&store)?;
    let (unused_measurable, always_on_heavy) = (unused.len(), always_on_heavy_list.len());
    println!("\n== config ==");
    println!(
        "  {unused_measurable} unused surface(s), {always_on_heavy} always-on heavy — see `wedges`"
    );

    // Prompting verdict.
    let pcounts = store.prompt_behavior_counts()?;
    let ptotal: i64 = pcounts.iter().map(|(_, n)| n).sum();
    if ptotal > 0 {
        let share = |name: &str| {
            let n = pcounts
                .iter()
                .find(|(b, _)| b == name)
                .map_or(0, |(_, n)| *n);
            (n as f64 * 100.0 / ptotal as f64).round() as i64
        };
        println!("\n== prompting ==");
        println!(
            "  {}% steering, {}% corrections — {}",
            share("steer"),
            share("correct"),
            if share("correct") >= 10 || share("steer") >= 25 {
                "see `prompts`"
            } else {
                "healthy mix"
            }
        );
    }
    Ok(())
}

/// Thrash bursts: a file edited many times in a short window — where Claude got
/// stuck and kept retrying, as opposed to a hotspot's healthy spread-out edits.
fn thrash(format: Format, db: &Path) -> Result<()> {
    const GAP_SECS: i64 = 5 * 60;
    const MIN_EDITS: u32 = 4;
    let store = Store::open(db).context("open store")?;
    let edits = store.work_event_rows("file_edit")?;
    let episodes = detect_thrash(&edits, GAP_SECS, MIN_EDITS);
    if episodes.is_empty() {
        println!("no thrash episodes (>= {MIN_EDITS} rapid re-edits) — run analyze first");
        return Ok(());
    }
    let rows: Vec<Vec<String>> = episodes
        .iter()
        .take(20)
        .map(|e| {
            let span = e.span_secs();
            vec![
                e.file.clone(),
                e.edits.to_string(),
                format!("{}m{}s", span / 60, span % 60),
            ]
        })
        .collect();
    render(
        &["file", "edits", "within"],
        &[Align::Left, Align::Right, Align::Right],
        &rows,
        format,
    );
    println!(
        "\nbursts of >= {MIN_EDITS} edits to one file within {}m — likely where Claude got stuck.",
        GAP_SECS / 60
    );
    Ok(())
}

/// Files Claude edits most. A high edit count is where effort concentrates — and
/// can flag churn (re-editing the same file many times = struggling).
fn hotspots(format: Format, db: &Path) -> Result<()> {
    let store = Store::open(db).context("open store")?;
    let counts = store.work_counts("file_edit")?;
    if counts.is_empty() {
        println!("no edits found — run `ccoptimizer analyze` first");
        return Ok(());
    }
    let rows: Vec<Vec<String>> = counts
        .iter()
        .take(25)
        .map(|(file, n)| vec![file.clone(), n.to_string()])
        .collect();
    render(
        &["file", "edits"],
        &[Align::Left, Align::Right],
        &rows,
        format,
    );
    Ok(())
}

/// The Bash command mix, and how much of it is `cd` overhead.
fn commands(format: Format, db: &Path) -> Result<()> {
    let store = Store::open(db).context("open store")?;
    let counts = store.work_counts("bash_cmd")?;
    let total: i64 = counts.iter().map(|(_, n)| n).sum();
    if total == 0 {
        println!("no Bash commands found — run `ccoptimizer analyze` first");
        return Ok(());
    }
    let rows: Vec<Vec<String>> = counts
        .iter()
        .take(20)
        .map(|(cmd, n)| {
            let pct = (*n as f64 * 100.0 / total as f64).round() as i64;
            vec![cmd.clone(), n.to_string(), format!("{pct}%")]
        })
        .collect();
    render(
        &["command", "count", "share"],
        &[Align::Left, Align::Right, Align::Right],
        &rows,
        format,
    );

    let cd = counts
        .iter()
        .find(|(c, _)| c == "cd")
        .map_or(0, |(_, n)| *n);
    let cd_pct = (cd as f64 * 100.0 / total as f64).round() as i64;
    println!("\n{total} Bash commands");
    if cd_pct >= 25 {
        println!(
            "  - {cd_pct}% are `cd` ({cd}): a lot of directory churn — absolute paths or a \
             working-dir convention (noted in CLAUDE.md) would cut it."
        );
    }
    Ok(())
}

/// Where the work stumbles: recurring tool failures by category, ranked, each
/// with what it suggests fixing. This is about the work, not the config —
/// recurring failures are fixable friction that wastes turns and tokens.
fn friction(project: Option<&str>, format: Format, db: &Path) -> Result<()> {
    let store = Store::open(db).context("open store")?;
    // With --project, fold the per-project breakdown down to one project's
    // categories; without it, the global per-category counts.
    let counts: Vec<(String, i64)> = match project {
        Some(name) => store
            .error_counts_by_project()?
            .into_iter()
            .filter(|(proj, _, _)| proj == name)
            .map(|(_, label, n)| (label, n))
            .collect(),
        None => store.error_counts()?,
    };
    let total: i64 = counts.iter().map(|(_, n)| n).sum();
    if total == 0 {
        match project {
            Some(name) => println!("no tool failures found for project `{name}`"),
            None => println!("no tool failures found — run `ccoptimizer analyze` first"),
        }
        return Ok(());
    }

    let rows: Vec<Vec<String>> = counts
        .iter()
        .map(|(label, n)| {
            vec![
                label.clone(),
                n.to_string(),
                ErrorCategory::from_label(label).suggestion().to_string(),
            ]
        })
        .collect();
    render(
        &["error", "count", "suggestion"],
        &[Align::Left, Align::Right, Align::Left],
        &rows,
        format,
    );
    println!("\n{total} tool failures (categories are lexical heuristics)");
    Ok(())
}

/// Show the mix of how the user steers the session, and what it suggests. Heavy
/// steering points to room for more autonomy; frequent corrections point to
/// clearer upfront specs. The classes are lexical heuristics (`core::prompt`).
fn prompts(format: Format, db: &Path) -> Result<()> {
    let store = Store::open(db).context("open store")?;
    let counts = store.prompt_behavior_counts()?;
    let total: i64 = counts.iter().map(|(_, n)| n).sum();
    if total == 0 {
        println!("no prompts found — run `ccoptimizer analyze` first");
        return Ok(());
    }

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

    println!("\n{total} prompts (behavioral classes are lexical heuristics)");
    let steer = pct(get("steer"));
    let correct = pct(get("correct"));
    let mut flagged = false;
    if steer >= 25 {
        flagged = true;
        println!(
            "  - {steer}% steering (\"go ahead\" / \"yes\" / \"next\"): you approve in small \
             steps — room for more autonomy (clearer upfront scope, /loop, longer leash)."
        );
    }
    if correct >= 10 {
        flagged = true;
        println!(
            "  - {correct}% corrections (\"no\" / \"instead\" / \"戻して\"): rework after a wrong \
             turn — tighter initial specs or rules could cut it."
        );
    }
    if !flagged {
        println!(
            "  - healthy mix: mostly substantive instructions, low correction ({correct}%) — \
             good alignment, no obvious babysitting or rework problem."
        );
    }
    Ok(())
}

/// Reconcile the empirical always-on floor against readable config. The floor is
/// what every session actually starts with; subtracting the config we can read
/// leaves the residual — system prompt, built-in tools, and MCP tool schemas the
/// catalog cannot weigh. The per-project floors let you infer an MCP server's
/// real cost by comparing a project that enables it against one that does not.
fn baseline(format: Format, db: &Path) -> Result<()> {
    let store = Store::open(db).context("open store")?;
    let floor = store.baseline_floor()?;
    if floor == 0 {
        println!("no data — run `ccoptimizer analyze` first");
        return Ok(());
    }
    let config = store.always_on_config_tokens()?;
    let residual = (floor - config).max(0);

    println!("Observed always-on floor (leanest session-start context): {floor} tokens");
    println!("  readable config (CLAUDE.md + always-on rules):          {config} tokens");
    println!("  residual (system prompt + built-in tools + MCP schemas): {residual} tokens");
    println!("  -> the residual is what you cannot read from files; compare projects below");
    println!(
        "     to see an MCP server's marginal cost (a project that enables it starts higher)."
    );
    println!();

    let rows: Vec<Vec<String>> = store
        .baseline_floor_per_project()?
        .into_iter()
        .map(|(project, floor)| vec![project, floor.to_string()])
        .collect();
    render(
        &["project", "floor_tokens"],
        &[Align::Left, Align::Right],
        &rows,
        format,
    );
    Ok(())
}

/// Static-token threshold above which an always-on or rarely-used surface counts
/// as "heavy" — a tuning knob, not a hard rule.
const HEAVY_TOKENS: u64 = 800;

fn analyze(projects: Option<PathBuf>, db: &Path) -> Result<()> {
    let projects = projects.map(Ok).unwrap_or_else(default_projects_dir)?;
    let mut store = Store::open(db).context("open store")?;

    let mut sessions = 0;
    let mut spans_total = 0;
    let mut denials = 0;
    for transcript in main_transcripts(&projects)? {
        let text = fs::read_to_string(&transcript)
            .with_context(|| format!("read {}", transcript.display()))?;
        denials += count_permission_denials(&text);
        let records = parse_session(&text);
        let mut spans = extract_spans(&records, DEFAULT_IDLE_GAP_MS);
        let usage = extract_usage_events(&records);
        let subagents = subagent_costs(&transcript);
        attribute_subagents(&mut spans, &subagents);
        let sub_tokens: i64 = subagents.iter().map(|(_, tokens)| *tokens as i64).sum();
        let meta = session_meta(&transcript, sub_tokens, subagents.len() as i64);
        store.ingest_session(&meta, &spans, &usage)?;
        let prompts: Vec<(usize, i64, &str)> = extract_prompt_pointers(&text)
            .into_iter()
            .map(|(line, ts, behavior)| (line, ts, behavior.label()))
            .collect();
        store.ingest_prompts(&meta.id, &meta.source_path, &prompts)?;
        let errors: Vec<(i64, &str)> = extract_tool_errors(&text)
            .into_iter()
            .map(|(ts, category)| (ts, category.label()))
            .collect();
        store.ingest_tool_errors(&meta.id, &meta.source_path, &errors)?;
        let work = extract_work_events(&text);
        store.ingest_work_events(&meta.id, &meta.source_path, &work)?;
        sessions += 1;
        spans_total += spans.len();
    }

    // Catalog the installed config (global scope) so usage can be joined against
    // what is actually installed. Project-scoped config is a later refinement.
    let surfaces = read_global_surfaces()?;
    let surface_count = surfaces.len();
    store.replace_surfaces(&surfaces)?;

    let (sub_tokens, sub_agents) = store.subagent_totals()?;
    println!(
        "analyzed {sessions} session(s), {spans_total} skill invocation(s), \
         {surface_count} surface(s) catalogued, \
         {sub_tokens} subagent tokens across {sub_agents} subagent(s), \
         {denials} permission denial(s) -> {}",
        db.display()
    );
    Ok(())
}

/// The `(prompt_id, output_tokens)` of each of a session's subagent transcripts,
/// found at `<sessionId>/subagents/agent-*.jsonl` beside the main transcript. A
/// subagent missing a prompt id gets an empty key (it matches no span and stays
/// in the session total only).
fn subagent_costs(transcript: &Path) -> Vec<(String, u64)> {
    let subagents_dir = transcript.with_extension("").join("subagents");
    let Ok(entries) = fs::read_dir(&subagents_dir) else {
        return Vec::new();
    };
    let mut costs = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "jsonl")
            && let Ok(text) = fs::read_to_string(&path)
        {
            let prompt_id = subagent_prompt_id(&text).unwrap_or_default();
            let tokens = output_tokens(&parse_session(&text));
            costs.push((prompt_id, tokens));
        }
    }
    costs
}

fn report(by: Option<&str>, format: Format, db: &Path) -> Result<()> {
    let store = Store::open(db).context("open store")?;

    if let Some(by) = by {
        let bucket = Bucket::parse(by)
            .with_context(|| format!("unknown --by value '{by}' (year|month|week|day|hour)"))?;
        return report_by_time(&store, bucket, format);
    }

    let usage = store.skill_usage()?;
    if usage.is_empty() {
        println!("no skill usage found — run `ccoptimizer analyze` first");
        return Ok(());
    }

    // Where the tokens actually go: main-thread skill output vs subagents. The
    // subagent figure is usually the larger by far — worth seeing before reading
    // the per-skill table.
    let main_out: i64 = usage.iter().map(|row| row.out_tokens).sum();
    let (sub_tokens, sub_agents) = store.subagent_totals()?;
    println!(
        "tokens: main-thread skill output {main_out}, subagents {sub_tokens} ({sub_agents} agents)\n"
    );

    let rows: Vec<Vec<String>> = usage
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
        &["skill", "count", "out_tok", "ctx_grow", "sec"],
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

/// Skill usage rolled up per time bucket (JST). A long span is assigned whole to
/// its start bucket (`docs/specs/cli.md`).
fn report_by_time(store: &Store, bucket: Bucket, format: Format) -> Result<()> {
    let events = store.skill_event_costs()?;
    if events.is_empty() {
        println!("no skill usage found — run `ccoptimizer analyze` first");
        return Ok(());
    }

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
        &["bucket", "count", "out_tok", "ctx_grow", "sec"],
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
    let home = claude_home()?;
    let mut surfaces = read_skill_surfaces(&home.join("skills"), Scope::Global);
    surfaces.extend(read_rule_surfaces(&home.join("rules"), Scope::Global));
    surfaces.extend(read_agent_surfaces(&home.join("agents"), Scope::Global));
    surfaces.extend(read_mcp_server_surfaces(
        &home.join("mcp.json"),
        Scope::Global,
    ));
    if let Some(claude_md) =
        read_claude_md_surface(&home.join("CLAUDE.md"), "global", Scope::Global)
    {
        surfaces.push(claude_md);
    }
    Ok(surfaces)
}

/// Join the catalogued surfaces against usage: each installed surface with its
/// static cost and (for usage-measurable kinds) invocation count, unused ones
/// flagged. Usage with no matching surface is shown as orphaned.
fn surfaces(format: Format, db: &Path) -> Result<()> {
    let store = Store::open(db).context("open store")?;
    let catalog = store.catalog()?;
    let usage = store.usage_counts()?;

    if catalog.is_empty() && usage.is_empty() {
        println!("nothing catalogued — run `ccoptimizer analyze` first");
        return Ok(());
    }

    // Usage keyed by (kind, id) so every surface kind joins, not just skills.
    let counts: std::collections::HashMap<(&str, &str), i64> = usage
        .iter()
        .map(|(kind, id, count)| ((kind.as_str(), id.as_str()), *count))
        .collect();

    let mut rows: Vec<Vec<String>> = Vec::new();
    for entry in &catalog {
        let measurable = is_usage_measurable(&entry.kind);
        let uses = counts
            .get(&(entry.kind.as_str(), entry.id.as_str()))
            .copied()
            .unwrap_or(0);
        // "unused" is only meaningful for usage-measurable kinds; catalog-only
        // kinds (rules, hooks, CLAUDE.md) emit no events, so 0 means nothing.
        let (uses_cell, status) = if measurable {
            (uses.to_string(), if uses == 0 { "UNUSED" } else { "" })
        } else {
            ("-".to_string(), "(catalog-only)")
        };
        let static_tokens = entry
            .static_tokens
            .map_or_else(|| "?".to_string(), |tokens| tokens.to_string());
        rows.push(vec![
            entry.kind.clone(),
            entry.id.clone(),
            static_tokens,
            uses_cell,
            entry.load_mode.clone(),
            status.to_string(),
        ]);
    }

    let catalogued: std::collections::HashSet<(&str, &str)> = catalog
        .iter()
        .map(|e| (e.kind.as_str(), e.id.as_str()))
        .collect();
    for (kind, id, count) in &usage {
        if !catalogued.contains(&(kind.as_str(), id.as_str())) {
            rows.push(vec![
                kind.clone(),
                id.clone(),
                "-".to_string(),
                count.to_string(),
                "-".to_string(),
                "ORPHANED".to_string(),
            ]);
        }
    }

    render(
        &["kind", "id", "static", "uses", "load", "status"],
        &[
            Align::Left,
            Align::Left,
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

/// List optimization wedges, ranked by what removing each surface actually saves
/// at session start — real always-on tokens first, then MCP schemas, then
/// declutter-only (skills/agents, whose body is on-demand). This is the "where
/// and how to optimize" view, honest about which removals save context.
fn wedges(format: Format, db: &Path) -> Result<()> {
    let store = Store::open(db).context("open store")?;
    let catalog = store.catalog()?;
    let counts: std::collections::HashMap<(String, String), i64> = store
        .usage_counts()?
        .into_iter()
        .map(|(kind, id, count)| ((kind, id), count))
        .collect();

    struct Row<'a> {
        wedge: Wedge,
        kind: &'a str,
        id: &'a str,
        uses: i64,
        savings: StartupSavings,
    }

    let mut found: Vec<Row> = Vec::new();
    for entry in &catalog {
        let measurable = is_usage_measurable(&entry.kind);
        let load_mode = LoadMode::from_label(&entry.load_mode).unwrap_or(LoadMode::OnDemand);
        let uses = counts
            .get(&(entry.kind.clone(), entry.id.clone()))
            .copied()
            .unwrap_or(0);
        let static_tokens = entry.static_tokens.map(|tokens| tokens as u64);
        if let Some(wedge) =
            classify_wedge(measurable, load_mode, static_tokens, uses, HEAVY_TOKENS)
        {
            found.push(Row {
                wedge,
                kind: &entry.kind,
                id: &entry.id,
                uses,
                savings: startup_savings(load_mode, static_tokens),
            });
        }
    }

    if found.is_empty() {
        println!("no optimization wedges found — run `ccoptimizer analyze` first");
        return Ok(());
    }

    // Rank by real startup savings: measured tokens (desc), then unmeasured MCP
    // schemas, then declutter-only.
    found.sort_by_key(|row| savings_rank(row.savings));

    let rows: Vec<Vec<String>> = found
        .iter()
        .map(|row| {
            let uses_cell = if is_usage_measurable(row.kind) {
                row.uses.to_string()
            } else {
                "-".to_string()
            };
            vec![
                row.wedge.label().to_string(),
                format!("{}/{}", row.kind, row.id),
                savings_cell(row.savings),
                uses_cell,
                savings_suggestion(row.wedge, row.savings),
            ]
        })
        .collect();
    render(
        &["wedge", "surface", "saves@start", "uses", "suggestion"],
        &[
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

/// Sort key: real measured savings first (largest first), then unmeasured MCP
/// schemas, then declutter-only.
fn savings_rank(savings: StartupSavings) -> (u8, std::cmp::Reverse<u64>) {
    match savings {
        StartupSavings::Tokens(n) => (0, std::cmp::Reverse(n)),
        StartupSavings::UnknownSchema => (1, std::cmp::Reverse(0)),
        StartupSavings::Declutter => (2, std::cmp::Reverse(0)),
    }
}

fn savings_cell(savings: StartupSavings) -> String {
    match savings {
        StartupSavings::Tokens(n) => format!("{n}/sess"),
        StartupSavings::UnknownSchema => "schema?".to_string(),
        StartupSavings::Declutter => "~0".to_string(),
    }
}

/// A suggestion honest about whether removal saves context or only declutters.
fn savings_suggestion(wedge: Wedge, savings: StartupSavings) -> String {
    match savings {
        StartupSavings::Tokens(n) => format!("removing saves ~{n} tokens every session"),
        StartupSavings::UnknownSchema => {
            "disable: drops its tool schema from every session (real, unmeasured)".to_string()
        }
        StartupSavings::Declutter => match wedge {
            Wedge::Unused => "declutter only: body is on-demand, ~no startup saving".to_string(),
            _ => "heavy only when invoked; rarely used".to_string(),
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

fn session_meta(transcript: &Path, sub_tokens: i64, sub_agent_count: i64) -> SessionMeta {
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
        source_path: transcript.display().to_string(),
        sub_tokens,
        sub_agent_count,
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

fn claude_home() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".claude"))
}

/// Column alignment for the table renderer.
#[derive(Clone, Copy)]
enum Align {
    Left,
    Right,
}

/// Output format for reports.
#[derive(Clone, Copy)]
enum Format {
    Table,
    Markdown,
}

fn parse_format(value: Option<&str>) -> Result<Format> {
    match value.unwrap_or("table") {
        "table" => Ok(Format::Table),
        "markdown" | "md" => Ok(Format::Markdown),
        other => anyhow::bail!("unknown --format '{other}' (table|markdown)"),
    }
}

/// Render a table as aligned text or GitHub-flavored markdown. Auto-sizes
/// columns; `aligns` controls per-column alignment in table mode.
fn render(headers: &[&str], aligns: &[Align], rows: &[Vec<String>], format: Format) {
    match format {
        Format::Markdown => {
            println!("| {} |", headers.join(" | "));
            let sep: Vec<&str> = headers.iter().map(|_| "---").collect();
            println!("| {} |", sep.join(" | "));
            for row in rows {
                println!("| {} |", row.join(" | "));
            }
        }
        Format::Table => {
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
            println!("{}", format_row(&header_cells));
            let total: usize = widths.iter().sum::<usize>() + 2 * widths.len().saturating_sub(1);
            println!("{}", "-".repeat(total));
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
