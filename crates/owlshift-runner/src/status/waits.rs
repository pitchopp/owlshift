//! `owlshift status --waits [OWNER/REPO]` (OWL-459, architecture section 5,
//! "`owlshift status`"): what each ticket in flight or admitted waits on,
//! and behind whom.
//!
//! It reads what `owlshift status` and `owlshift why` read, and the drain's
//! ready snapshot: the machine's run slots and pause, each project's ticket
//! refs, its pause, quarantine marker, the project file kept in its
//! `served.json` and its `ready.json`, and the event log's waits, folded by
//! [`why::fold`] and worded by [`why::phrase`]. It takes no lock: the slots
//! are counted as `status` counts them.
//!
//! A ticket is shown while it is in flight ([`held::holds`]), holds a run
//! slot, or the drain still reads it (OWL-477): its latest ready snapshot
//! lists it. So a ticket that left the ready column, and a ref whose pull
//! request ended, are not shown with a wait or as a holder that are no
//! longer true.
//!
//! [`gather`] does the reading; [`build`] makes the document
//! `--json` prints ([`StatusWaits`]) and [`render`] the text from it, both
//! pure, so the two show one content.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::Path;

use jiff::Timestamp;
use jiff::tz::TimeZone;
use owlshift_adapters::forge::Repo;
use owlshift_contracts::config::ProjectConfig;
use owlshift_contracts::format::Format;
use owlshift_contracts::ids::TicketId;
use owlshift_contracts::ready::ReadySnapshot;
use owlshift_contracts::result::Question;
use owlshift_contracts::service::Request;
use owlshift_contracts::waits::{
    Behind, HeldRun, Paused, ProjectWaits, RefStatus, RunSlots, StatusWaits, TicketWaits, Wait,
    WaitResource,
};
use owlshift_core::schedule::{holds_slot_or_unknown, natural_cmp};
use owlshift_core::state::Status;
use owlshift_core::vocab::Stage;
use serde_json::Value;

use crate::daemon::requests;
use crate::events::printable;
use crate::held;
use crate::project::ProjectDirs;
use crate::ready;
use crate::served;
use crate::slots::{HeldSlot, MachineSlots};
use crate::ticket_ref::{self, RefState};
use crate::voice::Voice;
use crate::why::{self, Folded, Waiting};

/// The reasons the drain tells a ticket of the ready column that is not
/// admitted: such a ticket is neither admitted nor in flight, so it is not
/// shown unless a ticket ref of it stands.
const NOT_ADMITTED: [&str; 3] = ["not_admitted", "excluded", "duplicate"];

/// Everything [`build`] needs, as read.
#[derive(Clone, Debug)]
pub struct Read {
    pub at: Timestamp,
    /// The machine's pause, while it stands; `None` within for a file that
    /// does not read.
    pub machine_pause: Option<Option<Request>>,
    /// The machine's run slots held now, with their holders, and
    /// `concurrent_runs`; `None` when they cannot be probed.
    pub slots: Option<(Vec<HeldSlot>, u32)>,
    pub projects: Vec<ProjectRead>,
    /// What could not be read.
    pub notes: Vec<String>,
}

/// One project, as read.
#[derive(Clone, Debug)]
pub struct ProjectRead {
    /// `owner/repo`, as the events name it.
    pub repository: String,
    pub served: bool,
    /// `policy.max_in_flight` of the project file its `served.json` keeps.
    pub max_in_flight: Option<u8>,
    /// Its ticket refs, each with its state and whether it holds
    /// ([`held::holds`], a ref that cannot be read holding).
    pub refs: Vec<(TicketId, RefState, bool)>,
    /// Its events, folded; a quarantine's wait dropped when no marker
    /// stands any more.
    pub folded: Folded,
    pub quarantined: bool,
    /// Its own pause, while it stands; `None` within for a file that does
    /// not read.
    pub pause: Option<Option<Request>>,
    /// The tickets of its latest ready snapshot; `None` when there is none
    /// or it does not read.
    pub ready: Option<BTreeSet<TicketId>>,
    /// That snapshot as read, once: what the web UI's blockers layer draws
    /// from, so that it joins the very snapshot the waits were shown with.
    pub snapshot: Result<Option<ReadySnapshot>, String>,
    /// What the web UI's overview needs of every ref that reads, the ones
    /// `refs` holds but no row of the document shows included (OWL-470).
    pub details: BTreeMap<TicketId, RefDetail>,
}

/// What a ticket ref says that the document's rows do not: for the web UI's
/// "For you" block, which counts every ref as the header does
/// (`why::standings`), whether or not its ticket has a row.
#[derive(Clone, Debug, PartialEq)]
pub struct RefDetail {
    /// Its status; `Unread` when the ref's record does not read or the core
    /// accepts no state from it.
    pub status: RefStatus,
    /// The pull request watch follows (`watch::followed`), a pull request
    /// for a person to review.
    pub pull_request: Option<u64>,
    /// The questions of its latest ask, in the ask's order.
    pub questions: Vec<Question>,
    /// Whether the ticket waits for its plan's approval: it needs input and
    /// its latest ask holds a question of the approval category, which the
    /// runner marks its approval round with (`on_demand::APPROVAL_CATEGORY`).
    pub plan_to_approve: bool,
}

/// The detail of the ref `stored`, `state` being what [`ticket_ref::state_of`]
/// made of it.
fn detail(stored: &ticket_ref::Stored, state: &RefState) -> RefDetail {
    let state = match state {
        RefState::Read(Some(state)) | RefState::Unadmitted(state) => Some(state),
        RefState::Read(None) | RefState::Unread(_) => None,
    };
    let Some(state) = state else {
        return RefDetail {
            status: RefStatus::Unread,
            pull_request: None,
            questions: Vec::new(),
            plan_to_approve: false,
        };
    };
    let status = state.status();
    let questions = stored
        .record
        .questions
        .asks
        .last()
        .map_or_else(Vec::new, |ask| ask.questions.clone());
    let needs_input = matches!(status, Status::NeedsInput { .. });
    let pull_request = crate::watch::followed(status, stored)
        .then(|| {
            stored
                .record
                .delivery
                .as_ref()
                .and_then(|delivery| delivery.pull_request.as_ref())
                .map(|pull_request| pull_request.number)
        })
        .flatten();
    RefDetail {
        status: ref_status(status),
        pull_request,
        plan_to_approve: needs_input
            && questions
                .iter()
                .any(|question| question.category == crate::on_demand::APPROVAL_CATEGORY),
        questions,
    }
}

