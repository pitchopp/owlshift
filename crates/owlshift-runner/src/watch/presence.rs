//! Whether `owlshift watch` runs on a project (OWL-420, architecture
//! section 5, "A restart handed to watch"): watch, plain or `--drain`,
//! holds a shared lock on `watch.lock` in the project's data directory for
//! as long as it runs ([`hold`]), and `owlshift continue --to-watch` tries
//! once to take it exclusively ([`watching`]), which fails while a watch
//! holds it. The kernel drops a dead watch's lock (`flock` on Unix,
//! `LockFileEx` on Windows), and the file is opened close-on-exec, as every
//! file the standard library opens, so a step's child never inherits it.
//!
//! Which build each watch runs (OWL-462) is its holder record,
//! `watches/<pid>.json` in the project's folder ([`record`]), written once
//! the watch holds `watches/<pid>.lock` alone and removed at its end, as a
//! run slot's holder record is (`slots.rs`). A record under a free lock is
//! a dead watch's: never read ([`running`]), and removed by the next watch
//! of the project.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use owlshift_contracts::service::WatchRecord;

use crate::project::{ProjectDirs, held_alone, try_lock_file};
use crate::slots::write_record;

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
/// released at once. A project no watch ever ran on has no file. A project
/// the daemon serves counts as watched, its watch running or about to start
/// again (OWL-435, architecture section 5, "The served lock").
pub fn watching(dirs: &ProjectDirs) -> io::Result<bool> {
    if crate::served::served_now(dirs)? {
        return Ok(true);
    }
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

/// A watch's holder record (OWL-462), kept for as long as the watch runs.
#[derive(Debug)]
pub struct Recorded {
    _lock: File,
    lock: PathBuf,
    record: PathBuf,
}

impl Drop for Recorded {
    /// The record goes while the lock is still held, then the lock file.
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.record);
        let _ = fs::remove_file(&self.lock);
    }
}

/// The folder of `dirs`'s watch records.
fn records_dir(dirs: &ProjectDirs) -> PathBuf {
    dirs.root().join("watches")
}

/// The lock and the record of the watch of pid `pid` in `dir`.
fn files(dir: &Path, pid: u32) -> (PathBuf, PathBuf) {
    (
        dir.join(format!("{pid}.lock")),
        dir.join(format!("{pid}.json")),
    )
}

/// The pids whose lock file stands in `dir`; none when `dir` is not there.
fn pids(dir: &Path) -> io::Result<Vec<u32>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut pids = Vec::new();
    for entry in entries {
        let name = entry?.file_name();
        if let Some(pid) = name
            .to_str()
            .and_then(|name| name.strip_suffix(".lock"))
            .and_then(|pid| pid.parse().ok())
        {
            pids.push(pid);
        }
    }
    pids.sort_unstable();
    Ok(pids)
}

/// Records the watch `watch` describes on `dirs`'s project: takes
/// `watches/<pid>.lock` alone and writes `watches/<pid>.json`, both removed
/// when the result is dropped. Dead watches' records, under a free lock, go
/// first. `None` when another process kept the lock through every try.
pub fn record(dirs: &ProjectDirs, watch: &WatchRecord) -> io::Result<Option<Recorded>> {
    let dir = records_dir(dirs);
    fs::create_dir_all(&dir)?;
    for pid in pids(&dir)? {
        let (lock, record) = files(&dir, pid);
        if pid != watch.pid
            && let Ok(Some(_dead)) = try_lock_file(&lock)
        {
            let _ = fs::remove_file(&record);
            let _ = fs::remove_file(&lock);
        }
    }
    let (lock, record) = files(&dir, watch.pid);
    for attempt in 0..HOLD_TRIES {
        if let Some(file) = try_lock_file(&lock)? {
            // Best effort, as a slot's: a watch whose record could not be
            // written is shown as running an unknown build.
            write_record(&record, &watch.render());
            return Ok(Some(Recorded {
                _lock: file,
                lock,
                record,
            }));
        }
        if attempt + 1 < HOLD_TRIES {
            thread::sleep(HOLD_PAUSE);
        }
    }
    Ok(None)
}

