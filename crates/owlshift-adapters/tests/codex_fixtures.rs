//! Checks on the Codex recordings in `fixtures/codex/`, and the Codex
//! harness adapter's contract tests (OWL-299), which replay them through
//! `harness::codex::Transcript`.
//!
//! The recordings were made on 2026-10-15 with codex-cli 0.154.0 for checks
//! C2 and C7 (Codex), by a throwaway script whose runs and results are in
//! `docs/design/build-plan.md` (results, "C2" and "C7 (Codex)"). Each
//! `<name>.jsonl` is the `--json` stream; `<name>.meta.json` says how it was
//! recorded: the command line without the prompt, the platform, the
//! confinement, the exit status, standard error and the `-o` file. Thread
//! ids, request ids, Cloudflare ray ids, timestamps and local paths were
//! replaced before they were committed; the checks below keep it so.
//!
//! `trusted_repo_mcp` and its control were recorded for OWL-131 against a
//! local stand-in for the model, with the adapter's own command line; their
//! records say how (build plan, OWL-130's entry).

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use owlshift_adapters::harness::codex::{self, Failure, Outcome, Run, Transcript};
use owlshift_adapters::harness::tested;
use owlshift_contracts::brief::{PermissionLevel, Permissions};
use owlshift_contracts::{Harness, Role};

/// Every recording, so that one added without its checks fails here.
const FIXTURES: [&str; 15] = [
    "success",
    "write_refused",
    "nested_sandbox",
    "output_schema",
    "invalid_schema",
    "unsupported_effort",
    "unknown_effort",
    "unknown_model",
    "rules_rejected",
    "usage_limit",
    "logged_out",
    "codex_home_read_only",
    "critic",
    "trusted_repo_mcp",
    "trusted_repo_mcp_control",
];

/// The one id left in the recordings, where each thread id was.
const PLACEHOLDER_UUID: &str = "00000000-0000-4000-8000-000000000000";

fn dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex")
}

fn read(file: &str) -> String {
    std::fs::read_to_string(dir().join(file)).unwrap_or_else(|e| panic!("{file}: {e}"))
}

fn events(name: &str) -> Vec<Value> {
    read(&format!("{name}.jsonl"))
        .lines()
        .map(|line| {
            let event: Value =
                serde_json::from_str(line).unwrap_or_else(|e| panic!("{name}.jsonl: {e}: {line}"));
            assert!(
                event["type"].is_string(),
                "{name}.jsonl: an event without a type: {line}"
            );
            event
        })
        .collect()
}

fn meta(name: &str) -> Value {
    serde_json::from_str(&read(&format!("{name}.meta.json")))
        .unwrap_or_else(|e| panic!("{name}.meta.json: {e}"))
}

#[test]
fn the_recording_folder_holds_exactly_the_listed_recordings() {
    let mut found: Vec<String> = std::fs::read_dir(dir())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    found.sort();
    let mut expected: Vec<String> = FIXTURES
        .iter()
        .flat_map(|name| [format!("{name}.jsonl"), format!("{name}.meta.json")])
        .collect();
    expected.sort();
    assert_eq!(found, expected);
}

#[test]
fn every_recording_is_a_stream_of_typed_events_with_its_record() {
    for name in FIXTURES {
        events(name);
        let meta = meta(name);
        // A recording's version is a tested one: re-recording with a new
        // release makes `tested.rs` follow.
        let version = meta["codex_version"].as_str().unwrap();
        assert!(
            tested::is_tested(Harness::Codex, version),
            "{name}: Codex {version} is missing from harness/tested.rs"
        );
        assert!(meta["exit_status"].is_i64(), "{name}");
        assert!(
            meta["platform"] == "macos" || meta["platform"] == "linux",
            "{name}"
        );
        let argv = meta["argv"].as_array().unwrap();
        assert_eq!(argv[..2], ["codex", "exec"], "{name}");
    }
}

