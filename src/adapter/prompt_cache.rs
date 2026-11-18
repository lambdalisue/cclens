//! Read a main transcript's API traffic into the prompt-cache domain model:
//! one `CacheRequest` per API response, compactions in between, the request
//! shapes the session sent, and the cost Claude Code itself reported
//! (`docs/specs/session-format.md`, "Request records").

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};

use serde::Deserialize;
use serde_json::Value;

use crate::core::cache::{CacheRequest, CacheStep, CacheTrace, CacheTtl, RequestShape};

const SYNTHETIC_MODEL: &str = "<synthetic>";

#[derive(Debug, Clone, PartialEq, Default)]
pub struct PromptCacheExtract {
    pub trace: CacheTrace,
    /// The last `cost-state` total in the file, when the session wrote one.
    pub reported_cost_usd: Option<f64>,
}

#[derive(Deserialize)]
struct Raw {
    #[serde(rename = "type")]
    kind: Option<String>,
    subtype: Option<String>,
    timestamp: Option<String>,
    #[serde(rename = "isSidechain")]
    is_sidechain: Option<bool>,
    #[serde(rename = "requestRef")]
    request_ref: Option<String>,
    #[serde(rename = "promptCacheRequested")]
    cache_requested: Option<String>,
    message: Option<RawMessage>,
    id: Option<String>,
    #[serde(rename = "shapeHash")]
    shape_hash: Option<String>,
    shape: Option<RawShape>,
    #[serde(rename = "totalCostUSD")]
    total_cost_usd: Option<f64>,
}

#[derive(Deserialize)]
struct RawMessage {
    id: Option<String>,
    model: Option<String>,
    usage: Option<RawUsage>,
}

#[derive(Deserialize)]
struct RawUsage {
    input_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
    cache_creation: Option<RawCacheCreation>,
}

#[derive(Deserialize)]
struct RawCacheCreation {
    ephemeral_5m_input_tokens: Option<u64>,
    ephemeral_1h_input_tokens: Option<u64>,
}

#[derive(Deserialize)]
struct RawShape {
    tools: Option<Vec<Value>>,
    system: Option<Vec<Value>>,
}

/// Extract the prompt-cache trace of one main transcript.
///
/// Every line of a response repeats its cache figures, so only the first line
/// of each `message.id` becomes a request. A sidechain line belongs to a
/// subagent's cache, and a `<synthetic>` line was never sent, so both are
/// skipped.
pub fn extract_prompt_cache(jsonl: &str) -> PromptCacheExtract {
    let mut extract = PromptCacheExtract::default();
    let mut sent: HashMap<String, (Option<i64>, Option<String>)> = HashMap::new();
    let mut seen: HashSet<String> = HashSet::new();
    for line in jsonl.lines() {
        let Ok(raw) = serde_json::from_str::<Raw>(line) else {
            continue;
        };
        match raw.kind.as_deref() {
            Some("api-request") => {
                if let Some(id) = raw.id {
                    let ts = raw.timestamp.as_deref().and_then(epoch_ms);
                    sent.insert(id, (ts, raw.shape_hash));
                }
            }
            Some("api-request-shape") => {
                if let (Some(hash), Some(shape)) = (raw.shape_hash, raw.shape) {
                    extract.trace.shapes.insert(hash, request_shape(&shape));
                }
            }
            Some("system") if raw.subtype.as_deref() == Some("compact_boundary") => {
                extract.trace.steps.push(CacheStep::Compaction);
            }
            Some("cost-state") => {
                if raw.total_cost_usd.is_some() {
                    extract.reported_cost_usd = raw.total_cost_usd;
                }
            }
            Some("assistant") => {
                if let Some(request) = cache_request(&raw, &sent, &mut seen) {
                    extract.trace.steps.push(CacheStep::Request(request));
                }
            }
            _ => {}
        }
    }
    extract
}

