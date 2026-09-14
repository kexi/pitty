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
    /// PTY) while the session is alive. `None` once `shutdown` has taken it,
    /// which is also how `Drop` knows not to repeat the (bounded but
    /// non-trivial) teardown and pay its grace period a second time.
    master: Option<Box<dyn MasterPty + Send>>,
    /// The writer used to send stdin to the child.
    writer: Option<Box<dyn Write + Send>>,
    /// The spawned child handle (wait/try_wait/kill).
    child: Box<dyn Child + Send + Sync>,
    /// Shared output buffer fed by the reader thread.
    output: OutputBufferHandle,
    /// Join handle for the reader thread, joined on teardown.
    reader_thread: Option<std::thread::JoinHandle<()>>,
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
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

impl PtySession {
    /// Open a PTY and spawn `command` (a shell-style command line) within it.
    ///
    /// `command` is split under `split` (see [`split_command`]): the first
    /// word is the program, the rest are its arguments. `cwd` sets the working
    /// directory and `env` injects extra environment variables. Tokenization,
    /// PTY, and spawn failures all classify as [`PittyError::Process`] (exit
    /// code 3).
    pub fn spawn(
        command: &str,
        split: &SplitMode,
        cwd: &Path,
        env: &[(String, String)],
    ) -> Result<Self, PittyError> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| PittyError::Process(format!("openpty failed: {e}")))?;

        let words = split_command(command, split)?;
        let (program, args) = words
            .split_first()
            .ok_or_else(|| PittyError::Process("empty spawn command".to_string()))?;
        let mut builder = CommandBuilder::new(program);
        for arg in args {
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

        // Not atomic with `CreateProcess` (see the known gap in `job.rs`): the
        // assignment is the very next thing after the spawn to keep that
        // window at the child's own start-up.
        #[cfg(windows)]
        let child = assign_to_job(&job, child)?;

        // From here on the child exists, so every failure must reap it: the
        // caller gets no handle to it once we return an error.
        let reader = match pair.master.try_clone_reader() {
            Ok(reader) => reader,
            Err(e) => return Err(reap_orphan(child, format!("clone reader failed: {e}"))),
        };
        let writer = match pair.master.take_writer() {
            Ok(writer) => writer,
            Err(e) => return Err(reap_orphan(child, format!("take writer failed: {e}"))),
        };

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
        observe_exit(&mut *self.child)
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
        poll_exit_until(&mut *self.child, deadline, observe_exit)
    }

    /// Block until the child exits and return its exit code.
    ///
    /// Not used by the scenario runner — `expect_exit` is a non-blocking poll
    /// via [`Self::try_exit_code`] (a scenario waits for exit explicitly with a
    /// `wait`/`expect` step). This blocking variant is retained as part of the
    /// public library surface for embedders driving a `PtySession` directly.
    ///
    /// Why a poll rather than `child.wait()`: on Unix `wait` reaps, and the
    /// session relies on the child staying reapable-but-unreaped until
    /// `shutdown` (see [`observe_exit`]).
    pub fn wait_exit_code(&mut self) -> Result<i32, PittyError> {
        loop {
            if let Some(code) = observe_exit(&mut *self.child)? {
                return Ok(code);
            }
            std::thread::sleep(EXIT_POLL_INTERVAL);
        }
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
        // Taking the master first is the re-entry guard: `Drop` calls
        // `shutdown` again — including during a panic unwind out of this
        // method — and must find nothing left to tear down rather than repeat
        // the kills and pay the grace periods a second time.
        let Some(master) = self.master.take() else {
            return Ok(Teardown::Clean);
        };
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

        // Unix: the child is a session leader (portable-pty calls setsid), so
        // its pid names its process group. Killing that group takes along
        // whatever still shares it — a `sh -c` pipeline, a plain `exec`'d
        // server, a background job of a shell without job control — which a
        // kill of the leader alone would orphan, and it runs even after the
        // leader has exited: that is exactly how such a job gets left behind
        // (a group outlives its leader, and the kernel's own SIGHUP to the
        // foreground group on leader exit does nothing for a job that ignores
        // HUP).
        //
        // Why this is safe to do unconditionally: the leader has never been
        // reaped at this point — every exit observation goes through
        // `observe_exit`, which peeks with `waitid(WNOWAIT)` — so its pid is
        // still held by either a live process or its zombie and cannot have
        // been recycled by a stranger. That is also why the sweep comes
        // before the status poll and the reap below, not after them. Why not
        // the reader seeing EOF as the "everything gone" signal instead: XNU
        // revokes the controlling terminal when the session leader exits, so
        // on macOS the reader hits EOF while a same-group job is still alive.
        // It is best-effort, not a session kill: an interactive shell's job
        // control puts each pipeline in its own group, so those are not
        // covered, and this never counts as a proven tree kill (see
        // `tree_killed`).
        #[cfg(unix)]
        if let Some(pid) = self.child.process_id() {
            // SAFETY: plain syscall. A group that is already empty yields
            // ESRCH, which is ignored because the direct kill below still
            // runs for a live leader.
            unsafe {
                libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
            }
        }

        // Kill only if still running; killing an already-exited child is a
        // no-op we would rather not surface as an error. An unreadable status
        // is reported but still treated as "possibly running", so the kill is
        // attempted rather than skipped on the optimistic reading. This is the
        // one place the child is actually reaped (`await_reap` / `try_wait`);
        // the sweep above must already have happened.
        let still_running = match observe_exit(&mut *self.child) {
            Ok(None) => true,
            Ok(Some(_)) => false,
            Err(e) => {
                fatal.push(format!("cannot read child status: {}", e.message()));
                true
            }
        };
        if !still_running {
            // Release the zombie now that the group sweep no longer needs its
            // pid pinned. It has exited, so this returns at once; anything but
            // a successful reap (or ECHILD from a reap that already happened
            // elsewhere) is a teardown problem, not something to drop.
            match self.child.try_wait() {
                Ok(Some(_)) => {}
                Ok(None) => fatal.push(
                    "child observed as exited but the reap found it still running".to_string(),
                ),
                Err(e) => {
                    #[cfg(unix)]
                    let already_reaped = e.raw_os_error() == Some(libc::ECHILD);
                    #[cfg(not(unix))]
                    let already_reaped = false;
                    if !already_reaped {
                        fatal.push(format!("failed to reap exited child: {e}"));
                    }
                }
            }
        }

        if still_running {
            // The direct kill is the Unix path and the Windows fallback for a
            // job that could not be terminated; a successful job termination
            // already covered the direct child.
            if !tree_killed {
                if let Err(e) = self.child.kill() {
                    fatal.push(format!("failed to kill child: {e}"));
                }
            }
            fatal.extend(await_reap(&mut *self.child, SHUTDOWN_GRACE));
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
        Ok(stalled.map_or(Teardown::Clean, Teardown::Stalled))
    }
}

