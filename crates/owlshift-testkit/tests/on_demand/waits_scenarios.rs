//! What each ticket in flight waits on (OWL-459; architecture section 5,
//! "`owlshift status`"): `owlshift status --waits` read from a drain played
//! on the bench of `on_demand.rs`, its refs and its event log, the way the
//! command reads them.

use owlshift_contracts::refs::{Ended, EndedWhy};
use owlshift_contracts::service::{KeptFile, ServedConfig};
use owlshift_contracts::waits::{Behind, RefStatus, StatusWaits, WaitResource};
use owlshift_runner::status::waits;
use owlshift_runner::{ready, served, why};

use super::parallel_scenarios::{
    building, parallel_bench, planned, standard_declaring, two_planned,
};
use super::*;

/// What `owlshift status --waits demo/project` reads of the bench at its
/// clock, as the document and as text.
fn waits_of(bench: &Bench) -> (StatusWaits, String) {
    let repo = Repo::parse(REPO).unwrap();
    let read = waits::gather(&bench.data, Some(&repo), 2, bench.clock.get());
    let built = waits::build(&read);
    let text = waits::render(&built, &TimeZone::UTC);
    (built, text)
}

/// The bench's project file kept as the one its project is served on, so
/// its capacity is known.
fn served_on(bench: &Bench) {
    let mut record = ServedConfig::new(REPO);
    record.kept = Some(KeptFile {
        branch: "main".to_owned(),
        commit: "a".repeat(40),
        text: fs::read_to_string(bench.remote.checkout.join("owlshift.toml")).unwrap(),
        since: Timestamp::now(),
    });
    let file = served::record_file(&bench.dirs());
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(file, record.render()).unwrap();
}

/// The lines of `text` that start with `prefixes`, in that order, one after
/// the other.
fn follow(text: &str, prefixes: &[&str]) {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines
        .iter()
        .position(|line| line.starts_with(prefixes[0]))
        .unwrap_or_else(|| panic!("{:?} missing in\n{text}", prefixes[0]));
    for (n, prefix) in prefixes.iter().enumerate() {
        let line = lines.get(start + n).copied().unwrap_or_default();
        assert!(line.starts_with(prefix), "{prefix:?} expected in\n{text}");
    }
}

/// Under a capacity of one, DEMO-1 is blocked by DEMO-9, a ticket of the
/// tracker no gesture admits, DEMO-2 by DEMO-1 and DEMO-3 by DEMO-9: once
/// their intakes ran, the chain of two shows DEMO-1 under DEMO-9 and DEMO-2
/// under DEMO-1, with their edges. Once DEMO-9 is done, the next pass
/// dispatches DEMO-1, whose wait is gone, and DEMO-3 waits for the project's
/// capacity, counted against `max_in_flight`, while DEMO-2 still waits
/// behind DEMO-1.
#[test]
fn a_chain_of_blockers_then_the_queue_for_the_capacity() {
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
    served_on(&bench);
    let mut replies: Vec<Reply> = ["app", "docs", "lib"]
        .iter()
        .map(|zone| bench.reply(None, Some(&standard_declaring(&[zone]))))
        .collect();
    replies.push(bench.read(&planned()));
    let chained = RefCell::new(None);
    let (_, lines, printed) = bench.drain_passes(replies, 4, |pass, _| {
        if pass == 3 {
            chained.replace(Some(waits_of(&bench)));
            bench.put_ticket("DEMO-9", "maintainer", "Done", &[], "", "A blocker.");
        }
    });
    let report = format!("{lines}\n{printed}");
    let (built, text) = chained.into_inner().expect("read after the intakes");
    let project = &built.projects[0];
    assert!(
        !project.served && project.max_in_flight == Some(1),
        "{text}"
    );
    let behind = |ticket: &str| {
        project
            .tickets
            .iter()
            .find(|row| row.ticket.as_str() == ticket)
            .and_then(|row| row.waits.clone())
            .map(|wait| (wait.reason, wait.behind))
    };
    let on = |id: &str| {
        vec![Behind::Ticket {
            id: TicketId::new(id).unwrap(),
            person: false,
        }]
    };
    assert_eq!(
        behind("DEMO-1"),
        Some(("blocked_by".to_owned(), on("DEMO-9")))
    );
    assert_eq!(
        behind("DEMO-2"),
        Some(("blocked_by".to_owned(), on("DEMO-1")))
    );
    follow(
        &text,
        &[
            "  Holders, and what waits behind each:",
            "    DEMO-9, not in flight here",
            "      DEMO-1 [ready]: blocked by DEMO-9 (told ",
            "        DEMO-2 [ready]: blocked by DEMO-1 (told ",
        ],
    );

    let (built, text) = waits_of(&bench);
    let project = &built.projects[0];
    let demo1 = project
        .tickets
        .iter()
        .find(|row| row.ticket.as_str() == "DEMO-1")
        .unwrap();
    assert!(
        demo1.waits.is_none() && demo1.holds_capacity && demo1.ref_status == RefStatus::Active,
        "{text}\n{report}"
    );
    assert_eq!(project.in_flight, 1, "{text}");
    follow(
        &text,
        &[
            "demo/project: 1 of 1 in flight (`policy.max_in_flight`), not served",
            "  Holders, and what waits behind each:",
            "    DEMO-1 [design_review]",
            "      DEMO-2 [ready]: blocked by DEMO-1 (told ",
            "  Waiting for the project's capacity (1 of 1 in flight: DEMO-1):",
            "    DEMO-3 [ready]: the project's slot is held by DEMO-1 (told ",
        ],
    );
    assert!(!text.contains("DEMO-9"), "{text}");
}

