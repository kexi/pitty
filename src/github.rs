//! GitHub Actions integration: step summaries and workflow annotations.
//!
//! When ptytest runs inside GitHub Actions it can emit two side-channel
//! outputs in addition to its normal stdout/exit code:
//!
//! - a **step summary**: Markdown appended to the file named by
//!   `$GITHUB_STEP_SUMMARY`, shown on the workflow run page;
//! - **annotations**: `::error`/`::warning` workflow commands printed to stdout
//!   that surface inline on the run and in the PR diff.
//!
//! Both are *side effects only*: the process exit code (0/1/2/3) is the verdict
//! and is never changed by anything here, and a failure to write the summary is
//! swallowed (best-effort) so a missing/unwritable summary file cannot turn a
//! passing run red.
//!
//! Security: every string written to a summary or annotation is run through
//! [`mask_secrets`] with the scenario's secret values first. The reports handed
//! in are already masked by the runner, so this is defense in depth — it
//! guarantees no secret reaches the summary, annotation, or CI log even if a
//! future code path were to assemble unmasked text here.

use std::io::Write;

use crate::bench::BenchReport;
use crate::matrix::MatrixReport;
use crate::report::{status_verdict_label, verdict_label, Report, Status};
use crate::workspace::mask_secrets;

/// The environment variable GitHub Actions sets to `"true"` on every runner.
const GITHUB_ACTIONS_ENV: &str = "GITHUB_ACTIONS";
/// The environment variable naming the step-summary file to append to.
const STEP_SUMMARY_ENV: &str = "GITHUB_STEP_SUMMARY";

/// Whether GitHub Actions output should be emitted.
///
/// True when running on a GitHub Actions runner (`GITHUB_ACTIONS=true`) or when
/// the caller forced it on (`--github`). The two are OR-ed, mirroring the
/// existing `--update`/`PTYTEST_UPDATE_SNAPSHOTS` pattern, so a user can preview
/// the output locally without faking the runner environment.
pub fn github_enabled(flag: bool) -> bool {
    flag || is_github_actions()
}

/// Whether the process is running on a GitHub Actions runner.
fn is_github_actions() -> bool {
    std::env::var(GITHUB_ACTIONS_ENV).as_deref() == Ok("true")
}

/// Append `markdown` to the `$GITHUB_STEP_SUMMARY` file, masking secrets first.
///
/// Best-effort: a missing env var or an unwritable file is reported to stderr
/// and otherwise ignored, since the step summary is informational and must not
/// affect the run verdict. Uses append mode because GitHub accumulates summary
/// fragments across steps into one document (the documented contract).
pub fn write_step_summary(markdown: &str, secrets: &[String]) {
    let Ok(path) = std::env::var(STEP_SUMMARY_ENV) else {
        // Not an error: `--github` may be forced on locally where no runner set
        // the summary path. The annotations still print to stdout.
        return;
    };
    let masked = mask_secrets(markdown, secrets);
    if let Err(e) = append_to_file(&path, &masked) {
        eprintln!("warning: could not write GitHub step summary: {e}");
    }
}

/// Open `path` in append mode and write `contents`.
fn append_to_file(path: &str, contents: &str) -> std::io::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    file.write_all(contents.as_bytes())?;
    file.write_all(b"\n")
}

/// Print a GitHub Actions error annotation for a failed assertion.
///
/// Both `title` and `message` are secret-masked, then escaped for the workflow
/// command grammar (see [`escape_property`]/[`escape_data`]). The annotation is
/// written to stdout, where the runner parses `::` command lines.
pub fn emit_error_annotation(title: &str, message: &str, secrets: &[String]) {
    println!("{}", format_error_annotation(title, message, secrets));
}

/// Build the `::error` command line for a failed assertion.
///
/// Split from [`emit_error_annotation`] so the mask-then-escape result is
/// directly testable without capturing stdout: this is the exact string the
/// emitter prints. Masking runs before escaping so a secret containing a
/// property delimiter (`:`/`,`) or a newline is replaced before those characters
/// are percent-encoded — the raw or escaped secret can never reach the line.
fn format_error_annotation(title: &str, message: &str, secrets: &[String]) -> String {
    let title = escape_property(&mask_secrets(title, secrets));
    let message = escape_data(&mask_secrets(message, secrets));
    format!("::error title={title}::{message}")
}

