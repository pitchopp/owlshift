//! Several tickets of a project at once (OWL-401; architecture section 5,
//! "Parallel", "Three counts", "Admission, one transaction"), played on the
//! bench of `on_demand.rs` in watch's own process: scenario S5 with three
//! tickets in flight under `policy.max_in_flight`, and the admission each
//! dispatch, `do` and person's `continue` makes, in both directions of a
//! race between a dispatch and a `do`.

use owlshift_contracts::format::Format;
use owlshift_contracts::step::{KeptFollowing, StepEnvelope, StepKind};
use owlshift_runner::follow::Following;
use owlshift_runner::step;

use super::drain_scenarios::{by_pass, collisions, dispatched, intake_declaring, waits};
use super::*;

/// The admitting bench, its project file's `[policy]` given `policy`, and
/// DEMO-1 blocked by DEMO-9, a ticket of the tracker no gesture admits.
pub(super) fn parallel_bench(policy: &str) -> Bench {
    let bench = Bench::build_admitting(
        "maintainer",
        "assignee = \"maintainer\"\nlabels = [\"agent\"]\nblocked_by = [\"DEMO-9\"]\n",
        policy,
        ", parked = \"Parked\"",
        "admit = { label = \"agent\" }\nadmitters = [\"maintainer\"]",
    );
    bench.put_ticket("DEMO-9", "maintainer", "Todo", &[], "", "A blocker.");
    bench
}

/// An intake declaring `zones`, the ticket `standard`: its dispatch runs
/// Design first, one stage step.
pub(super) fn standard_declaring(zones: &[&str]) -> String {
    let mut result: Value = serde_json::from_str(&intake_declaring(zones, &[])).unwrap();
    result["intake"]["variant"] = json!("standard");
    result.to_string()
}

/// A Design run's plan: the step stops at Design review.
pub(super) fn planned() -> String {
    super::design_scenarios::designed(super::design_scenarios::PLAN)
}

/// Each ticket's ref state, as its record keeps it.
fn status_of(bench: &Bench, id: &str) -> Option<Status> {
    let record = bench.record_of(id)?;
    Some(TicketState::try_from(&record.state).unwrap().status())
}

/// The ticket of each `dispatch` event of `events` that `command` made.
fn by_command(events: &[Event], command: &str) -> Vec<String> {
    events
        .iter()
        .filter(|e| e.kind == EventKind::Dispatch && e.data["command"] == json!(command))
        .map(|e| e.ticket.as_ref().unwrap().as_str().to_owned())
        .collect()
}

