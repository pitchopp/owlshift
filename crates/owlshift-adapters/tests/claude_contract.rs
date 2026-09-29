//! Contract tests of the Claude Code harness: recorded `claude -p` output
//! replayed through the adapter, with no model and no login.
//!
//! The fixtures in `fixtures/claude/` were recorded on 2026-09-28 and
//! 2026-09-29 with Claude Code 2.1.283, but for the sender of OWL-52's probe
//! (see `docs/design/build-plan.md`, OWL-14, OWL-46 and OWL-52 results),
//! then scrubbed: session, message and
//! tool-use ids, timestamps, paths and thinking signatures replaced, the
//! user's skills, plugins and agents and the local paths of the `init` event
//! removed or, for the inbox socket, replaced. Each test states the exit
//! status and standard error the run had, since both decide the outcome.
//! `usage_limit.jsonl` alone is constructed, not recorded: no run has hit a
//! limit on purpose (C7's open item). It follows the shapes C7 logged.
//! `repo_settings_ignored.jsonl` was recorded with the real CLI and a local
//! stand-in for the model, which played scripted tool calls (OWL-53).

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use jiff::Timestamp;
use owlshift_adapters::harness::claude::{
    Billing, EXIT_GRACE, Failure, Outcome, RateLimitStatus, Request, Run, Transcript, command,
    drive,
};
use owlshift_adapters::harness::tested::is_tested;
use owlshift_contracts::Harness;
use owlshift_contracts::brief::{PermissionLevel, Permissions};
use owlshift_contracts::ids::RelativePath;
use serde_json::{Value, json};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/claude")
        .join(name)
}

fn replay(name: &str, exit_code: i32) -> Run {
    let recorded = std::fs::read(fixture(name)).unwrap();
    let mut transcript = Transcript::default();
    for line in recorded.split_inclusive(|&byte| byte == b'\n') {
        transcript.feed(line);
    }
    transcript.finish(Some(exit_code), Vec::new())
}

/// A tiny read-only run on the subscription: exit 0, empty stderr.
#[test]
fn success() {
    let run = replay("success.jsonl", 0);
    assert_eq!(
        run.outcome,
        Outcome::Completed {
            text: "pong".into()
        }
    );
    assert_eq!(run.billing, Billing::Subscription);
    assert_eq!(run.harness_version.as_deref(), Some("2.1.284"));
    assert_eq!(run.model.as_deref(), Some("claude-haiku-4-5-20251001"));
    assert_eq!(run.malformed_lines, 0);
    assert!(run.permission_denials.is_empty());

    let usage = run.usage.unwrap();
    assert_eq!(
        (usage.input_tokens, usage.output_tokens),
        (10, 51),
        "{usage:?}"
    );
    assert_eq!(usage.cache_read_input_tokens, 13689);
    assert_eq!(usage.cache_creation_input_tokens, 7179);
    assert_eq!(usage.cost_usd, Some(0.0159919));
    assert_eq!((usage.duration_ms, usage.num_turns), (Some(1052), Some(1)));
    let haiku = &usage.models["claude-haiku-4-5-20251001"];
    assert_eq!((haiku.input_tokens, haiku.output_tokens), (10, 51));

    let limit = run.rate_limit.unwrap();
    assert_eq!(limit.status, RateLimitStatus::AllowedWarning);
    assert_eq!(limit.window.as_deref(), Some("seven_day"));
    assert_eq!(
        limit.resets_at,
        Some(Timestamp::from_second(1790748000).unwrap())
    );
    assert_eq!(limit.five_hour_utilization, Some(0.19));
    assert_eq!(limit.seven_day_utilization, Some(0.8));
}

/// A read-only run told to write its result file and another file: the
/// second write is refused, the run still completes. Exit 0, empty stderr.
#[test]
fn read_only_denial() {
    let run = replay("read_only_denial.jsonl", 0);
    assert!(
        matches!(run.outcome, Outcome::Completed { .. }),
        "{:?}",
        run.outcome
    );
    assert_eq!(run.permission_denials, ["Write"]);
}

/// An unknown model: exit 1, `is_error: true` although `subtype` says
/// success, and a line on stderr.
#[test]
fn unknown_model() {
    let run = replay("unknown_model.jsonl", 1);
    match run.outcome {
        Outcome::Failed(Failure::Error {
            exit_code,
            api_error_status,
            terminal_reason,
            kind,
            message,
        }) => {
            assert_eq!(exit_code, Some(1));
            assert_eq!(api_error_status, Some(404));
            assert_eq!(terminal_reason.as_deref(), Some("api_error"));
            assert_eq!(kind.as_deref(), Some("model_not_found"));
            assert!(message.unwrap().contains("owlshift-no-such-model"));
        }
        other => panic!("expected an error, got {other:?}"),
    }
}

