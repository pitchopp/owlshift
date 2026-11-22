//! A person's steps (OWL-619; architecture section 4, "A person's steps"),
//! on the bench of `on_demand.rs`: the Markdown tracker, the fake harness
//! playing every role, the fake GitHub. The plan holds a person's steps,
//! the Build passes them, the delivery lists them; at Watch a round asks
//! the decider those due before the merge, merge on green waiting for it;
//! the run after the answers fixes a defect and builds the agent's steps
//! that waited on them; at the merge, the steps due after it are filed as
//! one follow-up.

use std::num::NonZeroU32;

use owlshift_contracts::drain::ProposalOutcome;
use owlshift_contracts::review::{StepAnswer, StepBy};

use super::design_scenarios::{designed, reviewed};
use super::*;

/// A plan whose last step is a person's, due before the merge, as is its
/// second, with the agent's third step waiting on the second's result.
const PERSON_PLAN: &str = r#"{"goal":"Add GREETING.md saying Hello.",
"criteria":["GREETING.md says Hello.","The greeting reads well on staging."],
"approach":"Write GREETING.md with the line the gate greps, then have a person read it deployed.",
"steps":[
 {"what":"Add GREETING.md.","test":"The gate's grep for Hello."},
 {"what":"Open GREETING.md on staging in a browser, which no run can reach.",
  "test":"It reads Hello.","by":"person","when":"before_merge"},
 {"what":"Note the staging check in the README.","test":"The README names it."},
 {"what":"Ask the team on staging whether the tone fits.","test":"They say yes.",
  "by":"person","when":"before_merge"}]}"#;

/// A plan whose last step is a person's, due after the merge: it acts on
/// what the merge deploys.
const AFTER_PLAN: &str = r#"{"goal":"Add GREETING.md saying Hello.",
"criteria":["GREETING.md says Hello.","The team knows it is live."],
"approach":"Write GREETING.md with the line the gate greps; once merged, a person tells the team.",
"steps":[
 {"what":"Add GREETING.md.","test":"The gate's grep for Hello."},
 {"what":"Tell the team on staging the greeting is live.","test":"They saw it.",
  "by":"person","when":"after_merge"}]}"#;

/// The Build's `done`, the agent's third step left for the second's result.
const OWING: &str = r#"{"format":17,"status":"done","summary":"Added GREETING.md; step 3 waits on step 2.",
"owed":[3],
"pr":{"branch":"owlshift/demo-1","title":"Add a greeting","body":"Says hello. Gate: green."}}"#;

/// The run at Watch's `done` after the round: step 3 built with the
/// results.
const NOTED: &str = r#"{"format":17,"status":"done","summary":"Noted the staging check in the README.",
"pr":{"branch":"owlshift/demo-1","title":"Add a greeting","body":"Noted."}}"#;

/// The run at Watch's `done` after a defect reported on step 2.
const MENDED: &str = r#"{"format":17,"status":"done","summary":"Fixed the greeting's spelling.",
"pr":{"branch":"owlshift/demo-1","title":"Add a greeting","body":"Mended."}}"#;

/// Two minutes later on the bench's clock.
fn later(clock: &Cell<Timestamp>) {
    clock.set(
        clock
            .get()
            .checked_add(SignedDuration::from_mins(2))
            .unwrap(),
    );
}

/// An answer check's verdicts, each answered: its question, the option the
/// decider chose (none for words of their own) and the check's reading.
fn verdicts(answers: &[(&str, Option<&str>, &str)]) -> String {
    let verdicts: Vec<Value> = answers
        .iter()
        .map(|(question, option, reason)| {
            let mut verdict =
                json!({ "question": question, "class": "answered", "reason": reason });
            if let Some(option) = option {
                verdict["option"] = json!(option);
            }
            verdict
        })
        .collect();
    json!({ "format": 17, "status": "done", "summary": "Checked.", "verdicts": verdicts })
        .to_string()
}

