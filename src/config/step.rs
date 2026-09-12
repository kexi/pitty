//! Scenario step definitions and key-name parsing.
//!
//! Steps are modeled as a one-key map (`serde` internally tagged by the single
//! present key) rather than an untagged union. We avoid untagged on purpose:
//! untagged enums try each variant in turn and report only a generic
//! "did not match any variant" on failure, which produces poor error messages
//! and risks a malformed `expect` silently matching some other variant. The
//! one-key form lets us key off the step name directly and surface precise
//! errors. The sole exception is `spawn`, which accepts either a bare string
//! or a struct via a small untagged helper, since both shapes are unambiguous.

use serde::de::{self, MapAccess, Visitor};
use serde::{Deserialize, Deserializer};

use super::duration::DurationStr;
use crate::assert::json::JsonCheck;
use crate::pty::SplitMode;

/// Every step key the deserializer's dispatch accepts, in declaration order.
///
/// This is the machine-readable mirror of the `match key.as_str()` arms in
/// [`Step`]'s `Deserialize` impl. It exists so a test can compare the accepted
/// key set against the published JSON schema's `step.properties` keys (the G-8
/// gate) and against the dispatch itself, catching a key added on one side but
/// not the other. The `unknown_step_dispatch_matches_step_keys` test pins this
/// constant to the dispatch so the two cannot silently diverge.
pub const STEP_KEYS: &[&str] = &[
    "spawn",
    "send",
    "send_raw",
    "key",
    "wait",
    "expect",
    "expect_regex",
    "expect_not",
    "expect_file_exists",
    "expect_file_contains",
    "expect_file_not_contains",
    "expect_file_changed",
    "expect_exit",
    "expect_running",
    "expect_json",
    "expect_snapshot",
    "expect_semantic",
];

/// A single executable step in a scenario.
///
/// Each list entry is a one-key map whose key names the step kind. We hand-roll
/// `Deserialize` (below) rather than relying on serde's enum support: in
/// serde_norway, a derived enum deserializes from a YAML `!Variant` tag, not a
/// one-key map, which is not the ergonomic YAML we want. A custom impl also
/// lets us reject entries with zero or multiple keys and emit a precise
/// "unknown step" error keyed on the actual step name.
#[derive(Debug, Clone)]
pub enum Step {
    /// Spawn a child process inside the PTY. Either `spawn: bash` or
    /// `spawn: {command, cwd, env}`.
    Spawn(SpawnSpec),

    /// Write a line to the child's stdin. A trailing `\r` (CR, the line
    /// terminator a PTY expects from Enter) is appended automatically and
    /// `${var}` placeholders are expanded.
    Send(String),

    /// Write the payload to stdin without appending a line terminator. Unlike
    /// [`Step::Send`] it adds no trailing `\r`, but `${var}` placeholders are
    /// still expanded (use `$$` for a literal `$`) — the difference is only the
    /// missing terminator, not the absence of substitution.
    SendRaw(String),

    /// Send a named key (e.g. `enter`, `ctrl+c`) as its control byte sequence.
    Key(Key),

    /// Sleep for a fixed duration before continuing.
    Wait(DurationStr),

    /// Wait until output contains a substring or matches, up to `timeout`.
    Expect(MatchSpec),

    /// Wait until output matches a regular expression, up to `timeout`.
    ExpectRegex(RegexSpec),

    /// Assert immediately that the unconsumed output does NOT contain a match.
    ExpectNot(ExpectNotSpec),

    /// Assert a file exists.
    ExpectFileExists(FilePathSpec),

    /// Assert a file's contents contain a substring.
    ExpectFileContains(FileContainsSpec),

    /// Assert a file's contents do NOT contain a substring.
    ExpectFileNotContains(FileContainsSpec),

    /// Assert a file's contents changed between spawn time and now.
    ExpectFileChanged(FilePathSpec),

    /// Assert the child has exited with a specific code, optionally polling
    /// until it exits (or a deadline) before judging.
    ExpectExit(ExpectExitSpec),

    /// Assert whether the child is still running.
    ExpectRunning(bool),

    /// Assert on a JSON value extracted from output (tail block) or a file.
    ExpectJson(ExpectJsonSpec),

    /// Assert current output matches a recorded snapshot file.
    ExpectSnapshot(ExpectSnapshotSpec),

    /// Assert output is semantically close to an expected text.
    ExpectSemantic(ExpectSemanticSpec),
}

impl<'de> Deserialize<'de> for Step {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct StepVisitor;

        impl<'de> Visitor<'de> for StepVisitor {
            type Value = Step;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a step map with exactly one key, e.g. `expect: {...}`")
            }

            fn visit_map<A>(self, mut map: A) -> std::result::Result<Step, A::Error>
            where
                A: MapAccess<'de>,
            {
                // Read the single key naming the step kind. An empty map (no
                // key) is an error: a step must declare what it does.
                let key: String = map
                    .next_key()?
                    .ok_or_else(|| de::Error::custom("empty step (expected one key)"))?;

                // Dispatch on the key, deserializing the value into the kind's
                // payload type. The payload types are independently
                // Deserialize, so each gets precise field-level errors.
                let step = match key.as_str() {
                    "spawn" => Step::Spawn(map.next_value()?),
                    "send" => Step::Send(map.next_value()?),
                    "send_raw" => Step::SendRaw(map.next_value()?),
                    "key" => Step::Key(map.next_value()?),
                    "wait" => Step::Wait(map.next_value()?),
                    "expect" => Step::Expect(map.next_value()?),
                    "expect_regex" => Step::ExpectRegex(map.next_value()?),
                    "expect_not" => Step::ExpectNot(map.next_value()?),
                    "expect_file_exists" => Step::ExpectFileExists(map.next_value()?),
                    "expect_file_contains" => Step::ExpectFileContains(map.next_value()?),
                    "expect_file_not_contains" => Step::ExpectFileNotContains(map.next_value()?),
                    "expect_file_changed" => Step::ExpectFileChanged(map.next_value()?),
                    "expect_exit" => Step::ExpectExit(map.next_value()?),
                    "expect_running" => Step::ExpectRunning(map.next_value()?),
                    "expect_json" => Step::ExpectJson(map.next_value()?),
                    "expect_snapshot" => Step::ExpectSnapshot(map.next_value()?),
                    "expect_semantic" => Step::ExpectSemantic(map.next_value()?),
                    other => return Err(de::Error::custom(format!("unknown step '{other}'"))),
                };

                // Reject a second key: a one-key map keeps step semantics
                // unambiguous, so `{expect: ..., send: ...}` is a mistake.
                if let Some(extra) = map.next_key::<String>()? {
                    return Err(de::Error::custom(format!(
                        "step '{key}' has unexpected extra key '{extra}'"
                    )));
                }
                Ok(step)
            }
        }

        deserializer.deserialize_map(StepVisitor)
    }
}

