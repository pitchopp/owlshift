//! `owlshift status` (OWL-438): what the background service serves and
//! does. Read-only: it waits on no lock, reads no credential and calls
//! neither the tracker nor the forge, so it runs beside the daemon and its
//! watches. The reading is `owlshift_runner::status`; with `--waits`
//! (OWL-459), `owlshift_runner::status::waits`.

use std::process::ExitCode;

use jiff::Timestamp;
use jiff::tz::TimeZone;
use owlshift_adapters::forge::Repo;
use owlshift_runner::config::Effective;
use owlshift_runner::status;

use crate::{data_dir, fail};

pub fn run(config: &Effective) -> ExitCode {
    let data = match data_dir() {
        Ok(data) => data,
        Err(error) => return fail(&error),
    };
    let report = status::gather(
        &data,
        crate::do_cmd::concurrent_runs(&config.personal),
        &owlshift_runner::usage::caps_of(&config.personal),
        Timestamp::now(),
    );
    print!("{}", status::render(&report, &TimeZone::system()));
    ExitCode::SUCCESS
}

/// `owlshift status --waits [OWNER/REPO] [--json]`.
pub fn waits(config: &Effective, repo: Option<&str>, json: bool) -> ExitCode {
    let only = match repo.map(Repo::parse).transpose() {
        Ok(only) => only,
        Err(error) => return fail(&error.to_string()),
    };
    let data = match data_dir() {
        Ok(data) => data,
        Err(error) => return fail(&error),
    };
    let read = status::waits::gather(
        &data,
        only.as_ref(),
        crate::do_cmd::concurrent_runs(&config.personal),
        Timestamp::now(),
    );
    let waits = status::waits::build(&read);
    if json {
        print!("{}", waits.render());
    } else {
        print!("{}", status::waits::render(&waits, &TimeZone::system()));
    }
    ExitCode::SUCCESS
}
