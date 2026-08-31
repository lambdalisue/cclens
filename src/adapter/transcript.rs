//! Parse a Claude Code session transcript (JSONL) into domain records.
//!
//! `parse_session` is a pure function over the file's text — file walking and
//! reading are a thin shell around it (`docs/specs/session-format.md`). It
//! deserializes defensively: only the needed fields, unknown fields ignored, a
//! line that fails to parse or lacks a timestamp simply yields no records.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::Value;

use crate::core::friction::{ErrorCategory, classify_error};
use crate::core::path::basename;
use crate::core::prompt::{PromptBehavior, classify_prompt};
use crate::core::span::{Record, RecordKind, Source};

const SYNTHETIC_MODEL: &str = "<synthetic>";

/// Parse one transcript's text into domain records, in file order. The current
/// turn's prompt id is threaded forward and stamped onto agent spawns, whose own
/// record does not carry it — that id is the join key to the subagent transcript.
pub fn parse_session(jsonl: &str) -> Vec<Record> {
    let mut current_prompt_id: Option<String> = None;
    let mut records = Vec::new();
    for line in jsonl.lines() {
        parse_line(line, &mut current_prompt_id, &mut records);
    }
    records
}

#[derive(Deserialize)]
struct Raw {
    #[serde(rename = "type")]
    kind: Option<String>,
    timestamp: Option<String>,
    #[serde(rename = "isMeta")]
    is_meta: Option<bool>,
    #[serde(rename = "promptId")]
    prompt_id: Option<String>,
    /// Why a denied tool call was denied. Sits on the entry, one level above the
    /// `tool_result` block it explains, and only recent Claude Code versions
    /// write it — hence optional.
    #[serde(rename = "toolDenialKind")]
    denial_kind: Option<String>,
    message: Option<RawMessage>,
    /// Top-level content (system `local_command` records carry it here rather
    /// than under `message`).
    content: Option<Value>,
}

#[derive(Deserialize)]
struct RawMessage {
    model: Option<String>,
    usage: Option<RawUsage>,
    content: Option<Value>,
}

#[derive(Deserialize)]
struct RawUsage {
    input_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
    output_tokens: Option<u64>,
}

fn parse_line(line: &str, current_prompt_id: &mut Option<String>, out: &mut Vec<Record>) {
    let Ok(raw) = serde_json::from_str::<Raw>(line) else {
        return;
    };
    if raw.prompt_id.is_some() {
        current_prompt_id.clone_from(&raw.prompt_id);
    }
    let Some(ts) = raw.timestamp.as_deref().and_then(parse_timestamp_ms) else {
        return;
    };

    let mut records = match raw.kind.as_deref() {
        Some("assistant") => assistant_records(ts, raw.message.as_ref()),
        Some("user") | Some("system") => prompt_or_invocation(ts, &raw, line),
        _ => Vec::new(),
    };
    for record in &mut records {
        if let RecordKind::AgentSpawn { prompt_id, .. } = &mut record.kind {
            prompt_id.clone_from(current_prompt_id);
        }
    }
    out.extend(records);
}

/// An assistant line yields its accumulated cost, plus a tool-path skill
/// invocation when it called the Skill tool (emitted first so the span starts
/// at the invocation and includes the calling turn's cost).
fn assistant_records(ts: i64, message: Option<&RawMessage>) -> Vec<Record> {
    let Some(message) = message else {
        return Vec::new();
    };
    let mut records = Vec::new();

    if let Some(blocks) = message
        .content
        .as_ref()
        .and_then(|content| content.as_array())
    {
        for block in blocks {
            if let Some(kind) = tool_use_kind(block) {
                records.push(Record {
                    timestamp_ms: ts,
                    kind,
                });
            }
        }
    }

    if let Some(usage) = &message.usage {
        let prompt_size = usage.input_tokens.unwrap_or(0)
            + usage.cache_read_input_tokens.unwrap_or(0)
            + usage.cache_creation_input_tokens.unwrap_or(0);
        records.push(Record {
            timestamp_ms: ts,
            kind: RecordKind::Assistant {
                prompt_size,
                out_tokens: usage.output_tokens.unwrap_or(0),
                model: message
                    .model
                    .clone()
                    .unwrap_or_else(|| SYNTHETIC_MODEL.to_string()),
            },
        });
    }

    records
}

/// A user/system line is a slash invocation (when its content *is* a command
/// wrapper), a human turn (a real prompt), or nothing we track.
fn prompt_or_invocation(ts: i64, raw: &Raw, line: &str) -> Vec<Record> {
    if let Some(skill) = command_content(raw).and_then(extract_command_name) {
        return vec![Record {
            timestamp_ms: ts,
            kind: RecordKind::SkillInvocation {
                skill,
                source: Source::Slash,
            },
        }];
    }

    let is_human_turn = raw.kind.as_deref() == Some("user")
        && raw.is_meta != Some(true)
        && !line.contains("tool_result");
    if is_human_turn {
        vec![Record {
            timestamp_ms: ts,
            kind: RecordKind::HumanTurn,
        }]
    } else {
        Vec::new()
    }
}

