//! `owlshift status` (OWL-438, architecture section 5, "`owlshift status`"):
//! what the background service serves and does, read from its files and
//! nothing else, with no channel to the daemon.
//!
//! It reads `<data>/service/daemon.json`, `projects.json`, the stop request
//! and the pauses, each served project's `served.json`, stop file and
//! ticket refs, the run slots' holders, the usage files, `digest.json`, and
//! the daemon's `watch_started` and `watch_ended` events, from which a
//! watch is running or restarting and why. It waits on no lock: whether a
//! daemon runs is the instant probe of its lock (`daemon::running`), as
//! `owlshift start` and `stop` read it, and the slots are counted as
//! `owlshift why` counts them, each free one tried for a moment.
//!
//! Since OWL-478 it also shows each served project's standing quarantine,
//! and the steps no run slot names: a ticket lock a command holds
//! ([`ProjectDirs::ticket_holders`]) whose holder holds no slot, such as a
//! `do` or a `continue` a person typed in a terminal.
//!
//! Since OWL-464 each served project lists its green pull requests that
//! wait for a person to merge them ([`merge_waits`]), from watch's
//! `merge_waits` and final `merge_refused` events and the ticket refs.
//!
//! [`gather`] does the reading; [`watch_state`] and [`render`] are pure.
//! [`gather_files`] reads all of it but the event log, for the web UI's
//! header (OWL-467), which reads no event log.
//! `owlshift status --waits`, what each ticket in flight waits on, is
//! [`waits`] (OWL-459).

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use jiff::Timestamp;
use jiff::tz::TimeZone;
use owlshift_adapters::forge::Repo;
use owlshift_contracts::event::{Event, EventKind};
use owlshift_contracts::ids::TicketId;
use owlshift_contracts::service::{
    BY_DAEMON, DaemonRecord, DigestRecord, Request, ServedConfig, ServedProject,
};
use owlshift_core::state::{Status, TicketState};
use owlshift_core::vocab::{Harness, Stage};
use serde_json::Value;

use crate::daemon::digest::{self, HarnessUsage};
use crate::daemon::{self, requests};
use crate::events::{EventLog, printable};
use crate::project::{ProjectDirs, TicketHolder};
use crate::served::{self, Registry};
use crate::slots::{HeldSlot, MachineSlots};
use crate::voice::Voice;
use crate::{ticket_ref, watch, why};

pub mod waits;

/// Everything `owlshift status` shows. [`gather_files`] leaves unread what
/// only the event log tells: each project's watch is [`Watch::NotRead`]
/// while a daemon runs, and `merges_waiting` and `drain_steps` are empty.
#[derive(Clone, Debug)]
pub struct Report {
    pub daemon: Daemon,
    /// The daemon's stop request, while it stands: the daemon drains.
    pub stop_asked: Option<Request>,
    /// The upgrade's drain request, while it stands: the daemon exits once
    /// its steps under way finish (OWL-481).
    pub drain_asked: Option<Request>,
    /// While a running daemon drains, for a stop or an upgrade, the steps it
    /// waits on, one per held run slot (OWL-481).
    pub drain_steps: Vec<DrainStep>,
    /// The pause of every project, while it stands; `None` within for a
    /// file that does not read.
    pub machine_pause: Option<Option<Request>>,
    pub projects: Vec<Project>,
    /// The machine's run slots held now, with their holders, and
    /// `concurrent_runs`; `None` when they cannot be probed.
    pub slots: Option<(Vec<HeldSlot>, u32)>,
    /// The ticket locks commands hold now whose holder holds no run slot
    /// (OWL-478): steps typed in a terminal. Not looked for when the slots
    /// cannot be probed, since a slot's holder would then read as typed.
    pub typed: Vec<TypedStep>,
    pub usage: Vec<(Harness, HarnessUsage)>,
    /// The latest digest's record.
    pub digest: Option<DigestRecord>,
    /// What could not be read.
    pub notes: Vec<String>,
}

/// A step a draining daemon waits on (OWL-481): a held run slot's holder,
/// with the step's latest run as the event log tells it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DrainStep {
    pub project: String,
    pub ticket: TicketId,
    /// The role and start of the step's latest `run_started` event since the
    /// slot was taken, `None` while it has none. A review loop starts runs
    /// inside one step, so this is not the slot's `since`.
    pub run: Option<(String, Timestamp)>,
}

/// A step a command typed in a terminal runs (OWL-478): its ticket's lock
/// is held, and its holder holds no run slot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TypedStep {
    /// The project, its repository when it is served, its folder's
    /// `owner/name` otherwise.
    pub project: String,
    pub holder: TicketHolder,
}

/// Whether a daemon runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Daemon {
    NotRunning,
    /// One holds the lock: its record, `None` when it does not read.
    Running(Option<DaemonRecord>),
    /// Its lock could not be probed, for this reason.
    Unknown(String),
}

/// One served project.
#[derive(Clone, Debug)]
pub struct Project {
    /// Its repository, as `projects.json` lists it.
    pub repository: String,
    /// The checkout it was registered from.
    pub checkout: String,
    /// The project file it runs on, or why none is: its `served.json`, an
    /// empty one while its watch has read nothing; why it cannot be read.
    pub config: Result<ServedConfig, String>,
    pub watch: Watch,
    /// Its own pause, while it stands; `None` within for a file that does
    /// not read.
    pub pause: Option<Option<Request>>,
    /// Whether its stop file stands: its watch drains.
    pub draining: bool,
    /// Its quarantine's first line while one stands (OWL-478), or why its
    /// file cannot be read.
    pub quarantine: Option<Result<String, String>>,
    /// The tickets whose questions wait for a person, and the parked ones.
    pub waiting: Vec<TicketId>,
    pub parked: Vec<TicketId>,
    /// The tickets at Watch whose pull request watch follows: for a person
    /// to review (OWL-467). The web UI's header counts them; `render` does
    /// not show them.
    pub in_review: Vec<TicketId>,
    /// Its green pull requests that wait for a person to merge them
    /// (OWL-464), by [`merge_waits`].
    pub merges_waiting: Vec<MergeWait>,
}

/// A green pull request merge on green leaves for a person (OWL-464;
/// architecture section 5, "Merge on green", "Kept for a person").
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MergeWait {
    pub ticket: TicketId,
    pub pull_request: u64,
    /// Why, in a person's words: the hold's, or GitHub's own reason for a
    /// refusal that will not pass.
    pub reason: String,
    /// GitHub refused the merge for good (`merge_refused`).
    pub refused: bool,
}

/// The green pull requests of `project` that wait for a person (OWL-464):
/// for each ticket, the latest `merge_waits` or final `merge_refused`
/// decision of the events, oldest first, kept while `pushed_of`, the head
/// of Owlshift's last push its ticket ref keeps at Watch with its pull
/// request followed, is that event's head. `pushed_of` is asked only for a
/// ticket with such an event.
pub fn merge_waits(
    events: &[Event],
    project: &str,
    mut pushed_of: impl FnMut(&TicketId) -> Option<String>,
) -> Vec<MergeWait> {
    let mut latest: Vec<&Event> = Vec::new();
    for event in events {
        let watch = event.data.get("watch").and_then(Value::as_str);
        let told = match watch {
            Some("merge_waits") => true,
            Some("merge_refused") => event.data.get("final").and_then(Value::as_bool) == Some(true),
            _ => false,
        };
        if event.kind != EventKind::Decision
            || !told
            || !event.project.eq_ignore_ascii_case(project)
            || event.ticket.is_none()
        {
            continue;
        }
        latest.retain(|kept| kept.ticket != event.ticket);
        latest.push(event);
    }
    latest
        .into_iter()
        .filter_map(|event| {
            let ticket = event.ticket.clone()?;
            let head = event.data.get("head").and_then(Value::as_str)?;
            if pushed_of(&ticket).as_deref() != Some(head) {
                return None;
            }
            Some(MergeWait {
                ticket,
                pull_request: event.data.get("pull_request").and_then(Value::as_u64)?,
                reason: event
                    .data
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("no reason was recorded")
                    .to_owned(),
                refused: event.data.get("watch").and_then(Value::as_str) == Some("merge_refused"),
            })
        })
        .collect()
}

