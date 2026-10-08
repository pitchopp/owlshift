//! The executor: one role turned into one safe, verifiable run
//! (architecture, section 5 step 3 and section 8).
//!
//! [`Executor::run`] prepares the ticket's worktree and its brief, checks
//! that no credential is within the agent's reach, snapshots what the run
//! must leave alone, spawns the harness as the root of a process tree with
//! the agent environment, streams its output to the run's log files, stops
//! the whole tree at the deadline and after the run, then decides the
//! outcome:
//!
//! - an isolation violation quarantines the run, whatever else happened
//!   ([`isolation`]). The check starts with the worktree's `.git` link and
//!   the repository's shared git files, read from the file system, so a run
//!   that redirected its link or planted a command there is quarantined
//!   before the runner's git runs anywhere;
//! - a run is [`Outcome::Finished`] only when the harness reports it
//!   completed *and* the role left a valid `result.json`: an exit status is
//!   never taken as proof;
//! - a usage limit is an interruption to resume, not a failure;
//! - everything else is a [`Failure`] with its reason.
//!
//! What goes wrong before the spawn (the worktree, the brief, a credential
//! finding, the first snapshot, the command) is an [`ExecutorError`]:
//! nothing ran.
//!
//! A Build run that finishes with `done` is not taken at its word either:
//! the executor then runs the project's gate itself ([`gate`], OWL-16), and
//! checks isolation again after it. A red gate makes the run
//! [`Failure::Gate`]; a gate that breaks isolation quarantines it. The gate
//! makes the file-system checks itself before its own git runs, and stops
//! there on a violation (OWL-64).
//!
//! # Layout
//!
//! The run's own files live in the worktree under [`RUN_DIR`], which a
//! `.gitignore` of `*` keeps out of git in place, without touching any git
//! state shared with other worktrees: the brief ([`BRIEF_PATH`]), the result
//! ([`RESULT_PATH`]) and, for the build role, its plan and ledger. The
//! executor never writes there through a symbolic link. The log files and a
//! copy of the brief go to the caller's run directory, outside the worktree.

pub mod gate;
mod git;
pub mod harness;
pub mod isolation;
mod watch;
mod worktree;

use std::fmt;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use jiff::Timestamp;

use owlshift_adapters::harness::claude::Usage;
use owlshift_contracts::Role;
use owlshift_contracts::brief::{Brief, GateFailure};
use owlshift_contracts::ids::RelativePath;
use owlshift_contracts::result::{self, DecisionsRefusal, RunResult};
use owlshift_platform::confined::{ConfinedError, Refusal, read_confined};
use owlshift_platform::keychain::Secret;

use crate::agent_env::{AgentEnv, CredentialFinding, RunPaths};
use crate::artifact::{ArtifactContents, ArtifactError, MAX_ARTIFACT_BYTES, read_artifacts};

pub use gate::{DEFAULT_GATE_TIMEOUT, GateReport};
pub use git::{Git, GitError, MINIMUM_GIT_VERSION};
pub use harness::{
    Harness, HarnessEnd, HarnessError, HarnessLogin, HarnessRun, HarnessStatus, SandboxNeeds,
};
pub use isolation::{Violation, check_worktree_link};

/// The directory of the run's own files, relative to the worktree.
pub const RUN_DIR: &str = ".owlshift/run";

/// Where the executor writes the brief, relative to the worktree.
pub const BRIEF_PATH: &str = ".owlshift/run/brief.json";

/// Where the role writes `result.json`, relative to the worktree: the
/// brief's `result_path`.
pub const RESULT_PATH: &str = ".owlshift/run/result.json";

/// The most bytes `result.json` may hold, as for an artifact.
pub const MAX_RESULT_BYTES: u64 = MAX_ARTIFACT_BYTES;

/// The harness's configuration folder of one run, in the run's own
/// temporary folder, when the harness logs in with a token (OWL-94).
pub const HARNESS_CONFIG_DIR: &str = "harness-config";

/// The harness's temporary folder of one run, in the run's own temporary
/// folder, when the harness names one (OWL-100). The harness makes it.
pub const HARNESS_TEMP_DIR: &str = "harness-tmp";

