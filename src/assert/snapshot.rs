//! `expect_snapshot` assertion: compare current PTY output against a recorded
//! snapshot file, with an opt-in `--update` flow to record or refresh it.
//!
//! By default the output is ANSI-stripped (so cursor moves and color codes do
//! not make snapshots terminal-dependent); `raw: true` compares the bytes
//! verbatim. Diffs use `similar` so a mismatch reports a readable unified diff.

// Only the tests name `Path` directly now: both the read and the write
// traverse descriptors and component names, and the non-Unix paths go through
// `SnapshotTarget::display_path`.
#[cfg(test)]
use std::path::Path;

#[cfg(unix)]
use crate::safepath;
use crate::workspace::SnapshotTarget;

/// Outcome of an `expect_snapshot` evaluation.
pub struct SnapshotResult {
    /// Whether the assertion held (a fresh record under `--update` passes).
    pub passed: bool,
    /// On failure, the unified diff or reason; on a record, a note. `None` only
    /// when a silent pass (an exact match) occurred.
    pub message: Option<String>,
}

impl SnapshotResult {
    fn pass() -> Self {
        SnapshotResult {
            passed: true,
            message: None,
        }
    }
    fn pass_with(message: impl Into<String>) -> Self {
        SnapshotResult {
            passed: true,
            message: Some(message.into()),
        }
    }
    fn fail(message: impl Into<String>) -> Self {
        SnapshotResult {
            passed: false,
            message: Some(message.into()),
        }
    }
}

/// Compare `output` against the snapshot at `target`, recording when `update`.
///
/// `target` comes from [`crate::workspace::Workspace::resolve_write_path`],
/// which is the only place snapshot containment is decided. It carries the
/// component sequence *as containment validated it* together with the workspace
/// directory descriptor captured before the scenario's child was spawned, so
/// the recorder walks a path that was actually checked, starting from the
/// directory object the run began in. See [`write_snapshot`].
///
/// `raw` selects byte-verbatim comparison; otherwise `output` is ANSI-stripped
/// first. Behavior:
/// - file absent, `update` false  -> fail ("not recorded; rerun with --update")
/// - file absent, `update` true   -> write `expected`, pass ("recorded")
/// - file present, equal          -> pass
/// - file present, differ, no upd -> fail with a unified diff
/// - file present, differ, update -> overwrite, pass ("updated")
///
/// Why fail-on-absent instead of silently recording: in CI a brand-new
/// snapshot has never been reviewed, so auto-creating it would let any output
/// pass on first run. Requiring an explicit `--update` keeps the "snapshot was
/// approved by a human" invariant (user-approved policy).
pub fn check(output: &str, target: &SnapshotTarget, raw: bool, update: bool) -> SnapshotResult {
    let path = target.display_path();
    let expected = if raw {
        output.to_string()
    } else {
        strip_ansi(output)
    };

    // Read through the *same* protected traversal the write uses, not through
    // `display` — which is a label, not a capability. Reading by path resolved
    // symlinks the write half refuses, so the two halves could name different
    // objects: with `snapshots/` swapped for a link to an attacker-controlled
    // directory, a planted `out.snap` whose contents matched the output made
    // the assertion PASS while the real workspace held no snapshot at all.
    // Routing both halves through one traversal is what makes "the snapshot
    // this scenario is talking about" a single, unambiguous file.
    let existing = read_snapshot(target);

    let Some(actual) = existing else {
        if !update {
            return SnapshotResult::fail(format!(
                "snapshot {} not recorded; rerun with --update to create it",
                path.display()
            ));
        }
        return match write_snapshot(target, &expected) {
            Ok(()) => SnapshotResult::pass_with(format!("recorded snapshot {}", path.display())),
            Err(e) => SnapshotResult::fail(e),
        };
    };

    if actual == expected {
        return SnapshotResult::pass();
    }

    if update {
        return match write_snapshot(target, &expected) {
            Ok(()) => SnapshotResult::pass_with(format!("updated snapshot {}", path.display())),
            Err(e) => SnapshotResult::fail(e),
        };
    }

    SnapshotResult::fail(format!(
        "snapshot {} mismatch:\n{}",
        path.display(),
        unified_diff(&actual, &expected)
    ))
}

/// Write `contents` to `path`, creating parent directories as needed.
///
/// Why we do not mask secrets in the written bytes: a snapshot is, by design, a
/// faithful record of the program's real output; masking it would make the
/// comparison meaningless (a later run's unmasked output could never match a
/// masked file). The README warns that snapshot files may contain secrets and
/// should be treated/`.gitignore`d accordingly.
///
/// Because the bytes are unmasked, the file is `0600` on Unix before any
/// content reaches it — the same restriction `report::write_log` applies to
/// logs, whose content is *masked* and therefore strictly less sensitive than
/// this. Directories *this function creates* are created `0700` for the same
/// reason: under the default umask they would otherwise be world-listable.
///
/// Why only directories we create: an already-existing parent belongs to the
/// user, not to pitty. For the common `file: out.snap` the parent is the
/// scenario's own directory — the user's repository checkout — and tightening
/// that to `0700` would lock a group-shared `0770` checkout away from other
/// users and from later CI steps, besides failing outright (and so failing the
/// recording) in a directory that is writable but not owned.
///
/// See [`write_snapshot_unix`] for how the walk is bounded.
fn write_snapshot(target: &SnapshotTarget, contents: &str) -> Result<(), String> {
    write_snapshot_impl(target, contents.as_bytes()).map_err(|e| {
        format!(
            "cannot write snapshot {}: {e}",
            target.display_path().display()
        )
    })
}

#[cfg(unix)]
fn write_snapshot_impl(target: &SnapshotTarget, bytes: &[u8]) -> std::io::Result<()> {
    write_snapshot_unix(target, bytes)
}

