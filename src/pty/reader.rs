//! The shared output buffer and the dedicated reader thread.
//!
//! `portable-pty` exposes the master side as a blocking `Read`. We dedicate one
//! thread to draining it in a loop and appending into a shared
//! [`OutputBuffer`], notifying a `Condvar` after every append. Consumers
//! (`wait_for`) park on that condvar and wake the instant new bytes land. We
//! pick a thread + condvar over an async runtime because portable-pty's
//! reader/writer are blocking; wrapping them in tokio would require spawning
//! blocking tasks anyway, so a plain thread is simpler and dependency-free.

use std::io::Read;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

/// Per-read chunk size for the reader thread.
const READ_CHUNK: usize = 4096;

/// The output state shared between the reader thread and consumers.
///
/// Fields are `pub(super)` rather than `pub`: only this module's reader thread
/// and the sibling matcher (`super::matcher::wait_for`, which must hold the lock
/// across condvar waits) touch the raw cursor state. Keeping them out of the
/// public API funnels all other access through [`OutputBufferHandle`]'s methods,
/// so the search-cursor invariant (only `raw[search_from..]` is unconsumed)
/// lives in one place.
pub struct OutputBuffer {
    /// All bytes read from the PTY so far, in arrival order.
    pub(super) raw: Vec<u8>,
    /// The offset up to which output has been consumed by prior `expect`s.
    /// `wait_for` searches only `raw[search_from..]`.
    pub(super) search_from: usize,
    /// Set true once the reader thread observes EOF on the master.
    pub(super) closed: bool,
}

/// A cloneable handle to the shared output buffer and its condition variable.
///
/// Cloning shares the same underlying buffer (it is an `Arc`), which is exactly
/// what the reader thread and the session need.
#[derive(Clone)]
pub struct OutputBufferHandle {
    inner: Arc<(Mutex<OutputBuffer>, Condvar)>,
}

impl OutputBufferHandle {
    /// Create an empty, open output buffer.
    pub fn new() -> Self {
        OutputBufferHandle {
            inner: Arc::new((
                Mutex::new(OutputBuffer {
                    raw: Vec::new(),
                    search_from: 0,
                    closed: false,
                }),
                Condvar::new(),
            )),
        }
    }

    /// Borrow the `(Mutex, Condvar)` pair for direct locking in `wait_for`.
    pub fn parts(&self) -> (&Mutex<OutputBuffer>, &Condvar) {
        (&self.inner.0, &self.inner.1)
    }

    /// Append bytes and wake all waiters.
    ///
    /// Called by the reader thread on every successful read, and by tests to
    /// simulate output.
    pub fn append(&self, bytes: &[u8]) {
        let (mutex, condvar) = self.parts();
        {
            let mut buf = mutex.lock().expect("output buffer mutex poisoned");
            buf.raw.extend_from_slice(bytes);
        }
        // Notify outside (or after) extending so woken waiters see the new
        // bytes. notify_all because multiple expects could conceptually wait,
        // though in practice the session is single-consumer.
        condvar.notify_all();
    }

    /// Mark the stream closed (EOF) and wake all waiters so they can fail fast.
    pub fn mark_closed(&self) {
        let (mutex, condvar) = self.parts();
        {
            let mut buf = mutex.lock().expect("output buffer mutex poisoned");
            buf.closed = true;
        }
        condvar.notify_all();
    }

    /// Snapshot a lossy `String` of the entire buffer (used for logging).
    pub fn snapshot_string(&self) -> String {
        let (mutex, _) = self.parts();
        let buf = mutex.lock().expect("output buffer mutex poisoned");
        String::from_utf8_lossy(&buf.raw).into_owned()
    }

    /// Run `f` over the trailing `window` bytes of the buffer, holding the lock.
    ///
    /// The slice begins at the first UTF-8 char boundary at or after
    /// `len - window` (the edge is advanced forward to the next boundary so the
    /// bytes handed to `f` are always valid UTF-8 when re-decoded), and runs to
    /// the current buffer end. If the window edge lands inside a multibyte
    /// sequence with no boundary before `len`, the slice can be shorter than
    /// `window` — empty in the degenerate case — which callers must tolerate.
    /// Callers receive a borrowed slice rather than an owned `String`: this lets
    /// a poll loop inspect only the tail without the O(buffer) heap copy that
    /// [`Self::snapshot_string`] performs on every call. The closure must not
    /// retain the slice past its return (the lock is released afterward).
    ///
    /// Why a bounded tail rather than the whole buffer: the tail-JSON poll in
    /// the runner re-runs on a short interval, and copying/scanning a multi-MB
    /// buffer each pass is quadratic over a long-running process's output (the
    /// same O(B²) trap `wait_for` avoids with its incremental cursor). A fixed
    /// window keeps each poll O(window).
    pub fn with_tail<R>(&self, window: usize, f: impl FnOnce(&[u8]) -> R) -> R {
        let (mutex, _) = self.parts();
        let buf = mutex.lock().expect("output buffer mutex poisoned");
        let len = buf.raw.len();
        let mut start = len.saturating_sub(window);
        // Advance to the next char boundary so `from_utf8_lossy` over the slice
        // does not split a multibyte sequence at the window's left edge.
        while start < len && !is_char_boundary(&buf.raw, start) {
            start += 1;
        }
        f(&buf.raw[start..])
    }

