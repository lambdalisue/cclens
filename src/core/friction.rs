//! Classify tool failures into recurring, fixable categories. The value is not
//! the raw error count but the *pattern*: 100+ "string to replace not found"
//! across sessions is fixable friction (read-before-edit), not noise. Lexical
//! heuristics over the error text; reports label them as such.

/// A category of tool failure, ordered so the most specific match wins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCategory {
    /// Edit/Write precondition: stale read, file changed, target string absent.
    EditPrecondition,
    /// A command or operation blocked by a hook or permission rule.
    BlockedByHook,
    /// A command binary was not found.
    CommandNotFound,
    /// A file or path that does not exist.
    PathNotFound,
    /// A compile error (Rust/TS/etc.).
    CompileError,
    /// A test failure.
    TestFailure,
    /// An operation timed out.
    Timeout,
    /// Anything else.
    Other,
}

impl ErrorCategory {
    pub fn label(self) -> &'static str {
        match self {
            ErrorCategory::EditPrecondition => "edit-precondition",
            ErrorCategory::BlockedByHook => "blocked-by-hook",
            ErrorCategory::CommandNotFound => "command-not-found",
            ErrorCategory::PathNotFound => "path-not-found",
            ErrorCategory::CompileError => "compile-error",
            ErrorCategory::TestFailure => "test-failure",
            ErrorCategory::Timeout => "timeout",
            ErrorCategory::Other => "other",
        }
    }

    /// Parse a stored category label back into the enum.
    pub fn from_label(label: &str) -> ErrorCategory {
        match label {
            "edit-precondition" => ErrorCategory::EditPrecondition,
            "blocked-by-hook" => ErrorCategory::BlockedByHook,
            "command-not-found" => ErrorCategory::CommandNotFound,
            "path-not-found" => ErrorCategory::PathNotFound,
            "compile-error" => ErrorCategory::CompileError,
            "test-failure" => ErrorCategory::TestFailure,
            "timeout" => ErrorCategory::Timeout,
            _ => ErrorCategory::Other,
        }
    }

    /// What recurring instances of this category suggest fixing.
    pub fn suggestion(self) -> &'static str {
        match self {
            ErrorCategory::EditPrecondition => {
                "re-read before editing; a stale or fuzzy match keeps failing"
            }
            ErrorCategory::BlockedByHook => {
                "a habit keeps hitting a rule/hook — the rule blocks but doesn't prevent it"
            }
            ErrorCategory::CommandNotFound => "a missing tool; install it or note its absence",
            ErrorCategory::PathNotFound => {
                "paths are being guessed wrong — a file map in CLAUDE.md would help"
            }
            ErrorCategory::CompileError => {
                "expected during iteration; spikes may signal a hard spot"
            }
            ErrorCategory::TestFailure => "flaky or hard tests; worth stabilising the worst",
            ErrorCategory::Timeout => "slow commands; raise timeout or split the work",
            ErrorCategory::Other => "uncategorised — inspect the raw errors",
        }
    }
}

/// Classify an error tool-result's text. Order matters: more specific patterns
/// (which may also contain generic phrases like "not found") are checked first.
pub fn classify_error(text: &str) -> ErrorCategory {
    let lower = text.to_lowercase();
    let has = |needle: &str| lower.contains(needle);

    if has("string to replace not found")
        || has("has not been read")
        || has("has been modified")
        || has("file has not been read yet")
    {
        ErrorCategory::EditPrecondition
    } else if has("permission for this action was denied")
        || has("enforce-perl")
        || has("use perl instead")
        || has("denied by")
    {
        ErrorCategory::BlockedByHook
    } else if has("command not found") || has("is not recognized") {
        ErrorCategory::CommandNotFound
    } else if has("no such file") || has("does not exist") || has("cannot find the path") {
        ErrorCategory::PathNotFound
    } else if has("error[e") || has("mismatched types") || has("cannot borrow") {
        ErrorCategory::CompileError
    } else if has("test result: failed") || has("assertion `left") || has("panicked at") {
        ErrorCategory::TestFailure
    } else if has("timed out") || has("timeout") {
        ErrorCategory::Timeout
    } else {
        ErrorCategory::Other
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edit_precondition_is_detected_first() {
        assert_eq!(
            classify_error("String to replace not found in file"),
            ErrorCategory::EditPrecondition
        );
        assert_eq!(
            classify_error("File has been modified since read, either by the user"),
            ErrorCategory::EditPrecondition
        );
    }

    #[test]
    fn hook_and_permission_blocks() {
        assert_eq!(
            classify_error("Permission for this action was denied by the user"),
            ErrorCategory::BlockedByHook
        );
        assert_eq!(
            classify_error("[enforce-perl.sh]: sed/awk detected - Use perl instead"),
            ErrorCategory::BlockedByHook
        );
    }

    #[test]
    fn command_not_found_beats_generic_not_found() {
        assert_eq!(
            classify_error("bash: foo: command not found"),
            ErrorCategory::CommandNotFound
        );
    }

    #[test]
    fn path_not_found() {
        assert_eq!(
            classify_error("cat: /tmp/x: No such file or directory"),
            ErrorCategory::PathNotFound
        );
    }

    #[test]
    fn compile_and_test_and_timeout() {
        assert_eq!(
            classify_error("error[E0599]: no method named foo"),
            ErrorCategory::CompileError
        );
        assert_eq!(
            classify_error("test result: FAILED. 1 failed"),
            ErrorCategory::TestFailure
        );
        assert_eq!(
            classify_error("Command timed out after 120s"),
            ErrorCategory::Timeout
        );
    }

    #[test]
    fn unrecognised_is_other() {
        assert_eq!(
            classify_error("something weird happened"),
            ErrorCategory::Other
        );
    }
}
