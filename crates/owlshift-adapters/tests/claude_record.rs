//! Records the fixtures `claude_contract.rs` replays, with the installed
//! `claude` on the user's own login. It spends a little of the subscription
//! (`haiku` at effort `low`, and one `sonnet` run for the peer sender), so it
//! never runs in CI:
//!
//! ```sh
//! OWLSHIFT_RECORD_CLAUDE=1 cargo test -p owlshift-adapters --test claude_record -- --ignored [name]
//! ```
//!
//! Each run starts from the adapter's own `command()`, in the environment of
//! C1 (`HOME PATH USER LANG TMPDIR` alone); the edits a recording makes to
//! that command line are the only divergence, and each is named where it is
//! made. A recording is checked before it is written: the version, the exit
//! status, standard error and what the contract test will rely on. Then it
//! is scrubbed by [`Scrub`]: ids, timestamps, thinking signatures, local
//! paths and the user's skills, plugins, agents, commands and memory paths
//! go. `usage_limit.jsonl` is constructed by hand, not recorded (C7).
//!
//! The Linear fixtures have their own recorder, in `tests/support`.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use owlshift_adapters::harness::claude::{Effort, Outcome, Request, Run, command, drive};
use owlshift_contracts::brief::{PermissionLevel, Permissions};
use owlshift_contracts::ids::RelativePath;
use serde_json::{Value, json};

/// The release these fixtures are recorded with: a run on another one fails.
const VERSION: &str = "2.1.284";

const UUID: &str = "00000000-0000-4000-8000-000000000000";
const TIMESTAMP: &str = "2026-09-29T12:00:00.000Z";
/// Where a run's inbox socket appears in the fixtures; the peer sender's is
/// the second.
const SOCKETS: [&str; 2] = ["/tmp/cc-socks/12345.sock", "/tmp/cc-socks/23456.sock"];

/// Keys of the `init` event naming what the user installed, with local paths.
const USER_SETUP: [&str; 6] = [
    "agents",
    "plugins",
    "skills",
    "slash_commands",
    "terminal_slash_commands",
    "memory_paths",
];

fn recording_allowed() {
    assert_eq!(
        std::env::var("OWLSHIFT_RECORD_CLAUDE").as_deref(),
        Ok("1"),
        "recording spends subscription usage: set OWLSHIFT_RECORD_CLAUDE=1 to run it"
    );
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/claude")
        .join(name)
}

fn request(workdir: &Path, level: PermissionLevel, network: bool, result: &str) -> Request {
    Request {
        workdir: workdir.to_path_buf(),
        model: Some("haiku".into()),
        effort: Some(Effort::Low),
        permissions: Permissions {
            level,
            network,
            browser: false,
        },
        result_path: RelativePath::new(result).unwrap(),
        json_schema: None,
        max_budget_usd: None,
    }
}

/// `claude` with `args`, in `dir`, in the environment of C1.
fn claude(args: &[String], dir: &Path) -> Command {
    let mut claude = Command::new("claude");
    claude
        .args(args)
        .current_dir(dir)
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for var in ["HOME", "PATH", "USER", "LANG", "TMPDIR"] {
        if let Some(value) = std::env::var_os(var) {
            claude.env(var, value);
        }
    }
    claude
}

/// The adapter's command for `request`, its arguments passed through `edit`.
fn adapter(request: &Request, edit: impl FnOnce(&mut Vec<String>)) -> Command {
    let mut args: Vec<String> = command(Path::new("claude"), request)
        .unwrap()
        .get_args()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    edit(&mut args);
    claude(&args, &request.workdir)
}

/// The values of `flag` in `args`, up to the next flag.
fn values<'a>(args: &'a [String], flag: &str) -> &'a [String] {
    let at = args.iter().position(|arg| arg == flag).unwrap() + 1;
    let end = args[at..]
        .iter()
        .position(|arg| arg.starts_with("--"))
        .map_or(args.len(), |len| at + len);
    &args[at..end]
}

/// Replaces the value of `--settings`, the adapter's one settings flag.
fn set_settings(args: &mut [String], settings: &Value) {
    let at = args.iter().position(|arg| arg == "--settings").unwrap() + 1;
    let mut merged: Value = serde_json::from_str(&args[at]).unwrap();
    merged
        .as_object_mut()
        .unwrap()
        .extend(settings.as_object().unwrap().clone());
    args[at] = merged.to_string();
}

