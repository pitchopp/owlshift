//! Stopping every live process tree when the process ends: when it is told
//! to end ([`stop_trees_on_signal`]), and on Unix when it is killed outright
//! (`stop_trees_when_killed`, through the sentinel).
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
//! Since OWL-161, a process can stop by degrees instead ([`stop_by_degrees`],
//! architecture section 5, "Three ways to stop"): a drain, then a checked
//! stop, then the policy above as the last degree. Each tree says what a
//! checked stop does to it ([`AtCheckedStop`]).
//!
//! [`ProcessTree::spawn`]: super::ProcessTree::spawn

use std::io;
use std::process::Child;
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

/// How strongly a process that stops by degrees ([`stop_by_degrees`]) is
/// asked to stop (OWL-161). The last degree, now, is never seen: the
/// process ends at once, as [`stop_trees_on_signal`] ends it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Degree {
    /// End once the work under way is done, starting nothing new.
    Drain,
    /// Stop the runs under way, check them, then end.
    Checked,
}

/// What a checked stop does to a process tree (OWL-161).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AtCheckedStop {
    /// Nothing: the tree is left to end, as the process's own short
    /// commands are, which the checks after a stop need.
    Left,
    /// Every process of the tree is stopped at once, as
    /// [`ProcessTree::kill`] stops them: a run. One started after the
    /// checked stop is stopped as it starts.
    ///
    /// [`ProcessTree::kill`]: super::ProcessTree::kill
    Stopped,
    /// The tree's root, not its group, is sent SIGTERM, so that it stops its
    /// own runs: a step's child. None starts once any stop is asked.
    Told,
}

/// Makes the process stop by degrees (OWL-161, architecture section 5,
/// "Three ways to stop"): on Unix each SIGINT (Ctrl-C) asks one degree more
/// than the one asked, a drain, then a checked stop, then now; SIGTERM and
/// SIGHUP ask a checked stop unless one is asked; SIGQUIT asks now. Now is
/// [`stop_trees_on_signal`]'s policy, which it installs first if needed.
///
/// A drain only records the degree, for [`asked`] and [`wait_asked`]. A
/// checked stop records it too, then, under the lock of the live trees,
/// stops every tree spawned [`AtCheckedStop::Stopped`] and sends SIGTERM to
/// the root of every tree spawned [`AtCheckedStop::Told`]; with a `bound`,
/// the process is stopped now once it has passed since the checked stop.
///
/// On Windows it changes nothing: every console Ctrl-C keeps stopping every
/// tree at once, and nothing is ever asked. `owlshift watch`, its one user,
/// is refused there before any step (D9).
pub fn stop_by_degrees(bound: Option<Duration>) -> io::Result<()> {
    stop_trees_on_signal()?;
    #[cfg(unix)]
    {
        degrees().policy = Policy::Graded { bound };
    }
    #[cfg(windows)]
    let _ = bound;
    Ok(())
}

/// The degree asked of a process that stops by degrees, if any.
pub fn asked() -> Option<Degree> {
    degrees().asked
}

/// Waits until a degree is asked, or `timeout` has passed: the degree asked
/// then, if any.
pub fn wait_asked(timeout: Duration) -> Option<Degree> {
    let guard = degrees();
    let (guard, _) = ASKED
        .wait_timeout_while(guard, timeout, |degrees| degrees.asked.is_none())
        .unwrap_or_else(PoisonError::into_inner);
    guard.asked
}

/// What the signals ask, and what has been asked.
struct Degrees {
    #[cfg(unix)]
    policy: Policy,
    asked: Option<Degree>,
}

/// What a signal does.
#[cfg(unix)]
#[derive(Clone, Copy)]
enum Policy {
    /// [`stop_trees_on_signal`]'s: every one stops every tree at once.
    Now,
    /// [`stop_by_degrees`]'s, now past `bound` after a checked stop.
    Graded { bound: Option<Duration> },
}

/// Taken after the lock of the live trees, never before it.
static DEGREES: Mutex<Degrees> = Mutex::new(Degrees {
    #[cfg(unix)]
    policy: Policy::Now,
    asked: None,
});

/// Told of every degree asked.
static ASKED: Condvar = Condvar::new();

