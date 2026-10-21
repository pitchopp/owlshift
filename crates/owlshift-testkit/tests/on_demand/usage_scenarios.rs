//! Scenario S10's usage part (OWL-404; architecture section 5, "Parallel",
//! "Usage limits"): a harness at its usage limit is paused on the whole
//! machine until its reset, every run is checked before it starts, and the
//! operator is told once per outage. Two benches sharing one usage
//! directory play two projects of one machine.

use owlshift_core::usage::{self, Reported};
use owlshift_runner::usage as machine_usage;

use super::verify_scenarios::found;
use super::*;

/// The reset of this module's limits: the bench's clock reads 09:10.
const RESET: &str = "2026-10-02T12:00:00Z";

impl Bench {
    /// Plays a second project of this bench's machine: its usage state is
    /// `other`'s.
    fn share_usage(&self, other: &Bench) {
        *self.usage.borrow_mut() = other.usage.borrow().clone();
    }

    /// The machine's usage store, as this bench's commands read it.
    pub(super) fn usage_store(&self) -> UsageStore {
        UsageStore::new(self.usage.borrow().clone())
            .with_caps(self.usage_cap.get().map(|cap| (HarnessKind::Claude, cap)))
    }

    /// The `usage` decision events, as `usage` and `role`.
    fn usage_events(&self) -> Vec<(String, Value)> {
        data_of(&self.events(), EventKind::Decision, "usage")
            .into_iter()
            .map(|data| {
                (
                    data["usage"].as_str().unwrap().to_owned(),
                    data["role"].clone(),
                )
            })
            .collect()
    }

    /// Both clocks of the bench, the tracker's and the commands', at `at`.
    fn now_is(&self, at: Timestamp) {
        self.waited.set(SignedDuration::ZERO);
        self.clock.set(at);
    }
}

/// A reply cut by the usage limit, which resets at `reset`, or gives no
/// reset time.
fn limited(bench: &Bench, reset: Option<&str>) -> Reply {
    let mut reply = bench.reply(None, None);
    match reset {
        Some(reset) => reply.usage_limit = Some(reset.parse().unwrap()),
        None => reply.usage_limit_unknown = true,
    }
    reply
}

/// Something done while a run works: another project's run meets `harness`'s
/// limit, which resets at `reset`, in `store`, at `now`.
fn limit_elsewhere(store: UsageStore, harness: HarnessKind, reset: &str, now: Timestamp) -> During {
    let reset: Timestamp = reset.parse().unwrap();
    (
        1,
        Box::new(move || {
            let reported = Reported {
                limit: Some(Some(reset.as_millisecond())),
                answered: true,
                ..Reported::default()
            };
            store
                .update(harness, |state| {
                    usage::record(state, &reported, "elsewhere", now.as_millisecond());
                })
                .unwrap();
        }),
    )
}

