//! The isolation check: after every run, the main checkout is untouched,
//! the run's changes stayed in its worktree, and the worktree is on the
//! expected branch (architecture, section 8).
//!
//! [`Snapshot::take`] records, before the run, what the run must leave
//! alone; [`Snapshot::check`] compares after the run and lists every
//! [`Violation`]:
//!
//! - **Main checkout untouched**: its HEAD commit and checked-out branch,
//!   and every entry of `git status --porcelain=v2 --untracked-files=all
//!   --ignored=matching`, with the content of each listed file (hashed by
//!   `git hash-object`): tracked and staged changes, untracked files, and
//!   ignored files such as `.env`. An ignored directory counts by its
//!   presence alone.
//! - **Shared git state untouched**: the repository's `config`,
//!   `config.worktree`, `hooks/` and `info/`, which every worktree shares,
//!   so a planted hook or `core.hooksPath` cannot run code in the user's
//!   checkout later, and no `commondir` there, which would point git in the
//!   main checkout at another repository's configuration.
//! - **Other worktrees' entries untouched** (OWL-339): the files of every
//!   other `worktrees/<name>` of the common directory, which hold what
//!   another ticket's worktree stages (`index`) and sees (`HEAD`), but the
//!   run's own entry (`own_entry`), which its commits, `add`s and rebases
//!   write. The sandbox closes them to a confined run; this is the check for
//!   a run that got past it, or that ran bare. A file whose name is not
//!   UTF-8 is a key of its own, a file is compared whole (a digest past the
//!   first MiB), and a path that vanishes while it is read (a lock file) is
//!   absent, not an error.
//! - **Diff inside the worktree**: no ref of the repository was created,
//!   moved or deleted but the run's branch (tags, other branches,
//!   `refs/stash`); remote-tracking refs are left out, a fetch being
//!   harmless.
//! - **Expected branch**: the worktree's HEAD is still the run's branch,
//!   and its commit descends from the one the run started from or, for a
//!   rebase ([`Snapshot::take_rebase`]), from the new base, resolved to a
//!   commit before the run. A run that is not a rebase and rewrote its
//!   branch is still a breach; its violation says so when the branch shares
//!   a commit with the start (OWL-369).
//!
//! Runs in flight at the same time move their own branches and ticket refs,
//! so [`Snapshot::check_among`] is told which ([`Concurrent`], read from the
//! project's run files and ticket locks since OWL-400), just before and just
//! after it reads the refs: a ref running in either answer is left out,
//! that run's own check, or its ticket's next start, guarding it, and a ref
//! that had already ended in the first answer must still hold one of the
//! values its run file records. Everything else is checked as for
//! a run alone, and a breach there quarantines every run that sees it: the
//! check cannot tell which run did it. [`Snapshot::check`] is the check of a
//! run alone, told of no other run: it has no exemption. A ref is read with
//! its target when it is symbolic, so one a run turned into an alias is a
//! change even at the same commit.
//!
//! The entry of a run in flight is left out the same way, by the branch the
//! entry belonged to when the snapshot was taken (`owner`, read in rounds
//! while that run rewrites the entry's files), never by a later `HEAD` of an
//! existing entry, which a run could forge. An entry the snapshot did not see,
//! or caught while `git worktree add` was building it, is judged by its owner
//! read after the run and after the second answer, which is what lets a run
//! that registered between the two answers pass. Two limits: a brand-new
//! entry planted whole with a `HEAD` naming a running branch (which needs a
//! write the sandbox refuses), and the exemption itself, which hides every
//! write to the entry of a run in flight, a FIFO, link or oversized file
//! planted as its `HEAD` after the snapshot included, so that the sandbox
//! alone guards that entry; and an entry in the middle of an `--apply`
//! rebase reads as on no branch, since that backend detaches `HEAD` before
//! it writes any state.
//!
//! Before any git runs, two checks read the file system alone (OWL-59):
//!
//! - **The worktree's `.git` link** ([`check_worktree_link`]): an agent owns
//!   every file of its worktree, `.git` included, and a link pointed at a
//!   git directory of its own would have git in the worktree read that
//!   directory's hooks and configuration.
//! - **The shared git files**, compared with the snapshot's: a run that
//!   planted a command there, such as a clean filter that `git status` would
//!   run, is caught before the runner's git reads them.
//!
//! A violation there ends the check: no git runs, and the other checks are
//! not made (the other worktrees' entries are compared with the refs, after
//! those two checks, so a hook that changed ends the check before they are). The runner's git runs no hook and no file-system monitor
//! either way (`executor::Git`), and the main checkout's status starts no
//! git in a submodule (`--ignore-submodules=dirty`; it still reads the
//! submodule's HEAD itself), so no git runs with a configuration nothing
//! checked (OWL-50).
//!
//! A check that cannot run counts as a violation. Nothing is written: git
//! runs with `--no-optional-locks`, and `hash-object` without `-w`.
//!
//! It assumes nobody else changes the main checkout during the run: a
//! person editing there looks like a breach. Known limits: changes outside
//! the repository, a file inside an ignored directory, an edit inside a
//! submodule's work tree (a gitlink added or moved is still found), and an
//! index flag (`assume-unchanged`, `skip-worktree`) hiding an edit go
//! unseen; confining the agent is the operating system's job.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use super::git::{Git, GitError};
use crate::config::exit_text;

/// The most bytes a shared git file is kept in memory for; a larger one is
/// compared by its size and the digest of the whole file.
const SHARED_FILE_CAP: u64 = 1024 * 1024;

/// The most bytes of files one read of the shared git files takes in, all
/// files together. Past it the read fails, one byte later, and the check
/// reports that it could not run: a run that got past the sandbox cannot
/// keep the check reading with a huge or sparse file (OWL-339).
const READ_BUDGET: u64 = 1024 * 1024 * 1024;

/// The most bytes of files one read of the other worktrees' entries takes in
/// from any one entry, as [`READ_BUDGET`] does: far above what git writes in
/// an entry, whose largest file is the `index`, about 100 bytes a tracked
/// file, so some 100 MiB for a million files.
const ENTRY_BUDGET: u64 = 1024 * 1024 * 1024;

/// The most bytes of files one read of the other worktrees' entries takes in,
/// all entries together. The runner keeps every ticket's worktree, so the
/// entries add up. A worktree's checkout takes some 40 times what its index
/// does (at least a 4 KiB block a file), so 64 GiB of indexes, some 650 of
/// a million files, comes with 2.5 TiB of checkouts on the same disk. A
/// budget per entry alone would let a run past the sandbox keep the
/// check reading, a GiB at a time, with every entry it adds.
const ENTRIES_BUDGET: u64 = 64 * 1024 * 1024 * 1024;

/// The most names a violation lists before summing up the rest.
const LISTED: usize = 10;

/// The most bytes of a git link file (`.git`, `commondir`): one path.
const LINK_FILE_CAP: u64 = 4096;

/// What the run must leave alone, as it was before the run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    /// The repository's common git directory, resolved before the run, so
    /// that the checks made before any git know where it is.
    common: PathBuf,
    /// The name of the run's own entry in `<common>/worktrees`: the one
    /// entry of another's the run may change.
    own: OsString,
    main: MainState,
    /// The worktree's commit when the run started.
    start: String,
    /// For a rebase, the new base, as the commit it was before the run.
    onto: Option<String>,
}

/// The project's other tickets in flight at any time since a snapshot, as
/// their run files and locks tell them when [`Snapshot::check_among`] asks:
/// just before and just after it reads the refs (OWL-400,
/// `crate::ticket_run::Siblings`). Refs are full names: a ticket's branch
/// (`refs/heads/<branch>`) and its ticket ref.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Concurrent {
    /// The refs of tickets in flight, their lock held. A change to one is
    /// left out: that run's own check, or the ticket's next start, guards
    /// it.
    pub running: BTreeSet<String>,
    /// The refs of tickets that ended since the snapshot, each with the
    /// values it may hold: what its run file records and the target of a
    /// write under way, `None` for no ref. A ref that had already ended
    /// before the refs were read, and did not change again since, must
    /// still hold one of them.
    pub ended: BTreeMap<String, BTreeSet<Option<String>>>,
    /// The worktree entries, by their names in `worktrees/`, of tickets in
    /// flight or ended since the snapshot: left out, as the entries of runs
    /// in flight are.
    pub entries: BTreeSet<String>,
}

impl Concurrent {
    /// Whether no run is known: what a run alone is told.
    pub fn is_empty(&self) -> bool {
        self.running.is_empty() && self.ended.is_empty() && self.entries.is_empty()
    }

    /// Whether `name`, a full ref name, is in flight or ended since the
    /// snapshot, in this answer.
    pub fn knows(&self, name: &str) -> bool {
        self.running.contains(name) || self.ended.contains_key(name)
    }

    /// This answer with the branch `branch` in flight.
    pub fn running_branch(mut self, branch: &str) -> Self {
        self.running.insert(format!("refs/heads/{branch}"));
        self
    }

    /// This answer with the branch `branch` ended since the snapshot at
    /// `tip`, the commit its run's own check verified ([`Checked::tip`]).
    pub fn ended_branch(mut self, branch: &str, tip: &str) -> Self {
        self.ended.insert(
            format!("refs/heads/{branch}"),
            BTreeSet::from([Some(tip.to_owned())]),
        );
        self
    }
}

/// Whether either of the two answers knows `name`, a full ref name.
fn known(earlier: &Concurrent, later: &Concurrent, name: &str) -> bool {
    earlier.knows(name) || later.knows(name)
}

/// Whether a ref that had ended in the earlier answer, where it may hold
/// `accepted`, is held to those values: running in neither answer, and the
/// later answer giving the same values when it knows the ref. One whose
/// values changed between the two answers ran again meanwhile, so the refs
/// may hold it at any point of that run: it is left out.
fn settled(
    name: &str,
    accepted: &BTreeSet<Option<String>>,
    earlier: &Concurrent,
    later: &Concurrent,
) -> bool {
    !earlier.running.contains(name)
        && !later.running.contains(name)
        && later
            .ended
            .get(name)
            .is_none_or(|later_accepted| later_accepted == accepted)
}

/// What [`Snapshot::check_among`] found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Checked {
    /// Everything the run changed that it had to leave alone; empty when
    /// isolation held.
    pub violations: Vec<Violation>,
    /// The commit the run's branch was at when checked, whatever the
    /// verdict; `None` when the worktree was not on the branch or the check
    /// could not read it.
    pub tip: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct MainState {
    /// The main checkout's HEAD commit; `None` on an unborn branch.
    head: Option<String>,
    /// The checked-out branch; `None` when HEAD is detached.
    branch: Option<String>,
    /// `git status` entries by path.
    entries: BTreeMap<Vec<u8>, Entry>,
    /// The shared git files, by path relative to the common directory.
    shared: BTreeMap<PathBuf, Content>,
    /// The other worktrees' entries of the common directory (`worktrees/
    /// <name>/...`, the run's own left out), by path relative to it.
    worktree_entries: BTreeMap<PathBuf, Content>,
    /// Who each of those entries belonged to when the snapshot was taken,
    /// by entry name; empty after the run (see [`owner`]).
    owners: BTreeMap<String, Owner>,
    /// Every ref but the run's branch and remote-tracking refs, with its
    /// object, and `-> <target>` after it for a symbolic ref.
    refs: BTreeMap<String, String>,
}

/// One `git status` record, and the path it is about.
#[derive(Clone, Debug, PartialEq, Eq)]
struct StatusRecord {
    path: Vec<u8>,
    record: Vec<u8>,
}

