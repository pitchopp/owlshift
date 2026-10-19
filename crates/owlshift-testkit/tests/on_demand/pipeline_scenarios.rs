//! The quality pipeline's scenarios (OWL-310, build plan "P5 · Quality
//! pipeline", part M): S8, S9's findings and review passes, S17's routing
//! part, the `standard` and `risky` variants walked to their merge, plan
//! approval refused then given, a `replan` and a review loop, each played
//! end to end by `owlshift watch --drain` (`Watch::run_drain`) on the bench
//! of `on_demand.rs`: the Markdown tracker, the fake harness playing every
//! role, the fake GitHub. Parts F to J play each case part by part through
//! `do` and `continue`; here the drain dispatches, watch continues every
//! answer, and the steps compose. Part M's note says what is played
//! elsewhere.

use owlshift_contracts::result::Status as RunStatus;
use owlshift_contracts::review::Mandate;
use owlshift_core::vocab::Harness as HarnessKind;

use super::design_scenarios::{FORKED, PLAN, REPLAN, REVISED, answered, designed, reviewed, ruled};
use super::drain_scenarios::{by_pass, merge_read};
use super::review_scenarios::{FORGE, review};
use super::*;

/// The plan a design run writes once a `replan` is answered.
const REPLANNED: &str = r#"{"goal":"Add GREETING.md saying Hello.",
"criteria":["GREETING.md says Hello."],
"approach":"Write GREETING.md in plain text, as the decider answered the replan.",
"steps":[{"what":"Write GREETING.md.","test":"The gate's grep for Hello."}]}"#;

/// An intake `done` whose verdict is `variant`, on the bench's routing.
fn verdict(variant: &str) -> String {
    let mut result: Value = serde_json::from_str(&intake_result("done", None, "")).unwrap();
    result["intake"]["variant"] = json!(variant);
    result.to_string()
}

/// A review run's `done` with a finding per (severity, claim, criterion).
fn findings(found: &[(&str, &str, Option<&str>)]) -> String {
    let findings: Vec<Value> = found
        .iter()
        .enumerate()
        .map(|(n, (severity, claim, criterion))| {
            let mut finding = json!({
                "id": format!("F{}", n + 1),
                "severity": severity,
                "claim": claim,
                "evidence": "GREETING.md:1",
                "settle": format!("Settle: {claim}"),
            });
            if let Some(criterion) = criterion {
                finding["criterion"] = json!(criterion);
            }
            finding
        })
        .collect();
    json!({ "format": 12, "status": "done", "summary": "Reviewed.", "findings": findings })
        .to_string()
}

/// The roles of the runs of each pass, `ends` being the event count after
/// each pass but the last.
fn runs_by_pass(events: &[Event], ends: &[usize]) -> Vec<Vec<String>> {
    by_pass(events, 0, ends)
        .iter()
        .map(|pass| {
            pass.iter()
                .filter(|e| e.kind == EventKind::RunStarted)
                .map(|e| e.data["role"].as_str().unwrap().to_owned())
                .collect()
        })
        .collect()
}

/// Each `dispatch` event's command, oldest first: the drain's from Ready
/// and to Build, a review loop's `continue`.
fn dispatches(events: &[Event]) -> Vec<String> {
    data_of(events, EventKind::Dispatch, "command")
        .into_iter()
        .map(|data| data["command"].as_str().unwrap().to_owned())
        .collect()
}

impl Bench {
    /// The drain's bench with `extra` appended to the project file under
    /// `[policy]`, and DEMO-1's labels `labels`.
    fn draining_with(labels: &str, extra: &str) -> Self {
        Self::build_admitting(
            "maintainer",
            &format!("assignee = \"maintainer\"\nlabels = [{labels}]\n"),
            extra,
            ", parked = \"Parked\"",
            "admit = { label = \"agent\" }\nadmitters = [\"maintainer\"]",
        )
    }

    /// A Build run's `done` delivering `greeting`.
    fn greets(&self, greeting: &str) -> Reply {
        self.reply(Some(greeting), Some(DONE))
    }

