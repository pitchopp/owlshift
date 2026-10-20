//! The quality pipeline's contracts (OWL-300): the configuration keys,
//! `result.json` format 9 and the brief's format 15 against each other, and
//! the ticket-ref formats bumped for them, older formats read and newer
//! ones refused.

use std::fs;
use std::path::PathBuf;

use owlshift_contracts::brief::Brief;
use owlshift_contracts::config::{FindingsConfirmation, ProjectConfig};
use owlshift_contracts::drain::TicketProposals;
use owlshift_contracts::refs::{PushKind, TicketDelivery, TicketQuestions};
use owlshift_contracts::result::RunResult;
use owlshift_contracts::review::TicketReview;
use serde_json::{Value, json};

fn fixture(name: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
        .replace("\r\n", "\n")
}

fn json_fixture(name: &str) -> Value {
    serde_json::from_str(&fixture(name)).unwrap()
}

fn plan() -> Value {
    json_fixture("result-design.json")["plan"].clone()
}

fn finding(id: &str) -> Value {
    json_fixture("result-review.json")["findings"][0]
        .clone()
        .as_object()
        .map(|f| {
            let mut f = f.clone();
            f.insert("id".to_owned(), id.into());
            Value::Object(f)
        })
        .unwrap()
}

/// The fixture brief, as a brief of `role` with no refused result.
fn brief(role: &str) -> Value {
    let mut brief = json_fixture("brief.json");
    brief["role"] = role.into();
    brief.as_object_mut().unwrap().remove("result_refusal");
    match role {
        "design_review" | "verify" => {
            brief["review"] = json!({ "mandates": ["critic"], "pass": 1 });
            if role == "design_review" {
                brief["plan"] = json!({ "base": "a".repeat(40), "plan": plan() });
            }
        }
        "arbiter" => {
            brief["findings"] =
                json!({ "mode": "rule", "findings": [finding("F1"), finding("F3")] });
        }
        _ => {}
    }
    brief
}

fn base_result(status: &str) -> Value {
    json!({ "format": 14, "status": status, "summary": "s" })
}

fn question(replan: bool) -> Value {
    let mut q = json!({ "id": "Q1", "category": "scope", "context": "c", "text": "t", "recommendation": { "why": "r" } });
    if replan {
        q["replan"] = true.into();
    }
    q
}

/// Checks `result` against `brief`: `Ok` or the refusal's message.
fn against(brief: &Value, result: &Value) -> Result<(), String> {
    let brief = Brief::parse(&brief.to_string()).map_err(|e| format!("brief: {e}"))?;
    RunResult::parse_against(&result.to_string(), &brief)
        .map(|_| ())
        .map_err(|refused| refused.error.to_string())
}

