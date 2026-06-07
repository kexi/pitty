//! `expect_snapshot` assertion: compare current PTY output against a recorded
//! snapshot file, with an opt-in `--update` flow to record or refresh it.
//!
//! By default the output is ANSI-stripped (so cursor moves and color codes do
//! not make snapshots terminal-dependent); `raw: true` compares the bytes
//! verbatim. Diffs use `similar` so a mismatch reports a readable unified diff.

use std::path::Path;

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

/// Compare `output` against the snapshot at `path`, recording when `update`.
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
pub fn check(output: &str, path: &Path, raw: bool, update: bool) -> SnapshotResult {
    let expected = if raw {
        output.to_string()
    } else {
        strip_ansi(output)
    };

    let existing = std::fs::read_to_string(path).ok();

    let Some(actual) = existing else {
        if !update {
            return SnapshotResult::fail(format!(
                "snapshot {} not recorded; rerun with --update to create it",
                path.display()
            ));
        }
        return match write_snapshot(path, &expected) {
            Ok(()) => SnapshotResult::pass_with(format!("recorded snapshot {}", path.display())),
            Err(e) => SnapshotResult::fail(e),
        };
    };

    if actual == expected {
        return SnapshotResult::pass();
    }

    if update {
        return match write_snapshot(path, &expected) {
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
fn write_snapshot(path: &Path, contents: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create snapshot dir {}: {e}", parent.display()))?;
    }
    std::fs::write(path, contents)
        .map_err(|e| format!("cannot write snapshot {}: {e}", path.display()))
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
        let r = check("output", &path, false, false);
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
        let r = check("\x1b[32mhello\x1b[0m", &path, false, true);
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
        let r = check("\x1b[1mhello\x1b[0m", &path, false, false);
        assert!(r.passed);
        assert!(r.message.is_none());
    }

    #[test]
    fn mismatching_snapshot_fails_with_diff() {
        // A mismatch must fail and include a unified diff of recorded vs current.
        let dir = tempdir().unwrap();
        let path = dir.path().join("out.snap");
        std::fs::write(&path, "old line\n").unwrap();
        let r = check("new line\n", &path, false, false);
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
        let r = check("fresh", &path, false, true);
        assert!(r.passed);
        assert!(r.message.unwrap().contains("updated"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "fresh");
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
        let r = check("\x1b[32mred\x1b[0m", &path, true, false);
        assert!(!r.passed);
        // The identical raw bytes must match.
        let ok = check("\x1b[31mred\x1b[0m", &path, true, false);
        assert!(ok.passed);
    }
}
