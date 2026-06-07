//! Workspace setup: working directory, environment, `${var}` expansion, and
//! secret masking registration.
//!
//! A workspace either runs in an existing directory (relative to the scenario
//! file) or in a fresh `0700` temp directory. It resolves the scenario's
//! variables and environment, knows how to expand `${var}` placeholders in
//! step payloads, and registers secret values so logs and errors can mask them.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use tempfile::TempDir;

use crate::config::Scenario;
use crate::error::PittyError;

/// A prepared workspace for a scenario run.
pub struct Workspace {
    /// The directory commands run in.
    cwd: PathBuf,
    /// Variable name -> value, used for `${var}` expansion.
    variables: BTreeMap<String, String>,
    /// Parent-process environment, captured at prepare time, used as a
    /// fallback for `${var}` expansion when a name is not a scenario variable.
    ///
    /// Why a captured snapshot rather than reading `std::env::var` lazily at
    /// expansion time: capturing once keeps expansion deterministic for a run
    /// (the environment cannot shift mid-run) and keeps `expand` a pure
    /// function of the workspace, which is what the unit tests exercise.
    parent_env: BTreeMap<String, String>,
    /// Resolved environment for spawned processes.
    env: Vec<(String, String)>,
    /// Literal secret values to mask (`***`) in logs and errors.
    secrets: Vec<String>,
    /// Held to keep a temp directory alive; dropping it removes the dir. Kept
    /// even though unread so its `Drop` runs at the end of the run.
    _temp: Option<TempDir>,
}

impl Workspace {
    /// Prepare a workspace from a scenario, resolving paths against
    /// `base_dir` (the directory containing the scenario file).
    ///
    /// When `workspace.temp` is set, a `0700` temp directory is created via
    /// `tempfile::TempDir`. We use `mkdtemp`-backed `TempDir` rather than
    /// constructing our own predictable name: self-named temp dirs are prone
    /// to races and symlink attacks, whereas `TempDir` creates the directory
    /// atomically with a random name.
    pub fn prepare(scenario: &Scenario, base_dir: &Path) -> Result<Self, PittyError> {
        let mut secrets = Vec::new();
        let mut variables = BTreeMap::new();
        for (name, spec) in &scenario.variables {
            let value = spec.value();
            variables.insert(name.clone(), value.to_string());
            if spec.is_secret() && !value.is_empty() {
                secrets.push(value.to_string());
            }
        }

        let env: Vec<(String, String)> = scenario
            .env
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        let (cwd, temp) = if scenario.workspace.temp {
            let dir = TempDir::new().map_err(|e| {
                PittyError::Process(format!("failed to create temp workspace: {e}"))
            })?;
            set_permissions_0700(dir.path())?;
            (dir.path().to_path_buf(), Some(dir))
        } else {
            let resolved = base_dir.join(&scenario.workspace.cwd);
            (resolved, None)
        };

        Ok(Workspace {
            cwd,
            variables,
            parent_env: std::env::vars().collect(),
            env,
            secrets,
            _temp: temp,
        })
    }

    /// The directory commands run in.
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// The resolved environment for spawned processes.
    pub fn env(&self) -> &[(String, String)] {
        &self.env
    }

    /// The registered secret values, for the logger/masker.
    pub fn secrets(&self) -> &[String] {
        &self.secrets
    }

    /// Resolve a path that the scenario expresses relative to the workspace.
    ///
    /// This is the *read* resolver: it joins `rel` onto the workspace cwd
    /// without confining the result. Under the single-trust model, read targets
    /// (`expect_file_*`, `source: {file}`) are allowed to point anywhere the user
    /// could already read, so no containment check is imposed here.
    pub fn resolve_path(&self, rel: &str) -> PathBuf {
        self.cwd.join(rel)
    }

