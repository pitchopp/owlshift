//! The project's gate, run by the runner itself after a Build run reports
//! `done` (OWL-16): the role's own claim that the gate is green is never
//! taken as proof.
//!
//! `run` runs the brief's `gate` commands in the worktree, in order, each
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
//! The worktree's attributes, which the run wrote, choose the filter driver
//! each file goes through, and a driver already configured (git-lfs in the
//! user's configuration, one the repository had) would run in the gate's
//! status on content the run chose (OWL-73). Just before each status, the
//! gate lists every driver git would find and switches each one off for that
//! command (`-c filter.<name>.clean=`, `smudge`, `process`, and
//! `required=false`). A driver whose name a `-c` argument cannot carry fails
//! the gate, and so does a list git could not give. The cost: a file a driver
//! filters is read raw, so once its timestamps change it reads as modified,
//! and the gate fails. The status runs no other driver (diff, merge), and
//! `rev-parse` reads no attributes (checked on 2026-09-29, build plan).
//!
//! Every command's output, standard output and standard error interleaved,
//! goes whole to the gate log in the caller's run directory; a failure keeps
//! the last [`OUTPUT_TAIL`] bytes for the next Build run's brief. An output a
//! process that left the tree holds open is abandoned: what was read until
//! then is in the log and the tail alike, and what is read later reaches
//! neither, so no line of it lands after the command's status (OWL-69). The gate
//! commands share one deadline: each gets what is left of it, none starts
//! once it is spent, and a command, or a process it started, still running
//! at the deadline is stopped with its whole tree.
//!
//! The project's setup (`stack.setup`, OWL-330) runs first, the same way,
//! logged at the head of the same log, under a deadline of its own: a third
//! of the gate's ([`SETUP_SHARE`]). A failing setup command fails the gate,
//! its reason starting `setup: `; once it passes, the guard, the clean check
//! and HEAD are checked again, and the gate's commands get their whole
//! deadline. `run_setup` runs the setup alone before a Build's harness,
//! where the worktree may hold a resumed Build's unfinished work: it must
//! leave that work as it found it, which it reads before and after
//! (`worktree_state`) rather than requiring a clean tree.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use owlshift_contracts::brief::GateFailure;
use sha2::{Digest, Sha256};

use super::git::Git;
use super::isolation::Violation;
use super::watch;
use crate::agent_env::{AgentEnv, RunPaths};
use crate::config::exit_text;

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

/// The share of the gate's deadline the project's setup gets: a third
/// (OWL-330). The gate's commands keep their whole deadline after it.
pub const SETUP_SHARE: u32 = 3;

/// The start of the reason of a gate refused for uncommitted changes, before
/// any command ran (`refused_uncommitted`).
pub const UNCOMMITTED_PREFIX: &str = "uncommitted changes before the gate";

/// The file of the run directory a Build's setup before its harness logs
/// to (`run_setup`).
pub const SETUP_LOG: &str = "setup.log";

/// The start of the reason of a setup command's failure.
pub const SETUP_PREFIX: &str = "setup: ";

/// Whose a failed command's failure is (OWL-330): internal, never written
/// into a brief or a report.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Fault {
    /// The project's: the command exited non-zero, was ended by a signal or
    /// reached the deadline, and nothing went wrong on the runner's side.
    Command,
    /// The runner's: a command it could not confine, start or watch, a log
    /// it could not write, an output a process kept open. It wins over the
    /// command's own end, whatever that was.
    Runner,
}

/// A command that failed, and whose failure it is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CommandFailure {
    pub failure: GateFailure,
    pub fault: Fault,
}

/// Which commands [`run_commands`] runs: the deadline's words differ.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Gate,
    Setup,
}

impl Phase {
    /// The reason of a command stopped at the deadline of `timeout`.
    fn stopped(self, timeout: Duration) -> String {
        match self {
            Self::Gate => format!("stopped at the gate's deadline, {} s", timeout.as_secs()),
            Self::Setup => format!(
                "stopped at the setup's deadline, a third of the gate's, {} s",
                timeout.as_secs()
            ),
        }
    }
}

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
    /// Each command started, in order, the failing one last: what
    /// `owlshift gate` prints (OWL-329). Empty when none started.
    pub steps: Vec<GateStep>,
    /// Each setup command started before them, in order, as `steps`
    /// (OWL-330). Empty when the project has none or none started.
    pub setup: Vec<GateStep>,
    /// The failing command's failure was the runner's, not the command's
    /// (`Fault::Runner`: one it could not confine, start or watch, a log it
    /// could not write, an output a process kept open), so no base check
    /// may take it for the project's (OWL-418).
    pub runner_fault: bool,
}

impl GateReport {
    pub fn passed(&self) -> bool {
        self.failure.is_none() && self.breach.is_empty()
    }
}

/// One gate command the runner started, and how it ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GateStep {
    /// The command, as in the brief's `gate`.
    pub command: String,
    /// Why it failed the gate, which can be after an exit status 0 (an
    /// output held open, a log that could not be written); `None` when it
    /// passed.
    pub failure: Option<String>,
    /// Its time, from its start until its output closed or was abandoned.
    pub elapsed: Duration,
    /// The last [`OUTPUT_TAIL`] bytes of its output, as a failure keeps them.
    pub output: String,
    /// Whether the output was cut from the front.
    pub truncated: bool,
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
/// own git steps; see the module documentation. `setup`, the project's
/// `stack.setup`, runs first (OWL-330). Whatever goes wrong, the runner's
/// side included, is a failure: a gate that could not be run never passes. A
/// violation the guard finds is a breach, and a failure as well.
#[allow(
    clippy::too_many_arguments,
    reason = "one gate run's inputs, each its own"
)]
pub(crate) fn run(
    git: &Git,
    agent: &AgentEnv,
    paths: &RunPaths,
    setup: &[String],
    commands: &[String],
    timeout: Duration,
    log_path: &Path,
    guard: Guard<'_>,
) -> GateReport {
    let deadline = Instant::now() + timeout;
    let git = git.as_agent(agent);
    let mut report = empty_report(commands, log_path);
    let checked = check_and_run(
        &git,
        agent,
        paths,
        (setup, commands),
        (timeout, deadline),
        guard,
        &mut report,
    );
    match checked {
        Ok(()) => {}
        Err(Stop::Failed(failure)) => report.failure = Some(failure),
        Err(Stop::Breach(violations)) => breached(&mut report, violations),
    }
    report
}

/// A report of a gate of `commands`, logged to `log_path`, before anything
/// ran.
fn empty_report(commands: &[String], log_path: &Path) -> GateReport {
    GateReport {
        commands: commands.to_vec(),
        commit: None,
        log: log_path.to_owned(),
        failure: None,
        breach: Vec::new(),
        steps: Vec::new(),
        setup: Vec::new(),
        runner_fault: false,
    }
}

/// Records the guard's `violations` in `report`, as its failure too.
fn breached(report: &mut GateReport, violations: Vec<Violation>) {
    let reasons: Vec<String> = violations.iter().map(ToString::to_string).collect();
    report.failure = Some(around(format!(
        "the gate broke isolation, found before its own git ran: {}",
        reasons.join("; ")
    )));
    report.breach = violations;
}

/// A setup command's failure, as the gate reports it: its reason starts
/// [`SETUP_PREFIX`].
fn setup_failure(failure: GateFailure) -> GateFailure {
    GateFailure {
        reason: format!("{SETUP_PREFIX}{}", failure.reason),
        ..failure
    }
}

fn check_and_run(
    git: &Git,
    agent: &AgentEnv,
    paths: &RunPaths,
    (setup, commands): (&[String], &[String]),
    (timeout, mut deadline): (Duration, Instant),
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
            "{UNCOMMITTED_PREFIX}, which runs on the last commit: {dirty}"
        ))
        .into());
    }
    if !setup.is_empty() {
        let share = timeout / SETUP_SHARE;
        let ran = run_commands(
            agent,
            paths,
            (setup, Phase::Setup),
            share,
            Instant::now() + share,
            &mut log,
            &mut report.setup,
        );
        if let Err(failed) = ran {
            report.runner_fault = failed.fault == Fault::Runner;
            return Err(setup_failure(failed.failure).into());
        }
        // The setup ran the branch's code, as the gate does: checked as the
        // gate is, before the gate's commands get their whole deadline.
        guarded(guard, &mut log)?;
        let dirty = uncommitted(git, worktree)?;
        if !dirty.is_empty() {
            return Err(around(format!("the setup changed files: {dirty}")).into());
        }
        let after = head(git, worktree)?;
        if after != before {
            return Err(around(format!("the setup moved HEAD from {before} to {after}")).into());
        }
        deadline = Instant::now() + timeout;
    }
    let ran = run_commands(
        agent,
        paths,
        (commands, Phase::Gate),
        timeout,
        deadline,
        &mut log,
        &mut report.steps,
    );
    if let Err(failed) = ran {
        report.runner_fault = failed.fault == Fault::Runner;
        return Err(failed.failure.into());
    }
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

