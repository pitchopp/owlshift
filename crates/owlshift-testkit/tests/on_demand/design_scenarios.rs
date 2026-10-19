//! Design and Design review on the bench (OWL-303, build plan "P5 · Quality
//! pipeline", part F): `owlshift do` and `continue` take a `standard` or
//! `risky` ticket through Design and Design review before Build, on the
//! bench of `on_demand.rs`: the Markdown tracker, the fake harness playing
//! every role, the fake GitHub.

use owlshift_contracts::BasisKind;
use owlshift_contracts::comment::MarkedComment;
use owlshift_contracts::review::{
    FindingOutcome, FindingsMode, Mandate, PlanPart, ResponseOutcome,
};
use owlshift_core::vocab::Harness as HarnessKind;
use owlshift_runner::writer::MAX_POINTER_CHARS;

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

/// A plan with a fork and an alternative set aside, which the Build's brief
/// leaves out (OWL-365).
const FORKED_WITH_ALTERNATIVE: &str = r#"{"goal":"Add GREETING.md saying Hello.",
"criteria":["GREETING.md says Hello."],
"approach":"Write GREETING.md with the line the gate greps.",
"alternatives":[{"approach":"Greet in the README.","reason":"The ticket names GREETING.md."}],
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
    /// A bench whose ticket carries `variant`'s label.
    pub(super) fn of(variant: &str) -> Self {
        let bench = Bench::new(true);
        bench.add_labels(&[&format!("variant:{variant}")]);
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

    /// The plan's document, the ticket's one document (OWL-350).
    pub(super) fn plan_document(&self) -> String {
        let dir = self
            .remote
            .checkout
            .join("tickets")
            .join(TICKET)
            .join("documents");
        let names: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names, ["1.md"], "one document");
        fs::read_to_string(dir.join("1.md")).unwrap()
    }

    /// The last PLAN comment: a pointer to the plan's document, within its
    /// bound (OWL-350).
    pub(super) fn plan_pointer(&self) -> String {
        let pointer = self.marked("[owlshift] PLAN");
        assert!(pointer.chars().count() <= MAX_POINTER_CHARS, "{pointer}");
        assert!(
            pointer.contains("the plan and its review log, `tickets/DEMO-1/documents/1.md`."),
            "{pointer}"
        );
        pointer
    }
}

/// S9's design half: a `standard` ticket's dispatch from Ready goes to
/// Design; the plan, kept in `plan.json`, is reviewed by the critic and
/// conformance, both on Claude Code (OWL-347); a clean pass posts the PLAN
/// comment and the Build follows the plan from its base, dispatched again.
#[test]
fn a_standard_ticket_is_planned_and_reviewed_before_its_build() {
    let bench = Bench::of("standard");
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

    let plan = bench.plan_document();
    for text in [
        "# Plan · DEMO-1\n\nVersion 1, after design review pass 1.",
        "**Goal.** Add GREETING.md saying Hello.",
        "**Steps.**\n1. Add GREETING.md. Test: The gate's grep for Hello.",
        "## Review log\n\n### Design review, pass 1",
        "- critic on Claude Code (its default model",
    ] {
        assert!(plan.contains(text), "{text}\n---\n{plan}");
    }
    assert!(!plan.contains("standing in"), "{plan}");
    let pointer = bench.plan_pointer();
    for text in [
        "**Plan version 1, design review pass 1:**",
        "First version of the plan.",
        "**Forks:** 0. **Findings open:** 0.",
        "**Next.** The Build follows this plan.",
    ] {
        assert!(pointer.contains(text), "{text}\n---\n{pointer}");
    }
    assert!(!pointer.contains("**Goal.**"), "{pointer}");
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
    assert_eq!(passes[0]["reviewers"][0]["harness"], "claude");
    assert!(passes[0]["reviewers"][0].get("stand_in").is_none());
    assert_eq!(passes[0]["reviewers"][0]["same_model"], true);
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

/// The critic runs on Claude Code, as conformance does, on the standard
/// tier when no other model is configured (OWL-347); the arbiter confirms
/// one finding and rejects the other; the design run fixes the
/// confirmed one alone; the next pass is clean, and the PLAN comment lists
/// the rejected finding with its reason.
#[test]
fn the_arbiter_confirms_a_finding_the_design_fixes_and_rejects_another() {
    let bench = Bench::of("standard");
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
    for seat in &routed[1..3] {
        assert_eq!(
            (seat.harness, seat.tier),
            (HarnessKind::Claude, Tier::Standard)
        );
    }
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
    assert_eq!(first[0].reviewer.harness, HarnessKind::Claude);
    assert!(first[0].reviewer.same_model);
    assert!(review.passes[1].findings.is_empty());
    let plan = bench.plan_document();
    assert!(
        plan.contains(
            "configured)\n\n**Findings rejected.**\n- F2, major, critic on Claude Code: \
             GREETING.md needs a licence header. Rejected: Checked plan step 1 for F2.\n\n\
             ### Design review, pass 2"
        ),
        "{plan}"
    );
    assert!(plan.contains("### Design review, pass 1"), "{plan}");
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
    let bench = Bench::of("standard");
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
    let bench = Bench::of("standard");
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
    let plan = bench.plan_document();
    assert!(
        plan.contains("Rejected: No file of the repository has one."),
        "{plan}"
    );
}

/// The bound: a `major` still confirmed after the second pass goes to the
/// resolver first (OWL-342), told its finding, with the arbiter's ruling,
/// and the plan; passed on, it goes to the decider, one question; a `minor`
/// is noted in the PLAN comment. After the answer, the design runs with it
/// in its thread, and the one more pass it buys is clean, so the Build
/// follows.
#[test]
fn a_finding_two_passes_left_goes_to_the_decider_and_buys_one_more_pass() {
    let bench = Bench::of("standard");
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
        bench.read(&passed_on(&["Q1"])),
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
    let resolver = bench.last_of(Role::Resolver);
    let asked = resolver.asked_by.as_ref().unwrap();
    assert_eq!(asked.role, Role::DesignReview);
    assert!(asked.plan.is_some() && asked.diff.is_none());
    assert_eq!(asked.findings.len(), 2);
    assert!(asked.findings[0].ruling.is_some());
    assert_eq!(resolver.resolve[0].finding.as_ref().unwrap().as_str(), "F1");
    let plan = bench.plan_document();
    for text in [
        "F1, major, critic on Claude Code: Step 1's test still does not check the word. (asked \
         below)",
        "F2, minor, critic on Claude Code: The goal could name the file's path. (noted for the \
         Build)",
    ] {
        assert!(plan.contains(text), "{text}\n---\n{plan}");
    }
    let pointer = bench.plan_pointer();
    for text in [
        "**Findings open:** 2.",
        "**Next.** The findings left are settled by DECISION comments or by the decider's",
    ] {
        assert!(pointer.contains(text), "{text}\n---\n{pointer}");
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
    // The round is answered: it reads as its readings (OWL-359).
    assert!(latest_ask(after).is_empty());
    assert_eq!(last_answered(after), ["Q1"]);
    let review = bench.record().review.unwrap();
    assert_eq!(review.passes.len(), 4);
    assert_eq!(review.passes[2].pass.get(), 3);
}

/// A plan of two steps, and the one its revision changes step 2 of.
const TWO_STEPS: &str = r#"{"goal":"Add GREETING.md saying Hello.",
"criteria":["GREETING.md says Hello."],
"approach":"Write GREETING.md with the line the gate greps.",
"steps":[{"what":"Add GREETING.md.","test":"The gate's grep for Hello."},
{"what":"Say Hello in it.","test":"The gate's grep."}]}"#;
const TWO_STEPS_REVISED: &str = r#"{"goal":"Add GREETING.md saying Hello.",
"criteria":["GREETING.md says Hello."],
"approach":"Write GREETING.md with the line the gate greps.",
"steps":[{"what":"Add GREETING.md.","test":"The gate's grep for Hello."},
{"what":"Say Hello in it.","test":"The gate's grep for the word Hello."}]}"#;

