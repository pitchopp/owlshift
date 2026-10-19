//! `owlshift watch --drain` (OWL-250; architecture section 5, "The drain";
//! build plan, "P4 · The drain", part G): once watch's own work of a pass is
//! done, the drain reads the ticket refs again, then the ready column, takes
//! at most one admitted ticket in (intake), and dispatches at most one ready
//! ticket to Build, the core's pick ([`schedule::ready_set`]), through `do`'s
//! path ([`OnDemand::dispatch_seen`](crate::on_demand::OnDemand::dispatch_seen)).
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
//! a continue is, in watch's own [`Holds`](super::Holds), so a usage limit
//! anywhere pauses both.
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

use std::collections::HashMap;
use std::io::Write;

use jiff::Timestamp;
use serde_json::json;

use owlshift_adapters::tracker::{Column, Error, ErrorKind, Listed, StateType};
use owlshift_contracts::Variant;
use owlshift_contracts::config::{Admit as Gesture, ProjectConfig};
use owlshift_contracts::event::EventKind;
use owlshift_contracts::ids::TicketId;
use owlshift_contracts::refs::EndedWhy;
use owlshift_core::schedule::{self, Caps, Load, Wait, natural_cmp};
use owlshift_core::state::{Status, TicketState};
use owlshift_core::ticket::{Admission, Blocker, BlockerState, Ticket, text_blockers};
use owlshift_core::vocab::Stage;

