//! Run reports (serialize-only) and scenario log writing.
//!
//! A [`Report`] summarizes one scenario run for JSON output. Logs are written
//! under `logs/` (`0600` on Unix), and every byte that touches the log file is
//! run through secret masking first. The file name is derived from a
//! [`LogIdentity`] — the scenario file, its `name:`, and (in matrix mode) the
//! cell's coordinates — so two runs that are distinct at the source level never
//! share one log path.
//!
//! Each log also records a digest of the identity that wrote it (its *claim*),
//! so ownership survives the process that created the file: re-running a suite
//! into an existing `logs/` reuses each scenario's own log rather than piling up
//! numbered copies, while a genuinely different identity that happens to reduce
//! to the same file stem is still given a file of its own.

use std::collections::{BTreeMap, BTreeSet};
#[cfg(unix)]
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

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

/// What distinguishes one scenario run's log from every other run's.
///
/// The log file name is built from all three parts, because each addresses a
/// different way two runs can otherwise land on the same path:
///
/// - `file_stem` — two scenario *files* in one directory run may carry the same
///   `name:`, which nothing enforces as unique. The file stem is unique by
///   construction within a directory (a filesystem cannot hold two `a.yaml`).
/// - `scenario_name` — kept in the name so the file stays recognizable to a
///   human scanning `logs/`, which is the whole point of the artifact.
/// - `cell_coords` — one matrix scenario runs many times from a single file
///   under a single `name:`; only the per-cell coordinates tell those runs apart.
///
/// Why not drop `scenario_name` now that `file_stem` disambiguates: a run
/// launched through the library API (`run_scenario` on a parsed `Scenario`) has
/// no file behind it, so the name is the only identity available there.
#[derive(Debug, Clone, Default)]
pub struct LogIdentity {
    /// The scenario file's stem (`a` for `a.yaml`), when the run came from a file.
    ///
    /// `None` for a run driven straight from a parsed `Scenario`, where the name
    /// alone identifies it.
    pub file_stem: Option<String>,
    /// The scenario file's extension (`yaml` for `a.yaml`), when it came from a
    /// file.
    ///
    /// Part of the identity because `pitty run <dir>` accepts both `.yaml` and
    /// `.yml`, so `a.yaml` and `a.yml` are distinct scenarios that the stem alone
    /// would conflate.
    pub file_extension: Option<String>,
    /// The scenario's `name:` field.
    pub scenario_name: String,
    /// The matrix cell's per-axis values in axis order, empty for a plain run.
    pub cell_coords: BTreeMap<String, String>,
}

impl LogIdentity {
    /// Build the identity for a plain (non-matrix) run of `scenario_name`.
    pub fn new(scenario_name: &str) -> Self {
        LogIdentity {
            file_stem: None,
            file_extension: None,
            scenario_name: scenario_name.to_string(),
            cell_coords: BTreeMap::new(),
        }
    }

    /// Return a copy pinned to the scenario file at `path`.
    ///
    /// A path with no usable stem (a bare `..`, or non-UTF-8) leaves `file_stem`
    /// `None` rather than inventing one, so the name-only form is the fallback.
    ///
    /// The extension is recorded alongside the stem because a directory run
    /// accepts both `.yaml` and `.yml`: `a.yaml` and `a.yml` are two different
    /// scenario files that share one stem, so the stem alone cannot tell their
    /// logs apart. It is kept separate from the stem (rather than folded in) so
    /// [`LogIdentity::stem`] can spend it only where it is actually needed.
    pub fn with_file(mut self, path: &Path) -> Self {
        self.file_stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .map(str::to_string);
        self.file_extension = path
            .extension()
            .and_then(|s| s.to_str())
            .map(str::to_string);
        self
    }

    /// Return a copy pinned to one matrix cell's coordinates.
    pub fn with_cell(mut self, coords: BTreeMap<String, String>) -> Self {
        self.cell_coords = coords;
        self
    }

    /// Return a copy carrying `name` as the scenario name.
    pub fn with_name(mut self, name: &str) -> Self {
        self.scenario_name = name.to_string();
        self
    }

    /// The file stem this identity writes to, before collision resolution.
    ///
    /// Joins the parts with `.` after sanitizing each one *independently*.
    /// Sanitizing per part rather than sanitizing the joined string is what keeps
    /// the separators meaningful: a `.` produced by sanitizing a part would
    /// otherwise be indistinguishable from a separator, so `a.b` as a name and
    /// `a` + `b` as two parts would collapse onto the same stem.
    fn stem(&self, secrets: &[String]) -> String {
        let mut parts: Vec<String> = Vec::new();
        // The `name:` component is deliberately NOT masked here: that is issue #43,
        // which predates this scheme and is excluded from this change's scope.
        let name = sanitize_stem(&self.scenario_name);
        if let Some(file_stem) = &self.file_stem {
            // Mask before sanitizing: the file stem and the axis values below are
            // identity material this scheme put into the filename, so a secret
            // appearing in either would otherwise be written to disk in plain
            // text by the directory listing alone.
            let mut file = sanitize_stem(&mask_secrets(file_stem, secrets));
            // Spend the extension only when it is what distinguishes this file.
            // `.yaml` is the canonical spelling and stays invisible; a file
            // written `.yml` carries its extension so `a.yaml` and `a.yml` —
            // which a directory run executes as two separate scenarios — cannot
            // land on one log. Why mark the non-canonical one rather than both:
            // tagging both would rename every existing log for no gain, and the
            // pair only needs to differ from each other, not to be self-describing.
            let is_canonical_extension = self
                .file_extension
                .as_deref()
                .is_none_or(|ext| ext.eq_ignore_ascii_case(CANONICAL_SCENARIO_EXTENSION));
            if !is_canonical_extension {
                if let Some(ext) = &self.file_extension {
                    file = format!("{file}-{}", sanitize_stem(&mask_secrets(ext, secrets)));
                }
            }
            // Skip the file part when it already equals the name. That is the
            // overwhelmingly common case (`echo-flow.yaml` declaring
            // `name: echo-flow`), where repeating it would only add noise — and
            // it keeps the documented `logs/<scenario>.log` name for scenarios
            // that follow the convention, so existing tooling still finds them.
            // The parts still disambiguate whenever they actually differ, which
            // is exactly when two files can collide on one name.
            if file != name {
                parts.push(file);
            }
        }
        parts.push(name);
        for (axis, value) in &self.cell_coords {
            parts.push(format!(
                "{}-{}",
                sanitize_stem(&mask_secrets(axis, secrets)),
                sanitize_stem(&mask_secrets(value, secrets))
            ));
        }
        // Masking is many-to-one: two cells differing only inside a secret both
        // reduce to `***`, which would put them on one filename and reintroduce
        // the very overwrite class (#31/#35/#36) this scheme exists to prevent.
        // When masking actually removed distinguishing text, append a short
        // discriminator derived from the *unmasked* identity so the files stay
        // distinct. It is only spent when a secret really is present — an
        // ordinary scenario's filename is untouched.
        //
        // It is **not** a secrecy barrier. The digest is unkeyed, so anyone
        // holding a directory listing can hash candidate secrets against it and
        // recover a low-entropy one. SECURITY.md states this rather than
        // implying a digest hides its input; the operative guidance is that a
        // scenario identity is public, so do not put a secret in one (#43).
        let mut stem = parts.join(".");
        if self.masking_changed_the_stem(secrets) {
            stem = format!("{stem}-{}", &self.name_discriminator()[..8]);
        }
        // Bound the joined result, not each part: the limit is on the file name
        // the filesystem sees, and a name can exceed it through many short parts
        // just as easily as through one long one.
        bound_stem(&stem)
    }

    /// Whether masking altered any component this scheme puts in the filename.
    ///
    /// Only the file stem, extension, and cell coordinates are consulted — the
    /// `name:` component is out of scope here (issue #43). When this is true the
    /// masked parts are many-to-one, so the stem needs a discriminator to stay
    /// unique; when false the filename is built from text no secret touched and
    /// keeps its ordinary spelling.
    fn masking_changed_the_stem(&self, secrets: &[String]) -> bool {
        let touched = |text: &str| mask_secrets(text, secrets) != text;
        let in_file = self.file_stem.as_deref().is_some_and(touched)
            || self.file_extension.as_deref().is_some_and(touched);
        in_file
            || self
                .cell_coords
                .iter()
                .any(|(axis, value)| touched(axis) || touched(value))
    }

    /// A key that distinguishes this identity from every other one, losslessly.
    ///
    /// Built from the raw (unsanitized) parts, unlike [`LogIdentity::stem`]:
    /// `my/test` and `my_test` must compare as different identities even though
    /// they reduce to the same stem, since telling them apart is exactly what the
    /// collision suffix exists for.
    ///
    /// Each part is **length-prefixed** rather than separated by a delimiter.
    /// A delimiter — whatever byte is chosen — can in principle occur inside a
    /// part, making `["a", "b"]` and `["a<sep>b"]` flatten to the same key; a
    /// length prefix cannot be forged by content, so distinct part lists always
    /// produce distinct keys.
    fn dedup_key(&self) -> String {
        self.key_from(|part| part.to_string())
    }

    /// The key's shape: length-prefixed parts, transformed by `f`.
    ///
    /// Each part is **length-prefixed** rather than separated by a delimiter.
    /// A delimiter — whatever byte is chosen — can in principle occur inside a
    /// part, making `["a", "b"]` and `["a<sep>b"]` flatten to the same key; a
    /// length prefix cannot be forged by content, so distinct part lists always
    /// produce distinct keys.
    fn key_from(&self, f: impl Fn(&str) -> String) -> String {
        let mut key = String::new();
        let mut push = |part: &str| {
            let part = f(part);
            key.push_str(&part.len().to_string());
            key.push(':');
            key.push_str(&part);
        };
        push(self.file_stem.as_deref().unwrap_or_default());
        push(self.file_extension.as_deref().unwrap_or_default());
        push(&self.scenario_name);
        for (axis, value) in &self.cell_coords {
            push(axis);
            push(value);
        }
        key
    }

    /// The value recorded in this run's claim sidecar, matched by a later run.
    ///
    /// This is the **masked** identity key, stored as text rather than digested.
    /// It has to satisfy four constraints at once, and every digest-based design
    /// failed at least one of them:
    ///
    /// - **Not an oracle.** A digest of the *raw* identity lets anyone holding it
    ///   hash candidate secrets until one matches, recovering a low-entropy value
    ///   like a PIN used as a matrix axis. Salting the digest does not help here:
    ///   the salt lives in `logs/` as a dotfile, so anyone who obtained the
    ///   sidecar (a CI artifact bundle includes dotfiles) obtained the salt too.
    ///   The masked key contains no secret at all — masking removed it before
    ///   anything was stored — so there is nothing to brute-force.
    /// - **Survives salt loss.** No salt is involved, so losing one cannot orphan
    ///   a log.
    /// - **Survives masking.** The sidecar is never passed through
    ///   [`mask_secrets`], and this value is *already* masked, so even a stray
    ///   pass over it would be a no-op rather than corruption.
    /// - **Distinguishes cells that differ only inside a secret.** It does not,
    ///   on its own — two such cells mask to the same text. That is what the
    ///   per-log random token beside it is for (see [`ClaimRecord`]): identity
    ///   selects the *candidates*, and the token tells same-masked siblings apart
    ///   by construction rather than by any property of the secret.
    ///
    /// Why store text rather than a digest of it: a digest buys nothing here (the
    /// input is already secret-free) and would only make a collision possible.
    fn claim_key(&self, secrets: &[String]) -> String {
        let key = self.key_from(|part| mask_secrets(part, secrets));
        // Bounded the same way an over-long filename stem is: keep a recognizable
        // head and let a digest of the whole carry the rest, so two long keys
        // sharing a prefix stay distinct. Without this the writer could emit a
        // record the reader refuses as oversize, and the scenario would create a
        // new numbered log on every run until it hit the suffix ceiling.
        bound_text(&key, MAX_CLAIM_KEY_BYTES)
    }

    /// The discriminator appended to a filename whose masking was lossy.
    ///
    /// Two matrix cells whose axis values both mask to `***` would otherwise land
    /// on one filename and overwrite each other, so the name needs something that
    /// still differs. This digest of the *unmasked* identity supplies it.
    ///
    /// **It is not a secret and does not protect one.** It is a plain FNV-1a
    /// digest, reproducible by anyone: given a filename and a guess at the
    /// identity behind it, the guess is directly checkable. A low-entropy secret
    /// used inside a scenario identity — a PIN as a matrix axis value — is
    /// therefore recoverable from a directory listing.
    ///
    /// An earlier design keyed this with a random per-directory salt to defeat
    /// that. The keying did not hold: FNV-1a's per-byte step is invertible over
    /// its 64-bit state, so two tags from one directory recover a salt-equivalent
    /// state in about 2^32 work — and a directory listing supplies two filenames
    /// by construction. The alternatives all cost more than the guarantee is
    /// worth here: `DefaultHasher` is SipHash but std does not promise stability
    /// across releases, so a toolchain upgrade would rename every discriminated
    /// log and orphan it; hand-rolling a MAC in a log-naming sink is worse than
    /// the problem; and a crypto dependency is a large ask for a threat model of
    /// "someone read `ls` output but holds none of the files".
    ///
    /// The honest posture is the one issue #43 already documents: **do not put a
    /// secret in a scenario identity.** A secret in `name:` reaches the filename
    /// unmasked regardless, so the guidance is unchanged by dropping the salt.
    fn name_discriminator(&self) -> String {
        fnv1a_hex(self.dedup_key().as_bytes())
    }
}

/// Resolve a base directory that may be the empty path to something openable.
///
/// `Path::new("s.yaml").parent()` is `Some("")`, not `None` — so the CLI's
/// `unwrap_or_else(|| Path::new("."))` never fires for a bare filename, and
/// `base_dir` arrives here empty. An empty path cannot be opened, which used to
/// silently disable the whole containment mechanism: the anchor capture failed,
/// the writer fell back to path-based `create_dir_all`, and a symlinked `logs/`
/// was followed. The empty path denotes the current directory, so say so.
fn normalize_base_dir(base_dir: &Path) -> &Path {
    if base_dir.as_os_str().is_empty() {
        Path::new(".")
    } else {
        base_dir
    }
}

/// A descriptor for the directory logs are written under, captured before the
/// scenario's child process could interfere with it.
///
/// This is the anchor every log write descends from. It must be taken *before*
/// the first spawn: `safepath`'s guarantee comes entirely from the descriptor
/// naming a directory object that predates the untrusted process, so an anchor
/// obtained afterwards would prove nothing.
///
/// Anchored on the scenario file's directory, **not** the workspace cwd. Under
/// `workspace.temp: true` those are different trees, and `logs/` belongs to the
/// former; reusing the workspace descriptor would silently relocate every log.
///
/// `None` (or the non-Unix form) falls back to path-based writes, which is the
/// same position `assert::snapshot` takes on Windows.
#[derive(Debug, Default)]
pub struct LogAnchor {
    #[cfg(unix)]
    root_fd: Option<std::sync::Arc<std::os::fd::OwnedFd>>,
}

impl LogAnchor {
    /// Capture the anchor for `base_dir`, if it can be opened.
    ///
    /// A directory that cannot be opened yields an anchor with no descriptor.
    /// That does **not** fall back to a path-based write — `open_log_dir` refuses
    /// instead, and the run warns on stderr and skips the log. Writing through an
    /// unprotected path while the docs promise containment would be the worse
    /// outcome; an absent log is visible, a silently unconfined one is not.
    pub fn capture(base_dir: &Path) -> Self {
        let base_dir = normalize_base_dir(base_dir);
        #[cfg(unix)]
        {
            LogAnchor {
                root_fd: crate::safepath::open_dir_fd(base_dir)
                    .ok()
                    .map(std::sync::Arc::new),
            }
        }
        #[cfg(not(unix))]
        {
            let _ = base_dir;
            LogAnchor {}
        }
    }
}

/// The resolved `logs/` directory a run writes into.
///
/// On Unix this owns a descriptor obtained by descending from the anchor, so
/// every probe and write below is `*at`-relative and cannot be redirected. The
/// path is kept alongside purely for error messages and for the non-Unix arm.
struct LogDir {
    /// Only the non-Unix arm writes by path; on Unix every operation is
    /// descriptor-relative, so this is kept solely for error messages there.
    #[cfg_attr(unix, allow(dead_code))]
    path: PathBuf,
    /// Always present on Unix: [`open_log_dir`] refuses rather than returning a
    /// handle without one, so no code path below can silently fall back to a
    /// symlink-following path write.
    #[cfg(unix)]
    fd: crate::safepath::Fd,
}

impl LogDir {
    /// Open `name` for reading, refusing a symlink.
    ///
    /// `None` when the entry is absent, is a link, or cannot be read — every one
    /// of which the caller treats as "no claim here".
    #[cfg(unix)]
    fn open_read(&self, name: &str) -> Option<std::fs::File> {
        use std::os::fd::FromRawFd;
        let fd =
            crate::safepath::openat_file_read_no_follow(self.fd.as_raw(), OsStr::new(name)).ok()?;
        // SAFETY: a freshly opened, owned descriptor nothing else holds.
        Some(unsafe { std::fs::File::from_raw_fd(fd.into_raw()) })
    }

    #[cfg(not(unix))]
    fn open_read(&self, name: &str) -> Option<std::fs::File> {
        std::fs::File::open(self.path.join(name)).ok()
    }

    /// Whether `name` has no entry at all, without following symlinks.
    ///
    /// `Path::exists()` follows links, so a *dangling* symlink would read as
    /// absent and the writer would then create the link's target, outside
    /// `logs/` entirely. `fstatat(AT_SYMLINK_NOFOLLOW)` — and
    /// `symlink_metadata` on the fallback path — report on the link itself, so a
    /// planted link counts as occupied and is suffixed past.
    #[cfg(unix)]
    fn is_absent(&self, name: &str) -> bool {
        !entry_exists_at(self.fd.as_raw(), name)
    }

    #[cfg(not(unix))]
    fn is_absent(&self, name: &str) -> bool {
        std::fs::symlink_metadata(self.path.join(name)).is_err()
    }

