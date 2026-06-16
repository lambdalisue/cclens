//! CLI: the `analyze` and `report` commands. This is the thin shell that walks
//! files and renders tables; the analysis itself lives in the pure core. See
//! `docs/specs/cli.md`.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use crate::adapter::config::read_skill_surfaces;
use crate::adapter::transcript::parse_session;
use crate::core::span::{DEFAULT_IDLE_GAP_MS, extract_spans};
use crate::core::surface::{Scope, is_usage_measurable};
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

    // Catalog the installed skills (global scope) so usage can be joined against
    // what is actually installed. Project-scoped skills are a later refinement.
    let surfaces = read_skill_surfaces(&default_skills_dir()?, Scope::Global);
    let surface_count = surfaces.len();
    store.replace_surfaces(&surfaces)?;

    println!(
        "analyzed {sessions} session(s), {spans_total} skill invocation(s), {surface_count} skill(s) catalogued -> {}",
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

/// Join the catalogued skills against their usage: installed skills with their
/// static cost and invocation count, unused ones flagged. Usage with no matching
/// surface is shown as orphaned (the skill was likely deleted).
fn surfaces(db: &Path) -> Result<()> {
    let store = Store::open(db).context("open store")?;
    let catalog = store.skill_catalog()?;
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
        "{:<24}{:>11}{:>7}  {:<20}status",
        "skill", "static_tok", "uses", "load"
    );
    println!("{}", "-".repeat(70));
    for entry in &catalog {
        let uses = invocations.get(entry.id.as_str()).copied().unwrap_or(0);
        // "unused" is only meaningful for usage-measurable kinds (skills are).
        let status = if uses == 0 && is_usage_measurable("skill") {
            "UNUSED"
        } else {
            ""
        };
        println!(
            "{:<24}{:>11}{:>7}  {:<20}{status}",
            truncate(&entry.id, 23),
            entry.static_tokens.unwrap_or(0),
            uses,
            entry.load_mode,
        );
    }

    let catalogued: std::collections::HashSet<&str> =
        catalog.iter().map(|e| e.id.as_str()).collect();
    let dash = "-";
    for row in &usage {
        if !catalogued.contains(row.skill.as_str()) {
            let id = truncate(&row.skill, 23);
            let uses = row.invocations;
            println!("{id:<24}{dash:>11}{uses:>7}  {dash:<20}ORPHANED");
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

fn default_skills_dir() -> Result<PathBuf> {
    Ok(claude_home()?.join("skills"))
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
