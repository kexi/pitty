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

Within that model, the files pitty itself writes are still restricted. The
restrictions below are stated for a filesystem that is not being mutated
underneath pitty as it works; a child process that actively races the resolver
can still defeat some of them, and exactly which is set out in *Residual: the
path-mutation race*. **If you are deciding whether pitty is safe to point at an
untrusted program, read that section — it, not this one, states the limit.**

Snapshot recording under `--update` is confined to the workspace directory. A symlink on
the path is judged by where it leads: one resolving *inside* the workspace is
followed safely (an in-repository `snapshots -> real` is an ordinary layout),
one resolving *outside* is refused. A **dangling** link — whose target does not
exist yet — is resolved by hand and judged the same way, so
`out.snap -> real/out.snap` records while `out.snap -> ../outside/x` is refused;
a link chain is followed to its end, and a chain the running kernel itself
refuses to resolve — `ELOOP`, whether from a cycle or from exceeding that
platform's own hop limit — is refused rather than resolved by hand. The limit is
whatever the running kernel enforces (32 on macOS, 40 on Linux); pitty does not
reimplement the arithmetic, because a count of pitty's own link hops cannot see
the intermediate directory symlinks the kernel traverses inside each step. **No
bound of pitty's own caps chain length**, so a chain the running kernel accepts
resolves, up to the visited-set cap described next. A dangling cycle, which no single kernel call can detect —
every `read_link` in it succeeds — is caught by remembering the paths the walk
has already visited, so it is detected exactly rather than approximated by a
count. That visited set has a size cap purely so a pathological chain cannot
exhaust memory; reaching it refuses the path rather than truncating the walk and
reporting a destination never actually reached.
When the link is an *intermediate* component, the components after it are
re-appended to its target before anything is judged — `link -> missing-dir` with
`file: link/out.snap` lands at `missing-dir/out.snap` — and containment is
decided on that whole location, since either half can move the result across the
boundary. What the recorder then walks is that same location, never the link's
own name — see below.

A `..` in that combined path may only cancel a component that is a **resolvable
directory**, which is the rule the kernel applies: it resolves left to right, so
`link/../victim.snap` where `link -> missing` fails at `missing` with `ENOENT`,
and `blocker/../victim.snap` where `blocker` is a regular file fails at `blocker`
with `ENOTDIR`. Neither ever reaches `victim.snap`. **Whether a component is
traversable is decided by the kernel, not by inspecting the component's
attributes.** Successive attempts to answer it from attributes were each wrong in
a new way: "does it exist" admits a regular file, a FIFO and a device node; "is
it a directory" admits a directory with no search permission, which `stat`
reports as a perfectly ordinary directory while the kernel refuses to resolve
`locked/..` through it with `EACCES`. The list does not end there either —
`noexec` mounts, dangling mount points, an unresponsive NFS server and MAC
layers such as SELinux all make a path that passes every attribute test still
fail to resolve. pitty therefore asks the kernel to open the component for
*search* (`O_SEARCH` where the platform has it) and adopts its answer, which is
the same question a `..` asks. Note that search permission, not read permission,
is what traversal requires: a `0100` directory is traversable though it cannot
be listed, and a `0600` one is listable but not traversable. Eliminating `..`
purely lexically would
resolve such a path onto a real neighbouring file and — under `--update` —
truncate and rewrite it. That is not a containment escape (the file is inside the
workspace) but silent data loss, so a path the kernel could not resolve is
refused rather than redirected.

Refusing it is reported as a **failed assertion** (exit 1, in the report), not as
a scenario error: the path is one the filesystem itself would have rejected, and
pitty declines it before opening anything rather than discovering it mid-write.
Only a path that escapes the workspace whatever the filesystem holds is a
scenario error (exit 2, no report). Keeping the two apart matters because a
consumer parses the report for per-assertion results.

This rule is applied to the scenario's own `file:` value as well as to a link's
destination, since the same shape reaches the same write either way. On the
`file:` value one narrowing applies, and **only when recording**: a `..` across a
merely *absent* component stays allowed under `--update`, because `--update`
creates the snapshot's parent directories, so that path does resolve once
recording proceeds.

