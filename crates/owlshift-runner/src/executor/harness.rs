//! A harness as the executor drives it.
//!
//! The split follows the harness contract (architecture, section 6): a
//! harness builds the command of one run and reads its output; the
//! executor gives the command its directory, its environment and its
//! streams, spawns it in a process tree of its own, logs it, stops it and
//! judges the result. [`ClaudeHarness`] drives Claude Code through
//! `owlshift_adapters::harness::claude`, and `CodexHarness` (in
//! `executor::codex`) drives Codex; a harness with no driver of its own can
//! use [`drive_plain`].

use std::error::Error;
use std::fmt;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use jiff::Timestamp;

use owlshift_adapters::harness::claude::{self, EXIT_GRACE, Effort, STDERR_CAP, Usage};
use owlshift_contracts::brief::{Brief, PermissionLevel};
use owlshift_platform::keychain::Secret;
use owlshift_platform::process::{find_executable_in, hand_over_on_descriptor};

use super::message::last_message_result;
use super::{BRIEF_PATH, RunLog, RunStop};
use crate::agent_env::{AgentEnv, CredentialFinding, billing_finding, mcp_findings};

/// Why a harness could not build its command; the adapter's own error type
/// stays reachable through downcasting.
pub type HarnessError = Box<dyn Error + Send + Sync + 'static>;

/// What a harness knows of the run it builds and drives.
#[derive(Clone, Copy, Debug)]
pub struct HarnessRun<'a> {
    /// The ticket's worktree: the run's working directory.
    pub worktree: &'a Path,
    /// The brief as written, its `result_path` set by the executor.
    pub brief: &'a Brief,
    /// The brief's file: inside the worktree, or, under the read-only
    /// profile ([`SandboxNeeds::read_only`]), in the run's files folder.
    pub brief_file: &'a Path,
    /// The file the role's result is read from once the run ends: the
    /// brief's `result_path` in the worktree, or, under the read-only
    /// profile, `result.json` in the run's files folder
    /// ([`RUN_FILES_DIR`](super::RUN_FILES_DIR)), where the brief's
    /// `result_path` does not apply. That folder, in the run's own temporary
    /// folder, is read and written by the run and removed with it: a
    /// read-only harness keeps there the files of its own the run needs,
    /// made by [`Harness::command`].
    pub result_file: &'a Path,
    /// Stops the run's whole process tree from [`Harness::drive`], before
    /// the deadline: `None` while [`Harness::command`] builds the command,
    /// which comes before the tree; `Some` while the run is driven. A run
    /// stopped so is judged by the harness's end, never as timed out
    /// (OWL-319).
    pub stop: Option<&'a RunStop>,
}

/// A harness CLI, or a stand-in for one.
pub trait Harness {
    /// The command of one run: its program and arguments. The executor then
    /// sets its working directory to the worktree, replaces its whole
    /// environment with the agent's, and pipes its three standard streams.
    fn command(&self, run: &HarnessRun<'_>) -> Result<Command, HarnessError>;

    /// Reads the spawned child's output to its end, passing every byte to
    /// `log` as it comes, and says how the run ended. The executor stops the
    /// child's process tree at the deadline, which closes its output: this
    /// must then return. It may stop the tree sooner with
    /// [`HarnessRun::stop`].
    fn drive(
        &self,
        run: &HarnessRun<'_>,
        child: &mut Child,
        log: &mut RunLog,
    ) -> io::Result<HarnessEnd>;

    /// What the harness needs inside the sandbox besides the worktree and
    /// the repository's git folder, or why it cannot run confined, such as
    /// no login for agent runs (OWL-94). Nothing more by default. It only
    /// looks: it is also asked before anything is cloned, to refuse early.
    fn sandbox_needs(&self, agent: &AgentEnv) -> Result<SandboxNeeds, HarnessError> {
        let _ = agent;
        Ok(SandboxNeeds::default())
    }

    /// The secrets a copy of the user's login holds ([`SandboxNeeds::home`]),
    /// given its bytes: hidden in the run's logs and refused in its result,
    /// as a login token is. None by default.
    fn login_secrets(&self, login: &[u8]) -> Vec<Secret> {
        let _ = login;
        Vec::new()
    }

    /// Whether the run's copy of the user's login, which it changed, may
    /// replace the user's file ([`SandboxNeeds::home`]): `Err` says why not.
    /// Never, by default.
    fn login_kept(&self, original: &[u8], updated: &[u8]) -> Result<(), String> {
        let _ = (original, updated);
        Err("this harness writes no login back".to_owned())
    }
}

/// What a harness needs inside the sandbox.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SandboxNeeds {
    /// Folders read, such as where the harness is installed.
    pub readable: Vec<PathBuf>,
    /// Folders read and written.
    pub writable: Vec<PathBuf>,
    /// The login the harness runs on; `None`: it needs none from the runner.
    pub login: Option<HarnessLogin>,
    /// The variable naming a temporary folder of the harness's own, such as
    /// `CLAUDE_CODE_TMPDIR`. The executor sets it on the harness command
    /// alone to [`HARNESS_TEMP_DIR`](super::HARNESS_TEMP_DIR) in the run's own
    /// temporary folder, which the harness makes itself: nothing more is
    /// opened in the sandbox (OWL-100).
    pub temp_variable: Option<&'static str>,
    /// Whether the run gets the read-only profile (OWL-298, architecture
    /// section 5, "The Codex harness"): the worktree and the repository's
    /// git folder are read, never written, and the brief and the result
    /// live in the run's own temporary folder ([`HarnessRun::result_file`]).
    /// Only a brief whose permissions are read-only runs so, and no folder
    /// of [`SandboxNeeds::writable`] may hold, or lie in, the worktree or
    /// the git folder. On macOS the harness command also gets the system's
    /// certificate bundle when the agent environment names none
    /// (`SSL_CERT_FILE`): the sandbox closes the Keychain server, which some
    /// certificate checks ask.
    pub read_only: bool,
    /// A home folder of the harness's own for this run, holding only a copy
    /// of the user's login file (architecture section 5, "The Codex
    /// harness"). The executor makes it in the run's own files, bound read
    /// and written, copies the login in, and sets the variable on the
    /// harness command alone; once the run's processes are stopped, it brings
    /// a changed copy back under [`Harness::login_kept`]. Read-only runs
    /// only. Not [`HarnessLogin::config_variable`]'s folder, which is empty
    /// and serves a token handed over on a descriptor.
    pub home: Option<HarnessHome>,
    /// The harness cannot write the result: the run's last message is the
    /// result ([`HarnessEnd::message`]), which the executor writes as the
    /// result file once the run's processes are stopped, over whatever the
    /// run left there. Read-only runs only.
    pub result_in_message: bool,
}

