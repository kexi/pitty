# ptytest dogfood scenarios

`ptytest` tests itself by running `ptytest run` over the scenarios in this
directory. This is the end-to-end surface that used to live in `tests/e2e.rs`.

## Tiers

- **`scenarios/positive/`** — direct runs. Each scenario spawns a real shell in
  a PTY and asserts on its output/exit. A passing tier means `ptytest run
  e2e/scenarios/positive` exits `0`.
- **`scenarios/meta/`** — nested PTY. Each `verify-*.yaml` spawns an *inner*
  `ptytest` on a scenario under `scenarios/meta/inner/` that is designed to
  **fail**, then asserts the inner process's exit code (and, for the secret
  case, that the inner JSON report masks the secret). The outer run succeeds
  precisely because the inner run failed as expected.

  `scenarios/meta/inner/` holds the inner scenarios; they are not meant to be
  run on their own as part of the dogfood pass (they fail by design).

## Required environment variables

The meta tier parameterizes the inner binary path and scenario directory
through `${var}` expansion. These names are resolved from the **parent process
environment** (the expansion fallback in `src/workspace.rs`), so the caller
must export them before `ptytest run`:

| Variable      | Meaning                                                        |
|---------------|----------------------------------------------------------------|
| `PTYTEST_BIN` | Absolute path to the `ptytest` binary the meta tier spawns.    |
| `INNER_DIR`   | Absolute path to `e2e/scenarios/meta/inner`.                   |

> Note: values resolved via the parent-environment fallback are **not** masked
> in logs or reports (only `variables` with `secret: true` are). These two names
> are non-secret paths, so this is fine here — but never pass a secret value
> this way; declare it as a `secret: true` variable instead.

Why the parent env rather than scenario `variables`: variables are baked into
the YAML and cannot carry a caller-supplied absolute path, and scenario `env`
is injected into the spawned child rather than consulted by `${var}` expansion
of `spawn.command`.

## Running locally

Build first, then run each tier (real PTY required, so use the dev shell):

```sh
nix develop --command cargo build

export PTYTEST_BIN="$PWD/target/debug/ptytest"
export INNER_DIR="$PWD/e2e/scenarios/meta/inner"

nix develop --command "$PTYTEST_BIN" run e2e/scenarios/positive
nix develop --command "$PTYTEST_BIN" run e2e/scenarios/meta
```

Both commands must exit `0`.

## Notes

- The meta tier deliberately does **not** use `workspace.temp`: it runs in a
  fixed cwd so its `logs/` output is predictable and it does not depend on temp
  at all. Generated `logs/` directories are gitignored (`e2e/**/logs/`).
- The residual negative cases in `tests/e2e.rs` (`#[ignore]`) overlap with the
  meta tier and are kept only as a fallback. **Decommission condition:** once
  the meta tier has run green on **both** the `pty-e2e` (ubuntu) and
  `pty-e2e-macos` (macOS) gating jobs (job keys in
  [`.github/workflows/ci.yml`](../.github/workflows/ci.yml)) for **5 consecutive**
  CI runs on the default branch, delete `tests/e2e.rs` (see the `TODO(decommission)`
  marker there). Until then the duplication is intentional.
- Intermediate consolidation (v0.4): the redundant secret-masking PTY test was
  removed from `tests/e2e.rs`. The stdout-report masking path is covered without
  a PTY by the white-box unit test
  `runner::mask_report_redacts_secrets_in_assertion_messages_and_name`, and the
  real PTY path by `meta/verify-secret-masked.yaml`. As a result the secret
  string `supersecretvalue` now lives in only **two** places —
  `meta/inner/secret-leak.yaml` and `meta/verify-secret-masked.yaml` — and must
  be changed in both together (no shared constant spans Rust and YAML).
- The meta tier asserts inner exit codes via `expect_exit`'s **deadline form**
  (`expect_exit: {code: N, timeout: ...}`), which polls for the inner process's
  exit up to the timeout instead of relying on a preceding fixed `wait`. This
  removes the race where a slow PTY teardown (notably on macOS) outlasts a fixed
  wait. `verify-empty-spawn-errors.yaml` additionally asserts the inner error
  message `empty spawn command` so a missing `PTYTEST_BIN`/`INNER_DIR` cannot
  produce a false PASS (a literal unresolved `${PTYTEST_BIN}` would itself exit
  3 without printing that message).
