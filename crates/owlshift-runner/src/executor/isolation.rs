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
//! Runs in flight at the same time move their own branches, so
//! [`Snapshot::check_among`] is told which ([`Concurrent`]), just before and
//! just after it reads the refs: a branch running in either answer is left
//! out, that run's own check guarding it, and a branch that had already
//! ended in the first answer must still be at the commit its own check
//! verified ([`Checked::tip`]). Everything else is checked as for
//! a run alone, and a breach there quarantines every run that sees it: the
//! check cannot tell which run did it. [`Snapshot::check`] is the check of a
//! run alone, told of no other run.
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
//! not made. The runner's git runs no hook and no file-system monitor
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
use std::ffi::OsString;
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
    main: MainState,
    /// The worktree's commit when the run started.
    start: String,
    /// For a rebase, the new base, as the commit it was before the run.
    onto: Option<String>,
}

/// The other runs in flight at any time since a snapshot, as the scheduler
/// knows them when [`Snapshot::check_among`] asks: just before and just
/// after it reads the refs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Concurrent {
    /// The branches of runs still in flight. A change to one is left out:
    /// that run's own check guards its branch.
    pub running: BTreeSet<String>,
    /// The branches of runs that ended since the snapshot, each with the
    /// commit its own check verified ([`Checked::tip`]). A branch that had
    /// already ended before the refs were read, and did not run again
    /// since, must still be there.
    pub ended: BTreeMap<String, String>,
}

impl Concurrent {
    /// Whether `branch` is a run in flight or one that ended since the
    /// snapshot, in this answer.
    pub fn knows(&self, branch: &str) -> bool {
        self.running.contains(branch) || self.ended.contains_key(branch)
    }
}

