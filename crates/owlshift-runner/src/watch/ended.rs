//! The follow-ups a ticket still owes once its following ended (OWL-501;
//! architecture section 5, "Follow-ups", "Owed, then filed"): an owed
//! follow-up holds no merge on green, so a ticket may be merged while the
//! tracker refuses one, and watch goes on filing it at later passes, by the
//! same filing as a followed ticket's, at most every [`RETRY_AFTER`] per
//! ticket, until the tracker takes each.

use std::collections::HashMap;
use std::time::Duration;

use jiff::Timestamp;

use owlshift_contracts::ids::TicketId;

use super::{Memory, RETRY_AFTER, Watch};
use crate::events::EventSink;
use crate::proposals;
use crate::ticket_ref::Stored;

/// When this watch last tried to file each ended ticket's owed follow-ups:
/// a restart tries at once.
#[derive(Debug, Default)]
pub(super) struct EndedFilings {
    last: HashMap<TicketId, Timestamp>,
}

impl EndedFilings {
    /// Whether `ticket` is due a try at `now`: never tried by this watch, or
    /// last tried at least [`RETRY_AFTER`] before.
    fn due(&self, ticket: &TicketId, now: Timestamp) -> bool {
        self.last.get(ticket).is_none_or(|last| {
            Duration::try_from(now.duration_since(*last)).unwrap_or_default() >= RETRY_AFTER
        })
    }

    /// Forgets the tickets not in `owing`: filed, or gone.
    fn retain(&mut self, owing: &[(TicketId, Stored)]) {
        self.last
            .retain(|ticket, _| owing.iter().any(|(owes, _)| owes == ticket));
    }
}

/// Whether the ticket whose ref keeps `stored` still owes a follow-up after
/// its following ended, whatever ended it.
pub(super) fn owes_after_end(stored: &Stored) -> bool {
    stored
        .record
        .delivery
        .as_ref()
        .is_some_and(|delivery| delivery.pull_request.is_some() && delivery.ended.is_some())
        && proposals::owes(stored.proposals.as_ref())
}

impl Watch<'_> {
    /// Files what each ticket of `owing`, whose following ended, still owes
    /// on the tracker, those due a try ([`EndedFilings`]); a try a lock, a
    /// quarantine or a moved ref stopped does not count.
    pub(super) fn file_ended(
        &self,
        memory: &mut Memory,
        sink: &mut EventSink<'_>,
        owing: &[(TicketId, Stored)],
    ) {
        memory.ended_filings.retain(owing);
        if !proposals::has_inbox(self.on_demand.tracker) {
            return;
        }
        let now = (self.on_demand.clock)();
        for (ticket, stored) in owing {
            if !memory.ended_filings.due(ticket, now) {
                continue;
            }
            if self
                .on_demand
                .file_owed_after_end(ticket, stored, &mut memory.following, sink)
            {
                memory.ended_filings.last.insert(ticket.clone(), now);
            }
        }
    }
}
