//! `owlshift cancel TICKET [--reason TEXT]` (OWL-474, architecture section
//! 5, "The local web UI", "Cancel"): writes the ticket's cancel file, which
//! its step under way watches and the project's watch parks it on. The
//! checks and the writing are `owlshift_runner::cancel::ask`. No credential
//! is read: it never opens the keychain, the tracker or the forge.

use std::process::ExitCode;

use jiff::Timestamp;
use owlshift_contracts::ids::TicketId;
use owlshift_contracts::service::{BY_CANCEL, Request};
use owlshift_runner::cancel;
use owlshift_runner::config::Effective;
use owlshift_runner::events::printable;
use owlshift_runner::on_demand;
use owlshift_runner::project::{self, ProjectDirs};
use owlshift_runner::voice::Voice;

use crate::fail;

/// `owlshift cancel TICKET [--reason TEXT]`.
pub fn run(config: &Effective, ticket: &str, reason: Option<&str>) -> ExitCode {
    match ask(config, ticket, reason) {
        Ok(lines) => {
            for line in lines {
                println!("{}", printable(&line));
            }
            ExitCode::SUCCESS
        }
        Err(refusal) => fail(&refusal),
    }
}

/// Writes the cancel of `ticket`: the lines to print, or why not.
fn ask(config: &Effective, ticket: &str, reason: Option<&str>) -> Result<Vec<String>, String> {
    let ticket = TicketId::new(ticket).map_err(|error| error.to_string())?;
    let (project_file, project) = crate::do_cmd::loaded_project(config, "`owlshift cancel`")?;
    on_demand::check_team(project, &ticket)?;
    let root = project_file.parent().unwrap_or(project_file);
    let git = project::runner_git();
    let repo = on_demand::check_origin(&project::origin_url(&git, root)?)?;
    let dirs = ProjectDirs::new(&crate::data_dir()?, &repo);
    let why = reason.unwrap_or("asked by `owlshift cancel`");
    let request = Request::new(BY_CANCEL, Timestamp::now(), why);
    let asked = cancel::ask(&git, &dirs, &ticket, &request, Voice::Terminal)?;
    Ok(cancel::said(&ticket, &asked, Voice::Terminal))
}
