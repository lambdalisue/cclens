//! CLI: the `analyze` and `report` commands. This is the thin shell that walks
//! files and renders tables; the analysis itself lives in the pure core. See
//! `docs/specs/cli.md`.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use crate::adapter::config::{
    read_agent_surfaces, read_claude_md_surface, read_mcp_server_surfaces, read_project_surfaces,
    read_rule_surfaces, read_skill_surfaces,
};
use crate::adapter::transcript::{
    count_permission_denials, extract_prompt_pointers, extract_tool_errors, extract_work_events,
    parse_session, session_cwd, subagent_prompt_id,
};
use crate::core::bucket::{Bucket, JST_OFFSET_SECS, bucket_label};
use crate::core::friction::ErrorCategory;
use crate::core::optimize as optimize_mod;
use crate::core::scope::{ScopeFilter, split_friction};
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
    name = "cclens",
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
        #[arg(long, default_value = "cclens.db")]
        db: PathBuf,
    },
    /// Skill usage from the store — per skill, or per time bucket.
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
        #[arg(long, default_value = "cclens.db")]
        db: PathBuf,
    },
    /// Join the skill catalog against usage — installed skills, their cost, and
    /// what is unused.
    Surfaces {
        /// Restrict to a config layer: global | project | project:<slug>.
        #[arg(long)]
        scope: Option<String>,
        /// Output format: table | markdown.
        #[arg(long)]
        format: Option<String>,
        /// Read the store as-is; skip the automatic refresh.
        #[arg(long)]
        frozen: bool,
        #[arg(long, default_value = "cclens.db")]
        db: PathBuf,
    },
    /// List optimization opportunities (unused, always-on heavy, costly+rare),
    /// ranked, with a suggested action.
    Wedges {
        /// Restrict to a config layer: global | project | project:<slug>.
        #[arg(long)]
        scope: Option<String>,
        /// Output format: table | markdown.
        #[arg(long)]
        format: Option<String>,
        /// Read the store as-is; skip the automatic refresh.
        #[arg(long)]
        frozen: bool,
        #[arg(long, default_value = "cclens.db")]
        db: PathBuf,
    },
    /// Reconcile the measured always-on context against your readable config —
    /// what every session actually loads, and how much is MCP/system you cannot
    /// read from files.
    Baseline {
        /// Output format: table | markdown.
        #[arg(long)]
        format: Option<String>,
        /// Read the store as-is; skip the automatic refresh.
        #[arg(long)]
        frozen: bool,
        #[arg(long, default_value = "cclens.db")]
        db: PathBuf,
    },
    /// Show how you steer the session — the mix of steering / correcting /
    /// questioning / instructing prompts, with what it suggests.
    Prompts {
        /// Output format: table | markdown.
        #[arg(long)]
        format: Option<String>,
        /// Read the store as-is; skip the automatic refresh.
        #[arg(long)]
        frozen: bool,
        #[arg(long, default_value = "cclens.db")]
        db: PathBuf,
    },
    /// Where the work stumbles: recurring tool failures by category, ranked,
    /// with what each suggests fixing.
    Friction {
        /// Restrict to a config layer: global (failures spread across projects),
        /// project (each project's majority-owned failures), or project:<slug>
        /// (every failure in that one project).
        #[arg(long)]
        scope: Option<String>,
        /// Output format: table | markdown.
        #[arg(long)]
        format: Option<String>,
        /// Read the store as-is; skip the automatic refresh.
        #[arg(long)]
        frozen: bool,
        #[arg(long, default_value = "cclens.db")]
        db: PathBuf,
    },
    /// Thrash episodes — bursts of rapid re-edits to one file, where Claude got
    /// stuck (distinct from a healthy hotspot's spread-out edits).
    Thrash {
        /// Output format: table | markdown.
        #[arg(long)]
        format: Option<String>,
        /// Read the store as-is; skip the automatic refresh.
        #[arg(long)]
        frozen: bool,
        #[arg(long, default_value = "cclens.db")]
        db: PathBuf,
    },
    /// One-screen health check: the few most actionable findings across every
    /// view, split by the config layer that owns each fix. Start here.
    Summary {
        /// Restrict to a config layer: global | project | project:<slug>.
        #[arg(long)]
        scope: Option<String>,
        /// Read the store as-is; skip the automatic refresh.
        #[arg(long)]
        frozen: bool,
        #[arg(long, default_value = "cclens.db")]
        db: PathBuf,
    },
    /// Run an arbitrary read-only SQL query against the analyzed store. The query
    /// is the argument, or read from stdin when omitted (so `echo 'SELECT …' |
    /// cclens sql` works). A `tool_errors` view names the friction columns.
    Sql {
        /// The SQL to run. If omitted, the query is read from stdin.
        query: Option<String>,
        /// Output format: table | markdown.
        #[arg(long)]
        format: Option<String>,
        #[arg(long, default_value = "cclens.db")]
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
        #[arg(long, default_value = "cclens.db")]
        db: PathBuf,
        /// Optimize one config layer: global | project | project:<slug>.
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
        Command::Analyze { projects, db } => analyze(projects, &db),
        Command::Usage {
            by,
            format,
            frozen,
            db,
        } => usage(by.as_deref(), parse_format(format.as_deref())?, frozen, &db),
        Command::Surfaces {
            scope,
            format,
            frozen,
            db,
        } => surfaces(
            &parse_scope(scope.as_deref())?,
            parse_format(format.as_deref())?,
            frozen,
            &db,
        ),
        Command::Wedges {
            scope,
            format,
            frozen,
            db,
        } => wedges(
            &parse_scope(scope.as_deref())?,
            parse_format(format.as_deref())?,
            frozen,
            &db,
        ),
        Command::Baseline { format, frozen, db } => {
            baseline(parse_format(format.as_deref())?, frozen, &db)
        }
        Command::Prompts { format, frozen, db } => {
            prompts(parse_format(format.as_deref())?, frozen, &db)
        }
        Command::Friction {
            scope,
            format,
            frozen,
            db,
        } => friction(
            &parse_scope(scope.as_deref())?,
            parse_format(format.as_deref())?,
            frozen,
            &db,
        ),
        Command::Thrash { format, frozen, db } => {
            thrash(parse_format(format.as_deref())?, frozen, &db)
        }
        Command::Summary { scope, frozen, db } => {
            summary(&parse_scope(scope.as_deref())?, frozen, &db)
        }
        Command::Sql { query, format, db } => {
            sql(query.as_deref(), parse_format(format.as_deref())?, &db)
        }
        Command::Optimize {
            projects,
            db,
            scope,
            frozen,
            print,
        } => optimize(
            projects,
            &db,
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
            "store: refreshed just now ({} transcript(s) re-analyzed, {} unchanged)",
            stats.sessions, stats.skipped
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

/// Write `contents` to a uniquely-named file in the temp dir, readable only by
/// the current user (`0600`). Used for the optimization briefing, which may hold
/// sensitive paths/excerpts and must not sit on argv or be world-readable.
fn write_private_tempfile(contents: &str) -> Result<PathBuf> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let mut path = std::env::temp_dir();
    path.push(format!("cclens-briefing-{}.md", std::process::id()));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&path)?;
    file.write_all(contents.as_bytes())?;
    Ok(path)
}

