//! File-based assertions and the pre/post snapshot used by `expect_file_changed`.
//!
//! `expect_file_changed` needs a baseline: the file's contents at the moment
//! the process was spawned. We capture that snapshot lazily the first time a
//! changed-assertion path is seen for a session and compare against the
//! current contents at assertion time.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::bytes::find_bytes;

/// Tracks baseline file contents captured at (or before) spawn time so that
/// `expect_file_changed` can detect a difference later.
#[derive(Default)]
pub struct FileSnapshots {
    /// Path -> contents at snapshot time. A `None` value records that the file
    /// did not exist at snapshot time (creating it later counts as a change).
    baselines: BTreeMap<PathBuf, Option<Vec<u8>>>,
}

impl FileSnapshots {
    /// Create an empty snapshot store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Capture the current contents of `path` as its baseline, if not already
    /// captured. Idempotent per path so the first snapshot wins.
    ///
    /// A path that did not exist at capture time stores a `None` value, which is
    /// still a recorded baseline: the entry's *presence* marks the path as
    /// primed, while its `None` value records "did not exist then".
    pub fn capture(&mut self, path: &Path) {
        self.baselines
            .entry(path.to_path_buf())
            .or_insert_with(|| std::fs::read(path).ok());
    }

    /// Whether a baseline was ever captured for `path`.
    ///
    /// This distinguishes "baseline captured, file was absent then" (an entry
    /// with a `None` value) from "baseline never captured" (no entry at all).
    /// Without this distinction `changed` cannot tell a legitimate
    /// absent->present change from a missing baseline, and the latter would
    /// silently read as changed (a false positive). See [`Self::changed`].
    pub fn is_primed(&self, path: &Path) -> bool {
        self.baselines.contains_key(path)
    }

    /// Whether `path`'s current contents differ from its captured baseline.
    ///
    /// Callers must verify [`Self::is_primed`] first: this method assumes a
    /// baseline was captured. When the entry is missing it falls back to
    /// treating the baseline as absent, but that fallback exists only for
    /// robustness — relying on it produces the absent->present false positive
    /// this API is designed to prevent.
    pub fn changed(&self, path: &Path) -> bool {
        let baseline = self.baselines.get(path).cloned().flatten();
        let current = std::fs::read(path).ok();
        baseline != current
    }
}

/// Outcome of a file assertion: pass, or fail with a reason.
pub struct FileCheck {
    /// Whether the assertion held.
    pub passed: bool,
    /// On failure, a human-readable reason; `None` when passed.
    pub message: Option<String>,
}

impl FileCheck {
    fn pass() -> Self {
        FileCheck {
            passed: true,
            message: None,
        }
    }
    fn fail(msg: impl Into<String>) -> Self {
        FileCheck {
            passed: false,
            message: Some(msg.into()),
        }
    }
}

/// Assert that `path` exists.
pub fn check_exists(path: &Path) -> FileCheck {
    if path.exists() {
        FileCheck::pass()
    } else {
        FileCheck::fail(format!("file does not exist: {}", path.display()))
    }
}

/// Assert that `path`'s contents contain `needle`.
///
/// We read as bytes and search as bytes so files with non-UTF-8 content (logs,
/// binary-ish output) still match correctly.
pub fn check_contains(path: &Path, needle: &str) -> FileCheck {
    match std::fs::read(path) {
        Ok(contents) => {
            if find_bytes(&contents, needle.as_bytes()).is_some() {
                FileCheck::pass()
            } else {
                FileCheck::fail(format!(
                    "file {} does not contain {:?}",
                    path.display(),
                    needle
                ))
            }
        }
        Err(e) => FileCheck::fail(format!("cannot read {}: {e}", path.display())),
    }
}

