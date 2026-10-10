//! `owlshift logs [TICKET] [--last N] [--follow]`: the events `owlshift do`
//! and the other commands recorded, from the data directory's event log
//! (`owlshift_runner::events`).

use std::io::{self, ErrorKind};
use std::num::NonZeroUsize;
use std::process::ExitCode;
use std::time::Duration;

use owlshift_contracts::ids::TicketId;
use owlshift_runner::events::{EventLog, Follow, print, printable};

use crate::fail;

/// How often `--follow` looks for new events.
const POLL: Duration = Duration::from_millis(500);

pub fn run(ticket: Option<&str>, last: Option<NonZeroUsize>, follow: bool) -> ExitCode {
    let ticket = match ticket.map(TicketId::new).transpose() {
        Ok(ticket) => ticket,
        Err(error) => return fail(&error.to_string()),
    };
    let data_dir = match crate::data_dir() {
        Ok(data_dir) => data_dir,
        Err(error) => return fail(&error),
    };
    let log = EventLog::in_dir(&data_dir);
    // `--follow` runs until Ctrl-C ends the process.
    let never = || false;
    let follow_mode = follow.then_some(Follow {
        poll: POLL,
        stop: &never,
    });
    let printed = print(
        log.path(),
        ticket.as_ref(),
        &mut io::stdout().lock(),
        &mut io::stderr(),
        last,
        follow_mode,
    );
    match printed {
        Ok(0) => {
            let about = ticket.map_or_else(String::new, |id| format!(" for {id}"));
            eprintln!(
                "{}",
                printable(&format!(
                    "owlshift: no event recorded{about} yet in {}",
                    log.path().display()
                ))
            );
            ExitCode::SUCCESS
        }
        Ok(_) => ExitCode::SUCCESS,
        // The reader went away, as with `owlshift logs | head`.
        Err(error) if error.kind() == ErrorKind::BrokenPipe => ExitCode::SUCCESS,
        Err(error) => fail(&format!("{}: {error}", log.path().display())),
    }
}