/// S10 across two projects: project A's Build meets Claude Code's limit;
/// the ticket is interrupted, no failed run counted, and the operator is
/// told once. Before the reset, A's `continue` is refused before any run,
/// told nothing more, and project B's `watch --drain` starts nothing,
/// saying so once. At the reset, B takes its ticket in and builds it, and
/// A's Build resumes and delivers.
#[test]
fn scenario_s10_a_usage_limit_pauses_its_harness_on_every_project() {
    let a = Bench::new(true);
    let b = Bench::draining("maintainer");
    b.share_usage(&a);
    let reset: Timestamp = RESET.parse().unwrap();

    let (stopped, printed) = a.run(vec![limited(&a, Some(RESET))], None);
    assert_eq!(
        stopped.as_ref().err(),
        Some(&Stop::UsageLimit {
            resets_at: Some(reset)
        }),
        "{printed}"
    );
    let state = TicketState::try_from(&a.record().state).unwrap();
    assert_eq!(
        (state.status(), state.failed_runs()),
        (Status::Active(Stage::Build), 0)
    );
    let told = a.notified.0.borrow().clone();
    assert_eq!(told.len(), 1, "{told:?}");
    assert!(
        told[0].starts_with(
            "DEMO-1: Claude Code is at its usage limit until 2026-10-02T12:00:00Z: its runs \
             wait until then"
        ),
        "{told:?}"
    );
    let file = a.usage_store().read(HarnessKind::Claude).unwrap();
    assert_eq!(file.episodes.len(), 1);
    assert_eq!(file.episodes[0].until_ms, reset.as_millisecond());
    assert!(file.episodes[0].told);

    // A's `continue` before the reset: refused before its run, nothing
    // spent, nothing told again.
    let (again, printed) = a.continue_ticket(Vec::new());
    assert_eq!(
        again.as_ref().err(),
        Some(&Stop::UsageLimit {
            resets_at: Some(reset)
        }),
        "{printed}"
    );
    assert_eq!(a.briefs().len(), 1, "no run started");
    assert_eq!(
        a.usage_events(),
        [
            ("limited".to_owned(), Value::Null),
            ("waits".to_owned(), json!("build"))
        ]
    );
    assert_eq!(a.notified.0.borrow().len(), 1);
    let state = TicketState::try_from(&a.record().state).unwrap();
    assert_eq!(
        (state.status(), state.failed_runs()),
        (Status::Active(Stage::Build), 0)
    );

    // B, another project of the machine, starts nothing before the reset.
    let (outcomes, lines, printed) = b.drain(Vec::new(), 2, |_| {});
    assert!(outcomes.is_empty(), "{outcomes:?}\n{printed}");
    assert!(b.briefs().is_empty());
    assert_eq!(
        lines
            .matches(
                "Claude Code is at its usage limit until 2026-10-02T12:00:00Z: no step that runs \
                 it starts before then"
            )
            .count(),
        1,
        "{lines}"
    );
    assert!(b.notified.0.borrow().is_empty());
    let held = data_of(&b.events(), EventKind::Decision, "watch");
    assert_eq!(
        held,
        [
            json!({"watch": "waits", "reason": "usage_limit", "harness": "claude",
                "until": "2026-10-02T12:00:00Z"})
        ]
    );

    // At the reset, both go on.
    a.now_is(reset);
    b.now_is(reset);
    let replies = vec![
        b.reply(None, Some(&intake_result("done", None, ""))),
        b.reply(Some("Hello"), Some(DONE)),
    ];
    let (outcomes, lines, printed) = b.drain(replies, 2, |_| {});
    assert!(
        outcomes.last().is_some_and(Result::is_ok),
        "{outcomes:?}\n{lines}\n{printed}"
    );
    let (delivered, printed) = a.continue_ticket(vec![a.reply(Some("Hello"), Some(DONE))]);
    delivered.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    assert_eq!(a.notified.0.borrow().len(), 1);

    // A followed pull request is read while the harness is held: its read
    // runs no model.
    let store = a.usage_store();
    let later: Timestamp = "2026-10-02T15:00:00Z".parse().unwrap();
    (limit_elsewhere(store, HarnessKind::Claude, "2026-10-02T18:00:00Z", reset).1)();
    a.now_is(later);
    let reads = *a.github.follow_reads.lock().unwrap();
    let (outcomes, lines, _) = a.watch(Vec::new(), &mut |_| false);
    assert!(outcomes.is_empty(), "{outcomes:?}");
    assert!(*a.github.follow_reads.lock().unwrap() > reads, "{lines}");
}

