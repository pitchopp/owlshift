//! A run's log as the ticket page shows it (P8 part 6, OWL-471): one
//! harness line in, the records the page draws out. A pure classifier over
//! `serde_json::Value`, separate from the adapters' usage parsers, which
//! read a log for what the runner needs and drop the rest.
//!
//! The format of a line is told by its own `type` field, so no harness name
//! is needed: Claude Code's `stream-json` and Codex's `--json` are read
//! side by side. Nothing is dropped:
//!
//! - every line that is not blank gives at least one record, carrying its
//!   line number and its byte offset in the log;
//! - each content block of a Claude `assistant` or `user` message, and each
//!   Codex item, gives a record of its own;
//! - a line that is not JSON, a JSON line of a `type` neither dialect is
//!   known to write, a `system` subtype, a content block or an item of a
//!   kind the classifier does not know gives an [`Kind::Unknown`] record
//!   holding the text it could not read, whole and as it is. Only the 8 MiB
//!   rule of [`super::tail`] ever shortens a line, never the classifier.
//!
//! Only the shapes the recorded fixtures hold are named
//! (`crates/owlshift-adapters/tests/fixtures`): Codex's `todo_list` items
//! and its `item.updated` event were never recorded, because `codex exec`
//! 0.154.0 does not offer the plan tool (build plan, P8 part 6, OWL-502),
//! so they show as unknown until a Codex version records them (AGENTS.md:
//! an assumption about an external tool is verified before code depends on
//! it). A web_search item carries the key `id` twice; `Value` keeps the
//! last, which is why this module never parses an item into a typed struct.

use serde_json::{Map, Value, json};

/// The most characters a one-line summary holds.
const SUMMARY_CHARS: usize = 160;

/// What a record is, which tells the page how to draw it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// The agent's words, or a user message's.
    Text,
    /// The agent's reasoning, as the harness shows it.
    Thinking,
    /// A tool call, folded: its name, a one-line input and the whole input
    /// as the body.
    Tool,
    /// A tool's result, folded.
    ToolResult,
    /// A record of the harness's own: a start, a notice, a progress.
    Status,
    /// What a turn or a run used and cost.
    Usage,
    /// How the run or the turn ended.
    Result,
    /// A record the classifier does not know, as it is.
    Unknown,
}

impl Kind {
    /// The name the page's JSON gives it.
    pub fn name(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Thinking => "thinking",
            Self::Tool => "tool",
            Self::ToolResult => "tool_result",
            Self::Status => "status",
            Self::Usage => "usage",
            Self::Result => "result",
            Self::Unknown => "unknown",
        }
    }
}

/// One record of the page.
#[derive(Clone, Debug, PartialEq)]
pub struct ViewRecord {
    /// The line of the log it comes from, from 1.
    pub line: u64,
    /// The byte offset of that line in the log.
    pub offset: u64,
    pub kind: Kind,
    /// What a Codex item's records share (its started and its completed),
    /// a Claude tool call's id (which its result names), `None` otherwise.
    pub id: Option<String>,
    /// A short title: a tool's name, a status's subtype, why a record is
    /// unknown.
    pub label: Option<String>,
    /// The text, or a one-line summary when there is a body.
    pub text: String,
    /// What a fold holds.
    pub body: Option<String>,
    /// The state of a Codex tool call (`in_progress`, `completed`).
    pub state: Option<String>,
    /// Whether it tells of a failure.
    pub error: bool,
    /// The figures of a usage record.
    pub usage: Option<Value>,
}

impl ViewRecord {
    fn new(line: u64, offset: u64, kind: Kind, text: impl Into<String>) -> Self {
        Self {
            line,
            offset,
            kind,
            id: None,
            label: None,
            text: text.into(),
            body: None,
            state: None,
            error: false,
            usage: None,
        }
    }

    fn label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }

    fn id(mut self, id: Option<&str>) -> Self {
        self.id = id.map(str::to_owned);
        self
    }

    fn body(mut self, body: impl Into<String>) -> Self {
        self.body = Some(body.into());
        self
    }

    fn failed(mut self, error: bool) -> Self {
        self.error = error;
        self
    }

    /// The record as the page's JSON.
    pub fn json(&self) -> Value {
        let mut map = Map::new();
        map.insert("line".into(), self.line.into());
        map.insert("offset".into(), self.offset.into());
        map.insert("kind".into(), self.kind.name().into());
        map.insert("text".into(), self.text.clone().into());
        for (key, value) in [
            ("id", &self.id),
            ("label", &self.label),
            ("body", &self.body),
            ("state", &self.state),
        ] {
            if let Some(value) = value {
                map.insert(key.into(), value.clone().into());
            }
        }
        if self.error {
            map.insert("error".into(), true.into());
        }
        if let Some(usage) = &self.usage {
            map.insert("usage".into(), usage.clone());
        }
        Value::Object(map)
    }
}

