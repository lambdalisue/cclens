//! Path helpers shared by the analysis core and the adapter that feeds it.

/// The last segment of `path`, treating both `/` and `\` as separators.
///
/// A transcript carries whatever separator the machine that wrote it uses, and
/// within one session the same file arrives spelled both ways — one tool call
/// passes `docs/tasks.md`, the next passes `D:\notes\docs\tasks.md`. Splitting
/// on `/` alone left the whole path standing in for the name and let a single
/// file compete against itself for a hotspot slot, ranking on how the path
/// happened to be spelled rather than on the file.
///
/// Accepting both separators everywhere mis-splits a unix filename that really
/// contains a backslash. That is the far rarer case, and this label is
/// deliberately lossy already — the full path is kept alongside it for anything
/// that must tell two same-named files apart (`docs/specs/events.md`).
pub fn basename(path: &str) -> &str {
    match path.rsplit_once(['/', '\\']) {
        Some((_, name)) => name,
        None => path,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basename_takes_the_last_segment() {
        assert_eq!(basename("/tmp/example/repo/src/main.rs"), "main.rs");
    }

    #[test]
    fn basename_accepts_a_backslash_separator() {
        assert_eq!(basename(r"C:\example\repo\src\main.rs"), "main.rs");
    }

    #[test]
    fn basename_folds_both_spellings_of_one_file_together() {
        // The hotspot grouping key: however the path was spelled, one file is
        // one row.
        assert_eq!(
            basename(r"D:\notes\docs\tasks.md"),
            basename("docs/tasks.md")
        );
    }

    #[test]
    fn basename_passes_a_bare_name_through() {
        assert_eq!(basename("tasks.md"), "tasks.md");
    }
}
