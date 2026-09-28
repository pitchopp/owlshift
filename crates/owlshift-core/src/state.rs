//! The ticket state machine: where a ticket stands in its pipeline, and what
//! each event does to it.
//!
//! A ticket is working a stage, waiting for its decider's input, or parked
//! until a human restarts it. It leaves the machine when a human merges its
//! pull request or withdraws it. [`TicketState::apply`] is the only way to
//! move: an event that does not fit the current state is an error, never
//! silently ignored.

use std::fmt;

use crate::pipeline::Pipeline;
use crate::vocab::Stage;

/// Failed runs in one stage that park the ticket: the second one does.
pub const MAX_FAILED_RUNS: u32 = 2;

/// Re-asks of one question round before the ticket parks: an answer still
/// incomplete after the third re-ask parks it instead of a fourth.
pub const MAX_REASKS: u32 = 3;

/// Where a ticket stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Status {
    /// Working a stage: its role is due or running. At Ready, the ticket waits
    /// for the scheduler to dispatch it.
    Active(Stage),
    /// Questions wait for the decider. Once they are answered the ticket
    /// resumes at `return_to`: the stage that asked, or Ready for questions
    /// asked at Intake.
    NeedsInput { return_to: Stage },
    /// Parked by a circuit breaker, a blocked run or an isolation breach; a
    /// human restarts it. `awaiting_input` is set when it was parked while
    /// waiting for input, so a restart goes back to waiting for the answers
    /// rather than starting work without them.
    Parked { at: Stage, awaiting_input: bool },
}

impl Status {
    /// The stage the ticket is at: the one it works, returns to, or was parked
    /// at.
    pub const fn stage(self) -> Stage {
        match self {
            Status::Active(stage)
            | Status::NeedsInput { return_to: stage }
            | Status::Parked { at: stage, .. } => stage,
        }
    }

    /// Whether the ticket waits for its decider's answers.
    const fn awaits_input(self) -> bool {
        matches!(
            self,
            Status::NeedsInput { .. }
                | Status::Parked {
                    awaiting_input: true,
                    ..
                }
        )
    }

    /// Whether a run can be in flight: a stage's role (not at Ready, where
    /// nothing runs), or the answer check while waiting for input.
    fn runs(self) -> bool {
        match self {
            Status::Active(stage) => stage != Stage::Ready,
            Status::NeedsInput { .. } => true,
            Status::Parked { .. } => false,
        }
    }
}

/// Something that happened to a ticket.
///
/// How the runner maps what it observes onto events:
///
/// | Observed | Event |
/// | --- | --- |
/// | The scheduler claimed a ready ticket | `Dispatched` |
/// | A stage run returned `done`, and the stage passed | `Completed` |
/// | A stage run returned `done` asking for a revision or a fix; at Watch, a red check, a merge conflict or review comments | `LoopBack` |
/// | A stage run returned `questions` or `premise_false` (after the resolver) | `Questions` |
/// | A stage run returned `blocked` | `Blocked` |
/// | The answer check found every question answered | `Answered` |
/// | The answer check found questions left open (they are re-asked) | `Incomplete` |
/// | The decider asked a counter-question (answered in the thread) | `CounterQuestion` |
/// | A run returned `failed` or no valid `result.json`, whatever its exit code | `RunFailed` |
/// | A run was cut off by a harness usage limit; it resumes after the reset | `Interrupted` |
/// | A run broke isolation (main checkout touched, diff outside the worktree, wrong branch) | `Quarantined` |
/// | A human restarted a parked ticket | `Restarted` |
/// | A human merged the pull request | `Merged` |
/// | A human canceled, closed or excluded the ticket | `Withdrawn` |
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Event {
    Dispatched,
    Completed,
    LoopBack,
    Questions,
    Blocked,
    Answered,
    Incomplete,
    CounterQuestion,
    RunFailed,
    Interrupted,
    Quarantined,
    Restarted,
    Merged,
    Withdrawn,
}

