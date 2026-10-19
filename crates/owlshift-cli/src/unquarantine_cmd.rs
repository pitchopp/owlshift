//! `owlshift unquarantine` (OWL-368): finds the project of the current
//! repository from its `origin`, then hands it to
//! `owlshift_runner::quarantine::clear`, which shows the `unverified`
//! marker and clears it once a person types the project's name. It reads
//! no credential and needs no valid `owlshift.toml`, so a broken file does
//! not keep a project quarantined. It runs only at a terminal: a script or
//! a non-interactive shell gets a refusal, and there is no flag to skip the
//! question.

use std::io::{self, BufRead, IsTerminal, Write};
use std::path::Path;
use std::process::ExitCode;

use owlshift_runner::config::{Effective, FileState};
use owlshift_runner::events::{EventLog, EventSink, printable};
use owlshift_runner::on_demand;
use owlshift_runner::project::{self, ProjectDirs};
use owlshift_runner::quarantine::{self, Cleared};

use crate::fail;

pub fn run(config: &Effective) -> ExitCode {
    match run_with(config) {
        Ok(Cleared::Nothing) => {
            println!("Nothing to clear: the project is not quarantined.");
            ExitCode::SUCCESS
        }
        Ok(Cleared::Kept) => {
            println!("\nKept: the quarantine stands, and Owlshift still refuses the project.");
            ExitCode::FAILURE
        }
        Ok(Cleared::Cleared { .. }) => {
            println!(
                "\nCleared: the marker is gone, and the next command or watch pass works the \
                 project again."
            );
            ExitCode::SUCCESS
        }
        Err(refusal) => fail(&refusal),
    }
}

fn run_with(config: &Effective) -> Result<Cleared, String> {
    let root = repository_root(&config.project)?;
    let git = project::runner_git();
    let repo = on_demand::check_origin(&project::origin_url(&git, root)?)?;
    if !io::stdin().is_terminal() {
        return Err(
            "`owlshift unquarantine` lifts a security marker, so a person confirms it at a \
             terminal: run it in one"
                .to_owned(),
        );
    }
    let data_dir = crate::data_dir()?;
    let dirs = ProjectDirs::new(&data_dir, &repo);
    let mut stdout = io::stdout();
    let mut sink = EventSink::new(repo.to_string(), EventLog::in_dir(&data_dir), &mut stdout);
    quarantine::clear(&dirs, &mut ask, &mut sink)
}

/// The repository the project file is looked for in.
fn repository_root<T>(project: &FileState<T>) -> Result<&Path, String> {
    let path = match project {
        FileState::Loaded { path, .. } | FileState::Invalid { path, .. } | FileState::Absent(path) => {
            path
        }
        FileState::NotApplicable(reason) => {
            return Err(format!(
                "`owlshift unquarantine` runs in the project's git repository: {reason}"
            ));
        }
        FileState::Unavailable(reason) => return Err(reason.clone()),
    };
    path.parent()
        .ok_or_else(|| format!("{} is in no folder", path.display()))
}

/// Shows `prompt`, safe for a terminal, and reads one line of standard
/// input.
fn ask(prompt: &str) -> io::Result<String> {
    let mut stdout = io::stdout();
    write!(stdout, "{}", printable(prompt))?;
    stdout.flush()?;
    let mut line = String::new();
    io::stdin().lock().read_line(&mut line)?;
    Ok(line)
}