impl Step {
    /// A short human-readable label for this step, used in report rows and
    /// log lines.
    pub fn label(&self) -> String {
        match self {
            Step::Spawn(s) => format!("spawn: {}", s.command),
            Step::Send(s) => format!("send: {s}"),
            Step::SendRaw(s) => format!("send_raw: {s}"),
            Step::Key(k) => format!("key: {}", k.name),
            Step::Wait(d) => format!("wait: {:?}", d.as_duration()),
            Step::Expect(m) => format!("expect: {}", m.describe()),
            Step::ExpectRegex(r) => format!("expect_regex: {}", r.pattern),
            Step::ExpectNot(m) => format!("expect_not: {}", m.describe()),
            Step::ExpectFileExists(f) => format!("expect_file_exists: {}", f.path),
            Step::ExpectFileContains(f) => {
                format!("expect_file_contains: {} ~ {}", f.path, f.contains)
            }
            Step::ExpectFileNotContains(f) => {
                format!("expect_file_not_contains: {} ~ {}", f.path, f.contains)
            }
            Step::ExpectFileChanged(f) => format!("expect_file_changed: {}", f.path),
            Step::ExpectExit(s) => format!("expect_exit: {}", s.code),
            Step::ExpectRunning(b) => format!("expect_running: {b}"),
            Step::ExpectJson(s) => format!("expect_json: {} {}", s.path, s.describe_check()),
            Step::ExpectSnapshot(s) => format!("expect_snapshot: {}", s.file),
            Step::ExpectSemantic(s) => {
                format!("expect_semantic: similarity>={}", s.similarity)
            }
        }
    }
}

/// Specification of a process to spawn.
///
/// Accepts either a bare command string (`spawn: bash`) or a struct
/// (`spawn: {command: ..., cwd: ..., env: {...}, split: ...}`).
#[derive(Debug, Clone, Deserialize)]
#[serde(from = "SpawnSpecRaw")]
pub struct SpawnSpec {
    /// The command line, tokenized per [`SpawnSpec::split`].
    pub command: String,
    /// Optional working directory override (relative to the workspace cwd).
    pub cwd: Option<String>,
    /// Optional extra environment variables for this spawn.
    pub env: std::collections::BTreeMap<String, String>,
    /// How `command` is split into `[program, args...]`.
    ///
    /// Defaults to [`SplitMode::Whitespace`], the rule pitty has always used,
    /// because `COMPATIBILITY.md` forbids changing the meaning of an existing
    /// field within `1.x`. `split: posix` opts a single `spawn` into POSIX
    /// shell word rules; it is only expressible in the struct form (the bare
    /// string form has nowhere to put a modifier that is not itself a change
    /// to how existing command strings are read).
    pub split: SplitMode,
}

/// Untagged wire form for [`SpawnSpec`]: a string or a struct.
///
/// We funnel through this helper (rather than putting `#[serde(untagged)]` on
/// the public type) so the public `SpawnSpec` keeps non-optional `command`
/// and a defaulted `env`, while still accepting both YAML shapes.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum SpawnSpecRaw {
    Command(String),
    Struct {
        command: String,
        #[serde(default)]
        cwd: Option<String>,
        #[serde(default)]
        env: std::collections::BTreeMap<String, String>,
        #[serde(default)]
        split: SplitMode,
    },
}

impl From<SpawnSpecRaw> for SpawnSpec {
    fn from(raw: SpawnSpecRaw) -> Self {
        match raw {
            // The bare string form cannot carry `split`, so it is always the
            // legacy default — which is exactly what a scenario written before
            // the field existed must keep getting.
            SpawnSpecRaw::Command(command) => SpawnSpec {
                command,
                cwd: None,
                env: Default::default(),
                split: SplitMode::default(),
            },
            SpawnSpecRaw::Struct {
                command,
                cwd,
                env,
                split,
            } => SpawnSpec {
                command,
                cwd,
                env,
                split,
            },
        }
    }
}

/// A contains-style match specification with an optional timeout.
#[derive(Debug, Clone, Deserialize)]
pub struct MatchSpec {
    /// The substring to search for in pending output.
    pub contains: String,
    /// Optional per-step timeout; falls back to the global default when absent.
    #[serde(default)]
    pub timeout: Option<DurationStr>,
}

impl MatchSpec {
    fn describe(&self) -> String {
        format!("contains {:?}", self.contains)
    }
}

/// An `expect_not` specification: a substring asserted to be *absent* right now.
///
/// Deliberately a distinct type from [`MatchSpec`] with no `timeout` field.
/// `expect_not` is an immediate, non-waiting check of the already-captured
/// output (see the runner's `contains_now`), so a timeout has no meaning here.
///
/// Why a separate struct instead of reusing `MatchSpec`: reusing it let a
/// scenario write `expect_not: {contains, timeout}` where the `timeout` was
/// silently accepted and dropped — a contract that read as "timeout supported"
/// but was not. Omitting the field makes it structurally impossible for the
/// runner to ever read a timeout for `expect_not`. Why not also
/// `deny_unknown_fields` to reject a stray `timeout` outright: the forward-
/// compatibility policy (see `Scenario`/`COMPATIBILITY.md`) keeps spec types
/// lenient so a scenario authored for a newer 1.x parses on an older pitty;
/// a stray `timeout` is therefore ignored here, and the JSON schema flags it for
/// authors via `additionalProperties: false`.
#[derive(Debug, Clone, Deserialize)]
pub struct ExpectNotSpec {
    /// The substring asserted to be absent from the pending output.
    pub contains: String,
}

impl ExpectNotSpec {
    fn describe(&self) -> String {
        format!("contains {:?}", self.contains)
    }
}

/// An exit-code assertion with an optional deadline to poll for exit.
///
/// Accepts either a bare integer (`expect_exit: 0`) or a struct
/// (`expect_exit: {code: 1, timeout: 5s}`). The bare form keeps the original
/// non-blocking poll-once semantics (timeout `None`); the struct form lets a
/// scenario wait up to `timeout` for the child to exit before judging, which
/// removes the dependence on a preceding fixed `wait` being long enough.
#[derive(Debug, Clone, Deserialize)]
#[serde(from = "ExpectExitRaw")]
pub struct ExpectExitSpec {
    /// The exit code the child must have exited with.
    pub code: i32,
    /// Optional deadline to poll for the child's exit before judging. When
    /// absent, the assertion polls exactly once (the original semantics): a
    /// child still running fails immediately.
    pub timeout: Option<DurationStr>,
}

/// Untagged wire form for [`ExpectExitSpec`]: a scalar code or a struct.
///
/// We funnel through this helper (rather than `#[serde(untagged)]` on the
/// public type) so the public `ExpectExitSpec` keeps a non-optional `code` and
/// a defaulted `timeout`, while still accepting both YAML shapes. Why preserve
/// the bare-scalar form: every existing scenario and the README write
/// `expect_exit: 0`, so the scalar must keep parsing unchanged.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum ExpectExitRaw {
    Code(i32),
    Struct {
        code: i32,
        #[serde(default)]
        timeout: Option<DurationStr>,
    },
}

