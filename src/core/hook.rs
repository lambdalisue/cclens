//! Hooks: identity, trigger counting, trace resolution, and the per-hook
//! rollup. A hook's own traces cannot count its fires — a silent pass leaves
//! none — so uses come from its trigger and traces only say what it did
//! (`docs/specs/surfaces.md`, `docs/specs/events.md` "Hook runs").

/// What one hook trace records the hook doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookOutcome {
    /// It refused the call (or prevented the turn from continuing).
    Blocked,
    /// The hook itself failed without blocking.
    Error,
    /// A run recorded as successful.
    Ok,
    /// Context it injected into the conversation.
    Context,
}

impl HookOutcome {
    pub fn label(self) -> &'static str {
        match self {
            HookOutcome::Blocked => "blocked",
            HookOutcome::Error => "error",
            HookOutcome::Ok => "ok",
            HookOutcome::Context => "context",
        }
    }

    pub fn from_label(label: &str) -> Option<HookOutcome> {
        match label {
            "blocked" => Some(HookOutcome::Blocked),
            "error" => Some(HookOutcome::Error),
            "ok" => Some(HookOutcome::Ok),
            "context" => Some(HookOutcome::Context),
            _ => None,
        }
    }
}

/// One trace a hook left in a transcript.
#[derive(Debug, Clone, PartialEq)]
pub struct HookTrace {
    pub epoch_ms: i64,
    pub event: String,
    /// What the matcher was tested against — the tool name for a tool event.
    pub subject: Option<String>,
    /// The command as recorded; `None` when the transcript names no command.
    pub command: Option<String>,
    pub outcome: HookOutcome,
    pub exit_code: Option<i64>,
    pub duration_ms: Option<i64>,
    pub context_tokens: u64,
    /// The tool call the hook ran for, when it ran for one.
    pub tool_use_id: Option<String>,
}

/// One step of a session in transcript order — what the retry search walks.
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    Call {
        epoch_ms: i64,
        id: String,
        tool: String,
        result: CallResult,
    },
    HumanTurn,
}

/// What became of a tool call. `Pending` — no result recorded yet (a session
/// still running, or one cut off) — is neither, and is never read as success.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallResult {
    Pending,
    Succeeded,
    /// It ran and failed.
    Failed,
    /// It was refused before it ran.
    Denied,
}

impl CallResult {
    pub fn label(self) -> &'static str {
        match self {
            CallResult::Pending => "pending",
            CallResult::Succeeded => "ok",
            CallResult::Failed => "failed",
            CallResult::Denied => "denied",
        }
    }

    pub fn from_label(label: &str) -> Option<CallResult> {
        match label {
            "pending" => Some(CallResult::Pending),
            "ok" => Some(CallResult::Succeeded),
            "failed" => Some(CallResult::Failed),
            "denied" => Some(CallResult::Denied),
            _ => None,
        }
    }
}

/// A configured hook: the event it listens on, its matcher (`None` = every
/// occurrence), and its command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookDef {
    pub event: String,
    pub matcher: Option<String>,
    pub command: String,
}

impl HookDef {
    /// The catalog id: `<Event>[<matcher>] <command>`, `*` for no matcher.
    pub fn id(&self) -> String {
        let matcher = self
            .matcher
            .as_deref()
            .filter(|m| !m.is_empty())
            .unwrap_or("*");
        format!("{}[{}] {}", self.event, matcher, self.command)
    }

    /// Read a catalog id back. `None` for anything `id` did not produce.
    pub fn parse(id: &str) -> Option<HookDef> {
        let open = id.find('[')?;
        let close = open + id[open..].find("] ")?;
        let event = &id[..open];
        let matcher = &id[open + 1..close];
        let command = &id[close + 2..];
        if event.is_empty() || command.is_empty() {
            return None;
        }
        Some(HookDef {
            event: event.to_string(),
            matcher: (matcher != "*").then(|| matcher.to_string()),
            command: command.to_string(),
        })
    }

    /// Whether the hook has no matcher, or one that selects everything.
    fn is_unfiltered(&self) -> bool {
        matches!(self.matcher.as_deref(), None | Some("" | "*" | ".*"))
    }