    /// Write `bytes` to `name`, private before any content reaches the file.
    #[cfg(unix)]
    fn write_private(&self, name: &str, bytes: &[u8]) -> std::io::Result<()> {
        use std::io::Write;
        use std::os::fd::FromRawFd;

        // O_NOFOLLOW at the final component closes the race where a link is
        // planted between the probe above and this open.
        let fd = crate::safepath::openat_file_write_no_follow(
            self.fd.as_raw(),
            OsStr::new(name),
            0o600,
        )?;
        // SAFETY: a freshly opened, owned descriptor nothing else holds.
        let mut file = unsafe { std::fs::File::from_raw_fd(fd.into_raw()) };
        // Mode first: `0o600` on the open covers only *creation*, so a log that
        // already exists keeps whatever mode it had. fchmod on the descriptor
        // cannot be redirected by a concurrent swap.
        crate::safepath::fchmod_file(&file, 0o600)?;
        // Truncate only once the mode is restrictive, so an existing file is
        // never observable at its old mode holding new content.
        file.set_len(0)?;
        file.write_all(bytes)
    }

    /// Non-Unix: no `openat`, so the write is path-based and the containment
    /// guarantee does not hold, as documented in SECURITY.md.
    #[cfg(not(unix))]
    fn write_private(&self, name: &str, bytes: &[u8]) -> std::io::Result<()> {
        write_private_file(&self.path.join(name), bytes)
    }

    /// Acquire the log name `name` for `claim`, excluding other writers.
    ///
    /// Returns the held lock on success. The name is ours while that value is
    /// alive; dropping it releases the lock but leaves the durable record.
    ///
    /// Two different jobs are deliberately carried by one file, with different
    /// mechanisms for each:
    ///
    /// - **Durable ownership** is the sidecar's *content* — the tag survives the
    ///   process, so a later run recognizes its own log.
    /// - **Run-time exclusion** is an advisory `flock` on that sidecar. Matching
    ///   tags alone cannot exclude anyone: two processes running the *same*
    ///   scenario both read their own tag back and both concluded the name was
    ///   theirs, then both truncated and wrote the log, interleaving content. The
    ///   lock is what makes a second writer wait or move on.
    ///
    /// An existing sidecar whose tag differs belongs to another scenario, and one
    /// that is empty is the residue of an interrupted run (see
    /// [`LogDir::adopt_stale_sidecar`]); both are handled before the lock is
    /// taken.
    #[cfg(unix)]
    fn try_acquire(&self, name: &str, want: &ClaimRecord) -> Option<ClaimLock> {
        use std::io::Write;
        use std::os::fd::AsRawFd;

        let sidecar = claim_sidecar_name(name);
        let (mut file, created) = match openat_create_exclusive(self.fd.as_raw(), &sidecar) {
            Ok(Some(file)) => (file, true),
            Ok(None) => (self.adopt_existing_sidecar(name, want)?, false),
            Err(_) => return None,
        };

        if created {
            // `openat`'s mode argument is masked by the process umask, so a
            // creating run under `umask 0200` leaves the sidecar unwritable and
            // under `0400` unreadable — either way the next run cannot reuse it
            // and suffixes instead. fchmod on the descriptor we just created,
            // never on a re-resolved name.
            if crate::safepath::fchmod_file(&file, 0o600).is_err() {
                return None;
            }
        }

        // Non-blocking: a second writer of the same scenario moves to the next
        // candidate rather than stalling the run behind an unrelated process.
        // SAFETY: `file` is an open descriptor owned here.
        let locked = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0;
        if !locked {
            // Deliberately NOT removed here. Losing the lock means somebody else
            // holds it — the file may already be *their* inode, reached through
            // this name after our own creation lost a race. Unlinking it would
            // free the name for a third process to create a different inode under
            // while the holder still writes the log, and the two would interleave.
            //
            // A filesystem that rejects `flock` outright (ENOTSUP) therefore
            // leaves the empty sidecar behind; that is recovered by
            // `adopt_existing_sidecar`, which reclaims an unparseable record when
            // no log sits beside it.
            return None;
        }

        // Re-validate now that the lock is held. The reclaimability decision in
        // `adopt_existing_sidecar` was made *before* this lock existed, so it can
        // be stale: another process holding the lock truncates the sidecar to
        // rewrite it, a second process reads that momentary emptiness as
        // "malformed residue, reclaimable", stalls, and resumes after the first
        // has finished writing its claim and released — then overwrites a claim
        // and a log that are now complete and owned.
        //
        // Reading from the descriptor already held (not by name) makes this
        // observation refer to the same inode the lock protects, and nothing can
        // change it between here and the write below.
        if !created {
            // Re-validate now that the lock is held. The reclaimability decision
            // in `adopt_existing_sidecar` was made *before* this lock existed, so
            // it can be stale: another process holding the lock truncates the
            // sidecar to rewrite it, a second process reads that momentary
            // emptiness as "malformed residue, reclaimable", stalls, and resumes
            // after the first has finished writing its claim and released — then
            // overwrites a claim and a log that are now complete and owned.
            //
            // Re-read through a fresh `O_RDONLY` open rather than this
            // descriptor: the write handle is `O_WRONLY`, so it cannot be read
            // from. Opening by name again is safe *here specifically* because the
            // lock is already held on this inode, so nothing can rewrite the
            // record between this observation and the write below; and
            // `open_read` refuses symlinks, so the name cannot be redirected to
            // another file.
            // WHY THIS PRESENCE TEST IS NOT A CHECK-THEN-ACT RACE.
            //
            // It is read here and acted on much later — the log is truncated in
            // `write_private`, after this function returns. That gap is closed by
            // the lock rather than by re-checking, and the reasoning is worth
            // recording because the shape looks exactly like the TOCTOU bugs
            // removed elsewhere in this file.
            //
            // The lock above is held on the *sidecar*, and `claim_sidecar_name`
            // is a pure function of the log name. Any other pitty run that wants
            // to write this log must first acquire this name, which routes
            // through `acquire_for` -> `try_acquire` -> `flock` on that same
            // sidecar inode. `LOCK_EX | LOCK_NB` fails for it while we hold the
            // lock, so it takes the next candidate instead. The caller keeps the
            // returned `ClaimLock` alive across the log write, so the presence
            // test and the truncate sit inside one uninterrupted critical
            // section against every writer the lock can exclude.
            //
            // What it does not exclude is a *non-pitty* writer, since `flock` is
            // advisory — but that is not a window this ordering opens: such a
            // writer can equally clobber the log after the run finishes. It is
            // the advisory-lock limitation SECURITY.md already states, not a race
            // introduced here.
            let log_present = !self.is_absent(name);
            if !claim_is_reclaimable(&read_claim_state(self, name), want, log_present) {
                return None;
            }
        }

        // Write the record under the lock, so a racer that later acquires this
        // sidecar never observes a half-written one.
        let encoded = want.encode();
        let wrote = file.set_len(0).is_ok() && file.write_all(encoded.as_bytes()).is_ok();
        if !wrote {
            return None;
        }

        Some(ClaimLock { _file: file })
    }

    /// Reopen an existing sidecar this identity may take over, if any.
    ///
    /// `None` when the sidecar belongs to a different identity. One whose
    /// recorded *key* matches is ours — the token may differ, and the caller's
    /// token is adopted from it rather than minted fresh, so a re-run keeps the
    /// same log. An unparseable or empty one is residue from a run that died
    /// between creating the record and writing it; without this it would refuse
    /// the name permanently, and a disk that filled mid-run could poison all 1000
    /// candidates forever.
    #[cfg(unix)]
    fn adopt_existing_sidecar(&self, name: &str, want: &ClaimRecord) -> Option<std::fs::File> {
        use std::os::fd::FromRawFd;
        use std::os::unix::fs::MetadataExt;

        if !claim_is_reclaimable(&read_claim_state(self, name), want, !self.is_absent(name)) {
            return None;
        }

        let sidecar = claim_sidecar_name(name);
        // Open first, chmod second — never the reverse. Repairing the mode by
        // *name* before the open would chmod whatever that name currently
        // resolves to: a `.x.log.claim` hard-linked to a file outside `logs/`
        // reads as an unparseable record (multiple links), looks reclaimable, and
        // would have its external inode set to 0600 before the open was even
        // attempted. Acting only on a descriptor already obtained confines the
        // change to a file this directory handed us.
        //
        // The cost is that a sidecar an earlier pitty left at `0400` cannot be
        // reopened for writing; the run then treats it as unavailable and takes
        // the next candidate name, which is the safe direction.
        let fd = crate::safepath::openat_file_write_no_follow(
            self.fd.as_raw(),
            OsStr::new(&sidecar),
            0o600,
        )
        .ok()?;
        // SAFETY: a freshly opened, owned descriptor nothing else holds.
        let file = unsafe { std::fs::File::from_raw_fd(fd.into_raw()) };
        // Refuse anything with more than one link: a hard link means the inode is
        // reachable from outside `logs/`, so it is not a record this directory
        // owns and must not be written to or chmod-ed.
        if file.metadata().ok()?.nlink() != 1 {
            return None;
        }
        // `openat`'s mode applies only when the open *creates* the file, so a
        // pre-placed 0644 sidecar would otherwise have pitty write into a
        // world-readable file. On the descriptor, so no name is re-resolved.
        crate::safepath::fchmod_file(&file, 0o600).ok()?;
        Some(file)
    }

    /// A key identifying this directory as an *object*, for process-local maps.
    ///
    /// `(device, inode)` on Unix rather than the path: one physical `logs/`
    /// reached through a real path and through a symlink alias is the same
    /// directory, but two different `PathBuf`s. Keying by path gave the second
    /// view an empty "already bound" set, after which two identities with equal
    /// masked keys could adopt each other's log and overwrite it.
    #[cfg(unix)]
    fn identity_key(&self) -> String {
        use std::os::fd::FromRawFd;
        use std::os::unix::fs::MetadataExt;

        let stat = crate::safepath::dup_fd(self.fd.as_raw()).and_then(|fd| {
            // SAFETY: `fd` is a freshly duplicated descriptor nothing else holds;
            // `File` takes ownership and closes it.
            let file = unsafe { std::fs::File::from_raw_fd(fd.into_raw()) };
            file.metadata()
        });
        match stat {
            Ok(meta) => format!("{}:{}", meta.dev(), meta.ino()),
            // A directory whose identity cannot be read falls back to the path:
            // weaker, but it only costs the alias-sharing property, never
            // correctness of a single view.
            Err(_) => self.path.display().to_string(),
        }
    }

    /// Non-Unix: no stable inode identity available, so fall back to the path.
    #[cfg(not(unix))]
    fn identity_key(&self) -> String {
        self.path.display().to_string()
    }

    /// Non-Unix: no `openat`/`flock`, so ownership is recorded but **not**
    /// mutually exclusive.
    ///
    /// Windows lacks the primitives this handshake is built on, so two concurrent
    /// runs of one scenario can still interleave a log there. SECURITY.md states
    /// this limitation explicitly rather than implying a guarantee that does not
    /// hold on that platform.
    #[cfg(not(unix))]
    fn try_acquire(&self, name: &str, want: &ClaimRecord) -> Option<ClaimLock> {
        let existing = read_claim(self, name);
        let reusable = match &existing {
            Some(record) => record.key == want.key && record.token == want.token,
            None => self.is_absent(name),
        };
        if !reusable {
            return None;
        }
        self.write_private(&claim_sidecar_name(name), want.encode().as_bytes())
            .ok()
            .map(|()| ClaimLock {})
    }
}

/// A held claim on one log name.
///
/// On Unix this owns the locked sidecar descriptor; dropping it releases the
/// `flock` (the durable tag stays on disk). Elsewhere it is a marker, because the
/// platform provides no exclusion — see [`LogDir::try_acquire`].
struct ClaimLock {
    #[cfg(unix)]
    _file: std::fs::File,
}

/// `openat(dirfd, name, O_WRONLY|O_CREAT|O_EXCL|O_NOFOLLOW, 0600)`.
///
/// The exclusive create the claim handshake needs. `safepath` exposes a
/// non-exclusive writer (snapshots overwrite by design), and that file belongs to
/// another agent, so the one extra flag is spelled here rather than by changing
/// a shared module for a single caller.
///
/// `Ok(None)` means the name was already taken (`EEXIST`) — the caller moves to
/// the next candidate. Any other failure is a real error.
#[cfg(unix)]
fn openat_create_exclusive(
    dirfd: libc::c_int,
    name: &str,
) -> std::io::Result<Option<std::fs::File>> {
    use std::os::fd::FromRawFd;

    let c = std::ffi::CString::new(name)
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    // SAFETY: `dirfd` is an open directory descriptor and `c` a valid C string.
    // The variadic mode is meaningful because O_CREAT is set.
    let fd = unsafe {
        libc::openat(
            dirfd,
            c.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600 as libc::c_uint,
        )
    };
    if fd < 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EEXIST) {
            return Ok(None);
        }
        return Err(err);
    }
    // SAFETY: a freshly opened, owned descriptor nothing else holds.
    Ok(Some(unsafe { std::fs::File::from_raw_fd(fd) }))
}

