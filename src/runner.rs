//! The scenario runner: prepare workspace, execute steps (hard errors abort
//! immediately, assertion failures are collected), build a report.
//!
//! The runner owns the lifecycle of a single scenario: it resolves the
//! workspace, runs each step in order, and stops at the first hard error
//! (process/scenario fault) while recording assertion failures as report rows.
//! Assertion failures do not abort the run — they are collected so the report
//! shows every checked step — but they do drive the final status to `Failed`.

use std::path::Path;
use std::time::{Duration, Instant};

use crate::assert::file::FileSnapshots;
use crate::assert::semantic::{LexicalBackend, SemanticBackend};
use crate::assert::{self, AssertionResult};
use crate::config::{Scenario, Source, Step};
use crate::error::PittyError;
use crate::pty::reader::OutputBufferHandle;
use crate::pty::{Matcher, PtySession, Teardown};
use crate::report::{LogIdentity, Report, Status};
use crate::workspace::{mask_secrets, Workspace};

/// The global default timeout applied to `expect`/`expect_regex` when a step
/// omits its own.
const DEFAULT_EXPECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Options that cut across a whole scenario run, threaded from the CLI.
///
/// Introduced as a struct (rather than another positional argument) so future
/// run-wide options (e.g. a `--snapshot-dir`) can be added without churning
/// every `run_scenario` call site.
pub struct RunOptions {
    /// Record/refresh snapshots instead of failing on absent/mismatched ones.
    ///
    /// Why not guarded against concurrent updates: scenarios still run strictly
    /// sequentially (see `cmd_run`), so two `--update` snapshot writes never
    /// race. If a future release parallelizes scenarios, snapshot writes
    /// to a shared file would need coordination (per-path locking or partitioned
    /// output dirs); this note marks the spot to revisit before that change.
    pub update: bool,
    /// The backend `expect_semantic` scores with. Defaults to [`LexicalBackend`].
    ///
    /// Held as a trait object so a future `--features semantic-embeddings`
    /// backend can be injected here (in `cli.rs`, when assembling `RunOptions`)
    /// without the runner referencing any concrete backend. The runner only
    /// calls through the trait, keeping backend selection out of step execution.
    /// `Send + Sync` so `RunOptions` stays shareable across threads if a future
    /// caller parallelizes scenarios.
    pub semantic_backend: Box<dyn SemanticBackend + Send + Sync>,
    /// Extra identity the log file name is built from, beyond the scenario name.
    ///
    /// Carried here rather than added as a `run_scenario` parameter because the
    /// two callers that can supply it (the CLI, which knows the scenario file,
    /// and `run_matrix`, which knows the cell) already thread a `RunOptions`,
    /// while `run_bench` and the library API have nothing to add — this is the
    /// extension point this struct exists for. `None` means "name only", the
    /// shape a direct `run_scenario` call gets.
    pub log_identity: Option<LogIdentity>,
}

impl Default for RunOptions {
    fn default() -> Self {
        RunOptions {
            update: false,
            semantic_backend: Box::new(LexicalBackend),
            log_identity: None,
        }
    }
}

impl std::fmt::Debug for RunOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The backend trait object is not Debug; report only the observable
        // option so a derived Debug requirement elsewhere stays satisfiable.
        f.debug_struct("RunOptions")
            .field("update", &self.update)
            .finish_non_exhaustive()
    }
}

/// Run a single scenario and produce its report.
///
/// `base_dir` is the directory the scenario file lives in; relative paths in
/// the scenario resolve against it (or against a temp dir when configured).
///
/// Returns:
/// - `Ok(report)` with `status` Passed/Failed for normal completion (including
///   assertion failures, which are not errors but failures).
/// - `Err(PittyError)` for process/scenario faults that prevented the run
///   from completing; the caller maps these to exit codes via
///   `PittyError::exit_code` (the report on this path is logged but not
///   returned, so no status is emitted for a hard fault).
pub fn run_scenario(
    scenario: &Scenario,
    base_dir: &Path,
    options: &RunOptions,
) -> Result<Report, PittyError> {
    run_scenario_logged(scenario, base_dir, options, None)
}

