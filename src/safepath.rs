//! Symlink-resistant filesystem traversal, anchored on a directory descriptor.
//!
//! Every file pitty writes into a directory the scenario's own child process
//! can reach — snapshots and logs alike — faces the same hazard: the path is
//! validated at one moment and used at another, and in between the child can
//! replace a component with a symlink pointing anywhere. A `symlink_metadata`
//! check before the write cannot close that gap, because the check and the use
//! are separate syscalls naming the path by *name*.
//!
//! This module resolves paths the only way that does close it: descend from a
//! descriptor for a directory that existed before the child was spawned, one
//! component at a time, with `openat(O_DIRECTORY | O_NOFOLLOW)`. Each handle
//! names a directory *object*, so a rename or symlink swap after a component is
//! opened cannot change which directory the next step operates on, and a
//! component that *is* a symlink when the walk reaches it fails with `ELOOP`
//! rather than being followed.
//!
//! Unix only. Windows has no `openat` equivalent, so callers there fall back to
//! path-based writes and document that the guarantee does not hold.

#![cfg(unix)]

use std::ffi::OsStr;

/// An owned file descriptor that closes on drop.
///
/// Why a local type rather than `std::os::fd::OwnedFd`: the directory handles
/// here come straight from `openat` as raw `c_int`s, and wrapping them in a
/// tiny RAII guard keeps every early `?` return in a walk from leaking a
/// descriptor without threading `close` calls through each error path.
#[derive(Debug)]
pub struct Fd(libc::c_int);

impl Fd {
    /// The raw descriptor, for passing to an `*at` syscall.
    pub fn as_raw(&self) -> libc::c_int {
        self.0
    }

    /// Relinquish ownership, returning the raw descriptor to the caller.
    ///
    /// Used to hand a descriptor to `File::from_raw_fd`, which takes over
    /// closing it.
    pub fn into_raw(self) -> libc::c_int {
        let raw = self.0;
        std::mem::forget(self);
        raw
    }
}

impl Drop for Fd {
    fn drop(&mut self) {
        // SAFETY: `self.0` is owned by this value and not yet closed.
        unsafe { libc::close(self.0) };
    }
}

/// Whether [`walk_from`] may create missing directories as it descends.
#[derive(PartialEq, Clone, Copy)]
pub enum CreateDirs {
    /// Create each missing directory with mode `0700`.
    ///
    /// Only directories actually created are chmod-ed; one that already exists
    /// keeps the mode it has, because it belongs to the user rather than to
    /// pitty. `mkdirat` is what makes that distinction available at all —
    /// `create_dir_all` succeeds silently either way.
    Yes,
    /// Never create anything; a missing directory is reported as an error.
    No,
}

/// Reject any component that could undo containment.
///
/// Callers are expected to pass already-validated, normalized names, but
/// `openat` would happily traverse a literal `..` straight out of the anchor
/// directory, so the components that could escape are refused here too rather
/// than trusted to stay impossible upstream.
fn is_unsafe_component(name: &OsStr) -> bool {
    let bytes = name.as_encoded_bytes();
    bytes == b".." || bytes == b"." || bytes.is_empty() || bytes.contains(&b'/')
}

/// Descend `components` from `root_fd`, returning the final directory's
/// descriptor and the last component (the file name).
///
/// `components` must be a relative, normalized sequence: each entry a plain
/// name, no `..`, no separators. The last entry is treated as the file name and
/// is *not* opened — the caller opens it with whatever mode and flags it needs,
/// relative to the returned descriptor.
///
/// `root_fd` must be a descriptor for a directory obtained before the untrusted
/// process could interfere with it; the anti-swap property comes entirely from
/// that. Passing a descriptor opened from an attacker-influenced path gives
/// nothing.
///
/// Errors with `InvalidInput` if `components` is empty or contains an unsafe
/// name, and otherwise propagates the failing syscall's error — notably
/// `ELOOP` when a component is a symlink, and `ENOENT` under
/// [`CreateDirs::No`] when a directory on the way does not exist.
pub fn walk_from(
    root_fd: libc::c_int,
    components: &[std::ffi::OsString],
    create: CreateDirs,
) -> std::io::Result<(Fd, &OsStr)> {
    let Some((file_name, dirs)) = components.split_last() else {
        return Err(std::io::Error::from(std::io::ErrorKind::InvalidInput));
    };
    let file_name = file_name.as_os_str();

    if is_unsafe_component(file_name) || dirs.iter().any(|d| is_unsafe_component(d)) {
        return Err(std::io::Error::from(std::io::ErrorKind::InvalidInput));
    }

    let mut dir = dup_fd(root_fd)?;

    for name in dirs {
        let name = name.as_os_str();
        // `mkdirat` distinguishes "we created this" (Ok) from "it was already
        // there" (EEXIST), which `create_dir_all` cannot. Only the former may
        // have its mode tightened; an existing directory's mode is the user's.
        let created = if create == CreateDirs::Yes {
            match mkdirat(dir.as_raw(), name, creation_mode()) {
                Ok(()) => true,
                Err(e) if e.raw_os_error() == Some(libc::EEXIST) => false,
                Err(e) => return Err(e),
            }
        } else {
            false
        };
        // The exact window the scenario's child races for: after `mkdirat`
        // succeeded, before the mode is set. A test substitutes the directory
        // here to prove the mode is applied to a descriptor and not to the name;
        // in production this is a no-op that compiles away.
        after_mkdirat_hook(dir.as_raw(), name, created);
        // Open first, then fix the mode on the descriptor — never on the name.
        //
        // A name-based chmod here was a real hole, even immediately after a
        // successful `mkdirat`: the scenario's child can win the race to `rmdir`
        // the new directory and put a **hard link to an external file** at that
        // same name, and a name-relative chmod would then set that external
        // inode to 0700. `mkdirat` returning success does not prove the entry is
        // still the same object one syscall later.
        //
        // `openat` closes it because the descriptor *is* the object: whatever
        // the name refers to afterwards, `fchmod` can only reach what was opened
        // — and `O_DIRECTORY` refuses the hard-linked file outright with
        // ENOTDIR, so the swap fails before any mode change is attempted.
        //
        // This ordering is possible only because the open asks for search-only
        // access where the platform has it. The umask masks bits off `mkdirat`'s
        // mode argument, so under `umask 0400` the directory lands at 0300 and a
        // plain `O_RDONLY` open fails with EACCES — which is why the chmod used
        // to have to come first. Verified by running the syscalls on macOS: an
        // `O_SEARCH` descriptor for a 0300 directory both accepts `fchmod` and
        // remains usable for the traversal that follows.
        let child = openat_dir_no_follow(dir.as_raw(), name)?;
        if created {
            // Only a directory this walk created is tightened; an existing one
            // belongs to the user. Failure is fatal rather than ignored: unlike
            // the old pre-open repair, there is no later step that would catch
            // the problem, and a snapshot directory left at the umask's mode
            // could be world-listable.
            fchmod(child.as_raw(), 0o700)?;
        }
        dir = child;
    }

    Ok((dir, file_name))
}

/// Test hook fired between a successful `mkdirat` and the mode being set.
///
/// This is the window an attacker races for, and it cannot be reached from
/// outside the module: by the time a test regains control the walk has already
/// finished. Without the hook, a test can only plant its swap *before* the walk
/// — which makes `mkdirat` return `EEXIST`, so `created` is false and the branch
/// under test never executes. That is precisely why an earlier swap test passed
/// against a name-based chmod and failed to detect the regression.
#[cfg(test)]
fn after_mkdirat_hook(dirfd: libc::c_int, name: &OsStr, created: bool) {
    tests::AFTER_MKDIRAT.with(|h| {
        if let Some(f) = h.borrow().as_ref() {
            f(dirfd, name, created);
        }
    });
}