/// Print a GitHub Actions warning annotation (used for flaky bench results).
pub fn emit_warning_annotation(message: &str, secrets: &[String]) {
    println!("{}", format_warning_annotation(message, secrets));
}

/// Build the `::warning` command line. Split out for the same testability
/// reason as [`format_error_annotation`].
fn format_warning_annotation(message: &str, secrets: &[String]) -> String {
    let message = escape_data(&mask_secrets(message, secrets));
    format!("::warning::{message}")
}

/// Escape a value used in the *data* portion of a workflow command (after `::`).
///
/// GitHub's parser treats a raw newline as the end of the command, and `%` as
/// the escape introducer, so both must be percent-encoded or the annotation is
/// truncated/garbled. Carriage returns are encoded too so CRLF output renders
/// cleanly. Why a single pass rather than chained `replace`: each `replace`
/// allocates a fresh `String`, and a char scan also sidesteps any double-encode
/// ordering concern (`%` introduced by an encoding is never re-scanned).
fn escape_data(value: &str) -> String {
    escape_with(value, false)
}

/// Escape a value used in a command *property* (e.g. `title=...`).
///
/// Properties have a stricter grammar than data: in addition to the data
/// escapes, `:` and `,` are delimiters and must be encoded so a value
/// containing them cannot be misread as the start of another property or the
/// data section.
fn escape_property(value: &str) -> String {
    escape_with(value, true)
}

/// Percent-encode workflow-command metacharacters in one pass. `property` adds
/// the `:`/`,` property delimiters to the data set.
fn escape_with(value: &str, property: bool) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '%' => out.push_str("%25"),
            '\r' => out.push_str("%0D"),
            '\n' => out.push_str("%0A"),
            ':' if property => out.push_str("%3A"),
            ',' if property => out.push_str("%2C"),
            _ => out.push(ch),
        }
    }
    out
}

/// Render a single-scenario [`Report`] as a Markdown step-summary fragment.
///
/// Emits a heading with the overall verdict plus a per-assertion table. This is
/// a presentation-only view: it does not touch the JSON contract that
/// [`Report::to_json`] owns.
///
/// `secrets` are masked into each value *before* Markdown-cell escaping (see
/// [`mask_then_escape`]). Why mask here and not rely solely on
/// [`write_step_summary`]'s pass: cell escaping rewrites `|`/newlines, so a
/// secret containing one of those would no longer match the masker if masking
/// ran after escaping — masking each raw value first closes that gap.
pub fn report_to_markdown(report: &Report, secrets: &[String]) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "### ptytest: {} — {}\n\n",
        mask_secrets(&report.scenario, secrets),
        status_verdict_label(report.status)
    ));
    out.push_str(&format!("Duration: {}ms\n\n", report.duration_ms));
    out.push_str("| Result | Step | Detail |\n");
    out.push_str("| --- | --- | --- |\n");
    for assertion in &report.assertions {
        let result = verdict_label(assertion.passed);
        let detail = assertion.message.as_deref().unwrap_or("");
        out.push_str(&format!(
            "| {} | {} | {} |\n",
            result,
            mask_then_escape(&assertion.step, secrets),
            mask_then_escape(detail, secrets),
        ));
    }
    out
}