/// Constructed: a rejected rate-limit report, then the synthetic limit
/// message. The exit status is assumed to be 1, as for other API errors.
#[test]
fn usage_limit() {
    let run = replay("usage_limit.jsonl", 1);
    assert_eq!(
        run.outcome,
        Outcome::UsageLimit {
            resets_at: Some(Timestamp::from_second(1790613600).unwrap()),
            window: Some("five_hour".into()),
        }
    );
}

/// The events of a fixture, read as raw JSON: `Run` carries neither the
/// `init` tool list nor the tool calls, so what is read here pins the
/// recording, not the adapter's parsing.
fn recorded_events(name: &str) -> Vec<Value> {
    std::fs::read_to_string(fixture(name))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn offered_tools(name: &str) -> Vec<String> {
    let events = recorded_events(name);
    let init = events
        .iter()
        .find(|event| event["type"] == "system" && event["subtype"] == "init")
        .unwrap_or_else(|| panic!("{name} has no init event"));
    serde_json::from_value(init["tools"].clone()).unwrap()
}

/// The tools whose calls can act outside the run: on the user's claude.ai
/// account or design projects, their devices, their other sessions, a
/// schedule outliving the run, another worktree (OWL-42, OWL-46). `init`
/// lists the `Agent` tool as `Task`.
const BEYOND_RUN_TOOLS: [&str; 12] = [
    "RemoteTrigger",
    "PushNotification",
    "DesignSync",
    "Workflow",
    "CronCreate",
    "CronDelete",
    "CronList",
    "SendMessage",
    "ListAgents",
    "EnterWorktree",
    "ExitWorktree",
    "Agent",
];

fn sorted<'a>(names: impl IntoIterator<Item = &'a str>) -> Vec<&'a str> {
    let mut names: Vec<&str> = names.into_iter().collect();
    names.sort_unstable();
    names
}

