//! `owlshift do` on the first ticket `owlshift init --tracker markdown`
//! writes (OWL-34): the example's text, unchanged, is a ticket the run
//! takes in. The bench adds no assignee or label of its own.

use owlshift_runner::init::{EXAMPLE_ASSIGNEE, example_ticket};

use super::*;

#[test]
fn do_takes_in_the_ticket_init_wrote() {
    let (folder, text) = example_ticket();
    assert_eq!(folder, TICKET);
    // The bench's own ticket has no assignee; the example brings its own.
    let bench = Bench::new(false);
    fs::write(bench.ticket_file(), &text).unwrap();

    let (outcome, printed) = bench.run(vec![bench.reply(Some("Hello"), Some(DONE))], None);
    outcome.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));

    let events = bench.events();
    assert!(
        events.iter().any(|e| e.kind == EventKind::RunStarted),
        "{events:?}"
    );
    assert!(
        events
            .iter()
            .all(|e| e.ticket.as_ref().is_some_and(|t| t.as_str() == folder))
    );
    let briefs = bench.briefs();
    let brief = briefs.first().expect("a run started");
    assert_eq!(brief.ticket.id.as_str(), folder);
    assert_eq!(brief.ticket.title, "Try Owlshift on a first ticket");
    assert_eq!(brief.ticket.labels, ["agent"]);
    assert_eq!(brief.decider, EXAMPLE_ASSIGNEE);
    assert!(
        brief
            .ticket
            .description
            .contains(&format!("`owlshift do {folder}`")),
        "{}",
        brief.ticket.description
    );
}

/// Without its assignee the example has no decider: `do` refuses it, which
/// is why the example names one.
#[test]
fn do_refuses_the_example_without_its_assignee() {
    let (_, text) = example_ticket();
    let without: String = text
        .lines()
        .filter(|line| !line.starts_with("assignee = "))
        .map(|line| format!("{line}\n"))
        .collect();
    assert_ne!(without, text);
    let bench = Bench::new(false);
    fs::write(bench.ticket_file(), without).unwrap();
    let (outcome, _) = bench.run(Vec::new(), None);
    assert!(
        matches!(&outcome, Err(Stop::Refused(reason)) if reason.contains("DEMO-1 has no decider")),
        "{outcome:?}"
    );
}
