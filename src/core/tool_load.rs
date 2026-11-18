//! What a session pays for its tools before any work starts, and whether each
//! tool's loading mode fits how it is used — the request-shape half of the
//! always-on decomposition (`docs/specs/surfaces.md`). Pure: the adapter weighs
//! the transcript into these types; the store persists and aggregates them.

/// One request shape reduced to weights: never its text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WeighedShape {
    pub hash: String,
    pub system_tokens: u64,
    pub system_blocks: u64,
    pub tools: Vec<ShapeTool>,
}

/// One tool definition inside a shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShapeTool {
    pub name: String,
    /// The MCP server id for an `mcp__<server>__…` tool; `None` for a built-in.
    pub server: Option<String>,
    /// Sent as a stub the model cannot see until it is loaded.
    pub deferred: bool,
    pub tokens: u64,
}

/// A main-loop shape a session sent: when it first appeared and how often.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShapeUse {
    pub hash: String,
    pub first_epoch_ms: i64,
    pub requests: u64,
}

/// One tool as one session had it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionTool {
    pub name: String,
    pub server: Option<String>,
    /// Definition weight when eager in the session's startup shape.
    pub eager_tokens: Option<u64>,
    /// Weight of its deferred-listing line when it was announced as deferred.
    pub listing_tokens: Option<u64>,
    /// A tool-search load named it at least once.
    pub loaded: bool,
    pub calls: u64,
}

/// Everything one session's transcript says about its tool context.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolContext {
    /// Every main-loop shape the session referenced.
    pub shapes: Vec<WeighedShape>,
    pub uses: Vec<ShapeUse>,
    pub tools: Vec<SessionTool>,
    /// `(MCP server id, tokens)` of each MCP server's instructions.
    pub mcp_instructions: Vec<(String, u64)>,
}

/// One tool across every session that had it — the input to the eager↔deferred
/// wedges. Counts are sessions, not calls, except `calls`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ToolLoadStat {
    pub name: String,
    pub server: Option<String>,
    /// Sessions whose startup shape carried it eagerly.
    pub eager_sessions: u64,
    /// Of those, sessions that called it.
    pub eager_sessions_called: u64,
    /// Sessions that announced it as deferred.
    pub deferred_sessions: u64,
    /// Of those, sessions that loaded it through tool search.
    pub deferred_sessions_loaded: u64,
    pub calls: u64,
    /// Its definition weight when eager (the latest observed).
    pub eager_tokens: u64,
    /// Its mode in the latest session that had it eager or deferred: a wedge
    /// is only about the mode the tool is in now.
    pub currently_deferred: bool,
}

/// Which way a tool's loading mode should move.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolLoadWedgeKind {
    /// Eager but rarely called: its schema is paid every session for little use.
    DeferCandidate,
    /// Deferred but loaded in most sessions: a tool-search turn paid almost every time.
    EagerCandidate,
}

impl ToolLoadWedgeKind {
    pub fn label(self) -> &'static str {
        match self {
            ToolLoadWedgeKind::DeferCandidate => "DEFER CANDIDATE",
            ToolLoadWedgeKind::EagerCandidate => "EAGER CANDIDATE",
        }
    }
}

/// Tuning constants for the eager↔deferred wedges, injected so tests and
/// callers state them explicitly.
#[derive(Debug, Clone, Copy)]
pub struct ToolLoadThresholds {
    /// Fewer sessions than this is not evidence either way.
    pub min_sessions: u64,
    /// An eager tool called in at most this share of its sessions is a defer candidate.
    pub defer_max_use_rate: f64,
    /// A deferred tool loaded in at least this share of its sessions is an eager candidate.
    pub eager_min_load_rate: f64,
}

pub const TOOL_LOAD_THRESHOLDS: ToolLoadThresholds = ToolLoadThresholds {
    min_sessions: 5,
    defer_max_use_rate: 0.1,
    eager_min_load_rate: 0.5,
};

#[derive(Debug, Clone, PartialEq)]
pub struct ToolLoadWedge {
    pub kind: ToolLoadWedgeKind,
    pub stat: ToolLoadStat,
}