impl From<ExpectExitRaw> for ExpectExitSpec {
    fn from(raw: ExpectExitRaw) -> Self {
        match raw {
            ExpectExitRaw::Code(code) => ExpectExitSpec {
                code,
                timeout: None,
            },
            ExpectExitRaw::Struct { code, timeout } => ExpectExitSpec { code, timeout },
        }
    }
}

/// A regex match specification with an optional timeout.
#[derive(Debug, Clone, Deserialize)]
pub struct RegexSpec {
    /// The regular expression pattern (matched against output bytes).
    pub pattern: String,
    /// Optional per-step timeout; falls back to the global default when absent.
    #[serde(default)]
    pub timeout: Option<DurationStr>,
}

/// A file assertion that only needs a path.
#[derive(Debug, Clone, Deserialize)]
pub struct FilePathSpec {
    /// Path relative to the workspace cwd.
    pub path: String,
}

/// A file assertion that needs a path plus a substring.
#[derive(Debug, Clone, Deserialize)]
pub struct FileContainsSpec {
    /// Path relative to the workspace cwd.
    pub path: String,
    /// The substring asserted to be present (or absent).
    pub contains: String,
}

/// Where an assertion sources the text/JSON it inspects.
///
/// `output` (the default) reads the live PTY buffer; `file` reads a path
/// resolved against the workspace. Shared by `expect_json` and `expect_semantic`
/// so both step kinds accept the same `source:` grammar.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(from = "SourceRaw")]
pub enum Source {
    /// Read from the live PTY output buffer.
    #[default]
    Output,
    /// Read from a workspace-relative file path.
    File(String),
    /// The scenario gave an unrecognized `source` keyword. Carried as an
    /// explicit variant (rather than silently degrading to `Output`) so the
    /// runner can surface a precise Scenario error; the string is the offending
    /// keyword. This mirrors how `expect_json` carries a one-of violation.
    Invalid(String),
}

/// Untagged wire form for [`Source`].
///
/// Accepts the bare string `output`, or a mapping `{file: <path>}`. We funnel
/// through this helper so the public `Source` is a clean two-variant enum while
/// the YAML stays ergonomic. Why not `#[serde(untagged)]` directly on `Source`:
/// the `output` literal and the `{file}` map are distinct shapes, and routing
/// through a Raw lets us reject an unknown string with a precise message.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum SourceRaw {
    /// A bare string; only `output` is valid.
    Keyword(String),
    /// A `{file: <path>}` mapping.
    File { file: String },
}

impl From<SourceRaw> for Source {
    fn from(raw: SourceRaw) -> Self {
        match raw {
            SourceRaw::File { file } => Source::File(file),
            // The only valid keyword is `output` (also the default). An
            // unrecognized keyword is a scenario authoring mistake (likely a
            // typo for `output` or a malformed `{file: ...}`), so we record it as
            // `Source::Invalid` and let the runner reject it with a precise
            // Scenario error (exit 2). Why not silently degrade to `Output` (the
            // prior behavior): a typo would then run against live output instead
            // of the intended file, passing or failing for the wrong reason with
            // no signal to the author. An empty string is treated as the default
            // `output` so an omitted-but-present value is lenient.
            SourceRaw::Keyword(keyword) => {
                let normalized = keyword.trim().to_ascii_lowercase();
                if normalized.is_empty() || normalized == "output" {
                    Source::Output
                } else {
                    Source::Invalid(keyword)
                }
            }
        }
    }
}

/// An `expect_json` specification: a path, a one-of check, a source, a timeout.
///
/// The check is the single [`crate::assert::json::JsonCheck`] used by the
/// evaluator — there is no parallel config-side enum. Why a separate
/// `invalid_reason` field rather than an `Invalid` variant on `JsonCheck`: the
/// evaluator's `JsonCheck` models only the three valid comparisons, so polluting
/// it with an authoring-error variant would force every match site (including
/// `assert::json::evaluate`) to handle an `Invalid` arm that can never reach it.
/// Carrying the one-of validation failure out-of-band keeps the shared enum
/// clean and still lets the runner surface a precise scenario error.
#[derive(Debug, Clone, Deserialize)]
#[serde(from = "ExpectJsonRaw")]
pub struct ExpectJsonSpec {
    /// Dotted path into the JSON value (e.g. `result.items.0.name`).
    pub path: String,
    /// The comparison to run at `path`. Meaningful only when `invalid_reason`
    /// is `None`; otherwise a placeholder the runner never evaluates.
    pub check: JsonCheck,
    /// `Some(reason)` when the scenario specified zero or multiple checks (a
    /// one-of violation). The runner turns this into a Scenario error.
    pub invalid_reason: Option<String>,
    /// Where to read the JSON from (output tail block by default).
    pub source: Source,
    /// For `source: output`, how long to wait for a tail JSON block to appear.
    pub timeout: Option<DurationStr>,
}

impl ExpectJsonSpec {
    /// A short description of the check (or the one-of violation) for labels.
    fn describe_check(&self) -> String {
        match &self.invalid_reason {
            Some(reason) => format!("<invalid: {reason}>"),
            None => match &self.check {
                JsonCheck::Equals(v) => format!("equals {v}"),
                JsonCheck::Contains(s) => format!("contains {s:?}"),
                JsonCheck::Exists => "exists".to_string(),
            },
        }
    }
}

/// Untagged wire form for [`ExpectJsonSpec`].
///
/// The three check fields are mutually exclusive; specifying more than one is a
/// scenario authoring error. We collect all three as options here and reject a
/// multi-check (or no-check) entry in `From`, following the project's
/// Raw + `#[serde(from)]` pattern so the public spec carries a single resolved
/// `JsonCheck` rather than three loose options.
#[derive(Debug, Clone, Deserialize)]
struct ExpectJsonRaw {
    path: String,
    // Double `Option` so an explicitly written `equals: null` is distinguishable
    // from an omitted `equals`: the outer layer records *presence*, the inner
    // holds the value (`Some(Value::Null)` for an explicit null). Why not a plain
    // `Option<Value>`: serde collapses a YAML `null`/`~`/empty scalar into the
    // outer `Option`, so `equals: null` deserialized to `None` — identical to
    // omission — which made null unassertable and let the one-of guard miscount a
    // written check as absent. Why not `#[serde(default)]` alone on the inner
    // type: `Value` has no "absent" state to default to, so presence has to live
    // in a wrapper the deserializer only fills when the key appears.
    #[serde(default, deserialize_with = "some_value")]
    equals: Option<Option<serde_json::Value>>,
    #[serde(default)]
    contains: Option<String>,
    #[serde(default)]
    exists: Option<bool>,
    #[serde(default)]
    source: Source,
    #[serde(default)]
    timeout: Option<DurationStr>,
}

