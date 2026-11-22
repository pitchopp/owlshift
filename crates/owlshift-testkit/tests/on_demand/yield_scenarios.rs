//! Scenario S5 when a holder stops (OWL-621; architecture section 5, "A
//! stopped holder yields"), on the bench of `on_demand.rs`: DEMO-1 parks or
//! waits for its decider holding what its Build took, and DEMO-2's ref,
//! written as its own Build's admission would leave it, takes it.

use owlshift_contracts::refs::{Ended, EndedWhy, TicketDelivery, TicketHeld};
use owlshift_core::collision::{Dispatched, InFlight, weigh};
use owlshift_core::resource::{NamedResources, Resource};
use owlshift_core::vocab::Overlap;

use super::*;

/// A Build that stops for a person, which parks its ticket.
const BLOCKED: &str =
    r#"{"format":17,"status":"blocked","summary":"The migration needs a person's decision."}"#;

/// A yielded Build's rebase run's `done`.
const REBASED_YIELDED: &str = r#"{"format":17,"status":"done","summary":"Rebased onto the taker's merge; nothing conflicted."}"#;

/// A bench whose project's `[policy]` is given `policy`, and whose DEMO-1's
/// ref keeps an intake record declaring `zone`, a trivial ticket, which its
/// Build's admission takes.
fn yielding_bench(zone: &str, policy: &str) -> Bench {
    let bench = Bench::with("maintainer", "assignee = \"maintainer\"\n", policy);
    let mut intake = TicketIntake::parse(include_str!(
        "../../../owlshift-contracts/tests/fixtures/ticket-intake.json"
    ))
    .unwrap();
    intake.variant = owlshift_contracts::Variant::Trivial;
    intake.zones = vec![zone.to_owned()];
    intake.resources = Vec::new();
    let mut record = ticket_ref::blank(None);
    record.intake = Some(intake);
    let url = bench.remote.bare.to_string_lossy().into_owned();
    project::sync_checkout(&bench_git(&bench), &bench.dirs(), &url).unwrap();
    bench.write_record(&record);
    bench
}

/// A run of DEMO-1 that commits `file` and ends with `result`.
fn committing(bench: &Bench, file: &str, result: &str) -> Reply {
    let mut reply = bench.reply(None, Some(result));
    reply
        .files
        .insert(RelativePath::new(file).unwrap(), "-- one\n".to_owned());
    reply.commit = Some(format!("Add {file}"));
    reply
}

/// The runner's git on the bench.
fn bench_git(bench: &Bench) -> Git {
    let env = bench.env.clone();
    Git::with_setup("git", move |command| env.apply(command))
}

/// DEMO-2's ref as its Build's admission leaves it: at Build, holding
/// `file`, its record naming what it took of DEMO-1 (`taken`: paths and
/// named resources); at Watch with its pull request merged when `merged`.
fn sibling(bench: &Bench, file: &str, taken: (&[&str], &[&str]), merged: bool) {
    let git = bench_git(bench);
    let demo2 = TicketId::new("DEMO-2").unwrap();
    let owned = |items: &[&str]| items.iter().map(|i| (*i).to_owned()).collect::<Vec<_>>();
    let at = bench.clock.get();
    let mut record = bench.record();
    let stage = if merged { Stage::Watch } else { Stage::Build };
    record.state =
        PersistedState::from(&TicketState::restore(Status::Active(stage), 1, 0, 0, 0, 0).unwrap());
    record.delivery = merged.then(|| {
        let mut delivery = TicketDelivery::parse(include_str!(
            "../../../owlshift-contracts/tests/fixtures/ticket-delivery.json"
        ))
        .unwrap();
        delivery.pushing = None;
        delivery.ended = Some(Ended {
            why: EndedWhy::Merged,
            at,
            head: None,
        });
        delivery
    });
    let mut held = TicketHeld::dispatched(at);
    held.admit(vec![file.to_owned()], Vec::new(), None, at);
    held.record_overlap(&ticket(), &owned(taken.0), &owned(taken.1), at);
    record.held = Some(held);
    let previous = ticket_ref::read(&git, &bench.dirs().checkout(), &demo2)
        .unwrap()
        .map(|stored| stored.commit);
    write_through_run_file(&git, &bench.dirs(), &demo2, &record, previous.as_deref());
}

