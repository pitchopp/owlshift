//! The event log, indexed for the Orchestrator and Spend screens (P8 part 7,
//! OWL-472, runtime and operations, "Observability & the local web UI"). The
//! log is shared by every project of the machine and grows without end, so
//! the index keeps the byte offset it read to ([`LineTail`]) and parses only
//! what was appended since. It keeps what the two screens draw, not the log:
//!
//! - the *usage records* (time, project, ticket, run, cost in micro-dollars
//!   and tokens) of the `usage` events;
//! - the role and harness each run started with, by (project, ticket, run
//!   id): a run id is only unique inside one ticket's runs folder (it is a
//!   timestamp, numbered on a clash), so the id alone would mix two tickets'
//!   runs;
//! - the latest `watch_started` or `watch_ended` decision of the daemon per
//!   project, which is what `status::watch_state` reads;
//! - a *feed*: the last [`FEED_KEPT`] `decision` and `warning` events and
//!   lines that are not events, each keyed by the byte offset of its line,
//!   which is the cursor a page passes back.
//!
//! When the log is replaced or shortened the index clears all four, reads
//! the new file whole and raises its *generation*, so a page that holds the
//! old one knows what it kept is stale; it never adds a record twice.

use std::collections::{HashMap, VecDeque};
use std::path::Path;

use jiff::Timestamp;
use owlshift_contracts::event::{Event, EventKind};
use serde_json::Value;

use super::tail::LineTail;
use crate::events::EventLog;

/// The most entries the feed keeps: the oldest are dropped past it.
pub const FEED_KEPT: usize = 500;

/// The longest line of the log the index reads whole: a longer one is cut,
/// so it does not parse and is kept as a line that is not an event.
const LINE_CUT: usize = 4 * 1024 * 1024;

/// What a `usage` event says, compacted.
#[derive(Clone, Debug, PartialEq)]
pub struct UsageRecord {
    pub at: Timestamp,
    pub project: String,
    pub ticket: Option<String>,
    pub run: Option<String>,
    /// The run's own cost, in micro-dollars; 0 when the harness reports none.
    pub cost_micros: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// What a run's `run_started` says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunFacts {
    pub role: String,
    /// The harness, from the event; `None` for a run recorded before the
    /// event named it or on a harness with none.
    pub harness: Option<String>,
}

/// What a feed entry is.
#[derive(Clone, Debug, PartialEq)]
pub enum Item {
    Event(Box<Event>),
    /// A line that is not an event, as the log holds it (cut at
    /// 4 MiB).
    Line(String),
}

/// One entry of the feed.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    /// The byte offset of its line in the log: its cursor.
    pub offset: u64,
    pub item: Item,
}

/// The feed from a cursor.
#[derive(Clone, Debug, PartialEq)]
pub struct Feed {
    pub entries: Vec<Entry>,
    /// The page's cursors do not hold: the log was replaced since its
    /// generation, its cursor is past the log's end, or entries it did not
    /// see were dropped. `entries` are then the latest, to be read afresh.
    pub reset: bool,
    pub generation: u64,
}

/// The index of a data directory's event log.
pub struct EventIndex {
    tail: LineTail,
    generation: u64,
    usage: Vec<UsageRecord>,
    runs: HashMap<(String, String, String), RunFacts>,
    watches: HashMap<String, Event>,
    feed: VecDeque<Entry>,
    /// Entries the cap dropped since the last restart.
    dropped: u64,
}

fn lower(project: &str) -> String {
    project.to_ascii_lowercase()
}

impl EventIndex {
    pub fn new(data: &Path) -> Self {
        Self::at(EventLog::in_dir(data).path())
    }

    pub fn at(path: &Path) -> Self {
        Self {
            tail: LineTail::new(path),
            generation: 0,
            usage: Vec::new(),
            runs: HashMap::new(),
            watches: HashMap::new(),
            feed: VecDeque::new(),
            dropped: 0,
        }
    }

    fn clear(&mut self) {
        self.usage.clear();
        self.runs.clear();
        self.watches.clear();
        self.feed.clear();
        self.dropped = 0;
        self.generation += 1;
    }

