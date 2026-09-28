//! Finding a program on the `PATH` and running it to completion with a
//! deadline.
//!
//! This is for short probes such as `git --version`: a run that has not
//! finished by its deadline is abandoned, never waited on. Stopping a whole
//! process tree belongs to the executor.

use std::ffi::OsStr;
use std::fmt;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

/// The most bytes kept from each output stream; the rest is read and dropped.
pub const OUTPUT_CAP: usize = 64 * 1024;

const POLL: Duration = Duration::from_millis(10);

/// What a finished program returned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Captured {
    /// The exit code; `None` when a signal ended the program.
    pub code: Option<i32>,
    /// Standard output, up to [`OUTPUT_CAP`] bytes.
    pub stdout: Vec<u8>,
    /// Standard error, up to [`OUTPUT_CAP`] bytes.
    pub stderr: Vec<u8>,
}

/// Why a program gave no result.
#[derive(Debug)]
pub enum RunError {
    /// It could not be started or watched.
    Io(io::Error),
    /// It, or a process holding its output open, outlived the deadline.
    TimedOut,
}

impl fmt::Display for RunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "could not run it: {error}"),
            Self::TimedOut => f.write_str("it did not answer in time"),
        }
    }
}

impl std::error::Error for RunError {}

/// Finds a program on the `PATH`, with the `PATHEXT` extensions on Windows,
/// so an npm `.cmd` shim is found too.
pub fn find_executable(name: &str) -> Option<PathBuf> {
    which::which(name).ok()
}

/// Finds a program in the given search path, a `PATH`-style list.
pub fn find_executable_in(name: &str, search_path: impl AsRef<OsStr>) -> Option<PathBuf> {
    let cwd = std::env::current_dir().ok()?;
    which::which_in(name, Some(search_path), cwd).ok()
}

/// Runs a program with no input and captures its output, giving up at the
/// deadline.
///
/// The program runs in the C locale (`LC_ALL=C`, `LANGUAGE` empty), so its
/// messages are the English ones the callers match on, such as git's "not a
/// git repository" or the harness status phrases of live check C8, whatever
/// the user's own locale.
///
/// At the deadline the program is killed. A process it started may still
/// hold its output open; the readers are then left behind rather than waited
/// on, so the call never outlives `timeout` by more than a poll interval.
pub fn run(
    program: &Path,
    args: &[&str],
    cwd: Option<&Path>,
    timeout: Duration,
) -> Result<Captured, RunError> {
    let deadline = Instant::now() + timeout;
    let mut child = probe_command(program, args, cwd)
        .spawn()
        .map_err(RunError::Io)?;
    let stdout = read_capped(child.stdout.take());
    let stderr = read_capped(child.stderr.take());

    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                // Best effort: the child may have exited in between.
                let _ = child.kill();
                let _ = child.wait();
                return Err(RunError::TimedOut);
            }
            Ok(None) => thread::sleep(POLL),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(RunError::Io(error));
            }
        }
    };
    let receive = |stream: Receiver<Vec<u8>>| {
        stream
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .map_err(|_| RunError::TimedOut)
    };
    Ok(Captured {
        code: status.code(),
        stdout: receive(stdout)?,
        stderr: receive(stderr)?,
    })
}

/// The command [`run`] spawns: no input, captured output, the C locale.
fn probe_command(program: &Path, args: &[&str], cwd: Option<&Path>) -> Command {
    let mut command = Command::new(program);
    command
        .args(args)
        .env("LC_ALL", "C")
        .env("LANGUAGE", "")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(dir) = cwd {
        command.current_dir(dir);
    }
    command
}

