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

/// OWL-427's base setup: red while `marker`, the environment, is missing,
/// unless the code carries `FIXED`, the fix a later commit on `main` brings.
fn fixable_setup(marker: &Path) -> String {
    format!("{} || test -f FIXED", setup_line(marker))
}

/// A held Build's rebase run's `done`.
const REBASED_HELD: &str =
    r#"{"format":14,"status":"done","summary":"Rebased onto the fix; nothing conflicted."}"#;

/// What the agent of the `run`th run does in the ticket's worktree: `git`
/// with `args`, which must succeed.
fn agent_git(bench: &Bench, run: usize, args: Vec<String>) -> During {
    let (env, worktree) = (bench.env.clone(), bench.worktree());
    (
        run,
        Box::new(move || {
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            env.run(&worktree, &args).unwrap();
        }),
    )
}

/// The ticket's worktree's head.
fn worktree_head(bench: &Bench) -> String {
    let out = bench
        .env
        .run(&bench.worktree(), &["rev-parse", "HEAD"])
        .unwrap();
    String::from_utf8(out).unwrap().trim().to_owned()
}

/// The `decision` events of the rebases off a held commit.
fn held_rebases(bench: &Bench) -> Vec<Value> {
    data_of(&bench.events(), EventKind::Decision, "base")
        .into_iter()
        .filter(|data| data["base"] == json!("rebase"))
        .collect()
}

/// Each base check's commit, change and verdict, in order.
fn checks_made(bench: &Bench) -> Vec<(String, String, String)> {
    data_of(&bench.events(), EventKind::Decision, "base")
        .into_iter()
        .filter(|data| data["base"] == json!("checked"))
        .map(|data| {
            let text = |key: &str| data[key].as_str().unwrap().to_owned();
            (text("commit"), text("change"), text("verdict"))
        })
        .collect()
}

/// A done Build held at its base: the store loses its version while the
/// Build works, so its gate's setup breaks after its `done`, on the base
/// too. Returns the held commit and the branch's head.
fn held_done_build(bench: &Bench, marker: &Path) -> (String, String) {
    fs::write(marker, "").unwrap();
    let base = with_setup(bench, &fixable_setup(marker), None);
    let lost = marker.to_owned();
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
    assert!(base::read_wait(&bench.dirs(), &ticket()).is_some_and(|wait| wait.done.is_some()));
    (base, worktree_head(bench))
}

/// The Build's uncommitted plan, planted in the worktree's run folder,
/// which a rebase off a held commit keeps.
fn plant_plan(bench: &Bench) -> PathBuf {
    let plan = bench.worktree().join(".owlshift/run/plan.md");
    fs::write(&plan, "1. Add the greeting.\n").unwrap();
    plan
}