/// S10 with a limit that gives no reset time: a 30-minute deadline holds
/// the harness; past it, a probe that died leaves its mark until its own
/// deadline; then one step probes, and another project's step waits for it;
/// a probe that meets the limit again keeps the outage, told once; the
/// probe that runs ends it.
#[test]
fn a_limit_without_a_reset_time_is_probed_once_after_its_deadline() {
    let a = Bench::new(true);
    let b = Bench::draining("maintainer");
    b.share_usage(&a);
    let at = |time: &str| -> Timestamp { format!("2026-10-02T{time}:00Z").parse().unwrap() };

    let (stopped, printed) = a.run(vec![limited(&a, None)], None);
    assert_eq!(
        stopped.as_ref().err(),
        Some(&Stop::UsageLimit { resets_at: None }),
        "{printed}"
    );
    let told = a.notified.0.borrow().clone();
    assert_eq!(told.len(), 1, "{told:?}");
    assert!(
        told[0].starts_with(
            "DEMO-1: Claude Code is at its usage limit and gave no reset time: its runs wait, \
             and one tries it again at 2026-10-02T09:40:00Z"
        ),
        "{told:?}"
    );

    // Before the deadline, nothing starts.
    b.now_is(at("09:39"));
    let (outcomes, _, printed) = b.drain(Vec::new(), 1, |_| {});
    assert!(outcomes.is_empty(), "{outcomes:?}\n{printed}");

    // A probe that died at 09:45 holds every step until 10:15.
    let store = a.usage_store();
    store
        .update(HarnessKind::Claude, |state| {
            usage::admit(state, "dead", at("09:45").as_millisecond())
        })
        .unwrap();
    b.now_is(at("10:14"));
    let (outcomes, _, printed) = b.drain(Vec::new(), 1, |_| {});
    assert!(outcomes.is_empty(), "{outcomes:?}\n{printed}");

    // Past it, B's intake probes and meets the limit again: the same
    // outage, told nothing more, a new deadline.
    b.now_is(at("10:15"));
    let (outcomes, _, printed) = b.drain(vec![limited(&b, None)], 1, |_| {});
    assert!(
        matches!(&outcomes[..], [Err(Stop::UsageLimit { resets_at: None })]),
        "{outcomes:?}\n{printed}"
    );
    assert_eq!(
        b.usage_events(),
        [
            ("probe".to_owned(), json!("intake")),
            ("limited".to_owned(), Value::Null)
        ]
    );
    assert!(b.notified.0.borrow().is_empty());
    let file = store.read(HarnessKind::Claude).unwrap();
    assert_eq!(
        (file.episodes[0].until_ms, file.episodes[0].told),
        (at("10:45").as_millisecond(), true)
    );

    // At the next deadline A's Build probes, runs and ends the outage; its
    // Verify pass and delivery follow.
    a.now_is(at("10:45"));
    let mut build = a.reply(Some("Hello"), Some(DONE));
    build.cost_micros = Some(1_000_000);
    let (delivered, printed) = a.continue_ticket(vec![build]);
    delivered.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    assert_eq!(
        a.usage_events(),
        [
            ("limited".to_owned(), Value::Null),
            ("probe".to_owned(), json!("build")),
            ("free".to_owned(), Value::Null)
        ]
    );
    let file = store.read(HarnessKind::Claude).unwrap();
    assert!(file.episodes.is_empty(), "{file:?}");
    assert_eq!(a.notified.0.borrow().len(), 1);
}

/// Every run of a step is checked: a Verify reviewer whose harness another
/// project's run held during the Build ends the step before it starts, the
/// Build's run kept, the operator told once; the Verify pass runs once the
/// limit resets. The resolver is checked too: the asking Build's step ends
/// at it, no question posted.
#[test]
fn each_run_of_a_step_is_checked_before_it_starts() {
    let bench = Bench::verifying();
    let reset: Timestamp = RESET.parse().unwrap();
    let seen: Timestamp = "2026-10-02T09:10:00Z".parse().unwrap();
    let during = limit_elsewhere(bench.usage_store(), HarnessKind::Claude, RESET, seen);
    let (stopped, printed) = bench.invoke(
        false,
        vec![bench.reply(Some("Hello"), Some(DONE))],
        None,
        vec![during],
    );
    assert_eq!(
        stopped.as_ref().err(),
        Some(&Stop::UsageLimit {
            resets_at: Some(reset)
        }),
        "{printed}"
    );
    assert_eq!(bench.roles(), [Role::Build]);
    assert_eq!(
        bench.usage_events(),
        [("waits".to_owned(), json!("verify"))]
    );
    assert_eq!(bench.notified.0.borrow().len(), 1);
    let state = TicketState::try_from(&bench.record().state).unwrap();
    assert_eq!(state.failed_runs(), 0);

    bench.now_is(reset);
    let (delivered, printed) = bench.continue_ticket(vec![bench.reviewing(&found(&[]))]);
    delivered.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    assert_eq!(bench.roles(), [Role::Build, Role::Verify]);

    // The resolver, after an asking Build.
    let asking = Bench::new(true);
    let during = limit_elsewhere(asking.usage_store(), HarnessKind::Claude, RESET, seen);
    let (stopped, printed) = asking.invoke(
        false,
        vec![asking.reply(None, Some(NAMING))],
        None,
        vec![during],
    );
    assert!(
        matches!(&stopped, Err(Stop::UsageLimit { .. })),
        "{stopped:?}\n{printed}"
    );
    assert_eq!(asking.roles(), [Role::Build]);
    assert_eq!(
        asking.usage_events(),
        [("waits".to_owned(), json!("resolver"))]
    );
    assert!(asking.comments().is_empty(), "no question posted");
}

