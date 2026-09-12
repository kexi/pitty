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

### Worked example: a fix that had to be scoped, not reverted

`expect_json`'s `equals: null` is the case to reason from when a fix is
*obviously* right and still breaks the contract. Before 1.3, an explicitly
written `equals: null` was discarded while deserializing: paired with another
check it silently ran that other check and threw the null assertion away — a
false-green. Making it a real check was correct, but it also made two previously
*executing* documents (`equals: null` with `contains`, and with `exists`) trip
the one-of guard and become Scenario errors.

That is the forbidden transition, and the tempting argument — "the old behaviour
verified nothing, so nobody could depend on it" — is not one of the exceptions
above. Dependence is not the test; **execution** is. A pipeline can branch on
exit 1 (assertion failed) versus exit 2 (scenario error) without caring what the
assertion meant. The argument is also unfalsifiable: every tightening can be
described as fixing something nobody should have relied on.

The resolution was to scope the fix rather than abandon it. `equals: null` alone
now asserts null — 1.2.2 rejected that, so it is a pure loosening. The two shapes
1.2.2 executed keep executing, with the null dropped as before. Every shape 1.2.2
rejected stays rejected. The false-green is gone for new scenarios without any
existing one changing its exit code.

### Worked example: improving a rule without changing it

`spawn.split` shows how a genuinely better behavior ships under these rules
without breaking anyone, and is the case to reason from next time — including
the part that is *not* clean.

`spawn` tokenizes its command line with a plain whitespace split, which is a
footgun: quotes are not grouping syntax, so `spawn: "echo 'hello world'"` makes
the child print the quote characters and `spawn: "sh -c 'exit 3'"` runs the
program `'exit`. POSIX shell word rules are what authors expect and what the
format should arguably have specified from the start.

Applying them unconditionally would nonetheless have been a **breaking change**
under the rules above — it changes the meaning of an existing field, and it
turns a previously valid command line (an unterminated quote) into an error.
The breakage is concrete, not hypothetical: a scenario using
`spawn: "echo 'hello world'"` whose `expect_snapshot` was recorded on 1.2.2 has
the literal bytes `'hello world'` on disk, and an unconditional switch fails
that scenario on a patch-level upgrade.

So the better rule shipped as an **optional field** instead —
`spawn: {command: ..., split: posix}` — with the historical behavior as the
default. Authors opt in per `spawn`; nothing already written changes meaning.
Making `posix` the default is reserved for `2.0`.

#### The new field must itself be lenient

The first attempt at this field got a *second* compatibility break from the fix
for the first one, so the trap is worth naming.

`split` was initially strict about its value: `split: pisox` was a parse error,
by analogy with `key` and `source`, which reject unknown values. That analogy is
wrong. `key` and `source` existed in `1.0`, so an unknown value for them is
already an error on every `1.x` runner, and a new runner rejecting it agrees
with the old one. `split` did **not** exist, so an older runner accepts
`split: <anything>` and ignores it (nested fields are lenient — see *Forward
compatibility* above). Rejecting it in a newer runner therefore tightens
validation so a previously valid scenario becomes an error, which is the very
clause this field was added to respect.

An unrecognized `split` value is consequently **not** an error: it falls back to
the default rule and warns on stderr. The general rule:

> A field added within `1.x` may not reject any value **or any type**, because
> every runner that predates the field accepts all of them. Only a field that
> shipped in `1.0` can validate its value set.

The type half is easy to miss and was missed here once: making `split` a
string-typed field still rejected `split: 42`, and because the `spawn` wire form
is an untagged enum, that type mismatch rejected the *whole* `spawn` map with a
message that never mentioned `split`. A field added within `1.x` must therefore
deserialize from an arbitrary value and interpret it afterwards, not constrain
its type in the deserializer. The same applies to the published JSON schema: its
entry for such a field carries no `type`, `enum` or `pattern`, only `examples`.

