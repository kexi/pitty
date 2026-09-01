# Support policy

pitty is production ready for testing command-line programs through a local
pseudo-terminal. This document defines the boundary of that claim so users can
decide whether a workload is supported before depending on it in CI.

## Supported use

- Running declarative scenario files through `pitty run`, `pitty matrix`, and
  `pitty bench` on Linux, macOS, and Windows.
- Installing through the checksum-verified GitHub Release archives, the
  `kexi/pitty` composite action, or the Nix flake.
- Consuming the versioned scenario format and report JSON according to
  [`COMPATIBILITY.md`](COMPATIBILITY.md).
- Reporting defects through
  [GitHub Issues](https://github.com/kexi/pitty/issues). Include the pitty
  version, operating system and architecture, the smallest reproducible
  scenario, expected behavior, and the actual report or exit code. Remove
  secrets before posting.

The latest patch release in the current major line receives bug and security
fixes. Reproduce a defect on that release before reporting it. Older releases
remain downloadable for reproducibility but do not receive backports.

## Supported platforms

Release CI runs the full PTY test and dogfood suites on Linux, macOS, and
Windows (through ConPTY). Releases provide prebuilt binaries for:

- Linux X64 and ARM64
- macOS X64 and ARM64
- Windows X64

Other architectures may work through a source build but are not release-gated
and are not part of the production-ready support claim.

## Operational boundaries

- Each scenario is trusted code. A scenario can launch commands with the same
  permissions as pitty; do not run untrusted scenario files.
- pitty is a test runner, not a process supervisor or sandbox. Use the CI
  platform's timeout, isolation, and resource controls around it.
- Parent-environment variables are not automatically secret-masked. Declare
  sensitive values as scenario variables with `secret: true`.
- Human-readable output and internal Rust library APIs are not stable
  contracts. Automations should consume report JSON and process exit codes.
- Availability SLAs, paid support, and long-term release branches are not
  currently offered.

Security-sensitive reports must follow [`SECURITY.md`](SECURITY.md) rather than
being filed in a public issue.
