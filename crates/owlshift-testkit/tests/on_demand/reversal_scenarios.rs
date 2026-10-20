//! A reversed plan approval on the bench (OWL-345; architecture section 5, "A
//! reversed decision"): before delivery, the decider's reply to the DECISION
//! comment of the resolver's plan approval returns the ticket to Design, on
//! the bench of `on_demand.rs`: the Markdown tracker, the fake harness
//! playing every role, the fake GitHub.

use super::design_scenarios::{FORKED, at_level, designed, reviewed};
use super::review_scenarios::{ASKS_WHICH_CASE, FORGE, RENAMED, review};
use super::*;

/// The resolver's approval of the forked plan, as it decides it under
/// `on-fork`.
const APPROVES: &str = r#"{"format":14,"status":"done","summary":"The fork's choice is the repository's.",
"resolutions":[{"question":"Q1","outcome":"decided","category":"approval",
"decision":"Approve the plan as it is.",
"basis":"The plan's fork keeps Markdown, as the repository's docs are; plain text buys nothing.",
"basis_kind":"judgment","option":"A"}]}"#;

impl Bench {
    /// `on-fork` plan approval for a bench at the level `all`, on a
    /// `standard` ticket.
    pub(super) fn on_fork(&self) {
        let config = self.remote.checkout.join("owlshift.toml");
        let text = fs::read_to_string(&config).unwrap();
        fs::write(
            &config,
            text.replace("plan_approval = \"never\"", "plan_approval = \"on-fork\""),
        )
        .unwrap();
    }

    /// A run cut off by a usage limit.
    pub(super) fn limited(&self) -> Reply {
        self.limited_until("2026-10-02T15:00:00Z")
    }

    /// A run cut off by a usage limit that resets at `resets`.
    pub(super) fn limited_until(&self, resets: &str) -> Reply {
        let mut limited = self.reply(None, None);
        limited.usage_limit = Some(resets.parse().unwrap());
        limited
    }

    /// Runs `owlshift do` to the Build the resolver's approval of a forked
    /// plan opens, which a usage limit cuts off; returns the id of the
    /// approval's DECISION comment.
    pub(super) fn approved_then_limited(&self) -> String {
        let replies = vec![
            self.read(&designed(FORKED)),
            self.read(&reviewed(&[])),
            self.read(&reviewed(&[])),
            self.read(APPROVES),
            self.limited(),
        ];
        let (stopped, printed) = self.run(replies, None);
        assert!(
            matches!(&stopped, Err(Stop::UsageLimit { .. })),
            "{stopped:?}\n{printed}"
        );
        assert_eq!(
            self.roles(),
            [
                Role::Design,
                Role::DesignReview,
                Role::DesignReview,
                Role::Resolver,
                Role::Build
            ]
        );
        let record = self.record();
        let decision = record.questions.decisions[0].comment.clone();
        assert_eq!(record.plan.unwrap().approval.unwrap().comment, decision);
        decision
    }
}

/// Whether `brief`'s thread holds a decision shown as reversed and the
/// comment `reply`.
fn follows_reply(brief: &Brief, reply: &str) -> bool {
    let reversed = brief
        .thread
        .iter()
        .any(|entry| matches!(entry, ThreadEntry::Decision { reversed: true, .. }));
    let replied = brief
        .thread
        .iter()
        .any(|entry| matches!(entry, ThreadEntry::Comment { body, .. } if body == reply));
    reversed && replied
}

/// OWL-345's acceptance, a plan approval reversed: under `on-fork` the
/// resolver approved a forked plan and the Build began; the decider replies
/// to its DECISION comment. The next command drops the kept approval and
/// returns the ticket to Design, the reply followed as the decision
/// reversed; the new plan is reviewed and its approval asked again, the
/// resolver's second decision then opening the Build.
#[test]
fn a_reversed_plan_approval_returns_the_ticket_to_design() {
    let bench = at_level("standard", "all");
    bench.on_fork();
    let decision = bench.approved_then_limited();
    let before = bench.roles().len();

    bench.reply_as("maintainer", &decision, "No: plain text.");
    let (stopped, printed) = bench.continue_ticket(vec![bench.limited()]);
    assert!(
        matches!(&stopped, Err(Stop::UsageLimit { .. })),
        "{stopped:?}\n{printed}"
    );
    // Design runs, not Build, with the approval gone from the plan.
    assert_eq!(bench.roles()[before..], [Role::Design]);
    let record = bench.record();
    assert_eq!(
        (record.state.stage, record.state.waiting),
        (Stage::Design, None)
    );
    assert!(record.plan.unwrap().approval.is_none());
    assert!(record.questions.decisions[0].reversed.is_some());
    assert!(follows_reply(
        &bench.last_of(Role::Design),
        "No: plain text."
    ));
    let reopened = bench.decisions("reversed");
    assert_eq!(reopened.len(), 1, "{reopened:?}");
    assert_eq!(
        (
            &reopened[0]["from"],
            &reopened[0]["to"],
            &reopened[0]["approval_dropped"]
        ),
        (&json!("build"), &json!("design"), &json!(true))
    );

    // The new plan is reviewed and approved again, by a second decision.
    let replies = vec![
        bench.read(&designed(FORKED)),
        bench.read(&reviewed(&[])),
        bench.read(&reviewed(&[])),
        bench.read(APPROVES),
        bench.built(),
    ];
    let (outcome, printed) = bench.continue_ticket(replies);
    outcome.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    assert_eq!(
        bench.roles()[before + 1..],
        [
            Role::Design,
            Role::DesignReview,
            Role::DesignReview,
            Role::Resolver,
            Role::Build,
            Role::Verify
        ]
    );
    let record = bench.record();
    assert_eq!(record.questions.decisions.len(), 2);
    assert_eq!(
        record.plan.unwrap().approval.unwrap().comment,
        record.questions.decisions[1].comment
    );
    assert_eq!(bench.decisions("reversed").len(), 1, "returned once");
}