    /// Resolve a *write* path, confined to the workspace directory.
    ///
    /// Returns `Err` (a Scenario error message) when `rel` resolves outside the
    /// workspace root, whether via `..` segments or a symlink that points out.
    ///
    /// Why containment for writes when reads (`resolve_path`) are unconfined:
    /// under single-trust we accept that a scenario can *read* anything the user
    /// can read, so confining read targets would add friction for no real safety
    /// gain. A *write*, however, mutates the filesystem, and `expect_snapshot`
    /// under `--update` (especially with `PITTY_UPDATE_SNAPSHOTS=1` set
    /// globally in CI) records files automatically. A broken or hostile scenario
    /// with `file: ../../../tmp/x.snap` would then write outside the workspace on
    /// every run. Confining writes to the workspace keeps an automated record
    /// step from clobbering arbitrary paths, while leaving the (harmless) read
    /// asymmetry intact.
    pub fn resolve_write_path(&self, rel: &str) -> Result<PathBuf, PittyError> {
        let candidate = self.cwd.join(rel);
        let root = canonical_root(&self.cwd);
        // Canonicalize as far as the path exists: a brand-new snapshot file (and
        // possibly its parent dirs) does not exist yet, so we cannot canonicalize
        // the full path. We canonicalize the deepest existing ancestor (which
        // resolves any symlinks in the real portion) and re-append the not-yet-
        // created tail, then confirm the whole thing stays under the root.
        let resolved = resolve_existing_ancestor(&candidate);
        if is_within(&resolved, &root) {
            return Ok(candidate);
        }
        Err(PittyError::Scenario(format!(
            "snapshot path '{rel}' escapes the workspace; \
             snapshot writes are confined to the workspace directory"
        )))
    }

    /// Expand `${var}` placeholders in `input`.
    ///
    /// Resolution order per name: scenario `variables` first, then the
    /// parent-process environment captured at prepare time as a fallback, then
    /// the literal `${name}` text if still undefined.
    ///
    /// Why the parent-env fallback: dogfood "meta" scenarios spawn an inner
    /// `pitty` whose absolute path the surrounding CI exports as an
    /// environment variable (`PITTY_BIN`). Scenario `variables` cannot carry
    /// a caller-supplied path (they are baked into the YAML), and scenario `env`
    /// is injected into the spawned child rather than consulted by `${var}`
    /// expansion of `spawn.command`. Falling back to the parent env lets the
    /// caller parameterize a `spawn.command` path without editing the YAML. Why
    /// not error or special-case only `spawn`: keeping one uniform resolution
    /// rule for every step is simpler, and the precedence below preserves the
    /// prior behavior — scenario variables still win, so nothing a scenario
    /// already defined changes meaning.
    ///
    /// Unknown variables (absent from both sources) are left untouched as their
    /// literal `${name}` text rather than erroring: a missing variable is most
    /// often a typo the user will see verbatim in the sent input, which is
    /// easier to diagnose than a hard failure mid-run. Use `$$` to emit a
    /// literal `$`.
    ///
    /// Secret masking is unaffected: only values from `variables` flagged
    /// `secret: true` are registered for masking. Parent-env fallback values
    /// are never registered, matching the existing rule that only scenario-
    /// declared secrets are masked.
    pub fn expand(&self, input: &str) -> String {
        let bytes = input.as_bytes();
        let mut out = String::with_capacity(input.len());
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'$' {
                // Literal `$` via `$$`.
                if i + 1 < bytes.len() && bytes[i + 1] == b'$' {
                    out.push('$');
                    i += 2;
                    continue;
                }
                // `${name}` form. `OPEN` skips the `${` prefix (2 bytes); the
                // matching `}` is `CLOSE` (1 byte) past the name.
                const OPEN: usize = 2; // length of "${"
                const CLOSE: usize = 1; // length of "}"
                if i + 1 < bytes.len() && bytes[i + 1] == b'{' {
                    let name_start = i + OPEN;
                    if let Some(close) = input[name_start..].find('}') {
                        let name_end = name_start + close;
                        let name = &input[name_start..name_end];
                        // Index just past the closing brace.
                        let after_close = name_end + CLOSE;
                        // Scenario variables win; the parent env is only a
                        // fallback, so a scenario that defines a variable keeps
                        // its prior meaning regardless of the ambient env.
                        match self
                            .variables
                            .get(name)
                            .or_else(|| self.parent_env.get(name))
                        {
                            Some(value) => out.push_str(value),
                            // Unknown in both: re-emit the verbatim `${name}`.
                            None => out.push_str(&input[i..after_close]),
                        }
                        i = after_close;
                        continue;
                    }
                }
            }
            // Default: copy this byte's character. Indexing is safe because we
            // only ever advance by whole UTF-8 chars here.
            let ch = input[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
        out
    }
}

/// Apply `0700` permissions to a directory (Unix only).
#[cfg(unix)]
fn set_permissions_0700(path: &Path) -> Result<(), PittyError> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .map_err(|e| PittyError::Process(format!("failed to set temp dir mode: {e}")))
}