/// `fstatat(dirfd, name, AT_SYMLINK_NOFOLLOW)`: does an entry exist here?
#[cfg(unix)]
fn entry_exists_at(dirfd: libc::c_int, name: &str) -> bool {
    let Ok(c) = std::ffi::CString::new(name) else {
        return false;
    };
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: `dirfd` is an open directory descriptor, `c` a valid C string, and
    // `st` a live out-parameter.
    let rc = unsafe {
        libc::fstatat(
            dirfd,
            c.as_ptr(),
            &mut st as *mut libc::stat,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    rc == 0
}

/// Open (creating if needed) the `logs/` directory under `anchor`.
#[cfg(unix)]
fn open_log_dir(anchor: &LogAnchor, log_dir: &Path) -> Result<LogDir, PittyError> {
    let Some(root) = anchor.root_fd.as_ref() else {
        // Refuse rather than degrade. Falling back to a path-based write here is
        // what let a symlinked `logs/` be followed: the containment mechanism
        // switched itself off and nothing said so, while SECURITY.md went on
        // promising it. A security control that quietly disables itself is worse
        // than one that was never claimed.
        //
        // Why skip the log rather than fail the run: logging is best-effort and a
        // diagnostics sink must not turn a passing scenario red (see the caller's
        // warning path). But "warn and skip" and "silently write somewhere
        // unprotected" are different things, and only the first is honest.
        return Err(PittyError::Process(format!(
            "cannot secure the logs directory under '{}': the directory could not \
             be opened before the scenario started, so writes to it cannot be \
             confined; skipping the log",
            log_dir.display()
        )));
    };

    use std::os::fd::AsRawFd;
    // `logs/` is a single component under the anchor, so this is the one-step
    // case of `walk_from` and is spelled with the same primitives directly:
    // `mkdirat` distinguishes "created" (which may be tightened to 0700) from
    // "already there" (which belongs to the user and is left alone), and
    // `openat_dir_no_follow` refuses a `logs` that is a symlink with ELOOP.
    // `walk_from` itself is not used because it requires a trailing *file*
    // component and would refuse the `.` that naming a directory would need.
    let created = match crate::safepath::mkdirat(root.as_raw_fd(), OsStr::new("logs"), 0o700) {
        Ok(()) => true,
        Err(e) if e.raw_os_error() == Some(libc::EEXIST) => false,
        Err(e) => {
            return Err(PittyError::Process(format!(
                "cannot create logs dir '{}': {e}",
                log_dir.display()
            )))
        }
    };
    // Open the directory, repairing the mode on the *descriptor* when this call
    // created it.
    //
    // `chmod_created_dir` performs the safe sequence: open first, then `fchmod`
    // the descriptor. That closes the race a name-based chmod left open — a child
    // that wins a race to `rmdir` the new directory and hard-link an external file
    // at that name would otherwise have had that inode chmod-ed. The descriptor
    // *is* the object, so `fchmod` cannot reach anything else, and `O_DIRECTORY`
    // rejects the hard-linked file with `ENOTDIR` before any mode change.
    //
    // It also resolves the umask ordering: `mkdirat`'s mode is masked, so at
    // `umask 0400` the directory lands at `0300` and an `O_RDONLY` open fails.
    // The helper asks for search-only access where the platform provides it,
    // which suffices to `fchmod` and to keep traversing (measured on macOS: an
    // `O_SEARCH` descriptor for a `0300` directory accepts `fchmod` and stays
    // usable). Where it does not exist (glibc Linux defines no `O_SEARCH`), the
    // open fails — correct, since a mode that cannot be set safely should not be
    // set at all.
    //
    // A failure here is *not* silently tolerated: with the open happening first,
    // nothing downstream would notice a directory left at the umask's mode. It
    // propagates, and `run_scenario` renders it as a stderr warning. That is the
    // right severity for this sink specifically — snapshot recording fails the
    // run on the same condition, but a log is a diagnostic, and an environment
    // problem must not turn a passing scenario red.
    let opened = if created {
        crate::safepath::chmod_created_dir(root.as_raw_fd(), OsStr::new("logs"), 0o700)
    } else {
        crate::safepath::openat_dir_no_follow(root.as_raw_fd(), OsStr::new("logs"))
    };
    let fd = opened.map_err(|e| {
        // O_NOFOLLOW reports a symlinked directory as ELOOP or ENOTDIR
        // depending on the platform, and neither word tells the user what is
        // actually wrong, so name the cause explicitly.
        let is_symlink_refusal = matches!(
            e.raw_os_error(),
            Some(code) if code == libc::ELOOP || code == libc::ENOTDIR
        );
        if is_symlink_refusal {
            return PittyError::Process(format!(
                "logs directory '{}' is a symlink; refusing to write logs through it",
                log_dir.display()
            ));
        }
        PittyError::Process(format!("cannot open logs dir '{}': {e}", log_dir.display()))
    })?;

    Ok(LogDir {
        path: log_dir.to_path_buf(),
        fd,
    })
}

/// Non-Unix: no `openat`, so fall back to a path-based create.
#[cfg(not(unix))]
fn open_log_dir(_anchor: &LogAnchor, log_dir: &Path) -> Result<LogDir, PittyError> {
    std::fs::create_dir_all(log_dir)
        .map_err(|e| PittyError::Process(format!("cannot create logs dir: {e}")))?;
    Ok(LogDir {
        path: log_dir.to_path_buf(),
    })
}

/// How many random bytes a per-log ownership token holds, rendered as hex.
///
/// Only has to make two same-masked siblings distinct within one directory, so
/// 8 bytes is generous; it is not a secret and authenticates nothing.
const CLAIM_TOKEN_BYTES: usize = 8;

/// Suffix of the sidecar file recording a log's [`LogIdentity::claim_tag`].
///
/// Ownership lives beside the log rather than inside it because the log body is
/// passed through [`mask_secrets`], which rewrites any substring matching a
/// registered secret. A tag written into the body is therefore corruptible by a
/// secret as ordinary as a single digit, and no encoding can prevent that — the
/// secret may be any substring of whatever is emitted. The sidecar is never
/// masked, so the tag it holds always round-trips.
///
/// It carries **no** run-derived text: only a digest of the scenario's identity
/// (file, name, cell coordinates), which the log's own filename already exposes.
/// So exempting it from masking cannot leak anything masking would have caught.
///
/// The leading `.` keeps sidecars out of a casual `ls` and out of glob patterns
/// like `logs/*.log` that CI uses to collect artifacts.
const CLAIM_SUFFIX: &str = ".claim";

/// Marks the end of a complete claim record.
///
/// Its absence is how a short read is detected; see [`ClaimRecord::encode`].
const CLAIM_TERMINATOR: &str = "#end\n";

/// The largest claim *key* pitty will write.
///
/// Nothing upstream caps `name:`, the axis count, or an axis value — the schema
/// imposes no length — so a key has no natural bound and the previous constant
/// was a guess that the writer could exceed. A key past this is truncated to a
/// prefix plus a digest of the whole, exactly as an over-long *filename* stem is:
/// the result still identifies the scenario (the digest makes distinct keys stay
/// distinct) and is now bounded by construction, so the reader's limit and the
/// writer's output cannot disagree.
const MAX_CLAIM_KEY_BYTES: usize = 8 * 1024;

/// The largest complete record, derived from the encoding rather than assumed.
///
/// [`ClaimRecord::encode`] emits, in order: the token (hex, `CLAIM_TOKEN_BYTES`
/// per byte doubled), `\n`, the key's decimal length, `\n`, the key itself,
/// `\n`, and the terminator. Summing those exactly — rather than padding a round
/// number — is what makes this a true upper bound: every field the writer can
/// emit is accounted for, so a record pitty wrote always fits and a file that
/// does not fit was not written by pitty.
const MAX_CLAIM_BYTES: usize = CLAIM_TOKEN_BYTES * 2      // token, hex-encoded
    + 1                                                    // newline
    + 20                                                    // usize decimal digits
    + 1                                                    // newline
    + MAX_CLAIM_KEY_BYTES                                   // key
    + 1                                                     // newline
    + CLAIM_TERMINATOR.len();

/// The sidecar name recording ownership of the log file `name`.
fn claim_sidecar_name(name: &str) -> String {
    format!(".{name}{CLAIM_SUFFIX}")
}

/// What a claim sidecar records: who owns a log, and which sibling it is.
///
/// Two fields, because ownership needs two different things:
///
/// - `key` — the **masked** identity ([`LogIdentity::claim_key`]). A later run
///   reproduces it from its own scenario, so this is what makes a log findable.
///   It carries no secret, so holding it reveals nothing.
/// - `token` — a random value minted once per log. Two matrix cells whose axis
///   values both mask to `***` share a `key`, so the key alone cannot tell them
///   apart; the token does, by construction rather than by any property of the
///   secret. It is never recomputed — a run learns its own token by reading the
///   sidecar it already owns, and mints a new one only when taking a fresh name.
#[derive(Debug, Clone, PartialEq)]
struct ClaimRecord {
    key: String,
    token: String,
}

impl ClaimRecord {
    /// Serialize as two lines: the token first (fixed width), then the key.
    ///
    /// The key goes last because it is the variable-length field and may itself
    /// contain newlines after masking; everything past the first line is the key.
    fn encode(&self) -> String {
        // Length-prefixed and terminated. The trailing marker is what makes a
        // short read *detectable*: without it a truncated record still parses,
        // and because the key is itself length-prefixed, a cut-off two-axis key
        // can look exactly like a complete one-axis key — letting one scenario
        // adopt another's log. With the terminator, a record that did not arrive
        // whole is rejected instead of mis-parsed.
        format!(
            "{}\n{}\n{}\n{CLAIM_TERMINATOR}",
            self.token,
            self.key.len(),
            self.key
        )
    }

    /// Parse a complete record, or `None` for anything else.
    ///
    /// Every field is validated against the others: the declared key length must
    /// match the key actually present, and the terminator must be there. A
    /// truncated, padded, or hand-written file fails all three checks rather than
    /// yielding a partial record that compares equal to somebody else's.
    fn decode(text: &str) -> Option<Self> {
        let rest = text.strip_suffix(CLAIM_TERMINATOR)?;
        let (token, rest) = rest.split_once('\n')?;
        let (declared_len, rest) = rest.split_once('\n')?;
        let key = rest.strip_suffix('\n')?;

        let token = token.trim();
        if token.is_empty() {
            return None;
        }
        // The declared length is the anti-truncation check: a cut-off key cannot
        // satisfy it, so it can never masquerade as a shorter complete one.
        if declared_len.trim().parse::<usize>().ok()? != key.len() {
            return None;
        }
        Some(ClaimRecord {
            key: key.to_string(),
            token: token.to_string(),
        })
    }
}

/// Whether a sidecar in this state may be taken over by `want`.
///
/// `log_present` is what separates the two malformed cases, and it is the whole
/// point of this signature:
///
/// - `Record` — ours only if it matches exactly; anything else belongs to another
///   identity.
/// - `Malformed` **with no log** — residue from a run interrupted between
///   creating or truncating the sidecar and writing it, with nothing behind it.
///   Plainly abandoned, so reclaiming it costs nobody anything.
/// - `Malformed` **with a log present** — refused. A malformed record carries no
///   ownership information, which is exactly why this case is unsafe: "an
///   interrupted run of *this* identity" and "another identity's log" are
///   indistinguishable from disk, so adopting it is a coin flip that destroys a
///   log when it lands wrong. An earlier revision reclaimed it, reasoning that
///   refusing would strand the log forever — but the two losses are not
///   symmetric. Stranding leaves the file and its contents intact and readable;
///   the run simply takes a suffixed name, and deleting the stale file restores
///   the preferred one. Overwriting truncates diagnostics that exist nowhere
///   else. A recoverable cost always beats an unrecoverable one, so the run
///   suffixes past.
/// - `Unreadable` — ownership unknown for a different reason (permissions, I/O,
///   a non-regular file). Never reclaimed, with or without a log: the same
///   argument applies and more strongly, since the bytes may well be a perfectly
///   valid record belonging to somebody else.
/// - `Absent` — nothing to take over; the caller's exclusive create handles that
///   path instead.
///
/// Gated to unix alongside its only callers (`acquire_for` and
/// `adopt_existing_sidecar`, both `#[cfg(unix)]`): the sidecar protocol it
/// arbitrates is descriptor- and mode-based, which the non-unix log path does
/// not implement. Ungated it is dead code on Windows, which `just lint`
/// escalates to an error.
#[cfg(unix)]
fn claim_is_reclaimable(state: &ClaimRead, want: &ClaimRecord, log_present: bool) -> bool {
    match state {
        ClaimRead::Record(existing) => existing.key == want.key && existing.token == want.token,
        ClaimRead::Malformed => !log_present,
        ClaimRead::Unreadable | ClaimRead::Absent => false,
    }
}

/// What reading a claim sidecar found.
///
/// Three states, not two. Folding them together was a defect: a *valid* sidecar
/// an older pitty left at `0200` cannot be opened `O_RDONLY`, and treating that
/// read failure as "malformed residue" let a colliding identity adopt it and
/// overwrite the live log behind it. "Cannot read it" must never be read as "it
/// is garbage".
#[derive(Debug)]
enum ClaimRead {
    /// No sidecar exists.
    Absent,
    /// Read successfully and parsed.
    Record(ClaimRecord),
    /// Read successfully but does not parse — residue from a run interrupted
    /// mid-write. Safe to reclaim: the bytes carry no ownership information, so
    /// there is nothing to contradict.
    Malformed,
    /// Could not be read at all (permissions, I/O error, a symlink). Ownership is
    /// *unknown*, so it must never be reclaimed.
    Unreadable,
}

/// Read the claim recorded for the log named `name`.
///
/// No masking is involved on either side: the sidecar is written unmasked and
/// read back verbatim, which is the whole point of keeping it out of the body.
fn read_claim_state(dir: &LogDir, name: &str) -> ClaimRead {
    use std::io::Read;

    let sidecar = claim_sidecar_name(name);
    if dir.is_absent(&sidecar) {
        return ClaimRead::Absent;
    }
    let Some(file) = dir.open_read(&sidecar) else {
        // Present but not openable: a mode we cannot read through, or a symlink
        // the opener refuses. Unknown, not empty.
        return ClaimRead::Unreadable;
    };

    // Read one byte past the maximum a real record can occupy: if that byte
    // arrives, the file is larger than anything pitty writes and is rejected
    // rather than silently truncated into a valid-looking partial record.
    let mut buf = Vec::with_capacity(MAX_CLAIM_BYTES + 1);
    if file
        .take(MAX_CLAIM_BYTES as u64 + 1)
        .read_to_end(&mut buf)
        .is_err()
    {
        return ClaimRead::Unreadable;
    }
    if buf.len() > MAX_CLAIM_BYTES {
        return ClaimRead::Malformed;
    }
    match ClaimRecord::decode(&String::from_utf8_lossy(&buf)) {
        Some(record) => ClaimRead::Record(record),
        None => ClaimRead::Malformed,
    }
}

/// The parsed record for `name`, or `None` for any other state.
///
/// Callers that only care whether a *valid* record is present; anything deciding
/// reclaimability must use [`read_claim_state`] so it can tell malformed residue
/// from a sidecar it merely cannot read.
fn read_claim(dir: &LogDir, name: &str) -> Option<ClaimRecord> {
    match read_claim_state(dir, name) {
        ClaimRead::Record(record) => Some(record),
        _ => None,
    }
}

/// Write the captured terminal output and assertion results to a log file under
/// `base_dir/logs/`, masking secrets (`0600` on Unix).
///
/// The file name comes from `identity` (see [`LogIdentity`]); when two identities
/// still sanitize to the same stem, a numeric suffix keeps them apart (see
/// [`resolve_log_path`]). The log directory is created relative to `base_dir`.
/// Failures here are non-fatal to the run's pass/fail verdict, so callers may
/// log-and-continue; we still return a `Result` so the CLI can warn.
pub fn write_log(
    base_dir: &Path,
    anchor: &LogAnchor,
    identity: &LogIdentity,
    output: &str,
    report: &Report,
    fault: Option<&str>,
    secrets: &[String],
) -> Result<(), PittyError> {
    let log_dir = normalize_base_dir(base_dir).join("logs");
    // Open `logs/` through `safepath`, descending from the pre-spawn anchor with
    // `openat(O_DIRECTORY | O_NOFOLLOW)`. That refuses a `logs/` that is a
    // symlink (ELOOP) and, because the handle names a directory *object*, no
    // later rename or swap can change which directory the writes below land in.
    // Creating it with `CreateDirs::Yes` also makes a directory pitty creates
    // `0700` rather than umask-default.
    let log_dir_fd = open_log_dir(anchor, &log_dir)?;

    // The lock is held until this function returns, so no concurrent run can
    // truncate the same file midway through the write below.
    let (log_path, _claim_lock) = resolve_log_path(&log_dir_fd, identity, secrets)?;

    let scenario_name = &identity.scenario_name;
    let mut body = String::new();
    body.push_str(&format!("# scenario: {scenario_name}\n"));
    if !identity.cell_coords.is_empty() {
        // Name the cell inside the file too: a reader who found this log by
        // globbing still needs to know which axis values produced it.
        let coords = identity
            .cell_coords
            .iter()
            .map(|(axis, value)| format!("{axis}={value}"))
            .collect::<Vec<_>>()
            .join(" ");
        body.push_str(&format!("# cell: {coords}\n"));
    }
    // A hard fault outranks the assertion tally: a run that could not complete
    // did not "pass", however many assertions held before it stopped. The
    // serialized `Status` deliberately has no error variant (see its docs), so
    // the fault is rendered here rather than smuggled into the report.
    let status_label = match fault {
        Some(_) => "Errored".to_string(),
        None => format!("{:?}", report.status),
    };
    body.push_str(&format!("# status: {status_label}\n"));
    if let Some(message) = fault {
        body.push_str(&format!("# error: {message}\n"));
    }
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
    // The log body is masked in full — every byte of it is run-derived text.
    // Ownership is *not* in here: it lives in an unmasked sidecar (see
    // `CLAIM_SUFFIX`), because masking would corrupt any tag placed in the body.
    let masked = mask_secrets(&body, secrets);

    // Ownership was already recorded while acquiring the name — it has to be, so
    // a concurrent racer sees an owner rather than an empty file. If the log
    // itself now fails to land, that record would point at a file this run never
    // produced, so it is removed again: a name left claimed with no log behind it
    // would be refused to every future run.
    if let Err(e) = log_dir_fd.write_private(&log_path, masked.as_bytes()) {
        // Deliberately no cleanup. Unlinking by *name* is unsound here: between
        // deciding to remove and removing, the name can be swapped, and inode
        // reuse makes even a (dev, ino) comparison unreliable — the held `flock`
        // is on the old inode and does not prevent a rename. A leftover sidecar
        // costs nothing, because the next run's reclaim path already recognizes
        // an unparseable or empty record with no log beside it and takes it over.
        return Err(PittyError::Process(format!("cannot write log: {e}")));
    }

    Ok(())
}

/// The number of suffixed candidates tried before a log path is reused.
///
/// Why bounded at all: the probe below stats the filesystem once per candidate,
/// so an unbounded loop would turn a pathological suite into a long stall right
/// where the run is supposed to be finishing. A suite that genuinely produces a
/// thousand identities colliding on one stem has an authoring problem the log
/// writer cannot fix; overwriting the first candidate at that point loses one
/// log instead of hanging the run.
const MAX_LOG_SUFFIX: u32 = 1000;

/// The names this process has already resolved, keyed by `(logs dir, identity)`.
///
/// A *within-run* shortcut, not the mechanism that makes a re-run reuse its file
/// — the claim sidecar does that, and it works across processes. The memo keeps
/// repeated writes of one identity (a `pitty bench` loop, a matrix cell) from
/// re-probing, and keeps a run from moving to a different file midway.
///
/// A cached name is **re-validated against the current directory** before it is
/// trusted: the key is a path *string*, so if the base directory is replaced
/// between iterations the same key can name a different tree, where the cached
/// name may belong to somebody else or to nobody. Re-checking the claim there
/// costs one small read and removes the case where a stale entry overwrote an
/// unclaimed log in a fresh tree.
///
/// Process-global because `logs/` is a process-wide sink and scenarios run
/// strictly sequentially; the mutex is held only across a map lookup.
static WRITTEN_LOG_PATHS: std::sync::Mutex<Option<BTreeMap<(String, String), String>>> =
    std::sync::Mutex::new(None);

/// Log names this process has already bound to some identity, per logs directory.
///
/// Two matrix cells differing only *inside* a secret share a masked claim key, so
/// the on-disk record cannot tell them apart on a first run — by design, since
/// anything that could would be derived from the secret. What does tell them
/// apart is that they are different identities *in this process*: once one cell
/// has taken a name, the next must not adopt it, so they end up on separate
/// files.
///
/// That separation does **not** extend across processes. A later run cannot
/// recompute which of the two tokens was its own — the token is random, and the
/// only thing it could be derived from is the raw identity, which is the oracle
/// this design exists to avoid storing. So the two cells may swap files between
/// runs. What holds is that they never collide onto one file and the set of logs
/// stays stable; which cell owns which is not guaranteed.
static BOUND_LOG_NAMES: std::sync::Mutex<Option<BTreeMap<String, BTreeMap<String, String>>>> =
    std::sync::Mutex::new(None);

/// Pick the file in `log_dir` this identity writes to, suffixing on collision.
///
/// `stem.log` is used when it is free *or already belongs to this identity*;
/// otherwise `stem.2.log`, `stem.3.log`, and so on. Ownership is decided by the
/// claim tag each log carries in its header, so the decision is durable across
/// processes: re-running one scenario into an existing `logs/` reuses its own
/// file, while a genuinely different identity that happens to sanitize to the
/// same stem is suffixed past.
///
/// This is the backstop for the collision class [`sanitize_stem`] creates by
/// construction: it is lossy (every character outside `[A-Za-z0-9._-]` becomes
/// `_`), so distinct identities such as `my/test` and `my_test` still arrive here
/// with the same stem.
///
/// Why "first free name" rather than naming every log by a hash: the file name is
/// a human-facing artifact that a developer greps for after a failure, and a hash
/// suffix on every log would cost that readability to solve a collision that is
/// rare in practice. The hash is confined to the header, where nobody has to read
/// it. Why not truncate to a fixed width instead: that would *create* collisions
/// rather than resolve them.
///
/// Returns `Err` when every candidate up to [`MAX_LOG_SUFFIX`] is held by a
/// different identity. Reporting that is the point: overwriting one of them would
/// destroy a log, which is the failure this scheme exists to prevent.
fn resolve_log_path(
    dir: &LogDir,
    identity: &LogIdentity,
    secrets: &[String],
) -> Result<(String, ClaimLock), PittyError> {
    let claim_key = identity.claim_key(secrets);
    // Keyed by the directory *object*, so a symlink alias to one physical
    // `logs/` shares the same bookkeeping rather than starting empty.
    let dir_key = dir.identity_key();
    let key = (dir_key.clone(), identity.dedup_key());
    let mut guard = WRITTEN_LOG_PATHS.lock().unwrap_or_else(|e| e.into_inner());
    let seen = guard.get_or_insert_with(BTreeMap::new);
    if let Some(existing) = seen.get(&key) {
        // Re-acquire it here rather than trusting the entry: the directory may
        // not be the one this name was resolved against, and the lock has to be
        // taken afresh for this write in any case.
        let existing = existing.clone();
        if let Some(record) = read_claim(dir, &existing) {
            if record.key == claim_key {
                if let Some(lock) = acquire_for(dir, &existing, &record) {
                    return Ok((existing, lock));
                }
            }
        }
        seen.remove(&key);
    }

    // The discriminator is spent only when masking actually removed text, so a
    // scenario with no secret in its identity gets an untouched filename.
    let stem = identity.stem(secrets);
    // Names bound to a *different* identity in this directory. A run's own name
    // is never excluded from itself, so repeated writes still reuse one file.
    let taken: BTreeSet<String> = {
        let mut bound = BOUND_LOG_NAMES.lock().unwrap_or_else(|e| e.into_inner());
        bound
            .get_or_insert_with(BTreeMap::new)
            .entry(dir_key.clone())
            .or_default()
            .iter()
            .filter(|(_, owner)| *owner != &identity.dedup_key())
            .map(|(name, _)| name.clone())
            .collect()
    };
    let (chosen, lock) = first_free_name(dir, &stem, &claim_key, &taken).ok_or_else(|| {
        PittyError::Process(format!(
            "cannot find a free log name for '{stem}': \
             {MAX_LOG_SUFFIX} candidates are already taken or held by other runs"
        ))
    })?;
    seen.insert(key, chosen.clone());
    {
        let mut bound = BOUND_LOG_NAMES.lock().unwrap_or_else(|e| e.into_inner());
        bound
            .get_or_insert_with(BTreeMap::new)
            .entry(dir_key)
            .or_default()
            .insert(chosen.clone(), identity.dedup_key());
    }
    Ok((chosen, lock))
}

/// The first `stem[.N].log` in `dir` that is free or already carries `claim`.
///
/// A candidate is taken when no file occupies it, or when the file there was
/// written by this same identity (its header claim matches) — that second case is
/// what lets a re-run overwrite its own log instead of accumulating a numbered
/// copy per invocation. A file whose claim differs, or which carries no claim at
/// all (an older pitty's log), belongs to someone else and is skipped.
///
/// Returns `None` when every candidate up to the bound is taken by someone else.
///
/// Why not fall back to the unsuffixed name: that name is *occupied* by another
/// identity's log, so reusing it would silently destroy a log — the exact failure
/// this whole naming scheme exists to prevent, and a direct contradiction of the
/// README's promise that a colliding run is suffixed rather than replacing an
/// earlier log. Exhaustion is reported to the caller instead, which surfaces it
/// as a log-write failure on stderr. The bound stays; only the dishonest
/// fallback at the end of it is gone.
fn first_free_name(
    dir: &LogDir,
    stem: &str,
    claim_key: &str,
    taken: &BTreeSet<String>,
) -> Option<(String, ClaimLock)> {
    // Candidates, in the order a run should prefer them: its own stem first, then
    // that stem's numeric suffixes. Nothing outside this run's stem is ever
    // considered.
    //
    // An earlier revision searched the *whole* directory for any log carrying the
    // same masked claim key. That existed because the filename discriminator was
    // salted, so losing the salt changed the name a run would pick and its own
    // log had to be found some other way. The discriminator is unkeyed and
    // therefore stable now, so a run always derives the same stem — and the
    // cross-stem search had become a live bug: two scenario *files* whose stems
    // both mask to `___` share a claim key, so the search handed one of them the
    // other's log and destroyed it. That is the #36 class this scheme exists to
    // eliminate. Restricting the search to this stem still recovers a log pushed
    // to `.2.log` by an earlier collision, which is the only case it needs to.
    let candidates = || {
        std::iter::once(format!("{stem}.log"))
            .chain((2..=MAX_LOG_SUFFIX).map(|n| format!("{stem}.{n}.log")))
    };

    // Pass 1: a log this identity already owns, wherever in the sequence it sits.
    //
    // This must precede taking any free name. An earlier collision can push a
    // run's log past an occupied candidate — `s.log` held by a foreign file, so
    // the run lands on `s.2.log` — and when that occupant later disappears, a
    // single "first acquirable name wins" pass would take the newly-freed
    // `s.log` and never look at `s.2.log`. The identity would then own two logs,
    // both with its claim key, and the older one would be orphaned: nothing
    // reuses it again, while "the log you open is the newest one" quietly stops
    // being true for it.
    //
    // Why the run stays on the suffixed name rather than migrating back to the
    // preferred one: ownership stability is the property that matters, and
    // moving a log means either copying it (racy against a concurrent reader) or
    // leaving a gap where neither name is claimed. A stable `s.2.log` is a
    // cosmetic wart; a migration is a correctness risk for no functional gain.
    // Pass 1: find a log this identity already owns, wherever in the sequence it
    // sits.
    //
    // This must precede taking any free name. An earlier collision can push a
    // run's log past occupied candidates — `s.log` held by a foreign file, so the
    // run lands on `s.2.log` — and when those occupants later disappear, a single
    // "first acquirable name wins" pass would take the newly-freed `s.log` and
    // never look further. The identity would then own two logs, both carrying its
    // claim key, and the older one would be orphaned: nothing reuses it again,
    // while "the log you open is the newest one" quietly stops being true for it.
    //
    // Why the run stays on the suffixed name rather than migrating back to the
    // preferred one: ownership stability is the property that matters, and moving
    // a log means either copying it (racy against a concurrent reader) or leaving
    // a gap where neither name is claimed. A stable `s.2.log` is a cosmetic wart;
    // a migration is a correctness risk for no functional gain.
    //
    // # Why this scan is unbounded, and why that is still cheap
    //
    // An earlier revision stopped after a few consecutive absent candidates, to
    // avoid stat-ing every name on an ordinary run. That bound was wrong at any
    // length: with enough collisions the owned log sits past the gap, the scan
    // stops short, and pass 2 claims a freed earlier name — re-creating the exact
    // orphan this pass exists to prevent. "Silently orphan a log" is not an
    // acceptable outcome at any threshold, and the syscalls saved were never
    // worth it.
    //
    // The cost is paid only when it has to be. The overwhelmingly common case —
    // the preferred name is already ours, or nothing of ours exists yet and the
    // preferred name is free — is settled by the first candidate below. The full
    // walk happens only when the preferred name is occupied by *someone else*,
    // which is precisely the situation where an owned log may be hiding further
    // along and correctness requires looking.
    let preferred = format!("{stem}.log");
    let preferred_claim = read_claim(dir, &preferred);
    let preferred_is_ours = preferred_claim
        .as_ref()
        .is_some_and(|record| record.key == claim_key);

    if preferred_is_ours && !taken.contains(&preferred) {
        // Reusing the stored record keeps the token — and therefore the file —
        // stable across runs.
        let record = preferred_claim.expect("checked above");
        if let Some(lock) = acquire_for(dir, &preferred, &record) {
            return Some((preferred, lock));
        }
    }

    // The preferred name is not ours, so an owned log may sit anywhere in the
    // suffix range. Walk all of it rather than guessing where to stop.
    for candidate in candidates() {
        if taken.contains(&candidate) {
            continue;
        }
        let Some(record) = read_claim(dir, &candidate) else {
            continue;
        };
        if record.key != claim_key {
            continue;
        }
        if let Some(lock) = acquire_for(dir, &candidate, &record) {
            return Some((candidate, lock));
        }
    }

    // Pass 2: no log of ours exists yet, so take the first name we may have.
    // `acquire_for` refuses anything belonging to someone else, including a log
    // with no sidecar at all.
    for candidate in candidates() {
        if taken.contains(&candidate) {
            continue;
        }
        let want = ClaimRecord {
            key: claim_key.to_string(),
            token: random_hex(CLAIM_TOKEN_BYTES),
        };
        if let Some(lock) = acquire_for(dir, &candidate, &want) {
            return Some((candidate, lock));
        }
    }
    None
}

/// Acquire the log named `name` for this identity, if it may be written.
///
/// Returns the held lock (see [`ClaimLock`]) or `None`.
///
/// A log with **no sidecar at all** belongs to nobody this run can verify — an
/// older pitty's, or one a user dropped in — so it is left alone and suffixed
/// past. A log whose sidecar exists but does not parse is different: that is
/// residue from a run interrupted mid-write, and it is handled by
/// [`LogDir::adopt_existing_sidecar`] rather than refused here. Refusing it would
/// strand the log forever, since nothing could ever prove ownership of it again.
fn acquire_for(dir: &LogDir, name: &str, want: &ClaimRecord) -> Option<ClaimLock> {
    let log_present = !dir.is_absent(name);
    let sidecar_present = !dir.is_absent(&claim_sidecar_name(name));
    if log_present && !sidecar_present {
        return None;
    }
    dir.try_acquire(name, want)
}

/// Serializes the tests that clear [`WRITTEN_LOG_PATHS`].
///
/// Why a lock rather than letting them run free: clearing is process-global, so
/// two such tests interleaving (or one clearing while another is mid-resolve)
/// would make the memo's contents depend on scheduling. Each test uses its own
/// temp `logs/`, so a stray clear cannot produce a *wrong* path — only a
/// redundant re-probe — but serializing keeps the suite deterministic instead of
/// relying on that argument holding as tests are added. Mirrors the existing
/// `ENV_TEST_LOCK` idiom for process-global test state.
#[cfg(test)]
static LOG_MEMO_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Drop the in-process path memo, so the next resolve behaves as a fresh process.
///
/// Test-only. The cross-process behavior this crate has to guarantee is decided
/// by the on-disk claim, not by [`WRITTEN_LOG_PATHS`]; clearing the memo is how a
/// unit test reaches that path without actually spawning a second process (the
/// real multi-process coverage lives in `tests/e2e.rs`).
#[cfg(test)]
fn clear_written_log_paths() {
    let mut guard = WRITTEN_LOG_PATHS.lock().unwrap_or_else(|e| e.into_inner());
    *guard = None;
}

/// The scenario-file extension whose logs carry no extension marker.
///
/// `pitty run <dir>` accepts `.yaml` and `.yml` alike (see `cli::is_yaml`).
/// Treating `.yaml` as canonical keeps the common log name unchanged and marks
/// only the alternative spelling, which is all that is needed to separate a
/// `a.yaml`/`a.yml` pair.
const CANONICAL_SCENARIO_EXTENSION: &str = "yaml";

/// The longest file *name* (not path) a log may occupy, in bytes.
///
/// 255 is `NAME_MAX` on every filesystem pitty realistically writes logs to:
/// ext4, XFS, btrfs, APFS, HFS+, and NTFS all cap a single path component at 255
/// (NTFS counts UTF-16 units, so 255 bytes of the ASCII that [`sanitize_stem`]
/// emits is always within it). Windows' own limit is on the *whole path*
/// (`MAX_PATH`), not the component, and a 255-byte component leaves that budget
/// to the directory prefix exactly as it does on Unix.
///
/// Why a constant rather than querying `pathconf(_PC_NAME_MAX)` at runtime: the
/// query is Unix-only (so Windows would need the constant anyway), it can fail
/// or answer for the wrong mount when `logs/` has not been created yet, and a
/// filesystem that answered *smaller* than 255 is not one this tool targets.
/// Hard-coding the floor every target shares keeps one code path and makes the
/// bound testable; it errs toward a shorter name, never a too-long one.
const MAX_LOG_FILE_NAME: usize = 255;

/// The room [`resolve_log_path`] needs after the stem: `.log`, plus the widest
/// collision suffix it can append (`.1000`).
///
/// Reserved up front so a stem that just fits cannot be pushed over the limit by
/// the suffix that a collision later adds — the failure would then depend on
/// whether some *other* scenario happened to take the unsuffixed name first.
const LOG_SUFFIX_RESERVE: usize = ".log".len() + ".1000".len() + CLAIM_SIDECAR_RESERVE;

/// The extra room a log name's claim sidecar needs beyond the log name itself.
///
/// The sidecar is `.<log name>.claim`, so it is always longer than the log. If
/// only the log had to fit, a maximal name would produce a sidecar past
/// `NAME_MAX`, its exclusive create would fail with `ENAMETOOLONG`, and the
/// candidate would be judged unavailable — every candidate, so the run would
/// report "all 1000 names taken" and write no log at all.
const CLAIM_SIDECAR_RESERVE: usize = ".".len() + ".claim".len();

/// The longest stem that always leaves room for the suffix and extension.
const MAX_STEM_BYTES: usize = MAX_LOG_FILE_NAME - LOG_SUFFIX_RESERVE;

/// Shorten `stem` to [`MAX_STEM_BYTES`], ending it with a digest of the whole.
///
/// A stem within the bound is returned untouched, which is the overwhelmingly
/// common case — the readability argument in [`resolve_log_path`] applies here
/// too, so nothing pays for this until a name actually would not fit. Past the
/// bound the head is kept (it is what a human recognizes) and the tail is
/// replaced by `~<digest>`, so an over-long name still produces a *writable*,
/// deterministic file instead of an `ENAMETOOLONG` that loses the log entirely.
///
/// The digest is what keeps truncation honest: two long stems sharing a prefix
/// would otherwise collapse onto one name. They still can collide in principle —
/// this is a 64-bit digest, and [`sanitize_stem`] is lossy besides — which is
/// precisely why the claim mechanism in [`first_free_path`] is the authority on
/// ownership rather than the file name. Truncation only has to be *stable* and
/// *rarely* colliding; correctness on collision is already handled.
///
/// Truncation is on a UTF-8 char boundary, but since `sanitize_stem` has already
/// reduced every part to ASCII, that boundary is always a byte boundary here.
fn bound_stem(stem: &str) -> String {
    bound_text(stem, MAX_STEM_BYTES)
}

/// Shorten `text` to `limit` bytes, ending it with a digest of the whole.
///
/// Text within the limit is returned untouched — the overwhelmingly common case,
/// so nothing pays for this until a value actually would not fit. Past it the
/// head is kept (that is what a human recognizes, and what keeps a prefix
/// meaningful) and the tail is replaced by `~<digest>`.
///
/// The digest is what keeps truncation honest: two long values sharing a prefix
/// would otherwise collapse onto one result. They still can collide in principle
/// — this is a 64-bit digest — which is why neither caller treats the bounded
/// form as an identity: the filename has the collision-suffix machinery behind
/// it, and the claim key has the per-log token.
///
/// Truncation lands on a UTF-8 char boundary, so a multi-byte value is never cut
/// mid-character.
fn bound_text(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }

    let digest = fnv1a_hex(text.as_bytes());
    let marker_len = 1 + digest.len();
    let head_budget = limit.saturating_sub(marker_len);
    let mut head_end = head_budget;
    while head_end > 0 && !text.is_char_boundary(head_end) {
        head_end -= 1;
    }
    format!("{}~{digest}", &text[..head_end])
}

/// `n` random bytes as lowercase hex, from the OS entropy source.
///
/// Used only for the per-log ownership token, which has to be unique among the
/// logs of one directory. It is not a secret and authenticates nothing, so the
/// fallback below is adequate where `/dev/urandom` is unavailable.
fn random_hex(n: usize) -> String {
    let mut bytes = vec![0u8; n];

    #[cfg(unix)]
    {
        // `/dev/urandom` rather than `getrandom(2)`: the syscall is not exposed
        // by `libc` on every Unix this builds for (notably macOS), while the
        // device is available on all of them.
        use std::io::Read;
        if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
            if f.read_exact(&mut bytes).is_ok() {
                return bytes.iter().map(|b| format!("{b:02x}")).collect();
            }
        }
    }

    // Fallback: hash process-local entropy. Weaker, but a token only has to be
    // distinct from its siblings in one directory, never unguessable.
    use std::hash::{BuildHasher, Hasher};
    for (i, byte) in bytes.iter_mut().enumerate() {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_usize(i);
        h.write_u128(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default(),
        );
        *byte = (h.finish() & 0xff) as u8;
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// FNV-1a (64-bit) of `bytes`, rendered as 16 lowercase hex digits.
///
/// Shared by [`LogIdentity::claim_tag`] and [`bound_stem`]. See `claim_tag` for
/// why a non-cryptographic digest is the right tool for both.
fn fnv1a_hex(bytes: &[u8]) -> String {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET_BASIS;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    format!("{hash:016x}")
}

/// Reduce one identity part to a filesystem-safe stem.
///
/// Keeps alphanumerics, dash, underscore, and dot; replaces everything else
/// (including path separators) with `_`. This prevents a scenario `name` — or a
/// matrix axis value, which is equally free-form — from directing the log write
/// outside `logs/`.
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

/// Write `bytes` to `path`, with the file already `0600` before any content
/// reaches it (Unix).
///
/// Only reachable on non-Unix now that [`LogDir`] writes descriptor-relatively;
/// kept because the non-Unix arm of `LogDir::write_private` calls it.
///
/// Ordering mirrors `assert::snapshot`'s recorder, and for the same reason —
/// logs are the more sensitive of the two sinks, since they carry the full
/// terminal output:
///
/// 1. open with `mode(0o600)`, which applies only when the open *creates* the
///    file, so a log that already exists keeps whatever mode it had (including
///    the `0644` a pre-fix pitty wrote);
/// 2. `fchmod` the descriptor to `0600`, which fixes that existing-file case —
///    and acts on the descriptor rather than the path, so a concurrent swap
///    cannot redirect it;
/// 3. truncate, so an existing file is never observable at its old permissive
///    mode holding new content;
/// 4. only then write.
///
/// The previous order (create, write, then `set_permissions`) left every new log
/// at the umask default — `0644` on a typical runner — for the entire duration
/// of the write, which is exactly when the secret-bearing terminal output is
/// landing in it.
#[cfg(all(unix, test))]
fn write_private_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    // Scoped to this arm rather than the module: the non-unix arm writes via
    // `fs::write`, so a module-level import would be an unused-import warning
    // on Windows, which `just lint` escalates to an error.
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    // O_NOFOLLOW: refuse to write *through* a symlink at the final component.
    // Without it a link planted at the log's name — including a dangling one,
    // which the `symlink_metadata` probe now also catches — makes this open
    // create and fill the link's target, putting the full terminal output
    // wherever the link points. The probe and this flag are separate defenses on
    // purpose: the probe can be raced (a link planted after the name is chosen),
    // and O_NOFOLLOW closes that race at the moment of the open.
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    // fchmod-equivalent on the open descriptor: `File::set_permissions` acts on
    // the descriptor, not a re-resolved path.
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    file.set_len(0)?;
    file.write_all(bytes)
}

/// Non-Unix fallback: plain create-and-write.
///
/// Windows has no mode bits on `OpenOptions`, so logs there rely on the runner
/// user's default file ACLs — the same position `assert::snapshot` takes.
#[cfg(not(unix))]
fn write_private_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    std::fs::write(path, bytes)
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

    fn coords(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    /// Capture a log anchor for `dir`, as the runner does before spawning.
    fn anchor_for(dir: &Path) -> LogAnchor {
        LogAnchor::capture(dir)
    }

    /// The claim key recorded in `<dir>/logs/<name>`, as the writer stores it.
    fn claim_in(dir: &Path, name: &str, _secrets: &[String]) -> Option<String> {
        let log_dir = dir.join("logs");
        let handle = open_log_dir(&anchor_for(dir), &log_dir).expect("logs/ must open");
        read_claim(&handle, name).map(|r| r.key)
    }

    /// The claim key `identity` records under `secrets` (masked, salt-free).
    fn tag_in_with(identity: &LogIdentity, secrets: &[String]) -> String {
        identity.claim_key(secrets)
    }

    /// The claim key `identity` records with no secrets registered.
    fn tag_in(_dir: &Path, identity: &LogIdentity) -> String {
        identity.claim_key(&[])
    }

    fn demo_report() -> Report {
        Report {
            scenario: "demo".into(),
            status: Status::Passed,
            duration_ms: 1,
            assertions: Vec::new(),
        }
    }

    /// Log file names written under one `logs/` directory, sorted.
    ///
    /// Claim sidecars are excluded: they are an implementation detail of
    /// ownership, not artifacts a user reads, and every assertion here is about
    /// which *logs* exist.
    fn log_names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir.join("logs"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| !n.ends_with(CLAIM_SUFFIX))
            .collect();
        names.sort();
        names
    }

    #[test]
    fn plain_identity_keeps_the_name_only_stem() {
        // A run with no file and no cell must still write logs/<name>.log, the
        // shape the README documents for the ordinary case.
        assert_eq!(LogIdentity::new("demo").stem(&[]), "demo");
    }

    #[test]
    fn file_stem_prefixes_a_differing_scenario_name() {
        // The scenario file is part of the identity, so two files can be told
        // apart even when they declare the same name.
        let identity = LogIdentity::new("same-name").with_file(Path::new("dir/a.yaml"));
        assert_eq!(identity.stem(&[]), "a.same-name");
    }

    #[test]
    fn file_stem_matching_the_name_is_not_repeated() {
        // The conventional case (echo-flow.yaml declaring name: echo-flow) keeps
        // the documented logs/<scenario>.log name rather than doubling the stem.
        let identity = LogIdentity::new("echo-flow").with_file(Path::new("e2e/echo-flow.yaml"));
        assert_eq!(identity.stem(&[]), "echo-flow");
    }

    #[test]
    fn a_file_named_after_another_scenario_still_gets_a_distinct_stem() {
        // Collapsing a matching file stem must not reintroduce a collision: two
        // files can only produce one stem if BOTH file and name match, which a
        // single directory cannot contain.
        let same = LogIdentity::new("a").with_file(Path::new("a.yaml"));
        let other = LogIdentity::new("a").with_file(Path::new("b.yaml"));
        assert_eq!(same.stem(&[]), "a");
        assert_eq!(other.stem(&[]), "b.a");
        assert_ne!(same.stem(&[]), other.stem(&[]));
    }

    #[test]
    fn cell_coords_suffix_the_stem_in_axis_order() {
        // Each matrix cell contributes every axis as `axis-value`, in axis
        // (BTreeMap key) order, so a cell's stem is unique and reproducible.
        let identity = LogIdentity::new("m")
            .with_file(Path::new("m.yaml"))
            .with_cell(coords(&[("region", "us"), ("cmd", "bash")]));
        assert_eq!(identity.stem(&[]), "m.cmd-bash.region-us");
    }

    #[test]
    fn identity_parts_are_sanitized_independently() {
        // Path separators in any part (name or axis value) must be neutralized,
        // so no part can direct the write outside logs/.
        let identity = LogIdentity::new("../escape")
            .with_file(Path::new("../../f.yaml"))
            .with_cell(coords(&[("a", "../x")]));
        let stem = identity.stem(&[]);
        assert!(!stem.contains('/'), "stem must carry no separator: {stem}");
        assert_eq!(stem, "f..._escape.a-.._x");
    }

    #[test]
    fn two_files_sharing_a_name_write_separate_logs() {
        // Issue #36: a directory run of two files declaring the same `name:`
        // must leave two logs, not one overwritten by the other.
        let dir = tempfile::tempdir().unwrap();
        for file in ["a.yaml", "b.yaml"] {
            let identity = LogIdentity::new("same-name").with_file(Path::new(file));
            write_log(
                dir.path(),
                &anchor_for(dir.path()),
                &identity,
                file,
                &demo_report(),
                None,
                &[],
            )
            .unwrap();
        }
        assert_eq!(
            log_names(dir.path()),
            vec!["a.same-name.log", "b.same-name.log"]
        );
    }

    #[test]
    fn matrix_cells_write_one_log_each() {
        // Issue #31: every cell of a matrix must keep its own diagnostics; three
        // cells must leave three distinct files.
        let dir = tempfile::tempdir().unwrap();
        for word in ["alpha", "bravo", "charlie"] {
            let identity = LogIdentity::new("m")
                .with_file(Path::new("m.yaml"))
                .with_cell(coords(&[("word", word)]));
            write_log(
                dir.path(),
                &anchor_for(dir.path()),
                &identity,
                word,
                &demo_report(),
                None,
                &[],
            )
            .unwrap();
        }
        let names = log_names(dir.path());
        assert_eq!(names.len(), 3, "each cell needs its own log: {names:?}");
        for word in ["alpha", "bravo", "charlie"] {
            let body = std::fs::read_to_string(dir.path().join(format!("logs/m.word-{word}.log")))
                .unwrap_or_else(|e| panic!("cell {word} must have a log: {e}"));
            assert!(body.contains(&format!("# cell: word={word}")));
            assert!(body.contains(word));
        }
    }

    #[test]
    fn distinct_identities_colliding_on_a_stem_are_suffixed() {
        // sanitize_stem is lossy, so `my/test` and `my_test` reduce to the same
        // stem. The second must be suffixed rather than destroy the first.
        let dir = tempfile::tempdir().unwrap();
        write_log(
            dir.path(),
            &anchor_for(dir.path()),
            &LogIdentity::new("my/test"),
            "first",
            &demo_report(),
            None,
            &[],
        )
        .unwrap();
        write_log(
            dir.path(),
            &anchor_for(dir.path()),
            &LogIdentity::new("my_test"),
            "second",
            &demo_report(),
            None,
            &[],
        )
        .unwrap();
        assert_eq!(log_names(dir.path()), vec!["my_test.2.log", "my_test.log"]);
        let first = std::fs::read_to_string(dir.path().join("logs/my_test.log")).unwrap();
        assert!(
            first.contains("first"),
            "the earlier log must survive: {first}"
        );
    }

    #[test]
    fn repeating_one_identity_overwrites_its_own_log() {
        // `pitty bench` runs the same scenario N times, and those runs are meant
        // to be identical: they must reuse one log rather than leave a numbered
        // copy per iteration. The surviving log is the last run's.
        let dir = tempfile::tempdir().unwrap();
        for i in 0..3 {
            let body = format!("run-{i}");
            write_log(
                dir.path(),
                &anchor_for(dir.path()),
                &LogIdentity::new("bench-shell"),
                &body,
                &demo_report(),
                None,
                &[],
            )
            .unwrap();
        }
        assert_eq!(log_names(dir.path()), vec!["bench-shell.log"]);
        let log = std::fs::read_to_string(dir.path().join("logs/bench-shell.log")).unwrap();
        assert!(log.contains("run-2"), "the last run must win: {log}");
    }

    #[test]
    fn a_log_records_the_claim_of_the_identity_that_wrote_it() {
        // Ownership is durable only if it is on disk: the log must carry its
        // identity's claim tag, and reading that file back must recover it.
        let dir = tempfile::tempdir().unwrap();
        let identity = LogIdentity::new("claimed").with_file(Path::new("c.yaml"));
        write_log(
            dir.path(),
            &anchor_for(dir.path()),
            &identity,
            "out",
            &demo_report(),
            None,
            &[],
        )
        .unwrap();

        let log_name = "c.claimed.log";
        assert_eq!(
            claim_in(dir.path(), log_name, &[]).as_deref(),
            Some(&*tag_in(dir.path(), &identity))
        );
    }

    #[test]
    fn a_claim_survives_secret_masking() {
        // The body is masked before it is written, so a claim built from the raw
        // identity could be rewritten to *** and stop matching. The digest must
        // carry no identity material and therefore pass through untouched.
        let dir = tempfile::tempdir().unwrap();
        let identity = LogIdentity::new("topsecret-name");
        write_log(
            dir.path(),
            &anchor_for(dir.path()),
            &identity,
            "body",
            &demo_report(),
            None,
            &["topsecret".to_string()],
        )
        .unwrap();

        let secrets = ["topsecret".to_string()];
        let log_name = "topsecret-name.log";
        let body = std::fs::read_to_string(dir.path().join("logs").join(log_name)).unwrap();
        assert!(body.contains("***"), "the secret must be masked: {body}");
        // The reader must be given the same secret list the writer used: the tag
        // is derived from it, and the key it searches for is masked with it.
        assert_eq!(
            claim_in(dir.path(), log_name, &secrets).as_deref(),
            Some(&*tag_in_with(&identity, &secrets)),
            "masking must not disturb the claim"
        );
    }

    #[test]
    fn distinct_identities_get_distinct_claims() {
        // The claim has to separate exactly what the file name cannot: two
        // identities that reduce to one stem must not be mistaken for each other.
        let slash = LogIdentity::new("my/test");
        let underscore = LogIdentity::new("my_test");
        assert_eq!(
            slash.stem(&[]),
            underscore.stem(&[]),
            "stems must collide here"
        );
        assert_ne!(slash.claim_key(&[]), underscore.claim_key(&[]));
    }

    #[test]
    fn an_unclaimed_file_is_never_treated_as_ours() {
        // A log from an older pitty carries no claim. Absence of proof is not
        // proof of ownership, so such a file must be skipped, not overwritten.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("logs")).unwrap();
        let occupied = dir.path().join("logs/legacy.log");
        std::fs::write(&occupied, "# scenario: legacy\nno claim here\n").unwrap();

        write_log(
            dir.path(),
            &anchor_for(dir.path()),
            &LogIdentity::new("legacy"),
            "fresh",
            &demo_report(),
            None,
            &[],
        )
        .unwrap();

        let preserved = std::fs::read_to_string(&occupied).unwrap();
        assert!(preserved.contains("no claim here"), "{preserved}");
        assert_eq!(log_names(dir.path()), vec!["legacy.2.log", "legacy.log"]);
    }

    #[test]
    fn a_claimed_file_is_reused_even_by_a_fresh_process_view() {
        // Simulates the cross-process case the in-process memo cannot: write a
        // log, then resolve the same identity again with the memo cleared, as a
        // second `pitty run` would. The claim on disk must route it back to the
        // same file instead of suffixing past it.
        let _guard = LOG_MEMO_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let identity = LogIdentity::new("rerun");
        write_log(
            dir.path(),
            &anchor_for(dir.path()),
            &identity,
            "first",
            &demo_report(),
            None,
            &[],
        )
        .unwrap();

        clear_written_log_paths();

        write_log(
            dir.path(),
            &anchor_for(dir.path()),
            &identity,
            "second",
            &demo_report(),
            None,
            &[],
        )
        .unwrap();

        assert_eq!(log_names(dir.path()), vec!["rerun.log"]);
        let log = std::fs::read_to_string(dir.path().join("logs/rerun.log")).unwrap();
        assert!(log.contains("second"), "the re-run must win: {log}");
    }

    #[test]
    fn yaml_and_yml_siblings_get_distinct_stems() {
        // `pitty run <dir>` executes both extensions, so `a.yaml` and `a.yml` are
        // two scenarios whose logs must not collide even when they share a name.
        let yaml = LogIdentity::new("same").with_file(Path::new("a.yaml"));
        let yml = LogIdentity::new("same").with_file(Path::new("a.yml"));
        assert_ne!(yaml.stem(&[]), yml.stem(&[]));
        assert_ne!(yaml.claim_key(&[]), yml.claim_key(&[]));
    }

    #[test]
    fn the_canonical_extension_leaves_the_common_name_untouched() {
        // Separating the pair must not cost every ordinary scenario its plain
        // log name: `.yaml` stays invisible, and only `.yml` is marked.
        let conventional = LogIdentity::new("echo-flow").with_file(Path::new("echo-flow.yaml"));
        assert_eq!(conventional.stem(&[]), "echo-flow");

        let alternative = LogIdentity::new("echo-flow").with_file(Path::new("echo-flow.yml"));
        assert_eq!(alternative.stem(&[]), "echo-flow-yml.echo-flow");
    }

    #[test]
    fn two_yaml_and_yml_files_write_separate_logs() {
        // The end-to-end shape of the pair: two files, two logs, each holding
        // its own output.
        let dir = tempfile::tempdir().unwrap();
        for (file, body) in [("a.yaml", "AAA-yaml"), ("a.yml", "BBB-yml")] {
            let identity = LogIdentity::new("same").with_file(Path::new(file));
            write_log(
                dir.path(),
                &anchor_for(dir.path()),
                &identity,
                body,
                &demo_report(),
                None,
                &[],
            )
            .unwrap();
        }
        let names = log_names(dir.path());
        assert_eq!(names.len(), 2, "each file needs its own log: {names:?}");
        let plain = std::fs::read_to_string(dir.path().join("logs/a.same.log")).unwrap();
        assert!(plain.contains("AAA-yaml"), "{plain}");
    }

    #[test]
    fn an_over_long_stem_is_bounded_to_a_writable_name() {
        // A long matrix axis value used to push the file name past NAME_MAX, so
        // the write failed with ENAMETOOLONG and the log was lost entirely. The
        // stem must stay within the limit that leaves room for `.log` and a
        // collision suffix.
        let identity = LogIdentity::new("m")
            .with_file(Path::new("m.yaml"))
            .with_cell(coords(&[("word", &"x".repeat(250))]));
        let stem = identity.stem(&[]);
        assert!(
            stem.len() <= MAX_STEM_BYTES,
            "stem must be bounded, got {} bytes",
            stem.len()
        );
        assert!(
            format!("{stem}.1000.log").len() <= MAX_LOG_FILE_NAME,
            "the suffixed form must still fit in a file name"
        );
    }

    #[test]
    fn a_short_stem_is_never_rewritten_by_the_length_bound() {
        // The bound must cost nothing in the common case: an ordinary name keeps
        // its exact spelling, with no digest appended.
        assert_eq!(bound_stem("echo-flow"), "echo-flow");
        assert!(!bound_stem("echo-flow").contains('~'));
    }

    #[test]
    fn distinct_over_long_stems_stay_distinct_after_truncation() {
        // Truncation keeps a shared prefix, so the digest is what stops two long
        // cells from collapsing onto one name.
        let long_a = format!("m.word-{}a", "x".repeat(300));
        let long_b = format!("m.word-{}b", "x".repeat(300));
        assert_ne!(bound_stem(&long_a), bound_stem(&long_b));
    }

    #[test]
    fn an_over_long_identity_actually_writes_its_log() {
        // The behavioral form of the bound: the write must succeed and leave a
        // readable log, where before it failed with ENAMETOOLONG and lost it.
        let dir = tempfile::tempdir().unwrap();
        let identity = LogIdentity::new("m")
            .with_file(Path::new("m.yaml"))
            .with_cell(coords(&[("word", &"x".repeat(250))]));
        write_log(
            dir.path(),
            &anchor_for(dir.path()),
            &identity,
            "cell-output",
            &demo_report(),
            None,
            &[],
        )
        .expect("an over-long identity must still produce a writable log");

        let names = log_names(dir.path());
        assert_eq!(names.len(), 1, "{names:?}");
        assert!(names[0].len() <= MAX_LOG_FILE_NAME);
        let body = std::fs::read_to_string(dir.path().join("logs").join(&names[0])).unwrap();
        assert!(body.contains("cell-output"), "{body}");
    }

    #[test]
    fn a_secret_matching_the_claim_key_cannot_corrupt_the_claim() {
        // Masking is blind substring replacement: a secret of `id` rewrites the
        // literal key `# id:` to `# ***:`, and the next process then cannot
        // recognize its own log. The claim must survive any secret value.
        let dir = tempfile::tempdir().unwrap();
        let identity = LogIdentity::new("s");
        let secrets = vec!["id".to_string()];
        write_log(
            dir.path(),
            &anchor_for(dir.path()),
            &identity,
            "out",
            &demo_report(),
            None,
            &secrets,
        )
        .unwrap();

        let log_name = "s.log";
        assert_eq!(
            claim_in(dir.path(), log_name, &secrets).as_deref(),
            Some(&*tag_in(dir.path(), &identity)),
            "a secret colliding with the claim key must not corrupt it"
        );
    }

    #[test]
    fn a_secret_matching_the_digest_cannot_corrupt_the_claim() {
        // The same hazard through the value rather than the key: a secret equal
        // to a hex substring that actually appears in this identity's digest.
        let dir = tempfile::tempdir().unwrap();
        let identity = LogIdentity::new("s");
        let tag = identity.claim_key(&[]);
        let secrets = vec![tag[..4].to_string()];
        write_log(
            dir.path(),
            &anchor_for(dir.path()),
            &identity,
            "out",
            &demo_report(),
            None,
            &secrets,
        )
        .unwrap();

        let log_name = "s.log";
        assert_eq!(
            claim_in(dir.path(), log_name, &secrets).as_deref(),
            Some(&*tag_in(dir.path(), &identity)),
            "a secret colliding with the digest must not corrupt it"
        );
    }

    #[test]
    fn a_claim_colliding_secret_still_does_not_accumulate_logs() {
        // The behavioral consequence: with the claim intact, a re-run reuses its
        // own file even when the secret collides with the claim line. The memo is
        // cleared between writes so each resolve is a fresh-process view.
        let _guard = LOG_MEMO_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let identity = LogIdentity::new("s");
        let secrets = vec!["id".to_string(), identity.claim_key(&[])[..4].to_string()];

        for _ in 0..3 {
            clear_written_log_paths();
            write_log(
                dir.path(),
                &anchor_for(dir.path()),
                &identity,
                "out",
                &demo_report(),
                None,
                &secrets,
            )
            .unwrap();
        }
        assert_eq!(log_names(dir.path()), vec!["s.log"]);
    }

    #[test]
    fn every_byte_of_a_log_including_the_claim_goes_through_masking() {
        // CORRECTED: an earlier version of this test asserted the claim line was
        // *exempt* from masking, which encoded the leak it was meant to guard —
        // a secret equal to the digest then landed verbatim. Nothing is exempt
        // now; the claim survives because `claim_tag` mixes the secret list in,
        // so no registered secret can ever equal what is written.
        let dir = tempfile::tempdir().unwrap();
        let report = Report {
            scenario: "sec".into(),
            status: Status::Passed,
            duration_ms: 1,
            assertions: vec![AssertionResult::pass("step with topsecret inside")],
        };
        write_log(
            dir.path(),
            &anchor_for(dir.path()),
            &LogIdentity::new("scenario-name"),
            "output holding topsecret",
            &report,
            None,
            &["topsecret".to_string()],
        )
        .unwrap();

        let body = std::fs::read_to_string(dir.path().join("logs/scenario-name.log")).unwrap();
        assert!(
            !body.contains("topsecret"),
            "no secret may reach the log:\n{body}"
        );
        assert_eq!(body.matches("***").count(), 2, "body:\n{body}");
    }

    #[test]
    fn a_hard_fault_is_logged_as_errored_not_passed() {
        // Defect 3: the log was assembled before teardown, so a run that ended in
        // a process fault was recorded as `# status: Passed`. A diagnostic that
        // says a failed run passed actively misleads during an incident.
        let dir = tempfile::tempdir().unwrap();
        write_log(
            dir.path(),
            &anchor_for(dir.path()),
            &LogIdentity::new("q"),
            "(no process spawned)",
            &demo_report(),
            Some("process error: cannot parse spawn command"),
            &[],
        )
        .unwrap();

        let body = std::fs::read_to_string(dir.path().join("logs/q.log")).unwrap();
        assert!(body.contains("# status: Errored"), "body:\n{body}");
        assert!(
            body.contains("# error: process error: cannot parse spawn command"),
            "the fault itself must be recorded:\n{body}"
        );
        assert!(!body.contains("# status: Passed"), "body:\n{body}");
    }

    #[test]
    fn a_completed_run_is_still_logged_with_its_real_status() {
        // The guard against over-correction: with no fault, the status is the
        // report's own verdict, exactly as before.
        let dir = tempfile::tempdir().unwrap();
        write_log(
            dir.path(),
            &anchor_for(dir.path()),
            &LogIdentity::new("ok"),
            "out",
            &demo_report(),
            None,
            &[],
        )
        .unwrap();
        let body = std::fs::read_to_string(dir.path().join("logs/ok.log")).unwrap();
        assert!(body.contains("# status: Passed"), "body:\n{body}");
        assert!(!body.contains("# error:"), "body:\n{body}");
    }

    #[cfg(unix)]
    #[test]
    fn taking_a_log_name_is_atomic() {
        // Defect 4 (round 4): the probe and the create were separate syscalls, so
        // two processes could both see a candidate free and the second would
        // truncate the first's log.
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("logs");
        std::fs::create_dir_all(&log_dir).unwrap();
        let handle = open_log_dir(&anchor_for(dir.path()), &log_dir).unwrap();

        let first = handle.try_acquire(
            "x.log",
            &ClaimRecord {
                key: "aaaa".into(),
                token: "t1".into(),
            },
        );
        assert!(first.is_some(), "first claim must win");
        assert!(
            handle
                .try_acquire(
                    "x.log",
                    &ClaimRecord {
                        key: "bbbb".into(),
                        token: "t2".into()
                    }
                )
                .is_none(),
            "a different identity must not take a claimed name"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_second_writer_of_the_same_identity_is_excluded_while_the_lock_is_held() {
        // Defect 2 (round 5): matching tags alone excluded nobody. Two processes
        // running the SAME scenario both read their own tag back, both concluded
        // the name was theirs, and both truncated and wrote the log — interleaved
        // content, not merely last-wins. The lock is what excludes the second.
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("logs");
        std::fs::create_dir_all(&log_dir).unwrap();
        let handle = open_log_dir(&anchor_for(dir.path()), &log_dir).unwrap();

        let held = handle
            .try_acquire(
                "x.log",
                &ClaimRecord {
                    key: "same".into(),
                    token: "tok".into(),
                },
            )
            .expect("first writer must acquire");
        assert!(
            handle
                .try_acquire(
                    "x.log",
                    &ClaimRecord {
                        key: "same".into(),
                        token: "tok".into()
                    }
                )
                .is_none(),
            "a second writer of the SAME identity must be excluded while the lock is held"
        );

        // Releasing it makes the name available again to that same identity —
        // the lock is run-time exclusion, not a permanent bar.
        drop(held);
        assert!(
            handle
                .try_acquire(
                    "x.log",
                    &ClaimRecord {
                        key: "same".into(),
                        token: "tok".into()
                    }
                )
                .is_some(),
            "the name must be reusable once the lock is released"
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_empty_sidecar_from_an_interrupted_run_is_reclaimable() {
        // Defect 3 (round 5): a run that died between creating the sidecar and
        // writing its tag left an empty record. That refused the name to every
        // future run, so one failure could poison all 1000 candidates forever.
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("logs");
        std::fs::create_dir_all(&log_dir).unwrap();
        std::fs::write(log_dir.join(".x.log.claim"), b"").unwrap();
        let handle = open_log_dir(&anchor_for(dir.path()), &log_dir).unwrap();

        assert!(
            handle
                .try_acquire(
                    "x.log",
                    &ClaimRecord {
                        key: "mine".into(),
                        token: "tokm".into()
                    }
                )
                .is_some(),
            "empty residue with no log beside it must be reclaimable"
        );
        assert_eq!(
            read_claim(&handle, "x.log").map(|r| r.key).as_deref(),
            Some("mine"),
            "reclaiming must record the new owner"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_log_write_leaves_no_claimed_name_behind() {
        use std::os::unix::fs::PermissionsExt;
        // Defect 3 (round 5), other half: the sidecar is written while acquiring
        // the name, so a log that then fails to land would leave the name claimed
        // with nothing behind it — refused to every future run.
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("logs");
        std::fs::create_dir_all(&log_dir).unwrap();
        // Make the log name un-writable while leaving it *claimable*: the
        // sidecar can be created, but the log itself cannot. A directory at the
        // log's own name is skipped as an unclaimed entry, so instead the
        // directory is made read-only after the salt exists, which fails the log
        // open while the already-created sidecar is still removable.
        let identity = LogIdentity::new("s");
        // Establish the salt first, so its creation is not what fails.
        let _ = tag_in(dir.path(), &identity);
        std::fs::set_permissions(&log_dir, std::fs::Permissions::from_mode(0o500)).unwrap();

        let result = write_log(
            dir.path(),
            &anchor_for(dir.path()),
            &identity,
            "out",
            &demo_report(),
            None,
            &[],
        );

        // Restore before asserting so tempdir cleanup works even on failure.
        std::fs::set_permissions(&log_dir, std::fs::Permissions::from_mode(0o700)).unwrap();

        assert!(result.is_err(), "the write must fail");
        assert!(
            !log_dir.join(".s.log.claim").exists(),
            "a failed write must not leave the name claimed"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_stale_memo_entry_is_revalidated_against_the_current_directory() {
        // Defect 4, second half: the memo keys on a path *string*, so if the base
        // directory is replaced the same key can name a different tree. A cached
        // name must be re-checked there rather than trusted blindly.
        let _guard = LOG_MEMO_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_written_log_paths();

        let root = tempfile::tempdir().unwrap();
        let base = root.path().join("base");
        std::fs::create_dir_all(&base).unwrap();
        let identity = LogIdentity::new("s");

        write_log(
            &base,
            &anchor_for(&base),
            &identity,
            "first",
            &demo_report(),
            None,
            &[],
        )
        .unwrap();

        // Replace the whole tree, leaving an UNCLAIMED log at the cached name.
        std::fs::remove_dir_all(&base).unwrap();
        std::fs::create_dir_all(base.join("logs")).unwrap();
        std::fs::write(base.join("logs/s.log"), "someone else's log\n").unwrap();

        write_log(
            &base,
            &anchor_for(&base),
            &identity,
            "second",
            &demo_report(),
            None,
            &[],
        )
        .unwrap();

        let preserved = std::fs::read_to_string(base.join("logs/s.log")).unwrap();
        assert!(
            preserved.contains("someone else's log"),
            "an unclaimed log in a fresh tree must not be overwritten:\n{preserved}"
        );
    }

    #[test]
    fn a_one_character_secret_inside_the_tag_does_not_break_reuse() {
        // Defect 1: masking replaces every *substring*, not just exact matches.
        // A secret of `"2"` rewrote part of the hex tag when the tag lived in the
        // log body, and every process then created a new numbered file. Hex tags
        // contain 0-9a-f, so a one-character secret collides routinely.
        let _guard = LOG_MEMO_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let identity = LogIdentity::new("claimed");
        // Every hex digit at once: whatever the tag is, this collides with it.
        let secrets: Vec<String> = "0123456789abcdef".chars().map(|c| c.to_string()).collect();

        for _ in 0..3 {
            clear_written_log_paths();
            write_log(
                dir.path(),
                &anchor_for(dir.path()),
                &identity,
                "out",
                &demo_report(),
                None,
                &secrets,
            )
            .unwrap();
        }
        assert_eq!(
            log_names(dir.path()),
            vec!["claimed.log"],
            "a secret colliding with the tag must not cause accumulation"
        );
    }

    #[test]
    fn the_claim_tag_is_never_written_into_the_log_body() {
        // The structural reason defect 1 is fixed: there is nothing in the body
        // for masking to corrupt, because the tag is not in the body at all.
        let dir = tempfile::tempdir().unwrap();
        let identity = LogIdentity::new("s");
        write_log(
            dir.path(),
            &anchor_for(dir.path()),
            &identity,
            "out",
            &demo_report(),
            None,
            &[],
        )
        .unwrap();

        let body = std::fs::read_to_string(dir.path().join("logs/s.log")).unwrap();
        assert!(
            !body.contains(&identity.claim_key(&[])),
            "the tag must not appear in the masked body:\n{body}"
        );
        assert!(
            claim_in(dir.path(), "s.log", &[]).is_some(),
            "but it must be recorded in the sidecar"
        );
    }

    #[test]
    fn dedup_keys_cannot_be_confused_by_separator_content() {
        // The reviewer's third point: a delimiter can occur inside a part, so
        // `["a","b"]` and `["a<sep>b"]` could flatten identically. Length
        // prefixes cannot be forged by content.
        let split = LogIdentity::new("b").with_file(Path::new("a.yaml"));
        let joined = LogIdentity::new("a\u{0}b");
        assert_ne!(split.dedup_key(), joined.dedup_key());
        assert_ne!(split.claim_key(&[]), joined.claim_key(&[]));
    }

    #[cfg(unix)]
    #[test]
    fn a_created_sidecar_is_repaired_to_0600() {
        // Defect 3 (round 8): `openat`'s mode argument is masked by the process
        // umask, so under `umask 0200` the sidecar landed unwritable and the next
        // run could not reuse it — it suffixed instead. The create path now
        // fchmods the descriptor it just made.
        //
        // Tested without touching the process umask: that is global state, and a
        // test that sets it corrupts every file other tests create in parallel.
        // Instead the sidecar is damaged directly to the mode a umask would have
        // produced, which is the state the fix has to recover from.
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let identity = LogIdentity::new("s");

        write_log(
            dir.path(),
            &anchor_for(dir.path()),
            &identity,
            "first",
            &demo_report(),
            None,
            &[],
        )
        .unwrap();
        let sidecar = dir.path().join("logs/.s.log.claim");
        assert_eq!(
            std::fs::metadata(&sidecar).unwrap().permissions().mode() & 0o777,
            0o600,
            "a newly created sidecar must be 0600 regardless of umask"
        );

        // A normal re-run reuses it: the mode pitty writes is already correct.
        clear_written_log_paths();
        write_log(
            dir.path(),
            &anchor_for(dir.path()),
            &identity,
            "second",
            &demo_report(),
            None,
            &[],
        )
        .unwrap();
        assert_eq!(
            log_names(dir.path()),
            vec!["s.log"],
            "a re-run must reuse its log rather than suffixing"
        );

        // A sidecar an OLDER pitty left at 0400 is deliberately NOT repaired:
        // fixing it would mean chmod-ing a name before opening it, which can
        // reach an inode outside `logs/` through a hard link. The run takes the
        // next candidate instead — the safe direction, and no log is lost.
        std::fs::set_permissions(&sidecar, std::fs::Permissions::from_mode(0o400)).unwrap();
        clear_written_log_paths();
        write_log(
            dir.path(),
            &anchor_for(dir.path()),
            &identity,
            "third",
            &demo_report(),
            None,
            &[],
        )
        .unwrap();
        assert!(
            log_names(dir.path()).contains(&"s.2.log".to_string()),
            "an unrepairable sidecar must be stepped past, not written through"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_created_logs_directory_is_repaired_to_0700() {
        // The same umask hazard one level up: `mkdirat`'s mode is masked too, so
        // under `umask 0400` `logs/` landed at 0300 — unopenable for reading, so
        // the run wrote no log at all. The directory's mode is now repaired via
        // the parent descriptor *before* it is opened.
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        write_log(
            dir.path(),
            &anchor_for(dir.path()),
            &LogIdentity::new("s"),
            "out",
            &demo_report(),
            None,
            &[],
        )
        .unwrap();
        assert_eq!(
            std::fs::metadata(dir.path().join("logs"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700,
            "a logs/ directory pitty creates must be 0700 regardless of umask"
        );
    }

    #[test]
    fn an_unbounded_identity_still_produces_a_readable_record() {
        // Defect 2 (round 8): nothing caps `name:` or an axis value, so the
        // writer could emit a record the reader rejected as oversize — after
        // which the scenario made a new numbered log every run until it hit the
        // suffix ceiling. The key is now bounded, so writer and reader agree by
        // construction.
        let identity = LogIdentity::new(&"q".repeat(70_000));
        let record = ClaimRecord {
            key: identity.claim_key(&[]),
            token: "tok".to_string(),
        };
        let encoded = record.encode();
        assert!(
            encoded.len() <= MAX_CLAIM_BYTES,
            "the writer must never exceed what the reader accepts: {} > {}",
            encoded.len(),
            MAX_CLAIM_BYTES
        );
        assert_eq!(ClaimRecord::decode(&encoded).as_ref(), Some(&record));
    }

    #[test]
    fn bounding_keeps_distinct_long_identities_distinct() {
        // The bound must not collapse two long scenarios onto one key.
        let a = LogIdentity::new(&format!("{}a", "q".repeat(70_000)));
        let b = LogIdentity::new(&format!("{}b", "q".repeat(70_000)));
        assert_ne!(a.claim_key(&[]), b.claim_key(&[]));
    }
    #[test]
    fn the_filename_discriminator_is_deliberately_unkeyed() {
        // Defect 1 (round 9): the discriminator was keyed by a per-directory
        // salt, but FNV-1a's per-byte step is invertible over its 64-bit state,
        // so two tags from one directory recover a salt-equivalent state in about
        // 2^32 work — and a directory listing supplies two filenames by
        // construction. The keying is gone rather than left as a claim that does
        // not hold; this test pins that it is reproducible by anyone, so the docs
        // must say a secret in an identity is recoverable from a listing.
        let identity = LogIdentity::new("m")
            .with_file(Path::new("m.yaml"))
            .with_cell(coords(&[("word", "1234")]));
        assert_eq!(
            identity.name_discriminator(),
            fnv1a_hex(identity.dedup_key().as_bytes()),
            "the discriminator is a plain digest of the identity, by design"
        );
    }

    #[test]
    fn the_discriminator_still_separates_identically_masked_cells() {
        // Dropping the keying must not cost the property the discriminator
        // exists for: two cells whose axis values both mask to `***` still need
        // different filenames.
        let cell = |word: &str| {
            LogIdentity::new("m")
                .with_file(Path::new("m.yaml"))
                .with_cell(coords(&[("word", word)]))
        };
        assert_ne!(
            cell("alpha").name_discriminator(),
            cell("bravo").name_discriminator()
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_hard_linked_sidecar_is_refused_rather_than_adopted() {
        // Defect 3 (round 9): the mode was repaired by *name* before the open, so
        // a `.x.log.claim` hard-linked to a file outside `logs/` had that external
        // inode chmod-ed to 0600. Nothing is chmod-ed before an open now, and a
        // multiply-linked inode is refused outright.
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("logs");
        std::fs::create_dir_all(&log_dir).unwrap();

        let outside = dir.path().join("victim.txt");
        std::fs::write(&outside, b"not pitty's").unwrap();
        std::fs::set_permissions(&outside, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::hard_link(&outside, log_dir.join(".s.log.claim")).unwrap();

        let handle = open_log_dir(&anchor_for(dir.path()), &log_dir).unwrap();
        let want = ClaimRecord {
            key: "k".to_string(),
            token: "t".to_string(),
        };
        assert!(
            handle.adopt_existing_sidecar("s.log", &want).is_none(),
            "a hard-linked sidecar must not be adopted"
        );

        let mode = std::fs::metadata(&outside).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o644, "the external inode must not be chmod-ed");
        assert_eq!(
            std::fs::read_to_string(&outside).unwrap(),
            "not pitty's",
            "the external inode must not be written"
        );
    }

    #[cfg(unix)]
    #[test]
    fn two_files_whose_stems_both_mask_keep_separate_logs() {
        // Defect 1 (round 10): the name search scanned the WHOLE directory for
        // any log with a matching masked claim key. Two scenario *files* whose
        // stems both mask to `___` share that key, so one was handed the other's
        // log and destroyed it — the #36 class this scheme exists to eliminate.
        // The search is restricted to the run's own stem now.
        let _guard = LOG_MEMO_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();

        // Each file's stem IS its own secret, so both mask to `___` and their
        // claim keys are byte-identical; only the discriminator separates them.
        let alpha = LogIdentity::new("shared").with_file(Path::new("alpha.yaml"));
        let bravo = LogIdentity::new("shared").with_file(Path::new("bravo.yaml"));
        let a_secrets = vec!["alpha".to_string()];
        let b_secrets = vec!["bravo".to_string()];
        assert_eq!(
            alpha.claim_key(&a_secrets),
            bravo.claim_key(&b_secrets),
            "this test is only meaningful when the claim keys collide"
        );

        clear_written_log_paths();
        write_log(
            dir.path(),
            &anchor_for(dir.path()),
            &bravo,
            "FROM-BRAVO",
            &demo_report(),
            None,
            &b_secrets,
        )
        .unwrap();
        clear_written_log_paths();
        write_log(
            dir.path(),
            &anchor_for(dir.path()),
            &alpha,
            "FROM-ALPHA",
            &demo_report(),
            None,
            &a_secrets,
        )
        .unwrap();

        let names = log_names(dir.path());
        assert_eq!(names.len(), 2, "each file needs its own log: {names:?}");

        // And each must hold its OWN output — the failure mode was one file
        // silently receiving the other's content.
        let bodies: Vec<String> = names
            .iter()
            .map(|n| std::fs::read_to_string(dir.path().join("logs").join(n)).unwrap())
            .collect();
        assert!(
            bodies.iter().any(|b| b.contains("FROM-ALPHA")),
            "alpha's output must survive: {bodies:?}"
        );
        assert!(
            bodies.iter().any(|b| b.contains("FROM-BRAVO")),
            "bravo's output must survive: {bodies:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_long_gap_does_not_orphan_the_log_we_own() {
        // The owned-log scan was bounded at a few consecutive gaps to save
        // syscalls. That was wrong at any length: with enough collisions the log
        // sits past the bound, the scan stops short, and a freed earlier name is
        // claimed instead — re-creating the orphan the scan exists to prevent.
        // Nine squatters put the log at `s.10.log`, well past any short bound.
        let _guard = LOG_MEMO_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("logs");
        std::fs::create_dir_all(&log_dir).unwrap();
        let identity = LogIdentity::new("s");

        std::fs::write(log_dir.join("s.log"), "squat").unwrap();
        for n in 2..=9 {
            std::fs::write(log_dir.join(format!("s.{n}.log")), "squat").unwrap();
        }
        clear_written_log_paths();
        write_log(
            dir.path(),
            &anchor_for(dir.path()),
            &identity,
            "first",
            &demo_report(),
            None,
            &[],
        )
        .unwrap();
        assert!(
            log_dir.join("s.10.log").exists(),
            "the run must be suffixed past all nine squatters"
        );

        // Every squatter disappears, leaving a nine-name gap before ours.
        std::fs::remove_file(log_dir.join("s.log")).unwrap();
        for n in 2..=9 {
            std::fs::remove_file(log_dir.join(format!("s.{n}.log"))).unwrap();
        }
        clear_written_log_paths();
        write_log(
            dir.path(),
            &anchor_for(dir.path()),
            &identity,
            "second",
            &demo_report(),
            None,
            &[],
        )
        .unwrap();

        assert_eq!(
            log_names(dir.path()),
            vec!["s.10.log"],
            "the run must find its own log across the gap, not claim a freed name"
        );
        let body = std::fs::read_to_string(log_dir.join("s.10.log")).unwrap();
        assert!(body.contains("second"), "the owned log must be updated");
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_sidecar_is_never_reclaimed() {
        // `read_claim` folded "cannot read" into the same `None` as "does not
        // parse", and every `None` was treated as reclaimable residue. A *valid*
        // sidecar an older pitty left at `0200` is unreadable but its `O_WRONLY`
        // open succeeds, so a colliding identity rewrote it and overwrote the
        // live log behind it. Unknown ownership must never be adopted.
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("logs");
        std::fs::create_dir_all(&log_dir).unwrap();
        let sidecar = log_dir.join(".s.log.claim");

        // A well-formed record belonging to somebody else, left unreadable.
        let theirs = ClaimRecord {
            key: "someone-else".to_string(),
            token: "theirs".to_string(),
        };
        std::fs::write(&sidecar, theirs.encode()).unwrap();
        std::fs::write(log_dir.join("s.log"), "THEIR LOG").unwrap();
        std::fs::set_permissions(&sidecar, std::fs::Permissions::from_mode(0o200)).unwrap();

        let handle = open_log_dir(&anchor_for(dir.path()), &log_dir).unwrap();
        let state = read_claim_state(&handle, "s.log");
        // Root bypasses mode bits, so the read succeeds there and the state is
        // `Record` rather than `Unreadable`. Both outcomes are correct — what
        // must never happen is `Malformed`, which is the one that would let this
        // sidecar be adopted and the log behind it overwritten.
        assert!(
            !matches!(state, ClaimRead::Malformed),
            "an unreadable-or-valid sidecar must never be classed as residue: {state:?}"
        );
        let mine = ClaimRecord {
            key: "mine".to_string(),
            token: "tok".to_string(),
        };
        assert!(
            !claim_is_reclaimable(&read_claim_state(&handle, "s.log"), &mine, true),
            "unknown ownership must not be reclaimable"
        );
        assert!(
            handle.adopt_existing_sidecar("s.log", &mine).is_none(),
            "an unreadable sidecar must not be adopted"
        );

        std::fs::set_permissions(&sidecar, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            std::fs::read_to_string(log_dir.join("s.log")).unwrap(),
            "THEIR LOG",
            "the log behind it must be untouched"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_malformed_sidecar_is_still_distinguished_from_an_unreadable_one() {
        // The guard against over-correction: genuine residue must stay
        // reclaimable, or an interrupted rewrite orphans its log again.
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("logs");
        std::fs::create_dir_all(&log_dir).unwrap();
        std::fs::write(log_dir.join(".s.log.claim"), "half-written").unwrap();

        let handle = open_log_dir(&anchor_for(dir.path()), &log_dir).unwrap();
        assert!(matches!(
            read_claim_state(&handle, "s.log"),
            ClaimRead::Malformed
        ));
        let mine = ClaimRecord {
            key: "mine".to_string(),
            token: "tok".to_string(),
        };

        // REVERSED (round 14). This previously asserted that a malformed record
        // is reclaimable outright, on the reasoning that refusing it would strand
        // the log behind it forever. That was right about stranding but wrong
        // about the balance: a malformed record carries no ownership
        // information, so "an interrupted run of this identity" and "another
        // identity's log" are indistinguishable, and adopting one with a log
        // present is a coin flip that destroys diagnostics when it lands wrong.
        // Stranding is recoverable — the file and its contents survive, and the
        // run merely takes a suffixed name; overwriting is not. So the rule now
        // turns on whether a log sits beside it.
        assert!(
            claim_is_reclaimable(&read_claim_state(&handle, "s.log"), &mine, false),
            "residue with no log behind it is plainly abandoned and reclaimable"
        );
        assert!(
            !claim_is_reclaimable(&read_claim_state(&handle, "s.log"), &mine, true),
            "residue with a log behind it must be left alone, not adopted"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_freed_preferred_name_does_not_orphan_the_log_we_own() {
        // A foreign file occupying `s.log` pushes this run's log to `s.2.log`.
        // When that occupant later disappears, a "first acquirable name wins"
        // search would take the newly-freed `s.log` and never look at `s.2.log`
        // — leaving the identity owning two logs, both carrying its claim key,
        // with the older one orphaned forever.
        //
        // The search looks for a log we already own *before* taking any free
        // name, so the run stays on `s.2.log`. Staying put is deliberate:
        // ownership stability is the property that matters, and migrating the
        // file would be racy against a concurrent reader for no functional gain.
        let _guard = LOG_MEMO_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("logs");
        std::fs::create_dir_all(&log_dir).unwrap();
        let identity = LogIdentity::new("s");

        // A squatter with no sidecar: not ours, so it must be suffixed past.
        std::fs::write(log_dir.join("s.log"), "squatter").unwrap();
        clear_written_log_paths();
        write_log(
            dir.path(),
            &anchor_for(dir.path()),
            &identity,
            "first",
            &demo_report(),
            None,
            &[],
        )
        .unwrap();
        assert_eq!(
            log_names(dir.path()),
            vec!["s.2.log", "s.log"],
            "the squatter must be stepped past"
        );

        // The squatter goes away, freeing the preferred name.
        std::fs::remove_file(log_dir.join("s.log")).unwrap();
        clear_written_log_paths();
        write_log(
            dir.path(),
            &anchor_for(dir.path()),
            &identity,
            "second",
            &demo_report(),
            None,
            &[],
        )
        .unwrap();

        let names = log_names(dir.path());
        assert_eq!(
            names,
            vec!["s.2.log"],
            "the run must reuse the log it owns, not claim the freed name: {names:?}"
        );
        let body = std::fs::read_to_string(log_dir.join("s.2.log")).unwrap();
        assert!(body.contains("second"), "the owned log must be updated");

        // And it must stay there: no oscillation between the two names.
        clear_written_log_paths();
        write_log(
            dir.path(),
            &anchor_for(dir.path()),
            &identity,
            "third",
            &demo_report(),
            None,
            &[],
        )
        .unwrap();
        assert_eq!(
            log_names(dir.path()),
            vec!["s.2.log"],
            "ownership must be stable"
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_claim_lock_excludes_any_writer_of_the_same_log_name() {
        // Pins the property that makes the `log_present` test safe to act on
        // later: the sidecar name is a pure function of the log name, so *any*
        // identity wanting that log must take the same lock. While one is held,
        // no other acquisition of that name can succeed — which is what keeps the
        // presence test and the eventual truncate inside one critical section.
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("logs");
        std::fs::create_dir_all(&log_dir).unwrap();
        let handle = open_log_dir(&anchor_for(dir.path()), &log_dir).unwrap();

        assert_eq!(
            claim_sidecar_name("x.log"),
            ".x.log.claim",
            "the sidecar must be derived from the log name alone"
        );

        let held = handle
            .try_acquire(
                "x.log",
                &ClaimRecord {
                    key: "mine".to_string(),
                    token: "t1".to_string(),
                },
            )
            .expect("first acquisition must win");

        // A *different* identity — the one that could otherwise create the log
        // inside the window — cannot acquire the name while the lock is held.
        assert!(
            handle
                .try_acquire(
                    "x.log",
                    &ClaimRecord {
                        key: "theirs".to_string(),
                        token: "t2".to_string(),
                    },
                )
                .is_none(),
            "a foreign identity must be excluded while the lock is held"
        );

        drop(held);
    }

    #[cfg(unix)]
    #[test]
    fn a_malformed_sidecar_does_not_let_another_identity_take_the_log() {
        // The sequence this rule exists for. Two scenario files whose names
        // sanitize to the same stem (`a?` and `a*` both become `a_`) share a
        // claim name. A's log exists. A re-run of A is killed between truncating
        // its sidecar and writing the record, leaving a malformed one beside a
        // live log. B then runs.
        //
        // Adopting that record would truncate A's log — B and A are
        // indistinguishable from a record carrying no ownership information. B
        // must suffix past instead.
        let _guard = LOG_MEMO_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();

        let a = LogIdentity::new("shared").with_file(Path::new("a?.yaml"));
        let b = LogIdentity::new("shared").with_file(Path::new("a*.yaml"));
        assert_eq!(
            a.stem(&[]),
            b.stem(&[]),
            "this test is only meaningful when the stems collide"
        );

        clear_written_log_paths();
        write_log(
            dir.path(),
            &anchor_for(dir.path()),
            &a,
            "FROM-A",
            &demo_report(),
            None,
            &[],
        )
        .unwrap();
        let a_log = log_names(dir.path())
            .into_iter()
            .next()
            .expect("A must have written a log");

        // A is killed mid-rewrite: the log stays, the record does not.
        std::fs::write(dir.path().join("logs").join(format!(".{a_log}.claim")), "").unwrap();

        clear_written_log_paths();
        write_log(
            dir.path(),
            &anchor_for(dir.path()),
            &b,
            "FROM-B",
            &demo_report(),
            None,
            &[],
        )
        .unwrap();

        let preserved = std::fs::read_to_string(dir.path().join("logs").join(&a_log)).unwrap();
        assert!(
            preserved.contains("FROM-A"),
            "A's log must survive B's run:\n{preserved}"
        );
        let names = log_names(dir.path());
        assert_eq!(names.len(), 2, "B must write its own file: {names:?}");
    }

    #[cfg(unix)]
    #[test]
    fn an_interrupted_rewrite_leaves_the_existing_log_alone() {
        // HISTORY, because this test reversed. Round 10 found that a run
        // interrupted while rewriting an EXISTING record left an unparseable
        // sidecar beside a live log, and that log was then refused forever — so
        // this test originally asserted the record must be *reclaimed*.
        //
        // Round 14 found the cost of that: a malformed record carries no
        // ownership information, so with a log present "an interrupted run of
        // this identity" and "another identity's log" are indistinguishable.
        // Reclaiming is a coin flip, and losing it truncates diagnostics that
        // exist nowhere else. Stranding is recoverable — the file survives, the
        // run takes a suffixed name, and deleting the stale file restores the
        // preferred one — so the safe direction is to suffix past.
        //
        // What the original defect was really about is still guaranteed, by the
        // sibling test: residue with NO log behind it is reclaimed, so an
        // interrupted run that never produced a log does not poison its name.
        let _guard = LOG_MEMO_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let identity = LogIdentity::new("s");

        clear_written_log_paths();
        write_log(
            dir.path(),
            &anchor_for(dir.path()),
            &identity,
            "first",
            &demo_report(),
            None,
            &[],
        )
        .unwrap();

        for partial in ["tok", "", "tok\n5\nab"] {
            std::fs::write(dir.path().join("logs/.s.log.claim"), partial).unwrap();
            clear_written_log_paths();
            write_log(
                dir.path(),
                &anchor_for(dir.path()),
                &identity,
                "later",
                &demo_report(),
                None,
                &[],
            )
            .unwrap();

            // The original log must survive untouched...
            let original = std::fs::read_to_string(dir.path().join("logs/s.log")).unwrap();
            assert!(
                original.contains("first"),
                "partial record {partial:?} must not let anything overwrite the log"
            );
            // ...and the run lands on a suffixed name instead.
            assert!(
                log_names(dir.path()).len() > 1,
                "the run must suffix past a log it cannot prove it owns"
            );

            // Reset for the next shape.
            for name in log_names(dir.path()) {
                if name != "s.log" {
                    let _ = std::fs::remove_file(dir.path().join("logs").join(&name));
                    let _ = std::fs::remove_file(
                        dir.path().join("logs").join(format!(".{name}.claim")),
                    );
                }
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_log_with_no_sidecar_at_all_is_still_left_alone() {
        // The guard against over-correction: reclaiming an *unparseable* record
        // must not also adopt a log that has no record at all — an older pitty's,
        // or one a user dropped in. Those still belong to nobody and are
        // suffixed past.
        let _guard = LOG_MEMO_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("logs");
        std::fs::create_dir_all(&log_dir).unwrap();
        std::fs::write(log_dir.join("s.log"), "LEGACY\n").unwrap();

        clear_written_log_paths();
        write_log(
            dir.path(),
            &anchor_for(dir.path()),
            &LogIdentity::new("s"),
            "mine",
            &demo_report(),
            None,
            &[],
        )
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(log_dir.join("s.log")).unwrap(),
            "LEGACY\n",
            "a log with no sidecar must be preserved untouched"
        );
        assert!(log_names(dir.path()).contains(&"s.2.log".to_string()));
    }

    #[cfg(unix)]
    #[test]
    fn residue_from_an_interrupted_run_never_blocks_a_later_one() {
        // Defect 2 (round 9): failure paths used to unlink the sidecar by *name*,
        // which is unsound — the name can be swapped between the stat and the
        // unlink, and inode reuse makes even a (dev, ino) comparison unreliable;
        // the held `flock` is on the old inode and does not prevent a rename.
        // Nothing is unlinked now, which is safe precisely because residue never
        // blocks anyone: an unparseable record with no log beside it is reclaimed.
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("logs");
        std::fs::create_dir_all(&log_dir).unwrap();

        // Every shape an interrupted or failed run can leave behind.
        for residue in ["", "garbage", "tok\n"] {
            std::fs::write(log_dir.join(".s.log.claim"), residue).unwrap();
            clear_written_log_paths();

            write_log(
                dir.path(),
                &anchor_for(dir.path()),
                &LogIdentity::new("s"),
                "out",
                &demo_report(),
                None,
                &[],
            )
            .unwrap_or_else(|e| panic!("residue {residue:?} must not block a run: {e}"));

            assert_eq!(
                log_names(dir.path()),
                vec!["s.log"],
                "residue {residue:?} must be reclaimed, not stepped past"
            );
            std::fs::remove_file(log_dir.join("s.log")).unwrap();
        }
    }

    #[cfg(unix)]
    #[test]
    fn one_directory_reached_two_ways_shares_its_bookkeeping() {
        // Defect 4 (round 8): the process-local maps were keyed by `PathBuf`, so
        // one physical `logs/` reached through a real path and a symlink alias
        // looked like two directories. The second view started with an empty
        // "already bound" set, after which two identities with equal masked keys
        // could adopt each other's log.
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir_all(real.join("logs")).unwrap();
        let alias = dir.path().join("alias");
        std::os::unix::fs::symlink(&real, &alias).unwrap();

        let via_real = open_log_dir(&anchor_for(&real), &real.join("logs")).unwrap();
        let via_alias = open_log_dir(&anchor_for(&alias), &alias.join("logs")).unwrap();

        assert_eq!(
            via_real.identity_key(),
            via_alias.identity_key(),
            "the same directory reached two ways must key identically"
        );
    }

    #[test]
    fn a_long_identity_records_a_complete_claim() {
        // Defect 1 (round 7): a fixed 256-byte read truncated the record of any
        // long identity, so a re-run no longer recognized its own log. The
        // sidecar here exceeds that old cap.
        let identity = LogIdentity::new("m")
            .with_file(Path::new("m.yaml"))
            .with_cell(coords(&[("word", &"x".repeat(250))]));
        let record = ClaimRecord {
            key: identity.claim_key(&[]),
            token: "tok".to_string(),
        };
        let encoded = record.encode();
        assert!(
            encoded.len() > 256,
            "this test is only meaningful past the old cap: {}",
            encoded.len()
        );
        assert_eq!(
            ClaimRecord::decode(&encoded).as_ref(),
            Some(&record),
            "a complete record must round-trip however long it is"
        );
    }

    #[test]
    fn a_truncated_claim_is_rejected_rather_than_mis_parsed() {
        // The forging path: length-prefixing is only unambiguous over a COMPLETE
        // record, so a cut-off two-axis key could look exactly like a complete
        // one-axis key — letting one scenario adopt and overwrite another's log.
        let record = ClaimRecord {
            key: LogIdentity::new("m")
                .with_file(Path::new("m.yaml"))
                .with_cell(coords(&[("a", &"y".repeat(220)), ("b", "short")]))
                .claim_key(&[]),
            token: "tok".to_string(),
        };
        let encoded = record.encode();

        // Every prefix of a real record must be refused, at every cut point.
        for cut in 1..encoded.len() {
            assert!(
                ClaimRecord::decode(&encoded[..cut]).is_none(),
                "a record truncated at {cut} must not parse"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_long_identity_reuses_one_log_across_runs() {
        // The behavioural form: three writes of a long identity must share one
        // log, where the truncated record used to produce three.
        let _guard = LOG_MEMO_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let identity = LogIdentity::new("m")
            .with_file(Path::new("m.yaml"))
            .with_cell(coords(&[("word", &"x".repeat(250))]));

        for _ in 0..3 {
            clear_written_log_paths();
            write_log(
                dir.path(),
                &anchor_for(dir.path()),
                &identity,
                "out",
                &demo_report(),
                None,
                &[],
            )
            .unwrap();
        }
        let names = log_names(dir.path());
        assert_eq!(
            names.len(),
            1,
            "a long identity must reuse its log: {names:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn identically_masked_cells_are_interchangeable_across_processes() {
        // Defect 2 (round 7), pinned as a NARROWED guarantee rather than a fix.
        //
        // Two cells whose axis values differ only inside a secret mask to the
        // same claim key, and nothing on disk distinguishes them — binding them
        // would need a value reproducible from the raw identity yet not
        // invertible by someone holding `logs/`, which is impossible for a
        // low-entropy secret. What IS guaranteed: they never collide onto one
        // file, and the set of logs is stable across runs.
        let _guard = LOG_MEMO_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let secrets = vec!["alpha".to_string(), "bravo".to_string()];
        let cell = |word: &str| {
            LogIdentity::new("m")
                .with_file(Path::new("m.yaml"))
                .with_cell(coords(&[("word", word)]))
        };

        // Their stored keys are identical: that is the design, not a bug.
        assert_eq!(
            cell("alpha").claim_key(&secrets),
            cell("bravo").claim_key(&secrets),
            "identically-masked cells are indistinguishable on disk by design"
        );

        for word in ["alpha", "bravo"] {
            write_log(
                dir.path(),
                &anchor_for(dir.path()),
                &cell(word),
                word,
                &demo_report(),
                None,
                &secrets,
            )
            .unwrap();
        }
        let first = log_names(dir.path());
        assert_eq!(first.len(), 2, "they must not collide: {first:?}");

        // A later process re-runs them and must not grow the set.
        clear_written_log_paths();
        for word in ["bravo", "alpha"] {
            write_log(
                dir.path(),
                &anchor_for(dir.path()),
                &cell(word),
                word,
                &demo_report(),
                None,
                &secrets,
            )
            .unwrap();
        }
        assert_eq!(
            log_names(dir.path()),
            first,
            "the set of logs must be stable across processes"
        );
    }

    #[test]
    fn the_claim_record_is_not_recomputable_from_identity() {
        // Defect 1 (round 6): the sidecar held an unsalted digest of the RAW
        // identity, so anyone holding a `logs/` archive could hash 0000-9999
        // against it and recover a PIN used as a matrix axis. Salting it did not
        // help either — the salt is a dotfile in that same archive.
        //
        // What is stored now is the MASKED identity, which contains no secret at
        // all, so there is nothing to brute-force.
        let secrets = vec!["1234".to_string()];
        let identity = LogIdentity::new("m")
            .with_file(Path::new("m.yaml"))
            .with_cell(coords(&[("word", "1234")]));

        let stored = identity.claim_key(&secrets);
        assert!(
            !stored.contains("1234"),
            "the stored claim must not contain the secret: {stored}"
        );

        // The decisive property: an attacker who knows everything *except* the
        // secret, and tries every candidate, never reproduces a distinguishing
        // value — because the stored form is identical for all of them.
        let guess = |pin: &str| {
            LogIdentity::new("m")
                .with_file(Path::new("m.yaml"))
                .with_cell(coords(&[("word", pin)]))
                .claim_key(&[pin.to_string()])
        };
        assert_eq!(
            guess("1234"),
            guess("9999"),
            "brute-forcing candidates must not distinguish the real secret"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_write_keeps_a_sidecar_this_run_did_not_create() {
        // Defect 3 (round 6): the cleanup removed the sidecar unconditionally, so
        // a run that REUSED an existing log and then failed destroyed the
        // pre-existing claim — after which the next run refused that log as
        // unclaimed and orphaned it.
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let identity = LogIdentity::new("s");

        // First run establishes the log and its claim.
        write_log(
            dir.path(),
            &anchor_for(dir.path()),
            &identity,
            "first",
            &demo_report(),
            None,
            &[],
        )
        .unwrap();
        let log_dir = dir.path().join("logs");
        let sidecar = log_dir.join(".s.log.claim");
        assert!(sidecar.exists());
        clear_written_log_paths();

        // Second run reuses it, then fails to write: the log file itself is made
        // unwritable, so the reuse succeeds and only the content write fails.
        let log_path = log_dir.join("s.log");
        std::fs::set_permissions(&log_path, std::fs::Permissions::from_mode(0o400)).unwrap();
        let result = write_log(
            dir.path(),
            &anchor_for(dir.path()),
            &identity,
            "second",
            &demo_report(),
            None,
            &[],
        );
        std::fs::set_permissions(&log_path, std::fs::Permissions::from_mode(0o600)).unwrap();

        assert!(result.is_err(), "the write must fail");
        assert!(
            sidecar.exists(),
            "a reused sidecar must survive this run's failure"
        );
    }

    #[test]
    fn the_claim_tag_does_not_depend_on_secret_values() {
        // CORRECTED: this test previously asserted that registering a value as a
        // secret must CHANGE the tag — the old design, where secrets were mixed
        // into the digest to stop the tag from equalling a secret. That made the
        // tag an offline oracle (an unsalted digest over low-entropy input,
        // published in the filename, is brute-forced in milliseconds) and made a
        // rotated credential silently orphan a scenario's log.
        //
        // The tag is now identity-only, and protection from masking comes from
        // storing it outside the masked region instead. So the property to hold
        // is the opposite one: two runs differing ONLY in secret value must
        // produce the SAME tag.
        let identity = LogIdentity::new("s");
        let with_pin = identity.claim_key(&[]);
        let with_other = identity.claim_key(&[]);
        assert_eq!(with_pin, with_other);

        // And the tag must carry no secret material at all: the same identity
        // under wildly different secret lists is the same tag.
        let dir = tempfile::tempdir().unwrap();
        write_log(
            dir.path(),
            &anchor_for(dir.path()),
            &identity,
            "out",
            &demo_report(),
            None,
            &["1234".to_string()],
        )
        .unwrap();
        assert_eq!(
            claim_in(dir.path(), "s.log", &[]).as_deref(),
            Some(&*tag_in(dir.path(), &identity)),
            "the recorded tag must be the identity-only tag"
        );
    }

    #[test]
    fn a_secret_equal_to_the_digest_still_allows_cross_process_reuse() {
        // Both horns at once: no leak *and* a re-run still finds its own log.
        let _guard = LOG_MEMO_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let identity = LogIdentity::new("s");
        let secrets = vec![identity.claim_key(&[]), "id".to_string()];

        for _ in 0..3 {
            clear_written_log_paths();
            write_log(
                dir.path(),
                &anchor_for(dir.path()),
                &identity,
                "out",
                &demo_report(),
                None,
                &secrets,
            )
            .unwrap();
        }
        assert_eq!(log_names(dir.path()), vec!["s.log"]);
    }

    #[test]
    fn a_secret_in_a_matrix_axis_value_never_reaches_the_filename() {
        // Cell coordinates are identity material this scheme put into the
        // filename, so a secret appearing in one would be readable from a plain
        // directory listing.
        let dir = tempfile::tempdir().unwrap();
        let identity = LogIdentity::new("m")
            .with_file(Path::new("m.yaml"))
            .with_cell(coords(&[("command", "prod")]));
        write_log(
            dir.path(),
            &anchor_for(dir.path()),
            &identity,
            "out",
            &demo_report(),
            None,
            &["prod".to_string()],
        )
        .unwrap();

        let names = log_names(dir.path());
        assert!(
            !names.iter().any(|n| n.contains("prod")),
            "a secret must not reach the filename: {names:?}"
        );
    }

    #[test]
    fn a_secret_in_the_file_stem_never_reaches_the_filename() {
        // The same exposure through the other component this scheme added.
        let dir = tempfile::tempdir().unwrap();
        let identity = LogIdentity::new("plain").with_file(Path::new("prod-suite.yaml"));
        write_log(
            dir.path(),
            &anchor_for(dir.path()),
            &identity,
            "out",
            &demo_report(),
            None,
            &["prod".to_string()],
        )
        .unwrap();

        let names = log_names(dir.path());
        assert!(
            !names.iter().any(|n| n.contains("prod")),
            "a secret must not reach the filename: {names:?}"
        );
    }

    #[test]
    fn cells_differing_only_inside_a_secret_keep_separate_logs() {
        // Masking is many-to-one, so two cells differing only inside secret text
        // both reduce to the same visible stem. The discriminator must keep them
        // on separate files — collapsing them would reintroduce the #31 overwrite
        // class that this whole scheme exists to prevent.
        let dir = tempfile::tempdir().unwrap();
        let secrets = vec!["alpha".to_string(), "bravo".to_string()];
        for word in ["alpha", "bravo"] {
            let identity = LogIdentity::new("m")
                .with_file(Path::new("m.yaml"))
                .with_cell(coords(&[("word", word)]));
            write_log(
                dir.path(),
                &anchor_for(dir.path()),
                &identity,
                word,
                &demo_report(),
                None,
                &secrets,
            )
            .unwrap();
        }

        let names = log_names(dir.path());
        assert_eq!(names.len(), 2, "the two cells must not collide: {names:?}");
        assert!(!names
            .iter()
            .any(|n| n.contains("alpha") || n.contains("bravo")));
    }

    #[test]
    fn a_filename_is_untouched_when_no_secret_is_involved() {
        // The discriminator is spent only when masking actually removed text, so
        // an ordinary scenario keeps its plain, readable log name.
        let identity = LogIdentity::new("echo-flow").with_file(Path::new("echo-flow.yaml"));
        assert_eq!(identity.stem(&["unrelated".to_string()]), "echo-flow");
    }

    #[cfg(unix)]
    #[test]
    fn a_dangling_symlink_candidate_is_not_written_through() {
        // `Path::exists()` follows links, so a dangling link read as "free" and
        // the writer then created the link's *target* — putting the full terminal
        // output outside `logs/` entirely.
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("logs");
        std::fs::create_dir_all(&log_dir).unwrap();
        let outside = dir.path().join("victim.txt");
        std::os::unix::fs::symlink(&outside, log_dir.join("s.log")).unwrap();

        write_log(
            dir.path(),
            &anchor_for(dir.path()),
            &LogIdentity::new("s"),
            "sensitive terminal output",
            &demo_report(),
            None,
            &[],
        )
        .unwrap();

        assert!(
            !outside.exists(),
            "nothing may be written through the planted link"
        );
        let real = std::fs::read_to_string(log_dir.join("s.2.log"))
            .expect("the log must land on a suffixed name instead");
        assert!(real.contains("sensitive terminal output"));
    }

    #[cfg(unix)]
    #[test]
    fn an_empty_base_dir_is_normalized_to_the_current_directory() {
        // `Path::new("s.yaml").parent()` is `Some("")`, not `None`, so the CLI's
        // `unwrap_or_else(|| ".")` never fires for a bare filename and an empty
        // base reaches the log writer. An empty path cannot be opened, which used
        // to fail the anchor capture and silently disable containment.
        assert_eq!(normalize_base_dir(Path::new("")), Path::new("."));
        assert_eq!(normalize_base_dir(Path::new("dir")), Path::new("dir"));
    }

    #[cfg(unix)]
    #[test]
    fn an_empty_base_dir_still_captures_an_anchor() {
        // The regression in one assertion: an empty base must still yield a
        // usable anchor, because an anchorless one now refuses to log at all.
        let anchor = LogAnchor::capture(Path::new(""));
        assert!(
            anchor.root_fd.is_some(),
            "an empty base must resolve to the current directory"
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_anchorless_run_refuses_to_log_rather_than_writing_unprotected() {
        // The fail-open branch that was the real hole: with no anchor the writer
        // used to fall back to a path-based `create_dir_all` + path write, which
        // followed a symlinked `logs/`. It must refuse instead — writing through
        // an unprotected path while SECURITY.md promises containment is the one
        // outcome that is never acceptable.
        let dir = tempfile::tempdir().unwrap();
        let elsewhere = dir.path().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, dir.path().join("logs")).unwrap();

        let err = write_log(
            dir.path(),
            &LogAnchor::default(),
            &LogIdentity::new("s"),
            "out",
            &demo_report(),
            None,
            &[],
        )
        .expect_err("an anchorless run must refuse to log");
        assert_eq!(err.exit_code(), 3);
        assert!(
            err.to_string().contains("cannot secure"),
            "the error must say containment was unavailable: {err}"
        );
        assert_eq!(
            std::fs::read_dir(&elsewhere).unwrap().count(),
            0,
            "nothing may be written through the linked directory"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_logs_directory_is_refused() {
        // The per-file O_NOFOLLOW cannot see this: by the time the file is
        // opened, the directory component has already been resolved.
        //
        // NOTE: this covers only the case where the anchor was captured
        // successfully. It passed while a real escape existed, because it hands
        // `write_log` an *absolute* base. `an_empty_base_dir_still_refuses_a_
        // symlinked_logs_directory` below covers the branch it missed.
        let dir = tempfile::tempdir().unwrap();
        let elsewhere = dir.path().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, dir.path().join("logs")).unwrap();

        let err = write_log(
            dir.path(),
            &anchor_for(dir.path()),
            &LogIdentity::new("s"),
            "out",
            &demo_report(),
            None,
            &[],
        )
        .expect_err("a symlinked logs/ must be refused");
        assert_eq!(err.exit_code(), 3);
        assert_eq!(
            std::fs::read_dir(&elsewhere).unwrap().count(),
            0,
            "nothing may be written through the linked directory"
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_existing_log_is_never_observable_at_0644_with_new_content() {
        // The mode must be restrictive on the descriptor that receives the
        // content, not applied after the write. `mode(0o600)` on open covers only
        // creation, so a log that already exists at 0644 — what a pre-fix pitty
        // wrote — kept 0644 while the new terminal output landed in it.
        //
        // This observes the *window* rather than the end state: a polling thread
        // records a violation if it ever sees group/other bits set at a moment
        // when the file already holds part of the new content.
        use std::os::unix::fs::PermissionsExt;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("logs");
        std::fs::create_dir_all(&log_dir).unwrap();
        let path = log_dir.join("wide.log");
        std::fs::write(&path, b"old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let violated = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let watcher = {
            let (path, violated, stop) = (path.clone(), violated.clone(), stop.clone());
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    if let Ok(meta) = std::fs::metadata(&path) {
                        let mode = meta.permissions().mode() & 0o777;
                        let has_new_content = meta.len() > 3;
                        if has_new_content && mode & 0o077 != 0 {
                            violated.store(true, Ordering::Relaxed);
                        }
                    }
                }
            })
        };

        // A payload large enough that a pre-fix write spans many poll iterations.
        let big = "x".repeat(4 * 1024 * 1024);
        write_private_file(&path, big.as_bytes()).unwrap();

        stop.store(true, Ordering::Relaxed);
        watcher.join().unwrap();

        assert!(
            !violated.load(Ordering::Relaxed),
            "the log was group/other-readable while new content was in it"
        );
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn exhausting_every_candidate_name_reports_rather_than_overwrites() {
        // Past the suffix bound the writer used to return the *unsuffixed* path
        // even though another identity held it, destroying that log. Exhaustion
        // must be a reported failure instead.
        let _guard = LOG_MEMO_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let log_dir = dir.path().join("logs");
        std::fs::create_dir_all(&log_dir).unwrap();

        // Occupy every candidate with a log claimed by somebody else.
        std::fs::write(log_dir.join("taken.log"), "# id: 0000000000000000\nfirst\n").unwrap();
        for n in 2..=MAX_LOG_SUFFIX {
            std::fs::write(
                log_dir.join(format!("taken.{n}.log")),
                "# id: 0000000000000000\n",
            )
            .unwrap();
        }

        clear_written_log_paths();
        let err = write_log(
            dir.path(),
            &anchor_for(dir.path()),
            &LogIdentity::new("taken"),
            "mine",
            &demo_report(),
            None,
            &[],
        )
        .expect_err("exhausting every name must be reported, not resolved by overwriting");
        assert_eq!(err.exit_code(), 3);

        let first = std::fs::read_to_string(log_dir.join("taken.log")).unwrap();
        assert!(
            first.contains("first"),
            "the occupied log must survive untouched:\n{first}"
        );
    }

    #[test]
    fn a_log_write_failure_is_reported_rather_than_swallowed() {
        // write_log must return the error so the caller can surface it; the
        // runner's warning depends on this being an Err rather than a silent Ok.
        // `logs` is occupied by a *file*, so the directory cannot be created.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("logs"), b"not a directory").unwrap();

        let err = write_log(
            dir.path(),
            &anchor_for(dir.path()),
            &LogIdentity::new("blocked"),
            "out",
            &demo_report(),
            None,
            &[],
        )
        .expect_err("a log that cannot be written must report the failure");
        assert_eq!(err.exit_code(), 3, "a write fault is the process class");
    }

    #[test]
    fn a_colliding_identity_still_suffixes_past_a_claimed_file() {
        // The cross-process reuse must not become "overwrite whatever is there":
        // a *different* identity landing on the same stem still needs its own
        // file, with the memo cleared just as a separate process would have it.
        let _guard = LOG_MEMO_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        write_log(
            dir.path(),
            &anchor_for(dir.path()),
            &LogIdentity::new("my/test"),
            "first",
            &demo_report(),
            None,
            &[],
        )
        .unwrap();

        clear_written_log_paths();

        write_log(
            dir.path(),
            &anchor_for(dir.path()),
            &LogIdentity::new("my_test"),
            "second",
            &demo_report(),
            None,
            &[],
        )
        .unwrap();

        assert_eq!(log_names(dir.path()), vec!["my_test.2.log", "my_test.log"]);
        let first = std::fs::read_to_string(dir.path().join("logs/my_test.log")).unwrap();
        assert!(first.contains("first"), "the earlier log must survive");
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
            &anchor_for(dir.path()),
            &LogIdentity::new("sec"),
            "token=supersecret done",
            &report,
            None,
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