/// No-op in production.
#[cfg(not(test))]
fn after_mkdirat_hook(_dirfd: libc::c_int, _name: &OsStr, _created: bool) {}

/// The mode [`walk_from`] requests when creating a directory.
///
/// Always `0700` in production. The umask reduces it further, which is the whole
/// reason the pre-open repair exists — but a test cannot set the umask to prove
/// that, because the umask is process-global and would corrupt every test running
/// in parallel. This hook lets a test lower the *requested* mode instead, which
/// produces the identical post-`mkdirat` state without touching global process
/// state.
#[cfg(test)]
fn creation_mode() -> libc::mode_t {
    tests::REQUESTED_DIR_MODE.with(|m| m.get())
}

/// The mode [`walk_from`] requests when creating a directory: owner-only.
#[cfg(not(test))]
fn creation_mode() -> libc::mode_t {
    0o700
}

/// Set the mode of a directory entry that this process just created, relative to
/// its parent's descriptor, returning the descriptor it used.
///
/// Open the directory, then `fchmod` **that descriptor** — never the name. Use
/// this immediately after a `mkdirat` you saw succeed, to set the mode the umask
/// masked off.
///
/// # Why it opens rather than taking the caller's word
///
/// The obvious implementation, `fchmodat(dirfd, name, mode,
/// AT_SYMLINK_NOFOLLOW)`, is unsafe here however carefully it is called. It
/// resolves a *name*, and between the caller's `mkdirat` and the chmod the
/// scenario's child can `rmdir` the new directory and put a **hard link to an
/// external file** at that same name. `AT_SYMLINK_NOFOLLOW` cannot help: a hard
/// link is not a symlink, it is the file itself under another name, so there is
/// nothing for a "don't follow" flag to refuse. The chmod would then set an
/// inode outside the tree to `mode`.
///
/// An earlier version of this helper documented that hazard and required callers
/// to verify the link count first. No caller did — which is exactly how the
/// defect survived. A contract that depends on every caller remembering an
/// unenforced step is not a contract; the safe sequence is now performed here,
/// so there is nothing left for a caller to get wrong.
///
/// Opening closes the hole two ways over: the descriptor *is* the object, so
/// `fchmod` cannot reach anything else no matter what the name later refers to,
/// and `O_DIRECTORY` rejects the hard-linked file outright with `ENOTDIR` before
/// any mode change is attempted.
///
/// # Why this works under a restrictive umask
///
/// `mkdirat`'s mode argument is masked by the umask, so at `umask 0400` the new
/// directory lands at `0300` — unreadable — and opening it `O_RDONLY` fails with
/// `EACCES`. That is why the chmod originally had to precede the open.
/// [`openat_dir_no_follow`] asks for search-only access where the platform
/// provides it, which suffices: verified by running the syscalls on macOS, an
/// `O_SEARCH` descriptor for a `0300` directory accepts `fchmod` and stays usable
/// for further traversal.
///
/// Where search-only access is unavailable (notably glibc Linux, which does not
/// define `O_SEARCH`), a directory the umask left unreadable cannot be opened,
/// so this returns the open error and the caller fails. That is the intended
/// trade: a run that cannot safely set the mode should fail rather than chmod an
/// unverified name.
pub fn chmod_created_dir(
    dirfd: libc::c_int,
    name: &OsStr,
    mode: libc::mode_t,
) -> std::io::Result<Fd> {
    let fd = openat_dir_no_follow(dirfd, name)?;
    fchmod(fd.as_raw(), mode)?;
    Ok(fd)
}

/// `CString` for a single path component, rejecting an embedded NUL.
fn component_cstring(name: &OsStr) -> std::io::Result<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::CString::new(name.as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))
}

/// Duplicate a borrowed descriptor into one the caller owns.
///
/// A walk reassigns its directory handle as it descends and drops the previous
/// one each time, so it needs an owned descriptor to start from — but the
/// anchor must outlive the walk. `F_DUPFD_CLOEXEC` gives an independent
/// descriptor for the *same* directory object, which is what preserves the
/// anti-swap property.
pub fn dup_fd(fd: libc::c_int) -> std::io::Result<Fd> {
    // SAFETY: `fd` is a live descriptor borrowed from the caller.
    let duped = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
    if duped < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(Fd(duped))
}

/// `openat(dirfd, name, O_DIRECTORY | O_NOFOLLOW)` — a symlinked component
/// fails with `ELOOP` rather than being traversed.
/// Search-only access is requested first where the platform offers it, because
/// traversal never needs to *list* a directory and a directory whose mode the
/// umask reduced to `0300` cannot be opened for reading at all. Opening it this
/// way is what lets [`walk_from`] hold a descriptor *before* fixing the mode —
/// see the `created` branch there.
pub fn openat_dir_no_follow(dirfd: libc::c_int, name: &OsStr) -> std::io::Result<Fd> {
    let c = component_cstring(name)?;

    let open_with = |access: libc::c_int| {
        // SAFETY: `dirfd` is an open directory descriptor and `c` a valid C
        // string that outlives the call.
        let fd = unsafe {
            libc::openat(
                dirfd,
                c.as_ptr(),
                access | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(Fd(fd))
        }
    };

    #[cfg(target_os = "macos")]
    {
        if let Ok(fd) = open_with(libc::O_SEARCH) {
            return Ok(fd);
        }
    }

    open_with(libc::O_RDONLY)
}

/// Refuse a descriptor whose inode has more than one name.
///
/// # The gap this closes
///
/// `O_NOFOLLOW` and the per-component walk refuse *symlinks*, but a **hard
/// link** is not a symlink — it is the file, reached by a second name. So an
/// attacker who can create one inside the anchored directory (same filesystem)
/// makes `workspace/out.snap` and an external `victim.snap` the same inode.
/// Every `openat(O_NOFOLLOW)` then succeeds, exactly as designed, and yet:
/// a read returns the external file's contents (so a planted snapshot can
/// satisfy an assertion), and a write truncates and rewrites that external
/// inode, chmod-ing it to `0600`. Containment is bypassed without a single
/// symlink being involved.
///
/// # Why `fstat` on the open descriptor is sound here
///
/// A path-based check would be classic TOCTOU: an attacker could pass the
/// check and then swap the name. Checking the *already-open descriptor* is
/// different in kind, because the descriptor is bound to one inode — the same
/// inode the subsequent read or write will use. There is no window in which
/// "the thing checked" and "the thing used" can become different objects.
///
/// # What remains open
///
/// The link count is live, so it can still change *after* this check while we
/// hold the descriptor:
/// - It can **drop** (the attacker unlinks their name). Harmless — they have
///   given up the access this check exists to deny.
/// - It can **rise** (the attacker links a new name to the inode after we
///   passed it). We then write content the attacker can read through their
///   name. This is not fixable at this layer — the inode is legitimately ours
///   and they are adding a reference to it — but it is strictly weaker than the
///   bypass above: it cannot redirect a write to a pre-existing external file,
///   cannot make a planted file satisfy an assertion, and the file's `0600`
///   mode confines it to the same uid. The documentation states this limit
///   rather than claiming hard links are fully handled.
fn refuse_multiply_linked(fd: libc::c_int) -> std::io::Result<()> {
    // SAFETY: zeroed `stat` is a valid initial value; `fstat` fills it.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: `fd` is an open descriptor owned by the caller.
    if unsafe { libc::fstat(fd, &mut st) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    if st.st_nlink > 1 {
        // Not `InvalidInput`: this is a refusal of an existing on-disk state,
        // and callers surface the message to the user.
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "refusing a file with {} hard links: a second name may point \
                 outside the workspace, so reading or writing it could escape \
                 containment",
                st.st_nlink
            ),
        ));
    }
    Ok(())
}

