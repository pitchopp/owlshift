//! The web UI's overview against a drain played on the bench (OWL-470;
//! architecture section 5, "The local web UI"): `ui::overview::gather`, which
//! reads the ticket refs, `ready.json` and the event log from its offset,
//! compared with `owlshift status --waits`, which reads the whole log, and
//! with the bench's `ready.json`, pass by pass.

use std::num::NonZeroU32;

use owlshift_contracts::format::Format;
use owlshift_contracts::ids::QuestionId;
use owlshift_contracts::ready::Outcome;
use owlshift_contracts::refs::{Ask, AskKind};
use owlshift_contracts::result::Question;
use owlshift_contracts::service::{BuildIdentity, WatchRecord};
use owlshift_contracts::waits::{RefStatus, StatusWaits};
use owlshift_runner::served::Registry;
use owlshift_runner::slots::{Holder, MachineSlots};
use owlshift_runner::status::{gather_files, waits};
use owlshift_runner::ui::eventtail::EventTail;
use owlshift_runner::ui::header;
use owlshift_runner::ui::overview::{self, Deciders, Overview};
use owlshift_runner::voice::Voice;

use super::drain_scenarios::ready_now;
use super::parallel_scenarios::{parallel_bench, planned, standard_declaring};
use super::*;

/// The bench's project registered the way `owlshift start` does, so the
/// web UI, which lists the served projects, shows it. The capacity is not
/// needed here.
fn registered(bench: &Bench) {
    Registry::in_data_dir(&bench.data)
        .register(&Repo::parse(REPO).unwrap(), &bench.remote.checkout)
        .unwrap();
}

/// The waits document of the served projects now, in the UI's voice: the
/// words of `owlshift status --waits`, except that a parked wait names
/// Restart, which is what the overview's cards show.
fn waits_doc(bench: &Bench) -> StatusWaits {
    waits::build(
        &waits::gather(&bench.data, None, 2, bench.clock.get()),
        Voice::Ui,
    )
}

fn log_size(bench: &Bench) -> u64 {
    fs::metadata(EventLog::in_dir(&bench.data).path())
        .map(|meta| meta.len())
        .unwrap_or(0)
}

/// The tail kept across the builds of one scenario.
struct Watched {
    tail: EventTail,
    appended: u64,
}

impl Watched {
    fn new(bench: &Bench) -> Self {
        Self {
            tail: EventTail::new(&bench.data),
            appended: 0,
        }
    }

    /// The overview now. The tail reads only the bytes the log grew by since
    /// the last build, from where it stopped, and never starts over.
    fn build(&mut self, bench: &Bench) -> Overview {
        let (read_before, size_before) = (self.tail.bytes_read(), self.tail.offset());
        let size = log_size(bench);
        let built = overview::gather(
            &bench.data,
            &mut self.tail,
            2,
            bench.clock.get(),
            &Deciders::NONE,
        );
        assert_eq!(self.tail.restarts(), 0, "the tail started over");
        assert_eq!(
            self.tail.bytes_read() - read_before,
            size - size_before,
            "the tail read only the bytes appended since the last build"
        );
        assert_eq!(self.tail.offset(), size, "and read them whole");
        self.appended += size - size_before;
        built
    }
}

fn names(tickets: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut names: Vec<String> = tickets.into_iter().collect();
    names.sort();
    names
}