/// Scenario S5 with three tickets in flight (OWL-401, OWL-448): with
/// `policy.max_in_flight = 3`, five admitted tickets declare their zones at
/// intake, one a pass, while a blocker holds them all; once it is done, one
/// pass dispatches DEMO-1, DEMO-2 and DEMO-3, each running its Design and
/// holding the slot at Design review: DEMO-3's zone lies in DEMO-1's, but a
/// ready ticket never waits for a collision, and a ticket in Design holds
/// nothing. DEMO-4 and DEMO-5 wait for the project's capacity, which the
/// three hold though nothing is admitted, each told once. Three refs hold
/// the slot at once, each with its dispatch time and nothing admitted, and
/// nothing is quarantined.
#[test]
fn scenario_s5_three_tickets_in_flight_at_once() {
    let bench = parallel_bench("max_in_flight = 3\n");
    for (id, text) in [
        ("DEMO-2", "Document it."),
        ("DEMO-3", "Greet in the sub-app."),
        ("DEMO-4", "Library it."),
        ("DEMO-5", "Put it on the web."),
    ] {
        bench.put_ticket(
            id,
            "maintainer",
            "Todo",
            &["agent"],
            "blocked_by = [\"DEMO-9\"]\n",
            text,
        );
    }
    let mut replies: Vec<Reply> = ["app", "docs", "app/sub", "lib", "web"]
        .iter()
        .map(|zone| bench.reply(None, Some(&standard_declaring(&[zone]))))
        .collect();
    replies.extend((0..3).map(|_| bench.read(&planned())));
    let ends = RefCell::new(Vec::new());
    let (_, lines, printed) = bench.drain_passes(replies, 6, |pass, _| {
        ends.borrow_mut().push(bench.events().len());
        if pass == 5 {
            bench.put_ticket("DEMO-9", "maintainer", "Done", &[], "", "A blocker.");
        }
    });
    let report = format!("{lines}\n{printed}");
    let events = bench.events();
    let passes = by_pass(&events, 0, &ends.borrow());

    // One pass dispatches the first three, whatever their zones.
    assert_eq!(
        dispatched(&passes[5]),
        ["DEMO-1", "DEMO-2", "DEMO-3"],
        "{report}"
    );
    assert_eq!(dispatched(&events).len(), 3, "{report}");
    for id in ["DEMO-1", "DEMO-2", "DEMO-3"] {
        assert_eq!(
            status_of(&bench, id),
            Some(Status::Active(Stage::DesignReview)),
            "{id}\n{report}"
        );
        let held = bench.record_of(id).unwrap().held.unwrap();
        assert!(
            held.dispatched_at.is_some() && !held.admitted() && held.zones.is_empty(),
            "{id}: {held:?}"
        );
    }
    // No collision; DEMO-4 and DEMO-5 wait for the capacity the three hold
    // in Design, nothing admitted.
    assert!(collisions(&events, "drain").is_empty(), "{report}");
    assert!(collisions(&events, "watch").is_empty(), "{report}");
    let capacity: Vec<_> = waits(&events)
        .into_iter()
        .filter(|(_, reason, _)| *reason == json!("slot"))
        .collect();
    let three = json!(["DEMO-1", "DEMO-2", "DEMO-3"]);
    assert_eq!(
        capacity,
        [
            ("DEMO-4".to_owned(), json!("slot"), three.clone()),
            ("DEMO-5".to_owned(), json!("slot"), three)
        ],
        "{report}"
    );
    for id in ["DEMO-4", "DEMO-5"] {
        assert_eq!(status_of(&bench, id), Some(Status::Active(Stage::Ready)));
    }
    assert_eq!(bench.dirs().quarantine().unwrap(), None);
    assert!(!bench.dirs().obligations_stand());
}

/// OWL-161, the drain's guard within a pass: three tickets whose zones are
/// apart are due in one pass, steps in watch's own process. Once DEMO-1's
/// dispatch has run, the stop file is written: the two other dispatches
/// never start, and watch ends on its own, saying so, its stop recorded and
/// the file removed, nothing quarantined.
#[test]
fn a_drain_asked_mid_pass_starts_no_other_step() {
    let bench = parallel_bench("max_in_flight = 3\n");
    for (id, text) in [("DEMO-2", "Document it."), ("DEMO-4", "Library it.")] {
        bench.put_ticket(
            id,
            "maintainer",
            "Todo",
            &["agent"],
            "blocked_by = [\"DEMO-9\"]\n",
            text,
        );
    }
    let mut replies: Vec<Reply> = ["app", "docs", "lib"]
        .iter()
        .map(|zone| bench.reply(None, Some(&standard_declaring(&[zone]))))
        .collect();
    replies.push(bench.read(&planned()));
    let stop = owlshift_runner::watch::stop::file(&bench.dirs());
    let (passes, lines) = bench
        .with_on_demand(replies, None, Vec::new(), |on_demand, sink| {
            let watch = Watch {
                on_demand,
                interval: POLL_INTERVAL,
            };
            let (mut lines, mut pass) = (Vec::new(), 0);
            watch.run_drain(
                sink,
                &mut lines,
                &mut |_, outcome, _| {
                    if matches!(outcome, Err(Stop::Stepped { .. })) && !stop.exists() {
                        fs::write(&stop, "").unwrap();
                    }
                },
                &mut |_| {
                    pass += 1;
                    let blocker_done = ["DEMO-1", "DEMO-2", "DEMO-4"]
                        .iter()
                        .all(|id| bench.record_of(id).is_some_and(|r| r.intake.is_some()));
                    if blocker_done {
                        bench.put_ticket("DEMO-9", "maintainer", "Done", &[], "", "A blocker.");
                    }
                    pass < 20
                },
            );
            (pass, String::from_utf8(lines).unwrap())
        })
        .0;
    let events = bench.events();
    assert!(passes < 20, "watch did not end on its own\n{lines}");
    assert_eq!(dispatched(&events), ["DEMO-1"], "{lines}");
    for id in ["DEMO-2", "DEMO-4"] {
        assert_eq!(
            status_of(&bench, id),
            Some(Status::Active(Stage::Ready)),
            "{id}"
        );
    }
    let stops: Vec<_> = events
        .iter()
        .filter(|e| e.kind == EventKind::Decision && e.data.get("watch") == Some(&json!("stop")))
        .map(|e| (e.data["stop"].clone(), e.data["asked"].clone()))
        .collect();
    assert_eq!(stops, [(json!("drain"), json!("stop_file"))], "{lines}");
    assert!(lines.contains("ended: drained"), "{lines}");
    assert!(!stop.exists());
    assert_eq!(bench.dirs().quarantine().unwrap(), None);
    assert!(!bench.dirs().obligations_stand());
}

