//! The event log, read incrementally for the overview (P8 part 4, OWL-470,
//! architecture section 5, "The local web UI", "What it reads"): the log is
//! shared by every project of the machine and grows without end, so the tail
//! keeps the byte offset it read to and parses only what was appended since.
//!
//! - It reads whole lines only: a half-written last line waits for its
//!   newline.
//! - It starts again from byte 0, dropping what it kept, when the file is
//!   shorter than the offset or is another file (on unix the device and
//!   inode, elsewhere the length alone).
//! - From each event it keeps what the overview draws: one [`Folded`] per
//!   project, as `owlshift status --waits` folds them (`why::Folded::absorb`);
//!   the latest `run_started` of each ticket of each project, with its role;
//!   the latest `dispatch` title of each ticket; and the *highlights* the
//!   "since your last visit" panel counts.
//! - A line that is not an event is kept as a note, printable-escaped: the
//!   first [`NOTES_KEPT`] of them, then a count of the rest. None is dropped
//!   without being counted.
//!
//! What a highlight is, by the shape of the event that other code emits:
//!
//! - *delivered*: a `tracker_write` with `target` `forge` and `action`
//!   `pull_request` (the rule of the daily digest);
//! - *merged*: a `pr_merged` event;
//! - *parked*: a `decision` whose `parked` is a **string**, the reason's
//!   name, which the park writes. The `decision` events watch writes when it
//!   ends a park carry `parked: true`, a boolean: they are not parks;
//! - *asked*: a `tracker_write` whose `kind` is `QUESTIONS` or `RE-ASK`;
//! - *spent*: a `usage` event's `cost_usd`.
//!
//! The highlights are capped ([`HIGHLIGHT_CAP`]), the oldest dropped, and a
//! [`Since`] says `truncated` when the time it was asked for precedes the
//! oldest one kept. [`EventTail::highlights`] only ever goes up: it counts
//! the highlights absorbed, not the ones kept, so a page that watches it
//! sees every new event even once the cap is reached.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use jiff::Timestamp;
use owlshift_contracts::event::{Event, EventKind};
use owlshift_contracts::ids::TicketId;
use serde_json::{Value, json};

use crate::events::{EventLog, printable};
use crate::why::Folded;

/// The most highlights the tail keeps: the oldest are dropped past it.
pub const HIGHLIGHT_CAP: usize = 100_000;

/// The most lines that are not events the tail keeps as notes.
pub const NOTES_KEPT: usize = 20;

/// The most characters of a line that is not an event a note shows.
const NOTE_CHARS: usize = 200;

/// What a highlight says happened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Delivered,
    Merged,
    Parked,
    Asked,
    Spent,
}

#[derive(Clone, Debug)]
struct Highlight {
    at: Timestamp,
    project: String,
    ticket: Option<TicketId>,
    kind: Kind,
    /// The cost of a `Spent` one, in US dollars.
    usd: f64,
}

/// The latest run of a ticket that started.
#[derive(Clone, Debug, PartialEq)]
pub struct Run {
    pub role: String,
    pub at: Timestamp,
}

/// The file's identity, where the system gives one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Identity(Option<(u64, u64)>);

impl Identity {
    fn of(meta: &std::fs::Metadata) -> Self {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Self(Some((meta.dev(), meta.ino())))
        }
        #[cfg(not(unix))]
        {
            let _ = meta;
            Self(None)
        }
    }
}

/// The incremental reader of a data directory's event log.
pub struct EventTail {
    path: PathBuf,
    cap: usize,
    /// The offset of the first byte not read: a line start.
    offset: u64,
    identity: Option<Identity>,
    /// Bytes read from the file since the tail was made, restarts included.
    read: u64,
    /// How many times the tail started again from byte 0.
    restarts: u64,
    folds: BTreeMap<String, Folded>,
    runs: BTreeMap<(String, TicketId), Run>,
    titles: BTreeMap<(String, TicketId), String>,
    highlights: VecDeque<Highlight>,
    /// Highlights absorbed since the tail was made, the dropped included:
    /// only goes up.
    absorbed: u64,
    /// Highlights dropped by the cap, since the last restart.
    dropped: u64,
    notes: Vec<String>,
    notes_more: u64,
}

