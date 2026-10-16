//! The Codex harness: the critic of a review pass run headless with
//! `codex exec`, on the user's own Codex login (architecture section 5, "The
//! Codex harness").
//!
//! The split is Claude Code's ([`super::claude`]): [`command`] builds the
//! `codex exec` invocation for a [`Request`]; the executor spawns it and
//! hands the child to [`drive`], which sends the prompt on standard input,
//! reads the `--json` stream and returns a [`Run`]. The executor's read-only
//! profile confines the run (OWL-298); the runner gives it a `CODEX_HOME` of
//! its own and writes `result.json` from the run's last message.
//!
//! Every CLI behaviour relied on here was checked live and is recorded in
//! `docs/design/build-plan.md`, under checks C2 and C7 (Codex) and OWL-130's
//! entry (OWL-131, OWL-132, OWL-316, OWL-317), with codex-cli 0.154.0: the
//! flags, the stream's events, the rule that a run
//! failed exactly when its stream ends in `turn.failed` or is empty, the
//! usage-limit message, the effort levels, and the item a collaboration
//! tool's call leaves. The recordings are replayed by
//! `tests/codex_fixtures.rs`.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use serde_json::{Value, json};

use owlshift_contracts::Role;
use owlshift_contracts::brief::{PermissionLevel, Permissions};
use owlshift_contracts::format::RESULT_FORMAT;

use super::Usage;
use super::stream::{self, Sink};

/// How hard the model thinks: the contracts' vocabulary, mapped to Codex's
/// own levels by [`reasoning_effort`].
pub use owlshift_contracts::result::Effort;

/// The flags of every launch.
///
/// - `--ephemeral` keeps no session files.
/// - `--json` prints the event stream [`Transcript`] reads.
/// - `--ignore-user-config` loads no `config.toml`, the user's or a trusted
///   repository's, so no MCP server, provider or exporter of theirs applies;
///   the login still does (OWL-130, recorded under "Codex's remaining
///   routing paths").
/// - `--disable apps` keeps the ChatGPT apps server out: a ChatGPT login
///   otherwise opens it with its access token, and it offers the model MCP
///   resource tools and `request_plugin_install`, checked live on
///   2026-10-16 (OWL-132, recordings `apps_disabled` and its control).
/// - `--disable image_generation` and `-c web_search="disabled"` take image
///   generation and web search away from the critic: it returns JSON, and
///   web search searches on the operator's account, against the design's
///   "without network where the harness allows it" (OWL-316, recordings
///   `critic_tools` and its control). The collaboration tools
///   (`spawn_agent` and the rest) stay offered: neither
///   `--disable multi_agent` nor `--disable multi_agent_v2` took them away,
///   checked live on 2026-10-16, and `-c agents.max_depth=0` does not refuse
///   a spawn (OWL-320), so none is passed. The runner stops a run at the
///   first call to one its stream shows, and refuses it
///   ([`Run::collaboration`], OWL-317 and OWL-319).
/// - `--ignore-rules` drops execpolicy `.rules` files: the runner injects the
///   project's rules in the brief (C2, "Rules").
/// - `skills.include_instructions=false` lists no skill to the model, the
///   worktree's `.agents/skills` and `.codex/skills` included, which load
///   whatever the flags above say (OWL-131).
/// - `project_doc_max_bytes=0` drops the worktree's `AGENTS.md`: the brief
///   carries the project's rules, read at the base commit, never from the
///   ticket's branch (OWL-131).
/// - `--dangerously-bypass-approvals-and-sandbox` turns Codex's own sandbox
///   off: Seatbelt does not nest, and a nested one fails silently (C2,
///   `nested_sandbox`). The executor's read-only profile confines the run
///   instead; the runner refuses a Codex run it cannot confine. It does not
///   bypass hook trust; `--dangerously-bypass-hook-trust` would, and would
///   run the hooks of the run's own `CODEX_HOME` (OWL-131).
const LAUNCH_ARGS: &[&str] = &[
    "exec",
    "--ephemeral",
    "--json",
    "--ignore-user-config",
    "--disable",
    "apps",
    "--disable",
    "image_generation",
    "-c",
    "web_search=\"disabled\"",
    "--ignore-rules",
    "-c",
    "skills.include_instructions=false",
    "-c",
    "project_doc_max_bytes=0",
    "--dangerously-bypass-approvals-and-sandbox",
];

