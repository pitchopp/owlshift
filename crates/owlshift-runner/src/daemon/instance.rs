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

/// Where the daemon of `data` keeps the copy of its binary that its
/// children run where the system cannot pin it (OWL-553, architecture
/// section 5, "An upgrade"): `<data>/service/pinned`. Only the daemon that
/// holds the lock writes in it.
pub fn pinned_dir(data: &Path) -> PathBuf {
    service_dir(data).join("pinned")
}

/// This process as the daemon of its data directory: holds the lock, and
/// removes its record when dropped.
#[derive(Debug)]
pub struct Instance {
    lock: File,
    record: PathBuf,
    pid: u32,
}

/// Another daemon holds the data directory's lock: its record, when it reads.
#[derive(Debug)]
pub struct Held(pub Option<DaemonRecord>);

/// This process holding the lock of its data directory, not yet recorded:
/// a probe sees a daemon whose record does not read until
/// [`Locked::publish`] (OWL-553: the daemon pins its binary in between).
#[derive(Debug)]
pub struct Locked {
    lock: File,
}

/// Makes this process the daemon of `data`, recorded as `record` says:
/// [`Held`] when another daemon holds the lock.
pub fn take(data: &Path, record: &DaemonRecord) -> io::Result<Result<Instance, Held>> {
    match lock(data)? {
        Ok(locked) => locked.publish(data, record).map(Ok),
        Err(held) => Ok(Err(held)),
    }
}

/// Takes the lock of `data` for this process, removing the record a daemon
/// killed outright left: [`Held`] when another daemon holds the lock.
pub fn lock(data: &Path) -> io::Result<Result<Locked, Held>> {
    fs::create_dir_all(service_dir(data))?;
    let Some(lock) = project::lock_file_within(&lock_file(data), TAKE_WAIT, TAKE_POLL)? else {
        return Ok(Err(Held(read(data))));
    };
    match fs::remove_file(record_file(data)) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
        _ => {}
    }
    Ok(Ok(Locked { lock }))
}

/// Makes this process the daemon of `data` on the lock a daemon handed it
/// on the descriptor `fd` (OWL-616, architecture section 5, "An upgrade",
/// "Without a command"), removing the record left there. Refused unless the
/// descriptor is `data`'s `daemon.lock` (the same device and inode) and its
/// try-lock succeeds, as it does at once on the open file description that
/// already holds the lock.
#[cfg(unix)]
pub fn adopt(data: &Path, fd: i32) -> io::Result<Locked> {
    use std::fs::TryLockError;
    use std::os::unix::fs::MetadataExt;

    let lock = owlshift_platform::process::adopt_file(fd)?;
    let path = lock_file(data);
    let (ours, theirs) = (lock.metadata()?, fs::metadata(&path)?);
    if (ours.dev(), ours.ino()) != (theirs.dev(), theirs.ino()) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("descriptor {fd} is not {}", path.display()),
        ));
    }
    match lock.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                format!(
                    "descriptor {fd} does not hold {}: another daemon holds it",
                    path.display()
                ),
            ));
        }
        Err(TryLockError::Error(error)) => return Err(error),
    }
    match fs::remove_file(record_file(data)) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error),
        _ => {}
    }
    Ok(Locked { lock })
}

/// Native Windows runs no daemon (D9).
#[cfg(not(unix))]
pub fn adopt(data: &Path, fd: i32) -> io::Result<Locked> {
    let _ = (data, fd);
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "native Windows runs no daemon (D9)",
    ))
}

/// [`adopt`] the lock handed on `fd` when there is one, else [`lock`].
pub fn lock_or_adopt(data: &Path, fd: Option<i32>) -> io::Result<Result<Locked, Held>> {
    match fd {
        Some(fd) => adopt(data, fd).map(Ok),
        None => lock(data),
    }
}

impl Locked {
    /// Writes the record of the daemon that holds the lock, `record`.
    pub fn publish(self, data: &Path, record: &DaemonRecord) -> io::Result<Instance> {
        let path = record_file(data);
        step::write_atomically(&path, &record.render())?;
        Ok(Instance {
            lock: self.lock,
            record: path,
            pid: record.pid,
        })
    }
}

impl Instance {
    /// The lock this daemon holds, to hand to its successor (OWL-616).
    pub fn lock(&self) -> &File {
        &self.lock
    }

    /// Removes this daemon's record, keeping the lock: a probe reads a daemon
    /// held and not recorded, as while one pins (OWL-616).
    pub fn unpublish(&self) -> io::Result<()> {
        if read_at(&self.record).is_some_and(|record| record.pid == self.pid) {
            fs::remove_file(&self.record)?;
        }
        Ok(())
    }

    /// Writes this daemon's record again, `record`, once its successor did
    /// not take over.
    pub fn republish(&self, record: &DaemonRecord) -> io::Result<()> {
        step::write_atomically(&self.record, &record.render())
    }
}

impl Drop for Instance {
    /// The record goes while the lock is still held, so no other daemon's
    /// record is ever removed.
    fn drop(&mut self) {
        let _ = self.unpublish();
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
pub(crate) fn read(data: &Path) -> Option<DaemonRecord> {
    read_at(&record_file(data))
}

fn read_at(path: &Path) -> Option<DaemonRecord> {
    fs::read_to_string(path)
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
            watches: None,
        }
    }

    /// OWL-616: a daemon adopts the very lock it was handed, the first's
    /// record going, and the lock stays held once the first lets go; a fresh
    /// open of the lock, another open file description, and another file are
    /// refused.
    #[cfg(unix)]
    #[test]
    fn a_daemon_adopts_only_the_lock_it_was_handed() {
        use std::os::fd::IntoRawFd;

        let data = tempfile::tempdir().unwrap();
        let data = data.path();
        let first = take(data, &record(1)).unwrap().unwrap();
        let fresh = fs::OpenOptions::new()
            .write(true)
            .open(lock_file(data))
            .unwrap();
        let refused = adopt(data, fresh.into_raw_fd()).unwrap_err();
        assert_eq!(refused.kind(), io::ErrorKind::WouldBlock, "{refused}");
        let other = File::create(data.join("other")).unwrap();
        let refused = adopt(data, other.into_raw_fd()).unwrap_err();
        assert_eq!(refused.kind(), io::ErrorKind::InvalidInput, "{refused}");

        let copy = first.lock().try_clone().unwrap();
        let adopted = adopt(data, copy.into_raw_fd()).unwrap();
        assert_eq!(running(data).unwrap(), Some(None), "held, not recorded");
        drop(first);
        assert_eq!(running(data).unwrap(), Some(None), "still held");
        let second = adopted.publish(data, &record(2)).unwrap();
        assert_eq!(record_at(data).pid, 2);
        drop(second);
        assert!(running(data).unwrap().is_none());
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

        // A record left by a daemon killed outright is gone once the lock is
        // taken, and the new one shows only once published (OWL-553).
        fs::write(record_file(data), record(3).render()).unwrap();
        assert!(
            running(data).unwrap().is_none(),
            "a record alone is no daemon"
        );
        let locked = lock(data).unwrap().unwrap();
        assert_eq!(running(data).unwrap(), Some(None), "held, not recorded");
        let second = locked.publish(data, &record(4)).unwrap();
        assert_eq!(record_at(data).pid, 4);
        drop(second);
    }

    fn record_at(data: &Path) -> DaemonRecord {
        read(data).unwrap()
    }
}
