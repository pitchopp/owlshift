//! One project's place in Owlshift's data directory, and the dedicated
//! checkout the ticket worktrees hang off (build plan, OWL-20).
//!
//! The executor's isolation check assumes nobody changes the main checkout
//! during a run, so `owlshift do` never uses the person's own checkout:
//! it clones the project's `origin` under the data directory once, fetches
//! it before each ticket, and creates every worktree from that clone. The
//! person keeps working in their checkout (scenario S12).
//!
//! ```text
//! <data>/projects/github.com/<owner>/<repo>/
//!     checkout/            the dedicated clone: the runs' main checkout
//!     worktrees/<ticket>/  one worktree per ticket
//!     runs/<ticket>/<run>/ brief.json, stdout.log, stderr.log, gate.log
//!     lock                 held by the one `owlshift do` working the project
//!     unverified           a run whose isolation check has not passed
//! ```
//!
//! A run that breaks isolation may leave something in the clone, such as a
//! hook or a configuration entry, that the next run's snapshot would take as
//! the norm and the runner's own git would run. So before each run the
//! `unverified` marker is written, and it is removed only once the run's
//! isolation check passed; a quarantine rewrites it with the violations. A
//! crash or a Ctrl-C in the middle of a run leaves it too. While it exists,
//! `owlshift do` refuses the project until a person looks.

use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use owlshift_adapters::forge::Repo;
use owlshift_contracts::ids::TicketId;

use crate::executor::Git;

/// The variables that point git at another repository, index or object
/// store than the one of its working directory. The runner's own git drops
/// them, so a `GIT_DIR` inherited from a hook or a script never turns a
/// command meant for the dedicated checkout onto another repository. The
/// ones that carry the person's authentication, such as `GIT_SSH_COMMAND`,
/// stay: the runner fetches and pushes with the person's own credentials.
pub const REPOSITORY_VARIABLES: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_COMMON_DIR",
    "GIT_NAMESPACE",
    "GIT_CEILING_DIRECTORIES",
    "GIT_DISCOVERY_ACROSS_FILESYSTEM",
    "GIT_PREFIX",
];

/// The runner's own git for `owlshift do`: the `git` on the `PATH`, with the
/// runner's environment less [`REPOSITORY_VARIABLES`].
pub fn runner_git() -> Git {
    Git::with_setup("git", drop_repository_variables)
}

fn drop_repository_variables(command: &mut Command) {
    for name in REPOSITORY_VARIABLES {
        command.env_remove(name);
    }
}

/// The URL of the `origin` remote of the repository at `root`.
pub fn origin_url(git: &Git, root: &Path) -> Result<String, String> {
    let url = git
        .run(root, &["remote", "get-url", "origin"])
        .map_err(|_| "the repository has no remote `origin` to clone from".to_owned())?;
    let url = String::from_utf8(url)
        .map_err(|_| "the URL of `origin` is not UTF-8 text".to_owned())?
        .trim()
        .to_owned();
    if url.is_empty() {
        return Err("the remote `origin` has no URL".to_owned());
    }
    Ok(url)
}

/// Where one project's files live under the data directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectDirs {
    root: PathBuf,
}