/// OWL-356: a Design review pass whose only confirmed finding is `minor`
/// sends the plan to Build, not back to Design; the PLAN lists it noted,
/// and the Build's brief carries it as a note.
#[test]
fn a_design_review_pass_with_only_minors_goes_to_build_with_its_notes() {
    let bench = Bench::of("standard");
    let replies = vec![
        bench.read(&designed(PLAN)),
        bench.read(&reviewed(&[(
            "minor",
            "Step 1 could name the file's encoding.",
        )])),
        bench.read(&reviewed(&[])),
        bench.read(&ruled(&["confirmed"])),
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
            Role::Build,
            Role::Verify
        ]
    );
    let pass = &bench.decisions("review")[1];
    assert_eq!(
        (&pass["review"], &pass["outcome"], &pass["next"]),
        (&json!("pass"), &json!("passed"), &json!("build"))
    );
    let notes = bench.last_of(Role::Build).findings.expect("the notes");
    assert_eq!(notes.mode, FindingsMode::Note);
    let claims: Vec<&str> = notes.findings.iter().map(|f| f.claim.as_str()).collect();
    assert_eq!(claims, ["Step 1 could name the file's encoding."]);
    let plan = bench.plan_document();
    assert!(
        plan.contains(
            "F1, minor, critic on Claude Code: Step 1 could name the file's encoding. (noted \
             for the Build)"
        ),
        "{plan}"
    );
}

/// OWL-356: the pass after a revision is told what changed since pass 1,
/// the step whose text is new and the parts that changed, and pass 1's
/// findings, the major fixed and the minor noted; the Build gets the note.
#[test]
fn a_pass_after_a_revision_reviews_the_changed_steps_with_the_earlier_findings() {
    let bench = Bench::of("standard");
    let replies = vec![
        bench.read(&designed(TWO_STEPS)),
        bench.read(&reviewed(&[
            ("major", "Step 2's test does not check the word."),
            ("minor", "The goal could name the file's path."),
        ])),
        bench.read(&reviewed(&[])),
        bench.read(&ruled(&["confirmed", "confirmed"])),
        bench.read(&designed(TWO_STEPS_REVISED)),
        bench.read(&reviewed(&[])),
        bench.read(&reviewed(&[])),
        bench.built(),
    ];
    let (outcome, printed) = bench.run(replies, None);
    outcome.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    let briefs = bench.briefs();
    let revision = briefs[4].findings.as_ref().expect("the major to fix");
    let ids: Vec<&str> = revision.findings.iter().map(|f| f.id.as_str()).collect();
    assert_eq!(ids, ["F1"], "the minor is not the design's");
    assert!(briefs[1].review.as_ref().unwrap().since.is_none());
    for brief in &briefs[5..7] {
        let review = brief.review.as_ref().unwrap();
        assert_eq!(review.pass.get(), 2);
        let since = review.since.as_ref().expect("what changed");
        let steps: Vec<u32> = since.steps.iter().map(|n| n.get()).collect();
        assert_eq!(
            (
                since.pass.get(),
                since.parts.as_slice(),
                steps,
                since.removed
            ),
            (1, &[PlanPart::Steps][..], vec![2], 1)
        );
        let earlier: Vec<(&str, Option<FindingOutcome>)> = review
            .earlier
            .iter()
            .map(|e| (e.kept.finding.id.as_str(), e.kept.outcome))
            .collect();
        assert_eq!(earlier, [("F1", Some(FindingOutcome::Fixed)), ("F2", None)]);
    }
    let notes = bench.last_of(Role::Build).findings.expect("the note");
    assert_eq!(
        (notes.mode, notes.findings[0].claim.as_str()),
        (FindingsMode::Note, "The goal could name the file's path.")
    );
    let review = bench.record().review.unwrap();
    assert_eq!(
        review.passes[1].plan.as_ref().map(|p| p.steps.len()),
        Some(2)
    );
    // The PLAN counts pass 1's note among the findings left.
    let pointer = bench.plan_pointer();
    assert!(pointer.contains("**Findings open:** 1."), "{pointer}");
}

/// A `risky` ticket's plan approval (OWL-32): the clean pass of its three
/// reviewers asks the decider, its fork as an option; the base moves while
/// it waits; the approval is kept with the decider's comment and the plan's
/// hash, and the Build starts from the base fetched at its dispatch.
#[test]
fn a_risky_plan_is_approved_and_its_build_starts_from_the_moved_base() {
    let bench = Bench::of("risky");
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
        ["Approve the plan as it is", "Change it"]
    );
    // On a tracker with documents, the fork is a line under the question,
    // linking to the plan's document (OWL-352).
    let round = bench
        .comments()
        .into_iter()
        .rfind(|body| body.starts_with("[owlshift] QUESTIONS"))
        .expect("the approval round");
    assert!(
        round.contains(
            "**The plan's forks**, each with its choice:\n- Fork 1: Markdown. · its options"
        ),
        "{round}"
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
            .contains("**Next.** The plan waits for its approval")
    );
    // `risky` forces `always`: the approval is the decider's alone, with no
    // resolver run (OWL-342).
    assert!(!bench.roles().contains(&Role::Resolver));
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
    let bench = Bench::of("risky");
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
    // The Markdown tracker folds nothing, and says nothing of it (OWL-353).
    let events = bench.events();
    assert!(
        data_of(&events, EventKind::TrackerWrite, "action")
            .iter()
            .all(|write| write["action"] != "resolve")
    );
    assert!(
        data_of(&events, EventKind::Warning, "what")
            .iter()
            .all(|warning| warning["what"] != "comment_not_folded")
    );
}