/// Deserialize a present `equals` value into `Some(..)`, mapping an explicit
/// JSON/YAML null to `Some(Value::Null)` rather than to absence.
///
/// Paired with `#[serde(default)]`, which supplies `None` only when the key is
/// missing entirely: serde calls this function exactly when the key *is* present,
/// so wrapping unconditionally in `Some` is what separates "written" from
/// "omitted". `Value`'s own `Deserialize` already yields `Value::Null` for a null
/// scalar, so no null special-case is needed here.
fn some_value<'de, D>(deserializer: D) -> Result<Option<Option<serde_json::Value>>, D::Error>
where
    D: Deserializer<'de>,
{
    serde_json::Value::deserialize(deserializer).map(|value| Some(Some(value)))
}

impl From<ExpectJsonRaw> for ExpectJsonSpec {
    fn from(raw: ExpectJsonRaw) -> Self {
        // Enforce the one-of constraint by counting present check fields. A
        // valid spec carries exactly one. Why not fail in `From` (which is
        // infallible): the project's Raw + `#[serde(from)]` pattern keeps `From`
        // total and surfaces semantic errors at execution, so we record an
        // `invalid_reason` and let the runner emit a precise Scenario error.
        // `exists: false` counts as present (so it cannot pair with another
        // check) but does not select a check, hence resolves to invalid below.
        // An explicit `equals: null` is `Some(Some(Value::Null))`, so it counts as
        // present here exactly like any other written check — the same
        // "present but falsy" handling `exists: false` already gets.
        let present = [
            raw.equals.is_some(),
            raw.contains.is_some(),
            raw.exists.is_some(),
        ]
        .iter()
        .filter(|p| **p)
        .count();

        // COMPATIBILITY: `equals: null` paired with exactly one other check.
        //
        // 1.2.2 dropped `equals: null` while deserializing, so these documents
        // parsed as having a single check and *ran* — `equals: null` + `contains`
        // executed the `contains`, and `equals: null` + `exists` executed the
        // `exists`. Issue #30 made `equals: null` assertable, which correctly
        // turned it into a present check; but that also made these two shapes
        // trip the one-of guard, turning a scenario 1.2.2 executed (exit 0/1)
        // into a Scenario error (exit 2).
        //
        // COMPATIBILITY.md forbids tightening validation so a previously valid
        // scenario becomes an error, and lists no exception for "the old
        // behaviour verified nothing". Whether anyone *should* have depended on
        // it is not the contract's criterion — execution is, and a pipeline can
        // route on the exit code without depending on the assertion's meaning.
        //
        // So these two shapes keep 1.2.2's behaviour: the null `equals` is
        // dropped and the other check runs. Everything else is untouched —
        // `equals: null` alone still asserts null (#30's actual fix, a pure
        // loosening), and every combination 1.2.2 *rejected* is still rejected,
        // including `equals: null` with both `contains` and `exists`.
        let null_equals_with_one_other =
            matches!(raw.equals, Some(None) | Some(Some(serde_json::Value::Null)))
                && [raw.contains.is_some(), raw.exists.is_some()]
                    .iter()
                    .filter(|p| **p)
                    .count()
                    == 1;
        let (present, raw_equals) = if null_equals_with_one_other {
            (present - 1, None)
        } else {
            (present, raw.equals)
        };

        let (check, invalid_reason) = match (present, raw_equals, raw.contains, raw.exists) {
            (1, Some(v), _, _) => (
                JsonCheck::Equals(v.unwrap_or(serde_json::Value::Null)),
                None,
            ),
            (1, _, Some(s), _) => (JsonCheck::Contains(s), None),
            (1, _, _, Some(true)) => (JsonCheck::Exists, None),
            (1, _, _, Some(false)) => (
                JsonCheck::Exists,
                Some(
                    "exists: false is not a valid check (use equals/contains/exists: true)"
                        .to_string(),
                ),
            ),
            (0, ..) => (
                JsonCheck::Exists,
                Some("no check given; specify exactly one of equals/contains/exists".to_string()),
            ),
            _ => (
                JsonCheck::Exists,
                Some(
                    "multiple checks given; specify exactly one of equals/contains/exists"
                        .to_string(),
                ),
            ),
        };

        ExpectJsonSpec {
            path: raw.path,
            check,
            invalid_reason,
            source: raw.source,
            timeout: raw.timeout,
        }
    }
}

/// An `expect_snapshot` specification.
#[derive(Debug, Clone, Deserialize)]
pub struct ExpectSnapshotSpec {
    /// Snapshot file path, resolved against the workspace.
    pub file: String,
    /// When true, compare raw bytes; otherwise ANSI-strip before comparing.
    #[serde(default)]
    pub raw: bool,
}

/// An `expect_semantic` specification.
#[derive(Debug, Clone, Deserialize)]
pub struct ExpectSemanticSpec {
    /// The expected text to compare against.
    pub text: String,
    /// Minimum similarity (0.0..=1.0) required to pass.
    pub similarity: f64,
    /// Where to read the output text from (live output by default).
    #[serde(default)]
    pub source: Source,
}

/// A named key, carrying both the original name (for diagnostics) and the
/// resolved control byte sequence to write to the PTY.
#[derive(Debug, Clone)]
pub struct Key {
    /// The original key name as written in the scenario, e.g. `"ctrl+c"`.
    pub name: String,
    /// The bytes to send for this key.
    pub bytes: Vec<u8>,
}

/// Every key name [`Key::resolve`] accepts, in canonical (trimmed, lowercased)
/// form. The machine-readable mirror of the `match` arms in `resolve`, used by
/// the G-8 key-set gate to check the schema's `key` pattern lists the same names
/// the implementation resolves. `esc` is included as the documented alias of
/// `escape`. Pinned to `resolve` by the `key_names_const_matches_resolve` test.
pub const KEY_NAMES: &[&str] = &[
    "enter",
    "tab",
    "escape",
    "esc",
    "backspace",
    "up",
    "down",
    "right",
    "left",
    "ctrl+c",
    "ctrl+d",
    "ctrl+z",
];

impl Key {
    /// Resolve a key name into its byte sequence.
    ///
    /// Control combinations map to their ASCII control codes; arrow keys map
    /// to their CSI escape sequences as a typical terminal would emit them.
    fn resolve(name: &str) -> Option<Vec<u8>> {
        // Match case-insensitively on a trimmed, lowercased copy so that
        // "Enter", "ENTER", "ctrl+C" all resolve identically.
        let key = name.trim().to_ascii_lowercase();
        let bytes: Vec<u8> = match key.as_str() {
            "enter" => vec![b'\r'],
            "tab" => vec![b'\t'],
            "escape" | "esc" => vec![0x1b],
            "backspace" => vec![0x7f],
            "up" => vec![0x1b, b'[', b'A'],
            "down" => vec![0x1b, b'[', b'B'],
            "right" => vec![0x1b, b'[', b'C'],
            "left" => vec![0x1b, b'[', b'D'],
            "ctrl+c" => vec![0x03],
            "ctrl+d" => vec![0x04],
            "ctrl+z" => vec![0x1a],
            _ => return None,
        };
        Some(bytes)
    }
}