/// Render a [`MatrixReport`] as a Markdown PASS/FAIL table.
///
/// `secrets` are masked into each axis value before cell escaping; see
/// [`report_to_markdown`] for why the order matters. Matrix axis values can
/// never be a secret (an axis colliding with a `secret: true` variable is
/// rejected earlier), so this is defense in depth.
pub fn matrix_to_markdown(report: &MatrixReport, secrets: &[String]) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "### ptytest matrix — {}/{} passed\n\n",
        report.passed(),
        report.total()
    ));
    // One column per axis (in axis order) plus result and duration, so each cell
    // is self-describing without relying on a separate legend.
    out.push('|');
    for axis in &report.axes {
        out.push_str(&format!(" {} |", mask_then_escape(axis, secrets)));
    }
    out.push_str(" Result | Duration |\n|");
    for _ in &report.axes {
        out.push_str(" --- |");
    }
    out.push_str(" --- | --- |\n");
    for cell in &report.cells {
        out.push('|');
        // Iterate the declared axis order, not the map order, so columns line up
        // with the header even though `coords` is a BTreeMap.
        for axis in &report.axes {
            let value = cell.coords.get(axis).map(String::as_str).unwrap_or("");
            out.push_str(&format!(" {} |", mask_then_escape(value, secrets)));
        }
        out.push_str(&format!(
            " {} | {}ms |\n",
            status_verdict_label(cell.report.status),
            cell.report.duration_ms,
        ));
    }
    out
}

/// Render a [`BenchReport`] as a Markdown metrics block.
pub fn bench_to_markdown(report: &BenchReport, secrets: &[String]) -> String {
    let flaky = if report.is_flaky() { " (FLAKY)" } else { "" };
    let s = &report.stats;
    let mut out = String::new();
    out.push_str(&format!(
        "### ptytest bench: {} — {}/{} passed{}\n\n",
        mask_then_escape(&report.scenario, secrets),
        report.pass_count,
        report.runs,
        flaky,
    ));
    out.push_str("| Metric | Value |\n");
    out.push_str("| --- | --- |\n");
    out.push_str(&format!(
        "| runs | {} ({} warmup) |\n",
        report.runs, report.warmup
    ));
    out.push_str(&format!("| min | {}ms |\n", s.min));
    out.push_str(&format!("| median | {}ms |\n", s.median));
    out.push_str(&format!("| mean | {}ms |\n", s.mean));
    out.push_str(&format!("| p95 | {}ms |\n", s.p95));
    out.push_str(&format!("| max | {}ms |\n", s.max));
    out.push_str(&format!("| stddev | {}ms |\n", s.stddev));
    out
}

/// Escape a value for a Markdown table cell.
///
/// A literal `|` would split a cell and a newline would break the row, so both
/// are neutralized: `|` is backslash-escaped and newlines become spaces. Why
/// not full Markdown escaping: step labels and detail strings are short,
/// human-facing diagnostics, and over-escaping would make them harder to read;
/// only the two characters that actually corrupt table layout are handled.
fn escape_table_cell(value: &str) -> String {
    value.replace('|', "\\|").replace(['\r', '\n'], " ")
}

/// Mask secrets in `value`, then escape it for a Markdown table cell.
///
/// The mask must run first: [`escape_table_cell`] rewrites `|`/newlines, and a
/// secret containing one of those would no longer be a literal substring for
/// [`mask_secrets`] to find if masking ran afterward — it would leak.
fn mask_then_escape(value: &str, secrets: &[String]) -> String {
    escape_table_cell(&mask_secrets(value, secrets))
}

/// Emit the step summary and failure annotations for a single-scenario run.
///
/// Called only when [`github_enabled`] is true. Annotations are emitted for each
/// failed assertion so they surface inline in the PR; the summary captures the
/// full per-assertion table.
pub fn report_outputs(report: &Report, secrets: &[String]) {
    write_step_summary(&report_to_markdown(report, secrets), secrets);
    for assertion in &report.assertions {
        if assertion.passed {
            continue;
        }
        let title = format!("ptytest: {}", report.scenario);
        let message = match &assertion.message {
            Some(m) => format!("{} — {}", assertion.step, m),
            None => assertion.step.clone(),
        };
        emit_error_annotation(&title, &message, secrets);
    }
}

/// Emit the step summary and per-cell failure annotations for a matrix run.
pub fn matrix_outputs(report: &MatrixReport, secrets: &[String]) {
    write_step_summary(&matrix_to_markdown(report, secrets), secrets);
    for cell in &report.cells {
        if cell.report.status == Status::Passed {
            continue;
        }
        let coords = cell
            .coords
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(" ");
        let title = format!("ptytest matrix: {}", cell.report.scenario);
        emit_error_annotation(&title, &format!("cell failed: {coords}"), secrets);
    }
}

