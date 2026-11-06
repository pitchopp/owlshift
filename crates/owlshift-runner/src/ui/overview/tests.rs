use owlshift_contracts::format::Format;
use owlshift_contracts::ready::ReadyTicket;
use owlshift_contracts::waits::HeldRun;

use super::*;

fn id(ticket: &str) -> TicketId {
    TicketId::new(ticket).unwrap()
}

fn ts(text: &str) -> Timestamp {
    text.parse().unwrap()
}

fn wait(reason: &str, text: &str) -> Wait {
    Wait {
        reason: reason.to_owned(),
        since: None,
        behind: Vec::new(),
        resource: None,
        harness: None,
        until: None,
        text: text.to_owned(),
    }
}

fn row(ticket: &str, stage: Option<Stage>, status: RefStatus) -> TicketWaits {
    TicketWaits {
        ticket: id(ticket),
        stage,
        ref_status: status,
        holds_capacity: false,
        run: None,
        waits: None,
    }
}

fn active(ticket: &str, stage: Stage) -> TicketWaits {
    row(ticket, Some(stage), RefStatus::Active)
}

fn holding(mut row: TicketWaits, slot: u32) -> TicketWaits {
    row.run = Some(HeldRun {
        slot,
        pid: 7,
        since: ts("2026-11-06T09:00:00Z"),
    });
    row
}

fn waits_of(rows: Vec<TicketWaits>) -> StatusWaits {
    StatusWaits {
        format: Format,
        at: ts("2026-11-06T10:00:00Z"),
        machine_pause: None,
        runs: None,
        projects: vec![ProjectWaits {
            repository: "o/r".to_owned(),
            served: true,
            max_in_flight: Some(2),
            in_flight: 0,
            paused: None,
            quarantined: false,
            tickets: rows,
        }],
        notes: Vec::new(),
    }
}

fn listed(ticket: &str, drain: Outcome) -> ReadyTicket {
    ReadyTicket {
        ticket: id(ticket),
        title: format!("Title of {ticket}"),
        priority: Priority::High,
        blockers: Vec::new(),
        drain,
    }
}

fn snapshot(tickets: Vec<ReadyTicket>) -> ReadySnapshot {
    ReadySnapshot {
        format: Format,
        at: ts("2026-11-06T10:00:00Z"),
        interval_ms: 60_000,
        tickets,
    }
}

fn detail(status: RefStatus) -> RefDetail {
    RefDetail {
        status,
        pull_request: None,
        questions: Vec::new(),
        plan_to_approve: false,
    }
}

fn view(waits: &StatusWaits, aux: Aux) -> Overview {
    build(
        waits,
        &[aux],
        &BTreeMap::new(),
        &BTreeMap::new(),
        ts("2026-11-06T10:00:30Z"),
        0,
        Vec::new(),
    )
}

fn card<'a>(overview: &'a Overview, ticket: &str) -> &'a Card {
    overview.projects[0]
        .cards
        .iter()
        .find(|card| card.ticket.as_str() == ticket)
        .unwrap_or_else(|| panic!("{ticket} has no card in {overview:#?}"))
}

fn sorted(tickets: impl IntoIterator<Item = TicketId>) -> Vec<String> {
    let mut names: Vec<String> = tickets.into_iter().map(|t| t.to_string()).collect();
    names.sort_by(|a, b| natural_cmp(a, b));
    names
}

