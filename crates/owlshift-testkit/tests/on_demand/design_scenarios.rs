//! Design and Design review on the bench (OWL-303, build plan "P5 · Quality
//! pipeline", part F): `owlshift do` and `continue` take a `standard` or
//! `risky` ticket through Design and Design review before Build, on the
//! bench of `on_demand.rs`: the Markdown tracker, the fake harness playing
//! every role, the fake GitHub.

use owlshift_contracts::review::{FindingOutcome, FindingsMode, Mandate, ResponseOutcome};
use owlshift_core::vocab::Harness as HarnessKind;

use super::*;

/// The plan of a design run.
pub(super) const PLAN: &str = r#"{"goal":"Add GREETING.md saying Hello.",
"criteria":["GREETING.md says Hello."],
"approach":"Write GREETING.md with the line the gate greps.",
"steps":[{"what":"Add GREETING.md.","test":"The gate's grep for Hello."}]}"#;

/// The plan a design run revises it to.
pub(super) const REVISED: &str = r#"{"goal":"Add GREETING.md saying Hello.",
"criteria":["GREETING.md says Hello."],
"approach":"Write GREETING.md with the line the gate greps, the word checked.",
"steps":[{"what":"Add GREETING.md.","test":"The gate's grep for the word Hello."}]}"#;

/// A plan with a fork.
pub(super) const FORKED: &str = r#"{"goal":"Add GREETING.md saying Hello.",
"criteria":["GREETING.md says Hello."],
"approach":"Write GREETING.md with the line the gate greps.",
"steps":[{"what":"Add GREETING.md.","test":"The gate's grep for Hello."}],
"forks":[{"choice":"Markdown.","options":["Markdown.","Plain text."],
"reason":"The repository's docs are Markdown."}]}"#;

/// A design run's `done` with `plan`.
pub(super) fn designed(plan: &str) -> String {
    format!(r#"{{"format":12,"status":"done","summary":"Planned the greeting.","plan":{plan}}}"#)
}

/// A review run's `done` with a finding per (severity, claim).
pub(super) fn reviewed(findings: &[(&str, &str)]) -> String {
    let findings: Vec<Value> = findings
        .iter()
        .enumerate()
        .map(|(n, (severity, claim))| {
            json!({
                "id": format!("F{}", n + 1),
                "severity": severity,
                "claim": claim,
                "evidence": "Plan step 1.",
                "settle": "Change step 1.",
            })
        })
        .collect();
    json!({ "format": 12, "status": "done", "summary": "Reviewed.", "findings": findings })
        .to_string()
}

/// The arbiter's `done` with a ruling per outcome, on F1..Fn.
pub(super) fn ruled(outcomes: &[&str]) -> String {
    let rulings: Vec<Value> = outcomes
        .iter()
        .enumerate()
        .map(|(n, outcome)| {
            json!({
                "finding": format!("F{}", n + 1),
                "outcome": outcome,
                "reason": format!("Checked plan step 1 for F{}.", n + 1),
            })
        })
        .collect();
    json!({ "format": 12, "status": "done", "summary": "Ruled.", "rulings": rulings }).to_string()
}

/// A design run's answer to an approval round.
pub(super) fn answered(approval: &str, plan: Option<&str>) -> String {
    let mut result = json!({
        "format": 12,
        "status": "done",
        "summary": "Read the decider's reply.",
        "approval": approval,
    });
    if let Some(plan) = plan {
        result["plan"] = serde_json::from_str(plan).unwrap();
    }
    result.to_string()
}

/// A Build run that cannot follow its plan.
pub(super) const REPLAN: &str = r#"{"format":12,"status":"questions",
"summary":"Step 1 cannot be followed as written.",
"questions":[{"id":"Q1","category":"naming","context":"Step 1 names the file to write.",
"text":"Which file should hold the greeting?","replan":true,"recommendation":{"why":"The simplest reading of the ticket."}}]}"#;

const FAILED: &str = r#"{"format":12,"status":"failed","summary":"Could not read the plan."}"#;

impl Bench {
    /// A bench whose ticket carries `variant`'s label, with Codex given
    /// when `codex`.
    fn of(variant: &str, codex: bool) -> Self {
        let bench = Bench::new(true);
        bench.add_labels(&[&format!("variant:{variant}")]);
        bench.codex.set(codex);
        bench
    }

