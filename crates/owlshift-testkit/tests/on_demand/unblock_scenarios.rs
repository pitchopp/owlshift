//! A `blocked` through the resolver (OWL-620; architecture section 4, "A
//! `blocked` through the resolver"), on the bench of `on_demand.rs`: the
//! Markdown tracker, the fake harness playing every role, the fake GitHub.
//! A Build or a run at Watch that stops `blocked` gets one resolver run,
//! whose verdict the runner acts on: the run again, the branch rebased, a
//! step owed to a person, or the park with the resolver's sentence first.

use owlshift_contracts::UnblockVerdict;
use owlshift_contracts::review::StepWhen;

use super::design_scenarios::{designed, reviewed};
use super::*;

/// A Build that stops on a permission it misread.
const MISREAD: &str = r#"{"format":18,"status":"blocked",
"summary":"The brief says network false, so the crate cannot be fetched."}"#;

/// A Build that stops for what its base lacks.
const BEHIND: &str = r#"{"format":18,"status":"blocked",
"summary":"The greeting needs TWO.md, which the base gained since the branch left it."}"#;

/// A Build that stops at a step only a person can do.
const AT_A_BROWSER: &str = r#"{"format":18,"status":"blocked",
"summary":"Step 2 reads the greeting in a browser on staging, which no run can reach."}"#;

/// A plan whose last step is the agent's as Design wrote it, though only a
/// person can do it.
const BROWSER_PLAN: &str = r#"{"goal":"Add GREETING.md saying Hello.",
"criteria":["GREETING.md says Hello.","It reads well on staging."],
"approach":"Write GREETING.md, then read it on staging.",
"steps":[
 {"what":"Add GREETING.md.","test":"The gate's grep for Hello."},
 {"what":"Read GREETING.md on staging in a browser.","test":"It reads Hello."}]}"#;

/// The Build's `done` once its last step is a person's.
const DONE_OWING: &str = r#"{"format":18,"status":"done","summary":"Added GREETING.md; step 2 is a person's.",
"pr":{"branch":"owlshift/demo-1","title":"Add a greeting","body":"Says hello. Gate: green."}}"#;

/// A rebase run's `done`.
const REBASED: &str = r#"{"format":18,"status":"done","summary":"Rebased onto the base's head; nothing conflicted."}"#;

/// The resolver's `done` with `verdict`, its note `note`, on a `fact`; a
/// `person_step` names step 2, due before the merge.
fn verdict(verdict: &str, note: &str) -> String {
    let mut unblock = json!({
        "verdict": verdict,
        "note": note,
        "category": "environment",
        "basis": "Checked in the worktree on 2026-11-22.",
        "basis_kind": "fact",
    });
    if verdict == "person_step" {
        unblock["step"] = json!(2);
        unblock["when"] = json!("before_merge");
    }
    json!({ "format": 18, "status": "done", "summary": "Read the stop.", "unblock": unblock })
        .to_string()
}

/// The `decision` events of the verdicts on a `blocked`.
fn unblocks(bench: &Bench) -> Vec<Value> {
    data_of(&bench.events(), EventKind::Decision, "unblock")
}

/// The verdicts of [`unblocks`], in order.
fn verdicts(bench: &Bench) -> Vec<String> {
    unblocks(bench)
        .iter()
        .map(|event| event["unblock"].as_str().unwrap().to_owned())
        .collect()
}

/// The last resolver brief's open verdicts.
fn open_verdicts(bench: &Bench) -> Vec<UnblockVerdict> {
    bench.last_of(Role::Resolver).unblock.unwrap().verdicts
}

