//! The children `owlshift watch --drain` runs its steps in (OWL-399,
//! architecture section 5, "The step, in a child process", "Stopping").
//!
//! Each child is the root of a process tree of watch's
//! ([`ProcessTree`]): a process group of its own, registered with the signal
//! policy and the sentinel every command shares, so Ctrl-C, a closed
//! terminal, SIGTERM or a hard kill of watch stop it, and the child's own
//! sentinel then stops the runs it started. Watch prints each line the
//! child writes after its ticket, waits for the child's end without reaping
//! it, stops what it left, then withdraws and reaps it, and reads its
//! outcome record.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use owlshift_contracts::ids::TicketId;
use owlshift_contracts::step::{StepEnvelope, StepOutcome};
use owlshift_platform::process::{ExitWatch, ProcessTree};

use crate::events::printable;
use crate::project::ProjectDirs;
use crate::slots::MachineSlots;

/// How often watch looks at a running child between two lines.
const POLL: Duration = Duration::from_millis(100);

/// How long a child's output may stay open once it has ended, held by
/// something it left: then what it left is stopped.
const GRACE: Duration = Duration::from_secs(2);

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

/// Which of the child's streams a line came from.
#[derive(Clone, Copy)]
enum Stream {
    Out,
    Err,
}

/// What a reader of the child's output tells watch.
enum Output {
    Line(Stream, String),
    Closed,
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

    /// Runs `envelope`'s step in a child, its standard output's lines
    /// printed on `out` and its standard error's on watch's, each after the
    /// ticket; returns its outcome record, read and removed, or why there is
    /// none.
    pub fn run(
        &self,
        dirs: &ProjectDirs,
        envelope: &StepEnvelope,
        out: &mut dyn Write,
    ) -> Result<StepOutcome, String> {
        let ticket = &envelope.ticket;
        let mut command = self.command(envelope);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let (mut child, tree) = ProcessTree::spawn(&mut command)
            .map_err(|error| format!("it could not start: {error}"))?;
        drop(command);
        let (sender, lines) = mpsc::channel();
        read_lines(child.stdout.take(), Stream::Out, sender.clone());
        read_lines(child.stderr.take(), Stream::Err, sender);
        let exit = ExitWatch::start(&child);
        let mut open = 2;
        let mut ended: Option<Instant> = None;
        let mut watch_error = None;
        loop {
            if open > 0 {
                match lines.recv_timeout(POLL) {
                    Ok(Output::Line(stream, line)) => {
                        let line = printable(&format!("{ticket}: {line}"));
                        match stream {
                            Stream::Out => {
                                let _ = writeln!(out, "{line}");
                            }
                            Stream::Err => eprintln!("{line}"),
                        }
                        continue;
                    }
                    Ok(Output::Closed) => open -= 1,
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => open = 0,
                }
            }
            if ended.is_none() {
                // With the output closed, the wait for the end is the poll.
                let until = if open > 0 {
                    Instant::now()
                } else {
                    Instant::now() + POLL
                };
                let exited = match &exit {
                    Ok(exit) => exit.exited_by(&child, until),
                    Err(error) => Err(io::Error::new(error.kind(), error.to_string())),
                };
                match exited {
                    Ok(true) => ended = Some(Instant::now()),
                    Ok(false) => {}
                    // Its end cannot be watched: stop it rather than wait
                    // without end.
                    Err(error) => {
                        watch_error = Some(error.to_string());
                        break;
                    }
                }
            }
            match ended {
                Some(_) if open == 0 => break,
                Some(at) if at.elapsed() >= GRACE => break,
                _ => {}
            }
        }
        // What the child left is stopped while its root, ended or not,
        // still holds its group's id; then the tree is withdrawn and the
        // root reaped.
        let _ = tree.kill();
        let status = tree
            .reap(&mut child)
            .map_err(|error| format!("its end could not be read: {error}"))?;
        if let Some(error) = watch_error {
            return Err(format!(
                "its end could not be watched ({error}), so it was stopped"
            ));
        }
        let path = super::outcome_file(dirs, ticket, &envelope.attempt);
        match super::take_outcome(&path) {
            Ok(Some(record)) if record.ticket == *ticket && record.attempt == envelope.attempt => {
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

/// Reads `stream` line by line on a thread of its own, sending each line,
/// then that it closed.
fn read_lines(stream: Option<impl Read + Send + 'static>, from: Stream, to: Sender<Output>) {
    thread::spawn(move || {
        if let Some(stream) = stream {
            let mut reader = BufReader::new(stream);
            let mut line = Vec::new();
            loop {
                line.clear();
                match reader.read_until(b'\n', &mut line) {
                    Ok(0) => break,
                    Ok(_) => {
                        let text = String::from_utf8_lossy(&line);
                        let text = text.trim_end_matches(['\n', '\r']).to_owned();
                        if to.send(Output::Line(from, text)).is_err() {
                            return;
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
        }
        let _ = to.send(Output::Closed);
    });
}