#[test]
fn each_quality_pipeline_field_comes_from_its_roles_only() {
    let with = |mut document: Value, field: &str, value: Value| {
        document[field] = value;
        document
    };
    let mut approval_brief = brief("design");
    approval_brief["plan"] = json!({ "base": "a".repeat(40), "plan": plan() });
    approval_brief["approval"] = true.into();
    let mut answer_brief = brief("build");
    answer_brief["findings"] = json!({ "mode": "answer", "findings": [finding("F2")] });
    let mut fix_brief = brief("build");
    fix_brief["findings"] = json!({ "mode": "fix", "findings": [finding("F2")] });
    // A minor on no criterion, which the Build may defer (OWL-377), and one
    // on a criterion, which it may not (S13).
    let minor = |id: &str, on_criterion: bool| {
        let mut f = finding(id);
        f["severity"] = "minor".into();
        if !on_criterion {
            f.as_object_mut().unwrap().remove("criterion");
        }
        f
    };
    let mut minors_brief = brief("build");
    minors_brief["findings"] =
        json!({ "mode": "fix", "findings": [minor("F1", false), minor("F2", true)] });
    let responses = |answers: &[(&str, &str)]| {
        let answers: Vec<Value> = answers
            .iter()
            .map(|(id, outcome)| json!({ "finding": id, "outcome": outcome, "reason": "r" }))
            .collect();
        let mut result = base_result("done");
        result["finding_responses"] = answers.into();
        result
    };
    let premise_false = with(
        base_result("premise_false"),
        "questions",
        json!([question(false)]),
    );
    let replan = with(
        base_result("questions"),
        "questions",
        json!([question(true)]),
    );
    let planned = with(base_result("done"), "plan", plan());
    let approved = with(base_result("done"), "approval", json!("approved"));
    let changed = with(planned.clone(), "approval", json!("changed"));
    let found = json_fixture("result-review.json");
    let ruled = with(
        base_result("done"),
        "rulings",
        json!([
            { "finding": "F1", "outcome": "confirmed", "reason": "r" },
            { "finding": "F3", "outcome": "rejected", "reason": "r" }
        ]),
    );
    let answered = with(
        base_result("done"),
        "finding_responses",
        json!([{ "finding": "F2", "outcome": "rejected", "reason": "r" }]),
    );

    let cases: Vec<(&str, Value, Value, Option<&str>)> = vec![
        // premise_false: intake, design and build only.
        (
            "premise_false from intake",
            brief("intake"),
            premise_false.clone(),
            None,
        ),
        (
            "premise_false from design",
            brief("design"),
            premise_false.clone(),
            None,
        ),
        (
            "premise_false from build",
            brief("build"),
            premise_false.clone(),
            None,
        ),
        (
            "premise_false from verify",
            brief("verify"),
            premise_false.clone(),
            Some("premise_false is given but the run's role is verify"),
        ),
        (
            "premise_false from the arbiter",
            brief("arbiter"),
            premise_false,
            Some("premise_false is given but the run's role is arbiter"),
        ),
        // replan: build only.
        ("replan from build", brief("build"), replan.clone(), None),
        (
            "replan from design",
            brief("design"),
            replan,
            Some("replan on Q1 is given but the run's role is design"),
        ),
        // The plan and the approval: design only, by its brief.
        ("a plan from design", brief("design"), planned.clone(), None),
        (
            "design done without a plan",
            brief("design"),
            base_result("done"),
            Some("no plan is given"),
        ),
        (
            "a plan from build",
            brief("build"),
            planned.clone(),
            Some("plan is given but the run's role is build"),
        ),
        ("approved", approval_brief.clone(), approved.clone(), None),
        ("changed", approval_brief.clone(), changed.clone(), None),
        (
            "a plan after an approval round",
            approval_brief.clone(),
            planned,
            Some("no approval is given"),
        ),
        (
            "an approval without an approval round",
            brief("design"),
            changed,
            Some("the brief asks for no approval"),
        ),
        (
            "an approval from verify",
            brief("verify"),
            approved,
            Some("approval is given but the run's role is verify"),
        ),
        // Findings: the review roles only.
        (
            "findings from design review",
            brief("design_review"),
            found.clone(),
            None,
        ),
        ("findings from verify", brief("verify"), found.clone(), None),
        (
            "findings from the arbiter",
            brief("arbiter"),
            found,
            Some("findings is given but the run's role is arbiter"),
        ),
        // Rulings: the arbiter only, one per finding of its brief.
        ("rulings", brief("arbiter"), ruled.clone(), None),
        (
            "rulings missing a finding",
            brief("arbiter"),
            with(
                base_result("done"),
                "rulings",
                json!([{ "finding": "F1", "outcome": "confirmed", "reason": "r" }]),
            ),
            Some("the rulings name F1 but the brief gives F1, F3"),
        ),
        (
            "rulings from design review",
            brief("design_review"),
            ruled,
            Some("rulings is given but the run's role is design_review"),
        ),
        // Responses: design or build in author mode only, one per finding.
        ("responses", answer_brief.clone(), answered.clone(), None),
        (
            "author mode done without responses",
            answer_brief.clone(),
            base_result("done"),
            Some("the finding_responses name none but the brief gives F2"),
        ),
        // Findings to fix are answered too (format 13, OWL-377): fixed or
        // disagreed with, a minor on no criterion deferred.
        (
            "fix mode done without responses",
            fix_brief.clone(),
            base_result("done"),
            Some("the finding_responses name none but the brief gives F2"),
        ),
        (
            "a disagreement with a finding to fix",
            fix_brief.clone(),
            responses(&[("F2", "disagreed")]),
            None,
        ),
        (
            "a rejection of a confirmed finding",
            fix_brief.clone(),
            answered.clone(),
            Some("answers F2 rejected: the arbiter confirmed it"),
        ),
        (
            "a blocker deferred",
            fix_brief,
            responses(&[("F2", "deferred")]),
            Some("answers F2 deferred: only a minor finding that names no criterion"),
        ),
        (
            "a minor deferred and one fixed",
            minors_brief.clone(),
            responses(&[("F1", "deferred"), ("F2", "fixed")]),
            None,
        ),
        (
            "a minor on a criterion deferred",
            minors_brief,
            responses(&[("F1", "fixed"), ("F2", "deferred")]),
            Some("answers F2 deferred: only a minor finding that names no criterion"),
        ),
        (
            "a disagreement in author mode",
            answer_brief.clone(),
            responses(&[("F2", "disagreed")]),
            Some("answers F2 disagreed: no one judged it"),
        ),
        (
            "responses without findings",
            brief("build"),
            answered,
            Some("gives no finding to answer or fix"),
        ),
    ];
    for (case, brief, result, refusal) in cases {
        match (against(&brief, &result), refusal) {
            (Ok(()), None) => {}
            (Err(error), Some(expected)) => {
                assert!(error.contains(expected), "{case}: {error}")
            }
            (outcome, expected) => panic!("{case}: {outcome:?}, expected {expected:?}"),
        }
    }
}