/// OWL-353's acceptance on a tracker that resolves comments: after the two
/// rounds above, only round 2's QUESTIONS and the live PLAN stay unfolded.
/// The first plan, round 1's QUESTIONS and its RESUME are each folded once
/// by the comment that supersedes them; a refused fold is a warning, and
/// the command goes on as on the Markdown tracker.
#[test]
fn superseded_comments_fold_where_the_tracker_resolves_them() {
    let bench = Bench::of("risky");
    bench.fold.borrow_mut().on = true;
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
    // Nothing earlier to fold.
    assert!(bench.fold.borrow().resolved.is_empty());
    let kinds = || -> Vec<(String, String)> {
        MarkdownTracker::new(&bench.remote.checkout)
            .comments(&ticket())
            .unwrap()
            .into_iter()
            .filter_map(|c| {
                let header = MarkedComment::parse(&c.body).ok()??.header;
                Some((c.id, header.render()))
            })
            .collect()
    };
    let first_plan = kinds()
        .into_iter()
        .find(|(_, header)| header == "[owlshift] PLAN")
        .unwrap()
        .0;
    bench.fold.borrow_mut().refuse = Some(first_plan.clone());
    bench.answer("Use plain text instead.");
    let replies = vec![
        bench.read(&check(&[("Q1", "answered", "Plain text instead.")])),
        bench.read(&answered("changed", Some(REVISED))),
        bench.read(&reviewed(&[])),
        bench.read(&reviewed(&[])),
        bench.read(&reviewed(&[])),
    ];
    let (outcome, printed) = bench.continue_ticket(replies);
    assert!(
        matches!(&outcome, Err(Stop::NeedsInput { posted: Ok(round), .. }) if round.get() == 2),
        "{outcome:?}\n{printed}"
    );
    let marked = kinds();
    let id = |header: &str| -> Vec<String> {
        marked
            .iter()
            .filter(|(_, h)| h == header)
            .map(|(id, _)| id.clone())
            .collect()
    };
    let (plans, round_1, resume) = (
        id("[owlshift] PLAN"),
        id("[owlshift] QUESTIONS · round 1"),
        id("[owlshift] RESUME · round 1"),
    );
    assert_eq!(
        (plans.len(), round_1.len(), resume.len()),
        (2, 1, 1),
        "{marked:?}"
    );
    // Round 1's QUESTIONS by its RESUME, the first PLAN (refused) by the
    // second, the RESUME by round 2's QUESTIONS.
    assert_eq!(
        bench.fold.borrow().resolved,
        [round_1[0].clone(), resume[0].clone()]
    );
    let events = bench.events();
    let resolved: Vec<Value> = data_of(&events, EventKind::TrackerWrite, "action")
        .into_iter()
        .filter(|write| write["action"] == "resolve")
        .collect();
    assert_eq!(resolved.len(), 2);
    assert_eq!(resolved[0]["comment"], json!(round_1[0]));
    assert_eq!(resolved[0]["by"], json!(resume[0]));
    let warnings: Vec<Value> = data_of(&events, EventKind::Warning, "what")
        .into_iter()
        .filter(|warning| warning["what"] == "comment_not_folded")
        .collect();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert_eq!(warnings[0]["comment"], json!(first_plan));
    assert_eq!(warnings[0]["by"], json!(plans[1]));
    assert!(bench.record().plan.unwrap().approval.is_none());
}

/// A Build that cannot follow its plan asks a `replan`; the resolver
/// settles it, and the ticket goes back to Design in the same command: the
/// new design run gets the old plan and the decision, its plan is reviewed
/// again, and the Build follows it.
#[test]
fn a_replan_the_resolver_settles_sends_the_ticket_back_to_design() {
    let bench = Bench::of("standard");
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

/// On an approved plan, a `replan` goes through the resolver first since
/// OWL-342; passed on, it goes to the decider, and its round returns to
/// Design.
#[test]
fn a_replan_on_an_approved_plan_goes_to_the_decider_and_returns_to_design() {
    let bench = Bench::of("risky");
    let replies = vec![
        bench.read(&designed(FORKED_WITH_ALTERNATIVE)),
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
        bench.read(&passed_on(&["Q1"])),
    ];
    let (outcome, printed) = bench.continue_ticket(replies);
    let stop = outcome.expect_err("the replan goes to the decider");
    assert!(
        matches!(&stop, Stop::NeedsInput { posted: Ok(round), .. } if round.get() == 2),
        "{stop:?}\n{printed}"
    );
    let record = bench.record();
    let kept = record.plan.as_ref().expect("plan.json");
    // The Build follows the plan without its forks and alternatives
    // (OWL-365); the resolver deciding its replan reads it whole.
    let build = bench.last_of(Role::Build);
    let given = build.plan.as_ref().expect("the plan the Build follows");
    let mut cut = kept.plan.clone();
    cut.forks.clear();
    cut.alternatives.clear();
    assert_eq!((&given.base, &given.plan), (&kept.base, &cut));
    let resolver = bench.last_of(Role::Resolver);
    let asked_by = resolver.asked_by.as_ref().unwrap();
    assert_eq!(asked_by.role, Role::Build);
    let whole = &asked_by.plan.as_ref().unwrap().plan;
    assert_eq!(whole, &kept.plan);
    assert_eq!((whole.forks.len(), whole.alternatives.len()), (1, 1));
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
    let bench = Bench::of("standard");
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

/// A design run that finds the premise false and asks (OWL-306, S8): its
/// question goes through the resolver first (OWL-342), which at the default
/// level cannot decide a re-scope and passes it on; the round returns to
/// Design, the QUESTIONS comment says so, and no plan is kept.
#[test]
fn a_false_premise_at_design_is_a_round_to_the_decider() {
    let bench = Bench::of("standard");
    let premise = r#"{"format":12,"status":"premise_false",
"summary":"The greeting file the ticket extends is not in the repository.",
"questions":[{"id":"Q1","category":"naming","context":"The ticket extends GREETING.md; no such file exists.",
"text":"Re-scope to create GREETING.md, or close the ticket?",
"options":[{"label":"Re-scope: create GREETING.md."}],"recommendation":{"option":"A","why":"Re-scope"}}]}"#;
    let (outcome, printed) = bench.run(
        vec![bench.read(premise), bench.read(&passed_on(&["Q1"]))],
        None,
    );
    let stop = outcome.expect_err("the premise goes to the decider");
    assert!(
        matches!(
            &stop,
            Stop::NeedsInput { status: owlshift_contracts::result::Status::PremiseFalse, posted: Ok(round), .. }
                if round.get() == 1
        ),
        "{stop:?}\n{printed}"
    );
    assert_eq!(bench.roles(), [Role::Design, Role::Resolver]);
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
    let bench = Bench::of("standard");
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

/// A design run's question on a trade-off nothing settles.
const TRADE_OFF: &str = r#"{"format":12,"status":"questions",
"summary":"The ticket leaves the greeting's format open.",
"questions":[{"id":"Q1","category":"approach","context":"The ticket says Hello; nothing says Markdown or plain text.",
"text":"Should GREETING.md be Markdown or plain text?",
"options":[{"label":"Markdown"},{"label":"Plain text"}],
"recommendation":{"option":"A","why":"The repository's docs are Markdown."}}]}"#;

/// The resolver's decision on it, on its own weighing (OWL-343).
const JUDGED: &str = r#"{"format":12,"status":"done","summary":"Weighed the format.",
"resolutions":[{"question":"Q1","outcome":"decided","category":"file_format","decision":"Markdown.",
"basis":"Option A: the docs are Markdown, so a reader expects it; the cost is none, as nothing renders it.",
"basis_kind":"judgment","option":"A"}]}"#;