    /// Whether `matcher` currently matches the *unconsumed* output.
    ///
    /// Scope: this examines only `raw[search_from..]`. It never waits and never
    /// advances the cursor. `search_from` is the consumed cursor, which advances
    /// **only** when an `expect`/`expect_regex` succeeds. The precise scanned
    /// region therefore is:
    ///
    /// - After a successful prior `expect`: bytes from that match's end onward.
    ///   Earlier output the expect already consumed is masked, so an
    ///   `expect_not` for a word that appeared *before* the prior match passes.
    /// - With no prior `expect` (e.g. right after `spawn`): the whole buffer,
    ///   because `search_from` is still `0`.
    /// - After an `expect` that **failed** (timeout/EOF): `search_from` did not
    ///   advance, so output produced before that failed expect is still in
    ///   scope. Because assertions are not fail-fast, a later `expect_not` can
    ///   therefore see output from before the failed expect. This is intended.
    pub fn contains_now(&self, matcher: &super::matcher::Matcher) -> bool {
        let (mutex, _) = self.parts();
        let buf = mutex.lock().expect("output buffer mutex poisoned");
        matcher.find(&buf.raw[buf.search_from..]).is_some()
    }
}

impl Default for OutputBufferHandle {
    fn default() -> Self {
        Self::new()
    }
}

/// Whether byte index `i` of `bytes` is a UTF-8 char boundary.
///
/// A boundary is any index whose byte is not a continuation byte
/// (`0b10xx_xxxx`). `i == bytes.len()` is the trailing boundary. We hand-roll
/// this because the raw buffer is `&[u8]` (terminal output can carry invalid
/// UTF-8), so `str::is_char_boundary` is not available.
fn is_char_boundary(bytes: &[u8], i: usize) -> bool {
    if i == 0 || i == bytes.len() {
        return true;
    }
    bytes.get(i).is_none_or(|&b| (b & 0xc0) != 0x80)
}

