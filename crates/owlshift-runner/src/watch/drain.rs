//! `owlshift watch --drain` (OWL-250; architecture section 5, "The drain";
//! build plan, "P4 · The drain", part G): once watch's own work of a pass is
//! done, the drain reads the ticket refs again, then the ready column, takes
//! at most one admitted ticket in (intake), none while one's step runs, and
//! dispatches the ready tickets the core picks ([`schedule::ready_set`]), a
//! list since OWL-401, up to the project's capacity, each through `do`'s
//! path ([`OnDemand::dispatch_seen`](crate::on_demand::OnDemand::dispatch_seen)),
//! whose admission weighs the work outside Owlshift this pass read.
//!
//! The refs are read again after watch's work, so a merge watch read in this
//! pass releases the ticket it blocked in the same pass ("Unblocked by a
//! merge"). Intake goes first, highest priority first, whatever the blockers;
//! a ticket taken in is left out of the pass's ready set, so the relations
//! its intake wrote are read by the next listing. A ticket ref that cannot be
//! read stops every dispatch, since nothing tells whether it holds the slot,
//! and that ticket is neither taken in nor dispatched; one line says so.
//!
//! Each change of a waiting ticket's reason is one `decision` event (`drain`
//! = `waits`) and one line; a gesture that does not admit is one `warning`
//! event (`not_admitted`) and one line per ticket and account, and the ticket
//! is tried again after [`RETRY_AFTER`]. An intake or a dispatch is held as
//! a continue is, in watch's own [`Holds`](super::Holds). Neither starts
//! while the harness its first run needs is held by a usage limit, which
//! the machine's usage state says (OWL-404): the drain then takes nothing in
//! and dispatches nothing.
//!
//! The open pull request cap (OWL-251, part H): the pull requests counted
//! are the ticket refs whose delivery is still followed, whatever their
//! ticket's state, with no call to the forge. At `policy.max_open_prs` no
//! ticket is dispatched, and intake goes on. Reaching the cap is one
//! `decision` event (`drain` = `pr_cap`, with the count and the cap) and one
//! line, not one per pass; the tickets it holds back are not told one by
//! one. The dispatch counts again from the refs read under its lock
//! (OWL-313), so a pull request another command delivered since the
//! snapshot holds it too, told the same way.
//!
//! The ready snapshot (OWL-468, [`crate::ready`]): a pass that listed the
//! column writes, at its end, every ticket of it in the core's order with
//! what the pass made of it (taken in, running, dispatched, waiting and
//! why, not admitted and why) and the blockers it weighed. A pass that
//! listed nothing writes nothing.

use std::collections::HashMap;
use std::io::Write;

use jiff::Timestamp;
use serde_json::json;

use owlshift_adapters::tracker::{Column, Error, ErrorKind, Listed, StateType};
use owlshift_contracts::config::{Admit as Gesture, ProjectConfig};
use owlshift_contracts::event::EventKind;
use owlshift_contracts::format::Format;
use owlshift_contracts::ids::TicketId;
use owlshift_contracts::ready::{
    BlockerState as ReadyState, Outcome, ReadyBlocker, ReadySnapshot, ReadyTicket,
};
use owlshift_contracts::refs::EndedWhy;
use owlshift_contracts::step::{StepEnd, StepKind};
use owlshift_contracts::{Role, Variant};
use owlshift_core::schedule::{self, Caps, Load, Wait, natural_cmp};
use owlshift_core::state::{Status, TicketState};
use owlshift_core::ticket::{Admission, Blocker, BlockerState, Ticket, text_blockers};
use owlshift_core::vocab::{Harness, Stage};

use super::{Admit, Continued, Memory as WatchMemory, RETRY_AFTER, Seen, Then, Watch};
use crate::events::{Data, EventSink, data};
use crate::in_flight::{self, Kind, Outside};
use crate::on_demand::{self, delivery_followed};
use crate::quarantine::Held;
use crate::ticket_ref::{self, RefAt, Stored, text_sha256};
use crate::usage as machine_usage;
use crate::{held, ready};

/// Why `owlshift watch --drain` refuses to start on a project configured
/// so, from its working tree (architecture section 5, "The gesture", "By
/// whom"); the drain reads `admitters` again at the base commit before each
/// intake and dispatch.
pub fn refusal(config: &ProjectConfig) -> Option<String> {
    if config.tracker.admit == Gesture::Delegation {
        return Some(
            "`owlshift watch --drain` admits by a label or a state: the `delegation` gesture is \
             not built; set `[tracker].admit` in owlshift.toml (`owlshift doctor` checks it)"
                .to_owned(),
        );
    }
    if config.tracker.admitters.is_none() {
        return Some(
            "`owlshift watch --drain` needs `[tracker].admitters` in owlshift.toml, the tracker \
             accounts whose gesture admits a ticket (`owlshift doctor` checks them)"
                .to_owned(),
        );
    }
    None
}

/// What the drain keeps from one pass to the next.
#[derive(Default)]
pub(super) struct Memory {
    /// The tickets of the column last listed: watch's holds of them are
    /// kept.
    pub(super) listed: Vec<TicketId>,
    /// Each waiting ticket's reason as last told.
    waits: HashMap<TicketId, String>,
    /// The account of each gesture warned of as not admitting, and when
    /// the ticket is tried again.
    warned: HashMap<TicketId, (String, Timestamp)>,
    /// Why the column could not be read, as last said.
    unlisted: Option<String>,
    /// The ticket refs that could not be read, as last said.
    unread: Vec<TicketId>,
    /// Whether the cap was told as reached and the count has not fallen
    /// below it since.
    capped: bool,
    /// Why the ready snapshot could not be written, as last said.
    unwritten: Option<String>,
}