impl<'de> Deserialize<'de> for Key {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let name = String::deserialize(deserializer)?;
        let bytes = Key::resolve(&name)
            .ok_or_else(|| serde::de::Error::custom(format!("unknown key name '{name}'")))?;
        Ok(Key { name, bytes })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deserialize a single step from a one-line YAML map.
    fn step(yaml: &str) -> Step {
        serde_norway::from_str(yaml).unwrap()
    }

    #[test]
    fn spawn_accepts_bare_string() {
        // `spawn: bash` must yield a SpawnSpec with command "bash" and no env.
        let s = step("spawn: bash");
        match s {
            Step::Spawn(spec) => {
                assert_eq!(spec.command, "bash");
                assert!(spec.cwd.is_none());
                assert!(spec.env.is_empty());
                // The bare form has nowhere to carry `split`, so it is always
                // the legacy whitespace rule the v1 contract pins as default.
                assert_eq!(spec.split, SplitMode::Whitespace);
            }
            other => panic!("expected Spawn, got {other:?}"),
        }
    }

    #[test]
    fn spawn_struct_without_split_defaults_to_the_legacy_whitespace_rule() {
        // A scenario written before `split` existed must keep the tokenization
        // it was authored against; the field defaults rather than flipping it.
        let s = step("spawn:\n  command: echo 'hello world'");
        match s {
            Step::Spawn(spec) => assert_eq!(spec.split, SplitMode::Whitespace),
            other => panic!("expected Spawn, got {other:?}"),
        }
    }

    #[test]
    fn spawn_struct_opts_in_to_posix_tokenization() {
        // `split: posix` is the whole opt-in surface, normalized like `key`.
        for yaml in [
            "spawn:\n  command: echo hi\n  split: posix",
            "spawn:\n  command: echo hi\n  split: \"  POSIX  \"",
        ] {
            match step(yaml) {
                Step::Spawn(spec) => assert_eq!(spec.split, SplitMode::Posix, "{yaml}"),
                other => panic!("expected Spawn, got {other:?}"),
            }
        }
    }

    #[test]
    fn spawn_accepts_an_unknown_split_keyword_the_way_older_pitty_does() {
        // The v1 contract, verified against the 1.2.2 binary: `split` postdates
        // 1.0, so `split: custom` parses and runs on every earlier 1.x (the
        // field is simply ignored — nested deny_unknown_fields is off). This
        // pitty may not be stricter, so the keyword is carried as `Unknown`,
        // tokenizes as the default, and warns rather than erroring.
        let s = step("spawn:\n  command: echo hi\n  split: custom");
        match s {
            Step::Spawn(spec) => {
                assert_eq!(spec.split, SplitMode::Unknown("\"custom\"".to_string()));
                assert!(spec.split.warning().is_some(), "a typo must stay visible");
            }
            other => panic!("expected Spawn, got {other:?}"),
        }
    }

    #[test]
    fn spawn_accepts_a_non_string_split_value_the_way_older_pitty_does() {
        // The same clause one type away, and the case that matters most for the
        // untagged `SpawnSpecRaw`: a type mismatch at `split` fails the whole
        // variant match, so requiring a string would reject the entire `spawn`
        // map — with a message that never mentions `split`. 1.2.2 ignores the
        // key whatever its type (verified against the binary), so this build
        // must accept it and fall back to the default.
        for yaml in [
            "spawn:\n  command: echo hi\n  split: 42",
            "spawn:\n  command: echo hi\n  split: true",
            "spawn:\n  command: echo hi\n  split: null",
            "spawn:\n  command: echo hi\n  split: [a, b]",
            "spawn:\n  command: echo hi\n  split: {mode: posix}",
        ] {
            match step(yaml) {
                Step::Spawn(spec) => {
                    assert!(
                        matches!(spec.split, SplitMode::Unknown(_)),
                        "{yaml} must parse to an unknown split mode, got {:?}",
                        spec.split
                    );
                    assert_eq!(spec.command, "echo hi", "the rest of the map must survive");
                    assert!(spec.split.warning().is_some(), "{yaml} must warn");
                }
                other => panic!("expected Spawn for {yaml}, got {other:?}"),
            }
        }
    }

    #[test]
    fn spawn_accepts_struct() {
        // The struct form must populate command, cwd, and env.
        let s = step("spawn:\n  command: node app.js\n  cwd: sub\n  env:\n    A: \"1\"");
        match s {
            Step::Spawn(spec) => {
                assert_eq!(spec.command, "node app.js");
                assert_eq!(spec.cwd.as_deref(), Some("sub"));
                assert_eq!(spec.env.get("A").map(String::as_str), Some("1"));
            }
            other => panic!("expected Spawn, got {other:?}"),
        }
    }

    #[test]
    fn key_ctrl_c_maps_to_etx() {
        // `key: ctrl+c` must resolve to the single ETX byte 0x03.
        let s = step("key: ctrl+c");
        match s {
            Step::Key(k) => {
                assert_eq!(k.name, "ctrl+c");
                assert_eq!(k.bytes, vec![0x03]);
            }
            other => panic!("expected Key, got {other:?}"),
        }
    }

    #[test]
    fn key_arrow_up_maps_to_csi() {
        // `key: up` must resolve to the CSI escape sequence ESC [ A.
        let s = step("key: up");
        match s {
            Step::Key(k) => assert_eq!(k.bytes, vec![0x1b, b'[', b'A']),
            other => panic!("expected Key, got {other:?}"),
        }
    }

    #[test]
    fn key_enter_tab_escape_backspace() {
        // The plain named keys must map to their canonical bytes.
        assert!(matches!(step("key: enter"), Step::Key(k) if k.bytes == vec![b'\r']));
        assert!(matches!(step("key: tab"), Step::Key(k) if k.bytes == vec![b'\t']));
        assert!(matches!(step("key: escape"), Step::Key(k) if k.bytes == vec![0x1b]));
        assert!(matches!(step("key: backspace"), Step::Key(k) if k.bytes == vec![0x7f]));
    }

    #[test]
    fn key_remaining_arrows_map_to_csi() {
        // down/left/right must each map to their CSI escape sequences.
        assert!(matches!(step("key: down"), Step::Key(k) if k.bytes == vec![0x1b, b'[', b'B']));
        assert!(matches!(step("key: right"), Step::Key(k) if k.bytes == vec![0x1b, b'[', b'C']));
        assert!(matches!(step("key: left"), Step::Key(k) if k.bytes == vec![0x1b, b'[', b'D']));
    }

