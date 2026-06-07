//! Scenario configuration model (deserialize-only).
//!
//! These types mirror the YAML scenario format. They derive `Deserialize`
//! only: the config layer never serializes back out (the report layer owns
//! serialization), which keeps the two directions from accidentally coupling.

pub mod duration;
pub mod step;

use std::collections::BTreeMap;
use std::path::Path;

use serde::Deserialize;

use crate::error::PittyError;

pub use step::{
    ExpectExitSpec, ExpectJsonSpec, ExpectNotSpec, ExpectSemanticSpec, ExpectSnapshotSpec, Key,
    MatchSpec, RegexSpec, Source, SpawnSpec, Step, KEY_NAMES, STEP_KEYS,
};

/// The scenario format version this build of pitty understands.
///
/// Bumped only on a *breaking* change to the stable scenario input format (see
/// `COMPATIBILITY.md`); additive field/step changes keep version 1.
pub const SUPPORTED_VERSION: u32 = 1;

/// The default `version` when a scenario omits the field.
///
/// Why default to 1 (not reject omission): every scenario written before the
/// field existed has no `version:` key, and treating them as version 1 keeps
/// them parsing unchanged. New scenarios may state `version: 1` explicitly for
/// clarity, but the field is optional by design.
///
/// Why the literal `1`, not `SUPPORTED_VERSION`: an omitted version must always
/// mean "version 1" — the format the field-less scenarios were written for. If
/// this tracked `SUPPORTED_VERSION`, a future `2.0` build (which would bump that
/// constant) would silently reinterpret every legacy version-less scenario as
/// v2, changing its meaning without the author touching it — exactly the kind of
/// silent break `version` exists to prevent. Pinning the literal keeps
/// "omitted == v1" stable across major versions.
fn default_version() -> u32 {
    1
}

/// A complete test scenario parsed from one YAML document.
///
/// `deny_unknown_fields` is applied at this top level only (not on the nested
/// spec types). Why here: an unknown top-level key is almost always a fatal
/// typo — e.g. `stesp:` instead of `steps:` would otherwise be silently ignored,
/// leaving an empty step list that passes vacuously (a false green in CI). Why
/// NOT on the spec types (`MatchSpec`, `SpawnSpec`, `VarSpec`, `Source`, the
/// `expect_json` raw form, …): the input format is contracted to grow by
/// *adding* optional fields within v1 (see `COMPATIBILITY.md`), so a spec that
/// denied unknown fields would reject a scenario written for a newer v1.x
/// pitty when run on an older one — breaking forward compatibility. Denying
/// only the top level catches the high-value typo without freezing the spec
/// shapes, and on the untagged spec helpers it would additionally block serde's
/// variant probing.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scenario {
    /// Stable scenario format version. Optional; omitted means version 1.
    ///
    /// Validated by [`Scenario::validate_version`] after parsing: an unknown
    /// version is a Scenario error rather than a best-effort parse.
    #[serde(default = "default_version")]
    pub version: u32,

    /// Human-readable scenario name (shown by `list` and in reports).
    pub name: String,

    /// Variables available for `${var}` expansion in steps. Defaults to empty.
    #[serde(default)]
    pub variables: BTreeMap<String, VarSpec>,

    /// Matrix axes: each entry maps an axis name to the list of values to run.
    /// Empty (the default) means a plain, single-run scenario.
    ///
    /// Modeled as a `BTreeMap<String, Vec<String>>`: each axis is a key, and the
    /// runner takes the Cartesian product of all axes (see
    /// [`Scenario::matrix_axes`]). The `BTreeMap` ordering also fixes the axis
    /// order deterministically (lexicographic by key) so the product expansion is
    /// reproducible run to run.
    #[serde(default)]
    pub matrix: BTreeMap<String, Vec<String>>,

    /// Extra environment variables injected into every spawned process.
    #[serde(default)]
    pub env: BTreeMap<String, String>,

    /// Workspace (cwd / tempdir) configuration.
    #[serde(default)]
    pub workspace: Workspace,

    /// Ordered list of steps to execute; hard errors abort immediately, while
    /// assertion failures are collected rather than stopping the run.
    #[serde(default)]
    pub steps: Vec<Step>,
}

impl Scenario {
    /// Parse a scenario from YAML source text.
    ///
    /// Deserialization failures are classified as [`PittyError::Scenario`]
    /// (exit code 2) since they indicate a malformed scenario rather than a
    /// failed assertion or a process fault.
    pub fn from_yaml(src: &str) -> Result<Self, PittyError> {
        let scenario: Scenario = serde_norway::from_str(src)
            .map_err(|e| PittyError::Scenario(format!("failed to parse scenario: {e}")))?;
        scenario.validate_version()?;
        Ok(scenario)
    }

