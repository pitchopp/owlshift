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
use crate::vocab::{AnswerClass, Stage};

/// Failed runs in one stage that park the ticket: the second one does. A
/// refused Build or answer-check result never brings the count there
/// ([`Event::ResultRefused`]).
pub const MAX_FAILED_RUNS: u32 = 2;

/// Re-asks of one question round before the ticket parks: an answer still
/// incomplete after the third re-ask parks it instead of a fourth.
pub const MAX_REASKS: u32 = 3;

/// Runs in a row whose questions the resolver all decided, so that the stage
/// ran again without a stop, before the questions of the next run go to the
/// decider without the resolver: a stage that keeps asking what the
/// repository settles cannot loop without bound (architecture, section 4).
pub const MAX_RESOLVED_PASSES: u32 = 3;

/// Fix passes since the pull request was last read green: a fix pass is a
/// fix run at Watch whose push landed, and a red read after the second one
/// parks the ticket instead of a third ([`Event::FixDue`]; architecture,
/// section 5, "The bound").
pub const MAX_FIX_PASSES: u32 = 2;

/// Repairs of one delivery: every fix or rebase run at Watch whose push
/// landed. A green read does not clear them, so a flaky check or a busy base
/// cannot loop without end: the run due after the sixth parks the ticket
/// ([`Event::FixDue`], [`Event::RebaseDue`]).
pub const MAX_REPAIRS: u32 = 6;

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
/// | A stage run returned `done` asking for a revision or a fix; at Watch, review comments on the pull request (from P5) | `LoopBack` |
/// | A stage run returned `questions` or `premise_false` (after the resolver) | `Questions` |
/// | A stage run returned `blocked` | `Blocked` |
/// | The answer check found every question answered | `Answered` |
/// | The answer check found questions left open (they are re-asked) | `Incomplete` |
/// | The decider asked a counter-question (answered in the thread) | `CounterQuestion` |
/// | A run returned `failed` or no valid `result.json`, whatever its exit code, save the row below; a Build `done` whose project gate, run by the runner, failed | `RunFailed` |
/// | A Build run's (at Build, or a fix run's at Watch) or an answer check's `result.json` was refused, and the run had not been told of a refused result: the next run of its kind will be | `ResultRefused` |
/// | A run was cut off by a harness usage limit; it resumes after the reset | `Interrupted` |
/// | A run broke isolation (main checkout touched, diff outside the worktree, wrong branch) | `Quarantined` |
/// | At Watch, a fix run's push landed on the pull request | `FixPushed` |
/// | At Watch, a rebase run's push landed on the pull request | `RebasePushed` |
/// | At Watch, a correction run's push landed, or it found the pull request already following the decider's replies | `Corrected` |
/// | At Watch, the pull request was read green | `ChecksGreen` |
/// | At Watch, a read calls for a fix run, before it runs | `FixDue` |
/// | At Watch, a read calls for a rebase run, before it runs | `RebaseDue` |
/// | At Watch, a red no run fixes, or a push the forge refused | `ForgeBlocked` |
/// | While a round waits at Watch, or the ticket is parked there, its pull request was merged, closed, or pushed to by someone else | `FollowingEnded` |
/// | A human restarted a parked ticket | `Restarted` |
/// | A human merged the pull request | `Merged` |
/// | A human canceled, closed or excluded the ticket | `Withdrawn` |
///
/// One answer check gives exactly one of `Answered`, `Incomplete` and
/// `CounterQuestion`, even when it finds its questions in different states:
/// see [`Event::from_answer_check`].
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
    /// A failed run that never spends the stage's last attempt (OWL-183):
    /// it counts as [`Event::RunFailed`] below the last attempt, and at the
    /// last attempt leaves the ticket as it is, so the next run of its
    /// kind, told why the result was refused, gets that attempt. Only
    /// Build's brief and the answer check's carry that reason, so it is
    /// valid at Build, at Watch, where a fix run is the build role's, and
    /// while waiting for input, where the answer check runs (OWL-190). The bound is the runner's to keep: it gives this
    /// event only for a run that was not itself told of a refused result,
    /// so that of two refusals in a row, the second is a `RunFailed`.
    ResultRefused,
    Interrupted,
    Quarantined,
    /// A fix run's push landed (OWL-55): the ticket stays at Watch, its
    /// failed runs are cleared, and a fix pass and a repair are counted.
    /// The runner gives [`Event::FixDue`] before the run, so a push past a
    /// bound is an [`InvalidTransition`].
    FixPushed,
    /// A rebase run's push landed: as [`Event::FixPushed`], counting a
    /// repair and no fix pass.
    RebasePushed,
    /// A correction run, after the decider reversed a decision (OWL-146),
    /// pushed its commit or found nothing to change: the ticket stays at
    /// Watch and its failed runs are cleared. It counts no fix pass and no
    /// repair: each correction answers a reply of the decider's.
    Corrected,
    /// The pull request was read green: the fix passes and the failed runs
    /// are cleared, and the repairs are kept.
    ChecksGreen,
    /// A read calls for a fix run. It parks the ticket at a bound, the fix
    /// passes first, then the repairs; otherwise it changes nothing and
    /// the fix run may start.
    FixDue,
    /// A read calls for a rebase run. It parks the ticket when the repairs
    /// are at their bound; otherwise it changes nothing and the rebase run
    /// may start.
    RebaseDue,
    /// A red no run fixes, or a push the forge refused: the ticket parks.
    ForgeBlocked,
    /// Watch stopped following the pull request (merged, closed, or a head
    /// someone else pushed) while a round waited at Watch (OWL-242): the
    /// round is closed unanswered and the ticket is back at Watch, its
    /// counts kept. Since OWL-243 it also ends a park at Watch, the park
    /// a person was to look at: back at Watch with the round, the fix passes
    /// and the repairs kept, its re-asks and failed runs cleared. Nothing
    /// runs after it, and no bound is reset. Valid only while waiting with
    /// Watch as the return stage, or parked at Watch.
    FollowingEnded,
    Restarted,
    Merged,
    Withdrawn,
}