/// Whether `failure` is the gate's refusal of uncommitted changes, before
/// any command ran: the next run, told of it, can clean them up, so it does
/// not spend the stage's fix run (OWL-421, `on_demand::stage_event`).
pub(crate) fn refused_uncommitted(failure: &GateFailure) -> bool {
    failure.command.is_none() && failure.reason.starts_with(UNCOMMITTED_PREFIX)
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
/// guard does not check. Every filter driver git would find is switched off
/// ([`filter_drivers_off`]); a list read that way names them.
fn uncommitted(git: &Git, worktree: &Path) -> Result<String, GateFailure> {
    let status = status(git, worktree)?;
    let paths: Vec<String> = status
        .entries
        .iter()
        .map(|entry| String::from_utf8_lossy(&entry.path).into_owned())
        .collect();
    let mut listed = listed(&paths);
    if !listed.is_empty() && !status.drivers.is_empty() {
        listed.push_str(&format!(
            " (read with the filter drivers {} switched off: a file they filter reads as \
             modified once its timestamps change)",
            status.drivers.join(", ")
        ));
    }
    Ok(listed)
}

/// `paths` as a short list: the first [`LISTED_PATHS`], then "and N more".
fn listed(paths: &[String]) -> String {
    let mut listed = paths
        .iter()
        .take(LISTED_PATHS)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    if paths.len() > LISTED_PATHS {
        listed.push_str(&format!(" and {} more", paths.len() - LISTED_PATHS));
    }
    listed
}

/// One path `git status` lists: its two-letter code and its path, as git
/// wrote them, bytes and all (OWL-330).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct StatusEntry {
    code: [u8; 2],
    path: Vec<u8>,
}

/// The worktree's status, read as [`uncommitted`] reads it: every entry,
/// uncapped, and the filter drivers switched off to read it.
struct Status {
    entries: Vec<StatusEntry>,
    drivers: Vec<String>,
}

/// Reads the worktree's status: see [`uncommitted`] for what it leaves out
/// and why.
fn status(git: &Git, worktree: &Path) -> Result<Status, GateFailure> {
    let (drivers, off) = filter_drivers_off(git, worktree)?;
    let mut args: Vec<&str> = off.iter().map(String::as_str).collect();
    args.extend([
        "status",
        "--porcelain",
        "-z",
        "--untracked-files=all",
        "--no-renames",
        "--ignore-submodules=all",
    ]);
    let out = git.run(worktree, &agent_git_args(&args)).map_err(|e| {
        around(format!(
            "the runner could not read the worktree's status: {e}"
        ))
    })?;
    // Each entry is `XY path`, NUL-terminated; with no renames, no entry
    // carries a second path.
    let entries = out
        .split(|&b| b == 0)
        .filter(|entry| entry.len() > 3)
        .map(|entry| StatusEntry {
            code: [entry[0], entry[1]],
            path: entry[3..].to_vec(),
        })
        .collect();
    Ok(Status { entries, drivers })
}

/// Every filter driver git would find for the worktree, and the arguments
/// that switch each one off for a command: the gate's status runs outside
/// the sandbox, and the worktree's attributes, which the run wrote, choose
/// the driver a file goes through (OWL-73). The drivers are read just before
/// each status, from the configuration that status reads.
fn filter_drivers_off(
    git: &Git,
    worktree: &Path,
) -> Result<(Vec<String>, Vec<String>), GateFailure> {
    let listing = |detail: String| {
        around(format!(
            "the runner could not list the filter drivers git would run: {detail}"
        ))
    };
    let out = git
        .output(
            worktree,
            &agent_git_args(&["config", "-z", "--get-regexp", r"^filter\."]),
            None,
        )
        .map_err(|e| listing(e.to_string()))?;
    let records = match out.code {
        Some(0) => out.stdout,
        // No key matched.
        Some(1) if out.stdout.is_empty() => Vec::new(),
        code => {
            return Err(listing(format!(
                "git config failed with {}: {}",
                exit_text(code),
                String::from_utf8_lossy(&out.stderr).trim()
            )));
        }
    };
    let drivers = filter_drivers(&records).map_err(|name| {
        around(format!(
            "a filter driver named {name:?} is configured, which the gate cannot switch off \
             for its own git: the gate does not run"
        ))
    })?;
    let off = drivers
        .iter()
        .flat_map(|name| {
            ["clean=", "smudge=", "process=", "required=false"]
                .map(|setting| ["-c".to_owned(), format!("filter.{name}.{setting}")])
        })
        .flatten()
        .collect();
    Ok((drivers, off))
}

/// The names of the filter drivers in `records`, the output of `git config
/// -z --get-regexp`: each record is a key, then a newline and its value when
/// it has one. A key without a driver name is skipped, as git skips it. A
/// name that a `-c` argument cannot carry (holding `=`, empty, or not UTF-8)
/// is the error.
fn filter_drivers(records: &[u8]) -> Result<Vec<String>, String> {
    let mut names = std::collections::BTreeSet::new();
    for record in records.split(|&b| b == 0).filter(|r| !r.is_empty()) {
        let key = record.split(|&b| b == b'\n').next().unwrap_or_default();
        let Some(rest) = key.strip_prefix(b"filter.") else {
            continue;
        };
        let Some(end) = rest.iter().rposition(|&b| b == b'.') else {
            continue;
        };
        let name = &rest[..end];
        match std::str::from_utf8(name) {
            Ok(name) if !name.is_empty() && !name.contains('=') => {
                names.insert(name.to_owned());
            }
            _ => return Err(String::from_utf8_lossy(name).into_owned()),
        }
    }
    Ok(names.into_iter().collect())
}

/// The arguments of a git command run as the agent: no optional lock, and
/// no file-system monitor hook, a command the repository's configuration
/// could name.
fn agent_git_args<'a>(args: &[&'a str]) -> Vec<&'a str> {
    let mut all = vec!["-c", "core.fsmonitor=false", "--no-optional-locks"];
    all.extend_from_slice(args);
    all
}

/// What the project's setup came to before a Build's harness
/// ([`run_setup`], OWL-330).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SetupRun {
    /// The setup's report: its steps in `setup`, no gate command, its failure
    /// and a breach the guard found, as a gate's.
    pub report: GateReport,
    /// Whether the worktree held the Build's own work when the setup
    /// started: an entry `git status` lists, or a HEAD that is neither the
    /// run's base nor an ancestor of it. Read before the commands ran.
    pub resumed: bool,
    /// Whose the failure is: `Some(Fault::Command)` only when a setup
    /// command failed the project's way and the work was found as it was
    /// left, the guard passing; `None` when the setup passed, and for every
    /// failure around the commands.
    pub fault: Option<Fault>,
}

impl SetupRun {
    /// Whether a red setup may still let the harness start (OWL-330, round
    /// 3): only on a worktree that holds the Build's own work, and only for
    /// the command's own failure, the work left as it was. A passed setup
    /// is not recoverable: there is nothing to recover from.
    pub(crate) fn recoverable(&self) -> bool {
        self.resumed && self.fault == Some(Fault::Command)
    }
}

/// Why the setup before a harness stopped short of passing.
enum SetupStop {
    /// A failure, and whose it is when a command failed.
    Failed(GateFailure, Option<Fault>),
    Breach(Vec<Violation>),
}

impl From<GateFailure> for SetupStop {
    fn from(failure: GateFailure) -> Self {
        Self::Failed(failure, None)
    }
}

impl From<Stop> for SetupStop {
    fn from(stop: Stop) -> Self {
        match stop {
            Stop::Failed(failure) => Self::Failed(failure, None),
            Stop::Breach(violations) => Self::Breach(violations),
        }
    }
}

