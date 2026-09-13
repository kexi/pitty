//! Workspace setup: working directory, environment, `${var}` expansion, and
//! secret masking registration.
//!
//! A workspace either runs in an existing directory (relative to the scenario
//! file) or in a fresh temp directory (`0700` on Unix). It resolves the
//! scenario's variables and environment, knows how to expand `${var}`
//! placeholders in step payloads, and registers secret values so logs and
//! errors can mask them.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use tempfile::TempDir;

use crate::config::Scenario;
use crate::error::PittyError;

/// Which question [`Workspace::resolve_write_path`] is being asked.
///
/// The two differ on exactly one point — whether a **missing** path component
/// may be elided by a following `..` — and they differ because they differ on
/// whether that component is about to exist:
///
/// - [`Record`](Self::Record) (`--update`) is going to create the snapshot's
///   parent directories, so `../new-dir/../w/out.snap` genuinely resolves once
///   recording proceeds. 1.2.2 records it, so refusing it would break v1.
/// - [`Verify`](Self::Verify) creates nothing. `missing/../victim.snap` is a path
///   the kernel refuses with ENOENT and always will; eliding `missing/` made an
///   unreachable path *satisfy* an assertion by reading a different file.
///
/// Passed explicitly rather than inferred, because the resolver has no other way
/// to know: the update flag lives on the runner's options and the distinction is
/// invisible from the path alone. An enum rather than a `bool` so the call site
/// says which of the two it means — the same reason [`DanglingLink`] is a set of
/// named variants instead of an `Option`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotAccess {
    /// The snapshot is about to be written; missing parents will be created.
    Record,
    /// The snapshot is only being read back; nothing will be created.
    Verify,
}

/// A validated destination for a snapshot write.
///
/// Produced only by [`Workspace::resolve_write_path`], which is the single
/// place containment is decided. It bundles the three things the recorder needs
/// and guarantees they agree with each other:
///
/// - `components` is the path **as containment validated it**: relative to the
///   workspace root, already normalized, and free of `..` and absolute
///   segments. The recorder walks exactly this sequence, so it cannot step
///   somewhere containment never approved. Keeping the checked and the walked
///   sequence as one value is deliberate — when they were derived separately, a
///   `file:` that normalized back inside the workspace could still create
///   directories outside it along the way.
/// - `root_fd` is the workspace directory captured as a descriptor before the
///   scenario's child was spawned, so the walk starts from the directory object
///   the run began in rather than from whatever the root path resolves to now.
/// - `display` is the joined path, used only for messages.
#[derive(Debug)]
pub struct SnapshotTarget {
    /// The joined path, for error and result messages only. Never traversed.
    ///
    /// This is the `file:` value as written, so it is the form a user
    /// recognizes in a diagnostic — and, for exactly that reason, the form that
    /// may still contain `..`. Nothing may open it. See [`Self::safe_path`].
    display: PathBuf,
    /// The workspace root the components are relative to.
    root: PathBuf,
    /// Workspace-relative, normalized, `..`-free components; the last is the
    /// file name. Non-empty by construction.
    components: Vec<std::ffi::OsString>,
    /// Workspace-relative directory paths the `file:` value **names** but which
    /// `..` then cancels, so they do not appear in [`Self::components`].
    ///
    /// These exist so the recorder can stop *predicting* whether such a
    /// component could be created and simply attempt it. `file:
    /// missing/../victim.snap` normalizes to `victim.snap`, so nothing in
    /// `components` ever touches `missing/` — but 1.2.2 called `create_dir_all`
    /// on the raw path and therefore *did* create it, and failed with `EACCES`
    /// when the workspace was not writable. Reproducing v1 means reproducing
    /// both halves: the directory appears on a writable tree, and the run fails
    /// where the kernel refuses to make it.
    ///
    /// Each entry is a full component sequence from the root (not a single
    /// name), so a cancelled component nested under real directories is created
    /// in the right place.
    traversed_dirs: Vec<Vec<std::ffi::OsString>>,
    /// The pre-spawn workspace directory descriptor. Always present on Unix:
    /// there is no unprotected mode to fall back to.
    #[cfg(unix)]
    root_fd: std::sync::Arc<std::os::fd::OwnedFd>,
}

impl SnapshotTarget {
    /// The full path **as written**, for messages only.
    ///
    /// Never open this. It may contain `..` segments that leave the workspace
    /// part-way along, even when the path normalizes back inside; use
    /// [`Self::safe_path`] (or, on Unix, the descriptor walk) to touch the file.
    pub fn display_path(&self) -> &Path {
        &self.display
    }

    /// The destination as containment validated it: the workspace root joined
    /// with the normalized, `..`-free component sequence.
    ///
    /// This is what a platform without `openat` must use. Building the path from
    /// the validated components rather than the raw `file:` string is what stops
    /// a value like `../outside/new/../../w/out.snap` — which normalizes back
    /// inside the workspace, and so rightly passes containment — from creating
    /// `outside/new` on the way there. It is the same fix the Unix traversal
    /// got: walk what was checked, not what was typed.
    pub fn safe_path(&self) -> PathBuf {
        let mut path = self.root.clone();
        path.extend(self.components.iter());
        path
    }

    /// The validated, workspace-relative component sequence to walk.
    #[cfg(unix)]
    pub fn components(&self) -> &[std::ffi::OsString] {
        &self.components
    }

    /// The workspace root the components (and `traversed_dirs`) are relative to.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Directory sequences the `file:` value names but `..` cancels.
    ///
    /// The recorder must attempt to create each before writing, so that whether
    /// such a component *can* exist is decided by the kernel rather than
    /// predicted. See [`Self::traversed_dirs`](struct.SnapshotTarget.html) and
    /// `cancelled_directories`.
    pub fn traversed_dirs(&self) -> &[Vec<std::ffi::OsString>] {
        &self.traversed_dirs
    }

    /// The pre-spawn workspace directory descriptor.
    #[cfg(unix)]
    pub fn root_fd(&self) -> &std::os::fd::OwnedFd {
        &self.root_fd
    }

    /// Build a target directly from a root and a relative path, for tests.
    ///
    /// Exists so the snapshot writer's own tests can exercise it *without*
    /// going through the resolver — which is the point of those tests: they
    /// prove the write layer is independently safe against links planted after
    /// resolution, so they must be able to set up states the resolver would
    /// have refused. It captures the root descriptor the same way `prepare`
    /// does and splits `rel` into plain components, so the target it produces
    /// has the same shape the resolver's does.
    ///
    /// Not `#[cfg(test)]`: integration tests in `tests/` are separate crates
    /// and would not see it. It is inert in production because nothing on the
    /// run path calls it.
    #[doc(hidden)]
    pub fn for_test(root: &Path, rel: &Path) -> Self {
        let components = rel
            .components()
            .map(|c| c.as_os_str().to_os_string())
            .collect();
        SnapshotTarget {
            display: root.join(rel),
            root: root.to_path_buf(),
            components,
            // A test target names no `..`, so nothing is cancelled.
            traversed_dirs: Vec::new(),
            #[cfg(unix)]
            root_fd: std::sync::Arc::new(
                crate::safepath::open_dir_fd(root).expect("test root directory must be openable"),
            ),
        }
    }
}

/// A prepared workspace for a scenario run.
pub struct Workspace {
    /// The workspace directory canonicalized **before any scenario process ran**.
    ///
    /// Paired with `cwd_fd`: both describe the directory as it was pre-spawn, so
    /// containment decisions and the descriptor they are applied to cannot come
    /// to refer to different objects. Never recomputed from `cwd` at assertion
    /// time — a child can rename the workspace between the two.
    canonical_cwd: PathBuf,
    /// `(dev, ino)` of the directory `cwd_fd` refers to, read from the
    /// descriptor itself at capture time.
    ///
    /// A canonical path *string* is not an identity: a workspace deleted and
    /// recreated under the same name canonicalizes identically while being a
    /// different inode. Storing the identity the descriptor reports — rather
    /// than re-deriving it from the name later — is what lets the root the
    /// components are derived from be checked against the object they will be
    /// applied to.
    #[cfg(unix)]
    cwd_identity: Option<(u64, u64)>,
    /// The directory commands run in.
    cwd: PathBuf,
    /// Variable name -> value, used for `${var}` expansion.
    variables: BTreeMap<String, String>,
    /// Parent-process environment, captured at prepare time, used as a
    /// fallback for `${var}` expansion when a name is not a scenario variable.
    ///
    /// Why a captured snapshot rather than reading `std::env::var` lazily at
    /// expansion time: capturing once keeps expansion deterministic for a run
    /// (the environment cannot shift mid-run) and keeps `expand` a pure
    /// function of the workspace, which is what the unit tests exercise.
    parent_env: BTreeMap<String, String>,
    /// Resolved environment for spawned processes.
    env: Vec<(String, String)>,
    /// Literal secret values to mask (`***`) in logs and errors.
    secrets: Vec<String>,
    /// Held to keep a temp directory alive; dropping it removes the dir. Kept
    /// even though unread so its `Drop` runs at the end of the run.
    _temp: Option<TempDir>,
    /// A descriptor for [`Self::cwd`], opened during `prepare` — that is,
    /// *before* any scenario child process is spawned — and held for the whole
    /// run (Unix only).
    ///
    /// Why a descriptor and not just the path: a path is re-resolved on every
    /// use, so a child that does `mv "$PWD" "$PWD.old" && ln -s /tmp/victim
    /// "$PWD"` makes the same path name a different directory at snapshot time,
    /// and a snapshot write anchored on that path would land in `/tmp/victim`.
    /// A descriptor names the directory *object*: a later rename, or replacing
    /// the name with a symlink, cannot retarget it. Snapshot recording
    /// traverses from this fd, so the directory it writes into is the one that
    /// existed when the run started.
    ///
    /// Holds the *error* rather than `None` when capture failed, and that
    /// distinction is the point. This descriptor is the containment mechanism
    /// for snapshot writes on Unix, so there is no "protection unavailable"
    /// state a snapshot may proceed under: [`Workspace::resolve_write_path`]
    /// returns this error instead of handing out a target, and nothing
    /// downstream can fall back to a path-based write. An earlier version stored
    /// `None` and let the writer silently pick a symlink-following path — a
    /// control that turns itself off is worse than one that was never claimed.
    ///
    /// Why the failure is deferred to first use rather than raised in
    /// `prepare`: a run that records no snapshot needs no containment, and
    /// failing it helps nobody. Traversal needs execute/search permission, not
    /// read, so a `0300` workspace runs commands perfectly well while being
    /// unopenable for reading — rejecting it at prepare time broke legitimate
    /// runs. The error is therefore carried until something actually asks for a
    /// snapshot path.
    #[cfg(unix)]
    cwd_fd: Result<std::sync::Arc<std::os::fd::OwnedFd>, String>,
}

impl Workspace {
    /// Prepare a workspace from a scenario, resolving paths against
    /// `base_dir` (the directory containing the scenario file).
    ///
    /// When `workspace.temp` is set, a temp directory is created via
    /// `tempfile::TempDir` (`0700` on Unix). We use `mkdtemp`-backed `TempDir`
    /// rather than constructing our own predictable name: self-named temp dirs
    /// are prone to races and symlink attacks, whereas `TempDir` creates the
    /// directory atomically with a random name.
    pub fn prepare(scenario: &Scenario, base_dir: &Path) -> Result<Self, PittyError> {
        let mut secrets = Vec::new();
        let mut variables = BTreeMap::new();
        for (name, spec) in &scenario.variables {
            let value = spec.value();
            variables.insert(name.clone(), value.to_string());
            if spec.is_secret() && !value.is_empty() {
                secrets.push(value.to_string());
            }
        }

        // Capture the parent environment once, before expanding, so the
        // scenario-level `env` values below and every later `expand` call share
        // one snapshot and cannot disagree about a `${NAME}` that resolves
        // through the parent-env fallback.
        let parent_env: BTreeMap<String, String> = std::env::vars().collect();

        // Scenario-level `env` values are `${var}`-expanded, exactly like the
        // spawn-level `env` values the runner merges on top of them
        // (SCHEMA.md's expansion-site list covers both). They are expanded here,
        // during construction, via the shared `expand_with` rather than a
        // `self.expand` call, because `self` does not exist yet; routing both
        // through one body is what keeps the two halves of that merge from
        // drifting apart.
        //
        // Why not resolve `${var}` against the other scenario-level `env`
        // entries: expansion reads only `variables` and the parent env, so an
        // `env` value naming another `env` key is *not* self-referential — it
        // falls through to the parent-env value of that name, or stays literal.
        // That matches the single-pass, non-recursive rule every other
        // expansion site follows; making `env` the one map that can see itself
        // would introduce ordering and cycle questions the format does not
        // otherwise have.
        let env: Vec<(String, String)> = scenario
            .env
            .iter()
            .map(|(k, v)| (k.clone(), expand_with(&variables, &parent_env, v)))
            .collect();

        let (cwd, temp) = if scenario.workspace.temp {
            let dir = TempDir::new().map_err(|e| {
                PittyError::Process(format!("failed to create temp workspace: {e}"))
            })?;
            set_permissions_0700(dir.path())?;
            (dir.path().to_path_buf(), Some(dir))
        } else {
            let resolved = base_dir.join(&scenario.workspace.cwd);
            (resolved, None)
        };

        // Capture the workspace directory as a descriptor now, before any child
        // is spawned, so snapshot recording can traverse from the directory
        // object that existed at the start of the run rather than from whatever
        // the path happens to resolve to later. See the `cwd_fd` field.
        //
        // The attempt is made *here*, before any child exists, because that is
        // where the security value lies: a descriptor obtained later could
        // already name a directory the child substituted.
        //
        // A failure is recorded rather than raised. Raising it here failed runs
        // that never touch a snapshot, and the justification for doing so was
        // simply wrong: traversing a directory needs execute/search permission,
        // not read, so a `0300` workspace is perfectly usable — a child can
        // `chdir` into it and run — while `O_RDONLY` cannot open it. (On
        // platforms with `O_SEARCH` even that case now captures successfully;
        // see `safepath::open_dir_fd`.)
        //
        // What must not change is the fail-closed property: the error is held
        // and returned by `resolve_write_path`, so any run that actually records
        // a snapshot still fails rather than writing through an unprotected
        // path. A run with no snapshot step never asks, and never fails.
        #[cfg(unix)]
        let cwd_fd = crate::safepath::open_dir_fd(&cwd)
            .map(std::sync::Arc::new)
            .map_err(|e| {
                format!(
                    "cannot open workspace directory {} for snapshot containment: {e}",
                    cwd.display()
                )
            });

        // Canonicalized **now**, in the same breath as the descriptor above, and
        // stored rather than recomputed per assertion.
        //
        // `canonical_root(&self.cwd)` at assertion time re-resolved the name
        // *after* the scenario's child had run, so a child that renamed the
        // workspace and put a fresh directory at the old name had the root (and
        // the component list derived from it) describe the NEW directory while
        // the descriptor still referred to the OLD one. The write then landed in
        // the renamed-away directory while the report named a path under the new
        // one — check and use naming different objects, which is the very thing
        // the descriptor exists to prevent.
        //
        // Pinning it here keeps the pair consistent: this path and `cwd_fd` are
        // two views of the same directory as it was before any scenario process
        // existed, and neither is re-derived from a name a child can move.
        let canonical_cwd = canonical_root(&cwd);
        // Read from the DESCRIPTOR, at the same moment it is captured, so the
        // identity belongs to the object the writes go through rather than to
        // whatever the name resolves to later.
        #[cfg(unix)]
        let cwd_identity = cwd_fd.as_ref().ok().and_then(|fd| fd_identity(fd));

        Ok(Workspace {
            cwd,
            canonical_cwd,
            #[cfg(unix)]
            cwd_identity,
            variables,
            parent_env,
            env,
            secrets,
            _temp: temp,
            #[cfg(unix)]
            cwd_fd,
        })
    }

    /// The directory commands run in.
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// The resolved environment for spawned processes.
    pub fn env(&self) -> &[(String, String)] {
        &self.env
    }

    /// The registered secret values, for the logger/masker.
    pub fn secrets(&self) -> &[String] {
        &self.secrets
    }

    /// Resolve a path that the scenario expresses relative to the workspace.
    ///
    /// This is the *read* resolver: it joins `rel` onto the workspace cwd
    /// without confining the result. Under the single-trust model, read targets
    /// (`expect_file_*`, `source: {file}`) are allowed to point anywhere the user
    /// could already read, so no containment check is imposed here.
    pub fn resolve_path(&self, rel: &str) -> PathBuf {
        self.cwd.join(rel)
    }

    /// Resolve a *write* path, confined to the workspace directory.
    ///
    /// Returns `Err` (a Scenario error message) when `rel` resolves outside the
    /// workspace root, whether via `..` segments or a symlink that points out,
    /// and when any component `rel` itself names is a symlink — including a
    /// *dangling* one, whose target does not exist.
    ///
    /// Why containment for writes when reads (`resolve_path`) are unconfined:
    /// under single-trust we accept that a scenario can *read* anything the user
    /// can read, so confining read targets would add friction for no real safety
    /// gain. A *write*, however, mutates the filesystem, and `expect_snapshot`
    /// under `--update` (especially with `PITTY_UPDATE_SNAPSHOTS=1` set
    /// globally in CI) records files automatically. A broken or hostile scenario
    /// with `file: ../../../tmp/x.snap` would then write outside the workspace on
    /// every run. Confining writes to the workspace keeps an automated record
    /// step from clobbering arbitrary paths, while leaving the (harmless) read
    /// asymmetry intact.
    ///
    /// Why those components are scanned with `symlink_metadata` rather than
    /// trusted as "do not exist yet": `Path::canonicalize` fails with `ENOENT` on a
    /// dangling symlink, so a canonicalize-only check reads a planted
    /// `x.snap -> /outside/PWNED` as a non-existent file and lets the write
    /// through to the link's target. `symlink_metadata` does not follow links,
    /// so it sees that entry for what it is.
    ///
    /// # Which symlinks are refused, and why only those
    ///
    /// A link that **resolves** is allowed through this check and judged by
    /// containment instead: `snapshots -> real` inside the workspace is an
    /// ordinary repository layout — a shared fixture directory is a normal
    /// reason to have one — and refusing it outright broke scenarios that worked
    /// in 1.2.2. Only a link `canonicalize` cannot follow (a *dangling* one) is
    /// refused here, because that is precisely the case containment cannot see.
    ///
    /// Allowing a resolvable link does **not** reopen the check-then-use race
    /// that motivated the original blanket refusal. The recorder never traverses
    /// the link's *name*: `resolve_existing_ancestor` canonicalizes through it,
    /// and the component sequence handed to [`SnapshotTarget`] is the canonical
    /// one (`real/out.snap`, not `snapshots/out.snap`). So a child that
    /// re-points `snapshots` after resolution changes nothing — that name is
    /// never opened again. And if the child swaps a *canonical* component for a
    /// link instead, the descriptor walk refuses it: every component is opened
    /// `O_NOFOLLOW`. Both cases verified by running the syscalls.
    /// # Two refusal classes, and why they are not the same
    ///
    /// The `Err` arm is a **scenario error** (exit 2, no report): the path is
    /// wrong whatever the filesystem holds, so the author must fix the YAML.
    /// The `Ok(Err(_))` arm is a path the **filesystem would refuse**, reported
    /// by the caller as a failed snapshot assertion (exit 1, with a report).
    ///
    /// 1.2.2 drew exactly this line, and a consumer depends on it: it parses
    /// stdout for a report, and `pitty run <dir>` needs a row per scenario.
    /// 1.2.2 discovered the second class by attempting the write and surfacing
    /// the `ENOTDIR`/`ENOENT`/`ELOOP`; pitty now refuses beforehand so nothing is
    /// truncated, but detecting it earlier must not change what a consumer sees.
    /// The workspace root to derive components from, taken from the descriptor
    /// the writes actually go through.
    ///
    /// # Why this consults the descriptor and not the name
    ///
    /// Two earlier versions of this were check-then-use. The first compared
    /// `(dev, ino)` and then let the caller use `self.canonical_cwd` anyway — two
    /// expressions that merely happened to agree. The second returned the root
    /// from the check, closing that gap at the API level but not the temporal
    /// one: it `stat`ed the *name*, so the proof was about the filesystem at one
    /// instant while everything downstream consulted it again afterwards.
    ///
    /// What makes this sound is not the comparison but where the root comes
    /// from. The identity is read from the held descriptor by `fstat` — the
    /// object cannot be swapped out from under a descriptor — and the path is
    /// accepted only if the name still denotes that same object. So the check is
    /// a statement about the descriptor, which is also what every read and write
    /// is anchored on: `read_snapshot` and `write_snapshot_unix` both walk from
    /// `root_fd` with `openat(O_NOFOLLOW)` per component and never reopen the
    /// path by name.
    ///
    /// That is the limit of what a path can promise, and it is worth stating
    /// plainly: the returned `PathBuf` is used to *derive component names* and to
    /// judge containment, never to open anything. A later change to the
    /// filesystem cannot redirect the I/O, because the I/O does not go through
    /// this value. If the name and the descriptor have parted company, the run is
    /// refused rather than resolved against a directory the scenario substituted.
    #[cfg(unix)]
    fn validated_root(&self, rel: &str) -> Result<PathBuf, PittyError> {
        let root = self.canonical_cwd.clone();

        // Without a descriptor there is nothing to validate against;
        // `resolve_write_path` fails closed on its absence just below.
        let (Ok(fd), Some(captured)) = (self.cwd_fd.as_ref(), self.cwd_identity) else {
            return Ok(root);
        };

        // Re-read the identity FROM THE DESCRIPTOR rather than trusting the value
        // stored at capture time. The descriptor still refers to the same object
        // it always did, so this cannot change — which is exactly the point: it
        // is the fixed reference the name is measured against.
        let Some(live) = fd_identity(fd) else {
            return Err(PittyError::Process(format!(
                "cannot confirm the workspace directory for snapshot path '{rel}'"
            )));
        };
        if live != captured {
            return Err(PittyError::Process(format!(
                "the workspace descriptor for snapshot path '{rel}' no longer \
                 reports the directory it was opened on"
            )));
        }

        let Ok(named) = std::fs::metadata(&root) else {
            return Err(PittyError::Scenario(format!(
                "the workspace directory {} no longer exists, so snapshot path \
                 '{rel}' cannot be resolved against the directory this run started in",
                root.display()
            )));
        };

        use std::os::unix::fs::MetadataExt;
        if (named.dev(), named.ino()) != live {
            return Err(PittyError::Scenario(format!(
                "the workspace directory {} is no longer the directory this run \
                 started in, so snapshot path '{rel}' is refused; a snapshot is \
                 written only to the workspace captured before the scenario ran",
                root.display()
            )));
        }
        Ok(root)
    }

