//! Verify on the bench (OWL-304, build plan "P5 · Quality pipeline", part
//! G): once a Build's gate passed and the thread was read again, the
//! variant's roster reviews the Build's diff before the push, on the bench
//! of `on_demand.rs`: the Markdown tracker, the fake harness playing every
//! run, the verify seats' included, the fake GitHub.

use owlshift_contracts::result::FollowupSource;
use owlshift_contracts::review::{FindingOutcome, FindingsMode, Mandate, ResponseOutcome};
use owlshift_core::vocab::Harness as HarnessKind;

use super::*;

/// A Verify run's `done` with a finding per (severity, claim).
fn found(findings: &[(&str, &str)]) -> String {
    let findings: Vec<Value> = findings
        .iter()
        .enumerate()
        .map(|(n, (severity, claim))| {
            json!({
                "id": format!("F{}", n + 1),
                "severity": severity,
                "claim": claim,
                "evidence": "GREETING.md:1",
                "settle": format!("Settle: {claim}"),
            })
        })
        .collect();
    json!({ "format": 12, "status": "done", "summary": "Reviewed.", "findings": findings })
        .to_string()
}

/// The arbiter's `done` with a ruling per outcome, on F1..Fn.
fn ruled(outcomes: &[&str]) -> String {
    let rulings: Vec<Value> = outcomes
        .iter()
        .enumerate()
        .map(|(n, outcome)| {
            json!({
                "finding": format!("F{}", n + 1),
                "outcome": outcome,
                "reason": format!("Checked F{} at GREETING.md:1.", n + 1),
            })
        })
        .collect();
    json!({ "format": 12, "status": "done", "summary": "Ruled.", "rulings": rulings }).to_string()
}

const FAILED_REVIEW: &str =
    r#"{"format":12,"status":"failed","summary":"Could not read the diff."}"#;

impl Bench {
    /// A bench whose verify seats play from its reply queue, with Codex
    /// given when `codex`.
    fn verifying(codex: bool) -> Self {
        let bench = Bench::new(true);
        bench.verify_plays.set(true);
        bench.codex.set(codex);
        bench
    }

    /// A read-only run's reply: its result alone.
    fn reviewing(&self, result: &str) -> Reply {
        self.reply(None, Some(result))
    }

    /// The role of every run, oldest first.
    fn run_roles(&self) -> Vec<Role> {
        self.briefs().iter().map(|brief| brief.role).collect()
    }

    /// The `review` decision events of `kind`: `pass`, `ruling`, `residue`.
    fn reviews(&self, kind: &str) -> Vec<Value> {
        data_of(&self.events(), EventKind::Decision, "review")
            .into_iter()
            .filter(|data| data["review"] == kind)
            .collect()
    }

    /// The head of the ticket's branch in Owlshift's checkout.
    fn branch_head(&self) -> String {
        let out = self
            .env
            .run(
                &self.dirs().checkout(),
                &["rev-parse", "refs/heads/owlshift/demo-1"],
            )
            .unwrap();
        String::from_utf8(out).unwrap().trim().to_owned()
    }
}