    /// Reject a scenario declaring a version this build does not support.
    ///
    /// Why error rather than parse-and-continue on a newer version: a newer
    /// scenario may use steps or fields this build does not know. With top-level
    /// `deny_unknown_fields` a brand-new top key would already error, but a newer
    /// step *value* shape could still deserialize loosely and run with the new
    /// semantics silently dropped — a false green. Refusing up front tells the
    /// user to update pitty instead of trusting a partial interpretation. An
    /// older-than-1 version cannot occur (1 is the floor and the default).
    fn validate_version(&self) -> Result<(), PittyError> {
        let is_supported = self.version == SUPPORTED_VERSION;
        if is_supported {
            return Ok(());
        }
        Err(PittyError::Scenario(format!(
            "unsupported scenario version {}; this pitty supports version {} \
             (update pitty for newer scenarios)",
            self.version, SUPPORTED_VERSION
        )))
    }

    /// The non-empty values of every `secret: true` variable.
    ///
    /// Mirrors what [`crate::workspace::Workspace::prepare`] registers as the
    /// masking set, exposed on `Scenario` so callers that need the secret list
    /// for masking (e.g. GitHub Actions output) can obtain it without building a
    /// full workspace (which would create temp dirs and resolve env).
    pub fn secret_values(&self) -> Vec<String> {
        self.variables
            .values()
            .filter(|spec| spec.is_secret())
            .map(|spec| spec.value().to_string())
            .filter(|value| !value.is_empty())
            .collect()
    }

    /// Read and parse a scenario from a file path.
    pub fn from_path(path: &Path) -> Result<Self, PittyError> {
        let src = std::fs::read_to_string(path).map_err(|e| {
            PittyError::Scenario(format!("cannot read scenario '{}': {e}", path.display()))
        })?;
        Self::from_yaml(&src)
    }

    /// Whether this scenario declares a `matrix` section.
    pub fn has_matrix(&self) -> bool {
        !self.matrix.is_empty()
    }

    /// Resolve and validate every matrix axis to execute.
    ///
    /// Returns the `(axis_name, values)` pairs — in `BTreeMap` key order — when
    /// the scenario carries one or more axes, each with a non-empty value list,
    /// a key referenced as `${key}` somewhere expansion reaches, and no collision
    /// with a `secret: true` variable. The runner takes the Cartesian product of
    /// these axes. All failure modes are [`PittyError::Scenario`] (exit code 2):
    /// - no matrix section at all (callers should branch on [`Scenario::has_matrix`]
    ///   first; this guards the direct call),
    /// - any axis with an empty value list (`command: []`) — that axis would make
    ///   the product empty and the matrix pass vacuously, a false green for CI,
    /// - any axis whose key collides with a `secret: true` variable — injection
    ///   would overwrite the secret with a plain value and unmask it,
    /// - any axis key never referenced as `${key}` (an authoring mistake: the
    ///   injected value would silently do nothing).
    ///
    /// Why every axis is checked (not just one): each axis is injected and varied
    /// independently, so each must individually satisfy the same contracts the
    /// single-axis path enforced; a single bad axis can poison the whole product.
    pub fn matrix_axes(&self) -> Result<Vec<(&str, &[String])>, PittyError> {
        if self.matrix.is_empty() {
            return Err(PittyError::Scenario(
                "no matrix section; use `pitty run` for plain scenarios".to_string(),
            ));
        }

        let mut axes = Vec::with_capacity(self.matrix.len());
        // BTreeMap iteration is key-sorted, so the returned axis order is
        // deterministic (lexicographic) and the product expansion is reproducible.
        for (axis, values) in &self.matrix {
            // (R-1) An empty value list contributes zero values to the product,
            // making the whole product empty: worst-of-zero aggregates to exit 0,
            // a false green that hides a scenario that never actually ran. Reject
            // it as an authoring mistake rather than silently passing.
            if values.is_empty() {
                return Err(PittyError::Scenario(format!(
                    "matrix axis '{axis}' has no values"
                )));
            }

            // (C-1) Injection overwrites `variables[axis]` with a `VarSpec::Plain`,
            // which would strip a `secret: true` flag the scenario declared under
            // the same name and silently unmask the value everywhere (stdout JSON,
            // logs, errors). Why reject rather than preserve the secret flag:
            // matrix values are written in plaintext in the YAML, so a secret axis
            // is a design contradiction; an explicit error is safer than a value
            // that looks secret-declared yet leaks per cell.
            let collides_with_secret = self
                .variables
                .get(axis)
                .map(VarSpec::is_secret)
                .unwrap_or(false);
            if collides_with_secret {
                return Err(PittyError::Scenario(format!(
                    "matrix axis '{axis}' collides with a secret-declared variable"
                )));
            }

            // Static reference check: the axis value is injected into the
            // same-named variable, so unless some expansion-target text references
            // `${axis}` the value would do nothing — almost certainly a mistake.
            let placeholder = format!("${{{axis}}}");
            let is_referenced = self
                .expansion_targets()
                .any(|text| text.contains(&placeholder));
            if !is_referenced {
                return Err(PittyError::Scenario(format!(
                    "matrix key '{axis}' is never referenced as ${{{axis}}}"
                )));
            }

            axes.push((axis.as_str(), values.as_slice()));
        }

        Ok(axes)
    }

