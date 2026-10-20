//! Verify on the bench (OWL-304, build plan "P5 · Quality pipeline", part
//! G): once a Build's gate passed and the thread was read again, the
//! variant's roster reviews the Build's diff before the push, on the bench
//! of `on_demand.rs`: the Markdown tracker, the fake harness playing every
//! run, the verify seats' included, the fake GitHub.

use owlshift_contracts::review::{FindingOutcome, FindingsMode, Mandate, ResponseOutcome};
use owlshift_core::vocab::Harness as HarnessKind;

use super::*;

/// A Verify run's `done` with a finding per (severity, claim).
pub(super) fn found(findings: &[(&str, &str)]) -> String {
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
    json!({ "format": 13, "status": "done", "summary": "Reviewed.", "findings": findings })
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
    json!({ "format": 13, "status": "done", "summary": "Ruled.", "rulings": rulings }).to_string()
}

/// A Build's `done` answering the findings to fix of its brief, one per
/// (finding, outcome) (format 13, OWL-377).
pub(super) fn fixed_build(given: &[(&str, &str)]) -> String {
    let mut result: Value = serde_json::from_str(DONE).unwrap();
    result["finding_responses"] = super::design_scenarios::answers(given);
    result.to_string()
}

const FAILED_REVIEW: &str =
    r#"{"format":13,"status":"failed","summary":"Could not read the diff."}"#;

impl Bench {
    /// A bench whose verify seats play from its reply queue.
    pub(super) fn verifying() -> Self {
        let bench = Bench::new(true);
        bench.verify_plays.set(true);
        bench
    }