fn cache_request(
    raw: &Raw,
    sent: &HashMap<String, (Option<i64>, Option<String>)>,
    seen: &mut HashSet<String>,
) -> Option<CacheRequest> {
    if raw.is_sidechain == Some(true) {
        return None;
    }
    let message = raw.message.as_ref()?;
    let model = message.model.clone()?;
    if model == SYNTHETIC_MODEL {
        return None;
    }
    let usage = message.usage.as_ref()?;
    if let Some(id) = &message.id
        && !seen.insert(id.clone())
    {
        return None;
    }
    let (sent_at, shape) = raw
        .request_ref
        .as_ref()
        .and_then(|reference| sent.get(reference))
        .cloned()
        .unwrap_or_default();
    let sent_epoch_ms = sent_at.or_else(|| raw.timestamp.as_deref().and_then(epoch_ms))?;
    let written = usage.cache_creation_input_tokens.unwrap_or(0);
    // Without the split the whole write is the API's default lifetime.
    let (cache_write_5m, cache_write_1h) = match &usage.cache_creation {
        Some(split) => (
            split.ephemeral_5m_input_tokens.unwrap_or(0),
            split.ephemeral_1h_input_tokens.unwrap_or(0),
        ),
        None => (written, 0),
    };
    Some(CacheRequest {
        sent_epoch_ms,
        model,
        uncached: usage.input_tokens.unwrap_or(0),
        cache_read: usage.cache_read_input_tokens.unwrap_or(0),
        cache_write_5m,
        cache_write_1h,
        requested_ttl: match raw.cache_requested.as_deref() {
            Some("5m") => Some(CacheTtl::FiveMinutes),
            Some("1h") => Some(CacheTtl::OneHour),
            _ => None,
        },
        shape,
    })
}

/// Each tool's name with a digest of its whole definition, and a digest of
/// each system block. A digest only has to be stable within one run, since
/// shapes are compared within one session.
fn request_shape(shape: &RawShape) -> RequestShape {
    let tools = shape
        .tools
        .iter()
        .flatten()
        .filter_map(|tool| {
            let name = tool.get("name")?.as_str()?.to_string();
            Some((name, digest(&tool.to_string())))
        })
        .collect();
    let system = shape
        .system
        .iter()
        .flatten()
        .map(|block| digest(&block.get("text").and_then(Value::as_str)))
        .collect();
    RequestShape { tools, system }
}

