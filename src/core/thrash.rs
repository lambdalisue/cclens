//! Detect thrash: bursts of edits to the *same* file in a short window. A high
//! lifetime edit count (a hotspot) can be healthy active development; what
//! signals struggle is re-editing one file many times back-to-back — Claude
//! couldn't get it right and kept retrying. This is the "where did it get stuck"
//! signal that a flat edit count cannot give.

use std::collections::HashMap;

use crate::core::friction::{ErrorCategory, masked_failure};
use crate::core::path::basename;

/// One edit to one file, with the work context that makes it comparable to
/// another edit: the same path touched by a different session is different work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEdit {
    pub session_id: String,
    pub project: String,
    /// The edited file's full path — not its basename. Two `route.ts` files in
    /// different directories are different files.
    pub path: String,
    pub epoch: i64,
}

/// A burst of rapid re-edits to one file by one session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThrashEpisode {
    pub project: String,
    pub session_id: String,
    pub path: String,
    pub edits: u32,
    pub start_epoch: i64,
    pub end_epoch: i64,
}

impl ThrashEpisode {
    pub fn span_secs(&self) -> i64 {
        self.end_epoch - self.start_epoch
    }

    /// The edited file's basename — what a report shows; the full `path`
    /// disambiguates when two projects share a name.
    pub fn file(&self) -> &str {
        basename(&self.path)
    }
}

/// Find thrash episodes: per `(session, path)`, maximal runs of edits where each
/// edit is within `gap_secs` of the previous, keeping only runs of at least
/// `min_edits`. Sorted by edit count, densest first.
///
/// The grouping key is what makes an episode mean "one agent kept retrying this
/// file". Keying on the basename alone merged unrelated work — parallel sessions
/// and worktrees editing a same-named file were reported as one agent stuck in a
/// loop, which is exactly the signal this is supposed to detect. `session_id`
/// implies the project, and the path is unique within it, so the pair is
/// sufficient.
///
/// The trade is deliberate: thrash that genuinely continues across a resume or
/// `/clear` now splits into one episode per session. A split under-reports a
/// real burst; a merge invents one that never happened.
pub fn detect_thrash(edits: &[FileEdit], gap_secs: i64, min_edits: u32) -> Vec<ThrashEpisode> {
    let mut by_file: HashMap<(&str, &str), Vec<i64>> = HashMap::new();
    for edit in edits {
        by_file
            .entry((edit.session_id.as_str(), edit.path.as_str()))
            .or_default()
            .push(edit.epoch);
    }
    // The project travels with the session, so one lookup per session suffices.
    let project_of: HashMap<&str, &str> = edits
        .iter()
        .map(|edit| (edit.session_id.as_str(), edit.project.as_str()))
        .collect();

    let mut episodes = Vec::new();
    for ((session_id, path), mut times) in by_file {
        let project = project_of.get(session_id).copied().unwrap_or_default();
        let at = |edits, start, end| Group {
            project,
            session_id,
            path,
            edits,
            start,
            end,
        };
        times.sort_unstable();
        let mut start = times[0];
        let mut prev = times[0];
        let mut count: u32 = 1;
        for &t in &times[1..] {
            if t - prev <= gap_secs {
                count += 1;
            } else {
                push_if(&mut episodes, at(count, start, prev), min_edits);
                start = t;
                count = 1;
            }
            prev = t;
        }
        push_if(&mut episodes, at(count, start, prev), min_edits);
    }

    episodes.sort_by(|a, b| {
        b.edits
            .cmp(&a.edits)
            .then(b.span_secs().cmp(&a.span_secs()))
            // Ties would otherwise order by HashMap iteration — unstable output.
            .then(a.path.cmp(&b.path))
            .then(a.session_id.cmp(&b.session_id))
    });
    episodes
}

/// One candidate run, before the `min_edits` cut.
struct Group<'a> {
    project: &'a str,
    session_id: &'a str,
    path: &'a str,
    edits: u32,
    start: i64,
    end: i64,
}

fn push_if(out: &mut Vec<ThrashEpisode>, group: Group<'_>, min: u32) {
    if group.edits >= min {
        out.push(ThrashEpisode {
            project: group.project.to_string(),
            session_id: group.session_id.to_string(),
            path: group.path.to_string(),
            edits: group.edits,
            start_epoch: group.start,
            end_epoch: group.end,
        });
    }
}

/// How one Bash run ended: passed, or failed with a friction category.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BashOutcome {
    Ok,
    Failed(ErrorCategory),
}

impl BashOutcome {
    /// The stored form: `ok`, or the failure's category label.
    pub fn label(self) -> &'static str {
        match self {
            BashOutcome::Ok => "ok",
            BashOutcome::Failed(category) => category.label(),
        }
    }

    pub fn from_label(label: &str) -> Option<BashOutcome> {
        match label {
            "" => None,
            "ok" => Some(BashOutcome::Ok),
            other => Some(BashOutcome::Failed(ErrorCategory::from_label(other))),
        }
    }

    pub fn is_failure(self) -> bool {
        matches!(self, BashOutcome::Failed(_))
    }
}

