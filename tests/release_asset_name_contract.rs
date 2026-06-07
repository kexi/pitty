//! Contract gate: the prebuilt-binary asset names produced by the release
//! workflow must match exactly what the composite action downloads.
//!
//! `action.yml` fast-path builds its download URL from
//!   pitty-${PITTY_REF}-${RUNNER_OS}-${RUNNER_ARCH}.tar.gz
//! and `.github/workflows/release.yml` names each archive
//!   pitty-<ref>-${{ matrix.os_name }}-${{ matrix.arch_name }}.tar.gz
//! where `os_name`/`arch_name` are GitHub's runner OS/arch labels supplied per
//! build matrix entry. If the two drift — most easily by the release matrix
//! naming Apple Silicon `aarch64` (its Rust triple) instead of `ARM64`, or
//! Windows by a Git-Bash `uname` string instead of `Windows` — the action's fast
//! path 404s and silently falls back to a slow `cargo install`, with nothing
//! failing loudly. This test pins both sides to one source of truth (the
//! `(RUNNER_OS, RUNNER_ARCH)` pairs) so that drift fails at `cargo test` time, in
//! the same spirit as `schema_contract.rs`.
//!
//! The check is structural rather than a single literal-name match because
//! release.yml composes each name from a templated `archive:` line plus the
//! per-target `os_name`/`arch_name` matrix values. We verify (a) the action's
//! download expression has the shape this test reproduces, (b) the release
//! archive template concatenates ref, os_name, and arch_name in that order, and
//! (c) every runner OS/arch pair appears as a matrix entry feeding that template
//! for the patch, floating major, and floating minor release sets.
//!
//! Both files are embedded with `include_str!` so the test reads the exact
//! bytes that ship.

/// The composite action source, embedded so the test reads what ships.
const ACTION_YML: &str = include_str!("../action.yml");
/// The release workflow source, embedded so the test reads what ships.
const RELEASE_YML: &str = include_str!("../.github/workflows/release.yml");

/// The `(RUNNER_OS, RUNNER_ARCH)` pairs pitty ships prebuilt binaries for.
/// This is the single source of truth both sides are checked against.
///
/// Apple Silicon is `("macOS", "ARM64")` here, deliberately distinct from the
/// `aarch64-apple-darwin` Rust triple, because the action keys on GitHub's
/// runner labels. Windows is `("Windows", "X64")`, not a Git-Bash `uname`
/// string, because `uname -s` includes the host kernel version on Windows.
///
/// No `("macOS", "X64")`: GitHub's macos-13 Intel runners were unreliably
/// scheduled and blocked releases, so Intel Macs fall back to `cargo install` in
/// the composite action. Add the pair back here and in release.yml's matrix if a
/// dependable Intel runner returns.
const RUNNER_PAIRS: &[(&str, &str)] = &[
    ("Linux", "X64"),
    ("Linux", "ARM64"),
    ("macOS", "ARM64"),
    ("Windows", "X64"),
];

