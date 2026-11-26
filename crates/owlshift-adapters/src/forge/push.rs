//! Pushing the ticket's branch with the `git` CLI, so credential helpers and
//! SSH keys behave exactly as in the user's own terminal (build plan,
//! "Repository & workspace layout").
//!
//! Split like the harness adapter: [`push_command`] builds the invocation,
//! the caller spawns it (the runner in the worktree, a test in a hermetic
//! environment), and [`read_push`] reads its output.
//!
//! The outcome comes from the exit status and the ref's flag in `git push
//! --porcelain`, never from git's text: live check C3 (2026-09-28) saw GitHub
//! and a plain bare repository word the same rejections differently. The
//! flags are those of `git push --porcelain`: `*` new ref, ` ` fast-forward,
//! `=` up to date, `!` rejected, `+` forced, `-` deleted.

use std::path::Path;
use std::process::{Command, Output, Stdio};

use crate::forge::{Branch, CommitId};

/// What a successful push did to the branch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pushed {
    /// The branch did not exist on the remote and now does.
    Created,
    /// The branch moved forward to the commit.
    FastForward,
    /// The branch already pointed to the commit.
    UpToDate,
    /// The branch's history was replaced under a lease: a rebase run's push
    /// (OWL-210), read only by [`read_forced_push_parts`].
    Forced,
}

/// A push that did not land.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PushError {
    /// The remote refused the update, for instance because the branch holds
    /// commits the pushed one does not contain. `reason` is git's own
    /// summary, for a report only.
    Rejected { reason: String },
    /// No verdict for the branch: git could not run, could not reach or
    /// authenticate to the remote, or answered something unexpected. The
    /// message holds the end of git's standard error, with any credential in
    /// a URL removed.
    Failed { message: String },
}

impl std::fmt::Display for PushError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rejected { reason } => write!(f, "the remote rejected the push: {reason}"),
            Self::Failed { message } => write!(f, "the push failed: {message}"),
        }
    }
}

impl std::error::Error for PushError {}

/// `git push --porcelain <remote> <commit>:refs/heads/<branch>`, run in
/// `worktree`: pushes exactly `commit`, the one the runner verified, never
/// whatever the local branch points to by then. A plain push: the remote
/// refuses anything but creating the branch or moving it forward.
///
/// `remote` is a remote name or URL; one starting with `-` is refused, since
/// git would read it as an option.
pub fn push_command(
    git: &Path,
    worktree: &Path,
    remote: &str,
    commit: &CommitId,
    branch: &Branch,
) -> Result<Command, PushError> {
    check_remote(remote)?;
    let mut command = Command::new(git);
    command
        .current_dir(worktree)
        .args(["push", "--porcelain", remote])
        .arg(format!("{commit}:refs/heads/{branch}"))
        .stdin(Stdio::null());
    Ok(command)
}

/// [`push_command`] with a lease (architecture section 5, "Leases"):
/// `--force-with-lease=refs/heads/<branch>:<lease>`, so the remote takes the
/// push only while the branch is still at `lease`, the commit Owlshift last
/// pushed there, and a branch a person pushed to or deleted is never
/// overwritten or recreated.
///
/// The lease alone would also let a commit that does not descend from
/// `lease` replace the branch's history: a fix's push is of a commit that
/// descends from it, which the runner's Writer checks before git runs, and a
/// forced update (`+`) still reads as a failure ([`read_push`]). Only a
/// rebase run's push replaces history (OWL-210): the Writer checks that its
/// commit descends from the base it was rebased onto and keeps the
/// delivery's change, and reads the push with [`read_forced_push_parts`].
pub fn leased_push_command(
    git: &Path,
    worktree: &Path,
    remote: &str,
    commit: &CommitId,
    branch: &Branch,
    lease: &CommitId,
) -> Result<Command, PushError> {
    check_remote(remote)?;
    let mut command = Command::new(git);
    command
        .current_dir(worktree)
        .args(["push", "--porcelain"])
        .arg(format!("--force-with-lease=refs/heads/{branch}:{lease}"))
        .arg(remote)
        .arg(format!("{commit}:refs/heads/{branch}"))
        .stdin(Stdio::null());
    Ok(command)
}