/// The rule the adapter can rely on, seen on every run: a run exits
/// non-zero exactly when its stream ends with `turn.failed` or is empty
/// (Codex stopped before the thread started).
#[test]
fn a_run_fails_exactly_when_its_stream_ends_in_turn_failed_or_is_empty() {
    for name in FIXTURES {
        let events = events(name);
        let last = events.last().map(|event| event["type"].as_str().unwrap());
        let failed = matches!(last, None | Some("turn.failed"));
        assert_eq!(
            meta(name)["exit_status"] != 0,
            failed,
            "{name}: last event {last:?}"
        );
        if !failed {
            assert_eq!(last, Some("turn.completed"), "{name}");
        }
    }
}

/// Replays a recording through the adapter's transcript, with its recorded
/// exit status.
fn replay(name: &str) -> Run {
    let mut transcript = Transcript::default();
    for line in read(&format!("{name}.jsonl")).lines() {
        transcript.feed(line.as_bytes());
    }
    let exit = meta(name)["exit_status"].as_i64().unwrap();
    transcript.finish(Some(i32::try_from(exit).unwrap()), Vec::new())
}

/// How the adapter classifies each recording. That the exit status agrees
/// with the stream is pinned above; this is what the runner is told.
#[test]
fn the_adapter_reads_each_recording_as_the_checks_found_it() {
    for name in ["success", "nested_sandbox"] {
        assert_eq!(
            replay(name).outcome,
            Outcome::Completed { text: "OK".into() },
            "{name}"
        );
    }
    for name in ["write_refused", "rules_rejected"] {
        assert!(
            matches!(replay(name).outcome, Outcome::Completed { .. }),
            "{name}"
        );
    }
    // The last agent message is what `-o` wrote.
    let schema = replay("output_schema");
    assert_eq!(
        schema.outcome,
        Outcome::Completed {
            text: meta("output_schema")["output_last_message"]
                .as_str()
                .unwrap()
                .to_owned()
        }
    );
    let usage = replay("success").usage.unwrap();
    assert_eq!(
        (
            usage.input_tokens,
            usage.cache_read_input_tokens,
            usage.output_tokens
        ),
        (13725 - 11520, 11520, 21)
    );

    for (name, status) in [
        ("invalid_schema", 400),
        ("unsupported_effort", 400),
        ("unknown_effort", 400),
        ("unknown_model", 400),
        ("logged_out", 401),
    ] {
        let run = replay(name);
        assert!(
            matches!(
                &run.outcome,
                Outcome::Failed(Failure::Error {
                    exit_code: Some(1),
                    api_status: Some(found),
                    message: Some(_),
                }) if *found == status
            ),
            "{name}: {:?}",
            run.outcome
        );
        assert_eq!(run.usage, None, "{name}");
    }
    assert_eq!(replay("usage_limit").outcome, Outcome::UsageLimit);

    // The live critic run of `owlshift-testkit/tests/codex_live.rs` (OWL-299):
    // the adapter's command line of then, before OWL-131's two `-c`,
    // confined; its last message is the result the runner validated. The
    // stand-in runs of OWL-131 end the same way, with the MCP call or not.
    for name in ["critic", "trusted_repo_mcp", "trusted_repo_mcp_control"] {
        let run = replay(name);
        let Outcome::Completed { text } = &run.outcome else {
            panic!("{name}: {:?}", run.outcome);
        };
        assert_eq!(
            text,
            meta(name)["output_last_message"].as_str().unwrap(),
            "{name}"
        );
        assert!(run.usage.is_some(), "{name}");
    }
    assert_eq!(
        replay("codex_home_read_only").outcome,
        Outcome::Failed(Failure::NoThread)
    );
    for name in FIXTURES {
        assert_eq!(replay(name).malformed_lines, 0, "{name}");
    }
}

