//! Whether a test can run where it is (OWL-555).
//!
//! A few tests rely on what a cloud container does not give: a folder mode
//! that refuses a write (root bypasses it) and a git that takes
//! `--end-of-options` after `reset`. Each probes the capability and, where it
//! is missing, skips with a stated reason, unless the environment requires
//! the fixtures (`OWLSHIFT_REQUIRE_FIXTURE_TOOLS`, set on CI's Linux and macOS
//! legs): there a failed probe panics, so CI runs the test or fails.
//!
//! Live check, 2026-11-14, `ubuntu:24.04` as root with git 2.43.0: `git reset
//! --hard --quiet --end-of-options <commit>` exits 128 with `fatal: option
//! '--end-of-options' must come before non-option arguments`, in every
//! position; `git reset --hard <commit> --` exits 0.

use crate::fixture_gate::REQUIRE_FIXTURE_TOOLS;
use crate::git::GitEnv;

/// What a test does after probing a capability.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Gate {
    /// The capability is here.
    Run,
    /// It is missing and nothing requires it: the test returns, saying why.
    Skip(String),
    /// It is missing and the environment requires it: the test fails.
    Fail(String),
}

/// What a test does, from its probe and whether the environment requires it.
pub fn gate(probe: Result<(), String>, require: bool) -> Gate {
    match (probe, require) {
        (Ok(()), _) => Gate::Run,
        (Err(why), false) => Gate::Skip(why),
        (Err(why), true) => Gate::Fail(why),
    }
}

/// Whether the test goes on: false, after printing `skipped: <reason>`, when
/// the probe failed; a panic where the fixtures are required.
pub fn skip_unless(probe: Result<(), String>) -> bool {
    let require = std::env::var_os(REQUIRE_FIXTURE_TOOLS).is_some();
    match gate(probe, require) {
        Gate::Run => true,
        Gate::Skip(why) => {
            eprintln!("skipped: {why}");
            false
        }
        Gate::Fail(why) => panic!("{REQUIRE_FIXTURE_TOOLS} is set, but: {why}"),
    }
}

/// Whether a folder with mode 0o500 refuses a write: it does not for root.
/// Panics when the probe's own setup fails.
#[cfg(unix)]
pub fn refuses_writes_to_read_only_folder() -> Result<(), String> {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().expect("a scratch folder");
    let folder = dir.path().join("shut");
    fs::create_dir(&folder).expect("a scratch subfolder");
    fs::set_permissions(&folder, fs::Permissions::from_mode(0o500)).expect("a read-only mode");
    let wrote = fs::write(folder.join("probe"), "").is_ok();
    fs::set_permissions(&folder, fs::Permissions::from_mode(0o700)).expect("restoring the mode");
    if wrote {
        Err("running as root: a read-only folder does not refuse writes".to_owned())
    } else {
        Ok(())
    }
}

/// Whether the machine's git takes `--end-of-options` after `reset`, as the
/// runner's `restore` issues it. Err only for git's own rejection of the
/// option; any other failure, the probe's setup included, panics.
pub fn git_takes_reset_end_of_options() -> Result<(), String> {
    git_takes_reset_end_of_options_with("git")
}

/// The probe with the git program as a parameter, so a stub can stand in. The
/// scratch repository is made with the real `git`; only the probed command
/// and the version report go through `git`.
fn git_takes_reset_end_of_options_with(git: impl AsRef<std::ffi::OsStr>) -> Result<(), String> {
    use std::process::{Command, Stdio};

    let git = git.as_ref();
    let dir = tempfile::tempdir().expect("a scratch folder");
    let env = GitEnv::create(dir.path().join("home")).expect("the bench's git environment");
    let repo = dir.path().join("repo");
    std::fs::create_dir(&repo).expect("a scratch repository folder");
    for args in [
        &["init", "--quiet"][..],
        &["commit", "--quiet", "--allow-empty", "-m", "one"],
    ] {
        env.run(&repo, args).expect("the scratch repository");
    }
    let go = |args: &[&str]| {
        let mut command = Command::new(git);
        command.args(args).current_dir(&repo).stdin(Stdio::null());
        env.apply(&mut command);
        command.output().expect("the git program runs")
    };
    let args = ["reset", "--hard", "--quiet", "--end-of-options", "HEAD"];
    let output = go(&args);
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    if output.status.code() == Some(128)
        && stderr.contains("option '--end-of-options' must come before")
    {
        let version = String::from_utf8_lossy(&go(&["--version"]).stdout)
            .trim()
            .to_owned();
        Err(format!("{version} rejects `git {}`", args.join(" ")))
    } else {
        panic!("the git probe failed otherwise: {} {stderr}", output.status)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every combination of probe and requirement.
    #[test]
    fn a_probe_runs_skips_or_fails() {
        assert_eq!(gate(Ok(()), false), Gate::Run);
        assert_eq!(gate(Ok(()), true), Gate::Run);
        assert_eq!(gate(Err("why".into()), false), Gate::Skip("why".into()));
        assert_eq!(gate(Err("why".into()), true), Gate::Fail("why".into()));
    }

    /// A stub `git` script running `body` in place of the program.
    #[cfg(unix)]
    fn stub(dir: &std::path::Path, body: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("git-stub");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[cfg(unix)]
    #[test]
    fn a_git_that_takes_the_option_passes_the_probe() {
        let dir = tempfile::tempdir().unwrap();
        let git = stub(dir.path(), "exit 0");
        assert_eq!(git_takes_reset_end_of_options_with(&git), Ok(()));
    }

    #[cfg(unix)]
    #[test]
    fn a_git_that_rejects_the_option_fails_the_probe_naming_version_and_command() {
        let dir = tempfile::tempdir().unwrap();
        let git = stub(
            dir.path(),
            "if [ \"$1\" = --version ]; then echo 'git version 2.43.0'; exit 0; fi\n\
echo \"fatal: option '--end-of-options' must come before non-option arguments\" >&2\nexit 128",
        );
        let err = git_takes_reset_end_of_options_with(&git).unwrap_err();
        assert!(err.contains("git version 2.43.0"), "{err}");
        assert!(
            err.contains("git reset --hard --quiet --end-of-options HEAD"),
            "{err}"
        );
    }

    #[cfg(unix)]
    #[test]
    #[should_panic(expected = "the git probe failed otherwise")]
    fn a_git_that_fails_otherwise_panics_the_probe() {
        let dir = tempfile::tempdir().unwrap();
        let git = stub(dir.path(), "echo 'fatal: something else' >&2\nexit 1");
        let _ = git_takes_reset_end_of_options_with(&git);
    }

    /// The write probe agrees with a direct write into a read-only folder.
    #[cfg(unix)]
    #[test]
    fn the_write_probe_matches_a_direct_write() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("shut");
        std::fs::create_dir(&folder).unwrap();
        std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o500)).unwrap();
        let refused = std::fs::write(folder.join("x"), "").is_err();
        std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o700)).unwrap();
        let probe = refuses_writes_to_read_only_folder();
        assert_eq!(probe.is_ok(), refused);
        if let Err(why) = probe {
            assert!(why.contains("root"), "{why}");
        }
    }

    /// The real probes run on the machine: they pass, or skip with a reason
    /// where the machine cannot, and fail where the fixtures are required.
    #[test]
    fn the_probes_run_on_this_machine() {
        let _ = skip_unless(git_takes_reset_end_of_options());
        #[cfg(unix)]
        let _ = skip_unless(refuses_writes_to_read_only_folder());
    }
}
