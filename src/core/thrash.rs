//! Detect thrash: bursts of edits to the *same* file in a short window. A high
//! lifetime edit count (a hotspot) can be healthy active development; what
//! signals struggle is re-editing one file many times back-to-back — Claude
//! couldn't get it right and kept retrying. This is the "where did it get stuck"
//! signal that a flat edit count cannot give.

use std::collections::HashMap;

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
        self.path.rsplit('/').next().unwrap_or(&self.path)
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
