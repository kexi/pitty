//! Contract gate: `action.yml` must stay publishable to the GitHub Marketplace.
//!
//! Publishing an Action to the Marketplace has fixed metadata requirements that
//! GitHub enforces in its (manual) publish UI. Those requirements are easy to
//! break by editing `action.yml` — e.g. dropping `branding`, or setting a color
//! GitHub does not allow — and the breakage is only discovered when someone
//! tries to publish a release. This test pins the requirements so a regression
//! fails at `cargo test` time instead, in the same spirit as
//! `release_asset_name_contract.rs` and `schema_contract.rs`.
//!
//! Requirements checked (per the GitHub Marketplace publishing rules):
//! - `action.yml` lives at the repository root (this file reaches it via
//!   `../action.yml`, so a move breaks compilation — the strongest possible gate).
//! - `name:` is present (and is the unique Marketplace listing name).
//! - `description:` is present.
//! - `branding:` is present with both `icon:` and `color:`.
//! - `color:` is one of the eight colors GitHub allows.
//! - `icon:` is a single Feather icon name (Marketplace rejects unknown icons);
//!   we cannot vendor the whole Feather set, so we assert the value is a plain
//!   lowercase-kebab token and is not one of the icons GitHub explicitly blocks.
//!
//! What this does NOT do: it cannot press the "Publish this Action to the GitHub
//! Marketplace" button — that first publish requires accepting the Marketplace
//! agreement in the web UI and has no workflow/API switch. The first publish is
//! already done for this repository, so later published GitHub Releases are the
//! Marketplace update path. This gate guarantees those releases cannot fail on a
//! metadata regression.

/// The composite action source, embedded so the test reads exactly what ships.
const ACTION_YML: &str = include_str!("../action.yml");

/// The eight colors GitHub allows for `branding.color`.
const ALLOWED_COLORS: &[&str] = &[
    "white",
    "yellow",
    "blue",
    "green",
    "orange",
    "red",
    "purple",
    "gray-dark",
];

/// Icons GitHub explicitly disallows for `branding.icon` (its own brand marks
/// and a few reserved logos). A full Feather-set check is infeasible to vendor,
/// so we gate on the documented blocklist plus a shape check below.
const BLOCKED_ICONS: &[&str] = &[
    "coffee",
    "columns",
    "divide",
    "rotate-cw",
    "rotate-ccw",
    "code",
    "key",
    "trash",
    "trash-2",
    "github",
    "gitlab",
    "x",
    "slack",
    "twitter",
    "facebook",
    "instagram",
    "linkedin",
    "youtube",
];

/// Read the scalar value of a top-level `key:` line from `action.yml`.
///
/// Minimal on purpose: `action.yml`'s top-level metadata keys are simple
/// `key: value` scalars, so a full YAML parser is unnecessary (and would add a
/// dependency the project deliberately avoids). Returns the trimmed value.
fn top_level_scalar(key: &str) -> Option<String> {
    let prefix = format!("{key}:");
    ACTION_YML
        .lines()
        .find(|line| line.starts_with(&prefix))
        .map(|line| line[prefix.len()..].trim().to_string())
}

/// Read the value of a key nested one level (two-space indent) under `branding:`.
fn branding_scalar(key: &str) -> Option<String> {
    let mut in_branding = false;
    for line in ACTION_YML.lines() {
        if line.starts_with("branding:") {
            in_branding = true;
            continue;
        }
        if in_branding {
            // A new top-level key (no indent, non-empty) ends the branding block.
            if !line.is_empty() && !line.starts_with(' ') {
                break;
            }
            let needle = format!("  {key}:");
            if let Some(rest) = line.strip_prefix(&needle) {
                return Some(rest.trim().to_string());
            }
        }
    }
    None
}

/// Extract the shell body of the `Install pitty` step from `action.yml`.
///
/// The body is the block-scalar under that step's `run: |`, indented eight
/// spaces. Slicing it out lets the tests below execute the *real* installer
/// logic rather than a transcription of it, so the gate cannot drift from what
/// ships. The step is located by its `- name:` line so a reordering of the
/// steps does not silently pick up the wrong `run:` block.
///
/// Gated to unix alongside its only caller: slicing the body out is only useful
/// where the tests can actually execute it under bash, and an ungated helper
/// would be dead code on the Windows leg of CI.
#[cfg(unix)]
fn install_step_script() -> String {
    const INDENT: &str = "        ";
    let mut lines = ACTION_YML
        .lines()
        .skip_while(|l| l.trim() != "- name: Install pitty")
        .skip_while(|l| l.trim() != "run: |")
        .skip(1)
        .peekable();
    let mut body = String::new();
    for line in lines.by_ref() {
        // The block scalar ends at the first non-blank line that is not indented
        // at least as far as its content (the next step's `- name:`).
        if !line.trim().is_empty() && !line.starts_with(INDENT) {
            break;
        }
        body.push_str(line.strip_prefix(INDENT).unwrap_or(""));
        body.push('\n');
    }
    assert!(
        body.contains("PITTY_REF="),
        "could not slice the Install pitty script out of action.yml"
    );
    body
}

