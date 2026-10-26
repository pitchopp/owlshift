//! Starting a program as the root of a process tree that can be stopped as a
//! whole: a process group of its own on Unix, a Job Object on Windows.
//!
//! Every process the root starts joins the tree, so stopping the tree leaves
//! nothing behind. On Unix a descendant can leave its group on purpose
//! (`setsid`, `setpgid`), as Claude Code does for each Bash call: stopping
//! the tree searches for it then, by parent pid, with the members of a group
//! it leads (OWL-451), and misses one whose parent and group leader have both
//! ended, unless the tree is a run's, tied to its worktree, and the run left
//! it there ([`ProcessTree::spawn_in`], OWL-452). A Windows job gives no such
//! way out.

use std::io;
use std::path::Path;
use std::process::{Child, Command, ExitStatus};

use super::AtCheckedStop;

/// The handle that stops a process tree started by [`ProcessTree::spawn`].
///
/// It is separate from the root's [`Child`] so that one thread can stop the
/// tree while another is blocked on the child, reading its output or waiting
/// on it.
///
/// Dropping it stops nothing: a caller that gives up on a tree must call
/// [`ProcessTree::kill`] first, or the tree keeps running.
///
/// The tree is live from its spawn until this handle is dropped: a process
/// that called [`stop_trees_on_signal`] stops it when told to end, and on
/// Unix one that called `stop_trees_when_killed` has it stopped when it ends
/// in any other way, killed outright or not.
///
/// [`stop_trees_on_signal`]: super::stop_trees_on_signal
#[derive(Debug)]
pub struct ProcessTree {
    #[cfg(unix)]
    stopper: super::signals::Stopper,
    #[cfg(windows)]
    job: std::sync::Arc<std::os::windows::io::OwnedHandle>,
    _live: super::signals::Registration,
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
    ///
    /// A checked stop leaves the tree alone ([`AtCheckedStop::Left`]).
    pub fn spawn(command: &mut Command) -> io::Result<(Child, Self)> {
        Self::spawn_for(command, AtCheckedStop::Left)
    }

    /// [`ProcessTree::spawn`], the tree treated as `at` says by a checked
    /// stop of a process that stops by degrees (OWL-161): a tree
    /// [`AtCheckedStop::Told`] is refused once any stop is asked.
    pub fn spawn_for(command: &mut Command, at: AtCheckedStop) -> io::Result<(Child, Self)> {
        imp::spawn(command, at, None)
    }

    /// [`ProcessTree::spawn_for`] for a run working in `worktree`, an
    /// existing folder (OWL-452): on macOS and Linux every stop of the tree
    /// also reaches what the run left there once its parent and its group's
    /// leader ended, such as a background job a Bash call left once it
    /// returned, as the process table ties it to the run (architecture
    /// section 5, "Which processes"). The worktree is canonicalized first,
    /// and a failure to do so is the spawn's error. On Windows, where the
    /// Job Object holds every process the root starts, it changes nothing.
    pub fn spawn_in(
        command: &mut Command,
        at: AtCheckedStop,
        worktree: &Path,
    ) -> io::Result<(Child, Self)> {
        imp::spawn(command, at, Some(worktree))
    }

    /// Stops every process of the tree at once, the root included, without
    /// waiting for them to end; the root still has to be reaped through its
    /// [`Child`]. Stopping a tree whose processes have all ended succeeds.
    ///
    /// On Unix that includes what left the tree's group while it still
    /// descends from it, found in the process table at the stop (OWL-451),
    /// and, for a tree spawned in a worktree with [`ProcessTree::spawn_in`],
    /// what the run left there, even once the root ended (OWL-452): a few
    /// listings of the table, nothing waited on.
    pub fn kill(&self) -> io::Result<()> {
        imp::kill(self)
    }

    /// Withdraws the tree from the live ones, then reaps its root.
    ///
    /// The order is what keeps a reaped pid from being signalled on Unix: the
    /// group id is the root's pid, reserved while the root is unreaped, and
    /// once the tree is withdrawn neither the signal handler nor the sentinel
    /// stops it any more. Dropping the handle can block for good when the
    /// process is being stopped by a signal, by design. A caller that waits
    /// for the root's end without reaping it ([`ExitWatch`]) stops what the
    /// tree left, then reaps it here.
    pub fn reap(self, child: &mut Child) -> io::Result<ExitStatus> {
        drop(self);
        child.wait()
    }

    /// The tree's entry among the live ones.
    #[cfg(test)]
    pub(super) fn registration(&self) -> &super::signals::Registration {
        &self._live
    }
}

pub use imp::ExitWatch;
#[cfg(unix)]
pub(super) use imp::kill_group;
#[cfg(windows)]
pub(super) use imp::terminate_job;