/// A harness's home folder of one run ([`SandboxNeeds::home`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HarnessHome {
    /// The variable naming it, such as `CODEX_HOME`.
    pub variable: &'static str,
    /// The user's login file, copied into it under the same name.
    pub login: PathBuf,
}

/// A harness's login for agent runs (OWL-94). The executor gives it to the
/// harness command alone, right before the spawn ([`HarnessLogin::apply`]):
/// never in the agent environment, so a gate command and the credential
/// probes never get it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HarnessLogin {
    /// The variable naming the descriptor the harness reads its token from,
    /// such as `CLAUDE_CODE_OAUTH_TOKEN_FILE_DESCRIPTOR` (OWL-96).
    pub descriptor_variable: &'static str,
    /// The token; its `Debug` output is redacted.
    pub token: Secret,
    /// The variable that names the harness's configuration folder, such as
    /// `CLAUDE_CONFIG_DIR`. The executor makes the folder, empty, in the
    /// run's own temporary folder, so the harness finds no other login there
    /// and nothing it writes outlives the run.
    pub config_variable: &'static str,
}

impl HarnessLogin {
    /// Gives `command` this login: the token on a pipe its child inherits,
    /// the variable naming that descriptor, and `config` as the harness's
    /// configuration folder. The token is in no environment and no argument
    /// ([`hand_over_on_descriptor`]). Drop `command` right after the spawn:
    /// it holds the runner's copy of the pipe until then. The error never
    /// holds the token.
    pub fn apply(&self, command: &mut Command, config: &Path) -> io::Result<()> {
        let descriptor = hand_over_on_descriptor(command, self.token.expose().as_bytes())?;
        command
            .env(self.descriptor_variable, descriptor.to_string())
            .env(self.config_variable, config);
        Ok(())
    }
}

/// The keychain account, under service `owlshift`, of the Claude Code token
/// agent runs log in with.
pub const CLAUDE_AGENT_ACCOUNT: &str = "claude-agent";

/// How to give agent runs their Claude Code login, or a new one.
pub const AGENT_LOGIN_FIX: &str = "run `claude setup-token`, then `owlshift init` and paste \
     the token it printed; `owlshift init --replace-secrets` replaces a stored token that \
     expired, was revoked or was pasted wrong";

/// Claude Code has no usable login for agent runs: the run is refused with
/// the fix. Neither variant carries the token.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoAgentLogin {
    /// No token is stored.
    Missing,
    /// The stored token is not one word: blank, or holding a space or a
    /// control character.
    Malformed,
}

impl fmt::Display for NoAgentLogin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing => write!(
                f,
                "Claude Code has no login for agent runs. Agent runs are confined and cannot \
                 reach the Keychain, so they log in with a long-lived token of your \
                 subscription, kept by Owlshift in the system keychain (service `owlshift`, \
                 account `{CLAUDE_AGENT_ACCOUNT}`): {AGENT_LOGIN_FIX}"
            ),
            Self::Malformed => write!(
                f,
                "the Claude Code token for agent runs in the system keychain (service \
                 `owlshift`, account `{CLAUDE_AGENT_ACCOUNT}`) is blank or holds a space or a \
                 control character: {AGENT_LOGIN_FIX}"
            ),
        }
    }
}