/// OWL-131 (decision D1 of OWL-130): a repository the user's Codex trusts
/// starts no MCP server in a critic run, and neither does the user's own
/// configuration. Both recordings ran in a linked worktree of a repository
/// whose `.codex/config.toml` declares the server `probe`, with a
/// `CODEX_HOME` whose `config.toml` trusts it and declares `userprobe`; a
/// local stand-in for the model called `probe`'s tool `ping` in both.
/// Without `--ignore-user-config` both servers started and the call reached
/// `probe`; on the adapter's command line neither started and Codex refused
/// the call, so no MCP item reached the stream.
///
/// The recorded command line is `command()`'s, so a change to the adapter's
/// flags fails here until the pair is recorded again; the unit test in
/// `harness/codex.rs` is where the flags are read.
#[test]
fn a_trusted_repository_starts_no_mcp_server_in_a_critic_run() {
    let request = codex::Request {
        workdir: PathBuf::from("<worktree>"),
        model: None,
        effort: None,
        role: Role::Verify,
        permissions: Permissions {
            level: PermissionLevel::ReadOnly,
            network: false,
            browser: false,
        },
        schema_file: PathBuf::from("<run-files>/schema.json"),
        last_message_file: PathBuf::from("<run-files>/last-message.json"),
    };
    let command = codex::command(Path::new("codex"), &request).unwrap();
    let mut adapter = vec!["codex".to_owned()];
    adapter.extend(
        command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned()),
    );
    let argv = |name: &str| -> Vec<String> {
        meta(name)["argv"]
            .as_array()
            .unwrap()
            .iter()
            .map(|arg| arg.as_str().unwrap().to_owned())
            .collect()
    };
    assert_eq!(argv("trusted_repo_mcp"), adapter);
    assert!(adapter.iter().any(|arg| arg == "--ignore-user-config"));
    // The control differs by that flag alone.
    let without: Vec<String> = adapter
        .iter()
        .filter(|arg| *arg != "--ignore-user-config")
        .cloned()
        .collect();
    assert_eq!(argv("trusted_repo_mcp_control"), without);

    let mcp_items = |name: &str| -> Vec<Value> {
        events(name)
            .into_iter()
            .map(|event| event["item"].clone())
            .filter(|item| item["type"] == "mcp_tool_call")
            .collect()
    };
    let called: Vec<Value> = mcp_items("trusted_repo_mcp_control")
        .into_iter()
        .filter(|item| item["status"] == "completed")
        .collect();
    assert_eq!(called.len(), 1, "{called:?}");
    assert_eq!(called[0]["server"], "probe");
    assert_eq!(called[0]["tool"], "ping");
    assert_eq!(called[0]["result"]["content"][0]["text"], "PONG-repo");
    assert_eq!(
        meta("trusted_repo_mcp_control")["marks"],
        json!([
            "mcp.codexhome.started",
            "mcp.repo.called",
            "mcp.repo.started"
        ])
    );

    let guarded = "trusted_repo_mcp";
    assert_eq!(mcp_items(guarded), Vec::<Value>::new());
    assert_eq!(meta(guarded)["marks"], json!([]));
    let stderr = meta(guarded)["stderr"].to_string();
    assert!(
        stderr.contains("unsupported call: mcp__probeping"),
        "{stderr}"
    );
}

/// The values left where request ids, Cloudflare ray ids, timestamps and
/// home folders were.
const PLACEHOLDER_REQUEST: &str = "req_00000000000000000000000000000000";
const PLACEHOLDER_RAY: &str = "0000000000000000-XXX";
const PLACEHOLDER_TIMESTAMP: &str = "2026-10-15T12:00:00";
const PLACEHOLDER_HOME: &str = "user";

