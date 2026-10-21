//! `owlshift why [TICKET]` (OWL-407): why the project's admitted tickets are
//! not running. Read-only: it takes neither the project's lock nor a
//! ticket's, reads no credential and calls neither the tracker nor the
//! forge, so it runs while `owlshift watch` does. The reading is
//! `owlshift_runner::why`.

use std::process::ExitCode;

use owlshift_contracts::ids::TicketId;
use owlshift_runner::config::Effective;
use owlshift_runner::on_demand;
use owlshift_runner::project::{self, ProjectDirs};
use owlshift_runner::why;

use crate::fail;

pub fn run(config: &Effective, ticket: Option<&str>) -> ExitCode {
    match report(config, ticket) {
        Ok(text) => {
            print!("{text}");
            ExitCode::SUCCESS
        }
        Err(refusal) => fail(&refusal),
    }
}

fn report(config: &Effective, ticket: Option<&str>) -> Result<String, String> {
    let ticket = ticket
        .map(TicketId::new)
        .transpose()
        .map_err(|error| error.to_string())?;
    let (project_file, _) = crate::do_cmd::loaded_project(config, "`owlshift why`")?;
    let root = project_file.parent().unwrap_or(project_file);
    let git = project::runner_git();
    let repo = on_demand::check_origin(&project::origin_url(&git, root)?)?;
    let data_dir = crate::data_dir()?;
    let dirs = ProjectDirs::new(&data_dir, &repo);
    let report = why::gather(
        &data_dir,
        &dirs,
        &repo.to_string(),
        ticket,
        crate::do_cmd::concurrent_runs(&config.personal),
        &owlshift_runner::usage::caps_of(&config.personal),
    )
    .map_err(|error| format!("cannot read the event log: {error}"))?;
    Ok(why::render(&report))
}