/// The admission's first direction (OWL-401's acceptance, OWL-448): a `do`
/// typed while watch runs, between two of its passes, cannot take what an
/// admitted Build holds. DEMO-1, with no Design, is dispatched and asks at
/// Build, holding `app` while it waits; `owlshift do DEMO-2`, which would
/// hold `app/sub`, is dispatched, its capacity free, and its Build waits as
/// `owlshift why` words it, running nothing and admitting nothing; watch
/// then tells DEMO-2 its Build waits for DEMO-1.
#[test]
fn a_do_typed_during_a_pass_cannot_take_what_a_build_holds() {
    let bench = Bench::draining("maintainer");
    bench.put_ticket(
        "DEMO-2",
        "maintainer",
        "Todo",
        &["agent"],
        "",
        "Greet in the sub-app.",
    );
    let replies = vec![
        bench.reply(None, Some(&intake_declaring(&["app"], &[]))),
        bench.reply(None, Some(&intake_declaring(&["app/sub"], &[]))),
        bench.reply(None, Some(ROUND_1)),
    ];
    let demo2 = TicketId::new("DEMO-2").unwrap();
    let ((typed, lines), printed) =
        bench.with_on_demand(replies, None, Vec::new(), |on_demand, sink| {
            let watch = Watch {
                on_demand,
                interval: POLL_INTERVAL,
            };
            let (mut lines, mut typed, mut pass) = (Vec::new(), None, 0);
            watch.run_drain(sink, &mut lines, &mut |_, _, _| {}, &mut |_| {
                pass += 1;
                let took = bench
                    .events()
                    .iter()
                    .any(|e| e.kind == EventKind::Dispatch && e.data["command"] == "drain");
                if took && typed.is_none() {
                    let mut out = Vec::new();
                    let mut typing = EventSink::new(REPO, EventLog::in_dir(&bench.data), &mut out);
                    typed = Some(on_demand.run(&demo2, &mut typing));
                }
                pass < 4
            });
            (typed, String::from_utf8(lines).unwrap())
        });
    let report = format!("{lines}\n{printed}");
    match typed {
        Some(Err(Stop::Waits { ticket, said, cap })) => {
            assert_eq!((ticket, cap), (demo2, None));
            assert!(
                said.starts_with("DEMO-1 holds zone `app`, which its Build would take"),
                "{said}"
            );
        }
        other => panic!("{other:?}\n{report}"),
    }
    let events = bench.events();
    assert_eq!(by_command(&events, "drain"), ["DEMO-1"], "{report}");
    assert_eq!(by_command(&events, "do"), ["DEMO-2"], "{report}");
    assert_eq!(
        status_of(&bench, "DEMO-2"),
        Some(Status::Active(Stage::Build))
    );
    let held = bench.record_of("DEMO-2").unwrap().held.unwrap();
    assert!(!held.admitted(), "{held:?}");
    assert!(collisions(&events, "drain").is_empty(), "{report}");
    assert!(
        collisions(&events, "watch").contains(&(
            "DEMO-2".to_owned(),
            json!(["DEMO-1"]),
            json!({ "zone": "app" })
        )),
        "{report}"
    );
}