struct Recorded {
    lines: Vec<String>,
    run: Run,
}

impl Recorded {
    fn events(&self) -> Vec<Value> {
        self.lines
            .iter()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn init(&self) -> Value {
        self.events()
            .into_iter()
            .find(|event| event["type"] == "system" && event["subtype"] == "init")
            .expect("an init event")
    }

    /// Checks the version, the exit status and standard error.
    fn expect(&self, exit_code: i32, stderr_empty: bool) {
        let stderr = String::from_utf8_lossy(&self.run.stderr);
        for event in self.events() {
            if event["subtype"] == "init" {
                assert_eq!(event["claude_code_version"], VERSION, "update VERSION?");
            }
        }
        assert_eq!(self.run.exit_code, Some(exit_code), "stderr: {stderr}");
        assert_eq!(stderr.is_empty(), stderr_empty, "stderr: {stderr}");
    }
}

fn record(mut claude: Command, prompt: &str) -> Recorded {
    let mut child = claude.spawn().expect("`claude` is on the PATH");
    let mut lines = Vec::new();
    let run = drive(&mut child, prompt, |line| lines.push(text(line))).unwrap();
    Recorded { lines, run }
}

fn text(line: &[u8]) -> String {
    let line = String::from_utf8(line.to_vec()).unwrap();
    assert!(line.ends_with('\n'), "a whole line: {line}");
    serde_json::from_str::<Value>(&line).expect("a JSON line");
    line
}

fn canonical(path: &Path) -> String {
    path.canonicalize().unwrap().display().to_string()
}

/// A scrub shared by the runs of one fixture, so that an id or a path gets
/// the same stand-in in each of them.
#[derive(Default)]
struct Scrub {
    /// Literal text and its stand-in, longest first when applied.
    literals: Vec<(String, String)>,
    /// Tool-use ids in order of appearance: the n-th becomes
    /// `toolu_fixture-n`.
    tool_ids: Vec<String>,
}

impl Scrub {
    /// Maps `path`, as given and as resolved, to `stand_in`.
    fn path(&mut self, path: &Path, stand_in: &str) -> &mut Self {
        self.literal(&path.display().to_string(), stand_in);
        self.literal(&canonical(path), stand_in)
    }

    fn literal(&mut self, text: &str, stand_in: &str) -> &mut Self {
        self.literals.push((text.into(), stand_in.into()));
        self
    }

    /// The inbox socket of a run's `init`, as the `n`-th stand-in.
    fn socket(&mut self, recorded: &Recorded, n: usize) -> &mut Self {
        let init = recorded.init();
        let socket = init["messaging_socket_path"].as_str().expect("an inbox");
        self.literal(socket, SOCKETS[n])
    }

    fn lines(&mut self, lines: &[String]) -> String {
        lines.iter().map(|line| self.line(line)).collect()
    }

