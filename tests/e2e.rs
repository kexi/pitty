//! End-to-end integration tests exercising the full PTY path.
//!
//! These spawn a real shell inside a PTY. They are marked `#[ignore]` because
//! CI or sandboxed environments may lack a usable PTY; run them explicitly
//! with `cargo test -- --ignored` where a PTY is available.
//!
//! Fallback status: the framework now dogfoods itself via `pitty run` over
//! the scenarios in `e2e/scenarios/` (positive tier in `positive/`, nested-PTY
//! "meta" tier in `meta/`). The five positive cases that used to live here have
//! moved to `e2e/scenarios/positive/` and were removed from this file. The
//! negative cases below were also reproduced as dogfood "meta" scenarios
//! (`meta/inner/{timeout-fail,empty-spawn,secret-leak}.yaml` verified by
//! `meta/verify-*.yaml`), but are KEPT here as a fallback until the dogfood
//! pipeline is proven stable in CI across runners.
//!
//! Intermediate consolidation (v0.4): the redundant secret-masking PTY test
//! (`secret_is_masked_in_json_report_on_timeout`) was removed from this file.
//! The stdout-report masking path is now covered without a PTY by the white-box
//! unit test `runner::mask_report_redacts_secrets_in_assertion_messages_and_name`,
//! and the real PTY path by the dogfood `meta/verify-secret-masked.yaml`. Dropping
//! the copy here also eliminated the three-place `supersecretvalue` sync hazard
//! (the literal now lives only in the two YAML files). The remaining `#[ignore]`
//! tests below keep direct white-box value (e.g. asserting `report.status ==
//! Status::Failed`) that the PTY-free unit/dogfood split does not reproduce.
//!
//! TODO(decommission): delete this file once the nested-PTY meta tier has run
//! green on BOTH the `pty-e2e` (ubuntu) and `pty-e2e-macos` (macOS) gating jobs
//! (job keys in `.github/workflows/ci.yml`) for 5 consecutive CI runs on the
//! default branch, so the e2e surface lives solely in the dogfood scenarios.
//! Until then both copies are intentional duplication. This condition is the
//! single source of truth, also referenced from `e2e/README.md`.
//! No tracking issue is filed yet (the repo has no CI history to satisfy the
//! condition above); open one and link it here when the meta tier first goes
//! green on both gates, so the decommission can be tracked to completion.

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use pitty::bench::run_bench;
use pitty::config::Scenario;
use pitty::matrix::run_matrix;
use pitty::report::Status;
use pitty::run_scenario;
use pitty::runner::RunOptions;

/// Serializes every real-PTY test in this file so at most one is allocating PTYs
/// at a time.
///
/// Why not run them in parallel (cargo's default): each `#[ignore]` test below
/// spawns a real shell via `openpty`, and matrix tests spawn one PTY per cell.
/// Under default parallelism the concurrent PTY allocations exhaust the OS slave
/// PTY limit and `openpty` fails with errno -6 (`EBADF`/ENXIO depending on
/// platform), making the suite flaky (~1/5 runs). These tests are few and quick,
/// so full serialization via a single mutex is cheap and robustly removes the
/// resource race.
///
/// Why not a counting semaphore (N>1 parallelism): it would still let N PTYs
/// allocate at once, and since a single matrix test itself spawns several PTYs
/// the exhaustion window is not eliminated — only narrowed. A single lock makes
/// the bound exact (one test's PTYs at a time).
///
/// Why not `--test-threads=1` in CI only: serializing in the test code keeps a
/// plain `cargo test --test e2e -- --ignored` stable, so the suite is not
/// dependent on a harness flag a developer might omit locally.
static PTY_TEST_LOCK: Mutex<()> = Mutex::new(());

/// Acquires the PTY serialization lock, ignoring poisoning.
///
/// Why not `.lock().unwrap()` (as `ENV_TEST_LOCK` uses): a panicking PTY test
/// (e.g. an assertion failure, or the very `openpty` exhaustion this lock
/// guards against during regression triage) poisons the mutex. Propagating the
/// poison would turn one test's failure into a cascade of unrelated failures in
/// every later PTY test, hiding the real culprit. Recovering the guard keeps the
/// tests independent.
fn pty_lock() -> MutexGuard<'static, ()> {
    PTY_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// An `expect` for output that never appears must time out and fail (not hang),