/// No-op on non-Unix. pitty targets Unix PTYs, but keep this compiling
/// portably so a non-Unix build fails on PTY use, not on permissions.
#[cfg(not(unix))]
fn set_permissions_0700(_path: &Path) -> Result<(), PittyError> {
    Ok(())
}

/// Canonicalize the workspace root for containment checks.
///
/// Falls back to a lexical normalization when the cwd does not yet exist on disk
/// (a `workspace.cwd` may name a dir created later), so containment still has a
/// stable root to compare against.
fn canonical_root(cwd: &Path) -> PathBuf {
    cwd.canonicalize()
        .unwrap_or_else(|_| lexical_normalize(cwd))
}

/// Canonicalize the deepest existing ancestor of `path` and re-append the
/// non-existent tail.
///
/// Why not `path.canonicalize()` directly: the snapshot file (and possibly its
/// parent directories) does not exist yet on a first `--update` record, so a
/// full canonicalize would fail. Resolving the existing prefix still follows any
/// symlink in the real portion of the path (so a symlink-out is caught), and the
/// remaining lexical tail cannot introduce a new symlink because those
/// components do not exist yet.
fn resolve_existing_ancestor(path: &Path) -> PathBuf {
    let mut existing = path;
    loop {
        if let Ok(real) = existing.canonicalize() {
            let tail = path.strip_prefix(existing).unwrap_or(Path::new(""));
            return lexical_normalize(&real.join(tail));
        }
        match existing.parent() {
            Some(parent) => existing = parent,
            // No ancestor exists (e.g. a relative path with no real root):
            // fall back to a purely lexical normalization.
            None => return lexical_normalize(path),
        }
    }
}

