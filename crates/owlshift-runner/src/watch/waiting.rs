//! The WAITING comment of a ticket in flight whose next stage step watch
//! holds back (OWL-419; architecture section 5, "Why a ticket waits", "Told
//! on the ticket"): one comment per change of reason, folded once the wait
//! ends.
//!
//! The comment shown is kept in the ticket's folder, `waiting.json`, with
//! the reason it says: written once posted, removed once folded. So a watch
//! started again posts nothing for a reason already shown and folds a
//! comment whose wait ended meanwhile, and no comment is resolved twice.
//! Watch's memory caches what each file says.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::PathBuf;

use serde_json::{Value, json};

use owlshift_adapters::tracker::Tracker;
use owlshift_contracts::event::EventKind;
use owlshift_contracts::ids::TicketId;

use crate::events::{EventSink, data};
use crate::on_demand;
use crate::project::ProjectDirs;
use crate::writer::{WaitComment, Writer};

/// The file of the ticket's folder that keeps its WAITING comment shown.
const FILE: &str = "waiting.json";

/// A WAITING comment shown on a ticket.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Shown {
    /// The reason it says: the same reason posts nothing again.
    reason: String,
    /// The tracker's id for it.
    comment: String,
}

/// What watch knows of the WAITING comments shown, per ticket: `None` once
/// read and none is shown.
#[derive(Debug, Default)]
pub(super) struct WaitComments {
    shown: HashMap<TicketId, Option<Shown>>,
}

/// Where a WAITING comment is told: the tracker, the project's folders and
/// the event log.
pub(super) struct Board<'b, 's> {
    pub tracker: &'b dyn Tracker,
    pub dirs: &'b ProjectDirs,
    pub sink: &'b mut EventSink<'s>,
}

impl WaitComments {
    /// Shows that `ticket` waits for `reason`, said as `phrase`: nothing
    /// when that reason is already shown; otherwise one WAITING comment, its
    /// `tracker_write` event or a `warning`, and the comment shown before,
    /// if any, folded. The caller calls it once per change of reason.
    pub(super) fn show(
        &mut self,
        board: &mut Board<'_, '_>,
        ticket: &TicketId,
        reason: &str,
        phrase: String,
    ) {
        let before = self.current(board, ticket);
        if before.as_ref().is_some_and(|shown| shown.reason == reason) {
            return;
        }
        let comment = WaitComment {
            ticket: ticket.clone(),
            phrase,
        };
        let posted = match Writer::new(board.tracker).post_wait(&comment) {
            Ok(posted) => {
                board.sink.emit(
                    ticket,
                    None,
                    EventKind::TrackerWrite,
                    on_demand::comment_written("WAITING", &posted),
                );
                Some(posted.id)
            }
            Err(error) => {
                board.sink.emit(
                    ticket,
                    None,
                    EventKind::Warning,
                    data([
                        ("what", json!("wait_comment_not_posted")),
                        ("reason", json!(error.to_string())),
                    ]),
                );
                None
            }
        };
        // What the comment shown before said no longer holds, whether or
        // not the new one could be posted.
        if let Some(before) = before {
            fold(board, ticket, &before.comment, posted.as_deref());
        }
        let shown = posted.map(|comment| Shown {
            reason: reason.to_owned(),
            comment,
        });
        record(board, ticket, shown.as_ref());
        self.shown.insert(ticket.clone(), shown);
    }

    /// The wait of `ticket` ended: the WAITING comment shown, if any, is
    /// folded, and nothing is posted.
    pub(super) fn clear(&mut self, board: &mut Board<'_, '_>, ticket: &TicketId) {
        let Some(before) = self.current(board, ticket) else {
            return;
        };
        fold(board, ticket, &before.comment, None);
        record(board, ticket, None);
        self.shown.insert(ticket.clone(), None);
    }

    /// Clears every ticket watch knows of that `kept` does not list: it
    /// left the tickets in flight, so its wait ended.
    pub(super) fn retain(&mut self, board: &mut Board<'_, '_>, kept: &[TicketId]) {
        let left: Vec<TicketId> = self
            .shown
            .keys()
            .filter(|ticket| !kept.contains(ticket))
            .cloned()
            .collect();
        for ticket in left {
            self.clear(board, &ticket);
            self.shown.remove(&ticket);
        }
    }