/// Reads the output of a [`push_command`] for `branch`.
pub fn read_push(branch: &Branch, output: &Output) -> Result<Pushed, PushError> {
    read_push_parts(
        branch,
        output.status.success(),
        &output.status.to_string(),
        &output.stdout,
        &output.stderr,
    )
}

/// [`read_push`] from the parts of a push's output, for a caller that ran
/// [`push_command`] its own way: whether git exited with status 0, how it
/// ended (for a message, such as `exit status: 1`), and its standard output
/// and standard error.
pub fn read_push_parts(
    branch: &Branch,
    success: bool,
    ended: &str,
    stdout: &[u8],
    stderr: &[u8],
) -> Result<Pushed, PushError> {
    read_parts(branch, success, ended, stdout, stderr, false)
}

/// [`read_push_parts`] for a rebase run's leased push (OWL-210), the one
/// push that may replace the branch's history: a forced update (`+`) reads
/// as [`Pushed::Forced`] instead of a failure.
pub fn read_forced_push_parts(
    branch: &Branch,
    success: bool,
    ended: &str,
    stdout: &[u8],
    stderr: &[u8],
) -> Result<Pushed, PushError> {
    read_parts(branch, success, ended, stdout, stderr, true)
}

fn read_parts(
    branch: &Branch,
    success: bool,
    ended: &str,
    stdout: &[u8],
    stderr: &[u8],
    forced: bool,
) -> Result<Pushed, PushError> {
    let target = format!("refs/heads/{branch}");
    let stdout = String::from_utf8_lossy(stdout);
    match porcelain_line(&stdout, &target) {
        Some(("!", summary)) => Err(PushError::Rejected {
            reason: summary.trim().to_owned(),
        }),
        Some((flag, _)) if success => match flag {
            "*" => Ok(Pushed::Created),
            " " => Ok(Pushed::FastForward),
            "=" => Ok(Pushed::UpToDate),
            "+" if forced => Ok(Pushed::Forced),
            other => Err(PushError::Failed {
                message: format!("unexpected update {other:?} of {target}"),
            }),
        },
        _ => Err(no_verdict(ended, stderr)),
    }
}

/// The flag and the summary of `target`'s line in a `git push --porcelain`
/// output: `<flag>\t<from>:<to>\t<summary>`, `<from>` being empty or
/// `(delete)` for a deletion. Git for Windows ends lines with CRLF.
fn porcelain_line<'a>(stdout: &'a str, target: &str) -> Option<(&'a str, &'a str)> {
    stdout
        .lines()
        .map(|l| l.trim_end_matches('\r'))
        .find_map(|line| {
            let mut fields = line.splitn(3, '\t');
            let flag = fields.next()?;
            let (_, to) = fields.next()?.split_once(':')?;
            (to == target && flag.chars().count() == 1).then(|| (flag, fields.next().unwrap_or("")))
        })
}

/// A push that gave no verdict for a ref: how git ended, with what its
/// standard error says (the cause, without git's boilerplate or credentials).
fn no_verdict(ended: &str, stderr: &[u8]) -> PushError {
    PushError::Failed {
        message: format!(
            "git push exited with {ended}: {}",
            crate::git_stderr::summary(&String::from_utf8_lossy(stderr))
        ),
    }
}

/// One ref of a push of Owlshift's own refs (OWL-586), each leased on what
/// the remote must hold for it: the claims under `refs/owlshift/claims/`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefUpdate<'a> {
    /// `name` made at `commit`, only while the remote has no such ref:
    /// `--force-with-lease=<name>:`, which the remote applies as a
    /// compare-and-swap (live check C3).
    Create { name: &'a str, commit: &'a CommitId },
    /// `name` moved to `commit`, only while the remote holds `expected`
    /// there (live check T1).
    Update {
        name: &'a str,
        commit: &'a CommitId,
        expected: &'a CommitId,
    },
    /// `name` deleted, only while the remote holds `expected` there.
    Delete {
        name: &'a str,
        expected: &'a CommitId,
    },
}

impl RefUpdate<'_> {
    /// The ref it names.
    pub fn name(&self) -> &str {
        match self {
            Self::Create { name, .. } | Self::Update { name, .. } | Self::Delete { name, .. } => {
                name
            }
        }
    }
}

/// What a landed [`RefUpdate`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefPushed {
    /// The ref was made.
    Created,
    /// The ref moved from the expected commit.
    Updated,
    /// The ref was deleted.
    Deleted,
}

