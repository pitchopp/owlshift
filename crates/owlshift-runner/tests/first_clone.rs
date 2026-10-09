//! The first clone of a project has its own deadline (OWL-60), and a clone
//! that did not finish never passes for the project's checkout (OWL-80), on
//! every platform. It is its own test process: the project lock is an
//! `flock` that a child forked by another thread can hold for a moment, so
//! tests that start many commands stay away from the lock's unit test.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use owlshift_adapters::forge::Repo;
use owlshift_runner::executor::Git;
use owlshift_runner::project::{ProjectDirs, sync_checkout_within};
use tempfile::TempDir;

/// What the tests clone. Nothing answers at this address: git reaches the
/// remote through the transport command of [`Bench::git`].
const REMOTE_URL: &str = "ssh://example.invalid/project";

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

/// A remote repository to clone and a data directory to clone it into.
struct Bench {
    dir: TempDir,
    /// An empty file, git's global configuration.
    config: PathBuf,
    remote: PathBuf,
    dirs: ProjectDirs,
}

fn bench() -> Bench {
    // A space and an apostrophe, as a data directory's path can hold, so the
    // quoting of the transport command is exercised.
    let dir = tempfile::Builder::new()
        .prefix("owlshift it's ")
        .tempdir()
        .unwrap();
    let config = dir.path().join("gitconfig");
    fs::write(&config, "").unwrap();
    let remote = dir.path().join("remote");
    let dirs = ProjectDirs::new(dir.path(), &Repo::parse("demo/project").unwrap());
    let bench = Bench {
        dir,
        config,
        remote,
        dirs,
    };
    fs::create_dir_all(&bench.remote).unwrap();
    bench.run_git(&bench.remote, &["init", "--quiet", "--initial-branch=main"]);
    bench.run_git(
        &bench.remote,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.test",
            "commit",
            "--quiet",
            "--allow-empty",
            "--message=one",
        ],
    );
    bench
}

/// Makes `command` hermetic: no system configuration, `config` as the global
/// one.
fn hermetic(command: &mut Command, config: &Path) {
    command
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", config);
}

impl Bench {
    /// Runs git in `dir`, which must succeed.
    fn run_git(&self, dir: &Path, args: &[&str]) {
        let mut command = Command::new("git");
        command.args(args).current_dir(dir);
        hermetic(&mut command, &self.config);
        let status = command.status().unwrap();
        assert!(status.success(), "git {args:?}");
    }

    /// Written by the transport once the clone has created the checkout's
    /// `.git`, right before it stalls.
    fn witness(&self) -> PathBuf {
        self.dir.path().join("stalled")
    }

    /// A git whose transport, as a slow network would, waits `pause` s before
    /// serving the remote, and whose default deadline is `default`: a
    /// stand-in for the runner git's 120 s, so a test need not wait that long.
    ///
    /// git runs `GIT_SSH_COMMAND` through `sh`, Git for Windows' own on
    /// Windows, with the host and the remote command appended, which the
    /// final `#` drops; `GIT_SSH_VARIANT=simple` keeps git from probing the
    /// command with `-G` first. A clone creates the checkout's `.git`, with
    /// `origin` configured, before it reaches its transport: stopped there, it
    /// leaves a folder that looks like a checkout (checked with git 2.54 on
    /// macOS on 2026-09-29; the Windows leg of CI runs these tests).
    fn git(&self, pause: u64, default: Duration) -> Git {
        let transport = format!(
            "test -d {git_dir} && : > {witness} && sleep {pause} && exec git upload-pack {remote} #",
            git_dir = quoted(&self.dirs.checkout().join(".git")),
            witness = quoted(&self.witness()),
            remote = quoted(&self.remote),
        );
        let config = self.config.clone();
        Git::with_setup("git", move |command| {
            hermetic(command, &config);
            command
                .env("GIT_SSH_VARIANT", "simple")
                .env("GIT_SSH_COMMAND", &transport);
        })
        .with_timeout(default)
    }

    fn clone_pending(&self) -> bool {
        fs::symlink_metadata(self.dirs.unfinished_clone_file()).is_ok()
    }

