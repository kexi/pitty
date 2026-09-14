//! `expect_json` assertion: extract a JSON value from PTY output or a file,
//! navigate it by a path (dotted path or bracket notation), and compare with
//! `equals`/`contains`/`exists`.
//!
//! The hard parts are (1) locating a self-delimiting JSON block at the tail of
//! noisy terminal output and (2) doing it without a JSON-path dependency. Both
//! are handled by small, dependency-free scans below.
//!
//! Single-value contract: [`navigate_one`] always resolves to a single node.
//! Multi-value selectors (`[*]`, recursive `..`, filters) are intentionally
//! excluded — see its doc for why and for the upgrade path that keeps that
//! exclusion from leaking into callers.

use serde_json::Value;

/// The comparison an `expect_json` step performs against the value at `path`.
///
/// Exactly one variant is selected by the scenario; the one-of constraint is
/// enforced at deserialize time in `config::step` via a Raw helper.
#[derive(Debug, Clone)]
pub enum JsonCheck {
    /// The value must equal the given JSON value (type-aware).
    Equals(Value),
    /// The value must be a string containing the given substring.
    Contains(String),
    /// The path must resolve to a value (presence check).
    Exists,
}

/// Outcome of an `expect_json` evaluation: pass, or fail with a reason.
pub struct JsonResult {
    /// Whether the assertion held.
    pub passed: bool,
    /// On failure, a human-readable reason; `None` when passed.
    pub message: Option<String>,
}

impl JsonResult {
    fn pass() -> Self {
        JsonResult {
            passed: true,
            message: None,
        }
    }
    fn fail(message: impl Into<String>) -> Self {
        JsonResult {
            passed: false,
            message: Some(message.into()),
        }
    }
}

/// Evaluate an `expect_json` check against an already-parsed JSON `root`.
///
/// `path` is the dotted path (see [`navigate_one`]); resolution failure fails the
/// assertion for every check kind except where noted.
pub fn evaluate(root: &Value, path: &str, check: &JsonCheck) -> JsonResult {
    let resolved = navigate_one(root, path);

    // A single match over the check kind, with no second match and no dead arm:
    // `Exists` decides on presence alone, while the value-dependent checks resolve
    // the node (failing on a missing path) inside their own arm. Folding the two
    // former matches into one keeps every kind's resolution-failure behavior in
    // exactly one place and removes the previously unreachable `Exists` arm.
    match check {
        // `exists` only cares about presence, treating a missing path as a plain
        // failure rather than a type error.
        JsonCheck::Exists => match resolved {
            Some(_) => JsonResult::pass(),
            None => JsonResult::fail(format!("path '{path}' does not exist")),
        },
        JsonCheck::Equals(expected) => match resolved {
            None => JsonResult::fail(format!("path '{path}' does not exist")),
            Some(value) if value == expected => JsonResult::pass(),
            Some(value) => JsonResult::fail(format!(
                "path '{path}': expected {}, got {}",
                compact(expected),
                compact(value)
            )),
        },
        JsonCheck::Contains(needle) => match resolved {
            None => JsonResult::fail(format!("path '{path}' does not exist")),
            Some(Value::String(s)) if s.contains(needle.as_str()) => JsonResult::pass(),
            Some(Value::String(s)) => JsonResult::fail(format!(
                "path '{path}': string {s:?} does not contain {needle:?}"
            )),
            // `contains` is a substring test, which is only meaningful for a
            // string target. Other types fail with a clear type message rather
            // than silently stringifying, so authors fix the path/expectation.
            Some(other) => JsonResult::fail(format!(
                "path '{path}': contains requires a string value, got {}",
                type_name(other)
            )),
        },
    }
}

/// Navigate `root` by a `path` and return the single addressed value.
///
/// Single-value contract (Why not return a multi-value result): this resolves to
/// at most one node and the `Option<&Value>` return type encodes that. Multi-value
/// selectors (`[*]`, recursive `..`, filters) are intentionally excluded — the
/// `equals`/`contains`/`exists` checks each compare one leaf, so a multi-match
/// selector would make `equals` ambiguous (any vs. all of the matches) with no
/// obvious right answer. A future release adding such selectors must NOT widen this
/// signature in place (that would silently break every caller's single-value
/// assumption); it should add a separate multi-value entry point and leave this one
/// resolving a single node. The `_one` suffix names that contract so the boundary
/// is visible at the call site.
///
/// Grammar (a minimal, upward-compatible extension of the v0.3 dotted subset):
/// - dotted object keys: `result.status` (the backward-compatible core),
/// - dotted array indices: a token that is a run of ASCII digits (`items.0`),
/// - bracketed array indices: `items[0]` (a run of ASCII digits in brackets),
/// - bracketed quoted keys: `result["a.b"].value` — a double-quoted key in
///   brackets, used to address an object key that itself contains a `.` (or other
///   characters that the dotted form would split on). Quote-internal `\"` and
///   `\\` escapes are honored.
///
/// The forms compose freely: `a["b.c"][0].d` walks object key `a`, then object
/// key `b.c`, then array index `0`, then object key `d`.
///
/// A malformed path (an unterminated bracket, an unterminated/ill-escaped quote,
/// a non-numeric unquoted bracket index, an empty dotted token from `a..b` /
/// `a.`) resolves to `None`, so a typo surfaces as a missing path (assertion
/// failure) rather than silently addressing a different value or panicking.
///
/// Why not a full JSONPath engine (`$`, `[*]`, `..`, filters): `expect_json`'s
/// `equals`/`contains`/`exists` all compare a single leaf, so a multi-match
/// selector like `[*]` or a filter would make "equals" ambiguous (any vs. all of
/// the matches) with no obvious right answer; E2E scenarios address one known leaf
/// in a known report shape. Restricting to a single-leaf grammar keeps the
/// semantics unambiguous and dependency-free, and the bracket forms added here are
/// a strict superset of the v0.3 dotted grammar, so a future release can extend
/// toward fuller JSONPath without breaking existing paths.
pub fn navigate_one<'v>(root: &'v Value, path: &str) -> Option<&'v Value> {
    // An empty path addresses the root itself, which makes `exists` on the
    // whole document well-defined and avoids a spurious empty token.
    if path.is_empty() {
        return Some(root);
    }
    let segments = tokenize_path(path)?;
    let mut current = root;
    for segment in segments {
        current = match (&segment, current) {
            // A quoted bracket key always addresses an object key verbatim,
            // including keys that contain `.`; it never indexes an array.
            (Segment::Key(key), Value::Object(map)) => map.get(key.as_str())?,
            // A bare token may be an object key or, on an array, a numeric index.
            (Segment::Bare(token), Value::Object(map)) => map.get(token.as_str())?,
            (Segment::Bare(token), Value::Array(items)) => {
                let index = parse_array_index(token)?;
                items.get(index)?
            }
            // A bracketed numeric index only indexes arrays.
            (Segment::Index(index), Value::Array(items)) => items.get(*index)?,
            // Any other pairing (descending into a scalar, indexing an object by
            // number, keying an array) is a path that does not resolve.
            _ => return None,
        };
    }
    Some(current)
}

/// One resolved step of a navigation path.
enum Segment {
    /// A dotted token, which may name an object key or (on an array) a numeric
    /// index. Its array/object meaning is decided against the value at navigation
    /// time, preserving the v0.3 `items.0` behavior.
    Bare(String),
    /// A bracketed, double-quoted object key (`["a.b"]`), addressing an object
    /// key verbatim regardless of the characters it contains.
    Key(String),
    /// A bracketed numeric index (`[0]`), only valid against an array.
    Index(usize),
}

/// Tokenize a navigation `path` into ordered [`Segment`]s, or `None` on a
/// malformed path.
///
/// Hand-written byte/char scan (no regex), mirroring the dependency-free,
/// byte-walking philosophy of `string_mask`/`matching_open` in this module. The
/// scanner alternates between two states: reading a dotted token, and reading a
/// `[...]` group. A `[` may follow a dotted token directly (`items[0]`) or another
/// bracket (`["a"][0]`); a `.` separates dotted tokens. Why return `None` on any
/// malformation rather than partially resolving: a path is single-trust input, and
/// surfacing a typo as a missing path (assertion failure) is safer than addressing
/// a different value; it must never panic.
fn tokenize_path(path: &str) -> Option<Vec<Segment>> {
    let mut segments = Vec::new();
    let chars: Vec<char> = path.chars().collect();
    let mut i = 0;
    // True once a segment has been emitted. A bare token (one not introduced by a
    // `.` separator) is then only valid at the very start; after any segment it
    // would be `]foo` / `foo bar` style junk. A leading `[` (e.g. `[0].name`) is
    // allowed because the first segment can be a bracket group, not a dotted token.
    let mut segment_emitted = false;

    while i < chars.len() {
        match chars[i] {
            '[' => {
                let (segment, next) = parse_bracket(&chars, i)?;
                segments.push(segment);
                i = next;
                segment_emitted = true;
            }
            '.' => {
                // A `.` is a *separator* between segments, so it requires a prior
                // segment: a leading `.` (`.foo`) is rejected, matching the old
                // `split('.')` behavior where the empty first token failed. After
                // the dot, an empty token (`a..b`, trailing `a.`) is also rejected.
                if segments.is_empty() {
                    return None;
                }
                i += 1;
                let (token, next) = read_dotted_token(&chars, i);
                // Reject an empty token from `a..b` or a trailing `a.`: an empty
                // dotted segment never addresses anything, so surface it as a
                // malformed path (None) rather than matching an empty-string key.
                if token.is_empty() {
                    return None;
                }
                segments.push(Segment::Bare(token));
                i = next;
                segment_emitted = true;
            }
            _ => {
                // A bare token directly after a prior segment (a `]` or another
                // token) without a separating `.` — `foo bar`, `]foo` — is junk;
                // a bare token is only valid as the very first segment.
                if segment_emitted {
                    return None;
                }
                let (token, next) = read_dotted_token(&chars, i);
                // The empty-token guard also covers a degenerate first token (e.g.
                // a path that starts with a stray separator handled above).
                if token.is_empty() {
                    return None;
                }
                segments.push(Segment::Bare(token));
                i = next;
                segment_emitted = true;
            }
        }
    }

    if segments.is_empty() {
        return None;
    }
    Some(segments)
}

/// Read a dotted token starting at `start`, stopping before the next `.` or `[`.
/// Returns the token text and the index just past it.
fn read_dotted_token(chars: &[char], start: usize) -> (String, usize) {
    let mut i = start;
    let mut token = String::new();
    while i < chars.len() && chars[i] != '.' && chars[i] != '[' {
        token.push(chars[i]);
        i += 1;
    }
    (token, i)
}