/// What happened after a time, as the panel shows it.
#[derive(Clone, Debug, PartialEq)]
pub struct Since {
    /// The time the answer counts from.
    pub from: Timestamp,
    pub delivered: BTreeSet<(String, TicketId)>,
    pub merged: BTreeSet<(String, TicketId)>,
    pub parked: BTreeSet<(String, TicketId)>,
    pub asked: BTreeSet<(String, TicketId)>,
    pub spent_usd: f64,
    /// Highlights older than the oldest kept were dropped by the cap, and
    /// `from` precedes it: the answer may miss some.
    pub truncated: bool,
}

impl Since {
    /// The answer as the route sends it.
    pub fn json(&self) -> Value {
        let tickets = |set: &BTreeSet<(String, TicketId)>| -> Vec<Value> {
            set.iter()
                .map(|(project, ticket)| json!({ "project": project, "ticket": ticket }))
                .collect()
        };
        json!({
            "from": self.from.to_string(),
            "delivered": tickets(&self.delivered),
            "merged": tickets(&self.merged),
            "parked": tickets(&self.parked),
            "asked": tickets(&self.asked),
            "spent_usd": self.spent_usd,
            "truncated": self.truncated,
        })
    }
}

impl EventTail {
    /// The tail of the log of the data directory `data`; nothing is read
    /// before the first [`EventTail::advance`].
    pub fn new(data: &Path) -> Self {
        Self::at(EventLog::in_dir(data).path(), HIGHLIGHT_CAP)
    }

    /// The tail of the log at `path`, keeping `cap` highlights.
    pub fn at(path: &Path, cap: usize) -> Self {
        Self {
            path: path.to_owned(),
            cap: cap.max(1),
            offset: 0,
            identity: None,
            read: 0,
            restarts: 0,
            folds: BTreeMap::new(),
            runs: BTreeMap::new(),
            titles: BTreeMap::new(),
            highlights: VecDeque::new(),
            absorbed: 0,
            dropped: 0,
            notes: Vec::new(),
            notes_more: 0,
        }
    }

    /// Drops what was kept, to read from byte 0.
    fn restart(&mut self) {
        self.offset = 0;
        self.identity = None;
        self.folds.clear();
        self.runs.clear();
        self.titles.clear();
        self.highlights.clear();
        self.dropped = 0;
        self.notes.clear();
        self.notes_more = 0;
        self.restarts += 1;
    }

