//! What a person's check of the web UI's overview in a browser changes in a
//! bench data directory (OWL-508): the project's ready snapshot and the
//! event log. `owlshift-ui-bench` and the scenario that seeds the bench share
//! it, so that what the test asserts is what the checklist tells.

use std::path::Path;

use jiff::{SignedDuration, Timestamp};
use owlshift_adapters::forge::Repo;
use owlshift_contracts::Priority;
use owlshift_contracts::event::{Event, EventKind};
use owlshift_contracts::format::Format;
use owlshift_contracts::ids::TicketId;
use owlshift_contracts::ready::{Outcome, ReadyTicket};
use owlshift_runner::events::EventLog;
use owlshift_runner::project::ProjectDirs;
use owlshift_runner::ready;
use serde_json::{Map, Value, json};

/// The bench project's repository.
pub const REPO: &str = "demo/project";

/// The ticket `touch-ready` adds to the ready column, and its title.
pub const ADDED: &str = "DEMO-8";
pub const ADDED_TITLE: &str = "Added while you watch";

/// The ticket whose park `append-event` records.
pub const PARKED_LIVE: &str = "DEMO-2";

/// The interval the snapshot claims: a day, so that it is not stale while a
/// person works through the checklist (the bench has no drain to rewrite
/// it).
pub const INTERVAL_MS: u64 = 24 * 3600 * 1000;

fn dirs(data: &Path) -> ProjectDirs {
    ProjectDirs::new(data, &Repo::parse(REPO).expect("the bench repository"))
}

/// Whether the data directory holds what the bench seeds: the project's
/// `ready.json` and the event log.
pub fn seeded(data: &Path) -> Result<(), String> {
    let snapshot = ready::file(&dirs(data));
    if !snapshot.is_file() {
        return Err(format!("{} is missing", snapshot.display()));
    }
    let log = EventLog::in_dir(data);
    if !log.path().is_file() {
        return Err(format!("{} is missing", log.path().display()));
    }
    Ok(())
}

/// Rewrites the project's snapshot as read at `now`, stale after a day, and
/// with `ADDED` in the column when `add` is set.
pub fn rewrite_ready(data: &Path, now: Timestamp, add: bool) -> Result<(), String> {
    let dirs = dirs(data);
    let mut snapshot = ready::read(&dirs)?.ok_or("the project has no ready.json")?;
    snapshot.at = now;
    snapshot.interval_ms = INTERVAL_MS;
    let added = TicketId::new(ADDED).expect("an identifier");
    if add && snapshot.tickets.iter().all(|t| t.ticket != added) {
        snapshot.tickets.push(ReadyTicket {
            ticket: added,
            title: ADDED_TITLE.to_owned(),
            priority: Priority::Unset,
            blockers: Vec::new(),
            drain: Outcome::Dispatched,
        });
    }
    ready::write(&dirs, &snapshot)
}

fn event(at: Timestamp, ticket: &str, kind: EventKind, data: Value) -> Event {
    let Value::Object(data): Value = data else {
        unreachable!("an object")
    };
    Event {
        format: Format,
        at,
        project: REPO.to_owned(),
        ticket: Some(TicketId::new(ticket).expect("an identifier")),
        run: None,
        kind,
        data: data.into_iter().collect::<Map<_, _>>(),
    }
}

/// A park of `ticket` at `at`, as the overview's "since" reads one: a
/// `decision` with a string `parked`.
pub fn parked_event(ticket: &str, at: Timestamp) -> Event {
    event(
        at,
        ticket,
        EventKind::Decision,
        json!({ "parked": "stopped" }),
    )
}

/// A question asked about `ticket` at `at`: a tracker write of kind
/// `QUESTIONS`.
pub fn asked_event(ticket: &str, at: Timestamp) -> Event {
    event(
        at,
        ticket,
        EventKind::TrackerWrite,
        json!({ "kind": "QUESTIONS" }),
    )
}

/// Appends `events` to the data directory's log.
pub fn append(data: &Path, events: &[Event]) -> Result<(), String> {
    let log = EventLog::in_dir(data);
    for event in events {
        log.append(event)
            .map_err(|error| format!("{}: {error}", log.path().display()))?;
    }
    Ok(())
}

/// What the seeding dates its two old events by: a park 30 hours ago, out
/// of the 24 hours a first visit shows, and a question 2 hours ago, in them.
pub fn seeded_events(now: Timestamp) -> Vec<Event> {
    vec![
        parked_event("DEMO-4", now - SignedDuration::from_hours(30)),
        asked_event("DEMO-6", now - SignedDuration::from_hours(2)),
    ]
}
