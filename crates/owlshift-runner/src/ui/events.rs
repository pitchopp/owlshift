//! The event log as the UI reads it (P8 part 6, OWL-471, and part 5,
//! OWL-460): one incremental reader of `<data>/events.jsonl`, which the
//! ticket page's snapshot asks every few seconds while it is open, the run
//! log's route asks before each read of a file, and the drain's graph asks
//! at each refresh while a graph stream is open.
//!
//! The log is shared by every project of the machine and grows without end,
//! so the reader is an [`EventTail`]: it keeps its byte offset and reads only
//! what was appended since, never a half-written last line (it waits for the
//! newline, as `events::print` does), and starts again from 0 when the file
//! shrank or is another file. It keeps every event, since the graph's waits
//! fold them all (`why::fold`); the ticket page's questions are answered by
//! filtering them: the rows it draws are those of a ticket of the kinds
//! `run_started`, `run_ended`, `usage`, `warning`, `decision` and `dispatch`.
//! A line that does not parse is counted, never fatal.

use std::path::Path;
use std::sync::Mutex;

use owlshift_contracts::event::{Event, EventKind};
use owlshift_contracts::ids::TicketId;

use crate::events::EventTail;

/// The kinds of event the ticket page draws.
const KEPT: [EventKind; 6] = [
    EventKind::RunStarted,
    EventKind::RunEnded,
    EventKind::Usage,
    EventKind::Warning,
    EventKind::Decision,
    EventKind::Dispatch,
];

/// The incremental reader of a data directory's event log.
pub struct Reader {
    tail: Mutex<EventTail>,
}

/// What the reader holds of a ticket.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Rows {
    /// Its rows, in the order the log holds them.
    pub events: Vec<Event>,
    /// How many complete lines of the whole log did not parse.
    pub unparsable: u64,
}

