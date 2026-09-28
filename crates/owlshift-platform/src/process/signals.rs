//! Stopping every live process tree when the process is told to end.
//!
//! A tree's root runs in a process group of its own, so the signals a
//! terminal sends to its foreground group (Ctrl-C, Ctrl-\, a closed
//! terminal) reach Owlshift but not the trees it started, which would keep
//! running with nobody left to stop them. [`stop_trees_on_signal`] makes
//! Owlshift stop them first.
//!
//! A tree is live from [`ProcessTree::spawn`] until its handle is dropped:
//! every tree registers its process group here, under the lock the signal
//! thread takes, so a signal handled at any moment finds either no process or
//! a registered group.
//!
//! On Windows there is nothing to forward. A tree's root is created without
//! `CREATE_NEW_PROCESS_GROUP` and without a console of its own, so it shares
//! Owlshift's console and console process group, and gets the console's
//! Ctrl-C itself like every process attached to that console; the Job Object
//! plays no part in console events. This follows the Win32 documentation of
//! the process creation flags and of `GenerateConsoleCtrlEvent`; it was not
//! checked live (no Windows host, 2026-09-28). A process that ignores Ctrl-C
//! keeps running there, as it did before trees had a job of their own.
//!
//! [`ProcessTree::spawn`]: super::ProcessTree::spawn

use std::io;

/// Makes SIGINT (Ctrl-C), SIGQUIT (Ctrl-\), SIGTERM and SIGHUP (a closed
/// terminal) stop every live [`ProcessTree`] before the process ends, as the
/// signal's default action ends it: a shell sees the process killed by that
/// signal.
///
/// The trees are stopped as [`ProcessTree::kill`] stops them, each one best
/// effort. The work runs on a thread of its own, started here: the signal
/// handler only wakes it, so it may take locks.
///
/// Call it early, before the first tree is spawned. Once it succeeded, later
/// calls do nothing; after a failure, it can be called again. On Windows it
/// does nothing and succeeds (see the module documentation).
///
/// It installs one process-wide policy, fit for a command that should end
/// on these signals. Every tree is registered whether or not it is called.
///
/// [`ProcessTree`]: super::ProcessTree
/// [`ProcessTree::kill`]: super::ProcessTree::kill
pub fn stop_trees_on_signal() -> io::Result<()> {
    #[cfg(unix)]
    {
        unix::install()
    }
    #[cfg(windows)]
    {
        Ok(())
    }
}

#[cfg(unix)]
pub(super) use unix::{Registration, register};

#[cfg(unix)]
mod unix {
    use std::io;
    use std::process::Child;
    use std::sync::mpsc;
    use std::sync::{Mutex, MutexGuard, PoisonError};
    use std::thread;

    use rustix::process::Pid;
    use signal_hook::consts::{SIGHUP, SIGINT, SIGQUIT, SIGTERM};
    use signal_hook::iterator::Signals;
    use signal_hook::low_level::emulate_default_handler;

    use super::super::tree::kill_group;

    /// The process groups of the live trees, each under the id of its
    /// [`Registration`].
    struct Live {
        next: u64,
        groups: Vec<(u64, Pid)>,
    }

    static LIVE: Mutex<Live> = Mutex::new(Live {
        next: 0,
        groups: Vec::new(),
    });

    /// Whether the signal thread is watching.
    static INSTALLED: Mutex<bool> = Mutex::new(false);

    /// The live trees. A panic cannot leave them half-updated, so a poisoned
    /// lock is used as is: the signal thread must still find every group.
    fn live() -> MutexGuard<'static, Live> {
        LIVE.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A live tree's entry; dropping it removes the entry.
    #[derive(Debug)]
    pub(in crate::process) struct Registration {
        id: u64,
    }

    impl Drop for Registration {
        fn drop(&mut self) {
            live().groups.retain(|&(id, _)| id != self.id);
        }
    }

    /// Spawns a tree's root with `spawn` and registers its process group,
    /// whose id is the root's pid, under the lock the signal thread takes.
    ///
    /// Holding the lock across the spawn means a signal is handled either
    /// before (the process ends and the spawn never happens) or after the
    /// group is registered, never in between. The cost is that trees are
    /// spawned one at a time across threads, for the few milliseconds a spawn
    /// takes.
    pub(in crate::process) fn register(
        spawn: impl FnOnce() -> io::Result<Child>,
    ) -> io::Result<(Child, Pid, Registration)> {
        let mut live = live();
        let child = spawn()?;
        let group = Pid::from_child(&child);
        let id = live.next;
        live.next += 1;
        live.groups.push((id, group));
        Ok((child, group, Registration { id }))
    }

    pub(super) fn install() -> io::Result<()> {
        let mut installed = INSTALLED.lock().unwrap_or_else(PoisonError::into_inner);
        if *installed {
            return Ok(());
        }
        // The signals are registered on the thread that waits for them, once
        // it runs: registered and then dropped, they would be ignored from
        // then on (signal-hook-registry does not restore the default action).
        let (ready, registered) = mpsc::channel();
        thread::Builder::new()
            .name("owlshift-signals".to_owned())
            .spawn(
                move || match Signals::new([SIGINT, SIGQUIT, SIGTERM, SIGHUP]) {
                    Ok(mut signals) => {
                        let _ = ready.send(Ok(()));
                        if let Some(signal) = signals.forever().next() {
                            end(signal);
                        }
                    }
                    Err(error) => {
                        let _ = ready.send(Err(error));
                    }
                },
            )?;
        registered
            .recv()
            .map_err(|_| io::Error::other("the signal thread ended before it was ready"))??;
        *installed = true;
        Ok(())
    }

    /// Stops every live tree, then ends the process as `signal` would have.
    fn end(signal: i32) -> ! {
        // Kept locked until the process is gone: no tree starts from now on.
        let live = live();
        for &(_, group) in &live.groups {
            // Best effort: a group that cannot be stopped must not spare
            // the ones after it.
            let _ = kill_group(group);
        }
        let _ = emulate_default_handler(signal);
        // Not reached: the default action of these signals ends the
        // process.
        std::process::exit(128 + signal)
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::process::ProcessTree;
        use std::process::Command;

        fn registered(group: Pid) -> bool {
            live().groups.iter().any(|&(_, g)| g == group)
        }

        #[test]
        fn a_tree_is_registered_until_its_handle_is_dropped() {
            let (mut child, tree) = ProcessTree::spawn(&mut Command::new("true")).unwrap();
            let group = Pid::from_child(&child);
            assert!(registered(group));
            child.wait().unwrap();
            // Reaped, yet still the caller's: registered until dropped.
            assert!(registered(group));
            drop(tree);
            assert!(!registered(group));
        }
    }
}