/// The head of Owlshift's last push to `ticket`'s pull request, while its
/// ref keeps it at Watch with that pull request followed; `None` otherwise,
/// a ref that cannot be read said in `notes`.
fn followed_head(dirs: &ProjectDirs, ticket: &TicketId, notes: &mut Vec<String>) -> Option<String> {
    let stored = match ticket_ref::read(&crate::project::runner_git(), &dirs.checkout(), ticket) {
        Ok(stored) => stored?,
        Err(why) => {
            notes.push(format!("the ref of {ticket} cannot be read: {why}"));
            return None;
        }
    };
    let at_watch = TicketState::try_from(&stored.record.state)
        .is_ok_and(|state| state.status() == Status::Active(Stage::Watch));
    stored
        .record
        .delivery
        .filter(|delivery| at_watch && delivery.followed())
        .and_then(|delivery| delivery.pushed)
}

/// A served project's watch, as the daemon's events tell it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Watch {
    /// No daemon runs, so no watch is served.
    NoDaemon,
    /// A daemon runs, and the event log that tells its watch was not read:
    /// [`gather_files`]'s, never [`gather`]'s (OWL-467).
    NotRead,
    /// The daemon running now has not started it yet.
    NotStarted,
    /// Started at `since`, process `pid`.
    Running { pid: Option<u64>, since: Timestamp },
    /// It ended without being asked at `ended`, and starts again at `at`.
    Restarting {
        ended: Timestamp,
        at: Option<Timestamp>,
        why: String,
        in_a_row: Option<u64>,
    },
    /// It ended at `at` on a refusal no restart mends: the daemon paused
    /// the project, and tries it again at each of its passes.
    Unservable { at: Timestamp, why: String },
    /// It ended as asked, at `at`.
    Ended { at: Timestamp },
}

/// The watch of `project` the events say, oldest first, since `since`, the
/// start of the daemon running now: the latest `watch_started` or
/// `watch_ended` decision of the daemon's on that project.
pub fn watch_state(events: &[Event], project: &str, since: Timestamp) -> Watch {
    let latest = events.iter().rev().find(|event| {
        event.kind == EventKind::Decision
            && event.at >= since
            && event.project.eq_ignore_ascii_case(project)
            && matches!(
                event.data.get("service").and_then(Value::as_str),
                Some("watch_started" | "watch_ended")
            )
    });
    let Some(event) = latest else {
        return Watch::NotStarted;
    };
    let text = |key: &str| {
        event
            .data
            .get(key)
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
    };
    let number = |key: &str| event.data.get(key).and_then(Value::as_u64);
    if event.data.get("service").and_then(Value::as_str) == Some("watch_started") {
        return Watch::Running {
            pid: number("pid"),
            since: event.at,
        };
    }
    if let Some(why) = text("unservable") {
        return Watch::Unservable { at: event.at, why };
    }
    if event.data.get("asked").and_then(Value::as_bool) == Some(true) {
        return Watch::Ended { at: event.at };
    }
    let at = number("restart_in_ms")
        .and_then(|ms| i64::try_from(ms).ok())
        .and_then(|ms| {
            event
                .at
                .checked_add(jiff::SignedDuration::from_millis(ms))
                .ok()
        });
    Watch::Restarting {
        ended: event.at,
        at,
        why: text("end").unwrap_or_else(|| "it ended".to_owned()),
        in_a_row: number("in_a_row"),
    }
}

/// Every event of the data directory `data`'s log, oldest first; what
/// cannot be read is said in `notes`.
pub(crate) fn read_events(data: &Path, notes: &mut Vec<String>) -> Vec<Event> {
    match why::read_events(EventLog::in_dir(data).path()) {
        Ok((events, bad)) => {
            notes.extend(bad.into_iter().map(|why| format!("the event log's {why}")));
            events
        }
        Err(error) => {
            notes.push(format!("the event log cannot be read: {error}"));
            Vec::new()
        }
    }
}

/// The projects `projects.json` of `data` lists, each with its repository;
/// what cannot be read is said in `notes`.
pub(crate) fn served_projects(data: &Path, notes: &mut Vec<String>) -> Vec<(ServedProject, Repo)> {
    let listed = match Registry::in_data_dir(data).read() {
        Ok(served) => served.projects,
        Err(error) => {
            notes.push(format!("the served projects cannot be read: {error}"));
            Vec::new()
        }
    };
    listed
        .into_iter()
        .filter_map(|entry| match Repo::parse(&entry.repository) {
            Ok(repo) => Some((entry, repo)),
            Err(error) => {
                notes.push(format!("{} is not a repository: {error}", entry.repository));
                None
            }
        })
        .collect()
}

/// Reads what `owlshift status` shows of the data directory `data`, its
/// slots counted up to `concurrent_runs`, its harnesses' caps `caps`, at
/// `now`: [`gather_files`], then the event log's part.
pub fn gather(data: &Path, concurrent_runs: u32, caps: &[(Harness, u8)], now: Timestamp) -> Report {
    let mut report = gather_files(data, concurrent_runs, caps, now);
    let events = read_events(data, &mut report.notes);
    // A record that does not read leaves the daemon's start unknown: every
    // event counts.
    let since = match &report.daemon {
        Daemon::Running(record) => Some(
            record
                .as_ref()
                .map_or(Timestamp::MIN, |record| record.since),
        ),
        _ => None,
    };
    for project in &mut report.projects {
        if let Some(since) = since {
            project.watch = watch_state(&events, &project.repository, since);
        }
        let Ok(repo) = Repo::parse(&project.repository) else {
            continue;
        };
        let dirs = ProjectDirs::new(data, &repo);
        project.merges_waiting = merge_waits(&events, &project.repository, |ticket| {
            followed_head(&dirs, ticket, &mut report.notes)
        });
    }
    report.drain_steps = match (&report.daemon, &report.slots) {
        (Daemon::Running(_), Some((held, _)))
            if report.stop_asked.is_some() || report.drain_asked.is_some() =>
        {
            drain_steps(held, &events)
        }
        _ => Vec::new(),
    };
    report
}

