//! Run reports (serialize-only) and scenario log writing.
//!
//! A [`Report`] summarizes one scenario run for JSON output. Logs are written
//! to `logs/<scenario>.log` with `0600` permissions, and every byte that
//! touches the log file is run through secret masking first.

use std::io::Write;
use std::path::Path;

use serde::Serialize;

use crate::assert::AssertionResult;
use crate::error::PittyError;
use crate::workspace::mask_secrets;

/// Overall status of a completed scenario run.
///
/// `lowercase` so JSON consumers see `"passed"` or `"failed"`.
///
/// Why only two variants (no `Error`): a `Status` is only ever produced when a
/// run *completes* — the runner returns `Err(PittyError)` for any hard fault
/// (process/scenario), so a report's status reflects solely the pass/fail of the
/// assertions that ran. The exit-code truth for a hard fault lives in
/// [`PittyError::exit_code`](crate::error::PittyError::exit_code), the single
/// source of truth for fault classes; baking an `Error` status into the report
/// would duplicate (and could contradict) that mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// All assertions passed.
    Passed,
    /// At least one assertion failed (exit code 1 class).
    Failed,
}

/// Map a completed run's [`Status`] to its process exit code class.
///
/// The single authoritative `Status -> u8` table: `Passed` is success (0) and
/// `Failed` is the assertion class (1). Hard faults never reach here — they are
/// `Err(PittyError)` whose `exit_code()` owns the scenario (2) / process (3)
/// classes. Both the CLI's `run_one` and the matrix aggregation route through
/// here so the mapping lives in one exhaustive `match`. Why centralize rather
/// than inline at each call site: a future `Status` variant would otherwise need
/// two synchronized edits, and a missed one would map to a wrong code silently;
/// the exhaustive match makes the compiler flag every site that must be updated.
pub(crate) fn status_exit_code(status: Status) -> u8 {
    match status {
        Status::Passed => 0,
        Status::Failed => 1,
    }
}

/// The human-facing PASS/FAIL verdict string for a pass/fail boolean.
///
/// The single source of truth for the verdict wording, reused by the log
/// writer, the matrix table, and the GitHub step-summary tables. Why centralize:
/// the same `"PASS"`/`"FAIL"` literal was previously inlined in four places, so a
/// wording change risked the report, table, and summary disagreeing.
pub(crate) fn verdict_label(passed: bool) -> &'static str {
    if passed {
        "PASS"
    } else {
        "FAIL"
    }
}

/// The PASS/FAIL verdict for a completed [`Status`] (Passed/Failed only).
pub(crate) fn status_verdict_label(status: Status) -> &'static str {
    verdict_label(matches!(status, Status::Passed))
}

/// A serializable summary of one scenario run.
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    /// The scenario's name.
    pub scenario: String,
    /// Overall status.
    pub status: Status,
    /// Wall-clock duration of the run in milliseconds.
    pub duration_ms: u128,
    /// Per-step assertion results.
    pub assertions: Vec<AssertionResult>,
}

impl Report {
    /// Serialize this report to pretty JSON.
    pub fn to_json(&self) -> String {
        // serde_json cannot fail to serialize this owned, simple structure;
        // fall back to a minimal string only to avoid an unwrap in the
        // unlikely event of a serializer error.
        serde_json::to_string_pretty(self)
            .unwrap_or_else(|_| "{\"error\":\"failed to serialize report\"}".to_string())
    }
}

/// Write the captured terminal output and assertion results to
/// `logs/<scenario>.log`, masking secrets, with `0600` permissions.
///
/// The log directory is created relative to `base_dir`. Failures here are
/// non-fatal to the run's pass/fail verdict, so callers may log-and-continue;
/// we still return a `Result` so the CLI can warn.
pub fn write_log(
    base_dir: &Path,
    scenario_name: &str,
    output: &str,
    report: &Report,
    secrets: &[String],
) -> Result<(), PittyError> {
    let log_dir = base_dir.join("logs");
    std::fs::create_dir_all(&log_dir)
        .map_err(|e| PittyError::Process(format!("cannot create logs dir: {e}")))?;

    // Sanitize the scenario name into a safe file stem so a crafted name
    // cannot escape the logs directory via path separators.
    let stem = sanitize_stem(scenario_name);
    let log_path = log_dir.join(format!("{stem}.log"));

    let mut body = String::new();
    body.push_str(&format!("# scenario: {scenario_name}\n"));
    body.push_str(&format!("# status: {:?}\n", report.status));
    body.push_str(&format!("# duration_ms: {}\n\n", report.duration_ms));
    body.push_str("## terminal output\n");
    body.push_str(output);
    body.push_str("\n\n## assertions\n");
    for a in &report.assertions {
        let mark = verdict_label(a.passed);
        match &a.message {
            Some(msg) => body.push_str(&format!("[{mark}] {} -- {msg}\n", a.step)),
            None => body.push_str(&format!("[{mark}] {}\n", a.step)),
        }
    }

    // Mask secrets on the entire log body just before it is written, so no
    // secret value ever reaches disk regardless of where it appeared.
    let masked = mask_secrets(&body, secrets);

    let mut file = std::fs::File::create(&log_path)
        .map_err(|e| PittyError::Process(format!("cannot create log file: {e}")))?;
    file.write_all(masked.as_bytes())
        .map_err(|e| PittyError::Process(format!("cannot write log: {e}")))?;
    set_log_permissions_0600(&log_path)?;
    Ok(())
}