/// A standard ticket whose plan is `plan`, designed, reviewed and built by
/// `build`, its pull request open; merge on green under `switch`.
fn delivered(plan: &str, build: &str, switch: &str) -> Bench {
    let bench = Bench::with(
        "maintainer",
        "assignee = \"maintainer\"\n",
        &format!("[forge]\nmerge = \"{switch}\"\n"),
    );
    bench.add_labels(&["variant:standard"]);
    let replies = vec![
        bench.read(&designed(plan)),
        bench.read(&reviewed(&[])),
        bench.read(&reviewed(&[])),
        bench.reply(Some("Hello"), Some(build)),
    ];
    let (outcome, printed) = bench.run(replies, None);
    outcome.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    bench
}

/// The plan's checks the ticket ref keeps: each step, its round and how it
/// was answered.
fn checks(bench: &Bench) -> Vec<(u32, u32, Option<StepAnswer>)> {
    bench
        .record()
        .plan
        .unwrap()
        .checks
        .iter()
        .map(|check| (check.step.get(), check.round.get(), check.answer))
        .collect()
}

/// The `merge_waits` events naming the person's steps.
fn person_waits(bench: &Bench) -> Vec<Value> {
    data_of(&bench.events(), EventKind::Decision, "watch")
        .into_iter()
        .filter(|data| data["watch"] == "merge_waits" && data["blocker"] == "person_steps")
        .collect()
}