/// Runs roles. One value serves every run of a runner.
#[derive(Clone, Debug)]
pub struct Executor {
    /// The runner's own git, for the worktree and the isolation check. It
    /// runs no hook and no file-system monitor ([`Git`]).
    pub git: Git,
    /// The environment every agent gets. The caller builds it with
    /// [`AgentEnv::from_runner`] and the variables the project declares for
    /// its gate.
    pub agent: AgentEnv,
    /// The forge hosts the credential check asks git and gh about, such as
    /// `github.com`.
    pub forge_hosts: Vec<String>,
    /// How long a run may take before its whole process tree is stopped.
    pub timeout: Duration,
    /// How long the project's gate commands may take together, after a
    /// Build `done`; [`DEFAULT_GATE_TIMEOUT`] unless the caller has a reason.
    pub gate_timeout: Duration,
}

/// One run: where, on which branch, and with which brief.
#[derive(Clone, Copy, Debug)]
pub struct RunSpec<'a> {
    /// The checkout the worktree belongs to, which the run must leave
    /// untouched. A kept worktree is reused only while its `.git` links it
    /// to this checkout's repository. The isolation check assumes nobody
    /// else changes it during the run: a person editing or committing there
    /// makes the run look
    /// like a breach.
    pub main: &'a Path,
    /// The ticket's worktree, an absolute path: created at the first run,
    /// reused afterwards.
    pub worktree: &'a Path,
    /// The ticket's branch, checked out in the worktree.
    pub branch: &'a str,
    /// What a new branch starts from, read only when the branch is created:
    /// a full commit id, or a remote-tracking name such as `origin/main`,
    /// looked up as exactly `refs/remotes/<name>` so a local branch named
    /// like it cannot stand in for it (OWL-66). An abbreviated id, a local
    /// branch name and a revision expression such as `origin/main~1` are
    /// refused. The isolation check leaves remote-tracking
    /// refs out, so an earlier run can move `origin/main`: `owlshift do`
    /// passes the commit it resolved right after its fetch (OWL-51).
    pub base: &'a str,
    /// Where the run's log files and a copy of its brief go, outside the
    /// worktree; created if missing.
    pub run_dir: &'a Path,
    /// The brief; its `result_path` is replaced with [`RESULT_PATH`].
    pub brief: &'a Brief,
}

/// What a run came to.
#[derive(Debug)]
pub struct RunReport {
    pub outcome: Outcome,
    /// The harness's exit code, for the record only; `None` when a signal
    /// ended it or it never reported one.
    pub exit_code: Option<i32>,
    /// Usage, when the harness reports it.
    pub usage: Option<Usage>,
    /// The model, as the harness resolved it.
    pub model: Option<String>,
    /// The harness's version, to record with every run.
    pub harness_version: Option<String>,
    pub elapsed: Duration,
    /// The harness's standard output, as it came.
    pub stdout_log: PathBuf,
    /// The harness's standard error.
    pub stderr_log: PathBuf,
    /// The first error writing a log file: the run went on without it.
    pub log_error: Option<String>,
    /// The project's gate, when the executor ran it: after a Build `done`
    /// that broke no isolation. Its commands and commit fill the delivery
    /// report's gate when it passed.
    pub gate: Option<GateReport>,
}

/// How a run ended.
#[derive(Debug)]
pub enum Outcome {
    /// The harness completed and the role left a valid `result.json`, with
    /// the artifacts it names.
    Finished {
        result: Box<RunResult>,
        artifacts: ArtifactContents,
    },
    /// The harness reached its usage limit: an interruption, resumed after
    /// the reset.
    UsageLimit { resets_at: Option<Timestamp> },
    /// No usable result.
    Failed(Failure),
    /// The run broke isolation: its work cannot be trusted, and the ticket
    /// parks until a person looks.
    Quarantined(Vec<Violation>),
}