    /// Reads what was appended since the last call; the first call reads the
    /// whole log. A log that is not there is an empty one.
    pub fn update(&mut self) -> Result<(), String> {
        let read = self
            .tail
            .read(usize::MAX, LINE_CUT)
            .map_err(|error| error.to_string())?;
        if read.restarted {
            self.clear();
        }
        for line in read.lines {
            match Event::parse(&line.text) {
                Ok(event) => self.absorb(line.offset, event),
                Err(_) => self.push(Entry {
                    offset: line.offset,
                    item: Item::Line(line.text),
                }),
            }
        }
        Ok(())
    }

    fn push(&mut self, entry: Entry) {
        self.feed.push_back(entry);
        while self.feed.len() > FEED_KEPT {
            self.feed.pop_front();
            self.dropped += 1;
        }
    }

    fn absorb(&mut self, offset: u64, event: Event) {
        match event.kind {
            EventKind::Usage => {
                let number = |key: &str| event.data.get(key).and_then(Value::as_u64).unwrap_or(0);
                let cost = event
                    .data
                    .get("cost_usd")
                    .and_then(Value::as_f64)
                    .filter(|usd| usd.is_finite() && *usd > 0.0)
                    .map_or(0, |usd| (usd * 1_000_000.0).round() as u64);
                self.usage.push(UsageRecord {
                    at: event.at,
                    project: event.project.clone(),
                    ticket: event.ticket.as_ref().map(|t| t.as_str().to_owned()),
                    run: event.run.clone(),
                    cost_micros: cost,
                    input_tokens: number("input_tokens"),
                    output_tokens: number("output_tokens"),
                });
            }
            EventKind::RunStarted => {
                if let (Some(ticket), Some(run)) = (&event.ticket, &event.run) {
                    let text = |key: &str| {
                        event
                            .data
                            .get(key)
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                    };
                    self.runs.insert(
                        (
                            lower(&event.project),
                            ticket.as_str().to_owned(),
                            run.clone(),
                        ),
                        RunFacts {
                            role: text("role").unwrap_or_else(|| "unknown".to_owned()),
                            harness: text("harness"),
                        },
                    );
                }
            }
            EventKind::Decision => {
                if matches!(
                    event.data.get("service").and_then(Value::as_str),
                    Some("watch_started" | "watch_ended")
                ) {
                    self.watches.insert(lower(&event.project), event.clone());
                }
            }
            _ => {}
        }
        if matches!(event.kind, EventKind::Decision | EventKind::Warning) {
            self.push(Entry {
                offset,
                item: Item::Event(Box::new(event)),
            });
        }
    }

    /// How many times the log was replaced or shortened while the index
    /// read it.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The offset of the first byte not read: the log's end as the index
    /// knows it.
    pub fn offset(&self) -> u64 {
        self.tail.offset()
    }

    /// The usage records, oldest first.
    pub fn usage(&self) -> &[UsageRecord] {
        &self.usage
    }

    /// The role and harness `run` of `ticket` of `project` started with.
    pub fn run(&self, project: &str, ticket: &str, run: &str) -> Option<&RunFacts> {
        self.runs
            .get(&(lower(project), ticket.to_owned(), run.to_owned()))
    }

    /// The latest `watch_started` or `watch_ended` decision of `project`.
    pub fn watch_event(&self, project: &str) -> Option<&Event> {
        self.watches.get(&lower(project))
    }