/// A stranger's reply returns nothing (OWL-345): the next run is the Build the
/// usage limit cut off, and the approval stays.
#[test]
fn a_strangers_reply_does_not_return_the_ticket() {
    let bench = at_level("standard", "all");
    bench.on_fork();
    let decision = bench.approved_then_limited();
    let before = bench.roles().len();

    bench.reply_as("reviewer", &decision, "No: plain text.");
    let (stopped, printed) = bench.continue_ticket(vec![bench.limited()]);
    assert!(
        matches!(&stopped, Err(Stop::UsageLimit { .. })),
        "{stopped:?}\n{printed}"
    );
    assert_eq!(bench.roles()[before..], [Role::Build]);
    assert!(bench.decisions("reversed").is_empty());
    assert!(bench.record().plan.unwrap().approval.is_some());
}

/// The decider's reply still inside the quiet window returns nothing
/// (OWL-345), and a Build that runs to completion and delivers leaves it
/// unfollowed, so the correction run keeps it.
#[test]
fn a_reply_inside_the_quiet_window_survives_a_completed_build() {
    let bench = at_level("standard", "all");
    bench.on_fork();
    let decision = bench.approved_then_limited();
    let before = bench.roles().len();

    bench.reply_as("maintainer", &decision, "No: plain text.");
    bench.waited.set(SignedDuration::ZERO);
    let (outcome, printed) = bench.continue_ticket(vec![bench.built()]);
    outcome.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    assert_eq!(bench.roles()[before..], [Role::Build, Role::Verify]);
    let record = bench.record();
    assert!(record.questions.decisions[0].reversed.is_none());
    assert!(record.plan.unwrap().approval.is_some());
    assert!(bench.decisions("reversed").is_empty());
}

/// A review loop is after delivery, with its pull request open, and the core
/// has no way back from Design to Watch (OWL-345): a reply reversing the
/// plan's approval then returns the ticket nowhere. Its Build runs as the
/// loop's, the approval stays, and the reply is followed as it was before.
#[test]
fn a_reversed_approval_in_a_review_loop_leaves_the_ticket_at_build() {
    let bench = Bench::with("maintainer", "assignee = \"maintainer\"\n", FORGE);
    bench.add_labels(&["variant:standard"]);
    bench.policy("resolver_delegation = \"all\"");
    bench.on_fork();
    let replies = vec![
        bench.read(&designed(FORKED)),
        bench.read(&reviewed(&[])),
        bench.read(&reviewed(&[])),
        bench.read(APPROVES),
        bench.built(),
    ];
    let (outcome, printed) = bench.run(replies, None);
    outcome.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    let decision = bench.record().questions.decisions[0].comment.clone();

    // The decider's review sends the ticket back to Build, which asks.
    bench.reviews_on_github(vec![review(7_000_000_045, "ann-dev", "Use capitals.", &[])]);
    let asks = vec![
        bench.reply(None, Some(ASKS_WHICH_CASE)),
        bench.reply(None, Some(&passed_on(&["Q1"]))),
    ];
    let (asked, printed) = bench.continue_ticket(asks);
    assert!(
        matches!(&asked, Err(Stop::NeedsInput { posted: Ok(_), .. })),
        "{asked:?}\n{printed}"
    );
    bench.reply_as("maintainer", &decision, "No: plain text.");
    bench.answer("Q1: Its first word. Go.\n");
    let before = bench.roles().len();

    let replies = vec![
        bench.reply(None, Some(&check(&[("Q1", "answered", "First word.")]))),
        bench.reply(Some("Hello there"), Some(RENAMED)),
    ];
    let (outcome, printed) = bench.continue_ticket(replies);
    outcome.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    assert_eq!(
        bench.roles()[before..],
        [Role::AnswerCheck, Role::Build, Role::Verify]
    );
    assert!(bench.decisions("reversed").is_empty());
    let record = bench.record();
    assert!(record.plan.unwrap().approval.is_some());
    assert!(record.questions.decisions[0].reversed.is_some());
    assert_eq!(state_of(&bench).status(), Status::Active(Stage::Watch));
}