impl Error for NoAgentLogin {}

/// What a harness reported at the end of a run.
#[derive(Clone, Debug, PartialEq)]
pub struct HarnessEnd {
    /// The exit code; `None` when a signal ended the process.
    pub exit_code: Option<i32>,
    pub status: HarnessStatus,
    /// Credentials the harness loaded, such as MCP servers: they fail the
    /// run.
    pub findings: Vec<CredentialFinding>,
    pub usage: Option<Usage>,
    pub model: Option<String>,
    pub harness_version: Option<String>,
    /// The run's last message, when the harness gives its result that way
    /// ([`SandboxNeeds::result_in_message`]), or, for a read-only run, when
    /// the result may be read from it should the role leave no result file
    /// (Claude Code, OWL-323).
    pub message: Option<String>,
    /// The tools the harness's permissions refused, each once, in the order
    /// of their first refusal; empty when it reports none.
    pub permission_denials: Vec<String>,
}

impl HarnessEnd {
    /// An end with nothing reported beyond the exit code and the status.
    pub fn new(exit_code: Option<i32>, status: HarnessStatus) -> Self {
        Self {
            exit_code,
            status,
            findings: Vec::new(),
            usage: None,
            model: None,
            harness_version: None,
            message: None,
            permission_denials: Vec::new(),
        }
    }
}

/// How the harness process ended, as the harness itself tells: the role's
/// own answer is still its `result.json`.
#[derive(Clone, Debug, PartialEq)]
pub enum HarnessStatus {
    Completed,
    /// The subscription's usage limit stopped the run.
    UsageLimit {
        resets_at: Option<Timestamp>,
    },
    /// An error, a crash, a non-zero exit: the reason.
    Failed(String),
}

/// How long [`drive_plain`] waits between two looks at the child.
const POLL: Duration = Duration::from_millis(20);

/// What [`drive_plain`] saw.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlainRun {
    /// The exit code; `None` when a signal ended the process.
    pub exit_code: Option<i32>,
    /// Standard error, up to the adapter's `STDERR_CAP` bytes.
    pub stderr: Vec<u8>,
}

enum Chunk {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
}

/// Drives a child that needs no input: closes its standard input, logs its
/// output as it comes and waits for it. Like the Claude Code adapter's
/// `drive`, it returns at most `EXIT_GRACE` after the child exits, even if
/// a process the child started still holds its output open.
pub fn drive_plain(child: &mut Child, log: &mut RunLog) -> io::Result<PlainRun> {
    drop(child.stdin.take());
    let not_piped = || {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "the harness child needs piped output streams",
        )
    };
    let (sender, chunks) = mpsc::channel();
    pump(
        child.stdout.take().ok_or_else(not_piped)?,
        sender.clone(),
        Chunk::Stdout,
    );
    pump(
        child.stderr.take().ok_or_else(not_piped)?,
        sender,
        Chunk::Stderr,
    );

    let mut stderr = Vec::new();
    let mut exited_at = None;
    loop {
        match chunks.recv_timeout(POLL) {
            Ok(Chunk::Stdout(bytes)) => log.stdout(&bytes),
            Ok(Chunk::Stderr(bytes)) => {
                log.stderr(&bytes);
                let room = STDERR_CAP - stderr.len();
                stderr.extend_from_slice(&bytes[..bytes.len().min(room)]);
            }
            Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {}
        }
        if exited_at.is_none() && child.try_wait()?.is_some() {
            exited_at = Some(Instant::now());
        }
        if exited_at.is_some_and(|at| at.elapsed() >= EXIT_GRACE) {
            break;
        }
    }
    let status = child.wait()?;
    Ok(PlainRun {
        exit_code: status.code(),
        stderr,
    })
}

/// Reads a stream to its end on its own thread, sending what it reads.
fn pump(mut stream: impl Read + Send + 'static, sender: Sender<Chunk>, wrap: fn(Vec<u8>) -> Chunk) {
    thread::spawn(move || {
        let mut buffer = [0u8; 8192];
        loop {
            match stream.read(&mut buffer) {
                // A read error ends the stream as its end would.
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if sender.send(wrap(buffer[..n].to_vec())).is_err() {
                        break;
                    }
                }
            }
        }
    });
}