/// Spawn the reader thread that drains `reader` into `handle` until EOF.
///
/// The thread owns the boxed reader and runs until the read side closes (the
/// child's PTY slave is gone), at which point it marks the buffer closed and
/// exits. The returned `JoinHandle` lets the session join on teardown.
pub fn spawn_reader(
    mut reader: Box<dyn Read + Send>,
    handle: OutputBufferHandle,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let mut chunk = [0u8; READ_CHUNK];
        loop {
            match reader.read(&mut chunk) {
                // A zero-length read is the conventional EOF signal.
                Ok(0) => {
                    handle.mark_closed();
                    break;
                }
                Ok(n) => handle.append(&chunk[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                // On any other read error we treat the stream as closed: there
                // is nothing useful we can do from the reader thread, and
                // marking closed lets waiters fail fast instead of hanging.
                Err(_) => {
                    handle.mark_closed();
                    break;
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn append_makes_bytes_visible() {
        // Appended bytes must be observable in the shared buffer.
        let handle = OutputBufferHandle::new();
        handle.append(b"abc");
        handle.append(b"def");
        let (mutex, _) = handle.parts();
        assert_eq!(mutex.lock().unwrap().raw, b"abcdef");
    }

    #[test]
    fn mark_closed_sets_flag() {
        // mark_closed must flip the closed flag for fast-fail logic.
        let handle = OutputBufferHandle::new();
        assert!(!handle.parts().0.lock().unwrap().closed);
        handle.mark_closed();
        assert!(handle.parts().0.lock().unwrap().closed);
    }

    #[test]
    fn reader_thread_drains_to_eof() {
        // The reader thread must copy all bytes from a Read and then mark EOF.
        let handle = OutputBufferHandle::new();
        let data: &[u8] = b"streamed output";
        let cursor = std::io::Cursor::new(data.to_vec());
        let join = spawn_reader(Box::new(cursor), handle.clone());
        join.join().unwrap();
        let buf = handle.parts().0.lock().unwrap();
        assert_eq!(buf.raw, data);
        assert!(buf.closed);
    }

    #[test]
    fn contains_now_masks_bytes_consumed_by_prior_expect() {
        // After an expect consumes up to its match, a forbidden word that lived
        // in the consumed region must read as absent: expect_not passes because
        // the cursor masks it.
        use super::super::matcher::{wait_for, Matcher};
        let handle = OutputBufferHandle::new();
        handle.append(b"error then ok\n");
        // Consume up to and including "ok"; "error" now sits behind the cursor.
        let outcome = wait_for(&handle, &Matcher::contains("ok"), Duration::from_millis(50));
        assert!(matches!(
            outcome,
            super::super::matcher::ExpectOutcome::Matched { .. }
        ));
        // "error" is before the cursor, so it is no longer visible.
        assert!(!handle.contains_now(&Matcher::contains("error")));
    }

    #[test]
    fn contains_now_scans_whole_buffer_without_prior_expect() {
        // With no successful expect, search_from is 0, so contains_now scans the
        // entire buffer (spawn-direct expect_not semantics).
        use super::super::matcher::Matcher;
        let handle = OutputBufferHandle::new();
        handle.append(b"startup error: boom");
        assert!(handle.contains_now(&Matcher::contains("error")));
        assert!(!handle.contains_now(&Matcher::contains("absent")));
    }

    #[test]
    fn contains_now_sees_pre_failure_output_after_failed_expect() {
        // A failed expect (timeout) does not advance search_from, so a later
        // contains_now still sees output produced before that failed expect.
        use super::super::matcher::{wait_for, Matcher};
        let handle = OutputBufferHandle::new();
        handle.append(b"warning here");
        // This expect fails: "missing" never appears, so the cursor stays at 0.
        let outcome = wait_for(
            &handle,
            &Matcher::contains("missing"),
            Duration::from_millis(20),
        );
        assert!(matches!(
            outcome,
            super::super::matcher::ExpectOutcome::Timeout { .. }
        ));
        // "warning" predates the failed expect but is still in scope.
        assert!(handle.contains_now(&Matcher::contains("warning")));
    }

    #[test]
    fn contains_now_boundary_when_cursor_at_needle_start() {
        // Off-by-one fix: when search_from lands exactly on the first byte of
        // the needle, the needle is still in the unconsumed region and must be
        // found; one byte further (cursor past the first needle byte) must hide
        // it.
        use super::super::matcher::Matcher;
        let handle = OutputBufferHandle::new();
        handle.append(b"XXerror");
        {
            // Cursor exactly at the 'e' of "error" (index 2): still visible.
            let mut buf = handle.parts().0.lock().unwrap();
            buf.search_from = 2;
        }
        assert!(handle.contains_now(&Matcher::contains("error")));
        {
            // Cursor one byte past the needle start (index 3): "error" can no
            // longer be matched as a whole.
            let mut buf = handle.parts().0.lock().unwrap();
            buf.search_from = 3;
        }
        assert!(!handle.contains_now(&Matcher::contains("error")));
    }

    #[test]
    fn with_tail_limits_to_window_and_avoids_full_copy() {
        // with_tail must hand the closure only the trailing `window` bytes, so a
        // poll over a large buffer never touches the whole buffer.
        let handle = OutputBufferHandle::new();
        handle.append(&vec![b'x'; 100_000]);
        handle.append(b"TAIL");
        let seen_len = handle.with_tail(64, |bytes| bytes.len());
        assert_eq!(seen_len, 64);
        let ends_with_tail = handle.with_tail(64, |bytes| bytes.ends_with(b"TAIL"));
        assert!(ends_with_tail);
    }

    #[test]
    fn with_tail_window_larger_than_buffer_returns_whole_buffer() {
        // A window wider than the buffer yields the entire buffer, not a panic.
        let handle = OutputBufferHandle::new();
        handle.append(b"short");
        let copy = handle.with_tail(1024, <[u8]>::to_vec);
        assert_eq!(copy, b"short");
    }

    #[test]
    fn with_tail_advances_to_utf8_boundary() {
        // When the window edge falls inside a multibyte char, with_tail advances
        // to the next boundary so the slice re-decodes as valid UTF-8 (never
        // splitting a multibyte sequence).
        let handle = OutputBufferHandle::new();
        // "a" + "あ" (bytes: 61 E3 81 82, len 4). A window of 2 lands on the E3's
        // continuation bytes; the edge advances forward past them to the trailing
        // boundary, yielding an empty (still valid) slice rather than a split char.
        handle.append("aあ".as_bytes());
        let split = handle.with_tail(2, |bytes| {
            String::from_utf8(bytes.to_vec()).expect("valid utf8")
        });
        assert_eq!(split, "");
        // A window of 3 lands exactly on the E3 lead byte (a boundary), so the
        // whole "あ" is returned intact.
        let whole_char = handle.with_tail(3, |bytes| {
            String::from_utf8(bytes.to_vec()).expect("valid utf8")
        });
        assert_eq!(whole_char, "あ");
    }

    #[test]
    fn waiter_wakes_on_append() {
        // A thread parked on the condvar must wake when new bytes are appended.
        let handle = OutputBufferHandle::new();
        let producer = handle.clone();
        let consumer = handle.clone();
        let t = std::thread::spawn(move || {
            let (mutex, condvar) = consumer.parts();
            let mut buf = mutex.lock().unwrap();
            while buf.raw.is_empty() {
                let (next, _) = condvar.wait_timeout(buf, Duration::from_secs(2)).unwrap();
                buf = next;
            }
            buf.raw.clone()
        });
        std::thread::sleep(Duration::from_millis(20));
        producer.append(b"hi");
        assert_eq!(t.join().unwrap(), b"hi");
    }
}