/// A line the classifier cannot read: the whole text, as it is.
fn unknown(line: u64, offset: u64, why: impl Into<String>, raw: impl Into<String>) -> ViewRecord {
    ViewRecord::new(line, offset, Kind::Unknown, raw).label(why)
}

/// A line [`super::tail`] cut: `bytes` are its first bytes, `length` its
/// whole length in bytes. The one record whose text is not the whole line,
/// and it says so.
pub fn cut(line: u64, offset: u64, bytes: &[u8], length: u64) -> ViewRecord {
    let shown = String::from_utf8_lossy(bytes).into_owned();
    unknown(
        line,
        offset,
        format!("line cut: {length} bytes, the first {} shown", bytes.len()),
        shown,
    )
}

/// A line of a log that is plain text, such as a run's stderr: a text
/// record, none for a blank line.
pub fn plain(line: u64, offset: u64, bytes: &[u8]) -> Vec<ViewRecord> {
    let text = String::from_utf8_lossy(bytes);
    let text = text.strip_suffix('\r').unwrap_or(&text);
    if text.trim().is_empty() {
        return Vec::new();
    }
    vec![ViewRecord::new(line, offset, Kind::Text, text).label("stderr")]
}

/// `value` as indented JSON.
fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string())
}

/// The first line of `text`, cut to [`SUMMARY_CHARS`] characters.
fn one_line(text: &str) -> String {
    let first = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let first = first.trim();
    if first.chars().count() > SUMMARY_CHARS {
        let cut: String = first.chars().take(SUMMARY_CHARS).collect();
        format!("{cut}…")
    } else if text.trim().lines().count() > 1 {
        format!("{first} …")
    } else {
        first.to_owned()
    }
}

fn str_of<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