    /// The feed after the cursor `after` that a page holds at generation
    /// `generation`, or its latest `first` entries when it has none.
    pub fn feed(&self, cursor: Option<(u64, u64)>, first: usize) -> Feed {
        let latest = || {
            let skip = self.feed.len().saturating_sub(first);
            self.feed.iter().skip(skip).cloned().collect::<Vec<_>>()
        };
        let Some((generation, after)) = cursor else {
            return Feed {
                entries: latest(),
                reset: false,
                generation: self.generation,
            };
        };
        let oldest = self.feed.front().map(|entry| entry.offset);
        // A cursor is the offset of a line the page saw: it is before the
        // end of what was read. If entries were dropped and the page's
        // cursor is before the oldest kept, it missed some.
        let stale = generation != self.generation
            || after >= self.tail.offset()
            || (self.dropped > 0 && oldest.is_some_and(|oldest| after < oldest));
        if stale {
            return Feed {
                entries: latest(),
                reset: true,
                generation: self.generation,
            };
        }
        Feed {
            entries: self
                .feed
                .iter()
                .filter(|entry| entry.offset > after)
                .cloned()
                .collect(),
            reset: false,
            generation: self.generation,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use serde_json::json;

    use super::*;
    use crate::status::{Watch, watch_state};

    fn event(
        at: &str,
        project: &str,
        ticket: Option<&str>,
        run: Option<&str>,
        kind: &str,
        data: Value,
    ) -> String {
        let mut value = json!({
            "format": 1,
            "at": at,
            "project": project,
            "kind": kind,
            "data": data,
        });
        if let Some(ticket) = ticket {
            value["ticket"] = json!(ticket);
        }
        if let Some(run) = run {
            value["run"] = json!(run);
        }
        value.to_string()
    }

    fn append(path: &Path, text: &str) {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        file.write_all(text.as_bytes()).unwrap();
    }

    fn line(text: String) -> String {
        format!("{text}\n")
    }

    fn usage(at: &str, ticket: &str, run: &str, cost: f64) -> String {
        line(event(
            at,
            "acme/web",
            Some(ticket),
            Some(run),
            "usage",
            json!({ "cost_usd": cost, "input_tokens": 10, "output_tokens": 5 }),
        ))
    }

    #[test]
    fn a_second_update_reads_only_what_was_appended() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        append(&path, &usage("2026-11-05T09:00:00Z", "OWL-1", "r1", 0.5));
        let mut index = EventIndex::at(&path);
        index.update().unwrap();
        let first = index.offset();
        assert_eq!(index.usage().len(), 1);
        assert_eq!(index.usage()[0].cost_micros, 500_000);
        index.update().unwrap();
        assert_eq!(index.offset(), first);
        append(&path, &usage("2026-11-05T10:00:00Z", "OWL-1", "r2", 0.25));
        index.update().unwrap();
        assert!(index.offset() > first);
        assert_eq!(index.usage().len(), 2);
        assert_eq!(index.generation(), 0);
    }

    #[test]
    fn a_line_that_is_not_an_event_enters_the_feed_as_raw_text() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        append(&path, "not an event <script>\n");
        let mut index = EventIndex::at(&path);
        index.update().unwrap();
        let feed = index.feed(None, 100);
        assert_eq!(feed.entries.len(), 1);
        assert_eq!(
            feed.entries[0].item,
            Item::Line("not an event <script>".to_owned())
        );
    }