/// S9's Verify half on a `trivial` ticket: after its Build, the critic,
/// alone so holding conformance, standing in on Claude Code, reviews the
/// diff from the commit the Build started from, which the runner wrote
/// beside the brief; the clean pass is kept with its reviewer, and the
/// DELIVERY report names it.
#[test]
fn a_trivial_ticket_is_verified_before_its_pull_request_opens() {
    let bench = Bench::verifying(false);
    let replies = vec![
        bench.reply(Some("Hello"), Some(DONE)),
        bench.reviewing(&found(&[])),
    ];
    let (outcome, printed) = bench.run(replies, None);
    outcome.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    assert_eq!(bench.run_roles(), [Role::Build, Role::Verify]);
    let verify = bench.last_of(Role::Verify);
    assert_eq!(verify.permissions.level, PermissionLevel::ReadOnly);
    assert!(!verify.permissions.network);
    assert!(verify.plan.is_none(), "a trivial ticket has no plan");
    let review = verify.review.expect("a review");
    assert_eq!(review.mandates, [Mandate::Critic, Mandate::Conformance]);
    assert_eq!(review.pass.get(), 1);
    let diff = review.diff.expect("the diff");
    let dispatched = data_of(&bench.events(), EventKind::Dispatch, "base_commit");
    assert_eq!(json!(diff.base), dispatched[0]["base_commit"]);
    assert_eq!(diff.path.as_str(), ".owlshift/run/diff.patch");
    let head = bench.remote_branch().expect("pushed");
    let expected = bench
        .env
        .run(
            &bench.dirs().checkout(),
            &[
                "diff",
                "--no-ext-diff",
                "--no-textconv",
                "--no-color",
                &diff.base,
                &head,
            ],
        )
        .unwrap();
    assert!(String::from_utf8_lossy(&expected).contains("+Hello, reader."));
    assert_eq!(
        fs::read(bench.worktree().join(diff.path.as_str())).unwrap(),
        expected
    );

    let kept = bench.record().review.expect("review.json");
    assert!(kept.build.is_none(), "dropped once delivered");
    assert_eq!(kept.passes.len(), 1);
    let pass = &kept.passes[0];
    assert_eq!((pass.stage, pass.pass.get()), (Stage::Verify, 1));
    assert!(pass.findings.is_empty());
    assert_eq!(pass.reviewers.len(), 1);
    assert!(pass.reviewers[0].stand_in && pass.reviewers[0].same_model);
    let passes = bench.reviews("pass");
    assert_eq!(passes.len(), 1);
    assert_eq!(
        (
            &passes[0]["stage"],
            &passes[0]["outcome"],
            &passes[0]["next"]
        ),
        (&json!("verify"), &json!("passed"), &json!("deliver"))
    );
    let report = bench.comments().pop().unwrap();
    for text in [
        "**Verify, 1 pass.** Reviewers of the last pass:\n- critic and conformance on Claude \
         Code (its default model, the Build's own model: no other is configured)",
        "The critic ran on Claude Code, standing in for Codex",
    ] {
        assert!(report.contains(text), "{text}\n---\n{report}");
    }
}

/// OWL-321: `[roles.verify]` is accepted, and its file reaches the brief of
/// the Verify reviewer after `stack.rules`, and no other role's.
#[test]
fn the_verify_roles_rules_reach_the_verify_brief_alone() {
    let bench = Bench::verifying(false);
    let checkout = &bench.remote.checkout;
    fs::create_dir_all(checkout.join("docs/roles")).unwrap();
    fs::write(
        checkout.join("docs/roles/verify.md"),
        "Read the diff twice.\n",
    )
    .unwrap();
    let mut config = fs::read_to_string(checkout.join("owlshift.toml")).unwrap();
    config.push_str("[roles.verify]\nrules = [\"docs/roles/verify.md\"]\n");
    fs::write(checkout.join("owlshift.toml"), config).unwrap();
    bench
        .env
        .run(checkout, &["add", "owlshift.toml", "docs/roles"])
        .unwrap();
    bench
        .env
        .run(
            checkout,
            &["commit", "--quiet", "-m", "Name the verify rules"],
        )
        .unwrap();
    bench
        .env
        .run(checkout, &["push", "--quiet", "origin", "main"])
        .unwrap();
    let replies = vec![
        bench.reply(Some("Hello"), Some(DONE)),
        bench.reviewing(&found(&[])),
    ];
    let (outcome, printed) = bench.run(replies, None);
    outcome.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    assert_eq!(bench.run_roles(), [Role::Build, Role::Verify]);
    let sources = |role| -> Vec<String> {
        bench
            .last_of(role)
            .rules
            .iter()
            .map(|rule| rule.source.clone())
            .collect()
    };
    assert_eq!(sources(Role::Build), ["AGENTS.md"]);
    assert_eq!(sources(Role::Verify), ["AGENTS.md", "docs/roles/verify.md"]);
    assert_eq!(
        bench.last_of(Role::Verify).rules[1].text,
        "Read the diff twice.\n"
    );
}

