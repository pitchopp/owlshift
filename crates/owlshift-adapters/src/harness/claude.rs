//! The Claude Code harness: one role run headless with `claude -p`, on the
//! user's own login.
//!
//! The work is split along the executor's boundary. [`command`] builds the
//! `claude` invocation for a [`Request`]; the executor spawns it, in a process
//! group or Job Object of its own, and hands the child to [`drive`], which
//! sends the prompt, reads the `stream-json` output and returns a [`Run`].
//! Stopping a run, timeouts and the per-run log file stay with the executor:
//! it stops a run by stopping the child's process group, which ends
//! [`drive`] too.
//!
//! Owlshift never passes a credential, and this adapter sets no variable: the
//! executor gives the child the agent environment of
//! `owlshift_core::agent_env`, which keeps what `claude` needs to find the
//! login the user configured, normally their subscription (architecture
//! principle 9), and no tracker, forge or cloud credential.
//!
//! Every CLI behaviour relied on here was checked live and is recorded in
//! `docs/design/build-plan.md`, under checks C1 and C7 and the OWL-14
//! results: the flags, the permission mapping, the event shapes, and the
//! rule that `is_error` and the exit status decide, never `subtype`.
//!
//! A [`Run`] says how the harness process ended, not whether the role did its
//! job: the role's own answer is its `result.json`, which the executor
//! validates against `owlshift_contracts::result`.

use std::collections::BTreeMap;
use std::fmt;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use jiff::Timestamp;
use serde_json::Value;

use owlshift_contracts::brief::{PermissionLevel, Permissions};
use owlshift_contracts::ids::RelativePath;

/// The most bytes kept from standard error; the rest is read and dropped, as
/// `owlshift_platform::process` does for its probes.
pub const STDERR_CAP: usize = 64 * 1024;

/// The longest output line parsed as an event. A longer line still reaches
/// the caller, in pieces, and is counted in [`Run::oversized_lines`].
pub const LINE_CAP: usize = 16 * 1024 * 1024;

/// How long [`drive`] keeps reading after the child has exited. A process the
/// child started may hold its output open; past this delay it is left behind
/// rather than waited on.
pub const EXIT_GRACE: Duration = Duration::from_secs(2);

const POLL: Duration = Duration::from_millis(20);

/// The flags of check C1's guardrail finding: no user settings (hooks,
/// plugins, user `CLAUDE.md`) and no MCP server but those passed explicitly,
/// so the user's own connectors (tracker, chat, mail) never reach an agent.
/// The agent environment (`owlshift_core::agent_env`) covers what lies
/// outside Claude Code, and [`Run::mcp_servers`] lets the executor check that
/// no server was loaded.
const GUARDRAIL_ARGS: &[&str] = &["--setting-sources", "project,local", "--strict-mcp-config"];

/// The tools removed from every run because they act on the user's claude.ai
/// account, outside the run's worktree and budget: `RemoteTrigger` lists,
/// creates and runs cloud agents there. Checked live (OWL-42): Claude Code
/// offers it under both permission modes and neither asks before a call, so
/// only removing it keeps it out of reach. This lists the tools checked so
/// far, not every tool that might reach beyond the run.
const ACCOUNT_TOOLS: &[&str] = &["RemoteTrigger"];

/// The tools removed from a run that has no network access.
const NETWORK_TOOLS: &[&str] = &["WebFetch", "WebSearch"];

/// How hard the model thinks, as `claude --effort` names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Effort {
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl Effort {
    /// The value `claude --effort` takes.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }
}

/// One role run, as the executor asks for it.
#[derive(Clone, Debug, PartialEq)]
pub struct Request {
    /// The directory the run works in: the ticket's worktree.
    pub workdir: PathBuf,
    /// A model alias or full name; `None` leaves the CLI's default.
    pub model: Option<String>,
    /// `None` leaves the CLI's default.
    pub effort: Option<Effort>,
    /// What the role may do, from its brief.
    pub permissions: Permissions,
    /// Where the role writes `result.json`, relative to `workdir`: the one
    /// file a read-only role may write.
    pub result_path: RelativePath,
    /// A JSON Schema for the final answer, returned in
    /// [`Run::structured_output`].
    pub json_schema: Option<String>,
    /// A dollar cap on the run, passed as `--max-budget-usd`. It only means
    /// something for a CLI configured for API billing; whether to set it
    /// (the billing mode, the personal file's `budget_usd`) is the
    /// executor's decision, not this adapter's.
    pub max_budget_usd: Option<f64>,
}