/// Emit the step summary and a flakiness warning (if any) for a bench run.
pub fn bench_outputs(report: &BenchReport, secrets: &[String]) {
    write_step_summary(&bench_to_markdown(report, secrets), secrets);
    // Flakiness is a warning, not an error: bench exists to *observe* it, and
    // the exit code already reflects pass/fail. A bare-fail (0 passed) is not
    // flaky and gets no warning here — the failing exit code carries it.
    if report.is_flaky() {
        let message = format!(
            "{} is flaky: {}/{} runs passed",
            report.scenario, report.pass_count, report.runs
        );
        emit_warning_annotation(&message, secrets);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assert::AssertionResult;
    use crate::bench::Stats;
    use crate::matrix::MatrixCell;
    use std::collections::BTreeMap;

    #[test]
    fn github_enabled_is_or_of_flag_and_env() {
        // The flag forces output on regardless of the env (mirrors --update).
        assert!(github_enabled(true));
    }

    #[test]
    fn is_github_actions_detects_true_value() {
        // Only the literal "true" counts; any other value (or unset) is false.
        let _guard = crate::ENV_TEST_LOCK.lock().unwrap();
        std::env::set_var(GITHUB_ACTIONS_ENV, "true");
        assert!(is_github_actions());
        std::env::set_var(GITHUB_ACTIONS_ENV, "false");
        assert!(!is_github_actions());
        std::env::remove_var(GITHUB_ACTIONS_ENV);
        assert!(!is_github_actions());
    }

    #[test]
    fn escape_data_encodes_newline_and_percent() {
        // A newline must become %0A and a percent %25 (percent first) so the
        // annotation is neither truncated at the newline nor double-encoded.
        assert_eq!(escape_data("a\nb"), "a%0Ab");
        assert_eq!(escape_data("100%\n"), "100%25%0A");
        assert_eq!(escape_data("a\r\nb"), "a%0D%0Ab");
    }

    #[test]
    fn escape_property_encodes_colon_and_comma() {
        // Property values additionally encode the `:` and `,` delimiters.
        assert_eq!(escape_property("a:b,c"), "a%3Ab%2Cc");
    }

    #[test]
    fn error_annotation_masks_secret_in_title_and_message() {
        // A secret appearing in either the title or the message must be replaced
        // by *** before the annotation text is built, so it never reaches the log.
        let secrets = vec!["s3cr3t".to_string()];
        // Capture by reconstructing the same transformation the emitter uses.
        let title = escape_property(&mask_secrets("tok s3cr3t", &secrets));
        let message = escape_data(&mask_secrets("value=s3cr3t failed", &secrets));
        assert!(!title.contains("s3cr3t"));
        assert!(!message.contains("s3cr3t"));
        assert!(title.contains("***"));
        assert!(message.contains("***"));
    }

    #[test]
    fn error_annotation_masks_secret_with_property_delimiters() {
        // (G-3/G-4) The annotation escape path rewrites `:` and `,` (property
        // escapes) and newlines (data escapes). A secret containing any of those
        // must still be masked: masking runs *before* escaping, so the literal
        // secret is replaced with *** and the per-character escapes only touch
        // the surrounding text — the raw secret can never reach the annotation.
        let secrets = vec!["a:b,c".to_string()];
        // Drive the real emitter builder so a future change to its ordering is
        // caught. The secret sits in BOTH the title and the message.
        let line = format_error_annotation("token a:b,c here", "saw a:b,c in output", &secrets);
        // Neither the raw secret nor its escaped form (`a%3Ab%2Cc`) may appear.
        assert!(!line.contains("a:b,c"));
        assert!(!line.contains("a%3Ab%2Cc"));
        assert!(line.contains("***"));
    }

    #[test]
    fn error_annotation_masks_secret_containing_newline() {
        // (G-3/G-4) A secret spanning a newline (the data terminator GitHub
        // encodes as %0A) must be masked before escaping, so neither the raw
        // newline-bearing value nor its `%0A`-encoded form leaks into the title
        // or the message.
        let secrets = vec!["line1\nline2".to_string()];
        let line = format_error_annotation("tok line1\nline2", "got line1\nline2!", &secrets);
        assert!(!line.contains("line1\nline2"));
        assert!(!line.contains("line1%0Aline2"));
        assert!(line.contains("***"));
    }

    #[test]
    fn error_annotation_masks_multiple_distinct_secrets() {
        // (G-5) When a scenario declares several secrets, every one of them must
        // be masked in an annotation, not just the first — the masker iterates
        // the full set.
        let secrets = vec!["alpha".to_string(), "bravo".to_string()];
        let line = format_error_annotation("tok alpha", "saw bravo too", &secrets);
        assert!(!line.contains("alpha"));
        assert!(!line.contains("bravo"));
        assert!(line.contains("***"));
    }

    #[test]
    fn warning_annotation_masks_secret() {
        // (G-6) The flaky-bench warning path masks secrets too: a secret value
        // appearing in the warning message must be redacted before the line is
        // built, so a flaky bench cannot leak a secret into the annotation.
        let secrets = vec!["s3cr3t".to_string()];
        let line = format_warning_annotation("bench s3cr3t is flaky", &secrets);
        assert!(!line.contains("s3cr3t"));
        assert!(line.contains("***"));
        assert!(line.starts_with("::warning::"));
    }

    #[test]
    fn report_markdown_masks_secret_via_outputs_path() {
        // The Markdown built from a report carrying a secret in a message must,
        // after masking, contain no secret value.
        let report = Report {
            scenario: "s".into(),
            status: Status::Failed,
            duration_ms: 5,
            assertions: vec![AssertionResult::fail("expect: x", "saw s3cr3t")],
        };
        let md = report_to_markdown(&report, &["s3cr3t".to_string()]);
        assert!(!md.contains("s3cr3t"));
        assert!(md.contains("***"));
        assert!(md.contains("| FAIL |"));
    }

    #[test]
    fn report_markdown_masks_secret_containing_a_pipe_before_escaping() {
        // A secret value that contains a table-delimiter `|` must still be masked
        // in the cell. If escaping ran before masking, `a|b` would become `a\|b`
        // and the literal-substring masker would miss it, leaking the secret.
        let report = Report {
            scenario: "s".into(),
            status: Status::Failed,
            duration_ms: 1,
            assertions: vec![AssertionResult::fail("step", "leaked a|b here")],
        };
        let md = report_to_markdown(&report, &["a|b".to_string()]);
        assert!(!md.contains("a|b"));
        assert!(!md.contains("a\\|b"));
        assert!(md.contains("***"));
    }

    #[test]
    fn report_markdown_has_heading_and_table() {
        // The single-run summary must carry a verdict heading and one table row
        // per assertion.
        let report = Report {
            scenario: "demo".into(),
            status: Status::Passed,
            duration_ms: 12,
            assertions: vec![
                AssertionResult::pass("expect: hi"),
                AssertionResult::pass("expect_exit: 0"),
            ],
        };
        let md = report_to_markdown(&report, &[]);
        assert!(md.contains("### ptytest: demo — PASS"));
        assert_eq!(md.matches("| PASS |").count(), 2);
    }

    /// Build a one-axis matrix report with the given per-cell statuses.
    fn matrix_report(statuses: &[(&str, Status)]) -> MatrixReport {
        let cells = statuses
            .iter()
            .map(|(value, status)| {
                let mut coords = BTreeMap::new();
                coords.insert("command".to_string(), value.to_string());
                MatrixCell {
                    coords,
                    report: Report {
                        scenario: "m".into(),
                        status: *status,
                        duration_ms: 3,
                        assertions: Vec::new(),
                    },
                }
            })
            .collect();
        MatrixReport {
            axes: vec!["command".to_string()],
            cells,
        }
    }

    #[test]
    fn matrix_markdown_counts_and_columns() {
        // The matrix summary heading must show passed/total and the table must
        // carry one column per axis plus result/duration.
        let report = matrix_report(&[("a", Status::Passed), ("b", Status::Failed)]);
        let md = matrix_to_markdown(&report, &[]);
        assert!(md.contains("1/2 passed"));
        assert!(md.contains("| command | Result | Duration |"));
        assert!(md.contains("| a | PASS |"));
        assert!(md.contains("| b | FAIL |"));
    }

    #[test]
    fn matrix_markdown_multi_axis_has_one_column_per_axis() {
        // (G-2) For a multi-axis matrix the table header and separator row must
        // each carry one column per axis (in axis order) plus Result and
        // Duration, so a multi-axis cell is self-describing and the layout stays
        // valid Markdown (separator column count must match the header).
        let mut coords = BTreeMap::new();
        coords.insert("command".to_string(), "echo".to_string());
        coords.insert("region".to_string(), "us".to_string());
        let report = MatrixReport {
            axes: vec!["command".to_string(), "region".to_string()],
            cells: vec![MatrixCell {
                coords,
                report: Report {
                    scenario: "m".into(),
                    status: Status::Passed,
                    duration_ms: 2,
                    assertions: Vec::new(),
                },
            }],
        };
        let md = matrix_to_markdown(&report, &[]);
        // Header lists both axes (in key order) then Result/Duration.
        assert!(md.contains("| command | region | Result | Duration |"));
        // The separator row must have 4 columns (2 axes + result + duration) to
        // match the header, or GitHub will not render the table.
        let separator = md
            .lines()
            .find(|l| l.contains("---"))
            .expect("table must have a separator row");
        assert_eq!(
            separator.matches("---").count(),
            4,
            "separator column count must equal axis count + 2: {separator:?}"
        );
        // The single cell renders both coordinates in axis order.
        assert!(md.contains("| echo | us | PASS |"));
    }

    #[test]
    fn bench_markdown_shows_metrics_and_flaky_marker() {
        // A flaky bench (some but not all passed) must mark FLAKY and list every
        // metric row.
        let report = BenchReport {
            scenario: "b".into(),
            runs: 4,
            warmup: 1,
            pass_count: 2,
            durations: vec![10, 20, 30, 40],
            stats: Stats {
                min: 10,
                max: 40,
                mean: 25,
                median: 25,
                p95: 40,
                stddev: 11,
            },
        };
        let md = bench_to_markdown(&report, &[]);
        assert!(md.contains("2/4 passed (FLAKY)"));
        assert!(md.contains("| median | 25ms |"));
        assert!(md.contains("| stddev | 11ms |"));
    }

    #[test]
    fn escape_table_cell_neutralizes_pipe_and_newline() {
        // A literal pipe must be escaped and a newline collapsed to a space so a
        // value cannot corrupt the table layout.
        assert_eq!(escape_table_cell("a|b\nc"), "a\\|b c");
    }

    #[test]
    fn write_step_summary_appends_and_masks() {
        // write_step_summary must append (not truncate) to the file named by the
        // env var and mask secrets in the written text.
        let _guard = crate::ENV_TEST_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("summary.md");
        std::fs::write(&path, "existing\n").unwrap();
        std::env::set_var(STEP_SUMMARY_ENV, &path);
        write_step_summary("token s3cr3t here", &["s3cr3t".to_string()]);
        std::env::remove_var(STEP_SUMMARY_ENV);
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.starts_with("existing\n"));
        assert!(body.contains("token *** here"));
        assert!(!body.contains("s3cr3t"));
    }

    #[test]
    fn write_step_summary_without_env_is_silent_noop() {
        // With no GITHUB_STEP_SUMMARY set, writing is a no-op and must not panic.
        let _guard = crate::ENV_TEST_LOCK.lock().unwrap();
        std::env::remove_var(STEP_SUMMARY_ENV);
        write_step_summary("anything", &[]);
    }
}
