//! Finding a program on the `PATH`, running it to completion with a deadline,
//! and stopping a whole process tree.
//!
//! [`run`] is for short probes such as `git --version`: a run that has not
//! finished by its deadline is stopped with every process it started, never
//! waited on. [`run_command`] runs a command the caller prepared the same
//! way, keeping as much of its output as the caller asks for. [`ProcessTree`] is the mechanism, shared with the executor;
//! [`stop_trees_on_signal`] stops the live trees when the process is told to
//! end, and, on Unix, `stop_trees_when_killed` when it is killed outright.

#[cfg(unix)]
mod sentinel;
mod signals;
mod tree;

#[cfg(unix)]
pub use sentinel::{SentinelProbe, SentinelStatus, probe_sentinel};
pub use signals::stop_trees_on_signal;
#[cfg(unix)]
pub use signals::{sentinel_status, stop_trees_when_killed};
pub use tree::ProcessTree;

use std::ffi::OsStr;
use std::fmt;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

/// The most bytes [`run`] keeps from each output stream; the rest is read and
/// dropped.
pub const OUTPUT_CAP: usize = 64 * 1024;

const POLL: Duration = Duration::from_millis(10);

/// What a finished program returned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Captured {
    /// The exit code; `None` when a signal ended the program.
    pub code: Option<i32>,
    /// Standard output, up to the run's output cap.
    pub stdout: Vec<u8>,
    /// Standard error, up to the run's output cap.
    pub stderr: Vec<u8>,
}

impl Captured {
    /// Whether the program exited with status 0.
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }
}

/// Why a program gave no result.
#[derive(Debug)]
pub enum RunError {
    /// It could not be started or watched, or its output could not be read.
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
    run_command(
        &mut probe_command(program, args, cwd),
        None,
        timeout,
        OUTPUT_CAP,
    )
}

