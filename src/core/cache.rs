//! Prompt-cache efficiency: how much of each main-thread request's prompt was
//! read from the cache, and — when the cache broke — why, and which surfaces
//! the break implicates (`docs/specs/events.md`, "Prompt cache").
//!
//! Pure: the adapter hands over the session's request sequence and shapes; the
//! break threshold is injected.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde::Serialize;

use crate::core::usage::mcp_server_of;

/// A request breaks the cache when it reads less than this share of what the
/// previous request had cached.
pub const DEFAULT_MIN_REUSE: f64 = 0.5;

/// A surface that broke the cache in this many sessions is a config habit worth
/// fixing, not one unlucky session.
pub const RECURRING_BREAK_SESSIONS: i64 = 3;

/// The lifetime a cache entry was written with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheTtl {
    FiveMinutes,
    OneHour,
}

impl CacheTtl {
    pub fn millis(self) -> i64 {
        match self {
            CacheTtl::FiveMinutes => 5 * 60 * 1000,
            CacheTtl::OneHour => 60 * 60 * 1000,
        }
    }
}

/// One main-thread API request, its prompt split three ways.
#[derive(Debug, Clone, PartialEq)]
pub struct CacheRequest {
    pub sent_epoch_ms: i64,
    pub model: String,
    pub uncached: u64,
    pub cache_read: u64,
    pub cache_write_5m: u64,
    pub cache_write_1h: u64,
    /// The lifetime the request asked for, when the transcript says.
    pub requested_ttl: Option<CacheTtl>,
    /// The request shape's identity, joining `CacheTrace::shapes`.
    pub shape: Option<String>,
}

impl CacheRequest {
    fn cached(&self) -> u64 {
        self.cache_read + self.cache_write_5m + self.cache_write_1h
    }

    fn written(&self) -> u64 {
        self.cache_write_5m + self.cache_write_1h
    }

    /// The lifetime this request itself shows: the one asked for, else the one
    /// it wrote with. A request that asked for none and wrote nothing shows
    /// none — a pure hit says nothing about the lifetime.
    fn shown_ttl(&self) -> Option<CacheTtl> {
        self.requested_ttl.or(if self.cache_write_1h > 0 {
            Some(CacheTtl::OneHour)
        } else if self.cache_write_5m > 0 {
            Some(CacheTtl::FiveMinutes)
        } else {
            None
        })
    }
}

/// What the session did, in order, as far as the cache is concerned.
#[derive(Debug, Clone, PartialEq)]
pub enum CacheStep {
    Request(CacheRequest),
    Compaction,
}

/// What a request shape was made of: tool names and a digest per system block.
/// Only compared, so the system text itself never leaves the adapter.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RequestShape {
    /// Tool name → a digest of its definition, so a tool whose schema changed
    /// under the same name is told apart from one left as it was.
    pub tools: BTreeMap<String, u64>,
    pub system: Vec<u64>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct CacheTrace {
    pub steps: Vec<CacheStep>,
    pub shapes: HashMap<String, RequestShape>,
}

/// The one most likely reason a request broke the cache, in the precedence
/// order the spec defines.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum BreakCause {
    IdleExpired,
    ModelSwitch,
    TtlChanged,
    Compaction,
    ToolsChanged,
    SystemChanged,
    ShapeChanged,
    Unknown,
}

