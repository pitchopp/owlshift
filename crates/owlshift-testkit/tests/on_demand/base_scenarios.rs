//! OWL-418's acceptance through the commands (architecture section 5,
//! "Parallel", "A broken base"): a setup or gate failure the commit a Build
//! is configured at gives too counts nothing and holds that commit once;
//! the held tickets go on by themselves once it passes, a done Build
//! running only its gate; any other failure takes today's path. The bench's
//! "environment" is a file the base's setup checks, outside the
//! repository, as Locary's offline pnpm store was: removing it breaks every
//! branch cut from that base, before any of the branch's own code runs.

use owlshift_runner::base;

use super::*;

/// The base's setup: red while `marker` is missing.
fn setup_line(marker: &Path) -> String {
    format!("test -f '{}'", marker.display())
}

/// `main` given `setup` as its project file's `stack.setup`, and `gate`
/// as its gate when given; returns the new head, the commit the ticket's
/// Build is configured at.
fn with_setup(bench: &Bench, setup: &str, gate: Option<&str>) -> String {
    let config = fs::read_to_string(bench.remote.checkout.join("owlshift.toml")).unwrap();
    let gate_line = gate.map_or_else(|| GATE_LINE.to_owned(), str::to_owned);
    let config = config.replace(GATE_LINE, &format!("{gate_line}\nsetup = [{setup:?}]"));
    bench.merge_into_main(&[("owlshift.toml", &config)])
}

/// The Build runs started, each with whether it ran the gate alone.
fn builds(bench: &Bench) -> Vec<bool> {
    data_of(&bench.events(), EventKind::RunStarted, "role")
        .into_iter()
        .filter(|data| data["role"] == json!("build"))
        .map(|data| data.get("gate_only") == Some(&json!(true)))
        .collect()
}

/// The changes each base check made, as its `decision` event tells them.
fn checked(bench: &Bench) -> Vec<String> {
    data_of(&bench.events(), EventKind::Decision, "base")
        .into_iter()
        .filter(|data| data["base"] == json!("checked"))
        .map(|data| data["change"].as_str().unwrap().to_owned())
        .collect()
}

/// The `warning` events `what` = `base_unchecked`.
fn unchecked(bench: &Bench) -> usize {
    data_of(&bench.events(), EventKind::Warning, "what")
        .into_iter()
        .filter(|data| data["what"] == json!("base_unchecked"))
        .count()
}

/// The ticket's state, as its ref keeps it.
fn state_of(bench: &Bench) -> (Status, u32) {
    let state = TicketState::try_from(&bench.record().state).unwrap();
    (state.status(), state.failed_runs())
}

/// Criteria 1 and 2: the base's setup breaks before any of the branch's
/// code runs. `do` checks the base once, holds the commit and counts
/// nothing: no harness, no recovery run, no park, one notification; a
/// `continue` is held before its run, with no second check or notice.
#[test]
fn a_setup_the_base_breaks_holds_the_build_and_counts_nothing() {
    let bench = Bench::new(true);
    let marker = bench.tmp.path().join("store-has-rollup");
    let setup = setup_line(&marker);
    let base = with_setup(&bench, &setup, None);

    let (held, printed) = bench.run(Vec::new(), None);
    match &held {
        Err(Stop::BaseHeld {
            commit,
            command,
            pending: false,
            ..
        }) => assert_eq!(
            (commit.as_str(), command.as_deref()),
            (base.as_str(), Some(setup.as_str()))
        ),
        other => panic!("{other:?}\n{printed}"),
    }
    let text = held.unwrap_err().to_string();
    assert!(
        text.contains("nothing was counted")
            && text.contains(&format!("owlshift gate --at {base}")),
        "{text}"
    );
    assert_eq!(state_of(&bench), (Status::Active(Stage::Build), 0));
    assert_eq!(
        builds(&bench),
        [false],
        "one Build run, stopped at its setup"
    );
    assert_eq!(checked(&bench), ["opened"]);
    let told = bench.notified.0.borrow().clone();
    assert_eq!(told.len(), 1, "{told:?}");
    assert!(
        told[0].contains("fails its setup command")
            && told[0].contains(&format!("owlshift gate --at {base}")),
        "{told:?}"
    );
    assert!(base::read(&bench.dirs()).unwrap().holds_commit(&base));
    assert_eq!(bench.stage(), "In Progress");

    let (again, printed) = bench.continue_ticket(Vec::new());
    assert!(
        matches!(again, Err(Stop::BaseHeld { pending: false, .. })),
        "{again:?}\n{printed}"
    );
    assert_eq!(builds(&bench), [false], "held before any run");
    assert_eq!(
        checked(&bench),
        ["opened"],
        "no second check at the same head"
    );
    assert_eq!(bench.notified.0.borrow().len(), 1, "told once per hold");
    assert_eq!(state_of(&bench), (Status::Active(Stage::Build), 0));
}

