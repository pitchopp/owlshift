//! The web UI's overview (P8 part 4, OWL-470, architecture section 5, "The
//! local web UI"): what is worked on and what is not started, each ticket's
//! stage, what it waits on and what waits for you, from local state only:
//! the ticket refs and the drain's `ready.json` through
//! `owlshift status --waits`' own reader ([`waits`]), and the event log read
//! from its offset ([`EventTail`]). Nothing calls the tracker or the forge.
//!
//! [`build`] is pure. Its JSON is the UI's own, versioned with the binary:
//!
//! - `projects`: each `{"repository", "served", "paused", "quarantined",
//!   "ready", "cards", "list"}`. `ready` is `{"at", "stale"}` of the drain's
//!   latest snapshot, or null. `cards` has one card for each row of
//!   `owlshift status --waits` and one for each admitted ticket of
//!   `ready.json` that has neither a ref nor a row, in a column:
//!   `not_started`, one of the eight stages, `starting` (a run holds a slot
//!   and its ref is not written yet) or `unreadable` (its ref does not
//!   read). `list` names the cards' tickets in the list view's order.
//! - a card: `ticket`, `project`, `title` (ready.json's, else the dispatch
//!   event's), `priority`, `column`, `stage`, `ref_status`, `running`
//!   (`slot`, `since`, `role` of the ticket's latest run), `wait` (the shape
//!   `status --waits --json` prints, worded by `why::phrase`), `needs_you`
//!   (`question`, `plan`, `review`, `parked` or null), `pull_request`,
//!   `strip` (eight cells in pipeline order, `done`, `current` or `todo`),
//!   `position` in `ready.json`, `drain` (what the drain made of a ticket
//!   without a ref);
//! - `for_you`: `{"questions", "plans", "reviews", "parked"}`, entries
//!   `{"ticket", "project", "title", "kind", "pull_request", "questions",
//!   "card"}` built from every ticket ref the way the header counts them
//!   (`why::standings`), whether or not the ticket has a card;
//! - `highlights`: how many highlights the event log's tail absorbed, which
//!   only goes up: the page asks for the "since" panel again when it moves;
//! - `notes`: what could not be read, and the lines of the log that are not
//!   events.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use jiff::Timestamp;
use owlshift_adapters::forge::Repo;
use owlshift_contracts::ids::TicketId;
use owlshift_contracts::ready::{Outcome, ReadySnapshot};
use owlshift_contracts::waits::{Paused, ProjectWaits, RefStatus, StatusWaits, TicketWaits, Wait};
use owlshift_core::schedule::natural_cmp;
use owlshift_core::vocab::{Priority, Stage};
use serde_json::{Value, json};

use super::eventtail::{EventTail, Run, lookup};
use crate::project::ProjectDirs;
use crate::ready;
use crate::status::waits::{self, RefDetail};

/// The column a card is in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Column {
    /// Admitted by the drain and not dispatched: no ref.
    NotStarted,
    Intake,
    Ready,
    Design,
    DesignReview,
    Build,
    Verify,
    Deliver,
    Watch,
    /// A run holds a slot and the ticket's ref is not written yet.
    Starting,
    /// The ticket's ref does not read.
    Unreadable,
}

impl Column {
    fn of(stage: Stage) -> Self {
        match stage {
            Stage::Intake => Self::Intake,
            Stage::Ready => Self::Ready,
            Stage::Design => Self::Design,
            Stage::DesignReview => Self::DesignReview,
            Stage::Build => Self::Build,
            Stage::Verify => Self::Verify,
            Stage::Deliver => Self::Deliver,
            Stage::Watch => Self::Watch,
        }
    }
}

/// What a ticket needs from a person. The order is the list view's.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum NeedsYou {
    /// Questions wait for an answer.
    Question,
    /// A plan waits for its approval.
    Plan,
    /// A pull request waits for a review.
    Review,
    /// Parked: only a person restarts it.
    Parked,
}

/// A cell of a card's strip.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cell {
    Done,
    Current,
    Todo,
}

/// A step that holds a run slot now.
#[derive(Clone, Debug, PartialEq)]
pub struct Running {
    pub slot: u32,
    pub since: Timestamp,
    /// The role of the ticket's latest `run_started`, when the log has one.
    pub role: Option<String>,
}

