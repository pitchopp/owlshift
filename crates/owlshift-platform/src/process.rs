//! Finding a program on the `PATH`, running it to completion with a deadline,
//! and stopping a whole process tree.
//!
//! [`run`] is for short probes such as `git --version`: a run that has not
//! finished by its deadline is stopped with every process it started, never
//! waited on. [`ProcessTree`] is the mechanism, shared with the executor;
//! [`stop_trees_on_signal`] stops the live trees when the process is told to
//! end.

mod signals;
mod tree;

pub use signals::stop_trees_on_signal;
pub use tree::ProcessTree;

use std::ffi::OsStr;
use std::fmt;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
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
/// The program runs as the root of a [`ProcessTree`]. When the deadline
/// passes, whether the program itself is still running or a process it
/// started still holds its output open, the whole tree is stopped and the
/// output readers are left behind rather than waited on, so the call never
/// outlives `timeout` by more than a poll interval. A run that finishes in
/// time stops nothing: what it leaves running is left alone.
pub fn run(
    program: &Path,
    args: &[&str],
    cwd: Option<&Path>,
    timeout: Duration,
) -> Result<Captured, RunError> {
    run_command(&mut probe_command(program, args, cwd), None, timeout)
}

/// Runs a command the caller prepared (program, arguments, directory,
/// environment) as [`run`] runs a probe: output captured, the whole tree
/// stopped at the deadline. `input`, when given, is written on the command's
/// standard input from a thread of its own, which is then closed; without
/// it, the command gets no input. The standard streams set on `command` are
/// replaced.
pub fn run_command(
    command: &mut Command,
    input: Option<&[u8]>,
    timeout: Duration,
) -> Result<Captured, RunError> {
    let deadline = Instant::now() + timeout;
    command
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let (mut child, tree) = ProcessTree::spawn(command).map_err(RunError::Io)?;
    let stdout = read_capped(child.stdout.take());
    let stderr = read_capped(child.stderr.take());
    if let (Some(mut stdin), Some(input)) = (child.stdin.take(), input) {
        let input = input.to_vec();
        thread::spawn(move || {
            // A program that exits without reading its input closes the
            // pipe; its exit status and output tell the rest.
            let _ = stdin.write_all(&input);
        });
    }

    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                stop(&mut child, &tree);
                return Err(RunError::TimedOut);
            }
            Ok(None) => thread::sleep(POLL),
            Err(error) => {
                stop(&mut child, &tree);
                return Err(RunError::Io(error));
            }
        }
    };
    let receive = |stream: Receiver<Vec<u8>>| {
        stream
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .map_err(|_| RunError::TimedOut)
    };
    let output = receive(stdout).and_then(|stdout| Ok((stdout, receive(stderr)?)));
    match output {
        Ok((stdout, stderr)) => Ok(Captured {
            code: status.code(),
            stdout,
            stderr,
        }),
        Err(error) => {
            // The program is gone, but a process it started holds the output.
            stop(&mut child, &tree);
            Err(error)
        }
    }
}

/// Stops the tree, then reaps its root.
///
/// Best effort: the error the run reports is the timeout or failure that led
/// here. The root is also killed on its own, in case it left its process
/// group; it is waited on only if one of the two succeeded, so a root that
/// could not be stopped never blocks the call.
fn stop(child: &mut Child, tree: &ProcessTree) {
    let tree_stopped = tree.kill().is_ok();
    let root_stopped = child.kill().is_ok();
    if tree_stopped || root_stopped {
        let _ = child.wait();
    }
}