/// Criterion 3: the store loses its version while the Build works, so the
/// gate's setup breaks after its `done`, on the base too: held, its result
/// kept. Once the store is fixed and `main` moved, the next `continue`
/// checks the base again, lifts the hold, runs only the gate, no harness,
/// and the ticket goes on through Verify to its pull request.
#[test]
fn a_done_build_whose_gate_the_base_breaks_reruns_only_its_gate_once_the_base_passes() {
    let bench = Bench::new(true);
    let marker = bench.tmp.path().join("store-has-rollup");
    fs::write(&marker, "").unwrap();
    let setup = setup_line(&marker);
    let base = with_setup(&bench, &setup, None);
    let lost = marker.clone();
    let lose: During = (1, Box::new(move || fs::remove_file(&lost).unwrap()));

    let (held, printed) = bench.invoke(
        false,
        vec![bench.reply(Some("Hello"), Some(DONE))],
        None,
        vec![lose],
    );
    assert!(
        matches!(&held, Err(Stop::BaseHeld { commit, pending: false, .. }) if *commit == base),
        "{held:?}\n{printed}"
    );
    let wait = base::read_wait(&bench.dirs(), &ticket()).expect("the done Build's wait");
    assert!(wait.done.is_some(), "{wait:?}");
    assert_eq!(state_of(&bench), (Status::Active(Stage::Build), 0));

    fs::write(&marker, "").unwrap();
    bench.merge_into_main(&[("NEWS.md", "news\n")]);
    let (delivered, printed) = bench.continue_ticket(Vec::new());
    delivered.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    assert_eq!(
        builds(&bench),
        [false, true],
        "the gate alone, no Build run"
    );
    assert_eq!(checked(&bench), ["opened", "lifted"]);
    assert!(!base::wait_file(&bench.dirs(), &ticket()).exists());
    assert_eq!(bench.notified.0.borrow().len(), 1);
    assert!(bench.remote_branch().is_some(), "delivered");
}

/// Criterion 5: a base check that cannot run says so in a `warning` event,
/// and the failure takes today's path: the second failed run parks.
#[test]
fn a_base_check_that_cannot_run_takes_todays_path() {
    let bench = Bench::parking();
    let marker = bench.tmp.path().join("store-has-rollup");
    with_setup(&bench, &setup_line(&marker), None);
    // The base check's folder cannot be made.
    fs::create_dir_all(bench.dirs().root()).unwrap();
    fs::write(bench.dirs().root().join("base"), "not a folder").unwrap();

    let (parked, printed) = bench.run(Vec::new(), None);
    assert!(
        matches!(
            parked,
            Err(Stop::Parked {
                reason: ParkReason::FailedRuns,
                ..
            })
        ),
        "{parked:?}\n{printed}"
    );
    assert_eq!(builds(&bench), [false, false]);
    assert_eq!(unchecked(&bench), 2);
    assert!(checked(&bench).is_empty());
    assert!(bench.notified.0.borrow().is_empty());
}

/// Criterion 5: a base check that breaks isolation quarantines the project,
/// as `owlshift gate` does, and the command stops refused.
#[test]
fn a_base_check_that_breaks_isolation_quarantines_the_project() {
    let bench = Bench::new(true);
    // On the base, which has no GREETING.md, the first command tags the
    // repository: a ref the check's own run must leave alone.
    let gate = "gate = [\"test -e GREETING.md || git tag owl418-breach\", \
                \"test ! -e GREETING.md || git grep -q Hello -- GREETING.md\"]";
    let marker = bench.tmp.path().join("store-has-rollup");
    fs::write(&marker, "").unwrap();
    with_setup(&bench, &setup_line(&marker), Some(gate));

    let (refused, printed) = bench.run(vec![bench.reply(Some("Helo"), Some(DONE))], None);
    assert!(
        matches!(refused, Err(Stop::Unverified { .. })),
        "{refused:?}\n{printed}"
    );
    let marker = bench.dirs().quarantine().unwrap().expect("quarantined");
    assert!(marker.contains("broke isolation"), "{marker}");
    assert!(checked(&bench).is_empty());
}