/// The roles Codex runs: the critic of Design review and of Verify
/// (architecture section 5, "The roster").
pub const REVIEW_ROLES: [Role; 2] = [Role::DesignReview, Role::Verify];

/// The messages of a reached usage limit. The first was captured live (C7,
/// `usage_limit`); the other two are in the 0.154.0 binary and were not
/// observed.
pub const LIMIT_MESSAGES: [&str; 3] = [
    "You've hit your usage limit",
    "You're out of credits",
    "Usage limit reached",
];

/// The type of the item a call to one of Codex's collaboration tools leaves
/// in the stream. With codex-cli 0.154.0 a `wait_agent` call leaves one,
/// with `tool` `wait`; `spawn_agent` and `list_agents` leave none (OWL-317,
/// recording `critic_collaboration`).
pub const COLLABORATION_ITEM: &str = "collab_tool_call";

/// One critic run, as the runner asks for it.
#[derive(Clone, Debug, PartialEq)]
pub struct Request {
    /// The directory the run reads: the ticket's worktree.
    pub workdir: PathBuf,
    /// A model name; `None` leaves Codex's default (`gpt-6-astra` under
    /// `--ignore-user-config`, C2).
    pub model: Option<String>,
    /// `None` leaves the model's default.
    pub effort: Option<Effort>,
    /// The role of the run's brief: one of [`REVIEW_ROLES`].
    pub role: Role,
    /// What the role may do, from its brief: read-only.
    pub permissions: Permissions,
    /// The JSON Schema of the last message, [`review_schema`], written by
    /// the runner in the run's own files.
    pub schema_file: PathBuf,
    /// Where Codex writes its last message (`-o`), in the run's own files.
    pub last_message_file: PathBuf,
}

/// Why a request cannot be turned into a command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommandError {
    /// The request asks for something this adapter does not provide.
    Unsupported(String),
}

impl fmt::Display for CommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported(what) => write!(f, "the Codex harness does not run {what}"),
        }
    }
}

impl std::error::Error for CommandError {}

/// Builds the `codex exec` invocation of a request, ready to spawn: working
/// directory set, the three standard streams piped. The prompt is not on the
/// command line: `-` makes Codex read it from standard input, where [`drive`]
/// writes it.
///
/// Refused: a brief that lets the role write the worktree (Codex runs only
/// under the read-only profile), a browser, and any role but the critic's.
/// Codex cannot be kept off the network (architecture, "Its network"), so
/// the brief's `network` changes nothing.
pub fn command(program: &Path, request: &Request) -> Result<Command, CommandError> {
    if request.permissions.level != PermissionLevel::ReadOnly {
        return Err(CommandError::Unsupported(
            "a role that writes the worktree: Codex runs only under Owlshift's read-only profile"
                .to_owned(),
        ));
    }
    if request.permissions.browser {
        return Err(CommandError::Unsupported("a browser".to_owned()));
    }
    if !REVIEW_ROLES.contains(&request.role) {
        return Err(CommandError::Unsupported(format!(
            "the {} role: Codex runs the critic only",
            request.role.as_str()
        )));
    }
    let mut command = Command::new(program);
    command.args(LAUNCH_ARGS);
    command.arg("--output-schema").arg(&request.schema_file);
    command.arg("-o").arg(&request.last_message_file);
    if let Some(model) = &request.model {
        command.args(["-m", model]);
    }
    if let Some(effort) = request.effort {
        command.arg("-c").arg(format!(
            "model_reasoning_effort={}",
            reasoning_effort(effort)
        ));
    }
    command.arg("-");
    command
        .current_dir(&request.workdir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    Ok(command)
}

/// Codex's `model_reasoning_effort` for an effort level. One to one on
/// `gpt-6-astra`, Codex's default model, which took each of the five levels
/// (C2, 2026-10-15). The CLI does not check the value; the API does, per
/// model, so a model without one of them fails the run with the API's
/// refusal (`unsupported_effort`).
pub fn reasoning_effort(effort: Effort) -> &'static str {
    match effort {
        Effort::Low => "low",
        Effort::Medium => "medium",
        Effort::High => "high",
        Effort::Xhigh => "xhigh",
        Effort::Max => "max",
    }
}

