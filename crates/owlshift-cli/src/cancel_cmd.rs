//! `owlshift cancel TICKET [--reason TEXT]` (OWL-474, architecture section
//! 5, "The local web UI", "Cancel"): writes the ticket's cancel file, which
//! its step under way watches and the project's watch parks it on. The
//! checks and the writing are `owlshift_runner::cancel::ask`. No credential
//! is read: it never opens the keychain, the tracker or the forge.

use std::process::ExitCode;

use jiff::Timestamp;
use owlshift_contracts::ids::TicketId;
use owlshift_contracts::service::{BY_CANCEL, Request};
use owlshift_runner::cancel::{self, Asked};
use owlshift_runner::config::Effective;
use owlshift_runner::events::printable;
use owlshift_runner::on_demand;
use owlshift_runner::project::{self, ProjectDirs};

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
    let asked = cancel::ask(&git, &dirs, &ticket, &request)?;
    Ok(said(&ticket, &asked))
}

/// What the command says of the cancel of `ticket` it wrote.
fn said(ticket: &TicketId, asked: &Asked) -> Vec<String> {
    let mut lines = vec![format!(
        "Asked to cancel {ticket}: its step under way stops within 5 seconds, its runs stopped and \
         checked, and the project's watch then parks it, with a PARKED comment on the ticket. \
         `owlshift continue {ticket}` restarts it from where it was stopped. A step typed in a \
         terminal (`owlshift continue`, `owlshift do`) is not stopped."
    )];
    if let Some(before) = &asked.replaced {
        lines.push(match before {
            Some(before) => format!(
                "It replaces the cancel asked by {} at {}: {}",
                before.by, before.at, before.why
            ),
            None => "It replaces a cancel file that did not say who wrote it.".to_owned(),
        });
    }
    if asked.unkept {
        lines.push(format!(
            "Owlshift keeps no record of {ticket} yet: should its step stop before its first run \
             has kept anything, nothing is parked, and the file is dropped."
        ));
    }
    if asked.quarantined {
        lines.push(
            "The project is quarantined: no watch parks anything until `owlshift unquarantine` \
             lifts it."
                .to_owned(),
        );
    }
    if !asked.watching {
        lines.push(format!(
            "No watch runs on this project now: {ticket} is parked at the first pass of one, \
             such as the background service's (`owlshift start`)."
        ));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The command says what follows, and each standing that changes it.
    #[test]
    fn it_says_what_follows_and_what_holds_it() {
        let ticket = TicketId::new("WEB-1").unwrap();
        let plain = Asked {
            replaced: None,
            watching: true,
            quarantined: false,
            unkept: false,
        };
        let lines = said(&ticket, &plain);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].starts_with("Asked to cancel WEB-1: its step under way stops"));
        let held = Asked {
            replaced: Some(None),
            watching: false,
            quarantined: true,
            unkept: true,
        };
        let lines = said(&ticket, &held).join("\n");
        assert!(lines.contains("It replaces a cancel file"), "{lines}");
        assert!(lines.contains("keeps no record of WEB-1 yet"), "{lines}");
        assert!(lines.contains("The project is quarantined"), "{lines}");
        assert!(
            lines.contains("No watch runs on this project now"),
            "{lines}"
        );
    }
}
