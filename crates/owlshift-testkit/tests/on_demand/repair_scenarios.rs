//! OWL-622's acceptance on the bench of `on_demand.rs` (architecture section
//! 5, "A broken base", "The repair ticket"): a hold files one repair ticket,
//! which the runner admits itself, dispatches first whatever the project's
//! capacity counts and builds while the hold stands; the switch turns it
//! off; a tracker that refuses leaves the hold told; a hold that lifts first
//! is told on the ticket. OWL-639's: a repair merged with the base still
//! failing is followed by one more, then by a person.

use owlshift_core::base::{Failing, Repair, Step};
use owlshift_runner::base;

use super::base_scenarios::{
    REBASED_HELD, agent_git, builds, checks_made, fixable_setup, held_done_build, held_rebases,
    setup_line, with_setup,
};
use super::drain_scenarios::intake_declaring;
use super::*;

/// The repair the bench's Markdown tracker files after DEMO-1 and DEMO-2.
const REPAIR: &str = "DEMO-3";

/// A done Build with no pull request intent, so that it delivers on any
/// ticket's branch.
const DONE_ANY: &str = r#"{"format":18,"status":"done","summary":"Done; the gate passes."}"#;

/// The `decision` events of the repairs kept (`base` = `repair`).
fn repairs_kept(bench: &Bench) -> Vec<Value> {
    data_of(&bench.events(), EventKind::Decision, "base")
        .into_iter()
        .filter(|data| data["base"] == json!("repair"))
        .collect()
}

/// The `warning` events `what` = `what`.
fn warned(bench: &Bench, what: &str) -> Vec<Value> {
    data_of(&bench.events(), EventKind::Warning, "what")
        .into_iter()
        .filter(|data| data["what"] == json!(what))
        .collect()
}

/// The repair kept on the base check of `commit`, whatever its commands.
fn kept_repair(bench: &Bench, commit: &str) -> Option<Repair> {
    let checks = base::read(&bench.dirs()).unwrap();
    checks
        .checks
        .iter()
        .filter(|check| check.commit == commit)
        .find_map(|check| checks.repair(&check.commit, &check.commands).cloned())
}

/// The draining bench, its project's `[policy]` given `policy`.
fn repairing_bench(policy: &str) -> Bench {
    let bench = Bench::draining("maintainer");
    if !policy.is_empty() {
        bench.policy(policy);
    }
    bench
}

/// A run of `ticket` that commits `file`, ending done.
fn committing(bench: &Bench, file: &str) -> Reply {
    let mut reply = bench.reply(None, Some(DONE_ANY));
    reply
        .files
        .insert(RelativePath::new(file).unwrap(), format!("{file}\n"));
    reply.commit = Some(format!("Add {file}"));
    reply
}

/// What happens while the `run`th run works: `marker` removed, as the
/// operator's package store losing a version.
fn lose(marker: &Path, run: usize) -> During {
    let lost = marker.to_owned();
    (run, Box::new(move || fs::remove_file(&lost).unwrap()))
}

/// While the `run`th run works, when it is `id`'s rebase run, the agent
/// rebases that ticket's worktree onto the commit `onto` holds then: watch
/// orders the held tickets' steps as their holds lift, so either may come
/// first.
fn rebase_during(bench: &Bench, id: &str, onto: &Rc<RefCell<String>>, run: usize) -> During {
    let (env, onto) = (bench.env.clone(), onto.clone());
    let worktree = bench.dirs().worktree(&TicketId::new(id).unwrap());
    (
        run,
        Box::new(move || {
            let brief = fs::read_to_string(worktree.join(".owlshift/run/brief.json"));
            let rebasing = brief.is_ok_and(|brief| brief.contains("\"role\": \"rebase\""));
            let onto = onto.borrow().clone();
            let contains = env
                .run(&worktree, &["merge-base", "--is-ancestor", &onto, "HEAD"])
                .is_ok();
            if rebasing && !contains {
                env.run(&worktree, &["rebase", "--quiet", &onto]).unwrap();
            }
        }),
    )
}

