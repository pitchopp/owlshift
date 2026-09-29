//! The first clone of a project has its own deadline (OWL-60). It is its own
//! test process: the project lock is an `flock` that a child forked by
//! another thread can hold for a moment, so tests that start many commands
//! stay away from the lock's unit test.
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use owlshift_adapters::forge::Repo;
use owlshift_runner::executor::Git;
use owlshift_runner::project::{ProjectDirs, sync_checkout_within};

/// A git that takes `pause` s before every clone, as a slow remote would,
/// and whose default deadline is `default`: a stand-in for the runner git's
/// 120 s, so a test need not wait that long.
fn slow_clone_git(dir: &Path, pause: u32, default: Duration) -> Git {
    let script = dir.join("slow-git");
    fs::write(
        &script,
        format!(
            "#!/bin/sh\nfor a in \"$@\"; do [ \"$a\" = clone ] && sleep {pause}; done\nexec git \"$@\"\n"
        ),
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    Git::with_setup(&script, |command| {
        command
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null");
    })
    .with_timeout(default)
}

/// A repository with one commit on `main`, to clone.
fn remote_repository(dir: &Path) -> PathBuf {
    let remote = dir.join("remote");
    let git = |args: &[&str]| {
        let status = Command::new("git")
            .args(args)
            .current_dir(&remote)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    };
    fs::create_dir_all(&remote).unwrap();
    git(&["init", "--quiet", "--initial-branch=main"]);
    git(&[
        "-c",
        "user.name=t",
        "-c",
        "user.email=t@example.test",
        "commit",
        "--quiet",
        "--allow-empty",
        "--message=one",
    ]);
    remote
}

#[test]
fn a_first_clone_slower_than_the_default_deadline_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    let remote = remote_repository(dir.path());
    let dirs = ProjectDirs::new(dir.path(), &Repo::parse("demo/project").unwrap());
    // The clone takes 2 s: over the 1 s default, under its own 20 s.
    let git = slow_clone_git(dir.path(), 2, Duration::from_secs(1));
    let base = sync_checkout_within(
        &git,
        &dirs,
        remote.to_str().unwrap(),
        Duration::from_secs(20),
    )
    .unwrap();
    assert_eq!(base.branch, "main");
}

#[test]
fn a_first_clone_past_its_own_deadline_fails_and_leaves_no_checkout() {
    let dir = tempfile::tempdir().unwrap();
    let remote = remote_repository(dir.path());
    let dirs = ProjectDirs::new(dir.path(), &Repo::parse("demo/project").unwrap());
    let git = slow_clone_git(dir.path(), 30, Duration::from_secs(60));
    let error = sync_checkout_within(
        &git,
        &dirs,
        remote.to_str().unwrap(),
        Duration::from_secs(1),
    )
    .unwrap_err();
    assert!(error.contains("did not finish within 1 s"), "{error}");
    assert!(fs::symlink_metadata(dirs.checkout()).is_err());
}