/// OWL-619's first acceptance: a standard ticket whose plan's last step is
/// a person's is designed, reviewed and built, its Build given the person's
/// steps and passing them; it is delivered with its owed steps under "For a
/// person" in the pull request and in the DELIVERY report. Under `[forge]
/// merge = "all"`, watch's first read asks the decider steps 2 and 4 in one
/// round at Watch, through no resolver, and merges nothing while it waits.
/// Answered as the plan names, the run at Watch builds step 3 with the
/// results and pushes it, and watch merges the pull request, no hold of the
/// person's steps ever told.
#[test]
fn a_plan_whose_last_step_is_a_person_s_is_built_delivered_and_held_from_merging() {
    let bench = Bench::with(
        "maintainer",
        "assignee = \"maintainer\"\n",
        "[forge]\nmerge = \"all\"\n",
    );
    bench.add_labels(&["variant:standard"]);
    let replies = vec![
        bench.read(&designed(PERSON_PLAN)),
        bench.read(&reviewed(&[])),
        bench.read(&reviewed(&[])),
        bench.reply(Some("Hello"), Some(OWING)),
    ];
    let (outcome, printed) = bench.run(replies, None);
    let line = outcome.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    assert!(
        line.to_string()
            .contains("Owlshift merges it once it is green, unless a person must judge it first"),
        "{line}"
    );
    // The Build was given the person's steps, and its owed step is kept
    // with the plan.
    let briefs = bench.briefs();
    assert_eq!(briefs[3].role, Role::Build);
    let given = &briefs[3].plan.as_ref().unwrap().plan.steps;
    let by: Vec<StepBy> = given.iter().map(|step| step.by).collect();
    assert_eq!(
        by,
        [StepBy::Agent, StepBy::Person, StepBy::Agent, StepBy::Person]
    );
    let kept = bench.record().plan.unwrap();
    assert_eq!(kept.owed, [NonZeroU32::new(3).unwrap()]);
    // The PLAN comment lists the person's steps apart.
    let pointer = bench.plan_pointer();
    assert!(
        pointer.contains("**For a person:** step 2, before the merge; step 4, before the merge."),
        "{pointer}"
    );
    // The pull request lists the owed steps after the agent's text.
    let body = bench.github.created.lock().unwrap()[0]["body"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        body.starts_with("Says hello. Gate: green.\n\n## For a person\n\n")
            && body.contains(
                "- **Step 2, before the merge, a person's:** Open GREETING.md on staging in a \
                 browser, which no run can reach. Result to report: It reads Hello.\n- **Step \
                 3, before the merge, left for a person's result:** Note the staging check in \
                 the README.\n- **Step 4, before the merge, a person's:**"
            ),
        "{body}"
    );
    let report = bench
        .comments()
        .into_iter()
        .rfind(|body| body.starts_with("[owlshift] DELIVERY"))
        .unwrap();
    assert!(
        !report.contains("Owlshift will not merge")
            && report.contains("\n- **Step 4, before the merge, a person's:** Ask the team"),
        "{report}"
    );

    // Watch's first read asks steps 2 and 4, the runner's own round at
    // Watch, its checks kept with the plan; nothing runs or merges.
    let briefs = bench.briefs().len();
    let clock = bench.clock.clone();
    let mut pass = 0;
    let (outcomes, lines, printed) = bench.watch(Vec::new(), &mut |_| {
        pass += 1;
        later(&clock);
        pass < 4
    });
    assert!(
        matches!(&outcomes[..], [Err(Stop::NeedsInput { posted: Ok(round), .. })] if round.get() == 1),
        "{outcomes:?}\n{lines}\n{printed}"
    );
    assert_eq!(
        bench.briefs().len(),
        briefs,
        "no run: no Build, no resolver"
    );
    assert!(bench.github.merges.lock().unwrap().is_empty());
    assert_eq!(
        bench.state(),
        (
            Status::NeedsInput {
                return_to: Stage::Watch
            },
            1
        )
    );
    let asked = bench.questions_posted();
    assert_eq!(asked.len(), 1, "{asked:?}");
    assert!(
        asked[0].contains("Step 2: Open GREETING.md on staging in a browser")
            && asked[0].contains("Step 4: Ask the team on staging")
            && asked[0].contains("The agent's step 3 waits on it"),
        "{}",
        asked[0]
    );
    let record = bench.record();
    let categories: Vec<&str> = record.questions.asks[record.questions.asks.len() - 1]
        .questions
        .iter()
        .map(|question| question.category.as_str())
        .collect();
    assert_eq!(categories, ["person_step", "person_step"]);
    assert_eq!(checks(&bench), [(2, 1, None), (4, 1, None)]);

    // Answered as the plan names (option A): the RESUME keeps both passed,
    // and the same `continue` runs the build role at Watch on step 3, which
    // waited on step 2's result, then pushes it.
    bench.answer("Q1: A. Q2: A.\n");
    let (outcome, printed) = bench.continue_ticket(vec![
        bench.reply(
            None,
            Some(&verdicts(&[
                ("Q1", Some("A"), "Step 2 reads Hello on staging."),
                ("Q2", Some("A"), "The team says the tone fits."),
            ])),
        ),
        bench.reply(Some("Hello from staging"), Some(NOTED)),
    ]);
    outcome.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    let given = bench.last_of(Role::Build).person_steps.unwrap();
    let answered: Vec<(u32, bool)> = given
        .answered
        .iter()
        .map(|step| (step.step.get(), step.defect))
        .collect();
    assert_eq!(answered, [(2, false), (4, false)]);
    assert_eq!(given.build, [NonZeroU32::new(3).unwrap()]);
    assert_eq!(given.answered[0].reading, "Step 2 reads Hello on staging.");
    let plan = bench.record().plan.unwrap();
    assert!(plan.owed.is_empty(), "step 3 is built: {:?}", plan.owed);
    assert_eq!(plan.owed_before_merge(), 0);
    assert_eq!(
        checks(&bench),
        [
            (2, 1, Some(StepAnswer::Passed)),
            (4, 1, Some(StepAnswer::Passed))
        ]
    );
    let changed = bench.marked("[owlshift] DELIVERY");
    assert!(
        changed.contains(
            "**Changed after a person's steps,** to follow the decider's results on step 3"
        ),
        "{changed}"
    );

    // Watch merges it once it reads it green twice; nothing is asked again.
    let pushed = bench.remote_branch().unwrap();
    let mut pass = 0;
    let (_, lines, printed) = bench.watch(Vec::new(), &mut |_| {
        pass += 1;
        later(&clock);
        pass < 4
    });
    assert_eq!(
        bench.github.merges.lock().unwrap().clone(),
        [json!({ "sha": pushed, "merge_method": "squash" })],
        "{lines}\n{printed}"
    );
    assert_eq!(bench.questions_posted().len(), 1);
    assert!(
        person_waits(&bench).is_empty(),
        "{:?}",
        person_waits(&bench)
    );
}