/// Another ticket's merge into the remote's `main` writing `file`, its
/// folders made; returns `main`'s new head.
fn merge_file(bench: &Bench, file: &str, text: &str) -> String {
    let root = bench.tmp.path();
    let clone = root.join("taker");
    let env = &bench.env;
    let bare = bench.remote.bare.to_str().unwrap();
    env.run(root, &["clone", "--quiet", bare, clone.to_str().unwrap()])
        .unwrap();
    let path = clone.join(file);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
    env.run(&clone, &["add", "--all"]).unwrap();
    env.run(&clone, &["commit", "--quiet", "-m", "The taker's merge"])
        .unwrap();
    env.run(
        &clone,
        &["push", "--quiet", "origin", "HEAD:refs/heads/main"],
    )
    .unwrap();
    bench.remote_main()
}

/// The `yield` decision events of DEMO-1's.
fn yields(bench: &Bench) -> Vec<Value> {
    data_of(&bench.events(), EventKind::Decision, "yield")
}

/// What DEMO-1's ref holds, as another ticket's check weighs it.
fn demo1_in_flight(bench: &Bench, named: &NamedResources) -> InFlight<String> {
    let stored = ticket_ref::read(&bench_git(bench), &bench.dirs().checkout(), &ticket())
        .unwrap()
        .unwrap();
    owlshift_runner::held::in_flight(&ticket(), &stored, named)
}

/// A Build of DEMO-3 wanting `file`, weighed as its admission is.
fn wanting(file: &str) -> InFlight<String> {
    InFlight {
        id: "DEMO-3".to_owned(),
        dispatched: Dispatched::Now,
        delivered: false,
        outside: false,
        yielded: false,
        held: vec![Resource::Zone(file.to_owned())],
    }
}

/// Scenario S5 under `rebase`, a parked holder: DEMO-1's Build, holding
/// the migration chain (`db/*`, a named resource), parks; it yields what it
/// holds, its PARKED comment saying so. DEMO-2 takes the chain. Restarted,
/// DEMO-1 waits for it, still yielded, so a third Build wanting its files
/// waits for nothing. DEMO-2 merges its migration; restarted again, DEMO-1
/// waits for nothing, its branch is rebased onto that merge before its
/// Build runs, and the Build delivers the combined code.
#[test]
fn scenario_s5_a_parked_build_yields_its_migration_chain_and_is_rebased_onto_the_taker() {
    let bench = yielding_bench(
        "db",
        "max_in_flight = 2\noverlap = \"rebase\"\n[stack.resources]\nmigrations = \"db/*\"\n",
    );
    let named = NamedResources::new([("migrations", "db/*")]).unwrap();
    let (parked, printed) = bench.run(vec![committing(&bench, "db/0001.sql", BLOCKED)], None);
    assert!(
        matches!(
            &parked,
            Err(Stop::Parked {
                reason: ParkReason::Blocked,
                ..
            })
        ),
        "{parked:?}\n{printed}"
    );
    assert!(bench.record().held.unwrap().yielded());
    let told = yields(&bench);
    assert_eq!(
        (&told[0]["yield"], &told[0]["why"], &told[0]["resources"]),
        (&json!("yielded"), &json!("parked"), &json!(["migrations"])),
        "{told:?}"
    );
    let comment = bench
        .comments()
        .into_iter()
        .find(|c| c.starts_with("[owlshift] PARKED\n"))
        .unwrap();
    assert!(
        comment.contains("**It yields what it held**")
            && comment.contains("resource `migrations`, `db`, `db/0001.sql`"),
        "{comment}"
    );

    // DEMO-2 takes the chain: DEMO-1, restarted, waits at Build for it,
    // still yielded.
    sibling(&bench, "db/0002.sql", (&[], &["migrations"]), false);
    let (waits, printed) = bench.continue_ticket(Vec::new());
    match &waits {
        Err(Stop::Waits { said, .. }) => assert!(
            said.starts_with(
                "DEMO-2 holds resource `migrations`, which it held before it yielded: it is \
                 admitted again once DEMO-2 no longer holds it"
            ),
            "{said}"
        ),
        other => panic!("{other:?}\n{printed}"),
    }
    let record = bench.record();
    assert!(record.held.as_ref().unwrap().yielded());
    assert_eq!(
        TicketState::try_from(&record.state).unwrap().status(),
        Status::Active(Stage::Build)
    );
    // Nothing waits for it: a third Build takes its file.
    let third = weigh(
        &wanting("db/0001.sql"),
        &[demo1_in_flight(&bench, &named)],
        Overlap::Rebase,
    );
    assert!(
        third.collision.is_none() && third.overlaps[0].yielded,
        "{third:?}"
    );

    // DEMO-2 merges its migration; DEMO-1, restarted again, is admitted,
    // rebased onto the merge and delivered.
    sibling(&bench, "db/0002.sql", (&[], &["migrations"]), true);
    let main = merge_file(&bench, "db/0002.sql", "-- two\n");
    let (env, worktree) = (bench.env.clone(), bench.worktree());
    let onto = main.clone();
    let rebase: During = (
        1,
        Box::new(move || {
            env.run(&worktree, &["rebase", "--quiet", &onto]).unwrap();
        }),
    );
    let (delivered, printed) = bench.continue_during(
        vec![
            bench.reply(None, Some(REBASED_YIELDED)),
            bench.reply(Some("Hello"), Some(DONE)),
        ],
        vec![rebase],
    );
    delivered.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    let told = yields(&bench);
    let readmitted: Vec<&Value> = told.iter().filter(|d| d["yield"] == "readmitted").collect();
    assert_eq!(
        (&readmitted[0]["stage"], &readmitted[0]["rebase_due"]),
        (&json!("build"), &json!(true)),
        "{told:?}"
    );
    let rebased: Vec<&Value> = told.iter().filter(|d| d["yield"] == "rebase").collect();
    assert_eq!(rebased.len(), 1, "{told:?}");
    assert_eq!(rebased[0]["onto"], json!(main));
    assert!(bench.last_of(Role::Rebase).rebase.unwrap().yielded);
    let held = bench.record().held.unwrap();
    assert!(!held.yielded() && !held.rebase_due, "{held:?}");
    // The Build's gate and the delivery ran on the combined code.
    assert!(is_ancestor(&bench, &main, &bench.remote_branch().unwrap()));
}

