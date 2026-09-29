//! The ticket's worktree and the run's files in it.

use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use owlshift_contracts::brief::Brief;

use super::git::{Git, GitError};
use super::{BRIEF_PATH, ExecutorError, RUN_DIR, RunSpec};

/// Creates the ticket's worktree on its branch, or checks that the one
/// found is: the root of a linked worktree of the main checkout's
/// repository, on the branch, at the commit the branch points to there.
pub(super) fn prepare(git: &Git, spec: &RunSpec<'_>) -> Result<(), ExecutorError> {
    check_spec(git, spec)?;
    let head_ref = format!("refs/heads/{}", spec.branch);
    let worktree_error = |e: super::GitError| ExecutorError::Worktree(e.to_string());

    if fs::symlink_metadata(spec.worktree).is_ok() {
        // The kept worktree's `.git` is checked before any git runs in it:
        // an earlier run could have pointed it elsewhere.
        let common = common_dir(git, spec.main)?;
        super::isolation::check_worktree_link(&common, spec.worktree)
            .map_err(ExecutorError::Worktree)?;
        check_linked(git, spec)?;
        let head = git
            .output(spec.worktree, &["symbolic-ref", "-q", "HEAD"], None)
            .map_err(worktree_error)?;
        let head = String::from_utf8_lossy(&head.stdout).trim().to_owned();
        if head != head_ref {
            return Err(ExecutorError::Worktree(format!(
                "{} is on {}, not {head_ref}",
                spec.worktree.display(),
                if head.is_empty() { "no branch" } else { &head }
            )));
        }
        let here = git
            .run(spec.worktree, &["rev-parse", "HEAD"])
            .map_err(worktree_error)?;
        let there = git
            .run(
                spec.main,
                &[
                    "rev-parse",
                    "--verify",
                    "--end-of-options",
                    head_ref.as_str(),
                ],
            )
            .map_err(worktree_error)?;
        if here != there {
            return Err(ExecutorError::Worktree(format!(
                "{} is not at the commit {head_ref} has in {}",
                spec.worktree.display(),
                spec.main.display()
            )));
        }
        return Ok(());
    }

    let exists = git
        .output(
            spec.main,
            &["show-ref", "--verify", "--quiet", head_ref.as_str()],
            None,
        )
        .map_err(worktree_error)?
        .success();
    let mut args: Vec<&OsStr> = vec!["worktree".as_ref(), "add".as_ref(), "--quiet".as_ref()];
    let base;
    if exists {
        args.extend([spec.worktree.as_os_str(), spec.branch.as_ref()]);
    } else {
        base = resolve_base(git, spec.main, spec.base)
            .map_err(|e| ExecutorError::Spec(format!("base {:?}: {e}", spec.base)))?;
        args.extend([
            "-b".as_ref(),
            spec.branch.as_ref(),
            spec.worktree.as_os_str(),
            base.as_ref(),
        ]);
    }
    git.run(spec.main, &args).map_err(worktree_error)?;
    Ok(())
}

/// The commit a new branch starts from. A full object id is taken as the
/// commit it names: git never lets a ref shadow 40 or 64 hex digits. Any
/// other base is a remote-tracking name, looked up as exactly
/// `refs/remotes/<base>` (OWL-66): a local branch named like it
/// (`refs/heads/origin/main`, or `refs/heads/refs/remotes/origin/main` when
/// the remote-tracking ref is missing) cannot stand in for it, and a
/// revision expression such as `origin/main~1` is refused. Checked on
/// 2026-09-29 with git 2.54: `rev-parse` gave a planted
/// `refs/heads/origin/main` for `origin/main`, and a planted
/// `refs/heads/refs/remotes/origin/main` for an absent
/// `refs/remotes/origin/main`; `show-ref --verify` refused both the absent
/// ref and the expressions.
fn resolve_base(git: &Git, main: &Path, base: &str) -> Result<String, GitError> {
    let full_id = matches!(base.len(), 40 | 64) && base.bytes().all(|b| b.is_ascii_hexdigit());
    let object = if full_id {
        base.to_owned()
    } else {
        let full_name = format!("refs/remotes/{base}");
        let hash = git.run(
            main,
            &["show-ref", "--verify", "--hash", full_name.as_str()],
        )?;
        String::from_utf8_lossy(&hash).trim().to_owned()
    };
    let commit = git.run(
        main,
        &[
            "rev-parse",
            "--verify",
            "--end-of-options",
            format!("{object}^{{commit}}").as_str(),
        ],
    )?;
    Ok(String::from_utf8_lossy(&commit).trim().to_owned())
}