/// How a `spawn` command line is turned into `[program, args...]`.
///
/// Two rules, because the scenario format is frozen under the v1 compatibility
/// contract (`COMPATIBILITY.md`): the tokenization a scenario got in 1.0 must
/// still be what it gets in every later 1.x, so the historical rule stays the
/// default and the better rule is opt-in per `spawn`.
///
/// Deserialized from the YAML keyword `whitespace` or `posix`, matched
/// case-insensitively after trimming, the same normalization `key` and
/// `source` apply.
///
/// An unrecognized keyword is **not** a hard error. It deserializes to
/// [`SplitMode::Unknown`], which tokenizes as the default and warns on stderr.
/// That looks like the wrong call — a typo'd `split: pisox` silently gives the
/// author the behavior they were opting out of, which is what strictness would
/// prevent — but the alternative breaks the v1 contract, and the contract wins:
///
/// `split` did not exist before this release, so every pitty already in the
/// field parses `split: <anything>` by ignoring it entirely (nested
/// `deny_unknown_fields` is deliberately off; see `COMPATIBILITY.md`). A
/// scenario with an unrecognized value therefore *runs* on 1.2.2 and every
/// earlier 1.x. Rejecting it here would tighten validation so a previously
/// valid scenario becomes an error — the exact clause this whole field exists
/// to respect. The warning is how the typo stays visible without becoming
/// fatal.
///
/// Note this is narrower than "pitty is lenient about keyword values": `key`
/// and `source` reject unknown values outright, and 1.2.2 did too. The
/// difference is that those fields *existed*, so an old runner's verdict on a
/// bad value is already an error; `split` did not, so an old runner's verdict
/// is "accepted and ignored", and a new runner may not be stricter than that.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum SplitMode {
    /// Split on any run of whitespace, exactly as `str::split_whitespace` does.
    ///
    /// Quotes and backslashes carry no meaning: they are ordinary bytes inside
    /// a word and reach the child verbatim. This is what pitty has always done
    /// and therefore what the v1 contract pins as the default.
    #[default]
    Whitespace,
    /// Split with POSIX shell word rules: `'...'`, `"..."`, and backslash
    /// escapes group, and the quote characters are consumed rather than passed
    /// to the child. Opt in per `spawn` with `split: posix`.
    Posix,
    /// A value this pitty does not recognize, described for the warning.
    ///
    /// Tokenizes as [`SplitMode::Whitespace`] and warns, because that is what
    /// every pitty released before `split` existed does with the same document
    /// (minus the warning).
    ///
    /// Holds a short *description* rather than the value itself: an unknown
    /// keyword is quoted verbatim (`"pisox"`), but a non-string is named by
    /// type (`a number`, `a mapping`), so a large list or map cannot flood
    /// stderr with a structure the author can already see in their scenario.
    Unknown(String),
}