/// What [`gather`] reads but the event log, which it never opens
/// (OWL-467): the daemon, the served projects and their records, ticket
/// refs, pauses, stop files and quarantines, the stop and drain requests,
/// the slots and the steps typed in a terminal, the usage and the digest.
/// Each project's watch is [`Watch::NoDaemon`] when no daemon runs and
/// [`Watch::NotRead`] otherwise; no merge waits and no drain steps are
/// read.
pub fn gather_files(
    data: &Path,
    concurrent_runs: u32,
    caps: &[(Harness, u8)],
    now: Timestamp,
) -> Report {
    let mut notes = Vec::new();
    let daemon = match daemon::running(data) {
        Ok(None) => Daemon::NotRunning,
        Ok(Some(record)) => Daemon::Running(record),
        Err(error) => Daemon::Unknown(error.to_string()),
    };
    let mut projects = Vec::new();
    let mut named = Vec::new();
    for (entry, repo) in served_projects(data, &mut notes) {
        let dirs = ProjectDirs::new(data, &repo);
        named.push((dirs.root().to_owned(), entry.repository.clone()));
        let config = served::read_record(&dirs)
            .map(|record| record.unwrap_or_else(|| ServedConfig::new(repo.to_string())));
        let watch = match &daemon {
            Daemon::Running(_) => Watch::NotRead,
            _ => Watch::NoDaemon,
        };
        let (waiting, parked, in_review) = sorted(why::standings(&dirs, &mut notes));
        projects.push(Project {
            repository: entry.repository,
            checkout: entry.checkout,
            config,
            watch,
            pause: requests::pause(&dirs),
            draining: watch::stop::stands(&dirs),
            quarantine: quarantine_line(&dirs),
            waiting,
            parked,
            in_review,
            merges_waiting: Vec::new(),
        });
    }
    let slots = match MachineSlots::new(data, concurrent_runs).holders() {
        Ok(held) => Some((held, concurrent_runs)),
        Err(error) => {
            notes.push(format!("the run slots cannot be probed: {error}"));
            None
        }
    };
    let typed = match &slots {
        Some((held, _)) => typed_steps(data, &named, held, &mut notes),
        None => Vec::new(),
    };
    let usage = digest::usage(data, caps, now, &mut notes);
    let digest = match digest::read(data) {
        Ok(record) => record,
        Err(digest::Unread::Newer) => {
            notes.push("the digest's record is of a newer Owlshift's format".to_owned());
            None
        }
        Err(digest::Unread::Bad(why)) => {
            notes.push(format!("the digest's record does not read: {why}"));
            None
        }
    };
    Report {
        daemon,
        stop_asked: requests::stop_asked(data),
        drain_asked: requests::drain_asked(data),
        drain_steps: Vec::new(),
        machine_pause: requests::machine_pause(data),
        projects,
        slots,
        typed,
        usage,
        digest,
        notes,
    }
}

/// The tickets of `standings` whose questions wait for a person, the parked
/// ones, and those whose pull request watch follows, for a person to review.
fn sorted(
    standings: Vec<(TicketId, Status, bool)>,
) -> (Vec<TicketId>, Vec<TicketId>, Vec<TicketId>) {
    let (mut waiting, mut parked, mut in_review) = (Vec::new(), Vec::new(), Vec::new());
    for (ticket, status, followed) in standings {
        match status {
            Status::NeedsInput { .. } => waiting.push(ticket),
            Status::Parked { .. } => parked.push(ticket),
            Status::Active(_) if followed => in_review.push(ticket),
            Status::Active(_) => {}
        }
    }
    (waiting, parked, in_review)
}

/// The steps a draining daemon waits on: the holders of the slots `held`,
/// each with the role and time of the latest `run_started` event of its
/// ticket at or after the slot was taken.
fn drain_steps(held: &[HeldSlot], events: &[Event]) -> Vec<DrainStep> {
    held.iter()
        .filter_map(|slot| slot.holder.as_ref())
        .map(|holder| {
            let run = events
                .iter()
                .rev()
                .find(|event| {
                    event.kind == EventKind::RunStarted
                        && event.at >= holder.since
                        && event.ticket.as_ref() == Some(&holder.ticket)
                        && event.project.eq_ignore_ascii_case(&holder.project)
                })
                .map(|event| {
                    let role = event.data.get("role").and_then(Value::as_str);
                    (role.unwrap_or("unknown").to_owned(), event.at)
                });
            DrainStep {
                project: holder.project.clone(),
                ticket: holder.ticket.clone(),
                run,
            }
        })
        .collect()
}

/// The first line of the quarantine of the project at `dirs`, while one
/// stands, or why it cannot be read.
fn quarantine_line(dirs: &ProjectDirs) -> Option<Result<String, String>> {
    match dirs.quarantine() {
        Ok(None) => None,
        Ok(Some(text)) => Some(Ok(text
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .map_or("its file is empty", |line| line.trim_end_matches(':'))
            .to_owned())),
        Err(error) => Some(Err(error.to_string())),
    }
}

/// The steps typed in a terminal (OWL-478): in every project's folder under
/// `data`, the ticket locks a command holds whose holder holds none of the
/// slots `held`; `named` gives the served projects' repositories by folder.
fn typed_steps(
    data: &Path,
    named: &[(PathBuf, String)],
    held: &[HeldSlot],
    notes: &mut Vec<String>,
) -> Vec<TypedStep> {
    let in_slots: Vec<u32> = held
        .iter()
        .filter_map(|slot| slot.holder.as_ref().map(|holder| holder.pid))
        .collect();
    let all = match ProjectDirs::all(data) {
        Ok(all) => all,
        Err(error) => {
            notes.push(format!("the projects' folders cannot be read: {error}"));
            return Vec::new();
        }
    };
    let mut typed = Vec::new();
    for dirs in all {
        let project = named
            .iter()
            .find(|(root, _)| root == dirs.root())
            .map_or_else(|| folder_name(dirs.root()), |(_, name)| name.clone());
        match dirs.ticket_holders() {
            Ok(holders) => typed.extend(
                holders
                    .into_iter()
                    .filter(|holder| !in_slots.contains(&holder.pid))
                    .map(|holder| TypedStep {
                        project: project.clone(),
                        holder,
                    }),
            ),
            Err(error) => notes.push(format!(
                "the ticket locks of {project} cannot be probed: {error}"
            )),
        }
    }
    typed
}

/// `owner/name` of a project's folder, as [`ProjectDirs::new`] lays it out.
fn folder_name(root: &Path) -> String {
    let name = |path: Option<&Path>| {
        path.and_then(Path::file_name)
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
    };
    format!("{}/{}", name(root.parent()), name(Some(root)))
}

/// `at` in `tz`, to the minute.
fn time(at: Timestamp, tz: &TimeZone) -> String {
    at.to_zoned(tz.clone())
        .strftime("%Y-%m-%d %H:%M")
        .to_string()
}

/// Who asked `request`, when and why; a file that does not read says so.
fn asked(request: Option<&Request>, tz: &TimeZone) -> String {
    request.map_or_else(
        || "its file does not say who asked".to_owned(),
        |request| {
            format!(
                "by {} at {}: {}",
                request.by,
                time(request.at, tz),
                request.why
            )
        },
    )
}

fn tickets(tickets: &[TicketId]) -> String {
    tickets
        .iter()
        .map(TicketId::as_str)
        .collect::<Vec<_>>()
        .join(", ")
}

/// The kind of a line of [`render`]'s text (OWL-472): the web UI's
/// Orchestrator screen shows the kinds that are [`Part::on_the_page`], in
/// `owlshift status`'s own words, and the overview, graph and ticket parts
/// show the others.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Part {
    /// The daemon's line, and the stop and drain requests asked of it.
    Daemon,
    MachinePause,
    /// "No project is served".
    NoProject,
    /// A project's heading, `owner/name (from checkout)`.
    Project,
    /// The project file it runs on, a newer one refused, or why none.
    Record,
    Watch,
    ProjectPause,
    Quarantine,
    Draining,
    /// The tickets that wait for a person.
    Waiting,
    Parked,
    /// The green pull requests that wait for a person to merge.
    Merges,
    /// The steps a draining daemon waits on.
    DrainSteps,
    /// The steps under way, each holder, and the ones typed in a terminal.
    Slots,
    Usage,
    Digest,
    Note,
}