/// The command line of every kind of launch: both permission levels, with
/// and without network, with a JSON Schema.
fn every_launch() -> Vec<(PermissionLevel, bool, Vec<String>)> {
    let mut launches = Vec::new();
    for level in [PermissionLevel::ReadOnly, PermissionLevel::WriteWorktree] {
        for network in [true, false] {
            let request = Request {
                workdir: PathBuf::from("/work"),
                model: None,
                effort: None,
                permissions: Permissions {
                    level,
                    network,
                    browser: false,
                },
                result_path: RelativePath::new(".owlshift/result.json").unwrap(),
                json_schema: Some(r#"{"type":"object"}"#.into()),
                max_budget_usd: None,
            };
            let args = command(Path::new("claude"), &request)
                .unwrap()
                .get_args()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect();
            launches.push((level, network, args));
        }
    }
    launches
}

/// Where `flag` stands in `args`.
fn positions(args: &[String], flag: &str) -> Vec<usize> {
    (0..args.len()).filter(|&at| args[at] == flag).collect()
}

/// The tool calls the model made in a recording.
fn tool_calls(events: &[Value]) -> Vec<&Value> {
    events
        .iter()
        .filter(|event| event["type"] == "assistant")
        .filter_map(|event| event["message"]["content"].as_array())
        .flatten()
        .filter(|block| block["type"] == "tool_use")
        .collect()
}

/// The tool results of a recording, in order.
fn tool_results(events: &[Value]) -> Vec<&Value> {
    events
        .iter()
        .filter(|event| event["type"] == "user")
        .filter_map(|event| event["message"]["content"].as_array())
        .flatten()
        .filter(|block| block["tool_use_id"].is_string())
        .collect()
}

/// Every launch removes the tools that reach beyond the run, in its one
/// `--disallowedTools` flag, with the web tools when the run has no network.
/// `beyond_run_tools_denied.jsonl` was recorded with that argv (write in
/// worktree, no network, a JSON Schema) plus, for the recording only, one
/// `--settings` carrying an allow rule for each of those tools and a hook
/// letting only `ToolSearch` and `StructuredOutput` through; the model was
/// asked to load every deferred one with `ToolSearch`. None was offered,
/// `ToolSearch` found none, and the model called nothing else.
/// `success.jsonl`, recorded without the flag, was offered all of them. Exit
/// 0, empty stderr.
#[test]
fn tools_reaching_beyond_the_run_are_denied_on_every_launch() {
    for (level, network, args) in every_launch() {
        let flags = positions(&args, "--disallowedTools");
        let case = format!("{level:?}, network {network}: {args:?}");
        // One flag: whether a second one would add to the first or replace
        // it was not checked.
        assert_eq!(flags.len(), 1, "{case}");
        let denied: Vec<&str> = args[flags[0] + 1..]
            .iter()
            .take_while(|arg| !arg.starts_with("--"))
            .map(String::as_str)
            .collect();
        let mut expected = BEYOND_RUN_TOOLS.to_vec();
        if !network {
            expected.extend(["WebFetch", "WebSearch"]);
        }
        assert_eq!(sorted(denied), sorted(expected), "{case}");
    }

    // The `init` event names the `Agent` tool `Task`.
    let in_init = |tool: &str| (if tool == "Agent" { "Task" } else { tool }).to_owned();
    let offered = offered_tools("success.jsonl");
    for tool in BEYOND_RUN_TOOLS {
        assert!(offered.contains(&in_init(tool)), "{tool}: {offered:?}");
    }
    let name = "beyond_run_tools_denied.jsonl";
    let offered = offered_tools(name);
    for tool in BEYOND_RUN_TOOLS {
        assert!(!offered.contains(&in_init(tool)), "{tool}: {offered:?}");
    }

    let events = recorded_events(name);
    let calls = tool_calls(&events);
    let called: Vec<&Value> = calls.iter().map(|block| &block["name"]).collect();
    assert_eq!(called, [&json!("ToolSearch"), &json!("StructuredOutput")]);
    let query = calls[0]["input"]["query"].as_str().unwrap();
    let searched = query.strip_prefix("select:").unwrap().split(',');
    let deferred = BEYOND_RUN_TOOLS.into_iter().filter(|tool| *tool != "Agent");
    assert_eq!(sorted(searched), sorted(deferred));
    let found = tool_results(&events);
    assert_eq!(
        found[0]["content"],
        json!("No matching deferred tools found")
    );

    let run = replay(name, 0);
    assert!(
        matches!(run.outcome, Outcome::Completed { .. }),
        "{:?}",
        run.outcome
    );
    assert!(run.permission_denials.is_empty());
    assert_eq!(run.structured_output, Some(json!({"loaded": []})));
}

/// Every launch refuses what the user's other sessions send to the run's
/// inbox, in one `--settings` flag. `peer_message_refused.jsonl` was recorded
/// with the write-in-worktree argv, network on, that flag included, plus
/// `--model haiku --effort low --max-turns 6` for the recording; the model
/// was asked to run a 60 s Bash loop, then to report any message another
/// session had sent. Meanwhile a second `claude -p`
/// (`peer_message_sender.jsonl`, Claude Code 2.1.284, offered `SendMessage`
/// alone, with a recording-only hook letting through one call to that inbox
/// and nothing else) sent a probe to the `messaging_socket_path` of the first
/// one's `init`. The sender was told the message was refused, and the probe
/// reached neither the receiver's stream nor its answer. Without the flag,
/// the same probe came back as the receiver's answer (build plan, OWL-52).
/// Exit 0, empty stderr, for both.
#[test]
fn messages_from_other_sessions_are_refused_on_every_launch() {
    for (level, network, args) in every_launch() {
        let flags = positions(&args, "--settings");
        let case = format!("{level:?}, network {network}: {args:?}");
        // One flag: whether a second one would merge with the first or
        // replace it was not checked.
        assert_eq!(flags.len(), 1, "{case}");
        let settings: Value = serde_json::from_str(&args[flags[0] + 1]).unwrap();
        assert_eq!(settings, json!({"crossSessionInbound": "refuse"}), "{case}");
    }

    let receiver = recorded_events("peer_message_refused.jsonl");
    let init = receiver
        .iter()
        .find(|event| event["type"] == "system" && event["subtype"] == "init")
        .unwrap();
    // The flag refuses messages; the inbox itself stays open.
    let inbox = init["messaging_socket_path"].as_str().unwrap();

    let sender = recorded_events("peer_message_sender.jsonl");
    let calls = tool_calls(&sender);
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert_eq!(calls[0]["name"], "SendMessage");
    assert_eq!(calls[0]["input"]["to"], json!(format!("uds:{inbox}")));
    let probe = calls[0]["input"]["message"].as_str().unwrap();
    let notices: Vec<&str> = sender
        .iter()
        .filter(|event| event["type"] == "system" && event["subtype"] == "informational")
        .filter_map(|event| event["content"].as_str())
        .collect();
    assert!(
        notices
            .iter()
            .any(|notice| notice.starts_with("Cross-session message refused")),
        "{notices:?}"
    );

    let marker = probe.split(':').next().unwrap();
    assert!(marker.starts_with("OWL52-PROBE-"), "{probe}");
    let recorded = std::fs::read_to_string(fixture("peer_message_refused.jsonl")).unwrap();
    assert!(!recorded.contains(marker));
    assert_eq!(
        replay("peer_message_refused.jsonl", 0).outcome,
        Outcome::Completed {
            text: "NONE".into()
        }
    );
}

/// Every launch loads no settings file: `--setting-sources` with an empty
/// value leaves out the user's settings and the worktree's
/// `.claude/settings.json` and `.claude/settings.local.json`, while
/// `--settings` and managed settings still apply. Under `project,local`, a
/// trusted worktree's allow rule, in either file, let a read-only role run
/// Bash and Write, and a repository hook answering `allow` did so even in an
/// untrusted one (build plan, OWL-53).
///
/// The command line is the guard. `repo_settings_ignored.jsonl` records what
/// the CLI did with it: the read-only argv, in a trusted linked worktree
/// whose two settings files allowed Bash, Write and Edit and held hooks, one
/// of them approving every call. A local stand-in for the model, reached
/// with a made-up API key (hence `apiKeySource`), played three calls: Write
/// to the result file, Bash, Write to another file. The first succeeded, the
/// other two were denied, and no hook ran, where the same repository under
/// `project,local` had all three succeed and its `SessionStart` hooks
/// reported in the stream. Exit 0, empty stderr.
#[test]
fn repository_settings_cannot_widen_a_role_on_every_launch() {
    for (level, network, args) in every_launch() {
        let flags = positions(&args, "--setting-sources");
        let case = format!("{level:?}, network {network}: {args:?}");
        assert_eq!(flags.len(), 1, "{case}");
        assert_eq!(args[flags[0] + 1], "", "{case}");
    }

    let name = "repo_settings_ignored.jsonl";
    let events = recorded_events(name);
    let hooks: Vec<&Value> = events
        .iter()
        .filter(|event| {
            event["type"] == "system"
                && event["subtype"]
                    .as_str()
                    .is_some_and(|subtype| subtype.starts_with("hook_"))
        })
        .collect();
    assert!(hooks.is_empty(), "{hooks:?}");

    let calls = tool_calls(&events);
    let called: Vec<&Value> = calls.iter().map(|block| &block["name"]).collect();
    assert_eq!(called, [&json!("Write"), &json!("Bash"), &json!("Write")]);
    let results = tool_results(&events);
    assert_eq!(results.len(), calls.len());
    for (call, result) in calls.iter().zip(&results) {
        assert_eq!(result["tool_use_id"], call["id"]);
    }
    assert_eq!(
        calls[0]["input"]["file_path"],
        json!("/work/.owlshift/run/result.json")
    );
    let created = results[0]["content"].as_str().unwrap();
    assert!(
        created.starts_with("File created successfully"),
        "{created}"
    );
    for (call, result) in calls[1..].iter().zip(&results[1..]) {
        let tool = call["name"].as_str().unwrap();
        let denied = format!("Permission to use {tool} has been denied");
        let content = result["content"].as_str().unwrap();
        assert!(content.starts_with(&denied), "{content}");
    }

    let run = replay(name, 0);
    assert!(
        matches!(run.outcome, Outcome::Completed { .. }),
        "{:?}",
        run.outcome
    );
    assert_eq!(run.permission_denials, ["Bash", "Write"]);
}

/// Every recorded fixture carries a version listed in `harness/tested.rs`,
/// so re-recording with a new release cannot leave the list `owlshift
/// doctor` reads behind. The constructed fixture proves nothing about a
/// release and is skipped, and so is the sender of OWL-52's probe: it plays
/// one of the user's other sessions, whatever their version, not a run the
/// adapter launches.
#[test]
fn recorded_versions_are_listed_as_tested() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/claude");
    let mut recorded = 0;
    for entry in std::fs::read_dir(dir).unwrap() {
        let name = entry.unwrap().file_name().into_string().unwrap();
        if name == "usage_limit.jsonl" || name == "peer_message_sender.jsonl" {
            continue;
        }
        let version = replay(&name, 0)
            .harness_version
            .unwrap_or_else(|| panic!("{name} records no Claude Code version"));
        assert!(
            is_tested(Harness::Claude, &version),
            "{name} was recorded with Claude Code {version}: once the contract tests \
             pass on it, add it to crates/owlshift-adapters/src/harness/tested.rs"
        );
        recorded += 1;
    }
    assert!(recorded > 0, "no recorded fixture found");
}

