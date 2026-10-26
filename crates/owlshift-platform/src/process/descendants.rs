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
//! ([`verify`]) and hands back those whose parent, group and start are
//! unchanged, for the caller to kill. Any other process it froze is let go
//! with SIGCONT, whatever happens, so none is ever left stopped by it.
//!
//! A tree tied to a run's worktree ([`Workdir`], OWL-452) is also searched
//! from what the run left there ([`left_by`]): a background job a Bash call
//! left once that call returned, whose parent and group leader have both
//! ended, and which the search above cannot find (checked on 2026-10-25 and
//! 2026-10-26, L7, `docs/design/live-checks/owl-452.sh`).
//!
//! Nothing is waited on. A search is one listing of the process table, plus
//! one read of the working directory of each process the cheap rules of
//! [`left_by`] keep, and there are at most [`ROUNDS`] of them plus one, so
//! the signal thread, which stops trees under the lock of the live trees,
//! stays bounded. Measured on 2026-10-25 on a Mac running some 800
//! processes: a listing took 1.4 ms, and a stop of a shell with two jobs,
//! two listings, 3.2 ms at worst over 20 stops; reading the working
//! directory of every one of 655 processes took 1.1 to 1.4 ms.
//!
//! Not found: a process whose parent and group leader have both ended
//! before the stop, unless a run left it in its worktree as [`left_by`]
//! says; one the system does not describe to this process, such as one
//! running as another user or, on macOS, a zombie. The table is read with
//! `proc_listallpids` and `proc_pidinfo` on macOS, and from
//! `/proc/<pid>/stat` on Linux. The tests' own reader of a process's state,
//! `test_proc.rs`, parses the same file apart, on purpose: it is test code,
//! included by path, which this module cannot use.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::path::PathBuf;

use rustix::process::{Pid, Signal, kill_process};

/// The most searches made before the last: past them, what is frozen is
/// checked and killed, and a process started meanwhile and not frozen yet
/// is left, so that a stop stays bounded even while processes keep forking.
const ROUNDS: usize = 8;

/// Whether an orphan may be handed to one of its ancestors rather than to
/// pid 1: on Linux the nearest subreaper above it adopts it, such as
/// `systemd --user` above a user unit's processes (checked on 2026-10-25);
/// macOS has no subreapers.
const SUBREAPERS: bool = cfg!(target_os = "linux");

/// One process of the table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Entry {
    pub(super) pid: i32,
    pub(super) parent: i32,
    pub(super) group: i32,
    /// When it started, in the system's own unit: microseconds since the
    /// epoch on macOS, clock ticks since boot on Linux.
    pub(super) start: u64,
    /// Whether it runs no code: stopped, or ended and not reaped yet.
    pub(super) stopped: bool,
    /// Whether it ended and is not reaped yet; never on macOS, which
    /// describes no zombie.
    pub(super) ended: bool,
}

/// Every process the system describes to this one.
pub(super) fn list() -> io::Result<Vec<Entry>> {
    imp::list()
}

/// The working directory of `pid`, if the system shows it to this process.
pub(super) fn cwd(pid: i32) -> Option<PathBuf> {
    imp::cwd(pid)
}

/// What ties a process to the run whose tree left it (OWL-452): the run's
/// worktree, canonical, and the start of the tree's root.
#[derive(Debug)]
pub(in crate::process) struct Workdir {
    path: PathBuf,
    since: u64,
}

impl Workdir {
    /// The tie to `path`, canonical, of the tree whose root `root` was just
    /// spawned and is not reaped yet: none when the system does not describe
    /// the root, which then has ended already, having run nothing.
    pub(in crate::process) fn new(path: PathBuf, root: u32) -> Option<Self> {
        let root = i32::try_from(root).ok()?;
        imp::describe(root).map(|entry| Self {
            path,
            since: entry.start,
        })
    }
}