    /// The comment shown on `ticket`: what watch knows, or what its file
    /// says, read once. A file that does not read is a `warning` and taken
    /// as none.
    fn current(&mut self, board: &mut Board<'_, '_>, ticket: &TicketId) -> Option<Shown> {
        if let Some(known) = self.shown.get(ticket) {
            return known.clone();
        }
        let shown = match read(board.dirs, ticket) {
            Ok(shown) => shown,
            Err(reason) => {
                board.sink.emit(
                    ticket,
                    None,
                    EventKind::Warning,
                    data([
                        ("what", json!("wait_comment_unread")),
                        ("reason", json!(reason)),
                    ]),
                );
                None
            }
        };
        self.shown.insert(ticket.clone(), shown.clone());
        shown
    }
}

/// Resolves `comment`, superseded by `by` when one was posted: a
/// `tracker_write` event as a fold's (OWL-353), or a `comment_not_folded`
/// warning; nothing from a tracker that cannot fold.
fn fold(board: &mut Board<'_, '_>, ticket: &TicketId, comment: &str, by: Option<&str>) {
    match Writer::new(board.tracker).resolve_wait(comment) {
        Ok(false) => {}
        Ok(true) => {
            let mut event = data([
                ("target", json!("tracker")),
                ("action", json!("resolve")),
                ("comment", json!(comment)),
            ]);
            if let Some(by) = by {
                event.insert("by".to_owned(), json!(by));
            }
            board
                .sink
                .emit(ticket, None, EventKind::TrackerWrite, event);
        }
        Err(error) => {
            board.sink.emit(
                ticket,
                None,
                EventKind::Warning,
                data([
                    ("what", json!("comment_not_folded")),
                    ("comment", json!(comment)),
                    ("reason", json!(error.to_string())),
                ]),
            );
        }
    }
}

fn path(dirs: &ProjectDirs, ticket: &TicketId) -> PathBuf {
    dirs.ticket_dir(ticket).join(FILE)
}

/// What `ticket`'s file says: `None` when it is absent.
fn read(dirs: &ProjectDirs, ticket: &TicketId) -> Result<Option<Shown>, String> {
    let text = match fs::read_to_string(path(dirs, ticket)) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.to_string()),
    };
    let value: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    match (value["reason"].as_str(), value["comment"].as_str()) {
        (Some(reason), Some(comment)) => Ok(Some(Shown {
            reason: reason.to_owned(),
            comment: comment.to_owned(),
        })),
        _ => Err(format!("{FILE} names no reason and comment")),
    }
}