/// What was scrubbed stays scrubbed: no local path naming a user or the
/// recording machine's folders, no e-mail address, no token shape, and no
/// thread id, request id, ray id, timestamp or home folder but its
/// placeholder.
#[test]
fn the_recordings_carry_nothing_personal() {
    for name in FIXTURES {
        for file in [format!("{name}.jsonl"), format!("{name}.meta.json")] {
            let text = read(&file);
            for needle in ["/Users/", "/private/", "claude-501", "eyJ", "sk-"] {
                assert!(!text.contains(needle), "{file} contains {needle:?}");
            }
            assert!(!has_email(&text), "{file} contains an e-mail address");
            for id in uuids(&text) {
                assert_eq!(id, PLACEHOLDER_UUID, "{file}");
            }
            for id in following(&text, "req_", |c| c.is_ascii_alphanumeric()) {
                assert_eq!(format!("req_{id}"), PLACEHOLDER_REQUEST, "{file}");
            }
            for ray in following(&text, "cf-ray: ", |c| c.is_ascii_alphanumeric() || c == '-') {
                assert_eq!(ray, PLACEHOLDER_RAY, "{file}");
            }
            for stamp in timestamps(&text) {
                assert_eq!(stamp, PLACEHOLDER_TIMESTAMP, "{file}");
            }
            for user in following(&text, "/home/", |c| c != '/' && c != '"' && c != ' ') {
                assert_eq!(user, PLACEHOLDER_HOME, "{file}");
            }
        }
    }
}

/// What follows each occurrence of `prefix`, as long as `keep` holds.
fn following<'a>(text: &'a str, prefix: &str, keep: impl Fn(char) -> bool) -> Vec<&'a str> {
    text.match_indices(prefix)
        .map(|(i, _)| {
            let rest = &text[i + prefix.len()..];
            let end = rest.find(|c: char| !keep(c)).unwrap_or(rest.len());
            &rest[..end]
        })
        .collect()
}

/// Every substring shaped like an ISO 8601 date and time to the second:
/// `YYYY-MM-DDTHH:MM:SS`.
fn timestamps(text: &str) -> Vec<&str> {
    let shape = |s: &str| {
        s.char_indices().all(|(i, c)| match i {
            4 | 7 => c == '-',
            10 => c == 'T',
            13 | 16 => c == ':',
            _ => c.is_ascii_digit(),
        })
    };
    (0..text.len().saturating_sub(18))
        .filter(|&i| text.is_char_boundary(i) && text.is_char_boundary(i + 19))
        .map(|i| &text[i..i + 19])
        .filter(|s| shape(s))
        .collect()
}

fn has_email(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.iter().enumerate().any(|(i, &b)| {
        b == b'@'
            && i > 0
            && bytes[i - 1].is_ascii_alphanumeric()
            && text[i + 1..]
                .split(|c: char| !(c.is_ascii_alphanumeric() || c == '.' || c == '-'))
                .next()
                .is_some_and(|domain| domain.contains('.') && !domain.ends_with('.'))
    })
}

/// Every substring shaped like a UUID: 8-4-4-4-12 hexadecimal digits.
fn uuids(text: &str) -> Vec<&str> {
    let shape = |s: &str| {
        s.len() == 36
            && s.char_indices().all(|(i, c)| match i {
                8 | 13 | 18 | 23 => c == '-',
                _ => c.is_ascii_hexdigit(),
            })
    };
    (0..text.len().saturating_sub(35))
        .filter(|&i| text.is_char_boundary(i) && text.is_char_boundary(i + 36))
        .map(|i| &text[i..i + 36])
        .filter(|s| shape(s))
        .collect()
}

#[test]
fn the_scrub_checks_catch_what_they_look_for() {
    assert!(has_email("written by someone@example.com today"));
    assert!(!has_email("npm install @openai/codex"));
    assert_eq!(
        uuids("thread 01a13f6e-17c3-7ed0-90ab-9d2a59c4f4c6 started"),
        ["01a13f6e-17c3-7ed0-90ab-9d2a59c4f4c6"]
    );
    assert!(uuids("no id here").is_empty());
    assert_eq!(
        following(
            "request id: req_a7c3ddf654a8444fa4d251ed67f7a475, x",
            "req_",
            |c| c.is_ascii_alphanumeric()
        ),
        ["a7c3ddf654a8444fa4d251ed67f7a475"]
    );
    assert_eq!(
        timestamps("at 2026-10-15T11:31:45.447598Z ERROR"),
        ["2026-10-15T11:31:45"]
    );
    assert_eq!(
        following("/home/owl/lab and /home/user", "/home/", |c| c != '/'
            && c != ' '),
        ["owl", "user"]
    );
}