/// Parse a `[...]` group starting at the `[` at `open`, returning the resolved
/// segment and the index just past the closing `]`, or `None` if malformed.
///
/// Two bracket forms are recognized: a double-quoted key (`["a.b"]`) and a bare
/// numeric index (`[0]`). Anything else (an unquoted non-numeric token, an
/// unterminated bracket/quote, trailing junk before `]`) is malformed.
fn parse_bracket(chars: &[char], open: usize) -> Option<(Segment, usize)> {
    // chars[open] is '['.
    let mut i = open + 1;
    if i >= chars.len() {
        return None;
    }

    let is_quoted = chars[i] == '"';
    if is_quoted {
        let (key, after_quote) = read_quoted_key(chars, i)?;
        // The character right after the closing quote must be the closing `]`.
        if after_quote >= chars.len() || chars[after_quote] != ']' {
            return None;
        }
        return Some((Segment::Key(key), after_quote + 1));
    }

    // Bare bracket content: collect up to the closing `]`, then require it to be a
    // pure numeric index (reusing the strict digit-run rule).
    let mut content = String::new();
    while i < chars.len() && chars[i] != ']' {
        content.push(chars[i]);
        i += 1;
    }
    // An unterminated bracket (`items[0`) is malformed.
    if i >= chars.len() {
        return None;
    }
    let index = parse_array_index(&content)?;
    Some((Segment::Index(index), i + 1))
}

/// Read a double-quoted key starting at the opening `"` at `start`, honoring
/// `\"` and `\\` escapes. Returns the unescaped key and the index just past the
/// closing `"`, or `None` if the quote is unterminated or an escape is ill-formed.
fn read_quoted_key(chars: &[char], start: usize) -> Option<(String, usize)> {
    // chars[start] is the opening '"'.
    let mut i = start + 1;
    let mut key = String::new();
    while i < chars.len() {
        match chars[i] {
            '\\' => {
                // An escape must be followed by exactly `"` or `\`; a dangling or
                // unknown escape is malformed so it cannot silently swallow a char.
                let next = chars.get(i + 1)?;
                match next {
                    '"' => key.push('"'),
                    '\\' => key.push('\\'),
                    _ => return None,
                }
                i += 2;
            }
            '"' => return Some((key, i + 1)),
            c => {
                key.push(c);
                i += 1;
            }
        }
    }
    // Reached the end without a closing quote.
    None
}

/// Parse an array-index token, accepting only a pure run of ASCII digits.
///
/// Why not `token.parse::<usize>()`: `usize::from_str` accepts a leading `+`
/// (`+1` parses to `1`), so `items.+1` would silently address index 1 instead of
/// surfacing as a missing path. Requiring `[0-9]+` (and rejecting empty, signs,
/// and surrounding whitespace) keeps a typo'd index from quietly pointing at the
/// wrong element. A leading-zero token (`01`) is still accepted as that index:
/// it is unambiguous and harmless, and rejecting it would add a special case
/// without a real authoring hazard.
fn parse_array_index(token: &str) -> Option<usize> {
    if token.is_empty() || !token.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    token.parse::<usize>().ok()
}

/// Parse the JSON block at the tail of `text` (noisy terminal output).
///
/// Convenience wrapper over [`extract_tail_json_bytes`] for callers that
/// already hold a `&str` (e.g. file contents or tests). Output-buffer callers
/// should prefer the byte form so they can pass a bounded tail window without a
/// full-buffer `String` copy.
pub fn extract_tail_json(text: &str) -> Option<Value> {
    extract_tail_json_bytes(text.as_bytes())
}

/// How far back from the end of `bytes` a structural closer may sit and still be
/// tried as the tail block, in bytes.
///
/// Why a bound at all (C1): without it the backward scan tries *every* earlier
/// closer until one parses, so a truncated/garbled final report can silently
/// fall back to an unrelated, much older JSON fragment buried in log noise — and
/// a regression framework asserting on that stale block treats the wrong target
/// as correct. Capping the search to a window near the tail means a broken final
/// block fails to extract (→ assertion failure: "no valid JSON") instead of
/// resolving to ancient noise. The cap is generous enough to skip a few lines of
/// trailing log output printed *after* a real report while still excluding
/// distant history; tune it here if reports routinely sit further from the tail.
const MAX_TAIL_FALLBACK_BYTES: usize = 8 * 1024;

/// Hard ceiling on the byte-steps spent walking candidate blocks, as a
/// multiple of the buffer length.
///
/// This is a backstop, not the primary bound. [`MAX_CANDIDATE_DEPTH`] is what
/// actually keeps the scan linear: it abandons a walk that has nested past what
/// `serde_json` could parse, which caps every individual walk at a constant, so
/// the at most `n` candidates cost O(n) in total. Measured on an adversarial
/// phantom-heavy document, steps-per-byte plateaus at ~97 and stays flat as the
/// input grows; with the depth cap removed the same measurement doubles at every
/// size (245 -> 495 -> 995 -> 1995), i.e. genuinely quadratic. The multiple below
/// sits above that plateau so legitimate input is never cut off, while still
/// bounding input pathological in some way not yet anticipated.
///
/// Why a ceiling at all rather than trusting the two mechanisms: neither is a
/// proof. A failed walk genuinely cannot be skipped on the strength of an
/// earlier failure — `{}{` is a two-line counterexample, where the walk from the
/// last `{` fails while the one from offset 0 succeeds — so the scan really can
/// be asked to attempt one walk per opener.
///
/// Crucially, hitting this ceiling is reported to the caller rather than
/// silently narrowing the result — see `ScanOutcome`. A resource limit must
/// never be allowed to change *which* block is returned.
const MAX_UNRESOLVED_WALK_MULTIPLE: usize = 8 * MAX_CANDIDATE_DEPTH as usize;

/// Nesting depth past which a candidate block is abandoned mid-walk.
///
/// Two jobs, and they pin the value from opposite sides.
///
/// Performance: a block nested deeper than `serde_json` will parse can never
/// produce a result, so walking it is waste. Abandoning early is what keeps a
/// maximally nested run (`{`*k + `}`*k) linear — measured steps-per-byte holds
/// flat at ~97 with this cap and doubles at every input size without it
/// (245 -> 495 -> 995 -> 1995), i.e. genuinely quadratic.
///
/// Correctness: this must not sit *above* `serde_json`'s own recursion limit.
/// If it did, a candidate could balance, pass this check, and then be rejected
/// by the parser for depth alone — indistinguishable at the call site from a
/// malformed block, so the scan would quietly fall back to a shallower *inner*
/// block and assert against the wrong root. Keeping the walk's limit at or below
/// the parser's means depth is always caught here, as an explicit
/// `WalkEnd::GaveUp`, and never as an anonymous parse failure.
///
/// Measured on this `serde_json`: the deepest run of *same-kind* brackets that
/// still parses is 127, for arrays and objects alike. (A mixed document reaches
/// a smaller same-kind count before the parser's limit bites — `[`*126 around an
/// object is the deepest that parses — because the inner value consumes a level
/// this walk cannot see: it counts only the bracket kind it opened with.) So 127
/// is the largest count that can still belong to a parseable document, and
/// anything above it certainly cannot. Setting the cap there closes the gap in
/// both directions: no parseable document is abandoned, and no depth rejection
/// is left for `serde_json` to report anonymously.
///
/// `depth_limit_is_not_below_what_serde_json_accepts` pins that relationship, so
/// a `serde_json` change that moved its limit cannot silently reopen the gap.
const MAX_CANDIDATE_DEPTH: u32 = 127;

/// Parse the JSON block at the tail of `bytes` (noisy terminal output).
///
/// Scans backward from the end of `bytes` over candidate `{`/`[` openers and
/// walks forward from each to its balanced closer, keeping the block that
/// `serde_json` accepts and reaches furthest toward the tail. The **last block
/// that parses and closes** within [`MAX_TAIL_FALLBACK_BYTES`] of the tail is
/// returned.
///
/// Size envelope: the window bounds the block's **closer**, never its opener, so
/// there is no cap on how large an extractable block may be — only on how far
/// from the tail it may end. A block spanning the runner's whole 64 KiB tail
/// window extracts fine as long as it closes near the tail. Bounding the opener
/// instead looks equivalent and is not: it silently caps the document size at
/// the window, which broke every report over 8 KiB once already (see
/// `block_larger_than_fallback_window_still_extracts`).
///
/// Quote handling is derived per candidate block rather than from a parity scan
/// over the whole buffer, so unbalanced quotes in the surrounding log noise — or
/// a tail window that begins mid-string-literal — cannot misclassify structural
/// braces; see [`close_of_block`] for why the previous global mask was unsound.
///
/// Behavior to be aware of (C1): this returns the last *parseable* block, which
/// is not necessarily the last block the program *emitted*. If the final report
/// is truncated or malformed, the scan falls back to an earlier closer (bounded
/// by [`MAX_TAIL_FALLBACK_BYTES`]) and may return a preceding block. Place the
/// JSON report at the very tail of output (nothing after it but a newline) so
/// the intended block is the one extracted; do not rely on extraction to reject
/// a half-written trailing block when an older complete block sits just above
/// it within the window.
///
/// Why a bounded fallback rather than full ambiguity removal: a self-delimiting
/// JSON block at the tail cannot be distinguished from a complete older block
/// with certainty (both parse), so we cannot always know which the author meant.
/// Bounding the fallback to the tail window is the cheap, predictable guard that
/// stops a broken final report from resolving to distant history, and the
/// fixed-behavior tests below pin the remaining (documented) fallback so it
/// cannot drift silently.
///
/// Why a hand-written scan rather than a regex: a regex cannot count brace
/// nesting nor track whether a `{`/`}` sits inside a string literal (where it
/// is data, not structure). A backslash-aware string-skipping scan is required
/// to avoid splitting on braces that live inside `"...{...}..."`.
///
/// Why operate on `&[u8]` (not `&str`): the live PTY output is bytes and may
/// be split mid-UTF-8; taking a slice lets the runner hand us a fixed tail
/// window (see `with_tail`) instead of copying the whole buffer to a `String`
/// every poll. Candidate blocks are validated through `from_utf8` before
/// parsing, so an invalid-UTF-8 region simply fails to parse rather than
/// panicking.
pub fn extract_tail_json_bytes(bytes: &[u8]) -> Option<Value> {
    match scan_tail_json(bytes) {
        ScanOutcome::Found(value) => Some(value),
        // Both "nothing parsed" and "gave up early" surface as no extraction.
        // They are deliberately *not* collapsed inside the scan: see
        // `ScanOutcome::Incomplete` for why an exhausted scan must never fall
        // back to whatever it happened to have found so far.
        ScanOutcome::NotFound | ScanOutcome::Incomplete => None,
    }
}

