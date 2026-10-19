//! Telling a quarantine (OWL-368; architecture section 8, "The quarantine";
//! build plan, "Telling a quarantine").
//!
//! A run that breaks isolation leaves the project's `unverified` marker, and
//! every command then refuses the whole project on this machine until a
//! person clears it ([`crate::project`]). This module says so to everyone it
//! stops, once each:
//!
//! - the operator, by the notifier: as the runner writes a breach marker
//!   ([`written_line`]), or when watch finds a marker no command notified,
//!   as a crash, a Ctrl-C or `owlshift gate` leaves ([`found_line`]);
//! - each ticket it holds up, by one [`QuarantineWait`] comment;
//! - `owlshift doctor`, through [`marker_state`];
//! - and [`clear`], `owlshift unquarantine`, lifts it once a person types the
//!   project's name.
//!
//! What was told of a marker is its [`Record`], beside it, written only under
//! the project's lock: so no later pass and no other process tells it again.
//! Every marker's text is unique (it names its run or the time it was
//! written), and a record is keyed by that text's hash.

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde_json::json;
use sha2::{Digest, Sha256};

use owlshift_adapters::notifier::Notifier;
use owlshift_adapters::tracker::Tracker;
use owlshift_contracts::comment::{Footer, Header, MarkerKind};
use owlshift_contracts::event::EventKind;
use owlshift_contracts::format::Format;
use owlshift_contracts::ids::TicketId;

use crate::events::{EventSink, data, printable};
use crate::executor::Violation;
use crate::notify;
use crate::on_demand;
use crate::project::{ProjectDirs, ProjectLock};
use crate::writer::Writer;

/// The breach of a kept worktree whose `.git` link changed between runs.
pub const LINK_CHANGED: &str = "the worktree's .git link changed";

/// The kinds of breach `violations` show, in fixed words, each once, in the
/// order found: never a path or a ref name, which a run chooses.
pub fn breach_kinds(violations: &[Violation]) -> Vec<&'static str> {
    let mut kinds = Vec::new();
    for violation in violations {
        // No wildcard: a new violation does not compile until it has words.
        let kind = match violation {
            Violation::MainHead | Violation::MainBranch | Violation::MainFiles(_) => {
                "the main checkout changed"
            }
            Violation::SharedGitFiles(_) => "the repository's shared git files changed",
            Violation::Refs(_) => "refs changed outside the run's branch",
            Violation::Branch { .. } => "the worktree changed branch",
            Violation::WorktreeLink(_) => LINK_CHANGED,
            Violation::History { .. } => "the branch no longer descends from the run's start",
            Violation::CheckFailed(_) => "the isolation check could not run",
        };
        if !kinds.contains(&kind) {
            kinds.push(kind);
        }
    }
    kinds
}