/// The JSON Schema of the critic's last message, passed as
/// `--output-schema`: a review role's `result.json` (format 9), with the
/// fields the review roles may give.
///
/// The API takes it as a strict schema, which lists every property as
/// required and refuses one left out (C2, `invalid_schema`), so `criterion`
/// is required and nullable. The runner validates the result whatever the
/// schema let through.
pub fn review_schema() -> Value {
    let text = json!({ "type": "string" });
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["format", "status", "summary", "findings"],
        "properties": {
            "format": { "type": "integer", "enum": [RESULT_FORMAT] },
            "status": { "type": "string", "enum": ["done", "blocked", "failed"] },
            "summary": text,
            "findings": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["id", "severity", "claim", "evidence", "criterion", "settle"],
                    "properties": {
                        "id": text,
                        "severity": { "type": "string", "enum": ["blocker", "major", "minor"] },
                        "claim": text,
                        "evidence": text,
                        "criterion": { "type": ["string", "null"] },
                        "settle": text,
                    },
                },
            },
        },
    })
}

/// What a finished run reported.
#[derive(Clone, Debug, PartialEq)]
pub struct Run {
    /// The exit code; `None` when a signal ended the process.
    pub exit_code: Option<i32>,
    pub outcome: Outcome,
    /// Tokens from `turn.completed`; `None` when the run left none. Codex
    /// reports no cost and no model in its stream (C2).
    pub usage: Option<Usage>,
    /// The `tool` of every [`COLLABORATION_ITEM`] the stream carried, each
    /// once, in the order first seen; `unnamed` for an item without one. The
    /// outcome does not depend on it: the runner judges it.
    pub collaboration: Vec<String>,
    /// [`drive`] called its stop at the first collaboration item: the
    /// outcome then reads a run the runner stopped. Never set on a replay.
    pub stopped: bool,
    /// Output lines that were not JSON objects.
    pub malformed_lines: usize,
    /// Output lines longer than the stream's line cap, not parsed.
    pub oversized_lines: usize,
    /// Standard error, up to the stream's cap.
    pub stderr: Vec<u8>,
}

/// How the harness process ended.
#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    /// Exit status 0 and a stream ending in `turn.completed`. The role's
    /// answer still needs validating.
    Completed {
        /// The last agent message: the result's JSON text.
        text: String,
    },
    /// The subscription's usage limit stopped the run. Codex gives the
    /// reset time only as local wall-clock text, which is not read (C7).
    UsageLimit,
    Failed(Failure),
}

/// Why a run did not complete.
#[derive(Clone, Debug, PartialEq)]
pub enum Failure {
    /// Nothing on the stream: Codex stopped before its thread started, as
    /// with an unwritable `CODEX_HOME` (C2, `codex_home_read_only`).
    NoThread,
    /// The stream ended without `turn.completed` or `turn.failed`.
    Unfinished,
    /// A signal ended the process before it reported anything conclusive.
    Signal,
    /// The prompt could not be written whole to the CLI.
    PromptNotDelivered,
    /// The turn failed, or the exit status disagrees with the stream.
    Error {
        exit_code: Option<i32>,
        /// The HTTP status of the failed API call, such as 401, when the
        /// message names one.
        api_status: Option<u16>,
        /// `turn.failed`'s message, often the API's error as JSON text.
        message: Option<String>,
    },
}

/// Sends the prompt to a child spawned from [`command`], reads its output to
/// the end and returns what it reported, as the Claude Code adapter's
/// `drive` does: every byte of standard output reaches `on_line`, and it
/// returns at most the stream's grace period after the child exits.
///
/// `stop` is called once, as soon as the stream shows its first
/// [`COLLABORATION_ITEM`], and [`Run::stopped`] says so (OWL-319). It must
/// stop the child's whole process tree: `codex` is a Node wrapper around a
/// native binary that holds the output open, and once the tree is stopped
/// the output closes and this returns.
pub fn drive(
    child: &mut Child,
    prompt: &str,
    on_line: impl FnMut(&[u8]),
    stop: impl FnOnce(),
) -> io::Result<Run> {
    let mut guarded = Guarded {
        transcript: Transcript::default(),
        stop: Some(stop),
    };
    let driven = stream::drive(child, prompt, on_line, &mut guarded)?;
    let stopped = guarded.stop.is_none();
    let mut run = guarded.transcript.finish(driven.exit_code, driven.stderr);
    run.stopped = stopped;
    if !driven.prompt_delivered && matches!(run.outcome, Outcome::Completed { .. }) {
        run.outcome = Outcome::Failed(Failure::PromptNotDelivered);
    }
    Ok(run)
}

