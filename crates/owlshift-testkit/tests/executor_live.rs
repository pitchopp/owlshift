//! A tiny real role run through the executor on Claude Code: worktree,
//! brief, agent environment, process tree, result and isolation, on your own
//! login. It spends a little of the subscription, so it never runs in CI:
//!
//! ```sh
//! OWLSHIFT_LIVE_CLAUDE=1 cargo test -p owlshift-testkit --test executor_live -- --ignored
//! ```

use std::fs;
use std::time::Duration;

use owlshift_adapters::harness::claude::Effort;
use owlshift_contracts::Role;
use owlshift_contracts::brief::{Author, Brief, PermissionLevel, Permissions, Relation, TicketBrief};
use owlshift_contracts::format::Format;
use owlshift_contracts::ids::{RelativePath, TicketId};
use owlshift_contracts::result::Status;
use owlshift_runner::agent_env::AgentEnv;
use owlshift_runner::executor::harness::ClaudeHarness;
use owlshift_runner::executor::{Executor, Git, Outcome, RunSpec};
use owlshift_testkit::git::{GitEnv, seed};

/// A role small enough for a fast model: read the brief, answer in
/// `result.json`.
const ROLE: &str = "You are a test role. Read the brief, a JSON file. Then write the file named \
    by its `result_path`: a JSON object with exactly the fields `format` (the number 1), \
    `status` (the string \"done\") and `summary` (the brief's `ticket.title`, verbatim). \
    Do nothing else.";

#[test]
#[ignore = "live: runs `claude -p` on your login; set OWLSHIFT_LIVE_CLAUDE=1"]
fn a_real_role_runs_through_the_executor() {
    assert_eq!(
        std::env::var("OWLSHIFT_LIVE_CLAUDE").as_deref(),
        Ok("1"),
        "this test spends subscription usage: set OWLSHIFT_LIVE_CLAUDE=1 to run it"
    );
    let tmp = tempfile::Builder::new()
        .prefix("owlshift live ")
        .tempdir()
        .unwrap();
    let project = tmp.path().join("project");
    fs::create_dir_all(&project).unwrap();
    fs::write(project.join("README.md"), "hello\n").unwrap();
    let env = GitEnv::create(tmp.path().join("home")).unwrap();
    let remote = seed(&env, tmp.path(), &project).unwrap();
    let runner = env.clone();
    let executor = Executor {
        git: Git::with_setup("git", move |command| runner.apply(command)),
        // Your own environment: the agent needs your harness login.
        agent: AgentEnv::from_runner(&[]).unwrap(),
        forge_hosts: vec!["github.com".into()],
        timeout: Duration::from_secs(300),
    };
    let harness = ClaudeHarness {
        // Found on the PATH.
        program: "claude".into(),
        prompt: ROLE.into(),
        model: Some("haiku".into()),
        effort: Some(Effort::Low),
        max_budget_usd: None,
    };
    let title = "Heliotrope forty-two";
    let brief = Brief {
        format: Format,
        role: Role::Verify,
        project: "live".into(),
        ticket: TicketBrief {
            id: TicketId::new("LIVE-1").unwrap(),
            title: title.into(),
            url: None,
            labels: Vec::new(),
            author: Author {
                name: "maintainer".into(),
                relation: Relation::Decider,
            },
            description: "Report the title.".into(),
        },
        decider: "maintainer".into(),
        thread: Vec::new(),
        checkpoint: None,
        zones: Vec::new(),
        resources: Vec::new(),
        rules: Vec::new(),
        permissions: Permissions {
            level: PermissionLevel::ReadOnly,
            network: false,
            browser: false,
        },
        gate: Vec::new(),
        result_path: RelativePath::new("result.json").unwrap(),
    };
    let spec = RunSpec {
        main: &remote.checkout,
        worktree: &tmp.path().join("worktree"),
        branch: "owlshift/LIVE-1",
        base: "origin/main",
        run_dir: &tmp.path().join("run"),
        brief: &brief,
    };

    let report = executor.run(&spec, &harness).unwrap();
    let Outcome::Finished { result, .. } = &report.outcome else {
        panic!(
            "{:?}\nstderr: {}",
            report.outcome,
            fs::read_to_string(&report.stderr_log).unwrap_or_default()
        );
    };
    assert_eq!(result.status, Status::Done);
    assert_eq!(result.summary, title);
    assert!(report.usage.is_some(), "{report:?}");
    assert!(fs::metadata(&report.stdout_log).unwrap().len() > 0);
}