/// With Codex, the critic runs on it on the standard tier; the arbiter,
/// given the diff, confirms one finding and rejects the other; the Verify
/// fix run gets the confirmed one alone, without the diff, and the second
/// pass is clean. The report lists the rejected finding with its reason.
#[test]
fn the_arbiter_confirms_a_finding_the_verify_fix_run_fixes_and_rejects_another() {
    let bench = Bench::verifying(true);
    let replies = vec![
        bench.reply(Some("Hello"), Some(DONE)),
        bench.reviewing(&found(&[
            ("major", "The greeting has no full stop."),
            ("major", "GREETING.md needs a licence header."),
        ])),
        bench.reviewing(&ruled(&["confirmed", "rejected"])),
        bench.reply(Some("Hello, again"), Some(DONE)),
        bench.reviewing(&found(&[])),
    ];
    let (outcome, printed) = bench.run(replies, None);
    outcome.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    assert_eq!(
        bench.run_roles(),
        [
            Role::Build,
            Role::Verify,
            Role::Arbiter,
            Role::Build,
            Role::Verify
        ]
    );
    let routed = bench.routed();
    assert_eq!(
        (routed[1].harness, routed[1].tier),
        (HarnessKind::Codex, Tier::Standard)
    );
    let briefs = bench.briefs();
    let ruling = briefs[2].findings.as_ref().unwrap();
    assert_eq!(
        (ruling.mode, ruling.findings.len()),
        (FindingsMode::Rule, 2)
    );
    assert_eq!(
        ruling.diff.as_ref().map(|diff| &diff.base),
        briefs[1]
            .review
            .as_ref()
            .unwrap()
            .diff
            .as_ref()
            .map(|d| &d.base)
    );
    let fix = briefs[3].findings.as_ref().expect("the confirmed finding");
    assert_eq!(fix.mode, FindingsMode::Fix);
    let ids: Vec<&str> = fix.findings.iter().map(|f| f.id.as_str()).collect();
    assert_eq!(ids, ["F1"]);
    assert!(fix.diff.is_none());
    assert_eq!(briefs[4].review.as_ref().unwrap().pass.get(), 2);

    let review = bench.record().review.unwrap();
    let first = &review.passes[0].findings;
    assert_eq!(first[0].outcome, Some(FindingOutcome::Fixed));
    assert_eq!(first[1].outcome, Some(FindingOutcome::Rejected));
    assert_eq!(review.passes[0].reviewers[0].harness, HarnessKind::Codex);
    assert!(review.passes[1].findings.is_empty());
    let ruling = &bench.reviews("ruling")[0];
    assert_eq!(ruling["stage"], "verify");
    assert_eq!(
        (&ruling["confirmed"], &ruling["rejected"]),
        (&json!(["F1"]), &json!(["F2"]))
    );
    let next: Vec<Value> = bench
        .reviews("pass")
        .iter()
        .map(|pass| pass["next"].clone())
        .collect();
    assert_eq!(next, [json!("build"), json!("deliver")]);
    let report = bench.comments().pop().unwrap();
    for text in [
        "**Verify, 2 passes.** Reviewers of the last pass:\n- critic and conformance on Codex \
         (its default model)",
        "- Pass 1, F2, major, critic and conformance on Codex: GREETING.md needs a licence \
         header. Rejected: Checked F2 at GREETING.md:1.",
    ] {
        assert!(report.contains(text), "{text}\n---\n{report}");
    }
    assert!(!report.contains("standing in"), "{report}");
    let pushed = bench
        .env
        .run(
            &bench.remote.bare,
            &["show", &format!("{BRANCH}:GREETING.md")],
        )
        .unwrap();
    assert_eq!(
        String::from_utf8(pushed).unwrap(),
        "Hello, again, reader.\n"
    );
}

