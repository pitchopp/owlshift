//! Watch and a restart typed on the ticket (OWL-488; architecture section
//! 5, "A restart typed on the ticket"): at each pass, the thread of each
//! parked ticket the pass lists is read, and the newest `continue` comment
//! that may restart it ([`typed::restart_comment`], anyone's: watch reads
//! no project file to resolve a zone's owner) starts a step,
//! [`StepKind::Restart`], which decides with the ticket's decider under its
//! lock.
//!
//! A park restarted to Intake runs intake in that step, so it waits as an
//! intake does: for the harness's usage limit, and while another intake
//! runs. A review loop's park at Verify may build again, so it waits for
//! the slot's turn as an answer check weighs it, a collision and the build
//! harness's limit. Any other park is handed to watch at once, nothing
//! running in the step.
//!
//! While the project is quarantined no step starts: the decider's
//! `continue` is answered by one REPLY that says so, the decider being the
//! ticket's assignee, the one watch knows without a project file.

use std::io::Write;

use owlshift_contracts::Role;
use owlshift_contracts::ids::TicketId;
use owlshift_contracts::step::{Settled, StepEnd, StepKind};
use owlshift_core::schedule::{self, Turn};
use owlshift_core::state::{Event, Status, TicketState, Transition};
use owlshift_core::vocab::Stage;

use super::running::Then;
use super::{Continued, Memory, Seen, Watch};
use crate::events::EventSink;
use crate::held;
use crate::on_demand::{self, OnDemand, PIPELINE, typed};
use crate::ticket_ref::Stored;

/// Whether the ticket whose ref keeps `stored` is in a review loop: its
/// reviews answered by a Build, its pull request followed (OWL-307).
fn in_review_loop(stored: &Stored) -> bool {
    stored
        .record
        .review
        .as_ref()
        .is_some_and(|review| !review.answering.is_empty())
        && stored
            .record
            .delivery
            .as_ref()
            .is_some_and(|delivery| delivery.followed())
}