fn ref_status(status: Status) -> RefStatus {
    match status {
        Status::Active(_) => RefStatus::Active,
        Status::NeedsInput { .. } => RefStatus::NeedsInput,
        Status::Parked { .. } => RefStatus::Parked,
    }
}

/// Reads the waits of the data directory `data` at `now`: the project
/// `only`, served or not, or every served project; the slots counted up to
/// `concurrent_runs`.
pub fn gather(data: &Path, only: Option<&Repo>, concurrent_runs: u32, now: Timestamp) -> Read {
    let mut notes = Vec::new();
    let events = super::read_events(data, &mut notes);
    let served = super::served_projects(data, &mut notes);
    let chosen: Vec<(String, Repo, bool)> = match only {
        Some(repo) => {
            let name = repo.to_string();
            match served
                .into_iter()
                .find(|(entry, _)| entry.repository.eq_ignore_ascii_case(&name))
            {
                Some((entry, repo)) => vec![(entry.repository, repo, true)],
                None => vec![(spelled(&events, &name), repo.clone(), false)],
            }
        }
        None => served
            .into_iter()
            .map(|(entry, repo)| (entry.repository, repo, true))
            .collect(),
    };
    gather_folded(
        data,
        chosen,
        concurrent_runs,
        now,
        &mut |repository| why::fold(&events, repository),
        notes,
    )
}

/// [`gather`] for projects already chosen, each `(repository as the events
/// spell it, repo, served)`, with each project's events folded by the
/// caller: `fold_of` is asked for a repository's fold once. The web UI's
/// overview keeps its fold from one read of the event log to the next
/// (OWL-470) and passes the served projects, so it never needs the spelling
/// of one that is not. `notes` is what the caller could not read.
pub fn gather_folded(
    data: &Path,
    chosen: Vec<(String, Repo, bool)>,
    concurrent_runs: u32,
    now: Timestamp,
    fold_of: &mut dyn FnMut(&str) -> Folded,
    mut notes: Vec<String>,
) -> Read {
    let projects = chosen
        .into_iter()
        .map(|(repository, repo, served)| {
            let folded = fold_of(&repository);
            project(data, folded, repository, &repo, served, now, &mut notes)
        })
        .collect();
    let slots = match MachineSlots::new(data, concurrent_runs).holders() {
        Ok(held) => Some((held, concurrent_runs)),
        Err(error) => {
            notes.push(format!("the run slots cannot be probed: {error}"));
            None
        }
    };
    Read {
        at: now,
        machine_pause: requests::machine_pause(data),
        slots,
        projects,
        notes,
    }
}

/// `name` as the event log spells it: the project of its latest event that
/// is `name` in any ASCII case, or `name` when none is. A project's folder
/// is the same whatever the case (`project::component`); its events are
/// folded by their exact spelling.
fn spelled(events: &[owlshift_contracts::event::Event], name: &str) -> String {
    events
        .iter()
        .rev()
        .find(|event| event.project.eq_ignore_ascii_case(name))
        .map_or_else(|| name.to_owned(), |event| event.project.clone())
}

fn project(
    data: &Path,
    mut folded: Folded,
    repository: String,
    repo: &Repo,
    served: bool,
    now: Timestamp,
    notes: &mut Vec<String>,
) -> ProjectRead {
    let dirs = ProjectDirs::new(data, repo);
    let mut refs = Vec::new();
    let mut details = BTreeMap::new();
    if dirs.checkout().exists() {
        match ticket_ref::all(&crate::project::runner_git(), &dirs.checkout()) {
            Ok(all) => {
                refs = all
                    .into_iter()
                    .map(|(ticket, stored)| match stored {
                        Ok(stored) => {
                            let holds = held::holds(&stored);
                            let state = ticket_ref::state_of(&stored);
                            details.insert(ticket.clone(), detail(&stored, &state));
                            (ticket, state, holds)
                        }
                        Err(why) => (ticket, RefState::Unread(why), true),
                    })
                    .collect();
            }
            Err(why) => notes.push(format!(
                "{repository}: the ticket refs cannot be read: {why}"
            )),
        }
    }
    for (ticket, state, _) in &refs {
        if let RefState::Unread(why) = state {
            notes.push(format!(
                "{repository}: the ref of {ticket} cannot be read: {why}"
            ));
        }
    }
    let snapshot = ready::read(&dirs);
    let ready = ready::listed_of(&snapshot, now, &format!("{repository}: "), notes);
    let quarantined = match dirs.quarantine() {
        Ok(marker) => marker.is_some(),
        Err(error) => {
            notes.push(format!(
                "{repository}: the quarantine cannot be read: {error}"
            ));
            false
        }
    };
    if !quarantined {
        folded.current.retain(|_, wait| wait.reason != "quarantine");
    }
    let max_in_flight = match served::read_record(&dirs) {
        Ok(record) => record.and_then(|record| record.kept).and_then(|kept| {
            match ProjectConfig::parse(&kept.text) {
                Ok(config) => Some(config.policy.max_in_flight()),
                Err(error) => {
                    notes.push(format!(
                        "{repository}: the project file it is served on does not read: {error}"
                    ));
                    None
                }
            }
        }),
        Err(why) => {
            notes.push(format!(
                "{repository}: its served record cannot be read: {why}"
            ));
            None
        }
    };
    ProjectRead {
        repository,
        served,
        max_in_flight,
        refs,
        folded,
        quarantined,
        pause: requests::pause(&dirs),
        ready,
        details,
        snapshot,
    }
}

/// A pause file's request as the document says it.
fn paused(request: Option<&Request>) -> Paused {
    Paused {
        by: request.map(|request| request.by.clone()),
        at: request.map(|request| request.at),
        why: request.map(|request| request.why.clone()),
    }
}

/// What `owlshift status --waits` prints for `read`: the document in the
/// terminal's voice, as JSON or as the human text. The command prints this
/// and nothing else, so the web UI's voice never reaches it.
pub fn report(read: &Read, json: bool, tz: &TimeZone) -> String {
    let waits = build(read, Voice::Terminal);
    if json {
        waits.render()
    } else {
        render(&waits, tz)
    }
}

/// The document of `read`, a parked ticket's wait worded in `voice`.
pub fn build(read: &Read, voice: Voice) -> StatusWaits {
    let held: &[HeldSlot] = read.slots.as_ref().map_or(&[], |(held, _)| held);
    let mut notes = read.notes.clone();
    let projects = read
        .projects
        .iter()
        .map(|project| {
            let (waits, left_out) = build_project(project, held, voice);
            if left_out > 0 {
                notes.push(format!(
                    "{}: no ready snapshot reads, so the waits of {left_out} ticket(s) not in \
                     flight are left out (the drain writes `ready.json` at its next pass)",
                    project.repository
                ));
            }
            waits
        })
        .collect();
    StatusWaits {
        format: Format,
        at: read.at,
        machine_pause: read.machine_pause.as_ref().map(|r| paused(r.as_ref())),
        runs: read.slots.as_ref().map(|(held, of)| RunSlots {
            in_use: u32::try_from(held.len()).unwrap_or(u32::MAX),
            of: *of,
        }),
        projects,
        notes,
    }
}

