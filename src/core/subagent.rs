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
    pub model: Option<String>,
    /// The spawning turn's id — the coarse fallback join key when no sidecar
    /// names the spawning call.
    pub prompt_id: Option<String>,
    pub out_tokens: u64,
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
            prompt_id: None,
            out_tokens: 0,
            started_epoch_ms: 0,
            root_tool_use_id: None,
        }
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
}