/// Compares the overview with `status --waits` and `ready.json`, ticket for
/// ticket: each row of the document has one card with its column, status
/// and wait; the cards beyond the rows are the admitted tickets of
/// `ready.json` with neither a ref nor a row, in its order; the list is a
/// permutation of the cards.
fn matches_status(bench: &Bench, built: &Overview, doc: &StatusWaits, report: &str) -> usize {
    let project = &doc.projects[0];
    assert!(project.served, "the bench project is served: {doc:#?}");
    let json = built.json();
    let view = json["projects"]
        .as_array()
        .unwrap()
        .iter()
        .find(|view| view["repository"] == project.repository.as_str())
        .unwrap_or_else(|| panic!("the overview lacks {}: {json:#}", project.repository));
    let cards = view["cards"].as_array().unwrap();
    let snapshot = ready_now(bench);
    let listed = |ticket: &TicketId| {
        snapshot
            .as_ref()
            .is_some_and(|ready| ready.tickets.iter().any(|t| t.ticket == *ticket))
    };
    for row in &project.tickets {
        let mine: Vec<_> = cards
            .iter()
            .filter(|card| card["ticket"] == row.ticket.as_str())
            .collect();
        assert_eq!(mine.len(), 1, "{} has one card\n{report}", row.ticket);
        let card = mine[0];
        let column = match (&row.stage, row.ref_status) {
            (_, RefStatus::Unread) => json!("unreadable"),
            (Some(stage), _) => serde_json::to_value(stage).unwrap(),
            (None, _) if listed(&row.ticket) => json!("not_started"),
            (None, _) => json!("starting"),
        };
        assert_eq!(card["column"], column, "{}\n{report}", row.ticket);
        assert_eq!(
            card["ref_status"],
            serde_json::to_value(row.ref_status).unwrap(),
            "{}",
            row.ticket
        );
        assert_eq!(
            card["wait"],
            serde_json::to_value(&row.waits).unwrap(),
            "{}: the wait and its words\n{report}",
            row.ticket
        );
    }
    // The cards beyond the rows are the admitted tickets of `ready.json`
    // that have neither a ref nor a row, in its order.
    let rows: Vec<&str> = project.tickets.iter().map(|r| r.ticket.as_str()).collect();
    let extras: Vec<&str> = cards
        .iter()
        .map(|card| card["ticket"].as_str().unwrap())
        .filter(|ticket| !rows.contains(ticket))
        .collect();
    let expected: Vec<&str> = snapshot
        .iter()
        .flat_map(|ready| &ready.tickets)
        .filter(|t| !matches!(t.drain, Outcome::NotAdmitted { .. }))
        .map(|t| t.ticket.as_str())
        .filter(|ticket| !rows.contains(ticket))
        .collect();
    assert_eq!(extras, expected, "the not-started extras\n{report}");
    for extra in &extras {
        let card = cards.iter().find(|c| c["ticket"] == *extra).unwrap();
        assert_eq!(card["column"], "not_started", "{extra}");
    }
    assert_eq!(cards.len(), rows.len() + extras.len());
    // Every card of the not-started column is in `ready.json`, and a ticket
    // `ready.json` lists with no ref is in it.
    for card in cards.iter().filter(|c| c["column"] == "not_started") {
        assert!(
            listed(&TicketId::new(card["ticket"].as_str().unwrap()).unwrap()),
            "{card}"
        );
    }
    // The list is a permutation of the cards.
    let list: Vec<String> = view["list"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t.as_str().unwrap().to_owned())
        .collect();
    let all: Vec<String> = cards
        .iter()
        .map(|c| c["ticket"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(names(list), names(all), "the list holds every card once");
    cards
        .iter()
        .filter(|card| card["column"] == "not_started")
        .count()
}

/// The role of the latest `run_started` of `ticket` in the bench's log.
fn latest_role(bench: &Bench, ticket: &str) -> Option<String> {
    bench
        .events()
        .iter()
        .rev()
        .find(|event| {
            event.kind == EventKind::RunStarted
                && event.project == REPO
                && event.ticket.as_ref().map(TicketId::as_str) == Some(ticket)
        })
        .and_then(|event| event.data.get("role"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

/// The header's counts of the bench's refs, as the page shows them next to
/// For you: (needing an answer, pull requests to review, parked).
fn header_counts(bench: &Bench) -> (usize, usize, usize) {
    let report = gather_files(&bench.data, 2, &[], bench.clock.get());
    let json = header::of(&report).0;
    let count = |key: &str| json["waiting"][key].as_u64().unwrap() as usize;
    (count("questions"), count("reviews"), count("parked"))
}

fn for_you_counts(built: &Overview) -> (usize, usize, usize) {
    let you = &built.for_you;
    (
        // The header counts the asks other people decide too (OWL-584).
        you.questions.len() + you.plans.len() + you.others.len(),
        you.reviews.len(),
        you.parked.len(),
    )
}

/// Under a capacity of one, the chain of blockers of OWL-459's scenario:
/// at every pass of the drain the overview has the cards `status --waits`
/// has, ticket for ticket, with the not-started extras of `ready.json`, and
/// the tail reads only what each pass appended. Then the ticket in flight is
/// given a slot, a plan to approve, a question and a park in turn, and For
/// you follows it as the header counts it.
#[test]
fn the_overview_matches_status_waits_and_ready_json_pass_by_pass() {
    let bench = parallel_bench("max_in_flight = 1\n");
    bench.put_ticket(
        "DEMO-2",
        "maintainer",
        "Todo",
        &["agent"],
        "blocked_by = [\"DEMO-1\"]\n",
        "Document it.",
    );
    bench.put_ticket(
        "DEMO-3",
        "maintainer",
        "Todo",
        &["agent"],
        "blocked_by = [\"DEMO-9\"]\n",
        "Library it.",
    );
    registered(&bench);
    let mut replies: Vec<Reply> = ["app", "docs", "lib"]
        .iter()
        .map(|zone| bench.reply(None, Some(&standard_declaring(&[zone]))))
        .collect();
    replies.push(bench.read(&planned()));
    let watched = RefCell::new(Watched::new(&bench));
    let checked = Cell::new(0);
    let not_started = Cell::new(0);
    let (_, lines, printed) = bench.drain_passes(replies, 4, |pass, _| {
        let report = format!("pass {pass}");
        let built = watched.borrow_mut().build(&bench);
        let waiting = matches_status(&bench, &built, &waits_doc(&bench), &report);
        not_started.set(not_started.get() + waiting);
        checked.set(checked.get() + 1);
        if pass == 3 {
            bench.put_ticket("DEMO-9", "maintainer", "Done", &[], "", "A blocker.");
        }
    });
    let report = format!("{lines}\n{printed}");
    assert_eq!(checked.get(), 3);
    assert!(
        not_started.get() > 0,
        "a pass showed tickets not started\n{report}"
    );
    let mut watched = watched.into_inner();
    let built = watched.build(&bench);
    let doc = waits_doc(&bench);
    let _ = matches_status(&bench, &built, &doc, &report);
    assert!(watched.appended > 0, "the drain wrote events");
    assert_eq!(watched.tail.restarts(), 0);

    // After the drain: DEMO-1 is in flight, DEMO-2 and DEMO-3 wait.
    let card = |built: &Overview, ticket: &str| {
        let json = built.json();
        json["projects"][0]["cards"]
            .as_array()
            .unwrap()
            .iter()
            .find(|card| card["ticket"] == ticket)
            .cloned()
            .unwrap_or_else(|| panic!("{ticket} has no card\n{report}"))
    };
    assert_eq!(
        card(&built, "DEMO-1")["column"],
        "design_review",
        "{report}"
    );
    assert_eq!(card(&built, "DEMO-2")["wait"]["reason"], "blocked_by");
    assert_eq!(card(&built, "DEMO-3")["wait"]["reason"], "slot");
    assert_eq!(for_you_counts(&built), header_counts(&bench));
    assert_eq!(for_you_counts(&built), (0, 0, 0));

    // A step of DEMO-1 holds a slot: the card says so, with the role of the
    // ticket's latest run, read from the log.
    let slots = MachineSlots::new(&bench.data, 2);
    let slot = slots
        .take(&Holder {
            project: REPO.to_owned(),
            ticket: ticket(),
            pid: std::process::id(),
            since: bench.clock.get(),
        })
        .unwrap()
        .expect("a free slot");
    let built = watched.build(&bench);
    let _ = matches_status(&bench, &built, &waits_doc(&bench), &report);
    let running = card(&built, "DEMO-1")["running"].clone();
    assert_eq!(running["slot"], 0, "{running}");
    let role = latest_role(&bench, "DEMO-1").expect("DEMO-1 ran");
    assert_eq!(running["role"], role.as_str(), "{running}");
    assert!(card(&built, "DEMO-2")["running"].is_null());
    drop(slot);

    // DEMO-1's ref says its plan waits for approval: the approval round's
    // question marks it. For you lists it as a plan and the header counts it
    // as an answer owed.
    let record = bench.record();
    let state = TicketState::try_from(&record.state).unwrap();
    let rewrite = |status: Status, questions: owlshift_runner::ticket_ref::TicketRecord| {
        let state = TicketState::restore(status, state.round().max(1), 0, 0, 0, 0).unwrap();
        bench.write_record(&TicketRecord {
            state: PersistedState::from(&state),
            ..questions
        });
    };
    let mut asked = record.clone();
    asked.questions.asks.push(Ask {
        kind: AskKind::Questions,
        round: NonZeroU32::new(1).unwrap(),
        at: bench.clock.get(),
        comment: "c-1".to_owned(),
        questions: vec![Question {
            id: QuestionId::new("Q1").unwrap(),
            category: "plan_approval".to_owned(),
            context: "The plan is ready.".to_owned(),
            text: "Approve the plan?".to_owned(),
            options: Vec::new(),
            recommendation: None,
            replan: false,
        }],
        decider: AskDecider {
            account: "u1".to_owned(),
            by: DeciderRule::Assignee,
        },
        verdicts: Vec::new(),
        result_refusal: None,
        labels: Default::default(),
    });
    rewrite(
        Status::NeedsInput {
            return_to: Stage::DesignReview,
        },
        asked.clone(),
    );
    let built = watched.build(&bench);
    let _ = matches_status(&bench, &built, &waits_doc(&bench), &report);
    assert_eq!(card(&built, "DEMO-1")["needs_you"], "plan");
    assert_eq!(built.for_you.plans.len(), 1);
    assert_eq!(built.for_you.plans[0].ticket.as_str(), "DEMO-1");
    assert_eq!(for_you_counts(&built), header_counts(&bench));
    assert_eq!(for_you_counts(&built), (1, 0, 0));

    // The same ref with a plain question is a question.
    let mut plain = asked.clone();
    plain.questions.asks[0].questions[0].category = "scope".to_owned();
    rewrite(
        Status::NeedsInput {
            return_to: Stage::DesignReview,
        },
        plain,
    );
    let built = watched.build(&bench);
    let _ = matches_status(&bench, &built, &waits_doc(&bench), &report);
    assert_eq!(card(&built, "DEMO-1")["needs_you"], "question");
    assert_eq!(built.for_you.questions.len(), 1);
    assert_eq!(for_you_counts(&built), header_counts(&bench));

    // Parked.
    rewrite(
        Status::Parked {
            at: Stage::DesignReview,
            awaiting_input: false,
        },
        asked.clone(),
    );
    let built = watched.build(&bench);
    let _ = matches_status(&bench, &built, &waits_doc(&bench), &report);
    assert_eq!(card(&built, "DEMO-1")["needs_you"], "parked");
    // The card says Restart, the button the page offers; the command's own
    // document keeps `owlshift continue`. Both give the same reason.
    let shown = card(&built, "DEMO-1")["wait"].clone();
    assert_eq!(shown["reason"], "parked");
    assert_eq!(
        shown["text"],
        "parked: waits for a person's Restart, and keeps what it holds"
    );
    let printed = waits::build(
        &waits::gather(&bench.data, None, 2, bench.clock.get()),
        Voice::Terminal,
    );
    let printed = printed.projects[0]
        .tickets
        .iter()
        .find(|row| row.ticket.as_str() == "DEMO-1")
        .and_then(|row| row.waits.as_ref())
        .expect("the parked ticket's wait");
    assert_eq!(printed.reason, "parked");
    assert!(
        printed.text.contains("`owlshift continue`"),
        "{}",
        printed.text
    );
    assert!(!printed.text.contains("Restart"), "{}", printed.text);
    assert_eq!(for_you_counts(&built), header_counts(&bench));
    assert_eq!(for_you_counts(&built), (0, 0, 1));
    // The parked ticket is first in the list.
    assert_eq!(built.projects[0].list[0].as_str(), "DEMO-1");

    // A ticket at Intake waiting for an answer, which `ready.json` does not
    // list and which holds nothing: `status --waits` leaves it out, For you
    // still lists it as a question, with no card, and counts it as the
    // header does.
    let other = TicketId::new("DEMO-7").unwrap();
    let mut waiting = asked;
    waiting.questions.asks[0].questions[0].category = "scope".to_owned();
    let state = TicketState::restore(
        Status::NeedsInput {
            return_to: Stage::Intake,
        },
        1,
        0,
        0,
        0,
        0,
    )
    .unwrap();
    waiting.state = PersistedState::from(&state);
    write_record_of(&bench, &other, &waiting);
    let listed =
        ready_now(&bench).is_some_and(|ready| ready.tickets.iter().any(|t| t.ticket == other));
    assert!(!listed, "ready.json does not list DEMO-7");
    let built = watched.build(&bench);
    let doc = waits_doc(&bench);
    assert!(
        doc.projects[0]
            .tickets
            .iter()
            .all(|row| row.ticket != other),
        "status --waits leaves DEMO-7 out: {doc:#?}"
    );
    let _ = matches_status(&bench, &built, &doc, &report);
    assert!(
        built.projects[0]
            .cards
            .iter()
            .all(|card| card.ticket != other),
        "DEMO-7 has no card"
    );
    let entry = built
        .for_you
        .questions
        .iter()
        .find(|entry| entry.ticket == other)
        .unwrap_or_else(|| panic!("DEMO-7 is in For you: {:#?}", built.for_you));
    assert!(!entry.card);
    assert_eq!(for_you_counts(&built), header_counts(&bench));
    assert_eq!(for_you_counts(&built), (1, 0, 1));
    assert_eq!(watched.tail.restarts(), 0);
}

/// Writes `ticket`'s ref the way a person's command does, under its lock.
fn write_record_of(bench: &Bench, ticket: &TicketId, record: &TicketRecord) {
    let env = bench.env.clone();
    let git = Git::with_setup("git", move |command| env.apply(command));
    let previous = ticket_ref::read(&git, &bench.dirs().checkout(), ticket)
        .unwrap()
        .map(|stored| stored.commit);
    write_through_run_file(&git, &bench.dirs(), ticket, record, previous.as_deref());
}

/// The title of the ticket that must show as text (OWL-508).
const HOSTILE: &str = "<img src=x onerror=alert(1)>";

/// `record` with a question of `category` asked of the decider, the ticket
/// in `status`.
fn asking(record: &TicketRecord, category: &str, status: Status) -> TicketRecord {
    let mut asked = record.clone();
    asked.questions.asks.push(Ask {
        kind: AskKind::Questions,
        round: NonZeroU32::new(1).unwrap(),
        at: "2026-10-02T09:00:00Z".parse().unwrap(),
        comment: "c-1".to_owned(),
        questions: vec![Question {
            id: QuestionId::new("Q1").unwrap(),
            category: category.to_owned(),
            context: "Seeded for the browsers.".to_owned(),
            text: "Approve?".to_owned(),
            options: Vec::new(),
            recommendation: None,
            replan: false,
        }],
        decider: AskDecider {
            account: "u1".to_owned(),
            by: DeciderRule::Assignee,
        },
        verdicts: Vec::new(),
        result_refusal: None,
        labels: Default::default(),
    });
    asked.state = PersistedState::from(&TicketState::restore(status, 1, 0, 0, 0, 0).unwrap());
    asked
}

/// The bench the browsers' checklist runs on (OWL-508), built under `root`
/// when given: under a capacity of one, DEMO-1 in flight at Design review,
/// DEMO-2 blocked by it, DEMO-3 (titled [`HOSTILE`]) waiting for a slot,
/// DEMO-4 parked, DEMO-5 with a plan to approve, DEMO-6 with a question;
/// `ready.json` read now, and two dated events in the log. No slot is held:
/// a holder is a live process, which a kept bench has none of.
pub(super) fn browsers_bench(root: Option<&Path>) -> Bench {
    let bench = super::parallel_scenarios::parallel_bench_in(root, "max_in_flight = 1\n");
    bench.put_ticket(
        "DEMO-2",
        "maintainer",
        "Todo",
        &["agent"],
        "blocked_by = [\"DEMO-1\"]\n",
        "Document it.",
    );
    bench.put_ticket_titled(
        "DEMO-3",
        HOSTILE,
        "maintainer",
        "Todo",
        &["agent"],
        "",
        "Library it.",
    );
    registered(&bench);
    let mut replies: Vec<Reply> = ["app", "docs", "lib"]
        .iter()
        .map(|zone| bench.reply(None, Some(&standard_declaring(&[zone]))))
        .collect();
    replies.push(bench.read(&planned()));
    bench.drain_passes(replies, 4, |pass, _| {
        if pass == 3 {
            bench.put_ticket("DEMO-9", "maintainer", "Done", &[], "", "A blocker.");
        }
    });
    let template = bench.record();
    let at = Stage::DesignReview;
    let seeded = [
        (
            "DEMO-4",
            asking(
                &template,
                "scope",
                Status::Parked {
                    at,
                    awaiting_input: false,
                },
            ),
        ),
        (
            "DEMO-5",
            asking(
                &template,
                "plan_approval",
                Status::NeedsInput { return_to: at },
            ),
        ),
        (
            "DEMO-6",
            asking(&template, "scope", Status::NeedsInput { return_to: at }),
        ),
    ];
    for (id, record) in &seeded {
        write_record_of(&bench, &TicketId::new(*id).unwrap(), record);
    }
    let now = Timestamp::now();
    owlshift_testkit::ui_bench::rewrite_ready(&bench.data, now, false).unwrap();
    owlshift_testkit::ui_bench::append(
        &bench.data,
        &owlshift_testkit::ui_bench::seeded_events(now),
    )
    .unwrap();
    bench
}

/// The cards of the one project of `built`, by ticket: (column, title).
fn columns(built: &Overview) -> Vec<(String, String, Option<String>)> {
    built.json()["projects"][0]["cards"]
        .as_array()
        .unwrap()
        .iter()
        .map(|card| {
            (
                card["ticket"].as_str().unwrap().to_owned(),
                card["column"].as_str().unwrap().to_owned(),
                card["title"].as_str().map(ToOwned::to_owned),
            )
        })
        .collect()
}

/// Builds the bench a person drives in Safari and Chrome (OWL-508;
/// `docs/design/live-checks/owl-508-overview.sh`) and keeps it: it runs only
/// when `OWLSHIFT_BENCH_KEEP` names an absolute, missing or empty folder: it
/// returns at once without the variable, and fails, naming it, otherwise. The bench lands in a folder of that
/// one, which outlives the process; the absolute path of its data directory
/// is written to `data-path` there.
#[test]
#[ignore = "builds the bench of the browsers' checklist; run by owl-508-overview.sh"]
fn keep_a_bench_for_the_browsers() {
    let Some(keep) = std::env::var_os("OWLSHIFT_BENCH_KEEP").map(PathBuf::from) else {
        return;
    };
    assert!(
        keep.is_absolute(),
        "OWLSHIFT_BENCH_KEEP must be an absolute path, not {}",
        keep.display()
    );
    fs::create_dir_all(&keep).unwrap();
    assert!(
        fs::read_dir(&keep).unwrap().next().is_none(),
        "OWLSHIFT_BENCH_KEEP ({}) must be empty: a bench is already there",
        keep.display()
    );
    let bench = browsers_bench(Some(&keep));

    // The end state, as the page will show it.
    let mut tail = EventTail::new(&bench.data);
    let built = overview::gather(&bench.data, &mut tail, 2, Timestamp::now(), &Deciders::NONE);
    let cards = columns(&built);
    let column = |ticket: &str| {
        cards
            .iter()
            .find(|(id, _, _)| id == ticket)
            .unwrap_or_else(|| panic!("{ticket} has no card: {cards:?}"))
    };
    assert_eq!(column("DEMO-1").1, "design_review");
    assert_eq!(column("DEMO-2").1, "ready");
    assert_eq!(column("DEMO-3").1, "ready");
    for parked in ["DEMO-4", "DEMO-5", "DEMO-6"] {
        assert_eq!(column(parked).1, "design_review");
    }
    assert_eq!(column("DEMO-3").2.as_deref(), Some(HOSTILE));
    let json = built.json();
    let card_of = |ticket: &str| {
        json["projects"][0]["cards"]
            .as_array()
            .unwrap()
            .iter()
            .find(|card| card["ticket"] == ticket)
            .unwrap()
            .clone()
    };
    assert_eq!(card_of("DEMO-2")["wait"]["reason"], "blocked_by");
    assert_eq!(card_of("DEMO-3")["wait"]["reason"], "slot");
    assert_eq!(card_of("DEMO-4")["needs_you"], "parked");
    assert_eq!(card_of("DEMO-5")["needs_you"], "plan");
    assert_eq!(card_of("DEMO-6")["needs_you"], "question");
    assert_eq!(for_you_counts(&built), (2, 0, 1));
    assert_eq!(for_you_counts(&built), header_counts(&bench));
    let ready = ready_now(&bench).expect("a ready.json");
    assert!(
        !owlshift_runner::ready::stale(&ready, Timestamp::now()),
        "the snapshot is not stale"
    );

    // The events a first visit and a reload tell apart.
    let since = tail.since(None, Timestamp::now()).json();
    let text = since.to_string();
    assert!(
        text.contains("DEMO-6"),
        "the question is in the last 24 hours: {text}"
    );
    assert!(!text.contains("DEMO-4"), "the old park is not: {text}");

    // It outlives the process.
    let data = bench.data.clone();
    let kept = bench.tmp.keep();
    assert!(data.starts_with(&kept));
    fs::write(keep.join("data-path"), data.to_str().unwrap()).unwrap();
}

// ---------------------------------------------------------------------------
// OWL-584: the operator is told only of what is theirs to decide.
// ---------------------------------------------------------------------------

/// The operator of this machine in the scenario below.
const OPERATOR: &str = "maintainer";

/// The lines `owlshift do`'s report shows for `outcome` on the bench's
/// recording notifier, this machine's operator being [`OPERATOR`].
fn notified_of(bench: &Bench, outcome: &Result<Delivered, Stop>) -> Vec<String> {
    let notifier = Notified::default();
    let tracker = MarkdownTracker::new(&bench.remote.checkout);
    let mut out = Vec::new();
    let mut sink = EventSink::new(REPO, EventLog::in_dir(&bench.data), &mut out);
    owlshift_runner::notify::notify_blocker(
        Some(&notifier),
        &tracker,
        &ticket(),
        outcome,
        &|| Some(OPERATOR.to_owned()),
        &mut sink,
    );
    notifier.0.into_inner()
}

/// The overview of the bench's served project, [`OPERATOR`] its operator.
fn overview_of(bench: &Bench) -> Overview {
    let operator = |_: &str| Some(OPERATOR.to_owned());
    let deciders = Deciders {
        operator: &operator,
        name: &|_, _| None,
    };
    overview::gather(
        &bench.data,
        &mut EventTail::new(&bench.data),
        2,
        bench.clock.get(),
        &deciders,
    )
}

/// OWL-584's acceptance: a question whose decider is another person, lea,
/// shows no notification on the bench's notifier, is listed under her name
/// after the operator's asks in the UI's snapshot, and is left out of the
/// digest's count while a watch that told the operator runs; a question the
/// operator decides notifies as before and is theirs; a park notifies
/// whoever decides the ticket.
#[test]
fn an_ask_another_person_decides_is_theirs_and_not_the_operators() {
    // lea, the ticket's assignee, decides its round.
    let bench = Bench::with("maintainer", "assignee = \"lea\"\n", "");
    registered(&bench);
    let (asked, printed) = bench.run(vec![bench.reply(None, Some(ROUND_1))], None);
    assert!(
        matches!(&asked, Err(Stop::NeedsInput { decider: Some(decider), .. }) if decider.as_str() == "lea"),
        "{asked:?}\n{printed}"
    );
    assert_eq!(notified_of(&bench, &asked), Vec::<String>::new());

    let built = overview_of(&bench);
    assert!(built.for_you.questions.is_empty(), "{:#?}", built.for_you);
    assert_eq!(built.for_you.others.len(), 1, "{:#?}", built.for_you);
    let json = built.json();
    let other = &json["for_you"]["others"][0];
    assert_eq!(other["ticket"], "DEMO-1");
    assert_eq!(other["kind"], "question");
    assert_eq!(other["decider"]["account"], "lea");
    assert_eq!(
        other["decider"]["name"], "lea",
        "the account stands for an unread name"
    );
    // The header still counts every ask waiting on a person.
    assert_eq!(for_you_counts(&built), header_counts(&bench));

    // The daily digest counts the operator's own: lea's question is left out
    // while a watch that told its operator runs, and counted with none.
    let repo = Repo::parse(REPO).unwrap();
    let dirs = bench.dirs();
    let digest = || {
        let now = bench.clock.get();
        let projects = [(&repo, &dirs)];
        owlshift_runner::daemon::digest::gather(
            &bench.data,
            &projects,
            &[],
            now,
            now,
            &mut Vec::new(),
        )
        .waiting
    };
    assert_eq!(
        digest().len(),
        1,
        "no watch tells the operator: counted, as before"
    );
    let watch = WatchRecord {
        format: Format,
        pid: std::process::id(),
        project: REPO.to_owned(),
        served: true,
        since: bench.clock.get(),
        binary: "/bin/owlshift".to_owned(),
        version: "0.0.0".to_owned(),
        build: BuildIdentity::from_build("", "0", "0", ""),
        operator: Some(OPERATOR.to_owned()),
    };
    let running = owlshift_runner::watch::presence::record(&dirs, &watch)
        .unwrap()
        .expect("recorded");
    assert!(digest().is_empty(), "lea's question is not the operator's");
    drop(running);

    // A park notifies whoever decides the ticket: here after refused
    // results, as OWL-183's scenario parks it.
    let parked_bench = Bench::with("maintainer", "assignee = \"lea\"\n", "");
    let misnumbered = NAMING.replace(r#""id":"Q1""#, r#""id":"Q2""#);
    let (parked, printed) = parked_bench.run(
        vec![
            parked_bench.reply(None, None),
            parked_bench.reply(None, Some(&misnumbered)),
            parked_bench.reply(None, Some(&misnumbered)),
        ],
        None,
    );
    assert!(
        matches!(&parked, Err(Stop::Parked { .. })),
        "{parked:?}\n{printed}"
    );
    assert_eq!(
        notified_of(&parked_bench, &parked),
        ["DEMO-1 is parked: a person must look"]
    );

    // The operator decides: notified as before, and the ask is theirs.
    let mine = Bench::new(true);
    registered(&mine);
    let (asked, printed) = mine.run(vec![mine.reply(None, Some(ROUND_1))], None);
    assert!(
        matches!(&asked, Err(Stop::NeedsInput { .. })),
        "{asked:?}\n{printed}"
    );
    let lines = notified_of(&mine, &asked);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(
        lines[0].starts_with("DEMO-1: questions wait for an answer on the ticket"),
        "{lines:?}"
    );
    let built = overview_of(&mine);
    assert_eq!(built.for_you.questions.len(), 1, "{:#?}", built.for_you);
    assert!(built.for_you.others.is_empty());
}