use super::{Admit, Continued, Memory as WatchMemory, RETRY_AFTER, Seen, Watch};
use crate::events::{EventSink, data};
use crate::on_demand::{self, Delivered, DispatchSeen, Stop, Taken, delivery_followed};
use crate::quarantine::Held;
use crate::ticket_ref::{self, Stored, text_sha256};

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

    fn id(&self) -> &TicketId {
        &self.listed.ticket.id
    }

    fn commit(&self) -> Option<&str> {
        self.stored.as_ref().map(|stored| stored.commit.as_str())
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
        due && !self.excluded(exclude) && on_demand::has_decider(&self.listed.ticket)
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

/// What a waiting reason says on watch's line.
fn said(wait: &Wait<String>, exclude: Option<&str>) -> String {
    let list = |ids: &[String]| ids.join(", ");
    match wait {
        Wait::Duplicate => "the column lists it twice; the first is taken".to_owned(),
        Wait::NotAdmitted => {
            "its admission gesture is not from an account `[tracker].admitters` lists".to_owned()
        }
        Wait::Excluded => format!(
            "it carries the exclusion label {:?}",
            exclude.unwrap_or_default()
        ),
        Wait::NoDecider => "it has no decider: give it an assignee".to_owned(),
        Wait::NoIntake => "its intake has not run yet".to_owned(),
        Wait::StaleIntake => "its text changed since its intake, which runs again".to_owned(),
        Wait::NotReady => {
            "it is being worked, waits for an answer, is parked, or its pull request is followed"
                .to_owned()
        }
        Wait::BlockerCanceled(ids) => format!(
            "its blocker {} was canceled: it waits until a person removes the relation or the \
             line",
            list(ids)
        ),
        Wait::BlockerUnknown(ids) => format!(
            "its \"Blocked by\" line names {}, which the tracker does not know: it waits until a \
             person corrects the line",
            list(ids)
        ),
        Wait::BlockedBy(ids) => format!("blocked by {}", list(ids)),
        Wait::PrCap => "the open pull requests reached their cap".to_owned(),
        Wait::Slot(ids) => format!(
            "the project's slot is held by {} (`owlshift doctor` says how to clear a stale one)",
            list(ids)
        ),
        Wait::RunCap => "every run of this machine is taken".to_owned(),
    }
}

/// How an intake's outcome is reported, as a continue's: a record kept is
/// the ticket back in the column, waiting for its dispatch.
fn as_reported(ticket: &TicketId, outcome: Result<impl Sized, Stop>) -> Result<Delivered, Stop> {
    match outcome {
        Ok(_) => Err(Stop::Ready {
            ticket: ticket.clone(),
        }),
        Err(stop) => Err(stop),
    }
}

/// Where the pass's intake left the drain.
enum Intake {
    /// No intake ran, or one ran on this ticket.
    Done(Option<TicketId>),
    /// A usage limit pauses every run: nothing is dispatched either.
    Paused,
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
        // Read again: watch's work may have ended a following or closed a
        // round in this pass. A project busy now waits for the next pass,
        // which watch's own note tells. A quarantined one takes nothing in,
        // and its ticket the ready set would dispatch is told that it waits
        // (OWL-368).
        let Ok(snapshot) = self.snapshot(memory, sink) else {
            return;
        };
        let quarantined = snapshot.quarantine.is_some();
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
        let mut rows: Vec<Row> = listed
            .into_iter()
            .filter(|listed| !unread.contains(&listed.ticket.id))
            .map(|listed| {
                let stored = refs.get(&listed.ticket.id).cloned();
                Row::new(listed, stored)
            })
            .collect();
        rows.sort_by(Row::order);

        // Under quarantine no intake runs, nor is its candidate told: its
        // gesture is checked at the base commit under the lock, which the
        // quarantine refuses, and a ticket at Intake is not ready yet.
        let taken = if quarantined {
            None
        } else {
            match self.take_in(memory, &rows, sink, out, continued) {
                Intake::Done(taken) => taken,
                Intake::Paused => return,
            }
        };
        let exclude = tracker.exclude.as_deref();
        let mut reads = HashMap::new();
        let tickets: Vec<Ticket<String>> = rows
            .iter()
            .filter(|row| Some(row.id()) != taken.as_ref() && row.state.is_some())
            .map(|row| self.core_ticket(memory, row, &refs, exclude, &mut reads))
            .collect();
        // The slot's holders, named in the reasons (OWL-291): every ref read
        // that holds it, and every ref that could not be read, which stops
        // every dispatch below.
        let slot_held_by: Vec<String> = refs
            .iter()
            .filter(|(_, stored)| {
                let state = TicketState::try_from(&stored.record.state).ok();
                schedule::holds_slot_or_unknown(state.as_ref())
            })
            .map(|(ticket, _)| ticket)
            .chain(&unread)
            .map(|ticket| ticket.as_str().to_owned())
            .collect();
        // The pull requests still followed, from the refs read this pass
        // (an unread ref stops every dispatch below). One project, one run
        // at a time.
        let cap = u32::from(on_demand.config.policy.max_open_prs());
        let open_prs = u32::try_from(
            refs.values()
                .filter(|stored| delivery_followed(stored))
                .count(),
        )
        .unwrap_or(u32::MAX);
        let ready = schedule::ready_set(
            &tickets,
            Caps {
                concurrent_runs: 1,
                open_prs: Some(cap),
            },
            Load {
                runs: 0,
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
            if *wait == Wait::PrCap {
                continue;
            }
            let Some(row) = rows.iter().find(|row| row.id().as_str() == id) else {
                continue;
            };
            let told = format!("{}{:?}", wait.key(), wait.tickets());
            if memory.drain.waits.get(row.id()) == Some(&told) {
                continue;
            }
            let mut event = data([("drain", json!("waits")), ("reason", json!(wait.key()))]);
            if !wait.tickets().is_empty() {
                event.insert("tickets".to_owned(), json!(wait.tickets()));
            }
            sink.emit(row.id(), None, EventKind::Decision, event);
            self.line(out, &format!("{} waits: {}", row.id(), said(wait, exclude)));
            memory.drain.waits.insert(row.id().clone(), told);
        }
        let Some(id) = ready.dispatch else {
            return;
        };
        let Some(row) = rows.iter().find(|row| row.id().as_str() == id) else {
            return;
        };
        // Told again if it waits again for a reason it had before.
        memory
            .drain
            .waits
            .insert(row.id().clone(), "dispatch".to_owned());
        if !memory.drain.unread.is_empty() {
            return;
        }
        let Some(commit) = row.commit() else {
            return;
        };
        if quarantined {
            return self.tell(memory, sink, out, row.id(), Held::Ready);
        }
        self.dispatch(memory, row.id(), commit, sink, out, continued);
    }

    /// The pass's intake: the first candidate, in the core's order, that its
    /// gesture admits and no hold keeps back.
    fn take_in(
        &self,
        memory: &mut WatchMemory,
        rows: &[Row],
        sink: &mut EventSink<'_>,
        out: &mut dyn Write,
        continued: &mut Continued<'_>,
    ) -> Intake {
        let on_demand = self.on_demand;
        let exclude = on_demand.config.tracker.exclude.as_deref();
        for row in rows.iter().filter(|row| row.intake_due(exclude)) {
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
            let seen = Seen {
                commit: row.commit().unwrap_or_default().to_owned(),
                answer: None,
            };
            match memory.holds.admit(ticket, &seen, now) {
                Admit::Go => {}
                Admit::Held { .. } => continue,
                Admit::Paused { .. } => return Intake::Paused,
            }
            let Some(taken) = on_demand.intake_seen(ticket, row.commit(), sink) else {
                // The ref moved, or the ticket left the column: the next
                // pass reads it again.
                return Intake::Done(None);
            };
            match taken {
                Taken::NotAdmitted { account, reason } => {
                    self.not_admitted(memory, ticket, account, &reason, sink, out);
                }
                Taken::Ran(outcome) => {
                    memory.drain.warned.remove(ticket);
                    let outcome = as_reported(ticket, outcome);
                    self.ran(memory, ticket, seen, &outcome, sink, continued);
                    return Intake::Done(Some(ticket.clone()));
                }
            }
        }
        Intake::Done(None)
    }

    /// The dispatch of `ticket`, its ref at `commit`, held as a continue is.
    fn dispatch(
        &self,
        memory: &mut WatchMemory,
        ticket: &TicketId,
        commit: &str,
        sink: &mut EventSink<'_>,
        out: &mut dyn Write,
        continued: &mut Continued<'_>,
    ) {
        let on_demand = self.on_demand;
        let seen = Seen {
            commit: commit.to_owned(),
            answer: None,
        };
        if memory.holds.admit(ticket, &seen, (on_demand.clock)()) != Admit::Go {
            return;
        }
        match on_demand.dispatch_seen(ticket, commit, sink) {
            None => {}
            // A pull request delivered since the snapshot (OWL-313): nothing
            // ran, so nothing is held; the next pass counts again.
            Some(DispatchSeen::Capped {
                open_prs,
                max_open_prs,
            }) => self.tell_cap(memory, ticket, open_prs, max_open_prs, sink, out),
            Some(DispatchSeen::Taken(Taken::NotAdmitted { account, reason })) => {
                self.not_admitted(memory, ticket, account, &reason, sink, out);
            }
            Some(DispatchSeen::Taken(Taken::Ran(outcome))) => {
                memory.drain.warned.remove(ticket);
                self.ran(memory, ticket, seen, &outcome, sink, continued);
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

    /// An intake's or a dispatch's outcome reported as a continue's, then
    /// held as one: a ref left as it was, an absent one included, holds the
    /// ticket for [`RETRY_AFTER`].
    fn ran(
        &self,
        memory: &mut WatchMemory,
        ticket: &TicketId,
        seen: Seen,
        outcome: &Result<Delivered, Stop>,
        sink: &mut EventSink<'_>,
        continued: &mut Continued<'_>,
    ) {
        let on_demand = self.on_demand;
        // Another command took the project: the next pass tries again.
        if matches!(outcome, Err(Stop::Busy(_))) {
            return;
        }
        continued(ticket, outcome, sink);
        let git = &on_demand.executor.git;
        let after = ticket_ref::read(git, &on_demand.dirs.checkout(), ticket)
            .map(|stored| stored.map_or_else(String::new, |stored| stored.commit))
            .ok();
        let now = (on_demand.clock)();
        memory
            .holds
            .after(ticket, seen, outcome, after.as_deref(), now);
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
            has_decider: on_demand::has_decider(&row.listed.ticket),
            text_hash: row.text.clone(),
            intake_hash: row.intake_hash().map(ToOwned::to_owned),
            blocked_by,
            variant: record.map_or(Variant::Standard, |record| record.variant),
            resources: Vec::new(),
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
        let merged = |id: &str| {
            TicketId::new(id)
                .ok()
                .and_then(|id| refs.get(&id))
                .and_then(|stored| stored.record.delivery.as_ref())
                .and_then(|delivery| delivery.ended.as_ref())
                .is_some_and(|ended| ended.why == EndedWhy::Merged)
        };
        let mut blockers: Vec<Blocker<String>> = row
            .listed
            .blockers
            .iter()
            .map(|blocker| Blocker {
                id: blocker.id.as_str().to_owned(),
                tracker: relation_state(blocker.state),
                merged: merged(blocker.id.as_str()),
            })
            .collect();
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
                merged: merged(id),
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
