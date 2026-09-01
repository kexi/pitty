# Security policy

## Supported versions

Security fixes are released for the latest patch release in the current major
line. Users should pin an exact patch version for reproducibility and upgrade
when a security release is published. Older patch and major releases do not
receive security backports.

| Version | Security updates |
| --- | --- |
| Latest `1.x` patch | Supported |
| Earlier releases | Not supported |

## Reporting a vulnerability

Do not open a public issue for a suspected vulnerability. Use GitHub's
[private vulnerability reporting](https://github.com/kexi/pitty/security/advisories/new)
to share the affected version, impact, reproduction steps or proof of concept,
and any suggested mitigation.

The maintainer aims to acknowledge a report within seven days and provide an
initial assessment within fourteen days. Resolution time depends on severity
and complexity. Reporters will be updated through the private advisory and
credited in the published advisory unless they prefer to remain anonymous.

## Security model

pitty treats scenario files and the commands they launch as trusted input. It
does not sandbox child processes; they inherit the pitty process's operating
system privileges. Secret masking reduces accidental disclosure in pitty's
reports and GitHub annotations, but it is not a substitute for CI secret
isolation or least-privilege credentials.

Release archives are published with SHA-256 checksums. The composite action
fails closed when a prebuilt archive cannot be verified and otherwise falls
back to a locked source build. GitHub Actions dependencies are pinned to commit
SHAs and checked in CI.
