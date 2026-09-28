//! The scenarios of `tests/scenarios/`, played on the fake harness, and
//! short ones on their fixtures for the executor's guardrails and the gate.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use owlshift_testkit::scenario::{play, play_str};

fn fake_harness() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_owlshift-fake-harness"))
}

fn scenarios() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/scenarios")
}

#[test]
fn smoke() {
    if let Err(error) = play(&scenarios().join("smoke.toml"), fake_harness()) {
        panic!("{error}");
    }
}

/// OWL-16's acceptance: the gate fails first and passes after a fix run.
#[test]
fn gate() {
    if let Err(error) = play(&scenarios().join("gate.toml"), fake_harness()) {
        panic!("{error}");
    }
}

/// Plays a scenario given as text on the gate fixture.
fn play_on_gate(name: &str, scenario: &str) {
    if let Err(error) = play_str(name, scenario, &scenarios().join("gate"), fake_harness()) {
        panic!("{error}");
    }
}

#[test]
fn the_gate_runs_on_the_last_commit_only() {
    play_on_gate(
        "gate-uncommitted",
        r#"
        description = "The build reports done with its greeting left uncommitted: the gate runs on the last commit, so it fails without running."
        ticket = "DEMO-2"
        start = "2026-09-28T09:00:00Z"

        [[step]]
        dispatch = true

        [[step]]
        run = { files = { "GREETING.md" = "Hello.\n" }, result = "results/build-done.json" }
        expect = { event = "run_failed", stage = "build", gate_failure = "uncommitted changes before the gate", branch_pushed = false }
        "#,
    );
}

#[test]
fn a_gate_that_commits_fails() {
    play_on_gate(
        "gate-commits",
        r#"
        description = "The gate passes but commits: it no longer vouches for the commit the run delivers."
        ticket = "DEMO-2"
        start = "2026-09-28T09:00:00Z"
        gate = ["git commit -q --allow-empty -m moved"]

        [[step]]
        dispatch = true

        [[step]]
        run = { files = { "GREETING.md" = "Hello.\n" }, commit = "Add a greeting", result = "results/build-done.json" }
        expect = { event = "run_failed", stage = "build", gate_failure = "the gate moved HEAD", branch_pushed = false }
        "#,
    );
}

#[test]
fn a_gate_breaking_isolation_is_quarantined() {
    play_on_gate(
        "gate-breach",
        r#"
        description = "The gate creates a branch in the shared repository, then fails: the breach wins, quarantined and parked."
        ticket = "DEMO-2"
        start = "2026-09-28T09:00:00Z"
        gate = ["git branch planted && exit 1"]

        [[step]]
        dispatch = true

        [[step]]
        run = { files = { "GREETING.md" = "Hello.\n" }, commit = "Add a greeting", result = "results/build-done.json" }
        expect = { event = "quarantined", waiting = "parked", branch_pushed = false }
        "#,
    );
}

#[test]
fn a_wrong_expectation_fails_and_names_the_step() {
    let wrong = r#"
        description = "Dispatch to the trivial pipeline's first stage, expecting another."
        ticket = "DEMO-1"
        start = "2026-09-28T09:00:00Z"

        [[step]]
        dispatch = true
        expect = { stage = "verify" }
    "#;
    let error = play_str("wrong", wrong, &scenarios().join("smoke"), fake_harness()).unwrap_err();
    assert_eq!(
        error.to_string(),
        "wrong, step 1 (dispatch): expected stage verify, found build"
    );
}

/// Plays a scenario given as text on the smoke fixture.
fn play_on_smoke(name: &str, scenario: &str) {
    if let Err(error) = play_str(name, scenario, &scenarios().join("smoke"), fake_harness()) {
        panic!("{error}");
    }
}

#[test]
fn an_agent_writing_in_the_main_checkout_is_quarantined() {
    play_on_smoke(
        "main-checkout",
        r#"
        description = "The build writes a file in the main checkout: quarantined and parked, its valid result notwithstanding."
        ticket = "DEMO-1"
        start = "2026-09-28T09:00:00Z"

        [[step]]
        dispatch = true

        [[step]]
        run = { main_checkout = { "README.md" = "Rewritten from the worktree.\n" }, result = "results/build-done.json" }
        expect = { event = "quarantined", waiting = "parked", branch_pushed = false }
        "#,
    );
}

#[test]
fn an_agent_switching_branch_is_quarantined() {
    play_on_smoke(
        "switch-branch",
        r#"
        description = "The build commits on another branch than the ticket's: quarantined and parked."
        ticket = "DEMO-1"
        start = "2026-09-28T09:00:00Z"

        [[step]]
        dispatch = true

        [[step]]
        run = { switch_branch = "elsewhere", files = { "GREETING.md" = "Hello.\n" }, commit = "Add a greeting", result = "results/build-done.json" }
        expect = { event = "quarantined", waiting = "parked", branch_pushed = false }
        "#,
    );
}

/// The bound covers the whole scenario, git setup and isolation checks
/// included, which take over 20 s on Windows with the other scenarios running
/// alongside. It stays far from the harness's own 120 s, so a tree left
/// running until the harness ends still fails the test.
#[test]
fn a_run_past_its_deadline_is_stopped_and_fails() {
    let started = Instant::now();
    play_on_smoke(
        "deadline",
        r#"
        description = "The build would take 120 s; the executor stops it at its 1 s deadline, a failed run."
        ticket = "DEMO-1"
        start = "2026-09-28T09:00:00Z"
        timeout_ms = 1000

        [[step]]
        dispatch = true

        [[step]]
        run = { delay_ms = 120000, result = "results/build-done.json" }
        expect = { event = "run_failed", stage = "build", failed_runs = 1 }
        "#,
    );
    assert!(
        started.elapsed() < Duration::from_secs(60),
        "the run was not stopped at its deadline: {:?}",
        started.elapsed()
    );
}
