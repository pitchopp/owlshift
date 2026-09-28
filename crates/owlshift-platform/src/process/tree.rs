//! Starting a program as the root of a process tree that can be stopped as a
//! whole: a process group of its own on Unix, a Job Object on Windows.
//!
//! Every process the root starts joins the tree, so stopping the tree leaves
//! nothing behind. On Unix a descendant can still leave its group on purpose
//! (`setsid`, `setpgid`); a Windows job gives no such way out.

use std::io;
use std::process::{Child, Command};

/// The handle that stops a process tree started by [`ProcessTree::spawn`].
///
/// It is separate from the root's [`Child`] so that one thread can stop the
/// tree while another is blocked on the child, reading its output or waiting
/// on it.
///
/// Dropping it stops nothing: a caller that gives up on a tree must call
/// [`ProcessTree::kill`] first, or the tree keeps running.
///
/// On Unix the tree is live from its spawn until this handle is dropped: a
/// process that called [`stop_trees_on_signal`] stops it when told to end.
///
/// [`stop_trees_on_signal`]: super::stop_trees_on_signal
#[derive(Debug)]
pub struct ProcessTree {
    #[cfg(unix)]
    group: rustix::process::Pid,
    #[cfg(unix)]
    _live: super::signals::Registration,
    #[cfg(windows)]
    job: std::os::windows::io::OwnedHandle,
}

impl ProcessTree {
    /// Spawns `command` as the root of a new process tree.
    ///
    /// The command is changed first: on Unix it starts in a new process
    /// group, on Windows it is created suspended (replacing any creation
    /// flags set before), placed in a new Job Object, and only then resumed,
    /// so it cannot start a process outside the job.
    ///
    /// The caller owns the returned child and reaps it; the tree handle only
    /// stops processes.
    pub fn spawn(command: &mut Command) -> io::Result<(Child, Self)> {
        imp::spawn(command)
    }

    /// Stops every process of the tree at once, the root included, without
    /// waiting for them to end; the root still has to be reaped through its
    /// [`Child`]. Stopping a tree whose processes have all ended succeeds.
    pub fn kill(&self) -> io::Result<()> {
        imp::kill(self)
    }
}

#[cfg(unix)]
pub(super) use imp::kill_group;

#[cfg(unix)]
mod imp {
    use super::super::signals;
    use super::ProcessTree;
    use rustix::io::Errno;
    use rustix::process::{Pid, Signal, kill_process_group};
    use std::io;
    use std::os::unix::process::CommandExt;
    use std::process::{Child, Command};

    pub(super) fn spawn(command: &mut Command) -> io::Result<(Child, ProcessTree)> {
        // A group id of 0 makes the child the leader of a new group whose id
        // is its own pid.
        command.process_group(0);
        let (child, group, live) = signals::register(|| command.spawn())?;
        Ok((child, ProcessTree { group, _live: live }))
    }

    pub(super) fn kill(tree: &ProcessTree) -> io::Result<()> {
        kill_group(tree.group)
    }

    /// Sends `SIGKILL` to the group.
    ///
    /// The group id stays reserved while the root is unreaped or any member
    /// is alive. Once the root is reaped and the group is empty, the number
    /// could in theory be reused by an unrelated group after the pid space
    /// wraps around; callers stop a tree while its run is still theirs, and
    /// the signal thread only while its handle is.
    pub(in crate::process) fn kill_group(group: Pid) -> io::Result<()> {
        match kill_process_group(group, Signal::KILL) {
            // No process left in the group.
            Ok(()) | Err(Errno::SRCH) => Ok(()),
            // macOS answers EPERM for a group left with only zombies, such as
            // a root that exited and is not reaped yet (checked 2026-09-28 on
            // macOS 26.6).
            #[cfg(target_os = "macos")]
            Err(Errno::PERM) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}

#[cfg(windows)]
mod imp {
    //! `std::process::Command` on stable Rust offers no way to get the main
    //! thread handle of a child (`ChildExt::main_thread_handle` is unstable)
    //! nor to pass a job list at creation (`PROC_THREAD_ATTRIBUTE_JOB_LIST`),
    //! both checked on rustc 1.98.1 on 2026-09-28. The child is therefore
    //! created with `CREATE_SUSPENDED`, assigned to the job, and its single
    //! thread is found through a Toolhelp thread snapshot and resumed. A
    //! suspended process runs no code, so it cannot start a process before
    //! it is in the job; the processes it starts later inherit the job.

    use super::ProcessTree;
    use std::io;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::os::windows::process::CommandExt;
    use std::process::{Child, Command};
    use std::ptr;
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
    };
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, TerminateJobObject,
    };
    use windows_sys::Win32::System::Threading::{
        CREATE_SUSPENDED, OpenThread, ResumeThread, THREAD_SUSPEND_RESUME,
    };