    /// Iterate over every text that passes through `${var}` expansion at run
    /// time: each `spawn.command`, `send`, `send_raw`, `spawn.env` value, and
    /// scenario-level `env` value. Used by [`Scenario::matrix_axes`] to detect an
    /// axis key that is never referenced.
    fn expansion_targets(&self) -> impl Iterator<Item = &str> {
        let env_values = self.env.values().map(String::as_str);
        let step_values = self.steps.iter().flat_map(step_expansion_texts);
        env_values.chain(step_values)
    }
}

/// The texts within a single [`Step`] that pass through `${var}` expansion at
/// run time. Empty for steps that carry no expandable text.
///
/// Matches every [`Step`] variant exhaustively rather than using a `_ =>`
/// wildcard. Why not a wildcard: a wildcard would silently treat any future
/// step kind as having no expansion targets, so a new step that *does* expand
/// `${var}` would slip past [`Scenario::matrix_axes`]'s reference check and let
/// an unreferenced-axis authoring mistake through undetected. Listing every
/// variant forces the compiler to flag this function when a step is added.
fn step_expansion_texts(step: &Step) -> Vec<&str> {
    match step {
        // The command plus each spawn-local env value all expand.
        Step::Spawn(spec) => std::iter::once(spec.command.as_str())
            .chain(spec.env.values().map(String::as_str))
            .collect(),
        Step::Send(s) | Step::SendRaw(s) => vec![s.as_str()],
        // No `${var}` expansion reaches these steps' fields today: keys resolve
        // to fixed bytes, waits/expects/file/json/snapshot/semantic assertions
        // compare against captured output rather than feeding expanded input.
        Step::Key(_)
        | Step::Wait(_)
        | Step::Expect(_)
        | Step::ExpectRegex(_)
        | Step::ExpectNot(_)
        | Step::ExpectFileExists(_)
        | Step::ExpectFileContains(_)
        | Step::ExpectFileNotContains(_)
        | Step::ExpectFileChanged(_)
        | Step::ExpectExit(_)
        | Step::ExpectRunning(_)
        | Step::ExpectJson(_)
        | Step::ExpectSnapshot(_)
        | Step::ExpectSemantic(_) => Vec::new(),
    }
}

/// Workspace configuration: where the scenario's commands run.
#[derive(Debug, Clone, Deserialize)]
pub struct Workspace {
    /// Working directory (relative to the scenario file's directory). Ignored
    /// when `temp` is true.
    #[serde(default = "default_cwd")]
    pub cwd: String,

    /// When true, run inside a fresh temp directory created via
    /// `tempfile::TempDir` and cleaned up on drop (`0700` on Unix).
    #[serde(default)]
    pub temp: bool,
}

impl Default for Workspace {
    fn default() -> Self {
        Workspace {
            cwd: default_cwd(),
            temp: false,
        }
    }
}

fn default_cwd() -> String {
    ".".to_string()
}

/// A scenario variable, either a plain value or a secret to be masked in logs.
///
/// Untagged so YAML can write either `name: value` or
/// `name: {value: ..., secret: true}`. The two shapes are unambiguous (scalar
/// vs. mapping), so untagged here does not suffer the error-message problems
/// that make us avoid it for [`Step`].
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum VarSpec {
    /// A plain, non-secret string value.
    Plain(String),
    /// A value flagged as secret; its literal text is masked in logs/errors.
    Secret {
        /// The secret value.
        value: String,
        /// Marker; must be present (and typically `true`) to select this form.
        secret: bool,
    },
}

