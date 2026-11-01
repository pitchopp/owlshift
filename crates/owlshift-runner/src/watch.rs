//! `owlshift watch` (roadmap P3, OWL-152): what a person typing `owlshift
//! continue` does once the decider has answered, done by a loop in the
//! foreground. The decisions are architecture section 5's "Watch mode".
//!
//! Each pass takes the project's lock for a moment, the snapshot: it checks
//! the `unverified` marker and reads the ticket refs. The lock is released
//! before the tracker is read, so a command typed by hand meanwhile runs.
//! Each ticket whose questions wait ([`watched`]) has its comments read, and
//! when the decider's reply counts ([`answer_check::readiness`], the function
//! `continue` asks too), the ticket goes through `continue`'s own path,
//! [`OnDemand::continue_seen`], which runs only while the ticket ref, read
//! again under the lock, is still the one the snapshot read.
//!
//! Since OWL-209, each pass also looks at the pull request of each ticket at
//! Watch whose delivery Owlshift follows ([`followed`]): its read, without
//! the lock, decides by the core's table, and what it changes in the ticket
//! ref is written under the lock from the ref the snapshot read
//! ([`crate::follow`]). A fix run, a rebase run or a park it decides is
//! repaired through [`OnDemand::repair_seen`] (OWL-112, OWL-210), held as a
//! continue is. A read that asks for no repair leaves room for a correction:
//! the decider's replies to a DECISION comment, once they count, go to
//! [`OnDemand::correct_seen`] (OWL-146). A ticket whose round waits at
//! Watch ([`outlived`]) has its pull request read before its comments, for
//! an end alone, which closes the round (OWL-242); so has a ticket parked at
//! Watch, whose park an end ends without restarting anything (OWL-243).
//! Before its pull request is read, each ticket at Watch whose delivery is
//! followed has its stage read: one a stop left at working while no run
//! holds the project's lock is moved to the stage its ref shows (OWL-265,
//! architecture section 5, "The stage while a repair runs").
//!
//! Since OWL-307, a read that asks for no repair and ran no correction
//! leaves room for the reviews on the pull request: one read of them, and a
//! review of the decider's that `[forge].reviewers` lists starts a review
//! loop ([`OnDemand::review_seen`]), held as a continue is. A ticket whose
//! loop is at Build or Verify, working, waiting or parked there, has its
//! pull request read as a round waiting at Watch has, for an end, which
//! ends the loop, or for a push of the loop's own a crash cut short, which
//! is adopted; both under the project's lock.
//!
//! Since OWL-273, a ticket an answered intake round left at Intake
//! (`resumable_stage`), its second intake cut off by a usage limit or a
//! refusal, is resumed while the tracker still shows it needs input: its
//! intake runs again through [`OnDemand::continue_shown`], held as a
//! continue is. One shown elsewhere, in the ready column (the drain's) or
//! moved by a person, is left alone, its stage read again once its ref moves
//! or after [`RETRY_AFTER`].
//!
//! Since OWL-340, the same holds for a ticket left at Design, Design review,
//! Build or Verify with an answered round (round > 0) that the tracker still
//! shows working: a refusal or a cut-off run after the RESUME left it there,
//! shown In Progress with nothing running. It goes through `continue`'s path
//! with no answer check and no RESUME, as the ref waits for no answer. A
//! review loop's ticket at Build or Verify ([`outlived`]) is resumed once
//! its pull request, read for an end, has not ended the loop. While another
//! command holds the project's lock, the pass ends at its snapshot, before
//! any ticket is read.
//!
//! Since OWL-367, watch works a ticket one stage step a pass (architecture
//! section 5, "One step a pass"): a continue stops once its answer check's
//! RESUME is kept, and a resume, the drain's dispatch or a review loop once
//! its stage's run is kept, a Build and the Verify pass of its commit being
//! one step ([`Stop::Stepped`]). The next pass, which comes at once after a
//! step, takes the next step. So a ticket at Design, Design review, Build or
//! Verify that the tracker shows working is resumed whatever its round, at
//! round 0 too, in its turn of the project's slot ([`schedule::slot_turn`]):
//! the others wait, told once per holder, while every waiting ticket's
//! answer is still checked at each pass. What a step carries, the
//! resolver's chain and the late-comment integration, goes to the next,
//! through `carry.json` since OWL-399 ([`crate::step`]).
//!
//! Since OWL-399, each step that may run an agent or the gate is an
//! envelope ([`StepEnvelope`]) carried out by [`step::run`]: in watch's own
//! process for plain `owlshift watch` and the test bench ([`Watch::run`],
//! [`Watch::run_drain`]), or in a child process, `owlshift continue <TICKET>
//! --step <envelope>`, for `owlshift watch --drain`
//! ([`Watch::run_with_children`]), whose outcome record watch reads when it
//! reaps the child. A watch that starts applies the holds the records its
//! predecessor left call for, while each ticket's ref is where its record
//! says.
//!
//! Since OWL-401 (architecture section 5, "Parallel"), several tickets of a
//! project work at once, up to `policy.max_in_flight`: the slot's turn goes
//! to the first holders by dispatch time ([`schedule::slot_turn`]), the
//! drain dispatches a list of tickets a pass, and `owlshift watch --drain`
//! starts each step's child without waiting for it. Each pass first applies
//! the end of every child that ended, then leaves the tickets whose child
//! runs alone; between two passes watch waits for the first child to end,
//! or the interval. A pass takes the steps of tickets already dispatched in
//! the design's order: repairs at Watch, answer checks, resumes, each by
//! dispatch time; then the drain's one intake, none while one runs, and its
//! dispatches. Commands hold the project's lock for short sections only, so
//! a snapshot finds it busy for those moments alone.
//!
//! Since OWL-368, a snapshot that finds the `unverified` marker still reads
//! the ticket refs, through the runner's read-only plumbing, and the pass
//! then runs nothing: it notifies a marker no command notified, prints the
//! project's line once, and tells each waiting ticket whose answer counts,
//! and the ticket the drain would dispatch, that it waits for the quarantine,
//! once per ticket and marker ([`crate::quarantine`]). Since OWL-423, the
//! first snapshot that finds it cleared folds those comments.
//!
//! Nothing ends the loop but the operator: a ticket that cannot be read is a
//! `warning` event, once until it reads again, and the pass goes on. Since
//! OWL-161 the operator stops it by degrees ([`stop`]): once a drain or a
//! checked stop is asked, by the process's signals or the project's stop
//! file, no step starts and no pass runs; the watch waits for its steps'
//! children, applies their ends, then returns. A
//! continue that left the ticket ref as it was is held for [`RETRY_AFTER`]
//! ([`Holds`]), so a tracker that refuses writes does not cost an answer
//! check every pass.
//!
//! Since OWL-404, a usage limit holds its harness on the whole machine, in
//! the usage file every command reads (`crate::usage`): each pass reads it,
//! and no step whose first run would use a limited harness starts; a step
//! that runs no model first goes on. A step that ended at the limit holds
//! its own ticket until the limit's end, which only matters when the usage
//! file could not be kept. A continue that left a comment on the ticket whose
//! state was not kept ([`Stop::NotKept`]) is held until a person acts
//! (OWL-158, OWL-169 for a RESUME), so the comment is not posted again at
//! each retry.

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::io::Write;
use std::time::Duration;

use jiff::Timestamp;
use serde_json::json;

use owlshift_adapters::forge::ReviewsRead;
use owlshift_adapters::tracker::Person;
use owlshift_contracts::Role;
use owlshift_contracts::event::EventKind;
use owlshift_contracts::format::Format;
use owlshift_contracts::ids::TicketId;
use owlshift_contracts::step::{LandedKind, RefAfter, StepEnd, StepEnvelope, StepHold, StepKind};
use owlshift_core::collision::{Collision, Dispatched, InFlight, weigh};
use owlshift_core::follow::{Action, Decision, EndReason};
use owlshift_core::schedule::{self, Turn};
use owlshift_core::state::{Status, TicketState};
use owlshift_core::usage::{Standing, own_harness};
use owlshift_core::vocab::{Harness, Overlap, Stage};

use crate::answer_check::{self, Readiness};
use crate::base;
use crate::events::{Data, EventSink, data, printable};
use crate::follow::{self, Following, Look};
use crate::held;
use crate::in_flight;
use crate::on_demand::{self, Delivered, OnDemand, PIPELINE, Stop, delivery_followed};
use crate::pr_reviews;
use crate::project::{ProjectLock, TicketLock};
use crate::quarantine::{self, Held, Record};
use crate::reversal::{self, Current};
use crate::step::{self, Children};
use crate::ticket_ref::{self, Progress, ProposalsFile, RefAt, RefRead, Stored, TicketRecord};
use crate::ticket_run;
use crate::usage as machine_usage;
use crate::why;
use crate::writer::VisibleStage;

mod cancel;
mod drain;
mod limits;
mod merge;
pub mod offline;
mod outside;
mod pause;
pub mod presence;
mod refresh;
mod running;
pub mod stop;
mod waiting;

use running::{RunningStep, Then};

pub use drain::refusal as drain_refusal;

/// How long watch sleeps after each pass.
pub const POLL_INTERVAL: Duration = Duration::from_secs(60);

/// How long a continue that left the ticket ref as it was holds that ticket
/// back, unless the decider edits a comment or the ref moves meanwhile. One
/// that stopped with [`Stop::NotKept`] is held until then, with no end.
pub const RETRY_AFTER: Duration = Duration::from_secs(10 * 60);

/// How long a base check due holds every other step back at most (OWL-418):
/// longer than any step a run can make, its deadline and its gate's.
pub const BASE_DUE_AT_MOST: Duration = Duration::from_secs(3 * 60 * 60);

/// Whether watch continues a ticket in `status`: one whose questions wait
/// for its decider. A parked ticket waits for a person's `continue`, which
/// alone restarts it (watch only ends a park at Watch whose pull request
/// ended, [`outlived`]), and one at a stage waits for no answer: only one an
/// answered round left there is resumed, by `resumable_stage` (OWL-273,
/// OWL-340).
pub fn watched(status: Status) -> bool {
    matches!(status, Status::NeedsInput { .. })
}

/// Whether watch follows the pull request of a ticket in `status` whose
/// ref keeps `stored`: a ticket working at Watch, neither waiting nor
/// parked, whose delivery has a pull request and has not ended (OWL-209).
pub fn followed(status: Status, stored: &Stored) -> bool {
    status == Status::Active(Stage::Watch)
        && stored
            .record
            .delivery
            .as_ref()
            .is_some_and(|delivery| delivery.followed())
}

/// Whether watch reads the pull request of a ticket in `status` whose ref
/// keeps `stored` for whether its following ended: a ticket whose round
/// waits at Watch (OWL-242), or parked there (OWL-243), whose delivery is
/// still followed.
pub fn outlived(status: Status, stored: &Stored) -> bool {
    follow::ends_with_following(status)
        && stored
            .record
            .delivery
            .as_ref()
            .is_some_and(|delivery| delivery.followed())
}

/// The stage the tracker must still show for watch to resume a ticket in
/// `state`, `None` when watch does not resume it.
///
/// OWL-273: a ticket at Intake after a question round, as an answered intake
/// round leaves it while intake runs again and a cut-off run leaves it, is
/// resumed while it shows needs input. A ticket at Intake that asked no round
/// is the drain's.
///
/// OWL-340: a ticket at Design, Design review, Build or Verify with an
/// answered round (round > 0), which a refusal or a cut-off run left after
/// the RESUME moved it to working, is resumed while it still shows working.
///
/// OWL-367: whatever its round, since watch runs one stage step a pass and
/// leaves every ticket it works there between two passes; so is a `do` or a
/// drain's dispatch that a stop left at round 0. One parked or waiting needs
/// `owlshift continue` or an answer.
pub(crate) fn resumable_stage(state: &TicketState) -> Option<VisibleStage> {
    match state.status() {
        Status::Active(Stage::Intake) if state.round() > 0 => Some(VisibleStage::NeedsInput),
        Status::Active(Stage::Design | Stage::DesignReview | Stage::Build | Stage::Verify) => {
            Some(VisibleStage::Working)
        }
        _ => None,
    }
}

/// How the stage reads in watch's lines.
pub(crate) fn stage_label(stage: Stage) -> &'static str {
    match stage {
        Stage::Intake => "Intake",
        Stage::Ready => "Ready",
        Stage::Design => "Design",
        Stage::DesignReview => "Design review",
        Stage::Build => "Build",
        Stage::Verify => "Verify",
        Stage::Deliver => "Deliver",
        Stage::Watch => "Watch",
    }
}

/// Whether a step that ended `end` is held by its usage limit at `now`, which
/// the usage file's line says (OWL-404): one whose reset is still ahead.
fn usage_held(end: &StepEnd, now: Timestamp) -> bool {
    matches!(end, StepEnd::UsageLimit { resets_at: Some(at) } if *at > now)
}

/// The line for a resume of `ticket` at `stage` that left the ticket ref as
/// it was, held until `until`.
fn held_text(ticket: &TicketId, stage: Stage, until: Timestamp) -> String {
    format!(
        "{ticket} is at {} as it was before that resume: watch resumes it again at {until}, or \
         sooner once a command moves its ticket ref",
        stage_label(stage)
    )
}

/// What a continue starts from: the ticket ref's commit, which the continue
/// checks under the lock, its progress, which the holds compare (OWL-397:
/// the runner keeps the spend in the ref after every run, a move watch does
/// not read as progress), and the decider's newest edit when watch decided
/// to continue.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Seen {
    pub commit: String,
    pub progress: Progress,
    pub answer: Option<Timestamp>,
}

impl Seen {
    /// What a continue from the ref at `at`, with `answer`, starts from.
    pub fn at(at: RefAt, answer: Option<Timestamp>) -> Self {
        Seen {
            commit: at.commit,
            progress: at.progress,
            answer,
        }
    }
}

/// Whether a ticket whose reply counts may be continued now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Admit {
    Go,
    /// Its last continue, from the same ticket ref and answer, left the ref
    /// as it was.
    Held {
        until: Until,
    },
}

/// Until when a ticket is held, unless its decider edits a comment or its
/// ticket ref moves first, a move of its spend alone aside (OWL-397).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Until {
    /// [`RETRY_AFTER`] past the continue.
    At(Timestamp),
    /// No end: the continue left a comment on the ticket, a RESUME included,
    /// or a park whose comment could not be posted, whose state was not kept
    /// ([`Stop::NotKept`]), which asks a person to act (OWL-158, OWL-159,
    /// OWL-169).
    Person,
}

impl Until {
    fn holds_at(self, now: Timestamp) -> bool {
        match self {
            Until::At(until) => now < until,
            Until::Person => true,
        }
    }
}

/// What holds continues back between passes. Pure: the caller reads the
/// clock and the ticket refs.
#[derive(Debug, Default)]
pub struct Holds {
    held: HashMap<TicketId, (Seen, Until)>,
}

impl Holds {
    /// Whether `ticket`, at `seen`, may be continued at `now`. A hold from
    /// another answer, or from a ticket ref whose progress has changed, is
    /// lifted; one whose ref moved by its spend alone stands (OWL-397).
    pub fn admit(&mut self, ticket: &TicketId, seen: &Seen, now: Timestamp) -> Admit {
        match self.held.get(ticket) {
            Some((held, until))
                if held.progress == seen.progress
                    && held.answer == seen.answer
                    && until.holds_at(now) =>
            {
                Admit::Held { until: *until }
            }
            Some(_) => {
                self.held.remove(ticket);
                Admit::Go
            }
            None => Admit::Go,
        }
    }

