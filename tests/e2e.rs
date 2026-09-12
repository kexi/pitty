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
use std::time::{Duration, Instant};

use pitty::bench::run_bench;
use pitty::config::Scenario;
use pitty::matrix::run_matrix;
use pitty::pty::{ExpectOutcome, Matcher, PtySession, SplitMode, Teardown, SHUTDOWN_GRACE};
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

/// The bound `shutdown` promises: two grace periods (kill wait + handle
/// teardown) plus slack, derived from the crate's constant so a retuned grace
/// period cannot silently loosen or tighten this check. The healthy path
/// finishes in well under a second.
const SHUTDOWN_BOUND: Duration = Duration::from_secs(2 * SHUTDOWN_GRACE.as_secs() + 2);

/// Block until the shell is actually up: a computed needle (see
/// e2e/scenarios/positive/echo-flow.yaml) that only a running shell can print,
/// so what follows hits a live shell rather than one still starting.
fn wait_until_shell_is_up(session: &mut PtySession) {
    session
        .send_line("echo up-$((40+2))")
        .expect("send must succeed");
    let seen = session.wait_for(&Matcher::contains("up-42"), Duration::from_secs(10));
    assert!(
        matches!(seen, ExpectOutcome::Matched { .. }),
        "shell must come up before teardown: {seen:?}"
    );
}

/// Tear the session down and assert the healthy contract: bounded, the child
/// gone, and no stall reported.
fn assert_clean_bounded_shutdown(session: &mut PtySession) {
    let started = Instant::now();
    let outcome = session.shutdown();
    let elapsed = started.elapsed();

    assert!(
        elapsed < SHUTDOWN_BOUND,
        "shutdown must be bounded by its grace periods, took {elapsed:?}"
    );
    assert!(
        !session.is_running().expect("child status must be readable"),
        "the child must be gone after shutdown"
    );
    assert_eq!(
        outcome.expect("teardown must not fail"),
        Teardown::Clean,
        "teardown must complete cleanly"
    );
}

/// Teardown must stay bounded even when the child is racing its own exit:
/// `send: exit` immediately followed by `shutdown` is the sequence that stalled
/// for ~5 minutes on Windows ConPTY (Git for Windows bash). Deliberately no
/// readiness wait: the race is the point.
#[test]
#[ignore = "requires a usable PTY"]
fn shutdown_is_bounded_while_child_is_exiting() {
    let _pty = pty_lock();
    let dir = tempfile::tempdir().unwrap();
    let mut session = PtySession::spawn("bash", &SplitMode::default(), dir.path(), &[])
        .expect("bash must spawn inside a PTY");
    session.send_line("exit").expect("send must succeed");

    assert_clean_bounded_shutdown(&mut session);
}

/// The common teardown: an idle interactive shell that never asked to exit is
/// killed, its tree reaped, and the console handles released — promptly and
/// without reporting a stall.
#[test]
#[ignore = "requires a usable PTY"]
fn shutdown_kills_an_idle_shell_promptly() {
    let _pty = pty_lock();
    let dir = tempfile::tempdir().unwrap();
    let mut session = PtySession::spawn("bash", &SplitMode::default(), dir.path(), &[])
        .expect("bash must spawn inside a PTY");
    wait_until_shell_is_up(&mut session);

    assert_clean_bounded_shutdown(&mut session);
}