/// The project's document, and how many tickets with a told wait were left
/// out for want of a ready snapshot that reads.
fn build_project(project: &ProjectRead, held: &[HeldSlot], voice: Voice) -> (ProjectWaits, usize) {
    let running: BTreeSet<&TicketId> = held
        .iter()
        .filter_map(|slot| slot.holder.as_ref())
        .filter(|holder| holder.project.eq_ignore_ascii_case(&project.repository))
        .map(|holder| &holder.ticket)
        .collect();
    // Whether `ticket` holds a run slot or the drain's latest snapshot
    // lists it. The snapshot's time is the watch's clock and the events'
    // the system's, so a wait's time is not weighed against it.
    let shown = |ticket: &TicketId| {
        running.contains(ticket)
            || project
                .ready
                .as_ref()
                .is_some_and(|listed| listed.contains(ticket))
    };
    let mut left_out: BTreeSet<&TicketId> = BTreeSet::new();
    let mut tickets: BTreeMap<TicketId, TicketWaits> = BTreeMap::new();
    let blank = |ticket: &TicketId| TicketWaits {
        ticket: ticket.clone(),
        stage: None,
        ref_status: RefStatus::NoRef,
        holds_capacity: false,
        run: None,
        waits: None,
    };
    for (ticket, state, holds) in &project.refs {
        if !holds && !shown(ticket) {
            left_out.insert(ticket);
            continue;
        }
        let state = match state {
            RefState::Read(Some(state)) | RefState::Unadmitted(state) => Some(state),
            RefState::Read(None) | RefState::Unread(_) => None,
        };
        let mut row = blank(ticket);
        row.holds_capacity = holds_slot_or_unknown(state);
        match state {
            Some(state) => {
                let status = state.status();
                row.stage = Some(status.stage());
                row.ref_status = ref_status(status);
            }
            None => row.ref_status = RefStatus::Unread,
        }
        tickets.insert(ticket.clone(), row);
    }
    for (ticket, waiting) in &project.folded.current {
        if !tickets.contains_key(ticket) {
            if NOT_ADMITTED.contains(&waiting.reason.as_str()) {
                continue;
            }
            if !shown(ticket) {
                left_out.insert(ticket);
                continue;
            }
            tickets.insert(ticket.clone(), blank(ticket));
        }
    }
    for slot in held {
        let Some(holder) = &slot.holder else {
            continue;
        };
        if !holder.project.eq_ignore_ascii_case(&project.repository) {
            continue;
        }
        tickets
            .entry(holder.ticket.clone())
            .or_insert_with(|| blank(&holder.ticket))
            .run = Some(HeldRun {
            slot: slot.slot,
            pid: holder.pid,
            since: holder.since,
        });
    }
    for row in tickets.values_mut() {
        row.waits = match row.ref_status {
            RefStatus::NeedsInput => Some(person(
                "needs_input",
                "waits for its decider's answer, and keeps what it holds",
            )),
            RefStatus::Parked => Some(person(
                "parked",
                match voice {
                    Voice::Terminal => {
                        "parked: waits for a person's `owlshift continue`, and keeps what it holds"
                    }
                    // The Restart button is offered exactly when the latest
                    // record is parked (`ui::ticket::actions`).
                    Voice::Ui => "parked: waits for a person's Restart, and keeps what it holds",
                },
            )),
            _ => project.folded.current.get(&row.ticket).map(told),
        };
    }
    let mut tickets: Vec<TicketWaits> = tickets.into_values().collect();
    tickets.sort_by(|a, b| natural_cmp(a.ticket.as_str(), b.ticket.as_str()));
    // Only waits left out for want of a snapshot are worth a note: with one,
    // the drain no longer reading a ticket is the point.
    let unexplained = if project.ready.is_none() {
        left_out
            .iter()
            .filter(|ticket| {
                project
                    .folded
                    .current
                    .get(**ticket)
                    .is_some_and(|wait| !NOT_ADMITTED.contains(&wait.reason.as_str()))
            })
            .count()
    } else {
        0
    };
    let waits = ProjectWaits {
        repository: project.repository.clone(),
        served: project.served,
        max_in_flight: project.max_in_flight,
        in_flight: u32::try_from(tickets.iter().filter(|t| t.holds_capacity).count())
            .unwrap_or(u32::MAX),
        paused: project.pause.as_ref().map(|r| paused(r.as_ref())),
        quarantined: project.quarantined,
        tickets,
    };
    (waits, unexplained)
}

/// A wait for a person, which the ticket's ref says.
fn person(reason: &str, text: &str) -> Wait {
    Wait {
        reason: reason.to_owned(),
        since: None,
        behind: Vec::new(),
        resource: None,
        harness: None,
        until: None,
        text: text.to_owned(),
    }
}

/// A wait the event log tells, with what it named.
pub(crate) fn told(waiting: &Waiting) -> Wait {
    let data = &waiting.data;
    let text = |key: &str| data.get(key).and_then(Value::as_str);
    let mut behind = Vec::new();
    if let Some(number) = data.get("pull_request").and_then(Value::as_u64) {
        behind.push(Behind::PullRequest { number });
    }
    let person = data.get("person").and_then(Value::as_bool) == Some(true);
    for id in data
        .get("tickets")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        if let Ok(id) = TicketId::new(id) {
            behind.push(Behind::Ticket { id, person });
        }
    }
    let resource = data.get("resource").and_then(|value| {
        if let Some(zone) = value.get("zone").and_then(Value::as_str) {
            Some(WaitResource::Zone {
                path: zone.to_owned(),
            })
        } else if let Some(name) = value.get("named").and_then(Value::as_str) {
            Some(WaitResource::Named {
                name: name.to_owned(),
            })
        } else {
            value.get("browser").map(|_| WaitResource::Browser)
        }
    });
    Wait {
        reason: waiting.reason.clone(),
        since: Some(waiting.at),
        behind,
        resource,
        harness: text("harness").map(ToOwned::to_owned),
        until: text("until").and_then(|until| until.parse().ok()),
        text: why::phrase(waiting),
    }
}