/// A transcript that calls its stop at the first collaboration item.
struct Guarded<F: FnOnce()> {
    transcript: Transcript,
    /// `None` once called.
    stop: Option<F>,
}

impl<F: FnOnce()> Sink for Guarded<F> {
    fn line(&mut self, line: &[u8]) {
        self.transcript.feed(line);
        if !self.transcript.collaboration.is_empty()
            && let Some(stop) = self.stop.take()
        {
            stop();
        }
    }

    fn oversized(&mut self) {
        self.transcript.oversized();
    }
}

/// Reads a `codex exec --json` stream one line at a time. [`drive`] uses it
/// on a live child; the contract tests replay recorded streams through it.
#[derive(Debug, Default)]
pub struct Transcript {
    /// Any event was read.
    started: bool,
    /// The text of the last `agent_message` item.
    last_message: Option<String>,
    /// The last terminal event: `turn.completed` or `turn.failed`.
    end: Option<End>,
    usage: Option<Usage>,
    /// A usage-limit message was seen in an `error` or `turn.failed` event.
    limit: bool,
    /// See [`Run::collaboration`].
    collaboration: Vec<String>,
    malformed_lines: usize,
    oversized_lines: usize,
}

#[derive(Debug)]
enum End {
    Completed,
    Failed(Option<String>),
}

impl Sink for Transcript {
    fn line(&mut self, line: &[u8]) {
        self.feed(line);
    }

    fn oversized(&mut self) {
        self.oversized_lines += 1;
    }
}

impl Transcript {
    /// Reads one output line. Events of an unknown type are skipped; a line
    /// that is not a JSON object is counted. A [`COLLABORATION_ITEM`] is
    /// noted whichever of its started, updated or completed events comes.
    pub fn feed(&mut self, line: &[u8]) {
        let event = match serde_json::from_slice::<Value>(line) {
            Ok(event) if event.is_object() => event,
            _ if line.iter().all(u8::is_ascii_whitespace) => return,
            _ => {
                self.malformed_lines += 1;
                return;
            }
        };
        self.started = true;
        let kind = str_field(&event, "type");
        let item = &event["item"];
        if matches!(
            kind,
            Some("item.started" | "item.updated" | "item.completed")
        ) && str_field(item, "type") == Some(COLLABORATION_ITEM)
        {
            let tool = str_field(item, "tool").unwrap_or("unnamed");
            if !self.collaboration.iter().any(|seen| seen == tool) {
                self.collaboration.push(tool.to_owned());
            }
        }
        match kind {
            Some("item.completed") => {
                if str_field(item, "type") == Some("agent_message") {
                    self.last_message = str_field(item, "text").map(str::to_owned);
                }
            }
            Some("turn.completed") => {
                self.end = Some(End::Completed);
                self.usage = Some(usage(&event["usage"]));
            }
            Some("turn.failed") => {
                let message = str_field(&event["error"], "message");
                self.limit |= message.is_some_and(is_limit);
                self.end = Some(End::Failed(message.map(str::to_owned)));
            }
            Some("error") => {
                self.limit |= str_field(&event, "message").is_some_and(is_limit);
            }
            _ => {}
        }
    }

    /// Classifies the run once the process has exited.
    ///
    /// A run completed only with exit status 0 and a stream ending in
    /// `turn.completed` (C2, "Failure"). Otherwise a usage-limit message
    /// wins over the other failures.
    pub fn finish(self, exit_code: Option<i32>, stderr: Vec<u8>) -> Run {
        let outcome = match (&self.end, exit_code) {
            (Some(End::Completed), Some(0)) => Outcome::Completed {
                text: self.last_message.clone().unwrap_or_default(),
            },
            _ if self.limit => Outcome::UsageLimit,
            (_, None) => Outcome::Failed(Failure::Signal),
            _ if !self.started => Outcome::Failed(Failure::NoThread),
            (None, _) => Outcome::Failed(Failure::Unfinished),
            (Some(End::Failed(message)), _) => Outcome::Failed(Failure::Error {
                exit_code,
                api_status: message.as_deref().and_then(api_status),
                message: message.clone(),
            }),
            (Some(End::Completed), _) => Outcome::Failed(Failure::Error {
                exit_code,
                api_status: None,
                message: None,
            }),
        };
        Run {
            exit_code,
            outcome,
            usage: self.usage,
            collaboration: self.collaboration,
            stopped: false,
            malformed_lines: self.malformed_lines,
            oversized_lines: self.oversized_lines,
            stderr,
        }
    }
}