/// [`run_scenario`], with the log identity supplied directly by the caller.
///
/// `identity` overrides `options.log_identity` for this one run. It exists for
/// `run_matrix`, which runs many cells through one `RunOptions` and must vary
/// only the log path between them: `RunOptions` owns a non-`Clone` backend trait
/// object, so a per-cell clone of it is not available to carry the difference.
pub(crate) fn run_scenario_logged(
    scenario: &Scenario,
    base_dir: &Path,
    options: &RunOptions,
    identity: Option<LogIdentity>,
) -> Result<Report, PittyError> {
    let start = Instant::now();
    // Capture the log anchor *before* any child is spawned. The log is written
    // at the end of the run, by which time the scenario's own process may have
    // planted symlinks under `base_dir`; a descriptor taken now names the
    // directory object that existed beforehand, which is the entire basis of
    // `safepath`'s anti-swap property. Taken before `Workspace::prepare` so it
    // precedes every side effect of the run.
    //
    // Anchored on `base_dir` (the scenario file's directory), deliberately *not*
    // on the workspace cwd: under `workspace.temp: true` those are different
    // trees, and `logs/` belongs to the scenario file's directory.
    let log_anchor = crate::report::LogAnchor::capture(base_dir);
    let workspace = Workspace::prepare(scenario, base_dir)?;
    let mut state = RunState {
        session: None,
        snapshots: FileSnapshots::new(),
        assertions: Vec::new(),
        retired_output: Vec::new(),
    };

    let mut run_error: Option<PittyError> = None;
    for step in &scenario.steps {
        if matches!(step, Step::Spawn(_)) {
            // A second `spawn` retires the previous session through the same
            // classified teardown as the end of the run. Letting the later
            // assignment drop it would route it through `Drop`, which has to
            // swallow a live-tree error — the very thing the verdict is
            // supposed to reflect. Why in place rather than `take()`n out: on
            // a teardown or spawn failure the retired session must stay in
            // `state.session` so the hard-fault path below still writes its
            // output and the partial assertion rows to the log (a second
            // `shutdown` on it is a no-op).
            if let Some(mut previous) = state.session.take() {
                let teardown = classify_teardown(previous.shutdown());
                // Snapshot unconditionally, and only once. Taken *after*
                // `shutdown()` so the child's final bytes are in the buffer.
                //
                // Why the session is `take`n rather than borrowed: it used to
                // stay in `state.session` so a failing spawn below could still
                // log it — but then the final teardown snapshotted the very same
                // buffer a second time and the log showed one session twice. It
                // is moved out here, its output preserved in `retired_output`
                // either way, so exactly one copy reaches the log whether the
                // next spawn succeeds or fails.
                state
                    .retired_output
                    .push(previous.output().snapshot_string());
                if let Err(e) = teardown {
                    run_error = Some(e);
                    break;
                }
            }
            // Capture file baselines at spawn time so `expect_file_changed`
            // measures change from the moment *this* process starts. The store
            // is rebuilt rather than topped up: a baseline kept from an earlier
            // spawn would let that child's writes satisfy an assertion about
            // this one. Priming happens after the previous session is torn
            // down so its last writes are already in the baseline.
            state.snapshots = FileSnapshots::new();
            prime_snapshots(scenario, &workspace, &mut state.snapshots);
        }
        match execute_step(step, &workspace, &mut state, options) {
            Ok(()) => {}
            Err(e) => {
                // A hard error (process/scenario) stops the run fail-fast. We
                // record it and break so the report reflects progress so far.
                run_error = Some(e);
                break;
            }
        }
    }

    // Stop the clock before teardown begins.
    //
    // `duration_ms` is a published report field, and in 1.2.2 it measured the
    // scenario's own execution — the clock was read before the session was torn
    // down. Moving teardown earlier (so the log records the true final status)
    // silently folded cleanup into that number: a scenario leaving `bash` alive
    // went from ~3ms to ~64ms, and a stalled ConPTY teardown can add seconds.
    // `BenchReport` derives its statistics straight from this, so every recorded
    // threshold and historical comparison would shift.
    //
    // COMPATIBILITY.md treats a change in a report field's meaning as a major
    // change, so the value is captured here and the ordering below is unaffected:
    // the log still sees the post-teardown status and the drained output.
    let scenario_duration_ms = start.elapsed().as_millis();

    // Tear down the session BEFORE assembling the log, so the log records the
    // run's true final state.
    //
    // Why the order matters: `shutdown()` drains the child's remaining output,
    // and teardown can itself fail (a leaked process tree is a process error
    // even when every assertion passed). Logging first meant a run that ended in
    // a hard fault was written as `# status: Passed`, and a chatty child's tail
    // never reached the file — a diagnostic that misleads whoever reads it during
    // an incident is worse than no diagnostic at all.
    //
    // The session is taken out of `state` here, so `collect_output` below reads
    // the drained buffer from `retired_output` rather than a live session; that
    // is why the shutdown pushes its snapshot there.
    if let Some(mut session) = state.session.take() {
        let teardown = classify_teardown(session.shutdown());
        // Snapshot after the drain, so the final bytes are in the log.
        state
            .retired_output
            .push(session.output().snapshot_string());
        // A process failure outranks whatever the run already recorded (the
        // established severity order), so the earlier error is folded into
        // the message rather than the other way round.
        if let Err(teardown) = teardown {
            run_error = Some(match run_error.take() {
                Some(earlier) => {
                    PittyError::Process(format!("{}; earlier error: {earlier}", teardown.message()))
                }
                None => teardown,
            });
        }
    }

    // Invariant: a `Status` describes only the pass/fail of a *completed* run.
    // A hard fault (process/scenario) is reported by returning `Err` below, and
    // its exit-code class lives in `PittyError::exit_code` — the single source
    // of truth for fault classes. Why not also stamp the report when `run_error`
    // is set: that report is never observed on the `Err` path (the caller takes
    // the error branch), and a separate "error" status would duplicate, and could
    // contradict, the error's own exit code. The report is still assembled and
    // logged below on a hard fault so the partial assertions land in the on-disk
    // log; only the return value carries the fault.
    let status = if state.assertions.iter().all(|a| a.passed) {
        Status::Passed
    } else {
        Status::Failed
    };

    // Snapshot every session's output before `state.assertions` is moved into
    // the report, so the log body is assembled while `state` is still whole.
    let output = collect_output(&state);

    let mut report = Report {
        scenario: scenario.name.clone(),
        status,
        duration_ms: scenario_duration_ms,
        assertions: state.assertions,
    };
    // Mask secrets in the report before it leaves the runner. Assertion
    // messages can embed a timeout/EOF tail of raw PTY output, which may carry a
    // secret value; the CLI prints `report.to_json()` straight to stdout. Why
    // not rely on the per-scenario log masking: that only covers the on-disk log
    // (`write_log`), not the stdout JSON, so a secret would still surface there.
    mask_report(&mut report, workspace.secrets());

    // Write the per-scenario log on a best-effort basis; a logging failure
    // must not change the run's verdict.
    //
    // Written unconditionally, not only when a session exists: a spawn-less
    // scenario still produces assertion rows and can still fail, and its report
    // is exactly as worth keeping on disk as a spawned run's. The output section
    // is then the empty-run marker rather than terminal text.
    let identity = identity
        .or_else(|| options.log_identity.clone())
        .unwrap_or_else(|| LogIdentity::new(&scenario.name))
        // The scenario name always comes from the scenario actually run. A matrix
        // cell reuses the base identity supplied by the CLI, and a stale name in
        // it would label every cell's log with the wrong scenario.
        .with_name(&scenario.name);
    // A log-write failure warns on stderr but does not change the verdict.
    //
    // Why not fail the run: the log is a diagnostics *sink*, not an assertion.
    // Turning a full disk or a read-only `logs/` into a red test would report a
    // failure the scenario under test did not have, and in CI that misdirects
    // the investigation to the program under test instead of the runner's
    // environment. The verdict stays a statement about the scenario.
    //
    // Why not stay silent either (the previous `let _ =`): that lost the entire
    // class — permissions, full disk, a name too long for the filesystem — so a
    // developer opening `logs/` after a failure found nothing there and no
    // explanation for it. A warning is the honest middle: the run's verdict is
    // untouched, but the missing artifact is accounted for.
    //
    // stderr specifically, never stdout: `--json` consumers parse stdout, and a
    // warning mixed into it would corrupt the report. This matches where the
    // GitHub annotations write.
    // The hard fault, if any, is known by now: teardown ran above. Passing it
    // in is what lets the log say `Errored` instead of claiming a failed run
    // passed.
    let fault = run_error.as_ref().map(|e| e.to_string());
    if let Err(e) = crate::report::write_log(
        base_dir,
        &log_anchor,
        &identity,
        &output,
        &report,
        fault.as_deref(),
        workspace.secrets(),
    ) {
        // Mask the warning like every other diagnostic that leaves the runner:
        // the scenario name is author-supplied and the error embeds the log path
        // built from it, so a secret used in a name would otherwise surface here
        // after being carefully masked everywhere else.
        let secrets = workspace.secrets();
        let warning = format!(
            "warning: could not write the log for scenario '{}': {e}",
            scenario.name
        );
        eprintln!("{}", mask_secrets(&warning, secrets));
    }

    match run_error {
        Some(e) => Err(mask_error(e, workspace.secrets())),
        None => Ok(report),
    }
}