/// Whether `row` waits behind another in a chain: a resource it would take
/// or a blocker.
fn chained(row: &TicketWaits) -> bool {
    row.waits.as_ref().is_some_and(|wait| {
        matches!(wait.reason.as_str(), "collision" | "blocked_by") && !wait.behind.is_empty()
    })
}

/// The name of what a ticket waits behind.
fn behind_key(behind: &Behind) -> String {
    match behind {
        Behind::Ticket { id, .. } => id.as_str().to_owned(),
        Behind::PullRequest { number } => format!("pull request #{number}"),
    }
}

fn stage_name(stage: Stage) -> String {
    match serde_json::to_value(stage) {
        Ok(Value::String(name)) => name,
        _ => format!("{stage:?}"),
    }
}

/// A ticket's line: its stage, whether it runs, what it waits on.
fn ticket_line(row: &TicketWaits, tz: &TimeZone) -> String {
    let stage = match (row.ref_status, row.stage) {
        (RefStatus::NoRef, _) => "not dispatched".to_owned(),
        (RefStatus::Unread, _) | (_, None) => "its ref cannot be read".to_owned(),
        (RefStatus::NeedsInput, Some(stage)) => format!("{}, needs input", stage_name(stage)),
        (RefStatus::Parked, Some(stage)) => format!("{}, parked", stage_name(stage)),
        (RefStatus::Active, Some(stage)) => stage_name(stage),
    };
    let mut line = format!("{} [{stage}]", row.ticket);
    if let Some(run) = &row.run {
        let _ = write!(
            line,
            ", running (slot {}, pid {}, since {})",
            run.slot,
            run.pid,
            super::time(run.since, tz)
        );
    }
    if let Some(wait) = &row.waits {
        let _ = write!(line, ": {}", wait.text);
        if let Some(since) = wait.since {
            let _ = write!(line, " (told {})", super::time(since, tz));
        }
    }
    line
}

/// Who asked a pause, when and why.
fn asked(paused: &Paused, tz: &TimeZone) -> String {
    match (&paused.by, paused.at, &paused.why) {
        (Some(by), Some(at), Some(why)) => format!("by {by} at {}: {why}", super::time(at, tz)),
        _ => "its file does not say who asked".to_owned(),
    }
}

/// The document as text, its times in `tz`, safe for a terminal: per
/// project, the holders with what waits behind each, the queues for a slot,
/// the other waits, then the tickets that wait on nothing. A
/// ticket that waits for the project's capacity is listed in its queue, not
/// under the tickets that hold it.
pub fn render(waits: &StatusWaits, tz: &TimeZone) -> String {
    let mut out = String::new();
    match &waits.runs {
        Some(runs) => {
            let _ = writeln!(out, "Run slots: {} of {} in use.", runs.in_use, runs.of);
        }
        None => {
            let _ = writeln!(out, "Run slots: cannot be probed.");
        }
    }
    if let Some(paused) = &waits.machine_pause {
        let _ = writeln!(
            out,
            "Paused: every project of this machine, {}. Nothing is dispatched and no step starts.",
            asked(paused, tz)
        );
    }
    if waits.projects.is_empty() {
        let _ = writeln!(
            out,
            "No project is served (`owlshift start`, in a checkout, serves it)."
        );
    }
    for project in &waits.projects {
        render_project(&mut out, project, waits.runs.as_ref(), tz);
    }
    for note in &waits.notes {
        let _ = writeln!(out, "warning: {note}");
    }
    printable(&out)
}

fn render_project(
    out: &mut String,
    project: &ProjectWaits,
    runs: Option<&RunSlots>,
    tz: &TimeZone,
) {
    let cap = project.max_in_flight.map_or_else(
        || "? (not known: no project file is kept for it to be served on)".to_owned(),
        |cap| cap.to_string(),
    );
    let _ = writeln!(
        out,
        "{}: {} of {cap} in flight (`policy.max_in_flight`){}",
        project.repository,
        project.in_flight,
        if project.served { "" } else { ", not served" }
    );
    if let Some(paused) = &project.paused {
        let _ = writeln!(
            out,
            "  paused {}: nothing is dispatched and no step starts",
            asked(paused, tz)
        );
    }
    if project.quarantined {
        let _ = writeln!(
            out,
            "  quarantined: every command refuses the project (`owlshift unquarantine`)"
        );
    }
    if project.tickets.is_empty() {
        let _ = writeln!(out, "  no ticket in flight or admitted");
        return;
    }
    let by_id: BTreeMap<&str, &TicketWaits> = project
        .tickets
        .iter()
        .map(|row| (row.ticket.as_str(), row))
        .collect();
    // What waits behind each holder, in the tickets' order.
    let mut children: BTreeMap<String, Vec<&TicketWaits>> = BTreeMap::new();
    for row in project.tickets.iter().filter(|row| chained(row)) {
        let first = &row.waits.as_ref().expect("a chained ticket waits").behind[0];
        children.entry(behind_key(first)).or_default().push(row);
    }
    let mut roots: Vec<String> = children
        .keys()
        .filter(|key| by_id.get(key.as_str()).is_none_or(|row| !chained(row)))
        .cloned()
        .collect();
    for row in &project.tickets {
        let id = row.ticket.as_str();
        if row.run.is_some() && !chained(row) && !roots.iter().any(|root| root == id) {
            roots.push(id.to_owned());
        }
    }
    roots.sort_by(|a, b| natural_cmp(a, b));
    let mut printed: BTreeSet<String> = BTreeSet::new();
    let mut tree = String::new();
    for root in &roots {
        walk(&mut tree, root, 4, &by_id, &children, &mut printed, tz, "");
    }
    // What waits in a cycle has no root: each one from its first ticket.
    for row in project.tickets.iter().filter(|row| chained(row)) {
        if !printed.contains(row.ticket.as_str()) {
            let id = row.ticket.as_str().to_owned();
            walk(
                &mut tree,
                &id,
                4,
                &by_id,
                &children,
                &mut printed,
                tz,
                ", in a cycle of waits",
            );
        }
    }
    if !tree.is_empty() {
        let _ = writeln!(out, "  Holders, and what waits behind each:");
        out.push_str(&tree);
    }
    let reason =
        |row: &&TicketWaits, key: &str| row.waits.as_ref().is_some_and(|wait| wait.reason == key);
    let mut queue: Vec<&TicketWaits> = project
        .tickets
        .iter()
        .filter(|row| reason(row, "slot"))
        .collect();
    queue.sort_by_key(|row| row.waits.as_ref().and_then(|wait| wait.since));
    if !queue.is_empty() {
        let holders: Vec<&str> = project
            .tickets
            .iter()
            .filter(|row| row.holds_capacity)
            .map(|row| row.ticket.as_str())
            .collect();
        let cap = project
            .max_in_flight
            .map_or_else(|| "?".to_owned(), |cap| cap.to_string());
        let _ = writeln!(
            out,
            "  Waiting for the project's capacity ({} of {cap} in flight{}):",
            project.in_flight,
            if holders.is_empty() {
                String::new()
            } else {
                format!(": {}", holders.join(", "))
            }
        );
        for row in queue {
            let _ = writeln!(out, "    {}", ticket_line(row, tz));
            printed.insert(row.ticket.as_str().to_owned());
        }
    }
    let machine: Vec<&TicketWaits> = project
        .tickets
        .iter()
        .filter(|row| reason(row, "run_cap") && !printed.contains(row.ticket.as_str()))
        .collect();
    if !machine.is_empty() {
        let count = runs.map_or_else(
            || "cannot be probed".to_owned(),
            |runs| format!("{} of {} in use", runs.in_use, runs.of),
        );
        let _ = writeln!(out, "  Waiting for a machine run slot ({count}):");
        for row in machine {
            let _ = writeln!(out, "    {}", ticket_line(row, tz));
            printed.insert(row.ticket.as_str().to_owned());
        }
    }
    let other: Vec<&TicketWaits> = project
        .tickets
        .iter()
        .filter(|row| row.waits.is_some() && !printed.contains(row.ticket.as_str()))
        .collect();
    if !other.is_empty() {
        let _ = writeln!(out, "  Other waits:");
        for row in other {
            let _ = writeln!(out, "    {}", ticket_line(row, tz));
            printed.insert(row.ticket.as_str().to_owned());
        }
    }
    let rest: Vec<&TicketWaits> = project
        .tickets
        .iter()
        .filter(|row| !printed.contains(row.ticket.as_str()))
        .collect();
    if !rest.is_empty() {
        let _ = writeln!(out, "  Not waiting:");
        for row in rest {
            let _ = writeln!(out, "    {}", ticket_line(row, tz));
        }
    }
}