/// The commit of the ticket ref of `id`, as watch's pass reads it.
fn read_ref(bench: &Bench, id: &str) -> String {
    let env = bench.env.clone();
    let git = Git::with_setup("git", move |command| env.apply(command));
    let stored = ticket_ref::read(&git, &bench.dirs().checkout(), &TicketId::new(id).unwrap())
        .unwrap()
        .expect("a ticket ref");
    stored.commit
}

/// The admission's other direction (OWL-401, OWL-448): a dispatch the
/// drain's pass decided from refs read before a person's `do` took what it
/// would touch builds nothing. With DEMO-1 blocked while both intakes run,
/// the pass's view of DEMO-1 is kept; `owlshift do DEMO-2` then takes
/// `app/sub` and asks at Build; DEMO-1, its blocker done, is dispatched
/// from that view, the capacity free, straight to Build, where its
/// admission reads the refs again and finds DEMO-2: no Build runs, nothing
/// is admitted, the step's wait is recorded, and the next pass tells DEMO-1
/// its Build waits for DEMO-2.
#[test]
fn a_dispatch_decided_before_a_do_builds_nothing_once_the_do_took_its_zone() {
    let bench = parallel_bench("");
    bench.put_ticket(
        "DEMO-2",
        "maintainer",
        "Todo",
        &["agent"],
        "",
        "Greet in the sub-app.",
    );
    // Both intakes, DEMO-1 held by its blocker.
    let (_, lines, _) = bench.drain_passes(
        vec![
            bench.reply(None, Some(&intake_declaring(&["app"], &[]))),
            bench.reply(None, Some(&intake_declaring(&["app/sub"], &[]))),
        ],
        2,
        |_, _| {},
    );
    let commit = read_ref(&bench, "DEMO-1");
    let demo1 = TicketId::new("DEMO-1").unwrap();
    let demo2 = TicketId::new("DEMO-2").unwrap();
    let (stepped, printed) = bench.with_on_demand(
        vec![bench.reply(None, Some(ROUND_1))],
        None,
        Vec::new(),
        |on_demand, sink| {
            // The person's `do`, once the pass read DEMO-1.
            let asked = on_demand.run(&demo2, sink);
            assert!(matches!(asked, Err(Stop::NeedsInput { .. })), "{asked:?}");
            bench.put_ticket("DEMO-9", "maintainer", "Done", &[], "", "A blocker.");
            // The dispatch the pass decided, carried out after it.
            let envelope = StepEnvelope {
                format: Format,
                ticket: demo1.clone(),
                attempt: "owl-401-1".to_owned(),
                step: StepKind::Dispatch,
                commit: Some(commit.clone()),
                progress: String::new(),
                answer: None,
                generation: 0,
                kept: KeptFollowing::default(),
            };
            step::run(
                on_demand,
                &envelope,
                &mut Following::default(),
                None,
                None,
                sink,
            )
        },
    );
    assert_eq!(stepped, step::Stepped::Stale, "{lines}\n{printed}");
    assert_ne!(read_ref(&bench, "DEMO-1"), commit);
    assert_eq!(
        status_of(&bench, "DEMO-1"),
        Some(Status::Active(Stage::Build))
    );
    let held = bench.record_of("DEMO-1").unwrap().held.unwrap();
    assert!(held.dispatched_at.is_some() && !held.admitted(), "{held:?}");
    let events = bench.events();
    assert_eq!(by_command(&events, "drain"), ["DEMO-1"], "{printed}");
    assert!(
        !events.iter().any(|e| e.kind == EventKind::RunStarted
            && e.ticket.as_ref() == Some(&demo1)
            && e.data["role"] == json!("build")),
        "{printed}"
    );
    let waiting = (
        "DEMO-1".to_owned(),
        json!(["DEMO-2"]),
        json!({ "zone": "app/sub" }),
    );
    assert!(collisions(&events, "watch").contains(&waiting), "{printed}");
    // The next pass reads the refs again and tells why DEMO-1 waits.
    let (_, lines, printed) = bench.drain_passes(Vec::new(), 1, |_, _| {});
    assert!(
        lines.contains("DEMO-1's Build waits: DEMO-2 holds zone `app/sub`"),
        "{lines}\n{printed}"
    );
}