/// One ticket of the overview.
#[derive(Clone, Debug, PartialEq)]
pub struct Card {
    pub ticket: TicketId,
    pub project: String,
    pub title: Option<String>,
    pub priority: Option<Priority>,
    pub column: Column,
    pub stage: Option<Stage>,
    pub ref_status: RefStatus,
    pub running: Option<Running>,
    pub wait: Option<Wait>,
    pub needs_you: Option<NeedsYou>,
    pub pull_request: Option<u64>,
    pub strip: Vec<Cell>,
    /// Its place in `ready.json`, for a ticket the snapshot lists.
    pub position: Option<usize>,
    /// What the drain made of a ticket the snapshot lists and that has no
    /// ref: `waits`, `taken_in`, `running` or `dispatched`.
    pub drain: Option<&'static str>,
}

/// The drain's latest snapshot, as the page shows it.
#[derive(Clone, Debug, PartialEq)]
pub struct ReadyInfo {
    pub at: Timestamp,
    /// Older than two of its watch's intervals.
    pub stale: bool,
}

/// One project of the overview.
#[derive(Clone, Debug, PartialEq)]
pub struct ProjectView {
    pub repository: String,
    pub served: bool,
    pub paused: Option<Paused>,
    pub quarantined: bool,
    pub ready: Option<ReadyInfo>,
    pub cards: Vec<Card>,
    /// The tickets of `cards`, in the list view's order, each once.
    pub list: Vec<TicketId>,
}

/// One ticket "for you".
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub ticket: TicketId,
    pub project: String,
    pub title: Option<String>,
    pub kind: NeedsYou,
    /// The pull request of a review.
    pub pull_request: Option<u64>,
    /// The questions of a question or a plan.
    pub questions: Vec<Value>,
    /// Whether the ticket has a card in its project.
    pub card: bool,
}

/// What waits for you: questions, plans, pull requests, parked tickets.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ForYou {
    pub questions: Vec<Entry>,
    pub plans: Vec<Entry>,
    pub reviews: Vec<Entry>,
    pub parked: Vec<Entry>,
}

/// The overview.
#[derive(Clone, Debug, PartialEq)]
pub struct Overview {
    pub projects: Vec<ProjectView>,
    pub for_you: ForYou,
    pub highlights: u64,
    pub notes: Vec<String>,
}

/// A value of a contract's type as JSON, `null` when it does not convert.
macro_rules! value_of {
    ($value:expr) => {
        serde_json::to_value($value).unwrap_or(Value::Null)
    };
}

impl Column {
    fn name(self) -> &'static str {
        match self {
            Self::NotStarted => "not_started",
            Self::Intake => "intake",
            Self::Ready => "ready",
            Self::Design => "design",
            Self::DesignReview => "design_review",
            Self::Build => "build",
            Self::Verify => "verify",
            Self::Deliver => "deliver",
            Self::Watch => "watch",
            Self::Starting => "starting",
            Self::Unreadable => "unreadable",
        }
    }
}

impl NeedsYou {
    fn name(self) -> &'static str {
        match self {
            Self::Question => "question",
            Self::Plan => "plan",
            Self::Review => "review",
            Self::Parked => "parked",
        }
    }
}

impl Cell {
    fn name(self) -> &'static str {
        match self {
            Self::Done => "done",
            Self::Current => "current",
            Self::Todo => "todo",
        }
    }
}

impl Card {
    fn json(&self) -> Value {
        json!({
            "ticket": self.ticket,
            "project": self.project,
            "title": self.title,
            "priority": self.priority.as_ref().map(|value| value_of!(value)),
            "column": self.column.name(),
            "stage": self.stage.as_ref().map(|value| value_of!(value)),
            "ref_status": value_of!(&self.ref_status),
            "running": self.running.as_ref().map(|run| json!({
                "slot": run.slot,
                "since": run.since.to_string(),
                "role": run.role,
            })),
            "wait": self.wait.as_ref().map(|value| value_of!(value)),
            "needs_you": self.needs_you.map(NeedsYou::name),
            "pull_request": self.pull_request,
            "strip": self.strip.iter().map(|cell| cell.name()).collect::<Vec<_>>(),
            "position": self.position,
            "drain": self.drain,
        })
    }
}

