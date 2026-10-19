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
//!   never taken as proof. A read-only run that left none may have given it
//!   as its last message, which the executor then writes there first
//!   (architecture section 6, "Harness contract", OWL-323);
//! - a usage limit is an interruption to resume, not a failure;
//! - everything else is a [`Failure`] with its reason.
//!
//! What goes wrong before the spawn (the worktree, the brief, a credential
//! finding, the first snapshot, the command) is an [`ExecutorError`]:
//! nothing ran.
//!
//! A Build or rebase run that finishes with `done` is not taken at its word
//! either (a rebase run's isolation check accepts its new base, OWL-210):
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
//!
//! A harness that asks for the read-only profile ([`SandboxNeeds::read_only`],
//! OWL-298) gets the worktree and the repository's git folder read, never
//! written, and nothing is written in the worktree: the brief and the result
//! go to [`RUN_FILES_DIR`] in the run's own temporary folder, which the run
//! reads and writes and which is removed with it. A harness may also ask
//! there for a home folder of its own holding a copy of the user's login
//! ([`SandboxNeeds::home`]), and give its result as its last message
//! ([`SandboxNeeds::result_in_message`]): Codex does both
//! ([`CodexHarness`], OWL-299).

pub mod codex;
pub mod gate;
mod git;
pub mod harness;
mod home;
pub mod isolation;
mod message;
mod watch;
mod worktree;

use std::fmt;
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant, SystemTime};

use jiff::Timestamp;

use owlshift_adapters::harness::claude::Usage;
use owlshift_contracts::Role;
use owlshift_contracts::brief::{Brief, GateFailure, PermissionLevel};
use owlshift_contracts::ids::RelativePath;
use owlshift_contracts::result::{self, DecisionsRefusal, RunResult};
use owlshift_platform::confined::{ConfinedError, Refusal, read_confined};
use owlshift_platform::keychain::Secret;
use owlshift_platform::sandbox;

use crate::agent_env::{AgentEnv, CredentialFinding, RunPaths};
use crate::artifact::{ArtifactContents, ArtifactError, MAX_ARTIFACT_BYTES, read_artifacts};

pub use codex::CodexHarness;
pub use gate::{DEFAULT_GATE_TIMEOUT, GateReport, GateStep};
pub use git::{Git, GitError, MINIMUM_GIT_VERSION};
pub(crate) use git::{git_floor_reason, git_install_advice, git_recheck_note};
pub use harness::{
    Harness, HarnessEnd, HarnessError, HarnessHome, HarnessLogin, HarnessRun, HarnessStatus,
    SandboxNeeds,
};
pub use isolation::{Violation, check_worktree_link};
pub use watch::RunStop;

/// The directory of the run's own files, relative to the worktree.
pub const RUN_DIR: &str = ".owlshift/run";

/// Where the executor writes the brief, relative to the worktree.
pub const BRIEF_PATH: &str = ".owlshift/run/brief.json";

/// Where the runner writes the diff a Verify pass reviews, relative to the
/// worktree (OWL-302, OWL-304): beside the brief, out of git.
pub const DIFF_PATH: &str = ".owlshift/run/diff.patch";

/// Where the runner writes the diff since the loop's previous Verify pass,
/// relative to the worktree (OWL-356): beside the whole diff.
pub const SINCE_PATH: &str = ".owlshift/run/since.patch";

/// Writes `content` at `path` in `worktree`, [`DIFF_PATH`] or
/// [`SINCE_PATH`], through no link, the run folder made as for a run;
/// returns the file's path.
pub fn write_run_diff(
    worktree: &Path,
    path: &str,
    content: &[u8],
) -> Result<PathBuf, ExecutorError> {
    worktree::write_run_file(worktree, path, content)
}

/// Where the role writes `result.json`, relative to the worktree: the
/// brief's `result_path`.
pub const RESULT_PATH: &str = ".owlshift/run/result.json";

/// The folder of the run's own files under the read-only profile (OWL-298),
/// in the run's own temporary folder: the brief, the result
/// ([`RUN_FILES_RESULT`]) and the harness's own files of the run.
pub const RUN_FILES_DIR: &str = "run-files";

/// The result's name in [`RUN_FILES_DIR`].
pub const RUN_FILES_RESULT: &str = "result.json";