/// The bench of `variant`, the project's delegation level `level`.
pub(super) fn at_level(variant: &str, level: &str) -> Bench {
    let bench = Bench::of(variant);
    bench.policy(&format!("resolver_delegation = \"{level}\""));
    bench
}

/// OWL-343's acceptance, a Design question on judgment: at `all`, the
/// resolver decides a trade-off on its own weighing, its DECISION comment
/// says so on its visible line, the design runs again with the decision and
/// the delivery report lists it first. At `discoverable` the same decision
/// is refused, the level allowing no judgment, and the question goes to the
/// decider.
#[test]
fn a_design_question_is_decided_on_judgment_at_all_and_not_below() {
    let bench = at_level("standard", "all");
    let replies = vec![
        bench.read(TRADE_OFF),
        bench.read(JUDGED),
        bench.read(&designed(PLAN)),
        bench.read(&reviewed(&[])),
        bench.read(&reviewed(&[])),
        bench.built(),
    ];
    let (outcome, printed) = bench.run(replies, None);
    outcome.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    assert_eq!(
        bench.roles()[..3],
        [Role::Design, Role::Resolver, Role::Design]
    );
    let told = bench.last_of(Role::Resolver);
    assert_eq!(told.asked_by.as_ref().unwrap().role, Role::Design);
    assert!(shape(&bench.briefs()[2]).contains(&"decision".to_owned()));
    let decision = bench.marked("[owlshift] DECISION");
    // Its line says it rests on judgment, the lead the level (OWL-351).
    assert!(
        decision.contains("as delegation level `all` allows")
            && decision
                .lines()
                .any(|line| line.starts_with("- **D1 · ") && line.ends_with("*(on judgment)*")),
        "{decision}"
    );
    let kept = &bench.record().questions.decisions[0];
    assert_eq!(kept.basis_kind, Some(BasisKind::Judgment));
    let report = bench.marked("[owlshift] DELIVERY");
    assert!(report.contains("(decided on judgment)"), "{report}");
    let resolved = data_of(&bench.events(), EventKind::Gate, "resolver");
    assert_eq!(resolved[0]["asked_by"], json!("design"));

    let bench = at_level("standard", "discoverable");
    let (outcome, printed) = bench.run(vec![bench.read(TRADE_OFF), bench.read(JUDGED)], None);
    let stop = outcome.expect_err("refused below `all`");
    assert!(
        matches!(&stop, Stop::NeedsInput { posted: Ok(round), .. } if round.get() == 1),
        "{stop:?}\n{printed}"
    );
    assert!(bench.record().questions.decisions.is_empty());
    let resolved = data_of(&bench.events(), EventKind::Gate, "resolver");
    assert_eq!(resolved[0]["refused"], json!(["Q1"]));
    assert!(
        !bench
            .comments()
            .iter()
            .any(|c| c.starts_with("[owlshift] DECISION"))
    );
}

/// OWL-343's acceptance, a residue finding decided: the finding two passes
/// left reaches the resolver, which has its fix applied; no round opens,
/// the next design run follows the decision, and the pass it buys is clean,
/// so the Build follows.
#[test]
fn a_residue_finding_decided_is_applied_by_the_next_design_run() {
    let bench = Bench::of("standard");
    let applied = r#"{"format":12,"status":"done","summary":"The plan does not check the word.",
"resolutions":[{"question":"Q1","outcome":"decided","category":"testing",
"decision":"Settle it: change step 1 so its test greps for the word Hello.",
"basis":"Plan step 1's test greps for the line, not the word, as the arbiter confirmed.",
"basis_kind":"fact","option":"A"}]}"#;
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
        bench.read(applied),
        bench.read(&designed(REVISED)),
        bench.read(&reviewed(&[])),
        bench.read(&reviewed(&[])),
        bench.built(),
    ];
    let (outcome, printed) = bench.run(replies, None);
    outcome.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    let roles = bench.roles();
    assert_eq!(roles[8..10], [Role::Resolver, Role::Design]);
    let after = &bench.briefs()[9];
    assert!(after.findings.is_none() && !after.approval);
    assert!(shape(after).contains(&"decision".to_owned()));
    assert!(
        !bench
            .comments()
            .iter()
            .any(|c| c.starts_with("[owlshift] QUESTIONS"))
    );
    let record = bench.record();
    assert!(
        record
            .plan
            .unwrap()
            .plan
            .approach
            .contains("the word checked")
    );
    let passes: Vec<(Stage, u32)> = record
        .review
        .unwrap()
        .passes
        .iter()
        .map(|pass| (pass.stage, pass.pass.get()))
        .collect();
    assert_eq!(
        passes,
        [
            (Stage::DesignReview, 1),
            (Stage::DesignReview, 2),
            (Stage::DesignReview, 3),
            (Stage::Verify, 1)
        ]
    );
}

/// Under `on-fork`, the plan's approval reaches the resolver (OWL-342): its
/// option A approves the plan as it is, kept in `plan.json` with the
/// DECISION comment, and the Build follows it in the same command.
#[test]
fn the_resolver_approves_a_forked_plan_under_on_fork() {
    let bench = at_level("standard", "all");
    let config = bench.remote.checkout.join("owlshift.toml");
    let text = fs::read_to_string(&config).unwrap();
    fs::write(
        &config,
        text.replace("plan_approval = \"never\"", "plan_approval = \"on-fork\""),
    )
    .unwrap();
    let approves = r#"{"format":12,"status":"done","summary":"The fork's choice is the repository's.",
"resolutions":[{"question":"Q1","outcome":"decided","category":"approval",
"decision":"Approve the plan as it is.",
"basis":"The plan's fork keeps Markdown, as the repository's docs are; plain text buys nothing.",
"basis_kind":"judgment","option":"A"}]}"#;
    let replies = vec![
        bench.read(&designed(FORKED_WITH_ALTERNATIVE)),
        bench.read(&reviewed(&[])),
        bench.read(&reviewed(&[])),
        bench.read(approves),
        bench.built(),
    ];
    let (outcome, printed) = bench.run(replies, None);
    outcome.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    assert_eq!(
        bench.roles()[..5],
        [
            Role::Design,
            Role::DesignReview,
            Role::DesignReview,
            Role::Resolver,
            Role::Build
        ]
    );
    let told = bench.last_of(Role::Resolver);
    assert_eq!(
        told.resolve[0].options[0].label,
        "Approve the plan as it is"
    );
    assert!(told.asked_by.as_ref().unwrap().plan.is_some());
    let record = bench.record();
    // Verify reviews against the whole plan, which the Build got without
    // its fork and alternative (OWL-365).
    let kept = &record.plan.as_ref().unwrap().plan;
    let verify = bench.last_of(Role::Verify);
    assert_eq!(&verify.plan.as_ref().unwrap().plan, kept);
    let build = bench.last_of(Role::Build);
    let given = &build.plan.as_ref().unwrap().plan;
    assert!(given.forks.is_empty() && given.alternatives.is_empty());
    assert_eq!(given.steps, kept.steps);
    let decision = &record.questions.decisions[0];
    let approval = record.plan.unwrap().approval.expect("the approval is kept");
    assert_eq!(approval.comment, decision.comment);
    let approved = bench
        .decisions("plan")
        .into_iter()
        .find(|event| event["plan"] == "approved")
        .unwrap();
    assert_eq!(approved["by"], json!("resolver"));
}