impl Event {
    /// Every event.
    pub const ALL: [Self; 14] = [
        Self::Dispatched,
        Self::Completed,
        Self::LoopBack,
        Self::Questions,
        Self::Blocked,
        Self::Answered,
        Self::Incomplete,
        Self::CounterQuestion,
        Self::RunFailed,
        Self::Interrupted,
        Self::Quarantined,
        Self::Restarted,
        Self::Merged,
        Self::Withdrawn,
    ];
}

/// Why a ticket was parked; the parked comment says so.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ParkReason {
    /// A second run failed in the same stage.
    FailedRuns,
    /// The answer was still incomplete after three re-asks.
    Reasks,
    /// A run reported it cannot go on.
    Blocked,
    /// A run broke isolation; it is quarantined.
    IsolationBreach,
}

/// How a ticket left the machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Finish {
    /// A human merged the pull request.
    Merged,
    /// A human canceled, closed or excluded the ticket.
    Withdrawn,
}

/// The result of applying an event.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Transition {
    /// The ticket continues in this state (possibly unchanged).
    To(TicketState),
    /// The ticket was parked, for this reason.
    Parked {
        state: TicketState,
        reason: ParkReason,
    },
    /// The ticket left the machine.
    Finished(Finish),
}

/// A ticket's position in its pipeline, with the counters behind the circuit
/// breakers. It maps onto the persisted ticket state: `stage` is
/// [`Status::stage`], and it waits for input or is parked according to
/// [`Status`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TicketState {
    status: Status,
    round: u32,
    reasks: u32,
    failed_runs: u32,
}

impl TicketState {
    /// A ticket just admitted: at Intake, with every counter at zero.
    pub const fn admitted() -> Self {
        Self {
            status: Status::Active(Stage::Intake),
            round: 0,
            reasks: 0,
            failed_runs: 0,
        }
    }

    /// Rebuilds a state from its persisted parts, refusing one that breaks an
    /// invariant of the machine:
    ///
    /// - at most [`MAX_REASKS`] re-asks, and re-asks only while waiting for
    ///   input;
    /// - fewer than [`MAX_FAILED_RUNS`] failed runs unless parked, and never
    ///   more;
    /// - waiting for input means a question round was asked, and never at
    ///   Intake (intake questions return to Ready);
    /// - a ticket at Intake has asked no question round;
    /// - nothing runs at Ready, so nothing is parked there unless it was
    ///   waiting for input.
    pub fn restore(
        status: Status,
        round: u32,
        reasks: u32,
        failed_runs: u32,
    ) -> Result<Self, InvalidState> {
        let parked = matches!(status, Status::Parked { .. });
        let problem = if reasks > MAX_REASKS {
            Some("more re-asks than the breaker allows")
        } else if reasks > 0 && !status.awaits_input() {
            Some("re-asks while not waiting for input")
        } else if failed_runs > MAX_FAILED_RUNS || (failed_runs == MAX_FAILED_RUNS && !parked) {
            Some("failed runs past the breaker without parking")
        } else if status.awaits_input() && round == 0 {
            Some("waiting for input with no question round")
        } else if status.awaits_input() && status.stage() == Stage::Intake {
            Some("waiting for input at intake")
        } else if status.stage() == Stage::Intake && round > 0 {
            Some("a question round asked at intake")
        } else if status
            == (Status::Parked {
                at: Stage::Ready,
                awaiting_input: false,
            })
        {
            Some("parked at ready, where nothing runs")
        } else {
            None
        };
        let state = Self {
            status,
            round,
            reasks,
            failed_runs,
        };
        match problem {
            None => Ok(state),
            Some(reason) => Err(InvalidState { state, reason }),
        }
    }

    pub const fn status(&self) -> Status {
        self.status
    }

    /// The stage the ticket is at; see [`Status::stage`].
    pub const fn stage(&self) -> Stage {
        self.status.stage()
    }