/// The outcome of a **check** run: the recorded one, except that a clean exit
/// whose output shows the runner failing counts as that failure
/// (`friction::masked_failure` — a piped `| tail` hides the exit status).
///
/// A recorded failure is the check's only when the check is the command's last
/// segment. After `| grep …` or `&& rg …` the status belongs to that command —
/// `grep` exits 1 when a passing run printed nothing to match — so the failure
/// is read from the runner's output alone, and is unknown without it.
pub fn check_outcome(
    command: &str,
    recorded: Option<BashOutcome>,
    output: &str,
) -> Option<BashOutcome> {
    let masked = masked_failure(output).map(BashOutcome::Failed);
    // After `|| true` a clean exit says nothing; after `true || cargo test`
    // the check may never have run.
    let segments = command_segments(command);
    let or_list = segments.iter().any(|s| s == OR_LIST);
    let background = segments.iter().any(|s| s == BACKGROUND);
    match recorded {
        _ if background => masked,
        Some(BashOutcome::Ok) if or_list => masked,
        Some(BashOutcome::Ok) => Some(masked.unwrap_or(BashOutcome::Ok)),
        Some(BashOutcome::Failed(category))
            if !category.is_check_failure() || !check_owns_exit(command) =>
        {
            masked
        }
        other => other,
    }
}

/// Whether the command's last segment runs a check, so its exit status is
/// the check's.
/// The segment `command_segments` puts where a `||` stood.
const OR_LIST: &str = "||";

/// The segment `command_segments` puts where a lone `&` stood.
const BACKGROUND: &str = "&";

/// Whether the command's exit status is the check's: the last segment runs a
/// check, and everything before it is another check or setup that does not
/// fail the way a check does (directory changes, environment — not a sourced
/// script, which can run anything). After
/// `false && cargo test` the status may be `false`'s, with no test run.
fn check_owns_exit(command: &str) -> bool {
    let segments = command_segments(command);
    let segments: Vec<Vec<&str>> = segments
        .iter()
        .map(|segment| segment.split_whitespace().collect::<Vec<_>>())
        .filter(|words| !words.is_empty())
        .collect();
    let Some((last, before)) = segments.split_last() else {
        return false;
    };
    let is_check = |words: &Vec<&str>| segment_check(words.clone()).is_some();
    let is_setup = |words: &Vec<&str>| {
        matches!(words[0], "cd" | "pushd" | "export" | "set")
            || words.iter().all(|w| is_assignment(w))
    };
    is_check(last)
        && before
            .iter()
            .all(|words| is_check(words) || is_setup(words))
}

/// The check a Bash command runs, normalised so two runs of one check compare
/// equal however they were spelled: `cargo test`, `just check`, `npm run lint`.
/// `None` when no segment invokes a known check runner (`docs/specs/events.md`).
pub fn check_key(command: &str) -> Option<String> {
    let keys: Vec<String> = command_segments(command)
        .iter()
        .filter_map(|segment| segment_check(segment.split_whitespace().collect()))
        .collect();
    let mut unique: Vec<String> = Vec::new();
    for key in keys {
        if !unique.contains(&key) {
            unique.push(key);
        }
    }
    (!unique.is_empty()).then(|| unique.join(" && "))
}

