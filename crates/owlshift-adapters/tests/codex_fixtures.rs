//! Checks on the Codex recordings in `fixtures/codex/`, which the Codex
//! harness adapter's contract tests (OWL-299) replay. There is no Codex
//! adapter yet: these tests read the recordings, never a harness.
//!
//! The recordings were made on 2026-10-15 with codex-cli 0.154.0 for checks
//! C2 and C7 (Codex), by a throwaway script whose runs and results are in
//! `docs/design/build-plan.md` (results, "C2" and "C7 (Codex)"). Each
//! `<name>.jsonl` is the `--json` stream; `<name>.meta.json` says how it was
//! recorded: the command line without the prompt, the platform, the
//! confinement, the exit status, standard error and the `-o` file. Thread
//! ids, request ids, Cloudflare ray ids, timestamps and local paths were
//! replaced before they were committed; the checks below keep it so.

use std::path::{Path, PathBuf};

use serde_json::Value;

/// Every recording, so that one added without its checks fails here.
const FIXTURES: [&str; 12] = [
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
        assert_eq!(meta["codex_version"], "0.154.0", "{name}");
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
