//! Before/after a config change: did the friction a rule was written for
//! become rarer once the rule was in force? Sessions are split at the
//! surface's last change into two equal, adjacent windows and each friction
//! category's rate is compared across them, with the counts beside every rate
//! so a small sample reads as small (`docs/specs/surfaces.md` "Before / after
//! a config change"). Comparison, never causation: the result carries the
//! confounders it can see so the reader can discount it.

use std::collections::BTreeMap;

use crate::core::friction::ErrorCategory;
use crate::core::surface::Scope;

/// Where a surface's change date came from — how far to trust it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DateSource {
    /// The newest commit touching the file.
    Git,
    /// The file has edits newer than any commit; the date is its mtime.
    Uncommitted,
    /// No history at all; the mtime, which checkouts and copies rewrite.
    Mtime,
}

impl DateSource {
    pub fn label(self) -> &'static str {
        match self {
            DateSource::Git => "git",
            DateSource::Uncommitted => "uncommitted",
            DateSource::Mtime => "mtime",
        }
    }

    pub fn from_label(label: &str) -> DateSource {
        match label {
            "git" => DateSource::Git,
            "uncommitted" => DateSource::Uncommitted,
            _ => DateSource::Mtime,
        }
    }
}

/// When one surface last changed, and the friction it declares it targets.
#[derive(Debug, Clone, PartialEq)]
pub struct ConfigChange {
    pub kind: String,
    pub id: String,
    pub scope: Scope,
    pub changed_epoch: i64,
    pub introduced_epoch: i64,
    pub date_source: DateSource,
    /// Friction-category labels from the surface's own declaration; empty when
    /// it declares none.
    pub targets: Vec<String>,
    /// Whether every session loads it. A surface loaded only on demand (a
    /// `paths:` rule, a skill) was absent from some of the compared sessions,
    /// which dilute its rate, so its verdict never raises a flag.
    pub always_on: bool,
}

impl ConfigChange {
    /// `kind/id` — how a report names the surface.
    pub fn name(&self) -> String {
        format!("{}/{}", self.kind, self.id)
    }

    /// `kind/id`, plus the owning project for a project copy — how a list
    /// that mixes scopes names it.
    pub fn scoped_name(&self) -> String {
        match &self.scope {
            Scope::Global => self.name(),
            Scope::Project(project) => format!("{} (project {project})", self.name()),
        }
    }
}

/// One session as the comparison sees it: where and when it started, how many
/// tool calls it made, and how many failures of each category it hit.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionSample {
    pub project: String,
    pub start_epoch: i64,
    /// `None` for a session analysed before call counts were recorded.
    pub tool_calls: Option<u64>,
    pub errors: BTreeMap<String, u64>,
}

/// Tuning constants, injected so tests stay independent of the defaults.
#[derive(Debug, Clone, Copy)]
pub struct Params {
    /// The longest span compared on each side of the change.
    pub window_secs: i64,
    /// Fewer sessions than this on either side yields `TooFew`.
    pub min_sessions: usize,
    /// One-sided significance level for `Improved`.
    pub alpha: f64,
}

impl Default for Params {
    fn default() -> Self {
        Params {
            window_secs: 28 * 86_400,
            min_sessions: 5,
            alpha: 0.05,
        }
    }
}

/// One side of the comparison: `[start, end)` and what fell into it.
#[derive(Debug, Clone, PartialEq)]
pub struct Window {
    pub start: i64,
    pub end: i64,
    pub sessions: usize,
    /// Summed tool calls; `None` when any session in the window lacks a count.
    pub tool_calls: Option<u64>,
}

/// What the rates are measured against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Basis {
    ToolCalls,
    Sessions,
}

impl Basis {
    pub fn label(self) -> &'static str {
        match self {
            Basis::ToolCalls => "per 100 tool calls",
            Basis::Sessions => "per session",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Either window holds too few sessions to say anything.
    TooFew,
    /// The category occurred in neither window.
    NoSignal,
    /// Lower after, and unlikely to be chance.
    Improved,
    /// Lower after, but within what chance would produce.
    Unclear,
    /// Not lower after.
    NoImprovement,
}

impl Verdict {
    pub fn label(self) -> &'static str {
        match self {
            Verdict::TooFew => "too-few",
            Verdict::NoSignal => "no-signal",
            Verdict::Improved => "improved",
            Verdict::Unclear => "unclear",
            Verdict::NoImprovement => "no-improvement",
        }
    }
}