fn degrees() -> MutexGuard<'static, Degrees> {
    DEGREES.lock().unwrap_or_else(PoisonError::into_inner)
}

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

/// Makes every live [`ProcessTree`] stop when the process ends however it
/// ends, where no handler runs: killed outright (SIGKILL), crashed, or ended
/// by the system for want of memory. A tree still live at a normal exit is
/// stopped too.
///
/// It starts the sentinel, a `/bin/sh` loop in a process group of its own,
/// told of each tree as it becomes live and as its handle is dropped,
/// through a pipe only this process holds. When this process ends, the
/// system closes the pipe, and the sentinel kills the process group of each
/// tree still live, as [`ProcessTree::kill`] does, then ends. A tree whose
/// handle was dropped is left running, as dropping it promises, and so is a
/// tree the handler of [`stop_trees_on_signal`] already stopped.
///
/// It is a best effort, not a guarantee:
///
/// - a kill that lands between a tree's start and the moment the sentinel is
///   told of it, well under a millisecond, leaves that tree running;
/// - once the sentinel is killed it protects nothing more, and it is not
///   restarted; [`sentinel_status`] tells whether it still runs, or is
///   stopped, without waiting on it;
/// - a sentinel that is stopped reads nothing, but when this process ends
///   the system continues it, and it stops the live trees then, unless its
///   input filled first: a line to it is lost then, so this process kills it
///   rather than let it act on a list that misses one; and one stopped in
///   its first milliseconds, before its script ignores SIGHUP, dies of the
///   SIGHUP that comes with being continued;
/// - as with [`ProcessTree::kill`], a process that left its tree's group is
///   not stopped, and a group id could in theory belong to another group by
///   then, once the pid space wrapped around.
///
/// Call it first thing in `main`, before another thread starts a process: on
/// a system that has no `pipe2`, as macOS, a pipe is made close-on-exec just
/// after it is created, so a process started at that moment could inherit
/// the sentinel's pipe and hide this process's end from it. Trees live
/// before the call are covered too. Once it succeeded, later calls do
/// nothing; after a failure, it can be called again.
///
/// Unix only: native Windows is out of scope for this guarantee (decided on
/// 2026-09-29, see `docs/design/runtime-and-operations.md`).
///
/// [`ProcessTree`]: super::ProcessTree
/// [`ProcessTree::kill`]: super::ProcessTree::kill
#[cfg(unix)]
pub fn stop_trees_when_killed() -> io::Result<()> {
    let mut guard = live();
    let live = &mut *guard;
    if live.sentinel.is_some() {
        return Ok(());
    }
    // Under the lock, so no tree starts meanwhile.
    let mut sentinel = super::sentinel::Sentinel::start()?;
    for &(_, group, _) in &live.trees {
        sentinel.announce(group);
    }
    live.sentinel = Some(sentinel);
    Ok(())
}

/// Whether the sentinel [`stop_trees_when_killed`] started still runs, so
/// that a command can say when the protection is gone (OWL-88), or is
/// stopped (OWL-91). It never waits: a sentinel that ended is reaped here.
#[cfg(unix)]
pub fn sentinel_status() -> super::SentinelStatus {
    match &mut live().sentinel {
        Some(sentinel) => sentinel.status(),
        None => super::SentinelStatus::NotRunning,
    }
}

/// Acts on the sentinel [`stop_trees_when_killed`] started, if any, under
/// the lock of the live trees.
#[cfg(all(test, unix))]
pub(super) fn with_sentinel<R>(act: impl FnOnce(&mut super::sentinel::Sentinel) -> R) -> Option<R> {
    live().sentinel.as_mut().map(act)
}

/// What stops a live tree: its process group.
#[cfg(unix)]
pub(super) type Stopper = rustix::process::Pid;

/// What stops a live tree: its Job Object, shared with the tree's handle so
/// that it stays open while registered.
#[cfg(windows)]
pub(super) type Stopper = std::sync::Arc<std::os::windows::io::OwnedHandle>;