/// Why a run failed.
#[derive(Debug)]
pub enum Failure {
    /// The run outlived its deadline; its process tree was stopped.
    TimedOut,
    /// The harness loaded something that hands the agent a credential, such
    /// as an MCP server.
    Credentials(Vec<CredentialFinding>),
    /// The harness reported a failure: an error, a crash, a non-zero exit.
    Harness(String),
    /// The harness's output could not be read to its end.
    Driver(String),
    /// The role left no `result.json`.
    NoResult,
    /// `result.json` was refused: unreadable, too large, a link, or not a
    /// valid result for this run.
    InvalidResult(String),
    /// `result.json` was refused for the build role's own decisions: it
    /// listed some, or ended `done` while its predecessor's were open
    /// (OWL-176). The next Build run is told so, and may not end `done`
    /// (`Brief::decisions_refused`, OWL-186); `kind` says which, and
    /// carries the choices a result listed, typed, so every refused choice
    /// is kept for it (OWL-195, OWL-198).
    Decisions {
        kind: DecisionsRefusal,
        reason: String,
    },
    /// An artifact the result names was refused.
    Artifact(ArtifactError),
    /// The role reported `done`, and the project's gate, run by the
    /// executor, failed: what the next Build run is given to fix.
    Gate(Box<GateFailure>),
}

impl Failure {
    /// Why the run's `result.json`, or an artifact it names, was refused,
    /// when that is the run's own mistake to fix: what its next run is told
    /// (`on_demand::result_refusal`, OWL-180 and OWL-184). `None` for every
    /// other failure, an artifact the runner could not read included.
    pub fn refusal(&self) -> Option<String> {
        match self {
            Self::InvalidResult(reason) | Self::Decisions { reason, .. } => Some(reason.clone()),
            Self::Artifact(error) => error.is_the_runs().then(|| error.to_string()),
            Self::TimedOut
            | Self::Credentials(_)
            | Self::Harness(_)
            | Self::Driver(_)
            | Self::NoResult
            | Self::Gate(_) => None,
        }
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TimedOut => f.write_str("the run outlived its deadline and was stopped"),
            Self::Credentials(findings) => write!(f, "{}", list(findings)),
            Self::Harness(reason) => write!(f, "the harness failed: {reason}"),
            Self::Driver(reason) => write!(f, "the harness's output could not be read: {reason}"),
            Self::NoResult => write!(f, "the role left no {RESULT_PATH}"),
            Self::InvalidResult(reason) | Self::Decisions { reason, .. } => {
                write!(f, "{RESULT_PATH} was refused: {reason}")
            }
            Self::Artifact(error) => error.fmt(f),
            Self::Gate(failure) => match &failure.command {
                Some(command) => {
                    write!(f, "the project gate failed: {command}: {}", failure.reason)
                }
                None => write!(f, "the project gate failed: {}", failure.reason),
            },
        }
    }
}

/// Why a run could not start. Nothing was spawned.
#[derive(Debug)]
pub enum ExecutorError {
    /// The branch name, the base or a path cannot be used.
    Spec(String),
    /// The worktree could not be created, or the one found is not the
    /// ticket's.
    Worktree(String),
    /// The run's files in the worktree could not be written safely.
    Layout(String),
    /// The run directory or a log file could not be written.
    RunDir { path: PathBuf, source: io::Error },
    /// A credential is within the agent's reach.
    Credentials(Vec<CredentialFinding>),
    /// The state to protect could not be read before the run.
    Snapshot(String),
    /// The harness could not build its command.
    Command(HarnessError),
    /// The harness could not be spawned.
    Spawn(io::Error),
}

impl fmt::Display for ExecutorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Spec(reason) => write!(f, "invalid run: {reason}"),
            Self::Worktree(reason) => write!(f, "worktree: {reason}"),
            Self::Layout(reason) => write!(f, "run files: {reason}"),
            Self::RunDir { path, source } => write!(f, "{}: {source}", path.display()),
            Self::Credentials(findings) => write!(f, "run refused: {}", list(findings)),
            Self::Snapshot(reason) => write!(f, "could not read the state to protect: {reason}"),
            Self::Command(error) => write!(f, "the harness command: {error}"),
            Self::Spawn(error) => write!(f, "could not start the harness: {error}"),
        }
    }
}

impl std::error::Error for ExecutorError {}

fn list(items: &[impl fmt::Display]) -> String {
    let items: Vec<String> = items.iter().map(ToString::to_string).collect();
    items.join("; ")
}