/// Why a request cannot be turned into a command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommandError {
    /// The result path would not name one file in a permission rule.
    ResultPath(String),
    /// The request asks for something this adapter does not provide.
    Unsupported(&'static str),
    /// The budget is negative or not a number.
    Budget(String),
}

impl fmt::Display for CommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ResultPath(path) => write!(
                f,
                "result path {path:?} cannot be used in a permission rule: use `/`-separated \
                 segments of ASCII letters, digits, `.`, `_` and `-`, with no `.` segment"
            ),
            Self::Unsupported(what) => write!(f, "the Claude Code harness does not provide {what}"),
            Self::Budget(budget) => write!(f, "budget {budget} is not a dollar amount"),
        }
    }
}

impl std::error::Error for CommandError {}

/// Builds the `claude -p` invocation of a request, ready to spawn: working
/// directory set, the three standard streams piped. The prompt is not on the
/// command line; [`drive`] writes it on standard input.
///
/// Permission levels map onto Claude Code's own mechanism:
///
/// - read-only: `--permission-mode dontAsk`, which denies every tool that
///   would ask (Bash, writes), plus one allow rule for the result file;
/// - write in worktree: `--permission-mode acceptEdits`, which confines the
///   file tools to the working directory, plus Bash. Bash itself is not
///   confined to the directory: the executor's isolation check covers its
///   writes, and the agent environment leaves it no credential.
///
/// The account tools are always removed and, without network access, the web
/// tools too, all in one `--disallowedTools` flag; Bash, when allowed, can
/// still reach the network. The worktree's own project settings
/// (`.claude/settings.json`) still apply, since C1's guardrail keeps the
/// `project` and `local` sources; an allow rule given with `--settings` did not
/// bring a removed tool back (OWL-42).
pub fn command(program: &Path, request: &Request) -> Result<Command, CommandError> {
    if request.permissions.browser {
        return Err(CommandError::Unsupported("a browser"));
    }
    let result_path = permission_rule_path(&request.result_path)?;
    if let Some(budget) = request.max_budget_usd
        && !(budget.is_finite() && budget >= 0.0)
    {
        return Err(CommandError::Budget(budget.to_string()));
    }

    let mut command = Command::new(program);
    command.args(["-p", "--output-format", "stream-json", "--verbose"]);
    if let Some(model) = &request.model {
        command.args(["--model", model]);
    }
    if let Some(effort) = request.effort {
        command.args(["--effort", effort.as_str()]);
    }
    match request.permissions.level {
        PermissionLevel::ReadOnly => {
            command.args(["--permission-mode", "dontAsk", "--allowedTools"]);
            command.arg(format!("Edit(./{result_path})"));
        }
        PermissionLevel::WriteWorktree => {
            command.args(["--permission-mode", "acceptEdits", "--allowedTools", "Bash"]);
        }
    }
    command.args(["--permission-prompts", "none"]);
    command.arg("--disallowedTools").args(ACCOUNT_TOOLS);
    if !request.permissions.network {
        command.args(NETWORK_TOOLS);
    }
    if let Some(schema) = &request.json_schema {
        command.args(["--json-schema", schema]);
    }
    if let Some(budget) = request.max_budget_usd {
        command.arg("--max-budget-usd").arg(budget.to_string());
    }
    command.args(GUARDRAIL_ARGS);
    command.arg("--no-session-persistence");
    command
        .current_dir(&request.workdir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    Ok(command)
}

/// A result path as it may appear in a permission rule, where `*`, `?`,
/// brackets and spaces would be patterns rather than one file, and a `.`
/// segment would name a directory.
fn permission_rule_path(path: &RelativePath) -> Result<&str, CommandError> {
    let path = path.as_str();
    let safe_char = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-');
    let safe = path
        .split('/')
        .all(|segment| segment != "." && segment.chars().all(safe_char));
    if safe {
        Ok(path)
    } else {
        Err(CommandError::ResultPath(path.to_owned()))
    }
}

/// What a finished run reported.
#[derive(Clone, Debug, PartialEq)]
pub struct Run {
    /// The exit code; `None` when a signal ended the process.
    pub exit_code: Option<i32>,
    pub outcome: Outcome,
    /// Usage from the final record; `None` when the run left none, so an
    /// interrupted run's usage is unknown rather than zero.
    pub usage: Option<Usage>,
    /// The last rate-limit report of the stream.
    pub rate_limit: Option<RateLimit>,
    /// The Claude Code version, to record with every run.
    pub harness_version: Option<String>,
    /// The main model, as the CLI resolved it.
    pub model: Option<String>,
    pub billing: Billing,
    /// The MCP servers the CLI listed in its `init` event, by name, whatever
    /// their status: a server that failed to start is listed too. C1's
    /// guardrail flags leave none; empty as well when no `init` came.
    pub mcp_servers: Vec<String>,
    /// The tools the permission mode refused, one entry per refusal.
    pub permission_denials: Vec<String>,
    /// The final answer, when the request carried a JSON Schema.
    pub structured_output: Option<Value>,
    /// Output lines that were not JSON objects.
    pub malformed_lines: usize,
    /// Output lines longer than [`LINE_CAP`], not parsed.
    pub oversized_lines: usize,
    /// Standard error, up to [`STDERR_CAP`] bytes.
    pub stderr: Vec<u8>,
}

/// How the harness process ended.
#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    /// The CLI finished without error: exit status 0 and a final record with
    /// `is_error: false`. The role's `result.json` still needs validating.
    Completed {
        /// The final message.
        text: String,
    },
    /// The subscription's usage limit stopped the run.
    UsageLimit {
        /// When the limit resets, if the CLI reported it in a rate-limit
        /// event; never read from the message text.
        resets_at: Option<Timestamp>,
        /// The window that is exhausted, such as `five_hour`.
        window: Option<String>,
    },
    Failed(Failure),
}

