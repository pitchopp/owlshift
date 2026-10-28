//! A child a process supervises, the one type behind the daemon's watches
//! and a drain's step children (`step::children`, OWL-453): the root of a
//! process tree of the supervisor's ([`ProcessTree`]), its own process group
//! registered with the signal policy and sentinel, so a stop "now" or a hard
//! kill of the supervisor stops it, and its own sentinel then stops what it
//! started. A checked stop of the supervisor sends it SIGTERM, a checked stop
//! of its own ([`AtCheckedStop::Told`]), or refuses to start it, as the
//! caller chooses.
//!
//! It is started without waiting for it ([`Supervised::start`]) and polled
//! ([`Supervised::poll`]): each line it writes is printed after its label,
//! its end is seen without reaping it, what it left in its group is stopped,
//! then it is withdrawn and reaped. What its owner reads once it has ended,
//! a step's outcome record, is the owner's.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use owlshift_platform::process::{AtCheckedStop, ExitWatch, ProcessTree};

use crate::events::printable;

/// How long a child's output may stay open once it has ended, held by
/// something it left: then what it left is stopped.
const GRACE: Duration = Duration::from_secs(2);

/// Which of the child's streams a line came from.
#[derive(Clone, Copy)]
enum Stream {
    Out,
    Err,
}

/// What a reader of the child's output tells its owner.
enum Output {
    Line(Stream, String),
    Closed,
}

/// A child under way.
pub(crate) struct Supervised {
    label: String,
    child: Child,
    /// Its process tree, until it is withdrawn.
    tree: Option<ProcessTree>,
    lines: Receiver<Output>,
    exit: io::Result<ExitWatch>,
    /// Its streams still open.
    open: u8,
    started: Instant,
    /// When its end was seen.
    ended: Option<Instant>,
    /// The last line it wrote on its standard error.
    last_error: Option<String>,
}

/// How a child ended.
#[derive(Debug)]
pub(crate) struct Ended {
    /// Its exit status as reaped, or why it could not be read.
    pub reaped: Result<ExitStatus, String>,
    /// Why its end could not be watched, so that it was stopped.
    pub watch_error: Option<String>,
    /// The last line it wrote on its standard error: a refusal says why there.
    pub last_error: Option<String>,
    /// How long it ran.
    pub ran_for: Duration,
}

impl Ended {
    /// Its exit status, or why it could not be read; an end that could not
    /// be watched comes before how it was reaped.
    pub fn status(&self) -> Result<ExitStatus, String> {
        match &self.watch_error {
            Some(error) => Err(format!(
                "its end could not be watched ({error}), so it was stopped"
            )),
            None => self.reaped.clone(),
        }
    }
}

impl Supervised {
    /// Starts `command` as a child labelled `label`, its standard input
    /// closed and its output read, treated as `at` says at a checked stop:
    /// refused, for a child [`AtCheckedStop::Told`], once any stop is asked.
    pub fn start(mut command: Command, label: &str, at: AtCheckedStop) -> io::Result<Self> {
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let (mut child, tree) = ProcessTree::spawn_for(&mut command, at)?;
        drop(command);
        let (sender, lines) = mpsc::channel();
        read_lines(child.stdout.take(), Stream::Out, sender.clone());
        read_lines(child.stderr.take(), Stream::Err, sender);
        let exit = ExitWatch::start(&child);
        Ok(Self {
            label: label.to_owned(),
            child,
            tree: Some(tree),
            lines,
            exit,
            open: 2,
            started: Instant::now(),
            ended: None,
            last_error: None,
        })
    }

    /// Its process id.
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Prints the lines the child wrote since the last poll, its standard
    /// output's on `out` and its standard error's on the process's own, each
    /// after its label; then, once it has ended and its output closed, or two
    /// seconds after its end while something it left holds the output,
    /// stops what it left, withdraws and reaps it: how it ended. `None` while
    /// it runs.
    pub fn poll(&mut self, out: &mut dyn Write) -> Option<Ended> {
        while self.open > 0 {
            match self.lines.try_recv() {
                Ok(Output::Line(stream, line)) => {
                    let shown = printable(&format!("{}: {line}", self.label));
                    // Never a panic: the output may be gone, a closed terminal being a
                    // checked stop (OWL-161) whose children still end through here.
                    let _ = match stream {
                        Stream::Out => writeln!(out, "{shown}"),
                        Stream::Err => {
                            if !line.trim().is_empty() {
                                self.last_error = Some(line);
                            }
                            writeln!(io::stderr(), "{shown}")
                        }
                    };
                }
                Ok(Output::Closed) => self.open -= 1,
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => self.open = 0,
            }
        }
        let mut watch_error = None;
        if self.ended.is_none() {
            let exited = match &self.exit {
                Ok(exit) => exit.exited_by(&self.child, Instant::now()),
                Err(error) => Err(io::Error::new(error.kind(), error.to_string())),
            };
            match exited {
                Ok(true) => self.ended = Some(Instant::now()),
                Ok(false) => {}
                // Its end cannot be watched: stop it rather than wait
                // without end.
                Err(error) => watch_error = Some(error.to_string()),
            }
        }
        match self.ended {
            _ if watch_error.is_some() => {}
            Some(_) if self.open == 0 => {}
            Some(at) if at.elapsed() >= GRACE => {}
            _ => return None,
        }
        Some(self.finish(watch_error))
    }

    /// Stops what the child left while its root, ended or not, still holds
    /// its group's id; then withdraws the tree and reaps the root.
    fn finish(&mut self, watch_error: Option<String>) -> Ended {
        let ran_for = self.ended.unwrap_or_else(Instant::now) - self.started;
        let reaped = match self.tree.take() {
            Some(tree) => {
                let _ = tree.kill();
                tree.reap(&mut self.child)
                    .map_err(|error| format!("its end could not be read: {error}"))
            }
            None => Err("it was already withdrawn".to_owned()),
        };
        Ended {
            reaped,
            watch_error,
            last_error: self.last_error.take(),
            ran_for,
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
