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
fn parallel_bench(policy: &str) -> Bench {
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
fn standard_declaring(zones: &[&str]) -> String {
    let mut result: Value = serde_json::from_str(&intake_declaring(zones, &[])).unwrap();
    result["intake"]["variant"] = json!("standard");
    result.to_string()
}

/// A Design run's plan: the step stops at Design review.
fn planned() -> String {
    super::design_scenarios::designed(super::design_scenarios::PLAN)
}

/// Each ticket's ref state, as its record keeps it.
fn status_of(bench: &Bench, id: &str) -> Option<Status> {
    let record = bench.record_of(id)?;
    Some(TicketState::try_from(&record.state).unwrap().status())
}

/// Scenario S5 with three tickets in flight (OWL-401): with
/// `policy.max_in_flight = 3`, five admitted tickets declare their zones at
/// intake, one a pass, while a blocker holds them all; once it is done, one
/// pass dispatches DEMO-1, DEMO-2 and DEMO-4, whose zones are apart, each
/// running its Design and holding the slot at Design review. DEMO-3, whose
/// zone lies in DEMO-1's, waits for it, and DEMO-5 waits for the project's
/// capacity, each told once. Three refs hold the slot at once, each with
/// what it holds and its dispatch time, and nothing is quarantined.
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

    // One pass dispatches the three whose zones are apart.
    assert_eq!(
        dispatched(&passes[5]),
        ["DEMO-1", "DEMO-2", "DEMO-4"],
        "{report}"
    );
    assert_eq!(dispatched(&events).len(), 3, "{report}");
    for id in ["DEMO-1", "DEMO-2", "DEMO-4"] {
        assert_eq!(
            status_of(&bench, id),
            Some(Status::Active(Stage::DesignReview)),
            "{id}\n{report}"
        );
        let held = bench.record_of(id).unwrap().held.unwrap();
        assert!(held.dispatched_at.is_some(), "{id}: {held:?}");
    }
    // DEMO-3 waits for DEMO-1's zone, DEMO-5 for the capacity.
    assert_eq!(
        collisions(&events, "drain"),
        [(
            "DEMO-3".to_owned(),
            json!(["DEMO-1"]),
            json!({ "zone": "app" })
        )],
        "{report}"
    );
    let capacity: Vec<_> = waits(&events)
        .into_iter()
        .filter(|(_, reason, _)| *reason == json!("slot"))
        .collect();
    assert_eq!(
        capacity,
        [(
            "DEMO-5".to_owned(),
            json!("slot"),
            json!(["DEMO-1", "DEMO-2", "DEMO-4"])
        )],
        "{report}"
    );
    for id in ["DEMO-3", "DEMO-5"] {
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

/// The admission's first direction (OWL-401's acceptance): a `do` typed
/// while watch runs, between two of its passes, cannot take what a
/// dispatch took. DEMO-1 is dispatched and asks at Build, holding `app`
/// while it waits; `owlshift do DEMO-2`, which would hold `app/sub`, is
/// refused as `owlshift why` words it, running and writing nothing, and
/// the drain tells DEMO-2 it waits for DEMO-1.
#[test]
fn a_do_typed_during_a_pass_cannot_take_what_a_dispatch_took() {
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
            assert!(said.starts_with("DEMO-1 holds zone `app`"), "{said}");
        }
        other => panic!("{other:?}\n{report}"),
    }
    let events = bench.events();
    assert_eq!(dispatched(&events), ["DEMO-1"], "{report}");
    assert_eq!(
        status_of(&bench, "DEMO-2"),
        Some(Status::Active(Stage::Ready))
    );
    assert!(
        collisions(&events, "drain").contains(&(
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

/// The admission's other direction (OWL-401): a dispatch the drain's pass
/// decided from refs read before a person's `do` took what it would touch
/// runs nothing. With DEMO-1 blocked while both intakes run, the pass's
/// view of DEMO-1 is kept; `owlshift do DEMO-2` then takes `app/sub` and
/// asks at Build; DEMO-1, its blocker done, is dispatched from that view:
/// its admission reads the refs again and finds DEMO-2, so nothing runs,
/// no `dispatch` event is recorded and DEMO-1's ref stays where it was;
/// the next pass tells DEMO-1 it waits for DEMO-2.
#[test]
fn a_dispatch_decided_before_a_do_runs_nothing_once_the_do_took_its_zone() {
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
    assert_eq!(read_ref(&bench, "DEMO-1"), commit);
    let dispatches: Vec<Option<TicketId>> = bench
        .events()
        .into_iter()
        .filter(|e| e.kind == EventKind::Dispatch)
        .map(|e| e.ticket)
        .collect();
    assert_eq!(dispatches, [Some(demo2)], "{printed}");
    // The next pass reads the refs again and tells why DEMO-1 waits.
    let (_, lines, printed) = bench.drain_passes(Vec::new(), 1, |_, _| {});
    assert!(
        collisions(&bench.events(), "drain").contains(&(
            "DEMO-1".to_owned(),
            json!(["DEMO-2"]),
            json!({ "zone": "app/sub" })
        )),
        "{lines}\n{printed}"
    );
}

/// OWL-401: a person's `continue` and `do` make the drain's check. With a
/// capacity of one, DEMO-1 and DEMO-2 both ask at Build; watch takes
/// DEMO-1's answer to Build, where its ref holds the slot. A person's
/// `continue DEMO-2` then checks its answers and posts its RESUME, but its
/// Build waits: the capacity is DEMO-1's, dispatched first. A `do` while
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

    // Fail-closed: the open pull requests cannot be read.
    bench.put_ticket("DEMO-3", "maintainer", "Todo", &["agent"], "", "Other.");
    *bench.github.fail_pulls.lock().unwrap() = 1;
    let demo3 = TicketId::new("DEMO-3").unwrap();
    let (refused, printed) =
        bench.with_on_demand(Vec::new(), None, Vec::new(), |on_demand, sink| {
            on_demand.run(&demo3, sink)
        });
    match refused {
        Err(Stop::Waits { said, .. }) => assert!(
            said.starts_with("the work in flight outside Owlshift could not be read"),
            "{said}"
        ),
        other => panic!("{other:?}\n{printed}"),
    }
    assert_eq!(bench.record_of("DEMO-3"), None);
}