    /// Whether this hook's matcher selects `subject`. `None` when the matcher
    /// is a regex too intricate to evaluate without guessing.
    pub fn selects(&self, subject: &str) -> Option<bool> {
        match self.matcher.as_deref() {
            _ if self.is_unfiltered() => Some(true),
            Some(matcher) => matcher_selects(matcher, subject),
            None => Some(true),
        }
    }
}

/// Evaluate a matcher the way Claude Code reads it, for the subset that has one
/// reading. A matcher of names and `|` only is an exact list. Any other
/// character makes it a regex, searched anywhere in the name; that is evaluated
/// only when every alternative is a literal, optionally followed by `.*` (which
/// then adds nothing). Anything else could only be approximated, and an
/// approximated "never matched" would be a false delete verdict.
fn matcher_selects(matcher: &str, subject: &str) -> Option<bool> {
    let is_name = |text: &str| {
        !text.is_empty()
            && text
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
    };
    let alternatives: Vec<&str> = matcher.split('|').collect();
    if alternatives.iter().all(|alternative| is_name(alternative)) {
        return Some(alternatives.contains(&subject));
    }
    let mut selected = false;
    for alternative in alternatives {
        let literal = alternative.strip_suffix(".*").unwrap_or(alternative);
        if literal.is_empty()
            || !literal
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
        {
            return None;
        }
        selected |= subject.contains(literal);
    }
    Some(selected)
}

/// What a hook's trigger can be counted against, within its scope — over the
/// sessions whose hook activity was extracted, and only those (`sessions`).
/// A session stored before that, whose transcript is gone, holds no calls; it
/// is left out rather than counted as a session where nothing fired.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Triggers {
    /// Tool calls per tool name, split by what became of them.
    pub tool_calls: Vec<ToolCalls>,
    pub prompts: u64,
    pub sessions: u64,
}

/// One tool's calls, split by result.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ToolCalls {
    pub tool: String,
    pub succeeded: u64,
    pub failed: u64,
    /// Refused before it ran — by a hook, a permission rule, or the user.
    pub denied: u64,
    pub pending: u64,
}

/// How many times `def` was triggered: the occurrences of its event its
/// matcher selects. A tool event counts the calls it fires on — every call
/// for `PreToolUse`, the successful ones for `PostToolUse`, the ones that ran
/// and failed for `PostToolUseFailure` (a call refused before it ran reaches no
/// post-execution hook). `None` when the event has no countable trigger
/// (`PermissionRequest` fires only on calls that raised a prompt, which no
/// call records) or the matcher cannot be evaluated.
pub fn trigger_count(def: &HookDef, triggers: &Triggers) -> Option<u64> {
    // No observed session: nothing is known, and zero would read as "never".
    if triggers.sessions == 0 {
        return None;
    }
    let fires_on: fn(&ToolCalls) -> u64 = match def.event.as_str() {
        "PreToolUse" => |calls| calls.succeeded + calls.failed + calls.denied + calls.pending,
        "PostToolUse" => |calls| calls.succeeded,
        "PostToolUseFailure" => |calls| calls.failed,
        _ => return other_trigger_count(def, triggers),
    };
    let mut count = 0;
    for calls in &triggers.tool_calls {
        if def.selects(&calls.tool)? {
            count += fires_on(calls);
        }
    }
    Some(count)
}

fn other_trigger_count(def: &HookDef, triggers: &Triggers) -> Option<u64> {
    match def.event.as_str() {
        "UserPromptSubmit" => Some(triggers.prompts),
        // SessionStart's matcher selects a source (startup, resume, clear…)
        // that no stored session records, so only an unfiltered hook counts.
        "SessionStart" if def.is_unfiltered() => Some(triggers.sessions),
        _ => None,
    }
}

/// The catalogued hook a trace belongs to, as an index into `defs`: same event,
/// a matcher that selects the trace's subject, and — when the trace names one
/// — the same command. It resolves only when exactly one hook qualifies: one
/// command wired under two matching matchers runs once (Claude Code dedupes
/// identical handlers), so its trace cannot say which entry it was.
pub fn resolve(trace: &HookTrace, defs: &[HookDef]) -> Resolution {
    let candidates: Vec<usize> = defs
        .iter()
        .enumerate()
        .filter(|(_, def)| def.event == trace.event)
        .filter(|(_, def)| match trace.subject.as_deref() {
            Some(subject) => def.selects(subject) != Some(false),
            None => true,
        })
        .filter(|(_, def)| {
            trace
                .command
                .as_deref()
                .is_none_or(|command| def.command == command)
        })
        .map(|(index, _)| index)
        .collect();
    match candidates.as_slice() {
        [] => Resolution::Orphan,
        [index] => Resolution::Hook(*index),
        _ => Resolution::Ambiguous,
    }
}