/// The command [`run`] spawns, in the C locale.
fn probe_command(program: &Path, args: &[&str], cwd: Option<&Path>) -> Command {
    let mut command = Command::new(program);
    command.args(args).env("LC_ALL", "C").env("LANGUAGE", "");
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
    fn helper(name: &str, cwd: Option<&Path>, timeout: Duration) -> Result<Captured, RunError> {
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
            cwd,
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

    /// Starts a long-lived grandchild that inherits this process's output,
    /// and writes both pids to `pids` in the working directory.
    fn spawn_grandchild() -> Child {
        let exe = std::env::current_exe().unwrap();
        let grandchild = Command::new(exe)
            .args([
                "--exact",
                "process::tests::helper_sleep",
                "--ignored",
                "--nocapture",
            ])
            .spawn()
            .unwrap();
        let pids = format!("{} {}", std::process::id(), grandchild.id());
        std::fs::write("pids", pids).unwrap();
        grandchild
    }

    #[test]
    #[ignore = "helper, run by the tests below"]
    fn helper_leave_a_pipe_holder() {
        if helper_requested() {
            // The grandchild holds the output open after this process exits.
            drop(spawn_grandchild());
        }
    }

    #[test]
    #[ignore = "helper, run by the tests below"]
    fn helper_wait_on_a_grandchild() {
        if helper_requested() {
            spawn_grandchild().wait().unwrap();
        }
    }

    #[test]
    #[ignore = "helper, run by the tests below"]
    fn helper_sleep() {
        if helper_requested() {
            thread::sleep(Duration::from_secs(20));
        }
    }

    /// Runs a helper that starts a long-lived grandchild, and checks that the
    /// run stops at its deadline and that neither process outlives it.
    fn assert_nothing_outlives_the_deadline(name: &str) {
        let dir = tempfile::tempdir().unwrap();
        let timeout = Duration::from_secs(4);
        let started = Instant::now();
        let outcome = helper(name, Some(dir.path()), timeout);
        assert!(matches!(outcome, Err(RunError::TimedOut)), "{outcome:?}");
        assert!(started.elapsed() < timeout + Duration::from_secs(2));

        let pids = std::fs::read_to_string(dir.path().join("pids"))
            .expect("the helper started its grandchild before the deadline");
        let pids: Vec<u32> = pids.split(' ').map(|pid| pid.parse().unwrap()).collect();
        // Killed processes take a moment to be gone: an orphan is reaped by
        // the system, a Windows process ends asynchronously.
        let settled = Instant::now() + Duration::from_secs(5);
        while pids.iter().any(|&pid| is_alive(pid)) {
            assert!(Instant::now() < settled, "still running: {pids:?}");
            thread::sleep(Duration::from_millis(50));
        }
    }

    /// Whether a process is still running; a zombie is not.
    #[cfg(unix)]
    fn is_alive(pid: u32) -> bool {
        let ps = Command::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .unwrap();
        let state = String::from_utf8_lossy(&ps.stdout);
        let state = state.trim();
        !state.is_empty() && !state.starts_with('Z')
    }

    /// Whether a process is still running.
    #[cfg(windows)]
    fn is_alive(pid: u32) -> bool {
        use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
        use windows_sys::Win32::Foundation::{
            ERROR_INVALID_PARAMETER, WAIT_OBJECT_0, WAIT_TIMEOUT,
        };
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
        };

        // SAFETY: no pointer argument.
        let raw = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
        if raw.is_null() {
            // The only expected failure: no process has this id any more.
            let error = io::Error::last_os_error();
            assert_eq!(
                error.raw_os_error(),
                Some(ERROR_INVALID_PARAMETER as i32),
                "{error}"
            );
            return false;
        }
        // SAFETY: the call succeeded, so `raw` is a new handle owned here.
        let process = unsafe { OwnedHandle::from_raw_handle(raw) };
        // SAFETY: `process` is open for the duration of the call.
        match unsafe { WaitForSingleObject(process.as_raw_handle(), 0) } {
            WAIT_TIMEOUT => true,
            WAIT_OBJECT_0 => false,
            other => panic!("wait returned {other}: {}", io::Error::last_os_error()),
        }
    }

    #[test]
    fn output_is_capped() {
        let captured = helper("helper_flood", None, Duration::from_secs(30)).unwrap();
        assert_eq!(captured.code, Some(0));
        assert_eq!(captured.stdout.len(), OUTPUT_CAP);
    }

    #[test]
    fn a_timed_out_probe_is_stopped_with_the_processes_it_started() {
        assert_nothing_outlives_the_deadline("helper_wait_on_a_grandchild");
    }

    #[test]
    fn a_process_holding_the_output_open_is_stopped_at_the_deadline() {
        assert_nothing_outlives_the_deadline("helper_leave_a_pipe_holder");
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