#[test]
fn result_quality_pipeline_rules_need_no_brief() {
    let parse = |edit: &dyn Fn(&mut Value)| {
        let mut document = json_fixture("result-review.json");
        edit(&mut document);
        RunResult::parse(&document.to_string()).map_err(|e| e.to_string())
    };
    for (edit, reason) in [
        (
            &(|d: &mut Value| d["findings"][0]["id"] = "F2".into()) as &dyn Fn(&mut Value),
            "finding F2 is out of order: expected F1",
        ),
        (
            &|d| d["findings"][0]["settle"] = " ".into(),
            "finding F1 has a blank settle",
        ),
        (
            &|d| d["findings"][0]["severity"] = "critical".into(),
            "unknown variant",
        ),
        (
            &|d| {
                d["findings"] = json!(
                    (1..=65)
                        .map(|n| finding(&format!("F{n}")))
                        .collect::<Vec<_>>()
                )
            },
            "past the 64 allowed",
        ),
        (
            &|d| {
                d["rulings"] = json!([
                    { "finding": "F2", "outcome": "confirmed", "reason": "r" },
                    { "finding": "F1", "outcome": "confirmed", "reason": "r" }
                ])
            },
            "the ruling of F1 is repeated or out of order",
        ),
        (
            &|d| {
                d["finding_responses"] =
                    json!([{ "finding": "F1", "outcome": "fixed", "reason": "" }])
            },
            "the response of F1 has no reason",
        ),
        (
            &|d| d["status"] = "blocked".into(),
            "findings is given but status is not done",
        ),
        (&|d| d["plan"] = json!({ "goal": "g" }), "missing field"),
    ] {
        let error = parse(edit).unwrap_err();
        assert!(error.contains(reason), "{reason}: {error}");
    }
}

