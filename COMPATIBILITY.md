# Compatibility policy

pitty follows [Semantic Versioning](https://semver.org/). As of `1.0.0` it
exposes two separate, independently reasoned contracts. The crate version
covers both, but each evolves under its own rules below.

## 1. Scenario input format (the stability target)

The YAML scenario format specified in [`SCHEMA.md`](SCHEMA.md) is the primary
thing v1.0 stabilizes. Its guarantee:

- **A scenario valid under `1.0.0` is valid under every `1.x` release.**
- Within `1.x`, pitty may only make **additive** changes:
  - add a new step kind,
  - add a new **optional** field to an existing step or spec,
  - accept a new key name or `source` form.
- Within `1.x`, pitty will **not**:
  - remove a step or field,
  - change the meaning or default of an existing field,
  - make an existing optional field required,
  - tighten validation so a previously valid scenario becomes an error.

Such breaking changes are reserved for a future `2.0`.

### The `version` field

A scenario may declare `version: <n>` (default `1`). This build supports
version `1` only and rejects any other version with a Scenario error (exit
code 2) rather than guessing. The field exists so that a `2.0` pitty can
distinguish v1 and v2 scenarios, and so a v1 pitty fails clearly (telling the
user to upgrade) instead of silently mis-parsing a v2 scenario.

### Forward compatibility

`deny_unknown_fields` is enforced **only at the top level** of a scenario, to
catch fatal typos like `stesp:`. It is intentionally **not** enforced on the
nested step/spec types, so a scenario authored for a newer `1.x` (using an
additive optional field an older pitty does not know) still parses on the
older pitty — the unknown field is ignored, not rejected. This is what makes
the "additive only within 1.x" promise safe in both directions.

## 2. Report output JSON (a separate contract)

The machine-readable JSON emitted by `pitty run` (`Report`),
`pitty matrix --json` (`MatrixReport`), and `pitty bench --json`
(`BenchReport`) is a distinct contract from the input format:

- **Adding** a field to a report is a **minor** change.
- **Removing** a field, **changing its type**, or **changing its meaning** is a
  **major** change.
- **Consumers must ignore unknown fields.** Following the Robustness Principle,
  a tool reading pitty's JSON should tolerate fields it does not recognize, so
  that an additive (minor) change to a report never breaks it.

The human-readable renderings (`to_table`, `to_summary`, and the GitHub Actions
Markdown summaries) are **not** part of the JSON contract and may change freely;
parse the JSON, not the tables.

## Release checklist (creating a `v1.x` tag)

As of v1.1, cutting a release is **automated** by
[`.github/workflows/release.yml`](.github/workflows/release.yml): pushing a
`v1.x.y` tag builds the three prebuilt binaries (Linux x86_64/aarch64, macOS
arm64), uploads them with checksums, force-moves the floating `v1` tag to the
release commit, and publishes a parallel `v1`-named asset set. The composite
action's `version` input therefore defaults to `v1`.

The first release (v1.1.0) has run, so the `v1` tag and assets exist and
`uses: kexi/pitty@v1` resolves. (Historically, before that first release, `@v1`
could not resolve and `@main`/a SHA was needed; that bootstrap window is closed.)

When cutting a release:

- [ ] Bump the crate `version` in `Cargo.toml`, refresh `Cargo.lock`, and add a
      `CHANGELOG.md` entry.
- [ ] Push the release tag (e.g. `v1.1.0`). The release workflow then, on its
      own: creates the GitHub Release, builds the three `OS × arch` binaries,
      uploads `pitty-<tag>-<os>-<arch>.tar.gz` (+ `.sha256`) to the release,
      force-moves the `v1` tag to the release commit, and publishes the
      `pitty-v1-<os>-<arch>.tar.gz` asset set to the `v1` release.
- [ ] Verify the run is green and the six assets (three per ref: Linux
      x86_64/aarch64, macOS arm64) are attached. The asset names are
      machine-checked against `action.yml` by
      `tests/release_asset_name_contract.rs`, but a real run also confirms the
      uploads and the tag move succeeded.
- [ ] Post-push checks that the contract tests cannot cover statically (verify
      on the actual run/assets):
  - [ ] `tar tzf` an uploaded asset shows the `pitty` binary at the tarball
        **root** (no `pitty-.../` leading dir), so `action.yml`'s
        `chmod +x "$HOME/.local/bin/pitty"` resolves.
  - [ ] The Apple Silicon asset is named `...-Darwin-arm64.tar.gz` (raw
        `uname -m`), not `...-aarch64-...`.
  - [ ] On the 2nd `v*` push (the `v1` force-move), all four jobs skip
        (dotless `v1` fails the `contains('.')` guard) — no duplicate upload.
  - [ ] A consumer run's "Install pitty" log shows the prebuilt fast path
        (`Installing prebuilt pitty from ...` + `Verified sha256 of ...`),
        not the `cargo install` fallback.

### Publishing the Action to the GitHub Marketplace

The composite action's Marketplace **metadata** (`name`, `description`,
`branding.icon`/`color`) is machine-checked on every CI run by
[`tests/marketplace_action_contract.rs`](tests/marketplace_action_contract.rs),
so the listing can never drift out of a publishable shape. The listing `name` is
`pitty-action`: Marketplace names must be globally unique and the bare `pitty`
collides with the github.com/pitty user. This is only the listing name — the
repo and `uses: kexi/pitty@v1` are unaffected.

The **publish step itself cannot be automated** — GitHub exposes no API for it
and requires accepting the Marketplace Developer Agreement in the web UI. It is
a **one-time manual action**:

- [ ] After the first release exists, open the release on GitHub and tick
      **"Publish this Action to the GitHub Marketplace"**, accept the agreement,
      and pick a category. (Requires the metadata above, which CI already
      guarantees.)
- [ ] Once published, **every later release updates the Marketplace listing
      automatically** — no further manual step per release.

## What is explicitly out of scope for stability

- Exit code numbers (0/1/2/3) are stable, but the exact wording of error and
  assertion **messages** is not — match on exit codes, not message text.
- Log file format under `logs/` is diagnostic and not contracted.
- Internal Rust APIs (the library crate) are not covered by this policy; only
  the scenario format and report JSON are.
