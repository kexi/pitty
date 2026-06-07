//! Output matchers and the `wait_for` blocking-wait implementation.
//!
//! A [`Matcher`] decides whether a slice of pending PTY output satisfies an
//! `expect`. We operate on raw bytes (`&[u8]`) throughout: terminal output can
//! contain invalid UTF-8 (partial multibyte sequences split across reads,
//! control bytes, etc.), and `regex::bytes` lets us match such streams without
//! risking a panic or a lossy `String::from_utf8_lossy` that would change the
//! bytes we are matching against.

use std::time::{Duration, Instant};

use regex::bytes::Regex;

use super::reader::OutputBufferHandle;
use crate::bytes::find_bytes;

/// Maximum number of trailing buffer bytes attached to a timeout diagnostic.
const TIMEOUT_TAIL_BYTES: usize = 512;

/// A compiled matcher over output bytes.
pub enum Matcher {
    /// Substring (subsequence-of-bytes) containment.
    Contains(Vec<u8>),
    /// Regular-expression match over bytes.
    Regex(Regex),
}

impl Matcher {
    /// Build a `contains` matcher from a UTF-8 needle.
    pub fn contains(needle: &str) -> Self {
        Matcher::Contains(needle.as_bytes().to_vec())
    }

    /// Compile a regex matcher. Invalid patterns return an error string.
    pub fn regex(pattern: &str) -> Result<Self, String> {
        Regex::new(pattern)
            .map(Matcher::Regex)
            .map_err(|e| format!("invalid regex '{pattern}': {e}"))
    }

    /// Find the first match in `haystack`.
    ///
    /// Returns the half-open byte range `[start, end)` of the match so callers
    /// can advance their search cursor past it. `None` means no match yet.
    pub fn find(&self, haystack: &[u8]) -> Option<(usize, usize)> {
        match self {
            Matcher::Contains(needle) => {
                find_bytes(haystack, needle).map(|start| (start, start + needle.len()))
            }
            Matcher::Regex(re) => re.find(haystack).map(|m| (m.start(), m.end())),
        }
    }

    /// Find the first match at or after `window_start`, returning the match's
    /// absolute end offset within the full buffer.
    ///
    /// This is the incremental entry point used by [`wait_for`]: rather than
    /// re-scanning the whole unconsumed tail on every condvar wakeup, the caller
    /// advances `window_start` to just behind the bytes it has already scanned
    /// (minus [`Self::needle_overlap`]) so only genuinely new bytes are
    /// examined. `buf[window_start..]` is scanned and the relative match end is
    /// translated back to an absolute offset.
    fn find_from(&self, buf: &[u8], window_start: usize) -> Option<usize> {
        let window = &buf[window_start..];
        match self {
            Matcher::Contains(needle) => {
                find_bytes(window, needle).map(|rel_start| window_start + rel_start + needle.len())
            }
            // Why not increment the regex scan the way we do for Contains: the
            // regex engine cannot resume from a saved position, and anchors
            // (^, \A, $, \z) bind to the *start/end of the scanned slice*. If we
            // re-anchored a moving window we would silently change the pattern's
            // meaning. We therefore scan `buf[window_start..]` from a fixed
            // window_start; wait_for keeps window_start pinned to the consumed
            // cursor for regex so anchoring stays well-defined. The relative end
            // is shifted by window_start to recover the absolute offset.
            Matcher::Regex(re) => re.find(window).map(|m| window_start + m.end()),
        }
    }

    /// The number of trailing bytes of an already-scanned region that must be
    /// re-examined so a match straddling the boundary between previously and
    /// newly arrived bytes is not missed.
    ///
    /// For a substring needle this is `needle.len() - 1`: any shorter overlap
    /// could not hide a full needle occurrence. Regex has no fixed needle
    /// length, so it reports `None` and is handled by scanning from the consumed
    /// cursor instead (see [`Self::find_from`]).
    fn needle_overlap(&self) -> Option<usize> {
        match self {
            Matcher::Contains(needle) => Some(needle.len().saturating_sub(1)),
            Matcher::Regex(_) => None,
        }
    }
}

