//! Subagent runs: one execution of one agent type, with its own output cost.
//!
//! A run is what makes "which agent types consumed those tokens?" answerable —
//! the session-level subagent total says only *how much*. Runs also form a tree
//! (an agent can spawn agents), and only the tree's root was spawned from the
//! main thread, so `link_runs` resolves every run to that root before any
//! per-span attribution. See `docs/specs/events.md`.

/// One subagent execution: what ran, what it cost, and the keys that link it
/// back to the spawn that started it.
#[derive(Debug, Clone, PartialEq)]
pub struct SubagentRun {
    /// The agent type that ran (`Explore`, a custom agent's name). `None` when
    /// the transcript's sidecar is missing — the run is then counted but not
    /// attributed to a type, never guessed.
    pub agent: Option<String>,
    /// The run's own id, unique within the session; the key children name.
    pub agent_id: String,
    /// The subagent's own transcript, kept so a report can point at the run
    /// itself rather than only at the session that spawned it.
    pub run_path: String,
    /// The spawning `Agent` tool call's id — the exact join key back to the
    /// spawning event (`docs/specs/session-format.md`). `None` without a sidecar.
    pub tool_use_id: Option<String>,
    /// The run's parent run, for a subagent spawned by a subagent.
    pub parent_agent_id: Option<String>,
    /// 1 for a main-thread spawn, 2+ for a nested one.
    pub spawn_depth: u32,
    /// The model the run ran on, read from its own transcript.
    pub model: Option<String>,
    /// The model the spawning call asked for, as written (`opus`, `inherit`);
    /// `None` when it named none or no sidecar recorded it.
    pub requested_model: Option<String>,
    /// The spawning turn's id — the coarse fallback join key when no sidecar
    /// names the spawning call.
    pub prompt_id: Option<String>,
    pub out_tokens: u64,
    /// The full prompt size summed once per API response — what re-reading its
    /// own context cost, usually far more than its output.
    pub prompt_tokens: u64,
    pub started_epoch_ms: i64,
    /// The `tool_use_id` of the main-thread spawn that started this run's tree,
    /// filled by [`link_runs`]. Equals `tool_use_id` for a depth-1 run.
    pub root_tool_use_id: Option<String>,
}

/// Resolve each run's `root_tool_use_id` by walking `parent_agent_id` up to the
/// run that was spawned from the main thread.
///
/// A nested run's own `tool_use_id` names a call inside its *parent's*
/// transcript, which no main-thread span contains — so attributing a subtree's
/// cost to the main-thread work that caused it requires the root's id, not the
/// run's own. A run whose chain is broken (a missing parent, or a cycle in
/// malformed input) keeps `None` and stays unattributed rather than being
/// charged to an arbitrary spawn.
pub fn link_runs(runs: &mut [SubagentRun]) {
    let parents: Vec<(String, Option<String>, Option<String>)> = runs
        .iter()
        .map(|run| {
            (
                run.agent_id.clone(),
                run.parent_agent_id.clone(),
                run.tool_use_id.clone(),
            )
        })
        .collect();

    let roots: Vec<Option<String>> = (0..parents.len())
        .map(|start| {
            let mut current = start;
            let mut hops = 0;
            loop {
                let (_, parent, tool_use_id) = &parents[current];
                let Some(parent) = parent else {
                    break tool_use_id.clone();
                };
                // Bound the walk so a cycle in malformed input cannot hang analyze.
                hops += 1;
                if hops > parents.len() {
                    break None;
                }
                match parents.iter().position(|(id, _, _)| id == parent) {
                    Some(next) => current = next,
                    None => break None,
                }
            }
        })
        .collect();

    for (run, root) in runs.iter_mut().zip(roots) {
        run.root_tool_use_id = root;
    }
}

/// Who decided which model a spawned subagent ran on (`docs/specs/events.md`
/// "Model choice at spawn").
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelChoice {
    /// The spawning call named a model.
    Explicit,
    /// Nobody chose: the run ran on the caller's own model.
    Inherited,
    /// The call named none, and the agent's definition pinned a model — or,
    /// with no pin on record, the run ran on another model than the caller's,
    /// so a built-in default picked it.
    AgentDefault,
    /// The call named none, and either no run is left to say what it ran on or
    /// the caller's model is unknown, so there is nothing to compare.
    Unknown,
}

/// The request value that asks for the caller's model rather than naming one.
const INHERIT: &str = "inherit";

