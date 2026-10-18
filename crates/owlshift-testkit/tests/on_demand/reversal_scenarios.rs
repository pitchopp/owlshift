//! A reversed approval or residue decision on the bench (OWL-345; architecture
//! section 5, "A reversed decision"): before delivery, the decider's reply to
//! the DECISION comment of the resolver's plan approval or residue decision
//! returns the ticket to the author stage, on the bench of `on_demand.rs`: the
//! Markdown tracker, the fake harness playing every role, the fake GitHub.

use super::design_scenarios::{FORKED, PLAN, REVISED, at_level, designed, reviewed, ruled};
use super::review_scenarios::{ASKS_WHICH_CASE, FORGE, RENAMED, review};
use super::verify_scenarios::found;
use super::*;

/// The resolver's approval of the forked plan, as it decides it under
/// `on-fork`.
const APPROVES: &str = r#"{"format":12,"status":"done","summary":"The fork's choice is the repository's.",
"resolutions":[{"question":"Q1","outcome":"decided","category":"approval",
"decision":"Approve the plan as it is.",
"basis":"The plan's fork keeps Markdown, as the repository's docs are; plain text buys nothing.",
"basis_kind":"judgment","option":"A"}]}"#;

/// The resolver's decision on a Design review residue finding.
const APPLIED: &str = r#"{"format":12,"status":"done","summary":"The plan does not check the word.",
"resolutions":[{"question":"Q1","outcome":"decided","category":"testing",
"decision":"Settle it: change step 1 so its test greps for the word Hello.",
"basis":"Plan step 1's test greps for the line, not the word, as the arbiter confirmed.",
"basis_kind":"fact","option":"A"}]}"#;

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
        let mut limited = self.reply(None, None);
        limited.usage_limit = Some("2026-10-02T15:00:00Z".parse().unwrap());
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

/// A Design review residue decided by the resolver and reversed by the
/// decider (OWL-345): the pass cut short by a usage limit would run again,
/// but the ticket returns to Design first, its review passes kept, and the
/// review follows the Design run as after an answered round.
#[test]
fn a_reversed_design_review_residue_returns_the_ticket_to_design() {
    let bench = Bench::of("standard");
    let replies = vec![
        bench.read(&designed(PLAN)),
        bench.read(&reviewed(&[(
            "major",
            "Step 1's test does not check the word.",
        )])),
        bench.read(&reviewed(&[])),
        bench.read(&ruled(&["confirmed"])),
        bench.read(&designed(PLAN)),
        bench.read(&reviewed(&[(
            "major",
            "Step 1's test still does not check the word.",
        )])),
        bench.read(&reviewed(&[])),
        bench.read(&ruled(&["confirmed"])),
        bench.read(APPLIED),
        bench.read(&designed(REVISED)),
        bench.limited(),
    ];
    let (stopped, printed) = bench.run(replies, None);
    assert!(
        matches!(&stopped, Err(Stop::UsageLimit { .. })),
        "{stopped:?}\n{printed}"
    );
    let record = bench.record();
    assert_eq!(
        (record.state.stage, record.state.review_passes),
        (Stage::DesignReview, 2)
    );
    let passes = record.state.review_passes;
    let kept = &record.questions.decisions[0];
    assert_eq!(kept.asked_by, Some(Role::DesignReview));
    let decision = kept.comment.clone();
    let before = bench.roles().len();

    bench.reply_as("maintainer", &decision, "No: leave the test as it is.");
    // The Design run the reply returns to, then a usage limit at the review:
    // the return itself counts no review pass.
    let replies = vec![bench.read(&designed(REVISED)), bench.limited()];
    let (stopped, printed) = bench.continue_ticket(replies);
    assert!(
        matches!(&stopped, Err(Stop::UsageLimit { .. })),
        "{stopped:?}\n{printed}"
    );
    let record = bench.record();
    assert_eq!(
        (record.state.stage, record.state.review_passes),
        (Stage::DesignReview, passes)
    );
    let replies = vec![
        bench.read(&reviewed(&[])),
        bench.read(&reviewed(&[])),
        bench.built(),
    ];
    let (outcome, printed) = bench.continue_ticket(replies);
    outcome.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    assert_eq!(
        bench.roles()[before..],
        [
            Role::Design,
            Role::DesignReview,
            Role::DesignReview,
            Role::DesignReview,
            Role::Build,
            Role::Verify
        ]
    );
    assert!(follows_reply(
        &bench.briefs()[before],
        "No: leave the test as it is."
    ));
    assert!(bench.record().questions.decisions[0].reversed.is_some());
    let reopened = bench.decisions("reversed");
    assert_eq!(
        (&reopened[0]["from"], &reopened[0]["to"]),
        (&json!("design_review"), &json!("design"))
    );
    assert_eq!(reopened[0]["approval_dropped"], json!(false));
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

/// The resolver's decision on a Verify residue finding.
const SETTLED: &str = r#"{"format":12,"status":"done","summary":"The greeting has no full stop.",
"resolutions":[{"question":"Q1","outcome":"decided","category":"style",
"decision":"Settle it: add the full stop.",
"basis":"Both documents of the repository end their lines with one.",
"basis_kind":"fact","option":"A"}]}"#;