impl Watch<'_> {
    /// One parked ticket (OWL-488): its thread, and the step of the
    /// `continue` comment that may restart it, once nothing holds that step
    /// back.
    pub(super) fn restart_look(
        &self,
        memory: &mut Memory,
        sink: &mut EventSink<'_>,
        out: &mut dyn Write,
        continued: &mut Continued<'_>,
        ticket: &TicketId,
        stored: &Stored,
    ) {
        let on_demand = self.on_demand;
        // Under an operator's pause nothing restarts (OWL-438).
        if self.operator_paused(memory, sink, out) {
            return;
        }
        let Ok(state) = TicketState::try_from(&stored.record.state) else {
            return;
        };
        let thread = match on_demand.tracker.comments(ticket) {
            Ok(thread) => thread,
            Err(e) => {
                let reason = format!("reading its comments: {e}");
                return self.unreadable(memory, sink, ticket, reason);
            }
        };
        memory.failing.remove(ticket);
        let settled = typed::settled(on_demand.dirs, ticket);
        let Some(comment) = typed::restart_comment(&thread, None, settled.as_ref()) else {
            return;
        };
        let intake = match state.apply(PIPELINE, Event::Restarted) {
            Ok(Transition::To(next)) => next.status() == Status::Active(Stage::Intake),
            _ => false,
        };
        if intake {
            // One intake at a time, as the drain's (OWL-250).
            if memory.running.iter().any(|running| running.intake())
                || self.usage_waits(memory, sink, out, Some(Role::Intake))
            {
                return;
            }
        } else if state.stage() == Stage::Verify
            && in_review_loop(stored)
            && !self.restart_admitted(memory, sink, out, ticket, stored)
        {
            return;
        }
        let seen = Seen::at(stored.at(), None);
        let kind = StepKind::Restart {
            comment: comment.id.clone(),
        };
        let envelope = self.envelope(memory, ticket, kind, &seen);
        let then = Then::Restart { seen, intake };
        self.step(memory, sink, out, continued, envelope, None, None, then);
    }

    /// Whether the restart of a review loop's park at Verify, which may
    /// build the loop again, may start: its turn of the project's slot, as
    /// an answer check weighs it (OWL-367, OWL-401), no collision holding
    /// it back (OWL-403), and the build harness free (OWL-404). It waits
    /// untold: its ticket still shows parked.
    fn restart_admitted(
        &self,
        memory: &mut Memory,
        sink: &mut EventSink<'_>,
        out: &mut dyn Write,
        ticket: &TicketId,
        stored: &Stored,
    ) -> bool {
        let (holding, unread) = &memory.holding;
        let mut with = holding.clone();
        with.push((ticket.clone(), held::dispatched(stored)));
        let turn = matches!(
            schedule::slot_turn(&with, unread, self.capacity()),
            Turn::Holders(holders) if holders.contains(ticket)
        );
        turn && !memory.collisions.contains_key(ticket)
            && !memory.outside.unread_steps.contains(ticket)
            && !self.usage_waits(memory, sink, out, Some(Role::Build))
    }

    /// What a restart's step that ended `end` left (OWL-488): the slot's
    /// turn as any step's, and the next pass at once once the ref moved, so
    /// that watch takes the restarted ticket. A ref left as it was keeps no
    /// hold: its ticket is still parked, and the thread, with `restart.json`,
    /// says whether its comment still counts.
    pub(super) fn restarted(
        &self,
        memory: &mut Memory,
        sink: &mut EventSink<'_>,
        out: &mut dyn Write,
        ticket: &TicketId,
        seen: Seen,
        end: &StepEnd,
    ) {
        if *end == StepEnd::Stale {
            return;
        }
        let after = self.read_after(ticket);
        let progress = after.as_ref().map(Stored::progress);
        let holds_now = after.as_ref().is_some_and(super::holds);
        let moved = progress.as_ref() != Some(&seen.progress);
        self.after(
            memory,
            sink,
            out,
            ticket,
            seen,
            end,
            progress.as_ref(),
            holds_now,
        );
        if moved {
            memory.stepped = true;
        } else {
            memory.holds.release(ticket);
        }
    }

    /// One parked ticket while the project is quarantined (OWL-488): the
    /// decider's `continue`, the decider being its assignee, is answered by
    /// one REPLY that says so. A ticket without an assignee is left alone:
    /// its decider is a zone's owner, which watch cannot read, and its
    /// comment restarts it once the quarantine is cleared.
    pub(super) fn restart_quarantined(
        &self,
        memory: &mut Memory,
        sink: &mut EventSink<'_>,
        ticket: &TicketId,
    ) {
        let on_demand: &OnDemand<'_> = self.on_demand;
        let thread = match on_demand.tracker.comments(ticket) {
            Ok(thread) => thread,
            Err(e) => {
                let reason = format!("reading its comments: {e}");
                return self.unreadable(memory, sink, ticket, reason);
            }
        };
        let settled = typed::settled(on_demand.dirs, ticket);
        let Some(newest) = typed::restart_comment(&thread, None, settled.as_ref()) else {
            return;
        };
        let found = match on_demand.tracker.ticket(ticket) {
            Ok(found) => found,
            Err(e) => {
                let reason = format!("reading the ticket: {e}");
                return self.unreadable(memory, sink, ticket, reason);
            }
        };
        memory.failing.remove(ticket);
        let Some(decider) = on_demand::known_decider(&found) else {
            return;
        };
        match typed::restart_comment(&thread, Some(&decider), settled.as_ref()) {
            Some(comment) => {
                on_demand.reply_quarantined(ticket, &decider.id, comment, sink);
            }
            None => typed::settle(
                on_demand.dirs,
                ticket,
                &newest.id,
                newest.created_at,
                Settled::NotCounted,
                sink,
            ),
        }
    }
}