impl ModelChoice {
    pub fn label(self) -> &'static str {
        match self {
            ModelChoice::Explicit => "explicit",
            ModelChoice::Inherited => "inherited",
            ModelChoice::AgentDefault => "agent default",
            ModelChoice::Unknown => "unknown",
        }
    }
}

/// The models around one main-thread spawn: what its call asked for, what the
/// agent's definition pins, what the calling turn ran on, and what its run ran
/// on (`None` when no run is left).
#[derive(Debug, Clone, PartialEq)]
pub struct SpawnModels {
    pub agent: String,
    pub requested: Option<String>,
    pub pinned: Option<String>,
    pub caller: Option<String>,
    pub ran: Option<String>,
}

impl SpawnModels {
    /// Who chose the model this spawn ran on. A pin is read from the current
    /// definition, the rest from the spawn itself. Full model ids are compared,
    /// never tier names, so the rule holds for any model family.
    pub fn choice(&self) -> ModelChoice {
        let named = |model: &Option<String>| model.clone().filter(|model| !model.is_empty());
        match (
            named(&self.requested).as_deref(),
            named(&self.pinned).as_deref(),
        ) {
            (Some(INHERIT), _) => ModelChoice::Inherited,
            (Some(_), _) => ModelChoice::Explicit,
            (None, Some(pin)) if pin != INHERIT => ModelChoice::AgentDefault,
            _ => match (self.caller.as_deref(), self.ran.as_deref()) {
                (Some(caller), Some(ran)) if caller == ran => ModelChoice::Inherited,
                (Some(_), Some(_)) => ModelChoice::AgentDefault,
                _ => ModelChoice::Unknown,
            },
        }
    }
}

/// One agent type's spawns, split by who chose the model.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct AgentModelChoice {
    pub agent: String,
    pub spawns: i64,
    pub explicit: i64,
    pub inherited: i64,
    pub agent_default: i64,
    pub unknown: i64,
    /// The models the calls named, as written, most requested first.
    pub requested: Vec<(String, i64)>,
}

/// Roll spawns up per agent type, most spawned first.
pub fn model_choice_by_agent(spawns: &[SpawnModels]) -> Vec<AgentModelChoice> {
    let mut rows: Vec<AgentModelChoice> = Vec::new();
    for spawn in spawns {
        let index = match rows.iter().position(|row| row.agent == spawn.agent) {
            Some(index) => index,
            None => {
                rows.push(AgentModelChoice {
                    agent: spawn.agent.clone(),
                    spawns: 0,
                    explicit: 0,
                    inherited: 0,
                    agent_default: 0,
                    unknown: 0,
                    requested: Vec::new(),
                });
                rows.len() - 1
            }
        };
        let row = &mut rows[index];
        row.spawns += 1;
        match spawn.choice() {
            ModelChoice::Explicit => row.explicit += 1,
            ModelChoice::Inherited => row.inherited += 1,
            ModelChoice::AgentDefault => row.agent_default += 1,
            ModelChoice::Unknown => row.unknown += 1,
        }
        if let Some(requested) = spawn.requested.as_deref().filter(|m| !m.is_empty()) {
            tally(&mut row.requested, requested);
        }
    }
    for row in &mut rows {
        sort_tally(&mut row.requested);
    }
    rows.sort_by(|a, b| b.spawns.cmp(&a.spawns).then_with(|| a.agent.cmp(&b.agent)));
    rows
}

/// How many main-thread spawns each main-loop model made, most first. Spawns,
/// not turns: a turn that launches several agents in parallel counts each.
pub fn caller_models(spawns: &[SpawnModels]) -> Vec<(String, i64)> {
    let mut counts = Vec::new();
    for caller in spawns.iter().filter_map(|spawn| spawn.caller.as_deref()) {
        tally(&mut counts, caller);
    }
    sort_tally(&mut counts);
    counts
}

fn tally(counts: &mut Vec<(String, i64)>, key: &str) {
    match counts.iter_mut().find(|(name, _)| name == key) {
        Some((_, count)) => *count += 1,
        None => counts.push((key.to_string(), 1)),
    }
}

fn sort_tally(counts: &mut [(String, i64)]) {
    counts.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
}