impl ProjectView {
    fn json(&self) -> Value {
        json!({
            "repository": self.repository,
            "served": self.served,
            "paused": self.paused.as_ref().map(|value| value_of!(value)),
            "quarantined": self.quarantined,
            "ready": self.ready.as_ref().map(|ready| json!({
                "at": ready.at.to_string(),
                "stale": ready.stale,
            })),
            "cards": self.cards.iter().map(Card::json).collect::<Vec<_>>(),
            "list": self.list,
        })
    }
}

impl Entry {
    fn json(&self) -> Value {
        json!({
            "ticket": self.ticket,
            "project": self.project,
            "title": self.title,
            "kind": self.kind.name(),
            "pull_request": self.pull_request,
            "questions": self.questions,
            "card": self.card,
        })
    }
}

impl Overview {
    /// Its JSON.
    pub fn json(&self) -> Value {
        let entries = |list: &[Entry]| list.iter().map(Entry::json).collect::<Vec<_>>();
        json!({
            "projects": self.projects.iter().map(ProjectView::json).collect::<Vec<_>>(),
            "for_you": {
                "questions": entries(&self.for_you.questions),
                "plans": entries(&self.for_you.plans),
                "reviews": entries(&self.for_you.reviews),
                "parked": entries(&self.for_you.parked),
            },
            "highlights": self.highlights,
            "notes": self.notes,
        })
    }
}

/// What [`build`] needs of a project besides its `status --waits` project.
#[derive(Clone, Debug, Default)]
pub struct Aux {
    /// Every ref that reads, by ticket.
    pub details: BTreeMap<TicketId, RefDetail>,
    /// The drain's latest snapshot, when there is one that reads.
    pub ready: Option<ReadySnapshot>,
}

type Titles = BTreeMap<(String, TicketId), String>;
type Runs = BTreeMap<(String, TicketId), Run>;

/// The overview of `waits`, whose `n`th project `aux[n]` completes.
pub fn build(
    waits: &StatusWaits,
    aux: &[Aux],
    titles: &Titles,
    runs: &Runs,
    now: Timestamp,
    highlights: u64,
    extra_notes: Vec<String>,
) -> Overview {
    let empty = Aux::default();
    let mut projects = Vec::new();
    let mut for_you = Vec::new();
    for (n, project) in waits.projects.iter().enumerate() {
        let aux = aux.get(n).unwrap_or(&empty);
        let view = project_view(project, aux, titles, runs, now);
        for_you.extend(entries(&view, aux, titles));
        projects.push(view);
    }
    for_you.sort_by(|a, b| {
        a.project
            .cmp(&b.project)
            .then_with(|| natural_cmp(a.ticket.as_str(), b.ticket.as_str()))
    });
    let mut lists = ForYou::default();
    for entry in for_you {
        match entry.kind {
            NeedsYou::Question => lists.questions.push(entry),
            NeedsYou::Plan => lists.plans.push(entry),
            NeedsYou::Review => lists.reviews.push(entry),
            NeedsYou::Parked => lists.parked.push(entry),
        }
    }
    let mut notes = waits.notes.clone();
    notes.extend(extra_notes);
    Overview {
        projects,
        for_you: lists,
        highlights,
        notes,
    }
}

fn needs_of(detail: &RefDetail) -> Option<NeedsYou> {
    match detail.status {
        RefStatus::NeedsInput if detail.plan_to_approve => Some(NeedsYou::Plan),
        RefStatus::NeedsInput => Some(NeedsYou::Question),
        RefStatus::Parked => Some(NeedsYou::Parked),
        RefStatus::Active if detail.pull_request.is_some() => Some(NeedsYou::Review),
        _ => None,
    }
}

fn strip_of(stage: Option<Stage>) -> Vec<Cell> {
    Stage::ALL
        .iter()
        .map(|cell| match stage {
            Some(stage) if *cell < stage => Cell::Done,
            Some(stage) if *cell == stage => Cell::Current,
            _ => Cell::Todo,
        })
        .collect()
}