/// The result of one tail scan, separating "there is no block" from "the scan
/// did not finish".
///
/// Why the distinction matters: the scan improves its answer as it walks further
/// back (an enclosing block supersedes the inner one found first). If a resource
/// limit stops it early, whatever it holds is a *partial-confidence* result — it
/// may be an inner fragment of the block the author meant. Returning that would
/// turn a resource limit into a silently wrong assertion target, the exact class
/// of bug `#29` was about. `Incomplete` therefore yields no value at all, so an
/// exhausted scan fails loudly instead of answering with the wrong root.
enum ScanOutcome {
    /// The scan completed and the best qualifying block parsed.
    Found(Value),
    /// The scan completed; no block qualified.
    NotFound,
    /// The scan hit its walk ceiling, so no result can be trusted.
    Incomplete,
}

/// Back-to-front scan for the tail block. See [`extract_tail_json_bytes`].
fn scan_tail_json(bytes: &[u8]) -> ScanOutcome {
    // The earliest *closer* offset that still counts as "at the tail". The bound
    // is on the closer, never on the opener: `MAX_TAIL_FALLBACK_BYTES` exists to
    // stop a stale block from being resurrected, and what makes a block stale is
    // where it *ends* relative to the tail, not how large it is. Bounding the
    // opener instead silently caps the maximum extractable document at the
    // window size, so a report larger than it — well within the 64 KiB tail the
    // runner hands over — stops extracting entirely. That regression has been
    // shipped once already; keep the comparison below on `close`.
    //
    // Saturating so a buffer shorter than the window admits every closer.
    let closer_floor = bytes.len().saturating_sub(MAX_TAIL_FALLBACK_BYTES);

    // Why there is no "skip offsets already covered by a successful walk"
    // memo here: one was tried and measured to be dead code. The scan moves
    // backward, so every opener it visits after a successful walk has a
    // strictly smaller index than the one that recorded the span — the guard
    // could never fire. Instrumenting it across every shape in the tests below
    // (nested, phantom-heavy, large-report, noisy) counted zero hits, and
    // deleting it changed neither behavior nor a single timing. The work is
    // bounded by `MAX_CANDIDATE_DEPTH` instead; see that constant for the
    // measurement showing it is what actually keeps the scan linear.

    // Backstop for walks that resolve nothing (openers that never balance).
    let mut unresolved_walks_left = bytes.len().saturating_mul(MAX_UNRESOLVED_WALK_MULTIPLE);

    // Scan backward for candidate *openers*, not closers, and validate each by
    // walking forward from it. Why openers: JSON string state is only knowable
    // left-to-right from a point that is provably outside a string, and a
    // structural `{`/`[` is exactly such a point — so `close_of_block` can seed
    // `in_string = false` truthfully. A candidate closer offers no such anchor;
    // classifying one would need quote parity over all the arbitrary noise
    // before it, which is what made the previous whole-buffer mask wrong (see
    // `close_of_block`'s "why not a precomputed mask" note).
    //
    // Because an outer block's opener sits *before* its inner blocks' openers,
    // a backward scan meets inner openers first. We therefore do not return the
    // first candidate that parses; we keep the candidate whose block extends
    // furthest toward the tail, and on a tie prefer the earlier opener (the
    // enclosing block). That reproduces the previous "last closer, matched back
    // to its outermost opener" selection without needing a global mask.
    let mut best: Option<(usize, usize, bool, Value)> = None;
    // Regions the scan declined to evaluate, kept as disjoint byte ranges.
    //
    // Why extents and not offsets: an offset alone cannot tell an *enclosure*
    // from a *sibling* sitting earlier in the buffer, and that is wrong in both
    // directions — it discards a good tail block that merely follows unrelated
    // deep noise, and it lets a stale block stand in for an abandoned tail
    // block.
    //
    // Why a list and not one min/max pair: collapsing every region into
    // `[min_start, max_end)` invents coverage of the gap *between* two real
    // regions. Deep noise wholly before a report (harmless) plus a phantom
    // walk wholly inside it (also harmless) then aggregate into a span that
    // appears to straddle the report, and it is refused though neither real
    // region touches it. Merging is only sound for ranges that actually
    // overlap or abut, which is what `note_abandoned` below does.
    let mut abandoned: Vec<std::ops::Range<usize>> = Vec::new();
    let mut scan = bytes.len();
    while scan > 0 {
        let i = scan - 1;
        scan -= 1;
        let is_opener = bytes[i] == b'{' || bytes[i] == b'[';
        if !is_opener {
            continue;
        }
        if unresolved_walks_left == 0 {
            // Out of allowance with the scan unfinished. Refuse to answer rather
            // than return `best`, which may be an inner fragment.
            return ScanOutcome::Incomplete;
        }
        // Every walk is charged, whether or not it found a closer. Charging only
        // the failed ones left a hole: a candidate that balances but that
        // `serde_json` then rejects has still consumed the work, and measurement
        // put that at ~5% of total steps on a phantom-heavy document. Since the
        // backstop exists to bound total work, work it cannot see is work it
        // cannot bound.
        let (walk_end, steps) = close_of_block_budgeted(bytes, i, unresolved_walks_left);
        unresolved_walks_left -= steps;
        let close = match walk_end {
            WalkEnd::Closed(close) => close,
            // Provably malformed. This is the ordinary negative result that the
            // documented C1 fallback is built on: keep scanning, and an earlier
            // complete block (within the closer floor) may legitimately win.
            WalkEnd::Unbalanced => continue,
            // We stopped analysing this candidate without deciding it. Do NOT
            // abort the scan: an abandoned candidate is often a phantom `{`
            // inside a string value, whose "depth" is just string data being
            // counted, and the real enclosing block sits at a smaller offset
            // that the backward scan has not reached yet. Aborting here would
            // lose that block entirely.
            //
            // What we must not do is *fall back* past this point. Record the
            // abandoned candidate; if the scan ends holding only a block nested
            // inside it, that block is a fragment of something we declined to
            // evaluate, and answering with it would assert against the wrong
            // root. A block found later at a smaller offset encloses this one
            // and is strictly better information, so it is allowed to win.
            WalkEnd::GaveUp => {
                // Record the bytes this walk actually looked at. It began at `i`
                // and stopped after `steps` bytes without reaching a verdict, so
                // that is precisely the region whose structure is unknown to us.
                note_abandoned(&mut abandoned, i..i + steps);
                continue;
            }
        };
        // The tail-window test, applied to the closer. An opener far above the
        // floor is fine; a block that *ends* above it is the stale one we refuse.
        if close < closer_floor {
            continue;
        }
        // Once a candidate ending at `close` is held, an opener scanned later
        // (i.e. earlier in the buffer) only wins by reaching at least as far,
        // so a shorter block can be skipped before the parse attempt.
        // Rank candidates by (not-in-quoted-prose, then closer position). A
        // block inside quoted log text loses to any block outside it, however
        // much further toward the tail it sits — that is what stops a
        // JSON-looking run in a log line from displacing the real report. Among
        // equally-quoted candidates the tail-most still wins, as in 1.2.2.
        let quoted = opener_is_in_quoted_prose(bytes, i);
        let outranks = best.as_ref().is_none_or(|(_, best_close, best_quoted, _)| {
            match (*best_quoted, quoted) {
                (true, false) => true,
                (false, true) => false,
                _ => close >= *best_close,
            }
        });
        if !outranks {
            continue;
        }
        // `serde_json` is the final arbiter: a run that merely balances (log
        // noise with matched braces) is rejected here and the scan continues to
        // an earlier opener, so validity is never inferred from the byte scan.
        let Ok(candidate) = std::str::from_utf8(&bytes[i..=close]) else {
            continue;
        };
        if let Ok(value) = serde_json::from_str::<Value>(candidate) {
            best = Some((i, close, quoted, value));
        }
    }
    // A candidate is disqualified by an abandoned region when that region could
    // hold the block the author meant — i.e. when it overlaps the candidate
    // (so the candidate may be a fragment of it) or when it reaches nearer the
    // tail than the candidate does (so it may be the real tail block, making
    // this candidate a fallback we are not entitled to make).
    //
    // A region that lies wholly *before* the candidate disqualifies nothing: it
    // is a sibling earlier in the buffer, and the candidate we are holding was
    // evaluated successfully on its own bytes.
    let gave_up = !abandoned.is_empty();
    let compromised = |open: usize, close: usize| {
        abandoned.iter().any(|region| {
            // Wholly inside the candidate: harmless. The candidate balanced and
            // parsed, so its own walk read those bytes correctly and whatever we
            // abandoned there was a phantom — a `{` sitting in a string literal,
            // counted as structure only by the doomed walk that started on it.
            // This case must stay allowed, or a report whose string values
            // contain braces stops extracting.
            if region.start >= open && region.end <= close + 1 {
                return false;
            }
            // Otherwise the region is disqualifying if it could hold the block
            // the author meant: either it overlaps the candidate (which may then
            // be a fragment of it) or it reaches nearer the tail (so it may be
            // the real tail block, making this candidate an unearned fallback).
            let overlaps = region.start <= close && region.end > open;
            let nearer_the_tail = region.end > close;
            overlaps || nearer_the_tail
        })
    };

    match best {
        Some((open, close, _, _)) if compromised(open, close) => ScanOutcome::Incomplete,
        Some((_, _, _, value)) => ScanOutcome::Found(value),
        // Nothing parsed at all. If that is because we abandoned a candidate
        // rather than because the buffer holds no block, say so: the caller
        // treats both as "no extraction", but the distinction keeps the
        // guarantee honest and is visible to tests.
        None if gave_up => ScanOutcome::Incomplete,
        None => ScanOutcome::NotFound,
    }
}