/// In author mode the Verify fix run answers every finding: a result that
/// leaves one unanswered is refused and told to the next run, whose
/// rejection is kept with its reason and listed in the report.
#[test]
fn the_author_answers_every_verify_finding_and_its_rejection_is_kept() {
    let bench = Bench::verifying(false);
    let config = bench.remote.checkout.join("owlshift.toml");
    let text = fs::read_to_string(&config).unwrap();
    fs::write(
        &config,
        text.replace(
            "plan_approval = \"never\"",
            "plan_approval = \"never\"\nfindings = \"author\"",
        ),
    )
    .unwrap();
    let answering = DONE.replace(
        r#""pr":"#,
        r#""finding_responses":[
            {"finding":"F1","outcome":"fixed","reason":"Added the full stop."},
            {"finding":"F2","outcome":"rejected","reason":"No document of the repository has one."}],
        "pr":"#,
    );
    let replies = vec![
        bench.reply(Some("Hello"), Some(DONE)),
        bench.reviewing(&found(&[
            ("major", "The greeting has no full stop."),
            ("minor", "GREETING.md could open with a heading."),
        ])),
        // No answer to the findings: refused.
        bench.reply(None, Some(DONE)),
        bench.reply(Some("Hello, again"), Some(&answering)),
        bench.reviewing(&found(&[])),
    ];
    let (outcome, printed) = bench.run(replies, None);
    outcome.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    let briefs = bench.briefs();
    assert!(!bench.run_roles().contains(&Role::Arbiter));
    let answer = briefs[2].findings.as_ref().unwrap();
    assert_eq!(
        (answer.mode, answer.findings.len()),
        (FindingsMode::Answer, 2)
    );
    let told = briefs[3].result_refusal.as_deref().unwrap_or_default();
    assert!(told.contains("finding_responses"), "{told}");
    let review = bench.record().review.unwrap();
    let kept = &review.passes[0].findings;
    assert_eq!(
        kept[0].response.as_ref().map(|r| r.outcome),
        Some(ResponseOutcome::Fixed)
    );
    assert_eq!(kept[0].outcome, Some(FindingOutcome::Fixed));
    assert_eq!(kept[1].outcome, Some(FindingOutcome::Rejected));
    let report = bench.comments().pop().unwrap();
    assert!(
        report.contains("Rejected: No document of the repository has one."),
        "{report}"
    );
}

/// The residue: after the last pass, a `blocker` goes to the resolver first
/// (OWL-342), told the finding, its ruling and the diff, then, passed on, to
/// the decider in a round, and a `major` off every criterion becomes a
/// follow-up of the delivery. After the answer, a Verify fix run with it in
/// its thread and one more pass, a last one, clean; the follow-up is filed
/// with the reviewer as its source and listed in the report.
#[test]
fn a_blocker_two_passes_left_is_asked_and_another_finding_is_proposed() {
    let bench = Bench::verifying(false);
    bench.inbox.borrow_mut().on = true;
    let replies = vec![
        bench.reply(Some("Hello"), Some(DONE)),
        bench.reviewing(&found(&[("major", "The greeting has no full stop.")])),
        bench.reviewing(&ruled(&["confirmed"])),
        bench.reply(Some("Hello, again"), Some(DONE)),
        bench.reviewing(&found(&[
            ("blocker", "The greeting still has no full stop."),
            ("major", "GREETING.md could open with a heading."),
        ])),
        bench.reviewing(&ruled(&["confirmed", "confirmed"])),
        bench.reviewing(&passed_on(&["Q1"])),
    ];
    let (outcome, printed) = bench.run(replies, None);
    let stop = outcome.expect_err("the blocker goes to the decider");
    let Stop::NeedsInput {
        questions, posted, ..
    } = &stop
    else {
        panic!("{stop:?}\n{printed}");
    };
    assert_eq!(posted.as_ref().map(|round| round.get()), Ok(1));
    assert_eq!(questions.len(), 1);
    let question = &questions[0];
    assert_eq!(question.category, "review_finding");
    assert!(
        question
            .text
            .starts_with("The greeting still has no full stop.")
    );
    assert!(question.text.ends_with("How should the change settle it?"));
    assert_eq!(question.options[1].label, "Leave the change as it is");
    assert!(
        question
            .context
            .contains("Finding F1 of verify pass 2, a blocker")
    );
    let resolver = bench.last_of(Role::Resolver);
    let asked = resolver.asked_by.as_ref().unwrap();
    assert_eq!(asked.role, Role::Verify);
    let diff = asked.diff.as_ref().expect("the branch's diff");
    assert_eq!(diff.path.as_str(), ".owlshift/run/diff.patch");
    assert_eq!(resolver.resolve[0].finding.as_ref().unwrap().as_str(), "F1");
    let residue = &bench.reviews("residue")[0];
    assert_eq!(
        (&residue["asked"], &residue["proposed"]),
        (&json!(["F1"]), &json!(["F2"]))
    );
    let record = bench.record();
    assert_eq!(
        (
            record.state.stage,
            record.state.waiting,
            record.state.review_passes
        ),
        (Stage::Build, Some(Waiting::NeedsInput), 2)
    );
    let review = record.review.unwrap();
    let second = &review.passes[1].findings;
    assert_eq!(second[0].outcome, Some(FindingOutcome::Asked));
    assert_eq!(second[1].outcome, Some(FindingOutcome::Proposed));
    let followups = review.build.expect("the Build under review").followups;
    assert_eq!(followups.len(), 1);
    assert_eq!(followups[0].source, FollowupSource::Reviewer);
    assert_eq!(followups[0].title, "GREETING.md could open with a heading.");

    bench.answer("Q1: add the full stop. Go.");
    let replies = vec![
        bench.reviewing(&check(&[("Q1", "answered", "Add the full stop.")])),
        bench.reply(Some("Hello, at last."), Some(DONE)),
        bench.reviewing(&found(&[])),
    ];
    let (outcome, printed) = bench.continue_ticket(replies);
    outcome.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    let fix = bench.last_of(Role::Build);
    assert!(fix.findings.is_none(), "the answers are in the thread");
    assert_eq!(latest_ask(&fix), ["Q1"]);
    assert_eq!(
        bench.last_of(Role::Verify).review.unwrap().pass.get(),
        3,
        "the pass the round buys"
    );
    let passes: Vec<u32> = bench
        .record()
        .review
        .unwrap()
        .passes
        .iter()
        .map(|pass| pass.pass.get())
        .collect();
    assert_eq!(passes, [1, 2, 3]);
    let filed = bench.inbox.borrow().tickets.clone();
    assert_eq!(filed.len(), 1, "{filed:?}");
    assert_eq!(filed[0].1, "GREETING.md could open with a heading.");
    assert!(filed[0].2.contains("reviewer"), "{}", filed[0].2);
    let report = bench.comments().pop().unwrap();
    for text in [
        "**Verify, 3 passes.**",
        "**Findings proposed as follow-ups,** listed in what remains:\n- Pass 2, F2, major, \
         critic and conformance on Claude Code, standing in for Codex",
        &format!(
            "Proposed follow-up **GREETING.md could open with a heading.**, filed as {} for \
             triage",
            filed[0].0
        ),
    ] {
        assert!(report.contains(text), "{text}\n---\n{report}");
    }
}