/// Mutable state threaded through step execution.
struct RunState {
    /// The active PTY session, present after a `spawn` step.
    session: Option<PtySession>,
    /// File baselines for `expect_file_changed`.
    snapshots: FileSnapshots,
    /// Accumulated assertion rows.
    assertions: Vec<AssertionResult>,
    /// Output buffers of the sessions already retired by a later `spawn`.
    ///
    /// Each `spawn` builds a fresh `PtySession` with its own buffer and drops the
    /// previous one, so the retired buffer is the only record of what that
    /// process printed. It is snapshotted at retirement into this vector and
    /// concatenated with the live session's buffer when the log is written, so a
    /// multi-spawn log carries every session's output rather than just the last.
    retired_output: Vec<String>,
}

/// Execute one step, mutating state and recording assertions.
///
/// Returns `Err` only for hard faults (no session when one is required, spawn
/// failure, regex compile failure, etc.). Assertion outcomes are pushed to
/// `state.assertions` and never returned as errors.
fn execute_step(
    step: &Step,
    workspace: &Workspace,
    state: &mut RunState,
    options: &RunOptions,
) -> Result<(), PittyError> {
    let label = step.label();
    match step {
        Step::Spawn(spec) => {
            let cwd = match &spec.cwd {
                Some(rel) => workspace.resolve_path(rel),
                None => workspace.cwd().to_path_buf(),
            };
            // Merge scenario-wide env with the spawn-specific env; spawn wins.
            let mut env = workspace.env().to_vec();
            for (k, v) in &spec.env {
                env.push((k.clone(), workspace.expand(v)));
            }
            // The previous session (if any) was already retired and the file
            // baselines re-primed by `run_scenario` before this step ran.
            let command = workspace.expand(&spec.command);
            // An unrecognized `split` keyword is not fatal (it must stay as
            // runnable as it is on a pitty that predates the field), so this
            // warning is the only signal the author gets that it was ignored.
            if let Some(warning) = spec.split.warning() {
                eprintln!("{}", mask_secrets(&warning, workspace.secrets()));
            }
            state.session = Some(PtySession::spawn(&command, &spec.split, &cwd, &env)?);
            Ok(())
        }
        Step::Send(text) => {
            let expanded = workspace.expand(text);
            session_mut(state)?.send_line(&expanded)
        }
        Step::SendRaw(text) => {
            let expanded = workspace.expand(text);
            session_mut(state)?.send_raw(expanded.as_bytes())
        }
        Step::Key(key) => session_mut(state)?.send_key(&key.bytes),
        Step::Wait(d) => {
            // A plain sleep is correct here: `wait` is an unconditional delay,
            // not a condition to observe, so there is nothing to wake on.
            std::thread::sleep(d.as_duration());
            Ok(())
        }
        Step::Expect(spec) => {
            let matcher = Matcher::contains(&spec.contains);
            let timeout = spec
                .timeout
                .map(|d| d.as_duration())
                .unwrap_or(DEFAULT_EXPECT_TIMEOUT);
            let outcome = session_ref(state)?.wait_for(&matcher, timeout);
            state.assertions.push(assert::from_expect(&label, outcome));
            Ok(())
        }
        Step::ExpectRegex(spec) => {
            let matcher = Matcher::regex(&spec.pattern).map_err(PittyError::Scenario)?;
            let timeout = spec
                .timeout
                .map(|d| d.as_duration())
                .unwrap_or(DEFAULT_EXPECT_TIMEOUT);
            let outcome = session_ref(state)?.wait_for(&matcher, timeout);
            state.assertions.push(assert::from_expect(&label, outcome));
            Ok(())
        }
        Step::ExpectNot(spec) => {
            let matcher = Matcher::contains(&spec.contains);
            let present = session_ref(state)?.contains_now(&matcher);
            state
                .assertions
                .push(assert::from_expect_not(&label, present));
            Ok(())
        }
        Step::ExpectFileExists(spec) => {
            let path = workspace.resolve_path(&spec.path);
            let check = assert::file::check_exists(&path);
            state.assertions.push(pass_fail(&label, check));
            Ok(())
        }
        Step::ExpectFileContains(spec) => {
            let path = workspace.resolve_path(&spec.path);
            let check = assert::file::check_contains(&path, &spec.contains);
            state.assertions.push(pass_fail(&label, check));
            Ok(())
        }
        Step::ExpectFileNotContains(spec) => {
            let path = workspace.resolve_path(&spec.path);
            let check = assert::file::check_not_contains(&path, &spec.contains);
            state.assertions.push(pass_fail(&label, check));
            Ok(())
        }
        Step::ExpectFileChanged(spec) => {
            let path = workspace.resolve_path(&spec.path);
            // A baseline is only captured at spawn time (see `prime_snapshots`).
            // Without it we cannot tell "file changed since spawn" from "file
            // existed all along": an unprimed path would fall through to the
            // absent-baseline branch and read as changed, a false positive that
            // is fatal for a regression framework. Why not silently fail the
            // assertion instead: an unprimed `expect_file_changed` is a scenario
            // authoring mistake (the step ran before any `spawn`), so surfacing
            // it as a Scenario error (exit 2) tells the author to fix the
            // ordering rather than masking it as a failed expectation.
            if !state.snapshots.is_primed(&path) {
                return Err(PittyError::Scenario(format!(
                    "expect_file_changed for {} has no baseline: it must appear \
                     after a 'spawn' so the file can be snapshotted at spawn time",
                    spec.path
                )));
            }
            let check = assert::file::check_changed(&state.snapshots, &path);
            state.assertions.push(pass_fail(&label, check));
            Ok(())
        }
        Step::ExpectExit(spec) => {
            // With a timeout, poll until the child exits or the deadline passes;
            // without one, keep the original single non-blocking poll. Why not
            // always poll with a default deadline: the bare `expect_exit: N`
            // form is documented as a non-blocking poll-once, and existing
            // scenarios rely on that (a still-running child fails immediately).
            // Opting into the deadline only when a `timeout` is given keeps that
            // contract intact while letting authors structurally eliminate the
            // race against a preceding fixed `wait`.
            let session = session_mut(state)?;
            let actual = match spec.timeout {
                Some(timeout) => {
                    let deadline = Instant::now() + timeout.as_duration();
                    session.wait_exit_code_until(deadline)?
                }
                None => session.try_exit_code()?,
            };
            state
                .assertions
                .push(assert::check_exit(&label, spec.code, actual));
            Ok(())
        }
        Step::ExpectRunning(expected) => {
            let running = session_mut(state)?.is_running()?;
            state
                .assertions
                .push(assert::check_running(&label, *expected, running));
            Ok(())
        }
        Step::ExpectJson(spec) => {
            // A malformed one-of (recorded at deserialize time) is a scenario
            // authoring error, not a failed expectation: surface it as a Scenario
            // error (exit 2) so the author fixes the YAML.
            if let Some(reason) = &spec.invalid_reason {
                return Err(PittyError::Scenario(format!(
                    "expect_json '{}': {reason}",
                    spec.path
                )));
            }
            let check = &spec.check;

            let root = match &spec.source {
                Source::Invalid(keyword) => return Err(invalid_source_error(keyword)),
                Source::File(rel) => {
                    let path = workspace.resolve_path(rel);
                    match read_json_from_file(&path) {
                        Ok(value) => Some(value),
                        Err(message) => {
                            state
                                .assertions
                                .push(AssertionResult::fail(&label, message));
                            return Ok(());
                        }
                    }
                }
                // Output source: wait until a tail JSON block appears (or the
                // deadline passes), then extract it. The cursor is NOT consumed,
                // so the same JSON can be checked by multiple expect_json steps —
                // matching expect_not's non-consuming semantics. Why not reuse
                // wait_for: wait_for matches a substring/regex and advances the
                // consume cursor; it cannot express "a parseable JSON block at
                // the tail", so a dedicated poll loop is required.
                Source::Output => {
                    let timeout = spec
                        .timeout
                        .map(|d| d.as_duration())
                        .unwrap_or(DEFAULT_EXPECT_TIMEOUT);
                    wait_for_tail_json(session_ref(state)?.output(), timeout)
                }
            };

            let result = match root {
                Some(value) => assert::json::evaluate(&value, &spec.path, check),
                None => assert::json::JsonResult {
                    passed: false,
                    message: Some("no valid JSON block found in output before timeout".to_string()),
                },
            };
            state.assertions.push(pass_fail(&label, result));
            Ok(())
        }
        Step::ExpectSnapshot(spec) => {
            // Snapshots compare the whole buffer immediately; the author is
            // responsible for placing a preceding expect/wait so the output is
            // settled. snapshot_string() never blocks.
            let output = session_ref(state)?.output().snapshot_string();
            // Resolve the snapshot path through the write-confining resolver: a
            // snapshot may be *written* under --update, so an out-of-workspace
            // path is a Scenario error (exit 2) rather than a silent escape. A
            // failed resolution aborts the step as a hard error (see
            // resolve_write_path), matching the one-of/unknown-source class.
            // The resolver returns the validated destination: the component
            // sequence containment actually approved, plus the workspace
            // directory descriptor captured before the child was spawned. The
            // recorder walks only that, so it cannot create or write anywhere
            // containment did not sanction, nor follow a workspace path that a
            // child renamed and replaced with a symlink mid-run.
            // Two refusal classes, kept distinct because a consumer can see the
            // difference. `?` propagates the *scenario* errors — a path that
            // escapes the workspace however the filesystem is arranged — as
            // exit 2 with no report, which is what 1.2.2 did for them.
            //
            // A path the *filesystem* would have refused (ENOTDIR across a
            // non-directory, ELOOP on an over-long chain) is not that: 1.2.2
            // attempted the write, and the OS error surfaced as a failed
            // snapshot assertion — exit 1, with a full JSON report carrying the
            // message. pitty now refuses before opening anything so the victim
            // is never truncated, but that is a change in *when* the refusal
            // happens, not in what it is. Reporting it here keeps both the
            // exit-code and the report-JSON contracts intact; raising it as a
            // scenario error instead silently deleted the report a pipeline
            // parses, and dropped the scenario's row from `pitty run <dir>`.
            // Which question to ask depends on whether this run will create the
            // snapshot's missing parents. Under `--update` a `..` across an
            // absent component is sound (the component is about to exist, and
            // 1.2.2 recorded such paths); on a read it never becomes sound, and
            // eliding it let an unreachable path pass by comparing against a
            // different file.
            let access = if options.update {
                crate::workspace::SnapshotAccess::Record
            } else {
                crate::workspace::SnapshotAccess::Verify
            };
            let target = match workspace.resolve_write_path(&spec.file, access)? {
                Ok(target) => target,
                Err(unresolvable) => {
                    state
                        .assertions
                        .push(AssertionResult::fail(&label, unresolvable.into_message()));
                    return Ok(());
                }
            };
            let result = assert::snapshot::check(&output, &target, spec.raw, options.update);
            state.assertions.push(snapshot_result(&label, result));
            Ok(())
        }
        Step::ExpectSemantic(spec) => {
            // (N6) A threshold outside 0.0..=1.0 can never behave as the author
            // intends (>1.0 always fails, <0.0 always passes) and would do so
            // silently. Treat it like a malformed one-of: a Scenario error (exit
            // 2) so the author fixes the YAML rather than trusting a vacuous pass
            // or chasing an unwinnable fail.
            if !(0.0..=1.0).contains(&spec.similarity) {
                return Err(PittyError::Scenario(format!(
                    "expect_semantic similarity {} is out of range; it must be within 0.0..=1.0",
                    spec.similarity
                )));
            }
            let output = match &spec.source {
                Source::Invalid(keyword) => return Err(invalid_source_error(keyword)),
                Source::File(rel) => {
                    let path = workspace.resolve_path(rel);
                    match read_text_from_file(&path) {
                        Ok(text) => text,
                        Err(message) => {
                            state
                                .assertions
                                .push(AssertionResult::fail(&label, message));
                            return Ok(());
                        }
                    }
                }
                Source::Output => session_ref(state)?.output().snapshot_string(),
            };
            let result = assert::semantic::evaluate(
                options.semantic_backend.as_ref(),
                &output,
                &spec.text,
                spec.similarity,
            );
            state.assertions.push(pass_fail(&label, result));
            Ok(())
        }
    }
}

