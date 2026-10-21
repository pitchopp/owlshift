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
//!
//! Who holds a slot (OWL-417) is written beside its lock, `slots/<n>.holder`,
//! by the taker once it holds the lock, and removed before the lock is let
//! go. [`MachineSlots::holders`] reads it for each slot held, so `owlshift
//! why` names what holds the slots, not what the event log suggests. A
//! record under a free lock is a dead holder's: it is never read, and the
//! next probe that finds the slot free removes it while it holds the slot.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use jiff::Timestamp;
use owlshift_contracts::config::MAX_CONCURRENT_RUNS;
use owlshift_contracts::ids::TicketId;
use serde_json::{Value, json};

use crate::project::try_lock_file;
use crate::step::write_atomically;

/// How many times a child tries every slot before it ends with no slot:
/// another watch's probe holds a free slot for a moment.
const TAKE_ROUNDS: u32 = 3;

/// The pause between two of those rounds.
const TAKE_PAUSE: Duration = Duration::from_millis(50);

/// How many times a taker tries to write its holder record: on Windows, a
/// record a reader still has open is only marked for deletion, and a new
/// one cannot be renamed over it until the reader closes it.
const RECORD_TRIES: u32 = 5;

/// The pause between two of those tries.
const RECORD_PAUSE: Duration = Duration::from_millis(10);

/// The machine's run slots, `count` of them in `dir`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MachineSlots {
    dir: PathBuf,
    count: u32,
}

/// Who holds a slot, as its holder record says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Holder {
    /// The project, as the event log names it (`owner/name`).
    pub project: String,
    pub ticket: TicketId,
    /// The holding process.
    pub pid: u32,
    /// When it took the slot.
    pub since: Timestamp,
}

impl Holder {
    fn render(&self) -> String {
        json!({
            "project": self.project,
            "ticket": self.ticket.as_str(),
            "pid": self.pid,
            "since": self.since.to_string(),
        })
        .to_string()
    }

    /// The record read back; `None` when it is not one this binary reads.
    fn parse(text: &str) -> Option<Self> {
        let value: Value = serde_json::from_str(text).ok()?;
        let field = |key: &str| value.get(key).and_then(Value::as_str);
        Some(Self {
            project: field("project")?.to_owned(),
            ticket: TicketId::new(field("ticket")?).ok()?,
            pid: u32::try_from(value.get("pid")?.as_u64()?).ok()?,
            since: field("since")?.parse().ok()?,
        })
    }
}

/// A slot held now, and who holds it. `holder` is `None` when its record is
/// missing or cannot be read: an older Owlshift, a failed write, or a take
/// a moment ago.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeldSlot {
    pub slot: u32,
    pub holder: Option<Holder>,
}

/// A run slot, held until it is dropped or the process ends.
#[derive(Debug)]
pub struct Slot {
    /// Its holder record, removed before the lock is let go.
    record: PathBuf,
    _file: File,
}

impl Drop for Slot {
    /// Removes the holder record while the lock is still held, so it never
    /// removes the record of the slot's next holder; the lock goes after,
    /// with the file.
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.record);
    }
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

    fn record(&self, n: u32) -> PathBuf {
        self.dir.join(format!("{n}.holder"))
    }

    /// The slots another holder holds now: every slot file up to the most
    /// `concurrent_runs` may be, so that a slot held above a cap just
    /// lowered still counts. Each is tried and let go at once; a missing
    /// file is a free slot, and none is created. A free slot's holder
    /// record, a dead holder's, is removed while the try holds the slot.
    fn held_slots(&self) -> io::Result<Vec<u32>> {
        let mut held = Vec::new();
        for n in 0..MAX_CONCURRENT_RUNS.max(self.count) {
            let file = match OpenOptions::new().write(true).open(self.file(n)) {
                Ok(file) => file,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            match file.try_lock() {
                // Held by this try, so no taker is writing the record.
                Ok(()) => remove_stale(&self.record(n)),
                Err(TryLockError::WouldBlock) => held.push(n),
                Err(TryLockError::Error(error)) => return Err(error),
            }
        }
        Ok(held)
    }

    /// How many slots another holder holds now (see [`Self::holders`]).
    pub fn held(&self) -> io::Result<u32> {
        Ok(u32::try_from(self.held_slots()?.len()).unwrap_or(u32::MAX))
    }

    /// The slots held now, each with what its holder record says. A record
    /// is read only once its slot was found held, never under a free one.
    pub fn holders(&self) -> io::Result<Vec<HeldSlot>> {
        Ok(self
            .held_slots()?
            .into_iter()
            .map(|slot| HeldSlot {
                slot,
                holder: fs::read_to_string(self.record(slot))
                    .ok()
                    .and_then(|text| Holder::parse(&text)),
            })
            .collect())
    }

    /// Whether every slot is held: a step started now would find none.
    pub fn full(&self) -> io::Result<bool> {
        Ok(self.held()? >= self.count)
    }

    /// Takes the first free slot from 0 to `count` − 1 for `holder`, trying
    /// them all a few times; `None` when none frees meanwhile. Once it
    /// holds the slot, it removes a dead holder's record and writes its
    /// own. That write is best effort: a slot whose record could not be
    /// written is still the step's, and is shown as held by someone unknown.
    pub fn take(&self, holder: &Holder) -> io::Result<Option<Slot>> {
        fs::create_dir_all(&self.dir)?;
        for round in 0..TAKE_ROUNDS {
            if round > 0 {
                thread::sleep(TAKE_PAUSE);
            }
            for n in 0..self.count {
                if let Some(file) = try_lock_file(&self.file(n))? {
                    let record = self.record(n);
                    remove_stale(&record);
                    write_record(&record, &holder.render());
                    return Ok(Some(Slot {
                        record,
                        _file: file,
                    }));
                }
            }
        }
        Ok(None)
    }
}

