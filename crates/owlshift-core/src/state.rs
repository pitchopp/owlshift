//! The ticket state machine: where a ticket stands in its pipeline, and what
//! each event does to it.
//!
//! A ticket is working a stage, waiting for its decider's input, or parked
//! until a human restarts it. It leaves the machine when its pull request is
//! merged, by a human or, on a project that opted in, by the runner's merge
//! on green (D14), or when it is withdrawn. [`TicketState::apply`] is the only way to
//! move: an event that does not fit the current state is an error, never
//! silently ignored.

use std::fmt;

use crate::gate::PlanGate;
use crate::pipeline::Pipeline;
use crate::vocab::{AnswerClass, Stage};

/// Failed runs in one stage that park the ticket: the second one does. A
/// refused Build, design, review or answer-check result never brings the count there
/// ([`Event::ResultRefused`]), and a gate refused for uncommitted changes
/// before any command does not count ([`Event::UncommittedGate`]), nor does
/// a gate or setup failure the base reproduces ([`Event::BaseBroken`]).
pub const MAX_FAILED_RUNS: u32 = 2;

/// Re-asks of one question round before the ticket parks: an answer still
/// incomplete after the third re-ask parks it instead of a fourth.
pub const MAX_REASKS: u32 = 3;

/// Runs in a row whose questions the resolver all decided, so that the stage
/// ran again without a stop, before the questions of the next run go to the
/// decider without the resolver: a stage that keeps asking what the
/// repository settles cannot loop without bound (architecture, section 4).
/// The default of the project's `policy.resolver_passes` since OWL-328
/// ([`crate::gate::PassLimit`]), 3 before.
pub const MAX_RESOLVED_PASSES: u32 = 5;

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

/// Review passes at Design review or at Verify before what they leave goes
/// to one last run of its author: S9's bound (architecture, section 5, "The
/// quality pipeline"). A confirmed finding of a pass before the last sends
/// the ticket back to its author for another pass; what the last pass
/// leaves goes to its author once more, Design's last revision or Verify's
/// last fix, with no pass after it, and is listed, never asked (OWL-377,
/// [`crate::review::settle`]).
pub const MAX_REVIEW_PASSES: u32 = 2;

/// Whether review pass number `pass` (from 1, as `review.json` numbers
/// them) is a last one: the [`MAX_REVIEW_PASSES`]th, and every pass after
/// it, each of which follows an answered round. No [`Event::LoopBack`]
/// follows a last pass.
pub const fn is_last_review_pass(pass: u32) -> bool {
    pass >= MAX_REVIEW_PASSES
}

/// The review stage of the loop `stage` belongs to: Design review for
/// Design and Design review, Verify for Build and Verify; `None` elsewhere.
const fn review_loop(stage: Stage) -> Option<Stage> {
    match stage {
        Stage::Design | Stage::DesignReview => Some(Stage::DesignReview),
        Stage::Build | Stage::Verify => Some(Stage::Verify),
        Stage::Intake | Stage::Ready | Stage::Deliver | Stage::Watch => None,
    }
}