/// The opt-in Codex reviewer held by the machine's usage state drops its
/// seat, told nothing: the pass runs on Claude Code and the ticket is
/// delivered.
#[test]
fn a_codex_seat_held_by_its_usage_limit_drops_and_never_stops() {
    let bench = Bench::verifying();
    *bench.codex.borrow_mut() = Some(Ok(()));
    let now: Timestamp = "2026-10-02T09:10:00Z".parse().unwrap();
    (limit_elsewhere(bench.usage_store(), HarnessKind::Codex, RESET, now).1)();
    let replies = vec![
        bench.reply(Some("Hello"), Some(DONE)),
        bench.reviewing(&found(&[])),
    ];
    let (delivered, printed) = bench.run(replies, None);
    delivered.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    assert_eq!(bench.roles(), [Role::Build, Role::Verify]);
    let review = bench.record().review.unwrap();
    assert_eq!(
        review.passes[0].dropped.as_ref().unwrap().reason,
        "Codex reached its usage limit, until 2026-10-02T12:00:00Z."
    );
    assert!(bench.notified.0.borrow().is_empty());
    assert_eq!(
        machine_usage::key(HarnessKind::Codex),
        "codex",
        "its own file"
    );
}

/// A run admitted as the probe that ends before it starts, its run
/// directory refused, leaves no probe mark: the next run probes at once
/// rather than every run waiting out a probe that never ran.
#[test]
fn a_probe_that_never_starts_leaves_no_mark() {
    let bench = Bench::new(true);
    let store = bench.usage_store();
    let seen: Timestamp = "2026-10-02T08:00:00Z".parse().unwrap();
    store
        .update(HarnessKind::Claude, |state| {
            let reported = Reported {
                limit: Some(None),
                answered: true,
                ..Reported::default()
            };
            usage::record(state, &reported, "elsewhere", seen.as_millisecond());
        })
        .unwrap();
    // The ticket's run directories cannot be made.
    let root = bench.dirs().root().to_owned();
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("runs"), "").unwrap();
    let (stopped, printed) = bench.run(Vec::new(), None);
    assert!(
        matches!(&stopped, Err(Stop::Refused(why)) if why.starts_with("the run directory")),
        "{stopped:?}\n{printed}"
    );
    assert_eq!(bench.usage_events(), [("probe".to_owned(), json!("build"))]);
    let file = store.read(HarnessKind::Claude).unwrap();
    assert_eq!(file.probe, None);
    assert_eq!(file.episodes.len(), 1, "the outage stands");
}

/// Claude Code meets its limit during the Build, at 09:10 on the bench's
/// clock: the Build goes on, the next run finds it limited.
fn claude_limited_after_the_build(bench: &Bench) -> During {
    let seen: Timestamp = "2026-10-02T09:10:00Z".parse().unwrap();
    limit_elsewhere(bench.usage_store(), HarnessKind::Claude, RESET, seen)
}

/// OWL-406, scenario S17's fallback part: Claude Code at its limit, the
/// Verify pass runs its reviewers on the declared fallback, which the
/// review's line in the DELIVERY report and a `routing` event say; the
/// Build before it, an authoring role, was not moved.
#[test]
fn scenario_s17_a_review_runs_on_the_fallback_while_claude_code_is_at_its_limit() {
    let bench = Bench::verifying();
    *bench.review_fallback.borrow_mut() = Some(Ok(()));
    let replies = vec![
        bench.reply(Some("Hello"), Some(DONE)),
        bench.reviewing(&found(&[])),
    ];
    let (outcome, printed) = bench.invoke(
        false,
        replies,
        None,
        vec![claude_limited_after_the_build(&bench)],
    );
    outcome.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    assert_eq!(bench.roles(), [Role::Build, Role::Verify]);
    let harnesses: Vec<HarnessKind> = bench.routed().iter().map(|r| r.harness).collect();
    assert_eq!(harnesses, [HarnessKind::Claude, HarnessKind::Codex]);

    let review = bench.record().review.unwrap();
    let reviewer = &review.passes[0].reviewers[0];
    assert_eq!(
        (reviewer.harness, reviewer.fallback, reviewer.same_model),
        (HarnessKind::Codex, true, false)
    );
    assert!(review.passes[0].dropped.is_none());
    let routing: Vec<Value> = data_of(&bench.events(), EventKind::Decision, "routing")
        .into_iter()
        .filter(|data| data["routing"] == "fallback")
        .collect();
    assert_eq!(routing.len(), 1, "{routing:?}");
    assert_eq!(
        routing[0],
        json!({
            "routing": "fallback", "reason": "usage_limit", "role": "verify",
            "stage": "verify", "pass": 1, "harness": "codex", "in_place_of": "claude",
            "until": RESET
        })
    );
    let report = bench.comments().pop().unwrap();
    let line = "critic and conformance on Codex (its default model, in place of Claude Code, \
                at its usage limit)";
    assert!(report.contains(line), "{report}");
}

