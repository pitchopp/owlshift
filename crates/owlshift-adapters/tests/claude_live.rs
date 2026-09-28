//! A tiny real run of `claude -p` on the user's own login: the cheapest
//! model, low effort, one file written. It spends a little of the
//! subscription, so it never runs in CI:
//!
//! ```sh
//! OWLSHIFT_LIVE_CLAUDE=1 cargo test -p owlshift-adapters --test claude_live -- --ignored
//! ```

use std::path::Path;

use owlshift_adapters::harness::claude::{Billing, Effort, Outcome, Request, command, drive};
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
    };
    let prompt = "Use the Write tool to create the file result.json containing exactly \
                  {\"ok\": true}. Then reply with the single word done.";

    let mut child = command(Path::new("claude"), &request)
        .unwrap()
        .spawn()
        .expect("`claude` is on the PATH");
    let mut lines = 0;
    let run = drive(&mut child, prompt, |_| lines += 1).unwrap();

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
    assert!(run.harness_version.is_some());
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
}