    /// A read-only run's reply: its result alone.
    pub(super) fn read(&self, result: &str) -> Reply {
        self.reply(None, Some(result))
    }

    /// A Build run's reply that delivers.
    pub(super) fn built(&self) -> Reply {
        self.reply(Some("Hello"), Some(DONE))
    }

    /// The role of every run, oldest first.
    pub(super) fn roles(&self) -> Vec<Role> {
        self.briefs().iter().map(|brief| brief.role).collect()
    }

    /// The decision events holding `key`.
    pub(super) fn decisions(&self, key: &str) -> Vec<Value> {
        data_of(&self.events(), EventKind::Decision, key)
    }

    /// The comment starting with `header`, the last one when several do.
    pub(super) fn marked(&self, header: &str) -> String {
        self.comments()
            .into_iter()
            .rev()
            .find(|comment| comment.starts_with(header))
            .unwrap_or_else(|| panic!("no {header} comment"))
    }
}

/// S9's design half: a `standard` ticket's dispatch from Ready goes to
/// Design; the plan, kept in `plan.json`, is reviewed by the critic, on
/// Claude Code without Codex, and conformance; a clean pass posts the PLAN
/// comment and the Build follows the plan from its base, dispatched again.
#[test]
fn a_standard_ticket_is_planned_and_reviewed_before_its_build() {
    let bench = Bench::of("standard", false);
    let replies = vec![
        bench.read(&designed(PLAN)),
        bench.read(&reviewed(&[])),
        bench.read(&reviewed(&[])),
        bench.built(),
    ];
    let (outcome, printed) = bench.run(replies, None);
    outcome.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    assert_eq!(
        bench.roles(),
        [
            Role::Design,
            Role::DesignReview,
            Role::DesignReview,
            Role::Build,
            Role::Verify
        ]
    );
    let briefs = bench.briefs();
    for brief in &briefs[..3] {
        assert_eq!(brief.permissions.level, PermissionLevel::ReadOnly);
        assert!(!brief.permissions.network);
    }
    assert!(briefs[0].plan.is_none(), "a first design revises nothing");
    let mandates: Vec<Vec<Mandate>> = briefs[1..3]
        .iter()
        .map(|brief| brief.review.as_ref().unwrap().mandates.clone())
        .collect();
    assert_eq!(
        mandates,
        [vec![Mandate::Critic], vec![Mandate::Conformance]]
    );

    let record = bench.record();
    let kept = record.plan.expect("plan.json");
    let found = MarkdownTracker::new(&bench.remote.checkout)
        .ticket(&ticket())
        .unwrap();
    assert_eq!(
        kept.text_sha256,
        ticket_ref::text_sha256(&found.title, &found.description)
    );
    assert_eq!(kept.plan.goal, "Add GREETING.md saying Hello.");
    assert!(kept.approval.is_none());
    for brief in &briefs[1..] {
        let given = brief.plan.as_ref().expect("the plan reviewed and followed");
        assert_eq!((&given.base, &given.plan), (&kept.base, &kept.plan));
    }
    let review = record.review.expect("review.json");
    // The Design review pass, then the Verify pass (OWL-304).
    assert_eq!(review.passes.len(), 2);
    assert_eq!(review.passes[1].stage, Stage::Verify);
    assert_eq!(
        (review.passes[0].stage, review.passes[0].pass.get()),
        (Stage::DesignReview, 1)
    );
    assert!(review.passes[0].findings.is_empty());

    let plan = bench.marked("[owlshift] PLAN");
    for text in [
        "**Goal.** Add GREETING.md saying Hello.",
        "**Steps.**\n1. Add GREETING.md. Test: The gate's grep for Hello.",
        "- critic on Claude Code (its default model",
        "standing in for Codex",
        "**Next.** The Build follows this plan.",
    ] {
        assert!(plan.contains(text), "{text}\n---\n{plan}");
    }
    assert!(
        bench
            .comments()
            .last()
            .unwrap()
            .starts_with("[owlshift] DELIVERY")
    );
    // Design review's pass, then Verify's (OWL-304).
    let passes = bench.decisions("review");
    assert_eq!(passes.len(), 2, "{passes:?}");
    assert_eq!(passes[0]["outcome"], "passed");
    assert_eq!(passes[0]["next"], "build");
    assert_eq!(passes[0]["reviewers"][0]["stand_in"], true);
    assert_eq!(passes[0]["reviewers"][1]["same_model"], true);
    assert_eq!(
        (&passes[1]["stage"], &passes[1]["next"]),
        (&json!("verify"), &json!("deliver"))
    );
    let dispatches: Vec<Value> = data_of(&bench.events(), EventKind::Dispatch, "command");
    assert_eq!(dispatches.len(), 2, "from Ready to Design, then to Build");
    assert!(
        bench
            .routed()
            .iter()
            .all(|routing| routing.harness == HarnessKind::Claude)
    );
    assert_eq!(bench.stage(), "In Review");
}