/// The sha256 of a marker's text, in hexadecimal: what its [`Record`] is
/// keyed by.
fn marker_hash(marker: &str) -> String {
    Sha256::digest(marker.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// What was told of one marker, kept in `unverified-told` beside it: its
/// text's hash on the first line, `notified` once the operator was, then a
/// `told <ticket>` line for each ticket told that it waits.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Record {
    pub notified: bool,
    pub told: BTreeSet<TicketId>,
}

impl Record {
    /// The record of the marker whose text is `marker`: empty when there is
    /// none, when it is another marker's, or when it cannot be read.
    pub fn read(dirs: &ProjectDirs, marker: &str) -> Self {
        let Ok(text) = fs::read_to_string(dirs.told_file()) else {
            return Self::default();
        };
        let mut lines = text.lines();
        if lines.next() != Some(marker_hash(marker).as_str()) {
            return Self::default();
        }
        let mut record = Self::default();
        for line in lines {
            if line == "notified" {
                record.notified = true;
            } else if let Some(ticket) = line.strip_prefix("told ")
                && let Ok(ticket) = TicketId::new(ticket)
            {
                record.told.insert(ticket);
            }
        }
        record
    }

    /// Adds `self` to the record of `marker`, under the project's lock
    /// `_lock`, through a temporary file renamed over it: nothing when the
    /// marker is gone, or is another one now.
    pub(crate) fn merge_into(
        &self,
        dirs: &ProjectDirs,
        _lock: &ProjectLock,
        marker: &str,
    ) -> io::Result<()> {
        if dirs.unverified()?.as_deref() != Some(marker) {
            return Ok(());
        }
        let mut record = Self::read(dirs, marker);
        record.notified |= self.notified;
        record.told.extend(self.told.iter().cloned());
        let mut text = format!("{}\n", marker_hash(marker));
        if record.notified {
            text.push_str("notified\n");
        }
        for ticket in &record.told {
            text.push_str(&format!("told {ticket}\n"));
        }
        let partial = dirs.root().join("unverified-told.partial");
        fs::write(&partial, text)?;
        fs::rename(&partial, dirs.told_file())
    }
}

/// The line of the breach marker `run` of `ticket` left, with `kinds` from
/// [`breach_kinds`]: fixed words, the project's name, the run and the
/// ticket only.
pub fn written_line(project: &str, ticket: &TicketId, run: &str, kinds: &[&str]) -> String {
    format!(
        "{project} is quarantined: run {run} of {ticket} broke isolation ({}). No ticket of the \
         project runs until a person clears it with owlshift unquarantine",
        kinds.join("; ")
    )
}

/// The line of a marker found that no command notified.
pub fn found_line(project: &str) -> String {
    format!(
        "{project} is quarantined: a run's isolation check did not pass or did not finish. No \
         ticket of the project runs until a person clears it: owlshift doctor shows it, owlshift \
         unquarantine clears it"
    )
}

/// Tells the operator that the breach marker `marker`, just written under
/// the project's lock `lock` after `run` of `ticket`, quarantines the
/// project, with `kinds`, then records that it did. Nothing is shown, nor
/// recorded, when this machine shows no notification (`notifier` `None`).
/// A line that cannot be shown is a `warning` event, and counts as notified
/// all the same.
#[allow(clippy::too_many_arguments)]
pub(crate) fn notify_written(
    notifier: Option<&dyn Notifier>,
    tracker: &dyn Tracker,
    dirs: &ProjectDirs,
    lock: &ProjectLock,
    marker: &str,
    ticket: &TicketId,
    run: &str,
    kinds: &[&str],
    sink: &mut EventSink<'_>,
) {
    let Some(notifier) = notifier else {
        return;
    };
    let line = written_line(sink.project(), ticket, run, kinds);
    if let Err(error) = notifier.notify(&notify::with_link(line, tracker, ticket)) {
        sink.emit(
            ticket,
            Some(run),
            EventKind::Warning,
            notify::not_shown(&error),
        );
    }
    let notified = Record {
        notified: true,
        told: BTreeSet::new(),
    };
    if let Err(error) = notified.merge_into(dirs, lock, marker) {
        sink.emit(ticket, Some(run), EventKind::Warning, not_recorded(&error));
    }
}

/// Tells the operator of the marker `marker`, read under the project's lock
/// `lock`, unless its record says it was notified, then records that it
/// was. Returns whether the operator was notified of it, now or before:
/// `false` when this machine shows no notification.
pub(crate) fn notify_found(
    notifier: Option<&dyn Notifier>,
    dirs: &ProjectDirs,
    lock: &ProjectLock,
    marker: &str,
    sink: &mut EventSink<'_>,
) -> bool {
    if Record::read(dirs, marker).notified {
        return true;
    }
    let Some(notifier) = notifier else {
        return false;
    };
    if let Err(error) = notifier.notify(&found_line(sink.project())) {
        sink.emit_project(None, EventKind::Warning, notify::not_shown(&error));
    }
    let notified = Record {
        notified: true,
        told: BTreeSet::new(),
    };
    if let Err(error) = notified.merge_into(dirs, lock, marker) {
        sink.emit_project(None, EventKind::Warning, not_recorded(&error));
    }
    true
}

fn not_recorded(error: &io::Error) -> crate::events::Data {
    data([
        ("what", json!("quarantine_not_recorded")),
        ("reason", json!(error.to_string())),
    ])
}

/// Tells `ticket`, which the quarantine holds up as `held` says, that it
/// waits: one `decision` event (`watch` or `drain` = `waits`, `reason` =
/// `quarantine`) and one WAITING comment, its `tracker_write` event, or a
/// `warning` when it could not be posted. The caller tells each ticket once
/// per marker.
pub(crate) fn tell(tracker: &dyn Tracker, ticket: &TicketId, held: Held, sink: &mut EventSink<'_>) {
    let by = match held {
        Held::Answer => "watch",
        Held::Ready => "drain",
    };
    sink.emit(
        ticket,
        None,
        EventKind::Decision,
        data([(by, json!("waits")), ("reason", json!("quarantine"))]),
    );
    let comment = QuarantineWait {
        ticket: ticket.clone(),
        held,
    };
    let (kind, written) = match Writer::new(tracker).post_quarantine_wait(&comment) {
        Ok(posted) => (
            EventKind::TrackerWrite,
            on_demand::comment_written("WAITING", &posted),
        ),
        Err(error) => (
            EventKind::Warning,
            data([
                ("what", json!("quarantine_comment_not_posted")),
                ("reason", json!(error.to_string())),
            ]),
        ),
    };
    sink.emit(ticket, None, kind, written);
}

/// What a ticket held up by a quarantine waits to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Held {
    /// Its decider's answer counts: watch would continue it.
    Answer,
    /// It is ready: the drain would build it.
    Ready,
}