/// Unix: a background job left behind by a shell that has already exited
/// shares the shell's process group when job control is off (as under
/// `sh -c`), so teardown must sweep that group even though the direct child
/// is gone. Otherwise the job keeps the PTY open, the release times out, and
/// a run whose every assertion passed becomes a process error.
///
/// The job ignores SIGHUP because that is the only way it survives the shell:
/// the kernel HUPs the terminal's foreground group when the session leader
/// exits, so the case this guards against is a `nohup`ed job or a server
/// that treats HUP as "reload", not a plain `sleep`.
#[cfg(unix)]
#[test]
#[ignore = "requires a usable PTY"]
fn shutdown_sweeps_the_process_group_of_an_exited_child() {
    let _pty = pty_lock();
    let dir = tempfile::tempdir().unwrap();
    let mut session = PtySession::spawn("bash", &SplitMode::default(), dir.path(), &[])
        .expect("bash must spawn inside a PTY");
    // `set +m` turns job control off so the sleeper stays in bash's own
    // group; `trap '' HUP` is inherited across exec; `exit` then leaves the
    // sleeper behind, still holding the PTY.
    session
        .send_line("set +m; trap '' HUP; sleep 30 & echo $! > sleeper")
        .expect("send must succeed");
    wait_until_shell_is_up(&mut session);
    session.send_line("exit").expect("send must succeed");
    let exited = session
        .wait_exit_code_until(Instant::now() + Duration::from_secs(10))
        .expect("child status must be readable");
    assert!(
        exited.is_some(),
        "bash must exit on its own before teardown"
    );
    let sleeper: i32 = std::fs::read_to_string(dir.path().join("sleeper"))
        .expect("bash must record the sleeper pid")
        .trim()
        .parse()
        .expect("pid file must hold a pid");
    // Without this the post-teardown poll cannot tell "swept" from "never
    // started" (a `sleep` missing from PATH also yields a pid that is gone).
    // SAFETY: signal 0 only probes whether the pid exists.
    assert_eq!(
        unsafe { libc::kill(sleeper, 0) },
        0,
        "sleeper {sleeper} must still be alive after bash exits, or the test proves nothing"
    );

    assert_clean_bounded_shutdown(&mut session);

    // The sleeper was reparented when bash exited, so once killed it is
    // reaped by init; allow a moment for that before calling it survived.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        // SAFETY: signal 0 only probes whether the pid exists.
        let alive = unsafe { libc::kill(sleeper, 0) } == 0;
        if !alive {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "sleeper {sleeper} survived teardown (process group not swept)"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Windows: teardown must take the whole process tree with it, not just the
/// direct child. A background loop started by the shell keeps writing a
/// heartbeat file; once shutdown returns, the file must stop changing. This
/// is the grandchild case that kept the console host — and the reader
/// thread — alive for minutes before the job object existed. Windows-only
/// because Unix has no tree kill here (a SIGKILLed shell cannot forward
/// SIGHUP to its jobs), which is pre-existing and out of this test's scope.
#[cfg(windows)]
#[test]
#[ignore = "requires a usable PTY"]
fn shutdown_kills_grandchildren_on_windows() {
    let _pty = pty_lock();
    let dir = tempfile::tempdir().unwrap();
    let heartbeat = dir.path().join("heartbeat");
    let mut session = PtySession::spawn("bash", &SplitMode::default(), dir.path(), &[])
        .expect("bash must spawn inside a PTY");
    // The heartbeat write is the builtin `printf`, not an external `date`, so
    // it cannot silently fail on a minimal PATH and leave an empty file that
    // would compare equal before and after.
    session
        .send_line("(while :; do printf x >> heartbeat; sleep 0.2; done) &")
        .expect("send must succeed");
    wait_until_shell_is_up(&mut session);
    // Prove the grandchild is actually running before the kill: the file must
    // grow at least twice, not merely exist (a redirection creates it even if
    // nothing is ever written).
    let size = |path: &std::path::Path| std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut growths = 0;
    let mut last = 0;
    while growths < 2 {
        assert!(Instant::now() < deadline, "heartbeat loop never got going");
        std::thread::sleep(Duration::from_millis(50));
        let now = size(&heartbeat);
        if now > last {
            growths += 1;
            last = now;
        }
    }

    assert_clean_bounded_shutdown(&mut session);

    // Settle past any write that was in flight when the tree died, then the
    // size must hold still for a full second.
    std::thread::sleep(Duration::from_millis(300));
    let before = size(&heartbeat);
    std::thread::sleep(Duration::from_secs(1));
    let after = size(&heartbeat);
    assert_eq!(
        before, after,
        "the heartbeat loop must be dead after shutdown (grandchild survived)"
    );
}

/// `expect_file_changed` measures change since the *current* child was
/// spawned. A second `spawn` must re-baseline: a write made by the first child
/// is part of the world the second child starts in, not a change it made, so
/// asserting a change right after the second spawn must fail, and only a write
/// by the second child may satisfy it.
#[test]
#[ignore = "requires a usable PTY"]
fn respawn_rebaselines_expect_file_changed() {
    let _pty = pty_lock();
    let dir = tempfile::tempdir().unwrap();
    let yaml = r#"
name: respawn-baseline
steps:
  - spawn: bash
  - send: echo first > marker.txt; echo wrote-$((40+2))
  - expect:
      contains: wrote-42
      timeout: 10s
  - spawn: bash
  - expect_file_changed:
      path: marker.txt
  - send: echo second >> marker.txt; echo wrote-$((40+3))
  - expect:
      contains: wrote-43
      timeout: 10s
  - expect_file_changed:
      path: marker.txt
"#;
    let scenario = Scenario::from_yaml(yaml).unwrap();
    let report = run_scenario(&scenario, dir.path(), &RunOptions::default()).unwrap();

    let changed: Vec<bool> = report
        .assertions
        .iter()
        .filter(|a| a.step.starts_with("expect_file_changed"))
        .map(|a| a.passed)
        .collect();
    assert_eq!(
        changed,
        vec![false, true],
        "the first child's write must not count for the second child, but the \
         second child's own write must: {:?}",
        report.assertions
    );
    assert_eq!(report.status, Status::Failed);
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

/// Issue #34 case 1: a quoted argument must reach the child as ONE argument
/// with the quotes consumed, so the quote characters never appear in the
/// terminal output.
///
/// Why `expect_not` rather than `expect: "hello world"`: the bug's defining
/// property is that `contains: "hello world"` PASSES while the output is
/// wrong (`'hello world'` still contains the substring), so a positive
/// assertion cannot distinguish fixed from broken. Asserting the absence of
/// the literal quote is what fails before the fix and passes after.
#[cfg(unix)]
#[test]
#[ignore = "requires a usable PTY"]
fn quoted_spawn_argument_does_not_leak_quote_characters_into_output() {
    let _pty = pty_lock();
    let yaml = r#"
name: quote-demo
workspace:
  temp: true
steps:
  - spawn:
      command: "echo 'hello world'"
      split: posix
  - expect:
      contains: "hello world"
      timeout: 10s
  - expect_not:
      contains: "'"
"#;
    let scenario = Scenario::from_yaml(yaml).unwrap();
    let report = run_scenario(&scenario, Path::new("."), &RunOptions::default())
        .expect("the scenario must run to a verdict");
    assert_eq!(
        report.status,
        Status::Passed,
        "echo must print `hello world` with no quote characters: {report:?}"
    );
}

/// An unrecognized `split` keyword must run, not error — the way every pitty
/// released before the field existed runs it.
///
/// Direction one of the two-way compatibility check. Verified against the 1.2.2
/// binary, which reports `"status": "passed"` for this exact document because
/// nested unknown fields are deliberately lenient (`COMPATIBILITY.md`). An
/// earlier revision of this feature rejected it, which was the same class of
/// contract break as #34 reached through the fix for #34.
#[cfg(unix)]
#[test]
#[ignore = "requires a usable PTY"]
fn an_unknown_split_keyword_runs_under_the_default_rule() {
    let _pty = pty_lock();
    let yaml = r#"
name: unknown-split
workspace:
  temp: true
steps:
  - spawn:
      command: "echo 'hello world'"
      split: custom
  - expect:
      contains: "'hello world'"
      timeout: 10s
"#;
    let scenario = Scenario::from_yaml(yaml).unwrap();
    let report = run_scenario(&scenario, Path::new("."), &RunOptions::default())
        .expect("an unknown split keyword must not be a scenario error");
    assert_eq!(
        report.status,
        Status::Passed,
        "an unrecognized `split` must fall back to the default rule, as 1.2.2 does: {report:?}"
    );
}

/// A `split` value of any *type* must run under the default rule, not just an
/// unrecognized string.
///
/// `SpawnSpecRaw` is untagged, so a type mismatch at `split` fails the whole
/// variant match and rejects the entire `spawn` map — with a message that never
/// mentions `split`. 1.2.2 ignores the key whatever its type (verified against
/// the binary for each of these), so rejecting them would be the same contract
/// break as rejecting an unknown keyword, one type away.
#[cfg(unix)]
#[test]
#[ignore = "requires a usable PTY"]
fn a_non_string_split_value_runs_under_the_default_rule() {
    let _pty = pty_lock();
    for value in ["42", "true", "null", "[a, b]", "{mode: posix}"] {
        let yaml = format!(
            r#"
name: split-type
workspace:
  temp: true
steps:
  - spawn:
      command: "echo 'hello world'"
      split: {value}
  - expect:
      contains: "'hello world'"
      timeout: 10s
"#
        );
        let scenario = Scenario::from_yaml(&yaml)
            .unwrap_or_else(|e| panic!("`split: {value}` must parse, but: {e}"));
        let report = run_scenario(&scenario, Path::new("."), &RunOptions::default())
            .unwrap_or_else(|e| panic!("`split: {value}` must not be a scenario error: {e}"));
        assert_eq!(
            report.status,
            Status::Passed,
            "`split: {value}` must fall back to the default rule, as 1.2.2 does: {report:?}"
        );
    }
}

/// The v1 compatibility guarantee, end to end: a `spawn` that does not opt in
/// must tokenize exactly as pitty 1.2.2 did.
///
/// This is the regression the `split` field exists to prevent. Making POSIX
/// rules unconditional broke a real, contract-valid scenario: `echo 'hello
/// world'` recorded on 1.2.2 has the literal bytes `'hello world'` in its
/// snapshot, and the unconditional tokenizer produced `hello world`, failing
/// the scenario on a patch upgrade. Asserting the quote characters are still
/// present is what fails if the default ever flips again.
#[cfg(unix)]
#[test]
#[ignore = "requires a usable PTY"]
fn the_default_spawn_tokenization_still_leaks_quote_characters_like_1_2_2() {
    let _pty = pty_lock();
    let yaml = r#"
name: legacy-quote-demo
workspace:
  temp: true
steps:
  - spawn: "echo 'hello world'"
  - expect:
      contains: "'hello world'"
      timeout: 10s
"#;
    let scenario = Scenario::from_yaml(yaml).unwrap();
    let report = run_scenario(&scenario, Path::new("."), &RunOptions::default())
        .expect("the scenario must run to a verdict");
    assert_eq!(
        report.status,
        Status::Passed,
        "without `split: posix` the quotes must still reach the child verbatim: {report:?}"
    );
}

/// The default must not tighten validation either: a command line that
/// `split: posix` rejects has to keep running under the default rule.
///
/// `COMPATIBILITY.md` forbids turning a previously valid scenario into an
/// error within `1.x`, and `echo 'unterminated` is such a scenario — it ran
/// (splitting into mangled words) on every release before the opt-in existed.
#[cfg(unix)]
#[test]
#[ignore = "requires a usable PTY"]
fn the_default_spawn_tokenization_accepts_an_unterminated_quote() {
    let _pty = pty_lock();
    let yaml = r#"
name: legacy-bad-quote
workspace:
  temp: true
steps:
  - spawn: "echo 'unterminated"
  - expect:
      contains: "unterminated"
      timeout: 10s
"#;
    let scenario = Scenario::from_yaml(yaml).unwrap();
    let report = run_scenario(&scenario, Path::new("."), &RunOptions::default())
        .expect("the default rule must not reject a command POSIX rules cannot parse");
    assert_eq!(
        report.status,
        Status::Passed,
        "an unterminated quote must still spawn under the default rule: {report:?}"
    );
}

/// Issue #34 case 2, under `split: posix`: `sh -c '<script>'` must hand the
/// whole script to `sh` as one argument, so the child's exit code is the
/// script's own.
///
/// Before the fix argv was `["sh", "-c", "'exit", "3'"]`, `sh` failed to find
/// the program `'exit`, and the run reported exit code 2 — a wrong answer
/// pointing nowhere near the real cause.
#[cfg(unix)]
#[test]
#[ignore = "requires a usable PTY"]
fn shell_one_liner_reports_its_own_exit_code() {
    let _pty = pty_lock();
    let yaml = r#"
name: exit-codes
workspace:
  temp: true
steps:
  - spawn:
      command: "sh -c 'exit 3'"
      split: posix
  - expect_exit:
      code: 3
      timeout: 10s
"#;
    let scenario = Scenario::from_yaml(yaml).unwrap();
    let report = run_scenario(&scenario, Path::new("."), &RunOptions::default())
        .expect("the scenario must run to a verdict");
    assert_eq!(
        report.status,
        Status::Passed,
        "`sh -c 'exit 3'` must exit 3, not 2: {report:?}"
    );
}

/// Under `split: posix`, an unterminated quote is a command line the harness
/// cannot turn into a process: it must be a process error (exit 3) naming the
/// command, never a panic and never a silent fall back to the whitespace split
/// the author opted out of.
#[test]
#[ignore = "requires a usable PTY"]
fn unparseable_spawn_command_is_process_error() {
    let _pty = pty_lock();
    let yaml = r#"
name: bad-quoting
workspace:
  temp: true
steps:
  - spawn:
      command: "echo 'unterminated"
      split: posix
"#;
    let scenario = Scenario::from_yaml(yaml).unwrap();
    let err = run_scenario(&scenario, Path::new("."), &RunOptions::default()).unwrap_err();
    assert_eq!(err.exit_code(), 3);
    assert!(
        err.message().contains("echo 'unterminated"),
        "the error must name the offending command line: {}",
        err.message()
    );
}

/// A second `spawn` that fails must not cost the run its log: the first
/// session's output and the assertion rows recorded so far are what explain
/// the hard fault, and the hard-fault path promises they land on disk.
#[test]
#[ignore = "requires a usable PTY"]
fn failed_respawn_still_writes_the_log() {
    let _pty = pty_lock();
    let dir = tempfile::tempdir().unwrap();
    let yaml = r#"
name: respawn-log
workspace:
  temp: true
steps:
  - spawn: bash
  - send: echo first-$((40+2))
  - expect:
      contains: first-42
      timeout: 10s
  - spawn: "   "
"#;
    let scenario = Scenario::from_yaml(yaml).unwrap();
    let err = run_scenario(&scenario, dir.path(), &RunOptions::default()).unwrap_err();
    assert_eq!(err.exit_code(), 3);
    let log = std::fs::read_to_string(dir.path().join("logs/respawn-log.log"))
        .expect("the log must be written even though the second spawn failed");
    assert!(
        log.contains("first-42"),
        "log must carry the first session's output:\n{log}"
    );
}

/// A multi-spawn run's log must substantiate every assertion it lists: each
/// session's terminal output reaches the file, not only the last one's.
#[test]
#[ignore = "requires a usable PTY"]
fn multi_spawn_log_keeps_every_session_output() {
    let _pty = pty_lock();
    let dir = tempfile::tempdir().unwrap();
    let yaml = r#"
name: double-spawn-log
workspace:
  temp: true
steps:
  - spawn: bash
  - send: echo first-process
  - expect:
      contains: first-process
      timeout: 10s
  - spawn: bash
  - send: echo second-process
  - expect:
      contains: second-process
      timeout: 10s
"#;
    let scenario = Scenario::from_yaml(yaml).unwrap();
    let report = run_scenario(&scenario, dir.path(), &RunOptions::default()).unwrap();
    assert_eq!(report.status, Status::Passed);

    let log = std::fs::read_to_string(dir.path().join("logs/double-spawn-log.log")).unwrap();
    assert!(
        log.contains("first-process"),
        "the retired session's output must survive the respawn:\n{log}"
    );
    assert!(
        log.contains("second-process"),
        "the live session's output must be logged:\n{log}"
    );
}

/// Every matrix cell must keep its own log: the cell that failed is the one
/// whose terminal output the author needs, and it must not be overwritten by
/// whichever cell happens to run last.
#[test]
#[ignore = "requires a usable PTY"]
fn matrix_cells_each_write_their_own_log() {
    let _pty = pty_lock();
    let dir = tempfile::tempdir().unwrap();
    let yaml = r#"
name: matrix-log-demo
workspace:
  temp: true
matrix:
  word: [alpha, bravo, charlie]
steps:
  - spawn: bash
  - send: echo ${word}
  - expect:
      contains: ${word}
      timeout: 10s
"#;
    let scenario = Scenario::from_yaml(yaml).unwrap();
    let report = run_matrix(&scenario, dir.path(), &RunOptions::default()).unwrap();
    assert_eq!(report.total(), 3);

    for word in ["alpha", "bravo", "charlie"] {
        let path = dir
            .path()
            .join(format!("logs/matrix-log-demo.word-{word}.log"));
        // The scenario is built in-memory, so there is no file stem to prefix;
        // the cell coordinates alone separate the three logs.
        let log = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("cell {word} must keep its own log ({e})"));
        assert!(
            log.contains(word),
            "cell {word}'s log must carry its own output:\n{log}"
        );
    }
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

/// Run the pitty binary once with annotations forced on, returning its captured
/// `(stdout, stderr)`.
///
/// `GITHUB_ACTIONS=true` is set for the child so annotations auto-enable exactly
/// as they do on a runner — that is the environment the stream split has to hold
/// in. `GITHUB_STEP_SUMMARY` is pointed at `summary` so the summary write does
/// not fall back to some ambient path.
fn run_pitty_captured(args: &[&str], summary: &Path) -> (String, String) {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_pitty"))
        .args(args)
        .env("GITHUB_STEP_SUMMARY", summary)
        .env("GITHUB_ACTIONS", "true")
        .output()
        .expect("pitty binary must launch");
    (
        String::from_utf8(out.stdout).expect("stdout must be UTF-8"),
        String::from_utf8(out.stderr).expect("stderr must be UTF-8"),
    )
}

/// (#39) On a runner, stdout stays a single parseable JSON document while the
/// failure annotation still reaches GitHub.
///
/// `run` prints its JSON report to stdout unconditionally, and
/// `GITHUB_ACTIONS=true` auto-enables annotations, so this is the exact
/// combination that used to append `::error ...` after the JSON and make
/// `pitty run | jq` fail on red runs only. What is guaranteed: stdout parses
/// whole as JSON and carries no `::` workflow command, and the `::error`
/// annotation appears on stderr — which the runner scans for workflow commands
/// just as it does stdout. File-only scenario, so no PTY is needed.
#[test]
fn annotations_go_to_stderr_so_run_stdout_stays_parseable_json() {
    let dir = tempfile::tempdir().unwrap();
    // The absent file makes the single assertion fail, which is the only case
    // that emits an annotation at all — a green run could not detect the bug.
    let scenario = dir.path().join("s.yaml");
    std::fs::write(
        &scenario,
        "name: gha-json-purity\nsteps:\n  - expect_file_exists:\n      path: absent.txt\n",
    )
    .unwrap();

    let (stdout, stderr) = run_pitty_captured(
        &["run", scenario.to_str().unwrap()],
        &dir.path().join("summary.md"),
    );

    let parsed: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("stdout must be one JSON value, got {stdout:?}: {e}"));
    assert_eq!(
        parsed["status"], "failed",
        "the report must record the failure"
    );
    assert!(
        !stdout.contains("::"),
        "no workflow command may appear on stdout: {stdout:?}"
    );
    assert!(
        stderr.contains("::error title=pitty%3A gha-json-purity::"),
        "the failure annotation must still be emitted, on stderr: {stderr:?}"
    );
}

/// (#39) The same stream split must hold for `matrix --json`, the other command
/// whose stdout is a machine-readable document. What is guaranteed: with
/// annotations auto-enabled, `matrix --json` stdout parses whole as JSON and the
/// per-cell `::error` is on stderr instead. A file-only matrix keeps this
/// PTY-free.
#[test]
fn matrix_json_stdout_stays_parseable_with_annotations_enabled() {
    let dir = tempfile::tempdir().unwrap();
    let scenario = dir.path().join("m.yaml");
    // The axis must appear in an expansion target for `matrix_axes` to accept
    // it, and `expect_file_exists.path` is not one; referencing it from
    // scenario-level `env` satisfies that without needing a `spawn` (and hence
    // without a PTY), while the assertion still fails on the absent file.
    std::fs::write(
        &scenario,
        "name: gha-matrix-json\nenv:\n  TARGET: ${target}\nmatrix:\n  \
         target: [absent.txt]\nsteps:\n  - expect_file_exists:\n      path: absent.txt\n",
    )
    .unwrap();

    let (stdout, stderr) = run_pitty_captured(
        &["matrix", scenario.to_str().unwrap(), "--json"],
        &dir.path().join("summary.md"),
    );

    let parsed: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("stdout must be one JSON value, got {stdout:?}: {e}"));
    assert_eq!(parsed["cells"].as_array().map(Vec::len), Some(1));
    assert!(
        !stdout.contains("::"),
        "no workflow command may appear on --json stdout: {stdout:?}"
    );
    assert!(
        stderr.contains("::error title=pitty matrix%3A"),
        "the per-cell annotation must still be emitted, on stderr: {stderr:?}"
    );
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

/// Run the pitty binary once against `args`, ignoring its verdict.
///
/// Used by the multi-process log tests below, where what matters is the state
/// `logs/` is left in after several *separate* invocations, not each exit code.
fn run_pitty_ignoring_verdict(args: &[&str]) {
    std::process::Command::new(env!("CARGO_BIN_EXE_pitty"))
        .args(args)
        .env_remove("GITHUB_ACTIONS")
        .output()
        .expect("pitty binary must launch");
}

/// The log file names under `dir/logs`, sorted.
fn log_file_names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir.join("logs"))
        .expect("logs/ must exist")
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        // Claim sidecars are an implementation detail of ownership, not logs.
        .filter(|n| !n.ends_with(".claim"))
        .collect();
    names.sort();
    names
}