/// OWL-622's acceptance, one `watch --drain` with `policy.max_in_flight =
/// 2`: DEMO-1 and DEMO-2 are taken in, then DEMO-1's Build runs while the
/// store loses a version, so its gate's setup fails after its `done`, on
/// the base too. The hold files one repair ticket, urgent, in the ready
/// column with the admission label, assigned to the operator. DEMO-2's
/// Build, dispatched next, is held by the same hold before its run, and
/// nothing is filed again. The drain takes the repair in with no person's
/// gesture and dispatches it though the two held Builds take the project's
/// whole capacity and DEMO-1 holds the zone its intake declared; its Build
/// starts through the setup failure it was filed for and its gate passes
/// once it commits the fix. Merged, `main`'s head is checked and passes:
/// DEMO-1's done Build is rebased onto it and runs its gate alone, and
/// DEMO-2's, with no commit of its own, moves there at its step into Build
/// and runs.
#[test]
fn a_broken_base_files_its_repair_which_runs_first_and_frees_the_held_builds() {
    let bench = repairing_bench("max_in_flight = 2");
    bench.put_ticket(
        "DEMO-2",
        "maintainer",
        "Todo",
        &["agent"],
        "",
        "Document it.",
    );
    let marker = bench.tmp.path().join("store-has-rollup");
    fs::write(&marker, "").unwrap();
    let held = with_setup(&bench, &fixable_setup(&marker), None);
    let mut fix = committing(&bench, "FIXED");
    fix.commit = Some("Bring the store's version along".to_owned());
    let merged = Rc::new(RefCell::new(String::new()));
    let replies = vec![
        bench.reply(None, Some(&intake_declaring(&["app"], &[]))),
        bench.reply(None, Some(&intake_declaring(&["docs"], &[]))),
        committing(&bench, "app/ONE.md"),
        bench.reply(None, Some(&intake_declaring(&["app"], &[]))),
        fix,
        // DEMO-1's rebase run and DEMO-2's Build, in either order.
        bench.reply(None, Some(REBASED_HELD)),
        bench.reply(None, Some(REBASED_HELD)),
    ];
    let during = vec![
        lose(&marker, 3),
        rebase_during(&bench, "DEMO-1", &merged, 6),
        rebase_during(&bench, "DEMO-1", &merged, 7),
    ];
    let reopened = Cell::new(false);
    let (outcomes, lines, printed) = bench.drain_during(replies, during, 12, |_| {
        if merged.borrow().is_empty() {
            if bench
                .record_of(REPAIR)
                .and_then(|record| record.delivery)
                .is_some()
            {
                *merged.borrow_mut() = bench.merge(REPAIR);
            }
        } else if !reopened.get() {
            bench.reopened();
            reopened.set(true);
        }
        // Past watch's retry hold of the held Builds.
        let later = bench.clock.get().checked_add(SignedDuration::from_mins(11));
        bench.clock.set(later.unwrap());
    });
    let report = format!("{outcomes:?}\n{lines}\n{printed}");
    let merged = merged.borrow().clone();
    assert!(!merged.is_empty(), "the repair was delivered: {report}");
    let events = bench.events();

    // One repair, filed by DEMO-1's hold.
    let kept = repairs_kept(&bench);
    assert_eq!(kept.len(), 1, "one ticket a hold: {kept:?}");
    assert_eq!(
        (&kept[0]["ticket"], &kept[0]["commit"], &kept[0]["reused"]),
        (&json!(REPAIR), &json!(held), &json!(false))
    );
    let filed = MarkdownTracker::new(&bench.remote.checkout)
        .ticket(&TicketId::new(REPAIR).unwrap())
        .unwrap();
    assert!(
        filed.title.starts_with("Repair the base: `test -f")
            && filed.title.ends_with(&format!("fails at {}", &held[..12])),
        "{}",
        filed.title
    );
    assert_eq!(
        (
            filed.priority,
            filed.labels.clone(),
            filed.assignee.as_deref()
        ),
        (
            owlshift_contracts::Priority::Urgent,
            vec!["agent".to_owned()],
            Some("maintainer")
        )
    );
    assert!(
        filed.description.contains("fails its setup command")
            && filed
                .description
                .contains(&format!("owlshift gate --at {held}")),
        "{}",
        filed.description
    );
    assert!(matches!(
        kept_repair(&bench, &held),
        Some(Repair::Filed { ticket, .. }) if ticket == REPAIR
    ));
    // DEMO-2 held at the same commit before any run of its own.
    let held_stops: Vec<String> = events
        .iter()
        .filter(|e| e.kind == EventKind::Decision && e.data.get("base") == Some(&json!("waits")))
        .filter_map(|e| e.ticket.as_ref().map(|t| t.as_str().to_owned()))
        .collect();
    assert!(
        held_stops.iter().any(|t| t == "DEMO-1") && held_stops.iter().any(|t| t == "DEMO-2"),
        "{held_stops:?}"
    );

    // Taken in and dispatched by the runner's own admission, past the
    // capacity the two held Builds take, with no wait for it told.
    let intakes: Vec<(String, Value)> = events
        .iter()
        .filter(|e| e.kind == EventKind::Decision && e.data.get("drain") == Some(&json!("intake")))
        .map(|e| {
            (
                e.ticket.as_ref().unwrap().as_str().to_owned(),
                e.data["account"].clone(),
            )
        })
        .collect();
    assert_eq!(
        intakes,
        [
            ("DEMO-1".to_owned(), json!("maintainer")),
            ("DEMO-2".to_owned(), json!("maintainer")),
            (REPAIR.to_owned(), json!(base::RUNNER_GESTURE)),
        ],
        "{report}"
    );
    let drained: Vec<&str> = events
        .iter()
        .filter(|e| e.kind == EventKind::Dispatch && e.data["command"] == json!("drain"))
        .filter_map(|e| e.ticket.as_ref().map(TicketId::as_str))
        .collect();
    assert_eq!(drained, ["DEMO-1", "DEMO-2", REPAIR], "{report}");
    let repair_waits: Vec<Value> = super::drain_scenarios::waits(&events)
        .into_iter()
        .filter(|(ticket, ..)| ticket == REPAIR)
        .map(|(_, reason, _)| reason)
        .collect();
    assert!(
        !repair_waits.contains(&json!("slot")) && !repair_waits.contains(&json!("collision")),
        "{repair_waits:?}"
    );
    assert!(
        !events
            .iter()
            .any(|e| e.ticket.as_ref().is_some_and(|t| t.as_str() == REPAIR)
                && e.data.get("reason") == Some(&json!("collision"))),
        "{report}"
    );

    // Its Build started through the setup failure it was filed for; no
    // other Build did.
    let briefs = bench.briefs();
    let builds_of = |id: &str| -> Vec<&Brief> {
        briefs
            .iter()
            .filter(|brief| brief.role == Role::Build && brief.ticket.id.as_str() == id)
            .collect()
    };
    let repair_builds = builds_of(REPAIR);
    assert_eq!(repair_builds.len(), 1, "{report}");
    assert!(
        repair_builds[0]
            .setup_failure
            .as_ref()
            .is_some_and(|failure| failure.command.as_deref() == Some(&*fixable_setup(&marker))),
        "{:?}",
        repair_builds[0].setup_failure
    );
    assert!(
        briefs
            .iter()
            .filter(|brief| brief.ticket.id.as_str() != REPAIR)
            .all(|brief| brief.setup_failure.is_none())
    );
    assert!(is_ancestor(&bench, &held, &merged));

    // The merge's head checked and passing; DEMO-1's done Build rebased onto
    // it, its gate then run alone; DEMO-2's Build run there.
    let checks = checks_made(&bench);
    assert!(
        checks
            .iter()
            .any(|(commit, _, verdict)| *commit == merged && verdict == "passed"),
        "{checks:?}"
    );
    let rebased: Vec<(Value, Value)> = held_rebases(&bench)
        .into_iter()
        .map(|data| (data["from"].clone(), data["onto"].clone()))
        .collect();
    assert_eq!(rebased, [(json!(held), json!(merged))], "{report}");
    let gates_alone = builds(&bench).into_iter().filter(|alone| *alone).count();
    assert_eq!(gates_alone, 1, "{report}");
    assert_eq!(builds_of("DEMO-2").len(), 1, "built once, on the merge");
    for id in ["DEMO-1", "DEMO-2"] {
        assert!(
            events.iter().any(|event| event.kind == EventKind::Dispatch
                && event.ticket.as_ref().is_some_and(|t| t.as_str() == id)
                && event.data["configured_at"] == json!(merged)),
            "{id} configured at the merge: {report}"
        );
    }
}

