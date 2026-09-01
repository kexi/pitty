//! `PtySession`: owns the PTY, the child, the writer, and the reader thread.
//!
//! The public surface is deliberately synchronous: callers `expect`/`write`
//! against blocking methods, and all the concurrency (the reader thread and the
//! condvar handshake) is hidden behind [`PtySession::wait_for`]. This keeps the
//! async complexity contained while exposing a simple, testable contract.

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
}

/// Upper bound on each blocking phase of [`PtySession::shutdown`].
///
/// Why bound it at all: on Windows, ConPTY teardown of a Git for Windows bash
/// child was observed to stall for about five minutes per session (both after
/// `TerminateProcess` of an idle shell and when the shell was racing its own
/// `exit`), which turned a 500ms scenario into a 5-minute one and hid inside
/// the report because the runner fixes `duration_ms` before teardown. Healthy
/// teardown on every platform is on the order of milliseconds (macOS is the
/// slowest at ~100ms), so 5s is generous on the good path and caps the bad one.
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

        let child = pair
            .slave
            .spawn_command(builder)
            .map_err(|e| PittyError::Process(format!("spawn failed: {e}")))?;

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
        // Poll cadence: short enough to observe a fresh exit promptly, long
        // enough to avoid a busy loop. PTY teardown is on the order of tens of
        // milliseconds, so 10ms keeps observation tight without spinning.
        const POLL_INTERVAL: Duration = Duration::from_millis(10);
        loop {
            if let Some(code) = self.try_exit_code()? {
                return Ok(Some(code));
            }
            if std::time::Instant::now() >= deadline {
                return Ok(None);
            }
            // Never overshoot the deadline: cap the sleep at the remaining time
            // so the loop's worst-case overrun is one `try_wait` call.
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            std::thread::sleep(POLL_INTERVAL.min(remaining));
        }
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

    /// Best-effort terminate the child and join the reader thread, bounded by
    /// [`SHUTDOWN_GRACE`] per phase.
    ///
    /// Called from `Drop`, but exposed so the runner can tear down explicitly
    /// and surface a teardown problem when it matters. Every phase runs even if
    /// an earlier one failed: returning early would leave the master handle to
    /// be dropped on the caller's thread later, which is exactly the unbounded
    /// block this method exists to avoid. Problems are collected and reported
    /// together at the end.
    pub fn shutdown(&mut self) -> Result<(), PittyError> {
        if self.torn_down {
            return Ok(());
        }
        self.torn_down = true;
        let mut problems: Vec<String> = Vec::new();

        // Kill only if still running; killing an already-exited child is a
        // no-op we would rather not surface as an error.
        if matches!(self.child.try_wait(), Ok(None)) {
            if let Err(e) = self.child.kill() {
                problems.push(format!("failed to kill child: {e}"));
            }
            // Why poll instead of `wait()`: portable-pty's `wait` is unbounded,
            // and on Windows `TerminateProcess` only *requests* termination — a
            // child parked in a console read can take a long time to actually
            // die. A bounded poll keeps a stuck child from stalling the runner.
            match self.wait_exit_code_until(Instant::now() + SHUTDOWN_GRACE) {
                Ok(Some(_)) => {}
                Ok(None) => {
                    problems.push(format!("child still running {SHUTDOWN_GRACE:?} after kill"))
                }
                Err(e) => problems.push(format!("{e}")),
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
        let (done_tx, done_rx) = mpsc::channel::<()>();
        std::thread::spawn(move || {
            drop(writer);
            drop(master);
            if let Some(t) = reader {
                let _ = t.join();
            }
            let _ = done_tx.send(());
        });
        if done_rx.recv_timeout(SHUTDOWN_GRACE).is_err() {
            problems.push(format!(
                "pty teardown still blocked after {SHUTDOWN_GRACE:?}; abandoning it"
            ));
        }

        if problems.is_empty() {
            Ok(())
        } else {
            Err(PittyError::Process(problems.join("; ")))
        }
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        // Best-effort cleanup; Drop cannot propagate errors.
        let _ = self.shutdown();
    }
}