/// OWL-619's second acceptance: a defect reported in the round starts a run
/// at Watch, its brief carrying the step and what was reported, and not the
/// agent's step that waits on it; once its push lands, that step alone is
/// asked again, in a new round, the step answered as passed never again;
/// answered as the plan names, the next run builds the agent's step.
#[test]
fn a_defect_reported_in_the_round_starts_a_run_and_its_step_is_asked_again() {
    let bench = delivered(PERSON_PLAN, OWING, "never");
    let (asked, printed) = bench.continue_ticket(Vec::new());
    assert!(
        matches!(&asked, Err(Stop::NeedsInput { posted: Ok(round), .. }) if round.get() == 1),
        "{asked:?}\n{printed}"
    );
    let first_push = bench.remote_branch().unwrap();

    bench.answer("Q1: B, it reads Helo on staging. Q2: A.\n");
    let (outcome, printed) = bench.continue_ticket(vec![
        bench.reply(
            None,
            Some(&verdicts(&[
                ("Q1", Some("B"), "Step 2 reads Helo on staging, not Hello."),
                ("Q2", Some("A"), "The team says the tone fits."),
            ])),
        ),
        bench.reply(Some("Hello again"), Some(MENDED)),
    ]);
    outcome.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    let brief = bench.last_of(Role::Build);
    let given = brief.person_steps.unwrap();
    assert_eq!(given.head, first_push);
    let answered: Vec<(u32, bool, &str)> = given
        .answered
        .iter()
        .map(|step| (step.step.get(), step.defect, step.reading.as_str()))
        .collect();
    assert_eq!(
        answered,
        [
            (2, true, "Step 2 reads Helo on staging, not Hello."),
            (4, false, "The team says the tone fits.")
        ]
    );
    assert!(given.build.is_empty(), "step 3 waits on step 2");
    let thread = serde_json::to_string(&brief.thread).unwrap();
    assert!(thread.contains("it reads Helo on staging"), "{thread}");
    assert_ne!(bench.remote_branch().unwrap(), first_push, "pushed");
    assert_eq!(checks(&bench), [(4, 1, Some(StepAnswer::Passed))]);
    assert_eq!(bench.record().plan.unwrap().owed_before_merge(), 2);
    assert!(
        bench.marked("[owlshift] DELIVERY").contains(
            "**Changed after a person's steps,** to follow the decider's results on step 2;"
        ),
        "{}",
        bench.marked("[owlshift] DELIVERY")
    );

    // Step 2 alone is asked again, in round 2.
    let (asked, printed) = bench.continue_ticket(Vec::new());
    assert!(
        matches!(&asked, Err(Stop::NeedsInput { posted: Ok(round), .. }) if round.get() == 2),
        "{asked:?}\n{printed}"
    );
    let again = bench.questions_posted().pop().unwrap();
    assert!(
        again.contains("Step 2: Open GREETING.md") && !again.contains("Step 4:"),
        "{again}"
    );
    assert_eq!(
        checks(&bench),
        [(2, 2, None), (4, 1, Some(StepAnswer::Passed))]
    );

    // Answered as the plan names, the next run builds step 3.
    bench.answer("A.\n");
    let (outcome, printed) = bench.continue_ticket(vec![
        bench.reply(
            None,
            Some(&verdicts(&[("Q1", Some("A"), "Step 2 reads Hello now.")])),
        ),
        bench.reply(Some("Hello, noted"), Some(NOTED)),
    ]);
    outcome.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    let given = bench.last_of(Role::Build).person_steps.unwrap();
    assert_eq!(given.build, [NonZeroU32::new(3).unwrap()]);
    let plan = bench.record().plan.unwrap();
    assert_eq!((plan.owed_before_merge(), plan.owed.len()), (0, 0));
    assert_eq!(bench.questions_posted().len(), 2);
}

