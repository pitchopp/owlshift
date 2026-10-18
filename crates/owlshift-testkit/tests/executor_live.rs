//! Tiny real role runs through the executor on Claude Code: worktree,
//! brief, agent environment, process tree, result and isolation, confined
//! and on the token for agent runs that `owlshift init` stored (OWL-94).
//! They spend a little of the subscription, so they never run in CI:
//!
//! ```sh
//! OWLSHIFT_LIVE_CLAUDE=1 cargo test -p owlshift-testkit --test executor_live -- --ignored
//! ```

use std::collections::HashMap;
use std::fs;
use std::time::Duration;

use owlshift_adapters::harness::claude::Effort;
use owlshift_contracts::Role;
use owlshift_contracts::brief::{
    Author, Brief, PermissionLevel, Permissions, Relation, ReviewBrief, TicketBrief,
};
use owlshift_contracts::format::{Format, RESULT_FORMAT};
use owlshift_contracts::ids::{RelativePath, TicketId};
use owlshift_contracts::result::{RunResult, Status};
use owlshift_platform::keychain::{Keychain, Secret};
use owlshift_runner::agent_env::AgentEnv;
use owlshift_runner::executor::harness::{CLAUDE_AGENT_ACCOUNT, ClaudeHarness};
use owlshift_runner::executor::{Executor, Git, Outcome, RunReport, RunSpec};
use owlshift_testkit::git::{GitEnv, seed};
use serde_json::Value;

#[test]
#[ignore = "live: runs `claude -p` on your token for agent runs; set OWLSHIFT_LIVE_CLAUDE=1"]
fn a_real_role_runs_through_the_executor() {
    // A role small enough for a fast model: read the brief, answer in
    // `result.json`.
    let role = format!(
        "You are a test role. Read the brief, a JSON file. Then write the file named by its \
         `result_path`: a JSON object with exactly the fields `format` (the number \
         {RESULT_FORMAT}), `status` (the string \"done\") and `summary` (the brief's \
         `ticket.title`, verbatim). Do nothing else."
    );
    let title = "Heliotrope forty-two";
    let review = ReviewBrief {
        mandates: vec![owlshift_contracts::review::Mandate::Critic],
        pass: std::num::NonZeroU32::MIN,
        diff: None,
    };
    let live = run_live(
        &role,
        Role::Verify,
        PermissionLevel::ReadOnly,
        title,
        Some(review),
    );
    let result = live.result();
    assert_eq!(result.status, Status::Done);
    assert_eq!(result.summary, title);
    assert!(live.report.usage.is_some(), "{:?}", live.report);
    assert!(fs::metadata(&live.report.stdout_log).unwrap().len() > 0);
}

