//! The `cache` view and the doctor's prompt-cache lines: how well the prompt
//! cache held, why it broke, and which surfaces keep breaking it
//! (`docs/specs/cli.md`, `events.md` "Prompt cache").

use std::path::Path;

use anyhow::{Context, Result};

use super::{Align, Format, Style, emit_json, fmt_tokens, open_for_read, preamble, render};
use crate::core::bucket::{Bucket, JST_OFFSET_SECS, bucket_label};
use crate::core::cache::{
    BreakCause, CacheRow, CacheTotals, CauseBreaks, RECURRING_BREAK_SESSIONS, SurfaceBreaks,
    breaks_by_cause, breaks_by_surface, totals, totals_by,
};
use crate::store::Store;

/// What the main table's rows are.
enum Grouping {
    Session,
    Project,
    Time(Bucket),
}

impl Grouping {
    fn parse(value: &str) -> Option<Grouping> {
        match value {
            "session" => Some(Grouping::Session),
            "project" => Some(Grouping::Project),
            other => Bucket::parse(other).map(Grouping::Time),
        }
    }

    fn header(&self) -> &'static str {
        match self {
            Grouping::Session => "session",
            Grouping::Project => "project",
            Grouping::Time(_) => "bucket",
        }
    }

    /// `(group, session id)`. A session is grouped by its full id — only the
    /// displayed label is shortened, so two ids sharing a prefix never merge.
    fn key(&self, row: &CacheRow) -> (String, Option<String>) {
        match self {
            Grouping::Session => (row.project.clone(), Some(row.session_id.clone())),
            Grouping::Project => (row.project.clone(), None),
            Grouping::Time(bucket) => (
                bucket_label(row.started_epoch, *bucket, JST_OFFSET_SECS),
                None,
            ),
        }
    }

    fn label((group, session): &(String, Option<String>)) -> String {
        match session {
            Some(id) => format!("{group} {}", short_id(id)),
            None => group.clone(),
        }
    }
}

