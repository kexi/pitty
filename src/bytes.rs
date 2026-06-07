//! Byte-subsequence search shared by the output matcher and file assertions.
//!
//! Both the PTY output matcher (`pty::matcher`) and the file-content assertions
//! (`assert::file`) need the same "first occurrence of `needle` in `haystack`"
//! primitive. We centralize it here so the two call sites cannot drift apart.

/// Locate the first occurrence of `needle` within `haystack`.
///
/// Returns the start offset of the match, or `None` if absent. An empty needle
/// matches at offset 0 (it is trivially contained everywhere).
///
/// We implement this with a windowed scan over the standard library rather than
/// adding a dependency like `memchr`: scenario needles and per-read buffers are
/// small, so the simple approach is more than fast enough and keeps the
/// dependency footprint minimal.
pub(crate) fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    if needle.len() > haystack.len() {
        return None;
    }
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_first_occurrence() {
        // The search must return the offset of the first match, not a later one.
        assert_eq!(find_bytes(b"abcabc", b"bc"), Some(1));
    }

    #[test]
    fn empty_needle_matches_at_zero() {
        // An empty needle is contained everywhere; report offset 0.
        assert_eq!(find_bytes(b"abc", b""), Some(0));
    }

    #[test]
    fn longer_needle_than_haystack_misses() {
        // A needle longer than the haystack can never match.
        assert_eq!(find_bytes(b"ab", b"abc"), None);
    }

    #[test]
    fn absent_needle_returns_none() {
        // A needle not present must return None.
        assert_eq!(find_bytes(b"hello", b"xyz"), None);
    }
}
