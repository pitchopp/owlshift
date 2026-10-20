//! Contract tests of the Claude Code harness: recorded `claude -p` output
//! replayed through the adapter, with no model and no login.
//!
//! The fixtures in `fixtures/claude/` were recorded on 2026-10-19 with Claude
//! Code 2.1.302 by `tests/claude_record.rs` (OWL-338), but for the
//! `peer_inbox_*` pair, made by hand on 2.1.283 (see its test). The recorder
//! also scrubs them: session, message, request and tool-use ids, timestamps,
//! paths and thinking signatures replaced, the user's skills, plugins,
//! agents, commands and memory paths removed from the `init` event, and the
//! inbox socket replaced (see `docs/design/build-plan.md`, OWL-14, OWL-46,
//! OWL-52, OWL-53, OWL-77, OWL-327 and OWL-338 results). Each test states the exit status and standard error
//! the run had, since both decide the outcome; the recorder checks them.
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
    Billing, CommandError, EXIT_GRACE, Effort, Failure, Outcome, RateLimitStatus, Request, Run,
    Transcript, command, drive,
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
    assert_eq!(run.harness_version.as_deref(), Some("2.1.302"));
    assert_eq!(run.model.as_deref(), Some("claude-haiku-5-5"));
    assert_eq!(run.malformed_lines, 0);
    assert!(run.permission_denials.is_empty());

    let usage = run.usage.unwrap();
    assert_eq!(
        (usage.input_tokens, usage.output_tokens),
        (2, 4),
        "{usage:?}"
    );
    assert_eq!(usage.cache_read_input_tokens, 16868);
    assert_eq!(usage.cache_creation_input_tokens, 4115);
    assert_eq!(usage.cost_usd, Some(0.00099388));
    assert_eq!((usage.duration_ms, usage.num_turns), (Some(1371), Some(1)));
    let haiku = &usage.models["claude-haiku-5-5"];
    assert_eq!((haiku.input_tokens, haiku.output_tokens), (2, 4));

    let limit = run.rate_limit.unwrap();
    assert_eq!(limit.status, RateLimitStatus::Allowed);
    assert_eq!(limit.window.as_deref(), Some("five_hour"));
    assert_eq!(
        limit.resets_at,
        Some(Timestamp::from_second(1792425600).unwrap())
    );
    assert_eq!(limit.five_hour_utilization, Some(0.31));
    assert_eq!(limit.seven_day_utilization, Some(0.34));
}