    fn line(&mut self, line: &str) -> String {
        let mut line = line.to_owned();
        for key in USER_SETUP {
            line = rewrite_key(&line, key, None);
        }
        let quoted = |text: &str| json!(text).to_string();
        for (key, stand_in) in [
            ("timestamp", TIMESTAMP),
            ("signature", "redacted"),
            ("task_id", "task_fixture"),
            ("request_id", "req_fixture"),
            ("requestId", "req_fixture"),
        ] {
            line = rewrite_key(&line, key, Some(&quoted(stand_in)));
        }
        let mut literals = self.literals.clone();
        literals.sort_by_key(|(text, _)| std::cmp::Reverse(text.len()));
        for (text, stand_in) in literals {
            line = line.replace(&text, &stand_in);
        }
        line = rewrite_tokens(&line, "msg_", |_| "msg_fixture".into());
        line = rewrite_tokens(&line, "req_", |_| "req_fixture".into());
        line = rewrite_tokens(&line, "toolu_", |id| {
            let n = match self.tool_ids.iter().position(|seen| seen == id) {
                Some(at) => at + 1,
                None => {
                    self.tool_ids.push(id.into());
                    self.tool_ids.len()
                }
            };
            format!("toolu_fixture-{n}")
        });
        rewrite_uuids(&line)
    }
}

/// Replaces, or with `None` removes, the value of every `"key":` member in a
/// line of compact JSON, keeping the rest of the line as the CLI wrote it.
fn rewrite_key(line: &str, key: &str, value: Option<&str>) -> String {
    let member = format!("\"{key}\":");
    let mut out = String::new();
    let mut rest = line;
    while let Some(found) = rest.find(&member) {
        let before = &rest[..found];
        let start = found + member.len();
        if !(before.ends_with('{') || before.ends_with(',')) {
            // Not a member: text inside a string.
            out.push_str(&rest[..start]);
            rest = &rest[start..];
            continue;
        }
        let mut values =
            serde_json::Deserializer::from_str(&rest[start..]).into_iter::<serde::de::IgnoredAny>();
        values.next().unwrap().unwrap();
        let end = start + values.byte_offset();
        if let Some(value) = value {
            out.push_str(&rest[..start]);
            out.push_str(value);
        } else if let Some(before) = before.strip_suffix(',') {
            out.push_str(before);
        } else {
            out.push_str(before);
            rest = rest[end..].strip_prefix(',').unwrap_or(&rest[end..]);
            continue;
        }
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

/// Replaces each `prefix` followed by ten or more ASCII letters and digits,
/// at the start of a word, with `stand_in` of the whole token.
fn rewrite_tokens(line: &str, prefix: &str, mut stand_in: impl FnMut(&str) -> String) -> String {
    let mut out = String::new();
    let mut rest = line;
    while let Some(found) = rest.find(prefix) {
        let starts_word = !rest[..found]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_');
        let tail = &rest[found + prefix.len()..];
        let len = tail
            .find(|c: char| !c.is_ascii_alphanumeric())
            .unwrap_or(tail.len());
        out.push_str(&rest[..found]);
        if starts_word && len >= 10 {
            out.push_str(&stand_in(&rest[found..found + prefix.len() + len]));
        } else {
            out.push_str(&rest[found..found + prefix.len() + len]);
        }
        rest = &tail[len..];
    }
    out.push_str(rest);
    out
}

fn rewrite_uuids(line: &str) -> String {
    let bytes = line.as_bytes();
    let is_uuid = |at: usize| {
        bytes.get(at..at + 36).is_some_and(|candidate| {
            candidate.iter().enumerate().all(|(i, &b)| match i {
                8 | 13 | 18 | 23 => b == b'-',
                _ => b.is_ascii_hexdigit(),
            })
        })
    };
    let mut out = String::new();
    let mut at = 0;
    let mut copied = 0;
    while at < bytes.len() {
        if is_uuid(at) {
            out.push_str(&line[copied..at]);
            out.push_str(UUID);
            at += 36;
            copied = at;
        } else {
            at += 1;
        }
    }
    out.push_str(&line[copied..]);
    out
}

/// Fails on anything local left in a scrubbed recording, checks that each
/// tool result answers a call, and writes the fixture.
fn write(name: &str, scrubbed: &str) {
    let mut local = vec![
        "/Users/".to_owned(),
        "/home/".to_owned(),
        "/private/".to_owned(),
        "/var/folders".to_owned(),
    ];
    for var in ["HOME", "TMPDIR"] {
        local.extend(std::env::var(var).ok().filter(|value| value.len() > 1));
    }
    local.extend(std::env::var("USER").ok().filter(|user| user.len() >= 4));
    let email = Command::new("git")
        .args(["config", "--get", "user.email"])
        .output()
        .unwrap()
        .stdout;
    local.extend(
        Some(String::from_utf8(email).unwrap().trim().to_owned()).filter(|e| !e.is_empty()),
    );
    for text in &local {
        assert!(
            !scrubbed.contains(text.as_str()),
            "{name} still holds {text:?}"
        );
    }
    let mut sockets = scrubbed.to_owned();
    for socket in SOCKETS {
        sockets = sockets.replace(socket, "");
    }
    assert!(!sockets.contains("cc-socks"), "{name} holds a socket path");

    let events: Vec<Value> = scrubbed
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let blocks = |kind: &str| -> Vec<Value> {
        events
            .iter()
            .filter(|event| event["type"] == kind)
            .filter_map(|event| event["message"]["content"].as_array())
            .flatten()
            .cloned()
            .collect()
    };
    let calls: Vec<Value> = blocks("assistant")
        .into_iter()
        .filter(|block| block["type"] == "tool_use")
        .map(|block| block["id"].clone())
        .collect();
    for result in blocks("user") {
        if let Some(id) = result.get("tool_use_id") {
            assert!(calls.contains(id), "{name}: {id} answers no call");
        }
    }
    std::fs::write(fixture(name), scrubbed).unwrap();
}

/// Scrubs a single run and writes it.
fn scrub_and_write(name: &str, workdir: &Path, recorded: &Recorded) {
    let mut scrub = Scrub::default();
    scrub.path(workdir, "/work").socket(recorded, 0);
    write(name, &scrub.lines(&recorded.lines));
}

const RESULT: &str = ".owlshift/result.json";

/// A tiny read-only run, offered every tool: the `--disallowedTools` flag
/// and its values are left out, so that the contract test sees the tools the
/// adapter denies on offer.
#[test]
#[ignore = "records a fixture: set OWLSHIFT_RECORD_CLAUDE=1"]
fn success() {
    recording_allowed();
    let dir = tempfile::tempdir().unwrap();
    let request = request(dir.path(), PermissionLevel::ReadOnly, true, RESULT);
    let claude = adapter(&request, |args| {
        let denied = values(args, "--disallowedTools").len();
        let at = args
            .iter()
            .position(|arg| arg == "--disallowedTools")
            .unwrap();
        args.drain(at..=at + denied);
    });
    let recorded = record(claude, "Reply with the single word pong.");
    recorded.expect(0, true);
    assert_eq!(
        recorded.run.outcome,
        Outcome::Completed {
            text: "pong".into()
        }
    );
    scrub_and_write("success.jsonl", dir.path(), &recorded);
}

/// The read-only argv: the result file is written, a second file is refused.
#[test]
#[ignore = "records a fixture: set OWLSHIFT_RECORD_CLAUDE=1"]
fn read_only_denial() {
    recording_allowed();
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join(".owlshift")).unwrap();
    let request = request(dir.path(), PermissionLevel::ReadOnly, false, RESULT);
    let recorded = record(
        adapter(&request, |_| {}),
        "Use the Write tool to create .owlshift/result.json containing exactly {\"ok\": true}. \
         Then use the Write tool to create other.txt containing hello. \
         If a write is refused, do not try another way. \
         Then reply with the single word done.",
    );
    recorded.expect(0, true);
    assert_eq!(recorded.run.permission_denials, ["Write"]);
    let result = recorded.events().pop().unwrap();
    let denied = &result["permission_denials"][0]["tool_input"]["file_path"];
    assert!(denied.as_str().unwrap().ends_with("/other.txt"), "{denied}");
    let written = std::fs::read_to_string(dir.path().join(RESULT)).unwrap();
    let written: Value = serde_json::from_str(&written).unwrap();
    assert_eq!(written, json!({"ok": true}));
    assert!(!dir.path().join("other.txt").exists());
    scrub_and_write("read_only_denial.jsonl", dir.path(), &recorded);
}

/// The read-only argv with a model that does not exist: exit 1, and a line
/// on standard error.
#[test]
#[ignore = "records a fixture: set OWLSHIFT_RECORD_CLAUDE=1"]
fn unknown_model() {
    recording_allowed();
    let dir = tempfile::tempdir().unwrap();
    let mut request = request(dir.path(), PermissionLevel::ReadOnly, false, RESULT);
    request.model = Some("owlshift-no-such-model".into());
    let recorded = record(adapter(&request, |_| {}), "Reply with pong.");
    recorded.expect(1, false);
    scrub_and_write("unknown_model.jsonl", dir.path(), &recorded);
}

/// The write-in-worktree argv with no network and a JSON Schema. For the
/// recording only, its `--settings` also holds an allow rule for each denied
/// tool and a hook refusing every call but `ToolSearch` and
/// `StructuredOutput`; the model is asked to load the deferred ones.
#[test]
#[ignore = "records a fixture: set OWLSHIFT_RECORD_CLAUDE=1"]
fn beyond_run_tools_denied() {
    recording_allowed();
    let dir = tempfile::tempdir().unwrap();
    let mut request = request(dir.path(), PermissionLevel::WriteWorktree, false, RESULT);
    let loaded = json!({"type": "array", "items": {"type": "string"}});
    let schema =
        json!({"type": "object", "properties": {"loaded": loaded}, "required": ["loaded"]});
    request.json_schema = Some(schema.to_string());
    let mut deferred = Vec::new();
    let claude = adapter(&request, |args| {
        let denied = values(args, "--disallowedTools").to_vec();
        deferred = denied
            .iter()
            .filter(|tool| !["Agent", "WebFetch", "WebSearch"].contains(&tool.as_str()))
            .cloned()
            .collect();
        let gate = "grep -Eq '\"tool_name\": ?\"(ToolSearch|StructuredOutput)\"' \
                    || { echo 'refused by the recording' >&2; exit 2; }";
        let hook = json!([{"hooks": [{"type": "command", "command": gate}]}]);
        let settings = json!({"permissions": {"allow": denied}, "hooks": {"PreToolUse": hook}});
        set_settings(args, &settings);
    });
    let prompt = format!(
        "Call the ToolSearch tool once, with the query select:{}, to load those tools. \
         Call no other tool. Then answer with the names of the tools it loaded, in `loaded`.",
        deferred.join(",")
    );
    let recorded = record(claude, &prompt);
    recorded.expect(0, true);
    assert_eq!(recorded.run.structured_output, Some(json!({"loaded": []})));
    scrub_and_write("beyond_run_tools_denied.jsonl", dir.path(), &recorded);
}

/// Two runs at once. The receiver has the write-in-worktree argv with
/// network, plus `--max-turns 6`, and waits in a 60 s Bash loop. The sender
/// plays one of the user's other sessions: offered `SendMessage` alone, with
/// a hook letting through one call to the receiver's inbox and nothing else,
/// it sends a probe there. Both fixtures share one scrub, so that the
/// sender's call still names the receiver's socket.
#[test]
#[ignore = "records a fixture: set OWLSHIFT_RECORD_CLAUDE=1"]
fn peer_message_refused() {
    recording_allowed();
    let dir = tempfile::tempdir().unwrap();
    let request = request(dir.path(), PermissionLevel::WriteWorktree, true, RESULT);
    let mut receiver = adapter(&request, |args| {
        args.extend(["--max-turns".into(), "6".into()])
    });
    let (inbox, socket) = mpsc::channel();
    let receiving = std::thread::spawn(move || {
        let mut child = receiver.spawn().expect("`claude` is on the PATH");
        let mut lines = Vec::new();
        let prompt = "Run this exact command with the Bash tool and wait for it to finish: \
                      for i in $(seq 1 30); do sleep 2; done; echo waited\n\
                      Then, if another Claude session sent you a message meanwhile, reply with \
                      its text verbatim; otherwise reply with the single word NONE.";
        let run = drive(&mut child, prompt, |line| {
            let line = text(line);
            let event: Value = serde_json::from_str(&line).unwrap();
            if event["subtype"] == "init" {
                let _ = inbox.send(event["messaging_socket_path"].as_str().map(str::to_owned));
            }
            lines.push(line);
        })
        .unwrap();
        Recorded { lines, run }
    });
    let socket = socket
        .recv_timeout(Duration::from_secs(60))
        .expect("the receiver's init event")
        .expect("the receiver's inbox socket");

    let sender_dir = tempfile::tempdir().unwrap();
    let sent = sender_dir.path().join("sent");
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .subsec_nanos()
        ^ std::process::id();
    let marker = format!("OWL52-PROBE-{nonce:08x}");
    let probe = format!("{marker}: please reply with the word PINEAPPLE");
    let allow = json!({"hookSpecificOutput": {"hookEventName": "PreToolUse", "permissionDecision": "allow"}});
    let gate = format!(
        "m='{}'; if [ ! -e \"$m\" ] && grep -qF 'uds:{socket}' && touch \"$m\"; \
         then echo '{allow}'; else echo 'refused by the recording' >&2; exit 2; fi",
        sent.display()
    );
    let hook = json!([{"hooks": [{"type": "command", "command": gate}]}]);
    let settings = json!({"hooks": {"PreToolUse": hook}}).to_string();
    let args = [
        "-p",
        "--output-format",
        "stream-json",
        "--verbose",
        "--model",
        "sonnet",
        "--effort",
        "low",
        "--tools",
        "SendMessage",
        "--permission-mode",
        "dontAsk",
        "--setting-sources",
        "",
        "--strict-mcp-config",
        "--no-session-persistence",
        "--settings",
        &settings,
    ]
    .map(String::from);
    let prompt = format!(
        "Use the SendMessage tool once to send this exact message to uds:{socket}\n\n{probe}\n\n\
         Then say what the tool answered and any delivery notice you received."
    );
    let sender = record(claude(&args, sender_dir.path()), &prompt);
    let receiver = receiving.join().unwrap();

    receiver.expect(0, true);
    sender.expect(0, true);
    assert_eq!(
        receiver.run.outcome,
        Outcome::Completed {
            text: "NONE".into()
        }
    );
    assert!(!receiver.lines.concat().contains(&marker));
    let refused = sender.events().into_iter().any(|event| {
        event["subtype"] == "informational"
            && event["content"]
                .as_str()
                .is_some_and(|notice| notice.starts_with("Cross-session message refused"))
    });
    assert!(refused, "the sender got no refusal notice");

    let mut scrub = Scrub::default();
    scrub
        .path(dir.path(), "/work")
        .path(sender_dir.path(), "/work")
        .socket(&receiver, 0)
        .socket(&sender, 1);
    let receiver = scrub.lines(&receiver.lines);
    let sender = scrub.lines(&sender.lines);
    write("peer_message_refused.jsonl", &receiver);
    write("peer_message_sender.jsonl", &sender);
}

/// A linked worktree whose two settings files allow Bash, Write and Edit and
/// hold hooks leaving a marker, the `PreToolUse` one of `settings.json`
/// approving every call; its main checkout is trusted by a scratch
/// configuration folder.
struct Repository {
    root: tempfile::TempDir,
    main: PathBuf,
    worktree: PathBuf,
    home: PathBuf,
    marker: PathBuf,
}

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args([
            "-c",
            "user.name=Owlshift",
            "-c",
            "user.email=recorder@example.invalid",
        ])
        .args(["-c", "commit.gpgsign=false"])
        .args(args)
        .current_dir(dir)
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

fn repository() -> Repository {
    let root = tempfile::tempdir().unwrap();
    let main = root.path().join("main");
    let worktree = root.path().join("work");
    let home = root.path().join("home");
    let marker = root.path().join("hook-ran");
    std::fs::create_dir_all(&main).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    git(&main, &["init", "-q"]);
    std::fs::write(main.join("README"), "scratch\n").unwrap();
    git(&main, &["add", "README"]);
    git(&main, &["commit", "-qm", "scratch"]);
    git(&main, &["worktree", "add", "-q", "-b", "run", "../work"]);

    let touch = format!("touch '{}'", marker.display());
    let allow = json!({"hookSpecificOutput": {"hookEventName": "PreToolUse", "permissionDecision": "allow"}});
    let approve = format!("{touch}; echo '{allow}'");
    let hook = |command: &str| json!([{"hooks": [{"type": "command", "command": command}]}]);
    let settings = |pre_tool_use: &str| {
        json!({
            "permissions": {"allow": ["Bash", "Write", "Edit"]},
            "hooks": {
                "SessionStart": hook(&touch),
                "UserPromptSubmit": hook(&touch),
                "PreToolUse": hook(pre_tool_use),
            },
        })
        .to_string()
    };
    let claude_dir = worktree.join(".claude");
    std::fs::create_dir_all(&claude_dir).unwrap();
    std::fs::write(claude_dir.join("settings.json"), settings(&approve)).unwrap();
    std::fs::write(claude_dir.join("settings.local.json"), settings(&touch)).unwrap();
    std::fs::create_dir_all(worktree.join(".owlshift/run")).unwrap();

    let trusted = json!({
        "hasCompletedOnboarding": true,
        "projects": {canonical(&main): {"hasTrustDialogAccepted": true}},
    });
    std::fs::write(home.join(".claude.json"), trusted.to_string()).unwrap();
    Repository {
        root,
        main,
        worktree,
        home,
        marker,
    }
}

/// The read-only argv, `--setting-sources` set to `sources`, in the
/// repository's worktree, against a stand-in model.
fn stand_in_run(repository: &Repository, sources: &str) -> Recorded {
    let request = request(
        &repository.worktree,
        PermissionLevel::ReadOnly,
        false,
        ".owlshift/run/result.json",
    );
    let mut claude = adapter(&request, |args| {
        let at = args
            .iter()
            .position(|arg| arg == "--setting-sources")
            .unwrap();
        args[at + 1] = sources.into();
    });
    claude
        .env("HOME", &repository.home)
        .env("CLAUDE_CONFIG_DIR", &repository.home)
        .env(
            "ANTHROPIC_BASE_URL",
            stand_in(canonical(&repository.worktree)),
        )
        .env("ANTHROPIC_API_KEY", "sk-ant-owlshift-stand-in");
    record(claude, "Follow the script.")
}

/// The read-only argv in a trusted worktree whose settings would widen the
/// role, against a local stand-in for the model playing three calls: Write
/// to the result file, Bash, Write to another file. A control run of the
/// same repository under `project,local` must run the hooks, or the
/// recording would prove nothing.
#[test]
#[ignore = "records a fixture: set OWLSHIFT_RECORD_CLAUDE=1"]
fn repo_settings_ignored() {
    recording_allowed();
    let control = repository();
    let widened = stand_in_run(&control, "project,local");
    assert!(control.marker.exists(), "the control run ran no hook");
    assert!(
        widened.run.permission_denials.is_empty(),
        "{:?}",
        widened.run.permission_denials
    );

    let repository = repository();
    let recorded = stand_in_run(&repository, "");
    recorded.expect(0, true);
    assert!(!repository.marker.exists(), "a repository hook ran");
    assert_eq!(recorded.run.permission_denials, ["Bash", "Write"]);
    let mut scrub = Scrub::default();
    scrub
        .path(&repository.worktree, "/work")
        .path(&repository.main, "/main")
        .path(&repository.home, "/config")
        .path(repository.root.path(), "/scratch")
        .socket(&recorded, 0);
    write("repo_settings_ignored.jsonl", &scrub.lines(&recorded.lines));
}

/// Serves a stand-in for the Messages API on a local port and returns its
/// URL. Every request with tools gets the next call of the script, told by
/// the tool results it already carries; one without tools, a side request
/// of the CLI, gets text.
fn stand_in(worktree: String) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let worktree = worktree.clone();
            std::thread::spawn(move || answer(stream, &worktree));
        }
    });
    url
}

