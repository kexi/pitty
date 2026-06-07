# Changelog

All notable changes to ptytest are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/). See
[`COMPATIBILITY.md`](COMPATIBILITY.md) for what the version number guarantees
about the scenario format and the report JSON.

The pre-1.0 entries below are development milestones, not separately tagged
releases; only 1.0.0 carries a release date.

## [1.1.0] - 2026-06-07

### Added

- **Prebuilt-binary release automation.** A tag-push-triggered workflow
  ([`.github/workflows/release.yml`](.github/workflows/release.yml)) builds
  `ptytest` for four `OS × arch` targets (Linux x86_64/aarch64, macOS
  x86_64/arm64) and uploads each as `ptytest-<ref>-<os>-<arch>.tar.gz` with a
  `.sha256` checksum to the GitHub Release. The os/arch in the asset name use
  the raw `uname -s`/`uname -m` values the composite action keys on, so the
  action's fast path now finds a prebuilt binary instead of always building from
  source. A contract test
  ([`tests/release_asset_name_contract.rs`](tests/release_asset_name_contract.rs))
  pins the release asset names to what `action.yml` downloads.
- **Floating `v1` major tag.** Each `v1.x.y` tag push force-moves the `v1` tag to
  the release commit and publishes a parallel set of `ptytest-v1-<os>-<arch>.tar.gz`
  assets, so `uses: kexi/ptytest@v1` both resolves the action ref and gets a
  prebuilt binary. The composite action's default `version` input is now `v1`.

## [1.0.0] - 2026-06-06

First stable release. The scenario input format and the report JSON are now
contracts (see [`COMPATIBILITY.md`](COMPATIBILITY.md) and [`SCHEMA.md`](SCHEMA.md)).

### Added

- **Stable scenario format.** The YAML scenario format is specified in
  [`SCHEMA.md`](SCHEMA.md), with a hand-written JSON Schema at
  [`schema/ptytest-scenario-v1.json`](schema/ptytest-scenario-v1.json) for
  editor validation/autocompletion.
- **`version` field.** Scenarios may declare `version: 1` (the default when
  omitted). A scenario declaring an unsupported version is rejected with a
  Scenario error (exit code 2) instead of being mis-parsed.
- **GitHub Actions integration.**
  - `run`, `matrix`, and `bench` accept `--github`, and the output also turns on
    automatically when `GITHUB_ACTIONS=true`.
  - Writes a Markdown **step summary** to `$GITHUB_STEP_SUMMARY` (assertion
    table for `run`, PASS/FAIL table for `matrix`, metrics for `bench`).
  - Emits `::error` **annotations** for failed assertions/cells and a
    `::warning` for a flaky `bench`. Step summaries and annotations are always
    secret-masked.
  - A composite action ([`action.yml`](action.yml)) lets workflows use
    `uses: kexi/ptytest@v1`.

### Changed

- **Top-level `deny_unknown_fields`.** An unknown top-level scenario key (e.g. a
  `stesp:` typo) is now a Scenario error rather than silently ignored. Nested
  step/spec fields remain lenient for forward compatibility. This is technically
  stricter, but no documented scenario used keys outside the seven specified in
  [`SCHEMA.md`](SCHEMA.md).

## [0.4.0]

### Changed

- **Multi-axis matrix.** `matrix` accepts multiple axes; cells are the Cartesian
  product of all axes (previously a single axis). The `MatrixReport` shape
  changed to carry `axes` and per-cell `coords`.
- **Two-valued `Status`.** A run report's `status` is now `passed`/`failed`
  only; hard faults are carried as exit codes (2/3), not a report status.

### Added

- **`expect_json` JSONPath-style access.** Dotted paths with array indices, with
  `equals` (typed), `contains`, and `exists` checks, reading from output or a
  file.

## [0.3.0]

### Added

- **`matrix`** command: run one scenario across a list of values.
- **`bench`** command: repeat a scenario to measure duration statistics and
  detect flakiness.

## [0.2.0]

### Added

- Assertion steps beyond basic `expect`: `expect_regex`, `expect_not`,
  `expect_file_*`, `expect_exit`, `expect_running`, `expect_snapshot`, and
  `expect_semantic`.

## [0.1.0]

### Added

- Initial release: PTY-based execution of YAML scenarios with `spawn`, `send`,
  `send_raw`, `key`, `wait`, and `expect`; `init`/`run`/`list` commands; secret
  masking; `0700` temp workspaces and `0600` logs; single-trust model.