/// Whether either of the scheduler's two answers knows `branch`.
fn known(earlier: &Concurrent, later: &Concurrent, branch: &str) -> bool {
    earlier.knows(branch) || later.knows(branch)
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

#[derive(Clone, Debug, PartialEq, Eq)]
struct MainState {
    /// The main checkout's HEAD commit; `None` on an unborn branch.
    head: Option<String>,
    /// The checked-out branch; `None` when HEAD is detached.
    branch: Option<String>,
    /// `git status` entries by path.
    entries: BTreeMap<Vec<u8>, Entry>,
    /// The shared git files, by path relative to the common directory.
    shared: BTreeMap<PathBuf, Content>,
    /// Every ref but the run's branch and remote-tracking refs, with its
    /// object.
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
    /// A shared file past the cap: its size and the SHA-256 of the whole
    /// file, so a change anywhere in it shows.
    Large(u64, Vec<u8>),
    Link(PathBuf),
    Dir,
    Missing,
    /// A FIFO, a socket, a device.
    Other,
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
        let main = MainState::read(git, main, branch, &common)?;
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
        self.check_among(git, main, worktree, branch, &Concurrent::default)
            .violations
    }

    /// Checks a run among others in flight at the same time. `concurrent`
    /// is asked twice, just before and just after the refs are read. The
    /// second answer knows a run registered before it created its branch,
    /// even when it started during the check; the first tells a run that
    /// had already ended from one that ended during the check, after its
    /// branch was read.
    pub fn check_among(
        &self,
        git: &Git,
        main: &Path,
        worktree: &Path,
        branch: &str,
        concurrent: &dyn Fn() -> Concurrent,
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
        let earlier = concurrent();
        let mut violations = match MainState::read(git, main, branch, &self.common) {
            Ok(after) => self.main.compare(&after, &earlier, &concurrent()),
            Err(error) => vec![Violation::CheckFailed(error.to_string())],
        };
        let tip = self
            .check_branch(git, worktree, branch, &mut violations)
            .unwrap_or_else(|error| {
                violations.push(Violation::CheckFailed(error.to_string()));
                None
            });
        Checked { violations, tip }
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
    fn read(git: &Git, main: &Path, branch: &str, common: &Path) -> Result<Self, SnapshotError> {
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

        let own = format!("refs/heads/{branch}");
        let listed = git.run(main, &["for-each-ref", "--format=%(objectname) %(refname)"])?;
        let refs = String::from_utf8_lossy(&listed)
            .lines()
            .filter_map(|line| line.split_once(' '))
            .filter(|(_, name)| *name != own && !name.starts_with("refs/remotes/"))
            .map(|(object, name)| (name.to_owned(), object.to_owned()))
            .collect();

        Ok(Self {
            head,
            branch: branch_now,
            entries,
            shared,
            refs,
        })
    }

    fn compare(&self, after: &Self, earlier: &Concurrent, later: &Concurrent) -> Vec<Violation> {
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
        let refs = self.changed_refs(after, earlier, later);
        if !refs.is_empty() {
            violations.push(Violation::Refs(refs));
        }
        violations
    }

    /// The refs that changed, but concurrent runs' branches, known from the
    /// scheduler's answers just before (`earlier`) and just after (`later`)
    /// the refs were read. A branch running in either answer is left out,
    /// and so is one that ran again between them (its tip changed) or that
    /// only the later answer knows: the refs may hold it at any point of
    /// that run. A branch that had already ended in the earlier answer
    /// counts when it is not at its tip, whether it changed since the
    /// snapshot or not.
    fn changed_refs(&self, after: &Self, earlier: &Concurrent, later: &Concurrent) -> Vec<String> {
        let mut refs: Vec<String> = changed(&self.refs, &after.refs)
            .into_iter()
            .filter(|name| {
                !name
                    .strip_prefix("refs/heads/")
                    .is_some_and(|branch| known(earlier, later, branch))
            })
            .cloned()
            .collect();
        for (branch, tip) in &earlier.ended {
            let settled = !earlier.running.contains(branch)
                && !later.running.contains(branch)
                && later
                    .ended
                    .get(branch)
                    .is_none_or(|later_tip| later_tip == tip);
            let name = format!("refs/heads/{branch}");
            if settled && after.refs.get(&name) != Some(tip) {
                refs.push(name);
            }
        }
        refs.sort();
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
    for name in ["commondir", "config", "config.worktree", "hooks", "info"] {
        walk(common, Path::new(name), &mut shared, &|_| {})
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
/// any other I/O error is returned. `between` is called with the path after
/// its metadata is read and before it is opened, which lets a test change
/// the file at exactly that point.
fn walk(
    common: &Path,
    rel: &Path,
    out: &mut BTreeMap<PathBuf, Content>,
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
            walk(common, &rel.join(child), out, between)?;
        }
        Content::Dir
    } else if meta.is_file() {
        between(&path);
        let Some(mut file) = present(fs::File::open(&path))? else {
            return Ok(());
        };
        let mut bytes = Vec::new();
        (&mut file)
            .take(SHARED_FILE_CAP + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > SHARED_FILE_CAP {
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
            Content::Large(len, hasher.finalize().to_vec())
        } else {
            Content::File(bytes)
        }
    } else {
        Content::Other
    };
    out.insert(rel.to_owned(), content);
    Ok(())
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
        walk(common, Path::new(name), &mut out, between)?;
        Ok(out.into_keys().collect())
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
    fn a_branch_is_known_in_either_answer() {
        let running = Concurrent {
            running: BTreeSet::from(["a".to_owned()]),
            ended: BTreeMap::new(),
        };
        let ended = Concurrent {
            running: BTreeSet::new(),
            ended: BTreeMap::from([("b".to_owned(), "tip".to_owned())]),
        };
        assert!(running.knows("a") && !running.knows("b"));
        assert!(ended.knows("b") && !ended.knows("a"));
        let none = Concurrent::default();
        assert!(known(&running, &none, "a"));
        assert!(known(&none, &ended, "b"));
        assert!(!known(&running, &ended, "c"));
    }
}