/// A watch that runs now, as its record says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunningWatch {
    pub pid: u32,
    /// Its record; `None` when it does not read, or was not written yet.
    pub record: Option<WatchRecord>,
}

/// The watches that run now on every project of the data directory `data`
/// (OWL-462): those whose record's lock is held. Read only: a dead watch's
/// record is skipped, never removed.
pub fn running(data: &Path) -> io::Result<Vec<RunningWatch>> {
    let mut running = Vec::new();
    for dirs in ProjectDirs::all(data)? {
        let dir = records_dir(&dirs);
        for pid in pids(&dir)? {
            let (lock, record) = files(&dir, pid);
            if held_alone(&lock)? {
                running.push(RunningWatch {
                    pid,
                    record: fs::read_to_string(&record)
                        .ok()
                        .and_then(|text| WatchRecord::parse(&text).ok()),
                });
            }
        }
    }
    Ok(running)
}

#[cfg(test)]
mod tests {
    use owlshift_adapters::forge::Repo;

    use super::*;

    fn dirs(data: &std::path::Path) -> ProjectDirs {
        ProjectDirs::new(data, &Repo::parse("acme/widgets").unwrap())
    }

    /// OWL-462: a watch's record is read while it holds its lock, and gone
    /// once it ends; a dead watch's record, under a free lock, is never
    /// read, and the next watch of the project removes it.
    #[test]
    fn a_watch_record_is_read_while_its_watch_runs_and_a_dead_one_never() {
        use owlshift_contracts::format::Format;
        use owlshift_contracts::service::BuildIdentity;

        let data = tempfile::tempdir().unwrap();
        let dirs = dirs(data.path());
        let watch = |pid: u32| WatchRecord {
            format: Format,
            pid,
            project: "acme/widgets".to_owned(),
            served: false,
            since: jiff::Timestamp::UNIX_EPOCH,
            binary: "/bin/owlshift".to_owned(),
            version: "0.0.0".to_owned(),
            build: BuildIdentity::from_build("", "0", "0", ""),
        };
        assert_eq!(running(data.path()).unwrap(), []);
        let held = record(&dirs, &watch(7)).unwrap().expect("recorded");
        let seen = running(data.path()).unwrap();
        assert_eq!(
            seen,
            [RunningWatch {
                pid: 7,
                record: Some(watch(7))
            }]
        );
        drop(held);
        assert_eq!(running(data.path()).unwrap(), []);
        assert!(!records_dir(&dirs).join("7.json").exists());

        // A watch killed outright leaves both files, its lock free.
        let (lock, dead) = files(&records_dir(&dirs), 8);
        fs::write(&lock, "").unwrap();
        fs::write(&dead, watch(8).render()).unwrap();
        assert_eq!(running(data.path()).unwrap(), [], "never read");
        let next = record(&dirs, &watch(9)).unwrap().expect("recorded");
        assert!(!dead.exists(), "swept by the next watch");
        assert_eq!(running(data.path()).unwrap()[0].pid, 9);
        drop(next);
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

    /// OWL-435: a project the daemon serves is watched, between its watch's
    /// restarts too; a watch typed in a terminal shares the served lock,
    /// which does not make the project served, and keeps the daemon off it.
    #[test]
    fn a_served_project_is_watched_and_a_terminal_watch_keeps_the_daemon_off() {
        use crate::served::{served_now, share_served, take_served};

        let data = tempfile::tempdir().unwrap();
        let dirs = dirs(data.path());
        let daemon = take_served(&dirs).unwrap().expect("the daemon serves it");
        assert!(watching(&dirs).unwrap());
        assert!(served_now(&dirs).unwrap());
        assert!(
            share_served(&dirs).unwrap().is_none(),
            "a terminal watch refuses"
        );
        drop(daemon);
        assert!(!watching(&dirs).unwrap());

        let terminal = share_served(&dirs).unwrap().expect("a terminal watch runs");
        assert!(!served_now(&dirs).unwrap());
        assert!(take_served(&dirs).unwrap().is_none(), "the daemon waits");
        drop(terminal);
        assert!(take_served(&dirs).unwrap().is_some());
    }
}
