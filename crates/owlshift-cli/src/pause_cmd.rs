//! `owlshift pause [OWNER/REPO]` and `owlshift resume [OWNER/REPO]`
//! (OWL-438, architecture section 5, "Pause"): write and remove the pause
//! of every project of the machine, `<data>/service/pause`, or of one
//! project, `<project data>/pause`. Every watch reads them at each pass and
//! before each step: while one stands it dispatches nothing and starts no
//! step, nor a merge on green, and the steps under way finish. The writing
//! and the words are `owlshift_runner::controls`, which the web UI's Pause
//! and Resume share (OWL-473).

use std::process::ExitCode;

use jiff::Timestamp;
use owlshift_adapters::forge::Repo;
use owlshift_contracts::service::{BY_PAUSE, Request};
use owlshift_runner::controls;
use owlshift_runner::events::printable;
use owlshift_runner::voice::Voice;

use crate::{data_dir, fail};

/// The lines `done` says, printed, or its refusal.
fn print(done: Result<Vec<String>, String>) -> ExitCode {
    match done {
        Ok(lines) => {
            for line in lines {
                println!("{}", printable(&line));
            }
            ExitCode::SUCCESS
        }
        Err(error) => fail(&error),
    }
}

/// `owlshift pause [OWNER/REPO] [--reason TEXT]`.
pub fn pause(repo: Option<&str>, reason: Option<&str>) -> ExitCode {
    print(data_dir().and_then(|data| {
        let why = reason.unwrap_or("asked by `owlshift pause`");
        let request = Request::new(BY_PAUSE, Timestamp::now(), why);
        let target = repo
            .map(|repo| Repo::parse(repo).map_err(|error| error.to_string()))
            .transpose()?;
        controls::pause(&data, target.as_ref(), &request, Voice::Terminal)
    }))
}

/// `owlshift resume [OWNER/REPO]`.
pub fn resume(repo: Option<&str>) -> ExitCode {
    print(data_dir().and_then(|data| {
        let target = repo
            .map(|repo| Repo::parse(repo).map_err(|error| error.to_string()))
            .transpose()?;
        controls::resume(&data, target.as_ref(), Voice::Terminal, None)
    }))
}