/// The live trees, each under the id of its [`Registration`], with what a
/// checked stop does to it.
struct Live {
    next: u64,
    trees: Vec<(u64, Stopper, AtCheckedStop)>,
    /// The sentinel, once [`stop_trees_when_killed`] started it, told of
    /// every change to `trees` under this lock.
    #[cfg(unix)]
    sentinel: Option<super::sentinel::Sentinel>,
}

static LIVE: Mutex<Live> = Mutex::new(Live {
    next: 0,
    trees: Vec::new(),
    #[cfg(unix)]
    sentinel: None,
});

/// The live trees. A panic cannot leave them half-updated, so a poisoned
/// lock is used as is: the handler must still find every tree.
fn live() -> MutexGuard<'static, Live> {
    LIVE.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Runs `act` while no process tree can start: [`register`] holds the same
/// lock across its spawn. For a descriptor that must not be inherited by a
/// process forked meanwhile, on a system that has no `pipe2` (OWL-96).
#[cfg(unix)]
pub(super) fn while_no_tree_starts<R>(act: impl FnOnce() -> R) -> R {
    let _guard = live();
    act()
}

/// A live tree's entry; dropping it removes the entry.
#[derive(Debug)]
pub(super) struct Registration {
    id: u64,
}

impl Drop for Registration {
    fn drop(&mut self) {
        let mut guard = live();
        let live = &mut *guard;
        #[cfg(unix)]
        if let Some(sentinel) = &mut live.sentinel
            && let Some(&(_, group, _)) = live.trees.iter().find(|(id, ..)| *id == self.id)
        {
            sentinel.withdraw(group);
        }
        live.trees.retain(|(id, ..)| *id != self.id);
    }
}

/// Spawns a tree's root with `spawn` and registers what stops the tree,
/// taken from the new child by `stopper`, and what a checked stop does to it,
/// `at`, under the lock the handler takes.
///
/// Holding the lock across the spawn means an event is handled either before
/// (the process ends and the spawn never happens) or after the tree is
/// registered, never in between: on Windows, never while a child created
/// suspended is outside any registered job. The cost is that trees are
/// spawned one at a time across threads: on Unix for the few milliseconds a
/// spawn takes, on Windows also while the new child is placed in its job and
/// its thread is found in a snapshot of the system's threads and resumed.
///
/// For the same reason a stop asked by degrees is never missed (OWL-161): a
/// tree [`AtCheckedStop::Told`] is not spawned once any stop is asked, and
/// one [`AtCheckedStop::Stopped`] spawned after a checked stop is stopped
/// at once.
pub(super) fn register(
    spawn: impl FnOnce() -> io::Result<Child>,
    stopper: impl FnOnce(&Child) -> Stopper,
    at: AtCheckedStop,
) -> io::Result<(Child, Registration)> {
    let mut guard = live();
    let asked = asked();
    if at == AtCheckedStop::Told && asked.is_some() {
        return Err(io::Error::other(
            "this process is asked to stop: no step starts",
        ));
    }
    let child = spawn()?;
    let live = &mut *guard;
    let id = live.next;
    live.next += 1;
    let stopper = stopper(&child);
    #[cfg(unix)]
    if let Some(sentinel) = &mut live.sentinel {
        sentinel.announce(stopper);
    }
    #[cfg(unix)]
    if at == AtCheckedStop::Stopped && asked == Some(Degree::Checked) {
        // Best effort, as at the checked stop itself.
        let _ = super::tree::kill_group(stopper);
    }
    live.trees.push((id, stopper, at));
    Ok((child, Registration { id }))
}

/// Acts on the live trees at a checked stop (OWL-161): stops those
/// [`AtCheckedStop::Stopped`], each best effort, and sends SIGTERM to the
/// root of those [`AtCheckedStop::Told`]. Their entries stay: their handles
/// withdraw them as they are dropped, the process going on.
#[cfg(unix)]
fn act_on_trees(live: &Live) {
    use rustix::process::{Signal, kill_process};

    for &(_, stopper, at) in &live.trees {
        match at {
            AtCheckedStop::Left => {}
            AtCheckedStop::Stopped => {
                let _ = super::tree::kill_group(stopper);
            }
            // The root's pid is its group's id, and stays its own while
            // the tree is registered: it is reaped only once withdrawn.
            AtCheckedStop::Told => {
                let _ = kill_process(stopper, Signal::TERM);
            }
        }
    }
}