/// The record's content string *only when it is a command wrapper* — i.e. the
/// content, trimmed, begins with a `<command-…>` tag (a real invocation leads
/// with `<command-message>` or `<command-name>`). This is the structural guard
/// that keeps a `<command-name>` quoted inside ordinary prose (a prompt that
/// discusses commands) from being mis-read as an invocation. See
/// `docs/specs/session-format.md`.
fn command_content(raw: &Raw) -> Option<&str> {
    let content = raw
        .message
        .as_ref()
        .and_then(|message| message.content.as_ref())
        .or(raw.content.as_ref())?
        .as_str()?;
    content
        .trim_start()
        .starts_with("<command-")
        .then_some(content)
}

/// For each user prompt: a pointer `(source_line, epoch_ms)` and its behavioral
/// class (steer / correct / question / instruct). The prompt *text* is never
/// stored — only the pointer and the derived class — so prompt analysis stays
/// possible after transcripts rotate without copying personal text into the
/// store. See `docs/specs/storage.md`, `events.md`, `core::prompt`.
pub fn extract_prompt_pointers(jsonl: &str) -> Vec<(usize, i64, PromptBehavior)> {
    jsonl
        .lines()
        .enumerate()
        .filter_map(|(line_no, line)| {
            let raw: Raw = serde_json::from_str(line).ok()?;
            let ts = raw.timestamp.as_deref().and_then(parse_timestamp_ms)?;
            let is_prompt = raw.kind.as_deref() == Some("user")
                && raw.is_meta != Some(true)
                && command_content(&raw).is_none()
                && !line.contains("tool_result");
            if !is_prompt {
                return None;
            }
            let text = raw
                .message
                .as_ref()
                .and_then(|message| message.content.as_ref())
                .and_then(|content| content.as_str())
                .unwrap_or("");
            Some((line_no, ts, classify_prompt(text)))
        })
        .collect()
}

/// One failed tool result: when it happened, its friction category, a readable
/// excerpt of the error text, the tool that produced it, and the call's target —
/// the file_path it edited or the command it ran, recovered from the originating
/// `tool_use` input (the error text alone often omits it, e.g. edit-precondition).
pub struct ToolError {
    pub epoch_ms: i64,
    pub category: ErrorCategory,
    pub excerpt: String,
    pub tool: String,
    pub target: String,
}

/// Extract failed tool results from a transcript — the raw material for friction
/// analysis. A tool result is a failure when it is flagged `is_error` or carries
/// a `tool_use_error` wrapper. A denied call is categorised from the entry's
/// `toolDenialKind` marker (see `denial_category`) and everything else from its
/// text (`core::friction`). Two details ride along so a report is actionable without
/// re-reading the transcript: a cleaned, truncated **excerpt** (the actual
/// failing path/file), and the originating **tool** — recovered by threading the
/// `tool_use` → `tool_result` link (the result's `tool_use_id` matches the
/// assistant `tool_use` block's `id`), so file-edit failures are told apart from,
/// say, a Playwright locator miss that merely reads as "not found".
pub fn extract_tool_errors(jsonl: &str) -> Vec<ToolError> {
    // tool_use id -> (tool name, target), filled as assistant records stream past
    // (a tool_use always precedes its result, so one forward pass suffices).
    let mut tool_calls: std::collections::HashMap<String, (String, String)> =
        std::collections::HashMap::new();
    let mut errors = Vec::new();
    for line in jsonl.lines() {
        let Ok(raw) = serde_json::from_str::<Raw>(line) else {
            continue;
        };
        match raw.kind.as_deref() {
            Some("assistant") => {
                let Some(blocks) = raw
                    .message
                    .as_ref()
                    .and_then(|message| message.content.as_ref())
                    .and_then(|content| content.as_array())
                else {
                    continue;
                };
                for block in blocks {
                    if block.get("type").and_then(|v| v.as_str()) != Some("tool_use") {
                        continue;
                    }
                    if let (Some(id), Some(name)) = (
                        block.get("id").and_then(|v| v.as_str()),
                        block.get("name").and_then(|v| v.as_str()),
                    ) {
                        let target = tool_target(block.get("input"));
                        tool_calls.insert(id.to_string(), (name.to_string(), target));
                    }
                }
            }
            Some("user") => {
                let Some(ts) = raw.timestamp.as_deref().and_then(parse_timestamp_ms) else {
                    continue;
                };
                let Some(blocks) = raw
                    .message
                    .as_ref()
                    .and_then(|message| message.content.as_ref())
                    .and_then(|content| content.as_array())
                else {
                    continue;
                };
                // Collected once: the marker needs their count, the loop needs
                // the blocks themselves.
                let failed: Vec<&Value> = blocks
                    .iter()
                    .filter(|block| is_failed_result(block))
                    .collect();
                // The marker sits on the entry and names no block, so it can only
                // be attributed when the entry holds a single failure. Every
                // transcript seen writes exactly one `tool_result` per entry;
                // were upstream ever to batch several, a shared marker would not
                // say which of them it denied, so the text decides for all.
                let denial = raw
                    .denial_kind
                    .as_deref()
                    .and_then(denial_category)
                    .filter(|_| failed.len() == 1);
                for block in failed {
                    let content_value = block.get("content");
                    // Classify on the JSON form (substring heuristics); excerpt
                    // from the human-readable text.
                    let content = content_value.map(|v| v.to_string()).unwrap_or_default();
                    let call = block
                        .get("tool_use_id")
                        .and_then(|v| v.as_str())
                        .and_then(|id| tool_calls.get(id));
                    let (tool, target) = match call {
                        Some((name, target)) => (name.clone(), target.clone()),
                        None => ("unknown".to_string(), String::new()),
                    };
                    errors.push(ToolError {
                        epoch_ms: ts,
                        category: denial.unwrap_or_else(|| classify_error(&content)),
                        excerpt: content_value.map(error_excerpt).unwrap_or_default(),
                        tool,
                        target,
                    });
                }
            }
            _ => {}
        }
    }
    errors
}

