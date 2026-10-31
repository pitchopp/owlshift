//! `owlshift continue --to-watch` (OWL-420; architecture section 5, "A
//! restart handed to watch"): a park restarted as `continue` restarts it,
//! then left to `owlshift watch`, which drives the ticket from its next
//! pass; nothing runs in the command.
//!
//! - *What watch takes* ([`takes`]): the predicates watch selects its
//!   tickets by, so the two never drift: a waiting round
//!   ([`crate::watch::watched`]), a followed delivery at Watch, as
//!   [`crate::watch::followed`] tells it, a stage watch resumes
//!   ([`crate::watch::resumable_stage`]). A park restarted to Intake itself
//!   is refused, since it goes on showing parked (OWL-248) and watch resumes
//!   an Intake ticket only while it shows needs input.
//! - *The order*: the visible stage, then the ticket ref, unlike plain
//!   `continue` ([`OnDemand::restart_park`]); a ref that failed to move
//!   moves the stage back.
//! - *A ticket not parked*: nothing restarted or written; told what watch
//!   does with it when watch takes it as it stands, refused otherwise.

use super::*;
use crate::watch::{self, presence};

/// What watch does with a ticket left to it (OWL-420).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Takes {
    /// Watch resumes its stage, under its usual rules.
    Resume,
    /// Its round waits: watch checks the answers once the decider's reply
    /// counts.
    Answers,
    /// Watch follows its delivered pull request.
    Follow,
}

impl Takes {
    /// Its name in the restart's `decision` event.
    pub fn key(self) -> &'static str {
        match self {
            Takes::Resume => "resume",
            Takes::Answers => "answers",
            Takes::Follow => "follow",
        }
    }

    /// What watch does next, as the command says it.
    pub(super) fn next(self) -> &'static str {
        match self {
            Takes::Resume => "`owlshift watch` resumes it at its next pass, under its usual rules",
            Takes::Answers => "`owlshift watch` checks its answers once the decider's reply counts",
            Takes::Follow => "`owlshift watch` follows its pull request from its next pass",
        }
    }
}

/// What watch does with a ticket in `state`, `followed` whether its ref
/// keeps a delivery watch still follows; `None` when watch leaves it alone.
pub(super) fn takes(state: &TicketState, followed: bool) -> Option<Takes> {
    if watch::watched(state.status()) {
        Some(Takes::Answers)
    } else if followed && state.status() == Status::Active(Stage::Watch) {
        Some(Takes::Follow)
    } else if watch::resumable_stage(state).is_some() {
        Some(Takes::Resume)
    } else {
        None
    }
}

/// The refusal of the flag on a review loop whose Verify cannot resume:
/// plain `continue` builds the loop again (OWL-307), watch does not.
pub(super) fn rebuild_refused(ticket: &TicketId, why: &Stop) -> Stop {
    Stop::Refused(format!(
        "{why}; `owlshift continue {ticket}`, without `--to-watch`, builds its review loop again"
    ))
}

/// The refusal of the flag on a ticket watch does not take at `stage`.
fn not_taken(ticket: &TicketId, stage: &str) -> Stop {
    Stop::Refused(format!(
        "`owlshift watch` does not take {ticket} at {stage}: run `owlshift continue {ticket}` to \
         drive it now"
    ))
}