/// Split a command line at `&`, `|`, `;` and newlines that the shell would act
/// on — not those inside quotes or escaped — so a message that merely mentions
/// a check (`jj describe -m "fix; cargo test passes"`) is not read as running
/// it, and its success cannot close a loop the real check is still in.
///
/// A heredoc body is data, not commands: its lines are dropped up to the
/// closing delimiter, so a file written with `cat <<'EOF'` that mentions
/// `cargo test` does not run it either.
fn command_segments(command: &str) -> Vec<String> {
    let mut segments = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut heredocs: Vec<(String, bool)> = Vec::new();
    let mut chars = command.chars().peekable();
    while let Some(c) = chars.next() {
        if quote.is_none() && c == '<' && chars.peek() == Some(&'<') {
            chars.next();
            let strip_tabs = chars.next_if_eq(&'-').is_some();
            while chars.next_if(|c| *c == ' ').is_some() {}
            let word: String = std::iter::from_fn(|| {
                chars.next_if(|c| !c.is_whitespace() && !"|&;<>()".contains(*c))
            })
            .collect();
            let delimiter = word.trim_matches(['\'', '"']).to_string();
            if !delimiter.is_empty() {
                heredocs.push((delimiter, strip_tabs));
            }
            continue;
        }
        if quote.is_none() && c == '\n' {
            // Bodies follow the line that opened them, one per `<<`, in order.
            for (delimiter, strip_tabs) in std::mem::take(&mut heredocs) {
                loop {
                    let line: String =
                        std::iter::from_fn(|| chars.next_if(|c| *c != '\n')).collect();
                    let at_end = chars.next().is_none();
                    let line = if strip_tabs {
                        line.trim_start_matches('\t')
                    } else {
                        &line
                    };
                    if line == delimiter || at_end {
                        break;
                    }
                }
            }
        }
        match (quote, c) {
            (Some('\''), '\'') | (Some('"'), '"') => quote = None,
            (Some('"'), '\\') | (None, '\\') => {
                current.push(c);
                if let Some(next) = chars.next() {
                    current.push(next);
                }
                continue;
            }
            (None, '\'' | '"') => quote = Some(c),
            // A comment runs to the end of the line; the newline still ends
            // the segment.
            (None, '#') if current.is_empty() || current.ends_with(char::is_whitespace) => {
                while chars.next_if(|c| *c != '\n').is_some() {}
                continue;
            }
            // `a || b` is kept as its own segment: it decouples the command's
            // status from any check in it (`check_outcome`).
            (None, '|') if chars.peek() == Some(&'|') => {
                chars.next();
                segments.push(std::mem::take(&mut current));
                segments.push(OR_LIST.to_string());
                continue;
            }
            // `2>&1`, `>&2` and `&>` redirect; they separate nothing.
            (None, '&') if current.ends_with('>') || chars.peek() == Some(&'>') => {}
            (None, '&') if chars.next_if_eq(&'&').is_some() => {
                segments.push(std::mem::take(&mut current));
                continue;
            }
            // A lone `&` sends what precedes it to the background: the
            // command's status is not that job's.
            (None, '&') => {
                segments.push(std::mem::take(&mut current));
                segments.push(BACKGROUND.to_string());
                continue;
            }
            (None, '|' | ';' | '\n') => {
                segments.push(std::mem::take(&mut current));
                continue;
            }
            _ => {}
        }
        current.push(c);
    }
    segments.push(current);
    segments
}

/// The check one pipeline segment runs, after peeling what does not change
/// which check it is: environment assignments, redirections, and wrappers.
fn segment_check(mut words: Vec<&str>) -> Option<String> {
    words.retain(|w| !is_redirection(w));
    loop {
        match words.as_slice() {
            [first, ..] if is_assignment(first) => {
                words.remove(0);
            }
            ["env" | "time" | "command" | "exec", ..] => {
                words.remove(0);
            }
            ["timeout" | "gtimeout", ..] => {
                words.remove(0);
                while words.first().is_some_and(|w| w.starts_with('-')) {
                    words.remove(0);
                }
                if !words.is_empty() {
                    words.remove(0);
                }
            }
            ["nix", "develop", ..] => {
                let at = words.iter().position(|w| *w == "-c" || *w == "--command")?;
                words.drain(..=at);
            }
            _ => break,
        }
    }
    runner_key(&words)
}

fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| {
        !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    })
}

fn is_redirection(word: &str) -> bool {
    word.trim_start_matches(|c: char| c.is_ascii_digit())
        .starts_with(['>', '<'])
}

/// The key for a command line that starts at its program, if that program is
/// a known check runner. The list is curated: a runner missing from it
/// under-reports a loop, which is the safe direction.
fn runner_key(words: &[&str]) -> Option<String> {
    let program = words.first()?.rsplit('/').next()?;
    // Options whose value is a separate word; without this the value (often a
    // path) would be read as the subcommand.
    let takes_value: &[&str] = match program {
        "just" => &["--justfile", "-f", "--working-directory", "-d"],
        "make" => &["-C", "-f", "--directory", "--file", "-j"],
        "cargo" => &["-p", "--package", "--manifest-path", "-j", "--target-dir"],
        _ => &[],
    };
    let mut args: Vec<&str> = Vec::new();
    let mut rest = words[1..].iter().copied();
    while let Some(word) = rest.next() {
        if program == "just" && word == "--set" {
            // `--set <variable> <value>`
            rest.next();
            rest.next();
        } else if takes_value.contains(&word) {
            rest.next();
        } else if !word.starts_with(['-', '+']) {
            args.push(word);
        }
    }
    let sub = |allowed: &[&str]| {
        args.first()
            .filter(|a| allowed.contains(a))
            .map(|a| format!("{program} {a}"))
    };
    match program {
        "cargo" => sub(&["test", "build", "check", "clippy", "nextest", "fmt"]),
        "just" | "make" | "task" => Some(match args.first() {
            Some(recipe) => format!("{program} {recipe}"),
            None => program.to_string(),
        }),
        "npm" | "pnpm" | "yarn" | "bun" => match args.as_slice() {
            ["run", script, ..] => Some(format!("{program} run {script}")),
            [script, ..] if ["test", "lint", "build", "check", "typecheck"].contains(script) => {
                Some(format!("{program} {script}"))
            }
            _ => None,
        },
        "deno" => sub(&["test", "check", "lint", "fmt"]),
        "go" => sub(&["test", "build", "vet"]),
        "dotnet" | "swift" | "zig" => sub(&["test", "build"]),
        "nix" => match args.as_slice() {
            ["build", ..] => Some("nix build".to_string()),
            ["flake", "check", ..] => Some("nix flake check".to_string()),
            _ => None,
        },
        "python" | "python3" => {
            let at = words.iter().position(|w| *w == "-m")?;
            runner_key(&words[at + 1..])
        }
        "pytest" | "tsc" | "eslint" | "mypy" | "vitest" | "jest" | "biome" | "ruff"
        | "clippy-driver" | "shellcheck" => Some(program.to_string()),
        _ => None,
    }
}

