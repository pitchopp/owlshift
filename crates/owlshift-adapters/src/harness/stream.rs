//! Driving a harness child that takes its prompt on standard input and
//! reports on standard output one JSON event per line: what the Claude Code
//! and Codex adapters share.
//!
//! [`drive`] writes the prompt and closes standard input, reads standard
//! output one line at a time into a [`Sink`], keeps the start of standard
//! error, and returns at most [`EXIT_GRACE`] after the child exits. Each
//! adapter reads the lines with its own transcript and judges the run.
//!
//! The runner's `drive_plain` stays apart: it sends no prompt and logs raw
//! bytes rather than lines.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::process::Child;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::Duration;

use clock::{Clock, Incoming, RealTime};

/// The most bytes kept from standard error; the rest is read and dropped, as
/// `owlshift_platform::process` does for its probes.
pub const STDERR_CAP: usize = 64 * 1024;

/// The longest output line parsed as an event. A longer line still reaches
/// the caller, in pieces, and is counted as oversized.
pub const LINE_CAP: usize = 16 * 1024 * 1024;

/// How long a harness driver keeps reading after the child has exited. A process the
/// child started may hold its output open; past this delay it is left behind
/// rather than waited on.
pub const EXIT_GRACE: Duration = Duration::from_secs(2);

const POLL: Duration = Duration::from_millis(20);

/// Where [`drive`] puts the lines of standard output.
pub(crate) trait Sink {
    /// One whole line, with its newline when it had one.
    fn line(&mut self, line: &[u8]);

    /// A line longer than [`LINE_CAP`] began: it is not parsed.
    fn oversized(&mut self);
}

/// What [`drive`] saw besides the lines.
#[derive(Debug)]
pub(crate) struct Driven {
    /// The exit code; `None` when a signal ended the process.
    pub exit_code: Option<i32>,
    /// Standard error, up to [`STDERR_CAP`] bytes.
    pub stderr: Vec<u8>,
    /// Whether the prompt was written whole and standard input closed.
    pub prompt_delivered: bool,
}

/// Sends `prompt` to a child spawned with three piped streams, reads its
/// output to the end and returns how it ended.
///
/// Every byte of standard output reaches `on_line`, in order, one line at a
/// time (a line longer than [`LINE_CAP`] comes in several pieces), so the
/// executor can log the run as it goes; whole lines also reach `sink`.
///
/// It returns at most [`EXIT_GRACE`] after the child exits, even if a process
/// the child started still holds the output open. To stop a run, stop the
/// child's process group (its id is `child.id()`, read before this call):
/// the output then closes and this returns.
pub(crate) fn drive(
    child: &mut Child,
    prompt: &str,
    on_line: impl FnMut(&[u8]),
    sink: &mut impl Sink,
) -> io::Result<Driven> {
    let not_piped = || {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "the harness child needs piped standard streams",
        )
    };
    let stdin = child.stdin.take().ok_or_else(not_piped)?;
    let stdout = child.stdout.take().ok_or_else(not_piped)?;
    let stderr = child.stderr.take().ok_or_else(not_piped)?;

    let prompt_sent = Incoming::new(send_prompt(stdin, prompt.as_bytes().to_vec()));
    let frames = Incoming::new(read_frames(stdout));
    let stderr = Incoming::new(read_capped(stderr));
    drive_with(&RealTime, child, prompt_sent, frames, stderr, on_line, sink)
}

/// What [`drive`] needs of the child once its streams are taken. A trait so
/// that a test can hand [`drive_with`] a child that has already exited.
trait Exiting {
    /// Whether the child has exited, without waiting for it.
    fn has_exited(&mut self) -> io::Result<bool>;

    /// Waits for the child to exit and returns its exit code, `None` when a
    /// signal ended it.
    fn exit_code(&mut self) -> io::Result<Option<i32>>;
}

