//! The project's gate, run by the runner itself after a Build run reports
//! `done` (OWL-16): the role's own claim that the gate is green is never
//! taken as proof.
//!
//! [`run`] runs the brief's `gate` commands in the worktree, in order, each
//! through the platform's shell (`sh -c` on Unix, `cmd /d /s /c` on
//! Windows), with the agent environment and no input, and stops at the first
//! failure. Around them it requires a clean worktree before and after, and
//! the same HEAD: a passing gate vouches for the commit the run delivers,
//! nothing else. Those checks run git as the agent too, with the file-system
//! monitor hook off, so nothing the run planted in the repository's
//! configuration runs with the runner's environment.
//!
//! That git runs outside the sandbox, so before it runs, before the commands
//! and again after them, the caller's guard checks the worktree's `.git` link
//! and the repository's shared git files on the file system (OWL-64; the
//! checks of [`super::isolation`], OWL-59): a clean filter a gate command
//! planted would otherwise run in the gate's `git status`. A guard that finds
//! a violation stops the gate there, before any more git, and the report
//! carries it as a [`GateReport::breach`], which quarantines the run. The
//! status check ignores submodules, so a gitlink planted in the index does
//! not start git in a folder whose configuration nothing checked.
//!
//! Every command's output, standard output and standard error interleaved,
//! goes whole to the gate log in the caller's run directory; a failure keeps
//! the last [`OUTPUT_TAIL`] bytes for the next Build run's brief. The gate
//! commands share one deadline: each gets what is left of it, none starts
//! once it is spent, and a command, or a process it started, still running
//! at the deadline is stopped with its whole tree.

use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use owlshift_contracts::brief::GateFailure;

use super::git::Git;
use super::isolation::Violation;
use super::watch;
use crate::agent_env::{AgentEnv, RunPaths};

/// A gate deadline for callers without a reason to pick another.
pub const DEFAULT_GATE_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// The file of the caller's run directory the gate's output goes to.
pub const GATE_LOG: &str = "gate.log";

/// The most bytes of output a failure carries, from its end.
pub const OUTPUT_TAIL: usize = 16 * 1024;

/// How long the output may take to close once a command's tree is stopped:
/// a process that left the tree could hold it open for ever.
const OUTPUT_GRACE: Duration = Duration::from_secs(2);

/// The names of uncommitted paths a failure lists before "and N more".
const LISTED_PATHS: usize = 10;

/// What the runner's run of the gate came to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GateReport {
    /// The commands, as in the brief's `gate`.
    pub commands: Vec<String>,
    /// The commit the gate ran on, when it could be read. A passing gate
    /// ran every command on it and left it as it was.
    pub commit: Option<String>,
    /// The gate log.
    pub log: PathBuf,
    /// Why the gate failed; `None` when it passed. A breach is a failure
    /// too, so that no reader of this field takes a breached gate for a
    /// passing one.
    pub failure: Option<GateFailure>,
    /// What the guard found before the gate's own git: the run broke
    /// isolation and is quarantined, whatever the failure says. Empty when
    /// the guard found nothing.
    pub breach: Vec<Violation>,
}

impl GateReport {
    pub fn passed(&self) -> bool {
        self.failure.is_none() && self.breach.is_empty()
    }
}

/// The checks the gate makes before its own git runs, on the file system
/// alone: every isolation violation they find, empty when none.
pub(crate) type Guard<'a> = &'a dyn Fn() -> Vec<Violation>;

/// Why the gate stopped short of passing.
enum Stop {
    Failed(GateFailure),
    Breach(Vec<Violation>),
}

impl From<GateFailure> for Stop {
    fn from(failure: GateFailure) -> Self {
        Self::Failed(failure)
    }
}

/// Runs the gate in the worktree of `paths`, each command confined to them
/// as the run was (OWL-41), with `guard` checked before each of the gate's
/// own git steps; see the module documentation. Whatever goes wrong, the
/// runner's side included, is a failure: a gate that could not be run never
/// passes. A violation the guard finds is a breach, and a failure as well.
pub(crate) fn run(
    git: &Git,
    agent: &AgentEnv,
    paths: &RunPaths,
    commands: &[String],
    timeout: Duration,
    log_path: &Path,
    guard: Guard<'_>,
) -> GateReport {
    let deadline = Instant::now() + timeout;
    let git = git.as_agent(agent);
    let mut report = GateReport {
        commands: commands.to_vec(),
        commit: None,
        log: log_path.to_owned(),
        failure: None,
        breach: Vec::new(),
    };
    let checked = check_and_run(
        &git,
        agent,
        paths,
        commands,
        (timeout, deadline),
        guard,
        &mut report,
    );
    match checked {
        Ok(()) => {}
        Err(Stop::Failed(failure)) => report.failure = Some(failure),
        Err(Stop::Breach(violations)) => {
            let reasons: Vec<String> = violations.iter().map(ToString::to_string).collect();
            report.failure = Some(around(format!(
                "the gate broke isolation, found before its own git ran: {}",
                reasons.join("; ")
            )));
            report.breach = violations;
        }
    }
    report
}