impl Executor {
    /// Runs one role on `harness`. See the module documentation.
    pub fn run(
        &self,
        spec: &RunSpec<'_>,
        harness: &dyn Harness,
    ) -> Result<RunReport, ExecutorError> {
        let started = Instant::now();
        // An agent that cannot be confined never starts, before anything is
        // written (OWL-41).
        self.agent
            .sandbox_ready()
            .map_err(|e| ExecutorError::Spawn(io::Error::other(e)))?;
        worktree::prepare(&self.git, spec)?;
        let mut brief = spec.brief.clone();
        brief.result_path =
            RelativePath::new(RESULT_PATH).expect("RESULT_PATH is a plain relative path");
        let brief_file = worktree::write_run_files(spec.worktree, &brief)?;
        let run_dir = |source| ExecutorError::RunDir {
            path: spec.run_dir.to_owned(),
            source,
        };
        fs::create_dir_all(spec.run_dir).map_err(run_dir)?;
        fs::write(spec.run_dir.join("brief.json"), brief.render()).map_err(run_dir)?;
        let mut log = RunLog::create(spec.run_dir)?;

        let hosts: Vec<&str> = self.forge_hosts.iter().map(String::as_str).collect();
        let findings = self.agent.check(spec.worktree, &hosts);
        if !findings.is_empty() {
            return Err(ExecutorError::Credentials(findings));
        }
        let before = isolation::Snapshot::take(&self.git, spec.main, spec.worktree, spec.branch)
            .map_err(|e| ExecutorError::Snapshot(e.to_string()))?;

        let context = HarnessRun {
            worktree: spec.worktree,
            brief: &brief,
            brief_file: &brief_file,
        };
        let needs = harness
            .sandbox_needs(&self.agent)
            .map_err(ExecutorError::Command)?;
        let login = needs.login;
        if let Some(login) = &login {
            log.hide(&login.token);
        }
        // The run's own temporary folder: private to the user, removed when
        // `run` returns, after every process of the run is stopped.
        let temp = tempfile::Builder::new()
            .prefix("owlshift-run-")
            .tempdir()
            .map_err(|source| ExecutorError::RunDir {
                path: std::env::temp_dir(),
                source,
            })?;
        let mut paths = self.run_paths(spec)?;
        let inner = harness.command(&context).map_err(ExecutorError::Command)?;
        paths.readable = needs.readable;
        paths.writable = needs.writable;
        paths.temp = Some(temp.path().to_owned());
        // An empty configuration folder of the run's own, in its temporary
        // folder: no other login is found there, and nothing the harness
        // writes outlives the run (OWL-94). It is bound read and written:
        // under bwrap the temporary folder is made anew, empty, inside the
        // sandbox's private `/tmp`, so a folder the runner made there would
        // not be seen.
        let config = match &login {
            Some(_) => {
                let config = temp.path().join(HARNESS_CONFIG_DIR);
                fs::create_dir(&config).map_err(|source| ExecutorError::RunDir {
                    path: config.clone(),
                    source,
                })?;
                paths.writable.push(config.clone());
                Some(config)
            }
            None => None,
        };
        let mut command = self
            .agent
            .confine(inner, &paths)
            .map_err(|e| ExecutorError::Spawn(io::Error::other(e)))?;
        if let (Some(login), Some(config)) = (&login, &config) {
            // The token goes to this command alone, on a pipe its child
            // inherits, never in an environment (OWL-96); the error does not
            // hold it.
            login
                .apply(&mut command, config)
                .map_err(ExecutorError::Spawn)?;
        }
        if let Some(variable) = needs.temp_variable {
            // Clear of the temporary folders the user's own processes share,
            // which the sandbox closes (OWL-100). The harness makes the folder
            // in the run's temporary folder, already written: nothing more is
            // opened.
            command.env(variable, temp.path().join(HARNESS_TEMP_DIR));
        }
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let (mut child, tree, watchdog) =
            watch::spawn(&mut command, self.timeout).map_err(ExecutorError::Spawn)?;
        drop(command);
        let driven = harness.drive(&context, &mut child, &mut log);
        let timed_out = watchdog.finish();
        // Whatever the run left running is stopped before anything it wrote
        // is read, so nothing changes behind the checks. Best effort: a
        // process that left the tree is a documented limit.
        let _ = tree.kill();
        let _ = child.kill();
        let _ = child.wait();
        log.finish();

        let violations = before.check(&self.git, spec.main, spec.worktree, spec.branch);
        let hidden = login.as_ref().map(|login| &login.token);
        let (mut outcome, end) = decide(violations, timed_out, driven, spec, hidden);
        let mut gate = None;
        if brief.role == Role::Build && is_done(&outcome) {
            // The gate runs the code the run wrote, confined as the run was,
            // without the harness's own folders.
            let report = gate::run(
                &self.git,
                &self.agent,
                &RunPaths {
                    readable: Vec::new(),
                    writable: Vec::new(),
                    ..paths
                },
                &brief.gate,
                self.gate_timeout,
                &spec.run_dir.join(gate::GATE_LOG),
                &|| before.check_files(spec.worktree),
            );
            if let Some(after_gate) = after_gate(&report, || {
                before.check(&self.git, spec.main, spec.worktree, spec.branch)
            }) {
                outcome = after_gate;
            }
            gate = Some(report);
        }
        Ok(RunReport {
            outcome,
            exit_code: end.as_ref().and_then(|end| end.exit_code),
            usage: end.as_ref().and_then(|end| end.usage.clone()),
            model: end.as_ref().and_then(|end| end.model.clone()),
            harness_version: end.and_then(|end| end.harness_version),
            elapsed: started.elapsed(),
            stdout_log: log.stdout_path,
            stderr_log: log.stderr_path,
            log_error: log.error,
            gate,
        })
    }
}