impl Exiting for Child {
    fn has_exited(&mut self) -> io::Result<bool> {
        Ok(self.try_wait()?.is_some())
    }

    fn exit_code(&mut self) -> io::Result<Option<i32>> {
        Ok(self.wait()?.code())
    }
}

/// [`drive`] once the prompt writer and the two readers are started.
///
/// It reads and waits only through `clock` (see [`clock`]), so a test counts
/// the time it spends on a clock of its own, with no wall-clock bound.
fn drive_with<C: Clock>(
    clock: &C,
    child: &mut impl Exiting,
    prompt_sent: Incoming<io::Result<()>>,
    frames: Incoming<Frame>,
    stderr: Incoming<Vec<u8>>,
    mut on_line: impl FnMut(&[u8]),
    sink: &mut impl Sink,
) -> io::Result<Driven> {
    // One grace period in all, counted from the exit: the rest of standard
    // output, then standard error and the prompt share this one deadline, so
    // what the loop used is not given again to the other streams.
    let mut deadline = None;
    loop {
        match frames.recv_timeout(clock, POLL) {
            Ok(Frame::Line(line)) => {
                on_line(&line);
                sink.line(&line);
            }
            Ok(Frame::Oversized { piece, first }) => {
                on_line(&piece);
                if first {
                    sink.oversized();
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {}
        }
        if deadline.is_none() && child.has_exited()? {
            deadline = Some(clock.now() + EXIT_GRACE);
        }
        if deadline.is_some_and(|deadline| clock.until(deadline).is_zero()) {
            break;
        }
    }
    let exit_code = child.exit_code()?;
    // Standard output closed before the exit was seen: the grace period
    // starts now, the child having exited.
    let deadline = deadline.unwrap_or_else(|| clock.now() + EXIT_GRACE);
    let remaining = || clock.until(deadline);
    let stderr = stderr.recv_timeout(clock, remaining()).unwrap_or_default();
    let prompt_delivered = matches!(prompt_sent.recv_timeout(clock, remaining()), Ok(Ok(())));
    Ok(Driven {
        exit_code,
        stderr,
        prompt_delivered,
    })
}

/// Time as [`drive_with`] reads it and waits on it.
///
/// A module of its own so that `drive_with` cannot step around its clock: it
/// holds no [`Receiver`] it could wait on directly, only [`Incoming`] ends
/// waited on through a [`Clock`], and a clock's moments do not mix with
/// [`std::time::Instant`]. A second grace period can then only be spent on the
/// clock, where a test sees it.
mod clock {
    use std::ops::Add;
    use std::sync::mpsc::{Receiver, RecvTimeoutError};
    use std::time::{Duration, Instant};

    pub(super) trait Clock {
        /// A point in this clock's time.
        type Moment: Copy + Add<Duration, Output = Self::Moment>;

        fn now(&self) -> Self::Moment;

        /// How long until `deadline`: zero once it has passed.
        fn until(&self, deadline: Self::Moment) -> Duration;

        /// Waits at most `timeout` for the next value, as
        /// [`Receiver::recv_timeout`] does.
        fn recv_timeout<T>(
            &self,
            receiver: &Receiver<T>,
            timeout: Duration,
        ) -> Result<T, RecvTimeoutError>;
    }

    /// The monotonic clock [`super::drive`] runs on. Not the tracker-time
    /// clock of `owlshift_runner::on_demand`: this one only measures delays.
    pub(super) struct RealTime;

    impl Clock for RealTime {
        type Moment = Instant;

        fn now(&self) -> Instant {
            Instant::now()
        }

        fn until(&self, deadline: Instant) -> Duration {
            deadline.saturating_duration_since(Instant::now())
        }

        fn recv_timeout<T>(
            &self,
            receiver: &Receiver<T>,
            timeout: Duration,
        ) -> Result<T, RecvTimeoutError> {
            receiver.recv_timeout(timeout)
        }
    }

    /// The receiving end of one of `drive`'s threads, waited on only through
    /// a [`Clock`].
    pub(super) struct Incoming<T>(Receiver<T>);

    impl<T> Incoming<T> {
        pub(super) fn new(receiver: Receiver<T>) -> Self {
            Self(receiver)
        }

        pub(super) fn recv_timeout(
            &self,
            clock: &impl Clock,
            timeout: Duration,
        ) -> Result<T, RecvTimeoutError> {
            clock.recv_timeout(&self.0, timeout)
        }
    }
}

/// Writes the prompt on its own thread, so a child that writes before it
/// reads cannot block on a full pipe, then closes standard input.
fn send_prompt(
    mut stdin: impl Write + Send + 'static,
    prompt: Vec<u8>,
) -> Receiver<io::Result<()>> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let sent = stdin.write_all(&prompt).and_then(|()| stdin.flush());
        drop(stdin);
        // The receiver is gone when `drive` stopped waiting: nothing to report.
        let _ = sender.send(sent);
    });
    receiver
}