/// A `git status` entry of the main checkout: its record and its file.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Entry {
    record: Vec<u8>,
    content: Content,
}

/// The state of one file.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Content {
    /// A regular file, by its blob id or, for a shared file, its bytes.
    File(Vec<u8>),
    /// A file of the common directory past the cap of its read (every file
    /// of another worktree's entry): its size and the SHA-256 of the whole
    /// file, so a change anywhere in it shows.
    Large(u64, Vec<u8>),
    Link(PathBuf),
    Dir,
    Missing,
    /// A FIFO, a socket, a device.
    Other,
}

/// The run a worktree's entry belongs to, as [`owner`] reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Owner {
    /// The entry is on this branch, or in a rebase of it.
    Branch(String),
    /// The entry has no `HEAD` yet: `git worktree add` builds it.
    Creating,
    /// On no branch, or a `HEAD` that could not be read plainly.
    Detached,
}

/// Something the run changed that it had to leave alone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Violation {
    /// The main checkout's HEAD moved to another commit.
    MainHead,
    /// The main checkout has another branch checked out, or none.
    MainBranch,
    /// Paths of the main checkout whose status or content changed.
    MainFiles(Vec<String>),
    /// Shared git files that changed, relative to the common directory.
    SharedGitFiles(Vec<String>),
    /// Files of other worktrees' entries in the common directory
    /// (`worktrees/<name>/...`) that changed, relative to it, outside the
    /// entries of runs in flight.
    WorktreeEntries(Vec<String>),
    /// Refs other than the run's branch that were created, moved or
    /// deleted, outside what concurrent runs may change.
    Refs(Vec<String>),
    /// The worktree is no longer on the run's branch.
    Branch {
        expected: String,
        found: Option<String>,
    },
    /// The worktree's `.git` no longer links it to the repository: why
    /// ([`check_worktree_link`]).
    WorktreeLink(String),
    /// The branch descends neither from the run's start commit nor, for a
    /// rebase, from its new base.
    History {
        start: String,
        end: String,
        /// The rebase's new base; `None` for a run that may only add to its
        /// branch.
        onto: Option<String>,
        /// Whether the branch still shares a commit with the start, the
        /// likely mark of a rebase, an amend or a reset of the run's own
        /// branch rather than a history from elsewhere. Only looked for when
        /// `onto` is `None`, and always `false` otherwise. `false` means not
        /// found, not ruled out: an amended root commit, or a commit at a
        /// shallow clone's boundary, leaves no shared commit to find.
        rewritten: bool,
    },
    /// A check could not run, so a change cannot be ruled out.
    CheckFailed(String),
}

impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MainHead => f.write_str("the main checkout's HEAD moved"),
            Self::MainBranch => f.write_str("the main checkout changed branch"),
            Self::MainFiles(paths) => {
                write!(f, "files changed in the main checkout: {}", names(paths))
            }
            Self::SharedGitFiles(paths) => {
                write!(
                    f,
                    "the repository's shared git files changed: {}",
                    names(paths)
                )
            }
            Self::WorktreeEntries(paths) => {
                write!(f, "other worktrees' git entries changed: {}", names(paths))
            }
            Self::Refs(refs) => write!(f, "refs changed outside the run's branch: {}", names(refs)),
            Self::Branch { expected, found } => write!(
                f,
                "the worktree is on {}, not {expected}",
                found.as_deref().unwrap_or("no branch")
            ),
            Self::WorktreeLink(reason) => f.write_str(reason),
            Self::History {
                start,
                end,
                onto: None,
                rewritten,
            } => {
                write!(
                    f,
                    "the branch is at {end}, which does not descend from the run's start {start}"
                )?;
                if *rewritten {
                    f.write_str(": the run rewrote its branch's history (rebase, amend or reset)")?;
                }
                Ok(())
            }
            Self::History {
                start,
                end,
                onto: Some(onto),
                ..
            } => write!(
                f,
                "the branch is at {end}, which descends neither from the run's start {start} \
                 nor from its new base {onto}"
            ),
            Self::CheckFailed(reason) => write!(f, "the isolation check could not run: {reason}"),
        }
    }
}

fn names(items: &[String]) -> String {
    let mut text = items
        .iter()
        .take(LISTED)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    if items.len() > LISTED {
        text.push_str(&format!(" and {} more", items.len() - LISTED));
    }
    text
}

/// Why a snapshot could not be taken.
#[derive(Debug)]
pub struct SnapshotError(String);

impl fmt::Display for SnapshotError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SnapshotError {}

impl From<GitError> for SnapshotError {
    fn from(error: GitError) -> Self {
        Self(error.to_string())
    }
}

impl Snapshot {
    /// Records the state of `main` and the worktree's commit, before a run
    /// on `branch` that may only add to its history.
    pub fn take(
        git: &Git,
        main: &Path,
        worktree: &Path,
        branch: &str,
    ) -> Result<Self, SnapshotError> {
        Self::take_with(git, main, worktree, branch, None)
    }

    /// Records the same before a run that rebases `branch` onto `onto`:
    /// after the run, the branch may descend from `onto` instead of from its
    /// start. `onto` is resolved to a commit now, so that nothing the run
    /// does to a ref, a remote-tracking one included, can move it; the
    /// caller gives the base it resolved after its own fetch.
    pub fn take_rebase(
        git: &Git,
        main: &Path,
        worktree: &Path,
        branch: &str,
        onto: &str,
    ) -> Result<Self, SnapshotError> {
        Self::take_with(git, main, worktree, branch, Some(onto))
    }

    fn take_with(
        git: &Git,
        main: &Path,
        worktree: &Path,
        branch: &str,
        onto: Option<&str>,
    ) -> Result<Self, SnapshotError> {
        let printed = git.run(
            main,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        )?;
        let common = PathBuf::from(trimmed(&printed));
        let common = fs::canonicalize(&common)
            .map_err(|e| SnapshotError(format!("{}: {e}", common.display())))?;
        let own = own_entry(&common, worktree)
            .map_err(SnapshotError)?
            .file_name()
            .map(OsString::from)
            .ok_or_else(|| SnapshotError(format!("{}: no entry name", worktree.display())))?;
        let mut main = MainState::read(git, main, branch, &common, &own)?;
        main.owners = read_owners(&common, &own, &|_| {})?;
        let start = trimmed(&git.run(worktree, &["rev-parse", "--verify", "HEAD"])?);
        let onto = onto
            .map(|onto| {
                let commit = format!("{onto}^{{commit}}");
                git.run(
                    worktree,
                    &["rev-parse", "--verify", "--end-of-options", commit.as_str()],
                )
                .map(|resolved| trimmed(&resolved))
                .map_err(|e| SnapshotError(format!("the new base {onto:?}: {e}")))
            })
            .transpose()?;
        Ok(Self {
            common,
            own,
            main,
            start,
            onto,
        })
    }

    /// The checks made before any git runs, on the file system alone: the
    /// worktree's `.git` link, then the shared git files. The gate makes
    /// them too, before its own git (OWL-64).
    pub(crate) fn check_files(&self, worktree: &Path) -> Vec<Violation> {
        let mut violations = Vec::new();
        if let Err(reason) = check_worktree_link(&self.common, worktree) {
            violations.push(Violation::WorktreeLink(reason));
        }
        match read_shared(&self.common) {
            Ok(after) => {
                let shared: Vec<String> = changed(&self.main.shared, &after)
                    .into_iter()
                    .map(|path| display_path(path))
                    .collect();
                if !shared.is_empty() {
                    violations.push(Violation::SharedGitFiles(shared));
                }
            }
            Err(error) => violations.push(Violation::CheckFailed(error.to_string())),
        }
        violations
    }

    /// Everything a run alone changed that it had to leave alone; empty
    /// when isolation held. It is [`Snapshot::check_among`] told of no other
    /// run, kept as one path so that both stay the same check.
    pub fn check(&self, git: &Git, main: &Path, worktree: &Path, branch: &str) -> Vec<Violation> {
        self.check_among(git, main, worktree, branch, &|| Ok(Concurrent::default()))
            .violations
    }

    /// Checks a run among others in flight at the same time. `concurrent`
    /// is asked twice, just before and just after the refs are read. The
    /// second answer knows a run registered before it created its branch,
    /// even when it started during the check; the first tells a run that
    /// had already ended from one that ended during the check, after its
    /// branch was read. An answer that could not be read is a check that
    /// could not run, and counts as an empty one: a lost record never widens
    /// what the check leaves out (OWL-400).
    pub fn check_among(
        &self,
        git: &Git,
        main: &Path,
        worktree: &Path,
        branch: &str,
        concurrent: &dyn Fn() -> Result<Concurrent, String>,
    ) -> Checked {
        // Nothing the run left in the shared git files, or behind a
        // redirected link, may run in the runner's git: a violation found
        // on the file system ends the check before any git runs.
        let violations = self.check_files(worktree);
        if !violations.is_empty() {
            return Checked {
                violations,
                tip: None,
            };
        }
        let mut unread = Vec::new();
        let mut ask = || {
            concurrent().unwrap_or_else(|reason| {
                unread.push(Violation::CheckFailed(format!(
                    "the project's other tickets could not be read: {reason}"
                )));
                Concurrent::default()
            })
        };
        let earlier = ask();
        let mut violations = match MainState::read(git, main, branch, &self.common, &self.own) {
            Ok(after) => {
                let later = ask();
                // After the second answer, which knows a run registered
                // before it made its entry.
                let owners = self.owners_after(&after, &earlier, &later, &|_| {});
                self.main.compare(&after, &earlier, &later, &owners)
            }
            Err(error) => vec![Violation::CheckFailed(error.to_string())],
        };
        unread.dedup();
        violations.extend(unread);
        let tip = self
            .check_branch(git, worktree, branch, &mut violations)
            .unwrap_or_else(|error| {
                violations.push(Violation::CheckFailed(error.to_string()));
                None
            });
        Checked { violations, tip }
    }

    /// The owners, read after the run, of the entries the snapshot cannot
    /// speak for: those present now that were absent or still being made
    /// then. Read only when a run is known, and after the second answer, so
    /// that a run that registered between the two answers, with its entry
    /// made before the second one, is not flagged.
    fn owners_after(
        &self,
        after: &MainState,
        earlier: &Concurrent,
        later: &Concurrent,
        between: &dyn Fn(&Path),
    ) -> BTreeMap<String, Owner> {
        if earlier.is_empty() && later.is_empty() {
            return BTreeMap::new();
        }
        let names: BTreeSet<&str> = after
            .worktree_entries
            .keys()
            .filter_map(|path| path.components().nth(1)?.as_os_str().to_str())
            .filter(|name| matches!(self.main.owners.get(*name), None | Some(Owner::Creating)))
            .collect();
        names
            .into_iter()
            .map(|name| {
                let entry = self.common.join("worktrees").join(name);
                (name.to_owned(), owner(&entry, between))
            })
            .collect()
    }