    #[test]
    fn key_remaining_ctrl_combos_map_to_control_bytes() {
        // ctrl+d and ctrl+z must map to their ASCII control bytes.
        assert!(matches!(step("key: ctrl+d"), Step::Key(k) if k.bytes == vec![0x04]));
        assert!(matches!(step("key: ctrl+z"), Step::Key(k) if k.bytes == vec![0x1a]));
    }

    #[test]
    fn key_esc_is_alias_for_escape() {
        // "esc" must resolve identically to "escape" (single ESC byte).
        assert!(matches!(step("key: esc"), Step::Key(k) if k.bytes == vec![0x1b]));
    }

    #[test]
    fn key_names_are_case_and_whitespace_insensitive() {
        // Names are trimmed and lowercased, so surrounding spaces and mixed case
        // resolve to the same bytes.
        assert!(matches!(step("key: \"  Enter \""), Step::Key(k) if k.bytes == vec![b'\r']));
        assert!(matches!(step("key: \"Ctrl+C\""), Step::Key(k) if k.bytes == vec![0x03]));
    }

    #[test]
    fn unknown_key_is_rejected() {
        // An unrecognized key name must fail deserialization rather than send
        // garbage bytes.
        let res: std::result::Result<Step, _> = serde_norway::from_str("key: hyperjump");
        assert!(res.is_err());
    }

    #[test]
    fn step_keys_const_matches_dispatch() {
        // (G-8) STEP_KEYS must list exactly the keys the deserializer's dispatch
        // accepts: every listed key parses into a Step, and a key NOT listed is
        // rejected as unknown. This pins the constant to the dispatch so the
        // schema↔implementation gate (which reads STEP_KEYS) reflects reality.
        for key in STEP_KEYS {
            // Each key needs a minimally valid value to reach a successful parse;
            // we only care that the key is *recognized*, so we feed shapes that
            // satisfy each payload's required fields.
            let yaml = match *key {
                "spawn" | "send" | "send_raw" => format!("{key}: bash"),
                "key" => "key: enter".to_string(),
                "wait" => "wait: 1s".to_string(),
                "expect" | "expect_not" => format!("{key}:\n  contains: x"),
                "expect_regex" => "expect_regex:\n  pattern: x".to_string(),
                "expect_file_exists" | "expect_file_changed" => format!("{key}:\n  path: f"),
                "expect_file_contains" | "expect_file_not_contains" => {
                    format!("{key}:\n  path: f\n  contains: x")
                }
                "expect_exit" => "expect_exit: 0".to_string(),
                "expect_running" => "expect_running: true".to_string(),
                "expect_json" => "expect_json:\n  path: x\n  exists: true".to_string(),
                "expect_snapshot" => "expect_snapshot:\n  file: f".to_string(),
                "expect_semantic" => "expect_semantic:\n  text: x\n  similarity: 0.5".to_string(),
                other => panic!("STEP_KEYS lists '{other}' but the test has no sample for it"),
            };
            let parsed: std::result::Result<Step, _> = serde_norway::from_str(&yaml);
            assert!(
                parsed.is_ok(),
                "STEP_KEYS key '{key}' must dispatch: {yaml}"
            );
        }

        // A key absent from STEP_KEYS must be rejected as unknown, proving the
        // list is not merely a subset of what the dispatch accepts.
        let unknown = "definitely_not_a_step";
        assert!(!STEP_KEYS.contains(&unknown));
        let res: std::result::Result<Step, _> = serde_norway::from_str(&format!("{unknown}: x"));
        assert!(res.is_err(), "a key not in STEP_KEYS must be rejected");
    }

    #[test]
    fn key_names_const_matches_resolve() {
        // (G-8 bonus) KEY_NAMES must list exactly the names Key::resolve accepts:
        // every listed name resolves to bytes, and a name NOT listed does not.
        // Pins the constant so the schema's `key` pattern can be checked against
        // the implementation's accepted set.
        for name in KEY_NAMES {
            assert!(
                Key::resolve(name).is_some(),
                "KEY_NAMES name '{name}' must resolve"
            );
        }
        assert!(
            Key::resolve("hyperjump").is_none(),
            "a name not in KEY_NAMES must not resolve"
        );
    }

    #[test]
    fn expect_with_timeout() {
        // `expect: {contains, timeout}` must parse both fields.
        let s = step("expect:\n  contains: hello\n  timeout: 5s");
        match s {
            Step::Expect(m) => {
                assert_eq!(m.contains, "hello");
                assert_eq!(
                    m.timeout.map(|d| d.as_duration()),
                    Some(std::time::Duration::from_secs(5))
                );
            }
            other => panic!("expected Expect, got {other:?}"),
        }
    }

    #[test]
    fn expect_without_timeout_defaults_none() {
        // Omitting timeout must leave it None so the runner can apply the
        // global default.
        let s = step("expect:\n  contains: hi");
        match s {
            Step::Expect(m) => assert!(m.timeout.is_none()),
            other => panic!("expected Expect, got {other:?}"),
        }
    }

    #[test]
    fn expect_not_parses_contains() {
        // expect_not carries only the substring asserted to be absent.
        let s = step("expect_not:\n  contains: error");
        match s {
            Step::ExpectNot(spec) => assert_eq!(spec.contains, "error"),
            other => panic!("expected ExpectNot, got {other:?}"),
        }
    }

    #[test]
    fn expect_not_ignores_a_stray_timeout_rather_than_erroring() {
        // (S-1) expect_not takes no timeout. A scenario that writes one (e.g.
        // authored for a hypothetical newer pitty, or by mistake) must still
        // parse — the field is silently ignored, not rejected — preserving the
        // forward-compatibility policy that keeps step specs lenient. The schema
        // (additionalProperties: false) is what flags it for the author.
        let s = step("expect_not:\n  contains: error\n  timeout: 5s");
        match s {
            Step::ExpectNot(spec) => assert_eq!(spec.contains, "error"),
            other => panic!("expected ExpectNot, got {other:?}"),
        }
    }

    #[test]
    fn expect_exit_and_running() {
        // The bare scalar form must still parse to a code with no timeout,
        // preserving the original non-blocking poll-once semantics.
        assert!(matches!(
            step("expect_exit: 0"),
            Step::ExpectExit(s) if s.code == 0 && s.timeout.is_none()
        ));
        assert!(matches!(
            step("expect_running: true"),
            Step::ExpectRunning(true)
        ));
    }

    #[test]
    fn expect_exit_struct_form_parses_code_and_timeout() {
        // The struct form must capture both the code and the deadline used to
        // poll for the child's exit before judging.
        let s = step("expect_exit:\n  code: 1\n  timeout: 5s");
        match s {
            Step::ExpectExit(spec) => {
                assert_eq!(spec.code, 1);
                assert_eq!(
                    spec.timeout.map(|d| d.as_duration()),
                    Some(std::time::Duration::from_secs(5))
                );
            }
            other => panic!("expected ExpectExit, got {other:?}"),
        }
    }

