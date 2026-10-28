//! One daemon per data directory (architecture section 5, "One instance"):
//! `<data>/service/daemon.lock`, held alone for the daemon's whole life, and
//! `<data>/service/daemon.json`, the record of the daemon that holds it.

use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use owlshift_contracts::service::DaemonRecord;

use crate::{project, step};

/// How long a daemon tries the lock: a probe ([`running`]) holds it for an
/// instant.
const TAKE_WAIT: Duration = Duration::from_millis(250);

/// How often that wait tries it.
const TAKE_POLL: Duration = Duration::from_millis(50);

/// The daemon's folder in the data directory `data`: `<data>/service`.
pub fn service_dir(data: &Path) -> PathBuf {
    data.join("service")
}

fn lock_file(data: &Path) -> PathBuf {
    service_dir(data).join("daemon.lock")
}

/// The record of the daemon that runs on `data`.
pub fn record_file(data: &Path) -> PathBuf {
    service_dir(data).join("daemon.json")
}

/// This process as the daemon of its data directory: holds the lock, and
/// removes its record when dropped.
#[derive(Debug)]
pub struct Instance {
    _lock: File,
    record: PathBuf,
    pid: u32,
}

/// Another daemon holds the data directory's lock: its record, when it reads.
#[derive(Debug)]
pub struct Held(pub Option<DaemonRecord>);

/// Makes this process the daemon of `data`, recorded as `record` says:
/// [`Held`] when another daemon holds the lock.
pub fn take(data: &Path, record: &DaemonRecord) -> io::Result<Result<Instance, Held>> {
    fs::create_dir_all(service_dir(data))?;
    let Some(lock) = project::lock_file_within(&lock_file(data), TAKE_WAIT, TAKE_POLL)? else {
        return Ok(Err(Held(read(data))));
    };
    let path = record_file(data);
    step::write_atomically(&path, &record.render())?;
    Ok(Ok(Instance {
        _lock: lock,
        record: path,
        pid: record.pid,
    }))
}

impl Drop for Instance {
    /// The record goes while the lock is still held, so no other daemon's
    /// record is ever removed.
    fn drop(&mut self) {
        let path = &self.record;
        if fs::read_to_string(path)
            .ok()
            .and_then(|text| DaemonRecord::parse(&text).ok())
            .is_some_and(|record| record.pid == self.pid)
        {
            let _ = fs::remove_file(path);
        }
    }
}

/// The daemon that runs on `data`, if one holds its lock: its record, `None`
/// when it does not read. One try, let go at once, which never keeps a daemon
/// from starting: it tries a while.
pub fn running(data: &Path) -> io::Result<Option<Option<DaemonRecord>>> {
    if !project::held_alone(&lock_file(data))? {
        return Ok(None);
    }
    Ok(Some(read(data)))
}

/// The record in `data`, when there is one that reads.
fn read(data: &Path) -> Option<DaemonRecord> {
    fs::read_to_string(record_file(data))
        .ok()
        .and_then(|text| DaemonRecord::parse(&text).ok())
}

#[cfg(test)]
mod tests {
    use owlshift_contracts::format::Format;

    use super::*;

    fn record(pid: u32) -> DaemonRecord {
        DaemonRecord {
            format: Format,
            pid,
            version: "0.0.0".to_owned(),
            binary: "/bin/owlshift".to_owned(),
            since: jiff::Timestamp::now(),
            build: None,
        }
    }

    #[test]
    fn one_daemon_holds_the_data_directory_until_it_ends() {
        let data = tempfile::tempdir().unwrap();
        let data = data.path();
        assert!(running(data).unwrap().is_none(), "no daemon yet");
        let first = take(data, &record(1)).unwrap().unwrap();
        assert_eq!(running(data).unwrap(), Some(Some(record_at(data))));
        let Held(holder) = take(data, &record(2)).unwrap().unwrap_err();
        assert_eq!(holder.map(|record| record.pid), Some(1));
        drop(first);
        assert!(running(data).unwrap().is_none());
        assert!(!record_file(data).exists(), "its record goes with it");

        // A record left by a daemon killed outright is replaced.
        fs::write(record_file(data), record(3).render()).unwrap();
        assert!(
            running(data).unwrap().is_none(),
            "a record alone is no daemon"
        );
        let second = take(data, &record(4)).unwrap().unwrap();
        assert_eq!(record_at(data).pid, 4);
        drop(second);
    }

    fn record_at(data: &Path) -> DaemonRecord {
        read(data).unwrap()
    }
}