The position a `..` is judged from follows symlinks as the kernel does, **absent
a concurrent mutation of the path** — the qualifier is load-bearing and is spelt
out under *Residual: the path-mutation race* below. After `link -> real/subdir`
is crossed, `link/..` stands in `real`, not in the
directory holding `link`, so the component a following `..` cancels is the one at
the *destination*. Stepping to the lexical parent instead was a false green: with
`missing` present beside the link and absent under `real`,
`link/../missing/../victim.snap` passed its guard against the lexical `missing`
and the assertion was then satisfied by comparing against a file the kernel
refuses to reach. The asymmetry is what exposes it — the same name existing at
one position and not the other — so neither a plain symlinked component nor a
plain missing one is sufficient to test this.

When **verifying** — any run without `--update` — that narrowing does not apply,
because nothing will create the missing component: `missing/../victim.snap` is a
path the kernel refuses with `ENOENT` and always will. Eliding it there was worse
than a write hazard: the path normalized onto an already-recorded neighbouring
file and the assertion *passed* by comparing the output against a file the
scenario could never have opened. A false green is the worst failure direction
for a testing tool, so the resolver is told which of the two questions it is
being asked rather than inferring it from the path.

On Unix the recording never re-walks the `file:` path by name. The workspace
directory is captured as a file descriptor in `Workspace::prepare`, before any
scenario child process is spawned, and the recorder descends from *that
descriptor* one component at a time with `openat(O_DIRECTORY | O_NOFOLLOW)`,
creating the file relative to the resulting directory descriptor. Two properties
follow:

- **A symlink planted after the containment check cannot redirect the write** —
  at the final component or at any intermediate directory — because each handle
  names a directory object rather than a name, and a component that is a symlink
  when the walk reaches it is refused rather than followed. What this does *not*
  fix is *which* component sequence is applied to that descriptor: that is still
  derived by name, and a child racing the resolver can change it — see
  *Residual: the path-mutation race*.
- **Renaming the workspace and replacing it with a symlink cannot redirect the
  write either.** A descriptor cannot be retargeted, so a child that runs
  `mv "$PWD" "$PWD.old" && ln -s /elsewhere "$PWD"` mid-run sees the snapshot
  still land in the directory the run started in — which, when the rename wins
  the resolver's own race, is the *stale* directory rather than the intended
  one (same residual).

The **comparison read goes through the same traversal as the write**, so both
halves of `expect_snapshot` name the same file: whatever file the write would
have created is the file the read compares against, so the two halves cannot be
made to name different files. What this does *not* say is that the file is the one the
scenario author meant. A child racing the resolver can move which directory the
shared traversal descends into — see *Residual: the path-mutation race* below —
and a snapshot planted there is then read by an assertion that passes. The
guarantee here is the agreement of the two halves, not the identity of the
directory.

**Hard links are refused.** A hard link is not a symlink — it is the file under
a second name — so `O_NOFOLLOW` does not see one, and linking an external file
to a name inside the workspace would otherwise bypass containment entirely: the
read would return the external contents and `--update` would truncate and
rewrite that external inode. Both the read and the write therefore `fstat` the
descriptor they just opened and refuse any file whose link count exceeds one.
The check is made on the open descriptor rather than on the path, so the object
checked is necessarily the object used.

The residual limit *of the hard-link check* (not the only residual — see also
*Residual: the path-mutation race*), stated precisely: an attacker who adds a
link *after* that check, while pitty holds the descriptor, can read what pitty
then writes through their own name. That is not preventable at this layer — the inode is legitimately
ours and they are adding a reference to it — but it is strictly weaker than the
bypass above: it cannot redirect a write into a pre-existing external file and
cannot make a planted file satisfy an assertion. The file's `0600` mode confines
it to the same uid — which, since the scenario's own child *runs as* that uid, is
not a meaningful barrier against this particular attacker. Snapshot content is
unmasked by design, so a child can read the snapshot pitty writes. That adds no
capability it did not already have by reading the workspace directly, but it is
not something the mode bit mitigates either. A snapshot file with a legitimate second link is
refused rather than used; pitty treats multiple names on an unmasked-secret file
as a condition to report, not to work around.

