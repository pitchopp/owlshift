//! What each ticket in flight waits on (OWL-459; architecture section 5,
//! "`owlshift status`"): `owlshift status --waits` read from a drain played
//! on the bench of `on_demand.rs`, its refs and its event log, the way the
//! command reads them.

use owlshift_contracts::refs::{Ended, EndedWhy};
use owlshift_contracts::service::{KeptFile, ServedConfig};
use owlshift_contracts::waits::{Behind, RefStatus, StatusWaits, WaitResource};
use owlshift_runner::status::waits;
use owlshift_runner::voice::Voice;
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
    let built = waits::build(&read, Voice::Terminal);
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

/// What the web UI's graph reads of the bench through the server's own
/// read, `tail` kept from one call to the next, with the waits document read
/// at the same time, in the UI's voice: the `--json` words of `status
/// --waits`, except that a parked wait names Restart.
fn graph_and_document(
    bench: &Bench,
    tail: &mut owlshift_runner::ui::eventtail::EventTail,
) -> (Value, StatusWaits) {
    let now = bench.clock.get();
    let graph = owlshift_runner::ui::server::graph_read(&bench.data, tail, 2, now);
    let document = waits::build(&waits::gather(&bench.data, None, 2, now), Voice::Ui);
    (serde_json::to_value(graph).unwrap(), document)
}

/// What labels the edge of a wait: its resource, else its reason.
fn label_of(wait: &owlshift_contracts::waits::Wait) -> String {
    match &wait.resource {
        Some(WaitResource::Zone { path }) if path.is_empty() => "whole repository".to_owned(),
        Some(WaitResource::Zone { path }) => path.clone(),
        Some(WaitResource::Named { name }) => name.clone(),
        Some(WaitResource::Browser) => "browser".to_owned(),
        None => wait.reason.clone(),
    }
}