/// OWL-427, criteria 1, 2 and 5, with no person: a done Build held at its
/// base, whose environment stays broken, goes on once a fix lands on
/// `main`. Watch's first pass checks the held commit again (still failing),
/// then `main`'s head (passing), rebases the branch there with one rebase
/// run on the fast tier, briefed with the broken commit and no gate, runs
/// only the Build's gate, and delivers onto the fix, the Build's
/// uncommitted plan kept.
#[test]
fn watch_rebases_a_held_done_build_onto_a_head_that_passes() {
    let bench = Bench::new(true);
    let marker = bench.tmp.path().join("store-has-rollup");
    let (held, start) = held_done_build(&bench, &marker);
    let plan = plant_plan(&bench);
    let fixed = bench.merge_into_main(&[("FIXED", "fixed\n")]);
    let rebase = agent_git(
        &bench,
        1,
        vec!["rebase".to_owned(), "--quiet".to_owned(), fixed.clone()],
    );

    let (outcomes, lines, printed) = bench.watch_during(
        vec![bench.reply(None, Some(REBASED_HELD))],
        vec![rebase],
        &mut |_| false,
    );
    assert!(
        matches!(outcomes.last(), Some(Ok(_)))
            && outcomes
                .iter()
                .all(|o| !matches!(o, Err(Stop::Parked { .. }))),
        "{outcomes:?}\n{lines}\n{printed}"
    );
    assert_eq!(
        builds(&bench),
        [false, true],
        "the gate alone after the rebase"
    );
    let brief = bench.last_of(Role::Rebase);
    assert!(brief.gate.is_empty(), "{:?}", brief.gate);
    let told = brief.rebase.expect("what to rebase");
    assert_eq!(
        (
            told.pull_request,
            told.broken.as_deref(),
            told.onto.as_str(),
            told.head.as_str()
        ),
        (None, Some(held.as_str()), fixed.as_str(), start.as_str())
    );
    let passed = |commit: &str, change: &str, verdict: &str| {
        (commit.to_owned(), change.to_owned(), verdict.to_owned())
    };
    assert_eq!(
        checks_made(&bench),
        [
            passed(&held, "opened", "failed"),
            passed(&held, "still_held", "failed"),
            passed(&fixed, "cached", "passed"),
        ]
    );
    let rebases = held_rebases(&bench);
    assert_eq!(rebases.len(), 1, "{rebases:?}");
    assert_eq!(
        (
            &rebases[0]["from"],
            &rebases[0]["onto"],
            &rebases[0]["tier"],
            &rebases[0]["trivial"]
        ),
        (&json!(held), &json!(fixed), &json!("fast"), &json!(true))
    );
    let configured = data_of(&bench.events(), EventKind::Dispatch, "configured_at");
    assert_eq!(configured.last().unwrap()["configured_at"], json!(fixed));
    let delivered = bench.remote_branch().expect("delivered");
    assert!(is_ancestor(&bench, &fixed, &delivered));
    assert!(plan.exists(), "the Build's plan was kept");
    assert!(!base::wait_file(&bench.dirs(), &ticket()).exists());
}

/// OWL-427, criterion 3 and the guardrails: a rebase run that drops the
/// branch's work (here, the branch reset to the fix itself) is refused;
/// the branch goes back to its head before the run, nothing is pushed, and
/// the ticket parks with a reason naming both commits. `continue` then runs
/// the rebase again, which this time keeps the work, and the ticket
/// delivers.
#[test]
fn a_rebase_that_drops_the_work_parks_with_the_branch_where_it_was() {
    let bench = Bench::parking();
    let marker = bench.tmp.path().join("store-has-rollup");
    let (held, start) = held_done_build(&bench, &marker);
    let plan = plant_plan(&bench);
    let fixed = bench.merge_into_main(&[("FIXED", "fixed\n")]);

    let drop_work = agent_git(
        &bench,
        1,
        vec![
            "reset".to_owned(),
            "--hard".to_owned(),
            "--quiet".to_owned(),
            fixed.clone(),
        ],
    );
    let (parked, printed) =
        bench.continue_during(vec![bench.reply(None, Some(REBASED_HELD))], vec![drop_work]);
    assert!(
        matches!(
            parked,
            Err(Stop::Parked {
                reason: ParkReason::Rebase,
                ..
            })
        ),
        "{parked:?}\n{printed}"
    );
    assert_eq!(worktree_head(&bench), start, "the branch back where it was");
    assert!(bench.remote_branch().is_none(), "nothing pushed");
    assert!(plan.exists());
    let parked_comment = bench
        .comments()
        .into_iter()
        .find(|c| c.starts_with("[owlshift] PARKED"))
        .expect("a PARKED comment");
    assert!(
        parked_comment.contains("could not be rebased")
            && parked_comment.contains(base::short(&held))
            && parked_comment.contains(base::short(&fixed))
            && parked_comment.contains("it is the base itself"),
        "{parked_comment}"
    );

    let rebase = agent_git(
        &bench,
        1,
        vec!["rebase".to_owned(), "--quiet".to_owned(), fixed.clone()],
    );
    let (delivered, printed) =
        bench.continue_during(vec![bench.reply(None, Some(REBASED_HELD))], vec![rebase]);
    delivered.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    assert_eq!(held_rebases(&bench).len(), 2);
    assert_eq!(builds(&bench), [false, true]);
    let delivered = bench.remote_branch().expect("delivered");
    assert!(is_ancestor(&bench, &fixed, &delivered));
}

