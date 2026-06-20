//! Detect thrash: bursts of edits to the *same* file in a short window. A high
//! lifetime edit count (a hotspot) can be healthy active development; what
//! signals struggle is re-editing one file many times back-to-back — Claude
//! couldn't get it right and kept retrying. This is the "where did it get stuck"
//! signal that a flat edit count cannot give.

use std::collections::HashMap;

/// A burst of rapid re-edits to one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThrashEpisode {
    pub file: String,
    pub edits: u32,
    pub start_epoch: i64,
    pub end_epoch: i64,
}

impl ThrashEpisode {
    pub fn span_secs(&self) -> i64 {
        self.end_epoch - self.start_epoch
    }
}

/// Find thrash episodes: per file, maximal runs of edits where each edit is
/// within `gap_secs` of the previous, keeping only runs of at least `min_edits`.
/// Sorted by edit count, densest first.
pub fn detect_thrash(edits: &[(String, i64)], gap_secs: i64, min_edits: u32) -> Vec<ThrashEpisode> {
    let mut by_file: HashMap<&str, Vec<i64>> = HashMap::new();
    for (file, epoch) in edits {
        by_file.entry(file).or_default().push(*epoch);
    }

    let mut episodes = Vec::new();
    for (file, mut times) in by_file {
        times.sort_unstable();
        let mut start = times[0];
        let mut prev = times[0];
        let mut count: u32 = 1;
        for &t in &times[1..] {
            if t - prev <= gap_secs {
                count += 1;
            } else {
                push_if(&mut episodes, file, count, start, prev, min_edits);
                start = t;
                count = 1;
            }
            prev = t;
        }
        push_if(&mut episodes, file, count, start, prev, min_edits);
    }

    episodes.sort_by(|a, b| {
        b.edits
            .cmp(&a.edits)
            .then(b.span_secs().cmp(&a.span_secs()))
    });
    episodes
}

fn push_if(out: &mut Vec<ThrashEpisode>, file: &str, edits: u32, start: i64, end: i64, min: u32) {
    if edits >= min {
        out.push(ThrashEpisode {
            file: file.to_string(),
            edits,
            start_epoch: start,
            end_epoch: end,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edit(file: &str, epoch: i64) -> (String, i64) {
        (file.to_string(), epoch)
    }

    #[test]
    fn rapid_reedits_to_one_file_are_a_thrash_episode() {
        // Three edits within 60s of each other.
        let edits = [edit("a.rs", 0), edit("a.rs", 30), edit("a.rs", 50)];
        let episodes = detect_thrash(&edits, 60, 3);
        assert_eq!(episodes.len(), 1);
        assert_eq!(episodes[0].edits, 3);
        assert_eq!(episodes[0].span_secs(), 50);
    }

    #[test]
    fn edits_spread_out_are_not_thrash() {
        // Same count, but each edit is far from the last — healthy development.
        let edits = [edit("a.rs", 0), edit("a.rs", 1000), edit("a.rs", 2000)];
        assert!(detect_thrash(&edits, 60, 3).is_empty());
    }

    #[test]
    fn a_run_below_the_minimum_is_ignored() {
        let edits = [edit("a.rs", 0), edit("a.rs", 10)];
        assert!(detect_thrash(&edits, 60, 3).is_empty());
    }

    #[test]
    fn separate_bursts_of_the_same_file_are_separate_episodes() {
        let edits = [
            edit("a.rs", 0),
            edit("a.rs", 10),
            edit("a.rs", 20), // burst 1
            edit("a.rs", 5000),
            edit("a.rs", 5010),
            edit("a.rs", 5020), // burst 2 after a long gap
        ];
        let episodes = detect_thrash(&edits, 60, 3);
        assert_eq!(episodes.len(), 2);
    }
}