/// With Codex, the critic runs on it on the standard tier; the arbiter
/// confirms one finding and rejects the other; the design run fixes the
/// confirmed one alone; the next pass is clean, and the PLAN comment lists
/// the rejected finding with its reason.
#[test]
fn the_arbiter_confirms_a_finding_the_design_fixes_and_rejects_another() {
    let bench = Bench::of("standard", true);
    let replies = vec![
        bench.read(&designed(PLAN)),
        bench.read(&reviewed(&[
            ("major", "Step 1's test does not check the word."),
            ("major", "GREETING.md needs a licence header."),
        ])),
        bench.read(&reviewed(&[])),
        bench.read(&ruled(&["confirmed", "rejected"])),
        bench.read(&designed(REVISED)),
        bench.read(&reviewed(&[])),
        bench.read(&reviewed(&[])),
        bench.built(),
    ];
    let (outcome, printed) = bench.run(replies, None);
    outcome.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    assert_eq!(
        bench.roles(),
        [
            Role::Design,
            Role::DesignReview,
            Role::DesignReview,
            Role::Arbiter,
            Role::Design,
            Role::DesignReview,
            Role::DesignReview,
            Role::Build,
            Role::Verify,
        ]
    );
    let routed = bench.routed();
    assert_eq!(
        (routed[1].harness, routed[1].tier),
        (HarnessKind::Codex, Tier::Standard)
    );
    assert_eq!(routed[2].harness, HarnessKind::Claude);
    let briefs = bench.briefs();
    let ruling = briefs[3].findings.as_ref().unwrap();
    assert_eq!(
        (ruling.mode, ruling.findings.len()),
        (FindingsMode::Rule, 2)
    );
    let fix = briefs[4].findings.as_ref().expect("the confirmed finding");
    assert_eq!(fix.mode, FindingsMode::Fix);
    let ids: Vec<&str> = fix.findings.iter().map(|f| f.id.as_str()).collect();
    assert_eq!(ids, ["F1"]);
    assert!(briefs[4].plan.is_some(), "the revision gets the plan");

    let record = bench.record();
    assert!(
        record
            .plan
            .unwrap()
            .plan
            .approach
            .contains("the word checked")
    );
    let review = record.review.unwrap();
    let first = &review.passes[0].findings;
    assert_eq!(first[0].outcome, Some(FindingOutcome::Fixed));
    assert_eq!(first[1].outcome, Some(FindingOutcome::Rejected));
    assert_eq!(first[0].reviewer.harness, HarnessKind::Codex);
    assert!(!first[0].reviewer.stand_in);
    assert!(review.passes[1].findings.is_empty());
    let plan = bench.marked("[owlshift] PLAN");
    assert!(
        plan.contains(
            "- Pass 1, F2, major, critic on Codex: GREETING.md needs a licence header. \
             Rejected: Checked plan step 1 for F2."
        ),
        "{plan}"
    );
    assert!(!plan.contains("standing in"), "{plan}");
    let ruling = &bench.decisions("review")[0];
    assert_eq!(ruling["review"], "ruling");
    assert_eq!(ruling["confirmed"], json!(["F1"]));
    assert_eq!(ruling["rejected"], json!(["F2"]));
}