    #[test]
    fn file_steps_parse() {
        // File assertions must capture path and (where present) the substring.
        assert!(matches!(
            step("expect_file_exists:\n  path: result.txt"),
            Step::ExpectFileExists(f) if f.path == "result.txt"
        ));
        assert!(matches!(
            step("expect_file_contains:\n  path: r.txt\n  contains: ok"),
            Step::ExpectFileContains(f) if f.path == "r.txt" && f.contains == "ok"
        ));
    }

    #[test]
    fn expect_json_equals_default_source() {
        // equals with no source must default to Output and carry the typed value.
        let s = step("expect_json:\n  path: result.status\n  equals: success");
        match s {
            Step::ExpectJson(spec) => {
                assert_eq!(spec.path, "result.status");
                assert!(matches!(spec.source, Source::Output));
                assert!(spec.timeout.is_none());
                match spec.check {
                    JsonCheck::Equals(v) => assert_eq!(v, serde_json::json!("success")),
                    other => panic!("expected Equals, got {other:?}"),
                }
            }
            other => panic!("expected ExpectJson, got {other:?}"),
        }
    }

    #[test]
    fn expect_json_typed_equals_distinguishes_number_and_string() {
        // A bare numeric YAML value must deserialize to a JSON number, not a
        // string, so type-aware comparison works downstream.
        let s = step("expect_json:\n  path: code\n  equals: 200");
        match s {
            Step::ExpectJson(spec) => match spec.check {
                JsonCheck::Equals(v) => assert_eq!(v, serde_json::json!(200)),
                other => panic!("expected Equals number, got {other:?}"),
            },
            other => panic!("expected ExpectJson, got {other:?}"),
        }
    }

    #[test]
    fn expect_json_equals_accepts_yaml_structures() {
        // Nested YAML maps and lists must deserialize to the equivalent typed
        // JSON value for whole-structure equality checks.
        let s = step(
            "expect_json:\n  path: result\n  equals:\n    status: ok\n    items:\n      - 1\n      - true",
        );
        match s {
            Step::ExpectJson(spec) => match spec.check {
                JsonCheck::Equals(v) => assert_eq!(
                    v,
                    serde_json::json!({
                        "status": "ok",
                        "items": [1, true]
                    })
                ),
                other => panic!("expected Equals object, got {other:?}"),
            },
            other => panic!("expected ExpectJson, got {other:?}"),
        }
    }

    #[test]
    fn expect_json_contains_with_file_source_and_timeout() {
        // contains plus a file source and a timeout must all parse.
        let s = step(
            "expect_json:\n  path: result.message\n  contains: expired\n  \
             source:\n    file: report.json\n  timeout: 5s",
        );
        match s {
            Step::ExpectJson(spec) => {
                assert!(matches!(spec.check, JsonCheck::Contains(ref c) if c == "expired"));
                assert!(matches!(spec.source, Source::File(ref f) if f == "report.json"));
                assert_eq!(
                    spec.timeout.map(|d| d.as_duration()),
                    Some(std::time::Duration::from_secs(5))
                );
            }
            other => panic!("expected ExpectJson, got {other:?}"),
        }
    }

    #[test]
    fn expect_json_exists_parses() {
        // exists: true must select the Exists check.
        let s = step("expect_json:\n  path: result.items\n  exists: true");
        match s {
            Step::ExpectJson(spec) => assert!(matches!(spec.check, JsonCheck::Exists)),
            other => panic!("expected ExpectJson, got {other:?}"),
        }
    }

    #[test]
    fn expect_json_multiple_checks_set_invalid_reason() {
        // Specifying more than one of equals/contains/exists must record an
        // invalid_reason so the runner can reject it as a scenario error.
        let s = step("expect_json:\n  path: x\n  equals: a\n  contains: b");
        match s {
            Step::ExpectJson(spec) => {
                assert!(spec.invalid_reason.is_some());
            }
            other => panic!("expected ExpectJson, got {other:?}"),
        }
    }

    #[test]
    fn null_equals_paired_with_one_other_check_stays_executable() {
        // COMPATIBILITY (v1): 1.2.2 dropped `equals: null` while deserializing,
        // so these two shapes parsed as a single check and *ran* — verified
        // against the 1.2.2 binary, which exits 1 for the `contains` pair and 0
        // for the `exists` pair. Issue #30 made `equals: null` a real check,
        // which turned both into Scenario errors (exit 2).
        //
        // COMPATIBILITY.md forbids tightening validation so a previously valid
        // scenario becomes an error, and lists no exception for "the old
        // behaviour verified nothing". So the null `equals` is dropped for these
        // shapes and the other check runs, exactly as 1.2.2 did.
        let contains_pair = "expect_json:\n  path: x\n  equals: null\n  contains: b";
        match step(contains_pair) {
            Step::ExpectJson(spec) => {
                assert!(
                    spec.invalid_reason.is_none(),
                    "1.2.2 executed this shape, so it must not be rejected"
                );
                assert!(
                    matches!(&spec.check, JsonCheck::Contains(v) if v == "b"),
                    "the contains check must run, got {:?}",
                    spec.check
                );
            }
            other => panic!("expected ExpectJson, got {other:?}"),
        }

        let exists_pair = "expect_json:\n  path: x\n  equals: null\n  exists: true";
        match step(exists_pair) {
            Step::ExpectJson(spec) => {
                assert!(
                    spec.invalid_reason.is_none(),
                    "1.2.2 executed this shape, so it must not be rejected"
                );
                assert!(
                    matches!(spec.check, JsonCheck::Exists),
                    "the exists check must run, got {:?}",
                    spec.check
                );
            }
            other => panic!("expected ExpectJson, got {other:?}"),
        }
    }

    #[test]
    fn null_equals_alone_still_asserts_null() {
        // Issue #30's actual fix, and a pure loosening: 1.2.2 rejected this
        // (exit 2), so making it assertable breaks nobody.
        match step("expect_json:\n  path: x\n  equals: null") {
            Step::ExpectJson(spec) => {
                assert!(spec.invalid_reason.is_none());
                assert!(
                    matches!(&spec.check, JsonCheck::Equals(v) if v.is_null()),
                    "got {:?}",
                    spec.check
                );
            }
            other => panic!("expected ExpectJson, got {other:?}"),
        }
    }

    #[test]
    fn null_equals_with_two_other_checks_is_still_rejected() {
        // The guard may reject whatever 1.2.2 also rejected. 1.2.2 exits 2 here
        // (two checks remain after dropping the null), so this stays an error —
        // the compatibility carve-out is scoped to what actually executed.
        match step("expect_json:\n  path: x\n  equals: null\n  contains: b\n  exists: true") {
            Step::ExpectJson(spec) => {
                assert!(
                    spec.invalid_reason.is_some(),
                    "1.2.2 rejected this too, so it must stay rejected"
                );
            }
            other => panic!("expected ExpectJson, got {other:?}"),
        }
    }

