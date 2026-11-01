//! A canceled ticket parked (OWL-474, architecture section 5, "The local
//! web UI", "Cancel"): watch, at the pass after the cancel file stood,
//! parks the ticket through the park every reason takes
//! ([`OnDemand::cancel_seen`]); a person's typed `continue` withdraws a
//! cancel still standing ([`OnDemand::withdraw_cancel`]).
//!
//! The park goes through `continue`'s own preparation, the ticket's lock
//! and the fetch and reads that follow it: the park writes the ticket's ref
//! with everything that preparation loads (its intake, plan, review, spend
//! and pipeline), and a machine that cannot prepare a step could not post
//! the PARKED comment either. The file then waits for a later pass.

use owlshift_contracts::service::Request;

use super::*;
use crate::cancel::{self, NotParked};

/// What a cancel came to at a pass of watch (OWL-474).
#[derive(Clone, Debug, PartialEq)]
pub enum Canceled {
    /// The ticket parked: the park's stop, [`Stop::Parked`], or
    /// [`Stop::NotKept`] when its state could not be kept.
    Parked(Stop),
    /// Nothing was parked, for this reason.
    NotParked(NotParked),
}

impl OnDemand<'_> {
    /// Parks `ticket`, canceled by `request` (`None` for a cancel file that
    /// does not read), for `owlshift watch` (OWL-474): only while its ticket
    /// ref, read under the project's lock with the ticket's own lock held,
    /// is still `seen`, the commit watch read it at. `None`, with nothing
    /// written, when the project or the ticket is busy, or the ref moved or
    /// is gone: the next pass reads it again. A ticket already parked, at
    /// Ready, or whose delivery ended is not parked ([`cancel::parkable`]).
    /// Otherwise the core parks it ([`Event::Canceled`]), and the park keeps
    /// its ref, posts the PARKED comment saying who canceled, when and why
    /// ([`cancel::detail`]), moves the stage to parked and records the
    /// decision. No one is notified: the operator asked for it.
    pub fn cancel_seen(
        &self,
        ticket: &TicketId,
        seen: &str,
        request: Option<&Request>,
        sink: &mut EventSink<'_>,
    ) -> Option<Result<Canceled, Stop>> {
        let mut p = match self.prepare(ticket, Command::Continue) {
            Ok(p) => p,
            Err(stop) if stop.is_busy() => return None,
            Err(stop) => return Some(Err(stop)),
        };
        let stored = p.stored.as_ref()?;
        if stored.commit != seen {
            return None;
        }
        let state = match TicketState::try_from(&stored.record.state) {
            Ok(state) => state,
            Err(error) => return Some(Err(core_error(error))),
        };
        if let Err(not) = cancel::parkable(&state, stored) {
            return Some(Ok(Canceled::NotParked(not)));
        }
        let parked = match state.apply(p.pipeline, Event::Canceled) {
            Ok(Transition::Parked { state, .. }) => state,
            other => return Some(Err(core_error(other))),
        };
        let stop = self.park_with(
            &mut p,
            &parked,
            ParkReason::Stopped,
            None,
            cancel::detail(request),
            None,
            Vec::new(),
            sink,
        );
        Some(Ok(Canceled::Parked(stop)))
    }

    /// Withdraws the cancel standing on `ticket`, if any, for a person's
    /// typed `continue` (OWL-474): their later gesture wins. A `decision`
    /// event (`cancel` `withdrawn`, with who asked it, when and why) says
    /// so; a file that could not be removed is a `warning`. A step's child
    /// never comes here: it goes through [`Self::continue_seen`] and
    /// [`Self::continue_shown`], so it never removes the file that stops it.
    pub(super) fn withdraw_cancel(&self, ticket: &TicketId, sink: &mut EventSink<'_>) {
        let Some(request) = cancel::read(self.dirs, ticket) else {
            return;
        };
        if let Err(error) = cancel::remove_at(&cancel::file(self.dirs, ticket)) {
            sink.emit(
                ticket,
                None,
                EventKind::Warning,
                data([
                    ("what", json!("cancel_not_withdrawn")),
                    ("reason", json!(error.to_string())),
                ]),
            );
            return;
        }
        let mut event = data([("cancel", json!("withdrawn"))]);
        if let Some(request) = request {
            event.insert("by".to_owned(), json!(request.by));
            event.insert("at".to_owned(), json!(request.at.to_string()));
            event.insert("why".to_owned(), json!(request.why));
        }
        sink.emit(ticket, None, EventKind::Decision, event);
    }
}