impl Bench {
    /// Runs `owlshift do` through two Verify passes and the resolver's
    /// decision on their residue finding, then through `after`, whose last
    /// reply is a usage limit; returns the id of the decision's DECISION
    /// comment.
    fn verify_residue_decided(&self, after: Vec<Reply>) -> String {
        let mut replies = vec![
            self.reply(Some("Hello"), Some(DONE)),
            self.reviewing(&found(&[("major", "The greeting has no full stop.")])),
            self.reviewing(&ruled(&["confirmed"])),
            self.reply(Some("Hello, again"), Some(DONE)),
            self.reviewing(&found(&[(
                "blocker",
                "The greeting still has no full stop.",
            )])),
            self.reviewing(&ruled(&["confirmed"])),
            self.reviewing(SETTLED),
        ];
        replies.extend(after);
        let (stopped, printed) = self.run(replies, None);
        assert!(
            matches!(&stopped, Err(Stop::UsageLimit { .. })),
            "{stopped:?}\n{printed}"
        );
        let kept = &self.record().questions.decisions[0];
        assert_eq!(kept.asked_by, Some(Role::Verify));
        kept.comment.clone()
    }
}

/// A Verify residue decided by the resolver and reversed by the decider
/// (OWL-345): the Verify pass cut short by a usage limit would run again,
/// but the ticket returns to Build first, whose fix run follows the reply,
/// and Verify reviews its commit.
#[test]
fn a_reversed_verify_residue_returns_the_ticket_to_build() {
    let bench = Bench::verifying();
    let decision = bench.verify_residue_decided(vec![
        bench.reply(Some("Hello, at last"), Some(DONE)),
        bench.limited(),
    ]);
    assert_eq!(bench.record().state.stage, Stage::Verify);
    let before = bench.roles().len();

    bench.reply_as("maintainer", &decision, "No: leave the greeting as it is.");
    let replies = vec![
        bench.reply(Some("Hello, for good"), Some(DONE)),
        bench.reviewing(&found(&[])),
    ];
    let (outcome, printed) = bench.continue_ticket(replies);
    outcome.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    assert_eq!(bench.roles()[before..], [Role::Build, Role::Verify]);
    assert!(follows_reply(
        &bench.briefs()[before],
        "No: leave the greeting as it is."
    ));
    assert!(bench.record().questions.decisions[0].reversed.is_some());
    let reopened = bench.decisions("reversed");
    assert_eq!(
        (&reopened[0]["from"], &reopened[0]["to"]),
        (&json!("verify"), &json!("build"))
    );
    assert_eq!(reopened[0]["approval_dropped"], json!(false));
}

/// The same reply while the fix Build that the decision opened is cut off by
/// a usage limit (OWL-345): the ticket is at Build, where the reply returns
/// it, so nothing moves. `continue` runs the Build with the decision shown
/// as reversed in its brief, and the return is recorded from Build to Build.
#[test]
fn a_reversed_verify_residue_at_build_moves_nothing() {
    let bench = Bench::verifying();
    let decision = bench.verify_residue_decided(vec![bench.limited()]);
    assert_eq!(bench.record().state.stage, Stage::Build);
    let before = bench.roles().len();

    bench.reply_as("maintainer", &decision, "No: leave the greeting as it is.");
    let replies = vec![
        bench.reply(Some("Hello, for good"), Some(DONE)),
        bench.reviewing(&found(&[])),
    ];
    let (outcome, printed) = bench.continue_ticket(replies);
    outcome.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    assert_eq!(bench.roles()[before..], [Role::Build, Role::Verify]);
    assert!(follows_reply(
        &bench.briefs()[before],
        "No: leave the greeting as it is."
    ));
    assert!(bench.record().questions.decisions[0].reversed.is_some());
    let reopened = bench.decisions("reversed");
    assert_eq!(reopened.len(), 1);
    assert_eq!(
        (&reopened[0]["from"], &reopened[0]["to"]),
        (&json!("build"), &json!("build"))
    );
    assert_eq!(reopened[0]["approval_dropped"], json!(false));
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
