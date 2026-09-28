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
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use owlshift_contracts::brief::GateFailure;

use super::git::Git;
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
    /// Why the gate failed; `None` when it passed.
    pub failure: Option<GateFailure>,
}

impl GateReport {
    pub fn passed(&self) -> bool {
        self.failure.is_none()
    }
}

/// Runs the gate in the worktree of `paths`, each command confined to them
/// as the run was (OWL-41); see the module documentation. Whatever goes
/// wrong, the runner's side included, is a failure: a gate that could not be
/// run never passes.
pub(crate) fn run(
    git: &Git,
    agent: &AgentEnv,
    paths: &RunPaths,
    commands: &[String],
    timeout: Duration,
    log_path: &Path,
) -> GateReport {
    let deadline = Instant::now() + timeout;
    let git = git.as_agent(agent);
    let mut report = GateReport {
        commands: commands.to_vec(),
        commit: None,
        log: log_path.to_owned(),
        failure: None,
    };
    let checked = check_and_run(&git, agent, paths, commands, timeout, deadline, &mut report);
    report.failure = checked.err();
    report
}

fn check_and_run(
    git: &Git,
    agent: &AgentEnv,
    paths: &RunPaths,
    commands: &[String],
    timeout: Duration,
    deadline: Instant,
    report: &mut GateReport,
) -> Result<(), GateFailure> {
    let worktree = paths.workdir.as_path();
    let mut log = File::create(&report.log).map_err(|e| {
        around(format!(
            "the runner could not write the gate log {}: {e}",
            report.log.display()
        ))
    })?;
    let before = head(git, worktree)?;
    report.commit = Some(before.clone());
    let dirty = uncommitted(git, worktree)?;
    if !dirty.is_empty() {
        return Err(around(format!(
            "uncommitted changes before the gate, which runs on the last commit: {dirty}"
        )));
    }
    run_commands(agent, paths, commands, timeout, deadline, &mut log)?;
    let dirty = uncommitted(git, worktree)?;
    if !dirty.is_empty() {
        return Err(around(format!("the gate changed files: {dirty}")));
    }
    let after = head(git, worktree)?;
    if after != before {
        return Err(around(format!(
            "the gate moved HEAD from {before} to {after}"
        )));
    }
    Ok(())
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
/// when it is clean. Ignored files do not count.
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
            End::Exited(_) if ran.output_held => Some(format!(
                "a process it started kept its output open, {} s after it was stopped",
                OUTPUT_GRACE.as_secs()
            )),
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

    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        // The receiver is gone when the output was abandoned.
        let _ = sender.send(drain(reader, log));
    });
    let waited = child.wait();
    let timed_out = watchdog.finish();
    // What the command left running would hold the output open; the tree
    // stops it. Best effort, as for the harness.
    let _ = tree.kill();
    let wait_for = deadline
        .saturating_duration_since(Instant::now())
        .max(OUTPUT_GRACE);
    let (tail, log_error, output_held) = match receiver.recv_timeout(wait_for) {
        Ok((tail, log_error)) => (tail, log_error, false),
        Err(_) => (Tail::default(), None, true),
    };
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

/// Reads the output to its end, copying it to the log and keeping its tail.
/// A log write error stops the copy, not the reading.
fn drain(mut reader: impl Read, mut log: File) -> (Tail, Option<String>) {
    let mut tail = Tail::default();
    let mut log_error = None;
    let mut buffer = [0u8; 8192];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => {
                if log_error.is_none()
                    && let Err(e) = log.write_all(&buffer[..n])
                {
                    log_error = Some(e.to_string());
                }
                tail.push(&buffer[..n]);
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    (tail, log_error)
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

        let parent = std::env::vars_os()
            .filter(|(name, _)| name != "HOME")
            .chain([("HOME".into(), home.clone().into_os_string())]);
        let confined = AgentEnv::new(parent, &[]).unwrap();
        let bare = confined.clone().without_confinement();
        let paths = paths(&worktree);

        for file in [&token, &env_file] {
            let line = format!("cat '{}'", file.display());
            gate_with(&bare, &paths, &line).expect("the bare control reads it");
            refused(gate_with(&confined, &paths, &line), &line);
        }

        let zshrc = home.join(".zshrc");
        let _ = gate_with(
            &confined,
            &paths,
            &format!("echo planted >> '{}'", zshrc.display()),
        );
        assert!(!zshrc.exists(), "a confined write reached the home");

        gate_with(&confined, &paths, "echo ok > written.txt").unwrap();
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
}
