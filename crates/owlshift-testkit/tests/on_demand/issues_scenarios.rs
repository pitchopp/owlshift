//! The drain on GitHub Issues (OWL-536): `owlshift watch --drain` on the
//! bench of `on_demand.rs`, its tracker the real GitHub Issues adapter over
//! the Issues fake (`owlshift_testkit::issues`), whose answers are shaped
//! from the exchanges recorded live on the sandbox.

use owlshift_testkit::issues::Account;

use super::drain_scenarios::{dispatched, done_for, waits};
use super::*;

impl Bench {
    /// A drain bench whose tickets are the issues of `issues`: the project's
    /// tracker is GitHub Issues, its stages labels, admitting with the label
    /// `agent:ready` by `admitter`'s numeric id alone, committed on `main`
    /// as the drain reads it at its base.
    fn on_github_issues(admitter: &Account, issues: &FakeIssues) -> Self {
        let bench = Bench::draining("maintainer");
        let checkout = &bench.remote.checkout;
        let path = checkout.join("owlshift.toml");
        let text = fs::read_to_string(&path).unwrap();
        let text: Vec<String> = text
            .lines()
            .map(|line| match line {
                "kind = \"markdown\"" => "kind = \"github\"".to_owned(),
                "admit = { label = \"agent\" }" => "admit = { label = \"agent:ready\" }".to_owned(),
                "admitters = [\"maintainer\"]" => format!("admitters = [\"{}\"]", admitter.id),
                states if states.starts_with("states = ") => "states = { ready = \"agent:ready\", \
                     working = \"agent:working\", needs_input = \"agent:needs-input\", review = \
                     \"agent:review\" }"
                    .to_owned(),
                other => other.to_owned(),
            })
            .collect();
        fs::write(&path, text.join("\n") + "\n").unwrap();
        for args in [
            &[
                "commit",
                "--quiet",
                "--all",
                "-m",
                "Track the project's issues",
            ][..],
            &["push", "--quiet", "origin", "HEAD:refs/heads/main"][..],
        ] {
            bench.env.run(checkout, args).unwrap();
        }
        *bench.issues.borrow_mut() = Some(issues.clone());
        bench
    }
}

/// #1, labelled by the admitter, is taken in and built, its pull request's
/// body ending with `Closes #1`, and its follow-up is filed as an
/// `agent:proposed` issue blocked by #1; #2, blocked by #1, waits until #1's
/// pull request merges, which closes #1 as completed, then is built; #3,
/// labelled by an account that is no admitter, is never taken in.
#[test]
fn a_drain_on_github_issues() {
    let maintainer = Account::new("maintainer", 1);
    let stranger = Account::new("stranger", 7);
    let start = "2026-10-02T08:00:00Z".parse().unwrap();
    let issues = FakeIssues::new(REPO, maintainer.clone(), start);
    let by = |account| [("agent:ready", account)];
    let decider = Some(&maintainer);
    let greeting = "Add GREETING.md saying Hello.";
    issues.open(
        1,
        "Add a greeting",
        greeting,
        &maintainer,
        decider,
        &by(&maintainer),
    );
    issues.open(
        2,
        "Greet again",
        "Greet again.",
        &maintainer,
        decider,
        &by(&maintainer),
    );
    issues.block(1, 2);
    issues.open(
        3,
        "Greet all",
        "Greet the world.",
        &stranger,
        decider,
        &by(&stranger),
    );
    let bench = Bench::on_github_issues(&maintainer, &issues);

    let mut proposing: Value = serde_json::from_str(&done_for("1")).unwrap();
    proposing["followups"] = json!([{
        "title": "Translate the greeting", "why": "Readers abroad.", "source": "agent",
        "done_when": "Translated.", "blocked_by_parent": true,
    }]);
    let intake = || bench.reply(None, Some(&intake_result("done", None, "")));
    let replies = vec![
        intake(),
        intake(),
        bench.reply(Some("Hello"), Some(&proposing.to_string())),
        bench.reply(Some("Hello again"), Some(&done_for("2"))),
    ];
    let closed = RefCell::new(None);
    let (outcomes, lines, printed) = bench.drain(replies, 6, |pass| {
        if pass == 3 {
            bench.merge("1");
            let body = bench.github.created.lock().unwrap()[0]["body"].clone();
            *closed.borrow_mut() = issues.merged(body.as_str().unwrap());
        }
        if pass == 4 {
            bench.reopened();
        }
    });
    let delivered = outcomes.iter().filter(|outcome| outcome.is_ok()).count();
    assert_eq!(delivered, 2, "{outcomes:?}\n{lines}\n{printed}");
    let events = bench.events();
    assert_eq!(dispatched(&events), ["1", "2"], "{lines}");

    // The delivery closes its issue: its body ends with the closing line,
    // and GitHub closed #1 as completed when it merged.
    let created = bench.github.created.lock().unwrap().clone();
    let body = created[0]["body"].as_str().unwrap();
    assert!(body.ends_with("\n\nCloses #1"), "{body}");
    assert_eq!(*closed.borrow(), Some(1));
    assert_eq!(issues.issue(1).state_reason.as_deref(), Some("completed"));
    assert!(created[1]["body"].as_str().unwrap().ends_with("Closes #2"));

    // #2 waited for #1, by GitHub's relation.
    assert!(
        waits(&events).contains(&("2".to_owned(), json!("blocked_by"), json!(["1"]))),
        "{:?}",
        waits(&events)
    );

    // #3's label is a stranger's: no gesture, no intake run.
    let unadmitted: Vec<&Event> = events
        .iter()
        .filter(|e| e.kind == EventKind::Warning && e.data["what"] == json!("not_admitted"))
        .collect();
    assert_eq!(unadmitted.len(), 1, "{unadmitted:?}");
    assert_eq!(unadmitted[0].ticket.as_ref().unwrap().as_str(), "3");
    assert_eq!(unadmitted[0].data["account"], json!("7"));
    assert_eq!(
        bench.runs(),
        [
            (Role::Intake, "1".to_owned()),
            (Role::Intake, "2".to_owned()),
            (Role::Build, "1".to_owned()),
            (Role::Verify, "1".to_owned()),
            (Role::Build, "2".to_owned()),
            (Role::Verify, "2".to_owned()),
        ]
    );

    // The follow-up is an issue with the proposals' label, blocked by #1,
    // whose text names #1 as the drain's convention reads it.
    let proposal = issues.issue(4);
    assert_eq!(proposal.title, "Translate the greeting");
    assert_eq!(proposal.labels, ["agent:proposed"]);
    assert_eq!(proposal.blocked_by, [1]);
    assert!(
        proposal
            .body
            .contains("**Blocked by** #1, whose code it needs."),
        "{}",
        proposal.body
    );
    assert_eq!(issues.numbers(), [1, 2, 3, 4]);
}
