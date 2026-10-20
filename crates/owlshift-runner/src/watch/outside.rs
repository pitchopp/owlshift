//! Watch's read of the work in flight that Owlshift does not run (OWL-410;
//! architecture section 5, "The in-flight set", 2 and 3): at most once a
//! pass, and only when a check needs it, the repository's open pull requests
//! and the working state's tickets ([`in_flight::read`]), their files kept
//! from one pass to the next while a pull request does not move.
//!
//! A pass whose read fails dispatches nothing from Ready and starts no stage
//! step that the collision rule would check: its reason is one line, told
//! again when it changes; each ticket held is told once that it waits
//! (`reason` = `in_flight_unread`). A person's ticket counted for want of
//! zones is one `warning` event (`zones_unknown`) and one line, told again
//! only after it left the working state or gained a zone.

use std::collections::{HashMap, HashSet};
use std::io::Write;

use serde_json::json;

use owlshift_contracts::event::EventKind;
use owlshift_contracts::ids::TicketId;

use super::{Memory, StepWait, Watch, holds};
use crate::events::{EventSink, data};
use crate::in_flight::{self, Holders, Kind, Outside, PullCache};
use crate::on_demand::delivery_followed;
use crate::ticket_ref::{RefRead, Stored};

/// What watch keeps of the work outside Owlshift.
#[derive(Debug, Default)]
pub(super) struct OutsideMemory {
    /// This pass's read, once made.
    read: Option<Result<Outside, String>>,
    /// The pull requests' files, from one pass to the next.
    cache: PullCache,
    /// The reason last told that the read failed.
    told: Option<String>,
    /// The people's tickets told that they are not counted.
    unknown: HashSet<TicketId>,
    /// How the outside holders of the last read are named.
    pub(super) kinds: HashMap<String, Kind>,
    /// The tickets whose next stage step this pass's failed read holds.
    pub(super) unread_steps: HashSet<TicketId>,
}

impl OutsideMemory {
    /// Forgets the last pass's read: each pass reads again.
    pub(super) fn new_pass(&mut self) {
        self.read = None;
        self.unread_steps.clear();
    }
}

impl Watch<'_> {
    /// The outside holders against the ticket refs `refs` (`None` for one
    /// that cannot be read), reading the work outside Owlshift first if this
    /// pass has not; the failed read's reason otherwise.
    pub(super) fn outside<'a>(
        &self,
        memory: &mut Memory,
        sink: &mut EventSink<'_>,
        out: &mut dyn Write,
        refs: impl IntoIterator<Item = (&'a TicketId, Option<&'a Stored>)>,
    ) -> Result<Holders, String> {
        let on_demand = self.on_demand;
        let kept = &mut memory.outside;
        let read = kept.read.get_or_insert_with(|| {
            in_flight::read(
                on_demand.forge,
                on_demand.tracker,
                on_demand.config,
                &mut kept.cache,
            )
        });
        let outside = match read {
            Ok(outside) => outside,
            Err(reason) => {
                let reason = reason.clone();
                if kept.told.as_ref() != Some(&reason) {
                    self.line(
                        out,
                        &format!(
                            "nothing that would touch work in flight starts while {reason}"
                        ),
                    );
                    kept.told = Some(reason.clone());
                }
                return Err(reason);
            }
        };
        let named = on_demand
            .config
            .stack
            .named_resources()
            .unwrap_or_default();
        let holders = outside.holders(refs, &named);
        if kept.told.take().is_some() {
            self.line(out, "the work in flight outside Owlshift reads again");
        }
        for ticket in &holders.unknown {
            if kept.unknown.contains(ticket) {
                continue;
            }
            sink.emit(
                ticket,
                None,
                EventKind::Warning,
                data([("what", json!("zones_unknown"))]),
            );
            self.line(
                out,
                &format!(
                    "{ticket} is in the working state with neither a pull request nor a `zone:` \
                     label: it is not counted in flight"
                ),
            );
        }
        kept.unknown = holders.unknown.iter().cloned().collect();
        kept.kinds = holders.kinds.clone();
        Ok(holders)
    }

    /// The outside holders for this pass's stage steps, from the refs
    /// `read`: read only when a ticket whose next stage step the rule checks
    /// is there, one whose ref holds the slot and whose pull request is not
    /// open. When the read fails, those tickets are kept in
    /// [`OutsideMemory::unread_steps`].
    pub(super) fn step_holders(
        &self,
        memory: &mut Memory,
        sink: &mut EventSink<'_>,
        out: &mut dyn Write,
        read: &[RefRead],
    ) -> Option<Holders> {
        let checked: Vec<&TicketId> = read
            .iter()
            .filter_map(|(ticket, stored)| stored.as_ref().ok().map(|stored| (ticket, stored)))
            .filter(|(_, stored)| holds(stored) && !delivery_followed(stored))
            .map(|(ticket, _)| ticket)
            .collect();
        if checked.is_empty() {
            return None;
        }
        let refs = read
            .iter()
            .map(|(ticket, stored)| (ticket, stored.as_ref().ok()));
        match self.outside(memory, sink, out, refs) {
            Ok(holders) => Some(holders),
            Err(_) => {
                memory.outside.unread_steps = checked.into_iter().cloned().collect();
                None
            }
        }
    }

    /// Tells that `ticket`'s next stage step waits for the work in flight
    /// outside Owlshift to read: one `decision` event (`watch` = `waits`,
    /// `reason` = `in_flight_unread`) and one line, unless told last.
    pub(super) fn wait_for_unread(
        &self,
        memory: &mut Memory,
        sink: &mut EventSink<'_>,
        out: &mut dyn Write,
        ticket: &TicketId,
    ) {
        if memory.waits.get(ticket) == Some(&StepWait::Unread) {
            return;
        }
        sink.emit(
            ticket,
            None,
            EventKind::Decision,
            data([
                ("watch", json!("waits")),
                ("reason", json!("in_flight_unread")),
            ]),
        );
        self.line(
            out,
            &format!(
                "{ticket} waits: the work in flight outside Owlshift could not be read, and its \
                 next stage runs once it reads"
            ),
        );
        memory.waits.insert(ticket.clone(), StepWait::Unread);
    }
}