#### What this does *not* buy you

A scenario using a newer `1.x` field still parses on an older runner — that is
the forward-compatibility promise — but it **does not mean the same thing**
there. `split: posix` on a pitty predating the field is silently ignored and the
command is whitespace-split.

This is a real limitation of additive evolution and not something the field can
fix: an old binary cannot be taught to refuse a document it was built to accept.
The `version` field cannot be pressed into service either — it is an integer
pinned to `1`, and any value an old runner refuses (`2`, or a non-integer) is
itself the breaking change, so there is no way for a v1 scenario to declare a
minimum feature level.

In practice the failure is loud rather than silent, because the assertions that
motivate `split: posix` are exactly the ones that break without it: on 1.2.2,
`sh -c 'exit 3'` reports `expected exit code 3, got 2`, and a snapshot recorded
under `posix` mismatches with `-hello world +'hello world'`. The scenario fails;
it does not pass with the wrong meaning. Authors who need a hard guarantee
should pin a minimum pitty version in CI rather than rely on the scenario file
to enforce it.

The generalizable rules: when an existing behavior turns out to be wrong, within
`1.x` the fix is a new optional field that selects the new behavior, not a
redefinition of the old one — however obviously correct the new behavior is. The
new field must accept every value, since older runners do. And "still parses on
an older pitty" is not "still means the same thing"; say which one you are
promising.

### Accepted break: `expect_json` extraction and quoted log noise

The rules above have one deliberate exception, recorded here because it is a
break rather than an example of avoiding one.

Through `1.2.2`, locating the trailing JSON block counted `"` characters from the
start of the captured output and treated a `{` at odd parity as string data. That
rule is not merely imprecise, it is unsound on terminal output, which has no
obligation to balance its quotes: a single stray quote anywhere earlier —
including an ordinary JSON-escaped log line such as `msg: \"hi\"` — made pitty
either fail to find a valid report at all, or silently assert against an **older**
block further up the buffer. Both were filed as bugs.

Extraction now derives string state from a structural opening brace outward, so
the surrounding text's quote parity cannot affect which block is chosen. The two
behaviors are driven by the same signal and cannot both be had: the input that
motivates keeping the old rule (a JSON-looking substring inside quoted prose) and
the inputs that motivate replacing it are indistinguishable by quote parity.

**What changes for an existing scenario:** if a run produces *no* real JSON block
at all, and its output contains a JSON-looking substring inside quoted prose, an
`expect_json` step that previously failed now passes.

A JSON-looking run inside quoted log prose is ranked below any block outside it,
so it cannot displace a genuine report — including when the surrounding log line
contains backslash-escaped quotes, which are honoured as escapes exactly as
`1.2.2` honours them. An unquoted later block still wins on tail position, as it
did before.

The verdict can still differ from `1.2.2` when a quoted run is left
**unterminated at end of line**: `1.2.2` masks the remainder of the buffer from
that quote onward, while extraction re-derives quoting per line. That is the same
deliberate break — the whole-buffer parity it removes is exactly what previously
lost real reports and returned stale ones.

This was taken deliberately in preference to preserving a rule that loses or
misidentifies real reports. It is noted here so the divergence is discoverable
from the contract rather than only from the changelog.

### Accepted break: scenario-level `env` is `${var}`-expanded

Through `1.2.2`, values under the top-level `env:` key were passed to the child
verbatim, while `spawn.env` values a few lines away in the same merge were
expanded. A `${who}` in a scenario-level `env` value therefore reached the child
as the literal text `${who}`.

That was an implementation defect rather than a documented behavior: `SCHEMA.md`
has listed scenario-level `env` values as a `${var}` expansion site since `1.0`,
alongside `spawn.command`, `spawn.env`, and the `send` payloads. The contract's
unit is a scenario **valid under the published format**, and a scenario relying
on the literal text was relying on the code contradicting the spec it was written
against.