    /// Question rounds asked so far; re-asks do not count.
    pub const fn round(&self) -> u32 {
        self.round
    }

    /// Re-asks of the current round.
    pub const fn reasks(&self) -> u32 {
        self.reasks
    }

    /// Failed runs in the current stage.
    pub const fn failed_runs(&self) -> u32 {
        self.failed_runs
    }

    /// Applies an event to this state, in the ticket's pipeline.
    ///
    /// A human merge or withdrawal ends the ticket from any state. Every other
    /// event is valid only in some states (see [`Event`]); elsewhere, and for a
    /// question round past `u32::MAX`, it is an [`InvalidTransition`].
    pub fn apply(&self, pipeline: Pipeline, event: Event) -> Result<Transition, InvalidTransition> {
        let invalid = InvalidTransition {
            from: self.status,
            event,
        };
        let transition = match (self.status, event) {
            (_, Event::Merged) => Transition::Finished(Finish::Merged),
            (_, Event::Withdrawn) => Transition::Finished(Finish::Withdrawn),

            (Status::Active(Stage::Ready), Event::Dispatched) => {
                let first = pipeline.next(Stage::Ready).ok_or(invalid)?;
                Transition::To(self.enter(Status::Active(first)))
            }
            // Watch ends with a merge, never by completing.
            (Status::Active(stage), Event::Completed)
                if stage != Stage::Ready && stage != Stage::Watch =>
            {
                let next = pipeline.next(stage).ok_or(invalid)?;
                Transition::To(self.enter(Status::Active(next)))
            }
            (Status::Active(stage), Event::LoopBack) => {
                let target = pipeline.loop_back(stage).ok_or(invalid)?;
                Transition::To(self.enter(Status::Active(target)))
            }
            (Status::Active(stage), Event::Questions) if stage != Stage::Ready => {
                let round = self.round.checked_add(1).ok_or(invalid)?;
                let return_to = if stage == Stage::Intake {
                    Stage::Ready
                } else {
                    stage
                };
                Transition::To(Self {
                    round,
                    reasks: 0,
                    ..self.enter(Status::NeedsInput { return_to })
                })
            }
            (Status::Active(stage), Event::Blocked) if stage != Stage::Ready => {
                self.park(ParkReason::Blocked, self.failed_runs)
            }

            (Status::NeedsInput { return_to }, Event::Answered) => Transition::To(Self {
                reasks: 0,
                ..self.enter(Status::Active(return_to))
            }),
            (Status::NeedsInput { .. }, Event::Incomplete) => {
                if self.reasks >= MAX_REASKS {
                    self.park(ParkReason::Reasks, self.failed_runs)
                } else {
                    Transition::To(Self {
                        reasks: self.reasks + 1,
                        ..*self
                    })
                }
            }
            (Status::NeedsInput { .. }, Event::CounterQuestion) => Transition::To(*self),

            (status, Event::RunFailed) if status.runs() => {
                let failed_runs = self.failed_runs + 1;
                if failed_runs >= MAX_FAILED_RUNS {
                    self.park(ParkReason::FailedRuns, failed_runs)
                } else {
                    Transition::To(Self {
                        failed_runs,
                        ..*self
                    })
                }
            }
            // A usage limit is not a failure: the run resumes after the reset.
            (status, Event::Interrupted) if status.runs() => Transition::To(*self),
            (status, Event::Quarantined) if status.runs() => {
                self.park(ParkReason::IsolationBreach, self.failed_runs)
            }

            (Status::Parked { at, awaiting_input }, Event::Restarted) => {
                let status = if awaiting_input {
                    Status::NeedsInput { return_to: at }
                } else {
                    Status::Active(at)
                };
                Transition::To(Self {
                    status,
                    round: self.round,
                    reasks: 0,
                    failed_runs: 0,
                })
            }

            _ => return Err(invalid),
        };
        Ok(transition)
    }