#[test]
fn brief_quality_pipeline_fields_go_to_their_roles_only() {
    let parse = |document: &Value| Brief::parse(&document.to_string()).map_err(|e| e.to_string());
    let with = |mut document: Value, field: &str, value: Value| {
        if value.is_null() {
            document.as_object_mut().unwrap().remove(field);
        } else {
            document[field] = value;
        }
        document
    };
    let given_plan = json!({ "base": "a".repeat(40), "plan": plan() });
    let review = json!({
        "id": 7,
        "author": { "name": "ada", "relation": "decider" },
        "comments": [{ "path": "src/lib.rs", "line": 3, "body": "Rename this." }]
    });
    // The answer check since OWL-357: its approval question points to it.
    for role in [
        "design",
        "design_review",
        "build",
        "verify",
        "arbiter",
        "answer_check",
    ] {
        parse(&with(brief(role), "plan", given_plan.clone()))
            .unwrap_or_else(|e| panic!("{role}: {e}"));
    }
    let mut approval = with(brief("design"), "plan", given_plan.clone());
    approval["approval"] = true.into();
    parse(&approval).unwrap();
    for mode in ["answer", "fix"] {
        for role in ["design", "build"] {
            let findings = json!({ "mode": mode, "findings": [finding("F1")] });
            parse(&with(brief(role), "findings", findings)).unwrap_or_else(|e| panic!("{e}"));
        }
    }
    parse(&with(
        brief("build"),
        "pull_request_reviews",
        json!([review.clone()]),
    ))
    .unwrap();
    // The diff of a Verify pass (OWL-302): in verify's review, and with the
    // arbiter's findings to rule on.
    let diff = json!({ "base": "b".repeat(40), "path": ".owlshift/run/diff.patch" });
    let review_with = |role: &str, diff: &Value| {
        with(
            brief(role),
            "review",
            json!({ "mandates": ["critic"], "pass": 1, "diff": diff }),
        )
    };
    let findings_with = |mode: &str, diff: &Value| json!({ "mode": mode, "findings": [finding("F1")], "diff": diff });
    parse(&review_with("verify", &diff)).unwrap();
    parse(&with(
        brief("arbiter"),
        "findings",
        findings_with("rule", &diff),
    ))
    .unwrap();

    for (case, document, reason) in [
        (
            "a diff for design review",
            review_with("design_review", &diff),
            "a diff to review: the role is design_review",
        ),
        (
            "a diff with findings to answer",
            with(brief("build"), "findings", findings_with("answer", &diff)),
            "a diff with findings: the role is build",
        ),
        (
            "a diff on a branch name",
            review_with("verify", &json!({ "base": "main", "path": "diff.patch" })),
            "the diff's base \"main\" is not a full commit id",
        ),
        (
            "a plan for intake",
            with(brief("intake"), "plan", given_plan.clone()),
            "a plan: the role is intake",
        ),
        (
            "design review without a plan",
            with(brief("design_review"), "plan", Value::Null),
            "goes to the design review role always",
        ),
        (
            "a plan on a branch name",
            with(
                brief("build"),
                "plan",
                json!({ "base": "main", "plan": plan() }),
            ),
            "is not a full commit id",
        ),
        (
            "an approval round without a plan",
            with(brief("design"), "approval", true.into()),
            "an approval round: the role is design",
        ),
        (
            "an approval round for build",
            with(
                with(brief("build"), "plan", given_plan),
                "approval",
                true.into(),
            ),
            "an approval round: the role is build",
        ),
        (
            "findings to rule on for build",
            with(
                brief("build"),
                "findings",
                json!({ "mode": "rule", "findings": [finding("F1")] }),
            ),
            "findings: the role is build",
        ),
        (
            "findings to fix for verify",
            with(
                brief("verify"),
                "findings",
                json!({ "mode": "fix", "findings": [finding("F1")] }),
            ),
            "findings: the role is verify",
        ),
        (
            "an arbiter without findings",
            with(brief("arbiter"), "findings", Value::Null),
            "goes to the arbiter always",
        ),
        (
            "no finding",
            with(
                brief("arbiter"),
                "findings",
                json!({ "mode": "rule", "findings": [] }),
            ),
            "no finding is given",
        ),
        (
            "findings out of order",
            with(
                brief("arbiter"),
                "findings",
                json!({ "mode": "rule", "findings": [finding("F2"), finding("F2")] }),
            ),
            "finding F2 is repeated or out of order",
        ),
        (
            "verify without a review",
            with(brief("verify"), "review", Value::Null),
            "a review: the role is verify",
        ),
        (
            "a review for build",
            with(
                brief("build"),
                "review",
                json!({ "mandates": ["debt"], "pass": 1 }),
            ),
            "a review: the role is build",
        ),
        (
            "a mandate twice",
            with(
                brief("verify"),
                "review",
                json!({ "mandates": ["debt", "debt"], "pass": 1 }),
            ),
            "given twice",
        ),
        (
            "pull request reviews for verify",
            with(
                brief("verify"),
                "pull_request_reviews",
                json!([review.clone()]),
            ),
            "pull request reviews: the role is verify",
        ),
        (
            "an empty review",
            with(
                brief("build"),
                "pull_request_reviews",
                json!([{ "id": 7, "author": { "name": "ada", "relation": "other" } }]),
            ),
            "has neither a body nor a comment",
        ),
        (
            "a comment without a path",
            with(
                brief("build"),
                "pull_request_reviews",
                json!([{
                    "id": 7,
                    "author": { "name": "ada", "relation": "other" },
                    "comments": [{ "path": "", "body": "b" }]
                }]),
            ),
            "has a comment without a path or a body",
        ),
        (
            "a newer brief",
            with(brief("build"), "format", 25.into()),
            "upgrade Owlshift",
        ),
    ] {
        let error = parse(&document).unwrap_err();
        assert!(error.contains(reason), "{case}: {error}");
    }
}