    pub fn resolve_write_path(
        &self,
        rel: &str,
        access: SnapshotAccess,
    ) -> Result<Result<SnapshotTarget, UnresolvablePath>, PittyError> {
        let candidate = self.cwd.join(rel);
        // The pre-spawn root, not a fresh `canonical_root(&self.cwd)`. See the
        // `canonical_cwd` field: re-resolving here let a child's rename move the
        // root out from under the descriptor the write actually uses.
        // The root is obtained from the identity check rather than beside it, so
        // the components below cannot be derived from an unvalidated path. A
        // pinned *string* is not an identity. The canonical path of a workspace
        // that was deleted and recreated under the same name is byte-identical,
        // while the directory is a different object — so the pin alone can still
        // drift from the descriptor every write actually goes through.
        //
        // Only the descriptor tracks identity, so the pin is checked against it:
        // `fstat` the held fd, `stat` the pinned path, and require the same
        // `(dev, ino)`. If they differ, the name now denotes a different
        // directory than the one captured before any scenario process existed,
        // and the component list derived from that name would be applied to the
        // wrong object. Refuse rather than resolve — the same fail-closed
        // direction as every other check here.
        #[cfg(unix)]
        let root = self.validated_root(rel)?;
        #[cfg(not(unix))]
        let root = self.canonical_cwd.clone();
        // A `..` may only cancel a component the kernel could actually traverse,
        // and that has to be checked on the scenario's *own* path before any
        // normalization runs — not only on a dangling link's destination.
        //
        // The guard originally lived solely in the dangling-link branch below,
        // so the identical shape written straight into `file:` skipped it
        // entirely: `file: blocker/../victim.snap` with `blocker` a regular file
        // normalized lexically to `victim.snap` and `--update` truncated that
        // real, unrelated file. The kernel refuses the same path with ENOTDIR at
        // `blocker` (1.2.2 surfaced exactly that), so the scenario could never
        // legitimately reach it.
        //
        // Placed before containment because it is a stricter question than
        // containment: the victim here is *inside* the workspace, so containment
        // is blind to it — this is data loss, not an escape.
        //
        // Scoped to components that *exist and are not directories*, which is
        // narrower than the guard used on a dangling link's destination — but
        // only when the run is actually going to create the missing components.
        //
        // The exemption's whole justification is that `--update` creates the
        // snapshot's parent directories, so `../new-dir/../w/out.snap` is a path
        // 1.2.2 recorded successfully (verified by running it) and refusing it
        // would break v1. That justification simply does not hold on a **read**:
        // nothing creates `missing/`, the kernel refuses `missing/../victim.snap`
        // with ENOENT, and 1.2.2 reported "not recorded" and failed the
        // assertion. Applying the exemption unconditionally let the lexical
        // normalization elide `missing/` and read `victim.snap` instead — so an
        // unreachable path *satisfied* the assertion by comparing against a
        // different file, exiting 0 where v1 exited 1. A false green is the worst
        // direction for a testing tool, which is why the caller must now say
        // which question it is asking.
        //
        // A non-directory is different in kind and is refused under both: no
        // amount of directory creation makes `regular-file/..` resolvable, so the
        // kernel's ENOTDIR is a permanent verdict and the lexical answer is a
        // file the scenario could never open.
        let climbs = match access {
            // Recording: a missing component is neither exempted nor predicted.
            // It is collected below as a `traversed_dir` and the recorder
            // *attempts* to create it — succeeding on a writable tree, failing
            // with the kernel's own error where v1 also failed. Only a permanent
            // refusal (a non-directory, an unsearchable directory) bars the climb
            // here, because no creation makes those traversable.
            SnapshotAccess::Record => climbs_through_existing_non_directory(&candidate),
            // Verifying: nothing will be created, so an absent component is just
            // as untraversable as a non-directory — exactly the rule already
            // applied to a dangling link's destination, and for the same reason.
            SnapshotAccess::Verify => climbs_through_missing_component(&candidate),
        };
        if climbs {
            return Ok(Err(unresolvable_climb(rel)));
        }
        // Canonicalize as far as the path exists: a brand-new snapshot file (and
        // possibly its parent dirs) does not exist yet, so we cannot canonicalize
        // the full path. We canonicalize the deepest existing ancestor (which
        // resolves any symlinks in the real portion) and re-append the not-yet-
        // created tail, then confirm the whole thing stays under the root.
        let resolved = resolve_existing_ancestor(&candidate);
        if !is_within(&resolved, &root) {
            return Err(PittyError::Scenario(format!(
                "snapshot path '{rel}' escapes the workspace; \
                 snapshot writes are confined to the workspace directory"
            )));
        }
        // Containment above is necessary but not sufficient: it is computed from
        // `canonicalize`, which fails with ENOENT on a *dangling* symlink and so
        // reports a planted `x.snap -> /outside/PWNED` as a path that merely does
        // not exist yet. Re-walk the components `rel` names with
        // `symlink_metadata` (which does not follow links) and refuse a dangling
        // link only when its destination falls outside the workspace. A link
        // that *does* resolve is left to the containment check above, which saw
        // through it.
        //
        // Two roots, deliberately: the scan walks from `self.cwd` because
        // `candidate` was built by joining onto it, so that is the prefix which
        // strips — while containment for a dangling destination is judged
        // against the *canonical* root, the same one used above. Comparing a
        // canonicalized destination against a non-canonical root would reject
        // valid paths wherever the workspace is reached through a link, which on
        // macOS is every temp directory (`/var` -> `/private/var`).
        //
        // When a dangling link *is* accepted, the same walk returns where it
        // lands, and that destination replaces `resolved` below. It has to: the
        // recorder opens every component `O_NOFOLLOW`, so handing it the link's
        // own name would have the writer refuse the very path the resolver just
        // approved — which is exactly what happened before this was threaded
        // through, `out.snap -> real/out.snap` failing with ELOOP at the write
        // after passing containment. Walking the destination also preserves the
        // property that makes any of this safe: the link's name is never
        // reopened, so re-pointing it afterwards cannot redirect the write.
        let resolved = match dangling_link_target(&self.cwd, &root, &candidate) {
            DanglingLink::None => resolved,
            DanglingLink::ResolvesInside(destination) => destination,
            DanglingLink::Refused => return Err(symlink_tail_error(rel)),
            DanglingLink::UnresolvableChain => return Ok(Err(unresolvable_chain(rel))),
            DanglingLink::UnresolvableClimb => return Ok(Err(unresolvable_climb(rel))),
        };
        // Containment was judged on the *normalized* location, but the recorder
        // has to walk an actual component sequence. Deriving that sequence here,
        // from the same normalization containment used, is what keeps the two
        // from disagreeing: a `file:` like `../new/../w/out.snap` normalizes
        // back inside the workspace (so containment rightly passes) while its
        // raw components step outside on the way, and a recorder walking the raw
        // list would `mkdir` outside the workspace even though the snapshot
        // itself lands inside. The sequence below is relative to the workspace
        // root and contains no `..`, so walking it cannot leave the root.
        let components = relative_components(&root, &resolved).ok_or_else(|| {
            PittyError::Scenario(format!(
                "snapshot path '{rel}' escapes the workspace; \
                 snapshot writes are confined to the workspace directory"
            ))
        })?;
        if components.is_empty() {
            return Err(PittyError::Scenario(format!(
                "snapshot path '{rel}' names the workspace directory itself, \
                 not a file to record"
            )));
        }
        // Fail closed, here rather than at prepare time. This is the first point
        // at which containment is actually required, so it is the first point at
        // which its absence may fail a run — a scenario with no snapshot step
        // never reaches this line. There is no unprotected branch: without the
        // descriptor no `SnapshotTarget` is produced at all.
        #[cfg(unix)]
        let root_fd = self
            .cwd_fd
            .as_ref()
            .map_err(|e| PittyError::Process(e.clone()))?
            .clone();

        // Directories the path names but `..` cancels. Collected only when
        // recording: a verifying run creates nothing, and a `..` across a
        // missing component has already been refused above for that reason.
        let traversed_dirs = match access {
            // Derived from the SAME resolved candidate containment judges, so the
            // two cannot be computed from different representations of the path.
            // A disagreement is a bug in this resolver, not a user error, and it
            // fails the run loudly rather than silently dropping the entry — see
            // `PathRepresentationMismatch`.
            SnapshotAccess::Record => cancelled_directories(&root, &candidate).map_err(|m| {
                PittyError::Scenario(format!(
                    "internal error resolving snapshot path '{rel}': the workspace root \
                     {} and the resolved path {} are not in the same representation; \
                     refusing rather than recording through a path that was not fully \
                     checked",
                    m.root.display(),
                    m.candidate.display()
                ))
            })?,
            SnapshotAccess::Verify => Vec::new(),
        };

        Ok(Ok(SnapshotTarget {
            display: candidate,
            root: root.clone(),
            components,
            traversed_dirs,
            #[cfg(unix)]
            root_fd,
        }))
    }

    /// Expand `${var}` placeholders in `input`.
    ///
    /// Resolution order per name: scenario `variables` first, then the
    /// parent-process environment captured at prepare time as a fallback, then
    /// the literal `${name}` text if still undefined.
    ///
    /// Why the parent-env fallback: dogfood "meta" scenarios spawn an inner
    /// `pitty` whose absolute path the surrounding CI exports as an
    /// environment variable (`PITTY_BIN`). Scenario `variables` cannot carry
    /// a caller-supplied path (they are baked into the YAML), and scenario `env`
    /// is injected into the spawned child rather than consulted by `${var}`
    /// expansion of `spawn.command`. Falling back to the parent env lets the
    /// caller parameterize a `spawn.command` path without editing the YAML. Why
    /// not error or special-case only `spawn`: keeping one uniform resolution
    /// rule for every step is simpler, and the precedence below preserves the
    /// prior behavior — scenario variables still win, so nothing a scenario
    /// already defined changes meaning.
    ///
    /// Unknown variables (absent from both sources) are left untouched as their
    /// literal `${name}` text rather than erroring: a missing variable is most
    /// often a typo the user will see verbatim in the sent input, which is
    /// easier to diagnose than a hard failure mid-run. Use `$$` to emit a
    /// literal `$`.
    ///
    /// Secret masking is unaffected: only values from `variables` flagged
    /// `secret: true` are registered for masking. Parent-env fallback values
    /// are never registered, matching the existing rule that only scenario-
    /// declared secrets are masked.
    pub fn expand(&self, input: &str) -> String {
        expand_with(&self.variables, &self.parent_env, input)
    }
}

/// Expand `${var}` placeholders against an explicit variable table and
/// parent-env fallback.
///
/// Why a free function rather than only the [`Workspace::expand`] method:
/// `Workspace::prepare` must expand the scenario-level `env` values *while it
/// is still assembling the struct*, so `self` does not exist yet. Sharing this
/// one body between `prepare` and the method is what guarantees the two
/// expansion sites cannot drift — precedence, `$$`, unknown-name handling and
/// the single pass are literally the same code, not a re-implementation.
///
/// See [`Workspace::expand`] for the resolution rules this implements.
fn expand_with(
    variables: &BTreeMap<String, String>,
    parent_env: &BTreeMap<String, String>,
    input: &str,
) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' {
            // Literal `$` via `$$`.
            if i + 1 < bytes.len() && bytes[i + 1] == b'$' {
                out.push('$');
                i += 2;
                continue;
            }
            // `${name}` form. `OPEN` skips the `${` prefix (2 bytes); the
            // matching `}` is `CLOSE` (1 byte) past the name.
            const OPEN: usize = 2; // length of "${"
            const CLOSE: usize = 1; // length of "}"
            if i + 1 < bytes.len() && bytes[i + 1] == b'{' {
                let name_start = i + OPEN;
                if let Some(close) = input[name_start..].find('}') {
                    let name_end = name_start + close;
                    let name = &input[name_start..name_end];
                    // Index just past the closing brace.
                    let after_close = name_end + CLOSE;
                    // Scenario variables win; the parent env is only a
                    // fallback, so a scenario that defines a variable keeps
                    // its prior meaning regardless of the ambient env.
                    match variables.get(name).or_else(|| parent_env.get(name)) {
                        Some(value) => out.push_str(value),
                        // Unknown in both: re-emit the verbatim `${name}`.
                        None => out.push_str(&input[i..after_close]),
                    }
                    i = after_close;
                    continue;
                }
            }
        }
        // Default: copy this byte's character. Indexing is safe because we
        // only ever advance by whole UTF-8 chars here.
        let ch = input[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Apply `0700` permissions to a directory (Unix only).
#[cfg(unix)]
fn set_permissions_0700(path: &Path) -> Result<(), PittyError> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .map_err(|e| PittyError::Process(format!("failed to set temp dir mode: {e}")))
}

/// No-op on non-Unix: those platforms use their default temp directory ACLs.
#[cfg(not(unix))]
fn set_permissions_0700(_path: &Path) -> Result<(), PittyError> {
    Ok(())
}

/// Canonicalize the workspace root for containment checks.
///
/// Falls back to a lexical normalization when the cwd does not yet exist on disk
/// (a `workspace.cwd` may name a dir created later), so containment still has a
/// stable root to compare against.
fn canonical_root(cwd: &Path) -> PathBuf {
    cwd.canonicalize()
        .unwrap_or_else(|_| lexical_normalize(cwd))
}

/// The Scenario error raised when a write path passes through a symlink.
///
/// Separate from the "escapes the workspace" wording because the symlink may
/// well point *inside* the workspace: we refuse it regardless, so the message
/// must not claim an escape we did not actually observe.
fn symlink_tail_error(rel: &str) -> PittyError {
    PittyError::Scenario(format!(
        "snapshot path '{rel}' passes through a symlink; \
         snapshot writes must not follow symlinks"
    ))
}

/// Why a write path could not be resolved to a location on disk.
///
/// Returned instead of a [`PittyError`] so the caller can report it the way
/// 1.2.2 did: as a **failed snapshot assertion** (exit 1, with a report) rather
/// than a scenario error (exit 2, no report).
///
/// The distinction is 1.2.2's own, and it is worth preserving exactly. Verified
/// by running it:
/// - a *statically* wrong path — `../../escape.snap`, which escapes no matter
///   what the filesystem holds — was a scenario error, exit 2, no report. The
///   author must fix the YAML.
/// - a path the *filesystem* refused at write time — ENOTDIR on a non-directory,
///   ENOENT on a dangling target, ELOOP on an over-long chain — surfaced as a
///   failed assertion, exit 1, with a full JSON report carrying the message.
///
/// pitty now refuses these before opening anything rather than discovering them
/// mid-write, but *when* the refusal is detected is an implementation detail;
/// what a consumer sees must not change. Collapsing the second class into the
/// first silently removed the report a pipeline parses, and would have broken
/// both the exit-code and the report-JSON contracts at once.
#[derive(Debug)]
pub struct UnresolvablePath {
    message: String,
}

impl UnresolvablePath {
    /// The reason, phrased for a snapshot assertion's failure message.
    pub fn into_message(self) -> String {
        self.message
    }
}

/// The refusal raised when a `..` cancels a component the kernel could not
/// traverse.
///
/// Worded separately from both the escape and the symlink messages because it is
/// neither: the path may well stay inside the workspace and name no symlink at
/// all (`regular-file/../victim.snap`). What is wrong is that the kernel would
/// fail resolving it — ENOENT on an absent component, ENOTDIR on one that is not
/// a directory — so the location lexical normalization produces is one the
/// scenario could never legitimately reach.
fn unresolvable_climb(rel: &str) -> UnresolvablePath {
    UnresolvablePath {
        message: format!(
            "cannot write snapshot {rel}: '..' climbs through a component that \
             is not a resolvable directory"
        ),
    }
}

/// The refusal raised when a link chain has no destination that may be acted on.
fn unresolvable_chain(rel: &str) -> UnresolvablePath {
    UnresolvablePath {
        message: format!(
            "cannot write snapshot {rel}: the symlink chain does not resolve \
             (a cycle, or more levels of symbolic links than the platform \
             resolves)"
        ),
    }
}

/// Canonicalize the deepest existing ancestor of `path` and re-append the
/// non-existent tail.
///
/// Why not `path.canonicalize()` directly: the snapshot file (and possibly its
/// parent directories) does not exist yet on a first `--update` record, so a
/// full canonicalize would fail. Resolving the existing prefix still follows any
/// symlink in the real portion of the path, so a symlink-out whose target
/// exists is caught here.
///
/// Why this is not sufficient on its own: `canonicalize` also fails with ENOENT
/// on a *dangling* symlink, which this loop cannot distinguish from a component
/// that truly does not exist yet. Callers must additionally run
/// [`path_crosses_symlink`], which sees links this resolver is blind to.
fn resolve_existing_ancestor(path: &Path) -> PathBuf {
    let mut existing = path;
    loop {
        if let Ok(real) = existing.canonicalize() {
            let tail = path.strip_prefix(existing).unwrap_or(Path::new(""));
            return lexical_normalize(&real.join(tail));
        }
        match existing.parent() {
            Some(parent) => existing = parent,
            // No ancestor exists (e.g. a relative path with no real root):
            // fall back to a purely lexical normalization.
            None => return lexical_normalize(path),
        }
    }
}

