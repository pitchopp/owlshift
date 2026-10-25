//! What left a process tree's group, found when the tree is stopped
//! (OWL-451). Claude Code runs each Bash call of its run in a session and
//! process group of its own (`setsid`), which a signal to the tree's group
//! does not reach (checked on 2026-10-25, see
//! `docs/design/build-plan.md`, "P7 · Background service", L7).
//!
//! [`freeze_reached`] searches the process table for every process
//! descending by parent pid from one of the tree's, and every member of a
//! process group one of those leads ([`reach`]). It freezes each one found
//! with SIGSTOP, then searches again, until a search made once every frozen
//! process has stopped finds none new: a process started by one that was
//! being frozen is found that way too. It then checks each one again
//! ([`verify`]) and hands back those whose parent and group are unchanged,
//! for the caller to kill. Any other process it froze is let go with
//! SIGCONT, whatever happens, so none is ever left stopped by it.
//!
//! Nothing is waited on. A search is one listing of the process table, and
//! there are at most [`ROUNDS`] of them plus one, so the signal thread, which
//! stops trees under the lock of the live trees, stays bounded. Measured on
//! 2026-10-25 on a Mac running some 800 processes: a listing took 1.4 ms,
//! and a stop of a shell with two jobs, two listings, 3.2 ms at worst over
//! 20 stops.
//!
//! Not found: a process whose parent and group leader have both ended
//! before the stop, such as a background job a finished Bash call left
//! behind; one the system does not describe to this process, such as one
//! running as another user or, on macOS, a zombie. The table is read with
//! `proc_listallpids` and `proc_pidinfo` on macOS, and from
//! `/proc/<pid>/stat` on Linux. The tests' own reader of a process's state,
//! `test_proc.rs`, parses the same file apart, on purpose: it is test code,
//! included by path, which this module cannot use.

use std::collections::{BTreeMap, BTreeSet};
use std::io;

use rustix::process::{Pid, Signal, kill_process};

/// The most searches made before the last: past them, what is frozen is
/// checked and killed, and a process started meanwhile and not frozen yet
/// is left, so that a stop stays bounded even while processes keep forking.
const ROUNDS: usize = 8;

/// One process of the table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Entry {
    pub(super) pid: i32,
    pub(super) parent: i32,
    pub(super) group: i32,
    /// Whether it runs no code: stopped, or ended and not reaped yet.
    pub(super) stopped: bool,
}

/// Every process the system describes to this one.
pub(super) fn list() -> io::Result<Vec<Entry>> {
    imp::list()
}

/// The pids of `listing` that a stop of the tree of `group` reaches: the
/// group's members, then, until nothing changes, every process whose parent
/// is reached or whose group's id is the pid of a reached process, its
/// leader. Never `own`, this process, nor pid 1.
pub(super) fn reach(listing: &[Entry], group: i32, own: i32) -> BTreeSet<i32> {
    let eligible = |entry: &Entry| entry.pid != own && entry.pid > 1;
    let mut reached: BTreeSet<i32> = listing
        .iter()
        .filter(|entry| entry.group == group && eligible(entry))
        .map(|entry| entry.pid)
        .collect();
    loop {
        let before = reached.len();
        for entry in listing {
            if eligible(entry)
                && (reached.contains(&entry.parent) || reached.contains(&entry.group))
            {
                reached.insert(entry.pid);
            }
        }
        if reached.len() == before {
            return reached;
        }
    }
}

/// What the check of the frozen processes against `listing` decides: the
/// pids listed with the parent and the group they had when frozen, to kill,
/// and every other one, to let go. One listed otherwise may be another
/// process under a reused pid; one not listed may have ended, or may no
/// longer be described.
pub(super) fn verify(frozen: &BTreeMap<i32, Entry>, listing: &[Entry]) -> (Vec<i32>, Vec<i32>) {
    let listed: BTreeMap<i32, &Entry> = listing.iter().map(|entry| (entry.pid, entry)).collect();
    let (mut kill, mut let_go) = (Vec::new(), Vec::new());
    for was in frozen.values() {
        if listed.get(&was.pid).is_some_and(|now| same(was, now)) {
            kill.push(was.pid);
        } else {
            let_go.push(was.pid);
        }
    }
    (kill, let_go)
}

/// Whether `now` is still the process listed as `was`: same pid, parent and
/// group.
fn same(was: &Entry, now: &Entry) -> bool {
    was.pid == now.pid && was.parent == now.parent && was.group == now.group
}

