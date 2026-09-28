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
use owlshift_testkit::reply::{OWN_FAILURE, Reply, usage_limit_line};

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
    let worktree = std::env::current_dir().map_err(|e| format!("current directory: {e}"))?;

    thread::sleep(Duration::from_millis(reply.delay_ms));

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
