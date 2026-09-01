//! `PtySession`: owns the PTY, the child, the writer, and the reader thread.
//!
//! The public surface is deliberately synchronous: callers `expect`/`write`
//! against blocking methods, and all the concurrency (the reader thread and the
//! condvar handshake) is hidden behind [`PtySession::wait_for`]. This keeps the
//! async complexity contained while exposing a simple, testable contract.

#[cfg(windows)]
mod job;
pub mod matcher;
pub mod reader;

use std::io::Write;
use std::path::Path;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};

use crate::error::PittyError;

pub use matcher::{wait_for, ExpectOutcome, Matcher};
use reader::OutputBufferHandle;

/// How [`PtySession::shutdown`] ended when it did not fail outright.
///
/// The split exists because not every teardown problem means the same thing.
/// A child (or its tree) that is still alive, or a reader thread that
/// panicked, leaves the environment polluted and is a process error the
/// verdict must reflect. A console host that merely takes longer than the
/// grace period to release its handles after every process in the tree is
/// already dead changes nothing about the run; it is reported, not fatal.
#[derive(Debug, PartialEq, Eq)]
pub enum Teardown {
    /// Every phase completed within its grace period.
    Clean,
    /// The child tree is proven dead (Windows: the job was terminated) but
    /// releasing the console handles exceeded the grace period; the teardown
    /// was abandoned on its helper thread. Never produced where the tree kill
    /// is only best-effort (Unix): there a blocked release is fatal instead.
    Stalled(String),
}

/// A live PTY session with a spawned child process.
pub struct PtySession {
    /// The master side. Retained so it is not dropped (which would close the
    /// PTY) while the session is alive.
    master: Option<Box<dyn MasterPty + Send>>,
    /// The writer used to send stdin to the child.
    writer: Option<Box<dyn Write + Send>>,
    /// The spawned child handle (wait/try_wait/kill).
    child: Box<dyn Child + Send + Sync>,
    /// Shared output buffer fed by the reader thread.
    output: OutputBufferHandle,
    /// Join handle for the reader thread, joined on teardown.
    reader_thread: Option<std::thread::JoinHandle<()>>,
    /// Set once `shutdown` has run so `Drop` does not repeat the (bounded but
    /// non-trivial) teardown and pay its grace period a second time.
    torn_down: bool,
    /// Windows only: the job object the child (and everything it spawns) lives
    /// in, so teardown can kill the whole tree rather than just the direct
    /// child. See [`job`] for why a plain `TerminateProcess` is not enough.
    /// `Option` so `shutdown` can take and drop it (closing the last handle is
    /// what makes `KILL_ON_JOB_CLOSE` fire) before the console handles go.
    #[cfg(windows)]
    job: Option<job::Job>,
}

/// Upper bound on each blocking phase of [`PtySession::shutdown`].
///
/// Why bound it at all: on Windows, ConPTY teardown of a Git for Windows bash
/// child was observed to stall for about five minutes per session, which
/// turned a 500ms scenario into a 5-minute one and hid inside the report
/// because the runner fixes `duration_ms` before teardown. CI diagnostics
/// placed the stall in the handle-teardown phase — the console host kept the
/// output pipe open after the direct child had been terminated, consistent
/// with another process still attached to the pseudoconsole — which is why
/// Windows now terminates the child's whole job (tree) first. The bound stays
/// as the backstop for whatever the console host does next. Healthy teardown
/// on every platform is on the order of milliseconds (macOS is the slowest at
/// ~100ms), so 5s is generous on the good path and caps the bad one.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