/// Freezes what a stop of the tree of `group` reaches in the listings
/// `list` gives, as the module says, and hands back the pids to kill. Every
/// other process it froze is let go before it returns, a failed listing or
/// the last round included: such a failure leaves what left the group
/// running, as before OWL-451.
pub(super) fn freeze_reached(
    group: i32,
    mut list: impl FnMut() -> io::Result<Vec<Entry>>,
) -> Vec<Pid> {
    let own = i32::try_from(std::process::id()).unwrap_or(i32::MAX);
    let mut frozen = Frozen::default();
    for round in 0..=ROUNDS {
        let Ok(listing) = list() else {
            return Vec::new();
        };
        let reached = reach(&listing, group, own);
        let fresh: Vec<Entry> = listing
            .iter()
            .filter(|entry| reached.contains(&entry.pid) && !frozen.0.contains_key(&entry.pid))
            .copied()
            .collect();
        if (fresh.is_empty() && frozen.all_stopped_in(&listing)) || round == ROUNDS {
            let (kill, _let_go) = verify(&frozen.0, &listing);
            return frozen.hand_over(&kill);
        }
        for entry in fresh {
            // Only a process this call stopped is its to kill: one that
            // ended, or that it may not signal, is left out.
            if let Some(pid) = Pid::from_raw(entry.pid)
                && kill_process(pid, Signal::STOP).is_ok()
            {
                frozen.0.insert(entry.pid, entry);
            }
        }
    }
    unreachable!("the last round returns")
}

/// The processes a search froze, each as listed when it was frozen; those
/// still held when it is dropped are let go.
#[derive(Default)]
struct Frozen(BTreeMap<i32, Entry>);

impl Frozen {
    /// Whether each one listed as it was frozen is shown stopped: a fork it
    /// was making when frozen is then over, and its child listed.
    fn all_stopped_in(&self, listing: &[Entry]) -> bool {
        listing.iter().all(|entry| {
            self.0
                .get(&entry.pid)
                .is_none_or(|was| !same(was, entry) || entry.stopped)
        })
    }

    /// Hands back the pids of `kill`, which leave the guard; the rest are
    /// let go as it drops.
    fn hand_over(mut self, kill: &[i32]) -> Vec<Pid> {
        kill.iter()
            .filter_map(|pid| {
                self.0
                    .remove(pid)
                    .and_then(|entry| Pid::from_raw(entry.pid))
            })
            .collect()
    }
}