/// Constructed: the warning a run reported on 2.1.284, when the seven-day
/// window passed its threshold (the success run of OWL-77). The 2.1.302
/// recording reports a plain `allowed`, so this line keeps the warning read.
#[test]
fn rate_limit_warning() {
    let line = br#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed_warning","resetsAt":1790748000,"rateLimitType":"seven_day","utilization":0.8,"isUsingOverage":false,"surpassedThreshold":0.75,"unifiedWindows":{"five_hour":{"utilization":0.19,"resetsAt":1790701200},"seven_day":{"utilization":0.8,"resetsAt":1790748000}}}}
"#;
    let mut transcript = Transcript::default();
    transcript.feed(line);
    let limit = transcript.finish(Some(0), Vec::new()).rate_limit.unwrap();
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

/// The three calls of a run recorded by `after_cd` in `claude_record.rs`,
/// each with whether its result was an error: a Bash `cd sub && pwd` that
/// printed `sub`, then a `Write` of the result, then one of the same name
/// below `sub`. Checks the order and the paths, and returns the two writes'
/// error flags.
fn writes_after_cd(name: &str) -> (bool, bool) {
    let events = recorded_events(name);
    let calls = tool_calls(&events);
    let results = tool_results(&events);
    let result_of = |call: &Value| {
        let found = results
            .iter()
            .find(|result| result["tool_use_id"] == call["id"]);
        let result = found.unwrap_or_else(|| panic!("{name}: no result for {call}"));
        (result["content"].to_string(), result["is_error"] == true)
    };
    let names: Vec<&str> = calls.iter().map(|c| c["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["Bash", "Write", "Write"], "{name}");
    assert_eq!(calls[0]["input"]["command"], "cd sub && pwd", "{name}");
    let (printed, failed) = result_of(calls[0]);
    assert!(
        !failed && printed.contains("/work/sub"),
        "{name}: {printed}"
    );
    let paths: Vec<&Value> = calls[1..]
        .iter()
        .map(|call| &call["input"]["file_path"])
        .collect();
    assert_eq!(
        paths,
        [
            "/work/.owlshift/result.json",
            "/work/sub/.owlshift/result.json"
        ],
        "{name}"
    );
    (result_of(calls[1]).1, result_of(calls[2]).1)
}

/// The denied write's path, from the `result` record.
fn denied_path(name: &str) -> Value {
    let events = recorded_events(name);
    let result = events
        .iter()
        .find(|event| event["type"] == "result")
        .unwrap();
    result["permission_denials"][0]["tool_input"]["file_path"].clone()
}

/// The read-only rule holds after a Bash `cd` (OWL-323): the adapter's
/// read-only argv names the result anchored at the working directory, and
/// `read_only_after_cd.jsonl`, recorded with that argv in a folder whose path
/// holds a space, shows the result written after `cd sub` and the same name
/// below `sub` refused. Exit 0, empty stderr.
#[test]
fn the_read_only_result_survives_a_cd() {
    let dir = Path::new("/work");
    let request = Request {
        workdir: dir.to_path_buf(),
        model: None,
        effort: None,
        permissions: Permissions {
            level: PermissionLevel::ReadOnly,
            network: false,
            browser: false,
        },
        result_path: RelativePath::new(".owlshift/result.json").unwrap(),
        json_schema: None,
        max_budget_usd: None,
        bash: false,
        session: None,
    };
    let args: Vec<String> = command(Path::new("claude"), &request)
        .unwrap()
        .get_args()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    let at = args.iter().position(|arg| arg == "--allowedTools").unwrap();
    assert_eq!(args[at + 1], "Edit(/.owlshift/result.json)");
    // Bash is the resolver's alone (OWL-344).
    assert!(!args.iter().any(|arg| arg == "Bash"), "{args:?}");

    let name = "read_only_after_cd.jsonl";
    assert_eq!(writes_after_cd(name), (false, true));
    let run = replay(name, 0);
    assert!(matches!(run.outcome, Outcome::Completed { .. }), "{run:?}");
    assert_eq!(run.permission_denials, ["Write"]);
    assert_eq!(denied_path(name), "/work/sub/.owlshift/result.json");
}

/// What the rule shipped before OWL-323, `Edit(./.owlshift/result.json)`,
/// did on Claude Code: after the same `cd`, the result was refused and the
/// same name below `sub` written, as in Locary's first run. This pins the
/// CLI's behaviour the anchor answers, not the adapter's argv. Exit 0, empty
/// stderr.
#[test]
fn a_dot_rule_follows_the_shell_after_a_cd() {
    let name = "read_only_after_cd_dot_rule.jsonl";
    assert_eq!(writes_after_cd(name), (true, false));
    let run = replay(name, 0);
    assert_eq!(run.permission_denials, ["Write"]);
    assert_eq!(denied_path(name), "/work/.owlshift/result.json");
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

/// The command line of every kind of launch: both permission levels and the
/// resolver's read-only launch with Bash (OWL-344), with and without network,
/// with a JSON Schema.
fn every_launch() -> Vec<(PermissionLevel, bool, Vec<String>)> {
    let mut launches = Vec::new();
    for (level, bash) in [
        (PermissionLevel::ReadOnly, false),
        (PermissionLevel::ReadOnly, true),
        (PermissionLevel::WriteWorktree, false),
    ] {
        for network in [true, false] {
            let args = launch_args(&launch(level, network, bash)).unwrap();
            launches.push((level, network, args));
        }
    }
    launches
}

/// A launch at `level`, with or without network and Bash, with a JSON
/// Schema.
fn launch(level: PermissionLevel, network: bool, bash: bool) -> Request {
    Request {
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
        bash,
        session: None,
    }
}

/// The arguments `command` gives a request.
fn launch_args(request: &Request) -> Result<Vec<String>, CommandError> {
    Ok(command(Path::new("claude"), request)?
        .get_args()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect())
}

/// The resolver's launch (OWL-344): `dontAsk` with bare `Bash` as its one
/// allowed tool and no `Edit(...)` rule, with or without network, the argv
/// of the live check recorded in `docs/design/build-plan.md`. Bash is a
/// flag of the read-only level only: a run that writes the worktree already
/// allows every tool, so the flag there is refused.
#[test]
fn the_resolver_argv_allows_bash_without_an_edit_rule() {
    for network in [true, false] {
        let args = launch_args(&launch(PermissionLevel::ReadOnly, network, true)).unwrap();
        let case = format!("network {network}: {args:?}");
        let mode = positions(&args, "--permission-mode");
        assert_eq!(mode.len(), 1, "{case}");
        assert_eq!(args[mode[0] + 1], "dontAsk", "{case}");
        let allowed = positions(&args, "--allowedTools");
        assert_eq!(allowed.len(), 1, "{case}");
        let tools: Vec<&String> = args[allowed[0] + 1..]
            .iter()
            .take_while(|arg| !arg.starts_with("--"))
            .collect();
        assert_eq!(tools, ["Bash"], "{case}");
        assert!(!args.iter().any(|arg| arg.starts_with("Edit(")), "{case}");
    }

    let mut resolver = launch(PermissionLevel::ReadOnly, false, true);
    resolver.json_schema = None;
    resolver.model = Some("claude-sonnet-5-5".into());
    resolver.effort = Some(Effort::Low);
    let expected = [
        "-p",
        "--output-format",
        "stream-json",
        "--verbose",
        "--model",
        "claude-sonnet-5-5",
        "--effort",
        "low",
        "--permission-mode",
        "dontAsk",
        "--allowedTools",
        "Bash",
        "--permission-prompts",
        "none",
        "--disallowedTools",
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
        "WebFetch",
        "WebSearch",
        "--setting-sources",
        "",
        "--strict-mcp-config",
        "--disable-slash-commands",
        "--settings",
        r#"{"crossSessionInbound":"refuse"}"#,
        "--no-session-persistence",
    ];
    assert_eq!(launch_args(&resolver).unwrap(), expected);

    assert!(matches!(
        launch_args(&launch(PermissionLevel::WriteWorktree, true, true)),
        Err(CommandError::Unsupported(_))
    ));
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
/// (`peer_message_sender.jsonl`, on `sonnet`, offered `SendMessage`
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

/// The agent environment switches the inbox itself off, under the refuse
/// flag: `AgentEnv::from_runner` sets `PEER_INBOX_ENV` (OWL-65). The two
/// fixtures are a matched pair, recorded on 2026-09-29 with Claude Code
/// 2.1.283, unconfined, the write-in-worktree argv with network as
/// `command` builds it, plus `--model haiku --effort low`, in the agent
/// environment of `from_runner` as it was before this variable, with a
/// made-up claude.ai login and a local stand-in for the model (reached
/// through `ANTHROPIC_BASE_URL`) that played one Bash call. Only the
/// variable differs: `peer_inbox_open.jsonl` ran without it and its `init`
/// names a socket, `peer_inbox_switched_off.jsonl` ran with it and names
/// none. Each run completed. Exit 0, empty stderr, for both.
#[test]
fn the_agent_environment_switches_the_peer_inbox_off() {
    // The variable the recording was made with.
    assert_eq!(
        owlshift_adapters::harness::claude::PEER_INBOX_ENV,
        ("CLAUDE_CODE_HARBOR_KITE", "0")
    );
    let init = |name: &str| {
        recorded_events(name)
            .into_iter()
            .find(|event| event["type"] == "system" && event["subtype"] == "init")
            .unwrap_or_else(|| panic!("{name} has no init event"))
    };
    for (name, socket) in [
        ("peer_inbox_open.jsonl", true),
        ("peer_inbox_switched_off.jsonl", false),
    ] {
        let init = init(name);
        assert_eq!(init["claude_code_version"], "2.1.283", "{name}");
        assert_eq!(
            init.get("messaging_socket_path").is_some(),
            socket,
            "{name}: {init}"
        );
        assert_eq!(
            replay(name, 0).outcome,
            Outcome::Completed {
                text: "DONE".into()
            },
            "{name}"
        );
    }
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
    // The stand-in's made-up key, as a real run reports one (OWL-120).
    assert_eq!(run.billing, Billing::ApiKey("ANTHROPIC_API_KEY".into()));
}

/// Every launch switches skills off, Claude Code's bundled ones included:
/// under `--setting-sources ""` alone, a confined run's `init` listed the
/// CLI's own skills and offered `Skill`, and a run asked to invoke one got
/// its text; with `--disable-slash-commands`, `init` listed no skill and no
/// slash command and `Skill` was not offered (build plan, OWL-333, Claude
/// Code 2.1.302). Only the command line is pinned here: the fixtures recorded
/// since OWL-338 were made under the flag and offer no `Skill`, and the
/// CLI's behaviour under it is checked by the live test `bundled_skills_do_not_reach_a_confined_run` in
/// `owlshift-testkit/tests/executor_live.rs`.
#[test]
fn bundled_skills_are_disabled_on_every_launch() {
    for (level, network, args) in every_launch() {
        let case = format!("{level:?}, network {network}: {args:?}");
        assert_eq!(
            positions(&args, "--disable-slash-commands").len(),
            1,
            "{case}"
        );
    }
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

/// How long the grandchild holds the output pipes open.
const PIPE_HOLDER_SLEEP: Duration = Duration::from_secs(10);

// The pipe-holder test's upper bound is `PIPE_HOLDER_SLEEP - EXIT_GRACE`: keep
// it at least two grace periods above the lower bound, or the test could
// never pass (and the subtraction could underflow).
const _: () = assert!(PIPE_HOLDER_SLEEP.as_millis() >= 4 * EXIT_GRACE.as_millis());

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
        std::thread::sleep(PIPE_HOLDER_SLEEP);
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
    // The grandchild holds stdout and stderr open for `PIPE_HOLDER_SLEEP`. The
    // helper exits right after starting it, so `drive` returns after one grace
    // period. The upper bound is `PIPE_HOLDER_SLEEP - EXIT_GRACE` (8 s today): well
    // above a run slowed by a loaded machine (4.6 s seen under a confined
    // `cargo test --workspace`), and below the at least 10 s a `drive` that
    // waited for the pipe holder would take. It catches that wait, not a second grace
    // period spent on stderr or the prompt (that would add only 2 s): the unit test
    // `drive_spends_one_grace_period_not_two_when_the_output_stays_held`, in
    // `src/harness/claude.rs`, catches that one on a clock of its own.
    let elapsed = started.elapsed();
    assert!(elapsed >= EXIT_GRACE, "{elapsed:?}");
    assert!(elapsed < PIPE_HOLDER_SLEEP - EXIT_GRACE, "{elapsed:?}");
}
