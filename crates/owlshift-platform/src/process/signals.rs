//! Stopping every live process tree when the process is told to end.
//!
//! On Unix a tree's root runs in a process group of its own, so the signals a
//! terminal sends to its foreground group (Ctrl-C, Ctrl-\, a closed
//! terminal) reach Owlshift but not the trees it started, which would keep
//! running with nobody left to stop them. On Windows a tree's root shares
//! Owlshift's console and gets its Ctrl-C too, but a process that ignores
//! Ctrl-C, and its children, keep running once Owlshift has ended (checked
//! live, see `docs/design/runtime-and-operations.md`). [`stop_trees_on_signal`]
//! makes Owlshift stop them first.
//!
//! A tree is live from [`ProcessTree::spawn`] until its handle is dropped:
//! every tree registers what stops it here (its process group, its Job
//! Object), under the lock the handler takes, so an event handled at any
//! moment finds either no process or a registered tree.
//!
//! [`ProcessTree::spawn`]: super::ProcessTree::spawn

use std::io;
use std::process::Child;
use std::sync::{Mutex, MutexGuard, PoisonError};

/// Makes the process stop every live [`ProcessTree`] before it ends, when
/// told to end: on Unix on SIGINT (Ctrl-C), SIGQUIT (Ctrl-\), SIGTERM and
/// SIGHUP (a closed terminal), on Windows on Ctrl-C and Ctrl-Break from the
/// console. The process then ends as the event's default action ends it: a
/// shell sees it killed by the signal, or ended by Ctrl-C
/// (`STATUS_CONTROL_C_EXIT`).
///
/// The trees are stopped as [`ProcessTree::kill`] stops them, each one best
/// effort. On Unix the work runs on a thread of its own, started here: the
/// signal handler only wakes it, so it may take locks. On Windows the system
/// runs the console handler on a thread of its own already.
///
/// Call it early, before the first tree is spawned. Once it succeeded, later
/// calls do nothing; after a failure, it can be called again.
///
/// It installs one process-wide policy, fit for a command that should end
/// on these events. Every tree is registered whether or not it is called.
///
/// On Windows a process started with Ctrl-C turned off (`start /b`, for one)
/// keeps it off, and so do the trees it starts: Ctrl-C then ends neither,
/// Ctrl-Break still ends both.
///
/// [`ProcessTree`]: super::ProcessTree
/// [`ProcessTree::kill`]: super::ProcessTree::kill
pub fn stop_trees_on_signal() -> io::Result<()> {
    let mut installed = INSTALLED.lock().unwrap_or_else(PoisonError::into_inner);
    if *installed {
        return Ok(());
    }
    imp::install()?;
    *installed = true;
    Ok(())
}

/// Whether the policy is installed.
static INSTALLED: Mutex<bool> = Mutex::new(false);

/// What stops a live tree: its process group.
#[cfg(unix)]
pub(super) type Stopper = rustix::process::Pid;

/// What stops a live tree: its Job Object, shared with the tree's handle so
/// that it stays open while registered.
#[cfg(windows)]
pub(super) type Stopper = std::sync::Arc<std::os::windows::io::OwnedHandle>;

/// The live trees, each under the id of its [`Registration`].
struct Live {
    next: u64,
    trees: Vec<(u64, Stopper)>,
}

static LIVE: Mutex<Live> = Mutex::new(Live {
    next: 0,
    trees: Vec::new(),
});

/// The live trees. A panic cannot leave them half-updated, so a poisoned
/// lock is used as is: the handler must still find every tree.
fn live() -> MutexGuard<'static, Live> {
    LIVE.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A live tree's entry; dropping it removes the entry.
#[derive(Debug)]
pub(super) struct Registration {
    id: u64,
}

impl Drop for Registration {
    fn drop(&mut self) {
        live().trees.retain(|(id, _)| *id != self.id);
    }
}

