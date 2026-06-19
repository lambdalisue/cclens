//! Point usage events for non-skill surfaces: an agent spawn exercises an
//! `agent` surface, an MCP tool call exercises its `mcp_server` surface. These
//! carry no span/cost of their own — they are usage *counts* that join the
//! catalog so agents and MCP servers are not stuck at "usage n/a". See
//! `docs/specs/events.md`, `surfaces.md`.

use crate::core::span::{Record, RecordKind, Span};

/// One surface-usage occurrence, keyed for the catalog×usage join.
#[derive(Debug, Clone, PartialEq)]
pub struct UsageEvent {
    pub surface_kind: String,
    pub surface_id: String,
    pub started_epoch_ms: i64,
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

/// Attribute subagent costs to the spans that spawned them, joined by prompt id.
///
/// `subagents` is `(prompt_id, output_tokens)` per subagent transcript. A
/// subagent is attributed to every span whose `agent_prompt_ids` contains its
/// prompt id; when more than one span competes, its tokens are split equally and
/// those spans are flagged estimated (`docs/specs/events.md`). A subagent with
/// no matching span is left to the session-level total.
pub fn attribute_subagents(spans: &mut [Span], subagents: &[(String, u64)]) {
    for (prompt_id, tokens) in subagents {
        let claimants: Vec<usize> = spans
            .iter()
            .enumerate()
            .filter(|(_, span)| span.agent_prompt_ids.iter().any(|id| id == prompt_id))
            .map(|(index, _)| index)
            .collect();
        if claimants.is_empty() {
            continue;
        }
        let estimated = claimants.len() > 1;
        let share = tokens / claimants.len() as u64;
        for index in claimants {
            spans[index].sub_tokens += share;
            spans[index].sub_agent_count += 1;
            spans[index].sub_tokens_estimated |= estimated;
        }
    }
}

/// Extract agent-spawn and MCP-tool usage events from the record stream.
pub fn extract_usage_events(records: &[Record]) -> Vec<UsageEvent> {
    records
        .iter()
        .filter_map(|record| match &record.kind {
            RecordKind::AgentSpawn { agent, .. } => Some(UsageEvent {
                surface_kind: "agent".to_string(),
                surface_id: agent.clone(),
                started_epoch_ms: record.timestamp_ms,
            }),
            RecordKind::ToolUse { tool } => mcp_server_of(tool).map(|server| UsageEvent {
                surface_kind: "mcp_server".to_string(),
                surface_id: server,
                started_epoch_ms: record.timestamp_ms,
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

    fn at(timestamp_ms: i64, kind: RecordKind) -> Record {
        Record { timestamp_ms, kind }
    }

    #[test]
    fn an_agent_spawn_is_agent_usage() {
        let records = [at(
            100,
            RecordKind::AgentSpawn {
                agent: "Explore".into(),
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

    fn span_with(skill: &str, agent_prompt_ids: &[&str]) -> Span {
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
            agent_prompt_ids: agent_prompt_ids.iter().map(|s| s.to_string()).collect(),
            sub_tokens: 0,
            sub_agent_count: 0,
            sub_tokens_estimated: false,
        }
    }

    #[test]
    fn a_subagent_is_attributed_to_the_span_that_spawned_it() {
        let mut spans = vec![span_with("code-review", &["p1"])];
        attribute_subagents(&mut spans, &[("p1".to_string(), 500)]);
        assert_eq!(spans[0].sub_tokens, 500);
        assert_eq!(spans[0].sub_agent_count, 1);
        assert!(!spans[0].sub_tokens_estimated);
    }

    #[test]
    fn competing_spans_split_equally_and_are_flagged_estimated() {
        let mut spans = vec![span_with("a", &["p1"]), span_with("b", &["p1"])];
        attribute_subagents(&mut spans, &[("p1".to_string(), 100)]);
        assert_eq!(spans[0].sub_tokens, 50);
        assert_eq!(spans[1].sub_tokens, 50);
        assert!(spans[0].sub_tokens_estimated);
        assert!(spans[1].sub_tokens_estimated);
    }

    #[test]
    fn a_subagent_with_no_matching_span_is_left_unattributed() {
        let mut spans = vec![span_with("a", &["p1"])];
        attribute_subagents(&mut spans, &[("other".to_string(), 100)]);
        assert_eq!(spans[0].sub_tokens, 0);
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