/// The system's certificate bundle on macOS, given to a read-only harness
/// command as `SSL_CERT_FILE` (OWL-298).
pub const MACOS_CERT_BUNDLE: &str = "/etc/ssl/cert.pem";

/// The most bytes `result.json` may hold, as for an artifact.
pub const MAX_RESULT_BYTES: u64 = MAX_ARTIFACT_BYTES;

/// The harness's configuration folder of one run, in the run's own
/// temporary folder, when the harness logs in with a token (OWL-94).
pub const HARNESS_CONFIG_DIR: &str = "harness-config";

/// The harness's temporary folder of one run, in the run's own temporary
/// folder, when the harness names one (OWL-100). The harness makes it.
pub const HARNESS_TEMP_DIR: &str = "harness-tmp";

/// The harness's home folder of one run, in the run's own files, when the
/// harness asks for one ([`SandboxNeeds::home`]).
pub const HARNESS_HOME_DIR: &str = "harness-home";

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
    /// What a new branch starts from, read only when the branch is created
    /// (a dispatch from Ready first brings a kept branch with no commits of
    /// its own to it, [`Executor::start_from_base`]): a full commit id, or a remote-tracking name such as `origin/main`,
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
    /// The run's monotonic time. It leaves out the time the machine sleeps
    /// on macOS, so it is not what the operator waited (OWL-262).
    pub elapsed: Duration,
    /// The run's time by the system clock, sleep included. `None` when the
    /// clock reads earlier at the end than at the start (it was set back);
    /// a smaller step inside the run is not detected.
    pub wall_elapsed: Option<Duration>,
    /// The harness's standard output, as it came.
    pub stdout_log: PathBuf,
    /// The harness's standard error.
    pub stderr_log: PathBuf,
    /// The first error writing a log file: the run went on without it.
    pub log_error: Option<String>,
    /// The project's gate, when the executor ran it: after a Build or a
    /// rebase `done` that broke no isolation. Its commands and commit fill the delivery
    /// report's gate when it passed.
    pub gate: Option<GateReport>,
    /// What the run's harness home left to say: a changed login that was
    /// not written back, or a result that could not be written from the
    /// last message; and a read-only run's result read from its last
    /// message, or not written from it (OWL-323).
    pub warnings: Vec<String>,
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
    /// Brings the ticket's kept `branch` to `base` before a dispatch to Build
    /// from Ready when it has no commits of its own, its head an ancestor of
    /// `base`; any other branch is kept as it is, and a missing one is left
    /// for the run to create (OWL-274). `main`, `worktree`, `branch` and
    /// `base` are a [`RunSpec`]'s, and a worktree found is checked as a run
    /// checks it. A fast-forward that would overwrite a local change moves
    /// nothing and is an [`ExecutorError::Worktree`].
    pub fn start_from_base(
        &self,
        main: &Path,
        worktree: &Path,
        branch: &str,
        base: &str,
    ) -> Result<(), ExecutorError> {
        worktree::start_from_base(&self.git, main, worktree, branch, base)
    }

    /// Runs one role on `harness`. See the module documentation.
    pub fn run(
        &self,
        spec: &RunSpec<'_>,
        harness: &dyn Harness,
    ) -> Result<RunReport, ExecutorError> {
        let started = Instant::now();
        let wall_started = SystemTime::now();
        // A rebase run may end on a commit that descends from its new base
        // instead of its start (OWL-49): the base it was told, which must
        // be the commit the run was given, so the isolation check never
        // accepts a descent from another one (OWL-210).
        let onto = spec
            .brief
            .rebase
            .as_ref()
            .map(|rebase| rebase.onto.as_str());
        if let Some(onto) = onto
            && onto != spec.base
        {
            return Err(ExecutorError::Spec(format!(
                "the rebase is onto {onto}, but the run's base is {}",
                spec.base
            )));
        }
        // An agent that cannot be confined never starts, before anything is
        // written (OWL-41).
        self.agent
            .sandbox_ready()
            .map_err(|e| ExecutorError::Spawn(io::Error::other(e)))?;
        let needs = harness
            .sandbox_needs(&self.agent)
            .map_err(ExecutorError::Command)?;
        if needs.read_only && spec.brief.permissions.level != PermissionLevel::ReadOnly {
            return Err(ExecutorError::Spec(format!(
                "the {} role's brief lets it write the worktree, which the read-only \
                 profile its harness asks for does not",
                spec.brief.role.as_str()
            )));
        }
        if (needs.home.is_some() || needs.result_in_message) && !needs.read_only {
            return Err(ExecutorError::Spec(
                "the harness asks for a home of its own or gives its result as its last \
                 message, which only the read-only profile provides"
                    .to_owned(),
            ));
        }
        worktree::prepare(&self.git, spec)?;
        let mut paths = self.run_paths(spec.worktree, spec.run_dir)?;
        if needs.read_only {
            read_only_clear(&needs.writable, &paths)?;
        }
        // The run's own temporary folder: private to the user, removed when
        // `run` returns, after every process of the run is stopped.
        let temp = run_temp()?;
        let mut brief = spec.brief.clone();
        brief.result_path =
            RelativePath::new(RESULT_PATH).expect("RESULT_PATH is a plain relative path");
        // Where the run's own files go: the worktree, or, under the read-only
        // profile, a folder of the run's temporary folder, bound read and
        // written (as the configuration folder below), and nothing is written
        // in the worktree.
        let (brief_file, result_file, run_files) = if needs.read_only {
            let run_files = temp.path().join(RUN_FILES_DIR);
            let layout = |source| ExecutorError::RunDir {
                path: run_files.clone(),
                source,
            };
            fs::create_dir(&run_files).map_err(layout)?;
            let brief_file = run_files.join("brief.json");
            fs::write(&brief_file, brief.render()).map_err(layout)?;
            (
                brief_file,
                run_files.join(RUN_FILES_RESULT),
                Some(run_files),
            )
        } else {
            let brief_file = worktree::write_run_files(spec.worktree, &brief)?;
            (brief_file, spec.worktree.join(RESULT_PATH), None)
        };
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
        let before = match onto {
            Some(onto) => isolation::Snapshot::take_rebase(
                &self.git,
                spec.main,
                spec.worktree,
                spec.branch,
                onto,
            ),
            None => isolation::Snapshot::take(&self.git, spec.main, spec.worktree, spec.branch),
        }
        .map_err(|e| ExecutorError::Snapshot(e.to_string()))?;

        let context = HarnessRun {
            worktree: spec.worktree,
            brief: &brief,
            brief_file: &brief_file,
            result_file: &result_file,
            stop: None,
        };
        let login = needs.login;
        let inner = harness.command(&context).map_err(ExecutorError::Command)?;
        // The user's login is copied in as late as can be: an early refusal
        // leaves no copy behind.
        let seeded = match (&needs.home, &run_files) {
            (Some(home), Some(files)) => {
                let dir = files.join(HARNESS_HOME_DIR);
                let seeded =
                    home::seed(&dir, &home.login).map_err(|source| ExecutorError::RunDir {
                        path: home.login.clone(),
                        source,
                    })?;
                Some((home.variable, dir, seeded))
            }
            _ => None,
        };
        let mut hidden: Vec<Secret> = login.iter().map(|login| login.token.clone()).collect();
        if let Some((_, _, seeded)) = &seeded {
            hidden.extend(harness.login_secrets(seeded.original()));
        }
        for secret in &hidden {
            log.hide(secret);
        }
        paths.readable = needs.readable;
        paths.writable = needs.writable;
        paths.writable.extend(run_files.iter().cloned());
        paths.temp = Some(temp.path().to_owned());
        paths.read_only = needs.read_only;
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
        if let Some((variable, dir, _)) = &seeded {
            command.env(variable, dir);
        }
        if let Some(variable) = needs.temp_variable {
            // Clear of the temporary folders the user's own processes share,
            // which the sandbox closes (OWL-100). The harness makes the folder
            // in the run's temporary folder, already written: nothing more is
            // opened.
            command.env(variable, temp.path().join(HARNESS_TEMP_DIR));
        }
        if let Some(bundle) = certificate_bundle(&self.agent, needs.read_only) {
            command.env("SSL_CERT_FILE", bundle);
        }
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let (mut child, tree, watchdog) =
            watch::spawn(&mut command, self.timeout).map_err(ExecutorError::Spawn)?;
        drop(command);
        let stop = watchdog.run_stop();
        let driving = HarnessRun {
            stop: Some(&stop),
            ..context
        };
        let driven = harness.drive(&driving, &mut child, &mut log);
        // False when the driver stopped the run itself, even past the
        // deadline: its harness's end says why (OWL-319).
        let timed_out = watchdog.finish();
        // Whatever the run left running is stopped before anything it wrote
        // is read, so nothing changes behind the checks. Best effort: a
        // process that left the tree is a documented limit.
        let _ = tree.kill();
        let _ = child.kill();
        let _ = child.wait();
        log.finish();

        // Once nothing of the run is left running: the result from the last
        // message, over whatever the run wrote there, and the login back.
        let mut warnings = Vec::new();
        let mut result_written = Ok(());
        if needs.result_in_message {
            let message = driven.as_ref().ok().and_then(|end| end.message.as_deref());
            result_written = home::result_from_message(&result_file, message).map_err(|error| {
                format!("it could not be written from the run's last message: {error}")
            });
        }
        // A read-only run its harness reports completed, which left nothing
        // at the result's place, has its result read from its last message
        // (OWL-323): written there through no link, then judged as any
        // result. A failed write leaves whatever it left to that judgment.
        if !needs.result_in_message
            && run_files.is_none()
            && !timed_out
            && brief.permissions.level == PermissionLevel::ReadOnly
            && let Ok(end) = &driven
            && end.status == HarnessStatus::Completed
            && let Some(text) = end
                .message
                .as_deref()
                .and_then(message::last_message_result)
            && left_nothing(&result_file)
        {
            warnings.push(
                match worktree::write_run_file(spec.worktree, RESULT_PATH, text.as_bytes()) {
                    Ok(_) => from_message_warning(&end.permission_denials),
                    Err(error) => format!(
                        "the role left no {RESULT_PATH}, and the result of its last message \
                         could not be written there: {error}"
                    ),
                },
            );
        }
        if let Some((_, _, seeded)) = &seeded {
            warnings.extend(home::write_back(seeded, |original, updated| {
                harness.login_kept(original, updated)
            }));
        }

        let violations = before.check(&self.git, spec.main, spec.worktree, spec.branch);
        let result = match &run_files {
            Some(run_files) => (run_files.as_path(), RUN_FILES_RESULT),
            None => (spec.worktree, RESULT_PATH),
        };
        let (mut outcome, end) = decide(violations, timed_out, driven, spec, result, &hidden);
        if let Err(reason) = result_written
            && matches!(outcome, Outcome::Finished { .. })
        {
            outcome = Outcome::Failed(Failure::InvalidResult(reason));
        }
        let mut gate = None;
        if matches!(brief.role, Role::Build | Role::Rebase) && is_done(&outcome) {
            let report =
                self.project_gate(&before, spec.worktree, paths, &brief.gate, spec.run_dir);
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
            wall_elapsed: wall_span(wall_started, SystemTime::now()),
            stdout_log: log.stdout_path,
            stderr_log: log.stderr_path,
            log_error: log.error,
            gate,
            warnings,
        })
    }
}