/// How many trailing buffer bytes the tail-JSON poll scans each pass.
///
/// 64 KiB comfortably holds a CLI's final JSON report (and several lines of
/// surrounding noise) while keeping each poll O(window) instead of O(buffer).
/// Why a constant window rather than the whole buffer: see [`wait_for_tail_json`].
const TAIL_JSON_WINDOW: usize = 64 * 1024;

/// Poll the live output buffer until a parseable tail JSON block appears or the
/// deadline passes, returning the extracted value (or `None` on timeout).
///
/// Why a short-interval poll rather than the condvar handshake in `wait_for`:
/// the success condition here ("the tail parses as JSON") is not expressible as
/// a byte matcher, and re-deriving it would mean duplicating `wait_for`'s
/// internals. A 20ms poll is simple and bounded; JSON reports are emitted once,
/// so the poll typically succeeds on the first pass.
///
/// Why a bounded tail window instead of `snapshot_string()` + full scan: the
/// old form copied the whole buffer (O(B) heap) and re-ran `string_mask` over
/// all of it (O(B)) on every 20ms tick, so a 10 MB buffer over a 30s timeout
/// re-scanned gigabytes — the same quadratic trap `wait_for` already eliminated
/// with its incremental cursor. `with_tail` hands us a borrowed slice of the
/// last [`TAIL_JSON_WINDOW`] bytes under the lock (no copy), and we scan only
/// that, making each poll O(window). Why a fixed window rather than carrying an
/// incremental mask state like `wait_for`: a JSON report is self-delimiting and
/// emitted once at the tail, so a window that holds one report is sufficient and
/// far simpler than threading `in_string`/`escaped` state across polls; a block
/// larger than the window is the rare case and the window is a tunable constant.
fn wait_for_tail_json(output: &OutputBufferHandle, timeout: Duration) -> Option<serde_json::Value> {
    const POLL_INTERVAL: Duration = Duration::from_millis(20);
    let deadline = Instant::now() + timeout;
    loop {
        let found = output.with_tail(TAIL_JSON_WINDOW, assert::json::extract_tail_json_bytes);
        if let Some(value) = found {
            return Some(value);
        }
        if Instant::now() >= deadline {
            return None;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        std::thread::sleep(POLL_INTERVAL.min(remaining));
    }
}

/// Build the Scenario error for an unrecognized `source` keyword, shared by the
/// `expect_json` and `expect_semantic` arms so both reject a typo identically.
fn invalid_source_error(keyword: &str) -> PittyError {
    PittyError::Scenario(format!(
        "unknown source '{keyword}'; use 'output' (the default) or '{{file: <path>}}'"
    ))
}

/// Read `path` as UTF-8 text, returning a ready-to-report failure message on
/// error so `expect_json`/`expect_semantic` file sources share one read path.
fn read_text_from_file(path: &Path) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))
}