    /// The exit code the processes of a stopped tree get, as with
    /// `Child::kill`.
    const KILLED: u32 = 1;

    pub(super) fn spawn(command: &mut Command) -> io::Result<(Child, ProcessTree)> {
        // The job comes first: failing here leaves no process to clean up.
        // SAFETY: both pointers may be null, for default security and an
        // unnamed job.
        let raw = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
        if raw.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the call succeeded, so `raw` is a new handle owned here.
        let job = unsafe { OwnedHandle::from_raw_handle(raw) };

        command.creation_flags(CREATE_SUSPENDED);
        let mut child = command.spawn()?;
        if let Err(error) = contain(&job, &child) {
            // The child never ran; best effort, the error to report is the
            // one above.
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }
        Ok((child, ProcessTree { job }))
    }

    /// Places the suspended child in the job, then lets it run.
    fn contain(job: &OwnedHandle, child: &Child) -> io::Result<()> {
        // SAFETY: both handles are open for the duration of the call.
        if unsafe { AssignProcessToJobObject(job.as_raw_handle(), child.as_raw_handle()) } == 0 {
            return Err(io::Error::last_os_error());
        }
        resume(child.id())
    }

    /// Resumes the threads of a process created suspended: its main thread.
    fn resume(pid: u32) -> io::Result<()> {
        // SAFETY: no pointer argument; the process id is ignored for threads.
        let raw = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
        if raw == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the call succeeded, so `raw` is a new handle owned here.
        let snapshot = unsafe { OwnedHandle::from_raw_handle(raw) };

        let mut entry = THREADENTRY32 {
            dwSize: size_of::<THREADENTRY32>() as u32,
            cntUsage: 0,
            th32ThreadID: 0,
            th32OwnerProcessID: 0,
            tpBasePri: 0,
            tpDeltaPri: 0,
            dwFlags: 0,
        };
        let mut resumed = 0;
        // SAFETY: `entry` is a live local with `dwSize` set, as required.
        let mut more = unsafe { Thread32First(snapshot.as_raw_handle(), &mut entry) } != 0;
        while more {
            if entry.th32OwnerProcessID == pid {
                // SAFETY: no pointer argument.
                let raw = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) };
                if raw.is_null() {
                    return Err(io::Error::last_os_error());
                }
                // SAFETY: the call succeeded, so `raw` is a new handle owned
                // here.
                let thread = unsafe { OwnedHandle::from_raw_handle(raw) };
                // SAFETY: `thread` is open for the duration of the call.
                if unsafe { ResumeThread(thread.as_raw_handle()) } == u32::MAX {
                    return Err(io::Error::last_os_error());
                }
                resumed += 1;
            }
            // SAFETY: as for `Thread32First`.
            more = unsafe { Thread32Next(snapshot.as_raw_handle(), &mut entry) } != 0;
        }
        if resumed == 0 {
            return Err(io::Error::other("the new process has no thread to resume"));
        }
        Ok(())
    }

    pub(super) fn kill(tree: &ProcessTree) -> io::Result<()> {
        // SAFETY: the job handle is open for the duration of the call.
        if unsafe { TerminateJobObject(tree.job.as_raw_handle(), KILLED) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::ProcessTree;

    /// The executor stops a tree from one thread while another drives the
    /// child.
    const _: () = {
        const fn shareable<T: Send + Sync>() {}
        shareable::<ProcessTree>();
    };
}