impl Executor {
    /// What the sandbox opens for this run: the worktree, the repository's
    /// common git folder, as the runner's git reports it; and what it
    /// closes: the run directory, which holds the logs.
    fn run_paths(&self, worktree: &Path, run_dir: &Path) -> Result<RunPaths, ExecutorError> {
        let printed = self
            .git
            .run(
                worktree,
                &["rev-parse", "--path-format=absolute", "--git-common-dir"],
            )
            .map_err(|e| ExecutorError::Worktree(e.to_string()))?;
        let git_dir = PathBuf::from(String::from_utf8_lossy(&printed).trim());
        Ok(RunPaths {
            workdir: worktree.to_owned(),
            git_dir: Some(git_dir),
            readable: Vec::new(),
            writable: Vec::new(),
            hidden: vec![run_dir.to_owned()],
            temp: None,
            read_only: false,
        })
    }

    /// The project's gate, run by the runner on the code in `worktree`,
    /// confined as the run was (`paths`, its temporary folder included),
    /// without a harness's own folders, the guard checking `before`'s files
    /// before each of the gate's own git steps; its log goes to `run_dir`.
    /// The one path a Build's gate and `owlshift gate` share (OWL-329).
    fn project_gate(
        &self,
        before: &isolation::Snapshot,
        worktree: &Path,
        paths: RunPaths,
        commands: &[String],
        run_dir: &Path,
    ) -> GateReport {
        gate::run(
            &self.git,
            &self.agent,
            &RunPaths {
                readable: Vec::new(),
                writable: Vec::new(),
                read_only: false,
                ..paths
            },
            commands,
            self.gate_timeout,
            &run_dir.join(gate::GATE_LOG),
            &|| before.check_files(worktree),
        )
    }