/// Runs a command the caller prepared (program, arguments, directory,
/// environment) as [`run`] runs a probe: output captured, the whole tree
/// stopped at the deadline. `input`, when given, is written on the command's
/// standard input from a thread of its own, which is then closed; without
/// it, the command gets no input. The standard streams set on `command` are
/// replaced.
///
/// Each output stream is read to its end and its first `output_cap` bytes
/// are kept: [`OUTPUT_CAP`] for a probe, `usize::MAX` for output that must be
/// read whole, such as a large repository's `git status`.
pub fn run_command(
    command: &mut Command,
    input: Option<&[u8]>,
    timeout: Duration,
    output_cap: usize,
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
    let stdout = read_capped(child.stdout.take(), output_cap);
    let stderr = read_capped(child.stderr.take(), output_cap);
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
    let receive = |stream: Receiver<io::Result<Vec<u8>>>| {
        stream
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .map_err(|_| RunError::TimedOut)?
            .map_err(RunError::Io)
    };
    let output = receive(stdout).and_then(|stdout| Ok((stdout, receive(stderr)?)));
    match output {
        Ok((stdout, stderr)) => Ok(Captured {
            code: status.code(),
            stdout,
            stderr,
        }),
        Err(error) => {
            // The program is gone, but a process it started holds the output,
            // or the output could not be read whole: never a partial result.
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

/// Reads a stream to its end on its own thread, keeping the first `cap`
/// bytes, and sends them once the stream closes, or sends the error that
/// stopped the reading: what was read before it is not a whole output.
fn read_capped(
    stream: Option<impl Read + Send + 'static>,
    cap: usize,
) -> Receiver<io::Result<Vec<u8>>> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut kept = Vec::new();
        let mut outcome = Ok(());
        if let Some(mut stream) = stream {
            let mut buffer = [0u8; 8192];
            loop {
                match stream.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(n) => {
                        let room = cap - kept.len();
                        kept.extend_from_slice(&buffer[..n.min(room)]);
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    Err(error) => {
                        outcome = Err(error);
                        break;
                    }
                }
            }
        }
        // The receiver is gone when the run timed out: nothing to report.
        let _ = sender.send(outcome.map(|()| kept));
    });
    receiver
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs one of the helper tests below in a fresh copy of this test binary,
    /// as a probe.
    fn helper(name: &str, cwd: Option<&Path>, timeout: Duration) -> Result<Captured, RunError> {
        helper_capped(name, cwd, timeout, OUTPUT_CAP)
    }

    /// [`helper`], keeping `cap` bytes of each output stream.
    fn helper_capped(
        name: &str,
        cwd: Option<&Path>,
        timeout: Duration,
        cap: usize,
    ) -> Result<Captured, RunError> {
        let exe = std::env::current_exe().unwrap();
        let test = format!("process::tests::{name}");
        let args = [
            "--exact",
            &test,
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ];
        run_command(&mut probe_command(&exe, &args, cwd), None, timeout, cap)
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

    /// Runs a helper that starts a long-lived grandchild, as a probe, and
    /// checks that the run stops at its deadline and that neither the helper
    /// nor its grandchild outlives it.
    fn assert_nothing_outlives_the_deadline(name: &str) {
        let dir = tempfile::tempdir().unwrap();
        let exe = std::env::current_exe().unwrap();
        let test = format!("process::tests::{name}");
        let args = [
            "--exact",
            &test,
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ];
        let timeout = Duration::from_secs(4);
        let started = Instant::now();
        let outcome = run_command(
            &mut probe_command(&exe, &args, Some(dir.path())),
            None,
            timeout,
            OUTPUT_CAP,
        );
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
        let state = state(pid);
        !state.is_empty() && !state.starts_with('Z')
    }

    /// A process's state as `ps` shows it; empty once it is gone.
    #[cfg(unix)]
    fn state(pid: u32) -> String {
        let ps = Command::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .unwrap();
        String::from_utf8_lossy(&ps.stdout).trim().to_owned()
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

    /// A stream that gives some bytes, then fails.
    struct Failing(bool);

    impl Read for Failing {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            if std::mem::replace(&mut self.0, true) {
                Err(io::Error::other("broken"))
            } else {
                buffer[..4].copy_from_slice(b"part");
                Ok(4)
            }
        }
    }

    #[test]
    fn a_read_error_is_reported_not_a_partial_output() {
        let read = read_capped(Some(Failing(false)), usize::MAX);
        let outcome = read.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(outcome.unwrap_err().to_string(), "broken");
    }

    #[test]
    fn output_is_read_whole_without_a_cap() {
        let timeout = Duration::from_secs(30);
        let captured = helper_capped("helper_flood", None, timeout, usize::MAX).unwrap();
        assert!(captured.success());
        // The whole flood, among the test harness's own lines.
        let flood = captured.stdout.iter().filter(|&&byte| byte == b'x').count();
        assert_eq!(flood, 1024 * 1024);
    }

    #[test]
    fn a_timed_out_probe_is_stopped_with_the_processes_it_started() {
        assert_nothing_outlives_the_deadline("helper_wait_on_a_grandchild");
    }

    #[test]
    fn a_process_holding_the_output_open_is_stopped_at_the_deadline() {
        assert_nothing_outlives_the_deadline("helper_leave_a_pipe_holder");
    }

    /// Spawns `sleep 30` as the root of a tree.
    #[cfg(unix)]
    fn sleeper() -> (Child, ProcessTree) {
        ProcessTree::spawn(Command::new("sleep").arg("30")).unwrap()
    }

    /// Waits until `done` holds, 5 s at most.
    #[cfg(unix)]
    fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !done() {
            assert!(
                Instant::now() < deadline,
                "still waiting for {what} after 5 s"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// Writes `pids`, space-separated, to `pids` in the working directory,
    /// whole or not at all.
    #[cfg(unix)]
    fn write_pids(pids: &[u32]) {
        let pids: Vec<String> = pids.iter().map(u32::to_string).collect();
        std::fs::write("pids.tmp", pids.join(" ")).unwrap();
        std::fs::rename("pids.tmp", "pids").unwrap();
    }

    /// What a helper that owns trees does with its sentinel before it is
    /// killed.
    #[cfg(unix)]
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Scenario {
        /// Leaves it running.
        Running,
        /// Stops it (OWL-93).
        Stopped,
        /// Stops it, then sends it more lines than its pipe holds, and only
        /// then drops the tree to spare, so that the line telling so is
        /// lost (OWL-93).
        Filled,
    }

    /// Owns a live tree, spawned before the sentinel starts, which learns of
    /// it then, and a tree whose handle it drops; writes the pids of both
    /// roots and of the sentinel, then sleeps until it is killed.
    #[cfg(unix)]
    fn own_trees_then_sleep(scenario: Scenario) {
        use rustix::process::{Pid, Signal, kill_process};

        let (live, _live) = sleeper();
        stop_trees_when_killed().unwrap();
        signals::with_sentinel(|sentinel| sentinel.wait_until_it_ignores_hangups()).unwrap();
        let (dropped, tree) = sleeper();
        let mut tree = Some(tree);
        if scenario != Scenario::Filled {
            drop(tree.take());
        }
        let status = sentinel_status();
        let SentinelStatus::Running { pid: sentinel } = status else {
            panic!("the sentinel does not run: {status:?}");
        };
        if scenario != Scenario::Running {
            let pid = Pid::from_raw(i32::try_from(sentinel).unwrap()).unwrap();
            kill_process(pid, Signal::STOP).unwrap();
            wait_until("the sentinel to stop", || {
                matches!(sentinel_status(), SentinelStatus::Stopped { .. })
            });
        }
        if scenario == Scenario::Filled {
            // Pid 1 leads no tree, so each line changes nothing; 256 KiB of
            // them, more than a pipe holds.
            let none = Pid::from_raw(1).unwrap();
            signals::with_sentinel(|sentinel| {
                for _ in 0..64 * 1024 {
                    sentinel.withdraw(none);
                }
            });
            drop(tree.take());
        }
        write_pids(&[live.id(), dropped.id(), sentinel]);
        thread::sleep(Duration::from_secs(20));
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "helper, run by the tests below"]
    fn helper_own_trees_then_sleep() {
        if helper_requested() {
            own_trees_then_sleep(Scenario::Running);
        }
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "helper, run by the tests below"]
    fn helper_own_trees_with_the_sentinel_stopped_then_sleep() {
        if helper_requested() {
            own_trees_then_sleep(Scenario::Stopped);
        }
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "helper, run by the tests below"]
    fn helper_own_trees_with_the_sentinel_input_filled_then_sleep() {
        if helper_requested() {
            own_trees_then_sleep(Scenario::Filled);
        }
    }

    /// The name of the shell `helper_own_a_stopped_shell_then_sleep` starts.
    #[cfg(unix)]
    const ORPHAN: &str = "owlshift-test-orphan";

    /// Starts a shell in a process group of its own that writes `hup` to a
    /// file when it gets SIGHUP, and reads its input, a pipe this helper
    /// holds; stops it once it traps SIGHUP, writes its pid, then sleeps
    /// until it is killed.
    #[cfg(unix)]
    #[test]
    #[ignore = "helper, run by the tests below"]
    fn helper_own_a_stopped_shell_then_sleep() {
        if helper_requested() {
            use rustix::process::{Pid, Signal, WaitId, WaitIdOptions, kill_process, waitid};
            use std::os::unix::process::CommandExt;

            let mut shell = Command::new("/bin/sh")
                .args([
                    "-c",
                    "trap 'echo hup > hup' HUP; echo > ready; read -r line; :",
                    ORPHAN,
                ])
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .process_group(0)
                .spawn()
                .unwrap();
            wait_until("the shell to trap SIGHUP", || Path::new("ready").exists());
            let pid = Pid::from_child(&shell);
            kill_process(pid, Signal::STOP).unwrap();
            let stopped = WaitIdOptions::STOPPED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT;
            wait_until(
                "the shell to stop",
                || matches!(waitid(WaitId::Pid(pid), stopped), Ok(Some(status)) if status.stopped()),
            );
            write_pids(&[shell.id()]);
            thread::sleep(Duration::from_secs(20));
            // Not reached when the test kills this helper, as it does.
            let _ = shell.kill();
            let _ = shell.wait();
        }
    }

    /// A helper of this test binary, run in a process of its own and in a
    /// directory of its own, where it writes the pids of what it owns.
    #[cfg(unix)]
    struct Owner {
        process: Child,
        dir: tempfile::TempDir,
        pids: Vec<u32>,
    }

    #[cfg(unix)]
    impl Owner {
        /// Starts the helper `name` and waits for its pids.
        fn start(name: &str) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let exe = std::env::current_exe().unwrap();
            let test = format!("process::tests::{name}");
            let process = Command::new(exe)
                .args([
                    "--exact",
                    &test,
                    "--ignored",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .current_dir(dir.path())
                .stdout(Stdio::null())
                .spawn()
                .unwrap();
            let mut owner = Self {
                process,
                dir,
                pids: Vec::new(),
            };
            let pids = owner.dir.path().join("pids");
            let started = Instant::now() + Duration::from_secs(10);
            while !pids.exists() {
                if let Some(status) = owner.process.try_wait().unwrap() {
                    panic!("{name} ended before it wrote its pids: {status}");
                }
                assert!(Instant::now() < started, "{name} never wrote its pids");
                thread::sleep(Duration::from_millis(20));
            }
            owner.pids = std::fs::read_to_string(pids)
                .unwrap()
                .split(' ')
                .map(|pid| pid.parse().unwrap())
                .collect();
            owner
        }

        /// Kills it outright, with SIGKILL: no handler runs.
        fn kill(&mut self) {
            self.process.kill().unwrap();
            self.process.wait().unwrap();
        }
    }

    #[cfg(unix)]
    impl Drop for Owner {
        fn drop(&mut self) {
            // Once reaped, both do nothing.
            let _ = self.process.kill();
            let _ = self.process.wait();
        }
    }

    /// Kills, on the way out, what a test leaves behind: the trees, `sleep`
    /// processes, and a process left stopped when its parent was killed, each
    /// only while `ps` still shows it as such, so that its pid is still its
    /// own. `KillIfStopped` in the CLI's tests does as much for `owlshift`'s
    /// sentinel, in a crate of its own.
    #[cfg(unix)]
    struct Left {
        trees: Vec<u32>,
        /// The pid and the name of the process that may be left stopped.
        stopped: (u32, &'static str),
    }

    #[cfg(unix)]
    impl Drop for Left {
        fn drop(&mut self) {
            // Nothing here may panic: it also runs while a failed test
            // unwinds.
            let shown = |pid: u32| {
                Command::new("ps")
                    .args(["-ww", "-o", "stat=,command=", "-p", &pid.to_string()])
                    .output()
                    .map(|ps| String::from_utf8_lossy(&ps.stdout).trim().to_owned())
                    .unwrap_or_default()
            };
            let kill = |pid: u32| {
                let _ = Command::new("kill")
                    .args(["-s", "KILL", &pid.to_string()])
                    .status();
            };
            for &tree in &self.trees {
                let shown = shown(tree);
                if !shown.starts_with('Z') && shown.ends_with("sleep 30") {
                    kill(tree);
                }
            }
            let (pid, name) = self.stopped;
            let shown = shown(pid);
            if shown.starts_with('T') && shown.contains(name) {
                kill(pid);
            }
        }
    }

    /// Kills the owner of the trees outright, then checks that its live tree
    /// is gone within 5 s and that the tree whose handle it dropped still
    /// runs.
    #[cfg(unix)]
    fn assert_a_hard_kill_stops_only_the_live_tree(helper: &str) {
        let mut owner = Owner::start(helper);
        let &[live, dropped, sentinel] = owner.pids.as_slice() else {
            panic!("pids: {:?}", owner.pids);
        };
        let _left = Left {
            trees: vec![live, dropped],
            stopped: (sentinel, "owlshift-sentinel"),
        };
        owner.kill();
        wait_until("the live tree to be stopped", || !is_alive(live));
        // The sentinel stops every tree it knows of at once: give a wrong
        // stop time to land before checking it did not happen.
        thread::sleep(Duration::from_millis(300));
        assert!(is_alive(dropped), "the dropped tree was stopped");
    }

    /// OWL-86: once the sentinel runs, killing the process that owns the
    /// trees outright stops every live tree, one spawned before the sentinel
    /// started included, and leaves a tree whose handle was dropped running.
    #[cfg(unix)]
    #[test]
    fn a_hard_kill_stops_the_live_trees_and_leaves_a_dropped_one() {
        assert_a_hard_kill_stops_only_the_live_tree("helper_own_trees_then_sleep");
    }

    /// OWL-93: a sentinel stopped when the process owning the trees is killed
    /// outright still stops the live trees, and only them: that process's end
    /// orphans the sentinel's group, so the system sends it SIGHUP, which it
    /// ignores, then SIGCONT.
    #[cfg(unix)]
    #[test]
    fn a_hard_kill_with_the_sentinel_stopped_still_stops_the_live_trees() {
        assert_a_hard_kill_stops_only_the_live_tree(
            "helper_own_trees_with_the_sentinel_stopped_then_sleep",
        );
    }

    /// OWL-93: a stopped sentinel that lost a line, the one that spares a
    /// tree whose handle was dropped, never stops that tree, even once a hard
    /// kill of the process owning the trees has the system continue it.
    #[cfg(unix)]
    #[test]
    fn a_sentinel_that_lost_a_line_never_stops_a_dropped_tree() {
        let mut owner = Owner::start("helper_own_trees_with_the_sentinel_input_filled_then_sleep");
        let &[live, dropped, sentinel] = owner.pids.as_slice() else {
            panic!("pids: {:?}", owner.pids);
        };
        let _left = Left {
            trees: vec![live, dropped],
            stopped: (sentinel, "owlshift-sentinel"),
        };
        owner.kill();
        // Whatever the sentinel does, it has done once it is gone.
        wait_until("the sentinel to be gone", || !is_alive(sentinel));
        thread::sleep(Duration::from_millis(300));
        assert!(is_alive(dropped), "the dropped tree was stopped");
    }

    /// OWL-93, what the sentinel relies on: a stopped process whose group the
    /// end of its parent orphans gets SIGHUP, then SIGCONT, as POSIX requires
    /// of such an exit. Run on Linux and macOS; where a subreaper of the same
    /// session adopts the process, its group is not orphaned and this fails.
    #[cfg(unix)]
    #[test]
    fn a_stopped_group_is_hung_up_then_continued_when_its_parent_ends() {
        let mut owner = Owner::start("helper_own_a_stopped_shell_then_sleep");
        let shell = owner.pids[0];
        let _left = Left {
            trees: Vec::new(),
            stopped: (shell, ORPHAN),
        };
        let hup = owner.dir.path().join("hup");
        // While its parent lives, its group is not orphaned: nothing
        // continues it, nothing hangs it up.
        assert!(state(shell).starts_with('T'), "{}", state(shell));
        assert!(!hup.exists());
        owner.kill();
        // Continued, it runs its trap, then reads the end of its input.
        wait_until("the shell to be hung up and gone", || {
            hup.exists() && !is_alive(shell)
        });
        assert_eq!(std::fs::read_to_string(&hup).unwrap(), "hup\n");
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