/// Removes a holder record left by a dead holder, if there is one.
fn remove_stale(record: &Path) {
    if record.exists() {
        let _ = fs::remove_file(record);
    }
}

/// Writes a holder record whole, tried a few times; a record that cannot
/// be written is left out.
fn write_record(path: &Path, text: &str) {
    for tried in 0..RECORD_TRIES {
        if tried > 0 {
            thread::sleep(RECORD_PAUSE);
        }
        if write_atomically(path, text).is_ok() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn holder(ticket: &str) -> Holder {
        Holder {
            project: "o/r".to_owned(),
            ticket: TicketId::new(ticket).unwrap(),
            pid: 42,
            since: "2026-10-21T16:55:00Z".parse().unwrap(),
        }
    }

    fn held(slot: u32, ticket: &str) -> HeldSlot {
        HeldSlot {
            slot,
            holder: Some(holder(ticket)),
        }
    }

    /// Checked alone, in a fresh copy of this test binary, for the reason
    /// `project::tests::one_do_at_a_time_per_project` gives: a child spawned
    /// by another test's thread holds a lock until it execs.
    #[test]
    fn slots_are_taken_up_to_the_cap_counted_and_named() {
        crate::alone::run_alone("slots::tests::helper_slots");
    }

    #[test]
    #[ignore = "helper, run by the test above"]
    fn helper_slots() {
        if !crate::alone::running_alone() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let record = |n: u32| dir.path().join(format!("slots/{n}.holder"));
        let slots = MachineSlots::new(dir.path(), 2);
        assert_eq!(slots.held().unwrap(), 0, "no file yet");
        assert!(
            !dir.path().join("slots").exists(),
            "a probe creates nothing"
        );
        let first = slots.take(&holder("OWL-1")).unwrap().expect("slot 0");
        let second = slots.take(&holder("OWL-2")).unwrap().expect("slot 1");
        assert!(dir.path().join("slots/1.lock").exists());
        assert_eq!(slots.held().unwrap(), 2);
        assert!(slots.full().unwrap());
        assert!(slots.take(&holder("OWL-3")).unwrap().is_none(), "the cap");
        assert_eq!(
            slots.holders().unwrap(),
            [held(0, "OWL-1"), held(1, "OWL-2")]
        );
        drop(first);
        assert!(!record(0).exists(), "removed with the slot");
        assert_eq!(slots.held().unwrap(), 1, "a probe holds nothing");
        assert!(!slots.full().unwrap());

        // A dead holder's record under a free lock is never read, and the
        // probe that finds the slot free removes it.
        fs::write(record(0), holder("OWL-9").render()).unwrap();
        assert_eq!(slots.holders().unwrap(), [held(1, "OWL-2")]);
        assert!(!record(0).exists());

        // A taker replaces a dead holder's record: never the dead's name.
        fs::write(record(0), holder("OWL-9").render()).unwrap();
        let third = slots.take(&holder("OWL-3")).unwrap().expect("slot 0");
        assert_eq!(slots.holders().unwrap()[0], held(0, "OWL-3"));

        // A held slot whose record cannot be read: held, holder unknown.
        fs::write(record(0), "not a record").unwrap();
        let unknown = HeldSlot {
            slot: 0,
            holder: None,
        };
        assert_eq!(slots.holders().unwrap()[0], unknown);
        fs::remove_file(record(0)).unwrap();
        assert_eq!(slots.holders().unwrap()[0], unknown);
        drop(third);

        // A cap lowered to 1 while slot 1 is held: it still counts and is
        // named, and slot 0 is free for a step only once the count allows.
        let lowered = MachineSlots::new(dir.path(), 1);
        assert_eq!(lowered.held().unwrap(), 1);
        assert_eq!(lowered.holders().unwrap(), [held(1, "OWL-2")]);
        assert!(lowered.full().unwrap());
        drop(second);
        assert!(!lowered.full().unwrap());
        assert!(lowered.take(&holder("OWL-4")).unwrap().is_some());
    }
}