fn check_and_run(
    git: &Git,
    agent: &AgentEnv,
    paths: &RunPaths,
    commands: &[String],
    (timeout, deadline): (Duration, Instant),
    guard: Guard<'_>,
    report: &mut GateReport,
) -> Result<(), Stop> {
    let worktree = paths.workdir.as_path();
    let mut log = File::create(&report.log).map_err(|e| {
        around(format!(
            "the runner could not write the gate log {}: {e}",
            report.log.display()
        ))
    })?;
    // The caller checked isolation just before, with nothing run since; the
    // gate checks anyway, so that its own git never rests on the caller.
    guarded(guard, &mut log)?;
    let before = head(git, worktree)?;
    report.commit = Some(before.clone());
    let dirty = uncommitted(git, worktree)?;
    if !dirty.is_empty() {
        return Err(around(format!(
            "uncommitted changes before the gate, which runs on the last commit: {dirty}"
        ))
        .into());
    }
    run_commands(agent, paths, commands, timeout, deadline, &mut log)?;
    // The commands wrote what the sandbox let them, the repository's git
    // folder included: nothing they planted may run in the gate's git.
    guarded(guard, &mut log)?;
    let dirty = uncommitted(git, worktree)?;
    if !dirty.is_empty() {
        return Err(around(format!("the gate changed files: {dirty}")).into());
    }
    let after = head(git, worktree)?;
    if after != before {
        return Err(around(format!("the gate moved HEAD from {before} to {after}")).into());
    }
    Ok(())
}

/// Runs the guard; a violation stops the gate, with a line in its log.
fn guarded(guard: Guard<'_>, log: &mut File) -> Result<(), Stop> {
    let violations = guard();
    if violations.is_empty() {
        return Ok(());
    }
    for violation in &violations {
        let _ = writeln!(log, "[isolation broken, no more git runs: {violation}]");
    }
    Err(Stop::Breach(violations))
}

/// A failure around the commands, with no command or output of its own.
fn around(reason: String) -> GateFailure {
    GateFailure {
        command: None,
        reason,
        output: String::new(),
        truncated: false,
    }
}

/// The worktree's HEAD commit.
fn head(git: &Git, worktree: &Path) -> Result<String, GateFailure> {
    let out = git
        .run(
            worktree,
            &agent_git_args(&["rev-parse", "--verify", "HEAD"]),
        )
        .map_err(|e| around(format!("the runner could not read HEAD: {e}")))?;
    Ok(String::from_utf8_lossy(&out).trim().to_owned())
}

/// The worktree's uncommitted paths, tracked or not, as a short list; empty
/// when it is clean. Ignored files do not count, and neither do submodules:
/// git would look into one with its own git, reading configuration the
/// guard does not check.
fn uncommitted(git: &Git, worktree: &Path) -> Result<String, GateFailure> {
    let out = git
        .run(
            worktree,
            &agent_git_args(&[
                "status",
                "--porcelain",
                "-z",
                "--untracked-files=all",
                "--no-renames",
                "--ignore-submodules=all",
            ]),
        )
        .map_err(|e| {
            around(format!(
                "the runner could not read the worktree's status: {e}"
            ))
        })?;
    // Each entry is `XY path`, NUL-terminated; with no renames, no entry
    // carries a second path.
    let paths: Vec<String> = out
        .split(|&b| b == 0)
        .filter(|entry| entry.len() > 3)
        .map(|entry| String::from_utf8_lossy(&entry[3..]).into_owned())
        .collect();
    let mut listed = paths
        .iter()
        .take(LISTED_PATHS)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    if paths.len() > LISTED_PATHS {
        listed.push_str(&format!(" and {} more", paths.len() - LISTED_PATHS));
    }
    Ok(listed)
}

/// The arguments of a git command run as the agent: no optional lock, and
/// no file-system monitor hook, a command the repository's configuration
/// could name.
fn agent_git_args<'a>(args: &[&'a str]) -> Vec<&'a str> {
    let mut all = vec!["-c", "core.fsmonitor=false", "--no-optional-locks"];
    all.extend_from_slice(args);
    all
}

/// Runs the commands in order until the first failure, sharing one deadline.
fn run_commands(
    agent: &AgentEnv,
    paths: &RunPaths,
    commands: &[String],
    timeout: Duration,
    deadline: Instant,
    log: &mut File,
) -> Result<(), GateFailure> {
    for command in commands {
        let failed = |reason: String, tail: Tail| {
            let (output, truncated) = tail.text();
            GateFailure {
                command: Some(command.clone()),
                reason,
                output,
                truncated,
            }
        };
        let stopped = || format!("stopped at the gate's deadline, {} s", timeout.as_secs());
        if Instant::now() >= deadline {
            return Err(failed(stopped(), Tail::default()));
        }
        if let Err(e) = writeln!(log, "$ {command}") {
            return Err(failed(
                format!("the runner could not write the gate log: {e}"),
                Tail::default(),
            ));
        }
        let ran = run_one(agent, paths, command, deadline, log);
        let status = match &ran.end {
            End::Exited(status) => status.to_string(),
            End::TimedOut => "stopped at the deadline".to_owned(),
            End::Error(error) => format!("runner error: {error}"),
        };
        let _ = writeln!(log, "[{status}]");
        let reason = match ran.end {
            End::TimedOut => Some(stopped()),
            End::Error(error) => Some(error),
            End::Exited(status) if !status.success() => Some(match status.code() {
                Some(code) => format!("exit status {code}"),
                None => "ended by a signal".to_owned(),
            }),
            End::Exited(_) if ran.output_held => {
                Some("a process it started kept its output open after it ended".to_owned())
            }
            End::Exited(_) => ran
                .log_error
                .map(|e| format!("the runner could not write the gate log: {e}")),
        };
        if let Some(reason) = reason {
            return Err(failed(reason, ran.tail));
        }
    }
    Ok(())
}