/// OWL-401: a person's `continue` and `do` make the drain's check. With a
/// capacity of one, DEMO-1 and DEMO-2 both ask at Build; watch takes
/// DEMO-1's answer to Build, where its ref holds the slot. A person's
/// `continue DEMO-2` then checks its answers and posts its RESUME, but its
/// Build waits: the capacity is DEMO-1's, dispatched first. A `do` waits
/// for that capacity too, writing nothing, and a person's `continue` while
/// the work in flight outside Owlshift cannot be read is refused,
/// fail-closed.
#[test]
fn a_persons_continue_and_do_make_the_drains_check() {
    let bench = Bench::draining("maintainer");
    bench.put_ticket(
        "DEMO-2",
        "maintainer",
        "Todo",
        &["agent"],
        "",
        "Document it.",
    );
    let answered = || {
        bench.read(&check(&[
            ("Q1", "answered", "English."),
            ("Q2", "answered", "Hello."),
        ]))
    };
    let replies = vec![
        bench.reply(None, Some(&intake_declaring(&["app"], &[]))),
        bench.reply(None, Some(&intake_declaring(&["docs"], &[]))),
        bench.reply(None, Some(ROUND_1)),
        bench.reply(None, Some(ROUND_1)),
        answered(),
    ];
    let answering = Cell::new(false);
    let (_, lines, printed) = bench.drain_passes(replies, 4, |_, _| {
        let asked = |id| matches!(status_of(&bench, id), Some(Status::NeedsInput { .. }));
        if asked("DEMO-1") && asked("DEMO-2") && !answering.get() {
            bench.answer("Q1: English. Q2: Hello. Go.");
            answering.set(true);
        }
    });
    let report = format!("{lines}\n{printed}");
    assert_eq!(
        status_of(&bench, "DEMO-1"),
        Some(Status::Active(Stage::Build)),
        "{report}"
    );
    // The decider answers DEMO-2 too; a person continues it.
    let demo2 = TicketId::new("DEMO-2").unwrap();
    MarkdownTracker::new(&bench.remote.checkout)
        .post_comment(
            &demo2,
            "maintainer",
            tick(&bench.clock),
            "Q1: English. Q2: Hello. Go.",
        )
        .unwrap();
    let (continued, printed) =
        bench.with_on_demand(vec![answered()], None, Vec::new(), |on_demand, sink| {
            on_demand.continue_ticket(&demo2, sink)
        });
    match continued {
        Err(Stop::Waits { said, .. }) => {
            assert!(
                said.contains("the project's slot is held by DEMO-1"),
                "{said}"
            )
        }
        other => panic!("{other:?}\n{printed}"),
    }
    assert_eq!(
        status_of(&bench, "DEMO-2"),
        Some(Status::Active(Stage::Build))
    );
    assert!(
        bench
            .comments_on("DEMO-2")
            .iter()
            .any(|c| c.contains("RESUME"))
    );

    // A `do` waits for the capacity, which a dispatch checks alone.
    bench.put_ticket("DEMO-3", "maintainer", "Todo", &["agent"], "", "Other.");
    let demo3 = TicketId::new("DEMO-3").unwrap();
    let (refused, printed) =
        bench.with_on_demand(Vec::new(), None, Vec::new(), |on_demand, sink| {
            on_demand.run(&demo3, sink)
        });
    match refused {
        Err(Stop::Waits { said, .. }) => assert!(
            said.starts_with("the project's slot is held by DEMO-1"),
            "{said}"
        ),
        other => panic!("{other:?}\n{printed}"),
    }
    assert_eq!(bench.record_of("DEMO-3"), None);
    // Fail-closed: the open pull requests cannot be read.
    *bench.github.fail_pulls.lock().unwrap() = 1;
    let demo1 = TicketId::new("DEMO-1").unwrap();
    let before = read_ref(&bench, "DEMO-1");
    let (refused, printed) =
        bench.with_on_demand(Vec::new(), None, Vec::new(), |on_demand, sink| {
            on_demand.continue_ticket(&demo1, sink)
        });
    match refused {
        Err(Stop::Waits { said, .. }) => assert!(
            said.starts_with("the work in flight outside Owlshift could not be read"),
            "{said}"
        ),
        other => panic!("{other:?}\n{printed}"),
    }
    assert_eq!(read_ref(&bench, "DEMO-1"), before);
}