/// A piece of standard output.
enum Frame {
    /// A whole line, with its newline when it had one.
    Line(Vec<u8>),
    /// A piece of a line longer than [`LINE_CAP`].
    Oversized { piece: Vec<u8>, first: bool },
}

/// Reads standard output on its own thread, one line at a time, never
/// holding more than [`LINE_CAP`] bytes of a line.
fn read_frames(stream: impl Read + Send + 'static) -> Receiver<Frame> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        // A read error ends the stream like its end would: the outcome then
        // rests on what was read.
        let _ = pump_frames(BufReader::new(stream), &sender);
    });
    receiver
}

fn pump_frames(mut reader: impl BufRead, sender: &Sender<Frame>) -> io::Result<()> {
    let cap = LINE_CAP as u64;
    let mut in_long_line = false;
    loop {
        let mut piece = Vec::new();
        let read = (&mut reader).take(cap).read_until(b'\n', &mut piece)?;
        if read == 0 {
            return Ok(());
        }
        let ends_line = piece.ends_with(b"\n");
        let frame = if in_long_line || (!ends_line && read as u64 == cap) {
            let first = !in_long_line;
            in_long_line = !ends_line;
            Frame::Oversized { piece, first }
        } else {
            Frame::Line(piece)
        };
        if sender.send(frame).is_err() {
            return Ok(());
        }
    }
}