/// The `split` keywords, in declaration order, for the schema contract gate.
pub const SPLIT_MODE_NAMES: &[&str] = &["whitespace", "posix"];

/// The resolved tokenization rule: [`SplitMode`] with the unknown case already
/// collapsed onto the default. Exists so `split_command` has no unreachable arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EffectiveSplit {
    Whitespace,
    Posix,
}

impl<'de> serde::Deserialize<'de> for SplitMode {
    /// Accept **any** YAML value at `split`, recognizing only the two keywords.
    ///
    /// Why not `#[serde(from = "String")]`: that makes `split` a string-typed
    /// field, so `split: 42` fails to deserialize. Inside the untagged
    /// `SpawnSpecRaw` that failure rejects the whole `spawn` map, and the
    /// author sees "data did not match any variant" without `split` being
    /// mentioned at all. More importantly it is the same contract break as a
    /// rejected keyword, one type away: a pitty predating this field ignores
    /// `split: <any YAML value>` and runs the scenario, so this build may not
    /// be stricter about the *type* than about the *value*.
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = serde_norway::Value::deserialize(deserializer)?;
        Ok(SplitMode::from_value(&value))
    }
}

impl SplitMode {
    /// Interpret an arbitrary YAML value as a split mode.
    ///
    /// A recognized keyword (trimmed, case-folded) selects its rule; everything
    /// else — an unknown keyword, or any non-string type — becomes
    /// [`SplitMode::Unknown`] carrying a description for the warning.
    fn from_value(value: &serde_norway::Value) -> Self {
        let Some(text) = value.as_str() else {
            return SplitMode::Unknown(describe_yaml_type(value));
        };
        match text.trim().to_ascii_lowercase().as_str() {
            "whitespace" => SplitMode::Whitespace,
            "posix" => SplitMode::Posix,
            _ => SplitMode::Unknown(format!("{text:?}")),
        }
    }
}