    /// The ticket's state as its ref keeps it.
    fn status(&self) -> Status {
        TicketState::try_from(&self.record().state)
            .unwrap()
            .status()
    }

    /// The briefs of the runs of `role`, oldest first.
    fn briefs_of(&self, role: Role) -> Vec<Brief> {
        self.briefs()
            .into_iter()
            .filter(|brief| brief.role == role)
            .collect()
    }
}

/// Scenario S8, "A false premise", at intake, under `watch --drain`: intake
/// finds the ticket's premise false and asks to re-scope, the runner adding
/// the closure; the resolver, at the default level, passes it on (OWL-342);
/// the ticket waits at needs input with nothing built. Watch continues the
/// decider's re-scope: intake runs again with it in its thread, and the
/// drain dispatches the ticket in that same pass, since the continuation
/// ran before the drain read the column.
#[test]
fn scenario_s8_a_false_premise_at_intake_is_rescoped() {
    let bench = Bench::draining("maintainer");
    let premise = r#"{"format":12,"status":"premise_false",
"summary":"The ticket extends a greeting file that does not exist.",
"questions":[{"id":"Q1","category":"naming","context":"The ticket extends GREETING.md; there is none.",
"text":"Re-scope to create GREETING.md, or close the ticket?",
"options":[{"label":"Re-scope: create GREETING.md."}],"recommendation":{"option":"A","why":"Re-scope"}}]}"#;
    let rescope = "Re-scope: create GREETING.md.";
    let replies = vec![
        bench.reply(None, Some(premise)),
        bench.reply(None, Some(&passed_on(&["Q1"]))),
        bench.reply(None, Some(&check(&[("Q1", "answered", rescope)]))),
        bench.reply(None, Some(&verdict("trivial"))),
        bench.greets("Hello"),
    ];
    let ends = RefCell::new(Vec::new());
    let (outcomes, lines, printed) = bench.drain(replies, 2, |_| {
        ends.borrow_mut().push(bench.events().len());
        // The evidence went to the decider; nothing was built.
        assert_eq!(
            bench.status(),
            Status::NeedsInput {
                return_to: Stage::Intake
            }
        );
        assert_eq!(bench.stage(), "Needs Input");
        assert!(bench.record().intake.is_none());
        assert!(bench.remote_branch().is_none());
        bench.answer(rescope);
    });
    let [premise_round, back, delivered] = &outcomes[..] else {
        panic!("{outcomes:?}\n{lines}\n{printed}");
    };
    assert!(
        matches!(
            premise_round,
            Err(Stop::NeedsInput {
                status: RunStatus::PremiseFalse,
                ..
            })
        ),
        "{premise_round:?}"
    );
    assert!(matches!(back, Err(Stop::Ready { .. })), "{back:?}");
    assert!(delivered.is_ok(), "{delivered:?}\n{lines}\n{printed}");
    assert_eq!(
        runs_by_pass(&bench.events(), &ends.borrow()),
        [
            vec!["intake", "resolver"],
            vec!["answer_check", "intake", "build", "verify"],
        ]
    );
    let again = &bench.briefs_of(Role::Intake)[1];
    // The round is answered: it reads as its readings (OWL-359).
    assert!(latest_ask(again).is_empty());
    assert_eq!(last_answered(again), ["Q1"]);
    assert!(
        again.thread.iter().any(|entry| matches!(
            entry,
            ThreadEntry::Comment { body, .. } if body.contains(rescope)
        )),
        "the re-scope in intake's thread"
    );
    assert_eq!(dispatches(&bench.events()), ["drain"]);
    assert!(bench.remote_branch().is_some());
    assert_eq!(bench.stage(), "In Review");
}

/// Scenario S8 at Design: the design run of a drained `standard` ticket
/// finds its premise false and asks, the resolver passing it on (OWL-342);
/// the decider answers with the closure, the runner's own last option, the
/// design run that reads it ends `blocked`, and the ticket parks at Design.
/// Nothing is planned or built, and a parked ticket is not dispatched again.
#[test]
fn scenario_s8_a_false_premise_at_design_is_closed() {
    let bench = Bench::draining("maintainer");
    let premise = r#"{"format":12,"status":"premise_false",
"summary":"The greeting file the ticket extends is not in the repository.",
"questions":[{"id":"Q1","category":"naming","context":"The ticket extends GREETING.md; no such file exists.",
"text":"Re-scope to create GREETING.md, or close the ticket?",
"options":[{"label":"Re-scope: create GREETING.md."}],"recommendation":{"option":"A","why":"Re-scope"}}]}"#;
    let closed = r#"{"format":12,"status":"blocked",
