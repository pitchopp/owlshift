//! A tiny real run of `claude -p` spawned as the executor spawns agents:
//! confined, with the agent environment built from your own, and logged in
//! with the token for agent runs that `owlshift init` stored, in an empty
//! configuration folder of the run's own (OWL-94). It checks that the run
//! completes on the subscription and finds nothing else: no MCP server, and
//! no credential for git or gh. It spends a little of the subscription, so it
//! never runs in CI:
//!
//! ```sh
//! OWLSHIFT_LIVE_CLAUDE=1 cargo test -p owlshift-runner --test agent_live -- --ignored
//! ```

use std::path::Path;
use std::process::{Command, Stdio};

use owlshift_adapters::harness::claude::{Billing, Effort, Outcome, Request, command, drive};
use owlshift_contracts::brief::{PermissionLevel, Permissions};
use owlshift_contracts::ids::RelativePath;
use owlshift_platform::keychain::Keychain;
use owlshift_runner::agent_env::{AgentEnv, RunPaths, mcp_findings};
use owlshift_runner::executor::HARNESS_CONFIG_DIR;
use owlshift_runner::executor::harness::{CLAUDE_AGENT_ACCOUNT, ClaudeHarness, Harness};

#[test]
#[ignore = "live: runs `claude -p` on your token for agent runs; set OWLSHIFT_LIVE_CLAUDE=1"]
fn a_real_agent_run_keeps_the_login_and_nothing_else() {
    assert_eq!(
        std::env::var("OWLSHIFT_LIVE_CLAUDE").as_deref(),
        Ok("1"),
        "this test spends subscription usage: set OWLSHIFT_LIVE_CLAUDE=1 to run it"
    );
    let workdir = tempfile::tempdir().unwrap();
    let init = Command::new("git")
        .args(["init", "-q"])
        .current_dir(workdir.path())
        .status()
        .unwrap();
    assert!(init.success());
    let agent = AgentEnv::from_runner(&[], &[]).unwrap();
    assert_eq!(agent.check(workdir.path(), &["github.com"]), []);

    let token = Keychain::system()
        .unwrap()
        .read(CLAUDE_AGENT_ACCOUNT)
        .unwrap()
        .expect("no token for agent runs: run `claude setup-token`, then `owlshift init`");
    let harness = ClaudeHarness {
        program: "claude".into(),
        prompt: String::new(),
        model: None,
        effort: None,
        max_budget_usd: None,
        login: Some(token),
    };
    let needs = harness.sandbox_needs(&agent).unwrap();
    let login = needs.login.expect("a confined run logs in with the token");
    let temp = tempfile::tempdir().unwrap();
    let config = temp.path().join(HARNESS_CONFIG_DIR);
    std::fs::create_dir(&config).unwrap();
    let run = RunPaths {
        workdir: workdir.path().to_path_buf(),
        readable: needs.readable,
        temp: Some(temp.path().to_path_buf()),
        ..RunPaths::default()
    };

    let request = Request {
        workdir: workdir.path().to_path_buf(),
        model: Some("haiku".into()),
        effort: Some(Effort::Low),
        permissions: Permissions {
            level: PermissionLevel::ReadOnly,
            network: false,
            browser: false,
        },
        result_path: RelativePath::new("result.json").unwrap(),
        json_schema: None,
        max_budget_usd: None,
    };
    let built = command(Path::new("claude"), &request).unwrap();
    let mut inner = Command::new(built.get_program());
    inner.args(built.get_args());
    let mut command = agent.confine(inner, &run).unwrap();
    command
        .env(login.config_variable, &config)
        .env(login.token_variable, login.token.expose())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().expect("`claude` is on the PATH");
    let run = drive(&mut child, "Reply with the single word ok.", |_| {}).unwrap();

    assert!(
        matches!(run.outcome, Outcome::Completed { .. }),
        "{:?}\nstderr: {}",
        run.outcome,
        String::from_utf8_lossy(&run.stderr)
    );
    assert_eq!(
        run.billing,
        Billing::Subscription,
        "the run used an API key"
    );
    assert_eq!(mcp_findings(&run), None);
}
