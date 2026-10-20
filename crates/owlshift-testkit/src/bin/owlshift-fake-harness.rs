//! The fake harness: a stand-in for a harness CLI in tests. Launched in a
//! worktree with a brief and a reply file, it does what the reply says (see
//! `owlshift_testkit::reply`), so a scenario can script a run's outcome
//! without a model.
//!
//! It exits with status 2 when the brief or the reply is unreadable, or a
//! step it was told to do fails.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use clap::Parser;
use owlshift_contracts::brief::Brief;
use owlshift_contracts::ids::RelativePath;
use owlshift_testkit::reply::{
    FAKE_SESSION_ENV, OWN_FAILURE, Reply, TMPDIR_PREFIX, USAGE_LIMIT_UNKNOWN, cost_line,
    observed_line, transcript_place, usage_limit_line,
};

/// Plays one scripted run in the current directory, the worktree.
#[derive(Parser)]
#[command(name = "owlshift-fake-harness")]
struct Args {
    /// The brief the runner wrote for this run.
    #[arg(long)]
    brief: PathBuf,
    /// What to do: a reply file.
    #[arg(long)]
    reply: PathBuf,
    /// Starts a session under this id, kept in the folder
    /// `FAKE_SESSION_ENV` names (OWL-394).
    #[arg(long, conflicts_with = "resume")]
    session_id: Option<String>,
    /// Resumes the session of this id from that folder, as Claude Code
    /// does: refused, exit status 1, when the folder does not hold it.
    #[arg(long)]
    resume: Option<String>,
}

/// A session of the fake harness (OWL-394): its transcript, one line per
/// run, each the session's whole cost so far, and that cost before this
/// run.
struct Session {
    transcript: PathBuf,
    prior_micros: u64,
}

/// The session `args` name, or the exit status of a resume the folder does
/// not hold, as Claude Code's (C18).
fn session(args: &Args) -> Result<Result<Option<Session>, i32>, String> {
    let (id, resuming) = match (&args.session_id, &args.resume) {
        (Some(id), None) => (id, false),
        (None, Some(id)) => (id, true),
        _ => return Ok(Ok(None)),
    };
    let folder = std::env::var_os(FAKE_SESSION_ENV)
        .map(PathBuf::from)
        .ok_or_else(|| format!("a session without {FAKE_SESSION_ENV}"))?;
    let transcript = folder.join(transcript_place(id));
    if !resuming {
        return Ok(Ok(Some(Session {
            transcript,
            prior_micros: 0,
        })));
    }
    let Ok(text) = fs::read_to_string(&transcript) else {
        eprintln!("No conversation found with session ID: {id}");
        return Ok(Err(1));
    };
    let prior_micros = text
        .lines()
        .last()
        .and_then(|line| line.parse().ok())
        .ok_or_else(|| format!("{}: no cost on its last line", transcript.display()))?;
    Ok(Ok(Some(Session {
        transcript,
        prior_micros,
    })))
}

fn main() {
    let args = Args::parse();
    let code = match play(&args) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("owlshift-fake-harness: {error}");
            OWN_FAILURE
        }
    };
    // Best effort: nothing is left to report a failed flush to.
    let _ = io::stdout().flush();
    std::process::exit(code);
}