    /// The checkout is a whole clone: its `HEAD` is a commit, and no clone
    /// is left pending.
    fn assert_whole_checkout(&self) {
        self.run_git(
            &self.dirs.checkout(),
            &["rev-parse", "--verify", "--quiet", "HEAD^{commit}"],
        );
        assert!(!self.clone_pending(), "the marker outlived the clone");
    }
}

/// `path` as one `sh` word: forward slashes, which Git for Windows' `sh`
/// and git both take, in single quotes, an apostrophe written `'\''`.
fn quoted(path: &Path) -> String {
    let path = path.to_string_lossy().replace('\\', "/");
    format!("'{}'", path.replace('\'', r"'\''"))
}

#[test]
fn a_first_clone_slower_than_the_default_deadline_succeeds() {
    let bench = bench();
    // The clone takes over 5 s: past the 4 s default, under its own 60 s.
    let base =
        sync_checkout_within(&bench.git(5, secs(4)), &bench.dirs, REMOTE_URL, secs(60)).unwrap();
    assert_eq!(base.branch, "main");
    bench.assert_whole_checkout();
}

#[test]
fn a_first_clone_past_its_deadline_leaves_no_checkout_to_build_on() {
    let bench = bench();
    let git = bench.git(120, secs(60));
    let error = sync_checkout_within(&git, &bench.dirs, REMOTE_URL, secs(8)).unwrap_err();
    assert!(error.contains("did not finish within 8 s"), "{error}");
    assert!(
        bench.witness().exists(),
        "the clone was stopped before it created its folder"
    );
    assert!(bench.clone_pending(), "a stopped clone must stay marked");
    if !error.contains("could not be removed") {
        assert!(
            fs::symlink_metadata(bench.dirs.checkout()).is_err(),
            "{error}"
        );
    }

    // The next run clones anew.
    sync_checkout_within(&bench.git(0, secs(60)), &bench.dirs, REMOTE_URL, secs(60)).unwrap();
    bench.assert_whole_checkout();
}

/// What a run killed mid-clone leaves, or one whose removal of the partial
/// clone failed: a `.git` whose `origin` has a URL and nothing fetched, the
/// shape a clone stopped in its transport leaves (git 2.54, 2026-09-29).
/// Taken as is, it would fail every later run on `origin`'s default branch.
#[test]
fn a_clone_an_earlier_run_did_not_finish_is_cloned_anew() {
    let bench = bench();
    let checkout = bench.dirs.checkout();
    fs::create_dir_all(&checkout).unwrap();
    bench.run_git(&checkout, &["init", "--quiet"]);
    bench.run_git(&checkout, &["remote", "add", "origin", REMOTE_URL]);
    fs::write(checkout.join("left-over"), "").unwrap();
    fs::write(bench.dirs.unfinished_clone_file(), "").unwrap();

    let base =
        sync_checkout_within(&bench.git(0, secs(60)), &bench.dirs, REMOTE_URL, secs(60)).unwrap();
    assert_eq!(base.branch, "main");
    assert!(!checkout.join("left-over").exists());
    bench.assert_whole_checkout();
}

/// OWL-205: a git older than the floor is refused before the project's
/// folder is created or the checkout touched, in Owlshift's words with the
/// fix steps `owlshift doctor` gives. The stand-in answers `--version` and
/// leaves a witness for any other command.
#[cfg(unix)]
#[test]
fn a_git_older_than_the_floor_is_refused_before_the_checkout_is_touched() {
    use std::os::unix::fs::PermissionsExt;

    let bench = bench();
    let witness = bench.dir.path().join("other-command");
    let script = bench.dir.path().join("old-git");
    fs::write(
        &script,
        format!(
            "#!/bin/sh\ncase \"$*\" in\n  *--version*) echo 'git version 2.38.5';;\n  *) : > {}; exit 1;;\nesac\n",
            quoted(&witness)
        ),
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();

    let error =
        sync_checkout_within(&Git::new(&script), &bench.dirs, REMOTE_URL, secs(60)).unwrap_err();
    assert!(error.contains("git 2.38.5 is older than 2.39.0"), "{error}");
    assert!(
        error.contains("Install git 2.39.0 or later: https://git-scm.com/downloads"),
        "{error}"
    );
    assert!(!witness.exists(), "a command ran after the refusal");
    assert!(
        fs::symlink_metadata(bench.dirs.root()).is_err(),
        "the project's folder was created"
    );
}