    /// Checks that the worktree is on the run's branch and that its history
    /// is one the run may leave; returns the branch's commit when the
    /// worktree is on it.
    fn check_branch(
        &self,
        git: &Git,
        worktree: &Path,
        branch: &str,
        violations: &mut Vec<Violation>,
    ) -> Result<Option<String>, SnapshotError> {
        let expected = format!("refs/heads/{branch}");
        let found = symbolic_head(git, worktree)?;
        if found.as_deref() != Some(expected.as_str()) {
            violations.push(Violation::Branch { expected, found });
            return Ok(None);
        }
        let end = trimmed(&git.run(worktree, &["rev-parse", "--verify", "HEAD"])?);
        let accepted = git.is_ancestor(worktree, &self.start, &end)?
            || match &self.onto {
                Some(onto) => git.is_ancestor(worktree, onto, &end)?,
                None => false,
            };
        if !accepted {
            // Only for a run that may only add to its branch: a rebase that
            // lands elsewhere shares history with its start by design. The
            // answer only explains the breach, so a git that cannot give it
            // keeps the breach and adds a check that could not run.
            let rewritten = self.onto.is_none()
                && git
                    .shares_history(worktree, &self.start, &end)
                    .unwrap_or_else(|error| {
                        violations.push(Violation::CheckFailed(error.to_string()));
                        false
                    });
            violations.push(Violation::History {
                start: self.start.clone(),
                end: end.clone(),
                onto: self.onto.clone(),
                rewritten,
            });
        }
        Ok(Some(end))
    }
}

impl MainState {
    fn read(
        git: &Git,
        main: &Path,
        branch: &str,
        common: &Path,
        own: &OsStr,
    ) -> Result<Self, SnapshotError> {
        let head = git.output(main, &["rev-parse", "-q", "--verify", "HEAD"], None)?;
        let head = match head.code {
            Some(0) => Some(trimmed(&head.stdout)),
            Some(1) => None,
            code => {
                return Err(SnapshotError(format!(
                    "git rev-parse HEAD failed with {}",
                    exit_text(code)
                )));
            }
        };
        let branch_now = symbolic_head(git, main)?;

        // `--ignore-submodules=dirty`: git starts no git of its own in a
        // submodule, whose configuration (a clean filter) no shared git file
        // holds and would run with the runner's environment, yet a
        // gitlink added or moved is still listed. Not `all`, as the gate's
        // status has (`gate::uncommitted`): it hides a planted gitlink. A
        // `status` option, which no `-c` of `Git`'s hardening can set (OWL-50).
        let status = git.run(
            main,
            &[
                "--no-optional-locks",
                "status",
                "--porcelain=v2",
                "-z",
                "--untracked-files=all",
                "--ignored=matching",
                "--ignore-submodules=dirty",
            ],
        )?;
        let records = status_records(&status).map_err(SnapshotError)?;
        let entries = contents(git, main, records)?;

        let shared = read_shared(common)?;
        let worktree_entries = read_worktree_entries(common, own, &|_| {})?;

        // Each ref with its target when it is symbolic (OWL-400): a ref a
        // run turned into an alias is a change even at the same commit.
        let own = format!("refs/heads/{branch}");
        let listed = git.run(
            main,
            &[
                "for-each-ref",
                "--format=%(objectname) %(refname) %(symref)",
            ],
        )?;
        let refs = String::from_utf8_lossy(&listed)
            .lines()
            .filter_map(|line| {
                let (object, rest) = line.split_once(' ')?;
                let (name, target) = rest.split_once(' ').unwrap_or((rest, ""));
                Some((object, name, target))
            })
            .filter(|(_, name, _)| *name != own && !name.starts_with("refs/remotes/"))
            .map(|(object, name, target)| {
                let value = if target.is_empty() {
                    object.to_owned()
                } else {
                    format!("{object} -> {target}")
                };
                (name.to_owned(), value)
            })
            .collect();

        Ok(Self {
            head,
            branch: branch_now,
            entries,
            shared,
            worktree_entries,
            owners: BTreeMap::new(),
            refs,
        })
    }

    fn compare(
        &self,
        after: &Self,
        earlier: &Concurrent,
        later: &Concurrent,
        owners_after: &BTreeMap<String, Owner>,
    ) -> Vec<Violation> {
        let mut violations = Vec::new();
        if self.head != after.head {
            violations.push(Violation::MainHead);
        }
        if self.branch != after.branch {
            violations.push(Violation::MainBranch);
        }
        let files: Vec<String> = changed(&self.entries, &after.entries)
            .into_iter()
            .map(|path| String::from_utf8_lossy(path).into_owned())
            .collect();
        if !files.is_empty() {
            violations.push(Violation::MainFiles(files));
        }
        let shared: Vec<String> = changed(&self.shared, &after.shared)
            .into_iter()
            .map(|path| display_path(path))
            .collect();
        if !shared.is_empty() {
            violations.push(Violation::SharedGitFiles(shared));
        }
        let entries = self.changed_entries(after, earlier, later, owners_after);
        if !entries.is_empty() {
            violations.push(Violation::WorktreeEntries(entries));
        }
        let refs = self.changed_refs(after, earlier, later);
        if !refs.is_empty() {
            violations.push(Violation::Refs(refs));
        }
        violations
    }

    /// The files of other worktrees' entries that changed, but the entries
    /// of concurrent runs. An entry is a concurrent ticket's when either
    /// answer names it, from that ticket's run file (OWL-400); or by the
    /// branch it belonged to when the snapshot was taken (`owners`), never by
    /// a later `HEAD` of an existing entry, which a run could forge; an entry
    /// the snapshot did not see, or caught while it was being made, is judged
    /// by the owner read after the run (`owners_after`), which is what
    /// covers a first run's entry before its run file names it. A branch is
    /// a concurrent run's when either answer knows it; an entry on no branch
    /// that no answer names is never exempt.
    fn changed_entries(
        &self,
        after: &Self,
        earlier: &Concurrent,
        later: &Concurrent,
        owners_after: &BTreeMap<String, Owner>,
    ) -> Vec<String> {
        let exempt = |name: &OsStr| {
            let Some(name) = name.to_str() else {
                return false;
            };
            if earlier.entries.contains(name) || later.entries.contains(name) {
                return true;
            }
            let branch_of = |owner: Option<&Owner>| match owner {
                Some(Owner::Branch(branch)) => Some(branch.clone()),
                _ => None,
            };
            let owner = match self.owners.get(name) {
                Some(Owner::Creating) | None => owners_after.get(name),
                owner => owner,
            };
            branch_of(owner)
                .is_some_and(|branch| known(earlier, later, &format!("refs/heads/{branch}")))
        };
        changed(&self.worktree_entries, &after.worktree_entries)
            .into_iter()
            .filter(|path| {
                !path
                    .components()
                    .nth(1)
                    .is_some_and(|name| exempt(name.as_os_str()))
            })
            .map(|path| display_path(path))
            .collect()
    }

    /// The refs that changed, but concurrent tickets' branches and ticket
    /// refs, known from the answers just before (`earlier`) and just after
    /// (`later`) the refs were read. A ref running in either answer is left
    /// out, and so is one whose values changed between them or that only the
    /// later answer knows: the refs may hold it at any point of that run. A
    /// ref that had already ended in the earlier answer counts when it holds
    /// none of its values ([`settled`]), whether it changed since the
    /// snapshot or not; a symbolic ref never holds one.
    fn changed_refs(&self, after: &Self, earlier: &Concurrent, later: &Concurrent) -> Vec<String> {
        let mut refs: Vec<String> = changed(&self.refs, &after.refs)
            .into_iter()
            .filter(|name| !known(earlier, later, name))
            .cloned()
            .collect();
        for (name, accepted) in &earlier.ended {
            if settled(name, accepted, earlier, later)
                && !accepted.contains(&after.refs.get(name).cloned())
            {
                refs.push(name.clone());
            }
        }
        refs.sort();
        refs.dedup();
        refs
    }
}

/// The keys whose value differs between two maps, or that one lacks.
fn changed<'a, K: Ord, V: PartialEq>(
    before: &'a BTreeMap<K, V>,
    after: &'a BTreeMap<K, V>,
) -> Vec<&'a K> {
    let mut keys: Vec<&K> = before
        .iter()
        .filter(|(key, value)| after.get(key) != Some(value))
        .map(|(key, _)| key)
        .chain(after.keys().filter(|key| !before.contains_key(key)))
        .collect();
    keys.sort();
    keys
}

/// The branch HEAD names in `dir`; `None` when it is detached.
fn symbolic_head(git: &Git, dir: &Path) -> Result<Option<String>, SnapshotError> {
    let head = git.output(dir, &["symbolic-ref", "-q", "HEAD"], None)?;
    match head.code {
        Some(0) => Ok(Some(trimmed(&head.stdout))),
        Some(1) => Ok(None),
        code => Err(SnapshotError(format!(
            "git symbolic-ref HEAD in {} failed with {}: {}",
            dir.display(),
            exit_text(code),
            String::from_utf8_lossy(&head.stderr).trim()
        ))),
    }
}

fn trimmed(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).trim().to_owned()
}

/// Splits `git status --porcelain=v2 -z` output into its records. A rename
/// or copy record carries its original path as the next field, kept in its
/// record.
fn status_records(output: &[u8]) -> Result<Vec<StatusRecord>, String> {
    let mut fields = output.split(|&byte| byte == 0);
    let mut records = Vec::new();
    while let Some(field) = fields.next() {
        let path_after = |spaces: usize| {
            field
                .splitn(spaces + 1, |&byte| byte == b' ')
                .nth(spaces)
                .filter(|path| !path.is_empty())
                .ok_or_else(|| {
                    format!(
                        "unexpected status record {:?}",
                        String::from_utf8_lossy(field)
                    )
                })
        };
        let (path, record) = match field.first() {
            None => continue,
            Some(b'1') => (path_after(8)?, field.to_vec()),
            Some(b'2') => {
                let original = fields
                    .next()
                    .filter(|original| !original.is_empty())
                    .ok_or("a rename record without its original path")?;
                let mut record = field.to_vec();
                record.push(0);
                record.extend_from_slice(original);
                (path_after(9)?, record)
            }
            Some(b'u') => (path_after(10)?, field.to_vec()),
            Some(b'?' | b'!') => (path_after(1)?, field.to_vec()),
            Some(_) => {
                return Err(format!(
                    "unexpected status record {:?}",
                    String::from_utf8_lossy(field)
                ));
            }
        };
        records.push(StatusRecord {
            path: path.to_vec(),
            record,
        });
    }
    Ok(records)
}

/// Each status entry with the state of its file: regular files are hashed
/// by `git hash-object`, in one call.
fn contents(
    git: &Git,
    main: &Path,
    records: Vec<StatusRecord>,
) -> Result<BTreeMap<Vec<u8>, Entry>, SnapshotError> {
    let mut entries = BTreeMap::new();
    let mut to_hash = Vec::new();
    for StatusRecord { path, record } in records {
        let content = if path.ends_with(b"/") {
            Content::Dir
        } else {
            let full = main.join(path_of(&path));
            match fs::symlink_metadata(&full) {
                Ok(meta) if meta.file_type().is_symlink() => {
                    Content::Link(fs::read_link(&full).unwrap_or_default())
                }
                Ok(meta) if meta.is_file() => {
                    if path.contains(&b'\n') {
                        return Err(SnapshotError(format!(
                            "{:?}: a path holding a newline cannot be hashed",
                            String::from_utf8_lossy(&path)
                        )));
                    }
                    to_hash.push(path.clone());
                    Content::File(Vec::new())
                }
                Ok(meta) if meta.is_dir() => Content::Dir,
                Ok(_) => Content::Other,
                Err(error) if error.kind() == io::ErrorKind::NotFound => Content::Missing,
                Err(error) => {
                    return Err(SnapshotError(format!("{}: {error}", full.display())));
                }
            }
        };
        entries.insert(path, Entry { record, content });
    }
    if to_hash.is_empty() {
        return Ok(entries);
    }
    let mut input = to_hash.join(&b'\n');
    input.push(b'\n');
    let hashed = git.output(
        main,
        &["hash-object", "--no-filters", "--stdin-paths"],
        Some(&input),
    )?;
    if !hashed.success() {
        return Err(SnapshotError(format!(
            "git hash-object failed with {}: {}",
            exit_text(hashed.code),
            String::from_utf8_lossy(&hashed.stderr).trim()
        )));
    }
    let ids: Vec<&[u8]> = hashed
        .stdout
        .split(|&byte| byte == b'\n')
        .filter(|id| !id.is_empty())
        .collect();
    if ids.len() != to_hash.len() {
        return Err(SnapshotError(format!(
            "git hash-object gave {} ids for {} files",
            ids.len(),
            to_hash.len()
        )));
    }
    for (path, id) in to_hash.iter().zip(ids) {
        if let Some(entry) = entries.get_mut(path) {
            entry.content = Content::File(id.to_vec());
        }
    }
    Ok(entries)
}