/// A false premise at `all` (OWL-343): the resolver may decide the re-scope,
/// the design then planning the ticket as re-scoped; the closure, the
/// runner's own last option, is refused and goes to the decider.
#[test]
fn a_false_premise_rescope_is_decided_and_its_closure_refused() {
    let premise = r#"{"format":12,"status":"premise_false",
"summary":"The greeting file the ticket extends is not in the repository.",
"questions":[{"id":"Q1","category":"premise","context":"The ticket extends GREETING.md; no such file exists.",
"text":"Re-scope to create GREETING.md, or close the ticket?",
"options":[{"label":"Re-scope: create GREETING.md."}],"recommendation":{"option":"A","why":"Re-scope"}}]}"#;
    let deciding = |option: &str| {
        format!(
            r#"{{"format":12,"status":"done","summary":"Weighed the premise.",
"resolutions":[{{"question":"Q1","outcome":"decided","category":"scope","decision":"Option {option}.",
"basis":"The ticket wants a greeting; creating the file delivers it at no extra cost.",
"basis_kind":"judgment","option":"{option}"}}]}}"#
        )
    };
    let bench = at_level("standard", "all");
    let replies = vec![
        bench.read(premise),
        bench.read(&deciding("A")),
        bench.read(&designed(PLAN)),
        bench.read(&reviewed(&[])),
        bench.read(&reviewed(&[])),
        bench.built(),
    ];
    let (outcome, printed) = bench.run(replies, None);
    outcome.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    assert_eq!(
        bench.roles()[..3],
        [Role::Design, Role::Resolver, Role::Design]
    );
    let told = bench.last_of(Role::Resolver);
    assert!(told.asked_by.as_ref().unwrap().premise_false);
    assert_eq!(
        told.resolve[0].options[1].label,
        "Close the ticket: its premise is false"
    );

    let bench = at_level("standard", "all");
    let (outcome, printed) = bench.run(vec![bench.read(premise), bench.read(&deciding("B"))], None);
    let stop = outcome.expect_err("the closure goes to the decider");
    assert!(
        matches!(
            &stop,
            Stop::NeedsInput {
                status: owlshift_contracts::result::Status::PremiseFalse,
                ..
            }
        ),
        "{stop:?}\n{printed}"
    );
    let resolved = data_of(&bench.events(), EventKind::Gate, "resolver");
    assert_eq!(resolved[0]["refused"], json!(["Q1"]));
    assert!(bench.record().questions.decisions.is_empty());
}

/// OWL-350: a ticket through Design has one document, "Plan · DEMO-1",
/// updated at each PLAN comment, and short PLAN comments that point to it:
/// the first version, then, once the decider's reply changed the plan, the
/// second, naming the parts that changed. The approval still binds the
/// plan in `plan.json`.
#[test]
fn the_plan_lives_in_one_document_updated_at_each_pass() {
    let bench = Bench::of("risky");
    let replies = vec![
        bench.read(&designed(FORKED)),
        bench.read(&reviewed(&[])),
        bench.read(&reviewed(&[])),
        bench.read(&reviewed(&[])),
    ];
    let (outcome, printed) = bench.run(replies, None);
    assert!(
        matches!(outcome, Err(Stop::NeedsInput { .. })),
        "{outcome:?}\n{printed}"
    );
    let first = bench.plan_pointer();
    for text in [
        "**Plan version 1, design review pass 1:**",
        "First version of the plan.",
        "**Forks:** 1. **Findings open:** 0.",
        "**Next.** The plan waits for its approval",
    ] {
        assert!(first.contains(text), "{text}\n---\n{first}");
    }
    assert!(bench.plan_document().contains("Fork 1: Markdown."));

    bench.answer("Use plain text instead.");
    let replies = vec![
        bench.read(&check(&[("Q1", "answered", "Plain text instead.")])),
        bench.read(&answered("changed", Some(REVISED))),
        bench.read(&reviewed(&[])),
        bench.read(&reviewed(&[])),
        bench.read(&reviewed(&[])),
    ];
    let (outcome, printed) = bench.continue_ticket(replies);
    assert!(
        matches!(outcome, Err(Stop::NeedsInput { .. })),
        "{outcome:?}\n{printed}"
    );
    let plans: Vec<String> = bench
        .comments()
        .into_iter()
        .filter(|comment| comment.starts_with("[owlshift] PLAN"))
        .collect();
    assert_eq!(plans.len(), 2, "{plans:?}");
    let second = bench.plan_pointer();
    for text in [
        "**Plan version 2, design review pass 2:**",
        "**Changed since version 1.**\n- The approach changed.\n- The steps changed.\n- The \
         forks changed.",
        "**Forks:** 0.",
    ] {
        assert!(second.contains(text), "{text}\n---\n{second}");
    }
    let document = bench.plan_document();
    for text in [
        "Version 2, after design review pass 2.",
        "the word checked",
        "### Design review, pass 1",
        "### Design review, pass 2",
    ] {
        assert!(document.contains(text), "{text}\n---\n{document}");
    }
    assert!(!document.contains("Fork 1: Markdown."), "{document}");
    let kept = bench.record().plan.unwrap();
    let published = kept.published.expect("the published plan");
    assert_eq!(
        (published.document.as_str(), published.version.get()),
        ("1", 2)
    );
    assert!(kept.approval.is_none());
}

/// OWL-350: a PLAN comment that cannot be posted once its document is made
/// keeps the document's id in the ref, so `continue` runs the pass again
/// and updates that same document, the first version still.
#[test]
fn a_plan_comment_refused_after_its_document_keeps_one_document() {
    let bench = Bench::of("standard");
    bench.refuse.set(Some("[owlshift] PLAN"));
    let replies = vec![
        bench.read(&designed(PLAN)),
        bench.read(&reviewed(&[])),
        bench.read(&reviewed(&[])),
    ];
    let (outcome, printed) = bench.run(replies, None);
    assert!(outcome.is_err(), "{printed}");
    bench.plan_document();
    let published = bench.record().plan.unwrap().published;
    assert_eq!(published.map(|p| p.document), Some("1".to_owned()));
    bench.refuse.set(None);
    let replies = vec![
        bench.read(&reviewed(&[])),
        bench.read(&reviewed(&[])),
        bench.built(),
    ];
    let (outcome, printed) = bench.continue_ticket(replies);
    outcome.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    bench.plan_document();
    let pointer = bench.plan_pointer();
    assert!(pointer.contains("First version of the plan."), "{pointer}");
}