/// The WAITING comment of a ticket a quarantine holds up: fixed words, no
/// mention, nothing of the run that broke isolation, whose details describe
/// the runner's machine and stay there.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuarantineWait {
    pub ticket: TicketId,
    pub held: Held,
}

impl QuarantineWait {
    pub fn render(&self) -> String {
        let header = Header {
            kind: MarkerKind::Waiting,
            round: None,
        };
        let why = "a run's isolation check did not pass on the machine that runs Owlshift for \
                   this project, which is quarantined there until a person checks what the run \
                   changed and clears it with `owlshift unquarantine`";
        let text = match self.held {
            Held::Answer => format!(
                "Your answer counts, but Owlshift continues no ticket of this project for now: \
                 {why}. Nothing more is needed from you: this ticket continues by itself once \
                 the quarantine is cleared."
            ),
            Held::Ready => format!(
                "This ticket is ready, but Owlshift builds no ticket of this project for now: \
                 {why}. It is built once the quarantine is cleared."
            ),
        };
        let footer = Footer {
            format: Format,
            kind: MarkerKind::Waiting,
            ticket: self.ticket.clone(),
            round: None,
            run: None,
        };
        format!("{}\n\n{text}\n\n{}\n", header.render(), footer.render())
    }
}

/// Where the project's marker stands, for `owlshift doctor`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MarkerState {
    /// No marker: the project is not quarantined.
    Clear,
    /// A marker, while another command holds the project's lock: every run
    /// writes one as it starts, so it may only say that a run is in flight.
    InFlight,
    /// A marker with the lock free: the project is quarantined.
    Marked { path: PathBuf, text: String },
}

/// Where the marker of the project at `dirs` stands: read, then, when there
/// is one, read again under the project's lock, tried and released at once.
pub fn marker_state(dirs: &ProjectDirs) -> io::Result<MarkerState> {
    if dirs.unverified()?.is_none() {
        return Ok(MarkerState::Clear);
    }
    let Some(_lock) = dirs.lock()? else {
        return Ok(MarkerState::InFlight);
    };
    Ok(match dirs.unverified()? {
        None => MarkerState::Clear,
        Some(text) => MarkerState::Marked {
            path: dirs.unverified_file(),
            text,
        },
    })
}

/// What [`clear`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Cleared {
    /// There was no marker.
    Nothing,
    /// The answer was not the project's name: the quarantine stands.
    Kept,
    /// The marker, whose text this was, is gone.
    Cleared { marker: String },
}

