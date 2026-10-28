//! The steps watch starts (OWL-399, OWL-401; architecture section 5,
//! "Parallel", "The step, in a child process"): in watch's own process,
//! where a step ends before the next starts, or in a child process for
//! `owlshift watch --drain`, where watch starts each step's child without
//! waiting for it and applies its end once it ended, at the next pass or
//! while it waits between passes. What follows a step's end, its holds,
//! the slot's turn and its lines, is the same either way ([`Then`]).

use std::io::Write;
use std::thread;
use std::time::{Duration, Instant};

use jiff::Timestamp;
use serde_json::json;

use owlshift_adapters::forge::ReviewsRead;
use owlshift_contracts::event::EventKind;
use owlshift_contracts::ids::TicketId;
use owlshift_contracts::step::{StepEnd, StepEnvelope};
use owlshift_core::state::{Status, TicketState};
use owlshift_core::vocab::Stage;

use super::{Continued, Memory, Seen, Watch};
use crate::events::{EventSink, data};
use crate::in_flight::Outside;
use crate::step::{self, Running};

/// How often watch looks at its running children while it waits between
/// two passes.
const CHILD_POLL: Duration = Duration::from_millis(100);

/// Whether a step that ended `end` ran: neither another command nor watch's
/// own priority held it back, a run slot was free, and its ref had not
/// moved.
pub(super) fn ran(end: &StepEnd) -> bool {
    !matches!(end, StepEnd::Busy | StepEnd::NoSlot | StepEnd::Stale)
}

/// What follows a step's end: the code that followed each step while watch
/// waited for it, kept with the step's child until it ends (OWL-401).
pub(super) enum Then {
    /// A repair, a correction or a review loop's start, from what watch
    /// read ([`Watch::held_run`]).
    Held { seen: Seen },
    /// An answer check, from what watch read at `now`.
    Answer { seen: Seen, now: Timestamp },
    /// A resume at `stage`, from what watch read at `now`.
    Resume {
        seen: Seen,
        stage: Stage,
        now: Timestamp,
    },
    /// The drain's intake.
    Intake { seen: Seen },
    /// The drain's dispatch from Ready.
    Dispatch { seen: Seen },
}

/// A step's child in flight, with what follows its end.
pub(super) struct RunningStep {
    child: Running,
    then: Then,
}

impl RunningStep {
    /// The ticket whose step it runs.
    pub(super) fn ticket(&self) -> &TicketId {
        self.child.ticket()
    }

    /// Whether it is an intake.
    pub(super) fn intake(&self) -> bool {
        matches!(self.then, Then::Intake { .. })
    }

    /// Whether it is a dispatch from Ready.
    pub(super) fn dispatch(&self) -> bool {
        matches!(self.then, Then::Dispatch { .. })
    }
}

