//! Point usage events for non-skill surfaces: an agent spawn exercises an
//! `agent` surface, an MCP tool call exercises its `mcp_server` surface. These
//! carry no span/cost of their own — they are usage *counts* that join the
//! catalog so agents and MCP servers are not stuck at "usage n/a". See
//! `docs/specs/events.md`, `surfaces.md`.

use crate::core::span::{Record, RecordKind};

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

/// Extract agent-spawn and MCP-tool usage events from the record stream.
pub fn extract_usage_events(records: &[Record]) -> Vec<UsageEvent> {
    records
        .iter()
        .filter_map(|record| match &record.kind {
            RecordKind::AgentSpawn { agent } => Some(UsageEvent {
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