impl OnDemand<'_> {
    /// `owlshift continue --to-watch`: [`Self::continue_ticket`] up to the
    /// restart of a park, which it hands to watch with [`Stop::ToWatch`].
    pub fn continue_to_watch(
        &self,
        ticket: &TicketId,
        sink: &mut EventSink<'_>,
    ) -> Result<Delivered, Stop> {
        let mut p = self.prepare(ticket, Command::Continue)?;
        // A cancel still standing is withdrawn (OWL-474).
        self.withdraw_cancel(ticket, sink);
        self.drop_carry(ticket, sink);
        p.to_watch = true;
        self.continue_prepared(p, sink)
    }

    /// The flag's end, once `continue`'s first reads passed on `state`: the
    /// park restarted and left to watch, or a ticket not parked told what
    /// watch does with it; refused, nothing written, when watch would not
    /// take it.
    pub(super) fn hand_to_watch(
        &self,
        p: &mut Prepared,
        state: TicketState,
        sink: &mut EventSink<'_>,
    ) -> Stop {
        let ticket = p.ticket.clone();
        let followed = p
            .stored
            .as_ref()
            .and_then(|stored| stored.record.delivery.as_ref())
            .is_some_and(TicketDelivery::followed);
        let parked = matches!(state.status(), Status::Parked { .. });
        let next = if parked {
            match state.apply(p.pipeline, Event::Restarted) {
                Ok(Transition::To(next)) => next,
                other => return core_error(other),
            }
        } else {
            state
        };
        let stage = stage_name(next.stage());
        let Some(taken) = takes(&next, followed) else {
            return not_taken(&ticket, &stage);
        };
        if parked && next.status() == Status::Active(Stage::Intake) {
            return not_taken(&ticket, &stage);
        }
        if !parked
            && taken == Takes::Resume
            && let Some(shown) = watch::resumable_stage(&next)
        {
            // Watch resumes it only while the tracker shows `shown`.
            let wanted = shown.state(&self.config.tracker.states);
            match self.tracker.stage(&ticket) {
                Ok(found) if found == wanted => {}
                Ok(found) => {
                    return Stop::Refused(format!(
                        "{ticket} shows {found}, and `owlshift watch` resumes it at {stage} only \
                         while it shows {wanted}: move it there, or run `owlshift continue \
                         {ticket}` to drive it now"
                    ));
                }
                Err(e) => return refused(&format!("reading the stage of {ticket}"), e),
            }
        }
        if parked && let Err(stop) = self.restart_park(p, state, Some(taken), sink) {
            return stop;
        }
        Stop::ToWatch {
            ticket,
            stage: next.stage(),
            takes: taken,
            restarted: parked,
            watching: presence::watching(self.dirs).ok(),
        }
    }

    /// A person's restart of the park `state` (OWL-122): the core's restart,
    /// the budget counted afresh (OWL-378), the ref kept, the `decision`
    /// event, and the stage the restarted state shows, a park at Intake
    /// keeping the parked stage while intake runs (OWL-248). Plain
    /// `continue` keeps the ref, then shows the stage, a stage not moved
    /// being a warning. Handed to watch (`handed`, OWL-420), the stage moves
    /// first, refusing the restart with nothing written when it cannot, so
    /// that a pass of watch's never reads the restarted ref while the
    /// tracker still shows parked, which it would leave alone for ten
    /// minutes; a ref that failed to move then moves the stage back.
    pub(super) fn restart_park(
        &self,
        p: &mut Prepared,
        state: TicketState,
        handed: Option<Takes>,
        sink: &mut EventSink<'_>,
    ) -> Result<TicketState, Stop> {
        let ticket = &p.ticket.clone();
        let Status::Parked { at, .. } = state.status() else {
            return Ok(state);
        };
        let state = match state.apply(p.pipeline, Event::Restarted) {
            Ok(Transition::To(state)) => state,
            other => return Err(core_error(other)),
        };
        // The ticket leaves the parked stage now: whatever stops this
        // command next, it no longer shows Owlshift stopped on it. A ticket
        // parked at Intake keeps it while intake runs, out of the ready
        // column, and goes back there once intake ends at Ready (OWL-248);
        // one parked while its intake round waited waits again, and shows
        // it (OWL-269).
        let shown = VisibleStage::of(state.status())
            .filter(|_| state.status() != Status::Active(Stage::Intake));
        if handed.is_some()
            && let Some(stage) = shown
            && let Err(reason) = self.move_stage(ticket, None, stage, sink)
        {
            return Err(Stop::Refused(format!(
                "{ticket} could not be shown {}, so nothing is restarted: {reason}",
                stage.state(&self.config.tracker.states)
            )));
        }
        // The person's restart counts the ticket's budget afresh (OWL-378),
        // as it resets the breakers' counts.
        p.cost.restarted();
        let before = p.stored.as_ref().map(|stored| stored.commit.clone());
        if let Err(stop) = self.keep(p, &state) {
            if handed.is_some()
                && let Some(stage) = shown
                && !self.unshow_restart(p, before.as_deref(), sink)
            {
                return Err(Stop::Refused(format!(
                    "{stop}; {ticket} still shows {}, its ref having moved or not being \
                     readable: run `owlshift continue {ticket}` once it can be written",
                    stage.state(&self.config.tracker.states)
                )));
            }
            return Err(stop);
        }
        let mut event = data([("restarted", json!(true)), ("stage", json!(stage_name(at)))]);
        if let Some(taken) = handed {
            event.insert("to_watch".to_owned(), json!(true));
            event.insert("takes".to_owned(), json!(taken.key()));
        }
        sink.emit(ticket, None, EventKind::Decision, event);
        if handed.is_none()
            && let Some(stage) = shown
        {
            self.show(ticket, None, stage, sink);
        }
        Ok(state)
    }

    /// After a restart's ref write failed, the stage back to parked, while
    /// the ref, read again under the ticket's lock this command holds, is
    /// still `before`, the one the command started from: one that moved
    /// anyway (its run file not saved) or cannot be read keeps the stage
    /// the restart showed, and is told by `false`.
    fn unshow_restart(&self, p: &Prepared, before: Option<&str>, sink: &mut EventSink<'_>) -> bool {
        let ticket = &p.ticket;
        let unchanged = matches!(
            ticket_ref::read(&self.executor.git, &p.checkout, ticket),
            Ok(Some(stored)) if Some(stored.commit.as_str()) == before
        );
        if unchanged {
            self.show(ticket, None, VisibleStage::Parked, sink);
        }
        unchanged
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(status: Status, round: u32) -> TicketState {
        TicketState::restore(status, round, 0, 0, 0, 0).unwrap()
    }

    /// Every state a ticket can be in, parked or not, with and without a
    /// followed delivery: what the flag hands to watch.
    #[test]
    fn the_flag_hands_watch_what_watch_takes() {
        use Stage::*;
        for stage in [
            Intake,
            Ready,
            Design,
            DesignReview,
            Build,
            Verify,
            Deliver,
            Watch,
        ] {
            for round in [0, 1] {
                for followed in [false, true] {
                    let Ok(active) = TicketState::restore(Status::Active(stage), round, 0, 0, 0, 0)
                    else {
                        continue;
                    };
                    let active = takes(&active, followed);
                    let expected = match stage {
                        Design | DesignReview | Build | Verify => Some(Takes::Resume),
                        Intake if round > 0 => Some(Takes::Resume),
                        Watch if followed => Some(Takes::Follow),
                        _ => None,
                    };
                    assert_eq!(active, expected, "{stage:?} round {round} {followed}");
                    if TicketState::restore(
                        Status::NeedsInput { return_to: stage },
                        round,
                        0,
                        0,
                        0,
                        0,
                    )
                    .is_ok()
                    {
                        let waiting = state(Status::NeedsInput { return_to: stage }, round);
                        assert_eq!(takes(&waiting, followed), Some(Takes::Answers));
                    }
                    for awaiting_input in [false, true] {
                        let parked = Status::Parked {
                            at: stage,
                            awaiting_input,
                        };
                        if let Ok(parked) = TicketState::restore(parked, round, 0, 0, 0, 0) {
                            assert_eq!(takes(&parked, followed), None, "{parked:?}");
                        }
                    }
                }
            }
        }
    }
}