/// Why a run did not complete.
#[derive(Clone, Debug, PartialEq)]
pub enum Failure {
    /// The output held no final record.
    NoResult,
    /// The final record has no `is_error` flag.
    MalformedResult,
    /// A signal ended the process before it reported anything conclusive.
    Signal,
    /// The prompt could not be written whole to the CLI.
    PromptNotDelivered,
    /// The CLI reported an error or exited with a non-zero status.
    Error {
        exit_code: Option<i32>,
        /// The HTTP status of the failed API call, such as 404.
        api_error_status: Option<u16>,
        /// The CLI's `terminal_reason`, such as `api_error`.
        terminal_reason: Option<String>,
        /// The error kind of the last failed assistant message, such as
        /// `model_not_found`.
        kind: Option<String>,
        /// The final message.
        message: Option<String>,
    },
}

/// Tokens and cost, from the final record.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub cache_creation_input_tokens: u64,
    /// The CLI's cost estimate at list price. On a subscription it measures
    /// consumption, not a bill.
    pub cost_usd: Option<f64>,
    pub duration_ms: Option<u64>,
    pub num_turns: Option<u32>,
    /// The same counts per model, subagents included.
    pub models: BTreeMap<String, ModelUsage>,
}

/// One model's share of a run.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ModelUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cost_usd: Option<f64>,
}

/// A rate-limit report: the state of the subscription's usage windows.
#[derive(Clone, Debug, PartialEq)]
pub struct RateLimit {
    pub status: RateLimitStatus,
    /// The window the report is about, such as `five_hour`.
    pub window: Option<String>,
    pub resets_at: Option<Timestamp>,
    /// How much of the five-hour window is used, from 0 to 1.
    pub five_hour_utilization: Option<f64>,
    /// How much of the seven-day window is used, from 0 to 1.
    pub seven_day_utilization: Option<f64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RateLimitStatus {
    Allowed,
    AllowedWarning,
    Rejected,
    Unknown,
}

/// What the run is billed to, from the CLI's `apiKeySource`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Billing {
    /// No API key: the user's subscription login.
    Subscription,
    /// `ANTHROPIC_API_KEY`.
    ApiKey,
    /// Not reported, or a source C1 did not record.
    Unknown,
}