/// Runs the project's `setup` in the worktree of `paths` before a Build's
/// harness (OWL-330), each command confined as a gate command is, under a
/// third of `timeout`, the gate's deadline, logged to `log_path`. The guard
/// is checked before the runner's own git and again after the commands.
///
/// The worktree may hold a resumed Build's unfinished work, so it need not
/// be clean: its state ([`worktree_state`]) is read before the commands and
/// again after them, passed or failed, and any difference fails the setup,
/// a moved HEAD as the gate says it. `base_commit` is the run's base, a
/// commit id: the worktree holds the Build's own work when status lists an
/// entry or HEAD is neither it nor an ancestor of it.
#[allow(
    clippy::too_many_arguments,
    reason = "one setup run's inputs, each its own"
)]
pub(crate) fn run_setup(
    git: &Git,
    agent: &AgentEnv,
    paths: &RunPaths,
    base_commit: &str,
    setup: &[String],
    timeout: Duration,
    log_path: &Path,
    guard: Guard<'_>,
) -> SetupRun {
    let git = git.as_agent(agent);
    let mut run = SetupRun {
        report: empty_report(&[], log_path),
        resumed: false,
        fault: None,
    };
    let checked = check_and_set_up(
        &git,
        agent,
        paths,
        base_commit,
        setup,
        timeout,
        guard,
        &mut run,
    );
    match checked {
        Ok(()) => {}
        Err(SetupStop::Failed(failure, fault)) => {
            run.report.failure = Some(failure);
            run.report.runner_fault = fault == Some(Fault::Runner);
            run.fault = fault;
        }
        Err(SetupStop::Breach(violations)) => {
            breached(&mut run.report, violations);
            run.fault = None;
        }
    }
    run
}

#[allow(clippy::too_many_arguments, reason = "run_setup's inputs, passed on")]
fn check_and_set_up(
    git: &Git,
    agent: &AgentEnv,
    paths: &RunPaths,
    base_commit: &str,
    setup: &[String],
    timeout: Duration,
    guard: Guard<'_>,
    run: &mut SetupRun,
) -> Result<(), SetupStop> {
    let worktree = paths.workdir.as_path();
    let report = &mut run.report;
    let mut log = File::create(&report.log).map_err(|e| {
        around(format!(
            "the runner could not write the setup log {}: {e}",
            report.log.display()
        ))
    })?;
    guarded(guard, &mut log)?;
    let before = worktree_state(git, worktree)?;
    report.commit = Some(before.head.clone());
    let at_or_behind = git
        .is_ancestor(worktree, &before.head, base_commit)
        .map_err(|e| {
            around(format!(
                "the runner could not tell whether HEAD is the run's base {base_commit} or \
                 behind it: {e}"
            ))
        })?;
    run.resumed = !before.entries.is_empty() || !at_or_behind;
    let share = timeout / SETUP_SHARE;
    let ran = run_commands(
        agent,
        paths,
        (setup, Phase::Setup),
        share,
        Instant::now() + share,
        &mut log,
        &mut report.setup,
    );
    // Passed or failed, the commands ran the branch's code: the guard, then
    // the work as it was left.
    guarded(guard, &mut log)?;
    let after = worktree_state(git, worktree)?;
    if after.head != before.head {
        return Err(around(format!(
            "the setup moved HEAD from {} to {}",
            before.head, after.head
        ))
        .into());
    }
    let changed = before.changed(&after);
    if !changed.is_empty() {
        return Err(around(format!(
            "the setup changed unfinished work: {}",
            listed(&changed)
        ))
        .into());
    }
    ran.map_err(|failed| SetupStop::Failed(setup_failure(failed.failure), Some(failed.fault)))
}

/// What a setup must leave as it found it before a Build's harness
/// (OWL-330): HEAD, the index, and every path `git status` lists with its
/// code and its content on disk. Ignored files are outside it.
#[derive(Clone, Debug, PartialEq, Eq)]
struct WorktreeState {
    head: String,
    /// Each index entry by its path: `<mode> <blob> <stage>`, as
    /// `git ls-files -s -z` prints it, every stage of a conflict.
    index: BTreeMap<Vec<u8>, Vec<Vec<u8>>>,
    /// Each path status lists, with its code and what is on disk there.
    entries: BTreeMap<Vec<u8>, ([u8; 2], OnDisk)>,
}

impl WorktreeState {
    /// The paths whose index entries, status or content differ between
    /// `self` and `after`, each once, as text.
    fn changed(&self, after: &Self) -> Vec<String> {
        let mut paths: BTreeSet<&Vec<u8>> = BTreeSet::new();
        for (one, other) in [(self, after), (after, self)] {
            paths.extend(
                one.index
                    .iter()
                    .filter(|(path, stages)| other.index.get(*path) != Some(stages))
                    .map(|(path, _)| path),
            );
            paths.extend(
                one.entries
                    .iter()
                    .filter(|(path, entry)| other.entries.get(*path) != Some(entry))
                    .map(|(path, _)| path),
            );
        }
        paths
            .into_iter()
            .map(|path| String::from_utf8_lossy(path).into_owned())
            .collect()
    }
}

/// What is on disk at a path status lists, read by the runner itself, never
/// through git: no filter, textconv or diff driver the worktree names runs.
#[derive(Clone, Debug, PartialEq, Eq)]
enum OnDisk {
    /// A regular file: its content's SHA-256, and whether it is executable
    /// (always false where the system has no such bit).
    File { sha256: [u8; 32], executable: bool },
    /// A symbolic link: its target, never followed.
    Link(PathBuf),
    /// Nothing there, such as a tracked file the worktree deleted.
    Absent,
    /// A folder above it is a symbolic link: not followed, so nothing
    /// outside the worktree is read.
    UnderLink,
    /// Anything else, by its kind, never opened.
    Other(&'static str),
}

/// Reads the worktree's state ([`WorktreeState`]) with git run as the agent,
/// filter drivers off for the status as the gate reads it, and the index
/// read as it is: `ls-files` runs no filter and looks into no submodule.
fn worktree_state(git: &Git, worktree: &Path) -> Result<WorktreeState, GateFailure> {
    let head = head(git, worktree)?;
    let listing = git
        .run(worktree, &agent_git_args(&["ls-files", "-s", "-z"]))
        .map_err(|e| around(format!("the runner could not read the index: {e}")))?;
    let mut index: BTreeMap<Vec<u8>, Vec<Vec<u8>>> = BTreeMap::new();
    for record in listing.split(|&b| b == 0).filter(|r| !r.is_empty()) {
        // `<mode> <blob> <stage>\t<path>`.
        let Some(tab) = record.iter().position(|&b| b == b'\t') else {
            return Err(around(
                "the runner could not read the index: an entry without a path".to_owned(),
            ));
        };
        index
            .entry(record[tab + 1..].to_vec())
            .or_default()
            .push(record[..tab].to_vec());
    }
    let mut entries = BTreeMap::new();
    for entry in status(git, worktree)?.entries {
        let disk = on_disk(worktree, &entry.path).map_err(|e| {
            around(format!(
                "the runner could not read {}: {e}",
                String::from_utf8_lossy(&entry.path)
            ))
        })?;
        entries.insert(entry.path, (entry.code, disk));
    }
    Ok(WorktreeState {
        head,
        index,
        entries,
    })
}

/// What is on disk at `path`, as status wrote it, in `worktree`: each folder
/// above it is checked first, and none that is a link is followed.
fn on_disk(worktree: &Path, path: &[u8]) -> io::Result<OnDisk> {
    let relative = path_from_bytes(path);
    let parts: Vec<_> = relative.components().collect();
    let mut at = worktree.to_path_buf();
    for (i, part) in parts.iter().enumerate() {
        at.push(part);
        let meta = match std::fs::symlink_metadata(&at) {
            Ok(meta) => meta,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(OnDisk::Absent),
            Err(e) => return Err(e),
        };
        let last = i + 1 == parts.len();
        if !last {
            if meta.file_type().is_symlink() {
                return Ok(OnDisk::UnderLink);
            }
            if !meta.is_dir() {
                return Ok(OnDisk::Absent);
            }
            continue;
        }
        let kind = meta.file_type();
        return Ok(if kind.is_symlink() {
            OnDisk::Link(std::fs::read_link(&at)?)
        } else if kind.is_file() {
            OnDisk::File {
                sha256: file_sha256(&at)?,
                executable: executable(&meta),
            }
        } else if kind.is_dir() {
            OnDisk::Other("folder")
        } else {
            OnDisk::Other("special file")
        });
    }
    Ok(OnDisk::Absent)
}

/// A path as git printed it: bytes on Unix, UTF-8 elsewhere.
fn path_from_bytes(path: &[u8]) -> PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        PathBuf::from(std::ffi::OsStr::from_bytes(path))
    }
    #[cfg(not(unix))]
    {
        PathBuf::from(String::from_utf8_lossy(path).into_owned())
    }
}

/// Whether a file's mode lets its owner, group or others run it; always
/// false where the system keeps no such bit.
fn executable(meta: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        false
    }
}