/// How much of the session-level subagent total the per-agent rows account for.
///
/// The total and the rows come from different records and can disagree: a
/// session's total survives as long as the store does, while its per-agent
/// rows exist only if the transcript was still on disk when run extraction
/// ran over it. Claude Code prunes transcripts on its own schedule, so a store
/// analyzed before runs were extracted keeps totals whose rows can never be
/// recovered — and the rows that *were* extracted would pass for the whole
/// breakdown unless the gap is reported beside them. Every report that prints
/// the rows carries this alongside so a partial split is labeled as one.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct SplitCoverage {
    /// The session-level total: output tokens and run count.
    pub total_tokens: i64,
    pub total_runs: i64,
    /// What the per-agent rows sum to.
    pub known_tokens: i64,
    pub known_runs: i64,
    /// The part of the total no row accounts for — never negative.
    pub missing_tokens: i64,
    pub missing_runs: i64,
}

impl SplitCoverage {
    /// Compare a session-level total against per-agent `rows` of
    /// `(runs, out_tokens)`.
    pub fn new(
        total_tokens: i64,
        total_runs: i64,
        rows: impl IntoIterator<Item = (i64, i64)>,
    ) -> Self {
        let (known_runs, known_tokens) = rows
            .into_iter()
            .fold((0, 0), |(runs, tokens), (r, t)| (runs + r, tokens + t));
        Self {
            total_tokens,
            total_runs,
            known_tokens,
            known_runs,
            missing_tokens: (total_tokens - known_tokens).max(0),
            missing_runs: (total_runs - known_runs).max(0),
        }
    }

    /// A nonzero total with no rows behind it at all — the split was never
    /// extracted, which a refresh may still fix. Either axis makes the total
    /// nonzero: a run that wrote nothing is still a run.
    pub fn is_unavailable(&self) -> bool {
        (self.total_tokens > 0 || self.total_runs > 0)
            && self.known_runs == 0
            && self.known_tokens == 0
    }

    /// Rows exist but fall short of the total — the remainder belongs to
    /// sessions whose runs are gone for good.
    pub fn is_partial(&self) -> bool {
        !self.is_unavailable() && (self.missing_tokens > 0 || self.missing_runs > 0)
    }

    /// Whether `tokens` (a subset of the rows) is more than half the total —
    /// the bar for calling those rows "most of" the subagent output.
    pub fn is_majority(&self, tokens: i64) -> bool {
        self.total_tokens > 0 && tokens * 2 > self.total_tokens
    }