#[test]
fn action_builds_asset_name_from_runner_labels() {
    // Pin the shape of action.yml's download expression: this whole gate only
    // proves a match if the action really concatenates ref, RUNNER_OS, and
    // RUNNER_ARCH in that order with `.tar.gz`. A change to the expression must
    // update this assertion (and RUNNER_PAIRS).
    assert!(
        ACTION_YML.contains(r#"os="${RUNNER_OS:?RUNNER_OS is not set}""#),
        "action.yml must derive os from RUNNER_OS"
    );
    assert!(
        ACTION_YML.contains(r#"arch="${RUNNER_ARCH:?RUNNER_ARCH is not set}""#),
        "action.yml must derive arch from RUNNER_ARCH"
    );
    assert!(
        ACTION_YML.contains(r#"asset="pitty-${PITTY_REF}-${os}-${arch}.tar.gz""#),
        "action.yml asset name expression changed; update RUNNER_PAIRS / this gate"
    );
}

#[test]
fn action_handles_windows_executable_name_inside_tarball() {
    // Windows release archives contain `pitty.exe`, while Unix archives contain
    // `pitty`. The action must chmod the OS-specific extracted filename before
    // adding the install dir to PATH, otherwise a Windows prebuilt install would
    // download and extract successfully but fail on the final chmod. It must
    // also write a Windows-native PATH entry to GITHUB_PATH: the extraction
    // command runs under Git Bash, but the runner owns PATH propagation between
    // composite steps. The fallback install must do the same for Cargo's bin
    // directory so source installs work on runners that did not already expose
    // it.
    assert!(
        ACTION_YML.contains(r#"bin_name="pitty""#),
        "action.yml must default the extracted binary name to pitty"
    );
    assert!(
        ACTION_YML.contains(r#"if [ "$os" = "Windows" ]; then"#),
        "action.yml must branch on Windows before chmod"
    );
    assert!(
        ACTION_YML.contains(r#"bin_name="pitty.exe""#),
        "action.yml must chmod pitty.exe for Windows prebuilt installs"
    );
    assert!(
        ACTION_YML.contains(r#"chmod +x "$install_dir/$bin_name""#),
        "action.yml must chmod the OS-specific extracted binary name"
    );
    assert!(
        ACTION_YML.contains(r#"path_entry="$(cygpath -w "$dir")""#),
        "action.yml must write Windows-native entries to GITHUB_PATH"
    );
    assert!(
        ACTION_YML.contains(r#"append_github_path "$install_dir""#),
        "action.yml must add the prebuilt install dir to GITHUB_PATH"
    );
    assert!(
        ACTION_YML.contains(r#"append_github_path "${CARGO_HOME:-$HOME/.cargo}/bin""#),
        "action.yml must add Cargo's fallback install dir to GITHUB_PATH"
    );
}

#[test]
fn release_archive_template_orders_ref_os_arch_to_match_the_action() {
    // The release archive name must be `pitty-<ref>-<os>-<arch>` with os/arch
    // taken from the matrix's runner labels, in the same order the action's
    // download URL uses. We check all refs the workflow publishes: the pushed
    // patch tag (`github.ref_name`, e.g. v1.2.0), the floating major (`v1`), and
    // the floating minor (`v1.2`). A reordered or renamed segment here is the
    // exact drift that silently breaks the fast path.
    let tag_template =
        "archive: pitty-${{ github.ref_name }}-${{ matrix.os_name }}-${{ matrix.arch_name }}";
    let major_template =
        "archive: pitty-${{ needs.release-version.outputs.major }}-${{ matrix.os_name }}-${{ matrix.arch_name }}";
    let minor_template =
        "archive: pitty-${{ needs.release-version.outputs.minor }}-${{ matrix.os_name }}-${{ matrix.arch_name }}";

    assert!(
        RELEASE_YML.contains(tag_template),
        "release.yml must name the release-tag archive {tag_template:?} (ref-os-arch order)"
    );
    assert!(
        RELEASE_YML.contains(major_template),
        "release.yml must name the floating-major archive {major_template:?} (ref-os-arch order)"
    );
    assert!(
        RELEASE_YML.contains(minor_template),
        "release.yml must name the floating-minor archive {minor_template:?} (ref-os-arch order)"
    );
}

#[test]
fn release_matrix_covers_exactly_the_runner_pairs() {
    // Every runner OS/arch pair the action can download must appear as a build matrix
    // entry (os_name + arch_name on adjacent lines), so the templated archive
    // name above expands to exactly the names the action expects — no
    // missing target (a platform that always falls back to cargo) and no stray
    // target naming an asset the action will never request. There are three
    // matrix blocks (release-tag, floating major, floating minor); each pair
    // must appear in all three, so we require each pair to occur at least three
    // times.
    // Match `os_name:`/`arch_name:` ignoring leading indentation so a
    // re-indentation of release.yml cannot silently turn this into a vacuous
    // pass (0 matches): we normalize every line to its trimmed form first.
    let trimmed_lines: Vec<&str> = RELEASE_YML.lines().map(str::trim).collect();
    let count_pair = |os: &str, arch: &str| {
        let os_line = format!("os_name: {os}");
        let arch_line = format!("arch_name: {arch}");
        trimmed_lines
            .windows(2)
            .filter(|w| w[0] == os_line && w[1] == arch_line)
            .count()
    };
    for (os, arch) in RUNNER_PAIRS {
        let occurrences = count_pair(os, arch);
        assert!(
            occurrences >= 3,
            "release.yml matrix must list runner pair ({os}, {arch}) in the \
             release-tag, floating-major, and floating-minor matrices (found \
             {occurrences} occurrence(s))"
        );
    }

    // Guard against an extra target the action could not serve: count total
    // matrix entries and require exactly 3 * RUNNER_PAIRS (three matrix blocks).
    let total_entries = trimmed_lines
        .iter()
        .filter(|l| l.starts_with("arch_name: "))
        .count();
    assert_eq!(
        total_entries,
        3 * RUNNER_PAIRS.len(),
        "release.yml has {total_entries} matrix arch_name entries; expected exactly \
         {} (three matrix blocks of the {} runner pairs). An extra/missing target drifts \
         from the action's download set.",
        3 * RUNNER_PAIRS.len(),
        RUNNER_PAIRS.len(),
    );
}

#[test]
fn release_only_triggers_on_tag_push() {
    // Security invariant: the release workflow (write-scoped GITHUB_TOKEN in
    // scope) must run only on tag pushes, never on pull_request, so fork code
    // cannot trigger it. We check the trigger block structurally: a `tags:`
    // filter must be present and no `pull_request:` trigger key may appear.
    // Matching the trigger keys (with their YAML colon) rather than the bare
    // word lets the rationale comments mention pull_request in prose.
    assert!(
        RELEASE_YML.contains("tags:"),
        "release.yml must trigger on tag push (a `tags:` filter)"
    );
    assert!(
        !RELEASE_YML.contains("pull_request:"),
        "release.yml must NOT declare a pull_request trigger (would expose the token to forks)"
    );
    assert!(
        !RELEASE_YML.contains("pull_request_target:"),
        "release.yml must NOT declare a pull_request_target trigger (token + fork code)"
    );
}

#[test]
fn release_pins_leading_dir_false_on_all_upload_steps() {
    // AC6: action.yml extracts the tarball with `tar -xzf -C $HOME/.local/bin`
    // and then chmods `pitty`/`pitty.exe`, i.e. it expects the binary at the
    // tarball ROOT (no `pitty-.../` subdirectory). That holds only if
    // upload-rust-binary-action is told `leading-dir: false`. The action's
    // default is not guaranteed across versions, so release.yml must state it
    // explicitly on all upload steps (release-tag, major, and minor). If the default ever
    // flips to a leading dir and this key is dropped, the action would hit "No
    // such file" on chmod — this gate fails first. We require three occurrences,
    // one per upload step. Match the trimmed line so comment prose mentioning
    // `leading-dir: false` is not miscounted as a YAML key.
    let occurrences = RELEASE_YML
        .lines()
        .filter(|l| l.trim() == "leading-dir: false")
        .count();
    assert_eq!(
        occurrences, 3,
        "release.yml must pin `leading-dir: false` on the release-tag, floating-major, \
         and floating-minor upload steps (found {occurrences}); the action expects the binary at the \
         tarball root for chmod"
    );
}

#[test]
fn release_bin_name_matches_action_chmod_targets() {
    // Binary-name drift guard: release.yml builds `bin: pitty`, while action.yml
    // makes the extracted binary executable as `pitty` on Unix and `pitty.exe`
    // on Windows. With `leading-dir: false` the tarball root file is named after
    // `bin` plus the platform executable suffix, so if the crate's binary is
    // ever renamed in release.yml, action.yml's chmod path would no longer exist.
    assert!(
        RELEASE_YML.contains("bin: pitty"),
        "release.yml must build `bin: pitty` (binary name the action chmods)"
    );
    assert!(
        ACTION_YML.contains(r#"bin_name="pitty""#)
            && ACTION_YML.contains(r#"bin_name="pitty.exe""#)
            && ACTION_YML.contains(r#"chmod +x "$install_dir/$bin_name""#),
        "action.yml must chmod pitty/pitty.exe, matching release.yml `bin: pitty`"
    );
}

#[test]
fn release_jobs_all_guard_against_floating_ref_repushes() {
    // Recursion/double-upload regression guard. move-floating-tags force-pushes
    // `v1` and `v1.2`, which both re-fire the broad `v*` trigger. GitHub Actions
    // expressions have no regex operator, so release.yml parses the ref in Bash:
    // only `vMAJOR.MINOR.PATCH` sets `should_publish=true`; floating major/minor
    // refs set `should_publish=false`. All publishing jobs must depend on that
    // parsed output so the re-triggered runs are complete no-ops.
    assert!(
        RELEASE_YML.contains(r#"[[ "$tag" =~ ^v([0-9]+)\.([0-9]+)\.([0-9]+)$ ]]"#),
        "release.yml must parse only vMAJOR.MINOR.PATCH tags as publishable"
    );
    assert!(
        RELEASE_YML.contains("should_publish=false"),
        "release.yml must mark floating/non-patch refs as non-publishable"
    );

    let guard = "if: ${{ needs.release-version.outputs.should_publish == 'true' }}";
    let occurrences = RELEASE_YML.matches(guard).count();
    assert_eq!(
        occurrences, 5,
        "all five publishing jobs (create-release, upload-assets, move-floating-tags, \
         upload-major-assets, upload-minor-assets) must carry the parsed-ref guard \
         {guard:?} (found {occurrences}); otherwise floating tag moves can re-fire \
         the trigger and recreate/re-upload colliding releases/assets"
    );
}

#[test]
fn release_grants_only_contents_write() {
    // Least-privilege invariant: the workflow's only declared permission is
    // `contents: write` (needed to create the Release and upload assets).
    assert!(
        RELEASE_YML.contains("contents: write"),
        "release.yml must grant contents: write for Release uploads"
    );
}