fn answer(mut stream: TcpStream, worktree: &str) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    let mut length = 0;
    loop {
        let mut header = String::new();
        reader.read_line(&mut header)?;
        if header.trim().is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;

    let path = request_line.split_whitespace().nth(1).unwrap_or_default();
    let (status, kind, payload) = if !path.starts_with("/v1/messages") {
        let error = json!({"type": "error", "error": {"type": "not_found_error", "message": path}});
        ("404 Not Found", "application/json", error.to_string())
    } else if path.contains("count_tokens") {
        (
            "200 OK",
            "application/json",
            json!({"input_tokens": 10}).to_string(),
        )
    } else {
        let request: Value = serde_json::from_slice(&body).unwrap_or_default();
        let message = reply(&request, worktree);
        if request["stream"] == true {
            ("200 OK", "text/event-stream", events(&message))
        } else {
            ("200 OK", "application/json", message.to_string())
        }
    };
    write!(
        stream,
        "HTTP/1.1 {status}\r\ncontent-type: {kind}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{payload}",
        payload.len()
    )
}

fn reply(request: &Value, worktree: &str) -> Value {
    let results = request["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|message| message["content"].as_array())
        .flatten()
        .filter(|block| block["type"] == "tool_result")
        .count();
    let tool_use = |n: u32, name: &str, input: Value| {
        let id = format!("toolu_standin{n:010}");
        json!({"type": "tool_use", "id": id, "name": name, "input": input})
    };
    let scripted = request["tools"]
        .as_array()
        .is_some_and(|tools| !tools.is_empty());
    let block = match (scripted, results) {
        (true, 0) => tool_use(
            1,
            "Write",
            json!({"file_path": format!("{worktree}/.owlshift/run/result.json"), "content": "{}\n"}),
        ),
        (true, 1) => tool_use(
            2,
            "Bash",
            json!({"command": "touch widened-by-bash.txt && echo OWL53-BASH-RAN", "description": "probe"}),
        ),
        (true, 2) => tool_use(
            3,
            "Write",
            json!({"file_path": format!("{worktree}/widened-by-write.txt"), "content": "probe\n"}),
        ),
        _ => json!({"type": "text", "text": "DONE"}),
    };
    let stop = if block["type"] == "tool_use" {
        "tool_use"
    } else {
        "end_turn"
    };
    json!({
        "id": format!("msg_standin{results:010}"),
        "type": "message",
        "role": "assistant",
        "model": request["model"],
        "content": [block],
        "stop_reason": stop,
        "stop_sequence": null,
        "usage": {
            "input_tokens": 10,
            "output_tokens": 5,
            "cache_creation_input_tokens": 0,
            "cache_read_input_tokens": 0,
        },
    })
}

/// A message as the server-sent events of a streamed answer.
fn events(message: &Value) -> String {
    let mut start = message.clone();
    start["content"] = json!([]);
    start["stop_reason"] = Value::Null;
    let block = &message["content"][0];
    let (empty, delta) = if block["type"] == "tool_use" {
        let mut empty = block.clone();
        empty["input"] = json!({});
        let partial = block["input"].to_string();
        (
            empty,
            json!({"type": "input_json_delta", "partial_json": partial}),
        )
    } else {
        let text = &block["text"];
        (
            json!({"type": "text", "text": ""}),
            json!({"type": "text_delta", "text": text}),
        )
    };
    let stop = json!({"stop_reason": message["stop_reason"], "stop_sequence": null});
    [
        json!({"type": "message_start", "message": start}),
        json!({"type": "content_block_start", "index": 0, "content_block": empty}),
        json!({"type": "content_block_delta", "index": 0, "delta": delta}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "message_delta", "delta": stop, "usage": {"output_tokens": 5}}),
        json!({"type": "message_stop"}),
    ]
    .iter()
    .map(|event| {
        format!(
            "event: {}\ndata: {event}\n\n",
            event["type"].as_str().unwrap()
        )
    })
    .collect()
}