/// Spawns a tree's root with `spawn` and registers what stops the tree,
/// taken from the new child by `stopper`, under the lock the handler takes.
///
/// Holding the lock across the spawn means an event is handled either before
/// (the process ends and the spawn never happens) or after the tree is
/// registered, never in between: on Windows, never while a child created
/// suspended is outside any registered job. The cost is that trees are
/// spawned one at a time across threads: on Unix for the few milliseconds a
/// spawn takes, on Windows also while the new child is placed in its job and
/// its thread is found in a snapshot of the system's threads and resumed.
pub(super) fn register(
    spawn: impl FnOnce() -> io::Result<Child>,
    stopper: impl FnOnce(&Child) -> Stopper,
) -> io::Result<(Child, Registration)> {
    let mut live = live();
    let child = spawn()?;
    let id = live.next;
    live.next += 1;
    live.trees.push((id, stopper(&child)));
    Ok((child, Registration { id }))
}

/// Stops every live tree, then returns the lock, which the caller keeps
/// until the process is gone: no tree starts from now on, and a thread that
/// sees its tree stopped blocks when it drops the tree's handle, so it cannot
/// end the process first.
fn stop_every_tree() -> MutexGuard<'static, Live> {
    let live = live();
    for (_, stopper) in &live.trees {
        // Best effort: a tree that cannot be stopped must not spare the
        // ones after it.
        #[cfg(unix)]
        let _ = super::tree::kill_group(*stopper);
        #[cfg(windows)]
        let _ = super::tree::terminate_job(stopper);
    }
    live
}

#[cfg(unix)]
mod imp {
    use std::io;
    use std::sync::mpsc;
    use std::thread;

    use signal_hook::consts::{SIGHUP, SIGINT, SIGQUIT, SIGTERM};
    use signal_hook::iterator::Signals;
    use signal_hook::low_level::emulate_default_handler;

    pub(super) fn install() -> io::Result<()> {
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
            .map_err(|_| io::Error::other("the signal thread ended before it was ready"))?
    }

    /// Stops every live tree, then ends the process as `signal` would have.
    fn end(signal: i32) -> ! {
        let _live = super::stop_every_tree();
        let _ = emulate_default_handler(signal);
        // Not reached: the default action of these signals ends the
        // process.
        std::process::exit(128 + signal)
    }
}

#[cfg(windows)]
mod imp {
    use std::io;

    use windows_sys::Win32::Foundation::STATUS_CONTROL_C_EXIT;
    use windows_sys::Win32::System::Console::{
        CTRL_BREAK_EVENT, CTRL_C_EVENT, SetConsoleCtrlHandler,
    };
    use windows_sys::Win32::System::Threading::ExitProcess;
    use windows_sys::core::BOOL;

    pub(super) fn install() -> io::Result<()> {
        // SAFETY: `on_console_event` has the signature of a handler routine
        // and lives as long as the process.
        if unsafe { SetConsoleCtrlHandler(Some(on_console_event), 1) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Runs on a thread the system starts for each console event.
    ///
    /// A closed console is left to the next handler: the system ends every
    /// process attached to the console then, the trees' included.
    unsafe extern "system" fn on_console_event(event: u32) -> BOOL {
        match event {
            CTRL_C_EVENT | CTRL_BREAK_EVENT => end(),
            _ => 0,
        }
    }

    /// Stops every live tree, then ends the process as the default handler
    /// does for a console event; like it, without flushing what Rust still
    /// holds of the standard output.
    fn end() -> ! {
        let _live = super::stop_every_tree();
        // SAFETY: no pointer argument.
        unsafe { ExitProcess(STATUS_CONTROL_C_EXIT as u32) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::ProcessTree;
    use std::process::Command;

    fn registered(id: u64) -> bool {
        live().trees.iter().any(|(entry, _)| *entry == id)
    }

    /// A program that exits at once.
    fn quick() -> Command {
        if cfg!(windows) {
            let mut command = Command::new("cmd");
            command.args(["/C", "exit"]);
            command
        } else {
            Command::new("true")
        }
    }

    #[test]
    fn a_tree_is_registered_until_its_handle_is_dropped() {
        let (mut child, tree) = ProcessTree::spawn(&mut quick()).unwrap();
        let id = tree.registration().id;
        assert!(registered(id));
        child.wait().unwrap();
        // Reaped, yet still the caller's: registered until dropped.
        assert!(registered(id));
        drop(tree);
        assert!(!registered(id));
    }
}