/// Reads standard error to its end on its own thread, keeping the first
/// [`STDERR_CAP`] bytes and dropping the rest, so the child never blocks on
/// a full pipe.
fn read_capped(mut stream: impl Read + Send + 'static) -> Receiver<Vec<u8>> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut kept = Vec::new();
        let mut buffer = [0u8; 8192];
        loop {
            match stream.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => {
                    let room = STDERR_CAP - kept.len();
                    kept.extend_from_slice(&buffer[..n.min(room)]);
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        let _ = sender.send(kept);
    });
    receiver
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::sync::mpsc::TryRecvError;

    use super::*;

    #[test]
    fn a_line_over_the_cap_comes_in_pieces_and_is_not_parsed() {
        let mut long = vec![b'x'; LINE_CAP + 10];
        long.push(b'\n');
        long.extend_from_slice(b"{\"type\":\"result\"}\n");
        let (sender, receiver) = mpsc::channel();
        pump_frames(&long[..], &sender).unwrap();
        drop(sender);
        let frames: Vec<Frame> = receiver.into_iter().collect();
        let shape: Vec<(usize, &str)> = frames
            .iter()
            .map(|frame| match frame {
                Frame::Line(line) => (line.len(), "line"),
                Frame::Oversized { piece, first: true } => (piece.len(), "first"),
                Frame::Oversized {
                    piece,
                    first: false,
                } => (piece.len(), "rest"),
            })
            .collect();
        assert_eq!(
            shape,
            [(LINE_CAP, "first"), (11, "rest"), (18, "line")],
            "every byte arrives, in order"
        );
    }

    /// A clock that moves only when a wait finds nothing to receive, and then
    /// by the whole wait: a stream held open never delivers, so waiting on it
    /// costs exactly the timeout. Its moments count from zero.
    #[derive(Default)]
    struct StepClock(Cell<Duration>);

    impl Clock for StepClock {
        type Moment = Duration;

        fn now(&self) -> Duration {
            self.0.get()
        }

        fn until(&self, deadline: Duration) -> Duration {
            deadline.saturating_sub(self.0.get())
        }

        fn recv_timeout<T>(
            &self,
            receiver: &Receiver<T>,
            timeout: Duration,
        ) -> Result<T, RecvTimeoutError> {
            match receiver.try_recv() {
                Ok(value) => Ok(value),
                Err(TryRecvError::Disconnected) => Err(RecvTimeoutError::Disconnected),
                Err(TryRecvError::Empty) => {
                    self.0.set(self.0.get() + timeout);
                    Err(RecvTimeoutError::Timeout)
                }
            }
        }
    }

    /// A child that exited with status 0 before `drive` looked.
    struct Exited;

    impl Exiting for Exited {
        fn has_exited(&mut self) -> io::Result<bool> {
            Ok(true)
        }

        fn exit_code(&mut self) -> io::Result<Option<i32>> {
            Ok(Some(0))
        }
    }

    /// Counts the lines it is given.
    #[derive(Default)]
    struct Lines(usize);

    impl Sink for Lines {
        fn line(&mut self, _line: &[u8]) {
            self.0 += 1;
        }

        fn oversized(&mut self) {}
    }

    /// Drives a child that printed a line and exited, leaving behind a
    /// process that holds standard error and standard input open without
    /// reading the prompt, and standard output too unless `stdout_closes`.
    /// Returns what was driven, the lines seen and the time it took on the
    /// step clock.
    fn drive_after_exit(stdout_closes: bool) -> (Driven, usize, Duration) {
        let (stdout, frames) = mpsc::channel();
        stdout.send(Frame::Line(b"{}\n".to_vec())).unwrap();
        let stdout = if stdout_closes {
            drop(stdout);
            None
        } else {
            Some(stdout)
        };
        let (stderr_held, stderr) = mpsc::channel();
        let (stdin_held, prompt_sent) = mpsc::channel();
        let clock = StepClock::default();
        let mut lines = Lines::default();
        let driven = drive_with(
            &clock,
            &mut Exited,
            Incoming::new(prompt_sent),
            Incoming::new(frames),
            Incoming::new(stderr),
            |_| {},
            &mut lines,
        )
        .unwrap();
        drop((stdout, stderr_held, stdin_held));
        (driven, lines.0, clock.now())
    }

    /// The grace period is one in all, counted from the exit: what standard
    /// output took is not given again to standard error or the prompt, and
    /// neither gets one of its own. `drive_does_not_wait_for_a_process_holding_the_output_open`,
    /// in the Claude contract tests, checks the same on a real process, with
    /// a bound loose enough for a loaded machine.
    #[test]
    fn drive_spends_one_grace_period_not_two_when_the_output_stays_held() {
        // Standard output held: the grace period runs out while it is read.
        // Standard output closed at the exit: it runs out on standard error.
        for stdout_closes in [false, true] {
            let (driven, lines, spent) = drive_after_exit(stdout_closes);
            let case = format!("stdout closes: {stdout_closes}, spent {spent:?}");
            assert!(spent >= EXIT_GRACE, "{case}");
            assert!(
                spent < EXIT_GRACE + POLL,
                "{case}: more than one grace period"
            );
            assert!(!driven.prompt_delivered, "{case}");
            assert!(driven.stderr.is_empty(), "{case}");
            assert_eq!(lines, 1, "{case}");
        }
    }
}