/// Gather the COMPLETE analysis the optimization briefing carries — every view's
/// detail as owned data for `core::optimize`, so the seeded session works from
/// the briefing and need not re-run the tool. Lists are capped where a long tail
/// adds nothing for ranking.
fn collect_findings(store: &Store) -> Result<optimize_mod::Findings> {
    let floor = store.baseline_floor()?;

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

    // Thrash per project — a burst belongs to the project it happened in.
    let mut edits_by_project: std::collections::HashMap<String, Vec<(String, i64)>> =
        std::collections::HashMap::new();
    for (project, file, epoch) in store.work_event_rows_by_project("file_edit")? {
        edits_by_project
            .entry(project)
            .or_default()
            .push((file, epoch));
    }
    let mut thrash_by_project: std::collections::HashMap<String, Vec<optimize_mod::ThrashLine>> =
        edits_by_project
            .into_iter()
            .map(|(project, edits)| {
                let lines = detect_thrash(&edits, 5 * 60, 4)
                    .into_iter()
                    .take(5)
                    .map(|w| optimize_mod::ThrashLine {
                        span_secs: w.span_secs(),
                        file: w.file,
                        edits: w.edits,
                    })
                    .collect();
                (project, lines)
            })
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
                p.thrash.len(),
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
        friction_global,
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
    let surface_ref = |e: &crate::store::CatalogEntry| optimize_mod::SurfaceRef {
        kind: e.kind.clone(),
        id: e.id.clone(),
        static_tokens: e.static_tokens,
    };
    let mut scoped = ScopedWedges {
        global: WedgeRefs::default(),
        projects: std::collections::HashMap::new(),
    };
    for entry in store.effective_catalog()? {
        let refs = if entry.scope == "global" {
            &mut scoped.global
        } else {
            scoped.projects.entry(entry.project.clone()).or_default()
        };
        let load_mode = LoadMode::from_label(&entry.load_mode).unwrap_or(LoadMode::OnDemand);
        let static_tokens = entry.static_tokens.map(|t| t as u64);
        if is_usage_measurable(&entry.kind) && entry.uses == 0 {
            refs.unused.push(surface_ref(&entry));
        }
        if load_mode.is_always_on() && static_tokens.is_some_and(|t| t >= HEAVY_TOKENS) {
            refs.always_on_heavy.push(surface_ref(&entry));
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
        eprintln!("{}", freshness_line(analyzed_at.as_deref()));
    }
    let (columns, rows) = store.query(query)?;
    let headers: Vec<&str> = columns.iter().map(String::as_str).collect();
    let aligns = vec![Align::Left; columns.len()];
    render(&headers, &aligns, &rows, format);
    println!("\n{} row(s)", rows.len());
    Ok(())
}

/// One-screen health check: the few most actionable findings from every view,
/// split by the config layer that owns each fix — the global picture first,
/// then each project's own findings ("optimize my global setup" and "optimize
/// this project" are different tasks). `--scope` narrows to one layer.
fn summary(filter: &ScopeFilter, frozen: bool, db: &Path) -> Result<()> {
    let store = open_for_read(db, frozen)?;
    let f = collect_findings(&store)?;

    if filter.includes_global() {
        println!("== global — fix in ~/.claude ==");
        println!(
            "  tokens      main-thread skill output {} · subagents {} ({} agents)",
            f.main_out, f.sub_tokens, f.sub_agents
        );
        if f.floor > 0 {
            let residual = (f.floor - f.config_tokens).max(0);
            println!(
                "  always-on   ~{} tok/session = your global config {} + system/tools/MCP {}",
                f.floor, f.config_tokens, residual
            );
        }
        if let Some(cd_pct) = f.cd_pct {
            println!(
                "  workflow    cd is {cd_pct}% of Bash calls — a working-dir convention would cut the churn"
            );
        }
        println!(
            "  config      {} → `wedges --scope global`",
            count_summary(f.unused.len(), f.always_on_heavy.len())
        );
        if let (Some(steer), Some(correct)) = (f.steer_pct, f.correct_pct) {
            println!(
                "  prompting   {steer}% steering, {correct}% corrections — {}",
                if correct >= 10 || steer >= 25 {
                    "see `prompts`"
                } else {
                    "healthy mix"
                }
            );
        }
        if !f.friction_global.is_empty() {
            println!("\n  friction spread across projects (no single owner — a global habit):");
            for cat in f.friction_global.iter().take(3) {
                println!(
                    "  {:>5}  {} ({} projects) — {}",
                    cat.count,
                    cat.label,
                    cat.projects,
                    ErrorCategory::from_label(&cat.label).suggestion()
                );
            }
        }
    }

    // A project block earns its screen space with friction or config wedges;
    // thrash-only projects fold into the trailing count instead of five
    // near-empty blocks.
    const PROJECTS_SHOWN: usize = 5;
    let visible: Vec<_> = f
        .projects
        .iter()
        .filter(|p| filter.includes_project(&p.project))
        .collect();
    let shown: Vec<_> = if *filter == ScopeFilter::All {
        visible
            .iter()
            .filter(|p| {
                p.total_friction() > 0 || !p.unused.is_empty() || !p.always_on_heavy.is_empty()
            })
            .take(PROJECTS_SHOWN)
            .copied()
            .collect()
    } else {
        // An explicit project scope means the user asked for everything.
        visible.clone()
    };
    if !visible.is_empty() {
        if filter.includes_global() {
            println!("\n== projects — fix each in its own .claude / CLAUDE.md ==");
        }
        for p in &shown {
            let heading = if p.root.is_empty() {
                p.project.clone()
            } else {
                tilde_path(&p.root)
            };
            match p.total_friction() {
                0 => println!("\n  {heading}"),
                n => println!("\n  {heading} — owns {n} failure(s)"),
            }
            for cat in p.friction.iter().take(3) {
                println!(
                    "  {:>5}  {} — {}",
                    cat.count,
                    cat.label,
                    ErrorCategory::from_label(&cat.label).suggestion()
                );
            }
            if let Some(worst) = p.thrash.first() {
                println!(
                    "         thrash: {} edited {}x in {}m{}s",
                    worst.file,
                    worst.edits,
                    worst.span_secs / 60,
                    worst.span_secs % 60
                );
            }
            if !p.unused.is_empty() || !p.always_on_heavy.is_empty() {
                println!(
                    "         config: {} → `wedges --scope project:{}`",
                    count_summary(p.unused.len(), p.always_on_heavy.len()),
                    p.project
                );
            }
        }
        if visible.len() > shown.len() {
            println!(
                "\n  … and {} more project(s) with minor findings — see `summary --scope project`",
                visible.len() - shown.len()
            );
        }
    }
    Ok(())
}

/// `"N unused, M always-on heavy"` with zero parts dropped (never both zero at
/// a call site that prints it).
fn count_summary(unused: usize, always_on_heavy: usize) -> String {
    let mut parts = Vec::new();
    if unused > 0 {
        parts.push(format!("{unused} unused"));
    }
    if always_on_heavy > 0 {
        parts.push(format!("{always_on_heavy} always-on heavy"));
    }
    if parts.is_empty() {
        return "nothing flagged".to_string();
    }
    parts.join(", ")
}

/// Shorten an absolute path under $HOME to `~/…` for display.
fn tilde_path(path: &str) -> String {
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() => match path.strip_prefix(&home) {
            Some("") => "~".to_string(),
            Some(rest) if rest.starts_with('/') => format!("~{rest}"),
            _ => path.to_string(),
        },
        _ => path.to_string(),
    }
}