/// Sends the prompt to a child spawned from [`command`], reads its output to
/// the end and returns what it reported.
///
/// Every byte of standard output reaches `on_line`, in order, one line at a
/// time (a line longer than [`LINE_CAP`] comes in several pieces), so the
/// executor can log the run as it goes.
///
/// It returns at most [`EXIT_GRACE`] after the child exits, even if a process
/// the child started still holds the output open. To stop a run, stop the
/// child's process group (its id is `child.id()`, read before this call):
/// the output then closes and this returns.
pub fn drive(child: &mut Child, prompt: &str, mut on_line: impl FnMut(&[u8])) -> io::Result<Run> {
    let not_piped = || {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "the harness child needs piped standard streams (see `claude::command`)",
        )
    };
    let stdin = child.stdin.take().ok_or_else(not_piped)?;
    let stdout = child.stdout.take().ok_or_else(not_piped)?;
    let stderr = child.stderr.take().ok_or_else(not_piped)?;

    let prompt_sent = send_prompt(stdin, prompt.as_bytes().to_vec());
    let frames = read_frames(stdout);
    let stderr = read_capped(stderr);

    let mut transcript = Transcript::default();
    let mut exited_at = None;
    loop {
        match frames.recv_timeout(POLL) {
            Ok(Frame::Line(line)) => {
                on_line(&line);
                transcript.feed(&line);
            }
            Ok(Frame::Oversized { piece, first }) => {
                on_line(&piece);
                if first {
                    transcript.oversized_lines += 1;
                }
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
    // One grace period in all, counted from the exit: what the loop used is
    // not given again to the other streams.
    let deadline = exited_at.unwrap_or_else(Instant::now) + EXIT_GRACE;
    let remaining = || deadline.saturating_duration_since(Instant::now());
    let stderr = stderr.recv_timeout(remaining()).unwrap_or_default();
    let mut run = transcript.finish(status.code(), stderr);
    let delivered = matches!(prompt_sent.recv_timeout(remaining()), Ok(Ok(())));
    if !delivered && matches!(run.outcome, Outcome::Completed { .. }) {
        run.outcome = Outcome::Failed(Failure::PromptNotDelivered);
    }
    Ok(run)
}

/// Writes the prompt on its own thread, so a child that writes before it
/// reads cannot block on a full pipe, then closes standard input.
fn send_prompt(
    mut stdin: impl Write + Send + 'static,
    prompt: Vec<u8>,
) -> Receiver<io::Result<()>> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let sent = stdin.write_all(&prompt).and_then(|()| stdin.flush());
        drop(stdin);
        // The receiver is gone when `drive` stopped waiting: nothing to report.
        let _ = sender.send(sent);
    });
    receiver
}

/// A piece of standard output.
enum Frame {
    /// A whole line, with its newline when it had one.
    Line(Vec<u8>),
    /// A piece of a line longer than [`LINE_CAP`].
    Oversized { piece: Vec<u8>, first: bool },
}

/// Reads standard output on its own thread, one line at a time, never
/// holding more than [`LINE_CAP`] bytes of a line.
fn read_frames(stream: impl Read + Send + 'static) -> Receiver<Frame> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        // A read error ends the stream like its end would: the outcome then
        // rests on what was read.
        let _ = pump_frames(BufReader::new(stream), &sender);
    });
    receiver
}

fn pump_frames(mut reader: impl BufRead, sender: &Sender<Frame>) -> io::Result<()> {
    let cap = LINE_CAP as u64;
    let mut in_long_line = false;
    loop {
        let mut piece = Vec::new();
        let read = (&mut reader).take(cap).read_until(b'\n', &mut piece)?;
        if read == 0 {
            return Ok(());
        }
        let ends_line = piece.ends_with(b"\n");
        let frame = if in_long_line || (!ends_line && read as u64 == cap) {
            let first = !in_long_line;
            in_long_line = !ends_line;
            Frame::Oversized { piece, first }
        } else {
            Frame::Line(piece)
        };
        if sender.send(frame).is_err() {
            return Ok(());
        }
    }
}

/// Reads standard error to its end on its own thread, keeping the first
/// [`STDERR_CAP`] bytes and dropping the rest, so the child never blocks on
/// a full pipe.
fn read_capped(mut stream: impl Read + Send + 'static) -> Receiver<Vec<u8>> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut kept = Vec::new();
        let mut buffer = [0u8; 8192];
        loop {
            match stream.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => {
                    let room = STDERR_CAP - kept.len();
                    kept.extend_from_slice(&buffer[..n.min(room)]);
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        let _ = sender.send(kept);
    });
    receiver
}