    /// Runs the project's gate once, alone, as a Build's gate runs after a
    /// `done` (OWL-329): in a fresh worktree at `spec.base` on a new branch,
    /// with the run's paths, its own temporary folder and the credential
    /// check a run gets, isolation snapshotted before and checked after.
    /// Nothing is removed afterwards: the caller discards the worktree
    /// ([`Executor::discard_worktree`]) once it has judged the isolation.
    ///
    /// What goes wrong before the first command (the sandbox, the worktree,
    /// a credential finding, the snapshot) is an [`ExecutorError`]; the
    /// worktree may then exist.
    pub fn run_gate(&self, spec: &GateSpec<'_>) -> Result<GateRun, ExecutorError> {
        self.agent
            .sandbox_ready()
            .map_err(|e| ExecutorError::Spawn(io::Error::other(e)))?;
        worktree::create(&self.git, spec.main, spec.worktree, spec.branch, spec.base)?;
        let mut paths = self.run_paths(spec.worktree, spec.run_dir)?;
        let temp = run_temp()?;
        paths.temp = Some(temp.path().to_owned());
        fs::create_dir_all(spec.run_dir).map_err(|source| ExecutorError::RunDir {
            path: spec.run_dir.to_owned(),
            source,
        })?;
        let hosts: Vec<&str> = self.forge_hosts.iter().map(String::as_str).collect();
        let findings = self.agent.check(spec.worktree, &hosts);
        if !findings.is_empty() {
            return Err(ExecutorError::Credentials(findings));
        }
        let before = isolation::Snapshot::take(&self.git, spec.main, spec.worktree, spec.branch)
            .map_err(|e| ExecutorError::Snapshot(e.to_string()))?;
        let report = self.project_gate(&before, spec.worktree, paths, spec.commands, spec.run_dir);
        let violations = gate_violations(&report, || {
            before.check(&self.git, spec.main, spec.worktree, spec.branch)
        });
        Ok(GateRun { report, violations })
    }