    #[test]
    fn the_feed_drops_its_oldest_past_the_cap_and_tells_a_page_that_fell_behind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let mut text = String::new();
        for n in 0..FEED_KEPT + 20 {
            text.push_str(&format!("raw {n}\n"));
        }
        append(&path, &text);
        let mut index = EventIndex::at(&path);
        index.update().unwrap();
        let all = index.feed(None, usize::MAX);
        assert_eq!(all.entries.len(), FEED_KEPT);
        assert_eq!(all.entries[0].item, Item::Line("raw 20".to_owned()));
        // A cursor older than the oldest kept: the page missed entries.
        let behind = index.feed(Some((index.generation(), 0)), 100);
        assert!(behind.reset);
        assert_eq!(behind.entries.len(), 100);
        // A cursor within the kept ones gets only what follows it.
        let cursor = all.entries[FEED_KEPT - 3].offset;
        let near = index.feed(Some((index.generation(), cursor)), 100);
        assert!(!near.reset);
        assert_eq!(near.entries.len(), 2);
        // A cursor past the end is no cursor of this log.
        let past = index.feed(Some((index.generation(), index.offset() + 10)), 100);
        assert!(past.reset);
    }

    #[test]
    fn a_watch_is_the_one_status_reads_from_the_full_list() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let started = "2026-11-05T08:00:00Z";
        let events = [
            event(
                "2026-11-05T08:01:00Z",
                "acme/web",
                None,
                None,
                "decision",
                json!({ "service": "watch_started", "pid": 7 }),
            ),
            event(
                "2026-11-05T08:30:00Z",
                "Acme/Web",
                None,
                None,
                "decision",
                json!({ "service": "watch_ended", "end": "exit status 1", "asked": false,
                         "restart_in_ms": 60000, "in_a_row": 3 }),
            ),
            // An unservable project, an ended one (asked to), a running one
            // and one whose latest end came before the daemon started.
            event(
                "2026-11-05T08:02:00Z",
                "acme/api",
                None,
                None,
                "decision",
                json!({ "service": "watch_ended", "end": "refused", "asked": false,
                         "unservable": "owlshift.toml is not accepted" }),
            ),
            event(
                "2026-11-05T08:03:00Z",
                "acme/db",
                None,
                None,
                "decision",
                json!({ "service": "watch_started", "pid": 9 }),
            ),
            event(
                "2026-11-05T08:40:00Z",
                "acme/db",
                None,
                None,
                "decision",
                json!({ "service": "watch_ended", "end": "stopped", "asked": true }),
            ),
            event(
                "2026-11-05T08:04:00Z",
                "acme/ops",
                None,
                None,
                "decision",
                json!({ "service": "watch_started", "pid": 12 }),
            ),
            event(
                "2026-11-05T07:00:00Z",
                "acme/old",
                None,
                None,
                "decision",
                json!({ "service": "watch_ended", "end": "exit status 1", "asked": false,
                         "restart_in_ms": 1000, "in_a_row": 1 }),
            ),
        ];
        for event in &events {
            append(&path, &format!("{event}\n"));
        }
        let mut index = EventIndex::at(&path);
        index.update().unwrap();
        let since: Timestamp = started.parse().unwrap();
        let all: Vec<Event> = events.iter().map(|e| Event::parse(e).unwrap()).collect();
        let mut seen = Vec::new();
        for project in [
            "acme/web",
            "acme/api",
            "acme/db",
            "acme/ops",
            "acme/old",
            "acme/none",
        ] {
            let from_index: Vec<Event> = index.watch_event(project).cloned().into_iter().collect();
            let want = watch_state(&all, project, since);
            assert_eq!(watch_state(&from_index, project, since), want, "{project}");
            seen.push(want);
        }
        assert!(matches!(seen[0], Watch::Restarting { .. }), "{seen:?}");
        assert!(matches!(seen[1], Watch::Unservable { .. }), "{seen:?}");
        assert!(matches!(seen[2], Watch::Ended { .. }), "{seen:?}");
        assert!(matches!(seen[3], Watch::Running { .. }), "{seen:?}");
        // An end before the daemon started, and a project with no event.
        assert_eq!(seen[4], Watch::NotStarted);
        assert_eq!(seen[5], Watch::NotStarted);
        assert!(index.watch_event("acme/none").is_none());
    }

    #[test]
    fn replacing_the_log_does_not_double_the_sums_and_raises_the_generation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        append(&path, &usage("2026-11-05T09:00:00Z", "OWL-1", "r1", 1.0));
        append(&path, "old raw line\n");
        let mut index = EventIndex::at(&path);
        index.update().unwrap();
        assert_eq!(index.feed(None, 100).entries.len(), 1);
        let before = index.generation();
        std::fs::write(&path, usage("2026-11-06T09:00:00Z", "OWL-2", "r1", 2.0)).unwrap();
        index.update().unwrap();
        assert_eq!(index.generation(), before + 1);
        let sums: u64 = index.usage().iter().map(|usage| usage.cost_micros).sum();
        assert_eq!(sums, 2_000_000);
        assert!(index.feed(None, 100).entries.is_empty());
        // A page with the old generation is told to read afresh.
        assert!(index.feed(Some((before, 0)), 100).reset);
    }

    #[test]
    fn two_tickets_with_the_same_run_id_keep_their_own_role_and_harness() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let started = |ticket: &str, role: &str, harness: Option<&str>| {
            let mut data = json!({ "role": role });
            if let Some(harness) = harness {
                data["harness"] = json!(harness);
            }
            line(event(
                "2026-11-05T09:00:00Z",
                "acme/web",
                Some(ticket),
                Some("20261105T090000Z"),
                "run_started",
                data,
            ))
        };
        append(&path, &started("OWL-1", "build", Some("claude")));
        append(&path, &started("OWL-2", "verify", None));
        let mut index = EventIndex::at(&path);
        index.update().unwrap();
        let one = index.run("Acme/Web", "OWL-1", "20261105T090000Z").unwrap();
        assert_eq!(
            (one.role.as_str(), one.harness.as_deref()),
            ("build", Some("claude"))
        );
        let two = index.run("acme/web", "OWL-2", "20261105T090000Z").unwrap();
        assert_eq!(
            (two.role.as_str(), two.harness.as_deref()),
            ("verify", None)
        );
    }
}