/// Whether `ticket` is a row of the bench project's waits.
fn shown(built: &StatusWaits, ticket: &str) -> bool {
    built.projects[0]
        .tickets
        .iter()
        .any(|row| row.ticket.as_str() == ticket)
}

/// OWL-477: DEMO-2, a ready ticket told it waits behind DEMO-1, is done
/// outside Owlshift and leaves the ready column. The event log still tells
/// its wait, but once the drain's next pass wrote a snapshot without it,
/// `status --waits` no longer shows it, while DEMO-1, still in the column,
/// keeps its own wait.
#[test]
fn a_ready_ticket_that_left_the_column_is_not_shown() {
    let bench = parallel_bench("max_in_flight = 1\n");
    bench.put_ticket(
        "DEMO-2",
        "maintainer",
        "Todo",
        &["agent"],
        "blocked_by = [\"DEMO-1\"]\n",
        "Document it.",
    );
    served_on(&bench);
    let replies: Vec<Reply> = ["app", "docs"]
        .iter()
        .map(|zone| bench.reply(None, Some(&standard_declaring(&[zone]))))
        .collect();
    let told = RefCell::new(None);
    let (_, lines, printed) = bench.drain_passes(replies, 4, |pass, _| {
        if pass == 3 {
            told.replace(Some(waits_of(&bench)));
            bench.put_ticket(
                "DEMO-2",
                "maintainer",
                "Done",
                &["agent"],
                "blocked_by = [\"DEMO-1\"]\n",
                "Document it.",
            );
        }
    });
    let report = format!("{lines}\n{printed}");
    let (built, text) = told.into_inner().expect("read after the intakes");
    follow(
        &text,
        &[
            "  Holders, and what waits behind each:",
            "    DEMO-9, not in flight here",
            "      DEMO-1 [ready]: blocked by DEMO-9 (told ",
            "        DEMO-2 [ready]: blocked by DEMO-1 (told ",
        ],
    );
    assert!(shown(&built, "DEMO-2"), "{text}\n{report}");

    let (built, text) = waits_of(&bench);
    let snapshot = ready::read(&bench.dirs()).unwrap().expect("a snapshot");
    assert!(
        snapshot
            .tickets
            .iter()
            .all(|t| t.ticket.as_str() != "DEMO-2"),
        "{snapshot:?}"
    );
    let folded = why::fold(&bench.events(), REPO);
    assert!(
        folded
            .current
            .contains_key(&TicketId::new("DEMO-2").unwrap()),
        "the events still tell DEMO-2's wait"
    );
    assert!(!shown(&built, "DEMO-2"), "{text}\n{report}");
    assert!(!text.contains("DEMO-2"), "{text}");
    assert!(built.notes.is_empty(), "{:?}", built.notes);
    follow(
        &text,
        &[
            "  Holders, and what waits behind each:",
            "    DEMO-9, not in flight here",
            "      DEMO-1 [ready]: blocked by DEMO-9 (told ",
        ],
    );
}

/// Scenario S5's Build that waits on a file the one dispatched first holds
/// (OWL-448): `status --waits` names DEMO-1, delivered and followed at
/// Watch, as the holder and the zone, with the edge and the resource; the
/// project, named and not served, has no capacity known. Once DEMO-1's pull
/// request is merged, its ref no longer holds and it is no row (OWL-477):
/// DEMO-2's wait names it as not in flight.
#[test]
fn a_build_waiting_on_a_zone_names_its_holder_and_the_zone() {
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
    let (built, text) = waits_of(&bench);
    let project = &built.projects[0];
    assert!(!project.served && project.max_in_flight.is_none(), "{text}");
    let demo2 = project
        .tickets
        .iter()
        .find(|row| row.ticket.as_str() == "DEMO-2")
        .and_then(|row| row.waits.clone())
        .unwrap_or_else(|| panic!("DEMO-2 does not wait:\n{text}\n{report}"));
    assert_eq!(demo2.reason, "collision");
    assert_eq!(
        demo2.behind,
        [Behind::Ticket {
            id: TicketId::new("DEMO-1").unwrap(),
            person: false
        }]
    );
    assert_eq!(
        demo2.resource,
        Some(WaitResource::Zone {
            path: "app/one.rs".to_owned()
        })
    );
    follow(
        &text,
        &[
            "demo/project: 1 of ? (not known",
            "  Holders, and what waits behind each:",
            "    DEMO-1 [watch]",
            "      DEMO-2 [build]: its Build waits: DEMO-1 holds zone `app/one.rs`, which its \
             Build would take (told ",
        ],
    );

    let mut record = bench.record();
    let mut delivery = record.delivery.take().expect("DEMO-1 delivered");
    delivery.ended = Some(Ended {
        why: EndedWhy::Merged,
        at: bench.clock.get(),
        head: None,
    });
    record.delivery = Some(delivery);
    bench.write_record(&record);
    let (built, text) = waits_of(&bench);
    assert!(
        shown(&built, "DEMO-2") && !shown(&built, "DEMO-1"),
        "{text}"
    );
    assert!(!text.contains("DEMO-1 [watch]"), "{text}");
    follow(
        &text,
        &[
            "  Holders, and what waits behind each:",
            "    DEMO-1, not in flight here",
            "      DEMO-2 [build]: its Build waits: DEMO-1 holds zone ",
        ],
    );
}
