//! The event log as the ticket page reads it (P8 part 6, OWL-471): an
//! incremental reader of `<data>/events.jsonl`, which the page's snapshot
//! asks every few seconds while it is open, and the run log's route asks
//! before each read of a file.
//!
//! The log is shared by every project of the machine and grows without end,
//! so the reader keeps its byte offset and reads only what was appended
//! since, never a half-written last line (it waits for the newline, as
//! `events::print` does), and starts again from 0 when the file shrank. It
//! keeps only the rows the page draws, those of a ticket: `run_started`,
//! `run_ended`, `usage`, `warning`, `decision` and `dispatch`. A line that
//! does not parse is counted, never fatal.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use owlshift_contracts::event::{Event, EventKind};
use owlshift_contracts::ids::TicketId;

use crate::events::EventLog;

/// The kinds of event the reader keeps.
const KEPT: [EventKind; 6] = [
    EventKind::RunStarted,
    EventKind::RunEnded,
    EventKind::Usage,
    EventKind::Warning,
    EventKind::Decision,
    EventKind::Dispatch,
];

#[derive(Default)]
struct State {
    /// The offset of the first byte not read: a line start.
    offset: u64,
    rows: Vec<Event>,
    /// The complete lines read that were no event.
    unparsable: u64,
}

/// The incremental reader of a data directory's event log.
pub struct Reader {
    path: PathBuf,
    state: Mutex<State>,
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
            path: EventLog::in_dir(data).path().to_owned(),
            state: Mutex::new(State::default()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Reads what was appended since the last read, within `state`.
    fn refresh(&self, state: &mut State) -> Result<(), String> {
        let fail = |error: std::io::Error| format!("{}: {error}", self.path.display());
        let mut file = match File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                *state = State::default();
                return Ok(());
            }
            Err(error) => return Err(fail(error)),
        };
        let size = file.metadata().map_err(fail)?.len();
        if size < state.offset {
            *state = State::default();
        }
        if size == state.offset {
            return Ok(());
        }
        file.seek(SeekFrom::Start(state.offset)).map_err(fail)?;
        let mut appended = Vec::new();
        file.take(size - state.offset)
            .read_to_end(&mut appended)
            .map_err(fail)?;
        // Only whole lines: the rest waits for its newline.
        let whole = appended
            .iter()
            .rposition(|&b| b == b'\n')
            .map_or(0, |at| at + 1);
        for line in appended[..whole].split(|&b| b == b'\n') {
            let text = String::from_utf8_lossy(line);
            if text.trim().is_empty() {
                continue;
            }
            match Event::parse(&text) {
                Ok(event) => {
                    if event.ticket.is_some() && KEPT.contains(&event.kind) {
                        state.rows.push(event);
                    }
                }
                Err(_) => state.unparsable += 1,
            }
        }
        state.offset += whole as u64;
        Ok(())
    }

    /// The rows of `ticket` of `project` (as the events spell it, in any
    /// ASCII case) after a read of what was appended.
    pub fn rows(&self, project: &str, ticket: &TicketId) -> Result<Rows, String> {
        let mut state = self.lock();
        self.refresh(&mut state)?;
        Ok(Rows {
            events: state
                .rows
                .iter()
                .filter(|event| {
                    event.project.eq_ignore_ascii_case(project)
                        && event.ticket.as_ref() == Some(ticket)
                })
                .cloned()
                .collect(),
            unparsable: state.unparsable,
        })
    }

    /// Whether a `run_ended` event names `run` of `ticket` of `project`.
    /// A log that cannot be read says no: a run is then served as live for
    /// a poll more, never cut short.
    pub fn ended(&self, project: &str, ticket: &TicketId, run: &str) -> bool {
        let mut state = self.lock();
        if self.refresh(&mut state).is_err() {
            return false;
        }
        state.rows.iter().any(|event| {
            event.kind == EventKind::RunEnded
                && event.run.as_deref() == Some(run)
                && event.project.eq_ignore_ascii_case(project)
                && event.ticket.as_ref() == Some(ticket)
        })
    }

    /// The offset of the first byte not read.
    pub fn offset(&self) -> u64 {
        self.lock().offset
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
}