#[test]
fn bumped_ticket_ref_formats_read_older_ones_and_refuse_newer_ones() {
    // questions.json 11: a question's replan; format 10 read, without it.
    let questions = json_fixture("ticket-questions.json");
    let mut replan = questions.clone();
    replan["asks"][0]["questions"][0]["replan"] = true.into();
    let read = TicketQuestions::parse(&replan.to_string()).unwrap();
    assert!(read.asks[0].questions[0].replan);
    assert!(read.render().contains("\"replan\": true"));
    let mut older = questions.clone();
    older["format"] = 10.into();
    assert!(
        TicketQuestions::parse(&older.to_string())
            .unwrap()
            .render()
            .starts_with("{\n  \"format\": 16,")
    );
    for path in [
        &["asks", "0", "questions", "0"][..],
        &["decisions", "0", "question"][..],
    ] {
        let mut kept = older.clone();
        let mut node = &mut kept;
        for key in path {
            node = match key.parse::<usize>() {
                Ok(n) => &mut node[n],
                Err(_) => &mut node[*key],
            };
        }
        node["replan"] = true.into();
        let error = TicketQuestions::parse(&kept.to_string())
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("format 10 keeps no replan"),
            "{path:?}: {error}"
        );
    }
    // Format 11 keeps it (OWL-324 read formats 2 to 11).
    let mut eleven = replan.clone();
    eleven["format"] = 11.into();
    assert!(TicketQuestions::parse(&eleven.to_string()).unwrap().asks[0].questions[0].replan);
    let mut newer = questions;
    newer["format"] = 17.into();
    let error = TicketQuestions::parse(&newer.to_string())
        .unwrap_err()
        .to_string();
    assert!(error.contains("upgrade Owlshift"), "{error}");

    // delivery.json 5: a review loop's push and its report's reviews.
    let delivery = json_fixture("ticket-delivery.json");
    let mut review = delivery.clone();
    review["pushing"]["kind"] = "review".into();
    review["pushing"]["report"]
        .as_object_mut()
        .unwrap()
        .remove("checks");
    review["pushing"]["report"]["reviews"] = json!([{ "id": 7, "author": "ada" }]);
    let read = TicketDelivery::parse(&review.to_string()).unwrap();
    assert_eq!(read.pushing.as_ref().unwrap().kind, PushKind::Review);
    for (case, edit, reason) in [
        (
            "a review push without reviews",
            &(|d: &mut Value| {
                d["pushing"]["report"]
                    .as_object_mut()
                    .unwrap()
                    .remove("reviews");
            }) as &dyn Fn(&mut Value),
            "names reviews if and only if it is a review loop's",
        ),
        (
            "reviews on a fix push",
            &|d| d["pushing"]["kind"] = "fix".into(),
            "names reviews if and only if it is a review loop's",
        ),
        (
            "a review named twice",
            &|d| {
                d["pushing"]["report"]["reviews"] =
                    json!([{ "id": 7, "author": "ada" }, { "id": 7, "author": "ada" }])
            },
            "names review 7 twice",
        ),
        (
            "a format-4 review push",
            &|d| d["format"] = 4.into(),
            "format 4 keeps no review loop's push",
        ),
        (
            "a newer delivery",
            &|d| d["format"] = 6.into(),
            "upgrade Owlshift",
        ),
    ] {
        let mut document = review.clone();
        edit(&mut document);
        let error = TicketDelivery::parse(&document.to_string())
            .unwrap_err()
            .to_string();
        assert!(error.contains(reason), "{case}: {error}");
    }
    let mut format_4 = delivery;
    format_4["format"] = 4.into();
    assert!(
        TicketDelivery::parse(&format_4.to_string())
            .unwrap()
            .render()
            .starts_with("{\n  \"format\": 5,")
    );
    let mut reviews_in_4 = format_4;
    reviews_in_4["pushing"]["report"]["reviews"] = json!([{ "id": 7, "author": "ada" }]);
    let error = TicketDelivery::parse(&reviews_in_4.to_string())
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("format 4 keeps no report's reviews"),
        "{error}"
    );

    // proposals.json 2: a review loop's run; format 1 read, without one.
    let proposals = json_fixture("ticket-proposals.json");
    let mut review_run = proposals.clone();
    review_run["proposals"][0]["run"]["kind"] = "review".into();
    TicketProposals::parse(&review_run.to_string()).unwrap();
    let mut older = proposals.clone();
    older["format"] = 1.into();
    assert!(
        TicketProposals::parse(&older.to_string())
            .unwrap()
            .render()
            .starts_with("{\n  \"format\": 2,")
    );
    review_run["format"] = 1.into();
    let error = TicketProposals::parse(&review_run.to_string())
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("format 1 keeps no review loop's run"),
        "{error}"
    );
    let mut newer = proposals;
    newer["format"] = 3.into();
    let error = TicketProposals::parse(&newer.to_string())
        .unwrap_err()
        .to_string();
    assert!(error.contains("upgrade Owlshift"), "{error}");
}