/// The project's lock, or why not.
fn locked(dirs: &ProjectDirs) -> Result<ProjectLock, String> {
    match dirs.lock() {
        Ok(Some(lock)) => Ok(lock),
        Ok(None) => Err(format!(
            "another `owlshift` command is working this project ({}), and a run writes the marker \
             as it starts: run `owlshift unquarantine` again once it ends",
            dirs.root().display()
        )),
        Err(error) => Err(format!("the project's lock: {error}")),
    }
}

/// `owlshift unquarantine`: shows the marker of the project at `dirs`, whose
/// name the events record (`sink`), through `ask`, which prints its prompt
/// and returns the line a person typed, and clears the marker and its record
/// only if that line is the project's name. The marker is read under the
/// project's lock, released while the person answers, then taken again: the
/// marker is cleared only if it is still the one shown. One project-level
/// `decision` event (`unquarantined`, with the marker's text) records it.
pub fn clear(
    dirs: &ProjectDirs,
    ask: &mut dyn FnMut(&str) -> io::Result<String>,
    sink: &mut EventSink<'_>,
) -> Result<Cleared, String> {
    let marker_error = |error: io::Error| format!("the project's marker: {error}");
    // A project never run has no folder, which the lock would make. Only a
    // marker missing is no quarantine: one that cannot be looked at is.
    match fs::symlink_metadata(dirs.unverified_file()) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Cleared::Nothing),
        Err(error) => return Err(marker_error(error)),
    }
    let marker = {
        let _lock = locked(dirs)?;
        match dirs.unverified().map_err(marker_error)? {
            Some(marker) => marker,
            None => return Ok(Cleared::Nothing),
        }
    };
    let project = sink.project().to_owned();
    let answer = ask(&prompt(dirs, &project, &marker))
        .map_err(|error| format!("reading the answer: {error}"))?;
    if answer.trim() != project {
        return Ok(Cleared::Kept);
    }
    let _lock = locked(dirs)?;
    match dirs.unverified().map_err(marker_error)? {
        Some(now) if now == marker => {}
        None => return Ok(Cleared::Nothing),
        Some(_) => {
            return Err(
                "the marker changed while you answered: run `owlshift unquarantine` again to see \
                 what it says now"
                    .to_owned(),
            );
        }
    }
    dirs.clear_unverified().map_err(marker_error)?;
    sink.emit_project(
        None,
        EventKind::Decision,
        data([("unquarantined", json!(marker))]),
    );
    Ok(Cleared::Cleared { marker })
}