/// Whether a content block is a **failed** `tool_result`: flagged `is_error`, or
/// wrapping a `tool_use_error`.
fn is_failed_result(block: &Value) -> bool {
    block.get("type").and_then(|v| v.as_str()) == Some("tool_result")
        && (block.get("is_error").and_then(|v| v.as_bool()) == Some(true)
            || block.get("content").is_some_and(wraps_tool_use_error))
}

/// Whether a tool-result `content` carries the `tool_use_error` wrapper. Plain
/// string content — the common shape, and the one every wrapper uses — is
/// searched in place; only the array and object forms pay for a serialized copy.
/// This runs over every tool result in every transcript, most of which succeeded
/// and reach the substring test, so the copy is the cost worth not paying.
fn wraps_tool_use_error(content: &Value) -> bool {
    match content.as_str() {
        Some(text) => text.contains("tool_use_error"),
        None => content.to_string().contains("tool_use_error"),
    }
}

/// The friction category a denied tool call belongs to, from the `toolDenialKind`
/// marker Claude Code stamps on the denying entry. This is what makes denials
/// classifiable at all: the text below a denial is written by the *user's* hook,
/// so it carries arbitrary wording in an arbitrary language and no keyword list
/// can converge on it — while the marker is upstream's own, fixed vocabulary.
///
/// `None` for a kind we do not know, so the error text still gets its say;
/// upstream adds kinds over time and an unrecognised one must not erase what the
/// text already reveals. Teaching this map a new kind requires bumping
/// `ANALYZER_VERSION` (`store.rs`), or already-ingested sessions keep their old
/// categories forever (`docs/specs/storage.md`).
fn denial_category(kind: &str) -> Option<ErrorCategory> {
    match kind {
        // Refused by a permission rule, a PreToolUse hook, or auto mode's
        // classifier — all config the user owns and can relax.
        "permission-rule" | "automode-blocked" => Some(ErrorCategory::BlockedByHook),
        // The user answered no at the prompt: a stop, not a code fault.
        "user-rejected" => Some(ErrorCategory::Cancelled),
        _ => None,
    }
}

/// The subject of a tool call, from its `input`: the file it touched or the
/// command it ran — the first present of a small set of identifying fields. This
/// is what the error text frequently omits (an edit-precondition failure names
/// no path), so it is what a friction-by-file query needs.
fn tool_target(input: Option<&Value>) -> String {
    let Some(input) = input else {
        return String::new();
    };
    for key in [
        "file_path",
        "notebook_path",
        "path",
        "command",
        "pattern",
        "url",
    ] {
        if let Some(value) = input.get(key).and_then(|v| v.as_str()) {
            return clean_truncate(value);
        }
    }
    String::new()
}

/// Whitespace-collapse and truncate by Unicode scalar (never mid-character).
fn clean_truncate(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(200)
        .collect()
}

/// Distil a tool-result `content` value into a short, readable excerpt: the text
/// payload (a bare string, or the joined `text` blocks), whitespace-collapsed and
/// truncated by Unicode scalar so multi-byte text is never split mid-character.
fn error_excerpt(content: &Value) -> String {
    let raw = match content {
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join(" "),
        other => other.to_string(),
    };
    clean_truncate(&raw)
}

/// One unit of work Claude performed: a Bash command or a file edit.
pub struct WorkEvent {
    pub epoch_ms: i64,
    pub kind: &'static str,
    /// The ranking identity: the command's leading word, or the edited file's
    /// basename. Deliberately lossy — it is what groups a hotspot list.
    pub id: String,
    /// The edited file's full path (`file_edit` only). The basename cannot tell
    /// two same-named files apart, which matters for thrash detection
    /// (`core::thrash`) even though it does not for a hotspot ranking.
    pub path: Option<String>,
}

/// Extract work events from a transcript: the leading word of each Bash command
/// (`kind = "bash_cmd"`) and each Edit/Write target (`kind = "file_edit"`). These
/// drive the command-mix and file-hotspot views — where effort (and churn)
/// concentrates.
pub fn extract_work_events(jsonl: &str) -> Vec<WorkEvent> {
    let mut events = Vec::new();
    for line in jsonl.lines() {
        let Ok(raw) = serde_json::from_str::<Raw>(line) else {
            continue;
        };
        if raw.kind.as_deref() != Some("assistant") {
            continue;
        }
        let Some(ts) = raw.timestamp.as_deref().and_then(parse_timestamp_ms) else {
            continue;
        };
        let Some(blocks) = raw
            .message
            .as_ref()
            .and_then(|message| message.content.as_ref())
            .and_then(|content| content.as_array())
        else {
            continue;
        };
        for block in blocks {
            if block.get("type").and_then(|v| v.as_str()) != Some("tool_use") {
                continue;
            }
            let name = block.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let input = block.get("input");
            match name {
                "Bash" => {
                    if let Some(cmd) = input
                        .and_then(|i| i.get("command"))
                        .and_then(|v| v.as_str())
                        .and_then(|c| c.split_whitespace().next())
                    {
                        events.push(WorkEvent {
                            epoch_ms: ts,
                            kind: "bash_cmd",
                            id: cmd.to_string(),
                            path: None,
                        });
                    }
                }
                "Edit" | "Write" | "NotebookEdit" => {
                    if let Some(path) = input
                        .and_then(|i| i.get("file_path"))
                        .and_then(|v| v.as_str())
                    {
                        events.push(WorkEvent {
                            epoch_ms: ts,
                            kind: "file_edit",
                            id: basename(path).to_string(),
                            path: Some(path.to_string()),
                        });
                    }
                }
                _ => {}
            }
        }
    }
    events
}

