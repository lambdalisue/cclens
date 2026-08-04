//! Span extraction over the domain record stream: finding where each skill
//! invocation's work begins and ends, and rolling its records up into a `Span`
//! with cost metrics. The adapter produces the records from the transcript; the
//! core never sees raw JSON. See `docs/specs/events.md`.

use crate::core::metrics::{ctx_growth, duration_sec, representative_model};

/// How a skill invocation entered the transcript.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// The human typed the slash command.
    Slash,
    /// The model invoked the skill via the Skill tool.
    Tool,
}

/// A main-thread record, classified to what span extraction needs. More variants
/// and fields are added as later contracts need them.
#[derive(Debug, Clone)]
pub enum RecordKind {
    /// A real user turn — it delimits span ends.
    HumanTurn,
    /// A skill invocation: a span start.
    SkillInvocation { skill: String, source: Source },
    /// An assistant request, carrying the costs a span accumulates.
    Assistant {
        /// `input + cache_read + cache_creation` — the full prompt size.
        prompt_size: u64,
        out_tokens: u64,
        /// The model, or the `<synthetic>` sentinel.
        model: String,
    },
    /// A subagent spawn (the `Agent` tool) — usage of an `agent` surface. The
    /// `prompt_id` is the spawning turn's id, the join key to the subagent's
    /// transcript for cost attribution (`docs/specs/events.md`).
    AgentSpawn {
        agent: String,
        prompt_id: Option<String>,
    },
    /// A tool invocation by name — used to detect MCP tool usage.
    ToolUse { tool: String },
    /// Any other record.
    Other,
}

/// One classified record in main-thread order.
#[derive(Debug, Clone)]
pub struct Record {
    pub timestamp_ms: i64,
    pub kind: RecordKind,
}

/// A single extracted skill execution with its rolled-up cost metrics.
#[derive(Debug, Clone, PartialEq)]
pub struct Span {
    pub skill: String,
    pub source: Source,
    pub started_epoch_ms: i64,
    pub duration_sec: f64,
    pub out_tokens: u64,
    pub ctx_growth: u64,
    pub ctx_start: u64,
    pub ctx_peak: u64,
    pub model: Option<String>,
    /// True when the span closed only at the end of the session (no human turn,
    /// sibling skill, or idle gap followed) — its `duration_sec` is a lower
    /// bound (`docs/specs/events.md`).
    pub is_trailing: bool,
    /// Prompt ids of the subagents this span spawned — the join key for
    /// attribution; not persisted.
    pub agent_prompt_ids: Vec<String>,
    /// Subagent cost attributed to this span (filled by `attribute_subagents`).
    pub sub_tokens: u64,
    pub sub_agent_count: u32,
    /// True when any attributed subagent was equally split across competing
    /// spans, so `sub_tokens` is an estimate (`docs/specs/events.md`).
    pub sub_tokens_estimated: bool,
}

/// The index (exclusive) at which the span starting at `start` ends.
///
/// Default idle gap separating "the skill is still working" from "the user
/// walked away" — the value the shell injects when it has no override. Pure
/// functions take the gap as a parameter (see `.claude/rules/tdd.md`); this
/// constant is just the shell's default.
pub const DEFAULT_IDLE_GAP_MS: i64 = 30 * 60 * 1000;

/// The index (exclusive) at which the span starting at `start` ends.
///
/// Closes at the earliest of: the next human turn, the next skill invocation, or
/// a record that follows an idle gap longer than `idle_gap_ms` from the previous
/// record — else the end of the session. The next-skill rule keeps a span from
/// swallowing later, unrelated work; the idle gap closes a span the user walked
/// away from. Meta-skill nesting (a child invocation should not close its
/// parent) is added in a later contract. See `docs/specs/events.md`.
pub fn span_end(records: &[Record], start: usize, idle_gap_ms: i64) -> usize {
    for index in (start + 1)..records.len() {
        let record = &records[index];
        let closes = matches!(
            record.kind,
            RecordKind::HumanTurn | RecordKind::SkillInvocation { .. }
        ) || record.timestamp_ms - records[index - 1].timestamp_ms > idle_gap_ms;
        if closes {
            return index;
        }
    }
    records.len()
}

/// When a session began and the context it began with — one always-on floor
/// observation. The timestamp travels with the size because an observation
/// nobody can place in time cannot be audited against the session it came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionStart {
    pub timestamp_ms: i64,
    /// The prompt size of the session's first assistant request, before any work
    /// grew it.
    pub ctx: u64,
}

/// The session's start: its first assistant request.
///
/// This is the only honest observation of the always-on floor (system prompt +
/// tool/MCP schemas + always-on config). A skill span's `ctx_start` is *not* a
/// substitute: it is the prompt size wherever that skill happened to run, which
/// in a long or resumed session is far above the session's own start.
///
/// `None` when the transcript carries no assistant record — the session then
/// contributes no floor observation rather than a fabricated one.
pub fn session_start(records: &[Record]) -> Option<SessionStart> {
    records.iter().find_map(|record| match record.kind {
        RecordKind::Assistant { prompt_size, .. } => Some(SessionStart {
            timestamp_ms: record.timestamp_ms,
            ctx: prompt_size,
        }),
        _ => None,
    })
}