/// OWL-427, criterion 2 for a Build held before its run: its branch carries
/// a first round's commit, and a sibling's failure held the commit it was
/// made from. Once a fix lands, the answered round's `continue` rebases
/// the branch, then runs the Build on it, configured at the fix, its plan
/// kept.
#[test]
fn a_held_build_with_commits_of_its_own_runs_on_the_rebased_branch() {
    use owlshift_core::base::{Check, Checks, Entry, Failing, Failure, Hold, Step, Verdict};

    let bench = Bench::new(true);
    let marker = bench.tmp.path().join("store-has-rollup");
    fs::write(&marker, "").unwrap();
    let setup = fixable_setup(&marker);
    let held = with_setup(&bench, &setup, None);
    let (asked, printed) = bench.run(vec![bench.reply(Some("Hello"), Some(ROUND_1))], None);
    assert!(
        matches!(asked, Err(Stop::NeedsInput { .. })),
        "{asked:?}\n{printed}"
    );
    let plan = plant_plan(&bench);
    // The store loses its version; another ticket's failure held the commit.
    fs::remove_file(&marker).unwrap();
    let gate: Vec<String> =
        vec!["test ! -e GREETING.md || git grep -q Hello -- GREETING.md".to_owned()];
    let failing = Failing {
        step: Step::Setup,
        command: setup.clone(),
    };
    let checks = Checks {
        checks: vec![Check {
            commit: held.clone(),
            commands: base::commands_key(std::slice::from_ref(&setup), &gate),
            entry: Entry::Checked {
                head: held.clone(),
                checked_ms: 0,
                verdict: Verdict::Failed(Failure {
                    failing,
                    reason: "setup: exit status 1".to_owned(),
                    tail: String::new(),
                }),
                hold: Some(Hold {
                    since_ms: 0,
                    told: true,
                }),
            },
        }],
    };
    base::write(&bench.dirs(), &checks).unwrap();
    let fixed = bench.merge_into_main(&[("FIXED", "fixed\n")]);
    bench.answer("Q1: English.\nQ2: \"Hello, reader.\"\ngo");

    let answered = check(&[
        ("Q1", "answered", "English."),
        ("Q2", "answered", "Hello, reader."),
    ]);
    let rebase = agent_git(
        &bench,
        2,
        vec!["rebase".to_owned(), "--quiet".to_owned(), fixed.clone()],
    );
    let (delivered, printed) = bench.continue_during(
        vec![
            bench.reply(None, Some(&answered)),
            bench.reply(None, Some(REBASED_HELD)),
            bench.reply(None, Some(DONE)),
        ],
        vec![rebase],
    );
    delivered.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    let roles: Vec<Value> = data_of(&bench.events(), EventKind::RunStarted, "role")
        .into_iter()
        .map(|data| data["role"].clone())
        .collect();
    assert_eq!(
        roles[..4],
        [
            json!("build"),
            json!("answer_check"),
            json!("rebase"),
            json!("build"),
        ],
        "{roles:?}"
    );
    assert_eq!(
        builds(&bench),
        [false, false],
        "a Build run, not its gate alone"
    );
    let configured = data_of(&bench.events(), EventKind::Dispatch, "configured_at");
    assert_eq!(configured.last().unwrap()["configured_at"], json!(fixed));
    assert!(plan.exists(), "the Build's plan was kept");
    assert!(is_ancestor(&bench, &fixed, &bench.remote_branch().unwrap()));
}