/// A ticket per stage lands in its column, one row is one card, and the
/// columns no stage names hold what has no stage.
#[test]
fn a_row_is_one_card_in_the_column_its_ref_and_the_snapshot_give_it() {
    let mut told = row("T-11", None, RefStatus::NoRef);
    told.waits = Some(wait("next_pass", "chosen, starts at the next pass"));
    let mut rows = vec![
        active("T-1", Stage::Intake),
        active("T-2", Stage::Ready),
        active("T-3", Stage::Design),
        active("T-4", Stage::DesignReview),
        active("T-5", Stage::Build),
        active("T-6", Stage::Verify),
        active("T-7", Stage::Deliver),
        active("T-8", Stage::Watch),
        row("T-9", None, RefStatus::Unread),
        row("T-10", Some(Stage::Build), RefStatus::NeedsInput),
        told,
        holding(row("T-12", None, RefStatus::NoRef), 1),
    ];
    rows.sort_by(|a, b| natural_cmp(a.ticket.as_str(), b.ticket.as_str()));
    let waits = waits_of(rows);
    let ready = snapshot(vec![
        listed(
            "T-20",
            Outcome::Waits {
                wait: wait("blocked_by", "blocked by T-5"),
            },
        ),
        listed(
            "T-11",
            Outcome::Waits {
                wait: wait("next_pass", "chosen, starts at the next pass"),
            },
        ),
        listed(
            "T-21",
            Outcome::NotAdmitted {
                wait: wait("not_admitted", "not admitted"),
            },
        ),
        listed("T-22", Outcome::TakenIn),
        listed("T-8", Outcome::Running),
    ]);
    let overview = view(
        &waits,
        Aux {
            details: BTreeMap::new(),
            ready: Some(ready),
        },
    );
    let column = |ticket: &str| card(&overview, ticket).column;
    assert_eq!(column("T-1"), Column::Intake);
    assert_eq!(column("T-2"), Column::Ready);
    assert_eq!(column("T-3"), Column::Design);
    assert_eq!(column("T-4"), Column::DesignReview);
    assert_eq!(column("T-5"), Column::Build);
    assert_eq!(column("T-6"), Column::Verify);
    assert_eq!(column("T-7"), Column::Deliver);
    assert_eq!(column("T-8"), Column::Watch, "listed with a ref: once");
    assert_eq!(column("T-9"), Column::Unreadable);
    assert_eq!(column("T-10"), Column::Build);
    assert_eq!(column("T-11"), Column::NotStarted, "a listed NoRef row");
    assert_eq!(column("T-12"), Column::Starting, "a slot holder not listed");
    assert_eq!(column("T-20"), Column::NotStarted, "an extra");
    assert_eq!(column("T-22"), Column::NotStarted);
    let cards = &overview.projects[0].cards;
    assert!(cards.iter().all(|card| card.ticket.as_str() != "T-21"));
    assert_eq!(cards.len(), 12 + 2, "the rows and the extras");
    assert_eq!(
        card(&overview, "T-11").wait.as_ref().unwrap().reason,
        "next_pass",
        "status --waits' own wait"
    );
    assert_eq!(
        card(&overview, "T-20").wait.as_ref().unwrap().reason,
        "blocked_by"
    );
    assert_eq!(card(&overview, "T-22").drain, Some("taken_in"));
    assert_eq!(
        card(&overview, "T-20").title.as_deref(),
        Some("Title of T-20")
    );
    assert_eq!(card(&overview, "T-8").position, Some(4));
    let not_started: Vec<_> = overview.projects[0]
        .list
        .iter()
        .filter(|ticket| column(ticket.as_str()) == Column::NotStarted)
        .map(TicketId::as_str)
        .collect();
    assert_eq!(not_started, ["T-20", "T-11", "T-22"], "ready.json's order");
}