/// proving the deadline path works end to end.
///
/// Dogfood equivalent: `e2e/scenarios/meta/inner/timeout-fail.yaml`, verified by
/// `e2e/scenarios/meta/verify-timeout-fails.yaml`.
#[test]
#[ignore = "requires a usable PTY"]
fn expect_times_out_on_absent_output() {
    let _pty = pty_lock();
    let yaml = r#"
name: timeout-flow
workspace:
  temp: true
steps:
  - spawn: bash
  - send: echo hello
  - expect:
      contains: this-never-appears
      timeout: 1s
"#;
    let scenario = Scenario::from_yaml(yaml).unwrap();
    let report = run_scenario(&scenario, Path::new("."), &RunOptions::default()).unwrap();
    assert_eq!(report.status, Status::Failed);
    assert!(report.assertions.iter().any(|a| !a.passed));
}

/// An empty `spawn` command must surface as a process error (exit code 3)
/// rather than spawning an empty program. Gated on a PTY because the failure is
/// detected after `openpty` succeeds.
///
/// Dogfood equivalent: `e2e/scenarios/meta/inner/empty-spawn.yaml`, verified by
/// `e2e/scenarios/meta/verify-empty-spawn-errors.yaml`.
#[test]
#[ignore = "requires a usable PTY"]
fn empty_spawn_command_is_process_error() {
    let _pty = pty_lock();
    let yaml = r#"
name: empty-spawn
workspace:
  temp: true
steps:
  - spawn: "   "
"#;
    let scenario = Scenario::from_yaml(yaml).unwrap();
    let err = run_scenario(&scenario, Path::new("."), &RunOptions::default()).unwrap_err();
    assert_eq!(err.exit_code(), 3);
}

/// The deadline form of `expect_exit` must wait for a child that exits *after*
/// the step is reached (here a child that sleeps before exiting) and then pass,
/// proving the poll-until-deadline path removes the dependence on a preceding
/// fixed `wait` being long enough.
#[test]
#[ignore = "requires a usable PTY"]
fn expect_exit_deadline_waits_for_slow_child() {
    let _pty = pty_lock();
    let yaml = r#"
name: exit-deadline
workspace:
  temp: true
steps:
  # `sleep 1` exits 0 after a delay; whitespace-split spawn handles it as one
  # program + one arg (no shell quoting needed). The deadline form must wait out
  # the delay and then observe exit 0.
  - spawn: sleep 1
  - expect_exit:
      code: 0
      timeout: 10s
"#;
    let scenario = Scenario::from_yaml(yaml).unwrap();
    let report = run_scenario(&scenario, Path::new("."), &RunOptions::default()).unwrap();
    assert_eq!(report.status, Status::Passed);
    assert!(report.assertions.iter().all(|a| a.passed));
}

/// The deadline form must still FAIL (not hang) when the child never exits
/// within the deadline: a long-running child polled with a short timeout yields
/// "still running", driving the assertion to fail at the deadline.
#[test]
#[ignore = "requires a usable PTY"]
fn expect_exit_deadline_fails_when_child_outlives_timeout() {
    let _pty = pty_lock();
    let yaml = r#"
name: exit-deadline-timeout
workspace:
  temp: true
steps:
  # A 30s sleep outlives the 200ms deadline, so the poll must give up and the
  # assertion must fail (still running) rather than hang.
  - spawn: sleep 30
  - expect_exit:
      code: 0
      timeout: 200ms
"#;
    let scenario = Scenario::from_yaml(yaml).unwrap();
    let report = run_scenario(&scenario, Path::new("."), &RunOptions::default()).unwrap();
    assert_eq!(report.status, Status::Failed);
    assert!(report.assertions.iter().any(|a| !a.passed));
}