/// Reduce a scenario name to a filesystem-safe stem.
///
/// Keeps alphanumerics, dash, underscore, and dot; replaces everything else
/// (including path separators) with `_`. This prevents a scenario `name` from
/// directing the log write outside `logs/`.
fn sanitize_stem(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() {
        "scenario".to_string()
    } else {
        cleaned
    }
}

#[cfg(unix)]
fn set_log_permissions_0600(path: &Path) -> Result<(), PittyError> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| PittyError::Process(format!("cannot set log perms: {e}")))
}

#[cfg(not(unix))]
fn set_log_permissions_0600(_path: &Path) -> Result<(), PittyError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assert::AssertionResult;

    #[test]
    fn status_serializes_lowercase() {
        // Status must serialize lowercase for stable JSON consumers.
        assert_eq!(
            serde_json::to_string(&Status::Passed).unwrap(),
            "\"passed\""
        );
        assert_eq!(
            serde_json::to_string(&Status::Failed).unwrap(),
            "\"failed\""
        );
    }

    #[test]
    fn status_exit_code_is_two_valued() {
        // The Status -> exit-code table is exactly two branches: Passed 0,
        // Failed 1. Fault classes (2/3) are owned by PittyError, never Status.
        assert_eq!(status_exit_code(Status::Passed), 0);
        assert_eq!(status_exit_code(Status::Failed), 1);
    }

    #[test]
    fn report_json_never_carries_error_status() {
        // After dropping Status::Error, neither a passed nor a failed report's
        // JSON may contain the string "error" as a status value, so consumers
        // reading "status" only ever see passed/failed.
        for status in [Status::Passed, Status::Failed] {
            let report = Report {
                scenario: "s".into(),
                status,
                duration_ms: 1,
                assertions: Vec::new(),
            };
            assert!(!report.to_json().contains("\"error\""));
        }
    }

    #[test]
    fn report_json_includes_fields_and_omits_null_message() {
        // A passing assertion must omit the message key entirely in JSON.
        let report = Report {
            scenario: "demo".into(),
            status: Status::Passed,
            duration_ms: 12,
            assertions: vec![AssertionResult::pass("expect: hi")],
        };
        let json = report.to_json();
        assert!(json.contains("\"scenario\": \"demo\""));
        assert!(json.contains("\"status\": \"passed\""));
        assert!(!json.contains("\"message\""));
    }

    #[test]
    fn sanitize_stem_strips_path_separators() {
        // A name with slashes or dots must not escape the logs directory.
        assert_eq!(sanitize_stem("../../etc/passwd"), ".._.._etc_passwd");
        assert_eq!(sanitize_stem("ok-name_1.2"), "ok-name_1.2");
        assert_eq!(sanitize_stem(""), "scenario");
    }

    #[test]
    fn write_log_masks_secrets_and_sets_mode() {
        // The written log must contain *** in place of secrets and be 0600.
        let dir = tempfile::tempdir().unwrap();
        let report = Report {
            scenario: "sec".into(),
            status: Status::Passed,
            duration_ms: 1,
            assertions: vec![AssertionResult::pass("step")],
        };
        write_log(
            dir.path(),
            "sec",
            "token=supersecret done",
            &report,
            &["supersecret".to_string()],
        )
        .unwrap();
        let log = std::fs::read_to_string(dir.path().join("logs/sec.log")).unwrap();
        assert!(log.contains("token=***"));
        assert!(!log.contains("supersecret"));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join("logs/sec.log"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
    }
}