/// Re-running one scenario must reuse its own log file, not accumulate a
/// numbered copy per invocation.
///
/// This has to run the real binary several times: each `pitty run` is a separate
/// process, so any in-process bookkeeping starts empty every time and cannot be
/// what makes the re-run reuse its file. An in-process unit test passes whether
/// or not that holds, which is exactly how this regressed.
#[test]
#[ignore = "requires a usable PTY"]
fn rerunning_a_scenario_reuses_its_log_across_processes() {
    let _pty = pty_lock();
    let dir = tempfile::tempdir().unwrap();
    let scenario = dir.path().join("s.yaml");
    std::fs::write(
        &scenario,
        r#"
name: s
steps:
  - spawn: "echo hello-from-s"
  - expect:
      contains: hello-from-s
      timeout: 10s
"#,
    )
    .unwrap();
    let path = scenario.to_str().unwrap();

    for _ in 0..3 {
        run_pitty_ignoring_verdict(&["run", path]);
    }

    assert_eq!(
        log_file_names(dir.path()),
        vec!["s.log"],
        "three runs of one scenario must share one log file"
    );
    // The surviving log must be the newest run's, not a stale first one left
    // behind while later runs went to suffixed names.
    let log = std::fs::read_to_string(dir.path().join("logs/s.log")).unwrap();
    assert!(log.contains("hello-from-s"), "log:\n{log}");
}

