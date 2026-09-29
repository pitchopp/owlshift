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
use owlshift_platform::process::find_executable_in;

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
    /// no login made for agent runs (OWL-41). Nothing more by default.
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
    /// Folders read and written, such as the harness's login folder.
    pub writable: Vec<PathBuf>,
}

/// Claude Code has no login for agent runs in its folder: the run is refused
/// with the command that makes one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NoAgentLogin {
    /// The login folder, `CLAUDE_CONFIG_DIR` of the agent; `None` when the
    /// agent has none.
    pub dir: Option<PathBuf>,
}

impl fmt::Display for NoAgentLogin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.dir {
            None => f.write_str(
                "agent runs are confined, and the agent environment names no Claude Code \
                 login folder (CLAUDE_CONFIG_DIR)",
            ),
            Some(dir) => write!(
                f,
                "Claude Code has no login for agent runs in {}. Agent runs are confined and \
                 cannot reach the Keychain, so they use a second login of your own account, \
                 which you make once and can revoke at any time: {}",
                dir.display(),
                claude_login_command(dir)
            ),
        }
    }
}

impl Error for NoAgentLogin {}

/// The command that makes Claude Code's login for agent runs in `dir`,
/// written for a POSIX shell. On macOS it runs `claude auth login` with the
/// Keychain closed, as agent runs have it, so the login is written to
/// `dir/.credentials.json`, the CLI's plain-text store, where confined runs
/// read it. Owlshift never reads, copies or moves that login.
pub fn claude_login_command(dir: &Path) -> String {
    let dir = shell_quote(&dir.to_string_lossy());
    if cfg!(target_os = "macos") {
        format!(
            "CLAUDE_CONFIG_DIR={dir} /usr/bin/sandbox-exec -p '(version 1)(allow default)\
             (deny mach-lookup (global-name \"com.apple.SecurityServer\") \
             (global-name \"com.apple.securityd.xpc\"))' claude auth login"
        )
    } else {
        format!("CLAUDE_CONFIG_DIR={dir} claude auth login")
    }
}

/// `text` quoted for a POSIX shell.
fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

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