/// The steps due after the merge hold nothing: under merge on green, the
/// runner merges, and the pass after the merge files them as one follow-up
/// for the decider, outside the three a parent files; later passes file
/// nothing more (OWL-619).
#[test]
fn the_steps_after_the_merge_are_filed_as_one_follow_up_once_merged() {
    let bench = delivered(AFTER_PLAN, DONE, "all");
    bench.inbox.borrow_mut().on = true;
    assert!(bench.questions_posted().is_empty());
    let clock = bench.clock.clone();
    let mut pass = 0;
    let (_, lines, printed) = bench.watch(Vec::new(), &mut |_| {
        pass += 1;
        later(&clock);
        pass < 6
    });
    assert_eq!(
        bench.github.merges.lock().unwrap().len(),
        1,
        "{lines}\n{printed}"
    );
    assert!(bench.questions_posted().is_empty(), "nothing asked");
    let inbox = bench.inbox.borrow();
    assert_eq!(inbox.tickets.len(), 1, "{:?}", inbox.tickets);
    let (_, title, description) = &inbox.tickets[0];
    assert_eq!(title, "After DEMO-1's merge: the steps for a person");
    assert!(
        description.contains("Step 2, a person's: Tell the team on staging the greeting is live.")
            && description.contains("DEMO-1's decider (maintainer)"),
        "{description}"
    );
    // Waiting on the decider, it is assigned to them: the ticket's assignee
    // by the account the tracker names it (OWL-625).
    assert_eq!(
        inbox.assigned,
        [(inbox.tickets[0].0.clone(), Some("maintainer".to_owned()))]
    );
    let attempts = inbox.attempts;
    drop(inbox);
    assert!(matches!(
        &proposal_outcomes(&bench)[..],
        [ProposalOutcome::Filed { .. }]
    ));
    let (_, _, _) = bench.watch(Vec::new(), &mut |_| {
        pass += 1;
        later(&clock);
        pass < 8
    });
    assert_eq!(bench.inbox.borrow().attempts, attempts, "filed once");
}

/// A Build that meets a step only a person can do outside any plan asks it
/// under `person_step`, which goes to the decider with no resolver run, at
/// the level `all` too (OWL-619).
#[test]
fn a_person_s_step_met_outside_the_plan_is_asked_of_the_decider() {
    let bench = Bench::new(true);
    bench.policy("resolver_delegation = \"all\"");
    let asked = r#"{"format":17,"status":"questions",
"summary":"Added GREETING.md; its look on staging needs a person.",
"questions":[{"id":"Q1","category":"person_step",
 "context":"Staging sits behind a login no run holds.",
 "text":"Open GREETING.md on staging: does it read Hello?",
 "recommendation":{"why":"It reads Hello, as the file says."}}]}"#;
    let (outcome, printed) = bench.run(vec![bench.reply(Some("Hello"), Some(asked))], None);
    assert!(
        matches!(&outcome, Err(Stop::NeedsInput { posted: Ok(round), .. }) if round.get() == 1),
        "{outcome:?}\n{printed}"
    );
    assert_eq!(bench.roles(), [Role::Build]);
    let round = bench.marked("[owlshift] QUESTIONS");
    assert!(
        round.contains("Open GREETING.md on staging: does it read Hello?"),
        "{round}"
    );
}