/// OWL-318: `[roles.design]`, `[roles.design_review]` and `[roles.arbiter]`
/// are accepted, and each file reaches the brief of its role after
/// `stack.rules`, and no other role's.
#[test]
fn the_design_roles_rules_reach_their_own_briefs() {
    let bench = Bench::of("standard", true);
    let checkout = &bench.remote.checkout;
    let files = [
        ("design", "Plan in small steps.\n"),
        ("design_review", "Check the plan against the zones.\n"),
        ("arbiter", "Rule on severity alone.\n"),
    ];
    let mut config = fs::read_to_string(checkout.join("owlshift.toml")).unwrap();
    for (role, text) in files {
        fs::create_dir_all(checkout.join("docs/roles")).unwrap();
        fs::write(checkout.join(format!("docs/roles/{role}.md")), text).unwrap();
        config.push_str(&format!(
            "[roles.{role}]\nrules = [\"docs/roles/{role}.md\"]\n"
        ));
    }
    fs::write(checkout.join("owlshift.toml"), config).unwrap();
    bench
        .env
        .run(checkout, &["add", "owlshift.toml", "docs/roles"])
        .unwrap();
    bench
        .env
        .run(checkout, &["commit", "--quiet", "-m", "Name role rules"])
        .unwrap();
    bench
        .env
        .run(checkout, &["push", "--quiet", "origin", "main"])
        .unwrap();
    let replies = vec![
        bench.read(&designed(PLAN)),
        bench.read(&reviewed(&[(
            "major",
            "Step 1's test does not check the word.",
        )])),
        bench.read(&reviewed(&[])),
        bench.read(&ruled(&["confirmed"])),
        bench.read(&designed(REVISED)),
        bench.read(&reviewed(&[])),
        bench.read(&reviewed(&[])),
        bench.built(),
    ];
    let (outcome, printed) = bench.run(replies, None);
    outcome.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    let briefs = bench.briefs();
    let mut seen = Vec::new();
    for brief in &briefs {
        let sources: Vec<&str> = brief.rules.iter().map(|r| r.source.as_str()).collect();
        let own = match brief.role {
            Role::Design => Some("design"),
            Role::DesignReview => Some("design_review"),
            Role::Arbiter => Some("arbiter"),
            _ => None,
        };
        match own {
            Some(role) => {
                let file = format!("docs/roles/{role}.md");
                assert_eq!(sources, ["AGENTS.md", file.as_str()], "{:?}", brief.role);
                let text = files.iter().find(|(name, _)| *name == role).unwrap().1;
                assert_eq!(brief.rules[1].text, text);
                if !seen.contains(&role) {
                    seen.push(role);
                }
            }
            None => assert_eq!(sources, ["AGENTS.md"], "{:?}", brief.role),
        }
    }
    assert_eq!(seen, ["design", "design_review", "arbiter"]);
}

/// `pipeline.findings = "author"`: no arbiter runs; the next design run
/// answers every finding, a result that leaves one unanswered being refused
/// as a failed run, and its rejection is kept and shown.
#[test]
fn the_author_answers_every_finding_and_its_rejection_is_kept() {
    let bench = Bench::of("standard", false);
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
    let answering = |responses: &str| {
        format!(
            r#"{{"format":12,"status":"done","summary":"Revised.","plan":{REVISED},
            "finding_responses":[{responses}]}}"#
        )
    };
    let replies = vec![
        bench.read(&designed(PLAN)),
        bench.read(&reviewed(&[("major", "GREETING.md needs a licence header.")])),
        bench.read(&reviewed(&[])),
        // No answer to F1: refused.
        bench.read(&answering("")),
        bench.read(&answering(
            r#"{"finding":"F1","outcome":"rejected","reason":"No file of the repository has one."}"#,
        )),
        bench.read(&reviewed(&[])),
        bench.read(&reviewed(&[])),
        bench.built(),
    ];
    let (outcome, printed) = bench.run(replies, None);
    outcome.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    let roles = bench.roles();
    assert!(!roles.contains(&Role::Arbiter), "{roles:?}");
    let briefs = bench.briefs();
    let answer = briefs[3].findings.as_ref().unwrap();
    assert_eq!(
        (answer.mode, answer.findings.len()),
        (FindingsMode::Answer, 1)
    );
    let ended = data_of(&bench.events(), EventKind::RunEnded, "outcome");
    assert_eq!(ended[3]["outcome"], "failed", "{:?}", ended[3]);
    let review = bench.record().review.unwrap();
    let kept = &review.passes[0].findings[0];
    assert_eq!(
        kept.response.as_ref().map(|r| r.outcome),
        Some(ResponseOutcome::Rejected)
    );
    assert_eq!(kept.outcome, Some(FindingOutcome::Rejected));
    let plan = bench.marked("[owlshift] PLAN");
    assert!(
        plan.contains("Rejected: No file of the repository has one."),
        "{plan}"
    );
}