fn digest(value: &impl Hash) -> u64 {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

fn epoch_ms(timestamp: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(timestamp)
        .ok()
        .map(|dt| dt.timestamp_millis())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assistant(id: &str, request_ref: &str, read: u64, split: Option<(u64, u64)>) -> String {
        let creation = match split {
            Some((m5, h1)) => format!(
                r#","cache_creation":{{"ephemeral_5m_input_tokens":{m5},"ephemeral_1h_input_tokens":{h1}}}"#
            ),
            None => String::new(),
        };
        let written = split.map(|(a, b)| a + b).unwrap_or(40);
        format!(
            r#"{{"type":"assistant","timestamp":"2026-01-01T00:00:09.000Z","requestRef":"{request_ref}","promptCacheRequested":"1h","message":{{"id":"{id}","model":"model-a","usage":{{"input_tokens":3,"cache_read_input_tokens":{read},"cache_creation_input_tokens":{written}{creation},"output_tokens":5}}}}}}"#
        )
    }

    fn api_request(id: &str, at: &str, hash: &str) -> String {
        format!(r#"{{"type":"api-request","id":"{id}","timestamp":"{at}","shapeHash":"{hash}"}}"#)
    }

    fn requests(jsonl: &str) -> Vec<CacheRequest> {
        extract_prompt_cache(jsonl)
            .trace
            .steps
            .into_iter()
            .filter_map(|step| match step {
                CacheStep::Request(request) => Some(request),
                CacheStep::Compaction => None,
            })
            .collect()
    }

    #[test]
    fn a_response_written_as_several_lines_is_one_request() {
        let line = assistant("msg_1", "req_1", 100, Some((0, 20)));
        let got = requests(&[line.clone(), line].join("\n"));
        assert_eq!(got.len(), 1);
    }

    #[test]
    fn the_prompt_is_split_into_read_write_by_lifetime_and_uncached() {
        let got = &requests(&assistant("msg_1", "req_1", 100, Some((7, 20))))[0];
        assert_eq!(
            (
                got.uncached,
                got.cache_read,
                got.cache_write_5m,
                got.cache_write_1h
            ),
            (3, 100, 7, 20)
        );
        assert_eq!(got.requested_ttl, Some(CacheTtl::OneHour));
    }

    #[test]
    fn without_the_split_the_whole_write_is_five_minute() {
        let got = &requests(&assistant("msg_1", "req_1", 100, None))[0];
        assert_eq!((got.cache_write_5m, got.cache_write_1h), (40, 0));
    }

    #[test]
    fn the_request_record_supplies_send_time_and_shape() {
        let jsonl = [
            api_request("req_1", "2026-01-01T00:00:01.000Z", "hash_a"),
            assistant("msg_1", "req_1", 100, None),
        ]
        .join("\n");
        let got = &requests(&jsonl)[0];
        assert_eq!(
            got.sent_epoch_ms,
            epoch_ms("2026-01-01T00:00:01.000Z").unwrap()
        );
        assert_eq!(got.shape.as_deref(), Some("hash_a"));
    }

    #[test]
    fn without_a_request_record_the_response_time_stands_in() {
        let got = &requests(&assistant("msg_1", "req_missing", 100, None))[0];
        assert_eq!(
            got.sent_epoch_ms,
            epoch_ms("2026-01-01T00:00:09.000Z").unwrap()
        );
        assert_eq!(got.shape, None);
    }

    #[test]
    fn sidechain_and_synthetic_lines_are_not_requests() {
        let sidechain = assistant("msg_1", "r", 1, None).replacen(
            r#""type":"assistant""#,
            r#""type":"assistant","isSidechain":true"#,
            1,
        );
        let synthetic = assistant("msg_2", "r", 1, None).replace("model-a", "<synthetic>");
        assert!(requests(&[sidechain, synthetic].join("\n")).is_empty());
    }

    #[test]
    fn a_compaction_sits_between_the_requests_around_it() {
        let jsonl = [
            assistant("msg_1", "r1", 1, None),
            r#"{"type":"system","subtype":"compact_boundary","timestamp":"2026-01-01T00:00:10.000Z"}"#
                .to_string(),
            assistant("msg_2", "r2", 1, None),
        ]
        .join("\n");
        let steps = extract_prompt_cache(&jsonl).trace.steps;
        assert!(matches!(steps[1], CacheStep::Compaction));
        assert_eq!(steps.len(), 3);
    }

    #[test]
    fn a_shape_keeps_tool_names_and_compares_system_blocks_by_digest() {
        let jsonl = [
            r#"{"type":"api-request-shape","shapeHash":"h1","shape":{"tools":[{"name":"Read"},{"name":"mcp__srv__x"}],"system":[{"type":"text","text":"alpha"}]}}"#,
            r#"{"type":"api-request-shape","shapeHash":"h2","shape":{"tools":[{"name":"Read"}],"system":[{"type":"text","text":"beta"}]}}"#,
        ]
        .join("\n");
        let shapes = extract_prompt_cache(&jsonl).trace.shapes;
        let tools: Vec<_> = shapes["h1"].tools.keys().map(String::as_str).collect();
        assert_eq!(tools, vec!["Read", "mcp__srv__x"]);
        assert_ne!(shapes["h1"].system, shapes["h2"].system);
    }

    #[test]
    fn the_last_cost_state_is_the_reported_session_cost() {
        let jsonl = [
            r#"{"type":"cost-state","totalCostUSD":1.5}"#,
            r#"{"type":"cost-state","totalCostUSD":2.25}"#,
        ]
        .join("\n");
        assert_eq!(extract_prompt_cache(&jsonl).reported_cost_usd, Some(2.25));
    }

    #[test]
    fn a_session_without_cost_state_has_no_reported_cost() {
        let got = extract_prompt_cache(&assistant("msg_1", "r", 1, None));
        assert_eq!(got.reported_cost_usd, None);
    }
}
