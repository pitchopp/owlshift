//! The machine's run slots (OWL-409, architecture section 5, "Parallel",
//! "Locks"): `concurrent_runs` lock files, `slots/<n>.lock` in Owlshift's
//! data directory, which is the user's state directory the design names,
//! shared by every project's `owlshift watch --drain` on the machine.
//!
//! Watch counts those held ([`MachineSlots::held`], a probe that holds each
//! one for a moment only) before it starts a step; the step's child takes
//! one ([`MachineSlots::take`]) before anything else and keeps it to its
//! end, the gate included. The files are opened close-on-exec, as every
//! file the standard library opens, so neither a harness nor a gate
//! inherits one, and the kernel drops a dead holder's lock (check (e), the
//! build plan's "P6 · Parallel").

use std::fs::{File, OpenOptions, TryLockError};
use std::io;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use owlshift_contracts::config::MAX_CONCURRENT_RUNS;

use crate::project::try_lock_file;

/// How many times a child tries every slot before it ends with no slot:
/// another watch's probe holds a free slot for a moment.
const TAKE_ROUNDS: u32 = 3;

/// The pause between two of those rounds.
const TAKE_PAUSE: Duration = Duration::from_millis(50);

/// The machine's run slots, `count` of them in `dir`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MachineSlots {
    dir: PathBuf,
    count: u32,
}

/// A run slot, held until it is dropped or the process ends.
#[derive(Debug)]
pub struct Slot {
    _file: File,
}

impl MachineSlots {
    /// The `count` slots of the machine whose Owlshift data directory is
    /// `data_dir`, `count` being `concurrent_runs`.
    pub fn new(data_dir: &Path, count: u32) -> Self {
        Self {
            dir: data_dir.join("slots"),
            count,
        }
    }

    /// How many steps may run at once: `concurrent_runs`.
    pub fn count(&self) -> u32 {
        self.count
    }

    fn file(&self, n: u32) -> PathBuf {
        self.dir.join(format!("{n}.lock"))
    }

    /// How many slots another holder holds now: every slot file up to the
    /// most `concurrent_runs` may be, so that a slot held above a cap just
    /// lowered still counts. Each is tried and let go at once; a missing
    /// file is a free slot, and none is created.
    pub fn held(&self) -> io::Result<u32> {
        let mut held = 0;
        for n in 0..MAX_CONCURRENT_RUNS.max(self.count) {
            let file = match OpenOptions::new().write(true).open(self.file(n)) {
                Ok(file) => file,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            match file.try_lock() {
                Ok(()) => {}
                Err(TryLockError::WouldBlock) => held += 1,
                Err(TryLockError::Error(error)) => return Err(error),
            }
        }
        Ok(held)
    }

    /// Whether every slot is held: a step started now would find none.
    pub fn full(&self) -> io::Result<bool> {
        Ok(self.held()? >= self.count)
    }

    /// Takes the first free slot from 0 to `count` − 1, trying them all a
    /// few times; `None` when none frees meanwhile.
    pub fn take(&self) -> io::Result<Option<Slot>> {
        std::fs::create_dir_all(&self.dir)?;
        for round in 0..TAKE_ROUNDS {
            if round > 0 {
                thread::sleep(TAKE_PAUSE);
            }
            for n in 0..self.count {
                if let Some(file) = try_lock_file(&self.file(n))? {
                    return Ok(Some(Slot { _file: file }));
                }
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Checked alone, in a fresh copy of this test binary, for the reason
    /// `project::tests::one_do_at_a_time_per_project` gives: a child spawned
    /// by another test's thread holds a lock until it execs.
    #[test]
    fn slots_are_taken_up_to_the_cap_and_counted() {
        crate::alone::run_alone("slots::tests::helper_slots");
    }

    #[test]
    #[ignore = "helper, run by the test above"]
    fn helper_slots() {
        if !crate::alone::running_alone() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let slots = MachineSlots::new(dir.path(), 2);
        assert_eq!(slots.held().unwrap(), 0, "no file yet");
        assert!(
            !dir.path().join("slots").exists(),
            "a probe creates nothing"
        );
        let first = slots.take().unwrap().expect("slot 0");
        let second = slots.take().unwrap().expect("slot 1");
        assert!(dir.path().join("slots/1.lock").exists());
        assert_eq!(slots.held().unwrap(), 2);
        assert!(slots.full().unwrap());
        assert!(slots.take().unwrap().is_none(), "the cap");
        drop(first);
        assert_eq!(slots.held().unwrap(), 1, "a probe holds nothing");
        assert!(!slots.full().unwrap());

        // A cap lowered to 1 while slot 1 is held: it still counts, and
        // slot 0 is free for a step only once the count allows it.
        let lowered = MachineSlots::new(dir.path(), 1);
        assert_eq!(lowered.held().unwrap(), 1);
        assert!(lowered.full().unwrap());
        drop(second);
        assert!(!lowered.full().unwrap());
        assert!(lowered.take().unwrap().is_some());
    }
}