/// Claude Code, run with `claude -p` on the user's own login.
#[derive(Clone, Debug, PartialEq)]
pub struct ClaudeHarness {
    /// The `claude` program.
    pub program: PathBuf,
    /// The role's prompt; the executor adds where the brief is.
    pub prompt: String,
    pub model: Option<String>,
    pub effort: Option<Effort>,
    /// A dollar cap, only for a CLI configured for API billing.
    pub max_budget_usd: Option<f64>,
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
    /// one its link leads to), read; and, confined, the login made for
    /// agent runs, read and written, since the CLI refreshes its token
    /// there. Its absence refuses the run: only whether the login file
    /// exists is looked at, never its content.
    fn sandbox_needs(&self, agent: &AgentEnv) -> Result<SandboxNeeds, HarnessError> {
        let mut needs = SandboxNeeds::default();
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
        if agent.is_confined() {
            let dir = agent
                .var("CLAUDE_CONFIG_DIR")
                .map(PathBuf::from)
                .ok_or(NoAgentLogin { dir: None })?;
            if !dir.join(".credentials.json").is_file() {
                return Err(NoAgentLogin { dir: Some(dir) }.into());
            }
            needs.writable.push(dir);
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

    fn claude() -> ClaudeHarness {
        ClaudeHarness {
            program: PathBuf::from("claude"),
            prompt: String::new(),
            model: None,
            effort: None,
            max_budget_usd: None,
        }
    }

    /// A confined run needs the login made for agent runs, and is refused,
    /// with the command that makes it, when there is none; the login file is
    /// only looked for. The bench's bare runs need no login.
    #[test]
    fn a_confined_claude_run_needs_its_own_login() {
        let login = tempfile::tempdir().unwrap();
        let parent = [
            (OsString::from("PATH"), OsString::from("/usr/bin")),
            (
                OsString::from("CLAUDE_CONFIG_DIR"),
                login.path().as_os_str().to_owned(),
            ),
        ];
        let agent = AgentEnv::new(parent.clone(), &[]).unwrap();
        let refused = claude().sandbox_needs(&agent).unwrap_err();
        let text = refused.to_string();
        assert!(text.contains("no login for agent runs"), "{text}");
        assert!(text.contains("claude auth login"), "{text}");
        assert!(text.contains("CLAUDE_CONFIG_DIR='"), "{text}");

        std::fs::write(login.path().join(".credentials.json"), "").unwrap();
        let needs = claude().sandbox_needs(&agent).unwrap();
        assert_eq!(needs.writable, [login.path().to_path_buf()]);

        let bare = AgentEnv::new(parent, &[]).unwrap().without_confinement();
        std::fs::remove_file(login.path().join(".credentials.json")).unwrap();
        assert!(claude().sandbox_needs(&bare).unwrap().writable.is_empty());
    }

    #[test]
    fn the_login_command_quotes_its_folder() {
        let command = claude_login_command(Path::new("/tmp/it's here"));
        assert!(
            command.starts_with("CLAUDE_CONFIG_DIR='/tmp/it'\\''s here' "),
            "{command}"
        );
        assert!(command.ends_with("claude auth login"), "{command}");
    }

    #[test]
    fn the_prompt_ends_with_where_the_brief_is() {
        assert_eq!(
            prompt_with_brief("# Build\n\nDo it.\n\n"),
            "# Build\n\nDo it.\n\nThe brief of this run is the file `.owlshift/run/brief.json` \
             in the working directory.\n"
        );
    }

    /// What comes before the arguments the launched helper prints.
    #[cfg(windows)]
    const SENTINEL: &str = "launched-arguments";

    /// Arguments a quoting mistake would change. The helper reads them after
    /// [`SENTINEL`]; libtest takes them for more test names, which match no
    /// test, since none starts with `-`.
    #[cfg(windows)]
    const ARGUMENTS: [&str; 6] = ["", "a b", "a\"b", r#"a\"b"#, r"c:\x\", "a\tb"];

    #[cfg(windows)]
    #[test]
    #[ignore = "helper, run by the test below"]
    fn helper_echo() {
        if std::env::args().any(|arg| arg == "--exact") {
            use std::io::Write as _;
            let mut out = String::new();
            let args = std::env::args().skip_while(|arg| arg != SENTINEL).skip(1);
            for (index, arg) in args.enumerate() {
                out.push_str(&format!("launched-arg{index}={arg:?}\n"));
            }
            let mut input = String::new();
            io::stdin().read_to_string(&mut input).unwrap();
            out.push_str(&format!("launched-input={input:?}\n"));
            let token = std::env::var("GH_TOKEN").unwrap_or_default();
            out.push_str(&format!("launched-token={token:?}\n"));
            io::stdout().write_all(out.as_bytes()).unwrap();
            io::stdout().flush().unwrap();
            std::process::exit(3);
        }
    }

    /// OWL-71: on the harness's path, `AgentEnv::confine`, with a launcher
    /// set, the command starts through it; the program gets its arguments,
    /// its input and the agent's variables, and its exit code comes back.
    /// libtest prints its own lines on the same stream, hence `contains`.
    #[cfg(windows)]
    #[test]
    fn a_confined_harness_command_starts_through_the_launcher() {
        use crate::agent_env::RunPaths;
        use owlshift_platform::process::{OUTPUT_CAP, run_command};

        let _launcher = owlshift_platform::sandbox::use_built_launcher();
        let dir = tempfile::tempdir().unwrap();
        let agent = AgentEnv::new(std::env::vars_os(), &[]).unwrap();
        let paths = RunPaths {
            workdir: dir.path().to_owned(),
            ..RunPaths::default()
        };
        let mut inner = Command::new(std::env::current_exe().unwrap());
        inner
            .args([
                "--exact",
                "executor::harness::tests::helper_echo",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
                SENTINEL,
            ])
            .args(ARGUMENTS);
        let mut command = agent.confine(inner, &paths).unwrap();
        let program = Path::new(command.get_program()).file_name().unwrap();
        assert!(
            program.eq_ignore_ascii_case("owlshift-launch.exe"),
            "{program:?}"
        );

        let input = Some(&b"the prompt"[..]);
        let captured =
            run_command(&mut command, input, Duration::from_secs(60), OUTPUT_CAP).unwrap();
        let out = String::from_utf8_lossy(&captured.stdout);
        assert_eq!(captured.code, Some(3), "{out}");
        for (index, arg) in ARGUMENTS.iter().enumerate() {
            assert!(
                out.contains(&format!("launched-arg{index}={arg:?}\n")),
                "{out}"
            );
        }
        assert!(out.contains("launched-input=\"the prompt\"\n"), "{out}");
        assert!(
            out.contains("launched-token=\"owlshift-agent-has-no-credential\"\n"),
            "{out}"
        );
    }
}