/// The bound: a `major` still confirmed after the second pass goes to the
/// decider, one question, without the resolver; a `minor` is noted in the
/// PLAN comment. After the answer, the design runs with it in its thread,
/// and the one more pass it buys is clean, so the Build follows.
#[test]
fn a_finding_two_passes_left_goes_to_the_decider_and_buys_one_more_pass() {
    let bench = Bench::of("standard", false);
    let replies = vec![
        bench.read(&designed(PLAN)),
        bench.read(&reviewed(&[(
            "major",
            "Step 1's test does not check the word.",
        )])),
        bench.read(&reviewed(&[])),
        bench.read(&ruled(&["confirmed"])),
        bench.read(&designed(REVISED)),
        bench.read(&reviewed(&[
            ("major", "Step 1's test still does not check the word."),
            ("minor", "The goal could name the file's path."),
        ])),
        bench.read(&reviewed(&[])),
        bench.read(&ruled(&["confirmed", "confirmed"])),
    ];
    let (outcome, printed) = bench.run(replies, None);
    let stop = outcome.expect_err("the residue goes to the decider");
    let Stop::NeedsInput {
        questions, posted, ..
    } = &stop
    else {
        panic!("{stop:?}\n{printed}");
    };
    assert_eq!(posted.as_ref().map(|round| round.get()), Ok(1));
    assert_eq!(questions.len(), 1);
    assert_eq!(questions[0].category, "review_finding");
    assert!(
        questions[0]
            .text
            .starts_with("Step 1's test still does not check the word.")
    );
    assert!(!bench.roles().contains(&Role::Resolver));
    let plan = bench.marked("[owlshift] PLAN");
    for text in [
        "F1, major, critic on Claude Code, standing in for Codex: Step 1's test still does not \
         check the word. (asked below)",
        "F2, minor, critic on Claude Code, standing in for Codex: The goal could name the file's \
         path. (noted for the Build)",
        "**Next.** The findings asked below go to the decider",
    ] {
        assert!(plan.contains(text), "{text}\n---\n{plan}");
    }
    assert!(
        bench
            .comments()
            .last()
            .unwrap()
            .starts_with("[owlshift] QUESTIONS · round 1")
    );
    let record = bench.record();
    assert_eq!(
        (
            record.state.stage,
            record.state.waiting,
            record.state.review_passes
        ),
        (Stage::Design, Some(Waiting::NeedsInput), 2)
    );
    let residue = bench
        .decisions("review")
        .into_iter()
        .find(|event| event["review"] == "residue")
        .expect("the residue event");
    assert_eq!(residue["asked"], json!(["F1"]));
    assert_eq!(residue["noted"], json!(["F2"]));

    bench.answer("Q1: settle it as it says, with the word checked.");
    let replies = vec![
        bench.read(&check(&[(
            "Q1",
            "answered",
            "Settle it as the finding says.",
        )])),
        bench.read(&designed(REVISED)),
        bench.read(&reviewed(&[])),
        bench.read(&reviewed(&[])),
        bench.built(),
    ];
    let (outcome, printed) = bench.continue_ticket(replies);
    outcome.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    let briefs = bench.briefs();
    // Design, its review pass, the Build and its Verify pass (OWL-304).
    let after = &briefs[briefs.len() - 5];
    assert_eq!(after.role, Role::Design);
    assert!(after.findings.is_none() && !after.approval);
    assert!(after.plan.is_some());
    assert_eq!(latest_ask(after), ["Q1"]);
    let review = bench.record().review.unwrap();
    assert_eq!(review.passes.len(), 4);
    assert_eq!(review.passes[2].pass.get(), 3);
}

