//! A child the daemon supervises: a served project's watch, the root of a
//! process tree of the daemon's ([`ProcessTree`]), its own process group
//! registered with the daemon's signal policy and sentinel, so a stop "now"
//! or a hard kill of the daemon stops it, and its own sentinel then stops
//! its steps. A checked stop of the daemon sends it SIGTERM, a checked stop
//! of its own ([`AtCheckedStop::Told`]).
//!
//! It is started without waiting for it ([`Supervised::start`]) and polled
//! ([`Supervised::poll`]): each line it writes is printed after its label,
//! its end is seen without reaping it, what it left in its group is stopped,
//! then it is withdrawn and reaped. The same steps as a drain's step child
//! (`step::children`), without the step's outcome record.

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

/// What a reader of the child's output tells the daemon.
enum Output {
    Line(Stream, String),
    Closed,
}

/// A child under way.
pub struct Supervised {
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
pub struct Ended {
    /// Its exit status, or why it could not be read.
    pub status: Result<ExitStatus, String>,
    /// The last line it wrote on its standard error: a refusal says why there.
    pub last_error: Option<String>,
    /// How long it ran.
    pub ran_for: Duration,
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
    /// output's on `out` and its standard error's on the daemon's, each
    /// after its label; then, once it has ended and its output closed, or two
    /// seconds after its end while something it left holds the output,
    /// stops what it left, withdraws and reaps it: how it ended. `None` while
    /// it runs.
    pub fn poll(&mut self, out: &mut dyn Write) -> Option<Ended> {
        while self.open > 0 {
            match self.lines.try_recv() {
                Ok(Output::Line(stream, line)) => {
                    let shown = printable(&format!("{}: {line}", self.label));
                    // Never a panic: the daemon's output may be gone.
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
        let status = match self.tree.take() {
            Some(tree) => {
                let _ = tree.kill();
                tree.reap(&mut self.child)
                    .map_err(|error| format!("its end could not be read: {error}"))
            }
            None => Err("it was already withdrawn".to_owned()),
        };
        let status = match watch_error {
            Some(error) => Err(format!(
                "its end could not be watched ({error}), so it was stopped"
            )),
            None => status,
        };
        Ended {
            status,
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