impl Event {
    /// Every event.
    pub const ALL: [Self; 23] = [
        Self::Dispatched,
        Self::Completed,
        Self::LoopBack,
        Self::Questions,
        Self::Blocked,
        Self::Answered,
        Self::Incomplete,
        Self::CounterQuestion,
        Self::RunFailed,
        Self::ResultRefused,
        Self::Interrupted,
        Self::Quarantined,
        Self::FixPushed,
        Self::RebasePushed,
        Self::Corrected,
        Self::ChecksGreen,
        Self::FixDue,
        Self::RebaseDue,
        Self::ForgeBlocked,
        Self::FollowingEnded,
        Self::Restarted,
        Self::Merged,
        Self::Withdrawn,
    ];

    /// The one event of an answer check round, from the class of each
    /// question it checked.
    ///
    /// A counter-question on any question wins: the decider gets a reply and
    /// nothing is re-asked, so the round does not count toward the re-ask
    /// limit, and every question of the ask is checked again after the
    /// decider's next reply. Otherwise a partial or unanswered question makes
    /// the answer incomplete. Only when every question is answered does the
    /// ticket resume. Order and repetition do not matter.
    ///
    /// The fold sees only the classes it is given: pass those of a `done`
    /// answer check whose result was checked against its brief, which gives
    /// one verdict for each question of the latest ask. Every ask has a
    /// question, so `None`, for no class at all, never comes from such a
    /// result.
    pub fn from_answer_check(classes: impl IntoIterator<Item = AnswerClass>) -> Option<Self> {
        let mut checked = false;
        let mut missing = false;
        for class in classes {
            checked = true;
            match class {
                AnswerClass::CounterQuestion => return Some(Self::CounterQuestion),
                AnswerClass::Partial | AnswerClass::Unanswered => missing = true,
                AnswerClass::Answered => {}
            }
        }
        match (checked, missing) {
            (false, _) => None,
            (true, true) => Some(Self::Incomplete),
            (true, false) => Some(Self::Answered),
        }
    }
}