/// One friction category across the change.
#[derive(Debug, Clone, PartialEq)]
pub struct CategoryEffect {
    pub category: String,
    pub before: u64,
    pub after: u64,
    pub before_per_session: Option<f64>,
    pub after_per_session: Option<f64>,
    pub before_per_100_calls: Option<f64>,
    pub after_per_100_calls: Option<f64>,
    /// One-sided exact p-value of seeing this few after the change were the
    /// rate unchanged; `None` when no test was run.
    pub p_value: Option<f64>,
    pub verdict: Verdict,
    /// Whether the surface declares this category as its target.
    pub declared: bool,
    /// Set on a declared target that did not improve — what to do about it.
    pub flag: Option<Flag>,
}

/// What a declared target that did not improve suggests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flag {
    /// The prose did not change the behaviour; a deterministic hook would
    /// enforce what the rule only asks for.
    ReplaceWithHook,
    /// The failures are a hook's own blocks, so enforcement already exists:
    /// the prose is not preventing the attempts. Steer from the hook's message
    /// instead, or drop the rule.
    HookAlreadyBlocks,
}

impl Flag {
    pub fn label(self) -> &'static str {
        match self {
            Flag::ReplaceWithHook => "replace-with-hook",
            Flag::HookAlreadyBlocks => "hook-already-blocks",
        }
    }
}

/// The whole before/after comparison for one surface.
#[derive(Debug, Clone, PartialEq)]
pub struct Effect {
    pub change: ConfigChange,
    pub before: Window,
    pub after: Window,
    pub basis: Basis,
    pub categories: Vec<CategoryEffect>,
    /// Other surfaces changed inside `[before.start, after.end)` — any effect
    /// seen here is shared with them.
    pub concurrent: Vec<String>,
    /// Declared targets that name no known friction category.
    pub unknown_targets: Vec<String>,
}