/// OWL-622: with `policy.base_repair = false` at the base's head, a hold
/// is told as before and nothing is filed.
#[test]
fn with_the_switch_off_a_hold_files_nothing() {
    let bench = repairing_bench("base_repair = false");
    let marker = bench.tmp.path().join("store-has-rollup");
    let held = with_setup(&bench, &setup_line(&marker), None);
    let (outcome, printed) = bench.run(Vec::new(), None);
    assert!(
        matches!(&outcome, Err(Stop::BaseHeld { commit, .. }) if *commit == held),
        "{outcome:?}\n{printed}"
    );
    assert!(repairs_kept(&bench).is_empty());
    assert!(warned(&bench, "repair_unfiled").is_empty());
    assert_eq!(kept_repair(&bench, &held), None);
    assert!(!bench.remote.checkout.join("tickets/DEMO-2").exists());
    assert_eq!(bench.notified.0.borrow().len(), 1, "the hold told");
}

/// OWL-622: a tracker that refuses the filing leaves the hold told as
/// before, with a `repair_unfiled` warning, and no filing tried again for
/// that hold.
#[test]
fn a_tracker_that_refuses_the_repair_leaves_the_hold_told() {
    let bench = repairing_bench("");
    bench.inbox.borrow_mut().refuse = Some("usage limit exceeded");
    let marker = bench.tmp.path().join("store-has-rollup");
    let held = with_setup(&bench, &setup_line(&marker), None);
    let (outcome, printed) = bench.run(Vec::new(), None);
    assert!(
        matches!(&outcome, Err(Stop::BaseHeld { commit, .. }) if *commit == held),
        "{outcome:?}\n{printed}"
    );
    let unfiled = warned(&bench, "repair_unfiled");
    assert_eq!(unfiled.len(), 1, "{unfiled:?}");
    assert!(
        unfiled[0]["reason"]
            .as_str()
            .unwrap()
            .contains("usage limit exceeded"),
        "{unfiled:?}"
    );
    assert_eq!(kept_repair(&bench, &held), Some(Repair::Unfiled));
    assert!(repairs_kept(&bench).is_empty());
    assert_eq!(bench.notified.0.borrow().len(), 1, "the hold told");
    let (again, printed) = bench.continue_ticket(Vec::new());
    assert!(
        matches!(again, Err(Stop::BaseHeld { .. })),
        "{again:?}\n{printed}"
    );
    assert_eq!(warned(&bench, "repair_unfiled").len(), 1, "tried once");
}