/// Name a YAML value by type, for a warning that must not echo its content.
fn describe_yaml_type(value: &serde_norway::Value) -> String {
    use serde_norway::Value;
    match value {
        Value::Null => "a null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::Sequence(_) => "a list",
        Value::Mapping(_) => "a mapping",
        Value::Tagged(_) => "a tagged value",
        Value::String(_) => "a string",
    }
    .to_string()
}

impl SplitMode {
    /// The rule this mode actually tokenizes under.
    ///
    /// [`SplitMode::Unknown`] resolves to the default, matching what a pitty
    /// that predates the field does with the same scenario. The return type
    /// cannot represent "unknown", so the tokenizer below is total by
    /// construction rather than by a fallback arm that could silently absorb a
    /// future variant.
    fn effective(&self) -> EffectiveSplit {
        match self {
            SplitMode::Posix => EffectiveSplit::Posix,
            // Whitespace, and anything this build does not recognize.
            SplitMode::Whitespace | SplitMode::Unknown(_) => EffectiveSplit::Whitespace,
        }
    }

    /// The stderr warning this mode owes the author, if any.
    ///
    /// An unrecognized keyword is not fatal (see the type docs), so the warning
    /// is the only signal a typo gets. It names the spelling and says which
    /// rule was actually used, so `split: pisox` cannot look like it worked.
    pub fn warning(&self) -> Option<String> {
        let SplitMode::Unknown(keyword) = self else {
            return None;
        };
        Some(format!(
            "warning: unrecognized spawn split mode {keyword} (expected one of: {}); \
             using the default `whitespace` rule. Older pitty releases ignore \
             this field entirely, so it cannot be an error.",
            SPLIT_MODE_NAMES.join(", ")
        ))
    }
}

/// Split a `spawn` command line into `[program, args...]` under `mode`.
///
/// How, for [`SplitMode::Whitespace`]: `str::split_whitespace`, so every run of
/// whitespace separates words and nothing else is interpreted. `echo 'hello
/// world'` yields three words and the child prints the quote characters back.
/// That is a footgun, but it is the behavior scenarios recorded snapshots
/// against, so it stays the default (see [`SplitMode`]).
///
/// How, for [`SplitMode::Posix`]: delegates to `shell_words::split`, which
/// honors `'...'` (literal), `"..."` (escapes recognized inside), and backslash
/// escapes, and rejects an unterminated quote. Quotes and escapes are *grouping
/// syntax* and are consumed, so `echo 'hello world'` yields two words, the
/// second containing a space, rather than leaking `'` into the child's output.
///
/// Why not hand-roll the POSIX splitter: the corner cases (quote nesting,
/// escapes inside versus outside double quotes, a trailing backslash, an
/// unmatched quote) are exactly the ones a naive parser gets subtly wrong, and
/// a wrong split silently execs a *different* program than the author wrote —
/// the failure mode the opt-in exists to remove. `shell-words` is the
/// long-established implementation of this rule.
///
/// Why not interpose a real shell (`sh -c <command>`) instead: that would make
/// every scenario inherit the host shell's globbing, redirection, variable
/// expansion, and job control, so the program under test would no longer be the
/// direct PTY child. Teardown depends on that directness — the child is the
/// session leader whose pid names the group `shutdown` sweeps — and `cmd.exe`
/// has neither the same quoting rules nor the same semantics, so the scenario
/// format would mean two different things on two platforms. Splitting in-process
/// and exec'ing the program ourselves keeps one rule everywhere.
///
/// Why POSIX rules on Windows too, once opted in: `CommandBuilder` takes an argv
/// vector on every platform (portable-pty performs the Windows argv ->
/// command-line re-quoting itself), so the tokenizer's job is to produce argv,
/// not a `cmd.exe` command string. Applying `cmd.exe` quoting on Windows would
/// make the same scenario file split differently per runner and break the
/// cross-platform matrix promise. A Windows path with backslashes therefore
/// needs quoting like any other POSIX word: `spawn: {command: "'C:\\Program
/// Files\\app.exe' --flag", split: posix}`. Documented in `SCHEMA.md` and the
/// README.
///
/// A tokenization error is a [`PittyError::Process`] (exit code 3): the
/// scenario named a command line the harness cannot turn into a process, and
/// falling back to the whitespace split would re-introduce the silent
/// mis-exec the author opted out of. Only [`SplitMode::Posix`] can fail;
/// whitespace splitting has no invalid input.
pub fn split_command(command: &str, mode: &SplitMode) -> Result<Vec<String>, PittyError> {
    match mode.effective() {
        EffectiveSplit::Whitespace => Ok(command.split_whitespace().map(String::from).collect()),
        EffectiveSplit::Posix => shell_words::split(command).map_err(|e| {
            PittyError::Process(format!("cannot parse spawn command `{command}`: {e}"))
        }),
    }
}