/// Count the denials an allow rule could have prevented — the friction signal
/// behind the `permission` surface. A denying entry states its own reason, so
/// counting is structural wherever that marker is present; a user's own "no" is
/// excluded, since no rule change would have avoided it. Older transcripts carry
/// no marker, so the English denial phrase remains as a fallback — that part is
/// still a lower-confidence heuristic (`docs/specs/session-format.md`).
pub fn count_permission_denials(jsonl: &str) -> usize {
    jsonl.lines().filter(|line| is_rule_denial(line)).count()
}

/// Whether one raw entry is a denial an allow rule could have prevented.
fn is_rule_denial(line: &str) -> bool {
    const MARKER: &str = "Permission for this action was denied";
    match line_denial_kind(line).as_deref().map(denial_category) {
        // A kind we know decides on its own — including deciding *against*, as a
        // user's own "no" does.
        Some(Some(category)) => category == ErrorCategory::BlockedByHook,
        // No marker, or one we do not recognise: let the text speak.
        _ => line.contains("tool_result") && line.contains(MARKER),
    }
}

/// The denial marker on one raw entry, parsed only for lines that mention it so
/// the common case stays a substring check.
fn line_denial_kind(line: &str) -> Option<String> {
    if !line.contains("toolDenialKind") {
        return None;
    }
    serde_json::from_str::<Raw>(line)
        .ok()
        .and_then(|raw| raw.denial_kind)
}

/// The working directory a session started in — the project root. Records carry
/// a `cwd` field; a session that changes directory mid-way stamps later records
/// with the child path, so the **first** recorded cwd is the root (it is the
/// directory the transcript's own cwd-slug encodes, and it is deterministic).
/// `None` when no record carries one.
pub fn session_cwd(jsonl: &str) -> Option<String> {
    #[derive(Deserialize)]
    struct RawCwd {
        cwd: Option<String>,
    }
    jsonl.lines().find_map(|line| {
        serde_json::from_str::<RawCwd>(line)
            .ok()
            .and_then(|raw| raw.cwd)
    })
}

/// The `promptId` a subagent transcript was spawned under — the coarse join key
/// back to the spawning span. Read from the first record that carries one.
pub fn subagent_prompt_id(jsonl: &str) -> Option<String> {
    jsonl.lines().find_map(|line| {
        serde_json::from_str::<Raw>(line)
            .ok()
            .and_then(|raw| raw.prompt_id)
    })
}

/// What a subagent transcript's **sidecar** says about the run: which agent type
/// ran, which `Agent` call spawned it, and where it sits in the spawn tree.
///
/// The transcript itself never names the agent type, so without the sidecar a
/// run's cost cannot be attributed to a type at all. Only the fields the tool
/// needs are read, and every one is optional — a sidecar from a newer (or
/// older) Claude Code still yields whatever it does carry, and a missing file
/// yields `None` rather than a guess (`docs/specs/session-format.md`).
#[derive(Debug, Clone, PartialEq, Default, Deserialize)]
#[serde(default)]
pub struct SubagentSidecar {
    #[serde(rename = "agentType")]
    pub agent_type: Option<String>,
    #[serde(rename = "toolUseId")]
    pub tool_use_id: Option<String>,
    #[serde(rename = "parentAgentId")]
    pub parent_agent_id: Option<String>,
    #[serde(rename = "spawnDepth")]
    pub spawn_depth: Option<u32>,
    pub model: Option<String>,
}

/// Parse a subagent sidecar's JSON. `None` when the text is not readable JSON —
/// an absent or malformed sidecar leaves the run's type unknown, which every
/// consumer reports as unknown rather than filling in.
pub fn parse_subagent_sidecar(json: &str) -> Option<SubagentSidecar> {
    serde_json::from_str(json).ok()
}

/// The subagent's own id, from its transcript file name `agent-<agentId>.jsonl`
/// — the key a nested run's sidecar names as its parent.
pub fn subagent_id_from_file_name(file_stem: &str) -> String {
    file_stem
        .strip_prefix("agent-")
        .unwrap_or(file_stem)
        .to_string()
}

/// Where a session's subagent transcripts live: `<sessionId>/subagents/` beside
/// the main transcript (`docs/specs/session-format.md`).
///
/// A path rule, not a read — the shell still does the walking. It lives here
/// because the *layout* is Claude Code's, so a release that moves these files
/// changes this function and nothing downstream
/// (`.claude/rules/format-isolation.md`).
pub fn subagents_dir(transcript: &Path) -> PathBuf {
    transcript.with_extension("").join("subagents")
}