/// Whether the opener at `open_idx` sits inside a quoted run on **its own line**.
///
/// Replays string state from the start of the line containing `open_idx` using
/// the same escape rule JSON itself uses: a `"` toggles the state unless it is
/// preceded by an odd run of backslashes *while already inside* a string.
/// Ending inside a quoted run means a JSON-looking block there is log text
/// rather than a report.
///
/// Why escapes must be honoured rather than counted. A logger that prints a
/// quoted message escapes the quotes inside it (`log: "he said \" hi"`), so a
/// naive count of `"` bytes flips parity on every escape and judges the rest of
/// the line — and any genuine report competing with it — backwards. Measured
/// against the 1.2.2 binary over a backslash-run ladder (`\"`, `\\"`, `\\\"`,
/// …): its verdict alternates with the parity of the backslash run, exactly as
/// this rule does, and does *not* alternate with the raw count of quote bytes.
///
/// Why a backslash is honoured only inside a string. The same ladder, run with
/// the backslashes placed *outside* any quoted run, does not alternate at all:
/// 1.2.2 treats a backslash in unquoted prose as an ordinary character, so `\"`
/// with no string open still opens one. Escapes are a property of string
/// interiors, not of the byte before a quote.
///
/// Why this does not reintroduce the assumption `#29` removed. That bug came
/// from parity carried across *arbitrary* distance: a single stray `"` anywhere
/// earlier in the buffer — or a tail window that happened to cut through a
/// string literal — inverted the state for every byte after it, so a real report
/// went unfound or a stale block was returned in its place. The state was
/// inherited from text that had no obligation to balance its quotes.
///
/// A newline ends that inheritance. Terminal output is line-oriented, so a log
/// line with an unbalanced quote cannot affect the classification of the next
/// line: the signal is local, bounded, and re-derived per line rather than
/// assumed. The runner's tail window can still cut through the middle of the
/// first line, whose surviving fragment is then classified on the quotes it
/// happens to retain — that residue is confined to one line and, because this
/// judgement only ranks, can at worst reorder candidates, never discard the
/// real report the way whole-buffer parity could.
///
/// This is used only to *deprioritise*, never to discard: a candidate flagged
/// here is still returned when nothing outside quoted prose parses, which keeps
/// the round-15 decision (a quoted JSON run with no real report anywhere is
/// still extracted) while restoring 1.2.2's verdict whenever a genuine report is
/// also present.
fn opener_is_in_quoted_prose(bytes: &[u8], open_idx: usize) -> bool {
    let line_start = bytes[..open_idx]
        .iter()
        .rposition(|&b| b == b'\n')
        .map_or(0, |nl| nl + 1);
    let mut in_string = false;
    let mut i = line_start;
    while i < open_idx {
        match bytes[i] {
            // Inside a string, a backslash consumes the next byte whatever it
            // is, so an escaped quote cannot close the run. Outside a string it
            // is ordinary prose punctuation and escapes nothing.
            b'\\' if in_string => i += 1,
            b'"' => in_string = !in_string,
            _ => {}
        }
        i += 1;
    }
    in_string
}

/// Record a region the scan declined to evaluate, merging it into `regions`
/// when it actually touches one already there.
///
/// Merging is deliberately conditional. Ranges that overlap or abut describe one
/// contiguous unevaluated area, so fusing them loses nothing. Ranges separated by
/// a gap do not: the gap is buffer the scan *did* evaluate, and pretending
/// otherwise manufactures coverage no real region has — a report sitting in that
/// gap would be refused because of two unrelated regions on either side of it.
///
/// Bounded memory without that unsoundness: the backward scan meets openers in
/// descending order, so consecutive abandoned walks nearly always overlap and
/// fuse into the entry already at the end of the list. The list therefore tracks
/// the number of *disjoint* unevaluated areas — a handful in practice, and one
/// on the degenerate all-openers buffer that previously produced ~65k entries.
fn note_abandoned(regions: &mut Vec<std::ops::Range<usize>>, new: std::ops::Range<usize>) {
    // Fuse with any existing range that overlaps or abuts `new`, absorbing them
    // into one span; the rest are kept untouched.
    let mut merged = new;
    regions.retain(|region| {
        let touches = region.start <= merged.end && merged.start <= region.end;
        if touches {
            merged.start = merged.start.min(region.start);
            merged.end = merged.end.max(region.end);
        }
        !touches
    });
    regions.push(merged);
}

/// Why a candidate walk ended, kept distinct because two of these are ordinary
/// results and one is an admission that the scan stopped looking.
///
/// The distinction is the whole point: "this block is malformed" and "I gave up
/// analysing this block" look almost identical at the call site and mean
/// opposite things. The first licenses the documented C1 fallback to an earlier
/// complete block; the second must not, because the block we abandoned might be
/// the very one the author meant, and answering with a smaller inner block would
/// assert against the wrong root.
enum WalkEnd {
    /// The block balanced; this is the offset of its closer.
    Closed(usize),
    /// The block provably never balances within the buffer — it is truncated or
    /// garbled. An ordinary negative result: the scan may fall back.
    Unbalanced,
    /// The walk was abandoned before it could decide (nested past
    /// [`MAX_CANDIDATE_DEPTH`], or out of allowance). The scan learned nothing
    /// about this block and must not answer with a different one.
    GaveUp,
}

/// Find the closer that balances the opener at `open_idx`, or `None` if the
/// block never balances before the end of `bytes`.
///
/// Walks forward tracking brace depth and string state, where the initial
/// `in_string = false` is correct by construction: the structural `{`/`[` at
/// `open_idx` cannot itself lie inside a string literal of the block it opens.
/// A `"` toggles string state unless escaped by an odd run of backslashes.
///
/// Why not a precomputed whole-buffer mask (the previous `string_mask`): the
/// input is noisy terminal output, which has no obligation to contain balanced
/// double quotes, and the caller may hand us an arbitrary trailing byte window
/// that begins mid-string-literal. Seeding such a pass with `in_string = false`
/// was an assumption, not a fact — a single stray `"` inverted the parity for
/// every byte after it, so structural braces were read as string data (a valid
/// tail block went unfound) and string bodies were read as structure (a stale
/// older block was returned in its place). Deriving the state per candidate
/// block removes the dependency on the surrounding text entirely, at the cost
/// of re-scanning each candidate — acceptable because the caller charges every
/// byte stepped over against one shared budget (see
/// [`MAX_UNRESOLVED_WALK_MULTIPLE`]) and the scan stops at the first imbalance.
///
/// `budget` caps how many bytes this walk may step over; the returned count is
/// what it actually consumed, so the caller can debit a single shared allowance
/// across all candidates. Exhausting the budget reports `None` (no closer found)
/// — the same outcome as a block that never balances, which is the safe
/// direction: extraction fails loudly rather than returning a guessed block.
fn close_of_block_budgeted(bytes: &[u8], open_idx: usize, budget: usize) -> (WalkEnd, usize) {
    let (open_byte, close_byte) = match bytes[open_idx] {
        b'{' => (b'{', b'}'),
        b'[' => (b'[', b']'),
        _ => return (WalkEnd::Unbalanced, 0),
    };
    // `depth` counts only the opener's own bracket kind and is what decides
    // balance. `nesting` counts both kinds, which is what `serde_json` actually
    // limits — a `[` block with an object inside spends a level on that object
    // too. Tracking them separately keeps balance detection exact while still
    // catching depth the parser would reject.
    let mut depth: u32 = 0;
    let mut nesting: u32 = 0;
    let mut in_string = false;
    let mut escaped = false;
    let mut steps: usize = 0;
    for (offset, &b) in bytes.iter().enumerate().skip(open_idx) {
        if steps == budget {
            // Out of allowance mid-walk: undecided, not disproved.
            return (WalkEnd::GaveUp, steps);
        }
        steps += 1;
        if in_string {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        if b == b'"' {
            in_string = true;
        } else if b == b'{' || b == b'[' {
            nesting += 1;
            if b == open_byte {
                depth += 1;
            }
            // Nested past anything `serde_json` would accept, so continuing to
            // walk cannot produce a usable block. Reported as `GaveUp` rather
            // than `Unbalanced`: the block may well be balanced and simply too
            // deep, so the scan has NOT shown it malformed and must not fall
            // back to a shallower inner block in its place.
            if nesting > MAX_CANDIDATE_DEPTH {
                return (WalkEnd::GaveUp, steps);
            }
        } else if b == b'}' || b == b']' {
            nesting = nesting.saturating_sub(1);
            if b == close_byte {
                // Depth is at least 1 here: the loop starts on the opener, which
                // increments before any closer of the same kind can be reached.
                depth -= 1;
                if depth == 0 {
                    return (WalkEnd::Closed(offset), steps);
                }
            }
        }
    }
    // Ran off the end of the buffer with the block still open: provably
    // unbalanced within what we were given.
    (WalkEnd::Unbalanced, steps)
}

/// A compact one-line rendering of a JSON value for failure messages.
fn compact(value: &Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "<unserializable>".to_string())
}