/// `openat(dirfd, name, O_WRONLY | O_CREAT | O_NOFOLLOW, mode)`.
///
/// Deliberately *not* `O_TRUNC`: a caller that must restrict an existing file's
/// mode before exposing new content has to chmod first and truncate after, so
/// truncation is left to the caller rather than folded into the open.
pub fn openat_file_write_no_follow(
    dirfd: libc::c_int,
    name: &OsStr,
    mode: libc::mode_t,
) -> std::io::Result<Fd> {
    let c = component_cstring(name)?;
    // SAFETY: `dirfd` is an open directory descriptor and `c` a valid C string.
    // The variadic `mode` is meaningful because O_CREAT is set.
    //
    // `O_NONBLOCK` for the same reason as the read side: opening a FIFO
    // `O_WRONLY` blocks until a *reader* appears, so a planted `mkfifo out.snap`
    // would wedge the recording indefinitely. The flag makes the open return so
    // `refuse_non_regular` can reject it, and is cleared once the descriptor is
    // known to be a regular file.
    let fd = unsafe {
        libc::openat(
            dirfd,
            c.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
            mode as libc::c_uint,
        )
    };
    if fd < 0 {
        let err = std::io::Error::last_os_error();
        // `O_WRONLY | O_NONBLOCK` on a FIFO with no reader fails ENXIO before
        // the type check below can run. Without translation the user sees
        // "Device not configured", which says nothing about the actual problem;
        // report the same diagnosis `refuse_non_regular` would have given.
        if err.raw_os_error() == Some(libc::ENXIO) {
            return Err(non_regular_error());
        }
        return Err(err);
    }
    let fd = Fd(fd);
    // Only a regular file can hold a snapshot; a FIFO, directory, socket or
    // device node at this name was not put there by pitty.
    refuse_non_regular(fd.as_raw())?;
    clear_nonblock(fd.as_raw())?;
    // A freshly created file has exactly one link, so this only ever rejects a
    // file that already existed with a second name — which is precisely the
    // hard-link bypass. See `refuse_multiply_linked`.
    refuse_multiply_linked(fd.as_raw())?;
    Ok(fd)
}

/// Refuse a descriptor that is not a regular file.
///
/// # Why "regular file or refuse" is the honest rule
///
/// Every record pitty reads back — a snapshot, a claim sidecar — is a plain file
/// it wrote itself. Anything else at that name was put there by something other
/// than pitty, and none of the alternatives can be handled meaningfully: a
/// directory is not content, a socket or device node has no snapshot semantics,
/// and a FIFO's "content" is whatever a writer chooses to feed it at read time.
/// So rather than enumerate what to reject, this accepts only what pitty could
/// legitimately have written.
///
/// Checked on the descriptor, not the path, for the same reason as
/// [`refuse_multiply_linked`]: the object inspected must be the object used, or
/// the check is just another name-based race.
fn refuse_non_regular(fd: libc::c_int) -> std::io::Result<()> {
    // SAFETY: zeroed `stat` is a valid initial value; `fstat` fills it.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: `fd` is an open descriptor owned by the caller.
    if unsafe { libc::fstat(fd, &mut st) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    if st.st_mode & libc::S_IFMT != libc::S_IFREG {
        return Err(non_regular_error());
    }
    Ok(())
}

/// The error reported for a path that is not a regular file.
///
/// Shared so the type check and the `ENXIO` translation in
/// [`openat_file_write_no_follow`] give the user one diagnosis rather than two
/// unrelated-looking messages for the same underlying situation.
fn non_regular_error() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        "refusing a path that is not a regular file: only a plain file can \
         hold a record pitty wrote",
    )
}