"summary":"The decider closes the ticket: its premise is false, so nothing is to be built."}"#;
    let closure = "B: Close the ticket: its premise is false.";
    let replies = vec![
        bench.reply(None, Some(&verdict("standard"))),
        bench.read(premise),
        bench.read(&passed_on(&["Q1"])),
        bench.read(&check(&[("Q1", "answered", closure)])),
        bench.read(closed),
    ];
    let ends = RefCell::new(Vec::new());
    let (outcomes, lines, printed) = bench.drain(replies, 4, |pass| {
        ends.borrow_mut().push(bench.events().len());
        if pass == 2 {
            assert_eq!(
                bench.status(),
                Status::NeedsInput {
                    return_to: Stage::Design
                }
            );
            assert_eq!(bench.stage(), "Needs Input");
            bench.answer(closure);
        }
    });
    let Some(Err(parked)) = outcomes.last() else {
        panic!("{outcomes:?}\n{lines}\n{printed}");
    };
    assert!(
        matches!(
            parked,
            Stop::Parked {
                reason: ParkReason::Blocked,
                ..
            }
        ),
        "{parked:?}\n{lines}\n{printed}"
    );
    assert_eq!(
        runs_by_pass(&bench.events(), &ends.borrow()),
        [
            vec!["intake"],
            vec!["design", "resolver"],
            vec!["answer_check", "design"],
            vec![],
        ]
    );
    assert_eq!(
        bench.status(),
        Status::Parked {
            at: Stage::Design,
            awaiting_input: false
        }
    );
    assert_eq!(bench.stage(), "Parked");
    assert!(bench.record().plan.is_none());
    assert!(bench.remote_branch().is_none());
    // The drain's dispatch to Design, then watch's continue of the answer;
    // none after the park.
    assert_eq!(dispatches(&bench.events()), ["drain", "continue"]);
}