    /// Removes a worktree [`Executor::run_gate`] made, and its branch,
    /// without running git in it; see `worktree::discard`.
    pub fn discard_worktree(
        &self,
        main: &Path,
        worktree: &Path,
        branch: &str,
    ) -> Result<(), ExecutorError> {
        worktree::discard(&self.git, main, worktree, branch)
    }

    /// The commit `base` names in `main`, read as a run's base is: a full
    /// commit id, or a remote-tracking name such as `origin/main` looked up
    /// as exactly `refs/remotes/<name>` ([`RunSpec::base`]).
    pub fn resolve_commit(&self, main: &Path, base: &str) -> Result<String, ExecutorError> {
        worktree::resolve_commit(&self.git, main, base)
    }
}

/// One run of the project's gate alone ([`Executor::run_gate`]).
#[derive(Clone, Copy, Debug)]
pub struct GateSpec<'a> {
    /// The checkout the worktree is made from, as [`RunSpec::main`].
    pub main: &'a Path,
    /// Where the fresh worktree goes, an absolute path where nothing is.
    pub worktree: &'a Path,
    /// Its new branch, which must not exist.
    pub branch: &'a str,
    /// What it starts from, as [`RunSpec::base`].
    pub base: &'a str,
    /// Where the gate log goes, outside the worktree, hidden from the gate.
    pub run_dir: &'a Path,
    /// The gate commands, in order.
    pub commands: &'a [String],
}

/// What a run of the gate alone came to.
#[derive(Debug)]
pub struct GateRun {
    pub report: GateReport,
    /// What the isolation check found after the gate, the gate's own
    /// breach included; empty when isolation held.
    pub violations: Vec<Violation>,
}

impl GateRun {
    /// Whether the gate passed and isolation held.
    pub fn passed(&self) -> bool {
        self.report.passed() && self.violations.is_empty()
    }
}