/// The list is a permutation of the cards, in the order the design names.
#[test]
fn the_list_is_every_card_once_those_needing_you_first() {
    let waits = waits_of(vec![
        active("T-1", Stage::Intake),
        row("T-2", Some(Stage::Design), RefStatus::NeedsInput),
        active("T-3", Stage::Watch),
        active("T-4", Stage::Build),
        row("T-5", None, RefStatus::Unread),
        holding(row("T-6", None, RefStatus::NoRef), 0),
        row("T-7", Some(Stage::Build), RefStatus::Parked),
        active("T-8", Stage::Watch),
    ]);
    let mut details = BTreeMap::new();
    details.insert(
        id("T-2"),
        RefDetail {
            plan_to_approve: true,
            ..detail(RefStatus::NeedsInput)
        },
    );
    details.insert(id("T-7"), detail(RefStatus::Parked));
    details.insert(
        id("T-8"),
        RefDetail {
            pull_request: Some(9),
            ..detail(RefStatus::Active)
        },
    );
    let ready = snapshot(vec![
        listed("T-31", Outcome::TakenIn),
        listed("T-30", Outcome::Dispatched),
    ]);
    let overview = view(
        &waits,
        Aux {
            details,
            ready: Some(ready),
        },
    );
    let project = &overview.projects[0];
    assert_eq!(
        sorted(project.list.clone()),
        sorted(project.cards.iter().map(|card| card.ticket.clone())),
        "a permutation of the cards"
    );
    assert_eq!(project.list.len(), project.cards.len());
    let order: Vec<_> = project.list.iter().map(TicketId::as_str).collect();
    assert_eq!(
        order,
        [
            "T-2", "T-8", "T-7", // a plan, a review, a parked ticket
            "T-3", "T-4", "T-1", // the others, Watch back to Intake
            "T-6", "T-5", // starting, unreadable
            "T-31", "T-30", // not started, ready.json's order
        ]
    );
    assert_eq!(card(&overview, "T-2").needs_you, Some(NeedsYou::Plan));
    assert_eq!(card(&overview, "T-8").needs_you, Some(NeedsYou::Review));
    assert_eq!(card(&overview, "T-8").pull_request, Some(9));
    assert_eq!(card(&overview, "T-7").needs_you, Some(NeedsYou::Parked));
    assert_eq!(card(&overview, "T-3").needs_you, None);
}

/// For you is built from every ref, the ones with no card included, and
/// agrees with the header's counts of the same refs.
#[test]
fn for_you_counts_every_ref_as_the_header_does() {
    let waits = waits_of(vec![
        row("T-1", Some(Stage::Build), RefStatus::NeedsInput),
        row("T-2", Some(Stage::Design), RefStatus::NeedsInput),
        active("T-3", Stage::Watch),
    ]);
    let mut details = BTreeMap::new();
    details.insert(id("T-1"), detail(RefStatus::NeedsInput));
    details.insert(
        id("T-2"),
        RefDetail {
            plan_to_approve: true,
            ..detail(RefStatus::NeedsInput)
        },
    );
    details.insert(
        id("T-3"),
        RefDetail {
            pull_request: Some(12),
            ..detail(RefStatus::Active)
        },
    );
    // Refs with no row: a question round at Intake that ready.json does not
    // list, and a ref parked at Watch whose delivery ended.
    details.insert(id("T-4"), detail(RefStatus::NeedsInput));
    details.insert(id("T-5"), detail(RefStatus::Parked));
    details.insert(id("T-6"), detail(RefStatus::Active));
    let overview = view(
        &waits,
        Aux {
            details: details.clone(),
            ready: None,
        },
    );
    let you = &overview.for_you;
    // The header (`why::standings`): needs-input refs, plans included,
    // followed pull requests, parked refs.
    let count = |keep: &dyn Fn(&RefDetail) -> bool| details.values().filter(|d| keep(d)).count();
    assert_eq!(
        you.questions.len() + you.plans.len(),
        count(&|d| d.status == RefStatus::NeedsInput)
    );
    assert_eq!(
        you.reviews.len(),
        count(&|d| d.status == RefStatus::Active && d.pull_request.is_some())
    );
    assert_eq!(you.parked.len(), count(&|d| d.status == RefStatus::Parked));
    let tickets = |entries: &[Entry]| {
        entries
            .iter()
            .map(|entry| entry.ticket.to_string())
            .collect::<Vec<_>>()
    };
    assert_eq!(tickets(&you.questions), ["T-1", "T-4"]);
    assert_eq!(tickets(&you.plans), ["T-2"]);
    assert_eq!(you.reviews[0].pull_request, Some(12));
    assert_eq!(tickets(&you.parked), ["T-5"]);
    assert!(
        you.questions[0].card && !you.questions[1].card,
        "T-4 has no card"
    );
    assert!(!you.parked[0].card);
}