/// The graph's live layer against the document, node for node and edge for
/// edge: its nodes are the document's tickets plus what a wait names behind
/// a ticket that is none, its edges one per `behind` of every wait that is
/// not a place in a queue, and its queue the tickets that wait for a slot.
fn assert_matches(graph: &Value, document: &StatusWaits) {
    assert_eq!(graph["projects"].as_array().unwrap().len(), 1, "{graph}");
    let project = &document.projects[0];
    let mut nodes: Vec<String> = project
        .tickets
        .iter()
        .map(|row| row.ticket.as_str().to_owned())
        .collect();
    let mut edges: Vec<(String, String, String)> = Vec::new();
    let mut queue: Vec<(Option<Timestamp>, String)> = Vec::new();
    for row in &project.tickets {
        let Some(wait) = &row.waits else { continue };
        if wait.reason == "slot" {
            queue.push((wait.since, row.ticket.as_str().to_owned()));
        }
        if matches!(wait.reason.as_str(), "slot" | "run_cap") {
            continue;
        }
        for behind in &wait.behind {
            let name = match behind {
                Behind::Ticket { id, .. } => id.as_str().to_owned(),
                Behind::PullRequest { number } => format!("pull request #{number}"),
            };
            if !nodes.contains(&name) {
                nodes.push(name.clone());
            }
            edges.push((row.ticket.as_str().to_owned(), name, label_of(wait)));
        }
    }
    let live = &graph["projects"][0]["live"];
    let mut got_nodes: Vec<String> = live["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|node| node["id"].as_str().unwrap().to_owned())
        .collect();
    let mut got_edges: Vec<(String, String, String)> = live["edges"]
        .as_array()
        .unwrap()
        .iter()
        .map(|edge| {
            let text = |key: &str| edge[key].as_str().unwrap().to_owned();
            (text("from"), text("to"), text("label"))
        })
        .collect();
    nodes.sort();
    got_nodes.sort();
    edges.sort();
    got_edges.sort();
    assert_eq!(got_nodes, nodes, "{graph}");
    assert_eq!(got_edges, edges, "{graph}");
    queue.sort();
    let got_queue: Vec<&str> = graph["projects"][0]["queue"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|entry| entry["reason"] == "slot")
        .map(|entry| entry["ticket"].as_str().unwrap())
        .collect();
    let want: Vec<&str> = queue.iter().map(|(_, ticket)| ticket.as_str()).collect();
    assert_eq!(got_queue, want, "{graph}");
}

/// OWL-460: the web UI's graph of the chain drain, read through the server's
/// own read with one event tail kept across the passes, is the document of
/// `status --waits --json` node for node and edge for edge; the pass that
/// completes DEMO-9 takes its node and its edge away at the next read; the
/// blockers layer draws DEMO-9, which the column does not list, as a dashed
/// ghost, and so a blocker written into the snapshot as canceled.
#[test]
fn the_graph_of_a_chain_matches_the_document_and_a_cleared_wait_leaves_it() {
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
    let mut tail = owlshift_runner::ui::eventtail::EventTail::new(&bench.data);
    let chained = RefCell::new(None);
    let (_, lines, printed) = bench.drain_passes(replies, 4, |pass, _| {
        if pass == 3 {
            // The server's read counts a project served when `projects.json`
            // lists it, which `served_on` does not do.
            let repo = Repo::parse(REPO).unwrap();
            served::Registry::in_data_dir(&bench.data)
                .register(&repo, &bench.remote.checkout)
                .unwrap();
            chained.replace(Some(graph_and_document(&bench, &mut tail)));
            bench.put_ticket("DEMO-9", "maintainer", "Done", &[], "", "A blocker.");
        }
    });
    let report = format!("{lines}\n{printed}");
    let (graph, document) = chained.into_inner().expect("read after the intakes");
    assert_eq!(document.projects.len(), 1, "the project is served");
    assert_matches(&graph, &document);
    let live = &graph["projects"][0]["live"];
    let ids = |layer: &Value| -> Vec<String> {
        let mut ids: Vec<String> = layer["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|node| node["id"].as_str().unwrap().to_owned())
            .collect();
        ids.sort();
        ids
    };
    assert_eq!(
        ids(live),
        ["DEMO-1", "DEMO-2", "DEMO-3", "DEMO-9"],
        "{graph}"
    );
    let edge = |layer: &Value, from: &str, to: &str| {
        layer["edges"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["from"] == from && e["to"] == to)
    };
    assert!(edge(live, "DEMO-1", "DEMO-9") && edge(live, "DEMO-2", "DEMO-1"));
    // The blockers layer: DEMO-9 has no `agent` label, so the bench's ready
    // column does not list it, and it is a dashed ghost, open, with both
    // edges that name it; the tickets of the column are nodes of their own.
    let blockers = &graph["projects"][0]["blockers"];
    let demo9 = blockers["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|node| node["id"] == "DEMO-9")
        .unwrap_or_else(|| panic!("DEMO-9 missing in {graph}"));
    assert_eq!(demo9["state"], "ghost", "{demo9}");
    assert_eq!(demo9["blocker_state"], "open", "{demo9}");
    assert!(edge(blockers, "DEMO-1", "DEMO-9") && edge(blockers, "DEMO-3", "DEMO-9"));
    assert_eq!(
        ids(blockers),
        ["DEMO-1", "DEMO-2", "DEMO-3", "DEMO-9"],
        "{graph}"
    );

    // The pass that completes DEMO-9: the same tail, and neither its node
    // nor its edge.
    let (graph, document) = graph_and_document(&bench, &mut tail);
    assert_matches(&graph, &document);
    let live = &graph["projects"][0]["live"];
    assert!(
        !ids(live).contains(&"DEMO-9".to_owned()),
        "{graph}\n{report}"
    );
    assert!(!edge(live, "DEMO-1", "DEMO-9"), "{graph}");
    assert!(edge(live, "DEMO-2", "DEMO-1"), "{graph}");
    let queue: Vec<&str> = graph["projects"][0]["queue"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["ticket"].as_str().unwrap())
        .collect();
    assert_eq!(queue, ["DEMO-3"], "{graph}");
    assert!(
        !edge(live, "DEMO-3", "DEMO-1"),
        "a queued ticket has no edge"
    );

    // A blocker outside the column, which the bench cannot record as a
    // relation: written into the snapshot as the drain would.
    let mut snapshot = ready::read(&bench.dirs()).unwrap().expect("a snapshot");
    let waiting = snapshot
        .tickets
        .iter_mut()
        .find(|ticket| ticket.ticket.as_str() == "DEMO-3")
        .expect("DEMO-3 is in the column");
    for (name, state) in [
        ("DEMO-20", owlshift_contracts::ready::BlockerState::Open),
        ("DEMO-21", owlshift_contracts::ready::BlockerState::Canceled),
    ] {
        waiting
            .blockers
            .push(owlshift_contracts::ready::ReadyBlocker {
                ticket: name.to_owned(),
                state,
                merged: false,
            });
    }
    ready::write(&bench.dirs(), &snapshot).unwrap();
    let (graph, _) = graph_and_document(&bench, &mut tail);
    let blockers = &graph["projects"][0]["blockers"];
    for (name, state) in [("DEMO-20", "open"), ("DEMO-21", "canceled")] {
        let ghost = blockers["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|node| node["id"] == name)
            .unwrap_or_else(|| panic!("{name} missing in {graph}"));
        assert_eq!(ghost["state"], "ghost");
        assert_eq!(ghost["blocker_state"], state);
        assert!(edge(blockers, "DEMO-3", name));
    }
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

/// What `owlshift why [ticket]` prints of the bench at its clock.
fn why_of(bench: &Bench, ticket: Option<&str>) -> String {
    why_at(bench, ticket, bench.clock.get())
}

/// The same at the time `now`.
fn why_at(bench: &Bench, ticket: Option<&str>, now: Timestamp) -> String {
    let report = why::gather(
        &bench.data,
        &bench.dirs(),
        REPO,
        ticket.map(|t| TicketId::new(t).unwrap()),
        2,
        &[],
        now,
    )
    .unwrap();
    why::render(&report)
}

/// OWL-484: the same ticket as OWL-477's, DEMO-2, told it waits behind
/// DEMO-1, then done outside Owlshift. While the drain lists it, `why` says
/// its wait is current; once a pass wrote a snapshot without it, `why
/// DEMO-2` keeps the wait as history but says it is not shown as waiting,
/// and `why` alone leaves it out with a count while DEMO-1 keeps its own
/// wait. With the snapshot gone, the ticket out of flight is left out as
/// well, said otherwise.
#[test]
fn why_does_not_repeat_the_wait_of_a_ticket_the_drain_no_longer_reads() {
    let bench = parallel_bench("max_in_flight = 1\n");
    bench.put_ticket(
        "DEMO-2",
        "maintainer",
        "Todo",
        &["agent"],
        "blocked_by = [\"DEMO-1\"]\n",
        "Document it.",
    );
    let replies: Vec<Reply> = ["app", "docs"]
        .iter()
        .map(|zone| bench.reply(None, Some(&standard_declaring(&[zone]))))
        .collect();
    let told = RefCell::new(None);
    bench.drain_passes(replies, 4, |pass, _| {
        if pass == 3 {
            told.replace(Some(why_of(&bench, Some("DEMO-2"))));
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
    let before = told.into_inner().expect("read after the intakes");
    assert!(before.contains("now: blocked by DEMO-1"), "{before}");

    let one = why_of(&bench, Some("DEMO-2"));
    assert!(
        one.contains(" waits: blocked by DEMO-1"),
        "history kept: {one}"
    );
    assert!(
        one.contains("now: not shown as waiting: it is neither in flight nor listed in the drain's latest ready snapshot"),
        "{one}"
    );
    assert!(!one.contains("now: blocked"), "{one}");
    assert!(!one.contains("note:"), "{one}");

    let all = why_of(&bench, None);
    assert!(
        !all.contains("DEMO-2 [") && !all.contains("  DEMO-2 last"),
        "{all}"
    );
    assert!(all.contains("  DEMO-1 [ready] last told "), "{all}");
    assert!(
        all.contains("1 ticket(s) with a told wait are left out"),
        "{all}"
    );

    // A stale snapshot is used all the same, with a note.
    let later = bench.clock.get() + std::time::Duration::from_secs(30 * 24 * 3600);
    let stale = why_at(&bench, None, later);
    assert!(
        stale.contains("note: the drain last read the ready column at "),
        "{stale}"
    );
    assert!(stale.contains("  DEMO-1 [ready] last told "), "{stale}");

    fs::remove_file(ready::file(&bench.dirs())).unwrap();
    let none = why_of(&bench, None);
    assert!(!none.contains("no admitted ticket waits"), "{none}");
    assert!(
        none.contains("left out: they are not in flight and no ready snapshot reads"),
        "{none}"
    );
    let gone = why_of(&bench, Some("DEMO-2"));
    assert!(gone.contains("no ready snapshot reads"), "{gone}");
    assert!(!gone.contains("now: blocked"), "{gone}");
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