/// One step of a session's work, in transcript order: an edit, or a run of a
/// check that passed or failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkStep {
    pub session_id: String,
    pub project: String,
    pub epoch: i64,
    pub kind: StepKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepKind {
    Edit,
    Check { key: String, failed: bool },
}

/// One check failing round after round in one session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckLoop {
    pub project: String,
    pub session_id: String,
    pub check: String,
    /// Failing runs after the first that followed an edit — fix attempts
    /// that did not fix it.
    pub rounds: u32,
    /// Every failing run, bare re-runs included.
    pub failures: u32,
    /// Whether the check passed before the session ended.
    pub resolved: bool,
    pub start_epoch: i64,
    pub end_epoch: i64,
}

impl CheckLoop {
    pub fn span_secs(&self) -> i64 {
        self.end_epoch - self.start_epoch
    }
}

/// Find check loops: per `(session, check)`, a run of failures closed by the
/// next pass (or the session's end), kept when at least `min_rounds` of its
/// failures followed an edit. `steps` must be in transcript order; sorted
/// most rounds first. The rules are in `docs/specs/events.md`.
pub fn detect_check_loops(steps: &[WorkStep], min_rounds: u32) -> Vec<CheckLoop> {
    struct Open {
        rounds: u32,
        failures: u32,
        edited: bool,
        start: i64,
        end: i64,
        project: String,
    }
    let mut open: HashMap<(String, String), Open> = HashMap::new();
    let mut loops = Vec::new();
    let close = |(session_id, check): (String, String), o: Open, resolved, loops: &mut Vec<_>| {
        if o.rounds >= min_rounds {
            loops.push(CheckLoop {
                project: o.project,
                session_id,
                check,
                rounds: o.rounds,
                failures: o.failures,
                resolved,
                start_epoch: o.start,
                end_epoch: o.end,
            });
        }
    };
    for s in steps {
        match &s.kind {
            StepKind::Edit => {
                for ((session_id, _), o) in open.iter_mut() {
                    if *session_id == s.session_id {
                        o.edited = true;
                    }
                }
            }
            StepKind::Check { key, failed: true } => {
                let o = open
                    .entry((s.session_id.clone(), key.clone()))
                    .or_insert_with(|| Open {
                        // The opening failure is the problem, not a fix for it.
                        rounds: 0,
                        failures: 0,
                        edited: false,
                        start: s.epoch,
                        end: s.epoch,
                        project: s.project.clone(),
                    });
                o.failures += 1;
                if o.edited {
                    o.rounds += 1;
                }
                o.edited = false;
                o.end = s.epoch;
            }
            StepKind::Check { key, failed: false } => {
                let at = (s.session_id.clone(), key.clone());
                if let Some(mut o) = open.remove(&at) {
                    o.end = s.epoch;
                    close(at, o, true, &mut loops);
                }
            }
        }
    }
    for (at, o) in open {
        close(at, o, false, &mut loops);
    }
    loops.sort_by(|a, b| {
        b.rounds
            .cmp(&a.rounds)
            .then(b.failures.cmp(&a.failures))
            .then(a.session_id.cmp(&b.session_id))
            .then(a.check.cmp(&b.check))
            .then(a.start_epoch.cmp(&b.start_epoch))
    });
    loops
}

/// One Bash run, as the retry count sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxedRun {
    pub session_id: String,
    pub project: String,
    pub outcome: Option<BashOutcome>,
    pub unsandboxed: bool,
}

/// An unsandboxed run that came right after a sandboxed failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnsandboxedRetry {
    pub project: String,
    pub session_id: String,
    /// The failure it followed was itself classified as a sandbox refusal —
    /// the retries an allowlist entry would remove.
    pub after_sandbox_denial: bool,
}