/// Dogfood `expect_json` with `source: {file}` against pitty's OWN JSON
/// report: run an inner scenario, persist its report JSON to a file, then have
/// an outer scenario assert the report's `status` field via `expect_json`.
///
/// This is the self-verification the design calls for: the JSON-extraction and
/// path-navigation machinery is exercised against the framework's own output
/// shape, so a regression in either the report format or the assertion is
/// caught here. It needs a PTY because the inner scenario spawns a real shell.
#[test]
#[ignore = "requires a usable PTY"]
fn expect_json_verifies_pitty_own_report_from_file() {
    let _pty = pty_lock();
    let dir = tempfile::tempdir().unwrap();

    // 1. Run an inner scenario that passes, and write its report JSON to a file
    //    inside the temp dir so the outer scenario can read it.
    let inner_yaml = r#"
name: inner-pass
workspace:
  temp: true
steps:
  - spawn: bash
  - send: echo hello
  - expect:
      contains: hello
      timeout: 10s
"#;
    let inner = Scenario::from_yaml(inner_yaml).unwrap();
    let inner_report = run_scenario(&inner, Path::new("."), &RunOptions::default()).unwrap();
    assert_eq!(inner_report.status, Status::Passed);
    let report_path = dir.path().join("report.json");
    std::fs::write(&report_path, inner_report.to_json()).unwrap();

    // 2. An outer scenario reads that report file via `expect_json` and asserts
    //    the framework's own `status` is "passed".
    let outer_yaml = r#"
name: verify-inner-report
steps:
  - expect_json:
      path: status
      equals: passed
      source:
        file: report.json
  - expect_json:
      path: scenario
      contains: inner
      source:
        file: report.json
"#;
    let outer = Scenario::from_yaml(outer_yaml).unwrap();
    // base_dir = the temp dir, so `report.json` resolves to the file we wrote.
    let outer_report = run_scenario(&outer, dir.path(), &RunOptions::default()).unwrap();
    assert_eq!(outer_report.status, Status::Passed);
    assert!(
        outer_report.assertions.iter().all(|a| a.passed),
        "expect_json over pitty's own report must pass: {:?}",
        outer_report.assertions
    );
}

/// A single-axis matrix over AI-tool-independent shell commands must produce one
/// cell per value, each driving its `${command}` into the spawned line. Here two
/// `echo`-style commands print the matched substring, so both cells pass and the
/// matrix exit code is 0. Proves the inject-clone-run pipeline end to end.
#[test]
#[ignore = "requires a usable PTY"]
fn matrix_runs_one_cell_per_value_over_shell_commands() {
    let _pty = pty_lock();
    let yaml = r#"
name: matrix-shell
workspace:
  temp: true
matrix:
  command: ["echo matched", "printf 'matched\\n'"]
steps:
  - spawn: bash
  - send: "${command}"
  - expect:
      contains: matched
      timeout: 10s
"#;
    let scenario = Scenario::from_yaml(yaml).unwrap();
    let report = run_matrix(&scenario, Path::new("."), &RunOptions::default()).unwrap();
    assert_eq!(report.axes, vec!["command".to_string()]);
    assert_eq!(report.cells.len(), 2);
    assert!(
        report
            .cells
            .iter()
            .all(|c| c.report.status == Status::Passed),
        "both shell cells must pass: {:?}",
        report.cells
    );
    assert_eq!(report.worst_exit_code(), 0);
}

/// A two-axis matrix must expand to the Cartesian product of its axes: 2 commands
/// x 2 regions = 4 cells, each injecting both `${command}` and `${region}` into a
/// single shell line. Both echo-style commands always print the matched
/// substring, so all 4 cells pass and the matrix exit code is 0. Proves the
/// multi-axis inject-clone-run pipeline and the product expansion end to end.
#[test]
#[ignore = "requires a usable PTY"]
fn matrix_expands_two_axes_to_their_cartesian_product() {
    let _pty = pty_lock();
    let yaml = r#"
name: matrix-two-axis
workspace:
  temp: true
matrix:
  command: ["echo", "printf"]
  region: ["us", "eu"]
steps:
  - spawn: bash
  - send: "${command} matched-${region}"
  - expect:
      contains: "matched-"
      timeout: 10s
"#;
    let scenario = Scenario::from_yaml(yaml).unwrap();
    let report = run_matrix(&scenario, Path::new("."), &RunOptions::default()).unwrap();
    assert_eq!(
        report.axes,
        vec!["command".to_string(), "region".to_string()]
    );
    assert_eq!(report.cells.len(), 4, "2 x 2 product must yield 4 cells");
    assert!(
        report
            .cells
            .iter()
            .all(|c| c.report.status == Status::Passed),
        "every product cell must pass: {:?}",
        report.cells
    );
    assert_eq!(report.worst_exit_code(), 0);
}