/// What a review loop keeps (OWL-307): the delivery's start, a review
/// loop's owed report unchanged or with notes, and the reviews the loop
/// under way answers.
#[test]
fn a_review_loop_keeps_its_start_its_report_and_its_reviews() {
    let delivery = json_fixture("ticket-delivery.json");
    let mut started = delivery.clone();
    started["delivered_at"] = "2026-10-16T12:00:00Z".into();
    let read = TicketDelivery::parse(&started.to_string()).unwrap();
    assert_eq!(
        read.delivered_at,
        Some("2026-10-16T12:00:00Z".parse().unwrap())
    );
    assert_eq!(TicketDelivery::parse(&read.render()).unwrap(), read);
    let mut in_4 = started.clone();
    in_4["format"] = 4.into();
    let error = TicketDelivery::parse(&in_4.to_string())
        .unwrap_err()
        .to_string();
    assert!(error.contains("format 4 keeps no delivered_at"), "{error}");

    let mut unchanged = delivery;
    unchanged["owed"] = json!({
        "kind": "review",
        "count": 1,
        "unchanged": true,
        "report": {
            "summary": "The comment asks for what the code already does.",
            "notes": [{
                "question": "Late comment",
                "decision": "Given to one more Build run before delivery, to integrate",
                "basis": "The decider's comments posted or edited at 2026-10-16T12:00:00Z"
            }],
            "reviews": [{ "id": 5547776064u64, "author": "ada" }]
        }
    });
    TicketDelivery::parse(&unchanged.to_string()).unwrap();
    for (case, edit, reason) in [
        (
            "an unchanged fix",
            &(|d: &mut Value| {
                d["owed"]["kind"] = "fix".into();
                d["owed"]["report"]
                    .as_object_mut()
                    .unwrap()
                    .remove("reviews");
                d["owed"]["report"].as_object_mut().unwrap().remove("notes");
            }) as &dyn Fn(&mut Value),
            "unchanged only if it is a correction's or a review loop's",
        ),
        (
            "a review loop's report counting 2",
            &|d| d["owed"]["count"] = 2.into(),
            "owed report counts 1",
        ),
        (
            "notes on a correction",
            &|d| {
                d["owed"]["kind"] = "correction".into();
                d["owed"]["report"]
                    .as_object_mut()
                    .unwrap()
                    .remove("reviews");
                d["owed"]["report"]["questions"] = json!(["Which file?"]);
            },
            "keeps notes only if it is a delivery's or a review loop's",
        ),
    ] {
        let mut document = unchanged.clone();
        edit(&mut document);
        let error = TicketDelivery::parse(&document.to_string())
            .unwrap_err()
            .to_string();
        assert!(error.contains(reason), "{case}: {error}");
    }

    let review = json_fixture("ticket-review.json");
    let answering = json!({
        "id": 5547776257u64,
        "author": { "name": "ada", "relation": "decider" },
        "comments": [{ "path": "src/lib.rs", "line": 3, "body": "Say two in digits." }]
    });
    let mut under_way = review.clone();
    under_way["answering"] = json!([answering.clone()]);
    let read = TicketReview::parse(&under_way.to_string()).unwrap();
    assert_eq!(read.answering[0].id, 5_547_776_257);
    assert_eq!(TicketReview::parse(&read.render()).unwrap(), read);
    for (case, answering, reason) in [
        (
            "twice",
            json!([answering.clone(), answering.clone()]),
            "is answered twice by the loop under way",
        ),
        (
            "already answered",
            json!([{
                "id": 3141592653u64,
                "author": { "name": "ada", "relation": "decider" },
                "body": "Rename it."
            }]),
            "is under way although answered",
        ),
        (
            "empty",
            json!([{ "id": 9, "author": { "name": "ada", "relation": "other" } }]),
            "has neither a body nor a comment",
        ),
    ] {
        let mut document = review.clone();
        document["answering"] = answering;
        let error = TicketReview::parse(&document.to_string())
            .unwrap_err()
            .to_string();
        assert!(error.contains(reason), "{case}: {error}");
    }
}