impl Part {
    /// Every kind, in the order [`render`] prints them.
    pub const ALL: [Part; 17] = [
        Part::Daemon,
        Part::MachinePause,
        Part::NoProject,
        Part::Project,
        Part::Record,
        Part::Watch,
        Part::ProjectPause,
        Part::Quarantine,
        Part::Draining,
        Part::Waiting,
        Part::Parked,
        Part::Merges,
        Part::DrainSteps,
        Part::Slots,
        Part::Usage,
        Part::Digest,
        Part::Note,
    ];

    /// Whether the Orchestrator screen shows this kind. The others belong
    /// to the overview (waits for a person, parked tickets, merge waits,
    /// the digest), the graph (steps under way and typed in a terminal) and
    /// the daemon's drain (the steps it waits on) and are left out here.
    pub fn on_the_page(self) -> bool {
        !matches!(
            self,
            Part::Waiting
                | Part::Parked
                | Part::Merges
                | Part::DrainSteps
                | Part::Slots
                | Part::Digest
        )
    }

    /// The kind's name in the web UI's JSON.
    pub fn name(self) -> &'static str {
        match self {
            Part::Daemon => "daemon",
            Part::MachinePause => "machine_pause",
            Part::NoProject => "no_project",
            Part::Project => "project",
            Part::Record => "record",
            Part::Watch => "watch",
            Part::ProjectPause => "project_pause",
            Part::Quarantine => "quarantine",
            Part::Draining => "draining",
            Part::Waiting => "waiting",
            Part::Parked => "parked",
            Part::Merges => "merges",
            Part::DrainSteps => "drain_steps",
            Part::Slots => "slots",
            Part::Usage => "usage",
            Part::Digest => "digest",
            Part::Note => "note",
        }
    }
}

/// The report as text, its times in `tz`, safe for a terminal.
pub fn render(report: &Report, tz: &TimeZone) -> String {
    let mut out = String::new();
    for (_, text) in parts(report, tz, Voice::Terminal) {
        let _ = writeln!(out, "{text}");
    }
    out
}