/// Put the freshly spawned child into the job, or reap it and fail.
///
/// Takes and returns the child by value so a failed assignment cannot leave
/// the caller holding a process nothing will ever kill.
#[cfg(windows)]
fn assign_to_job(
    job: &job::Job,
    child: Box<dyn Child + Send + Sync>,
) -> Result<Box<dyn Child + Send + Sync>, PittyError> {
    let assigned = child
        .as_raw_handle()
        .ok_or_else(|| "spawned child has no process handle".to_string())
        .and_then(|handle| {
            job.assign(handle)
                .map_err(|e| format!("job assignment failed: {e}"))
        });
    match assigned {
        Ok(()) => Ok(child),
        Err(msg) => Err(reap_orphan(child, msg)),
    }
}

/// Kill a child that `spawn` cannot hand back and fold every cleanup problem
/// into the returned error, so a child that outlived the attempt is visible.
fn reap_orphan(mut child: Box<dyn Child + Send + Sync>, cause: String) -> PittyError {
    let mut problems = vec![cause];
    if let Err(e) = child.kill() {
        problems.push(format!("cleanup kill failed: {e}"));
    }
    problems.extend(await_reap(&mut *child, SHUTDOWN_GRACE));
    PittyError::Process(problems.join("; "))
}

/// Wait up to `grace` for a killed child to be reaped; the problems found.
///
/// Why poll instead of `wait()`: portable-pty's `wait` is unbounded, and on
/// Windows `TerminateProcess` only *requests* termination — a child parked in
/// a console read can take a long time to actually die. A bounded poll keeps
/// a stuck child from stalling the runner.
fn await_reap(child: &mut (dyn Child + Send + Sync), grace: Duration) -> Option<String> {
    match poll_exit_until(child, Instant::now() + grace, try_child_exit) {
        Ok(Some(_)) => None,
        Ok(None) => Some(format!("child still running {grace:?} after kill")),
        Err(e) => Some(e.message().to_string()),
    }
}

/// Poll the child once; its exit code if it has exited.
fn try_child_exit(child: &mut (dyn Child + Send + Sync)) -> Result<Option<i32>, PittyError> {
    match child.try_wait() {
        Ok(Some(status)) => Ok(Some(status.exit_code() as i32)),
        Ok(None) => Ok(None),
        Err(e) => Err(PittyError::Process(format!("try_wait failed: {e}"))),
    }
}

