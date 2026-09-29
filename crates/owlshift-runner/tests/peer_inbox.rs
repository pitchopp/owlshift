//! The agent environment the runner builds switches Claude Code's inbox for
//! the user's other sessions off, and the command the executor spawns
//! carries the switch (OWL-65, recorded under check C1 in
//! `docs/design/build-plan.md`). The adapter's contract test shows what the
//! CLI does with it.

use std::ffi::OsStr;
use std::process::Command;

use owlshift_adapters::harness::claude::PEER_INBOX_ENV;
use owlshift_runner::agent_env::{AgentEnv, RunPaths};

/// Set in the helper's environment only.
const HELPER: &str = "OWLSHIFT_PEER_INBOX_HELPER";

/// The value the agent environment gives the inbox variable, and how many of
/// its entries name it, in any letter case.
fn inbox(agent: &AgentEnv) -> (Option<&OsStr>, usize) {
    let (name, _) = PEER_INBOX_ENV;
    let entries = agent
        .vars()
        .iter()
        .filter(|(n, _)| n.eq_ignore_ascii_case(name))
        .count();
    (agent.var(name), entries)
}

#[test]
fn the_runner_switches_claude_codes_inbox_off() {
    let (name, value) = PEER_INBOX_ENV;
    let agent = AgentEnv::from_runner(&[], &[]).unwrap();
    assert_eq!(inbox(&agent), (Some(OsStr::new(value)), 1));

    // The command the executor spawns carries it, whatever the harness's own
    // command asked for. Where agents cannot be confined no run starts, and
    // the variables are applied the same way.
    let workdir = tempfile::tempdir().unwrap();
    let run = RunPaths {
        workdir: workdir.path().to_owned(),
        ..RunPaths::default()
    };
    let mut inner = Command::new("claude");
    inner.env(name, "1");
    let spawned = if agent.sandbox_ready().is_ok() {
        agent.confine(inner, &run).unwrap()
    } else {
        agent.apply(&mut inner);
        inner
    };
    let set: Vec<_> = spawned
        .get_envs()
        .filter(|(n, _)| n.eq_ignore_ascii_case(name))
        .collect();
    assert_eq!(set, [(OsStr::new(name), Some(OsStr::new(value)))]);

    // A runner whose own environment turns the inbox on, for a project that
    // declares the name and an operator who allows it.
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "helper_reports_the_agent_value",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(HELPER, "1")
        .env(name, "1")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains(&format!("inbox={value} entries=1;")),
        "{stdout}"
    );
}

/// Run alone by the test above, in a child copy of this binary: setting the
/// test process's own environment is not safe while other threads run.
#[test]
#[ignore = "helper: run by the_runner_switches_claude_codes_inbox_off"]
fn helper_reports_the_agent_value() {
    if std::env::var_os(HELPER).is_none() {
        return;
    }
    let (name, _) = PEER_INBOX_ENV;
    assert_eq!(std::env::var(name).as_deref(), Ok("1"));
    let agent = AgentEnv::from_runner(&[name], &[name]).unwrap();
    let (value, entries) = inbox(&agent);
    println!(
        "inbox={} entries={entries};",
        value.unwrap_or_default().to_string_lossy()
    );
}