/// A `standard` ticket whose design asked a question the decider answered:
/// the ref waits at Design in round 1, and the answer is posted.
pub(super) fn answered_design() -> Bench {
    let bench = Bench::of("standard");
    let (outcome, printed) = bench.run(
        vec![bench.read(TRADE_OFF), bench.read(&passed_on(&["Q1"]))],
        None,
    );
    let stop = outcome.expect_err("the question goes to the decider");
    assert!(
        matches!(stop, Stop::NeedsInput { .. }),
        "{stop:?}\n{printed}"
    );
    bench.answer("Q1: Markdown.");
    bench
}

/// The replies of a design round's answer check, the design, its two
/// reviews and the Build.
pub(super) fn design_replies(bench: &Bench, check_first: bool) -> Vec<Reply> {
    let mut replies = Vec::new();
    if check_first {
        replies.push(bench.read(&check(&[("Q1", "answered", "Markdown.")])));
    }
    replies.extend([
        bench.read(&designed(PLAN)),
        bench.read(&reviewed(&[])),
        bench.read(&reviewed(&[])),
        bench.built(),
    ]);
    replies
}

impl Bench {
    /// Moves the bench's clock `minutes` on, for the next watch pass.
    pub(super) fn wait(&self, minutes: i64) {
        self.waited
            .set(self.waited.get() + SignedDuration::from_mins(minutes));
    }

    /// The ticket ref's state: its status and its round.
    pub(super) fn state(&self) -> (Status, u32) {
        let state = TicketState::try_from(&self.record().state).unwrap();
        (state.status(), state.round())
    }

    pub(super) fn open_reads(&self) -> u32 {
        *self.github.open_reads.lock().unwrap()
    }

    pub(super) fn resumes(&self) -> usize {
        self.comments()
            .iter()
            .filter(|comment| comment.starts_with("[owlshift] RESUME"))
            .count()
    }

    /// The `dispatch` events of the event log so far.
    pub(super) fn dispatches(&self) -> usize {
        self.events()
            .iter()
            .filter(|event| event.kind == EventKind::Dispatch)
            .count()
    }

    pub(super) fn answer_checks(&self) -> usize {
        self.roles()
            .iter()
            .filter(|role| **role == Role::AnswerCheck)
            .count()
    }

    /// The `stage` of each `watch: continue` decision that names one, the
    /// resumes of a stage left (OWL-273, OWL-340), oldest first.
    pub(super) fn watch_resumes(&self) -> Vec<String> {
        self.decisions("watch")
            .iter()
            .filter(|event| event["watch"] == "continue")
            .filter_map(|event| event["stage"].as_str().map(str::to_owned))
            .collect()
    }
}

/// OWL-340's acceptance, the gap: the decider answers, the answer check
/// resumes the ticket and posts the RESUME, and the dispatch's forge read is
/// refused once (LOC-1406's transient 401). The ref stays at Design, round 1,
/// shown In Progress with no run, and a later watch pass starts the Design
/// run with no `continue` typed, without a second answer check or RESUME.
#[test]
fn watch_resumes_a_stage_a_refusal_stopped_right_after_its_answer_check() {
    let bench = answered_design();
    *bench.github.refuse_open.lock().unwrap() = 1;
    let reads = bench.open_reads();
    let mut pass = 0;
    let mut sleep = |_: Duration| {
        pass += 1;
        match pass {
            1 => {
                assert_eq!(bench.stage(), "In Progress");
                assert_eq!(bench.state(), (Status::Active(Stage::Design), 1));
                let record = bench.record();
                assert!(record.questions.checked_through.is_some());
                assert_eq!(bench.resumes(), 1);
                assert_eq!(bench.answer_checks(), 1);
                assert_eq!(bench.open_reads(), reads + 1);
                assert_eq!(bench.dispatches(), 1, "the first design's only");
                bench.wait(1);
                true
            }
            _ => false,
        }
    };
    let (outcomes, lines, printed) = bench.watch(design_replies(&bench, true), &mut sleep);
    // Pass 1 ends on the refusal, pass 2 runs the stages on to Build.
    assert!(
        matches!(&outcomes[0], Err(Stop::Refused(m)) if m.contains("GitHub (demo/project)")
            && m.contains("HTTP 401 Bad credentials")),
        "{outcomes:?}\n{lines}\n{printed}"
    );
    assert!(
        outcomes[1..].iter().all(Result::is_ok),
        "{outcomes:?}\n{lines}\n{printed}"
    );
    assert_eq!(bench.watch_resumes(), ["design"]);
    assert_eq!(bench.answer_checks(), 1);
    assert_eq!(bench.resumes(), 1);
    assert_eq!(
        bench.roles(),
        [
            Role::Design,
            Role::Resolver,
            Role::AnswerCheck,
            Role::Design,
            Role::DesignReview,
            Role::DesignReview,
            Role::Build,
            Role::Verify
        ]
    );
    assert_eq!(bench.stage(), "In Review");
}

/// OWL-340: a forge that keeps refusing costs no answer check and no RESUME
/// at a pass. The resume is refused with the ref as it was, so it is held
/// ten minutes: the pass inside the hold reads neither the stage nor the
/// forge, and the one after it tries again.
#[test]
fn a_forge_that_keeps_refusing_costs_neither_an_answer_check_nor_a_resume() {
    let bench = answered_design();
    *bench.github.refuse_open.lock().unwrap() = 100;
    let mut pass = 0;
    let seen = Cell::new((0, 0));
    let mut sleep = |_: Duration| {
        pass += 1;
        let now = (bench.open_reads(), bench.stage_reads.get());
        match pass {
            // The answer check, the RESUME, the refusal.
            1 => {
                assert_eq!((bench.answer_checks(), bench.resumes()), (1, 1));
                bench.wait(1);
            }
            // Resumed, refused again, held.
            2 => {
                assert!(now.0 > seen.get().0 && now.1 > seen.get().1);
                bench.wait(9);
            }
            // Nine minutes on, inside the hold: nothing read, nothing run.
            3 => {
                assert_eq!(now, seen.get());
                bench.wait(2);
            }
            // Eleven minutes after pass 2: tried again, refused again.
            4 => {
                assert!(now.0 > seen.get().0, "{now:?} {:?}", seen.get());
                return false;
            }
            _ => unreachable!(),
        }
        seen.set(now);
        true
    };
    let (outcomes, lines, printed) = bench.watch(
        vec![bench.read(&check(&[("Q1", "answered", "Markdown.")]))],
        &mut sleep,
    );
    assert_eq!(outcomes.len(), 3, "{outcomes:?}\n{lines}\n{printed}");
    assert!(
        outcomes
            .iter()
            .all(|outcome| matches!(outcome, Err(Stop::Refused(_)))),
        "{outcomes:?}"
    );
    assert_eq!((bench.answer_checks(), bench.resumes()), (1, 1));
    assert_eq!(bench.state(), (Status::Active(Stage::Design), 1));
    assert!(
        lines.contains("DEMO-1 is at Design as it was before that resume: watch resumes it again"),
        "{lines}"
    );
    assert_eq!(bench.stage(), "In Progress");
}