    /// This state moved to `status`; the failed-run count restarts when the
    /// stage changes.
    fn enter(&self, status: Status) -> Self {
        let failed_runs = if status.stage() == self.stage() {
            self.failed_runs
        } else {
            0
        };
        Self {
            status,
            failed_runs,
            ..*self
        }
    }

    /// Parks the ticket at its current stage, remembering whether it was
    /// waiting for input.
    fn park(&self, reason: ParkReason, failed_runs: u32) -> Transition {
        let status = Status::Parked {
            at: self.stage(),
            awaiting_input: self.status.awaits_input(),
        };
        Transition::Parked {
            state: Self {
                status,
                failed_runs,
                ..*self
            },
            reason,
        }
    }
}

/// An event that does not fit the ticket's state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct InvalidTransition {
    pub from: Status,
    pub event: Event,
}

impl fmt::Display for InvalidTransition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "event {:?} is not valid for a ticket in state {:?}",
            self.event, self.from
        )
    }
}

impl std::error::Error for InvalidTransition {}

/// Persisted parts that do not form a state the machine can be in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct InvalidState {
    state: TicketState,
    reason: &'static str,
}

impl fmt::Display for InvalidState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let TicketState {
            status,
            round,
            reasks,
            failed_runs,
        } = self.state;
        write!(
            f,
            "invalid ticket state {status:?} (round {round}, re-asks {reasks}, failed runs {failed_runs}): {}",
            self.reason
        )
    }
}

impl std::error::Error for InvalidState {}

#[cfg(test)]
mod tests {
    use std::collections::{HashSet, VecDeque};

    use super::*;
    use crate::vocab::Variant;

    const STANDARD: Pipeline = Pipeline::new(Variant::Standard);

    /// Every status, valid or not.
    fn all_statuses() -> Vec<Status> {
        let mut statuses = Vec::new();
        for stage in Stage::ALL {
            statuses.push(Status::Active(stage));
            statuses.push(Status::NeedsInput { return_to: stage });
            for awaiting_input in [false, true] {
                statuses.push(Status::Parked {
                    at: stage,
                    awaiting_input,
                });
            }
        }
        statuses
    }

    /// The state with this status and the lowest counters it allows, or `None`
    /// for a status the machine never has.
    fn baseline(status: Status) -> Option<TicketState> {
        let round = u32::from(status.stage() != Stage::Intake);
        TicketState::restore(status, round, 0, 0).ok()
    }

    fn state(status: Status, round: u32, reasks: u32, failed_runs: u32) -> TicketState {
        TicketState::restore(status, round, reasks, failed_runs).expect("a valid state")
    }

    fn to(transition: Result<Transition, InvalidTransition>) -> TicketState {
        match transition {
            Ok(Transition::To(state)) => state,
            other => panic!("expected a state, got {other:?}"),
        }
    }

    #[derive(Debug, PartialEq)]
    enum Expect {
        Invalid,
        To(Status),
        Parked(Status, ParkReason),
        Finished(Finish),
    }