**What changes for an existing scenario:** every scenario-level `env` value now
goes through the expander, but only two constructs actually change meaning — a
**resolvable** `${name}` (one defined in `variables` or in the parent
environment) now expands, and `$$` collapses to a single literal `$`. Everything
else is byte-for-byte as in `1.2.2`: a bare `$` is untouched (`"$PATH"`,
`"price: $5"`, a trailing `"$"`), and an **unresolvable** `${name}` stays
literal. To keep a literal, double the `$` — `$${name}` yields `${name}`, and
`$$$$` yields `$$`.

Fixing this also closed a masking hole: a `secret: true` variable referenced from
a scenario-level `env` value never reached the child at all, and the literal
placeholder that did was not a secret, so nothing was masked.

### Accepted break: a workspace replaced mid-run is refused

If the scenario's own child replaces the workspace directory *before* an
`expect_snapshot` resolves — deleting and recreating the directory under the same
name, or repointing a symlinked workspace to a directory **outside** the captured
one — the snapshot is now **refused** instead of being written to the
replacement. `1.2.2` re-resolved the workspace name at assertion time and wrote
into whatever directory then sat there, including one the child had just created.

The refusal is a Scenario error (**exit 2**) when the workspace name still
resolves but denotes a different directory, and when it no longer resolves at
all. It is a Process error (**exit 3**) in the narrower case where pitty cannot
re-read the held descriptor's own identity. Both are refusals that write nothing;
a pipeline distinguishing "the run was refused" from "an assertion failed" should
therefore test for non-zero-and-not-1 rather than for exit 2 alone.

Snapshots are now resolved against the directory captured before the scenario
started, identified by `(dev, ino)` from the held descriptor rather than by its
path, so a directory that merely reuses the name is not accepted.

The break is narrower than "a repointed symlink is refused", and the exclusion is
worth stating exactly, because it is the case most likely to be assumed covered.
The identity is held on the workspace's **canonical** path — the alias is
resolved away before the descriptor is opened — so a repoint whose new
destination lands *inside* the captured canonical root is **not** refused.
Repointing `w -> real` to `w -> real/subdir` leaves `real` unchanged and
`real/subdir/out.snap` inside the captured root, so the snapshot is written to
the replacement. Measured: `1.2.2` exits 0 and this build exits 0, both using the
replacement — identical behaviour, and therefore no break at all in that case.

Beyond that, this entry affects only runs where the workspace is actually
replaced and the replacement lands before the assertion. A workspace reached
through an ordinary symlink that is never repointed, one whose *contents* change,
and platform aliases such as macOS's `/tmp` → `/private/tmp` are all unaffected
by *this break* and behave as in `1.2.2`. (That is a statement about this entry,
not a blanket equivalence: a scenario whose child races the resolver can still
differ from `1.2.2`, per the paragraph below.)

The refusal is also not a defence against a child that is *racing* the
resolver rather than replacing the workspace outright; see
[`SECURITY.md`](SECURITY.md)'s *Residual: the path-mutation race* for the limit.
Under those two windows the verdict **can** differ from `1.2.2` — in both known
cases this build passes an assertion `1.2.2` failed — so it is a divergence as
well as a security caveat. It is recorded there rather than as an accepted break
above because no ordinary scenario reaches it: it requires a program mutating its
own workspace concurrently with an assertion.

Relatedly, a `file:` whose `..` detour *names* a directory outside the workspace
no longer creates that directory. `1.2.2` called `create_dir_all` on the raw
path, so `../outside/new/../../w/out.snap` materialised `outside/new` outside the
workspace as a side effect before writing the snapshot inside it. The snapshot
still records; only the escape is gone.

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

## Release checklist (creating a `v1.x.y` tag)

