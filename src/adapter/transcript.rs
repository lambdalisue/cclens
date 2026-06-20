//! Parse a Claude Code session transcript (JSONL) into domain records.
//!
//! `parse_session` is a pure function over the file's text — file walking and
//! reading are a thin shell around it (`docs/specs/session-format.md`). It
//! deserializes defensively: only the needed fields, unknown fields ignored, a
//! line that fails to parse or lacks a timestamp simply yields no records.

use serde::Deserialize;
use serde_json::Value;

use crate::core::friction::{ErrorCategory, classify_error};
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

/// Extract failed tool results from a transcript as `(epoch_ms, category)` — the
/// raw material for friction analysis. A tool result is a failure when it is
/// flagged `is_error` or carries a `tool_use_error` wrapper; its text is
/// classified into a recurring category (`core::friction`).
pub fn extract_tool_errors(jsonl: &str) -> Vec<(i64, ErrorCategory)> {
    let mut errors = Vec::new();
    for line in jsonl.lines() {
        let Ok(raw) = serde_json::from_str::<Raw>(line) else {
            continue;
        };
        if raw.kind.as_deref() != Some("user") {
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
            if block.get("type").and_then(|v| v.as_str()) != Some("tool_result") {
                continue;
            }
            let content = block
                .get("content")
                .map(|v| v.to_string())
                .unwrap_or_default();
            let is_error = block.get("is_error").and_then(|v| v.as_bool()) == Some(true)
                || content.contains("tool_use_error");
            if is_error {
                errors.push((ts, classify_error(&content)));
            }
        }
    }
    errors
}

/// Count permission denials in a transcript — a friction signal. There is no
/// structured record for these (`docs/specs/session-format.md`); they appear as
/// denial text inside a tool-result, so this is a lower-confidence heuristic:
/// the marker phrase within a `tool_result` line.
pub fn count_permission_denials(jsonl: &str) -> usize {
    const MARKER: &str = "Permission for this action was denied";
    jsonl
        .lines()
        .filter(|line| line.contains("tool_result") && line.contains(MARKER))
        .count()
}

/// The `promptId` a subagent transcript was spawned under — the join key back to
/// the spawning span. Read from the first record that carries one.
pub fn subagent_prompt_id(jsonl: &str) -> Option<String> {
    jsonl.lines().find_map(|line| {
        serde_json::from_str::<Raw>(line)
            .ok()
            .and_then(|raw| raw.prompt_id)
    })
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
            // Agent record itself does not carry it.
            Some(RecordKind::AgentSpawn {
                agent,
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
}