/// Scenario S5 under `rebase`, a round: DEMO-1's Build asks, holding
/// `app/one.rs`. Watch's pass yields nothing before the round's sixth hour,
/// then yields what it holds. DEMO-2 takes the file meanwhile; once the
/// decider answers, DEMO-1 is admitted again, sharing the file with DEMO-2,
/// an overlap recorded, and delivers.
#[test]
fn scenario_s5_a_round_yields_from_its_sixth_hour_and_its_answer_finds_its_file_taken() {
    let bench = yielding_bench("app", "max_in_flight = 2\noverlap = \"rebase\"\n");
    let (asked, printed) = bench.run(vec![committing(&bench, "app/one.rs", ROUND_1)], None);
    assert!(
        matches!(&asked, Err(Stop::NeedsInput { .. })),
        "{asked:?}\n{printed}"
    );
    let mut pass = 0;
    let (_, lines, printed) = bench.watch_passes(Vec::new(), &mut |_| {
        pass += 1;
        match pass {
            1 => {
                assert!(!bench.record().held.unwrap().yielded(), "before six hours");
                let later = bench.clock.get() + SignedDuration::from_hours(6);
                bench.clock.set(later);
                true
            }
            _ => false,
        }
    });
    let held = bench.record().held.unwrap();
    assert!(held.yielded(), "{held:?}\n{lines}\n{printed}");
    let told = yields(&bench);
    assert_eq!(
        (&told[0]["why"], &told[0]["paths"]),
        (&json!("round"), &json!(["app", "app/one.rs"])),
        "{told:?}"
    );

    sibling(&bench, "app/one.rs", (&["app/one.rs"], &[]), false);
    bench.answer("Q1: English.\nQ2: \"Hello, reader.\" Go.\n");
    let answered = check(&[
        ("Q1", "answered", "English."),
        ("Q2", "answered", "Hello, reader."),
    ]);
    let (delivered, printed) = bench.continue_ticket(vec![
        bench.reply(None, Some(&answered)),
        bench.reply(Some("Hello"), Some(DONE)),
    ]);
    delivered.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    let held = bench.record().held.unwrap();
    assert!(!held.yielded() && !held.rebase_due, "{held:?}");
    let shared: Vec<_> = held
        .overlaps
        .iter()
        .map(|overlap| (overlap.ticket.as_str(), overlap.paths.clone()))
        .collect();
    assert!(
        shared.contains(&("DEMO-2", vec!["app/one.rs".to_owned()])),
        "{shared:?}"
    );
    assert!(
        yields(&bench).iter().any(|d| d["yield"] == "readmitted"),
        "{printed}"
    );
}

/// DEMO-2's ref at Build, holding `file` (in flight), as its Build's
/// admission leaves it while it waits for another ticket.
fn waiting_sibling(bench: &Bench, file: &str) {
    let git = bench_git(bench);
    let demo2 = TicketId::new("DEMO-2").unwrap();
    let at = bench.clock.get();
    let mut record = bench.record();
    record.state = PersistedState::from(
        &TicketState::restore(Status::Active(Stage::Build), 1, 0, 0, 0, 0).unwrap(),
    );
    record.delivery = None;
    let mut held = TicketHeld::dispatched(at);
    held.admit(vec![file.to_owned()], Vec::new(), None, at);
    record.held = Some(held);
    write_through_run_file(&git, &bench.dirs(), &demo2, &record, None);
}