/// A review loop's Build that commits nothing still keeps the agent's steps
/// it names as owed (OWL-619): the loop ends with no push, and `plan.json`
/// owes them from that write on.
#[test]
fn a_review_loop_build_that_changes_nothing_keeps_what_it_owes() {
    let bench = Bench::with(
        "maintainer",
        "assignee = \"maintainer\"\n",
        super::review_scenarios::FORGE,
    );
    bench.add_labels(&["variant:standard"]);
    let replies = vec![
        bench.read(&designed(PERSON_PLAN)),
        bench.read(&reviewed(&[])),
        bench.read(&reviewed(&[])),
        bench.reply(Some("Hello"), Some(DONE)),
    ];
    let (outcome, printed) = bench.run(replies, None);
    outcome.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    assert!(bench.record().plan.unwrap().owed.is_empty());
    let delivered = bench.remote_branch().unwrap();
    bench.reviews_on_github(vec![super::review_scenarios::review(
        7_000_000_001,
        "ann-dev",
        "Does it say Hello?",
        &[],
    )]);
    let unchanged = r#"{"format":17,"status":"done","summary":"GREETING.md already says Hello: nothing to change; step 3 waits on step 2.",
"owed":[3],
"pr":{"branch":"owlshift/demo-1","title":"Add a greeting","body":"Unchanged."}}"#;
    let (outcome, printed) = bench.continue_ticket(vec![bench.reply(None, Some(unchanged))]);
    outcome.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    assert_eq!(bench.remote_branch().unwrap(), delivered, "nothing pushed");
    assert_eq!(
        bench.record().plan.unwrap().owed,
        [NonZeroU32::new(3).unwrap()]
    );
}

/// A plan of a person's steps alone: one due before the merge, one after.
const PERSON_ONLY_PLAN: &str = r#"{"goal":"Check the greeting on staging.",
"criteria":["The greeting reads well on staging."],
"approach":"A person reads the deployed greeting; nothing in the repository changes.",
"steps":[
 {"what":"Open GREETING.md on staging in a browser, which no run can reach.",
  "test":"It reads Hello.","by":"person","when":"before_merge"},
 {"what":"Tell the team on staging the greeting is live.","test":"They saw it.",
  "by":"person","when":"after_merge"}]}"#;

/// The HANDBACK comments on DEMO-1, oldest first.
fn handbacks(bench: &Bench) -> Vec<String> {
    bench
        .comments()
        .into_iter()
        .filter(|body| body.starts_with("[owlshift] HANDBACK"))
        .collect()
}

/// What a person reads of DEMO-1 now (OWL-626), the bench's project served
/// as `owlshift start` serves it: `owlshift status`, `owlshift why DEMO-1`,
/// `owlshift why`, `owlshift status --waits` and the web UI's waits card.
fn shown(bench: &Bench) -> [String; 5] {
    use owlshift_runner::{status, voice::Voice, why};
    let repo = Repo::parse(REPO).unwrap();
    owlshift_runner::served::Registry::in_data_dir(&bench.data)
        .register(&repo, &bench.remote.checkout)
        .unwrap();
    let now = bench.clock.get();
    let tz = jiff::tz::TimeZone::UTC;
    let why = |only| {
        why::render(&why::gather(&bench.data, &bench.dirs(), REPO, only, 2, &[], now).unwrap())
    };
    let waits = status::waits::gather(&bench.data, Some(&repo), 2, now);
    let card = status::waits::build(&waits, Voice::Ui).projects[0]
        .tickets
        .iter()
        .find(|row| row.ticket == ticket())
        .and_then(|row| row.waits.as_ref())
        .map_or_else(String::new, |wait| wait.text.clone());
    [
        status::render(&status::gather_files(&bench.data, 2, &[], now), &tz),
        why(Some(ticket())),
        why(None),
        status::waits::report(&waits, false, &tz),
        card,
    ]
}

