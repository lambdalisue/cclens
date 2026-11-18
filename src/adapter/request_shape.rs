//! Weigh the request shape a transcript records — the system prompt and tool
//! definitions sent with each main-loop request — plus the deferred-tool listing,
//! the MCP instructions, and the tool-search loads, into a `ToolContext`
//! (`docs/specs/session-format.md` "The request shape"). Only weights, counts,
//! tool names and hashes leave this module; the text never does.

use std::collections::{BTreeMap, HashMap};

use serde_json::Value;

use crate::adapter::config::{approx_tokens, sanitize_mcp_server_id};
use crate::core::tool_load::{SessionTool, ShapeTool, ShapeUse, ToolContext, WeighedShape};
use crate::core::usage::mcp_server_of;

/// Whether a request's `querySource` is a session's main loop. A missing source
/// is read as the main loop: the field arrived with the shape records, and a
/// main transcript's requests are main-loop unless they say otherwise.
fn is_main_loop(source: Option<&str>) -> bool {
    matches!(source, None | Some("repl_main_thread") | Some("sdk"))
}

fn weigh_shape(hash: &str, shape: &Value) -> WeighedShape {
    let blocks = shape
        .get("system")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let tools = shape
        .get("tools")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    WeighedShape {
        hash: hash.to_string(),
        system_tokens: blocks
            .iter()
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .map(approx_tokens)
            .sum(),
        system_blocks: blocks.len() as u64,
        tools: tools
            .iter()
            .filter_map(|tool| {
                let name = tool.get("name").and_then(Value::as_str)?;
                Some(ShapeTool {
                    name: name.to_string(),
                    server: mcp_server_of(name),
                    deferred: tool.get("defer_loading").and_then(Value::as_bool) == Some(true),
                    tokens: approx_tokens(&tool.to_string()),
                })
            })
            .collect(),
    }
}

/// `(name, text)` pairs of a delta attachment's parallel name/text arrays.
fn named_texts<'a>(attachment: &'a Value, names: &str, texts: &str) -> Vec<(&'a str, &'a str)> {
    let as_strs = |key: &str| -> Vec<&'a str> {
        attachment
            .get(key)
            .and_then(Value::as_array)
            .map(|items| items.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default()
    };
    as_strs(names).into_iter().zip(as_strs(texts)).collect()
}

#[derive(Default)]
struct ToolSeen {
    listing_tokens: Option<u64>,
    loaded: bool,
    calls: u64,
}

/// Extract one session's tool context from its main transcript's text.
pub fn extract_tool_context(jsonl: &str) -> ToolContext {
    let mut shapes: HashMap<String, WeighedShape> = HashMap::new();
    let mut uses: Vec<ShapeUse> = Vec::new();
    let mut tools: BTreeMap<String, ToolSeen> = BTreeMap::new();
    let mut instructions: Vec<(String, u64)> = Vec::new();

    for line in jsonl.lines() {
        // Cheap pre-filter: most lines are conversation, and parsing each into a
        // `Value` would dominate analyze time for nothing.
        if !(line.contains("api-request")
            || line.contains("\"attachment\"")
            || line.contains("\"tool_use\""))
        {
            continue;
        }
        let Ok(record) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let text = |key: &str| record.get(key).and_then(Value::as_str);
        match text("type") {
            Some("api-request-shape") => {
                if let (Some(hash), Some(shape)) = (text("shapeHash"), record.get("shape")) {
                    shapes
                        .entry(hash.to_string())
                        .or_insert_with(|| weigh_shape(hash, shape));
                }
            }
            Some("api-request") if is_main_loop(text("querySource")) => {
                let Some(hash) = text("shapeHash") else {
                    continue;
                };
                let Some(epoch_ms) = text("timestamp").and_then(parse_timestamp_ms) else {
                    continue;
                };
                match uses.iter_mut().find(|use_| use_.hash == hash) {
                    Some(use_) => use_.requests += 1,
                    None => uses.push(ShapeUse {
                        hash: hash.to_string(),
                        first_epoch_ms: epoch_ms,
                        requests: 1,
                    }),
                }
            }
            Some("attachment") => {
                let Some(attachment) = record.get("attachment") else {
                    continue;
                };
                match attachment.get("type").and_then(Value::as_str) {
                    Some("deferred_tools_delta") => {
                        for (name, line) in named_texts(attachment, "addedNames", "addedLines") {
                            let seen = tools.entry(name.to_string()).or_default();
                            seen.listing_tokens.get_or_insert(approx_tokens(line));
                        }
                    }
                    Some("mcp_instructions_delta") => {
                        for (name, block) in named_texts(attachment, "addedNames", "addedBlocks") {
                            let server = sanitize_mcp_server_id(name);
                            if !instructions.iter().any(|(seen, _)| *seen == server) {
                                instructions.push((server, approx_tokens(block)));
                            }
                        }
                    }
                    Some("deferred_tools_record") => {
                        for entry in attachment
                            .get("entries")
                            .and_then(Value::as_array)
                            .into_iter()
                            .flatten()
                        {
                            if let Some(name) = entry.get("name").and_then(Value::as_str) {
                                tools.entry(name.to_string()).or_default().loaded = true;
                            }
                        }
                    }
                    _ => {}
                }
            }
            Some("assistant") => {
                for block in record
                    .pointer("/message/content")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    if block.get("type").and_then(Value::as_str) == Some("tool_use")
                        && let Some(name) = block.get("name").and_then(Value::as_str)
                    {
                        tools.entry(name.to_string()).or_default().calls += 1;
                    }
                }
            }
            _ => {}
        }
    }

    // A use whose shape was never recorded is kept: dropping it would promote
    // a later shape (a server connected, a tool loaded) to the startup shape.
    uses.sort_by_key(|use_| use_.first_epoch_ms);
    let eager: HashMap<&str, u64> = uses
        .first()
        .and_then(|startup| shapes.get(&startup.hash))
        .map(|startup| {
            startup
                .tools
                .iter()
                .filter(|tool| !tool.deferred)
                .map(|tool| (tool.name.as_str(), tool.tokens))
                .collect()
        })
        .unwrap_or_default();
    let mut session_tools: Vec<SessionTool> = eager
        .keys()
        .filter(|name| !tools.contains_key(**name))
        .map(|name| SessionTool {
            name: name.to_string(),
            server: mcp_server_of(name),
            eager_tokens: Some(eager[name]),
            listing_tokens: None,
            loaded: false,
            calls: 0,
        })
        .chain(tools.iter().map(|(name, seen)| SessionTool {
            name: name.clone(),
            server: mcp_server_of(name),
            eager_tokens: eager.get(name.as_str()).copied(),
            listing_tokens: seen.listing_tokens,
            loaded: seen.loaded,
            calls: seen.calls,
        }))
        .collect();
    session_tools.sort_by(|a, b| a.name.cmp(&b.name));

    let shapes = uses
        .iter()
        .filter_map(|use_| shapes.get(&use_.hash).cloned())
        .collect();
    ToolContext {
        shapes,
        uses,
        tools: session_tools,
        mcp_instructions: instructions,
    }
}