/// A path git printed, as a path of this platform: raw bytes on Unix,
/// UTF-8 elsewhere.
fn path_of(bytes: &[u8]) -> PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        PathBuf::from(std::ffi::OsStr::from_bytes(bytes))
    }
    #[cfg(not(unix))]
    {
        PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
    }
}

/// The shared git files every worktree reads: `config`, `config.worktree`,
/// `hooks/` and `info/` under the common directory, and `commondir`, which
/// git follows from the main checkout's git directory to read another's
/// configuration (OWL-50).
fn read_shared(common: &Path) -> Result<BTreeMap<PathBuf, Content>, SnapshotError> {
    let mut shared = BTreeMap::new();
    let mut budget = Budget::new(SHARED_FILE_CAP, READ_BUDGET, READ_BUDGET);
    for name in ["commondir", "config", "config.worktree", "hooks", "info"] {
        walk(common, Path::new(name), &mut shared, &mut budget, &|_| {})
            .map_err(|e| SnapshotError(format!("{}: {e}", common.join(name).display())))?;
    }
    Ok(shared)
}

/// Checks that the worktree's `.git` still links it to the repository whose
/// common git directory is `common`, such as `<checkout>/.git`, before the
/// runner trusts anything git says in it.
///
/// An agent owns every file of its worktree, `.git` included: pointing it,
/// or the `commondir` of its administrative folder, at a git directory of
/// its own would make git in the worktree read that directory's hooks and
/// configuration, which the isolation check never sees. So `.git` must be a
/// plain file whose `gitdir: ` names a folder of `<common>/worktrees`, that
/// folder's `commondir` must lead back to `common`, and it must hold no
/// `config.worktree`. Both files are read as git reads them: the whole file,
/// less its trailing line breaks, so a trailing space names another folder
/// here as it does for git; a file past a few kilobytes is refused. Paths
/// are compared once resolved, links included. No git command runs here.
pub fn check_worktree_link(common: &Path, worktree: &Path) -> Result<(), String> {
    let admin = own_entry(common, worktree)?;
    let common =
        fs::canonicalize(common).map_err(|e| breach(format!("{}: {e}", common.display())))?;
    let commondir = admin.join("commondir");
    let named = read_link_file(&commondir).map_err(breach)?;
    if resolve_link(&admin, &named)? != common {
        return Err(breach(format!(
            "{} leads away from {}",
            commondir.display(),
            common.display()
        )));
    }
    let own_config = admin.join("config.worktree");
    if fs::symlink_metadata(&own_config).is_ok() {
        return Err(breach(format!("{} appeared", own_config.display())));
    }
    Ok(())
}

/// The worktree's own entry in the common git directory
/// (`<common>/worktrees/<name>`), as the worktree's `.git` names it, resolved:
/// the first half of [`check_worktree_link`], and the one place the entry is
/// derived, so that the folder the sandbox leaves writable and the one the
/// isolation check exempts cannot differ. The entry must be a direct child of
/// the canonical `<common>/worktrees`. No git command runs here.
pub(crate) fn own_entry(common: &Path, worktree: &Path) -> Result<PathBuf, String> {
    let common =
        fs::canonicalize(common).map_err(|e| breach(format!("{}: {e}", common.display())))?;
    let worktrees = common.join("worktrees");
    let link = worktree.join(".git");
    let text = read_link_file(&link).map_err(breach)?;
    let target = text
        .strip_prefix("gitdir: ")
        .filter(|target| !target.is_empty())
        .ok_or_else(|| breach(format!("{} does not name a gitdir", link.display())))?;
    let admin = resolve_link(worktree, target)?;
    if admin.parent() != Some(worktrees.as_path()) {
        return Err(breach(format!(
            "it leads to {}, outside {}",
            admin.display(),
            worktrees.display()
        )));
    }
    Ok(admin)
}

fn breach(what: String) -> String {
    format!("the worktree's .git link was changed: {what}")
}

/// `path`, relative to `from` unless absolute, resolved with its links.
fn resolve_link(from: &Path, path: &str) -> Result<PathBuf, String> {
    let path = Path::new(path);
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        from.join(path)
    };
    fs::canonicalize(&path).map_err(|e| breach(format!("{}: {e}", path.display())))
}

/// A git link file (`.git`, `commondir`), as git reads it: a plain file,
/// less its trailing line breaks, holding one line of text.
fn read_link_file(path: &Path) -> Result<String, String> {
    let fail = |what: &str| format!("{} {what}", path.display());
    if !fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_file()) {
        return Err(fail("is not a plain file"));
    }
    let mut bytes = Vec::new();
    fs::File::open(path)
        .and_then(|file| file.take(LINK_FILE_CAP + 1).read_to_end(&mut bytes))
        .map_err(|e| format!("{}: {e}", path.display()))?;
    if bytes.len() as u64 > LINK_FILE_CAP {
        return Err(fail("is too large"));
    }
    let text = String::from_utf8(bytes).map_err(|_| fail("is not UTF-8 text"))?;
    let text = text.trim_end_matches(['\n', '\r']);
    if text.contains(['\n', '\r', '\0']) {
        return Err(fail("holds more than one line"));
    }
    Ok(text.to_owned())
}