/// What `owlshift unquarantine` shows before it asks.
fn prompt(dirs: &ProjectDirs, project: &str, marker: &str) -> String {
    let shown = |path: &Path| path.display().to_string();
    let text: String = marker
        .trim_end()
        .lines()
        .map(|line| format!("  {line}\n"))
        .collect();
    printable(&format!(
        "{project} is quarantined: Owlshift refuses it on this machine. Its marker, {}, says:\n\
         {text}\
         Clearing it keeps the dedicated checkout ({}) and the worktrees ({}) as they are: \
         whatever the run changed there stays, and the next runs start from it. To start from a \
         fresh clone instead, keep the quarantine, delete those two folders (the ticket refs go \
         with the checkout), then run `owlshift unquarantine` again.\n\
         Type {project} to clear the quarantine, anything else to keep it: ",
        shown(&dirs.unverified_file()),
        shown(&dirs.checkout()),
        shown(&dirs.worktrees()),
    ))
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use owlshift_adapters::forge::Repo;
    use owlshift_adapters::notifier::Error;
    use owlshift_adapters::tracker::{
        Capability, Comment, Error as TrackerError, Ticket as TrackerTicket,
    };
    use owlshift_contracts::comment::MarkedComment;
    use owlshift_contracts::event::Event;

    use super::*;
    use crate::events::EventLog;

    #[derive(Default)]
    struct Recording(RefCell<Vec<String>>);

    impl Notifier for Recording {
        fn notify(&self, line: &str) -> Result<(), Error> {
            self.0.borrow_mut().push(line.to_owned());
            Ok(())
        }
    }

    /// A tracker asked for a ticket's link alone.
    struct Linkless;

    impl Tracker for Linkless {
        fn capabilities(&self) -> &'static [Capability] {
            &[]
        }
        fn ticket(&self, _: &TicketId) -> Result<TrackerTicket, TrackerError> {
            unreachable!()
        }
        fn comments(&self, _: &TicketId) -> Result<Vec<Comment>, TrackerError> {
            unreachable!()
        }
        fn post_comment(&self, _: &TicketId, _: &str) -> Result<Comment, TrackerError> {
            unreachable!()
        }
        fn set_stage(&self, _: &TicketId, _: &str) -> Result<(), TrackerError> {
            unreachable!()
        }
    }

    struct Bench {
        _tmp: tempfile::TempDir,
        dirs: ProjectDirs,
        log: EventLog,
    }

    impl Bench {
        fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let dirs = ProjectDirs::new(tmp.path(), &Repo::parse("acme/app").unwrap());
            fs::create_dir_all(dirs.root()).unwrap();
            let log = EventLog::in_dir(tmp.path());
            Self {
                _tmp: tmp,
                dirs,
                log,
            }
        }

        fn mark(&self, text: &str) {
            self.dirs.mark_unverified(text).unwrap();
        }

        fn events(&self) -> Vec<Event> {
            fs::read_to_string(self.log.path())
                .unwrap_or_default()
                .lines()
                .map(|line| Event::parse(line).unwrap())
                .collect()
        }
    }

    fn owl_7() -> TicketId {
        TicketId::new("OWL-7").unwrap()
    }

    const BREACH: &str = "Run r1 of OWL-7 broke isolation:\n- files changed in the main checkout: \
                          evil.txt\n";

    #[test]
    fn a_breach_is_told_in_fixed_words_each_once() {
        let kinds = breach_kinds(&[
            Violation::MainFiles(vec!["evil\u{1b}[2J.txt".to_owned()]),
            Violation::MainHead,
            Violation::Refs(vec!["refs/heads/x".to_owned()]),
            Violation::WorktreeLink("its link points at /tmp/evil".to_owned()),
        ]);
        assert_eq!(
            kinds,
            [
                "the main checkout changed",
                "refs changed outside the run's branch",
                LINK_CHANGED
            ]
        );
        let line = written_line("acme/app", &owl_7(), "r1", &kinds);
        assert_eq!(
            line,
            "acme/app is quarantined: run r1 of OWL-7 broke isolation (the main checkout changed; \
             refs changed outside the run's branch; the worktree's .git link changed). No ticket \
             of the project runs until a person clears it with owlshift unquarantine"
        );
        for chosen in ["evil", "refs/heads/x", "/tmp/evil"] {
            assert!(!line.contains(chosen), "{line}");
        }
    }

    /// A breach marker is notified once as it is written; the passes that
    /// find it later stay silent. A marker no command notified is notified
    /// once by the first that finds it. A machine with no notifier records
    /// nothing, so a later one that has one still notifies.
    #[test]
    fn each_marker_is_notified_once() {
        let bench = Bench::new();
        let dirs = &bench.dirs;
        let recording = Recording::default();
        let mut out = Vec::new();
        let mut sink = EventSink::new("acme/app", bench.log.clone(), &mut out);
        bench.mark(BREACH);
        let lock = dirs.lock().unwrap().unwrap();
        let kinds = ["the main checkout changed"];
        notify_written(
            Some(&recording),
            &Linkless,
            dirs,
            &lock,
            BREACH,
            &owl_7(),
            "r1",
            &kinds,
            &mut sink,
        );
        for _ in 0..2 {
            assert!(notify_found(
                Some(&recording),
                dirs,
                &lock,
                BREACH,
                &mut sink
            ));
        }
        assert_eq!(recording.0.borrow().len(), 1);

        // A cut-off run's marker, found by watch with no notifier, then
        // with one, then again.
        let cut = "Run r2 of OWL-7 started at 2026-10-19T00:00:00Z; its isolation check has not \
                   passed.\n";
        bench.mark(cut);
        assert!(!notify_found(None, dirs, &lock, cut, &mut sink));
        assert!(!Record::read(dirs, cut).notified);
        assert!(notify_found(Some(&recording), dirs, &lock, cut, &mut sink));
        assert!(notify_found(Some(&recording), dirs, &lock, cut, &mut sink));
        assert_eq!(
            recording.0.borrow()[1..],
            [found_line("acme/app")],
            "{:?}",
            recording.0
        );
        // The breach's record is another marker's now.
        assert_eq!(Record::read(dirs, BREACH), Record::default());
        drop(sink);
        assert!(bench.events().is_empty(), "{:?}", bench.events());
    }

    /// The told tickets join what the record holds, and nothing is written
    /// for a marker that is no longer the project's.
    #[test]
    fn a_record_keeps_its_marker_alone() {
        let bench = Bench::new();
        let dirs = &bench.dirs;
        bench.mark(BREACH);
        let lock = dirs.lock().unwrap().unwrap();
        let told = |ids: &[&str]| Record {
            notified: false,
            told: ids.iter().map(|id| TicketId::new(*id).unwrap()).collect(),
        };
        told(&["OWL-1"]).merge_into(dirs, &lock, BREACH).unwrap();
        told(&["OWL-2"]).merge_into(dirs, &lock, BREACH).unwrap();
        assert_eq!(Record::read(dirs, BREACH), told(&["OWL-1", "OWL-2"]));
        // Another marker: the old record reads empty and is not added to.
        told(&["OWL-3"]).merge_into(dirs, &lock, "other").unwrap();
        assert_eq!(Record::read(dirs, BREACH), told(&["OWL-1", "OWL-2"]));
        assert_eq!(Record::read(dirs, "other"), Record::default());
        // A passed check removes both.
        dirs.clear_unverified().unwrap();
        assert!(!dirs.told_file().exists());
    }

    #[test]
    fn a_waiting_comment_is_marked_and_says_why() {
        for held in [Held::Answer, Held::Ready] {
            let body = QuarantineWait {
                ticket: owl_7(),
                held,
            }
            .render();
            let marked = MarkedComment::parse(&body).unwrap().unwrap();
            assert_eq!(marked.header.kind, MarkerKind::Waiting);
            assert_eq!(marked.footer.unwrap().ticket, owl_7());
            assert!(body.contains("`owlshift unquarantine`"), "{body}");
        }
    }

    /// The marker is cleared only by the project's name, typed while the
    /// project is free; no marker is nothing to clear.
    #[test]
    fn only_the_projects_name_clears_the_quarantine() {
        let cases: [(&str, Option<&str>, bool, &str); 5] = [
            ("cleared", Some(BREACH), false, "acme/app\n"),
            ("kept", Some(BREACH), false, "yes\n"),
            ("kept, a near name", Some(BREACH), false, "acme/app2\n"),
            ("nothing", None, false, "acme/app\n"),
            ("busy", Some(BREACH), true, "acme/app\n"),
        ];
        for (case, marker, busy, typed) in cases {
            let bench = Bench::new();
            if let Some(marker) = marker {
                bench.mark(marker);
                Record {
                    notified: true,
                    told: BTreeSet::new(),
                }
                .merge_into(&bench.dirs, &bench.dirs.lock().unwrap().unwrap(), marker)
                .unwrap();
            }
            let held = busy.then(|| bench.dirs.lock().unwrap().unwrap());
            let mut shown = Vec::new();
            let mut ask = |prompt: &str| {
                shown.push(prompt.to_owned());
                Ok(typed.to_owned())
            };
            let mut out = Vec::new();
            let mut sink = EventSink::new("acme/app", bench.log.clone(), &mut out);
            let cleared = clear(&bench.dirs, &mut ask, &mut sink);
            drop(sink);
            drop(held);
            let left = bench.dirs.unverified().unwrap();
            match case {
                "cleared" => {
                    assert_eq!(
                        cleared,
                        Ok(Cleared::Cleared {
                            marker: BREACH.to_owned()
                        })
                    );
                    assert_eq!(left, None);
                    assert!(!bench.dirs.told_file().exists());
                    assert!(shown[0].contains("evil.txt"), "{shown:?}");
                    assert!(shown[0].ends_with(
                        "Type acme/app to clear the quarantine, anything else to keep it: "
                    ));
                    let events = bench.events();
                    assert_eq!(events.len(), 1, "{events:?}");
                    assert_eq!(events[0].ticket, None);
                    assert_eq!(events[0].data["unquarantined"], json!(BREACH));
                }
                "nothing" => {
                    assert_eq!(cleared, Ok(Cleared::Nothing));
                    assert!(shown.is_empty());
                }
                "busy" => {
                    assert!(cleared.unwrap_err().contains("another `owlshift` command"));
                    assert!(shown.is_empty());
                    assert_eq!(left.as_deref(), Some(BREACH));
                }
                _ => {
                    assert_eq!(cleared, Ok(Cleared::Kept), "{case}");
                    assert_eq!(left.as_deref(), Some(BREACH), "{case}");
                    assert!(Record::read(&bench.dirs, BREACH).notified, "{case}");
                    assert!(bench.events().is_empty(), "{case}");
                }
            }
        }
    }

    /// A marker that cannot be looked at is a refusal, never "nothing to
    /// clear" (OWL-368, Codex P2).
    #[cfg(unix)]
    #[test]
    fn a_marker_that_cannot_be_looked_at_is_no_clear_quarantine() {
        use std::os::unix::fs::PermissionsExt;

        let bench = Bench::new();
        bench.mark(BREACH);
        let root = bench.dirs.root();
        fs::set_permissions(root, fs::Permissions::from_mode(0o000)).unwrap();
        // A superuser looks through any folder: nothing to check then.
        let superuser = fs::read_dir(root).is_ok();
        let mut ask = |_: &str| -> io::Result<String> { unreachable!("nothing is shown") };
        let mut out = Vec::new();
        let mut sink = EventSink::new("acme/app", bench.log.clone(), &mut out);
        let cleared = clear(&bench.dirs, &mut ask, &mut sink);
        fs::set_permissions(root, fs::Permissions::from_mode(0o700)).unwrap();
        if !superuser {
            let refused = cleared.unwrap_err();
            assert!(refused.starts_with("the project's marker: "), "{refused}");
        }
        assert_eq!(bench.dirs.unverified().unwrap().as_deref(), Some(BREACH));
    }

    /// A marker the person did not see is never cleared: one written while
    /// they answered refuses the clear.
    #[test]
    fn a_marker_changed_while_the_person_answers_is_kept() {
        let bench = Bench::new();
        bench.mark(BREACH);
        let mut ask = |_: &str| {
            bench.mark("Run r9 of OWL-8 broke isolation:\n- the main checkout's HEAD moved\n");
            Ok("acme/app".to_owned())
        };
        let mut out = Vec::new();
        let mut sink = EventSink::new("acme/app", bench.log.clone(), &mut out);
        let refused = clear(&bench.dirs, &mut ask, &mut sink).unwrap_err();
        assert!(refused.contains("changed while you answered"), "{refused}");
        assert!(bench.dirs.unverified().unwrap().is_some());
    }

    #[test]
    fn doctor_tells_a_quarantine_from_a_run_in_flight() {
        let bench = Bench::new();
        assert_eq!(marker_state(&bench.dirs).unwrap(), MarkerState::Clear);
        bench.mark(BREACH);
        assert_eq!(
            marker_state(&bench.dirs).unwrap(),
            MarkerState::Marked {
                path: bench.dirs.unverified_file(),
                text: BREACH.to_owned()
            }
        );
        let _held = bench.dirs.lock().unwrap().unwrap();
        assert_eq!(marker_state(&bench.dirs).unwrap(), MarkerState::InFlight);
    }
}