/// Scenario S9's findings and review passes on a drained `standard` ticket.
/// At Design review the arbiter confirms the critic's finding,
/// the design run fixes it and the next pass is clean. At Verify, pass 1's
/// confirmed finding is fixed and its rejected one listed; pass 2 leaves a
/// `blocker`, asked, a `major` off every criterion, proposed, and a `minor`
/// on a criterion, never asked (OWL-356). Watch continues the decider's
/// answer: a Verify fix run given the minor, the pass the round buys, the
/// delivery, and the proposal filed at `Triage` in the Markdown tracker's
/// inbox. The texts each part
/// writes are pinned part by part (`design_scenarios.rs`,
/// `verify_scenarios.rs`); this plays their composition.
#[test]
fn scenario_s9_findings_and_review_passes() {
    let bench = Bench::draining("maintainer");
    bench.verify_plays.set(true);
    bench.inbox.borrow_mut().markdown = true;
    let criterion = Some("GREETING.md says Hello.");
    let replies = vec![
        // Pass 1: intake.
        bench.reply(None, Some(&verdict("standard"))),
        // Pass 2: Design and its two review passes.
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
        // Build and Verify's two passes.
        bench.greets("Hello"),
        bench.read(&findings(&[
            ("major", "The greeting has no full stop.", None),
            ("major", "GREETING.md needs a licence header.", None),
        ])),
        bench.read(&ruled(&["confirmed", "rejected"])),
        bench.greets("Hello, again"),
        bench.read(&findings(&[
            ("blocker", "The greeting still has no full stop.", None),
            ("major", "GREETING.md could open with a heading.", None),
            ("minor", "The greeting does not say Hello alone.", criterion),
        ])),
        bench.read(&ruled(&["confirmed", "confirmed", "confirmed"])),
        // The residue reaches the resolver first, which passes it on
        // (OWL-342).
        bench.read(&passed_on(&["Q1"])),
        // Pass 3: the answer, a Verify fix run and the pass it buys.
        bench.read(&check(&[("Q1", "answered", "Add the full stop.")])),
        bench.greets("Hello, at last."),
        bench.read(&findings(&[])),
    ];
    let ends = RefCell::new(Vec::new());
    let (outcomes, lines, printed) = bench.drain(replies, 3, |pass| {
        ends.borrow_mut().push(bench.events().len());
        if pass == 2 {
            assert_eq!(
                bench.status(),
                Status::NeedsInput {
                    return_to: Stage::Build
                }
            );
            bench.answer("Q1: add the full stop.");
        }
    });
    let Some(Err(Stop::NeedsInput { questions, .. })) = outcomes
        .iter()
        .find(|outcome| matches!(outcome, Err(Stop::NeedsInput { .. })))
    else {
        panic!("{outcomes:?}\n{lines}\n{printed}");
    };
    let asked: Vec<&str> = questions.iter().map(|q| q.text.as_str()).collect();
    assert_eq!(asked.len(), 1, "a minor is never asked: {asked:?}");
    assert!(asked[0].starts_with("The greeting still has no full stop."));
    // The fix run after the round is given the minor on the criterion.
    let fix = bench
        .briefs()
        .into_iter()
        .rev()
        .find(|brief| brief.role == Role::Build)
        .and_then(|brief| brief.findings)
        .expect("the minor to fix");
    let ids: Vec<&str> = fix.findings.iter().map(|f| f.id.as_str()).collect();
    assert_eq!(ids, ["F3"]);
    assert!(
        matches!(outcomes.last(), Some(Ok(_))),
        "{outcomes:?}\n{lines}\n{printed}"
    );
    assert_eq!(
        runs_by_pass(&bench.events(), &ends.borrow()),
        [
            vec!["intake"],
            vec![
                "design",
                "design_review",
                "design_review",
                "arbiter",
                "design",
                "design_review",
                "design_review",
                "build",
                "verify",
                "arbiter",
                "build",
                "verify",
                "arbiter",
                "resolver",
            ],
            vec!["answer_check", "build", "verify"],
        ]
    );
    let passes: Vec<(Value, Value, Value)> = bench
        .decisions("review")
        .into_iter()
        .filter(|data| data["review"] == "pass")
        .map(|data| {
            (
                data["stage"].clone(),
                data["pass"].clone(),
                data["next"].clone(),
            )
        })
        .collect();
    assert_eq!(
        passes,
        [
            (json!("design_review"), json!(1), json!("design")),
            (json!("design_review"), json!(2), json!("build")),
            (json!("verify"), json!(1), json!("build")),
            (json!("verify"), json!(2), json!("questions")),
            (json!("verify"), json!(3), json!("deliver")),
        ]
    );
    // The off-criterion major alone is filed, at Triage, by the reviewer.
    let proposal = bench.shown("DEMO-2");
    assert_eq!(proposal.title, "GREETING.md could open with a heading.");
    assert_eq!(
        (proposal.stage.as_str(), proposal.author.as_str()),
        ("Triage", "owlshift")
    );
    assert!(!bench.remote.checkout.join("tickets/DEMO-3").exists());
    let report = bench.comments().pop().unwrap();
    for text in [
        "**Verify, 3 passes.**",
        "GREETING.md needs a licence header. Rejected: ",
        "filed as DEMO-2 for triage",
    ] {
        assert!(report.contains(text), "{text}\n---\n{report}");
    }
    // To Design, then to Build, by the drain; the answers by watch's
    // continue.
    assert_eq!(dispatches(&bench.events()), ["drain", "drain", "continue"]);
}

