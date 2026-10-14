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
    check_spec(git, spec.main, spec.worktree, spec.branch)?;
    if fs::symlink_metadata(spec.worktree).is_ok() {
        return check_kept(git, spec.main, spec.worktree, spec.branch);
    }

    let head_ref = format!("refs/heads/{}", spec.branch);
    let exists = branch_exists(git, spec.main, &head_ref)?;
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

fn worktree_error(error: GitError) -> ExecutorError {
    ExecutorError::Worktree(error.to_string())
}

/// Whether `head_ref`, a full branch ref, exists in `main`.
fn branch_exists(git: &Git, main: &Path, head_ref: &str) -> Result<bool, ExecutorError> {
    Ok(git
        .output(main, &["show-ref", "--verify", "--quiet", head_ref], None)
        .map_err(worktree_error)?
        .success())
}

/// Checks a kept worktree before it is used: its `.git` first, before any
/// git runs in it, since an earlier run could have pointed it elsewhere;
/// then that it is the root of a linked worktree of `main`'s repository,
/// on `branch`, at the commit the branch points to in `main`.
fn check_kept(git: &Git, main: &Path, worktree: &Path, branch: &str) -> Result<(), ExecutorError> {
    let head_ref = format!("refs/heads/{branch}");
    let common = common_dir(git, main)?;
    super::isolation::check_worktree_link(&common, worktree).map_err(ExecutorError::Worktree)?;
    check_linked(git, main, worktree)?;
    let head = git
        .output(worktree, &["symbolic-ref", "-q", "HEAD"], None)
        .map_err(worktree_error)?;
    let head = String::from_utf8_lossy(&head.stdout).trim().to_owned();
    if head != head_ref {
        return Err(ExecutorError::Worktree(format!(
            "{} is on {}, not {head_ref}",
            worktree.display(),
            if head.is_empty() { "no branch" } else { &head }
        )));
    }
    let here = git
        .run(worktree, &["rev-parse", "HEAD"])
        .map_err(worktree_error)?;
    let there = git
        .run(
            main,
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
            worktree.display(),
            main.display()
        )));
    }
    Ok(())
}

/// Brings a kept `branch` with no commits of its own to `base`, at a
/// dispatch to Build from Ready (OWL-274): the run's own branch may have
/// been made by an earlier run, such as the ticket's intake, at the base of
/// its time. A worktree found is checked first, as [`prepare`] checks it.
///
/// - No branch: nothing to do, the run creates it from `base`.
/// - A head that is an ancestor of `base` is fast-forwarded to it: in the
///   worktree by `git merge --ff-only`, which carries uncommitted work
///   along; without a worktree by `git branch -f`, which refuses a branch
///   another worktree has checked out. Moving from an ancestor to a
///   descendant loses no commit: every commit of the old head is
///   reachable from the new one.
/// - Any other head, with commits of its own or ahead of `base`, is kept.
///
/// The merge refuses to overwrite a local change, tracked, untracked or
/// ignored (`--no-overwrite-ignore`), and moves nothing then: the error
/// carries git's message. `--no-squash` and `--no-autostash` keep a
/// branch's `mergeOptions` and `merge.autoStash` from changing what it
/// does.
pub(super) fn start_from_base(
    git: &Git,
    main: &Path,
    worktree: &Path,
    branch: &str,
    base: &str,
) -> Result<(), ExecutorError> {
    check_spec(git, main, worktree, branch)?;
    let kept = fs::symlink_metadata(worktree).is_ok();
    if kept {
        check_kept(git, main, worktree, branch)?;
    }
    let head_ref = format!("refs/heads/{branch}");
    if !branch_exists(git, main, &head_ref)? {
        return Ok(());
    }
    let base = resolve_base(git, main, base)
        .map_err(|e| ExecutorError::Spec(format!("base {base:?}: {e}")))?;
    let head = git
        .run(
            main,
            &[
                "rev-parse",
                "--verify",
                "--end-of-options",
                head_ref.as_str(),
            ],
        )
        .map_err(worktree_error)?;
    let head = String::from_utf8_lossy(&head).trim().to_owned();
    if head == base
        || !git
            .is_ancestor(main, &head, &base)
            .map_err(worktree_error)?
    {
        return Ok(());
    }
    let moved = if kept {
        git.run(
            worktree,
            &[
                "merge",
                "--ff-only",
                "--no-squash",
                "--no-autostash",
                "--no-overwrite-ignore",
                "--quiet",
                "--end-of-options",
                base.as_str(),
            ],
        )
    } else {
        git.run(
            main,
            &[
                "branch",
                "--force",
                "--end-of-options",
                branch,
                base.as_str(),
            ],
        )
    };
    moved.map(drop).map_err(|e| {
        ExecutorError::Worktree(format!(
            "fast-forwarding {branch} from {head} to {base}: {}",
            e.detail
        ))
    })
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
fn check_linked(git: &Git, main: &Path, worktree: &Path) -> Result<(), ExecutorError> {
    let top = ["rev-parse", "--path-format=absolute", "--show-toplevel"];
    let worktree_top = resolved(git, worktree, &top)?;
    let linked = common_dir(git, worktree)? == common_dir(git, main)?
        && worktree_top != resolved(git, main, &top)?
        && worktree_top == canonical(worktree)?;
    if linked {
        Ok(())
    } else {
        Err(ExecutorError::Worktree(format!(
            "{} is not a linked worktree of {}",
            worktree.display(),
            main.display()
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
fn check_spec(git: &Git, main: &Path, worktree: &Path, branch: &str) -> Result<(), ExecutorError> {
    if !worktree.is_absolute() {
        return Err(ExecutorError::Spec(format!(
            "the worktree path {} is not absolute",
            worktree.display()
        )));
    }
    let valid = !branch.starts_with('-') && {
        let checked = git
            .output(main, &["check-ref-format", "--branch", branch], None)
            .map_err(|e| ExecutorError::Spec(e.to_string()))?;
        checked.success() && String::from_utf8_lossy(&checked.stdout).trim() == branch
    };
    if valid {
        Ok(())
    } else {
        Err(ExecutorError::Spec(format!(
            "{branch:?} is not a branch name"
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