/// Where a trace lands in the catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    /// The catalogued hook at this index left it.
    Hook(usize),
    /// It names no command and several catalogued hooks could have left it.
    Ambiguous,
    /// No catalogued hook could have left it — config outside the settings.
    Orphan,
}

/// Traces no single catalogued hook claims, grouped by their
/// `<event>:<subject> <command>` key.
#[derive(Debug, Clone, PartialEq)]
pub struct Unattributed {
    pub key: String,
    /// Several catalogued hooks could have left these (an unnamed command), as
    /// opposed to none — the difference between "which of yours?" and "not yours".
    pub ambiguous: bool,
    pub usage: HookUsage,
}

/// What a block cost: the tool calls after the blocked one up to and including
/// the first later call of the same tool that succeeded. `None` when a human
/// turn or the end of the session comes first, or the next same-tool call has
/// no result yet (`docs/specs/events.md`). Only calls issued after the block
/// was seen at `blocked_at_ms` are retries: one issued alongside the blocked
/// call, in the same response, was already on its way.
pub fn retry_calls(steps: &[Step], blocked_call_id: &str, blocked_at_ms: i64) -> Option<u32> {
    let start = steps
        .iter()
        .position(|step| matches!(step, Step::Call { id, .. } if id == blocked_call_id))?;
    let Step::Call { tool: blocked, .. } = &steps[start] else {
        return None;
    };
    let mut calls = 0;
    for step in &steps[start + 1..] {
        match step {
            Step::HumanTurn => return None,
            Step::Call { epoch_ms, .. } if *epoch_ms <= blocked_at_ms => {}
            Step::Call { tool, result, .. } => {
                calls += 1;
                match result {
                    _ if tool != blocked => {}
                    CallResult::Succeeded => return Some(calls),
                    CallResult::Pending => return None,
                    CallResult::Failed | CallResult::Denied => {}
                }
            }
        }
    }
    None
}

/// One hook's usage and cost over the window.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct HookUsage {
    /// Occurrences its matcher selected; `None` when not countable.
    pub triggers: Option<u64>,
    pub traces: u64,
    pub blocks: u64,
    pub errors: u64,
    /// Runs that recorded a duration, and the durations' total / p50 / p95.
    pub measured_runs: u64,
    pub duration_total_ms: i64,
    pub duration_p50_ms: Option<i64>,
    pub duration_p95_ms: Option<i64>,
    pub context_injections: u64,
    pub context_tokens: u64,
    /// Blocks whose retry resolved, and the calls those retries took.
    pub resolved_blocks: u64,
    pub retry_calls: u64,
}

impl HookUsage {
    /// Blocks over triggers; `None` without a known, nonzero trigger.
    pub fn block_rate(&self) -> Option<f64> {
        self.triggers
            .filter(|&t| t > 0)
            .map(|t| self.blocks as f64 / t as f64)
    }

    /// Mean retry calls per resolved block.
    pub fn mean_retry_calls(&self) -> Option<f64> {
        (self.resolved_blocks > 0).then(|| self.retry_calls as f64 / self.resolved_blocks as f64)
    }

    /// Its trigger was counted and came to zero: the matcher never selected
    /// anything in the window, so the hook cannot have fired.
    pub fn never_matched(&self) -> bool {
        self.triggers == Some(0)
    }

    /// Mean tokens per context injection.
    pub fn mean_context_tokens(&self) -> Option<u64> {
        (self.context_injections > 0).then(|| self.context_tokens / self.context_injections)
    }
}

/// What an event's context injection recurs with — `prompt` or `session` —
/// when it is a standing cost, paid like always-on config rather than per use
/// (`docs/specs/events.md` "Hook runs").
pub fn standing_context(event: &str) -> Option<&'static str> {
    match event {
        "UserPromptSubmit" => Some("prompt"),
        "SessionStart" => Some("session"),
        _ => None,
    }
}