/// OWL-340: a ticket the tracker shows in another stage than working, such
/// as a person's move to review, is left alone, and its stage is not read
/// again for ten minutes while its ref does not move.
#[test]
fn a_stage_left_shown_elsewhere_is_left_alone_and_its_stage_read_rarely() {
    let bench = answered_design();
    *bench.github.refuse_open.lock().unwrap() = 1;
    let mut pass = 0;
    let reads = Cell::new(0);
    let mut sleep = |_: Duration| {
        pass += 1;
        match pass {
            1 => {
                // The refusal left it working; a person moves it.
                assert_eq!(bench.state(), (Status::Active(Stage::Design), 1));
                MarkdownTracker::new(&bench.remote.checkout)
                    .set_stage(&ticket(), "In Review")
                    .unwrap();
                bench.wait(1);
            }
            // Read once, found elsewhere.
            2 => {
                reads.set(bench.stage_reads.get());
                bench.wait(1);
            }
            // Not read again, nothing run.
            3 => {
                assert_eq!(bench.stage_reads.get(), reads.get());
                return false;
            }
            _ => unreachable!(),
        }
        true
    };
    let (outcomes, lines, printed) = bench.watch(
        vec![bench.read(&check(&[("Q1", "answered", "Markdown.")]))],
        &mut sleep,
    );
    assert_eq!(outcomes.len(), 1, "{outcomes:?}\n{lines}\n{printed}");
    assert!(bench.watch_resumes().is_empty());
    assert_eq!(bench.answer_checks(), 1);
    assert_eq!(bench.stage(), "In Review");
    assert_eq!(bench.state(), (Status::Active(Stage::Design), 1));
}

/// A reset long past at the bench's clock.
const LONG_PAST: &str = "2026-10-01T00:00:00Z";
/// A reset ahead of the bench's clock for the whole test.
const FAR_AHEAD: &str = "2026-10-03T18:00:00Z";

/// A ticket whose design round was answered and whose `stage` a usage limit
/// then cut off (round 1, the stage shown working), and the replies that
/// resume it.
fn cut_off_at(stage: Stage, resets: &str) -> (Bench, Vec<Reply>) {
    let bench = answered_design();
    let cut = |replies: Vec<Reply>| {
        let (outcome, printed) = bench.continue_ticket(replies);
        assert!(
            matches!(outcome, Err(Stop::UsageLimit { .. })),
            "{outcome:?}\n{printed}"
        );
    };
    let check = bench.read(&check(&[("Q1", "answered", "Markdown.")]));
    let resume = match stage {
        Stage::DesignReview => {
            cut(vec![
                check,
                bench.read(&designed(PLAN)),
                bench.limited_until(resets),
            ]);
            vec![
                bench.read(&reviewed(&[])),
                bench.read(&reviewed(&[])),
                bench.built(),
            ]
        }
        Stage::Build => {
            cut(vec![
                check,
                bench.read(&designed(PLAN)),
                bench.read(&reviewed(&[])),
                bench.read(&reviewed(&[])),
                bench.limited_until(resets),
            ]);
            vec![bench.built()]
        }
        Stage::Verify => {
            bench.verify_plays.set(true);
            cut(vec![
                check,
                bench.read(&designed(PLAN)),
                bench.read(&reviewed(&[])),
                bench.read(&reviewed(&[])),
                bench.built(),
                bench.limited_until(resets),
            ]);
            vec![bench.read(&verify_scenarios::found(&[]))]
        }
        other => panic!("{other:?}"),
    };
    assert_eq!(bench.state(), (Status::Active(stage), 1));
    assert_eq!(bench.stage(), "In Progress");
    (bench, resume)
}

/// OWL-340: a ticket at Design review, Build or Verify with an answered
/// round, which a usage limit cut off and whose stage still shows working, is
/// resumed by a watch pass with no `continue` typed and no second answer
/// check.
#[test]
fn watch_resumes_design_review_build_and_verify_cut_off_after_a_round() {
    for (stage, name) in [
        (Stage::DesignReview, "design_review"),
        (Stage::Build, "build"),
        (Stage::Verify, "verify"),
    ] {
        let (bench, replies) = cut_off_at(stage, LONG_PAST);
        let checks = bench.answer_checks();
        let (outcomes, lines, printed) = bench.watch(replies, &mut |_| false);
        assert!(
            matches!(&outcomes[..], [Ok(_)]),
            "{stage:?}: {outcomes:?}\n{lines}\n{printed}"
        );
        assert_eq!(bench.watch_resumes(), [name]);
        assert_eq!(bench.answer_checks(), checks, "{stage:?}");
        assert_eq!(bench.stage(), "In Review", "{stage:?}");
    }
}

/// OWL-340, a usage limit met again by the resume. With a reset still ahead,
/// every continue is paused until it: the next pass starts no harness and
/// reads no stage, and a pass after the reset resumes. With a reset already
/// past, nothing pauses: the resume is held ten minutes like any continue
/// that left the ref as it was.
#[test]
fn a_resume_meeting_the_usage_limit_again_waits_for_its_reset_or_ten_minutes() {
    // A reset ahead.
    let (bench, mut replies) = cut_off_at(Stage::Build, FAR_AHEAD);
    replies.insert(0, bench.limited_until(FAR_AHEAD));
    let (mut ran, mut reads) = (0, 0);
    let mut pass = 0;
    let (outcomes, lines, printed) = bench.watch(replies, &mut |_| {
        pass += 1;
        match pass {
            1 => {
                (ran, reads) = (bench.briefs().len(), bench.stage_reads.get());
                bench.wait(1);
            }
            2 => {
                assert_eq!(
                    (bench.briefs().len(), bench.stage_reads.get()),
                    (ran, reads)
                );
                // Past the reset.
                bench.wait(40 * 60);
            }
            _ => return false,
        }
        true
    });
    assert!(
        matches!(&outcomes[..], [Err(Stop::UsageLimit { .. }), Ok(_)]),
        "{outcomes:?}\n{lines}\n{printed}"
    );
    assert!(lines.contains("no ticket is continued before the usage limit resets"));
    assert_eq!(bench.watch_resumes(), ["build", "build"]);

    // A reset already past.
    let (bench, mut replies) = cut_off_at(Stage::Build, LONG_PAST);
    replies.insert(0, bench.limited_until(LONG_PAST));
    let (mut ran, mut reads) = (0, 0);
    let mut pass = 0;
    let (outcomes, lines, printed) = bench.watch(replies, &mut |_| {
        pass += 1;
        match pass {
            1 => {
                (ran, reads) = (bench.briefs().len(), bench.stage_reads.get());
                bench.wait(1);
            }
            2 => {
                assert_eq!(
                    (bench.briefs().len(), bench.stage_reads.get()),
                    (ran, reads)
                );
                bench.wait(11);
            }
            _ => return false,
        }
        true
    });
    assert!(
        matches!(&outcomes[..], [Err(Stop::UsageLimit { .. }), Ok(_)]),
        "{outcomes:?}\n{lines}\n{printed}"
    );
    assert!(lines.contains("DEMO-1 is at Build as it was before that resume"));
    assert_eq!(bench.watch_resumes(), ["build", "build"]);
}

