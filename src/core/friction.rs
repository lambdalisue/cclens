//! Classify tool failures into recurring, fixable categories. The value is not
//! the raw error count but the *pattern*: 100+ "string to replace not found"
//! across sessions is fixable friction (read-before-edit), not noise. Lexical
//! heuristics over the error text — the fallback for everything the transcript
//! does not label itself; reports say which is which.

/// A category of tool failure, ordered so the most specific match wins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCategory {
    /// Edit/Write precondition: stale read, file changed, target string absent.
    EditPrecondition,
    /// A command or operation blocked by a hook or permission rule.
    BlockedByHook,
    /// A network connection the Bash sandbox refused — read from the
    /// harness's own report of the refusal, so not a guess at wording.
    SandboxNetwork,
    /// A write the OS refused (`EPERM`, read-only file system), most often the
    /// Bash sandbox guarding paths outside its allowed directories. A lexical
    /// heuristic: the same wording comes from any other permission refusal.
    SandboxWrite,
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
            ErrorCategory::SandboxNetwork => "sandbox-network",
            ErrorCategory::SandboxWrite => "sandbox-write",
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
            "sandbox-network" => ErrorCategory::SandboxNetwork,
            "sandbox-write" => ErrorCategory::SandboxWrite,
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

    /// Whether recurring instances point to something the user can fix. The
    /// non-actionable rest is noise: user stops (`Cancelled`), infra blips
    /// (`Transient`), and the unclassified buckets (`CommandFailed`, `Other`)
    /// whose cause the heuristics could not pin down. Reports filter on this so
    /// "top friction" stays a list of leverage, not a tally of everything.
    pub fn is_actionable(self) -> bool {
        !matches!(
            self,
            ErrorCategory::Cancelled
                | ErrorCategory::Transient
                | ErrorCategory::CommandFailed
                | ErrorCategory::Other
        )
    }

    /// Whether the Bash sandbox refused the call — the failures whose fix is a
    /// sandbox allowlist entry, and whose usual workaround is an unsandboxed
    /// retry (`core::thrash::count_unsandboxed_retries`).
    pub fn is_sandbox(self) -> bool {
        matches!(
            self,
            ErrorCategory::SandboxNetwork | ErrorCategory::SandboxWrite
        )
    }

    /// Whether a check run failing this way means the check itself failed. A
    /// missing path, a refusal or a stop means the check never got to say —
    /// `cd /missing && cargo test` ran no tests — so such a failure is no fix
    /// round (`core::thrash::check_outcome`).
    pub fn is_check_failure(self) -> bool {
        matches!(
            self,
            ErrorCategory::CompileError
                | ErrorCategory::TestFailure
                | ErrorCategory::Timeout
                | ErrorCategory::CommandFailed
                | ErrorCategory::Other
        )
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
            ErrorCategory::SandboxNetwork => {
                "the sandbox refused a host — allow it in the sandbox network settings if the work needs it"
            }
            ErrorCategory::SandboxWrite => {
                "a write was refused (likely the sandbox) — allow the path in the sandbox filesystem settings, or keep writes inside the project"
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
///
/// This is the **fallback**: a denied call is classified from the structural
/// marker the transcript carries instead (`adapter::transcript`), because denial
/// text is hook-authored and may be in any language. Text matching only has to
/// cover what carries no such marker.
///
/// Editing these patterns changes what an unchanged transcript yields, so it
/// requires bumping `ANALYZER_VERSION` (`store.rs`) — otherwise already-ingested
/// sessions keep their old categories forever (`docs/specs/storage.md`).
pub fn classify_error(text: &str) -> ErrorCategory {
    let lower = text.to_lowercase();
    let has = |needle: &str| lower.contains(needle);

    if has("string to replace not found")
        || has("matches of the string to replace")
        || has("old_string and new_string are exactly the same")
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
    } else if has("operation not permitted") || has("read-only file system") {
        ErrorCategory::SandboxWrite
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

/// The failure a check runner printed even though the command exited 0 — the
/// usual reason being `cmd 2>&1 | tail`, where the pipeline's status is
/// `tail`'s. Read from the runner's own summary lines (cargo, rustc, tsc, bun,
/// just), so it is a lexical heuristic, and it is only asked of check runs
/// (`core::thrash::check_key`) whose result was not already a failure.
pub fn masked_failure(output: &str) -> Option<ErrorCategory> {
    let failed_tests = output.lines().any(|line| {
        let line = line.trim();
        line.starts_with("test result: FAILED")
            || line
                .strip_suffix(" fail")
                .and_then(|n| n.parse::<u64>().ok())
                .is_some_and(|n| n > 0)
    });
    if failed_tests {
        Some(ErrorCategory::TestFailure)
    } else if output.contains("error[E")
        || output.contains("error: could not compile")
        || output.contains(": error TS")
    {
        Some(ErrorCategory::CompileError)
    } else if output.contains("error: Recipe `") {
        Some(ErrorCategory::CommandFailed)
    } else {
        None
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
    fn edit_preconditions_beyond_a_missing_string() {
        // Both texts are emitted verbatim by the Edit tool, so matching them is
        // precise rather than a guess at free-form wording.
        assert_eq!(
            classify_error("Found 2 matches of the string to replace, but replace_all is false"),
            ErrorCategory::EditPrecondition
        );
        assert_eq!(
            classify_error("No changes to make: old_string and new_string are exactly the same."),
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
    fn an_os_permission_refusal_is_a_sandbox_write() {
        assert_eq!(
            classify_error("rm: /tmp/example/x: Operation not permitted\nExit code 1"),
            ErrorCategory::SandboxWrite
        );
        assert_eq!(
            classify_error("touch: /tmp/example/y: Read-only file system"),
            ErrorCategory::SandboxWrite
        );
    }

    #[test]
    fn a_sandbox_write_beats_the_path_it_names() {
        // The refused path may also "not exist" from the sandbox's view; the
        // refusal is the cause worth routing.
        assert_eq!(
            classify_error("mkdir: /tmp/example/a: Operation not permitted; No such file"),
            ErrorCategory::SandboxWrite
        );
    }

    #[test]
    fn sandbox_categories_round_trip_and_are_actionable() {
        for category in [ErrorCategory::SandboxNetwork, ErrorCategory::SandboxWrite] {
            assert_eq!(ErrorCategory::from_label(category.label()), category);
            assert!(category.is_actionable());
            assert!(category.is_sandbox());
        }
        assert!(!ErrorCategory::PathNotFound.is_sandbox());
    }

    #[test]
    fn a_runners_own_failure_line_reveals_a_masked_failure() {
        assert_eq!(
            masked_failure("running 3 tests\ntest result: FAILED. 1 passed; 2 failed"),
            Some(ErrorCategory::TestFailure)
        );
        assert_eq!(
            masked_failure(" 10 pass\n 2 fail\nRan 12 tests"),
            Some(ErrorCategory::TestFailure)
        );
        assert_eq!(
            masked_failure("error[E0308]: mismatched types\nerror: could not compile `x`"),
            Some(ErrorCategory::CompileError)
        );
        assert_eq!(
            masked_failure("src/a.ts(3,1): error TS2322: Type 'x' is not assignable"),
            Some(ErrorCategory::CompileError)
        );
        assert_eq!(
            masked_failure("error: Recipe `check` failed on line 12 with exit code 101"),
            Some(ErrorCategory::CommandFailed)
        );
    }

    #[test]
    fn a_passing_runs_output_is_no_masked_failure() {
        assert_eq!(masked_failure("test result: ok. 12 passed; 0 failed"), None);
        assert_eq!(masked_failure(" 12 pass\n 0 fail\nRan 12 tests"), None);
        assert_eq!(masked_failure("Finished `test` profile"), None);
    }

    #[test]
    fn unrecognised_is_other() {
        assert_eq!(
            classify_error("something weird happened"),
            ErrorCategory::Other
        );
    }

    #[test]
    fn noise_categories_are_not_actionable() {
        // Fixable friction the user can act on.
        assert!(ErrorCategory::EditPrecondition.is_actionable());
        assert!(ErrorCategory::PathNotFound.is_actionable());
        // Noise: stops, infra, and the unclassified buckets.
        assert!(!ErrorCategory::Cancelled.is_actionable());
        assert!(!ErrorCategory::Transient.is_actionable());
        assert!(!ErrorCategory::CommandFailed.is_actionable());
        assert!(!ErrorCategory::Other.is_actionable());
    }
}
