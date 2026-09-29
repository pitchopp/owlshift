//! A tiny real run of `claude -p` on the user's own login: the cheapest
//! model, low effort, one file written. It spends a little of the
//! subscription, so it never runs in CI:
//!
//! ```sh
//! OWLSHIFT_LIVE_CLAUDE=1 cargo test -p owlshift-adapters --test claude_live -- --ignored
//! ```
//!
//! It also fails when the live CLI reports a `claude_code_version` outside
//! [`owlshift_adapters::harness::tested`]: unlike `owlshift doctor`, which
//! only warns, this is the run that actually exercises the CLI, so a silent
//! self-update should not pass quietly.
//!
//! The run's directory carries repository settings whose hooks would leave a
//! marker at start-up, on the prompt and before each tool call. A hook runs
//! whether or not the directory is trusted, so the marker's absence is the
//! live check that the adapter's flags still load no settings file (OWL-53).

use std::path::Path;

use owlshift_adapters::harness::claude::{Billing, Effort, Outcome, Request, command, drive};
use owlshift_adapters::harness::tested;
use owlshift_contracts::Harness;
use owlshift_contracts::brief::{PermissionLevel, Permissions};
use owlshift_contracts::ids::RelativePath;

#[test]
#[ignore = "live: runs `claude -p` on your login; set OWLSHIFT_LIVE_CLAUDE=1"]
fn a_tiny_real_run_completes_on_the_users_login() {
    assert_eq!(
        std::env::var("OWLSHIFT_LIVE_CLAUDE").as_deref(),
        Ok("1"),
        "this test spends subscription usage: set OWLSHIFT_LIVE_CLAUDE=1 to run it"
    );
    let workdir = tempfile::tempdir().unwrap();
    let marker = workdir.path().join("repository-hook-ran");
    let hook = serde_json::json!([{
        "hooks": [{"type": "command", "command": format!("touch '{}'", marker.display())}]
    }]);
    let settings = serde_json::json!({
        "permissions": {"allow": ["Bash", "Write", "Edit"]},
        "hooks": {"SessionStart": hook, "UserPromptSubmit": hook, "PreToolUse": hook},
    });
    std::fs::create_dir(workdir.path().join(".claude")).unwrap();
    for file in ["settings.json", "settings.local.json"] {
        let path = workdir.path().join(".claude").join(file);
        std::fs::write(path, settings.to_string()).unwrap();
    }
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
    let prompt = "Use the Write tool to create the file result.json containing exactly \
                  {\"ok\": true}. Then reply with the single word done.";

    let mut child = command(Path::new("claude"), &request)
        .unwrap()
        .spawn()
        .expect("`claude` is on the PATH");
    let mut lines = 0;
    let run = drive(&mut child, prompt, |_| lines += 1).unwrap();

    // Checked first: a silent CLI self-update is the most likely reason any
    // assertion below would fail, and this is the one that names it instead
    // of leaving a flag- or message-shaped failure to puzzle out.
    let harness_version = run
        .harness_version
        .as_deref()
        .expect("the init event reports claude_code_version");
    assert!(
        tested::is_tested(Harness::Claude, harness_version),
        "the installed Claude Code is {harness_version}, not in the tested list \
         ({:?}); once the contract tests pass on it, add it to \
         crates/owlshift-adapters/src/harness/tested.rs",
        tested::tested_versions(Harness::Claude)
    );

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
    assert!(run.usage.unwrap().output_tokens > 0);
    assert!(
        run.rate_limit.is_some(),
        "every run reports its usage window"
    );
    assert!(
        lines >= 3,
        "init, messages and the final record were streamed"
    );
    let written = std::fs::read_to_string(workdir.path().join("result.json")).unwrap();
    let written: serde_json::Value = serde_json::from_str(&written).unwrap();
    assert_eq!(written, serde_json::json!({"ok": true}));
    assert!(
        !marker.exists(),
        "a hook of the directory's .claude settings ran: the adapter's flags no longer \
         keep a repository's settings out"
    );
}