/// A matrix cell whose injected command makes the assertion fail must surface as
/// a failing cell (status Failed) while the passing cell stays green, so the
/// matrix worst exit code is the assertion class (1). Confirms per-cell verdicts
/// are independent and aggregation gates on the worst.
#[test]
#[ignore = "requires a usable PTY"]
fn matrix_failing_cell_drives_worst_exit_code() {
    let _pty = pty_lock();
    let yaml = r#"
name: matrix-mixed
workspace:
  temp: true
matrix:
  command: ["echo present", "echo absent"]
steps:
  - spawn: bash
  - send: "${command}"
  - expect:
      contains: present
      timeout: 5s
"#;
    let scenario = Scenario::from_yaml(yaml).unwrap();
    let report = run_matrix(&scenario, Path::new("."), &RunOptions::default()).unwrap();
    assert_eq!(report.cells.len(), 2);
    // The "echo present" cell passes; "echo absent" never prints "present" and
    // times out, so its cell fails.
    assert_eq!(report.worst_exit_code(), 1);
}

/// (AC-9) Meta dogfood: a `MatrixReport`'s own `--json` shape must be verifiable
/// by `expect_json` using the v0.4 bracket-notation paths. This pins the
/// destructive JSON format change (`axes` array + per-cell `coords`) AND the
/// bracket path grammar (`axes[0]`, `cells[0].coords["command"]`) together, so a
/// regression in either the matrix serialization or the path navigator is caught
/// against the framework's own output. Needs a PTY because the matrix spawns a
/// real shell per cell.
#[test]
#[ignore = "requires a usable PTY"]
fn matrix_report_json_is_self_verifiable_via_bracket_paths() {
    let _pty = pty_lock();
    let dir = tempfile::tempdir().unwrap();

    // 1. Run a single-cell matrix (one axis, one value) so the produced JSON shape
    //    is fully determined: axes == ["command"], cells[0].coords["command"] is
    //    the injected value, and the cell's report passed.
    let matrix_yaml = r#"
name: matrix-selfcheck
workspace:
  temp: true
matrix:
  command: ["echo matched"]
steps:
  - spawn: bash
  - send: "${command}"
  - expect:
      contains: matched
      timeout: 10s
"#;
    let matrix_scenario = Scenario::from_yaml(matrix_yaml).unwrap();
    let matrix_report =
        run_matrix(&matrix_scenario, Path::new("."), &RunOptions::default()).unwrap();
    assert_eq!(matrix_report.cells.len(), 1);
    let report_path = dir.path().join("matrix.json");
    std::fs::write(&report_path, matrix_report.to_json()).unwrap();

    // 2. An outer scenario reads that matrix JSON via `expect_json` and addresses
    //    it with bracket-notation paths: the first axis name, the first cell's
    //    injected coordinate (an object key the dotted form could index), and the
    //    first cell's report status — exercising the v0.4 format end to end.
    let verify_yaml = r#"
name: verify-matrix-report
steps:
  - expect_json:
      path: axes[0]
      equals: command
      source:
        file: matrix.json
  - expect_json:
      path: cells[0].coords["command"]
      equals: echo matched
      source:
        file: matrix.json
  - expect_json:
      path: cells[0].report.status
      equals: passed
      source:
        file: matrix.json
"#;
    let verify = Scenario::from_yaml(verify_yaml).unwrap();
    let verify_report = run_scenario(&verify, dir.path(), &RunOptions::default()).unwrap();
    assert_eq!(verify_report.status, Status::Passed);
    assert!(
        verify_report.assertions.iter().all(|a| a.passed),
        "expect_json over pitty's own matrix report must pass: {:?}",
        verify_report.assertions
    );
}