/// Requires an existing worktree to be one the isolation check protects
/// against: the root of a linked worktree of the main checkout's
/// repository. Its common git directory must be the main checkout's, so a
/// separate clone is refused; its top level must be its own path and not
/// the main checkout's, so the main checkout itself, or a folder inside
/// either, is refused too. Paths are compared once resolved, links
/// included.
fn check_linked(git: &Git, spec: &RunSpec<'_>) -> Result<(), ExecutorError> {
    let top = ["rev-parse", "--path-format=absolute", "--show-toplevel"];
    let worktree_top = resolved(git, spec.worktree, &top)?;
    let linked = common_dir(git, spec.worktree)? == common_dir(git, spec.main)?
        && worktree_top != resolved(git, spec.main, &top)?
        && worktree_top == canonical(spec.worktree)?;
    if linked {
        Ok(())
    } else {
        Err(ExecutorError::Worktree(format!(
            "{} is not a linked worktree of {}",
            spec.worktree.display(),
            spec.main.display()
        )))
    }
}

/// The common git directory of the repository at `dir`, resolved.
fn common_dir(git: &Git, dir: &Path) -> Result<PathBuf, ExecutorError> {
    resolved(
        git,
        dir,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
}

/// The path git prints in `dir`, resolved.
fn resolved(git: &Git, dir: &Path, args: &[&str]) -> Result<PathBuf, ExecutorError> {
    let printed = git
        .run(dir, args)
        .map_err(|e| ExecutorError::Worktree(e.to_string()))?;
    canonical(&dir.join(String::from_utf8_lossy(&printed).trim()))
}

fn canonical(path: &Path) -> Result<PathBuf, ExecutorError> {
    fs::canonicalize(path).map_err(|e| ExecutorError::Worktree(format!("{}: {e}", path.display())))
}

/// Refuses a branch name git would not take as is (an option, a name it
/// rejects or expands, such as `@{-1}`), and a worktree path that is not
/// absolute.
fn check_spec(git: &Git, spec: &RunSpec<'_>) -> Result<(), ExecutorError> {
    if !spec.worktree.is_absolute() {
        return Err(ExecutorError::Spec(format!(
            "the worktree path {} is not absolute",
            spec.worktree.display()
        )));
    }
    let valid = !spec.branch.starts_with('-') && {
        let checked = git
            .output(
                spec.main,
                &["check-ref-format", "--branch", spec.branch],
                None,
            )
            .map_err(|e| ExecutorError::Spec(e.to_string()))?;
        checked.success() && String::from_utf8_lossy(&checked.stdout).trim() == spec.branch
    };
    if valid {
        Ok(())
    } else {
        Err(ExecutorError::Spec(format!(
            "{:?} is not a branch name",
            spec.branch
        )))
    }
}

/// Writes the run's files under [`RUN_DIR`]: its `.gitignore`, the brief,
/// and no stale result. Returns the brief's path.
///
/// A previous run's agent could write here, so nothing is written through a
/// link: the two directories must be real ones, and each file is removed,
/// never opened, then created anew.
pub(super) fn write_run_files(worktree: &Path, brief: &Brief) -> Result<PathBuf, ExecutorError> {
    let mut dir = worktree.to_owned();
    for segment in RUN_DIR.split('/') {
        dir.push(segment);
        real_dir(&dir)?;
    }
    replace(&dir.join(".gitignore"), b"*\n")?;
    remove(&worktree.join(super::RESULT_PATH))?;
    let brief_file = worktree.join(BRIEF_PATH);
    replace(&brief_file, brief.render().as_bytes())?;
    Ok(brief_file)
}

fn layout(path: &Path, error: impl std::fmt::Display) -> ExecutorError {
    ExecutorError::Layout(format!("{}: {error}", path.display()))
}

/// Makes sure `path` is a directory and not a link, creating it if missing.
fn real_dir(path: &Path) -> Result<(), ExecutorError> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => Err(layout(path, "is a symbolic link")),
        Ok(meta) if meta.is_dir() => Ok(()),
        Ok(_) => Err(layout(path, "is not a directory")),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            fs::create_dir(path).map_err(|e| layout(path, e))
        }
        Err(error) => Err(layout(path, error)),
    }
}

/// Removes the file or link at `path`, if any; a directory is refused.
fn remove(path: &Path) -> Result<(), ExecutorError> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => Err(layout(path, "is a directory")),
        Ok(_) => fs::remove_file(path).map_err(|e| layout(path, e)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(layout(path, error)),
    }
}

/// Writes a new file at `path`, whatever was there: a link is removed, not
/// followed.
fn replace(path: &Path, content: &[u8]) -> Result<(), ExecutorError> {
    remove(path)?;
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .and_then(|mut file| file.write_all(content))
        .map_err(|e| layout(path, e))
}