/// Where a ticket stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Status {
    /// Working a stage: its role is due or running. At Ready, the ticket waits
    /// for the scheduler to dispatch it.
    Active(Stage),
    /// Questions wait for the decider. Once they are answered the ticket
    /// resumes at `return_to`, the round's return stage
    /// ([`Pipeline::round_return`]): the stage that asked, but Design for a
    /// round asked at Design review and Build for one asked at Verify, and
    /// Design for a Build's round carrying a `replan` ([`Pipeline::replan`],
    /// OWL-301). At Intake, intake runs again with the answers (OWL-269). A
    /// round asked at Intake before OWL-269 returned to Ready, which refs
    /// written then still keep.
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
/// | A stage run returned `done`, and the stage passed; at Design review, with the plan gate open ([`TicketState::apply_gated`]) | `Completed` |
/// | A stage run returned `done` asking for a revision or a fix; a review pass before the last left confirmed findings ([`crate::review::settle`]); at Watch, review comments on the pull request (from P5) | `LoopBack` |
/// | A stage run returned `questions` or `premise_false` (after the resolver); a Design review pass passed with the plan gate shut, the approval round (OWL-32) | `Questions` |
/// | A Build run asked a `replan`, and a round opens for its questions, the `replan` asked or decided (after the resolver) | `ReplanRound` |
/// | A Build run asked a `replan`, and the resolver decided every question of the run | `ReplanDecided` |
/// | The resolver decided every question a Design review or Verify pass left for the decider, its findings or the approval (OWL-342); since OWL-377 the approval only | `RoundDecided` |
/// | A Verify pass left findings for one last Build, `minor` ones at any pass (OWL-356) or any at a last pass; a last Design review pass left `blocker` or `major` findings for Design's last revision (OWL-377) ([`crate::review::PassOutcome::LastFix`]) | `LastFix` |
/// | Before delivery, the decider reversed a resolver's plan approval (OWL-345) | `ReversedToDesign` |
/// | A design run answered an approval round `approved`, and the runner kept the approval | `PlanApproved` |
/// | A stage run returned `blocked` | `Blocked` |
/// | The answer check found every question answered | `Answered` |
/// | The answer check found questions left open (they are re-asked) | `Incomplete` |
/// | The decider asked a counter-question (answered in the thread) | `CounterQuestion` |
/// | A run returned `failed` or no valid `result.json`, whatever its exit code, save the row below; a Build `done` whose project gate, run by the runner, failed | `RunFailed` |
/// | A Build run's (at Build, or a fix run's at Watch), a design run's, a review seat's (at Design review or Verify, OWL-430) or an answer check's `result.json` was refused, and the run had not been told of a refused result: the next run of its kind will be | `ResultRefused` |
/// | A Build run's (at Build, or a fix run's at Watch) gate was refused for uncommitted changes before any command ran, and the run had not been told of such a refusal: the next run will be | `UncommittedGate` |
/// | A Build run's at Build (a Verify fix run included) gate or setup failed on a command that the commit it was configured at fails too, or that commit's check is due; or a Build run at Build was held before it started for such a commit (OWL-418, [`crate::base`]) | `BaseBroken` |
/// | A rebase run off a held commit, at Build, ended without a rebase the runner accepts (OWL-427, [`crate::base::leave`]) | `RebaseFailed` |
/// | A run was cut off by a harness usage limit; it resumes after the reset | `Interrupted` |
/// | A run broke isolation (main checkout touched, diff outside the worktree, wrong branch) | `Quarantined` |
/// | A run is due while the ticket's runs since its last restart cost its budget or more (`policy.ticket_budget_usd`, OWL-378): none starts | `BudgetReached` |
/// | At Watch, a fix run's push landed on the pull request | `FixPushed` |
/// | At Watch, a rebase run's push landed on the pull request | `RebasePushed` |
/// | At Watch, a correction run's push landed, or it found the pull request already following the decider's replies | `Corrected` |
/// | At Watch, the pull request was read green | `ChecksGreen` |
/// | At Watch, a read calls for a fix run, before it runs | `FixDue` |
/// | At Watch, a read calls for a rebase run, before it runs | `RebaseDue` |
/// | At Watch, a red no run fixes, or a push the forge refused | `ForgeBlocked` |
/// | While a round waits at Watch, or the ticket is parked there, its pull request was merged, closed, or pushed to by someone else; likewise while a review loop's ticket is at Build or Verify, working, waiting or parked there (OWL-307) | `FollowingEnded` |
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
    /// A stage loops back ([`Pipeline::loop_back`]). From Design review or
    /// Verify it ends a review pass, counted, and is refused after a last
    /// pass ([`is_last_review_pass`]).
    LoopBack,
    /// A question round opens; it returns to [`Pipeline::round_return`].
    /// At Design review or Verify it ends a review pass, counted.
    Questions,
    /// A round opens for a Build run that asked a `replan` (OWL-301): it
    /// returns to [`Pipeline::replan`], Design, whose new plan is reviewed
    /// again, or Build on `trivial`. Valid only at Build: a fix run's
    /// `replan` at Watch is a plain question.
    ReplanRound,
    /// The resolver settled a Build run's `replan` with the run's other
    /// questions, so no round opens: the ticket goes to
    /// [`Pipeline::replan`] at once. Valid only at Build.
    ReplanDecided,
    /// The resolver decided every question a review pass at Design review
    /// or Verify left for the decider, so no round opens (OWL-342): the
    /// ticket goes where the round would have returned
    /// ([`Pipeline::round_return`]), Design or Build, whose next run follows
    /// the decisions, and the pass is ended and counted as
    /// [`Event::Questions`] ends it. Like an answered round, it is valid at
    /// a last pass, which it buys one more pass after; the resolver's pass
    /// limit or dollar ceiling bounds such rounds in a row. Valid only at
    /// Design review and Verify.
    RoundDecided,
    /// A review pass left findings for one last run of their author, with
    /// no review pass after it: at Verify, the last fix of OWL-356, to
    /// Build, `minor` findings at any pass and every finding at a last
    /// pass; at Design review, Design's last revision of OWL-377, the
    /// `blocker` and `major` findings of a last pass. The pass is ended and
    /// counted as [`Event::LoopBack`] ends one; unlike a loop back, it is
    /// valid at a last pass. No review pass follows that run, which the
    /// runner reads from the review record (its mark of the last fix, and
    /// at Design review the pass it keeps for the revision), dropped when
    /// anything else reaches the run. Valid only at Design review and
    /// Verify.
    LastFix,
    /// The decider reversed, by replying to its DECISION comment, a decision
    /// the resolver took on the plan's approval (OWL-345), before delivery:
    /// the ticket goes back to Design, whose new plan is reviewed and
    /// approved again. From Design review the review pass count is kept, as
    /// an answered round keeps it; from Build or Verify it restarts, as a
    /// `replan` leaves it. No pass is counted. Valid at Design review, Build and Verify, and only when the
    /// pipeline has a Design, so it is refused on `trivial`.
    ReversedToDesign,
    /// A design run answered an approval round `approved` (OWL-32): the
    /// plan stays as it was and the ticket goes to Build. Valid only at
    /// Design after a review pass of the loop, which the approval round's
    /// [`Event::Questions`] at Design review always ends, and only with a
    /// plan gate that required the approval and holds it
    /// ([`TicketState::apply_gated`]); so a Design back from a `replan`, or
    /// one revising its plan, never skips the review.
    PlanApproved,
    Blocked,
    Answered,
    Incomplete,
    CounterQuestion,
    RunFailed,
    /// A failed run that never spends the stage's last attempt (OWL-183):
    /// it counts as [`Event::RunFailed`] below the last attempt, and at the
    /// last attempt leaves the ticket as it is, so the next run of its
    /// kind, told why the result was refused, gets that attempt. Only the
    /// briefs of the runs retried within their stage and the answer
    /// check's carry that reason, so it is valid at Build, at Watch, where a
    /// fix run is the build role's, at Design, Design review and Verify,
    /// where the design role and the review seats run (OWL-430), and while
    /// waiting for input, where the answer check runs (OWL-190). The bound is the runner's to keep: it gives this
    /// event only for a run that was not itself told of a refused result,
    /// so that of two refusals in a row, the second is a `RunFailed`.
    ResultRefused,
    /// A gate refused for uncommitted changes before any command ran
    /// (OWL-421): the ticket stays as it is, its count kept, so a refusal
    /// that ran nothing never spends the stage's fix run. Only a Build
    /// role's run reaches the gate, so it is valid at Build and at Watch,
    /// where a fix run is the build role's. The bound is the runner's, as
    /// for [`Event::ResultRefused`]: it gives this event only for a run that
    /// was not itself told of such a refusal, so that of two in a row, the
    /// second is a `RunFailed`.
    UncommittedGate,
    /// A Build run's gate or setup failure that the base reproduces, or a
    /// Build held before it started because its base does (OWL-418): the
    /// ticket stays as it is, its counts kept, and its Build waits for that
    /// base to pass. Only a Build at Build is held, so it is valid only
    /// there: a fix run at Watch takes the failed-run path.
    BaseBroken,
    /// A rebase run that was to move a held Build's branch off its broken
    /// base onto a newer head that passes (OWL-427) ended without a rebase
    /// the runner accepts: the ticket parks at once
    /// ([`ParkReason::Rebase`]), its counts kept, since the Build's
    /// worktree is never made anew for a second attempt. Valid only at
    /// Build, where a Build is held.
    RebaseFailed,
    Interrupted,
    Quarantined,
    /// A run was due, but what the ticket's runs cost since its last
    /// restart reached its budget (OWL-378): no run starts and the ticket
    /// parks where it stands, waiting for input if it was. Valid wherever a
    /// run can be in flight.
    BudgetReached,
    /// A person canceled the ticket's work with `owlshift cancel` or the
    /// UI's Cancel (OWL-474, architecture section 5, "Cancel"): its step
    /// under way, if any, was stopped and checked, and the ticket parks
    /// where it stands ([`ParkReason::Stopped`]), waiting for input if it
    /// was. Not [`Event::Withdrawn`], which ends the ticket: a person's
    /// `continue` restarts it. Valid wherever a run can be in flight, so a
    /// ticket at Ready, the admission's, is never parked by it.
    Canceled,
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
    /// runs after it, and no bound is reset. Since OWL-307 it also ends a
    /// review loop, which took a delivered ticket back to Build on reviews of
    /// its pull request: from Build or Verify, working, waiting for input
    /// (whose round returns to Build) or parked there, the ticket is back at
    /// Watch as above, its review passes reset by leaving their loop. The
    /// state does not know whether a delivery is followed: the runner gives
    /// it there only for a ticket whose delivery watch follows. Valid only
    /// while waiting with Watch or Build as the return stage, parked at
    /// Watch, Build or Verify, or working at Build or Verify.
    FollowingEnded,
    Restarted,
    Merged,
    Withdrawn,
}