    /// Reads what was appended since the last call. The first call reads the
    /// whole log. An absent log is an empty one.
    pub fn advance(&mut self) -> Result<(), String> {
        let path = self.path.clone();
        let fail = |error: std::io::Error| format!("{}: {error}", path.display());
        let mut file = match File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if self.offset > 0 || self.identity.is_some() {
                    self.restart();
                }
                return Ok(());
            }
            Err(error) => return Err(fail(error)),
        };
        let meta = file.metadata().map_err(fail)?;
        let identity = Identity::of(&meta);
        let size = meta.len();
        if size < self.offset || self.identity.is_some_and(|known| known != identity) {
            self.restart();
        }
        self.identity = Some(identity);
        if size == self.offset {
            return Ok(());
        }
        file.seek(SeekFrom::Start(self.offset)).map_err(fail)?;
        let mut appended = Vec::new();
        file.take(size - self.offset)
            .read_to_end(&mut appended)
            .map_err(fail)?;
        self.read += appended.len() as u64;
        // Only whole lines: the rest waits for its newline.
        let whole = appended
            .iter()
            .rposition(|&b| b == b'\n')
            .map_or(0, |at| at + 1);
        for line in appended[..whole].split(|&b| b == b'\n') {
            let text = String::from_utf8_lossy(line);
            let text = text.trim_end_matches('\r');
            if text.trim().is_empty() {
                continue;
            }
            match Event::parse(text) {
                Ok(event) => self.absorb(&event),
                Err(error) => self.note(text, &error.to_string()),
            }
        }
        self.offset += whole as u64;
        Ok(())
    }

    fn note(&mut self, line: &str, error: &str) {
        if self.notes.len() >= NOTES_KEPT {
            self.notes_more += 1;
            return;
        }
        let shown: String = line.chars().take(NOTE_CHARS).collect();
        let cut = if line.chars().count() > NOTE_CHARS {
            "…"
        } else {
            ""
        };
        self.notes.push(format!(
            "the event log holds a line that is not an event ({}): {}{cut}",
            printable(error),
            printable(&shown)
        ));
    }

    fn absorb(&mut self, event: &Event) {
        if !self.folds.contains_key(&event.project) {
            self.folds.insert(event.project.clone(), Folded::default());
        }
        if let Some(folded) = self.folds.get_mut(&event.project) {
            folded.absorb(&event.project, event);
        }
        if let Some(ticket) = &event.ticket {
            let key = (event.project.clone(), ticket.clone());
            match event.kind {
                EventKind::RunStarted => {
                    let role = event.data.get("role").and_then(Value::as_str);
                    self.runs.insert(
                        key,
                        Run {
                            role: role.unwrap_or("unknown").to_owned(),
                            at: event.at,
                        },
                    );
                }
                EventKind::Dispatch => {
                    if let Some(title) = event.data.get("title").and_then(Value::as_str) {
                        self.titles.insert(key, title.to_owned());
                    }
                }
                _ => {}
            }
        }
        if let Some((kind, usd)) = highlight_of(event) {
            self.absorbed += 1;
            self.highlights.push_back(Highlight {
                at: event.at,
                project: event.project.clone(),
                ticket: event.ticket.clone(),
                kind,
                usd,
            });
            while self.highlights.len() > self.cap {
                self.highlights.pop_front();
                self.dropped += 1;
            }
        }
    }

    /// The project's events folded, as `why::fold` would fold the whole log
    /// read so far; `project` as the events spell it.
    pub fn folded(&self, project: &str) -> Folded {
        self.folds.get(project).cloned().unwrap_or_default()
    }

    /// The latest run that started for `ticket` of `project`, the project
    /// in any ASCII case when no event spells it exactly as given.
    pub fn run_of(&self, project: &str, ticket: &TicketId) -> Option<&Run> {
        lookup(&self.runs, project, ticket)
    }

    /// The title of the latest `dispatch` event of `ticket` of `project`.
    pub fn title_of(&self, project: &str, ticket: &TicketId) -> Option<&str> {
        lookup(&self.titles, project, ticket).map(String::as_str)
    }

    /// The lines that are not events, as notes, the cap's rest counted.
    pub fn notes(&self) -> Vec<String> {
        let mut notes = self.notes.clone();
        if self.notes_more > 0 {
            notes.push(format!(
                "and {} more lines that are not events",
                self.notes_more
            ));
        }
        notes
    }

    /// How many highlights were absorbed since the tail was made: only goes
    /// up, the cap's dropped ones included.
    pub fn highlights(&self) -> u64 {
        self.absorbed
    }

    /// The bytes read from the file since the tail was made.
    pub fn bytes_read(&self) -> u64 {
        self.read
    }

    /// How many times the tail started again from byte 0.
    pub fn restarts(&self) -> u64 {
        self.restarts
    }

    /// The offset the tail has read to.
    pub fn offset(&self) -> u64 {
        self.offset
    }

    /// What happened after `at`, or after `now` minus 24 hours when `at` is
    /// `None`: the first visit of a browser, which has no time to give.
    pub fn since(&self, at: Option<Timestamp>, now: Timestamp) -> Since {
        let from = at.unwrap_or_else(|| {
            now.checked_sub(jiff::SignedDuration::from_hours(24))
                .unwrap_or(Timestamp::MIN)
        });
        let mut since = Since {
            from,
            delivered: BTreeSet::new(),
            merged: BTreeSet::new(),
            parked: BTreeSet::new(),
            asked: BTreeSet::new(),
            spent_usd: 0.0,
            truncated: self.dropped > 0
                && self
                    .highlights
                    .front()
                    .is_some_and(|oldest| from < oldest.at),
        };
        for highlight in self.highlights.iter().filter(|h| h.at > from) {
            let Some(ticket) = &highlight.ticket else {
                if highlight.kind == Kind::Spent {
                    since.spent_usd += highlight.usd;
                }
                continue;
            };
            let key = (highlight.project.clone(), ticket.clone());
            match highlight.kind {
                Kind::Delivered => since.delivered.insert(key),
                Kind::Merged => since.merged.insert(key),
                Kind::Parked => since.parked.insert(key),
                Kind::Asked => since.asked.insert(key),
                Kind::Spent => {
                    since.spent_usd += highlight.usd;
                    continue;
                }
            };
        }
        since
    }
}

