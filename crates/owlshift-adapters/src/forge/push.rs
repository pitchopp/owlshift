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
    if remote.is_empty() || remote.starts_with('-') {
        return Err(PushError::Failed {
            message: format!("{remote:?} is not a remote"),
        });
    }
    let mut command = Command::new(git);
    command
        .current_dir(worktree)
        .args(["push", "--porcelain", remote])
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
    let target = format!("refs/heads/{branch}");
    let stdout = String::from_utf8_lossy(stdout);
    // `<flag>\t<from>:<to>\t<summary>`; Git for Windows ends lines with CRLF.
    let line = stdout
        .lines()
        .map(|l| l.trim_end_matches('\r'))
        .find_map(|line| {
            let mut fields = line.splitn(3, '\t');
            let flag = fields.next()?;
            let (_, to) = fields.next()?.split_once(':')?;
            (to == target && flag.chars().count() == 1).then(|| (flag, fields.next().unwrap_or("")))
        });
    match line {
        Some(("!", summary)) => Err(PushError::Rejected {
            reason: summary.trim().to_owned(),
        }),
        Some((flag, _)) if success => match flag {
            "*" => Ok(Pushed::Created),
            " " => Ok(Pushed::FastForward),
            "=" => Ok(Pushed::UpToDate),
            other => Err(PushError::Failed {
                message: format!("unexpected update {other:?} of {target}"),
            }),
        },
        _ => Err(PushError::Failed {
            message: format!(
                "git push exited with {ended}: {}",
                tail(&String::from_utf8_lossy(stderr))
            ),
        }),
    }
}

/// The last lines of git's standard error, without credentials.
fn tail(stderr: &str) -> String {
    let lines: Vec<&str> = stderr
        .lines()
        .map(|l| l.trim_end_matches('\r'))
        .filter(|l| !l.trim().is_empty())
        .collect();
    let start = lines.len().saturating_sub(3);
    let text = lines[start..].join(" / ");
    if text.is_empty() {
        "no error output".to_owned()
    } else {
        redact_userinfo(&text)
    }
}

/// Removes the `user:password@` part of every URL in `text`.
fn redact_userinfo(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("://") {
        let (before, after) = rest.split_at(at + 3);
        out.push_str(before);
        let authority_end = after
            .find(|c: char| c == '/' || c.is_whitespace())
            .unwrap_or(after.len());
        let authority = &after[..authority_end];
        match authority.rfind('@') {
            Some(i) => out.push_str(&authority[i + 1..]),
            None => out.push_str(authority),
        }
        rest = &after[authority_end..];
    }
    out.push_str(rest);
    out
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
    fn userinfo_is_removed_from_every_url() {
        assert_eq!(
            redact_userinfo("a https://x@h/p and ssh://git@h:22/p, http://h/q"),
            "a https://h/p and ssh://h:22/p, http://h/q"
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