impl BreakCause {
    pub fn label(self) -> &'static str {
        match self {
            BreakCause::IdleExpired => "idle_expired",
            BreakCause::ModelSwitch => "model_switch",
            BreakCause::TtlChanged => "ttl_changed",
            BreakCause::Compaction => "compaction",
            BreakCause::ToolsChanged => "tools_changed",
            BreakCause::SystemChanged => "system_changed",
            BreakCause::ShapeChanged => "shape_changed",
            BreakCause::Unknown => "unknown",
        }
    }

    pub fn from_label(label: &str) -> Option<BreakCause> {
        [
            BreakCause::IdleExpired,
            BreakCause::ModelSwitch,
            BreakCause::TtlChanged,
            BreakCause::Compaction,
            BreakCause::ToolsChanged,
            BreakCause::SystemChanged,
            BreakCause::ShapeChanged,
            BreakCause::Unknown,
        ]
        .into_iter()
        .find(|cause| cause.label() == label)
    }

    /// What a reader can do about breaks of this cause.
    pub fn suggestion(self) -> &'static str {
        match self {
            BreakCause::IdleExpired => {
                "the cache expired while the session sat idle; long pauses re-pay the whole prompt"
            }
            BreakCause::ModelSwitch => {
                "each model has its own cache; switching mid-session re-pays the prompt"
            }
            BreakCause::TtlChanged => {
                "the request asked for a different cache lifetime than the one before it"
            }
            BreakCause::Compaction => "compaction rewrites the history the cache was built on",
            BreakCause::ToolsChanged => {
                "the tool list changed mid-session (a late MCP server, a deferred tool loaded on demand)"
            }
            BreakCause::SystemChanged => "the system prompt changed mid-session",
            BreakCause::ShapeChanged => {
                "the request shape changed, but what changed was not recorded"
            }
            BreakCause::Unknown => "no recorded change explains it",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct CacheBreak {
    pub cause: BreakCause,
    /// Previously cached tokens this request had to write again.
    pub rebuilt_tokens: u64,
    /// `kind:id` of each surface the break implicates, sorted.
    pub surfaces: Vec<String>,
}

/// A request as measured against the one before it.
#[derive(Debug, Clone, PartialEq)]
pub struct CacheObservation {
    pub request: CacheRequest,
    /// Since the previous request was sent; `None` for the session's first.
    pub gap_ms: Option<i64>,
    /// The lifetime the gap is judged against: the previous request's, or this
    /// request's own on the first.
    pub ttl: CacheTtl,
    pub cache_break: Option<CacheBreak>,
}

/// Measure every request of one session against the one before it.
pub fn observe_cache(trace: &CacheTrace, min_reuse: f64) -> Vec<CacheObservation> {
    let mut observations = Vec::new();
    // `(request, the lifetime it lives under)`: a request showing no lifetime
    // of its own inherits the last one seen, else the API default.
    let mut previous: Option<(&CacheRequest, CacheTtl)> = None;
    let mut compacted = false;
    for step in &trace.steps {
        let request = match step {
            CacheStep::Compaction => {
                compacted = true;
                continue;
            }
            CacheStep::Request(request) => request,
        };
        let own_ttl = request
            .shown_ttl()
            .or(previous.map(|(_, ttl)| ttl))
            .unwrap_or(CacheTtl::FiveMinutes);
        let observation = match previous {
            None => CacheObservation {
                request: request.clone(),
                gap_ms: None,
                ttl: own_ttl,
                cache_break: None,
            },
            Some((prev, ttl)) => {
                let gap_ms = request.sent_epoch_ms - prev.sent_epoch_ms;
                let was_cached = prev.cached();
                let broke =
                    was_cached > 0 && (request.cache_read as f64) < was_cached as f64 * min_reuse;
                let cache_break = broke.then(|| {
                    let (cause, surfaces) = break_cause(
                        prev,
                        request,
                        gap_ms > ttl.millis(),
                        ttl != own_ttl,
                        compacted,
                        &trace.shapes,
                    );
                    CacheBreak {
                        cause,
                        // Capped by what was written: a prompt that shrank
                        // (compaction) dropped part of the old prefix rather
                        // than re-paying it.
                        rebuilt_tokens: (was_cached - request.cache_read).min(request.written()),
                        surfaces,
                    }
                });
                CacheObservation {
                    request: request.clone(),
                    gap_ms: Some(gap_ms),
                    ttl,
                    cache_break,
                }
            }
        };
        observations.push(observation);
        previous = Some((request, own_ttl));
        compacted = false;
    }
    observations
}

fn break_cause(
    prev: &CacheRequest,
    request: &CacheRequest,
    idle_expired: bool,
    ttl_changed: bool,
    compacted: bool,
    shapes: &HashMap<String, RequestShape>,
) -> (BreakCause, Vec<String>) {
    if idle_expired {
        return (BreakCause::IdleExpired, Vec::new());
    }
    if prev.model != request.model {
        return (BreakCause::ModelSwitch, Vec::new());
    }
    if ttl_changed {
        return (BreakCause::TtlChanged, Vec::new());
    }
    if compacted {
        return (BreakCause::Compaction, Vec::new());
    }
    let (Some(before), Some(after)) = (&prev.shape, &request.shape) else {
        return (BreakCause::Unknown, Vec::new());
    };
    if before == after {
        return (BreakCause::Unknown, Vec::new());
    }
    let (Some(before), Some(after)) = (shapes.get(before), shapes.get(after)) else {
        return (BreakCause::ShapeChanged, Vec::new());
    };
    if before.tools != after.tools {
        let surfaces: BTreeSet<String> = before
            .tools
            .keys()
            .chain(after.tools.keys())
            .filter(|tool| before.tools.get(*tool) != after.tools.get(*tool))
            .map(|tool| tool_surface(tool))
            .collect();
        return (BreakCause::ToolsChanged, surfaces.into_iter().collect());
    }
    if before.system != after.system {
        return (BreakCause::SystemChanged, Vec::new());
    }
    (BreakCause::ShapeChanged, Vec::new())
}

/// One stored request, as reports read it back.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct CacheRow {
    pub session_id: String,
    pub project: String,
    pub started_epoch: i64,
    pub uncached: i64,
    pub cache_read: i64,
    pub cache_write_5m: i64,
    pub cache_write_1h: i64,
    pub break_cause: Option<BreakCause>,
    pub rebuilt_tokens: i64,
    pub surfaces: Vec<String>,
}

