//! Windows job objects: the reliable way to kill a process *tree*.
//!
//! `TerminateProcess` on the direct child is not enough under ConPTY. Git for
//! Windows ships `bin\bash.exe` as a launcher that spawns the real
//! `usr\bin\bash.exe`, and any shell can leave grandchildren behind; every
//! process still attached to the pseudoconsole keeps the console host — and
//! therefore the output pipe the reader thread blocks on — alive. Putting the
//! child in a job with `KILL_ON_JOB_CLOSE` means `TerminateJobObject` (and,
//! as a backstop, dropping the handle) takes the whole tree down at once.
//!
//! Why not `taskkill /T`: it costs a process spawn per teardown, depends on a
//! tool being on PATH, and races PID reuse. A job handle does the same thing
//! in-process and also covers the case where pitty itself dies unexpectedly.
//!
//! Known gap: the child is assigned right after `CreateProcess` returns, not
//! atomically with it (portable-pty 0.8 exposes neither `CREATE_SUSPENDED`
//! nor `PROC_THREAD_ATTRIBUTE_JOB_LIST`), so a descendant spawned in that
//! window is not a member. Nested jobs are required for this to work at all
//! on a runner that already sits in a job; that is standard since Windows 8.

use std::io;
use std::os::windows::io::RawHandle;
use std::ptr;

use winapi::um::handleapi::CloseHandle;
use winapi::um::jobapi2::{
    AssignProcessToJobObject, CreateJobObjectW, SetInformationJobObject, TerminateJobObject,
};
use winapi::um::winnt::{
    JobObjectExtendedLimitInformation, HANDLE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};

/// An owned job object handle configured to kill its members when closed.
pub struct Job(HANDLE);

// The handle is a plain kernel object reference; the kernel serializes access,
// so moving or sharing it across threads is sound.
unsafe impl Send for Job {}
unsafe impl Sync for Job {}

impl Job {
    /// Create an anonymous job whose members die when the last handle closes.
    pub fn new() -> io::Result<Self> {
        // SAFETY: both arguments are documented as optional (NULL is valid),
        // and the returned handle is checked before use.
        let handle = unsafe { CreateJobObjectW(ptr::null_mut(), ptr::null()) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        let job = Job(handle);

        // SAFETY: `info` is a properly sized, zero-initialised struct of the
        // exact type `JobObjectExtendedLimitInformation` expects, and the
        // length passed matches its size.
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let ok = unsafe {
            SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                &mut info as *mut _ as *mut _,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(job)
    }

    /// Add a process (by handle) to the job. Its future descendants join
    /// automatically; anything it spawned *before* this call does not, which
    /// is why the session assigns immediately after spawn.
    pub fn assign(&self, process: RawHandle) -> io::Result<()> {
        // SAFETY: both handles are valid for the duration of the call; the
        // process handle comes from portable-pty's live child.
        let ok = unsafe { AssignProcessToJobObject(self.0, process as HANDLE) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Terminate every process in the job now.
    pub fn terminate(&self) -> io::Result<()> {
        // SAFETY: the job handle is valid until `Drop`; the exit code is
        // arbitrary and only observable by whoever waits on the members.
        let ok = unsafe { TerminateJobObject(self.0, 1) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        // SAFETY: the handle was returned by CreateJobObjectW and is closed
        // exactly once here. Closing the last handle kills remaining members
        // (KILL_ON_JOB_CLOSE), which is the intended backstop.
        unsafe {
            CloseHandle(self.0);
        }
    }
}
