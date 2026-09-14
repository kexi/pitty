# Changelog

All notable changes to pitty are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and this project
adheres to [Semantic Versioning](https://semver.org/). See
[`COMPATIBILITY.md`](COMPATIBILITY.md) for what the version number guarantees
about the scenario format and the report JSON.

The pre-1.0 entries below are development milestones, not separately tagged
releases; only 1.0.0 carries a release date.

## [Unreleased]

### Added

- **Production support and security policies.** `SUPPORT.md` now defines the
  supported workloads, platforms, and operational boundaries, while
  `SECURITY.md` documents security-update coverage and private vulnerability
  reporting.
- **Native macOS Intel release assets.** Release automation now publishes
  checksum-protected X64 binaries alongside the existing Apple Silicon assets.
- **Opt-in POSIX shell quoting for `spawn` (`split: posix`).** A `spawn`
  command line is tokenized with a bare whitespace split, so quotes are never
  grouping syntax — they are handed to the child as literal characters and any
  argument containing a space is torn into several. `spawn: "echo 'hello
  world'"` makes the child print `'hello world'` while a
  `contains: "hello world"` assertion still *passes*, silently recording the
  wrong bytes in a snapshot, and `spawn: "sh -c 'exit 3'"` runs the program
  `'exit` and reports exit code 2. The mapping form of `spawn` now accepts an
  optional `split` field; `split: posix` tokenizes that one command with
  **POSIX shell word rules** (`'...'`, `"..."`, and backslash escapes group and
  the quote characters are consumed), fixing both cases:

  ```yaml
  - spawn:
      command: "sh -c 'exit 3'"
      split: posix
  - expect_exit: 3
  ```

  The rules apply on every platform including Windows — pitty builds an argv
  vector and portable-pty performs the Windows re-quoting, so one scenario file
  cannot mean two things on two runners. Under `split: posix` a command line
  that cannot be tokenized (an unterminated quote) is a process error (exit 3)
  rather than a best-effort split. No shell is interposed either way, so pipes,
  redirection, and globbing still require an explicit `sh -c '...'`.

  An **unrecognized `split` value is not an error**: it falls back to the
  default rule and warns on stderr. This holds for any value of any *type* —
  a number, boolean, list or mapping at that key warns rather than failing the
  scenario. That is forced by the same contract — the field postdates `1.0`, so
  every earlier `1.x` accepts any value at that key, of any type, and ignores
  the field, and this build may not reject a document they run. The
  converse is that `split: posix` on a pitty predating the field is ignored and
  the command is whitespace-split; the scenario still runs there but does not
  mean the same thing, and in practice fails the assertion that motivated the
  opt-in. Pin a minimum pitty version in CI if that matters.

  **The default is unchanged.** A `spawn` without `split`, and the bare string
  form (`spawn: echo hi`) which cannot carry the field at all, tokenize exactly
  as every previous 1.x release did. **Why the obviously-correct rule is not the
  default:** [`COMPATIBILITY.md`](COMPATIBILITY.md) promises that within `1.x`
  pitty will not change the meaning of an existing field or tighten validation
  so a previously valid scenario becomes an error, and making POSIX rules
  unconditional does both. It is not theoretical — a scenario with
  `spawn: "echo 'hello world'"` whose snapshot was recorded on 1.2.2 contains
  the literal bytes `'hello world'`, and an unconditional switch fails it with
  `-'hello world' +hello world` on a patch-level upgrade. Adding an optional
  field is the additive change the contract does permit. A future `2.0` may
  make `posix` the default. Documented in
  [`SCHEMA.md`](SCHEMA.md#spawn-command-tokenization) and the README, which were
  previously silent on tokenization.

### Changed

- **Production-readiness status.** The README now identifies the supported
  CLI-testing scope as production ready and links that claim to explicit
  compatibility, support, and security policies.
- **Windows is a full CI gate.** The Windows job now runs the real-PTY test
  suite and all three dogfood tiers through ConPTY, matching Linux and macOS,
  instead of a single cmd.exe smoke scenario.
- **Releases wait for CI.** The release workflow now refuses to publish until
  a CI run of the tagged commit has succeeded on every platform. A transient
  GitHub API failure while waiting is retried rather than treated as a red CI.
- **Bounded PTY teardown.** Session shutdown is capped at five seconds per
  phase and reports a stalled teardown on stderr instead of hanging; on
  Windows ConPTY a stalled teardown previously blocked a scenario for about
  five minutes. On Windows the child now runs inside a kill-on-close job
  object, so teardown terminates the whole process tree (for example the real
  shell behind Git for Windows' `bin\bash.exe` launcher) rather than only the
  direct child. A teardown that leaves the child or its tree alive is now a
  process error (exit 3) even when every assertion passed; only a console
  host that is slow to release its handles after the tree died is downgraded
  to a stderr warning. On Unix, teardown now also sends `SIGKILL` to the
  child's process group (the child is a session leader), a best-effort sweep
  of descendants that still share that group even after the child itself has
  exited; pipelines an interactive shell placed in their own job-control
  groups are not covered. Exit observation (`expect_exit`, `expect_running`)
  no longer reaps the child on Unix — it peeks with `waitid(WNOWAIT)` — so the
  leader's pid stays pinned until that sweep has run and cannot be recycled
  by an unrelated process in between. A `spawn` that replaces a live session
  tears the previous one down through the same classification instead of
  silently.
- **Dogfood scenarios assert real output.** Every dogfood needle is now
  computed by the shell (for example `$((40+2))`), so the PTY's echo of the
  typed command can no longer satisfy an assertion on its own.
- **Source fallback fails closed.** The composite action's `cargo install`
  fallback no longer retries without `--locked` when the committed lockfile
  cannot be used.

### Fixed

- **A snapshot path whose `..` crosses a non-directory no longer destroys a real
  file.** The guard that keeps `..` from being cancelled lexically asked only
  whether the crossed component *existed*. A regular file exists, so
  `blocker/../victim.snap` passed it — while the kernel refuses that path with
  `ENOTDIR` at `blocker`, exactly as it refuses a missing component with
  `ENOENT`. The lexical answer is a real neighbouring file, and under `--update`
  pitty truncated and rewrote it: silent data loss inside the workspace, where
  1.2.2 reported "Not a directory" and left the file untouched. The rule the
  kernel actually applies is that `..` is only meaningful across a **resolvable
  directory**, which is now what is tested — existence alone is not enough, since
  a regular file, a FIFO, a device node and a dangling symlink all exist while
  being equally untraversable. The check also now runs on the scenario's own
  `file:` value and not only on a symlink's destination; previously the identical
  shape written straight into `file:` bypassed the guard entirely. On the `file:`
  value it is narrowed to components that exist and are *not* directories, so a
  `..` across a merely absent component keeps working — `--update` creates the
  snapshot's parent directories, and refusing those would break scenarios 1.2.2
  records. Both refusals are reported as **failed snapshot assertions** (exit 1,
  with a report), which is how 1.2.2 surfaced them once the write failed —
  refusing earlier keeps the victim intact without costing a consumer the report
  it parses. A path that escapes the workspace remains a scenario error (exit 2).
- **A symlink chain the kernel refuses is no longer resolved by hand.** The hop
  bound counted pitty's own `read_link` calls, but each of those makes the kernel
  traverse the path's intermediate directory components internally, and those
  hops were invisible to the counter. `out.snap -> d0/final` over a 31-link
  directory chain costs the kernel 33 hops — it answers `ELOOP`, and 1.2.2
  reported "Too many levels of symbolic links" — while this counter saw 2,
  resolved the path, and wrote through to the file behind it. No count of pitty's
  own hops can reproduce that arithmetic, because the two are not counting the
  same events. Resolvability is now the running kernel's answer about the whole
  path rather than a constant maintained in pitty, so the behaviour matches
  whatever limit the platform enforces. Like the `..` refusals above, an
  unresolvable chain is a failed snapshot assertion (exit 1, with a report) — the
  class 1.2.2 put it in — rather than a scenario error.
- **No bound of pitty's own caps symlink chain length any more.** The previous
  entry claimed the hardcoded limit was gone; it was not. `MAX_DANGLING_LINK_HOPS
  = 32` survived as a "liveness guard", and `0..=32` still completed at most 32
  links, so the **v1 compatibility** break on Linux it was supposed to fix
  remained: Linux's limit is 40, so a 33-link chain that 1.2.2 recorded was still
  refused as an unresolvable assertion (exit 1 where v1 exited 0). The count is
  now gone outright, replaced by a set of the paths the walk has visited. That is
  a better fit for the only question pitty must answer itself — a dangling cycle
  is *precisely* a repeated path, so a set decides it exactly, while a count only
  approximated it and truncated legitimate chains in the process. The set's size
  is capped so a pathological chain cannot exhaust memory, and reaching that cap
  refuses the path rather than truncating the walk and reporting a destination
  never actually reached.

  Measured boundary: `0..=32` supplied exactly 33 walk iterations, and a 33-link
  chain needs 34 — so 33 links was the first chain truncated. macOS rejects that
  chain itself (`MAXSYMLINKS` is 32), which is why the defect was invisible
  there; on Linux (limit 40) it was a live v1 break. The macOS numbers were
  verified by execution; the Linux break was **not** — no Linux host was
  available — but it shares this exact cause.
- **`--update` no longer overwrites a snapshot on a workspace it cannot write
  to.** The resolver let a `..` cancel a missing component on the grounds that
  `--update` would create it. That premise was false twice over. It was false in
  general, because the component list the recorder walks is `..`-eliminated, so
  `missing/` was never created by anything — while 1.2.2 called `create_dir_all`
  on the raw path and therefore *did* create it. And it was false specifically on
  a workspace that is not writable, where that creation fails with `EACCES`:
  1.2.2 exited 1 and left the recorded snapshot alone, while predicting success
  let the overwrite proceed — exit 0 where v1 exited 1, destroying a real
  snapshot.

  This was the fourth variant of one root cause: the code predicting an outcome
  only the kernel decides (existence, then file type, then traversability, now
  creatability). The prediction is gone. The directories a `file:` value names
  and `..` cancels are carried to the recorder, which **attempts** them through
  the same anchored `openat`/`mkdirat` walk the write uses. A writable tree
  records and materializes the component exactly as 1.2.2 did; a read-only one
  fails with the kernel's own error. Nothing here forecasts which.

  The first version of this fix worked only for *absolutely*-addressed scenarios.
  The cancelled-directory list was built on the workspace's configured `cwd`,
  which stays **relative** when the scenario itself is named by a relative path
  (`pitty run s.yaml` with `workspace.cwd: w` leaves it as `w`), while the list is
  stripped against the canonical absolute root — so the prefix did not match, the
  entry was silently dropped, and the recorder had nothing to attempt. The
  read-only refusal therefore reappeared for exactly the invocation form people
  actually use.

  That fix was itself incomplete, and the claim that anchoring on the same
  canonical root made a disagreement impossible was **wrong**: it removed one
  spelling, not the mismatch. The same data loss returned twice more through
  other representations of the same path — a symlinked component, then a
  symlinked workspace named by an *absolute* `file:`, where the absolute path
  re-rooted through `RootDir` and the lexical `strip_prefix` failed again. Three
  BLOCKERs, one cause: two independent derivations of "where does this path
  point", one lexical and one canonical, which will always disagree on some
  input. macOS's `/var` -> `/private/var` is an ordinary alias of exactly this
  kind, so the condition is reachable in normal use and not only under attack.

  The mismatch is now removed rather than patched. The cancelled components are
  derived from a single anchoring step that re-expresses the candidate in the
  root's own representation while preserving its `..` structure, so every
  spelling — relative, absolute, dotted, through a symlinked workspace or a
  symlinked component — reduces to one form before anything is compared. Where a
  disagreement is still structurally possible it is reported as an error instead
  of silently discarding the entry, because a silent drop is what hid all three.
  A cross-representation test asserts every spelling of one location yields the
  same result, and the end-to-end matrix covers invocation form and path
  representation as explicit dimensions (224 cells, no divergence from 1.2.2).

  A second correction: attempting the cancelled component must not demand that it
  be *created*. `walk_from` opens every component `O_NOFOLLOW`, which is right for
  the snapshot's own path but wrong for a component that merely has to be
  traversable — so `dl -> d` with `file: dl/../out.snap` was refused with ENOTDIR
  and `--update` recorded nothing, while telling the user to rerun with the flag
  they had just used. 1.2.2 records it, because `create_dir_all` follows symlinks.
  A component the kernel can already traverse is now skipped: nothing needs
  creating, and no byte is written through it. Measured against v1's
  `create_dir_all` on the raw path, which is the behaviour being reproduced:
  `dl/..` and `missing/..` succeed (the latter creating the directory), while
  `blocker/..` gives ENOTDIR and `locked/..` gives EACCES — the last two already
  refused earlier, with the same verdict.
- **A `..` after a symlink now steps where the kernel steps.** Position was
  tracked as a path but still built *lexically*, so a `..` following a traversed
  symlink popped the link's own parent instead of the destination's. With
  `link -> real/subdir`, `link/../missing/../victim.snap` was judged against a
  `missing` beside the link; the kernel was looking at `real/missing`. Where the
  first exists and the second does not, the guard approved a path the kernel
  refuses with `ENOENT`, and the assertion **passed** by comparing the output
  against `real/victim.snap` — exit 0 where 1.2.2 exits 1 with "not recorded".
  The walk now canonicalizes its position as it descends, so a later `..` moves
  the way the kernel moves.

  The asymmetry is what makes this reachable and what made it hide: the same
  name must exist at the lexical position and be absent at the symlink's
  destination. A plain symlinked component or a plain missing component reaches
  neither half, which is why a matrix that varied path shape, invocation form,
  representation and excursion still missed it. Existence-at-lexical vs
  existence-at-destination is now an explicit test axis.
- **A `..` after the path re-enters the workspace is no longer discarded.** A
  `file:` may leave the workspace and come back —
  `../outside/new/../../w/inside/../victim.snap` — and the `w/inside/..` at the
  end is a cancellation squarely inside the root. The walk decided "am I inside
  the root" **once**, from where the path began, and gated recording on that
  sample; since this path begins outside, everything cancelled after the
  re-entry was dropped. Nothing was attempted, the read-only-workspace refusal
  disappeared, and `--update` destroyed a snapshot 1.2.2 preserved. Position is
  now tracked per component instead of sampled, so the recorded cancellations are
  exactly those inside the root however many times the path exits and re-enters.
  A cancellation that is deliberately *not* recorded (because it names the root
  or a location above it) is now counted at that site rather than falling
  through, since a silent discard here has caused four data-loss bugs.
- **The pinned workspace root now carries the descriptor's identity, not just
  its spelling.** Pinning the canonical root as a *string* does not tie it to the
  directory the write goes through: a workspace deleted and recreated under the
  same name canonicalizes to a byte-identical path while being a different
  inode, so the pinned name and the held descriptor could still denote different
  directories. The pin is now validated against the descriptor by `(dev, ino)` —
  `fstat` on the held fd versus `stat` on the pinned path — and a mismatch
  refuses the run rather than resolving against a directory the scenario
  substituted. 1.2.2 writes into the recreated directory here (verified by
  running it); pitty refuses, the same deliberate divergence as the symlinked
  workspace case. The check is no longer a gate standing beside the root: it
  *produces* the root the components are derived from, so a caller cannot obtain
  an unvalidated one — the same check-and-use discipline the descriptor walk
  applies per component, applied to the root itself.
- **Snapshot containment is no longer re-derived from a name the child can
  move.** The workspace descriptor is captured before any scenario process is
  spawned, but the canonical root — and so the component list applied to that
  descriptor — was recomputed from the workspace *path* at assertion time, which
  runs after the child. With the workspace reached through a symlink the child
  repointed, containment was judged against one directory and applied to the
  descriptor of another: check and use naming different objects, the very split
  the descriptor exists to close, one level above the per-component handles that
  already close it within a walk. The canonical root is now pinned at capture
  time alongside the descriptor. 1.2.2 wrote the snapshot into the substituted
  directory in this case (verified by running it); pitty now refuses and writes
  nowhere — a deliberate, safer divergence from v1. The pin is on the *canonical*
  root, so the divergence covers a repoint that lands outside it; repointing
  `w -> real` to `w -> real/subdir` leaves the canonical root unchanged and is
  still written (1.2.2 and this build both exit 0 there). Neither this pin nor
  the per-component walk closes the path-mutation races recorded in
  `SECURITY.md`. The
  resolver allowed a `..` to cancel a missing component because `--update`
  creates the snapshot's parent directories — but it was never told whether this
  run was updating, so the exemption applied to verifying runs too. On a read
  nothing creates the component: `missing/../victim.snap` normalized onto
  `victim.snap`, an already-recorded neighbouring file, and the assertion
  **passed** by comparing the output against a file the scenario could never have
  opened. Exit 0 where 1.2.2 exited 1 with "not recorded". A false green is the
  worst failure direction for a testing tool. `resolve_write_path` now takes a
  `SnapshotAccess` (`Record`/`Verify`) and applies the exemption only when
  recording; `--update` still records through a missing component, so the v1
  behaviour the exemption protects is unchanged.
- **The `..` traversability check no longer fails open where `O_SEARCH` is
  absent.** On platforms without `O_SEARCH` (glibc Linux, a CI platform) the
  check fell back to `O_DIRECTORY`, which implies `O_RDONLY` and so asks for
  *read* permission where traversal requires *search*. It reports a `0600`
  directory as traversable while the kernel refuses `dir/..` through it with
  `EACCES` — reinstating the data-loss defect above on exactly those platforms.
  The fallback now stops predicting from a mode bit and **performs** the
  traversal, opening `dir/.`, which the kernel can only resolve by descending
  through `dir`. Every measured case matches the kernel or is stricter; the sole
  divergence is a `0100` directory, refused where the kernel would allow — a
  false refusal (a reported assertion failure), never a false pass. That
  direction is enforced, not assumed: such a directory classifies as *refused*
  and never as *absent*, since absent components are exempt under `--update`.
  Verified by execution on macOS with `O_SEARCH` forced off so the Linux arm runs
  here; the Linux behaviour itself was not run (no Linux host) and rests on the
  POSIX rule that resolving a component requires search permission on it.
- **A `..` across a directory with no search permission no longer destroys a real
  file.** The guard decided whether `..` could cancel a component by testing that
  component's attributes, and a `chmod 000` directory passes every such test: it
  *is* a directory, so `metadata().is_ok_and(|m| m.is_dir())` said the climb was
  sound. The kernel disagrees — resolving `locked/..` needs **search** permission
  on `locked` — so `file: locked/../victim.snap` under `--update` reported
  `passed`, exited 0, and truncated and rewrote `victim.snap`, a real unrelated
  file. 1.2.2 attempted the write, got `EACCES`, exited 1 and left the file
  intact.

  This was the third shape to defeat the same guard (a missing component, then a
  regular file, now an unsearchable directory), each fixed by testing one more
  attribute. The attribute list does not terminate — `noexec` mounts, dangling
  mount points, a dead NFS server and SELinux denials all make an
  attribute-perfect component untraversable — so the guard no longer inspects the
  component at all. It asks the kernel to open it for *search* (`O_SEARCH` where
  the platform provides it) and adopts the answer, which is the same question a
  `..` asks. `O_SEARCH` specifically and not `O_DIRECTORY`: the latter implies
  `O_RDONLY` and so requests the wrong permission, accepting a `0600` directory
  the kernel will not traverse and refusing a `0100` one it will (both measured).
  As with the other `..` refusals this is a failed assertion (exit 1, with a
  report), matching the class 1.2.2 put it in.
- **`expect_json` no longer asserts against the wrong JSON block.** Locating the
  trailing JSON in terminal output masked string literals with a scan that
  assumed the buffer began outside a string. Terminal output is arbitrary text
  with no obligation to balance its quotes, so a single stray `"` — in a
  warning, a log line, or wherever the trailing 64 KiB window happened to be
  cut — inverted that assumption for every byte after it: real braces were read
  as string data and real string bodies as structure. The result was silent and
  worst-case wrong. `{"phase":"old"}`, a warning containing one quote, then
  `{"phase":"new"}` asserted against **`old`** — a stale, older block — rather
  than failing. Extraction now scans for a candidate opening brace and derives
  string state forward from there, where being outside a string literal is
  guaranteed by the structure rather than assumed, so the surrounding text's
  quote parity and the window boundary cannot affect which block is chosen.
  The 8 KiB staleness guard that keeps a truncated final report from resolving
  to ancient log noise bounds where a block **ends**, not how large it may be:
  a report of any size within the 64 KiB tail window extracts as long as it
  closes near the tail. Extraction also now always returns the complete
  outermost block or nothing — a `{` appearing inside a string *value* no longer
  starves the search (a 175-byte document with ~150 such braces could fail to
  extract at all), and a deeply nested document no longer yields an inner
  fragment in place of its real root. A block nested deeper than the JSON parser
  accepts is likewise reported as "no valid JSON block" rather than resolving to
  the deepest inner portion that happens to parse on its own — an assertion
  would otherwise have run against a fragment while the real document went
  unevaluated. The distinction that makes this safe: a final block shown to be
  *malformed* still falls back to an earlier complete block as documented, while
  a block the scan *could not analyse* suppresses that fallback. That suppression
  is decided by containment rather than by position, so unrelated deep output
  earlier in the log no longer discards a valid report at the tail, and a stale
  block is no longer returned when the tail region is the part that was skipped.
  Multiple skipped stretches are tracked separately rather than as one span, so a
  report sitting in the gap between two of them is not refused on their account.
  Extraction cost is linear in the size of the output it scans, including on
  documents whose string values are dense with `{` characters. `SCHEMA.md` states
  the envelope.

  **v1 behavior change.** Because extraction no longer infers string context from
  the surrounding log text, a JSON-looking run inside quoted prose (for example
  `log: "{"status":"ok"}"`) is now extracted where 1.2.2 found nothing, so such a
  step can change from fail to pass. This applies **only when the output contains
  no real JSON block at all**: a run inside quoted prose is ranked below any block
  outside it, so it cannot displace a genuine report, even one printed earlier in
  the output. The quoted-prose judgement honours backslash escapes, matching
  1.2.2: inside a quoted run an escaped quote does not end it, so a logged
  message that quotes something itself (`log: "he said \" then stopped"`) no
  longer demotes a real report printed alongside it. Outside a quoted run a
  backslash escapes nothing, also as in 1.2.2. An unquoted later block still wins on tail position, as in 1.2.2. This is the same rule change that fixes the bugs above and cannot be
  separated from them: 1.2.2 classified a `{` as string data by counting double
  quotes from the start of the buffer, and the brace sits at odd parity both here
  and in the cases where a stray or escaped quote hid a real report or returned a
  stale one. Keeping 1.2.2's verdict on this input would mean keeping those.
- **`expect_json` can now assert that a value is `null`.** `equals: null` was
  dropped while reading the scenario, so the check silently did not exist. The
  step was not merely ineffective: because the dropped key also went uncounted
  by the "exactly one of `equals`/`contains`/`exists`" guard, `equals: null`
  written *alone* left a step with no assertion at all, and written alongside
  `contains` or `exists` slipped past the guard that should have rejected it.
  Either way the scenario reported a pass having verified nothing. An explicit
  `null` is now distinguished from an omitted key and asserts that the value is
  JSON `null`.
- **Scenario-level `env` values are now `${var}`-expanded.** `SCHEMA.md` has
  always listed them as an expansion site, but `Workspace::prepare` copied them
  verbatim while the spawn-level `env` half of the very same merge expanded
  normally, so two identically-written values resolved differently based only on
  which map declared them. A `${who}` in a scenario-level `env` value reached
  the child as the literal text `${who}`, with no error or warning — typically
  surfacing much later as a path that does not exist or a token that does not
  authenticate. A `secret: true` variable referenced this way was doubly wrong:
  the secret never reached the child, and the literal placeholder that did was
  not a secret, so nothing was masked. Both `env` levels now share one
  expansion implementation, so they cannot drift apart again. **Behaviour
  change:** a scenario that relied on a literal `${...}` surviving in a
  scenario-level `env` value must now write `$${...}`. `env` values still
  cannot reference each other — expansion sees `variables` and the parent
  environment only, never a sibling `env` key.
- **Per-scenario logs no longer overwrite one another or lose output.** The log
  path was derived from the scenario's `name:` alone and its body from whichever
  PTY session happened to be current at the end of the run, so three separate
  classes of diagnostics were silently destroyed: every matrix cell wrote the
  same `logs/<name>.log` (only the last cell's output survived); two scenario
  *files* declaring the same `name:` overwrote each other in a directory run; and
  within one run only the final `spawn`'s buffer was written, while a scenario
  that never spawned wrote no log at all — including when it failed. The log is
  the artifact a developer opens after a failure, so in each case the run whose
  output was needed could be the one erased. A log's name is now built from the
  scenario's `name:`, prefixed by the scenario **file** stem when it differs from
  the name, and suffixed by the matrix cell's coordinates — so a scenario
  following the usual convention (`echo-flow.yaml` declaring `name: echo-flow`)
  keeps its existing `logs/<scenario>.log` name, while colliding files and matrix
  cells each get their own. Two genuinely different runs that still reduce to one
  name get a numeric suffix (`<stem>.2.log`), whereas repeating the *same*
  scenario — another `pitty run`, a `pitty bench`, or a re-run of a whole suite
  into an existing `logs/` — overwrites its own log rather than accumulating one
  file per invocation. Ownership is recorded as a short digest in each log's
  header, so it holds across processes rather than only within one; a log left by
  an older pitty carries no digest and is preserved rather than overwritten. A
  log is now written for every run, including a spawn-less one (whose output
  section reads `(no process spawned)`), and every session's output is retained,
  each under a `--- session N ---` banner. Two further paths into the same class
  are closed: `a.yaml` and `a.yml` in one directory are distinct scenarios and no
  longer share a log (the alternative spelling is marked, so the canonical
  `.yaml` keeps its plain name), and a log name that would exceed the
  filesystem's component limit is now shortened with a digest instead of failing
  to write — an over-long matrix axis value previously lost its log entirely.
- **The log writer no longer follows symlinks out of the workspace.** The probe
  that decided a candidate name was free used `Path::exists()`, which follows
  links, so a **dangling** symlink read as free — and the open that followed had
  no `O_NOFOLLOW`, so it created and filled the link's target. A link planted at
  `logs/<name>.log` therefore redirected a scenario's full terminal output
  anywhere the runner could write. The probe now stats without following (a
  planted link counts as occupied and is suffixed past), the open uses
  `O_NOFOLLOW` so a link planted after the name is chosen still cannot redirect
  it, and a `logs/` directory that is itself a symlink is refused before anything
  is written under it. This is the log-sink counterpart of the snapshot fix in
  #37, and it runs through the shared `safepath` traversal rather than a second,
  parallel implementation: the anchor descriptor is captured before the first
  spawn, `logs/` is opened from it with `openat(O_DIRECTORY | O_NOFOLLOW)`, and
  every probe, read, and write beneath it is descriptor-relative — so the read
  half refuses the same links the write half does. A `logs/` directory pitty
  creates is now `0700` rather than umask-default.

  Two follow-ups closed the same hole from the other side. `Path::parent()` of a
  bare filename is `Some("")`, not `None`, so `pitty run s.yaml` from inside a
  scenario's own directory passed an *empty* base directory: the pre-spawn
  descriptor could not be opened, and the writer silently fell back to a
  path-based write that followed a symlinked `logs/`. The empty path is now
  resolved to the current directory. And that fallback is gone entirely — when
  the descriptor cannot be captured, the log is skipped with a warning on stderr
  instead of being written through an unprotected path. The run's verdict is
  unchanged either way; a diagnostics sink still never turns a passing scenario
  red.
- **An interrupted run no longer poisons log names.** The claim sidecar is
  created before its tag is written, so a run that died in between — or whose tag
  write failed on a full disk — left an empty record that refused the name to
  every future run; one bad run could poison all 1000 candidates permanently. An
  empty sidecar with no log beside it is now recognized as residue and reclaimed,
  a failed tag write removes the record it just created, and a log write that
  fails removes the claim it took so the name does not stay owned with nothing
  behind it.
- **A failed second `spawn` no longer duplicates the first session in the log.**
  The retired session's buffer was snapshotted at retirement and, because the
  session stayed in place when the next spawn failed, snapshotted again by the
  final teardown — so the log showed one session twice. The session is now moved
  out when it is retired, so its output reaches the log exactly once either way.
- **`expect_json`'s `equals: null` no longer rejects a scenario 1.2.2 ran.**
  Making `equals: null` assertable (its real fix) also made it count toward the
  "exactly one of equals/contains/exists" guard — so `equals: null` paired with
  `contains`, or with `exists`, went from *executing* (exit 1 and 0 against the
  1.2.2 binary) to a Scenario error (exit 2). `COMPATIBILITY.md` forbids
  tightening validation so a previously valid scenario becomes an error, and
  lists no exception for "the old behaviour verified nothing" — whether anyone
  ought to have relied on it is not the criterion, since a pipeline can route on
  exit 1 versus exit 2 without depending on the assertion's meaning. Those two
  shapes therefore keep 1.2.2's behaviour until 2.0: the null `equals` is dropped
  and the other check runs. Everything 1.2.2 *rejected* is still rejected,
  including `equals: null` alongside both `contains` and `exists`, and
  `equals: null` on its own still asserts null — a pure loosening, since 1.2.2
  rejected it.
- **A half-written claim record no longer lets another scenario take over an
  existing log.** A record left malformed by a run killed mid-rewrite was treated
  as reclaimable by any identity. Two scenario files whose names sanitize to the
  same stem (`a?.yaml` and `a*.yaml` both reduce to `a_`) could therefore collide:
  the second adopted the first's abandoned record and truncated its log. The rule
  now turns on whether a log sits beside the record. With none, it is plainly
  abandoned and is reclaimed, so an interrupted run never poisons its own name.
  With a log present it is left alone and the run takes a suffixed name — because
  a malformed record carries no ownership information, which makes "an interrupted
  run of this identity" and "another identity's log" indistinguishable, and the
  two possible mistakes are not symmetric: leaving a log unreused keeps the file
  and its contents intact and recoverable, while overwriting destroys diagnostics
  that exist nowhere else.
- **`duration_ms` still measures the scenario, not its teardown.** Moving session
  teardown before the log write (so the log records the true final status) also
  moved the point at which the run's clock was read, silently folding cleanup
  into a published report field: a scenario leaving a shell alive went from ~3ms
  to ~64ms, and a stalled ConPTY teardown can add seconds. `BenchReport` derives
  its statistics straight from this value, so every recorded threshold and
  historical comparison would have shifted. The clock is now read before teardown
  begins; the ordering that made the log correct is unchanged. `COMPATIBILITY.md`
  treats a change in a report field's meaning as a major change, and this was one.
- **A failed run is no longer logged as having passed.** The log was assembled
  and written before the session teardown that finalizes the run, so a scenario
  ending in a hard fault — a spawn command that could not be parsed, say —
  recorded `# status: Passed`, and a chatty child's final output never reached
  the file because the drain had not happened yet. Teardown now runs first, so
  the log records the true final status (`Errored`, with the fault under
  `# error:`) and, **when teardown completes cleanly**, the child's full output.
  If teardown itself stalls, the reader may still be draining when the buffer is
  snapshotted, so the tail can be short — that case is reported as a stalled
  teardown rather than silently promising completeness. A diagnostic that says a failed run passed
  is worse than none at all.
- **Taking a log name excludes other writers, and a cached name is
  re-validated.** The free-name probe and the file creation were separate steps,
  so two concurrent processes could both see a candidate free and the second
  would truncate the first's log. A name is now taken by exclusively creating its
  claim sidecar (`O_EXCL`) *and* holding an advisory `flock` on it across the
  write. The exclusive create alone was not enough: two runs of the **same**
  scenario share a tag, so each read its own tag back, concluded the name was
  theirs, and both wrote — interleaving content. The lock separates the two jobs
  that file does, durable ownership (its content) from run-time exclusion (the
  lock). On Unix only; Windows has neither primitive, and SECURITY.md now says so
  explicitly rather than implying a guarantee that does not hold there. Separately, the in-process memo keyed on a path *string*, so if a base
  directory was replaced between iterations a cached name could be reused in a
  different tree without checking; cached names are now re-validated against the
  directory actually in front of the writer, so an unclaimed log there is
  preserved rather than overwritten.
- **Losing `logs/.pitty-log-salt` no longer orphans every existing log.** The
  ownership tag in each sidecar was keyed by the directory's salt, and so was the
  discriminator in the filename — so if that one dotfile went missing, every
  scenario in the directory picked a new name and stranded its previous log. The
  two values are now separated by what they protect: the **filename**
  discriminator stays salted, because a directory listing is a far wider surface
  than a file's contents; the **sidecar** record is salt-free, because it is what
  ownership depends on and must survive. (An earlier revision justified the
  unsalted sidecar by claiming its reader could already read the log anyway —
  that was wrong, since the log body is masked and the record was not. The record
  stores the *masked* identity for that reason.) A run also
  reclaims a log it owns under a different name, so a regenerated salt changes at
  most the name of a *new* log. The salt is now created only when a filename
  actually needs a discriminator, so a suite with no secret in any identity never
  has one to lose. A salt that exists but cannot be read is left alone rather than
  overwritten, since replacing it would rename every discriminated log; one that
  is ill-formed or world-readable is replaced, since pitty never wrote it and it
  therefore names no existing log. Note this covers salt loss specifically —
  restoring only `logs/*.log` also drops the `.claim` sidecars, and logs without
  those are left alone rather than reused (see README).
- **A log's ownership record no longer collides with secret masking, and no
  longer reveals anything.** A log carries a record identifying the scenario that
  wrote it, so a later process reuses that scenario's own file instead of piling
  up numbered copies. Getting that record right took several attempts, and the
  dead ends are worth recording because each looked correct in isolation:

  - Stored **inside the log body**, it was prey to masking — blind *substring*
    replacement, so a secret as ordinary as `"2"` rewrote part of a hex tag (and a
    secret of `id` rewrote the `# id: ` key). No process then recognized its own
    log, and the accumulation returned.
  - Mixing the **secret list into a digest** stopped the tag from *equalling* a
    secret but not from being corrupted by one, and made things worse twice over:
    an unsalted digest over low-entropy input is an offline oracle that recovers a
    4-digit PIN in milliseconds, and the tag changed whenever a secret's value
    changed, so rotating a credential silently orphaned a scenario's log.
  - Keying the digest with a **per-directory salt** closed the oracle only against
    someone holding a filename. It did not close it against someone holding the
    `logs/` directory, because the salt is a dotfile *in that directory* — and it
    made ownership depend on a file that can go missing, orphaning every log at
    once when it did.

  What ships: ownership lives in an unmasked sidecar beside the log
  (`.<log name>.claim`) holding the scenario's identity **after masking**, plus a
  random per-log token. The masked identity contains no secret, so there is
  nothing to brute-force; it is not a digest, so nothing can be corrupted; it
  involves no salt, so nothing is orphaned when one is lost; and the token keeps
  two cells whose masked identities are equal from colliding onto one file.
  Identity parts are length-prefixed and the record is terminated, so a truncated
  or hand-written file is rejected rather than mis-parsed as somebody else's.

  Known limit: two matrix cells differing only *inside* a secret are
  indistinguishable on disk. They always get separate logs, but which one a given
  cell reuses later is not guaranteed — binding them would need a value the cell
  can reproduce and a reader of `logs/` cannot invert, which does not exist for a
  low-entropy secret. The **filename** discriminator is still salted, since a
  directory listing is a far wider surface than a file's contents. That keying
  protects only a listing seen *without* the files: the salt is itself a dotfile
  in `logs/`, so anyone holding an archive of the directory holds the salt and can
  compute the discriminator for each candidate secret. It is also checked only for
  shape and mode, which are not provenance, so a planted salt passes.
- **Secrets no longer reach a log's filename through the file stem or matrix
  coordinates.** The filename was built from unmasked identity material, so a
  secret used as a matrix axis value (or in a scenario file's name) was readable
  from a plain directory listing even though every byte inside the file was
  masked. Those components are now masked before the name is built. Because
  masking is many-to-one, a name whose masking removed text gains a short digest
  suffix, so two cells differing only inside a secret keep separate logs rather
  than collapsing onto one and overwriting each other; a filename containing no
  secret is unchanged. A secret in the scenario's own `name:` predates this
  naming scheme and remains tracked as issue #43.
- **Logs are `0600` before any content reaches them.** The writer created the
  file, wrote every byte, and only then applied the mode, so under the usual
  `umask 022` a new log — holding the full terminal output — was `0644` for the
  entire duration of the write, and a log an earlier pitty left at `0644` stayed
  readable while new content landed in it. The open now requests `0600`, the
  mode is reapplied on the open descriptor (so a concurrent swap cannot redirect
  it), and the file is truncated only once it is restrictive — the same ordering
  `expect_snapshot` recording uses. `SECURITY.md` stated this guarantee for logs
  before the code provided it.
- **An exhausted set of log names is reported instead of overwriting one.** After
  trying every suffix up to the bound, the writer returned the *unsuffixed* path
  even though another scenario's log occupied it, destroying that log — the exact
  failure the naming scheme exists to prevent, and a contradiction of the
  README's promise that a colliding run is suffixed rather than replacing an
  earlier log. Exhaustion is now a reported log-write failure on stderr.
- **A log that cannot be written is no longer silent.** The result of the log
  write was discarded, so the whole failure class — permissions, a full disk, a
  name too long for the filesystem — vanished: `logs/` was simply empty with no
  error and no warning. Such a failure now prints a warning on stderr. It
  deliberately does *not* change the run's verdict: the log is a diagnostics
  sink rather than an assertion, and turning an environment problem into a red
  test would misreport the program under test. stdout stays a clean JSON report
  so `--json` consumers are unaffected.
- **Snapshot write containment no longer fails open on a dangling symlink.**
  `Path::canonicalize` reports `ENOENT` for a symlink whose target does not
  exist, and the containment check read that as "this file does not exist yet",
  so a planted `x.snap -> /outside/PWNED` let `--update` write outside the
  workspace while reporting a passing assertion. The security outcome therefore
  inverted on whether the target had been pre-created — the one condition an
  attacker controls. Every component a scenario's `file:` names is now checked
  with `symlink_metadata`, which does not follow links, and a symlink is refused
  whether it is dangling or resolvable and whether it points outside the
  workspace or inside it. The directory-component form (`out/x.snap` with `out`
  a dangling link), previously stopped only by `create_dir_all` returning
  `EEXIST`, is now refused by the check itself. On Unix the write additionally
  descends one component at a time with `openat(O_DIRECTORY | O_NOFOLLOW)`,
  creating the file relative to the resulting directory descriptor, closing the
  check-then-write race in which the scenario's own child process plants a link
  after resolution. Because each handle names a directory *object* rather than a
  path, this covers a swapped *intermediate* directory (`snapshots/` replaced by
  a link out of the workspace) and not only the final component — a plain
  `O_NOFOLLOW` on a path-based open constrains the last component alone and
  would have followed such a link. Windows has no equivalent and relies on the
  resolver's check.
- **The snapshot recorder no longer creates directories outside the workspace.**
  Containment was judged on the *normalized* destination while the recorder
  walked the *raw* `file:` components, so the two could disagree: a path such as
  `../new-private-dir/../w/out.snap` normalizes back inside the workspace (and
  rightly passed containment) but stepped outside on the way, where the recorder
  created — and chmod-ed `0700` — a directory in the workspace's parent. The
  resolver now returns the very sequence it validated: workspace-relative,
  normalized, and free of `..`. An absolute `file:` inside the workspace reduces
  to that same relative sequence instead of bypassing the per-component walk;
  one outside the workspace is refused, as is a path naming the workspace root
  itself.
- **A hard-linked snapshot no longer bypasses containment.** Every symlink
  defense missed this, by construction: a hard link is not a symlink but the
  file itself under a second name, so `O_NOFOLLOW` and the per-component walk
  both succeed on one. An attacker who linked an external `victim.snap` to a
  name inside the workspace therefore got both halves — the comparison read
  returned the external file's contents, so matching them to the program's
  output made the assertion **pass** with no snapshot in the workspace at all,
  and `--update` truncated and rewrote that external inode and chmod-ed it to
  `0600`. Both the read and the write now `fstat` the descriptor they opened and
  refuse a link count above one. The check is on the descriptor rather than the
  path, so the inode checked is necessarily the inode used. What remains, and is
  documented rather than papered over: a link added *after* that check lets its
  creator read what pitty subsequently writes — unpreventable at this layer, but
  unable to redirect a write or make a planted file pass. Ordinary files are
  unaffected, including those in a git checkout, which have a single link.
- **A planted snapshot can no longer satisfy an assertion.** The `--update`
  write went through the protected traversal, but the *comparison read* still
  used the display path, which resolves symlinks. The two halves could therefore
  name different files: with `snapshots/` swapped for a link to a directory the
  attacker controlled, a planted `x.snap` whose contents matched the program's
  output made `expect_snapshot` **pass** while the real workspace contained no
  snapshot at all — a green test verifying an attacker-supplied file, which is
  strictly worse than a failure. Both halves now share one traversal, so the read
  refuses exactly what the write refuses and the two always name the same file.
- **Failing to capture the workspace descriptor is now fatal rather than a
  silent downgrade.** The capture used `.ok()`, so any failure left `None` and
  the recorder fell back to a symlink-following path open. That made the
  protection fail-open and reachable by an attacker who could induce the failure:
  a workspace that is searchable and writable but not readable lets the scenario
  run while the capture fails, after which renaming the workspace re-opens the
  redirect the descriptor exists to prevent. A run that records a snapshot now
  fails with a process error naming the directory, and the unprotected opener has
  been removed rather than left unused.
- **A snapshot path through an in-workspace symlink works again.** Hardening the
  snapshot write against symlink races started refusing *every* symlinked
  component, wherever it pointed. That broke an ordinary repository layout — a
  workspace containing `real/` and `snapshots -> real`, with
  `file: snapshots/out.snap`, passed on 1.2.2 and became a scenario error — and a
  shared fixture directory is a normal reason to have such a link. Refusing it
  changed the meaning of an existing field and tightened validation, which
  [`COMPATIBILITY.md`](COMPATIBILITY.md) forbids within `1.x`. Only a **dangling**
  link is refused now, which is the one case containment cannot judge for itself
  (`canonicalize` reports `ENOENT` for it, so a planted
  `x.snap -> /outside/PWNED` would otherwise read as a path that merely does not
  exist yet). A link that resolves is judged by containment: inside the
  workspace it is allowed, outside it is still refused.
  A **dangling** link is likewise judged by where it leads rather than refused
  outright, because "unresolvable" conflated two different things: `canonicalize`
  answers `ENOENT` identically for `out.snap -> real/out.snap` (inside, the
  target simply not recorded yet — which recorded fine on 1.2.2, exit 0) and for
  `out.snap -> ../outside/PWNED` (an escape). The chain is now followed with
  `read_link`, a relative target resolved against the link's own directory as the
  kernel does, and the destination judged by the same containment rule as any
  other path: inside is allowed, outside is refused. A chain is followed to its
  end, so an escape cannot be laundered through an intermediate hop, and the walk
  is hop-bounded so a cycle (`a -> b -> a`, which never reaches `canonicalize`
  and so cannot report `ELOOP`) is refused rather than spun on. The accepted
  destination is what the recorder walks — handing it the link's own name instead
  made the writer refuse, with `ELOOP`, the very path the resolver had approved.
  A link in the *middle* of the path keeps the components that follow it:
  `link -> missing-dir` with `file: link/out.snap` records at
  `missing-dir/out.snap`, where dropping the tail had silently written the
  snapshot into a *file* named `missing-dir` while the `out.snap` the scenario
  asked for never existed — so the run reported success and a later verifying run
  found nothing. Containment is judged on the destination and the tail together,
  because either half can move the result across the boundary: a link pointing
  outside with a tail that climbs back in lands inside, and a link pointing
  inside with a tail that climbs out lands outside. Both are normalized the same
  way the rest of the resolver normalizes — except that a `..` may only cancel a
  component that **exists**. The kernel resolves left to right, so
  `link/../victim.snap` where `link -> missing` fails at `missing` and never
  reaches `victim.snap`; eliminating the pair lexically resolved that
  unreachable path onto a real neighbouring file and had `--update` truncate and
  rewrite it. Not a containment escape — the file is inside the workspace — but
  silent data loss where 1.2.2 left the file untouched, so such a path is now
  refused. `..` across a directory that *does* exist is unaffected. The hop bound
  counts hops rather than loop iterations, so a legitimate chain of exactly the
  limit resolves instead of being refused one short, and the limit itself is the
  platform's own (32, macOS `MAXSYMLINKS`, confirmed by execution) rather than an
  arbitrary 16 that refused 17-hop chains the OS would have followed.
  This does **not** reopen the check-then-use race that motivated the blanket
  refusal. The recorder never traverses the link's *name* — the sequence walked
  is `real/out.snap`, not `snapshots/out.snap` — and a child that re-points the
  link afterwards changes nothing, because that name is never opened again. A
  child that swaps the *canonical* component instead is refused by the descriptor
  walk, which opens every component `O_NOFOLLOW`. Both cases were confirmed by
  running the syscalls rather than reasoned about.
- **A FIFO left at a snapshot path no longer hangs the run.** Opening a FIFO for
  reading blocks until a writer appears, and for writing until a reader does —
  forever, if neither comes. A scenario that left `mkfifo out.snap` behind
  therefore wedged the snapshot step indefinitely: not a failure, not a timeout,
  a stuck process that sat until CI's global timeout with no diagnostic, and a
  state no result type could represent because the open never returned. Both
  opens now pass `O_NONBLOCK` so the open returns whatever the file type is, then
  `fstat` the **descriptor** and refuse anything that is not a regular file — a
  FIFO, directory, socket or device node alike, since only a plain file can hold
  a record pitty wrote. The flag is cleared once the type is known, so a later
  read cannot mistake `EAGAIN` for end-of-file.
- **Windows no longer creates directories outside the workspace for a `..`
  detour.** Containment is judged on the normalized destination, but the target
  also kept the `file:` value as written, and the Windows branch — which has no
  `openat` to walk with — passed that raw string to `create_dir_all`. So
  `../outside/new/../../w/out.snap` normalized back inside the workspace and
  rightly passed containment, while creating `outside/new` outside it on the way.
  This was the same `..` escape fixed for Unix earlier, still live on the branch
  the component-sequence fix never reached. The non-Unix read and write now
  rebuild the path from the workspace root and the validated components, so the
  guarantee that a detour creates nothing outside the workspace holds on every
  platform rather than only on Unix.
- **A restrictive umask no longer breaks nested snapshot paths, and setting the
  new directory's mode can no longer touch an inode outside the workspace.** The
  traversal created each missing directory with `mkdirat(0700)` and set the mode
  afterwards, but a umask masks bits off that argument: under `umask 0400` the
  directory landed at `0300`, and the `O_RDONLY | O_DIRECTORY` open of it then
  failed with `EACCES` before the repair could run, so
  `file: snaps/deep/out.snap` reported "cannot write snapshot … Permission
  denied". Moving the repair *before* the open fixed availability but introduced
  a worse problem, because a pre-open repair has only the name to work with: a
  `mkdirat` returning success does not prove the entry is still the same object
  at the next syscall, and the scenario's child can `rmdir` the new directory and
  leave a **hard link to an external file** at that name. `AT_SYMLINK_NOFOLLOW`
  does not help — a hard link is not a symlink — so the chmod would have set an
  inode outside the workspace to `0700`.
  Both are now fixed by one change: directories are opened with **search-only**
  access where the platform provides it (`O_SEARCH`), and the mode is set on the
  resulting **descriptor**. Search-only access opens a directory the umask left
  unreadable, so the open can come first; and because the descriptor is the
  object, `fchmod` cannot reach anything else, while `O_DIRECTORY` rejects a
  swapped hard link with `ENOTDIR` before any mode change is attempted. That an
  `O_SEARCH` descriptor for a `0300` directory accepts `fchmod` and stays usable
  for further traversal was confirmed by running the syscalls, not inferred.
  Where search-only access is unavailable, such a run now fails rather than
  chmod-ing an unverified name.
- **The shared chmod helper no longer takes a name at all.** It used to be a
  `fchmodat(..., AT_SYMLINK_NOFOLLOW)` that, on `ENOTSUP`, silently retried
  *without* the flag — a symlink-following chmod, the same fail-open shape
  removed elsewhere — and that documented to callers that a hard link defeats the
  flag regardless, requiring them to check the link count first. No caller did,
  which is how the hazard survived: a requirement that depends on every caller
  remembering an unenforced step is not a contract. The helper now opens the
  directory and sets the mode on the resulting descriptor, so the safe sequence
  is the only sequence and there is nothing left for a caller to get wrong.
- **A workspace that is searchable but not readable no longer fails a
  snapshot-free run.** Making the capture mandatory overshot: it was demanded at
  preparation time, on the reasoning that no legitimate workspace could fail to
  open and that one which did could not host a `spawn` either. Both halves were
  wrong — entering a directory needs execute/search permission, not read, so a
  `0300` workspace runs commands perfectly well — and a scenario containing only
  `spawn` and `expect` died before its first step. Two changes fix it without
  weakening containment. The descriptor is still requested once before any child
  is spawned, but a failure is now reported only when a snapshot actually needs
  it, so a scenario with no `expect_snapshot` never asks; and on platforms with
  `O_SEARCH` (POSIX 2008; macOS) the capture requests search-only access, which
  is all a traversal needs, so even a `0300` workspace records snapshots
  normally. Linux's `O_PATH` is the natural equivalent there and `open(2)` does
  allow an `O_PATH` descriptor as the `dirfd` of the `*at()` calls this traversal
  uses; it is not adopted only because it could not be **verified by execution**
  here (no Linux host was available, and the `O_SEARCH` behavior was confirmed by
  running the syscalls). A source `TODO` in `safepath::open_dir_fd` names the
  checks to run. Fail-closed is unchanged where it matters: without a descriptor,
  no snapshot target is produced at all.
- **Renaming the workspace can no longer redirect a snapshot write.** The
  recorder anchored its traversal on the workspace *path*, which is re-resolved
  at snapshot time, so a scenario child running
  `mv "$PWD" "$PWD.old" && ln -s /tmp/victim "$PWD"` made that path name a
  different directory and the snapshot landed in the attacker's. The premise
  that nothing at or above the workspace is plantable holds for the path but not
  for what it resolves to later. The workspace directory is now captured as a
  file descriptor in `Workspace::prepare`, before any child is spawned, and the
  walk starts there; a descriptor cannot be retargeted by a later rename or
  symlink swap. As a side effect, components *above* the workspace are no longer
  traversed at all, which also removes the macOS `/var -> /private/var` caveat
  the previous approach had to tolerate.
- **Recorded snapshots are no longer world-readable.** Snapshot content is
  written unmasked by design and may contain secrets, yet it landed at the
  umask default (`0644` on a typical runner) while the *masked* logs were
  correctly `0600`. Snapshots are now `0600` on Unix, matching the protection
  logs already had. The mode is set on the descriptor *before* the file is
  truncated or any content is written, so refreshing a snapshot an earlier pitty
  left at `0644` repairs its mode without exposing the new content in between —
  applying the mode only after the write would have left the new secret readable
  to every local user for the length of it. Windows continues to use the runner
  user's default ACLs.
- **Snapshot recording no longer rewrites the mode of a directory it did not
  create.** The parent of a snapshot was chmod-ed to `0700` unconditionally, but
  for the common `file: out.snap` that parent is the scenario's own directory —
  the user's repository checkout. Recording into a group-shared `0770` checkout
  silently reduced it to `0700`, locking out other users and later CI steps, and
  in a directory that is writable but not owned the unnecessary `chmod` failed
  outright and took the recording down with it. Directories pitty creates are
  still `0700` (requested at `mkdirat` and reasserted with `fchmod`, so the umask
  cannot widen them); an existing directory's mode is now left alone.
- **GitHub annotations no longer corrupt machine-readable stdout.** `::error`
  and `::warning` workflow commands were printed to stdout, the same stream
  that carries `run`'s JSON report and `matrix`/`bench` output under `--json`.
  Because annotations auto-enable on a runner (`GITHUB_ACTIONS=true`) and are
  emitted only for failures, `pitty matrix --json | jq` worked on a green run
  and failed to parse on a red one — breaking exactly where `--json` is most
  used, and masking the failure it was meant to report. Annotations now go to
  stderr, which GitHub parses for workflow commands just as it does stdout, so
  the inline run/PR annotations are unchanged while stdout stays a single
  parseable JSON value.
- **An explicit action `version:` pin is now honored over a pre-existing
  binary.** The composite action's installer short-circuited on any `pitty`
  already on `PATH` before it read the `version:` input, so a caller pinning a
  ref could silently run a different binary — and skip the checksum-verified
  download with it. The reuse branch now applies only when no `version:` is
  pinned; a pinned ref is always installed, and the step logs a notice when it
  overrides a binary already on `PATH`. Runs that omit `version:` still reuse a
  pre-provisioned `pitty` without downloading anything.
- **The release workflow's least-privilege gate can now actually fail.**
  `release_grants_only_contents_write` asserted that the substring
  `contents: write` was *present*, which the very line it bounds always
  satisfies — so adding `id-token: write`, `packages: write`, or any other
  scope to the release token left the gate green. It now parses the top-level
  `permissions:` block and asserts the grant set is exactly
  `{contents: write}`.

## [1.2.2] - 2026-08-02

### Added

- **CI gates the flake package.** A `nix-build` job runs `nix build .#default`
  and smoke-tests the resulting binary. CI previously used Nix only as a
  toolchain provider (`nix develop --command cargo ...`) and never evaluated
  `nix/package.nix`, which is how the stale `cargoHash` fixed in this release
  shipped undetected in two of them. `just nix-build` reproduces the gate
  locally.

### Changed

- **YAML structures are the documented form for JSON expectations.** The README
  now writes step examples in block YAML and shows `expect_json`'s `equals`
  taking a nested YAML map/list, which deserializes to the equivalent typed
  JSON value for whole-structure equality. The nested form was already
  accepted; it is now documented in the README and `SCHEMA.md` and locked in by
  a parser test.

### Fixed

- **The published JSON schema now agrees with pitty on what it accepts.** Three
  divergences let the schema and the runner disagree, defeating the very
  editor-side checking `SCHEMA.md` recommends. A misspelled step name (`expct:`)
  validated green and then exited 2, because `definitions.step` listed its
  properties but never closed the map; it now sets `additionalProperties:
  false`. An `expect_json` with zero or two of `equals`/`contains`/`exists` also
  validated green and exited 2, because the "exactly one of" rule was prose
  only; it is now encoded as a `oneOf` over the three check fields. In the other
  direction, `source: Output` (or `OUTPUT`, `"  output  "`, `""`) runs correctly
  but the schema's exact-match `enum` rejected it; the schema now mirrors the
  deserializer's trim-and-lowercase normalization, the way `key` already did. No
  scenario changes meaning — only the schema moved, which keeps the v1 promise
  that a scenario valid under `1.0.0` stays valid.
- **The schema loads in editors again.** The `key` pattern used an inline `(?i)`
  flag, which is a syntax error in the ECMA-262 regex dialect draft-07
  specifies and that YAML language servers use, so a strict validator refused
  the whole schema. Both case-insensitive patterns now spell the folding out as
  two-case character classes. `tests/schema_contract.rs` compiles the shipped
  bytes with a real draft-07 validator and checks a table of scenarios reaches
  the same verdict under the schema as under pitty, so this class of drift fails
  the build instead of shipping.
- **Nix flake builds work again.** `nix/package.nix` carried a stale
  `cargoHash`, so `nix build github:kexi/pitty` and `nix profile install
  github:kexi/pitty` failed at the vendor-staging fixed-output derivation
  throughout v1.2.0 and v1.2.1. nixpkgs' vendor staging hashes `Cargo.lock`
  itself, so the release commits that bumped only the `pitty` version line
  rotated the hash without touching a single dependency.

## [1.2.1] - 2026-06-08

### Added

- **Marketplace-backed minor release refs.** Release automation now moves the
  floating minor tag (for example `v1.2`) alongside the floating major tag on
  future patch-tag releases and publishes matching prebuilt assets, so GitHub
  Marketplace/Actions users can pin either `@v1` or a minor line without losing
  the fast path.
- **Action-ref-aligned installs.** When the composite action's `version` input
  is omitted, the installer now follows the ref used in `uses: kexi/pitty@...`
  instead of always installing from `v1`.

## [1.2.0] - 2026-06-07

### Added

- **Native Windows support.** CI now runs on `windows-latest`, compiles and tests
  the Windows backend, and dogfoods a `cmd.exe` scenario through ConPTY.
- **Windows prebuilt assets.** Release automation now publishes Windows X64
  tarballs alongside Linux X64/ARM64 and macOS ARM64, and the composite action
  installs `pitty.exe` from the prebuilt fast path when available.
- **Pinned action verification.** CI runs `pinact verify` so GitHub Actions pins
  and version comments stay enforceable.

### Changed

- **Release asset names now use GitHub runner labels.** Archives are named with
  `RUNNER_OS`/`RUNNER_ARCH` values (for example `Windows-X64` and `macOS-ARM64`)
  to match what the composite action can download on each runner.

### Fixed

- **PTY shutdown on Windows.** `PtySession` now closes the master and writer
  handles before joining the reader thread, avoiding a ConPTY teardown hang.

## [1.1.0] - 2026-06-07

### Added

- **Prebuilt-binary release automation.** A tag-push-triggered workflow
  ([`.github/workflows/release.yml`](.github/workflows/release.yml)) builds
  `pitty` for three `OS × arch` targets (Linux x86_64/aarch64, macOS arm64) and
  uploads each as `pitty-<ref>-<os>-<arch>.tar.gz` with a
  `.sha256` checksum to the GitHub Release. (macOS Intel is served by the
  composite action's `cargo install` fallback; GitHub's macos-13 runners were too
  unreliably scheduled to gate a release on.) The os/arch in the asset name use
  the raw `uname -s`/`uname -m` values the composite action keys on, so the
  action's fast path now finds a prebuilt binary instead of always building from
  source. A contract test
  ([`tests/release_asset_name_contract.rs`](tests/release_asset_name_contract.rs))
  pins the release asset names to what `action.yml` downloads.
- **Floating `v1` major tag.** Each `v1.x.y` tag push force-moves the `v1` tag to
  the release commit and publishes a parallel set of `pitty-v1-<os>-<arch>.tar.gz`
  assets, so `uses: kexi/pitty@v1` both resolves the action ref and gets a
  prebuilt binary. The composite action's default `version` input is now `v1`.

## [1.0.0] - 2026-06-06

First stable release. The scenario input format and the report JSON are now
contracts (see [`COMPATIBILITY.md`](COMPATIBILITY.md) and [`SCHEMA.md`](SCHEMA.md)).

### Added

- **Stable scenario format.** The YAML scenario format is specified in
  [`SCHEMA.md`](SCHEMA.md), with a hand-written JSON Schema at
  [`schema/pitty-scenario-v1.json`](schema/pitty-scenario-v1.json) for
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
    `uses: kexi/pitty@v1`.

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