/// A scalar field as text, for a summary.
fn scalar(value: &Value, key: &str) -> Option<String> {
    match value.get(key)? {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// The records of one log line: `line` counts from 1, `offset` is the
/// line's first byte in the log, `bytes` the line with or without its line
/// break. A blank line gives none.
pub fn classify(line: u64, offset: u64, bytes: &[u8]) -> Vec<ViewRecord> {
    let text = String::from_utf8_lossy(bytes);
    let raw = text.strip_suffix('\n').unwrap_or(&text);
    let raw = raw.strip_suffix('\r').unwrap_or(raw);
    if raw.trim().is_empty() {
        return Vec::new();
    }
    let Ok(value) = serde_json::from_str::<Value>(raw) else {
        return vec![unknown(line, offset, "not JSON", raw)];
    };
    let Some(kind) = value.get("type").and_then(Value::as_str) else {
        return vec![unknown(line, offset, "no record type", raw)];
    };
    let known = match kind {
        "system" => Some(system(line, offset, &value, raw)),
        "assistant" => message(line, offset, &value, false),
        "user" => message(line, offset, &value, true),
        "tool_progress" => Some(vec![tool_progress(line, offset, &value)]),
        "rate_limit_event" => Some(vec![rate_limit(line, offset, &value)]),
        "result" => Some(claude_result(line, offset, &value)),
        "thread.started" => Some(vec![
            ViewRecord::new(
                line,
                offset,
                Kind::Status,
                format!("thread {}", str_of(&value, "thread_id").unwrap_or("")),
            )
            .label("thread.started")
            .body(pretty(&value)),
        ]),
        "turn.started" => Some(vec![
            ViewRecord::new(line, offset, Kind::Status, "turn started")
                .label("turn.started")
                .body(pretty(&value)),
        ]),
        "item.started" | "item.completed" => item(line, offset, &value),
        "turn.completed" => Some(vec![turn_completed(line, offset, &value)]),
        "turn.failed" => Some(vec![turn_failed(line, offset, &value)]),
        "error" => Some(vec![
            ViewRecord::new(
                line,
                offset,
                Kind::Status,
                str_of(&value, "message").unwrap_or("error"),
            )
            .label("error")
            .failed(true)
            .body(pretty(&value)),
        ]),
        _ => None,
    };
    known.unwrap_or_else(|| {
        let why = match value.get("type").and_then(Value::as_str) {
            Some(kind) => format!("unknown record type {kind}"),
            None => "no record type".to_owned(),
        };
        vec![unknown(line, offset, why, raw)]
    })
}

/// A Claude `system` record: its known subtypes are status records, any
/// other is unknown.
fn system(line: u64, offset: u64, value: &Value, raw: &str) -> Vec<ViewRecord> {
    let subtype = str_of(value, "subtype").unwrap_or("");
    let text = match subtype {
        "init" => {
            let tools = value
                .get("tools")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            format!(
                "{} · {} tools · {}",
                str_of(value, "model").unwrap_or("?"),
                tools,
                str_of(value, "cwd").unwrap_or("")
            )
        }
        "informational" => str_of(value, "content").unwrap_or("").to_owned(),
        "permission_denied" => format!(
            "{}: {}",
            str_of(value, "tool_name").unwrap_or("?"),
            one_line(str_of(value, "message").unwrap_or(""))
        ),
        "post_turn_summary" => str_of(value, "status_detail").unwrap_or("").to_owned(),
        "task_notification" => format!(
            "{}: {}",
            str_of(value, "status").unwrap_or("?"),
            str_of(value, "summary").unwrap_or("")
        ),
        "task_started" => str_of(value, "description").unwrap_or("").to_owned(),
        "thinking_tokens" => format!(
            "about {} thinking tokens",
            scalar(value, "estimated_tokens").unwrap_or_default()
        ),
        _ => {
            return vec![unknown(
                line,
                offset,
                format!("unknown system subtype {subtype}"),
                raw,
            )];
        }
    };
    let warned = matches!(subtype, "permission_denied")
        || str_of(value, "level").is_some_and(|level| level == "error");
    vec![
        ViewRecord::new(line, offset, Kind::Status, text)
            .label(subtype)
            .failed(warned)
            .body(pretty(value)),
    ]
}

/// What a Claude tool call's input says in a line.
fn input_summary(input: &Value) -> String {
    if let Some(map) = input.as_object() {
        for key in [
            "command",
            "file_path",
            "path",
            "pattern",
            "query",
            "url",
            "description",
            "prompt",
        ] {
            if let Some(text) = map.get(key).and_then(Value::as_str) {
                return one_line(text);
            }
        }
    }
    one_line(&input.to_string())
}

/// An `assistant` or `user` message: one record per content block.
fn message(line: u64, offset: u64, value: &Value, user: bool) -> Option<Vec<ViewRecord>> {
    let content = value.get("message")?.get("content")?;
    let label = if user { "user" } else { "assistant" };
    match content {
        Value::String(text) => Some(vec![
            ViewRecord::new(line, offset, Kind::Text, text.clone()).label(label),
        ]),
        Value::Array(blocks) if blocks.is_empty() => Some(vec![
            ViewRecord::new(line, offset, Kind::Status, format!("empty {label} message"))
                .label(label)
                .body(pretty(value)),
        ]),
        Value::Array(blocks) => Some(
            blocks
                .iter()
                .map(|block| {
                    if user {
                        user_block(line, offset, block)
                    } else {
                        assistant_block(line, offset, block)
                    }
                })
                .collect(),
        ),
        _ => None,
    }
}

fn assistant_block(line: u64, offset: u64, block: &Value) -> ViewRecord {
    match str_of(block, "type") {
        Some("text") if block.get("text").is_some_and(Value::is_string) => ViewRecord::new(
            line,
            offset,
            Kind::Text,
            str_of(block, "text").unwrap_or_default(),
        )
        .label("assistant"),
        Some("thinking") if block.get("thinking").is_some_and(Value::is_string) => ViewRecord::new(
            line,
            offset,
            Kind::Thinking,
            str_of(block, "thinking").unwrap_or_default(),
        )
        .label("thinking"),
        Some("tool_use") if str_of(block, "name").is_some() => {
            let input = block.get("input").cloned().unwrap_or(Value::Null);
            ViewRecord::new(line, offset, Kind::Tool, input_summary(&input))
                .label(str_of(block, "name").unwrap_or_default())
                .id(str_of(block, "id"))
                .body(pretty(&input))
        }
        other => unknown(
            line,
            offset,
            format!(
                "unknown content block {}",
                other.unwrap_or("without a type")
            ),
            block.to_string(),
        ),
    }
}

fn user_block(line: u64, offset: u64, block: &Value) -> ViewRecord {
    match str_of(block, "type") {
        Some("tool_result") => {
            let (summary, body) = result_content(block.get("content"));
            let mut record = ViewRecord::new(line, offset, Kind::ToolResult, summary)
                .label("result")
                .id(str_of(block, "tool_use_id"))
                .failed(block.get("is_error").and_then(Value::as_bool) == Some(true));
            if !body.is_empty() {
                record = record.body(body);
            }
            record
        }
        Some("text") if block.get("text").is_some_and(Value::is_string) => ViewRecord::new(
            line,
            offset,
            Kind::Text,
            str_of(block, "text").unwrap_or_default(),
        )
        .label("user"),
        other => unknown(
            line,
            offset,
            format!(
                "unknown content block {}",
                other.unwrap_or("without a type")
            ),
            block.to_string(),
        ),
    }
}

/// A tool result's `content`, a string or a list of blocks: its one-line
/// summary and its whole text. A block of a kind not known is shown as its
/// raw JSON.
fn result_content(content: Option<&Value>) -> (String, String) {
    let body = match content {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .map(|block| match str_of(block, "type") {
                Some("text") if block.get("text").is_some_and(Value::is_string) => {
                    str_of(block, "text").unwrap_or_default().to_owned()
                }
                Some("tool_reference") => {
                    format!(
                        "tool reference: {}",
                        str_of(block, "tool_name").unwrap_or("?")
                    )
                }
                _ => block.to_string(),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Some(other) => other.to_string(),
    };
    (one_line(&body), body)
}

fn tool_progress(line: u64, offset: u64, value: &Value) -> ViewRecord {
    let text = format!(
        "{} running, {} s",
        str_of(value, "tool_name").unwrap_or("a tool"),
        scalar(value, "elapsed_time_seconds").unwrap_or_else(|| "?".into())
    );
    ViewRecord::new(line, offset, Kind::Status, text)
        .label("tool_progress")
        .id(str_of(value, "tool_use_id"))
        .body(pretty(value))
}

fn rate_limit(line: u64, offset: u64, value: &Value) -> ViewRecord {
    let info = value.get("rate_limit_info").unwrap_or(&Value::Null);
    let text = format!(
        "{} {}",
        str_of(info, "rateLimitType").unwrap_or("rate limit"),
        str_of(info, "status").unwrap_or("")
    );
    ViewRecord::new(line, offset, Kind::Status, text.trim())
        .label("rate_limit")
        .body(pretty(value))
}

/// The usage figures of a Claude `usage` object or a Codex one, under
/// common names.
fn usage_figures(usage: &Value, cost: Option<f64>) -> Value {
    let number = |keys: &[&str]| {
        keys.iter()
            .find_map(|key| usage.get(key).and_then(Value::as_u64))
    };
    let mut map = Map::new();
    for (name, keys) in [
        ("input_tokens", &["input_tokens"][..]),
        ("output_tokens", &["output_tokens"][..]),
        (
            "cache_read_tokens",
            &["cache_read_input_tokens", "cached_input_tokens"][..],
        ),
        (
            "cache_write_tokens",
            &["cache_creation_input_tokens", "cache_write_input_tokens"][..],
        ),
        ("reasoning_tokens", &["reasoning_output_tokens"][..]),
    ] {
        if let Some(n) = number(keys) {
            map.insert(name.into(), n.into());
        }
    }
    if let Some(cost) = cost {
        map.insert("cost_usd".into(), json!(cost));
    }
    Value::Object(map)
}

fn usage_record(line: u64, offset: u64, figures: Value) -> ViewRecord {
    let part = |key: &str| figures.get(key).and_then(Value::as_u64);
    let mut text = format!(
        "{} in, {} out",
        part("input_tokens").unwrap_or(0),
        part("output_tokens").unwrap_or(0)
    );
    if let Some(cost) = figures.get("cost_usd").and_then(Value::as_f64) {
        text.push_str(&format!(", ${cost:.4}"));
    }
    let mut record = ViewRecord::new(line, offset, Kind::Usage, text).label("usage");
    record.usage = Some(figures);
    record
}

/// Claude's `result`: how the run ended, and what it used.
fn claude_result(line: u64, offset: u64, value: &Value) -> Vec<ViewRecord> {
    let subtype = str_of(value, "subtype").unwrap_or("");
    let failed = value.get("is_error").and_then(Value::as_bool) == Some(true)
        || (!subtype.is_empty() && subtype != "success");
    let text = str_of(value, "result")
        .map(str::to_owned)
        .unwrap_or_else(|| subtype.to_owned());
    let mut records = vec![
        ViewRecord::new(line, offset, Kind::Result, one_line(&text))
            .label(subtype)
            .failed(failed)
            .body(pretty(value)),
    ];
    if let Some(usage) = value.get("usage") {
        let cost = value.get("total_cost_usd").and_then(Value::as_f64);
        records.push(usage_record(line, offset, usage_figures(usage, cost)));
    }
    records
}

/// A Codex `item.started` or `item.completed`: one record by the item's
/// type; `None` for a type not known, which the caller shows whole.
fn item(line: u64, offset: u64, value: &Value) -> Option<Vec<ViewRecord>> {
    let item = value.get("item").filter(|item| item.is_object())?;
    let id = str_of(item, "id");
    let state = str_of(item, "status").map(str::to_owned);
    let record = match str_of(item, "type")? {
        "agent_message" => {
            ViewRecord::new(line, offset, Kind::Text, str_of(item, "text")?).label("assistant")
        }
        "command_execution" => {
            let mut body = String::new();
            if let Some(output) = str_of(item, "aggregated_output") {
                body.push_str(output);
            }
            if let Some(code) = scalar(item, "exit_code") {
                if !body.is_empty() && !body.ends_with('\n') {
                    body.push('\n');
                }
                body.push_str(&format!("exit code {code}"));
            }
            let failed = item
                .get("exit_code")
                .and_then(Value::as_i64)
                .is_some_and(|code| code != 0);
            ViewRecord::new(line, offset, Kind::Tool, one_line(str_of(item, "command")?))
                .label("command")
                .body(body)
                .failed(failed)
        }
        "mcp_tool_call" => {
            let call = format!(
                "{}.{}",
                str_of(item, "server").unwrap_or("?"),
                str_of(item, "tool").unwrap_or("?")
            );
            let arguments = item.get("arguments").cloned().unwrap_or(Value::Null);
            let failed = item.get("error").is_some_and(|error| !error.is_null());
            ViewRecord::new(line, offset, Kind::Tool, input_summary(&arguments))
                .label(call)
                .body(pretty(item))
                .failed(failed)
        }
        "collab_tool_call" => ViewRecord::new(
            line,
            offset,
            Kind::Tool,
            str_of(item, "tool").unwrap_or("collaboration"),
        )
        .label("collab")
        .body(pretty(item)),
        // Recorded 2026-11-07 (codex-cli 0.154.0, `items_shapes`): `text`
        // holds the reasoning summary.
        "reasoning" => {
            ViewRecord::new(line, offset, Kind::Thinking, str_of(item, "text")?).label("reasoning")
        }
        // `changes[]` of `{path, kind}`; `status` goes in_progress, then
        // completed. A `failed` status was never recorded, so it sets no
        // error flag: the state shows it as it is.
        "file_change" => {
            let changes: Vec<String> = item
                .get("changes")?
                .as_array()?
                .iter()
                .map(|change| {
                    format!(
                        "{} {}",
                        str_of(change, "kind").unwrap_or("change"),
                        str_of(change, "path").unwrap_or("?")
                    )
                })
                .collect();
            ViewRecord::new(line, offset, Kind::Tool, one_line(&changes.join("\n")))
                .label("file_change")
                .body(pretty(item))
        }
        "web_search" => ViewRecord::new(line, offset, Kind::Tool, search_summary(item))
            .label("web_search")
            .body(pretty(item)),
        "error" => ViewRecord::new(line, offset, Kind::Status, str_of(item, "message")?)
            .label("error")
            .failed(true)
            .body(pretty(item)),
        _ => return None,
    };
    let mut record = record.id(id);
    record.state = state;
    Some(vec![record])
}

/// What a web_search item searched for: its `query`, which the started
/// record leaves empty, else the `search` action's `query` or `queries[]`.
fn search_summary(item: &Value) -> String {
    let action = item.get("action");
    let queries = action
        .and_then(|action| action.get("queries"))
        .and_then(Value::as_array)
        .map(|queries| {
            queries
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        });
    [
        str_of(item, "query").map(str::to_owned),
        action
            .and_then(|action| str_of(action, "query"))
            .map(str::to_owned),
        queries,
    ]
    .into_iter()
    .flatten()
    .find(|query| !query.trim().is_empty())
    .map_or_else(|| "searching".to_owned(), |query| one_line(&query))
}

fn turn_completed(line: u64, offset: u64, value: &Value) -> ViewRecord {
    match value.get("usage") {
        Some(usage) => usage_record(line, offset, usage_figures(usage, None)),
        None => ViewRecord::new(line, offset, Kind::Status, "turn completed")
            .label("turn.completed")
            .body(pretty(value)),
    }
}

fn turn_failed(line: u64, offset: u64, value: &Value) -> ViewRecord {
    let message = value
        .get("error")
        .and_then(|error| str_of(error, "message"))
        .unwrap_or("the turn failed");
    ViewRecord::new(line, offset, Kind::Result, one_line(message))
        .label("turn.failed")
        .failed(true)
        .body(pretty(value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::path::PathBuf;

    fn fixtures() -> Vec<PathBuf> {
        let root =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../owlshift-adapters/tests/fixtures");
        let mut files = Vec::new();
        for dir in ["claude", "codex"] {
            for entry in std::fs::read_dir(root.join(dir)).expect("the fixtures folder") {
                let path = entry.expect("an entry").path();
                if path.extension().is_some_and(|ext| ext == "jsonl") {
                    files.push(path);
                }
            }
        }
        files.sort();
        files
    }

    /// The fixture lines whose shape the classifier shows as unknown, each
    /// with why: expected empty, every recorded shape being known.
    const LISTED_UNKNOWN: [(&str, u64); 0] = [];

    #[test]
    fn every_fixture_line_is_shown() {
        let files = fixtures();
        assert!(files.len() >= 30, "{} fixtures", files.len());
        for path in files {
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            let text = std::fs::read(&path).unwrap();
            let mut shown = BTreeSet::new();
            let mut wanted = BTreeSet::new();
            let mut offset = 0u64;
            for (n, line) in text.split_inclusive(|&b| b == b'\n').enumerate() {
                let number = n as u64 + 1;
                let records = classify(number, offset, line);
                assert!(
                    records
                        .iter()
                        .all(|r| r.line == number && r.offset == offset)
                );
                if !String::from_utf8_lossy(line).trim().is_empty() {
                    wanted.insert(number);
                    assert!(!records.is_empty(), "{name}:{number} gives no record");
                }
                if !records.is_empty() {
                    shown.insert(number);
                }
                let value: Value = serde_json::from_slice(line).unwrap_or(Value::Null);
                match value["type"].as_str() {
                    Some("assistant" | "user") => {
                        let blocks = value["message"]["content"].as_array().map_or(1, Vec::len);
                        assert!(records.len() >= blocks, "{name}:{number}");
                    }
                    Some("item.started" | "item.completed") => {
                        assert_eq!(
                            records.iter().filter(|r| r.kind != Kind::Usage).count(),
                            1,
                            "{name}:{number}"
                        );
                    }
                    _ => {}
                }
                for record in &records {
                    if record.kind == Kind::Unknown {
                        assert!(
                            LISTED_UNKNOWN.contains(&(name.as_str(), number)),
                            "{name}:{number} is unknown: {:?}",
                            record.label
                        );
                    }
                }
                offset += line.len() as u64;
            }
            assert_eq!(shown, wanted, "{name}");
        }
    }

    fn one(line: &str) -> Vec<ViewRecord> {
        classify(1, 0, line.as_bytes())
    }

    #[test]
    fn a_text_and_a_tool_use_give_a_text_and_a_folded_tool() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Reading."},{"type":"tool_use","id":"t1","name":"Read","input":{"file_path":"/a/b.rs","limit":5}}]}}"#;
        let records = one(line);
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].kind, Kind::Text);
        assert_eq!(records[0].text, "Reading.");
        assert_eq!(records[1].kind, Kind::Tool);
        assert_eq!(records[1].label.as_deref(), Some("Read"));
        assert_eq!(records[1].text, "/a/b.rs");
        assert_eq!(records[1].id.as_deref(), Some("t1"));
        assert!(records[1].body.as_deref().unwrap().contains("\"limit\": 5"));
    }

    #[test]
    fn an_unknown_block_is_shown_beside_the_known_ones() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hi"},{"type":"server_tool_use","name":"x"}]}}"#;
        let records = one(line);
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].kind, Kind::Text);
        assert_eq!(records[1].kind, Kind::Unknown);
        let block: Value = serde_json::from_str(&records[1].text).unwrap();
        assert_eq!(block, json!({"type": "server_tool_use", "name": "x"}));
    }

    #[test]
    fn a_tool_result_of_a_list_shows_its_blocks() {
        let line = r#"{"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","is_error":true,"content":[{"type":"text","text":"one\ntwo"},{"type":"tool_reference","tool_name":"WebSearch"},{"type":"image","source":"x"}]}]}}"#;
        let records = one(line);
        assert_eq!(records.len(), 1);
        let record = &records[0];
        assert_eq!(record.kind, Kind::ToolResult);
        assert!(record.error);
        assert_eq!(record.id.as_deref(), Some("t1"));
        let body = record.body.as_deref().unwrap();
        assert!(body.contains("one\ntwo"));
        assert!(body.contains("tool reference: WebSearch"));
        assert!(body.contains(r#""type":"image""#));
    }

    #[test]
    fn a_system_record_of_an_unlisted_subtype_is_unknown() {
        let line = r#"{"type":"system","subtype":"brand_new","x":1}"#;
        let records = one(line);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].kind, Kind::Unknown);
        assert_eq!(records[0].text, line);
        let known = one(r#"{"type":"system","subtype":"thinking_tokens","estimated_tokens":50}"#);
        assert_eq!(known[0].kind, Kind::Status);
        assert_eq!(known[0].text, "about 50 thinking tokens");
    }

    #[test]
    fn a_claude_result_gives_a_result_and_a_usage_record() {
        let line = r#"{"type":"result","subtype":"success","is_error":false,"result":"Done.","total_cost_usd":0.0225,"usage":{"input_tokens":4,"output_tokens":197,"cache_read_input_tokens":10068,"cache_creation_input_tokens":10249}}"#;
        let records = one(line);
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].kind, Kind::Result);
        assert_eq!(records[0].text, "Done.");
        assert!(!records[0].error);
        let usage = records[1].usage.as_ref().unwrap();
        assert_eq!(usage["input_tokens"], 4);
        assert_eq!(usage["output_tokens"], 197);
        assert_eq!(usage["cache_read_tokens"], 10068);
        assert_eq!(usage["cache_write_tokens"], 10249);
        assert_eq!(usage["cost_usd"], 0.0225);
        let failed = one(r#"{"type":"result","subtype":"error_max_turns","is_error":true}"#);
        assert!(failed[0].error);
        assert_eq!(failed.len(), 1);
    }

    #[test]
    fn a_codex_turn_gives_usage_and_a_failed_turn_its_message() {
        let usage = one(
            r#"{"type":"turn.completed","usage":{"input_tokens":11935,"cached_input_tokens":5,"output_tokens":195,"reasoning_output_tokens":78}}"#,
        );
        assert_eq!(usage.len(), 1);
        assert_eq!(usage[0].kind, Kind::Usage);
        let figures = usage[0].usage.as_ref().unwrap();
        assert_eq!(figures["input_tokens"], 11935);
        assert_eq!(figures["cache_read_tokens"], 5);
        assert_eq!(figures["reasoning_tokens"], 78);
        let failed = one(r#"{"type":"turn.failed","error":{"message":"it broke"}}"#);
        assert_eq!(failed[0].kind, Kind::Result);
        assert!(failed[0].error);
        assert_eq!(failed[0].text, "it broke");
    }

    #[test]
    fn a_codex_items_started_and_completed_share_an_id() {
        let started = one(
            r#"{"type":"item.started","item":{"id":"item_1","type":"command_execution","command":"ls","aggregated_output":"","exit_code":null,"status":"in_progress"}}"#,
        );
        let done = one(
            r#"{"type":"item.completed","item":{"id":"item_1","type":"command_execution","command":"ls","aggregated_output":"a\nb","exit_code":0,"status":"completed"}}"#,
        );
        assert_eq!(started[0].id, done[0].id);
        assert_eq!(started[0].id.as_deref(), Some("item_1"));
        assert_eq!(started[0].state.as_deref(), Some("in_progress"));
        assert_eq!(done[0].state.as_deref(), Some("completed"));
        assert!(done[0].body.as_deref().unwrap().contains("a\nb"));
        assert!(!done[0].error);
    }

    #[test]
    fn the_recorded_reasoning_file_change_and_web_search_items_are_named() {
        let lines: Vec<String> = std::fs::read_to_string(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../owlshift-adapters/tests/fixtures/codex/items_shapes.jsonl"),
        )
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
        let at = |n: usize| one(&lines[n]);
        let reasoning = at(3);
        assert_eq!(reasoning[0].kind, Kind::Thinking);
        assert_eq!(reasoning[0].text, "**Following remaining steps**");
        assert_eq!(reasoning[0].id.as_deref(), Some("item_1"));

        let (started, done) = (at(9), at(10));
        for record in [&started[0], &done[0]] {
            assert_eq!(record.kind, Kind::Tool);
            assert_eq!(record.label.as_deref(), Some("file_change"));
            assert_eq!(record.text, "add /home/user/repo/VERSION.txt");
        }
        assert_eq!(started[0].id, done[0].id);
        assert_eq!(started[0].id.as_deref(), Some("item_5"));
        assert_eq!(started[0].state.as_deref(), Some("in_progress"));
        assert_eq!(done[0].state.as_deref(), Some("completed"));

        // The two ids of a web_search item: a typed parse would reject the
        // duplicate key, a `Value` keeps the last (the `exec-` one).
        let (s3, d3, s4, d4) = (at(5), at(6), at(7), at(8));
        for record in [&s3, &d3, &s4, &d4] {
            assert_eq!(record.len(), 1);
            assert_eq!(record[0].kind, Kind::Tool);
            assert_eq!(record[0].label.as_deref(), Some("web_search"));
        }
        assert_eq!(s3[0].id, d3[0].id);
        assert_eq!(s4[0].id, d4[0].id);
        assert_ne!(s3[0].id, s4[0].id);
        assert!(s3[0].id.as_deref().unwrap().starts_with("exec-"));
        assert_eq!(s3[0].text, "searching");
        assert_eq!(
            d3[0].text,
            "site.rust-lang.org latest stable Rust release November 2026"
        );
        assert_eq!(d4[0].text, "https://blog.rust-lang.org/releases/");
    }

    #[test]
    fn a_search_names_its_queries_and_a_malformed_item_stays_unknown() {
        let search = one(
            r#"{"type":"item.completed","item":{"id":"i","type":"web_search","query":"","action":{"type":"search","queries":["a","b"]}}}"#,
        );
        assert_eq!(search[0].text, "a, b");
        for line in [
            r#"{"type":"item.completed","item":{"id":"i","type":"file_change"}}"#,
            r#"{"type":"item.completed","item":{"id":"i","type":"reasoning"}}"#,
        ] {
            assert_eq!(one(line)[0].kind, Kind::Unknown, "{line}");
        }
    }

    /// `todo_list` and `item.updated` are unreachable from `codex exec`
    /// 0.154.0 (no plan tool): never recorded, so unknown on purpose.
    #[test]
    fn a_codex_shape_never_recorded_is_unknown() {
        for line in [
            r#"{"type":"item.updated","item":{"id":"i","type":"agent_message","text":"x"}}"#,
            r#"{"type":"item.completed","item":{"id":"i","type":"todo_list","items":[]}}"#,
        ] {
            let records = one(line);
            assert_eq!(records.len(), 1, "{line}");
            assert_eq!(records[0].kind, Kind::Unknown, "{line}");
        }
    }

    #[test]
    fn what_no_dialect_writes_comes_back_unknown_and_whole() {
        let huge = format!(
            r#"{{"type":"brand_new","blob":"{}"}}"#,
            "x".repeat(100 * 1024)
        );
        for line in [
            r#"{"type":"brand_new"}"#,
            "not json",
            "[1,2]",
            huge.as_str(),
        ] {
            let records = classify(7, 42, format!("{line}\n").as_bytes());
            assert_eq!(records.len(), 1, "{line:.40}");
            assert_eq!(records[0].kind, Kind::Unknown);
            assert_eq!(records[0].text, line);
            assert_eq!((records[0].line, records[0].offset), (7, 42));
        }
        assert!(classify(1, 0, b"  \r\n").is_empty());
    }

    #[test]
    fn markup_stays_in_the_text() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"<script>alert(1)</script> &  "}]}}"#;
        let records = one(line);
        assert_eq!(records[0].text, "<script>alert(1)</script> & \u{2028}");
    }
}