impl Drop for Frozen {
    fn drop(&mut self) {
        for &pid in self.0.keys() {
            if let Some(pid) = Pid::from_raw(pid) {
                // Best effort: one that ended needs nothing.
                let _ = kill_process(pid, Signal::CONT);
            }
        }
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use std::ffi::{c_int, c_void};
    use std::{io, mem, ptr};

    use super::Entry;

    /// Room for the processes started between the count and the listing.
    const SLACK: usize = 256;

    pub(super) fn list() -> io::Result<Vec<Entry>> {
        // SAFETY: a null buffer asks for the number of processes only.
        let count = unsafe { libc::proc_listallpids(ptr::null_mut(), 0) };
        let mut room = usize::try_from(count).map_err(|_| io::Error::last_os_error())? + SLACK;
        loop {
            let mut pids: Vec<libc::pid_t> = vec![0; room];
            let size =
                c_int::try_from(room * size_of::<libc::pid_t>()).map_err(io::Error::other)?;
            // SAFETY: `pids` holds `size` bytes and outlives the call.
            let listed =
                unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast::<c_void>(), size) };
            let listed = usize::try_from(listed).map_err(|_| io::Error::last_os_error())?;
            if listed < room {
                pids.truncate(listed);
                return Ok(pids.into_iter().filter_map(describe).collect());
            }
            // Full: more may not have fitted.
            room *= 2;
        }
    }

    /// The process `pid`, if the system describes it: not when it ended,
    /// is a zombie, or runs as another user.
    fn describe(pid: libc::pid_t) -> Option<Entry> {
        // SAFETY: `proc_bsdinfo` is plain data, valid all zero.
        let mut info: libc::proc_bsdinfo = unsafe { mem::zeroed() };
        let size = c_int::try_from(size_of::<libc::proc_bsdinfo>()).ok()?;
        // SAFETY: `info` is `size` bytes and outlives the call.
        let written = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                (&raw mut info).cast::<c_void>(),
                size,
            )
        };
        (written == size).then(|| Entry {
            pid,
            parent: i32::try_from(info.pbi_ppid).unwrap_or(0),
            group: i32::try_from(info.pbi_pgid).unwrap_or(0),
            stopped: info.pbi_status == libc::SSTOP,
        })
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use std::{fs, io};

    use super::Entry;

    pub(super) fn list() -> io::Result<Vec<Entry>> {
        let mut entries = Vec::new();
        for dir in fs::read_dir("/proc")? {
            let Ok(dir) = dir else { continue };
            let Some(pid) = dir.file_name().to_str().and_then(|name| name.parse().ok()) else {
                continue;
            };
            // Gone meanwhile, or not readable: not described.
            let Ok(stat) = fs::read_to_string(dir.path().join("stat")) else {
                continue;
            };
            entries.extend(parse(pid, &stat));
        }
        Ok(entries)
    }

    /// `pid (name) state ppid pgrp ...`: the name may hold spaces and
    /// parentheses, so the fields follow the last closing one.
    fn parse(pid: i32, stat: &str) -> Option<Entry> {
        let (_, rest) = stat.rsplit_once(')')?;
        let mut fields = rest.split_whitespace();
        let state = fields.next()?;
        let parent = fields.next()?.parse().ok()?;
        let group = fields.next()?.parse().ok()?;
        Some(Entry {
            pid,
            parent,
            group,
            stopped: matches!(state, "T" | "t" | "Z" | "X" | "x"),
        })
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
mod imp {
    use std::io;

    use super::Entry;

    /// No search on another system: what left a tree's group is not found.
    pub(super) fn list() -> io::Result<Vec<Entry>> {
        Err(io::ErrorKind::Unsupported.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(pid: i32, parent: i32, group: i32) -> Entry {
        Entry {
            pid,
            parent,
            group,
            stopped: false,
        }
    }

    /// Reached: the tree's group, a Bash call's shell in a group of its own
    /// below it, the shell's job and a job orphaned in the shell's group. Not
    /// reached: the runner, a job whose group's leader is gone, nor this
    /// process, even below the tree.
    #[test]
    fn reach_follows_parents_and_the_groups_they_lead() {
        let listing = [
            entry(100, 1, 100),
            entry(200, 100, 200),
            entry(201, 200, 200),
            entry(300, 200, 300),
            entry(301, 300, 300),
            entry(302, 1, 300),
            entry(400, 1, 399),
            entry(600, 301, 600),
        ];
        assert_eq!(
            reach(&listing, 200, 600),
            BTreeSet::from([200, 201, 300, 301, 302])
        );
    }

    /// Killed: a frozen process listed as it was. Let go: one listed with
    /// another parent, maybe another process under its pid, and one no
    /// longer described.
    #[test]
    fn a_frozen_process_that_changed_is_let_go_not_killed() {
        let frozen = BTreeMap::from([
            (300, entry(300, 200, 300)),
            (301, entry(301, 300, 300)),
            (302, entry(302, 1, 300)),
        ]);
        let listing = [entry(300, 200, 300), entry(301, 77, 301)];
        assert_eq!(verify(&frozen, &listing), (vec![300], vec![301, 302]));
    }

    /// A search whose listing fails once it froze a process lets it go and
    /// hands back nothing to kill.
    #[test]
    fn a_failed_listing_lets_go_of_what_was_frozen() {
        use std::os::unix::process::CommandExt;
        use std::process::Command;
        use std::time::{Duration, Instant};

        let mut sleep = Command::new("sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .unwrap();
        let pid = i32::try_from(sleep.id()).unwrap();
        let mut listings = 0;
        let kill = freeze_reached(pid, || {
            listings += 1;
            match listings {
                1 => Ok(vec![entry(pid, 1, pid)]),
                _ => Err(io::Error::other("broken")),
            }
        });
        // A SIGCONT takes a moment to land.
        let running = Instant::now() + Duration::from_secs(5);
        let mut state = crate::test_proc::state(sleep.id());
        while state.starts_with('T') && Instant::now() < running {
            std::thread::sleep(Duration::from_millis(20));
            state = crate::test_proc::state(sleep.id());
        }
        sleep.kill().unwrap();
        sleep.wait().unwrap();
        assert!(kill.is_empty(), "{kill:?}");
        assert_eq!(listings, 2);
        assert!(!state.starts_with('T'), "left stopped: {state}");
    }
}