/// Claude Code, run with `claude -p`: confined, on the token of `login`;
/// bare (the test bench), on the user's own login unless a token is given.
#[derive(Clone, Debug, PartialEq)]
pub struct ClaudeHarness {
    /// The `claude` program.
    pub program: PathBuf,
    /// The role's prompt; the executor adds where the brief is.
    pub prompt: String,
    pub model: Option<String>,
    pub effort: Option<Effort>,
    /// A dollar cap on each run, passed as `--max-budget-usd`: the personal
    /// file's `budget_usd`, set whatever the billing.
    pub max_budget_usd: Option<f64>,
    /// The token agent runs log in with, made by `claude setup-token` and
    /// read by the runner from the keychain ([`CLAUDE_AGENT_ACCOUNT`]).
    pub login: Option<Secret>,
    /// The resolver's run (OWL-344): bare Bash allowed
    /// ([`claude::Request::bash`]), inside the read-only profile, which
    /// bounds what its commands read and write, the result given as the
    /// run's last message.
    pub bash: bool,
}

impl Harness for ClaudeHarness {
    fn command(&self, run: &HarnessRun<'_>) -> Result<Command, HarnessError> {
        let request = claude::Request {
            workdir: run.worktree.to_owned(),
            model: self.model.clone(),
            effort: self.effort,
            permissions: run.brief.permissions.clone(),
            result_path: run.brief.result_path.clone(),
            json_schema: None,
            max_budget_usd: self.max_budget_usd,
            bash: self.bash,
        };
        Ok(claude::command(&self.program, &request)?)
    }

    /// Drives `claude -p`; a read-only run's last message comes with its
    /// end, for the executor to read the result from when the role left no
    /// result file (OWL-323), or, with [`ClaudeHarness::bash`], as its
    /// result. A run with Bash is told where its brief is in the run's own
    /// files, and to give its result as its last message.
    fn drive(
        &self,
        run: &HarnessRun<'_>,
        child: &mut Child,
        log: &mut RunLog,
    ) -> io::Result<HarnessEnd> {
        let ran = claude::drive(child, &self.prompt_with(run.brief_file), |line| {
            log.stdout(line);
        })?;
        log.stderr(&ran.stderr);
        Ok(claude_end(ran, run.brief.permissions.level, self.bash))
    }

    /// The folders `claude` is installed in (the one on the `PATH` and the
    /// one its link leads to), read; and its login, the token of `login`
    /// with a configuration folder of the run's own; and, confined, a
    /// temporary folder of the run's own, clear of the user's
    /// `/tmp/claude-<uid>` (OWL-100). A confined run without a token, or with
    /// one that is not one word, is refused with the fix. With
    /// [`ClaudeHarness::bash`], the read-only profile and the result from
    /// the last message (OWL-344).
    fn sandbox_needs(&self, agent: &AgentEnv) -> Result<SandboxNeeds, HarnessError> {
        let mut needs = SandboxNeeds {
            read_only: self.bash,
            result_in_message: self.bash,
            ..SandboxNeeds::default()
        };
        if agent.is_confined() {
            needs.temp_variable = Some(claude::TMPDIR_ENV);
        }
        needs.readable = program_folders(&self.program, agent);
        match &self.login {
            Some(token) if !token.is_one_word() => return Err(NoAgentLogin::Malformed.into()),
            Some(token) => {
                needs.login = Some(HarnessLogin {
                    descriptor_variable: claude::LOGIN_TOKEN_FD_ENV,
                    token: token.clone(),
                    config_variable: claude::CONFIG_DIR_ENV,
                });
            }
            None if agent.is_confined() => return Err(NoAgentLogin::Missing.into()),
            None => {}
        }
        Ok(needs)
    }
}

impl ClaudeHarness {
    /// The role's prompt, then where its brief is: `brief_file`, in the
    /// run's own files, for a run with Bash, which gives its result as its
    /// last message; [`BRIEF_PATH`] in the worktree otherwise.
    fn prompt_with(&self, brief_file: &Path) -> String {
        if self.bash {
            prompt_with_brief_file(&self.prompt, brief_file, "")
        } else {
            prompt_with_brief(&self.prompt)
        }
    }
}

/// The folders a harness program is installed in, read: the one on the
/// agent's `PATH` (or of its absolute path) and the one its link leads to.
pub(super) fn program_folders(program: &Path, agent: &AgentEnv) -> Vec<PathBuf> {
    let mut folders = Vec::new();
    let found = if program.is_absolute() {
        Some(program.to_owned())
    } else {
        find_executable_in(
            &program.to_string_lossy(),
            agent.var("PATH").unwrap_or_default(),
        )
    };
    if let Some(found) = found {
        folders.extend(found.parent().map(Path::to_path_buf));
        if let Ok(real) = fs::canonicalize(&found) {
            folders.extend(real.parent().map(Path::to_path_buf));
        }
    }
    folders
}

/// The role's prompt, then where its brief is.
pub fn prompt_with_brief(prompt: &str) -> String {
    format!(
        "{}\n\nThe brief of this run is the file `{BRIEF_PATH}` in the working directory.\n",
        prompt.trim_end()
    )
}