/// Two scenario files sharing a `name:` must keep separate logs even when the
/// directory is run repeatedly — the re-run reuses each file rather than either
/// overwriting the other or spawning a numbered copy per invocation.
#[test]
#[ignore = "requires a usable PTY"]
fn duplicate_names_keep_separate_logs_across_repeated_runs() {
    let _pty = pty_lock();
    let dir = tempfile::tempdir().unwrap();
    for (file, marker) in [("a.yaml", "AAA-from-file-a"), ("b.yaml", "BBB-from-file-b")] {
        std::fs::write(
            dir.path().join(file),
            format!(
                r#"
name: same-name
steps:
  - spawn: "echo {marker}"
  - expect:
      contains: {marker}
      timeout: 10s
"#
            ),
        )
        .unwrap();
    }
    let path = dir.path().to_str().unwrap();

    run_pitty_ignoring_verdict(&["run", path]);
    run_pitty_ignoring_verdict(&["run", path]);

    assert_eq!(
        log_file_names(dir.path()),
        vec!["a.same-name.log", "b.same-name.log"],
        "each file keeps one log, and a re-run reuses it"
    );
    let from_a = std::fs::read_to_string(dir.path().join("logs/a.same-name.log")).unwrap();
    let from_b = std::fs::read_to_string(dir.path().join("logs/b.same-name.log")).unwrap();
    assert!(from_a.contains("AAA-from-file-a"), "{from_a}");
    assert!(from_b.contains("BBB-from-file-b"), "{from_b}");
}

