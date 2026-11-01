//! The drain's ready snapshot (OWL-468, architecture section 5, "The local
//! web UI", "The ready snapshot"): `<project data>/ready.json`, written by
//! `owlshift watch --drain` at each pass that read the ready column
//! (`watch::drain`), replaced whole. Only the project's draining watch
//! writes it, which the watch lock keeps to one, so no other lock is taken.

use std::path::PathBuf;

use jiff::Timestamp;
use owlshift_contracts::ids::TicketId;
use owlshift_contracts::ready::ReadySnapshot;
use owlshift_contracts::waits::Wait;

use crate::events::Data;
use crate::project::ProjectDirs;
use crate::step::write_atomically;
use crate::why::Waiting;

/// The snapshot's file name in the project's folder.
pub const FILE: &str = "ready.json";

/// The project's ready snapshot.
pub fn file(dirs: &ProjectDirs) -> PathBuf {
    dirs.root().join(FILE)
}

/// The project's snapshot, `None` when there is none; the error says why
/// it does not read.
pub fn read(dirs: &ProjectDirs) -> Result<Option<ReadySnapshot>, String> {
    let path = file(dirs);
    match std::fs::read_to_string(&path) {
        Ok(text) => ReadySnapshot::parse(&text)
            .map(Some)
            .map_err(|e| format!("{}: {e}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("{}: {error}", path.display())),
    }
}

/// Whether `snapshot` is stale at `now`: older than two of its watch's
/// intervals (architecture section 5, "The ready snapshot").
pub fn stale(snapshot: &ReadySnapshot, now: Timestamp) -> bool {
    let age = now
        .as_millisecond()
        .saturating_sub(snapshot.at.as_millisecond());
    u64::try_from(age).is_ok_and(|age| age > snapshot.interval_ms.saturating_mul(2))
}

/// Replaces the project's snapshot with `snapshot`.
pub fn write(dirs: &ProjectDirs, snapshot: &ReadySnapshot) -> Result<(), String> {
    let path = file(dirs);
    write_atomically(&path, &snapshot.render()).map_err(|e| format!("{}: {e}", path.display()))
}

/// A wait of `ticket` for `reason`, `data` being what the drain's event
/// for it carries, in the shape and the words `owlshift status --waits`
/// gives it, with no `since`.
pub(crate) fn wait(ticket: &TicketId, reason: &str, data: Data) -> Wait {
    let mut wait = crate::status::waits::told(&Waiting {
        at: Timestamp::UNIX_EPOCH,
        ticket: ticket.clone(),
        reason: reason.to_owned(),
        data,
    });
    wait.since = None;
    wait
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::data;
    use owlshift_contracts::waits::Behind;
    use serde_json::json;

    #[test]
    fn a_snapshot_older_than_two_intervals_is_stale() {
        let snapshot = ReadySnapshot {
            format: Default::default(),
            at: "2026-10-31T10:00:00Z".parse().unwrap(),
            interval_ms: 60_000,
            tickets: Vec::new(),
        };
        let at = |time: &str| stale(&snapshot, time.parse().unwrap());
        assert!(!at("2026-10-31T10:02:00Z"));
        assert!(at("2026-10-31T10:02:01Z"));
        assert!(!at("2026-10-31T09:00:00Z"), "a clock behind is not stale");
    }

    #[test]
    fn a_wait_names_its_tickets_and_the_new_reasons_are_worded() {
        let ticket = TicketId::new("DEMO-2").unwrap();
        let blocked = wait(
            &ticket,
            "blocked_by",
            data([("tickets", json!(["DEMO-1", "not an id"]))]),
        );
        assert_eq!(blocked.since, None);
        assert_eq!(blocked.text, "blocked by DEMO-1, not an id");
        assert_eq!(
            blocked.behind,
            [Behind::Ticket {
                id: TicketId::new("DEMO-1").unwrap(),
                person: false,
            }]
        );
        for reason in ["held", "next_pass", "ref_unread", "refs_unread"] {
            let text = wait(&ticket, reason, Data::new()).text;
            assert_ne!(
                text,
                reason.replace('_', " "),
                "{reason} has no words of its own"
            );
        }
    }
}