/// The role's prompt, then where its brief is and how the run ends: a run
/// that cannot write `result.json` gives it as its last message, `how`
/// saying more of that message's shape (Codex's output schema), or nothing.
pub fn prompt_with_brief_file(prompt: &str, brief_file: &Path, how: &str) -> String {
    format!(
        "{}\n\nThe brief of this run is the file `{}`. This harness gives you no way to write \
         `result.json`: do not write it, and make your last message that JSON, whole{how}, with \
         nothing after it.\n",
        prompt.trim_end(),
        brief_file.display()
    )
}

/// What a Claude Code run at `level` reported, as the executor reads it: an
/// MCP server or an API-key source in its `init` event is a credential
/// finding. The final message of a completed read-only run comes along, and
/// the refused tools always. With `in_message`, a run whose result is its
/// last message ([`ClaudeHarness::bash`], OWL-344), that message is read as
/// the executor reads a read-only run's message (OWL-323): bare JSON, or the
/// body of a closing fenced block after prose; any other message comes whole,
/// for the result's validation to refuse.
///
/// A run that completed in another permission mode than the one asked,
/// as its `init` event reports it, failed (OWL-346): under managed settings
/// that disable bypass mode, a write-in-worktree run falls back to `default`
/// and every edit is refused, with exit status 0 and no other sign. A run
/// that failed keeps its own reason, and a mode the run did not report is
/// not judged. A failed write-in-worktree run that Claude Code refused to
/// start as root says so (`ROOT_REFUSAL`).
fn claude_end(run: claude::Run, level: PermissionLevel, in_message: bool) -> HarnessEnd {
    let read_only = level == PermissionLevel::ReadOnly;
    let mut permission_denials: Vec<String> = Vec::new();
    for tool in &run.permission_denials {
        if !permission_denials.contains(tool) {
            permission_denials.push(tool.clone());
        }
    }
    let message = match &run.outcome {
        claude::Outcome::Completed { text } if read_only && in_message => {
            Some(last_message_result(text).unwrap_or(text).to_owned())
        }
        claude::Outcome::Completed { text } if read_only => Some(text.clone()),
        _ => None,
    };
    let asked = claude::permission_mode(level);
    let status = match &run.outcome {
        claude::Outcome::Completed { .. } => match run.permission_mode.as_deref() {
            Some(ran) if ran != asked => HarnessStatus::Failed(mode_changed(ran, asked)),
            _ => HarnessStatus::Completed,
        },
        claude::Outcome::UsageLimit { resets_at, .. } => HarnessStatus::UsageLimit {
            resets_at: *resets_at,
        },
        claude::Outcome::Failed(failure) => {
            let mut reason = describe(failure);
            if level == PermissionLevel::WriteWorktree
                && String::from_utf8_lossy(&run.stderr).contains(ROOT_REFUSAL)
            {
                reason.push_str(&format!(
                    ": Claude Code refuses the `{asked}` mode of a write-in-worktree run to a \
                     process running as root; run Owlshift as a normal user"
                ));
            }
            HarnessStatus::Failed(reason)
        }
    };
    HarnessEnd {
        exit_code: run.exit_code,
        status,
        findings: mcp_findings(&run)
            .into_iter()
            .chain(billing_finding(&run))
            .collect(),
        usage: run.usage,
        model: run.model,
        harness_version: run.harness_version,
        message,
        permission_denials,
    }
}

/// What Claude Code prints on standard error, then exits 1 before any
/// event, when asked for `bypassPermissions` by a process running as root
/// outside a sandbox it recognises (read in the 2.1.302 binary, OWL-346).
const ROOT_REFUSAL: &str = "cannot be used with root/sudo privileges";

/// The reason of a run that completed in mode `ran` when `asked` was asked.
fn mode_changed(ran: &str, asked: &str) -> String {
    let mut reason = format!(
        "Claude Code ran in permission mode `{ran}`, not `{asked}` as Owlshift asked: a setting \
         outside Owlshift's command line changed it, such as the machine's managed settings"
    );
    if asked == claude::permission_mode(PermissionLevel::WriteWorktree) {
        reason.push_str(
            "; `permissions.disableBypassPermissionsMode` set to `disable` does this, and a \
             write-in-worktree run can then edit nothing",
        );
    }
    reason
}

