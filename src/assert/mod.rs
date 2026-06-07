//! Assertion results and output/process-level assertion helpers.
//!
//! Output assertions translate a [`crate::pty::ExpectOutcome`] into a pass/fail
//! row; process assertions inspect the child's exit/running state. File
//! assertions live in [`file`].

pub mod file;
pub mod json;
pub mod semantic;
pub mod snapshot;

use serde::Serialize;

use crate::pty::ExpectOutcome;

/// One assertion's outcome, recorded in the report.
///
/// Serializes for the JSON report; the message is populated only on failure.
#[derive(Debug, Clone, Serialize)]
pub struct AssertionResult {
    /// The step label this assertion came from.
    pub step: String,
    /// Whether the assertion passed.
    pub passed: bool,
    /// Failure detail; `None` when passed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl AssertionResult {
    /// A passing result for `step`.
    pub fn pass(step: impl Into<String>) -> Self {
        AssertionResult {
            step: step.into(),
            passed: true,
            message: None,
        }
    }

    /// A failing result for `step` with a reason.
    pub fn fail(step: impl Into<String>, message: impl Into<String>) -> Self {
        AssertionResult {
            step: step.into(),
            passed: false,
            message: Some(message.into()),
        }
    }
}

/// Turn an `expect` outcome into an assertion result.
///
/// `Matched` passes; `Timeout` and `EofBeforeMatch` fail with a diagnostic
/// tail so the user can see what the output actually was.
pub fn from_expect(step: &str, outcome: ExpectOutcome) -> AssertionResult {
    match outcome {
        ExpectOutcome::Matched { .. } => AssertionResult::pass(step),
        ExpectOutcome::Timeout { tail } => AssertionResult::fail(
            step,
            format!("timed out waiting for match; last output: {tail:?}"),
        ),
        ExpectOutcome::EofBeforeMatch { tail } => AssertionResult::fail(
            step,
            format!("stream closed before match; last output: {tail:?}"),
        ),
    }
}

/// Assert an `expect_not`: `matched_now` true means the forbidden pattern was
/// present, which is a failure.
pub fn from_expect_not(step: &str, matched_now: bool) -> AssertionResult {
    if matched_now {
        AssertionResult::fail(step, "forbidden pattern was present in output")
    } else {
        AssertionResult::pass(step)
    }
}

/// Assert the child exited with `expected` code.
///
/// `actual` is `None` when the child is still running, which fails the
/// exit-code assertion since no exit has occurred yet.
pub fn check_exit(step: &str, expected: i32, actual: Option<i32>) -> AssertionResult {
    match actual {
        Some(code) if code == expected => AssertionResult::pass(step),
        Some(code) => {
            AssertionResult::fail(step, format!("expected exit code {expected}, got {code}"))
        }
        None => AssertionResult::fail(
            step,
            format!("expected exit code {expected}, but process is still running"),
        ),
    }
}

/// Assert the child's running state matches `expected`.
pub fn check_running(step: &str, expected: bool, actual: bool) -> AssertionResult {
    if actual == expected {
        AssertionResult::pass(step)
    } else {
        AssertionResult::fail(
            step,
            format!("expected running={expected}, but running={actual}"),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matched_outcome_passes() {
        // A Matched outcome must produce a passing assertion with no message.
        let r = from_expect("expect: hi", ExpectOutcome::Matched { consumed_to: 2 });
        assert!(r.passed);
        assert!(r.message.is_none());
    }

    #[test]
    fn timeout_and_eof_fail_with_tail() {
        // Timeout and EOF outcomes must fail and include the diagnostic tail.
        let t = from_expect("e", ExpectOutcome::Timeout { tail: "ctx".into() });
        assert!(!t.passed && t.message.as_ref().unwrap().contains("ctx"));
        let eof = from_expect("e", ExpectOutcome::EofBeforeMatch { tail: "ctx".into() });
        assert!(!eof.passed && eof.message.as_ref().unwrap().contains("ctx"));
    }

    #[test]
    fn expect_not_fails_when_present() {
        // expect_not must fail when the pattern is present and pass otherwise.
        assert!(!from_expect_not("n", true).passed);
        assert!(from_expect_not("n", false).passed);
    }

    #[test]
    fn exit_code_assertions() {
        // Exit assertions must compare codes and treat a running child as fail.
        assert!(check_exit("x", 0, Some(0)).passed);
        assert!(!check_exit("x", 0, Some(1)).passed);
        assert!(!check_exit("x", 0, None).passed);
    }

    #[test]
    fn running_assertions() {
        // Running assertions must compare the boolean states.
        assert!(check_running("r", true, true).passed);
        assert!(!check_running("r", true, false).passed);
    }
}