/// The run's own temporary folder: private to the user, removed when the
/// value is dropped. A Build run and `owlshift gate` make it alike.
fn run_temp() -> Result<tempfile::TempDir, ExecutorError> {
    tempfile::Builder::new()
        .prefix("owlshift-run-")
        .tempdir()
        .map_err(|source| ExecutorError::RunDir {
            path: std::env::temp_dir(),
            source,
        })
}

/// Refuses a read-only run whose harness would write a folder that holds,
/// or lies in, the worktree or the git folder, compared by real path: the
/// profile would no longer keep them unwritten.
fn read_only_clear(writable: &[PathBuf], run: &RunPaths) -> Result<(), ExecutorError> {
    let kept: Vec<PathBuf> = [&run.workdir]
        .into_iter()
        .chain(&run.git_dir)
        .map(|path| sandbox::real(path))
        .collect();
    for path in writable {
        let real = sandbox::real(path);
        if let Some(kept) = kept
            .iter()
            .find(|kept| real.starts_with(kept) || kept.starts_with(&real))
        {
            return Err(ExecutorError::Spec(format!(
                "the harness asks to write {}, which would open {} to a read-only run",
                path.display(),
                kept.display()
            )));
        }
    }
    Ok(())
}

/// Whether nothing is at `file`, not even a link: as `read_result` finds no
/// result through `read_confined`, which a link there would make refused
/// rather than missing.
fn left_nothing(file: &Path) -> bool {
    matches!(fs::symlink_metadata(file), Err(error) if error.kind() == io::ErrorKind::NotFound)
}

/// The warning of a result read from the run's last message, naming the
/// tools the harness refused, if any (OWL-323).
fn from_message_warning(refused: &[String]) -> String {
    let mut warning =
        format!("the role left no {RESULT_PATH}: its result was read from its last message");
    if !refused.is_empty() {
        warning.push_str(&format!(" (the harness refused: {})", refused.join(", ")));
    }
    warning
}

/// The certificate bundle a harness command gets as `SSL_CERT_FILE`: on
/// macOS only, for a read-only run only, when the agent environment names
/// none and the bundle is there. The sandbox closes the Keychain server
/// (`com.apple.SecurityServer`), which Codex's certificate check asks: it
/// then fails with `UnknownIssuer` and retries without end, and with the
/// system's bundle it connects (checked live on 2026-10-15, build plan,
/// "C2"). Elsewhere the environment is left as it is.
fn certificate_bundle(agent: &AgentEnv, read_only: bool) -> Option<&'static Path> {
    let bundle = Path::new(MACOS_CERT_BUNDLE);
    (cfg!(target_os = "macos")
        && read_only
        && agent.var("SSL_CERT_FILE").is_none()
        && bundle.is_file())
    .then_some(bundle)
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
    let violations = gate_violations(report, check);
    if !violations.is_empty() {
        Some(Outcome::Quarantined(violations))
    } else {
        report
            .failure
            .as_ref()
            .map(|failure| Outcome::Failed(Failure::Gate(Box::new(failure.clone()))))
    }
}

/// The isolation violations after a gate: the breach the gate found before
/// its own git, as it is, with no more git; otherwise what `check` finds.
fn gate_violations(report: &GateReport, check: impl FnOnce() -> Vec<Violation>) -> Vec<Violation> {
    if report.breach.is_empty() {
        check()
    } else {
        report.breach.clone()
    }
}

/// The outcome, in order of precedence: a violation, the deadline, a
/// credential the harness loaded, a usage limit, a harness failure, then
/// the result itself, read as `result` (a folder and a name in it), refused
/// when it holds one of `hidden`, the harness's login secrets.
fn decide(
    violations: Vec<Violation>,
    timed_out: bool,
    driven: io::Result<HarnessEnd>,
    spec: &RunSpec<'_>,
    result: (&Path, &str),
    hidden: &[Secret],
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
                    read_result(result, spec.worktree, spec.branch, spec.brief, hidden)
                }
            }
        }
    };
    (outcome, end)
}