fn drain_of(outcome: &Outcome) -> &'static str {
    match outcome {
        Outcome::TakenIn => "taken_in",
        Outcome::Running => "running",
        Outcome::Dispatched => "dispatched",
        Outcome::Waits { .. } | Outcome::NotAdmitted { .. } => "waits",
    }
}

fn project_view(
    project: &ProjectWaits,
    aux: &Aux,
    titles: &Titles,
    runs: &Runs,
    now: Timestamp,
) -> ProjectView {
    let repository = &project.repository;
    let listed: BTreeMap<&TicketId, (usize, &owlshift_contracts::ready::ReadyTicket)> = aux
        .ready
        .iter()
        .flat_map(|snapshot| snapshot.tickets.iter().enumerate())
        .map(|(place, ticket)| (&ticket.ticket, (place, ticket)))
        .collect();
    let title_of = |ticket: &TicketId| {
        listed
            .get(ticket)
            .map(|(_, ready)| ready.title.clone())
            .or_else(|| lookup(titles, repository, ticket).cloned())
    };
    let mut cards: Vec<Card> = project
        .tickets
        .iter()
        .map(|row| {
            let snapshot = listed.get(&row.ticket);
            let detail = aux.details.get(&row.ticket);
            let column = column_of(row, snapshot.is_some());
            let drain = (row.ref_status == RefStatus::NoRef)
                .then(|| snapshot.map(|(_, ready)| drain_of(&ready.drain)))
                .flatten();
            Card {
                ticket: row.ticket.clone(),
                project: repository.clone(),
                title: title_of(&row.ticket),
                priority: snapshot.map(|(_, ready)| ready.priority),
                column,
                stage: row.stage,
                ref_status: row.ref_status,
                running: row.run.map(|run| Running {
                    slot: run.slot,
                    since: run.since,
                    // Only a run that started once the slot was taken: before
                    // its own `run_started`, the slot is a new step's and the
                    // latest run is the previous step's (as `status` has it).
                    role: lookup(runs, repository, &row.ticket)
                        .filter(|latest| latest.at >= run.since)
                        .map(|latest| latest.role.clone()),
                }),
                wait: row.waits.clone(),
                needs_you: detail.and_then(needs_of),
                pull_request: detail.and_then(|detail| detail.pull_request),
                strip: strip_of(row.stage),
                position: snapshot.map(|(place, _)| *place),
                drain,
            }
        })
        .collect();
    // The admitted tickets of the snapshot that have neither a ref nor a
    // row: not started, whatever `status --waits` says of them.
    let rows: BTreeSet<&TicketId> = project.tickets.iter().map(|row| &row.ticket).collect();
    if let Some(snapshot) = &aux.ready {
        for (place, ready) in snapshot.tickets.iter().enumerate() {
            if rows.contains(&ready.ticket) || matches!(ready.drain, Outcome::NotAdmitted { .. }) {
                continue;
            }
            cards.push(Card {
                ticket: ready.ticket.clone(),
                project: repository.clone(),
                title: Some(ready.title.clone()),
                priority: Some(ready.priority),
                column: Column::NotStarted,
                stage: None,
                ref_status: RefStatus::NoRef,
                running: None,
                wait: match &ready.drain {
                    Outcome::Waits { wait } => Some(wait.clone()),
                    _ => None,
                },
                needs_you: None,
                pull_request: None,
                strip: strip_of(None),
                position: Some(place),
                drain: Some(drain_of(&ready.drain)),
            });
        }
    }
    let list = list_order(&cards);
    ProjectView {
        repository: repository.clone(),
        served: project.served,
        paused: project.paused.clone(),
        quarantined: project.quarantined,
        ready: aux.ready.as_ref().map(|snapshot| ReadyInfo {
            at: snapshot.at,
            stale: ready::stale(snapshot, now),
        }),
        cards,
        list,
    }
}

fn column_of(row: &TicketWaits, listed: bool) -> Column {
    match (row.ref_status, row.stage) {
        (RefStatus::Unread, _) => Column::Unreadable,
        (_, Some(stage)) => Column::of(stage),
        (_, None) if listed => Column::NotStarted,
        (_, None) => Column::Starting,
    }
}