impl Executor {
    /// What the sandbox opens for this run: the worktree, the repository's
    /// common git folder, as the runner's git reports it; and what it
    /// closes: the run directory, which holds the logs.
    fn run_paths(&self, spec: &RunSpec<'_>) -> Result<RunPaths, ExecutorError> {
        let printed = self
            .git
            .run(
                spec.worktree,
                &["rev-parse", "--path-format=absolute", "--git-common-dir"],
            )
            .map_err(|e| ExecutorError::Worktree(e.to_string()))?;
        let git_dir = PathBuf::from(String::from_utf8_lossy(&printed).trim());
        Ok(RunPaths {
            workdir: spec.worktree.to_owned(),
            git_dir: Some(git_dir),
            readable: Vec::new(),
            writable: Vec::new(),
            hidden: vec![spec.run_dir.to_owned()],
            temp: None,
        })
    }
}

/// Whether the role finished with `done`.
fn is_done(outcome: &Outcome) -> bool {
    matches!(outcome, Outcome::Finished { result, .. } if result.status == result::Status::Done)
}

/// What the gate makes of a Build `done`; `None` leaves it as it was. The
/// gate ran code the run wrote, so isolation counts first: a breach the gate
/// found before its own git quarantines the run as it is, with no more git;
/// otherwise `check` checks isolation again, and a violation there wins over
/// the gate's own verdict.
fn after_gate(report: &GateReport, check: impl FnOnce() -> Vec<Violation>) -> Option<Outcome> {
    if !report.breach.is_empty() {
        return Some(Outcome::Quarantined(report.breach.clone()));
    }
    let violations = check();
    if !violations.is_empty() {
        Some(Outcome::Quarantined(violations))
    } else {
        report
            .failure
            .as_ref()
            .map(|failure| Outcome::Failed(Failure::Gate(Box::new(failure.clone()))))
    }
}

/// The outcome, in order of precedence: a violation, the deadline, a
/// credential the harness loaded, a usage limit, a harness failure, then
/// the result itself, refused when it holds `hidden`, the harness's login.
fn decide(
    violations: Vec<Violation>,
    timed_out: bool,
    driven: io::Result<HarnessEnd>,
    spec: &RunSpec<'_>,
    hidden: Option<&Secret>,
) -> (Outcome, Option<HarnessEnd>) {
    let (driven, end) = match driven {
        Ok(end) => (Ok(()), Some(end)),
        Err(error) => (Err(error), None),
    };
    let outcome = if !violations.is_empty() {
        Outcome::Quarantined(violations)
    } else if timed_out {
        Outcome::Failed(Failure::TimedOut)
    } else if let Err(error) = driven {
        Outcome::Failed(Failure::Driver(error.to_string()))
    } else {
        let end = end.as_ref().expect("the harness was driven to its end");
        if !end.findings.is_empty() {
            Outcome::Failed(Failure::Credentials(end.findings.clone()))
        } else {
            match &end.status {
                HarnessStatus::UsageLimit { resets_at } => Outcome::UsageLimit {
                    resets_at: *resets_at,
                },
                HarnessStatus::Failed(reason) => Outcome::Failed(Failure::Harness(reason.clone())),
                HarnessStatus::Completed => {
                    read_result(spec.worktree, spec.branch, spec.brief, hidden)
                }
            }
        }
    };
    (outcome, end)
}

