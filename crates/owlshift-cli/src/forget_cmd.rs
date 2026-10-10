//! `owlshift forget TICKET` (OWL-202): opens what forgetting a ticket's
//! record needs, the project's dedicated checkout and the event log, and
//! hands it to `owlshift_runner::forget`. No credential is read: it never
//! opens the keychain, the tracker or the forge.

use std::io;
use std::process::ExitCode;

use owlshift_contracts::ids::TicketId;
use owlshift_runner::config::Effective;
use owlshift_runner::events::{EventLog, EventSink, printable};
use owlshift_runner::forget;
use owlshift_runner::on_demand;
use owlshift_runner::project::{self, ProjectDirs};

use crate::fail;

pub fn run(config: &Effective, ticket: &str) -> ExitCode {
    let mut stdout = io::stdout();
    match run_with(config, ticket, &mut stdout) {
        Ok(forgotten) => {
            println!("\n{}", printable(&forgotten));
            ExitCode::SUCCESS
        }
        Err(refusal) => fail(&refusal),
    }
}

/// Forgets `ticket`'s record, its event printed on `out`; the summary to
/// print, or why not.
fn run_with(config: &Effective, ticket: &str, out: &mut dyn io::Write) -> Result<String, String> {
    let ticket = TicketId::new(ticket).map_err(|error| error.to_string())?;
    let (project_file, project) = crate::do_cmd::loaded_project(config, "`owlshift forget`")?;
    on_demand::check_team(project, &ticket)?;
    let root = project_file.parent().unwrap_or(project_file);
    let git = project::runner_git();
    let repo = on_demand::check_origin(&project::origin_url(&git, root)?)?;
    let data_dir = crate::data_dir()?;
    let dirs = ProjectDirs::new(&data_dir, &repo);
    let mut sink = EventSink::new(repo.to_string(), EventLog::in_dir(&data_dir), out);
    forget::forget(&git, &dirs, &ticket, &mut sink)
        .map(|forgotten| forgotten.to_string())
        .map_err(|stop| stop.to_string())
}