/// Why a ticket was parked; the parked comment says so.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ParkReason {
    /// A run failed in a stage with no attempt left: the second failed run
    /// counted there.
    FailedRuns,
    /// The answer was still incomplete after three re-asks.
    Reasks,
    /// A run reported it cannot go on.
    Blocked,
    /// A run broke isolation; it is quarantined.
    IsolationBreach,
    /// The pull request was read red after [`MAX_FIX_PASSES`] fix passes
    /// since it was last green.
    FixPasses,
    /// A repair was due after [`MAX_REPAIRS`] repairs of the delivery.
    Repairs,
    /// The forge blocks the pull request: a red no run fixes, or a refused
    /// push.
    Forge,
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
    fix_passes: u32,
    repairs: u32,
}

impl TicketState {
    /// A ticket just admitted: at Intake, with every counter at zero.
    pub const fn admitted() -> Self {
        Self {
            status: Status::Active(Stage::Intake),
            round: 0,
            reasks: 0,
            failed_runs: 0,
            fix_passes: 0,
            repairs: 0,
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
    ///   waiting for input;
    /// - at most [`MAX_FIX_PASSES`] fix passes and [`MAX_REPAIRS`] repairs,
    ///   and either only at Watch.
    pub fn restore(
        status: Status,
        round: u32,
        reasks: u32,
        failed_runs: u32,
        fix_passes: u32,
        repairs: u32,
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
        } else if fix_passes > MAX_FIX_PASSES {
            Some("more fix passes than the bound allows")
        } else if repairs > MAX_REPAIRS {
            Some("more repairs than the bound allows")
        } else if (fix_passes > 0 || repairs > 0) && status.stage() != Stage::Watch {
            Some("fix passes or repairs outside watch")
        } else {
            None
        };
        let state = Self {
            status,
            round,
            reasks,
            failed_runs,
            fix_passes,
            repairs,
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

    /// Fix passes since the pull request was last read green; only at Watch.
    pub const fn fix_passes(&self) -> u32 {
        self.fix_passes
    }

    /// Repairs of the current delivery; only at Watch.
    pub const fn repairs(&self) -> u32 {
        self.repairs
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
            (
                Status::NeedsInput {
                    return_to: Stage::Watch,
                },
                Event::FollowingEnded,
            ) => Transition::To(Self {
                reasks: 0,
                ..self.enter(Status::Active(Stage::Watch))
            }),
            // The park a person was to look at ends with its pull request
            // (OWL-243): the failed runs it may have parked on are cleared,
            // the fix passes and repairs kept, so no bound is reset.
            (
                Status::Parked {
                    at: Stage::Watch, ..
                },
                Event::FollowingEnded,
            ) => Transition::To(Self {
                reasks: 0,
                failed_runs: 0,
                ..self.enter(Status::Active(Stage::Watch))
            }),

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
            (
                Status::Active(Stage::Build | Stage::Watch) | Status::NeedsInput { .. },
                Event::ResultRefused,
            ) => {
                let failed_runs = (self.failed_runs + 1).min(MAX_FAILED_RUNS - 1);
                Transition::To(Self {
                    failed_runs,
                    ..*self
                })
            }
            // A usage limit is not a failure: the run resumes after the reset.
            (status, Event::Interrupted) if status.runs() => Transition::To(*self),
            (status, Event::Quarantined) if status.runs() => {
                self.park(ParkReason::IsolationBreach, self.failed_runs)
            }

            (Status::Active(Stage::Watch), Event::FixPushed) => {
                if self.fix_passes >= MAX_FIX_PASSES || self.repairs >= MAX_REPAIRS {
                    return Err(invalid);
                }
                Transition::To(Self {
                    fix_passes: self.fix_passes + 1,
                    repairs: self.repairs + 1,
                    failed_runs: 0,
                    ..*self
                })
            }
            (Status::Active(Stage::Watch), Event::RebasePushed) => {
                if self.repairs >= MAX_REPAIRS {
                    return Err(invalid);
                }
                Transition::To(Self {
                    repairs: self.repairs + 1,
                    failed_runs: 0,
                    ..*self
                })
            }
            (Status::Active(Stage::Watch), Event::Corrected) => Transition::To(Self {
                failed_runs: 0,
                ..*self
            }),
            (Status::Active(Stage::Watch), Event::ChecksGreen) => Transition::To(Self {
                fix_passes: 0,
                failed_runs: 0,
                ..*self
            }),
            (Status::Active(Stage::Watch), Event::FixDue) => {
                if self.fix_passes >= MAX_FIX_PASSES {
                    self.park(ParkReason::FixPasses, self.failed_runs)
                } else if self.repairs >= MAX_REPAIRS {
                    self.park(ParkReason::Repairs, self.failed_runs)
                } else {
                    Transition::To(*self)
                }
            }
            (Status::Active(Stage::Watch), Event::RebaseDue) => {
                if self.repairs >= MAX_REPAIRS {
                    self.park(ParkReason::Repairs, self.failed_runs)
                } else {
                    Transition::To(*self)
                }
            }
            (Status::Active(Stage::Watch), Event::ForgeBlocked) => {
                self.park(ParkReason::Forge, self.failed_runs)
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
                    fix_passes: 0,
                    repairs: 0,
                })
            }

            _ => return Err(invalid),
        };
        Ok(transition)
    }

