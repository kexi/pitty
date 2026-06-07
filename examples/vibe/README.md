# Example: testing an external CLI (`vibe`) with pitty

These scenarios demonstrate using pitty to E2E-test a **third-party** CLI —
[`vibe`](https://github.com/kexi/vibe), a Git worktree manager — rather than
pitty itself. They double as a worked example of the patterns in the main README.

## Running

The scenarios resolve the `vibe` binary from the `VIBE_BIN` environment variable
(via pitty's parent-env `${var}` fallback), so they are portable across machines:

```sh
export VIBE_BIN="$(command -v vibe)"   # path to a real vibe binary
pitty run examples/vibe/scenarios
# or:  just vibe-e2e
```

> `vibe` is normally a shell function wrapper (`eval "$(command vibe …)"`) so it
> can `cd` into worktrees. Point `VIBE_BIN` at the underlying **binary**
> (`command -v vibe` inside a non-wrapped shell, or the absolute path), not the
> function.

## Scenarios

| Scenario | What it checks |
| --- | --- |
| `version` | `vibe --version` prints the banner and exits 0 |
| `help` | `vibe --help` lists subcommands and exits 0 |
| `config` | `vibe config` shows the settings file and exits 0 |
| `start-dry-run` | inside a throwaway git repo, `vibe start --dry-run` prints the plan and creates **no** branch (exit 0) |
| `unknown-command` | an unknown subcommand prints `Unknown command` and exits 1 (negative path) |

`start-dry-run` runs in a fresh `0700` temp workspace (`workspace.temp: true`),
so it never touches a real repository.