/// Prints `key` at `indent` and, under it, what waits behind it, each once.
#[allow(clippy::too_many_arguments)]
fn walk(
    out: &mut String,
    key: &str,
    indent: usize,
    by_id: &BTreeMap<&str, &TicketWaits>,
    children: &BTreeMap<String, Vec<&TicketWaits>>,
    printed: &mut BTreeSet<String>,
    tz: &TimeZone,
    mark: &str,
) {
    if !printed.insert(key.to_owned()) {
        return;
    }
    let line = match by_id.get(key) {
        Some(row) => ticket_line(row, tz),
        None if key.starts_with("pull request") => key.to_owned(),
        None => format!("{key}, not in flight here"),
    };
    let _ = writeln!(out, "{:indent$}{line}{mark}", "");
    for child in children.get(key).map_or(&[][..], Vec::as_slice) {
        walk(
            out,
            child.ticket.as_str(),
            indent + 2,
            by_id,
            children,
            printed,
            tz,
            "",
        );
    }
}

#[cfg(test)]
mod tests {
    use owlshift_contracts::event::{Event, EventKind};
    use owlshift_core::state::TicketState;
    use serde_json::json;

    use super::*;
    use crate::events::{Data, data};
    use crate::slots::Holder;

    fn id(ticket: &str) -> TicketId {
        TicketId::new(ticket).unwrap()
    }

    fn event(at: &str, ticket: &str, kind: EventKind, details: Data) -> Event {
        Event {
            format: Default::default(),
            at: at.parse().unwrap(),
            project: "o/r".to_owned(),
            ticket: Some(id(ticket)),
            run: None,
            kind,
            data: details,
        }
    }

    fn waits(at: &str, ticket: &str, pairs: Vec<(&str, Value)>) -> Event {
        let mut details = data([]);
        for (key, value) in pairs {
            details.insert(key.to_owned(), value);
        }
        event(at, ticket, EventKind::Decision, details)
    }

    fn at(status: Status) -> RefState {
        let round = u32::from(status.stage() != Stage::Ready);
        RefState::Read(Some(
            TicketState::restore(status, round, 0, 0, 0, 0).unwrap(),
        ))
    }

    /// A project whose refs all hold and whose snapshot lists every ticket
    /// the events name: every wait told is shown.
    fn project(refs: Vec<(&str, RefState)>, events: &[Event]) -> ProjectRead {
        let listed = events.iter().filter_map(|e| e.ticket.clone()).collect();
        ProjectRead {
            repository: "o/r".to_owned(),
            served: true,
            max_in_flight: Some(1),
            refs: refs.into_iter().map(|(t, s)| (id(t), s, true)).collect(),
            folded: why::fold(events, "o/r"),
            quarantined: false,
            pause: None,
            ready: Some(listed),
            details: BTreeMap::new(),
            snapshot: Ok(None),
        }
    }

    fn read(projects: Vec<ProjectRead>, held: Vec<HeldSlot>) -> Read {
        Read {
            at: "2026-10-28T10:00:00Z".parse().unwrap(),
            machine_pause: None,
            slots: Some((held, 2)),
            projects,
            notes: Vec::new(),
        }
    }

    /// The document in the terminal's voice, which most tests read.
    fn build(read: &Read) -> StatusWaits {
        super::build(read, Voice::Terminal)
    }

