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
    /// A non-MCP tool name that does not exist (wrong/deferred tool).
    ToolNotAvailable,
    /// A tool was called with invalid arguments.
    InputValidation,
    /// An MCP server returned an error (auth, validation, upstream).
    McpError,
    /// A user-stopped or cascade-cancelled call — not a code failure.
    Cancelled,
    /// A transient infrastructure error (model unavailable, etc.).
    Transient,
    /// A generic command failure (non-zero exit) with no more specific cause.
    CommandFailed,
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
            ErrorCategory::ToolNotAvailable => "tool-not-available",
            ErrorCategory::InputValidation => "input-validation",
            ErrorCategory::McpError => "mcp-error",
            ErrorCategory::Cancelled => "cancelled",
            ErrorCategory::Transient => "transient",
            ErrorCategory::CommandFailed => "command-failed",
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
            "tool-not-available" => ErrorCategory::ToolNotAvailable,
            "input-validation" => ErrorCategory::InputValidation,
            "mcp-error" => ErrorCategory::McpError,
            "cancelled" => ErrorCategory::Cancelled,
            "transient" => ErrorCategory::Transient,
            "command-failed" => ErrorCategory::CommandFailed,
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
            ErrorCategory::ToolNotAvailable => {
                "a tool was called by a name that doesn't exist — wrong/deferred tool"
            }
            ErrorCategory::InputValidation => "tool called with invalid arguments",
            ErrorCategory::McpError => "an MCP server erroring (auth/validation/upstream)",
            ErrorCategory::Cancelled => {
                "not a code failure: a user stop or parallel-cascade cancel; frequent stops \
                 may mean the work went off-track"
            }
            ErrorCategory::Transient => {
                "transient infra (e.g. model unavailable) — not yours to fix"
            }
            ErrorCategory::CommandFailed => "a command exited non-zero; cause unclassified",
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
    } else if has("cancelled: parallel tool call")
        || has("request interrupted by user")
        || has("the user doesn't want to proceed")
    {
        // User stop or a sibling failing in a parallel batch — not a code fault.
        ErrorCategory::Cancelled
    } else if has("permission for this action was denied")
        || has("enforce-perl")
        || has("use perl instead")
        || has("denied by")
        || has("blocked:")
    {
        ErrorCategory::BlockedByHook
    } else if has("temporarily unavailable") || has("overloaded") {
        ErrorCategory::Transient
    } else if has("no such tool available") {
        ErrorCategory::ToolNotAvailable
    } else if has("inputvalidationerror") || has("input validation error") {
        ErrorCategory::InputValidation
    } else if has("mcp error") {
        ErrorCategory::McpError
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
    } else if has("exit code") {
        // Generic non-zero exit, checked last so specific causes above win.
        ErrorCategory::CommandFailed
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
    fn user_stops_and_cascades_are_cancelled_not_failures() {
        assert_eq!(
            classify_error("<tool_use_error>Cancelled: parallel tool call Bash(grep ...)"),
            ErrorCategory::Cancelled
        );
        assert_eq!(
            classify_error("The user doesn't want to proceed with this tool use."),
            ErrorCategory::Cancelled
        );
    }

    #[test]
    fn infra_tool_and_validation_errors() {
        assert_eq!(
            classify_error("claude-opus-4-8 is temporarily unavailable"),
            ErrorCategory::Transient
        );
        assert_eq!(
            classify_error("<tool_use_error>Error: No such tool available: Glob"),
            ErrorCategory::ToolNotAvailable
        );
        assert_eq!(
            classify_error("InputValidationError: Read failed"),
            ErrorCategory::InputValidation
        );
        assert_eq!(
            classify_error("MCP error -32602: Input validation"),
            ErrorCategory::McpError
        );
    }

    #[test]
    fn a_specific_cause_beats_generic_exit_code() {
        // A compile error that also prints "Exit code 1" is a compile error.
        assert_eq!(
            classify_error("error[E0599]: no method\nExit code 1"),
            ErrorCategory::CompileError
        );
        // A bare non-zero exit with no known cause is command-failed.
        assert_eq!(
            classify_error("Exit code 2 (eval): bad substitution"),
            ErrorCategory::CommandFailed
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