/// Re-running a matrix must keep exactly one log per cell across invocations.
#[test]
#[ignore = "requires a usable PTY"]
fn rerunning_a_matrix_keeps_one_log_per_cell_across_processes() {
    let _pty = pty_lock();
    let dir = tempfile::tempdir().unwrap();
    let scenario = dir.path().join("m.yaml");
    std::fs::write(
        &scenario,
        r#"
name: m
matrix:
  word: [alpha, bravo]
steps:
  - spawn: "echo ${word}"
  - expect:
      contains: "${word}"
      timeout: 10s
"#,
    )
    .unwrap();
    let path = scenario.to_str().unwrap();

    run_pitty_ignoring_verdict(&["matrix", path]);
    run_pitty_ignoring_verdict(&["matrix", path]);

    assert_eq!(
        log_file_names(dir.path()),
        vec!["m.word-alpha.log", "m.word-bravo.log"],
        "each cell keeps one log, and a re-run reuses it"
    );
}

/// A log written by an older pitty carries no ownership claim, so it must be
/// left alone rather than overwritten by a scenario that happens to share its
/// name — the claim is proof of ownership, and its absence is not.
#[test]
#[ignore = "requires a usable PTY"]
fn an_unclaimed_legacy_log_is_not_overwritten() {
    let _pty = pty_lock();
    let dir = tempfile::tempdir().unwrap();
    let scenario = dir.path().join("s.yaml");
    std::fs::write(
        &scenario,
        r#"
name: s
steps:
  - spawn: "echo hello-from-s"
  - expect:
      contains: hello-from-s
      timeout: 10s
"#,
    )
    .unwrap();
    std::fs::create_dir_all(dir.path().join("logs")).unwrap();
    std::fs::write(
        dir.path().join("logs/s.log"),
        "# scenario: s\n# status: Passed\nLEGACY-LOG-CONTENT\n",
    )
    .unwrap();

    run_pitty_ignoring_verdict(&["run", scenario.to_str().unwrap()]);

    let legacy = std::fs::read_to_string(dir.path().join("logs/s.log")).unwrap();
    assert!(
        legacy.contains("LEGACY-LOG-CONTENT"),
        "an unclaimed log must be preserved, not clobbered:\n{legacy}"
    );
    assert!(
        dir.path().join("logs/s.2.log").exists(),
        "the new run must write alongside it"
    );
}

/// `pitty run` and `pitty bench` on the same file must claim the same log.
///
/// They build their run options separately, so it is easy for one to stamp the
/// scenario file into the log identity and the other not to. That divergence is
/// invisible in a single command's output — it only shows up as two log files
/// for one scenario, where `logs/<scenario>.log` is whichever command ran last.
#[test]
#[ignore = "requires a usable PTY"]
fn run_and_bench_share_one_log_for_the_same_scenario() {
    let _pty = pty_lock();
    let dir = tempfile::tempdir().unwrap();
    let scenario = dir.path().join("s.yaml");
    std::fs::write(
        &scenario,
        r#"
name: s
steps:
  - spawn: "echo hello-from-s"
  - expect:
      contains: hello-from-s
      timeout: 10s
"#,
    )
    .unwrap();
    let path = scenario.to_str().unwrap();

    run_pitty_ignoring_verdict(&["run", path]);
    run_pitty_ignoring_verdict(&["bench", path, "--runs", "2"]);
    run_pitty_ignoring_verdict(&["run", path]);

    assert_eq!(
        log_file_names(dir.path()),
        vec!["s.log"],
        "run and bench must write the same scenario's log to one file"
    );
}

/// `a.yaml` and `a.yml` in one directory are two separate scenarios, so a
/// directory run must leave two logs even when both declare the same `name:`.
///
/// The stem alone conflates them, which is the #36 failure mode reached through
/// a different pair of filenames.
#[test]
#[ignore = "requires a usable PTY"]
fn yaml_and_yml_siblings_keep_separate_logs() {
    let _pty = pty_lock();
    let dir = tempfile::tempdir().unwrap();
    for (file, marker) in [("a.yaml", "AAA-yaml"), ("a.yml", "BBB-yml")] {
        std::fs::write(
            dir.path().join(file),
            format!(
                r#"
name: same
steps:
  - spawn: "echo {marker}"
  - expect:
      contains: {marker}
      timeout: 10s
"#
            ),
        )
        .unwrap();
    }

    run_pitty_ignoring_verdict(&["run", dir.path().to_str().unwrap()]);

    let names = log_file_names(dir.path());
    assert_eq!(
        names.len(),
        2,
        "a.yaml and a.yml must each keep a log, got {names:?}"
    );
    // The .yaml file keeps the plain name; the .yml one is the marked variant.
    let plain = std::fs::read_to_string(dir.path().join("logs/a.same.log"))
        .expect("the .yaml scenario keeps the unmarked log name");
    assert!(
        plain.contains("AAA-yaml"),
        "the .yaml run's own output must survive:\n{plain}"
    );
}

/// A matrix axis value long enough to push the log name past the filesystem's
/// limit must still produce a log, not silently lose it.
#[test]
#[ignore = "requires a usable PTY"]
fn a_long_axis_value_still_writes_its_cell_log() {
    let _pty = pty_lock();
    let dir = tempfile::tempdir().unwrap();
    let long_value = "x".repeat(250);
    std::fs::write(
        dir.path().join("m.yaml"),
        format!(
            r#"
name: m
matrix:
  word: ["{long_value}"]
steps:
  - spawn: "echo ${{word}}"
  - expect:
      contains: xxx
      timeout: 10s
"#
        ),
    )
    .unwrap();

    run_pitty_ignoring_verdict(&["matrix", dir.path().join("m.yaml").to_str().unwrap()]);

    let names = log_file_names(dir.path());
    assert_eq!(
        names.len(),
        1,
        "the cell must leave exactly one log, got {names:?}"
    );
    assert!(
        names[0].len() <= 255,
        "the log name must fit a path component, got {} bytes",
        names[0].len()
    );
}