impl ProjectDirs {
    /// The directory of `repo`, a GitHub repository, under `data_dir`.
    pub fn new(data_dir: &Path, repo: &Repo) -> Self {
        Self {
            root: data_dir
                .join("projects")
                .join("github.com")
                .join(component(repo.owner()))
                .join(component(repo.name())),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The dedicated clone.
    pub fn checkout(&self) -> PathBuf {
        self.root.join("checkout")
    }

    /// Every ticket's worktree lives here.
    pub fn worktrees(&self) -> PathBuf {
        self.root.join("worktrees")
    }

    /// The ticket's worktree.
    pub fn worktree(&self, ticket: &TicketId) -> PathBuf {
        self.worktrees().join(component(ticket.as_str()))
    }

    /// The ticket's run directories live here.
    pub fn runs(&self, ticket: &TicketId) -> PathBuf {
        self.root.join("runs").join(component(ticket.as_str()))
    }

    fn lock_file(&self) -> PathBuf {
        self.root.join("lock")
    }

    /// The marker of a run whose isolation check has not passed.
    pub fn unverified_file(&self) -> PathBuf {
        self.root.join("unverified")
    }

    /// Takes the project's lock, held until the returned value is dropped or
    /// the process ends; `None` when another process holds it.
    pub fn lock(&self) -> io::Result<Option<ProjectLock>> {
        fs::create_dir_all(&self.root)?;
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(self.lock_file())?;
        match file.try_lock() {
            Ok(()) => Ok(Some(ProjectLock { _file: file })),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(error)) => Err(error),
        }
    }

    /// What the `unverified` marker says, when there is one.
    pub fn unverified(&self) -> io::Result<Option<String>> {
        match fs::read_to_string(self.unverified_file()) {
            Ok(text) => Ok(Some(text)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub(crate) fn mark_unverified(&self, text: &str) -> io::Result<()> {
        fs::write(self.unverified_file(), text)
    }

    pub(crate) fn clear_unverified(&self) -> io::Result<()> {
        match fs::remove_file(self.unverified_file()) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        }
    }
}

/// The project's lock: see [`ProjectDirs::lock`].
#[derive(Debug)]
pub struct ProjectLock {
    _file: File,
}

/// A path component every platform takes as is, and the same whatever the
/// case of the name: lower case, with `_` added after a Windows device name
/// (`con`, `nul`, `com1`…) or a trailing dot.
fn component(name: &str) -> String {
    let lower = name.to_ascii_lowercase();
    let stem = lower.split('.').next().unwrap_or_default();
    let numbered = |prefix: &str| {
        stem.strip_prefix(prefix)
            .is_some_and(|n| n.len() == 1 && n.as_bytes()[0].is_ascii_digit())
    };
    let device =
        matches!(stem, "con" | "prn" | "aux" | "nul") || numbered("com") || numbered("lpt");
    if device || lower.ends_with('.') {
        format!("{lower}_")
    } else {
        lower
    }
}

/// The branch new ticket branches start from, as the dedicated checkout
/// knows it after a fetch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Base {
    /// The remote-tracking name, such as `origin/main`.
    pub remote_ref: String,
    /// The branch on the forge, such as `main`: the pull request's base.
    pub branch: String,
}

/// Brings the dedicated checkout up to date: clones `remote_url` into it the
/// first time, and otherwise points its `origin` at `remote_url`, fetches,
/// pruning what the forge deleted, and reads the forge's default branch
/// again, since a project can change it. Worktrees whose folder was removed
/// are forgotten. Returns `origin`'s default branch, which a clone records
/// as `refs/remotes/origin/HEAD` (checked with git 2.54 and on GitHub on
/// 2026-09-29) and `git remote set-head origin --auto` refreshes.
///
/// Every command runs under the runner git's own deadline, 120 s at the
/// time of writing: a first clone of a very large repository can outlive
/// it (a known limit).
pub fn sync_checkout(git: &Git, dirs: &ProjectDirs, remote_url: &str) -> Result<Base, String> {
    let checkout = dirs.checkout();
    let failed = |e: crate::executor::GitError| e.to_string();
    if fs::symlink_metadata(&checkout).is_err() {
        fs::create_dir_all(dirs.root()).map_err(|e| format!("{}: {e}", dirs.root().display()))?;
        let args: [&OsStr; 5] = [
            "clone".as_ref(),
            "--quiet".as_ref(),
            "--".as_ref(),
            remote_url.as_ref(),
            "checkout".as_ref(),
        ];
        git.run(dirs.root(), &args).map_err(|e| {
            format!(
                "could not clone the project into {}: {}",
                checkout.display(),
                e.detail
            )
        })?;
    } else {
        let args: [&OsStr; 4] = [
            "remote".as_ref(),
            "set-url".as_ref(),
            "origin".as_ref(),
            remote_url.as_ref(),
        ];
        git.run(&checkout, &args).map_err(|e| {
            format!(
                "{} is not a usable clone ({}); remove it and run again to clone the project anew",
                checkout.display(),
                e.detail
            )
        })?;
        git.run(&checkout, &["fetch", "--quiet", "--prune", "origin"])
            .map_err(failed)?;
        set_head(git, &checkout)?;
    }
    git.run(&checkout, &["worktree", "prune"]).map_err(failed)?;

    let head = [
        "symbolic-ref",
        "--quiet",
        "--short",
        "refs/remotes/origin/HEAD",
    ];
    let remote_ref = match git.run(&checkout, &head) {
        Ok(name) => name,
        // A clone of a repository without a default branch records none.
        Err(_) => {
            set_head(git, &checkout)?;
            git.run(&checkout, &head).map_err(failed)?
        }
    };
    let remote_ref = String::from_utf8_lossy(&remote_ref).trim().to_owned();
    let branch = remote_ref
        .strip_prefix("origin/")
        .filter(|branch| !branch.is_empty())
        .ok_or_else(|| {
            format!("origin's default branch is {remote_ref:?}, not a branch of origin")
        })?
        .to_owned();
    Ok(Base { remote_ref, branch })
}

/// `git remote set-head origin --auto`: asks the forge for its default
/// branch and records it. A network call; its failure stops `owlshift do`
/// rather than leaving a stale base in place.
fn set_head(git: &Git, checkout: &Path) -> Result<(), String> {
    git.run(checkout, &["remote", "set-head", "origin", "--auto"])
        .map(drop)
        .map_err(|e| format!("could not read origin's default branch: {}", e.detail))
}

/// Checks that the worktree's `.git` still links it to the dedicated
/// checkout, before the runner trusts anything git says in it.
///
/// An agent owns every file of its worktree, `.git` included: pointing it,
/// or the `commondir` of its administrative folder, at a git directory of
/// its own would make git in the worktree read that directory's hooks and
/// configuration, which the isolation check never sees. So `.git` must be a
/// plain file whose `gitdir:` names a folder of `<checkout>/.git/worktrees`,
/// that folder's `commondir` must lead back to `<checkout>/.git`, and it
/// must hold no `config.worktree`. Paths are compared once resolved, links
/// included. No git command runs here.
pub fn check_worktree_link(checkout: &Path, worktree: &Path) -> Result<(), String> {
    let link = worktree.join(".git");
    let breach = |what: String| format!("the worktree's .git link was changed: {what}");
    let plain =
        |path: &Path| fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_file());
    if !plain(&link) {
        return Err(breach(format!("{} is not a plain file", link.display())));
    }
    let text = read_small(&link).map_err(|e| breach(format!("{}: {e}", link.display())))?;
    let target = text
        .trim()
        .strip_prefix("gitdir:")
        .map(str::trim)
        .filter(|target| !target.is_empty() && !target.contains('\n'))
        .ok_or_else(|| breach(format!("{} does not name a gitdir", link.display())))?;
    let resolve = |from: &Path, path: &str| {
        let path = Path::new(path);
        let path = if path.is_absolute() {
            path.to_owned()
        } else {
            from.join(path)
        };
        fs::canonicalize(&path).map_err(|e| breach(format!("{}: {e}", path.display())))
    };
    let admin = resolve(worktree, target)?;
    let common = fs::canonicalize(checkout.join(".git"))
        .map_err(|e| format!("{}: {e}", checkout.join(".git").display()))?;
    if admin.parent() != Some(common.join("worktrees").as_path()) {
        return Err(breach(format!(
            "it leads to {}, outside {}",
            admin.display(),
            common.join("worktrees").display()
        )));
    }
    let commondir = admin.join("commondir");
    if !plain(&commondir) {
        return Err(breach(format!(
            "{} is not a plain file",
            commondir.display()
        )));
    }
    let named =
        read_small(&commondir).map_err(|e| breach(format!("{}: {e}", commondir.display())))?;
    if resolve(&admin, named.trim())? != common {
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

/// A small text file: a git link names one path.
fn read_small(path: &Path) -> io::Result<String> {
    use std::io::Read;
    let mut text = String::new();
    File::open(path)?.take(4096).read_to_string(&mut text)?;
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_folders_are_portable_and_case_blind() {
        let data = Path::new("data");
        let dirs = ProjectDirs::new(data, &Repo::parse("Pitchopp/OwlShift").unwrap());
        assert_eq!(
            dirs.root(),
            Path::new("data/projects/github.com/pitchopp/owlshift")
        );
        let ticket = TicketId::new("OWL-12").unwrap();
        assert_eq!(dirs.worktree(&ticket), dirs.root().join("worktrees/owl-12"));
        assert_eq!(dirs.runs(&ticket), dirs.root().join("runs/owl-12"));

        for (name, expected) in [
            ("CON", "con_"),
            ("nul.txt", "nul.txt_"),
            ("com1", "com1_"),
            ("LPT9", "lpt9_"),
            ("com10", "com10"),
            ("console", "console"),
            ("name.", "name._"),
            ("owlshift", "owlshift"),
        ] {
            assert_eq!(component(name), expected, "{name}");
        }
    }

    /// What `git worktree add` leaves, built by hand: a checkout's
    /// administrative folder for the worktree, and the worktree's `.git`.
    fn linked(dir: &Path) -> (PathBuf, PathBuf, PathBuf) {
        let checkout = dir.join("checkout");
        let admin = checkout.join(".git").join("worktrees").join("owl-1");
        let worktree = dir.join("worktrees").join("owl-1");
        fs::create_dir_all(&admin).unwrap();
        fs::create_dir_all(&worktree).unwrap();
        fs::write(admin.join("commondir"), "../..\n").unwrap();
        fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", admin.display()),
        )
        .unwrap();
        (checkout, admin, worktree)
    }

    #[test]
    fn a_worktree_must_stay_linked_to_the_checkout() {
        let dir = tempfile::tempdir().unwrap();
        let (checkout, admin, worktree) = linked(dir.path());
        assert_eq!(check_worktree_link(&checkout, &worktree), Ok(()));

        // A relative gitdir, as `worktree.useRelativePaths` writes it.
        fs::write(
            worktree.join(".git"),
            "gitdir: ../../checkout/.git/worktrees/owl-1\n",
        )
        .unwrap();
        assert_eq!(check_worktree_link(&checkout, &worktree), Ok(()));

        drop(admin);

        for case in [
            "a gitdir elsewhere",
            "no gitdir line",
            "a .git folder",
            "a commondir elsewhere",
            "a per-worktree configuration",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let (checkout, admin, worktree) = linked(dir.path());
            // A git directory of the agent's own, with the same layout.
            let evil = dir.path().join("evil");
            let evil_admin = evil.join("worktrees").join("owl-1");
            fs::create_dir_all(&evil_admin).unwrap();
            fs::write(evil_admin.join("commondir"), "../..\n").unwrap();
            let link = worktree.join(".git");
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
                _ => fs::write(admin.join("config.worktree"), "[core]\n\tfsmonitor = x\n").unwrap(),
            }
            let error = check_worktree_link(&checkout, &worktree).unwrap_err();
            assert!(
                error.contains("the worktree's .git link was changed"),
                "{case}: {error}"
            );
        }
    }

    #[test]
    fn one_do_at_a_time_per_project() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = ProjectDirs::new(dir.path(), &Repo::parse("demo/project").unwrap());
        let held = dirs.lock().unwrap().expect("the first lock is free");
        assert!(dirs.lock().unwrap().is_none(), "a second lock waits");
        drop(held);
        assert!(dirs.lock().unwrap().is_some(), "free again once released");
    }