    /// Records what a continue of `ticket` from `seen` did: `outcome`, then
    /// `progress`, its ticket ref's progress read after it (`None` when gone
    /// or unreadable), at `now`. Returns the hold it set, if any.
    ///
    /// A busy project or ticket, or no free run slot (OWL-409), ran
    /// nothing: no hold; nor does a checked stop's end (OWL-161), so the
    /// ticket resumes at the next start. A usage limit with a reset time holds this ticket
    /// until then, whatever its ref did (OWL-404): the usage file holds
    /// every other step of its harness, and this hold keeps the ticket back
    /// should that file not have been kept. A comment, a RESUME included, or
    /// a
    /// park whose comment could not be posted, whose state was not kept
    /// ([`Stop::NotKept`]) holds the ticket until a person acts, unless the
    /// ref was read moved: an unreadable ref is no reason to post the comment
    /// or park again. Otherwise a ticket ref left as it was holds the ticket for
    /// [`RETRY_AFTER`]; one that moved is progress, which the next pass reads.
    /// A ref that moved by its spend alone, which the runner keeps after
    /// every run, is left as it was (OWL-397). No other way to stop is told
    /// apart, so a way added later is held exactly when it leaves the ref as
    /// it was.
    pub fn after(
        &mut self,
        ticket: &TicketId,
        seen: Seen,
        outcome: &StepEnd,
        progress: Option<&Progress>,
        now: Timestamp,
    ) -> Option<Admit> {
        if matches!(outcome, StepEnd::Busy | StepEnd::NoSlot | StepEnd::Stopped) {
            return None;
        }
        let as_it_was = progress == Some(&seen.progress);
        let until = match outcome {
            StepEnd::UsageLimit {
                resets_at: Some(at),
            } if *at > now => Until::At(*at),
            StepEnd::NotKept { .. } if as_it_was || progress.is_none() => Until::Person,
            _ if as_it_was => Until::At(now.checked_add(RETRY_AFTER).unwrap_or(Timestamp::MAX)),
            _ => {
                self.held.remove(ticket);
                return None;
            }
        };
        self.held.insert(ticket.clone(), (seen, until));
        Some(Admit::Held { until })
    }

    /// Whether a hold of `ticket` stands at `now`, whatever its ref says
    /// since.
    fn stands(&self, ticket: &TicketId, now: Timestamp) -> bool {
        self.held
            .get(ticket)
            .is_some_and(|(_, until)| until.holds_at(now))
    }

    /// Forgets the holds of the tickets `kept` does not name.
    fn retain(&mut self, kept: &[TicketId]) {
        self.held.retain(|ticket, _| kept.contains(ticket));
    }

    /// Lifts `ticket`'s hold: what it waited for ended (OWL-418).
    pub fn release(&mut self, ticket: &TicketId) {
        self.held.remove(ticket);
    }

    /// Applies `hold`, which a step of `ticket` from `seen` called for, as
    /// [`Self::after`] would have set it (OWL-399): a record a predecessor
    /// left.
    pub fn restore(&mut self, ticket: &TicketId, seen: Seen, hold: &StepHold) {
        let StepHold::Held { until } = hold;
        let until = until.map_or(Until::Person, Until::At);
        self.held.insert(ticket.clone(), (seen, until));
    }
}

/// The hold a step of `ticket` from `seen` that ended `end` calls for, its
/// ref's progress after it being `progress`, at `now`, by [`Holds::after`]'s
/// rule: what the child of a step records for a watch that starts after it
/// (OWL-399).
pub(crate) fn hold_for(
    ticket: &TicketId,
    seen: Seen,
    end: &StepEnd,
    progress: Option<&Progress>,
    now: Timestamp,
) -> Option<StepHold> {
    match Holds::default().after(ticket, seen, end, progress, now)? {
        Admit::Go => None,
        Admit::Held {
            until: Until::At(until),
        } => Some(StepHold::Held { until: Some(until) }),
        Admit::Held {
            until: Until::Person,
        } => Some(StepHold::Held { until: None }),
    }
}

/// `owlshift watch` over one project.
pub struct Watch<'a> {
    /// What `continue` runs with; its clock is the one watch reads.
    pub on_demand: &'a OnDemand<'a>,
    /// How long to sleep after each pass: [`POLL_INTERVAL`].
    pub interval: Duration,
}

/// What a pass found about the whole project, printed when it changes.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Project {
    /// Another command holds the project's lock.
    Busy,
    /// A run's isolation check did not pass: a person looks first. The
    /// marker's text, so that another quarantine is printed too.
    Quarantined(String),
    /// The snapshot failed.
    Unread(String),
    /// The tickets whose questions wait, and those whose pull request watch
    /// follows.
    Watching {
        waiting: Vec<TicketId>,
        followed: Vec<TicketId>,
    },
}

/// What the loop keeps from one pass to the next.
#[derive(Default)]
struct Memory<'c> {
    /// The children watch runs its steps in, for `owlshift watch --drain`;
    /// none: in its own process (OWL-399).
    children: Option<&'c Children<'c>>,
    /// When this watch started, in milliseconds, and how many steps it
    /// started since: its steps' attempt ids.
    started_ms: i64,
    attempts: u64,
    /// The steps whose children run (OWL-401): their tickets are left alone
    /// until each ends.
    running: Vec<RunningStep>,
    /// The holds a predecessor's records call for, applied once this pass
    /// has dropped those of the tickets it no longer lists.
    adopted: Vec<(TicketId, Seen, StepHold)>,
    holds: Holds,
    /// What was last printed about the project.
    project: Option<Project>,
    /// Why each ticket could not be read, as last warned of.
    failing: HashMap<TicketId, String>,
    /// What watch keeps of the pull requests it follows.
    following: Following,
    /// What watch keeps of the stages it found left at working.
    stale: Stale,
    /// What the drain keeps (OWL-250).
    drain: drain::Memory,
    /// The tickets a stop left in a stage watch resumes, found shown in
    /// another stage than that stage must show (OWL-273 at Intake, which must
    /// show needs input; OWL-340 at Design, Design review, Build and Verify,
    /// which must show working): their ref's progress then, and until when
    /// their stage is not read again unless the ref moves.
    elsewhere: HashMap<TicketId, (Progress, Timestamp)>,
    /// What watch knows of the marker that quarantines the project, while
    /// one stands (OWL-368).
    quarantine: Option<Quarantine>,
    /// Whose turn the project's slot is for watch's stage steps (OWL-367,
    /// OWL-401): chosen at each pass by [`schedule::slot_turn`], the first
    /// `policy.max_in_flight` holders by dispatch time, and taken during a
    /// pass by a ticket whose ref came to hold the slot while there is room.
    turn: Option<Turn<TicketId>>,
    /// The holders the pass chose the turn among, each with its dispatch
    /// time, and the refs it could not read: what an answer check weighs to
    /// tell whether its ticket will wait for the turn (OWL-401).
    holding: (Vec<(TicketId, Dispatched)>, Vec<TicketId>),
    /// The tickets told that their next stage step waits, with what was
    /// told: the slot's holders, or a collision (OWL-403).
    waits: HashMap<TicketId, StepWait>,
    /// The tickets whose next stage step a collision holds back, as this
    /// pass's snapshot read them (OWL-403), each with whether it is its
    /// Build's admission that waits (OWL-448): they leave the slot's turn to
    /// the others.
    collisions: HashMap<TicketId, (Collision<String>, bool)>,
    /// The work in flight outside Owlshift, as read (OWL-410).
    outside: outside::OutsideMemory,
    /// The tickets whose answer check watch ran into a stage that needs the
    /// slot, with their ref's progress after it: the tracker may still show
    /// them needs input when the move to working after the RESUME failed.
    resumed: HashMap<TicketId, Progress>,
    /// This pass took a step after which the next pass is due at once: a
    /// stage step whose ref moved, or a holder of the slot that left it.
    stepped: bool,
    /// Each harness's usage standing as last told (OWL-404).
    usage: HashMap<Harness, Standing>,
    /// The WAITING comments shown on the tickets whose next stage step
    /// waits (OWL-419).
    wait_comments: waiting::WaitComments,
    /// The quarantine's WAITING comments, folded once it is cleared
    /// (OWL-423).
    quarantine_comments: waiting::QuarantineComments,
    /// The ticket whose base check is due and could not run beside other
    /// steps (OWL-418), since when: no other step starts until those in
    /// flight end and that ticket's step has run, for at most
    /// [`BASE_DUE_AT_MOST`].
    base_due: Option<(TicketId, Timestamp)>,
    /// How many steps of a ticket whose check was due have ended.
    base_due_ran: u64,
    /// The tickets whose check's priority was given up, their step unable
    /// to start or past [`BASE_DUE_AT_MOST`]: not given it again from
    /// `base.json` until a step of theirs has run.
    base_due_given_up: BTreeSet<TicketId>,
    /// The tickets whose last step ended held for their base (OWL-431).
    base_held: BTreeSet<TicketId>,
    /// Those a drain's dispatch gave a turn: each one's retry hold is
    /// lifted until a step of its own starts (OWL-431).
    base_turn: BTreeSet<TicketId>,
    /// The strongest stop told, once one is asked (OWL-161).
    stop_told: Option<owlshift_platform::process::Degree>,
    /// The operator's pause last told, while one stands (OWL-438).
    operator_pause: Option<(
        crate::daemon::requests::PauseScope,
        Option<owlshift_contracts::service::Request>,
    )>,
    /// The base heads merge on green read at this pass (OWL-445).
    base_heads: merge::BaseHeads,
    /// The refreshes the overlaps make due (OWL-449).
    refreshes: refresh::Refreshes,
    /// Why each cancel waits, by its ticket's folder, as last warned of
    /// (OWL-474).
    cancel_failing: HashMap<String, String>,
    /// The tickets a cancel's park whose state was not kept holds, with
    /// their ref's progress then: left alone until it moves (OWL-474).
    cancel_held: HashMap<TicketId, Progress>,
}

/// What a correction's or the reviews' read found due at a pass (OWL-445):
/// merge on green waits for anything a person's words may still change,
/// so only an absence read for sure is [`Due::Nothing`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Due {
    /// Read, and nothing is due.
    Nothing,
    /// Due, held, within its quiet window, or not read: nothing ran.
    Waits,
    /// Its run started.
    Ran,
}

/// Why a ticket's next stage step waits, as told once.
#[derive(Clone, Debug, PartialEq, Eq)]
enum StepWait {
    /// These tickets hold the slot's turn (OWL-367).
    Slot(Vec<TicketId>),
    /// A ticket holds what it would touch (OWL-403), its Build's admission
    /// waiting when marked (OWL-448).
    Collision(Collision<String>, bool),
    /// The work in flight outside Owlshift could not be read (OWL-410).
    Unread,
    /// The harness its first run would use is at its usage limit (OWL-404),
    /// told on the ticket (OWL-419).
    Usage(Harness),
    /// The commit its Build is configured at fails its gate, or its check is
    /// due (OWL-418), told on the ticket.
    Base(String),
}

/// What watch knows of one marker: what its [`Record`] says, joined with
/// what watch did and could not write there yet.
#[derive(Debug)]
struct Quarantine {
    /// The marker's text, unique to it.
    marker: String,
    /// The operator was notified of it.
    notified: bool,
    /// The tickets told that they wait for it.
    told: BTreeSet<TicketId>,
    /// Those of `told` the record does not hold yet.
    pending: BTreeSet<TicketId>,
}

/// What a snapshot read: the ticket refs, and the marker's text when one
/// stands.
#[derive(Default)]
struct Snapshot {
    refs: Vec<(TicketId, Result<Stored, String>)>,
    quarantine: Option<String>,
}

/// What watch keeps, per ticket, of the stages a stop left at working
/// (OWL-265, architecture section 5, "The stage while a repair runs").
#[derive(Default)]
struct Stale {
    /// The ticket ref's progress when watch last moved its stage: no stage
    /// is read again, nor a person's working moved, until the ref moves.
    moved: HashMap<TicketId, Progress>,
    /// Until when a move the tracker refused is not tried again.
    held: HashMap<TicketId, Timestamp>,
    /// Why the stage could not be read or moved, as last warned of.
    warned: HashMap<TicketId, String>,
}

impl Stale {
    /// Forgets the tickets `kept` does not name.
    fn retain(&mut self, kept: &[TicketId]) {
        self.moved.retain(|ticket, _| kept.contains(ticket));
        self.held.retain(|ticket, _| kept.contains(ticket));
        self.warned.retain(|ticket, _| kept.contains(ticket));
    }
}

/// Called with each continue's outcome, for the caller to print it and tell
/// the operator when they are the blocker.
pub type Continued<'c> = dyn FnMut(&TicketId, &Result<Delivered, Stop>, &mut EventSink<'_>) + 'c;