/// Assert that `path`'s contents do NOT contain `needle`.
pub fn check_not_contains(path: &Path, needle: &str) -> FileCheck {
    match std::fs::read(path) {
        Ok(contents) => {
            if find_bytes(&contents, needle.as_bytes()).is_some() {
                FileCheck::fail(format!(
                    "file {} unexpectedly contains {:?}",
                    path.display(),
                    needle
                ))
            } else {
                FileCheck::pass()
            }
        }
        // A missing file vacuously does not contain the needle; treat as pass
        // so "must not contain error" holds even when the file was never made.
        Err(_) => FileCheck::pass(),
    }
}

/// Assert that `path`'s contents changed relative to the captured baseline.
pub fn check_changed(snapshots: &FileSnapshots, path: &Path) -> FileCheck {
    if snapshots.changed(path) {
        FileCheck::pass()
    } else {
        FileCheck::fail(format!(
            "file {} did not change since spawn",
            path.display()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::tempdir;

    fn write_file(dir: &Path, name: &str, contents: &[u8]) -> PathBuf {
        let path = dir.join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(contents).unwrap();
        path
    }

    #[test]
    fn exists_detects_presence_and_absence() {
        // check_exists must pass for a present file and fail for a missing one.
        let dir = tempdir().unwrap();
        let present = write_file(dir.path(), "there.txt", b"x");
        assert!(check_exists(&present).passed);
        assert!(!check_exists(&dir.path().join("nope.txt")).passed);
    }

    #[test]
    fn contains_and_not_contains() {
        // check_contains must find a substring; check_not_contains must pass
        // only when the substring is absent.
        let dir = tempdir().unwrap();
        let path = write_file(dir.path(), "log.txt", b"build success: ok");
        assert!(check_contains(&path, "success").passed);
        assert!(!check_contains(&path, "failure").passed);
        assert!(check_not_contains(&path, "failure").passed);
        assert!(!check_not_contains(&path, "success").passed);
    }

    #[test]
    fn not_contains_passes_for_missing_file() {
        // A non-existent file vacuously does not contain anything.
        let dir = tempdir().unwrap();
        let missing = dir.path().join("absent.txt");
        assert!(check_not_contains(&missing, "error").passed);
    }

    #[test]
    fn changed_detects_modification() {
        // After capturing a baseline, modifying the file must read as changed
        // and leaving it untouched must read as unchanged.
        let dir = tempdir().unwrap();
        let path = write_file(dir.path(), "src.ts", b"const a = 1;");
        let mut snaps = FileSnapshots::new();
        snaps.capture(&path);

        // Unchanged immediately after capture.
        assert!(!check_changed(&snaps, &path).passed);

        // Modify and re-check.
        write_file(dir.path(), "src.ts", b"const a = 2;");
        assert!(check_changed(&snaps, &path).passed);
    }

    #[test]
    fn is_primed_distinguishes_captured_from_uncaptured() {
        // is_primed must report true once a path is captured (even if the file
        // was absent at capture time) and false for a path never captured. This
        // is what lets the runner reject an unprimed expect_file_changed instead
        // of silently reading it as changed.
        let dir = tempdir().unwrap();
        let existing = write_file(dir.path(), "here.txt", b"x");
        let absent = dir.path().join("absent.txt");
        let never = dir.path().join("never.txt");

        let mut snaps = FileSnapshots::new();
        snaps.capture(&existing);
        snaps.capture(&absent); // captures None, but is still primed

        assert!(snaps.is_primed(&existing));
        assert!(snaps.is_primed(&absent));
        assert!(!snaps.is_primed(&never));
    }

    #[test]
    fn changed_detects_creation_after_snapshot() {
        // A file absent at snapshot time but present later must count as
        // changed.
        let dir = tempdir().unwrap();
        let path = dir.path().join("created.txt");
        let mut snaps = FileSnapshots::new();
        snaps.capture(&path); // captures None (absent)
        write_file(dir.path(), "created.txt", b"now here");
        assert!(check_changed(&snaps, &path).passed);
    }
}
