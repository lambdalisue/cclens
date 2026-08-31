//! Point usage events for non-skill surfaces: an agent spawn exercises an
//! `agent` surface, an MCP tool call exercises its `mcp_server` surface. These
//! carry no span/cost of their own — they are usage *counts* that join the
//! catalog so agents and MCP servers are not stuck at "usage n/a". See
//! `docs/specs/events.md`, `surfaces.md`.

use crate::core::span::{Record, RecordKind, Span};
use crate::core::subagent::SubagentRun;

/// One surface-usage occurrence, keyed for the catalog×usage join.
#[derive(Debug, Clone, PartialEq)]
pub struct UsageEvent {
    pub surface_kind: String,
    pub surface_id: String,
    pub started_epoch_ms: i64,
    /// For an agent spawn, the `Agent` call's id — persisted so a stored spawn
    /// joins the subagent run it started. `None` for every other surface.
    pub tool_use_id: Option<String>,
}

/// Total assistant output tokens across a record stream — used to sum a
/// subagent transcript's cost for attribution (`docs/specs/events.md`).
pub fn output_tokens(records: &[Record]) -> u64 {
    records
        .iter()
        .map(|record| match &record.kind {
            RecordKind::Assistant { out_tokens, .. } => *out_tokens,
            _ => 0,
        })
        .sum()
}

/// Attribute subagent costs to the spans that spawned them.
///
/// The join is tried at two precisions, and only falls back when it must
/// (`docs/specs/events.md`):
///
/// 1. **By spawn call.** A run whose tree was rooted at a known `Agent` call
///    lands on the one span containing that call — spans do not overlap, so
///    this is exact and never flagged estimated. Nested runs resolve through
///    their root (`core::subagent::link_runs`), so a subtree's whole cost is
///    charged to the main-thread work that started it.
/// 2. **By turn.** Only for a run that carries no call id at all (an older
///    transcript with no sidecar). The run is then claimed by every span that
///    spawned anything in the same turn; when more than one competes they split
///    its tokens equally and are flagged estimated.
///
/// Once a run carries a call id, the turn join is never used for it — not when
/// its rooted call is in no span (the root says which call spawned it, so no
/// span containing that call means it was spawned outside every span), and not
/// when its parent chain is broken (`link_runs` left it unrooted). Falling back
/// in either case would charge it to whichever unrelated span happened to spawn
/// something in the same turn, which is the error the call id exists to prevent.
///
/// A run matching no span is left to the session-level total, which is exact.
pub fn attribute_subagents(spans: &mut [Span], runs: &[SubagentRun]) {
    for run in runs {
        let (claimants, estimated): (Vec<usize>, bool) = match run {
            _ if run.root_tool_use_id.is_some() => {
                (claim_by_spawn(spans, run).into_iter().collect(), false)
            }
            // Unrooted despite having a call id: its chain is broken, not absent.
            _ if run.tool_use_id.is_some() => (Vec::new(), false),
            _ => {
                let claimants = claim_by_turn(spans, run);
                let estimated = claimants.len() > 1;
                (claimants, estimated)
            }
        };
        if claimants.is_empty() {
            continue;
        }
        // Integer division drops the remainder on purpose: a split figure is
        // already flagged estimated, and the authoritative total sums the runs
        // themselves, so nothing that must balance depends on this.
        let share = run.out_tokens / claimants.len() as u64;
        for index in claimants {
            spans[index].sub_tokens += share;
            spans[index].sub_agent_count += 1;
            spans[index].sub_tokens_estimated |= estimated;
        }
    }
}

/// The single span containing the `Agent` call this run's tree was rooted at.
fn claim_by_spawn(spans: &[Span], run: &SubagentRun) -> Option<usize> {
    let root = run.root_tool_use_id.as_deref()?;
    spans.iter().position(|span| {
        span.spawns
            .iter()
            .any(|spawn| spawn.tool_use_id.as_deref() == Some(root))
    })
}

/// Every span that spawned a subagent in the same turn as this run.
fn claim_by_turn(spans: &[Span], run: &SubagentRun) -> Vec<usize> {
    let Some(prompt_id) = run.prompt_id.as_deref() else {
        return Vec::new();
    };
    spans
        .iter()
        .enumerate()
        .filter(|(_, span)| {
            span.spawns
                .iter()
                .any(|spawn| spawn.prompt_id.as_deref() == Some(prompt_id))
        })
        .map(|(index, _)| index)
        .collect()
}