/// A log that cannot be written must be reported on stderr, leave stdout's JSON
/// report intact, and not change the scenario's verdict.
///
/// The diagnostics sink failing is an environment problem, not a failure of the
/// program under test — but it must not be invisible either.
#[test]
#[ignore = "requires a usable PTY"]
fn an_unwritable_log_warns_without_failing_the_run() {
    let _pty = pty_lock();
    let dir = tempfile::tempdir().unwrap();
    let scenario = dir.path().join("s.yaml");
    std::fs::write(
        &scenario,
        r#"
name: s
steps:
  - spawn: "echo hello"
  - expect:
      contains: hello
      timeout: 10s
"#,
    )
    .unwrap();
    // Occupy `logs` with a regular file so the log directory cannot be created.
    // Why not a read-only directory: a test run as root would still be able to
    // write into one, making the test silently vacuous there.
    std::fs::write(dir.path().join("logs"), b"not a directory").unwrap();

    let out = std::process::Command::new(env!("CARGO_BIN_EXE_pitty"))
        .args(["run", scenario.to_str().unwrap()])
        .env_remove("GITHUB_ACTIONS")
        .output()
        .expect("pitty binary must launch");

    assert_eq!(
        out.status.code(),
        Some(0),
        "a diagnostics-sink failure must not turn a passing scenario red"
    );

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("\"status\": \"passed\""),
        "stdout must stay a clean JSON report:\n{stdout}"
    );
    assert!(
        !stdout.contains("warning"),
        "the warning must not pollute stdout (--json consumers parse it):\n{stdout}"
    );

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("could not write the log"),
        "the failure must be surfaced on stderr, not swallowed:\n{stderr}"
    );
}

/// A secret that collides with the claim line must not cost a re-run its log.
///
/// The claim is how a later *process* recognizes its own log. Masking is blind
/// substring replacement, so a secret of `id` rewrote the literal key `# id:` to
/// `# ***:` and every invocation then created a new numbered file. This must run
/// the real binary several times: an in-process test shares the path memo and
/// passes whether or not the on-disk claim survived, which is exactly how this
/// defect reached production.
#[test]
#[ignore = "requires a usable PTY"]
fn a_secret_colliding_with_the_claim_key_does_not_accumulate_logs() {
    let _pty = pty_lock();
    let dir = tempfile::tempdir().unwrap();
    let scenario = dir.path().join("s.yaml");
    std::fs::write(
        &scenario,
        r#"
name: s
variables:
  tok:
    value: id
    secret: true
steps:
  - spawn: "echo hello"
  - expect:
      contains: hello
      timeout: 10s
"#,
    )
    .unwrap();
    let path = scenario.to_str().unwrap();

    for _ in 0..3 {
        run_pitty_ignoring_verdict(&["run", path]);
    }

    assert_eq!(
        log_file_names(dir.path()),
        vec!["s.log"],
        "a secret matching the claim key must not break cross-process reuse"
    );
    // The key itself is masked on the way out — a secret of `id` rewrites
    // `# id: ` to `# ***: ` — and the reader masks the key it searches for, so
    // both sides agree. What matters is that a claim line is present and the
    // reuse above held; the literal spelling of the key is not the contract.
    // Ownership lives in an unmasked sidecar beside the log, not in the body:
    // masking would corrupt any tag written into the body.
    assert!(
        dir.path().join("logs/.s.log.claim").exists(),
        "the log's ownership sidecar must exist"
    );
}

/// The same hazard reached through the digest rather than the key: a secret
/// equal to a hex substring that actually appears in the log's own claim.
#[test]
#[ignore = "requires a usable PTY"]
fn a_secret_colliding_with_the_claim_digest_does_not_accumulate_logs() {
    let _pty = pty_lock();
    let dir = tempfile::tempdir().unwrap();
    let scenario = dir.path().join("s.yaml");
    let body = |secret: &str| {
        format!(
            r#"
name: s
variables:
  tok:
    value: "{secret}"
    secret: true
steps:
  - spawn: "echo hello"
  - expect:
      contains: hello
      timeout: 10s
"#
        )
    };

    // First run with a harmless secret, purely to learn this identity's digest.
    std::fs::write(&scenario, body("zzzz")).unwrap();
    run_pitty_ignoring_verdict(&["run", scenario.to_str().unwrap()]);
    let digest = std::fs::read_to_string(dir.path().join("logs/.s.log.claim"))
        .expect("the log must carry a claim sidecar")
        .trim()
        .to_string();

    // Now make a real hex substring of that digest the secret, and start clean.
    std::fs::remove_dir_all(dir.path().join("logs")).unwrap();
    std::fs::write(&scenario, body(&digest[..4])).unwrap();
    for _ in 0..3 {
        run_pitty_ignoring_verdict(&["run", scenario.to_str().unwrap()]);
    }

    assert_eq!(
        log_file_names(dir.path()),
        vec!["s.log"],
        "a secret matching part of the digest must not break cross-process reuse"
    );
}

/// A log must never be group/other-readable, including one an earlier pitty
/// left at 0644 that a later run reuses.
#[cfg(unix)]
#[test]
#[ignore = "requires a usable PTY"]
fn a_reused_log_is_restored_to_0600() {
    use std::os::unix::fs::PermissionsExt;

    let _pty = pty_lock();
    let dir = tempfile::tempdir().unwrap();
    let scenario = dir.path().join("s.yaml");
    std::fs::write(
        &scenario,
        r#"
name: s
steps:
  - spawn: "echo hello"
  - expect:
      contains: hello
      timeout: 10s
"#,
    )
    .unwrap();
    let path = scenario.to_str().unwrap();

    run_pitty_ignoring_verdict(&["run", path]);
    let log = dir.path().join("logs/s.log");
    // Simulate the log a pre-fix pitty left behind.
    std::fs::set_permissions(&log, std::fs::Permissions::from_mode(0o644)).unwrap();

    run_pitty_ignoring_verdict(&["run", path]);

    let mode = std::fs::metadata(&log).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "a reused log must be repaired to 0600");
}