/// The pids of `listing` that the run tied to `workdir` left, from which a
/// stop of its tree also searches (OWL-452). Each one:
///
/// 1. has its working directory, as `cwd` reads it, in the worktree;
/// 2. started no earlier than the tree's root;
/// 3. was orphaned: its parent is pid 1, or on Linux one of the ancestors
///    of `own`, this process, never `own` itself unless it is pid 1;
/// 4. has no leader of its group left: no process listed, not ended, under
///    the group's id that started no later than it, so a pid reused since
///    is no leader.
///
/// Never `own` nor one of its ancestors. The working directory is read only
/// for the processes the other rules keep.
pub(super) fn left_by(
    listing: &[Entry],
    workdir: &Workdir,
    own: i32,
    cwd: impl Fn(i32) -> Option<PathBuf>,
) -> BTreeSet<i32> {
    let listed: BTreeMap<i32, &Entry> = listing.iter().map(|entry| (entry.pid, entry)).collect();
    let mut ancestors = BTreeSet::new();
    let mut at = Some(own);
    while let Some(pid) = at
        && ancestors.insert(pid)
    {
        at = listed
            .get(&pid)
            .map(|entry| entry.parent)
            .filter(|&parent| parent > 0);
    }
    let orphaned =
        |parent: i32| parent == 1 || (SUBREAPERS && parent != own && ancestors.contains(&parent));
    let led = |entry: &Entry| {
        listed
            .get(&entry.group)
            .is_some_and(|leader| !leader.ended && leader.start <= entry.start)
    };
    listing
        .iter()
        .filter(|entry| {
            entry.pid > 1
                && !entry.ended
                && !ancestors.contains(&entry.pid)
                && entry.start >= workdir.since
                && orphaned(entry.parent)
                && !led(entry)
                && cwd(entry.pid).is_some_and(|dir| dir.starts_with(&workdir.path))
        })
        .map(|entry| entry.pid)
        .collect()
}