/// OWL-620's first acceptance: a Build `blocked` on a permission it
/// misread gets one resolver run, briefed with the run and every verdict
/// but those its stage cannot act on; its `retry` is a DECISION comment,
/// kept with its bound, and the Build runs again with it in its thread,
/// then ends `done` and is delivered.
#[test]
fn a_build_blocked_on_a_misread_permission_runs_again_and_delivers() {
    let bench = Bench::new(true);
    let (outcome, printed) = bench.run(
        vec![
            bench.reply(None, Some(MISREAD)),
            bench.read(&verdict(
                "retry",
                "The shell reaches the network; network false only means no web tools.",
            )),
            bench.reply(Some("Hello"), Some(DONE)),
        ],
        None,
    );
    outcome.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    let briefs = bench.briefs();
    let roles: Vec<Role> = briefs.iter().map(|brief| brief.role).collect();
    assert_eq!(
        roles,
        [Role::Build, Role::Resolver, Role::Build, Role::Verify]
    );
    let resolver = &briefs[1];
    let asked = resolver.asked_by.as_ref().unwrap();
    assert_eq!(asked.role, Role::Build);
    assert!(asked.summary.contains("network false"), "{asked:?}");
    let unblock = resolver.unblock.as_ref().unwrap();
    assert_eq!(unblock.stage, Stage::Build);
    // A trivial ticket has no plan to owe a step of, and its branch holds
    // the base's head.
    assert_eq!(
        unblock.verdicts,
        [UnblockVerdict::Retry, UnblockVerdict::Park]
    );
    assert!(resolver.resolve.is_empty());
    let decision = bench
        .comments()
        .into_iter()
        .find(|c| c.starts_with("[owlshift] DECISION"))
        .unwrap();
    assert!(
        decision.contains("Run the stage again: The shell reaches the network"),
        "{decision}"
    );
    assert!(
        briefs[2].thread.iter().any(|entry| matches!(
            entry,
            ThreadEntry::Decision { decision, .. } if decision.starts_with("Run the stage again")
        )),
        "{:?}",
        briefs[2].thread
    );
    let kept = &bench.record().questions.decisions;
    assert_eq!(
        kept.iter()
            .map(|d| d.unblock.map(|u| u.verdict))
            .collect::<Vec<_>>(),
        [Some(UnblockVerdict::Retry)]
    );
    assert_eq!(verdicts(&bench), ["retry"]);
    assert_eq!(unblocks(&bench)[0]["stage"], "build");
}

/// OWL-620: a second `blocked` after a `retry` at the same stage has `park`
/// alone open; the ticket parks, the resolver's sentence on what the person
/// must do first leading the PARKED comment.
#[test]
fn a_second_blocked_after_a_retry_parks_with_the_resolvers_sentence_first() {
    let bench = Bench::new(true);
    let (outcome, printed) = bench.run(
        vec![
            bench.reply(None, Some(MISREAD)),
            bench.read(&verdict("retry", "The shell reaches the network.")),
            bench.reply(None, Some(MISREAD)),
            bench.read(&verdict(
                "park",
                "Add the registry's token to the keychain, then continue.",
            )),
        ],
        None,
    );
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
    assert_eq!(open_verdicts(&bench), [UnblockVerdict::Park]);
    let parked = bench
        .comments()
        .into_iter()
        .find(|c| c.starts_with("[owlshift] PARKED"))
        .unwrap();
    assert!(
        parked.starts_with(
            "[owlshift] PARKED\n\n**First, for a person:** Add the registry's token to the \
             keychain, then continue.\n\nParked: the run is blocked. The run said:"
        ),
        "{parked}"
    );
    assert_eq!(verdicts(&bench), ["retry", "park"]);
    // A park posts no DECISION: one, the retry's.
    assert_eq!(
        bench
            .comments()
            .iter()
            .filter(|c| c.starts_with("[owlshift] DECISION"))
            .count(),
        1
    );
}

/// OWL-620: below the delegation level `discoverable` nothing changes: a
/// `blocked` parks as before, with no resolver run and no `unblock` event.
#[test]
fn below_discoverable_a_blocked_parks_as_before() {
    let bench = Bench::new(true);
    bench.policy("resolver_delegation = \"facts\"");
    let (outcome, printed) = bench.run(vec![bench.reply(None, Some(MISREAD))], None);
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
    assert!(bench.briefs().iter().all(|b| b.role != Role::Resolver));
    assert!(unblocks(&bench).is_empty());
    let parked = bench
        .comments()
        .into_iter()
        .find(|c| c.starts_with("[owlshift] PARKED"))
        .unwrap();
    assert!(!parked.contains("**First"), "{parked}");
}