/// OWL-619's third part, its first trigger: a standard ticket whose design
/// run plans a person's steps alone is handed back right after that run,
/// with no Design review, arbiter or Build run. The ref is at Ready with its
/// handback and the plan; one HANDBACK comment lists both steps and the
/// gesture that gives the ticket back; the ticket shows parked, which
/// notifies as a park does. `continue` then refuses it and moves nothing
/// back to the ready column.
#[test]
fn a_plan_of_a_person_s_steps_alone_is_handed_back_with_no_build() {
    let bench = Bench::parking();
    bench.add_labels(&["variant:standard"]);
    let (outcome, printed) = bench.run(vec![bench.read(&designed(PERSON_ONLY_PLAN))], None);
    let stop = outcome.unwrap_err();
    assert!(
        matches!(&stop, Stop::HandedBack { unposted: None, .. }),
        "{stop}\n{printed}"
    );
    assert_eq!(
        owlshift_runner::notify::blocker_line(&ticket(), &stop).as_deref(),
        Some("DEMO-1 was handed back: its work is a person's")
    );
    assert_eq!(bench.roles(), [Role::Design]);
    let record = bench.record();
    assert_eq!(state_of(&bench).status(), Status::Active(Stage::Ready));
    assert!(record.handback.is_some());
    assert!(!record.plan.unwrap().plan.has_agent_step());
    let handbacks = handbacks(&bench);
    assert_eq!(handbacks.len(), 1, "{handbacks:?}");
    for part in [
        "Every step of the plan its design run wrote is a person's",
        "- **Step 1, before the merge, a person's:** Open GREETING.md on staging in a browser, \
         which no run can reach. Result to report: It reads Hello.",
        "- **Step 2, after the merge, a person's:** Tell the team",
        "or run `owlshift do DEMO-1`",
    ] {
        assert!(handbacks[0].contains(part), "{part}\n{}", handbacks[0]);
    }
    assert_eq!(bench.stage(), "Parked");
    assert_eq!(
        bench.stage_events(EventKind::TrackerWrite),
        ["In Progress", "Parked"]
    );
    // OWL-626: every surface says it is handed back, what was read and the
    // gesture that gives it back.
    let [status, why_one, why_all, waits, card] = shown(&bench);
    let read = "every step of its plan is a person's (2 steps); to give it back to Owlshift, \
                make the project's admission gesture again after its HANDBACK comment";
    for (said, line) in [
        (
            &status,
            format!("  handed back to a person: DEMO-1 at 2026-10-02 09:10: {read}"),
        ),
        (
            &why_one,
            format!("now: handed back to a person at 2026-10-02T09:10:00Z: {read}"),
        ),
        (
            &why_all,
            "  DEMO-1 [ready, handed back]: handed back to a person at".to_owned(),
        ),
        (
            &waits,
            format!("DEMO-1 [ready, handed back]: handed back to a person: {read}"),
        ),
    ] {
        assert!(said.contains(&line), "{line}\n{said}");
        assert!(said.contains("or `owlshift do DEMO-1`"), "{said}");
    }
    assert!(
        why_one.contains("  - **Step 2, after the merge, a person's:** Tell the team"),
        "{why_one}"
    );
    assert_eq!(card, format!("handed back to a person: {read}"));

    let (again, printed) = bench.continue_ticket(Vec::new());
    assert!(
        matches!(&again, Err(Stop::Refused(why)) if why.contains("was handed back to a person")),
        "{again:?}\n{printed}"
    );
    assert_eq!(bench.stage(), "Parked");
    assert_eq!(bench.roles(), [Role::Design]);
}

/// An intake `done` that reads DEMO-1 as a person's work whole, in the words
/// of its description.
fn person_only_intake(quote: &str) -> String {
    let mut result: Value = serde_json::from_str(&intake_result("done", None, "")).unwrap();
    result["intake"]["variant"] = json!("standard");
    result["intake"]["person_only"] = json!({ "quote": quote });
    result.to_string()
}