/// Reads and validates the role's `result.json`, then its artifacts. A
/// result or an artifact that holds `hidden`, the harness's login, fails the
/// run: what it says would reach the tracker, the pull request and the
/// events (OWL-94).
fn read_result(worktree: &Path, branch: &str, brief: &Brief, hidden: Option<&Secret>) -> Outcome {
    let copied = |into: &str, bytes: &[u8]| {
        hidden.is_some_and(|token| holds(bytes, token)).then(|| {
            Outcome::Failed(Failure::Credentials(vec![CredentialFinding::LoginCopied {
                into: into.to_owned(),
            }]))
        })
    };
    let bytes = match read_confined(worktree, RESULT_PATH, MAX_RESULT_BYTES) {
        Ok(bytes) => bytes,
        Err(ConfinedError::Refused {
            reason: Refusal::NotFound,
            ..
        }) => return Outcome::Failed(Failure::NoResult),
        Err(error) => return Outcome::Failed(Failure::InvalidResult(error.to_string())),
    };
    if let Some(refused) = copied(RESULT_PATH, &bytes) {
        return refused;
    }
    let result = match validate_result(&bytes, branch, brief) {
        Ok(result) => result,
        Err(failure) => return Outcome::Failed(failure),
    };
    match read_artifacts(worktree, &result.artifacts) {
        Ok(artifacts) => {
            let named = [
                ("the plan artifact", &artifacts.plan),
                ("the ledger artifact", &artifacts.ledger),
                ("the findings artifact", &artifacts.findings),
                ("the report artifact", &artifacts.report),
            ];
            for (into, bytes) in named {
                if let Some(refused) = bytes.as_deref().and_then(|bytes| copied(into, bytes)) {
                    return refused;
                }
            }
            Outcome::Finished {
                result: Box::new(result),
                artifacts,
            }
        }
        Err(error) => Outcome::Failed(Failure::Artifact(error)),
    }
}

/// Parses `result.json` against its contract and against the run's brief
/// (verdicts from the answer check only, covering its latest ask), and
/// requires a pull request to name the run's own branch. A refusal for the
/// build role's own decisions is [`Failure::Decisions`], any other
/// [`Failure::InvalidResult`].
pub fn validate_result(bytes: &[u8], branch: &str, brief: &Brief) -> Result<RunResult, Failure> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| Failure::InvalidResult("it is not UTF-8 text".to_owned()))?;
    let result = RunResult::parse_against(text, brief).map_err(|refused| {
        let reason = refused.error.to_string();
        match refused.decisions {
            Some(kind) => Failure::Decisions { kind, reason },
            None => Failure::InvalidResult(reason),
        }
    })?;
    if let Some(pr) = &result.pr
        && pr.branch != branch
    {
        return Err(Failure::InvalidResult(format!(
            "pr.branch is {:?}, not the run's branch {branch:?}",
            pr.branch
        )));
    }
    Ok(result)
}

/// What replaces the harness's login token in the run's log files.
pub const REDACTED: &str = "<redacted>";

/// The run's log files: the harness's standard output and standard error,
/// written as they come. A write error does not stop the run; the first one
/// is kept for the report.
///
/// Once told of the harness's login token (`RunLog::hide`), each file gets
/// [`REDACTED`] wherever the harness printed the token, even split across two
/// writes: the bytes that could start one are held back until the next
/// write, or `RunLog::finish`. The agent's shell inherits the token, so a
/// command such as `env` would otherwise log it (OWL-94).
pub struct RunLog {
    stdout: File,
    stderr: File,
    stdout_path: PathBuf,
    stderr_path: PathBuf,
    error: Option<String>,
    hidden: Option<Secret>,
    stdout_held: Vec<u8>,
    stderr_held: Vec<u8>,
}

impl fmt::Debug for RunLog {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The held bytes may be the start of the token.
        f.debug_struct("RunLog")
            .field("stdout_path", &self.stdout_path)
            .field("stderr_path", &self.stderr_path)
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}