/// Records `rel` under `common`, and everything below it for a directory,
/// without following links: the one walker of the shared git files and of
/// the other worktrees' entries.
///
/// Names stay raw ([`OsString`]) from the listing to the map's keys, so a
/// name that is not UTF-8 is a key of its own and a plant under it is seen.
/// A path that vanishes between the listing, the metadata and the open (a
/// concurrent run renaming a lock file) is read as absent, not as an error;
/// any other I/O error is returned. A file within `budget`'s cap is kept as
/// its bytes, a larger one as its size and digest. Every byte read is taken
/// from `budget`: a file that would read past what is left of it is an
/// error, found after one byte more and no more. `between` is called with the path after its
/// metadata is read and before it is opened, which lets a test change the
/// file at exactly that point.
fn walk(
    common: &Path,
    rel: &Path,
    out: &mut BTreeMap<PathBuf, Content>,
    budget: &mut Budget,
    between: &dyn Fn(&Path),
) -> io::Result<()> {
    fn present<T>(result: io::Result<T>) -> io::Result<Option<T>> {
        match result {
            Ok(value) => Ok(Some(value)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }
    let path = common.join(rel);
    let Some(meta) = present(fs::symlink_metadata(&path))? else {
        return Ok(());
    };
    let content = if meta.file_type().is_symlink() {
        let Some(target) = present(fs::read_link(&path))? else {
            return Ok(());
        };
        Content::Link(target)
    } else if meta.is_dir() {
        let listing: io::Result<Vec<OsString>> = fs::read_dir(&path)
            .and_then(|entries| entries.map(|entry| entry.map(|e| e.file_name())).collect());
        let Some(mut children) = present(listing)? else {
            return Ok(());
        };
        children.sort();
        for child in children {
            walk(common, &rel.join(child), out, budget, between)?;
        }
        Content::Dir
    } else if meta.is_file() {
        between(&path);
        let Some(file) = present(fs::File::open(&path))? else {
            return Ok(());
        };
        // One byte past what is left, so that a file past it shows.
        let mut file = file.take(budget.left().saturating_add(1));
        let mut bytes = Vec::new();
        (&mut file).take(budget.cap + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > budget.cap {
            // Past the cap: the digest of the whole file, so a change
            // anywhere in it shows.
            let mut hasher = Sha256::new();
            hasher.update(&bytes);
            let mut len = bytes.len() as u64;
            let mut chunk = [0u8; 64 * 1024];
            loop {
                match file.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(read) => {
                        hasher.update(&chunk[..read]);
                        len += read as u64;
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    Err(error) => return Err(error),
                }
            }
            budget.spend(&path, len)?;
            Content::Large(len, hasher.finalize().to_vec())
        } else {
            budget.spend(&path, bytes.len() as u64)?;
            Content::File(bytes)
        }
    } else {
        Content::Other
    };
    out.insert(rel.to_owned(), content);
    Ok(())
}

/// How one read of files takes them in: the bytes it may still read, in all
/// ([`READ_BUDGET`], [`ENTRIES_BUDGET`]) and of the entry it reads
/// ([`ENTRY_BUDGET`]), and the size up to which a file is kept as it is.
struct Budget {
    /// The most bytes of one file kept as they are; past it, its size and
    /// digest.
    cap: u64,
    /// What one entry may take, and what is left of it for the current one.
    each: u64,
    part: u64,
    /// What the whole read may take, and what is left of it.
    all: u64,
    left: u64,
}

impl Budget {
    fn new(cap: u64, each: u64, all: u64) -> Self {
        Self {
            cap,
            each,
            part: each,
            all,
            left: all,
        }
    }

    /// Starts the next entry, with the whole of what one entry may take.
    fn next_entry(&mut self) {
        self.part = self.each;
    }

    /// What may still be read: the least of what is left of the entry and
    /// of the whole.
    fn left(&self) -> u64 {
        self.part.min(self.left)
    }

    /// Takes the `read` bytes of `path` from what is left, or fails when
    /// they are more.
    fn spend(&mut self, path: &Path, read: u64) -> io::Result<()> {
        let past = |what: &str, limit: u64| {
            io::Error::other(format!(
                "{} takes {what} past the {limit} bytes a check reads",
                path.display()
            ))
        };
        let left = self
            .left
            .checked_sub(read)
            .ok_or_else(|| past("the files read", self.all))?;
        let part = self
            .part
            .checked_sub(read)
            .ok_or_else(|| past("its entry", self.each))?;
        self.left = left;
        self.part = part;
        Ok(())
    }
}

/// The children of `<common>/worktrees` but the run's own entry, sorted; a
/// repository with no linked worktree has none.
fn other_entries(common: &Path, own: &OsStr) -> io::Result<Vec<OsString>> {
    let listing: io::Result<Vec<OsString>> = fs::read_dir(common.join("worktrees"))
        .and_then(|entries| entries.map(|entry| entry.map(|e| e.file_name())).collect());
    let mut names = match listing {
        Ok(names) => names,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    names.retain(|name| name != own);
    names.sort();
    Ok(names)
}

/// The files of the other worktrees' entries under `<common>/worktrees`,
/// the run's own (`own`) left out, by path relative to `common`. A run that
/// got past the sandbox could change another ticket's `index` or `HEAD`
/// there, and with it what that worktree stages and sees (OWL-339). Every
/// file is kept as its size and digest, so that what the entries hold in
/// memory does not grow with their bytes; each entry has a budget of its
/// own, and all of them one more ([`ENTRY_BUDGET`], [`ENTRIES_BUDGET`]).
fn read_worktree_entries(
    common: &Path,
    own: &OsStr,
    between: &dyn Fn(&Path),
) -> Result<BTreeMap<PathBuf, Content>, SnapshotError> {
    let budget = Budget::new(0, ENTRY_BUDGET, ENTRIES_BUDGET);
    read_entries_within(common, own, between, budget)
}

/// [`read_worktree_entries`] within `budget`.
fn read_entries_within(
    common: &Path,
    own: &OsStr,
    between: &dyn Fn(&Path),
    mut budget: Budget,
) -> Result<BTreeMap<PathBuf, Content>, SnapshotError> {
    let worktrees = common.join("worktrees");
    let fail = |e: io::Error| SnapshotError(format!("{}: {e}", worktrees.display()));
    let mut entries = BTreeMap::new();
    for name in other_entries(common, own).map_err(fail)? {
        budget.next_entry();
        walk(
            common,
            &Path::new("worktrees").join(name),
            &mut entries,
            &mut budget,
            between,
        )
        .map_err(fail)?;
    }
    Ok(entries)
}

/// The owner of each other entry, by name, as [`owner`] reads it; an entry
/// whose name is not UTF-8 has none, so it is never exempt.
fn read_owners(
    common: &Path,
    own: &OsStr,
    between: &dyn Fn(&Path),
) -> Result<BTreeMap<String, Owner>, SnapshotError> {
    let worktrees = common.join("worktrees");
    let names = other_entries(common, own)
        .map_err(|e| SnapshotError(format!("{}: {e}", worktrees.display())))?;
    Ok(names
        .into_iter()
        .filter_map(|name| {
            let name = name.into_string().ok()?;
            let found = owner(&worktrees.join(&name), between);
            Some((name, found))
        })
        .collect())
}

/// How many times [`owner`] repeats its reads before it gives up.
const OWNER_ROUNDS: usize = 3;

/// What [`read_guarded`] found at a path.
enum Guarded {
    Missing,
    /// There, but not a plain file of one short UTF-8 line.
    Odd,
    Text(String),
}

/// Reads a file a run could have written as [`read_link_file`] does: a plain
/// file (a FIFO, a link or a folder is refused before any `open`, so
/// nothing blocks and nothing is followed), at most [`LINK_FILE_CAP`]
/// bytes of UTF-8 on one line, less its trailing line breaks.
fn read_guarded(path: &Path) -> Guarded {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_file() => {}
        Ok(_) => return Guarded::Odd,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Guarded::Missing,
        Err(_) => return Guarded::Odd,
    }
    let mut bytes = Vec::new();
    match fs::File::open(path) {
        Ok(file) => {
            if file
                .take(LINK_FILE_CAP + 1)
                .read_to_end(&mut bytes)
                .is_err()
            {
                return Guarded::Odd;
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Guarded::Missing,
        Err(_) => return Guarded::Odd,
    }
    if bytes.len() as u64 > LINK_FILE_CAP {
        return Guarded::Odd;
    }
    match String::from_utf8(bytes) {
        Ok(text) => {
            let text = text.trim_end_matches(['\n', '\r']);
            if text.contains(['\n', '\r', '\0']) {
                Guarded::Odd
            } else {
                Guarded::Text(text.to_owned())
            }
        }
        Err(_) => Guarded::Odd,
    }
}

/// The branch a rebase in the entry is rebasing, from its `head-name`
/// (`refs/heads/<branch>`): the merge backend's, then the apply backend's.
fn rebasing_branch(entry: &Path, between: &dyn Fn(&Path)) -> Option<String> {
    ["rebase-merge", "rebase-apply"].iter().find_map(|folder| {
        let path = entry.join(folder).join("head-name");
        let read = read_guarded(&path);
        between(&path);
        match read {
            Guarded::Text(text) => text.strip_prefix("refs/heads/").map(str::to_owned),
            _ => None,
        }
    })
}

/// The run an entry of `worktrees` belongs to, read while that run, if one
/// is in flight, rewrites the entry's files. The reads rest on the order git
/// writes and removes an entry's state (checked live on 2026-10-19, build
/// plan, "OWL-339 live checks"): `git worktree add` writes `HEAD` last, and
/// a merge-backend rebase writes `rebase-merge/head-name` before it detaches
/// `HEAD`, and restores `HEAD` before it removes the folder. Up to
/// [`OWNER_ROUNDS`] rounds of: the `head-name` files, `HEAD`, the
/// `head-name` files again. A missing `HEAD` is [`Owner::Creating`]; a
/// symbolic `HEAD` names its branch; a detached one belongs to the branch a
/// `head-name` names, in either read of it; a detached `HEAD` with none is
/// a rebase finishing between the reads, so the round is repeated, and after
/// the last round the entry is [`Owner::Detached`]. The `--apply` backend
/// detaches `HEAD` before it writes any state, so an entry in the middle of
/// one reads as `Detached`. A `HEAD` that exists but is not a plain short
/// line is `Detached` too, never `Creating`, so that a planted anomaly
/// cannot pass for an entry under creation; none of these gives an
/// exemption. `between` is called after each read.
fn owner(entry: &Path, between: &dyn Fn(&Path)) -> Owner {
    let head_path = entry.join("HEAD");
    for _ in 0..OWNER_ROUNDS {
        let before = rebasing_branch(entry, between);
        let head = read_guarded(&head_path);
        between(&head_path);
        let after = rebasing_branch(entry, between);
        match head {
            Guarded::Missing => return Owner::Creating,
            Guarded::Odd => return Owner::Detached,
            Guarded::Text(text) => {
                if let Some(target) = text.strip_prefix("ref: ") {
                    return match target.strip_prefix("refs/heads/") {
                        Some(branch) => Owner::Branch(branch.to_owned()),
                        None => Owner::Detached,
                    };
                }
                if let Some(branch) = before.or(after) {
                    return Owner::Branch(branch);
                }
            }
        }
    }
    Owner::Detached
}

/// A path as a violation lists it: its names joined with `/` on every
/// system, each through `to_string_lossy` (two names that differ in bytes
/// that are not UTF-8 may print alike, which costs a diagnostic, not a
/// detection). Not `to_string_lossy` of the whole path, which would spell
/// `\` on Windows.
fn display_path(path: &Path) -> String {
    path.components()
        .map(|part| part.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_records_keep_paths_with_spaces_and_renames_whole() {
        let output = b"1 .M N... 100644 100644 100644 aaaa aaaa a file.txt\0\
                       2 R. N... 100644 100644 100644 bbbb bbbb R100 new name.md\0old name.md\0\
                       u UU N... 100644 100644 100644 100644 c1 c2 c3 both.rs\0\
                       ? new dir/n.txt\0\
                       ! target/\0\
                       ! .env\0";
        let records = status_records(output).unwrap();
        let paths: Vec<String> = records
            .iter()
            .map(|record| String::from_utf8_lossy(&record.path).into_owned())
            .collect();
        assert_eq!(
            paths,
            [
                "a file.txt",
                "new name.md",
                "both.rs",
                "new dir/n.txt",
                "target/",
                ".env"
            ]
        );
        // The original path belongs to the rename's record.
        assert!(records[1].record.ends_with(b"\0old name.md"));

        assert!(status_records(b"2 R. N... 100644 100644 100644 b b R100 new.md\0").is_err());
        assert!(status_records(b"# branch.oid abc\0").is_err());
        assert_eq!(status_records(b"").unwrap(), []);
    }

    /// What `git worktree add` leaves, built by hand: a checkout's
    /// administrative folder for the worktree, and the worktree's `.git`.
    /// Returns the common directory, the administrative folder and the
    /// worktree.
    fn linked(dir: &Path) -> (PathBuf, PathBuf, PathBuf) {
        let common = dir.join("checkout").join(".git");
        let admin = common.join("worktrees").join("owl-1");
        let worktree = dir.join("worktrees").join("owl-1");
        fs::create_dir_all(&admin).unwrap();
        fs::create_dir_all(&worktree).unwrap();
        fs::write(admin.join("commondir"), "../..\n").unwrap();
        fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", admin.display()),
        )
        .unwrap();
        (common, admin, worktree)
    }

    #[test]
    fn a_worktree_must_stay_linked_to_the_checkout() {
        let dir = tempfile::tempdir().unwrap();
        let (common, _, worktree) = linked(dir.path());
        assert_eq!(check_worktree_link(&common, &worktree), Ok(()));

        // A relative gitdir, as `worktree.useRelativePaths` writes it, with
        // a Windows line break.
        fs::write(
            worktree.join(".git"),
            "gitdir: ../../checkout/.git/worktrees/owl-1\r\n",
        )
        .unwrap();
        assert_eq!(check_worktree_link(&common, &worktree), Ok(()));

        // Windows drops a path's trailing space, so no folder `owl-1 ` can
        // exist: the link names the real administrative folder, for git and
        // for the check alike, and is accepted.
        #[cfg(windows)]
        {
            let admin = common.join("worktrees").join("owl-1");
            fs::write(
                worktree.join(".git"),
                format!("gitdir: {} \n", admin.display()),
            )
            .unwrap();
            assert_eq!(check_worktree_link(&common, &worktree), Ok(()));
        }

        let cases = [
            "a gitdir elsewhere",
            "no gitdir line",
            "a .git folder",
            "a commondir elsewhere",
            "a per-worktree configuration",
            "a trailing space",
            "a second line",
            "an oversized link",
        ];
        // On Windows the trailing-space attack cannot be built (see above).
        let cases = cases
            .into_iter()
            .filter(|case| cfg!(not(windows)) || *case != "a trailing space");
        for case in cases {
            let dir = tempfile::tempdir().unwrap();
            let (common, admin, worktree) = linked(dir.path());
            // A git directory of the agent's own, with the same layout.
            let evil = dir.path().join("evil");
            let evil_admin = evil.join("worktrees").join("owl-1");
            fs::create_dir_all(&evil_admin).unwrap();
            fs::write(evil_admin.join("commondir"), "../..\n").unwrap();
            let link = worktree.join(".git");
            let gitdir = format!("gitdir: {}", admin.display());
            match case {
                "a gitdir elsewhere" => {
                    fs::write(&link, format!("gitdir: {}\n", evil_admin.display())).unwrap()
                }
                "no gitdir line" => fs::write(&link, "ref: HEAD\n").unwrap(),
                "a .git folder" => {
                    fs::remove_file(&link).unwrap();
                    fs::create_dir(&link).unwrap();
                }
                "a commondir elsewhere" => {
                    fs::write(admin.join("commondir"), evil.display().to_string()).unwrap()
                }
                "a per-worktree configuration" => {
                    fs::write(admin.join("config.worktree"), "[core]\n\tfsmonitor = x\n").unwrap()
                }
                // Git keeps the space: it opens `owl-1 `, a folder of the
                // agent's, not `owl-1`.
                "a trailing space" => {
                    let sibling = admin.with_file_name("owl-1 ");
                    fs::create_dir_all(&sibling).unwrap();
                    fs::write(sibling.join("commondir"), evil.display().to_string()).unwrap();
                    fs::write(&link, format!("{gitdir} \n")).unwrap();
                }
                "a second line" => fs::write(&link, format!("{gitdir}\nmore\n")).unwrap(),
                _ => fs::write(&link, format!("{gitdir}{}\n", " ".repeat(5000))).unwrap(),
            }
            let error = check_worktree_link(&common, &worktree).unwrap_err();
            assert!(
                error.contains("the worktree's .git link was changed"),
                "{case}: {error}"
            );
        }
    }

    /// The own entry is the canonical folder `.git` names, a direct child of
    /// `<common>/worktrees`. The refusals of the link test come through the
    /// same function.
    #[test]
    fn the_own_entry_is_the_canonical_folder_the_link_names() {
        let dir = tempfile::tempdir().unwrap();
        let (common, admin, worktree) = linked(dir.path());
        let expected = fs::canonicalize(&admin).unwrap();
        assert_eq!(own_entry(&common, &worktree), Ok(expected.clone()));

        // Named through `..` and a relative path, the same entry.
        fs::write(
            worktree.join(".git"),
            "gitdir: ../../checkout/.git/worktrees/../worktrees/owl-1\n",
        )
        .unwrap();
        assert_eq!(own_entry(&common, &worktree), Ok(expected));

        // The common folder itself is no entry, and a worktree with no
        // `.git` has none.
        fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", common.display()),
        )
        .unwrap();
        let error = own_entry(&common, &worktree).unwrap_err();
        assert!(error.contains("outside"), "{error}");
        fs::remove_file(worktree.join(".git")).unwrap();
        assert!(own_entry(&common, &worktree).is_err());
    }

    #[test]
    fn changed_keys_cover_edits_additions_and_removals() {
        let before = BTreeMap::from([("a", 1), ("b", 2), ("c", 3)]);
        let after = BTreeMap::from([("a", 1), ("b", 9), ("d", 4)]);
        assert_eq!(changed(&before, &after), [&"b", &"c", &"d"]);
    }

    /// No `cfg`: it runs on every system, the Windows leg included, where
    /// `Path::join` spells `\` and the violations must not.
    #[test]
    fn a_displayed_path_is_joined_with_slashes_on_every_system() {
        assert_eq!(
            display_path(&Path::new("hooks").join("pre-commit")),
            "hooks/pre-commit"
        );
        let entry: PathBuf = ["worktrees", "other", "index"].iter().collect();
        assert_eq!(display_path(&entry), "worktrees/other/index");
        assert_eq!(display_path(Path::new("config")), "config");
    }

    fn walked(common: &Path, name: &str, between: &dyn Fn(&Path)) -> io::Result<Vec<PathBuf>> {
        let mut out = BTreeMap::new();
        let mut budget = Budget::new(SHARED_FILE_CAP, READ_BUDGET, READ_BUDGET);
        walk(common, Path::new(name), &mut out, &mut budget, between)?;
        Ok(out.into_keys().collect())
    }

    /// What one read takes in is bounded: past the budget, whether by many
    /// files or one huge or sparse file, the walk fails instead of reading
    /// on, and within it every byte is counted.
    #[test]
    fn a_read_past_its_budget_fails_instead_of_reading_on() {
        let dir = tempfile::tempdir().unwrap();
        let hooks = dir.path().join("hooks");
        fs::create_dir(&hooks).unwrap();
        fs::write(hooks.join("a"), [b'a'; 600]).unwrap();
        fs::write(hooks.join("b"), [b'b'; 600]).unwrap();
        let read = |limit: u64| {
            let mut budget = Budget::new(SHARED_FILE_CAP, limit, limit);
            let mut out = BTreeMap::new();
            walk(
                dir.path(),
                Path::new("hooks"),
                &mut out,
                &mut budget,
                &|_| {},
            )
            .map(|()| budget.left())
        };
        assert_eq!(read(1200).unwrap(), 0);
        let error = read(1199).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert!(error.to_string().contains("1199 bytes"), "{error}");

        // One file past the budget and past the cap, whose bytes are
        // never written: the read stops one byte past the budget.
        fs::File::create(hooks.join("c"))
            .unwrap()
            .set_len(4 * SHARED_FILE_CAP)
            .unwrap();
        let limit = SHARED_FILE_CAP + 1200 + 10;
        let error = read(limit).unwrap_err();
        assert!(error.to_string().contains("hooks"), "{error}");
        assert!(read(1200 + 4 * SHARED_FILE_CAP).is_ok());
    }

    /// The entries the runner keeps add up: each has a budget of its own,
    /// so many entries within theirs are read whatever their sum, while one
    /// entry past its own, or all of them past the whole, fail. Every file
    /// of an entry is kept as its size and digest.
    #[test]
    fn each_worktree_entry_has_its_own_budget_within_one_for_all() {
        let dir = tempfile::tempdir().unwrap();
        let entries = dir.path().join("worktrees");
        for name in ["a", "b", "c", "own"] {
            fs::create_dir_all(entries.join(name)).unwrap();
            fs::write(entries.join(name).join("index"), [b'i'; 900]).unwrap();
        }
        let own = OsStr::new("own");
        let read = |each: u64, all: u64| {
            read_entries_within(dir.path(), own, &|_| {}, Budget::new(0, each, all))
        };
        // Three entries of 900 bytes, 2,700 in all, past one entry's 1,000.
        let read_all = read(1000, 2700).unwrap();
        let index: PathBuf = ["worktrees", "c", "index"].iter().collect();
        assert!(
            matches!(read_all.get(&index), Some(Content::Large(900, _))),
            "{read_all:?}"
        );
        assert!(!read_all.contains_key(Path::new("worktrees/own/index")));
        let error = read(1000, 2699).unwrap_err().to_string();
        assert!(
            error.contains("the files read past the 2699 bytes"),
            "{error}"
        );
        // One entry past its own budget fails, whatever is left of the whole.
        fs::write(entries.join("b").join("index"), [b'i'; 1001]).unwrap();
        let error = read(1000, 10_000).unwrap_err().to_string();
        assert!(error.contains("its entry past the 1000 bytes"), "{error}");
        assert!(error.contains("index"), "{error}");
    }

    #[test]
    fn a_file_that_vanishes_while_it_is_read_is_absent_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let entry = dir.path().join("worktrees").join("other");
        fs::create_dir_all(&entry).unwrap();
        fs::write(entry.join("index"), "i").unwrap();
        fs::write(entry.join("index.lock"), "l").unwrap();
        let remove_lock = |path: &Path| {
            if path.file_name().is_some_and(|name| name == "index.lock") {
                fs::remove_file(path).unwrap();
            }
        };
        let keys = walked(dir.path(), "worktrees", &remove_lock).unwrap();
        assert_eq!(
            keys,
            [
                PathBuf::from("worktrees"),
                ["worktrees", "other"].iter().collect(),
                ["worktrees", "other", "index"].iter().collect(),
            ]
        );
        // A missing root is empty too.
        assert!(walked(dir.path(), "nothing", &|_| {}).unwrap().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn a_file_that_cannot_be_read_is_an_error_not_an_absence() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("config");
        fs::write(&file, "x").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o000)).unwrap();
        // A user who can read it anyway (root) cannot show the error.
        if fs::File::open(&file).is_err() {
            let error = walked(dir.path(), "config", &|_| {}).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        }
    }

    #[test]
    fn a_change_past_the_first_mebibyte_of_a_shared_file_is_found() {
        let dir = tempfile::tempdir().unwrap();
        let mut bytes = vec![b'a'; SHARED_FILE_CAP as usize + 10];
        fs::write(dir.path().join("config"), &bytes).unwrap();
        let before = read_shared(dir.path()).unwrap();
        assert!(matches!(
            before.get(Path::new("config")),
            Some(Content::Large(len, _)) if *len == SHARED_FILE_CAP + 10
        ));
        assert_eq!(read_shared(dir.path()).unwrap(), before);
        *bytes.last_mut().unwrap() = b'b';
        fs::write(dir.path().join("config"), &bytes).unwrap();
        let after = read_shared(dir.path()).unwrap();
        assert_eq!(changed(&before, &after), [&PathBuf::from("config")]);
        // Within the cap, the bytes themselves are kept.
        fs::write(dir.path().join("config"), b"small").unwrap();
        assert_eq!(
            read_shared(dir.path()).unwrap().get(Path::new("config")),
            Some(&Content::File(b"small".to_vec()))
        );
    }

    /// Only Linux can make these names: APFS refuses what is not UTF-8.
    #[cfg(target_os = "linux")]
    #[test]
    fn names_that_are_not_utf8_are_keys_of_their_own() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let dir = tempfile::tempdir().unwrap();
        let hooks = dir.path().join("hooks");
        fs::create_dir(&hooks).unwrap();
        let odd = |bytes: &[u8]| OsStr::from_bytes(bytes).to_owned();
        fs::write(hooks.join(odd(b"x\xff")), "1").unwrap();
        let before = read_shared(dir.path()).unwrap();
        assert!(before.contains_key(&Path::new("hooks").join(odd(b"x\xff"))));
        // The lossy names of the two would collide.
        fs::write(hooks.join(odd(b"x\xfe")), "1").unwrap();
        let after = read_shared(dir.path()).unwrap();
        assert!(after.contains_key(&Path::new("hooks").join(odd(b"x\xfe"))));
        assert_eq!(changed(&before, &after).len(), 1);
        // Removing one shows as a changed key, never as an unchanged `Dir`.
        fs::remove_file(hooks.join(odd(b"x\xff"))).unwrap();
        let removed = read_shared(dir.path()).unwrap();
        assert_eq!(
            changed(&after, &removed),
            [&Path::new("hooks").join(odd(b"x\xff"))]
        );
    }

    #[test]
    fn a_ref_is_known_in_either_answer() {
        let running = Concurrent::default().running_branch("a");
        let ended = Concurrent::default().ended_branch("b", "tip");
        assert!(running.knows("refs/heads/a") && !running.knows("refs/heads/b"));
        assert!(ended.knows("refs/heads/b") && !ended.knows("refs/heads/a"));
        let none = Concurrent::default();
        assert!(known(&running, &none, "refs/heads/a"));
        assert!(known(&none, &ended, "refs/heads/b"));
        assert!(!known(&running, &ended, "refs/heads/c"));
    }

    /// An ended ref is held to its values only when it is running in
    /// neither answer and the later answer, knowing it, gives the same
    /// values; and it passes only at one of them, a symbolic ref at the
    /// same commit never (OWL-400).
    #[test]
    fn an_ended_ref_is_held_to_its_values_only_while_settled() {
        const NAME: &str = "refs/owlshift/tickets/OWL-2";
        let values = |values: &[Option<&str>]| -> BTreeSet<Option<String>> {
            values.iter().map(|v| v.map(str::to_owned)).collect()
        };
        let ended = |values: BTreeSet<Option<String>>| Concurrent {
            ended: BTreeMap::from([(NAME.to_owned(), values)]),
            ..Concurrent::default()
        };
        let running = Concurrent {
            running: BTreeSet::from([NAME.to_owned()]),
            ..Concurrent::default()
        };
        let held = values(&[Some("c1"), Some("c2")]);
        for (case, later, expected) in [
            ("unknown later", Concurrent::default(), true),
            ("same values later", ended(held.clone()), true),
            ("other values later", ended(values(&[Some("c3")])), false),
            ("running later", running.clone(), false),
        ] {
            assert_eq!(
                settled(NAME, &held, &ended(held.clone()), &later),
                expected,
                "{case}"
            );
        }
        assert!(!settled(NAME, &held, &running, &Concurrent::default()));

        let state = |value: Option<&str>| MainState {
            refs: value
                .map(|v| BTreeMap::from([(NAME.to_owned(), v.to_owned())]))
                .unwrap_or_default(),
            ..MainState::default()
        };
        let before = state(Some("c0"));
        let earlier = ended(values(&[Some("c1"), Some("c2"), None]));
        for (after, flagged) in [
            (Some("c1"), false),
            (Some("c2"), false),
            (None, false),
            (Some("c0"), true),
            (Some("c1 -> refs/remotes/origin/x"), true),
        ] {
            let found = before.changed_refs(&state(after), &earlier, &Concurrent::default());
            assert_eq!(!found.is_empty(), flagged, "{after:?}: {found:?}");
        }
        // Running in either answer, or ended only in the later one: left
        // out, whatever it holds.
        for (first, second) in [
            (running.clone(), Concurrent::default()),
            (Concurrent::default(), running.clone()),
            (Concurrent::default(), earlier.clone()),
        ] {
            assert_eq!(
                before.changed_refs(&state(Some("c9")), &first, &second),
                Vec::<String>::new()
            );
        }
    }

    const RUNNING: &str = "owlshift/running";
    const SHA: &str = "0123456789abcdef0123456789abcdef01234567\n";

    /// What `git worktree add` leaves for a worktree `name` on `branch`, as
    /// far as the check reads it, in a fake common folder.
    fn fake_entry(common: &Path, name: &str, branch: &str) -> PathBuf {
        let entry = common.join("worktrees").join(name);
        fs::create_dir_all(&entry).unwrap();
        fs::write(entry.join("gitdir"), format!("/elsewhere/{name}/.git\n")).unwrap();
        fs::write(entry.join("commondir"), "../..\n").unwrap();
        fs::write(entry.join("HEAD"), format!("ref: refs/heads/{branch}\n")).unwrap();
        fs::write(entry.join("index"), "index\n").unwrap();
        entry
    }

    /// A merge-backend rebase of `branch` under way: `head-name` written,
    /// then `HEAD` detached.
    fn start_rebase(entry: &Path, branch: &str) {
        fs::create_dir_all(entry.join("rebase-merge")).unwrap();
        fs::write(
            entry.join("rebase-merge/head-name"),
            format!("refs/heads/{branch}\n"),
        )
        .unwrap();
        fs::write(entry.join("HEAD"), SHA).unwrap();
    }

    /// The same rebase finished: `HEAD` back on the branch, then the state
    /// folder removed.
    fn finish_rebase(entry: &Path, branch: &str) {
        fs::write(entry.join("HEAD"), format!("ref: refs/heads/{branch}\n")).unwrap();
        let _ = fs::remove_dir_all(entry.join("rebase-merge"));
    }

    #[test]
    fn a_rebase_finishing_between_the_reads_still_belongs_to_its_branch() {
        // (1) `head-name` read, then the rebase finishes before `HEAD` is.
        let dir = tempfile::tempdir().unwrap();
        let entry = fake_entry(dir.path(), "other", "topic");
        start_rebase(&entry, "topic");
        let fired = std::cell::Cell::new(false);
        let found = owner(&entry, &|path| {
            if path.ends_with("HEAD") && !fired.replace(true) {
                finish_rebase(&entry, "topic");
            }
        });
        assert_eq!(found, Owner::Branch("topic".into()));

        // (2) No `head-name` at first; the rebase starts before `HEAD` is
        // read: the second read of `head-name` finds it.
        let dir = tempfile::tempdir().unwrap();
        let entry = fake_entry(dir.path(), "other", "topic");
        let fired = std::cell::Cell::new(false);
        let found = owner(&entry, &|_| {
            if !fired.replace(true) {
                start_rebase(&entry, "topic");
            }
        });
        assert_eq!(found, Owner::Branch("topic".into()));

        // (3) A detached `HEAD` and nothing else in the first round, and a
        // whole rebase between the reads: the second round reads `HEAD` on
        // its branch.
        let dir = tempfile::tempdir().unwrap();
        let entry = fake_entry(dir.path(), "other", "topic");
        fs::write(entry.join("HEAD"), SHA).unwrap();
        let fired = std::cell::Cell::new(false);
        let found = owner(&entry, &|path| {
            if path.ends_with("HEAD") && !fired.replace(true) {
                finish_rebase(&entry, "topic");
            }
        });
        assert_eq!(found, Owner::Branch("topic".into()));

        // (4) A detached `HEAD` that stays without a `head-name`: three
        // rounds of five reads, then no branch.
        let dir = tempfile::tempdir().unwrap();
        let entry = fake_entry(dir.path(), "other", "topic");
        fs::write(entry.join("HEAD"), SHA).unwrap();
        let reads = std::cell::Cell::new(0);
        let found = owner(&entry, &|_| reads.set(reads.get() + 1));
        assert_eq!(found, Owner::Detached);
        assert_eq!(reads.get(), 3 * 5);

        // A `head-name` of the apply backend counts as the merge backend's;
        // one that names no branch gives none.
        fs::create_dir(entry.join("rebase-apply")).unwrap();
        fs::write(entry.join("rebase-apply/head-name"), "refs/heads/applied\n").unwrap();
        assert_eq!(owner(&entry, &|_| {}), Owner::Branch("applied".into()));
        fs::write(entry.join("rebase-apply/head-name"), "detached HEAD\n").unwrap();
        assert_eq!(owner(&entry, &|_| {}), Owner::Detached);
    }

    #[test]
    fn an_entry_in_the_middle_of_an_apply_rebase_reads_as_detached() {
        // The `--apply` backend detaches `HEAD` before it writes any
        // `head-name` (checked live on 2026-10-19): the entry is caught with
        // a detached `HEAD` and a state folder without `head-name`. No
        // branch is guessed from `ORIG_HEAD` or the reflog, which a run can
        // write.
        let dir = tempfile::tempdir().unwrap();
        let entry = fake_entry(dir.path(), "other", "topic");
        fs::write(entry.join("HEAD"), SHA).unwrap();
        fs::write(entry.join("ORIG_HEAD"), SHA).unwrap();
        fs::create_dir(entry.join("rebase-apply")).unwrap();
        fs::write(entry.join("rebase-apply/applying"), "").unwrap();
        fs::write(entry.join("rebase-apply/orig-head"), SHA).unwrap();
        fs::create_dir(entry.join("logs")).unwrap();
        fs::write(
            entry.join("logs/HEAD"),
            "0 1 A <a@b> 0 +0000\trebase (start): checkout topic\n",
        )
        .unwrap();
        assert_eq!(owner(&entry, &|_| {}), Owner::Detached);

        // Once the backend has written `head-name`, the entry is its
        // branch's again.
        fs::write(entry.join("rebase-apply/head-name"), "refs/heads/topic\n").unwrap();
        assert_eq!(owner(&entry, &|_| {}), Owner::Branch("topic".into()));
    }

    #[test]
    fn an_entry_without_a_head_is_being_made_and_a_head_that_is_odd_is_not() {
        let dir = tempfile::tempdir().unwrap();
        let entry = fake_entry(dir.path(), "other", "topic");
        fs::remove_file(entry.join("HEAD")).unwrap();
        assert_eq!(owner(&entry, &|_| {}), Owner::Creating);
        // A `HEAD` of several lines, or a folder, is something else.
        fs::write(entry.join("HEAD"), "ref: refs/heads/a\nmore\n").unwrap();
        assert_eq!(owner(&entry, &|_| {}), Owner::Detached);
        fs::remove_file(entry.join("HEAD")).unwrap();
        fs::create_dir(entry.join("HEAD")).unwrap();
        assert_eq!(owner(&entry, &|_| {}), Owner::Detached);
        // Symbolic, but not to a branch.
        fs::remove_dir(entry.join("HEAD")).unwrap();
        fs::write(entry.join("HEAD"), "ref: refs/tags/v1\n").unwrap();
        assert_eq!(owner(&entry, &|_| {}), Owner::Detached);
    }

    /// A snapshot of `common` as taken before the run, by hand: the entries
    /// and their owners, with a run whose entry is `own`.
    fn snapshot_of(common: &Path) -> Snapshot {
        let own = OsString::from("own");
        let main = MainState {
            worktree_entries: read_worktree_entries(common, &own, &|_| {}).unwrap(),
            owners: read_owners(common, &own, &|_| {}).unwrap(),
            ..MainState::default()
        };
        Snapshot {
            common: common.to_owned(),
            own,
            main,
            start: String::new(),
            onto: None,
        }
    }

    fn concurrent(running: &[&str]) -> Concurrent {
        running.iter().fold(Concurrent::default(), |known, branch| {
            known.running_branch(branch)
        })
    }

    /// The violations of the entries between `before` and the folder as it
    /// is now, as `check_among` judges them (no git).
    fn judged(before: &Snapshot, earlier: &Concurrent, later: &Concurrent) -> Vec<String> {
        let after = MainState {
            worktree_entries: read_worktree_entries(&before.common, &before.own, &|_| {}).unwrap(),
            ..MainState::default()
        };
        let owners = before.owners_after(&after, earlier, later, &|_| {});
        before.main.changed_entries(&after, earlier, later, &owners)
    }

    /// An entry a ticket's run file names is left out by that name, in either
    /// answer, even on no branch, as an `--apply` rebase leaves it; one no
    /// answer names is still checked (OWL-400).
    #[test]
    fn an_entry_named_by_a_run_file_is_left_out_on_any_branch() {
        let dir = tempfile::tempdir().unwrap();
        let common = dir.path();
        fake_entry(common, "own", "owlshift/own");
        let named = fake_entry(common, "named", "owlshift/named");
        let other = fake_entry(common, "other", "owlshift/other");
        let before = snapshot_of(common);
        fs::write(named.join("HEAD"), SHA).unwrap();
        fs::write(named.join("index"), "staged\n").unwrap();
        fs::write(other.join("index"), "tampered\n").unwrap();
        let naming = Concurrent {
            entries: BTreeSet::from(["named".to_owned()]),
            ..Concurrent::default()
        };
        let none = Concurrent::default();
        for (earlier, later) in [(&naming, &none), (&none, &naming)] {
            assert_eq!(judged(&before, earlier, later), ["worktrees/other/index"]);
        }
        assert_eq!(
            judged(&before, &none, &none),
            [
                "worktrees/named/HEAD",
                "worktrees/named/index",
                "worktrees/other/index"
            ]
        );
    }

    #[test]
    fn the_entries_of_other_worktrees_are_checked_but_not_the_runs_own() {
        let dir = tempfile::tempdir().unwrap();
        let common = dir.path();
        let own = fake_entry(common, "own", "owlshift/own");
        let other = fake_entry(common, "other", "owlshift/other");
        let before = snapshot_of(common);
        let alone = Concurrent::default();
        assert_eq!(judged(&before, &alone, &alone), Vec::<String>::new());

        // The run's own entry is its to change.
        fs::write(own.join("index"), "staged\n").unwrap();
        fs::write(own.join("index.lock"), "").unwrap();
        assert_eq!(judged(&before, &alone, &alone), Vec::<String>::new());

        // Another's: an edit, a new file, a removal, a new entry.
        fs::write(other.join("index"), "tampered\n").unwrap();
        fs::write(other.join("planted"), "x").unwrap();
        fs::remove_file(other.join("gitdir")).unwrap();
        fake_entry(common, "evil", "owlshift/evil");
        let found = judged(&before, &alone, &alone);
        assert_eq!(
            found,
            [
                "worktrees/evil",
                "worktrees/evil/HEAD",
                "worktrees/evil/commondir",
                "worktrees/evil/gitdir",
                "worktrees/evil/index",
                "worktrees/other/gitdir",
                "worktrees/other/index",
                "worktrees/other/planted",
            ]
        );
    }

    #[test]
    fn a_concurrent_runs_entry_is_exempt_by_the_branch_it_had_at_the_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let common = dir.path();
        fake_entry(common, "own", "owlshift/own");
        let other = fake_entry(common, "other", "owlshift/other");
        let before = snapshot_of(common);
        let alone = Concurrent::default();
        assert_eq!(
            before.main.owners.get("other"),
            Some(&Owner::Branch("owlshift/other".into()))
        );

        // Told of that run, in either answer, whatever its files become.
        fs::write(other.join("index"), "its own work\n").unwrap();
        fs::write(other.join("planted"), "x").unwrap();
        let other_run = concurrent(&["owlshift/other"]);
        assert!(judged(&before, &other_run, &alone).is_empty());
        assert!(judged(&before, &alone, &other_run).is_empty());
        // Told of another run, or of none, it is a breach.
        let unrelated = concurrent(&["owlshift/unrelated"]);
        assert_eq!(judged(&before, &unrelated, &unrelated).len(), 2);
        assert_eq!(judged(&before, &alone, &alone).len(), 2);

        // A rewritten `HEAD` forges nothing: the entry was another
        // branch's, so naming a running branch now does not exempt it.
        fs::write(other.join("HEAD"), format!("ref: refs/heads/{RUNNING}\n")).unwrap();
        let running = concurrent(&[RUNNING]);
        assert_eq!(
            judged(&before, &running, &running),
            [
                "worktrees/other/HEAD",
                "worktrees/other/index",
                "worktrees/other/planted"
            ]
        );

        // An entry on no branch is never exempt, whoever is told.
        let dir = tempfile::tempdir().unwrap();
        let common = dir.path();
        fake_entry(common, "own", "owlshift/own");
        let parked = fake_entry(common, "parked", "x");
        fs::write(parked.join("HEAD"), SHA).unwrap();
        let before = snapshot_of(common);
        assert_eq!(before.main.owners.get("parked"), Some(&Owner::Detached));
        fs::write(parked.join("index"), "tampered\n").unwrap();
        fs::write(parked.join("HEAD"), format!("ref: refs/heads/{RUNNING}\n")).unwrap();
        assert_eq!(judged(&before, &running, &running).len(), 2);
    }

    #[test]
    fn an_entry_made_during_the_check_is_judged_by_its_owner_after_it() {
        let dir = tempfile::tempdir().unwrap();
        let common = dir.path();
        fake_entry(common, "own", "owlshift/own");
        let running = concurrent(&[RUNNING]);
        let alone = Concurrent::default();

        // Absent from the snapshot, made before the second answer: the run
        // registered between the two answers, `earlier` empty.
        let before = snapshot_of(common);
        fake_entry(common, "new", RUNNING);
        assert!(judged(&before, &alone, &running).is_empty());
        assert!(!judged(&before, &alone, &alone).is_empty());
        fake_entry(common, "stranger", "owlshift/stranger");
        assert!(
            judged(&before, &alone, &running)
                .iter()
                .all(|p| p.starts_with("worktrees/stranger"))
        );

        // Caught while it was being made: no `HEAD` yet at the snapshot.
        let dir = tempfile::tempdir().unwrap();
        let common = dir.path();
        fake_entry(common, "own", "owlshift/own");
        let making = fake_entry(common, "making", RUNNING);
        fs::remove_file(making.join("HEAD")).unwrap();
        let before = snapshot_of(common);
        assert_eq!(before.main.owners.get("making"), Some(&Owner::Creating));
        fs::write(making.join("HEAD"), format!("ref: refs/heads/{RUNNING}\n")).unwrap();
        assert!(judged(&before, &running, &running).is_empty());
        // ... naming an unknown branch it is a breach.
        fs::write(making.join("HEAD"), "ref: refs/heads/owlshift/unknown\n").unwrap();
        assert!(!judged(&before, &running, &running).is_empty());
    }

    #[test]
    fn a_run_alone_reads_no_owner_after_the_run() {
        let dir = tempfile::tempdir().unwrap();
        let common = dir.path();
        fake_entry(common, "own", "owlshift/own");
        let before = snapshot_of(common);
        fake_entry(common, "new", RUNNING);
        let after = MainState {
            worktree_entries: read_worktree_entries(common, &before.own, &|_| {}).unwrap(),
            ..MainState::default()
        };
        let alone = Concurrent::default();
        let owners = before.owners_after(&after, &alone, &alone, &|_| {
            panic!("an owner was read for a run alone")
        });
        assert!(owners.is_empty());
        // With a run known it is read: once the hook is called, it panics.
        let read = std::cell::Cell::new(false);
        let owners =
            before.owners_after(&after, &alone, &concurrent(&[RUNNING]), &|_| read.set(true));
        assert!(read.get());
        assert_eq!(owners.get("new"), Some(&Owner::Branch(RUNNING.into())));
    }

    #[test]
    fn a_file_that_vanishes_while_the_entries_are_read_is_no_violation() {
        let dir = tempfile::tempdir().unwrap();
        let common = dir.path();
        fake_entry(common, "own", "owlshift/own");
        let other = fake_entry(common, "other", "owlshift/other");
        // The snapshot holds no lock: it appears after, and goes while the
        // entries are read, in the same pass where `index` really changes.
        let before = snapshot_of(common);
        fs::write(other.join("index.lock"), "").unwrap();
        fs::write(other.join("index"), "tampered\n").unwrap();
        let gone = |path: &Path| {
            if path.ends_with("index.lock") {
                fs::remove_file(path).unwrap();
            }
        };
        let after = MainState {
            worktree_entries: read_worktree_entries(common, &before.own, &gone).unwrap(),
            ..MainState::default()
        };
        assert!(
            !after
                .worktree_entries
                .contains_key(&Path::new("worktrees/other/index.lock").to_owned())
        );
        let alone = Concurrent::default();
        // Only the real change is reported, not the lock that vanished.
        assert_eq!(
            before
                .main
                .changed_entries(&after, &alone, &alone, &BTreeMap::new()),
            ["worktrees/other/index"]
        );
    }

    /// A FIFO, a link or a file past the cap where `name` is, in `entry`.
    #[cfg(unix)]
    fn plant(entry: &Path, name: &str, kind: &str) {
        let path = entry.join(name);
        let _ = fs::remove_file(&path);
        match kind {
            "fifo" => {
                let status = Command::new("mkfifo").arg(&path).status().unwrap();
                assert!(status.success());
            }
            "link to a fifo" => {
                let fifo = entry.join("fifo");
                let _ = fs::remove_file(&fifo);
                let status = Command::new("mkfifo").arg(&fifo).status().unwrap();
                assert!(status.success());
                std::os::unix::fs::symlink(&fifo, &path).unwrap();
            }
            _ => fs::write(&path, vec![b'x'; LINK_FILE_CAP as usize * 2]).unwrap(),
        }
    }

    #[cfg(unix)]
    use std::process::Command;

    #[cfg(unix)]
    #[test]
    fn a_fifo_a_link_or_a_huge_file_as_head_never_blocks_the_check_or_gives_an_exemption() {
        let kinds = ["fifo", "link to a fifo", "a file past the cap"];
        for kind in kinds {
            for name in ["HEAD", "rebase-merge/head-name"] {
                let dir = tempfile::tempdir().unwrap();
                let entry = fake_entry(dir.path(), "other", "owlshift/other");
                fs::create_dir_all(entry.join("rebase-merge")).unwrap();
                plant(&entry, name, kind);
                let (sender, receiver) = std::sync::mpsc::channel();
                let path = entry.clone();
                std::thread::spawn(move || {
                    let _ = sender.send(owner(&path, &|_| {}));
                });
                let found = receiver
                    .recv_timeout(std::time::Duration::from_secs(10))
                    .unwrap_or_else(|_| panic!("{kind} as {name}: owner blocked"));
                let expected = if name == "HEAD" {
                    Owner::Detached
                } else {
                    // The plain `HEAD`'s answer: the planted file adds nothing.
                    Owner::Branch("owlshift/other".into())
                };
                assert_eq!(found, expected, "{kind} as {name}");
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_plant_as_head_is_judged_by_when_it_lands() {
        let running = concurrent(&[RUNNING]);
        let alone = Concurrent::default();
        for kind in ["fifo", "link to a fifo", "a file past the cap"] {
            // (i) Planted before the snapshot: the owner reads `Detached`, so
            // a later edit of `index` is a violation, a running branch told
            // or not.
            let dir = tempfile::tempdir().unwrap();
            let common = dir.path();
            fake_entry(common, "own", "owlshift/own");
            let other = fake_entry(common, "other", RUNNING);
            plant(&other, "HEAD", kind);
            let before = snapshot_of(common);
            assert_eq!(before.main.owners.get("other"), Some(&Owner::Detached));
            fs::write(other.join("index"), "tampered\n").unwrap();
            assert_eq!(
                judged(&before, &running, &running),
                ["worktrees/other/index"]
            );

            // (ii) With an empty `Concurrent`, as production passes it, an
            // entry whose `HEAD` was replaced after the snapshot is reported.
            let dir = tempfile::tempdir().unwrap();
            let common = dir.path();
            fake_entry(common, "own", "owlshift/own");
            let other = fake_entry(common, "other", "owlshift/other");
            let before = snapshot_of(common);
            plant(&other, "HEAD", kind);
            assert!(judged(&before, &alone, &alone).contains(&"worktrees/other/HEAD".to_owned()));

            // (iii) An entry absent from the snapshot, or still being made,
            // with the plant as its `HEAD`: not exempt, a run known or not.
            for making in [false, true] {
                let dir = tempfile::tempdir().unwrap();
                let common = dir.path();
                fake_entry(common, "own", "owlshift/own");
                let new = if making {
                    let entry = fake_entry(common, "new", RUNNING);
                    fs::remove_file(entry.join("HEAD")).unwrap();
                    Some(entry)
                } else {
                    None
                };
                let before = snapshot_of(common);
                let new = new.unwrap_or_else(|| fake_entry(common, "new", RUNNING));
                plant(&new, "HEAD", kind);
                let found = judged(&before, &running, &running);
                assert!(
                    found.contains(&"worktrees/new/HEAD".to_owned()),
                    "{kind}: {found:?}"
                );
            }

            // (iv) An existing entry whose snapshot owner is a running
            // branch stays exempt whatever is planted after the snapshot:
            // the documented limit, that the sandbox alone guards the entry
            // of a run in flight. Asserted as the limit, not as a detection.
            let dir = tempfile::tempdir().unwrap();
            let common = dir.path();
            fake_entry(common, "own", "owlshift/own");
            let other = fake_entry(common, "other", RUNNING);
            let before = snapshot_of(common);
            plant(&other, "HEAD", kind);
            fs::create_dir_all(other.join("rebase-merge")).unwrap();
            plant(&other, "rebase-merge/head-name", kind);
            fs::write(other.join("index"), "tampered\n").unwrap();
            assert!(judged(&before, &running, &running).is_empty(), "{kind}");

            // A plain `HEAD` forged to name a running branch beside a bad
            // `head-name`, on an entry that was another branch's, is still a
            // violation.
            let dir = tempfile::tempdir().unwrap();
            let common = dir.path();
            fake_entry(common, "own", "owlshift/own");
            let other = fake_entry(common, "other", "owlshift/other");
            let before = snapshot_of(common);
            fs::write(other.join("HEAD"), format!("ref: refs/heads/{RUNNING}\n")).unwrap();
            fs::create_dir_all(other.join("rebase-merge")).unwrap();
            plant(&other, "rebase-merge/head-name", kind);
            assert!(!judged(&before, &running, &running).is_empty(), "{kind}");
        }
    }
}
