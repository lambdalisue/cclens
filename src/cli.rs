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
    count_permission_denials, extract_prompt_pointers, parse_session, subagent_prompt_id,
};
use crate::core::bucket::{Bucket, JST_OFFSET_SECS, bucket_label};
use crate::core::span::{DEFAULT_IDLE_GAP_MS, extract_spans};
use crate::core::surface::{LoadMode, Scope, Surface, Wedge, classify_wedge, is_usage_measurable};
use crate::core::usage::{attribute_subagents, extract_usage_events, output_tokens};
use crate::store::{SessionMeta, Store};

#[derive(Parser)]
#[command(
    name = "ccoptimizer",
    about = "Find where your Claude Code config can be optimized"
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
}

pub fn run() -> Result<()> {
    match Cli::parse().command {
        Command::Analyze { projects, db } => analyze(projects, &db),
        Command::Report { by, format, db } => {
            report(by.as_deref(), parse_format(format.as_deref())?, &db)
        }
        Command::Surfaces { format, db } => surfaces(parse_format(format.as_deref())?, &db),
        Command::Wedges { format, db } => wedges(parse_format(format.as_deref())?, &db),
    }
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
        let prompts = extract_prompt_pointers(&text);
        store.ingest_prompts(&meta.id, &meta.source_path, &prompts)?;
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

    let rows: Vec<Vec<String>> = usage
        .iter()
        .map(|row| {
            vec![
                row.skill.clone(),
                row.invocations.to_string(),
                row.out_tokens.to_string(),
                row.ctx_growth.to_string(),
                row.sub_tokens.to_string(),
                format!("{:.0}", row.duration_sec),
            ]
        })
        .collect();
    render(
        &["skill", "count", "out_tok", "ctx_grow", "sub_tok", "sec"],
        &[
            Align::Left,
            Align::Right,
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

/// List optimization wedges across all catalogued surfaces, ranked by priority
/// then by static cost. This is the "where and how to optimize" view.
fn wedges(format: Format, db: &Path) -> Result<()> {
    let store = Store::open(db).context("open store")?;
    let catalog = store.catalog()?;
    let counts: std::collections::HashMap<(String, String), i64> = store
        .usage_counts()?
        .into_iter()
        .map(|(kind, id, count)| ((kind, id), count))
        .collect();

    let mut found: Vec<(Wedge, &str, &str, Option<i64>, i64)> = Vec::new();
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
            found.push((wedge, &entry.kind, &entry.id, entry.static_tokens, uses));
        }
    }

    if found.is_empty() {
        println!("no optimization wedges found — run `ccoptimizer analyze` first");
        return Ok(());
    }

    // Rank by wedge priority, then by static cost (heaviest first).
    found.sort_by(|a, b| {
        a.0.priority()
            .cmp(&b.0.priority())
            .then(b.3.unwrap_or(0).cmp(&a.3.unwrap_or(0)))
    });

    let rows: Vec<Vec<String>> = found
        .iter()
        .map(|(wedge, kind, id, static_tokens, uses)| {
            vec![
                wedge.label().to_string(),
                format!("{kind}/{id}"),
                static_tokens.map_or_else(|| "?".to_string(), |tokens| tokens.to_string()),
                uses.to_string(),
                wedge.suggestion().to_string(),
            ]
        })
        .collect();
    render(
        &["wedge", "surface", "static", "uses", "suggestion"],
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