/// (R-5) A hard fault in a matrix cell aborts the matrix and is NOT suppressed
/// by `--no-fail`: `run_matrix` returns `Err` (process class) so the CLI exits 3
/// regardless of `--no-fail`, and the cell after the faulting one never runs.
/// Here the FIRST cell spawns a non-existent program (spawn failure -> process
/// error), so `run_matrix` returns Err before reaching the second cell.
#[test]
#[ignore = "requires a usable PTY"]
fn matrix_process_fault_aborts_and_is_not_masked_by_no_fail() {
    let _pty = pty_lock();
    let yaml = r#"
name: matrix-fault
workspace:
  temp: true
matrix:
  command: ["definitely-not-a-real-binary-xyzzy", "echo second"]
steps:
  - spawn: "${command}"
  - expect:
      contains: second
      timeout: 5s
"#;
    let scenario = Scenario::from_yaml(yaml).unwrap();
    // The first cell's spawn fails, so run_matrix returns an Err (the second cell
    // is never reached) rather than an Ok report with per-cell statuses.
    let err = run_matrix(&scenario, Path::new("."), &RunOptions::default())
        .expect_err("a spawn fault in a cell must abort the matrix as an Err");
    // Process class (3): --no-fail only suppresses the Ok-report assertion path;
    // the CLI maps this Err straight through PittyError::exit_code, so the
    // process exits 3 even with --no-fail.
    assert_eq!(err.exit_code(), 3);
}

/// (R-7) A matrix cell whose `expect_snapshot` has no recorded snapshot must
/// FAIL (matrix never records: RunOptions::default has update=false and there is
/// no --update flag). This proves the no-update contract by observing the
/// behavioral consequence: an absent snapshot is a failing cell, not a silent
/// record-and-pass.
#[test]
#[ignore = "requires a usable PTY"]
fn matrix_absent_snapshot_cell_fails_because_matrix_never_records() {
    let _pty = pty_lock();
    let yaml = r#"
name: matrix-snapshot-absent
workspace:
  temp: true
matrix:
  command: ["echo one", "echo two"]
steps:
  - spawn: bash
  - send: "${command}"
  - wait: 300ms
  # No snapshot exists in the fresh temp workspace, and matrix does not record,
  # so each cell's expect_snapshot must fail rather than pass.
  - expect_snapshot:
      file: out.snap
"#;
    let scenario = Scenario::from_yaml(yaml).unwrap();
    let report = run_matrix(&scenario, Path::new("."), &RunOptions::default()).unwrap();
    assert_eq!(report.cells.len(), 2);
    assert!(
        report
            .cells
            .iter()
            .all(|c| c.report.status == Status::Failed),
        "every cell with an absent snapshot must fail (matrix never records): {:?}",
        report.cells
    );
    // A failing cell drives the matrix worst exit code to the assertion class.
    assert_eq!(report.worst_exit_code(), 1);
}

/// (R-8) Each bench run must get a fresh temp workspace: a file one run writes
/// must NOT be visible to the next run. The scenario writes a marker file and
/// prints a `CLEAN`/`LEAK` verdict that depends on whether the file already
/// existed at spawn time. With a fresh temp dir per run the file never pre-exists
/// (`CLEAN` every time, all runs pass); a shared temp dir would print `LEAK` from
/// the 2nd run on and fail it. The run then confirms the file exists, proving the
/// write actually landed in the (fresh) workspace.
#[test]
#[ignore = "requires a usable PTY"]
fn bench_gives_each_run_a_fresh_temp_workspace() {
    let _pty = pty_lock();
    let yaml = r#"
name: bench-fresh-temp
workspace:
  temp: true
steps:
  - spawn: bash
  # Probe for a leaked file from a prior run. The verdict token is BUILT at
  # runtime (`VERDICT_` + the `ls` exit status word) so the success string
  # `VERDICT_absent` never appears literally in the command line — the matcher
  # therefore matches the command's OUTPUT, not the PTY echo of the input. A
  # leaked file makes `ls` succeed and prints `VERDICT_present`, failing the
  # expect; a fresh temp dir prints `VERDICT_absent`.
  - send: "ls leaked.txt >/dev/null 2>&1 && s=present || s=absent; echo VERDICT_$s"
  - expect:
      contains: VERDICT_absent
      timeout: 5s
  # Write a marker whose token (`MARKER_payload`) does not appear in this command
  # line, so the read-back expect below cannot match the PTY echo of the write.
  - send: "printf 'MARKER_%s\\n' payload > leaked.txt"
  # Confirm the write actually landed by reading it back through the PTY: the
  # matcher waits up to the timeout, so this is not a fixed-sleep race. The token
  # only appears in the file's CONTENT (not in the `cat` command echo), so a match
  # proves the write completed. Once seen, the file is guaranteed present for
  # expect_file_exists and the shell is idle so the exit below is clean —
  # important on macOS, whose PTY teardown is slower and made a fixed `wait` flake.
  - send: "cat leaked.txt"
  - expect:
      contains: MARKER_payload
      timeout: 5s
  - expect_file_exists:
      path: leaked.txt
  - send: exit
  - expect_exit:
      code: 0
      timeout: 5s
"#;
    let scenario = Scenario::from_yaml(yaml).unwrap();
    let report = run_bench(&scenario, Path::new("."), &RunOptions::default(), 3, 0).unwrap();
    assert_eq!(report.runs, 3);
    assert_eq!(
        report.pass_count, 3,
        "every run must see a fresh temp dir (no leaked file from a prior run): {report:?}"
    );
    assert!(!report.is_flaky());
}

