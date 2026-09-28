//! A process tree stopped at its deadline by a thread of its own, for the
//! runs whose output is streamed rather than captured: the harness run and
//! the gate's commands. The executor's own git commands are captured by
//! `owlshift_platform::process::run_command` (see `git.rs`).

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
    thread: Option<JoinHandle<()>>,
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
        thread: Some(thread),
    };
    Ok((child, tree, watchdog))
}

impl Watchdog {
    /// Ends the watch; whether the deadline passed and the tree was stopped.
    pub(crate) fn finish(mut self) -> bool {
        self.cancel.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        self.fired.load(Ordering::SeqCst)
    }
}

impl Drop for Watchdog {
    /// A watch dropped without [`Watchdog::finish`] ends too, without
    /// waiting for its thread.
    fn drop(&mut self) {
        self.cancel.take();
    }
}