/// A symlink planted at a log's name must never be written through, even when
/// its target does not exist yet.
///
/// `Path::exists()` follows links, so a dangling link read as "free" and the
/// writer created the link's target — putting the full terminal output wherever
/// the link pointed, outside the workspace entirely.
#[cfg(unix)]
#[test]
#[ignore = "requires a usable PTY"]
fn a_planted_dangling_symlink_cannot_redirect_a_log() {
    let _pty = pty_lock();
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    let victim_dir = dir.path().join("victim");
    std::fs::create_dir_all(ws.join("logs")).unwrap();
    std::fs::create_dir_all(&victim_dir).unwrap();
    std::fs::write(
        ws.join("s.yaml"),
        r#"
name: s
steps:
  - spawn: "echo sensitive-output"
  - expect:
      contains: sensitive-output
      timeout: 10s
"#,
    )
    .unwrap();

    let victim = victim_dir.join("pwned.txt");
    std::os::unix::fs::symlink("../../victim/pwned.txt", ws.join("logs/s.log")).unwrap();

    run_pitty_ignoring_verdict(&["run", ws.join("s.yaml").to_str().unwrap()]);

    assert!(
        !victim.exists(),
        "the log must not be written through a dangling symlink"
    );
    assert_eq!(
        std::fs::read_dir(&victim_dir).unwrap().count(),
        0,
        "nothing may be created outside the workspace"
    );
}

/// A `logs/` symlink must be refused even when pitty is invoked with a bare
/// relative filename from inside the scenario's own directory.
///
/// This is the shape that actually escaped: `Path::new("s.yaml").parent()` is
/// `Some("")`, so `base_dir` arrived empty, the pre-spawn anchor could not be
/// opened, and the writer silently fell back to a path-based write that followed
/// the link. The sibling test below passes an *absolute* path, which captures an
/// anchor successfully and therefore never reached that branch — which is why it
/// certified a refusal that was not happening.
#[cfg(unix)]
#[test]
#[ignore = "requires a usable PTY"]
fn a_symlinked_logs_directory_is_refused_for_a_bare_relative_path() {
    let _pty = pty_lock();
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    let elsewhere = dir.path().join("elsewhere");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::create_dir_all(&elsewhere).unwrap();
    std::fs::write(
        ws.join("s.yaml"),
        r#"
name: s
steps:
  - spawn: "echo sensitive-output"
  - expect:
      contains: sensitive-output
      timeout: 10s
"#,
    )
    .unwrap();
    std::os::unix::fs::symlink("../elsewhere", ws.join("logs")).unwrap();

    // Bare filename, resolved against the child's cwd — the CI-shaped invocation.
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_pitty"))
        .args(["run", "s.yaml"])
        .current_dir(&ws)
        .env_remove("GITHUB_ACTIONS")
        .output()
        .expect("pitty binary must launch");

    assert_eq!(
        std::fs::read_dir(&elsewhere).unwrap().count(),
        0,
        "the log must not be written through the linked directory"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("symlink"),
        "the refusal must be reported, not silent:\n{stderr}"
    );
}

/// A bare relative path with no symlink must still log normally — the empty-base
/// normalization must not cost the ordinary invocation its log.
#[test]
#[ignore = "requires a usable PTY"]
fn a_bare_relative_path_still_writes_its_log() {
    let _pty = pty_lock();
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("s.yaml"),
        r#"
name: s
steps:
  - spawn: "echo hello"
  - expect:
      contains: hello
      timeout: 10s
"#,
    )
    .unwrap();

    let out = std::process::Command::new(env!("CARGO_BIN_EXE_pitty"))
        .args(["run", "s.yaml"])
        .current_dir(dir.path())
        .env_remove("GITHUB_ACTIONS")
        .output()
        .expect("pitty binary must launch");

    assert!(
        String::from_utf8_lossy(&out.stderr).is_empty(),
        "an ordinary run must not warn: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let log = std::fs::read_to_string(dir.path().join("logs/s.log"))
        .expect("a bare relative invocation must still leave a log");
    assert!(log.contains("hello"), "log:\n{log}");
}

/// A `logs/` that is itself a symlink must be refused, not followed.
///
/// NOTE: this passes an *absolute* path, so the anchor is captured and only the
/// post-capture refusal is exercised. The bare-relative test above covers the
/// branch this one structurally cannot reach.
#[cfg(unix)]
#[test]
#[ignore = "requires a usable PTY"]
fn a_symlinked_logs_directory_is_refused_end_to_end() {
    let _pty = pty_lock();
    let dir = tempfile::tempdir().unwrap();
    let ws = dir.path().join("ws");
    let elsewhere = dir.path().join("elsewhere");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::create_dir_all(&elsewhere).unwrap();
    std::fs::write(
        ws.join("s.yaml"),
        r#"
name: s
steps:
  - spawn: "echo sensitive-output"
  - expect:
      contains: sensitive-output
      timeout: 10s
"#,
    )
    .unwrap();
    std::os::unix::fs::symlink(&elsewhere, ws.join("logs")).unwrap();

    let out = std::process::Command::new(env!("CARGO_BIN_EXE_pitty"))
        .args(["run", ws.join("s.yaml").to_str().unwrap()])
        .env_remove("GITHUB_ACTIONS")
        .output()
        .expect("pitty binary must launch");

    assert_eq!(
        std::fs::read_dir(&elsewhere).unwrap().count(),
        0,
        "nothing may be written through the linked directory"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("symlink"), "stderr:\n{stderr}");
}

/// A secret equal to the log's own claim digest must not reach disk, and the
/// re-run must still find its own log.
#[test]
#[ignore = "requires a usable PTY"]
fn a_secret_equal_to_the_digest_neither_leaks_nor_breaks_reuse() {
    let _pty = pty_lock();
    let dir = tempfile::tempdir().unwrap();
    let scenario = dir.path().join("s.yaml");
    let body = |vars: &str| {
        format!(
            r#"
name: s
{vars}
steps:
  - spawn: "echo hello"
  - expect:
      contains: hello
      timeout: 10s
"#
        )
    };

    // Learn the tag this identity writes with no secrets registered.
    std::fs::write(&scenario, body("")).unwrap();
    run_pitty_ignoring_verdict(&["run", scenario.to_str().unwrap()]);
    let digest = std::fs::read_to_string(dir.path().join("logs/.s.log.claim"))
        .expect("the log must carry a claim sidecar")
        .trim()
        .to_string();

    // Register exactly that tag as a secret and start clean.
    std::fs::remove_dir_all(dir.path().join("logs")).unwrap();
    let vars = format!("variables:\n  tok:\n    value: \"{digest}\"\n    secret: true");
    std::fs::write(&scenario, body(&vars)).unwrap();
    for _ in 0..3 {
        run_pitty_ignoring_verdict(&["run", scenario.to_str().unwrap()]);
    }

    assert_eq!(
        log_file_names(dir.path()),
        vec!["s.log"],
        "reuse must still hold when the secret equals the old digest"
    );
    let log = std::fs::read_to_string(dir.path().join("logs/s.log")).unwrap();
    assert!(
        !log.contains(&digest),
        "the registered secret must not appear in the log:\n{log}"
    );
}