/// Bench mode must repeat a deterministic, always-passing scenario N times and
/// report N measured durations with a full pass rate (not flaky). Exercises the
/// warmup-exclusion and statistics path over real PTY runs.
#[test]
#[ignore = "requires a usable PTY"]
fn bench_repeats_passing_scenario_and_reports_full_pass_rate() {
    let _pty = pty_lock();
    let yaml = r#"
name: bench-shell
workspace:
  temp: true
steps:
  - spawn: bash
  - send: echo hello
  - expect:
      contains: hello
      timeout: 10s
"#;
    let scenario = Scenario::from_yaml(yaml).unwrap();
    let report = run_bench(&scenario, Path::new("."), &RunOptions::default(), 3, 1).unwrap();
    assert_eq!(report.runs, 3);
    assert_eq!(report.warmup, 1);
    assert_eq!(report.durations.len(), 3, "warmup run must be excluded");
    assert_eq!(report.pass_count, 3);
    assert!(!report.is_flaky(), "a deterministic pass must not be flaky");
    assert!(report.stats.min <= report.stats.max);
}

/// Serializes tests that mutate the `GITHUB_STEP_SUMMARY` process env var, so a
/// concurrent test cannot observe another's setting. Separate from the PTY lock
/// because these tests allocate no PTY and need not contend with PTY tests.
static GH_ENV_LOCK: Mutex<()> = Mutex::new(());

/// `--github` on a `run` must append a Markdown step summary to the file named
/// by `$GITHUB_STEP_SUMMARY`, with secrets masked, while the exit code stays the
/// verdict. Uses a file-only scenario (no spawn) so it needs no PTY.
#[test]
fn github_run_writes_masked_markdown_step_summary() {
    use pitty::github;

    let _guard = GH_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    // A present file so the single assertion passes deterministically.
    std::fs::write(dir.path().join("present.txt"), b"x").unwrap();
    let summary = dir.path().join("summary.md");

    let yaml = r#"
name: gha-run
variables:
  tok:
    value: s3cr3t
    secret: true
steps:
  - expect_file_exists:
      path: present.txt
"#;
    let scenario = Scenario::from_yaml(yaml).unwrap();
    let report = run_scenario(&scenario, dir.path(), &RunOptions::default()).unwrap();
    assert_eq!(report.status, Status::Passed);

    std::env::set_var("GITHUB_STEP_SUMMARY", &summary);
    github::report_outputs(&report, &scenario.secret_values());
    std::env::remove_var("GITHUB_STEP_SUMMARY");

    let body = std::fs::read_to_string(&summary).unwrap();
    assert!(body.contains("### pitty: gha-run — PASS"));
    assert!(body.contains("| PASS |"));
    // The secret value must never appear in the summary text.
    assert!(!body.contains("s3cr3t"));
}

/// `--github` on a `matrix` must write a PASS/FAIL Markdown table summarizing
/// every cell. Marked `#[ignore]` because each cell spawns a real PTY.
#[test]
#[ignore = "requires a usable PTY"]
fn github_matrix_writes_passfail_table_summary() {
    use pitty::github;

    let _pty = pty_lock();
    let _guard = GH_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    let summary = dir.path().join("summary.md");

    let yaml = r#"
name: gha-matrix
workspace:
  temp: true
matrix:
  word: [hello, world]
steps:
  - spawn: bash
  - send: echo ${word}
  - expect:
      contains: ${word}
      timeout: 10s
"#;
    let scenario = Scenario::from_yaml(yaml).unwrap();
    let report = run_matrix(&scenario, dir.path(), &RunOptions::default()).unwrap();

    std::env::set_var("GITHUB_STEP_SUMMARY", &summary);
    github::matrix_outputs(&report, &scenario.secret_values());
    std::env::remove_var("GITHUB_STEP_SUMMARY");

    let body = std::fs::read_to_string(&summary).unwrap();
    assert!(body.contains("pitty matrix"));
    assert!(body.contains("| word | Result | Duration |"));
    assert!(body.contains("passed"));
}