/// How one command ended.
enum End {
    Exited(ExitStatus),
    TimedOut,
    /// The runner could not start or watch it.
    Error(String),
}

struct Ran {
    end: End,
    /// The tail of its output, as far as it was read: an abandoned output
    /// keeps what was read before it was.
    tail: Tail,
    /// Its output was still open after the grace period: abandoned.
    output_held: bool,
    /// The first error writing the log.
    log_error: Option<String>,
}

/// Runs one command as the root of a process tree, inside the sandbox of
/// `paths`, its standard output and standard error on one pipe, copied whole
/// to `log`. A command that cannot be confined does not run.
fn run_one(agent: &AgentEnv, paths: &RunPaths, line: &str, deadline: Instant, log: &File) -> Ran {
    let error = |message: String| Ran {
        end: End::Error(message),
        tail: Tail::default(),
        output_held: false,
        log_error: None,
    };
    let (reader, writer) = match io::pipe() {
        Ok(pipe) => pipe,
        Err(e) => return error(format!("could not create a pipe: {e}")),
    };
    let (log, stdout) = match (log.try_clone(), writer.try_clone()) {
        (Ok(log), Ok(stdout)) => (log, stdout),
        (Err(e), _) | (_, Err(e)) => return error(format!("could not share the output: {e}")),
    };
    let mut command = match agent.confine(shell(line, agent), paths) {
        Ok(command) => command,
        Err(e) => return error(format!("could not be confined: {e}")),
    };
    command.stdin(Stdio::null()).stdout(stdout).stderr(writer);
    let remaining = deadline.saturating_duration_since(Instant::now());
    let spawned = watch::spawn(&mut command, remaining);
    // The command holds the pipe's write ends: dropping it leaves them to
    // the child alone, so the output closes when the tree is gone.
    drop(command);
    let (mut child, tree, watchdog) = match spawned {
        Ok(spawned) => spawned,
        Err(e) => return error(format!("could not start: {e}")),
    };

    let tail = Arc::new(Mutex::new(Tail::default()));
    let (sender, receiver) = mpsc::channel();
    {
        let tail = Arc::clone(&tail);
        thread::spawn(move || {
            // The receiver is gone when the output was abandoned.
            let _ = sender.send(drain(reader, log, &tail));
        });
    }
    let waited = child.wait();
    let timed_out = watchdog.finish();
    // What the command left running would hold the output open; the tree
    // stops it. Best effort, as for the harness.
    let _ = tree.kill();
    let wait_for = deadline
        .saturating_duration_since(Instant::now())
        .max(OUTPUT_GRACE);
    let (log_error, output_held) = match receiver.recv_timeout(wait_for) {
        Ok(log_error) => (log_error, false),
        Err(_) => (None, true),
    };
    // An abandoned drain keeps reading into the log; the failure carries
    // what it had kept when the wait ended, and nothing it reads later.
    let tail = take_tail(&tail);
    let end = match waited {
        _ if timed_out => End::TimedOut,
        Ok(status) => End::Exited(status),
        Err(e) => End::Error(format!("could not wait for it: {e}")),
    };
    Ran {
        end,
        tail,
        output_held,
        log_error,
    }
}