/// A human-readable type name for failure messages.
fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extracts_tail_block_from_noisy_output() {
        // A JSON object printed after log noise must be extracted, ignoring the
        // surrounding non-JSON text.
        let text = "starting up...\nLOG: doing work\n{\"status\": \"ok\"}\n";
        let value = extract_tail_json(text).expect("should extract");
        assert_eq!(value, json!({"status": "ok"}));
    }

    #[test]
    fn extracts_last_of_multiple_blocks() {
        // When several JSON blocks appear, the last parseable one wins so a
        // final report supersedes earlier intermediate JSON.
        let text = "{\"phase\": 1}\nmore\n{\"phase\": 2, \"done\": true}";
        let value = extract_tail_json(text).expect("should extract");
        assert_eq!(value, json!({"phase": 2, "done": true}));
    }

    #[test]
    fn does_not_split_on_braces_inside_strings() {
        // A closing brace inside a string literal must not be mistaken for a
        // structural boundary; the whole object must still parse.
        let text = "noise {\"msg\": \"a } b { c\", \"ok\": true}";
        let value = extract_tail_json(text).expect("should extract");
        assert_eq!(value, json!({"msg": "a } b { c", "ok": true}));
    }

    #[test]
    fn handles_escaped_quote_inside_string() {
        // An escaped quote must not prematurely end the string, so a brace after
        // it is still string data.
        let text = "x {\"q\": \"he said \\\"hi}\\\"\", \"n\": 1}";
        let value = extract_tail_json(text).expect("should extract");
        assert_eq!(value, json!({"q": "he said \"hi}\"", "n": 1}));
    }

    #[test]
    fn handles_nested_objects_and_arrays() {
        // Nested structures must be balanced correctly across depth.
        let text = "log\n{\"a\": {\"b\": [1, 2, {\"c\": 3}]}}";
        let value = extract_tail_json(text).expect("should extract");
        assert_eq!(value, json!({"a": {"b": [1, 2, {"c": 3}]}}));
    }

    #[test]
    fn extracts_tail_array_block() {
        // A top-level array block must be extracted just like an object.
        let text = "result: [1, 2, 3]";
        let value = extract_tail_json(text).expect("should extract");
        assert_eq!(value, json!([1, 2, 3]));
    }

    #[test]
    fn returns_none_when_no_json_present() {
        // Plain text with no balanced JSON block must yield None.
        assert!(extract_tail_json("no json here at all").is_none());
        assert!(extract_tail_json("unbalanced {oops").is_none());
    }

    #[test]
    fn extract_tail_json_bytes_tolerates_invalid_utf8_prefix() {
        // A tail window can begin mid-UTF-8 (the runner slices raw bytes). An
        // invalid-UTF-8 prefix before the JSON must not panic and must not stop
        // the trailing valid block from being extracted.
        let mut bytes = vec![0xff, 0xfe];
        bytes.extend_from_slice(b" noise {\"ok\": true}");
        let value = extract_tail_json_bytes(&bytes).expect("should extract");
        assert_eq!(value, json!({"ok": true}));
    }

    #[test]
    fn extract_tail_json_bytes_rejects_block_with_invalid_utf8_inside() {
        // If the only balanced block contains invalid UTF-8 it cannot be valid
        // JSON; extraction must yield None rather than panicking on the slice.
        let bytes = [b'{', 0xff, b'}'];
        assert!(extract_tail_json_bytes(&bytes).is_none());
    }

    #[test]
    fn navigate_resolves_object_and_array_indices() {
        // Dotted keys index objects and numeric tokens index arrays.
        let root = json!({"result": {"items": [{"name": "first"}, {"name": "second"}]}});
        assert_eq!(
            navigate_one(&root, "result.items.1.name"),
            Some(&json!("second"))
        );
        assert_eq!(
            navigate_one(&root, "result.items.0.name"),
            Some(&json!("first"))
        );
    }

    #[test]
    fn navigate_returns_none_for_absent_path() {
        // A path that does not exist (missing key, out-of-range index, descent
        // into a scalar) must resolve to None.
        let root = json!({"a": {"b": 1}, "list": [10]});
        assert!(navigate_one(&root, "a.missing").is_none());
        assert!(navigate_one(&root, "list.5").is_none());
        assert!(navigate_one(&root, "a.b.c").is_none());
    }

    #[test]
    fn equals_is_type_aware() {
        // equals compares typed JSON values: a string must not equal a number,
        // and bool/null are matched exactly.
        let root = json!({"status": "success", "code": 200, "ok": true, "data": null});
        assert!(evaluate(&root, "status", &JsonCheck::Equals(json!("success"))).passed);
        assert!(evaluate(&root, "code", &JsonCheck::Equals(json!(200))).passed);
        assert!(evaluate(&root, "ok", &JsonCheck::Equals(json!(true))).passed);
        assert!(evaluate(&root, "data", &JsonCheck::Equals(json!(null))).passed);
        // Type mismatch: the string "200" must not equal the number 200.
        assert!(!evaluate(&root, "code", &JsonCheck::Equals(json!("200"))).passed);
    }

    #[test]
    fn equals_failure_message_shows_both_sides() {
        // A mismatch message must show expected and actual so the diff is clear.
        let root = json!({"status": "fail"});
        let r = evaluate(&root, "status", &JsonCheck::Equals(json!("success")));
        assert!(!r.passed);
        let msg = r.message.unwrap();
        assert!(msg.contains("success") && msg.contains("fail"));
    }

    #[test]
    fn contains_only_matches_string_targets() {
        // contains is a substring test for strings; a non-string target is a
        // type error rather than a silent stringify.
        let root = json!({"message": "token expired", "count": 3});
        assert!(evaluate(&root, "message", &JsonCheck::Contains("expired".into())).passed);
        assert!(!evaluate(&root, "message", &JsonCheck::Contains("valid".into())).passed);
        let type_err = evaluate(&root, "count", &JsonCheck::Contains("3".into()));
        assert!(!type_err.passed);
        assert!(type_err.message.unwrap().contains("string"));
    }

    #[test]
    fn exists_checks_presence() {
        // exists passes when the path resolves and fails when it does not.
        let root = json!({"result": {"items": []}});
        assert!(evaluate(&root, "result.items", &JsonCheck::Exists).passed);
        assert!(!evaluate(&root, "result.missing", &JsonCheck::Exists).passed);
    }

    #[test]
    fn navigate_rejects_signed_and_spaced_array_index() {
        // (R2) An array-index token must be a pure digit run. A leading `+` or
        // surrounding whitespace must NOT be coerced into an index (which
        // `usize::parse` would do for `+1`), so a typo surfaces as a missing
        // path rather than silently addressing a different element.
        let root = json!({"items": [10, 20, 30]});
        assert!(navigate_one(&root, "items.+1").is_none());
        assert!(navigate_one(&root, "items. 1").is_none());
        assert!(navigate_one(&root, "items.1 ").is_none());
        // The plain digit index still resolves.
        assert_eq!(navigate_one(&root, "items.1"), Some(&json!(20)));
    }

    #[test]
    fn navigate_dotted_path_remains_backward_compatible() {
        // The v0.3 dotted grammar must still resolve unchanged: object keys and
        // `items.0` numeric indices are the backward-compatibility core.
        let root = json!({"result": {"status": "ok", "items": [{"name": "first"}]}});
        assert_eq!(navigate_one(&root, "result.status"), Some(&json!("ok")));
        assert_eq!(
            navigate_one(&root, "result.items.0.name"),
            Some(&json!("first"))
        );
    }

    #[test]
    fn navigate_bracket_index_addresses_array() {
        // A bracketed numeric index `[0]` must address an array element, and
        // compose with following dotted keys.
        let root = json!({"items": [{"name": "first"}, {"name": "second"}]});
        assert_eq!(navigate_one(&root, "items[0].name"), Some(&json!("first")));
        assert_eq!(navigate_one(&root, "items[1].name"), Some(&json!("second")));
    }

    #[test]
    fn navigate_leading_bracket_index_addresses_root_array() {
        // A path may start with a bracket group when the root is an array.
        let root = json!(["a", "b", "c"]);
        assert_eq!(navigate_one(&root, "[2]"), Some(&json!("c")));
    }

    #[test]
    fn navigate_bracket_quoted_key_addresses_dotted_object_key() {
        // A bracketed double-quoted key must address an object key verbatim,
        // including a key that itself contains a `.` (which the dotted form would
        // otherwise split on).
        let root = json!({"a.b": {"value": 7}, "plain": 1});
        assert_eq!(navigate_one(&root, "[\"a.b\"].value"), Some(&json!(7)));
        // A quoted key works for an ordinary key too.
        assert_eq!(navigate_one(&root, "[\"plain\"]"), Some(&json!(1)));
    }

    #[test]
    fn navigate_mixed_bracket_and_dotted_segments_compose() {
        // The forms compose: object key, quoted dotted key, array index, object
        // key, in one path `a["b.c"][0].d`.
        let root = json!({"a": {"b.c": [{"d": "deep"}]}});
        assert_eq!(navigate_one(&root, "a[\"b.c\"][0].d"), Some(&json!("deep")));
    }

    #[test]
    fn navigate_bracket_quoted_key_honors_escapes() {
        // Quote-internal `\"` and `\\` escapes must be unescaped so a key
        // containing a quote or backslash is addressable.
        let root = json!({"he\"y": 1, "back\\slash": 2});
        assert_eq!(navigate_one(&root, "[\"he\\\"y\"]"), Some(&json!(1)));
        assert_eq!(navigate_one(&root, "[\"back\\\\slash\"]"), Some(&json!(2)));
    }

    #[test]
    fn navigate_dotted_numeric_index_still_coexists_with_brackets() {
        // The legacy `items.0` dotted index must keep working alongside the new
        // `items[0]` bracket index (both resolve to the same element).
        let root = json!({"items": [10, 20, 30]});
        assert_eq!(
            navigate_one(&root, "items.1"),
            navigate_one(&root, "items[1]")
        );
        assert_eq!(navigate_one(&root, "items.1"), Some(&json!(20)));
    }

    #[test]
    fn navigate_bracket_empty_quoted_key_addresses_empty_object_key() {
        // (AC-8) `[""]` is a quoted bracket key whose content is the empty string,
        // so it must address an object's empty-string key verbatim: it resolves
        // when that key exists and is None when it does not. This is distinct from
        // the unquoted empty bracket `[]`, which is a malformed (non-numeric)
        // index and rejected as None (see the malformed-bracket test). Pinning
        // `[""]` keeps the quoted-key path from accidentally treating an empty key
        // as malformed.
        let present = json!({"": 9});
        assert_eq!(navigate_one(&present, "[\"\"]"), Some(&json!(9)));
        // The same empty-key selector is None when no empty key exists.
        let absent = json!({"a": 1});
        assert!(navigate_one(&absent, "[\"\"]").is_none());
    }

    #[test]
    fn navigate_consecutive_bracket_quoted_keys_resolve_nested_objects() {
        // (AC-8) Two consecutive bracket-quoted keys `["a"]["b"]` must walk an
        // object key then a nested object key, equivalent to the dotted `a.b`.
        // This pins that a bracket group may directly follow another bracket group
        // (no separating `.`) for object descent, not only after a dotted token.
        let root = json!({"a": {"b": "deep"}});
        assert_eq!(navigate_one(&root, "[\"a\"][\"b\"]"), Some(&json!("deep")));
        assert_eq!(
            navigate_one(&root, "[\"a\"][\"b\"]"),
            navigate_one(&root, "a.b")
        );
    }

    #[test]
    fn navigate_array_index_out_of_range_and_negative_resolve_to_none() {
        // (recommended) An array index past the end resolves to None (missing
        // path), and a negative index `[-1]` is not a digit run so it is a
        // malformed bracket -> None (pitty has no Python-style negative
        // indexing). Both surface as a missing path rather than a panic or a
        // wrap-around to an unintended element.
        let root = json!({"items": [10, 20, 30]});
        assert!(navigate_one(&root, "items[3]").is_none());
        assert!(navigate_one(&root, "items.3").is_none());
        assert!(navigate_one(&root, "items[-1]").is_none());
    }

    #[test]
    fn navigate_malformed_brackets_resolve_to_none_without_panic() {
        // (Robustness) Malformed paths must resolve to None (missing path), never
        // panic: an unterminated bracket, an unterminated quote, an ill-formed
        // escape, a non-numeric bare bracket index, and trailing junk after a
        // quoted key all fail cleanly.
        let root = json!({"items": [1, 2], "a": {"b": 3}});
        assert!(navigate_one(&root, "items[0").is_none()); // unterminated bracket
        assert!(navigate_one(&root, "items[\"oops").is_none()); // unterminated quote
        assert!(navigate_one(&root, "items[\"bad\\x\"]").is_none()); // ill-formed escape
        assert!(navigate_one(&root, "items[abc]").is_none()); // non-numeric bare index
        assert!(navigate_one(&root, "a[\"b\"x]").is_none()); // junk after quoted key
        assert!(navigate_one(&root, "a[]").is_none()); // empty bracket
    }

    #[test]
    fn navigate_rejects_empty_path_tokens() {
        // (R3) An empty token from `a..b` or a trailing `a.` must resolve to None
        // (missing path), not silently match an empty-string key or descend
        // oddly, so a dotted-path typo is surfaced as a failure.
        let root = json!({"a": {"b": 1}, "": {"x": 9}});
        assert!(navigate_one(&root, "a..b").is_none());
        assert!(navigate_one(&root, "a.").is_none());
        // A leading `.` (`.a`) has no segment before the separator and must also
        // resolve to None, matching the old `split('.')` empty-first-token reject;
        // the bracket-aware tokenizer must not silently accept it as key `a`.
        assert!(navigate_one(&root, ".a").is_none());
        // Even though a literal "" key exists, an empty token never addresses it.
        assert!(navigate_one(&root, "").is_some()); // empty *path* is the root (distinct from empty token)
    }

    #[test]
    fn evaluate_resolves_bracket_quoted_dotted_key_for_every_check() {
        // (recommended) The bracket-quoted key path must flow through `evaluate`,
        // not just `navigate_one`: a key containing a `.` (`a["b.c"]`) must be
        // addressable by equals, contains, and exists. This guards the integration
        // seam between path navigation and the check kinds for the bracket form.
        let root = json!({"a": {"b.c": "token expired"}});
        assert!(
            evaluate(
                &root,
                "a[\"b.c\"]",
                &JsonCheck::Equals(json!("token expired"))
            )
            .passed
        );
        assert!(evaluate(&root, "a[\"b.c\"]", &JsonCheck::Contains("expired".into())).passed);
        assert!(evaluate(&root, "a[\"b.c\"]", &JsonCheck::Exists).passed);
        // A bracket-quoted key that does not exist fails every value check as a
        // missing path rather than panicking.
        assert!(!evaluate(&root, "a[\"x.y\"]", &JsonCheck::Exists).passed);
        assert!(
            !evaluate(
                &root,
                "a[\"x.y\"]",
                &JsonCheck::Equals(json!("token expired"))
            )
            .passed
        );
    }

    #[test]
    fn deeply_nested_object_rejects_without_panic() {
        // (R1) A pathologically deep object must not abort the process via a
        // stack overflow. serde_json's default recursion limit (128) rejects the
        // whole deep block with an Err rather than overflowing, so a broken/
        // hostile child output cannot crash the runner. This pins that depths
        // well past the limit stay a graceful Err (not a panic) on this
        // serde_json version.
        for depth in [200usize, 500] {
            let mut text = String::new();
            for _ in 0..depth {
                text.push_str("{\"a\":");
            }
            text.push('1');
            for _ in 0..depth {
                text.push('}');
            }
            assert!(
                serde_json::from_str::<Value>(&text).is_err(),
                "depth {depth} unexpectedly parsed; recursion-limit assumption broken"
            );
            // The tail extractor must not panic on the deep input, and must not
            // answer with an inner sub-block either. An over-deep block is one
            // the scan *abandoned* rather than disproved, so falling back to a
            // shallower fragment of it would assert against the wrong root — the
            // C1 fallback is for blocks shown to be malformed, not for blocks we
            // declined to evaluate. (This comment previously blessed returning
            // an inner block, contradicting the guarantee SCHEMA.md states.)
            assert!(
                extract_tail_json(&text).is_none(),
                "depth {depth}: an abandoned block must not resolve to an inner fragment"
            );
        }
    }

    #[test]
    fn deeply_nested_truncated_tail_extracts_none_without_panic() {
        // (R1) When an over-deep block is the only candidate near the tail and it
        // cannot parse, extraction is a clean None (assertion failure), never a
        // panic. The over-deep openers are left unbalanced (no matching closers),
        // so there is no parseable block at all — exercising the "deep input ->
        // None, not abort" path directly.
        let depth = 400usize;
        let mut text = String::from("noise ");
        text.push_str(&"{\"a\":".repeat(depth));
        // No closing braces: nothing balances, so the only thing the scan can
        // find is the deep run, which never closes -> None.
        assert!(extract_tail_json(&text).is_none());
    }

    #[test]
    fn truncated_final_block_falls_back_to_prior_block() {
        // (C1) Documented, fixed behavior: extraction returns the last
        // *parseable* block, which may not be the last *emitted* block. When the
        // final block is truncated, a preceding complete block within the tail
        // fallback window is returned. This pins the fallback so the documented
        // behavior cannot drift silently.
        let text = "{\"phase\": 1, \"final\": true}\nmore log\n{\"phase\": 2, \"trunc";
        let value = extract_tail_json(text).expect("should fall back to prior block");
        assert_eq!(value, json!({"phase": 1, "final": true}));
    }

    #[test]
    fn unbalanced_quote_in_noise_does_not_return_a_stale_block() {
        // (#29 failure mode A) Guarantee: an odd number of stray quotes in the
        // surrounding log output must not cause an older block to be returned in
        // place of the complete block at the tail. The stray `"` used to open a
        // phantom string that swallowed the final block's closer, so the scan
        // fell back to the earlier, still-correctly-parsed block.
        let text = "{\"phase\":\"old\"}\nwarning: unmatched \" in pattern\n{\"phase\":\"new\"}\n";
        let value = extract_tail_json_bytes(text.as_bytes()).expect("should extract tail block");
        assert_eq!(value, json!({"phase": "new"}));
    }

    #[test]
    fn unbalanced_quote_in_noise_does_not_hide_the_tail_block() {
        // (#29 failure mode B) Guarantee: a complete block sitting at the very
        // tail — the placement the module doc instructs authors to use — is still
        // found when preceding noise contains an unmatched quote. This previously
        // returned None and surfaced as a spurious extraction timeout.
        let text = "warning: unmatched \" in pattern\n{\"status\":\"ok\"}\n";
        let value = extract_tail_json_bytes(text.as_bytes()).expect("should extract tail block");
        assert_eq!(value, json!({"status": "ok"}));
    }

    #[test]
    fn quote_parity_in_noise_does_not_change_the_extracted_block() {
        // (#29) Guarantee: extraction depends only on the block itself, not on
        // the quote parity of unrelated preceding text. The same report must be
        // extracted whether the noise above it has an even or an odd number of
        // quotes — the control case that isolated parity as the cause.
        let report = "{\"status\":\"ok\",\"code\":42}\n";
        let expected = json!({"status": "ok", "code": 42});
        let balanced = format!("LOG: he said \"hello\" and left\n{report}");
        let odd = format!("LOG: he said \"hello and left\n{report}");
        assert_eq!(extract_tail_json(&balanced), Some(expected.clone()));
        assert_eq!(extract_tail_json(&odd), Some(expected));
    }

    #[test]
    fn tail_window_starting_mid_string_literal_still_extracts() {
        // (#29 truncation trigger) Guarantee: the runner hands over only a
        // trailing byte window, which may slice through a quoted string. A window
        // whose start lies inside a string literal must still extract the report,
        // rather than inheriting a wrong assumed string state from the cut.
        let report = "{\"status\":\"ok\",\"code\":42}\n";
        let truncated = format!("hello\" and left\n{report}");
        assert_eq!(
            extract_tail_json_bytes(truncated.as_bytes()),
            Some(json!({"status": "ok", "code": 42})),
        );
    }

    #[test]
    fn brace_inside_string_in_noise_is_not_treated_as_structure() {
        // (#29) Guarantee: fixing quote parity must not regress the original
        // purpose of string tracking — braces inside the *block's own* string
        // literals stay data. Here an unmatched quote in the noise precedes a
        // block whose string value contains braces and an escaped quote.
        let text = "oops \" here\n{\"msg\": \"a } b { c \\\" d\", \"ok\": true}\n";
        let value = extract_tail_json(text).expect("should extract");
        assert_eq!(value, json!({"msg": "a } b { c \" d", "ok": true}));
    }

    #[test]
    fn unterminated_string_in_final_block_falls_back_to_prior_block() {
        // (#29 / C1) Guarantee: a final block cut off inside a string literal is
        // not balanced, so it is not returned; the documented bounded fallback to
        // the prior complete block still applies. This pins that an unterminated
        // quote inside a candidate is rejected rather than silently swallowing
        // the rest of the buffer.
        let text = "{\"phase\": 1, \"final\": true}\nlog\n{\"phase\": 2, \"msg\": \"trunc";
        let value = extract_tail_json(text).expect("should fall back to prior block");
        assert_eq!(value, json!({"phase": 1, "final": true}));
    }

    #[test]
    fn distant_block_beyond_fallback_window_is_not_returned() {
        // (C1) The fallback is bounded: a parseable block sitting farther than
        // MAX_TAIL_FALLBACK_BYTES before the tail (with only a broken/garbled
        // trailing block near the tail) must NOT be resurrected. This stops a
        // broken final report from silently resolving to ancient history.
        let mut text = String::from("{\"old\": \"report\"}");
        // Push the old block far above the tail window with non-JSON noise.
        text.push_str(&"x".repeat(MAX_TAIL_FALLBACK_BYTES + 1024));
        // A truncated trailing block near the tail yields no parseable closer.
        text.push_str("\n{\"new\": \"trunc");
        assert!(
            extract_tail_json(&text).is_none(),
            "a block beyond the fallback window must not be returned"
        );
    }

    #[test]
    fn block_larger_than_fallback_window_still_extracts() {
        // (C1 envelope) Guarantee: MAX_TAIL_FALLBACK_BYTES bounds where a block
        // *ends*, not how large it may be. A single report at the very tail whose
        // body exceeds the window has its opener far above the floor, and must
        // still extract. A previous opener-anchored bound broke exactly this,
        // turning any report over 8 KiB into a "no valid JSON block" timeout.
        let payload = "A".repeat(9000);
        let doc = format!(r#"{{"payload":"{payload}","status":"ok"}}"#);
        assert!(
            doc.len() > MAX_TAIL_FALLBACK_BYTES,
            "fixture must exceed window"
        );
        let value = extract_tail_json(&doc).expect("a >8 KiB tail block must extract");
        assert_eq!(value["status"], json!("ok"));
        assert_eq!(value["payload"], json!(payload));
    }

    /// The tail window `runner::TAIL_JSON_WINDOW` hands to the extractor. Mirrored
    /// here so the size-envelope tests state the real upper end they pin.
    const TAIL_JSON_WINDOW_BYTES: usize = 64 * 1024;

    #[test]
    fn block_spanning_the_full_runner_tail_window_extracts() {
        // (C1 envelope) Pins the upper end of the supported size range: the runner
        // hands `extract_tail_json_bytes` a 64 KiB tail window, so a report filling
        // that window — far larger than MAX_TAIL_FALLBACK_BYTES — must extract.
        // Together with the test above this fixes the 8 KiB..64 KiB band that the
        // opener-anchored bound silently dropped.
        let payload = "B".repeat(60 * 1024);
        let doc = format!(r#"{{"payload":"{payload}","status":"ok"}}"#);
        assert!(
            doc.len() > MAX_TAIL_FALLBACK_BYTES && doc.len() < TAIL_JSON_WINDOW_BYTES,
            "fixture must sit inside the 8 KiB..64 KiB band"
        );
        let value =
            extract_tail_json_bytes(doc.as_bytes()).expect("a ~60 KiB tail block must extract");
        assert_eq!(value["status"], json!("ok"));
    }

    #[test]
    fn large_tail_block_and_distant_stale_block_guard_coexist() {
        // (C1) The two halves of the envelope must hold at once: a large block
        // whose closer is at the tail extracts, while a complete block whose
        // *closer* sits beyond the window is still refused in the same buffer
        // shape. Restoring large-document support must not reopen the stale-block
        // resurrection that MAX_TAIL_FALLBACK_BYTES exists to prevent.
        let payload = "C".repeat(9000);
        let big = format!(r#"{{"payload":"{payload}","status":"fresh"}}"#);

        // Half 1: the oversized block sits at the tail -> extracted.
        let at_tail = format!("{}\n{big}\n", "noise ".repeat(100));
        assert_eq!(
            extract_tail_json(&at_tail).expect("tail block must extract")["status"],
            json!("fresh")
        );

        // Half 2: the same complete block is pushed so its closer is beyond the
        // window, with only a truncated block near the tail -> not resurrected.
        let mut stale = big.clone();
        stale.push_str(&"x".repeat(MAX_TAIL_FALLBACK_BYTES + 1024));
        stale.push_str("\n{\"new\": \"trunc");
        assert!(
            extract_tail_json(&stale).is_none(),
            "a block closing beyond the window must stay unreachable, however large"
        );
    }

    #[test]
    fn braces_inside_string_values_do_not_starve_the_scan() {
        // (budget defect 1) Guarantee: `{` characters inside a string VALUE are
        // phantom openers -- not structure -- and must not consume the scan's
        // allowance to the point that the real outermost opener is never
        // reached. A shallow, perfectly valid document used to extract as None
        // once its string held ~150 braces (a 175-byte input!), because each
        // phantom paid for its own forward walk: ~k^2 work against a ~64k
        // allowance. Sizes here bracket that old cliff by orders of magnitude.
        for k in [100usize, 150, 1_000, 20_000] {
            let braces = "{".repeat(k);
            let doc = format!(r#"{{"note":"{braces}","status":"ok"}}"#);
            assert!(
                serde_json::from_str::<Value>(&doc).is_ok(),
                "k={k} fixture must be valid JSON"
            );
            let value = extract_tail_json(&doc)
                .unwrap_or_else(|| panic!("k={k}: valid document must extract"));
            assert_eq!(value["status"], json!("ok"), "k={k}");
            assert_eq!(value["note"], json!(braces), "k={k}");
        }
    }

    #[test]
    fn deeply_nested_block_returns_the_outermost_value_not_an_inner_one() {
        // (budget defect 2 -- the silent-wrong-answer one) Guarantee: when a
        // valid nested document is extracted, the value returned is the COMPLETE
        // outermost block, never an inner fragment. A shared work budget used to
        // be consumed by the inner blocks (met first by the backward scan), so
        // the outermost opener was never examined and a 65-level document
        // extracted as its 64-level inner value. The assertion then ran against
        // the wrong root and passed or failed for the wrong reason -- strictly
        // worse than the slow-but-correct behavior the budget replaced.
        let depth = 65usize;
        let payload = "y".repeat(60 * 1024);
        let doc = format!("{}\"{payload}\"{}", "[".repeat(depth), "]".repeat(depth));
        assert!(
            serde_json::from_str::<Value>(&doc).is_ok(),
            "fixture must be valid JSON"
        );
        let value = extract_tail_json(&doc).expect("nested document must extract");

        // Walk down and confirm we got all `depth` levels, not depth - 1.
        let mut levels = 0;
        let mut cursor = &value;
        while let Value::Array(items) = cursor {
            levels += 1;
            assert_eq!(items.len(), 1, "each level wraps exactly one value");
            cursor = &items[0];
        }
        assert_eq!(
            levels, depth,
            "must return the outermost block, not an inner one"
        );
        assert_eq!(cursor, &json!(payload));
    }

    #[test]
    fn over_deep_block_is_not_answered_with_an_inner_fragment() {
        // (R4) Guarantee: when the tail block nests deeper than the parser will
        // accept, extraction reports nothing. It must NOT return the deepest
        // inner fragment that happens to parse — an assertion written against
        // that fragment would pass while the real root was never evaluated.
        //
        // `[`*d around an object: serde_json rejects the whole thing for depth,
        // while inner slices of it parse fine, so the scan is directly tempted
        // to answer with one.
        for d in [127usize, 128, 130, 150, 200] {
            let doc = format!("{}{}{}", "[".repeat(d), r#"{"ok":true}"#, "]".repeat(d));
            assert!(
                serde_json::from_str::<Value>(&doc).is_err(),
                "d={d} fixture must be too deep for the parser"
            );
            assert!(
                extract_tail_json(&doc).is_none(),
                "d={d}: an over-deep block must not resolve to an inner fragment"
            );
        }
    }

    #[test]
    fn blocks_the_parser_accepts_still_extract_in_full() {
        // (R4) The other side of the boundary: tightening the depth handling
        // must not start refusing documents that are perfectly parseable. Every
        // depth the parser accepts must still yield the COMPLETE outermost root,
        // right up to the limit.
        // The deepest `[`*d around an object that the parser still accepts. Found
        // rather than hardcoded: the innermost object costs a nesting level, so
        // this is one below the deepest same-kind run (a round-4 report claimed
        // 127 here, which was off by one for exactly that reason).
        let deepest = (1..200usize)
            .take_while(|d| {
                let doc = format!("{}{}{}", "[".repeat(*d), r#"{"ok":true}"#, "]".repeat(*d));
                serde_json::from_str::<Value>(&doc).is_ok()
            })
            .last()
            .expect("some depth must parse");
        assert_eq!(deepest, 126, "fixture-shape boundary moved");

        for d in [1usize, 100, 124, 125, deepest] {
            let doc = format!("{}{}{}", "[".repeat(d), r#"{"ok":true}"#, "]".repeat(d));
            assert!(
                serde_json::from_str::<Value>(&doc).is_ok(),
                "d={d} fixture must be parseable"
            );
            let value = extract_tail_json(&doc).unwrap_or_else(|| panic!("d={d} must extract"));

            // Peel exactly `d` array levels; the object must sit at the bottom.
            let mut cursor = &value;
            for level in 0..d {
                let Value::Array(items) = cursor else {
                    panic!("d={d}: level {level} is not an array — got an inner fragment");
                };
                assert_eq!(items.len(), 1, "d={d} level {level}");
                cursor = &items[0];
            }
            assert_eq!(cursor, &json!({"ok": true}), "d={d}");
        }
    }

    #[test]
    fn depth_limit_is_not_below_what_serde_json_accepts() {
        // (R4) Pins the relationship MAX_CANDIDATE_DEPTH depends on: the walk's
        // limit must not sit below the deepest run the parser will accept, or
        // valid documents would be abandoned. It must not sit above it either,
        // or a depth rejection reaches us as an anonymous parse failure and the
        // scan falls back to an inner block. If a serde_json upgrade moves its
        // recursion limit, this fails and points at the constant to retune.
        let deepest_parseable = (1..300usize)
            .take_while(|d| {
                let doc = format!("{}1{}", "[".repeat(*d), "]".repeat(*d));
                serde_json::from_str::<Value>(&doc).is_ok()
            })
            .last()
            .expect("some depth must parse");
        assert_eq!(
            MAX_CANDIDATE_DEPTH as usize, deepest_parseable,
            "walk depth limit and serde_json's recursion limit have drifted apart"
        );
    }

    #[test]
    fn truncated_tail_still_falls_back_when_the_scan_did_not_give_up() {
        // (R4 / C1) The distinction the give-up handling turns on: a tail block
        // the scan *disproved* (it runs off the end unbalanced) is an ordinary
        // negative result, and the documented fallback to a complete earlier
        // block still applies. Only an *abandoned* candidate suppresses that.
        // Without this, the fix for over-deep blocks could easily have been
        // written so that all fallback stopped working.
        let text = "{\"phase\": 1, \"final\": true}\nlog\n{\"phase\": 2, \"trunc";
        let value = extract_tail_json(text).expect("must still fall back");
        assert_eq!(value, json!({"phase": 1, "final": true}));
    }

    #[test]
    fn abandoned_sibling_before_a_good_block_does_not_suppress_it() {
        // (R5 case A) Guarantee: giving up on one region must not discard an
        // unrelated valid block elsewhere. Deep noise that the scan abandons,
        // followed by a perfectly good report at the tail, must still extract —
        // the noise is a SIBLING preceding the block, not an enclosure of it.
        //
        // The previous design compared a single "smallest abandoned offset"
        // against the candidate's opener, which cannot tell those apart: the
        // good block starts after the noise, so it looked nested and was
        // wrongly suppressed.
        let doc = format!(
            "{}{}\n{}",
            "[".repeat(128),
            "]".repeat(128),
            r#"{"ok":true}"#
        );
        let value = extract_tail_json(&doc)
            .expect("a valid tail block must survive unrelated deep noise above it");
        assert_eq!(value, json!({"ok": true}));
    }

    #[test]
    fn abandoned_tail_does_not_fall_back_to_an_earlier_block() {
        // (R5 case B) Guarantee: when the scan gives up on the region nearest
        // the tail, it must NOT resurrect a complete earlier block in its place.
        // The abandoned region may BE the report the author meant, so answering
        // with the older block asserts against the wrong root — the exact
        // failure the give-up handling exists to prevent.
        //
        // The previous design missed this because the stale block sits at a
        // SMALLER offset than the abandoned run, so an offset comparison let it
        // through. Containment has to consider that the abandoned region reaches
        // nearer the tail than the candidate does.
        let doc = format!("{}\n{}", r#"{"stale":true}"#, "[".repeat(128));
        assert!(
            extract_tail_json(&doc).is_none(),
            "an abandoned tail region must not fall back to an earlier block"
        );
    }

    #[test]
    fn two_separate_abandoned_regions_do_not_combine_into_a_false_overlap() {
        // (R6) Guarantee: abandoned regions are judged individually. Here there
        // are two, each harmless on its own by the containment rules:
        //   - deep noise wholly BEFORE the report (a sibling, ignored);
        //   - a phantom walk wholly INSIDE the report's string value (allowed,
        //     because the report itself balanced and parsed).
        // Summarising them as one `[min_start, max_end)` span invents coverage
        // of the gap between them, which straddles the report and refuses it.
        // Only ranges that genuinely overlap or abut may be merged.
        let braces = "{".repeat(128);
        let noise = format!("{}{}", "[".repeat(128), "]".repeat(128));
        let block = format!(r#"{{"note":"{braces}","ok":true}}"#);

        // Each half must extract on its own, or the fixture proves nothing.
        assert!(
            extract_tail_json(&block).is_some(),
            "fixture invalid: the report must extract without the noise"
        );

        let doc = format!("{noise}\n{block}");
        let value = extract_tail_json(&doc)
            .expect("two individually harmless abandoned regions must not combine");
        assert_eq!(value["ok"], json!(true));
        assert_eq!(value["note"], json!(braces));
    }

    #[test]
    fn abandoned_regions_merge_only_when_they_touch() {
        // (R6) Pins the aggregation rule directly, so a future "optimisation"
        // that fuses unconditionally fails here rather than in a subtle
        // extraction bug. Ranges that overlap or abut describe one contiguous
        // unevaluated area and may fuse; ranges with a gap between them describe
        // two, and the gap is buffer the scan *did* evaluate.
        let mut regions = Vec::new();
        note_abandoned(&mut regions, 0..10);
        note_abandoned(&mut regions, 20..30);
        assert_eq!(regions.len(), 2, "disjoint regions must stay separate");

        note_abandoned(&mut regions, 10..20);
        assert_eq!(
            regions,
            vec![0..30],
            "a range bridging the gap must fuse all three into one"
        );

        let mut regions = Vec::new();
        note_abandoned(&mut regions, 0..100);
        note_abandoned(&mut regions, 40..50);
        assert_eq!(
            regions,
            vec![0..100],
            "a contained range must not shrink the span"
        );
    }

    #[test]
    fn json_inside_quoted_log_noise_is_still_extracted() {
        // (v1 divergence, deliberate — and now NARROWLY scoped) A JSON-looking
        // run inside quoted log prose is extracted only when nothing outside
        // quoted prose parses. pitty 1.2.2 found nothing here.
        //
        // 1.2.2 decided "is this `{` inside a string?" by counting double quotes
        // from the start of the whole buffer, calling odd parity "inside" —
        // measured against the 1.2.2 binary at 0/1/2/3 preceding quotes, and
        // identical whether the stray quote was on the same line or three lines
        // earlier. That one rule also produces the #29 bugs: on 1.2.2, `warn "
        // oops` before a report hides it entirely, and a stray quote between two
        // reports returns the OLDER one. The brace sits at odd parity in all of
        // them, so no parity-based rule can reject this input while accepting
        // those. Restoring 1.2.2 here would restore a stale-block bug.
        //
        // What IS recovered (see the sibling test below): whenever a genuine
        // block exists outside quoted prose, it wins, so this divergence is
        // confined to output containing no real JSON at all.
        let text = "log: \"{\"status\":\"ok\"}\"\n";
        assert_eq!(
            extract_tail_json(text),
            Some(json!({"status": "ok"})),
            "documented v1 divergence: the inner block is extracted"
        );
    }

    #[test]
    fn a_real_block_outranks_a_json_run_inside_quoted_log_prose() {
        // (R16) Guarantee: a JSON-looking run inside quoted log text never
        // displaces a genuine report, wherever it sits relative to it. The
        // tail-most-closing preference alone would hand the verdict to the fake
        // one, flipping a passing v1 scenario to failing.
        //
        // The rule that prevents it is LINE-scoped quote parity, used only to
        // rank candidates, never to discard them. It does not reintroduce what
        // #29 removed: that bug came from parity inherited across arbitrary
        // distance — a stray `"` anywhere earlier, or a tail window cutting
        // through a string literal, inverted the state for every byte after it.
        // A newline ends that inheritance, and terminal output is line-oriented,
        // so the signal is re-derived per line rather than assumed. Verified
        // against the 1.2.2 binary: every shape in this test now agrees with it.
        let real = json!({"status": "real"});

        // Fake after the report, on its own line, on the same line, further away,
        // and with escaped quotes — the tail-most in every case.
        for text in [
            "{\"status\":\"real\"}\nlog: \"{\"status\":\"fake\"}\"\n",
            "{\"status\":\"real\"} log: \"{\"status\":\"fake\"}\"\n",
            "{\"status\":\"real\"}\na\nb\nlog: \"{\"status\":\"fake\"}\"\n",
            "{\"status\":\"real\"}\nlog: \\\"{\"status\":\"fake\"}\\\"\n",
            "log: \"{\"s\":1}\"\n{\"status\":\"real\"}\nlog: \"{\"s\":2}\"\n",
        ] {
            assert_eq!(
                extract_tail_json(text),
                Some(real.clone()),
                "quoted log prose must not outrank the real report in {text:?}"
            );
        }

        // An UNQUOTED later block still wins, exactly as in 1.2.2: the ranking
        // only demotes candidates inside quoted prose, it does not abandon the
        // tail-most preference.
        assert_eq!(
            extract_tail_json("{\"status\":\"real\"}\nlog: {\"status\":\"fake\"}\n"),
            Some(json!({"status": "fake"})),
            "an unquoted later block must still win on tail position"
        );

        // A report immediately following a quoted word is NOT inside quoted
        // prose (the quotes balance before it), so it must still be found.
        assert_eq!(
            extract_tail_json("tool said \"done\"{\"status\":\"ok\"}\n"),
            Some(json!({"status": "ok"})),
            "balanced quotes earlier on the line must not demote the report"
        );
    }

    #[test]
    fn an_escaped_quote_in_log_prose_does_not_demote_the_real_report() {
        // (R18) Guarantee: the quoted-prose ranking honours backslash escapes,
        // so an ordinary escaped quote inside a logged message cannot flip the
        // classification of the rest of that line and hand the verdict to a
        // JSON-looking run sitting inside the message.
        //
        // The rule was measured against the 1.2.2 binary rather than assumed. A
        // backslash-run ladder (N backslashes then a quote, inside an opened
        // string) alternates its verdict with the parity of N — odd N leaves the
        // string open, even N closes it — which is standard JSON escaping and
        // not a raw count of quote bytes. Counting every `"` byte instead judged
        // `real` to be the quoted one here and returned `fake`.
        let real = json!({"status": "real"});

        for text in [
            // The reported shape: one escaped quote inside the logged message.
            "{\"status\":\"real\"}\nlog: \"prefix \\\" {\"status\":\"fake\"}\"\n",
            // Unterminated message, escaped quote before the run.
            "{\"status\":\"real\"}\nlog: \"a \\\" {\"status\":\"fake\"}\n",
            // An escaped backslash then an escaped quote: still an odd run
            // before the quote, so the message stays open.
            "{\"status\":\"real\"}\nlog: \"x\\\\\\\" {\"status\":\"fake\"}\n",
        ] {
            assert_eq!(
                extract_tail_json(text),
                Some(real.clone()),
                "an escaped quote must not demote the real report in {text:?}"
            );
        }

        // Deliberately NOT extended across the newline. A message left open at
        // end of line — with or without a trailing backslash — leaves 1.2.2
        // masking everything after it, so 1.2.2 returns `real` here while this
        // build returns the later block. That divergence is the documented #29
        // trade: recovering it would mean inheriting quote parity across lines,
        // which is exactly the whole-buffer assumption that lost real reports.
        // It is escape-independent (the same shape with no backslash at all
        // behaves identically), so it is not something the escape rule can fix.
        assert_eq!(
            extract_tail_json("{\"status\":\"real\"}\nlog: \"open \n{\"status\":\"fake\"}\n"),
            Some(json!({"status": "fake"})),
            "an unterminated message must not leak its quoted state across a newline"
        );

        // The other half of the escape rule: an EVEN backslash run leaves the
        // quote real, so the message genuinely closes and the later block is
        // ordinary unquoted output that still wins on tail position — as 1.2.2
        // does. This is what stops the fix from over-correcting into "any line
        // with a backslash is quoted".
        let fake = json!({"status": "fake"});
        for text in [
            "{\"status\":\"real\"}\nlog: \"x\\\\\" {\"status\":\"fake\"}\n",
            "{\"status\":\"real\"}\nlog: \"a \\\" b\" {\"status\":\"fake\"}\n",
        ] {
            assert_eq!(
                extract_tail_json(text),
                Some(fake.clone()),
                "a closed message must leave the later block unquoted in {text:?}"
            );
        }

        // A backslash OUTSIDE any quoted run escapes nothing: 1.2.2's ladder
        // does not alternate when the backslashes sit in unquoted prose, so a
        // bare `\"` still opens a message and demotes the run inside it.
        assert_eq!(
            extract_tail_json("{\"status\":\"real\"}\nlog: \\\" {\"status\":\"fake\"}\n"),
            Some(real),
            "a backslash in unquoted prose must not suppress the quote it precedes"
        );
    }

    #[test]
    fn phantom_openers_in_string_values_scale_linearly() {
        // (perf) Guarantee: a VALID document whose string values are full of
        // `{` — phantom openers, not structure — costs work linear in its size.
        // Each phantom is still attempted as a candidate (the scan cannot know
        // it is phantom without walking), so the protection is that
        // MAX_CANDIDATE_DEPTH caps each attempt at a constant. Without that cap
        // this shape is quadratic: measured steps-per-byte doubled at every size
        // (245 -> 495 -> 995 -> 1995), where with it the figure plateaus flat.
        //
        // The assertion is on the SHAPE of the growth, not on wall-clock time,
        // so it does not flake on a loaded machine: doubling the input must not
        // more than triple the time. Quadratic growth would quadruple it.
        fn adversarial(k: usize) -> String {
            let inner = format!("{}{}", "{".repeat(k), "}".repeat(k));
            format!(r#"{{"note":"{inner}","status":"ok"}}"#)
        }

        // Warm up so the first measurement does not absorb one-time costs.
        let _ = extract_tail_json(&adversarial(500));

        let time_for = |k: usize| {
            let doc = adversarial(k);
            assert!(
                serde_json::from_str::<Value>(&doc).is_ok(),
                "k={k} fixture must be valid JSON"
            );
            let start = std::time::Instant::now();
            let value = extract_tail_json(&doc);
            let elapsed = start.elapsed();
            assert!(value.is_some(), "k={k}: a valid document must extract");
            elapsed
        };

        let small = time_for(2_000);
        let large = time_for(4_000);
        assert!(
            large < small * 3 + std::time::Duration::from_millis(5),
            "doubling the input more than tripled the work \
             ({small:?} -> {large:?}): the scan has gone quadratic"
        );
    }

    #[test]
    fn pathological_brace_runs_stay_within_the_work_budget() {
        // (perf) Guarantee: the opener scan is deliberately unbounded in position,
        // so the shared work budget is what keeps it from going quadratic. These
        // are the two quadratic shapes -- openers that never balance, and a
        // maximally nested run where every opener balances over its own span --
        // at the runner's window size. Both must return promptly without panic.
        let n = 64 * 1024;
        for input in [
            "{".repeat(n),
            format!("{}{}", "{".repeat(n / 2), "}".repeat(n / 2)),
            "[".repeat(n),
        ] {
            let start = std::time::Instant::now();
            let _ = extract_tail_json(&input);
            // Generous enough for a debug build on a loaded machine, yet far
            // below the unbudgeted cost: removing the budget takes >5s on these
            // inputs even in release, so a regression fails this decisively.
            let elapsed = start.elapsed();
            assert!(
                elapsed < std::time::Duration::from_secs(2),
                "pathological input must stay within the work budget, took {elapsed:?}"
            );
        }
    }
}
