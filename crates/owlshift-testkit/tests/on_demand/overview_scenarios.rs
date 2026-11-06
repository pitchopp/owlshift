//! The web UI's overview against a drain played on the bench (OWL-470;
//! architecture section 5, "The local web UI"): `ui::overview::gather`, which
//! reads the ticket refs, `ready.json` and the event log from its offset,
//! compared with `owlshift status --waits`, which reads the whole log, and
//! with the bench's `ready.json`, pass by pass.

use std::num::NonZeroU32;

use owlshift_contracts::ids::QuestionId;
use owlshift_contracts::ready::Outcome;
use owlshift_contracts::refs::{Ask, AskKind};
use owlshift_contracts::result::Question;
use owlshift_contracts::waits::{RefStatus, StatusWaits};
use owlshift_runner::served::Registry;
use owlshift_runner::slots::{Holder, MachineSlots};
use owlshift_runner::status::{gather_files, waits};
use owlshift_runner::ui::eventtail::EventTail;
use owlshift_runner::ui::header;
use owlshift_runner::ui::overview::{self, Overview};

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

/// What `owlshift status --waits` reads of the served projects now.
fn waits_doc(bench: &Bench) -> StatusWaits {
    waits::build(&waits::gather(&bench.data, None, 2, bench.clock.get()))
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
        let built = overview::gather(&bench.data, &mut self.tail, 2, bench.clock.get());
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
        you.questions.len() + you.plans.len(),
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
        asked,
    );
    let built = watched.build(&bench);
    let _ = matches_status(&bench, &built, &waits_doc(&bench), &report);
    assert_eq!(card(&built, "DEMO-1")["needs_you"], "parked");
    assert_eq!(for_you_counts(&built), header_counts(&bench));
    assert_eq!(for_you_counts(&built), (0, 0, 1));
    // The parked ticket is first in the list.
    assert_eq!(built.projects[0].list[0].as_str(), "DEMO-1");
    assert_eq!(watched.tail.restarts(), 0);
}