/// A trace as the store hands it back: the trace plus its block cost.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredTrace {
    pub trace: HookTrace,
    /// The owning session's project, for scope-aware resolution.
    pub project: String,
    pub retry_calls: Option<u32>,
}

/// One catalogued hook as the rollup needs it: its definition, the project it
/// is installed in (`None` = global), and its trigger count within that scope.
#[derive(Debug, Clone, PartialEq)]
pub struct CatalogHook {
    pub def: HookDef,
    pub project: Option<String>,
    pub triggers: Option<u64>,
}

/// Roll up traces onto their hooks, one usage per entry of `hooks`. A trace is
/// attributed with `resolve` among the hooks installed where its session ran —
/// every global hook, plus that project's own. Hooks are not shadowed: global
/// and project hooks all run (`docs/specs/surfaces.md`). The traces no hook
/// claims come back as `Unattributed`.
pub fn rollup(
    hooks: &[CatalogHook],
    traces: &[&StoredTrace],
) -> (Vec<HookUsage>, Vec<Unattributed>) {
    let mut durations: Vec<Vec<i64>> = vec![Vec::new(); hooks.len()];
    let mut usages: Vec<HookUsage> = hooks
        .iter()
        .map(|hook| HookUsage {
            triggers: hook.triggers,
            ..HookUsage::default()
        })
        .collect();
    let mut orphans: Vec<(Unattributed, Vec<i64>)> = Vec::new();
    for stored in traces {
        // Only hooks installed where the session ran can have produced it.
        let visible: Vec<usize> = (0..hooks.len())
            .filter(|&i| {
                hooks[i]
                    .project
                    .as_deref()
                    .is_none_or(|project| project == stored.project)
            })
            .collect();
        let defs: Vec<HookDef> = visible.iter().map(|&i| hooks[i].def.clone()).collect();
        let resolution = resolve(&stored.trace, &defs);
        let (usage, measured) = match resolution {
            Resolution::Hook(at) => (&mut usages[visible[at]], &mut durations[visible[at]]),
            Resolution::Ambiguous | Resolution::Orphan => {
                let key = orphan_key(&stored.trace);
                let ambiguous = resolution == Resolution::Ambiguous;
                let at = match orphans
                    .iter()
                    .position(|(o, _)| o.key == key && o.ambiguous == ambiguous)
                {
                    Some(at) => at,
                    None => {
                        orphans.push((
                            Unattributed {
                                key,
                                ambiguous,
                                usage: HookUsage::default(),
                            },
                            Vec::new(),
                        ));
                        orphans.len() - 1
                    }
                };
                let (orphan, measured) = &mut orphans[at];
                (&mut orphan.usage, measured)
            }
        };
        add_trace(usage, measured, stored);
    }
    for (usage, measured) in usages.iter_mut().zip(&mut durations) {
        settle_durations(usage, measured);
    }
    let orphans = orphans
        .into_iter()
        .map(|(mut orphan, mut measured)| {
            settle_durations(&mut orphan.usage, &mut measured);
            orphan
        })
        .collect();
    (usages, orphans)
}

fn orphan_key(trace: &HookTrace) -> String {
    let name = match trace.subject.as_deref() {
        Some(subject) => format!("{}:{subject}", trace.event),
        None => trace.event.clone(),
    };
    format!("{name} {}", trace.command.as_deref().unwrap_or("(unnamed)"))
}

fn add_trace(usage: &mut HookUsage, measured: &mut Vec<i64>, stored: &StoredTrace) {
    let trace = &stored.trace;
    usage.traces += 1;
    match trace.outcome {
        HookOutcome::Blocked => {
            usage.blocks += 1;
            if let Some(calls) = stored.retry_calls {
                usage.resolved_blocks += 1;
                usage.retry_calls += u64::from(calls);
            }
        }
        HookOutcome::Error => usage.errors += 1,
        HookOutcome::Context => usage.context_injections += 1,
        HookOutcome::Ok => {}
    }
    usage.context_tokens += trace.context_tokens;
    if let Some(ms) = trace.duration_ms {
        measured.push(ms);
    }
}