/// The sidecar beside a subagent transcript: `agent-<id>.jsonl` →
/// `agent-<id>.meta.json`.
pub fn subagent_sidecar_path(run_path: &Path) -> PathBuf {
    run_path.with_extension("meta.json")
}

fn parse_timestamp_ms(timestamp: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(timestamp)
        .ok()
        .map(|dt| dt.timestamp_millis())
}

/// Classify a `tool_use` content block into a domain record kind: the Skill tool
/// is a tool-path skill invocation, the Agent tool is a subagent spawn, and any
/// other named tool is a `ToolUse` (the core decides which are MCP). Returns
/// `None` for non-`tool_use` blocks.
fn tool_use_kind(block: &Value) -> Option<RecordKind> {
    if block.get("type")?.as_str()? != "tool_use" {
        return None;
    }
    let name = block.get("name")?.as_str()?;
    match name {
        "Skill" => {
            let skill = block.get("input")?.get("skill")?.as_str()?.to_string();
            Some(RecordKind::SkillInvocation {
                skill,
                source: Source::Tool,
            })
        }
        "Agent" => {
            let agent = block
                .get("input")?
                .get("subagent_type")?
                .as_str()?
                .to_string();
            // The spawning turn's prompt id is threaded in by parse_session; the
            // Agent record itself does not carry it. The block's own id does
            // identify this call, and the spawned transcript names it back.
            Some(RecordKind::AgentSpawn {
                agent,
                tool_use_id: block.get("id").and_then(|id| id.as_str()).map(String::from),
                prompt_id: None,
            })
        }
        tool => Some(RecordKind::ToolUse {
            tool: tool.to_string(),
        }),
    }
}