fn lookup<'a, V>(
    map: &'a BTreeMap<(String, TicketId), V>,
    project: &str,
    ticket: &TicketId,
) -> Option<&'a V> {
    map.get(&(project.to_owned(), ticket.clone())).or_else(|| {
        map.iter()
            .find(|((spelled, id), _)| id == ticket && spelled.eq_ignore_ascii_case(project))
            .map(|(_, value)| value)
    })
}

/// What an event highlights, if it does, and the cost it carries.
fn highlight_of(event: &Event) -> Option<(Kind, f64)> {
    let says = |key: &str, value: &str| event.data.get(key).and_then(Value::as_str) == Some(value);
    match event.kind {
        EventKind::TrackerWrite if says("target", "forge") && says("action", "pull_request") => {
            Some((Kind::Delivered, 0.0))
        }
        EventKind::TrackerWrite if says("kind", "QUESTIONS") || says("kind", "RE-ASK") => {
            Some((Kind::Asked, 0.0))
        }
        EventKind::PrMerged => Some((Kind::Merged, 0.0)),
        EventKind::Decision if event.data.get("parked").is_some_and(Value::is_string) => {
            Some((Kind::Parked, 0.0))
        }
        EventKind::Usage => event
            .data
            .get("cost_usd")
            .and_then(Value::as_f64)
            .filter(|cost| cost.is_finite() && *cost >= 0.0)
            .map(|cost| (Kind::Spent, cost)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;
    use crate::events::{Data, data};
    use crate::why;

    fn ts(text: &str) -> Timestamp {
        text.parse().unwrap()
    }

    fn event(
        at: &str,
        project: &str,
        ticket: Option<&str>,
        kind: EventKind,
        details: Data,
    ) -> Event {
        Event {
            format: Default::default(),
            at: ts(at),
            project: project.to_owned(),
            ticket: ticket.map(|t| TicketId::new(t).unwrap()),
            run: None,
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

    fn waits(at: &str, project: &str, ticket: &str, reason: &str) -> Event {
        event(
            at,
            project,
            Some(ticket),
            EventKind::Decision,
            data([("drain", json!("waits")), ("reason", json!(reason))]),
        )
    }

    fn id(ticket: &str) -> TicketId {
        TicketId::new(ticket).unwrap()
    }

    /// (a) A read, an append and a second read parse only the new bytes, and
    /// the folds equal a whole read's.
    #[test]
    fn a_second_read_parses_only_what_was_appended() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let first = [
            waits("2026-11-01T10:00:00Z", "o/r", "OWL-1", "slot"),
            waits("2026-11-01T10:01:00Z", "p/q", "OWL-1", "collision"),
        ];
        for event in &first {
            append(&path, &line(event));
        }
        let mut tail = EventTail::at(&path, 10);
        tail.advance().unwrap();
        let after_first = tail.bytes_read();
        assert_eq!(after_first, std::fs::metadata(&path).unwrap().len());
        let second = waits("2026-11-01T10:02:00Z", "o/r", "OWL-2", "slot");
        append(&path, &line(&second));
        tail.advance().unwrap();
        assert_eq!(
            tail.bytes_read() - after_first,
            line(&second).len() as u64,
            "only the appended bytes were read"
        );
        assert_eq!(tail.restarts(), 0);
        tail.advance().unwrap();
        assert_eq!(tail.bytes_read() - after_first, line(&second).len() as u64);
        let all = [first[0].clone(), first[1].clone(), second];
        for project in ["o/r", "p/q"] {
            assert_eq!(tail.folded(project), why::fold(&all, project), "{project}");
        }
        assert_eq!(tail.folded("nowhere/else"), Folded::default());
    }

    /// (b) A half line waits for its newline.
    #[test]
    fn a_half_written_line_waits_for_its_newline() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let event = waits("2026-11-01T10:00:00Z", "o/r", "OWL-1", "slot");
        let text = line(&event);
        let (head, rest) = text.split_at(text.len() / 2);
        append(&path, head);
        let mut tail = EventTail::at(&path, 10);
        tail.advance().unwrap();
        assert!(tail.folded("o/r").current.is_empty());
        assert_eq!(tail.offset(), 0);
        assert!(tail.notes().is_empty(), "a half line is not a bad line");
        append(&path, rest);
        tail.advance().unwrap();
        assert_eq!(tail.folded("o/r").current.len(), 1);
        assert_eq!(tail.offset(), text.len() as u64);
    }

    /// (c) A shorter log and another file are read again from the start.
    #[test]
    fn a_shorter_log_and_a_replaced_one_are_read_again() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        for n in 1..=3 {
            append(
                &path,
                &line(&waits(
                    "2026-11-01T10:00:00Z",
                    "o/r",
                    &format!("OWL-{n}"),
                    "slot",
                )),
            );
        }
        let mut tail = EventTail::at(&path, 10);
        tail.advance().unwrap();
        assert_eq!(tail.folded("o/r").current.len(), 3);
        // Truncated to one line.
        std::fs::write(
            &path,
            line(&waits("2026-11-01T11:00:00Z", "o/r", "OWL-9", "slot")),
        )
        .unwrap();
        tail.advance().unwrap();
        assert_eq!(tail.restarts(), 1);
        let current = tail.folded("o/r").current;
        assert_eq!(current.keys().collect::<Vec<_>>(), [&id("OWL-9")]);
        // Replaced by another file of the same length or longer.
        let other = dir.path().join("other.jsonl");
        std::fs::write(
            &other,
            format!(
                "{}{}",
                line(&waits("2026-11-01T12:00:00Z", "o/r", "OWL-7", "slot")),
                line(&waits("2026-11-01T12:01:00Z", "o/r", "OWL-8", "slot"))
            ),
        )
        .unwrap();
        std::fs::rename(&other, &path).unwrap();
        tail.advance().unwrap();
        #[cfg(unix)]
        {
            assert_eq!(tail.restarts(), 2);
            let current = tail.folded("o/r").current;
            assert_eq!(
                current.keys().collect::<Vec<_>>(),
                [&id("OWL-7"), &id("OWL-8")]
            );
        }
        // Removed: an empty log.
        std::fs::remove_file(&path).unwrap();
        tail.advance().unwrap();
        assert!(tail.folded("o/r").current.is_empty());
    }

    fn tracker(at: &str, project: &str, ticket: &str, pairs: Vec<(&str, Value)>) -> Event {
        event(
            at,
            project,
            Some(ticket),
            EventKind::TrackerWrite,
            pairs.into_iter().map(|(k, v)| (k.to_owned(), v)).collect(),
        )
    }

    fn lived(tail: &mut EventTail, path: &Path, events: &[Event]) {
        for event in events {
            append(path, &line(event));
        }
        tail.advance().unwrap();
    }

    /// (d) What `since` counts, by the shapes other code writes.
    #[test]
    fn since_counts_what_happened_after_the_given_time_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let mut tail = EventTail::at(&path, 100);
        let usage = |at: &str, cost: Value| {
            event(
                at,
                "o/r",
                Some("OWL-1"),
                EventKind::Usage,
                data([("cost_usd", cost)]),
            )
        };
        let delivered = |at: &str, ticket: &str| {
            tracker(
                at,
                "o/r",
                ticket,
                vec![
                    ("target", json!("forge")),
                    ("action", json!("pull_request")),
                ],
            )
        };
        let comment = |at: &str, ticket: &str, kind: &str| {
            tracker(
                at,
                "o/r",
                ticket,
                vec![
                    ("target", json!("tracker")),
                    ("action", json!("comment")),
                    ("kind", json!(kind)),
                ],
            )
        };
        let decision = |at: &str, ticket: &str, parked: Value| {
            event(
                at,
                "o/r",
                Some(ticket),
                EventKind::Decision,
                data([("parked", parked)]),
            )
        };
        lived(
            &mut tail,
            &path,
            &[
                // Before the asked time: not counted.
                delivered("2026-11-01T09:00:00Z", "OWL-0"),
                usage("2026-11-01T09:01:00Z", json!(9.0)),
                // After it.
                delivered("2026-11-01T10:30:00Z", "OWL-1"),
                delivered("2026-11-01T10:31:00Z", "OWL-1"),
                event(
                    "2026-11-01T10:40:00Z",
                    "o/r",
                    Some("OWL-2"),
                    EventKind::PrMerged,
                    Data::new(),
                ),
                // A real park: the reason's name, a string.
                decision("2026-11-01T10:50:00Z", "OWL-3", json!("AwaitingInput")),
                // Watch's ending event: a boolean, not a park.
                decision("2026-11-01T10:51:00Z", "OWL-4", json!(true)),
                comment("2026-11-01T11:00:00Z", "OWL-5", "QUESTIONS"),
                comment("2026-11-01T11:01:00Z", "OWL-6", "RE-ASK"),
                comment("2026-11-01T11:02:00Z", "OWL-7", "PARKED"),
                usage("2026-11-01T11:10:00Z", json!(0.25)),
                usage("2026-11-01T11:11:00Z", json!(0.5)),
                usage("2026-11-01T11:12:00Z", Value::Null),
            ],
        );
        let since = tail.since(Some(ts("2026-11-01T10:00:00Z")), ts("2026-11-01T12:00:00Z"));
        let names = |set: &BTreeSet<(String, TicketId)>| -> Vec<String> {
            set.iter().map(|(_, t)| t.to_string()).collect()
        };
        assert_eq!(names(&since.delivered), ["OWL-1"], "one ticket once");
        assert_eq!(names(&since.merged), ["OWL-2"]);
        assert_eq!(names(&since.parked), ["OWL-3"], "watch's ending is no park");
        assert_eq!(names(&since.asked), ["OWL-5", "OWL-6"]);
        assert!((since.spent_usd - 0.75).abs() < 1e-9, "{}", since.spent_usd);
        assert!(!since.truncated);
        assert_eq!(since.from, ts("2026-11-01T10:00:00Z"));
        let json = since.json();
        assert_eq!(json["delivered"][0]["ticket"], "OWL-1");
        assert_eq!(json["delivered"][0]["project"], "o/r");
    }

    /// (e) With no time given, the last 24 hours.
    #[test]
    fn since_with_no_time_is_the_last_24_hours() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let mut tail = EventTail::at(&path, 100);
        let merged = |at: &str, ticket: &str| {
            event(at, "o/r", Some(ticket), EventKind::PrMerged, Data::new())
        };
        lived(
            &mut tail,
            &path,
            &[
                merged("2026-11-04T22:00:00Z", "OWL-OLD"),
                merged("2026-11-05T22:00:00Z", "OWL-NEW"),
            ],
        );
        let now = ts("2026-11-06T00:00:00Z");
        let since = tail.since(None, now);
        assert_eq!(since.from, ts("2026-11-05T00:00:00Z"));
        let merged: Vec<_> = since.merged.iter().map(|(_, t)| t.as_str()).collect();
        assert_eq!(merged, ["OWL-NEW"]);
    }

    /// (f) A line holding markup and a line that is no event are notes,
    /// escaped on the wire, and the cap on notes counts the rest.
    #[test]
    fn lines_that_are_not_events_are_notes_and_the_rest_is_counted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        append(&path, "<script>alert(1)</script>\n");
        let mut tail = EventTail::at(&path, 10);
        tail.advance().unwrap();
        let notes = tail.notes();
        assert_eq!(notes.len(), 1);
        let wire = crate::ui::server::json_text(&json!({ "notes": notes }));
        assert!(!wire.contains('<') && !wire.contains('>'), "{wire}");
        assert!(wire.contains("\\u003cscript"), "{wire}");
        for n in 0..(NOTES_KEPT + 4) {
            append(&path, &format!("not an event {n}\u{1b}[31m\n"));
        }
        tail.advance().unwrap();
        let notes = tail.notes();
        assert_eq!(notes.len(), NOTES_KEPT + 1, "{notes:#?}");
        assert_eq!(
            notes.last().unwrap(),
            "and 5 more lines that are not events"
        );
        assert!(
            notes.iter().all(|note| !note.contains('\u{1b}')),
            "{notes:#?}"
        );
        assert_eq!(tail.highlights(), 0);
    }

    /// (g) The latest run and the title are per (project, ticket).
    #[test]
    fn the_latest_run_and_title_belong_to_a_project_and_a_ticket() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let mut tail = EventTail::at(&path, 10);
        let started = |at: &str, project: &str, role: &str| {
            event(
                at,
                project,
                Some("OWL-1"),
                EventKind::RunStarted,
                data([("role", json!(role))]),
            )
        };
        lived(
            &mut tail,
            &path,
            &[
                started("2026-11-01T10:00:00Z", "o/r", "design"),
                started("2026-11-01T10:05:00Z", "o/r", "build"),
                started("2026-11-01T10:10:00Z", "p/q", "verify"),
                event(
                    "2026-11-01T09:00:00Z",
                    "o/r",
                    Some("OWL-1"),
                    EventKind::Dispatch,
                    data([("title", json!("Show the overview"))]),
                ),
            ],
        );
        let run = tail.run_of("o/r", &id("OWL-1")).unwrap();
        assert_eq!(
            (run.role.as_str(), run.at),
            ("build", ts("2026-11-01T10:05:00Z"))
        );
        let other = tail.run_of("p/q", &id("OWL-1")).unwrap();
        assert_eq!(other.role, "verify");
        assert_eq!(tail.run_of("O/R", &id("OWL-1")).unwrap().role, "build");
        assert_eq!(tail.run_of("o/r", &id("OWL-2")), None);
        assert_eq!(
            tail.title_of("o/r", &id("OWL-1")),
            Some("Show the overview")
        );
        assert_eq!(tail.title_of("p/q", &id("OWL-1")), None);
    }

    /// F1/F6: the highlights counter only goes up, past the cap too, and the
    /// answer says `truncated` for a time the cap dropped.
    #[test]
    fn the_highlights_counter_goes_up_past_the_cap() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let mut tail = EventTail::at(&path, 2);
        let merged = |minute: u32, ticket: &str| {
            event(
                &format!("2026-11-01T10:{minute:02}:00Z"),
                "o/r",
                Some(ticket),
                EventKind::PrMerged,
                Data::new(),
            )
        };
        lived(&mut tail, &path, &[merged(0, "OWL-1"), merged(1, "OWL-2")]);
        assert_eq!(tail.highlights(), 2);
        let seen = tail.highlights();
        lived(&mut tail, &path, &[merged(2, "OWL-3")]);
        assert!(tail.highlights() > seen, "the count changed at the cap");
        lived(&mut tail, &path, &[merged(3, "OWL-4")]);
        assert_eq!(tail.highlights(), 4);
        let now = ts("2026-11-01T11:00:00Z");
        let early = tail.since(Some(ts("2026-11-01T09:00:00Z")), now);
        assert!(early.truncated);
        let names: Vec<_> = early.merged.iter().map(|(_, t)| t.to_string()).collect();
        assert_eq!(names, ["OWL-3", "OWL-4"]);
        let late = tail.since(Some(ts("2026-11-01T10:02:00Z")), now);
        assert!(!late.truncated);
        assert_eq!(late.merged.len(), 1);
    }
}