fn play(args: &Args) -> Result<i32, String> {
    let brief = Brief::parse(&read(&args.brief)?).map_err(|e| e.to_string())?;
    let reply = Reply::parse(&read(&args.reply)?)?;
    let session = match session(args)? {
        Ok(session) => session,
        Err(code) => return Ok(code),
    };
    let worktree = std::env::current_dir().map_err(|e| format!("current directory: {e}"))?;

    thread::sleep(Duration::from_millis(reply.delay_ms));

    if let Some(into) = &reply.observe_into {
        let mut lines = String::new();
        for entry in &reply.observe {
            let path = match entry.strip_prefix(TMPDIR_PREFIX) {
                Some(rest) => PathBuf::from(
                    std::env::var_os("TMPDIR")
                        .ok_or_else(|| format!("{entry}: TMPDIR is not set"))?,
                )
                .join(rest),
                None => worktree.join(entry),
            };
            let content = match fs::read(&path) {
                Ok(bytes) => Some(bytes),
                Err(e) if e.kind() == io::ErrorKind::NotFound => None,
                Err(e) => return Err(format!("{}: {e}", path.display())),
            };
            lines.push_str(&observed_line(entry, content.as_deref()));
            lines.push('\n');
        }
        write(&worktree.join(into.as_str()), lines.as_bytes())?;
    }

    if let Some(branch) = &reply.switch_branch {
        git(&["switch", "--quiet", "-c", branch], None)?;
    }
    for (path, content) in &reply.files {
        write(&worktree.join(path.as_str()), content.as_bytes())?;
    }
    if let Some(message) = &reply.commit {
        let mut add = vec!["add", "--"];
        add.extend(reply.files.keys().map(RelativePath::as_str));
        git(&add, None)?;
        let date = reply.date.map(|date| format!("{} +0000", date.as_second()));
        git(&["commit", "--quiet", "-m", message], date.as_deref())?;
    }
    if !reply.main_checkout.is_empty() {
        let main = main_checkout(&worktree)?;
        for (path, content) in &reply.main_checkout {
            write(&main.join(path.as_str()), content.as_bytes())?;
        }
    }

    if let Some(prepared) = &reply.result {
        let content = fs::read(prepared).map_err(|e| format!("{}: {e}", prepared.display()))?;
        write(&worktree.join(brief.result_path.as_str()), &content)?;
    }

    if let Some(text) = &reply.stdout {
        print!("{text}");
    }
    if let Some(text) = &reply.stderr {
        eprint!("{text}");
    }
    if let Some(reset) = reply.usage_limit {
        eprintln!("{}", usage_limit_line(reset));
    }
    if reply.usage_limit_unknown {
        eprintln!("{USAGE_LIMIT_UNKNOWN}");
    }
    // A session's run reports the whole session's cost, as a resumed Claude
    // Code session does (C18), and its transcript keeps it.
    let reported = match &session {
        Some(session) => {
            let total = session.prior_micros + reply.cost_micros.unwrap_or(0);
            let mut lines = fs::read_to_string(&session.transcript).unwrap_or_default();
            lines.push_str(&format!("{total}\n"));
            write(&session.transcript, lines.as_bytes())?;
            reply.cost_micros.map(|_| total)
        }
        None => reply.cost_micros,
    };
    if let Some(micros) = reported {
        eprintln!("{}", cost_line(micros));
    }
    Ok(reply.exit_code())
}

fn read(path: &Path) -> Result<String, String> {
    fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))
}

fn write(path: &Path, content: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    fs::write(path, content).map_err(|e| format!("{}: {e}", path.display()))
}

/// Runs git in the worktree, with the environment the runner gave this
/// process, and `date` as the author and committer date when given.
fn git(args: &[&str], date: Option<&str>) -> Result<Vec<u8>, String> {
    let mut command = Command::new("git");
    command.args(args).stdin(Stdio::null());
    if let Some(date) = date {
        command
            .env("GIT_AUTHOR_DATE", date)
            .env("GIT_COMMITTER_DATE", date);
    }
    let output = command.output().map_err(|e| format!("git: {e}"))?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(format!(
            "git {} failed with {}: {}",
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

/// The main checkout: the folder holding the repository's common git
/// directory, which git names relative to the worktree or in full.
fn main_checkout(worktree: &Path) -> Result<PathBuf, String> {
    let common = git(&["rev-parse", "--git-common-dir"], None)?;
    let common = worktree.join(String::from_utf8_lossy(&common).trim());
    common
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| format!("{} has no parent folder", common.display()))
}
