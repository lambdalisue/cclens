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
use crate::adapter::transcript::parse_session;
use crate::core::span::{DEFAULT_IDLE_GAP_MS, extract_spans};
use crate::core::surface::{Scope, Surface, is_usage_measurable};
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
    /// Report per-skill usage from the store.
    Report {
        #[arg(long, default_value = "ccoptimizer.db")]
        db: PathBuf,
    },
    /// Join the skill catalog against usage — installed skills, their cost, and
    /// what is unused.
    Surfaces {
        #[arg(long, default_value = "ccoptimizer.db")]
        db: PathBuf,
    },
}

pub fn run() -> Result<()> {
    match Cli::parse().command {
        Command::Analyze { projects, db } => analyze(projects, &db),
        Command::Report { db } => report(&db),
        Command::Surfaces { db } => surfaces(&db),
    }
}

fn analyze(projects: Option<PathBuf>, db: &Path) -> Result<()> {
    let projects = projects.map(Ok).unwrap_or_else(default_projects_dir)?;
    let mut store = Store::open(db).context("open store")?;

    let mut sessions = 0;
    let mut spans_total = 0;
    for transcript in main_transcripts(&projects)? {
        let text = fs::read_to_string(&transcript)
            .with_context(|| format!("read {}", transcript.display()))?;
        let spans = extract_spans(&parse_session(&text), DEFAULT_IDLE_GAP_MS);
        let meta = session_meta(&transcript);
        store.ingest_session(&meta, &spans)?;
        sessions += 1;
        spans_total += spans.len();
    }

    // Catalog the installed config (global scope) so usage can be joined against
    // what is actually installed. Project-scoped config is a later refinement.
    let surfaces = read_global_surfaces()?;
    let surface_count = surfaces.len();
    store.replace_surfaces(&surfaces)?;

    println!(
        "analyzed {sessions} session(s), {spans_total} skill invocation(s), {surface_count} surface(s) catalogued -> {}",
        db.display()
    );
    Ok(())
}

fn report(db: &Path) -> Result<()> {
    let store = Store::open(db).context("open store")?;
    let usage = store.skill_usage()?;

    if usage.is_empty() {
        println!("no skill usage found — run `ccoptimizer analyze` first");
        return Ok(());
    }

    println!(
        "{:<26}{:>7}{:>12}{:>12}{:>10}",
        "skill", "count", "out_tok", "ctx_grow", "sec"
    );
    println!("{}", "-".repeat(67));
    for row in usage {
        println!(
            "{:<26}{:>7}{:>12}{:>12}{:>10.0}",
            truncate(&row.skill, 25),
            row.invocations,
            row.out_tokens,
            row.ctx_growth,
            row.duration_sec,
        );
    }
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
fn surfaces(db: &Path) -> Result<()> {
    let store = Store::open(db).context("open store")?;
    let catalog = store.catalog()?;
    let usage = store.skill_usage()?;

    if catalog.is_empty() && usage.is_empty() {
        println!("nothing catalogued — run `ccoptimizer analyze` first");
        return Ok(());
    }

    let invocations: std::collections::HashMap<&str, i64> = usage
        .iter()
        .map(|row| (row.skill.as_str(), row.invocations))
        .collect();

    println!(
        "{:<12}{:<24}{:>9}{:>6}  {:<20}status",
        "kind", "id", "static", "uses", "load"
    );
    println!("{}", "-".repeat(82));
    for entry in &catalog {
        // Usage is currently extracted only for skills. For other
        // usage-measurable kinds (agents, MCP) we have no usage yet, so we say
        // so rather than borrow a skill's count or claim a false UNUSED.
        // Catalog-only kinds (rules, hooks, CLAUDE.md) can never have events.
        let (uses_cell, status) = if entry.kind == "skill" {
            let uses = invocations.get(entry.id.as_str()).copied().unwrap_or(0);
            (uses.to_string(), if uses == 0 { "UNUSED" } else { "" })
        } else if is_usage_measurable(&entry.kind) {
            ("?".to_string(), "(usage n/a)")
        } else {
            ("-".to_string(), "(catalog-only)")
        };
        let static_tokens = entry
            .static_tokens
            .map_or_else(|| "?".to_string(), |tokens| tokens.to_string());
        println!(
            "{:<12}{:<24}{static_tokens:>9}{uses_cell:>6}  {:<20}{status}",
            entry.kind,
            truncate(&entry.id, 23),
            entry.load_mode,
        );
    }

    let catalogued: std::collections::HashSet<&str> = catalog
        .iter()
        .filter(|e| e.kind == "skill")
        .map(|e| e.id.as_str())
        .collect();
    for row in &usage {
        if !catalogued.contains(row.skill.as_str()) {
            let id = truncate(&row.skill, 23);
            let uses = row.invocations;
            println!(
                "{:<12}{id:<24}{:>9}{uses:>6}  {:<20}ORPHANED",
                "skill", "-", "-"
            );
        }
    }
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

fn session_meta(transcript: &Path) -> SessionMeta {
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

fn truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        text.to_string()
    } else {
        text.chars().take(max_chars).collect()
    }
}