/// Writes `shown` to `ticket`'s file, whole or not at all, or removes the
/// file for none; a failure is a `warning`, watch's memory still knowing it.
fn record(board: &mut Board<'_, '_>, ticket: &TicketId, shown: Option<&Shown>) {
    let file = path(board.dirs, ticket);
    let written = match shown {
        None => match fs::remove_file(&file) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
            _ => Ok(()),
        },
        Some(shown) => {
            let text = json!({ "reason": shown.reason, "comment": shown.comment }).to_string();
            let partial = file.with_extension("json.partial");
            fs::create_dir_all(board.dirs.ticket_dir(ticket))
                .and_then(|()| fs::write(&partial, text))
                .and_then(|()| fs::rename(&partial, &file))
        }
    };
    if let Err(error) = written {
        board.sink.emit(
            ticket,
            None,
            EventKind::Warning,
            data([
                ("what", json!("wait_comment_not_recorded")),
                ("reason", json!(error.to_string())),
            ]),
        );
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use owlshift_adapters::forge::Repo;
    use owlshift_adapters::tracker::{
        Author, Capability, Comment, Error as TrackerError, ErrorKind, Ticket,
    };
    use owlshift_contracts::event::Event;

    use super::*;
    use crate::events::EventLog;

    /// A tracker that records what is posted and resolved, folding or not.
    struct Board2 {
        fold: bool,
        refuse_posts: bool,
        posted: RefCell<Vec<String>>,
        resolved: RefCell<Vec<String>>,
    }

    impl Board2 {
        fn new(fold: bool) -> Self {
            Self {
                fold,
                refuse_posts: false,
                posted: RefCell::default(),
                resolved: RefCell::default(),
            }
        }
    }

    impl Tracker for Board2 {
        fn capabilities(&self) -> &'static [Capability] {
            if self.fold {
                &[Capability::Comments, Capability::FoldComments]
            } else {
                &[Capability::Comments]
            }
        }

        fn ticket(&self, _: &TicketId) -> Result<Ticket, TrackerError> {
            unreachable!("a WAITING comment reads no ticket")
        }

        fn comments(&self, _: &TicketId) -> Result<Vec<Comment>, TrackerError> {
            unreachable!("a WAITING comment reads no comment")
        }

        fn post_comment(&self, _: &TicketId, body: &str) -> Result<Comment, TrackerError> {
            if self.refuse_posts {
                return Err(TrackerError::new(ErrorKind::Other, "tracker down"));
            }
            let mut posted = self.posted.borrow_mut();
            posted.push(body.to_owned());
            Ok(Comment {
                id: format!("c{}", posted.len()),
                author: Author::Other {
                    name: "owlshift".to_owned(),
                },
                created_at: jiff::Timestamp::UNIX_EPOCH,
                edited_at: None,
                body: body.to_owned(),
                parent: None,
                reactions: Vec::new(),
            })
        }

        fn set_stage(&self, _: &TicketId, _: &str) -> Result<(), TrackerError> {
            unreachable!("a WAITING comment moves no stage")
        }

        fn resolve_comment(&self, comment: &str) -> Result<(), TrackerError> {
            self.resolved.borrow_mut().push(comment.to_owned());
            Ok(())
        }
    }

    struct Bench {
        tmp: tempfile::TempDir,
        dirs: ProjectDirs,
    }

    impl Bench {
        fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let dirs = ProjectDirs::new(tmp.path(), &Repo::parse("acme/app").unwrap());
            Self { tmp, dirs }
        }

        /// Runs `act` with a board on `tracker`, and returns the events it
        /// wrote.
        fn with(&self, tracker: &dyn Tracker, act: impl FnOnce(&mut Board<'_, '_>)) -> Vec<Event> {
            let log = EventLog::in_dir(self.tmp.path());
            let before = self.events().len();
            let mut out = Vec::new();
            let mut sink = EventSink::new("acme/app", log, &mut out);
            act(&mut Board {
                tracker,
                dirs: &self.dirs,
                sink: &mut sink,
            });
            self.events().split_off(before)
        }

        fn events(&self) -> Vec<Event> {
            fs::read_to_string(EventLog::in_dir(self.tmp.path()).path())
                .unwrap_or_default()
                .lines()
                .map(|line| Event::parse(line).unwrap())
                .collect()
        }
    }

    fn owl_7() -> TicketId {
        TicketId::new("OWL-7").unwrap()
    }

    const COLLISION: &str = "OWL-3 holds zone `backend/leases`, which it would touch";
    const SLOT: &str = "the project's slot is held by OWL-3";

    fn actions(events: &[Event]) -> Vec<String> {
        events
            .iter()
            .map(|e| {
                let what = e.data.get("what").or_else(|| e.data.get("action"));
                format!(
                    "{:?} {}",
                    e.kind,
                    what.and_then(Value::as_str).unwrap_or("")
                )
            })
            .collect()
    }

    /// One comment per reason: the same reason told again posts nothing; a
    /// new reason posts one and folds the one shown; the wait's end folds it
    /// and posts nothing, and a second end does nothing more.
    #[test]
    fn a_reason_is_shown_once_and_folded_when_it_changes_or_ends() {
        let bench = Bench::new();
        let tracker = Board2::new(true);
        let mut comments = WaitComments::default();
        let ticket = owl_7();
        let shown = bench.with(&tracker, |board| {
            comments.show(board, &ticket, COLLISION, COLLISION.to_owned());
            comments.show(board, &ticket, COLLISION, COLLISION.to_owned());
        });
        assert_eq!(actions(&shown), ["TrackerWrite comment"]);
        let posted = tracker.posted.borrow().clone();
        assert_eq!(posted.len(), 1);
        assert!(
            posted[0].starts_with("[owlshift] WAITING\n"),
            "{}",
            posted[0]
        );
        assert!(
            posted[0].contains(&format!(
                "This ticket waits: {COLLISION}. Nothing is needed"
            )),
            "{}",
            posted[0]
        );

        let changed = bench.with(&tracker, |board| {
            comments.show(board, &ticket, SLOT, SLOT.to_owned());
        });
        assert_eq!(
            actions(&changed),
            ["TrackerWrite comment", "TrackerWrite resolve"]
        );
        assert_eq!(changed[1].data["comment"], "c1");
        assert_eq!(changed[1].data["by"], "c2");
        assert_eq!(*tracker.resolved.borrow(), ["c1"]);

        let ended = bench.with(&tracker, |board| {
            comments.clear(board, &ticket);
            comments.clear(board, &ticket);
        });
        assert_eq!(actions(&ended), ["TrackerWrite resolve"]);
        assert_eq!(ended[0].data.get("by"), None);
        assert_eq!(*tracker.resolved.borrow(), ["c1", "c2"]);
        assert_eq!(tracker.posted.borrow().len(), 2);
        assert!(!path(&bench.dirs, &ticket).exists());
    }

    /// A watch started again reads what the last one showed: the same
    /// reason posts nothing, and the wait's end folds it, once.
    #[test]
    fn a_watch_started_again_posts_nothing_for_a_reason_shown_and_folds_it() {
        let bench = Bench::new();
        let tracker = Board2::new(true);
        let ticket = owl_7();
        bench.with(&tracker, |board| {
            WaitComments::default().show(board, &ticket, COLLISION, COLLISION.to_owned());
        });
        let mut again = WaitComments::default();
        let told = bench.with(&tracker, |board| {
            again.show(board, &ticket, COLLISION, COLLISION.to_owned());
        });
        assert!(told.is_empty(), "{told:?}");
        assert_eq!(tracker.posted.borrow().len(), 1);

        let mut third = WaitComments::default();
        bench.with(&tracker, |board| {
            third.clear(board, &ticket);
            WaitComments::default().clear(board, &ticket);
        });
        assert_eq!(*tracker.resolved.borrow(), ["c1"]);
    }

    /// A ticket that left the tickets in flight has its comment folded; one
    /// still in flight keeps it.
    #[test]
    fn a_ticket_that_left_has_its_comment_folded() {
        let bench = Bench::new();
        let tracker = Board2::new(true);
        let (left, kept) = (owl_7(), TicketId::new("OWL-8").unwrap());
        let mut comments = WaitComments::default();
        bench.with(&tracker, |board| {
            comments.show(board, &left, COLLISION, COLLISION.to_owned());
            comments.show(board, &kept, SLOT, SLOT.to_owned());
            comments.retain(board, std::slice::from_ref(&kept));
        });
        assert_eq!(*tracker.resolved.borrow(), ["c1"]);
        assert!(path(&bench.dirs, &kept).exists());
        assert!(!path(&bench.dirs, &left).exists());
    }

    /// A tracker that cannot fold is asked nothing, a change of reason still
    /// posts; a post that fails is a warning, the comment shown before
    /// folded all the same, and nothing is recorded as shown.
    #[test]
    fn a_tracker_that_cannot_fold_still_posts_and_a_failed_post_warns() {
        let bench = Bench::new();
        let plain = Board2::new(false);
        let ticket = owl_7();
        let mut comments = WaitComments::default();
        let told = bench.with(&plain, |board| {
            comments.show(board, &ticket, COLLISION, COLLISION.to_owned());
            comments.show(board, &ticket, SLOT, SLOT.to_owned());
            comments.clear(board, &ticket);
        });
        assert_eq!(
            actions(&told),
            ["TrackerWrite comment", "TrackerWrite comment"]
        );
        assert!(plain.resolved.borrow().is_empty());

        let mut down = Board2::new(true);
        let mut comments = WaitComments::default();
        bench.with(&down, |board| {
            comments.show(board, &ticket, COLLISION, COLLISION.to_owned());
        });
        down.refuse_posts = true;
        let told = bench.with(&down, |board| {
            comments.show(board, &ticket, SLOT, SLOT.to_owned());
        });
        assert_eq!(
            actions(&told),
            ["Warning wait_comment_not_posted", "TrackerWrite resolve"]
        );
        assert!(!path(&bench.dirs, &ticket).exists());
    }

    /// The comment stays far within a comment's limit, ten holders and a
    /// long path included (OWL-228).
    #[test]
    fn the_comment_stays_within_a_comments_limit() {
        let holders: Vec<String> = (1..=10).map(|n| format!("OWL-{n}")).collect();
        let phrase = format!(
            "{} hold zone `{}`, which it would touch",
            holders.join(", "),
            "deep/".repeat(200)
        );
        let body = WaitComment {
            ticket: owl_7(),
            phrase,
        }
        .render();
        assert!(
            body.chars().count() < crate::writer::MAX_COMMENT_CHARS / 10,
            "{}",
            body.len()
        );
    }
}