/// OWL-340, the bound is the stage's: a Build watch resumes whose run fails
/// moves the ref each time, is resumed at the next pass while it can be, and
/// is parked by the second failed run; a parked ref is not resumed.
#[test]
fn a_resumed_build_that_fails_twice_is_parked_and_not_resumed_again() {
    let (bench, _) = cut_off_at(Stage::Build, LONG_PAST);
    let mut pass = 0;
    // The first resume fails once and meets the usage limit, which keeps the
    // failed run in the ref: the ref moved, so the next pass resumes it, and
    // its failed run is the second.
    let replies = vec![
        bench.read(FAILED),
        bench.limited_until(LONG_PAST),
        bench.read(FAILED),
    ];
    let (outcomes, lines, printed) = bench.watch(replies, &mut |_| {
        pass += 1;
        pass < 4
    });
    assert!(
        matches!(
            &outcomes[..],
            [Err(Stop::UsageLimit { .. }), Err(Stop::Parked { .. })]
        ),
        "{outcomes:?}\n{lines}\n{printed}"
    );
    assert!(matches!(
        bench.state().0,
        Status::Parked {
            at: Stage::Build,
            ..
        }
    ));
    assert_eq!(bench.watch_resumes(), ["build", "build"]);
}

/// OWL-340: a resumed Design run asks questions, and keeping the round fails
/// once its QUESTIONS comment is posted: the ticket is held until a person
/// acts, so later passes read no stage, run nothing and post nothing.
#[test]
fn a_resume_whose_state_was_not_kept_waits_for_a_person() {
    let bench = answered_design();
    *bench.github.refuse_open.lock().unwrap() = 1;
    let mark = Cell::new((0, 0, 0));
    let mut pass = 0;
    let (outcomes, lines, printed) = bench.watch(
        vec![
            bench.read(&check(&[("Q1", "answered", "Markdown.")])),
            bench.read(TRADE_OFF),
            bench.read(&passed_on(&["Q1"])),
        ],
        &mut |_| {
            pass += 1;
            let now = (
                bench.briefs().len(),
                bench.comments().len(),
                bench.stage_reads.get(),
            );
            match pass {
                1 => {
                    bench.lock_after.set(Some("[owlshift] QUESTIONS"));
                    bench.wait(1);
                }
                // The resume asked and its state was not kept.
                2 => {
                    mark.set(now);
                    bench.wait(11);
                }
                3 => {
                    assert_eq!(now, mark.get());
                    bench.wait(10);
                }
                4 => {
                    assert_eq!(now, mark.get());
                    return false;
                }
                _ => unreachable!(),
            }
            true
        },
    );
    assert_eq!(outcomes.len(), 2, "{outcomes:?}\n{lines}\n{printed}");
    assert!(
        matches!(&outcomes[1], Err(Stop::NotKept { .. })),
        "{outcomes:?}"
    );
    assert!(
        lines.contains("DEMO-1 waits for a person to act"),
        "{lines}"
    );
    assert_eq!(bench.state(), (Status::Active(Stage::Design), 1));
}

/// OWL-340: while another command holds the project's lock, a ticket shown
/// working is a run in flight. A pass ends at its snapshot with the busy
/// line: no stage read, no resume, no hold and no comment. The first pass
/// after the lock is released resumes it, with no ten-minute wait.
#[test]
fn a_run_in_flight_is_not_resumed_and_its_ticket_is_resumed_once_it_ends() {
    let (bench, replies) = cut_off_at(Stage::Build, LONG_PAST);
    let dirs = bench.dirs();
    let mut lock = Some(dirs.lock().unwrap().expect("the project is free"));
    let before = (
        bench.briefs().len(),
        bench.comments().len(),
        bench.ref_commit(),
    );
    let reads = bench.stage_reads.get();
    let mut pass = 0;
    let (outcomes, lines, printed) = bench.watch(replies, &mut |_| {
        pass += 1;
        match pass {
            1 | 2 => {
                assert_eq!(bench.stage_reads.get(), reads);
                assert_eq!(
                    (
                        bench.briefs().len(),
                        bench.comments().len(),
                        bench.ref_commit()
                    ),
                    before
                );
                assert!(bench.watch_resumes().is_empty());
                if pass == 2 {
                    drop(lock.take());
                }
                bench.wait(1);
                true
            }
            _ => false,
        }
    });
    assert!(matches!(&outcomes[..], [Ok(_)]), "{outcomes:?}\n{lines}");
    assert_eq!(
        lines
            .matches("another command is working the project: watch looks again at its next pass")
            .count(),
        1,
        "{lines}"
    );
    for text in ["resumes it again", "waits for a person", "Not run"] {
        assert!(!lines.contains(text) && !printed.contains(text), "{lines}");
    }
    assert_eq!(bench.watch_resumes(), ["build"]);
}

/// OWL-340: a ticket whose pull request was closed, which a person then ran
/// `do` on again as a new cycle, and which a usage limit left at Build, is
/// resumed by watch as a typed `continue` would run it.
#[test]
fn watch_resumes_a_new_cycle_started_after_its_pull_request_closed() {
    let bench = answered_design();
    let (outcome, printed) = bench.continue_ticket(design_replies(&bench, true));
    outcome.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    // The pull request is closed, and watch ends the following.
    bench.github.followed.lock().unwrap().state = "CLOSED";
    *bench.github.open.lock().unwrap() = None;
    let (outcomes, lines, printed) = bench.watch(Vec::new(), &mut |_| false);
    assert!(outcomes.is_empty(), "{outcomes:?}\n{lines}\n{printed}");
    assert!(bench.record().delivery.is_some_and(|d| !d.followed()));
    // A person runs it again; a usage limit cuts the new Build off.
    let (outcome, printed) = bench.run(
        vec![
            bench.read(&designed(PLAN)),
            bench.read(&reviewed(&[])),
            bench.read(&reviewed(&[])),
            bench.limited_until(LONG_PAST),
        ],
        None,
    );
    assert!(
        matches!(outcome, Err(Stop::UsageLimit { .. })),
        "{outcome:?}\n{printed}"
    );
    assert_eq!(bench.state(), (Status::Active(Stage::Build), 1));
    // The first cycle's commit holds the greeting: this Build says it anew.
    let rebuilt = bench.reply(Some("Hello again"), Some(DONE));
    let (outcomes, lines, printed) = bench.watch(vec![rebuilt], &mut |_| false);
    assert!(
        matches!(&outcomes[..], [Ok(_)]),
        "{outcomes:?}\n{lines}\n{printed}"
    );
    assert_eq!(bench.watch_resumes(), ["build"]);
}