/// Scenario S5 under `exclusive` (OWL-621, OWL-623): nothing yields. While
/// DEMO-1's Build runs holding `app/one.rs`, DEMO-2's Build admission waits
/// for it; DEMO-1 parks keeping the file, its PARKED comment saying nothing
/// of a yield and that DEMO-2 waits for it, as `owlshift why`, the daily
/// digest and the web UI's card (`status --waits` in the UI's voice) say; a
/// third Build wanting that file waits for it too.
#[test]
fn under_exclusive_a_parked_holder_keeps_a_second_build_waiting() {
    let bench = yielding_bench("app", "max_in_flight = 2\n");
    waiting_sibling(&bench, "lib/two.rs");
    let data = bench.data.clone();
    // DEMO-2's wait, told as its Build's admission tells it, while DEMO-1's
    // Build (the command's first run) holds the file.
    let told: During = (
        1,
        Box::new(move || {
            let mut out = io::sink();
            let mut sink = EventSink::new(REPO, EventLog::in_dir(&data), &mut out);
            sink.emit(
                &TicketId::new("DEMO-2").unwrap(),
                None,
                EventKind::Decision,
                owlshift_runner::events::data([
                    ("watch", json!("waits")),
                    ("reason", json!("collision")),
                    ("stage", json!("build")),
                    ("resource", json!({"zone": "app/one.rs"})),
                    ("tickets", json!(["DEMO-1"])),
                ]),
            );
        }),
    );
    let (parked, printed) = bench.invoke(
        false,
        vec![committing(&bench, "app/one.rs", BLOCKED)],
        None,
        vec![told],
    );
    assert!(
        matches!(&parked, Err(Stop::Parked { .. })),
        "{parked:?}\n{printed}"
    );
    assert!(!bench.record().held.unwrap().yielded());
    assert!(yields(&bench).is_empty());
    let comment = bench
        .comments()
        .into_iter()
        .find(|c| c.starts_with("[owlshift] PARKED\n"))
        .unwrap();
    assert!(!comment.contains("It yields"), "{comment}");
    assert!(
        comment.contains(
            "**1 ticket waits for it**, for what it keeps: DEMO-2 for zone `app/one.rs`."
        ),
        "{comment}"
    );
    let found = weigh(
        &wanting("app/one.rs"),
        &[demo1_in_flight(&bench, &NamedResources::default())],
        Overlap::Exclusive,
    );
    assert_eq!(found.collision.map(|c| c.holder), Some("DEMO-1".to_owned()));

    // `owlshift why`, the daily digest and the web UI's card say it too.
    let now = bench.clock.get();
    let repo = Repo::parse(REPO).unwrap();
    let report =
        owlshift_runner::why::gather(&bench.data, &bench.dirs(), REPO, None, 2, &[], now).unwrap();
    let why = owlshift_runner::why::render(&report);
    assert!(
        why.contains(
            "  DEMO-1 [build, parked]: waits for a person (an answer, or `owlshift continue`) \
             and keeps what it holds; 1 ticket waits for it: DEMO-2 for zone `app/one.rs`\n"
        ),
        "{why}"
    );
    let summary = owlshift_runner::daemon::digest::gather(
        &bench.data,
        &[(&repo, &bench.dirs())],
        &[],
        now,
        now,
        &mut io::sink(),
    );
    let digest = owlshift_runner::daemon::digest::line(&summary, &TimeZone::UTC);
    assert!(
        digest.contains(
            "1 parked (DEMO-1; DEMO-1 keeps 1 ticket waiting: DEMO-2 for zone `app/one.rs`)"
        ),
        "{digest}"
    );
    let read = owlshift_runner::status::waits::gather(&bench.data, Some(&repo), 2, now);
    let waits = owlshift_runner::status::waits::build(&read, owlshift_runner::voice::Voice::Ui);
    let card = waits.projects[0]
        .tickets
        .iter()
        .find(|row| row.ticket.as_str() == TICKET)
        .and_then(|row| row.waits.clone())
        .unwrap();
    assert_eq!(
        card.text,
        "parked: waits for a person's Restart, and keeps what it holds; 1 ticket waits for it: \
         DEMO-2 for zone `app/one.rs`"
    );
}