/// Runs one of the helpers below in a fresh copy of this test binary, as the
/// harness child.
fn helper(name: &str, exit_code: i32) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            name,
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("OWLSHIFT_HELPER_EXIT", exit_code.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

/// The helpers act only when run alone by [`helper`], never in a plain
/// `cargo test -- --ignored`.
fn helper_requested() -> bool {
    std::env::args().any(|arg| arg == "--exact")
}

/// Plays `claude`: reads the whole prompt, reports its size on stderr,
/// prints the success fixture and exits with the requested status.
#[test]
#[ignore = "helper, run by the tests below"]
fn helper_replay() {
    if helper_requested() {
        let mut prompt = Vec::new();
        std::io::stdin().read_to_end(&mut prompt).unwrap();
        eprint!("prompt bytes: {}", prompt.len());
        print!(
            "{}",
            std::fs::read_to_string(fixture("success.jsonl")).unwrap()
        );
        exit_now();
    }
}

/// Starts a process that inherits stdout and outlives this one, then exits.
#[test]
#[ignore = "helper, run by the tests below"]
#[expect(
    clippy::zombie_processes,
    reason = "the grandchild must outlive this process"
)]
fn helper_leave_a_pipe_holder() {
    if helper_requested() {
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "helper_sleep", "--ignored", "--nocapture"])
            .spawn()
            .unwrap();
        print!(
            "{}",
            std::fs::read_to_string(fixture("success.jsonl")).unwrap()
        );
        exit_now();
    }
}