/// The scrub on lines shaped like the CLI's: the same id keeps its stand-in
/// across lines and inside other strings, object keys included.
#[test]
fn scrub_rewrites_ids_paths_and_user_setup() {
    let mut scrub = Scrub::default();
    scrub.path(Path::new("/tmp"), "/work");
    let session = "0f8e2c1a-9b3d-4e5f-8a7b-6c5d4e3f2a1b";
    let lines = [
        format!(
            r#"{{"type":"system","subtype":"init","cwd":"/tmp/x","skills":[{{"path":"/tmp/s"}}],"session_id":"{session}","memory_paths":["/tmp/m"]}}"#
        ) + "\n",
        r#"{"type":"assistant","message":{"id":"msg_01ABCDEFGHIJKL","content":[{"type":"tool_use","id":"toolu_01AAAAAAAAAAAA","name":"Bash"},{"type":"thinking","signature":"abc=="}]},"wire_tool_inputs":{"toolu_01AAAAAAAAAAAA":{}},"timestamp":"2026-10-01T08:00:00.000Z"}"#.to_owned() + "\n",
        r#"{"type":"tool_progress","tool_use_id":"toolu_01BBBBBBBBBBBB-heartbeat-0","parent_tool_use_id":"toolu_01AAAAAAAAAAAA","text":"see \"timestamp\": toolu_01AAAAAAAAAAAA and msg_id"}"#.to_owned() + "\n",
    ];
    let scrubbed = scrub.lines(&lines);
    assert_eq!(
        scrubbed,
        [
            format!(
                r#"{{"type":"system","subtype":"init","cwd":"/work/x","session_id":"{UUID}"}}"#
            ),
            format!(
                r#"{{"type":"assistant","message":{{"id":"msg_fixture","content":[{{"type":"tool_use","id":"toolu_fixture-1","name":"Bash"}},{{"type":"thinking","signature":"redacted"}}]}},"wire_tool_inputs":{{"toolu_fixture-1":{{}}}},"timestamp":"{TIMESTAMP}"}}"#
            ),
            r#"{"type":"tool_progress","tool_use_id":"toolu_fixture-2-heartbeat-0","parent_tool_use_id":"toolu_fixture-1","text":"see \"timestamp\": toolu_fixture-1 and msg_id"}"#.to_owned(),
        ]
        .map(|line| line + "\n")
        .concat()
    );
}