impl VarSpec {
    /// The underlying string value, regardless of secrecy.
    pub fn value(&self) -> &str {
        match self {
            VarSpec::Plain(v) => v,
            VarSpec::Secret { value, .. } => value,
        }
    }

    /// Whether this variable's value should be masked in logs and errors.
    pub fn is_secret(&self) -> bool {
        matches!(self, VarSpec::Secret { secret: true, .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_full_scenario() {
        // A representative scenario must populate name, variables, env,
        // workspace, and steps.
        let yaml = r#"
name: demo
variables:
  username: test-user
  token:
    value: secret-token
    secret: true
env:
  NODE_ENV: test
workspace:
  temp: true
steps:
  - spawn: bash
  - send: echo hello
  - expect:
      contains: hello
"#;
        let s = Scenario::from_yaml(yaml).unwrap();
        assert_eq!(s.name, "demo");
        assert_eq!(s.variables.len(), 2);
        assert!(!s.variables["username"].is_secret());
        assert!(s.variables["token"].is_secret());
        assert_eq!(s.variables["token"].value(), "secret-token");
        assert_eq!(s.env["NODE_ENV"], "test");
        assert!(s.workspace.temp);
        assert_eq!(s.steps.len(), 3);
    }

    #[test]
    fn version_defaults_to_one_when_omitted() {
        // Scenarios written before the field existed have no version key and
        // must parse as version 1, unchanged.
        let s = Scenario::from_yaml("name: x\nsteps: []").unwrap();
        assert_eq!(s.version, 1);
    }

    #[test]
    fn explicit_version_one_is_accepted() {
        // Stating `version: 1` explicitly is valid and equivalent to omitting it.
        let s = Scenario::from_yaml("version: 1\nname: x\nsteps: []").unwrap();
        assert_eq!(s.version, 1);
    }

    #[test]
    fn version_two_is_scenario_error() {
        // A version newer than this build supports must be a Scenario error
        // (exit 2): silently parsing it could drop new semantics into a false
        // green.
        let err = Scenario::from_yaml("version: 2\nname: x\nsteps: []").unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert!(err.to_string().contains("unsupported scenario version 2"));
    }

    #[test]
    fn version_zero_is_scenario_error() {
        // (G-1) version 0 deserializes as a valid u32 but is below the supported
        // floor, so validate_version rejects it with a friendly Scenario error
        // (exit 2) — not a silent parse.
        let err = Scenario::from_yaml("version: 0\nname: x\nsteps: []").unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert!(err.to_string().contains("unsupported scenario version 0"));
    }

    #[test]
    fn version_negative_string_and_float_are_scenario_errors() {
        // (G-1) A negative, string, or float version cannot deserialize into the
        // `u32` field, so serde fails at parse time. Each must surface as a
        // Scenario error (exit 2), the same class as an unsupported version, so a
        // mistyped version never silently parses as 1.
        for bad in ["version: -1", "version: \"1\"", "version: 1.0"] {
            let yaml = format!("{bad}\nname: x\nsteps: []");
            let result = Scenario::from_yaml(&yaml);
            let err = result
                .err()
                .unwrap_or_else(|| panic!("'{bad}' must be a Scenario error"));
            assert_eq!(err.exit_code(), 2, "'{bad}' must exit 2");
        }
    }

    #[test]
    fn unknown_top_level_key_is_scenario_error() {
        // A misspelled top-level key (here `stesp:` for `steps:`) must error
        // rather than be silently ignored, which would leave an empty step list
        // that passes vacuously.
        let err = Scenario::from_yaml("name: x\nstesp:\n  - spawn: bash\n").unwrap_err();
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn all_seven_known_top_level_keys_are_accepted() {
        // The full set of documented top-level keys must coexist under
        // deny_unknown_fields without tripping the unknown-key guard.
        let yaml = r#"
version: 1
name: full
variables:
  u: user
env:
  E: v
workspace:
  temp: true
matrix:
  command: [a]
steps:
  - spawn: "${command}"
"#;
        let s = Scenario::from_yaml(yaml).unwrap();
        assert_eq!(s.name, "full");
    }

    #[test]
    fn unknown_field_inside_a_spec_is_still_accepted() {
        // deny_unknown_fields is top-level only: an unknown field inside a step
        // spec must still parse (forward compatibility for additive v1.x fields).
        let s = Scenario::from_yaml(
            "name: x\nsteps:\n  - expect:\n      contains: hi\n      future: 1\n",
        )
        .unwrap();
        assert_eq!(s.steps.len(), 1);
    }

    #[test]
    fn secret_values_collects_only_nonempty_secrets() {
        // secret_values must return the values of secret-flagged variables only,
        // skipping plain variables and empty secrets, matching the masker's set.
        let yaml = r#"
name: x
variables:
  plain: visible
  tok:
    value: s3cr3t
    secret: true
  empty:
    value: ""
    secret: true
steps: []
"#;
        let s = Scenario::from_yaml(yaml).unwrap();
        assert_eq!(s.secret_values(), vec!["s3cr3t".to_string()]);
    }

    #[test]
    fn workspace_defaults_to_cwd_dot() {
        // Omitting workspace must default cwd to "." and temp to false.
        let s = Scenario::from_yaml("name: x\nsteps: []").unwrap();
        assert_eq!(s.workspace.cwd, ".");
        assert!(!s.workspace.temp);
    }

    #[test]
    fn invalid_yaml_is_scenario_error() {
        // Malformed YAML must surface as a Scenario error (exit code 2).
        let err = Scenario::from_yaml("name: [unterminated").unwrap_err();
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn scenario_without_matrix_has_no_matrix() {
        // A scenario with no matrix section is a plain scenario: has_matrix is
        // false and matrix_axes reports the no-section error.
        let s = Scenario::from_yaml("name: x\nsteps: []").unwrap();
        assert!(!s.has_matrix());
        assert_eq!(s.matrix_axes().unwrap_err().exit_code(), 2);
    }

    #[test]
    fn single_axis_matrix_with_reference_resolves() {
        // A single axis whose key is referenced as ${command} in spawn must
        // resolve to a one-element list carrying that axis name and its values.
        let yaml = r#"
name: bugfix
matrix:
  command: [a, b, c]
steps:
  - spawn: "${command} --fix bug.py"
  - expect: {contains: fixed}
"#;
        let s = Scenario::from_yaml(yaml).unwrap();
        assert!(s.has_matrix());
        let axes = s.matrix_axes().unwrap();
        assert_eq!(axes.len(), 1);
        assert_eq!(axes[0].0, "command");
        assert_eq!(
            axes[0].1,
            &["a".to_string(), "b".to_string(), "c".to_string()]
        );
    }

    #[test]
    fn multi_axis_matrix_resolves_all_axes_in_key_order() {
        // Two axes must both resolve, returned in BTreeMap key order (region
        // before command is false — `command` sorts before `region`), each with
        // its own value list, so the runner can take the Cartesian product.
        let yaml = r#"
name: multi
matrix:
  region: [x, y]
  command: [a, b]
steps:
  - spawn: "${command} ${region}"
"#;
        let s = Scenario::from_yaml(yaml).unwrap();
        let axes = s.matrix_axes().unwrap();
        assert_eq!(axes.len(), 2);
        // Key-sorted: "command" precedes "region".
        assert_eq!(axes[0].0, "command");
        assert_eq!(axes[1].0, "region");
        assert_eq!(axes[0].1, &["a".to_string(), "b".to_string()]);
        assert_eq!(axes[1].1, &["x".to_string(), "y".to_string()]);
    }

    #[test]
    fn empty_axis_value_list_is_scenario_error() {
        // (R-1) An empty value list (`command: []`) makes the product empty and
        // would pass vacuously; matrix_axes must reject it as a Scenario error
        // (exit 2) even when the key is otherwise correctly referenced.
        let yaml = r#"
name: empty-axis
matrix:
  command: []
steps:
  - spawn: "${command}"
"#;
        let s = Scenario::from_yaml(yaml).unwrap();
        let err = s.matrix_axes().unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert!(err.to_string().contains("has no values"));
    }

    #[test]
    fn empty_value_list_on_a_second_axis_is_scenario_error() {
        // Validation is per-axis: a valid first axis must not mask an empty
        // second axis. The empty `region` axis must still be a Scenario error.
        let yaml = r#"
name: empty-second-axis
matrix:
  command: [a, b]
  region: []
steps:
  - spawn: "${command} ${region}"
"#;
        let s = Scenario::from_yaml(yaml).unwrap();
        let err = s.matrix_axes().unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert!(err.to_string().contains("region"));
        assert!(err.to_string().contains("has no values"));
    }

    #[test]
    fn axis_colliding_with_secret_variable_is_scenario_error() {
        // (C-1) An axis name that also names a `secret: true` variable would be
        // overwritten with a plain injected value, silently unmasking it. This
        // must be a Scenario error (exit 2) rather than a quiet de-masking.
        let yaml = r#"
name: secret-axis
variables:
  command:
    value: secret-impl
    secret: true
matrix:
  command: [a, b]
steps:
  - spawn: "${command}"
"#;
        let s = Scenario::from_yaml(yaml).unwrap();
        let err = s.matrix_axes().unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert!(err.to_string().contains("collides with a secret-declared"));
    }

    #[test]
    fn secret_collision_on_any_axis_is_scenario_error() {
        // The secret-collision guard is checked on every axis, not only the
        // first: a secret colliding with the *second* axis must still be rejected
        // so a multi-axis matrix cannot unmask a secret through any axis.
        let yaml = r#"
name: secret-second-axis
variables:
  region:
    value: secret-region
    secret: true
matrix:
  command: [a, b]
  region: [x, y]
steps:
  - spawn: "${command} ${region}"
"#;
        let s = Scenario::from_yaml(yaml).unwrap();
        let err = s.matrix_axes().unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert!(err.to_string().contains("region"));
        assert!(err.to_string().contains("collides with a secret-declared"));
    }

    #[test]
    fn axis_colliding_with_plain_variable_is_allowed() {
        // The secret-collision guard must not over-trigger: an axis sharing its
        // name with a *plain* (non-secret) variable is the normal override case
        // and must still resolve.
        let yaml = r#"
name: plain-collision
variables:
  command: static-default
matrix:
  command: [a, b]
steps:
  - spawn: "${command}"
"#;
        let s = Scenario::from_yaml(yaml).unwrap();
        assert!(s.matrix_axes().is_ok());
    }

    #[test]
    fn unreferenced_axis_key_is_scenario_error() {
        // Defining an axis whose key never appears as ${key} in any expansion
        // target is an authoring mistake -> Scenario error.
        let yaml = r#"
name: typo
matrix:
  command: [a, b]
steps:
  - spawn: "echo hello"
"#;
        let s = Scenario::from_yaml(yaml).unwrap();
        let err = s.matrix_axes().unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert!(err.to_string().contains("never referenced"));
    }

    #[test]
    fn unreferenced_second_axis_key_is_scenario_error() {
        // The reference check runs on every axis: a referenced first axis must
        // not excuse an unreferenced second axis.
        let yaml = r#"
name: typo-second
matrix:
  command: [a, b]
  region: [x, y]
steps:
  - spawn: "${command}"
"#;
        let s = Scenario::from_yaml(yaml).unwrap();
        let err = s.matrix_axes().unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert!(err.to_string().contains("region"));
        assert!(err.to_string().contains("never referenced"));
    }

    #[test]
    fn axis_reference_check_is_not_a_loose_substring_match() {
        // (R-2) The reference check matches the full `${axis}` placeholder
        // (braces included), so a longer name that merely shares a prefix must
        // not satisfy it. Axis `command` with only `${command2}` in the text is
        // unreferenced -> error...
        let unreferenced = Scenario::from_yaml(
            "name: r2a\nmatrix:\n  command: [a]\nsteps:\n  - spawn: \"${command2}\"\n",
        )
        .unwrap();
        let err = unreferenced.matrix_axes().unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert!(err.to_string().contains("never referenced"));

        // ...while axis `command2` against the same `${command2}` text is a real
        // reference and must resolve.
        let referenced = Scenario::from_yaml(
            "name: r2b\nmatrix:\n  command2: [a]\nsteps:\n  - spawn: \"${command2}\"\n",
        )
        .unwrap();
        assert!(referenced.matrix_axes().is_ok());
    }

    #[test]
    fn axis_reference_in_send_and_env_counts() {
        // ${key} appearing in a send step (not only spawn) satisfies the
        // reference check.
        let from_send = Scenario::from_yaml(
            "name: s\nmatrix:\n  command: [a]\nsteps:\n  - spawn: bash\n  - send: \"${command}\"\n",
        )
        .unwrap();
        assert!(from_send.matrix_axes().is_ok());

        // ${key} in a scenario-level env value also counts.
        let from_env = Scenario::from_yaml(
            "name: e\nmatrix:\n  command: [a]\nenv:\n  TOOL: \"${command}\"\nsteps:\n  - spawn: bash\n",
        )
        .unwrap();
        assert!(from_env.matrix_axes().is_ok());
    }
}
