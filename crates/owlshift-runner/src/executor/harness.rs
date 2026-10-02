//! A harness as the executor drives it.
//!
//! The split follows the harness contract (architecture, section 6): a
//! harness builds the command of one run and reads its output; the
//! executor gives the command its directory, its environment and its
//! streams, spawns it in a process tree of its own, logs it, stops it and
//! judges the result. [`ClaudeHarness`] drives Claude Code through
//! `owlshift_adapters::harness::claude`; a harness with no driver of its own
//! can use [`drive_plain`].

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
use owlshift_contracts::brief::Brief;
use owlshift_platform::keychain::Secret;
use owlshift_platform::process::{find_executable_in, hand_over_on_descriptor};

use super::{BRIEF_PATH, RunLog};
use crate::agent_env::{AgentEnv, CredentialFinding, mcp_findings};

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
    /// The brief's file, inside the worktree.
    pub brief_file: &'a Path,
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
    /// must then return.
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
        };
        Ok(claude::command(&self.program, &request)?)
    }

    fn drive(
        &self,
        _run: &HarnessRun<'_>,
        child: &mut Child,
        log: &mut RunLog,
    ) -> io::Result<HarnessEnd> {
        let run = claude::drive(child, &prompt_with_brief(&self.prompt), |line| {
            log.stdout(line);
        })?;
        log.stderr(&run.stderr);
        Ok(claude_end(run))
    }

    /// The folders `claude` is installed in (the one on the `PATH` and the
    /// one its link leads to), read; and its login, the token of `login`
    /// with a configuration folder of the run's own; and, confined, a
    /// temporary folder of the run's own, clear of the user's
    /// `/tmp/claude-<uid>` (OWL-100). A confined run without a token, or with
    /// one that is not one word, is refused with the fix.
    fn sandbox_needs(&self, agent: &AgentEnv) -> Result<SandboxNeeds, HarnessError> {
        let mut needs = SandboxNeeds::default();
        if agent.is_confined() {
            needs.temp_variable = Some(claude::TMPDIR_ENV);
        }
        let program = if self.program.is_absolute() {
            Some(self.program.clone())
        } else {
            find_executable_in(
                &self.program.to_string_lossy(),
                agent.var("PATH").unwrap_or_default(),
            )
        };
        if let Some(program) = program {
            needs
                .readable
                .extend(program.parent().map(Path::to_path_buf));
            if let Ok(real) = fs::canonicalize(&program) {
                needs.readable.extend(real.parent().map(Path::to_path_buf));
            }
        }
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

/// The role's prompt, then where its brief is.
pub fn prompt_with_brief(prompt: &str) -> String {
    format!(
        "{}\n\nThe brief of this run is the file `{BRIEF_PATH}` in the working directory.\n",
        prompt.trim_end()
    )
}

/// What a Claude Code run reported, as the executor reads it: an MCP server
/// in its `init` event is a credential finding.
fn claude_end(run: claude::Run) -> HarnessEnd {
    let status = match &run.outcome {
        claude::Outcome::Completed { .. } => HarnessStatus::Completed,
        claude::Outcome::UsageLimit { resets_at, .. } => HarnessStatus::UsageLimit {
            resets_at: *resets_at,
        },
        claude::Outcome::Failed(failure) => HarnessStatus::Failed(describe(failure)),
    };
    HarnessEnd {
        exit_code: run.exit_code,
        status,
        findings: mcp_findings(&run).into_iter().collect(),
        usage: run.usage,
        model: run.model,
        harness_version: run.harness_version,
    }
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

    #[test]
    fn a_claude_run_is_read_as_the_executor_needs_it() {
        let end = claude_end(run(&[INIT, DONE], 0));
        assert_eq!(end.status, HarnessStatus::Completed);
        assert_eq!(end.findings, []);
        assert_eq!(end.harness_version.as_deref(), Some("2.1.283"));

        let failed = claude_end(run(
            &[
                INIT,
                r#"{"type":"result","subtype":"success","is_error":true,"api_error_status":404,"terminal_reason":"api_error","result":"x"}"#,
            ],
            1,
        ));
        let HarnessStatus::Failed(reason) = failed.status else {
            panic!("{failed:?}");
        };
        assert!(reason.contains("API status 404"), "{reason}");
        assert!(!reason.contains("setup-token"), "{reason}");

        // A refused login names the token's fix.
        let refused = claude_end(run(
            &[
                INIT,
                r#"{"type":"result","subtype":"success","is_error":true,"api_error_status":401,"terminal_reason":"api_error","result":"x"}"#,
            ],
            1,
        ));
        let HarnessStatus::Failed(reason) = refused.status else {
            panic!("{refused:?}");
        };
        assert!(reason.contains("API status 401"), "{reason}");
        assert!(reason.contains("`claude setup-token`"), "{reason}");

        let limited = claude_end(run(
            &[
                INIT,
                r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","resetsAt":1790000000,"rateLimitType":"five_hour"}}"#,
            ],
            1,
        ));
        assert!(
            matches!(
                limited.status,
                HarnessStatus::UsageLimit { resets_at: Some(_) }
            ),
            "{limited:?}"
        );

        // An MCP server in `init` is a finding, even on a completed run.
        let init = r#"{"type":"system","subtype":"init","mcp_servers":[{"name":"claude.ai Linear","status":"connected"}]}"#;
        let loaded = claude_end(run(&[init, DONE], 0));
        assert_eq!(
            loaded.findings,
            [CredentialFinding::McpServers(vec![
                "claude.ai Linear".into()
            ])]
        );
    }

    fn claude(login: Option<&str>) -> ClaudeHarness {
        ClaudeHarness {
            program: PathBuf::from("claude"),
            prompt: String::new(),
            model: None,
            effort: None,
            max_budget_usd: None,
            login: login.map(Secret::new),
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

    #[test]
    fn the_prompt_ends_with_where_the_brief_is() {
        assert_eq!(
            prompt_with_brief("# Build\n\nDo it.\n\n"),
            "# Build\n\nDo it.\n\nThe brief of this run is the file `.owlshift/run/brief.json` \
             in the working directory.\n"
        );
    }
}