    /// This state moved to `status`. The failed-run count restarts when the
    /// stage changes, and so do the fix passes and repairs, which count
    /// only at Watch: leaving it, which only review comments do (P5), ends
    /// the delivery they counted.
    fn enter(&self, status: Status) -> Self {
        if status.stage() == self.stage() {
            Self { status, ..*self }
        } else {
            Self {
                status,
                failed_runs: 0,
                fix_passes: 0,
                repairs: 0,
                ..*self
            }
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
            fix_passes,
            repairs,
        } = self.state;
        write!(
            f,
            "invalid ticket state {status:?} (round {round}, re-asks {reasks}, failed runs \
             {failed_runs}, fix passes {fix_passes}, repairs {repairs}): {}",
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
        TicketState::restore(status, round, 0, 0, 0, 0).ok()
    }

    fn state(status: Status, round: u32, reasks: u32, failed_runs: u32) -> TicketState {
        TicketState::restore(status, round, reasks, failed_runs, 0, 0).expect("a valid state")
    }

    /// A ticket working at Watch with these counts, in round 1.
    fn watch(failed_runs: u32, fix_passes: u32, repairs: u32) -> TicketState {
        TicketState::restore(
            Status::Active(Stage::Watch),
            1,
            0,
            failed_runs,
            fix_passes,
            repairs,
        )
        .expect("a valid state")
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
            (Status::Active(Build | Watch), Event::ResultRefused) => Expect::To(status),
            // With every count at zero, nothing is at a bound.
            (
                Status::Active(Watch),
                Event::FixPushed
                | Event::RebasePushed
                | Event::Corrected
                | Event::ChecksGreen
                | Event::FixDue
                | Event::RebaseDue,
            ) => Expect::To(status),
            (Status::Active(Watch), Event::ForgeBlocked) => {
                Expect::Parked(parked(Watch, false), ParkReason::Forge)
            }
            (Status::Active(stage), Event::Quarantined) if stage != Ready => {
                Expect::Parked(parked(stage, false), ParkReason::IsolationBreach)
            }

            (Status::NeedsInput { return_to }, Event::Answered) => {
                Expect::To(Status::Active(return_to))
            }
            (
                Status::NeedsInput { .. },
                Event::Incomplete
                | Event::CounterQuestion
                | Event::RunFailed
                | Event::ResultRefused
                | Event::Interrupted,
            ) => Expect::To(status),
            (Status::NeedsInput { return_to }, Event::Quarantined) => {
                Expect::Parked(parked(return_to, true), ParkReason::IsolationBreach)
            }
            (
                Status::NeedsInput { return_to: Watch } | Status::Parked { at: Watch, .. },
                Event::FollowingEnded,
            ) => Expect::To(Status::Active(Watch)),

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
    fn a_refused_result_counts_but_never_spends_the_last_attempt() {
        let build = state(Status::Active(Stage::Build), 1, 0, 0);
        let refused = to(build.apply(STANDARD, Event::ResultRefused));
        assert_eq!(refused, state(Status::Active(Stage::Build), 1, 0, 1));
        // At the last attempt, it leaves the ticket as it is: the next run,
        // told why, gets that attempt.
        assert_eq!(to(refused.apply(STANDARD, Event::ResultRefused)), refused);

        // The answer check's, while waiting for input, the same way
        // (OWL-190), re-asks kept.
        let waiting = Status::NeedsInput {
            return_to: Stage::Build,
        };
        let refused = to(state(waiting, 1, 2, 0).apply(STANDARD, Event::ResultRefused));
        assert_eq!(refused, state(waiting, 1, 2, 1));
        assert_eq!(to(refused.apply(STANDARD, Event::ResultRefused)), refused);
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
    fn answer_check_classes_fold_into_one_event_with_counter_questions_first() {
        use AnswerClass::{Answered as A, CounterQuestion as C, Partial as P, Unanswered as U};
        // Every non-empty combination of the four classes, written out.
        let table: [(&[AnswerClass], Event); 15] = [
            (&[A], Event::Answered),
            (&[P], Event::Incomplete),
            (&[U], Event::Incomplete),
            (&[C], Event::CounterQuestion),
            (&[A, P], Event::Incomplete),
            (&[A, U], Event::Incomplete),
            (&[P, U], Event::Incomplete),
            (&[A, C], Event::CounterQuestion),
            (&[P, C], Event::CounterQuestion),
            (&[U, C], Event::CounterQuestion),
            (&[A, P, U], Event::Incomplete),
            (&[A, P, C], Event::CounterQuestion),
            (&[A, U, C], Event::CounterQuestion),
            (&[P, U, C], Event::CounterQuestion),
            (&[A, P, U, C], Event::CounterQuestion),
        ];
        let combinations: HashSet<Vec<AnswerClass>> =
            table.iter().map(|(classes, _)| classes.to_vec()).collect();
        assert_eq!(combinations.len(), 15, "the table repeats a combination");

        for (classes, event) in table {
            assert_eq!(
                Event::from_answer_check(classes.iter().copied()),
                Some(event),
                "{classes:?}"
            );
            // Only which classes appear counts, not their order or repetition.
            let shuffled = classes.iter().rev().chain(classes).copied();
            assert_eq!(
                Event::from_answer_check(shuffled),
                Some(event),
                "{classes:?} reversed and repeated"
            );
        }
        assert_eq!(Event::from_answer_check([]), None);
    }

    #[test]
    fn a_counter_question_at_the_reask_limit_waits_and_an_incomplete_answer_then_parks() {
        let limit = state(
            Status::NeedsInput {
                return_to: Stage::Build,
            },
            1,
            MAX_REASKS,
            0,
        );
        let mixed =
            Event::from_answer_check([AnswerClass::Unanswered, AnswerClass::CounterQuestion])
                .expect("an event");
        assert_eq!(to(limit.apply(STANDARD, mixed)), limit);

        let incomplete = Event::from_answer_check([AnswerClass::Answered, AnswerClass::Partial])
            .expect("an event");
        let Ok(Transition::Parked { reason, .. }) = limit.apply(STANDARD, incomplete) else {
            panic!("an incomplete answer at the re-ask limit must park the ticket");
        };
        assert_eq!(reason, ParkReason::Reasks);
    }

    #[test]
    fn watch_counts_fix_passes_and_repairs_up_to_their_bounds() {
        let parked = |reason| {
            move |from: TicketState| {
                Ok::<_, InvalidTransition>(Transition::Parked {
                    state: TicketState {
                        status: Status::Parked {
                            at: Stage::Watch,
                            awaiting_input: false,
                        },
                        ..from
                    },
                    reason,
                })
            }
        };
        let fix_passes = parked(ParkReason::FixPasses);
        let repairs = parked(ParkReason::Repairs);
        let refused = |from: TicketState, event| {
            Err(InvalidTransition {
                from: from.status(),
                event,
            })
        };
        let stays = |state| Ok(Transition::To(state));
        // (from, event, expected), each written out: (failed runs, fix
        // passes, repairs).
        let table = [
            // A push clears the failed runs and counts a repair, a fix also
            // a fix pass.
            (watch(1, 0, 0), Event::FixPushed, stays(watch(0, 1, 1))),
            (watch(1, 1, 4), Event::FixPushed, stays(watch(0, 2, 5))),
            (watch(1, 2, 0), Event::RebasePushed, stays(watch(0, 2, 1))),
            // A correction clears the failed runs and counts nothing, at
            // the bounds too.
            (watch(1, 2, 6), Event::Corrected, stays(watch(0, 2, 6))),
            // Green clears the fix passes and the failed runs, never the
            // repairs.
            (watch(1, 2, 5), Event::ChecksGreen, stays(watch(0, 0, 5))),
            // Below the bounds, a due run changes nothing.
            (watch(1, 1, 5), Event::FixDue, stays(watch(1, 1, 5))),
            (watch(1, 2, 5), Event::RebaseDue, stays(watch(1, 2, 5))),
            // At a bound, it parks, the fix passes first.
            (watch(1, 2, 3), Event::FixDue, fix_passes(watch(1, 2, 3))),
            (watch(0, 2, 6), Event::FixDue, fix_passes(watch(0, 2, 6))),
            (watch(0, 0, 6), Event::FixDue, repairs(watch(0, 0, 6))),
            (watch(0, 0, 6), Event::RebaseDue, repairs(watch(0, 0, 6))),
            (
                watch(1, 2, 6),
                Event::ForgeBlocked,
                parked(ParkReason::Forge)(watch(1, 2, 6)),
            ),
            // A push past a bound is the runner's error: it asks first.
            (
                watch(0, 2, 3),
                Event::FixPushed,
                refused(watch(0, 2, 3), Event::FixPushed),
            ),
            (
                watch(0, 1, 6),
                Event::FixPushed,
                refused(watch(0, 1, 6), Event::FixPushed),
            ),
            (
                watch(0, 0, 6),
                Event::RebasePushed,
                refused(watch(0, 0, 6), Event::RebasePushed),
            ),
            // A refused fix run's result counts as at Build.
            (watch(0, 1, 2), Event::ResultRefused, stays(watch(1, 1, 2))),
            (watch(1, 1, 2), Event::ResultRefused, stays(watch(1, 1, 2))),
        ];
        for (from, event, expected) in table {
            assert_eq!(
                from.apply(STANDARD, event),
                expected,
                "{from:?} + {event:?}"
            );
        }

        // Waiting for input at Watch keeps the counts; leaving Watch, which
        // only review comments do, clears them with the failed runs.
        let repaired = watch(1, 2, 5);
        let waiting = to(repaired.apply(STANDARD, Event::Questions));
        assert_eq!((waiting.fix_passes(), waiting.repairs()), (2, 5));
        assert_eq!(
            to(waiting.apply(STANDARD, Event::Answered)),
            TicketState {
                round: 2,
                ..repaired
            }
        );
        assert_eq!(
            to(repaired.apply(STANDARD, Event::LoopBack)),
            state(Status::Active(Stage::Build), 1, 0, 0)
        );

        // A round the pull request outlives (OWL-242) closes unanswered: back
        // at Watch with the round and the counts, its re-asks cleared.
        let reasked = TicketState {
            reasks: 2,
            ..waiting
        };
        assert_eq!(
            to(reasked.apply(STANDARD, Event::FollowingEnded)),
            TicketState {
                round: 2,
                ..repaired
            }
        );
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

        let Ok(Transition::Parked { state: bounded, .. }) =
            watch(1, MAX_FIX_PASSES, MAX_REPAIRS).apply(STANDARD, Event::FixDue)
        else {
            panic!("a fix due at the bounds must park the ticket");
        };
        assert_eq!(
            to(bounded.apply(STANDARD, Event::Restarted)),
            watch(0, 0, 0)
        );
    }

    /// A park the pull request outlives (OWL-243) ends back at Watch: the
    /// round, the fix passes and the repairs kept, so no bound is reset; the
    /// failed runs and re-asks cleared, whatever the park waited on.
    #[test]
    fn a_park_at_watch_ends_with_its_pull_request_keeping_the_bounds() {
        let parked = |from: TicketState, event| match from.apply(STANDARD, event) {
            Ok(Transition::Parked { state, .. }) => state,
            other => panic!("expected a park, got {other:?}"),
        };
        let bounded = parked(watch(1, MAX_FIX_PASSES, 3), Event::FixDue);
        assert_eq!(
            to(bounded.apply(STANDARD, Event::FollowingEnded)),
            watch(0, MAX_FIX_PASSES, 3)
        );
        let failed = parked(watch(1, 1, 2), Event::RunFailed);
        assert_eq!(failed.failed_runs(), MAX_FAILED_RUNS);
        assert_eq!(
            to(failed.apply(STANDARD, Event::FollowingEnded)),
            watch(0, 1, 2)
        );
        let reasked = TicketState {
            reasks: MAX_REASKS,
            ..to(watch(0, 1, 2).apply(STANDARD, Event::Questions))
        };
        let unanswered = parked(reasked, Event::Incomplete);
        assert_eq!(
            unanswered.status(),
            Status::Parked {
                at: Stage::Watch,
                awaiting_input: true
            }
        );
        assert_eq!(
            to(unanswered.apply(STANDARD, Event::FollowingEnded)),
            TicketState {
                round: 2,
                ..watch(0, 1, 2)
            }
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
        // Every status the table test uses is reachable, and so is each
        // bound: the fix passes', and the repairs' below it, green having
        // cleared the passes and not the repairs.
        let statuses: HashSet<_> = seen.iter().map(TicketState::status).collect();
        assert_eq!(statuses.len(), 29);
        assert!(seen.iter().any(|s| s.fix_passes() == MAX_FIX_PASSES));
        assert!(
            seen.iter()
                .any(|s| s.repairs() == MAX_REPAIRS && s.fix_passes() < MAX_FIX_PASSES)
        );
        for reached in seen {
            let restored = TicketState::restore(
                reached.status(),
                reached.round(),
                reached.reasks(),
                reached.failed_runs(),
                reached.fix_passes(),
                reached.repairs(),
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
        let watching = Status::Active(Stage::Watch);
        let broken = [
            (waiting, 1, MAX_REASKS + 1, 0, 0, 0),
            (build, 1, 1, 0, 0, 0),
            (parked(Stage::Build, false), 1, 1, 0, 0, 0),
            (build, 0, 0, MAX_FAILED_RUNS, 0, 0),
            (waiting, 1, 0, MAX_FAILED_RUNS, 0, 0),
            (parked(Stage::Build, false), 0, 0, MAX_FAILED_RUNS + 1, 0, 0),
            (waiting, 0, 0, 0, 0, 0),
            (parked(Stage::Build, true), 0, 0, 0, 0, 0),
            (
                Status::NeedsInput {
                    return_to: Stage::Intake,
                },
                1,
                0,
                0,
                0,
                0,
            ),
            (parked(Stage::Intake, true), 1, 0, 0, 0, 0),
            (Status::Active(Stage::Intake), 1, 0, 0, 0, 0),
            (parked(Stage::Intake, false), 1, 0, 0, 0, 0),
            (parked(Stage::Ready, false), 0, 0, 0, 0, 0),
            (watching, 1, 0, 0, MAX_FIX_PASSES + 1, MAX_FIX_PASSES + 1),
            (watching, 1, 0, 0, 0, MAX_REPAIRS + 1),
            (build, 1, 0, 0, 1, 0),
            (waiting, 1, 0, 0, 0, 1),
            (parked(Stage::Deliver, false), 1, 0, 0, 0, 1),
        ];
        for (status, round, reasks, failed_runs, fix_passes, repairs) in broken {
            assert!(
                TicketState::restore(status, round, reasks, failed_runs, fix_passes, repairs)
                    .is_err(),
                "{status:?} round {round} re-asks {reasks} failed {failed_runs} \
                 fix passes {fix_passes} repairs {repairs}"
            );
        }
        // The bounds themselves hold at Watch, working, waiting or parked.
        for status in [
            watching,
            Status::NeedsInput {
                return_to: Stage::Watch,
            },
            parked(Stage::Watch, false),
            parked(Stage::Watch, true),
        ] {
            assert!(
                TicketState::restore(status, 1, 0, 0, MAX_FIX_PASSES, MAX_REPAIRS).is_ok(),
                "{status:?}"
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
        let error = TicketState::restore(Status::Active(Stage::Build), 0, 1, 0, 0, 0).unwrap_err();
        assert!(
            error
                .to_string()
                .ends_with("re-asks while not waiting for input")
        );
        let error = TicketState::restore(Status::Active(Stage::Verify), 1, 0, 0, 1, 2).unwrap_err();
        assert_eq!(
            error.to_string(),
            "invalid ticket state Active(Verify) (round 1, re-asks 0, failed runs 0, fix passes \
             1, repairs 2): fix passes or repairs outside watch"
        );
    }
}