/// Scenario S17's routing part on a drained `standard` ticket: the verdict
/// names Codex on the deep tier at effort `high`, an `effort:max` label
/// overrides its effort, and the project names a model for `deep` and for
/// `standard`. Design and Build run on Claude Code (D4) on the verdict's
/// tier at the label's effort, each dispatch telling the verdict not
/// followed; every reviewer, the critic included, runs on Claude Code on
/// the other model, the standard tier (OWL-347), at the ticket's effort;
/// the arbiter takes none.
#[test]
fn scenario_s17_the_tickets_routing_is_followed() {
    let bench = Bench::draining_with("\"agent\", \"effort:max\"", "");
    let config = bench.remote.checkout.join("owlshift.toml");
    let text = fs::read_to_string(&config).unwrap();
    fs::write(
        &config,
        text.replace(
            "[models]\n",
            "[models]\ndeep = { claude = \"model-deep\" }\nstandard = { claude = \"model-standard\" }\n",
        ),
    )
    .unwrap();
    bench.verify_plays.set(true);
    let mut intake: Value = serde_json::from_str(&verdict("standard")).unwrap();
    intake["intake"]["routing"] = json!({ "harness": "codex", "tier": "deep", "effort": "high" });
    let replies = vec![
        bench.reply(None, Some(&intake.to_string())),
        bench.read(&designed(PLAN)),
        bench.read(&reviewed(&[(
            "minor",
            "Step 1 could name the file's encoding.",
        )])),
        bench.read(&reviewed(&[])),
        bench.read(&ruled(&["rejected"])),
        bench.greets("Hello"),
        bench.read(&findings(&[])),
    ];
    let (outcomes, lines, printed) = bench.drain(replies, 2, |_| {});
    assert!(
        matches!(outcomes.last(), Some(Ok(_))),
        "{outcomes:?}\n{lines}\n{printed}"
    );
    let route = |harness, tier, effort| RunRouting {
        harness,
        tier,
        effort,
    };
    let claude = HarnessKind::Claude;
    assert_eq!(
        bench.routed(),
        [
            route(claude, Tier::Deep, Some(Effort::Max)),
            route(claude, Tier::Standard, Some(Effort::Max)),
            route(claude, Tier::Standard, Some(Effort::Max)),
            route(claude, Tier::Standard, None),
            route(claude, Tier::Deep, Some(Effort::Max)),
            route(claude, Tier::Standard, Some(Effort::Max)),
        ],
        "design, critic, conformance, arbiter, build, verify"
    );
    let routing: Vec<(Value, Value, Value, Value)> =
        data_of(&bench.events(), EventKind::Decision, "routing")
            .into_iter()
            .map(|data| {
                (
                    data["routing"].clone(),
                    data["command"].clone(),
                    data["tier_from"].clone(),
                    data["effort_from"].clone(),
                )
            })
            .collect();
    let not_followed = (
        json!("not_followed"),
        json!("drain"),
        json!("verdict"),
        json!("label"),
    );
    assert_eq!(routing, [not_followed.clone(), not_followed]);
    let pass = bench
        .decisions("review")
        .into_iter()
        .find(|data| data["review"] == "pass")
        .unwrap();
    for reviewer in pass["reviewers"].as_array().unwrap() {
        assert_eq!(
            (
                &reviewer["harness"],
                &reviewer["model"],
                &reviewer["same_model"]
            ),
            (&json!("claude"), &json!("model-standard"), &json!(false)),
            "{pass}"
        );
    }
    let intake_comment = bench
        .comments()
        .into_iter()
        .find(|c| c.starts_with("[owlshift] INTAKE"))
        .unwrap();
    assert!(
        intake_comment.contains("Harness not followed"),
        "{intake_comment}"
    );
}