/// A pass after a revision (OWL-356): the brief tells it what changed and
/// the earlier findings, each at its own stage; the review record keeps
/// what each pass reviewed and the last fix under way; a Build alone is
/// given notes, and answers none.
#[test]
fn a_later_pass_is_told_what_changed_and_the_last_fix_is_kept() {
    let kept = json_fixture("ticket-review.json")["passes"][0]["findings"][0].clone();
    let at_pass = |role: &str, since: Value, earlier: Value| {
        let mut brief = brief(role);
        brief["review"]["pass"] = 2.into();
        brief["review"]["since"] = since;
        brief["review"]["earlier"] = earlier;
        if role == "verify" {
            brief["review"]["diff"] = json!({ "base": "b".repeat(40), "path": "d.patch" });
        }
        Brief::parse(&brief.to_string()).map_err(|e| e.to_string())
    };
    let earlier = json!([{ "pass": 1, "kept": kept }]);
    let since_diff = json!({ "pass": 1, "diff": { "base": "c".repeat(40), "path": "s.patch" } });
    let since_steps =
        json!({ "pass": 1, "parts": ["approach", "steps"], "steps": [1], "removed": 1 });
    at_pass("verify", since_diff.clone(), earlier.clone()).unwrap();
    at_pass("design_review", since_steps.clone(), earlier.clone()).unwrap();
    let notes = json!({ "mode": "note", "findings": [finding("F1")] });
    let mut build = brief("build");
    build["findings"] = notes.clone();
    let parsed = Brief::parse(&build.to_string()).unwrap();
    let done = json!({ "format": 14, "status": "done", "summary": "s",
        "finding_responses": [{ "finding": "F1", "outcome": "fixed", "reason": "r" }] });
    assert!(
        RunResult::parse_against(&done.to_string(), &parsed).is_err(),
        "notes are answered by no response"
    );
    let mut design = brief("design");
    design["findings"] = notes;
    for (case, error, reason) in [
        (
            "notes for design",
            Brief::parse(&design.to_string()).unwrap_err().to_string(),
            "findings: the role is design",
        ),
        (
            "plan steps at verify",
            at_pass("verify", since_steps.clone(), json!([])).unwrap_err(),
            "verify's since names plan parts or steps",
        ),
        (
            "no diff at verify",
            at_pass("verify", json!({ "pass": 1 }), json!([])).unwrap_err(),
            "verify's since names no diff",
        ),
        (
            "a diff at design review",
            at_pass("design_review", since_diff.clone(), json!([])).unwrap_err(),
            "design review's since names a diff",
        ),
        (
            "a step past the plan's",
            at_pass(
                "design_review",
                json!({ "pass": 1, "steps": [9] }),
                json!([]),
            )
            .unwrap_err(),
            "do not increase within the plan's",
        ),
        (
            "parts out of order",
            at_pass(
                "design_review",
                json!({ "pass": 1, "parts": ["steps", "goal"] }),
                json!([]),
            )
            .unwrap_err(),
            "out of the plan's order",
        ),
        (
            "since the pass itself",
            at_pass("verify", json!({ "pass": 2 }), json!([])).unwrap_err(),
            "since pass 2, not before pass 2",
        ),
        (
            "an earlier finding of this pass",
            at_pass("verify", Value::Null, json!([{ "pass": 2, "kept": kept }])).unwrap_err(),
            "an earlier finding of pass 2",
        ),
    ] {
        assert!(error.contains(reason), "{case}: {error}");
    }

    let review = json_fixture("ticket-review.json");
    let verify_pass = |head: &str| {
        let mut pass = review["passes"][0].clone();
        pass["stage"] = "verify".into();
        for finding in pass["findings"].as_array_mut().unwrap() {
            finding["reviewer"]["role"] = "verify".into();
        }
        pass["head"] = head.into();
        pass
    };
    let print = json!({ "parts": vec!["e".repeat(64); 9], "steps": ["f".repeat(64)] });
    let mut kept_review = review.clone();
    kept_review["passes"][0]["plan"] = print.clone();
    kept_review["passes"]
        .as_array_mut()
        .unwrap()
        .push(verify_pass(&"a".repeat(40)));
    kept_review["last_fix"] = 1.into();
    let parsed = TicketReview::parse(&kept_review.to_string()).unwrap();
    assert_eq!(parsed.last_fix.map(|n| n.get()), Some(1));
    assert_eq!(
        parsed.passes[1].head.as_deref(),
        Some("a".repeat(40).as_str())
    );
    let refused = |edit: &dyn Fn(&mut Value)| {
        let mut document = kept_review.clone();
        edit(&mut document);
        TicketReview::parse(&document.to_string())
            .unwrap_err()
            .to_string()
    };
    for (case, error, reason) in [
        (
            "a commit at design review",
            refused(&|d| d["passes"][0]["head"] = "a".repeat(40).into()),
            "keeps a reviewed commit",
        ),
        (
            "a branch for a commit",
            refused(&|d| d["passes"][1]["head"] = "main".into()),
            "not a full commit id",
        ),
        (
            "a print at verify",
            refused(&|d| d["passes"][1]["plan"] = print.clone()),
            "keeps a plan's print",
        ),
        (
            "a print short of a part",
            refused(&|d| d["passes"][0]["plan"]["parts"] = json!(["e".repeat(64)])),
            "9 part hashes",
        ),
        (
            "a last fix of another pass",
            refused(&|d| d["last_fix"] = 2.into()),
            "not the record's last pass",
        ),
    ] {
        assert!(error.contains(reason), "{case}: {error}");
    }
}