/// OWL-619's third part under `watch --drain`, its second trigger and the
/// way back: intake reads DEMO-1, labelled `agent`, as a person's work
/// whole in its description's words. It is handed back with no Design run:
/// its record kept with the quote, a HANDBACK comment and no INTAKE one,
/// the ticket shown parked, out of the column. A person moves it back to
/// the ready column, its label left as it was put on before the HANDBACK:
/// the drain dispatches nothing and warns once that the gesture was made
/// before the handback, naming the gesture to make again. The label put on
/// again after the HANDBACK admits it: the next pass dispatches it to
/// Design, the handback dropped and the record naming that gesture. No
/// write of the runner ever moved the ticket to the ready column.
#[test]
fn a_ticket_intake_reads_as_a_person_s_is_handed_back_and_admitted_again_after() {
    let bench = Bench::draining("maintainer");
    let quote = "I will check the banner on staging myself.";
    bench.describe(quote);
    let at = |time: &str| -> Timestamp { time.parse().unwrap() };
    // The label, put on before anything ran, after the text's last edit.
    bench.edits.borrow_mut().push((
        ticket(),
        at("2026-10-01T08:00:00Z"),
        at("2026-10-01T07:00:00Z"),
    ));
    let replies = vec![
        bench.reply(None, Some(&person_only_intake(quote))),
        bench.read(&designed(PERSON_PLAN)),
    ];
    let handed_back = Cell::new(None);
    let (outcomes, lines, printed) = bench.drain_passes(replies, 3, |pass, _| {
        match pass {
            1 => {
                // Handed back: no Design run, the record kept, parked.
                assert_eq!(bench.roles(), [Role::Intake]);
                assert_eq!(state_of(&bench).status(), Status::Active(Stage::Ready));
                let record = bench.record();
                let kept = record.intake.unwrap().person_only.unwrap();
                assert_eq!(kept.quote, quote);
                handed_back.set(record.handback.map(|handback| handback.at));
                assert_eq!(bench.stage(), "Parked");
                // OWL-626: shown handed back on the ticket's own words.
                let read = format!("intake read it so in the ticket's own words: `{quote}`");
                for said in shown(&bench) {
                    assert!(said.contains("handed back to a person"), "{said}");
                    assert!(said.contains(&read), "{read}\n{said}");
                }
                // A person moves it back to the column, the label as it was.
                move_ticket(&bench, "Todo");
            }
            2 => {
                // Nothing taken in or dispatched; then the label put on
                // again after the HANDBACK, once the hold is over.
                assert_eq!(bench.roles(), [Role::Intake]);
                let again = handed_back.get().unwrap() + SignedDuration::from_mins(5);
                bench.edits.borrow_mut()[0].1 = again;
            }
            _ => {}
        }
        let later = bench.clock.get().checked_add(SignedDuration::from_mins(11));
        bench.clock.set(later.unwrap());
    });
    let report = format!("{outcomes:?}\n{lines}\n{printed}");
    assert!(
        matches!(&outcomes[0], Err(Stop::HandedBack { unposted: None, .. })),
        "{report}"
    );
    // Dispatched once the label was put on again: its first stage, Design.
    assert_eq!(bench.roles(), [Role::Intake, Role::Design], "{report}");
    let record = bench.record();
    assert_eq!(record.handback, None);
    for said in shown(&bench) {
        assert!(!said.contains("handed back"), "{said}");
    }
    let again = handed_back.get().unwrap() + SignedDuration::from_mins(5);
    assert_eq!(record.intake.unwrap().gesture.at, again);

    let comments = bench.comments();
    assert!(
        !comments.iter().any(|c| c.starts_with("[owlshift] INTAKE")),
        "{comments:?}"
    );
    let handbacks = handbacks(&bench);
    assert_eq!(handbacks.len(), 1, "{handbacks:?}");
    for part in [
        format!("Intake read it so in the ticket's own words: `{quote}`"),
        "take the label `agent` off and put it on again after this comment".to_owned(),
    ] {
        assert!(handbacks[0].contains(&part), "{part}\n{}", handbacks[0]);
    }
    // One warning for the gesture made before the handback.
    let warned: Vec<String> = bench
        .events()
        .iter()
        .filter(|e| e.kind == EventKind::Warning && e.data["what"] == json!("not_admitted"))
        .map(|e| e.data["reason"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(warned.len(), 1, "{report}");
    for part in [
        "DEMO-1 was handed back to a person at",
        "its admission gesture \"agent\" at 2026-10-01T08:00:00Z was made before",
        "take the label `agent` off and put it on again",
    ] {
        assert!(warned[0].contains(part), "{part}\n{}", warned[0]);
    }
    // The runner moved the ticket to parked, then to working at its
    // dispatch: never to the ready column, the one move a state gesture
    // could read as an admission.
    assert_eq!(
        bench.stage_events(EventKind::TrackerWrite),
        ["Parked", "In Progress"],
        "{report}"
    );
}