/// Reads the output to its end, keeping its tail in `tail`, shared with the
/// caller so that what was read survives the drain being abandoned, and
/// copying it to the log; the first log write error, which stops the copy,
/// not the reading. Each chunk joins the tail before the log write, and the
/// tail is never locked during a read or a write.
fn drain(mut reader: impl Read, mut log: File, tail: &Mutex<Tail>) -> Option<String> {
    let mut log_error = None;
    let mut buffer = [0u8; 8192];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => {
                lock(tail).push(&buffer[..n]);
                if log_error.is_none()
                    && let Err(e) = log.write_all(&buffer[..n])
                {
                    log_error = Some(e.to_string());
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    log_error
}

/// The tail kept so far, leaving an empty one in its place.
fn take_tail(tail: &Mutex<Tail>) -> Tail {
    std::mem::take(&mut *lock(tail))
}

/// The tail, even when a thread panicked holding it: what it held is still
/// the output, and the runner must not panic in turn.
fn lock(tail: &Mutex<Tail>) -> MutexGuard<'_, Tail> {
    tail.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The command line run through the platform's shell.
#[cfg(unix)]
fn shell(line: &str, _agent: &AgentEnv) -> Command {
    let mut command = Command::new("sh");
    command.arg("-c").arg(line);
    command
}

/// The command line run through the platform's shell: `cmd.exe`, given the
/// line verbatim, since its quoting rules are not those of other programs.
/// `/s` makes it strip the outer quotes and keep everything between them.
#[cfg(windows)]
fn shell(line: &str, agent: &AgentEnv) -> Command {
    use std::os::windows::process::CommandExt;
    let comspec = agent
        .vars()
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("COMSPEC"))
        .map_or_else(|| "cmd.exe".into(), |(_, value)| value.clone());
    let mut command = Command::new(comspec);
    command.raw_arg(format!("/d /s /c \"{line}\""));
    command
}

/// The last [`OUTPUT_TAIL`] bytes of an output.
#[derive(Default)]
struct Tail {
    bytes: std::collections::VecDeque<u8>,
    truncated: bool,
}

impl Tail {
    fn push(&mut self, chunk: &[u8]) {
        self.bytes.extend(chunk);
        if self.bytes.len() > OUTPUT_TAIL {
            let excess = self.bytes.len() - OUTPUT_TAIL;
            self.bytes.drain(..excess);
            self.truncated = true;
        }
    }

    /// The tail as text of at most [`OUTPUT_TAIL`] bytes, and whether
    /// anything was dropped.
    fn text(self) -> (String, bool) {
        let bytes: Vec<u8> = self.bytes.into();
        tail_text(&bytes, self.truncated)
    }
}

/// `bytes` as text of at most [`OUTPUT_TAIL`] bytes, cut from the front on a
/// character boundary. Invalid UTF-8 becomes replacement characters, which
/// are longer than the bytes they replace, so the limit is applied to the
/// text. A tail cut from a longer output starts without the continuation
/// bytes of a character it cut.
fn tail_text(bytes: &[u8], truncated: bool) -> (String, bool) {
    let start = if truncated {
        bytes
            .iter()
            .take(3)
            .take_while(|&&b| b & 0xC0 == 0x80)
            .count()
    } else {
        0
    };
    let text = String::from_utf8_lossy(&bytes[start..]);
    if text.len() <= OUTPUT_TAIL {
        return (text.into_owned(), truncated);
    }
    let mut cut = text.len() - OUTPUT_TAIL;
    while !text.is_char_boundary(cut) {
        cut += 1;
    }
    (text[cut..].to_owned(), true)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The agent environment of these tests, bare: they check the gate's own
    /// rules; the sandbox's are checked below and in the test bench.
    fn agent() -> AgentEnv {
        AgentEnv::new(std::env::vars_os(), &[])
            .unwrap()
            .without_confinement()
    }

    fn paths(dir: &Path) -> RunPaths {
        RunPaths {
            workdir: dir.to_owned(),
            ..RunPaths::default()
        }
    }

    fn gate(commands: &[String], timeout: Duration) -> (Result<(), GateFailure>, String) {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join(GATE_LOG);
        let mut log = File::create(&log_path).unwrap();
        let outcome = run_commands(
            &agent(),
            &paths(dir.path()),
            commands,
            timeout,
            Instant::now() + timeout,
            &mut log,
        );
        (outcome, std::fs::read_to_string(&log_path).unwrap())
    }

    fn lines(commands: &[&str]) -> Vec<String> {
        commands.iter().map(|c| (*c).to_owned()).collect()
    }

    /// A gate command running one of the helper tests below in a fresh copy
    /// of this test binary.
    fn helper(name: &str) -> String {
        let exe = std::env::current_exe().unwrap();
        format!(
            "\"{}\" --exact executor::gate::tests::{name} --ignored --nocapture --test-threads=1",
            exe.display()
        )
    }

    /// The helpers act only when run alone by [`helper`].
    fn helper_requested() -> bool {
        std::env::args().any(|arg| arg == "--exact")
    }

    #[test]
    #[ignore = "helper, run by the tests below"]
    fn helper_flood() {
        if helper_requested() {
            let mut out = io::stdout();
            out.write_all(&vec![b'x'; 40 * 1024]).unwrap();
            out.write_all(b"END").unwrap();
        }
    }

    #[test]
    #[ignore = "helper, run by the tests below"]
    fn helper_sleep() {
        if helper_requested() {
            thread::sleep(Duration::from_secs(20));
        }
    }

    #[test]
    #[ignore = "helper, run by the tests below"]
    fn helper_leave_a_pipe_holder() {
        if helper_requested() {
            // The child inherits this process's output and outlives it.
            let exe = std::env::current_exe().unwrap();
            drop(
                Command::new(exe)
                    .args([
                        "--exact",
                        "executor::gate::tests::helper_sleep",
                        "--ignored",
                    ])
                    .spawn()
                    .unwrap(),
            );
        }
    }

    #[test]
    fn commands_run_in_order_until_the_first_failure() {
        let (outcome, log) = gate(
            &lines(&["echo first", "exit 3", "echo never"]),
            Duration::from_secs(60),
        );
        let failure = outcome.unwrap_err();
        assert_eq!(failure.command.as_deref(), Some("exit 3"));
        assert_eq!(failure.reason, "exit status 3");
        assert!(log.contains("$ echo first\nfirst"), "{log}");
        assert!(!log.contains("never"), "{log}");
    }

    #[test]
    fn a_failure_keeps_the_end_of_a_long_output() {
        let (outcome, log) = gate(
            &[format!("{} && exit 1", helper("helper_flood"))],
            Duration::from_secs(60),
        );
        let failure = outcome.unwrap_err();
        assert_eq!(failure.reason, "exit status 1");
        assert!(failure.truncated);
        assert!(failure.output.len() <= OUTPUT_TAIL);
        // The test harness prints its own lines after the helper's.
        assert!(failure.output.contains("xEND"));
        let whole = format!("{}END", "x".repeat(40 * 1024));
        assert!(log.contains(&whole), "the log is whole");
    }

    #[test]
    fn a_command_past_the_deadline_is_stopped() {
        let started = Instant::now();
        let (outcome, _) = gate(&[helper("helper_sleep")], Duration::from_secs(1));
        let failure = outcome.unwrap_err();
        assert_eq!(failure.reason, "stopped at the gate's deadline, 1 s");
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "helper, run by the tests below"]
    fn helper_leave_an_escaped_pipe_holder() {
        if helper_requested() {
            use std::os::unix::process::CommandExt;
            println!("BEFORE-THE-HOLD");
            // In a group of its own, stopping the tree does not reach it, and
            // it inherits this process's output.
            drop(
                Command::new("sleep")
                    .arg("10")
                    .process_group(0)
                    .spawn()
                    .unwrap(),
            );
        }
    }

    /// The output read before the drain was abandoned is kept: a process
    /// that left the tree holds the output past the wait, and the failure
    /// still carries what the command wrote.
    #[cfg(unix)]
    #[test]
    fn the_output_read_before_an_abandoned_drain_is_kept() {
        let (outcome, log) = gate(
            &[helper("helper_leave_an_escaped_pipe_holder")],
            Duration::from_secs(5),
        );
        let failure = outcome.unwrap_err();
        assert_eq!(
            failure.reason,
            "a process it started kept its output open after it ended"
        );
        assert!(failure.output.contains("BEFORE-THE-HOLD"), "{failure:?}");
        assert!(log.contains("BEFORE-THE-HOLD"), "{log}");
    }

    /// The tail can be taken while the drain waits in a read, with what it
    /// has read so far, cut as usual.
    #[test]
    fn the_tail_read_so_far_is_taken_while_the_drain_waits() {
        let dir = tempfile::tempdir().unwrap();
        let log = File::create(dir.path().join(GATE_LOG)).unwrap();
        let (reader, mut writer) = io::pipe().unwrap();
        let tail = Arc::new(Mutex::new(Tail::default()));
        let drained = {
            let tail = Arc::clone(&tail);
            thread::spawn(move || drain(reader, log, &tail))
        };
        let mut output = vec![b'x'; OUTPUT_TAIL + 1024];
        output.extend_from_slice(b"END");
        writer.write_all(&output).unwrap();
        // The writer stays open: the drain ends up waiting in its next read.
        let waited = Instant::now();
        loop {
            let kept: Vec<u8> = tail.lock().unwrap().bytes.iter().copied().collect();
            if kept.ends_with(b"END") {
                break;
            }
            assert!(
                waited.elapsed() < Duration::from_secs(30),
                "the drain never published the output"
            );
            thread::sleep(Duration::from_millis(10));
        }
        let (text, truncated) = take_tail(&tail).text();
        assert!(truncated);
        assert_eq!(text.len(), OUTPUT_TAIL);
        assert!(text.ends_with("xEND"));
        drop(writer);
        assert_eq!(drained.join().unwrap(), None);
    }

    /// The tree is stopped once its root exits, so a process left holding
    /// the output neither holds the gate nor fails it.
    #[test]
    fn a_process_left_holding_the_output_does_not_hold_the_gate() {
        let started = Instant::now();
        let (outcome, _) = gate(
            &[helper("helper_leave_a_pipe_holder")],
            Duration::from_secs(60),
        );
        assert_eq!(outcome, Ok(()));
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    /// The agent environment's placeholder token proves the command got
    /// that environment, not the runner's.
    #[test]
    fn commands_get_the_agent_environment() {
        #[cfg(unix)]
        let probe = "test \"$GH_TOKEN\" = owlshift-agent-has-no-credential";
        #[cfg(windows)]
        let probe = "if not \"%GH_TOKEN%\"==\"owlshift-agent-has-no-credential\" exit 1";
        let (outcome, _) = gate(&lines(&[probe]), Duration::from_secs(60));
        assert_eq!(outcome, Ok(()));
    }

    /// `cmd.exe` gets the line verbatim, inner quotes included.
    #[cfg(windows)]
    #[test]
    fn cmd_keeps_the_line_verbatim() {
        let (outcome, log) = gate(
            &lines(&["echo \"a  b\" && exit 5"]),
            Duration::from_secs(60),
        );
        assert_eq!(outcome.unwrap_err().reason, "exit status 5");
        assert!(log.contains("\"a  b\""), "{log}");
    }

    #[test]
    fn the_tail_is_bounded_as_text() {
        // Invalid bytes grow into replacement characters: the text is cut.
        let invalid = vec![0xFFu8; OUTPUT_TAIL];
        let (text, truncated) = tail_text(&invalid, false);
        assert!(truncated);
        assert!(text.len() <= OUTPUT_TAIL);
        assert!(text.chars().all(|c| c == char::REPLACEMENT_CHARACTER));

        // A tail cut inside a character drops what is left of it.
        let (text, truncated) = tail_text("é end".as_bytes()[1..].as_ref(), true);
        assert_eq!((text.as_str(), truncated), (" end", true));

        let (text, truncated) = tail_text(b"short", false);
        assert_eq!((text.as_str(), truncated), ("short", false));
    }

    /// Runs one command as a gate, with this agent, in `paths`, within a
    /// deadline: a command held by a prompt fails as stopped, not as refused.
    fn gate_with(agent: &AgentEnv, paths: &RunPaths, line: &str) -> Result<(), GateFailure> {
        let log_dir = tempfile::tempdir().unwrap();
        let mut log = File::create(log_dir.path().join(GATE_LOG)).unwrap();
        let timeout = Duration::from_secs(30);
        run_commands(
            agent,
            paths,
            &[line.to_owned()],
            timeout,
            Instant::now() + timeout,
            &mut log,
        )
    }

    /// Whether agent runs can be confined here. Where they cannot, the test
    /// is skipped with a message, unless `OWLSHIFT_REQUIRE_CONFINEMENT` is
    /// set, as on a CI leg that must prove the sandbox.
    #[cfg(unix)]
    fn sandbox_or_skip() -> bool {
        match owlshift_platform::sandbox::available() {
            Ok(()) => true,
            Err(error) => {
                assert!(
                    std::env::var_os("OWLSHIFT_REQUIRE_CONFINEMENT").is_none(),
                    "confinement is required here, but: {error}"
                );
                eprintln!("skipped: agent runs cannot be confined here: {error}");
                false
            }
        }
    }

    /// A refusal by the sandbox: the command ran and failed, in time.
    #[cfg(unix)]
    fn refused(outcome: Result<(), GateFailure>, what: &str) {
        let failure = outcome.expect_err(what);
        assert!(
            failure.reason.starts_with("exit status"),
            "{what}: {}",
            failure.reason
        );
        assert!(!failure.output.contains("FAKE_owl41"), "{what}");
    }

    /// OWL-41's acceptance on the gate path: planted fake secrets in a home,
    /// a gh token file and a sibling project's `.env`, are read bare and not
    /// confined; a write into the home never reaches it; the worktree stays
    /// writable. On Linux the runner's own process and the session bus are
    /// out of sight too; on a macOS CI runner, so is the item of a throwaway
    /// keychain.
    #[cfg(unix)]
    #[test]
    fn a_confined_gate_command_reaches_no_planted_secret() {
        if !sandbox_or_skip() {
            return;
        }
        let base = tempfile::tempdir().unwrap();
        let home = base.path().join("home");
        let worktree = base.path().join("worktree");
        std::fs::create_dir_all(home.join(".config/gh")).unwrap();
        std::fs::create_dir_all(home.join("other-project")).unwrap();
        std::fs::create_dir_all(&worktree).unwrap();
        let token = home.join(".config/gh/hosts.yml");
        let env_file = home.join("other-project/.env");
        std::fs::write(
            &token,
            "example.invalid:\n    oauth_token: gho_FAKE_owl41\n",
        )
        .unwrap();
        std::fs::write(&env_file, "API_KEY=FAKE_owl41\n").unwrap();
        // The runner's inherited `TMPDIR` names a folder holding a secret,
        // and another sits in the temporary folder every process shares.
        let inherited_tmp = base.path().join("inherited-tmp");
        std::fs::create_dir_all(&inherited_tmp).unwrap();
        let in_tmpdir = inherited_tmp.join("secret.txt");
        std::fs::write(&in_tmpdir, "FAKE_owl41\n").unwrap();
        let shared = tempfile::Builder::new()
            .prefix("owlshift-owl41-shared-")
            .tempfile()
            .unwrap();
        std::fs::write(shared.path(), "FAKE_owl41\n").unwrap();
        let run_temp = base.path().join("run-temp");
        std::fs::create_dir_all(&run_temp).unwrap();
        // git's configuration folder is opened; a credential file in it is
        // not.
        let git_config = home.join(".config/git");
        std::fs::create_dir_all(&git_config).unwrap();
        std::fs::write(git_config.join("config"), "[core]\n").unwrap();
        let git_credentials = git_config.join("credentials");
        std::fs::write(&git_credentials, "https://u:FAKE_owl41@example.invalid\n").unwrap();

        let parent = std::env::vars_os()
            .filter(|(name, _)| {
                !["HOME", "TMPDIR", "XDG_CONFIG_HOME"].contains(&&*name.to_string_lossy())
            })
            .chain([
                ("HOME".into(), home.clone().into_os_string()),
                ("TMPDIR".into(), inherited_tmp.clone().into_os_string()),
                (
                    "XDG_CONFIG_HOME".into(),
                    home.join(".config").into_os_string(),
                ),
            ]);
        let confined = AgentEnv::new(parent, &[]).unwrap();
        let bare = confined.clone().without_confinement();
        let paths = RunPaths {
            temp: Some(run_temp.clone()),
            ..paths(&worktree)
        };

        for file in [&token, &env_file, &in_tmpdir, &shared.path().to_owned()] {
            let line = format!("cat '{}'", file.display());
            gate_with(&bare, &paths, &line).expect("the bare control reads it");
            refused(gate_with(&confined, &paths, &line), &line);
        }

        // Inside the opened folder, its configuration is read and the
        // credential file is not: refused on macOS, empty on Linux.
        gate_with(
            &confined,
            &paths,
            &format!("cat '{}/config'", git_config.display()),
        )
        .expect("the opened git configuration is read");
        let line = format!("grep -q FAKE_owl41 '{}'", git_credentials.display());
        gate_with(&bare, &paths, &line).expect("the bare control finds the secret");
        refused(gate_with(&confined, &paths, &line), &line);

        // The inherited temporary folder is not written either; the run's
        // own is, and it is the one `TMPDIR` names.
        let planted = inherited_tmp.join("planted.txt");
        let _ = gate_with(
            &confined,
            &paths,
            &format!("echo planted > '{}'", planted.display()),
        );
        assert!(
            !planted.exists(),
            "a confined write reached the inherited TMPDIR"
        );
        gate_with(
            &confined,
            &paths,
            &format!(
                "test \"$TMPDIR\" = '{}' && echo ok > \"$TMPDIR/scratch.txt\"",
                run_temp.display()
            ),
        )
        .expect("the run writes in its own temporary folder");

        let zshrc = home.join(".zshrc");
        let _ = gate_with(
            &confined,
            &paths,
            &format!("echo planted >> '{}'", zshrc.display()),
        );
        assert!(!zshrc.exists(), "a confined write reached the home");

        gate_with(&confined, &paths, "echo ok > written.txt").unwrap();
        // The worktree lies in a closed temporary folder, as it lies in the
        // closed home in real runs: its working directory can still be read
        // by the program `pwd`, not the shell's own.
        gate_with(&confined, &paths, "/bin/pwd -P > /dev/null")
            .expect("a confined command reads its working directory");
        assert_eq!(
            std::fs::read_to_string(worktree.join("written.txt")).unwrap(),
            "ok\n"
        );

        #[cfg(target_os = "linux")]
        {
            let runner = format!("test -e /proc/{}", std::process::id());
            gate_with(&bare, &paths, &runner).expect("the bare control sees the runner");
            refused(
                gate_with(&confined, &paths, &runner),
                "the runner's process",
            );
            let uid = std::process::Command::new("id").arg("-u").output().unwrap();
            let run_user = format!("/run/user/{}", String::from_utf8_lossy(&uid.stdout).trim());
            if std::path::Path::new(&run_user).is_dir() {
                refused(
                    gate_with(&confined, &paths, &format!("test -e {run_user}")),
                    "the session bus folder",
                );
            }
        }

        #[cfg(target_os = "macos")]
        if std::env::var_os("CI").is_some() {
            keychain::a_throwaway_keychain_item_is_out_of_reach(
                &confined,
                &bare,
                &paths,
                base.path(),
            );
        }
    }

    /// The keychain half, on CI only: a throwaway keychain, never on the
    /// search list, created unlocked and without auto-lock, so its reads
    /// cannot prompt; deleted afterwards whatever happens. It stays off the
    /// maintainer's machine, where no automated run may risk a Keychain
    /// dialog.
    #[cfg(target_os = "macos")]
    mod keychain {
        use std::path::{Path, PathBuf};
        use std::process::Command;

        use super::{AgentEnv, RunPaths, gate_with, refused};

        struct Throwaway(PathBuf);

        impl Drop for Throwaway {
            fn drop(&mut self) {
                let _ = Command::new("/usr/bin/security")
                    .arg("delete-keychain")
                    .arg(&self.0)
                    .status();
            }
        }

        fn security(args: &[&str], keychain: &Path) {
            let status = Command::new("/usr/bin/security")
                .args(args)
                .arg(keychain)
                .status()
                .unwrap();
            assert!(status.success(), "security {args:?}");
        }

        pub(super) fn a_throwaway_keychain_item_is_out_of_reach(
            confined: &AgentEnv,
            bare: &AgentEnv,
            paths: &RunPaths,
            base: &Path,
        ) {
            let path = base.join("probe.keychain-db");
            security(&["create-keychain", "-p", "owl41"], &path);
            let keychain = Throwaway(path);
            security(&["set-keychain-settings"], &keychain.0);
            security(&["unlock-keychain", "-p", "owl41"], &keychain.0);
            security(
                &[
                    "add-generic-password",
                    "-s",
                    "owlshift-owl41",
                    "-a",
                    "probe",
                    "-w",
                    "FAKE_owl41",
                ],
                &keychain.0,
            );
            let line = format!(
                "/usr/bin/security find-generic-password -s owlshift-owl41 -w '{}' > /dev/null",
                keychain.0.display()
            );
            security(&["unlock-keychain", "-p", "owl41"], &keychain.0);
            gate_with(bare, paths, &line).expect("the bare control reads the item");
            refused(gate_with(confined, paths, &line), "the keychain item");
        }
    }

    /// Native Windows has no sandbox: a confined gate command is refused, and
    /// says to use WSL2.
    #[cfg(windows)]
    #[test]
    fn a_confined_gate_command_is_refused_on_native_windows() {
        let dir = tempfile::tempdir().unwrap();
        let confined = AgentEnv::new(std::env::vars_os(), &[]).unwrap();
        let failure = gate_with(&confined, &paths(dir.path()), "echo hi").unwrap_err();
        assert!(failure.reason.contains("WSL2"), "{}", failure.reason);
    }

    /// OWL-64: what a gate command plants for git to run never runs in the
    /// gate's own git. Each test ends with a control: a plain `git status`
    /// run afterwards does run what was planted.
    #[cfg(unix)]
    mod planted {
        use super::*;
        use crate::executor::isolation::Snapshot;

        /// A checkout whose `a.txt` names the `owl` filter, and a worktree of
        /// it on `owl-1`, snapshotted as a run's would be.
        struct Repo {
            base: tempfile::TempDir,
            worktree: PathBuf,
            marker: PathBuf,
            snapshot: Snapshot,
        }

        /// The test's own git, away from the operator's configuration.
        fn git_in(dir: &Path, args: &[&str]) -> Vec<u8> {
            let out = Command::new("git")
                .args([
                    "-c",
                    "user.name=owl",
                    "-c",
                    "user.email=owl@example.invalid",
                ])
                .args(["-c", "commit.gpgsign=false"])
                .args(args)
                .current_dir(dir)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}: {out:?}");
            out.stdout
        }

        fn repo() -> Repo {
            let base = tempfile::tempdir().unwrap();
            let root = std::fs::canonicalize(base.path()).unwrap();
            let main = root.join("main");
            std::fs::create_dir(&main).unwrap();
            git_in(&main, &["init", "-q"]);
            std::fs::write(main.join("a.txt"), "a\n").unwrap();
            std::fs::write(main.join(".gitattributes"), "a.txt filter=owl\n").unwrap();
            git_in(&main, &["add", "a.txt", ".gitattributes"]);
            git_in(&main, &["commit", "-q", "-m", "start"]);
            git_in(
                &main,
                &["worktree", "add", "-q", "-b", "owl-1", "../worktree"],
            );
            let worktree = root.join("worktree");
            let snapshot = Snapshot::take(&Git::new("git"), &main, &worktree, "owl-1").unwrap();
            Repo {
                marker: root.join("marker"),
                base,
                worktree,
                snapshot,
            }
        }

        impl Repo {
            fn gate(&self, command: &str) -> GateReport {
                run(
                    &Git::new("git"),
                    &agent(),
                    &paths(&self.worktree),
                    &[command.to_owned()],
                    Duration::from_secs(60),
                    &self.base.path().join(GATE_LOG),
                    &|| self.snapshot.check_files(&self.worktree),
                )
            }

            /// A command that has git touch the marker, for `filter.owl.clean`.
            fn touch_marker(&self) -> String {
                format!("touch '{}'; cat", self.marker.display())
            }

            fn log(&self) -> String {
                std::fs::read_to_string(self.base.path().join(GATE_LOG)).unwrap()
            }
        }

        /// The acceptance: a clean filter planted in the shared config is
        /// caught before the gate's `git status`, and never runs. The file
        /// keeps its size and gets another time, so that git must run the
        /// filter to compare it.
        #[test]
        fn a_planted_clean_filter_never_runs() {
            let repo = repo();
            let report = repo.gate(&format!(
                "git config filter.owl.clean \"{}\" && touch -t 200001010000 a.txt",
                repo.touch_marker()
            ));
            assert!(
                matches!(report.breach.as_slice(), [Violation::SharedGitFiles(files)] if files == &["config"]),
                "{report:?}"
            );
            assert!(!report.passed());
            assert!(report.log.exists() && repo.log().contains("isolation broken"));
            assert!(!repo.marker.exists(), "the gate's git ran the filter");

            git_in(&repo.worktree, &["status", "--porcelain"]);
            assert!(repo.marker.exists(), "the control never ran the filter");
        }

        /// A `.git` link redirected by a gate command stops the gate as well.
        #[test]
        fn a_redirected_link_stops_the_gate() {
            let repo = repo();
            let other = repo.base.path().join("other");
            std::fs::create_dir(&other).unwrap();
            git_in(&other, &["init", "-q"]);
            let report = repo.gate(&format!(
                "printf 'gitdir: {}\\n' > .git",
                other.join(".git").display()
            ));
            assert!(
                matches!(report.breach.as_slice(), [Violation::WorktreeLink(_)]),
                "{report:?}"
            );
            assert!(!report.passed());
        }

        /// A gitlink planted in the worktree's index, over a repository
        /// holding its own filter, is not looked into by the gate's status:
        /// nothing checks that repository's configuration.
        #[test]
        fn a_planted_submodule_is_not_looked_into() {
            let repo = repo();
            // The command builds a repository with its own filter in the
            // worktree, stages it as a gitlink, and gives its file another
            // time. Adding the file runs the filter once: the marker goes.
            let report = repo.gate(&format!(
                "export GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1 && \
                 git init -q sub && cd sub && git config filter.owl.clean \"{marker}\" && \
                 printf 'f filter=owl\\n' > .gitattributes && printf 'f\\n' > f && \
                 git add f .gitattributes && \
                 git -c user.name=owl -c user.email=owl@example.invalid \
                 -c commit.gpgsign=false commit -q -m sub && rm -f '{path}' && cd .. && \
                 git update-index --add --cacheinfo \"160000,$(git -C sub rev-parse HEAD),sub\" && \
                 touch -t 200001010000 sub/f",
                marker = repo.touch_marker(),
                path = repo.marker.display(),
            ));
            assert!(report.breach.is_empty(), "{report:?}");
            let command_failed = report.failure.as_ref().is_some_and(|f| f.command.is_some());
            assert!(!command_failed, "{report:?}");
            assert!(!repo.marker.exists(), "the gate's git ran the filter");

            git_in(&repo.worktree, &["status", "--porcelain"]);
            assert!(repo.marker.exists(), "the control never ran the filter");
        }
    }
}