/// `git push --porcelain [--atomic] --force-with-lease=<name>:<expected>…
/// <remote> <commit>:<name>…`, run in `dir`: every update leased, a deletion
/// pushed as `:<name>`. `atomic`: the remote applies all of them or none
/// (live check T1). A ref that is not under `refs/owlshift/` is refused, as
/// is a remote that git would read as an option.
pub fn ref_push_command(
    git: &Path,
    dir: &Path,
    remote: &str,
    updates: &[RefUpdate<'_>],
    atomic: bool,
) -> Result<Command, PushError> {
    check_remote(remote)?;
    if updates.is_empty() {
        return Err(PushError::Failed {
            message: "no ref to push".to_owned(),
        });
    }
    let mut command = Command::new(git);
    command.current_dir(dir).args(["push", "--porcelain"]);
    if atomic {
        command.arg("--atomic");
    }
    for update in updates {
        let name = update.name();
        if !name.starts_with("refs/owlshift/") || name.contains(':') {
            return Err(PushError::Failed {
                message: format!("{name:?} is not one of Owlshift's own refs"),
            });
        }
        command.arg(match update {
            RefUpdate::Create { .. } => format!("--force-with-lease={name}:"),
            RefUpdate::Update { expected, .. } | RefUpdate::Delete { expected, .. } => {
                format!("--force-with-lease={name}:{expected}")
            }
        });
    }
    command.arg(remote);
    for update in updates {
        command.arg(match update {
            RefUpdate::Create { name, commit } | RefUpdate::Update { name, commit, .. } => {
                format!("{commit}:{name}")
            }
            RefUpdate::Delete { name, .. } => format!(":{name}"),
        });
    }
    command.stdin(Stdio::null());
    Ok(command)
}

/// Reads a [`ref_push_command`]'s output, one verdict per ref of `updates`,
/// in their order, from each ref's porcelain flag: `*` made, `+` moved under
/// its lease, `-` deleted, `!` rejected, whether by git, its lease stale
/// against the refs the remote advertised, by the remote's own
/// compare-and-swap, or because an atomic push failed for another ref. A
/// ref with no line, or a success flag while git failed, has no verdict.
pub fn read_ref_push_parts(
    updates: &[RefUpdate<'_>],
    success: bool,
    ended: &str,
    stdout: &[u8],
    stderr: &[u8],
) -> Vec<Result<RefPushed, PushError>> {
    let stdout = String::from_utf8_lossy(stdout);
    updates
        .iter()
        .map(|update| {
            let name = update.name();
            match (porcelain_line(&stdout, name), update) {
                (Some(("!", summary)), _) => Err(PushError::Rejected {
                    reason: summary.trim().to_owned(),
                }),
                (Some(("*", _)), RefUpdate::Create { .. }) if success => Ok(RefPushed::Created),
                (Some(("+", _)), RefUpdate::Update { .. }) if success => Ok(RefPushed::Updated),
                (Some(("-", _)), RefUpdate::Delete { .. }) if success => Ok(RefPushed::Deleted),
                (Some((flag, _)), _) if success => Err(PushError::Failed {
                    message: format!("unexpected update {flag:?} of {name}"),
                }),
                _ => Err(no_verdict(ended, stderr)),
            }
        })
        .collect()
}

/// Refuses a remote git would read as an option, or none.
fn check_remote(remote: &str) -> Result<(), PushError> {
    if remote.is_empty() || remote.starts_with('-') {
        return Err(PushError::Failed {
            message: format!("{remote:?} is not a remote"),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn status(code: i32) -> std::process::ExitStatus {
        use std::os::unix::process::ExitStatusExt;
        std::process::ExitStatus::from_raw(code << 8)
    }

    #[cfg(windows)]
    fn status(code: i32) -> std::process::ExitStatus {
        use std::os::windows::process::ExitStatusExt;
        std::process::ExitStatus::from_raw(code as u32)
    }

    fn output(code: i32, stdout: &str, stderr: &str) -> Output {
        Output {
            status: status(code),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    fn branch() -> Branch {
        Branch::new("owl-17").unwrap()
    }

    const OID: &str = "0123456789012345678901234567890123456789";

    fn porcelain(flag: &str, summary: &str, eol: &str) -> String {
        format!("To origin{eol}{flag}\t{OID}:refs/heads/owl-17\t{summary}{eol}Done{eol}")
    }

    #[test]
    fn the_flag_decides_and_the_text_does_not() {
        let read = |code, flag, summary| {
            read_push(
                &branch(),
                &output(code, &porcelain(flag, summary, "\n"), ""),
            )
        };
        assert_eq!(read(0, "*", "[new branch]"), Ok(Pushed::Created));
        assert_eq!(read(0, " ", "aaa..bbb"), Ok(Pushed::FastForward));
        assert_eq!(read(0, "=", "[up to date]"), Ok(Pushed::UpToDate));
        assert_eq!(
            read(1, "!", "[rejected] (fetch first)"),
            Err(PushError::Rejected {
                reason: "[rejected] (fetch first)".to_owned()
            })
        );
        // A success flag with a failed exit (a hook, a second ref) is no verdict.
        assert!(matches!(
            read(1, "*", "[new branch]"),
            Err(PushError::Failed { .. })
        ));
        assert!(matches!(
            read(0, "+", "aaa...bbb (forced update)"),
            Err(PushError::Failed { .. })
        ));
        // Only a rebase's push reads a forced update as landed.
        let forced = |code, flag, summary| {
            let out = output(code, &porcelain(flag, summary, "\n"), "");
            read_forced_push_parts(
                &branch(),
                out.status.success(),
                "exit",
                &out.stdout,
                &out.stderr,
            )
        };
        assert_eq!(
            forced(0, "+", "aaa...bbb (forced update)"),
            Ok(Pushed::Forced)
        );
        assert_eq!(forced(0, " ", "aaa..bbb"), Ok(Pushed::FastForward));
        assert_eq!(
            forced(1, "!", "[rejected] (stale info)"),
            Err(PushError::Rejected {
                reason: "[rejected] (stale info)".to_owned()
            })
        );
        assert!(matches!(
            forced(1, "+", "aaa...bbb (forced update)"),
            Err(PushError::Failed { .. })
        ));
    }

    const CLAIM: &str = "refs/owlshift/claims/OWL-1";
    const OTHER: &str = "refs/owlshift/claims/OWL-2";

    /// OWL-586: a claim's take, renewal and delete, leased, read ref by ref,
    /// from the porcelain git 2.54 printed against a bare remote.
    #[test]
    fn a_ref_push_is_leased_and_read_ref_by_ref() {
        let commit = CommitId::new(OID).unwrap();
        let expected = CommitId::new("1111111111111111111111111111111111111111").unwrap();
        let updates = [
            RefUpdate::Create {
                name: CLAIM,
                commit: &commit,
            },
            RefUpdate::Update {
                name: OTHER,
                commit: &commit,
                expected: &expected,
            },
        ];
        let command =
            ref_push_command(Path::new("git"), Path::new("."), "origin", &updates, true).unwrap();
        let args: Vec<_> = command.get_args().map(|a| a.to_string_lossy()).collect();
        assert_eq!(
            args,
            [
                "push",
                "--porcelain",
                "--atomic",
                &format!("--force-with-lease={CLAIM}:"),
                &format!("--force-with-lease={OTHER}:{expected}"),
                "origin",
                &format!("{OID}:{CLAIM}"),
                &format!("{OID}:{OTHER}"),
            ]
        );
        let delete = [RefUpdate::Delete {
            name: CLAIM,
            expected: &expected,
        }];
        let command =
            ref_push_command(Path::new("git"), Path::new("."), "origin", &delete, false).unwrap();
        let args: Vec<_> = command.get_args().map(|a| a.to_string_lossy()).collect();
        assert_eq!(args.last().unwrap(), &format!(":{CLAIM}"));
        // Only Owlshift's own refs, and never an option as remote.
        let branch = [RefUpdate::Create {
            name: "refs/heads/main",
            commit: &commit,
        }];
        assert!(
            ref_push_command(Path::new("git"), Path::new("."), "origin", &branch, false).is_err()
        );
        assert!(
            ref_push_command(
                Path::new("git"),
                Path::new("."),
                "--mirror",
                &updates,
                false
            )
            .is_err()
        );
        assert!(ref_push_command(Path::new("git"), Path::new("."), "origin", &[], false).is_err());

        let read = |code: i32, stdout: &str, updates: &[RefUpdate<'_>]| {
            let out = output(code, stdout, "error: failed to push some refs");
            read_ref_push_parts(
                updates,
                out.status.success(),
                "exit",
                &out.stdout,
                &out.stderr,
            )
        };
        let taken = format!("To ../r\n*\t{OID}:{CLAIM}\t[new reference]\nDone\n");
        assert_eq!(read(0, &taken, &updates[..1]), [Ok(RefPushed::Created)]);
        let renewed =
            format!("To ../r\n+\t{OID}:{OTHER}\t1111111...0123456 (forced update)\nDone\n");
        assert_eq!(read(0, &renewed, &updates[1..]), [Ok(RefPushed::Updated)]);
        let deleted = format!("To ../r\n-\t:{CLAIM}\t[deleted]\nDone\n");
        assert_eq!(read(0, &deleted, &delete), [Ok(RefPushed::Deleted)]);
        // A take of a claim that exists, and a delete of one that does not.
        let stale = format!("To ../r\n!\t{OID}:{CLAIM}\t[rejected] (stale info)\nDone\n");
        assert!(matches!(
            read(1, &stale, &updates[..1])[0],
            Err(PushError::Rejected { .. })
        ));
        let gone = format!("To ../r\n!\t(delete):{CLAIM}\t[rejected] (stale info)\nDone\n");
        assert!(matches!(
            read(1, &gone, &delete)[0],
            Err(PushError::Rejected { .. })
        ));
        // An atomic push refused for one ref refuses the other.
        let atomic = format!(
            "To ../r\n!\t{OID}:{CLAIM}\t[rejected] (stale info)\n\
             !\t{OID}:{OTHER}\t[rejected] (atomic push failed)\nDone\n"
        );
        let both = read(1, &atomic, &updates);
        assert!(
            both.iter()
                .all(|r| matches!(r, Err(PushError::Rejected { .. }))),
            "{both:?}"
        );
        // A ref with no line, or a flag that is not its update's, has no verdict.
        assert!(matches!(
            read(128, "", &updates[..1])[0],
            Err(PushError::Failed { .. })
        ));
        assert!(matches!(
            read(0, &deleted, &updates[..1])[0],
            Err(PushError::Failed { .. })
        ));
        assert!(matches!(
            read(1, &taken, &updates[..1])[0],
            Err(PushError::Failed { .. })
        ));
    }

    #[test]
    fn crlf_output_reads_the_same() {
        let out = output(1, &porcelain("!", "[remote rejected] (x)", "\r\n"), "");
        assert_eq!(
            read_push(&branch(), &out),
            Err(PushError::Rejected {
                reason: "[remote rejected] (x)".to_owned()
            })
        );
    }

    #[test]
    fn another_refs_line_is_not_the_branchs() {
        let other = format!("To origin\n*\t{OID}:refs/heads/owl-170\t[new branch]\nDone\n");
        assert!(matches!(
            read_push(&branch(), &output(0, &other, "")),
            Err(PushError::Failed { .. })
        ));
    }

    #[test]
    fn a_failure_without_a_verdict_keeps_stderr_without_credentials() {
        let stderr = "remote: Invalid username or token.\n\
                      fatal: Authentication failed for 'https://user:ghp_secret@github.com/o/r.git/'\n";
        let error = read_push(&branch(), &output(128, "", stderr)).unwrap_err();
        let text = error.to_string();
        assert!(text.contains("Authentication failed"), "{text}");
        assert!(text.contains("https://github.com/o/r.git/"), "{text}");
        assert!(
            !text.contains("ghp_secret") && !text.contains("user:"),
            "{text}"
        );
    }

    #[test]
    fn the_command_pushes_the_exact_commit_and_refuses_an_option_as_remote() {
        let commit = CommitId::new(OID).unwrap();
        let command = push_command(
            Path::new("git"),
            Path::new("."),
            "origin",
            &commit,
            &branch(),
        )
        .unwrap();
        let args: Vec<_> = command.get_args().map(|a| a.to_string_lossy()).collect();
        assert_eq!(
            args,
            [
                "push",
                "--porcelain",
                "origin",
                &format!("{OID}:refs/heads/owl-17")
            ]
        );
        for bad in ["", "--mirror"] {
            assert!(
                push_command(Path::new("git"), Path::new("."), bad, &commit, &branch()).is_err()
            );
        }
    }
}