#[cfg(unix)]
mod imp {
    use super::super::descendants::{self, Workdir};
    use super::super::signals::{self, Stopper};
    use super::{AtCheckedStop, ProcessTree};
    use rustix::io::Errno;
    use rustix::process::{
        Pid, Signal, WaitId, WaitIdOptions, kill_process, kill_process_group, waitid,
    };
    use std::io;
    use std::os::unix::process::CommandExt;
    use std::path::Path;
    use std::process::{Child, Command};
    use std::sync::Arc;
    use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
    use std::thread;
    use std::time::Instant;

    pub(super) fn spawn(
        command: &mut Command,
        at: AtCheckedStop,
        worktree: Option<&Path>,
    ) -> io::Result<(Child, ProcessTree)> {
        let worktree = worktree.map(std::fs::canonicalize).transpose()?;
        // A group id of 0 makes the child the leader of a new group whose id
        // is its own pid.
        command.process_group(0);
        let mut made = None;
        let stopper = |child: &Child| {
            // The root's start, read while it is unreaped (OWL-452).
            let left_in = worktree
                .and_then(|path| Workdir::new(path, child.id()))
                .map(Arc::new);
            let stopper = Stopper {
                group: Pid::from_child(child),
                left_in,
            };
            made = Some(stopper.clone());
            stopper
        };
        let (child, live) = signals::register(|| command.spawn(), stopper, at)?;
        let stopper = made.expect("a registered tree has its stopper");
        Ok((
            child,
            ProcessTree {
                stopper,
                _live: live,
            },
        ))
    }

    pub(super) fn kill(tree: &ProcessTree) -> io::Result<()> {
        kill_group(&tree.stopper)
    }

    /// Tells when the root of a tree exits, without reaping it.
    ///
    /// A thread blocks in `waitid` with `WNOWAIT`, which leaves the root a
    /// zombie: its pid, the tree's group id, stays reserved until the caller
    /// reaps it through its [`Child`], so stopping the tree after the root
    /// exited never signals a reused pid. `waitid` with `WEXITED | WNOWAIT`
    /// was checked on 2026-10-15 on Linux 6.18; macOS by the CI leg running
    /// the tests of `process`.
    #[derive(Debug)]
    pub struct ExitWatch(Receiver<io::Result<()>>);

    impl ExitWatch {
        /// Starts watching `child`, a root not reaped yet.
        pub fn start(child: &Child) -> io::Result<Self> {
            let pid = Pid::from_child(child);
            let (sender, receiver) = mpsc::channel();
            thread::Builder::new()
                .name("owlshift-exit-watch".into())
                .spawn(move || {
                    let exit = WaitIdOptions::EXITED | WaitIdOptions::NOWAIT;
                    let outcome = loop {
                        match waitid(WaitId::Pid(pid), exit) {
                            // Without `WNOHANG`, it returns only once the
                            // child has exited.
                            Ok(_) => break Ok(()),
                            Err(Errno::INTR) => {}
                            Err(error) => break Err(error.into()),
                        }
                    };
                    // The receiver is gone when the run timed out.
                    let _ = sender.send(outcome);
                })?;
            Ok(Self(receiver))
        }

        /// Waits until the root exits, `true`, or the deadline passes,
        /// `false`. The `true` is given once: the answer is consumed.
        pub fn exited_by(&self, _child: &Child, deadline: Instant) -> io::Result<bool> {
            match self
                .0
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            {
                Ok(outcome) => outcome.map(|()| true),
                Err(RecvTimeoutError::Timeout) => Ok(false),
                // Only if the thread panicked.
                Err(RecvTimeoutError::Disconnected) => Err(io::Error::other(
                    "the thread watching the program ended without an answer",
                )),
            }
        }
    }

    /// Sends `SIGKILL` to the group, and first to what left it (OWL-451):
    /// the group is frozen with `SIGSTOP`, so that nothing in it starts or
    /// reaps a process meanwhile, then every process descending from one of
    /// its own, and every member of a group one of those leads, is found,
    /// frozen and checked again, as `descendants` says, and killed. For a
    /// tree tied to a run's worktree, the search also starts from what the
    /// run left there (OWL-452), and runs even when the group has no live
    /// process left, as once the run ended by itself; it then starts from
    /// those alone, never from the group's id.
    ///
    /// The group id stays reserved while the root is unreaped or any member
    /// is alive: [`run_command`] reaps the root only after the tree is
    /// withdrawn ([`ProcessTree::reap`]), and [`ExitWatch`] never reaps.
    /// Once the root is reaped and the group is empty, the number could in
    /// theory be reused by an unrelated group after the pid space wraps
    /// around; callers stop a tree while its run is still theirs, and the
    /// signal thread only while its handle is.
    ///
    /// The result is the group's own kill: the search is best effort, and a
    /// search that fails leaves what left the group running.
    ///
    /// [`run_command`]: crate::process::run_command
    pub(in crate::process) fn kill_group(stopper: &Stopper) -> io::Result<()> {
        let group = stopper.group;
        // A group with no process left, or zombies only, has nothing that
        // still descends from it by a parent pid; any other failure skips
        // the search from it, and the group's kill answers.
        let live = signal_group(group, Signal::STOP).unwrap_or(false);
        if live || stopper.left_in.is_some() {
            let found = descendants::freeze_reached(
                live.then(|| group.as_raw_nonzero().get()),
                stopper.left_in.as_deref(),
                descendants::list,
                descendants::cwd,
            );
            for pid in found {
                // Best effort: one that ended needs nothing.
                let _ = kill_process(pid, Signal::KILL);
            }
        }
        signal_group(group, Signal::KILL).map(drop)
    }