impl Watch<'_> {
    /// Passes over the project until `sleep`, called with the interval after
    /// each pass, returns `false`. Its own lines go to `out`; events,
    /// a continue's included, to `sink`.
    pub fn run(
        &self,
        sink: &mut EventSink<'_>,
        out: &mut dyn Write,
        continued: &mut Continued<'_>,
        sleep: &mut dyn FnMut(Duration) -> bool,
    ) {
        self.run_passes(false, None, sink, out, continued, sleep);
    }

    /// `owlshift watch --drain` (OWL-250): [`Self::run`], each pass then
    /// taking admitted tickets in from the ready column and dispatching one
    /// at a time. The caller has checked [`drain_refusal`].
    pub fn run_drain(
        &self,
        sink: &mut EventSink<'_>,
        out: &mut dyn Write,
        continued: &mut Continued<'_>,
        sleep: &mut dyn FnMut(Duration) -> bool,
    ) {
        self.run_passes(true, None, sink, out, continued, sleep);
    }

    /// [`Self::run`], or [`Self::run_drain`] when `draining`, each step
    /// that may run an agent or the gate run in a child that `children`
    /// starts (OWL-399): its lines printed on `out` after its ticket, its
    /// outcome reported by the child, so `continued` is called for none.
    pub fn run_with_children(
        &self,
        draining: bool,
        children: &Children<'_>,
        sink: &mut EventSink<'_>,
        out: &mut dyn Write,
        continued: &mut Continued<'_>,
        sleep: &mut dyn FnMut(Duration) -> bool,
    ) {
        self.run_passes(draining, Some(children), sink, out, continued, sleep);
    }

    fn run_passes(
        &self,
        draining: bool,
        children: Option<&Children<'_>>,
        sink: &mut EventSink<'_>,
        out: &mut dyn Write,
        continued: &mut Continued<'_>,
        sleep: &mut dyn FnMut(Duration) -> bool,
    ) {
        let mut memory = Memory {
            children,
            started_ms: Timestamp::now().as_millisecond(),
            ..Memory::default()
        };
        self.adopt_leftovers(&mut memory, sink);
        loop {
            // Asked to stop (OWL-161), by a signal or the stop file, a file
            // found at the start included: no further pass.
            if let Some(asked) = self.stopping() {
                self.stop(&mut memory, sink, out, asked);
                return;
            }
            memory.stepped = false;
            // A check left pending in `base.json`, by a watch before this one
            // or by a person's command, gets its priority too (OWL-418).
            if memory.base_due.is_none() {
                memory.base_due = self.pending_due(&memory);
            }
            // A check due whose ticket's step did not start in a pass with
            // nothing else in flight, held back for another reason, gives
            // up its priority, so it never holds the project (OWL-418).
            let due = memory
                .base_due
                .clone()
                .filter(|_| memory.running.is_empty())
                .map(|(ticket, _)| (ticket, memory.base_due_ran));
            // An operator's pause, told as the pass starts (OWL-438): the
            // pass reads on, and no step starts.
            let paused = self.operator_paused(&mut memory, sink, out);
            self.pass(&mut memory, sink, out, continued);
            if let Some(asked) = self.stopping() {
                self.stop(&mut memory, sink, out, asked);
                return;
            }
            if draining {
                self.drain(&mut memory, sink, out, continued);
            }
            // A pause keeps a check due its priority: no step ran for it.
            if let Some((ticket, ran)) = due
                && !paused
                && memory.base_due_ran == ran
                && !memory.running.iter().any(|step| *step.ticket() == ticket)
            {
                memory.base_due = None;
                memory.base_due_given_up.insert(ticket);
            }
            // The interval is only between idle passes (OWL-367): after a
            // step, the next stage step is due at once. While steps'
            // children run, the wait ends with the first that ends
            // (OWL-401).
            let interval = if memory.stepped {
                Duration::ZERO
            } else {
                self.interval
            };
            let interval = self.wait_for_children(&mut memory, sink, out, interval);
            if !sleep(interval) {
                self.wait_for_all(&mut memory, sink, out);
                return;
            }
        }
    }

    fn pass(
        &self,
        memory: &mut Memory,
        sink: &mut EventSink<'_>,
        out: &mut dyn Write,
        continued: &mut Continued<'_>,
    ) {
        // The steps whose children ended since, first, whatever the pass
        // finds (OWL-401).
        self.reap(memory, sink, out);
        // The work outside Owlshift is read again at each pass (OWL-410),
        // and the base heads of merge on green (OWL-445).
        memory.outside.new_pass();
        memory.base_heads.clear();
        let read = match self.snapshot(memory, sink) {
            Ok(Snapshot {
                quarantine: Some(marker),
                refs,
            }) => return self.quarantined(memory, sink, out, continued, marker, refs),
            Ok(snapshot) => {
                // None stands: its WAITING comments are folded (OWL-423).
                memory.quarantine_comments.fold_all(&mut self.board(sink));
                snapshot.refs
            }
            Err(project) => return self.note(memory, out, project),
        };
        // A harness's usage limit, told when it changed (OWL-404).
        self.read_usage(memory, sink, out);
        // The cancels standing (OWL-474): each ticket whose step does not
        // run is parked, or its cancel dropped, before anything else of it.
        let canceled = self.cancels(memory, sink, out, &read);
        let mut waiting = Vec::new();
        let mut following = Vec::new();
        let mut resumable = Vec::new();
        // A ticket whose ref cannot be read now keeps its hold: only one
        // read and found not waiting, or gone from the list, loses it.
        let mut unread = Vec::new();
        // The tickets whose ref holds the slot (OWL-367), one that does not
        // read among the unread, which may hold it.
        let mut holding = Vec::new();
        // Which of them a collision holds back before their next stage step
        // (OWL-403), from what the refs read hold and the work in flight
        // outside Owlshift (OWL-410).
        let outside = self.step_holders(memory, sink, out, &read);
        // A Build held back this pass, by watch's own hold or its base,
        // takes nothing yet for the Builds after it (OWL-448).
        let now = (self.on_demand.clock)();
        // The switch at the default branch's head: each step reads it again
        // at its own base (OWL-449).
        let overlap = if read.is_empty() {
            Overlap::Exclusive
        } else {
            self.overlap_switch(memory)
        };
        let collisions = self.collisions(&read, outside.as_ref(), overlap, &|ticket| {
            memory.holds.stands(ticket, now) || memory.base_held.contains(ticket)
        });
        memory.collisions = collisions;
        // Which followed pull requests an overlap's base refreshes (OWL-449).
        self.overlap_refreshes(memory, &read);
        // The tickets whose step's child runs (OWL-401): each counts in the
        // slot's turn as its ref says, and is left alone until it ends; so
        // is each a cancel stood on at this pass (OWL-474).
        let busy: Vec<TicketId> = memory
            .running
            .iter()
            .map(|running| running.ticket().clone())
            .chain(canceled)
            .collect();
        for (ticket, stored) in read {
            let state = stored.and_then(|stored| {
                TicketState::try_from(&stored.record.state)
                    .map(|state| (stored, state))
                    .map_err(|e| format!("its ticket ref keeps no valid state: {e:?}"))
            });
            if let Ok((stored, state)) = &state
                && schedule::holds_slot(state)
            {
                holding.push((ticket.clone(), held::dispatched(stored)));
            }
            if busy.contains(&ticket) {
                continue;
            }
            match state {
                // A ticket parked at Watch is read for an end alone (OWL-243),
                // its comments never: only a waiting ticket's are.
                Ok((stored, state))
                    if watched(state.status()) || outlived(state.status(), &stored) =>
                {
                    let outlived = outlived(state.status(), &stored);
                    // Only a review loop's ref at Build or Verify is both
                    // outlived and resumable (OWL-340): its pull request is
                    // read first, and a loop it did not end is resumed.
                    let resume = if outlived {
                        resumable_stage(&state)
                    } else {
                        None
                    };
                    waiting.push((ticket, stored, state.status(), outlived, resume));
                }
                Ok((stored, state)) if followed(state.status(), &stored) => {
                    memory.failing.remove(&ticket);
                    following.push((ticket, stored));
                }
                // Its failing read is cleared once its stage reads (OWL-273).
                Ok((stored, state)) if let Some(shown) = resumable_stage(&state) => {
                    resumable.push((ticket, stored, shown, state.stage()));
                }
                Ok(_) => {
                    memory.failing.remove(&ticket);
                }
                Err(reason) => {
                    self.unreadable(memory, sink, &ticket, reason);
                    unread.push(ticket);
                }
            }
        }
        // A parked ticket keeps no hold: only a person's `continue` restarts
        // it.
        let ids: Vec<TicketId> = waiting
            .iter()
            .filter(|(_, _, status, _, _)| watched(*status))
            .map(|(ticket, _, _, _, _)| ticket.clone())
            .collect();
        // A waiting or parked ticket whose pull request is read keeps the
        // forge failure warned of (OWL-242, OWL-243).
        let read: Vec<TicketId> = waiting
            .iter()
            .filter(|(_, _, _, outlived, _)| *outlived)
            .map(|(ticket, _, _, _, _)| ticket.clone())
            .collect();
        let followed: Vec<TicketId> = following.iter().map(|(ticket, _)| ticket.clone()).collect();
        // The tickets watch may resume: those left at a stage, and a review
        // loop's, whose pull request is read first (OWL-340).
        let left: Vec<TicketId> = resumable
            .iter()
            .map(|(ticket, _, _, _)| ticket.clone())
            .chain(
                waiting
                    .iter()
                    .filter(|(_, _, _, _, resume)| resume.is_some())
                    .map(|(ticket, _, _, _, _)| ticket.clone()),
            )
            .collect();
        // A followed ticket keeps its repair's hold too: otherwise a repair
        // that left the ref as it was, or a round under it whose state was
        // not kept (OWL-211), would run again at the next pass.
        // The drain's intakes and dispatches are held there too (OWL-250),
        // and the resumes of tickets left at Intake (OWL-273).
        // A ticket whose step's child runs keeps its hold (OWL-401).
        memory.holds.retain(
            &[
                &ids[..],
                &followed[..],
                &left[..],
                &unread[..],
                &busy[..],
                &memory.drain.listed[..],
            ]
            .concat(),
        );
        // A predecessor's holds, once the drain's tickets were listed or
        // not (OWL-399).
        for (ticket, seen, hold) in std::mem::take(&mut memory.adopted) {
            memory.holds.restore(&ticket, seen, &hold);
        }
        memory.elsewhere.retain(|ticket, _| {
            left.contains(ticket) || unread.contains(ticket) || busy.contains(ticket)
        });
        memory
            .following
            .retain(&[&followed[..], &read[..], &unread[..], &busy[..]].concat());
        memory
            .stale
            .retain(&[&followed[..], &read[..], &unread[..], &busy[..]].concat());
        // The slot's turn for this pass's stage steps (OWL-367), the first
        // holders by dispatch time up to the project's capacity (OWL-401),
        // among those no collision holds back (OWL-403): one that waits for
        // a ticket that waits for the turn would hold both.
        let free: Vec<(TicketId, Dispatched)> = holding
            .iter()
            .filter(|(ticket, _)| !memory.collisions.contains_key(ticket))
            .cloned()
            .collect();
        memory.turn = Some(schedule::slot_turn(&free, &unread, self.capacity()));
        memory.holding = (free, unread.clone());
        let holding: Vec<TicketId> = holding.into_iter().map(|(ticket, _)| ticket).collect();
        memory.resumed.retain(|ticket, _| holding.contains(ticket));
        memory
            .waits
            .retain(|ticket, _| holding.contains(ticket) || followed.contains(ticket));
        // A ticket that left the tickets in flight no longer waits: its
        // WAITING comment is folded (OWL-419). One unread or whose step runs
        // keeps it until it reads or its step ends.
        let kept = [&holding[..], &followed[..], &unread[..], &busy[..]].concat();
        memory.wait_comments.retain(&mut self.board(sink), &kept);
        memory.base_held.retain(|ticket| kept.contains(ticket));
        memory.base_turn.retain(|ticket| kept.contains(ticket));
        self.note(
            memory,
            out,
            Project::Watching {
                waiting: ids,
                followed,
            },
        );
        // The steps of tickets already dispatched, by dispatch time
        // (OWL-401; architecture section 5, "Order"): repairs at Watch, then
        // answer checks, then resumes. Finishing comes before starting: the
        // drain's intake and dispatches come after them.
        let first = |a: &TicketId, at: &Stored, b: &TicketId, bt: &Stored| {
            held::dispatched(at)
                .cmp(&held::dispatched(bt))
                .then_with(|| schedule::natural_cmp(a.as_str(), b.as_str()))
        };
        following.sort_by(|(a, at), (b, bt)| first(a, at, b, bt));
        waiting.sort_by(|(a, at, ..), (b, bt, ..)| first(a, at, b, bt));
        resumable.sort_by(|(a, at, ..), (b, bt, ..)| first(a, at, b, bt));
        for (ticket, stored) in following {
            // A repair this pass decides moves the stage to working again
            // once it starts, and back once it ends.
            self.unstick(
                memory,
                sink,
                out,
                &ticket,
                &stored,
                Status::Active(Stage::Watch),
            );
            self.follow(memory, sink, out, continued, &ticket, &stored);
        }
        for (ticket, stored, status, outlived, resume) in waiting {
            // A stage a stop left at working first (OWL-265): an end then
            // moves the stage it shows to review, which it would not do
            // from working, so the two may make two moves in one pass. A
            // review loop working at Build or Verify shows working by right
            // (OWL-307): its stage is left as it is.
            if outlived {
                self.unstick(memory, sink, out, &ticket, &stored, status);
            }
            // A pass whose read ended the following reads no comment: an
            // answer posted with the merge starts nothing.
            if outlived && self.close_outlived(memory, sink, out, &ticket, &stored, status) {
                continue;
            }
            if watched(status) {
                self.look(memory, sink, out, continued, &ticket, &stored, false);
            } else if let Some(shown) = resume {
                self.resume_stage(
                    memory,
                    sink,
                    out,
                    continued,
                    &ticket,
                    &stored,
                    shown,
                    status.stage(),
                );
            }
        }
        for (ticket, stored, shown, stage) in resumable {
            self.resume_stage(memory, sink, out, continued, &ticket, &stored, shown, stage);
        }
    }

    /// A pass while the marker `marker` quarantines the project (OWL-368):
    /// its line once, then each waiting ticket whose answer counts is told
    /// that it waits, once per marker, instead of continued; so is each
    /// ticket left at a stage watch would resume, whose step does not run,
    /// while the tracker still shows that stage (OWL-456). Nothing else
    /// runs: no continue, follow, stage move or resume.
    fn quarantined(
        &self,
        memory: &mut Memory,
        sink: &mut EventSink<'_>,
        out: &mut dyn Write,
        continued: &mut Continued<'_>,
        marker: String,
        refs: Vec<(TicketId, Result<Stored, String>)>,
    ) {
        self.note(memory, out, Project::Quarantined(marker));
        // A ticket whose step's child still runs is told once it has
        // stopped, at the run's boundary, and is left at its stage.
        let busy: Vec<TicketId> = memory
            .running
            .iter()
            .map(|running| running.ticket().clone())
            .collect();
        for (ticket, stored) in refs {
            // A ticket told already costs no read of its comments.
            let told = memory
                .quarantine
                .as_ref()
                .is_some_and(|known| known.told.contains(&ticket));
            let (false, Ok(stored)) = (told, stored) else {
                continue;
            };
            let Ok(state) = TicketState::try_from(&stored.record.state) else {
                continue;
            };
            if watched(state.status()) {
                self.look(memory, sink, out, continued, &ticket, &stored, true);
            } else if let Some(shown) = resumable_stage(&state)
                && !busy.contains(&ticket)
            {
                self.tell_stage(memory, sink, out, &ticket, shown);
            }
        }
    }

    /// Tells `ticket`, left at a stage the tracker must show as `shown` for
    /// watch to resume it, that it waits for the quarantine (OWL-456): only
    /// while the tracker shows it so, since a ticket a person moved
    /// elsewhere would not go on by itself.
    fn tell_stage(
        &self,
        memory: &mut Memory,
        sink: &mut EventSink<'_>,
        out: &mut dyn Write,
        ticket: &TicketId,
        shown: VisibleStage,
    ) {
        let on_demand = self.on_demand;
        let found = match on_demand.tracker.stage(ticket) {
            Ok(found) => found,
            Err(e) => {
                let reason = format!("reading its stage: {e}");
                return self.unreadable(memory, sink, ticket, reason);
            }
        };
        memory.failing.remove(ticket);
        if found == shown.state(&on_demand.config.tracker.states) {
            self.tell(memory, sink, out, ticket, Held::Stage);
        }
    }

    /// Tells `ticket`, held up as `held` says, that it waits for the
    /// quarantine (OWL-368): once per marker, and never a ticket of another
    /// team, which only a ref a breaking run planted could name. The record
    /// keeps it at once when the project's lock is free, else at the next
    /// snapshot.
    fn tell(
        &self,
        memory: &mut Memory,
        sink: &mut EventSink<'_>,
        out: &mut dyn Write,
        ticket: &TicketId,
        held: Held,
    ) {
        let on_demand = self.on_demand;
        let Some(known) = memory.quarantine.as_mut() else {
            return;
        };
        if known.told.contains(ticket) || on_demand::check_team(on_demand.config, ticket).is_err() {
            return;
        }
        if let Some(comment) = quarantine::tell(on_demand.tracker, ticket, held, sink) {
            memory
                .quarantine_comments
                .posted(&mut self.board(sink), ticket, &comment);
        }
        known.told.insert(ticket.clone());
        let told = Record {
            notified: false,
            told: BTreeSet::from([ticket.clone()]),
        };
        let dirs = on_demand.dirs;
        // Watch never waits for a lock (OWL-414): busy, it is kept for later.
        let kept = matches!(
            dirs.try_lock(),
            Ok(Some(lock)) if told.merge_into(dirs, &lock, &known.marker).is_ok()
        );
        if !kept {
            known.pending.insert(ticket.clone());
        }
        self.line(
            out,
            &format!("{ticket} waits for the quarantine to be cleared: said so on the ticket"),
        );
    }

    /// Under the snapshot's lock `lock`, what watch knows of the marker
    /// `marker`: what its record says, what watch could not write there
    /// before, and the operator notified of it if no command did.
    fn keep_quarantine(
        &self,
        memory: &mut Memory,
        lock: &ProjectLock,
        marker: &str,
        sink: &mut EventSink<'_>,
    ) {
        let dirs = self.on_demand.dirs;
        if memory
            .quarantine
            .as_ref()
            .is_none_or(|known| known.marker != marker)
        {
            memory.quarantine = Some(Quarantine {
                marker: marker.to_owned(),
                notified: false,
                told: BTreeSet::new(),
                pending: BTreeSet::new(),
            });
        }
        let Some(known) = memory.quarantine.as_mut() else {
            return;
        };
        let record = Record::read(dirs, marker);
        known.notified |= record.notified;
        known.told.extend(record.told);
        if !known.pending.is_empty() {
            let pending = Record {
                notified: false,
                told: known.pending.clone(),
            };
            if pending.merge_into(dirs, lock, marker).is_ok() {
                known.pending.clear();
            }
        }
        if !known.notified {
            known.notified =
                quarantine::notify_found(self.on_demand.notifier, dirs, lock, marker, sink);
        }
    }

    /// One followed pull request (OWL-209): its read and what it decides,
    /// the ticket ref written with what it changes, then its events. A fix
    /// run, a rebase run or a park it decides goes to
    /// [`OnDemand::repair_seen`] (OWL-112, OWL-210), held as a continue is
    /// ([`Holds`]).
    fn follow(
        &self,
        memory: &mut Memory,
        sink: &mut EventSink<'_>,
        out: &mut dyn Write,
        continued: &mut Continued<'_>,
        ticket: &TicketId,
        stored: &Stored,
    ) {
        let on_demand = self.on_demand;
        let Some(delivery) = &stored.record.delivery else {
            return;
        };
        let now = (on_demand.clock)();
        let overlap = memory.refreshes.due(ticket);
        let (look, warnings) =
            memory
                .following
                .look(on_demand.forge, ticket, delivery, now, overlap.as_deref());
        for warning in warnings {
            sink.emit(ticket, None, EventKind::Warning, warning);
        }
        let seen = match look {
            Look::Read(seen) => seen,
            Look::Paused { .. } | Look::Unread { .. } => return,
        };
        let mut replaced = None;
        // The runner's merge (OWL-445): told once before the end is kept.
        let runner_merged = delivery
            .merging
            .clone()
            .filter(|_| matches!(seen.decision, Decision::End(EndReason::Merged)));
        let change =
            follow::changed(&stored.record, PIPELINE, &seen, now).and_then(|change| match change {
                Some(changed) => {
                    replaced = changed.replaced;
                    match &runner_merged {
                        Some(merging) => self.write_merged(
                            memory,
                            sink,
                            ticket,
                            stored,
                            merging,
                            &changed.record,
                        ),
                        None => self.write_seen(ticket, stored, &changed.record),
                    }
                }
                None => Ok(Some(stored.at())),
            });
        let at = match change {
            Ok(Some(at)) => at,
            // Busy, or the ref moved: the next pass reads it again.
            Ok(None) => return,
            Err(reason) => {
                let reason = format!("keeping what watch read of its pull request: {reason}");
                return self.unreadable(memory, sink, ticket, reason);
            }
        };
        // Told once the ref keeps it (OWL-239).
        if let Some(replaced) = &replaced {
            on_demand.replaced(ticket, replaced, delivery.pushed.as_deref(), sink);
        }
        // A push a crash cut short ends as its run would have: an adoption
        // always changes the ref, which keeps it now (OWL-259).
        if matches!(seen.decision, Decision::Follow { adopt: true, .. }) {
            on_demand.review_after_working(ticket, sink);
        }
        if let (Some(merging), Some(pr)) = (&runner_merged, &delivery.pull_request) {
            crate::merge::merged_event(ticket, pr.number, merging, None, sink);
        }
        if let Some(event) = memory.following.events(ticket, &seen) {
            sink.emit(ticket, None, EventKind::Decision, event);
            self.line(out, &format!("{ticket}: {seen}"));
        }
        // A head moved since the read ends the pass: the next read decides.
        let Some(at) =
            on_demand.post_owed_report(ticket, stored, &seen, at, &mut memory.following, sink)
        else {
            return;
        };
        if !matches!(
            seen.decision,
            Decision::Follow {
                action: Action::Fix { .. }
                    | Action::Park
                    | Action::Rebase { .. }
                    | Action::RebaseFailed { .. },
                ..
            }
        ) {
            // A read that asks for no repair leaves room for a correction
            // (OWL-146), then for the reviews on the pull request (OWL-307),
            // read once the pass ran no correction; then, on a green read
            // with neither due, for merge on green (OWL-445).
            if matches!(
                seen.decision,
                Decision::Follow {
                    adopt: false,
                    action: Action::Green | Action::WaitChecks | Action::WaitBaseRed,
                }
            ) {
                let corrected =
                    self.correct(memory, sink, out, continued, ticket, stored, at.clone());
                let reviewed = if corrected == Due::Ran {
                    Due::Ran
                } else {
                    self.review(memory, sink, out, continued, ticket, stored, at.clone())
                };
                if (corrected, reviewed) == (Due::Nothing, Due::Nothing)
                    && seen.decision
                        == (Decision::Follow {
                            adopt: false,
                            action: Action::Green,
                        })
                {
                    self.merge(memory, sink, out, continued, ticket, stored, at, &seen);
                }
            }
            return;
        }
        let held = Seen::at(at, None);
        let failing_for_ms = seen
            .failing_for
            .map(|failing| u64::try_from(failing.as_millis()).unwrap_or(u64::MAX));
        // A park runs no model: a usage limit never holds it (OWL-404).
        let first = match &seen.decision {
            Decision::Follow {
                action: Action::Park,
                ..
            } => None,
            Decision::Follow {
                action: Action::Rebase { .. } | Action::RebaseFailed { .. },
                ..
            } => Some(Role::Rebase),
            _ => Some(Role::Build),
        };
        self.held_run(
            memory,
            sink,
            out,
            continued,
            ticket,
            held,
            (
                StepKind::Repair {
                    failing_for_ms,
                    merge_refresh: false,
                },
                first,
            ),
            None,
        );
    }

    /// Runs the step `kind` watch decided for `ticket` from `held`, unless
    /// a hold or a pause keeps it back ([`Holds::admit`]), with watch's
    /// memory of the pull requests it follows and, for a review loop, the
    /// reviews watch read ([`Self::step`]): a step that found the ref moved
    /// ran nothing. Its outcome is told, then what it did is held as a
    /// continue is ([`Holds::after`]), from the ticket ref read after it.
    /// A step whose first run, of `first`, has no harness for a usage limit
    /// does not start (OWL-404). Returns whether it ran.
    #[allow(clippy::too_many_arguments)]
    fn held_run(
        &self,
        memory: &mut Memory,
        sink: &mut EventSink<'_>,
        out: &mut dyn Write,
        continued: &mut Continued<'_>,
        ticket: &TicketId,
        held: Seen,
        (kind, first): (StepKind, Option<Role>),
        reviews: Option<ReviewsRead>,
    ) -> bool {
        let on_demand = self.on_demand;
        let now = (on_demand.clock)();
        // Under an operator's pause no repair, correction or review loop
        // starts (OWL-438).
        if self.operator_paused(memory, sink, out)
            || memory.holds.admit(ticket, &held, now) != Admit::Go
            || self.usage_waits(memory, sink, out, first)
        {
            return false;
        }
        let envelope = self.envelope(memory, ticket, kind, &held);
        let then = Then::Held { seen: held };
        self.step(memory, sink, out, continued, envelope, reviews, None, then)
            .is_none_or(|end| end != StepEnd::Stale)
    }

    /// What a step of [`Self::held_run`] that ended `end` left (since
    /// OWL-401 applied once its child ends): held as a continue is, from the
    /// ticket ref read after it.
    fn held_after(
        &self,
        memory: &mut Memory,
        sink: &mut EventSink<'_>,
        out: &mut dyn Write,
        ticket: &TicketId,
        held: Seen,
        end: &StepEnd,
    ) {
        if *end == StepEnd::Stale {
            return;
        }
        // A review loop's ref at Build holds the slot from its start, run or
        // refused after it (OWL-367).
        let after = self.read_after(ticket);
        let progress = after.as_ref().map(Stored::progress);
        let holds_now = after.as_ref().is_some_and(holds);
        self.after(
            memory,
            sink,
            out,
            ticket,
            held,
            end,
            progress.as_ref(),
            holds_now,
        );
    }

    /// The envelope of a step of `kind` of `ticket` from `seen`, what watch
    /// read (OWL-399): with the generation of the carry tied to that ref and
    /// watch's memory of the ticket's pull request.
    fn envelope(
        &self,
        memory: &mut Memory,
        ticket: &TicketId,
        kind: StepKind,
        seen: &Seen,
    ) -> StepEnvelope {
        memory.attempts += 1;
        StepEnvelope {
            format: Format,
            ticket: ticket.clone(),
            attempt: format!(
                "{}-{}-{}",
                memory.started_ms,
                std::process::id(),
                memory.attempts
            ),
            step: kind,
            commit: (!seen.commit.is_empty()).then(|| seen.commit.clone()),
            progress: seen.progress.0.clone(),
            answer: seen.answer,
            generation: step::carry_generation(self.on_demand.dirs, ticket, &seen.commit),
            kept: memory.following.kept(ticket),
        }
    }

    /// Reads the outcome records a predecessor left (OWL-399) and keeps the
    /// hold each calls for while its ticket's ref is where the record says,
    /// for the first pass to apply; every record is removed. One that does
    /// not read is a `step_unreported` warning of the project.
    fn adopt_leftovers(&self, memory: &mut Memory, sink: &mut EventSink<'_>) {
        let on_demand = self.on_demand;
        // What a predecessor handed its dispatches and they never took
        // (OWL-401).
        step::remove_outside_leftovers(on_demand.dirs);
        for path in step::leftovers(on_demand.dirs) {
            let record = match step::take_outcome(&path) {
                Ok(Some(record)) => record,
                Ok(None) => continue,
                Err(reason) => {
                    sink.emit_project(
                        None,
                        EventKind::Warning,
                        data([
                            ("what", json!("step_unreported")),
                            (
                                "reason",
                                json!(format!(
                                    "an outcome record an earlier watch left does not read: {reason}"
                                )),
                            ),
                        ]),
                    );
                    continue;
                }
            };
            // No dedicated checkout yet: no ticket has a ref.
            let checkout = on_demand.dirs.checkout();
            let current = if fs::symlink_metadata(&checkout).is_ok() {
                ticket_ref::read(&on_demand.executor.git, &checkout, &record.ticket)
            } else {
                Ok(None)
            };
            let at_ref = match (&record.after, current) {
                (RefAfter::Present { commit, .. }, Ok(Some(stored))) => stored.commit == *commit,
                (RefAfter::Absent, Ok(None)) => true,
                _ => false,
            };
            if at_ref && let Some((seen, hold)) = step::hold_of(&record) {
                memory.adopted.push((record.ticket, seen, hold));
            }
        }
    }

    /// The reviews on a followed pull request whose read asked for no
    /// repair (OWL-307, architecture section 5, "Review comments on the
    /// pull request"): one read of them, at `at`, the ticket ref after the
    /// look. Those due ([`pr_reviews::due`], by `[forge].reviewers` as
    /// watch loaded it) that the ticket's decider wrote start a review loop
    /// ([`OnDemand::review_seen`]), held as a continue is, the hold lifted
    /// by a newer review. The decider is the ticket's assignee, read only
    /// once a review is due; a ticket whose zones' owners decide is left to
    /// the loop, which reads them under the lock with the reviewers at the
    /// base commit. A delivery made before OWL-307 is dated first, and no
    /// review is due at that pass. Returns what it found due (OWL-445):
    /// nothing only when the reviews read and none is due, or the project
    /// lists no reviewer.
    #[allow(clippy::too_many_arguments)]
    fn review(
        &self,
        memory: &mut Memory,
        sink: &mut EventSink<'_>,
        out: &mut dyn Write,
        continued: &mut Continued<'_>,
        ticket: &TicketId,
        stored: &Stored,
        at: RefAt,
    ) -> Due {
        let on_demand = self.on_demand;
        let Some(forge) = &on_demand.config.forge else {
            return Due::Nothing;
        };
        if forge.reviewers.is_empty() {
            return Due::Nothing;
        }
        let Some(delivery) = &stored.record.delivery else {
            return Due::Waits;
        };
        let Some(pr) = &delivery.pull_request else {
            return Due::Waits;
        };
        let now = (on_demand.clock)();
        let Some(delivered_at) = delivery.delivered_at else {
            if let Err(reason) = self.date_delivery(ticket, &at.commit, now) {
                self.unreadable(
                    memory,
                    sink,
                    ticket,
                    format!("dating its delivery: {reason}"),
                );
            }
            return Due::Waits;
        };
        let (read, warnings) = memory
            .following
            .reviews(on_demand.forge, ticket, pr.number, now);
        for warning in warnings {
            sink.emit(ticket, None, EventKind::Warning, warning);
        }
        let Some(read) = read else {
            return Due::Waits;
        };
        let answered = stored
            .record
            .review
            .as_ref()
            .map(|review| review.answered_reviews.clone())
            .unwrap_or_default();
        let due = pr_reviews::due(&read.reviews, &forge.reviewers, delivered_at, &answered);
        if due.is_empty() {
            return Due::Nothing;
        }
        let decider = match on_demand.tracker.ticket(ticket) {
            Ok(found) => on_demand::known_decider(&found),
            Err(e) => {
                let reason = format!("reading the ticket: {e}");
                self.unreadable(memory, sink, ticket, reason);
                return Due::Waits;
            }
        };
        if decider
            .as_ref()
            .is_some_and(|decider| !pr_reviews::instructed(&due, &decider.id))
        {
            return Due::Nothing;
        }
        // A review loop's start is a repair at Watch, which needs no room in
        // the project's capacity (OWL-401; architecture section 5, "Three
        // counts"): its Build then holds the slot, and its next step waits
        // for its turn by dispatch time.
        let held = Seen::at(at, pr_reviews::newest(&due));
        if self.held_run(
            memory,
            sink,
            out,
            continued,
            ticket,
            held,
            (StepKind::Review, Some(Role::Build)),
            Some(read),
        ) {
            Due::Ran
        } else {
            Due::Waits
        }
    }

    /// Dates the delivery of `ticket`, whose ref is at `seen`, at `now`
    /// (OWL-307), under the project's lock: the delivery was made before
    /// OWL-307 kept its date, so the reviews already on its pull request
    /// were never this delivery's to answer. Nothing is written when the
    /// project is busy or the ref moved.
    fn date_delivery(&self, ticket: &TicketId, seen: &str, now: Timestamp) -> Result<(), String> {
        let Some((_project, ticket_lock)) = self.locked_at(ticket, seen)? else {
            return Ok(());
        };
        let git = &self.on_demand.executor.git;
        let checkout = self.on_demand.dirs.checkout();
        let Some(current) = ticket_ref::read(git, &checkout, ticket)? else {
            return Ok(());
        };
        let mut record = current.record;
        let Some(delivery) = record.delivery.as_mut() else {
            return Ok(());
        };
        delivery.delivered_at = Some(now);
        self.write_ref(&ticket_lock, ticket, &record, ProposalsFile::Keep, seen)
            .map(|_| ())
    }

    /// Writes `record` as `ticket`'s ref, from `seen`, through its run file
    /// (OWL-400), under the ticket's lock watch holds: a breach found there
    /// quarantines the project.
    fn write_ref(
        &self,
        ticket_lock: &TicketLock,
        ticket: &TicketId,
        record: &TicketRecord,
        proposals: ProposalsFile<'_>,
        seen: &str,
    ) -> Result<String, String> {
        let on_demand = self.on_demand;
        ticket_run::write_ref(
            &on_demand.executor.git,
            on_demand.dirs,
            &on_demand.dirs.checkout(),
            ticket_lock,
            ticket,
            record,
            proposals,
            Some(seen),
        )
        .map_err(|fault| on_demand.ref_fault(ticket, fault))
    }

    /// A ticket in `status`, its round waiting at Watch (OWL-242) or parked
    /// there (OWL-243): its pull request read for an end alone, never its
    /// base. An end moves the ticket's stage to review while it shows the
    /// state `status` set, then closes the round or ends the park in the
    /// ticket ref, with the following's end, from the ref the snapshot read;
    /// then its event, and the owed report dropped. A stage or ref that
    /// cannot be read or written is warned of once and leaves the ticket to
    /// the next pass. Returns whether the read ended the following, which no
    /// comment read in this pass outweighs.
    fn close_outlived(
        &self,
        memory: &mut Memory,
        sink: &mut EventSink<'_>,
        out: &mut dyn Write,
        ticket: &TicketId,
        stored: &Stored,
        status: Status,
    ) -> bool {
        let on_demand = self.on_demand;
        let Some(delivery) = &stored.record.delivery else {
            return false;
        };
        let now = (on_demand.clock)();
        let (look, warnings) = memory
            .following
            .look_ending(on_demand.forge, ticket, delivery, now);
        for warning in warnings {
            sink.emit(ticket, None, EventKind::Warning, warning);
        }
        // A review loop's ticket is read for its end, and for a push of its
        // own a crash cut short, which ends the loop as its command would
        // have (OWL-307).
        let in_loop = status.stage() != Stage::Watch;
        let seen = match look {
            Look::Read(seen) if matches!(seen.decision, Decision::End(_)) => seen,
            Look::Read(seen)
                if in_loop && matches!(seen.decision, Decision::Follow { adopt: true, .. }) =>
            {
                seen
            }
            _ => return false,
        };
        let change = if in_loop {
            self.close_loop(sink, ticket, stored, status, &seen, now)
        } else {
            // The stage first: a round closed or a park ended in the ref is
            // no longer read, so a stage left at needs input would stay
            // there.
            if let Err(reason) = on_demand.review_after_end(ticket, status, sink) {
                self.unreadable(memory, sink, ticket, reason);
                return true;
            }
            follow::changed(&stored.record, PIPELINE, &seen, now).and_then(|change| match change {
                Some(changed) => self.write_seen(ticket, stored, &changed.record),
                None => Ok(None),
            })
        };
        let at = match change {
            Ok(Some(at)) => at,
            // Busy, or the ref moved: the next pass reads it again.
            Ok(None) => return true,
            Err(reason) => {
                let doing = if in_loop {
                    "ending its review loop"
                } else if matches!(status, Status::Parked { .. }) {
                    "ending its park"
                } else {
                    "closing its round"
                };
                self.unreadable(memory, sink, ticket, format!("{doing}: {reason}"));
                return true;
            }
        };
        if let Some(mut event) = memory.following.events(ticket, &seen) {
            let (marks, said) = ended_marks(status, stored.record.state.round);
            event.extend(marks);
            sink.emit(ticket, None, EventKind::Decision, event);
            self.line(out, &format!("{ticket}: {seen}; {said}"));
        }
        // Dropped, the following having ended; posted, a review loop's
        // push adopted.
        on_demand.post_owed_report(ticket, stored, &seen, at, &mut memory.following, sink);
        true
    }

    /// The end of a review loop (OWL-307) that `seen`, a read of its pull
    /// request, decided for a ticket in `status` at Build or Verify: its
    /// following ended, or its own push adopted. Under the project's lock
    /// from the start, since a person's `continue` may be running the loop:
    /// busy, the ref moved or the project is quarantined, nothing is
    /// moved or written (`Ok(None)`), and the next pass reads it again.
    /// Otherwise the stage moves to review, while it shows what `status`
    /// set, then the ref keeps what the read changed; where it is then is
    /// returned.
    #[allow(clippy::too_many_arguments)]
    fn close_loop(
        &self,
        sink: &mut EventSink<'_>,
        ticket: &TicketId,
        stored: &Stored,
        status: Status,
        seen: &follow::Seen,
        now: Timestamp,
    ) -> Result<Option<RefAt>, String> {
        let on_demand = self.on_demand;
        let Some((_project, ticket_lock)) = self.locked_at(ticket, &stored.commit)? else {
            return Ok(None);
        };
        if !matches!(on_demand.dirs.quarantine(), Ok(None)) {
            return Ok(None);
        }
        let Some(changed) = follow::changed(&stored.record, PIPELINE, seen, now)? else {
            return Ok(None);
        };
        if matches!(seen.decision, Decision::End(_)) {
            on_demand.review_after_end(ticket, status, sink)?;
        } else {
            on_demand.review_after_working(ticket, sink);
        }
        let written = self.write_ref(
            &ticket_lock,
            ticket,
            &changed.record,
            ProposalsFile::Keep,
            &stored.commit,
        )?;
        // Told once the ref keeps it (OWL-239).
        if let Some(replaced) = &changed.replaced {
            let pushed = stored
                .record
                .delivery
                .as_ref()
                .and_then(|delivery| delivery.pushed.as_deref());
            on_demand.replaced(ticket, replaced, pushed, sink);
        }
        Ok(Some(RefAt {
            commit: written,
            progress: ticket_ref::progress(&changed.record, stored.proposals.as_ref()),
        }))
    }

    /// A ticket in `status` at Watch whose delivery is followed, its stage
    /// left at working by a stop that did not move it back (OWL-265): a
    /// kill or a crash before any push, a park or a round whose own move
    /// the tracker refused, a move back that failed. Its stage is read
    /// without the lock; one that shows working is moved to the stage
    /// `status` shows only under the lock, so no repair runs, while the
    /// project is not quarantined and the ticket ref is the one the snapshot
    /// read, and only while a second read still shows working. Once moved,
    /// nothing is read again until the ref moves, so a person's own working
    /// is moved at most once; a move the tracker refused is tried again
    /// after [`RETRY_AFTER`]. A read or move that fails is a
    /// `stage_not_moved` warning, once until its reason changes.
    fn unstick(
        &self,
        memory: &mut Memory,
        sink: &mut EventSink<'_>,
        out: &mut dyn Write,
        ticket: &TicketId,
        stored: &Stored,
        status: Status,
    ) {
        let on_demand = self.on_demand;
        // Working is what a review loop at Build or Verify shows (OWL-307).
        let Some(to) = VisibleStage::of(status).filter(|to| *to != VisibleStage::Working) else {
            return;
        };
        let stale = &memory.stale;
        let now = (on_demand.clock)();
        if stale.moved.get(ticket) == Some(&stored.progress())
            || stale.held.get(ticket).is_some_and(|until| now < *until)
        {
            return;
        }
        match on_demand.shows_working(ticket) {
            Ok(true) => {}
            Ok(false) => {
                memory.stale.warned.remove(ticket);
                return;
            }
            Err(reason) => return self.stale_warning(memory, sink, ticket, to, reason),
        }
        let _lock = match self.locked_at(ticket, &stored.commit) {
            Ok(Some(lock)) => lock,
            // Busy, or the ref moved: the next pass reads it again.
            Ok(None) => return,
            Err(reason) => return self.stale_warning(memory, sink, ticket, to, reason),
        };
        // A run stopped since the snapshot may have left the project for a
        // person to look at first.
        if !matches!(on_demand.dirs.quarantine(), Ok(None)) {
            return;
        }
        match on_demand.unstick(ticket, status, sink) {
            Ok(moved) => {
                memory.stale.warned.remove(ticket);
                if moved {
                    memory.stale.moved.insert(ticket.clone(), stored.progress());
                    self.line(
                        out,
                        &format!(
                            "{ticket} showed working with no run working it: shown as {} again",
                            to.state(&on_demand.config.tracker.states)
                        ),
                    );
                }
            }
            Err((to, reason)) => {
                let until = now.checked_add(RETRY_AFTER).unwrap_or(Timestamp::MAX);
                memory.stale.held.insert(ticket.clone(), until);
                self.stale_warning(memory, sink, ticket, to, reason);
            }
        }
    }

    /// Warns that `ticket`'s stage could not be read or moved to `to`, unless
    /// the last warning said the same.
    fn stale_warning(
        &self,
        memory: &mut Memory,
        sink: &mut EventSink<'_>,
        ticket: &TicketId,
        to: VisibleStage,
        reason: String,
    ) {
        if memory.stale.warned.get(ticket) == Some(&reason) {
            return;
        }
        self.on_demand
            .stage_not_moved(ticket, None, to, &reason, sink);
        memory.stale.warned.insert(ticket.clone(), reason);
    }

    /// A followed pull request whose ticket keeps decisions (OWL-146): its
    /// comments, and a correction once the decider's replies to a DECISION
    /// count, by the quiet window as an answer does, held as a continue is
    /// ([`Holds`]). Who the decider is follows the brief's thread: the
    /// decider of the latest ask before a reply, or the ticket's assignee,
    /// read only once a reply is found; a ticket whose zones' owners decide
    /// is left to [`OnDemand::correct_seen`], which reads them under the
    /// lock. Returns what it found due (OWL-445): nothing only when the
    /// ticket keeps no decision or its comments read with no reversal; a
    /// reply within its quiet window, a run held back or a failed read
    /// waits.
    #[allow(clippy::too_many_arguments)]
    fn correct(
        &self,
        memory: &mut Memory,
        sink: &mut EventSink<'_>,
        out: &mut dyn Write,
        continued: &mut Continued<'_>,
        ticket: &TicketId,
        stored: &Stored,
        at: RefAt,
    ) -> Due {
        let on_demand = self.on_demand;
        let questions = &stored.record.questions;
        if questions.decisions.is_empty() {
            return Due::Nothing;
        }
        let comments = match on_demand.tracker.comments(ticket) {
            Ok(comments) => comments,
            Err(e) => {
                let reason = format!("reading its comments: {e}");
                self.unreadable(memory, sink, ticket, reason);
                return Due::Waits;
            }
        };
        if reversal::reversals(&comments, questions, Current::Unresolved).is_empty() {
            return Due::Nothing;
        }
        let decider = match on_demand.tracker.ticket(ticket) {
            Ok(found) => on_demand::known_decider(&found),
            Err(e) => {
                let reason = format!("reading the ticket: {e}");
                self.unreadable(memory, sink, ticket, reason);
                return Due::Waits;
            }
        };
        let current = decider.as_ref().map_or(Current::Unresolved, Current::Known);
        let reversed = reversal::reversals(&comments, questions, current);
        if reversed.is_empty() {
            return Due::Nothing;
        }
        let now = (on_demand.clock)();
        let window = on_demand.config.policy.quiet_window();
        if reversal::counts_at(&reversed, now, window).is_some() {
            return Due::Waits;
        }
        let held = Seen::at(
            at,
            reversal::newest(&reversed).map(|reply| reply.last_edit()),
        );
        if self.held_run(
            memory,
            sink,
            out,
            continued,
            ticket,
            held,
            (StepKind::Correct, Some(Role::Build)),
            None,
        ) {
            Due::Ran
        } else {
            Due::Waits
        }
    }

    /// Writes `record` as the ticket ref of `ticket` under the project's
    /// lock, only while the ref is still at `seen`'s commit, the one the
    /// snapshot read, and returns where the ref is then: `None`, with
    /// nothing written, when the project is busy or the ref moved. The write
    /// keeps `seen`'s proposals.
    fn write_seen(
        &self,
        ticket: &TicketId,
        seen: &Stored,
        record: &TicketRecord,
    ) -> Result<Option<RefAt>, String> {
        let Some((_project, ticket_lock)) = self.locked_at(ticket, &seen.commit)? else {
            return Ok(None);
        };
        let commit = self.write_ref(
            &ticket_lock,
            ticket,
            record,
            ProposalsFile::Keep,
            &seen.commit,
        )?;
        Ok(Some(RefAt {
            commit,
            progress: ticket_ref::progress(record, seen.proposals.as_ref()),
        }))
    }

    /// The project's lock, then the ticket's without waiting (OWL-409),
    /// once the ticket ref read under them is still `seen`, the commit the
    /// snapshot read: `None` when the project or the ticket is busy or the
    /// ref moved.
    fn locked_at(
        &self,
        ticket: &TicketId,
        seen: &str,
    ) -> Result<Option<(ProjectLock, TicketLock)>, String> {
        let dirs = self.on_demand.dirs;
        let lock = match dirs.lock_for_write(ticket) {
            Ok(Some(lock)) => lock,
            Ok(None) => return Ok(None),
            Err(e) => return Err(format!("the project's or the ticket's lock: {e}")),
        };
        let current = ticket_ref::read(&self.on_demand.executor.git, &dirs.checkout(), ticket)?;
        if current.map(|stored| stored.commit).as_deref() != Some(seen) {
            return Ok(None);
        }
        Ok(Some(lock))
    }

    /// The ticket refs, read under the project's lock, and the marker of a
    /// run whose isolation check did not pass, when one stands. While it
    /// does, what watch knows of it is brought up to date under that lock
    /// (`keep_quarantine`, OWL-368), and the refs are read through the
    /// runner's read-only plumbing alone, nothing written: none when they
    /// cannot be. The marker is read first, so that one left once the
    /// dedicated checkout was deleted still stands. No dedicated checkout
    /// and no marker: nothing was asked.
    fn snapshot(&self, memory: &mut Memory, sink: &mut EventSink<'_>) -> Result<Snapshot, Project> {
        let dirs = self.on_demand.dirs;
        let checkout = dirs.checkout();
        let checked_out = fs::symlink_metadata(&checkout).is_ok();
        if !checked_out
            && fs::symlink_metadata(dirs.quarantine_file()).is_err()
            && fs::symlink_metadata(dirs.obligations_dir()).is_err()
        {
            memory.quarantine = None;
            return Ok(Snapshot::default());
        }
        // One try: a busy project skips the pass, watch never waits (OWL-414).
        let lock = match dirs.try_lock() {
            Ok(Some(lock)) => lock,
            Ok(None) => return Err(Project::Busy),
            Err(e) => return Err(Project::Unread(format!("the project's lock: {e}"))),
        };
        // The quarantine, or a run left behind, which becomes it (OWL-400).
        let quarantine = match ticket_run::look(dirs, None) {
            Ok(ticket_run::Look::Quarantined { text, .. }) => Some(text),
            Ok(ticket_run::Look::Clear { .. }) => None,
            Err(e) => return Err(Project::Unread(format!("the project's quarantine: {e}"))),
        };
        match &quarantine {
            Some(marker) => self.keep_quarantine(memory, &lock, marker, sink),
            None => memory.quarantine = None,
        }
        if !checked_out {
            return Ok(Snapshot {
                refs: Vec::new(),
                quarantine,
            });
        }
        let git = &self.on_demand.executor.git;
        let tickets = match ticket_ref::list(git, &checkout) {
            Ok(tickets) => tickets,
            Err(_) if quarantine.is_some() => Vec::new(),
            Err(e) => return Err(Project::Unread(e)),
        };
        let refs = tickets
            .into_iter()
            .map(|ticket| {
                let stored = ticket_ref::read(git, &checkout, &ticket)
                    .and_then(|stored| stored.ok_or_else(|| "its ticket ref is gone".to_owned()));
                (ticket, stored)
            })
            .collect();
        Ok(Snapshot { refs, quarantine })
    }

    /// One waiting ticket: its comments, whether the reply counts, and the
    /// continue when it does; while `quarantined`, telling it that it waits
    /// instead, whatever holds it (OWL-368).
    #[allow(clippy::too_many_arguments)]
    fn look(
        &self,
        memory: &mut Memory,
        sink: &mut EventSink<'_>,
        out: &mut dyn Write,
        continued: &mut Continued<'_>,
        ticket: &TicketId,
        stored: &Stored,
        quarantined: bool,
    ) {
        let on_demand = self.on_demand;
        let questions = &stored.record.questions;
        let Some(latest) = questions.latest() else {
            let reason = "its ticket ref waits for answers but keeps no ask".to_owned();
            return self.unreadable(memory, sink, ticket, reason);
        };
        // The decider recorded on the ask answers it, matched by account as
        // the brief marks it; the name is for messages watch does not print.
        let decider = Person {
            id: latest.decider.account.clone(),
            name: latest.decider.account.clone(),
        };
        let comments = match on_demand.tracker.comments(ticket) {
            Ok(comments) => comments,
            Err(e) => {
                let reason = format!("reading its comments: {e}");
                return self.unreadable(memory, sink, ticket, reason);
            }
        };
        memory.failing.remove(ticket);
        let now = (on_demand.clock)();
        let window = on_demand.config.policy.quiet_window();
        let Some(Readiness::Counts) =
            answer_check::readiness(&comments, &decider, questions, now, window)
        else {
            return;
        };
        if quarantined {
            return self.tell(memory, sink, out, ticket, Held::Answer);
        }
        // The answer's time: a new comment or a thumbs-up (OWL-371) lifts a
        // hold alike.
        let seen = Seen::at(
            stored.at(),
            answer_check::new_answer(&comments, &decider, questions),
        );
        // An answer read under an operator's pause waits for its lift,
        // nothing told on the ticket (OWL-438).
        if self.operator_paused(memory, sink, out) {
            return;
        }
        if memory.holds.admit(ticket, &seen, now) != Admit::Go
            || self.usage_waits(memory, sink, out, Some(Role::AnswerCheck))
        {
            return;
        }
        // The RESUME names the ticket whose turn the slot is (OWL-367).
        // Since OWL-401, the first holder of the turn when the ticket, once
        // resumed, would not be among them by its dispatch time.
        let held_by = {
            let (holding, unread) = &memory.holding;
            let mut with = holding.clone();
            with.push((ticket.clone(), held::dispatched(stored)));
            match schedule::slot_turn(&with, unread, self.capacity()) {
                Turn::Holders(holders) if !holders.contains(ticket) => holders.first().cloned(),
                _ => None,
            }
        };
        // The step records watch's `decision` event once the ticket ref is
        // confirmed under the lock, so the log never says watch continued a
        // ticket it left.
        let kind = StepKind::Answer {
            slot_held_by: held_by,
        };
        let envelope = self.envelope(memory, ticket, kind, &seen);
        let then = Then::Answer { seen, now };
        self.step(memory, sink, out, continued, envelope, None, None, then);
    }

    /// What an answer check that ended `end` left (OWL-367; since OWL-401
    /// applied once its child ends): held as a continue is, from the ticket
    /// ref read after it, and told.
    #[allow(clippy::too_many_arguments)]
    fn answered(
        &self,
        memory: &mut Memory,
        sink: &mut EventSink<'_>,
        out: &mut dyn Write,
        ticket: &TicketId,
        seen: Seen,
        now: Timestamp,
        end: &StepEnd,
    ) {
        if *end == StepEnd::Stale {
            // The ticket ref moved since the snapshot: the next pass reads it.
            return;
        }
        let after = self.read_after(ticket);
        // Its move to working after the RESUME may have failed: the stage
        // step that follows takes it as shown needs input too (OWL-367).
        if matches!(end, StepEnd::Stepped { .. })
            && let Some(stored) = after.as_ref().filter(|stored| holds(stored))
        {
            memory.resumed.insert(ticket.clone(), stored.progress());
        }
        let progress = after.as_ref().map(Stored::progress);
        let holds_now = after.as_ref().is_some_and(holds);
        match self.after(
            memory,
            sink,
            out,
            ticket,
            seen,
            end,
            progress.as_ref(),
            holds_now,
        ) {
            // The usage limit's own line says it (OWL-404).
            Some(Admit::Held { .. }) if usage_held(end, now) => {}
            Some(Admit::Held {
                until: Until::At(until),
            }) => self.line(
                out,
                &format!(
                    "{ticket} is as it was before that continue: watch continues it again at \
                     {until}, or sooner once its decider edits a comment"
                ),
            ),
            Some(Admit::Held {
                until: Until::Person,
            }) => {
                // A RESUME asks the decider nothing: the remedy is the
                // operator's, once the ticket ref can be written.
                let remedy = if matches!(
                    end,
                    StepEnd::NotKept {
                        landed: LandedKind::Resume,
                    }
                ) {
                    "run `owlshift continue` once the ticket ref can be written; watch also \
                     continues it again once its decider edits a comment or a command moves \
                     its ticket ref"
                } else {
                    "watch continues it again only once its decider edits a comment or a \
                     command moves its ticket ref"
                };
                self.line(
                    out,
                    &format!(
                        "{ticket} waits for a person to act, but its state was not kept: {remedy}"
                    ),
                )
            }
            Some(Admit::Go) | None => {}
        }
    }

    /// A ticket left at a stage by an answered round (OWL-273 at Intake,
    /// OWL-340 at Design, Design review, Build and Verify; architecture
    /// section 5, "Which tickets"): resumed, its stage run again, while the
    /// tracker still shows `shown`, held as a continue is ([`Holds`]). The
    /// stage is read first without the lock, so a ticket shown elsewhere
    /// costs no lock, checkout or ticket read, and
    /// [`OnDemand::continue_shown`] reads it again under the lock, so a
    /// person's move meanwhile is kept. From a ref that waits for no answer
    /// `continue` runs no answer check and posts no RESUME.
    #[allow(clippy::too_many_arguments)]
    fn resume_stage(
        &self,
        memory: &mut Memory,
        sink: &mut EventSink<'_>,
        out: &mut dyn Write,
        continued: &mut Continued<'_>,
        ticket: &TicketId,
        stored: &Stored,
        shown: VisibleStage,
        stage: Stage,
    ) {
        let on_demand = self.on_demand;
        // Under an operator's pause no stage resumes, and no wait is told
        // on the ticket for it (OWL-438).
        if self.operator_paused(memory, sink, out) {
            return;
        }
        let now = (on_demand.clock)();
        let at = stored.at();
        if memory
            .elsewhere
            .get(ticket)
            .is_some_and(|(progress, until)| *progress == at.progress && now < *until)
        {
            return;
        }
        // A stage that needs the slot runs in its ticket's turn (OWL-367):
        // another ticket's turn holds it back, told once per holder. Before
        // it, a ticket that would touch what another holds waits (OWL-403),
        // told once per holder and resource.
        if shown == VisibleStage::Working {
            if memory.outside.unread_steps.contains(ticket) {
                return self.wait_for_unread(memory, sink, out, ticket);
            }
            if let Some((found, build)) = memory.collisions.get(ticket).cloned() {
                return self.wait_for_collision(memory, sink, out, ticket, found, build);
            }
            match memory.turn.clone() {
                Some(Turn::Holders(holders)) if !holders.contains(ticket) => {
                    return self.wait_for_slot(memory, sink, out, ticket, holders);
                }
                Some(Turn::Unknown(unread)) => {
                    return self.wait_for_slot(memory, sink, out, ticket, unread);
                }
                // A usage limit's wait ends below, once its harness is free;
                // a base's once a step goes on (OWL-418).
                _ if matches!(
                    memory.waits.get(ticket),
                    Some(StepWait::Usage(_) | StepWait::Base(_))
                ) => {}
                _ => self.wait_ended(memory, sink, ticket),
            }
        }
        let seen = Seen::at(at, None);
        // Another ticket's check holds the priority: one held for its base
        // waits for it as it is, its turn kept (OWL-431).
        if memory.base_held.contains(ticket)
            && memory
                .base_due
                .as_ref()
                .is_some_and(|(due, _)| due != ticket)
        {
            return;
        }
        // A hold on its base lifted, or its check due with no other step in
        // flight, or a turn a drain's dispatch gave it: the ticket goes at
        // once (OWL-418, OWL-431).
        if self.base_released(memory, ticket) {
            memory.holds.release(ticket);
        }
        if memory.holds.admit(ticket, &seen, now) != Admit::Go {
            return;
        }
        // Such a step may start a rebase run first (OWL-427), whose own
        // check of its harness's usage holds it, nothing spent.
        let gate_only = stage == Stage::Build && base::wait_file(on_demand.dirs, ticket).exists();
        match self.usage_wait(
            memory,
            sink,
            out,
            limits::resume_role(stage, stored, gate_only),
        ) {
            // Told on the ticket while it shows working (OWL-419).
            Some((harness, until_ms)) => {
                if shown == VisibleStage::Working {
                    self.wait_for_usage(memory, sink, ticket, harness, until_ms);
                }
                return;
            }
            None if matches!(memory.waits.get(ticket), Some(StepWait::Usage(_))) => {
                self.wait_ended(memory, sink, ticket);
            }
            None => {}
        }
        let found = match on_demand.tracker.stage(ticket) {
            Ok(found) => found,
            Err(e) => {
                let reason = format!("reading its stage: {e}");
                return self.unreadable(memory, sink, ticket, reason);
            }
        };
        memory.failing.remove(ticket);
        let states = &on_demand.config.tracker.states;
        if found != shown.state(states) {
            // An answer check of this watch whose move to working after the
            // RESUME failed: moved now, then resumed (OWL-367).
            let resumed = shown == VisibleStage::Working
                && found == VisibleStage::NeedsInput.state(states)
                && memory.resumed.get(ticket) == Some(&seen.progress);
            if !(resumed && self.show_working(sink, ticket, &stored.commit)) {
                let until = now.checked_add(RETRY_AFTER).unwrap_or(Timestamp::MAX);
                memory
                    .elsewhere
                    .insert(ticket.clone(), (seen.progress.clone(), until));
                return;
            }
        }
        memory.elsewhere.remove(ticket);
        // The step records watch's `decision` event once the ticket ref and
        // its stage are confirmed under the lock, and goes on from what the
        // last step left while the ref is where that step left it
        // (`carry.json`, OWL-399).
        let kind = StepKind::Resume {
            shown: step::shown(shown),
            stage,
        };
        let envelope = self.envelope(memory, ticket, kind, &seen);
        let then = Then::Resume { seen, stage, now };
        self.step(memory, sink, out, continued, envelope, None, None, then);
    }

    /// What a resume at `stage` that ended `end` left (OWL-340, OWL-367;
    /// since OWL-401 applied once its child ends): held as a continue is,
    /// from the ticket ref read after it, and told.
    #[allow(clippy::too_many_arguments)]
    fn resumed(
        &self,
        memory: &mut Memory,
        sink: &mut EventSink<'_>,
        out: &mut dyn Write,
        ticket: &TicketId,
        (seen, stage, now): (Seen, Stage, Timestamp),
        end: &StepEnd,
    ) {
        if *end == StepEnd::Stale {
            // The ref moved, or the ticket was moved, since the reads: the
            // next pass reads it again.
            return;
        }
        let after = self.read_after(ticket);
        let progress = after.as_ref().map(Stored::progress);
        let holds_now = after.as_ref().is_some_and(holds);
        let admitted = self.after(
            memory,
            sink,
            out,
            ticket,
            seen,
            end,
            progress.as_ref(),
            holds_now,
        );
        // A Build held for its base: told once per commit, on the ticket
        // too; a later step that goes on ends that wait (OWL-418).
        if let StepEnd::BaseHeld {
            commit,
            command,
            pending,
        } = end
        {
            self.wait_for_base(
                memory,
                sink,
                out,
                ticket,
                commit,
                command.as_deref(),
                *pending,
            );
            return;
        }
        // A step that ran nothing did not go on: the base still holds it.
        if running::ran(end) && matches!(memory.waits.get(ticket), Some(StepWait::Base(_))) {
            self.wait_ended(memory, sink, ticket);
        }
        match admitted {
            // The usage limit's own line says it (OWL-404), and the ticket,
            // which shows working, a WAITING comment (OWL-419).
            Some(Admit::Held { .. }) if usage_held(end, now) => {
                if let (
                    StepEnd::UsageLimit {
                        resets_at: Some(until),
                    },
                    Some(stored),
                ) = (end, after.as_ref().filter(|stored| holds(stored)))
                {
                    let role = limits::resume_role(stage, stored, false);
                    let harness = own_harness(role.unwrap_or(Role::Build));
                    let until_ms = machine_usage::ms(*until);
                    self.wait_for_usage(memory, sink, ticket, harness, until_ms);
                }
            }
            Some(Admit::Held {
                until: Until::At(until),
            }) => self.line(out, &held_text(ticket, stage, until)),
            Some(Admit::Held {
                until: Until::Person,
            }) => self.line(
                out,
                &format!(
                    "{ticket} waits for a person to act, but its state was not kept: run \
                     `owlshift continue` once the ticket ref can be written; watch resumes it \
                     again only once a command moves its ticket ref"
                ),
            ),
            Some(Admit::Go) | None => {}
        }
    }

    /// The ticket ref of `ticket` read after a command: `None` when gone or
    /// unreadable.
    fn read_after(&self, ticket: &TicketId) -> Option<Stored> {
        let on_demand = self.on_demand;
        ticket_ref::read(&on_demand.executor.git, &on_demand.dirs.checkout(), ticket)
            .ok()
            .flatten()
    }

    /// What a command of watch's on `ticket` from `seen` left, once its
    /// `outcome` is told, `progress` being its ticket ref's progress read
    /// after it (`None` when gone or unreadable; a move of the spend alone is
    /// none, OWL-397) and `holds_now` whether that ref holds the slot: held
    /// as a continue is ([`Holds::after`]), and, since OWL-367, the slot's
    /// turn and whether the next pass is due at once; what a stage step
    /// carries is the step's own, in `carry.json` (OWL-399). Returns the
    /// hold or the pause it set.
    #[allow(clippy::too_many_arguments)]
    fn after(
        &self,
        memory: &mut Memory,
        sink: &mut EventSink<'_>,
        out: &mut dyn Write,
        ticket: &TicketId,
        seen: Seen,
        outcome: &StepEnd,
        progress: Option<&Progress>,
        holds_now: bool,
    ) -> Option<Admit> {
        let moved = progress != Some(&seen.progress);
        // A stage step whose ref moved: its next step is due at once.
        if moved && matches!(outcome, StepEnd::Stepped { .. }) {
            memory.stepped = true;
        }
        // A ref that holds the slot now takes the turn while fewer than the
        // project's capacity hold it, or waits for its holders, told in this
        // very pass; a holder that left the slot gives its turn at the next
        // pass, at once (OWL-367, OWL-401).
        // A Build held for its base still waits: its comment stays (OWL-418),
        // as it does when the step ran nothing, held back for that base's
        // check or for a run slot (found on LOC-1458, 2026-10-25).
        let base_held = matches!(outcome, StepEnd::BaseHeld { .. })
            || !running::ran(outcome)
                && matches!(memory.waits.get(ticket), Some(StepWait::Base(_)));
        match memory.turn.clone().unwrap_or(Turn::Holders(Vec::new())) {
            Turn::Holders(holders) if holders.contains(ticket) => {
                if holds_now && !base_held {
                    self.wait_ended(memory, sink, ticket);
                } else if moved {
                    memory.stepped = true;
                }
            }
            Turn::Holders(mut holders) if holds_now => {
                if holders.len() < self.capacity() as usize {
                    holders.push(ticket.clone());
                    memory.turn = Some(Turn::Holders(holders));
                    if !base_held {
                        self.wait_ended(memory, sink, ticket);
                    }
                } else {
                    self.wait_for_slot(memory, sink, out, ticket, holders);
                }
            }
            Turn::Unknown(unread) if holds_now => {
                self.wait_for_slot(memory, sink, out, ticket, unread);
            }
            _ => {}
        }
        let now = (self.on_demand.clock)();
        memory.holds.after(ticket, seen, outcome, progress, now)
    }

    /// Tells that `ticket` waits for the project's slot, which `holders`
    /// hold (OWL-367): one `decision` event (`watch` = `waits`, `reason` =
    /// `slot`) and one line, unless the last told named the same holders.
    fn wait_for_slot(
        &self,
        memory: &mut Memory,
        sink: &mut EventSink<'_>,
        out: &mut dyn Write,
        ticket: &TicketId,
        holders: Vec<TicketId>,
    ) {
        let told = StepWait::Slot(holders);
        if memory.waits.get(ticket) == Some(&told) {
            return;
        }
        let StepWait::Slot(holders) = told else {
            unreachable!("made a slot's wait above")
        };
        let names: Vec<&str> = holders.iter().map(TicketId::as_str).collect();
        let event = data([
            ("watch", json!("waits")),
            ("reason", json!("slot")),
            ("tickets", json!(names)),
        ]);
        sink.emit(ticket, None, EventKind::Decision, event.clone());
        self.show_wait(memory, sink, ticket, "slot", event);
        self.line(
            out,
            &format!(
                "{ticket} waits: the project's slot is held by {}, and its next stage runs once \
                 the slot is its turn (`owlshift doctor` says how to clear a stale one)",
                names.join(", ")
            ),
        );
        memory.waits.insert(ticket.clone(), StepWait::Slot(holders));
    }

    /// Tells that `ticket`'s next stage step waits for `found` (OWL-403;
    /// architecture section 5, "The collision rule"), its Build's admission
    /// when `build` (OWL-448, "Resources admitted at Build"): one `decision`
    /// event (`watch` = `waits`, `reason` = `collision`, the holder in
    /// `tickets`, its `resource`, and `stage` = `build` for a Build's) and one
    /// line, unless the last told was the same.
    fn wait_for_collision(
        &self,
        memory: &mut Memory,
        sink: &mut EventSink<'_>,
        out: &mut dyn Write,
        ticket: &TicketId,
        found: Collision<String>,
        build: bool,
    ) {
        let told = StepWait::Collision(found, build);
        if memory.waits.get(ticket) == Some(&told) {
            return;
        }
        let StepWait::Collision(found, _) = &told else {
            unreachable!("made a collision's wait above")
        };
        // A pull request or a person's ticket is named as such (OWL-410);
        // under `rebase`, both tickets that share the path (OWL-449).
        let (holding, who) = in_flight::holding(&memory.outside.kinds, found);
        let mut event = data([
            ("watch", json!("waits")),
            ("reason", json!("collision")),
            ("resource", held::resource_data(&found.resource)),
        ]);
        if build {
            event.insert("stage".to_owned(), json!("build"));
        }
        for (key, value) in in_flight::holding_data(&memory.outside.kinds, found) {
            event.insert(key.to_owned(), value);
        }
        sink.emit(ticket, None, EventKind::Decision, event.clone());
        self.show_wait(memory, sink, ticket, "collision", event);
        let line = if build {
            format!(
                "{ticket}'s Build waits: {holding} {}, which its Build would take, and it is \
                 admitted once {who} no longer holds it",
                found.resource
            )
        } else {
            format!(
                "{ticket} waits: {holding} {}, which it would touch, and its next stage runs once \
                 {who} no longer holds it",
                found.resource
            )
        };
        self.line(out, &line);
        memory.waits.insert(ticket.clone(), told);
    }

    /// Tells `ticket`, whose Build waits for `commit`, its base, which fails
    /// `command` too or whose check is `pending` (OWL-418): one line and one
    /// WAITING comment per commit, the `decision` event being the step's.
    #[allow(clippy::too_many_arguments)]
    fn wait_for_base(
        &self,
        memory: &mut Memory,
        sink: &mut EventSink<'_>,
        out: &mut dyn Write,
        ticket: &TicketId,
        commit: &str,
        command: Option<&str>,
        pending: bool,
    ) {
        let told = StepWait::Base(commit.to_owned());
        if memory.waits.get(ticket) == Some(&told) {
            return;
        }
        // A held commit whose check again waits (OWL-431) is told as held:
        // the step's end says only that a check waits.
        let pending = pending
            && !base::read(self.on_demand.dirs).is_ok_and(|checks| {
                checks.checks.iter().any(|check| {
                    check.commit == commit
                        && matches!(
                            check.entry,
                            owlshift_core::base::Entry::Checked { hold: Some(_), .. }
                        )
                })
            });
        let event = data([
            ("commit", json!(commit)),
            ("command", json!(command)),
            ("pending", json!(pending)),
        ]);
        let phrase = why::phrase(&why::Waiting {
            at: (self.on_demand.clock)(),
            ticket: ticket.clone(),
            reason: "base_broken".to_owned(),
            data: event.clone(),
        });
        self.show_wait(memory, sink, ticket, "base_broken", event);
        self.line(out, &format!("{ticket} waits: {phrase}"));
        memory.waits.insert(ticket.clone(), told);
    }

    /// The ticket a check pending in `base.json` gives its priority to, from
    /// now, when one waits and its priority was not given up (OWL-418).
    fn pending_due(&self, memory: &Memory) -> Option<(TicketId, Timestamp)> {
        let dirs = self.on_demand.dirs;
        let checks = base::read(dirs).ok()?;
        // The tickets' folders are read only while a check is pending.
        if !checks
            .standing()
            .any(|check| matches!(check.entry, owlshift_core::base::Entry::Pending { .. }))
        {
            return None;
        }
        let ticket = base::pending_ticket(&checks, &base::waits(dirs), |ticket| {
            memory.base_due_given_up.contains(ticket)
        })?;
        Some((ticket, (self.on_demand.clock)()))
    }

    /// Whether `ticket`'s hold is to be lifted for its base (OWL-418): the
    /// commit its Build waited for no longer holds it, or its check is due
    /// and no other step is in flight, or a drain's dispatch gave it a turn
    /// (OWL-431).
    fn base_released(&self, memory: &Memory, ticket: &TicketId) -> bool {
        if memory
            .base_due
            .as_ref()
            .is_some_and(|(due, _)| due == ticket)
            && memory.running.is_empty()
        {
            return true;
        }
        if memory.base_turn.contains(ticket) {
            return true;
        }
        match memory.waits.get(ticket) {
            Some(StepWait::Base(commit)) => {
                !base::read(self.on_demand.dirs).is_ok_and(|checks| checks.holds_commit(commit))
            }
            _ => false,
        }
    }

    /// Tells `ticket`, whose next stage step waits for `harness`'s usage
    /// limit until `until_ms` at least, on the tracker (OWL-419): one
    /// WAITING comment per harness, the `decision` event being the
    /// project's ([`Watch::usage_waits`]).
    fn wait_for_usage(
        &self,
        memory: &mut Memory,
        sink: &mut EventSink<'_>,
        ticket: &TicketId,
        harness: Harness,
        until_ms: i64,
    ) {
        let told = StepWait::Usage(harness);
        if memory.waits.get(ticket) == Some(&told) {
            return;
        }
        let event = data([
            ("harness", json!(machine_usage::shown(harness))),
            ("until", json!(machine_usage::at(until_ms).to_string())),
        ]);
        self.show_wait(memory, sink, ticket, "usage_limit", event);
        memory.waits.insert(ticket.clone(), told);
    }

    /// Shows on the tracker that `ticket` waits for `reason`, as `event`
    /// says it, in `owlshift why`'s words (OWL-419): one WAITING comment,
    /// unless the one shown says the same.
    fn show_wait(
        &self,
        memory: &mut Memory,
        sink: &mut EventSink<'_>,
        ticket: &TicketId,
        reason: &str,
        event: Data,
    ) {
        // A harness's limit is one reason whatever its reset time, so a
        // probe's next deadline posts nothing; a base is one whatever its
        // check says (OWL-418).
        let usage = match reason {
            "usage_limit" => Some(format!("{reason}: {}", event["harness"])),
            "base_broken" => Some(format!("{reason}: {}", event["commit"])),
            _ => None,
        };
        let phrase = why::phrase(&why::Waiting {
            at: (self.on_demand.clock)(),
            ticket: ticket.clone(),
            reason: reason.to_owned(),
            data: event,
        });
        let key = usage.unwrap_or_else(|| format!("{reason}: {phrase}"));
        memory
            .wait_comments
            .show(&mut self.board(sink), ticket, &key, phrase);
    }

    /// The wait of `ticket`'s next stage step ended (OWL-403, OWL-419):
    /// forgotten, and its WAITING comment folded.
    fn wait_ended(&self, memory: &mut Memory, sink: &mut EventSink<'_>, ticket: &TicketId) {
        memory.waits.remove(ticket);
        memory.wait_comments.clear(&mut self.board(sink), ticket);
    }

    /// Where a WAITING comment is told.
    fn board<'b, 's>(&'b self, sink: &'b mut EventSink<'s>) -> waiting::Board<'b, 's> {
        waiting::Board {
            tracker: self.on_demand.tracker,
            dirs: self.on_demand.dirs,
            sink,
        }
    }

    /// The tickets whose next stage step a collision holds back (OWL-403):
    /// each ticket ref of `read` that holds the slot and whose pull request
    /// is not open, against every other ref that holds what its ticket
    /// holds ([`held::holds`]), closed with the named resources of the
    /// project file watch loaded. A ref that cannot be read holds no
    /// ticket back here: it stops every stage step already ([`Turn::Unknown`]).
    /// The work in flight outside Owlshift, `outside`, holds them too, and
    /// its pull requests' files join the tickets that delivered them
    /// (OWL-410).
    ///
    /// A ticket at Build whose admission is due ([`held::due`], its labels
    /// from `outside`'s read of the working state) is predicted as its step
    /// would admit it (OWL-448): those due together in dispatch order, each
    /// waiting for anything held whatever the holder's dispatch time
    /// ([`schedule::admissions`]); one `held_back` this pass holds nothing
    /// for those after it. Each wait says whether it is a Build's.
    fn collisions(
        &self,
        read: &[RefRead],
        outside: Option<&in_flight::Holders>,
        overlap: Overlap,
        held_back: &dyn Fn(&TicketId) -> bool,
    ) -> HashMap<TicketId, (Collision<String>, bool)> {
        let on_demand = self.on_demand;
        let named = on_demand.config.stack.named_resources().unwrap_or_default();
        let refs: Vec<(&TicketId, &Stored)> = read
            .iter()
            .filter_map(|(ticket, stored)| stored.as_ref().ok().map(|stored| (ticket, stored)))
            .collect();
        let mut in_flight: Vec<_> = refs
            .iter()
            .filter(|(_, stored)| held::holds(stored))
            .map(|(ticket, stored)| held::in_flight(ticket, stored, &named))
            .collect();
        if let Some(outside) = outside {
            outside.join(&mut in_flight, &named);
        }
        let now = (on_demand.clock)();
        let mut found = HashMap::new();
        let mut due = Vec::new();
        for (ticket, stored) in refs {
            if !holds(stored) || delivery_followed(stored) {
                continue;
            }
            let own = held::in_flight(ticket, stored, &named);
            if let Some(collided) = weigh(&own, &in_flight, overlap).collision {
                found.insert(ticket.clone(), (collided, false));
                continue;
            }
            let at_build = TicketState::try_from(&stored.record.state)
                .is_ok_and(|state| state.status() == Status::Active(Stage::Build));
            let labels = outside
                .and_then(|outside| outside.labels.get(ticket.as_str()))
                .map_or(&[][..], Vec::as_slice);
            let admitted = at_build
                .then(|| {
                    held::due(
                        &stored.record,
                        labels,
                        on_demand.config.pipeline.default,
                        &named,
                        now,
                    )
                })
                .flatten();
            if let (Some(admitted), Some(before)) = (admitted, stored.record.held.as_ref()) {
                due.push(schedule::Due {
                    ticket: InFlight {
                        held: held::of(ticket, Some(&admitted), false, &named).held,
                        ..own
                    },
                    added: held::added(ticket, before, &admitted, &named),
                    starts: !held_back(ticket),
                });
            }
        }
        for (id, collided) in schedule::admissions(&due, &in_flight, overlap) {
            if let (Some(collided), Ok(ticket)) = (collided, TicketId::new(&id)) {
                found.insert(ticket, (collided, true));
            }
        }
        found
    }

    /// Moves `ticket` to working, under the project's lock while its ref is
    /// still `seen`, the commit this watch's answer check left, and only
    /// while the tracker still shows needs input: the move after its RESUME
    /// failed (OWL-367). Returns whether it shows working now; a move that
    /// fails is a `stage_not_moved` warning, tried again later.
    fn show_working(&self, sink: &mut EventSink<'_>, ticket: &TicketId, seen: &str) -> bool {
        let Ok(Some(_lock)) = self.locked_at(ticket, seen) else {
            return false;
        };
        let on_demand = self.on_demand;
        match on_demand.working_after_resume(ticket, sink) {
            Ok(moved) => moved,
            Err(reason) => {
                on_demand.stage_not_moved(ticket, None, VisibleStage::Working, &reason, sink);
                false
            }
        }
    }

    /// Warns that `ticket` could not be read, unless the last warning said
    /// the same.
    fn unreadable(
        &self,
        memory: &mut Memory,
        sink: &mut EventSink<'_>,
        ticket: &TicketId,
        reason: String,
    ) {
        if memory.failing.get(ticket) == Some(&reason) {
            return;
        }
        sink.emit(
            ticket,
            None,
            EventKind::Warning,
            data([
                ("what", json!("watch_unreadable")),
                ("reason", json!(reason)),
            ]),
        );
        memory.failing.insert(ticket.clone(), reason);
    }

    /// Prints what the pass found about the project when it changed.
    fn note(&self, memory: &mut Memory, out: &mut dyn Write, project: Project) {
        if memory.project.as_ref() == Some(&project) {
            return;
        }
        let text = match &project {
            Project::Busy => {
                "another command is working the project: watch looks again at its next pass"
                    .to_owned()
            }
            Project::Quarantined(_) => format!(
                "a run's isolation check did not pass, so no ticket of the project is continued \
                 or built until a person clears the quarantine: `owlshift unquarantine` shows \
                 what {} says and clears it",
                self.on_demand.dirs.quarantine_file().display()
            ),
            Project::Unread(reason) => format!("the ticket refs could not be read: {reason}"),
            Project::Watching { waiting, followed } => {
                let list = |tickets: &[TicketId]| {
                    tickets
                        .iter()
                        .map(TicketId::as_str)
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                let mut text = if waiting.is_empty() {
                    "no ticket's questions wait for an answer".to_owned()
                } else {
                    format!("questions wait for an answer on {}", list(waiting))
                };
                if !followed.is_empty() {
                    text.push_str(&format!(
                        "; watch follows the pull requests of {}",
                        list(followed)
                    ));
                }
                text
            }
        };
        self.line(out, &text);
        memory.project = Some(project);
    }

    /// One line of watch's own, with this machine's time.
    /// The project's capacity, `policy.max_in_flight` (OWL-401).
    fn capacity(&self) -> u32 {
        u32::from(self.on_demand.config.policy.max_in_flight())
    }

    fn line(&self, out: &mut dyn Write, text: &str) {
        let now = (self.on_demand.clock)();
        let _ = writeln!(out, "{}", printable(&format!("{now} watch: {text}")));
    }
}

