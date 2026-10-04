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

/// OWL-41: the smoke scenario with the fake harness and the gate inside the
/// OS sandbox, as agent runs are. A legitimate build still commits and is
/// pushed. The other scenarios run bare: they check the pipeline and the
/// isolation check, which the sandbox would pre-empt. Skipped, with a
/// message, where agents cannot be confined, unless
/// `OWLSHIFT_REQUIRE_CONFINEMENT` is set.
#[test]
fn smoke_confined() {
    let parent = std::env::vars_os();
    if let Err(error) = owlshift_runner::agent_env::AgentEnv::new(parent)
        .unwrap()
        .sandbox_ready()
    {
        assert!(
            std::env::var_os("OWLSHIFT_REQUIRE_CONFINEMENT").is_none(),
            "confinement is required here, but: {error}"
        );
        eprintln!("skipped: agent runs cannot be confined here: {error}");
        return;
    }
    let path = scenarios().join("smoke.toml");
    let input = std::fs::read_to_string(&path).unwrap();
    let input = format!("confined = true\n{input}");
    if let Err(error) = play_str(
        "smoke-confined",
        &input,
        &path.with_extension(""),
        fake_harness(),
    ) {
        panic!("{error}");
    }
}

/// OWL-116's acceptance, the P2 exit gate in small: an incomplete answer is
/// re-asked, the open question alone, over two question rounds.
#[test]
fn reask() {
    if let Err(error) = play(&scenarios().join("reask.toml"), fake_harness()) {
        panic!("{error}");
    }
}

/// OWL-138's acceptance, in the stand-in driver: a discoverable question is
/// decided and the round holds the always-human one alone; a run whose
/// questions are all decided goes on without a round.
#[test]
fn resolver() {
    if let Err(error) = play(&scenarios().join("resolver.toml"), fake_harness()) {
        panic!("{error}");
    }
}

/// OWL-123's acceptance: past the re-ask limit, a fourth incomplete answer
/// parks the ticket with a PARKED comment naming what is still open and
/// what restarts it. The project names a parked state, which the ticket
/// shows (OWL-148).
#[test]
fn the_reask_limit_parks_the_ticket_with_a_parked_comment() {
    let answered_again = |n: u32| {
        format!(
            r#"
        [[step]]
        comment = {{ author = "maintainer", body = "Q2: answer {n}.\n" }}

        [[step]]
        answer = {{ result = "results/check-round1-q2-still-partial.json" }}
        expect = {{ event = "incomplete", reasks = {n}, waiting = "needs_input" }}
        "#
        )
    };
    let scenario = format!(
        r#"
        description = "Round 1's Q2 stays partial through three re-asks: the fourth incomplete answer parks the ticket."
        ticket = "DEMO-3"
        start = "2026-09-28T09:00:00Z"

        [[step]]
        dispatch = true

        [[step]]
        run = {{ result = "results/questions-round1.json" }}

        [[step]]
        comment = {{ author = "maintainer", body = "Q1: English.\nQ2: \"Hello, reader.\"\n" }}

        [[step]]
        answer = {{ result = "results/check-round1-q2-partial.json" }}
        expect = {{ event = "incomplete", reasks = 1 }}
        {}{}
        [[step]]
        comment = {{ author = "maintainer", body = "Q2: answer 4.\n" }}

        [[step]]
        answer = {{ result = "results/check-round1-q2-still-partial.json" }}
        [step.expect]
        event = "incomplete"
        waiting = "parked"
        round = 1
        comments = 9
        tracker_stage = "Parked"
        [step.expect.last_comment]
        author = "owlshift"
        first_line = "[owlshift] PARKED"
        contains = [
          "Parked: the answers stayed incomplete, after 3 re-asks. Still open in round 1:",
          "**Q2** (scope) What should the greeting say, and should it end with a sign-off?\nStill open (partial): Still no word on the sign-off.",
          "**To restart it:** answer the questions still open here, then run `owlshift continue DEMO-3`.",
          '<!-- owlshift:{{"format":1,"kind":"PARKED","ticket":"DEMO-3"}} -->',
        ]
        lacks = ["**Q1**"]
        "#,
        answered_again(2),
        answered_again(3),
    );
    if let Err(error) = play_str(
        "reask-limit",
        &scenario,
        &scenarios().join("reask"),
        fake_harness(),
    ) {
        panic!("{error}");
    }
}

/// OWL-123's acceptance: a second failed run parks the ticket with a PARKED
/// comment; no question round was asked, so `do` runs it again. The project
/// names no parked state, so the ticket shows needs input (OWL-148).
#[test]
fn a_second_failed_run_parks_the_ticket_with_a_parked_comment() {
    play_on_smoke(
        "failed-runs",
        r#"
        description = "The build crashes twice: the second failure parks the ticket."
        ticket = "DEMO-1"
        start = "2026-09-28T09:00:00Z"

        [[step]]
        dispatch = true

        [[step]]
        run = { stderr = "fatal: out of memory\n", exit_code = 101 }
        expect = { event = "run_failed", failed_runs = 1, comments = 0 }

        [[step]]
        run = { stderr = "fatal: out of memory\n", exit_code = 101 }
        [step.expect]
        event = "run_failed"
        waiting = "parked"
        comments = 1
        tracker_stage = "Needs Input"
        [step.expect.last_comment]
        author = "owlshift"
        first_line = "[owlshift] PARKED"
        contains = [
          "Parked: a second run failed. The last failure: ",
          "**To restart it:** run `owlshift do DEMO-1` to run it again.",
        ]
        "#,
    );
}

/// The answer check runs once answers arrive: with no comment from the
/// decider since the questions, an `answer` step is refused before any run.
#[test]
fn an_answer_check_waits_for_an_answer() {
    let early = r#"
        description = "The answer check is asked for before the decider replied."
        ticket = "DEMO-3"
        start = "2026-09-28T09:00:00Z"

        [[step]]
        dispatch = true

        [[step]]
        run = { result = "results/questions-round1.json" }

        [[step]]
        comment = { author = "reporter", body = "Q1: French, surely.\n" }

        [[step]]
        answer = { result = "results/check-round1-q2-partial.json" }
    "#;
    let error = play_str("early", early, &scenarios().join("reask"), fake_harness()).unwrap_err();
    assert_eq!(
        error.to_string(),
        "early, step 4 (answer): no comment from the decider since 2026-09-28T09:02:00Z: the \
         answer check runs once answers arrive"
    );
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

/// OWL-44's acceptance: a variable the project declares in `stack.gate_env`
/// reaches the gate. `git --config-env` fails with "missing environment
/// variable" when the variable is unset, the same under sh and cmd.
#[test]
fn a_declared_variable_reaches_the_gate() {
    play_on_gate(
        "gate-env",
        r#"
        description = "The fixture declares OWLSHIFT_GATE_PROBE for its gate and the runner has it: the gate, which needs it, passes."
        ticket = "DEMO-2"
        start = "2026-09-28T09:00:00Z"
        gate = ["git --config-env=owlshift.probe=OWLSHIFT_GATE_PROBE config --get owlshift.probe"]
        runner_env = { OWLSHIFT_GATE_PROBE = "on" }

        [[step]]
        dispatch = true

        [[step]]
        run = { files = { "GREETING.md" = "Hello.\n" }, commit = "Add a greeting", result = "results/build-done.json" }
        expect = { event = "completed", stage = "verify", gate_failure = "none", branch_pushed = true }
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