fn short_id(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

/// Everything both the view and the doctor read, computed once.
pub(super) struct CacheSummary {
    rows: Vec<CacheRow>,
    pub(super) totals: CacheTotals,
    pub(super) causes: Vec<CauseBreaks>,
    pub(super) surfaces: Vec<SurfaceBreaks>,
    /// `(sum, sessions reporting, sessions analyzed)`.
    pub(super) reported_cost: (f64, i64, i64),
}

impl CacheSummary {
    pub(super) fn read(store: &Store) -> Result<CacheSummary> {
        let rows = store.cache_rows()?;
        let (cost, reporting) = store.reported_cost()?;
        let (sessions, _) = store.session_stats()?;
        Ok(CacheSummary {
            totals: totals(&rows),
            causes: breaks_by_cause(&rows),
            surfaces: breaks_by_surface(&rows),
            reported_cost: (cost, reporting, sessions),
            rows,
        })
    }

    /// Surfaces that broke the cache in enough sessions to be a config habit.
    pub(super) fn recurring_surfaces(&self) -> impl Iterator<Item = &SurfaceBreaks> {
        self.surfaces
            .iter()
            .filter(|s| s.sessions >= RECURRING_BREAK_SESSIONS)
    }

    pub(super) fn json(&self) -> serde_json::Value {
        let (cost, reporting, sessions) = self.reported_cost;
        serde_json::json!({
            "totals": totals_json(&self.totals),
            "reported_cost": {
                "total_usd": (reporting > 0).then_some(cost),
                "sessions_reporting": reporting,
                "sessions": sessions,
            },
            "causes": self.causes,
            "surfaces": self.surfaces,
        })
    }

    /// The doctor's COST line, or nothing when no request was measured.
    pub(super) fn doctor_lines(&self, style: &Style) -> Vec<String> {
        let Some(hit) = self.totals.hit_rate() else {
            return Vec::new();
        };
        let mut lines = vec![format!(
            "Prompt cache: {} of prompt tokens were read from cache across {} requests.",
            style.bold(&pct(hit)),
            self.totals.requests
        )];
        if let Some(top) = self.causes.first() {
            lines.push(format!(
                "{} cache breaks re-wrote {} tokens; the most costly cause is {} ({}). Details: {}",
                self.totals.breaks,
                fmt_tokens(self.totals.rebuilt_tokens),
                top.cause,
                fmt_tokens(top.rebuilt_tokens),
                style.command("cclens cache")
            ));
        }
        let (cost, reporting, sessions) = self.reported_cost;
        if reporting > 0 {
            lines.push(format!(
                "Claude Code reported ${cost:.2} for the {reporting} of {sessions} sessions that recorded a cost."
            ));
        }
        lines
    }
}

fn pct(share: f64) -> String {
    format!("{:.1}%", share * 100.0)
}

fn opt_pct(share: Option<f64>) -> String {
    share.map(pct).unwrap_or_else(|| "-".to_string())
}

fn totals_json(t: &CacheTotals) -> serde_json::Value {
    let mut value = serde_json::to_value(t).expect("totals serialize");
    let object = value.as_object_mut().expect("object");
    object.insert("hit_rate".into(), serde_json::json!(t.hit_rate()));
    object.insert(
        "write_1h_share".into(),
        serde_json::json!(t.write_1h_share()),
    );
    value
}

pub(super) fn cache(by: Option<&str>, format: Format, frozen: bool, db: &Path) -> Result<()> {
    let grouping = match by {
        None => Grouping::Project,
        Some(by) => Grouping::parse(by).with_context(|| {
            format!("unknown --by value '{by}' (session|project|year|month|week|day|hour)")
        })?,
    };
    let store = open_for_read(db, frozen)?;
    let summary = CacheSummary::read(&store)?;
    let groups = totals_by(&summary.rows, |row| grouping.key(row));

    if format == Format::Json {
        let mut value = summary.json();
        let rows: Vec<serde_json::Value> = groups
            .iter()
            .map(|((group, session), t)| {
                let mut row = totals_json(t);
                let object = row.as_object_mut().expect("object");
                match session {
                    Some(id) => {
                        object.insert("project".into(), serde_json::json!(group));
                        object.insert("session_id".into(), serde_json::json!(id));
                    }
                    None => {
                        object.insert(grouping.header().into(), serde_json::json!(group));
                    }
                }
                row
            })
            .collect();
        value
            .as_object_mut()
            .expect("object")
            .insert("rows".into(), serde_json::json!(rows));
        return emit_json(&value);
    }
    if summary.rows.is_empty() {
        println!(
            "no prompt-cache data found — run `cclens analyze` (transcripts written before \
             Claude Code recorded cache usage have none)"
        );
        return Ok(());
    }

    preamble(
        format,
        "How well the prompt cache held on main-thread requests. hit rate = share of\n\
         prompt tokens read from cache rather than written again or sent uncached.\n\
         A break is a request that could not reuse what the request before it cached;\n\
         rebuilt = the tokens it had to write again. 1h share = cache writes made with\n\
         the 1-hour lifetime, which costs more than the 5-minute one.",
    );
    let t = &summary.totals;
    println!(
        "{} requests · hit rate {} · cache writes {} (1h share {}) · {} breaks rebuilt {} tokens ({} after idle)",
        t.requests,
        opt_pct(t.hit_rate()),
        fmt_tokens(t.cache_write()),
        opt_pct(t.write_1h_share()),
        t.breaks,
        fmt_tokens(t.rebuilt_tokens),
        fmt_tokens(t.idle_rebuilt_tokens),
    );
    let (cost, reporting, sessions) = summary.reported_cost;
    if reporting > 0 {
        println!(
            "Claude Code reported ${cost:.2} for {reporting} of {sessions} sessions; the rest recorded no cost (unknown, not free)."
        );
    }
    println!();

    let rows: Vec<Vec<String>> = groups
        .iter()
        .map(|(key, t)| {
            vec![
                Grouping::label(key),
                t.requests.to_string(),
                opt_pct(t.hit_rate()),
                t.cache_write().to_string(),
                opt_pct(t.write_1h_share()),
                t.breaks.to_string(),
                t.rebuilt_tokens.to_string(),
                t.idle_rebuilt_tokens.to_string(),
            ]
        })
        .collect();
    render(
        &[
            grouping.header(),
            "requests",
            "hit rate",
            "cache writes",
            "1h share",
            "breaks",
            "rebuilt",
            "rebuilt after idle",
        ],
        &[
            Align::Left,
            Align::Right,
            Align::Right,
            Align::Right,
            Align::Right,
            Align::Right,
            Align::Right,
            Align::Right,
        ],
        &rows,
        format,
    );

    if !summary.causes.is_empty() {
        println!();
        let rows: Vec<Vec<String>> = summary
            .causes
            .iter()
            .map(|c| {
                vec![
                    c.cause.to_string(),
                    c.breaks.to_string(),
                    c.rebuilt_tokens.to_string(),
                    BreakCause::from_label(c.cause)
                        .map(BreakCause::suggestion)
                        .unwrap_or_default()
                        .to_string(),
                ]
            })
            .collect();
        render(
            &["cause", "breaks", "rebuilt", "what it means"],
            &[Align::Left, Align::Right, Align::Right, Align::Left],
            &rows,
            format,
        );
    }

    if !summary.surfaces.is_empty() {
        println!();
        let rows: Vec<Vec<String>> = summary
            .surfaces
            .iter()
            .map(|s| {
                vec![
                    s.surface.clone(),
                    s.sessions.to_string(),
                    s.breaks.to_string(),
                    s.rebuilt_tokens.to_string(),
                ]
            })
            .collect();
        render(
            &[
                "surface whose tools changed",
                "sessions",
                "breaks",
                "rebuilt",
            ],
            &[Align::Left, Align::Right, Align::Right, Align::Right],
            &rows,
            format,
        );
        if format == Format::Table {
            println!(
                "{}",
                Style::stdout().dim(
                    "A break that changed several surfaces counts for each, so this column does not sum.\n\
                     An MCP server that connects at session start, rather than partway through,\n\
                     keeps the tool list — and the cache — stable."
                )
            );
        }
    }
    Ok(())
}