Cutting a release is **automated** by
[`.github/workflows/release.yml`](.github/workflows/release.yml): pushing a
`v1.x.y` tag builds the five prebuilt binaries (Linux X64/ARM64, macOS
X64/ARM64, Windows X64), uploads them with checksums, force-moves the floating
`v1` major tag and `v1.x` minor tag to the release commit, and publishes parallel
floating-ref asset sets. The composite action's `version` input therefore
defaults to the action ref used in `uses: kexi/pitty@...`, so callers can pin
`@v1`, a floating minor ref such as `@v1.x` once it exists, or an exact patch
tag and get matching assets.

The first release (v1.1.0) has run, so the `v1` tag and assets exist and
`uses: kexi/pitty@v1` resolves. (Historically, before that first release, `@v1`
could not resolve and `@main`/a SHA was needed; that bootstrap window is closed.)
The current `v1.2.0` release was cut before minor floating release automation
landed, so `v1.2` will first appear on the next `v1.2.y` release.

When cutting a release:

- [ ] Bump the crate `version` in `Cargo.toml`, refresh `Cargo.lock`, and add a
      `CHANGELOG.md` entry.
- [ ] Run `nix build .#default` and, if Nix reports a hash mismatch, update
      `cargoHash` in [`nix/package.nix`](nix/package.nix). Refreshing
      `Cargo.lock` rotates the vendor hash **even when no dependency changed** —
      the bumped `pitty` version line alone is enough. CI's `nix-build` job
      gates this, but catching it here keeps the release commit green.
- [ ] Push the release tag (e.g. `v1.2.1`). The release workflow then, on its
      own: creates the GitHub Release, builds the five `OS × arch` binaries,
      uploads `pitty-<tag>-<runner-os>-<runner-arch>.tar.gz` (+ `.sha256`) to
      the release, force-moves the `v1` and matching `v1.x` tags to the release
      commit, and publishes the matching `pitty-v1-...` and `pitty-v1.x-...`
      asset sets to their floating releases.
- [ ] Verify the run is green and the fifteen archives (five per ref: Linux
      X64/ARM64, macOS X64/ARM64, Windows X64; refs are `<tag>`, `v1`, and
      `v1.x`) plus their checksums are attached.
      The asset names are
      machine-checked against `action.yml` by
      `tests/release_asset_name_contract.rs`, but a real run also confirms the
      uploads and the tag moves succeeded.
- [ ] Post-push checks that the contract tests cannot cover statically (verify
      on the actual run/assets):
  - [ ] `tar tzf` an uploaded Unix asset shows the `pitty` binary at the
        tarball **root** (no `pitty-.../` leading dir), and the Windows asset
        shows `pitty.exe` at the root, so `action.yml`'s chmod target resolves.
  - [ ] The Apple Silicon asset is named `...-macOS-ARM64.tar.gz`, and the
        Windows x64 asset is named `...-Windows-X64.tar.gz`; asset names use
        GitHub `RUNNER_OS`/`RUNNER_ARCH` labels, not Rust target triples or
        Git-Bash `uname` values.
  - [ ] On the `v*` pushes caused by the `v1`/`v1.x` force-moves, the parse job
        marks them non-publishable and all publishing jobs skip — no duplicate
        upload.
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

The initial publish step cannot be automated — GitHub exposes no workflow/API
switch for it and requires accepting the Marketplace Developer Agreement in the
web UI. That first publish is already complete for this repository. From here,
the release workflow's published GitHub Releases (`v1.2.0` today, refreshed `v1`
today, and refreshed `v1.x` floating releases on future `v1.x.y` releases) are
the Marketplace update path; no further manual Marketplace step is expected per
release.

## What is explicitly out of scope for stability

- Exit code numbers (0/1/2/3) are stable, but the exact wording of error and
  assertion **messages** is not — match on exit codes, not message text.
- Log file format under `logs/` is diagnostic and not contracted.
- Internal Rust APIs (the library crate) are not covered by this policy; only
  the scenario format and report JSON are.
