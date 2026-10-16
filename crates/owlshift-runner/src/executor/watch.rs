//! A process tree stopped at its deadline by a thread of its own, for the
//! runs whose output is streamed rather than captured: the harness run and
//! the gate's commands. The executor's own git commands are captured by
//! `owlshift_platform::process::run_command` (see `git.rs`).
//!
//! A harness run's driver may also stop its tree before the deadline, with
//! the watchdog's [`RunStop`] (OWL-319).

use std::io;
use std::process::{Child, Command};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use owlshift_platform::process::ProcessTree;

/// Watches a process tree and stops it when the deadline passes.
pub(crate) struct Watchdog {
    cancel: Option<Sender<()>>,
    fired: Arc<AtomicBool>,
    /// Shared with the [`RunStop`]s this watch hands out.
    stopped: Arc<AtomicBool>,
    tree: Arc<ProcessTree>,
    thread: Option<JoinHandle<()>>,
}

/// Stops a run's whole process tree from its harness's driver, before the
/// deadline: on Unix its process group, on Windows its Job Object. A run
/// stopped so is judged by its harness's own end, never as timed out, even
/// when the deadline passed in the same moment (OWL-319).
#[derive(Debug)]
pub struct RunStop {
    tree: Arc<ProcessTree>,
    stopped: Arc<AtomicBool>,
}

impl RunStop {
    /// Stops every process of the run's tree, without waiting for them to
    /// end. Best effort: should the stop fail, the deadline still stops the
    /// tree, and the executor stops it again once the run is driven.
    pub fn stop(&self) {
        self.stopped.store(true, Ordering::SeqCst);
        let _ = self.tree.kill();
    }
}

/// Spawns `command` as the root of a process tree, and starts a watchdog
/// that stops the whole tree `timeout` from now.
pub(crate) fn spawn(
    command: &mut Command,
    timeout: Duration,
) -> io::Result<(Child, Arc<ProcessTree>, Watchdog)> {
    let (child, tree) = ProcessTree::spawn(command)?;
    let tree = Arc::new(tree);
    let (cancel, cancelled) = mpsc::channel::<()>();
    let fired = Arc::new(AtomicBool::new(false));
    let thread = {
        let tree = Arc::clone(&tree);
        let fired = Arc::clone(&fired);
        thread::spawn(move || {
            // The sender is dropped when the watch ends: that is the only
            // other way out of the wait.
            if let Err(RecvTimeoutError::Timeout) = cancelled.recv_timeout(timeout) {
                fired.store(true, Ordering::SeqCst);
                // Best effort; the caller stops the tree again at the end.
                let _ = tree.kill();
            }
        })
    };
    let watchdog = Watchdog {
        cancel: Some(cancel),
        fired,
        stopped: Arc::new(AtomicBool::new(false)),
        tree: Arc::clone(&tree),
        thread: Some(thread),
    };
    Ok((child, tree, watchdog))
}

impl Watchdog {
    /// A handle that stops the watched tree before the deadline.
    pub(crate) fn run_stop(&self) -> RunStop {
        RunStop {
            tree: Arc::clone(&self.tree),
            stopped: Arc::clone(&self.stopped),
        }
    }

    /// Ends the watch; whether the deadline passed and the tree was stopped
    /// for it. A run its driver stopped with [`Watchdog::run_stop`]'s handle
    /// did not time out, whichever stop came first: the driver stops a run
    /// for a reason of its own, such as a Codex critic's call to a
    /// collaboration tool, which says more than the deadline (OWL-319).
    pub(crate) fn finish(mut self) -> bool {
        self.cancel.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        self.fired.load(Ordering::SeqCst) && !self.stopped.load(Ordering::SeqCst)
    }
}

impl Drop for Watchdog {
    /// A watch dropped without [`Watchdog::finish`] ends too, without
    /// waiting for its thread.
    fn drop(&mut self) {
        self.cancel.take();
    }
}