/// Sums over a set of requests. The rates are derived on demand so a sum of
/// sums stays exact.
#[derive(Debug, Clone, PartialEq, Default, Serialize)]
pub struct CacheTotals {
    pub requests: i64,
    pub uncached: i64,
    pub cache_read: i64,
    pub cache_write_5m: i64,
    pub cache_write_1h: i64,
    pub breaks: i64,
    pub rebuilt_tokens: i64,
    pub idle_rebuilt_tokens: i64,
}

impl CacheTotals {
    pub fn add(&mut self, row: &CacheRow) {
        self.requests += 1;
        self.uncached += row.uncached;
        self.cache_read += row.cache_read;
        self.cache_write_5m += row.cache_write_5m;
        self.cache_write_1h += row.cache_write_1h;
        if let Some(cause) = row.break_cause {
            self.breaks += 1;
            self.rebuilt_tokens += row.rebuilt_tokens;
            if cause == BreakCause::IdleExpired {
                self.idle_rebuilt_tokens += row.rebuilt_tokens;
            }
        }
    }

    pub fn cache_write(&self) -> i64 {
        self.cache_write_5m + self.cache_write_1h
    }

    /// Share of prompt tokens read from the cache; `None` with no prompt tokens.
    pub fn hit_rate(&self) -> Option<f64> {
        let prompt = self.cache_read + self.cache_write() + self.uncached;
        (prompt > 0).then(|| self.cache_read as f64 / prompt as f64)
    }

    /// Share of cache writes made with the 1-hour lifetime.
    pub fn write_1h_share(&self) -> Option<f64> {
        let written = self.cache_write();
        (written > 0).then(|| self.cache_write_1h as f64 / written as f64)
    }
}

pub fn totals(rows: &[CacheRow]) -> CacheTotals {
    let mut totals = CacheTotals::default();
    for row in rows {
        totals.add(row);
    }
    totals
}