fn settle_durations(usage: &mut HookUsage, measured: &mut [i64]) {
    measured.sort_unstable();
    usage.measured_runs = measured.len() as u64;
    usage.duration_total_ms = measured.iter().sum();
    usage.duration_p50_ms = percentile(measured, 50);
    usage.duration_p95_ms = percentile(measured, 95);
}

/// Nearest-rank percentile of sorted values; `None` when empty.
fn percentile(sorted: &[i64], pct: usize) -> Option<i64> {
    if sorted.is_empty() {
        return None;
    }
    let rank = (pct * sorted.len()).div_ceil(100).max(1);
    sorted.get(rank - 1).copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def(event: &str, matcher: Option<&str>, command: &str) -> HookDef {
        HookDef {
            event: event.into(),
            matcher: matcher.map(String::from),
            command: command.into(),
        }
    }

    fn trace(event: &str, subject: Option<&str>, command: Option<&str>) -> HookTrace {
        HookTrace {
            epoch_ms: 0,
            event: event.into(),
            subject: subject.map(String::from),
            command: command.map(String::from),
            outcome: HookOutcome::Ok,
            exit_code: None,
            duration_ms: None,
            context_tokens: 0,
            tool_use_id: None,
        }
    }

    /// A call issued at the second its id numbers (`t3` → 3s); its block, if
    /// any, is seen half a second later (`seen`).
    fn call(id: &str, tool: &str, failed: bool) -> Step {
        let result = if failed {
            CallResult::Failed
        } else {
            CallResult::Succeeded
        };
        Step::Call {
            epoch_ms: issued(id),
            id: id.into(),
            tool: tool.into(),
            result,
        }
    }

    fn issued(id: &str) -> i64 {
        id.trim_start_matches('t').parse::<i64>().unwrap_or(0) * 1000
    }

    fn seen(id: &str) -> i64 {
        issued(id) + 500
    }

    fn calls(tool: &str, succeeded: u64) -> ToolCalls {
        ToolCalls {
            tool: tool.into(),
            succeeded,
            ..ToolCalls::default()
        }
    }

    #[test]
    fn a_hook_id_round_trips() {
        let hook = def("PreToolUse", Some("Edit|Write"), "/tmp/example/guard.sh");
        assert_eq!(hook.id(), "PreToolUse[Edit|Write] /tmp/example/guard.sh");
        assert_eq!(HookDef::parse(&hook.id()), Some(hook));
    }

    #[test]
    fn a_hook_without_a_matcher_is_written_with_a_star() {
        let hook = def("Stop", None, "notify");
        assert_eq!(hook.id(), "Stop[*] notify");
        assert_eq!(HookDef::parse("Stop[*] notify"), Some(hook));
    }

    #[test]
    fn a_command_with_spaces_survives_the_round_trip() {
        let hook = def("PreToolUse", Some("Bash"), "sh -c 'echo [x] done'");
        assert_eq!(HookDef::parse(&hook.id()), Some(hook));
    }

    #[test]
    fn a_plain_matcher_is_an_exact_list_of_names() {
        let hook = def("PreToolUse", Some("Edit|Write"), "x");
        assert_eq!(hook.selects("Edit"), Some(true));
        assert_eq!(hook.selects("Write"), Some(true));
        assert_eq!(hook.selects("Read"), Some(false));
        // Exact, not substring: `Edit` does not select `NotebookEdit`.
        assert_eq!(hook.selects("NotebookEdit"), Some(false));
    }

    #[test]
    fn a_regex_matcher_is_unanchored() {
        // Any regex character makes the whole matcher a regex, searched
        // anywhere in the name — so `Edit` now selects `NotebookEdit` too.
        let hook = def("PreToolUse", Some("Edit|mcp__fs.*"), "x");
        assert_eq!(hook.selects("mcp__fs__read"), Some(true));
        assert_eq!(hook.selects("NotebookEdit"), Some(true));
        assert_eq!(hook.selects("Write"), Some(false));
        let suffix = def("PreToolUse", Some("Edit.*"), "x");
        assert_eq!(suffix.selects("NotebookEdit"), Some(true));
    }

    #[test]
    fn an_intricate_regex_is_not_guessed() {
        let hook = def("PreToolUse", Some("^(Edit|Write)$"), "x");
        assert_eq!(hook.selects("Edit"), None);
    }

    #[test]
    fn a_tool_hook_is_triggered_by_the_calls_its_matcher_selects() {
        let triggers = Triggers {
            tool_calls: vec![calls("Bash", 40), calls("Edit", 7), calls("Read", 90)],
            sessions: 1,
            ..Triggers::default()
        };
        let hook = def("PreToolUse", Some("Bash|Edit"), "x");
        assert_eq!(trigger_count(&hook, &triggers), Some(47));
    }

    #[test]
    fn a_matcher_that_selects_no_call_has_zero_triggers() {
        let triggers = Triggers {
            tool_calls: vec![calls("Bash", 40)],
            sessions: 1,
            ..Triggers::default()
        };
        let hook = def("PreToolUse", Some("Task"), "x");
        assert_eq!(trigger_count(&hook, &triggers), Some(0));
    }

    #[test]
    fn prompt_and_session_hooks_count_prompts_and_sessions() {
        let triggers = Triggers {
            prompts: 12,
            sessions: 3,
            ..Triggers::default()
        };
        assert_eq!(
            trigger_count(&def("UserPromptSubmit", None, "x"), &triggers),
            Some(12)
        );
        assert_eq!(
            trigger_count(&def("SessionStart", None, "x"), &triggers),
            Some(3)
        );
    }

    #[test]
    fn an_event_without_a_countable_trigger_is_unknown() {
        let triggers = Triggers {
            sessions: 1,
            ..Triggers::default()
        };
        assert_eq!(trigger_count(&def("Stop", None, "x"), &triggers), None);
        // A source-filtered SessionStart hook: no session records its source.
        assert_eq!(
            trigger_count(&def("SessionStart", Some("clear"), "x"), &triggers),
            None
        );
    }

    #[test]
    fn with_no_observed_session_every_trigger_is_unknown() {
        let hook = def("PreToolUse", Some("Bash"), "x");
        assert_eq!(trigger_count(&hook, &Triggers::default()), None);
    }

    #[test]
    fn an_unevaluable_matcher_leaves_the_trigger_unknown() {
        let triggers = Triggers {
            tool_calls: vec![calls("Bash", 4)],
            sessions: 1,
            ..Triggers::default()
        };
        let hook = def("PreToolUse", Some("^Bash$"), "x");
        assert_eq!(trigger_count(&hook, &triggers), None);
    }

    #[test]
    fn a_trace_resolves_by_event_matcher_and_command() {
        let defs = [
            def("PreToolUse", Some("Bash"), "a.sh"),
            def("PreToolUse", Some("Bash"), "b.sh"),
            def("PostToolUse", Some("Bash"), "b.sh"),
        ];
        let found = resolve(&trace("PreToolUse", Some("Bash"), Some("b.sh")), &defs);
        assert_eq!(found, Resolution::Hook(1));
    }

    #[test]
    fn a_trace_whose_tool_the_matcher_rejects_does_not_resolve() {
        let defs = [def("PreToolUse", Some("Edit"), "a.sh")];
        assert_eq!(
            resolve(&trace("PreToolUse", Some("Bash"), Some("a.sh")), &defs),
            Resolution::Orphan
        );
    }

    #[test]
    fn an_unnamed_trace_resolves_only_to_a_sole_candidate() {
        let one = [def("PostToolUse", Some("Edit|Write"), "fmt")];
        assert_eq!(
            resolve(&trace("PostToolUse", Some("Write"), None), &one),
            Resolution::Hook(0)
        );
        let two = [
            def("PreToolUse", Some("Bash"), "a.sh"),
            def("PreToolUse", Some("Bash"), "b.sh"),
        ];
        assert_eq!(
            resolve(&trace("PreToolUse", Some("Bash"), None), &two),
            Resolution::Ambiguous
        );
    }

    #[test]
    fn a_block_costs_the_calls_until_the_same_tool_passes() {
        let steps = [
            call("t1", "Bash", true),
            call("t2", "Read", false),
            call("t3", "Bash", true),
            call("t4", "Bash", false),
        ];
        assert_eq!(retry_calls(&steps, "t1", seen("t1")), Some(3));
    }

    #[test]
    fn a_block_followed_by_a_human_turn_is_unresolved() {
        let steps = [
            call("t1", "Bash", true),
            Step::HumanTurn,
            call("t2", "Bash", false),
        ];
        assert_eq!(retry_calls(&steps, "t1", seen("t1")), None);
    }

    #[test]
    fn a_call_issued_alongside_the_blocked_one_is_not_a_retry() {
        let parallel = Step::Call {
            epoch_ms: issued("t1"),
            id: "t2".into(),
            tool: "Bash".into(),
            result: CallResult::Succeeded,
        };
        let steps = [call("t1", "Bash", true), parallel];
        assert_eq!(retry_calls(&steps, "t1", seen("t1")), None);
    }

    #[test]
    fn a_retry_still_waiting_for_its_result_is_unresolved() {
        let steps = [
            call("t1", "Bash", true),
            Step::Call {
                epoch_ms: issued("t2"),
                id: "t2".into(),
                tool: "Bash".into(),
                result: CallResult::Pending,
            },
        ];
        assert_eq!(retry_calls(&steps, "t1", seen("t1")), None);
    }

    #[test]
    fn each_tool_event_counts_the_calls_it_fires_on() {
        let triggers = Triggers {
            tool_calls: vec![ToolCalls {
                tool: "Bash".into(),
                succeeded: 7,
                failed: 2,
                denied: 3,
                pending: 1,
            }],
            sessions: 1,
            ..Triggers::default()
        };
        let count = |event| trigger_count(&def(event, Some("Bash"), "x"), &triggers);
        assert_eq!(count("PreToolUse"), Some(13));
        assert_eq!(count("PostToolUse"), Some(7));
        // A call refused before it ran never reaches a post-execution hook.
        assert_eq!(count("PostToolUseFailure"), Some(2));
        // Fires only on calls that raised a permission prompt; no call says so.
        assert_eq!(count("PermissionRequest"), None);
    }

    #[test]
    fn a_named_trace_two_entries_could_have_left_is_ambiguous() {
        // One command under two matching matchers runs once, deduplicated.
        let defs = [
            def("PreToolUse", None, "guard.sh"),
            def("PreToolUse", Some("Bash"), "guard.sh"),
        ];
        assert_eq!(
            resolve(&trace("PreToolUse", Some("Bash"), Some("guard.sh")), &defs),
            Resolution::Ambiguous
        );
    }

    #[test]
    fn a_block_never_retried_is_unresolved() {
        let steps = [call("t1", "Bash", true), call("t2", "Read", false)];
        assert_eq!(retry_calls(&steps, "t1", seen("t1")), None);
    }

    fn catalog(defs: &[HookDef], triggers: Option<u64>) -> Vec<CatalogHook> {
        defs.iter()
            .map(|def| CatalogHook {
                def: def.clone(),
                project: None,
                triggers,
            })
            .collect()
    }

    fn stored(trace: HookTrace, retry_calls: Option<u32>) -> StoredTrace {
        StoredTrace {
            trace,
            project: "example".into(),
            retry_calls,
        }
    }

    #[test]
    fn the_rollup_counts_blocks_and_their_retry_cost() {
        let defs = [def("PreToolUse", Some("Bash"), "guard.sh")];
        let blocked = |retry| {
            let mut t = trace("PreToolUse", Some("Bash"), Some("guard.sh"));
            t.outcome = HookOutcome::Blocked;
            stored(t, retry)
        };
        let traces = [blocked(Some(2)), blocked(Some(4)), blocked(None)];
        let refs: Vec<&StoredTrace> = traces.iter().collect();
        let (usages, orphans) = rollup(&catalog(&defs, Some(60)), &refs);
        assert!(orphans.is_empty());
        let usage = &usages[0];
        assert_eq!(usage.blocks, 3);
        assert_eq!(usage.block_rate(), Some(0.05));
        assert_eq!(usage.resolved_blocks, 2);
        assert_eq!(usage.mean_retry_calls(), Some(3.0));
    }

    #[test]
    fn the_rollup_reports_duration_percentiles_over_measured_runs() {
        let defs = [def("PostToolUse", Some("Edit"), "fmt")];
        let traces: Vec<StoredTrace> = [10, 20, 30, 40, 1000]
            .into_iter()
            .map(|ms| {
                let mut t = trace("PostToolUse", Some("Edit"), Some("fmt"));
                t.outcome = HookOutcome::Error;
                t.duration_ms = Some(ms);
                stored(t, None)
            })
            .chain(std::iter::once(stored(
                trace("PostToolUse", Some("Edit"), Some("fmt")),
                None,
            )))
            .collect();
        let refs: Vec<&StoredTrace> = traces.iter().collect();
        let (usages, _) = rollup(&catalog(&defs, None), &refs);
        let usage = &usages[0];
        assert_eq!(usage.measured_runs, 5);
        assert_eq!(usage.duration_total_ms, 1100);
        assert_eq!(usage.duration_p50_ms, Some(30));
        assert_eq!(usage.duration_p95_ms, Some(1000));
    }

    #[test]
    fn the_rollup_sums_injected_context() {
        let defs = [def("UserPromptSubmit", None, "inject.sh")];
        let traces: Vec<StoredTrace> = [300, 100]
            .into_iter()
            .map(|tokens| {
                let mut t = trace("UserPromptSubmit", None, None);
                t.outcome = HookOutcome::Context;
                t.context_tokens = tokens;
                stored(t, None)
            })
            .collect();
        let refs: Vec<&StoredTrace> = traces.iter().collect();
        let (usages, _) = rollup(&catalog(&defs, Some(2)), &refs);
        assert_eq!(usages[0].context_injections, 2);
        assert_eq!(usages[0].context_tokens, 400);
        assert_eq!(usages[0].mean_context_tokens(), Some(200));
    }

    #[test]
    fn a_project_hook_claims_only_its_own_projects_traces() {
        let hooks = [CatalogHook {
            def: def("PreToolUse", Some("Bash"), "guard.sh"),
            project: Some("alpha".into()),
            triggers: Some(1),
        }];
        let mut elsewhere = stored(trace("PreToolUse", Some("Bash"), Some("guard.sh")), None);
        elsewhere.project = "beta".into();
        let at_home = stored(trace("PreToolUse", Some("Bash"), Some("guard.sh")), None);
        let mut at_home = at_home;
        at_home.project = "alpha".into();
        let (usages, orphans) = rollup(&hooks, &[&elsewhere, &at_home]);
        assert_eq!(usages[0].traces, 1);
        assert_eq!(orphans.len(), 1);
    }

    #[test]
    fn a_hook_whose_trigger_counted_zero_never_matched() {
        let zero = HookUsage {
            triggers: Some(0),
            ..HookUsage::default()
        };
        let unknown = HookUsage::default();
        assert!(zero.never_matched());
        // An uncountable trigger is no evidence either way.
        assert!(!unknown.never_matched());
    }

    #[test]
    fn prompt_and_session_hooks_inject_standing_context() {
        assert_eq!(standing_context("UserPromptSubmit"), Some("prompt"));
        assert_eq!(standing_context("SessionStart"), Some("session"));
        assert_eq!(standing_context("PostToolUse"), None);
    }

    #[test]
    fn traces_no_hook_claims_come_back_as_orphans() {
        let traces = [
            stored(trace("Stop", None, Some("wrapper hook")), None),
            stored(trace("Stop", None, Some("wrapper hook")), None),
        ];
        let refs: Vec<&StoredTrace> = traces.iter().collect();
        let (usages, orphans) = rollup(&[], &refs);
        assert!(usages.is_empty());
        assert_eq!(orphans.len(), 1);
        assert_eq!(orphans[0].key, "Stop wrapper hook");
        assert!(!orphans[0].ambiguous);
        assert_eq!(orphans[0].usage.traces, 2);
    }

    #[test]
    fn an_unnamed_trace_several_hooks_could_have_left_is_ambiguous_not_orphaned() {
        let defs = [
            def("PreToolUse", Some("Bash"), "a.sh"),
            def("PreToolUse", Some("Bash"), "b.sh"),
        ];
        let mut blocked = trace("PreToolUse", Some("Bash"), None);
        blocked.outcome = HookOutcome::Blocked;
        let traces = [stored(blocked, None)];
        let refs: Vec<&StoredTrace> = traces.iter().collect();
        let (usages, unattributed) = rollup(&catalog(&defs, Some(5)), &refs);
        assert_eq!(usages[0].blocks + usages[1].blocks, 0);
        assert!(unattributed[0].ambiguous);
        assert_eq!(unattributed[0].usage.blocks, 1);
    }
}