    #[test]
    fn the_marker_is_written_read_and_cleared() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = ProjectDirs::new(dir.path(), &Repo::parse("demo/project").unwrap());
        fs::create_dir_all(dirs.root()).unwrap();
        assert_eq!(dirs.unverified().unwrap(), None);
        dirs.mark_unverified("run r1 of OWL-1").unwrap();
        assert_eq!(
            dirs.unverified().unwrap().as_deref(),
            Some("run r1 of OWL-1")
        );
        dirs.clear_unverified().unwrap();
        dirs.clear_unverified().unwrap();
        assert_eq!(dirs.unverified().unwrap(), None);
    }

    #[test]
    fn the_runner_git_drops_what_points_it_elsewhere() {
        let mut command = Command::new("git");
        command
            .env("GIT_DIR", "/elsewhere/.git")
            .env("GIT_INDEX_FILE", "/elsewhere/index")
            .env("GIT_SSH_COMMAND", "ssh -i key");
        drop_repository_variables(&mut command);
        let envs: Vec<_> = command.get_envs().collect();
        for name in REPOSITORY_VARIABLES {
            assert!(
                envs.contains(&(OsStr::new(name), None)),
                "{name} is not removed"
            );
        }
        assert!(envs.contains(&(
            OsStr::new("GIT_SSH_COMMAND"),
            Some(OsStr::new("ssh -i key"))
        )));
    }
}