/// The `standard` variant walked to its merge, with a review loop: the
/// drain takes the ticket in, then dispatches it to Design; its plan is
/// reviewed by the critic and conformance, the Build follows it, Verify's
/// critic reviews the diff, and the pull request opens. The decider's review
/// on it sends the ticket back to Build on the delivered branch, the plan
/// and the review in its brief, then Verify and a push to the same pull
/// request; the merge, read by watch, ends the following.
#[test]
fn scenario_a_standard_ticket_walks_to_its_merge_through_a_review_loop() {
    let bench = Bench::draining_with("\"agent\"", FORGE);
    let replies = vec![
        bench.reply(None, Some(&verdict("standard"))),
        bench.read(&designed(PLAN)),
        bench.read(&reviewed(&[])),
        bench.read(&reviewed(&[])),
        bench.greets("Hello"),
        bench.greets("Hello there"),
    ];
    let ends = RefCell::new(Vec::new());
    let (outcomes, lines, printed) = bench.drain(replies, 4, |pass| {
        ends.borrow_mut().push(bench.events().len());
        match pass {
            2 => {
                assert_eq!(bench.stage(), "In Review");
                *bench.github.reviews.lock().unwrap() = vec![review(
                    5_547_776_064,
                    "Ann-Dev",
                    "Say hello there.",
                    &[("GREETING.md", 1, "Here.")],
                )];
            }
            3 => {
                bench.merge("DEMO-1");
            }
            _ => {}
        }
    });
    assert_eq!(
        outcomes.iter().filter(|o| o.is_ok()).count(),
        2,
        "the delivery and the loop's push: {outcomes:?}\n{lines}\n{printed}"
    );
    let events = bench.events();
    assert_eq!(
        runs_by_pass(&events, &ends.borrow()),
        [
            vec!["intake"],
            vec![
                "design",
                "design_review",
                "design_review",
                "build",
                "verify"
            ],
            vec!["build", "verify"],
            vec![],
        ]
    );
    assert_eq!(dispatches(&events), ["drain", "drain", "continue"]);
    let mandates: Vec<Vec<Mandate>> = bench
        .briefs()
        .iter()
        .filter_map(|brief| brief.review.as_ref().map(|r| r.mandates.clone()))
        .collect();
    assert_eq!(
        mandates,
        [
            vec![Mandate::Critic],
            vec![Mandate::Conformance],
            vec![Mandate::Critic, Mandate::Conformance],
            vec![Mandate::Critic, Mandate::Conformance],
        ],
        "Design review's two, then Verify's critic alone, holding conformance"
    );
    let kept = bench.record().plan.expect("plan.json");
    let builds = bench.briefs_of(Role::Build);
    for build in &builds {
        assert_eq!(build.plan.as_ref().map(|p| &p.plan), Some(&kept.plan));
    }
    assert!(builds[0].pull_request_reviews.is_empty());
    assert_eq!(builds[1].pull_request_reviews.len(), 1);
    assert_eq!(
        bench.github.created.lock().unwrap().len(),
        1,
        "one pull request"
    );
    let report = bench.comments().pop().unwrap();
    assert!(
        report.contains("Changed after reviews on the pull request"),
        "{report}"
    );
    assert!(merge_read(&events, TICKET).is_some(), "{lines}");
}