/// OWL-406: without a usable fallback the review waits for the reset like
/// any run, and goes back to Claude Code once the limit is over; a fallback
/// that is itself limited, or an authoring role, never takes a run.
#[test]
fn a_review_without_a_usable_fallback_waits_and_returns_to_claude_code_after_the_reset() {
    let reset: Timestamp = RESET.parse().unwrap();
    // None declared.
    let bench = Bench::verifying();
    let replies = vec![bench.reply(Some("Hello"), Some(DONE))];
    let (stopped, printed) = bench.invoke(
        false,
        replies,
        None,
        vec![claude_limited_after_the_build(&bench)],
    );
    assert_eq!(
        stopped.as_ref().err(),
        Some(&Stop::UsageLimit {
            resets_at: Some(reset)
        }),
        "{printed}"
    );
    assert_eq!(bench.roles(), [Role::Build]);

    // Declared, but Codex cannot serve here, or is limited itself.
    for (ready, codex_limited) in [
        (Err("`codex` is not on the PATH".to_owned()), false),
        (Ok(()), true),
    ] {
        let bench = Bench::verifying();
        *bench.review_fallback.borrow_mut() = Some(ready);
        let now: Timestamp = "2026-10-02T09:10:00Z".parse().unwrap();
        if codex_limited {
            (limit_elsewhere(bench.usage_store(), HarnessKind::Codex, RESET, now).1)();
        }
        let replies = vec![bench.reply(Some("Hello"), Some(DONE))];
        let (stopped, printed) = bench.invoke(
            false,
            replies,
            None,
            vec![claude_limited_after_the_build(&bench)],
        );
        assert_eq!(
            stopped.as_ref().err(),
            Some(&Stop::UsageLimit {
                resets_at: Some(reset)
            }),
            "{printed}"
        );
        assert_eq!(bench.roles(), [Role::Build]);
        assert!(
            bench
                .record()
                .review
                .is_none_or(|review| review.passes.is_empty())
        );

        // The reset: the review runs on Claude Code again.
        *bench.review_fallback.borrow_mut() = Some(Ok(()));
        bench.now_is(reset);
        let (delivered, printed) = bench.continue_ticket(vec![bench.reviewing(&found(&[]))]);
        delivered.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
        let harnesses: Vec<HarnessKind> = bench.routed().iter().map(|r| r.harness).collect();
        assert_eq!(harnesses.last(), Some(&HarnessKind::Claude));
        let review = bench.record().review.unwrap();
        assert!(!review.passes[0].reviewers[0].fallback);
    }

    // An authoring role at the limit waits even with a fallback ready.
    let bench = Bench::new(true);
    *bench.review_fallback.borrow_mut() = Some(Ok(()));
    let now: Timestamp = "2026-10-02T09:10:00Z".parse().unwrap();
    (limit_elsewhere(bench.usage_store(), HarnessKind::Claude, RESET, now).1)();
    let (stopped, printed) = bench.run(Vec::new(), None);
    assert!(
        matches!(&stopped, Err(Stop::UsageLimit { .. })),
        "{stopped:?}\n{printed}"
    );
    assert!(bench.briefs().is_empty());
}