/// The lines [`render`] prints, each with its kind and already passed
/// through [`printable`]: `render` is their join, one per line. In the
/// terminal's `voice` a pause is lifted by `owlshift resume`; in the UI's, which
/// shows them on the Orchestrator screen, by its Resume button (OWL-529).
pub fn parts(report: &Report, tz: &TimeZone, voice: Voice) -> Vec<(Part, String)> {
    let mut out: Vec<(Part, String)> = Vec::new();
    let mut line = |part: Part, text: String| out.push((part, printable(&text)));
    line(
        Part::Daemon,
        match &report.daemon {
            Daemon::NotRunning => {
                "Daemon: not running, so no project is served (`owlshift start` starts it)."
                    .to_owned()
            }
            Daemon::Running(Some(record)) => format!(
                "Daemon: running since {} (pid {}, version {}).",
                time(record.since, tz),
                record.pid,
                record.version
            ),
            Daemon::Running(None) => "Daemon: running (its record does not read).".to_owned(),
            Daemon::Unknown(why) => format!("Daemon: unknown, its lock cannot be probed: {why}."),
        },
    );
    if let Some(request) = &report.stop_asked {
        line(
            Part::Daemon,
            format!(
                "  Stopping, asked {}: each watch ends once its steps under way finish.",
                asked(Some(request), tz)
            ),
        );
    }
    if let Some(request) = &report.drain_asked {
        line(
            Part::Daemon,
            format!(
                "  Draining for an upgrade, asked {}: the daemon exits once its steps under way \
                 finish.",
                asked(Some(request), tz)
            ),
        );
    }
    for step in &report.drain_steps {
        line(
            Part::DrainSteps,
            match &step.run {
                Some((role, at)) => format!(
                    "  waits on {} {}: role {role}, run started {}",
                    step.project,
                    step.ticket,
                    time(*at, tz)
                ),
                None => format!(
                    "  waits on {} {}: no run started yet",
                    step.project, step.ticket
                ),
            },
        );
    }
    if let Some(request) = &report.machine_pause {
        let lifts = match voice {
            Voice::Terminal => "`owlshift resume` lifts it.",
            Voice::Ui => "its Resume button lifts it.",
        };
        line(
            Part::MachinePause,
            format!(
                "Paused: every project of this machine, {}. Nothing is dispatched and no step \
                 starts; {lifts}",
                asked(request.as_ref(), tz)
            ),
        );
    }
    if report.projects.is_empty() {
        line(
            Part::NoProject,
            "No project is served (`owlshift start`, in a checkout, serves it).".to_owned(),
        );
    }
    for project in &report.projects {
        line(
            Part::Project,
            format!("{} (from {})", project.repository, project.checkout),
        );
        match &project.config {
            Ok(record) => {
                if let Some(kept) = &record.kept {
                    line(
                        Part::Record,
                        format!("  runs on {}", served::kept_name(kept)),
                    );
                    if let Some(refused) = &record.refused {
                        line(
                            Part::Record,
                            format!(
                                "  a newer project file is refused since {}: {}",
                                time(refused.since, tz),
                                refused.reason
                            ),
                        );
                    }
                } else {
                    let reason = record.refused.as_ref().map_or(
                        "no project file was read from its default branch yet",
                        |refused| refused.reason.as_str(),
                    );
                    line(Part::Record, format!("  not served: {reason}"));
                }
            }
            Err(why) => line(
                Part::Record,
                format!("  its served record cannot be read: {why}"),
            ),
        }
        line(
            Part::Watch,
            format!(
                "  watch: {}",
                match &project.watch {
                    Watch::NoDaemon => "none, no daemon runs".to_owned(),
                    Watch::NotRead => "not read".to_owned(),
                    Watch::NotStarted => "not started yet".to_owned(),
                    Watch::Running { pid, since } => format!(
                        "running since {}{}",
                        time(*since, tz),
                        pid.map_or_else(String::new, |pid| format!(" (pid {pid})"))
                    ),
                    Watch::Restarting {
                        ended,
                        at,
                        why,
                        in_a_row,
                    } => {
                        let when = at.map_or_else(
                            || "later".to_owned(),
                            |at| format!("at {}", time(at, tz)),
                        );
                        let row = match in_a_row {
                            Some(n) if *n > 1 => format!(", {n} ends in a row"),
                            _ => String::new(),
                        };
                        format!(
                            "restarting {when}: it ended at {} ({why}){row}",
                            time(*ended, tz)
                        )
                    }
                    Watch::Unservable { at, why } => {
                        format!("ended at {} and cannot start: {why}", time(*at, tz))
                    }
                    Watch::Ended { at } => format!("ended as asked at {}", time(*at, tz)),
                }
            ),
        );
        if let Some(request) = &project.pause {
            let more = match request {
                Some(request) if request.by == BY_DAEMON => {
                    "; the daemon tries it again at each of its passes"
                }
                _ => match voice {
                    Voice::Terminal => "; `owlshift resume <owner/repo>` lifts it",
                    Voice::Ui => "; its Resume button lifts it",
                },
            };
            line(
                Part::ProjectPause,
                format!("  paused {}{more}", asked(request.as_ref(), tz)),
            );
        }
        let lifted = "`owlshift unquarantine`, in its checkout, shows it whole and clears it";
        match &project.quarantine {
            Some(Ok(first)) => line(
                Part::Quarantine,
                format!("  quarantined: {first}; {lifted}"),
            ),
            Some(Err(why)) => line(
                Part::Quarantine,
                format!("  quarantined, its file cannot be read ({why}); {lifted}"),
            ),
            None => {}
        }
        if project.draining {
            line(
                Part::Draining,
                "  draining: its watch ends once its steps under way finish".to_owned(),
            );
        }
        if !project.waiting.is_empty() {
            line(
                Part::Waiting,
                format!("  waiting for a person: {}", tickets(&project.waiting)),
            );
        }
        if !project.parked.is_empty() {
            line(
                Part::Parked,
                format!("  parked: {}", tickets(&project.parked)),
            );
        }
        if !project.merges_waiting.is_empty() {
            line(
                Part::Merges,
                "  green, waiting for a person to merge:".to_owned(),
            );
        }
        for wait in &project.merges_waiting {
            let why = if wait.refused {
                format!("GitHub refused to merge it: {}", wait.reason)
            } else {
                wait.reason.clone()
            };
            line(
                Part::Merges,
                format!(
                    "    {}: pull request #{}, {why}",
                    wait.ticket, wait.pull_request
                ),
            );
        }
    }
    match &report.slots {
        Some((held, count)) if held.is_empty() && report.typed.is_empty() => {
            line(
                Part::Slots,
                format!("Steps under way: none (0 of {count} run slots)."),
            );
        }
        Some((held, count)) => {
            let typed = match report.typed.len() {
                0 => String::new(),
                n => format!(", and {n} typed in a terminal"),
            };
            line(
                Part::Slots,
                format!(
                    "Steps under way: {} of {count} run slots{typed}.",
                    held.len()
                ),
            );
            for slot in held {
                line(
                    Part::Slots,
                    match &slot.holder {
                        Some(holder) => format!(
                            "  {} {} (pid {}, since {})",
                            holder.project,
                            holder.ticket,
                            holder.pid,
                            time(holder.since, tz)
                        ),
                        None => format!("  slot {}: its holder is not recorded", slot.slot),
                    },
                );
            }
            for step in &report.typed {
                line(
                    Part::Slots,
                    format!(
                        "  {} {}, typed in a terminal (pid {}, since {})",
                        step.project,
                        step.holder.ticket,
                        step.holder.pid,
                        time(step.holder.since, tz)
                    ),
                );
            }
        }
        None => {}
    }
    if report.usage.is_empty() {
        line(Part::Usage, "Usage: nothing under way recorded.".to_owned());
    }
    for (harness, usage) in &report.usage {
        let name = crate::usage::key(*harness);
        line(
            Part::Usage,
            match usage {
                HarnessUsage::Limited(Some(until)) => {
                    format!("Usage: {name} at its limit until {}.", time(*until, tz))
                }
                HarnessUsage::Limited(None) => {
                    format!("Usage: {name} at its limit, until a probe shows it reset.")
                }
                HarnessUsage::Share { share, resets_at } => format!(
                    "Usage: {name} {:.0} % of a window that resets at {}.",
                    share * 100.0,
                    time(*resets_at, tz)
                ),
            },
        );
    }
    match &report.digest {
        Some(DigestRecord {
            at,
            line: Some(text),
            ..
        }) => line(
            Part::Digest,
            format!("Latest digest, {}: {text}", time(*at, tz)),
        ),
        _ => line(Part::Digest, "Latest digest: none yet.".to_owned()),
    }
    for note in &report.notes {
        line(Part::Note, format!("warning: {note}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use std::io;

    use owlshift_contracts::format::Format;
    use owlshift_contracts::service::{BY_PAUSE, KeptFile, ServedProject, ServedProjects};
    use serde_json::json;

    use super::*;
    use crate::events::{EventSink, data};

    fn record(since: Timestamp) -> DaemonRecord {
        DaemonRecord {
            format: Format,
            pid: 4242,
            version: "0.4.0".to_owned(),
            binary: "/bin/owlshift".to_owned(),
            since,
            build: None,
        }
    }

    /// The daemon's own event of `repo`, now.
    fn service(data_dir: &Path, repo: &str, pairs: Vec<(&str, Value)>) {
        let mut nowhere = io::sink();
        let mut details = data([]);
        for (key, value) in pairs {
            details.insert(key.to_owned(), value);
        }
        EventSink::new(repo, EventLog::in_dir(data_dir), &mut nowhere).emit_project(
            None,
            EventKind::Decision,
            details,
        );
    }

    /// OWL-467: the files' reading never opens the event log, which `gather`
    /// reads, and leaves a running daemon's watches unread.
    #[test]
    fn gather_files_never_reads_the_event_log() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path();
        let served = ServedProjects {
            format: Format,
            projects: vec![ServedProject {
                repository: "acme/web".to_owned(),
                checkout: "/home/ada/acme/web".to_owned(),
            }],
        };
        crate::step::write_atomically(&Registry::in_data_dir(data_dir).file(), &served.render())
            .unwrap();
        let log = EventLog::in_dir(data_dir).path().to_owned();
        std::fs::create_dir_all(&log).unwrap();
        let event_note = |notes: &[String]| notes.iter().any(|note| note.contains("event log"));

        let files = gather_files(data_dir, 2, &[], Timestamp::now());
        assert!(!event_note(&files.notes), "{:?}", files.notes);
        assert_eq!(files.projects[0].watch, Watch::NoDaemon);
        let all = gather(data_dir, 2, &[], Timestamp::now());
        assert!(event_note(&all.notes), "{:?}", all.notes);

        let _daemon = daemon::instance::take(data_dir, &record(Timestamp::now()))
            .unwrap()
            .unwrap();
        let files = gather_files(data_dir, 2, &[], Timestamp::now());
        assert_eq!(files.projects[0].watch, Watch::NotRead);
        assert!(!event_note(&files.notes), "{:?}", files.notes);
        let all = gather(data_dir, 2, &[], Timestamp::now());
        assert_eq!(all.projects[0].watch, Watch::NotStarted);
        let text = render(&files, &TimeZone::UTC);
        assert!(text.contains("  watch: not read"), "{text}");
    }

    /// OWL-467: a ticket at Watch whose pull request watch follows is for a
    /// person to review; one whose following ended is not.
    #[test]
    fn a_followed_pull_request_at_watch_is_in_review() {
        let id = |id: &str| TicketId::new(id).unwrap();
        let (waiting, parked, in_review) = sorted(vec![
            (id("OWL-1"), Status::Active(Stage::Watch), true),
            (id("OWL-2"), Status::Active(Stage::Watch), false),
            (
                id("OWL-3"),
                Status::NeedsInput {
                    return_to: Stage::Design,
                },
                false,
            ),
            (
                id("OWL-4"),
                Status::Parked {
                    at: Stage::Build,
                    awaiting_input: false,
                },
                false,
            ),
            (id("OWL-5"), Status::Active(Stage::Build), false),
        ]);
        assert_eq!(in_review, [id("OWL-1")]);
        assert_eq!(waiting, [id("OWL-3")]);
        assert_eq!(parked, [id("OWL-4")]);
    }

    /// The acceptance: a running daemon serving two projects, one whose
    /// watch ended and restarts, saying why, the other paused by a person,
    /// with the reason, and the machine's pause.
    #[test]
    fn status_shows_a_restarting_watch_and_a_paused_project_with_its_reason() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path();
        let since = Timestamp::now()
            .checked_sub(jiff::SignedDuration::from_mins(5))
            .unwrap();
        let _daemon = daemon::instance::take(data_dir, &record(since))
            .unwrap()
            .unwrap();
        let served = ServedProjects {
            format: Format,
            projects: ["acme/web", "acme/api"]
                .into_iter()
                .map(|repository| ServedProject {
                    repository: repository.to_owned(),
                    checkout: format!("/home/ada/{repository}"),
                })
                .collect(),
        };
        crate::step::write_atomically(&Registry::in_data_dir(data_dir).file(), &served.render())
            .unwrap();
        let web = ProjectDirs::new(data_dir, &Repo::parse("acme/web").unwrap());
        let mut kept = ServedConfig::new("acme/web");
        kept.kept = Some(KeptFile {
            branch: "main".to_owned(),
            commit: "a".repeat(40),
            text: String::new(),
            since,
        });
        crate::step::write_atomically(&served::record_file(&web), &kept.render()).unwrap();
        service(
            data_dir,
            "acme/web",
            vec![("service", json!("watch_started")), ("pid", json!(77))],
        );
        service(
            data_dir,
            "acme/web",
            vec![
                ("service", json!("watch_ended")),
                (
                    "end",
                    json!("exit status: 1: owlshift: the tracker did not answer"),
                ),
                ("asked", json!(false)),
                ("restart_in_ms", json!(20_000)),
                ("in_a_row", json!(2)),
            ],
        );
        service(
            data_dir,
            "acme/api",
            vec![("service", json!("watch_started")), ("pid", json!(78))],
        );
        let api = ProjectDirs::new(data_dir, &Repo::parse("acme/api").unwrap());
        let lunch = Request::new(BY_PAUSE, since, "lunch break");
        requests::ask_pause(data_dir, Some(&api), &lunch).unwrap();
        let all = Request::new(BY_PAUSE, since, "travelling");
        requests::ask_pause(data_dir, None, &all).unwrap();

        let report = gather(data_dir, 2, &[], Timestamp::now());
        assert!(report.notes.is_empty(), "{:?}", report.notes);
        assert!(
            matches!(&report.projects[0].watch, Watch::Restarting { why, .. }
            if why.ends_with("the tracker did not answer"))
        );
        let text = render(&report, &TimeZone::UTC);
        let has = |part: &str| assert!(text.contains(part), "{part:?} missing in\n{text}");
        has("Daemon: running since");
        has("(pid 4242, version 0.4.0)");
        has("Paused: every project of this machine, by owlshift pause at");
        has(": travelling.");
        has("acme/web (from /home/ada/acme/web)");
        has("  runs on owlshift.toml at origin/main (aaaaaaaaaaaa)");
        has("  watch: restarting at ");
        has("(exit status: 1: owlshift: the tracker did not answer), 2 ends in a row");
        has("acme/api (from /home/ada/acme/api)");
        has("  not served: no project file was read from its default branch yet");
        has("  watch: running since ");
        has("(pid 78)");
        has("  paused by owlshift pause at ");
        has(": lunch break; `owlshift resume <owner/repo>` lifts it");
        has("Steps under way: none (0 of 2 run slots).");
        has("Latest digest: none yet.");

        // OWL-529: the terminal names the command that lifts a pause, the UI
        // its Resume button; every other line is the same in both voices.
        let tz = TimeZone::UTC;
        let terminal = parts(&report, &tz, Voice::Terminal);
        let ui = parts(&report, &tz, Voice::Ui);
        assert_eq!(terminal.len(), ui.len());
        let is_pause = |part: &Part| matches!(part, Part::MachinePause | Part::ProjectPause);
        let others = |all: &[(Part, String)]| -> Vec<(Part, String)> {
            all.iter()
                .filter(|(part, _)| !is_pause(part))
                .cloned()
                .collect()
        };
        assert_eq!(others(&terminal), others(&ui));
        let pause_line = |all: &[(Part, String)], kind: Part, start: &str| -> String {
            let found: Vec<&String> = all
                .iter()
                .filter(|(part, line)| *part == kind && line.starts_with(start))
                .map(|(_, line)| line)
                .collect();
            assert_eq!(found.len(), 1, "{kind:?}: {all:?}");
            found[0].clone()
        };
        let machine = "Paused: every project of this machine, by owlshift pause at";
        let terminal_machine = pause_line(&terminal, Part::MachinePause, machine);
        let ui_machine = pause_line(&ui, Part::MachinePause, machine);
        assert!(
            terminal_machine.ends_with(": travelling. Nothing is dispatched and no step starts; `owlshift resume` lifts it."),
            "{terminal_machine}"
        );
        assert!(
            ui_machine.ends_with(
                ": travelling. Nothing is dispatched and no step starts; its Resume button lifts it."
            ),
            "{ui_machine}"
        );
        let project = "  paused by owlshift pause at ";
        let terminal_project = pause_line(&terminal, Part::ProjectPause, project);
        let ui_project = pause_line(&ui, Part::ProjectPause, project);
        assert!(
            terminal_project.ends_with(": lunch break; `owlshift resume <owner/repo>` lifts it"),
            "{terminal_project}"
        );
        assert!(
            ui_project.ends_with(": lunch break; its Resume button lifts it"),
            "{ui_project}"
        );
        for (_, line) in &ui {
            assert!(!line.contains("owlshift resume"), "{line}");
        }
        // `render`, which `owlshift status` prints, is the terminal's.
        assert_eq!(
            render(&report, &tz),
            terminal
                .iter()
                .map(|(_, line)| format!("{line}\n"))
                .collect::<String>()
        );
    }

    /// OWL-478: a ticket lock a command holds with no run slot is a step
    /// typed in a terminal, in a served project or not, and its holder is
    /// not shown twice once it holds a slot; a served project's quarantine
    /// is shown with its first line and the way to clear it.
    #[test]
    fn status_shows_a_typed_step_and_a_standing_quarantine() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path();
        let served = ServedProjects {
            format: Format,
            projects: vec![ServedProject {
                repository: "acme/web".to_owned(),
                checkout: "/home/ada/acme/web".to_owned(),
            }],
        };
        crate::step::write_atomically(&Registry::in_data_dir(data_dir).file(), &served.render())
            .unwrap();
        let web = ProjectDirs::new(data_dir, &Repo::parse("acme/web").unwrap());
        std::fs::create_dir_all(web.root()).unwrap();
        std::fs::write(
            web.quarantine_file(),
            "\nRun r1 of OWL-1 broke isolation:\n- the checkout's hooks changed\n",
        )
        .unwrap();
        let typed = TicketId::new("OWL-5").unwrap();
        let _web_lock = web.ticket_lock(&typed).unwrap().unwrap();
        let api = ProjectDirs::new(data_dir, &Repo::parse("Other/Api").unwrap());
        let _api_lock = api
            .ticket_lock(&TicketId::new("OWL-9").unwrap())
            .unwrap()
            .unwrap();

        let report = gather(data_dir, 2, &[], Timestamp::now());
        assert!(report.notes.is_empty(), "{:?}", report.notes);
        let text = render(&report, &TimeZone::UTC);
        let has = |part: &str| assert!(text.contains(part), "{part:?} missing in\n{text}");
        has(
            "  quarantined: Run r1 of OWL-1 broke isolation; `owlshift unquarantine`, in its \
             checkout, shows it whole and clears it",
        );
        has("Steps under way: 0 of 2 run slots, and 2 typed in a terminal.");
        let pid = std::process::id();
        has(&format!(
            "  acme/web OWL-5, typed in a terminal (pid {pid}, since "
        ));
        has(&format!(
            "  other/api OWL-9, typed in a terminal (pid {pid}, since "
        ));

        // A step child takes its slot before the ticket's lock, under the
        // same process: its lock is the slot's, never a typed step.
        let holder = crate::slots::Holder {
            project: "acme/web".to_owned(),
            ticket: typed,
            pid,
            since: Timestamp::now(),
        };
        let _slot = MachineSlots::new(data_dir, 2)
            .take(&holder)
            .unwrap()
            .unwrap();
        let report = gather(data_dir, 2, &[], Timestamp::now());
        assert_eq!(report.typed, []);
        let text = render(&report, &TimeZone::UTC);
        assert!(
            text.contains("Steps under way: 1 of 2 run slots.\n"),
            "{text}"
        );
    }

    /// OWL-481: a draining daemon, for a stop or an upgrade, says so and
    /// names each step it waits on with its latest run's role and start,
    /// which is not the slot's `since`: a review loop starts runs inside one
    /// step.
    #[test]
    fn a_draining_daemon_names_the_steps_it_waits_on() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path();
        let since: Timestamp = "2026-10-30T18:00:00Z".parse().unwrap();
        let _daemon = daemon::instance::take(data_dir, &record(since))
            .unwrap()
            .unwrap();
        let (one, two) = (
            TicketId::new("OWL-5").unwrap(),
            TicketId::new("OWL-6").unwrap(),
        );
        let take = |ticket: &TicketId, at: &str| crate::slots::Holder {
            project: "acme/web".to_owned(),
            ticket: ticket.clone(),
            pid: std::process::id(),
            since: at.parse().unwrap(),
        };
        let slots = MachineSlots::new(data_dir, 3);
        let _first = slots
            .take(&take(&one, "2026-10-30T18:10:00Z"))
            .unwrap()
            .unwrap();
        let _second = slots
            .take(&take(&two, "2026-10-30T18:11:00Z"))
            .unwrap()
            .unwrap();
        let log = EventLog::in_dir(data_dir);
        for (at, ticket, role) in [
            ("2026-10-30T18:10:30Z", "OWL-5", "build"),
            ("2026-10-30T18:40:00Z", "OWL-5", "design_review"),
            ("2026-10-30T18:12:00Z", "OWL-7", "build"),
        ] {
            let mut started = Event {
                format: Format,
                at: at.parse().unwrap(),
                project: "Acme/Web".to_owned(),
                ticket: Some(TicketId::new(ticket).unwrap()),
                run: Some("r1".to_owned()),
                kind: EventKind::RunStarted,
                data: data([("role", json!(role))]),
            };
            started.run = Some(format!("run-{at}"));
            log.append(&started).unwrap();
        }
        let text = |report: &Report| render(report, &TimeZone::UTC);

        let quiet = gather(data_dir, 3, &[], Timestamp::now());
        assert!(quiet.drain_steps.is_empty());
        assert!(!text(&quiet).contains("waits on"));

        let request = |by: &str| Request {
            by: by.to_owned(),
            at: "2026-10-30T19:00:00Z".parse().unwrap(),
            why: "an upgrade".to_owned(),
            pid: None,
        };
        requests::ask_drain(data_dir, &request("upgrade")).unwrap();
        let report = gather(data_dir, 3, &[], Timestamp::now());
        let out = text(&report);
        let has = |part: &str| assert!(out.contains(part), "{part:?} missing in\n{out}");
        has(
            "  Draining for an upgrade, asked by upgrade at 2026-10-30 19:00: an upgrade: the daemon exits once its steps under way finish.",
        );
        has("  waits on acme/web OWL-5: role design_review, run started 2026-10-30 18:40");
        has("  waits on acme/web OWL-6: no run started yet");
        assert!(!out.contains("OWL-7"), "{out}");
        assert!(!out.contains("Stopping"), "{out}");

        requests::remove_drain_request(data_dir).unwrap();
        requests::ask_stop(data_dir, &request("stop")).unwrap();
        let out = text(&gather(data_dir, 3, &[], Timestamp::now()));
        assert!(out.contains("  Stopping, asked by stop at"), "{out}");
        assert!(
            out.contains("waits on acme/web OWL-5: role design_review"),
            "{out}"
        );
        assert!(!out.contains("Draining for an upgrade"), "{out}");
    }

    /// A watch the daemon paused for a refusal no restart mends is never
    /// read as running, nor any watch of a daemon that does not run, nor an
    /// event of an earlier daemon.
    /// OWL-464: a project lists each ticket whose latest `merge_waits` or
    /// final `merge_refused` event is at the head its ref still follows,
    /// with the reason, and reads the ref only of a ticket with such an
    /// event.
    #[test]
    fn status_lists_the_green_pull_requests_that_wait_for_a_person() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path();
        let emit = |repo: &str, ticket: &str, pairs: Vec<(&str, Value)>| {
            let mut details = data([]);
            for (key, value) in pairs {
                details.insert(key.to_owned(), value);
            }
            let ticket = TicketId::new(ticket).unwrap();
            EventSink::new(repo, EventLog::in_dir(data_dir), &mut io::sink()).emit(
                &ticket,
                None,
                EventKind::Decision,
                details,
            );
        };
        let waits = |head: &str, reason: &str| {
            vec![
                ("watch", json!("merge_waits")),
                ("pull_request", json!(3)),
                ("head", json!(head)),
                ("reason", json!(reason)),
            ]
        };
        let refused = |head: &str, final_: bool| {
            vec![
                ("watch", json!("merge_refused")),
                ("pull_request", json!(4)),
                ("head", json!(head)),
                ("final", json!(final_)),
                ("reason", json!("Merge method squash is not allowed")),
            ]
        };
        emit("acme/web", "OWL-1", waits("a", "first hold"));
        emit("acme/web", "OWL-1", waits("b", "the label is missing"));
        emit("acme/web", "OWL-2", refused("c", true));
        emit("acme/web", "OWL-3", refused("d", false));
        emit("acme/web", "OWL-4", waits("e", "no longer followed"));
        emit("acme/api", "OWL-5", waits("f", "another project's"));
        let (events, _) = why::read_events(EventLog::in_dir(data_dir).path()).unwrap();
        let mut asked = Vec::new();
        let listed = merge_waits(&events, "ACME/web", |ticket| {
            asked.push(ticket.to_string());
            match ticket.as_str() {
                "OWL-1" => Some("b".to_owned()),
                "OWL-2" => Some("c".to_owned()),
                _ => None,
            }
        });
        assert_eq!(asked, ["OWL-1", "OWL-2", "OWL-4"]);
        let wait = |ticket: &str, pull_request, reason: &str, refused| MergeWait {
            ticket: TicketId::new(ticket).unwrap(),
            pull_request,
            reason: reason.to_owned(),
            refused,
        };
        assert_eq!(
            listed,
            [
                wait("OWL-1", 3, "the label is missing", false),
                wait("OWL-2", 4, "Merge method squash is not allowed", true),
            ]
        );

        // The text, under its project.
        let served = ServedProjects {
            format: Format,
            projects: vec![ServedProject {
                repository: "acme/web".to_owned(),
                checkout: "/home/ada/acme/web".to_owned(),
            }],
        };
        crate::step::write_atomically(&Registry::in_data_dir(data_dir).file(), &served.render())
            .unwrap();
        let mut report = gather(data_dir, 1, &[], Timestamp::now());
        assert!(report.projects[0].merges_waiting.is_empty());
        report.projects[0].merges_waiting = listed;
        let text = render(&report, &TimeZone::UTC);
        assert!(
            text.contains(
                "  green, waiting for a person to merge:\n    OWL-1: pull request #3, the label is \
                 missing\n    OWL-2: pull request #4, GitHub refused to merge it: Merge method \
                 squash is not allowed\n"
            ),
            "{text}"
        );
    }

    #[test]
    fn a_watch_is_read_from_the_daemons_events_since_it_started() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path();
        service(
            data_dir,
            "acme/web",
            vec![("service", json!("watch_started")), ("pid", json!(1))],
        );
        let (events, _) = why::read_events(EventLog::in_dir(data_dir).path()).unwrap();
        let later = Timestamp::now()
            .checked_add(jiff::SignedDuration::from_mins(1))
            .unwrap();
        assert_eq!(watch_state(&events, "acme/web", later), Watch::NotStarted);
        assert!(matches!(
            watch_state(&events, "ACME/web", Timestamp::MIN),
            Watch::Running { pid: Some(1), .. }
        ));
        assert_eq!(
            watch_state(&events, "acme/api", Timestamp::MIN),
            Watch::NotStarted
        );
        service(
            data_dir,
            "acme/web",
            vec![
                ("service", json!("watch_ended")),
                ("end", json!("exit status: 78")),
                ("asked", json!(false)),
                ("unservable", json!("the keychain refused")),
            ],
        );
        service(
            data_dir,
            "acme/web",
            vec![("service", json!("pause_lifted"))],
        );
        let (events, _) = why::read_events(EventLog::in_dir(data_dir).path()).unwrap();
        assert!(matches!(
            watch_state(&events, "acme/web", Timestamp::MIN),
            Watch::Unservable { why, .. } if why == "the keychain refused"
        ));
        let report = gather(data_dir, 1, &[], Timestamp::now());
        assert_eq!(report.daemon, Daemon::NotRunning);
        assert!(report.projects.is_empty());
        let text = render(&report, &TimeZone::UTC);
        assert!(text.starts_with("Daemon: not running"), "{text}");
        assert!(text.contains("No project is served"), "{text}");
    }

    /// A report that holds every kind of line `render` prints.
    fn full_report() -> Report {
        let at = |text: &str| text.parse::<Timestamp>().unwrap();
        let ask = |by: &str, why: &str| Request::new(by, at("2026-11-05T09:00:00Z"), why);
        let holder = |ticket: &str| crate::slots::Holder {
            project: "acme/web".to_owned(),
            ticket: TicketId::new(ticket).unwrap(),
            pid: 77,
            since: at("2026-11-05T09:30:00Z"),
        };
        let mut kept = ServedConfig::new("acme/web");
        kept.kept = Some(KeptFile {
            branch: "main".to_owned(),
            commit: "a".repeat(40),
            text: String::new(),
            since: at("2026-11-04T09:00:00Z"),
        });
        kept.refused = Some(owlshift_contracts::service::RefusedFile {
            commit: None,
            reason: "requires a newer Owlshift".to_owned(),
            since: at("2026-11-05T08:00:00Z"),
        });
        let project = |repository: &str, config: Result<ServedConfig, String>| Project {
            repository: repository.to_owned(),
            checkout: format!("/home/ada/{repository}"),
            config,
            watch: Watch::Restarting {
                ended: at("2026-11-05T09:00:00Z"),
                at: Some(at("2026-11-05T09:01:00Z")),
                why: "exit status 1".to_owned(),
                in_a_row: Some(3),
            },
            pause: Some(Some(ask(BY_DAEMON, "the keychain refused"))),
            draining: true,
            quarantine: Some(Ok("a run changed the checkout".to_owned())),
            waiting: vec![TicketId::new("OWL-1").unwrap()],
            parked: vec![TicketId::new("OWL-2").unwrap()],
            in_review: Vec::new(),
            merges_waiting: vec![MergeWait {
                ticket: TicketId::new("OWL-3").unwrap(),
                pull_request: 9,
                reason: "a person merges it".to_owned(),
                refused: false,
            }],
        };
        Report {
            daemon: Daemon::Running(Some(record(at("2026-11-05T08:00:00Z")))),
            stop_asked: Some(ask("stop", "maintenance")),
            drain_asked: Some(ask("upgrade", "a new build")),
            drain_steps: vec![DrainStep {
                project: "acme/web".to_owned(),
                ticket: TicketId::new("OWL-4").unwrap(),
                run: Some(("build".to_owned(), at("2026-11-05T09:10:00Z"))),
            }],
            machine_pause: Some(Some(ask(BY_PAUSE, "lunch"))),
            projects: vec![
                project("acme/web", Ok(kept)),
                project("acme/api", Ok(ServedConfig::new("acme/api"))),
                project("acme/old", Err("served.json is unreadable".to_owned())),
            ],
            slots: Some((
                vec![
                    HeldSlot {
                        slot: 0,
                        holder: Some(holder("OWL-5")),
                    },
                    HeldSlot {
                        slot: 1,
                        holder: None,
                    },
                ],
                2,
            )),
            typed: vec![TypedStep {
                project: "acme/web".to_owned(),
                holder: TicketHolder {
                    ticket: TicketId::new("OWL-6").unwrap(),
                    pid: 88,
                    since: at("2026-11-05T09:40:00Z"),
                },
            }],
            usage: vec![
                (
                    Harness::Claude,
                    HarnessUsage::Share {
                        share: 0.42,
                        resets_at: at("2026-11-05T14:00:00Z"),
                    },
                ),
                (Harness::Codex, HarnessUsage::Limited(None)),
            ],
            digest: Some(DigestRecord {
                format: Format,
                at: at("2026-11-05T07:00:00Z"),
                line: Some("3 delivered".to_owned()),
            }),
            notes: vec!["the run slots cannot be probed: nope".to_owned()],
        }
    }

    /// OWL-472: `render` is the join of `parts`, every line of it is in
    /// exactly one part, and the parts the Orchestrator shows are the ones
    /// the overview, the graph and the ticket page do not.
    #[test]
    fn the_parts_are_the_lines_render_prints_by_kind() {
        let report = full_report();
        let tz = TimeZone::UTC;
        let parts = parts(&report, &tz, Voice::Terminal);
        let text = render(&report, &tz);
        let joined: String = parts.iter().map(|(_, line)| format!("{line}\n")).collect();
        assert_eq!(joined, text);
        assert_eq!(text.lines().count(), parts.len());
        // The kinds appear in `render`'s order, and every kind is present.
        let kinds: Vec<Part> = parts.iter().map(|(kind, _)| *kind).collect();
        for kind in Part::ALL
            .into_iter()
            .filter(|kind| *kind != Part::NoProject)
        {
            assert!(kinds.contains(&kind), "{kind:?} is missing: {text}");
        }
        // With no project served the line that says so is the one of its kind.
        let bare = Report {
            projects: Vec::new(),
            ..report.clone()
        };
        assert!(
            super::parts(&bare, &tz, Voice::Terminal)
                .iter()
                .any(|(kind, _)| *kind == Part::NoProject)
        );
        let shown: Vec<Part> = Part::ALL
            .into_iter()
            .filter(|kind| kind.on_the_page())
            .collect();
        assert_eq!(
            shown,
            [
                Part::Daemon,
                Part::MachinePause,
                Part::NoProject,
                Part::Project,
                Part::Record,
                Part::Watch,
                Part::ProjectPause,
                Part::Quarantine,
                Part::Draining,
                Part::Usage,
                Part::Note
            ]
        );
        // A line that holds a control character is printable in its part.
        let mut report = report;
        report.notes = vec!["bad\u{1b}[31m".to_owned()];
        assert!(
            super::parts(&report, &tz, Voice::Terminal)
                .iter()
                .all(|(_, line)| !line.contains('\u{1b}'))
        );
    }
}