/// A last pass proposes a follow-up, then the push is refused. `continue`
/// runs that pass again in place of its record; clean this time, it takes
/// back its follow-up, so the delivery files none.
#[test]
fn a_pass_run_again_after_a_failed_delivery_takes_back_its_proposals() {
    let bench = Bench::verifying(false);
    bench.inbox.borrow_mut().on = true;
    // Someone else's commit is already on the ticket's branch: the push is
    // refused.
    let checkout = &bench.remote.checkout;
    for args in [
        &["switch", "--quiet", "-c", BRANCH][..],
        &["commit", "--quiet", "--allow-empty", "-m", "Other"],
        &["push", "--quiet", "origin", BRANCH],
        &["switch", "--quiet", "main"],
    ] {
        bench.env.run(checkout, args).unwrap();
    }
    let replies = vec![
        bench.reply(Some("Hello"), Some(DONE)),
        bench.reviewing(&found(&[("major", "The greeting has no full stop.")])),
        bench.reviewing(&ruled(&["confirmed"])),
        bench.reply(Some("Hello, again"), Some(DONE)),
        bench.reviewing(&found(&[(
            "major",
            "GREETING.md could open with a heading.",
        )])),
        bench.reviewing(&ruled(&["confirmed"])),
    ];
    let (outcome, printed) = bench.run(replies, None);
    assert!(
        matches!(&outcome, Err(Stop::Delivery(why)) if why.contains("rejected")),
        "{outcome:?}\n{printed}"
    );
    let review = bench.record().review.unwrap();
    assert_eq!(
        review.passes[1].findings[0].outcome,
        Some(FindingOutcome::Proposed)
    );
    assert_eq!(review.build.unwrap().followups.len(), 1);

    bench
        .env
        .run(checkout, &["push", "--quiet", "origin", "--delete", BRANCH])
        .unwrap();
    let (outcome, printed) = bench.continue_ticket(vec![bench.reviewing(&found(&[]))]);
    outcome.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    let passes: Vec<(Stage, u32, usize)> = bench
        .record()
        .review
        .unwrap()
        .passes
        .iter()
        .map(|pass| (pass.stage, pass.pass.get(), pass.findings.len()))
        .collect();
    assert_eq!(passes, [(Stage::Verify, 1, 1), (Stage::Verify, 2, 0)]);
    assert!(bench.inbox.borrow().tickets.is_empty(), "nothing filed");
    let report = bench.comments().pop().unwrap();
    assert!(report.starts_with("[owlshift] DELIVERY"), "{report}");
    assert!(!report.contains("Proposed follow-up"), "{report}");
}