/// Poll `child` for exit until it exits or `deadline` elapses.
///
/// Free function rather than a method so `spawn`'s failure path can reap a
/// child it has not wrapped in a session yet.
/// Poll cadence for exit observation: short enough to observe a fresh exit
/// promptly, long enough to avoid a busy loop. PTY teardown is on the order of
/// tens of milliseconds, so 10ms keeps observation tight without spinning.
const EXIT_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// The child's exit status if it has exited, observed *without* reaping it.
///
/// Why not `try_wait` (which `try_child_exit` wraps): on Unix that is
/// `waitpid`, and reaping releases the pid as soon as the process group is
/// empty. `shutdown` sends `SIGKILL` to the group named by that pid, so a
/// reaped leader would open a window in which the number belongs to a
/// stranger. Peeking with `waitid(WNOWAIT)` leaves the zombie in place, which
/// pins the pid until `shutdown` has swept the group and only then reaps.
/// Windows process handles never recycle underneath us, so `try_wait` is fine
/// there. Signal deaths map to exit code 1, matching portable-pty's own
/// `ExitStatus` conversion so the report is identical either way.
fn observe_exit(child: &mut (dyn Child + Send + Sync)) -> Result<Option<i32>, PittyError> {
    #[cfg(unix)]
    {
        let pid = child
            .process_id()
            .ok_or_else(|| PittyError::Process("child has no pid".to_string()))?;
        // SAFETY: `info` is zero-initialised and sized for `siginfo_t`; with
        // WNOHANG and no state change the kernel leaves `si_pid` at 0. WNOWAIT
        // keeps the child reapable afterwards, which `shutdown` relies on.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let rc = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if rc == -1 {
            let e = std::io::Error::last_os_error();
            // ECHILD: the child has already been reaped (only `shutdown` does
            // that), so the kernel no longer knows it; the status is cached in
            // the std `Child` underneath portable-pty, which `try_wait` serves.
            if e.raw_os_error() == Some(libc::ECHILD) {
                return try_child_exit(child);
            }
            return Err(PittyError::Process(format!("waitid failed: {e}")));
        }
        let (reported_pid, code, status) = siginfo_child_fields(&info);
        if reported_pid == 0 {
            return Ok(None);
        }
        // Anything but a normal exit is a signal death (killed/dumped),
        // reported as 1 like portable-pty does.
        Ok(Some(if code == libc::CLD_EXITED { status } else { 1 }))
    }
    #[cfg(not(unix))]
    {
        try_child_exit(child)
    }
}

/// `(si_pid, si_code, si_status)` — the libc crate exposes these as accessor
/// methods on Linux/Android (the struct is a union there) and as plain fields
/// on the BSD family including macOS.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn siginfo_child_fields(info: &libc::siginfo_t) -> (libc::pid_t, libc::c_int, libc::c_int) {
    // SAFETY: the accessors read the `_sigchld` union member, which is the
    // populated one for a WEXITED report.
    unsafe { (info.si_pid(), info.si_code, info.si_status()) }
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
fn siginfo_child_fields(info: &libc::siginfo_t) -> (libc::pid_t, libc::c_int, libc::c_int) {
    (info.si_pid, info.si_code, info.si_status)
}