/// Find unsandboxed retries in Bash runs given in transcript order: a run with
/// the sandbox disabled whose previous run in the same session was sandboxed
/// and failed (`docs/specs/events.md`).
pub fn count_unsandboxed_retries(runs: &[SandboxedRun]) -> Vec<UnsandboxedRetry> {
    let mut previous: HashMap<&str, &SandboxedRun> = HashMap::new();
    let mut retries = Vec::new();
    for run in runs {
        if let Some(prev) = previous.insert(run.session_id.as_str(), run)
            && run.unsandboxed
            && !prev.unsandboxed
            && let Some(BashOutcome::Failed(category)) = prev.outcome
        {
            retries.push(UnsandboxedRetry {
                project: run.project.clone(),
                session_id: run.session_id.clone(),
                after_sandbox_denial: category.is_sandbox(),
            });
        }
    }
    retries
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edit(path: &str, epoch: i64) -> FileEdit {
        edit_in("s1", "demo", path, epoch)
    }

    fn edit_in(session_id: &str, project: &str, path: &str, epoch: i64) -> FileEdit {
        FileEdit {
            session_id: session_id.to_string(),
            project: project.to_string(),
            path: path.to_string(),
            epoch,
        }
    }

    #[test]
    fn rapid_reedits_to_one_file_are_a_thrash_episode() {
        // Three edits within 60s of each other.
        let edits = [edit("/p/a.rs", 0), edit("/p/a.rs", 30), edit("/p/a.rs", 50)];
        let episodes = detect_thrash(&edits, 60, 3);
        assert_eq!(episodes.len(), 1);
        assert_eq!(episodes[0].edits, 3);
        assert_eq!(episodes[0].span_secs(), 50);
        assert_eq!(episodes[0].file(), "a.rs");
    }

    #[test]
    fn the_reported_file_is_a_basename_on_either_separator() {
        let edits = [
            edit(r"C:\example\p\a.rs", 0),
            edit(r"C:\example\p\a.rs", 30),
            edit(r"C:\example\p\a.rs", 50),
        ];
        let episodes = detect_thrash(&edits, 60, 3);
        assert_eq!(episodes[0].file(), "a.rs");
    }

    #[test]
    fn edits_spread_out_are_not_thrash() {
        // Same count, but each edit is far from the last — healthy development.
        let edits = [
            edit("/p/a.rs", 0),
            edit("/p/a.rs", 1000),
            edit("/p/a.rs", 2000),
        ];
        assert!(detect_thrash(&edits, 60, 3).is_empty());
    }

    #[test]
    fn a_run_below_the_minimum_is_ignored() {
        let edits = [edit("/p/a.rs", 0), edit("/p/a.rs", 10)];
        assert!(detect_thrash(&edits, 60, 3).is_empty());
    }

    #[test]
    fn separate_bursts_of_the_same_file_are_separate_episodes() {
        let edits = [
            edit("/p/a.rs", 0),
            edit("/p/a.rs", 10),
            edit("/p/a.rs", 20), // burst 1
            edit("/p/a.rs", 5000),
            edit("/p/a.rs", 5010),
            edit("/p/a.rs", 5020), // burst 2 after a long gap
        ];
        let episodes = detect_thrash(&edits, 60, 3);
        assert_eq!(episodes.len(), 2);
    }

    #[test]
    fn concurrent_sessions_editing_the_same_file_are_not_one_episode() {
        // Two agents working the same file in parallel. Interleaved in time, so
        // merging them fabricates a single agent retrying six times.
        let edits = [
            edit_in("s1", "demo", "/p/a.rs", 0),
            edit_in("s2", "demo", "/p/a.rs", 5),
            edit_in("s1", "demo", "/p/a.rs", 10),
            edit_in("s2", "demo", "/p/a.rs", 15),
            edit_in("s1", "demo", "/p/a.rs", 20),
            edit_in("s2", "demo", "/p/a.rs", 25),
        ];
        let episodes = detect_thrash(&edits, 60, 3);
        assert_eq!(episodes.len(), 2);
        assert!(episodes.iter().all(|e| e.edits == 3));
    }

    #[test]
    fn same_basename_in_different_directories_is_not_one_episode() {
        // One session touching two different route.ts files is not thrash.
        let edits = [
            edit("/p/users/route.ts", 0),
            edit("/p/posts/route.ts", 5),
            edit("/p/users/route.ts", 10),
            edit("/p/posts/route.ts", 15),
        ];
        assert!(detect_thrash(&edits, 60, 3).is_empty());
    }

    #[test]
    fn a_check_key_is_the_runner_and_its_subcommand() {
        assert_eq!(
            check_key("cargo test -p cclens --lib").as_deref(),
            Some("cargo test")
        );
        assert_eq!(check_key("just check").as_deref(), Some("just check"));
        assert_eq!(
            check_key("npm run lint -- --fix").as_deref(),
            Some("npm run lint")
        );
        assert_eq!(check_key("pnpm test").as_deref(), Some("pnpm test"));
        assert_eq!(
            check_key("pytest tests/x.py -k y").as_deref(),
            Some("pytest")
        );
        assert_eq!(check_key("python3 -m pytest -q").as_deref(), Some("pytest"));
    }

    #[test]
    fn a_check_key_ignores_directory_changes_env_wrappers_and_pipes() {
        assert_eq!(
            check_key("cd /tmp/example && RUST_LOG=debug timeout 600 cargo test 2>&1 | tail -20")
                .as_deref(),
            Some("cargo test")
        );
        assert_eq!(
            check_key("nix develop -c cargo clippy --all-targets -- -D warnings").as_deref(),
            Some("cargo clippy")
        );
    }

    #[test]
    fn a_just_recipe_is_found_past_options_that_take_a_value() {
        assert_eq!(
            check_key("just --justfile /tmp/example/justfile ci").as_deref(),
            Some("just ci")
        );
        assert_eq!(
            check_key("just -f /tmp/example/justfile -d /tmp/example").as_deref(),
            Some("just")
        );
        assert_eq!(
            check_key("make -C /tmp/example test").as_deref(),
            Some("make test")
        );
        assert_eq!(
            check_key("just --set mode ci check").as_deref(),
            Some("just check")
        );
    }

    #[test]
    fn a_compound_check_keys_on_every_check_it_runs() {
        assert_eq!(
            check_key("cargo fmt --check && cargo clippy; echo done").as_deref(),
            Some("cargo fmt && cargo clippy")
        );
    }

    #[test]
    fn a_separator_inside_quotes_does_not_start_a_command() {
        assert_eq!(check_key(r#"echo "done; cargo test --lib""#), None);
        assert_eq!(check_key("jj describe -m 'fix | cargo test passes'"), None);
        assert_eq!(check_key(r#"echo a\;cargo test"#), None);
        assert_eq!(
            check_key(r#"echo "a;b" && cargo test"#).as_deref(),
            Some("cargo test")
        );
    }

    #[test]
    fn a_heredoc_body_is_data_not_commands() {
        let command = "cat > /tmp/example/justfile <<'EOF'\ntest:\n    cargo test\nEOF\nls";
        assert_eq!(check_key(command), None);
        let command = "cat <<-END | wc -l\n\tcargo test\n\tEND\ncargo clippy";
        assert_eq!(check_key(command).as_deref(), Some("cargo clippy"));
    }

    #[test]
    fn a_command_that_runs_no_known_check_has_no_key() {
        assert_eq!(check_key("rg -n foo src"), None);
        assert_eq!(check_key("cd /tmp/example && ls"), None);
        assert_eq!(check_key("cargo add serde"), None);
    }

    fn step(kind: StepKind, epoch: i64) -> WorkStep {
        WorkStep {
            session_id: "s1".to_string(),
            project: "demo".to_string(),
            epoch,
            kind,
        }
    }

    fn run(key: &str, ok: bool) -> StepKind {
        StepKind::Check {
            key: key.to_string(),
            failed: !ok,
        }
    }

    /// A first failing run of `key`, then `rounds` fix attempts — an edit and
    /// another failing run each.
    fn failing_rounds(key: &str, rounds: i64) -> Vec<WorkStep> {
        let first = step(run(key, false), 5);
        let fixes = (1..=rounds).flat_map(|i| {
            [
                step(StepKind::Edit, i * 10),
                step(run(key, false), i * 10 + 5),
            ]
        });
        std::iter::once(first).chain(fixes).collect()
    }

    #[test]
    fn three_failed_fix_rounds_after_the_first_failure_are_a_check_loop() {
        let mut steps = failing_rounds("cargo test", 3);
        steps.push(step(run("cargo test", true), 100));
        let loops = detect_check_loops(&steps, 3);
        assert_eq!(loops.len(), 1);
        assert_eq!(loops[0].check, "cargo test");
        assert_eq!(loops[0].rounds, 3);
        assert_eq!(loops[0].failures, 4);
        assert!(loops[0].resolved);
        assert_eq!(loops[0].start_epoch, 5);
        assert_eq!(loops[0].end_epoch, 100);
    }

    #[test]
    fn the_first_failure_is_not_a_fix_round() {
        // An edit before the first run is the work being tested, not a fix.
        let mut steps = vec![step(StepKind::Edit, 0)];
        steps.extend(failing_rounds("cargo test", 2));
        assert!(detect_check_loops(&steps, 3).is_empty());
    }

    #[test]
    fn a_rerun_without_an_edit_is_a_failure_but_not_a_round() {
        let mut steps = failing_rounds("just check", 3);
        steps.push(step(run("just check", false), 50));
        steps.push(step(run("just check", false), 51));
        let loops = detect_check_loops(&steps, 3);
        assert_eq!(loops[0].rounds, 3);
        assert_eq!(loops[0].failures, 6);
        // Never passed before the session ended.
        assert!(!loops[0].resolved);
    }

    #[test]
    fn a_pass_closes_the_episode_and_a_later_failure_starts_another() {
        let mut steps = failing_rounds("cargo test", 1);
        steps.push(step(run("cargo test", true), 30));
        steps.extend(failing_rounds("cargo test", 1).into_iter().map(|mut s| {
            s.epoch += 100;
            s
        }));
        assert!(detect_check_loops(&steps, 2).is_empty());
    }

    #[test]
    fn different_checks_are_different_episodes() {
        let mut steps = failing_rounds("cargo test", 3);
        steps.push(step(run("cargo clippy", true), 100));
        let loops = detect_check_loops(&steps, 3);
        assert_eq!(loops.len(), 1);
        assert!(!loops[0].resolved, "another check passing resolves nothing");
    }

    #[test]
    fn parallel_sessions_running_one_check_are_not_one_loop() {
        // Interleaved, the two sessions read as one agent failing four rounds.
        let steps: Vec<WorkStep> = (0..8)
            .flat_map(|i| {
                [
                    step(StepKind::Edit, i * 10),
                    step(run("cargo test", false), i * 10 + 5),
                ]
            })
            .enumerate()
            .map(|(n, mut s)| {
                s.session_id = if (n / 2) % 2 == 0 { "s1" } else { "s2" }.to_string();
                s
            })
            .collect();
        let loops = detect_check_loops(&steps, 3);
        assert_eq!(loops.len(), 2);
        assert!(loops.iter().all(|l| l.rounds == 3));
        assert!(detect_check_loops(&steps, 4).is_empty());
    }

    fn bash(
        session: &str,
        failed: Option<Option<ErrorCategory>>,
        unsandboxed: bool,
    ) -> SandboxedRun {
        SandboxedRun {
            session_id: session.to_string(),
            project: "demo".to_string(),
            outcome: failed.map(|f| match f {
                Some(category) => BashOutcome::Failed(category),
                None => BashOutcome::Ok,
            }),
            unsandboxed,
        }
    }

    #[test]
    fn an_unsandboxed_run_right_after_a_sandboxed_failure_is_a_retry() {
        let runs = [
            bash("s1", Some(Some(ErrorCategory::SandboxWrite)), false),
            bash("s1", Some(None), true),
            bash("s1", Some(Some(ErrorCategory::CommandFailed)), false),
            bash("s1", Some(None), true),
        ];
        let retries = count_unsandboxed_retries(&runs);
        assert_eq!(retries.len(), 2);
        assert_eq!(retries.iter().filter(|r| r.after_sandbox_denial).count(), 1);
        assert_eq!(retries[0].project, "demo");
    }

    #[test]
    fn an_unsandboxed_run_after_a_success_or_another_unsandboxed_run_is_not_a_retry() {
        let runs = [
            bash("s1", Some(None), false),
            bash("s1", Some(None), true),
            bash("s1", Some(Some(ErrorCategory::CommandFailed)), true),
            bash("s1", Some(None), true),
        ];
        assert!(count_unsandboxed_retries(&runs).is_empty());
    }

    #[test]
    fn a_failure_in_another_session_prompts_no_retry() {
        let runs = [
            bash("s1", Some(Some(ErrorCategory::SandboxWrite)), false),
            bash("s2", Some(None), true),
        ];
        assert!(count_unsandboxed_retries(&runs).is_empty());
    }

    #[test]
    fn a_check_that_exited_clean_but_printed_a_failure_failed() {
        let masked = "test result: FAILED. 0 passed; 1 failed";
        assert_eq!(
            check_outcome("cargo test", Some(BashOutcome::Ok), masked),
            Some(BashOutcome::Failed(ErrorCategory::TestFailure))
        );
        // A recorded failure keeps its own category; no result stays unknown.
        let failed = Some(BashOutcome::Failed(ErrorCategory::Timeout));
        assert_eq!(check_outcome("cargo test", failed, masked), failed);
        assert_eq!(check_outcome("cargo test", None, masked), None);
        assert_eq!(
            check_outcome("cargo test", Some(BashOutcome::Ok), "test result: ok"),
            Some(BashOutcome::Ok)
        );
    }

    #[test]
    fn a_redirection_ampersand_is_not_a_separator() {
        let failed = Some(BashOutcome::Failed(ErrorCategory::CommandFailed));
        assert_eq!(check_outcome("pytest 2>&1", failed, ""), failed);
        assert_eq!(
            check_outcome("cargo test &> /tmp/example/log", failed, ""),
            failed
        );
        assert_eq!(check_key("cargo test >&2").as_deref(), Some("cargo test"));
    }

    #[test]
    fn a_failure_the_check_did_not_produce_is_not_the_checks_failure() {
        // `cd /missing && cargo test` never ran the tests.
        let missing = Some(BashOutcome::Failed(ErrorCategory::PathNotFound));
        assert_eq!(
            check_outcome("cd /tmp/example/missing && cargo test", missing, ""),
            None
        );
        let refused = Some(BashOutcome::Failed(ErrorCategory::SandboxWrite));
        assert_eq!(check_outcome("cargo test", refused, ""), None);
        // …unless the runner itself says it failed.
        assert_eq!(
            check_outcome(
                "cargo test",
                missing,
                "test result: FAILED. 0 passed; 1 failed"
            ),
            Some(BashOutcome::Failed(ErrorCategory::TestFailure))
        );
    }

    #[test]
    fn a_failure_before_the_check_ran_is_not_the_checks_failure() {
        let failed = Some(BashOutcome::Failed(ErrorCategory::CommandFailed));
        assert_eq!(check_outcome("false && cargo test", failed, ""), None);
        // Directory changes and environment setup do not fail that way.
        assert_eq!(
            check_outcome("cd /tmp/example && export A=1 && cargo test", failed, ""),
            failed
        );
        assert_eq!(
            check_outcome("cargo fmt --check && cargo test", failed, ""),
            failed
        );
    }

    #[test]
    fn a_shell_comment_runs_nothing() {
        assert_eq!(check_key("echo done # ; cargo test"), None);
        assert_eq!(
            check_key("cargo clippy # then cargo test\nls").as_deref(),
            Some("cargo clippy")
        );
        assert_eq!(
            check_key("echo a#b; cargo test").as_deref(),
            Some("cargo test")
        );
    }

    #[test]
    fn an_or_list_hides_how_the_check_ended() {
        // Neither a clean exit nor a failure says what `cargo test` did.
        let ok = Some(BashOutcome::Ok);
        assert_eq!(
            check_outcome("cargo test > /tmp/example/log 2>&1 || true", ok, ""),
            None
        );
        assert_eq!(check_outcome("true || cargo test", ok, ""), None);
        let failed = Some(BashOutcome::Failed(ErrorCategory::CommandFailed));
        assert_eq!(check_outcome("cargo test || cargo test", failed, ""), None);
        // The runner's own line still decides.
        assert_eq!(
            check_outcome(
                "cargo test 2>&1 || true",
                ok,
                "test result: FAILED. 0 passed; 1 failed"
            ),
            Some(BashOutcome::Failed(ErrorCategory::TestFailure))
        );
    }

    #[test]
    fn a_check_sent_to_the_background_has_not_ended() {
        let ok = Some(BashOutcome::Ok);
        assert_eq!(
            check_outcome("cargo test > /tmp/example/log 2>&1 &", ok, ""),
            None
        );
        assert_eq!(check_outcome("cargo test & wait", ok, ""), None);
        // `&&` is a list, not a background job.
        assert_eq!(check_outcome("cd /tmp/example && cargo test", ok, ""), ok);
    }

    #[test]
    fn a_sourced_script_may_fail_before_the_check() {
        let failed = Some(BashOutcome::Failed(ErrorCategory::CommandFailed));
        assert_eq!(
            check_outcome("source /tmp/example/env.sh && cargo test", failed, ""),
            None
        );
        assert_eq!(
            check_outcome(". /tmp/example/env.sh && cargo test", failed, ""),
            None
        );
    }

    #[test]
    fn a_failure_after_the_check_is_not_the_checks_failure() {
        // `grep` exits 1 when nothing matched — a passing run.
        let piped = "cd /tmp/example && cargo test 2>&1 | grep FAILED";
        let failed = Some(BashOutcome::Failed(ErrorCategory::CommandFailed));
        assert_eq!(check_outcome(piped, failed, ""), None);
        // The runner's own line still decides when it is there.
        assert_eq!(
            check_outcome(piped, failed, "test result: FAILED. 0 passed; 1 failed"),
            Some(BashOutcome::Failed(ErrorCategory::TestFailure))
        );
        assert_eq!(
            check_outcome("cargo test && rg missing src", failed, ""),
            None
        );
        // A check that is the command's last word owns its exit status.
        assert_eq!(
            check_outcome("cd /tmp/example && cargo test", failed, ""),
            failed
        );
    }

    #[test]
    fn an_outcome_round_trips_through_its_label() {
        for outcome in [
            BashOutcome::Ok,
            BashOutcome::Failed(ErrorCategory::SandboxNetwork),
        ] {
            assert_eq!(BashOutcome::from_label(outcome.label()), Some(outcome));
        }
        assert_eq!(BashOutcome::from_label("ok"), Some(BashOutcome::Ok));
    }

    #[test]
    fn an_episode_carries_the_project_and_session_that_produced_it() {
        let edits = [
            edit_in("s9", "acme", "/p/a.rs", 0),
            edit_in("s9", "acme", "/p/a.rs", 10),
            edit_in("s9", "acme", "/p/a.rs", 20),
        ];
        let episodes = detect_thrash(&edits, 60, 3);
        assert_eq!(episodes[0].project, "acme");
        assert_eq!(episodes[0].session_id, "s9");
        assert_eq!(episodes[0].path, "/p/a.rs");
    }
}
