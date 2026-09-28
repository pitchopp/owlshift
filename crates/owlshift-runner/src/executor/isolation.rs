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
//!   checkout later.
//! - **Diff inside the worktree**: no ref of the repository was created,
//!   moved or deleted but the run's branch (tags, other branches,
//!   `refs/stash`); remote-tracking refs are left out, a fetch being
//!   harmless.
//! - **Expected branch**: the worktree's HEAD is still the run's branch,
//!   and its commit descends from the one the run started from.
//!
//! A check that cannot run counts as a violation. Nothing is written: git
//! runs with `--no-optional-locks`, and `hash-object` without `-w`.
//!
//! It assumes nobody else changes the main checkout during the run: a
//! person editing there looks like a breach. Known limits: changes outside
//! the repository, a file inside an ignored directory, and an index flag
//! (`assume-unchanged`, `skip-worktree`) hiding an edit go unseen; confining
//! the agent is the operating system's job.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use super::git::{Git, GitError};

/// The most bytes read from one shared git file; a larger one is compared
/// by its size and first bytes.
const SHARED_FILE_CAP: u64 = 1024 * 1024;

/// The most names a violation lists before summing up the rest.
const LISTED: usize = 10;

/// What the run must leave alone, as it was before the run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Snapshot {
    main: MainState,
    /// The worktree's commit when the run started.
    start: String,
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
    shared: BTreeMap<String, Content>,
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
    /// A shared file past the cap: its size and first bytes.
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
    /// deleted.
    Refs(Vec<String>),
    /// The worktree is no longer on the run's branch.
    Branch {
        expected: String,
        found: Option<String>,
    },
    /// The branch no longer descends from the run's start commit.
    History { start: String, end: String },
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
            Self::History { start, end } => write!(
                f,
                "the branch is at {end}, which does not descend from the run's start {start}"
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
    /// on `branch`.
    pub fn take(
        git: &Git,
        main: &Path,
        worktree: &Path,
        branch: &str,
    ) -> Result<Self, SnapshotError> {
        let main = MainState::read(git, main, branch)?;
        let start = trimmed(&git.run(worktree, &["rev-parse", "--verify", "HEAD"])?);
        Ok(Self { main, start })
    }

    /// Everything the run changed that it had to leave alone; empty when
    /// isolation held.
    pub fn check(&self, git: &Git, main: &Path, worktree: &Path, branch: &str) -> Vec<Violation> {
        let mut violations = match MainState::read(git, main, branch) {
            Ok(after) => self.main.compare(&after),
            Err(error) => vec![Violation::CheckFailed(error.to_string())],
        };
        if let Err(error) = self.check_branch(git, worktree, branch, &mut violations) {
            violations.push(Violation::CheckFailed(error.to_string()));
        }
        violations
    }

    fn check_branch(
        &self,
        git: &Git,
        worktree: &Path,
        branch: &str,
        violations: &mut Vec<Violation>,
    ) -> Result<(), SnapshotError> {
        let expected = format!("refs/heads/{branch}");
        let found = symbolic_head(git, worktree)?;
        if found.as_deref() != Some(expected.as_str()) {
            violations.push(Violation::Branch { expected, found });
            return Ok(());
        }
        let end = trimmed(&git.run(worktree, &["rev-parse", "--verify", "HEAD"])?);
        let ancestry = git.output(
            worktree,
            &[
                "merge-base",
                "--is-ancestor",
                self.start.as_str(),
                end.as_str(),
            ],
            None,
        )?;
        match ancestry.status.code() {
            Some(0) => Ok(()),
            Some(1) => {
                violations.push(Violation::History {
                    start: self.start.clone(),
                    end,
                });
                Ok(())
            }
            _ => Err(SnapshotError(format!(
                "git merge-base failed with {}",
                ancestry.status
            ))),
        }
    }
}

impl MainState {
    fn read(git: &Git, main: &Path, branch: &str) -> Result<Self, SnapshotError> {
        let head = git.output(main, &["rev-parse", "-q", "--verify", "HEAD"], None)?;
        let head = match head.status.code() {
            Some(0) => Some(trimmed(&head.stdout)),
            Some(1) => None,
            _ => {
                return Err(SnapshotError(format!(
                    "git rev-parse HEAD failed with {}",
                    head.status
                )));
            }
        };
        let branch_now = symbolic_head(git, main)?;

        let status = git.run(
            main,
            &[
                "--no-optional-locks",
                "status",
                "--porcelain=v2",
                "-z",
                "--untracked-files=all",
                "--ignored=matching",
            ],
        )?;
        let records = status_records(&status).map_err(SnapshotError)?;
        let entries = contents(git, main, records)?;

        let common = trimmed(&git.run(main, &["rev-parse", "--git-common-dir"])?);
        let common = main.join(common);
        let mut shared = BTreeMap::new();
        for name in ["config", "config.worktree", "hooks", "info"] {
            shared_files(&common, name, &mut shared)
                .map_err(|e| SnapshotError(format!("{}: {e}", common.join(name).display())))?;
        }

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

    fn compare(&self, after: &Self) -> Vec<Violation> {
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
            .cloned()
            .collect();
        if !shared.is_empty() {
            violations.push(Violation::SharedGitFiles(shared));
        }
        let refs: Vec<String> = changed(&self.refs, &after.refs)
            .into_iter()
            .cloned()
            .collect();
        if !refs.is_empty() {
            violations.push(Violation::Refs(refs));
        }
        violations
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
    match head.status.code() {
        Some(0) => Ok(Some(trimmed(&head.stdout))),
        Some(1) => Ok(None),
        _ => Err(SnapshotError(format!(
            "git symbolic-ref HEAD in {} failed with {}: {}",
            dir.display(),
            head.status,
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
    if !hashed.status.success() {
        return Err(SnapshotError(format!(
            "git hash-object failed with {}: {}",
            hashed.status,
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

/// Records `name` under `common`, and everything below it for a
/// directory, without following links.
fn shared_files(common: &Path, name: &str, out: &mut BTreeMap<String, Content>) -> io::Result<()> {
    let path = common.join(name);
    let meta = match fs::symlink_metadata(&path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let content = if meta.file_type().is_symlink() {
        Content::Link(fs::read_link(&path)?)
    } else if meta.is_dir() {
        let mut children: Vec<String> = fs::read_dir(&path)?
            .map(|entry| entry.map(|entry| entry.file_name().to_string_lossy().into_owned()))
            .collect::<io::Result<_>>()?;
        children.sort();
        for child in children {
            shared_files(common, &format!("{name}/{child}"), out)?;
        }
        Content::Dir
    } else if meta.is_file() {
        let mut bytes = Vec::new();
        fs::File::open(&path)?
            .take(SHARED_FILE_CAP + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > SHARED_FILE_CAP {
            bytes.truncate(SHARED_FILE_CAP as usize);
            Content::Large(meta.len(), bytes)
        } else {
            Content::File(bytes)
        }
    } else {
        Content::Other
    };
    out.insert(name.to_owned(), content);
    Ok(())
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

    #[test]
    fn changed_keys_cover_edits_additions_and_removals() {
        let before = BTreeMap::from([("a", 1), ("b", 2), ("c", 3)]);
        let after = BTreeMap::from([("a", 1), ("b", 9), ("d", 4)]);
        assert_eq!(changed(&before, &after), [&"b", &"c", &"d"]);
    }
}