fn parse_timestamp_ms(timestamp: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(timestamp)
        .ok()
        .map(|dt| dt.timestamp_millis())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::tool_load::{SessionTool, ShapeTool, ShapeUse, WeighedShape};

    fn shape(hash: &str, system: &[&str], tools: &[(&str, bool)]) -> String {
        let system: Vec<serde_json::Value> = system
            .iter()
            .map(|text| serde_json::json!({"type": "text", "text": text}))
            .collect();
        let tools: Vec<serde_json::Value> = tools
            .iter()
            .map(|(name, deferred)| {
                let mut tool = serde_json::json!({
                    "name": name,
                    "description": "dddd",
                    "input_schema": {},
                });
                if *deferred {
                    tool["defer_loading"] = serde_json::json!(true);
                }
                tool
            })
            .collect();
        serde_json::json!({
            "type": "api-request-shape",
            "shapeHash": hash,
            "shape": {"system": system, "tools": tools},
        })
        .to_string()
    }

    fn request(hash: &str, source: &str, ts: &str) -> String {
        serde_json::json!({
            "type": "api-request",
            "shapeHash": hash,
            "querySource": source,
            "timestamp": ts,
        })
        .to_string()
    }

    fn attachment(attachment: serde_json::Value) -> String {
        serde_json::json!({
            "type": "attachment",
            "timestamp": "2026-01-01T00:00:00.000Z",
            "attachment": attachment,
        })
        .to_string()
    }

    fn tool_use(name: &str) -> String {
        serde_json::json!({
            "type": "assistant",
            "timestamp": "2026-01-01T00:00:05.000Z",
            "message": {"content": [{"type": "tool_use", "id": "toolu_x", "name": name, "input": {}}]},
        })
        .to_string()
    }

    /// Weight of one synthetic tool definition as `shape` writes it.
    fn tool_tokens(name: &str, deferred: bool) -> u64 {
        let mut tool = serde_json::json!({"name": name, "description": "dddd", "input_schema": {}});
        if deferred {
            tool["defer_loading"] = serde_json::json!(true);
        }
        crate::adapter::config::approx_tokens(&tool.to_string())
    }

    #[test]
    fn a_main_loop_shape_is_weighed_without_its_text() {
        let jsonl = [
            shape("h1", &["12345678", "abcd"], &[("Bash", false)]),
            request("h1", "repl_main_thread", "2026-01-01T00:00:01.000Z"),
        ]
        .join("\n");
        let context = extract_tool_context(&jsonl);
        assert_eq!(
            context.shapes,
            vec![WeighedShape {
                hash: "h1".into(),
                system_tokens: 3,
                system_blocks: 2,
                tools: vec![ShapeTool {
                    name: "Bash".into(),
                    server: None,
                    deferred: false,
                    tokens: tool_tokens("Bash", false),
                }],
            }]
        );
    }

    #[test]
    fn a_subagent_shape_is_not_a_session_shape() {
        let jsonl = [
            shape("h_agent", &["x"], &[]),
            request(
                "h_agent",
                "agent:builtin:Explore",
                "2026-01-01T00:00:01.000Z",
            ),
        ]
        .join("\n");
        assert_eq!(extract_tool_context(&jsonl), ToolContext::default());
    }

    #[test]
    fn shape_uses_count_requests_from_their_first_appearance() {
        let jsonl = [
            shape("h1", &[], &[]),
            request("h1", "repl_main_thread", "2026-01-01T00:00:01.000Z"),
            request("h1", "repl_main_thread", "2026-01-01T00:00:02.000Z"),
            shape("h2", &[], &[]),
            request("h2", "sdk", "2026-01-01T00:00:03.000Z"),
        ]
        .join("\n");
        assert_eq!(
            extract_tool_context(&jsonl).uses,
            vec![
                ShapeUse {
                    hash: "h1".into(),
                    first_epoch_ms: 1_767_225_601_000,
                    requests: 2,
                },
                ShapeUse {
                    hash: "h2".into(),
                    first_epoch_ms: 1_767_225_603_000,
                    requests: 1,
                },
            ]
        );
    }

    #[test]
    fn eager_tools_come_from_the_startup_shape_and_carry_their_server() {
        let jsonl = [
            shape(
                "h1",
                &[],
                &[
                    ("Read", false),
                    ("mcp__srv__q", false),
                    ("DeferredToolPlaceholder", true),
                ],
            ),
            request("h1", "repl_main_thread", "2026-01-01T00:00:01.000Z"),
            // A later shape that loaded a deferred tool does not make it eager.
            shape(
                "h2",
                &[],
                &[
                    ("Read", false),
                    ("mcp__srv__q", false),
                    ("mcp__srv__late", true),
                ],
            ),
            request("h2", "repl_main_thread", "2026-01-01T00:00:09.000Z"),
            tool_use("mcp__srv__q"),
            tool_use("mcp__srv__q"),
        ]
        .join("\n");
        let tools = extract_tool_context(&jsonl).tools;
        assert_eq!(
            tools,
            vec![
                SessionTool {
                    name: "Read".into(),
                    server: None,
                    eager_tokens: Some(tool_tokens("Read", false)),
                    listing_tokens: None,
                    loaded: false,
                    calls: 0,
                },
                SessionTool {
                    name: "mcp__srv__q".into(),
                    server: Some("srv".into()),
                    eager_tokens: Some(tool_tokens("mcp__srv__q", false)),
                    listing_tokens: None,
                    loaded: false,
                    calls: 2,
                },
            ]
        );
    }

    #[test]
    fn a_deferred_tool_is_listed_loaded_and_called_once_per_session() {
        let jsonl = [
            attachment(serde_json::json!({
                "type": "deferred_tools_delta",
                "addedNames": ["mcp__srv__late"],
                "addedLines": ["mcp__srv__late"],
            })),
            // Re-announced after a compaction: still one listing line.
            attachment(serde_json::json!({
                "type": "deferred_tools_delta",
                "addedNames": ["mcp__srv__late"],
                "addedLines": ["mcp__srv__late"],
            })),
            attachment(serde_json::json!({
                "type": "deferred_tools_record",
                "entries": [{"name": "mcp__srv__late", "defer_loading": true}],
            })),
            tool_use("mcp__srv__late"),
        ]
        .join("\n");
        assert_eq!(
            extract_tool_context(&jsonl).tools,
            vec![SessionTool {
                name: "mcp__srv__late".into(),
                server: Some("srv".into()),
                eager_tokens: None,
                listing_tokens: Some(4),
                loaded: true,
                calls: 1,
            }]
        );
    }

    #[test]
    fn mcp_instructions_are_weighed_once_per_server_under_its_id() {
        let block = attachment(serde_json::json!({
            "type": "mcp_instructions_delta",
            "addedNames": ["Server A"],
            "addedBlocks": ["## Server A\nabcd"],
        }));
        let jsonl = [block.clone(), block].join("\n");
        assert_eq!(
            extract_tool_context(&jsonl).mcp_instructions,
            vec![("Server_A".to_string(), 4)]
        );
    }

    #[test]
    fn a_startup_shape_that_was_never_recorded_leaves_eager_tools_unknown() {
        let jsonl = [
            request("h_missing", "repl_main_thread", "2026-01-01T00:00:01.000Z"),
            shape("h2", &[], &[("mcp__srv__q", false)]),
            request("h2", "repl_main_thread", "2026-01-01T00:00:09.000Z"),
        ]
        .join("\n");
        let context = extract_tool_context(&jsonl);
        assert!(
            context.tools.is_empty(),
            "a later shape is not the startup shape"
        );
        let hashes: Vec<&str> = context.uses.iter().map(|use_| use_.hash.as_str()).collect();
        assert_eq!(hashes, vec!["h_missing", "h2"]);
    }

    #[test]
    fn a_transcript_without_shape_records_yields_nothing() {
        let jsonl = ["not json", r#"{"type":"user","message":{"content":"hi"}}"#].join("\n");
        assert_eq!(extract_tool_context(&jsonl), ToolContext::default());
    }
}