/// OWL-622: an open repair found again by its title whose text cannot be
/// read is not kept, which an empty hash would never admit: the hold stays
/// untried, warned as `repair_unfiled`, and its next step tries again,
/// keeping the ticket once it reads.
#[test]
fn a_repair_found_but_unread_is_tried_again() {
    let bench = repairing_bench("");
    let marker = bench.tmp.path().join("store-has-rollup");
    let held = with_setup(&bench, &setup_line(&marker), None);
    let failing = owlshift_core::base::Failing {
        step: owlshift_core::base::Step::Setup,
        command: setup_line(&marker),
    };
    let found = TicketId::new("DEMO-7").unwrap();
    bench.inbox.borrow_mut().tickets.push((
        found.clone(),
        base::repair_title(&held, &failing),
        String::new(),
    ));
    let (outcome, printed) = bench.run(Vec::new(), None);
    assert!(
        matches!(&outcome, Err(Stop::BaseHeld { .. })),
        "{outcome:?}\n{printed}"
    );
    assert_eq!(kept_repair(&bench, &held), None, "not kept: tried again");
    assert_eq!(warned(&bench, "repair_unfiled").len(), 1);
    assert!(repairs_kept(&bench).is_empty());

    bench.put_ticket(
        "DEMO-7",
        "maintainer",
        "Todo",
        &["agent"],
        "",
        "The repair.",
    );
    let (again, printed) = bench.continue_ticket(Vec::new());
    assert!(
        matches!(again, Err(Stop::BaseHeld { .. })),
        "{again:?}\n{printed}"
    );
    assert!(matches!(
        kept_repair(&bench, &held),
        Some(Repair::Filed { ticket, text_sha256 }) if ticket == "DEMO-7" && !text_sha256.is_empty()
    ));
    let kept = repairs_kept(&bench);
    assert_eq!(
        (kept.len(), &kept[0]["ticket"], &kept[0]["reused"]),
        (1, &json!("DEMO-7"), &json!(true))
    );
}

