//! Whether `owlshift watch` runs on a project (OWL-420, architecture
//! section 5, "A restart handed to watch"): watch, plain or `--drain`,
//! holds a shared lock on `watch.lock` in the project's data directory for
//! as long as it runs ([`hold`]), and `owlshift continue --to-watch` tries
//! once to take it exclusively ([`watching`]), which fails while a watch
//! holds it. The kernel drops a dead watch's lock (`flock` on Unix,
//! `LockFileEx` on Windows), and the file is opened close-on-exec, as every
//! file the standard library opens, so a step's child never inherits it.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io;
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use crate::project::ProjectDirs;

/// How many times watch tries to take its shared lock: a probe holds the
/// lock exclusively for a moment.
const HOLD_TRIES: u32 = 5;

/// The pause between two of those tries.
const HOLD_PAUSE: Duration = Duration::from_millis(50);

/// A watch's hold on its project, kept for as long as the watch runs.
#[derive(Debug)]
pub struct Presence {
    _file: File,
}

/// The lock file of `dirs`'s project.
fn path(dirs: &ProjectDirs) -> PathBuf {
    dirs.root().join("watch.lock")
}

/// Takes the shared lock that tells a running watch, creating the project's
/// data directory and the file when needed: `None` when a probe kept it
/// through every try, for the caller to warn of.
pub fn hold(dirs: &ProjectDirs) -> io::Result<Option<Presence>> {
    let path = path(dirs);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)?;
    for attempt in 0..HOLD_TRIES {
        match file.try_lock_shared() {
            Ok(()) => return Ok(Some(Presence { _file: file })),
            Err(TryLockError::WouldBlock) => {
                if attempt + 1 < HOLD_TRIES {
                    thread::sleep(HOLD_PAUSE);
                }
            }
            Err(TryLockError::Error(error)) => return Err(error),
        }
    }
    Ok(None)
}

/// Whether a watch holds `dirs`'s project now: a single exclusive try,
/// released at once. A project no watch ever ran on has no file.
pub fn watching(dirs: &ProjectDirs) -> io::Result<bool> {
    let file = match OpenOptions::new().write(true).open(path(dirs)) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    match file.try_lock() {
        Ok(()) => Ok(false),
        Err(TryLockError::WouldBlock) => Ok(true),
        Err(TryLockError::Error(error)) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use owlshift_adapters::forge::Repo;

    use super::*;

    fn dirs(data: &std::path::Path) -> ProjectDirs {
        ProjectDirs::new(data, &Repo::parse("acme/widgets").unwrap())
    }

    #[test]
    fn a_watch_is_seen_while_it_holds_its_project_and_not_after() {
        let data = tempfile::tempdir().unwrap();
        let dirs = dirs(data.path());
        assert!(!watching(&dirs).unwrap(), "no file yet");
        let first = hold(&dirs).unwrap().expect("the first watch holds");
        // A second watch of the same project shares it.
        let second = hold(&dirs).unwrap().expect("a second watch holds too");
        assert!(watching(&dirs).unwrap());
        drop(first);
        assert!(watching(&dirs).unwrap(), "the second still holds");
        drop(second);
        assert!(!watching(&dirs).unwrap(), "the file stays, unheld");
    }
}