/// OWL-620: the resolver's run counts in `policy.resolver_passes` as any
/// resolver run's: a chain that reached its bound runs none on a `blocked`,
/// and the ticket parks, the event saying why.
#[test]
fn a_chain_at_its_bound_parks_a_blocked_without_a_resolver_run() {
    let bench = Bench::new(true);
    bench.policy("resolver_passes = 1");
    let asks = r#"{"format":18,"status":"questions","summary":"Asked where the greeting goes.",
"questions":[{"id":"Q1","category":"naming","context":"The repository root holds README.md.",
"text":"Where does the greeting go?","recommendation":{"why":"GREETING.md at the root."}}]}"#;
    let decides = r#"{"format":18,"status":"done","summary":"The ticket names it.",
"resolutions":[{"question":"Q1","outcome":"decided","category":"naming",
"decision":"GREETING.md at the root.","basis":"The ticket names GREETING.md.","basis_kind":"settled"}]}"#;
    let (outcome, printed) = bench.run(
        vec![
            bench.reply(None, Some(asks)),
            bench.read(decides),
            bench.reply(None, Some(MISREAD)),
        ],
        None,
    );
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
    let roles: Vec<Role> = bench.briefs().iter().map(|b| b.role).collect();
    assert_eq!(roles, [Role::Build, Role::Resolver, Role::Build]);
    let told = unblocks(&bench);
    assert_eq!(
        (
            &told[0]["unblock"],
            &told[0]["reason"],
            &told[0]["resolver"]
        ),
        (&json!("park"), &json!("pass_limit"), &Value::Null),
        "{told:?}"
    );
}

/// OWL-620: one `blocked` on its base is rebased. The Build's first stop
/// parks on the resolver's word; the base gains what the work needs; the
/// restarted Build stops again, and the resolver, `retry` and `rebase` still
/// open since a park closes nothing, gives the branch to the rebase run,
/// after which the Build delivers on the combined code.
#[test]
fn a_build_blocked_on_its_base_is_rebased_and_delivers() {
    let bench = Bench::new(true);
    let mut committing = bench.reply(None, Some(BEHIND));
    committing.files.insert(
        RelativePath::new("app/one.rs").unwrap(),
        "// one\n".to_owned(),
    );
    committing.commit = Some("Add app/one.rs".to_owned());
    let (parked, printed) = bench.run(
        vec![
            committing,
            bench.read(&verdict("park", "Merge TWO.md into main first.")),
        ],
        None,
    );
    assert!(
        matches!(&parked, Err(Stop::Parked { .. })),
        "{parked:?}\n{printed}"
    );
    // At the first stop the base had not moved: no rebase to offer.
    assert!(!open_verdicts(&bench).contains(&UnblockVerdict::Rebase));

    let main = bench.merge_into_main(&[("TWO.md", "// two\n")]);
    let (env, worktree) = (bench.env.clone(), bench.worktree());
    let onto = main.clone();
    let rebase: During = (
        3,
        Box::new(move || {
            env.run(&worktree, &["rebase", "--quiet", &onto]).unwrap();
        }),
    );
    let (delivered, printed) = bench.continue_during(
        vec![
            bench.reply(None, Some(BEHIND)),
            bench.read(&verdict("rebase", "The base now holds TWO.md.")),
            bench.reply(None, Some(REBASED)),
            bench.reply(Some("Hello"), Some(DONE)),
        ],
        vec![rebase],
    );
    delivered.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    assert_eq!(
        open_verdicts(&bench),
        [
            UnblockVerdict::Retry,
            UnblockVerdict::Rebase,
            UnblockVerdict::Park
        ]
    );
    let roles: Vec<Role> = bench.briefs().iter().map(|b| b.role).collect();
    assert_eq!(
        roles[2..],
        [
            Role::Build,
            Role::Resolver,
            Role::Rebase,
            Role::Build,
            Role::Verify
        ]
    );
    assert_eq!(verdicts(&bench), ["park", "rebase"]);
    let rebased = data_of(&bench.events(), EventKind::Decision, "unblocked");
    assert_eq!(rebased[0]["onto"], json!(main), "{rebased:?}");
    // The Build's gate and the delivery ran on the combined code.
    assert!(is_ancestor(&bench, &main, &bench.remote_branch().unwrap()));
}