    /// The sentence a report prints beside a partial split, with exact
    /// figures: what the rows cover, what they do not, and why the rest can
    /// never be filled in. `None` when there is no gap to explain.
    pub fn partial_note(&self) -> Option<String> {
        self.is_partial().then(|| {
            format!(
                "per-agent split covers {} tokens over {} run(s); the other {} tokens over \
                 {} run(s) have no split (analyzed before runs were extracted, transcripts \
                 since pruned)",
                self.known_tokens, self.known_runs, self.missing_tokens, self.missing_runs
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(agent_id: &str, parent: Option<&str>, tool_use_id: Option<&str>) -> SubagentRun {
        SubagentRun {
            agent: Some("Explore".into()),
            agent_id: agent_id.into(),
            run_path: format!("/tmp/example/subagents/agent-{agent_id}.jsonl"),
            tool_use_id: tool_use_id.map(String::from),
            parent_agent_id: parent.map(String::from),
            spawn_depth: 1,
            model: None,
            requested_model: None,
            prompt_id: None,
            out_tokens: 0,
            prompt_tokens: 0,
            started_epoch_ms: 0,
            root_tool_use_id: None,
        }
    }

    fn spawn(agent: &str, requested: Option<&str>, caller: &str, ran: Option<&str>) -> SpawnModels {
        SpawnModels {
            agent: agent.into(),
            requested: requested.map(String::from),
            pinned: None,
            caller: Some(caller.into()),
            ran: ran.map(String::from),
        }
    }

    fn models(
        requested: Option<&str>,
        pinned: Option<&str>,
        caller: Option<&str>,
        ran: Option<&str>,
    ) -> SpawnModels {
        SpawnModels {
            agent: "Explore".into(),
            requested: requested.map(String::from),
            pinned: pinned.map(String::from),
            caller: caller.map(String::from),
            ran: ran.map(String::from),
        }
    }

    #[test]
    fn a_definition_pinning_a_model_is_the_agents_default_even_on_the_callers_model() {
        let choice = models(None, Some("big"), Some("m-big"), Some("m-big")).choice();
        assert_eq!(choice, ModelChoice::AgentDefault);
    }

    #[test]
    fn a_call_naming_a_model_overrides_the_definitions_pin() {
        let choice = models(Some("small"), Some("big"), Some("m-big"), Some("m-small")).choice();
        assert_eq!(choice, ModelChoice::Explicit);
    }

    #[test]
    fn a_definition_pinned_to_inherit_pins_nothing() {
        let choice = models(None, Some("inherit"), Some("m-big"), Some("m-big")).choice();
        assert_eq!(choice, ModelChoice::Inherited);
    }

    #[test]
    fn a_call_that_names_a_model_is_explicit() {
        let choice = models(Some("sonnet"), None, Some("m-big"), Some("m-mid")).choice();
        assert_eq!(choice, ModelChoice::Explicit);
    }

    #[test]
    fn an_unpinned_run_on_the_callers_model_inherited_it() {
        let choice = models(None, None, Some("m-big"), Some("m-big")).choice();
        assert_eq!(choice, ModelChoice::Inherited);
    }

    #[test]
    fn asking_to_inherit_is_inheriting_whatever_ran() {
        let choice = models(Some("inherit"), None, Some("m-big"), None).choice();
        assert_eq!(choice, ModelChoice::Inherited);
    }

    #[test]
    fn an_unpinned_run_on_another_model_took_the_agents_default() {
        let choice = models(None, None, Some("m-big"), Some("m-small")).choice();
        assert_eq!(choice, ModelChoice::AgentDefault);
    }

    #[test]
    fn an_unpinned_spawn_without_its_run_is_unknown() {
        let choice = models(None, None, Some("m-big"), None).choice();
        assert_eq!(choice, ModelChoice::Unknown);
    }

    #[test]
    fn an_unpinned_spawn_whose_callers_model_is_unknown_is_unknown() {
        let choice = models(None, None, None, Some("m-small")).choice();
        assert_eq!(choice, ModelChoice::Unknown);
    }

    #[test]
    fn an_empty_requested_model_counts_as_none() {
        let choice = models(Some(""), None, Some("m-big"), Some("m-big")).choice();
        assert_eq!(choice, ModelChoice::Inherited);
    }

    #[test]
    fn model_choice_rolls_up_per_agent_type_most_spawned_first() {
        let spawns = [
            spawn("Plan", None, "m-big", Some("m-big")),
            spawn("Explore", Some("sonnet"), "m-big", Some("m-mid")),
            spawn("Explore", None, "m-big", Some("m-big")),
            spawn("Explore", Some("sonnet"), "m-big", None),
        ];
        let rows = model_choice_by_agent(&spawns);
        assert_eq!(
            rows,
            vec![
                AgentModelChoice {
                    agent: "Explore".into(),
                    spawns: 3,
                    explicit: 2,
                    inherited: 1,
                    agent_default: 0,
                    unknown: 0,
                    requested: vec![("sonnet".into(), 2)],
                },
                AgentModelChoice {
                    agent: "Plan".into(),
                    spawns: 1,
                    explicit: 0,
                    inherited: 1,
                    agent_default: 0,
                    unknown: 0,
                    requested: vec![],
                },
            ]
        );
    }

    #[test]
    fn requested_models_are_listed_most_requested_first() {
        let spawns = [
            spawn("Explore", Some("haiku"), "m-big", None),
            spawn("Explore", Some("opus"), "m-big", None),
            spawn("Explore", Some("opus"), "m-big", None),
        ];
        let rows = model_choice_by_agent(&spawns);
        assert_eq!(
            rows[0].requested,
            vec![("opus".to_string(), 2), ("haiku".to_string(), 1)]
        );
    }

    #[test]
    fn caller_models_count_spawns_per_main_loop_model() {
        let spawns = [
            spawn("Explore", None, "m-big", None),
            spawn("Plan", None, "m-big", None),
            spawn("Plan", None, "m-mid", None),
        ];
        assert_eq!(
            caller_models(&spawns),
            vec![("m-big".to_string(), 2), ("m-mid".to_string(), 1)]
        );
    }

    #[test]
    fn a_top_level_run_is_its_own_root() {
        let mut runs = vec![run("a1", None, Some("toolu_1"))];
        link_runs(&mut runs);
        assert_eq!(runs[0].root_tool_use_id.as_deref(), Some("toolu_1"));
    }

    #[test]
    fn a_nested_run_resolves_to_the_main_thread_spawn_that_started_its_tree() {
        let mut runs = vec![
            run("a1", None, Some("toolu_1")),
            run("a2", Some("a1"), Some("toolu_2")),
            run("a3", Some("a2"), Some("toolu_3")),
        ];
        link_runs(&mut runs);
        let roots: Vec<_> = runs.iter().map(|r| r.root_tool_use_id.as_deref()).collect();
        assert_eq!(
            roots,
            vec![Some("toolu_1"), Some("toolu_1"), Some("toolu_1")]
        );
    }

    #[test]
    fn a_run_whose_parent_is_missing_stays_unrooted() {
        let mut runs = vec![run("a2", Some("gone"), Some("toolu_2"))];
        link_runs(&mut runs);
        assert_eq!(runs[0].root_tool_use_id, None);
    }

    #[test]
    fn a_parent_cycle_terminates_without_a_root() {
        let mut runs = vec![
            run("a1", Some("a2"), Some("toolu_1")),
            run("a2", Some("a1"), Some("toolu_2")),
        ];
        link_runs(&mut runs);
        assert!(runs.iter().all(|r| r.root_tool_use_id.is_none()));
    }

    #[test]
    fn rows_summing_to_the_total_are_a_complete_split() {
        let coverage = SplitCoverage::new(1_800_000, 300, [(220, 1_500_000), (80, 300_000)]);
        assert_eq!(coverage.missing_tokens, 0);
        assert_eq!(coverage.missing_runs, 0);
        assert!(!coverage.is_partial());
        assert!(!coverage.is_unavailable());
    }

    #[test]
    fn rows_short_of_the_total_are_a_partial_split() {
        // Sessions analyzed before per-run extraction keep their totals, but
        // once Claude Code pruned their transcripts the runs can never be
        // extracted — so the rows that were cover only a fraction.
        let coverage = SplitCoverage::new(3_093_466, 375, [(11, 315_663), (14, 62_067)]);
        assert_eq!(coverage.known_tokens, 377_730);
        assert_eq!(coverage.known_runs, 25);
        assert_eq!(coverage.missing_tokens, 2_715_736);
        assert_eq!(coverage.missing_runs, 350);
        assert!(coverage.is_partial());
        assert!(!coverage.is_unavailable());
    }

    #[test]
    fn no_rows_behind_a_nonzero_total_is_an_unavailable_split() {
        let coverage = SplitCoverage::new(3_093_466, 375, []);
        assert!(coverage.is_unavailable());
        assert!(!coverage.is_partial());
    }

    #[test]
    fn no_rows_behind_runs_that_wrote_nothing_is_still_an_unavailable_split() {
        // A run that produced no output still counts as a run, so a total can
        // be nonzero on the run axis alone — and rows are just as absent.
        let coverage = SplitCoverage::new(0, 3, []);
        assert!(coverage.is_unavailable());
        assert!(!coverage.is_partial());
    }

    #[test]
    fn no_rows_behind_a_zero_total_is_neither_partial_nor_unavailable() {
        let coverage = SplitCoverage::new(0, 0, []);
        assert!(!coverage.is_partial());
        assert!(!coverage.is_unavailable());
    }

    #[test]
    fn rows_exceeding_the_total_report_no_gap() {
        // The gap is reported as what is missing, never as a negative figure.
        let coverage = SplitCoverage::new(100, 1, [(2, 150)]);
        assert_eq!(coverage.missing_tokens, 0);
        assert_eq!(coverage.missing_runs, 0);
        assert!(!coverage.is_partial());
    }

    #[test]
    fn a_majority_is_more_than_half_of_the_total() {
        let coverage = SplitCoverage::new(1_000, 10, [(10, 1_000)]);
        assert!(coverage.is_majority(501));
        assert!(!coverage.is_majority(500));
        // A zero total has no majority to claim.
        assert!(!SplitCoverage::new(0, 0, []).is_majority(0));
    }

    #[test]
    fn a_partial_split_explains_its_gap_and_a_complete_one_says_nothing() {
        let partial = SplitCoverage::new(3_093_466, 375, [(11, 315_663), (14, 62_067)]);
        assert_eq!(
            partial.partial_note().as_deref(),
            Some(
                "per-agent split covers 377730 tokens over 25 run(s); the other 2715736 tokens \
                 over 350 run(s) have no split (analyzed before runs were extracted, \
                 transcripts since pruned)"
            )
        );
        assert_eq!(SplitCoverage::new(100, 1, [(1, 100)]).partial_note(), None);
        // An unavailable split is a different message (a refresh may fix it),
        // so it is not described as partial.
        assert_eq!(SplitCoverage::new(100, 1, []).partial_note(), None);
    }
}