/// Reads and validates the role's `result.json`, the file `name` in the
/// folder `root` (the worktree, or the run's files folder under the
/// read-only profile), then its artifacts, in the worktree. A result or an
/// artifact that holds one of `hidden`, the harness's login secrets, fails
/// the run: what it says would reach the tracker, the pull request and the
/// events (OWL-94).
fn read_result(
    (root, name): (&Path, &str),
    worktree: &Path,
    branch: &str,
    brief: &Brief,
    hidden: &[Secret],
) -> Outcome {
    let copied = |into: &str, bytes: &[u8]| {
        hidden.iter().any(|token| holds(bytes, token)).then(|| {
            Outcome::Failed(Failure::Credentials(vec![CredentialFinding::LoginCopied {
                into: into.to_owned(),
            }]))
        })
    };
    let bytes = match read_confined(root, name, MAX_RESULT_BYTES) {
        Ok(bytes) => bytes,
        Err(ConfinedError::Refused {
            reason: Refusal::NotFound,
            ..
        }) => return Outcome::Failed(Failure::NoResult),
        Err(error) => return Outcome::Failed(Failure::InvalidResult(error.to_string())),
    };
    if let Some(refused) = copied(name, &bytes) {
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
/// Once told of the harness's login secrets (`RunLog::hide`), each file gets
/// [`REDACTED`] wherever the harness printed one, even split across two
/// writes: the bytes that could start one are held back until the next
/// write, or `RunLog::finish`. The agent's shell inherits the token, or can
/// read a login copied into the run's home, so a command such as `env` or
/// `cat` would otherwise log it (OWL-94, OWL-299).
pub struct RunLog {
    stdout: File,
    stderr: File,
    stdout_path: PathBuf,
    stderr_path: PathBuf,
    error: Option<String>,
    hidden: Vec<Secret>,
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
            hidden: Vec::new(),
            stdout_held: Vec::new(),
            stderr_held: Vec::new(),
        })
    }

    /// Replaces `token` with [`REDACTED`] in everything logged from now on.
    fn hide(&mut self, token: &Secret) {
        if !token.expose().is_empty() {
            self.hidden.push(token.clone());
        }
    }

    /// Appends bytes of standard output.
    pub fn stdout(&mut self, bytes: &[u8]) {
        let written = redacted_write(&mut self.stdout, &mut self.stdout_held, &self.hidden, bytes);
        self.keep(written, "stdout.log");
    }

    /// Appends bytes of standard error.
    pub fn stderr(&mut self, bytes: &[u8]) {
        let written = redacted_write(&mut self.stderr, &mut self.stderr_held, &self.hidden, bytes);
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

/// Writes `bytes` to `file`, each of `hidden`'s values replaced with
/// [`REDACTED`]. `held` carries the last bytes of the previous write, which
/// could start a secret, and keeps this write's, fewer than the longest
/// secret's length.
fn redacted_write(
    file: &mut File,
    held: &mut Vec<u8>,
    hidden: &[Secret],
    bytes: &[u8],
) -> io::Result<()> {
    let Some(longest) = hidden.iter().map(|secret| secret.expose().len()).max() else {
        return file.write_all(bytes);
    };
    held.extend_from_slice(bytes);
    let mut text = std::mem::take(held);
    for secret in hidden {
        text = replace_all(&text, secret.expose().as_bytes(), REDACTED.as_bytes());
    }
    let split = text.len().saturating_sub(longest - 1);
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

/// The time between two readings of the system clock, `None` when the clock
/// reads earlier at the end: it was set back during the run (OWL-262).
fn wall_span(start: SystemTime, end: SystemTime) -> Option<Duration> {
    end.duration_since(start).ok()
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

    /// OWL-298: the system's bundle reaches a read-only harness command on
    /// macOS alone, when the agent environment names no bundle.
    #[test]
    fn a_read_only_run_on_macos_gets_the_systems_certificate_bundle() {
        let bare = AgentEnv::new([("PATH".into(), "/usr/bin".into())]).unwrap();
        let own = AgentEnv::new([
            ("PATH".into(), "/usr/bin".into()),
            ("SSL_CERT_FILE".into(), "/opt/corp/ca.pem".into()),
        ])
        .unwrap();
        let expected = (cfg!(target_os = "macos") && Path::new(MACOS_CERT_BUNDLE).is_file())
            .then_some(Path::new(MACOS_CERT_BUNDLE));
        assert_eq!(certificate_bundle(&bare, true), expected);
        assert_eq!(certificate_bundle(&bare, false), None);
        assert_eq!(certificate_bundle(&own, true), None);
    }

    /// OWL-298: a harness's writable folder may not hold, nor lie in, the
    /// worktree or the git folder of a read-only run.
    #[test]
    fn a_read_only_run_writes_nothing_that_overlaps_its_worktree() {
        let base = tempfile::tempdir().unwrap();
        let run = RunPaths {
            workdir: base.path().join("wt"),
            git_dir: Some(base.path().join("repo/.git")),
            ..RunPaths::default()
        };
        for path in [
            base.path().to_owned(),
            base.path().join("wt"),
            base.path().join("wt/cache"),
            base.path().join("repo"),
            base.path().join("repo/.git/hooks"),
        ] {
            let refused = read_only_clear(std::slice::from_ref(&path), &run);
            assert!(
                matches!(refused, Err(ExecutorError::Spec(_))),
                "{}: {refused:?}",
                path.display()
            );
        }
        let beside = [base.path().join("temp/run-files"), base.path().join("wt2")];
        assert!(read_only_clear(&beside, &run).is_ok());
    }

    #[test]
    fn the_wall_span_is_none_when_the_clock_was_set_back() {
        let start = SystemTime::UNIX_EPOCH + Duration::from_secs(100);
        let later = start + Duration::from_secs(5);
        assert_eq!(wall_span(start, later), Some(Duration::from_secs(5)));
        assert_eq!(wall_span(start, start), Some(Duration::ZERO));
        assert_eq!(wall_span(later, start), None);
    }

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
        let done = br#"{"format":12,"status":"done","summary":"s","pr":{"branch":"owlshift/T-1","title":"t","body":"b"}}"#;
        assert!(validate_result(done, "owlshift/T-1", &brief).is_ok());

        let other = validate_result(done, "owlshift/T-2", &brief)
            .unwrap_err()
            .to_string();
        assert!(other.contains("pr.branch"), "{other}");
        // A contract rule: questions needs a question.
        let empty = br#"{"format":12,"status":"questions","summary":"s","questions":[]}"#;
        assert!(validate_result(empty, "owlshift/T-1", &brief).is_err());
        // A rule against the brief: verdicts come from the answer check only.
        let verdicts = br#"{"format":12,"status":"done","summary":"s","verdicts":[{"question":"Q1","class":"answered","reason":"r"}]}"#;
        let refused = validate_result(verdicts, "owlshift/T-1", &brief).unwrap_err();
        assert!(
            matches!(&refused, Failure::InvalidResult(reason) if reason.contains("role is build")),
            "{refused:?}"
        );
        // The build role's own decisions are a refusal of their own kind,
        // which its next run is told of (OWL-186).
        let decided = br#"{"format":12,"status":"done","summary":"s","decisions":[{"question":"q","decision":"d","basis":"b"}]}"#;
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
        // Questions past their byte bound are a refusal the retry is told
        // (OWL-213).
        let verbose = format!(
            r#"{{"format":12,"status":"questions","summary":"s","questions":[{{"id":"Q1","category":"scope","context":"{}","text":"t","recommendation":{{"why":"r"}}}}]}}"#,
            "c".repeat(owlshift_contracts::result::MAX_QUESTION_BYTES)
        );
        let refused = validate_result(verbose.as_bytes(), "owlshift/T-1", &brief).unwrap_err();
        assert!(
            matches!(&refused, Failure::InvalidResult(reason)
                if reason.contains("question Q1 weighs") && reason.contains("shorten")),
            "{refused:?}"
        );
        // Unknown fields are refused, and so is text that is not UTF-8.
        let unknown = br#"{"format":12,"status":"done","summary":"s","extra":1}"#;
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
        // A second secret, such as a login copied into the run's home
        // (OWL-299), is hidden too, with its own length.
        log.hide(&Secret::new("refresh-OWL299"));
        log.stderr(b" cat: refresh-OW");
        log.stderr(b"L299.");
        log.finish();
        assert!(!format!("{log:?}").contains("sk-an"));
        let read = |name| std::fs::read_to_string(dir.path().join(name)).unwrap();
        assert_eq!(
            read("stdout.log"),
            "env: A=1 T=<redacted>\nagain <redacted><redacted> end sk-an"
        );
        assert_eq!(read("stderr.log"), "<redacted> cat: <redacted>.");
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
            steps: Vec::new(),
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