/// OWL-620: one `blocked` on its last step, which only a person can do, is
/// delivered with that step owed: the resolver hands it to a person, due
/// before the merge, kept with the plan, whose text stays as Design wrote
/// it; the Build runs again given the step as a person's and ends `done`,
/// and the pull request lists it under "For a person".
#[test]
fn a_build_blocked_on_its_last_step_is_delivered_with_that_step_owed() {
    let bench = Bench::new(true);
    bench.add_labels(&["variant:standard"]);
    let (outcome, printed) = bench.run(
        vec![
            bench.read(&designed(BROWSER_PLAN)),
            bench.read(&reviewed(&[])),
            bench.read(&reviewed(&[])),
            bench.reply(Some("Hello"), Some(AT_A_BROWSER)),
            bench.read(&verdict(
                "person_step",
                "A person reads GREETING.md on staging in a browser.",
            )),
            bench.reply(None, Some(DONE_OWING)),
        ],
        None,
    );
    outcome.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    let resolver = bench.last_of(Role::Resolver);
    assert_eq!(
        resolver.unblock.as_ref().unwrap().verdicts,
        [
            UnblockVerdict::Retry,
            UnblockVerdict::PersonStep,
            UnblockVerdict::Park
        ]
    );
    let plan = bench.record().plan.unwrap();
    assert_eq!(plan.handed.len(), 1);
    assert_eq!(
        (plan.handed[0].step.get(), plan.handed[0].when),
        (2, StepWhen::BeforeMerge)
    );
    assert!(
        !plan.plan.steps[1].is_person(),
        "the plan's text as written"
    );
    // The Build after the verdict was given step 2 as a person's.
    let given = bench.last_of(Role::Build).plan.unwrap();
    assert!(given.plan.steps[1].is_person());
    let body = bench.github.created.lock().unwrap()[0]["body"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        body.contains("## For a person")
            && body
                .contains("**Step 2, before the merge, a person's:** Read GREETING.md on staging"),
        "{body}"
    );
    let decision = bench
        .comments()
        .into_iter()
        .find(|c| c.starts_with("[owlshift] DECISION"))
        .unwrap();
    assert!(
        decision.contains("Step 2 is a person's, due before the merge"),
        "{decision}"
    );
    let told = unblocks(&bench);
    assert_eq!(
        (&told[0]["unblock"], &told[0]["step"], &told[0]["when"]),
        (&json!("person_step"), &json!(2), &json!("before_merge")),
        "{told:?}"
    );
}

/// OWL-620: a fix run at Watch that stops `blocked` gets one resolver run,
/// `retry` and `park` alone open there; its `retry` runs the fix again,
/// whose push lands, the ticket staying at Watch.
#[test]
fn a_fix_run_blocked_at_watch_runs_again_and_its_push_lands() {
    let bench = delivered();
    bench.github.followed.lock().unwrap().conclusion = "FAILURE";
    let (fixed, printed) = bench.continue_ticket(vec![
        bench.reply(None, Some(MISREAD)),
        bench.read(&verdict("retry", "The shell reaches the network.")),
        bench.reply(Some("Hello again"), Some(FIXED)),
    ]);
    fixed.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    let resolver = bench.last_of(Role::Resolver);
    let unblock = resolver.unblock.unwrap();
    assert_eq!(unblock.stage, Stage::Watch);
    assert_eq!(
        unblock.verdicts,
        [UnblockVerdict::Retry, UnblockVerdict::Park]
    );
    assert_eq!(verdicts(&bench), ["retry"]);
    assert_eq!(state_of(&bench).status(), Status::Active(Stage::Watch));
}