impl Reader {
    /// The reader of the log of the data directory `data`; nothing is read
    /// before the first question.
    pub fn new(data: &Path) -> Self {
        Self {
            tail: Mutex::new(EventTail::in_dir(data)),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, EventTail> {
        self.tail
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Reads what was appended since the last read, within `tail`.
    fn refresh(tail: &mut EventTail) -> Result<(), String> {
        tail.refresh()
            .map_err(|error| format!("{}: {error}", tail.path().display()))
    }

    /// The rows of `ticket` of `project` (as the events spell it, in any
    /// ASCII case) after a read of what was appended.
    pub fn rows(&self, project: &str, ticket: &TicketId) -> Result<Rows, String> {
        let mut tail = self.lock();
        Self::refresh(&mut tail)?;
        Ok(Rows {
            events: tail
                .events()
                .iter()
                .filter(|event| {
                    KEPT.contains(&event.kind)
                        && event.project.eq_ignore_ascii_case(project)
                        && event.ticket.as_ref() == Some(ticket)
                })
                .cloned()
                .collect(),
            unparsable: tail.notes().len() as u64,
        })
    }

    /// Whether a `run_ended` event names `run` of `ticket` of `project`.
    /// A log that cannot be read says no: a run is then served as live for
    /// a poll more, never cut short.
    pub fn ended(&self, project: &str, ticket: &TicketId, run: &str) -> bool {
        let mut tail = self.lock();
        if Self::refresh(&mut tail).is_err() {
            return false;
        }
        tail.events().iter().any(|event| {
            event.kind == EventKind::RunEnded
                && event.run.as_deref() == Some(run)
                && event.project.eq_ignore_ascii_case(project)
                && event.ticket.as_ref() == Some(ticket)
        })
    }

    /// Runs `read` on the tail itself, the log's lock held: the drain's
    /// graph reads every event through it, from the offset the ticket page
    /// had kept, never a second copy of the log.
    pub fn with_tail<T>(&self, read: impl FnOnce(&mut EventTail) -> T) -> T {
        read(&mut self.lock())
    }

    /// The offset of the first byte not read.
    pub fn offset(&self) -> u64 {
        self.lock().offset()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{Data, data};
    use owlshift_contracts::format::Format;
    use serde_json::json;
    use std::io::Write;

    fn event(project: &str, ticket: &str, run: &str, kind: EventKind, details: Data) -> Event {
        Event {
            format: Format,
            at: "2026-11-05T10:00:00Z".parse().unwrap(),
            project: project.to_owned(),
            ticket: Some(TicketId::new(ticket).unwrap()),
            run: Some(run.to_owned()),
            kind,
            data: details,
        }
    }

    fn append(path: &Path, text: &str) {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        file.write_all(text.as_bytes()).unwrap();
    }

    fn line(event: &Event) -> String {
        format!("{}\n", event.render_line())
    }

    const RUN: &str = "20261105T100000Z";

    #[test]
    fn an_appended_run_end_is_picked_up_without_rereading() {
        let dir = tempfile::tempdir().unwrap();
        let reader = Reader::new(dir.path());
        let log = dir.path().join("events.jsonl");
        let ticket = TicketId::new("OWL-1").unwrap();
        assert!(!reader.ended("o/r", &ticket, RUN), "no log yet");
        let started = line(&event(
            "o/r",
            "OWL-1",
            RUN,
            EventKind::RunStarted,
            Data::new(),
        ));
        append(&log, &started);
        assert!(!reader.ended("o/r", &ticket, RUN));
        assert_eq!(reader.offset(), started.len() as u64);
        let ended = line(&event(
            "o/r",
            "OWL-1",
            RUN,
            EventKind::RunEnded,
            data([("outcome", json!("finished"))]),
        ));
        append(&log, &ended);
        assert!(reader.ended("o/r", &ticket, RUN));
        assert_eq!(reader.offset(), (started.len() + ended.len()) as u64);
        let rows = reader.rows("O/R", &ticket).unwrap();
        assert_eq!(rows.events.len(), 2);
        assert_eq!(rows.events[1].data["outcome"], "finished");
        // Reading again moves nothing.
        assert!(reader.ended("o/r", &ticket, RUN));
        assert_eq!(reader.offset(), (started.len() + ended.len()) as u64);
    }

    #[test]
    fn a_half_written_line_is_read_once_complete() {
        let dir = tempfile::tempdir().unwrap();
        let reader = Reader::new(dir.path());
        let log = dir.path().join("events.jsonl");
        let ticket = TicketId::new("OWL-1").unwrap();
        let whole = line(&event(
            "o/r",
            "OWL-1",
            RUN,
            EventKind::RunEnded,
            Data::new(),
        ));
        let (head, tail) = whole.split_at(whole.len() / 2);
        append(&log, head);
        assert!(!reader.ended("o/r", &ticket, RUN));
        assert_eq!(reader.offset(), 0);
        assert_eq!(reader.rows("o/r", &ticket).unwrap().unparsable, 0);
        append(&log, tail);
        assert!(reader.ended("o/r", &ticket, RUN));
        assert_eq!(reader.offset(), whole.len() as u64);
    }

    #[test]
    fn a_shrunk_log_is_read_again_from_zero() {
        let dir = tempfile::tempdir().unwrap();
        let reader = Reader::new(dir.path());
        let log = dir.path().join("events.jsonl");
        let ticket = TicketId::new("OWL-1").unwrap();
        let first = line(&event(
            "o/r",
            "OWL-1",
            RUN,
            EventKind::RunEnded,
            Data::new(),
        ));
        append(&log, &first);
        append(&log, &first);
        assert!(reader.ended("o/r", &ticket, RUN));
        std::fs::write(
            &log,
            line(&event(
                "o/r",
                "OWL-1",
                "other",
                EventKind::RunStarted,
                Data::new(),
            )),
        )
        .unwrap();
        assert!(!reader.ended("o/r", &ticket, RUN));
        let rows = reader.rows("o/r", &ticket).unwrap();
        assert_eq!(rows.events.len(), 1);
        assert_eq!(rows.events[0].run.as_deref(), Some("other"));
    }

    #[test]
    fn another_ticket_or_project_is_not_returned_and_a_bad_line_is_counted() {
        let dir = tempfile::tempdir().unwrap();
        let reader = Reader::new(dir.path());
        let log = dir.path().join("events.jsonl");
        append(
            &log,
            &line(&event(
                "o/r",
                "OWL-2",
                RUN,
                EventKind::RunEnded,
                Data::new(),
            )),
        );
        append(
            &log,
            &line(&event(
                "o/other",
                "OWL-1",
                RUN,
                EventKind::RunEnded,
                Data::new(),
            )),
        );
        append(
            &log,
            &line(&event("o/r", "OWL-1", RUN, EventKind::Scan, Data::new())),
        );
        append(&log, "not an event\n");
        append(
            &log,
            &line(&event("o/r", "OWL-1", RUN, EventKind::Usage, Data::new())),
        );
        let ticket = TicketId::new("OWL-1").unwrap();
        assert!(!reader.ended("o/r", &ticket, RUN));
        let rows = reader.rows("o/r", &ticket).unwrap();
        assert_eq!(rows.events.len(), 1);
        assert_eq!(rows.events[0].kind, EventKind::Usage);
        assert_eq!(rows.unparsable, 1);
    }

    #[test]
    fn the_graph_reads_every_event_through_the_same_offset() {
        let dir = tempfile::tempdir().unwrap();
        let reader = Reader::new(dir.path());
        let log = dir.path().join("events.jsonl");
        let ticket = TicketId::new("OWL-1").unwrap();
        let scan = line(&event("o/r", "OWL-1", RUN, EventKind::Scan, Data::new()));
        let usage = line(&event("o/r", "OWL-1", RUN, EventKind::Usage, Data::new()));
        append(&log, &scan);
        append(&log, &usage);
        // The ticket page reads first: the graph's read starts from there,
        // and holds the event the page's rows leave out.
        assert_eq!(reader.rows("o/r", &ticket).unwrap().events.len(), 1);
        let total = (scan.len() + usage.len()) as u64;
        assert_eq!(reader.offset(), total);
        let kinds = reader.with_tail(|tail| {
            assert_eq!(tail.offset(), total);
            tail.events().iter().map(|e| e.kind).collect::<Vec<_>>()
        });
        assert_eq!(kinds, [EventKind::Scan, EventKind::Usage]);
        // What the graph's read takes in, the page's does not read again.
        let ended = line(&event(
            "o/r",
            "OWL-1",
            RUN,
            EventKind::RunEnded,
            Data::new(),
        ));
        append(&log, &ended);
        reader.with_tail(|tail| tail.refresh().unwrap());
        assert_eq!(reader.offset(), total + ended.len() as u64);
        assert!(reader.ended("o/r", &ticket, RUN));
    }
}