/// What the pass's intake did.
enum Intake {
    /// It ran on no ticket.
    None,
    /// It went to `ticket`: its step ran or started (`started`), or found
    /// the project, the ticket or the machine's run slots taken.
    Took { ticket: TicketId, started: bool },
    /// Intake's harness is held by a usage limit until `until_ms`: no
    /// intake runs (OWL-404).
    UsageLimit { harness: Harness, until_ms: i64 },
}

/// What a dispatch did.
enum Dispatch {
    /// Its step ran or started.
    Started,
    /// Watch's hold keeps the ticket back: its last step left its ref as it
    /// was.
    Held,
    /// Build's harness is held by a usage limit until `until_ms`.
    UsageLimit { harness: Harness, until_ms: i64 },
    /// Its step found the project, the ticket or the machine's run slots
    /// taken, or the ref moved: the next pass tries again.
    NotStarted,
}

/// A wait of the ready snapshot.
fn waits(ticket: &TicketId, reason: &str, data: Data) -> Outcome {
    Outcome::Waits {
        wait: ready::wait(ticket, reason, data),
    }
}

/// What a usage limit's event carries.
fn usage_data(harness: Harness, until_ms: i64) -> Data {
    data([
        ("harness", json!(machine_usage::key(harness))),
        ("until", json!(machine_usage::at(until_ms))),
    ])
}

/// What the drain's `waits` decision for `wait` carries besides its reason:
/// the holder a collision names, as a pull request or a person's ticket
/// (OWL-410), or the tickets it names; the resource.
fn wait_data(wait: &Wait<String>, kinds: &HashMap<String, Kind>) -> Data {
    let mut event = Data::new();
    if let Wait::Collision(found) = wait {
        let holder = in_flight::named(kinds, &found.holder);
        for (key, value) in holder.data() {
            event.insert(key.to_owned(), value);
        }
    } else if !wait.tickets().is_empty() {
        event.insert("tickets".to_owned(), json!(wait.tickets()));
    }
    if let Some(resource) = wait.resource() {
        event.insert("resource".to_owned(), held::resource_data(resource));
    }
    event
}

/// A blocker as the ready snapshot keeps it.
fn ready_blocker(blocker: Blocker<String>) -> ReadyBlocker {
    ReadyBlocker {
        ticket: blocker.id,
        state: match blocker.tracker {
            BlockerState::Open => ReadyState::Open,
            BlockerState::Completed => ReadyState::Completed,
            BlockerState::Canceled => ReadyState::Canceled,
            BlockerState::Unknown => ReadyState::Unknown,
        },
        merged: blocker.merged,
    }
}

/// Whether the ticket ref of `id` records its pull request merged.
fn merged_in(refs: &HashMap<TicketId, Stored>, id: &str) -> bool {
    TicketId::new(id)
        .ok()
        .and_then(|id| refs.get(&id))
        .and_then(|stored| stored.record.delivery.as_ref())
        .and_then(|delivery| delivery.ended.as_ref())
        .is_some_and(|ended| ended.why == EndedWhy::Merged)
}

/// A row's relations, with what the listing shows of each blocker and
/// whether its ticket ref records its pull request merged: no read.
fn relations(row: &Row, refs: &HashMap<TicketId, Stored>) -> Vec<Blocker<String>> {
    row.listed
        .blockers
        .iter()
        .map(|blocker| Blocker {
            id: blocker.id.as_str().to_owned(),
            tracker: relation_state(blocker.state),
            merged: merged_in(refs, blocker.id.as_str()),
        })
        .collect()
}

impl Memory {
    fn retain(&mut self, listed: &[TicketId]) {
        self.waits.retain(|ticket, _| listed.contains(ticket));
        self.warned.retain(|ticket, _| listed.contains(ticket));
    }
}

/// A ticket of the column with what its ticket ref keeps.
struct Row {
    listed: Listed,
    stored: Option<Stored>,
    /// Its state, `None` when the ref keeps none that reads.
    state: Option<TicketState>,
    /// The hash of its current text.
    text: String,
}

impl Row {
    fn new(listed: Listed, stored: Option<Stored>) -> Self {
        let state = match &stored {
            Some(stored) => TicketState::try_from(&stored.record.state).ok(),
            None => Some(TicketState::admitted()),
        };
        let text = text_sha256(&listed.ticket.title, &listed.ticket.description);
        Row {
            listed,
            stored,
            state,
            text,
        }
    }

    /// A ticket of the column whose ref cannot be read: no state.
    fn unread(listed: Listed) -> Self {
        let mut row = Row::new(listed, None);
        row.state = None;
        row
    }

    fn id(&self) -> &TicketId {
        &self.listed.ticket.id
    }

    fn intake_hash(&self) -> Option<&str> {
        let record = self.stored.as_ref()?.record.intake.as_ref()?;
        Some(record.text_sha256.as_str())
    }

    fn followed(&self) -> bool {
        self.stored.as_ref().is_some_and(delivery_followed)
    }

    fn excluded(&self, exclude: Option<&str>) -> bool {
        exclude.is_some_and(|label| self.listed.ticket.labels.iter().any(|l| l == label))
    }