/// OWL-427's limit, decided on 2026-10-23: a held ticket whose pull request
/// is open is not rebased, its notice saying a person rebases it.
#[test]
fn a_held_ticket_with_an_open_pull_request_is_not_rebased() {
    use owlshift_contracts::format::Format;
    use owlshift_contracts::refs::{DeliveredPullRequest, TicketDelivery};

    let bench = Bench::new(true);
    let marker = bench.tmp.path().join("store-has-rollup");
    let (held, start) = held_done_build(&bench, &marker);
    let mut record = bench.record();
    record.delivery = Some(TicketDelivery {
        format: Format,
        pull_request: Some(DeliveredPullRequest {
            number: 1,
            url: "https://github.com/demo/project/pull/1".to_owned(),
        }),
        branch: BRANCH.to_owned(),
        base_branch: "main".to_owned(),
        delivered_at: None,
        pushed: Some(start.clone()),
        base_at_push: Some(held.clone()),
        rebased_onto: None,
        pushing: None,
        owed: None,
        ended: None,
    });
    bench.write_record(&record);
    bench.merge_into_main(&[("FIXED", "fixed\n")]);

    let (still, printed) = bench.continue_ticket(Vec::new());
    let text = match &still {
        Err(stop @ Stop::BaseHeld { commit, .. }) if *commit == held => stop.to_string(),
        other => panic!("{other:?}\n{printed}"),
    };
    assert!(text.contains("unless its pull request is open"), "{text}");
    assert!(held_rebases(&bench).is_empty());
    assert_eq!(worktree_head(&bench), start);
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

/// A watch started again while a check waits in `base.json` gives it its
/// priority back: the retry hold its predecessor's step left, adopted from
/// that step's record, is lifted, and the check runs in the first pass with
/// no clock moving.
#[test]
fn a_watch_started_again_gives_a_waiting_check_its_priority() {
    use owlshift_contracts::format::Format;
    use owlshift_contracts::step::{KeptFollowing, RefAfter, StepEnd, StepHold, StepOutcome};
    use owlshift_runner::step;
    use owlshift_runner::ticket_ref;

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
    drop(sibling);
    // What the step of the watch before left: held for the retry delay.
    let env = bench.env.clone();
    let git = Git::with_setup("git", move |command| env.apply(command));
    let stored = ticket_ref::read(&git, &bench.dirs().checkout(), &ticket())
        .unwrap()
        .unwrap();
    let now = bench.clock.get();
    let record = StepOutcome {
        format: Format,
        ticket: ticket(),
        attempt: "1-1-1".to_owned(),
        commit: Some(stored.commit.clone()),
        progress: stored.progress().as_str().to_owned(),
        answer: None,
        end: StepEnd::BaseHeld {
            commit: base,
            command: Some(setup_line(&marker)),
            pending: true,
        },
        after: RefAfter::Present {
            commit: stored.commit.clone(),
            progress: stored.progress().as_str().to_owned(),
        },
        hold: Some(StepHold::Held {
            // Past the bench's commands' clock, which reads ten minutes on.
            until: Some(now + SignedDuration::from_mins(60)),
        }),
        kept: KeptFollowing::default(),
        at: now,
    };
    step::write_outcome(&bench.dirs(), &record).unwrap();

    let (outcomes, lines, printed) = bench.watch(Vec::new(), &mut |_| false);
    assert!(
        matches!(
            outcomes.as_slice(),
            [Err(Stop::BaseHeld { pending: false, .. })]
        ),
        "{outcomes:?}\n{lines}\n{printed}"
    );
    assert_eq!(checked(&bench), ["opened"]);
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

/// OWL-431, the head's check that must wait: a done Build held at its base,
/// whose commit was checked again at the fix's head and fails still, finds
/// that head's own check due while DEMO-2's step runs. It stays held, that
/// check due (watch's priority), its text the hold's, no check made and its
/// branch where it was; once DEMO-2's step ends, `continue` checks the head
/// alone, rebases onto it and delivers.
#[test]
fn a_head_check_that_must_wait_is_due_then_runs_alone() {
    use owlshift_core::base::Entry;

    let bench = Bench::new(true);
    let marker = bench.tmp.path().join("store-has-rollup");
    let (held, start) = held_done_build(&bench, &marker);
    let fixed = bench.merge_into_main(&[("FIXED", "fixed\n")]);
    // The held commit checked again at the fix's head, failing still.
    let mut checks = base::read(&bench.dirs()).unwrap();
    for check in &mut checks.checks {
        if check.commit == held
            && let Entry::Checked { head, .. } = &mut check.entry
        {
            head.clone_from(&fixed);
        }
    }
    base::write(&bench.dirs(), &checks).unwrap();
    let two = TicketId::new("DEMO-2").unwrap();
    let sibling = bench.dirs().ticket_lock(&two).unwrap().unwrap();

    let (due, printed) = bench.continue_ticket(Vec::new());
    assert!(
        matches!(&due, Err(Stop::BaseHeld { commit, pending: false, due: true, .. }) if *commit == held),
        "{due:?}\n{printed}"
    );
    let text = due.unwrap_err().to_string();
    assert!(
        text.contains(&format!("owlshift gate --at {held}")),
        "told as held: {text}"
    );
    assert_eq!(
        checks_made(&bench).len(),
        1,
        "no check beside DEMO-2's step"
    );
    assert!(held_rebases(&bench).is_empty());
    assert_eq!(worktree_head(&bench), start);
    drop(sibling);

    let rebase = agent_git(
        &bench,
        1,
        vec!["rebase".to_owned(), "--quiet".to_owned(), fixed.clone()],
    );
    let (delivered, printed) =
        bench.continue_during(vec![bench.reply(None, Some(REBASED_HELD))], vec![rebase]);
    delivered.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    let made = |commit: &str, change: &str, verdict: &str| {
        (commit.to_owned(), change.to_owned(), verdict.to_owned())
    };
    assert_eq!(
        checks_made(&bench),
        [
            made(&held, "opened", "failed"),
            made(&fixed, "cached", "passed")
        ]
    );
    assert_eq!(held_rebases(&bench).len(), 1);
    assert!(is_ancestor(&bench, &fixed, &bench.remote_branch().unwrap()));
}

/// Each step's ticket and outcome, as watch reported them.
type Outcomes = Vec<(String, Result<Delivered, Stop>)>;

/// Runs `owlshift watch --drain` in watch's own process, with what happens
/// while given runs work, `between(pass)` called after every pass until it
/// returns `false`; returns each step's ticket and outcome, and what was
/// printed.
fn drain_until(
    bench: &Bench,
    replies: Vec<Reply>,
    during: Vec<During>,
    between: &mut dyn FnMut(usize) -> bool,
) -> (Outcomes, String) {
    let mut passes = 0;
    let ((outcomes, lines), printed) =
        bench.with_on_demand(replies, None, during, |on_demand, sink| {
            let (mut outcomes, mut lines) = (Vec::new(), Vec::new());
            let watch = Watch {
                on_demand,
                interval: POLL_INTERVAL,
            };
            watch.run_drain(
                sink,
                &mut lines,
                &mut |ticket, outcome, _| {
                    outcomes.push((ticket.as_str().to_owned(), outcome.clone()));
                },
                &mut |_| {
                    passes += 1;
                    assert!(passes < 40, "the drain never settled");
                    between(passes)
                },
            );
            (outcomes, String::from_utf8(lines).unwrap())
        });
    (outcomes, format!("{lines}\n{printed}"))
}

/// The drain's dispatches among `events`, each with its position in them
/// and its ticket.
fn drained(events: &[Event]) -> Vec<(usize, String)> {
    events
        .iter()
        .enumerate()
        .filter(|(_, e)| e.kind == EventKind::Dispatch && e.data["command"] == json!("drain"))
        .map(|(at, e)| (at, e.ticket.as_ref().unwrap().as_str().to_owned()))
        .collect()
}

/// The ticket's `decision` events `base` = `waits` among `events`.
fn base_waits(events: &[Event], id: &str) -> usize {
    events
        .iter()
        .filter(|e| {
            e.kind == EventKind::Decision
                && e.ticket.as_ref().is_some_and(|t| t.as_str() == id)
                && e.data.get("base") == Some(&json!("waits"))
        })
        .count()
}

/// OWL-431's bound in a busy drain, under `policy.max_in_flight = 3`:
/// DEMO-1, a done Build held at its base, is held again at each step while
/// DEMO-2 and DEMO-3 are taken in, blocked. The store is fixed and `main`
/// moves; the blocker is done. The drain dispatches DEMO-2, which gives
/// DEMO-1 a turn before any other dispatch, with no clock moved by the test.
/// Its check again finds another ticket's step in flight (a lock held for
/// that pass): it is due, and its priority holds DEMO-3's dispatch back.
/// Once that step ends, DEMO-1's check runs alone and lifts the hold before
/// the second dispatch after the fix.
#[test]
fn a_held_ticket_goes_on_within_one_dispatch_of_a_fix_in_a_busy_drain() {
    use super::drain_scenarios::{done_for, intake_declaring};

    let bench = Bench::build_admitting(
        "maintainer",
        "assignee = \"maintainer\"\nlabels = [\"agent\"]\n",
        "max_in_flight = 3\n",
        ", parked = \"Parked\"",
        "admit = { label = \"agent\" }\nadmitters = [\"maintainer\"]",
    );
    let marker = bench.tmp.path().join("store-has-rollup");
    fs::write(&marker, "").unwrap();
    let held = with_setup(&bench, &setup_line(&marker), None);
    let lost = marker.clone();
    let lose: During = (2, Box::new(move || fs::remove_file(&lost).unwrap()));
    let holds = |bench: &Bench| base::read(&bench.dirs()).is_ok_and(|c| c.holds_commit(&held));
    let (_, printed) = drain_until(
        &bench,
        vec![
            bench.reply(None, Some(&intake_declaring(&["app"], &[]))),
            bench.reply(Some("Hello"), Some(DONE)),
        ],
        vec![lose],
        &mut |_| !holds(&bench),
    );
    assert!(holds(&bench), "{printed}");

    bench.put_ticket("DEMO-9", "maintainer", "Todo", &[], "", "A blocker.");
    for (id, text) in [("DEMO-2", "Document it."), ("DEMO-3", "Library it.")] {
        let blocked = "blocked_by = [\"DEMO-9\"]\n";
        bench.put_ticket(id, "maintainer", "Todo", &["agent"], blocked, text);
    }
    let start = bench.events().len();
    let (mut fix_at, mut lock_at) = (None, None);
    let mut lock = None;
    let eight = TicketId::new("DEMO-8").unwrap();
    let (outcomes, printed) = drain_until(
        &bench,
        vec![
            bench.reply(None, Some(&intake_declaring(&["docs"], &[]))),
            bench.reply(None, Some(&intake_declaring(&["lib"], &[]))),
            bench.reply(Some("Hello again"), Some(&done_for("DEMO-2"))),
            bench.reply(Some("Hello at last"), Some(&done_for("DEMO-3"))),
        ],
        Vec::new(),
        &mut |_| {
            let events = bench.events();
            let taken_in = bench
                .record_of("DEMO-3")
                .is_some_and(|record| record.intake.is_some());
            match (fix_at, lock_at) {
                // DEMO-1 held again in this watch, both others taken in.
                (None, _) if taken_in && base_waits(&events[start..], TICKET) > 0 => {
                    fs::write(&marker, "").unwrap();
                    bench.merge_into_main(&[("NEWS.md", "news\n")]);
                    bench.put_ticket("DEMO-9", "maintainer", "Done", &[], "", "A blocker.");
                    fix_at = Some(events.len());
                }
                // The first dispatch after the fix: another step is in
                // flight for the next pass.
                (Some(fix), None) if !drained(&events[fix..]).is_empty() => {
                    lock = Some(bench.dirs().ticket_lock(&eight).unwrap().unwrap());
                    lock_at = Some(events.len());
                }
                // DEMO-1's check found that step in flight: it ends.
                (Some(_), Some(at)) if lock.is_some() && base_waits(&events[at..], TICKET) > 0 => {
                    lock = None;
                }
                _ => {}
            }
            fix_at.is_none_or(|fix| drained(&events[fix..]).len() < 2)
        },
    );
    let events = bench.events();
    let fix = fix_at.expect("the fix landed");
    let lifted = events
        .iter()
        .position(|e| {
            e.kind == EventKind::Decision
                && e.data.get("base") == Some(&json!("checked"))
                && e.data["change"] == json!("lifted")
                && e.data["commit"] == json!(held)
        })
        .unwrap_or_else(|| panic!("never lifted\n{printed}"));
    let dispatches: Vec<(usize, String)> = drained(&events)
        .into_iter()
        .filter(|(at, _)| *at >= fix)
        .collect();
    assert!(
        dispatches.len() >= 2 && dispatches[0].0 < lifted && lifted < dispatches[1].0,
        "{dispatches:?}, lifted at {lifted}\n{printed}"
    );
    assert_eq!(dispatches[0].1, "DEMO-2");
    let due = outcomes
        .iter()
        .filter(|(id, outcome)| {
            id == TICKET && matches!(outcome, Err(Stop::BaseHeld { due: true, .. }))
        })
        .count();
    assert_eq!(due, 1, "its check found a step in flight once\n{printed}");
    assert!(lock.is_none());
}