impl Event {
    /// Every event.
    pub const ALL: [Self; 34] = [
        Self::Dispatched,
        Self::Completed,
        Self::LoopBack,
        Self::Questions,
        Self::ReplanRound,
        Self::ReplanDecided,
        Self::RoundDecided,
        Self::LastFix,
        Self::ReversedToDesign,
        Self::PlanApproved,
        Self::Blocked,
        Self::Answered,
        Self::Incomplete,
        Self::CounterQuestion,
        Self::RunFailed,
        Self::ResultRefused,
        Self::UncommittedGate,
        Self::BaseBroken,
        Self::RebaseFailed,
        Self::Interrupted,
        Self::Quarantined,
        Self::BudgetReached,
        Self::Canceled,
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
    /// A run was due, but the ticket's runs since its last restart cost its
    /// budget or more (`policy.ticket_budget_usd`, OWL-378).
    TicketBudget,
    /// A held Build's branch could not be rebased off its broken base onto
    /// a newer head that passes (OWL-427).
    Rebase,
    /// A person canceled the ticket's work ([`Event::Canceled`], OWL-474):
    /// its step under way was stopped and its runs checked.
    Stopped,
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
    review_passes: u32,
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
            review_passes: 0,
        }
    }

    /// Rebuilds a state from its persisted parts, refusing one that breaks an
    /// invariant of the machine:
    ///
    /// - at most [`MAX_REASKS`] re-asks, and re-asks only while waiting for
    ///   input;
    /// - fewer than [`MAX_FAILED_RUNS`] failed runs unless parked, and never
    ///   more;
    /// - waiting for input means a question round was asked; a round asked
    ///   at Intake returns to Intake (OWL-269), and one that returns to Ready,
    ///   which a ref written before OWL-269 keeps, is still restored, though
    ///   the machine no longer reaches it;
    /// - a ticket at Intake may count rounds: those it asked before it was
    ///   taken in again, its text edited, and those answered there;
    /// - nothing runs at Ready, so nothing is parked there unless it was
    ///   waiting for input;
    /// - at most [`MAX_FIX_PASSES`] fix passes and [`MAX_REPAIRS`] repairs,
    ///   and either only at Watch.
    ///
    /// The state has no review pass; [`TicketState::with_review_passes`]
    /// gives it those a ticket ref keeps, with their own rule.
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
            review_passes: 0,
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

    /// The review passes ended in the current review loop (`state.json`
    /// format 4, OWL-300, OWL-301): Design and Design review, or Build and
    /// Verify. A [`Event::LoopBack`] or [`Event::Questions`] from Design
    /// review or Verify ends one; moving within the loop, a round, a park
    /// and a restart keep the count; entering a stage of another loop or
    /// of none resets it (Design review to Build, Verify to Deliver, a
    /// `replan` from Build to Design). Rounds being unlimited, so is the
    /// count: each answered round at a last pass buys one more pass.
    pub const fn review_passes(&self) -> u32 {
        self.review_passes
    }

    /// The number of the review pass due at the current review loop, from
    /// 1, as `review.json` numbers them: [`TicketState::review_passes`]
    /// plus one, saturating. [`is_last_review_pass`] tells whether it is a
    /// last one.
    pub const fn review_pass_due(&self) -> u32 {
        self.review_passes.saturating_add(1)
    }

    /// This state with `review_passes` review passes, as a ticket ref keeps
    /// them; [`TicketState::restore`] rebuilds the other parts. Refused
    /// above 0 outside a review loop's stages (Design, Design review, Build
    /// and Verify, working, waiting or parked there), since leaving the
    /// loop resets the count.
    pub fn with_review_passes(self, review_passes: u32) -> Result<Self, InvalidState> {
        let state = Self {
            review_passes,
            ..self
        };
        if review_passes > 0 && review_loop(self.stage()).is_none() {
            return Err(InvalidState {
                state,
                reason: "review passes outside design, design review, build and verify",
            });
        }
        Ok(state)
    }

    /// Applies an event to this state, in the ticket's pipeline.
    ///
    /// A human merge or withdrawal ends the ticket from any state. Every other
    /// event is valid only in some states (see [`Event`]); elsewhere, and for a
    /// question round past `u32::MAX`, it is an [`InvalidTransition`].
    ///
    /// Entering Build from Design or Design review takes the plan gate, in
    /// a pipeline with Design review: there it is refused here and made by
    /// [`TicketState::apply_gated`]. A `trivial` pipeline has no plan to
    /// approve, so a ticket whose variant changed while it was at Design
    /// moves on without one; [`Event::PlanApproved`] always takes it.
    pub fn apply(&self, pipeline: Pipeline, event: Event) -> Result<Transition, InvalidTransition> {
        self.step(pipeline, event, None)
    }

    /// Applies an event as [`TicketState::apply`] does, with the plan gate
    /// of the ticket's current plan ([`crate::gate::GatePolicy::plan_gate`],
    /// OWL-32). The gate bears only on entering Build from Design or Design
    /// review: [`Event::Completed`] at Design review needs it open, and
    /// [`Event::PlanApproved`] needs it to require the approval and hold
    /// it. So when approval is required, Build is never entered on a plan
    /// whose hash is not the approved one. Every other move is
    /// [`TicketState::apply`]'s.
    pub fn apply_gated(
        &self,
        pipeline: Pipeline,
        event: Event,
        gate: PlanGate,
    ) -> Result<Transition, InvalidTransition> {
        self.step(pipeline, event, Some(gate))
    }

    fn step(
        &self,
        pipeline: Pipeline,
        event: Event,
        gate: Option<PlanGate>,
    ) -> Result<Transition, InvalidTransition> {
        let invalid = InvalidTransition {
            from: self.status,
            event,
        };
        let transition = self.transition(pipeline, event, invalid)?;
        let from_design = matches!(
            self.status,
            Status::Active(Stage::Design | Stage::DesignReview)
        );
        let to_build = matches!(
            transition,
            Transition::To(Self {
                status: Status::Active(Stage::Build),
                ..
            })
        );
        if from_design && to_build {
            let passes = match (event, gate) {
                (Event::PlanApproved, gate) => gate.is_some_and(|g| g.required() && g.approved()),
                (_, Some(gate)) => gate.is_open(),
                (_, None) => !pipeline.contains(Stage::DesignReview),
            };
            if !passes {
                return Err(invalid);
            }
        }
        Ok(transition)
    }

    /// The transition table, before the plan gate.
    fn transition(
        &self,
        pipeline: Pipeline,
        event: Event,
        invalid: InvalidTransition,
    ) -> Result<Transition, InvalidTransition> {
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
                if review_loop(stage) == Some(stage) && is_last_review_pass(self.review_pass_due())
                {
                    return Err(invalid);
                }
                let entered = self.enter(Status::Active(target));
                Transition::To(self.end_review_pass(stage, entered).ok_or(invalid)?)
            }
            // A round returns to the stage that asked, Intake's included:
            // intake runs again with the answers (OWL-269); a review stage's
            // returns to its author.
            (Status::Active(stage), Event::Questions) if stage != Stage::Ready => {
                let round = self.round.checked_add(1).ok_or(invalid)?;
                let entered = Self {
                    round,
                    reasks: 0,
                    ..self.enter(Status::NeedsInput {
                        return_to: pipeline.round_return(stage),
                    })
                };
                Transition::To(self.end_review_pass(stage, entered).ok_or(invalid)?)
            }
            (Status::Active(Stage::Build), Event::ReplanRound) => {
                let round = self.round.checked_add(1).ok_or(invalid)?;
                Transition::To(Self {
                    round,
                    reasks: 0,
                    ..self.enter(Status::NeedsInput {
                        return_to: pipeline.replan(),
                    })
                })
            }
            (Status::Active(Stage::Build), Event::ReplanDecided) => {
                Transition::To(self.enter(Status::Active(pipeline.replan())))
            }
            // A review round the resolver decided whole (OWL-342): where the
            // round would have returned, the pass counted, no round opened.
            (
                Status::Active(stage @ (Stage::DesignReview | Stage::Verify)),
                Event::RoundDecided,
            ) => {
                let entered = self.enter(Status::Active(pipeline.round_return(stage)));
                Transition::To(self.end_review_pass(stage, entered).ok_or(invalid)?)
            }
            // One last run of the author, the pass counted, at a last pass
            // too: Verify's last fix (OWL-356), Design's last revision
            // (OWL-377).
            (Status::Active(stage @ (Stage::DesignReview | Stage::Verify)), Event::LastFix) => {
                let target = pipeline.loop_back(stage).ok_or(invalid)?;
                let entered = self.enter(Status::Active(target));
                Transition::To(self.end_review_pass(stage, entered).ok_or(invalid)?)
            }
            // A decider's reply reversed a resolver's approval before
            // delivery (OWL-345): back to Design, no pass counted, no round
            // opened.
            (
                Status::Active(Stage::DesignReview | Stage::Build | Stage::Verify),
                Event::ReversedToDesign,
            ) if pipeline.contains(Stage::Design) => {
                Transition::To(self.enter(Status::Active(Stage::Design)))
            }
            // The plan gate is checked on top ([`TicketState::step`]).
            (Status::Active(Stage::Design), Event::PlanApproved) if self.review_passes > 0 => {
                Transition::To(self.enter(Status::Active(Stage::Build)))
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
            // A review loop the pull request outlived (OWL-307): back at
            // Watch, its round closed or its park ended as there.
            (
                Status::Active(Stage::Build | Stage::Verify)
                | Status::NeedsInput {
                    return_to: Stage::Build,
                }
                | Status::Parked {
                    at: Stage::Build | Stage::Verify,
                    ..
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
                Status::Active(
                    Stage::Build
                    | Stage::Watch
                    | Stage::Design
                    | Stage::DesignReview
                    | Stage::Verify,
                )
                | Status::NeedsInput { .. },
                Event::ResultRefused,
            ) => {
                let failed_runs = (self.failed_runs + 1).min(MAX_FAILED_RUNS - 1);
                Transition::To(Self {
                    failed_runs,
                    ..*self
                })
            }
            // Neither a usage limit nor a gate refused before any command is
            // a failure: the run resumes after the reset, or runs again told.
            (status, Event::Interrupted) if status.runs() => Transition::To(*self),
            (Status::Active(Stage::Build | Stage::Watch), Event::UncommittedGate) => {
                Transition::To(*self)
            }
            // A base that breaks the gate is the project's, not the ticket's.
            (Status::Active(Stage::Build), Event::BaseBroken) => Transition::To(*self),
            (Status::Active(Stage::Build), Event::RebaseFailed) => {
                self.park(ParkReason::Rebase, self.failed_runs)
            }
            (status, Event::Quarantined) if status.runs() => {
                self.park(ParkReason::IsolationBreach, self.failed_runs)
            }
            (status, Event::BudgetReached) if status.runs() => {
                self.park(ParkReason::TicketBudget, self.failed_runs)
            }
            (status, Event::Canceled) if status.runs() => {
                self.park(ParkReason::Stopped, self.failed_runs)
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
                    review_passes: self.review_passes,
                })
            }

            _ => return Err(invalid),
        };
        Ok(transition)
    }

    /// This state moved to `status`. The failed-run count restarts when the
    /// stage changes, and so do the fix passes and repairs, which count
    /// only at Watch: leaving it, which only review comments do (P5), ends
    /// the delivery they counted. The review passes are kept within a
    /// review loop and restart when entering a stage of another loop or of
    /// none.
    fn enter(&self, status: Status) -> Self {
        if status.stage() == self.stage() {
            return Self { status, ..*self };
        }
        let to = review_loop(status.stage());
        let review_passes = if to.is_some() && to == review_loop(self.stage()) {
            self.review_passes
        } else {
            0
        };
        Self {
            status,
            failed_runs: 0,
            fix_passes: 0,
            repairs: 0,
            review_passes,
            ..*self
        }
    }

    /// `entered`, the state an event leaving `stage` moved to, with the
    /// review pass that event ended counted when `stage` is Design review or
    /// Verify; `None` past `u32::MAX`.
    fn end_review_pass(&self, stage: Stage, entered: Self) -> Option<Self> {
        if review_loop(stage) != Some(stage) {
            return Some(entered);
        }
        Some(Self {
            review_passes: self.review_passes.checked_add(1)?,
            ..entered
        })
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
            review_passes,
        } = self.state;
        write!(
            f,
            "invalid ticket state {status:?} (round {round}, re-asks {reasks}, failed runs \
             {failed_runs}, fix passes {fix_passes}, repairs {repairs}, review passes \
             {review_passes}): {}",
            self.reason
        )
    }
}