/// A Design run's plan naming `files`.
fn planning(files: &[&str]) -> String {
    let mut plan: Value = serde_json::from_str(super::design_scenarios::PLAN).unwrap();
    plan["files"] = json!(files);
    super::design_scenarios::designed(&plan.to_string())
}

/// A Build run of `id` that greets, as the gate asks, and writes `file`.
pub(super) fn building(bench: &Bench, id: &str, file: &str) -> Reply {
    let mut reply = bench.reply(Some("Hello"), Some(&super::drain_scenarios::done_for(id)));
    reply
        .files
        .insert(RelativePath::new(file).unwrap(), "Built.\n".to_owned());
    reply
}

/// Two standard tickets, DEMO-1 declaring `app` and DEMO-2 `app/sub` at
/// intake, held by a blocker until both intakes ran, under a capacity of
/// two, DEMO-2 first when `urgent`; each Design plans `plans`' files; then
/// the drain's passes, `builds` the Build replies of the pass whose steps
/// reach Build. Returns watch's lines and the events printed.
pub(super) fn two_planned(bench: &Bench, plans: [&[&str]; 2], builds: Vec<Reply>) -> String {
    let reviewed = || bench.read(&super::design_scenarios::reviewed(&[]));
    let mut replies = vec![
        bench.reply(None, Some(&standard_declaring(&["app"]))),
        bench.reply(None, Some(&standard_declaring(&["app/sub"]))),
    ];
    replies.extend(plans.iter().map(|files| bench.read(&planning(files))));
    replies.extend((0..4).map(|_| reviewed()));
    replies.extend(builds);
    let (_, lines, printed) = bench.drain_passes(replies, 5, |pass, _| {
        if pass == 2 {
            bench.put_ticket("DEMO-9", "maintainer", "Done", &[], "", "A blocker.");
        }
    });
    format!("{lines}\n{printed}")
}

/// Scenario S5, resources admitted at Build (OWL-448): DEMO-1 and DEMO-2
/// declared intake zones that overlap, `app` and `app/sub`, but their plans
/// share no file. Both are dispatched in one pass, both Designs and reviews
/// run, and both Builds are admitted and run in the same pass, each holding
/// its plan's file from its admission's time, with no collision.
#[test]
fn scenario_s5_builds_whose_plans_share_no_file_run_together() {
    let bench = parallel_bench("max_in_flight = 2\n");
    bench.put_ticket(
        "DEMO-2",
        "maintainer",
        "Todo",
        &["agent"],
        "blocked_by = [\"DEMO-9\"]\n",
        "Greet in the sub-app.",
    );
    let builds = vec![
        building(&bench, "DEMO-1", "app/one.rs"),
        building(&bench, "DEMO-2", "app/sub/two.rs"),
    ];
    let report = two_planned(&bench, [&["app/one.rs"], &["app/sub/two.rs"]], builds);
    let events = bench.events();
    let built: Vec<&str> = events
        .iter()
        .filter(|e| e.kind == EventKind::RunStarted && e.data["role"] == json!("build"))
        .map(|e| e.ticket.as_ref().unwrap().as_str())
        .collect();
    assert_eq!(built, ["DEMO-1", "DEMO-2"], "{report}");
    assert!(collisions(&events, "watch").is_empty(), "{report}");
    for (id, file) in [("DEMO-1", "app/one.rs"), ("DEMO-2", "app/sub/two.rs")] {
        let held = bench.record_of(id).unwrap().held.unwrap();
        assert_eq!(held.zones, [file], "{id}\n{report}");
        assert!(
            held.admitted_at > held.dispatched_at && held.plan_sha256.is_some(),
            "{id}: {held:?}"
        );
    }
}