/// Whether the ticket ref `stored` holds the project's slot
/// ([`schedule::holds_slot`]); one whose state does not read is no holder
/// here, its ref being read by the next snapshot.
fn holds(stored: &Stored) -> bool {
    TicketState::try_from(&stored.record.state).is_ok_and(|state| schedule::holds_slot(&state))
}

/// What an end of the following adds to the `ended` event, and the words it
/// adds to watch's line, for a ticket in `status` whose ref keeps `round`:
/// `review_loop` for a review loop it ends (OWL-307), `parked` for a park it
/// ends (OWL-243), and the round for one whose questions waited, closed
/// unanswered (OWL-242).
fn ended_marks(status: Status, round: u32) -> (Data, String) {
    let mut marks = Data::new();
    let mut said = Vec::new();
    // A review loop's ticket, at Build or Verify (OWL-307).
    if status.stage() != Stage::Watch {
        marks.insert("review_loop".to_owned(), json!(true));
        said.push("its review loop ends".to_owned());
    }
    if matches!(status, Status::Parked { .. }) {
        marks.insert("parked".to_owned(), json!(true));
        said.push("it is no longer parked".to_owned());
    }
    if matches!(
        status,
        Status::NeedsInput { .. }
            | Status::Parked {
                awaiting_input: true,
                ..
            }
    ) {
        marks.insert("round".to_owned(), json!(round));
        said.push(format!("its round {round} no longer waits for an answer"));
    }
    (marks, said.join(", and "))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use owlshift_contracts::Stage;

    use super::*;
    use crate::on_demand::Landed;

    fn at(minute: i64) -> Timestamp {
        Timestamp::from_second(1_790_000_000 + minute * 60).unwrap()
    }

    /// The progress `key` stands for.
    fn p(key: &str) -> Progress {
        Progress(key.to_owned())
    }

    /// A ticket ref at progress `key`, whose commit is `key`'s own, and the
    /// decider's edit at minute `answer`.
    fn seen(key: &str, answer: i64) -> Seen {
        seen_at(key, key, answer)
    }

    /// [`seen`], its commit `commit`.
    fn seen_at(commit: &str, key: &str, answer: i64) -> Seen {
        Seen {
            commit: commit.to_owned(),
            progress: p(key),
            answer: Some(at(answer)),
        }
    }

    fn refused() -> StepEnd {
        end(Err(Stop::Refused("the tracker is down".to_owned())))
    }

    /// How a step with `outcome` ended, as watch reads it (OWL-399).
    fn end(outcome: Result<Delivered, Stop>) -> StepEnd {
        step::end(&step::Stepped::Ran(outcome))
    }

    #[test]
    fn only_a_ticket_whose_questions_wait_is_watched() {
        assert!(watched(Status::NeedsInput {
            return_to: Stage::Build
        }));
        for status in [
            Status::Active(Stage::Build),
            Status::Parked {
                at: Stage::Build,
                awaiting_input: true,
            },
        ] {
            assert!(!watched(status), "{status:?}");
        }
    }

    fn restored(status: Status, round: u32) -> TicketState {
        TicketState::restore(status, round, 0, 0, 0, 0).unwrap()
    }

    /// OWL-273 and OWL-340: a stage left after an answered round shows the
    /// stage its run keeps, and one left at round 0, waiting or parked is a
    /// person's.
    #[test]
    fn a_stage_left_after_an_answered_round_is_resumable() {
        let active = |stage| Status::Active(stage);
        assert_eq!(
            resumable_stage(&restored(active(Stage::Intake), 1)),
            Some(VisibleStage::NeedsInput)
        );
        for stage in [
            Stage::Design,
            Stage::DesignReview,
            Stage::Build,
            Stage::Verify,
        ] {
            // Whatever the round since OWL-367: a stage step leaves it there.
            for round in [0, 1] {
                assert_eq!(
                    resumable_stage(&restored(active(stage), round)),
                    Some(VisibleStage::Working),
                    "{stage:?} {round}"
                );
            }
        }
        assert_eq!(resumable_stage(&restored(active(Stage::Intake), 0)), None);
        for status in [
            active(Stage::Ready),
            active(Stage::Deliver),
            active(Stage::Watch),
            Status::NeedsInput {
                return_to: Stage::Design,
            },
            Status::NeedsInput {
                return_to: Stage::Build,
            },
            Status::Parked {
                at: Stage::Build,
                awaiting_input: false,
            },
            Status::Parked {
                at: Stage::Verify,
                awaiting_input: false,
            },
        ] {
            assert_eq!(resumable_stage(&restored(status, 1)), None, "{status:?}");
        }
    }

    #[test]
    fn the_held_line_names_the_stage() {
        let ticket = TicketId::new("DEMO-1").unwrap();
        let until = Timestamp::from_second(1_792_324_800).unwrap();
        assert_eq!(
            held_text(&ticket, Stage::Intake, until),
            "DEMO-1 is at Intake as it was before that resume: watch resumes it again at \
             2026-10-18T12:00:00Z, or sooner once a command moves its ticket ref"
        );
        assert_eq!(
            held_text(&ticket, Stage::DesignReview, until),
            "DEMO-1 is at Design review as it was before that resume: watch resumes it again \
             at 2026-10-18T12:00:00Z, or sooner once a command moves its ticket ref"
        );
    }

    /// An end marks a park (OWL-243) and names a round whose questions
    /// waited (OWL-242), both for a park that waited for answers.
    #[test]
    fn an_end_marks_the_park_and_the_round_it_closes() {
        let parked = |awaiting_input| Status::Parked {
            at: Stage::Watch,
            awaiting_input,
        };
        let waiting = Status::NeedsInput {
            return_to: Stage::Watch,
        };
        let cases = [
            (
                waiting,
                json!({ "round": 2 }),
                "its round 2 no longer waits for an answer",
            ),
            (
                parked(false),
                json!({ "parked": true }),
                "it is no longer parked",
            ),
            (
                parked(true),
                json!({ "parked": true, "round": 2 }),
                "it is no longer parked, and its round 2 no longer waits for an answer",
            ),
        ];
        for (status, marks, said) in cases {
            let (actual, words) = ended_marks(status, 2);
            assert_eq!(
                (serde_json::Value::Object(actual), words.as_str()),
                (marks, said),
                "{status:?}"
            );
        }
    }

    /// A continue that left the ticket ref as it was is held for
    /// `RETRY_AFTER`, unless the answer or the ref changes; one that moved
    /// it, or found the project busy, is not held.
    #[test]
    fn a_continue_that_changed_nothing_is_held() {
        let ticket = TicketId::new("DEMO-1").unwrap();
        let mut holds = Holds::default();
        assert_eq!(holds.admit(&ticket, &seen("c1", 0), at(1)), Admit::Go);

        let busy = Err(Stop::Busy(PathBuf::from("/p")));
        assert_eq!(
            holds.after(&ticket, seen("c1", 0), &end(busy), Some(&p("c1")), at(1)),
            None
        );
        assert_eq!(holds.admit(&ticket, &seen("c1", 0), at(2)), Admit::Go);

        let held = holds.after(&ticket, seen("c1", 0), &refused(), Some(&p("c1")), at(2));
        assert_eq!(
            held,
            Some(Admit::Held {
                until: Until::At(at(12))
            })
        );
        assert_eq!(
            holds.admit(&ticket, &seen("c1", 0), at(11)),
            Admit::Held {
                until: Until::At(at(12))
            }
        );
        // A newer edit of the decider's, or a ref moved by a typed command,
        // lifts the hold.
        assert_eq!(holds.admit(&ticket, &seen("c1", 5), at(11)), Admit::Go);
        holds.after(&ticket, seen("c1", 0), &refused(), Some(&p("c1")), at(2));
        assert_eq!(holds.admit(&ticket, &seen("c2", 0), at(3)), Admit::Go);
        // And the hold ends.
        holds.after(&ticket, seen("c1", 0), &refused(), Some(&p("c1")), at(2));
        assert_eq!(holds.admit(&ticket, &seen("c1", 0), at(12)), Admit::Go);

        // A continue that moved the ref, a failed check kept for one, is
        // progress.
        assert_eq!(
            holds.after(&ticket, seen("c1", 0), &refused(), Some(&p("c3")), at(13)),
            None
        );
        assert_eq!(holds.admit(&ticket, &seen("c3", 0), at(13)), Admit::Go);
    }

    /// OWL-404: a usage limit with a reset time holds its own ticket until
    /// then, never another one: the usage file holds the harness; one
    /// without falls under the ref rule.
    #[test]
    fn a_usage_limit_holds_its_own_ticket_until_it_resets() {
        let (one, two) = (
            TicketId::new("DEMO-1").unwrap(),
            TicketId::new("DEMO-2").unwrap(),
        );
        let mut holds = Holds::default();
        let limited = Err(Stop::UsageLimit {
            resets_at: Some(at(30)),
        });
        assert_eq!(
            holds.after(&one, seen("c1", 0), &end(limited), Some(&p("c1")), at(1)),
            Some(Admit::Held {
                until: Until::At(at(30))
            })
        );
        assert_eq!(holds.admit(&two, &seen("d1", 0), at(29)), Admit::Go);
        assert_eq!(
            holds.admit(&one, &seen("c1", 0), at(29)),
            Admit::Held {
                until: Until::At(at(30))
            }
        );
        assert_eq!(holds.admit(&one, &seen("c1", 0), at(30)), Admit::Go);

        let unknown = Err(Stop::UsageLimit { resets_at: None });
        assert_eq!(
            holds.after(&one, seen("c1", 0), &end(unknown), Some(&p("c1")), at(31)),
            Some(Admit::Held {
                until: Until::At(at(41))
            })
        );
        assert_eq!(holds.admit(&two, &seen("d1", 0), at(32)), Admit::Go);
    }

    /// OWL-418: a Build held for its base leaves its ref as it was, so it is
    /// held for the retry delay, and lifting its base's hold releases it at
    /// once.
    #[test]
    fn a_base_hold_is_held_until_released() {
        let one = TicketId::new("DEMO-1").unwrap();
        let mut holds = Holds::default();
        let held = Err(Stop::BaseHeld {
            ticket: one.clone(),
            commit: "c".repeat(40),
            command: Some("sh setup.sh".to_owned()),
            pending: false,
            due: false,
        });
        assert_eq!(
            holds.after(&one, seen("c1", 0), &end(held), Some(&p("c1")), at(1)),
            Some(Admit::Held {
                until: Until::At(at(11))
            })
        );
        assert_ne!(holds.admit(&one, &seen("c1", 0), at(2)), Admit::Go);
        holds.release(&one);
        assert_eq!(holds.admit(&one, &seen("c1", 0), at(2)), Admit::Go);
    }

    /// OWL-158: a continue that left a comment on the ticket whose state was
    /// not kept is held until a person acts, whichever comment landed, a
    /// RESUME too (OWL-169), even when the ref could not be read after it; a
    /// ref read moved is progress.
    #[test]
    fn a_comment_whose_state_is_not_kept_is_held_until_a_person_acts() {
        let ticket = TicketId::new("DEMO-1").unwrap();
        let not_kept = |landed| {
            Err(Stop::NotKept {
                landed,
                message: "the ticket's ref is locked".to_owned(),
            })
        };
        let person = Some(Admit::Held {
            until: Until::Person,
        });
        let mut holds = Holds::default();
        for landed in [
            Landed::Resume,
            Landed::Reask,
            Landed::Reply,
            Landed::Parked,
            Landed::ParkedUnposted,
        ] {
            for progress in [Some(&p("c1")), None] {
                let outcome = not_kept(landed);
                assert_eq!(
                    holds.after(&ticket, seen("c1", 0), &end(outcome), progress, at(1)),
                    person,
                    "{landed:?} {progress:?}"
                );
                // A day later, as at once.
                assert_eq!(
                    holds.admit(&ticket, &seen("c1", 0), at(24 * 60)),
                    Admit::Held {
                        until: Until::Person
                    }
                );
            }
        }
        // The decider's edit lifts it, and so does a ref a command moved.
        assert_eq!(holds.admit(&ticket, &seen("c1", 5), at(2)), Admit::Go);
        holds.after(
            &ticket,
            seen("c1", 0),
            &end(not_kept(Landed::Reask)),
            None,
            at(2),
        );
        assert_eq!(holds.admit(&ticket, &seen("c2", 0), at(3)), Admit::Go);

        // A round asked after a RESUME that was kept moved the ref.
        assert_eq!(
            holds.after(
                &ticket,
                seen("c1", 0),
                &end(not_kept(Landed::Questions)),
                Some(&p("c3")),
                at(4)
            ),
            None
        );
        assert_eq!(holds.admit(&ticket, &seen("c1", 0), at(5)), Admit::Go);
    }

    /// OWL-397: the runner keeps the spend in the ticket ref after every
    /// run, so a continue that changed nothing else leaves a new commit at
    /// the same progress. Its hold stands at the next pass, which reads that
    /// commit, a 10-minute one as one until a person acts; a ref whose
    /// progress changed lifts either.
    #[test]
    fn a_ref_moved_by_its_spend_alone_keeps_the_hold() {
        let ticket = TicketId::new("DEMO-1").unwrap();
        let mut holds = Holds::default();
        assert_eq!(
            holds.after(&ticket, seen("p1", 0), &refused(), Some(&p("p1")), at(1)),
            Some(Admit::Held {
                until: Until::At(at(11))
            })
        );
        assert_eq!(
            holds.admit(&ticket, &seen_at("spent", "p1", 0), at(5)),
            Admit::Held {
                until: Until::At(at(11))
            }
        );
        assert_eq!(holds.admit(&ticket, &seen("p2", 0), at(5)), Admit::Go);

        let not_kept = Err(Stop::NotKept {
            landed: Landed::Reask,
            message: "the ticket's ref is locked".to_owned(),
        });
        holds.after(
            &ticket,
            seen("p1", 0),
            &end(not_kept),
            Some(&p("p1")),
            at(6),
        );
        assert_eq!(
            holds.admit(&ticket, &seen_at("spent", "p1", 0), at(24 * 60)),
            Admit::Held {
                until: Until::Person
            }
        );
        assert_eq!(holds.admit(&ticket, &seen("p2", 0), at(24 * 60)), Admit::Go);
    }
}