impl std::error::Error for InvalidState {}

#[cfg(test)]
mod tests {
    use std::collections::{HashSet, VecDeque};

    use super::*;
    use crate::gate::GatePolicy;
    use crate::vocab::{PlanApproval, Variant};

    const STANDARD: Pipeline = Pipeline::new(Variant::Standard);
    const RISKY: Pipeline = Pipeline::new(Variant::Risky);

    /// A plan's hash, and another's.
    const PLAN: &str = "plan";
    const OTHER_PLAN: &str = "other plan";

    /// A gate requiring no approval: a standard plan under `never`.
    fn not_required() -> PlanGate {
        GatePolicy::new(Vec::<String>::new(), PlanApproval::Never)
            .plan_gate(STANDARD, true, PLAN, None)
    }

    /// A risky plan's gate, with the approval of `kept`, if any.
    fn risky_gate(kept: Option<&str>) -> PlanGate {
        GatePolicy::new(Vec::<String>::new(), PlanApproval::Never)
            .plan_gate(RISKY, false, PLAN, kept)
    }

    /// Every state reachable from admission in `pipeline`, each event
    /// applied without a gate and with each of `gates`, with question rounds
    /// bounded at three and review passes at two past the last one: past
    /// that, only the round or pass counter grows, a review round the
    /// resolver decides counting a pass without a round (OWL-342).
    fn reachable(pipeline: Pipeline, gates: &[PlanGate]) -> HashSet<TicketState> {
        let mut seen = HashSet::from([TicketState::admitted()]);
        let mut queue = VecDeque::from([TicketState::admitted()]);
        while let Some(current) = queue.pop_front() {
            for event in Event::ALL {
                let gated = gates
                    .iter()
                    .map(|&gate| current.apply_gated(pipeline, event, gate));
                for transition in std::iter::once(current.apply(pipeline, event)).chain(gated) {
                    let next = match transition {
                        Ok(Transition::To(next) | Transition::Parked { state: next, .. }) => next,
                        Ok(Transition::Finished(_)) | Err(_) => continue,
                    };
                    if next.round() <= 3
                        && next.review_passes() <= MAX_REVIEW_PASSES + 2
                        && seen.insert(next)
                    {
                        queue.push_back(next);
                    }
                }
            }
        }
        seen
    }

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
        let round = u32::from(status.stage() != Stage::Intake || status.awaits_input());
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
            // Design review to Build takes the plan gate (OWL-32), which
            // `apply` has not: see `the_design_loop_enters_build_only_through_the_plan_gate`.
            (Status::Active(Build), Event::Completed) => Expect::To(Status::Active(Verify)),
            (Status::Active(Verify), Event::Completed) => Expect::To(Status::Active(Deliver)),
            (Status::Active(Deliver), Event::Completed) => Expect::To(Status::Active(Watch)),

            (Status::Active(DesignReview), Event::LoopBack) => Expect::To(Status::Active(Design)),
            (Status::Active(Verify), Event::LoopBack) => Expect::To(Status::Active(Build)),
            (Status::Active(Watch), Event::LoopBack) => Expect::To(Status::Active(Build)),