/// Scenario S5, resources admitted at Build (OWL-448): the plans of DEMO-1
/// and DEMO-2 share `app/one.rs`. Dispatched in the same pass, DEMO-1 comes
/// first (its identifier breaks the tie of their dispatch times), so its
/// Build is admitted first, and DEMO-2's Build, due in the same pass, waits
/// at Build for it: told once by watch, by `owlshift why`'s phrase and by
/// its WAITING comment, its capacity kept, nothing admitted. A person's
/// `continue DEMO-2` is told the same.
#[test]
fn scenario_s5_a_build_whose_plan_shares_a_file_waits_for_the_one_dispatched_first() {
    let bench = parallel_bench("max_in_flight = 2\n");
    bench.put_ticket(
        "DEMO-2",
        "maintainer",
        "Todo",
        &["agent"],
        "blocked_by = [\"DEMO-9\"]\n",
        "Greet in the sub-app.",
    );
    let builds = vec![building(&bench, "DEMO-1", "app/one.rs")];
    let report = two_planned(&bench, [&["app/one.rs"], &["app/one.rs"]], builds);
    let events = bench.events();
    assert_eq!(
        by_command(&events, "drain"),
        ["DEMO-1", "DEMO-2"],
        "{report}"
    );
    let shared = json!({ "zone": "app/one.rs" });
    let told: Vec<_> = events
        .iter()
        .filter(|e| e.data.get("watch") == Some(&json!("waits")))
        .map(|e| {
            (
                e.ticket.as_ref().unwrap().as_str(),
                e.data["stage"].clone(),
                e.data["tickets"].clone(),
                e.data["resource"].clone(),
            )
        })
        .collect();
    assert_eq!(
        told,
        [("DEMO-2", json!("build"), json!(["DEMO-1"]), shared)],
        "{report}"
    );
    assert!(
        report.contains("DEMO-2's Build waits: DEMO-1 holds zone `app/one.rs`"),
        "{report}"
    );
    assert_eq!(
        status_of(&bench, "DEMO-2"),
        Some(Status::Active(Stage::Build))
    );
    assert!(!bench.record_of("DEMO-2").unwrap().held.unwrap().admitted());
    let waiting: Vec<String> = bench
        .comments_on("DEMO-2")
        .into_iter()
        .filter(|c| c.starts_with("[owlshift] WAITING\n"))
        .collect();
    assert_eq!(waiting.len(), 1, "{waiting:?}");
    assert!(
        waiting[0].contains(
            "its Build waits: DEMO-1 holds zone `app/one.rs`, which its Build would take"
        ),
        "{}",
        waiting[0]
    );
    let demo2 = TicketId::new("DEMO-2").unwrap();
    let (continued, printed) =
        bench.with_on_demand(Vec::new(), None, Vec::new(), |on_demand, sink| {
            on_demand.continue_ticket(&demo2, sink)
        });
    match continued {
        Err(Stop::Waits { said, .. }) => assert!(
            said.starts_with("DEMO-1 holds zone `app/one.rs`, which its Build would take"),
            "{said}"
        ),
        other => panic!("{other:?}\n{printed}"),
    }
}