/// Reads a `claude -p --output-format stream-json` output one line at a
/// time. [`drive`] uses it on a live child; contract tests replay recorded
/// output through it.
#[derive(Debug, Default)]
pub struct Transcript {
    harness_version: Option<String>,
    model: Option<String>,
    billing: Option<Billing>,
    mcp_servers: Vec<String>,
    rate_limit: Option<RateLimit>,
    /// The latest rejection, cleared by a later report that allows again.
    rejection: Option<RateLimit>,
    /// An assistant message reported a rate limit, and no later report
    /// allowed again.
    limit_message: bool,
    error_kind: Option<String>,
    result: Option<FinalRecord>,
    malformed_lines: usize,
    oversized_lines: usize,
}

/// The final `result` record.
#[derive(Debug)]
struct FinalRecord {
    is_error: Option<bool>,
    text: Option<String>,
    api_error_status: Option<u16>,
    terminal_reason: Option<String>,
    usage: Usage,
    denials: Vec<String>,
    structured_output: Option<Value>,
}

impl Transcript {
    /// Reads one output line. Events of an unknown type are skipped; a line
    /// that is not a JSON object is counted.
    pub fn feed(&mut self, line: &[u8]) {
        let event = match serde_json::from_slice::<Value>(line) {
            Ok(event) if event.is_object() => event,
            _ if line.iter().all(u8::is_ascii_whitespace) => return,
            _ => {
                self.malformed_lines += 1;
                return;
            }
        };
        match str_field(&event, "type") {
            Some("system") if str_field(&event, "subtype") == Some("init") => self.init(&event),
            Some("rate_limit_event") => self.rate_limit_event(&event),
            Some("assistant") => {
                if let Some(kind) = str_field(&event, "error") {
                    self.limit_message |= kind == "rate_limit";
                    self.error_kind = Some(kind.to_owned());
                }
            }
            Some("result") => self.result = Some(final_record(&event)),
            _ => {}
        }
    }

    fn init(&mut self, event: &Value) {
        self.harness_version = str_field(event, "claude_code_version").map(str::to_owned);
        self.model = str_field(event, "model").map(str::to_owned);
        self.billing = Some(match str_field(event, "apiKeySource") {
            Some("none") => Billing::Subscription,
            Some("ANTHROPIC_API_KEY") => Billing::ApiKey,
            _ => Billing::Unknown,
        });
        // Live shape (OWL-22): `[{"name":…,"status":"failed","source":"dynamic"}]`.
        self.mcp_servers = event["mcp_servers"]
            .as_array()
            .map(|servers| {
                servers
                    .iter()
                    .map(|server| str_field(server, "name").unwrap_or("unnamed").to_owned())
                    .collect()
            })
            .unwrap_or_default();
    }

    fn rate_limit_event(&mut self, event: &Value) {
        let info = &event["rate_limit_info"];
        let windows = &info["unifiedWindows"];
        let report = RateLimit {
            status: match str_field(info, "status") {
                Some("allowed") => RateLimitStatus::Allowed,
                Some("allowed_warning") => RateLimitStatus::AllowedWarning,
                Some("rejected") => RateLimitStatus::Rejected,
                _ => RateLimitStatus::Unknown,
            },
            window: str_field(info, "rateLimitType").map(str::to_owned),
            resets_at: info["resetsAt"]
                .as_i64()
                .and_then(|second| Timestamp::from_second(second).ok()),
            five_hour_utilization: windows["five_hour"]["utilization"].as_f64(),
            seven_day_utilization: windows["seven_day"]["utilization"].as_f64(),
        };
        match report.status {
            RateLimitStatus::Rejected => self.rejection = Some(report.clone()),
            RateLimitStatus::Allowed | RateLimitStatus::AllowedWarning => {
                self.rejection = None;
                self.limit_message = false;
            }
            RateLimitStatus::Unknown => {}
        }
        self.rate_limit = Some(report);
    }