The component sequence the recorder walks is the one containment validated:
workspace-relative, normalized, and free of `..`. A `file:` whose raw spelling
steps outside the workspace before returning to it (`../sibling/../w/out.snap`)
therefore creates nothing outside it, even though the snapshot itself lands
inside. An absolute `file:` inside the workspace goes through the same
traversal; one outside it is refused.

**This holds on every platform, including Windows.** The raw `file:` value is
kept only for diagnostics and is never opened; the path actually used is rebuilt
from the workspace root and the validated components. Windows has no `openat`
and so lacks the *symlink*-race protections described above, but it does not
create directories outside the workspace for a path that normalizes back inside
— a `..` detour is not an escape there either.

The workspace the snapshot is resolved against is the one captured **before any
scenario process ran**, and that is enforced by identity rather than by name. The
descriptor's `(dev, ino)` is read from the descriptor itself at capture time, and
the root the component sequence is derived from is produced only after being
checked against it — so the value that is validated and the value that is used
are the same expression, not two that happen to agree. A canonical path *string*
cannot carry this: a workspace deleted and recreated under the same name
canonicalizes identically while being a different directory.

If the workspace directory is replaced during a run — renamed and recreated under
the same name, or a symlinked workspace repointed to a directory *outside* the
captured one — the snapshot is **refused** rather than written to the
replacement. This is a deliberate difference from 1.2.2, which resolves the name
afresh and writes into whatever now sits there (verified by execution). A
scenario whose own child replaces its workspace mid-run is the case this
protects, and refusing is the fail-closed direction *for the non-racing case*:
nothing is written to a directory the run did not start in.

The refusal holds against a replacement that has **already happened** when the
check runs. It is **not** proof against one that happens *during* it: the
identity check and the path resolution that follows are separate name-based
steps, and a child that lands the rename between them is not refused — the write
goes to the pre-rename directory and a verify can pass there. See *Residual: the
path-mutation race*.

**Why that *outside* qualifier is load-bearing, and structural rather than
incidental.** The identity check
is made against the workspace's *canonical* path, which is what the descriptor
was opened on — the alias is resolved away in `Workspace::prepare` and never
consulted again. So repointing `w -> real` to `w -> real/subdir` changes nothing
the check can see: `real` is still `real`, and `real/subdir/out.snap` is inside
the captured root, so the write proceeds. Measured against 1.2.2, both builds
exit 0 and both use the replacement — so this is not a divergence from v1
either. A repoint whose new destination lands *inside* the captured canonical
root is simply outside the scope of the refusal above, which is why that
paragraph says *outside* rather than naming repointing as such.

### Residual: the path-mutation race

The protections above are stated as they behave when pitty is the only thing
touching the path. They are **not** proof against a child that mutates the path
*between* pitty's own syscalls, and that class of attack is **not closed**:

> **An adversarial child process that renames, replaces or unlinks a path
> component in the window between two of pitty's syscalls can still make an
> assertion pass, or make a write land in a stale directory.**

Two windows are known and are recorded rather than fixed:

- **Workspace identity.** The identity check compares the descriptor against a
  `stat` of the name, but the path resolution that follows re-consults the name
  and then applies the resulting components to the *original* descriptor. A child
  that runs `mv w w.old; mkdir w` inside that window makes a verify read
  `w.old/victim.snap` and **pass**, where 1.2.2 reads the new `w/victim.snap` and
  fails with "not recorded"; under `--update` the same window updates the old
  workspace's file.
- **Per-component position.** Deciding that a component is traversable and then
  canonicalizing it are two separate name-based syscalls, and a failure of the
  second is ignored — the walk keeps the lexical position. A child that unlinks
  `link` between them makes `link/../victim.snap` normalize to
  `root/victim.snap` and **pass**, where 1.2.2's raw read hits `ENOENT` at `link`
  and fails. A second window remains between a successful canonicalize and the
  later re-resolution of the same path.

Closing these requires resolving every component relative to a held descriptor
rather than by name — a redesign of the resolver, not a patch to it — and that is
deliberately not attempted here. **The limit is therefore a real one: do not rely
on pitty's snapshot containment to hold against a program that is actively
racing it.** If the program under test is untrusted, or merely mutates its own
workspace layout concurrently with assertions, treat a passing snapshot
assertion as unproven.