            (Status::Active(DesignReview), Event::Questions) => {
                Expect::To(Status::NeedsInput { return_to: Design })
            }
            (Status::Active(Verify), Event::Questions) => {
                Expect::To(Status::NeedsInput { return_to: Build })
            }
            (Status::Active(stage), Event::Questions) if stage != Ready => {
                Expect::To(Status::NeedsInput { return_to: stage })
            }
            (Status::Active(Build), Event::ReplanRound) => {
                Expect::To(Status::NeedsInput { return_to: Design })
            }
            (Status::Active(Build), Event::ReplanDecided) => Expect::To(Status::Active(Design)),
            (Status::Active(DesignReview), Event::RoundDecided | Event::LastFix) => {
                Expect::To(Status::Active(Design))
            }
            (Status::Active(Verify), Event::RoundDecided | Event::LastFix) => {
                Expect::To(Status::Active(Build))
            }
            (Status::Active(DesignReview | Build | Verify), Event::ReversedToDesign) => {
                Expect::To(Status::Active(Design))
            }
            (Status::Active(stage), Event::Blocked) if stage != Ready => {
                Expect::Parked(parked(stage, false), ParkReason::Blocked)
            }
            (Status::Active(stage), Event::RunFailed | Event::Interrupted) if stage != Ready => {
                Expect::To(status)
            }
            (Status::Active(Build | Watch), Event::ResultRefused | Event::UncommittedGate) => {
                Expect::To(status)
            }
            (Status::Active(Design | DesignReview | Verify), Event::ResultRefused) => {
                Expect::To(status)
            }
            (Status::Active(Build), Event::BaseBroken) => Expect::To(status),
            (Status::Active(Build), Event::RebaseFailed) => {
                Expect::Parked(parked(Build, false), ParkReason::Rebase)
            }
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
            (Status::Active(stage), Event::BudgetReached) if stage != Ready => {
                Expect::Parked(parked(stage, false), ParkReason::TicketBudget)
            }
            (Status::Active(stage), Event::Canceled) if stage != Ready => {
                Expect::Parked(parked(stage, false), ParkReason::Stopped)
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
            (Status::NeedsInput { return_to }, Event::BudgetReached) => {
                Expect::Parked(parked(return_to, true), ParkReason::TicketBudget)
            }
            (Status::NeedsInput { return_to }, Event::Canceled) => {
                Expect::Parked(parked(return_to, true), ParkReason::Stopped)
            }
            (
                Status::NeedsInput { return_to: Watch } | Status::Parked { at: Watch, .. },
                Event::FollowingEnded,
            ) => Expect::To(Status::Active(Watch)),
            // A review loop's ticket (OWL-307).
            (
                Status::Active(Build | Verify)
                | Status::NeedsInput { return_to: Build }
                | Status::Parked {
                    at: Build | Verify, ..
                },
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
        // 8 active, 8 needs-input, 7 parked at a stage that ran (never
        // Ready), 8 parked while waiting for input: those returning to Ready
        // are a round asked at Intake before OWL-269, still restored.
        assert_eq!(states.len(), 31);
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

    /// An intake round returns to Intake, so intake runs again with the
    /// answers (OWL-269); the stage is the same throughout, so a failed run
    /// before the round still counts after it, as at Build.
    #[test]
    fn questions_open_a_new_round_and_intake_questions_return_to_intake() {
        let intake = state(Status::Active(Stage::Intake), 0, 0, 1);
        let waiting = to(intake.apply(STANDARD, Event::Questions));
        assert_eq!(
            waiting.status(),
            Status::NeedsInput {
                return_to: Stage::Intake
            }
        );
        assert_eq!(waiting.round(), 1);
        let answered = to(waiting.apply(STANDARD, Event::Answered));
        assert_eq!(answered, state(Status::Active(Stage::Intake), 1, 0, 1));
        assert_eq!(
            to(answered.apply(STANDARD, Event::Completed)),
            state(Status::Active(Stage::Ready), 1, 0, 0)
        );

        let build = state(Status::Active(Stage::Build), 1, 0, 1);
        let waiting = to(build.apply(STANDARD, Event::Questions));
        assert_eq!(waiting.round(), 2);
        assert_eq!(waiting.reasks(), 0);
        // Same stage: the earlier failure still counts.
        assert_eq!(waiting.failed_runs(), 1);
    }

    /// A ticket taken in again after its text changed is back at Intake with
    /// the rounds it already asked (architecture, section 5, "Intake").
    #[test]
    fn a_ticket_taken_in_again_keeps_its_rounds() {
        let again = state(Status::Active(Stage::Intake), 2, 0, 0);
        let waiting = to(again.apply(STANDARD, Event::Questions));
        assert_eq!(
            waiting.status(),
            Status::NeedsInput {
                return_to: Stage::Intake
            }
        );
        assert_eq!(waiting.round(), 3);
        assert_eq!(
            to(again.apply(STANDARD, Event::Completed)),
            state(Status::Active(Stage::Ready), 2, 0, 0)
        );

        let parked = state(
            Status::Parked {
                at: Stage::Intake,
                awaiting_input: false,
            },
            2,
            0,
            MAX_FAILED_RUNS,
        );
        assert_eq!(to(parked.apply(STANDARD, Event::Restarted)), again);
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
        // Build's, and since OWL-430 the design role's and the review
        // seats'.
        for stage in [
            Stage::Build,
            Stage::Design,
            Stage::DesignReview,
            Stage::Verify,
        ] {
            let at = state(Status::Active(stage), 1, 0, 0);
            let refused = to(at.apply(STANDARD, Event::ResultRefused));
            assert_eq!(refused, state(Status::Active(stage), 1, 0, 1), "{stage:?}");
            // At the last attempt, it leaves the ticket as it is: the next
            // run, told why, gets that attempt.
            assert_eq!(to(refused.apply(STANDARD, Event::ResultRefused)), refused);
            // A told run's failure is a plain failed run, which parks.
            assert!(matches!(
                refused.apply(STANDARD, Event::RunFailed),
                Ok(Transition::Parked {
                    reason: ParkReason::FailedRuns,
                    ..
                })
            ));
        }

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
    fn an_uncommitted_gate_never_counts() {
        for failed_runs in 0..MAX_FAILED_RUNS {
            let build = state(Status::Active(Stage::Build), 1, 0, failed_runs);
            assert_eq!(to(build.apply(STANDARD, Event::UncommittedGate)), build);
        }
        // Not where no gate runs.
        let waiting = state(
            Status::NeedsInput {
                return_to: Stage::Build,
            },
            1,
            0,
            0,
        );
        assert!(waiting.apply(STANDARD, Event::UncommittedGate).is_err());
    }

    /// OWL-418: a broken base never counts, at any count, and holds a
    /// Build at Build alone.
    #[test]
    fn a_broken_base_never_counts() {
        for failed_runs in 0..MAX_FAILED_RUNS {
            let build = state(Status::Active(Stage::Build), 1, 0, failed_runs);
            assert_eq!(to(build.apply(STANDARD, Event::BaseBroken)), build);
        }
        for status in [
            Status::Active(Stage::Watch),
            Status::Active(Stage::Verify),
            Status::NeedsInput {
                return_to: Stage::Build,
            },
        ] {
            let other = state(status, 1, 0, 0);
            assert!(
                other.apply(STANDARD, Event::BaseBroken).is_err(),
                "{status:?}"
            );
        }
    }

    /// OWL-427: a failed rebase off a held commit parks a Build at once,
    /// whatever its count, which it keeps; it is valid at Build alone.
    #[test]
    fn a_failed_rebase_parks_a_build_at_once() {
        for failed_runs in 0..MAX_FAILED_RUNS {
            let build = state(Status::Active(Stage::Build), 1, 0, failed_runs);
            match build.apply(STANDARD, Event::RebaseFailed) {
                Ok(Transition::Parked { state, reason }) => {
                    assert_eq!(reason, ParkReason::Rebase);
                    assert_eq!(state.failed_runs(), failed_runs);
                    assert_eq!(
                        state.status(),
                        Status::Parked {
                            at: Stage::Build,
                            awaiting_input: false
                        }
                    );
                }
                other => panic!("{other:?}"),
            }
        }
        for status in [
            Status::Active(Stage::Watch),
            Status::Active(Stage::Verify),
            Status::NeedsInput {
                return_to: Stage::Build,
            },
        ] {
            let other = state(status, 1, 0, 0);
            assert!(
                other.apply(STANDARD, Event::RebaseFailed).is_err(),
                "{status:?}"
            );
        }
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
            // A fix run's gate refused before any command keeps the count.
            (
                watch(0, 1, 2),
                Event::UncommittedGate,
                stays(watch(0, 1, 2)),
            ),
            (
                watch(1, 1, 2),
                Event::UncommittedGate,
                stays(watch(1, 1, 2)),
            ),
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
        let seen = reachable(STANDARD, &[risky_gate(Some(PLAN))]);
        // The round cap bounds the review passes too: past the last pass
        // only a round, which counts, ends another.
        //
        // Every status the table test uses is reachable but the two of a
        // round that returned to Ready, asked at Intake before OWL-269, and
        // the four of a round waiting at Design review or Verify, which
        // returns to Design or Build since OWL-301; and so is each bound:
        // the fix passes', the repairs' below it, green having cleared the
        // passes and not the repairs, and a review pass past the last.
        let statuses: HashSet<_> = seen.iter().map(TicketState::status).collect();
        assert_eq!(statuses.len(), 25);
        assert!(seen.iter().any(|s| s.fix_passes() == MAX_FIX_PASSES));
        assert!(
            seen.iter()
                .any(|s| s.repairs() == MAX_REPAIRS && s.fix_passes() < MAX_FIX_PASSES)
        );
        assert!(seen.iter().any(|s| s.review_passes() > MAX_REVIEW_PASSES));
        for reached in seen {
            let restored = TicketState::restore(
                reached.status(),
                reached.round(),
                reached.reasks(),
                reached.failed_runs(),
                reached.fix_passes(),
                reached.repairs(),
            )
            .and_then(|state| state.with_review_passes(reached.review_passes()));
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
            (Status::Active(Stage::Intake), 1, 1, 0, 0, 0),
            (parked(Stage::Intake, false), 1, 1, 0, 0, 0),
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
             1, repairs 2, review passes 0): fix passes or repairs outside watch"
        );
    }

    /// Applies `events` in order, each one the state must accept.
    fn walk(from: TicketState, pipeline: Pipeline, events: &[Event]) -> TicketState {
        events.iter().fold(from, |current, &event| {
            match current.apply(pipeline, event) {
                Ok(Transition::To(next) | Transition::Parked { state: next, .. }) => next,
                other => panic!("{:?} + {event:?}: {other:?}", current.status()),
            }
        })
    }

    /// (status, review passes) of a state, for compact expectations.
    fn at(state: TicketState) -> (Status, u32) {
        (state.status(), state.review_passes())
    }

    /// Two passes, the first sending the plan back; the second's findings
    /// go to the last revision (OWL-377), or a round opens, whose answer
    /// buys one more pass, itself last; passing leaves the loop and resets
    /// the count.
    #[test]
    fn design_review_counts_passes_to_the_bound_and_each_answered_round_buys_one() {
        use Event::{Answered, Completed, LoopBack, Questions};
        let design = state(Status::Active(Stage::Design), 1, 0, 0);
        let second = walk(design, STANDARD, &[Completed, LoopBack, Completed]);
        assert_eq!(at(second), (Status::Active(Stage::DesignReview), 1));
        assert_eq!(second.review_pass_due(), 2);
        assert!(is_last_review_pass(second.review_pass_due()));
        assert!(second.apply(STANDARD, LoopBack).is_err());
        // What the last pass leaves goes to Design's last revision, the
        // pass counted (OWL-377); its approval round then counts the
        // revision's pass, which the runner keeps with no reviewer.
        let revised = walk(second, STANDARD, &[Event::LastFix]);
        assert_eq!(at(revised), (Status::Active(Stage::Design), 2));
        let revision = walk(revised, STANDARD, &[Completed]);
        assert_eq!(at(revision), (Status::Active(Stage::DesignReview), 2));
        assert_eq!(
            at(walk(revision, STANDARD, &[Questions])),
            (
                Status::NeedsInput {
                    return_to: Stage::Design
                },
                3
            )
        );
        assert_eq!(
            at(walk(design, STANDARD, &[Completed, Event::LastFix])),
            (Status::Active(Stage::Design), 1),
            "valid before the last pass too"
        );

        let asked = walk(second, STANDARD, &[Questions]);
        assert_eq!(
            at(asked),
            (
                Status::NeedsInput {
                    return_to: Stage::Design
                },
                2
            )
        );
        assert_eq!(asked.round(), 2);
        let third = walk(asked, STANDARD, &[Answered, Completed]);
        assert_eq!(at(third), (Status::Active(Stage::DesignReview), 2));
        assert!(third.apply(STANDARD, LoopBack).is_err());
        assert_eq!(
            at(walk(third, STANDARD, &[Questions])),
            (
                Status::NeedsInput {
                    return_to: Stage::Design
                },
                3
            )
        );
        assert!(third.apply(STANDARD, Completed).is_err(), "no plan gate");
        assert_eq!(
            at(to(third.apply_gated(STANDARD, Completed, not_required()))),
            (Status::Active(Stage::Build), 0)
        );
    }

    /// Verify's loop is Build's: a fix run's own round, a park and a restart
    /// keep the count; Verify's residue round returns to Build; passing
    /// leaves the loop.
    #[test]
    fn verify_counts_passes_across_the_fix_runs_and_their_rounds() {
        use Event::{Answered, Completed, LoopBack, Quarantined, Questions, Restarted};
        let build = state(Status::Active(Stage::Build), 1, 0, 0);
        let fixing = walk(build, STANDARD, &[Completed, LoopBack]);
        assert_eq!(at(fixing), (Status::Active(Stage::Build), 1));
        let second = walk(
            fixing,
            STANDARD,
            &[Questions, Answered, Quarantined, Restarted, Completed],
        );
        assert_eq!(at(second), (Status::Active(Stage::Verify), 1));
        assert!(second.apply(STANDARD, LoopBack).is_err());
        // Minors alone at the last pass get one last Build, the pass
        // counted (OWL-356).
        assert_eq!(
            at(walk(second, STANDARD, &[Event::LastFix])),
            (Status::Active(Stage::Build), 2)
        );
        let asked = walk(second, STANDARD, &[Questions]);
        assert_eq!(
            at(asked),
            (
                Status::NeedsInput {
                    return_to: Stage::Build
                },
                2
            )
        );
        let third = walk(asked, STANDARD, &[Answered, Completed]);
        assert_eq!(at(third), (Status::Active(Stage::Verify), 2));
        assert_eq!(
            at(walk(third, STANDARD, &[Completed])),
            (Status::Active(Stage::Deliver), 0)
        );
    }

    /// A settled `replan` takes a Build back to Design, a new plan with its
    /// review passes from zero; on `trivial` the Build runs again, its
    /// count kept.
    #[test]
    fn a_settled_replan_goes_back_to_design_or_on_trivial_to_build() {
        let trivial = Pipeline::new(Variant::Trivial);
        let fixing = state(Status::Active(Stage::Build), 1, 0, 1)
            .with_review_passes(1)
            .expect("passes at Build");

        let decided = to(fixing.apply(STANDARD, Event::ReplanDecided));
        assert_eq!(at(decided), (Status::Active(Stage::Design), 0));
        assert_eq!(decided.failed_runs(), 0);
        let round = to(fixing.apply(STANDARD, Event::ReplanRound));
        assert_eq!(
            at(round),
            (
                Status::NeedsInput {
                    return_to: Stage::Design
                },
                0
            )
        );
        assert_eq!(round.round(), 2);

        assert_eq!(to(fixing.apply(trivial, Event::ReplanDecided)), fixing);
        let round = to(fixing.apply(trivial, Event::ReplanRound));
        assert_eq!(
            at(round),
            (
                Status::NeedsInput {
                    return_to: Stage::Build
                },
                1
            )
        );
        assert_eq!(round.failed_runs(), 1, "same stage: the failure counts");
    }

    /// A review round the resolver decides whole (OWL-342) goes where the
    /// round would have returned, its pass counted and no round opened,
    /// even at a last pass, which it buys one more pass after, as an
    /// answered round does; it is no event at another stage.
    #[test]
    fn a_decided_review_round_counts_its_pass_and_opens_none() {
        for (stage, back) in [
            (Stage::DesignReview, Stage::Design),
            (Stage::Verify, Stage::Build),
        ] {
            let last = state(Status::Active(stage), 1, 0, 1)
                .with_review_passes(MAX_REVIEW_PASSES - 1)
                .expect("passes in the loop");
            assert!(is_last_review_pass(last.review_pass_due()));
            assert!(last.apply(STANDARD, Event::LoopBack).is_err());
            let decided = to(last.apply(STANDARD, Event::RoundDecided));
            assert_eq!(at(decided), (Status::Active(back), MAX_REVIEW_PASSES));
            assert_eq!((decided.round(), decided.failed_runs()), (1, 0));
            assert_eq!(decided.review_pass_due(), MAX_REVIEW_PASSES + 1);
        }
        for stage in [Stage::Design, Stage::Build, Stage::Watch] {
            let other = state(Status::Active(stage), 1, 0, 0);
            assert!(
                other.apply(STANDARD, Event::RoundDecided).is_err(),
                "{stage:?}"
            );
        }
    }

    /// A reply reversing a resolver's plan approval before delivery
    /// (OWL-345) returns the ticket to Design, no pass counted: from Design
    /// review with its count kept, and from Build or Verify with the count
    /// restarted. `trivial` has no Design to return to.
    #[test]
    fn a_reversed_approval_returns_to_design_and_counts_no_pass() {
        let trivial = Pipeline::new(Variant::Trivial);
        let passes = |stage| {
            state(Status::Active(stage), 1, 0, 1)
                .with_review_passes(1)
                .expect("passes in the loop")
        };
        let from_review = to(passes(Stage::DesignReview).apply(STANDARD, Event::ReversedToDesign));
        assert_eq!(at(from_review), (Status::Active(Stage::Design), 1));
        assert_eq!((from_review.round(), from_review.failed_runs()), (1, 0));
        for stage in [Stage::Build, Stage::Verify] {
            let back = to(passes(stage).apply(STANDARD, Event::ReversedToDesign));
            assert_eq!(at(back), (Status::Active(Stage::Design), 0), "{stage:?}");
        }

        let event = Event::ReversedToDesign;
        for status in all_statuses() {
            let Some(other) = baseline(status) else {
                continue;
            };
            let valid = matches!(
                status,
                Status::Active(Stage::DesignReview | Stage::Build | Stage::Verify)
            );
            assert_eq!(
                other.apply(STANDARD, event).is_ok(),
                valid,
                "{status:?} + {event:?}"
            );
        }
        for stage in [Stage::Build, Stage::Verify] {
            assert!(
                passes(stage)
                    .apply(trivial, Event::ReversedToDesign)
                    .is_err(),
                "{stage:?}"
            );
        }
    }

    #[test]
    fn a_review_pass_past_u32_max_is_refused() {
        let verify = state(Status::Active(Stage::Verify), 1, 0, 0)
            .with_review_passes(u32::MAX)
            .expect("passes at Verify");
        assert_eq!(verify.review_pass_due(), u32::MAX);
        assert!(verify.apply(STANDARD, Event::Questions).is_err());
    }

    #[test]
    fn review_passes_are_kept_only_in_a_review_loop() {
        let parked = |at, awaiting_input| Status::Parked { at, awaiting_input };
        for status in [
            Status::Active(Stage::Design),
            Status::Active(Stage::DesignReview),
            Status::NeedsInput {
                return_to: Stage::Build,
            },
            parked(Stage::Verify, true),
        ] {
            let base = baseline(status).expect("a valid status");
            assert_eq!(
                base.with_review_passes(3).map(|s| s.review_passes()),
                Ok(3),
                "{status:?}"
            );
        }
        for status in [
            Status::Active(Stage::Intake),
            Status::Active(Stage::Ready),
            parked(Stage::Deliver, false),
            Status::NeedsInput {
                return_to: Stage::Watch,
            },
        ] {
            let base = baseline(status).expect("a valid status");
            assert_eq!(base.with_review_passes(0), Ok(base));
            let error = base.with_review_passes(1).expect_err("outside a loop");
            assert!(
                error
                    .to_string()
                    .ends_with("review passes outside design, design review, build and verify"),
                "{error}"
            );
        }
    }

    /// The (state, event) table of the design loop's states, with each
    /// review pass count a ticket has there and each plan gate (OWL-32):
    /// none, approval not required, required and not given, required and
    /// given. Completing Design review needs the gate open; `PlanApproved`
    /// needs a review pass of the loop behind it and a gate that required
    /// the approval and holds it; every other move is the table's.
    #[test]
    fn the_design_loop_enters_build_only_through_the_plan_gate() {
        use Stage::{Design, DesignReview};
        let gates = [
            None,
            Some(not_required()),
            Some(risky_gate(None)),
            Some(risky_gate(Some(OTHER_PLAN))),
            Some(risky_gate(Some(PLAN))),
        ];
        let expect = |status: Status, passes: u32, event: Event, gate: Option<PlanGate>| {
            let to_build = Expect::To(Status::Active(Stage::Build));
            match (status, event) {
                (Status::Active(DesignReview), Event::Completed) => {
                    if gate.is_some_and(PlanGate::is_open) {
                        to_build
                    } else {
                        Expect::Invalid
                    }
                }
                (Status::Active(Design), Event::PlanApproved) => {
                    if passes > 0 && gate.is_some_and(|g| g.required() && g.approved()) {
                        to_build
                    } else {
                        Expect::Invalid
                    }
                }
                // A last pass is never sent back.
                (Status::Active(DesignReview), Event::LoopBack)
                    if is_last_review_pass(passes + 1) =>
                {
                    Expect::Invalid
                }
                _ => expected(status, event),
            }
        };
        for status in [Status::Active(Design), Status::Active(DesignReview)] {
            for passes in 0..=MAX_REVIEW_PASSES {
                let state = baseline(status)
                    .and_then(|s| s.with_review_passes(passes).ok())
                    .expect("a valid state");
                for event in Event::ALL {
                    for gate in gates {
                        let applied = match gate {
                            None => state.apply(STANDARD, event),
                            Some(gate) => state.apply_gated(STANDARD, event, gate),
                        };
                        let actual = match applied {
                            Err(_) => Expect::Invalid,
                            Ok(Transition::To(next)) => Expect::To(next.status()),
                            Ok(Transition::Parked { state, reason }) => {
                                Expect::Parked(state.status(), reason)
                            }
                            Ok(Transition::Finished(finish)) => Expect::Finished(finish),
                        };
                        assert_eq!(
                            actual,
                            expect(status, passes, event, gate),
                            "{status:?} with {passes} passes + {event:?}, gate {gate:?}"
                        );
                    }
                }
            }
        }

        // Outside the design loop the gate changes nothing.
        let states: Vec<_> = all_statuses().into_iter().filter_map(baseline).collect();
        for state in states.iter().filter(|s| {
            !matches!(
                s.status(),
                Status::Active(Stage::Design | Stage::DesignReview)
            )
        }) {
            for event in Event::ALL {
                for gate in gates.into_iter().flatten() {
                    assert_eq!(
                        state.apply_gated(STANDARD, event, gate),
                        state.apply(STANDARD, event),
                        "{:?} + {event:?}",
                        state.status()
                    );
                }
            }
        }

        // `trivial` has no plan to approve: a ticket whose variant changed
        // at Design moves on to Build without a gate, but never by an
        // approval.
        let trivial = Pipeline::new(Variant::Trivial);
        let design = state(Status::Active(Design), 1, 0, 0)
            .with_review_passes(1)
            .expect("passes at Design");
        assert_eq!(
            to(design.apply(trivial, Event::Completed)).status(),
            Status::Active(Stage::Build)
        );
        assert!(design.apply(trivial, Event::PlanApproved).is_err());
    }

    /// A risky ticket never reaches Build, nor any stage after it, without
    /// its plan approved: not with every event applied without a gate, nor
    /// with a gate whose approval is missing or of another plan.
    #[test]
    fn a_risky_ticket_cannot_reach_build_without_an_approval() {
        let past_design = |seen: &HashSet<TicketState>| {
            seen.iter()
                .filter(|s| s.stage() >= Stage::Build)
                .map(TicketState::status)
                .collect::<Vec<_>>()
        };
        let shut = reachable(RISKY, &[risky_gate(None), risky_gate(Some(OTHER_PLAN))]);
        assert!(
            shut.iter()
                .any(|s| s.status() == Status::Active(Stage::DesignReview))
        );
        assert_eq!(past_design(&shut), Vec::new());
        let approved = reachable(RISKY, &[risky_gate(Some(PLAN))]);
        assert!(
            approved
                .iter()
                .any(|s| s.status() == Status::Active(Stage::Build))
        );
    }

    /// The approval round is a `Questions` at Design review on top of a
    /// passed pass: it counts the pass and returns to Design. An approval
    /// takes the plan to Build; a changed plan gets one more pass, a last
    /// one, and is asked again; an approval kept for another plan opens
    /// nothing.
    #[test]
    fn an_approval_round_takes_an_approved_plan_to_build_and_a_changed_one_to_review() {
        use crate::review::PassOutcome;
        use Event::{Answered, Completed, PlanApproved};
        let design = state(Status::Active(Stage::Design), 1, 0, 0);
        let reviewed = walk(design, RISKY, &[Completed]);
        let shut = risky_gate(None);
        assert!(reviewed.apply(RISKY, Completed).is_err());
        assert!(reviewed.apply_gated(RISKY, Completed, shut).is_err());
        let round = PassOutcome::Passed.design_review_event(shut);
        let asked = walk(reviewed, RISKY, &[round]);
        assert_eq!(
            at(asked),
            (
                Status::NeedsInput {
                    return_to: Stage::Design
                },
                1
            )
        );
        let answering = walk(asked, RISKY, &[Answered]);
        assert_eq!(at(answering), (Status::Active(Stage::Design), 1));
        for gate in [shut, risky_gate(Some(OTHER_PLAN))] {
            assert!(answering.apply_gated(RISKY, PlanApproved, gate).is_err());
        }
        assert!(answering.apply(RISKY, PlanApproved).is_err());
        let approved = risky_gate(Some(PLAN));
        assert_eq!(
            at(to(answering.apply_gated(RISKY, PlanApproved, approved))),
            (Status::Active(Stage::Build), 0)
        );

        // Changed: the revised plan's last pass, then the round again.
        let last = walk(answering, RISKY, &[Completed]);
        assert_eq!(at(last), (Status::Active(Stage::DesignReview), 1));
        assert!(is_last_review_pass(last.review_pass_due()));
        assert!(last.apply(RISKY, Event::LoopBack).is_err());
        let changed = risky_gate(Some(OTHER_PLAN));
        assert!(last.apply_gated(RISKY, Completed, changed).is_err());
        let again = walk(
            last,
            RISKY,
            &[PassOutcome::Passed.design_review_event(changed), Answered],
        );
        assert_eq!(at(again), (Status::Active(Stage::Design), 2));
        assert_eq!(again.round(), 3);
        assert_eq!(
            at(to(again.apply_gated(RISKY, PlanApproved, approved))),
            (Status::Active(Stage::Build), 0)
        );
    }

    /// A resolver's change for the notes alone (OWL-463): the approval round
    /// it decided at a last pass counts the pass; the revision's own pass,
    /// which no reviewer runs, takes it to Build only on the approval of the
    /// revised plan's hash, never on the plan put to approval.
    #[test]
    fn a_resolvers_change_for_notes_reaches_build_on_the_revised_plans_approval() {
        use Event::{Completed, RoundDecided};
        let last = state(Status::Active(Stage::DesignReview), 1, 0, 0)
            .with_review_passes(MAX_REVIEW_PASSES - 1)
            .expect("passes in the loop");
        let revising = walk(last, STANDARD, &[RoundDecided]);
        assert_eq!(
            at(revising),
            (Status::Active(Stage::Design), MAX_REVIEW_PASSES)
        );
        let unreviewed = walk(revising, STANDARD, &[Completed]);
        let gate = |kept| {
            GatePolicy::new(Vec::<String>::new(), PlanApproval::OnFork)
                .plan_gate(STANDARD, true, OTHER_PLAN, kept)
        };
        assert!(
            unreviewed
                .apply_gated(STANDARD, Completed, gate(Some(PLAN)))
                .is_err()
        );
        assert_eq!(
            at(to(unreviewed.apply_gated(
                STANDARD,
                Completed,
                gate(Some(OTHER_PLAN))
            ))),
            (Status::Active(Stage::Build), 0)
        );
    }

    /// A plan invalidated during Build goes back to Design with no review
    /// pass behind it: no approval takes it to Build before its new plan is
    /// reviewed, and that plan goes through the gate as the first did.
    #[test]
    fn a_replanned_ticket_reaches_build_again_only_through_review_and_the_gate() {
        use Event::{Answered, Completed, PlanApproved, ReplanRound};
        let build = state(Status::Active(Stage::Build), 2, 0, 0);
        let design = walk(build, RISKY, &[ReplanRound, Answered]);
        assert_eq!(at(design), (Status::Active(Stage::Design), 0));
        let approved = risky_gate(Some(PLAN));
        assert!(design.apply_gated(RISKY, PlanApproved, approved).is_err());
        let reviewed = walk(design, RISKY, &[Completed]);
        assert!(reviewed.apply(RISKY, Completed).is_err());
        assert!(
            reviewed
                .apply_gated(RISKY, Completed, risky_gate(None))
                .is_err()
        );
        assert_eq!(
            to(reviewed.apply_gated(RISKY, Completed, approved)).status(),
            Status::Active(Stage::Build)
        );
    }

    /// A review loop (OWL-307): reviews take a delivered ticket from Watch
    /// back to Build, its repairs no longer counted and its Verify passes
    /// counted from the first; its end with the pull request brings it back
    /// to Watch from wherever it waits, and a second loop counts its passes
    /// afresh.
    #[test]
    fn a_review_loop_counts_its_own_passes_and_ends_with_its_pull_request() {
        use Event::{Completed, FollowingEnded, LoopBack, Questions, RunFailed};
        let watching = watch(1, 2, 5);
        let building = to(watching.apply(STANDARD, LoopBack));
        assert_eq!(building.status(), Status::Active(Stage::Build));
        assert_eq!(
            (
                building.fix_passes(),
                building.repairs(),
                building.failed_runs()
            ),
            (0, 0, 0)
        );
        // Its Build holds the drain's one slot as any Build does.
        assert!(crate::schedule::holds_slot(&building));
        assert!(!crate::schedule::holds_slot(&watching));
        let revised = walk(building, STANDARD, &[Completed, LoopBack, Completed]);
        assert_eq!(at(revised), (Status::Active(Stage::Verify), 1));
        // Waiting on the residue's round, then parked on two failed runs of
        // its answer check: each ends with the pull request.
        let waiting = to(revised.apply(STANDARD, Questions));
        assert_eq!(
            waiting.status(),
            Status::NeedsInput {
                return_to: Stage::Build
            }
        );
        let parked = match walk(waiting, STANDARD, &[RunFailed]).apply(STANDARD, RunFailed) {
            Ok(Transition::Parked { state, .. }) => state,
            other => panic!("{other:?}"),
        };
        for loop_state in [building, revised, waiting, parked] {
            let ended = to(loop_state.apply(STANDARD, FollowingEnded));
            assert_eq!(at(ended), (Status::Active(Stage::Watch), 0));
            assert_eq!((ended.reasks(), ended.failed_runs()), (0, 0));
            let again = walk(ended, STANDARD, &[LoopBack, Completed]);
            assert_eq!(at(again), (Status::Active(Stage::Verify), 0));
        }
        // Delivered: back at Watch, the loop's passes no longer counted.
        let delivered = walk(revised, STANDARD, &[Completed, Completed]);
        assert_eq!(at(delivered), (Status::Active(Stage::Watch), 0));
        // Elsewhere the end of the following is no event.
        for stage in [Stage::Design, Stage::DesignReview, Stage::Deliver] {
            let other = state(Status::Active(stage), 1, 0, 0);
            assert!(other.apply(STANDARD, FollowingEnded).is_err(), "{stage:?}");
        }
    }
}