/// A usage limit cuts a Verify pass short: the Build Verify reviews is
/// kept, and `continue` runs the pass again on it alone, then delivers its
/// commit, with no Build run (part F's rule for Design review).
#[test]
fn a_verify_pass_cut_short_runs_again_under_continue_and_delivers() {
    let bench = Bench::verifying(false);
    let mut limited = bench.reply(None, None);
    limited.usage_limit = Some("2026-10-02T15:00:00Z".parse().unwrap());
    let (outcome, printed) = bench.run(vec![bench.reply(Some("Hello"), Some(DONE)), limited], None);
    assert!(
        matches!(outcome, Err(Stop::UsageLimit { .. })),
        "{outcome:?}\n{printed}"
    );
    let record = bench.record();
    assert_eq!(
        (record.state.stage, record.state.waiting),
        (Stage::Verify, None)
    );
    let built = record
        .review
        .unwrap()
        .build
        .expect("the Build under review");
    assert_eq!(built.commit, bench.branch_head());
    assert!(bench.remote_branch().is_none(), "nothing pushed");

    let (outcome, printed) = bench.continue_ticket(vec![bench.reviewing(&found(&[]))]);
    outcome.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    assert_eq!(bench.run_roles(), [Role::Build, Role::Verify, Role::Verify]);
    assert_eq!(bench.remote_branch(), Some(built.commit));
}

/// A reviewer failing twice parks the ticket at Verify. `continue` will not
/// run the pass again on a Build that is no longer what would be pushed:
/// while the branch has moved, then once the decider commented after the
/// Build read the thread; each refusal keeps the park and runs nothing.
#[test]
fn a_ticket_parked_at_verify_is_not_resumed_on_a_moved_branch_or_a_late_comment() {
    let bench = Bench::verifying(false);
    let check = answered_round(&bench);
    let (outcome, printed) = bench.continue_ticket(vec![
        check,
        bench.reply(Some("Hello"), Some(DONE)),
        bench.reviewing(FAILED_REVIEW),
        bench.reviewing(FAILED_REVIEW),
    ]);
    assert!(
        matches!(
            outcome,
            Err(Stop::Parked {
                reason: ParkReason::FailedRuns,
                ..
            })
        ),
        "{outcome:?}\n{printed}"
    );
    let parked = |bench: &Bench| {
        let state = bench.record().state;
        (state.stage, state.waiting)
    };
    assert_eq!(parked(&bench), (Stage::Verify, Some(Waiting::Parked)));
    let commit = bench.record().review.unwrap().build.unwrap().commit;
    let checkout = bench.dirs().checkout();
    let moves = |to: &str| {
        bench
            .env
            .run(&checkout, &["update-ref", "refs/heads/owlshift/demo-1", to])
            .unwrap();
    };
    let ran = bench.briefs().len();

    moves(&format!("{commit}^"));
    let (refused, printed) = bench.continue_ticket(Vec::new());
    match refused {
        Err(Stop::Refused(why)) => {
            assert!(why.contains(&format!("not at {commit}")), "{why}");
            assert!(why.contains("`owlshift do DEMO-1`"), "{why}");
        }
        other => panic!("{other:?}\n{printed}"),
    }
    assert_eq!(parked(&bench), (Stage::Verify, Some(Waiting::Parked)));
    moves(&commit);

    bench.answer("Also say Bonjour.");
    let (refused, printed) = bench.continue_ticket(Vec::new());
    match refused {
        Err(Stop::Refused(why)) => {
            assert!(
                why.contains("commented on DEMO-1 after the Build that Verify reviews"),
                "{why}"
            );
        }
        other => panic!("{other:?}\n{printed}"),
    }
    assert_eq!(parked(&bench), (Stage::Verify, Some(Waiting::Parked)));
    assert_eq!(bench.briefs().len(), ran, "nothing ran");
}
