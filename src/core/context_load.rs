//! Config-file loads: the domain model of a rule / `CLAUDE.md` / memory file
//! entering the context, and the verdicts drawn from a rule's load history
//! (never-fires, effectively always-on). See `docs/specs/events.md`
//! (`context_load`) and `docs/specs/surfaces.md` ("Rule loads").

use crate::core::surface::{LoadMode, Wedge};

/// What kind of file was loaded. A label for unmatched loads only — the
/// catalog, joined by path, is what decides which surface a load belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileClass {
    Rule,
    ClaudeMd,
    Memory,
}

impl FileClass {
    pub fn label(self) -> &'static str {
        match self {
            FileClass::Rule => "rule",
            FileClass::ClaudeMd => "claude_md",
            FileClass::Memory => "memory",
        }
    }
}

/// When the file was loaded: with the session's always-on context, or later
/// because a file it is conditional on was touched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadTrigger {
    Startup,
    OnDemand,
}

impl LoadTrigger {
    pub fn label(self) -> &'static str {
        match self {
            LoadTrigger::Startup => "startup",
            LoadTrigger::OnDemand => "on_demand",
        }
    }
}

/// One config file entering the context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextLoad {
    pub epoch_ms: i64,
    pub path: String,
    pub class: FileClass,
    pub trigger: LoadTrigger,
    /// The Claude Code version that wrote the record — what decides whether
    /// the session could have recorded an on-demand load at all.
    pub version: Option<String>,
}

/// A Claude Code version as comparable numeric components (`2.1.302` →
/// `[2, 1, 302]`), read up to the first non-numeric component so a suffix
/// (`2.1.302-beta`) does not make it unreadable. `None` when it starts with
/// none.
pub fn version_key(version: &str) -> Option<Vec<u64>> {
    let key: Vec<u64> = version
        .split(['.', '-', '+'])
        .map_while(|part| part.parse().ok())
        .collect();
    (!key.is_empty()).then_some(key)
}

/// The oldest version seen writing an on-demand load: every session at or
/// above it is taken to record them, every session below it to be unable to.
/// Judged per version rather than per timestamp, because transcripts from an
/// older install can be written after newer ones (`docs/specs/surfaces.md`).
/// `None` when no on-demand load was ever recorded — then nothing is observed.
pub fn recording_floor<'a>(
    on_demand_versions: impl IntoIterator<Item = &'a str>,
) -> Option<Vec<u64>> {
    on_demand_versions.into_iter().filter_map(version_key).min()
}

/// Whether a session whose loads were written by `version` could have recorded
/// an on-demand load. A session with no readable version is not observed: its
/// silence cannot be told apart from an older install's.
pub fn is_observed(version: Option<&str>, floor: Option<&[u64]>) -> bool {
    match (version.and_then(version_key), floor) {
        (Some(key), Some(floor)) => key.as_slice() >= floor,
        _ => false,
    }
}

/// Fewest observed sessions a verdict may rest on. Below it a rule is
/// unmeasured: a handful of sessions that never touched the rule's files says
/// little about the glob.
pub const MIN_OBSERVED_SESSIONS: u64 = 5;

/// Share of observed sessions above which a conditional rule is paid so often
/// that its condition buys nothing.
pub const ALWAYS_ON_SHARE: f64 = 0.8;

/// The thresholds a verdict is drawn with, injected so tests pin them.
#[derive(Debug, Clone, Copy)]
pub struct LoadTuning {
    pub min_observed_sessions: u64,
    pub always_on_share: f64,
}

impl Default for LoadTuning {
    fn default() -> Self {
        LoadTuning {
            min_observed_sessions: MIN_OBSERVED_SESSIONS,
            always_on_share: ALWAYS_ON_SHARE,
        }
    }
}

/// A rule's load history over the window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LoadHistory {
    pub loads: u64,
    /// Distinct sessions it loaded in.
    pub sessions: u64,
    /// Sessions in the rule's reach that could have recorded a load.
    pub observed_sessions: u64,
}

/// What a rule's load history says about its `paths:` condition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadVerdict {
    /// Not path-conditional: it loads at every start, so its history can
    /// confirm its cost but flag nothing.
    Unconditional,
    /// Too few observed sessions to say anything.
    Unmeasured,
    /// Zero loads across enough observed sessions.
    NeverLoaded,
    /// Loaded in nearly every observed session.
    AlwaysLoaded,
    /// Loads in some sessions and not others — what a condition is for.
    Selective,
}

impl LoadVerdict {
    pub fn label(self) -> &'static str {
        match self {
            LoadVerdict::Unconditional => "unconditional",
            LoadVerdict::Unmeasured => "unmeasured",
            LoadVerdict::NeverLoaded => "never_loaded",
            LoadVerdict::AlwaysLoaded => "always_loaded",
            LoadVerdict::Selective => "selective",
        }
    }
}