    /// Whether intake runs on it (architecture section 5, "Intake", "When"):
    /// not excluded, with a decider, and its ref at Intake, an intake that
    /// has not ended, whatever its record; or its ref absent, at Ready, or at
    /// Watch with its following ended, its intake record absent or for
    /// another text. A ref at Intake in the column is one a person moved
    /// back after a round, or the drain's own intake cut off (OWL-275).
    fn intake_due(&self, exclude: Option<&str>) -> bool {
        let Some(state) = &self.state else {
            return false;
        };
        let stale = self.intake_hash() != Some(self.text.as_str());
        let due = match state.status() {
            Status::Active(Stage::Intake) => true,
            Status::Active(Stage::Ready) => stale,
            Status::Active(Stage::Watch) => !self.followed() && stale,
            _ => false,
        };
        // Intake runs only on a ticket its assignee or labels give a
        // decider: the zones of its record count once it ended (OWL-412).
        due && !self.excluded(exclude) && on_demand::has_decider(&self.listed.ticket, &[])
    }

    /// The core's order: priority, then the first created, then the
    /// identifier.
    fn order(&self, other: &Row) -> std::cmp::Ordering {
        let created = |row: &Row| row.listed.created_at.map_or(0, Timestamp::as_millisecond);
        self.listed
            .ticket
            .priority
            .cmp(&other.listed.ticket.priority)
            .then(created(self).cmp(&created(other)))
            .then_with(|| natural_cmp(self.id().as_str(), other.id().as_str()))
    }
}

/// The blocker state of a relation's blocker as the listing gives it: only
/// `completed` releases; a canceled or duplicate one holds the ticket until
/// a person acts; any other, or none (a Markdown stage other than `Done`, a
/// `blocked_by` with no ticket folder), is open.
fn relation_state(state: Option<StateType>) -> BlockerState {
    match state {
        Some(StateType::Completed) => BlockerState::Completed,
        Some(StateType::Canceled | StateType::Duplicate) => BlockerState::Canceled,
        _ => BlockerState::Open,
    }
}

/// The blocker state of a "Blocked by" identifier as its read answered
/// (build plan, part G, OWL-264): a definite "not found" is unknown, any
/// other failure open, so the next pass reads it again.
fn text_state(read: Result<Option<StateType>, Error>) -> BlockerState {
    match read {
        Ok(state) => relation_state(state),
        Err(error) if error.kind == ErrorKind::NotFound => BlockerState::Unknown,
        Err(_) => BlockerState::Open,
    }
}