For completeness rather than reassurance: 1.2.2 re-resolves the whole path by
name at assertion time and is exposed to this class at least as widely. But it
is **not** uniformly worse, and the honest form of the comparison has to say so:
in both windows above, 1.2.2's naive re-resolution happens to *fail* the
assertion where this build *passes* it, so for those two shapes the current
behaviour is the weaker one. The comparison is recorded because it is a fact
about the two builds, not because it makes either safe to point at a hostile
program — a reader deciding that question needs the limit above, not the
comparison.

How the 1.2.2 comparisons in this document were verified: by running a 1.2.2
binary by hand when each was written. There is no committed differential
harness — `tests/` contains no 1.2.2 invocation — so these are point-in-time
observations rather than properties CI re-checks.

### Containment, continued

There is no unprotected fallback. If the workspace descriptor cannot be
captured, a run that records a snapshot **fails** rather than reverting to a
path-based write: a control that silently switches itself off under conditions
an attacker can influence is worse than one that was never claimed.

The descriptor is requested once, before any scenario process is spawned — that
timing is what makes it trustworthy — but a failure to obtain it is reported only
when a snapshot actually needs containment. A scenario with no `expect_snapshot`
step never asks and is never affected. An earlier version raised the failure at
preparation time and justified it by claiming no legitimate workspace could fail
capture; that was **wrong in both directions**. Entering a directory requires
execute/search permission, not read, so a `0300` workspace runs commands
perfectly well while `O_RDONLY` cannot open it — such a run was killed before its
first step even though it never touched a snapshot.

Where the platform provides `O_SEARCH` (POSIX 2008; macOS), directories are
opened with search-only access, which is all a traversal needs, so even a `0300`
workspace supports snapshots normally. Elsewhere — notably glibc Linux, which
does not define `O_SEARCH` — capturing the workspace descriptor falls back to
`O_RDONLY`, so a snapshot in a workspace that is searchable but not readable is
**refused** there: the run fails with a reported error instead of recording. That
is the only thing "fails closed" means here — a lost capability, not a weakened
check. A run without snapshots still succeeds on every platform.

The same `O_SEARCH` gap affects the separate question of whether a `..` may
cancel a path component, and there it is answered differently, because a wrong
answer costs more. That check must not fall back to `O_DIRECTORY`: `O_DIRECTORY`
implies `O_RDONLY`, so it asks for *read* permission where traversal requires
*search*, and it reports a `0600` directory as traversable while the kernel
refuses `dir/..` through it with `EACCES` — which would reinstate exactly the
data-loss defect this rule exists to prevent, on every Unix without `O_SEARCH`.

So where `O_SEARCH` is unavailable the check stops predicting traversability from
a mode bit and **performs the traversal**, opening `dir/.` — a path the kernel can
only resolve by descending *through* `dir`, and therefore one that demands the
same search permission a `..` does. The kernel's answer is adopted as given.
Measured against directories created for the purpose (on macOS, with `O_SEARCH`
forced off), every case either matches the kernel or is stricter than it: `0600`, `0000` and a regular file are refused
(matching), an ordinary directory resolves (matching), and a `0100` directory —
searchable but not readable — is refused where the kernel would allow it.

That last row is the one residual difference between the two paths, and its
direction is what makes it acceptable: it is a **false refusal**, surfacing as a
reported assertion failure, and never a false pass or a write to the wrong file.
The distinction is enforced rather than assumed — such a directory is classified
as *refused*, never as *absent*, because an absent component is deliberately
exempt under `--update` and misclassifying a live directory that way is precisely
how a false refusal would become a false pass.

What was verified by execution, and where: the truth table above, the
classification of `0100` as refused, and the fallback path itself were all run on
macOS, with `O_SEARCH` forced off so the arm glibc Linux takes is exercised here
rather than only reasoned about. **The Linux behaviour itself was not run** — no
Linux host, container runtime or remote builder was available — so that claim
rests on the POSIX rule that resolving a component requires search permission on
it, not on a Linux-specific assumption or on a run that did not happen.