/// The outcome of a [`wait_for`] call.
#[derive(Debug, PartialEq, Eq)]
pub enum ExpectOutcome {
    /// The matcher matched. Carries the absolute end offset of the match so
    /// the session can advance its consumed cursor.
    Matched { consumed_to: usize },
    /// The deadline elapsed without a match. Carries a lossy snapshot of the
    /// trailing output bytes for diagnostics.
    Timeout { tail: String },
    /// The stream reached EOF before the matcher matched.
    EofBeforeMatch { tail: String },
}

/// Block until `matcher` matches new output, or the timeout / EOF intervenes.
///
/// Algorithm:
/// 1. Compute a fixed `deadline` so repeated condvar waits converge instead of
///    each resetting the full timeout.
/// 2. Scan only the bytes that have arrived since the previous wakeup, not the
///    whole unconsumed tail. `search_from` is where the previous `expect`
///    stopped; advancing it past each match gives Playwright-style sequential
///    semantics (two consecutive `expect: hello` calls require two distinct
///    occurrences). Within one `wait_for` call we additionally track
///    `searched_up_to`: a local cursor marking the buffer length already
///    examined on prior loop iterations. Re-scanning from `searched_up_to`
///    (minus the needle overlap) instead of from `search_from` means each
///    condvar wakeup costs O(new bytes) rather than O(unconsumed tail). Without
///    this, a long agent transcript would re-scan ~the whole buffer on every
///    one of the hundreds of chunk wakeups, turning a single `expect` into
///    quadratic work over the output size.
/// 3. The scan starts at `searched_up_to - needle_overlap` so a needle that
///    straddles the boundary between previously-scanned and freshly-arrived
///    bytes is still found. For regex (no fixed needle length) we keep the
///    window pinned at `search_from`, see [`Matcher::find_from`].
/// 4. On a miss, if the stream is closed there can be no more bytes, so we fail
///    fast with `EofBeforeMatch` instead of waiting out the whole timeout.
/// 5. Otherwise wait on the condvar with the *remaining* time. We block on the
///    condvar rather than polling with `sleep`: the reader thread notifies us
///    the instant new bytes arrive, so a match is observed immediately. A
///    sleep-poll loop would add latency jitter equal to the poll interval and
///    make tight timeouts flaky.
pub fn wait_for(
    handle: &OutputBufferHandle,
    matcher: &Matcher,
    timeout: Duration,
) -> ExpectOutcome {
    let deadline = Instant::now() + timeout;
    let (mutex, condvar) = handle.parts();

    let mut buf = mutex.lock().expect("output buffer mutex poisoned");
    // Bytes already examined on a previous iteration of this call. Starts at the
    // consumed cursor; never moves backwards across wakeups.
    let mut searched_up_to = buf.search_from;
    loop {
        // Begin the scan just behind the already-scanned region so a match
        // spanning the boundary is not lost. For regex, needle_overlap is None
        // and we pin the window at search_from (anchors must see a stable
        // slice start). The max with search_from keeps us from scanning bytes a
        // prior expect already consumed.
        let window_start = match matcher.needle_overlap() {
            Some(overlap) => searched_up_to.saturating_sub(overlap).max(buf.search_from),
            None => buf.search_from,
        };
        if let Some(absolute_end) = matcher.find_from(&buf.raw, window_start) {
            buf.search_from = absolute_end;
            return ExpectOutcome::Matched {
                consumed_to: absolute_end,
            };
        }
        // Everything up to the current end has now been examined.
        searched_up_to = buf.raw.len();

        if buf.closed {
            return ExpectOutcome::EofBeforeMatch {
                tail: tail_snapshot(&buf.raw),
            };
        }

        let now = Instant::now();
        if now >= deadline {
            return ExpectOutcome::Timeout {
                tail: tail_snapshot(&buf.raw),
            };
        }

        let remaining = deadline - now;
        // wait_timeout releases the lock while parked and reacquires it on
        // wake, so the reader thread can append + notify in between.
        let (next, _timed_out) = condvar
            .wait_timeout(buf, remaining)
            .expect("output buffer mutex poisoned");
        buf = next;
        // Loop re-checks the matcher and the deadline; a spurious wakeup simply
        // re-evaluates and re-parks.
    }
}