/// Compare friction before and after `change`, over the sessions it governs.
/// `all_changes` supplies the confounding changes; `now` closes the
/// after-window (no clock is read here).
pub fn compare(
    change: &ConfigChange,
    all_changes: &[ConfigChange],
    samples: &[SessionSample],
    now: i64,
    params: &Params,
) -> Effect {
    let governed: Vec<&SessionSample> = samples
        .iter()
        .filter(|s| match &change.scope {
            Scope::Global => true,
            Scope::Project(project) => &s.project == project,
        })
        .collect();
    let boundary = change.changed_epoch;
    let span = {
        let first = governed.iter().map(|s| s.start_epoch).min();
        let available_before = first.map_or(0, |first| boundary - first);
        params
            .window_secs
            .min(now - boundary)
            .min(available_before)
            .max(0)
    };
    let before_sessions: Vec<&SessionSample> = governed
        .iter()
        .copied()
        .filter(|s| s.start_epoch >= boundary - span && s.start_epoch < boundary)
        .collect();
    let after_sessions: Vec<&SessionSample> = governed
        .iter()
        .copied()
        .filter(|s| s.start_epoch >= boundary && s.start_epoch < boundary + span)
        .collect();
    let before = window(boundary - span, boundary, &before_sessions);
    let after = window(boundary, boundary + span, &after_sessions);
    let basis = match (before.tool_calls, after.tool_calls) {
        (Some(b), Some(a)) if b > 0 && a > 0 => Basis::ToolCalls,
        _ => Basis::Sessions,
    };

    let (known, unknown_targets): (Vec<String>, Vec<String>) = change
        .targets
        .iter()
        .cloned()
        .partition(|label| is_known_category(label));
    let declared = !change.targets.is_empty();
    let categories: Vec<String> = if declared {
        known
    } else {
        let mut totals: BTreeMap<&str, u64> = BTreeMap::new();
        for s in before_sessions.iter().chain(after_sessions.iter()) {
            for (category, n) in &s.errors {
                if ErrorCategory::from_label(category).is_actionable() {
                    *totals.entry(category.as_str()).or_default() += n;
                }
            }
        }
        let mut ranked: Vec<(&str, u64)> = totals.into_iter().filter(|(_, n)| *n > 0).collect();
        ranked.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
        ranked.into_iter().map(|(c, _)| c.to_string()).collect()
    };

    let categories = categories
        .into_iter()
        .map(|category| {
            let count = |sessions: &[&SessionSample]| -> u64 {
                sessions
                    .iter()
                    .map(|s| s.errors.get(&category).copied().unwrap_or(0))
                    .sum()
            };
            let before_n = count(&before_sessions);
            let after_n = count(&after_sessions);
            let (before_exposure, after_exposure) = match basis {
                Basis::ToolCalls => (
                    before.tool_calls.unwrap_or(0) as f64,
                    after.tool_calls.unwrap_or(0) as f64,
                ),
                Basis::Sessions => (before.sessions as f64, after.sessions as f64),
            };
            // A side whose sessions are known to have made no tool calls had no
            // chance to fail; a zero there is not an improvement.
            let attempted = !matches!(
                (before.tool_calls, after.tool_calls),
                (Some(0), _) | (_, Some(0))
            );
            let enough = attempted
                && before.sessions >= params.min_sessions
                && after.sessions >= params.min_sessions;
            let (verdict, p_value) = if !enough {
                (Verdict::TooFew, None)
            } else if before_n + after_n == 0 {
                (Verdict::NoSignal, None)
            } else if after_n as f64 / after_exposure >= before_n as f64 / before_exposure {
                (Verdict::NoImprovement, None)
            } else {
                let share = after_exposure / (before_exposure + after_exposure);
                let p = binomial_cdf(after_n, before_n + after_n, share);
                let verdict = if p < params.alpha {
                    Verdict::Improved
                } else {
                    Verdict::Unclear
                };
                (verdict, Some(p))
            };
            CategoryEffect {
                before_per_session: rate(before_n, before.sessions as u64, 1.0),
                after_per_session: rate(after_n, after.sessions as u64, 1.0),
                before_per_100_calls: before.tool_calls.and_then(|c| rate(before_n, c, 100.0)),
                after_per_100_calls: after.tool_calls.and_then(|c| rate(after_n, c, 100.0)),
                p_value,
                verdict,
                declared,
                flag: (declared && change.always_on && verdict == Verdict::NoImprovement).then(
                    || {
                        if ErrorCategory::from_label(&category) == ErrorCategory::BlockedByHook {
                            Flag::HookAlreadyBlocks
                        } else {
                            Flag::ReplaceWithHook
                        }
                    },
                ),
                before: before_n,
                after: after_n,
                category,
            }
        })
        .collect();

    // Copies of one file were merged by the adapter, so a same-named row in
    // another scope is a different file and confounds like any other.
    let concurrent = {
        let mut nearby: Vec<&ConfigChange> = all_changes
            .iter()
            .filter(|other| {
                !(other.kind == change.kind && other.id == change.id && other.scope == change.scope)
            })
            .filter(|other| match (&change.scope, &other.scope) {
                (Scope::Project(mine), Scope::Project(theirs)) => mine == theirs,
                _ => true,
            })
            .filter(|other| {
                span > 0
                    && other.changed_epoch >= boundary - span
                    && other.changed_epoch < boundary + span
            })
            .collect();
        nearby.sort_by_key(|other| (other.changed_epoch - boundary).abs());
        let mut names: Vec<String> = Vec::new();
        for name in nearby.into_iter().map(ConfigChange::scoped_name) {
            if !names.contains(&name) {
                names.push(name);
            }
        }
        names
    };

    Effect {
        change: change.clone(),
        before,
        after,
        basis,
        categories,
        concurrent,
        unknown_targets,
    }
}

fn window(start: i64, end: i64, sessions: &[&SessionSample]) -> Window {
    Window {
        start,
        end,
        sessions: sessions.len(),
        tool_calls: sessions.iter().map(|s| s.tool_calls).sum(),
    }
}

fn rate(count: u64, exposure: u64, per: f64) -> Option<f64> {
    (exposure > 0).then(|| count as f64 * per / exposure as f64)
}

fn is_known_category(label: &str) -> bool {
    ErrorCategory::from_label(label).label() == label
}