    fn row<'a>(waits: &'a StatusWaits, ticket: &str) -> &'a TicketWaits {
        waits.projects[0]
            .tickets
            .iter()
            .find(|row| row.ticket.as_str() == ticket)
            .unwrap_or_else(|| panic!("{ticket} missing in {waits:#?}"))
    }

    /// A Build waiting on a zone names its holder, which runs, and the
    /// zone; a blocked_by chain of two nests both; JSON carries the edges.
    #[test]
    fn a_zone_holder_and_a_chain_are_nested_with_their_edges() {
        let events = [
            waits(
                "2026-10-28T09:00:00Z",
                "DEMO-2",
                vec![
                    ("watch", json!("waits")),
                    ("reason", json!("collision")),
                    ("stage", json!("build")),
                    ("tickets", json!(["DEMO-1"])),
                    ("resource", json!({"zone": "backend/leases"})),
                ],
            ),
            waits(
                "2026-10-28T09:01:00Z",
                "DEMO-3",
                vec![
                    ("drain", json!("waits")),
                    ("reason", json!("blocked_by")),
                    ("tickets", json!(["DEMO-2"])),
                ],
            ),
            waits(
                "2026-10-28T09:02:00Z",
                "DEMO-4",
                vec![
                    ("drain", json!("waits")),
                    ("reason", json!("blocked_by")),
                    ("tickets", json!(["DEMO-3"])),
                ],
            ),
        ];
        let held = vec![HeldSlot {
            slot: 0,
            holder: Some(Holder {
                project: "o/r".to_owned(),
                ticket: id("DEMO-1"),
                pid: 7,
                since: "2026-10-28T08:00:00Z".parse().unwrap(),
            }),
        }];
        let built = build(&read(
            vec![project(
                vec![
                    ("DEMO-1", at(Status::Active(Stage::Build))),
                    (
                        "DEMO-2",
                        RefState::Unadmitted(
                            TicketState::restore(Status::Active(Stage::Build), 1, 0, 0, 0, 0)
                                .unwrap(),
                        ),
                    ),
                ],
                &events,
            )],
            held,
        ));
        assert_eq!(built.projects[0].in_flight, 2);
        let demo2 = row(&built, "DEMO-2").waits.as_ref().unwrap();
        assert_eq!(
            demo2.behind,
            [Behind::Ticket {
                id: id("DEMO-1"),
                person: false
            }]
        );
        assert_eq!(
            demo2.resource,
            Some(WaitResource::Zone {
                path: "backend/leases".to_owned()
            })
        );
        assert_eq!(row(&built, "DEMO-1").run.unwrap().pid, 7);
        assert_eq!(row(&built, "DEMO-4").ref_status, RefStatus::NoRef);
        let text = render(&built, &TimeZone::UTC);
        assert!(
            text.contains(
                "  Holders, and what waits behind each:\n    DEMO-1 [build], running (slot 0, \
                 pid 7, since 2026-10-28 08:00)\n      DEMO-2 [build]: its Build waits: DEMO-1 \
                 holds zone `backend/leases`, which its Build would take (told 2026-10-28 \
                 09:00)\n        DEMO-3 [not dispatched]: blocked by DEMO-2 (told 2026-10-28 \
                 09:01)\n          DEMO-4 [not dispatched]: blocked by DEMO-3 (told \
                 2026-10-28 09:02)\n"
            ),
            "{text}"
        );
        assert!(
            text.starts_with("Run slots: 1 of 2 in use.\no/r: 2 of 1 in flight"),
            "{text}"
        );
    }

    /// A full project shows its queue with the count against its cap; a
    /// ticket dispatched since its wait is not waiting; one the drain told
    /// was not admitted is no row; the ref's person waits win.
    #[test]
    fn the_slot_queue_is_counted_and_a_cleared_wait_is_gone() {
        let slot = |at: &str, ticket: &str| {
            waits(
                at,
                ticket,
                vec![
                    ("drain", json!("waits")),
                    ("reason", json!("slot")),
                    ("tickets", json!(["DEMO-1"])),
                ],
            )
        };
        let events = [
            slot("2026-10-28T09:00:00Z", "DEMO-1"),
            event(
                "2026-10-28T09:01:00Z",
                "DEMO-1",
                EventKind::Dispatch,
                Data::new(),
            ),
            slot("2026-10-28T09:03:00Z", "DEMO-3"),
            slot("2026-10-28T09:02:00Z", "DEMO-2"),
            waits(
                "2026-10-28T09:04:00Z",
                "DEMO-7",
                vec![("drain", json!("waits")), ("reason", json!("not_admitted"))],
            ),
            slot("2026-10-28T09:05:00Z", "DEMO-5"),
        ];
        let built = build(&read(
            vec![project(
                vec![
                    ("DEMO-1", at(Status::Active(Stage::Design))),
                    (
                        "DEMO-5",
                        at(Status::NeedsInput {
                            return_to: Stage::Design,
                        }),
                    ),
                    ("DEMO-6", RefState::Unread("bad".to_owned())),
                ],
                &events,
            )],
            Vec::new(),
        ));
        let tickets: Vec<&str> = built.projects[0]
            .tickets
            .iter()
            .map(|row| row.ticket.as_str())
            .collect();
        assert_eq!(tickets, ["DEMO-1", "DEMO-2", "DEMO-3", "DEMO-5", "DEMO-6"]);
        assert_eq!(row(&built, "DEMO-1").waits, None);
        assert_eq!(
            row(&built, "DEMO-5").waits.as_ref().unwrap().reason,
            "needs_input"
        );
        // DEMO-1, and DEMO-6 whose ref does not read.
        assert_eq!(built.projects[0].in_flight, 2);
        let text = render(&built, &TimeZone::UTC);
        assert!(
            text.contains(
                "  Waiting for the project's capacity (2 of 1 in flight: DEMO-1, DEMO-6):\n    \
                 DEMO-2 [not dispatched]: the project's slot is held by DEMO-1 (told 2026-10-28 \
                 09:02)\n    DEMO-3 [not dispatched]"
            ),
            "{text}"
        );
        assert!(
            text.contains(
                "  Other waits:\n    DEMO-5 [design, needs input]: waits for its decider's \
                 answer, and keeps what it holds\n  Not waiting:\n    DEMO-1 \
                 [design]\n    DEMO-6 [its ref cannot be read]\n"
            ),
            "{text}"
        );
        assert!(!text.contains("DEMO-7"), "{text}");
    }

    /// A project named in another case than its events' is read under
    /// their spelling, so its waits are folded.
    #[test]
    fn a_project_named_in_another_case_takes_the_events_spelling() {
        let events = [waits(
            "2026-10-28T09:00:00Z",
            "DEMO-1",
            vec![("drain", json!("waits")), ("reason", json!("pr_cap"))],
        )];
        assert_eq!(spelled(&events, "O/R"), "o/r");
        assert_eq!(spelled(&events, "acme/web"), "acme/web");
        assert!(why::fold(&events, "O/R").current.is_empty());
        assert_eq!(
            why::fold(&events, &spelled(&events, "O/R")).current.len(),
            1
        );
    }

    /// A pull request holder, a person's ticket, a usage limit's end, a
    /// cycle printed once, the project's pause and quarantine and a cap not
    /// known.
    #[test]
    fn holders_of_every_kind_and_a_cycle_are_told() {
        let events = [
            waits(
                "2026-10-28T09:00:00Z",
                "DEMO-1",
                vec![
                    ("watch", json!("waits")),
                    ("reason", json!("collision")),
                    ("pull_request", json!(12)),
                    ("resource", json!({"named": "migrations"})),
                ],
            ),
            waits(
                "2026-10-28T09:00:00Z",
                "DEMO-2",
                vec![
                    ("drain", json!("waits")),
                    ("reason", json!("blocked_by")),
                    ("tickets", json!(["DEMO-3"])),
                ],
            ),
            waits(
                "2026-10-28T09:00:00Z",
                "DEMO-3",
                vec![
                    ("drain", json!("waits")),
                    ("reason", json!("blocked_by")),
                    ("tickets", json!(["DEMO-2"])),
                ],
            ),
            waits(
                "2026-10-28T09:00:00Z",
                "DEMO-4",
                vec![
                    ("usage", json!("waits")),
                    ("harness", json!("claude")),
                    ("until", json!("2026-10-28T15:00:00Z")),
                ],
            ),
            waits(
                "2026-10-28T09:00:00Z",
                "DEMO-5",
                vec![
                    ("watch", json!("waits")),
                    ("reason", json!("collision")),
                    ("tickets", json!(["LOC-9"])),
                    ("person", json!(true)),
                    ("resource", json!({"zone": ""})),
                ],
            ),
        ];
        let mut one = project(Vec::new(), &events);
        one.max_in_flight = None;
        one.served = false;
        one.quarantined = true;
        one.pause = Some(Some(Request::new(
            "owlshift pause",
            "2026-10-28T08:00:00Z".parse().unwrap(),
            "lunch",
        )));
        let built = build(&read(vec![one], Vec::new()));
        let usage = row(&built, "DEMO-4").waits.as_ref().unwrap();
        assert_eq!(usage.harness.as_deref(), Some("claude"));
        assert_eq!(usage.until, Some("2026-10-28T15:00:00Z".parse().unwrap()));
        assert_eq!(
            row(&built, "DEMO-5").waits.as_ref().unwrap().behind,
            [Behind::Ticket {
                id: id("LOC-9"),
                person: true
            }]
        );
        let text = render(&built, &TimeZone::UTC);
        let has = |part: &str| assert!(text.contains(part), "{part:?} missing in\n{text}");
        has(
            "o/r: 0 of ? (not known: no project file is kept for it to be served on) in flight \
             (`policy.max_in_flight`), not served\n",
        );
        has("  paused by owlshift pause at 2026-10-28 08:00: lunch: nothing is dispatched");
        has("  quarantined: every command refuses the project");
        has(
            "    pull request #12\n      DEMO-1 [not dispatched]: pull request #12 holds resource \
             `migrations`",
        );
        has(
            "    LOC-9, not in flight here\n      DEMO-5 [not dispatched]: LOC-9, which a person \
             works on, holds the whole repository",
        );
        has(
            "    DEMO-2 [not dispatched]: blocked by DEMO-3 (told 2026-10-28 09:00), in a cycle of \
             waits\n      DEMO-3 [not dispatched]: blocked by DEMO-2",
        );
        has(
            "  Other waits:\n    DEMO-4 [not dispatched]: claude is at its usage limit until \
             2026-10-28T15:00:00Z",
        );
        assert_eq!(text.matches("DEMO-3 [").count(), 1, "{text}");
    }

    /// The case seen live in OWL-477: DEMO-1, at Watch with its pull request
    /// ended, was told the holder of a zone in front of DEMO-2, DEMO-3 and
    /// DEMO-4. With a snapshot that lists DEMO-3 only, DEMO-2 and DEMO-1 are
    /// gone, DEMO-3 still waits, and DEMO-1 is named as not in flight; DEMO-4,
    /// out of the column but holding a run slot, stays, with its ref.
    #[test]
    fn a_ticket_the_drain_no_longer_reads_and_an_ended_holder_are_not_shown() {
        let collision = |at: &str, ticket: &str| {
            waits(
                at,
                ticket,
                vec![
                    ("drain", json!("waits")),
                    ("reason", json!("collision")),
                    ("tickets", json!(["DEMO-1"])),
                    ("resource", json!({"zone": "backend/leases"})),
                ],
            )
        };
        let events = [
            collision("2026-10-24T12:48:00Z", "DEMO-2"),
            collision("2026-10-24T12:48:00Z", "DEMO-3"),
            collision("2026-10-24T12:48:00Z", "DEMO-4"),
        ];
        let mut one = project(Vec::new(), &events);
        one.refs = vec![
            (id("DEMO-1"), at(Status::Active(Stage::Watch)), false),
            (id("DEMO-4"), at(Status::Active(Stage::Ready)), false),
        ];
        one.ready = Some(BTreeSet::from([id("DEMO-3")]));
        let held = vec![HeldSlot {
            slot: 1,
            holder: Some(Holder {
                project: "o/r".to_owned(),
                ticket: id("DEMO-4"),
                pid: 9,
                since: "2026-10-28T09:00:00Z".parse().unwrap(),
            }),
        }];
        let built = build(&read(vec![one.clone()], held));
        let shown: Vec<&str> = built.projects[0]
            .tickets
            .iter()
            .map(|row| row.ticket.as_str())
            .collect();
        assert_eq!(shown, ["DEMO-3", "DEMO-4"], "{built:#?}");
        assert_eq!(row(&built, "DEMO-4").stage, Some(Stage::Ready));
        assert!(built.notes.is_empty(), "{:?}", built.notes);
        let text = render(&built, &TimeZone::UTC);
        assert!(
            text.contains("    DEMO-1, not in flight here\n      DEMO-3 [not dispatched]: "),
            "{text}"
        );
        assert!(
            !text.contains("DEMO-1 [watch]") && !text.contains("DEMO-2"),
            "{text}"
        );

        // With no snapshot that reads, nothing out of flight is shown, and
        // a note says how many waits were left out.
        one.ready = None;
        let built = build(&read(vec![one], Vec::new()));
        assert!(built.projects[0].tickets.is_empty(), "{built:#?}");
        assert_eq!(
            built.notes,
            [
                "o/r: no ready snapshot reads, so the waits of 3 ticket(s) not in flight are left \
              out (the drain writes `ready.json` at its next pass)"
            ]
        );
    }

    /// OWL-470: a detail is kept for every ref that reads, the pull request
    /// only for a followed one, the questions of the latest ask, and the plan
    /// to approve only for the approval category.
    #[test]
    fn a_detail_is_kept_for_every_ref_with_its_pull_request_and_questions() {
        use std::num::NonZeroU32;

        use owlshift_contracts::ids::QuestionId;
        use owlshift_contracts::refs::{
            Ask, AskDecider, AskKind, DeliveredPullRequest, TicketDelivery, TicketQuestions,
        };
        use owlshift_core::decider::DeciderRule;

        let question = |category: &str| Question {
            id: QuestionId::new("Q1").unwrap(),
            category: category.to_owned(),
            context: "c".to_owned(),
            text: "t".to_owned(),
            options: Vec::new(),
            recommendation: None,
            replan: false,
        };
        let asking = |category: &str| {
            let mut questions = TicketQuestions::new();
            questions.asks.push(Ask {
                kind: AskKind::Questions,
                round: NonZeroU32::new(1).unwrap(),
                at: "2026-10-28T09:00:00Z".parse().unwrap(),
                comment: "c1".to_owned(),
                questions: vec![question(category)],
                decider: AskDecider {
                    account: "u1".to_owned(),
                    by: DeciderRule::Assignee,
                },
                verdicts: Vec::new(),
                result_refusal: None,
                labels: BTreeMap::new(),
            });
            questions
        };
        let stored =
            |state: TicketState, questions: TicketQuestions, delivery| ticket_ref::Stored {
                record: ticket_ref::TicketRecord {
                    state: owlshift_contracts::refs::PersistedState::from(&state),
                    questions,
                    delivery,
                    intake: None,
                    plan: None,
                    review: None,
                    cost: None,
                    session: None,
                    held: None,
                },
                commit: "0".repeat(40),
                proposals: None,
            };
        let state = |status: Status| {
            let round = u32::from(status.stage() != Stage::Ready);
            TicketState::restore(status, round, 0, 0, 0, 0).unwrap()
        };
        let delivery = TicketDelivery {
            pull_request: Some(DeliveredPullRequest {
                number: 42,
                url: "https://github.com/o/r/pull/42".to_owned(),
            }),
            ..TicketDelivery::new("owlshift/demo-1", "main")
        };
        let read = |stored: &ticket_ref::Stored| detail(stored, &ticket_ref::state_of(stored));

        let watching = read(&stored(
            state(Status::Active(Stage::Watch)),
            TicketQuestions::new(),
            Some(delivery.clone()),
        ));
        assert_eq!(watching.status, RefStatus::Active);
        assert_eq!(watching.pull_request, Some(42));
        assert!(!watching.plan_to_approve && watching.questions.is_empty());

        let approving = read(&stored(
            state(Status::NeedsInput {
                return_to: Stage::Design,
            }),
            asking(crate::on_demand::APPROVAL_CATEGORY),
            None,
        ));
        assert_eq!(approving.status, RefStatus::NeedsInput);
        assert!(approving.plan_to_approve);
        assert_eq!(approving.questions.len(), 1);
        assert_eq!(approving.pull_request, None);

        let asking_scope = read(&stored(
            state(Status::NeedsInput {
                return_to: Stage::Intake,
            }),
            asking("scope"),
            None,
        ));
        assert_eq!(asking_scope.status, RefStatus::NeedsInput);
        assert!(!asking_scope.plan_to_approve);
        assert_eq!(asking_scope.questions[0].category, "scope");

        // A pull request nobody follows (the delivery ended) is no review.
        let mut ended = delivery;
        ended.ended = Some(owlshift_contracts::refs::Ended {
            why: owlshift_contracts::refs::EndedWhy::Merged,
            at: "2026-10-28T10:00:00Z".parse().unwrap(),
            head: None,
        });
        let parked = read(&stored(
            state(Status::Parked {
                at: Stage::Watch,
                awaiting_input: false,
            }),
            TicketQuestions::new(),
            Some(ended),
        ));
        assert_eq!(parked.status, RefStatus::Parked);
        assert_eq!(parked.pull_request, None);
    }

    const PARKED_TERMINAL: &str =
        "parked: waits for a person's `owlshift continue`, and keeps what it holds";
    const PARKED_UI: &str = "parked: waits for a person's Restart, and keeps what it holds";

    fn parked_read() -> Read {
        read(
            vec![project(
                vec![(
                    "DEMO-1",
                    at(Status::Parked {
                        at: Stage::Build,
                        awaiting_input: false,
                    }),
                )],
                &[],
            )],
            Vec::new(),
        )
    }

    /// A parked ticket's wait names the command in the terminal's voice and
    /// the Restart button in the UI's, and nothing else of the wait differs.
    #[test]
    fn a_parked_wait_is_worded_by_voice() {
        let read = parked_read();
        let terminal = super::build(&read, Voice::Terminal);
        let ui = super::build(&read, Voice::Ui);
        let in_terminal = row(&terminal, "DEMO-1").waits.clone().unwrap();
        let in_ui = row(&ui, "DEMO-1").waits.clone().unwrap();
        assert_eq!(in_terminal.text, PARKED_TERMINAL);
        assert_eq!(in_ui.text, PARKED_UI);
        assert_eq!(in_terminal.reason, "parked");
        assert_eq!(
            Wait {
                text: String::new(),
                ..in_terminal
            },
            Wait {
                text: String::new(),
                ..in_ui
            }
        );
    }

    /// The why phrases of a refused merge and of a restart left to `watch`
    /// have no button to name: both voices read the same.
    #[test]
    fn the_why_phrases_read_the_same_in_both_voices() {
        let events = [
            waits(
                "2026-10-28T09:00:00Z",
                "DEMO-1",
                vec![
                    ("watch", json!("merge_refused")),
                    ("pull_request", json!(12)),
                    ("final", json!(true)),
                    ("reason", json!("Base branch was modified.")),
                ],
            ),
            waits(
                "2026-10-28T09:00:00Z",
                "DEMO-2",
                vec![
                    ("restarted", json!(true)),
                    ("stage", json!("build")),
                    ("to_watch", json!(true)),
                    ("takes", json!("resume")),
                ],
            ),
        ];
        let read = read(
            vec![project(
                vec![
                    ("DEMO-1", at(Status::Active(Stage::Watch))),
                    ("DEMO-2", at(Status::Active(Stage::Build))),
                ],
                &events,
            )],
            Vec::new(),
        );
        let terminal = super::build(&read, Voice::Terminal);
        let ui = super::build(&read, Voice::Ui);
        for ticket in ["DEMO-1", "DEMO-2"] {
            let wait = row(&terminal, ticket).waits.as_ref().unwrap();
            assert_eq!(Some(wait), row(&ui, ticket).waits.as_ref());
            assert!(wait.text.contains("`owlshift continue"), "{}", wait.text);
        }
    }

    /// What the command prints, as JSON and as text, keeps the command.
    #[test]
    fn the_report_the_command_prints_names_the_command() {
        let read = parked_read();
        let json = report(&read, true, &TimeZone::UTC);
        let document: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(
            document["projects"][0]["tickets"][0]["waits"]["text"],
            PARKED_TERMINAL
        );
        assert!(!json.contains("Restart"), "{json}");
        let text = report(&read, false, &TimeZone::UTC);
        assert!(text.contains("`owlshift continue`"), "{text}");
        assert!(!text.contains("Restart"), "{text}");
    }
}