/// Classify every tool whose loading mode fights its use
/// (`docs/specs/surfaces.md` "Eager vs deferred tools"). Defer candidates come
/// first, the most tokens paid for nothing first; eager candidates follow, the
/// most consistently loaded first.
pub fn tool_load_wedges(stats: &[ToolLoadStat], limits: ToolLoadThresholds) -> Vec<ToolLoadWedge> {
    let rate = |part: u64, whole: u64| part as f64 / whole as f64;
    let mut defer: Vec<&ToolLoadStat> = stats
        .iter()
        .filter(|stat| {
            stat.server.is_some()
                && !stat.currently_deferred
                && stat.eager_sessions >= limits.min_sessions
                && rate(stat.eager_sessions_called, stat.eager_sessions)
                    <= limits.defer_max_use_rate
        })
        .collect();
    defer.sort_by_key(|stat| std::cmp::Reverse(stat.eager_tokens * stat.eager_sessions));
    let mut eager: Vec<&ToolLoadStat> = stats
        .iter()
        .filter(|stat| {
            stat.server.is_some()
                && stat.currently_deferred
                && stat.deferred_sessions >= limits.min_sessions
                && rate(stat.deferred_sessions_loaded, stat.deferred_sessions)
                    >= limits.eager_min_load_rate
        })
        .collect();
    eager.sort_by(|a, b| {
        rate(b.deferred_sessions_loaded, b.deferred_sessions)
            .total_cmp(&rate(a.deferred_sessions_loaded, a.deferred_sessions))
            .then(b.deferred_sessions.cmp(&a.deferred_sessions))
    });
    defer
        .into_iter()
        .map(|stat| (ToolLoadWedgeKind::DeferCandidate, stat))
        .chain(
            eager
                .into_iter()
                .map(|stat| (ToolLoadWedgeKind::EagerCandidate, stat)),
        )
        .map(|(kind, stat)| ToolLoadWedge {
            kind,
            stat: stat.clone(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stat(name: &str) -> ToolLoadStat {
        ToolLoadStat {
            name: name.into(),
            server: name
                .strip_prefix("mcp__")
                .map(|rest| rest.split("__").next().unwrap().into()),
            eager_sessions: 0,
            eager_sessions_called: 0,
            deferred_sessions: 0,
            deferred_sessions_loaded: 0,
            calls: 0,
            eager_tokens: 100,
            currently_deferred: false,
        }
    }

    fn eager(name: &str, sessions: u64, called: u64) -> ToolLoadStat {
        ToolLoadStat {
            eager_sessions: sessions,
            eager_sessions_called: called,
            ..stat(name)
        }
    }

    fn deferred(name: &str, sessions: u64, loaded: u64) -> ToolLoadStat {
        ToolLoadStat {
            deferred_sessions: sessions,
            deferred_sessions_loaded: loaded,
            currently_deferred: true,
            ..stat(name)
        }
    }

    fn kinds(wedges: &[ToolLoadWedge]) -> Vec<(ToolLoadWedgeKind, &str)> {
        wedges
            .iter()
            .map(|w| (w.kind, w.stat.name.as_str()))
            .collect()
    }

    #[test]
    fn an_eager_mcp_tool_rarely_called_is_a_defer_candidate() {
        let wedges = tool_load_wedges(&[eager("mcp__srv__q", 20, 1)], TOOL_LOAD_THRESHOLDS);
        assert_eq!(
            kinds(&wedges),
            vec![(ToolLoadWedgeKind::DeferCandidate, "mcp__srv__q")]
        );
    }

    #[test]
    fn an_eager_mcp_tool_called_often_is_left_alone() {
        assert!(tool_load_wedges(&[eager("mcp__srv__q", 20, 15)], TOOL_LOAD_THRESHOLDS).is_empty());
    }

    #[test]
    fn a_built_in_tool_is_never_a_defer_candidate() {
        assert!(tool_load_wedges(&[eager("NotebookEdit", 20, 0)], TOOL_LOAD_THRESHOLDS).is_empty());
    }

    #[test]
    fn a_deferred_tool_loaded_in_most_sessions_is_an_eager_candidate() {
        let wedges = tool_load_wedges(&[deferred("mcp__srv__send", 10, 8)], TOOL_LOAD_THRESHOLDS);
        assert_eq!(
            kinds(&wedges),
            vec![(ToolLoadWedgeKind::EagerCandidate, "mcp__srv__send")]
        );
    }

    #[test]
    fn a_deferred_tool_loaded_now_and_then_stays_deferred() {
        assert!(tool_load_wedges(&[deferred("WebSearch", 10, 2)], TOOL_LOAD_THRESHOLDS).is_empty());
    }

    #[test]
    fn a_built_in_deferred_tool_is_never_an_eager_candidate() {
        assert!(
            tool_load_wedges(&[deferred("WebSearch", 10, 10)], TOOL_LOAD_THRESHOLDS).is_empty()
        );
    }

    #[test]
    fn a_tool_whose_mode_changed_is_judged_only_in_its_current_mode() {
        let history = |currently_deferred| ToolLoadStat {
            eager_sessions: 10,
            eager_sessions_called: 0,
            deferred_sessions: 10,
            deferred_sessions_loaded: 10,
            currently_deferred,
            ..stat("mcp__srv__q")
        };
        assert_eq!(
            kinds(&tool_load_wedges(&[history(true)], TOOL_LOAD_THRESHOLDS)),
            vec![(ToolLoadWedgeKind::EagerCandidate, "mcp__srv__q")]
        );
        assert_eq!(
            kinds(&tool_load_wedges(&[history(false)], TOOL_LOAD_THRESHOLDS)),
            vec![(ToolLoadWedgeKind::DeferCandidate, "mcp__srv__q")]
        );
    }

    #[test]
    fn too_few_sessions_are_no_evidence_either_way() {
        let stats = [eager("mcp__srv__q", 4, 0), deferred("mcp__srv__send", 4, 4)];
        assert!(tool_load_wedges(&stats, TOOL_LOAD_THRESHOLDS).is_empty());
    }

    #[test]
    fn defer_candidates_rank_by_tokens_paid_and_precede_eager_candidates() {
        let heavy = ToolLoadStat {
            eager_tokens: 900,
            ..eager("mcp__srv__heavy", 10, 0)
        };
        let stats = [
            deferred("mcp__srv__send", 10, 9),
            eager("mcp__srv__light", 10, 0),
            heavy,
        ];
        assert_eq!(
            kinds(&tool_load_wedges(&stats, TOOL_LOAD_THRESHOLDS)),
            vec![
                (ToolLoadWedgeKind::DeferCandidate, "mcp__srv__heavy"),
                (ToolLoadWedgeKind::DeferCandidate, "mcp__srv__light"),
                (ToolLoadWedgeKind::EagerCandidate, "mcp__srv__send"),
            ]
        );
    }
}