/// A secret used as a matrix axis value must not appear in any log filename,
/// while cells differing only inside a secret still get separate logs.
#[test]
#[ignore = "requires a usable PTY"]
fn a_secret_matrix_axis_value_stays_out_of_log_filenames() {
    let _pty = pty_lock();
    let dir = tempfile::tempdir().unwrap();
    let scenario = dir.path().join("m.yaml");
    std::fs::write(
        &scenario,
        r#"
name: m
variables:
  a:
    value: alpha
    secret: true
  b:
    value: bravo
    secret: true
matrix:
  word: [alpha, bravo]
steps:
  - spawn: "echo ${word}"
  - expect:
      contains: a
      timeout: 10s
"#,
    )
    .unwrap();

    run_pitty_ignoring_verdict(&["matrix", scenario.to_str().unwrap()]);

    let names = log_file_names(dir.path());
    assert_eq!(
        names.len(),
        2,
        "cells differing only inside a secret must not collide: {names:?}"
    );
    for name in &names {
        assert!(
            !name.contains("alpha") && !name.contains("bravo"),
            "a secret must not reach a log filename: {name}"
        );
    }
}

/// A run that ends in a hard fault must be logged as errored, never as passed.
///
/// The log used to be assembled before teardown, so a scenario whose spawn could
/// not even be parsed produced `# status: Passed` — a diagnostic that actively
/// misleads whoever opens it during an incident.
#[test]
#[ignore = "requires a usable PTY"]
fn a_failed_run_is_not_logged_as_passed() {
    let _pty = pty_lock();
    let dir = tempfile::tempdir().unwrap();
    let scenario = dir.path().join("q.yaml");
    // An unterminated quote under `split: posix`: tokenization fails, so this is
    // a process fault. The opt-in is required — under the default whitespace
    // rule the same line tokenizes fine and the run would not fault at all.
    std::fs::write(
        &scenario,
        "name: q\nsteps:\n  - spawn:\n      command: \"echo 'unterminated\"\n      split: posix\n",
    )
    .unwrap();

    let out = std::process::Command::new(env!("CARGO_BIN_EXE_pitty"))
        .args(["run", scenario.to_str().unwrap()])
        .env_remove("GITHUB_ACTIONS")
        .output()
        .expect("pitty binary must launch");

    assert_eq!(
        out.status.code(),
        Some(3),
        "a spawn fault is the process class"
    );

    let log = std::fs::read_to_string(dir.path().join("logs/q.log"))
        .expect("a faulting run must still leave a log");
    assert!(
        !log.contains("# status: Passed"),
        "a failed run must not be logged as passed:\n{log}"
    );
    assert!(log.contains("# status: Errored"), "log:\n{log}");
    assert!(
        log.contains("# error:"),
        "the fault must be recorded:\n{log}"
    );
}

/// A one-character secret that collides with the ownership tag must not cause
/// logs to accumulate across processes.
#[test]
#[ignore = "requires a usable PTY"]
fn a_one_character_secret_does_not_reintroduce_log_accumulation() {
    let _pty = pty_lock();
    let dir = tempfile::tempdir().unwrap();
    let scenario = dir.path().join("claimed.yaml");
    std::fs::write(
        &scenario,
        r#"
name: claimed
variables:
  t:
    value: "2"
    secret: true
steps:
  - spawn: "echo hello"
  - expect:
      contains: hello
      timeout: 10s
"#,
    )
    .unwrap();
    let path = scenario.to_str().unwrap();

    for _ in 0..3 {
        run_pitty_ignoring_verdict(&["run", path]);
    }

    assert_eq!(
        log_file_names(dir.path()),
        vec!["claimed.log"],
        "a one-character secret must not break cross-process reuse"
    );
}

/// A failed second `spawn` must not make the first session appear twice.
///
/// The retirement path saved the old session's buffer and, because the session
/// stayed in `state` when the new spawn failed, the final teardown saved the very
/// same buffer again — so the log showed one session twice.
#[test]
#[ignore = "requires a usable PTY"]
fn a_failed_respawn_does_not_duplicate_the_first_session() {
    let _pty = pty_lock();
    let dir = tempfile::tempdir().unwrap();
    let scenario = dir.path().join("r.yaml");
    std::fs::write(
        &scenario,
        r#"
name: r
steps:
  - spawn: bash
  - send: echo unique-marker-42
  - expect:
      contains: unique-marker-42
      timeout: 10s
  - spawn: "   "
"#,
    )
    .unwrap();

    run_pitty_ignoring_verdict(&["run", scenario.to_str().unwrap()]);

    let log = std::fs::read_to_string(dir.path().join("logs/r.log"))
        .expect("a failed respawn must still leave a log");
    let sessions = log.matches("--- session ").count();
    assert!(
        sessions <= 1,
        "the retired session must appear exactly once, found {sessions} session banners:\n{log}"
    );
    // The marker is echoed by the shell and printed by it, so it legitimately
    // appears more than once *within* one session; what must not happen is the
    // whole buffer being repeated under a second banner.
    assert!(log.contains("unique-marker-42"), "log:\n{log}");
}

/// `duration_ms` measures the scenario's own execution, never its teardown.
///
/// Moving teardown before the log write (so the log records the true final
/// status) silently folded cleanup into this field, and `BenchReport` derives
/// its statistics straight from it — so every recorded threshold would shift.
/// COMPATIBILITY.md treats a change in a report field's meaning as a major
/// change.
///
/// The assertion is on a scenario whose child has *already exited* before the
/// run ends. Its own work is a couple of milliseconds, so the only thing that can
/// inflate the number is teardown: measured at ~65ms with teardown counted and
/// ~8ms without. The bound sits well above the honest value and far below the
/// regression, so it survives a loaded machine without going blind to the bug.
#[test]
#[ignore = "requires a usable PTY"]
fn duration_ms_excludes_session_teardown() {
    let _pty = pty_lock();
    let dir = tempfile::tempdir().unwrap();

    // The child has exited by the time the run ends, so the scenario's own work
    // is trivially small — but pitty still tears the session down afterwards.
    let scenario = dir.path().join("exited.yaml");
    std::fs::write(
        &scenario,
        r#"
name: exited
steps:
  - spawn: "echo up-42"
  - expect:
      contains: up-42
      timeout: 10s
"#,
    )
    .unwrap();

    // Best of several runs: the floor is what carries the signal, and a single
    // sample on a busy machine can be arbitrarily high for unrelated reasons.
    let best = (0..5)
        .map(|_| {
            let out = std::process::Command::new(env!("CARGO_BIN_EXE_pitty"))
                .args(["run", scenario.to_str().unwrap()])
                .env_remove("GITHUB_ACTIONS")
                .output()
                .expect("pitty binary must launch");
            let stdout = String::from_utf8_lossy(&out.stdout);
            let report: serde_json::Value =
                serde_json::from_str(&stdout).expect("stdout must be a JSON report");
            report["duration_ms"].as_u64().expect("duration_ms")
        })
        .min()
        .expect("at least one run");

    assert!(
        best < 40,
        "duration_ms must exclude teardown (~8ms honest, ~65ms with teardown counted), got {best}ms"
    );
}
