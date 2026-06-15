//! CLI: the `analyze` and `report` commands. This is the thin shell that walks
//! files and renders tables; the analysis itself lives in the pure core. See
//! `docs/specs/cli.md`.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use crate::adapter::transcript::parse_session;
use crate::core::span::extract_spans;
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
}

pub fn run() -> Result<()> {
    match Cli::parse().command {
        Command::Analyze { projects, db } => analyze(projects, &db),
        Command::Report { db } => report(&db),
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
        let spans = extract_spans(&parse_session(&text));
        let meta = session_meta(&transcript);
        store.ingest_session(&meta, &spans)?;
        sessions += 1;
        spans_total += spans.len();
    }

    println!(
        "analyzed {sessions} session(s), {spans_total} skill invocation(s) -> {}",
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
    let home = std::env::var("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".claude").join("projects"))
}

fn truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        text.to_string()
    } else {
        text.chars().take(max_chars).collect()
    }
}