    #[test]
    fn expect_json_no_check_sets_invalid_reason() {
        // A path with no check at all is also an authoring error -> invalid_reason.
        let s = step("expect_json:\n  path: x");
        match s {
            Step::ExpectJson(spec) => assert!(spec.invalid_reason.is_some()),
            other => panic!("expected ExpectJson, got {other:?}"),
        }
    }

    #[test]
    fn expect_json_valid_check_has_no_invalid_reason() {
        // A well-formed one-of must leave invalid_reason None so the runner runs
        // the check instead of erroring.
        let s = step("expect_json:\n  path: x\n  equals: 1");
        match s {
            Step::ExpectJson(spec) => assert!(spec.invalid_reason.is_none()),
            other => panic!("expected ExpectJson, got {other:?}"),
        }
    }

    #[test]
    fn expect_json_explicit_null_equals_is_an_assertable_check() {
        // (#30) Guarantee: `equals: null` is a real check. It must select
        // Equals(Value::Null) — so a null leaf can be asserted, as the README
        // promises — and must NOT be mistaken for an omitted `equals` (which
        // would resolve to the "no check given" one-of violation).
        for yaml in [
            "expect_json:\n  path: result.error\n  equals: null",
            "expect_json:\n  path: result.error\n  equals: ~",
        ] {
            match step(yaml) {
                Step::ExpectJson(spec) => {
                    assert!(
                        spec.invalid_reason.is_none(),
                        "explicit null must not read as a missing check: {yaml}"
                    );
                    assert!(
                        matches!(spec.check, JsonCheck::Equals(serde_json::Value::Null)),
                        "expected Equals(Null) for {yaml}, got {:?}",
                        spec.check
                    );
                }
                other => panic!("expected ExpectJson, got {other:?}"),
            }
        }
    }

    #[test]
    fn expect_json_explicit_null_equals_is_kept_executable_for_v1() {
        // HISTORY, because this test reversed.
        //
        // #30 found that `equals: null` was discarded while deserializing, so
        // pairing it with another check silently ran the *other* check and threw
        // the author's null assertion away — a false-green vector. This test
        // originally asserted that both pairings became "multiple checks"
        // Scenario errors.
        //
        // Round 15 found the cost: 1.2.2 *executed* both shapes (verified against
        // the 1.2.2 binary — exit 1 for the `contains` pair, exit 0 for the
        // `exists` pair), and COMPATIBILITY.md forbids tightening validation so a
        // previously valid scenario becomes an error. It lists no exception for
        // "the old behaviour verified nothing", and whether anyone *should* have
        // relied on it is not the criterion — execution is, and a pipeline can
        // route on exit 1 versus exit 2 without depending on the assertion's
        // meaning.
        //
        // So these two shapes keep 1.2.2's behaviour until 2.0. #30's real fix
        // survives intact and is covered by `null_equals_alone_still_asserts_null`:
        // `equals: null` on its own now asserts null, which 1.2.2 rejected, so it
        // is a pure loosening.
        for yaml in [
            "expect_json:\n  path: a\n  equals: null\n  contains: hi",
            "expect_json:\n  path: a\n  equals: null\n  exists: true",
        ] {
            match step(yaml) {
                Step::ExpectJson(spec) => {
                    assert!(
                        spec.invalid_reason.is_none(),
                        "1.2.2 executed {yaml}, so v1 must keep executing it"
                    );
                }
                other => panic!("expected ExpectJson, got {other:?}"),
            }
        }
    }

    #[test]
    fn expect_json_omitted_equals_still_reads_as_no_check() {
        // (#30) Guarantee: making an explicit null "present" must not make an
        // *omitted* `equals` present too. A bare path keeps resolving to the
        // "no check given" violation, so the two cases stay distinguishable.
        match step("expect_json:\n  path: a") {
            Step::ExpectJson(spec) => {
                let reason = spec.invalid_reason.expect("expected a one-of violation");
                assert!(
                    reason.contains("no check given"),
                    "expected a no-check reason, got {reason:?}"
                );
            }
            other => panic!("expected ExpectJson, got {other:?}"),
        }
    }

    #[test]
    fn unknown_source_keyword_resolves_to_invalid() {
        // A typo'd source keyword must resolve to Source::Invalid (not silently
        // degrade to Output) so the runner can reject it as a scenario error.
        let s = step("expect_json:\n  path: x\n  equals: 1\n  source: outpt");
        match s {
            Step::ExpectJson(spec) => {
                assert!(matches!(spec.source, Source::Invalid(ref k) if k == "outpt"));
            }
            other => panic!("expected ExpectJson, got {other:?}"),
        }
    }

    #[test]
    fn explicit_output_source_keyword_is_output() {
        // The explicit `source: output` keyword must resolve to Output.
        let s = step("expect_json:\n  path: x\n  equals: 1\n  source: output");
        match s {
            Step::ExpectJson(spec) => assert!(matches!(spec.source, Source::Output)),
            other => panic!("expected ExpectJson, got {other:?}"),
        }
    }

    #[test]
    fn expect_snapshot_parses_file_and_raw() {
        // The snapshot step must capture the file path and default raw to false.
        let plain = step("expect_snapshot:\n  file: __snapshots__/out.snap");
        match plain {
            Step::ExpectSnapshot(spec) => {
                assert_eq!(spec.file, "__snapshots__/out.snap");
                assert!(!spec.raw);
            }
            other => panic!("expected ExpectSnapshot, got {other:?}"),
        }
        let raw = step("expect_snapshot:\n  file: out.snap\n  raw: true");
        match raw {
            Step::ExpectSnapshot(spec) => assert!(spec.raw),
            other => panic!("expected ExpectSnapshot, got {other:?}"),
        }
    }

    #[test]
    fn expect_semantic_parses_text_similarity_and_source() {
        // The semantic step must capture text, similarity, and an optional file
        // source (defaulting to Output).
        let s = step("expect_semantic:\n  text: hello world\n  similarity: 0.8");
        match s {
            Step::ExpectSemantic(spec) => {
                assert_eq!(spec.text, "hello world");
                assert!((spec.similarity - 0.8).abs() < 1e-9);
                assert!(matches!(spec.source, Source::Output));
            }
            other => panic!("expected ExpectSemantic, got {other:?}"),
        }
        let with_file =
            step("expect_semantic:\n  text: hi\n  similarity: 0.5\n  source:\n    file: out.txt");
        match with_file {
            Step::ExpectSemantic(spec) => {
                assert!(matches!(spec.source, Source::File(ref f) if f == "out.txt"));
            }
            other => panic!("expected ExpectSemantic, got {other:?}"),
        }
    }
}