/// Lexically normalize a path by resolving `.`/`..` segments without touching
/// the filesystem. Used as a fallback when a path (or root) is not yet on disk.
fn lexical_normalize(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                // Pop the last real segment; if there is none, keep the `..` so a
                // path that climbs above its root still reads as "outside".
                if !out.pop() {
                    out.push("..");
                }
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Whether `path` is the root itself or lies beneath it.
fn is_within(path: &Path, root: &Path) -> bool {
    path == root || path.starts_with(root)
}

/// Replace every registered secret value in `text` with `***`.
///
/// Applied at log-write and error-format boundaries so secret values never
/// reach disk or terminal output. We walk `text` once and, at each byte
/// position, check whether any secret begins there — rather than calling
/// `str::replace` once per secret, which would allocate a fresh intermediate
/// `String` for every secret in the list. Secret lists are tiny, so checking
/// each against the current position is cheap and avoids the chained
/// allocations. Longest secrets are tried first so a secret that is a prefix of
/// another does not mask only part of the longer one.
pub fn mask_secrets(text: &str, secrets: &[String]) -> String {
    let mut active: Vec<&str> = secrets
        .iter()
        .map(String::as_str)
        .filter(|s| !s.is_empty())
        .collect();
    if active.is_empty() {
        return text.to_string();
    }
    active.sort_by_key(|s| std::cmp::Reverse(s.len()));

    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while !rest.is_empty() {
        let matched = active.iter().find(|secret| rest.starts_with(**secret));
        match matched {
            Some(secret) => {
                out.push_str("***");
                rest = &rest[secret.len()..];
            }
            None => {
                // No secret starts here: copy one whole char and advance past
                // it so we never split a multibyte UTF-8 boundary.
                let ch = rest.chars().next().expect("rest is non-empty");
                out.push(ch);
                rest = &rest[ch.len_utf8()..];
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace_with_vars(pairs: &[(&str, &str)], secrets: &[&str]) -> Workspace {
        workspace_with_vars_and_env(pairs, &[], secrets)
    }

    /// Build a workspace with explicit scenario variables and a fixed parent
    /// env so expansion precedence can be tested without touching `std::env`.
    fn workspace_with_vars_and_env(
        vars: &[(&str, &str)],
        parent_env: &[(&str, &str)],
        secrets: &[&str],
    ) -> Workspace {
        let mut variables = BTreeMap::new();
        for (k, v) in vars {
            variables.insert(k.to_string(), v.to_string());
        }
        let mut env_map = BTreeMap::new();
        for (k, v) in parent_env {
            env_map.insert(k.to_string(), v.to_string());
        }
        Workspace {
            cwd: PathBuf::from("."),
            variables,
            parent_env: env_map,
            env: Vec::new(),
            secrets: secrets.iter().map(|s| s.to_string()).collect(),
            _temp: None,
        }
    }

    #[test]
    fn expands_known_variable() {
        // ${name} must be replaced by the variable's value.
        let ws = workspace_with_vars(&[("user", "alice")], &[]);
        assert_eq!(ws.expand("hello ${user}!"), "hello alice!");
    }

    #[test]
    fn leaves_unknown_variable_literal() {
        // An undefined ${name} (absent from variables and parent env) must be
        // left verbatim so the typo is visible.
        let ws = workspace_with_vars(&[], &[]);
        assert_eq!(ws.expand("value=${missing}"), "value=${missing}");
    }

    #[test]
    fn expands_from_parent_env_when_not_a_scenario_variable() {
        // A name absent from scenario variables but present in the parent env
        // must expand from the env: this is how a meta scenario resolves the
        // CI-exported ${PITTY_BIN} path into spawn.command.
        let ws = workspace_with_vars_and_env(&[], &[("PITTY_BIN", "/abs/pitty")], &[]);
        assert_eq!(ws.expand("${PITTY_BIN} run x"), "/abs/pitty run x");
    }

    #[test]
    fn scenario_variable_takes_precedence_over_parent_env() {
        // When a name exists in both, the scenario variable wins so a scenario
        // keeps its prior meaning regardless of the ambient environment.
        let ws = workspace_with_vars_and_env(&[("X", "from-var")], &[("X", "from-env")], &[]);
        assert_eq!(ws.expand("${X}"), "from-var");
    }

    #[test]
    fn unknown_in_both_variables_and_env_stays_literal() {
        // A name in neither source must remain literal, not silently blank.
        let ws = workspace_with_vars_and_env(&[("a", "1")], &[("b", "2")], &[]);
        assert_eq!(ws.expand("${c}"), "${c}");
    }

    #[test]
    fn expansion_is_single_pass_not_recursive() {
        // (r-1) A variable whose value itself contains `${other}` is expanded in
        // a single left-to-right pass: the substituted text is NOT re-scanned, so
        // the inner `${other}` is emitted verbatim. This pins the matrix
        // contract that a matrix value containing `${...}` is not re-expanded
        // into another variable (no recursion, no injection surprise).
        let ws = workspace_with_vars(&[("command", "run ${other}"), ("other", "INNER")], &[]);
        assert_eq!(ws.expand("${command}"), "run ${other}");
    }

    #[test]
    fn double_dollar_is_literal_dollar() {
        // $$ must collapse to a single literal $ and not start an expansion.
        let ws = workspace_with_vars(&[("x", "1")], &[]);
        assert_eq!(ws.expand("cost is $$5 not ${x}"), "cost is $5 not 1");
    }

    #[test]
    fn expand_leaves_unclosed_placeholder_verbatim() {
        // An unterminated ${ has no closing brace, so it is copied literally
        // rather than consuming the rest of the string or panicking.
        let ws = workspace_with_vars(&[("x", "1")], &[]);
        assert_eq!(ws.expand("value=${x and ${"), "value=${x and ${");
    }

    #[test]
    fn expand_empty_name_placeholder_is_unknown_variable() {
        // ${} names the empty variable, which is undefined, so it is re-emitted
        // verbatim like any other unknown variable.
        let ws = workspace_with_vars(&[("x", "1")], &[]);
        assert_eq!(ws.expand("a${}b"), "a${}b");
    }

    #[test]
    fn expand_handles_multibyte_text() {
        // Expansion must not corrupt surrounding multibyte UTF-8 characters.
        let ws = workspace_with_vars(&[("name", "世界")], &[]);
        assert_eq!(ws.expand("こんにちは ${name}"), "こんにちは 世界");
    }

    #[test]
    fn prepare_registers_secret_values() {
        // Secret-flagged variables must be registered for masking; plain ones
        // must not.
        let yaml = r#"
name: s
variables:
  plain: visible
  token:
    value: top-secret
    secret: true
steps: []
"#;
        let scenario = Scenario::from_yaml(yaml).unwrap();
        let ws = Workspace::prepare(&scenario, Path::new(".")).unwrap();
        assert_eq!(ws.secrets(), &["top-secret".to_string()]);
    }

    #[test]
    fn parent_env_values_are_never_registered_as_secrets() {
        // A value resolved through the parent-env fallback (not a scenario
        // `secret: true` variable) must NOT be registered for masking. This
        // pins the deliberate boundary that only scenario-declared secrets are
        // masked: parameterizing a `spawn.command` via the ambient env is a
        // non-secret path, and silently masking ambient values would both hide
        // legitimate output and imply a protection the framework does not give.
        // A scenario passing a real secret this way is using the wrong channel
        // (it must declare `secret: true`), and this test fixes that contract so
        // a future change cannot quietly start masking — or claim to mask —
        // parent-env values.
        let key = "PITTY_TEST_PARENT_ENV_SECRET";
        // Set on this process so `prepare`'s `std::env::vars()` snapshot sees it.
        std::env::set_var(key, "ambient-would-be-secret");
        let yaml = format!("name: env-fallback\nsteps:\n  - send: \"${{{key}}}\"\n");
        let scenario = Scenario::from_yaml(&yaml).unwrap();
        let ws = Workspace::prepare(&scenario, Path::new(".")).unwrap();
        std::env::remove_var(key);

        // The value expands from the parent env (proving it is reachable)...
        assert_eq!(ws.expand(&format!("${{{key}}}")), "ambient-would-be-secret");
        // ...yet it is absent from the masked set: parent-env values are never
        // secrets.
        assert!(ws.secrets().is_empty());
        assert!(!ws.secrets().iter().any(|s| s == "ambient-would-be-secret"));
    }

    #[test]
    fn mask_secrets_replaces_all_occurrences() {
        // Every occurrence of a secret must be replaced in diagnostic text.
        let masked = mask_secrets("token=abc123 retry token=abc123", &["abc123".to_string()]);
        assert_eq!(masked, "token=*** retry token=***");
    }

    #[test]
    fn mask_secrets_ignores_empty_secret() {
        // An empty secret must not turn into a degenerate replace-everything.
        let masked = mask_secrets("unchanged", &["".to_string()]);
        assert_eq!(masked, "unchanged");
    }

    #[test]
    fn mask_secrets_prefers_longest_overlapping_secret() {
        // When one secret is a prefix of another, the longer one must win so the
        // full value is masked rather than leaving its tail exposed.
        let masked = mask_secrets(
            "value=supersecret-key end",
            &["super".to_string(), "supersecret-key".to_string()],
        );
        assert_eq!(masked, "value=*** end");
    }

    #[test]
    fn mask_secrets_preserves_multibyte_context() {
        // Masking must not split surrounding multibyte UTF-8 when scanning.
        let masked = mask_secrets("鍵=tok値", &["tok".to_string()]);
        assert_eq!(masked, "鍵=***値");
    }

    /// Build a workspace rooted at a real on-disk `cwd` so write-path
    /// containment can be exercised against actual canonicalization.
    fn workspace_at(cwd: &Path) -> Workspace {
        Workspace {
            cwd: cwd.to_path_buf(),
            variables: BTreeMap::new(),
            parent_env: BTreeMap::new(),
            env: Vec::new(),
            secrets: Vec::new(),
            _temp: None,
        }
    }

    #[test]
    fn resolve_write_path_allows_workspace_subdirectory() {
        // (C3) A normal snapshot path inside the workspace (including a
        // not-yet-created __snapshots__ subdir) must resolve to a path under the
        // workspace, so legitimate snapshot writes keep working.
        let dir = tempfile::tempdir().unwrap();
        let ws = workspace_at(dir.path());
        let resolved = ws
            .resolve_write_path("__snapshots__/x.snap")
            .expect("in-workspace path must be allowed");
        assert!(resolved.starts_with(dir.path()));
    }

    #[test]
    fn resolve_write_path_rejects_parent_traversal() {
        // (C3) A `..`-traversal write path must be rejected as a Scenario error
        // so a snapshot --update cannot write outside the workspace.
        let dir = tempfile::tempdir().unwrap();
        let ws = workspace_at(dir.path());
        let err = ws
            .resolve_write_path("../../../tmp/escape.snap")
            .expect_err("traversal must be rejected");
        assert_eq!(err.exit_code(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn resolve_write_path_rejects_symlink_escape() {
        // (C3) A symlinked subdirectory pointing outside the workspace must not
        // be a write escape hatch: resolving through the existing symlink lands
        // outside the root and is rejected.
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let link = dir.path().join("out");
        std::os::unix::fs::symlink(outside.path(), &link).unwrap();
        let ws = workspace_at(dir.path());
        let err = ws
            .resolve_write_path("out/escape.snap")
            .expect_err("symlink escape must be rejected");
        assert_eq!(err.exit_code(), 2);
    }
}