/// A slot holder carries its slot, its time and the role of its latest run
/// in its own project; a card with no slot carries none.
#[test]
fn a_running_card_names_the_role_of_its_latest_run() {
    let waits = waits_of(vec![
        holding(active("T-1", Stage::Build), 2),
        active("T-2", Stage::Build),
    ]);
    let mut runs = BTreeMap::new();
    runs.insert(
        ("o/r".to_owned(), id("T-1")),
        Run {
            role: "build".to_owned(),
            at: ts("2026-11-06T09:30:01Z"),
        },
    );
    runs.insert(
        ("other/repo".to_owned(), id("T-2")),
        Run {
            role: "verify".to_owned(),
            at: ts("2026-11-06T09:31:00Z"),
        },
    );
    let overview = build(
        &waits,
        &[Aux::default()],
        &BTreeMap::new(),
        &runs,
        ts("2026-11-06T10:00:00Z"),
        0,
        Vec::new(),
    );
    let running = card(&overview, "T-1").running.clone().unwrap();
    assert_eq!((running.slot, running.role.as_deref()), (2, Some("build")));
    assert_eq!(card(&overview, "T-2").running, None);
}

/// The strip has eight cells in pipeline order.
#[test]
fn the_strip_has_eight_cells_with_the_stage_current() {
    let waits = waits_of(vec![
        active("T-1", Stage::Build),
        row("T-2", None, RefStatus::Unread),
    ]);
    let overview = view(&waits, Aux::default());
    let strip = &card(&overview, "T-1").strip;
    let shown: Vec<_> = strip.iter().map(|cell| cell.name()).collect();
    assert_eq!(
        shown,
        [
            "done", "done", "done", "done", "current", "todo", "todo", "todo"
        ]
    );
    assert!(
        card(&overview, "T-2")
            .strip
            .iter()
            .all(|cell| *cell == Cell::Todo)
    );
}

/// A snapshot older than two intervals is stale; a title with markup is
/// text, and is escaped on the wire.
#[test]
fn a_stale_snapshot_is_marked_and_a_title_with_markup_is_escaped() {
    let mut old = snapshot(vec![listed("T-1", Outcome::TakenIn)]);
    old.tickets[0].title = "<script>alert(1)</script>".to_owned();
    let waits = waits_of(Vec::new());
    let overview = build(
        &waits,
        &[Aux {
            details: BTreeMap::new(),
            ready: Some(old),
        }],
        &BTreeMap::new(),
        &BTreeMap::new(),
        ts("2026-11-06T10:05:00Z"),
        3,
        vec!["a note".to_owned()],
    );
    assert!(overview.projects[0].ready.as_ref().unwrap().stale);
    assert_eq!(overview.highlights, 3);
    assert_eq!(overview.notes, ["a note"]);
    let json = overview.json();
    assert_eq!(
        json["projects"][0]["cards"][0]["title"],
        "<script>alert(1)</script>"
    );
    let wire = crate::ui::server::json_text(&json);
    assert!(!wire.contains('<') && !wire.contains('>'), "{wire}");
    assert_eq!(json["projects"][0]["ready"]["stale"], true);
    let fresh = build(
        &waits,
        &[Aux {
            details: BTreeMap::new(),
            ready: Some(snapshot(Vec::new())),
        }],
        &BTreeMap::new(),
        &BTreeMap::new(),
        ts("2026-11-06T10:01:00Z"),
        0,
        Vec::new(),
    );
    assert!(!fresh.projects[0].ready.as_ref().unwrap().stale);
}
