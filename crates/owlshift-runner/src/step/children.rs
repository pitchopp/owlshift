//! The children `owlshift watch --drain` runs its steps in (OWL-399,
//! architecture section 5, "The step, in a child process", "Stopping").
//!
//! Each child is the root of a process tree of watch's
//! ([`owlshift_platform::process::ProcessTree`]): a process group of its own, registered with the signal
//! policy and the sentinel every command shares, so a stop "now" or a hard
//! kill of watch stops it, and the child's own sentinel then stops the runs
//! it started. Since OWL-161 a checked stop of watch sends the child SIGTERM
//! instead, and it stops and checks its runs itself. Since OWL-401 watch starts a
//! child without waiting for it ([`Children::start`]) and polls each that
//! runs ([`Running::poll`]): it prints each line the child writes after its
//! ticket, sees the child's end without reaping it, stops what it left, then
//! withdraws and reaps it, and reads its outcome record.

use std::io::Write;
use std::process::Command;
use std::thread;
use std::time::Duration;

use owlshift_contracts::ids::TicketId;
use owlshift_contracts::step::{StepEnvelope, StepOutcome};
use owlshift_platform::process::AtCheckedStop;

use crate::daemon::child::{Ended as ChildEnded, Supervised};
use crate::project::ProjectDirs;
use crate::slots::MachineSlots;

/// How often watch looks at a running child between two lines.
const POLL: Duration = Duration::from_millis(100);

/// Makes the command of a step's child, from its ticket and its envelope as
/// one line of JSON.
pub type ChildCommand<'a> = dyn Fn(&TicketId, &str) -> Command + 'a;

/// How watch starts the child of a step: for `owlshift`, the binary itself
/// with `continue <TICKET> --step <envelope>`.
pub struct Children<'a> {
    command: Box<ChildCommand<'a>>,
    /// The machine's run slots, which watch counts before it starts a
    /// child (OWL-409): none for children that take none.
    slots: Option<MachineSlots>,
}

impl<'a> Children<'a> {
    /// The children `command` makes.
    pub fn new(command: impl Fn(&TicketId, &str) -> Command + 'a) -> Self {
        Self {
            command: Box::new(command),
            slots: None,
        }
    }

    /// These children, each of which takes one of `slots` before its step
    /// (OWL-409): watch counts them before it starts one.
    pub fn with_slots(self, slots: MachineSlots) -> Self {
        Self {
            slots: Some(slots),
            ..self
        }
    }

    /// The machine's run slots the children take, if they take any.
    pub fn slots(&self) -> Option<&MachineSlots> {
        self.slots.as_ref()
    }

    /// The command of `envelope`'s child.
    pub fn command(&self, envelope: &StepEnvelope) -> Command {
        (self.command)(&envelope.ticket, &envelope.render())
    }

    /// Starts `envelope`'s step in a child, without waiting for it
    /// (OWL-401): watch polls it ([`Running::poll`]) while its other steps'
    /// children run. Why it could not start otherwise.
    pub fn start(&self, envelope: &StepEnvelope) -> Result<Running, String> {
        let command = self.command(envelope);
        // Told at a checked stop, so that it stops its own runs; never
        // started once any stop is asked (OWL-161).
        let child = Supervised::start(command, envelope.ticket.as_ref(), AtCheckedStop::Told)
            .map_err(|error| format!("it could not start: {error}"))?;
        Ok(Running {
            ticket: envelope.ticket.clone(),
            attempt: envelope.attempt.clone(),
            child,
        })
    }
}

/// A step's child in flight (OWL-401): started by [`Children::start`],
/// polled by watch until it ended.
pub struct Running {
    ticket: TicketId,
    attempt: String,
    child: Supervised,
}

impl Running {
    /// The ticket whose step it runs.
    pub fn ticket(&self) -> &TicketId {
        &self.ticket
    }

    /// The step's attempt id.
    pub fn attempt(&self) -> &str {
        &self.attempt
    }

    /// Prints the lines the child wrote since the last poll, its standard
    /// output's on `out` and its standard error's on watch's, each after
    /// the ticket; then, once the child has ended and its output closed, or
    /// two seconds after its end while something it left holds the output,
    /// stops what it left, withdraws and reaps it, and returns its outcome
    /// record, read and removed from `dirs`, or why there is none. `None`
    /// while it runs.
    pub fn poll(
        &mut self,
        dirs: &ProjectDirs,
        out: &mut dyn Write,
    ) -> Option<Result<StepOutcome, String>> {
        let ended = self.child.poll(out)?;
        Some(self.finish(dirs, ended))
    }

    /// Waits for the child's end, polling, then returns as
    /// [`Running::poll`] does.
    pub fn wait(mut self, dirs: &ProjectDirs, out: &mut dyn Write) -> Result<StepOutcome, String> {
        loop {
            if let Some(ended) = self.poll(dirs, out) {
                return ended;
            }
            thread::sleep(POLL);
        }
    }

    /// What the child left is stopped while its root, ended or not, still
    /// holds its group's id; then the tree is withdrawn, the root reaped and
    /// its outcome record read.
    fn finish(&mut self, dirs: &ProjectDirs, ended: ChildEnded) -> Result<StepOutcome, String> {
        // A reap error comes before a watch error here, the opposite of
        // `ended.status()`: keep it so.
        let status = ended.reaped?;
        if let Some(error) = ended.watch_error {
            return Err(format!(
                "its end could not be watched ({error}), so it was stopped"
            ));
        }
        let ticket = &self.ticket;
        // What its watch handed a dispatch, if the child never took it.
        let _ = std::fs::remove_file(super::outside_file(dirs, ticket, &self.attempt));
        let path = super::outcome_file(dirs, ticket, &self.attempt);
        match super::take_outcome(&path) {
            Ok(Some(record)) if record.ticket == *ticket && record.attempt == self.attempt => {
                Ok(record)
            }
            Ok(Some(_)) => Err(format!(
                "it ended ({status}) with an outcome record of another step"
            )),
            Ok(None) => Err(format!("it ended ({status}) without its outcome record")),
            Err(reason) => Err(format!(
                "it ended ({status}) with an outcome record that does not read: {reason}"
            )),
        }
    }
}
