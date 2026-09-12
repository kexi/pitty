# pitty

[![CI](https://github.com/kexi/pitty/actions/workflows/ci.yml/badge.svg)](https://github.com/kexi/pitty/actions/workflows/ci.yml)
[![Socket](https://github.com/kexi/pitty/actions/workflows/socket.yml/badge.svg)](https://github.com/kexi/pitty/actions/workflows/socket.yml)

A PTY-based CLI testing framework.

`pitty` runs your program inside a real pseudo-terminal, simulates keystrokes
and stdin, and verifies streamed output, file changes, and process behavior —
all driven by declarative YAML scenarios.

> [!NOTE]
> **`pitty` is production ready for supported CLI-testing workloads.** The
> scenario format and report JSON are covered by the SemVer policy in
> [`COMPATIBILITY.md`](COMPATIBILITY.md), every release is gated on Linux,
> macOS, and Windows, and prebuilt archives are checksum-verified before use.
> Pin an exact patch release for reproducible production CI. See
> [`SUPPORT.md`](SUPPORT.md) for the supported platforms (including which
> architectures the test suites run on) and operational scope,
> and [`SECURITY.md`](SECURITY.md) for vulnerability reporting and security
> update coverage.

## Why a PTY?

Many CLI tools and interactive agents behave differently when attached to a
real terminal (line editing, color, prompts, paging). Piping stdin/stdout is
not enough. `pitty` allocates an actual PTY via
[`portable-pty`](https://crates.io/crates/portable-pty), so the program under
test sees a genuine terminal.

## Install

With Nix flakes:

```sh
nix profile install github:kexi/pitty
pitty --help
```

Or run/build from source without installing:

```sh
nix run github:kexi/pitty -- --help
nix build github:kexi/pitty
./result/bin/pitty --help
```

Once pitty is available from nixpkgs, the install target will be:

```sh
nix profile install nixpkgs#pitty
```

For development, use the repo's dev shell:

```sh
direnv allow        # loads the dev shell (rust toolchain via rust-overlay)
cargo build --release
```

Building pitty itself, the test tiers, and the release process are covered in
[`CONTRIBUTING.md`](CONTRIBUTING.md).

## Quick start

```sh
pitty init                       # scaffold pitty.yaml + scenarios/hello.yaml
pitty list                       # list scenario names under scenarios/ (default)
pitty list e2e/                  # list scenario names under any directory
pitty run scenarios/hello.yaml   # run a single scenario
pitty run scenarios/             # run every *.yaml/*.yml in a directory (name order)
```

Each run prints a JSON report and exits with a code (see [Exit codes](#exit-codes)).

## Scenario format

```yaml
name: hello-world
variables:
  username: test-user             # plain value
  token:                          # secret value (masked in logs and errors)
    value: secret-token
    secret: true
env:
  NODE_ENV: test                  # injected into every spawned process
workspace:
  cwd: .                          # run dir, relative to the scenario file
  temp: true                      # OR run in a fresh temp dir (0700 on Unix)
steps:
  - spawn: bash                                   # split on whitespace; add `split: posix` for shell quoting
  - send: echo ${username}                        # stdin line; \r (CR) appended; ${var} expanded
  - send_raw: "y"                                 # raw bytes, no newline
  - key: enter                                    # named key -> control bytes
  - wait: 2s                                      # or 500ms
  - expect:                                       # wait for substring
      contains: hello
      timeout: 30s                                # optional
  - expect_regex:                                 # wait for regex (regex::bytes)
      pattern: 'hello.*world'
  - expect_not:                                   # immediate: must NOT be present now
      contains: error
  - expect_file_exists:
      path: result.txt
  - expect_file_contains:
      path: result.txt
      contains: success
  - expect_file_not_contains:
      path: result.txt
      contains: error
  - expect_file_changed:                          # content differs vs. spawn time
      path: src/auth.ts
  - expect_exit: 0
  - expect_running: true
```

### Stable scenario format (v1)

As of `1.0.0` the scenario format is **stable**. The full specification —
every step, field, the `${var}` rules, and the key set — lives in
[`SCHEMA.md`](SCHEMA.md), with a machine-readable JSON Schema at
[`schema/pitty-scenario-v1.json`](schema/pitty-scenario-v1.json) for editor
autocompletion and validation:

```yaml
# yaml-language-server: $schema=./schema/pitty-scenario-v1.json
version: 1                          # optional; omitted means 1
name: hello-world
# ...
```

A scenario may declare `version: 1` (the default when omitted). A newer version
this build does not understand is a Scenario error rather than a silent
mis-parse. Unknown **top-level** keys (e.g. a `stesp:` typo) are also rejected;
nested step/spec fields stay lenient so a scenario written for a newer `1.x`
pitty still *parses and runs* on an older one — though a field the older build
does not know is ignored, so the scenario may not mean the same thing there.
See [`COMPATIBILITY.md`](COMPATIBILITY.md)
for the full SemVer policy on the scenario format and the report JSON, and
[`CHANGELOG.md`](CHANGELOG.md) for the version history.

### `spawn`, `send` and `env` semantics

- **`spawn`** takes a command line as a single string. By default it is split
  on **whitespace only**: quotes and backslashes are ordinary bytes, so
  `spawn: "echo 'hello world'"` makes the child print the quotes and
  `spawn: "sh -c 'exit 3'"` tries to run the program `'exit`. Add
  **`split: posix`** (mapping form only) to get **POSIX shell word rules**
  instead — `'single quotes'`, `"double quotes"`, and backslash escapes group,
  and the quote characters are consumed rather than passed to the child:

  ```yaml
  - spawn:
      command: "sh -c 'exit 3'"
      split: posix
  ```

  The whitespace rule remains the default because the v1 compatibility contract
  forbids changing the meaning of an existing field — see
  [`COMPATIBILITY.md`](COMPATIBILITY.md). For the same reason an unrecognized
  `split` value — of any type, including a number, list or mapping — is **not**
  an error (older pitty releases ignore the field entirely, so this one may not
  be stricter): it falls back to the default rule and warns on stderr. Note that `split: posix` on a pitty predating the field
  is ignored, so such a scenario runs there with whitespace splitting — usually
  failing the very assertion that motivated the opt-in. Under `split: posix` an
  unterminated
  quote is a process error (exit 3) rather than a best-effort split, and the
  rules apply on **every platform including Windows** (pitty builds an argv, and
  portable-pty re-quotes it for Windows), so a Windows path needs quoting:
  `command: "'C:\\Program Files\\app.exe'"`. **No shell is interposed** under
  either rule — pitty execs the program directly — so pipes, redirection, and
  globbing need an explicit shell (`sh -c '...'`). There is no array (`argv`)
  form; quoting plus `split: posix` is how an argument with spaces is expressed.
  Full rules in [`SCHEMA.md`](SCHEMA.md#spawn-command-tokenization).
- **`send`** appends a carriage return (`\r`, the canonical-mode line
  terminator a PTY expects from Enter) and expands `${var}` placeholders.
- **`send_raw`** writes bytes verbatim with no terminator. `${var}` is still
  expanded; use `$$` for a literal `$`.
- **`env`** at the top level applies to every `spawn`. A `spawn`'s own `env`
  is merged on top and wins on conflicts. Values on both levels are
  `${var}`-expanded, but an `env` value cannot reference a sibling `env` key —
  only `variables` and the parent environment are in scope.
- **`${var}`** is resolved in order: (1) scenario `variables`, (2) the **parent
  process environment** (so `export MY_VAR=value` before `pitty run` lets a
  scenario reference `${MY_VAR}` — e.g. in `spawn.command` — without editing the
  YAML), (3) the literal `${name}` text if absent from both (so a typo is
  visible in the sent input rather than failing the run). Use `$$` for a literal
  `$`.
  - **Secrets and the parent-env fallback:** only `variables` flagged
    `secret: true` are masked. Values pulled in via the parent-environment
    fallback are **not** masked and may appear verbatim in logs and the JSON
    report. Pass any secret value through a `secret: true` variable, not a bare
    `${ENV_VAR}`.

### Named keys

`enter`, `tab`, `escape` (alias `esc`), `backspace`, `up`, `down`, `left`,
`right`, `ctrl+c`, `ctrl+d`, `ctrl+z`. Arrow keys send their CSI escape
sequences (e.g. `up` → `ESC [ A`); control combinations send their ASCII
control byte (e.g. `ctrl+c` → `0x03`).

### `expect` vs. `expect_not`

- `expect` / `expect_regex` **wait** for new output up to a timeout (global
  default 30s). A successful match advances an internal cursor, so two
  consecutive `expect: hello` steps require two distinct occurrences
  (Playwright-style sequential semantics).
- The PTY **echoes what you `send`**, and that echo is part of the output
  `expect` searches. `send: echo ready` followed by `expect: ready` therefore
  matches the typed line before the shell has run anything. Make the needle
  something only execution can produce — for example `send: echo
  ready-$((40+2))` with `expect: ready-42`, the idiom the bundled
  `e2e/scenarios` use — or assert on a file with `expect_file_contains`.
- `expect_not` is **immediate**: it never waits. It fails if the pattern is
  currently present in the *unconsumed* output, and succeeds otherwise. The
  unconsumed region is everything after the last **successful** `expect`'s
  match. So:
  - If a prior `expect` succeeded, `expect_not` ignores output that expect
    already consumed (a forbidden word that appeared *before* the match passes).
  - With no prior `expect` (e.g. right after `spawn`), it scans the whole
    output.
  - If a prior `expect` *failed* (timeout/EOF), the cursor did not advance, so
    `expect_not` still sees output from before that failed expect. Because
    assertions are not fail-fast, this is intended.

### `expect_exit`: poll-once or deadline-bounded

`expect_exit` has two forms:

- **Bare `expect_exit: 0`** checks the child's exit status **once, without
  waiting**. A child that is still running fails the assertion (it has no exit
  code yet). Put a `wait`/`expect` step before it so the process has actually
  exited — for example `send: exit`, then `wait: 500ms`, then `expect_exit: 0`.
- **`expect_exit: {code: 0, timeout: 5s}`** polls for the child's exit up to
  `timeout` before judging, returning the instant the child exits. Prefer this
  when the exit timing is not deterministic (e.g. spawning a process that takes
  a variable amount of time to finish): it removes the race where a fixed
  preceding `wait` is too short. The bare form is kept for the common case where
  the child has demonstrably already exited.

(The library also exposes a blocking `PtySession::wait_exit_code` and a
deadline-bounded `PtySession::wait_exit_code_until` for embedders driving a
session directly.)

### Failure handling: assertions vs. hard errors

- **Assertion failures are not fail-fast.** A failed `expect`, file check, or
  exit-code check is recorded and the run continues, so the report shows the
  outcome of *every* step rather than stopping at the first failure. Any failed
  assertion drives the scenario's status to `failed` (exit code 1).
- **Hard errors abort immediately.** A scenario fault (unknown step, a step that
  needs a prior `spawn`) or a process fault (`openpty`/spawn/kill failure) stops
  the run at that step. The report reflects progress so far and the process
  exits with code 2 (scenario) or 3 (process).
- **The JSON report `status` is two-valued: `passed` or `failed`.** It describes
  only the pass/fail of a *completed* run. A process or scenario fault is **not** a
  third `status` value — it is reported solely through the process **exit code**
  (2 scenario / 3 process), so a hard fault never emits a `status: "error"` JSON
  report. Gate on the exit code for faults; read `status` only to tell a passing
  run from one with a failed assertion.

### `expect_json`: assert on structured output

`expect_json` extracts a JSON value and asserts on a field addressed by a path:

```yaml
- expect_json:
    path: result.status
    equals: success
- expect_json:
    path: result.message
    contains: expired
- expect_json:
    path: result.items
    exists: true
- expect_json:
    path: result.items.0.name             # dotted array index
    equals: first
- expect_json:
    path: 'result.items[0].name'          # bracket array index
    equals: first
- expect_json:
    path: 'result["a.b"].value'           # key containing a dot
    equals: 7
- expect_json:
    path: status
    equals: passed
    source:
      file: report.json
- expect_json:
    path: status
    equals: ok
    timeout: 5s                           # wait for output JSON
- expect_json:                            # YAML value, JSON comparison
    path: result
    equals:
      status: success
      items: [1, 2]
```

- **Source.** With no `source` (the default `output`, which may also be written
  explicitly as `source: output`), pitty extracts the **last *parseable* JSON
  block at the tail of the PTY output** — so a final JSON report printed after
  log noise is found, and braces inside string literals are not mistaken for
  structure. With `source: {file: <path>}` it reads and parses that
  workspace-relative file instead. Any other `source` value (e.g. a typo of
  `output`) is a **scenario error** (exit 2), not a silent fallback. The same
  `source` grammar is shared by `expect_semantic`.
  - **Last *parseable*, not last *emitted*.** Extraction returns the last block
    that parses, which is not guaranteed to be the last block the program wrote.
    If the final report is truncated or malformed, pitty falls back to an
    earlier complete block (bounded to a window near the tail). **Place the JSON
    report at the very end of output** (nothing after it but a newline) so the
    block you intend is the one extracted; do not rely on extraction to reject a
    half-written trailing block when an older complete block sits just above it.
  - **Tail window (`source: output`).** The poll scans only the **last 64 KiB**
    of output. If a real JSON report is followed by **more than ~64 KiB of
    further output**, it is pushed out of the window and not found (the step
    times out and fails). Put the `expect_json` step **right after** the output
    that prints the report, and for reports that may be followed by large output
    prefer `source: {file: <path>}`. The window is a tunable constant in the
    source (`TAIL_JSON_WINDOW`).
- **Path grammar.** A minimal, single-leaf subset of JSONPath: dotted object
  keys (`result.status`), dotted numeric array indices (`items.0.name`),
  bracketed array indices (`items[0].name`), and bracketed double-quoted keys for
  keys that contain a `.` or other separators (`result["a.b"].value`, with `\"`
  and `\\` escapes honored). The forms compose (`a["b.c"][0].d`). A malformed path
  (unterminated bracket/quote, non-numeric bracket index) resolves to a missing
  path (the assertion fails) rather than erroring. Full JSONPath (`$`, `[*]`,
  recursive `..`, filters) is out of scope: the checks compare a single leaf, so a
  multi-match selector would make `equals` ambiguous (any vs. all). The bracket
  forms are a strict superset of the older dotted grammar, so existing paths are
  unaffected.
- **Checks (exactly one).** `equals` compares **typed** JSON (`"200"` the string
  ≠ `200` the number; `true`/`null` match exactly). `contains` is a substring
  test that requires a string target (other types fail with a type message).
  `exists` passes when the path resolves. Specifying zero or multiple checks is a
  scenario error.
- **Waiting.** For `source: output`, pitty polls until a parseable tail JSON
  block appears, up to `timeout` (default 30s). It does **not** consume the
  output cursor, so several `expect_json` steps can inspect the same JSON.
  `source: file` is read immediately.

### `expect_snapshot`: golden-output comparison

```yaml
- expect_snapshot: {file: __snapshots__/output.snap}
- expect_snapshot: {file: output.snap, raw: true}   # do not strip ANSI
```

- The current PTY output is compared against the recorded snapshot `file`
  (workspace-relative). By default ANSI escape sequences are stripped so
  snapshots are terminal-independent; `raw: true` compares the bytes verbatim.
  - **What is stripped:** CSI sequences (color/SGR, cursor moves, erase), OSC
    sequences (window title, hyperlinks; BEL- or ST-terminated), and SS3
    sequences (`ESC O <final>` — the application-keypad arrow/function-key
    responses).
  - **What is *not* normalized (still true as of v1.0):** carriage-return overwrites
    (`50%\r100%`) are kept verbatim — both the pre- and post-CR text and the CR
    itself remain — rather than collapsed to the last write, because faithful
    last-write-wins normalization requires per-column cursor modeling. 8-bit C1
    controls (e.g. a lone `0x9b` as CSI) are also not handled: on UTF-8 terminals
    those bytes are usually multibyte text, and programs emit the 7-bit `ESC [`
    form anyway. Both are deferred to a later release. If a snapshot is sensitive
    to CR overwrites, use a preceding `expect`/`wait` so the final line has
    settled, or `raw: true` for an exact record.
- **Path is confined to the workspace.** Because a snapshot may be *written*
  under `--update`, the `file` path is resolved **inside the workspace
  directory**: a path that escapes via `..` is a **scenario error** (exit 2) and
  nothing is written. A `file` path through a **symlink** is judged by where the
  link leads: one resolving *inside* the workspace is followed (so an
  in-repository `snapshots -> real` keeps working), one resolving *outside* is a
  scenario error, and a **dangling** link — whose target does not exist yet — is
  resolved by hand and judged the same way, so `out.snap -> real/out.snap`
  records while `out.snap -> ../outside/x` is refused. A chain of links is
  followed to its end, and one the running kernel refuses to resolve — `ELOOP`,
  from a cycle or from that platform's own hop limit (32 on macOS, 40 on Linux) —
  is refused rather than resolved by hand; pitty adopts the kernel's verdict
  instead of reimplementing the limit, and imposes **no length bound of its own**,
  so a chain the kernel accepts resolves, up to a visited-set cap that exists
  only so a pathological chain cannot exhaust memory. A dangling cycle, which no
  single kernel call can detect, is caught by remembering the paths already
  visited. A `..` in the path may only cancel a component the kernel can actually
  **traverse**, and it is judged at the position the kernel would be standing in
  — following any symlink already crossed, so `link/..` steps to the *link
  target's* parent rather than to the lexical one. **Both halves of that sentence
  hold only absent a concurrent mutation of the path by the program under test**
  — a child racing the resolver can defeat either; see
  [`SECURITY.md`](SECURITY.md), *Residual: the path-mutation race*, which is
  **not** closed. (Asking about the lexical
  parent let `link/../missing/../victim.snap` pass by checking a `missing` that
  exists beside the link while the kernel was looking at the destination, where
  it does not.) Traversability itself is asked of the kernel rather than inferred
  from the component: `link/../out.snap` where `link -> missing`
  (`ENOENT`), `blocker/../out.snap` where `blocker` is a regular file
  (`ENOTDIR`), and `locked/../out.snap` where `locked` is a directory with no
  search permission (`EACCES`) are all refused rather than quietly resolving onto
  a real neighbouring file. That last one is a directory by every attribute test,
  which is why the question is put to the kernel. Where the platform has no
  `O_SEARCH` (glibc Linux), the check cannot ask for search access directly, so
  it *performs* the traversal instead of predicting it — it opens `dir/.`, which
  the kernel can only resolve by descending through `dir`. Every case there
  either matches the kernel or is stricter; the one difference is a `0100`
  directory (searchable, not readable), which is refused though the kernel would
  allow it — a false refusal reported as a failed assertion, never a false pass.
  A `..` across a merely **absent** component is allowed only under `--update`,
  which is about to create it; on a verifying run nothing creates it, so
  `missing/../out.snap` is refused there too rather than silently comparing
  against a neighbouring file.

  Those last refusals are **failed assertions** (exit 1) and appear in the report
  like any other failing step, because the path is one the *filesystem* would
  have rejected — pitty declines it up front instead of discovering it
  mid-write, so nothing is truncated on the way. Only a path that is wrong
  however the filesystem is arranged — one escaping the workspace — is a
  **scenario error** (exit 2, no report), because there the YAML itself must
  change. A link in the *middle* of the
  path keeps everything after it — `link -> missing-dir` with
  `file: link/out.snap` records at `missing-dir/out.snap` — and that combined
  location is what containment judges. What gets walked is the link's
  target, never the link's own name, so re-pointing it after resolution does not
  redirect the write — *provided nothing is mutating the path concurrently*. A
  program that races the resolver is a separate and **unclosed** limit; see
  [`SECURITY.md`](SECURITY.md), *Residual: the path-mutation race*.
  On Unix the write never re-walks the
  path by name: the workspace directory is captured as a file descriptor before
  any scenario process is spawned, and the recorder descends from that
  descriptor one component at a time with `openat(O_DIRECTORY | O_NOFOLLOW)`.
  So a link planted *after* the check cannot redirect the write to a different
  target — at the final component or at an intermediate directory — and neither
  can renaming the workspace itself and putting a symlink in its place. What
  that does **not** cover is a child mutating the path *between* two of pitty's
  syscalls: that can still leave a write in a stale directory, or make a verify
  pass against a file planted there (same unclosed limit as above).
  The components walked are the ones containment
  validated, normalized and `..`-free, so a `file:` that detours outside the
  workspace before returning to it creates nothing outside. The comparison read
  uses that same traversal, so both halves name the same file and a snapshot
  planted *outside* the workspace cannot satisfy the assertion — a guarantee
  that the two halves agree, not that the directory is the one you meant. A snapshot file that is a **hard link** (link count
  above one) is refused by both the read and the write, since a second name may
  reach outside the workspace and `O_NOFOLLOW` cannot detect one. A `file:` that
  is not a **regular file** — a FIFO, directory, socket or device node — is also
  refused rather than read or written: opening a FIFO would otherwise block the
  run indefinitely waiting for a peer that may never arrive.
  Components above the workspace are never traversed.
  Windows lacks the `openat`-based symlink-race protections, so there the
  resolver's check is the only guard against a link — but the `..`-detour
  guarantee above holds on Windows too, because the path written is rebuilt from
  the validated components rather than the raw `file:` string.
  (Read-only file assertions — `expect_file_*` — are a different thing and are
  not confined under the single-trust model; only snapshot paths are.)
- **Recording / updating** happens only with `--update` (or
  `PITTY_UPDATE_SNAPSHOTS` set to `1`, `true`, or `yes`):
  - file absent, no `--update` → **fail** (`not recorded; rerun with --update`).
    A brand-new snapshot is never silently created, so CI cannot pass an
    unreviewed snapshot.
  - file absent, with `--update` → record current output, pass.
  - file present → compare; a mismatch fails with a unified diff (or, under
    `--update`, overwrites and passes).
- `expect_snapshot` reads the buffer immediately and does not wait; place an
  `expect`/`wait` before it so the output has settled.

> **Security note:** snapshot files are a faithful record of real output and are
> written **unmasked** (masking them would make comparison meaningless). A
> snapshot may therefore contain secrets — do not snapshot sensitive output, and
> `.gitignore` snapshot files that could capture secrets. Because the content is
> unmasked, pitty makes snapshot files `0600` on Unix *before any content is
> written to them* (including one recorded `0644` by an older pitty), and
> creates any snapshot directory it makes itself as `0700`, so another local
> user on a shared runner cannot read them. pitty does **not** change the mode
> of a directory that already exists — recording `file: out.snap` into your
> repository leaves the checkout's own permissions alone. Windows uses the
> runner user's default ACLs. That protects local *read* access, not accidental
> commit — the `.gitignore` advice still applies.

### `expect_semantic`: fuzzy text match

```yaml
- expect_semantic:
    text: |
      Authentication failed due to expired token.
    similarity: 0.8
    # source: {file: ...}    # optional, same grammar as expect_json
```

Asserts the output is "close enough" to `text`, passing when a similarity score
(`0.0`–`1.0`) is at least `similarity`. The optional `source` accepts the same
grammar as `expect_json` (the default `output`, `source: output`, or
`source: {file: <path>}`); an unknown keyword is a scenario error.

- **`similarity` must be within `0.0..=1.0`.** A threshold outside that range is
  a **scenario error** (exit 2): a value above `1.0` could never pass and a
  negative value would always pass, so rather than silently doing either,
  pitty rejects it so the typo is fixed.
- **Leave headroom on round thresholds.** The lexical score is a float, so a
  "clean" fraction is not always represented exactly: a case that is intuitively
  "half a match" can score just **under** `0.5`, and an exact `>=` comparison
  then fails a `similarity: 0.5`. Pick a threshold with margin (e.g. `0.45` or
  `0.8`) rather than the exact boundary you have in mind.

> **Semantic matching is lexical approximation only (still true as of v1.0; a true embeddings backend is planned for a later release).** Similarity is token-bag cosine
> similarity over **normalized words** (lowercased, punctuation stripped, then
> compared as an order-insensitive bag). It rewards shared vocabulary but is
> **blind to paraphrase and synonymy**: "login rejected" and "authentication
> denied" share no words and so score near zero even though they mean the same
> thing, and word order is ignored so "a before b" matches "b before a". Pick a
> `text` that reuses the program's actual wording, and treat the threshold as a
> lexical-overlap gate rather than a true semantic one. A true embeddings backend
> is planned for a later release (behind a Cargo feature) — the YAML grammar
> above is stable so it can be swapped in without changes. Failure messages
> include the computed score and this caveat.

### Updating snapshots: `--update`

`pitty run <path> --update` records absent snapshots and overwrites mismatched
ones (then passes), for **every** `expect_snapshot` in the run. The environment
variable `PITTY_UPDATE_SNAPSHOTS` enables the same behavior, so CI or a local
shell can opt in globally. It is truthy when set (case-insensitively, after
trimming) to `1`, `true`, or `yes` — e.g. `PITTY_UPDATE_SNAPSHOTS=1`,
`PITTY_UPDATE_SNAPSHOTS=true`, or `PITTY_UPDATE_SNAPSHOTS=yes`. Any other
value leaves updating off. Without it, an absent or mismatched snapshot fails.

> **Do not leave `PITTY_UPDATE_SNAPSHOTS` enabled in CI.** With updating on,
> every snapshot is rewritten to the current output and passes, which silently
> disables regression detection. Use `--update` as a one-off, locally, when you
> have reviewed the change.

## Matrix: run one scenario across many values

`pitty matrix <file>` runs a single scenario once per **cell** of a matrix,
comparing implementations or configurations against the same steps. A matrix
declares one or more **axes**; the cells are the **Cartesian product** of all
axes. The matrix is a general-purpose mechanism with **no AI-tool dependency**:
the axis values are arbitrary strings (here, shell command lines).

```yaml
name: bugfix
matrix:
  command: [bash-impl, python-impl, node-impl]   # any command name
steps:
  - spawn: "${command} --fix bug.py"
  - expect:
      contains: fixed
```

Multiple axes expand to their product. With two axes of two values each, four
cells run — every `(command, region)` combination:

```yaml
name: bugfix-matrix
matrix:
  command: [bash-impl, python-impl]
  region: [us, eu]
steps:
  - spawn: "${command} --fix bug.py --region ${region}"
  - expect:
      contains: fixed
```

Cell order is deterministic: axes are taken in **lexicographic key order** and
each axis varies in its **declared value order**, with later axes varying
fastest. For `{command:[a,b], region:[x,y]}` the cells are
`(a,x), (a,y), (b,x), (b,y)`.

> The `*-impl` names above are a **conceptual illustration** (stand-ins for the
> implementations you would compare). For an example that runs out of the box,
> see [`e2e/scenarios/samples/matrix-shell.yaml`](e2e/scenarios/samples/matrix-shell.yaml),
> which uses real shell commands.

Each axis value is injected into the same-named variable (here `${command}`)
just before the run, so the existing `${var}` expansion resolves each cell. A
matrix value **overrides** a static `variables:` entry of the same name — the
axis is the thing being varied, so it wins.

Each axis value is **injected verbatim** into the same-named variable as a plain
value and expanded in a **single pass**: if a matrix value itself contains a
`${other}` placeholder, that placeholder is **not** re-expanded (no recursion),
so the value lands literally. Pick matrix values that are final, not templates
referencing other variables.

Constraints (all reported as scenario errors, exit code 2). Every axis is
checked independently, so a single bad axis fails the whole matrix:

- Each axis value list **must be non-empty**. An empty list (`command: []`)
  makes the product empty and would pass vacuously (a false green in CI), so it
  is rejected (`matrix axis '<key>' has no values`).
- No axis name **may collide with a `secret: true` variable**. Injection
  overwrites the same-named variable with a plain value, which would strip the
  secret flag and unmask the value in the report, logs, and errors. Because
  matrix values are written in plaintext YAML, a secret axis is a design
  contradiction and is rejected
  (`matrix axis '<key>' collides with a secret-declared variable`) rather than
  silently de-masked.
- Each axis key **must be referenced** as `${key}` somewhere expansion reaches
  (a `spawn` command, `send`/`send_raw`, or an `env` value). The reference check
  matches the full `${key}` placeholder, so a longer name that merely shares a
  prefix (`${command2}` for axis `command`) does **not** count. An unreferenced
  axis is an authoring mistake (`matrix key '<key>' is never referenced`).
- The product **must not exceed the cell cap** (default **256**). Because the
  product grows multiplicatively, a few axes can demand thousands of real
  process spawns; an oversized product is rejected up front
  (`matrix expands to N cells, exceeding the limit of ...`) rather than starting
  a spawn storm. Raise it for an intentional large sweep by setting
  `PITTY_MATRIX_MAX_CELLS` (an unset or non-numeric value falls back to 256).
- A scenario with a `matrix:` section run via `pitty run` is **refused** (use
  `pitty matrix`); a scenario without one run via `pitty matrix` errors
  (`no matrix section`).

**Snapshots are never recorded or updated by `matrix`.** Each cell only
*compares* against an existing snapshot; there is no `--update` flag. Every cell
shares the same snapshot path, so recording would let the last cell clobber the
others (a write race). A cell whose snapshot is absent therefore **fails**
(`not recorded; rerun with --update`). Record snapshots once with
`pitty run --update`, then gate with `pitty matrix`.

Output: a column-aligned table by default, or a machine-readable `MatrixReport`
JSON with `--json`. A single-axis matrix prints `value  PASS/FAIL  (ms)`; a
multi-axis matrix prints each cell's coordinates as `key=value key=value  PASS/FAIL (ms)`
(one space before the verdict) so every cell is self-describing. The exit code is
the worst across cells (one failing cell fails CI); `--no-fail` walks every cell
and exits 0 despite red ones, for the "observe all implementations" use case.
**`--no-fail` suppresses only red (assertion-failing) cells.** A hard fault — a spawn failure or
a scenario error in a cell — aborts the matrix at that cell (later cells do not
run) and still exits with its class (scenario 2 / process 3) even under
`--no-fail`, because a broken harness is not an "informational" red cell.

```console
$ pitty matrix scenarios/bugfix.yaml
bash-impl    PASS  (1180ms)
python-impl  FAIL  (1920ms)
node-impl    PASS  (1340ms)
$ echo $?
1
```

A multi-axis matrix prints one `key=value …` row per cell:

```console
$ pitty matrix e2e/scenarios/samples/matrix-multi-axis.yaml
command=echo region=us    PASS (2ms)
command=echo region=eu    PASS (1ms)
command=printf region=us  PASS (2ms)
command=printf region=eu  PASS (4ms)
```

#### `--json` shape

`--json` emits a `MatrixReport`: an `axes` array (axis names in lexicographic
order) plus a `cells` array, where each cell carries its `coords` map (axis name
→ value used) and the embedded run `report`.

```json
{
  "axes": ["command", "region"],
  "cells": [
    {
      "coords": { "command": "echo", "region": "us" },
      "report": {
        "scenario": "bugfix-matrix",
        "status": "passed",
        "duration_ms": 12,
        "assertions": [ { "step": "expect: contains \"matched-\"", "passed": true } ]
      }
    }
  ]
}
```

> **Breaking change in v0.4:** the `MatrixReport` JSON moved from the old
> single-axis `{axis, value}` per-cell fields to the multi-axis `axes` array plus
> per-cell `coords` map (`{axes: [...], cells: [{coords: {axis: value}, report: {...}}]}`).
> A script that read the old top-level `axis`/per-cell `value` fields will not find
> them — read `axes` and each cell's `coords` instead.

## Bench: repeat a scenario to measure timing and flakiness

`pitty bench <file> [--runs N] [--warmup M]` runs a scenario `warmup + runs`
times (default `--runs 10 --warmup 0`), discards the warmup iterations, and
reports duration statistics plus a **pass rate** that surfaces flakiness.

```console
$ pitty bench scenarios/bugfix.yaml --runs 10
scenario: bugfix
runs: 10 (0 warmup)
pass: 9/10  (FLAKY)
duration_ms: min 1180  median 1340  mean 1402  p95 1980  max 2100  stddev 240
```

- **Flaky** (`FLAKY` marker) means some — but not all — measured runs passed;
  all-pass and all-fail are deterministic and unmarked.
- **p95** uses the **nearest-rank** method (rank = `ceil(0.95 × n)`, 1-indexed),
  so it is always an actually-observed duration rather than an interpolated
  value. For small `n` it can equal `max` (e.g. `n = 10` → rank 10; `n = 2` →
  rank 2, the upper of the pair).
- `stddev` is the population standard deviation (divides by `n`).
- **Failed runs still count toward the timing statistics.** A run's duration is
  recorded whether it passed or failed, so the distribution reflects every
  measured run; the pass rate (and `FLAKY` marker) tracks correctness separately.
- **`--warmup` may exceed `--runs`.** Warmups are simply discarded first, so
  `--runs 1 --warmup 3` runs four times and reports the single measured run.
- **Each run gets a fresh workspace.** With `workspace.temp: true`, every run is
  given its own temp directory (`0700` on Unix), so a file one run writes is
  never visible to the next — runs do not share state.
- **Snapshots are never recorded or updated by `bench`** (no `--update` flag):
  re-recording across N runs would just keep whichever run wrote last, so an
  absent snapshot fails. Record once with `pitty run --update` first.
- `--json` emits a `BenchReport` with the raw `durations` array and nested
  `stats`.
- Exit code: 0 only when every measured run passed; any assertion failure yields
  1 (bench exists to catch flakiness, so a single failure fails the process),
  and a hard fault keeps its class (scenario 2 / process 3).

## Use in GitHub Actions

Run scenarios as a step with the bundled composite action:

```yaml
- uses: kexi/pitty@v1                # floating major tag, maintained per release
  with:
    scenario: e2e/scenarios/positive   # file or directory
    command: run                       # run (default), matrix, or bench
    args: ""                           # extra flags, e.g. "--no-fail"
```

The action installs pitty and runs it. It prefers a **prebuilt binary** from
the GitHub Release matching the runner's OS/arch (fast: a tarball download, no
compilation), and falls back to `cargo install --git` from source when no
prebuilt asset exists for that platform. The release automation
([`.github/workflows/release.yml`](.github/workflows/release.yml)) publishes
prebuilt binaries for Linux (X64, ARM64), macOS (X64, ARM64), and Windows (X64) on
every release and keeps both floating major (`v1`) and matching minor (`v1.x`)
assets in step with their tags for release lines created by that workflow. The
installer defaults to the same ref used in `uses: kexi/pitty@...`, so
semver-pinned action refs get the matching fast path on those platforms. The
step's exit code is the verdict, so a failing scenario fails the job.

An explicit `version:` is **authoritative**: the pinned ref is installed and
takes precedence even when a `pitty` binary is already on the runner's `PATH`
(a self-hosted runner image, a cached tool directory, or an earlier step). When
`version:` is omitted, a `pitty` already on `PATH` is reused as-is and nothing
is downloaded — so pre-provisioned runners keep working, and a caller who wants
a specific build asks for it by pinning.

The action is published to the GitHub Marketplace as
[**pitty-action**](https://github.com/marketplace/actions/pitty-action) (the
bare name `pitty` is taken by an unrelated GitHub user; the Marketplace listing
name does not affect how you reference it — use `kexi/pitty@v1`, a floating
minor tag after that line exists, or a patch tag such as `kexi/pitty@v1.2.0`).

When pitty detects `GITHUB_ACTIONS=true` (or you pass `--github`) it emits two
extra outputs alongside its normal stdout:

- a **step summary** appended to `$GITHUB_STEP_SUMMARY` — an assertion table for
  `run`, a PASS/FAIL table for `matrix`, and a metrics table for `bench`;
- **annotations**: a `::error` per failed assertion or matrix cell (surfaced
  inline on the run and PR), and a `::warning` for a flaky `bench`.

Both are **side effects only** — they never change the exit code, and a missing
or unwritable summary file is ignored. All summary and annotation text is
**secret-masked**: any `secret: true` variable's value is replaced with `***`
before it can reach the summary, an annotation, or the CI log.

Annotations are written to **stderr**, never stdout. stdout stays pitty's
machine-readable channel — `run` always prints a JSON report there, and
`matrix`/`bench` do under `--json` — so `pitty matrix --json scenario.yaml | jq`
keeps working on a runner even when a cell fails and an annotation is emitted.
GitHub parses workflow commands off both streams, so the inline run/PR
annotations appear either way.

To preview the output locally without a runner:

```sh
GITHUB_STEP_SUMMARY=/tmp/summary.md pitty matrix scenario.yaml --github
cat /tmp/summary.md
```

## Exit codes

| Code | Meaning                                                                |
|------|-----------------------------------------------------------------------|
| 0    | All assertions passed.                                                |
| 1    | An assertion failed (mismatch, timeout, EOF before match, file/exit). |
| 2    | Scenario error (invalid YAML, unknown step, missing file, invalid matrix). |
| 3    | Process error (openpty/spawn/kill failure).                           |

When running multiple scenarios (or matrix cells), the final exit code is the
most severe outcome: process (3) > scenario (2) > assertion (1) > success (0).
`pitty matrix --no-fail` suppresses **assertion** failures only, exiting 0 for a
run whose cells merely went red. A hard fault — a scenario error (2) or a process
error (3) — still exits with its own class, because a broken harness is not an
informational result. See
[Matrix: run one scenario across many values](#matrix-run-one-scenario-across-many-values)
for the full rule.

## Logs

Every run writes a log containing the captured terminal output and per-step
results — including a run that never spawned a process, whose output section
reads `(no process spawned)`. When a scenario spawns more than once, each
session's output appears under its own `--- session N ---` banner, so every
assertion in the file is backed by the output that produced it.

The file name identifies the run:

| Case | Log file |
|------|----------|
| `echo-flow.yaml` declaring `name: echo-flow` | `logs/echo-flow.log` |
| `a.yaml` declaring `name: other` | `logs/a.other.log` |
| `pitty matrix m.yaml` (`name: m`, axis `word`) | `logs/m.word-<value>.log` per cell |

The scenario's `name:` is the base. The **file stem** is prefixed only when it
differs from the name, so a scenario that follows the usual convention keeps the
familiar `logs/<scenario>.log`, while two files declaring the same `name:` no
longer overwrite each other in a directory run. **Matrix cell coordinates** are
appended so each cell keeps its own diagnostics.

Secrets registered with `secret: true` are masked out of the *filename* too,
wherever they appear in the components this scheme adds — the scenario file's
stem and a matrix cell's axis names and values. Because masking is many-to-one,
a name whose masking removed text gains a short digest suffix so two runs
differing only inside a secret still get separate logs; a filename with no
secret in it is left exactly as it reads above. (A secret in the scenario's own
`name:` is tracked separately as issue #43 and is not yet masked.)

A scenario written `.yml` carries its extension in the name (`a-yml.<scenario>.log`),
since `pitty run <dir>` executes `a.yaml` and `a.yml` as two separate scenarios;
the canonical `.yaml` spelling stays unmarked. A name that would exceed the
filesystem's limit is shortened, with a digest replacing the dropped tail, so an
over-long matrix axis value still produces a log instead of failing to write one.

Each part is reduced to `[A-Za-z0-9._-]`. Log files are created with `0600`
permissions on Unix (Windows uses the runner user's default file ACLs), and
registered secret values are replaced with `***` before anything is written.

**Ownership.** Each log has a hidden sidecar, `.<log name>.claim`, recording
which scenario wrote it: the scenario's identity *after secret masking*, plus a
random per-log token. That record is what lets a re-run reuse its own file
instead of accumulating numbered copies, so in the ordinary case the log you open
after a failure is the newest one. A log with no sidecar — an older pitty's, or
one restored without its dotfiles — belongs to nobody pitty can verify, so it is
left untouched and the run writes alongside it (its name is then taken by a
suffixed `<stem>.2.log`, and the stale file stays). A run never *overwrites*
another scenario's log: where two identities would land on one name, the later
one is suffixed instead.

On Unix a lock on the sidecar also keeps two concurrent pitty runs from writing
one file at once. That lock is advisory, so it does not constrain anything but
pitty, and on a network filesystem that accepts `flock` without sharing locks
between clients two hosts can still interleave; see `SECURITY.md` for the exact
scope.

**One limit worth knowing.** Because the record stores the *masked* identity, two
matrix cells whose axis values differ only inside a secret are indistinguishable
on disk — their records are byte-identical, and the random token cannot help,
since a later process has no way to recompute which token was its own. They
always get separate logs and the set of logs is stable across runs, but **which
of the two a given cell reuses is not guaranteed**: on a later run the two can
swap files. Binding them would require a value a cell can reproduce but a reader
of `logs/` cannot invert, which is not achievable for a low-entropy secret. This
is the one case where "each scenario reuses its own file" holds only for the
*pair*, not for each cell individually.

Neither dotfile is a log, and `logs/*.log` globs skip both.

`logs/` is a unit: preserve or discard it whole, dotfiles included. Restoring
only `logs/*.log` drops the `.claim` sidecars, which are what record ownership —
a restored log without its sidecar is treated as belonging to nobody, so pitty
leaves it untouched and writes alongside it under a new name. Nothing is
destroyed, but the old logs stop being reused.

If a log cannot be written at all — a full disk, an unwritable `logs/`, or every
candidate name already held by other scenarios — pitty prints a warning on
stderr and leaves the run's verdict alone. It never resolves a name collision by
overwriting another scenario's log. **On Unix**, a `logs/` directory that is a
symlink is refused, and a symlink planted at a log's own name is never written
through — a log always lands inside `logs/`, never at a link's target. `logs/` is
opened from a descriptor taken before the scenario starts, so replacing it with a
symlink mid-run cannot redirect the write either; if that descriptor cannot be
obtained, the log is skipped with a warning rather than written to an
unprotected path.

**On Windows none of those log guarantees hold.** There is no pre-spawn
descriptor and the log path is resolved by name on each use, so a directory
junction at `logs/` pointing elsewhere, or a link swapped in after the name is
chosen, redirects the write. Log content is masked, which bounds the exposure,
but do not rely on log containment on Windows. (This differs from *snapshot*
writes, whose `..`-detour containment does hold on Windows — see the security
section. The difference is not an oversight: a snapshot path is author-supplied
and can contain `..`, so validating its components is what makes containment
achievable without `openat`. A log name has no author-supplied path in it — the
directory is the fixed literal `logs/` and the file name is reduced to
`[A-Za-z0-9._-]`, so no separator survives — which leaves only the symlink races
that genuinely require `openat` to close.)

The log is a diagnostics sink, not an assertion, so its failure is reported
rather than counted against the program under test; stdout stays a clean JSON
report.

## Security and trust model

> **pitty is single-trust (unchanged since v0.1).** You run your own
> scenarios in your own environment. Untrusted scenario YAML is **not**
> supported: a scenario can spawn arbitrary processes and read/write files, so
> treat scenario files as code you own.

Minimal guards (in place since v0.1):

- **Temp/log/snapshot permissions.** `workspace.temp: true` uses
  `tempfile::TempDir` (atomic temp-directory creation, no self-chosen names). On
  Unix, temp workspaces are set to `0700`, logs to `0600`, and recorded
  snapshots to `0600` — applied to the descriptor before any content is written,
  so refreshing a snapshot an older pitty left at `0644` never exposes the new
  content — because snapshot content is unmasked. Snapshot directories **pitty
  creates** are `0700`; a directory that already exists keeps the mode it has,
  since it belongs to the user (tightening a shared repository checkout to
  `0700` would lock out other users and later CI steps). On Windows, pitty
  relies on the runner user's default ACLs.
- **Snapshot reads and writes do not follow a symlink at use time** (Unix; see
  the caveats at the end of this bullet). A `file`
  path through a link that lands *outside* the workspace is a scenario error,
  whether the link resolves or dangles; one that lands inside is followed once
  during resolution and thereafter only its target is walked, so the link's own
  name is never opened. A symlink cycle is refused. On Unix the write
  additionally descends with
  `openat(O_DIRECTORY | O_NOFOLLOW)` per component, starting from a descriptor
  for the workspace directory captured before any scenario process was spawned,
  and creates the file relative to that descriptor. So a link planted after the
  check cannot redirect it at the final component *or* at an intermediate
  directory, and a child that renames the workspace and leaves a symlink in its
  place cannot redirect it either. The walked components are the normalized,
  `..`-free sequence containment approved, so nothing is created outside the
  workspace even when the `file:` value detours through a parent directory.
  The **comparison read uses the same traversal**, so a snapshot planted outside
  the workspace cannot make an assertion pass. (That is a statement about where
  the traversal can reach, not about a program racing it: a child that mutates a
  path component between two of pitty's syscalls can still steer the shared
  traversal into a stale directory and make an assertion pass there. That class
  is documented as unclosed in [`SECURITY.md`](SECURITY.md) under *Residual: the
  path-mutation race*; do not rely on snapshot containment against a program
  that is actively racing it.) **Hard links are refused** on
  both halves: a hard link is not a symlink, so `O_NOFOLLOW` cannot see one, and
  a file linked to an external inode would otherwise be read from and written
  through; pitty `fstat`s the descriptor it opened and refuses a link count above
  one. (Residual: an attacker who adds a link *after* that check can read what
  pitty subsequently writes, which no check at this layer can prevent; it cannot
  redirect a write or make a planted file pass.) If the workspace descriptor
  cannot be captured, a run that records a snapshot fails rather than falling
  back to an unprotected write — but a scenario with no `expect_snapshot` step
  never needs *that* descriptor, so a search-only (`0300`) workspace still runs
  its steps. (Log writing has a separate pre-spawn descriptor, taken on the
  scenario file's directory rather than the workspace, under the same
  fail-closed rule — so such a run can still lose its log. See *Log files*.) Where `O_SEARCH` exists (macOS) such a workspace supports snapshots too,
  since a traversal needs search permission rather than read. Components above
  the workspace are never traversed. **On Windows** there is no `openat`, so
  none of the *symlink and hard-link races* above are closed — a junction or a
  link swapped in after the resolver's check redirects the write. What does
  still hold there is `..` containment: the path written is rebuilt from the
  validated component sequence, which needs no `openat`, so a `file:` that
  detours through a parent creates nothing outside the workspace on any
  platform.
- **Secret masking.** Variables flagged `secret: true` have their literal
  value masked (`***`) in logs and error messages.
- **Best-effort cleanup.** Temp directories are removed when their `TempDir`
  is dropped at the end of a run. On `Ctrl-C` or a panic, a temp directory may
  be left behind (cleanup relies on `TempDir`'s `Drop`, unchanged since v0.1).

## Contributing

Working on pitty itself — the dev environment, the test tiers, security
scanning, and the release process — is documented in
[`CONTRIBUTING.md`](CONTRIBUTING.md).

## License

Licensed under the MIT license ([LICENSE](LICENSE)).