#[test]
#[ignore = "helper, run by the tests below"]
fn helper_sleep() {
    if helper_requested() {
        std::thread::sleep(Duration::from_secs(10));
    }
}

fn exit_now() -> ! {
    use std::io::Write;
    std::io::stdout().flush().unwrap();
    let code = std::env::var("OWLSHIFT_HELPER_EXIT")
        .unwrap()
        .parse()
        .unwrap();
    std::process::exit(code);
}

/// A prompt larger than a pipe's buffer reaches the child whole, every
/// output line reaches the caller, and the exit status decides.
#[test]
fn drive_sends_the_prompt_and_streams_the_output() {
    let prompt = "x".repeat(300 * 1024);
    let mut child = helper("helper_replay", 0).spawn().unwrap();
    let mut logged = Vec::new();
    let run = drive(&mut child, &prompt, |line| logged.extend_from_slice(line)).unwrap();
    assert_eq!(
        run.outcome,
        Outcome::Completed {
            text: "pong".into()
        }
    );
    assert_eq!(
        String::from_utf8_lossy(&run.stderr),
        format!("prompt bytes: {}", prompt.len())
    );
    let recorded = std::fs::read(fixture("success.jsonl")).unwrap();
    assert!(
        logged
            .windows(recorded.len())
            .any(|window| window == recorded),
        "the log holds every recorded line"
    );

    let mut child = helper("helper_replay", 1).spawn().unwrap();
    let run = drive(&mut child, "hi", |_| {}).unwrap();
    assert!(matches!(
        run.outcome,
        Outcome::Failed(Failure::Error {
            exit_code: Some(1),
            ..
        })
    ));
}

#[test]
fn drive_does_not_wait_for_a_process_holding_the_output_open() {
    let started = Instant::now();
    let mut child = helper("helper_leave_a_pipe_holder", 0).spawn().unwrap();
    let run = drive(&mut child, "hi", |_| {}).unwrap();
    assert!(
        matches!(run.outcome, Outcome::Completed { .. }),
        "{:?}",
        run.outcome
    );
    // The grandchild holds stdout and stderr open for 10 s. The helper exits
    // right after starting it, so one grace period plus the helper's start-up
    // is the bound; a second grace period spent on stderr or the prompt
    // would reach it.
    let elapsed = started.elapsed();
    assert!(elapsed >= EXIT_GRACE, "{elapsed:?}");
    assert!(elapsed < 2 * EXIT_GRACE, "{elapsed:?}");
}