/// `P(X <= x)` for `X ~ Binomial(n, p)`, summed in log space: with a few
/// hundred failures the first term alone underflows an `f64`.
fn binomial_cdf(x: u64, n: u64, p: f64) -> f64 {
    if p <= 0.0 {
        return 1.0;
    }
    if p >= 1.0 {
        return if x >= n { 1.0 } else { 0.0 };
    }
    let (ln_p, ln_q) = (p.ln(), (1.0 - p).ln());
    let mut ln_term = n as f64 * ln_q;
    let mut terms = Vec::with_capacity(x as usize + 1);
    for i in 0..=x.min(n) {
        terms.push(ln_term);
        ln_term += ((n - i) as f64).ln() - ((i + 1) as f64).ln() + ln_p - ln_q;
    }
    let max = terms.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let sum: f64 = terms.iter().map(|t| (t - max).exp()).sum();
    (max + sum.ln()).exp().min(1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: i64 = 86_400;
    const B: i64 = 100 * DAY;

    fn change(id: &str, changed: i64, targets: &[&str]) -> ConfigChange {
        ConfigChange {
            kind: "rule".to_string(),
            id: id.to_string(),
            scope: Scope::Global,
            changed_epoch: changed,
            introduced_epoch: changed,
            date_source: DateSource::Git,
            targets: targets.iter().map(|t| t.to_string()).collect(),
            always_on: true,
        }
    }

    fn session(start: i64, calls: Option<u64>, errors: &[(&str, u64)]) -> SessionSample {
        SessionSample {
            project: "example".to_string(),
            start_epoch: start,
            tool_calls: calls,
            errors: errors.iter().map(|(c, n)| (c.to_string(), *n)).collect(),
        }
    }

    /// `n` sessions a day apart ending just before `end`, each with `calls`
    /// tool calls and `errors` failures of `category`.
    fn days(end: i64, n: i64, calls: u64, category: &str, errors: u64) -> Vec<SessionSample> {
        (1..=n)
            .map(|d| session(end - d * DAY, Some(calls), &[(category, errors)]))
            .collect()
    }

    fn params() -> Params {
        Params {
            window_secs: 10 * DAY,
            min_sessions: 3,
            alpha: 0.05,
        }
    }

    #[test]
    fn a_declared_target_that_dropped_sharply_is_improved() {
        let mut samples = days(B, 8, 100, "edit-precondition", 6);
        samples.extend(days(B + 9 * DAY, 8, 100, "edit-precondition", 0));
        let rule = change("edit-needs-read", B, &["edit-precondition"]);
        let effect = compare(&rule, &[], &samples, B + 9 * DAY, &params());
        let row = &effect.categories[0];
        assert_eq!((row.before, row.after), (48, 0));
        assert_eq!(row.verdict, Verdict::Improved);
        assert_eq!(row.flag, None);
    }

    #[test]
    fn a_declared_target_that_did_not_drop_is_a_hook_candidate() {
        let mut samples = days(B, 6, 100, "path-not-found", 2);
        samples.extend(days(B + 7 * DAY, 6, 100, "path-not-found", 3));
        let rule = change("scratchpad", B, &["path-not-found"]);
        let effect = compare(&rule, &[], &samples, B + 7 * DAY, &params());
        let row = &effect.categories[0];
        assert_eq!(row.verdict, Verdict::NoImprovement);
        assert_eq!(row.flag, Some(Flag::ReplaceWithHook));
    }

    #[test]
    fn a_rule_whose_blocks_did_not_drop_is_not_told_to_become_a_hook() {
        // The failures are a hook's own blocks: enforcement already exists, and
        // what did not work is the prose meant to stop the attempts.
        let mut samples = days(B, 6, 100, "blocked-by-hook", 2);
        samples.extend(days(B + 7 * DAY, 6, 100, "blocked-by-hook", 3));
        let rule = change("text-processing", B, &["blocked-by-hook"]);
        let effect = compare(&rule, &[], &samples, B + 7 * DAY, &params());
        assert_eq!(effect.categories[0].flag, Some(Flag::HookAlreadyBlocks));
    }

    #[test]
    fn a_rule_that_loads_only_sometimes_is_never_flagged() {
        // A `paths:` rule was absent from the sessions that never touched its
        // files; those sessions dilute the rate, so "it did not help" would be
        // a claim the data cannot back.
        let mut samples = days(B, 6, 100, "path-not-found", 2);
        samples.extend(days(B + 7 * DAY, 6, 100, "path-not-found", 3));
        let mut rule = change("conditional", B, &["path-not-found"]);
        rule.always_on = false;
        let effect = compare(&rule, &[], &samples, B + 7 * DAY, &params());
        assert_eq!(effect.categories[0].verdict, Verdict::NoImprovement);
        assert_eq!(effect.categories[0].flag, None);
    }

    #[test]
    fn a_side_that_made_no_tool_calls_had_no_chance_to_fail() {
        // Conversation-only sessions after the change cannot show an
        // improvement: nothing was attempted that could have failed.
        let mut samples = days(B, 6, 100, "timeout", 5);
        samples.extend(days(B + 7 * DAY, 6, 0, "timeout", 0));
        let effect = compare(
            &change("r", B, &["timeout"]),
            &[],
            &samples,
            B + 7 * DAY,
            &params(),
        );
        assert_eq!(effect.categories[0].verdict, Verdict::TooFew);
    }

    #[test]
    fn a_small_drop_is_unclear_not_improved() {
        let mut samples = days(B, 5, 100, "path-not-found", 2);
        samples.extend(days(B + 5 * DAY, 4, 100, "path-not-found", 1));
        let rule = change("scratchpad", B, &["path-not-found"]);
        let effect = compare(&rule, &[], &samples, B + 5 * DAY, &params());
        let row = &effect.categories[0];
        assert_eq!((row.before, row.after), (10, 4));
        assert_eq!(row.verdict, Verdict::Unclear);
        assert!(row.p_value.is_some_and(|p| p >= 0.05));
    }

    #[test]
    fn a_change_made_today_has_too_few_sessions_after_it() {
        let mut samples = days(B, 8, 100, "path-not-found", 2);
        samples.push(session(B + 3600, Some(100), &[("path-not-found", 1)]));
        let rule = change("scratchpad", B, &["path-not-found"]);
        let effect = compare(&rule, &[], &samples, B + 2 * 3600, &params());
        assert_eq!(effect.after.end - effect.after.start, 2 * 3600);
        assert_eq!(effect.categories[0].verdict, Verdict::TooFew);
        assert_eq!(effect.categories[0].flag, None);
    }

    #[test]
    fn windows_are_equal_and_bounded_by_the_requested_span() {
        let mut samples = days(B, 30, 10, "timeout", 0);
        samples.extend(days(B + 30 * DAY, 30, 10, "timeout", 0));
        let effect = compare(&change("r", B, &[]), &[], &samples, B + 30 * DAY, &params());
        assert_eq!((effect.before.start, effect.before.end), (B - 10 * DAY, B));
        assert_eq!((effect.after.start, effect.after.end), (B, B + 10 * DAY));
    }

    #[test]
    fn the_before_window_shrinks_to_the_first_stored_session() {
        let mut samples = vec![session(B - 4 * DAY, Some(10), &[])];
        samples.extend(days(B + 30 * DAY, 30, 10, "timeout", 0));
        let effect = compare(&change("r", B, &[]), &[], &samples, B + 30 * DAY, &params());
        assert_eq!(effect.before.end - effect.before.start, 4 * DAY);
        assert_eq!(effect.after.end - effect.after.start, 4 * DAY);
    }

    #[test]
    fn a_session_belongs_wholly_to_the_side_it_started_on() {
        let samples = vec![
            session(B - 1, Some(10), &[("timeout", 1)]),
            session(B, Some(10), &[("timeout", 1)]),
        ];
        let effect = compare(&change("r", B, &[]), &[], &samples, B + DAY, &params());
        assert_eq!((effect.before.sessions, effect.after.sessions), (1, 1));
    }

    #[test]
    fn an_undeclared_surface_shows_every_actionable_category_that_occurred() {
        let samples = vec![
            session(B - DAY, Some(10), &[("timeout", 1), ("cancelled", 9)]),
            session(
                B + DAY / 2,
                Some(10),
                &[("path-not-found", 2), ("compile-error", 0)],
            ),
        ];
        let effect = compare(&change("r", B, &[]), &[], &samples, B + 2 * DAY, &params());
        let names: Vec<&str> = effect
            .categories
            .iter()
            .map(|c| c.category.as_str())
            .collect();
        assert_eq!(names, ["path-not-found", "timeout"]);
        assert!(
            effect
                .categories
                .iter()
                .all(|c| !c.declared && c.flag.is_none())
        );
    }

    #[test]
    fn rates_fall_back_to_sessions_when_a_call_count_is_missing() {
        let samples = vec![
            session(B - DAY, None, &[("timeout", 2)]),
            session(B + DAY / 2, Some(50), &[("timeout", 1)]),
        ];
        let effect = compare(
            &change("r", B, &["timeout"]),
            &[],
            &samples,
            B + 2 * DAY,
            &params(),
        );
        assert_eq!(effect.basis, Basis::Sessions);
        assert_eq!(effect.categories[0].before_per_100_calls, None);
        assert_eq!(effect.categories[0].after_per_100_calls, Some(2.0));
    }

    #[test]
    fn the_per_call_rate_judges_when_sessions_got_longer() {
        // Per session the failures doubled, but the sessions made four times
        // the calls: per call, the rate halved.
        let mut samples = days(B, 6, 100, "timeout", 4);
        samples.extend(days(B + 7 * DAY, 6, 400, "timeout", 8));
        let effect = compare(
            &change("r", B, &["timeout"]),
            &[],
            &samples,
            B + 7 * DAY,
            &params(),
        );
        assert_eq!(effect.basis, Basis::ToolCalls);
        assert_ne!(effect.categories[0].verdict, Verdict::NoImprovement);
    }

    #[test]
    fn a_project_rule_is_judged_on_its_own_project_only() {
        let mut other = session(B + DAY, Some(10), &[("timeout", 5)]);
        other.project = "elsewhere".to_string();
        let samples = vec![session(B - DAY, Some(10), &[("timeout", 1)]), other];
        let mut rule = change("r", B, &["timeout"]);
        rule.scope = Scope::Project("example".to_string());
        let effect = compare(&rule, &[], &samples, B + 2 * DAY, &params());
        assert_eq!(effect.after.sessions, 0);
    }

    #[test]
    fn other_changes_inside_the_span_are_named_as_confounders() {
        let samples = days(B + 5 * DAY, 10, 10, "timeout", 0);
        let rule = change("a", B, &[]);
        let all = vec![
            rule.clone(),
            change("same-commit", B, &[]),
            change("long-ago", B - 50 * DAY, &[]),
        ];
        let effect = compare(&rule, &all, &samples, B + 5 * DAY, &params());
        assert_eq!(effect.concurrent, ["rule/same-commit"]);
    }

    #[test]
    fn confounders_are_named_once_nearest_first_and_never_include_the_surface_itself() {
        let samples = days(B + 5 * DAY, 10, 10, "timeout", 0);
        let rule = change("a", B, &[]);
        // A project's own `rule/a` is a different file (copies of one file
        // are merged before this point), so it is a confounder like any other.
        let mut namesake = rule.clone();
        namesake.scope = Scope::Project("example".to_string());
        namesake.changed_epoch = B + DAY;
        let mut far_copy = change("far", B + 3 * DAY, &[]);
        far_copy.scope = Scope::Project("example".to_string());
        let all = vec![
            rule.clone(),
            namesake,
            change("far", B + 3 * DAY, &[]),
            far_copy,
            change("near", B - 60, &[]),
        ];
        let effect = compare(&rule, &all, &samples, B + 5 * DAY, &params());
        assert_eq!(
            effect.concurrent,
            [
                "rule/near",
                "rule/a (project example)",
                "rule/far",
                "rule/far (project example)"
            ]
        );
    }

    #[test]
    fn an_unknown_declared_target_is_reported_not_compared() {
        let samples = days(B + 5 * DAY, 10, 10, "timeout", 0);
        let rule = change("r", B, &["timeout", "made-up"]);
        let effect = compare(&rule, &[], &samples, B + 5 * DAY, &params());
        assert_eq!(effect.unknown_targets, ["made-up"]);
        assert_eq!(effect.categories.len(), 1);
    }

    #[test]
    fn binomial_cdf_matches_known_values() {
        assert!((binomial_cdf(0, 10, 0.5) - 1.0 / 1024.0).abs() < 1e-12);
        assert!((binomial_cdf(5, 10, 0.5) - 0.623_046_875).abs() < 1e-9);
        assert!(binomial_cdf(0, 2000, 0.5) < 1e-300 || binomial_cdf(0, 2000, 0.5) == 0.0);
    }
}