impl RunLog {
    fn create(dir: &Path) -> Result<Self, ExecutorError> {
        let open = |name: &str| {
            let path = dir.join(name);
            File::create(&path)
                .map(|file| (file, path.clone()))
                .map_err(|source| ExecutorError::RunDir { path, source })
        };
        let (stdout, stdout_path) = open("stdout.log")?;
        let (stderr, stderr_path) = open("stderr.log")?;
        Ok(Self {
            stdout,
            stderr,
            stdout_path,
            stderr_path,
            error: None,
            hidden: None,
            stdout_held: Vec::new(),
            stderr_held: Vec::new(),
        })
    }

    /// Replaces `token` with [`REDACTED`] in everything logged from now on.
    fn hide(&mut self, token: &Secret) {
        if !token.expose().is_empty() {
            self.hidden = Some(token.clone());
        }
    }

    /// Appends bytes of standard output.
    pub fn stdout(&mut self, bytes: &[u8]) {
        let written = redacted_write(
            &mut self.stdout,
            &mut self.stdout_held,
            self.hidden.as_ref(),
            bytes,
        );
        self.keep(written, "stdout.log");
    }

    /// Appends bytes of standard error.
    pub fn stderr(&mut self, bytes: &[u8]) {
        let written = redacted_write(
            &mut self.stderr,
            &mut self.stderr_held,
            self.hidden.as_ref(),
            bytes,
        );
        self.keep(written, "stderr.log");
    }

    /// Writes the bytes held back: too few to hold the token.
    fn finish(&mut self) {
        let written = self
            .stdout
            .write_all(&std::mem::take(&mut self.stdout_held));
        self.keep(written, "stdout.log");
        let written = self
            .stderr
            .write_all(&std::mem::take(&mut self.stderr_held));
        self.keep(written, "stderr.log");
    }

    fn keep(&mut self, written: io::Result<()>, name: &str) {
        if let Err(error) = written
            && self.error.is_none()
        {
            self.error = Some(format!("{name}: {error}"));
        }
    }
}

/// Writes `bytes` to `file`, `hidden`'s value replaced with [`REDACTED`].
/// `held` carries the last bytes of the previous write, which could start
/// the token, and keeps this write's, fewer than the token's length.
fn redacted_write(
    file: &mut File,
    held: &mut Vec<u8>,
    hidden: Option<&Secret>,
    bytes: &[u8],
) -> io::Result<()> {
    let Some(token) = hidden.map(|secret| secret.expose().as_bytes()) else {
        return file.write_all(bytes);
    };
    held.extend_from_slice(bytes);
    let text = replace_all(held, token, REDACTED.as_bytes());
    let split = text.len().saturating_sub(token.len() - 1);
    *held = text[split..].to_vec();
    file.write_all(&text[..split])
}

/// `haystack` with every occurrence of `needle`, which is not empty,
/// replaced with `with`.
fn replace_all(haystack: &[u8], needle: &[u8], with: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(haystack.len());
    let mut at = 0;
    while at < haystack.len() {
        if haystack[at..].starts_with(needle) {
            out.extend_from_slice(with);
            at += needle.len();
        } else {
            out.push(haystack[at]);
            at += 1;
        }
    }
    out
}

