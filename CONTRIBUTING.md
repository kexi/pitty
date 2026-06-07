# Contributing to pitty

Contributor- and maintainer-facing docs for working **on** pitty. If you just
want to *use* pitty to test your CLI, the [README](README.md) is what you
want.

## Dev environment

pitty is a Rust project built through a Nix dev shell (flake-utils +
rust-overlay), so everyone gets the same pinned toolchain.

```sh
direnv allow        # or: nix develop
cargo build
cargo test
```

The dev shell also provides a [`just`](https://github.com/casey/just) task
runner. Run `just` (no args) to list recipes:

| Recipe | What it does |
| --- | --- |
| `just build` / `just build-release` | compile (debug / release) |
| `just test` / `just test-pty` | tests (excluding / including the PTY-gated `#[ignore]` tests) |
| `just lint` / `just fmt` | `cargo fmt --check` + `clippy -D warnings` / auto-format |
| `just run <path>` / `just matrix <file>` / `just bench <file> [runs]` | drive pitty |
| `just dogfood` | run all three dogfood tiers (see below) |
| `just vibe-e2e [vibe_bin]` | E2E the external `vibe` CLI (see [`examples/vibe`](examples/vibe)) |
| `just ci` | reproduce the full CI gate set locally |

The dev shell installs the [lefthook](https://github.com/evilmartians/lefthook)
git hooks automatically (via `shellHook`), so the gitleaks pre-commit tripwire
is wired up the first time you enter the shell.

## Concurrency model

pitty is synchronous on the surface. Internally, one dedicated reader thread
drains the (blocking) PTY master into a shared buffer and notifies a `Condvar`;
`expect` waits on that condvar so a match is observed the instant output
arrives, with a bounded timeout. We avoid an async runtime because
`portable-pty`'s reader/writer are blocking — wrapping them in `tokio` would
require blocking tasks anyway.

## Dogfooding

pitty tests itself by running `pitty run` over the scenarios in `e2e/`:

- a **`positive/`** tier runs scenarios directly;
- a nested-PTY **`meta/`** tier spawns an inner `pitty` on scenarios that fail
  by design and asserts their exit codes (and secret masking);
- a **`samples/`** tier exercises the `matrix` and `bench` subcommands.

See [`e2e/README.md`](e2e/README.md) for the tiers and the
`PITTY_BIN` / `INNER_DIR` environment-variable convention.

```sh
nix develop --command cargo build
export PITTY_BIN="$PWD/target/debug/pitty"
export INNER_DIR="$PWD/e2e/scenarios/meta/inner"
nix develop --command "$PITTY_BIN" run e2e/scenarios/positive   # must exit 0
nix develop --command "$PITTY_BIN" run e2e/scenarios/meta       # must exit 0
```

CI runs these tiers plus the residual `#[ignore]` PTY tests on **both Linux and
macOS as required gates** via `nix develop`, so a macOS PTY regression blocks
merges. The meta tier asserts inner exit via `expect_exit`'s deadline form so the
macOS gate stays non-flaky despite slower PTY teardown. See
[`.github/workflows/ci.yml`](.github/workflows/ci.yml). Locally, `just dogfood`
runs all three tiers and `just ci` reproduces the full gate set.

## Security scanning

Two scanners guard the repository:

- **Secret scanning (gitleaks).** A pre-commit hook (via lefthook, installed
  automatically by the nix dev shell) runs `gitleaks git --staged` on every
  commit, so a secret is caught before it lands. The authoritative gate is the
  `gitleaks` job in [`.github/workflows/ci.yml`](.github/workflows/ci.yml),
  which scans full history on push (and the PR range on pull requests) — the hook
  is bypassable with `git commit --no-verify`, so CI is the backstop. Both read
  the in-repo [`.gitleaks.toml`](.gitleaks.toml), which extends gitleaks'
  default ruleset and deliberately ships no broad allowlist.
- **Supply-chain scanning (Socket).** A separate [Socket](https://socket.dev)
  workflow ([`.github/workflows/socket.yml`](.github/workflows/socket.yml))
  scans the dependency manifest (`Cargo.toml`) for malicious or risky crates. It
  needs a `SOCKET_SECURITY_API_KEY` repository (or org) Actions secret; without
  it the scan step is skipped (so pull requests from forks, which cannot see the
  secret, do not fail). Its actions are pinned to commit SHAs because this is a
  workflow where a secret is in scope.

## Releasing

Cutting a release is automated by
[`.github/workflows/release.yml`](.github/workflows/release.yml): pushing a
`v1.x.y` tag creates the GitHub Release, builds the three prebuilt binaries
(Linux x86_64/aarch64, macOS arm64), uploads them with checksums, force-moves the
floating `v1` tag to the release commit, and publishes a parallel `v1`-named
asset set. The full step-by-step checklist (including the post-push verification
and the one-time GitHub Marketplace publish) lives in
[`COMPATIBILITY.md`](COMPATIBILITY.md).

## Nix packaging

The flake exposes a source-built package and app:

```sh
nix build .#pitty
nix run .#pitty -- --help
nix flake check
```

The package expression lives in [`nix/package.nix`](nix/package.nix) and uses
nixpkgs' standard `rustPlatform.buildRustPackage`, not the dev shell's
`rust-overlay` toolchain. Keep it that way so the expression stays close to
what nixpkgs expects for an official package.

`Cargo.lock` is intentionally tracked. `pitty` is an application, and the lock
file keeps Nix source builds reproducible. When dependencies change, rebuild the
Nix package and update `cargoHash` in `nix/package.nix` if Nix reports a new
vendor hash.

For a future nixpkgs submission, copy the shape of `nix/package.nix`, set
`version` explicitly, replace the local `src` default with `fetchFromGitHub`,
set the release `hash`, keep the reported `cargoHash`, and add the nixpkgs
maintainer entry.

## Further reading

- [`SCHEMA.md`](SCHEMA.md) — the full scenario-format specification.
- [`COMPATIBILITY.md`](COMPATIBILITY.md) — SemVer policy + release checklist.
- [`CHANGELOG.md`](CHANGELOG.md) — version history.
- [`e2e/README.md`](e2e/README.md) — the dogfood test tiers.