Search-only access also removes the last name-based operation from the
traversal. A directory pitty creates has its mode set on a **descriptor it
opened**, not on the name it just created: a plain `mkdirat` succeeding does not
prove the entry is still the same object one syscall later, and the scenario's
child can `rmdir` it and leave a **hard link to an external file** at that name.
`AT_SYMLINK_NOFOLLOW` is no defense there — a hard link is not a symlink — so a
name-based `fchmodat` would have set an inode outside the workspace to `0700`.
Opening first makes the descriptor the object, and `O_DIRECTORY` rejects the
swapped file outright. This ordering is only possible because the open asks for
search-only access: the umask masks bits off `mkdirat`'s mode argument, so the
new directory can land unreadable, and opening it for reading would fail. Where
search-only access is unavailable, such a run fails rather than chmod-ing an
unverified name.

Linux's `O_PATH` is the obvious candidate to close that gap, and the `open(2)`
documentation does permit an `O_PATH` descriptor as the `dirfd` of the `*at()`
calls this traversal uses. It is not adopted because it has not been *verified by
execution* on Linux — the `O_SEARCH` behavior above was confirmed by running the
syscalls, and no Linux host was available to do the same for `O_PATH`. Adopting a
containment-critical flag on documentation alone is precisely the error this
section is meant to prevent, so the limitation is recorded rather than papered
over. A source-level `TODO` in `safepath::open_dir_fd` names the checks a future
change should run.

Components *above* the workspace directory are never traversed.

**What holds on Windows, precisely.** There is no `openat`, so none of the
*race* protections above apply: the resolver's symlink check is the only guard,
and a junction or a link swapped in after that check redirects the write. The
hard-link and non-regular-file refusals, which are `fstat`-on-descriptor checks,
likewise do not run. What **does** hold is everything that needs no descriptor:
`..` containment (the path is rebuilt from the validated component sequence, per
the paragraph above), the refusal of a `file:` resolving outside the workspace,
and the refusal of a path naming the workspace root itself. In short, on Windows
a *mistake* in a `file:` value is still contained; a *deliberate race* is not.
Two readings to rule out. This sentence is about **snapshots** only — log
containment on Windows does not hold even against a mistake, as the log
paragraphs below state. And it is not the converse claim that Unix contains every
deliberate race: Unix closes the symlink-swap races described above and leaves
the path-mutation races of the residual section open.

**On Unix**, log writes are resolved with the same descriptor-anchored
traversal: `logs/` is opened relative to a base-directory descriptor with
`openat(O_DIRECTORY | O_NOFOLLOW)` and each log file relative to that, so a
`logs/` replaced by a symlink is refused rather than followed, and the swap race
that a path-based check would leave open is closed. The candidate-name probe
uses the same descriptor, so a **dangling** symlink counts as occupied rather
than free. The anchor is the base directory (the scenario file's directory, not
the workspace) as it resolved before the run started; unlike the snapshot
workspace it carries no `(dev, ino)` re-check, because nothing the log path is
applied to is re-derived from a name afterwards. Everything stated about log
containment in this section is Unix-only unless it says otherwise.

**On Windows, log containment does not hold at all.** There is no pre-spawn
descriptor and no `openat`; the log directory and file are resolved by name on
each use (`create_dir_all`, `File::open`, `std::fs::write`). A directory junction
at `logs/` pointing outside the base directory, or a link swapped in after the
name check, redirects the write, and there is no descriptor-based equivalent to
fall back on. Log content is masked, so the exposure is bounded to where the
bytes land rather than what they contain, but a Windows run should not be relied
on to keep logs inside `logs/`.

The technique that makes *snapshot* `..`-containment hold on Windows — rebuilding
the path from validated components rather than a raw string — does not transfer,
and the reason is worth stating so it is not re-proposed. A snapshot path is
author-supplied and may contain `..` or separators, so validating its components
is what makes it safe. A log path contains no author-supplied path at all: the
directory is the fixed literal `logs/`, and the file name is reduced to
`[A-Za-z0-9._-]`, which no separator or `..` survives. That half of the guarantee
is therefore already unconditional on every platform. What remains on Windows is
only the symlink and swap races, and closing those requires `openat`.

Note the asymmetry with snapshots, which is real and deliberate rather than an
oversight: snapshot writes **do** keep their `..`-detour containment on Windows,
because the path is rebuilt from the validated component sequence rather than
the raw `file:` string. That fix needs no `openat` and so ports to Windows,
whereas the symlink-race protections do not. The log path has not had the
equivalent treatment.