/// Read `path` and parse it as a single JSON document, mapping both an I/O
/// error and a parse error to a ready-to-report failure message.
fn read_json_from_file(path: &Path) -> Result<serde_json::Value, String> {
    let text = read_text_from_file(path)?;
    serde_json::from_str::<serde_json::Value>(&text)
        .map_err(|e| format!("file {} is not valid JSON: {e}", path.display()))
}

/// Capture baselines for all `expect_file_changed` paths declared in the
/// scenario, at spawn time, so that "changed" is measured from the moment the
/// process starts.
fn prime_snapshots(scenario: &Scenario, workspace: &Workspace, snapshots: &mut FileSnapshots) {
    for step in &scenario.steps {
        if let Step::ExpectFileChanged(spec) = step {
            let path = workspace.resolve_path(&spec.path);
            snapshots.capture(&path);
        }
    }
}

/// The marker the log carries in place of terminal output for a spawn-less run.
///
/// An explicit marker rather than an empty section so the log distinguishes
/// "this scenario never started a process" from "the process printed nothing",
/// which look identical to a reader otherwise.
const NO_SPAWN_OUTPUT: &str = "(no process spawned)";

/// Join every session's output into the single block the log records.
///
/// Sessions are emitted in spawn order — the buffers retired by each later
/// `spawn`, then the live one — each under a `--- session N ---` banner so a
/// multi-spawn log stays attributable to the process that produced each chunk.
/// A single-session run gets no banner, keeping the common log's shape unchanged.
fn collect_output(state: &RunState) -> String {
    let live = state
        .session
        .as_ref()
        .map(|session| session.output().snapshot_string());
    let sessions: Vec<&str> = state
        .retired_output
        .iter()
        .map(String::as_str)
        .chain(live.as_deref())
        .collect();

    match sessions.len() {
        0 => NO_SPAWN_OUTPUT.to_string(),
        1 => sessions[0].to_string(),
        _ => sessions
            .iter()
            .enumerate()
            .map(|(i, output)| format!("--- session {} ---\n{output}", i + 1))
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

/// Borrow the active session immutably, erroring if none was spawned.
fn session_ref(state: &RunState) -> Result<&PtySession, PittyError> {
    state
        .session
        .as_ref()
        .ok_or_else(|| PittyError::Scenario("step requires a prior 'spawn'".to_string()))
}

/// Borrow the active session mutably, erroring if none was spawned.
fn session_mut(state: &mut RunState) -> Result<&mut PtySession, PittyError> {
    state
        .session
        .as_mut()
        .ok_or_else(|| PittyError::Scenario("step requires a prior 'spawn'".to_string()))
}

/// A check outcome reducible to pass/fail plus an optional failure message.
///
/// File, JSON, and semantic checks all map to a report row the same way (pass,
/// or fail carrying the message), so they share one conversion via [`pass_fail`]
/// instead of three identical functions. Snapshot results are the exception —
/// they can pass *with* a note — and keep their own [`snapshot_result`].
trait CheckOutcome {
    fn into_parts(self) -> (bool, Option<String>);
}

impl CheckOutcome for assert::file::FileCheck {
    fn into_parts(self) -> (bool, Option<String>) {
        (self.passed, self.message)
    }
}

impl CheckOutcome for assert::json::JsonResult {
    fn into_parts(self) -> (bool, Option<String>) {
        (self.passed, self.message)
    }
}

impl CheckOutcome for assert::semantic::SemanticResult {
    fn into_parts(self) -> (bool, Option<String>) {
        (self.passed, self.message)
    }
}

/// Convert a pass/fail check into a report row under `label`.
fn pass_fail(label: &str, check: impl CheckOutcome) -> AssertionResult {
    let (passed, message) = check.into_parts();
    if passed {
        AssertionResult::pass(label)
    } else {
        AssertionResult::fail(label, message.unwrap_or_default())
    }
}

/// Convert an `expect_snapshot` result into a report row under `label`.
///
/// A snapshot that passed *with* a message (a record/update note) keeps that
/// note on the passing row so the report shows the snapshot was written.
fn snapshot_result(label: &str, result: assert::snapshot::SnapshotResult) -> AssertionResult {
    match (result.passed, result.message) {
        (true, None) => AssertionResult::pass(label),
        (true, Some(note)) => AssertionResult {
            step: label.to_string(),
            passed: true,
            message: Some(note),
        },
        (false, message) => AssertionResult::fail(label, message.unwrap_or_default()),
    }
}

/// Mask secret values inside a report before it leaves the runner.
///
/// Walks every assertion message and step label plus the scenario name,
/// replacing registered secrets with `***`. Why not mask only the message:
/// the on-disk log masks the whole body (label included), so masking the step
/// label here too keeps the stdout JSON and the log symmetric — a secret an
/// author wrote directly into a step (rather than via `${var}`) is redacted in
/// both sinks instead of leaking through the unmasked label.
fn mask_report(report: &mut Report, secrets: &[String]) {
    if secrets.is_empty() {
        return;
    }
    report.scenario = mask_secrets(&report.scenario, secrets);
    for assertion in &mut report.assertions {
        assertion.step = mask_secrets(&assertion.step, secrets);
        if let Some(message) = &assertion.message {
            assertion.message = Some(mask_secrets(message, secrets));
        }
    }
}

/// Turn a session teardown outcome into the run's view of it: a stall is
/// reported on stderr and changes nothing, everything else passes through.
///
/// Shared by the end-of-run teardown and a `spawn` that replaces a live
/// session, so both classify a leaked child tree the same way.
fn classify_teardown(outcome: Result<Teardown, PittyError>) -> Result<(), PittyError> {
    match outcome? {
        Teardown::Clean => Ok(()),
        Teardown::Stalled(msg) => {
            eprintln!("warning: pty teardown: {msg}");
            Ok(())
        }
    }
}

/// Mask secret values inside an error's message before it leaves the runner.
fn mask_error(err: PittyError, secrets: &[String]) -> PittyError {
    match err {
        PittyError::AssertionFailed(m) => PittyError::AssertionFailed(mask_secrets(&m, secrets)),
        PittyError::Scenario(m) => PittyError::Scenario(mask_secrets(&m, secrets)),
        PittyError::Process(m) => PittyError::Process(mask_secrets(&m, secrets)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn success_command() -> &'static str {
        if cfg!(windows) {
            "cmd.exe /C exit 0"
        } else {
            "true"
        }
    }

    #[test]
    fn missing_spawn_is_scenario_error() {
        // A send before any spawn must produce a Scenario error (exit code 2).
        let yaml = "name: x\nsteps:\n  - send: hello\n";
        let scenario = Scenario::from_yaml(yaml).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let err = run_scenario(&scenario, dir.path(), &RunOptions::default()).unwrap_err();
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn file_only_scenario_reports_assertions_without_spawn() {
        // File assertions need no process; they must run and the report status
        // must reflect their pass/fail without requiring a spawn.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("result.txt"), b"success").unwrap();
        let yaml = r#"
name: files
steps:
  - expect_file_exists:
      path: result.txt
  - expect_file_contains:
      path: result.txt
      contains: success
"#;
        let scenario = Scenario::from_yaml(yaml).unwrap();
        let report = run_scenario(&scenario, dir.path(), &RunOptions::default()).unwrap();
        assert_eq!(report.status, Status::Passed);
        assert_eq!(report.assertions.len(), 2);
        assert!(report.assertions.iter().all(|a| a.passed));
    }

    #[test]
    fn spawn_less_run_still_writes_its_log() {
        // Issue #35 defect 2: a scenario that never spawns still produces
        // assertion rows, and a failing one is exactly the run whose record is
        // worth keeping. The log must exist and carry the failing row plus the
        // explicit no-process marker in place of terminal output.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), b"actual").unwrap();
        let yaml = r#"
name: file-only-no-log
steps:
  - expect_file_contains:
      path: f.txt
      contains: nope-this-fails
"#;
        let scenario = Scenario::from_yaml(yaml).unwrap();
        let report = run_scenario(&scenario, dir.path(), &RunOptions::default()).unwrap();
        assert_eq!(report.status, Status::Failed);

        let log = std::fs::read_to_string(dir.path().join("logs/file-only-no-log.log"))
            .expect("a spawn-less run must still leave a log");
        assert!(log.contains(NO_SPAWN_OUTPUT), "log:\n{log}");
        assert!(log.contains("[FAIL] expect_file_contains"), "log:\n{log}");
    }

    #[test]
    fn collect_output_marks_a_run_that_never_spawned() {
        // With no session at all the output block is the explicit marker, so a
        // reader can tell "never started" from "printed nothing".
        let state = RunState {
            session: None,
            snapshots: FileSnapshots::new(),
            assertions: Vec::new(),
            retired_output: Vec::new(),
        };
        assert_eq!(collect_output(&state), NO_SPAWN_OUTPUT);
    }

    #[test]
    fn collect_output_keeps_every_retired_session_in_spawn_order() {
        // Issue #35 defect 1: the buffers of sessions retired by a later spawn
        // must all reach the log, in spawn order, each attributed to its session.
        let state = RunState {
            session: None,
            snapshots: FileSnapshots::new(),
            assertions: Vec::new(),
            retired_output: vec!["first-process".to_string(), "second-process".to_string()],
        };
        let output = collect_output(&state);
        assert!(output.contains("first-process"), "output:\n{output}");
        assert!(output.contains("second-process"), "output:\n{output}");
        assert!(
            output.find("first-process") < output.find("second-process"),
            "sessions must stay in spawn order:\n{output}"
        );
        assert!(output.contains("--- session 1 ---"), "output:\n{output}");
        assert!(output.contains("--- session 2 ---"), "output:\n{output}");
    }

    #[test]
    fn collect_output_leaves_a_single_session_unbannered() {
        // The common single-spawn log keeps its existing shape: raw output with
        // no session banner.
        let state = RunState {
            session: None,
            snapshots: FileSnapshots::new(),
            assertions: Vec::new(),
            retired_output: vec!["only-one".to_string()],
        };
        assert_eq!(collect_output(&state), "only-one");
    }

    #[test]
    fn expect_file_changed_without_spawn_is_scenario_error() {
        // An existing file checked by expect_file_changed in a spawn-less
        // scenario must NOT pass: with no baseline captured it would otherwise
        // read as "changed" (false positive). It must be a Scenario error
        // (exit code 2) telling the author to place the step after a spawn.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("kept.txt"), b"unchanged").unwrap();
        let yaml = r#"
name: no-spawn-changed
steps:
  - expect_file_changed:
      path: kept.txt
"#;
        let scenario = Scenario::from_yaml(yaml).unwrap();
        let err = run_scenario(&scenario, dir.path(), &RunOptions::default()).unwrap_err();
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn expect_file_changed_before_spawn_is_scenario_error() {
        // Even when the scenario does spawn later, an expect_file_changed placed
        // *before* the spawn runs with no baseline yet and must error rather
        // than falsely pass on an existing file.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("kept.txt"), b"unchanged").unwrap();
        let yaml = r#"
name: changed-before-spawn
steps:
  - expect_file_changed:
      path: kept.txt
  - spawn: "true"
"#;
        let scenario = Scenario::from_yaml(yaml).unwrap();
        let err = run_scenario(&scenario, dir.path(), &RunOptions::default()).unwrap_err();
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn mask_report_redacts_secrets_in_assertion_messages_and_name() {
        // A secret captured into an assertion message (e.g. a timeout tail of
        // PTY output) or the scenario name must be replaced with *** so the
        // stdout JSON report never leaks it.
        let mut report = Report {
            scenario: "leak-supersecret".into(),
            status: Status::Failed,
            duration_ms: 0,
            assertions: vec![
                AssertionResult::fail("expect: x", "timed out; last output: \"token=supersecret\""),
                AssertionResult::pass("expect: y"),
            ],
        };
        mask_report(&mut report, &["supersecret".to_string()]);
        let json = report.to_json();
        assert!(!json.contains("supersecret"), "raw secret leaked: {json}");
        assert!(json.contains("***"));
        assert!(json.contains("leak-***"));
    }

    #[test]
    fn expect_json_unknown_source_is_scenario_error() {
        // A typo'd source keyword must abort as a Scenario error (exit 2) rather
        // than silently reading live output.
        let dir = tempfile::tempdir().unwrap();
        let yaml = "name: x\nsteps:\n  - expect_json:\n      path: a\n      equals: 1\n      source: outpt\n";
        let scenario = Scenario::from_yaml(yaml).unwrap();
        let err = run_scenario(&scenario, dir.path(), &RunOptions::default()).unwrap_err();
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn expect_json_invalid_check_is_scenario_error() {
        // Zero/multiple checks (recorded as invalid_reason) must abort as a
        // Scenario error (exit 2) before any source read.
        let dir = tempfile::tempdir().unwrap();
        let yaml = "name: x\nsteps:\n  - expect_json:\n      path: a\n      equals: 1\n      contains: b\n";
        let scenario = Scenario::from_yaml(yaml).unwrap();
        let err = run_scenario(&scenario, dir.path(), &RunOptions::default()).unwrap_err();
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn expect_json_from_file_evaluates_without_spawn() {
        // A file-sourced expect_json needs no process: it must read the file,
        // navigate the path, and pass on a matching equals.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("report.json"), br#"{"status": "ok"}"#).unwrap();
        let yaml = "name: x\nsteps:\n  - expect_json:\n      path: status\n      equals: ok\n      source:\n        file: report.json\n";
        let scenario = Scenario::from_yaml(yaml).unwrap();
        let report = run_scenario(&scenario, dir.path(), &RunOptions::default()).unwrap();
        assert_eq!(report.status, Status::Passed);
    }

    #[test]
    fn wait_for_tail_json_extracts_from_large_buffer() {
        // A tail JSON block must be found even when preceded by megabytes of
        // noise, and the scan is bounded to the tail window (not the whole
        // buffer), so this stays fast regardless of buffer size.
        let handle = OutputBufferHandle::new();
        handle.append(&vec![b'.'; 5 * 1024 * 1024]);
        handle.append(b"\nLOG: done\n{\"status\": \"ok\", \"n\": 7}\n");
        let value = wait_for_tail_json(&handle, Duration::from_secs(1)).expect("should extract");
        assert_eq!(value, serde_json::json!({"status": "ok", "n": 7}));
    }

    #[test]
    fn wait_for_tail_json_polls_until_json_appears() {
        // When the JSON is not present yet, the poll must wait and then return it
        // once a later append produces a parseable tail block.
        let handle = OutputBufferHandle::new();
        let producer = handle.clone();
        let join = std::thread::spawn(move || wait_for_tail_json(&handle, Duration::from_secs(2)));
        std::thread::sleep(Duration::from_millis(40));
        producer.append(b"warming up...\n");
        std::thread::sleep(Duration::from_millis(40));
        producer.append(b"{\"ready\": true}\n");
        let value = join.join().unwrap().expect("should extract after append");
        assert_eq!(value, serde_json::json!({"ready": true}));
    }

    #[test]
    fn wait_for_tail_json_drops_block_pushed_beyond_window() {
        // (R4) Known, fixed behavior: the tail-JSON poll only scans the last
        // TAIL_JSON_WINDOW bytes. If a real JSON report is followed by more than
        // a window's worth of trailing noise, the report is pushed out of the
        // window and is NOT extracted (the poll times out -> None). This pins the
        // documented limitation so it cannot flake silently; authors must place
        // the JSON at the tail or use source: {file}.
        let handle = OutputBufferHandle::new();
        handle.append(b"{\"status\": \"ok\"}\n");
        // Append strictly more than one window of trailing noise after the JSON.
        handle.append(&vec![b'.'; TAIL_JSON_WINDOW + 1024]);
        let value = wait_for_tail_json(&handle, Duration::from_millis(50));
        assert!(
            value.is_none(),
            "a JSON block shoved past the tail window must not be extracted"
        );
    }

    #[test]
    fn wait_for_tail_json_times_out_without_json() {
        // With no JSON ever appearing, the poll must return None at the deadline
        // rather than blocking forever.
        let handle = OutputBufferHandle::new();
        handle.append(b"just plain log lines, no json here\n");
        let value = wait_for_tail_json(&handle, Duration::from_millis(30));
        assert!(value.is_none());
    }

    #[test]
    fn expect_semantic_out_of_range_similarity_is_scenario_error() {
        // (N6) A similarity threshold outside 0.0..=1.0 must abort as a Scenario
        // error (exit 2), not silently always-pass (<0) or always-fail (>1).
        let dir = tempfile::tempdir().unwrap();
        let high = "name: x\nsteps:\n  - expect_semantic:\n      text: hi\n      similarity: 1.5\n      source:\n        file: out.txt\n";
        std::fs::write(dir.path().join("out.txt"), b"hi").unwrap();
        let scenario = Scenario::from_yaml(high).unwrap();
        let err = run_scenario(&scenario, dir.path(), &RunOptions::default()).unwrap_err();
        assert_eq!(err.exit_code(), 2);

        let low = "name: x\nsteps:\n  - expect_semantic:\n      text: hi\n      similarity: -0.1\n      source:\n        file: out.txt\n";
        let scenario = Scenario::from_yaml(low).unwrap();
        let err = run_scenario(&scenario, dir.path(), &RunOptions::default()).unwrap_err();
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn expect_snapshot_escape_path_is_scenario_error_and_writes_nothing() {
        // (C3) A snapshot whose `file` escapes the workspace via `..` must be a
        // Scenario error (exit 2) under --update, and must NOT write the file
        // outside the workspace.
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().parent().unwrap().join("pitty-escape.snap");
        // Defensive: ensure no stale file from a prior run.
        let _ = std::fs::remove_file(&outside);
        let yaml = format!(
            "name: x\nsteps:\n  - spawn: \"{}\"\n  - expect_snapshot:\n      file: ../pitty-escape.snap\n",
            success_command()
        );
        let scenario = Scenario::from_yaml(&yaml).unwrap();
        let options = RunOptions {
            update: true,
            ..RunOptions::default()
        };
        let err = run_scenario(&scenario, dir.path(), &options).unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert!(
            !outside.exists(),
            "snapshot must not be written outside the workspace"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_path_the_filesystem_refuses_fails_the_assertion_and_still_reports() {
        // (Contract) The two refusal classes are not interchangeable, and a
        // consumer can tell them apart.
        //
        // `../escape.snap` is wrong whatever is on disk, so it stays a scenario
        // error: exit 2, no report (pinned by the test above). But a path the
        // *filesystem* would refuse — here `..` across a regular file, which the
        // kernel answers with ENOTDIR — was a failed snapshot assertion in
        // 1.2.2: exit 1, with a full JSON report carrying the message. Verified
        // by running 1.2.2.
        //
        // pitty now refuses before opening anything, so the victim is never
        // truncated. That is a change in *when* the refusal is detected, not in
        // what it is, and it must not cost the report: a pipeline parses stdout
        // for per-assertion results, and `pitty run <dir>` needs a row for this
        // scenario. Raising it as a scenario error deleted both.
        //
        // This asserts the observable contract (a report exists, with a failed
        // assertion) rather than the internal error type, because that is what a
        // consumer actually depends on.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("blocker"), "a regular file").unwrap();
        std::fs::write(dir.path().join("victim.snap"), "ORIGINAL").unwrap();
        let yaml = format!(
            "name: x\nsteps:\n  - spawn: \"{}\"\n  - expect_snapshot:\n      file: blocker/../victim.snap\n",
            success_command()
        );
        let scenario = Scenario::from_yaml(&yaml).unwrap();
        let options = RunOptions {
            update: true,
            ..RunOptions::default()
        };

        // A report, not an Err: this is an assertion failure, not a malformed
        // scenario.
        let report = run_scenario(&scenario, dir.path(), &options)
            .expect("a filesystem-refused path must still produce a report");
        let snapshot_step = report
            .assertions
            .iter()
            .find(|a| a.step.contains("expect_snapshot"))
            .expect("the snapshot step must appear in the report");
        assert!(
            !snapshot_step.passed,
            "the refused path must fail the assertion rather than pass"
        );
        // And the data-loss guarantee the whole fix exists for.
        assert_eq!(
            std::fs::read_to_string(dir.path().join("victim.snap")).unwrap(),
            "ORIGINAL",
            "a path the kernel refuses must never reach a real file"
        );
    }

    #[test]
    fn expect_snapshot_in_workspace_subdir_records_under_update() {
        // (C3) The containment must not break the normal case: a snapshot in a
        // workspace subdirectory records under --update and is written inside the
        // workspace.
        let dir = tempfile::tempdir().unwrap();
        let yaml = format!(
            "name: x\nsteps:\n  - spawn: \"{}\"\n  - expect_snapshot:\n      file: __snapshots__/x.snap\n",
            success_command()
        );
        let scenario = Scenario::from_yaml(&yaml).unwrap();
        let options = RunOptions {
            update: true,
            ..RunOptions::default()
        };
        let report = run_scenario(&scenario, dir.path(), &options).unwrap();
        assert_eq!(report.status, Status::Passed);
        assert!(dir.path().join("__snapshots__/x.snap").exists());
    }

    #[test]
    fn failing_file_assertion_marks_failed() {
        // A failing file assertion must drive overall status to Failed while
        // still completing the run.
        let dir = tempfile::tempdir().unwrap();
        let yaml = r#"
name: files
steps:
  - expect_file_exists:
      path: nope.txt
"#;
        let scenario = Scenario::from_yaml(yaml).unwrap();
        let report = run_scenario(&scenario, dir.path(), &RunOptions::default()).unwrap();
        assert_eq!(report.status, Status::Failed);
        assert!(!report.assertions[0].passed);
    }
}