impl PtySession {
    /// Open a PTY and spawn `command` (a shell-style command line) within it.
    ///
    /// The first whitespace-separated token is the program; the remainder are
    /// arguments. `cwd` sets the working directory and `env` injects extra
    /// environment variables. PTY/spawn failures classify as
    /// [`PittyError::Process`] (exit code 3).
    ///
    /// Argument splitting is plain `split_whitespace`: it does NOT honor shell
    /// quoting or escapes, so a program path containing spaces or an argument
    /// with embedded whitespace (e.g. `"my arg"`) is not parsed as a single
    /// token. We avoid a shell-quoting parser in v0.1 to keep spawning
    /// dependency-free and predictable; wrap such a command in an explicit shell
    /// (`spawn: sh -c '...'`) if you need shell semantics.
    pub fn spawn(command: &str, cwd: &Path, env: &[(String, String)]) -> Result<Self, PittyError> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| PittyError::Process(format!("openpty failed: {e}")))?;

        let mut parts = command.split_whitespace();
        let program = parts
            .next()
            .ok_or_else(|| PittyError::Process("empty spawn command".to_string()))?;
        let mut builder = CommandBuilder::new(program);
        for arg in parts {
            builder.arg(arg);
        }
        builder.cwd(cwd);
        for (k, v) in env {
            builder.env(k, v);
        }

        // Windows: create the kill-on-close job *before* the child exists, so
        // the only work left after `CreateProcess` is the assignment itself.
        // Anything the child spawns (a launcher's real shell, a shell's
        // grandchildren) then dies with it at teardown instead of keeping the
        // pseudoconsole alive. Failing closed is deliberate: a session we
        // cannot tear down reliably is a process error, not a silently weaker
        // session.
        #[cfg(windows)]
        let job = job::Job::new()
            .map_err(|e| PittyError::Process(format!("job object setup failed: {e}")))?;

        let child = pair
            .slave
            .spawn_command(builder)
            .map_err(|e| PittyError::Process(format!("spawn failed: {e}")))?;

        // Assignment is not atomic with process creation: portable-pty 0.8
        // exposes neither `CREATE_SUSPENDED` nor `PROC_THREAD_ATTRIBUTE_JOB_LIST`,
        // so a descendant the child manages to spawn before this call returns
        // is not a job member. The window is the child's own start-up, which
        // for every supported shell is far longer than the assignment; the
        // residual gap is documented in `job.rs` rather than hidden.
        #[cfg(windows)]
        let job = {
            let assigned = child
                .as_raw_handle()
                .ok_or_else(|| "spawned child has no process handle".to_string())
                .and_then(|handle| {
                    job.assign(handle)
                        .map_err(|e| format!("job assignment failed: {e}"))
                });
            match assigned {
                Ok(()) => job,
                Err(msg) => {
                    // Do not leak the process we just started: without a job to
                    // reap it, the caller has no handle to it once we return.
                    // Every cleanup problem is folded into the error so a child
                    // that outlived this attempt is visible, not silent.
                    let mut child = child;
                    let mut problems = vec![msg];
                    if let Err(e) = child.kill() {
                        problems.push(format!("cleanup kill failed: {e}"));
                    }
                    match wait_child_until(&mut *child, Instant::now() + SHUTDOWN_GRACE) {
                        Ok(Some(_)) => {}
                        Ok(None) => problems.push(format!(
                            "cleanup: child still running {SHUTDOWN_GRACE:?} after kill"
                        )),
                        Err(e) => problems.push(format!("cleanup: {e}")),
                    }
                    return Err(PittyError::Process(problems.join("; ")));
                }
            }
        };

        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| PittyError::Process(format!("clone reader failed: {e}")))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|e| PittyError::Process(format!("take writer failed: {e}")))?;

        let output = OutputBufferHandle::new();
        let reader_thread = reader::spawn_reader(reader, output.clone());

        // Drop the slave so that, once the child exits, the master read side
        // sees EOF. Keeping the slave open would make the reader thread block
        // forever and never observe closure.
        drop(pair.slave);

        Ok(PtySession {
            master: Some(pair.master),
            writer: Some(writer),
            child,
            output,
            reader_thread: Some(reader_thread),
            torn_down: false,
            #[cfg(windows)]
            job: Some(job),
        })
    }

    /// Write a line to the child's stdin, appending a carriage return.
    ///
    /// Uses `\r` (not `\n`) because a PTY in canonical mode treats CR as the
    /// line terminator the same way a real Enter keypress does.
    pub fn send_line(&mut self, text: &str) -> Result<(), PittyError> {
        self.write_bytes(text.as_bytes())?;
        self.write_bytes(b"\r")
    }

    /// Write raw bytes to stdin with no terminator appended.
    pub fn send_raw(&mut self, bytes: &[u8]) -> Result<(), PittyError> {
        self.write_bytes(bytes)
    }

    /// Write a key's resolved byte sequence to stdin.
    pub fn send_key(&mut self, bytes: &[u8]) -> Result<(), PittyError> {
        self.write_bytes(bytes)
    }

    fn write_bytes(&mut self, bytes: &[u8]) -> Result<(), PittyError> {
        let writer = self
            .writer
            .as_mut()
            .ok_or_else(|| PittyError::Process("pty writer is closed".to_string()))?;
        writer
            .write_all(bytes)
            .and_then(|()| writer.flush())
            .map_err(|e| PittyError::Process(format!("write to pty failed: {e}")))
    }

    /// Block until `matcher` matches new output, or `timeout`/EOF intervenes.
    pub fn wait_for(&self, matcher: &Matcher, timeout: Duration) -> ExpectOutcome {
        wait_for(&self.output, matcher, timeout)
    }

    /// Immediately test whether the unconsumed output contains a match.
    ///
    /// This backs `expect_not`: per the design it never waits. If a match
    /// exists in the unconsumed tail right now, the assertion fails; otherwise
    /// it succeeds immediately. Waiting would be wrong — `expect_not` asserts a
    /// property of output seen *so far*, not a prediction about the future. The
    /// cursor-scoped lookup lives on the buffer handle (`contains_now`).
    pub fn contains_now(&self, matcher: &Matcher) -> bool {
        self.output.contains_now(matcher)
    }

    /// Poll whether the child has exited; returns its exit code if so.
    pub fn try_exit_code(&mut self) -> Result<Option<i32>, PittyError> {
        match self.child.try_wait() {
            Ok(Some(status)) => Ok(Some(status.exit_code() as i32)),
            Ok(None) => Ok(None),
            Err(e) => Err(PittyError::Process(format!("try_wait failed: {e}"))),
        }
    }

    /// Poll for the child's exit until it exits or `deadline` elapses.
    ///
    /// Returns `Ok(Some(code))` as soon as the child has exited, or `Ok(None)`
    /// if the child is still running once the deadline passes. This backs the
    /// deadline form of `expect_exit`: it removes the dependence on a preceding
    /// fixed `wait` being long enough by actively waiting for the child up to
    /// the deadline.
    ///
    /// Why poll rather than block on `wait()`: `wait()` blocks until exit with
    /// no upper bound, so a child that never exits would hang the assertion.
    /// Polling `try_wait` on a short interval bounds the wait at `deadline`
    /// while still returning the instant the child exits. The interval is kept
    /// small relative to typical teardown so the observed exit is prompt, and
    /// we sleep between polls rather than spinning so the wait does not burn a
    /// core. The final poll runs even after the deadline to avoid a race where
    /// the child exits during the last sleep.
    pub fn wait_exit_code_until(
        &mut self,
        deadline: std::time::Instant,
    ) -> Result<Option<i32>, PittyError> {
        wait_child_until(&mut *self.child, deadline)
    }

    /// Block until the child exits and return its exit code.
    ///
    /// Not used by the scenario runner — `expect_exit` is a non-blocking poll
    /// via [`Self::try_exit_code`] (a scenario waits for exit explicitly with a
    /// `wait`/`expect` step). This blocking variant is retained as part of the
    /// public library surface for embedders driving a `PtySession` directly.
    pub fn wait_exit_code(&mut self) -> Result<i32, PittyError> {
        self.child
            .wait()
            .map(|status| status.exit_code() as i32)
            .map_err(|e| PittyError::Process(format!("wait failed: {e}")))
    }

    /// Whether the child is still running.
    pub fn is_running(&mut self) -> Result<bool, PittyError> {
        Ok(self.try_exit_code()?.is_none())
    }

    /// Borrow the output handle for log snapshots.
    pub fn output(&self) -> &OutputBufferHandle {
        &self.output
    }

    /// Terminate the child (tree, on Windows) and release the console, bounded
    /// by [`SHUTDOWN_GRACE`] per phase.
    ///
    /// Called from `Drop`, but exposed so the runner can tear down explicitly
    /// and classify the outcome: `Err` for anything that leaves the child or
    /// its tree alive (the verdict becomes a process error), [`Teardown`] for
    /// the rest. Every phase runs even if an earlier one failed: returning
    /// early would leave the master handle to be dropped on the caller's
    /// thread later, which is exactly the unbounded block this method exists
    /// to avoid.
    pub fn shutdown(&mut self) -> Result<Teardown, PittyError> {
        if self.torn_down {
            return Ok(Teardown::Clean);
        }
        self.torn_down = true;
        // Fatal: the environment may be polluted (a live child or tree, an
        // unreadable status, a broken reader). Stall: only the console-handle
        // release ran out of time after the tree was already dead.
        let mut fatal: Vec<String> = Vec::new();

        // Windows: terminate the whole job unconditionally, whatever the direct
        // child's state — a launcher or shell that already exited can leave
        // job members behind, and any of them keeps the console host (and the
        // output pipe the reader blocks on) alive. Dropping the handle
        // afterwards closes the last reference, so `KILL_ON_JOB_CLOSE` is the
        // backstop for anything `TerminateJobObject` missed.
        #[cfg(windows)]
        let tree_killed = match self.job.take() {
            Some(job) => {
                let ok = job.terminate();
                if let Err(e) = &ok {
                    fatal.push(format!("failed to terminate job: {e}"));
                }
                drop(job);
                ok.is_ok()
            }
            None => false,
        };
        #[cfg(not(windows))]
        let tree_killed = false;

        // Kill only if still running; killing an already-exited child is a
        // no-op we would rather not surface as an error. An unreadable status
        // is reported but still treated as "possibly running", so the kill is
        // attempted rather than skipped on the optimistic reading.
        let still_running = match self.child.try_wait() {
            Ok(None) => true,
            Ok(Some(_)) => false,
            Err(e) => {
                fatal.push(format!("cannot read child status: {e}"));
                true
            }
        };
        if still_running {
            // Unix: the child is a session leader (portable-pty calls setsid),
            // so its pid names its process group. Killing the group takes the
            // foreground descendants with it — a `sh -c` pipeline, a server
            // the scenario started — which a kill of the leader alone would
            // orphan. Background jobs under job control sit in their own
            // groups and are not covered, so this is best-effort and does not
            // count as a proven tree kill (see `tree_killed`).
            #[cfg(unix)]
            if let Some(pid) = self.child.process_id() {
                // SAFETY: plain syscall; a stale pid only yields ESRCH, which
                // is ignored because the direct kill below still runs.
                unsafe {
                    libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
                }
            }
            // The direct kill is the Unix path and the Windows fallback for a
            // job that could not be terminated; a successful job termination
            // already covered the direct child.
            if !tree_killed {
                if let Err(e) = self.child.kill() {
                    fatal.push(format!("failed to kill child: {e}"));
                }
            }
            // Why poll instead of `wait()`: portable-pty's `wait` is unbounded,
            // and on Windows `TerminateProcess` only *requests* termination — a
            // child parked in a console read can take a long time to actually
            // die. A bounded poll keeps a stuck child from stalling the runner.
            match self.wait_exit_code_until(Instant::now() + SHUTDOWN_GRACE) {
                Ok(Some(_)) => {}
                Ok(None) => {
                    fatal.push(format!("child still running {SHUTDOWN_GRACE:?} after kill"))
                }
                Err(e) => fatal.push(format!("{e}")),
            }
        }

        // Why not just join the reader thread here: Windows ConPTY may keep the
        // read side open while the owning master/writer handles are still live,
        // so close them before waiting for the reader to observe EOF.
        //
        // Why a helper thread: dropping the master (`ClosePseudoConsole` on
        // Windows) and joining the reader can both block indefinitely when the
        // console host does not release the output pipe. Moving them off the
        // caller's thread lets us wait with a deadline and abandon the teardown
        // (leaking one thread and its handles) instead of hanging the scenario.
        let writer = self.writer.take();
        let master = self.master.take();
        let reader = self.reader_thread.take();
        let (done_tx, done_rx) = mpsc::channel::<Result<(), String>>();
        std::thread::spawn(move || {
            drop(writer);
            drop(master);
            let outcome = match reader {
                Some(t) => t.join().map_err(|_| "reader thread panicked".to_string()),
                None => Ok(()),
            };
            let _ = done_tx.send(outcome);
        });
        // A timeout is only a benign stall when the whole tree is proven dead
        // (Windows job terminated). Without that proof — Unix, or a Windows
        // job that failed to terminate — a blocked reader most likely means a
        // surviving descendant still holds the PTY, which is a live process
        // the run leaves behind: fatal, like any other unreaped child.
        let stalled = match done_rx.recv_timeout(SHUTDOWN_GRACE) {
            Ok(Ok(())) => None,
            Ok(Err(e)) => {
                fatal.push(e);
                None
            }
            Err(mpsc::RecvTimeoutError::Timeout) if tree_killed => Some(format!(
                "console handles still held {SHUTDOWN_GRACE:?} after the child tree exited; abandoning their release"
            )),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                fatal.push(format!(
                    "pty still open {SHUTDOWN_GRACE:?} after killing the child; a descendant is probably holding it"
                ));
                None
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                fatal.push("pty teardown helper thread died before reporting".to_string());
                None
            }
        };

        if !fatal.is_empty() {
            return Err(PittyError::Process(fatal.join("; ")));
        }
        Ok(match stalled {
            Some(msg) => Teardown::Stalled(msg),
            None => Teardown::Clean,
        })
    }
}

/// Poll `child` for exit until it exits or `deadline` elapses.
///
/// Free function rather than a method so `spawn`'s failure path can reap a
/// child it has not wrapped in a session yet.
fn wait_child_until(
    child: &mut (dyn Child + Send + Sync),
    deadline: Instant,
) -> Result<Option<i32>, PittyError> {
    // Poll cadence: short enough to observe a fresh exit promptly, long
    // enough to avoid a busy loop. PTY teardown is on the order of tens of
    // milliseconds, so 10ms keeps observation tight without spinning.
    const POLL_INTERVAL: Duration = Duration::from_millis(10);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(Some(status.exit_code() as i32)),
            Ok(None) => {}
            Err(e) => return Err(PittyError::Process(format!("try_wait failed: {e}"))),
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        // Never overshoot the deadline: cap the sleep at the remaining time
        // so the loop's worst-case overrun is one `try_wait` call.
        let remaining = deadline.saturating_duration_since(Instant::now());
        std::thread::sleep(POLL_INTERVAL.min(remaining));
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        // Best-effort cleanup; Drop cannot propagate errors.
        let _ = self.shutdown();
    }
}