/// Record a snapshot by descending from the workspace directory descriptor one
/// component at a time, refusing to follow a symlink at any step, and writing
/// through a descriptor opened relative to the parent directory's own fd.
///
/// # Why `openat` per component rather than a path-based open
///
/// `O_NOFOLLOW` on a single path-based `open` constrains only the *final*
/// component. The workspace resolver's symlink scan
/// ([`crate::workspace::Workspace::resolve_write_path`]) runs earlier and
/// against the whole path, so between the two the scenario's own child process
/// — which runs concurrently — can replace an *intermediate* directory
/// (`snapshots/`) with a symlink out of the workspace. A subsequent
/// `open("snapshots/x.snap", O_NOFOLLOW)` happily traverses that link and lands
/// the bytes outside.
///
/// Descending with `openat(dirfd, component, O_DIRECTORY | O_NOFOLLOW)` closes
/// that race: each handle names a directory *object*, not a path, so a rename
/// or symlink swap after a component is opened cannot change which directory
/// the next step operates on, and a component that *is* a symlink at the moment
/// we reach it fails with `ELOOP` instead of being followed.
///
/// # Why the walk starts at a descriptor, not the workspace path
///
/// Anchoring on the workspace *path* was itself escapable: the resolver
/// re-resolves that path at snapshot time, so a child that runs
/// `mv "$PWD" "$PWD.old" && ln -s /tmp/victim "$PWD"` makes the same path name
/// a different directory, and the write follows it to `/tmp/victim`. The
/// premise "nothing at or above the workspace is plantable" holds for the path
/// but not for what the path *resolves to* later. `target.root_fd()` is opened
/// once in `Workspace::prepare`, before any child exists, and a descriptor
/// cannot be retargeted by a later rename or symlink swap.
///
/// Components *above* the workspace directory are never traversed at all now —
/// the walk begins inside it — which also sidesteps the macOS reality that a
/// temp workspace is routinely reached through `/var -> /private/var`.
///
/// # Why the component list comes from the resolver
///
/// `target.components()` is the sequence containment actually validated:
/// workspace-relative, normalized, and free of `..`. Walking the *raw* `file:`
/// components instead was escapable even when containment passed, because
/// `../new-private-dir/../w/out.snap` normalizes back inside the workspace
/// while its raw form steps outside on the way and would `mkdirat` there.
#[cfg(unix)]
fn write_snapshot_unix(target: &SnapshotTarget, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::io::FromRawFd;

    // Attempt every directory the `file:` value named that `..` then cancelled,
    // BEFORE the write, and let the kernel decide whether it can exist.
    //
    // These do not appear in `components` — `missing/../victim.snap` normalizes
    // to `victim.snap` — so without this the path's own `missing/` would never
    // be touched by anything. 1.2.2 called `create_dir_all` on the raw path, so
    // it *did* create it, and failed with EACCES on a workspace that is not
    // writable. Reproducing v1 means reproducing both halves, and attempting the
    // creation is what removes the prediction that four rounds kept getting
    // wrong: a writable tree succeeds, a read-only one fails with the kernel's
    // own error, and nothing here guesses which.
    create_traversed_dirs(target)?;

    let (dir, file_name) = open_parent_dir(target, safepath::CreateDirs::Yes)?;

    // O_NOFOLLOW: if `file_name` is a symlink planted after the resolver's scan,
    // fail with ELOOP rather than writing through it. mode 0600 applies only
    // when this open *creates* the file.
    let fd = safepath::openat_file_write_no_follow(dir.as_raw(), file_name, 0o600)?;
    // SAFETY: `fd` is a freshly opened, owned descriptor that nothing else holds.
    let mut file = unsafe { std::fs::File::from_raw_fd(fd.into_raw()) };

    // BEFORE any content reaches the descriptor. `mode(0o600)` on the open
    // covers only creation, so a snapshot that already exists keeps whatever
    // mode it had — including the 0644 a pre-fix pitty recorded. Writing first
    // and chmod-ing afterwards would leave the *new* secret-bearing content
    // readable by every local user for the length of the write. `fchmod` on the
    // descriptor (not the path) cannot be redirected by a concurrent swap.
    safepath::fchmod_file(&file, 0o600)?;
    // Truncate after the mode is restrictive, for the same reason: an existing
    // file must not be observable at its old permissive mode with new content.
    file.set_len(0)?;
    file.write_all(bytes)
}

/// Non-Unix fallback: create parent directories and write.
///
/// Windows has no `O_NOFOLLOW`/`openat` equivalent on `OpenOptions` and no mode
/// bits, so snapshots there rely on the workspace resolver's symlink refusal
/// and on the runner user's default file ACLs — the same position
/// `report::write_log` takes for logs.
#[cfg(not(unix))]
fn write_snapshot_impl(target: &SnapshotTarget, bytes: &[u8]) -> std::io::Result<()> {
    // `safe_path`, never `display_path`. The latter is the `file:` value as
    // written, and a value like `../outside/new/../../w/out.snap` normalizes
    // back inside the workspace — so it rightly passes containment — while its
    // raw form steps outside on the way. `create_dir_all` on that raw path
    // created directories *outside the workspace* even though the snapshot
    // itself landed inside; this is the same `..` escape the Unix traversal
    // fixed by walking the validated components, applied to the branch that has
    // no `openat` to walk with.
    // Same rule as the Unix branch: attempt each directory the path named and
    // `..` cancelled, so whether it can exist is decided by the filesystem
    // rather than predicted. Built from the workspace root plus the validated,
    // `..`-free sequence, never from the raw `file:` string.
    for seq in target.traversed_dirs() {
        let mut dir = target.root().to_path_buf();
        dir.extend(seq.iter());
        std::fs::create_dir_all(&dir)?;
    }

    let path = target.safe_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, bytes)
}