/// The skill name from a `<command-name>/NAME</command-name>` tag, leading slash
/// stripped. The caller passes only a verified command-wrapper string
/// (`command_content`), so this is structural, not a substring scan over
/// arbitrary content — see `docs/specs/session-format.md`.
fn extract_command_name(content: &str) -> Option<String> {
    let start = content.find("<command-name>")? + "<command-name>".len();
    let end = start + content[start..].find("</command-name>")?;
    let name = content[start..end].trim().trim_start_matches('/').trim();
    (!name.is_empty()).then(|| name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::span::extract_spans;

    #[test]
    fn parses_a_slash_invocation_with_its_assistant_cost() {
        // A synthetic, fabricated transcript — never a real one (privacy rule).
        let jsonl = concat!(
            r#"{"type":"user","timestamp":"2026-01-01T00:00:00.000Z","message":{"content":"<command-name>/git-commit</command-name>"}}"#,
            "\n",
            r#"{"type":"assistant","timestamp":"2026-01-01T00:00:01.000Z","message":{"model":"claude-opus-4-7","usage":{"input_tokens":10,"cache_read_input_tokens":90,"cache_creation_input_tokens":0,"output_tokens":40}}}"#,
            "\n",
            r#"{"type":"assistant","timestamp":"2026-01-01T00:00:03.000Z","message":{"model":"claude-opus-4-7","usage":{"input_tokens":0,"cache_read_input_tokens":250,"cache_creation_input_tokens":0,"output_tokens":60}}}"#,
            "\n",
            r#"{"type":"user","timestamp":"2026-01-01T00:00:05.000Z","message":{"content":"thanks"}}"#,
        );

        let spans = extract_spans(
            &parse_session(jsonl),
            crate::core::span::DEFAULT_IDLE_GAP_MS,
        );

        assert_eq!(spans.len(), 1);
        let span = &spans[0];
        assert_eq!(span.skill, "git-commit");
        assert_eq!(span.source, Source::Slash);
        assert_eq!(span.out_tokens, 100); // 40 + 60
        assert_eq!(span.ctx_growth, 150); // (250 - 100), the closing prompt excluded
        assert_eq!(span.duration_sec, 3.0); // last in-window record at 3s, start at 0s
        assert_eq!(span.model.as_deref(), Some("claude-opus-4-7"));
    }

    #[test]
    fn detects_a_tool_path_invocation_from_a_skill_tool_use() {
        let jsonl = concat!(
            r#"{"type":"assistant","timestamp":"2026-01-01T00:00:00.000Z","message":{"model":"claude-opus-4-7","usage":{"input_tokens":5,"cache_read_input_tokens":5,"cache_creation_input_tokens":0,"output_tokens":1},"content":[{"type":"tool_use","name":"Skill","input":{"skill":"loop"}}]}}"#,
            "\n",
            r#"{"type":"user","timestamp":"2026-01-01T00:00:02.000Z","message":{"content":"done"}}"#,
        );

        let spans = extract_spans(
            &parse_session(jsonl),
            crate::core::span::DEFAULT_IDLE_GAP_MS,
        );

        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].skill, "loop");
        assert_eq!(spans[0].source, Source::Tool);
    }

    #[test]
    fn detects_a_command_wrapper_that_leads_with_command_message() {
        // Real invocations lead with <command-message>, then <command-name>.
        let jsonl = concat!(
            r#"{"type":"user","timestamp":"2026-01-01T00:00:00.000Z","message":{"content":"<command-message>git-commit</command-message>\n<command-name>/git-commit</command-name>"}}"#,
            "\n",
            r#"{"type":"user","timestamp":"2026-01-01T00:00:02.000Z","message":{"content":"ok"}}"#,
        );

        let spans = extract_spans(
            &parse_session(jsonl),
            crate::core::span::DEFAULT_IDLE_GAP_MS,
        );
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].skill, "git-commit");
    }

    #[test]
    fn work_events_capture_bash_leading_word_and_edit_basename() {
        let jsonl = concat!(
            r#"{"type":"assistant","timestamp":"2026-01-01T00:00:00.000Z","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"cd /x && cargo test"}}]}}"#,
            "\n",
            r#"{"type":"assistant","timestamp":"2026-01-01T00:00:01.000Z","message":{"content":[{"type":"tool_use","name":"Edit","input":{"file_path":"/a/b/cli.rs"}}]}}"#,
        );
        let events = extract_work_events(jsonl);
        assert_eq!(events[0].kind, "bash_cmd");
        assert_eq!(events[0].id, "cd"); // leading word only
        assert_eq!(events[0].path, None);
        assert_eq!(events[1].kind, "file_edit");
        assert_eq!(events[1].id, "cli.rs"); // basename, for hotspot ranking
        // The full path rides along so thrash detection can tell two same-named
        // files apart.
        assert_eq!(events[1].path.as_deref(), Some("/a/b/cli.rs"));
    }

    #[test]
    fn work_events_take_the_edit_basename_from_a_backslash_path() {
        // A transcript written on Windows spells the target natively, and the
        // same file also arrives with forward slashes. Both must land on one id
        // or the hotspot list ranks spellings instead of files.
        let jsonl = concat!(
            r#"{"type":"assistant","timestamp":"2026-01-01T00:00:00.000Z","message":{"content":[{"type":"tool_use","name":"Edit","input":{"file_path":"C:\\example\\repo\\cli.rs"}}]}}"#,
            "\n",
            r#"{"type":"assistant","timestamp":"2026-01-01T00:00:01.000Z","message":{"content":[{"type":"tool_use","name":"Edit","input":{"file_path":"repo/cli.rs"}}]}}"#,
        );
        let events = extract_work_events(jsonl);
        assert_eq!(events[0].id, "cli.rs");
        assert_eq!(events[1].id, "cli.rs");
        // The full path still distinguishes them for thrash detection.
        assert_eq!(events[0].path.as_deref(), Some(r"C:\example\repo\cli.rs"));
    }

    #[test]
    fn counts_permission_denials_in_tool_results_only() {
        let jsonl = concat!(
            r#"{"type":"user","timestamp":"2026-01-01T00:00:00.000Z","message":{"content":[{"type":"tool_result","content":"Permission for this action was denied by the user"}]}}"#,
            "\n",
            // The same phrase quoted in a prompt is not a denial.
            r#"{"type":"user","timestamp":"2026-01-01T00:00:01.000Z","message":{"content":"why does Permission for this action was denied appear?"}}"#,
            "\n",
            r#"{"type":"user","timestamp":"2026-01-01T00:00:02.000Z","message":{"content":[{"type":"tool_result","content":"ok"}]}}"#,
        );
        assert_eq!(count_permission_denials(jsonl), 1);
    }

    #[test]
    fn prompt_pointers_point_at_user_prompts_only() {
        let jsonl = concat!(
            r#"{"type":"user","timestamp":"2026-01-01T00:00:00.000Z","message":{"content":"do the thing"}}"#,
            "\n",
            r#"{"type":"assistant","timestamp":"2026-01-01T00:00:01.000Z","message":{"usage":{"output_tokens":1}}}"#,
            "\n",
            r#"{"type":"user","timestamp":"2026-01-01T00:00:02.000Z","message":{"content":[{"type":"tool_result","content":"x"}]}}"#,
            "\n",
            r#"{"type":"user","timestamp":"2026-01-01T00:00:03.000Z","message":{"content":"and another"}}"#,
        );

        let pointers = extract_prompt_pointers(jsonl);
        // Lines 0 and 3 are prompts; line 1 is assistant, line 2 a tool result.
        let lines: Vec<usize> = pointers.iter().map(|(line, _, _)| *line).collect();
        assert_eq!(lines, vec![0, 3]);
        assert!(
            pointers
                .iter()
                .all(|(_, _, b)| *b == crate::core::prompt::PromptBehavior::Instruct)
        );
    }

    #[test]
    fn does_not_treat_a_quoted_command_name_in_prose_as_an_invocation() {
        // A real prompt that merely *discusses* the tag must not be mis-detected.
        let jsonl = r#"{"type":"user","timestamp":"2026-01-01T00:00:00.000Z","message":{"content":"explain how <command-name>/git-commit</command-name> works"}}"#;

        let spans = extract_spans(
            &parse_session(jsonl),
            crate::core::span::DEFAULT_IDLE_GAP_MS,
        );
        assert!(spans.is_empty());
    }

    #[test]
    fn ignores_unparseable_lines_and_tool_results() {
        let jsonl = concat!(
            "not json\n",
            r#"{"type":"user","timestamp":"2026-01-01T00:00:00.000Z","message":{"content":[{"type":"tool_result","content":"x"}]}}"#,
        );
        assert!(parse_session(jsonl).is_empty());
    }

    #[test]
    fn session_cwd_is_the_first_recorded_cwd() {
        // A session may change directory mid-way (records then carry a child
        // cwd); the project root is the cwd the session *started* in — the same
        // directory the transcript's slug encodes.
        let jsonl = concat!(
            r#"{"type":"user","cwd":"/tmp/example/project","timestamp":"2026-01-01T00:00:00.000Z","message":{"content":"hi"}}"#,
            "\n",
            r#"{"type":"user","cwd":"/tmp/example/project/subdir","timestamp":"2026-01-01T00:00:01.000Z","message":{"content":"more"}}"#,
        );
        assert_eq!(session_cwd(jsonl).as_deref(), Some("/tmp/example/project"));
    }

    #[test]
    fn session_cwd_is_none_when_no_record_carries_one() {
        let jsonl = concat!(
            "not json\n",
            r#"{"type":"user","timestamp":"2026-01-01T00:00:00.000Z","message":{"content":"hi"}}"#,
        );
        assert_eq!(session_cwd(jsonl), None);
    }

    #[test]
    fn tool_errors_keep_a_readable_excerpt_with_the_failing_path() {
        // A string-content failure: the excerpt must carry the actual path so a
        // report can show which path was missed, not just the category.
        let jsonl = r#"{"type":"user","timestamp":"2026-01-01T00:00:00.000Z","message":{"content":[{"type":"tool_result","is_error":true,"content":"File does not exist: /tmp/example/foo.rs"}]}}"#;
        let errors = extract_tool_errors(jsonl);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].category, ErrorCategory::PathNotFound);
        assert!(errors[0].excerpt.contains("/tmp/example/foo.rs"));
    }

    #[test]
    fn tool_errors_are_attributed_to_the_tool_and_target_that_produced_them() {
        // The assistant's tool_use names the tool and its input target; the
        // failing tool_result links back via tool_use_id. The error text omits
        // the path, so the target must come from the tool_use input.
        let jsonl = concat!(
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_1","name":"Edit","input":{"file_path":"/tmp/example/foo.rs"}}]}}"#,
            "\n",
            r#"{"type":"user","timestamp":"2026-01-01T00:00:00.000Z","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_1","is_error":true,"content":"File has not been read yet."}]}}"#,
        );
        let errors = extract_tool_errors(jsonl);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].category, ErrorCategory::EditPrecondition);
        assert_eq!(errors[0].tool, "Edit");
        assert_eq!(errors[0].target, "/tmp/example/foo.rs");
    }

    #[test]
    fn bash_tool_error_target_is_the_command() {
        let jsonl = concat!(
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"toolu_2","name":"Bash","input":{"command":"fd --type f"}}]}}"#,
            "\n",
            r#"{"type":"user","timestamp":"2026-01-01T00:00:00.000Z","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_2","is_error":true,"content":"command not found: fd"}]}}"#,
        );
        let errors = extract_tool_errors(jsonl);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].tool, "Bash");
        assert_eq!(errors[0].target, "fd --type f");
    }

    #[test]
    fn an_error_with_no_matching_tool_use_is_attributed_to_unknown() {
        let jsonl = r#"{"type":"user","timestamp":"2026-01-01T00:00:00.000Z","message":{"content":[{"type":"tool_result","tool_use_id":"toolu_x","is_error":true,"content":"boom"}]}}"#;
        let errors = extract_tool_errors(jsonl);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].tool, "unknown");
    }

    #[test]
    fn a_marked_denial_is_classified_by_its_kind_not_its_wording() {
        // The text is hook-authored, so it can be in any language and match no
        // keyword; the entry-level denial marker classifies it regardless.
        let jsonl = concat!(
            r#"{"type":"user","timestamp":"2026-01-01T00:00:00.000Z","toolDenialKind":"permission-rule","#,
            r#""message":{"content":[{"type":"tool_result","is_error":true,"content":"この操作は許可されていません"}]}}"#,
        );
        let errors = extract_tool_errors(jsonl);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].category, ErrorCategory::BlockedByHook);
    }

    #[test]
    fn a_marker_is_not_spread_across_several_failures_in_one_entry() {
        // The marker names no block, so with more than one failure it cannot say
        // which one it denied — the text decides for all of them.
        let jsonl = concat!(
            r#"{"type":"user","timestamp":"2026-01-01T00:00:00.000Z","toolDenialKind":"permission-rule","message":{"content":["#,
            r#"{"type":"tool_result","is_error":true,"content":"この操作は許可されていません"},"#,
            r#"{"type":"tool_result","is_error":true,"content":"File does not exist: /tmp/example/foo.rs"}]}}"#,
        );
        let errors = extract_tool_errors(jsonl);
        assert_eq!(errors.len(), 2);
        assert_eq!(errors[0].category, ErrorCategory::Other);
        assert_eq!(errors[1].category, ErrorCategory::PathNotFound);
    }

    #[test]
    fn a_user_rejection_is_cancelled_not_a_blocked_call() {
        let jsonl = concat!(
            r#"{"type":"user","timestamp":"2026-01-01T00:00:00.000Z","toolDenialKind":"user-rejected","#,
            r#""message":{"content":[{"type":"tool_result","is_error":true,"content":"やめておいて"}]}}"#,
        );
        let errors = extract_tool_errors(jsonl);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].category, ErrorCategory::Cancelled);
    }

    #[test]
    fn an_unknown_denial_kind_falls_back_to_the_error_text() {
        // Upstream may add kinds; an unrecognised one must not swallow what the
        // text already tells us.
        let jsonl = concat!(
            r#"{"type":"user","timestamp":"2026-01-01T00:00:00.000Z","toolDenialKind":"some-future-kind","#,
            r#""message":{"content":[{"type":"tool_result","is_error":true,"content":"File does not exist: /tmp/example/foo.rs"}]}}"#,
        );
        let errors = extract_tool_errors(jsonl);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].category, ErrorCategory::PathNotFound);
    }

    #[test]
    fn permission_denials_are_counted_by_marker_and_by_text() {
        let jsonl = concat!(
            // Marked, and in a language no keyword list reaches.
            r#"{"type":"user","timestamp":"2026-01-01T00:00:00.000Z","toolDenialKind":"permission-rule","message":{"content":[{"type":"tool_result","is_error":true,"content":"この操作は許可されていません"}]}}"#,
            "\n",
            // Marked and phrased in English — counted once, not twice.
            r#"{"type":"user","timestamp":"2026-01-01T00:00:01.000Z","toolDenialKind":"permission-rule","message":{"content":[{"type":"tool_result","is_error":true,"content":"Permission for this action was denied"}]}}"#,
            "\n",
            // A user saying no is not a rule the user could relax.
            r#"{"type":"user","timestamp":"2026-01-01T00:00:02.000Z","toolDenialKind":"user-rejected","message":{"content":[{"type":"tool_result","is_error":true,"content":"やめておいて"}]}}"#,
            "\n",
            // Unmarked (older transcript): the text heuristic still finds it.
            r#"{"type":"user","timestamp":"2026-01-01T00:00:03.000Z","message":{"content":[{"type":"tool_result","content":"Permission for this action was denied by the user"}]}}"#,
            "\n",
            // A kind from a future release: the text still gets its say.
            r#"{"type":"user","timestamp":"2026-01-01T00:00:04.000Z","toolDenialKind":"some-future-kind","message":{"content":[{"type":"tool_result","is_error":true,"content":"Permission for this action was denied"}]}}"#,
        );
        assert_eq!(count_permission_denials(jsonl), 4);
    }

    #[test]
    fn an_agent_spawn_carries_the_call_id_that_names_it() {
        let jsonl = r#"{"type":"assistant","timestamp":"2026-01-01T00:00:00.000Z","promptId":"p1","message":{"content":[{"type":"tool_use","id":"toolu_1","name":"Agent","input":{"subagent_type":"Explore"}}]}}"#;
        let records = parse_session(jsonl);
        let RecordKind::AgentSpawn {
            agent,
            tool_use_id,
            prompt_id,
        } = &records[0].kind
        else {
            panic!("expected an agent spawn, got {:?}", records[0].kind);
        };
        assert_eq!(agent, "Explore");
        assert_eq!(tool_use_id.as_deref(), Some("toolu_1"));
        assert_eq!(prompt_id.as_deref(), Some("p1"));
    }

    #[test]
    fn a_sidecar_names_the_agent_type_and_the_call_that_spawned_it() {
        let json = r#"{"agentType":"Explore","description":"look around","toolUseId":"toolu_1","spawnDepth":2,"model":"sonnet","parentAgentId":"a1","newField":"ignored"}"#;
        assert_eq!(
            parse_subagent_sidecar(json),
            Some(SubagentSidecar {
                agent_type: Some("Explore".into()),
                tool_use_id: Some("toolu_1".into()),
                parent_agent_id: Some("a1".into()),
                spawn_depth: Some(2),
                model: Some("sonnet".into()),
            })
        );
    }

    #[test]
    fn a_sidecar_missing_every_known_field_still_parses() {
        // Forward compatibility: a renamed or dropped upstream field must not
        // fail the whole run, only leave that fact unknown.
        assert_eq!(
            parse_subagent_sidecar("{}"),
            Some(SubagentSidecar::default())
        );
        assert_eq!(parse_subagent_sidecar("not json"), None);
    }

    #[test]
    fn a_subagent_id_comes_from_its_file_name() {
        assert_eq!(subagent_id_from_file_name("agent-a1b2c3"), "a1b2c3");
    }

    #[test]
    fn subagent_files_are_laid_out_beside_the_main_transcript() {
        let transcript = Path::new("/tmp/example/projects/slug/session.jsonl");
        let dir = subagents_dir(transcript);
        assert_eq!(
            dir,
            Path::new("/tmp/example/projects/slug/session/subagents")
        );
        assert_eq!(
            subagent_sidecar_path(&dir.join("agent-a1b2c3.jsonl")),
            dir.join("agent-a1b2c3.meta.json")
        );
    }

    #[test]
    fn tool_error_excerpt_joins_text_blocks_and_collapses_whitespace() {
        // Array content (text blocks) with noisy whitespace — the excerpt is the
        // joined, single-spaced text.
        let jsonl = r#"{"type":"user","timestamp":"2026-01-01T00:00:00.000Z","message":{"content":[{"type":"tool_result","is_error":true,"content":[{"type":"text","text":"String to replace not found\n   in   /tmp/example/bar.rs"}]}]}}"#;
        let errors = extract_tool_errors(jsonl);
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].category, ErrorCategory::EditPrecondition);
        assert_eq!(
            errors[0].excerpt,
            "String to replace not found in /tmp/example/bar.rs"
        );
    }
}
