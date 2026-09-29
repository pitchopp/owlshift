//! A tiny real run of `claude -p` spawned as the executor will spawn agents:
//! with the agent environment built from your own. It checks that the run
//! still finds your subscription login and nothing else: no MCP server, and
//! no credential for git or gh. It spends a little of the subscription, so it
//! never runs in CI:
//!
//! ```sh
//! OWLSHIFT_LIVE_CLAUDE=1 cargo test -p owlshift-runner --test agent_live -- --ignored
//! ```

use std::path::Path;

use owlshift_adapters::harness::claude::{Billing, Effort, Outcome, Request, command, drive};
use owlshift_contracts::brief::{PermissionLevel, Permissions};
use owlshift_contracts::ids::RelativePath;
use owlshift_runner::agent_env::{AgentEnv, mcp_findings};

#[test]
#[ignore = "live: runs `claude -p` on your login; set OWLSHIFT_LIVE_CLAUDE=1"]
fn a_real_agent_run_keeps_the_login_and_nothing_else() {
    assert_eq!(
        std::env::var("OWLSHIFT_LIVE_CLAUDE").as_deref(),
        Ok("1"),
        "this test spends subscription usage: set OWLSHIFT_LIVE_CLAUDE=1 to run it"
    );
    let workdir = tempfile::tempdir().unwrap();
    let agent = AgentEnv::from_runner(&[], &[]).unwrap();
    assert_eq!(agent.check(workdir.path(), &["github.com"]), []);

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
    let mut command = command(Path::new("claude"), &request).unwrap();
    agent.apply(&mut command);
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