    /// A read-only run's reply: its result alone.
    pub(super) fn reviewing(&self, result: &str) -> Reply {
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
/// alone so holding conformance, on Claude Code (OWL-347), reviews the
/// diff from the commit the Build started from, which the runner wrote
/// beside the brief; the clean pass is kept with its reviewer, and the
/// DELIVERY report names it.
#[test]
fn a_trivial_ticket_is_verified_before_its_pull_request_opens() {
    let bench = Bench::verifying();
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
    assert_eq!(pass.reviewers[0].harness, HarnessKind::Claude);
    assert!(pass.reviewers[0].same_model);
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
    let verify = "**Verify, 1 pass.** Reviewers of the last pass:\n- critic and conformance on \
                  Claude Code (its default model, the Build's own model: no other is configured)";
    assert!(report.contains(verify), "{report}");
    assert!(!report.contains("standing in"), "{report}");
}

/// OWL-321: `[roles.verify]` is accepted, and its file reaches the brief of
/// the Verify reviewer after `stack.rules`, and no other role's.
#[test]
fn the_verify_roles_rules_reach_the_verify_brief_alone() {
    let bench = Bench::verifying();
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

/// The critic runs on Claude Code, on the standard tier when no other model
/// is configured (OWL-347); the arbiter,
/// given the diff, confirms one finding and rejects the other; the Verify
/// fix run gets the confirmed one alone, without the diff, and the second
/// pass is clean. The report lists the rejected finding with its reason.
#[test]
fn the_arbiter_confirms_a_finding_the_verify_fix_run_fixes_and_rejects_another() {
    let bench = Bench::verifying();
    let replies = vec![
        bench.reply(Some("Hello"), Some(DONE)),
        bench.reviewing(&found(&[
            ("major", "The greeting has no full stop."),
            ("major", "GREETING.md needs a licence header."),
        ])),
        bench.reviewing(&ruled(&["confirmed", "rejected"])),
        bench.reply(Some("Hello, again"), Some(&fixed_build(&[("F1", "fixed")]))),
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
        (HarnessKind::Claude, Tier::Standard)
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
    assert_eq!(review.passes[0].reviewers[0].harness, HarnessKind::Claude);
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
        "**Verify, 2 passes.** Reviewers of the last pass:\n- critic and conformance on Claude \
         Code (its default model",
        "- Pass 1, F2, major, critic and conformance on Claude Code: GREETING.md needs a \
         licence header. Rejected: Checked F2 at GREETING.md:1.",
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
    let bench = Bench::verifying();
    let config = bench.remote.checkout.join("owlshift.toml");
    let text = fs::read_to_string(&config).unwrap();
    fs::write(
        &config,
        // No `pipeline.findings`: the author's triage is the default.
        text.replace("findings = \"arbiter\"", ""),
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

/// OWL-377 at Verify: each finding goes to the Build that owns the code,
/// none to the decider. Pass 1's two majors go to a fix run, which fixes
/// one and disagrees with the other, with its reason; pass 2 reads the
/// disagreement in its `earlier` and leaves a `blocker` and a `minor`,
/// which go to one last Build: it fixes the blocker and defers the minor,
/// and the delivery follows with no third pass. No question round, no
/// resolver run, no follow-up filed by the runner; the DELIVERY report
/// lists what the Builds kept against their reviewers, with their reasons,
/// and what the last Build fixed.
#[test]
fn every_verify_finding_goes_to_its_build_and_the_report_lists_what_it_kept() {
    let bench = Bench::verifying();
    bench.inbox.borrow_mut().on = true;
    let replies = vec![
        bench.reply(Some("Hello"), Some(DONE)),
        bench.reviewing(&found(&[
            ("major", "The greeting has no full stop."),
            ("major", "GREETING.md needs a licence header."),
        ])),
        bench.reviewing(&ruled(&["confirmed", "confirmed"])),
        bench.reply(
            Some("Hello, again"),
            Some(&fixed_build(&[("F1", "fixed"), ("F2", "disagreed")])),
        ),
        bench.reviewing(&found(&[
            ("blocker", "The greeting still has no full stop."),
            ("minor", "GREETING.md could open with a heading."),
        ])),
        bench.reviewing(&ruled(&["confirmed", "confirmed"])),
        bench.reply(
            Some("Hello, at last"),
            Some(&fixed_build(&[("F1", "fixed"), ("F2", "deferred")])),
        ),
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
            Role::Verify,
            Role::Arbiter,
            Role::Build
        ],
        "no third pass, no resolver"
    );
    assert!(
        !bench
            .comments()
            .iter()
            .any(|c| c.starts_with("[owlshift] QUESTIONS")),
        "no round"
    );
    let second = bench.last_of(Role::Verify).review.expect("a review");
    let earlier: Vec<(u32, &str, Option<FindingOutcome>)> = second
        .earlier
        .iter()
        .map(|e| (e.pass.get(), e.kept.finding.id.as_str(), e.kept.outcome))
        .collect();
    assert_eq!(
        earlier,
        [
            (1, "F1", Some(FindingOutcome::Fixed)),
            (1, "F2", Some(FindingOutcome::Disagreed))
        ]
    );
    let last = bench.last_of(Role::Build);
    let given = last.findings.as_ref().expect("the last pass's findings");
    let ids: Vec<&str> = given.findings.iter().map(|f| f.id.as_str()).collect();
    assert_eq!((given.mode, ids), (FindingsMode::Fix, vec!["F1", "F2"]));
    let residue = &bench.reviews("residue")[0];
    assert_eq!(
        (&residue["pass"], &residue["asked"], &residue["last_fix"]),
        (&json!(2), &json!([]), &json!(["F1", "F2"]))
    );
    let review = bench.record().review.unwrap();
    let outcomes: Vec<Vec<Option<FindingOutcome>>> = review
        .passes
        .iter()
        .map(|pass| pass.findings.iter().map(|kept| kept.outcome).collect())
        .collect();
    assert_eq!(
        outcomes,
        [
            vec![Some(FindingOutcome::Fixed), Some(FindingOutcome::Disagreed)],
            vec![Some(FindingOutcome::Fixed), Some(FindingOutcome::Deferred)]
        ]
    );
    assert!(bench.inbox.borrow().tickets.is_empty(), "nothing filed");
    let report = bench.comments().pop().unwrap();
    for text in [
        "**Verify, 2 passes.**",
        "**Findings the Build kept its choice on or deferred,** with its reasons:\n- Pass 1, F2, \
         major, critic and conformance on Claude Code: GREETING.md needs a licence header. \
         Disagreed: F2 disagreed: the plan's step 1 shows it.\n- Pass 2, F2, minor, critic and \
         conformance on Claude Code: GREETING.md could open with a heading. Deferred: F2 \
         deferred: the plan's step 1 shows it.",
        "**Findings given to the last Build,** which the runner's gate checked, with no Verify \
         pass after it:\n- Pass 2, F1, blocker, critic and conformance on Claude Code: The \
         greeting still has no full stop.",
    ] {
        assert!(report.contains(text), "{text}\n---\n{report}");
    }
}
/// OWL-160's run of 2026-10-18, replayed (OWL-356): Verify raises four
/// `minor` findings and the arbiter confirms them; one more Build fixes
/// them, the runner's gate passes, and the delivery follows with no second
/// Verify pass. The DELIVERY report names the four as given to that last
/// Build, and the review record keeps the commit the pass reviewed.
#[test]
fn verify_minors_get_one_last_build_and_no_second_pass() {
    let bench = Bench::verifying();
    let claims = [
        "The greeting has no full stop.",
        "No test covers an empty greeting.",
        "GREETING.md could open with a heading.",
        "The greeting could name the reader.",
    ];
    let minors: Vec<(&str, &str)> = claims.iter().map(|claim| ("minor", *claim)).collect();
    let replies = vec![
        bench.reply(Some("Hello"), Some(DONE)),
        bench.reviewing(&found(&minors)),
        bench.reviewing(&ruled(&["confirmed"; 4])),
        bench.reply(
            Some("Hello, again"),
            Some(&fixed_build(&[
                ("F1", "fixed"),
                ("F2", "fixed"),
                ("F3", "fixed"),
                ("F4", "fixed"),
            ])),
        ),
    ];
    let (outcome, printed) = bench.run(replies, None);
    outcome.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    // At most one Build after the first Verify, and no Verify after it.
    assert_eq!(
        bench.run_roles(),
        [Role::Build, Role::Verify, Role::Arbiter, Role::Build]
    );
    let last = bench.last_of(Role::Build);
    let given = last.findings.as_ref().expect("the minors to fix");
    let ids: Vec<&str> = given.findings.iter().map(|f| f.id.as_str()).collect();
    assert_eq!(
        (given.mode, ids),
        (FindingsMode::Fix, vec!["F1", "F2", "F3", "F4"])
    );
    let pass = &bench.reviews("pass")[0];
    assert_eq!(
        (&pass["outcome"], &pass["next"]),
        (&json!("last_fix"), &json!("build"))
    );
    assert_eq!(
        bench.reviews("residue")[0]["last_fix"]
            .as_array()
            .unwrap()
            .len(),
        4
    );
    let skipped = &bench.reviews("last_fix")[0];
    assert_eq!(
        (&skipped["pass"], &skipped["next"]),
        (&json!(1), &json!("deliver"))
    );
    let review = bench.record().review.unwrap();
    assert_eq!(review.passes.len(), 1);
    assert!(
        review.passes[0]
            .findings
            .iter()
            .all(|kept| kept.outcome == Some(FindingOutcome::Fixed))
    );
    assert_eq!(review.passes[0].head.as_ref().map(String::len), Some(40));
    assert!(
        review.last_fix.is_none() && review.build.is_none(),
        "delivered"
    );
    let report = bench.comments().pop().unwrap();
    assert!(report.contains("**Verify, 1 pass.**"), "{report}");
    assert!(
        report.contains(
            "**Findings given to the last Build,** which the runner's gate checked, with no \
             Verify pass after it:\n- Pass 1, F1, minor, critic and conformance on Claude Code: \
             The greeting has no full stop."
        ),
        "{report}"
    );
    for (n, claim) in claims.iter().enumerate() {
        assert!(
            report.contains(&format!("F{}, minor", n + 1)),
            "{claim}\n{report}"
        );
    }
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

/// A last fix whose Build asks is no longer the last fix alone (OWL-356):
/// once the resolver decides its question, the Build runs again, and a
/// Verify pass reviews it, told the diff since the commit pass 1 reviewed
/// and pass 1's finding, fixed.
#[test]
fn a_last_fix_that_asks_is_reviewed_on_what_changed_since() {
    let bench = Bench::verifying();
    let replies = vec![
        bench.reply(Some("Hello"), Some(DONE)),
        bench.reviewing(&found(&[("minor", "The greeting has no full stop.")])),
        bench.reviewing(&ruled(&["confirmed"])),
        bench.reply(None, Some(NAMING)),
        bench.reviewing(DECIDED_Q1),
        bench.reply(Some("Hello, again"), Some(&fixed_build(&[("F1", "fixed")]))),
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
            Role::Resolver,
            Role::Build,
            Role::Verify
        ]
    );
    assert!(bench.reviews("last_fix").is_empty(), "no pass skipped");
    let review = bench.record().review.unwrap();
    let first_head = review.passes[0].head.clone().expect("the commit reviewed");
    let second = bench.last_of(Role::Verify).review.expect("a review");
    assert_eq!(second.pass.get(), 2);
    let since = second.since.expect("what changed since pass 1");
    assert_eq!(since.pass.get(), 1);
    let diff = since.diff.expect("the diff since");
    assert_eq!(diff.base, first_head);
    assert_eq!(diff.path.as_str(), ".owlshift/run/since.patch");
    let written = fs::read_to_string(bench.worktree().join(diff.path.as_str())).unwrap();
    assert!(written.contains("+Hello, again, reader."), "{written}");
    assert!(!written.contains("+Hello, reader."), "{written}");
    let earlier: Vec<(u32, &str, Option<FindingOutcome>)> = second
        .earlier
        .iter()
        .map(|e| (e.pass.get(), e.kept.finding.id.as_str(), e.kept.outcome))
        .collect();
    assert_eq!(earlier, [(1, "F1", Some(FindingOutcome::Fixed))]);
    let report = bench.comments().pop().unwrap();
    assert!(report.contains("**Verify, 2 passes.**"), "{report}");
    assert!(!report.contains("given to the last Build"), "{report}");
}

/// A last pass passes, then the push is refused. `continue` runs that pass
/// again in place of its record, and delivers; the runner files no
/// follow-up from a finding (OWL-377).
#[test]
fn a_pass_run_again_after_a_failed_delivery_takes_its_records_place() {
    let bench = Bench::verifying();
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
        bench.reply(Some("Hello, again"), Some(&fixed_build(&[("F1", "fixed")]))),
        bench.reviewing(&found(&[])),
    ];
    let (outcome, printed) = bench.run(replies, None);
    assert!(
        matches!(&outcome, Err(Stop::Delivery(why)) if why.contains("rejected")),
        "{outcome:?}\n{printed}"
    );
    let review = bench.record().review.unwrap();
    assert_eq!(review.passes.len(), 2);
    assert!(review.build.unwrap().followups.is_empty());

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
    let bench = Bench::verifying();
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
    let bench = Bench::verifying();
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
