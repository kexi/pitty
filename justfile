# pitty task runner. Run `just` (or `just help`) to list recipes.
#
# Recipes assume the dev shell (`nix develop` / direnv) so the pinned Rust
# toolchain and `just` itself are on PATH. The dogfood/ci recipes mirror
# .github/workflows/ci.yml so the gates can be reproduced locally.

# The debug binary the dogfood recipes drive.
pitty_bin := justfile_directory() / "target/debug/pitty"

# Show the available recipes (default).
help:
    @just --list

# --- Build & quality -------------------------------------------------------

# Compile the workspace.
build:
    cargo build

# Compile in release mode.
build-release:
    cargo build --release

# Run the unit + integration tests (excludes PTY-gated #[ignore] tests).
test:
    cargo test

# Type-check the Windows target from the dev shell.
check-windows:
    cargo check --target x86_64-pc-windows-msvc --all-targets

# Verify GitHub Actions SHA pins and version comments.
pinact-verify:
    pinact run -fix=false -verify

# Run the PTY-gated tests too (needs a usable PTY).
test-pty:
    cargo test -- --ignored

# Format check + clippy with warnings denied (the CI lint gate).
lint: pinact-verify
    cargo fmt --check
    cargo clippy --all-targets -- -D warnings

# Auto-format the code.
fmt:
    cargo fmt

# --- Running pitty ---------------------------------------------------------

# Run a scenario file or directory: `just run e2e/scenarios/positive`.
run path:
    cargo run -- run {{path}}

# Run a matrix scenario: `just matrix e2e/scenarios/samples/matrix-shell.yaml`.
matrix file:
    cargo run -- matrix {{file}}

# Bench a scenario N times: `just bench e2e/scenarios/samples/bench-shell.yaml 10`.
bench file runs="10":
    cargo run -- bench {{file}} --runs {{runs}}

# Scaffold pitty.yaml + scenarios/ in the current directory.
init:
    cargo run -- init

# --- Dogfood (pitty testing itself, mirrors CI) ----------------------------

# Build then run all three dogfood tiers (positive, meta, samples).
dogfood: build dogfood-positive dogfood-meta dogfood-samples

# Positive tier: run scenarios directly.
dogfood-positive: build
    PITTY_BIN="{{pitty_bin}}" INNER_DIR="{{justfile_directory()}}/e2e/scenarios/meta/inner" \
        "{{pitty_bin}}" run e2e/scenarios/positive

# Meta tier: pitty spawns an inner pitty on by-design failures (nested PTY).
dogfood-meta: build
    PITTY_BIN="{{pitty_bin}}" INNER_DIR="{{justfile_directory()}}/e2e/scenarios/meta/inner" \
        "{{pitty_bin}}" run e2e/scenarios/meta

# Samples tier: exercise the matrix/bench subcommands.
dogfood-samples: build
    "{{pitty_bin}}" matrix e2e/scenarios/samples/matrix-shell.yaml
    "{{pitty_bin}}" matrix e2e/scenarios/samples/matrix-multi-axis.yaml
    "{{pitty_bin}}" bench e2e/scenarios/samples/bench-shell.yaml --runs 3

# --- Examples --------------------------------------------------------------

# E2E the external `vibe` CLI with pitty. Set VIBE_BIN or it is auto-detected.
vibe-e2e vibe_bin=`command -v vibe || true`: build
    @test -n "{{vibe_bin}}" || { echo "vibe not found; set VIBE_BIN=/path/to/vibe" >&2; exit 1; }
    VIBE_BIN="{{vibe_bin}}" "{{pitty_bin}}" run examples/vibe/scenarios

# --- Aggregate -------------------------------------------------------------

# Reproduce the CI gates locally: lint, tests, Windows check, PTY tests, and dogfood tiers.
ci: lint test check-windows test-pty dogfood