/// Judge a rule's `paths:` condition from its load history.
pub fn load_verdict(load_mode: LoadMode, history: LoadHistory, tuning: LoadTuning) -> LoadVerdict {
    if load_mode != LoadMode::PathConditional {
        return LoadVerdict::Unconditional;
    }
    if history.observed_sessions < tuning.min_observed_sessions {
        return LoadVerdict::Unmeasured;
    }
    if history.loads == 0 {
        return LoadVerdict::NeverLoaded;
    }
    let share = history.sessions as f64 / history.observed_sessions as f64;
    if share >= tuning.always_on_share {
        LoadVerdict::AlwaysLoaded
    } else {
        LoadVerdict::Selective
    }
}

/// The optimization wedge a verdict raises, if any. Only a conditional rule's
/// verdict can raise one; an unmeasured rule raises none, so a store older than
/// the load records flags nothing.
pub fn load_wedge(verdict: LoadVerdict) -> Option<Wedge> {
    match verdict {
        LoadVerdict::NeverLoaded => Some(Wedge::NeverFires),
        LoadVerdict::AlwaysLoaded => Some(Wedge::AlwaysLoaded),
        LoadVerdict::Unconditional | LoadVerdict::Unmeasured | LoadVerdict::Selective => None,
    }
}

/// Tokens the loads cost: the file's static weight, re-paid on every load.
/// Unknown when the weight is.
pub fn tokens_paid(static_tokens: Option<u64>, loads: u64) -> Option<u64> {
    static_tokens.map(|tokens| tokens * loads)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn history(loads: u64, sessions: u64, observed_sessions: u64) -> LoadHistory {
        LoadHistory {
            loads,
            sessions,
            observed_sessions,
        }
    }

    fn tuning() -> LoadTuning {
        LoadTuning {
            min_observed_sessions: 5,
            always_on_share: 0.8,
        }
    }

    #[test]
    fn an_unconditional_rule_gets_no_verdict_whatever_its_history() {
        assert_eq!(
            load_verdict(LoadMode::StartupFull, history(0, 0, 50), tuning()),
            LoadVerdict::Unconditional
        );
    }

    #[test]
    fn a_conditional_rule_never_loaded_over_enough_sessions_never_fires() {
        assert_eq!(
            load_verdict(LoadMode::PathConditional, history(0, 0, 5), tuning()),
            LoadVerdict::NeverLoaded
        );
    }

    #[test]
    fn too_few_observed_sessions_leave_a_never_loaded_rule_unmeasured() {
        assert_eq!(
            load_verdict(LoadMode::PathConditional, history(0, 0, 4), tuning()),
            LoadVerdict::Unmeasured
        );
    }

    #[test]
    fn a_store_without_load_records_measures_nothing() {
        assert_eq!(
            load_verdict(LoadMode::PathConditional, history(0, 0, 0), tuning()),
            LoadVerdict::Unmeasured
        );
    }

    #[test]
    fn a_conditional_rule_loaded_in_nearly_every_session_is_always_loaded() {
        assert_eq!(
            load_verdict(LoadMode::PathConditional, history(30, 8, 10), tuning()),
            LoadVerdict::AlwaysLoaded
        );
    }

    #[test]
    fn the_always_on_share_counts_sessions_not_loads() {
        assert_eq!(
            load_verdict(LoadMode::PathConditional, history(30, 3, 10), tuning()),
            LoadVerdict::Selective
        );
    }

    #[test]
    fn a_never_loaded_rule_raises_the_never_fires_wedge() {
        assert_eq!(
            load_wedge(LoadVerdict::NeverLoaded),
            Some(Wedge::NeverFires)
        );
    }

    #[test]
    fn an_always_loaded_rule_raises_the_always_loaded_wedge() {
        assert_eq!(
            load_wedge(LoadVerdict::AlwaysLoaded),
            Some(Wedge::AlwaysLoaded)
        );
    }

    #[test]
    fn an_unmeasured_rule_raises_no_wedge() {
        assert_eq!(load_wedge(LoadVerdict::Unmeasured), None);
    }

    #[test]
    fn versions_compare_numerically_not_lexically() {
        assert!(version_key("2.1.302") > version_key("2.1.99"));
    }

    #[test]
    fn a_version_suffix_does_not_hide_its_numbers() {
        assert_eq!(version_key("2.1.302-beta"), Some(vec![2, 1, 302]));
    }

    #[test]
    fn the_recording_floor_is_the_oldest_version_writing_on_demand_loads() {
        assert_eq!(
            recording_floor(["2.1.318", "2.1.302", "garbage"]),
            Some(vec![2, 1, 302])
        );
    }

    #[test]
    fn a_session_below_the_floor_is_not_observed_whenever_it_was_written() {
        let floor = version_key("2.1.302");
        assert!(!is_observed(Some("2.1.299"), floor.as_deref()));
        assert!(is_observed(Some("2.1.302"), floor.as_deref()));
    }

    #[test]
    fn without_a_floor_or_a_version_nothing_is_observed() {
        assert!(!is_observed(Some("2.1.318"), None));
        assert!(!is_observed(None, version_key("2.1.302").as_deref()));
    }

    #[test]
    fn tokens_paid_repays_the_static_weight_per_load() {
        assert_eq!(tokens_paid(Some(120), 3), Some(360));
    }

    #[test]
    fn tokens_paid_is_unknown_when_the_weight_is() {
        assert_eq!(tokens_paid(None, 3), None);
    }
}