/// Extract agent-spawn and MCP-tool usage events from the record stream.
pub fn extract_usage_events(records: &[Record]) -> Vec<UsageEvent> {
    records
        .iter()
        .filter_map(|record| match &record.kind {
            RecordKind::AgentSpawn {
                agent, tool_use_id, ..
            } => Some(UsageEvent {
                surface_kind: "agent".to_string(),
                surface_id: agent.clone(),
                started_epoch_ms: record.timestamp_ms,
                tool_use_id: tool_use_id.clone(),
            }),
            RecordKind::ToolUse { tool } => mcp_server_of(tool).map(|server| UsageEvent {
                surface_kind: "mcp_server".to_string(),
                surface_id: server,
                started_epoch_ms: record.timestamp_ms,
                tool_use_id: None,
            }),
            _ => None,
        })
        .collect()
}

/// The MCP server name from a tool named `mcp__<server>__<tool>`. The server may
/// contain single underscores; only the `__` delimiters are structural. Returns
/// `None` for a non-MCP tool.
fn mcp_server_of(tool: &str) -> Option<String> {
    let rest = tool.strip_prefix("mcp__")?;
    let server = rest.split("__").next()?;
    (!server.is_empty()).then(|| server.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::span::SpawnRef;

    fn at(timestamp_ms: i64, kind: RecordKind) -> Record {
        Record { timestamp_ms, kind }
    }

    #[test]
    fn an_agent_spawn_is_agent_usage() {
        let records = [at(
            100,
            RecordKind::AgentSpawn {
                agent: "Explore".into(),
                tool_use_id: Some("toolu_1".into()),
                prompt_id: None,
            },
        )];
        let events = extract_usage_events(&records);
        assert_eq!(
            events,
            vec![UsageEvent {
                surface_kind: "agent".into(),
                surface_id: "Explore".into(),
                started_epoch_ms: 100,
                tool_use_id: Some("toolu_1".into()),
            }]
        );
    }

    #[test]
    fn an_mcp_tool_use_is_keyed_to_its_server() {
        let records = [
            at(
                1,
                RecordKind::ToolUse {
                    tool: "mcp__playwright__browser_click".into(),
                },
            ),
            at(
                2,
                RecordKind::ToolUse {
                    tool: "mcp__grafana_arrove-production__list_incidents".into(),
                },
            ),
        ];
        let events = extract_usage_events(&records);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].surface_kind, "mcp_server");
        assert_eq!(events[0].surface_id, "playwright");
        // Single underscores in the server name survive; only `__` is structural.
        assert_eq!(events[1].surface_id, "grafana_arrove-production");
    }

    #[test]
    fn a_non_mcp_tool_use_is_ignored() {
        let records = [at(
            1,
            RecordKind::ToolUse {
                tool: "Bash".into(),
            },
        )];
        assert!(extract_usage_events(&records).is_empty());
    }

    /// A span whose window contains one spawn per `(tool_use_id, prompt_id)`.
    fn span_with(skill: &str, spawns: &[(Option<&str>, &str)]) -> Span {
        Span {
            skill: skill.to_string(),
            source: crate::core::span::Source::Tool,
            started_epoch_ms: 0,
            duration_sec: 0.0,
            out_tokens: 0,
            ctx_growth: 0,
            ctx_start: 0,
            ctx_peak: 0,
            model: None,
            is_trailing: false,
            spawns: spawns
                .iter()
                .map(|(tool_use_id, prompt_id)| SpawnRef {
                    tool_use_id: tool_use_id.map(String::from),
                    prompt_id: Some(prompt_id.to_string()),
                })
                .collect(),
            sub_tokens: 0,
            sub_agent_count: 0,
            sub_tokens_estimated: false,
        }
    }

    /// A run rooted at `root` (the spawn call that started its tree), spawned in
    /// turn `prompt_id`.
    fn run(root: Option<&str>, prompt_id: &str, out_tokens: u64) -> SubagentRun {
        SubagentRun {
            agent: Some("Explore".into()),
            agent_id: "a1".into(),
            run_path: "/tmp/example/subagents/agent-a1.jsonl".into(),
            tool_use_id: root.map(String::from),
            parent_agent_id: None,
            spawn_depth: 1,
            model: None,
            prompt_id: Some(prompt_id.to_string()),
            out_tokens,
            started_epoch_ms: 0,
            root_tool_use_id: root.map(String::from),
        }
    }

    #[test]
    fn a_subagent_is_attributed_to_the_span_that_spawned_it() {
        let mut spans = vec![span_with("code-review", &[(Some("toolu_1"), "p1")])];
        attribute_subagents(&mut spans, &[run(Some("toolu_1"), "p1", 500)]);
        assert_eq!(spans[0].sub_tokens, 500);
        assert_eq!(spans[0].sub_agent_count, 1);
        assert!(!spans[0].sub_tokens_estimated);
    }

    #[test]
    fn the_spawn_call_settles_which_of_two_spans_in_a_turn_pays() {
        // Both spans spawned in turn p1, so the turn alone cannot tell them
        // apart — the call id can, exactly and without an estimate.
        let mut spans = vec![
            span_with("a", &[(Some("toolu_1"), "p1")]),
            span_with("b", &[(Some("toolu_2"), "p1")]),
        ];
        attribute_subagents(&mut spans, &[run(Some("toolu_2"), "p1", 100)]);
        assert_eq!(spans[0].sub_tokens, 0);
        assert_eq!(spans[1].sub_tokens, 100);
        assert!(!spans[1].sub_tokens_estimated);
    }

    #[test]
    fn a_nested_run_is_charged_to_the_span_that_started_its_tree() {
        let mut spans = vec![span_with("code-review", &[(Some("toolu_1"), "p1")])];
        let mut nested = run(Some("toolu_1"), "p1", 300);
        // The nested run's own call lives in its parent's transcript, which no
        // span contains; only its root links it back to the main thread.
        nested.tool_use_id = Some("toolu_nested".into());
        nested.spawn_depth = 2;
        attribute_subagents(&mut spans, &[nested]);
        assert_eq!(spans[0].sub_tokens, 300);
    }

    #[test]
    fn a_run_whose_parent_chain_is_broken_stays_unattributed() {
        // It has a call id, so it was not spawned from the main thread — its
        // parent's transcript is simply missing. The turn join would hand it to
        // whichever span spawned something in that turn, which is a guess the
        // call id already rules out.
        let mut spans = vec![span_with("git-commit", &[(Some("toolu_1"), "p1")])];
        let mut orphan = run(None, "p1", 100);
        orphan.tool_use_id = Some("toolu_nested".into());
        orphan.parent_agent_id = Some("gone".into());
        orphan.spawn_depth = 2;
        attribute_subagents(&mut spans, &[orphan]);
        assert_eq!(spans[0].sub_tokens, 0);
        assert_eq!(spans[0].sub_agent_count, 0);
    }

    #[test]
    fn competing_spans_split_equally_and_are_flagged_estimated() {
        // No sidecar named the spawning call, so only the turn is left to join on.
        let mut spans = vec![
            span_with("a", &[(None, "p1")]),
            span_with("b", &[(None, "p1")]),
        ];
        attribute_subagents(&mut spans, &[run(None, "p1", 100)]);
        assert_eq!(spans[0].sub_tokens, 50);
        assert_eq!(spans[1].sub_tokens, 50);
        assert!(spans[0].sub_tokens_estimated);
        assert!(spans[1].sub_tokens_estimated);
    }

    #[test]
    fn a_subagent_with_no_matching_span_is_left_unattributed() {
        let mut spans = vec![span_with("a", &[(Some("toolu_1"), "p1")])];
        attribute_subagents(&mut spans, &[run(Some("toolu_9"), "other", 100)]);
        assert_eq!(spans[0].sub_tokens, 0);
    }

    #[test]
    fn a_rooted_run_spawned_outside_every_span_does_not_fall_back_to_the_turn() {
        // The span spawned its own agent in turn p1; this run was rooted at a
        // different call, made outside any span in that same turn. Knowing the
        // call is knowing it was not this span's — the turn must not re-claim it.
        let mut spans = vec![span_with("git-commit", &[(Some("toolu_1"), "p1")])];
        attribute_subagents(&mut spans, &[run(Some("toolu_outside"), "p1", 100)]);
        assert_eq!(spans[0].sub_tokens, 0);
        assert_eq!(spans[0].sub_agent_count, 0);
        assert!(!spans[0].sub_tokens_estimated);
    }

    #[test]
    fn output_tokens_sums_assistant_records_only() {
        let records = [
            at(
                1,
                RecordKind::Assistant {
                    prompt_size: 100,
                    out_tokens: 30,
                    model: "m".into(),
                },
            ),
            at(2, RecordKind::HumanTurn),
            at(
                3,
                RecordKind::Assistant {
                    prompt_size: 200,
                    out_tokens: 70,
                    model: "m".into(),
                },
            ),
        ];
        assert_eq!(output_tokens(&records), 100);
    }
}