/// The list view's order: the tickets that need you first (questions, plans,
/// pull requests, parked, then by ticket), then the others in flight from
/// Watch back to Intake (then by ticket), the tickets that are starting, the
/// unreadable ones, and the not-started ones in `ready.json`'s order.
fn list_order(cards: &[Card]) -> Vec<TicketId> {
    let mut order: Vec<&Card> = cards.iter().collect();
    order.sort_by(|a, b| {
        let group = |card: &Card| match card.column {
            _ if card.needs_you.is_some() => 0,
            Column::Starting => 2,
            Column::Unreadable => 3,
            Column::NotStarted => 4,
            _ => 1,
        };
        group(a)
            .cmp(&group(b))
            .then_with(|| match group(a) {
                0 => a.needs_you.cmp(&b.needs_you),
                1 => b.stage.cmp(&a.stage),
                4 => a.position.cmp(&b.position),
                _ => std::cmp::Ordering::Equal,
            })
            .then_with(|| natural_cmp(a.ticket.as_str(), b.ticket.as_str()))
    });
    order.into_iter().map(|card| card.ticket.clone()).collect()
}

/// The "for you" entries of a project, from every ref that reads.
fn entries(view: &ProjectView, aux: &Aux, titles: &Titles) -> Vec<Entry> {
    aux.details
        .iter()
        .filter_map(|(ticket, detail)| {
            let kind = needs_of(detail)?;
            let card = view.cards.iter().find(|card| card.ticket == *ticket);
            let title = card.and_then(|card| card.title.clone()).or_else(|| {
                aux.ready
                    .iter()
                    .flat_map(|snapshot| &snapshot.tickets)
                    .find(|ready| ready.ticket == *ticket)
                    .map(|ready| ready.title.clone())
                    .or_else(|| lookup(titles, &view.repository, ticket).cloned())
            });
            Some(Entry {
                ticket: ticket.clone(),
                project: view.repository.clone(),
                title,
                kind,
                pull_request: detail.pull_request.filter(|_| kind == NeedsYou::Review),
                questions: match kind {
                    NeedsYou::Question | NeedsYou::Plan => detail
                        .questions
                        .iter()
                        .filter_map(|question| serde_json::to_value(question).ok())
                        .collect(),
                    _ => Vec::new(),
                },
                card: card.is_some(),
            })
        })
        .collect()
}

/// Reads the overview of the data directory `data` at `now`: advances
/// `tail`, then reads what `owlshift status --waits` reads for every served
/// project, with the tail's folds in place of a whole read of the log, and
/// each project's `ready.json`.
pub fn gather(data: &Path, tail: &mut EventTail, concurrent_runs: u32, now: Timestamp) -> Overview {
    let mut notes = Vec::new();
    if let Err(why) = tail.advance() {
        notes.push(format!("the event log cannot be read: {why}"));
    }
    let chosen: Vec<(String, Repo, bool)> = crate::status::served_projects(data, &mut notes)
        .into_iter()
        .map(|(entry, repo)| (entry.repository, repo, true))
        .collect();
    let repos: Vec<(String, Repo)> = chosen
        .iter()
        .map(|(repository, repo, _)| (repository.clone(), repo.clone()))
        .collect();
    let tail: &EventTail = tail;
    let read = waits::gather_folded(
        data,
        chosen,
        concurrent_runs,
        now,
        &mut |repository| tail.folded(repository),
        notes,
    );
    let built = waits::build(&read);
    let aux: Vec<Aux> = read
        .projects
        .iter()
        .map(|project| {
            let snapshot = repos
                .iter()
                .find(|(repository, _)| *repository == project.repository)
                .and_then(|(_, repo)| ready::read(&ProjectDirs::new(data, repo)).ok().flatten());
            Aux {
                details: project.details.clone(),
                ready: snapshot,
            }
        })
        .collect();
    build(
        &built,
        &aux,
        tail.titles(),
        tail.runs(),
        now,
        tail.highlights(),
        tail.notes(),
    )
}

#[cfg(test)]
mod tests;
