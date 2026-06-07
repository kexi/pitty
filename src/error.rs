//! Error types and their mapping to process exit codes.
//!
//! Errors are split into three severity classes so that the CLI can translate
//! an internal failure into a stable, scriptable exit code at the process
//! boundary.

use thiserror::Error;

/// Top-level error type for pitty.
///
/// Each variant maps to a distinct exit code (see [`PittyError::exit_code`]).
/// Fallible operations across the crate produce this concrete enum directly so
/// the exit-code mapping is exhaustive; we deliberately do not use `anyhow`,
/// whose erased `Error` would obscure which severity class an error belongs to.
#[derive(Debug, Error)]
pub enum PittyError {
    /// An assertion within a scenario did not hold: an `expect` mismatch, a
    /// timeout, EOF before a match, a file assertion failure, or an exit-code
    /// mismatch. Maps to exit code 1.
    #[error("assertion failed: {0}")]
    AssertionFailed(String),

    /// The scenario itself is malformed or could not be located: invalid YAML,
    /// an unknown step, or a missing scenario file. Maps to exit code 2.
    #[error("scenario error: {0}")]
    Scenario(String),

    /// A process/PTY-level failure: `openpty` failed, the child could not be
    /// spawned, or the child could not be killed. Maps to exit code 3.
    #[error("process error: {0}")]
    Process(String),
}

impl PittyError {
    /// The exit code associated with this error class.
    ///
    /// Codes are chosen so callers can distinguish "your app behaved wrong"
    /// (1) from "your scenario is wrong" (2) from "the harness could not run"
    /// (3).
    pub fn exit_code(&self) -> u8 {
        match self {
            PittyError::AssertionFailed(_) => 1,
            PittyError::Scenario(_) => 2,
            PittyError::Process(_) => 3,
        }
    }
}

/// Numeric severity used when reducing many scenario outcomes to a single
/// process exit code.
///
/// Higher number wins: a process error in any scenario dominates a scenario
/// error, which dominates an assertion failure, which dominates success.
///
/// Why not just return `code` (the mapping is currently the identity): we keep
/// an explicit table so that if the public exit codes and their severity
/// ordering ever diverge (e.g. a new code 4 that should rank below assertion),
/// this stays the single place to express the ordering rather than an
/// assumption that code value == severity rank scattered across callers.
pub fn severity(code: u8) -> u8 {
    match code {
        3 => 3, // process
        2 => 2, // scenario
        1 => 1, // assertion
        _ => 0, // success
    }
}

/// Result alias used across the crate for fallible operations that ultimately
/// surface as a [`PittyError`].
pub type Result<T> = std::result::Result<T, PittyError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_map_per_class() {
        // Each error class must map to its documented exit code.
        assert_eq!(PittyError::AssertionFailed("x".into()).exit_code(), 1);
        assert_eq!(PittyError::Scenario("x".into()).exit_code(), 2);
        assert_eq!(PittyError::Process("x".into()).exit_code(), 3);
    }

    #[test]
    fn severity_orders_all_branches() {
        // severity must rank process > scenario > assertion > success, and map
        // any unknown code (here 4) down to success rank 0.
        assert_eq!(severity(0), 0);
        assert_eq!(severity(1), 1);
        assert_eq!(severity(2), 2);
        assert_eq!(severity(3), 3);
        assert_eq!(severity(4), 0);
    }
}