fn is_limit(message: &str) -> bool {
    LIMIT_MESSAGES.iter().any(|limit| message.contains(limit))
}

/// The tokens of `turn.completed`, in [`Usage`]'s terms. Codex counts the
/// cached input inside `input_tokens` and the reasoning inside
/// `output_tokens`, as the OpenAI API does, so the cached part is taken out
/// of the input and the output is kept whole.
fn usage(usage: &Value) -> Usage {
    let count = |key: &str| usage[key].as_u64().unwrap_or(0);
    let cached = count("cached_input_tokens");
    Usage {
        input_tokens: count("input_tokens").saturating_sub(cached),
        output_tokens: count("output_tokens"),
        cache_read_input_tokens: cached,
        cache_creation_input_tokens: count("cache_write_input_tokens"),
        ..Usage::default()
    }
}

/// The HTTP status a failure message names: the `status` of the API's error
/// when the message is that JSON, or the number after `unexpected status`
/// (C2, `unknown_model`, `logged_out`).
fn api_status(message: &str) -> Option<u16> {
    if let Ok(error) = serde_json::from_str::<Value>(message) {
        return error["status"]
            .as_u64()
            .and_then(|status| u16::try_from(status).ok());
    }
    let (_, rest) = message.split_once("unexpected status ")?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

fn str_field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

#[cfg(test)]
mod tests {
    use owlshift_contracts::result::RunResult;

    use super::*;

    fn request(role: Role, level: PermissionLevel) -> Request {
        Request {
            workdir: PathBuf::from("/work"),
            model: None,
            effort: None,
            role,
            permissions: Permissions {
                level,
                network: false,
                browser: false,
            },
            schema_file: PathBuf::from("/run/schema.json"),
            last_message_file: PathBuf::from("/run/last-message.json"),
        }
    }

    fn joined(request: &Request) -> String {
        let command = command(Path::new("codex"), request).unwrap();
        let args: Vec<String> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        args.join(" ")
    }

    #[test]
    fn a_critic_run_is_launched_with_the_flags_of_the_design() {
        let mut request = request(Role::Verify, PermissionLevel::ReadOnly);
        assert_eq!(
            joined(&request),
            "exec --ephemeral --json --ignore-user-config --disable apps \
             --disable image_generation -c web_search=\"disabled\" \
             --ignore-rules \
             -c skills.include_instructions=false -c project_doc_max_bytes=0 \
             --dangerously-bypass-approvals-and-sandbox \
             --output-schema /run/schema.json -o /run/last-message.json -"
        );
        request.model = Some("gpt-6-astra".into());
        request.effort = Some(Effort::Xhigh);
        assert!(
            joined(&request).ends_with(
                "-o /run/last-message.json -m gpt-6-astra -c model_reasoning_effort=xhigh -"
            ),
            "{}",
            joined(&request)
        );
        let command = command(Path::new("codex"), &request).unwrap();
        assert_eq!(command.get_current_dir(), Some(Path::new("/work")));
        assert_eq!(command.get_envs().count(), 0);
    }

    #[test]
    fn efforts_map_one_to_one() {
        let levels = [
            Effort::Low,
            Effort::Medium,
            Effort::High,
            Effort::Xhigh,
            Effort::Max,
        ];
        let mapped: Vec<&str> = levels.into_iter().map(reasoning_effort).collect();
        assert_eq!(mapped, ["low", "medium", "high", "xhigh", "max"]);
    }

    #[test]
    fn only_a_read_only_critic_is_run() {
        let writes = request(Role::Verify, PermissionLevel::WriteWorktree);
        let refused = command(Path::new("codex"), &writes)
            .unwrap_err()
            .to_string();
        assert!(refused.contains("read-only profile"), "{refused}");
        let mut browser = request(Role::DesignReview, PermissionLevel::ReadOnly);
        browser.permissions.browser = true;
        assert!(command(Path::new("codex"), &browser).is_err());
        for role in [Role::Build, Role::Design, Role::Arbiter, Role::Intake] {
            let refused = command(
                Path::new("codex"),
                &request(role, PermissionLevel::ReadOnly),
            )
            .unwrap_err()
            .to_string();
            assert!(refused.contains("critic only"), "{refused}");
        }
    }

    /// C2: the API refuses a strict schema whose object leaves a property
    /// out of `required`, and every object must close its properties.
    #[test]
    fn the_schema_is_strict_and_matches_the_result_contract() {
        fn check(schema: &Value, path: &str) {
            if let Some(properties) = schema["properties"].as_object() {
                let mut required: Vec<&str> = schema["required"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|key| key.as_str().unwrap())
                    .collect();
                let mut keys: Vec<&str> = properties.keys().map(String::as_str).collect();
                required.sort_unstable();
                keys.sort_unstable();
                assert_eq!(required, keys, "{path}");
                assert_eq!(schema["additionalProperties"], false, "{path}");
                for (key, property) in properties {
                    check(property, &format!("{path}.{key}"));
                }
            }
            if schema.get("items").is_some() {
                check(&schema["items"], &format!("{path}[]"));
            }
        }
        let schema = review_schema();
        check(&schema, "$");

        // A message the schema allows is a valid result.
        let message = json!({
            "format": RESULT_FORMAT,
            "status": "done",
            "summary": "One finding.",
            "findings": [
                {"id": "F1", "severity": "major", "claim": "c", "evidence": "a.rs:1",
                 "criterion": null, "settle": "s"},
                {"id": "F2", "severity": "minor", "claim": "c", "evidence": "a.rs:2",
                 "criterion": "criterion 1", "settle": "s"}
            ]
        });
        RunResult::parse(&message.to_string()).unwrap();
        for status in ["blocked", "failed"] {
            let message = json!({"format": RESULT_FORMAT, "status": status,
                                 "summary": "s", "findings": []});
            RunResult::parse(&message.to_string()).unwrap();
        }
        // Every severity the schema names is one of the contract's.
        for severity in schema["properties"]["findings"]["items"]["properties"]["severity"]["enum"]
            .as_array()
            .unwrap()
        {
            serde_json::from_value::<owlshift_contracts::review::Severity>(severity.clone())
                .unwrap();
        }
    }

    fn replay(lines: &[&str], exit_code: Option<i32>) -> Run {
        let mut transcript = Transcript::default();
        for line in lines {
            transcript.feed(line.as_bytes());
        }
        transcript.finish(exit_code, Vec::new())
    }

    const STARTED: &str = r#"{"type":"thread.started","thread_id":"t"}"#;
    const MESSAGE: &str =
        r#"{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"{}"}}"#;
    const COMPLETED: &str = r#"{"type":"turn.completed","usage":{"input_tokens":100,"cached_input_tokens":60,"cache_write_input_tokens":5,"output_tokens":7,"reasoning_output_tokens":3}}"#;

    #[test]
    fn a_completed_turn_gives_the_last_message_and_the_usage() {
        let run = replay(&[STARTED, MESSAGE, COMPLETED], Some(0));
        assert_eq!(run.outcome, Outcome::Completed { text: "{}".into() });
        let usage = run.usage.unwrap();
        assert_eq!(
            (
                usage.input_tokens,
                usage.cache_read_input_tokens,
                usage.cache_creation_input_tokens,
                usage.output_tokens
            ),
            (40, 60, 5, 7)
        );
        assert_eq!(usage.cost_usd, None);

        // The exit status and the stream must agree.
        let run = replay(&[STARTED, MESSAGE, COMPLETED], Some(1));
        assert!(matches!(
            run.outcome,
            Outcome::Failed(Failure::Error {
                exit_code: Some(1),
                message: None,
                ..
            })
        ));
        assert_eq!(
            replay(&[STARTED, MESSAGE], Some(0)).outcome,
            Outcome::Failed(Failure::Unfinished)
        );
        assert_eq!(
            replay(&[STARTED, COMPLETED], None).outcome,
            Outcome::Failed(Failure::Signal)
        );
        assert_eq!(
            replay(&["noise\n", ""], Some(1)).outcome,
            Outcome::Failed(Failure::NoThread)
        );
    }

    /// C7 captured the first message; the binary holds the other two. A
    /// limit counts only on an `error` or `turn.failed` event, never in what
    /// the model wrote.
    #[test]
    fn a_usage_limit_is_read_from_error_events_only() {
        for limit in LIMIT_MESSAGES {
            let failed = format!(
                r#"{{"type":"turn.failed","error":{{"message":"{limit}. Try again at 1:45 PM."}}}}"#
            );
            assert_eq!(
                replay(&[STARTED, &failed], Some(1)).outcome,
                Outcome::UsageLimit
            );
        }
        let quoted = r#"{"type":"item.completed","item":{"type":"agent_message","text":"You've hit your usage limit"}}"#;
        let failed = r#"{"type":"turn.failed","error":{"message":"boom"}}"#;
        assert!(matches!(
            replay(&[STARTED, quoted, failed], Some(1)).outcome,
            Outcome::Failed(Failure::Error { .. })
        ));
    }

    /// OWL-317: a collaboration call is read from Codex's own items, from
    /// any of their three events, each tool once; never from what the model
    /// wrote. The outcome is left to the runner.
    #[test]
    fn a_collaboration_call_is_read_from_codex_items_only() {
        let item = |event: &str, tool: &str| {
            format!(
                r#"{{"type":"{event}","item":{{"id":"item_1","type":"collab_tool_call"{tool},"status":"completed"}}}}"#
            )
        };
        for event in ["item.started", "item.updated", "item.completed"] {
            let call = item(event, r#","tool":"wait""#);
            let run = replay(&[STARTED, &call, MESSAGE, COMPLETED], Some(0));
            assert_eq!(run.collaboration, ["wait"], "{event}");
            assert_eq!(run.outcome, Outcome::Completed { text: "{}".into() });
        }
        let (started, completed) = (
            item("item.started", r#","tool":"wait""#),
            item("item.completed", r#","tool":"wait""#),
        );
        let spawn = item("item.completed", r#","tool":"spawn_agent""#);
        let unnamed = item("item.completed", "");
        let run = replay(
            &[STARTED, &started, &completed, &spawn, &unnamed, COMPLETED],
            Some(0),
        );
        assert_eq!(run.collaboration, ["wait", "spawn_agent", "unnamed"]);

        let named = r#"{"type":"item.completed","item":{"type":"agent_message","text":"collab_tool_call spawn_agent"}}"#;
        let shell = r#"{"type":"item.completed","item":{"type":"command_execution","command":"spawn_agent","tool":"wait"}}"#;
        let run = replay(&[STARTED, named, shell, COMPLETED], Some(0));
        assert!(run.collaboration.is_empty(), "{:?}", run.collaboration);
    }

    /// OWL-319: the stop is called once, at the first collaboration item and
    /// not before; a stream without one never calls it.
    #[test]
    fn the_stop_is_called_at_the_first_collaboration_item_only() {
        let wait = r#"{"type":"item.started","item":{"type":"collab_tool_call","tool":"wait"}}"#;
        let calls = std::cell::Cell::new(0);
        let mut guarded = Guarded {
            transcript: Transcript::default(),
            stop: Some(|| calls.set(calls.get() + 1)),
        };
        for (line, expected) in [(STARTED, 0), (MESSAGE, 0), (wait, 1), (wait, 1)] {
            guarded.line(line.as_bytes());
            assert_eq!(calls.get(), expected, "{line}");
        }
        let mut guarded = Guarded {
            transcript: Transcript::default(),
            stop: Some(|| calls.set(calls.get() + 1)),
        };
        for line in [STARTED, MESSAGE, COMPLETED] {
            guarded.line(line.as_bytes());
        }
        assert_eq!(calls.get(), 1);
        assert!(guarded.stop.is_some());
    }

    #[test]
    fn the_api_status_is_read_from_either_shape_of_message() {
        assert_eq!(
            api_status(r#"{"type":"error","status":400,"error":{"message":"m"}}"#),
            Some(400)
        );
        assert_eq!(
            api_status("unexpected status 401 Unauthorized: Missing bearer"),
            Some(401)
        );
        assert_eq!(api_status("boom"), None);
        assert_eq!(api_status(r#"{"error":"no status"}"#), None);
    }
}