/// Totals per key, in key order.
pub fn totals_by<K: Ord>(
    rows: &[CacheRow],
    key: impl Fn(&CacheRow) -> K,
) -> BTreeMap<K, CacheTotals> {
    let mut grouped: BTreeMap<K, CacheTotals> = BTreeMap::new();
    for row in rows {
        grouped.entry(key(row)).or_default().add(row);
    }
    grouped
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CauseBreaks {
    pub cause: &'static str,
    pub breaks: i64,
    pub rebuilt_tokens: i64,
}

/// Breaks per cause, costliest first.
pub fn breaks_by_cause(rows: &[CacheRow]) -> Vec<CauseBreaks> {
    let mut grouped: BTreeMap<BreakCause, (i64, i64)> = BTreeMap::new();
    for row in rows {
        if let Some(cause) = row.break_cause {
            let entry = grouped.entry(cause).or_default();
            entry.0 += 1;
            entry.1 += row.rebuilt_tokens;
        }
    }
    let mut ranked: Vec<CauseBreaks> = grouped
        .into_iter()
        .map(|(cause, (breaks, rebuilt_tokens))| CauseBreaks {
            cause: cause.label(),
            breaks,
            rebuilt_tokens,
        })
        .collect();
    ranked.sort_by_key(|c| std::cmp::Reverse(c.rebuilt_tokens));
    ranked
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SurfaceBreaks {
    /// `kind:id`.
    pub surface: String,
    pub breaks: i64,
    /// Distinct sessions the surface broke the cache in — what makes it a
    /// habit of the config rather than one bad session.
    pub sessions: i64,
    /// Rebuilt tokens of every break naming this surface. A break naming
    /// several surfaces credits each, so these do not sum across surfaces.
    pub rebuilt_tokens: i64,
}

/// Breaks per implicated surface, the ones spread over most sessions first.
pub fn breaks_by_surface(rows: &[CacheRow]) -> Vec<SurfaceBreaks> {
    let mut grouped: BTreeMap<&str, (i64, BTreeSet<&str>, i64)> = BTreeMap::new();
    for row in rows.iter().filter(|row| row.break_cause.is_some()) {
        for surface in &row.surfaces {
            let entry = grouped.entry(surface.as_str()).or_default();
            entry.0 += 1;
            entry.1.insert(row.session_id.as_str());
            entry.2 += row.rebuilt_tokens;
        }
    }
    let mut ranked: Vec<SurfaceBreaks> = grouped
        .into_iter()
        .map(
            |(surface, (breaks, sessions, rebuilt_tokens))| SurfaceBreaks {
                surface: surface.to_string(),
                breaks,
                sessions: sessions.len() as i64,
                rebuilt_tokens,
            },
        )
        .collect();
    ranked.sort_by_key(|s| std::cmp::Reverse((s.sessions, s.rebuilt_tokens)));
    ranked
}

/// The surface a tool belongs to: its MCP server, or the tool itself.
fn tool_surface(tool: &str) -> String {
    match mcp_server_of(tool) {
        Some(server) => format!("mcp_server:{server}"),
        None => format!("tool:{tool}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: i64 = 60 * 1000;

    fn request(at_ms: i64, read: u64, write_1h: u64) -> CacheRequest {
        CacheRequest {
            sent_epoch_ms: at_ms,
            model: "model-a".into(),
            uncached: 3,
            cache_read: read,
            cache_write_5m: 0,
            cache_write_1h: write_1h,
            requested_ttl: Some(CacheTtl::OneHour),
            shape: Some("s1".into()),
        }
    }

    fn trace(steps: Vec<CacheStep>) -> CacheTrace {
        CacheTrace {
            steps,
            shapes: HashMap::new(),
        }
    }

    fn breaks(trace: &CacheTrace) -> Vec<Option<CacheBreak>> {
        observe_cache(trace, DEFAULT_MIN_REUSE)
            .into_iter()
            .map(|obs| obs.cache_break)
            .collect()
    }

    #[test]
    fn a_request_reusing_the_previous_prefix_is_not_a_break() {
        let t = trace(vec![
            CacheStep::Request(request(0, 0, 10_000)),
            CacheStep::Request(request(MIN, 10_000, 500)),
        ]);
        assert_eq!(breaks(&t), vec![None, None]);
    }

    #[test]
    fn the_first_request_is_never_a_break() {
        let t = trace(vec![CacheStep::Request(request(0, 0, 10_000))]);
        let obs = observe_cache(&t, DEFAULT_MIN_REUSE);
        assert_eq!(obs[0].cache_break, None);
        assert_eq!(obs[0].gap_ms, None);
    }

    #[test]
    fn a_small_trim_below_the_threshold_is_not_a_break() {
        let t = trace(vec![
            CacheStep::Request(request(0, 0, 10_000)),
            CacheStep::Request(request(MIN, 9_000, 200)),
        ]);
        assert_eq!(breaks(&t), vec![None, None]);
    }

    #[test]
    fn a_break_rebuilds_what_the_previous_request_had_cached() {
        let t = trace(vec![
            CacheStep::Request(request(0, 2_000, 8_000)),
            CacheStep::Request(request(MIN, 1_000, 9_500)),
        ]);
        let b = breaks(&t)[1].clone().expect("a break");
        assert_eq!(b.rebuilt_tokens, 9_000);
        assert_eq!(b.cause, BreakCause::Unknown);
    }

    #[test]
    fn a_gap_past_the_previous_requests_ttl_is_idle_expired() {
        let mut first = request(0, 0, 10_000);
        first.requested_ttl = Some(CacheTtl::FiveMinutes);
        let t = trace(vec![
            CacheStep::Request(first),
            CacheStep::Request(request(6 * MIN, 0, 10_000)),
        ]);
        let obs = observe_cache(&t, DEFAULT_MIN_REUSE);
        assert_eq!(obs[1].ttl, CacheTtl::FiveMinutes);
        assert_eq!(obs[1].gap_ms, Some(6 * MIN));
        assert_eq!(
            obs[1].cache_break.as_ref().map(|b| b.cause),
            Some(BreakCause::IdleExpired)
        );
    }

    #[test]
    fn without_a_requested_ttl_a_1h_write_implies_the_1h_lifetime() {
        let mut first = request(0, 0, 10_000);
        first.requested_ttl = None;
        let t = trace(vec![
            CacheStep::Request(first),
            CacheStep::Request(request(10 * MIN, 0, 10_000)),
        ]);
        let obs = observe_cache(&t, DEFAULT_MIN_REUSE);
        assert_eq!(obs[1].ttl, CacheTtl::OneHour);
        assert_eq!(
            obs[1].cache_break.as_ref().map(|b| b.cause),
            Some(BreakCause::Unknown)
        );
    }

    #[test]
    fn idle_takes_precedence_over_a_coinciding_compaction() {
        let t = trace(vec![
            CacheStep::Request(request(0, 0, 10_000)),
            CacheStep::Compaction,
            CacheStep::Request(request(61 * MIN, 0, 2_000)),
        ]);
        assert_eq!(
            breaks(&t)[1].as_ref().map(|b| b.cause),
            Some(BreakCause::IdleExpired)
        );
    }

    #[test]
    fn a_compaction_between_requests_explains_a_break() {
        let t = trace(vec![
            CacheStep::Request(request(0, 0, 10_000)),
            CacheStep::Compaction,
            CacheStep::Request(request(MIN, 0, 2_000)),
        ]);
        assert_eq!(
            breaks(&t)[1].as_ref().map(|b| b.cause),
            Some(BreakCause::Compaction)
        );
    }

    #[test]
    fn a_model_switch_explains_a_break() {
        let mut second = request(MIN, 0, 10_000);
        second.model = "model-b".into();
        let t = trace(vec![
            CacheStep::Request(request(0, 0, 10_000)),
            CacheStep::Request(second),
        ]);
        assert_eq!(
            breaks(&t)[1].as_ref().map(|b| b.cause),
            Some(BreakCause::ModelSwitch)
        );
    }

    #[test]
    fn a_break_after_the_prompt_shrank_rebuilds_only_what_it_wrote() {
        let t = trace(vec![
            CacheStep::Request(request(0, 0, 100_000)),
            CacheStep::Compaction,
            CacheStep::Request(request(MIN, 0, 20_000)),
        ]);
        assert_eq!(
            breaks(&t)[1].as_ref().map(|b| b.rebuilt_tokens),
            Some(20_000)
        );
    }

    #[test]
    fn a_hit_that_wrote_nothing_keeps_the_lifetime_seen_before_it() {
        let unrequested = |at_ms, read, write_1h| CacheRequest {
            requested_ttl: None,
            ..request(at_ms, read, write_1h)
        };
        let t = trace(vec![
            CacheStep::Request(unrequested(0, 0, 10_000)),
            CacheStep::Request(unrequested(MIN, 10_000, 0)),
            CacheStep::Request(unrequested(11 * MIN, 0, 10_000)),
        ]);
        let obs = observe_cache(&t, DEFAULT_MIN_REUSE);
        assert_eq!(obs[2].ttl, CacheTtl::OneHour);
        assert_eq!(
            obs[2].cache_break.as_ref().map(|b| b.cause),
            Some(BreakCause::Unknown)
        );
    }

    #[test]
    fn switching_the_cache_lifetime_explains_a_break() {
        let mut first = request(0, 0, 0);
        first.requested_ttl = Some(CacheTtl::FiveMinutes);
        first.cache_write_5m = 10_000;
        let t = trace(vec![
            CacheStep::Request(first),
            CacheStep::Request(request(MIN, 0, 10_000)),
        ]);
        assert_eq!(
            breaks(&t)[1].as_ref().map(|b| b.cause),
            Some(BreakCause::TtlChanged)
        );
    }

    fn shape(tools: &[&str], system: &[u64]) -> RequestShape {
        RequestShape {
            tools: tools.iter().map(|t| (t.to_string(), 0)).collect(),
            system: system.to_vec(),
        }
    }

    #[test]
    fn a_tool_whose_definition_changed_under_the_same_name_is_named() {
        let mut after = shape(&["Read", "mcp__srv__x"], &[1]);
        after.tools.insert("mcp__srv__x".into(), 7);
        let b = shape_change(shape(&["Read", "mcp__srv__x"], &[1]), after);
        assert_eq!(b.cause, BreakCause::ToolsChanged);
        assert_eq!(b.surfaces, vec!["mcp_server:srv"]);
    }

    fn shape_change(before: RequestShape, after: RequestShape) -> CacheBreak {
        let mut second = request(MIN, 0, 10_000);
        second.shape = Some("s2".into());
        let t = CacheTrace {
            steps: vec![
                CacheStep::Request(request(0, 0, 10_000)),
                CacheStep::Request(second),
            ],
            shapes: HashMap::from([("s1".to_string(), before), ("s2".to_string(), after)]),
        };
        breaks(&t)[1].clone().expect("a break")
    }

    #[test]
    fn a_changed_tool_list_names_the_mcp_servers_and_tools_it_added_or_removed() {
        let b = shape_change(
            shape(&["Read", "mcp__old__x"], &[1]),
            shape(
                &["Read", "Monitor", "mcp__late_srv__a", "mcp__late_srv__b"],
                &[1],
            ),
        );
        assert_eq!(b.cause, BreakCause::ToolsChanged);
        assert_eq!(
            b.surfaces,
            vec!["mcp_server:late_srv", "mcp_server:old", "tool:Monitor"]
        );
    }

    #[test]
    fn a_changed_system_block_alone_is_system_changed() {
        let b = shape_change(shape(&["Read"], &[1, 2]), shape(&["Read"], &[1, 3]));
        assert_eq!(b.cause, BreakCause::SystemChanged);
        assert!(b.surfaces.is_empty());
    }

    #[test]
    fn a_shape_id_change_without_recorded_shapes_is_shape_changed() {
        let mut second = request(MIN, 0, 10_000);
        second.shape = Some("s2".into());
        let t = trace(vec![
            CacheStep::Request(request(0, 0, 10_000)),
            CacheStep::Request(second),
        ]);
        assert_eq!(
            breaks(&t)[1].as_ref().map(|b| b.cause),
            Some(BreakCause::ShapeChanged)
        );
    }

    #[test]
    fn a_shape_change_that_kept_the_cache_is_not_a_break() {
        let mut second = request(MIN, 10_000, 100);
        second.shape = Some("s2".into());
        let t = trace(vec![
            CacheStep::Request(request(0, 0, 10_000)),
            CacheStep::Request(second),
        ]);
        assert_eq!(breaks(&t), vec![None, None]);
    }

    fn row(session: &str, read: i64, write_5m: i64, write_1h: i64) -> CacheRow {
        CacheRow {
            session_id: session.into(),
            uncached: 10,
            cache_read: read,
            cache_write_5m: write_5m,
            cache_write_1h: write_1h,
            ..CacheRow::default()
        }
    }

    fn broken(session: &str, cause: BreakCause, rebuilt: i64, surfaces: &[&str]) -> CacheRow {
        CacheRow {
            break_cause: Some(cause),
            rebuilt_tokens: rebuilt,
            surfaces: surfaces.iter().map(|s| s.to_string()).collect(),
            ..row(session, 0, 0, rebuilt)
        }
    }

    #[test]
    fn hit_rate_is_a_share_of_prompt_tokens_not_of_requests() {
        let t = totals(&[row("a", 990, 0, 0), row("a", 0, 0, 990)]);
        assert_eq!(t.hit_rate(), Some(990.0 / 2000.0));
    }

    #[test]
    fn hit_rate_is_unknown_without_prompt_tokens() {
        assert_eq!(totals(&[]).hit_rate(), None);
    }

    #[test]
    fn the_1h_share_is_of_cache_writes() {
        let t = totals(&[row("a", 0, 300, 100)]);
        assert_eq!(t.write_1h_share(), Some(0.25));
    }

    #[test]
    fn idle_rebuilds_are_counted_apart_from_other_rebuilds() {
        let t = totals(&[
            broken("a", BreakCause::IdleExpired, 500, &[]),
            broken("a", BreakCause::Compaction, 200, &[]),
        ]);
        assert_eq!(
            (t.breaks, t.rebuilt_tokens, t.idle_rebuilt_tokens),
            (2, 700, 500)
        );
    }

    #[test]
    fn causes_rank_by_rebuilt_tokens() {
        let ranked = breaks_by_cause(&[
            broken("a", BreakCause::IdleExpired, 100, &[]),
            broken("a", BreakCause::ToolsChanged, 300, &[]),
            row("a", 1, 0, 0),
        ]);
        let causes: Vec<_> = ranked.iter().map(|c| (c.cause, c.breaks)).collect();
        assert_eq!(causes, vec![("tools_changed", 1), ("idle_expired", 1)]);
    }

    #[test]
    fn a_surface_breaking_many_sessions_outranks_one_costly_session() {
        let ranked = breaks_by_surface(&[
            broken("a", BreakCause::ToolsChanged, 1_000, &["mcp_server:big"]),
            broken("a", BreakCause::ToolsChanged, 1_000, &["mcp_server:big"]),
            broken("b", BreakCause::ToolsChanged, 10, &["mcp_server:spread"]),
            broken("c", BreakCause::ToolsChanged, 10, &["mcp_server:spread"]),
        ]);
        let order: Vec<_> = ranked
            .iter()
            .map(|s| (s.surface.as_str(), s.sessions))
            .collect();
        assert_eq!(order, vec![("mcp_server:spread", 2), ("mcp_server:big", 1)]);
    }

    #[test]
    fn a_break_naming_two_surfaces_credits_both() {
        let ranked = breaks_by_surface(&[broken(
            "a",
            BreakCause::ToolsChanged,
            40,
            &["mcp_server:x", "tool:Y"],
        )]);
        assert!(
            ranked
                .iter()
                .all(|s| s.rebuilt_tokens == 40 && s.breaks == 1)
        );
        assert_eq!(ranked.len(), 2);
    }

    #[test]
    fn a_compaction_before_the_first_request_does_not_leak_into_later_ones() {
        let t = trace(vec![
            CacheStep::Compaction,
            CacheStep::Request(request(0, 0, 10_000)),
            CacheStep::Request(request(MIN, 0, 10_000)),
        ]);
        assert_eq!(
            breaks(&t)[1].as_ref().map(|b| b.cause),
            Some(BreakCause::Unknown)
        );
    }
}