/// Poll `probe` until it reports an exit or `deadline` elapses.
///
/// `probe` is [`observe_exit`] for observation (non-reaping) and
/// [`try_child_exit`] for the final reap in teardown.
fn poll_exit_until(
    child: &mut (dyn Child + Send + Sync),
    deadline: Instant,
    probe: fn(&mut (dyn Child + Send + Sync)) -> Result<Option<i32>, PittyError>,
) -> Result<Option<i32>, PittyError> {
    loop {
        if let Some(code) = probe(child)? {
            return Ok(Some(code));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        // Never overshoot the deadline: cap the sleep at the remaining time
        // so the loop's worst-case overrun is one probe.
        let remaining = deadline.saturating_duration_since(Instant::now());
        std::thread::sleep(EXIT_POLL_INTERVAL.min(remaining));
    }
}

impl Drop for PtySession {
    fn drop(&mut self) {
        // Best-effort cleanup; Drop cannot propagate errors.
        let _ = self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_split_mode_is_the_legacy_whitespace_rule() {
        // The v1 compatibility contract forbids changing the meaning of an
        // existing field, so a `spawn` that does not opt in must tokenize
        // exactly as pitty 1.2.2 did. Verified byte-for-byte against the 1.2.2
        // binary; see the CHANGELOG entry for why this is the default.
        assert_eq!(SplitMode::default(), SplitMode::Whitespace);
    }

    #[test]
    fn whitespace_mode_leaves_quotes_and_escapes_in_the_words() {
        // The pre-#34 behavior, pinned: quotes are ordinary bytes, so a quoted
        // argument is torn apart and the quote characters reach the child. A
        // scenario that recorded a snapshot of that output must keep passing.
        assert_eq!(
            split_command("echo 'hello world'", &SplitMode::Whitespace).unwrap(),
            vec!["echo", "'hello", "world'"]
        );
        assert_eq!(
            split_command("echo \"a b\"", &SplitMode::Whitespace).unwrap(),
            vec!["echo", "\"a", "b\""]
        );
        assert_eq!(
            split_command(r"echo hello\ world", &SplitMode::Whitespace).unwrap(),
            vec!["echo", r"hello\", "world"]
        );
    }

    #[test]
    fn whitespace_mode_never_fails_on_an_unterminated_quote() {
        // Whitespace splitting has no invalid input: the command line that
        // POSIX mode rejects must still tokenize (into mangled words) under the
        // default, because rejecting it would tighten validation on a scenario
        // that is valid under 1.0.
        assert_eq!(
            split_command("echo 'unterminated", &SplitMode::Whitespace).unwrap(),
            vec!["echo", "'unterminated"]
        );
    }

    #[test]
    fn quoted_words_group_and_the_quotes_are_consumed() {
        // Guarantees the fix for issue #34 case 1 at the tokenizer level, now
        // behind `split: posix`: a quoted argument becomes ONE word whose text
        // has no quote characters, so the child cannot print the quotes back
        // out as literal bytes.
        assert_eq!(
            split_command("echo 'hello world'", &SplitMode::Posix).unwrap(),
            vec!["echo", "hello world"]
        );
        assert_eq!(
            split_command("echo \"hello world\"", &SplitMode::Posix).unwrap(),
            vec!["echo", "hello world"]
        );
        assert_eq!(
            split_command(r"echo hello\ world", &SplitMode::Posix).unwrap(),
            vec!["echo", "hello world"]
        );
    }

    #[test]
    fn a_shell_one_liner_keeps_its_script_in_a_single_argument() {
        // Guarantees the fix for issue #34 case 2 at the tokenizer level under
        // `split: posix`: the program text handed to `sh -c` must stay one argv
        // entry, otherwise `sh` runs `'exit` and reports an unrelated exit code.
        assert_eq!(
            split_command("sh -c 'exit 3'", &SplitMode::Posix).unwrap(),
            vec!["sh", "-c", "exit 3"]
        );
    }

    #[test]
    fn unquoted_words_split_on_any_run_of_whitespace_in_both_modes() {
        // The common case — no quotes, no backslashes — must tokenize
        // identically under both rules, including repeated and mixed
        // whitespace. This is what makes the opt-in safe to add to an existing
        // scenario whose command happens to be quote-free.
        for mode in [SplitMode::Whitespace, SplitMode::Posix] {
            assert_eq!(
                split_command("cargo  test\t--all", &mode).unwrap(),
                vec!["cargo", "test", "--all"],
                "mode {mode:?}"
            );
        }
    }

    #[test]
    fn a_blank_command_yields_no_words_in_both_modes() {
        // `spawn` turns the empty word list into its "empty spawn command"
        // process error; neither splitter may invent a program.
        for mode in [SplitMode::Whitespace, SplitMode::Posix] {
            assert!(
                split_command("", &mode).unwrap().is_empty(),
                "mode {mode:?}"
            );
            assert!(
                split_command("   ", &mode).unwrap().is_empty(),
                "mode {mode:?}"
            );
        }
    }

    #[test]
    fn an_unmatched_quote_under_posix_is_a_process_error_not_a_panic_or_a_fallback() {
        // Under the opt-in, a command line that cannot be tokenized must fail
        // loudly as exit code 3 and name the offending command — never panic,
        // and never fall back to the whitespace split, which would silently
        // exec the wrong argv the author opted out of.
        let err = split_command("echo 'unterminated", &SplitMode::Posix).unwrap_err();
        assert_eq!(err.exit_code(), 3);
        assert!(
            err.message().contains("echo 'unterminated"),
            "the error must quote the offending command line: {}",
            err.message()
        );
    }

    /// Deserialize a `split` value from the YAML that would follow the key.
    fn split_mode(yaml: &str) -> SplitMode {
        serde_norway::from_str(yaml).expect("any YAML value must deserialize as a split mode")
    }

    #[test]
    fn split_mode_keywords_are_trimmed_and_case_folded() {
        // Normalized like `key` and `source`.
        assert_eq!(split_mode("\"  POSIX \""), SplitMode::Posix);
        assert_eq!(split_mode("Whitespace"), SplitMode::Whitespace);
        assert_eq!(split_mode("posix"), SplitMode::Posix);
    }

    #[test]
    fn an_unknown_split_keyword_falls_back_to_the_default_and_warns() {
        // The v1 contract, not a preference: `split` did not exist before this
        // release, so `split: custom` is *accepted and ignored* by 1.2.2 and
        // every earlier 1.x. Rejecting it here would tighten validation so a
        // previously valid scenario becomes an error. Verified against the
        // 1.2.2 binary, which runs this document to "status": "passed".
        let mode = split_mode("custom");
        assert_eq!(mode, SplitMode::Unknown("\"custom\"".to_string()));
        // It must tokenize as the default — the same argv 1.2.2 produces.
        assert_eq!(
            split_command("echo 'hello world'", &mode).unwrap(),
            vec!["echo", "'hello", "world'"]
        );
        // And it must still be visible, since it is not an error.
        let warning = mode.warning().expect("an unknown keyword must warn");
        assert!(
            warning.contains("custom") && warning.contains("whitespace"),
            "the warning must name the typo and the rule used: {warning}"
        );
        // A recognized keyword owes no warning.
        assert!(SplitMode::Posix.warning().is_none());
        assert!(SplitMode::Whitespace.warning().is_none());
    }

    #[test]
    fn a_non_string_split_value_falls_back_to_the_default_and_warns_by_type() {
        // The same contract clause one type away. `split` postdates 1.0, so
        // 1.2.2 ignores the key whatever its *type* — verified against the
        // binary for an integer, boolean, null, float, list and mapping, all of
        // which run to "status": "passed" there. Requiring a string would
        // reject the whole `spawn` map (the untagged enum fails to match) on a
        // scenario every earlier 1.x runs.
        for (yaml, expected_type) in [
            ("42", "a number"),
            ("1.5", "a number"),
            ("true", "a boolean"),
            ("null", "a null"),
            ("[a, b]", "a list"),
            ("{mode: posix}", "a mapping"),
        ] {
            let mode = split_mode(yaml);
            assert_eq!(
                mode,
                SplitMode::Unknown(expected_type.to_string()),
                "`split: {yaml}` must be carried as an unknown value"
            );
            // It tokenizes as the default, exactly as 1.2.2 does.
            assert_eq!(
                split_command("echo 'hello world'", &mode).unwrap(),
                vec!["echo", "'hello", "world'"],
                "`split: {yaml}` must tokenize with the default rule"
            );
            // The warning names the type rather than echoing the structure, so
            // a large list or map cannot flood stderr.
            let warning = mode.warning().expect("a non-string value must warn");
            assert!(
                warning.contains(expected_type) && warning.contains("whitespace"),
                "the warning must name the type and the rule used: {warning}"
            );
        }
    }
}
