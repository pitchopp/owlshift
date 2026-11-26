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
//!
//! A child may be held ([`Supervised::hold`], OWL-650): its lines are kept
//! rather than printed, until its owner releases them or drops them with its
//! end, as the daemon does for the quiet retry of a project that waits for a
//! person.

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
#[derive(Clone, Copy, Debug)]
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
    /// While it is held: the lines it wrote, each with its stream, kept
    /// rather than printed.
    held: Option<Vec<(Stream, String)>>,
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
    /// The lines it wrote while held and that were not released, each with
    /// its stream, as they would have been printed.
    held: Vec<(Stream, String)>,
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

    /// Prints the lines it wrote while held, as [`Supervised::poll`] would
    /// have: its standard output's on `out`, its standard error's on the
    /// process's own. Once only: they are gone after.
    pub fn write_held(&mut self, out: &mut dyn Write) {
        write_lines(std::mem::take(&mut self.held), out);
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
            held: None,
        })
    }

    /// Keeps the lines the child writes from now on rather than print them,
    /// until [`release`](Self::release); its end carries those still kept.
    pub fn hold(&mut self) {
        self.held.get_or_insert_with(Vec::new);
    }

    /// Prints the lines kept while it was held, in order, then prints each
    /// new line as it comes again.
    pub fn release(&mut self, out: &mut dyn Write) {
        if let Some(held) = self.held.take() {
            write_lines(held, out);
        }
    }

    /// Its process id.
    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Stops the child and everything in its group now, as a stop "now"
    /// does; [`Supervised::poll`] then sees its end and reaps it. The web
    /// UI's end once the daemon's watches have ended (OWL-467).
    pub fn kill(&self) -> io::Result<()> {
        match &self.tree {
            Some(tree) => tree.kill(),
            None => Ok(()),
        }
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
                    if matches!(stream, Stream::Err) && !line.trim().is_empty() {
                        self.last_error = Some(line);
                    }
                    match &mut self.held {
                        Some(held) => held.push((stream, shown)),
                        None => write_lines([(stream, shown)], out),
                    }
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
            held: self.held.take().unwrap_or_default(),
        }
    }
}

/// Prints each of `lines`, already shown after its label: standard
/// output's on `out`, standard error's on the process's own.
fn write_lines(lines: impl IntoIterator<Item = (Stream, String)>, out: &mut dyn Write) {
    for (stream, shown) in lines {
        // Never a panic: the output may be gone, a closed terminal being a
        // checked stop (OWL-161) whose children still end through here.
        let _ = match stream {
            Stream::Out => writeln!(out, "{shown}"),
            Stream::Err => writeln!(io::stderr(), "{shown}"),
        };
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// `script` run by `sh` as a child labelled `t`, polled until it ends,
    /// held from its start when `held`; what was printed on `out` meanwhile,
    /// and how it ended.
    fn polled(script: &str, held: bool) -> (String, Ended) {
        let mut command = Command::new("sh");
        command.args(["-c", script]);
        let mut child = Supervised::start(command, "t", AtCheckedStop::Left).unwrap();
        if held {
            child.hold();
        }
        let mut out = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(ended) = child.poll(&mut out) {
                return (String::from_utf8(out).unwrap(), ended);
            }
            assert!(Instant::now() < deadline, "the child never ended");
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// OWL-650: a held child prints nothing; its end carries its lines and
    /// its last error, printed once, when its owner writes them.
    #[test]
    fn a_held_childs_lines_are_carried_by_its_end() {
        let (printed, mut ended) = polled("echo one; echo two; echo why >&2", true);
        assert_eq!(printed, "");
        assert_eq!(ended.last_error.as_deref(), Some("why"));
        assert!(!ended.held.is_empty());
        let mut out = Vec::new();
        ended.write_held(&mut out);
        assert_eq!(String::from_utf8(out).unwrap(), "t: one\nt: two\n");
        assert!(ended.held.is_empty());

        let (printed, ended) = polled("echo one", false);
        assert_eq!(printed, "t: one\n");
        assert!(ended.held.is_empty());
    }

    /// OWL-650: a released child prints what it held, in order, then each
    /// new line as it comes.
    #[test]
    fn a_released_child_prints_what_it_held_then_the_rest() {
        let mut command = Command::new("sh");
        command.args(["-c", "echo one; sleep 1; echo two"]);
        let mut child = Supervised::start(command, "t", AtCheckedStop::Left).unwrap();
        child.hold();
        let mut out = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(30);
        while child.held.as_ref().is_some_and(Vec::is_empty) {
            assert!(
                child.poll(&mut out).is_none(),
                "ended before its first line"
            );
            assert!(Instant::now() < deadline, "no first line");
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(String::from_utf8_lossy(&out), "");
        child.release(&mut out);
        assert_eq!(String::from_utf8_lossy(&out), "t: one\n");
        let ended = loop {
            if let Some(ended) = child.poll(&mut out) {
                break ended;
            }
            assert!(Instant::now() < deadline, "the child never ended");
            thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(String::from_utf8(out).unwrap(), "t: one\nt: two\n");
        assert!(ended.held.is_empty());
    }
}