/// Reads a stream to its end on its own thread, keeping the first
/// [`OUTPUT_CAP`] bytes, and sends them once the stream closes.
fn read_capped(stream: Option<impl Read + Send + 'static>) -> Receiver<Vec<u8>> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut kept = Vec::new();
        if let Some(mut stream) = stream {
            let mut buffer = [0u8; 8192];
            loop {
                match stream.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(n) => {
                        let room = OUTPUT_CAP - kept.len();
                        kept.extend_from_slice(&buffer[..n.min(room)]);
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
        }
        // The receiver is gone when the run timed out: nothing to report.
        let _ = sender.send(kept);
    });
    receiver
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs one of the helper tests below in a fresh copy of this test binary.
    fn helper(name: &str, timeout: Duration) -> Result<Captured, RunError> {
        let exe = std::env::current_exe().unwrap();
        let test = format!("process::tests::{name}");
        run(
            &exe,
            &[
                "--exact",
                &test,
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ],
            None,
            timeout,
        )
    }

    /// The helpers act only when run alone by [`helper`], never in a plain
    /// `cargo test -- --ignored`.
    fn helper_requested() -> bool {
        std::env::args().any(|arg| arg == "--exact")
    }

    #[test]
    #[ignore = "helper, run by the tests below"]
    fn helper_flood() {
        if helper_requested() {
            use std::io::Write;
            let chunk = vec![b'x'; 1024 * 1024];
            std::io::stdout().write_all(&chunk).unwrap();
        }
    }

    #[test]
    #[ignore = "helper, run by the tests below"]
    #[expect(
        clippy::zombie_processes,
        reason = "the grandchild must outlive this process"
    )]
    fn helper_leave_a_pipe_holder() {
        if helper_requested() {
            // A grandchild inherits this process's output and outlives it.
            let exe = std::env::current_exe().unwrap();
            Command::new(exe)
                .args([
                    "--exact",
                    "process::tests::helper_sleep",
                    "--ignored",
                    "--nocapture",
                ])
                .spawn()
                .unwrap();
        }
    }

    #[test]
    #[ignore = "helper, run by the tests below"]
    fn helper_sleep() {
        if helper_requested() {
            thread::sleep(Duration::from_secs(5));
        }
    }

    #[test]
    fn output_is_capped() {
        let captured = helper("helper_flood", Duration::from_secs(30)).unwrap();
        assert_eq!(captured.code, Some(0));
        assert_eq!(captured.stdout.len(), OUTPUT_CAP);
    }

    #[test]
    fn a_process_holding_the_output_open_cannot_outlive_the_deadline() {
        let started = Instant::now();
        let outcome = helper("helper_leave_a_pipe_holder", Duration::from_secs(2));
        assert!(matches!(outcome, Err(RunError::TimedOut)), "{outcome:?}");
        assert!(started.elapsed() < Duration::from_secs(4));
    }

    #[test]
    fn probes_run_in_the_c_locale() {
        let command = probe_command(Path::new("git"), &["--version"], None);
        let envs: Vec<_> = command.get_envs().collect();
        assert!(envs.contains(&(OsStr::new("LC_ALL"), Some(OsStr::new("C")))));
        assert!(envs.contains(&(OsStr::new("LANGUAGE"), Some(OsStr::new("")))));
    }

    #[test]
    fn a_missing_program_is_an_io_error() {
        let missing = Path::new("owlshift-no-such-program-4f1c");
        let outcome = run(missing, &[], None, Duration::from_secs(5));
        assert!(matches!(outcome, Err(RunError::Io(_))), "{outcome:?}");
        assert_eq!(find_executable("owlshift-no-such-program-4f1c"), None);
    }

    /// An npm-installed CLI on Windows is a `.cmd` shim; it must be found and
    /// run with its arguments.
    #[cfg(windows)]
    #[test]
    fn a_cmd_shim_is_found_and_run() {
        let dir = tempfile::Builder::new()
            .prefix("owlshift shim ")
            .tempdir()
            .unwrap();
        std::fs::write(
            dir.path().join("fakecli.cmd"),
            "@echo off\r\necho args: %1 %2\r\necho oops 1>&2\r\nexit /b 3\r\n",
        )
        .unwrap();
        let program = find_executable_in("fakecli", dir.path()).expect("shim found");
        let captured = run(&program, &["auth", "status"], None, Duration::from_secs(20)).unwrap();
        assert_eq!(captured.code, Some(3));
        assert_eq!(
            String::from_utf8_lossy(&captured.stdout).trim(),
            "args: auth status"
        );
        assert_eq!(String::from_utf8_lossy(&captured.stderr).trim(), "oops");
    }
}