/// What the scan for a dangling symlink found on a `file:` path.
///
/// Returning the destination rather than a yes/no keeps the check and the thing
/// checked as one value — the same discipline the component sequence follows.
/// When the two were separate, the resolver could approve a dangling link while
/// the recorder was handed the link's *name* and refused it with `ELOOP`.
enum DanglingLink {
    /// No dangling link on the path; use the normally-resolved location.
    None,
    /// A dangling link whose destination is inside the workspace. The payload is
    /// that destination, which the recorder walks instead of the link's name.
    ResolvesInside(PathBuf),
    /// A dangling link that leaves the workspace. The caller refuses the path as
    /// a containment violation.
    Refused,
    /// A chain the kernel itself could not resolve: a cycle, or more levels of
    /// symbolic links than the platform resolves. Distinguished from
    /// [`Refused`](Self::Refused) so the message can say the path does not
    /// resolve rather than claim an escape that was never observed.
    UnresolvableChain,
    /// A `..` in the destination-plus-tail climbing through something that is
    /// not a resolvable directory. Separate from
    /// [`UnresolvableChain`](Self::UnresolvableChain) only so each reports the
    /// reason a reader can act on.
    UnresolvableClimb,
}

/// Memory bound on the set of paths visited while walking a dangling chain.
/// **Not** a bound on chain length, and **not** a model of the platform's
/// symlink limit.
///
/// # Why counting hops was abandoned
///
/// Two earlier versions of this constant tried to *be* the platform's limit —
/// first 16, then 32 — and both were wrong in the same structural way. Each
/// `read_link(&current)` makes the kernel resolve `current`'s own directory
/// components, and any symlinks among those are traversed *inside that one
/// call*: `out.snap -> d0/final` over a 31-link directory chain costs the kernel
/// 33 hops and the walk exactly 2. No value of a `read_link` counter reproduces
/// the kernel's arithmetic, because the two do not count the same events.
///
/// A count also broke v1 compatibility outright. Linux's `MAXSYMLINKS` is 40, so
/// a 33-to-40-hop chain that 1.2.2 recorded — resolution being the kernel's job
/// then — became an unresolvable assertion under a hardcoded 32. Keeping the
/// constant at 32 "as a liveness guard" did not fix that: `0..=32` still
/// completed at most 32 links, so the Linux break survived the rename.
///
/// # What replaced it
///
/// Resolvability is the kernel's answer (`ELOOP` from `read_link` is adopted
/// verbatim, at whatever limit the running platform has). The one question the
/// kernel *cannot* answer is a **dangling cycle** (`a -> b -> a`), where every
/// individual `read_link` succeeds and only the walk's own repetition reveals
/// it. That is now detected exactly, by remembering the paths visited, rather
/// than by proxy through a count — so a legitimate chain of any length the
/// kernel accepts resolves, and a cycle is caught on its first repeat.
///
/// This constant only caps how much the visited set may hold, so a pathological
/// chain cannot exhaust memory. It is far above any real chain (every platform's
/// own symlink limit is an order of magnitude lower, so the kernel's `ELOOP`
/// arrives long first), and exceeding it **refuses** rather than silently
/// truncating — a truncated walk would report a destination it never reached.
const MAX_VISITED_LINK_PATHS: usize = 4096;

/// Whether the kernel would descend through `probe` as a path component.
///
/// The single place the `..` guards ask about traversal, so neither can drift
/// back toward testing an attribute. On Unix the question goes to the kernel via
/// `open` (see [`crate::safepath::traversability`]); Windows has no equivalent
/// and no `O_SEARCH`, so it keeps the directory test it always had — `..` there
/// is resolved lexically by the OS anyway, and the snapshot writer on Windows is
/// path-based rather than descriptor-anchored (documented in SECURITY.md).
fn component_traverses(probe: &Path) -> bool {
    #[cfg(unix)]
    {
        matches!(
            crate::safepath::traversability(probe),
            crate::safepath::Traversability::Traversable
        )
    }
    #[cfg(not(unix))]
    {
        probe.metadata().is_ok_and(|m| m.is_dir())
    }
}

/// Whether the kernel *permanently* refuses to traverse `probe`.
///
/// Distinct from `!component_traverses`: an **absent** component is not a
/// refusal, because `--update` creates a snapshot's missing parent directories.
/// Only the candidate-path guard may use this looser test; see
/// [`climbs_through_existing_non_directory`].
fn component_traversal_refused(probe: &Path) -> bool {
    #[cfg(unix)]
    {
        matches!(
            crate::safepath::traversability(probe),
            crate::safepath::Traversability::Refused
        )
    }
    #[cfg(not(unix))]
    {
        probe.metadata().is_ok_and(|m| !m.is_dir())
    }
}

/// Whether `path` uses `..` to cancel a component the kernel could not traverse.
///
/// Lexical `..` elimination is only sound across components that are really
/// there *and are directories*. The kernel resolves left to right, so
/// `missing/../victim.snap` fails with `ENOENT` at `missing` and
/// `regular-file/../victim.snap` fails with `ENOTDIR` at `regular-file`; neither
/// ever reaches `victim.snap` — while a purely lexical normalization cancels the
/// pair and produces a path to a real file. Acting on the lexical answer let a
/// snapshot the scenario could not reach land on (and destroy) a neighbouring
/// file.
///
/// Walks the path as the kernel would: each `..` must pop a component this walk
/// has already confirmed is a **resolvable directory**. Existence alone is not
/// enough, and checking only existence was the original miss: a regular file, a
/// FIFO, a device node and a dangling symlink all make `symlink_metadata`
/// succeed while `..` across them is exactly as unresolvable as across something
/// absent. The state below therefore tracks "the kernel would already have
/// failed here", not "this component is missing".
///
/// The directory test follows symlinks deliberately (`metadata`, not
/// `symlink_metadata`): `dirlink/..` where `dirlink -> realdir` is something the
/// kernel resolves happily, so refusing it would over-correct. A *dangling*
/// link fails that test, which is right — the kernel cannot traverse it either.
///
/// Only the segment *below* the deepest traversable prefix matters; everything
/// above resolves normally, which is why the walk simply stops caring once a
/// component is unusable and a later `..` tries to cross it.
/// Whether `path` uses `..` to cancel a component that exists but is **not** a
/// directory.
///
/// The narrow half of [`climbs_through_missing_component`], for the scenario's
/// own `file:` value. Both ask what the kernel would do with a `..`, but they
/// differ on the *absent* component, because the two callers differ on whether
/// absence is permanent:
///
/// - On a **dangling link's destination**, a missing component stays missing —
///   nothing creates it — so `link/../victim.snap` with `link -> missing` can
///   never resolve and the wider guard refuses it.
/// - On the **candidate path**, `--update` creates the snapshot's parent
///   directories, so `../new-dir/../w/out.snap` is a path that legitimately
///   resolves once recording proceeds. 1.2.2 records it (verified by
///   execution), so refusing it would be a v1 break.
///
/// A non-directory is permanent in both: no directory creation makes
/// `regular-file/..` traversable, so the kernel's ENOTDIR is final and the path
/// the lexical shortcut produces is one the scenario could never open.
fn climbs_through_existing_non_directory(path: &Path) -> bool {
    use std::path::Component;

    let mut stack: Vec<std::ffi::OsString> = Vec::new();
    let mut prefix = PathBuf::new();
    // Depth inside a component that exists but is not a directory. Unlike the
    // wider guard, an *absent* component does not open this state.
    let mut blocked_depth: usize = 0;

    for component in path.components() {
        match component {
            Component::ParentDir => {
                if blocked_depth > 0 {
                    return true;
                }
                stack.pop();
            }
            Component::CurDir => {}
            Component::Normal(name) => {
                stack.push(name.to_os_string());
                if blocked_depth > 0 {
                    blocked_depth += 1;
                    continue;
                }
                let mut probe = prefix.clone();
                probe.extend(stack.iter());
                // Only a component the kernel *refuses* blocks; an absent one is
                // deliberately ignored here, because `--update` may create it.
                // The kernel is asked directly (see `safepath::traversability`)
                // rather than consulted through `metadata`, which answered "is a
                // directory" for a `chmod 000` directory the kernel will not
                // traverse — and `--update` then destroyed a real file through
                // the `..` that attribute wrongly allowed.
                let blocks = component_traversal_refused(&probe);
                if blocks {
                    blocked_depth = 1;
                }
            }
            other => prefix.push(other.as_os_str()),
        }
    }
    false
}

fn climbs_through_missing_component(path: &Path) -> bool {
    use std::path::Component;

    // Where the KERNEL would stand, not where a lexical walk would.
    //
    // A `..` after a symlink has been traversed steps to the *destination's*
    // parent, which is the whole difference between this and popping a name off
    // a stack. With `link -> real/subdir`, `link/..` lands in `real`; a lexical
    // pop lands in the workspace root. That divergence let
    // `link/../missing/../victim.snap` pass its guard by checking a `missing`
    // that exists at the lexical position while the kernel was looking at
    // `real/missing`, which does not exist — the path the kernel refuses with
    // ENOENT satisfied the assertion by comparing against a different file.
    //
    // The asymmetry is essential to the bug: the same name must exist at the
    // lexical position and be absent at the destination. Neither a plain
    // symlinked component nor a plain missing one exposes it.
    //
    // So position is resolved rather than computed. `here` is canonicalized
    // whenever the filesystem can answer, which is what makes a following `..`
    // move the way the kernel moves. When it cannot answer — the component does
    // not exist yet — the walk has already entered the untraversable state
    // below, so no further resolution is needed or possible.
    let mut here = PathBuf::new();
    // How many levels deep we are inside a component the kernel could not have
    // traversed — absent, or present but not a directory. While this is non-zero
    // the kernel would already have failed, so a `..` here is the unsound climb
    // we are looking for.
    let mut untraversable_depth: usize = 0;

    for component in path.components() {
        match component {
            Component::ParentDir => {
                if untraversable_depth > 0 {
                    // Climbing back out of something the kernel could not enter:
                    // it would have failed at that component (ENOENT if absent,
                    // ENOTDIR if it is not a directory), so the lexical shortcut
                    // is unsound and the caller must refuse.
                    return true;
                }
                // Step to the parent of where the kernel actually stands. `here`
                // was canonicalized on the way in, so if a symlink was followed
                // this pops the destination's parent rather than the link's.
                here.pop();
            }
            Component::CurDir => {}
            Component::Normal(name) => {
                here.push(name);
                if untraversable_depth > 0 {
                    untraversable_depth += 1;
                    continue;
                }
                // The kernel's own answer, not an attribute standing in for it:
                // a directory with no search permission passes every `metadata`
                // test and is still untraversable. Here *both* absence and
                // refusal block, because on a dangling link's destination
                // nothing ever creates the missing component.
                if !component_traverses(&here) {
                    untraversable_depth = 1;
                    continue;
                }
                // Traversable, so the kernel can tell us where it now stands.
                // Canonicalizing here is what carries a followed symlink into
                // the position a later `..` pops from.
                if let Ok(real) = here.canonicalize() {
                    here = real;
                }
            }
            // RootDir / Prefix: the anchor the rest is relative to.
            other => here.push(other.as_os_str()),
        }
    }
    false
}

/// Where a dangling symlink would eventually write, if it can be determined.
///
/// `canonicalize` cannot resolve a link whose target does not exist yet, so it
/// reports `ENOENT` identically for `out.snap -> real/out.snap` (inside the
/// workspace, the target simply not recorded yet) and
/// `out.snap -> ../outside/x` (an escape). Refusing both was a v1 break: the
/// first is an ordinary scenario that passed on 1.2.2.
///
/// This walks the chain with `read_link` so the two can be told apart, and
/// returns the location the write would land at, for the caller to judge with
/// the same containment rule it applies to everything else.
///
/// Cases it has to get right, all verified against the real filesystem:
/// - **relative target** — resolved against the *link's own directory*, not the
///   workspace root or the process cwd, which is what the kernel does.
/// - **absolute target** — used as-is; containment then judges it.
/// - **chain of links** — followed until a non-link or a missing entry is
///   reached, so `a -> b -> real/c` is judged on `real/c`.
/// - **chain that leaves** — the final location is outside the root, so the
///   caller refuses it exactly as it would a direct escape.
/// - **loop** — detected by the visited-path set, not by a hop count; `None` is
///   returned and the caller refuses, because a cycle has no location to judge.
fn dangling_link_destination(link: &Path) -> Option<PathBuf> {
    let mut current = link.to_path_buf();
    // Cycle detection, and the only bound on this walk. A cycle is precisely a
    // path visited twice, so a set answers it exactly; a counter only ever
    // approximated it, and the approximation truncated legitimate chains the
    // kernel resolves (see [`MAX_VISITED_LINK_PATHS`]). Authority over "is this
    // chain resolvable at all" still belongs to the `read_link` calls inside.
    let mut visited: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    loop {
        if visited.len() >= MAX_VISITED_LINK_PATHS {
            // Refuse rather than truncate: a walk stopped early has not reached
            // a destination, and returning the path it happened to be standing
            // on would approve a location never actually resolved.
            return None;
        }
        if !visited.insert(current.clone()) {
            // Already stood here: a dangling cycle. Every `read_link` in
            // `a -> b -> a` succeeds, so this repetition is the only evidence
            // that exists, and no kernel call can supply it.
            return None;
        }
        let target = match std::fs::read_link(&current) {
            Ok(target) => target,
            // EINVAL ("exists, is not a symlink") and ENOENT ("does not exist")
            // are the two legitimate ways a chain ends: the first is a real file
            // or directory, the second the not-yet-recorded case this whole
            // resolution exists to support. Either way `current` is where the
            // chain lands.
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::InvalidInput | std::io::ErrorKind::NotFound
                ) =>
            {
                return Some(current);
            }
            // Anything else is the kernel *refusing to resolve this path*, and
            // it is authoritative in a way this loop cannot be. ELOOP is the
            // important one: `read_link` traverses the path's own directory
            // components internally, and those intermediate hops are invisible
            // here. `out.snap -> d0/final` over a 31-link directory chain is 33
            // hops to the kernel and exactly 2 to this loop, so a counter of
            // `read_link` calls resolved a path the kernel refuses — and
            // `--update` then wrote through it. Treating the kernel's own error
            // as the answer closes that gap without needing to know the
            // platform's limit. ENOTDIR (a non-directory component) and EACCES
            // (an unreadable directory) are refused for the same reason: the
            // chain has no destination we may act on.
            Err(_) => return None,
        };
        current = if target.is_absolute() {
            target
        } else {
            // A relative link target is relative to the directory holding the
            // link, which is what the kernel resolves it against.
            let parent = current.parent()?;
            // The parent is *canonicalized* before the join, so a `..` in the
            // target is applied to the directory the kernel would really be
            // standing in rather than cancelling a name lexically.
            //
            // This is what makes the kernel the authority on hop budget. For
            // `out.snap -> d0/final` where `final -> ../victim.snap` and `d0` is
            // a 31-link chain to `real`, the lexical join produced
            // `d0/../victim.snap`; `..` then erased `d0` without anyone ever
            // paying for its 31 hops, and a path costing the kernel 33 hops
            // (ELOOP, which 1.2.2 surfaced) resolved here at a counted cost of
            // 2. Canonicalizing the parent makes the kernel walk `d0` for real,
            // so it charges those hops and answers ELOOP itself — at whatever
            // limit the running platform has, with no constant to match.
            //
            // A parent that cannot be canonicalized is not fatal: on a dangling
            // chain the intermediate directory may legitimately not exist yet
            // (`out.snap -> not-yet/sub/../x`). Falling back to the lexical join
            // keeps that case working, and it is sound precisely because a path
            // that does not exist has no hops to charge — while
            // `climbs_through_missing_component` still refuses a `..` that
            // crosses such a component.
            match parent.canonicalize() {
                Ok(real_parent) => real_parent.join(target),
                Err(_) => parent.join(target),
            }
        };
    }
}

/// Whether any component of `path` below `root` is a symlink that cannot be
/// resolved to a location inside `root`.
///
/// Uses `symlink_metadata` so a link is reported as a link instead of being
/// followed to (or hidden by) its target; an `Err` means the component really
/// does not exist, which is the legitimate "not recorded yet" case and not a
/// symlink.
///
/// Why the walk starts at `root` rather than the filesystem root: the workspace
/// root itself is routinely reached through a symlink that is none of the
/// scenario's doing (`/tmp` -> `/private/tmp` on macOS, a symlinked checkout in
/// CI), and refusing those would break ordinary runs. Only the components the
/// scenario's `file:` value actually names are the scenario's to control, so
/// only those are scanned.
fn dangling_link_target(walk_root: &Path, canonical_root: &Path, path: &Path) -> DanglingLink {
    // A candidate outside the root has already been rejected by containment; if
    // the prefix does not strip, scan the whole path rather than silently
    // skipping the check.
    let below = path.strip_prefix(walk_root).unwrap_or(path);
    // Kept so the components *after* a dangling link can be re-appended to its
    // destination. Dropping them silently redirected the write: `link ->
    // missing-dir` with `file: link/out.snap` resolved to `missing-dir` alone,
    // so the snapshot became a *file* named `missing-dir` and the `out.snap` the
    // scenario asked for never existed. The path checked and the path used must
    // be the same one.
    let all: Vec<_> = below.components().collect();
    let mut probe = walk_root.to_path_buf();
    for (index, component) in all.iter().enumerate() {
        probe.push(component.as_os_str());
        let Ok(meta) = probe.symlink_metadata() else {
            // The entry does not exist at all — the ordinary "not recorded yet"
            // case, and nothing further down can exist either.
            return DanglingLink::None;
        };
        if !meta.file_type().is_symlink() {
            continue;
        }
        // A symlink whose target resolves is *not* refused here: containment
        // already judged it, because `resolve_existing_ancestor` canonicalized
        // through it, and the component sequence handed to the recorder is the
        // canonical one. A link pointing outside the workspace was therefore
        // already rejected above; one pointing inside is an ordinary repository
        // layout (a shared fixture directory, say) and must keep working.
        //
        // `canonicalize` alone is *not* a sound test for "this path resolves",
        // which is why `metadata` has to agree before we skip. `realpath`
        // collapses `..` lexically, so for `out.snap -> blocker/../victim.snap`
        // with `blocker` a regular file it happily returns `.../victim.snap`
        // while `stat` and `open` on the same name both fail with ENOTDIR
        // (verified at the libc level, not just through Rust). Skipping on
        // `canonicalize` alone therefore classified this link as an ordinary
        // resolvable one, containment saw a legal in-workspace destination, and
        // `--update` truncated an unrelated real file that the scenario could
        // never have opened.
        //
        // `metadata` and not `symlink_metadata`: `probe` is the link's own name,
        // and `symlink_metadata` deliberately does not follow it, so it succeeds
        // for *every* link and would make this test a no-op. `metadata` performs
        // the same full traversal `open` would, so it reports the ENOTDIR (and
        // the ELOOP of an over-long directory-symlink chain) that `realpath`
        // hides. Requiring both keeps genuine resolvable links working while
        // sending these shapes down the hand-resolution path below.
        //
        // ENOENT is excluded from the refusal: a dangling link whose target does
        // not exist yet is the ordinary "not recorded yet" case this whole
        // resolution exists to support, and it is handled below. Only an error
        // meaning *the kernel would not traverse this* (ENOTDIR, ELOOP, EACCES)
        // disqualifies the skip, and that case is adjudicated just below.
        let kernel_reached_it = probe.metadata().is_ok();
        if probe.canonicalize().is_ok() && kernel_reached_it {
            continue;
        }
        // The kernel's verdict on this exact path, taken before any hand
        // resolution and treated as final when it is a refusal.
        //
        // This is the check that actually enforces the hop budget, and it has to
        // happen here rather than inside the walk. The hand walk canonicalizes
        // each intermediate parent, and every one of those calls gets a *fresh*
        // hop budget from the kernel — so a path the kernel refuses as a whole
        // (ELOOP over `out.snap -> d0/final` with `d0` a 31-link chain, which
        // 1.2.2 surfaced) decomposes into steps that each stay under the limit
        // and resolve. Only asking about the whole path preserves the budget.
        //
        // ENOENT is again the exception, and the reason this cannot simply be a
        // `canonicalize().is_ok()` gate: the dangling case that the rest of this
        // function exists to support reports ENOENT here and must proceed.
        //
        // Deliberately *not* a reimplementation of the limit: whatever the
        // running kernel enforces — 32 on macOS, 40 on Linux — is what this
        // adopts, which is also what keeps a 33-to-40-hop chain that 1.2.2
        // recorded on Linux recording rather than becoming a scenario error.
        //
        // `metadata` is the oracle rather than `canonicalize` because
        // `canonicalize` is the more permissive of the two and disagrees with
        // the operation that ultimately runs. At exactly the boundary depth,
        // `canonicalize` returns Ok while `metadata` and `File::create` on the
        // same path both return ELOOP (verified by execution). `metadata`
        // performs the identical traversal `open` will, so trusting it is what
        // makes this check agree with the write that follows.
        let kernel_refused_path = probe
            .metadata()
            .is_err_and(|e| e.kind() != std::io::ErrorKind::NotFound);
        if kernel_refused_path {
            return DanglingLink::UnresolvableClimb;
        }
        // The link is *dangling*, and containment is blind to it: `canonicalize`
        // answers ENOENT identically for `out.snap -> real/out.snap` (inside,
        // target simply not recorded yet) and `out.snap -> ../outside/PWNED` (an
        // escape). Refusing both broke the first, which is an ordinary v1
        // scenario. Resolve the chain by hand and judge where it would actually
        // land, using the same containment rule as everything else.
        //
        // `None` means the chain has no destination this code may act on: a
        // cycle, or a path the kernel itself refused to resolve (ELOOP from a
        // directory-symlink chain, ENOTDIR, EACCES). Not an escape — nothing was
        // ever resolved far enough to judge containment — so it is reported as
        // unresolvable rather than as a containment violation.
        let Some(destination) = dangling_link_destination(&probe) else {
            return DanglingLink::UnresolvableChain;
        };
        // Re-append everything that followed the link before judging anything.
        // `link -> missing-dir` with `file: link/out.snap` must land at
        // `missing-dir/out.snap`, not at `missing-dir`.
        //
        // Containment is judged on destination-plus-tail *together*, because
        // either half can move the result across the boundary and only the
        // combination says where the write actually goes:
        //   - `link -> ../outside` with tail `../back-inside/x.snap` normalizes
        //     back in, so judging the destination alone would wrongly refuse it;
        //   - `link -> inside-dir` with tail `../../escape.snap` normalizes out,
        //     so judging the destination alone would wrongly allow it.
        // `resolve_existing_ancestor` applies the same normalization the rest of
        // the resolver uses, so the two cannot disagree.
        let mut full = destination;
        for tail in &all[index + 1..] {
            full.push(tail.as_os_str());
        }
        // A `..` in the tail may only cancel a component that actually EXISTS.
        //
        // The lexical normalization below happily erases `missing/..`, but the
        // kernel does not: resolving `link/../victim.snap` where `link ->
        // missing` has to traverse `missing` first and fails with ENOENT. Taking
        // the lexical answer meant a path the scenario could never legitimately
        // reach resolved onto a real neighbouring file — and `--update` then
        // truncated and rewrote it. That is silent data loss, not a containment
        // escape: the victim is inside the workspace, and 1.2.2 left it alone.
        //
        // This is an extra constraint on *how* destination-plus-tail is
        // normalized, not a reason to judge the halves separately — containment
        // still needs the combined location (see the comment above).
        if climbs_through_missing_component(&full) {
            return DanglingLink::UnresolvableClimb;
        }
        // The destination may itself not exist yet (that is the whole point), so
        // it gets the same deepest-existing-ancestor treatment as the original
        // candidate before the containment comparison.
        let landing = resolve_existing_ancestor(&full);
        if !is_within(&landing, canonical_root) {
            return DanglingLink::Refused;
        }
        // Accepted: hand back where it lands, so the recorder walks the
        // destination rather than the link's own name.
        return DanglingLink::ResolvesInside(landing);
    }
    DanglingLink::None
}