    /// Sends `signal` to the group: whether it had a live process to get it.
    fn signal_group(group: Pid, signal: Signal) -> io::Result<bool> {
        match kill_process_group(group, signal) {
            Ok(()) => Ok(true),
            // No process left in the group.
            Err(Errno::SRCH) => Ok(false),
            // macOS answers EPERM for a group left with only zombies, such as
            // a root that exited and is not reaped yet (checked 2026-09-28 on
            // macOS 26.6).
            #[cfg(target_os = "macos")]
            Err(Errno::PERM) => Ok(false),
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

    use super::super::signals;
    use super::{AtCheckedStop, ProcessTree};
    use std::io;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use std::os::windows::process::CommandExt;
    use std::process::{Child, Command};
    use std::ptr;
    use std::sync::Arc;
    use std::time::Instant;
    use windows_sys::Win32::Foundation::{
        INVALID_HANDLE_VALUE, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT,
    };
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
    };
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, TerminateJobObject,
    };
    use windows_sys::Win32::System::Threading::{
        CREATE_SUSPENDED, INFINITE, OpenThread, ResumeThread, THREAD_SUSPEND_RESUME,
        WaitForSingleObject,
    };

    /// The exit code the processes of a stopped tree get, as with
    /// `Child::kill`.
    const KILLED: u32 = 1;

    /// The worktree is not needed: the job holds whatever the run leaves.
    pub(super) fn spawn(
        command: &mut Command,
        at: AtCheckedStop,
        _worktree: Option<&std::path::Path>,
    ) -> io::Result<(Child, ProcessTree)> {
        // The job comes first: failing here leaves no process to clean up.
        // SAFETY: both pointers may be null, for default security and an
        // unnamed job.
        let raw = unsafe { CreateJobObjectW(ptr::null(), ptr::null()) };
        if raw.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the call succeeded, so `raw` is a new handle owned here.
        let job = Arc::new(unsafe { OwnedHandle::from_raw_handle(raw) });

        command.creation_flags(CREATE_SUSPENDED);
        let spawn = || {
            let mut child = command.spawn()?;
            if let Err(error) = contain(&job, &child) {
                // The child never ran; best effort, the error to report is
                // the one above.
                let _ = child.kill();
                let _ = child.wait();
                return Err(error);
            }
            Ok(child)
        };
        let (child, live) = signals::register(spawn, |_| Arc::clone(&job), at)?;
        Ok((child, ProcessTree { job, _live: live }))
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
        terminate_job(&tree.job)
    }

    /// Tells when the root of a tree exits, without reaping it: a wait with a
    /// timeout on the root's process handle, so no thread is needed. Nothing
    /// here signals a pid: the tree is stopped through its job handle, the
    /// root through its process handle, which keeps the process object.
    #[derive(Debug)]
    pub struct ExitWatch;

    impl ExitWatch {
        /// Starts watching `child`, a root not reaped yet.
        pub fn start(_child: &Child) -> io::Result<Self> {
            Ok(Self)
        }

        /// Waits until the root exits, `true`, or the deadline passes,
        /// `false`.
        pub fn exited_by(&self, child: &Child, deadline: Instant) -> io::Result<bool> {
            loop {
                let left = deadline.saturating_duration_since(Instant::now());
                // Whole milliseconds, rounded up, short of `INFINITE`.
                let millis = u32::try_from(left.as_nanos().div_ceil(1_000_000))
                    .unwrap_or(u32::MAX)
                    .min(INFINITE - 1);
                // SAFETY: the child's handle is open while it is borrowed.
                match unsafe { WaitForSingleObject(child.as_raw_handle(), millis) } {
                    WAIT_OBJECT_0 => return Ok(true),
                    // The timer may fire a tick early, or the deadline was
                    // too far for one wait: check it again.
                    WAIT_TIMEOUT if Instant::now() >= deadline => return Ok(false),
                    WAIT_TIMEOUT => {}
                    WAIT_FAILED => return Err(io::Error::last_os_error()),
                    other => return Err(io::Error::other(format!("wait returned {other}"))),
                }
            }
        }
    }

    /// Stops every process in the job.
    pub(in crate::process) fn terminate_job(job: &OwnedHandle) -> io::Result<()> {
        // SAFETY: the job handle is open for the duration of the call.
        if unsafe { TerminateJobObject(job.as_raw_handle(), KILLED) } == 0 {
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