impl Watch<'_> {
    /// The drain's part of a pass, after watch's own work.
    pub(super) fn drain(
        &self,
        memory: &mut WatchMemory,
        sink: &mut EventSink<'_>,
        out: &mut dyn Write,
        continued: &mut Continued<'_>,
    ) {
        let on_demand = self.on_demand;
        // An operator's pause, one written during the pass included
        // (OWL-438): nothing is taken in or dispatched, nor read for it.
        if self.operator_paused(memory, sink, out) {
            return;
        }
        // Read again: watch's work may have ended a following or closed a
        // round in this pass. A project busy now waits for the next pass,
        // which watch's own note tells. A quarantined one takes nothing in,
        // and its ticket the ready set would dispatch is told that it waits
        // (OWL-368).
        let Ok(snapshot) = self.snapshot(memory, sink) else {
            return;
        };
        let quarantined = snapshot.quarantine.is_some();
        // Cleared since watch's own snapshot: folded before any dispatch
        // (OWL-423).
        if !quarantined {
            memory.quarantine_comments.fold_all(&mut self.board(sink));
        }
        let read = snapshot.refs;
        let mut refs = HashMap::new();
        let mut unread = Vec::new();
        for (ticket, stored) in read {
            match stored {
                Ok(stored) => {
                    refs.insert(ticket, stored);
                }
                Err(_) => unread.push(ticket),
            }
        }
        let tracker = &on_demand.config.tracker;
        let column = Column {
            team: tracker.team.as_deref(),
            ready: &tracker.states.ready,
            admit: &tracker.admit,
        };
        let listed = match on_demand.tracker.admitted(&column) {
            Ok(listed) => listed,
            Err(error) => {
                // Nothing kept is forgotten: the next listing goes on from it.
                let reason = error.to_string();
                if memory.drain.unlisted.as_ref() != Some(&reason) {
                    self.line(
                        out,
                        &format!(
                            "the ready column could not be read, so the drain takes nothing in: \
                             {reason}"
                        ),
                    );
                    memory.drain.unlisted = Some(reason);
                }
                return;
            }
        };
        if memory.drain.unlisted.take().is_some() {
            self.line(out, "the ready column reads again");
        }
        let ids: Vec<TicketId> = listed.iter().map(|l| l.ticket.id.clone()).collect();
        memory.drain.retain(&ids);
        memory.drain.listed = ids;
        if memory.drain.unread != unread {
            if !unread.is_empty() {
                let names: Vec<&str> = unread.iter().map(TicketId::as_str).collect();
                self.line(
                    out,
                    &format!(
                        "nothing is dispatched while the ticket refs of {} cannot be read",
                        names.join(", ")
                    ),
                );
            }
            memory.drain.unread = unread.clone();
        }
        // A ticket whose ref cannot be read is weighed by nothing: the ready
        // snapshot alone keeps it (OWL-468).
        let (mut rows, mut unread_rows) = (Vec::new(), Vec::new());
        for listed in listed {
            if unread.contains(&listed.ticket.id) {
                unread_rows.push(Row::unread(listed));
            } else {
                let stored = refs.get(&listed.ticket.id).cloned();
                rows.push(Row::new(listed, stored));
            }
        }
        rows.sort_by(Row::order);
        // What the pass made of each ticket, for the ready snapshot.
        let mut made: HashMap<TicketId, Outcome> = HashMap::new();
        for row in &unread_rows {
            made.insert(row.id().clone(), waits(row.id(), "ref_unread", Data::new()));
        }
        // The tickets whose step's child runs (OWL-401) are neither taken in
        // nor dispatched again; a dispatch's holds the slot, as one of this
        // pass's, until its admission writes its ref.
        let busy: Vec<TicketId> = memory
            .running
            .iter()
            .map(|running| running.ticket().clone())
            .collect();
        let dispatching: Vec<TicketId> = memory
            .running
            .iter()
            .filter(|running| running.dispatch())
            .map(|running| running.ticket().clone())
            .collect();

        // Under quarantine no intake runs, nor is its candidate told: its
        // gesture is checked at the base commit under the lock, which the
        // quarantine refuses, and a ticket at Intake is not ready yet.
        let intake = if quarantined {
            Intake::None
        } else {
            self.take_in(memory, &rows, &busy, sink, out, continued)
        };
        for row in rows.iter().filter(|row| busy.contains(row.id())) {
            made.insert(row.id().clone(), Outcome::Running);
        }
        let taken = match &intake {
            Intake::Took { ticket, started } => {
                let outcome = if *started {
                    Outcome::TakenIn
                } else {
                    waits(ticket, "next_pass", Data::new())
                };
                made.insert(ticket.clone(), outcome);
                Some(ticket.clone())
            }
            _ => None,
        };
        for row in rows.iter().filter(|row| row.state.is_none()) {
            made.entry(row.id().clone())
                .or_insert_with(|| waits(row.id(), "ref_unread", Data::new()));
        }
        let exclude = tracker.exclude.as_deref();
        let mut reads = HashMap::new();
        let tickets: Vec<Ticket<String>> = rows
            .iter()
            .filter(|row| {
                Some(row.id()) != taken.as_ref() && row.state.is_some() && !busy.contains(row.id())
            })
            .map(|row| self.core_ticket(memory, row, &refs, exclude, &mut reads))
            .collect();
        // The slot's holders, named in the reasons (OWL-291): every ref read
        // that holds it, every ref that could not be read, which stops every
        // dispatch below, and every dispatch whose child runs.
        let slot_held_by: Vec<String> = refs
            .iter()
            .filter(|(_, stored)| {
                let state = TicketState::try_from(&stored.record.state).ok();
                schedule::holds_slot_or_unknown(state.as_ref())
            })
            .map(|(ticket, _)| ticket)
            .chain(&unread)
            .chain(&dispatching)
            .map(|ticket| ticket.as_str().to_owned())
            .collect();
        // The pull requests still followed, from the refs read this pass
        // (an unread ref stops every dispatch below).
        let cap = u32::from(on_demand.config.policy.max_open_prs());
        let open_prs = u32::try_from(
            refs.values()
                .filter(|stored| delivery_followed(stored))
                .count(),
        )
        .unwrap_or(u32::MAX);
        // The machine's run slots every project's drain shares (OWL-409):
        // those held now, by a probe, against `concurrent_runs`, this
        // watch's own children counted while one has not taken its slot yet
        // (OWL-401); a dispatch that would find none waits. Without slots,
        // or with a count that cannot be read, the child's own take decides;
        // in watch's own process, its steps run one after another.
        let (concurrent_runs, runs) =
            memory
                .children
                .and_then(|c| c.slots())
                .map_or((u32::MAX, 0), |slots| {
                    let own = u32::try_from(memory.running.len()).unwrap_or(u32::MAX);
                    (slots.count(), slots.held().unwrap_or(0).max(own))
                });
        // The work in flight outside Owlshift (OWL-410), read when a ticket
        // could be dispatched; a failed read dispatches nothing. A dispatch
        // weighs none of it since OWL-448: it is handed to the step, for a
        // Build's admission that the dispatch reaches.
        let outside_unread = !tickets.is_empty() && {
            let ours = refs
                .iter()
                .map(|(ticket, stored)| (ticket, Some(stored)))
                .chain(unread.iter().map(|ticket| (ticket, None)));
            self.outside(memory, sink, out, ours).is_err()
        };
        let ready = schedule::ready_set(
            &tickets,
            Caps {
                concurrent_runs,
                open_prs: Some(cap),
                max_in_flight: u32::from(on_demand.config.policy.max_in_flight()),
            },
            Load {
                runs,
                open_prs,
                slot_held_by,
            },
        );
        // A pass with an unread ref counts short: it neither re-arms nor
        // tells the cap.
        if !unread.is_empty() {
        } else if open_prs < cap {
            memory.drain.capped = false;
        } else if let Some((held, _)) = ready.waiting.iter().find(|(_, wait)| *wait == Wait::PrCap)
            && let Ok(held) = TicketId::new(held)
        {
            self.tell_cap(memory, &held, open_prs, cap, sink, out);
        }
        for (id, wait) in &ready.waiting {
            let Some(row) = rows.iter().find(|row| row.id().as_str() == id) else {
                continue;
            };
            let detail = wait_data(wait, &memory.outside.kinds);
            let outcome = match (wait, &intake) {
                (Wait::NotAdmitted | Wait::Excluded, _) => Outcome::NotAdmitted {
                    wait: ready::wait(row.id(), wait.key(), detail.clone()),
                },
                (Wait::PrCap, _) => waits(
                    row.id(),
                    wait.key(),
                    data([("open_prs", json!(open_prs)), ("max_open_prs", json!(cap))]),
                ),
                // Its intake is due, and every intake waits for its
                // harness's usage limit (OWL-404).
                (Wait::NoIntake | Wait::StaleIntake, Intake::UsageLimit { harness, until_ms })
                    if row.intake_due(exclude) =>
                {
                    waits(row.id(), "usage_limit", usage_data(*harness, *until_ms))
                }
                _ => waits(row.id(), wait.key(), detail.clone()),
            };
            made.insert(row.id().clone(), outcome);
            if *wait == Wait::PrCap {
                continue;
            }
            let told = format!("{}{:?}{:?}", wait.key(), wait.tickets(), wait.resource());
            if memory.drain.waits.get(row.id()) == Some(&told) {
                continue;
            }
            // A pull request or a person's ticket is named as such
            // (OWL-410).
            let mut event = data([("drain", json!("waits")), ("reason", json!(wait.key()))]);
            event.extend(detail);
            sink.emit(row.id(), None, EventKind::Decision, event);
            let reason = in_flight::said(wait, exclude, &memory.outside.kinds);
            self.line(out, &format!("{} waits: {reason}", row.id()));
            memory.drain.waits.insert(row.id().clone(), told);
        }
        // The pass's dispatches, in the ready set's order (OWL-401). Each is
        // kept as not started until the pass tells otherwise.
        let picked: Vec<&Row> = ready
            .dispatch
            .iter()
            .filter_map(|id| rows.iter().find(|row| row.id().as_str() == id))
            .collect();
        for row in &picked {
            made.insert(row.id().clone(), waits(row.id(), "next_pass", Data::new()));
        }
        for (turn, row) in picked.iter().enumerate() {
            if outside_unread {
                self.wait_for_outside(memory, sink, out, row.id());
                made.insert(
                    row.id().clone(),
                    waits(row.id(), "in_flight_unread", Data::new()),
                );
                continue;
            }
            // Told again if it waits again for a reason it had before.
            memory
                .drain
                .waits
                .insert(row.id().clone(), "dispatch".to_owned());
            if !memory.drain.unread.is_empty() {
                for row in &picked[turn..] {
                    made.insert(
                        row.id().clone(),
                        waits(row.id(), "refs_unread", Data::new()),
                    );
                }
                break;
            }
            let Some(stored) = &row.stored else {
                continue;
            };
            if quarantined {
                self.tell(memory, sink, out, row.id(), Held::Ready);
                made.insert(row.id().clone(), waits(row.id(), "quarantine", Data::new()));
                continue;
            }
            let outside = memory.outside.current();
            let dispatched = self.dispatch(
                memory,
                row.id(),
                stored.at(),
                outside.as_ref(),
                sink,
                out,
                continued,
            );
            let outcome = match dispatched {
                Dispatch::Started => Outcome::Dispatched,
                Dispatch::Held => waits(row.id(), "held", Data::new()),
                Dispatch::UsageLimit { harness, until_ms } => {
                    waits(row.id(), "usage_limit", usage_data(harness, until_ms))
                }
                Dispatch::NotStarted => waits(row.id(), "next_pass", Data::new()),
            };
            made.insert(row.id().clone(), outcome);
            // Each dispatch gives the tickets held for their base a turn,
            // taken at the next pass before any other dispatch (OWL-431):
            // a fix that landed is checked within one dispatch.
            if matches!(dispatched, Dispatch::Started) {
                let held: Vec<TicketId> = memory
                    .base_held
                    .iter()
                    .filter(|held| *held != row.id())
                    .cloned()
                    .collect();
                if !held.is_empty() {
                    memory.base_turn.extend(held);
                    break;
                }
            }
        }
        let weighed: HashMap<&str, &Ticket<String>> = tickets
            .iter()
            .map(|ticket| (ticket.id.as_str(), ticket))
            .collect();
        self.keep_ready(memory, out, rows, unread_rows, &weighed, &refs, made);
    }

    /// Writes the ready snapshot of a pass that read the column (OWL-468):
    /// `rows` and `unread_rows` in the core's order, each with what the pass
    /// made of it (`made`) and its blockers: those the core weighed
    /// (`weighed`) when its intake holds its current text, the only ones
    /// whose "Blocked by" lines are read, its relations otherwise. A write
    /// that fails is one line, until it fails otherwise.
    #[allow(clippy::too_many_arguments)]
    fn keep_ready(
        &self,
        memory: &mut WatchMemory,
        out: &mut dyn Write,
        rows: Vec<Row>,
        unread_rows: Vec<Row>,
        weighed: &HashMap<&str, &Ticket<String>>,
        refs: &HashMap<TicketId, Stored>,
        mut made: HashMap<TicketId, Outcome>,
    ) {
        let mut all: Vec<Row> = rows.into_iter().chain(unread_rows).collect();
        all.sort_by(Row::order);
        let tickets = all
            .iter()
            .map(|row| {
                let blockers = weighed
                    .get(row.id().as_str())
                    .filter(|_| row.intake_hash() == Some(row.text.as_str()))
                    .map_or_else(|| relations(row, refs), |ticket| ticket.blocked_by.clone());
                ReadyTicket {
                    ticket: row.id().clone(),
                    title: row.listed.ticket.title.clone(),
                    priority: row.listed.ticket.priority,
                    blockers: blockers
                        .into_iter()
                        .filter(|blocker| blocker.id != row.id().as_str())
                        .map(ready_blocker)
                        .collect(),
                    drain: made
                        .remove(row.id())
                        .unwrap_or_else(|| waits(row.id(), "next_pass", Data::new())),
                }
            })
            .collect();
        let snapshot = ReadySnapshot {
            format: Format,
            at: (self.on_demand.clock)(),
            interval_ms: u64::try_from(self.interval.as_millis()).unwrap_or(u64::MAX),
            tickets,
        };
        match ready::write(self.on_demand.dirs, &snapshot) {
            Ok(()) => memory.drain.unwritten = None,
            Err(error) => {
                if memory.drain.unwritten.as_ref() != Some(&error) {
                    self.line(
                        out,
                        &format!("the ready snapshot could not be written: {error}"),
                    );
                    memory.drain.unwritten = Some(error);
                }
            }
        }
    }

    /// Tells that `ticket`, which the ready set would dispatch, waits for
    /// the work in flight outside Owlshift to read (OWL-410): one `decision`
    /// event (`drain` = `waits`, `reason` = `in_flight_unread`) and one
    /// line, unless told last.
    fn wait_for_outside(
        &self,
        memory: &mut WatchMemory,
        sink: &mut EventSink<'_>,
        out: &mut dyn Write,
        ticket: &TicketId,
    ) {
        let told = "in_flight_unread".to_owned();
        if memory.drain.waits.get(ticket) == Some(&told) {
            return;
        }
        sink.emit(
            ticket,
            None,
            EventKind::Decision,
            data([("drain", json!("waits")), ("reason", json!(told))]),
        );
        self.line(
            out,
            &format!(
                "{ticket} waits: the work in flight outside Owlshift could not be read, and it \
                 is dispatched once it reads"
            ),
        );
        memory.drain.waits.insert(ticket.clone(), told);
    }

    /// The pass's intake: the first candidate, in the core's order, that its
    /// gesture admits and no hold keeps back, none whose step runs. One
    /// intake runs at a time (OWL-401): none starts while one's child runs.
    /// Returns the ticket it went to, if any, or the usage limit that holds
    /// every intake.
    fn take_in(
        &self,
        memory: &mut WatchMemory,
        rows: &[Row],
        busy: &[TicketId],
        sink: &mut EventSink<'_>,
        out: &mut dyn Write,
        continued: &mut Continued<'_>,
    ) -> Intake {
        let on_demand = self.on_demand;
        if memory.running.iter().any(|running| running.intake()) {
            return Intake::None;
        }
        let exclude = on_demand.config.tracker.exclude.as_deref();
        for row in rows
            .iter()
            .filter(|row| row.intake_due(exclude) && !busy.contains(row.id()))
        {
            let ticket = row.id();
            let now = (on_demand.clock)();
            if memory
                .drain
                .warned
                .get(ticket)
                .is_some_and(|(_, retry)| now < *retry)
            {
                continue;
            }
            let at = row.stored.as_ref().map_or_else(RefAt::none, Stored::at);
            let seen = Seen::at(at, None);
            if memory.holds.admit(ticket, &seen, now) != Admit::Go {
                continue;
            }
            // Intake's harness held by a usage limit holds every intake
            // (OWL-404).
            if let Some((harness, until_ms)) =
                self.usage_wait(memory, sink, out, Some(Role::Intake))
            {
                return Intake::UsageLimit { harness, until_ms };
            }
            let envelope = self.envelope(memory, ticket, StepKind::Intake, &seen);
            let then = Then::Intake { seen };
            match self.step(memory, sink, out, continued, envelope, None, None, then) {
                // The ref moved, or the ticket left the column: the next
                // pass reads it again.
                Some(StepEnd::Stale) => return Intake::None,
                // Told: the next candidate is tried.
                Some(StepEnd::NotAdmitted { .. }) => {}
                end => {
                    return Intake::Took {
                        ticket: ticket.clone(),
                        started: end.as_ref().is_none_or(super::running::ran),
                    };
                }
            }
        }
        Intake::None
    }

    /// What an intake that ended `end` left: a gesture that does not admit
    /// is told; any other end is held as a continue is.
    pub(super) fn taken_in(
        &self,
        memory: &mut WatchMemory,
        ticket: &TicketId,
        seen: Seen,
        end: &StepEnd,
        sink: &mut EventSink<'_>,
        out: &mut dyn Write,
    ) {
        match end {
            StepEnd::Stale => {}
            StepEnd::NotAdmitted { account, reason } => {
                self.not_admitted(memory, ticket, account.clone(), reason, sink, out);
            }
            end => {
                memory.drain.warned.remove(ticket);
                self.ran(memory, ticket, seen, end, sink, out);
            }
        }
    }

    /// The dispatch of `ticket`, its ref at `at`, held as a continue is, its
    /// admission weighing `outside`, the work outside Owlshift this pass
    /// read (OWL-401). Says whether its step started or ran, and what kept
    /// it back otherwise: the hold is checked before the usage limit, which
    /// is read only for a ticket the hold lets go.
    #[allow(clippy::too_many_arguments)]
    fn dispatch(
        &self,
        memory: &mut WatchMemory,
        ticket: &TicketId,
        at: RefAt,
        outside: Option<&Outside>,
        sink: &mut EventSink<'_>,
        out: &mut dyn Write,
        continued: &mut Continued<'_>,
    ) -> Dispatch {
        let on_demand = self.on_demand;
        let seen = Seen::at(at, None);
        if memory.holds.admit(ticket, &seen, (on_demand.clock)()) != Admit::Go {
            return Dispatch::Held;
        }
        // A dispatch's first run, Design or Build, an authoring role's on
        // Claude Code, waits for its usage limit (OWL-404).
        if let Some((harness, until_ms)) = self.usage_wait(memory, sink, out, Some(Role::Build)) {
            return Dispatch::UsageLimit { harness, until_ms };
        }
        let envelope = self.envelope(memory, ticket, StepKind::Dispatch, &seen);
        let then = Then::Dispatch { seen };
        match self.step(memory, sink, out, continued, envelope, None, outside, then) {
            Some(end) if !super::running::ran(&end) => Dispatch::NotStarted,
            _ => Dispatch::Started,
        }
    }

    /// What a dispatch that ended `end` left: the open pull requests' cap
    /// its admission found, told; a gesture that does not admit, told; any
    /// other end held as a continue is. One its admission held back for the
    /// project's capacity or a collision ended stale: the next pass reads
    /// the refs again and tells why it waits.
    pub(super) fn dispatched(
        &self,
        memory: &mut WatchMemory,
        ticket: &TicketId,
        seen: Seen,
        end: &StepEnd,
        sink: &mut EventSink<'_>,
        out: &mut dyn Write,
    ) {
        match end {
            StepEnd::Stale => {}
            // A pull request delivered since the snapshot (OWL-313): nothing
            // ran, so nothing is held; the next pass counts again.
            StepEnd::Capped {
                open_prs,
                max_open_prs,
            } => self.tell_cap(memory, ticket, *open_prs, *max_open_prs, sink, out),
            StepEnd::NotAdmitted { account, reason } => {
                self.not_admitted(memory, ticket, account.clone(), reason, sink, out);
            }
            end => {
                memory.drain.warned.remove(ticket);
                self.ran(memory, ticket, seen, end, sink, out);
            }
        }
    }

    /// The cap reached, `open_prs` of `cap`, holding `held` back: one
    /// `decision` event (`drain` = `pr_cap`) carried by `held` and one line,
    /// unless already told since the count last fell below the cap. `held`
    /// is the pass's first ticket the cap holds, or the dispatch the refs
    /// read under its lock held (OWL-313).
    fn tell_cap(
        &self,
        memory: &mut WatchMemory,
        held: &TicketId,
        open_prs: u32,
        cap: u32,
        sink: &mut EventSink<'_>,
        out: &mut dyn Write,
    ) {
        if memory.drain.capped {
            return;
        }
        sink.emit(
            held,
            None,
            EventKind::Decision,
            data([
                ("drain", json!("pr_cap")),
                ("open_prs", json!(open_prs)),
                ("max_open_prs", json!(cap)),
            ]),
        );
        self.line(
            out,
            &format!(
                "the open pull requests reached their cap ({open_prs} of {cap}): no new Build \
                 starts until one is merged or closed; intake goes on"
            ),
        );
        memory.drain.capped = true;
    }

    /// An intake's or a dispatch's end, its outcome already reported as a
    /// continue's ([`Watch::step`](super::Watch)), held as one: a ref left
    /// as it was, an absent one included, holds the ticket for
    /// [`RETRY_AFTER`].
    fn ran(
        &self,
        memory: &mut WatchMemory,
        ticket: &TicketId,
        seen: Seen,
        outcome: &StepEnd,
        sink: &mut EventSink<'_>,
        out: &mut dyn Write,
    ) {
        let on_demand = self.on_demand;
        // Another command took the project or the ticket, or every run slot
        // of the machine was taken (OWL-409): the next pass tries again. A
        // checked stop (OWL-161) has the watch run no next pass.
        if matches!(outcome, StepEnd::Busy | StepEnd::NoSlot | StepEnd::Stopped) {
            return;
        }
        let git = &on_demand.executor.git;
        let after = ticket_ref::read(git, &on_demand.dirs.checkout(), ticket).ok();
        // An absent ref is as one its first run made for its cost alone.
        let progress = after.as_ref().map(|stored| {
            stored
                .as_ref()
                .map_or_else(|| RefAt::none().progress, Stored::progress)
        });
        let holds_now = after.flatten().as_ref().is_some_and(super::holds);
        // A dispatch is its ticket's first stage step (OWL-367): it takes
        // the slot's turn, and its next step is due at once.
        self.after(
            memory,
            sink,
            out,
            ticket,
            seen,
            outcome,
            progress.as_ref(),
            holds_now,
        );
    }

    /// A gesture by `account` that does not admit `ticket`: one `warning`
    /// event and one line per ticket and account; the ticket is tried again
    /// after [`RETRY_AFTER`], so an account added to `admitters` since
    /// admits it.
    fn not_admitted(
        &self,
        memory: &mut WatchMemory,
        ticket: &TicketId,
        account: String,
        reason: &str,
        sink: &mut EventSink<'_>,
        out: &mut dyn Write,
    ) {
        let now = (self.on_demand.clock)();
        let retry = now.checked_add(RETRY_AFTER).unwrap_or(Timestamp::MAX);
        let told = memory
            .drain
            .warned
            .get(ticket)
            .is_some_and(|(warned, _)| *warned == account);
        if !told {
            sink.emit(
                ticket,
                None,
                EventKind::Warning,
                data([
                    ("what", json!("not_admitted")),
                    ("account", json!(account)),
                    ("reason", json!(reason)),
                ]),
            );
            self.line(out, &format!("{ticket} is not taken in: {reason}"));
        }
        memory.drain.warned.insert(ticket.clone(), (account, retry));
    }

    /// The core's view of a row: its admission, decider, text and intake,
    /// state and blockers. The blockers are read only for a ticket whose
    /// intake record holds its current text, the only one the core reads
    /// them of; each "Blocked by" identifier not among its relations is read
    /// on the tracker, once a pass (`reads`).
    fn core_ticket(
        &self,
        memory: &WatchMemory,
        row: &Row,
        refs: &HashMap<TicketId, Stored>,
        exclude: Option<&str>,
        reads: &mut HashMap<String, BlockerState>,
    ) -> Ticket<String> {
        let record = row
            .stored
            .as_ref()
            .and_then(|stored| stored.record.intake.as_ref());
        // A gesture found not admitting, at intake or at a dispatch (its
        // admitter dropped from the list), keeps the ticket out of the pick
        // until it is read again, so the next ticket goes meanwhile.
        let now = (self.on_demand.clock)();
        let warned = memory
            .drain
            .warned
            .get(row.id())
            .is_some_and(|(_, retry)| now < *retry);
        let admission = if row.excluded(exclude) {
            Admission::Excluded
        } else if warned {
            Admission::NotAdmitted
        } else {
            Admission::Admitted
        };
        let blocked_by = if row.intake_hash() == Some(row.text.as_str()) {
            self.blockers(row, refs, reads)
        } else {
            Vec::new()
        };
        Ticket {
            id: row.id().as_str().to_owned(),
            priority: row.listed.ticket.priority,
            created_ms: row.listed.created_at.map_or(0, Timestamp::as_millisecond),
            admission,
            // The zones of an intake record that holds the ticket's text
            // join its labels' (OWL-412). A command counts a record kept for
            // an earlier text too, but the drain takes in a ticket whose
            // text changed again, so such a record is about to be replaced
            // and its ticket waits for that intake, whose own check counts no
            // record's zones.
            has_decider: on_demand::has_decider(
                &row.listed.ticket,
                record
                    .filter(|record| record.text_sha256 == row.text)
                    .map_or(&[], |record| &record.zones),
            ),
            text_hash: row.text.clone(),
            intake_hash: row.intake_hash().map(ToOwned::to_owned),
            blocked_by,
            variant: record.map_or(Variant::Standard, |record| record.variant),
            state: row.state.unwrap_or_else(TicketState::admitted),
            delivery_followed: row.followed(),
        }
    }

    /// A row's blockers: its relations, then the identifiers of its "Blocked
    /// by" lines that are not among them, each with what the tracker shows
    /// and whether its ticket ref records its pull request merged.
    fn blockers(
        &self,
        row: &Row,
        refs: &HashMap<TicketId, Stored>,
        reads: &mut HashMap<String, BlockerState>,
    ) -> Vec<Blocker<String>> {
        let mut blockers = relations(row, refs);
        for id in text_blockers(&row.listed.ticket.description) {
            if blockers.iter().any(|blocker| blocker.id == id) {
                continue;
            }
            // The ticket's own identifier is passed as is: the core
            // ignores it, and nothing is read for it.
            let tracker = if id == row.id().as_str() {
                BlockerState::Open
            } else {
                *reads
                    .entry(id.to_owned())
                    .or_insert_with(|| match TicketId::new(id) {
                        Ok(ticket) => text_state(self.on_demand.tracker.state_type(&ticket)),
                        Err(_) => BlockerState::Unknown,
                    })
            };
            blockers.push(Blocker {
                id: id.to_owned(),
                tracker,
                merged: merged_in(refs, id),
            });
        }
        blockers
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(admit: &str) -> ProjectConfig {
        ProjectConfig::parse(&format!(
            "requires = \">=0.0\"\n[tracker]\nkind = \"markdown\"\n{admit}\n\
             states = {{ ready = \"Todo\", working = \"In Progress\", needs_input = \"Needs \
             Input\", review = \"In Review\" }}\n[stack]\ngate = [\"true\"]\n[pipeline]\n\
             default = \"trivial\"\nplan_approval = \"never\"\n[models]\n[policy]\nalways_human = []\n"
        ))
        .unwrap()
    }

    #[test]
    fn the_drain_refuses_delegation_and_a_project_without_admitters() {
        let delegation = refusal(&project("admit = \"delegation\"")).unwrap();
        assert!(delegation.contains("not built"), "{delegation}");
        let unlisted = refusal(&project("admit = { label = \"agent\" }")).unwrap();
        assert!(unlisted.contains("`[tracker].admitters`"), "{unlisted}");
        let listed = project("admit = { label = \"agent\" }\nadmitters = [\"maintainer\"]");
        assert_eq!(refusal(&listed), None);
    }

    #[test]
    fn only_a_definite_not_found_makes_a_blocker_unknown() {
        let error = |kind| Error::new(kind, "x");
        assert_eq!(
            text_state(Err(error(ErrorKind::NotFound))),
            BlockerState::Unknown
        );
        for kind in [ErrorKind::Other, ErrorKind::Unauthorized] {
            assert_eq!(text_state(Err(error(kind))), BlockerState::Open);
        }
        assert_eq!(text_state(Ok(None)), BlockerState::Open);
        assert_eq!(
            text_state(Ok(Some(StateType::Completed))),
            BlockerState::Completed
        );
        for state in [StateType::Canceled, StateType::Duplicate] {
            assert_eq!(relation_state(Some(state)), BlockerState::Canceled);
        }
        for state in [StateType::Started, StateType::Triage, StateType::Unstarted] {
            assert_eq!(relation_state(Some(state)), BlockerState::Open);
        }
    }
}