/// Criterion 2 and the busy path: DEMO-2's step in flight, DEMO-1's check
/// cannot run and is left pending, kept per commit, which holds every Build
/// configured there before it starts: DEMO-1's next Build is held with no
/// run while the sibling works, and checks the base once it is alone, one
/// notification.
#[test]
fn a_check_that_must_wait_holds_the_builds_on_its_base_until_it_runs() {
    let bench = Bench::new(true);
    let marker = bench.tmp.path().join("store-has-rollup");
    let base = with_setup(&bench, &setup_line(&marker), None);
    let two = TicketId::new("DEMO-2").unwrap();

    let sibling = bench.dirs().ticket_lock(&two).unwrap().unwrap();
    let (pending, printed) = bench.run(Vec::new(), None);
    assert!(
        matches!(pending, Err(Stop::BaseHeld { pending: true, .. })),
        "{pending:?}\n{printed}"
    );
    let text = pending.unwrap_err().to_string();
    assert!(text.contains("once no other ticket's step runs"), "{text}");
    assert!(base::read(&bench.dirs()).unwrap().holds_commit(&base));
    assert_eq!(state_of(&bench), (Status::Active(Stage::Build), 0));
    let (still, printed) = bench.continue_ticket(Vec::new());
    assert!(
        matches!(still, Err(Stop::BaseHeld { pending: true, .. })),
        "{still:?}\n{printed}"
    );
    assert_eq!(
        builds(&bench),
        [false],
        "held before any run while the check waits"
    );
    assert!(checked(&bench).is_empty());
    assert!(bench.notified.0.borrow().is_empty());
    drop(sibling);

    let (held, printed) = bench.continue_ticket(Vec::new());
    assert!(
        matches!(held, Err(Stop::BaseHeld { pending: false, .. })),
        "{held:?}\n{printed}"
    );
    assert_eq!(builds(&bench), [false], "the check ran before any run");
    assert_eq!(checked(&bench), ["opened"]);
    assert_eq!(bench.notified.0.borrow().len(), 1);
    assert_eq!(state_of(&bench), (Status::Active(Stage::Build), 0));
}

/// Criterion 3 with no person: watch resumes the held ticket at each retry,
/// still held while the base fails, with one WAITING comment; once the
/// store is fixed and `main` moved, its next retry checks the base, lifts
/// the hold, runs only the gate and delivers, the comment folded.
#[test]
fn watch_reruns_a_held_gate_once_the_base_passes_with_no_continue() {
    let bench = Bench::new(true);
    bench.fold.borrow_mut().on = true;
    let marker = bench.tmp.path().join("store-has-rollup");
    fs::write(&marker, "").unwrap();
    let base = with_setup(&bench, &setup_line(&marker), None);
    let lost = marker.clone();
    let lose: During = (1, Box::new(move || fs::remove_file(&lost).unwrap()));
    let (held, printed) = bench.invoke(
        false,
        vec![bench.reply(Some("Hello"), Some(DONE))],
        None,
        vec![lose],
    );
    assert!(
        matches!(&held, Err(Stop::BaseHeld { commit, .. }) if *commit == base),
        "{held:?}\n{printed}"
    );

    let later = |bench: &Bench| {
        bench
            .clock
            .set(bench.clock.get() + SignedDuration::from_mins(11));
    };
    let mut passes = 0;
    let (outcomes, lines, printed) = bench.watch(Vec::new(), &mut |_| {
        passes += 1;
        match passes {
            1 => later(&bench),
            2 => {
                fs::write(&marker, "").unwrap();
                bench.merge_into_main(&[("NEWS.md", "news\n")]);
                later(&bench);
            }
            _ => return false,
        }
        true
    });
    assert!(
        matches!(
            outcomes.as_slice(),
            [
                Err(Stop::BaseHeld { .. }),
                Err(Stop::BaseHeld { .. }),
                Ok(_)
            ]
        ),
        "{outcomes:?}\n{lines}\n{printed}"
    );
    assert_eq!(builds(&bench), [false, true]);
    assert_eq!(checked(&bench), ["opened", "lifted"]);
    let waiting: Vec<String> = bench
        .comments_on(TICKET)
        .into_iter()
        .filter(|c| c.starts_with("[owlshift] WAITING\n"))
        .collect();
    assert_eq!(waiting.len(), 1, "{waiting:?}");
    assert!(
        waiting[0].contains("fails `test -f") && waiting[0].contains("goes on by itself"),
        "{}",
        waiting[0]
    );
    assert_eq!(bench.fold.borrow().resolved.len(), 1, "{lines}");
}

/// The busy path under watch: a check that must wait, DEMO-2's step in
/// flight, is tried again at the next pass once nothing else runs, not
/// after the retry delay: no clock moves between the two passes.
#[test]
fn watch_tries_a_check_that_waited_again_once_nothing_else_runs() {
    let bench = Bench::new(true);
    let marker = bench.tmp.path().join("store-has-rollup");
    with_setup(&bench, &setup_line(&marker), None);
    let two = TicketId::new("DEMO-2").unwrap();
    let mut sibling = Some(bench.dirs().ticket_lock(&two).unwrap().unwrap());
    let (pending, printed) = bench.run(Vec::new(), None);
    assert!(
        matches!(pending, Err(Stop::BaseHeld { pending: true, .. })),
        "{pending:?}\n{printed}"
    );

    let mut passes = 0;
    let (outcomes, lines, printed) = bench.watch(Vec::new(), &mut |_| {
        passes += 1;
        // DEMO-2's step ends after the first pass.
        drop(sibling.take());
        passes < 2
    });
    assert!(
        matches!(
            outcomes.as_slice(),
            [
                Err(Stop::BaseHeld { pending: true, .. }),
                Err(Stop::BaseHeld { pending: false, .. })
            ]
        ),
        "{outcomes:?}\n{lines}\n{printed}"
    );
    assert_eq!(checked(&bench), ["opened"]);
    assert_eq!(builds(&bench), [false]);
}