/// Whether `haystack` holds `token`'s value.
fn holds(haystack: &[u8], token: &Secret) -> bool {
    let needle = token.expose().as_bytes();
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_result_is_valid_for_its_contract_its_brief_and_its_branch() {
        let brief = Brief::parse(&format!(
            r#"{{"format":{},"role":"build","project":"p",
                "ticket":{{"id":"T-1","title":"t","author":{{"name":"a","relation":"decider"}},"description":"d"}},
                "decider":"a","permissions":{{"level":"write_worktree","network":false,"browser":false}},
                "gate":[],"always_human":[],"result_path":"result.json"}}"#,
            owlshift_contracts::format::BRIEF_FORMAT
        ))
        .unwrap();
        let done = br#"{"format":6,"status":"done","summary":"s","pr":{"branch":"owlshift/T-1","title":"t","body":"b"}}"#;
        assert!(validate_result(done, "owlshift/T-1", &brief).is_ok());

        let other = validate_result(done, "owlshift/T-2", &brief)
            .unwrap_err()
            .to_string();
        assert!(other.contains("pr.branch"), "{other}");
        // A contract rule: questions needs a question.
        let empty = br#"{"format":6,"status":"questions","summary":"s","questions":[]}"#;
        assert!(validate_result(empty, "owlshift/T-1", &brief).is_err());
        // A rule against the brief: verdicts come from the answer check only.
        let verdicts = br#"{"format":6,"status":"done","summary":"s","verdicts":[{"question":"Q1","class":"answered","reason":"r"}]}"#;
        let refused = validate_result(verdicts, "owlshift/T-1", &brief).unwrap_err();
        assert!(
            matches!(&refused, Failure::InvalidResult(reason) if reason.contains("role is build")),
            "{refused:?}"
        );
        // The build role's own decisions are a refusal of their own kind,
        // which its next run is told of (OWL-186).
        let decided = br#"{"format":6,"status":"done","summary":"s","decisions":[{"question":"q","decision":"d","basis":"b"}]}"#;
        let refused = validate_result(decided, "owlshift/T-1", &brief).unwrap_err();
        assert!(
            matches!(
                &refused,
                Failure::Decisions { kind: DecisionsRefusal::Listed(choices), reason }
                    if reason.contains("ask each")
                        && choices.len() == 1
                        && (choices[0].question.as_str(), choices[0].recorded.as_str())
                            == ("q", "d")
            ),
            "{refused:?}"
        );
        // Unknown fields are refused, and so is text that is not UTF-8.
        let unknown = br#"{"format":6,"status":"done","summary":"s","extra":1}"#;
        assert!(validate_result(unknown, "owlshift/T-1", &brief).is_err());
        assert!(validate_result(b"\xff", "owlshift/T-1", &brief).is_err());
    }

    /// OWL-94: the harness's login token never reaches the log files, even
    /// split across writes or printed twice in a row; what is not the token
    /// is logged as it came, the held bytes included.
    #[test]
    fn the_run_log_hides_the_login_token() {
        const TOKEN: &str = "sk-ant-oat01-SENTINEL";
        let dir = tempfile::tempdir().unwrap();
        let mut log = RunLog::create(dir.path()).unwrap();
        log.hide(&Secret::new(TOKEN));
        for chunk in [
            "env: A=1 T=sk-ant-",
            "oat01-SENTINEL\nagain ",
            TOKEN,
            TOKEN,
            " end sk-an",
        ] {
            log.stdout(chunk.as_bytes());
        }
        log.stderr(TOKEN.as_bytes());
        log.finish();
        assert!(!format!("{log:?}").contains("sk-an"));
        let read = |name| std::fs::read_to_string(dir.path().join(name)).unwrap();
        assert_eq!(
            read("stdout.log"),
            "env: A=1 T=<redacted>\nagain <redacted><redacted> end sk-an"
        );
        assert_eq!(read("stderr.log"), "<redacted>");
        assert_eq!(log.error, None);
    }

    /// A breach the gate found quarantines the run without the second
    /// check, over the failure it also carries; a violation the second check
    /// finds wins over a red gate; a red gate alone fails the run.
    #[test]
    fn isolation_wins_over_the_gate_verdict() {
        let failure = GateFailure {
            command: None,
            reason: "red".to_owned(),
            output: String::new(),
            truncated: false,
        };
        let report = |breach: Vec<Violation>, failure: Option<GateFailure>| GateReport {
            commands: Vec::new(),
            commit: None,
            log: PathBuf::new(),
            failure,
            breach,
        };
        let link = Violation::WorktreeLink("redirected".to_owned());
        let breached = report(vec![link.clone()], Some(failure.clone()));
        let outcome = after_gate(&breached, || panic!("no more git after a breach"));
        assert!(matches!(outcome, Some(Outcome::Quarantined(v)) if v == [link.clone()]));

        let red = report(Vec::new(), Some(failure));
        let outcome = after_gate(&red, || vec![Violation::MainHead]);
        assert!(matches!(outcome, Some(Outcome::Quarantined(v)) if v == [Violation::MainHead]));
        let outcome = after_gate(&red, Vec::new);
        assert!(matches!(outcome, Some(Outcome::Failed(Failure::Gate(f))) if f.reason == "red"));

        assert!(after_gate(&report(Vec::new(), None), Vec::new).is_none());
    }
}