/// Stops every live tree, then returns the lock, which the caller keeps
/// until the process is gone: no tree starts from now on, and a thread that
/// sees its tree stopped blocks when it drops the tree's handle, so it cannot
/// end the process first.
fn stop_every_tree() -> MutexGuard<'static, Live> {
    let mut guard = live();
    let live = &mut *guard;
    for (_, stopper, _) in &live.trees {
        // Best effort: a tree that cannot be stopped must not spare the
        // ones after it.
        #[cfg(unix)]
        if super::tree::kill_group(*stopper).is_ok()
            && let Some(sentinel) = &mut live.sentinel
        {
            // Stopped: once this process is gone, its group id may be
            // another group's, which the sentinel must not kill.
            sentinel.withdraw(*stopper);
        }
        #[cfg(windows)]
        let _ = super::tree::terminate_job(stopper);
    }
    guard
}

#[cfg(unix)]
mod imp {
    use std::io;
    use std::sync::mpsc;
    use std::thread;

    use signal_hook::consts::{SIGHUP, SIGINT, SIGQUIT, SIGTERM};
    use signal_hook::iterator::Signals;
    use signal_hook::low_level::emulate_default_handler;

    use super::{ASKED, Degree, Policy};

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
                        for signal in signals.forever() {
                            on_signal(signal);
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

    /// What `signal` asks of a process that stops by degrees, which has
    /// been asked `asked` (OWL-161): a degree, nothing more, or now (`Err`).
    pub(super) fn next(signal: i32, asked: Option<Degree>) -> Result<Option<Degree>, ()> {
        match (signal, asked) {
            (SIGINT, None) => Ok(Some(Degree::Drain)),
            (SIGINT, Some(Degree::Drain)) => Ok(Some(Degree::Checked)),
            (SIGTERM | SIGHUP, Some(Degree::Checked)) => Ok(None),
            (SIGTERM | SIGHUP, _) => Ok(Some(Degree::Checked)),
            _ => Err(()),
        }
    }

    /// Ends the process, or records the degree `signal` asks and acts on it.
    fn on_signal(signal: i32) {
        let live = super::live();
        let mut degrees = super::degrees();
        let Policy::Graded { bound } = degrees.policy else {
            drop((degrees, live));
            end(signal);
        };
        let degree = match next(signal, degrees.asked) {
            Ok(Some(degree)) => degree,
            Ok(None) => return,
            Err(()) => {
                drop((degrees, live));
                end(signal);
            }
        };
        degrees.asked = Some(degree);
        ASKED.notify_all();
        drop(degrees);
        if degree == Degree::Checked {
            // Recorded first, so a run that ends because its tree was
            // stopped finds the stop asked.
            super::act_on_trees(&live);
            drop(live);
            if let Some(bound) = bound {
                let _ = thread::Builder::new()
                    .name("owlshift-stop-bound".to_owned())
                    .spawn(move || {
                        thread::sleep(bound);
                        end(SIGTERM);
                    });
            }
        }
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
        live().trees.iter().any(|(entry, ..)| *entry == id)
    }

    /// OWL-161: what each signal asks, by what was asked before; `Err` is
    /// now.
    #[cfg(unix)]
    #[test]
    fn each_signal_asks_its_degree() {
        use signal_hook::consts::{SIGHUP, SIGINT, SIGQUIT, SIGTERM};

        let (drain, checked) = (Some(Degree::Drain), Some(Degree::Checked));
        for (signal, asked, next) in [
            (SIGINT, None, Ok(drain)),
            (SIGINT, drain, Ok(checked)),
            (SIGINT, checked, Err(())),
            (SIGTERM, None, Ok(checked)),
            (SIGTERM, drain, Ok(checked)),
            (SIGTERM, checked, Ok(None)),
            (SIGHUP, drain, Ok(checked)),
            (SIGHUP, checked, Ok(None)),
            (SIGQUIT, None, Err(())),
            (SIGQUIT, drain, Err(())),
        ] {
            assert_eq!(imp::next(signal, asked), next, "{signal} after {asked:?}");
        }
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