/// Thrash bursts: a file edited many times in a short window — where Claude got
/// stuck and kept retrying, as opposed to a hotspot's healthy spread-out edits.
fn thrash(format: Format, frozen: bool, db: &Path) -> Result<()> {
    const GAP_SECS: i64 = 5 * 60;
    const MIN_EDITS: u32 = 4;
    let store = open_for_read(db, frozen)?;
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

/// Where the work stumbles: recurring tool failures by category, ranked, each
/// with what it suggests fixing. This is about the work, not the config —
/// recurring failures are fixable friction that wastes turns and tokens.
/// `--scope` routes by ownership (`core::scope`): `global` shows the failures
/// no project owns, `project` each project's majority-owned failures, and
/// `project:<slug>` everything that happened in that one project.
fn friction(filter: &ScopeFilter, format: Format, frozen: bool, db: &Path) -> Result<()> {
    let store = open_for_read(db, frozen)?;
    let cells = store.error_counts_by_project()?;

    // (project column shown?, rows)
    let (with_project, counts): (bool, Vec<(String, String, i64)>) = match filter {
        ScopeFilter::All => (
            false,
            store
                .error_counts()?
                .into_iter()
                .map(|(label, n)| (String::new(), label, n))
                .collect(),
        ),
        ScopeFilter::Global => (
            false,
            split_friction(&cells)
                .global
                .into_iter()
                .map(|g| {
                    (
                        String::new(),
                        format!("{} (across {} projects)", g.category, g.projects),
                        g.total,
                    )
                })
                .collect(),
        ),
        ScopeFilter::Project(None) => (
            true,
            split_friction(&cells)
                .per_project
                .into_iter()
                .flat_map(|(project, cats)| {
                    cats.into_iter()
                        .map(move |(label, n)| (project.clone(), label, n))
                })
                .collect(),
        ),
        ScopeFilter::Project(Some(slug)) => (
            false,
            cells
                .into_iter()
                .filter(|(project, _, _)| project == slug)
                .map(|(_, label, n)| (String::new(), label, n))
                .collect(),
        ),
    };
    let total: i64 = counts.iter().map(|(_, _, n)| n).sum();
    if total == 0 {
        println!("no tool failures found for this scope — run `cclens analyze` first?");
        return Ok(());
    }

    let rows: Vec<Vec<String>> = counts
        .iter()
        .map(|(project, label, n)| {
            let mut row = Vec::new();
            if with_project {
                row.push(project.clone());
            }
            let category = label.split(' ').next().unwrap_or(label);
            row.extend([
                label.clone(),
                n.to_string(),
                ErrorCategory::from_label(category).suggestion().to_string(),
            ]);
            row
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
    println!("\n{total} tool failures (categories are lexical heuristics)");
    Ok(())
}

/// Show the mix of how the user steers the session, and what it suggests. Heavy
/// steering points to room for more autonomy; frequent corrections point to
/// clearer upfront specs. The classes are lexical heuristics (`core::prompt`).
fn prompts(format: Format, frozen: bool, db: &Path) -> Result<()> {
    let store = open_for_read(db, frozen)?;
    let counts = store.prompt_behavior_counts()?;
    let total: i64 = counts.iter().map(|(_, n)| n).sum();
    if total == 0 {
        println!("no prompts found — run `cclens analyze` first");
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
fn baseline(format: Format, frozen: bool, db: &Path) -> Result<()> {
    let store = open_for_read(db, frozen)?;
    let floor = store.baseline_floor()?;
    if floor == 0 {
        println!("no data — run `cclens analyze` first");
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

/// Counters from one analyze run, for the command's summary line and the
/// freshness header on auto-refreshing reads.
struct AnalyzeStats {
    sessions: usize,
    skipped: usize,
    spans_total: usize,
    surface_count: usize,
    denials: usize,
}

fn analyze(projects: Option<PathBuf>, db: &Path) -> Result<()> {
    let stats = run_analyze(projects, db)?;
    let store = Store::open(db).context("open store")?;
    let (sub_tokens, sub_agents) = store.subagent_totals()?;
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
        if let Some((mtime, size)) = fingerprint
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
        let subagents = subagent_costs(&transcript);
        attribute_subagents(&mut spans, &subagents);
        let sub_tokens: i64 = subagents.iter().map(|(_, tokens)| *tokens as i64).sum();
        let root = session_cwd(&text).map(|cwd| normalize_root(&cwd));
        let meta = session_meta(
            &transcript,
            root.unwrap_or_default(),
            sub_tokens,
            subagents.len() as i64,
        );
        store.ingest_session(&meta, &spans, &usage)?;
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
        let work = extract_work_events(&text);
        store.ingest_work_events(&meta.id, &meta.source_path, &work)?;
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
            surfaces.extend(read_project_surfaces(Path::new(&root), &project));
        }
    }
    let surface_count = surfaces.len();
    store.replace_surfaces(&surfaces)?;

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
            "store: refreshed just now ({} transcript(s) re-analyzed, {} unchanged)",
            stats.sessions, stats.skipped
        );
        return Store::open(db).context("open store");
    }

    let store = Store::open(db).context("open store")?;
    eprintln!("{}", freshness_line(store.meta("analyzed_at")?.as_deref()));
    Ok(store)
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
fn normalize_root(cwd: &str) -> String {
    match cwd.split_once("/.wt/") {
        Some((parent, _)) => parent.to_string(),
        None => cwd.to_string(),
    }
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

fn usage(by: Option<&str>, format: Format, frozen: bool, db: &Path) -> Result<()> {
    let store = open_for_read(db, frozen)?;

    if let Some(by) = by {
        let bucket = Bucket::parse(by)
            .with_context(|| format!("unknown --by value '{by}' (year|month|week|day|hour)"))?;
        return usage_by_time(&store, bucket, format);
    }

    let skills = store.skill_usage()?;
    if skills.is_empty() {
        println!("no skill usage found — run `cclens analyze` first");
        return Ok(());
    }

    // Where the tokens actually go: main-thread skill output vs subagents. The
    // subagent figure is usually the larger by far — worth seeing before reading
    // the per-skill table.
    let main_out: i64 = skills.iter().map(|row| row.out_tokens).sum();
    let (sub_tokens, sub_agents) = store.subagent_totals()?;
    println!(
        "tokens: main-thread skill output {main_out}, subagents {sub_tokens} ({sub_agents} agents)\n"
    );

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
fn usage_by_time(store: &Store, bucket: Bucket, format: Format) -> Result<()> {
    let events = store.skill_event_costs()?;
    if events.is_empty() {
        println!("no skill usage found — run `cclens analyze` first");
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
    let scope = Scope::Global;
    let mut surfaces = read_skill_surfaces(&home.join("skills"), &scope);
    surfaces.extend(read_rule_surfaces(&home.join("rules"), &scope));
    surfaces.extend(read_agent_surfaces(&home.join("agents"), &scope));
    surfaces.extend(read_mcp_server_surfaces(&home.join("mcp.json"), &scope));
    if let Some(claude_md) = read_claude_md_surface(&home.join("CLAUDE.md"), "global", &scope) {
        surfaces.push(claude_md);
    }
    Ok(surfaces)
}

/// Join the catalogued surfaces against usage: each installed surface with its
/// static cost and (for usage-measurable kinds) invocation count, unused ones
/// flagged. Usage with no matching surface is shown as orphaned.
fn surfaces(filter: &ScopeFilter, format: Format, frozen: bool, db: &Path) -> Result<()> {
    let store = open_for_read(db, frozen)?;
    let catalog: Vec<_> = store
        .effective_catalog()?
        .into_iter()
        .filter(|e| in_scope(filter, &e.scope, &e.project))
        .collect();
    let usage = store.usage_counts()?;

    if catalog.is_empty() && usage.is_empty() {
        println!("nothing catalogued — run `cclens analyze` first");
        return Ok(());
    }

    let mut rows: Vec<Vec<String>> = Vec::new();
    for entry in &catalog {
        let measurable = is_usage_measurable(&entry.kind);
        // "unused" is only meaningful for usage-measurable kinds; catalog-only
        // kinds (rules, hooks, CLAUDE.md) emit no events, so 0 means nothing.
        let (uses_cell, status) = if measurable {
            (
                entry.uses.to_string(),
                if entry.uses == 0 { "UNUSED" } else { "" },
            )
        } else {
            ("-".to_string(), "(catalog-only)")
        };
        let static_tokens = entry
            .static_tokens
            .map_or_else(|| "?".to_string(), |tokens| tokens.to_string());
        rows.push(vec![
            entry.kind.clone(),
            entry.id.clone(),
            scope_cell(&entry.scope, &entry.project),
            static_tokens,
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
                count.to_string(),
                "-".to_string(),
                "ORPHANED".to_string(),
            ]);
        }
    }

    render(
        &["kind", "id", "scope", "static", "uses", "load", "status"],
        &[
            Align::Left,
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
fn wedges(filter: &ScopeFilter, format: Format, frozen: bool, db: &Path) -> Result<()> {
    let store = open_for_read(db, frozen)?;
    let catalog: Vec<_> = store
        .effective_catalog()?
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
        savings: StartupSavings,
    }

    let mut found: Vec<Row> = Vec::new();
    for entry in &catalog {
        let measurable = is_usage_measurable(&entry.kind);
        let load_mode = LoadMode::from_label(&entry.load_mode).unwrap_or(LoadMode::OnDemand);
        let static_tokens = entry.static_tokens.map(|tokens| tokens as u64);
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
                savings: startup_savings(load_mode, static_tokens),
            });
        }
    }

    if found.is_empty() {
        println!("no optimization wedges found — run `cclens analyze` first");
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
                scope_cell(row.scope, row.project),
                savings_cell(row.savings),
                uses_cell,
                savings_suggestion(row.wedge, row.savings),
            ]
        })
        .collect();
    render(
        &[
            "wedge",
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

fn session_meta(
    transcript: &Path,
    root: String,
    sub_tokens: i64,
    sub_agent_count: i64,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_root_folds_a_worktree_path_onto_its_repo() {
        assert_eq!(
            normalize_root("/tmp/example/repo/.wt/feat-x"),
            "/tmp/example/repo"
        );
        // Idempotent: an already-folded root stays put.
        assert_eq!(normalize_root("/tmp/example/repo"), "/tmp/example/repo");
    }
}