    /// The transition table of the standard pipeline, written out literally.
    fn expected(status: Status, event: Event) -> Expect {
        use Stage::*;
        let parked = |at, awaiting_input| Status::Parked { at, awaiting_input };
        match (status, event) {
            (_, Event::Merged) => Expect::Finished(Finish::Merged),
            (_, Event::Withdrawn) => Expect::Finished(Finish::Withdrawn),

            (Status::Active(Ready), Event::Dispatched) => Expect::To(Status::Active(Design)),
            (Status::Active(Intake), Event::Completed) => Expect::To(Status::Active(Ready)),
            (Status::Active(Design), Event::Completed) => Expect::To(Status::Active(DesignReview)),
            (Status::Active(DesignReview), Event::Completed) => Expect::To(Status::Active(Build)),
            (Status::Active(Build), Event::Completed) => Expect::To(Status::Active(Verify)),
            (Status::Active(Verify), Event::Completed) => Expect::To(Status::Active(Deliver)),
            (Status::Active(Deliver), Event::Completed) => Expect::To(Status::Active(Watch)),

            (Status::Active(DesignReview), Event::LoopBack) => Expect::To(Status::Active(Design)),
            (Status::Active(Verify), Event::LoopBack) => Expect::To(Status::Active(Build)),
            (Status::Active(Watch), Event::LoopBack) => Expect::To(Status::Active(Build)),

            (Status::Active(Intake), Event::Questions) => {
                Expect::To(Status::NeedsInput { return_to: Ready })
            }
            (Status::Active(stage), Event::Questions) if stage != Ready => {
                Expect::To(Status::NeedsInput { return_to: stage })
            }
            (Status::Active(stage), Event::Blocked) if stage != Ready => {
                Expect::Parked(parked(stage, false), ParkReason::Blocked)
            }
            (Status::Active(stage), Event::RunFailed | Event::Interrupted) if stage != Ready => {
                Expect::To(status)
            }
            (Status::Active(stage), Event::Quarantined) if stage != Ready => {
                Expect::Parked(parked(stage, false), ParkReason::IsolationBreach)
            }

            (Status::NeedsInput { return_to }, Event::Answered) => {
                Expect::To(Status::Active(return_to))
            }
            (
                Status::NeedsInput { .. },
                Event::Incomplete | Event::CounterQuestion | Event::RunFailed | Event::Interrupted,
            ) => Expect::To(status),
            (Status::NeedsInput { return_to }, Event::Quarantined) => {
                Expect::Parked(parked(return_to, true), ParkReason::IsolationBreach)
            }

            (
                Status::Parked {
                    at,
                    awaiting_input: false,
                },
                Event::Restarted,
            ) => Expect::To(Status::Active(at)),
            (
                Status::Parked {
                    at,
                    awaiting_input: true,
                },
                Event::Restarted,
            ) => Expect::To(Status::NeedsInput { return_to: at }),

            _ => Expect::Invalid,
        }
    }

    #[test]
    fn every_state_and_event_pair_follows_the_transition_table() {
        let states: Vec<_> = all_statuses().into_iter().filter_map(baseline).collect();
        // 8 active, 7 needs-input (never Intake), 7 parked at a stage that ran
        // (never Ready), 7 parked while waiting for input (never Intake).
        assert_eq!(states.len(), 29);
        for state in states {
            for event in Event::ALL {
                let actual = match state.apply(STANDARD, event) {
                    Err(error) => {
                        assert_eq!(
                            error,
                            InvalidTransition {
                                from: state.status(),
                                event
                            }
                        );
                        Expect::Invalid
                    }
                    Ok(Transition::To(next)) => Expect::To(next.status()),
                    Ok(Transition::Parked { state, reason }) => {
                        Expect::Parked(state.status(), reason)
                    }
                    Ok(Transition::Finished(finish)) => Expect::Finished(finish),
                };
                assert_eq!(
                    actual,
                    expected(state.status(), event),
                    "{:?} + {event:?}",
                    state.status()
                );
            }
        }
    }

    #[test]
    fn trivial_dispatches_to_build_and_has_no_design_loop() {
        let trivial = Pipeline::new(Variant::Trivial);
        let ready = state(Status::Active(Stage::Ready), 0, 0, 0);
        assert_eq!(
            to(ready.apply(trivial, Event::Dispatched)).status(),
            Status::Active(Stage::Build)
        );
        let review = state(Status::Active(Stage::DesignReview), 0, 0, 0);
        assert!(review.apply(trivial, Event::LoopBack).is_err());
        let verify = state(Status::Active(Stage::Verify), 0, 0, 0);
        assert_eq!(
            to(verify.apply(trivial, Event::LoopBack)).status(),
            Status::Active(Stage::Build)
        );
    }