/// Render the trailing `TIMEOUT_TAIL_BYTES` of the buffer for diagnostics.
///
/// Uses `from_utf8_lossy` only for the human-facing message; the matching path
/// itself never goes through lossy conversion.
fn tail_snapshot(raw: &[u8]) -> String {
    let start = raw.len().saturating_sub(TIMEOUT_TAIL_BYTES);
    String::from_utf8_lossy(&raw[start..]).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pty::reader::OutputBufferHandle;

    #[test]
    fn contains_finds_substring() {
        // A contains matcher must locate the needle and report its byte range.
        let m = Matcher::contains("world");
        assert_eq!(m.find(b"hello world!"), Some((6, 11)));
        assert_eq!(m.find(b"hello there"), None);
    }

    #[test]
    fn regex_matches_bytes() {
        // A regex matcher must match across arbitrary bytes and report range.
        let m = Matcher::regex("hello.*world").unwrap();
        assert!(m.find(b"say hello to the world now").is_some());
        assert!(m.find(b"hello").is_none());
    }

    #[test]
    fn regex_tolerates_invalid_utf8() {
        // regex::bytes must not panic on invalid UTF-8 in the haystack.
        let m = Matcher::regex("ok").unwrap();
        let haystack = [0xff, 0xfe, b'o', b'k'];
        assert!(m.find(&haystack).is_some());
    }

    #[test]
    fn wait_for_matches_existing_output() {
        // When the buffer already contains the needle, wait_for must return
        // Matched without blocking and advance the consumed cursor.
        let handle = OutputBufferHandle::new();
        handle.append(b"login successful\n");
        let outcome = wait_for(
            &handle,
            &Matcher::contains("successful"),
            Duration::from_secs(1),
        );
        // "login successful" ends at byte 16 (the trailing '\n' is index 16).
        match outcome {
            ExpectOutcome::Matched { consumed_to } => assert_eq!(consumed_to, 16),
            other => panic!("expected Matched, got {other:?}"),
        }
    }

    #[test]
    fn search_from_advances_so_consecutive_expects_need_distinct_matches() {
        // Two consecutive expects for the same needle must consume two distinct
        // occurrences; the second must not re-match the first's bytes.
        let handle = OutputBufferHandle::new();
        handle.append(b"tick tick\n");
        let first = wait_for(
            &handle,
            &Matcher::contains("tick"),
            Duration::from_millis(50),
        );
        assert!(matches!(first, ExpectOutcome::Matched { .. }));
        let second = wait_for(
            &handle,
            &Matcher::contains("tick"),
            Duration::from_millis(50),
        );
        assert!(matches!(second, ExpectOutcome::Matched { .. }));
        // A third must time out: only two "tick" occurrences existed.
        let third = wait_for(
            &handle,
            &Matcher::contains("tick"),
            Duration::from_millis(50),
        );
        assert!(matches!(third, ExpectOutcome::Timeout { .. }));
    }

    #[test]
    fn wait_for_times_out_when_absent() {
        // An absent needle on an open stream must yield Timeout after the
        // deadline, with a diagnostic tail.
        let handle = OutputBufferHandle::new();
        handle.append(b"nothing relevant here");
        let outcome = wait_for(
            &handle,
            &Matcher::contains("missing"),
            Duration::from_millis(30),
        );
        match outcome {
            ExpectOutcome::Timeout { tail } => assert!(tail.contains("nothing relevant")),
            other => panic!("expected Timeout, got {other:?}"),
        }
    }

    #[test]
    fn wait_for_matches_needle_straddling_chunk_boundary() {
        // The incremental scan must still find a needle whose bytes are split
        // across two appends (i.e. across two condvar wakeups). This guards the
        // searched_up_to/needle_overlap rewind: a naive "scan only new bytes"
        // would miss the boundary-spanning occurrence.
        let handle = OutputBufferHandle::new();
        let producer = handle.clone();
        let waiter = handle.clone();
        let join = std::thread::spawn(move || {
            wait_for(&waiter, &Matcher::contains("hello"), Duration::from_secs(2))
        });
        // Append the needle split down the middle, with the consumer parked in
        // between so each half arrives on its own wakeup.
        producer.append(b"xxhel");
        std::thread::sleep(Duration::from_millis(20));
        producer.append(b"loyy");
        match join.join().unwrap() {
            // "xxhello" -> needle ends at absolute offset 7.
            ExpectOutcome::Matched { consumed_to } => assert_eq!(consumed_to, 7),
            other => panic!("expected Matched, got {other:?}"),
        }
    }

    #[test]
    fn wait_for_does_not_rematch_consumed_bytes_across_chunks() {
        // After consuming the first occurrence, a second wait must not re-match
        // the same bytes even though the incremental window rewinds by the
        // needle overlap: the rewind is clamped to search_from.
        let handle = OutputBufferHandle::new();
        handle.append(b"abcabc");
        let first = wait_for(
            &handle,
            &Matcher::contains("abc"),
            Duration::from_millis(50),
        );
        assert!(matches!(first, ExpectOutcome::Matched { consumed_to: 3 }));
        let second = wait_for(
            &handle,
            &Matcher::contains("abc"),
            Duration::from_millis(50),
        );
        assert!(matches!(second, ExpectOutcome::Matched { consumed_to: 6 }));
        let third = wait_for(
            &handle,
            &Matcher::contains("abc"),
            Duration::from_millis(30),
        );
        assert!(matches!(third, ExpectOutcome::Timeout { .. }));
    }

    #[test]
    fn wait_for_regex_matches_across_chunks() {
        // Regex matching must still succeed when the matched span arrives over
        // multiple appends; the regex window stays pinned at search_from.
        let handle = OutputBufferHandle::new();
        let producer = handle.clone();
        let waiter = handle.clone();
        let join = std::thread::spawn(move || {
            wait_for(
                &waiter,
                &Matcher::regex("ab.*yz").unwrap(),
                Duration::from_secs(2),
            )
        });
        producer.append(b"ab12");
        std::thread::sleep(Duration::from_millis(20));
        producer.append(b"34yz");
        assert!(matches!(
            join.join().unwrap(),
            ExpectOutcome::Matched { .. }
        ));
    }

    #[test]
    fn wait_for_zero_timeout_absent_needle_returns_immediately() {
        // A zero timeout with an absent needle must return Timeout right away
        // without parking on the condvar (which could otherwise hang).
        let handle = OutputBufferHandle::new();
        handle.append(b"nothing here");
        let start = Instant::now();
        let outcome = wait_for(&handle, &Matcher::contains("absent"), Duration::ZERO);
        assert!(
            start.elapsed() < Duration::from_millis(100),
            "zero timeout must not park"
        );
        assert!(matches!(outcome, ExpectOutcome::Timeout { .. }));
    }

    #[test]
    fn wait_for_zero_timeout_present_needle_matches() {
        // With a zero timeout but the needle already present, the find happens
        // before the deadline check, so the result is Matched, not Timeout.
        let handle = OutputBufferHandle::new();
        handle.append(b"ready now");
        let outcome = wait_for(&handle, &Matcher::contains("ready"), Duration::ZERO);
        assert!(matches!(outcome, ExpectOutcome::Matched { .. }));
    }

    #[test]
    fn wait_for_reports_eof_before_match() {
        // A closed stream without the needle must fail fast as EofBeforeMatch
        // rather than waiting out the timeout.
        let handle = OutputBufferHandle::new();
        handle.append(b"partial");
        handle.mark_closed();
        let start = Instant::now();
        let outcome = wait_for(
            &handle,
            &Matcher::contains("complete"),
            Duration::from_secs(10),
        );
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "must not wait full timeout"
        );
        assert!(matches!(outcome, ExpectOutcome::EofBeforeMatch { .. }));
    }
}