/// The SHA-256 of the file at `path`, read in chunks.
fn file_sha256(path: &Path) -> io::Result<[u8; 32]> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        match file.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => hasher.update(&buffer[..n]),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(hasher.finalize().into())
}

/// Runs the commands in order until the first failure, sharing one deadline,
/// and records each one started in `steps`. A failure says whose it is
/// ([`classify`]): a command not started because the deadline had passed is
/// the command's, a log line that could not be written the runner's.
fn run_commands(
    agent: &AgentEnv,
    paths: &RunPaths,
    (commands, phase): (&[String], Phase),
    timeout: Duration,
    deadline: Instant,
    log: &mut File,
    steps: &mut Vec<GateStep>,
) -> Result<(), CommandFailure> {
    for command in commands {
        let failed = |reason: String, tail: Tail, fault: Fault| {
            let (output, truncated) = tail.text();
            CommandFailure {
                failure: GateFailure {
                    command: Some(command.clone()),
                    reason,
                    output,
                    truncated,
                },
                fault,
            }
        };
        let stopped = || phase.stopped(timeout);
        if Instant::now() >= deadline {
            return Err(failed(stopped(), Tail::default(), Fault::Command));
        }
        if let Err(e) = writeln!(log, "$ {command}") {
            return Err(failed(
                format!("the runner could not write the gate log: {e}"),
                Tail::default(),
                Fault::Runner,
            ));
        }
        let started = Instant::now();
        let ran = run_one(agent, paths, command, deadline, log);
        let elapsed = started.elapsed();
        let status = match &ran.end {
            End::Exited(status) => status.to_string(),
            End::TimedOut => "stopped at the deadline".to_owned(),
            End::Error(error) => format!("runner error: {error}"),
        };
        let _ = writeln!(log, "[{status}]");
        let classified = classify(ran.end, ran.output_held, ran.log_error, stopped);
        let (output, truncated) = ran.tail.text();
        steps.push(GateStep {
            command: command.clone(),
            failure: classified.as_ref().map(|(reason, _)| reason.clone()),
            elapsed,
            output: output.clone(),
            truncated,
        });
        if let Some((reason, fault)) = classified {
            return Err(CommandFailure {
                failure: GateFailure {
                    command: Some(command.clone()),
                    reason,
                    output,
                    truncated,
                },
                fault,
            });
        }
    }
    Ok(())
}

/// Why a command that ended so failed, and whose failure it is; `None` when
/// it passed. The reason keeps the gate's order: the deadline, a runner
/// error, the exit status, a held output, a log write error. The fault is the
/// runner's for a runner error, a held output or a log write error, whatever
/// the end; the command's only for a non-zero exit, a signal or the deadline
/// with neither (OWL-330, round 4).
fn classify(
    end: End,
    output_held: bool,
    log_error: Option<String>,
    stopped: impl FnOnce() -> String,
) -> Option<(String, Fault)> {
    let fault = if output_held || log_error.is_some() {
        Fault::Runner
    } else {
        Fault::Command
    };
    match end {
        End::TimedOut => Some((stopped(), fault)),
        End::Error(error) => Some((error, Fault::Runner)),
        End::Exited(status) if !status.success() => Some((
            match status.code() {
                Some(code) => format!("exit status {code}"),
                None => "ended by a signal".to_owned(),
            },
            fault,
        )),
        End::Exited(_) if output_held => Some((
            "a process it started kept its output open after it ended".to_owned(),
            Fault::Runner,
        )),
        End::Exited(_) => log_error.map(|e| {
            (
                format!("the runner could not write the gate log: {e}"),
                Fault::Runner,
            )
        }),
    }
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
    /// keeps what was read before it was, which is what the log has of it.
    tail: Tail,
    /// Its output was still open after the grace period: abandoned.
    output_held: bool,
    /// The first error writing the log.
    log_error: Option<String>,
}