    #[test]
    fn questions_open_a_new_round_and_intake_questions_return_to_ready() {
        let intake = state(Status::Active(Stage::Intake), 0, 0, 1);
        let waiting = to(intake.apply(STANDARD, Event::Questions));
        assert_eq!(
            waiting.status(),
            Status::NeedsInput {
                return_to: Stage::Ready
            }
        );
        assert_eq!(waiting.round(), 1);
        // The stage changed from Intake to Ready: its failure count restarts.
        assert_eq!(waiting.failed_runs(), 0);

        let build = state(Status::Active(Stage::Build), 1, 0, 1);
        let waiting = to(build.apply(STANDARD, Event::Questions));
        assert_eq!(waiting.round(), 2);
        assert_eq!(waiting.reasks(), 0);
        // Same stage: the earlier failure still counts.
        assert_eq!(waiting.failed_runs(), 1);
    }

    #[test]
    fn a_question_round_past_u32_max_is_refused() {
        let build = state(Status::Active(Stage::Build), u32::MAX, 0, 0);
        assert!(build.apply(STANDARD, Event::Questions).is_err());
    }

    #[test]
    fn the_answer_still_incomplete_after_three_reasks_parks() {
        let mut current = state(
            Status::NeedsInput {
                return_to: Stage::Build,
            },
            1,
            0,
            0,
        );
        for reasks in 1..=MAX_REASKS {
            current = to(current.apply(STANDARD, Event::Incomplete));
            assert_eq!(current.reasks(), reasks);
            assert_eq!(current.round(), 1, "a re-ask is not a new round");
        }
        let Ok(Transition::Parked { state, reason }) = current.apply(STANDARD, Event::Incomplete)
        else {
            panic!("the fourth incomplete answer must park the ticket");
        };
        assert_eq!(reason, ParkReason::Reasks);
        assert_eq!(
            state.status(),
            Status::Parked {
                at: Stage::Build,
                awaiting_input: true
            }
        );
    }

    #[test]
    fn an_answer_clears_the_reasks_and_resumes_at_the_return_stage() {
        let waiting = state(
            Status::NeedsInput {
                return_to: Stage::Watch,
            },
            2,
            2,
            1,
        );
        let resumed = to(waiting.apply(STANDARD, Event::Answered));
        assert_eq!(resumed, state(Status::Active(Stage::Watch), 2, 0, 1));
    }

    #[test]
    fn the_second_failed_run_in_a_stage_parks_and_a_new_stage_restarts_the_count() {
        let build = state(Status::Active(Stage::Build), 0, 0, 0);
        let once = to(build.apply(STANDARD, Event::RunFailed));
        assert_eq!(once.failed_runs(), 1);

        // Moving on restarts the count.
        let verify = to(once.apply(STANDARD, Event::Completed));
        assert_eq!(verify.failed_runs(), 0);
        let fixing =
            to(to(verify.apply(STANDARD, Event::RunFailed)).apply(STANDARD, Event::LoopBack));
        assert_eq!(fixing.failed_runs(), 0);

        let Ok(Transition::Parked { state, reason }) = once.apply(STANDARD, Event::RunFailed)
        else {
            panic!("the second failed run must park the ticket");
        };
        assert_eq!(reason, ParkReason::FailedRuns);
        assert_eq!(state.failed_runs(), MAX_FAILED_RUNS);
        assert_eq!(
            state.status(),
            Status::Parked {
                at: Stage::Build,
                awaiting_input: false
            }
        );
    }

    #[test]
    fn interruptions_and_counter_questions_change_nothing() {
        let waiting = state(
            Status::NeedsInput {
                return_to: Stage::Build,
            },
            3,
            2,
            1,
        );
        assert_eq!(to(waiting.apply(STANDARD, Event::Interrupted)), waiting);
        assert_eq!(to(waiting.apply(STANDARD, Event::CounterQuestion)), waiting);
        let build = state(Status::Active(Stage::Build), 3, 0, 1);
        assert_eq!(to(build.apply(STANDARD, Event::Interrupted)), build);
    }