/// Extract one `Span` per skill invocation in `records`, in order.
///
/// Each span runs from its invocation to `span_end` (using `idle_gap_ms`); its
/// cost metrics are rolled up from the assistant records inside that window.
/// Nesting and subagent attribution are not yet applied (see
/// `docs/specs/events.md`).
pub fn extract_spans(records: &[Record], idle_gap_ms: i64) -> Vec<Span> {
    records
        .iter()
        .enumerate()
        .filter_map(|(start, record)| match &record.kind {
            RecordKind::SkillInvocation { skill, source } => {
                Some(roll_up(records, start, skill.clone(), *source, idle_gap_ms))
            }
            _ => None,
        })
        .collect()
}

fn roll_up(
    records: &[Record],
    start: usize,
    skill: String,
    source: Source,
    idle_gap_ms: i64,
) -> Span {
    let end = span_end(records, start, idle_gap_ms);
    // Nothing closed it but the session ending — its duration is a lower bound.
    let is_trailing = end == records.len();
    let window = &records[start..end];

    let timestamps: Vec<i64> = window.iter().map(|record| record.timestamp_ms).collect();

    let mut prompt_sizes = Vec::new();
    let mut out_tokens = 0;
    let mut models = Vec::new();
    let mut agent_prompt_ids = Vec::new();
    for record in window {
        match &record.kind {
            RecordKind::Assistant {
                prompt_size,
                out_tokens: out,
                model,
            } => {
                prompt_sizes.push(*prompt_size);
                out_tokens += out;
                models.push(model.as_str());
            }
            RecordKind::AgentSpawn {
                prompt_id: Some(prompt_id),
                ..
            } => agent_prompt_ids.push(prompt_id.clone()),
            _ => {}
        }
    }

    Span {
        skill,
        source,
        started_epoch_ms: records[start].timestamp_ms,
        duration_sec: duration_sec(&timestamps),
        out_tokens,
        ctx_growth: ctx_growth(&prompt_sizes),
        ctx_start: prompt_sizes.first().copied().unwrap_or(0),
        ctx_peak: prompt_sizes.iter().copied().max().unwrap_or(0),
        model: representative_model(&models).map(String::from),
        is_trailing,
        agent_prompt_ids,
        sub_tokens: 0,
        sub_agent_count: 0,
        sub_tokens_estimated: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(timestamp_ms: i64, kind: RecordKind) -> Record {
        Record { timestamp_ms, kind }
    }

    #[test]
    fn session_start_ctx_is_the_first_assistant_prompt_size() {
        let records = [
            at(0, RecordKind::HumanTurn),
            at(1, assistant(52_000, 10, "opus")),
            at(2, assistant(90_000, 10, "opus")),
        ];
        assert_eq!(
            session_start(&records),
            Some(SessionStart {
                timestamp_ms: 1,
                ctx: 52_000
            })
        );
    }

    #[test]
    fn session_start_carries_when_the_session_began() {
        // The observation is worthless without its timestamp: an event stamped
        // at the epoch would place every session start in 1970.
        let records = [at(1_700_000_000_000, assistant(52_000, 10, "opus"))];
        assert_eq!(
            session_start(&records).unwrap().timestamp_ms,
            1_700_000_000_000
        );
    }

    #[test]
    fn session_start_ctx_ignores_a_skill_that_runs_later_in_the_session() {
        // The old floor took the first prompt size *inside a skill span*, so a
        // session that only invoked a skill after a long conversation reported
        // that late, inflated size as its start.
        let records = [
            at(0, assistant(52_000, 10, "opus")),
            at(1, assistant(300_000, 10, "opus")),
            at(2, skill("git-commit")),
            at(3, assistant(310_000, 10, "opus")),
        ];
        assert_eq!(session_start(&records).map(|s| s.ctx), Some(52_000));
    }

    #[test]
    fn session_start_ctx_is_absent_without_an_assistant_record() {
        let records = [at(0, RecordKind::HumanTurn)];
        assert_eq!(session_start(&records), None);
    }

    fn skill(name: &str) -> RecordKind {
        RecordKind::SkillInvocation {
            skill: name.to_string(),
            source: Source::Slash,
        }
    }

    fn assistant(prompt_size: u64, out_tokens: u64, model: &str) -> RecordKind {
        RecordKind::Assistant {
            prompt_size,
            out_tokens,
            model: model.to_string(),
        }
    }

    #[test]
    fn closes_at_session_end_when_no_human_turn_follows() {
        let records = [
            at(0, skill("git-commit")),
            at(1, RecordKind::Other),
            at(2, RecordKind::Other),
        ];
        assert_eq!(span_end(&records, 0, DEFAULT_IDLE_GAP_MS), 3);
    }

    #[test]
    fn closes_at_the_next_human_turn() {
        let records = [
            at(0, skill("git-commit")),
            at(1, RecordKind::Other),
            at(2, RecordKind::HumanTurn),
            at(3, RecordKind::Other),
        ];
        assert_eq!(span_end(&records, 0, DEFAULT_IDLE_GAP_MS), 2);
    }

    #[test]
    fn closes_at_the_next_skill_invocation() {
        let records = [
            at(0, skill("git-commit")),
            at(1, RecordKind::Other),
            at(2, skill("pr-create")), // a sibling invocation closes the first span
            at(3, RecordKind::Other),
        ];
        assert_eq!(span_end(&records, 0, DEFAULT_IDLE_GAP_MS), 2);
    }

    #[test]
    fn a_human_turn_before_start_does_not_close_the_span() {
        let records = [
            at(0, RecordKind::HumanTurn),
            at(1, skill("git-commit")),
            at(2, RecordKind::Other),
        ];
        assert_eq!(span_end(&records, 1, DEFAULT_IDLE_GAP_MS), 3);
    }

    #[test]
    fn closes_after_an_idle_gap_longer_than_the_threshold() {
        // A 10-minute gap with a 5-minute threshold closes the span; the
        // post-gap record is the boundary, not part of the span.
        let five_min = 5 * 60 * 1000;
        let records = [
            at(0, skill("git-commit")),
            at(1_000, RecordKind::Other),
            at(1_000 + 10 * 60 * 1000, RecordKind::Other), // 10 min later
        ];
        assert_eq!(span_end(&records, 0, five_min), 2);
    }

    #[test]
    fn a_gap_within_the_threshold_does_not_close_the_span() {
        let five_min = 5 * 60 * 1000;
        let records = [
            at(0, skill("git-commit")),
            at(1_000, RecordKind::Other),
            at(1_000 + 60 * 1000, RecordKind::Other), // 1 min later
        ];
        assert_eq!(span_end(&records, 0, five_min), 3);
    }

    #[test]
    fn a_span_with_no_closer_is_trailing() {
        let records = [at(0, skill("loop")), at(1, assistant(10, 5, "m"))];
        let span = &extract_spans(&records, DEFAULT_IDLE_GAP_MS)[0];
        assert!(span.is_trailing);
    }

    #[test]
    fn a_span_closed_by_a_human_turn_is_not_trailing() {
        let records = [
            at(0, skill("git-commit")),
            at(1, assistant(10, 5, "m")),
            at(2, RecordKind::HumanTurn),
        ];
        let span = &extract_spans(&records, DEFAULT_IDLE_GAP_MS)[0];
        assert!(!span.is_trailing);
    }

    #[test]
    fn no_invocations_yields_no_spans() {
        let records = [at(0, RecordKind::HumanTurn), at(1, RecordKind::Other)];
        assert!(extract_spans(&records, DEFAULT_IDLE_GAP_MS).is_empty());
    }

    #[test]
    fn rolls_up_cost_from_assistant_records_in_the_window() {
        let records = [
            at(1000, skill("git-commit")),
            at(2000, assistant(100, 30, "claude-opus-4-7")),
            at(5000, assistant(250, 70, "claude-opus-4-7")),
            at(6000, RecordKind::HumanTurn), // closes the span
            at(7000, assistant(999, 999, "claude-opus-4-7")), // outside; excluded
        ];

        let spans = extract_spans(&records, DEFAULT_IDLE_GAP_MS);

        assert_eq!(spans.len(), 1);
        let span = &spans[0];
        assert_eq!(span.skill, "git-commit");
        assert_eq!(span.started_epoch_ms, 1000);
        // The closing human turn (6000) is the boundary, not inside the span;
        // the last in-window record is the assistant at 5000. 5000 - 1000 = 4.0s.
        assert_eq!(span.duration_sec, 4.0);
        assert_eq!(span.out_tokens, 100); // 30 + 70, excludes the post-span 999
        assert_eq!(span.ctx_growth, 150); // (250 - 100), positive step only
        assert_eq!(span.ctx_start, 100);
        assert_eq!(span.ctx_peak, 250);
        assert_eq!(span.model.as_deref(), Some("claude-opus-4-7"));
    }

    #[test]
    fn excludes_synthetic_model_and_handles_no_assistant_records() {
        let records = [
            at(1000, skill("loop")),
            at(2000, assistant(50, 10, "<synthetic>")),
        ];

        let span = &extract_spans(&records, DEFAULT_IDLE_GAP_MS)[0];
        // Only a synthetic assistant record -> no representative model.
        assert_eq!(span.model, None);
        assert_eq!(span.out_tokens, 10);
    }
}
