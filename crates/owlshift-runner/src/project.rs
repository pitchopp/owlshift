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
//!     unfinished-clone     a first clone that has not finished
//! ```
//!
//! A first clone stopped part-way, by its deadline, a crash or a Ctrl-C,
//! leaves a folder that looks like a checkout. So the clone is marked
//! `unfinished-clone` until it succeeds, and while the marker exists, the
//! next `owlshift do` removes that folder and clones anew (OWL-80). Unlike
//! `unverified`, it needs no person: a clone runs nothing of the project's.
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
use std::thread;
use std::time::{Duration, Instant};

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
    origin_text(url)
}

/// The URL in what `git remote get-url origin` printed.
pub fn origin_text(url: Vec<u8>) -> Result<String, String> {
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
    ///
    /// The lock belongs to an open file description (`flock` on Unix), which
    /// a child forked by any thread of the process shares until it execs.
    /// The standard library forks, rather than using `posix_spawn`, for a
    /// command that sets `PATH` and names a bare program, as the gate's
    /// `sh` and the agent's `git` do. So a lock just released can still look
    /// held for the few moments a child forked by another thread takes to
    /// exec. `owlshift do` takes it once per process, so this matters only
    /// where one process runs several `do` in threads: the tests, which run
    /// them one at a time.
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
        remove_file_if_any(&self.unverified_file())
    }

    /// The marker of a first clone that has not finished (OWL-80).
    pub fn unfinished_clone_file(&self) -> PathBuf {
        self.root.join("unfinished-clone")
    }
}