/// A `risky` ticket's plan approval (OWL-32): the clean pass of its three
/// reviewers asks the decider, its fork as an option; the base moves while
/// it waits; the approval is kept with the decider's comment and the plan's
/// hash, and the Build starts from the base fetched at its dispatch.
#[test]
fn a_risky_plan_is_approved_and_its_build_starts_from_the_moved_base() {
    let bench = Bench::of("risky", false);
    let replies = vec![
        bench.read(&designed(FORKED)),
        bench.read(&reviewed(&[])),
        bench.read(&reviewed(&[])),
        bench.read(&reviewed(&[])),
    ];
    let (outcome, printed) = bench.run(replies, None);
    let stop = outcome.expect_err("the approval goes to the decider");
    let Stop::NeedsInput { questions, .. } = &stop else {
        panic!("{stop:?}\n{printed}");
    };
    assert_eq!(questions[0].category, "plan_approval");
    assert_eq!(
        questions[0]
            .options
            .iter()
            .map(|o| o.label.as_str())
            .collect::<Vec<_>>(),
        [
            "Approve the plan as it is",
            "Fork 1: Plain text. instead of Markdown."
        ]
    );
    let mandates: Vec<Vec<Mandate>> = bench
        .briefs()
        .iter()
        .filter_map(|brief| brief.review.as_ref().map(|review| review.mandates.clone()))
        .collect();
    assert_eq!(
        mandates,
        [
            vec![Mandate::Critic],
            vec![Mandate::Conformance],
            vec![Mandate::Debt]
        ]
    );
    assert!(
        bench
            .marked("[owlshift] PLAN")
            .contains("**Next.** The plan waits for the decider's approval")
    );
    assert_eq!(bench.stage(), "Needs Input");
    let planned_on = bench.record().plan.unwrap().base;

    let moved = bench.merge_into_main(&[("NOTES.md", "Notes.\n")]);
    bench.answer("Approved as it is.");
    let replies = vec![
        bench.read(&check(&[("Q1", "answered", "Approved as it is.")])),
        bench.read(&answered("approved", None)),
        bench.built(),
    ];
    let (outcome, printed) = bench.continue_ticket(replies);
    outcome.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    let briefs = bench.briefs();
    // The Build, then `risky`'s two Verify seats (OWL-304).
    let approving = &briefs[briefs.len() - 4];
    assert_eq!(approving.role, Role::Design);
    assert!(approving.approval && approving.plan.is_some());
    let comments = MarkdownTracker::new(&bench.remote.checkout)
        .comments(&ticket())
        .unwrap();
    let reply = comments
        .iter()
        .rev()
        .find(|comment| comment.body.starts_with("Approved"))
        .unwrap();
    let kept = bench.record().plan.unwrap();
    let approval = kept.approval.expect("the approval is kept");
    assert_eq!(approval.comment, reply.id);
    let planned = bench
        .decisions("plan")
        .into_iter()
        .find(|event| event["plan"] == "kept")
        .unwrap();
    assert_eq!(json!(approval.plan_sha256), planned["plan_sha256"]);
    // The Build follows the plan written on the old base, from the new one.
    let build = bench.last_of(Role::Build);
    assert_eq!(build.plan.as_ref().unwrap().base, planned_on);
    let dispatched = data_of(&bench.events(), EventKind::Dispatch, "command");
    assert_eq!(dispatched.last().unwrap()["base_commit"], json!(moved));
    assert_ne!(planned_on, moved);
}

/// A reply that changes the plan: the design run revises it, the revision
/// gets one pass, and its approval is asked again, the plan unapproved.
#[test]
fn a_changed_plan_gets_one_more_pass_and_is_asked_again() {
    let bench = Bench::of("risky", false);
    let replies = vec![
        bench.read(&designed(FORKED)),
        bench.read(&reviewed(&[])),
        bench.read(&reviewed(&[])),
        bench.read(&reviewed(&[])),
    ];
    let (outcome, _) = bench.run(replies, None);
    assert!(
        matches!(outcome, Err(Stop::NeedsInput { .. })),
        "{outcome:?}"
    );
    bench.answer("Use plain text instead.");
    let replies = vec![
        bench.read(&check(&[("Q1", "answered", "Plain text instead.")])),
        bench.read(&answered("changed", Some(REVISED))),
        bench.read(&reviewed(&[])),
        bench.read(&reviewed(&[])),
        bench.read(&reviewed(&[])),
    ];
    let (outcome, printed) = bench.continue_ticket(replies);
    let stop = outcome.expect_err("asked again");
    assert!(
        matches!(&stop, Stop::NeedsInput { posted: Ok(round), .. } if round.get() == 2),
        "{stop:?}\n{printed}"
    );
    let record = bench.record();
    let kept = record.plan.unwrap();
    assert!(kept.plan.approach.contains("the word checked"));
    assert!(kept.approval.is_none());
    assert_eq!(record.review.unwrap().passes.len(), 2);
    assert_eq!(record.state.review_passes, 2);
}