/// Something done while a run works: another run, of this project or of
/// another, reports Claude Code's `window` at `share`, resetting at `reset`
/// (OWL-405), at the bench's 09:10.
fn window_elsewhere(bench: &Bench, window: &str, share: f64, reset: &str) -> During {
    let store = bench.usage_store();
    let seen: Timestamp = "2026-10-02T09:10:00Z".parse().unwrap();
    let reported = Reported {
        windows: vec![(
            window.to_owned(),
            usage::Window {
                share,
                resets_at_ms: reset.parse::<Timestamp>().unwrap().as_millisecond(),
                read_at_ms: seen.as_millisecond(),
            },
        )],
        answered: true,
        ..Reported::default()
    };
    (
        1,
        Box::new(move || {
            store
                .update(HarnessKind::Claude, |state| {
                    usage::record(state, &reported, "elsewhere", seen.as_millisecond());
                })
                .unwrap();
        }),
    )
}

/// OWL-405: a window at the personal file's cap pauses Claude Code until
/// that window's reset, as a limit does: the next run is refused before it
/// starts, the operator is told once in the cap's words, and the run goes
/// on at the reset. A window below the cap, and a harness with no cap,
/// hold nothing.
#[test]
fn a_window_at_the_usage_cap_pauses_the_harness_until_that_windows_reset() {
    let reset: Timestamp = RESET.parse().unwrap();
    // The five-hour window at 85% of a cap of 80.
    let bench = Bench::verifying();
    bench.usage_cap.set(Some(80));
    let replies = vec![bench.reply(Some("Hello"), Some(DONE))];
    let (stopped, printed) = bench.invoke(
        false,
        replies,
        None,
        vec![window_elsewhere(&bench, "five_hour", 0.85, RESET)],
    );
    assert_eq!(
        stopped.as_ref().err(),
        Some(&Stop::UsageLimit {
            resets_at: Some(reset)
        }),
        "{printed}"
    );
    assert_eq!(bench.roles(), [Role::Build], "the review was refused");
    let told = bench.notified.0.borrow().clone();
    assert_eq!(
        told,
        [
            "DEMO-1: Claude Code reached its usage cap: its runs wait until its window resets \
             at 2026-10-02T12:00:00Z"
                .to_owned()
        ],
        "{told:?}"
    );
    let events = bench.usage_events();
    assert_eq!(
        events
            .iter()
            .map(|(what, _)| what.as_str())
            .collect::<Vec<_>>(),
        ["capped", "waits"],
        "{events:?}"
    );

    // Refused again before the reset, told nothing more.
    let (again, _) = bench.continue_ticket(Vec::new());
    assert!(matches!(&again, Err(Stop::UsageLimit { .. })));
    assert_eq!(bench.notified.0.borrow().len(), 1);
    assert_eq!(bench.roles(), [Role::Build]);

    // The reset: the review runs.
    bench.now_is(reset);
    let (delivered, printed) = bench.continue_ticket(vec![bench.reviewing(&found(&[]))]);
    delivered.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    assert_eq!(bench.roles(), [Role::Build, Role::Verify]);

    // Below the cap, or with no cap, nothing is held.
    for (cap, share) in [(Some(90), 0.85), (None, 0.99)] {
        let bench = Bench::verifying();
        bench.usage_cap.set(cap);
        let replies = vec![
            bench.reply(Some("Hello"), Some(DONE)),
            bench.reviewing(&found(&[])),
        ];
        let (outcome, printed) = bench.invoke(
            false,
            replies,
            None,
            vec![window_elsewhere(&bench, "five_hour", share, RESET)],
        );
        outcome.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
        assert_eq!(bench.roles(), [Role::Build, Role::Verify]);
    }
}

/// OWL-405: a review role goes on the declared fallback while a cap holds
/// Claude Code, as it does at a limit.
#[test]
fn a_review_runs_on_the_fallback_while_a_cap_holds_claude_code() {
    let bench = Bench::verifying();
    bench.usage_cap.set(Some(80));
    *bench.review_fallback.borrow_mut() = Some(Ok(()));
    let replies = vec![
        bench.reply(Some("Hello"), Some(DONE)),
        bench.reviewing(&found(&[])),
    ];
    let (outcome, printed) = bench.invoke(
        false,
        replies,
        None,
        vec![window_elsewhere(&bench, "five_hour", 0.8, RESET)],
    );
    outcome.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    let harnesses: Vec<HarnessKind> = bench.routed().iter().map(|r| r.harness).collect();
    assert_eq!(harnesses, [HarnessKind::Claude, HarnessKind::Codex]);
    assert!(bench.record().review.unwrap().passes[0].reviewers[0].fallback);
}