impl Watch<'_> {
    /// Carries out `envelope`'s step (OWL-399): in watch's own process,
    /// with its memory of the pull requests it follows and `reviews`, the
    /// outcome told to `continued` as a continue's; or in a child, started
    /// without waiting for it (OWL-401), its record giving back that memory
    /// once it ends, the child having reported the outcome itself. A
    /// dispatch's step is handed `outside`, the work outside Owlshift the
    /// pass read. What follows its end, `then`, is applied at once when the
    /// step ended at once; otherwise once its child ends
    /// ([`Watch::reap`]). A child that leaves no record, or one that does
    /// not read, is a `step_unreported` warning, and the step is held as a
    /// refusal is. Returns how the step ended, or `None` while its child
    /// runs.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn step(
        &self,
        memory: &mut Memory,
        sink: &mut EventSink<'_>,
        out: &mut dyn Write,
        continued: &mut Continued<'_>,
        envelope: StepEnvelope,
        reviews: Option<ReviewsRead>,
        outside: Option<&Outside>,
        then: Then,
    ) -> Option<StepEnd> {
        let on_demand = self.on_demand;
        let ticket = envelope.ticket.clone();
        // Asked to stop (OWL-161): no step starts, before anything of it is
        // made; the watch ends once the pass does.
        if self.stopping().is_some() {
            return Some(StepEnd::Busy);
        }
        // An operator's pause (OWL-438), one written since the pass began
        // included: no step starts, nothing of it is made or held, and the
        // next pass after the pause is lifted decides it again.
        if self.operator_paused(memory, sink, out) {
            return Some(StepEnd::Busy);
        }
        // A base check due runs alone (OWL-418): no other step starts, nor
        // its own while another is in flight.
        if self.base_due_holds(memory, &ticket) {
            let end = StepEnd::Busy;
            self.finish(memory, sink, out, &ticket, then, &end);
            return Some(end);
        }
        // A step of its own starts: a turn a dispatch gave it is taken
        // (OWL-431), unless no run slot is free below.
        let turn = memory.base_turn.remove(&ticket);
        let Some(children) = memory.children else {
            let stepped = step::run(
                on_demand,
                &envelope,
                &mut memory.following,
                reviews,
                outside.cloned(),
                sink,
            );
            if let step::Stepped::Ran(outcome) = &stepped
                && step::reported(&envelope.step, outcome)
            {
                continued(&ticket, outcome, sink);
            }
            let end = step::end(&stepped);
            self.finish(memory, sink, out, &ticket, then, &end);
            return Some(end);
        };
        // Every run slot of the machine taken (OWL-409): no child is
        // started, and a later pass tries again. A count that cannot be
        // read leaves it to the child, whose own take decides.
        if children
            .slots()
            .is_some_and(|slots| slots.full().unwrap_or(false))
        {
            if turn {
                memory.base_turn.insert(ticket.clone());
            }
            let end = StepEnd::NoSlot;
            self.finish(memory, sink, out, &ticket, then, &end);
            return Some(end);
        }
        // What a dispatch weighs of the work outside Owlshift (OWL-401):
        // one that could not be handed is read again by the step.
        if let Some(outside) = outside {
            let _ = step::write_outside(on_demand.dirs, &envelope, outside);
        }
        match children.start(&envelope) {
            Ok(child) => {
                memory.running.push(RunningStep { child, then });
                None
            }
            // A stop asked meanwhile, which refuses every child from then
            // on (OWL-161): the step did not start.
            Err(_) if self.stopping().is_some() => {
                let _ = std::fs::remove_file(step::outside_file(
                    on_demand.dirs,
                    &ticket,
                    &envelope.attempt,
                ));
                Some(StepEnd::Busy)
            }
            Err(reason) => {
                self.unreported(sink, &ticket, &envelope.attempt, &reason);
                let _ = std::fs::remove_file(step::outside_file(
                    on_demand.dirs,
                    &ticket,
                    &envelope.attempt,
                ));
                let end = StepEnd::Other;
                self.finish(memory, sink, out, &ticket, then, &end);
                Some(end)
            }
        }
    }

    /// A step's child that left no outcome record, or one that does not
    /// read, or that could not start: a `step_unreported` warning.
    fn unreported(&self, sink: &mut EventSink<'_>, ticket: &TicketId, attempt: &str, reason: &str) {
        sink.emit(
            ticket,
            None,
            EventKind::Warning,
            data([
                ("what", json!("step_unreported")),
                ("attempt", json!(attempt)),
                ("reason", json!(reason)),
            ]),
        );
    }

    /// Whether a base check due holds `ticket`'s step back (OWL-418): another
    /// ticket's is due, or its own while another step is in flight. A due
    /// check whose ticket is no longer at Build, or past
    /// [`super::BASE_DUE_AT_MOST`], is forgotten.
    fn base_due_holds(&self, memory: &mut Memory, ticket: &TicketId) -> bool {
        let Some((due, since)) = memory.base_due.clone() else {
            return false;
        };
        let now = (self.on_demand.clock)();
        let expired = now.duration_since(since).unsigned_abs() > super::BASE_DUE_AT_MOST;
        let at_build = self.read_after(&due).is_some_and(|stored| {
            TicketState::try_from(&stored.record.state)
                .is_ok_and(|state| state.status() == Status::Active(Stage::Build))
        });
        if expired || !at_build {
            memory.base_due = None;
            memory.base_due_given_up.insert(due);
            return false;
        }
        due != *ticket || !memory.running.is_empty()
    }

    /// Applies what follows the end of `ticket`'s step.
    fn finish(
        &self,
        memory: &mut Memory,
        sink: &mut EventSink<'_>,
        out: &mut dyn Write,
        ticket: &TicketId,
        then: Then,
        end: &StepEnd,
    ) {
        // A check due that could not run beside other steps holds them back
        // until this ticket's next step has run (OWL-418).
        let ran = ran(end);
        if ran {
            memory.base_due_given_up.remove(ticket);
        }
        // Since when this ticket's check has been due, kept across its steps
        // that find it due still, so that the priority ends after
        // [`super::BASE_DUE_AT_MOST`] (OWL-431).
        let since = memory
            .base_due
            .as_ref()
            .filter(|(due, _)| due == ticket)
            .map(|(_, since)| *since);
        if ran && since.is_some() {
            memory.base_due_ran += 1;
            memory.base_due = None;
        }
        if let StepEnd::BaseHeld { pending: true, .. } = end {
            let since = since.unwrap_or_else(|| (self.on_demand.clock)());
            memory.base_due = Some((ticket.clone(), since));
        }
        // Which tickets a drain's dispatch gives a turn (OWL-431).
        if matches!(end, StepEnd::BaseHeld { .. }) {
            memory.base_held.insert(ticket.clone());
        } else if ran {
            memory.base_held.remove(ticket);
        }
        match then {
            Then::Held { seen } => self.held_after(memory, sink, out, ticket, seen, end),
            Then::Answer { seen, now } => {
                self.answered(memory, sink, out, ticket, seen, now, end);
            }
            Then::Resume { seen, stage, now } => {
                self.resumed(memory, sink, out, ticket, (seen, stage, now), end);
            }
            Then::Intake { seen } => self.taken_in(memory, ticket, seen, end, sink, out),
            Then::Dispatch { seen } => self.dispatched(memory, ticket, seen, end, sink, out),
        }
    }

    /// Polls every step's child that runs (OWL-401), printing what each
    /// wrote, and applies the end of each that ended. Returns whether one
    /// ended.
    pub(super) fn reap(
        &self,
        memory: &mut Memory,
        sink: &mut EventSink<'_>,
        out: &mut dyn Write,
    ) -> bool {
        let dirs = self.on_demand.dirs;
        let mut ended = false;
        let mut at = 0;
        while at < memory.running.len() {
            let Some(result) = memory.running[at].child.poll(dirs, out) else {
                at += 1;
                continue;
            };
            let RunningStep { child, then } = memory.running.remove(at);
            let ticket = child.ticket().clone();
            let attempt = child.attempt().to_owned();
            let end = match result {
                Ok(record) => {
                    memory.following.keep(&ticket, record.kept);
                    record.end
                }
                Err(reason) => {
                    self.unreported(sink, &ticket, &attempt, &reason);
                    StepEnd::Other
                }
            };
            self.finish(memory, sink, out, &ticket, then, &end);
            ended = true;
        }
        ended
    }

    /// Waits between two passes while steps' children run (OWL-401): until
    /// one ends, `interval` has passed, or a stop is asked (OWL-161),
    /// polling them. Returns what is left of the interval for `sleep`: all
    /// of it when none runs, none otherwise.
    pub(super) fn wait_for_children(
        &self,
        memory: &mut Memory,
        sink: &mut EventSink<'_>,
        out: &mut dyn Write,
        interval: Duration,
    ) -> Duration {
        if memory.running.is_empty() {
            return interval;
        }
        let started = Instant::now();
        while !self.reap(memory, sink, out) {
            let waited = started.elapsed();
            if waited >= interval || self.stopping().is_some() {
                break;
            }
            thread::sleep(CHILD_POLL.min(interval - waited));
        }
        Duration::ZERO
    }

    /// Waits for every step's child to end and applies each end: a watch
    /// whose loop ends lets its steps finish. A watch asked to stop ends
    /// through [`Watch::stop`] instead (OWL-161), and one stopped now never
    /// gets here: the policy every command shares stops every child's tree.
    pub(super) fn wait_for_all(
        &self,
        memory: &mut Memory,
        sink: &mut EventSink<'_>,
        out: &mut dyn Write,
    ) {
        while !memory.running.is_empty() {
            if !self.reap(memory, sink, out) {
                thread::sleep(CHILD_POLL);
            }
        }
    }
}