/// A Build that cannot follow its plan asks a `replan`; the resolver
/// settles it, and the ticket goes back to Design in the same command: the
/// new design run gets the old plan and the decision, its plan is reviewed
/// again, and the Build follows it.
#[test]
fn a_replan_the_resolver_settles_sends_the_ticket_back_to_design() {
    let bench = Bench::of("standard", false);
    let replies = vec![
        bench.read(&designed(PLAN)),
        bench.read(&reviewed(&[])),
        bench.read(&reviewed(&[])),
        bench.read(REPLAN),
        bench.read(DECIDED_Q1),
        bench.read(&designed(REVISED)),
        bench.read(&reviewed(&[])),
        bench.read(&reviewed(&[])),
        bench.built(),
    ];
    let (outcome, printed) = bench.run(replies, None);
    outcome.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    assert_eq!(
        bench.roles(),
        [
            Role::Design,
            Role::DesignReview,
            Role::DesignReview,
            Role::Build,
            Role::Resolver,
            Role::Design,
            Role::DesignReview,
            Role::DesignReview,
            Role::Build,
            Role::Verify,
        ]
    );
    let briefs = bench.briefs();
    let again = &briefs[5];
    assert!(
        again
            .plan
            .as_ref()
            .unwrap()
            .plan
            .approach
            .ends_with("greps.")
    );
    assert!(shape(again).contains(&"decision".to_owned()));
    assert_eq!(bench.decisions("replan")[0]["replan"], "decided");
    let record = bench.record();
    assert!(
        record
            .plan
            .unwrap()
            .plan
            .approach
            .contains("the word checked")
    );
    let passes: Vec<u32> = record
        .review
        .unwrap()
        .passes
        .iter()
        .map(|pass| pass.pass.get())
        .collect();
    // Design review's two loops, then the Verify pass (OWL-304).
    assert_eq!(passes, [1, 1, 1], "a new loop of passes");
}

/// On an approved plan, a `replan` goes to the decider without the
/// resolver, and its round returns to Design.
#[test]
fn a_replan_on_an_approved_plan_goes_to_the_decider_and_returns_to_design() {
    let bench = Bench::of("risky", false);
    let replies = vec![
        bench.read(&designed(FORKED)),
        bench.read(&reviewed(&[])),
        bench.read(&reviewed(&[])),
        bench.read(&reviewed(&[])),
    ];
    let (outcome, _) = bench.run(replies, None);
    assert!(
        matches!(outcome, Err(Stop::NeedsInput { .. })),
        "{outcome:?}"
    );
    bench.answer("Approved.");
    let replies = vec![
        bench.read(&check(&[("Q1", "answered", "Approved.")])),
        bench.read(&answered("approved", None)),
        bench.read(REPLAN),
    ];
    let (outcome, printed) = bench.continue_ticket(replies);
    let stop = outcome.expect_err("the replan goes to the decider");
    assert!(
        matches!(&stop, Stop::NeedsInput { posted: Ok(round), .. } if round.get() == 2),
        "{stop:?}\n{printed}"
    );
    assert!(!bench.roles().contains(&Role::Resolver));
    let record = bench.record();
    assert_eq!(
        (record.state.stage, record.state.waiting),
        (Stage::Design, Some(Waiting::NeedsInput))
    );
    assert!(record.questions.asks.last().unwrap().questions[0].replan);
}