    /// Classifies the run once the process has exited.
    ///
    /// A run completed only with exit status 0 and a final record saying
    /// `is_error: false`. Otherwise, a usage limit still in force at the end
    /// of the stream wins over the other failures; the CLI's `subtype` is
    /// never read (check C1).
    pub fn finish(self, exit_code: Option<i32>, stderr: Vec<u8>) -> Run {
        let completed = exit_code == Some(0)
            && self
                .result
                .as_ref()
                .is_some_and(|record| record.is_error == Some(false));
        let outcome = if completed {
            let text = self.result.as_ref().and_then(|r| r.text.clone());
            Outcome::Completed {
                text: text.unwrap_or_default(),
            }
        } else if self.rejection.is_some() || self.limit_message {
            let rejection = self.rejection.as_ref();
            Outcome::UsageLimit {
                resets_at: rejection.and_then(|r| r.resets_at),
                window: rejection.and_then(|r| r.window.clone()),
            }
        } else {
            Outcome::Failed(match &self.result {
                _ if exit_code.is_none() => Failure::Signal,
                None => Failure::NoResult,
                Some(record) if record.is_error.is_none() => Failure::MalformedResult,
                Some(record) => Failure::Error {
                    exit_code,
                    api_error_status: record.api_error_status,
                    terminal_reason: record.terminal_reason.clone(),
                    kind: self.error_kind.clone(),
                    message: record.text.clone(),
                },
            })
        };
        let (usage, permission_denials, structured_output) = match self.result {
            Some(record) => (Some(record.usage), record.denials, record.structured_output),
            None => (None, Vec::new(), None),
        };
        Run {
            exit_code,
            outcome,
            usage,
            rate_limit: self.rate_limit,
            harness_version: self.harness_version,
            model: self.model,
            billing: self.billing.unwrap_or(Billing::Unknown),
            mcp_servers: self.mcp_servers,
            permission_denials,
            structured_output,
            malformed_lines: self.malformed_lines,
            oversized_lines: self.oversized_lines,
            stderr,
        }
    }
}

/// Reads the final record field by field, so a change in the telemetry
/// fields never hides the outcome.
fn final_record(event: &Value) -> FinalRecord {
    let usage = &event["usage"];
    let models = event["modelUsage"]
        .as_object()
        .map(|models| {
            models
                .iter()
                .map(|(name, model)| {
                    let usage = ModelUsage {
                        input_tokens: count(model, "inputTokens"),
                        output_tokens: count(model, "outputTokens"),
                        cache_read_input_tokens: count(model, "cacheReadInputTokens"),
                        cache_creation_input_tokens: count(model, "cacheCreationInputTokens"),
                        cost_usd: model["costUSD"].as_f64(),
                    };
                    (name.clone(), usage)
                })
                .collect()
        })
        .unwrap_or_default();
    let denials = event["permission_denials"]
        .as_array()
        .map(|denials| {
            denials
                .iter()
                .map(|denial| {
                    str_field(denial, "tool_name")
                        .unwrap_or("unknown")
                        .to_owned()
                })
                .collect()
        })
        .unwrap_or_default();
    FinalRecord {
        is_error: event["is_error"].as_bool(),
        text: str_field(event, "result").map(str::to_owned),
        api_error_status: event["api_error_status"]
            .as_u64()
            .and_then(|status| u16::try_from(status).ok()),
        terminal_reason: str_field(event, "terminal_reason").map(str::to_owned),
        usage: Usage {
            input_tokens: count(usage, "input_tokens"),
            output_tokens: count(usage, "output_tokens"),
            cache_read_input_tokens: count(usage, "cache_read_input_tokens"),
            cache_creation_input_tokens: count(usage, "cache_creation_input_tokens"),
            cost_usd: event["total_cost_usd"].as_f64(),
            duration_ms: event["duration_ms"].as_u64(),
            num_turns: event["num_turns"]
                .as_u64()
                .and_then(|turns| u32::try_from(turns).ok()),
            models,
        },
        denials,
        structured_output: Some(&event["structured_output"])
            .filter(|output| !output.is_null())
            .cloned(),
    }
}

fn str_field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