/// `openat(dirfd, name, O_RDONLY | O_NOFOLLOW)`, refusing anything that is not a
/// single-linked regular file.
///
/// The read counterpart. A reader must refuse the same links the writer does:
/// when only the write half was protected, the two could resolve to different
/// files, and a planted file could satisfy a comparison the real directory
/// could not. That applies to hard links as much as to symlinks, so this
/// rejects a multiply-linked file too.
///
/// # Why `O_NONBLOCK` on the open
///
/// Opening a **FIFO** for reading blocks until a writer appears — forever, if
/// none ever does. A scenario that leaves `mkfifo out.snap` behind would
/// therefore hang the snapshot comparison indefinitely: not a failure, not a
/// timeout, a wedged process that sits until CI's global timeout with no
/// diagnostic. That also defeats the point of having a distinct "unreadable"
/// outcome, since an open that never returns is not a state any result type can
/// represent.
///
/// `O_NONBLOCK` makes the open return immediately whatever the file type is, so
/// the `fstat` below can see what it actually got and refuse it. The flag is
/// then cleared, because it is wrong for the subsequent read: on a regular file
/// `O_NONBLOCK` has no effect on blocking, but leaving it set would make a
/// future caller's short read look like a real end-of-file.
pub fn openat_file_read_no_follow(dirfd: libc::c_int, name: &OsStr) -> std::io::Result<Fd> {
    let c = component_cstring(name)?;
    // SAFETY: `dirfd` is an open directory descriptor and `c` a valid C string.
    let fd = unsafe {
        libc::openat(
            dirfd,
            c.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let fd = Fd(fd);
    // Order matters only for the error reported, not for safety: both checks are
    // on the descriptor. Type first, because "not a regular file" is the more
    // specific diagnosis for a FIFO or directory.
    refuse_non_regular(fd.as_raw())?;
    refuse_multiply_linked(fd.as_raw())?;
    clear_nonblock(fd.as_raw())?;
    Ok(fd)
}

/// Drop `O_NONBLOCK` from an open descriptor.
///
/// The flag exists only to keep the *open* from blocking on a FIFO; once the
/// descriptor is known to be a regular file it should behave like any other, so
/// a later read cannot mistake `EAGAIN` for end-of-file.
fn clear_nonblock(fd: libc::c_int) -> std::io::Result<()> {
    // SAFETY: `fd` is an open descriptor owned by the caller.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: same descriptor, clearing one flag bit.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// `mkdirat(dirfd, name, mode)`; `EEXIST` is surfaced to the caller so it can
/// tell an existing directory from one it just created.
pub fn mkdirat(dirfd: libc::c_int, name: &OsStr, mode: libc::mode_t) -> std::io::Result<()> {
    let c = component_cstring(name)?;
    // SAFETY: `dirfd` is an open directory descriptor and `c` a valid C string.
    if unsafe { libc::mkdirat(dirfd, c.as_ptr(), mode) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// `fchmod` on a raw descriptor.
///
/// On a descriptor rather than a path so a concurrent swap cannot retarget the
/// mode change to some other file.
pub fn fchmod(fd: libc::c_int, mode: libc::mode_t) -> std::io::Result<()> {
    // SAFETY: `fd` is an open descriptor owned by the caller.
    if unsafe { libc::fchmod(fd, mode) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// `fchmod` on an owned `File`, via its descriptor.
pub fn fchmod_file(file: &std::fs::File, mode: libc::mode_t) -> std::io::Result<()> {
    use std::os::unix::io::AsRawFd;
    fchmod(file.as_raw_fd(), mode)
}

/// Open a directory by path, for capturing an anchor before untrusted code runs.
///
/// This *does* follow symlinks, and that is correct for its one job: obtaining
/// the initial descriptor for a directory that exists before any scenario
/// process is spawned. It must never be used to resolve a path component that
/// untrusted code could have influenced — that is what [`walk_from`] is for.
///
/// Whether the kernel would traverse `dir` as a path component — the question a
/// `..` actually asks.
///
/// # Why this is a syscall and not an attribute test
///
/// Three rounds of this guard tried to answer "may `..` cross this component?"
/// by inspecting a property of the component, and each was broken by the next
/// property nobody thought of: first "does it exist" (broken by a regular
/// **file**, which exists), then "is it a directory" via
/// `metadata().is_ok_and(|m| m.is_dir())` (broken by a directory with no
/// **search permission**, which is a directory). Each fix added one more
/// attribute, and each time the kernel knew something the attribute did not.
///
/// The property list does not terminate. Beyond permissions there are
/// filesystems mounted `noexec`, dangling mount points, an unresponsive NFS
/// server, and MAC layers such as SELinux — every one of which can make a path
/// that passes all three tests still fail to resolve. So this stops asking about
/// properties. Traversal is the kernel's decision, `open` is how the kernel is
/// asked, and its answer is adopted verbatim.
///
/// # Why `O_SEARCH` and specifically not `O_DIRECTORY`
///
/// Measured on macOS with the real syscalls, against directories created for
/// the purpose (the two disagree, and only one matches the kernel's own `..`):
///
/// | `dir` mode                     | `O_DIRECTORY` | `O_SEARCH` | kernel resolves `dir/..` |
/// |--------------------------------|---------------|------------|--------------------------|
/// | `0600` readable, not searchable| **OK**        | `EACCES`   | **refused**              |
/// | `0100` searchable, not readable| `EACCES`      | **OK**     | **resolves**             |
///
/// `O_DIRECTORY` implies `O_RDONLY`, so it requests *read* permission — the
/// wrong permission entirely. Traversing a component requires *search* (execute)
/// permission, which is exactly what `O_SEARCH` requests. Had `O_DIRECTORY` been
/// used, a `0600` directory would be the next silent data-loss bug and a `0100`
/// one a false refusal.
///
/// # What the `Err` kinds mean to callers
///
/// `NotFound` is deliberately distinguished from every other error, because the
/// two callers differ on whether absence is permanent: `--update` creates a
/// snapshot's missing parent directories, so an absent component may legitimately
/// become traversable, while `EACCES`/`ENOTDIR`/`ELOOP` are verdicts no directory
/// creation changes.
///
/// # Fallback where there is no `O_SEARCH`, and why it is not `O_DIRECTORY`
///
/// glibc Linux defines no `O_SEARCH` (musl aliases it to `O_PATH`), so the
/// question has to be asked another way there. It must not be asked with plain
/// `O_DIRECTORY`: that is precisely the flag the table above shows succeeding on
/// a `0600` directory the kernel refuses to traverse, so using it would
/// reintroduce the silent data-loss defect on every Unix without `O_SEARCH` —
/// glibc Linux among them, which is a CI platform.
///
/// So the fallback stops predicting the answer and **performs the traversal**,
/// letting the kernel decide in aggregate: it opens `dir/.`, which the kernel can
/// only resolve by descending *through* `dir`, and so requires exactly the search
/// permission a `..` requires. Measured on macOS against directories created for
/// the purpose (`O_SEARCH` was bypassed to exercise this arm directly):
///
/// | `dir`             | `open(dir/.)` | kernel resolves `dir/..` |
/// |-------------------|---------------|--------------------------|
/// | `0600`            | `EACCES`      | refused                  |
/// | `0000`            | `EACCES`      | refused                  |
/// | `0755`            | OK            | resolves                 |
/// | `0100`            | `EACCES`      | **resolves**             |
/// | a regular file    | `ENOTDIR`     | refused (`ENOTDIR`)      |
///
/// Every row either matches the kernel or is **stricter** than it. The single
/// divergence is `0100` (search-only, unreadable): the fallback refuses a
/// component the kernel would traverse, which is a false *refusal* — a reported
/// assertion failure — and never a false pass or a write to the wrong file. That
/// is the direction this module fails in by design, and it is the same
/// limitation already documented for a search-only *workspace* on [`open_dir_fd`].
///
/// **The Linux behaviour is not verified by execution**, for the reason recorded
/// on [`open_dir_fd`]: no Linux host, container runtime or remote builder was
/// available to this work. What *was* verified is the fallback arm itself, run on
/// macOS with `O_SEARCH` bypassed (see the `non_o_search_fallback_*` tests), and
/// the claim it rests on is a POSIX one — resolving a component requires search
/// permission on it — rather than a Linux-specific guess.
fn kernel_traverses_dir(dir: &std::path::Path) -> std::io::Result<()> {
    #[cfg(target_os = "macos")]
    if !force_non_o_search_fallback() {
        return open_for_search(dir);
    }
    traverse_through_dir(dir)
}

/// Ask for search access directly. Only available where the platform defines
/// `O_SEARCH`; see [`kernel_traverses_dir`] for why it is preferred.
#[cfg(target_os = "macos")]
fn open_for_search(dir: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;

    std::fs::OpenOptions::new()
        // `read(true)` sets O_RDONLY (0); the real access mode is in the flags.
        .read(true)
        .custom_flags(libc::O_SEARCH | libc::O_CLOEXEC)
        .open(dir)
        .map(|_| ())
}

/// Attempt the traversal instead of predicting it: opening `dir/.` forces the
/// kernel to descend through `dir`, so its verdict is the kernel's own.
///
/// The `.` is appended rather than the path being opened directly, because that
/// is the whole point — `open(dir)` asks about `dir`, while `open(dir/.)` asks
/// whether `dir` can be *entered*, which is the question a `..` asks.
fn traverse_through_dir(dir: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;

    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC)
        .open(dir.join("."))
        .map(|_| ())
}

/// Test-only switch that forces the non-`O_SEARCH` arm on a platform that has
/// `O_SEARCH`, so the fallback glibc Linux will take can be exercised by
/// execution here rather than only reasoned about.
#[cfg(all(target_os = "macos", test))]
fn force_non_o_search_fallback() -> bool {
    FORCE_FALLBACK.with(|f| f.get())
}

#[cfg(all(target_os = "macos", not(test)))]
fn force_non_o_search_fallback() -> bool {
    false
}

#[cfg(all(target_os = "macos", test))]
thread_local! {
    /// Thread-local so parallel tests cannot see each other's setting.
    static FORCE_FALLBACK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Run `body` with the non-`O_SEARCH` fallback forced on.
#[cfg(all(target_os = "macos", test))]
pub fn with_forced_fallback<T>(body: impl FnOnce() -> T) -> T {
    FORCE_FALLBACK.with(|f| f.set(true));
    let out = body();
    FORCE_FALLBACK.with(|f| f.set(false));
    out
}

/// Whether a `..` may lexically cancel `dir`, and whether a refusal is permanent.
///
/// Thin classification over [`kernel_traverses_dir`] so callers never re-derive
/// the `NotFound` distinction by hand. See that function for why the question is
/// put to the kernel rather than to `metadata`.
pub fn traversability(dir: &std::path::Path) -> Traversability {
    match kernel_traverses_dir(dir) {
        Ok(()) => Traversability::Traversable,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Traversability::Absent,
        Err(_) => Traversability::Refused,
    }
}

/// The kernel's verdict on traversing one path component.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Traversability {
    /// The kernel would descend through it; a `..` across it is sound.
    Traversable,
    /// Nothing is there. Permanent for a dangling link's destination, but
    /// `--update` may still create it under the snapshot's own path.
    Absent,
    /// The kernel refuses to traverse it and no directory creation will change
    /// that: a non-directory, an unsearchable directory, an unresolvable chain.
    Refused,
}

/// # Why `O_SEARCH` is tried first
///
/// Anchoring only ever needs to *traverse* the directory, never to list it, and
/// on Unix traversal requires execute/search permission, not read. A workspace
/// with mode `0300` is therefore perfectly usable — a child can `chdir` into it
/// and run — but `O_RDONLY` cannot open it. Requesting read here rejected such a
/// workspace outright.
///
/// `O_SEARCH` (POSIX 2008) asks for exactly the right thing. Where it exists it
/// opens both a search-only and an ordinary directory, and `mkdirat` / `openat`
/// relative to the result work normally — verified on macOS, where `O_SEARCH` is
/// `O_EXEC | O_DIRECTORY`, against a real `0300` directory.
///
/// # Why the fallback is `O_RDONLY` and not (yet) `O_PATH`
///
/// `O_SEARCH` is not universal: glibc Linux does not define it (musl aliases it
/// to `O_PATH`). Linux's `O_PATH` is the natural candidate for the same job
/// there, and `open(2)` does list `O_PATH` descriptors as usable for the `dirfd`
/// argument of the `*at()` calls, `openat` and `mkdirat` among them.
///
/// It is not adopted here for one reason only: **it has not been verified on
/// Linux by this work.** The `O_SEARCH` behavior above was confirmed by running
/// the actual syscalls against a real `0300` directory; no Linux machine,
/// container runtime, or remote builder was available to do the equivalent for
/// `O_PATH`, and shipping a security-relevant flag on the strength of a man page
/// is the mistake this module exists to avoid. An earlier version of this
/// comment asserted the opposite — that an `O_PATH` dirfd *fails* with `EBADF` —
/// which was an untested claim in the other direction and simply wrong; it has
/// been removed rather than softened.
///
/// **TODO (needs a Linux host):** confirm by execution that `mkdirat`,
/// `openat(O_CREAT | O_NOFOLLOW)`, `openat(O_RDONLY | O_NOFOLLOW)` and `fchmodat`
/// all work with an `O_PATH | O_DIRECTORY` dirfd on glibc and musl. If they do,
/// use `O_PATH` as the fallback here and a `0300` workspace supports snapshots
/// on Linux too. `tests/` has no coverage for this because it cannot be written
/// without such a host.
///
/// Until then: on platforms with `O_SEARCH` a search-only workspace works for
/// snapshots; elsewhere such a workspace cannot be *captured*, which is why
/// capture is deferred until a snapshot actually needs it (see
/// [`crate::workspace::Workspace::resolve_write_path`]). A run that records no
/// snapshot never asks for the descriptor and so is never affected either way.
pub fn open_dir_fd(dir: &std::path::Path) -> std::io::Result<std::os::fd::OwnedFd> {
    use std::os::unix::fs::OpenOptionsExt;

    let open_with = |flags: libc::c_int| {
        std::fs::OpenOptions::new()
            // `read(true)` sets O_RDONLY, which is 0; the real access mode comes
            // from `flags`. OpenOptions requires some access mode to be set.
            .read(true)
            .custom_flags(flags)
            .open(dir)
            .map(std::os::fd::OwnedFd::from)
    };

    #[cfg(target_os = "macos")]
    {
        if let Ok(fd) = open_with(libc::O_SEARCH | libc::O_CLOEXEC) {
            return Ok(fd);
        }
    }

    open_with(libc::O_DIRECTORY | libc::O_CLOEXEC)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use tempfile::tempdir;

    /// Adversarial check: in the fallback, a `0100` directory must classify as
    /// `Refused`, never `Absent`.
    ///
    /// This distinction is load-bearing and the dangerous direction is specific:
    /// under `SnapshotAccess::Record` an **absent** component is deliberately
    /// exempt (`--update` is about to create it), so misclassifying an
    /// unreadable-but-searchable directory as absent would let a `..` elide it
    /// and land the write on a neighbouring file. `Refused` is inert by
    /// comparison — it only ever produces a reported assertion failure.
    #[cfg(target_os = "macos")]
    #[test]
    fn non_o_search_fallback_never_misreports_a_live_directory_as_absent() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let search_only = root.join("m100");
        std::fs::create_dir(&search_only).unwrap();
        std::fs::set_permissions(&search_only, std::fs::Permissions::from_mode(0o100)).unwrap();

        let verdict = with_forced_fallback(|| traversability(&search_only));

        std::fs::set_permissions(&search_only, std::fs::Permissions::from_mode(0o755)).unwrap();

        assert_eq!(
            verdict,
            Traversability::Refused,
            "the fallback's 0100 divergence must be a false REFUSAL; classifying it \
             as Absent would make it exempt under --update and turn a false refusal \
             into a false pass that writes to the wrong file"
        );
    }

    /// The non-`O_SEARCH` fallback must fail **closed**.
    ///
    /// glibc Linux has no `O_SEARCH`, so it takes the `traverse_through_dir`
    /// arm. That arm cannot be reached normally on macOS, so these tests force
    /// it — the same technique used elsewhere in this module to exercise a
    /// platform path the host does not naturally take.
    ///
    /// The defect being guarded against is specific: judging a `0600` directory
    /// with `O_DIRECTORY` reports it traversable, while the kernel refuses
    /// `dir/..` with `EACCES`. Shipping that would reintroduce the round-18
    /// data-loss bug on every Unix without `O_SEARCH`.
    ///
    /// **The Linux behaviour itself is not verified here** — no Linux host was
    /// available — but the fallback *arm* is, by execution, on this host.
    #[cfg(target_os = "macos")]
    #[test]
    fn non_o_search_fallback_refuses_a_readable_but_unsearchable_directory() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let locked = root.join("m600");
        std::fs::create_dir(&locked).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o600)).unwrap();

        let verdict = with_forced_fallback(|| traversability(&locked));

        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();

        assert_eq!(
            verdict,
            Traversability::Refused,
            "a 0600 directory is readable but NOT searchable; the kernel refuses \
             `dir/..` through it, so the fallback must refuse it too rather than \
             reporting it traversable as plain O_DIRECTORY would"
        );
    }

    /// A `0000` directory — the round-18 shape — must also be refused by the
    /// fallback, not merely by the `O_SEARCH` path.
    #[cfg(target_os = "macos")]
    #[test]
    fn non_o_search_fallback_refuses_an_unsearchable_directory() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let locked = root.join("m000");
        std::fs::create_dir(&locked).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();

        let verdict = with_forced_fallback(|| traversability(&locked));

        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();

        assert_eq!(verdict, Traversability::Refused);
    }

    /// The fallback must not over-correct: an ordinary directory still resolves,
    /// and absence is still reported as absence (the `--update` case depends on
    /// that distinction surviving into this arm).
    #[cfg(target_os = "macos")]
    #[test]
    fn non_o_search_fallback_still_allows_an_ordinary_directory() {
        let dir = tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("plain")).unwrap();
        std::fs::write(root.join("regular"), "x").unwrap();

        with_forced_fallback(|| {
            assert_eq!(
                traversability(&root.join("plain")),
                Traversability::Traversable
            );
            assert_eq!(traversability(&root.join("absent")), Traversability::Absent);
            // A regular file is ENOTDIR, a permanent refusal — never `Absent`,
            // which would let `--update` treat it as creatable.
            assert_eq!(
                traversability(&root.join("regular")),
                Traversability::Refused
            );
        });
    }

    fn names(parts: &[&str]) -> Vec<OsString> {
        parts.iter().map(OsString::from).collect()
    }

    type AfterMkdirat = Box<dyn Fn(libc::c_int, &OsStr, bool)>;

    thread_local! {
        /// Fired inside `walk_from` between `mkdirat` and the mode being set.
        ///
        /// Thread-local so parallel tests cannot see each other's hook.
        pub(super) static AFTER_MKDIRAT: std::cell::RefCell<Option<AfterMkdirat>> =
            const { std::cell::RefCell::new(None) };

        /// The mode `walk_from` requests from `mkdirat`, overridable per test.
        ///
        /// Thread-local rather than a global: `cargo test` runs tests in parallel
        /// threads, so a process-wide switch would leak between them — the same
        /// hazard that makes setting the real umask in a test unacceptable.
        pub(super) static REQUESTED_DIR_MODE: std::cell::Cell<libc::mode_t> =
            const { std::cell::Cell::new(0o700) };
    }

    /// Run `f` with `walk_from` creating directories at `mode`.
    ///
    /// Simulates what a umask does to `mkdirat`'s mode argument without touching
    /// the process-global umask.
    fn with_creation_mode<T>(mode: libc::mode_t, f: impl FnOnce() -> T) -> T {
        REQUESTED_DIR_MODE.with(|m| m.set(mode));
        let out = f();
        REQUESTED_DIR_MODE.with(|m| m.set(0o700));
        out
    }

    #[test]
    fn walk_refuses_a_parent_dir_component() {
        // `..` would climb out of the anchor, undoing containment entirely, so
        // it must be refused before any syscall is made.
        let dir = tempdir().unwrap();
        let root = open_dir_fd(dir.path()).unwrap();
        use std::os::unix::io::AsRawFd;

        let err = walk_from(root.as_raw_fd(), &names(&["..", "x"]), CreateDirs::Yes)
            .expect_err("a parent-dir component must be refused");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        assert!(
            !dir.path().parent().unwrap().join("x").exists(),
            "nothing may be created outside the anchor"
        );
    }

    #[test]
    fn walk_refuses_a_symlinked_intermediate_directory() {
        // The core guarantee: a component that is a symlink when the walk
        // reaches it fails rather than being followed, even though the link was
        // planted before the walk started (which is what a concurrent child
        // achieves in practice).
        use std::os::unix::io::AsRawFd;
        let dir = tempdir().unwrap();
        let outside = tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("sub")).unwrap();

        let root = open_dir_fd(dir.path()).unwrap();
        let components = names(&["sub", "f"]);
        let err = walk_from(root.as_raw_fd(), &components, CreateDirs::Yes)
            .expect_err("a symlinked directory component must be refused");
        // The refusal comes from the kernel's O_NOFOLLOW handling, not from a
        // pre-check, so the errno is whatever the platform reports for "this
        // name is a symlink and you said not to follow it": ELOOP on Linux,
        // ENOTDIR on macOS (O_DIRECTORY is evaluated first there). Accept
        // either; what matters is that the walk refused rather than traversed.
        assert!(
            matches!(err.raw_os_error(), Some(libc::ELOOP) | Some(libc::ENOTDIR)),
            "expected a NOFOLLOW refusal, got {err}"
        );
        assert!(
            !outside.path().join("f").exists(),
            "nothing may be created through the link"
        );
    }

    #[test]
    fn walk_creates_only_missing_directories_and_leaves_existing_modes() {
        use std::os::unix::fs::PermissionsExt;
        use std::os::unix::io::AsRawFd;

        let dir = tempdir().unwrap();
        let existing = dir.path().join("existing");
        std::fs::create_dir(&existing).unwrap();
        std::fs::set_permissions(&existing, std::fs::Permissions::from_mode(0o755)).unwrap();

        let root = open_dir_fd(dir.path()).unwrap();
        let components = names(&["existing", "made", "f"]);
        let (_fd, file) = walk_from(root.as_raw_fd(), &components, CreateDirs::Yes)
            .expect("the walk must succeed");
        assert_eq!(file, OsStr::new("f"));

        // The pre-existing directory keeps the user's mode...
        let existing_mode = std::fs::metadata(&existing).unwrap().permissions().mode() & 0o777;
        assert_eq!(existing_mode, 0o755);
        // ...while the one the walk created is private.
        let made_mode = std::fs::metadata(existing.join("made"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(made_mode, 0o700);
    }

    #[test]
    fn walk_repairs_a_created_directory_mode_before_opening_it() {
        // A umask masks bits off `mkdirat`'s mode argument, so under `umask
        // 0400` the requested 0700 lands as 0300 — no read — and the
        // `O_RDONLY | O_DIRECTORY` open of that directory then fails with
        // EACCES. Repairing the mode only *after* the open was therefore
        // unreachable, and a nested snapshot path failed outright with
        // "Permission denied". The repair must happen before the open.
        //
        // The umask is deliberately NOT set here: it is process-global, so a
        // test that changes it corrupts every other test running in parallel.
        // Instead the effect is reproduced on the one directory whose mode the
        // walk is responsible for — the one it creates itself — by asserting the
        // post-condition that the broken ordering could not satisfy.
        //
        // Distinction that matters: a *pre-existing* directory left at 0300 is
        // correctly refused, because its mode belongs to the user and the walk
        // must not chmod it (see
        // `walk_creates_only_missing_directories_and_leaves_existing_modes`).
        // The bug was about directories the walk creates.
        //
        // TWO CLAIMS, ONLY ONE OF THEM UNIVERSAL:
        //
        // 1. Universal — if the walk succeeds, every directory it created is
        //    0700. The mode is set on a descriptor the walk opened, so the umask
        //    cannot leave a snapshot directory more permissive than intended.
        // 2. Platform-dependent — the walk *succeeds at all* on a directory the
        //    umask left at 0300. That needs search-only access
        //    (`openat_dir_no_follow` tries `O_SEARCH` first), which glibc Linux
        //    does not provide; there the fallback `O_RDONLY` open of a 0300
        //    directory is EACCES and the walk correctly fails.
        //
        // Asserting (2) unconditionally is a CI-red on Linux, so the outcome is
        // branched on.
        //
        // WHAT THIS TEST DOES NOT PROVE: it does not detect an ordering
        // regression. A name-based `fchmodat` before the open still yields 0700
        // in the uncontended case, so the Ok branch passes; and with search-only
        // access unavailable, omitting the chmod entirely still yields
        // PermissionDenied, so the Err branch passes. Verified by reinstating
        // `mkdirat -> fchmodat(name) -> openat` and watching every test here
        // still pass. The claim this test does support is narrower and still
        // worth having: the walk behaves correctly under a umask on both the
        // search-only and the fallback platform.
        //
        // The ordering itself is covered by
        // `the_mode_is_set_on_a_descriptor_not_on_the_name`, which swaps the
        // directory inside the post-`mkdirat` window where the distinction is
        // observable.
        use std::os::unix::fs::PermissionsExt;
        use std::os::unix::io::AsRawFd;

        let dir = tempdir().unwrap();
        let root = open_dir_fd(dir.path()).unwrap();

        // 0300 is what `mkdirat(0700)` yields under `umask 0400`. Requesting it
        // directly reproduces that post-creation state without touching the
        // process-global umask.
        let components = names(&["snaps", "deep", "out.snap"]);
        let walked = with_creation_mode(0o300, || {
            walk_from(root.as_raw_fd(), &components, CreateDirs::Yes)
        });

        match walked {
            Ok((_fd, file)) => {
                assert_eq!(file, OsStr::new("out.snap"));
                // Claim (1): every level the walk created is private, whatever
                // mode `mkdirat` actually managed to set.
                for level in ["snaps", "snaps/deep"] {
                    let mode = std::fs::metadata(dir.path().join(level))
                        .unwrap()
                        .permissions()
                        .mode()
                        & 0o777;
                    assert_eq!(mode, 0o700, "{level} must be 0700");
                }
            }
            Err(e) => {
                // Only acceptable as the documented search-only shortfall, and
                // only as a clean refusal. Any other errno means something real
                // broke, so it is not swallowed.
                assert_eq!(
                    e.kind(),
                    std::io::ErrorKind::PermissionDenied,
                    "a platform without search-only access must refuse the open \
                     cleanly; got {e}"
                );
                // The refusal must also be total: nothing may be left usable
                // below a directory the walk could not secure.
                assert!(
                    !dir.path().join("snaps/deep/out.snap").exists(),
                    "a refused walk must not leave a snapshot behind"
                );
            }
        }
    }

    #[test]
    fn a_created_directory_is_openable_even_when_mkdirat_lands_non_readable() {
        // The mechanism that lets the open precede the chmod: a directory left
        // at 0300 (what `mkdirat(0700)` yields under `umask 0400`) must still be
        // openable, so its mode can be fixed on a descriptor rather than a name.
        //
        // Where search-only access exists this succeeds; where it does not, the
        // open legitimately fails and the caller fails with it, which is the
        // documented trade — so the assertion follows what the platform can
        // actually do rather than hardcoding one outcome.
        use std::os::unix::fs::PermissionsExt;
        use std::os::unix::io::AsRawFd;

        let dir = tempdir().unwrap();
        let root = open_dir_fd(dir.path()).unwrap();
        mkdirat(root.as_raw_fd(), OsStr::new("masked"), 0o300).unwrap();

        match chmod_created_dir(root.as_raw_fd(), OsStr::new("masked"), 0o700) {
            Ok(_fd) => {
                let mode = std::fs::metadata(dir.path().join("masked"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777;
                assert_eq!(mode, 0o700, "the repair must reach the directory");
            }
            Err(e) => {
                // Acceptable only as a refusal to proceed, never as a silent
                // skip: the caller propagates this and the run fails.
                assert_eq!(
                    e.kind(),
                    std::io::ErrorKind::PermissionDenied,
                    "a platform without search-only access must refuse the open, \
                     not fail some other way: {e}"
                );
            }
        }
    }

    #[test]
    fn a_swapped_name_cannot_chmod_an_external_inode() {
        // The defect this replaces the name-based chmod to fix. After a
        // successful `mkdirat`, the scenario's child can win the race to `rmdir`
        // the new directory and put a **hard link to an external file** at that
        // same name. `fchmodat(..., AT_SYMLINK_NOFOLLOW)` would then chmod that
        // external inode to 0700 — the flag is no defense, because a hard link
        // is not a symlink but the file itself under another name.
        //
        // The fix opens the name and chmods the *descriptor*, so `O_DIRECTORY`
        // rejects the hard-linked file with ENOTDIR before any mode change is
        // attempted. This asserts the security-relevant outcome directly: the
        // external file's mode is untouched.
        use std::os::unix::fs::PermissionsExt;
        use std::os::unix::io::AsRawFd;

        let dir = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let victim = outside.path().join("victim");
        std::fs::write(&victim, "external").unwrap();
        std::fs::set_permissions(&victim, std::fs::Permissions::from_mode(0o644)).unwrap();

        // The post-swap state: the name holds a hard link to the external file.
        let swapped = dir.path().join("snaps");
        if std::fs::hard_link(&victim, &swapped).is_err() {
            // Cross-device: the attack is unavailable here.
            return;
        }

        let root = open_dir_fd(dir.path()).unwrap();
        let result = chmod_created_dir(root.as_raw_fd(), OsStr::new("snaps"), 0o700);

        assert!(result.is_err(), "a swapped non-directory must be refused");
        let mode = std::fs::metadata(&victim).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o644,
            "an inode outside the tree must never be chmod-ed"
        );
    }

    /// Run `f` with `hook` installed at `walk_from`'s post-`mkdirat` window.
    fn with_after_mkdirat<T>(
        hook: impl Fn(libc::c_int, &OsStr, bool) + 'static,
        f: impl FnOnce() -> T,
    ) -> T {
        AFTER_MKDIRAT.with(|h| *h.borrow_mut() = Some(Box::new(hook)));
        let out = f();
        AFTER_MKDIRAT.with(|h| *h.borrow_mut() = None);
        out
    }

    #[test]
    fn the_mode_is_set_on_a_descriptor_not_on_the_name() {
        // THE ordering regression, caught where it actually lives. The companion
        // swap tests plant their hard link *before* the walk, which makes
        // `mkdirat` return EEXIST — so `created` is false and the chmod branch
        // never runs, and a name-based chmod passed them unchanged. (That gap is
        // why this test exists: verified by reinstating
        // `mkdirat -> fchmodat(name) -> openat` and watching all other tests
        // still pass.)
        //
        // Here the swap happens in the real window — after `mkdirat` succeeded,
        // before the mode is set — via a hook at that exact point. A name-based
        // chmod then chmods the external inode; a descriptor-based one cannot,
        // because `O_DIRECTORY` refuses the hard-linked file first.
        use std::os::unix::fs::PermissionsExt;
        use std::os::unix::io::AsRawFd;

        let dir = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let victim = outside.path().join("victim");
        std::fs::write(&victim, "external").unwrap();
        std::fs::set_permissions(&victim, std::fs::Permissions::from_mode(0o644)).unwrap();

        let root = open_dir_fd(dir.path()).unwrap();
        let swap_target = victim.clone();
        let dir_path = dir.path().to_path_buf();

        let components = names(&["snaps", "out.snap"]);
        let walked = with_after_mkdirat(
            move |_dirfd, name, created| {
                if !created || name != OsStr::new("snaps") {
                    return;
                }
                // The child wins the race: remove the new directory and leave a
                // hard link to an external file at the same name.
                let planted = dir_path.join("snaps");
                let _ = std::fs::remove_dir(&planted);
                let _ = std::fs::hard_link(&swap_target, &planted);
            },
            || walk_from(root.as_raw_fd(), &components, CreateDirs::Yes),
        );

        assert!(
            walked.is_err(),
            "a component swapped for a non-directory must be refused"
        );
        let mode = std::fs::metadata(&victim).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o644,
            "the mode must be set on the opened descriptor, never on the name: \
             a name-based chmod reaches this external inode"
        );
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "external");
    }

    #[test]
    fn a_walk_whose_created_directory_is_swapped_writes_nothing_outside() {
        // The same attack through the real entry point. `walk_from` must not
        // chmod an external inode reached by a swapped component, and must fail
        // rather than proceeding.
        use std::os::unix::fs::PermissionsExt;
        use std::os::unix::io::AsRawFd;

        let dir = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let victim = outside.path().join("victim");
        std::fs::write(&victim, "external").unwrap();
        std::fs::set_permissions(&victim, std::fs::Permissions::from_mode(0o644)).unwrap();
        if std::fs::hard_link(&victim, dir.path().join("snaps")).is_err() {
            return;
        }

        let root = open_dir_fd(dir.path()).unwrap();
        let components = names(&["snaps", "out.snap"]);
        let walked = walk_from(root.as_raw_fd(), &components, CreateDirs::Yes);

        assert!(walked.is_err(), "the walk must refuse a swapped component");
        let mode = std::fs::metadata(&victim).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o644, "the external inode must be untouched");
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "external");
    }

    #[test]
    fn walk_without_create_refuses_a_missing_directory() {
        // A reader must not conjure directories into existence just by looking.
        use std::os::unix::io::AsRawFd;
        let dir = tempdir().unwrap();
        let root = open_dir_fd(dir.path()).unwrap();

        walk_from(root.as_raw_fd(), &names(&["absent", "f"]), CreateDirs::No)
            .expect_err("a missing directory must not be created by a read walk");
        assert!(!dir.path().join("absent").exists());
    }

    #[test]
    fn walk_refuses_an_empty_component_list() {
        use std::os::unix::io::AsRawFd;
        let dir = tempdir().unwrap();
        let root = open_dir_fd(dir.path()).unwrap();
        let err = walk_from(root.as_raw_fd(), &[], CreateDirs::Yes)
            .expect_err("an empty sequence names no file");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn both_opens_refuse_a_fifo_promptly_instead_of_blocking() {
        // A FIFO opened `O_RDONLY` blocks until a writer appears, and `O_WRONLY`
        // until a reader does — forever, if none ever comes. So a scenario that
        // leaves `mkfifo out.snap` behind used to wedge the snapshot step
        // indefinitely: not a failure, not a timeout, a hung process that sits
        // until CI's global timeout with no diagnostic. That also defeats the
        // point of a distinct "unreadable" outcome, because an open that never
        // returns is not a state any result type can represent.
        //
        // Both opens must refuse it, and refuse it *promptly*. The work runs on
        // a worker thread with a join deadline, so a regression fails this test
        // instead of wedging the whole suite — which is the failure mode a plain
        // assertion could not distinguish from success.
        use std::os::unix::io::AsRawFd;
        use std::sync::mpsc;

        let dir = tempdir().unwrap();
        let fifo = dir.path().join("out.snap");
        let c = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: `c` is a valid NUL-terminated path for the duration of the call.
        if unsafe { libc::mkfifo(c.as_ptr(), 0o600) } != 0 {
            // A filesystem without FIFOs: the hazard does not exist here.
            return;
        }

        let path = dir.path().to_path_buf();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let root = open_dir_fd(&path).unwrap();
            let read = openat_file_read_no_follow(root.as_raw_fd(), OsStr::new("out.snap"));
            let write =
                openat_file_write_no_follow(root.as_raw_fd(), OsStr::new("out.snap"), 0o600);
            // Send only the outcome; `Fd` is not `Send` and must not escape.
            let _ = tx.send((read.is_err(), write.is_err()));
        });

        let (read_refused, write_refused) = rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("opening a FIFO must return promptly, not block forever");

        assert!(read_refused, "the read open must refuse a FIFO");
        assert!(write_refused, "the write open must refuse a FIFO");
    }

    #[test]
    fn the_read_open_refuses_a_directory() {
        // "Regular file or refuse" is the rule, so the other non-regular types
        // are rejected too rather than producing a confusing partial read. A
        // directory is the one an ordinary mistake actually produces.
        use std::os::unix::io::AsRawFd;

        let dir = tempdir().unwrap();
        std::fs::create_dir(dir.path().join("out.snap")).unwrap();
        let root = open_dir_fd(dir.path()).unwrap();

        openat_file_read_no_follow(root.as_raw_fd(), OsStr::new("out.snap"))
            .expect_err("a directory must not be read as a snapshot");
    }

    #[test]
    fn an_ordinary_regular_file_is_still_readable_and_writable() {
        // Guard against over-correction: the type check must not reject the
        // normal case, and clearing `O_NONBLOCK` must leave a descriptor that
        // reads to completion rather than returning early.
        use std::io::Read;
        use std::os::unix::io::{AsRawFd, FromRawFd};

        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("out.snap"), "recorded-content").unwrap();
        let root = open_dir_fd(dir.path()).unwrap();

        let fd = openat_file_read_no_follow(root.as_raw_fd(), OsStr::new("out.snap"))
            .expect("a regular file must still be readable");
        // SAFETY: a freshly opened, owned descriptor nothing else holds.
        let mut file = unsafe { std::fs::File::from_raw_fd(fd.into_raw()) };
        let mut buf = String::new();
        file.read_to_string(&mut buf).unwrap();
        assert_eq!(buf, "recorded-content");

        openat_file_write_no_follow(root.as_raw_fd(), OsStr::new("out.snap"), 0o600)
            .expect("a regular file must still be writable");
    }

    #[test]
    fn both_opens_refuse_a_hard_linked_file() {
        // A hard link is NOT a symlink — it is the file under a second name —
        // so every `openat(O_NOFOLLOW)` in the walk succeeds on one. An
        // attacker who links an external `victim` to a name inside the anchored
        // directory therefore got both halves for free: a read returned the
        // external contents (letting a planted file satisfy an assertion) and a
        // write truncated and rewrote that external inode. Both opens must
        // refuse a file whose inode has more than one name.
        let dir = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let victim = outside.path().join("victim.snap");
        std::fs::write(&victim, "external-content").unwrap();

        let inside = dir.path().join("out.snap");
        // Same filesystem (both under the same temp root on this system), which
        // is what makes the link possible at all.
        if std::fs::hard_link(&victim, &inside).is_err() {
            // Cross-device or a filesystem without hard links: the attack is not
            // available here, so there is nothing to assert.
            return;
        }

        let root = open_dir_fd(dir.path()).unwrap();
        use std::os::unix::io::AsRawFd;

        let read_err = openat_file_read_no_follow(root.as_raw_fd(), OsStr::new("out.snap"))
            .expect_err("the read must refuse a multiply-linked file");
        assert_eq!(read_err.kind(), std::io::ErrorKind::PermissionDenied);

        let write_err =
            openat_file_write_no_follow(root.as_raw_fd(), OsStr::new("out.snap"), 0o600)
                .expect_err("the write must refuse a multiply-linked file");
        assert_eq!(write_err.kind(), std::io::ErrorKind::PermissionDenied);

        // The external inode must be untouched: not truncated, not chmod-ed.
        assert_eq!(
            std::fs::read_to_string(&victim).unwrap(),
            "external-content",
            "the external file must not have been rewritten"
        );
    }

    #[test]
    fn an_ordinary_single_linked_file_is_accepted() {
        // The guard must not break normal use. A freshly created snapshot has
        // one link, and so does an ordinary file in a git checkout, so neither
        // the create nor the reopen path may trip on the link count.
        let dir = tempdir().unwrap();
        let root = open_dir_fd(dir.path()).unwrap();
        use std::os::unix::io::AsRawFd;

        // Create: nlink becomes 1, must be accepted.
        openat_file_write_no_follow(root.as_raw_fd(), OsStr::new("new.snap"), 0o600)
            .expect("creating a new file must be allowed");
        // Reopen the now-existing single-linked file, for both read and write.
        openat_file_write_no_follow(root.as_raw_fd(), OsStr::new("new.snap"), 0o600)
            .expect("rewriting a single-linked file must be allowed");
        openat_file_read_no_follow(root.as_raw_fd(), OsStr::new("new.snap"))
            .expect("reading a single-linked file must be allowed");
    }

    #[test]
    fn unlinking_the_second_name_restores_acceptance() {
        // The check reads the *live* link count from the open descriptor, so an
        // attacker who withdraws their extra name has genuinely given up the
        // access the guard denies, and the file becomes usable again. This pins
        // that the guard tracks current reality rather than a sticky flag.
        let dir = tempdir().unwrap();
        let a = dir.path().join("a.snap");
        std::fs::write(&a, "x").unwrap();
        let b = dir.path().join("b.snap");
        if std::fs::hard_link(&a, &b).is_err() {
            return;
        }

        let root = open_dir_fd(dir.path()).unwrap();
        use std::os::unix::io::AsRawFd;

        openat_file_read_no_follow(root.as_raw_fd(), OsStr::new("a.snap"))
            .expect_err("two links must be refused");
        std::fs::remove_file(&b).unwrap();
        openat_file_read_no_follow(root.as_raw_fd(), OsStr::new("a.snap"))
            .expect("one link must be accepted again");
    }
}