#[test]
fn findings_and_forge_keys_are_read_and_checked() {
    let base = fixture("owlshift.toml");
    let parsed = ProjectConfig::parse(&base).unwrap();
    assert_eq!(parsed.pipeline.findings(), FindingsConfirmation::Author);
    assert!(parsed.forge.is_none());

    let with = |pipeline: &str, forge: &str| {
        base.replace(
            "plan_approval = \"on-fork\"",
            &format!("plan_approval = \"on-fork\"\n{pipeline}"),
        ) + forge
    };
    let author = ProjectConfig::parse(&with("findings = \"author\"", "")).unwrap();
    assert_eq!(author.pipeline.findings(), FindingsConfirmation::Author);
    let arbiter = ProjectConfig::parse(&with("findings = \"arbiter\"", "")).unwrap();
    assert_eq!(arbiter.pipeline.findings(), FindingsConfirmation::Arbiter);
    // OWL-348: the opt-in Codex reviewer, off when absent.
    assert!(!parsed.pipeline.codex_reviewer());
    let codex = ProjectConfig::parse(&with("codex_reviewer = true", "")).unwrap();
    assert!(codex.pipeline.codex_reviewer());
    assert_eq!(ProjectConfig::parse(&codex.render()).unwrap(), codex);
    assert!(ProjectConfig::parse(&with("codex_reviewer = \"yes\"", "")).is_err());
    let forge = ProjectConfig::parse(&with(
        "",
        "\n[forge]\nreviewers = { pitchopp = \"u-ann\", Octo-Cat = \"u-bob\" }\n",
    ))
    .unwrap();
    let reviewers = &forge.forge.as_ref().unwrap().reviewers;
    assert_eq!(reviewers.get("Octo-Cat").map(String::as_str), Some("u-bob"));
    assert_eq!(ProjectConfig::parse(&forge.render()).unwrap(), forge);

    for (case, text, reason) in [
        (
            "an unknown confirmation",
            with("findings = \"vote\"", ""),
            "unknown variant `vote`",
        ),
        (
            "no reviewer",
            with("", "\n[forge]\nreviewers = {}\n"),
            "lists no reviewer",
        ),
        (
            "no reviewers key",
            with("", "\n[forge]\n"),
            "missing field `reviewers`",
        ),
        (
            "a padded account",
            with("", "\n[forge]\nreviewers = { ada = \" u-ann\" }\n"),
            "has a space at either end",
        ),
        (
            "a blank login",
            with("", "\n[forge]\nreviewers = { \"\" = \"u-ann\" }\n"),
            "the login \"\" is blank",
        ),
        (
            "a login twice in another case",
            with(
                "",
                "\n[forge]\nreviewers = { ada = \"u-ann\", ADA = \"u-ann\" }\n",
            ),
            "listed twice",
        ),
        (
            "an unknown forge key",
            with(
                "",
                "\n[forge]\nreviewers = { ada = \"u-ann\" }\nkind = \"github\"\n",
            ),
            "unknown field `kind`",
        ),
    ] {
        let error = ProjectConfig::parse(&text).unwrap_err().to_string();
        assert!(error.contains(reason), "{case}: {error}");
    }
}