/// A token count; an absent counter in a present record counts as zero.
fn count(value: &Value, key: &str) -> u64 {
    value[key].as_u64().unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use super::*;

    fn request(level: PermissionLevel) -> Request {
        Request {
            workdir: PathBuf::from("/work"),
            model: None,
            effort: None,
            permissions: Permissions {
                level,
                network: true,
                browser: false,
            },
            result_path: RelativePath::new(".owlshift/result.json").unwrap(),
            json_schema: None,
            max_budget_usd: None,
        }
    }

    fn args(request: &Request) -> Vec<String> {
        let command = command(Path::new("claude"), request).unwrap();
        command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    fn joined(request: &Request) -> String {
        args(request).join(" ")
    }

    #[test]
    fn a_read_only_run_may_write_only_its_result_file() {
        let request = request(PermissionLevel::ReadOnly);
        assert_eq!(
            joined(&request),
            "-p --output-format stream-json --verbose \
             --permission-mode dontAsk --allowedTools Edit(./.owlshift/result.json) \
             --permission-prompts none --disallowedTools RemoteTrigger \
             --setting-sources project,local --strict-mcp-config --no-session-persistence"
        );
        let command = command(Path::new("claude"), &request).unwrap();
        assert_eq!(command.get_current_dir(), Some(Path::new("/work")));
    }

    #[test]
    fn a_write_run_edits_the_worktree_and_runs_bash() {
        let mut request = request(PermissionLevel::WriteWorktree);
        request.model = Some("haiku".into());
        request.effort = Some(Effort::Xhigh);
        request.permissions.network = false;
        request.json_schema = Some(r#"{"type":"object"}"#.into());
        request.max_budget_usd = Some(2.5);
        assert_eq!(
            joined(&request),
            "-p --output-format stream-json --verbose --model haiku --effort xhigh \
             --permission-mode acceptEdits --allowedTools Bash \
             --permission-prompts none --disallowedTools RemoteTrigger WebFetch WebSearch \
             --json-schema {\"type\":\"object\"} --max-budget-usd 2.5 \
             --setting-sources project,local --strict-mcp-config --no-session-persistence"
        );
    }

    #[test]
    fn requests_the_adapter_cannot_honour_are_refused() {
        for path in ["result *.json", "out/./result.json", "a/[b].json", "."] {
            let mut request = request(PermissionLevel::ReadOnly);
            request.result_path = RelativePath::new(path).unwrap();
            let refused = command(Path::new("claude"), &request).unwrap_err();
            assert_eq!(refused, CommandError::ResultPath(path.into()), "{path}");
        }
        for budget in [-1.0, f64::NAN, f64::INFINITY] {
            let mut request = request(PermissionLevel::WriteWorktree);
            request.max_budget_usd = Some(budget);
            let refused = command(Path::new("claude"), &request).unwrap_err();
            assert!(matches!(refused, CommandError::Budget(_)), "{budget}");
        }
        let mut request = request(PermissionLevel::WriteWorktree);
        request.permissions.browser = true;
        assert!(matches!(
            command(Path::new("claude"), &request),
            Err(CommandError::Unsupported(_))
        ));
    }

    #[test]
    fn the_prompt_and_the_environment_are_left_alone() {
        let command = command(Path::new("claude"), &request(PermissionLevel::ReadOnly)).unwrap();
        assert_eq!(command.get_envs().count(), 0);
        assert!(!command.get_args().any(|arg| arg == OsStr::new("--bare")));
    }

    fn replay(lines: &[&str], exit_code: Option<i32>) -> Run {
        let mut transcript = Transcript::default();
        for line in lines {
            transcript.feed(line.as_bytes());
        }
        transcript.finish(exit_code, Vec::new())
    }

    const OK: &str =
        r#"{"type":"result","is_error":false,"result":"done","usage":{"output_tokens":3}}"#;
    const REJECTED: &str = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","resetsAt":1790613600,"rateLimitType":"seven_day"}}"#;
    const ALLOWED: &str = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed","resetsAt":1790600000}}"#;
    const LIMIT_MESSAGE: &str = r#"{"type":"assistant","error":"rate_limit","message":{"content":[{"type":"text","text":"You've hit your limit · resets 2pm"}]}}"#;
    const FAILED: &str =
        r#"{"type":"result","is_error":true,"api_error_status":500,"result":"boom"}"#;

    #[test]
    fn a_limit_counts_only_while_in_force_and_only_when_the_run_did_not_complete() {
        // A run that completes despite a rejection is complete; the report stays visible.
        let run = replay(&[REJECTED, OK], Some(0));
        assert_eq!(
            run.outcome,
            Outcome::Completed {
                text: "done".into()
            }
        );
        assert_eq!(run.rate_limit.unwrap().status, RateLimitStatus::Rejected);

        // A rejection a later report lifted does not explain a later failure.
        let run = replay(&[REJECTED, ALLOWED, FAILED], Some(1));
        assert!(matches!(
            run.outcome,
            Outcome::Failed(Failure::Error {
                api_error_status: Some(500),
                ..
            })
        ));

        // The reset time comes from the rejection, never from the message or
        // an unrelated report.
        let run = replay(&[ALLOWED, REJECTED, FAILED], Some(1));
        assert_eq!(
            run.outcome,
            Outcome::UsageLimit {
                resets_at: Some(Timestamp::from_second(1790613600).unwrap()),
                window: Some("seven_day".into()),
            }
        );
        let run = replay(&[ALLOWED, LIMIT_MESSAGE], None);
        assert_eq!(
            run.outcome,
            Outcome::UsageLimit {
                resets_at: None,
                window: None
            }
        );
    }

    #[test]
    fn a_run_without_a_conclusive_final_record_failed() {
        let run = replay(&[], Some(0));
        assert_eq!(run.outcome, Outcome::Failed(Failure::NoResult));
        assert_eq!(run.usage, None);
        assert_eq!(run.billing, Billing::Unknown);

        let run = replay(&[OK], None);
        assert_eq!(run.outcome, Outcome::Failed(Failure::Signal));

        let run = replay(&[OK], Some(1));
        assert!(matches!(
            run.outcome,
            Outcome::Failed(Failure::Error {
                exit_code: Some(1),
                ..
            })
        ));

        let run = replay(&[r#"{"type":"result","result":"done"}"#], Some(0));
        assert_eq!(run.outcome, Outcome::Failed(Failure::MalformedResult));
    }

    #[test]
    fn noise_is_counted_and_skipped() {
        let run = replay(
            &[
                "running 1 test\n",
                "\n",
                r#"{"type":"future_event","x":1}"#,
                "[1,2]",
                // The last line of a stream may lack its newline.
                OK,
            ],
            Some(0),
        );
        assert_eq!(run.malformed_lines, 2);
        assert_eq!(
            run.outcome,
            Outcome::Completed {
                text: "done".into()
            }
        );
        assert_eq!(run.usage.unwrap().output_tokens, 3);
    }

    #[test]
    fn the_mcp_servers_of_the_init_event_are_reported() {
        // As in the recorded runs, made with C1's guardrail flags.
        let init = r#"{"type":"system","subtype":"init","mcp_servers":[],"apiKeySource":"none"}"#;
        assert!(replay(&[init, OK], Some(0)).mcp_servers.is_empty());
        // The shape of a live run given one server that failed to start
        // (OWL-22); a server without a name is still counted.
        let init = r#"{"type":"system","subtype":"init","mcp_servers":[{"name":"owlshift-probe","status":"failed","source":"dynamic"},{"status":"connected"}]}"#;
        assert_eq!(
            replay(&[init, OK], Some(0)).mcp_servers,
            ["owlshift-probe", "unnamed"]
        );
    }

    #[test]
    fn a_line_over_the_cap_comes_in_pieces_and_is_not_parsed() {
        let mut long = vec![b'x'; LINE_CAP + 10];
        long.push(b'\n');
        long.extend_from_slice(b"{\"type\":\"result\"}\n");
        let (sender, receiver) = mpsc::channel();
        pump_frames(&long[..], &sender).unwrap();
        drop(sender);
        let frames: Vec<Frame> = receiver.into_iter().collect();
        let shape: Vec<(usize, &str)> = frames
            .iter()
            .map(|frame| match frame {
                Frame::Line(line) => (line.len(), "line"),
                Frame::Oversized { piece, first: true } => (piece.len(), "first"),
                Frame::Oversized {
                    piece,
                    first: false,
                } => (piece.len(), "rest"),
            })
            .collect();
        assert_eq!(
            shape,
            [(LINE_CAP, "first"), (11, "rest"), (18, "line")],
            "every byte arrives, in order"
        );
    }
}