/// The `risky` variant walked to its merge: plan approval refused, then
/// given, and a `replan`. Its plan is reviewed by the critic, conformance
/// and debt, then put to the decider, whose reply picks the fork's other
/// option: the design run changes the plan, which gets one pass and is
/// asked again, with no Build meanwhile, each approval the decider's alone
/// with no resolver run, as `risky` forces `always` (OWL-342). Approved, the
/// Build follows it, then asks a `replan`, which the resolver passes on to
/// the decider. Watch continues the answer back to Design: a new plan, its
/// pass, its approval asked and given, then the Build, `risky`'s two Verify
/// seats, the pull request and its merge.
#[test]
fn scenario_a_risky_plan_refused_approved_and_replanned() {
    let bench = Bench::draining("maintainer");
    let three = || {
        [
            bench.read(&reviewed(&[])),
            bench.read(&reviewed(&[])),
            bench.read(&reviewed(&[])),
        ]
    };
    let mut replies = vec![
        bench.reply(None, Some(&verdict("risky"))),
        bench.read(&designed(FORKED)),
    ];
    replies.extend(three());
    replies.extend([
        bench.read(&check(&[("Q1", "answered", "Plain text instead.")])),
        bench.read(&answered("changed", Some(REVISED))),
    ]);
    replies.extend(three());
    replies.extend([
        bench.read(&check(&[("Q1", "answered", "Approved as it is.")])),
        bench.read(&answered("approved", None)),
        bench.read(REPLAN),
        bench.read(&passed_on(&["Q1"])),
        bench.read(&check(&[("Q1", "answered", "GREETING.md, in plain text.")])),
        bench.read(&designed(REPLANNED)),
    ]);
    replies.extend(three());
    replies.extend([
        bench.read(&check(&[("Q1", "answered", "Approved.")])),
        bench.read(&answered("approved", None)),
        bench.greets("Hello"),
    ]);
    let answers = [
        "Use plain text instead.",
        "Approved as it is.",
        "GREETING.md, in plain text.",
        "Approved.",
    ];
    let ends = RefCell::new(Vec::new());
    let (outcomes, lines, printed) = bench.drain(replies, 7, |pass| {
        ends.borrow_mut().push(bench.events().len());
        match pass {
            2..=5 => {
                assert_eq!(bench.stage(), "Needs Input", "after pass {pass}");
                bench.answer(answers[pass - 2]);
            }
            6 => {
                bench.merge("DEMO-1");
            }
            _ => {}
        }
        if pass == 4 {
            // The replan waits for the decider, to return to Design.
            let record = bench.record();
            assert!(record.questions.asks.last().unwrap().questions[0].replan);
            assert_eq!(
                bench.status(),
                Status::NeedsInput {
                    return_to: Stage::Design
                }
            );
        }
    });
    let rounds: Vec<u32> = outcomes
        .iter()
        .filter_map(|outcome| match outcome {
            Err(Stop::NeedsInput {
                posted: Ok(round), ..
            }) => Some(round.get()),
            _ => None,
        })
        .collect();
    assert_eq!(rounds, [1, 2, 3, 4], "{outcomes:?}\n{lines}\n{printed}");
    assert!(matches!(outcomes.last(), Some(Ok(_))), "{outcomes:?}");
    let review = ["design_review"; 3];
    let design_and_review = |first: &[&str]| -> Vec<String> {
        first
            .iter()
            .chain(&review)
            .map(|role| (*role).to_owned())
            .collect()
    };
    assert_eq!(
        runs_by_pass(&bench.events(), &ends.borrow()),
        [
            vec!["intake".to_owned()],
            design_and_review(&["design"]),
            design_and_review(&["answer_check", "design"]),
            ["answer_check", "design", "build", "resolver"]
                .map(str::to_owned)
                .to_vec(),
            design_and_review(&["answer_check", "design"]),
            ["answer_check", "design", "build", "verify", "verify"]
                .map(str::to_owned)
                .to_vec(),
            vec![],
        ]
    );
    // The replan's alone: no approval reached the resolver.
    assert_eq!(bench.briefs_of(Role::Resolver).len(), 1);
    // Each Build follows the plan approved for it.
    let builds = bench.briefs_of(Role::Build);
    let plan = |text: &str| serde_json::from_str::<Value>(text).unwrap();
    let given: Vec<Value> = builds
        .iter()
        .map(|b| serde_json::to_value(&b.plan.as_ref().unwrap().plan).unwrap())
        .collect();
    assert_eq!(given.len(), 2);
    assert_eq!(given[0]["approach"], plan(REVISED)["approach"]);
    assert_eq!(given[1]["approach"], plan(REPLANNED)["approach"]);
    let kept = bench.record().plan.unwrap();
    let approval = kept.approval.expect("the last plan approved");
    let approved: Vec<Value> = bench
        .decisions("plan")
        .into_iter()
        .filter(|data| data["plan"] == "approved")
        .collect();
    assert_eq!(approved.len(), 2, "{approved:?}");
    let last_kept = bench
        .decisions("plan")
        .into_iter()
        .rfind(|data| data["plan"] == "kept")
        .unwrap();
    assert_eq!(json!(approval.plan_sha256), last_kept["plan_sha256"]);
    assert_eq!(
        dispatches(&bench.events()),
        [
            "drain", "continue", "continue", "continue", "continue", "continue", "continue"
        ],
        "the drain's from Ready to Design; then each pass's continue of an answer, and, \
         within the continue that approves a plan, its own dispatch to Build"
    );
    assert!(merge_read(&bench.events(), TICKET).is_some(), "{lines}");
}