/// The pids of `listing` that a stop of a tree reaches: the members of its
/// `group`, if any, and the `seeds`, then, until nothing changes, every
/// process whose parent is reached or whose group's id is the pid of a
/// reached process, its leader. Never `own`, this process, nor pid 1.
pub(super) fn reach(
    listing: &[Entry],
    group: Option<i32>,
    seeds: &BTreeSet<i32>,
    own: i32,
) -> BTreeSet<i32> {
    let eligible = |entry: &Entry| entry.pid != own && entry.pid > 1;
    let mut reached: BTreeSet<i32> = listing
        .iter()
        .filter(|entry| {
            eligible(entry) && (Some(entry.group) == group || seeds.contains(&entry.pid))
        })
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
/// pids listed with the parent, the group and the start they had when
/// frozen, to kill, and every other one, to let go. One listed otherwise may
/// be another process under a reused pid; one not listed may have ended, or
/// may no longer be described.
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

/// Whether `now` is still the process listed as `was`: same pid, parent,
/// group and start (OWL-452: a pid reused meanwhile is another process).
fn same(was: &Entry, now: &Entry) -> bool {
    was.pid == now.pid
        && was.parent == now.parent
        && was.group == now.group
        && was.start == now.start
}

/// Freezes what a stop of a tree reaches in the listings `list` gives, as
/// the module says, from the members of `group`, if any, and from what the
/// run tied to `workdir`, if any, left, each working directory read with
/// `cwd`; hands back the pids to kill. Every other process it froze is let
/// go before it returns, a failed listing or the last round included: such
/// a failure leaves what left the group running, as before OWL-451.
pub(super) fn freeze_reached(
    group: Option<i32>,
    workdir: Option<&Workdir>,
    mut list: impl FnMut() -> io::Result<Vec<Entry>>,
    cwd: impl Fn(i32) -> Option<PathBuf>,
) -> Vec<Pid> {
    let own = i32::try_from(std::process::id()).unwrap_or(i32::MAX);
    let mut frozen = Frozen::default();
    for round in 0..=ROUNDS {
        let Ok(listing) = list() else {
            return Vec::new();
        };
        let seeds = workdir
            .map(|workdir| left_by(&listing, workdir, own, &cwd))
            .unwrap_or_default();
        let reached = reach(&listing, group, &seeds, own);
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
    use std::ffi::{OsStr, c_int, c_void};
    use std::os::unix::ffi::OsStrExt;
    use std::path::PathBuf;
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
    pub(super) fn describe(pid: libc::pid_t) -> Option<Entry> {
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
            start: info
                .pbi_start_tvsec
                .saturating_mul(1_000_000)
                .saturating_add(info.pbi_start_tvusec),
            stopped: info.pbi_status == libc::SSTOP,
            ended: false,
        })
    }

    /// The working directory of `pid`, read with `PROC_PIDVNODEPATHINFO`:
    /// shown for any process of the same user, an Apple binary's and a
    /// confined one's included (checked on 2026-10-25).
    pub(super) fn cwd(pid: libc::pid_t) -> Option<PathBuf> {
        // SAFETY: `proc_vnodepathinfo` is plain data, valid all zero.
        let mut info: libc::proc_vnodepathinfo = unsafe { mem::zeroed() };
        let size = c_int::try_from(size_of::<libc::proc_vnodepathinfo>()).ok()?;
        // SAFETY: `info` is `size` bytes and outlives the call.
        let written = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDVNODEPATHINFO,
                0,
                (&raw mut info).cast::<c_void>(),
                size,
            )
        };
        if written != size {
            return None;
        }
        // The path ends at its first NUL, within `MAXPATHLEN`.
        let path: Vec<u8> = info
            .pvi_cdir
            .vip_path
            .as_flattened()
            .iter()
            .take_while(|&&byte| byte != 0)
            .map(|&byte| byte as u8)
            .collect();
        (!path.is_empty()).then(|| PathBuf::from(OsStr::from_bytes(&path)))
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use std::path::PathBuf;
    use std::{fs, io};

    use super::Entry;

    pub(super) fn list() -> io::Result<Vec<Entry>> {
        let mut entries = Vec::new();
        for dir in fs::read_dir("/proc")? {
            let Ok(dir) = dir else { continue };
            let Some(pid) = dir.file_name().to_str().and_then(|name| name.parse().ok()) else {
                continue;
            };
            entries.extend(describe(pid));
        }
        Ok(entries)
    }

    /// The process `pid`, if `/proc/<pid>/stat` can be read: not when it is
    /// gone, or went meanwhile.
    pub(super) fn describe(pid: i32) -> Option<Entry> {
        parse(pid, &fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)
    }

    /// `pid (name) state ppid pgrp ... starttime ...`: the name may hold
    /// spaces and parentheses, so the fields follow the last closing one;
    /// `starttime` is the 22nd field, the 20th after the name.
    fn parse(pid: i32, stat: &str) -> Option<Entry> {
        let (_, rest) = stat.rsplit_once(')')?;
        let fields: Vec<&str> = rest.split_whitespace().collect();
        let state = *fields.first()?;
        Some(Entry {
            pid,
            parent: fields.get(1)?.parse().ok()?,
            group: fields.get(2)?.parse().ok()?,
            start: fields.get(19)?.parse().ok()?,
            stopped: matches!(state, "T" | "t" | "Z" | "X" | "x"),
            ended: matches!(state, "Z" | "X" | "x"),
        })
    }

    /// The working directory of `pid`: not shown for another user's
    /// process, nor for a non-dumpable one.
    pub(super) fn cwd(pid: i32) -> Option<PathBuf> {
        fs::read_link(format!("/proc/{pid}/cwd")).ok()
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
mod imp {
    use std::io;
    use std::path::PathBuf;

    use super::Entry;

    /// No search on another system: what left a tree's group is not found.
    pub(super) fn list() -> io::Result<Vec<Entry>> {
        Err(io::ErrorKind::Unsupported.into())
    }

    pub(super) fn describe(_pid: i32) -> Option<Entry> {
        None
    }

    pub(super) fn cwd(_pid: i32) -> Option<PathBuf> {
        None
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
            start: 100,
            stopped: false,
            ended: false,
        }
    }

    /// Reached: the tree's group, a Bash call's shell in a group of its own
    /// below it, the shell's job and a job orphaned in the shell's group, and
    /// a seed's own child. Not reached: the runner, a job whose group's leader
    /// is gone, nor this process, even below the tree.
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
            entry(700, 1, 699),
            entry(701, 700, 699),
        ];
        assert_eq!(
            reach(&listing, Some(200), &BTreeSet::from([700]), 600),
            BTreeSet::from([200, 201, 300, 301, 302, 700, 701])
        );
    }

    /// OWL-452: what a run left, from a listing where this process is 50,
    /// below 40 and 1, the run's root started at 100, the worktree `/wt`.
    #[test]
    fn left_by_ties_only_what_a_run_left() {
        let at = |pid, parent, group, start| Entry {
            start,
            ..entry(pid, parent, group)
        };
        let zombie = Entry {
            ended: true,
            stopped: true,
            ..entry(901, 1, 901)
        };
        let listing = [
            at(40, 1, 40, 1),
            at(50, 40, 50, 2),
            // Left: orphaned to pid 1, its group's leader gone.
            at(300, 1, 299, 200),
            // Left: its group's leader ended, not reaped yet.
            at(301, 1, 901, 200),
            zombie,
            // Left: its group's "leader" a pid reused since, younger.
            at(302, 1, 902, 200),
            at(902, 77, 902, 250),
            // Left on Linux only: handed to an ancestor, a subreaper.
            at(303, 40, 299, 200),
            // Not left: started before the run's root.
            at(310, 1, 299, 99),
            // Not left: working outside the worktree.
            at(311, 1, 299, 200),
            // Not left: its parent alive, not an ancestor.
            at(312, 77, 299, 200),
            at(77, 1, 77, 3),
            // Not left: its group's leader alive.
            at(313, 1, 313, 200),
            // Not left: its parent is this process.
            at(314, 50, 299, 200),
        ];
        let workdir = Workdir {
            path: PathBuf::from("/wt"),
            since: 100,
        };
        let cwd = |pid: i32| {
            Some(PathBuf::from(match pid {
                311 => "/wt-other",
                _ => "/wt/sub",
            }))
        };
        let mut expected = BTreeSet::from([300, 301, 302]);
        if SUBREAPERS {
            expected.insert(303);
        }
        assert_eq!(left_by(&listing, &workdir, 50, cwd), expected);
        // This process as pid 1 adopts what is orphaned, and is never left
        // itself; its ancestors never are either.
        let own_init = [at(1, 0, 1, 1), at(300, 1, 299, 200)];
        assert_eq!(left_by(&own_init, &workdir, 1, cwd), BTreeSet::from([300]));
        let below = [at(40, 1, 299, 150), at(50, 40, 50, 160)];
        assert!(left_by(&below, &workdir, 50, cwd).is_empty());
    }

    /// Killed: a frozen process listed as it was. Let go: one listed with
    /// another parent, maybe another process under its pid, one listed with
    /// another start, another process under its pid (OWL-452), and one no
    /// longer described.
    #[test]
    fn a_frozen_process_that_changed_is_let_go_not_killed() {
        let frozen = BTreeMap::from([
            (300, entry(300, 200, 300)),
            (301, entry(301, 300, 300)),
            (302, entry(302, 1, 300)),
            (303, entry(303, 1, 300)),
        ]);
        let listing = [
            entry(300, 200, 300),
            entry(301, 77, 301),
            Entry {
                start: 101,
                ..entry(303, 1, 300)
            },
        ];
        assert_eq!(verify(&frozen, &listing), (vec![300], vec![301, 302, 303]));
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
        let kill = freeze_reached(
            Some(pid),
            None,
            || {
                listings += 1;
                match listings {
                    1 => Ok(vec![entry(pid, 1, pid)]),
                    _ => Err(io::Error::other("broken")),
                }
            },
            cwd,
        );
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