/// OWL-622: a hold that lifts before its repair merged, the store fixed on
/// the machine, is told on the repair ticket, which the runner no longer
/// admits: it is its decider's.
#[test]
fn a_hold_that_lifts_first_is_told_on_its_repair() {
    let bench = repairing_bench("");
    let marker = bench.tmp.path().join("store-has-rollup");
    let held = with_setup(&bench, &setup_line(&marker), None);
    let (outcome, printed) = bench.run(Vec::new(), None);
    assert!(
        matches!(&outcome, Err(Stop::BaseHeld { .. })),
        "{outcome:?}\n{printed}"
    );
    let repair = "DEMO-2";
    assert_eq!(base::repairs(&bench.dirs()).len(), 1);
    // The store is fixed and `main` moves: the held commit's check again
    // passes.
    fs::write(&marker, "").unwrap();
    bench.merge_into_main(&[("NOTES.md", "notes\n")]);
    let (delivered, printed) = bench.continue_ticket(vec![bench.reply(Some("Hello"), Some(DONE))]);
    delivered.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    let told: Vec<String> = bench
        .comments_on(repair)
        .into_iter()
        .filter(|comment| comment.starts_with("[owlshift] LIFTED\n"))
        .collect();
    assert_eq!(told.len(), 1, "{:?}", bench.comments_on(repair));
    assert!(told[0].contains(&format!("`{held}`")), "{}", told[0]);
    assert!(base::repairs(&bench.dirs()).is_empty());
}

/// Moves `id` to the tracker's `Done` stage, as its merge does there.
fn finish(bench: &Bench, id: &str) {
    let path = bench
        .remote
        .checkout
        .join("tickets")
        .join(id)
        .join("ticket.md");
    let text = fs::read_to_string(&path).unwrap();
    let stage = text
        .lines()
        .find(|line| line.starts_with("stage = "))
        .unwrap()
        .to_owned();
    fs::write(&path, text.replacen(&stage, "stage = \"Done\"", 1)).unwrap();
}

/// `outcome`, a stop held at `commit`.
fn held_at(outcome: &Result<Delivered, Stop>, commit: &str) -> bool {
    matches!(outcome, Err(Stop::BaseHeld { commit: at, .. }) if at == commit)
}

/// OWL-639's acceptance: DEMO-1's done Build is held at its base and DEMO-2
/// filed. DEMO-2 merges without fixing the base before the tracker shows it
/// done: DEMO-1's next step checks the merge, holds it and takes DEMO-2
/// again, telling nothing new. Once DEMO-2 shows done, the next step at the
/// same head files DEMO-3, which names DEMO-2. DEMO-3 merges without fixing
/// it either: the new head is held, no third repair is filed, the warning
/// names both, and the operator is told that a person takes it from here.
/// No step of those runs anything or counts a failure. A real fix then
/// passes, and DEMO-1 is rebased onto it and delivered.
#[test]
fn a_repair_merged_on_a_base_still_broken_files_one_more_then_waits_for_a_person() {
    let bench = repairing_bench("");
    let marker = bench.tmp.path().join("store-has-rollup");
    let (held, _) = held_done_build(&bench, &marker);
    let failing = Failing {
        step: Step::Setup,
        command: fixable_setup(&marker),
    };
    let runs = bench.briefs().len();
    assert_eq!(bench.notified.0.borrow().len(), 1, "the break told");

    let first = bench.merge_into_main(&[("REPAIR-1.md", "tried\n")]);
    let (outcome, printed) = bench.continue_ticket(Vec::new());
    assert!(held_at(&outcome, &held), "{outcome:?}\n{printed}");
    let opened = |head: &str| (head.to_owned(), "opened".to_owned(), "failed".to_owned());
    assert!(checks_made(&bench).contains(&opened(&first)), "{printed}");
    assert!(matches!(
        kept_repair(&bench, &first),
        Some(Repair::Filed { ticket, .. }) if ticket == "DEMO-2"
    ));
    assert_eq!(bench.notified.0.borrow().len(), 1, "the break told once");

    finish(&bench, "DEMO-2");
    let (outcome, printed) = bench.continue_ticket(Vec::new());
    assert!(held_at(&outcome, &held), "{outcome:?}\n{printed}");
    assert!(matches!(
        kept_repair(&bench, &first),
        Some(Repair::Filed { ticket, .. }) if ticket == "DEMO-3"
    ));
    let kept: Vec<(Value, Value, Value)> = repairs_kept(&bench)
        .into_iter()
        .map(|data| {
            (
                data["ticket"].clone(),
                data["reused"].clone(),
                data["attempt"].clone(),
            )
        })
        .collect();
    assert_eq!(
        kept,
        [
            (json!("DEMO-2"), json!(false), json!(1)),
            (json!("DEMO-2"), json!(true), json!(1)),
            (json!("DEMO-3"), json!(false), json!(2)),
        ]
    );
    let second_repair = bench.shown("DEMO-3");
    assert!(
        second_repair
            .description
            .contains("**Repairs before this one:** DEMO-2 was filed for this break"),
        "{}",
        second_repair.description
    );

    finish(&bench, "DEMO-3");
    let second = bench.merge_into_main(&[("REPAIR-2.md", "tried again\n")]);
    let (outcome, printed) = bench.continue_ticket(Vec::new());
    assert!(held_at(&outcome, &held), "{outcome:?}\n{printed}");
    assert!(checks_made(&bench).contains(&opened(&second)), "{printed}");
    assert_eq!(kept_repair(&bench, &second), Some(Repair::Unfiled));
    assert!(!bench.remote.checkout.join("tickets/DEMO-4").exists());
    assert_eq!(repairs_kept(&bench).len(), 3);
    let unfiled = warned(&bench, "repair_unfiled");
    assert!(
        unfiled.len() == 1
            && unfiled[0]["reason"]
                .as_str()
                .unwrap()
                .contains("Owlshift filed DEMO-2 and DEMO-3 for this break"),
        "{unfiled:?}"
    );
    let notified = bench.notified.0.borrow().clone();
    assert_eq!(notified.len(), 2, "{notified:?}");
    assert!(notified[1].contains("files no more repair"), "{notified:?}");
    assert_eq!(bench.briefs().len(), runs, "no run while held");
    let state = TicketState::try_from(&bench.record().state).unwrap();
    assert_eq!(state.failed_runs(), 0);
    assert_eq!(
        base::read(&bench.dirs()).unwrap().break_repairs(&failing),
        ["DEMO-2", "DEMO-3"]
    );

    let fixed = bench.merge_into_main(&[("FIXED", "fixed\n")]);
    let rebase = agent_git(
        &bench,
        1,
        vec!["rebase".to_owned(), "--quiet".to_owned(), fixed.clone()],
    );
    let (delivered, printed) =
        bench.continue_during(vec![bench.reply(None, Some(REBASED_HELD))], vec![rebase]);
    delivered.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    let rebased = held_rebases(&bench);
    assert_eq!(
        (&rebased[0]["from"], &rebased[0]["onto"]),
        (&json!(held), &json!(fixed))
    );
}