/// A `variant:` label overrides the project's default: `trivial` goes
/// from Ready to Build; a label naming no variant is a warning, left out.
#[test]
fn a_trivial_label_skips_design_and_an_unknown_one_is_left_out() {
    let bench = Bench::new(true);
    let config = bench.remote.checkout.join("owlshift.toml");
    let text = fs::read_to_string(&config).unwrap();
    fs::write(
        &config,
        text.replace("default = \"trivial\"", "default = \"standard\""),
    )
    .unwrap();
    bench.add_labels(&["variant:trivial", "variant:huge"]);
    let (outcome, printed) = bench.run(vec![bench.built()], None);
    outcome.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    assert_eq!(bench.roles(), [Role::Build, Role::Verify]);
    assert!(bench.record().plan.is_none());
    let ignored = data_of(&bench.events(), EventKind::Warning, "label");
    assert!(
        ignored
            .iter()
            .any(|warning| warning["label"] == "variant:huge"),
        "{ignored:?}"
    );
}

/// A reviewer that fails twice parks the ticket at Design review, the
/// stage's failed-run bound.
#[test]
fn a_reviewer_failing_twice_parks_the_ticket_at_design_review() {
    let bench = Bench::of("standard", false);
    let replies = vec![
        bench.read(&designed(PLAN)),
        bench.read(FAILED),
        bench.read(FAILED),
    ];
    let (outcome, printed) = bench.run(replies, None);
    assert!(
        matches!(
            &outcome,
            Err(Stop::Parked {
                reason: ParkReason::FailedRuns,
                ..
            })
        ),
        "{outcome:?}\n{printed}"
    );
    let record = bench.record();
    assert_eq!(
        (record.state.stage, record.state.waiting),
        (Stage::DesignReview, Some(Waiting::Parked))
    );
    assert!(record.plan.is_some(), "the plan is kept for the restart");
}

/// A design run that finds the premise false and asks (OWL-306, S8): every
/// question goes to the decider without the resolver, the round returns to
/// Design, the QUESTIONS comment says so, and no plan is kept.
#[test]
fn a_false_premise_at_design_is_a_round_to_the_decider() {
    let bench = Bench::of("standard", false);
    let premise = r#"{"format":12,"status":"premise_false",
"summary":"The greeting file the ticket extends is not in the repository.",
"questions":[{"id":"Q1","category":"naming","context":"The ticket extends GREETING.md; no such file exists.",
"text":"Re-scope to create GREETING.md, or close the ticket?",
"options":[{"label":"Re-scope: create GREETING.md."},{"label":"Close the ticket."}],"recommendation":{"option":"A","why":"Re-scope"}}]}"#;
    let (outcome, printed) = bench.run(vec![bench.read(premise)], None);
    let stop = outcome.expect_err("the premise goes to the decider");
    assert!(
        matches!(
            &stop,
            Stop::NeedsInput { status: owlshift_contracts::result::Status::PremiseFalse, posted: Ok(round), .. }
                if round.get() == 1
        ),
        "{stop:?}\n{printed}"
    );
    assert_eq!(bench.roles(), [Role::Design]);
    let record = bench.record();
    assert_eq!(
        (record.state.stage, record.state.waiting),
        (Stage::Design, Some(Waiting::NeedsInput))
    );
    assert!(record.plan.is_none());
    assert!(
        bench
            .marked("[owlshift] QUESTIONS")
            .contains("The run found the ticket's premise false.")
    );
}

/// A design run that finds the premise false and asks nothing parks the
/// ticket at Design, `continue` restarting it (OWL-306).
#[test]
fn a_false_premise_at_design_without_a_question_parks_the_ticket() {
    let bench = Bench::of("standard", false);
    let premise = r#"{"format":12,"status":"premise_false","summary":"The file is not there."}"#;
    let (outcome, printed) = bench.run(vec![bench.read(premise)], None);
    assert!(
        matches!(
            &outcome,
            Err(Stop::Parked {
                reason: ParkReason::Blocked,
                ..
            })
        ),
        "{outcome:?}\n{printed}"
    );
    assert_eq!(bench.roles(), [Role::Design]);
    let record = bench.record();
    assert_eq!(
        (record.state.stage, record.state.waiting),
        (Stage::Design, Some(Waiting::Parked))
    );
    let parked = bench.marked("[owlshift] PARKED");
    assert!(
        parked.contains("found its premise false, with no question"),
        "{parked}"
    );
    assert!(parked.contains("owlshift continue"), "{parked}");
}
