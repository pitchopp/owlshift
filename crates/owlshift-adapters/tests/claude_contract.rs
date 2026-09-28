//! Contract tests of the Claude Code harness: recorded `claude -p` output
//! replayed through the adapter, with no model and no login.
//!
//! The fixtures in `fixtures/claude/` were recorded on 2026-09-28 with Claude
//! Code 2.1.283 (see `docs/design/build-plan.md`, OWL-14 results), then
//! scrubbed: session and message ids, timestamps and paths replaced, the
//! user's skills, plugins and agents removed. Each test states the exit
//! status and standard error the run had, since both decide the outcome.
//! `usage_limit.jsonl` alone is constructed, not recorded: no run has hit a
//! limit on purpose (C7's open item). It follows the shapes C7 logged.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use jiff::Timestamp;
use owlshift_adapters::harness::claude::{
    Billing, EXIT_GRACE, Failure, Outcome, RateLimitStatus, Run, Transcript, drive,
};

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
    assert_eq!(run.harness_version.as_deref(), Some("2.1.283"));
    assert_eq!(run.model.as_deref(), Some("claude-haiku-4-5-20251001"));
    assert_eq!(run.malformed_lines, 0);
    assert!(run.permission_denials.is_empty());

    let usage = run.usage.unwrap();
    assert_eq!(
        (usage.input_tokens, usage.output_tokens),
        (10, 44),
        "{usage:?}"
    );
    assert_eq!(usage.cache_read_input_tokens, 13689);
    assert_eq!(usage.cache_creation_input_tokens, 7993);
    assert_eq!(usage.cost_usd, Some(0.0175849));
    assert_eq!((usage.duration_ms, usage.num_turns), (Some(1037), Some(1)));
    let haiku = &usage.models["claude-haiku-4-5-20251001"];
    assert_eq!((haiku.input_tokens, haiku.output_tokens), (10, 44));

    let limit = run.rate_limit.unwrap();
    assert_eq!(limit.status, RateLimitStatus::Allowed);
    assert_eq!(limit.window.as_deref(), Some("five_hour"));
    assert_eq!(
        limit.resets_at,
        Some(Timestamp::from_second(1790613600).unwrap())
    );
    assert_eq!(limit.five_hour_utilization, Some(0.02));
    assert_eq!(limit.seven_day_utilization, Some(0.62));
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