/// The directories the installer's own utilities are found in, taken from the
/// ambient `PATH` rather than assumed.
///
/// Why not the historical `/usr/bin:/bin`: inside the nix build sandbox neither
/// directory exists — every tool lives under `/nix/store/...` — so a hardcoded
/// list left the child with a `PATH` on which nothing resolved. Reading the
/// ambient `PATH` works in both worlds without naming a store path: on a normal
/// machine it yields `/usr/bin` and `/bin` anyway, and in the sandbox it yields
/// the store `bin` directories stdenv put there.
///
/// Entries are filtered to those that exist, and the relative/empty entries a
/// shell treats as "the current directory" are dropped: the fixture depends on
/// its own `bin` winning the `command -v pitty` lookup, and a `.` entry could
/// let an unrelated file in the test's cwd shadow it.
#[cfg(unix)]
fn ambient_path_dirs() -> Vec<std::path::PathBuf> {
    let Some(path) = std::env::var_os("PATH") else {
        return Vec::new();
    };
    std::env::split_paths(&path)
        .filter(|dir| dir.is_absolute() && dir.is_dir())
        .collect()
}

/// Locate an executable by scanning the ambient `PATH`, the way a shell would.
///
/// `Command::new("bash")` already resolves against the inherited `PATH`, but
/// resolving it ourselves lets the test report a precise skip reason when the
/// tool is genuinely absent instead of failing on an opaque `NotFound` from the
/// spawn.
#[cfg(unix)]
fn find_on_ambient_path(name: &str) -> Option<std::path::PathBuf> {
    ambient_path_dirs()
        .into_iter()
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// Run the real installer body with a fake `pitty` already on PATH, returning
/// its combined output.
///
/// The download and the cargo fallback are neutralized by pointing `PITTY_REPO`
/// at an unroutable URL and stubbing `curl`/`git`/`cargo` as failures, so the
/// script reaches its reuse-or-install decision and then stops. What the caller
/// reads back is which branch it took: the reuse message, or the download
/// attempt that proves the pin was honored.
///
/// Returns `None` only when no `bash` exists anywhere on the ambient `PATH`, so
/// the body simply cannot be executed; callers skip with a printed reason rather
/// than reporting a failure they cannot act on.
#[cfg(unix)]
fn run_installer_with_pitty_on_path(input_ref: &str) -> Option<String> {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    std::fs::create_dir(&bin).unwrap();

    // A `pitty` on PATH stands in for a self-hosted runner image or an earlier
    // install step. Stubs for the network tools make every install route fail
    // loudly and immediately instead of reaching out.
    for (name, body) in [
        ("pitty", "#!/bin/sh\nexit 0\n"),
        ("curl", "#!/bin/sh\nexit 1\n"),
        ("git", "#!/bin/sh\nexit 1\n"),
        (
            "cargo",
            "#!/bin/sh\necho 'cargo install attempted' >&2\nexit 1\n",
        ),
    ] {
        let path = bin.join(name);
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let script = dir.path().join("install.sh");
    std::fs::write(&script, install_step_script()).unwrap();

    let bash = find_on_ambient_path("bash")?;

    // The fixture `bin` stays FIRST: the property under test is that the script
    // sees a `pitty` (and the `curl`/`git`/`cargo` stubs) ahead of anything real
    // on the machine, so prepending it is what makes the test mean anything. The
    // ambient directories follow so the script's own utilities (`mkdir`, `grep`,
    // `awk`, ...) still resolve wherever they happen to live.
    let mut child_path = vec![bin.clone()];
    child_path.extend(ambient_path_dirs());
    let child_path = std::env::join_paths(child_path).expect("PATH entries must not contain ':'");

    let out = std::process::Command::new(&bash)
        .arg(&script)
        .env("PATH", &child_path)
        .env("PITTY_INPUT_REF", input_ref)
        .env("PITTY_ACTION_REF", "v1")
        .env("PITTY_REPO", "https://github.com/kexi/pitty")
        .env("RUNNER_OS", "Linux")
        .env("RUNNER_ARCH", "X64")
        .env("RUNNER_TEMP", dir.path())
        .env("GITHUB_PATH", dir.path().join("github_path"))
        .output()
        .expect("the bash located on PATH must be spawnable");
    Some(format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    ))
}

/// Skip-with-reason wrapper: `None` means the environment has no `bash` at all.
///
/// A macro rather than a function so the `return` lands in the calling test's
/// body. Skipping is the last resort and should never trigger in CI: both the
/// GitHub runners and the nix build sandbox provide bash. It exists so an
/// exotic environment reports *why* it could not check the contract instead of
/// failing on an opaque spawn error, as the hardcoded `/usr/bin:/bin` PATH did.
#[cfg(unix)]
macro_rules! installer_output_or_skip {
    ($input_ref:expr) => {
        match run_installer_with_pitty_on_path($input_ref) {
            Some(output) => output,
            None => {
                println!(
                    "SKIP {}: no `bash` found on PATH, so the installer body cannot be executed",
                    concat!(module_path!(), "::", line!())
                );
                return;
            }
        }
    };
}

/// (#42) An explicit `version:` pin is authoritative. What is guaranteed: with a
/// `pitty` already on PATH *and* `version:` set, the installer does not
/// short-circuit on the pre-existing binary — it goes on to install the pinned
/// ref, so the caller gets the ref they named rather than whatever the machine
/// happened to have. This is the reproducibility bug: before the fix the script
/// exited at the PATH check before `version:` was ever read.
#[cfg(unix)]
#[test]
fn an_explicit_version_pin_overrides_a_pitty_already_on_path() {
    let output = installer_output_or_skip!("v1.2.0");
    assert!(
        !output.contains("reusing it"),
        "a pinned version must not be satisfied by a pre-existing binary: {output:?}"
    );
    assert!(
        output.contains("pitty-v1.2.0-Linux-X64.tar.gz") || output.contains("cargo install"),
        "the pinned ref v1.2.0 must drive an install attempt: {output:?}"
    );
    assert!(
        output.contains("version: v1.2.0 was pinned"),
        "the caller must be told their pin overrode the binary on PATH: {output:?}"
    );
}

/// (#42) With no `version:` pinned, a pre-existing `pitty` is still reused
/// as-is. What is guaranteed: the fix narrows the short-circuit to the unpinned
/// case rather than removing it, so self-hosted runners and jobs that
/// pre-install pitty keep working without a download.
#[cfg(unix)]
#[test]
fn an_unpinned_run_reuses_a_pitty_already_on_path() {
    let output = installer_output_or_skip!("");
    assert!(
        output.contains("already on PATH") && output.contains("reusing it"),
        "an unpinned run must reuse the binary already on PATH: {output:?}"
    );
    assert!(
        !output.contains("cargo install"),
        "an unpinned run with pitty present must not install anything: {output:?}"
    );
}

#[test]
fn action_has_name_and_description() {
    let name = top_level_scalar("name").expect("action.yml must declare a top-level `name:`");
    assert!(
        !name.is_empty(),
        "Marketplace listing requires a non-empty `name:`"
    );
    let description =
        top_level_scalar("description").expect("action.yml must declare a `description:`");
    assert!(
        !description.is_empty(),
        "Marketplace listing requires a non-empty `description:`"
    );
}

#[test]
fn action_has_branding_block() {
    assert!(
        ACTION_YML.lines().any(|l| l.starts_with("branding:")),
        "Marketplace publishing requires a top-level `branding:` block"
    );
    assert!(
        branding_scalar("icon").is_some(),
        "Marketplace `branding:` requires an `icon:`"
    );
    assert!(
        branding_scalar("color").is_some(),
        "Marketplace `branding:` requires a `color:`"
    );
}

#[test]
fn branding_color_is_allowed() {
    let color = branding_scalar("color").expect("branding.color must be present");
    assert!(
        ALLOWED_COLORS.contains(&color.as_str()),
        "branding.color {color:?} is not a GitHub-allowed color; must be one of {ALLOWED_COLORS:?}"
    );
}

#[test]
fn branding_icon_is_shaped_like_a_feather_icon() {
    let icon = branding_scalar("icon").expect("branding.icon must be present");
    // A Feather icon name is a single lowercase-kebab token (letters/digits/`-`).
    assert!(
        !icon.is_empty()
            && icon
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
        "branding.icon {icon:?} must be a single lowercase-kebab Feather icon name"
    );
    assert!(
        !BLOCKED_ICONS.contains(&icon.as_str()),
        "branding.icon {icon:?} is on GitHub's disallowed-icon list; pick another Feather icon"
    );
}