fn describe(failure: &claude::Failure) -> String {
    match failure {
        claude::Failure::NoResult => "Claude Code left no final record".to_owned(),
        claude::Failure::MalformedResult => {
            "Claude Code's final record has no error flag".to_owned()
        }
        claude::Failure::Signal => "a signal ended Claude Code".to_owned(),
        claude::Failure::PromptNotDelivered => "the prompt could not be sent".to_owned(),
        claude::Failure::Error {
            exit_code,
            api_error_status,
            terminal_reason,
            kind,
            ..
        } => {
            let mut text = format!("Claude Code reported an error (exit code {exit_code:?}");
            if let Some(status) = api_error_status {
                text.push_str(&format!(", API status {status}"));
            }
            if let Some(reason) = terminal_reason {
                text.push_str(&format!(", {reason}"));
            }
            if let Some(kind) = kind {
                text.push_str(&format!(", {kind}"));
            }
            text.push(')');
            if *api_error_status == Some(401) {
                text.push_str(&format!(
                    ": the API refused the login; if the token of agent runs expired or was \
                     revoked, {AGENT_LOGIN_FIX}"
                ));
            }
            text
        }
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::*;

    fn run(lines: &[&str], exit_code: i32) -> claude::Run {
        let mut transcript = claude::Transcript::default();
        for line in lines {
            transcript.feed(line.as_bytes());
        }
        transcript.finish(Some(exit_code), Vec::new())
    }

    const INIT: &str = r#"{"type":"system","subtype":"init","claude_code_version":"2.1.283","model":"claude-haiku","apiKeySource":"none","mcp_servers":[]}"#;
    const DONE: &str = r#"{"type":"result","subtype":"success","is_error":false,"result":"ok"}"#;

    /// OWL-344: a run whose result is its last message gives bare JSON,
    /// trimmed, or the body of a closing fenced block after prose, and any
    /// other message whole; without the flag, a read-only run's message
    /// comes whole, for the OWL-323 tolerance to read.
    #[test]
    fn a_bash_run_gives_the_json_of_its_last_message() {
        let message = |text: &str, in_message| {
            let line = serde_json::json!({
                "type": "result", "subtype": "success", "is_error": false, "result": text,
            })
            .to_string();
            claude_end(
                run(&[INIT, &line], 0),
                PermissionLevel::ReadOnly,
                in_message,
            )
            .message
        };
        let result = r#"{"status":"blocked"}"#;
        let block = format!("I checked the history.\n\n```json\n{result}\n```\n");

        assert_eq!(message(&format!("\n{result}\n"), true), Some(result.into()));
        assert_eq!(message(&block, true), Some(format!("{result}\n")));
        assert_eq!(
            message("I could not decide.", true),
            Some("I could not decide.".into())
        );
        assert_eq!(message(&block, false), Some(block.clone()));
    }

    #[test]
    fn a_claude_run_is_read_as_the_executor_needs_it() {
        let end = claude_end(run(&[INIT, DONE], 0), PermissionLevel::WriteWorktree, false);
        assert_eq!(end.status, HarnessStatus::Completed);
        assert_eq!(end.findings, []);
        assert_eq!(end.harness_version.as_deref(), Some("2.1.283"));
        assert_eq!(end.message, None, "a write run's message is not read");

        // OWL-323: a completed read-only run gives its final message, and
        // the tools refused, each once, in the order of their first refusal.
        let denied = r#"{"type":"result","is_error":false,"result":"ok","permission_denials":[{"tool_name":"Bash"},{"tool_name":"Write"},{"tool_name":"Bash"}]}"#;
        let read_only = claude_end(run(&[INIT, denied], 0), PermissionLevel::ReadOnly, false);
        assert_eq!(read_only.message.as_deref(), Some("ok"));
        assert_eq!(read_only.permission_denials, ["Bash", "Write"]);

        let failed = claude_end(
            run(
                &[
                    INIT,
                    r#"{"type":"result","subtype":"success","is_error":true,"api_error_status":404,"terminal_reason":"api_error","result":"x"}"#,
                ],
                1,
            ),
            PermissionLevel::ReadOnly,
            false,
        );
        assert_eq!(failed.message, None, "a failed run gives no result");
        let HarnessStatus::Failed(reason) = failed.status else {
            panic!("{failed:?}");
        };
        assert!(reason.contains("API status 404"), "{reason}");
        assert!(!reason.contains("setup-token"), "{reason}");

        // A refused login names the token's fix.
        let refused = claude_end(
            run(
                &[
                    INIT,
                    r#"{"type":"result","subtype":"success","is_error":true,"api_error_status":401,"terminal_reason":"api_error","result":"x"}"#,
                ],
                1,
            ),
            PermissionLevel::WriteWorktree,
            false,
        );
        let HarnessStatus::Failed(reason) = refused.status else {
            panic!("{refused:?}");
        };
        assert!(reason.contains("API status 401"), "{reason}");
        assert!(reason.contains("`claude setup-token`"), "{reason}");

        let limited = claude_end(
            run(
                &[
                    INIT,
                    r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","resetsAt":1790000000,"rateLimitType":"five_hour"}}"#,
                ],
                1,
            ),
            PermissionLevel::WriteWorktree,
            false,
        );
        assert!(
            matches!(
                limited.status,
                HarnessStatus::UsageLimit { resets_at: Some(_) }
            ),
            "{limited:?}"
        );

        // An MCP server in `init` is a finding, even on a completed run.
        let init = r#"{"type":"system","subtype":"init","mcp_servers":[{"name":"claude.ai Linear","status":"connected"}]}"#;
        let loaded = claude_end(run(&[init, DONE], 0), PermissionLevel::WriteWorktree, false);
        assert_eq!(
            loaded.findings,
            [CredentialFinding::McpServers(vec![
                "claude.ai Linear".into()
            ])]
        );

        // OWL-120: an API key, such as one managed settings hand over, is a
        // finding too; a run that does not say has none.
        let init =
            r#"{"type":"system","subtype":"init","mcp_servers":[],"apiKeySource":"apiKeyHelper"}"#;
        let keyed = claude_end(run(&[init, DONE], 0), PermissionLevel::WriteWorktree, false);
        assert_eq!(
            keyed.findings,
            [CredentialFinding::ApiKey {
                source: "apiKeyHelper".into()
            }]
        );
        let text = keyed.findings[0].to_string();
        assert!(
            text.contains("apiKeyHelper") && text.contains("managed settings"),
            "{text}"
        );
        let silent = r#"{"type":"system","subtype":"init","mcp_servers":[]}"#;
        assert_eq!(
            claude_end(
                run(&[silent, DONE], 0),
                PermissionLevel::WriteWorktree,
                false
            )
            .findings,
            []
        );
    }

    /// OWL-346: a run that completed in another mode than the one asked
    /// failed, saying which setting does that; a failed run keeps its own
    /// reason; a write-in-worktree run refused as root says so.
    #[test]
    fn a_run_in_another_permission_mode_than_asked_fails() {
        let init = |mode: &str| {
            format!(
                r#"{{"type":"system","subtype":"init","mcp_servers":[],"apiKeySource":"none","permissionMode":"{mode}"}}"#
            )
        };
        let end = |mode: &str, level| claude_end(run(&[&init(mode), DONE], 0), level, false);
        let write = PermissionLevel::WriteWorktree;

        // As recorded on 2026-10-18 under managed-style settings disabling
        // bypass mode: exit status 0, `init` reporting `default`.
        let HarnessStatus::Failed(reason) = end("default", write).status else {
            panic!("a write run in `default` mode completed");
        };
        assert!(
            reason.contains("`default`")
                && reason.contains("`bypassPermissions`")
                && reason.contains("disableBypassPermissionsMode"),
            "{reason}"
        );
        assert_eq!(
            end("bypassPermissions", write).status,
            HarnessStatus::Completed
        );
        let HarnessStatus::Failed(reason) =
            end("bypassPermissions", PermissionLevel::ReadOnly).status
        else {
            panic!("a read-only run in bypass mode completed");
        };
        assert!(
            reason.contains("`dontAsk`") && !reason.contains("disableBypassPermissionsMode"),
            "{reason}"
        );

        let refused = r#"{"type":"result","is_error":true,"api_error_status":401,"result":"x"}"#;
        let HarnessStatus::Failed(reason) =
            claude_end(run(&[&init("default"), refused], 1), write, false).status
        else {
            panic!("a failed run completed");
        };
        assert!(
            reason.contains("API status 401") && !reason.contains("permission mode"),
            "{reason}"
        );

        let stderr = b"--dangerously-skip-permissions cannot be used with root/sudo privileges \
                       for security reasons\n";
        let as_root = |level| {
            let run = claude::Transcript::default().finish(Some(1), stderr.to_vec());
            claude_end(run, level, false).status
        };
        let HarnessStatus::Failed(reason) = as_root(write) else {
            panic!("a run that never started completed");
        };
        assert!(
            reason.contains("as root") && reason.contains("normal user"),
            "{reason}"
        );
        let HarnessStatus::Failed(reason) = as_root(PermissionLevel::ReadOnly) else {
            panic!("a run that never started completed");
        };
        assert!(!reason.contains("as root"), "{reason}");
    }

    fn claude(login: Option<&str>) -> ClaudeHarness {
        ClaudeHarness {
            program: PathBuf::from("claude"),
            prompt: String::new(),
            model: None,
            effort: None,
            max_budget_usd: None,
            login: login.map(Secret::new),
            bash: false,
        }
    }

    /// OWL-94: a confined run needs the token for agent runs, and is
    /// refused with the fix when there is none or it is not one word; with
    /// one, it logs in with it, in a configuration folder of the run's own.
    /// Neither the refusal nor the needs show the token. The bench's bare
    /// runs need none.
    #[test]
    fn a_confined_claude_run_needs_its_own_login() {
        const TOKEN: &str = "sk-ant-oat01-SENTINEL_owl94";
        let parent = [(OsString::from("PATH"), OsString::from("/usr/bin"))];
        let agent = AgentEnv::new(parent.clone()).unwrap();
        for (login, refusal) in [
            (None, "Claude Code has no login for agent runs"),
            (Some(" "), "is blank or holds a space"),
            (
                Some("sk-ant-oat01-SENTINEL\nowl94"),
                "is blank or holds a space",
            ),
        ] {
            let text = claude(login).sandbox_needs(&agent).unwrap_err().to_string();
            assert!(text.contains(refusal), "{text}");
            assert!(text.contains("`claude setup-token`"), "{text}");
            assert!(text.contains("`owlshift init --replace-secrets`"), "{text}");
            assert!(!text.contains("SENTINEL"), "{text}");
        }

        let needs = claude(Some(TOKEN)).sandbox_needs(&agent).unwrap();
        assert_eq!(
            needs.login,
            Some(HarnessLogin {
                descriptor_variable: "CLAUDE_CODE_OAUTH_TOKEN_FILE_DESCRIPTOR",
                token: Secret::new(TOKEN),
                config_variable: "CLAUDE_CONFIG_DIR",
            })
        );
        assert_eq!(needs.temp_variable, Some("CLAUDE_CODE_TMPDIR"));
        assert!(needs.writable.is_empty());
        assert!(!format!("{needs:?}").contains("SENTINEL"));

        let bare = AgentEnv::new(parent).unwrap().without_confinement();
        let needs = claude(None).sandbox_needs(&bare).unwrap();
        assert_eq!((needs.login, needs.temp_variable), (None, None));
    }

    /// OWL-344: with Bash, the read-only profile and the result from the
    /// last message, bare `Bash` allowed and no result rule, and a prompt
    /// naming the brief in the run's own files that mentions no schema;
    /// without it, the needs, the rule and the prompt as before.
    #[test]
    fn a_bash_run_is_read_only_and_gives_its_result_as_its_last_message() {
        let agent = AgentEnv::new([(OsString::from("PATH"), OsString::from("/usr/bin"))])
            .unwrap()
            .without_confinement();
        let brief = Brief::parse(&format!(
            r#"{{"format":{},"role":"intake","project":"p",
                "ticket":{{"id":"T-1","title":"t","author":{{"name":"a","relation":"decider"}},"description":"d"}},
                "decider":"a","permissions":{{"level":"read_only","network":false,"browser":false}},
                "gate":[],"always_human":[],"result_path":"result.json"}}"#,
            owlshift_contracts::format::BRIEF_FORMAT
        ))
        .unwrap();
        let brief_file = Path::new("/tmp/run/run-files/brief.json");
        let run = HarnessRun {
            worktree: Path::new("/work"),
            brief: &brief,
            brief_file,
            result_file: Path::new("/tmp/run/run-files/result.json"),
            stop: None,
        };
        let allowed = |harness: &ClaudeHarness| {
            let command = harness.command(&run).unwrap();
            let args: Vec<String> = command
                .get_args()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect();
            let at = args.iter().position(|arg| arg == "--allowedTools").unwrap();
            args[at + 1].clone()
        };

        let mut harness = claude(None);
        let needs = harness.sandbox_needs(&agent).unwrap();
        assert!(!needs.read_only && !needs.result_in_message, "{needs:?}");
        assert_eq!(allowed(&harness), "Edit(/result.json)");
        assert_eq!(harness.prompt_with(brief_file), prompt_with_brief(""));

        harness.bash = true;
        let needs = harness.sandbox_needs(&agent).unwrap();
        assert!(needs.read_only && needs.result_in_message, "{needs:?}");
        assert_eq!(allowed(&harness), "Bash");
        let prompt = harness.prompt_with(brief_file);
        assert!(
            prompt.contains("The brief of this run is the file `/tmp/run/run-files/brief.json`.")
                && !prompt.contains(BRIEF_PATH),
            "{prompt}"
        );
        assert!(
            prompt.contains("do not write it, and make your last message that JSON, whole, with")
                && !prompt.contains("schema"),
            "{prompt}"
        );
    }

    /// Codex's prompt is the shared one with its schema's words, as it was
    /// before the text moved here (OWL-344).
    #[test]
    fn the_codex_prompt_keeps_its_text() {
        assert_eq!(
            super::super::codex::prompt_with_brief_file("# Verify\n\n", Path::new("/tmp/b.json")),
            "# Verify\n\nThe brief of this run is the file `/tmp/b.json`. This harness gives you \
             no way to write `result.json`: do not write it, and make your last message that \
             JSON, whole, as the output schema describes, with nothing after it.\n"
        );
    }

    #[test]
    fn the_prompt_ends_with_where_the_brief_is() {
        assert_eq!(
            prompt_with_brief("# Build\n\nDo it.\n\n"),
            "# Build\n\nDo it.\n\nThe brief of this run is the file `.owlshift/run/brief.json` \
             in the working directory.\n"
        );
    }
}