/// OWL-639, a held ticket with no commits of its own under a person's
/// `continue`, which does not move it: its step checks the merge of a repair
/// that fixed nothing and holds it. The next repair, found by its title but
/// not readable yet, is not kept; the next `continue` follows the hold and
/// keeps it. Neither step runs anything.
#[test]
fn a_continued_ticket_with_no_commits_of_its_own_checks_the_head_and_holds_it() {
    let bench = repairing_bench("");
    let marker = bench.tmp.path().join("store-has-rollup");
    let held = with_setup(&bench, &setup_line(&marker), None);
    let (outcome, printed) = bench.run(Vec::new(), None);
    assert!(held_at(&outcome, &held), "{outcome:?}\n{printed}");
    let runs = bench.briefs().len();
    finish(&bench, "DEMO-2");
    let head = bench.merge_into_main(&[("REPAIR-1.md", "tried\n")]);
    let failing = Failing {
        step: Step::Setup,
        command: setup_line(&marker),
    };
    let found = TicketId::new("DEMO-7").unwrap();
    bench.inbox.borrow_mut().tickets.push((
        found.clone(),
        base::repair_title(&head, &failing),
        String::new(),
    ));

    let (outcome, printed) = bench.continue_ticket(Vec::new());
    assert!(held_at(&outcome, &held), "{outcome:?}\n{printed}");
    let checked = checks_made(&bench);
    assert!(
        checked.contains(&(head.clone(), "opened".to_owned(), "failed".to_owned())),
        "{checked:?}"
    );
    assert_eq!(kept_repair(&bench, &head), None, "not kept: tried again");
    assert_eq!(warned(&bench, "repair_unfiled").len(), 1);

    bench.put_ticket(
        "DEMO-7",
        "maintainer",
        "Todo",
        &["agent"],
        "",
        "The repair.",
    );
    let (outcome, printed) = bench.continue_ticket(Vec::new());
    assert!(held_at(&outcome, &held), "{outcome:?}\n{printed}");
    assert!(matches!(
        kept_repair(&bench, &head),
        Some(Repair::Filed { ticket, .. }) if ticket == "DEMO-7"
    ));
    assert_eq!(bench.briefs().len(), runs, "no run while held");
}