If that base-directory descriptor cannot be obtained before the scenario starts,
pitty **does not write the log at all** (Unix): it reports the failure on stderr
and leaves the run's verdict unchanged. It never falls back to a path-based
write, so containment is either in force or visibly absent — never silently
skipped. On Windows there is no descriptor to obtain, so this rule has nothing
to apply to and the path-based write is what always happens.

A log name is taken by exclusively creating its ownership sidecar
(`openat` with `O_CREAT | O_EXCL | O_NOFOLLOW`) and holding an advisory `flock`
on it for the duration of the write. Two concurrent runs — including two runs of
the *same* scenario, which cannot be told apart by the record alone — therefore
never write one log file at once; the second takes the next candidate name
instead. Three limits apply, and they fail differently:

- The lock is **advisory**. It excludes other pitty processes, not a `tee` or an
  editor writing the same path.
- A filesystem that **rejects** `flock` (some NFS exports answer `ENOTSUP`)
  exhausts every candidate, and the run reports a log-write failure rather than
  writing anyway — loud, not silent. The unsupported-lock condition is not
  detected directly, so all 1000 candidates are attempted first.
- A filesystem that **accepts** `flock` without sharing locks between clients is
  the quiet case: each side believes it holds the lock, and two hosts writing one
  network-mounted `logs/` can still interleave. Nothing detects this.

Windows has neither primitive, so ownership is recorded there but not enforced,
and two concurrent runs of one scenario can still interleave a log.

Registered secrets are masked out of a log's **filename** as well as its
contents, for the scenario-file stem and matrix axis coordinates. A secret in a
scenario's `name:` is a known gap tracked as issue #43.

Two different values identify a log, and **neither protects a secret**:

- The **filename discriminator** — the 8 hex characters appended when masking a
  secret out of a name would otherwise make two identities collide — is a plain
  FNV-1a digest of the unmasked identity. It is reproducible by anyone: given a
  filename and a guess at the identity behind it, the guess is directly
  checkable. **A low-entropy secret used inside a scenario identity — a PIN as a
  matrix axis value — is therefore recoverable from a directory listing alone.**

  An earlier release keyed this with a random per-directory salt, which is now
  removed. The keying did not hold: FNV-1a's per-byte step is invertible over its
  64-bit state, so two tags from one directory recover a salt-equivalent state in
  roughly 2^32 work, and a listing supplies two filenames by construction. The
  available alternatives cost more than the guarantee is worth — `DefaultHasher`
  is SipHash but std does not promise stability across releases, so a toolchain
  upgrade would rename and orphan every discriminated log; hand-rolling a MAC in
  a log-naming sink is worse than the problem it solves; and a crypto dependency
  is a large ask for a threat model of "someone read `ls` output but holds none
  of the files". Keeping a defence we cannot support would be worse than stating
  its absence.

- The **ownership record** in each log's `.claim` sidecar stores the scenario's
  identity *after masking*, plus a random per-log token. Unlike the filename, this
  contains no secret to recover: masking removed it before anything was stored.
  It is deliberately not a digest of the raw identity, which would be exactly the
  oracle described above.

**The operative guidance is unchanged and is the one that matters: do not put a
secret in a scenario identity.** A secret in `name:` reaches the filename unmasked
regardless (issue #43), so a scenario identity should be treated as public.

On Unix, logs and snapshots are `0600` before any content is written to them —
including when an existing file is reused, whose mode is restored on the open
descriptor before the file is truncated, so a log a pre-fix pitty left at `0644`
is never observable at that mode holding new content. Temp workspaces plus any
snapshot directory **pitty creates** are `0700`, so another local user on a
shared runner cannot read them. pitty does not change
the mode of a directory that already exists — that belongs to the user, and a
snapshot recorded into a shared repository checkout leaves the checkout's
permissions alone. Windows has no mode bits on the write path at all, so none of
the ordering above (mode before content, mode restored on reuse) applies there: a
log or snapshot inherits the runner user's default ACLs and nothing narrows them. Snapshot
content is written unmasked by design — do not snapshot sensitive output.

Release archives are published with SHA-256 checksums. The composite action
fails closed when a prebuilt archive cannot be verified and otherwise falls
back to a locked source build. GitHub Actions dependencies are pinned to commit
SHAs and checked in CI.