/// Removes the file at `path`, if there is one.
fn remove_file_if_any(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

/// Whether anything is at `path`, a symbolic link included. Only a missing
/// entry means no: any other error is returned, never taken for absence.
fn exists(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
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
    /// The remote-tracking name, such as `origin/main`, for messages and
    /// events only: a run can move a remote-tracking ref (OWL-51), so a later
    /// step is given `commit`, never this name.
    pub remote_ref: String,
    /// The branch on the forge, such as `main`: the pull request's base.
    pub branch: String,
    /// The commit `refs/remotes/origin/<branch>` points to right after the
    /// fetch, resolved once through that full name: what new branches start
    /// from and what the project's rules are read at.
    pub commit: String,
}

/// How long the first clone of a project may take (OWL-60): a large
/// repository's whole history crosses the network once, and can outlive the
/// runner git's 120 s deadline on a slow link. The fetch of a later `do` is
/// incremental and keeps the short deadline, so a stalled network still stops
/// it fast.
pub const CLONE_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// What the `unfinished-clone` marker says to a person who finds it.
const UNFINISHED_CLONE: &str = "A first clone of the project started and has not finished: \
     the next `owlshift do` removes `checkout` and clones the project anew.\n";

/// How long removing a clone that did not finish keeps trying (OWL-80). On
/// Windows, the files of a git just stopped can stay locked for a moment
/// after its Job Object is terminated; on any platform, a child still dying
/// can add a file while the folder is emptied.
const REMOVAL_BUDGET: Duration = Duration::from_secs(10);

/// Removes the folder of a clone that did not finish, if there is one,
/// trying again with a growing pause until `budget` has passed; the error is
/// the last attempt's.
fn remove_partial_clone(checkout: &Path, budget: Duration) -> io::Result<()> {
    let deadline = Instant::now() + budget;
    let mut pause = Duration::from_millis(50);
    loop {
        let error = match fs::remove_dir_all(checkout) {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => error,
        };
        if Instant::now() + pause > deadline {
            return Err(error);
        }
        thread::sleep(pause);
        pause = (pause * 2).min(Duration::from_secs(1));
    }
}

/// Brings the dedicated checkout up to date: clones `remote_url` into it the
/// first time, and otherwise points its `origin` at `remote_url`, fetches,
/// pruning what the forge deleted, and reads the forge's default branch
/// again, since a project can change it. Worktrees whose folder was removed
/// are forgotten. Returns `origin`'s default branch, which a clone records
/// as `refs/remotes/origin/HEAD` (checked with git 2.54 and on GitHub on
/// 2026-09-29) and `git remote set-head origin --auto` refreshes, with the
/// commit it points to after the fetch.
///
/// The first clone runs under [`CLONE_TIMEOUT`]; every other command runs
/// under the runner git's own deadline, 120 s. It is marked `unfinished-clone`
/// until it succeeds (OWL-80): a failed clone's folder is removed, trying
/// again for up to `REMOVAL_BUDGET`, and while the marker exists, the next
/// call removes whatever is left in `checkout` before cloning anew, or
/// refuses when it cannot. The marker relies on the project lock, which
/// `owlshift do` holds around this call: no other clone of the project runs.
pub fn sync_checkout(git: &Git, dirs: &ProjectDirs, remote_url: &str) -> Result<Base, String> {
    sync_checkout_within(git, dirs, remote_url, CLONE_TIMEOUT)
}

/// [`sync_checkout`] with `clone_timeout` as the first clone's deadline: for
/// a test that cannot wait [`CLONE_TIMEOUT`].
pub fn sync_checkout_within(
    git: &Git,
    dirs: &ProjectDirs,
    remote_url: &str,
    clone_timeout: Duration,
) -> Result<Base, String> {
    let checkout = dirs.checkout();
    let marker = dirs.unfinished_clone_file();
    let failed = |e: crate::executor::GitError| e.to_string();
    let io_failed = |path: &Path, e: io::Error| format!("{}: {e}", path.display());
    if exists(&marker).map_err(|e| io_failed(&marker, e))? {
        remove_partial_clone(&checkout, REMOVAL_BUDGET).map_err(|e| {
            format!(
                "{} holds a clone of the project that did not finish, and it could not be \
                 removed ({e}); remove it and run again",
                checkout.display()
            )
        })?;
    }
    if !exists(&checkout).map_err(|e| io_failed(&checkout, e))? {
        fs::create_dir_all(dirs.root()).map_err(|e| io_failed(dirs.root(), e))?;
        fs::write(&marker, UNFINISHED_CLONE).map_err(|e| io_failed(&marker, e))?;
        let args: [&OsStr; 5] = [
            "clone".as_ref(),
            "--quiet".as_ref(),
            "--".as_ref(),
            remote_url.as_ref(),
            "checkout".as_ref(),
        ];
        git.run_within(dirs.root(), &args, clone_timeout)
            .map_err(|e| {
                let mut message = format!(
                    "could not clone the project into {}: {}",
                    checkout.display(),
                    e.detail
                );
                // The marker stays whatever happens here, so the next run
                // removes what is left rather than taking it for the
                // project's checkout.
                if let Err(error) = remove_partial_clone(&checkout, REMOVAL_BUDGET) {
                    message.push_str(&format!(
                        "; the partial clone could not be removed ({error}), the next run removes it"
                    ));
                }
                message
            })?;
        remove_file_if_any(&marker).map_err(|e| io_failed(&marker, e))?;
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

    // The full name: `--short` would answer `remotes/origin/main` when a
    // local branch `origin/main` exists (git 2.54, 2026-09-29). One level
    // only: `set-head` has just written it.
    let head = [
        "symbolic-ref",
        "--quiet",
        "--no-recurse",
        "refs/remotes/origin/HEAD",
    ];
    let target = match git.run(&checkout, &head) {
        Ok(name) => name,
        // A clone of a repository without a default branch records none.
        Err(_) => {
            set_head(git, &checkout)?;
            git.run(&checkout, &head).map_err(failed)?
        }
    };
    let target = String::from_utf8_lossy(&target).trim().to_owned();
    let branch = target
        .strip_prefix("refs/remotes/origin/")
        .filter(|branch| !branch.is_empty())
        .ok_or_else(|| format!("origin's default branch is {target:?}, not a branch of origin"))?
        .to_owned();
    // Resolved now, right after the fetch reset every remote-tracking ref a
    // run could have moved, and by its full name, which no local branch
    // shadows (OWL-51). A run can also make that ref symbolic, pointing at
    // another branch of origin: the fetch keeps such a ref, and git follows
    // it (git 2.54, 2026-09-29), so it is refused.
    let symbolic = git
        .output(
            &checkout,
            &["symbolic-ref", "--quiet", target.as_str()],
            None,
        )
        .map_err(failed)?
        .success();
    if symbolic {
        return Err(format!(
            "{target} in {} is a symbolic ref, not a branch fetched from origin; \
             remove it with `git update-ref --no-deref -d {target}` there and run again",
            checkout.display()
        ));
    }
    let full = format!("{target}^{{commit}}");
    let commit = git
        .run(
            &checkout,
            &["rev-parse", "--verify", "--end-of-options", full.as_str()],
        )
        .map_err(|e| format!("origin's default branch {target} does not resolve: {e}"))?;
    let commit = String::from_utf8_lossy(&commit).trim().to_owned();
    Ok(Base {
        remote_ref: format!("origin/{branch}"),
        branch,
        commit,
    })
}

/// `git remote set-head origin --auto`: asks the forge for its default
/// branch and records it. A network call; its failure stops `owlshift do`
/// rather than leaving a stale base in place.
fn set_head(git: &Git, checkout: &Path) -> Result<(), String> {
    git.run(checkout, &["remote", "set-head", "origin", "--auto"])
        .map(drop)
        .map_err(|e| format!("could not read origin's default branch: {}", e.detail))
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

    /// The lock is checked in a fresh copy of this test binary, running
    /// [`helper_one_do_at_a_time_per_project`] alone. In this process, other
    /// tests fork children (`git`, `sh`), and a child forked while the lock
    /// is held keeps it until it execs (see [`ProjectDirs::lock`]): the
    /// released lock then looked held (seen once on macOS on 2026-09-29).
    /// Alone, nothing forks.
    #[test]
    fn one_do_at_a_time_per_project() {
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "project::tests::helper_one_do_at_a_time_per_project",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.contains("running 1 test"),
            "{output:?}"
        );
    }

    #[test]
    #[ignore = "helper, run by the test above"]
    fn helper_one_do_at_a_time_per_project() {
        if !std::env::args().any(|arg| arg == "--exact") {
            return;
        }
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

    /// A partial clone that stays in the way is reported once the budget is
    /// spent, not waited on forever: on Unix, a file where the folder should
    /// be; on Windows, a file inside it held open without delete sharing.
    #[test]
    fn a_partial_clone_that_stays_in_the_way_is_reported_within_the_budget() {
        let dir = tempfile::tempdir().unwrap();
        let checkout = dir.path().join("checkout");
        #[cfg(unix)]
        fs::write(&checkout, "").unwrap();
        #[cfg(windows)]
        let _held = held_open(&checkout);
        let started = Instant::now();
        assert!(remove_partial_clone(&checkout, Duration::from_millis(200)).is_err());
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    /// The case the retry is for (OWL-80): a stopped git's file stays locked
    /// for a moment, which a single attempt does not get past.
    #[cfg(windows)]
    #[test]
    fn a_partial_clone_locked_for_a_moment_is_removed() {
        let dir = tempfile::tempdir().unwrap();
        let checkout = dir.path().join("checkout");
        let held = held_open(&checkout);
        assert!(fs::remove_dir_all(&checkout).is_err(), "not locked");
        let release = thread::spawn(move || {
            thread::sleep(Duration::from_millis(300));
            drop(held);
        });
        remove_partial_clone(&checkout, Duration::from_secs(10)).unwrap();
        release.join().unwrap();
        assert!(fs::symlink_metadata(&checkout).is_err());
    }

    /// A `checkout` folder with a file in its `.git` held open without
    /// delete sharing, as a git still being stopped holds it.
    #[cfg(windows)]
    fn held_open(checkout: &Path) -> File {
        use std::os::windows::fs::OpenOptionsExt;
        let git_dir = checkout.join(".git");
        fs::create_dir_all(&git_dir).unwrap();
        let file = git_dir.join("index");
        fs::write(&file, "").unwrap();
        OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(file)
            .unwrap()
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