/// Claude Code's bundled skills do not reach a confined run, read-only or
/// write (OWL-333). Without `--disable-slash-commands`, both runs listed the
/// CLI's own skills in `init`, were offered `Skill`, and got `dataviz`'s
/// text when they invoked it. The `init` checks hold whatever the model
/// does; the check on `Skill` calls covers a model that tries anyway. It
/// prints counts only, never a skill's name or text.
#[test]
#[ignore = "live: runs `claude -p` on your token for agent runs; set OWLSHIFT_LIVE_CLAUDE=1"]
fn bundled_skills_do_not_reach_a_confined_run() {
    let role = format!(
        "You are a test role. First invoke the skill named `dataviz` with the `Skill` tool, \
         with no arguments; if no `Skill` tool is offered, look for it once with any tool \
         search you have, then go on. Do not follow a skill's instructions. Then write the file \
         named by the brief's `result_path`: a JSON object with exactly the fields `format` \
         (the number {RESULT_FORMAT}), `status` (the string \"done\") and `summary` (one \
         sentence: whether the skill's text came back, or why not). Do nothing else."
    );
    for level in [PermissionLevel::ReadOnly, PermissionLevel::WriteWorktree] {
        let live = run_live(&role, Role::Build, level, "Invoke a skill", None);
        assert_eq!(live.result().status, Status::Done, "{level:?}");

        let log = fs::read_to_string(&live.report.stdout_log).unwrap();
        let events: Vec<Value> = log
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let inits: Vec<&Value> = events
            .iter()
            .filter(|event| event["type"] == "system" && event["subtype"] == "init")
            .collect();
        assert!(!inits.is_empty(), "{level:?}: no init event");
        for init in &inits {
            let listed = |key: &str| match &init[key] {
                Value::Null => 0,
                Value::Array(items) => items.len(),
                other => panic!("{level:?}: `{key}` is not a list: {other}"),
            };
            let tools = init["tools"].as_array().expect("init lists its tools");
            println!(
                "{level:?}: claude_code_version {}, skills {}, slash_commands {}, tools {}",
                init["claude_code_version"],
                listed("skills"),
                listed("slash_commands"),
                tools.len()
            );
            assert_eq!(listed("skills"), 0, "{level:?}");
            assert_eq!(listed("slash_commands"), 0, "{level:?}");
            assert!(!tools.iter().any(|tool| tool == "Skill"), "{level:?}");
        }

        // A `Skill` call, if the model makes one anyway, must have failed.
        let contents = || {
            events
                .iter()
                .filter_map(|event| event["message"]["content"].as_array())
                .flatten()
        };
        let results: HashMap<&str, &Value> = contents()
            .filter(|item| item["type"] == "tool_result")
            .filter_map(|item| Some((item["tool_use_id"].as_str()?, item)))
            .collect();
        for call in contents().filter(|item| item["type"] == "tool_use" && item["name"] == "Skill")
        {
            let id = call["id"].as_str().unwrap();
            let result = results
                .get(id)
                .unwrap_or_else(|| panic!("{level:?}: a Skill call has no result"));
            assert_eq!(
                result["is_error"], true,
                "{level:?}: a Skill call succeeded"
            );
        }
    }
}

/// One finished live run, and the folder that holds its files.
struct Live {
    _tmp: tempfile::TempDir,
    report: RunReport,
}

impl Live {
    /// The run's validated result; panics when the run did not finish.
    fn result(&self) -> &RunResult {
        let Outcome::Finished { result, .. } = &self.report.outcome else {
            panic!(
                "{:?}\nstderr: {}",
                self.report.outcome,
                fs::read_to_string(&self.report.stderr_log).unwrap_or_default()
            );
        };
        result
    }
}

/// Runs `prompt` as `role` on a fresh one-commit repository through the
/// executor, confined, on `haiku` at effort `low`, with no network.
fn run_live(
    prompt: &str,
    role: Role,
    level: PermissionLevel,
    title: &str,
    review: Option<ReviewBrief>,
) -> Live {
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
        agent: AgentEnv::from_runner(&[], &[]).unwrap(),
        forge_hosts: vec!["github.com".into()],
        timeout: Duration::from_secs(300),
        gate_timeout: Duration::from_secs(300),
    };
    let harness = ClaudeHarness {
        // Found on the PATH.
        program: "claude".into(),
        prompt: prompt.into(),
        model: Some("haiku".into()),
        effort: Some(Effort::Low),
        max_budget_usd: None,
        login: Some(agent_login()),
    };
    let brief = Brief {
        format: Format,
        role,
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
        resolve: Vec::new(),
        checkpoint: None,
        zones: Vec::new(),
        resources: Vec::new(),
        rules: Vec::new(),
        permissions: Permissions {
            level,
            network: false,
            browser: false,
        },
        gate: Vec::new(),
        always_human: Vec::new(),
        delegation: None, asked_by: None,
        gate_failure: None,
        result_refusal: None,
        decisions_refused: false,
        refused_choices: Vec::new(),
        check_failure: None,
        correction: None,
        rebase: None,
        result_path: RelativePath::new("result.json").unwrap(),
        plan: None,
        approval: false,
        findings: None,
        review,
        pull_request_reviews: Vec::new(),
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
    Live { _tmp: tmp, report }
}

/// The token for agent runs, from the system keychain.
fn agent_login() -> Secret {
    Keychain::system()
        .unwrap()
        .read(CLAUDE_AGENT_ACCOUNT)
        .unwrap()
        .expect("no token for agent runs: run `claude setup-token`, then `owlshift init`")
}