/// Run the pitty binary once and return its exit code.
///
/// `with_github` toggles the `--github` flag. `GITHUB_STEP_SUMMARY` is pointed
/// at `summary` (an empty temp path) and `GITHUB_ACTIONS` is forced unset, so
/// the GitHub side effects are exercised in isolation without depending on the
/// test runner's own CI env. Returns the child's exit code (the verdict).
fn run_pitty_exit_code(args: &[&str], with_github: bool, summary: &Path) -> i32 {
    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_pitty"));
    cmd.args(args);
    if with_github {
        cmd.arg("--github");
    }
    // Set the summary target for the child only; force GITHUB_ACTIONS off so the
    // flag (not an ambient CI env) is the sole driver of the GitHub output.
    cmd.env("GITHUB_STEP_SUMMARY", summary)
        .env_remove("GITHUB_ACTIONS");
    let status = cmd.status().expect("pitty binary must launch");
    status
        .code()
        .expect("pitty must exit with a code, not a signal")
}

/// (G-7) The GitHub output is a pure side effect: emitting it must not change
/// the process exit code. For a `run` of a file-only scenario whose single
/// assertion FAILS (an absent file), the verdict is the assertion class (1) with
/// `--github` both off and on — the summary/annotations are written but the exit
/// code is identical. File-only, so no PTY is needed.
#[test]
fn github_flag_does_not_change_run_exit_code() {
    let dir = tempfile::tempdir().unwrap();
    // A scenario asserting a file that does not exist -> the run FAILS (exit 1),
    // so this also proves the invariance holds on the non-trivial (failing) path
    // where annotations are actually emitted.
    let scenario = dir.path().join("s.yaml");
    std::fs::write(
        &scenario,
        "name: gha-exit\nsteps:\n  - expect_file_exists:\n      path: absent.txt\n",
    )
    .unwrap();

    let summary_off = dir.path().join("off.md");
    let summary_on = dir.path().join("on.md");
    let path = scenario.to_str().unwrap();

    let off = run_pitty_exit_code(&["run", path], false, &summary_off);
    let on = run_pitty_exit_code(&["run", path], true, &summary_on);

    assert_eq!(off, 1, "the absent-file assertion must fail (exit 1)");
    assert_eq!(
        on, off,
        "--github must not change the run exit code (it is a side effect only)"
    );
    // The --github run must actually have emitted a summary (proving the side
    // effect ran), while the exit code stayed put.
    assert!(
        std::fs::read_to_string(&summary_on)
            .map(|b| b.contains("FAIL"))
            .unwrap_or(false),
        "--github run must write a FAIL summary"
    );
}

/// (G-7) The same exit-code invariance must hold for `matrix`: emitting the
/// per-cell summary/annotations does not change the worst-cell verdict. Here a
/// two-cell matrix has one passing and one failing cell, so the verdict is the
/// assertion class (1) with `--github` both off and on. Marked `#[ignore]`
/// because each cell spawns a real PTY.
#[test]
#[ignore = "requires a usable PTY"]
fn github_flag_does_not_change_matrix_exit_code() {
    let _pty = pty_lock();
    let dir = tempfile::tempdir().unwrap();
    let scenario = dir.path().join("m.yaml");
    std::fs::write(
        &scenario,
        r#"
name: gha-matrix-exit
workspace:
  temp: true
matrix:
  command: ["echo present", "echo absent"]
steps:
  - spawn: bash
  - send: "${command}"
  - expect:
      contains: present
      timeout: 5s
"#,
    )
    .unwrap();

    let summary_off = dir.path().join("off.md");
    let summary_on = dir.path().join("on.md");
    let path = scenario.to_str().unwrap();

    let off = run_pitty_exit_code(&["matrix", path], false, &summary_off);
    let on = run_pitty_exit_code(&["matrix", path], true, &summary_on);

    assert_eq!(off, 1, "a failing cell drives the matrix verdict to 1");
    assert_eq!(
        on, off,
        "--github must not change the matrix exit code (side effect only)"
    );
}