    #[test]
    fn a_restart_clears_the_breakers_keeps_the_round_and_never_skips_open_questions() {
        let failed = state(
            Status::Parked {
                at: Stage::Verify,
                awaiting_input: false,
            },
            2,
            0,
            MAX_FAILED_RUNS,
        );
        assert_eq!(
            to(failed.apply(STANDARD, Event::Restarted)),
            state(Status::Active(Stage::Verify), 2, 0, 0)
        );

        let unanswered = state(
            Status::Parked {
                at: Stage::Build,
                awaiting_input: true,
            },
            1,
            MAX_REASKS,
            0,
        );
        assert_eq!(
            to(unanswered.apply(STANDARD, Event::Restarted)),
            state(
                Status::NeedsInput {
                    return_to: Stage::Build
                },
                1,
                0,
                0
            )
        );
    }

    #[test]
    fn every_reachable_state_can_be_restored() {
        let mut seen = HashSet::from([TicketState::admitted()]);
        let mut queue = VecDeque::from([TicketState::admitted()]);
        while let Some(current) = queue.pop_front() {
            for event in Event::ALL {
                let next = match current.apply(STANDARD, event) {
                    Ok(Transition::To(next) | Transition::Parked { state: next, .. }) => next,
                    Ok(Transition::Finished(_)) | Err(_) => continue,
                };
                if next.round() <= 3 && seen.insert(next) {
                    queue.push_back(next);
                }
            }
        }
        // Every status the table test uses is reachable.
        let statuses: HashSet<_> = seen.iter().map(TicketState::status).collect();
        assert_eq!(statuses.len(), 29);
        for reached in seen {
            let restored = TicketState::restore(
                reached.status(),
                reached.round(),
                reached.reasks(),
                reached.failed_runs(),
            );
            assert_eq!(restored, Ok(reached));
        }
    }

    #[test]
    fn restore_refuses_a_state_that_breaks_an_invariant() {
        let build = Status::Active(Stage::Build);
        let waiting = Status::NeedsInput {
            return_to: Stage::Build,
        };
        let parked = |at, awaiting_input| Status::Parked { at, awaiting_input };
        let broken = [
            (waiting, 1, MAX_REASKS + 1, 0),
            (build, 1, 1, 0),
            (parked(Stage::Build, false), 1, 1, 0),
            (build, 0, 0, MAX_FAILED_RUNS),
            (waiting, 1, 0, MAX_FAILED_RUNS),
            (parked(Stage::Build, false), 0, 0, MAX_FAILED_RUNS + 1),
            (waiting, 0, 0, 0),
            (parked(Stage::Build, true), 0, 0, 0),
            (
                Status::NeedsInput {
                    return_to: Stage::Intake,
                },
                1,
                0,
                0,
            ),
            (parked(Stage::Intake, true), 1, 0, 0),
            (Status::Active(Stage::Intake), 1, 0, 0),
            (parked(Stage::Intake, false), 1, 0, 0),
            (parked(Stage::Ready, false), 0, 0, 0),
        ];
        for (status, round, reasks, failed_runs) in broken {
            assert!(
                TicketState::restore(status, round, reasks, failed_runs).is_err(),
                "{status:?} round {round} re-asks {reasks} failed {failed_runs}"
            );
        }
    }

    #[test]
    fn errors_name_the_state_and_the_event() {
        let ready = TicketState::admitted();
        let error = ready.apply(STANDARD, Event::Answered).unwrap_err();
        assert_eq!(
            error.to_string(),
            "event Answered is not valid for a ticket in state Active(Intake)"
        );
        let error = TicketState::restore(Status::Active(Stage::Build), 0, 1, 0).unwrap_err();
        assert!(
            error
                .to_string()
                .ends_with("re-asks while not waiting for input")
        );
    }
}
