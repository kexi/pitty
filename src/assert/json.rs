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

/// Parse the JSON block at the tail of `bytes` (noisy terminal output).
///
/// Scans backward from the end of `bytes` to find the last `}` or `]`, then
/// walks backward from each candidate closer to the matching opener to find a
/// balanced block that `serde_json` accepts. The **last block that parses**
/// within [`MAX_TAIL_FALLBACK_BYTES`] of the tail is returned.
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
    // Find the last structural closing byte (`}` or `]`) that is NOT inside a
    // string literal. We compute, in one forward pass, the set of byte offsets
    // that are "in string" so the backward search can skip closers that are
    // mere data. One pass is enough because string state is left-to-right.
    let in_string = string_mask(bytes);

    // The earliest closer offset we will still try. Saturating so a buffer
    // shorter than the window scans from 0 (its whole length).
    let fallback_floor = bytes.len().saturating_sub(MAX_TAIL_FALLBACK_BYTES);

    let mut scan = bytes.len();
    while scan > fallback_floor {
        let i = scan - 1;
        let is_closer = (bytes[i] == b'}' || bytes[i] == b']') && !in_string[i];
        if !is_closer {
            scan -= 1;
            continue;
        }
        // Walk backward from this closer to the matching opener by depth,
        // skipping bytes that are inside string literals.
        if let Some(open) = matching_open(bytes, i, &in_string) {
            if let Ok(candidate) = std::str::from_utf8(&bytes[open..=i]) {
                if let Ok(value) = serde_json::from_str::<Value>(candidate) {
                    return Some(value);
                }
            }
        }
        // This closer did not yield a parseable block; keep searching earlier
        // (still within the tail fallback window).
        scan -= 1;
    }
    None
}

/// Find the opener that matches the closer at `close_idx`, by depth, ignoring
/// braces inside string literals (per `in_string`).
fn matching_open(bytes: &[u8], close_idx: usize, in_string: &[bool]) -> Option<usize> {
    let (open_byte, close_byte) = match bytes[close_idx] {
        b'}' => (b'{', b'}'),
        b']' => (b'[', b']'),
        _ => return None,
    };
    // Start at depth 1 for the closer at `close_idx` itself and scan the bytes
    // strictly before it; the matching opener is the byte that brings depth back
    // to 0. Why depth=1 + skip rather than seeding depth=0 and counting the
    // closer: it keeps the loop body a plain "opener decrements, closer
    // increments" without a special first-iteration case.
    if close_idx == 0 {
        return None;
    }
    let mut depth: i32 = 1;
    let mut i = close_idx - 1;
    loop {
        if !in_string[i] {
            if bytes[i] == close_byte {
                depth += 1;
            } else if bytes[i] == open_byte {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
        }
        if i == 0 {
            return None;
        }
        i -= 1;
    }
}

/// Compute, for each byte, whether it lies inside a JSON string literal.
///
/// A double quote toggles string state unless it is escaped by an odd run of
/// backslashes. The opening and closing quotes themselves are marked as
/// in-string so a `"` is never treated as a structural boundary. This single
/// left-to-right pass is what lets the backward closer search skip braces that
/// are string data rather than structure.
fn string_mask(bytes: &[u8]) -> Vec<bool> {
    let mut mask = vec![false; bytes.len()];
    let mut in_string = false;
    let mut escaped = false;
    for (i, &b) in bytes.iter().enumerate() {
        if in_string {
            mask[i] = true;
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
            mask[i] = true;
        }
    }
    mask
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
        // malformed bracket -> None (ptytest has no Python-style negative
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
            // The tail extractor must not panic on the deep input. (It may still
            // return an inner sub-block that is within the limit — see the C1
            // fallback — but it must never abort.)
            let _ = extract_tail_json(&text);
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
}