/// `(dev, ino)` of the directory a descriptor refers to.
///
/// Read via `fstat` on the descriptor itself, so it is the identity of the
/// *object* rather than of whatever a name resolves to at some later moment.
#[cfg(unix)]
fn fd_identity(fd: &std::os::fd::OwnedFd) -> Option<(u64, u64)> {
    use std::os::unix::io::AsRawFd;
    // SAFETY: `fd` is a live, owned descriptor for the duration of the call.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd.as_raw_fd(), &mut st) } != 0 {
        return None;
    }
    Some((st.st_dev as u64, st.st_ino as u64))
}

/// Lexically normalize a path by resolving `.`/`..` segments without touching
/// the filesystem. Used as a fallback when a path (or root) is not yet on disk.
fn lexical_normalize(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                // Pop the last real segment; if there is none, keep the `..` so a
                // path that climbs above its root still reads as "outside".
                if !out.pop() {
                    out.push("..");
                }
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Whether `path` is the root itself or lies beneath it.
fn is_within(path: &Path, root: &Path) -> bool {
    path == root || path.starts_with(root)
}

/// The components of `path` relative to `root`, as plain names.
///
/// `path` must already be normalized (it comes from `resolve_existing_ancestor`,
/// which lexically resolves `.` and `..`) and contained within `root`. Returns
/// `None` if it is not under `root`, or if any surviving component is not a
/// plain name — a `..` that normalization could not cancel, or a root/prefix
/// segment. Both would let a traversal leave the workspace, so they are refused
/// rather than sanitized: silently dropping a `..` would write to a different
/// file than the scenario asked for.
/// The workspace-relative directories `rel` names that `..` then cancels.
///
/// # Why these have to be carried rather than predicted
///
/// Lexical normalization is what makes `missing/../victim.snap` reduce to
/// `victim.snap`, and the reduction is *correct* — that is where the write
/// lands. But it also erases the fact that the scenario named `missing/`, and
/// 1.2.2 did not erase it: it called `create_dir_all` on the raw path, which
/// materialized `missing/` as a side effect and, on a workspace that is not
/// writable, failed with `EACCES` before writing anything.
///
/// Four rounds of this bug were all the same mistake — the code *predicting* an
/// outcome the kernel decides. The last prediction standing was the exemption
/// that let a `..` cross a missing component "because `--update` will create
/// it", which was false twice over: `--update` could not create it when the
/// parent was read-only, and the normalized component list meant nothing ever
/// tried to. Returning the cancelled directories lets the recorder *attempt* the
/// creation, so a writable tree behaves as v1 and a read-only one fails as v1,
/// with nothing left to predict.
///
/// Only components below the root are returned, and only those a `..` actually
/// cancels; a directory that survives normalization is already in the component
/// list and is created by the ordinary walk.
/// The two ways of naming "where this path points" disagreed.
///
/// # Why this is an error and not a `None`
///
/// A silent `None` here has now caused **three** separate data-loss BLOCKERs.
/// Each time, a path arrived in a representation the lexical `strip_prefix`
/// could not match against the canonical root, the cancelled directory was
/// quietly discarded, the recorder had nothing to attempt, and `--update`
/// overwrote a real snapshot on a tree where 1.2.2 refused:
///
/// 1. a **relative** `workspace.cwd` (`w`) against an absolute canonical root;
/// 2. a **symlinked component** canonicalizing to its target;
/// 3. a **symlinked workspace** plus an **absolute** `file:`, where the absolute
///    path re-rooted to `/` through `RootDir`.
///
/// The representation mismatch is now structurally impossible — the components
/// are derived from the same resolved `candidate` containment judges, stripped
/// against the same root — but if a future change reintroduces one, it must be
/// loud. macOS's `/var` -> `/private/var` is an ordinary alias of exactly this
/// kind, so the condition is reachable in normal use, not just under attack.
#[derive(Debug)]
struct PathRepresentationMismatch {
    root: PathBuf,
    candidate: PathBuf,
}

fn cancelled_directories(
    root: &Path,
    candidate: &Path,
) -> Result<Vec<Vec<std::ffi::OsString>>, PathRepresentationMismatch> {
    use std::path::Component;

    // Anchored on the SAME resolved path containment judges, and on the same
    // canonical root it strips against — see `PathRepresentationMismatch` for
    // why this is now one derivation instead of two.
    //
    // `candidate` has already been canonicalized-as-far-as-it-exists by the
    // caller, so a symlinked workspace, a relative `cwd`, an absolute `file:`
    // and a `./`-laden one all arrive here in the single representation the
    // containment check uses. Re-deriving this from the raw `rel` string is what
    // produced three separate data-loss bugs: an absolute `rel` re-rooted to `/`
    // via `RootDir`, a relative `cwd` stayed relative, and a symlinked component
    // canonicalized to its target — each time `strip_prefix` failed and the
    // entry was silently discarded.
    // Re-express `candidate` in the ROOT's representation while keeping its `..`
    // structure intact.
    //
    // Neither input works alone, which is the whole lesson of the three bugs:
    // the fully-resolved path has already had its `..` eliminated (nothing left
    // to record), while the raw candidate still carries the `..` but may name the
    // workspace through a symlink, a relative `cwd`, or an absolute path — the
    // representations that kept failing to strip.
    //
    // Canonicalizing the deepest existing ancestor puts the prefix into the same
    // representation as `root` (that is precisely what `resolve_existing_ancestor`
    // does for containment), and the not-yet-existing tail is appended
    // unnormalized so its `..` components survive to be counted below.
    let anchored = anchor_in_root_representation(candidate);

    // Walk the anchored path the way the kernel resolves it, maintaining the
    // walk's ACTUAL position at every step rather than deciding once where it
    // began.
    //
    // A path may leave the workspace and come back — `../outside/new/../../w/...`
    // is a shape 1.2.2 records — and it may do so repeatedly. An earlier version
    // sampled "am I inside the root" once, at the start, and gated recording on
    // that sample. Everything cancelled AFTER a re-entry was therefore discarded,
    // so `w/inside/../victim.snap` reached at the end of an excursion recorded
    // nothing: the recorder never attempted `w/inside`, the read-only refusal
    // disappeared, and `--update` destroyed a real snapshot that 1.2.2 preserved.
    //
    // Position is now a full path, not a flag. `here` is where the walk stands
    // after each component, so whether a cancellation is inside the root is asked
    // fresh every time — and holds however many times the path exits and
    // re-enters, which is the property this has to guarantee for any spelling of
    // a location.
    let mut out: Vec<Vec<std::ffi::OsString>> = Vec::new();
    let mut here = PathBuf::new();
    let mut dropped_outside = 0usize;

    for component in anchored.components() {
        match component {
            Component::Normal(name) => here.push(name),
            Component::CurDir => {}
            Component::RootDir => here.push(Component::RootDir.as_os_str()),
            Component::Prefix(p) => here.push(p.as_os_str()),
            Component::ParentDir => {
                // The directory this `..` cancels is wherever the walk currently
                // stands. It is named by the path and will not survive
                // normalization, so it is the recorder's to attempt — but only
                // when it lies inside the workspace.
                let cancelled = here.clone();
                if !here.pop() {
                    // Climbed above the filesystem root; nothing to cancel.
                    continue;
                }
                match relative_components(root, &cancelled) {
                    Some(seq) if !seq.is_empty() => {
                        if !out.contains(&seq) {
                            out.push(seq);
                        }
                    }
                    // The cancelled directory is the root itself, or lies
                    // outside it. Deliberately not recorded, and counted rather
                    // than dropped on the floor: the recorder may only create
                    // directories inside the workspace, and containment judges
                    // separately whether a path that leaves ever comes back.
                    //
                    // Explicit because a silent discard at this site is exactly
                    // what hid four data-loss bugs; see
                    // `PathRepresentationMismatch`.
                    _ => dropped_outside += 1,
                }
            }
        }
    }

    // `dropped_outside` counts cancellations deliberately not recorded because
    // they name the root itself or a location above it. It is kept as a named
    // value rather than an anonymous fallthrough so the decision is visible at
    // this site — four data-loss bugs came from an entry disappearing here
    // without anyone having written down that it could.
    let _ = dropped_outside;

    Ok(out)
}

/// Put `path`'s existing prefix into canonical form while leaving the rest —
/// including any `..` — exactly as written.
///
/// The companion to [`resolve_existing_ancestor`], which normalizes `..` away.
/// Here the `..` components are the payload, so only the prefix is rewritten.
fn anchor_in_root_representation(path: &Path) -> PathBuf {
    let mut existing = path;
    loop {
        if let Ok(real) = existing.canonicalize() {
            let tail = path.strip_prefix(existing).unwrap_or(Path::new(""));
            return real.join(tail);
        }
        match existing.parent() {
            Some(parent) => existing = parent,
            None => return path.to_path_buf(),
        }
    }
}

fn relative_components(root: &Path, path: &Path) -> Option<Vec<std::ffi::OsString>> {
    use std::path::Component;
    let tail = path.strip_prefix(root).ok()?;
    let mut out = Vec::new();
    for component in tail.components() {
        match component {
            Component::Normal(name) => out.push(name.to_os_string()),
            Component::CurDir => {}
            // `..`, `/`, or a Windows prefix surviving normalization means the
            // path does not reduce to a location inside the root after all.
            _ => return None,
        }
    }
    Some(out)
}

/// Replace every registered secret value in `text` with `***`.
///
/// Applied at log-write and error-format boundaries so secret values never
/// reach disk or terminal output. We walk `text` once and, at each byte
/// position, check whether any secret begins there — rather than calling
/// `str::replace` once per secret, which would allocate a fresh intermediate
/// `String` for every secret in the list. Secret lists are tiny, so checking
/// each against the current position is cheap and avoids the chained
/// allocations. Longest secrets are tried first so a secret that is a prefix of
/// another does not mask only part of the longer one.
pub fn mask_secrets(text: &str, secrets: &[String]) -> String {
    let mut active: Vec<&str> = secrets
        .iter()
        .map(String::as_str)
        .filter(|s| !s.is_empty())
        .collect();
    if active.is_empty() {
        return text.to_string();
    }
    active.sort_by_key(|s| std::cmp::Reverse(s.len()));

    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while !rest.is_empty() {
        let matched = active.iter().find(|secret| rest.starts_with(**secret));
        match matched {
            Some(secret) => {
                out.push_str("***");
                rest = &rest[secret.len()..];
            }
            None => {
                // No secret starts here: copy one whole char and advance past
                // it so we never split a multibyte UTF-8 boundary.
                let ch = rest.chars().next().expect("rest is non-empty");
                out.push(ch);
                rest = &rest[ch.len_utf8()..];
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace_with_vars(pairs: &[(&str, &str)], secrets: &[&str]) -> Workspace {
        workspace_with_vars_and_env(pairs, &[], secrets)
    }

    /// Build a workspace with explicit scenario variables and a fixed parent
    /// env so expansion precedence can be tested without touching `std::env`.
    fn workspace_with_vars_and_env(
        vars: &[(&str, &str)],
        parent_env: &[(&str, &str)],
        secrets: &[&str],
    ) -> Workspace {
        let mut variables = BTreeMap::new();
        for (k, v) in vars {
            variables.insert(k.to_string(), v.to_string());
        }
        let mut env_map = BTreeMap::new();
        for (k, v) in parent_env {
            env_map.insert(k.to_string(), v.to_string());
        }
        Workspace {
            canonical_cwd: canonical_root(Path::new(".")),
            cwd: PathBuf::from("."),
            variables,
            parent_env: env_map,
            env: Vec::new(),
            secrets: secrets.iter().map(|s| s.to_string()).collect(),
            _temp: None,
            // This helper builds workspaces for `${var}`-expansion tests only;
            // they never resolve a write path. The current directory is a
            // stand-in so the field can be non-optional (see `cwd_fd`).
            #[cfg(unix)]
            cwd_identity: crate::safepath::open_dir_fd(Path::new("."))
                .ok()
                .and_then(|fd| fd_identity(&fd)),
            #[cfg(unix)]
            cwd_fd: crate::safepath::open_dir_fd(Path::new("."))
                .map(std::sync::Arc::new)
                .map_err(|e| e.to_string()),
        }
    }

    #[test]
    fn expands_known_variable() {
        // ${name} must be replaced by the variable's value.
        let ws = workspace_with_vars(&[("user", "alice")], &[]);
        assert_eq!(ws.expand("hello ${user}!"), "hello alice!");
    }

    #[test]
    fn leaves_unknown_variable_literal() {
        // An undefined ${name} (absent from variables and parent env) must be
        // left verbatim so the typo is visible.
        let ws = workspace_with_vars(&[], &[]);
        assert_eq!(ws.expand("value=${missing}"), "value=${missing}");
    }

    #[test]
    fn expands_from_parent_env_when_not_a_scenario_variable() {
        // A name absent from scenario variables but present in the parent env
        // must expand from the env: this is how a meta scenario resolves the
        // CI-exported ${PITTY_BIN} path into spawn.command.
        let ws = workspace_with_vars_and_env(&[], &[("PITTY_BIN", "/abs/pitty")], &[]);
        assert_eq!(ws.expand("${PITTY_BIN} run x"), "/abs/pitty run x");
    }

    #[test]
    fn scenario_variable_takes_precedence_over_parent_env() {
        // When a name exists in both, the scenario variable wins so a scenario
        // keeps its prior meaning regardless of the ambient environment.
        let ws = workspace_with_vars_and_env(&[("X", "from-var")], &[("X", "from-env")], &[]);
        assert_eq!(ws.expand("${X}"), "from-var");
    }

    #[test]
    fn unknown_in_both_variables_and_env_stays_literal() {
        // A name in neither source must remain literal, not silently blank.
        let ws = workspace_with_vars_and_env(&[("a", "1")], &[("b", "2")], &[]);
        assert_eq!(ws.expand("${c}"), "${c}");
    }

    #[test]
    fn expansion_is_single_pass_not_recursive() {
        // (r-1) A variable whose value itself contains `${other}` is expanded in
        // a single left-to-right pass: the substituted text is NOT re-scanned, so
        // the inner `${other}` is emitted verbatim. This pins the matrix
        // contract that a matrix value containing `${...}` is not re-expanded
        // into another variable (no recursion, no injection surprise).
        let ws = workspace_with_vars(&[("command", "run ${other}"), ("other", "INNER")], &[]);
        assert_eq!(ws.expand("${command}"), "run ${other}");
    }

    #[test]
    fn double_dollar_is_literal_dollar() {
        // $$ must collapse to a single literal $ and not start an expansion.
        let ws = workspace_with_vars(&[("x", "1")], &[]);
        assert_eq!(ws.expand("cost is $$5 not ${x}"), "cost is $5 not 1");
    }

    #[test]
    fn expand_leaves_unclosed_placeholder_verbatim() {
        // An unterminated ${ has no closing brace, so it is copied literally
        // rather than consuming the rest of the string or panicking.
        let ws = workspace_with_vars(&[("x", "1")], &[]);
        assert_eq!(ws.expand("value=${x and ${"), "value=${x and ${");
    }

    #[test]
    fn expand_empty_name_placeholder_is_unknown_variable() {
        // ${} names the empty variable, which is undefined, so it is re-emitted
        // verbatim like any other unknown variable.
        let ws = workspace_with_vars(&[("x", "1")], &[]);
        assert_eq!(ws.expand("a${}b"), "a${}b");
    }

    #[test]
    fn expand_handles_multibyte_text() {
        // Expansion must not corrupt surrounding multibyte UTF-8 characters.
        let ws = workspace_with_vars(&[("name", "世界")], &[]);
        assert_eq!(ws.expand("こんにちは ${name}"), "こんにちは 世界");
    }

    #[test]
    fn prepare_expands_variables_in_scenario_level_env() {
        // (#38) A `${var}` in a scenario-level `env` value must be substituted,
        // not delivered to the child as literal text. SCHEMA.md lists
        // scenario-level `env` values as an expansion site, and the spawn-level
        // half of the same merge already expands, so two identically-written
        // values must not resolve differently based on which map declared them.
        let yaml = r#"
name: envexp
variables:
  who: alice
env:
  SCENARIO_LEVEL: "hi-${who}"
steps: []
"#;
        let scenario = Scenario::from_yaml(yaml).unwrap();
        let ws = Workspace::prepare(&scenario, Path::new(".")).unwrap();
        assert_eq!(
            ws.env(),
            &[("SCENARIO_LEVEL".to_string(), "hi-alice".to_string())]
        );
    }

    #[test]
    fn prepare_leaves_unknown_variable_in_env_literal() {
        // An `env` value naming an undefined variable stays verbatim, exactly as
        // at every other expansion site: expansion never errors and never
        // silently blanks the placeholder, so the typo stays visible in the
        // child's environment.
        let yaml = r#"
name: envexp
env:
  V: "x-${missing}"
steps: []
"#;
        let scenario = Scenario::from_yaml(yaml).unwrap();
        let ws = Workspace::prepare(&scenario, Path::new(".")).unwrap();
        assert_eq!(ws.env(), &[("V".to_string(), "x-${missing}".to_string())]);
    }

    #[test]
    fn prepare_expands_env_from_parent_env_fallback() {
        // The parent-env fallback applies to scenario-level `env` too: a name
        // absent from `variables` but present in the parent process environment
        // resolves from there, matching `expand`'s documented resolution order.
        let key = "PITTY_TEST_SCENARIO_ENV_FALLBACK";
        std::env::set_var(key, "from-parent");
        let yaml = format!("name: envexp\nenv:\n  V: \"v=${{{key}}}\"\nsteps: []\n");
        let scenario = Scenario::from_yaml(&yaml).unwrap();
        let ws = Workspace::prepare(&scenario, Path::new(".")).unwrap();
        std::env::remove_var(key);
        assert_eq!(ws.env(), &[("V".to_string(), "v=from-parent".to_string())]);
    }

    #[test]
    fn prepare_does_not_expand_env_values_against_each_other() {
        // Scenario-level `env` entries are NOT a variable table: `${A}` in one
        // `env` value does not see a sibling `env` key named `A`. Expansion
        // reads `variables` and the parent env only, so the reference falls
        // through to "unknown" and stays literal. This pins the decision to keep
        // `env` non-self-referential rather than introducing ordering or cycle
        // semantics that no other expansion site has.
        let yaml = r#"
name: envexp
env:
  A: "base"
  B: "${A}/sub"
steps: []
"#;
        let scenario = Scenario::from_yaml(yaml).unwrap();
        let ws = Workspace::prepare(&scenario, Path::new(".")).unwrap();
        assert_eq!(
            ws.env(),
            &[
                ("A".to_string(), "base".to_string()),
                ("B".to_string(), "${A}/sub".to_string()),
            ]
        );
    }

    #[test]
    fn prepare_expands_env_in_a_single_non_recursive_pass() {
        // A variable whose value itself contains `${other}` is substituted once
        // and the result is not re-scanned, identically to `expand`. Scenario
        // `env` gets no extra expansion round that other sites lack.
        let yaml = r#"
name: envexp
variables:
  outer: "run ${inner}"
  inner: "INNER"
env:
  V: "${outer}"
steps: []
"#;
        let scenario = Scenario::from_yaml(yaml).unwrap();
        let ws = Workspace::prepare(&scenario, Path::new(".")).unwrap();
        assert_eq!(ws.env(), &[("V".to_string(), "run ${inner}".to_string())]);
    }

    #[test]
    fn prepare_expands_secret_variable_into_env_and_keeps_it_masked() {
        // A `secret: true` variable referenced from a scenario-level `env` value
        // must actually reach the child (the scenario does what it says), and
        // the substituted literal must still be registered for masking so it is
        // redacted in reports and logs.
        let yaml = r#"
name: envexp
variables:
  token:
    value: top-secret
    secret: true
env:
  TOKEN: "Bearer ${token}"
steps: []
"#;
        let scenario = Scenario::from_yaml(yaml).unwrap();
        let ws = Workspace::prepare(&scenario, Path::new(".")).unwrap();
        assert_eq!(
            ws.env(),
            &[("TOKEN".to_string(), "Bearer top-secret".to_string())]
        );
        assert_eq!(
            mask_secrets("TOKEN=Bearer top-secret", ws.secrets()),
            "TOKEN=Bearer ***"
        );
    }

    #[test]
    fn prepare_registers_secret_values() {
        // Secret-flagged variables must be registered for masking; plain ones
        // must not.
        let yaml = r#"
name: s
variables:
  plain: visible
  token:
    value: top-secret
    secret: true
steps: []
"#;
        let scenario = Scenario::from_yaml(yaml).unwrap();
        let ws = Workspace::prepare(&scenario, Path::new(".")).unwrap();
        assert_eq!(ws.secrets(), &["top-secret".to_string()]);
    }

    #[test]
    fn parent_env_values_are_never_registered_as_secrets() {
        // A value resolved through the parent-env fallback (not a scenario
        // `secret: true` variable) must NOT be registered for masking. This
        // pins the deliberate boundary that only scenario-declared secrets are
        // masked: parameterizing a `spawn.command` via the ambient env is a
        // non-secret path, and silently masking ambient values would both hide
        // legitimate output and imply a protection the framework does not give.
        // A scenario passing a real secret this way is using the wrong channel
        // (it must declare `secret: true`), and this test fixes that contract so
        // a future change cannot quietly start masking — or claim to mask —
        // parent-env values.
        let key = "PITTY_TEST_PARENT_ENV_SECRET";
        // Set on this process so `prepare`'s `std::env::vars()` snapshot sees it.
        std::env::set_var(key, "ambient-would-be-secret");
        let yaml = format!("name: env-fallback\nsteps:\n  - send: \"${{{key}}}\"\n");
        let scenario = Scenario::from_yaml(&yaml).unwrap();
        let ws = Workspace::prepare(&scenario, Path::new(".")).unwrap();
        std::env::remove_var(key);

        // The value expands from the parent env (proving it is reachable)...
        assert_eq!(ws.expand(&format!("${{{key}}}")), "ambient-would-be-secret");
        // ...yet it is absent from the masked set: parent-env values are never
        // secrets.
        assert!(ws.secrets().is_empty());
        assert!(!ws.secrets().iter().any(|s| s == "ambient-would-be-secret"));
    }

    #[test]
    fn mask_secrets_replaces_all_occurrences() {
        // Every occurrence of a secret must be replaced in diagnostic text.
        let masked = mask_secrets("token=abc123 retry token=abc123", &["abc123".to_string()]);
        assert_eq!(masked, "token=*** retry token=***");
    }

    #[test]
    fn mask_secrets_ignores_empty_secret() {
        // An empty secret must not turn into a degenerate replace-everything.
        let masked = mask_secrets("unchanged", &["".to_string()]);
        assert_eq!(masked, "unchanged");
    }

    #[test]
    fn mask_secrets_prefers_longest_overlapping_secret() {
        // When one secret is a prefix of another, the longer one must win so the
        // full value is masked rather than leaving its tail exposed.
        let masked = mask_secrets(
            "value=supersecret-key end",
            &["super".to_string(), "supersecret-key".to_string()],
        );
        assert_eq!(masked, "value=*** end");
    }

    #[test]
    fn mask_secrets_preserves_multibyte_context() {
        // Masking must not split surrounding multibyte UTF-8 when scanning.
        let masked = mask_secrets("鍵=tok値", &["tok".to_string()]);
        assert_eq!(masked, "鍵=***値");
    }

    /// Build a workspace rooted at a real on-disk `cwd` so write-path
    /// containment can be exercised against actual canonicalization.
    fn workspace_at(cwd: &Path) -> Workspace {
        Workspace {
            canonical_cwd: canonical_root(cwd),
            cwd: cwd.to_path_buf(),
            variables: BTreeMap::new(),
            parent_env: BTreeMap::new(),
            env: Vec::new(),
            secrets: Vec::new(),
            _temp: None,
            // Captured the same way `prepare` does, so containment tests
            // exercise the real descriptor-anchored path.
            #[cfg(unix)]
            #[cfg(unix)]
            cwd_identity: crate::safepath::open_dir_fd(cwd)
                .ok()
                .and_then(|fd| fd_identity(&fd)),
            #[cfg(unix)]
            cwd_fd: crate::safepath::open_dir_fd(cwd)
                .map(std::sync::Arc::new)
                .map_err(|e| e.to_string()),
        }
    }

    /// Resolve, requiring a usable target.
    ///
    /// `resolve_write_path` now returns a nested `Result`: the outer `Err` is a
    /// scenario error (exit 2), the inner one a path the filesystem would refuse
    /// (exit 1, reported as a failed assertion). Tests asserting on a *resolved*
    /// target flatten both here so the assertion reads as it did before.
    // Used only by the symlink tests, which are all `cfg(unix)`.
    #[cfg(unix)]
    fn resolve_ok(ws: &Workspace, rel: &str) -> SnapshotTarget {
        ws.resolve_write_path(rel, SnapshotAccess::Record)
            .unwrap_or_else(|e| panic!("expected '{rel}' to resolve, got scenario error: {e}"))
            .unwrap_or_else(|e| {
                panic!(
                    "expected '{rel}' to resolve, got an unresolvable path: {}",
                    e.into_message()
                )
            })
    }

    /// Resolve, requiring a usable target, with the caller's own message.
    fn resolve_ok_expect(ws: &Workspace, rel: &str, msg: &str) -> SnapshotTarget {
        match ws.resolve_write_path(rel, SnapshotAccess::Record) {
            Ok(Ok(target)) => target,
            Ok(Err(e)) => panic!("{msg}: unresolvable path: {}", e.into_message()),
            Err(e) => panic!("{msg}: scenario error: {e}"),
        }
    }

    /// A refusal from either class, carrying the exit code a consumer observes.
    ///
    /// Tests assert on `exit_code` because that — together with whether a report
    /// is produced — is the contract 1.2.2 established: a statically wrong path
    /// is 2 (no report), while a path the filesystem would refuse is 1, reported
    /// as a failed assertion.
    struct Refusal {
        exit_code: u8,
        message: String,
    }

    impl Refusal {
        fn exit_code(&self) -> u8 {
            self.exit_code
        }
        #[cfg(unix)]
        fn message(&self) -> &str {
            &self.message
        }
    }

    impl std::fmt::Display for Refusal {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(&self.message)
        }
    }

    /// Resolve, requiring a refusal of *either* class.
    fn resolve_refused(ws: &Workspace, rel: &str) -> Refusal {
        match ws.resolve_write_path(rel, SnapshotAccess::Record) {
            // A scenario error: the path is wrong whatever is on disk.
            Err(e) => Refusal {
                exit_code: e.exit_code(),
                message: e.message().to_string(),
            },
            // A path the filesystem would refuse: the runner reports this as a
            // failed snapshot assertion, which is exit 1 *with* a report.
            Ok(Err(unresolvable)) => Refusal {
                exit_code: 1,
                message: unresolvable.into_message(),
            },
            Ok(Ok(target)) => panic!(
                "expected '{rel}' to be refused, but it resolved to {}",
                target.display_path().display()
            ),
        }
    }

    #[test]
    fn resolve_write_path_allows_workspace_subdirectory() {
        // (C3) A normal snapshot path inside the workspace (including a
        // not-yet-created __snapshots__ subdir) must resolve to a path under the
        // workspace, so legitimate snapshot writes keep working.
        let dir = tempfile::tempdir().unwrap();
        let ws = workspace_at(dir.path());
        let resolved = resolve_ok_expect(
            &ws,
            "__snapshots__/x.snap",
            "in-workspace path must be allowed",
        );
        assert!(resolved.display_path().starts_with(dir.path()));
    }

    #[test]
    fn resolve_write_path_rejects_parent_traversal() {
        // (C3) A `..`-traversal write path must be rejected as a Scenario error
        // so a snapshot --update cannot write outside the workspace.
        let dir = tempfile::tempdir().unwrap();
        let ws = workspace_at(dir.path());
        let err = resolve_refused(&ws, "../../../tmp/escape.snap");
        assert_eq!(err.exit_code(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn resolve_write_path_rejects_symlink_escape() {
        // (C3) A symlinked subdirectory pointing outside the workspace must not
        // be a write escape hatch: resolving through the existing symlink lands
        // outside the root and is rejected.
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let link = dir.path().join("out");
        std::os::unix::fs::symlink(outside.path(), &link).unwrap();
        let ws = workspace_at(dir.path());
        let err = resolve_refused(&ws, "out/escape.snap");
        assert_eq!(err.exit_code(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn resolve_write_path_rejects_dangling_symlink_final_component() {
        // (#37) A snapshot path that *is* a dangling symlink pointing outside
        // the workspace must be rejected. `canonicalize` fails with ENOENT on a
        // dangling link, so a canonicalize-only check reads this as "file does
        // not exist yet" and lets the write through to the link target — the
        // exact bypass reported. Containment must not invert based on whether
        // the attacker pre-created the target, which is the one condition the
        // attacker controls.
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let victim = outside.path().join("PWNED.txt");
        let link = dir.path().join("x.snap");
        std::os::unix::fs::symlink(&victim, &link).unwrap();
        assert!(!victim.exists(), "target must be dangling for this case");

        let ws = workspace_at(dir.path());
        let err = resolve_refused(&ws, "x.snap");
        assert_eq!(err.exit_code(), 2);
        assert!(!victim.exists(), "nothing may be written outside");
    }

    #[cfg(unix)]
    #[test]
    fn resolve_write_path_rejects_dangling_symlink_directory_component() {
        // (#37) The directory form of the same bypass: `out/x.snap` where `out`
        // is a dangling symlink to an out-of-workspace directory. Previously
        // this passed containment and was stopped only by create_dir_all
        // returning EEXIST — an implementation accident, not a check. It must be
        // refused by containment itself.
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let missing_dir = outside.path().join("not-created-yet");
        std::os::unix::fs::symlink(&missing_dir, dir.path().join("out")).unwrap();

        let ws = workspace_at(dir.path());
        let err = resolve_refused(&ws, "out/x.snap");
        assert_eq!(err.exit_code(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn resolve_write_path_allows_a_symlink_that_resolves_inside_the_workspace() {
        // A symlink whose target is inside the workspace must keep working. An
        // earlier round refused every symlink outright, which was a v1
        // compatibility break: `snapshots -> real` is an ordinary repository
        // layout (a shared fixture directory is a normal reason to have one) and
        // such a scenario passed in 1.2.2.
        //
        // Allowing it does not reopen the check-then-use race that motivated the
        // blanket refusal, because the recorder never traverses the link's name:
        // resolution canonicalizes through it, and the components handed to the
        // target are the canonical ones. See
        // `a_resolvable_link_is_recorded_by_its_canonical_components` below for
        // the property that makes this safe.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("real")).unwrap();
        std::os::unix::fs::symlink("real", root.join("snapshots")).unwrap();

        let ws = workspace_at(&root);
        resolve_ok_expect(
            &ws,
            "snapshots/out.snap",
            "a link resolving inside the workspace must be allowed",
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_resolvable_link_is_recorded_by_its_canonical_components() {
        // The property that makes allowing in-workspace links safe, and the
        // reason the TOCTOU the blanket refusal guarded against does not return:
        // the recorder is handed the *canonical* sequence, so the link's own
        // name is never opened. A child that re-points `snapshots` after
        // resolution changes nothing, because nothing looks at `snapshots`
        // again. (And if it swaps the canonical component instead, the
        // descriptor walk refuses it — every component is opened `O_NOFOLLOW`.)
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("real")).unwrap();
        std::os::unix::fs::symlink("real", root.join("snapshots")).unwrap();

        let ws = workspace_at(&root);
        let resolved = resolve_ok(&ws, "snapshots/out.snap");

        let names: Vec<_> = resolved
            .components()
            .iter()
            .map(|c| c.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            vec!["real".to_string(), "out.snap".to_string()],
            "the recorder must walk the link's target, never the link's name"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_dangling_link_pointing_inside_the_workspace_is_allowed() {
        // (v1 regression) `canonicalize` answers ENOENT identically for a link
        // whose target is simply not recorded yet and one that escapes, so
        // refusing every unresolvable link also refused
        // `out.snap -> real/out.snap` — an ordinary scenario that recorded fine
        // on 1.2.2, and whose exit code changed from 0 to 2. The destination is
        // now resolved by hand and judged by the same containment rule as
        // everything else.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("real")).unwrap();
        // Target does NOT exist yet — that is the whole point.
        std::os::unix::fs::symlink("real/out.snap", root.join("out.snap")).unwrap();

        let ws = workspace_at(&root);
        resolve_ok_expect(
            &ws,
            "out.snap",
            "a dangling link pointing inside the workspace must be allowed",
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_dangling_link_with_an_absolute_inside_target_is_allowed() {
        // An absolute target is used as written rather than joined against the
        // link's directory; containment then judges it the same way.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("real")).unwrap();
        std::os::unix::fs::symlink(root.join("real/out.snap"), root.join("out.snap")).unwrap();

        let ws = workspace_at(&root);
        resolve_ok_expect(
            &ws,
            "out.snap",
            "an absolute dangling target inside the workspace must be allowed",
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_dangling_chain_of_links_is_judged_on_its_final_destination() {
        // A link to a link to a not-yet-created file: the chain is followed to
        // the end, so the decision is made on where it actually lands rather
        // than on the first hop.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("real")).unwrap();
        std::os::unix::fs::symlink("real/out.snap", root.join("mid.snap")).unwrap();
        std::os::unix::fs::symlink("mid.snap", root.join("chain.snap")).unwrap();

        let ws = workspace_at(&root);
        resolve_ok_expect(
            &ws,
            "chain.snap",
            "a dangling chain landing inside the workspace must be allowed",
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_dangling_link_pointing_outside_the_workspace_is_refused() {
        // The half that must not regress. A relative target is resolved against
        // the link's own directory, so `../outside/x` genuinely leaves.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let workspace_dir = root.join("w");
        std::fs::create_dir(&workspace_dir).unwrap();
        std::fs::create_dir(root.join("outside")).unwrap();
        std::os::unix::fs::symlink("../outside/PWNED", workspace_dir.join("out.snap")).unwrap();

        let ws = workspace_at(&workspace_dir);
        let err = resolve_refused(&ws, "out.snap");
        // Exit 2 (a scenario error, no report), and deliberately *not* aligned
        // with 1.2.2 here: 1.2.2 exited 0 and created `outside/PWNED`, which is
        // the containment escape this project fixed. Verified by running it. The
        // break was accepted in an earlier round; the path is wrong whatever the
        // filesystem holds, so the author must fix the YAML.
        assert_eq!(err.exit_code(), 2);
        assert!(
            !root.join("outside/PWNED").exists(),
            "nothing may be created at the escape target"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_dangling_chain_that_leaves_the_workspace_is_refused() {
        // Following the chain must not become a way to launder an escape: the
        // last hop is what counts, and it is outside.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let workspace_dir = root.join("w");
        std::fs::create_dir(&workspace_dir).unwrap();
        std::fs::create_dir(root.join("outside")).unwrap();
        std::os::unix::fs::symlink("../outside/PWNED", workspace_dir.join("mid.snap")).unwrap();
        std::os::unix::fs::symlink("mid.snap", workspace_dir.join("chain.snap")).unwrap();

        let ws = workspace_at(&workspace_dir);
        let err = resolve_refused(&ws, "chain.snap");
        assert_eq!(err.exit_code(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn an_intermediate_dangling_link_keeps_the_components_after_it() {
        // The components following a dangling link were dropped, so `link ->
        // missing-dir` with `file: link/out.snap` resolved to `missing-dir`
        // alone: the snapshot became a *file* named `missing-dir` and the
        // `out.snap` the scenario asked for never existed — a later verifying
        // run would compare against the wrong thing. The path checked and the
        // path written must be the same one.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::os::unix::fs::symlink("missing-dir", root.join("link")).unwrap();

        let ws = workspace_at(&root);
        let resolved = resolve_ok_expect(
            &ws,
            "link/out.snap",
            "an inward dangling link with a tail must be allowed",
        );

        let names: Vec<_> = resolved
            .components()
            .iter()
            .map(|c| c.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            vec!["missing-dir".to_string(), "out.snap".to_string()],
            "the tail after the link must be preserved, not dropped"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_tail_that_escapes_only_after_the_link_resolves_is_refused() {
        // Containment must be judged on destination *plus* tail, and this is the
        // shape where that is load-bearing. The escape is invisible to the
        // primary containment check, which normalizes the path *as written*:
        // `w/deep/link/../../out.snap` reduces to `w/out.snap`, safely inside.
        // Only after `link -> ../../outside` is resolved does the tail's `../..`
        // climb out of the workspace for real.
        //
        // (A tail whose `..` escapes lexically — `link/../../outside/x` — is
        // caught earlier by that primary check, so it does not exercise this
        // path at all.)
        //
        // Honest about what this does and does not catch: it passes against the
        // pre-fix code too, because dropping the tail leaves the destination
        // `../../outside/gone`, which is already outside and refused anyway. It
        // pins the guarantee rather than detecting the tail-dropping bug — that
        // job belongs to `an_intermediate_dangling_link_keeps_the_components_after_it`
        // and `a_tail_that_returns_inside_from_an_outward_destination_is_allowed`,
        // both of which do fail without the fix. Kept because the guarantee is
        // worth pinning: no future change may let a post-resolution escape
        // through.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let workspace_dir = root.join("w");
        std::fs::create_dir_all(workspace_dir.join("deep")).unwrap();
        std::fs::create_dir(root.join("outside")).unwrap();
        // Dangling, and pointing out of the workspace.
        std::os::unix::fs::symlink("../../outside/gone", workspace_dir.join("deep/link")).unwrap();

        let ws = workspace_at(&workspace_dir);
        let err = resolve_refused(&ws, "deep/link/../../out.snap");
        // Exit 1, not 2, and with a report: 1.2.2 discovered this by attempting
        // the write and surfacing the OS error as a failed snapshot assertion
        // (verified by running it). pitty refuses earlier so nothing is
        // truncated, but a consumer parsing stdout must still get its report.
        assert_eq!(err.exit_code(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn a_tail_that_returns_inside_from_an_outward_destination_is_allowed() {
        // The mirror case, and the reason the two halves cannot be judged
        // separately: the link's destination is outside the workspace, so
        // judging it alone would refuse this — but the tail comes back in, and
        // the write lands inside after all. Normalizing the combination is what
        // gets both directions right.
        //
        // `outside` must actually exist: `..` may only cancel a component the
        // kernel could traverse, and this test originally omitted it — asserting
        // a resolution the OS itself refuses with ENOENT. That is now caught by
        // `climbs_through_missing_component` and covered by
        // `a_tail_climbing_over_a_missing_component_cannot_reach_a_real_file`.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let workspace_dir = root.join("w");
        std::fs::create_dir(&workspace_dir).unwrap();
        std::fs::create_dir(root.join("outside")).unwrap();
        std::os::unix::fs::symlink("../outside", workspace_dir.join("link")).unwrap();

        let ws = workspace_at(&workspace_dir);
        let resolved = resolve_ok_expect(
            &ws,
            "link/../w/out.snap",
            "a tail returning inside the workspace must be allowed",
        );
        assert!(resolved.safe_path().starts_with(&workspace_dir));
    }

    #[cfg(unix)]
    #[test]
    fn a_tail_climbing_over_a_missing_component_cannot_reach_a_real_file() {
        // (Data loss) Lexical `..` elimination is only sound across components
        // that exist. `link -> missing` with `file: link/../victim.snap`
        // normalizes lexically to `victim.snap` — a real, unrelated file — but
        // the kernel resolves left to right and fails at `missing` with ENOENT,
        // so the scenario could never legitimately reach it. Acting on the
        // lexical answer had `--update` truncate and rewrite that file: silent
        // data loss, inside the workspace, where 1.2.2 left it untouched.
        //
        // Not a containment question — the victim is in the workspace — which is
        // why containment alone did not catch it.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::write(root.join("victim.snap"), "ORIGINAL").unwrap();
        std::os::unix::fs::symlink("missing", root.join("link")).unwrap();

        let ws = workspace_at(&root);
        let err = resolve_refused(&ws, "link/../victim.snap");
        // Exit 1, not 2, and with a report: 1.2.2 discovered this by attempting
        // the write and surfacing the OS error as a failed snapshot assertion
        // (verified by running it). pitty refuses earlier so nothing is
        // truncated, but a consumer parsing stdout must still get its report.
        assert_eq!(err.exit_code(), 1);
        assert_eq!(
            std::fs::read_to_string(root.join("victim.snap")).unwrap(),
            "ORIGINAL",
            "an unreachable path must never reach a real file"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_cancelled_component_is_attempted_rather_than_assumed_creatable() {
        // (Data loss) The last prediction in this resolver: a `..` was allowed to
        // cancel a missing component "because `--update` will create it". That
        // premise was false twice over.
        //
        // First, nothing created it. `missing/../victim.snap` normalizes to
        // `victim.snap`, so the component list the recorder walks never mentions
        // `missing/` — while 1.2.2 called `create_dir_all` on the RAW path and
        // therefore did create it (verified by running 1.2.2: `missing/` appears
        // in its tree).
        //
        // Second, and worse: when the workspace is not writable the creation
        // 1.2.2 attempted FAILS with EACCES, so 1.2.2 exits 1 and leaves the
        // existing snapshot alone. Predicting success instead let `--update`
        // overwrite a real recorded file on a read-only tree — exit 0 where v1
        // exited 1.
        //
        // The fix carries the cancelled directory to the recorder, which
        // attempts it; this test pins that the attempt is recorded as work to do
        // rather than assumed away.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::write(root.join("victim.snap"), "stale").unwrap();
        let ws = workspace_at(&root);

        let target = resolve_ok_expect(
            &ws,
            "missing/../victim.snap",
            "recording through a cancelled component stays allowed",
        );
        // The write still lands on the normalized location...
        assert!(target.safe_path().ends_with("victim.snap"));
        // ...but the component the path named is carried for the recorder to
        // attempt, so whether it can exist is the kernel's call and not a
        // prediction made here.
        let names: Vec<Vec<String>> = target
            .traversed_dirs()
            .iter()
            .map(|seq| seq.iter().map(|c| c.to_string_lossy().into()).collect())
            .collect();
        assert_eq!(
            names,
            vec![vec!["missing".to_string()]],
            "the cancelled component must be handed to the recorder to attempt"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_cancelled_component_is_collected_for_a_relative_workspace_too() {
        // (Data loss) The cancelled-directory list was built on the workspace's
        // `cwd` field, which is RELATIVE whenever the scenario was named by a
        // relative path (`pitty run s.yaml` with `workspace.cwd: w` leaves it as
        // `w`) — while `relative_components` strips against the canonical
        // absolute root. `strip_prefix` then failed, the entry was silently
        // dropped, and the recorder had nothing to attempt: the read-only
        // workspace refusal vanished and `--update` overwrote a real snapshot.
        //
        // It reproduced ONLY for relative invocations, which is exactly why it
        // survived a full end-to-end battery: every harness there passed an
        // absolute scenario path, which made `cwd` absolute and the prefix strip
        // correctly. This test pins both forms so the invocation shape cannot
        // silently change the answer again.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::write(root.join("victim.snap"), "stale").unwrap();

        // A relative workspace path, as a relative `pitty run` produces.
        let previous = std::env::current_dir().unwrap();
        std::env::set_current_dir(&root).unwrap();
        let relative = Workspace {
            canonical_cwd: canonical_root(Path::new(".")),
            cwd: PathBuf::from("."),
            variables: BTreeMap::new(),
            parent_env: BTreeMap::new(),
            env: Vec::new(),
            secrets: Vec::new(),
            _temp: None,
            cwd_identity: crate::safepath::open_dir_fd(Path::new("."))
                .ok()
                .and_then(|fd| fd_identity(&fd)),
            cwd_fd: crate::safepath::open_dir_fd(Path::new("."))
                .map(std::sync::Arc::new)
                .map_err(|e| e.to_string()),
        };
        let target = relative
            .resolve_write_path("missing/../victim.snap", SnapshotAccess::Record)
            .expect("not a scenario error")
            .expect("recording through a cancelled component stays allowed");
        std::env::set_current_dir(previous).unwrap();

        let names: Vec<Vec<String>> = target
            .traversed_dirs()
            .iter()
            .map(|seq| seq.iter().map(|c| c.to_string_lossy().into()).collect())
            .collect();
        assert_eq!(
            names,
            vec![vec!["missing".to_string()]],
            "a relatively-addressed workspace must still hand the cancelled \
             component to the recorder; dropping it removes the only thing that \
             makes a read-only workspace refuse"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_cancelled_component_that_is_a_symlinked_directory_still_records() {
        // (v1 regression) `dl -> d` with `file: dl/../out.snap`. 1.2.2 records
        // and verifies it: `create_dir_all` FOLLOWS symlinks, so
        // `create_dir_all("dl/..")` returns Ok (measured directly).
        //
        // Carrying the cancelled component to the recorder broke this, because
        // `walk_from` opens every component `O_NOFOLLOW` — correct for the
        // snapshot's own path, wrong for a component that only needs to be
        // *traversable*. The walk answered ENOTDIR on the symlink, `--update`
        // recorded nothing, and the failure message told the user to rerun with
        // the flag they had just used.
        //
        // The fix skips a component the kernel can already traverse: there is
        // nothing to create, and no byte is written through this component, so
        // `O_NOFOLLOW` has nothing to protect here. Refusal is still the answer
        // for a regular file (ENOTDIR) or an unsearchable directory (EACCES) —
        // both already refused earlier by the traversability guard, matching the
        // same measurements of v1's `create_dir_all`.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("d")).unwrap();
        std::os::unix::fs::symlink("d", root.join("dl")).unwrap();

        let ws = workspace_at(&root);
        let target = resolve_ok_expect(
            &ws,
            "dl/../out.snap",
            "a `..` across a symlinked directory is ordinary path arithmetic",
        );

        let result = crate::assert::snapshot::check("recorded", &target, false, true);
        assert!(
            result.passed,
            "recording across a symlinked directory must succeed as it does on \
             1.2.2: {:?}",
            result.message
        );
        assert_eq!(
            std::fs::read_to_string(root.join("out.snap")).unwrap(),
            "recorded"
        );
    }

    #[cfg(unix)]
    #[test]
    fn cancelled_components_survive_a_symlinked_workspace_and_an_absolute_file() {
        // (Data loss, third representation) A symlinked workspace plus an
        // ABSOLUTE `file:`. The absolute path re-rooted through `RootDir`, the
        // lexical `strip_prefix` against the canonical root failed, and the
        // cancelled entry was silently dropped — so `--update` overwrote a real
        // snapshot on a read-only tree where 1.2.2 refused.
        //
        // Same defect as the relative-`cwd` and symlinked-component cases,
        // reached through a third spelling of the same path. The fix is
        // structural: one derivation, anchored in the root's representation.
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let real = base.join("real");
        std::fs::create_dir(&real).unwrap();
        std::fs::write(real.join("victim.snap"), "stale").unwrap();
        let link = base.join("w");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let ws = workspace_at(&link);
        // The `file:` value names the workspace through the LINK, absolutely.
        let abs = format!("{}/missing/../victim.snap", link.display());
        let target = resolve_ok_expect(&ws, &abs, "an absolute in-workspace path resolves");

        let names: Vec<Vec<String>> = target
            .traversed_dirs()
            .iter()
            .map(|seq| seq.iter().map(|c| c.to_string_lossy().into()).collect())
            .collect();
        assert_eq!(
            names,
            vec![vec!["missing".to_string()]],
            "the cancelled component must survive a symlinked workspace named by \
             an absolute path; dropping it removes the read-only refusal entirely"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_missing_component_beneath_a_symlinked_workspace_still_records() {
        // (MAJOR) The writable-tree half of the same shape: a missing cancelled
        // component under a symlinked workspace must record, creating the
        // component, exactly as 1.2.2 does.
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let real = base.join("real");
        std::fs::create_dir(&real).unwrap();
        let link = base.join("w");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let ws = workspace_at(&link);
        let target = resolve_ok_expect(&ws, "missing/../out.snap", "writable tree records");
        let result = crate::assert::snapshot::check("recorded", &target, false, true);

        assert!(result.passed, "must record: {:?}", result.message);
        assert_eq!(
            std::fs::read_to_string(real.join("out.snap")).unwrap(),
            "recorded"
        );
        assert!(
            real.join("missing").is_dir(),
            "1.2.2 creates the cancelled component here; so must this"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_cancelled_component_is_never_silently_dropped_in_any_representation() {
        // The guarantee that outlives the three specific cases. A silent `None`
        // discarding the entry is what hid all three data-loss BLOCKERs
        // (relative `cwd`, symlinked component, symlinked workspace plus an
        // absolute `file:`), and macOS's `/var` -> `/private/var` is an ordinary
        // alias of exactly that kind — reachable in normal use, not only under
        // attack.
        //
        // So rather than pin one contrived mismatch, this asserts the property
        // across every representation of the SAME location: each must yield the
        // same cancelled component. Any future derivation that silently drops one
        // fails here regardless of which spelling exposes it.
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let real = base.join("real");
        std::fs::create_dir(&real).unwrap();
        let link = base.join("w");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let expected = vec![vec!["missing".to_string()]];
        for (label, ws_path, file_value) in [
            (
                "plain root",
                real.clone(),
                "missing/../out.snap".to_string(),
            ),
            (
                "symlinked workspace",
                link.clone(),
                "missing/../out.snap".to_string(),
            ),
            (
                "symlinked workspace, absolute file",
                link.clone(),
                format!("{}/missing/../out.snap", link.display()),
            ),
            (
                "symlinked workspace, dotted file",
                link.clone(),
                "./missing/.././missing/../out.snap".to_string(),
            ),
        ] {
            let ws = workspace_at(&ws_path);
            let target = resolve_ok_expect(&ws, &file_value, label);
            let names: Vec<Vec<String>> = target
                .traversed_dirs()
                .iter()
                .map(|seq| seq.iter().map(|c| c.to_string_lossy().into()).collect())
                .collect();
            assert_eq!(
                names, expected,
                "representation '{label}' must yield the same cancelled component; \
                 dropping it silently is what caused three data-loss bugs"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_workspace_recreated_under_the_same_name_is_refused() {
        // (Escape) Pinning the canonical root as a *string* does not pin an
        // identity. A workspace deleted and recreated under the same name
        // canonicalizes to a byte-identical path while being a different inode,
        // so the pinned name and the held descriptor can still denote different
        // directories — and the component list derived from the name would be
        // applied to the wrong object.
        //
        // Only the descriptor tracks identity, so the pin is now validated
        // against it by `(dev, ino)`.
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let workspace = base.join("w");
        std::fs::create_dir(&workspace).unwrap();

        // Pre-spawn capture, as `prepare` does.
        let ws = workspace_at(&workspace);

        // The child's swap: same name, different directory object.
        std::fs::remove_dir_all(&workspace).unwrap();
        std::fs::create_dir(&workspace).unwrap();
        std::fs::write(workspace.join("planted.snap"), "PLANTED").unwrap();

        let err = ws
            .resolve_write_path("out.snap", SnapshotAccess::Record)
            .expect_err("a recreated workspace must be refused, not resolved");
        assert_eq!(
            err.exit_code(),
            2,
            "the YAML is fine; the workspace moved under the run, so this is a \
             scenario error rather than a reported assertion failure"
        );
        assert!(
            !workspace.join("out.snap").exists(),
            "nothing may be written into the substituted directory"
        );
    }

    #[cfg(unix)]
    #[test]
    fn cancellations_after_re_entering_the_workspace_are_still_recorded() {
        // (Data loss) A path may LEAVE the workspace and come back:
        // `../outside/new/../../w/inside/../victim.snap`. The `w/inside/..` at
        // the end is a cancellation squarely inside the root, and `w/inside` does
        // not exist — so on a read-only workspace 1.2.2 fails with EACCES trying
        // to create it and leaves the snapshot alone.
        //
        // The previous walk decided "am I inside the root" ONCE, from where the
        // anchored path began, and gated recording on that sample. This path
        // begins outside, so everything cancelled after the re-entry was
        // discarded: nothing was attempted, the read-only refusal vanished, and
        // `--update` destroyed a real snapshot.
        //
        // Position is now tracked per component rather than sampled, so the
        // answer holds however many times the path exits and re-enters.
        //
        // Preconditions are ASSERTED, not assumed: with `outside/new` present the
        // path resolves differently and the bug hides — the same masking that has
        // produced false "clean" runs twice in this area.
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let workspace = base.join("w");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::create_dir(base.join("outside")).unwrap();
        std::fs::write(workspace.join("victim.snap"), "ORIGINAL").unwrap();

        assert!(
            !base.join("outside/new").exists(),
            "precondition: outside/new must be ABSENT or the path resolves \
             differently and the defect hides"
        );
        assert!(
            !workspace.join("inside").exists(),
            "precondition: w/inside must be ABSENT — it is the component whose \
             creation must be attempted"
        );

        let ws = workspace_at(&workspace);
        let target = resolve_ok_expect(
            &ws,
            "../outside/new/../../w/inside/../victim.snap",
            "a path that leaves and returns still resolves",
        );

        let names: Vec<Vec<String>> = target
            .traversed_dirs()
            .iter()
            .map(|seq| seq.iter().map(|c| c.to_string_lossy().into()).collect())
            .collect();
        assert!(
            names.contains(&vec!["inside".to_string()]),
            "a cancellation occurring after the walk re-enters the workspace must \
             be recorded; got {names:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_climb_after_a_symlink_uses_the_kernels_position_not_the_lexical_one() {
        // (False green) `link -> real/subdir`, then `link/../missing/../victim.snap`.
        //
        // The kernel follows `link` into `real/subdir`, so the first `..` lands
        // in `real` and it then looks for `real/missing` — which does not exist,
        // so the whole path is ENOENT and 1.2.2 reports "not recorded" and fails.
        //
        // A lexical `stack.pop()` lands in the workspace root instead, where
        // `missing` DOES exist and is traversable, so the guard approved the
        // climb and the assertion was satisfied by comparing the output against
        // `real/victim.snap` — a file the scenario could never have opened. Exit
        // 0 where v1 exits 1.
        //
        // The asymmetry is the bug, and both halves are asserted below so a
        // stale fixture cannot mask it: the same name must EXIST at the lexical
        // position and be ABSENT at the symlink's destination. A plain symlinked
        // component or a plain missing component reaches neither.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("real/subdir")).unwrap();
        std::fs::create_dir(root.join("missing")).unwrap();
        std::os::unix::fs::symlink("real/subdir", root.join("link")).unwrap();
        std::fs::write(root.join("real/victim.snap"), "RECORDED").unwrap();

        assert!(
            root.join("missing").is_dir(),
            "precondition: `missing` must EXIST at the lexical position"
        );
        assert!(
            !root.join("real/missing").exists(),
            "precondition: `missing` must be ABSENT at the symlink's destination \
             — without this asymmetry the defect does not appear"
        );
        // The kernel's own verdict, pinned so the test states what it reproduces.
        assert!(
            std::fs::metadata(root.join("link/../missing/../victim.snap")).is_err(),
            "precondition: the kernel must refuse this path"
        );

        let ws = workspace_at(&root);
        // Verifying: the path is unreachable, so it must be refused outright and
        // never satisfied by reading a different file.
        match ws.resolve_write_path("link/../missing/../victim.snap", SnapshotAccess::Verify) {
            Ok(Err(_)) => {}
            Ok(Ok(target)) => panic!(
                "a path the kernel refuses must not resolve, but it reached {}",
                target.safe_path().display()
            ),
            Err(e) => panic!("expected a reported assertion failure, not a scenario error: {e}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn recording_through_a_cancelled_component_fails_on_a_read_only_workspace() {
        // (Data loss) The end-to-end half of
        // `a_cancelled_component_is_attempted_rather_than_assumed_creatable`,
        // driving the real `check()` write path rather than just the resolver.
        //
        // `file: missing/../victim.snap` normalizes to `victim.snap`, and that
        // write alone would succeed: the file already exists and only the
        // *create* permission is missing. 1.2.2 still failed, because it created
        // the parents of the RAW path and `mkdir w/missing` needs write
        // permission on `w`; it exited 1 and left the recorded snapshot alone
        // (verified by running 1.2.2).
        //
        // Predicting that `--update` "will create" the component let the
        // overwrite proceed on a tree where v1 refused. Attempting the creation
        // restores both halves: writable trees record, this one fails with the
        // kernel's own EACCES.
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::write(root.join("victim.snap"), "stale").unwrap();

        let ws = workspace_at(&root);
        let target = resolve_ok_expect(
            &ws,
            "missing/../victim.snap",
            "recording through a cancelled component stays allowed",
        );

        // 0555: nothing new may be created in the workspace.
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o555)).unwrap();
        let result = crate::assert::snapshot::check("fresh", &target, false, true);
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();

        assert!(
            !result.passed,
            "a component the kernel refuses to create must fail the assertion, \
             not be assumed away"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("victim.snap")).unwrap(),
            "stale",
            "the existing snapshot must survive a recording the filesystem refused"
        );
    }

    #[cfg(unix)]
    #[test]
    fn recording_through_a_cancelled_component_still_works_on_a_writable_tree() {
        // The other half: the fix must not over-correct. On a writable tree the
        // creation succeeds, the snapshot records, and — matching 1.2.2, which
        // created it via `create_dir_all` on the raw path — the cancelled
        // component actually appears on disk.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::write(root.join("victim.snap"), "stale").unwrap();

        let ws = workspace_at(&root);
        let target = resolve_ok_expect(&ws, "missing/../victim.snap", "writable tree records");
        let result = crate::assert::snapshot::check("fresh", &target, false, true);

        assert!(result.passed, "a writable tree must still record");
        assert_eq!(
            std::fs::read_to_string(root.join("victim.snap")).unwrap(),
            "fresh"
        );
        assert!(
            root.join("missing").is_dir(),
            "1.2.2 created the cancelled component; attempting it must too"
        );
    }

    #[cfg(unix)]
    #[test]
    fn resolution_after_a_workspace_swap_uses_the_pre_spawn_directory() {
        // (Escape) The runner resolves the snapshot path *after* the scenario's
        // child has run, so everything `resolve_write_path` derives from the
        // workspace NAME is computed from something the child may already have
        // repointed — while the descriptor the write uses was captured before
        // the child existed. Deriving the component list from one directory and
        // applying it to the descriptor of another is the check/use split this
        // module exists to prevent, one level above the per-component `openat`
        // handles that already close it within a walk.
        //
        // The existing `a_renamed_and_symlinked_workspace_cannot_redirect_the_write`
        // does NOT cover this: it builds the target *before* the swap, which is
        // not the runner's ordering. This one resolves afterwards, which is.
        //
        // The workspace is reached through a SYMLINK, which is what makes the two
        // roots actually differ. A plain `mv w w.old && mkdir w` does not: a
        // canonical path is a string, so `w.canonicalize()` yields `w` again once
        // a fresh `w` exists, and the stale and pinned roots are textually
        // identical (verified by running the syscalls). Repointing a symlink
        // makes `canonical_root` return a genuinely different directory, so this
        // shape distinguishes the pinned root from a re-resolved one.
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let real = base.join("real");
        let decoy = base.join("decoy");
        std::fs::create_dir(&real).unwrap();
        std::fs::create_dir(&decoy).unwrap();
        let workspace = base.join("w");
        std::os::unix::fs::symlink(&real, &workspace).unwrap();

        // Pre-spawn capture: descriptor AND canonical root, as `prepare` does.
        let ws = workspace_at(&workspace);

        // The child's swap, before the assertion resolves — the runner's order.
        std::fs::remove_file(&workspace).unwrap();
        std::os::unix::fs::symlink(&decoy, &workspace).unwrap();

        // Two acceptable verdicts, and the unacceptable one is neither of them.
        //
        // Refusing outright is fine and is what happens today: `candidate` is
        // joined onto the workspace NAME, which now resolves to the decoy, so it
        // no longer lies under the pinned pre-spawn root and containment rejects
        // it as an escape. That is fail-closed — nothing is written anywhere.
        //
        // Resolving is also fine, provided it resolves to the PRE-SPAWN
        // directory. What must never happen is the write landing in the
        // directory the child substituted, which is exactly what 1.2.2 did here
        // (verified by running it: 1.2.2 exits 0 and the snapshot appears in the
        // decoy). This is a deliberate, safer divergence from v1.
        match ws.resolve_write_path("out.snap", SnapshotAccess::Record) {
            // Fail-closed: the swapped workspace is refused as an escape.
            Err(_) => {}
            Ok(Err(_)) => {}
            Ok(Ok(target)) => {
                assert!(
                    target.safe_path().starts_with(&real),
                    "if it resolves at all it must be anchored on the pre-spawn \
                     directory, got {}",
                    target.safe_path().display()
                );
                let result = crate::assert::snapshot::check("recorded", &target, false, true);
                assert!(result.passed, "write must succeed: {:?}", result.message);
                assert_eq!(
                    std::fs::read_to_string(real.join("out.snap")).unwrap(),
                    "recorded"
                );
            }
        }

        // The invariant that holds under every verdict.
        assert!(
            !decoy.join("out.snap").exists(),
            "nothing may be written into the directory the child substituted"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_missing_component_climb_cannot_satisfy_a_verifying_assertion() {
        // (False green — the worst direction for a testing tool.) On a **read**
        // nothing creates `missing/`, so `missing/../victim.snap` is a path the
        // kernel refuses with ENOENT and always will. 1.2.2 attempted the raw
        // read, got ENOENT, and reported "not recorded" — a failed assertion,
        // exit 1.
        //
        // The resolver exempted a missing component because `--update` creates
        // the snapshot's parent directories, but it was not told which mode it
        // was in, so the exemption applied on reads too: the `..` elided
        // `missing/`, the path normalized to `victim.snap`, and the assertion
        // was satisfied by comparing the output against a DIFFERENT, already
        // recorded file. Exit 0 where v1 exited 1.
        //
        // `SnapshotAccess` now makes the caller state the question, and this
        // pins both halves of it.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::write(root.join("victim.snap"), "RECORDED").unwrap();
        let ws = workspace_at(&root);

        // Verifying: the path is unreachable and must be refused outright, so no
        // comparison against `victim.snap` can ever happen.
        match ws.resolve_write_path("missing/../victim.snap", SnapshotAccess::Verify) {
            Ok(Err(_)) => {}
            Ok(Ok(target)) => panic!(
                "an unreachable path must never satisfy an assertion, but it resolved to {}",
                target.safe_path().display()
            ),
            Err(e) => panic!("expected a reported assertion failure, not a scenario error: {e}"),
        }

        // Recording: still allowed, because `--update` really does create the
        // missing parent. Refusing this would be the v1 break the exemption
        // exists to avoid (1.2.2 records `../new-dir/../w/out.snap`).
        resolve_ok_expect(
            &ws,
            "missing/../victim.snap",
            "recording may still cross a component it is about to create",
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_climb_through_an_unsearchable_directory_cannot_reach_a_real_file() {
        use std::os::unix::fs::PermissionsExt;
        // (Data loss) `locked` IS a directory, so every attribute test this
        // guard ever used says `locked/..` is fine — but it has no search
        // permission, and the kernel needs search permission to resolve `..`
        // through it. `stat locked/../victim.snap` fails with EACCES; 1.2.2
        // surfaced that as a failed assertion and left `victim.snap` alone,
        // while the attribute-based guard reported PASSED and overwrote it.
        //
        // This is the third shape to break the same guard (missing component,
        // then regular file, now an unsearchable directory), which is why the
        // fix asks the kernel via `open(O_SEARCH)` instead of adding a fourth
        // attribute — permissions are not the last one either (noexec mounts,
        // dead NFS, SELinux).
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::write(root.join("victim.snap"), "ORIGINAL").unwrap();
        std::fs::create_dir(root.join("locked")).unwrap();
        std::fs::set_permissions(root.join("locked"), std::fs::Permissions::from_mode(0o000))
            .unwrap();

        let ws = workspace_at(&root);
        let err = resolve_refused(&ws, "locked/../victim.snap");

        // Restore before the assertions so a failure still cleans up.
        std::fs::set_permissions(root.join("locked"), std::fs::Permissions::from_mode(0o755))
            .unwrap();

        // Exit 1 with a report, matching 1.2.2: it discovered this by attempting
        // the write and surfacing the EACCES as a failed snapshot assertion.
        assert_eq!(err.exit_code(), 1);
        assert_eq!(
            std::fs::read_to_string(root.join("victim.snap")).unwrap(),
            "ORIGINAL",
            "a path the kernel refuses to resolve must never reach a real file"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_climb_through_a_searchable_but_unreadable_directory_is_still_allowed() {
        use std::os::unix::fs::PermissionsExt;
        // The mirror, and the reason the probe uses `O_SEARCH` rather than
        // `O_DIRECTORY`. A `0100` directory cannot be *listed* but can be
        // *traversed*, and the kernel resolves `searchonly/..` happily — so
        // refusing it would be a false refusal. `O_DIRECTORY` implies `O_RDONLY`
        // and fails here with EACCES (measured); `O_SEARCH` succeeds, matching
        // the kernel.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("searchonly")).unwrap();
        std::fs::set_permissions(
            root.join("searchonly"),
            std::fs::Permissions::from_mode(0o100),
        )
        .unwrap();

        let ws = workspace_at(&root);
        let resolved = ws.resolve_write_path("searchonly/../out.snap", SnapshotAccess::Record);

        std::fs::set_permissions(
            root.join("searchonly"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();

        // On platforms without `O_SEARCH` the probe falls back to `O_RDONLY |
        // O_DIRECTORY`, which is stricter than the kernel here and refuses. That
        // fails closed (a report, never a write to the wrong file), so both
        // verdicts are acceptable; what must never happen is a scenario error.
        match resolved {
            Ok(Ok(target)) => assert!(target.safe_path().starts_with(&root)),
            // A refusal here is `Ok(Err(_))`, which the runner reports as a
            // failed snapshot assertion (exit 1, with a report) by construction.
            Ok(Err(_)) => {}
            Err(e) => panic!("a traversable directory must not be a scenario error: {e}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_chain_that_would_exhaust_the_visited_set_is_refused_not_truncated() {
        // The memory bound must fail closed. A walk stopped at the cap has not
        // reached a destination, so returning the path it happened to be
        // standing on would approve a location never actually resolved — the
        // same class of mistake as trusting a lexical `..`.
        //
        // This shape also shows why the cap is a real backstop and not dead
        // code: each hop appends another `../d/` segment, so the path *grows*
        // instead of repeating, and the visited set fills rather than hitting a
        // duplicate. Verified by execution: with both the duplicate check and
        // this cap removed, the walk on this layout runs indefinitely.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("d")).unwrap();
        // b -> a and a -> b, but each through a `..` that lengthens the path.
        std::os::unix::fs::symlink("../d/b.snap", root.join("d/a.snap")).unwrap();
        std::os::unix::fs::symlink("../d/a.snap", root.join("d/b.snap")).unwrap();

        // Whichever guard trips first, the answer must be "no destination".
        assert_eq!(
            dangling_link_destination(&root.join("d/a.snap")),
            None,
            "a walk that cannot finish must refuse, never hand back a partial location"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_cycle_the_kernel_cannot_see_is_caught_by_the_visited_set() {
        // The visited-path set's own reason to exist, and the one case no kernel
        // call can answer.
        //
        // A plain `a -> b -> a` never reaches this walk: all three names
        // resolve, so `metadata` reports `ELOOP` and the earlier
        // `kernel_refused_path` gate refuses it (verified by instrumenting the
        // walk — the set was never entered for that shape).
        //
        // This shape does reach it. The links sit in a directory and point at
        // each other through `..`, so every individual `read_link` succeeds and
        // returns a *different* path string each hop; only the walk's own
        // repetition reveals that it is back where it started. Verified by
        // execution: with the set removed, an instrumented copy of this walk
        // exceeded 200 iterations on this exact layout (a hang, not a failure),
        // and with the set present it trips on the repeat.
        //
        // This is why a set and not a counter: a cycle IS a repeated path, so a
        // set decides it exactly, while a count only ever approximated it — and
        // the approximation truncated legitimate chains (see
        // `no_bound_of_pittys_own_caps_a_dangling_chain_the_kernel_accepts`).
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("d")).unwrap();
        std::os::unix::fs::symlink("../d/b.snap", root.join("d/a.snap")).unwrap();
        std::os::unix::fs::symlink("../d/a.snap", root.join("d/b.snap")).unwrap();

        assert_eq!(
            dangling_link_destination(&root.join("d/a.snap")),
            None,
            "a cycle only this walk can see must be refused, not spun on"
        );

        // And end to end: a refusal, reported as a failed assertion (exit 1 with
        // a report), never a hang and never a scenario error.
        let ws = workspace_at(&root);
        let err = resolve_refused(&ws, "d/a.snap");
        assert_eq!(err.exit_code(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn a_tail_climbing_over_an_existing_component_is_still_allowed() {
        // The guard must not over-correct: `..` across a directory that *does*
        // exist is ordinary path arithmetic the kernel performs happily, so a
        // link to a real directory with a climbing tail keeps working.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("realdir")).unwrap();
        std::os::unix::fs::symlink("realdir", root.join("link")).unwrap();

        let ws = workspace_at(&root);
        let resolved = resolve_ok_expect(
            &ws,
            "link/../out.snap",
            "`..` across an existing directory must still resolve",
        );
        assert_eq!(resolved.safe_path(), root.join("out.snap"));
    }

    #[cfg(unix)]
    #[test]
    fn a_tail_climbing_over_a_non_directory_cannot_reach_a_real_file() {
        // (Data loss, BLOCKER) The `..` guard originally asked only whether the
        // crossed component *existed*, via `symlink_metadata().is_err()`. A
        // regular file exists, so `blocker/../victim.snap` sailed through — but
        // the kernel refuses it with ENOTDIR at `blocker` exactly as it refuses
        // a missing component with ENOENT. Same class, same consequence: the
        // lexical answer is a real neighbouring file, and `--update` truncated
        // it. 1.2.2 reported "Not a directory (os error 20)" and left the file
        // alone; the regressed build exited 0 having destroyed it.
        //
        // The rule the kernel actually applies is that `..` is only meaningful
        // across a *resolvable directory*, which is what the guard now tests.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::write(root.join("blocker"), "a regular file, not a directory").unwrap();
        std::fs::write(root.join("victim.snap"), "ORIGINAL").unwrap();
        std::os::unix::fs::symlink("blocker/../victim.snap", root.join("out.snap")).unwrap();

        let ws = workspace_at(&root);
        let err = resolve_refused(&ws, "out.snap");
        // Exit 1, not 2, and with a report: 1.2.2 discovered this by attempting
        // the write and surfacing the OS error as a failed snapshot assertion
        // (verified by running it). pitty refuses earlier so nothing is
        // truncated, but a consumer parsing stdout must still get its report.
        assert_eq!(err.exit_code(), 1);
        assert_eq!(
            std::fs::read_to_string(root.join("victim.snap")).unwrap(),
            "ORIGINAL",
            "a path the kernel answers ENOTDIR for must never reach a real file"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_direct_file_value_climbing_over_a_non_directory_is_refused() {
        // The companion to the test above, for the path that had no guard at
        // all. The check lived only in the dangling-link branch, so the very
        // same shape written straight into the scenario's `file:` value —
        // needing no symlink whatsoever — bypassed it and destroyed the victim.
        // A guard that only covers one of two routes to the same write is not a
        // guard, which is why this now runs on the candidate itself.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::write(root.join("blocker"), "a regular file, not a directory").unwrap();
        std::fs::write(root.join("victim.snap"), "ORIGINAL").unwrap();

        let ws = workspace_at(&root);
        let err = resolve_refused(&ws, "blocker/../victim.snap");
        // Exit 1, not 2, and with a report: 1.2.2 discovered this by attempting
        // the write and surfacing the OS error as a failed snapshot assertion
        // (verified by running it). pitty refuses earlier so nothing is
        // truncated, but a consumer parsing stdout must still get its report.
        assert_eq!(err.exit_code(), 1);
        assert!(
            err.message().contains("cannot write snapshot"),
            "the message must read as a write failure, as 1.2.2's did: {}",
            err.message()
        );
        assert_eq!(
            std::fs::read_to_string(root.join("victim.snap")).unwrap(),
            "ORIGINAL",
            "the guard must cover the direct `file:` route, not only symlinks"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_chain_the_kernel_refuses_with_eloop_is_not_resolved_by_hand() {
        // (Data loss, BLOCKER) The hop bound counted `read_link` calls, but each
        // `read_link(&current)` makes the kernel traverse `current`'s own
        // directory components internally, and those hops were invisible to the
        // counter.
        //
        // `out.snap -> d0/final` over a 31-link directory chain, with
        // `final -> ../victim.snap`, costs the kernel 33 hops — it answers ELOOP,
        // and 1.2.2 reported "Too many levels of symbolic links". This code
        // counted 2, resolved it, and `--update` wrote through to the victim.
        //
        // No `read_link` counter can reproduce that arithmetic, because the two
        // are not counting the same events. So resolvability is now the kernel's
        // answer about the whole path rather than a constant maintained here —
        // correct at whatever limit the running platform has.
        //
        // The chain length is likewise *not* a constant here. A fixed 31 links
        // encoded macOS's limit (`MAXSYMLINKS` 32) as if it were universal: the
        // same shape resolves fine on Linux, whose limit is 40, so the premise
        // assertion below fired there and the test was wrong rather than merely
        // failing. The length is therefore grown until the running kernel
        // actually refuses, which is the only portable way to name "a chain this
        // kernel will not resolve" — and it keeps the property under test intact
        // on every platform, because what is asserted is still that pitty
        // refuses whatever the kernel refused.
        //
        // Each length gets a fresh workspace so that chain length is the only
        // variable, and the search is capped so a kernel with no limit at all
        // skips rather than builds links forever.
        const MAX_CHAIN: usize = 128;

        // `out.snap -> d0/final` over an `n`-link directory chain, with
        // `final -> ../victim.snap`. Returns the workspace root, or `None` when
        // this kernel resolves the shape at that length.
        fn build_unresolvable(dir: &std::path::Path, n: usize) -> Option<std::path::PathBuf> {
            let root = dir.join(format!("n{n}"));
            std::fs::create_dir(&root).unwrap();
            let root = root.canonicalize().unwrap();
            std::fs::write(root.join("victim.snap"), "ORIGINAL").unwrap();
            std::fs::create_dir(root.join("real")).unwrap();

            std::os::unix::fs::symlink("real", root.join(format!("d{n}"))).unwrap();
            for i in (0..n).rev() {
                std::os::unix::fs::symlink(format!("d{}", i + 1), root.join(format!("d{i}")))
                    .unwrap();
            }
            std::os::unix::fs::symlink("../victim.snap", root.join("real/final")).unwrap();
            std::os::unix::fs::symlink("d0/final", root.join("out.snap")).unwrap();

            // The kernel's own answer about the whole path — the same question
            // the resolver defers to, asked here to locate this platform's edge.
            //
            // Specifically ELOOP, not merely "some error": every component of
            // this shape exists (the chain ends at a real `victim.snap`), so the
            // only way it can fail is the symlink limit. Matching on the kind
            // keeps a future edit from picking up an ENOENT boundary — a
            // *dangling* chain — which pitty resolves by hand on purpose and
            // which would therefore turn this into a test that cannot fail.
            match std::fs::metadata(root.join("out.snap")) {
                Err(e) if e.raw_os_error() == Some(libc::ELOOP) => Some(root),
                _ => None,
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let Some(root) = (8..=MAX_CHAIN).find_map(|n| build_unresolvable(dir.path(), n)) else {
            // Not an assertion: a kernel that resolves a 128-link chain has no
            // limit this test can reach, so there is no "chain the kernel
            // refuses" to hand the resolver. Skipping says that honestly instead
            // of asserting a premise the platform does not hold.
            eprintln!(
                "skipping: this kernel resolves symlink chains up to {MAX_CHAIN} links, \
                 so no kernel-refused chain could be constructed"
            );
            return;
        };

        let ws = workspace_at(&root);
        let err = resolve_refused(&ws, "out.snap");
        // Exit 1, not 2, and with a report: 1.2.2 discovered this by attempting
        // the write and surfacing the OS error as a failed snapshot assertion
        // (verified by running it). pitty refuses earlier so nothing is
        // truncated, but a consumer parsing stdout must still get its report.
        assert_eq!(err.exit_code(), 1);
        assert_eq!(
            std::fs::read_to_string(root.join("victim.snap")).unwrap(),
            "ORIGINAL",
            "a chain the kernel answers ELOOP for must never reach a real file"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_directory_symlink_chain_the_kernel_accepts_still_resolves() {
        // The other half of the bound's job, and the v1-compatibility guard: a
        // directory chain the kernel *does* resolve must keep recording. Pinning
        // both sides is what stops a future "just refuse deep chains" from
        // silently breaking scenarios that 1.2.2 recorded.
        //
        // Deliberately below every platform limit (macOS 32, Linux 40) so the
        // assertion holds wherever it runs — the point is that the kernel
        // decides, not that a particular number is baked in here.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("real")).unwrap();

        let n = 10;
        std::os::unix::fs::symlink("real", root.join(format!("d{n}"))).unwrap();
        for i in (0..n).rev() {
            std::os::unix::fs::symlink(format!("d{}", i + 1), root.join(format!("d{i}"))).unwrap();
        }

        let ws = workspace_at(&root);
        resolve_ok_expect(
            &ws,
            "d0/out.snap",
            "a directory chain the kernel resolves must keep recording",
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_dangling_chain_longer_than_sixteen_hops_still_resolves() {
        // The bound was set to 16 as "far above any legitimate layout", which
        // was simply wrong: the platform resolves far more (macOS MAXSYMLINKS is
        // 32, verified by execution), so a 17-hop chain that recorded fine on
        // 1.2.2 became a scenario error. A bound below the platform's refuses
        // chains the OS would have followed.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("real")).unwrap();

        let n = 17;
        std::os::unix::fs::symlink("real/out.snap", root.join(format!("hop_{}", n - 1))).unwrap();
        for i in (0..n - 1).rev() {
            std::os::unix::fs::symlink(format!("hop_{}", i + 1), root.join(format!("hop_{i}")))
                .unwrap();
        }

        let ws = workspace_at(&root);
        resolve_ok(&ws, "hop_0");
    }

    #[cfg(unix)]
    #[test]
    fn no_bound_of_pittys_own_caps_a_dangling_chain_the_kernel_accepts() {
        // (v1 compatibility) Only the kernel's symlink limit may cap chain
        // depth. The `MAX_DANGLING_LINK_HOPS = 32` / `0..=32` that used to be
        // here capped the *walk* instead, and a walk step is not a kernel hop.
        //
        // The boundary for THIS shape (`hop_0 -> ... -> hop_{n-1} -> real/out.snap`),
        // re-measured on macOS 26 / arm64 by `stat`-ing generated chains:
        //
        // | links | walk iterations | macOS kernel | Linux kernel (MAXSYMLINKS 40) |
        // |-------|-----------------|--------------|-------------------------------|
        // | 31    | 32              | ENOENT (ok)  | ok                            |
        // | 32    | 33              | ELOOP        | ok                            |
        // | 33    | 34              | ELOOP        | **ok**                        |
        //
        // An earlier revision of this table recorded 32 links as ENOENT on
        // macOS. That is wrong — 32 is already ELOOP here — and the correction
        // matters for reading the test, though not for its outcome: this chain
        // is *dangling*, and a dangling chain never reaches `canonicalize`, so
        // the by-hand walk resolves it whether or not the kernel would. That is
        // deliberate (it is the only way `out.snap -> real/out.snap` can record
        // a not-yet-existing target), and it is why this test still passes at a
        // length the kernel itself refuses.
        //
        // `0..=32` supplies exactly 33 iterations, so 33 links (34 iterations)
        // was the first chain the old cap truncated — a Linux-only v1 break,
        // where such a chain is one 1.2.2 recorded and the cap turned into exit
        // 1. No Linux host was available to this work, so that break is **not
        // verified by execution** here; what is verified is the shape below and
        // the macOS column of the table.
        //
        // The test therefore asserts the property that holds on every platform:
        // no constant of pitty's may cut the walk short. The length is kept at
        // 32 because it exercises the walk deeper than any legitimate layout
        // would; the visited-path set that replaced the count has no depth
        // ceiling at all, which is what removes the Linux break as well.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();

        // hop_0 -> hop_1 -> ... -> hop_31 -> real/out.snap (not created yet).
        std::fs::create_dir(root.join("real")).unwrap();
        let n = 32usize;
        std::os::unix::fs::symlink("real/out.snap", root.join(format!("hop_{}", n - 1))).unwrap();
        for i in (0..n - 1).rev() {
            std::os::unix::fs::symlink(format!("hop_{}", i + 1), root.join(format!("hop_{i}")))
                .unwrap();
        }

        let ws = workspace_at(&root);
        let resolved = resolve_ok_expect(
            &ws,
            "hop_0",
            "a dangling chain the kernel resolves must not be truncated by a bound of pitty's own",
        );
        // It must land on the chain's real destination, not on the link's own
        // name: the recorder opens every component `O_NOFOLLOW`, so handing it
        // `hop_0` would fail at the write after passing resolution.
        assert!(resolved.safe_path().ends_with("real/out.snap"));
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_loop_is_refused_rather_than_spun_on() {
        // `canonicalize` reports ELOOP for a cycle it can see, but a dangling
        // chain never reaches `canonicalize` — this resolution walks it by hand,
        // so it needs its own bound or `a -> b -> a` would spin forever. A cycle
        // has no destination to judge, so it is refused. The test also serves as
        // a liveness check: a regression here hangs rather than fails, which is
        // why the hop limit exists at all.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::os::unix::fs::symlink("loopb.snap", root.join("loopa.snap")).unwrap();
        std::os::unix::fs::symlink("loopa.snap", root.join("loopb.snap")).unwrap();

        let ws = workspace_at(&root);
        let err = resolve_refused(&ws, "loopa.snap");
        // Exit 1, not 2, and with a report: 1.2.2 discovered this by attempting
        // the write and surfacing the OS error as a failed snapshot assertion
        // (verified by running it). pitty refuses earlier so nothing is
        // truncated, but a consumer parsing stdout must still get its report.
        assert_eq!(err.exit_code(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn resolve_write_path_still_rejects_a_symlink_that_leaves_the_workspace() {
        // The narrowing must not weaken containment: a link resolving *outside*
        // is still refused, now by the containment check (which canonicalizes
        // through it) rather than by a blanket symlink ban.
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("snapshots")).unwrap();

        let ws = workspace_at(dir.path());
        let err = resolve_refused(&ws, "snapshots/out.snap");
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn resolve_write_path_allows_plain_not_yet_created_file() {
        // (#37) The symlink refusal must not catch the ordinary first-record
        // case: a plain path whose file (and parent dir) genuinely do not exist
        // yet stays allowed, so `--update` can still record a new snapshot.
        let dir = tempfile::tempdir().unwrap();
        let ws = workspace_at(dir.path());
        let resolved = resolve_ok_expect(
            &ws,
            "__snapshots__/new/out.snap",
            "a genuinely absent path must remain allowed",
        );
        assert!(resolved.display_path().starts_with(dir.path()));
    }

    #[test]
    fn the_safe_path_never_steps_outside_the_workspace() {
        // (Windows `..` escape) Containment is judged on the normalized
        // destination, but `display` keeps the `file:` value as written. A
        // platform without `openat` has to build a path, and building it from
        // `display` re-introduced the escape the component sequence exists to
        // close: `../outside/new/../../w/out.snap` normalizes back inside the
        // workspace, so containment rightly passes, while `create_dir_all` on
        // the raw form created `outside/new` *outside* the workspace on the way.
        //
        // This runs on every platform, not just Windows, because it asserts a
        // property of the target rather than a syscall: `safe_path` is confined
        // to the root and contains no parent-dir hop, whoever consumes it.
        use std::path::Component;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let workspace_dir = root.join("w");
        std::fs::create_dir(&workspace_dir).unwrap();
        let ws = workspace_at(&workspace_dir);

        let resolved = resolve_ok_expect(
            &ws,
            "../outside/new/../../w/out.snap",
            "a path that normalizes back inside must still be allowed",
        );

        let safe = resolved.safe_path();
        assert!(
            safe.starts_with(&workspace_dir),
            "safe_path must stay under the workspace: {}",
            safe.display()
        );
        assert!(
            !safe.components().any(|c| c == Component::ParentDir),
            "safe_path must contain no `..`: {}",
            safe.display()
        );
        assert_eq!(safe, workspace_dir.join("out.snap"));

        // The display path is deliberately NOT the sanitized form — that is what
        // makes it unsafe to open, and why the distinction has to exist at all.
        //
        // Asserting that specifically by looking for a surviving `..` was a
        // macOS/Linux-only premise. `display` is `cwd.join(rel)`, and on Windows
        // `cwd` comes from `canonicalize()`, which yields a *verbatim* (`\\?\`)
        // path; `PathBuf::push` documents that "verbatim paths need . and ..
        // removed" and strips them during the join. So on Windows the raw `..`
        // is gone before `display` is ever stored, and the old assertion tested
        // a spelling the platform does not produce rather than the property.
        //
        // What must hold everywhere is the distinction itself: `display_path` is
        // whatever the join produced and is NOT trusted, while `safe_path` is
        // rebuilt from the validated component sequence. Pinning that they are
        // *different paths* keeps the defect this test exists to catch — a
        // `safe_path` derived from `display` — detectable on every platform,
        // because such a derivation would make the two equal.
        assert_ne!(
            resolved.display_path(),
            safe.as_path(),
            "safe_path must be rebuilt from the validated components, not taken \
             from the raw display path"
        );

        // And on the platforms whose join preserves it, the raw `..` really does
        // survive into `display` — the stronger, spelling-level statement, kept
        // where it is true rather than asserted everywhere.
        if !cfg!(windows) {
            assert!(
                resolved
                    .display_path()
                    .components()
                    .any(|c| c == Component::ParentDir),
                "display_path is expected to keep the value as written"
            );
        }
    }

    #[test]
    fn resolved_components_never_step_outside_the_workspace() {
        // (Escape 1) Containment is judged on the *normalized* destination, but
        // the recorder has to walk an actual component sequence. When those two
        // were derived separately, `../sibling/../w/out.snap` passed
        // containment — it really does normalize back to `<w>/out.snap` — while
        // a recorder walking the *raw* components stepped out to the parent on
        // the way and created (and chmod-ed 0700) a directory outside the
        // workspace, even though the snapshot itself landed inside.
        //
        // The fix makes the resolver hand back the sequence it validated. This
        // test pins the invariant that closes the escape: no component the
        // recorder receives is ever `..`, so walking it cannot leave the root
        // regardless of how the `file:` value was spelled.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let workspace_dir = root.join("w");
        std::fs::create_dir(&workspace_dir).unwrap();
        let ws = workspace_at(&workspace_dir);

        let resolved = resolve_ok_expect(
            &ws,
            "../new-private-dir/../w/out.snap",
            "a path that normalizes back inside must still be allowed",
        );

        // It resolves to the in-workspace file, as containment concluded...
        assert!(resolved.display_path().starts_with(&workspace_dir));
        // ...and the sequence handed to the recorder is purely in-workspace.
        #[cfg(unix)]
        {
            let names: Vec<_> = resolved
                .components()
                .iter()
                .map(|c| c.to_string_lossy().into_owned())
                .collect();
            assert_eq!(
                names,
                vec!["out.snap".to_string()],
                "the recorder must receive the normalized in-workspace sequence"
            );
            assert!(
                !names.iter().any(|n| n == ".."),
                "no component may be a parent-dir hop"
            );
        }
    }

    #[test]
    fn an_absolute_in_workspace_path_still_gets_the_protected_traversal() {
        // (Escape 2, related) When `file:` is an absolute path inside the
        // workspace, the writer used to fall back to opening the file's parent
        // directly, losing the per-component `openat` protection entirely. It
        // must instead go through the same relative traversal as any other
        // path: `cwd.join(abs)` yields the absolute path, and stripping the
        // root leaves an ordinary in-workspace sequence.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let ws = workspace_at(&root);

        let absolute = root.join("__snapshots__/out.snap");
        let resolved = resolve_ok_expect(
            &ws,
            absolute.to_str().unwrap(),
            "an absolute path inside the workspace must be allowed",
        );

        assert!(resolved.display_path().starts_with(&root));

        #[cfg(unix)]
        {
            let names: Vec<_> = resolved
                .components()
                .iter()
                .map(|c| c.to_string_lossy().into_owned())
                .collect();
            assert_eq!(
                names,
                vec!["__snapshots__".to_string(), "out.snap".to_string()],
                "an absolute in-workspace path must reduce to relative components"
            );
        }
    }

    #[test]
    fn an_absolute_out_of_workspace_path_is_refused() {
        // (Escape 2, related) The other half: an absolute path outside the
        // workspace must be refused by containment, not silently written via a
        // parent-directory fallback.
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let ws = workspace_at(dir.path());
        let err = ws
            .resolve_write_path(
                outside.path().join("escape.snap").to_str().unwrap(),
                SnapshotAccess::Record,
            )
            .expect_err("an absolute out-of-workspace path must be refused");
        assert_eq!(err.exit_code(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn a_search_only_workspace_prepares_for_a_snapshot_free_run() {
        // A `0300` workspace is legitimate: on Unix, entering a directory needs
        // execute/search permission, not read, so a child can `chdir` into this
        // one and run normally. An earlier fail-closed version demanded the
        // descriptor unconditionally at prepare time and killed such a run
        // before its first step — even with no `expect_snapshot` anywhere.
        //
        // Preparation must therefore succeed. Containment is only *required* by
        // a snapshot, so its unavailability may only fail a run that wants one
        // (see the companion test below).
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let workspace_dir = dir.path().join("w");
        std::fs::create_dir(&workspace_dir).unwrap();
        // Search + write, no read.
        std::fs::set_permissions(&workspace_dir, std::fs::Permissions::from_mode(0o300)).unwrap();

        let yaml = "name: s\nworkspace:\n  cwd: w\nsteps: []\n";
        let scenario = Scenario::from_yaml(yaml).unwrap();
        let prepared = Workspace::prepare(&scenario, dir.path());

        // Restore before asserting so tempdir cleanup works even on failure.
        std::fs::set_permissions(&workspace_dir, std::fs::Permissions::from_mode(0o755)).unwrap();

        prepared.expect("a search-only workspace must not fail a snapshot-free run");
    }

    #[cfg(unix)]
    #[test]
    fn a_snapshot_on_an_uncapturable_workspace_fails_closed() {
        // The other half of the contract: deferring the failure must not lose
        // it. When the descriptor genuinely cannot be obtained, a run that asks
        // for a snapshot path must be refused — never handed a target it would
        // write through an unprotected, symlink-following path.
        //
        // Note this cannot be provoked by mode alone on every platform: where
        // `O_SEARCH` exists (macOS) a `0300` workspace *is* capturable, and
        // snapshots there simply work — a strictly better outcome. So this test
        // drives the failure path directly, by constructing the workspace with
        // a capture error, which is what any uncapturable directory produces.
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace {
            canonical_cwd: canonical_root(dir.path()),
            cwd: dir.path().to_path_buf(),
            variables: BTreeMap::new(),
            parent_env: BTreeMap::new(),
            env: Vec::new(),
            secrets: Vec::new(),
            _temp: None,
            cwd_identity: None,
            cwd_fd: Err("cannot open workspace directory for snapshot containment: \
                         Permission denied (os error 13)"
                .to_string()),
        };

        let err = resolve_refused(&ws, "out.snap");
        // A process fault (exit 3), not a scenario error: the YAML is valid,
        // the environment cannot support the guarantee.
        assert_eq!(err.exit_code(), 3);
        assert!(
            err.to_string().contains("snapshot containment"),
            "the error must say why the directory had to be opened: {err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_search_only_workspace_still_records_snapshots_where_o_search_exists() {
        // Where the platform can open a directory for search only, a `0300`
        // workspace is fully usable *including* snapshots — the guarantee gets
        // stronger, not weaker, because capture stays unconditional. On
        // platforms without `O_SEARCH` this case falls back to the fail-closed
        // path proven above, so the assertion is conditional on what capture
        // actually returned rather than on a hardcoded platform list.
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let workspace_dir = dir.path().join("w");
        std::fs::create_dir(&workspace_dir).unwrap();
        std::fs::set_permissions(&workspace_dir, std::fs::Permissions::from_mode(0o300)).unwrap();

        let captured = crate::safepath::open_dir_fd(&workspace_dir).is_ok();

        let yaml = "name: s\nworkspace:\n  cwd: w\nsteps: []\n";
        let scenario = Scenario::from_yaml(yaml).unwrap();
        let ws = Workspace::prepare(&scenario, dir.path()).unwrap();
        let resolved = ws.resolve_write_path("out.snap", SnapshotAccess::Record);

        std::fs::set_permissions(&workspace_dir, std::fs::Permissions::from_mode(0o755)).unwrap();

        if captured {
            resolved
                .expect("a capturable search-only workspace must allow snapshots")
                .unwrap_or_else(|e| {
                    panic!(
                        "a capturable search-only workspace must resolve: {}",
                        e.into_message()
                    )
                });
        } else {
            // Fail-closed is a *process* error (exit 3): the descriptor could not
            // be captured, so the harness cannot run this step safely at all.
            // Distinct from both refusal classes above.
            let err = resolved.expect_err("without capture, a snapshot must fail closed");
            assert_eq!(err.exit_code(), 3);
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_normal_workspace_still_prepares() {
        // (Defect 1, guard against over-correction) Making the capture
        // mandatory must not break ordinary runs: a plain readable workspace
        // directory still prepares, and so does a temp workspace.
        let dir = tempfile::tempdir().unwrap();
        let scenario = Scenario::from_yaml("name: s\nsteps: []\n").unwrap();
        Workspace::prepare(&scenario, dir.path()).expect("a normal workspace must prepare");

        let temp_scenario =
            Scenario::from_yaml("name: s\nworkspace:\n  temp: true\nsteps: []\n").unwrap();
        Workspace::prepare(&temp_scenario, dir.path()).expect("a temp workspace must prepare");
    }

    #[test]
    fn resolve_write_path_refuses_the_workspace_directory_itself() {
        // (Escape 1, boundary) A `file:` that normalizes to the workspace root
        // names no file to write. Left unhandled it would yield an empty
        // component sequence, and a recorder popping a file name off an empty
        // list has no sensible behavior. Refuse it as a scenario error instead.
        let dir = tempfile::tempdir().unwrap();
        let ws = workspace_at(dir.path());
        let err = resolve_refused(&ws, "sub/..");
        assert_eq!(err.exit_code(), 2);
    }

    #[cfg(windows)]
    #[test]
    fn lexical_normalize_preserves_windows_drive_prefix() {
        // Windows drive prefixes must survive lexical normalization; otherwise
        // snapshot-write containment would compare a malformed candidate/root
        // pair and reject a legitimate in-workspace path.
        let normalized = lexical_normalize(Path::new(r"C:\work\pitty\..\snapshots\out.snap"));
        assert_eq!(normalized, PathBuf::from(r"C:\work\snapshots\out.snap"));
    }

    #[cfg(windows)]
    #[test]
    fn lexical_normalize_preserves_windows_unc_prefix() {
        // UNC prefixes carry the server/share root. Dropping them would make a
        // network workspace look like a local absolute path during containment.
        let normalized =
            lexical_normalize(Path::new(r"\\server\share\pitty\..\snapshots\out.snap"));
        assert_eq!(
            normalized,
            PathBuf::from(r"\\server\share\snapshots\out.snap")
        );
    }
}