/// Runs one command as the root of a process tree, inside the sandbox of
/// `paths`, its standard output and standard error on one pipe, copied whole
/// to `log` unless abandoned; once this returns, nothing more of it is
/// written there. A command that cannot be confined does not run.
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
    let mut command = match agent.confine_shell(line, paths) {
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
    let sink = Arc::new(Mutex::new(Some(log)));
    let (sender, receiver) = mpsc::channel();
    {
        let (tail, sink) = (Arc::clone(&tail), Arc::clone(&sink));
        thread::spawn(move || {
            // The receiver is gone when the output was abandoned.
            let _ = sender.send(drain(reader, &sink, &tail));
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
    // A drain that sent has written its last: nothing to close. One that did
    // not is abandoned, and its log is taken from it under the lock each of
    // its writes holds (OWL-69): the write it may have in progress lands
    // first, and none starts afterwards, so none can land after the status
    // line the caller writes next. The wait is for that one write, which a
    // stalled log would hold, as it would hold the status line. The drain
    // still reads, so the process holding the output is not stopped by a
    // closed pipe, and drops what it reads. Its first log write error, if
    // any, is lost with it: the gate fails on the held output anyway.
    let (log_error, output_held) = match receiver.recv_timeout(wait_for) {
        Ok(log_error) => (log_error, false),
        Err(_) => {
            lock(&sink).take();
            (None, true)
        }
    };
    // The tail stops growing with the log, so the failure carries what the
    // log has of the output, and nothing the drain reads later.
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

/// Reads the output to its end, keeping its tail in `tail` and copying it to
/// the log in `sink`, both shared with the caller: what was read survives
/// the drain being abandoned, and the caller abandons it by emptying the
/// sink. Returns the first log write error, which stops the copy, not the
/// reading.
///
/// Each chunk joins the tail and is written to the log under the sink's
/// lock, and only while the sink holds the log: emptying it cuts both at the
/// same chunk, and waits for a write in progress. The tail's own lock is
/// never held during a read or a write, so the tail can be taken while the
/// drain waits on either.
fn drain(
    mut reader: impl Read,
    sink: &Mutex<Option<impl Write>>,
    tail: &Mutex<Tail>,
) -> Option<String> {
    let mut log_error = None;
    let mut buffer = [0u8; 8192];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => {
                // The guard spans the write: bound here, not a temporary.
                let mut sink = lock(sink);
                let Some(log) = sink.as_mut() else {
                    continue;
                };
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

/// What `mutex` guards, even when a thread panicked holding it: the tail it
/// held is still the output, the log still the log, and the runner must not
/// panic in turn.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
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
    use std::process::Command;

    use super::*;

    /// The agent environment of these tests, bare: they check the gate's own
    /// rules; the sandbox's are checked below and in the test bench.
    fn agent() -> AgentEnv {
        AgentEnv::new(std::env::vars_os())
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
        gate_as(&agent(), commands, timeout)
    }

    fn gate_as(
        agent: &AgentEnv,
        commands: &[String],
        timeout: Duration,
    ) -> (Result<(), GateFailure>, String) {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join(GATE_LOG);
        let mut log = File::create(&log_path).unwrap();
        let outcome = run_commands(
            agent,
            &paths(dir.path()),
            (commands, Phase::Gate),
            timeout,
            Instant::now() + timeout,
            &mut log,
            &mut Vec::new(),
        )
        .map_err(|failed| failed.failure);
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
            // it inherits this process's output and working folder.
            let exe = std::env::current_exe().unwrap();
            drop(
                Command::new(exe)
                    .args([
                        "--exact",
                        "executor::gate::tests::helper_write_late",
                        "--ignored",
                        "--nocapture",
                    ])
                    .process_group(0)
                    .spawn()
                    .unwrap(),
            );
        }
    }

    /// Past the gate's wait, once the test creates `go` in the working
    /// folder, writes `LATE` and more output than a pipe holds, then creates
    /// `done`.
    #[cfg(unix)]
    #[test]
    #[ignore = "helper, run by the tests below"]
    fn helper_write_late() {
        if helper_requested() {
            wait_until("the test to say go", || Path::new("go").exists());
            // Written raw: libtest captures `print!`, not the handle.
            let mut out = io::stdout();
            out.write_all(b"LATE").unwrap();
            out.write_all(&vec![b'z'; 1024 * 1024]).unwrap();
            out.flush().unwrap();
            File::create("done").unwrap();
        }
    }

    /// Polls `ready` until it holds, failing past 30 s.
    fn wait_until(what: &str, ready: impl Fn() -> bool) {
        let waited = Instant::now();
        while !ready() {
            assert!(
                waited.elapsed() < Duration::from_secs(30),
                "timed out waiting for {what}"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// The output read before the drain was abandoned is kept: a process
    /// that left the tree holds the output past the wait, and the failure
    /// still carries what the command wrote. What that process writes later
    /// reaches no log line after the command's status, not even those of a
    /// command run after it (OWL-69). A gate stops at a held output, so that
    /// next command runs in a second call on the same log, as it would if
    /// the gate went on.
    #[cfg(unix)]
    #[test]
    fn an_abandoned_drain_keeps_what_it_read_and_writes_nothing_more() {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join(GATE_LOG);
        let mut log = File::create(&log_path).unwrap();
        // The wait lasts until the deadline, at least `OUTPUT_GRACE`: 5 s
        // leaves the helper time to start on a loaded machine.
        let timeout = Duration::from_secs(5);
        let outcome = run_commands(
            &agent(),
            &paths(dir.path()),
            (
                &[helper("helper_leave_an_escaped_pipe_holder")],
                Phase::Gate,
            ),
            timeout,
            Instant::now() + timeout,
            &mut log,
            &mut Vec::new(),
        );
        let failure = outcome.unwrap_err().failure;
        assert_eq!(
            failure.reason,
            "a process it started kept its output open after it ended"
        );
        assert!(failure.output.contains("BEFORE-THE-HOLD"), "{failure:?}");
        let timeout = Duration::from_secs(60);
        let next = run_commands(
            &agent(),
            &paths(dir.path()),
            (&lines(&["echo second"]), Phase::Gate),
            timeout,
            Instant::now() + timeout,
            &mut log,
            &mut Vec::new(),
        );
        assert_eq!(next, Ok(()));
        File::create(dir.path().join("go")).unwrap();
        // The drain reads in order, one chunk at a time, and the pipe holds
        // at most 64 KiB: the 1 MiB write ends only once the drain has read
        // well past the chunk holding `LATE`, so it has handled that chunk.
        wait_until("the late output to be read", || {
            dir.path().join("done").exists()
        });
        let log = std::fs::read_to_string(&log_path).unwrap();
        assert!(log.contains("BEFORE-THE-HOLD"), "{log}");
        assert!(!log.contains("LATE"), "{log}");
        let last: Vec<&str> = log.lines().rev().take(3).collect();
        assert_eq!(last[2], "$ echo second", "{log}");
        assert_eq!(last[1], "second", "{log}");
        assert!(last[0].starts_with('[') && log.ends_with("]\n"), "{log}");
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
            let sink = Mutex::new(Some(log));
            thread::spawn(move || drain(reader, &sink, &tail))
        };
        let mut output = vec![b'x'; OUTPUT_TAIL + 1024];
        output.extend_from_slice(b"END");
        writer.write_all(&output).unwrap();
        // The writer stays open: the drain ends up waiting in its next read.
        wait_until("the drain to publish the output", || {
            let kept: Vec<u8> = tail.lock().unwrap().bytes.iter().copied().collect();
            kept.ends_with(b"END")
        });
        let (text, truncated) = take_tail(&tail).text();
        assert!(truncated);
        assert_eq!(text.len(), OUTPUT_TAIL);
        assert!(text.ends_with("xEND"));
        drop(writer);
        assert_eq!(drained.join().unwrap(), None);
    }

    /// A log whose writes wait for the test's leave, recording what lands.
    struct HeldLog {
        entered: mpsc::Sender<()>,
        leave: mpsc::Receiver<()>,
        landed: Arc<Mutex<Vec<u8>>>,
    }

    impl Write for HeldLog {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            let _ = self.entered.send(());
            // Once the test dropped its sender, writes no longer wait.
            let _ = self.leave.recv();
            lock(&self.landed).extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Emptying the sink waits for the write in progress, and nothing the
    /// drain reads afterwards reaches the log or the tail (OWL-69): the lock
    /// is held across the write, not only checked before it.
    #[test]
    fn an_abandoned_drain_writes_nothing_more_to_the_log() {
        let (reader, mut writer) = io::pipe().unwrap();
        let (entered, write_entered) = mpsc::channel();
        let (leave, left) = mpsc::channel();
        let landed = Arc::new(Mutex::new(Vec::new()));
        let log = HeldLog {
            entered,
            leave: left,
            landed: Arc::clone(&landed),
        };
        let tail = Arc::new(Mutex::new(Tail::default()));
        let sink = Arc::new(Mutex::new(Some(log)));
        let drained = {
            let (tail, sink) = (Arc::clone(&tail), Arc::clone(&sink));
            thread::spawn(move || drain(reader, &*sink, &tail))
        };
        writer.write_all(b"EARLY").unwrap();
        write_entered.recv().unwrap();
        // The drain is inside the log write, holding the sink.
        assert!(sink.try_lock().is_err(), "the write does not hold the sink");
        let abandoned = {
            let sink = Arc::clone(&sink);
            thread::spawn(move || drop(lock(&sink).take()))
        };
        leave.send(()).unwrap();
        drop(leave);
        abandoned.join().unwrap();
        assert_eq!(*lock(&landed), b"EARLY", "the write in progress landed");
        writer.write_all(b"LATE").unwrap();
        drop(writer);
        assert_eq!(drained.join().unwrap(), None);
        assert_eq!(*lock(&landed), b"EARLY", "nothing lands after");
        assert_eq!(take_tail(&tail).text(), ("EARLY".to_owned(), false));
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

    /// `cmd.exe` gets the line verbatim, inner quotes, doubled spaces and
    /// backslashes before a quote included.
    #[cfg(windows)]
    #[test]
    fn cmd_keeps_the_line_verbatim() {
        let echoed = r#""a  b" \"q\" "c:\x\""#;
        let (outcome, log) = gate(
            &[format!("echo {echoed} && exit 5")],
            Duration::from_secs(60),
        );
        assert_eq!(outcome.unwrap_err().reason, "exit status 5");
        assert!(log.contains(echoed), "{log}");
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
            (&[line.to_owned()], Phase::Gate),
            timeout,
            Instant::now() + timeout,
            &mut log,
            &mut Vec::new(),
        )
        .map_err(|failed| failed.failure)
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
        let confined = AgentEnv::new(parent).unwrap();
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
        let confined = AgentEnv::new(std::env::vars_os()).unwrap();
        let failure = gate_with(&confined, &paths(dir.path()), "echo hi").unwrap_err();
        assert!(failure.reason.contains("WSL2"), "{}", failure.reason);
    }

    /// OWL-73: the drivers named in `git config -z --get-regexp` records,
    /// with or without a value, each once and case kept; a name `-c` cannot
    /// carry is refused.
    #[test]
    fn filter_drivers_are_read_from_the_config_listing() {
        let records = b"filter.Owl.clean\ntouch x; cat\0filter.Owl.required\0\
            filter.a.b.process\nf\0filter.clean\nx\0filter.owl.smudge\ny\0";
        assert_eq!(filter_drivers(records).unwrap(), ["Owl", "a.b", "owl"]);
        assert_eq!(filter_drivers(b"").unwrap(), Vec::<String>::new());
        for (records, name) in [
            (&b"filter.a=b.clean\nv\0"[..], "a=b"),
            (b"filter..clean\nv\0", ""),
            (b"filter.\xff.clean\nv\0", "\u{fffd}"),
        ] {
            assert_eq!(filter_drivers(records).unwrap_err(), name);
        }
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
        pub(super) struct Repo {
            pub(super) base: tempfile::TempDir,
            pub(super) worktree: PathBuf,
            pub(super) marker: PathBuf,
            pub(super) snapshot: Snapshot,
        }

        /// The test's own git, away from the operator's configuration.
        pub(super) fn git_in(dir: &Path, args: &[&str]) -> Vec<u8> {
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

        pub(super) fn repo() -> Repo {
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
                self.gate_as(&agent(), command)
            }

            fn gate_as(&self, agent: &AgentEnv, command: &str) -> GateReport {
                run(
                    &Git::new("git"),
                    agent,
                    &paths(&self.worktree),
                    &[],
                    &[command.to_owned()],
                    Duration::from_secs(60),
                    &self.base.path().join(GATE_LOG),
                    &|| self.snapshot.check_files(&self.worktree),
                )
            }

            /// A command that has git touch the marker, for `filter.owl.clean`.
            pub(super) fn touch_marker(&self) -> String {
                format!("touch '{}'; cat", self.marker.display())
            }

            fn log(&self) -> String {
                std::fs::read_to_string(self.base.path().join(GATE_LOG)).unwrap()
            }

            /// Sets `key` in the repository's shared configuration, as it
            /// was before the run: the snapshot is taken again.
            fn configure(&mut self, key: &str, value: &str) {
                let main = self.worktree.with_file_name("main");
                git_in(&main, &["config", key, value]);
                self.snapshot =
                    Snapshot::take(&Git::new("git"), &main, &self.worktree, "owl-1").unwrap();
            }

            /// Gives `a.txt` and `b.txt` the time `stamp`, their size kept,
            /// so that git must read them again.
            fn touch(&self, stamp: &str) {
                let touched = Command::new("touch")
                    .args(["-t", stamp, "a.txt", "b.txt"])
                    .current_dir(&self.worktree)
                    .status()
                    .unwrap();
                assert!(touched.success());
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

        /// OWL-73, the acceptance: attributes naming a driver the repository
        /// had and one the user's configuration holds make neither run in
        /// the gate's git, before the commands or after them. Their files
        /// keep their content, so a gate that changes nothing else passes.
        #[test]
        fn a_configured_filter_named_by_the_attributes_never_runs() {
            let mut repo = repo();
            // The build run's commit: `b.txt`, and attributes naming both
            // drivers, committed before any driver exists.
            let worktree = &repo.worktree;
            std::fs::write(worktree.join("b.txt"), "b\n").unwrap();
            std::fs::write(
                worktree.join(".gitattributes"),
                "a.txt filter=owl\nb.txt filter=home\n",
            )
            .unwrap();
            git_in(worktree, &["add", "b.txt", ".gitattributes"]);
            git_in(worktree, &["commit", "-q", "-m", "build"]);
            repo.configure("filter.owl.clean", &repo.touch_marker());
            let home = repo.worktree.with_file_name("home");
            let home_marker = home.join("marker");
            std::fs::create_dir(&home).unwrap();
            std::fs::write(
                home.join(".gitconfig"),
                format!(
                    "[filter \"home\"]\n\tclean = \"touch '{}'; cat\"\n",
                    home_marker.display()
                ),
            )
            .unwrap();
            let agent = AgentEnv::new(
                std::env::vars_os()
                    .filter(|(name, _)| name != "HOME")
                    .chain([("HOME".into(), home.clone().into_os_string())]),
            )
            .unwrap()
            .without_confinement();
            // The snapshot's own git may have run the repository's driver.
            let _ = std::fs::remove_file(&repo.marker);
            let ran = || repo.marker.exists() || home_marker.exists();

            repo.touch("200001010000");
            let report = repo.gate_as(&agent, "true");
            assert!(report.passed(), "{report:?}");
            assert!(!ran(), "the gate's git ran a driver");

            let report = repo.gate_as(
                &agent,
                "printf 'a.txt filter=home\\nb.txt filter=owl\\n' > .gitattributes && \
                 touch -t 200001020000 a.txt b.txt",
            );
            assert!(report.breach.is_empty(), "{report:?}");
            let failure = report.failure.as_ref().expect("the gate failed");
            assert!(failure.command.is_none(), "{report:?}");
            // The machine's own drivers, such as a system git-lfs, are
            // switched off and named too.
            let reason = &failure.reason;
            let named = reason
                .strip_prefix(
                    "the gate changed files: .gitattributes (read with the filter drivers ",
                )
                .and_then(|rest| rest.split_once(" switched off"))
                .map(|(drivers, _)| drivers.split(", ").collect::<Vec<_>>())
                .unwrap_or_default();
            assert!(
                named.contains(&"home") && named.contains(&"owl"),
                "{reason}"
            );
            assert!(!ran(), "the gate's git ran a driver");

            // The control: the user's own git, with that home.
            let control = Command::new("git")
                .args(["status", "--porcelain"])
                .current_dir(&repo.worktree)
                .env("HOME", &home)
                .env("XDG_CONFIG_HOME", home.join("xdg"))
                .env_remove("GIT_CONFIG_GLOBAL")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .output()
                .unwrap();
            assert!(control.status.success(), "{control:?}");
            assert!(
                repo.marker.exists() && home_marker.exists(),
                "the control never ran both drivers"
            );
        }

        /// A configured driver whose name a `-c` argument cannot carry stops
        /// the gate before any command runs.
        #[test]
        fn a_driver_that_cannot_be_switched_off_stops_the_gate() {
            let mut repo = repo();
            repo.configure("filter.a=b.clean", "cat");
            let report = repo.gate(&format!("touch '{}'", repo.marker.display()));
            assert!(report.breach.is_empty(), "{report:?}");
            let failure = report.failure.as_ref().expect("the gate failed");
            assert!(
                failure.command.is_none() && failure.reason.contains("named \"a=b\""),
                "{report:?}"
            );
            assert!(!repo.marker.exists(), "a gate command ran");
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

    /// OWL-330: whose a failure is, from how the command ended.
    #[cfg(unix)]
    #[test]
    fn a_failure_is_the_commands_only_with_nothing_wrong_on_the_runners_side() {
        use std::os::unix::process::ExitStatusExt;
        let stopped = || "stopped".to_owned();
        let ends = || {
            [
                ("exit 0", End::Exited(ExitStatus::from_raw(0))),
                ("exit 1", End::Exited(ExitStatus::from_raw(1 << 8))),
                ("signal", End::Exited(ExitStatus::from_raw(9))),
                ("deadline", End::TimedOut),
                ("error", End::Error("could not start: no".to_owned())),
            ]
        };
        for held in [false, true] {
            for log_error in [None, Some("disk full".to_owned())] {
                for (name, end) in ends() {
                    let classified = classify(end, held, log_error.clone(), stopped);
                    let runner_side = held || log_error.is_some();
                    let expected = match name {
                        "exit 0" if held => Some((
                            "a process it started kept its output open after it ended",
                            Fault::Runner,
                        )),
                        "exit 0" if log_error.is_some() => Some((
                            "the runner could not write the gate log: disk full",
                            Fault::Runner,
                        )),
                        "exit 0" => None,
                        "error" => Some(("could not start: no", Fault::Runner)),
                        _ => {
                            let reason = match name {
                                "exit 1" => "exit status 1",
                                "signal" => "ended by a signal",
                                _ => "stopped",
                            };
                            let fault = if runner_side {
                                Fault::Runner
                            } else {
                                Fault::Command
                            };
                            Some((reason, fault))
                        }
                    };
                    assert_eq!(
                        classified,
                        expected.map(|(reason, fault)| (reason.to_owned(), fault)),
                        "{name}, held {held}, log error {log_error:?}"
                    );
                }
            }
        }
    }

    /// OWL-330: a red setup lets the harness start only on a worktree
    /// holding the Build's own work, and only for the command's own fault.
    #[test]
    fn only_the_commands_own_failure_on_unfinished_work_is_recoverable() {
        for resumed in [false, true] {
            for fault in [None, Some(Fault::Command), Some(Fault::Runner)] {
                let run = SetupRun {
                    report: empty_report(&[], Path::new(SETUP_LOG)),
                    resumed,
                    fault,
                };
                assert_eq!(
                    run.recoverable(),
                    resumed && fault == Some(Fault::Command),
                    "resumed {resumed}, fault {fault:?}"
                );
            }
        }
    }

    /// Runs `commands` as setup commands, unconfined, under `timeout`: the
    /// failure and its fault, and the log.
    #[cfg(unix)]
    fn set_up(commands: &[String], timeout: Duration) -> (Result<(), CommandFailure>, String) {
        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join(GATE_LOG);
        let mut log = File::create(&log_path).unwrap();
        let outcome = run_commands(
            &agent(),
            &paths(dir.path()),
            (commands, Phase::Setup),
            timeout,
            Instant::now() + timeout,
            &mut log,
            &mut Vec::new(),
        );
        // What a helper left behind may wait for this, then stops.
        File::create(dir.path().join("go")).unwrap();
        (outcome, std::fs::read_to_string(&log_path).unwrap())
    }

    /// OWL-330: real commands. An exit status, a signal and the deadline are
    /// the command's; the same with a process left holding the output is the
    /// runner's, the reason unchanged; and so is a log the runner cannot
    /// write, here its `$ command` line to a log opened read-only. A runner
    /// error writing the output to the log has no portable trigger: the
    /// `classify` table covers it.
    #[cfg(unix)]
    #[test]
    fn a_real_command_failure_says_whose_it_is() {
        let fault = |commands: &[String], timeout: u64| {
            let (outcome, _) = set_up(commands, Duration::from_secs(timeout));
            let failed = outcome.unwrap_err();
            (failed.failure.reason, failed.fault)
        };
        assert_eq!(
            fault(&lines(&["exit 1"]), 60),
            ("exit status 1".to_owned(), Fault::Command)
        );
        assert_eq!(
            fault(&lines(&["kill -9 $$"]), 60),
            ("ended by a signal".to_owned(), Fault::Command)
        );
        assert_eq!(
            fault(&lines(&["sleep 20"]), 1),
            (
                "stopped at the setup's deadline, a third of the gate's, 1 s".to_owned(),
                Fault::Command
            )
        );
        let holder = helper("helper_leave_an_escaped_pipe_holder");
        assert_eq!(
            fault(&[format!("{holder}; exit 1")], 5),
            ("exit status 1".to_owned(), Fault::Runner)
        );
        assert_eq!(
            fault(&[format!("{holder}; sleep 20")], 3),
            (
                "stopped at the setup's deadline, a third of the gate's, 3 s".to_owned(),
                Fault::Runner
            )
        );

        let dir = tempfile::tempdir().unwrap();
        let log_path = dir.path().join(GATE_LOG);
        File::create(&log_path).unwrap();
        let mut read_only = File::open(&log_path).unwrap();
        let failed = run_commands(
            &agent(),
            &paths(dir.path()),
            (&lines(&["true"]), Phase::Setup),
            Duration::from_secs(60),
            Instant::now() + Duration::from_secs(60),
            &mut read_only,
            &mut Vec::new(),
        )
        .unwrap_err();
        assert!(
            failed
                .failure
                .reason
                .starts_with("the runner could not write the gate log"),
            "{failed:?}"
        );
        assert_eq!(failed.fault, Fault::Runner);
    }

    /// OWL-330: the project's setup in the gate, before its commands.
    #[cfg(unix)]
    mod setup {
        use super::planted::{Repo, git_in, repo};
        use super::*;

        impl Repo {
            /// The gate with `setup` before `commands`, the run's own
            /// temporary folder given, within `timeout`.
            fn gate_set_up(
                &self,
                setup: &[&str],
                commands: &[&str],
                timeout: Duration,
            ) -> GateReport {
                let temp = tempfile::tempdir().unwrap();
                run(
                    &Git::new("git"),
                    &agent(),
                    &RunPaths {
                        temp: Some(temp.path().to_owned()),
                        ..paths(&self.worktree)
                    },
                    &lines(setup),
                    &lines(commands),
                    timeout,
                    &self.base.path().join(GATE_LOG),
                    &|| self.snapshot.check_files(&self.worktree),
                )
            }

            /// The setup before a harness, with `base` as the run's base.
            fn before_harness(&self, base: &str, setup: &[&str]) -> SetupRun {
                let temp = tempfile::tempdir().unwrap();
                run_setup(
                    &Git::new("git"),
                    &agent(),
                    &RunPaths {
                        temp: Some(temp.path().to_owned()),
                        ..paths(&self.worktree)
                    },
                    base,
                    &lines(setup),
                    Duration::from_secs(180),
                    &self.base.path().join(SETUP_LOG),
                    &|| self.snapshot.check_files(&self.worktree),
                )
            }

            fn head(&self) -> String {
                self.git(&["rev-parse", "HEAD"])
            }

            fn git(&self, args: &[&str]) -> String {
                String::from_utf8(git_in(&self.worktree, args))
                    .unwrap()
                    .trim()
                    .to_owned()
            }

            fn write(&self, path: &str, content: &str) {
                let path = self.worktree.join(path);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(path, content).unwrap();
            }

            fn read(&self, path: &str) -> String {
                std::fs::read_to_string(self.worktree.join(path)).unwrap()
            }

            /// Commits on the branch the checkout has, `main`'s, so the
            /// worktree's base moves ahead of it.
            fn advance_main(&self) -> String {
                let main = self.worktree.with_file_name("main");
                git_in(&main, &["commit", "-q", "--allow-empty", "-m", "ahead"]);
                String::from_utf8(git_in(&main, &["rev-parse", "HEAD"]))
                    .unwrap()
                    .trim()
                    .to_owned()
            }
        }

        fn reason(report: &GateReport) -> &str {
            &report.failure.as_ref().expect("the setup failed").reason
        }

        /// OWL-421: only the refusal before any command is one the next run
        /// can clean up untold of a command's output; a file left by the
        /// setup or the gate, or a red command, is not.
        #[test]
        fn only_uncommitted_changes_before_any_command_are_refused_uncommitted() {
            let refused = |report: &GateReport| {
                refused_uncommitted(report.failure.as_ref().expect("the gate failed"))
            };
            let dirty = repo();
            dirty.write(".pnpm-store/v10/index", "x");
            let touch = format!("touch '{}'", dirty.marker.display());
            let report = dirty.gate_set_up(&[], &[&touch], Duration::from_secs(60));
            assert!(!dirty.marker.exists(), "a gate command ran");
            assert!(refused(&report), "{report:?}");

            for (setup, commands) in [
                (&["echo x > stray.txt"][..], &["true"][..]),
                (&[][..], &["echo x > stray.txt"][..]),
                (&[][..], &["exit 1"][..]),
            ] {
                let report = repo().gate_set_up(setup, commands, Duration::from_secs(60));
                assert!(!refused(&report), "{report:?}");
            }
        }

        #[test]
        fn the_setup_runs_first_and_its_temporary_files_reach_the_gate() {
            let repo = repo();
            let report = repo.gate_set_up(
                &["echo a > \"$TMPDIR/x\""],
                &["test \"$(cat \"$TMPDIR/x\")\" = a"],
                Duration::from_secs(60),
            );
            assert!(report.passed(), "{report:?}");
            assert_eq!(report.setup.len(), 1);
            assert_eq!(report.steps.len(), 1);
            let log = std::fs::read_to_string(&report.log).unwrap();
            let setup = log.find("$ echo a").unwrap();
            assert!(setup < log.find("$ test").unwrap(), "{log}");
        }

        #[test]
        fn a_failing_setup_command_stops_the_gate_before_its_commands() {
            let repo = repo();
            let touch = format!("touch '{}'", repo.marker.display());
            let report = repo.gate_set_up(
                &["echo partial; exit 1", "echo never"],
                &[&touch],
                Duration::from_secs(60),
            );
            assert!(!repo.marker.exists(), "a gate command ran");
            let failure = report.failure.clone().unwrap();
            assert_eq!(failure.command.as_deref(), Some("echo partial; exit 1"));
            assert_eq!(failure.reason, "setup: exit status 1");
            assert!(failure.output.contains("partial"), "{failure:?}");
            assert_eq!(report.setup.len(), 1);
            assert!(report.steps.is_empty());
        }

        #[test]
        fn a_setup_that_leaves_a_file_or_moves_head_fails_the_gate() {
            let repo = repo();
            let report =
                repo.gate_set_up(&["echo x > stray.txt"], &["true"], Duration::from_secs(60));
            assert!(
                reason(&report).starts_with("the setup changed files: stray.txt"),
                "{report:?}"
            );
            assert!(report.steps.is_empty());

            let repo = super::planted::repo();
            let report = repo.gate_set_up(
                &[
                    "git -c user.name=o -c user.email=o@example.invalid -c commit.gpgsign=false \
                   commit -q --allow-empty -m moved",
                ],
                &["true"],
                Duration::from_secs(60),
            );
            assert!(
                reason(&report).starts_with("the setup moved HEAD from "),
                "{report:?}"
            );
        }

        /// The setup has a third of the gate's deadline, in its own words;
        /// the gate's commands get the whole of it afterwards.
        #[test]
        fn the_setup_has_a_third_of_the_deadline_and_the_gate_its_whole() {
            let repo = repo();
            let started = Instant::now();
            let report = repo.gate_set_up(&["sleep 20"], &["true"], Duration::from_secs(3));
            assert_eq!(
                reason(&report),
                "setup: stopped at the setup's deadline, a third of the gate's, 1 s"
            );
            assert!(started.elapsed() < Duration::from_secs(10));

            // 2 s of setup and 7.5 s of gate: past the 9 s they would share.
            let report = repo.gate_set_up(&["sleep 2"], &["sleep 7.5"], Duration::from_secs(9));
            assert!(report.passed(), "{report:?}");
        }

        /// Unfinished work, tracked, staged and untracked, comes out of a
        /// setup as it went in, and the run counts as resumed.
        #[test]
        fn unfinished_work_survives_the_setup_before_a_harness() {
            let repo = repo();
            let base = repo.head();
            repo.write("a.txt", "edited\n");
            repo.write("b.txt", "staged\n");
            repo.git(&["add", "b.txt"]);
            repo.write("c.txt", "untracked\n");
            let run = repo.before_harness(&base, &["echo set up > \"$TMPDIR/x\""]);
            assert!(run.report.passed(), "{run:?}");
            assert!(run.resumed && run.fault.is_none() && !run.recoverable());
            assert_eq!(run.report.setup.len(), 1);
            assert_eq!(
                (repo.read("a.txt"), repo.read("b.txt"), repo.read("c.txt")),
                ("edited\n".into(), "staged\n".into(), "untracked\n".into())
            );
            assert!(run.report.log.ends_with(SETUP_LOG));
        }

        /// Resumed or pristine: by HEAD against the run's base, ancestry and
        /// not equality, and by any entry status lists.
        #[test]
        fn resumed_is_read_from_head_and_the_base() {
            let repo = repo();
            let base = repo.head();
            assert!(!repo.before_harness(&base, &["true"]).resumed);
            // The base moved ahead: a branch with no commits of its own.
            let ahead = repo.advance_main();
            assert!(!repo.before_harness(&ahead, &["true"]).resumed);
            // The branch has a commit of its own: past the base, or beside it.
            repo.git(&["commit", "-q", "--allow-empty", "-m", "own"]);
            assert!(repo.before_harness(&base, &["true"]).resumed);
            assert!(repo.before_harness(&ahead, &["true"]).resumed);

            let repo = super::planted::repo();
            let base = repo.head();
            repo.write("u.txt", "u\n");
            assert!(repo.before_harness(&base, &["true"]).resumed);

            let run = repo.before_harness(&"0".repeat(40), &["true"]);
            let failure = run.report.failure.clone().expect("an unknown base fails");
            assert!(failure.command.is_none(), "{failure:?}");
            assert!(run.fault.is_none() && !run.recoverable());
            assert!(run.report.setup.is_empty(), "no command ran");
        }

        /// A red command on unfinished work is the command's, recoverable;
        /// on a pristine worktree it is not.
        #[test]
        fn a_red_setup_is_recoverable_only_on_unfinished_work() {
            let repo = repo();
            let base = repo.head();
            let run = repo.before_harness(&base, &["exit 1"]);
            assert_eq!(reason(&run.report), "setup: exit status 1");
            assert_eq!(run.fault, Some(Fault::Command));
            assert!(!run.resumed && !run.recoverable());
            repo.write("a.txt", "edited\n");
            let run = repo.before_harness(&base, &["exit 1"]);
            assert!(run.resumed && run.recoverable(), "{run:?}");
        }

        /// Each change a setup makes to unfinished work fails it, naming the
        /// path: a tracked edit changed, an untracked file deleted, a file
        /// added, a deleted file restored, the index alone, the mode alone,
        /// a conflict's stages. Ignored files and a deletion left as it was
        /// pass.
        #[test]
        fn a_setup_that_changes_unfinished_work_fails_naming_it() {
            let changed = |prepare: &dyn Fn(&Repo), setup: &str| {
                let repo = repo();
                let base = repo.head();
                prepare(&repo);
                let run = repo.before_harness(&base, &[setup]);
                assert!(run.fault.is_none() && !run.recoverable(), "{run:?}");
                reason(&run.report).to_owned()
            };
            let edited = |repo: &Repo| {
                repo.write("a.txt", "edited\n");
                repo.write("u.txt", "untracked\n");
            };
            assert_eq!(
                changed(&edited, "echo again > a.txt"),
                "the setup changed unfinished work: a.txt"
            );
            assert_eq!(
                changed(&edited, "rm u.txt"),
                "the setup changed unfinished work: u.txt"
            );
            assert_eq!(
                changed(&edited, "echo new > new.txt"),
                "the setup changed unfinished work: new.txt"
            );
            // A red command that also changed the work fails for the change.
            assert_eq!(
                changed(&edited, "echo again > a.txt; exit 1"),
                "the setup changed unfinished work: a.txt"
            );
            let deleted = |repo: &Repo| std::fs::remove_file(repo.worktree.join("a.txt")).unwrap();
            assert_eq!(
                changed(&deleted, "echo a > a.txt"),
                "the setup changed unfinished work: a.txt"
            );
            let staged = |repo: &Repo| {
                repo.write("a.txt", "staged\n");
                repo.git(&["add", "a.txt"]);
            };
            assert_eq!(
                changed(
                    &staged,
                    "git update-index --cacheinfo \
                     \"100644,$(echo other | git hash-object -w --stdin),a.txt\""
                ),
                "the setup changed unfinished work: a.txt"
            );
            assert_eq!(
                changed(&edited, "chmod +x a.txt"),
                "the setup changed unfinished work: a.txt"
            );
            let conflicted = |repo: &Repo| {
                let blob = repo.git(&["rev-parse", "HEAD:a.txt"]);
                let stages = format!("100644 {blob} 1\tc.txt\n100644 {blob} 2\tc.txt\n");
                let mut command = Command::new("git");
                command
                    .args(["update-index", "--index-info"])
                    .current_dir(&repo.worktree)
                    .stdin(Stdio::piped());
                let mut child = command.spawn().unwrap();
                child
                    .stdin
                    .take()
                    .unwrap()
                    .write_all(stages.as_bytes())
                    .unwrap();
                assert!(child.wait().unwrap().success());
            };
            assert_eq!(
                changed(
                    &conflicted,
                    "printf '100644 %s 3\\tc.txt\\n' \"$(git rev-parse HEAD:a.txt)\" | \
                     git update-index --index-info"
                ),
                "the setup changed unfinished work: c.txt"
            );

            let repo = repo();
            let base = repo.head();
            repo.write(".gitignore", "*.log\n");
            std::fs::remove_file(repo.worktree.join("a.txt")).unwrap();
            let run = repo.before_harness(&base, &["echo log > x.log"]);
            assert!(run.report.passed(), "{run:?}");
        }

        /// Every path counts, past the ten a failure lists.
        #[test]
        fn the_last_of_many_unfinished_paths_is_checked() {
            let repo = repo();
            let base = repo.head();
            for i in 1..=12 {
                repo.write(&format!("u{i:02}.txt"), "u\n");
            }
            let run = repo.before_harness(&base, &["echo changed > u12.txt"]);
            assert_eq!(
                reason(&run.report),
                "the setup changed unfinished work: u12.txt"
            );
        }

        /// A folder of tracked files swapped for a link to a folder outside
        /// the worktree: the state read follows no link, so what changes
        /// behind it is not read and does not count.
        #[test]
        fn the_state_read_follows_no_link_out_of_the_worktree() {
            let repo = repo();
            let base = repo.head();
            repo.write("dir/f", "tracked\n");
            repo.git(&["add", "dir/f"]);
            repo.git(&["commit", "-q", "-m", "dir"]);
            let elsewhere = repo.base.path().join("elsewhere");
            std::fs::create_dir(&elsewhere).unwrap();
            std::fs::write(elsewhere.join("f"), "outside\n").unwrap();
            std::fs::remove_dir_all(repo.worktree.join("dir")).unwrap();
            std::os::unix::fs::symlink(&elsewhere, repo.worktree.join("dir")).unwrap();
            let run = repo.before_harness(
                &base,
                &[&format!("echo changed > '{}/f'", elsewhere.display())],
            );
            assert!(run.report.passed(), "{run:?}");
            assert!(run.resumed);
        }

        /// The planted submodule of OWL-64, planted by a setup: neither state
        /// read looks into it, so its filter never runs.
        #[test]
        fn a_submodule_planted_by_the_setup_is_not_looked_into() {
            let repo = repo();
            let base = repo.head();
            let plant = format!(
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
            );
            let run = repo.before_harness(&base, &[&plant]);
            assert!(!run.report.passed());
            assert!(!repo.marker.exists(), "the state read ran the filter");
            git_in(&repo.worktree, &["status", "--porcelain"]);
            assert!(repo.marker.exists(), "the control never ran the filter");
        }

        /// A path that is not UTF-8 is read by its bytes: a setup that edits
        /// it is caught. Linux only: APFS refuses such names.
        #[cfg(target_os = "linux")]
        #[test]
        fn a_path_that_is_not_utf8_is_read_by_its_bytes() {
            use std::os::unix::ffi::OsStrExt;
            let repo = repo();
            let base = repo.head();
            let name = std::ffi::OsStr::from_bytes(b"odd-\xff.txt");
            std::fs::write(repo.worktree.join(name), "u\n").unwrap();
            let run = repo.before_harness(&base, &["for f in odd-*; do echo x > \"$f\"; done"]);
            assert!(
                reason(&run.report).starts_with("the setup changed unfinished work: odd-"),
                "{run:?}"
            );
        }
    }
}