/// Create each directory the `file:` value names but `..` cancels.
///
/// Uses the same anchored, component-by-component traversal as the write, so a
/// cancelled component cannot be used to create anything outside the workspace:
/// the sequences come from the resolver already normalized and `..`-free, and
/// each is walked from the pre-spawn descriptor.
///
/// The trailing element of each sequence is the directory to create, so the
/// walk creates the parents and `mkdirat` handles the last one — which is why
/// `CreateDirs::Yes` is passed rather than the directory being made by hand.
#[cfg(unix)]
fn create_traversed_dirs(target: &SnapshotTarget) -> std::io::Result<()> {
    use std::os::unix::io::AsRawFd;

    for seq in target.traversed_dirs() {
        // A component that the kernel can ALREADY traverse needs no creation,
        // and must not be walked as if it did.
        //
        // `walk_from` opens every component `O_NOFOLLOW`, which is right for the
        // snapshot's own path but wrong here: a cancelled component that is a
        // **symlink to a real directory** (`dl -> d`, `file: dl/../out.snap`) is
        // refused with ENOTDIR by that walk, while 1.2.2 resolved it happily —
        // `create_dir_all` follows symlinks, and `create_dir_all("dl/..")`
        // returns Ok (measured). Refusing it stopped `--update` recording at all
        // and told the user to rerun with the flag they had just used.
        //
        // What v1's `create_dir_all` on the raw path actually yields for each
        // shape, measured rather than inferred:
        //
        //   dl/..       (symlink to a directory) -> Ok        record
        //   missing/..  (absent)                 -> Ok        record, creating it
        //   blocker/..  (regular file)           -> ENOTDIR   refuse
        //   locked/..   (mode 0000)              -> EACCES    refuse
        //
        // The last two never reach here — the traversability guard in the
        // resolver refuses them earlier, with the same verdict. So the only work
        // left is the absent case, and asking the kernel whether the component is
        // already traversable is what separates the two: if it is, there is
        // nothing to create and nothing to check `O_NOFOLLOW` against, because
        // this component is not part of the path any byte is written through.
        let mut probe = target.root().to_path_buf();
        probe.extend(seq.iter());
        if crate::safepath::traversability(&probe) == crate::safepath::Traversability::Traversable {
            continue;
        }

        // `walk_from` treats the last element as a file name and returns its
        // parent, so append a placeholder to have every real component created.
        let mut with_leaf = seq.clone();
        with_leaf.push(std::ffi::OsString::from(".keep"));
        match safepath::walk_from(
            target.root_fd().as_raw_fd(),
            &with_leaf,
            safepath::CreateDirs::Yes,
        ) {
            Ok(_) => {}
            // A genuine refusal — EACCES on a read-only workspace above all — is
            // exactly the verdict v1 surfaced and must not be swallowed.
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Walk `target`'s validated components from the pre-spawn workspace descriptor
/// and return the parent directory's descriptor plus the file name.
///
/// A thin adapter over [`crate::safepath::walk_from`], which owns the reusable
/// traversal; this function's only job is to supply the anchor descriptor and
/// component list out of a [`SnapshotTarget`].
///
/// Both halves of [`check`] go through here — the comparison read and the
/// `--update` write. Sharing one traversal is the point, not an incidental
/// tidy-up: when the read went by path and only the write was protected, the
/// two could resolve to *different files*, and an attacker who planted a
/// matching snapshot behind a swapped directory made the assertion pass
/// against a file the workspace did not contain.
#[cfg(unix)]
fn open_parent_dir(
    target: &SnapshotTarget,
    create: safepath::CreateDirs,
) -> std::io::Result<(safepath::Fd, &std::ffi::OsStr)> {
    use std::os::unix::io::AsRawFd;
    safepath::walk_from(target.root_fd().as_raw_fd(), target.components(), create)
}

/// Read the recorded snapshot through the protected traversal.
///
/// `None` means "no snapshot this scenario can legitimately see": the file does
/// not exist, a directory on the way does not exist, a component is a symlink
/// (refused, not followed), or the bytes are not UTF-8. All of those reach the
/// caller the same way, as "not recorded" — which is the safe direction: an
/// unreadable or suspicious path must never be able to *satisfy* an assertion,
/// only to fail it or trigger a record.
#[cfg(unix)]
fn read_snapshot(target: &SnapshotTarget) -> Option<String> {
    use std::io::Read;
    use std::os::unix::io::FromRawFd;

    let (dir, file_name) = open_parent_dir(target, safepath::CreateDirs::No).ok()?;
    let fd = safepath::openat_file_read_no_follow(dir.as_raw(), file_name).ok()?;
    // SAFETY: `fd` is a freshly opened, owned descriptor that nothing else holds.
    let mut file = unsafe { std::fs::File::from_raw_fd(fd.into_raw()) };
    let mut buf = String::new();
    file.read_to_string(&mut buf).ok()?;
    Some(buf)
}

/// Non-Unix read: a plain path read, matching the unprotected write there.
#[cfg(not(unix))]
fn read_snapshot(target: &SnapshotTarget) -> Option<String> {
    // The read must name the same file the write does, so it uses the validated
    // path too — otherwise the two halves could disagree about which file the
    // scenario is talking about, the defect fixed on Unix in an earlier round.
    std::fs::read_to_string(target.safe_path()).ok()
}

/// Render a unified diff between the recorded and current snapshot text.
fn unified_diff(recorded: &str, current: &str) -> String {
    let diff = similar::TextDiff::from_lines(recorded, current);
    diff.unified_diff()
        .header("recorded", "current")
        .to_string()
}

/// Strip ANSI control sequences from `text`, leaving printable content.
///
/// Handles the families a terminal program commonly emits:
/// - CSI sequences `ESC [ ... <final>` (cursor moves, SGR color, erase) whose
///   final byte is in the range `0x40..=0x7e`.
/// - OSC sequences `ESC ] ... <terminator>` (window title, hyperlinks)
///   terminated by BEL (`0x07`) or ST (`ESC \`).
/// - SS3 sequences `ESC O <final>` (three bytes): the application-keypad-mode
///   responses for arrow/function keys (`ESC O A` for Up, etc.). Without this
///   the `ESC O` is dropped as a two-byte escape but the final byte (`A`) leaks
///   into the snapshot, making a TUI snapshot terminal-mode-dependent.
///
/// A bare `ESC` followed by any other byte drops the two-byte escape.
///
/// Why a state machine rather than a regex: terminal escape grammar is not a
/// single regular shape — OSC has two possible terminators (BEL or the
/// two-byte ST) and CSI has a variable parameter run before its final byte. A
/// small explicit state machine encodes those terminators precisely and never
/// risks a catastrophic-backtracking pattern, while staying readable.
///
/// Why not normalize carriage-return overwrites (`50%\r100%` -> `100%`): doing
/// so correctly means modeling per-line cursor-column overwrite (the second
/// write replaces only the columns it covers, leaving any longer tail of the
/// first write visible), which is materially more complex than escape removal
/// and easy to get subtly wrong. v0.2 deliberately keeps both pre- and post-CR
/// content (CR is preserved verbatim) and documents this in the README so the
/// behavior is predictable; a faithful last-write-wins normalization is deferred.
///
/// Why not handle 8-bit C1 controls (e.g. a lone `0x9b` as CSI): on the UTF-8
/// terminals pitty targets, `0x9b` is a continuation byte of a multibyte
/// character far more often than a real C1 CSI introducer, so treating it as a
/// control would corrupt legitimate text. Programs emit the 7-bit `ESC [` form
/// in practice, which is handled above. 8-bit C1 support is deferred to v0.3.
pub fn strip_ansi(text: &str) -> String {
    #[derive(PartialEq)]
    enum State {
        /// Normal text, copying bytes through.
        Text,
        /// Just saw ESC; the next byte selects the sequence kind.
        Escape,
        /// Inside `ESC [ ... <final>`; consume until the final byte.
        Csi,
        /// Inside `ESC ] ... `; consume until BEL or ST.
        Osc,
        /// Inside an OSC and just saw an ESC, expecting `\` to complete ST.
        OscEsc,
        /// Saw `ESC O`; the single next byte is the SS3 final byte to drop.
        Ss3,
    }

    let mut out = String::with_capacity(text.len());
    let mut state = State::Text;
    for ch in text.chars() {
        match state {
            State::Text => {
                if ch == '\x1b' {
                    state = State::Escape;
                } else {
                    out.push(ch);
                }
            }
            State::Escape => match ch {
                '[' => state = State::Csi,
                ']' => state = State::Osc,
                // SS3: `ESC O` introduces a three-byte single-shift sequence
                // whose next byte is the final; consume that byte too.
                'O' => state = State::Ss3,
                // Any other byte after ESC is a short two-byte escape we drop
                // wholesale; return to text without emitting it.
                _ => state = State::Text,
            },
            State::Csi => {
                // CSI ends at a final byte in 0x40..=0x7e; parameter and
                // intermediate bytes before it are consumed.
                if ('\u{40}'..='\u{7e}').contains(&ch) {
                    state = State::Text;
                }
            }
            State::Osc => match ch {
                '\x07' => state = State::Text, // BEL terminator
                '\x1b' => state = State::OscEsc,
                _ => {}
            },
            State::OscEsc => {
                // ST is the two-byte ESC `\`; any other byte after ESC means the
                // OSC was malformed, but we still leave the OSC either way.
                state = State::Text;
            }
            State::Ss3 => {
                // The SS3 final byte (e.g. `A` for Up) is consumed; back to text.
                state = State::Text;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    /// A [`SnapshotTarget`] for `path` (absolute, under `root`), bypassing the
    /// resolver.
    ///
    /// These tests deliberately construct targets the resolver would have
    /// refused: their whole purpose is to show the *write* layer is safe on its
    /// own, against links planted after resolution has already passed. Going
    /// through the resolver would make that unprovable, because it would reject
    /// the setup before the writer ever ran.
    fn target(root: &Path, path: &Path) -> SnapshotTarget {
        let rel = path.strip_prefix(root).expect("test paths live under root");
        SnapshotTarget::for_test(root, rel)
    }

    #[test]
    fn strips_csi_color_and_cursor_sequences() {
        // SGR color codes and cursor moves (CSI) must be removed, leaving text.
        let input = "\x1b[31mred\x1b[0m and \x1b[2Kcleared";
        assert_eq!(strip_ansi(input), "red and cleared");
    }

    #[test]
    fn strips_osc_with_bel_and_st_terminators() {
        // OSC sequences (window title / hyperlink) terminated by BEL or ST must
        // be removed entirely.
        let bel = "before\x1b]0;title\x07after";
        assert_eq!(strip_ansi(bel), "beforeafter");
        let st = "a\x1b]8;;http://x\x1b\\link\x1b]8;;\x1b\\b";
        assert_eq!(strip_ansi(st), "alinkb");
    }

    #[test]
    fn leaves_plain_text_untouched() {
        // Text with no escapes must pass through unchanged.
        assert_eq!(strip_ansi("plain line\nsecond"), "plain line\nsecond");
    }

    #[test]
    fn strips_ss3_application_keypad_sequences() {
        // (R5) SS3 (`ESC O <final>`, the application-keypad arrow/function-key
        // responses) must be fully removed, including the final byte, so a TUI
        // snapshot does not leak a stray `A`/`B`/`H` and become mode-dependent.
        let input = "before\x1bOAmiddle\x1bOHend";
        assert_eq!(strip_ansi(input), "beforemiddleend");
    }

    #[test]
    fn preserves_carriage_return_overwrites() {
        // (R5) CR-overwrite is intentionally NOT normalized in v0.2: both the
        // pre- and post-CR content (and the CR itself) are preserved verbatim.
        // This pins the documented behavior so a future last-write-wins
        // normalization is a deliberate, tested change rather than an accident.
        assert_eq!(strip_ansi("50%\r100%"), "50%\r100%");
    }

    #[test]
    fn absent_snapshot_without_update_fails() {
        // A missing snapshot file with no --update must fail and tell the author
        // to rerun with --update, never silently pass.
        let dir = tempdir().unwrap();
        let path = dir.path().join("missing.snap");
        let r = check("output", &target(dir.path(), &path), false, false);
        assert!(!r.passed);
        assert!(r.message.unwrap().contains("--update"));
        assert!(!path.exists(), "must not create the file without --update");
    }

    #[test]
    fn absent_snapshot_with_update_records_and_passes() {
        // With --update, a missing snapshot is recorded (ANSI-stripped) and the
        // assertion passes, creating parent dirs as needed.
        let dir = tempdir().unwrap();
        let path = dir.path().join("__snapshots__/out.snap");
        let r = check(
            "\x1b[32mhello\x1b[0m",
            &target(dir.path(), &path),
            false,
            true,
        );
        assert!(r.passed);
        assert!(r.message.unwrap().contains("recorded"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello");
    }

    #[test]
    fn matching_snapshot_passes() {
        // An exact match against the recorded (stripped) content passes silently.
        let dir = tempdir().unwrap();
        let path = dir.path().join("out.snap");
        std::fs::write(&path, "hello").unwrap();
        let r = check(
            "\x1b[1mhello\x1b[0m",
            &target(dir.path(), &path),
            false,
            false,
        );
        assert!(r.passed);
        assert!(r.message.is_none());
    }

    #[test]
    fn mismatching_snapshot_fails_with_diff() {
        // A mismatch must fail and include a unified diff of recorded vs current.
        let dir = tempdir().unwrap();
        let path = dir.path().join("out.snap");
        std::fs::write(&path, "old line\n").unwrap();
        let r = check("new line\n", &target(dir.path(), &path), false, false);
        assert!(!r.passed);
        let msg = r.message.unwrap();
        assert!(msg.contains("old line") && msg.contains("new line"));
        assert!(msg.contains("@@") || msg.contains("---"));
    }

    #[test]
    fn update_overwrites_mismatch_and_passes() {
        // Under --update a mismatch overwrites the file with current output and
        // passes, so authors can refresh a snapshot deliberately.
        let dir = tempdir().unwrap();
        let path = dir.path().join("out.snap");
        std::fs::write(&path, "stale").unwrap();
        let r = check("fresh", &target(dir.path(), &path), false, true);
        assert!(r.passed);
        assert!(r.message.unwrap().contains("updated"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "fresh");
    }

    #[cfg(unix)]
    #[test]
    fn recorded_snapshot_is_not_world_readable() {
        // (#33) Snapshot contents are written *unmasked* and may contain
        // secrets, so a recorded snapshot must be 0600 — the same restriction
        // logs get, whose content is masked and therefore less sensitive. It
        // must not land at the umask default (0644 on a typical runner), where
        // any other local user could read it. The parent directory pitty
        // creates must likewise be 0700, not world-listable.
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().unwrap();
        let path = dir.path().join("__snapshots__/secret.snap");
        let r = check("token=supersecret", &target(dir.path(), &path), false, true);
        assert!(r.passed);

        let file_mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(file_mode, 0o600, "snapshot file must be 0600");
        let dir_mode = std::fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(dir_mode, 0o700, "snapshot dir must be 0700");
    }

    #[cfg(unix)]
    #[test]
    fn updating_an_existing_snapshot_keeps_it_private() {
        // (#33) The 0600 guarantee must hold on the overwrite path too, not
        // only on first record: a snapshot refreshed under --update is the same
        // class of unmasked data.
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().unwrap();
        let path = dir.path().join("out.snap");
        std::fs::write(&path, "stale").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let r = check("fresh", &target(dir.path(), &path), false, true);
        assert!(r.passed);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "fresh");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "an updated snapshot must not stay 0644");
    }

    #[cfg(unix)]
    #[test]
    fn write_refuses_to_follow_a_symlink_planted_after_resolution() {
        // (#37) TOCTOU: the workspace resolver rejects a symlinked snapshot
        // path, but the scenario's own child process runs concurrently and can
        // plant the link *after* that check. The write itself must therefore
        // refuse to follow a final-component symlink (O_NOFOLLOW), so the bytes
        // cannot be redirected out of the workspace in that window. This calls
        // `check` directly — bypassing the resolver — precisely to prove the
        // write is independently safe.
        let dir = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let victim = outside.path().join("PWNED.txt");
        let path = dir.path().join("planted.snap");
        std::os::unix::fs::symlink(&victim, &path).unwrap();

        let r = check("escaped-write", &target(dir.path(), &path), false, true);
        assert!(!r.passed, "a symlinked snapshot write must fail");
        assert!(!victim.exists(), "nothing may be written through the link");
    }

    #[test]
    fn raw_mode_compares_bytes_verbatim() {
        // raw: true must compare without stripping, so an escape that differs
        // makes the comparison fail.
        let dir = tempdir().unwrap();
        let path = dir.path().join("raw.snap");
        std::fs::write(&path, "\x1b[31mred\x1b[0m").unwrap();
        // Same visible text but different escapes: stripped would match, raw
        // must not.
        let r = check(
            "\x1b[32mred\x1b[0m",
            &target(dir.path(), &path),
            true,
            false,
        );
        assert!(!r.passed);
        // The identical raw bytes must match.
        let ok = check(
            "\x1b[31mred\x1b[0m",
            &target(dir.path(), &path),
            true,
            false,
        );
        assert!(ok.passed);
    }

    #[cfg(unix)]
    #[test]
    fn recording_leaves_an_existing_directorys_mode_alone() {
        // (A) pitty may tighten only a directory it created itself. An existing
        // parent belongs to the user: for the common `file: out.snap` it is the
        // scenario's own directory — the repository checkout — so chmod-ing it
        // to 0700 would lock a group-shared 0770 checkout away from other users
        // and from later CI steps. Pre-create the parent at 0755 and require the
        // recording to leave it exactly there.
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().unwrap();
        let parent = dir.path().join("existing");
        std::fs::create_dir(&parent).unwrap();
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o755)).unwrap();

        let path = parent.join("out.snap");
        let r = check("recorded", &target(dir.path(), &path), false, true);
        assert!(r.passed, "recording must succeed: {:?}", r.message);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "recorded");

        let mode = std::fs::metadata(&parent).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755, "an existing directory's mode is the user's");
        // The file itself is still pitty's to protect.
        let file_mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(file_mode, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn recording_succeeds_in_a_writable_directory_whose_mode_cannot_be_changed() {
        // (A) The unconditional chmod also *failed* — and so failed the whole
        // recording — in a directory that is writable but whose mode this
        // process may not change. `chmod` is permitted only to the owner, so a
        // non-owned directory is the real-world case; that needs a second uid,
        // which a unit test cannot arrange portably. We approximate it with the
        // property that actually matters: recording must not depend on being
        // able to chmod the parent at all. A read-only *parent of the parent*
        // makes a path-based retarget impossible while the target dir stays
        // writable, and the recording must still go through untouched.
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().unwrap();
        let outer = dir.path().join("outer");
        std::fs::create_dir(&outer).unwrap();
        let inner = outer.join("inner");
        std::fs::create_dir(&inner).unwrap();
        std::fs::set_permissions(&inner, std::fs::Permissions::from_mode(0o775)).unwrap();
        // 0555: no new names may be created or removed in `outer`.
        std::fs::set_permissions(&outer, std::fs::Permissions::from_mode(0o555)).unwrap();

        let path = inner.join("out.snap");
        let r = check("recorded", &target(dir.path(), &path), false, true);

        // Restore before any assertion can unwind, so tempdir cleanup works.
        std::fs::set_permissions(&outer, std::fs::Permissions::from_mode(0o755)).unwrap();

        assert!(r.passed, "recording must succeed: {:?}", r.message);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "recorded");
        let mode = std::fs::metadata(&inner).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o775, "the existing directory must be untouched");
    }

    #[cfg(unix)]
    #[test]
    fn an_existing_snapshot_is_never_observable_at_0644_with_new_content() {
        // (B) `mode(0o600)` on `open` applies only when the open *creates* the
        // file. A snapshot that already exists at 0644 — exactly what a pre-fix
        // pitty recorded, and exactly the case the #33 repair targets —
        // therefore kept 0644 while the new, secret-bearing content was being
        // written, and was chmod-ed to 0600 only afterwards. Any other local
        // user on a shared runner could read the new secret during that window.
        //
        // This test *observes the window* rather than only the end state: a
        // concurrent thread polls the file while the recording runs, and records
        // a violation if it ever sees a mode with group/other bits set at a
        // moment when the file already holds part of the new content. The
        // payload is large enough that the pre-fix write spans many poll
        // iterations, so the window is real rather than theoretical.
        //
        // What this proves: with the fixed ordering (fchmod -> set_len ->
        // write_all) no observer ever catches new bytes at a permissive mode.
        // What it does NOT prove: absence of a race in the formal sense. Polling
        // can only sample; a sufficiently fast machine could in principle miss a
        // narrow window. It is a regression detector — it reliably catches the
        // pre-fix ordering (see the assertion message if it ever fires) — not a
        // proof of atomicity. The actual guarantee rests on the syscall order in
        // `write_snapshot_unix`, where the mode is made restrictive on the
        // descriptor before a single byte of content or even the truncation
        // reaches it.
        use std::os::unix::fs::PermissionsExt;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let dir = tempdir().unwrap();
        let path = dir.path().join("existing.snap");
        // Pre-create at 0644 with content, as a pre-fix pitty would have left it.
        std::fs::write(&path, "stale").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        // Large enough that the write is not a single instantaneous syscall.
        let secret = "token=supersecret\n".repeat(200_000);

        let done = Arc::new(AtomicBool::new(false));
        let leaked = Arc::new(AtomicBool::new(false));

        let watcher = {
            let (path, done, leaked) = (path.clone(), done.clone(), leaked.clone());
            std::thread::spawn(move || {
                while !done.load(Ordering::Relaxed) {
                    let Ok(meta) = std::fs::metadata(&path) else {
                        continue;
                    };
                    let mode = meta.permissions().mode() & 0o777;
                    let is_permissive = mode & 0o077 != 0;
                    // "Holds new content" is detectable by size: the new payload
                    // is far larger than the 5-byte pre-existing content, so any
                    // size above that means new bytes have landed.
                    let holds_new_content = meta.len() > 5;
                    if is_permissive && holds_new_content {
                        leaked.store(true, Ordering::Relaxed);
                        return;
                    }
                }
            })
        };

        let r = check(&secret, &target(dir.path(), &path), false, true);
        done.store(true, Ordering::Relaxed);
        watcher.join().unwrap();

        assert!(r.passed, "{:?}", r.message);
        assert!(
            !leaked.load(Ordering::Relaxed),
            "new snapshot content was observable while the file was still \
             group/other-readable; the mode must be restrictive before any \
             content reaches the descriptor"
        );
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "an updated snapshot must not stay 0644");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), secret);
    }

    #[cfg(unix)]
    #[test]
    fn the_restricted_mode_lands_on_the_inode_that_receives_the_content() {
        // (B) The chmod must apply to the same object the bytes go into — the
        // reason the implementation uses `fchmod` on the open descriptor rather
        // than a path-based `set_permissions`, which a concurrent swap could
        // retarget.
        //
        // This once used a hard link as the measuring instrument, reading the
        // mode through a second name to prove it reached the same inode. That
        // setup is now itself refused (a multiply-linked snapshot is a
        // containment bypass, see `a_hard_linked_snapshot_neither_passes_nor_is_rewritten`),
        // so identity is established with the inode number instead: we record
        // which inode held the file before the write and require the bytes and
        // the 0600 mode to be on *that* inode afterwards.
        use std::os::unix::fs::MetadataExt;
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().unwrap();
        let path = dir.path().join("existing.snap");
        std::fs::write(&path, "old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let before = std::fs::metadata(&path).unwrap();
        let (dev, ino) = (before.dev(), before.ino());

        let r = check("secret", &target(dir.path(), &path), false, true);
        assert!(r.passed, "{:?}", r.message);

        let after = std::fs::metadata(&path).unwrap();
        assert_eq!(
            (after.dev(), after.ino()),
            (dev, ino),
            "the write must land in the existing inode, not replace it"
        );
        assert_eq!(
            after.permissions().mode() & 0o777,
            0o600,
            "the written inode itself must be 0600"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "secret");
        assert_eq!(
            after.nlink(),
            1,
            "the recorded snapshot must have exactly one name"
        );
    }

    #[cfg(unix)]
    #[test]
    fn write_refuses_an_intermediate_directory_swapped_for_a_symlink() {
        // (C) The real TOCTOU that `O_NOFOLLOW` on a single path-based open did
        // NOT close: that flag constrains only the final component, while the
        // workspace resolver's symlink scan runs earlier and against the whole
        // path. The scenario's own concurrent child can therefore wait out the
        // check and replace an *intermediate* directory with a link pointing
        // outside the workspace; `open("snapshots/x.snap", O_NOFOLLOW)` would
        // follow it and write outside.
        //
        // We simulate the post-check state directly (the link is already in
        // place when `check` runs, which is what the racing child achieves) and
        // require the recording to fail with nothing written through the link.
        let dir = tempdir().unwrap();
        let outside = tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("snapshots")).unwrap();

        let path = dir.path().join("snapshots/x.snap");
        let r = check("escaped-write", &target(dir.path(), &path), false, true);

        assert!(
            !r.passed,
            "a swapped intermediate directory must be refused"
        );
        assert!(
            !outside.path().join("x.snap").exists(),
            "nothing may be written through the intermediate link"
        );
    }

    #[cfg(unix)]
    #[test]
    fn write_does_not_chmod_a_directory_reached_through_a_symlink() {
        // (C) The companion hazard: before the fix the unconditional, path-based
        // `set_permissions` on the parent ran *before* the open, so a swapped
        // intermediate link let pitty chmod a directory outside the workspace to
        // 0700. The walk must refuse the component rather than chmod through it.
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let victim = outside.path().join("victim");
        std::fs::create_dir(&victim).unwrap();
        std::fs::set_permissions(&victim, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink(&victim, dir.path().join("snapshots")).unwrap();

        let path = dir.path().join("snapshots/x.snap");
        let r = check("escaped-write", &target(dir.path(), &path), false, true);
        assert!(!r.passed);

        let mode = std::fs::metadata(&victim).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755, "an external directory must not be chmod-ed");
    }

    #[cfg(unix)]
    #[test]
    fn directories_pitty_creates_are_still_private() {
        // (A/C) The narrowing must not lose the #33 guarantee: a directory that
        // pitty itself creates is still 0700, and so is every level of a nested
        // creation, so a freshly made `__snapshots__/` is never world-listable.
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().unwrap();
        let path = dir.path().join("__snapshots__/nested/out.snap");
        let r = check("secret", &target(dir.path(), &path), false, true);
        assert!(r.passed, "{:?}", r.message);

        for level in ["__snapshots__", "__snapshots__/nested"] {
            let mode = std::fs::metadata(dir.path().join(level))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o700, "{level} must be 0700");
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_renamed_and_symlinked_workspace_cannot_redirect_the_write() {
        // (Escape 2) Anchoring the traversal on the workspace *path* was itself
        // escapable. The path is re-resolved at snapshot time, so the
        // scenario's own child can run, in its own workspace:
        //
        //     mv "$PWD" "$PWD.old" && ln -s /tmp/victim "$PWD"
        //
        // and the same path name now denotes a different directory. Everything
        // downstream — containment and the write — then agrees on the attacker's
        // directory, and the snapshot lands in /tmp/victim. The premise "nothing
        // at or above the workspace is plantable" is true of the *path* but not
        // of what that path *resolves to* later.
        //
        // The fix captures the workspace as a descriptor in `Workspace::prepare`,
        // before any child exists. A descriptor names the directory object, so a
        // later rename plus symlink cannot retarget it.
        //
        // This test is deterministic, not racy: it performs the swap explicitly
        // *after* the target is built (which is what `prepare` does) and before
        // the write, which is exactly the attacker's ordering. What it proves is
        // that the write follows the captured descriptor rather than the path.
        // What it does not simulate is a swap landing *during* the traversal;
        // that case is covered by the per-component `openat` handles, since each
        // one already names an object rather than a name.
        let dir = tempdir().unwrap();
        let workspace = dir.path().join("w");
        std::fs::create_dir(&workspace).unwrap();
        let victim = dir.path().join("victim");
        std::fs::create_dir(&victim).unwrap();

        // Built while `w` is still the real workspace — the pre-spawn capture.
        let target = SnapshotTarget::for_test(&workspace, Path::new("out.snap"));

        // The child's swap: the workspace name now points at the victim dir.
        std::fs::rename(&workspace, dir.path().join("w.old")).unwrap();
        std::os::unix::fs::symlink(&victim, &workspace).unwrap();

        let r = check("secret", &target, false, true);
        assert!(r.passed, "the write must still succeed: {:?}", r.message);

        // The bytes must be in the ORIGINAL directory, now named `w.old`...
        let original = dir.path().join("w.old/out.snap");
        assert_eq!(
            std::fs::read_to_string(&original).unwrap(),
            "secret",
            "the write must follow the captured descriptor"
        );
        // ...and nothing may have been written into the attacker's target.
        assert!(
            !victim.join("out.snap").exists(),
            "a renamed-and-symlinked workspace must not redirect the write"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_planted_external_snapshot_cannot_satisfy_the_assertion() {
        // (Defect 2) The comparison read used the display path, which resolves
        // symlinks — the very thing the write half refuses. So the two halves
        // could name different files: with `snapshots/` swapped for a link to a
        // directory the attacker controls, a planted `x.snap` whose contents
        // matched the program's output made `expect_snapshot` PASS, while the
        // real workspace contained no snapshot at all. A passing assertion that
        // verified an attacker-supplied file is the worst possible outcome for
        // a test framework, strictly worse than a failure.
        //
        // Both halves now go through one traversal, so the read refuses the
        // swapped component exactly as the write does and the assertion cannot
        // be satisfied from outside the workspace.
        let dir = tempdir().unwrap();
        let outside = tempdir().unwrap();
        // The attacker pre-plants a snapshot whose content matches the output.
        std::fs::write(outside.path().join("x.snap"), "matching-output").unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("snapshots")).unwrap();

        let path = dir.path().join("snapshots/x.snap");
        // No --update: this is the plain comparison path, the one that decides
        // whether the scenario passes.
        let r = check("matching-output", &target(dir.path(), &path), false, false);

        assert!(
            !r.passed,
            "an assertion must never be satisfied by a file reached through a \
             planted symlink; the read must refuse it as the write does"
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_comparison_read_refuses_a_symlinked_final_component() {
        // (Defect 2) The final-component form of the same defect: `out.snap`
        // itself is a link to an attacker-controlled file with matching
        // content. Reading by path would follow it and pass.
        let dir = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let planted = outside.path().join("planted.snap");
        std::fs::write(&planted, "matching-output").unwrap();

        let path = dir.path().join("out.snap");
        std::os::unix::fs::symlink(&planted, &path).unwrap();

        let r = check("matching-output", &target(dir.path(), &path), false, false);
        assert!(
            !r.passed,
            "a symlinked snapshot must not satisfy a comparison"
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_ordinary_recorded_snapshot_still_compares_equal() {
        // (Defect 2, guard against over-correction) Routing the read through
        // the protected traversal must not break the ordinary case: a snapshot
        // recorded in the normal way, in a real subdirectory, still matches and
        // passes silently. Without this, a read that refused everything would
        // also satisfy the tests above.
        let dir = tempdir().unwrap();
        let path = dir.path().join("__snapshots__/out.snap");

        let recorded = check("hello", &target(dir.path(), &path), false, true);
        assert!(recorded.passed, "{:?}", recorded.message);

        // A second run with the same output must compare equal through the
        // protected read, with no message (a silent pass).
        let compared = check("hello", &target(dir.path(), &path), false, false);
        assert!(compared.passed, "{:?}", compared.message);
        assert!(compared.message.is_none(), "an exact match passes silently");

        // And a genuine mismatch must still fail with a diff, so the read is
        // really returning the recorded bytes rather than always None.
        let mismatch = check("goodbye", &target(dir.path(), &path), false, false);
        assert!(!mismatch.passed);
        assert!(mismatch.message.unwrap().contains("hello"));
    }

    #[cfg(unix)]
    #[test]
    fn a_hard_linked_snapshot_neither_passes_nor_is_rewritten() {
        // A hard link is not a symlink — it is the file under a second name —
        // so the per-component `openat(O_NOFOLLOW)` walk succeeds on one, and
        // containment was bypassed without any symlink. An attacker who links
        // an external `victim.snap` to `workspace/out.snap` got both halves:
        // the comparison read returned the external contents (so matching them
        // to the PTY output made the assertion PASS with no snapshot really in
        // the workspace), and `--update` truncated and rewrote that external
        // inode, chmod-ing it 0600.
        //
        // Both halves must now refuse. This asserts the security-relevant
        // outcomes directly: the assertion does not pass, and the external file
        // is byte-for-byte untouched.
        let dir = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let victim = outside.path().join("victim.snap");
        std::fs::write(&victim, "planted-output").unwrap();

        let path = dir.path().join("out.snap");
        if std::fs::hard_link(&victim, &path).is_err() {
            // Cross-device: the attack is unavailable here, nothing to prove.
            return;
        }

        // Read half: the planted content matches the output exactly, which is
        // what would have made this pass before the fix.
        let compared = check("planted-output", &target(dir.path(), &path), false, false);
        assert!(
            !compared.passed,
            "a hard-linked external snapshot must not satisfy an assertion"
        );

        // Write half: --update must not truncate or rewrite the external inode.
        let updated = check("new-secret", &target(dir.path(), &path), false, true);
        assert!(!updated.passed, "--update must refuse a hard-linked target");
        assert_eq!(
            std::fs::read_to_string(&victim).unwrap(),
            "planted-output",
            "the external file must not have been rewritten"
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_traversal_never_walks_a_parent_dir_component() {
        // (Escape 1, at the write layer) The recorder's own defense in depth.
        // Even handed a sequence containing `..` — which the resolver now makes
        // impossible, but which a future caller could reintroduce — the walk
        // must not create anything outside the root it was given. `openat` with
        // a literal ".." name would climb out, so the writer must never receive
        // one; here we assert the failure is contained rather than silently
        // escaping.
        let dir = tempdir().unwrap();
        let root = dir.path().join("w");
        std::fs::create_dir(&root).unwrap();

        let target = SnapshotTarget::for_test(&root, Path::new("../escaped/out.snap"));
        let r = check("escaped-write", &target, false, true);

        // Whether this is refused outright or fails on the open, the invariant
        // that matters is that nothing was created outside the workspace.
        assert!(
            !dir.path().join("escaped").exists(),
            "no directory may be created outside the workspace root"
        );
        assert!(!r.passed, "a parent-dir component must not be recorded");
    }
}