/// Scenario S5 under `policy.overlap = "rebase"` (OWL-449): DEMO-1, DEMO-2
/// and DEMO-3 plan `app/one.rs`, and DEMO-1 and DEMO-4 plan a migration
/// each, which the project names as one resource (`stack.resources`). All
/// four are dispatched together; at Build, DEMO-1 and DEMO-2 run together,
/// DEMO-2's admission recording its overlap with DEMO-1 and telling it; the
/// third holder of `app/one.rs`, DEMO-3, waits for both, and DEMO-4 waits
/// for DEMO-1's migration chain, a named resource never being shared.
#[test]
fn scenario_s5_under_rebase_two_builds_share_a_file_and_a_third_waits() {
    let bench = parallel_bench(
        "max_in_flight = 4\noverlap = \"rebase\"\n[stack.resources]\nmigrations = \"db/*\"\n",
    );
    for (id, text) in [
        ("DEMO-2", "Greet twice."),
        ("DEMO-3", "Greet thrice."),
        ("DEMO-4", "Migrate."),
    ] {
        bench.put_ticket(
            id,
            "maintainer",
            "Todo",
            &["agent"],
            "blocked_by = [\"DEMO-9\"]\n",
            text,
        );
    }
    let reviewed = || bench.read(&super::design_scenarios::reviewed(&[]));
    let mut replies: Vec<Reply> = ["app", "app", "app", "db"]
        .iter()
        .map(|zone| bench.reply(None, Some(&standard_declaring(&[zone]))))
        .collect();
    let plans: [&[&str]; 4] = [
        &["app/one.rs", "db/1.sql"],
        &["app/one.rs"],
        &["app/one.rs"],
        &["db/2.sql"],
    ];
    replies.extend(plans.iter().map(|files| bench.read(&planning(files))));
    replies.extend((0..8).map(|_| reviewed()));
    replies.push(building(&bench, "DEMO-1", "app/one.rs"));
    replies.push(building(&bench, "DEMO-2", "app/one.rs"));
    let (_, lines, printed) = bench.drain_passes(replies, 7, |pass, _| {
        if pass == 4 {
            bench.put_ticket("DEMO-9", "maintainer", "Done", &[], "", "A blocker.");
        }
    });
    let report = format!("{lines}\n{printed}");
    let events = bench.events();
    let built: Vec<&str> = events
        .iter()
        .filter(|e| e.kind == EventKind::RunStarted && e.data["role"] == json!("build"))
        .map(|e| e.ticket.as_ref().unwrap().as_str())
        .collect();
    assert_eq!(built, ["DEMO-1", "DEMO-2"], "{report}");
    // DEMO-2's admission records the overlap its plan makes, and its
    // widening the file its Build changed beside DEMO-1's, each told once.
    let held = bench.record_of("DEMO-2").unwrap().held.unwrap();
    let overlaps: Vec<_> = held
        .overlaps
        .iter()
        .map(|o| (o.ticket.as_str(), o.paths.clone()))
        .collect();
    assert_eq!(
        overlaps,
        [(
            "DEMO-1",
            vec!["GREETING.md".to_owned(), "app/one.rs".to_owned()]
        )],
        "{report}"
    );
    let recorded: Vec<_> = events
        .iter()
        .filter(|e| e.data.get("overlap") == Some(&json!("recorded")))
        .map(|e| {
            (
                e.ticket.as_ref().unwrap().as_str(),
                e.data["by"].clone(),
                e.data["tickets"].clone(),
                e.data["paths"].clone(),
            )
        })
        .collect();
    assert_eq!(
        recorded,
        [
            (
                "DEMO-2",
                json!("admission"),
                json!(["DEMO-1"]),
                json!(["app/one.rs"])
            ),
            (
                "DEMO-2",
                json!("widening"),
                json!(["DEMO-1"]),
                json!(["GREETING.md"])
            ),
        ],
        "{report}"
    );
    assert!(
        bench
            .record_of("DEMO-1")
            .unwrap()
            .held
            .unwrap()
            .overlaps
            .is_empty()
    );
    // The third holder waits for both, the migration for DEMO-1's chain.
    let waiting = collisions(&events, "watch");
    assert!(
        waiting.contains(&(
            "DEMO-3".to_owned(),
            json!(["DEMO-1", "DEMO-2"]),
            json!({ "zone": "app/one.rs" })
        )),
        "{waiting:?}\n{report}"
    );
    assert!(
        waiting.contains(&(
            "DEMO-4".to_owned(),
            json!(["DEMO-1"]),
            json!({ "named": "migrations" })
        )),
        "{waiting:?}\n{report}"
    );
    assert!(
        report.contains("DEMO-3's Build waits: DEMO-1 and DEMO-2 hold zone `app/one.rs`"),
        "{report}"
    );
    for id in ["DEMO-3", "DEMO-4"] {
        assert_eq!(status_of(&bench, id), Some(Status::Active(Stage::Build)));
        assert!(
            !bench.record_of(id).unwrap().held.unwrap().admitted(),
            "{id}"
        );
    }
}
