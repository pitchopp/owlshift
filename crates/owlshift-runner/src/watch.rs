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
//! Since OWL-368, a snapshot that finds the `unverified` marker still reads
//! the ticket refs, through the runner's read-only plumbing, and the pass
//! then runs nothing: it notifies a marker no command notified, prints the
//! project's line once, and tells each waiting ticket whose answer counts,
//! and the ticket the drain would dispatch, that it waits for the quarantine,
//! once per ticket and marker ([`crate::quarantine`]).
//!
//! Nothing ends the loop but the operator: a ticket that cannot be read is a
//! `warning` event, once until it reads again, and the pass goes on. A
//! continue that left the ticket ref as it was is held for [`RETRY_AFTER`],
//! and a usage limit with a reset time holds every continue until then
//! ([`Holds`]), so a tracker that refuses writes does not cost an answer
//! check every pass. A continue that left a comment on the ticket whose
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
use owlshift_contracts::event::EventKind;
use owlshift_contracts::format::Format;
use owlshift_contracts::ids::TicketId;
use owlshift_contracts::step::{LandedKind, RefAfter, StepEnd, StepEnvelope, StepHold, StepKind};
use owlshift_core::follow::{Action, Decision};
use owlshift_core::schedule::{self, Turn};
use owlshift_core::state::{Status, TicketState};
use owlshift_core::vocab::Stage;

use crate::answer_check::{self, Readiness};
use crate::events::{Data, EventSink, data, printable};
use crate::follow::{self, Following, Look};
use crate::on_demand::{self, Delivered, OnDemand, PIPELINE, Stop};
use crate::pr_reviews;
use crate::project::ProjectLock;
use crate::quarantine::{self, Held, Record};
use crate::reversal::{self, Current};
use crate::step::{self, Children};
use crate::ticket_ref::{self, Progress, RefAt, Stored, TicketRecord};
use crate::writer::VisibleStage;

mod drain;

pub use drain::refusal as drain_refusal;

/// How long watch sleeps after each pass.
pub const POLL_INTERVAL: Duration = Duration::from_secs(60);

/// How long a continue that left the ticket ref as it was holds that ticket
/// back, unless the decider edits a comment or the ref moves meanwhile. One
/// that stopped with [`Stop::NotKept`] is held until then, with no end.
pub const RETRY_AFTER: Duration = Duration::from_secs(10 * 60);

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
fn resumable_stage(state: &TicketState) -> Option<VisibleStage> {
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
    /// The harness reached its usage limit, which resets then.
    Paused {
        until: Timestamp,
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
    paused_until: Option<Timestamp>,
    held: HashMap<TicketId, (Seen, Until)>,
}

impl Holds {
    /// Whether `ticket`, at `seen`, may be continued at `now`. A hold from
    /// another answer, or from a ticket ref whose progress has changed, is
    /// lifted; one whose ref moved by its spend alone stands (OWL-397).
    pub fn admit(&mut self, ticket: &TicketId, seen: &Seen, now: Timestamp) -> Admit {
        if let Some(until) = self.paused_until {
            if now < until {
                return Admit::Paused { until };
            }
            self.paused_until = None;
        }
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
    /// or unreadable), at `now`. Returns the hold or the pause it set, if
    /// any.
    ///
    /// A busy project ran nothing: no hold. A usage limit with a reset time
    /// pauses every continue until then. A comment, a RESUME included, or a
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
        if matches!(outcome, StepEnd::Busy) {
            return None;
        }
        if let StepEnd::UsageLimit {
            resets_at: Some(at),
        } = outcome
            && *at > now
        {
            let until = self.paused_until.map_or(*at, |paused| paused.max(*at));
            self.paused_until = Some(until);
            return Some(Admit::Paused { until });
        }
        let as_it_was = progress == Some(&seen.progress);
        let until = match outcome {
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

    /// Forgets the holds of the tickets `kept` does not name.
    fn retain(&mut self, kept: &[TicketId]) {
        self.held.retain(|ticket, _| kept.contains(ticket));
    }

    /// Applies `hold`, which a step of `ticket` from `seen` called for, as
    /// [`Self::after`] would have set it (OWL-399): a record a predecessor
    /// left.
    pub fn restore(&mut self, ticket: &TicketId, seen: Seen, hold: &StepHold) {
        match hold {
            StepHold::Paused { until } => {
                self.paused_until = Some(self.paused_until.map_or(*until, |at| at.max(*until)));
            }
            StepHold::Held { until } => {
                let until = until.map_or(Until::Person, Until::At);
                self.held.insert(ticket.clone(), (seen, until));
            }
        }
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
        Admit::Paused { until } => Some(StepHold::Paused { until }),
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
    /// Whose turn the project's one slot is for watch's stage steps
    /// (OWL-367): chosen at each pass by [`schedule::slot_turn`], its holder
    /// keeping it while its ref holds the slot, and taken during a pass by
    /// a ticket whose ref came to hold a free slot.
    turn: Option<Turn<TicketId>>,
    /// The tickets told that they wait for the slot, with the holders told.
    slot_waits: HashMap<TicketId, Vec<TicketId>>,
    /// The tickets whose answer check watch ran into a stage that needs the
    /// slot, with their ref's progress after it: the tracker may still show
    /// them needs input when the move to working after the RESUME failed.
    resumed: HashMap<TicketId, Progress>,
    /// This pass took a step after which the next pass is due at once: a
    /// stage step whose ref moved, or a holder of the slot that left it.
    stepped: bool,
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
            memory.stepped = false;
            self.pass(&mut memory, sink, out, continued);
            if draining {
                self.drain(&mut memory, sink, out, continued);
            }
            // The interval is only between idle passes (OWL-367): after a
            // step, the next stage step is due at once.
            let interval = if memory.stepped {
                Duration::ZERO
            } else {
                self.interval
            };
            if !sleep(interval) {
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
        let read = match self.snapshot(memory, sink) {
            Ok(Snapshot {
                quarantine: Some(marker),
                refs,
            }) => return self.quarantined(memory, sink, out, continued, marker, refs),
            Ok(snapshot) => snapshot.refs,
            Err(project) => return self.note(memory, out, project),
        };
        let mut waiting = Vec::new();
        let mut following = Vec::new();
        let mut resumable = Vec::new();
        // A ticket whose ref cannot be read now keeps its hold: only one
        // read and found not waiting, or gone from the list, loses it.
        let mut unread = Vec::new();
        // The tickets whose ref holds the slot (OWL-367), one that does not
        // read among the unread, which may hold it.
        let mut holding = Vec::new();
        for (ticket, stored) in read {
            let state = stored.and_then(|stored| {
                TicketState::try_from(&stored.record.state)
                    .map(|state| (stored, state))
                    .map_err(|e| format!("its ticket ref keeps no valid state: {e:?}"))
            });
            if let Ok((_, state)) = &state
                && schedule::holds_slot(state)
            {
                holding.push(ticket.clone());
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
        memory.holds.retain(
            &[
                &ids[..],
                &followed[..],
                &left[..],
                &unread[..],
                &memory.drain.listed[..],
            ]
            .concat(),
        );
        // A predecessor's holds, once the drain's tickets were listed or
        // not (OWL-399).
        for (ticket, seen, hold) in std::mem::take(&mut memory.adopted) {
            memory.holds.restore(&ticket, seen, &hold);
        }
        memory
            .elsewhere
            .retain(|ticket, _| left.contains(ticket) || unread.contains(ticket));
        memory
            .following
            .retain(&[&followed[..], &read[..], &unread[..]].concat());
        memory
            .stale
            .retain(&[&followed[..], &read[..], &unread[..]].concat());
        // The slot's turn for this pass's stage steps (OWL-367).
        let kept = match &memory.turn {
            Some(Turn::Holder(holder)) => Some(holder),
            _ => None,
        };
        memory.turn = Some(schedule::slot_turn(&holding, &unread, kept));
        memory.resumed.retain(|ticket, _| holding.contains(ticket));
        memory
            .slot_waits
            .retain(|ticket, _| holding.contains(ticket) || followed.contains(ticket));
        self.note(
            memory,
            out,
            Project::Watching {
                waiting: ids,
                followed,
            },
        );
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
    }

    /// A pass while the marker `marker` quarantines the project (OWL-368):
    /// its line once, then each waiting ticket whose answer counts is told
    /// that it waits, once per marker, instead of continued. Nothing else
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
        for (ticket, stored) in refs {
            // A ticket told already costs no read of its comments.
            let told = memory
                .quarantine
                .as_ref()
                .is_some_and(|known| known.told.contains(&ticket));
            let (false, Ok(stored)) = (told, stored) else {
                continue;
            };
            if TicketState::try_from(&stored.record.state).is_ok_and(|s| watched(s.status())) {
                self.look(memory, sink, out, continued, &ticket, &stored, true);
            }
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
        quarantine::tell(on_demand.tracker, ticket, held, sink);
        known.told.insert(ticket.clone());
        let told = Record {
            notified: false,
            told: BTreeSet::from([ticket.clone()]),
        };
        let dirs = on_demand.dirs;
        let kept = matches!(
            dirs.lock(),
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
        let (look, warnings) = memory
            .following
            .look(on_demand.forge, ticket, delivery, now);
        for warning in warnings {
            sink.emit(ticket, None, EventKind::Warning, warning);
        }
        let seen = match look {
            Look::Read(seen) => seen,
            Look::Paused { .. } | Look::Unread { .. } => return,
        };
        let mut replaced = None;
        let change =
            follow::changed(&stored.record, PIPELINE, &seen, now).and_then(|change| match change {
                Some(changed) => {
                    replaced = changed.replaced;
                    self.write_seen(ticket, stored, &changed.record)
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
            // read once the pass ran no correction.
            if matches!(
                seen.decision,
                Decision::Follow {
                    adopt: false,
                    action: Action::Green | Action::WaitChecks | Action::WaitBaseRed,
                }
            ) && !self.correct(memory, sink, out, continued, ticket, stored, at.clone())
            {
                self.review(memory, sink, out, continued, ticket, stored, at);
            }
            return;
        }
        let held = Seen::at(at, None);
        let failing_for_ms = seen
            .failing_for
            .map(|failing| u64::try_from(failing.as_millis()).unwrap_or(u64::MAX));
        self.held_run(
            memory,
            sink,
            out,
            continued,
            ticket,
            held,
            StepKind::Repair { failing_for_ms },
            None,
        );
    }

    /// Runs the step `kind` watch decided for `ticket` from `held`, unless
    /// a hold or a pause keeps it back ([`Holds::admit`]), with watch's
    /// memory of the pull requests it follows and, for a review loop, the
    /// reviews watch read ([`Self::step`]): a step that found the ref moved
    /// ran nothing. Its outcome is told, then what it did is held as a
    /// continue is ([`Holds::after`]), from the ticket ref read after it.
    /// Returns whether it ran.
    #[allow(clippy::too_many_arguments)]
    fn held_run(
        &self,
        memory: &mut Memory,
        sink: &mut EventSink<'_>,
        out: &mut dyn Write,
        continued: &mut Continued<'_>,
        ticket: &TicketId,
        held: Seen,
        kind: StepKind,
        reviews: Option<ReviewsRead>,
    ) -> bool {
        let on_demand = self.on_demand;
        let now = (on_demand.clock)();
        if memory.holds.admit(ticket, &held, now) != Admit::Go {
            return false;
        }
        let envelope = self.envelope(memory, ticket, kind, &held);
        let end = self.step(memory, sink, out, continued, envelope, reviews);
        if end == StepEnd::Stale {
            return false;
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
            &end,
            progress.as_ref(),
            holds_now,
        );
        true
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

    /// Carries out `envelope`'s step (OWL-399): in watch's own process,
    /// with its memory of the pull requests it follows and `reviews`, the
    /// outcome told to `continued` as a continue's; or in a child, whose
    /// record gives back that memory, the child having reported the outcome
    /// itself. A child that leaves no record, or one that does not read, is
    /// a `step_unreported` warning, and the step is held as a refusal is.
    /// Returns how the step ended.
    fn step(
        &self,
        memory: &mut Memory,
        sink: &mut EventSink<'_>,
        out: &mut dyn Write,
        continued: &mut Continued<'_>,
        envelope: StepEnvelope,
        reviews: Option<ReviewsRead>,
    ) -> StepEnd {
        let on_demand = self.on_demand;
        let ticket = envelope.ticket.clone();
        let Some(children) = memory.children else {
            let stepped = step::run(on_demand, &envelope, &mut memory.following, reviews, sink);
            if let step::Stepped::Ran(outcome) = &stepped
                && step::reported(&envelope.step, outcome)
            {
                continued(&ticket, outcome, sink);
            }
            return step::end(&stepped);
        };
        match children.run(on_demand.dirs, &envelope, out) {
            Ok(record) => {
                memory.following.keep(&ticket, record.kept);
                record.end
            }
            Err(reason) => {
                sink.emit(
                    &ticket,
                    None,
                    EventKind::Warning,
                    data([
                        ("what", json!("step_unreported")),
                        ("attempt", json!(envelope.attempt)),
                        ("reason", json!(reason)),
                    ]),
                );
                StepEnd::Other
            }
        }
    }

    /// Reads the outcome records a predecessor left (OWL-399) and keeps the
    /// hold each calls for while its ticket's ref is where the record says,
    /// for the first pass to apply; every record is removed. One that does
    /// not read is a `step_unreported` warning of the project.
    fn adopt_leftovers(&self, memory: &mut Memory, sink: &mut EventSink<'_>) {
        let on_demand = self.on_demand;
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
            let current = ticket_ref::read(
                &on_demand.executor.git,
                &on_demand.dirs.checkout(),
                &record.ticket,
            );
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
    /// review is due at that pass.
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
    ) {
        let on_demand = self.on_demand;
        let Some(forge) = &on_demand.config.forge else {
            return;
        };
        let Some(delivery) = &stored.record.delivery else {
            return;
        };
        let Some(pr) = &delivery.pull_request else {
            return;
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
            return;
        };
        let (read, warnings) = memory
            .following
            .reviews(on_demand.forge, ticket, pr.number, now);
        for warning in warnings {
            sink.emit(ticket, None, EventKind::Warning, warning);
        }
        let Some(read) = read else {
            return;
        };
        let answered = stored
            .record
            .review
            .as_ref()
            .map(|review| review.answered_reviews.clone())
            .unwrap_or_default();
        let due = pr_reviews::due(&read.reviews, &forge.reviewers, delivered_at, &answered);
        if due.is_empty() {
            return;
        }
        let decider = match on_demand.tracker.ticket(ticket) {
            Ok(found) => on_demand::known_decider(&found),
            Err(e) => {
                let reason = format!("reading the ticket: {e}");
                return self.unreadable(memory, sink, ticket, reason);
            }
        };
        if decider
            .as_ref()
            .is_some_and(|decider| !pr_reviews::instructed(&due, &decider.id))
        {
            return;
        }
        // The loop's Build holds the slot: it starts in a free turn only,
        // read again at a later pass (OWL-367).
        match memory.turn.clone() {
            Some(Turn::Holder(holder)) => {
                return self.wait_for_slot(memory, sink, out, ticket, vec![holder]);
            }
            Some(Turn::Unknown(unread)) => {
                return self.wait_for_slot(memory, sink, out, ticket, unread);
            }
            Some(Turn::Free) | None => {}
        }
        let held = Seen::at(at, pr_reviews::newest(&due));
        self.held_run(
            memory,
            sink,
            out,
            continued,
            ticket,
            held,
            StepKind::Review,
            Some(read),
        );
    }

    /// Dates the delivery of `ticket`, whose ref is at `seen`, at `now`
    /// (OWL-307), under the project's lock: the delivery was made before
    /// OWL-307 kept its date, so the reviews already on its pull request
    /// were never this delivery's to answer. Nothing is written when the
    /// project is busy or the ref moved.
    fn date_delivery(&self, ticket: &TicketId, seen: &str, now: Timestamp) -> Result<(), String> {
        let Some(_lock) = self.locked_at(ticket, seen)? else {
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
        ticket_ref::write(git, &checkout, ticket, &record, Some(seen)).map(|_| ())
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
    /// busy, the ref moved or a run left the project unverified, nothing is
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
        let Some(_lock) = self.locked_at(ticket, &stored.commit)? else {
            return Ok(None);
        };
        if !matches!(on_demand.dirs.unverified(), Ok(None)) {
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
        let git = &on_demand.executor.git;
        let written = ticket_ref::write(
            git,
            &on_demand.dirs.checkout(),
            ticket,
            &changed.record,
            Some(&stored.commit),
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
        if !matches!(on_demand.dirs.unverified(), Ok(None)) {
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
    /// lock. Returns whether a correction ran.
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
    ) -> bool {
        let on_demand = self.on_demand;
        let questions = &stored.record.questions;
        if questions.decisions.is_empty() {
            return false;
        }
        let comments = match on_demand.tracker.comments(ticket) {
            Ok(comments) => comments,
            Err(e) => {
                let reason = format!("reading its comments: {e}");
                self.unreadable(memory, sink, ticket, reason);
                return false;
            }
        };
        if reversal::reversals(&comments, questions, Current::Unresolved).is_empty() {
            return false;
        }
        let decider = match on_demand.tracker.ticket(ticket) {
            Ok(found) => on_demand::known_decider(&found),
            Err(e) => {
                let reason = format!("reading the ticket: {e}");
                self.unreadable(memory, sink, ticket, reason);
                return false;
            }
        };
        let current = decider.as_ref().map_or(Current::Unresolved, Current::Known);
        let reversed = reversal::reversals(&comments, questions, current);
        let now = (on_demand.clock)();
        let window = on_demand.config.policy.quiet_window();
        if reversed.is_empty() || reversal::counts_at(&reversed, now, window).is_some() {
            return false;
        }
        let held = Seen::at(
            at,
            reversal::newest(&reversed).map(|reply| reply.last_edit()),
        );
        self.held_run(
            memory,
            sink,
            out,
            continued,
            ticket,
            held,
            StepKind::Correct,
            None,
        )
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
        let Some(_lock) = self.locked_at(ticket, &seen.commit)? else {
            return Ok(None);
        };
        let git = &self.on_demand.executor.git;
        let commit = ticket_ref::write(
            git,
            &self.on_demand.dirs.checkout(),
            ticket,
            record,
            Some(&seen.commit),
        )?;
        Ok(Some(RefAt {
            commit,
            progress: ticket_ref::progress(record, seen.proposals.as_ref()),
        }))
    }

    /// The project's lock, once the ticket ref read under it is still
    /// `seen`, the commit the snapshot read: `None` when the project is busy
    /// or the ref moved.
    fn locked_at(&self, ticket: &TicketId, seen: &str) -> Result<Option<ProjectLock>, String> {
        let dirs = self.on_demand.dirs;
        let lock = match dirs.lock() {
            Ok(Some(lock)) => lock,
            Ok(None) => return Ok(None),
            Err(e) => return Err(format!("the project's lock: {e}")),
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
        if !checked_out && fs::symlink_metadata(dirs.unverified_file()).is_err() {
            memory.quarantine = None;
            return Ok(Snapshot::default());
        }
        let lock = match dirs.lock() {
            Ok(Some(lock)) => lock,
            Ok(None) => return Err(Project::Busy),
            Err(e) => return Err(Project::Unread(format!("the project's lock: {e}"))),
        };
        let quarantine = match dirs.unverified() {
            Ok(quarantine) => quarantine,
            Err(e) => return Err(Project::Unread(format!("the project's marker: {e}"))),
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
        if memory.holds.admit(ticket, &seen, now) != Admit::Go {
            return;
        }
        // The RESUME names the ticket whose turn the slot is (OWL-367).
        let held_by = match &memory.turn {
            Some(Turn::Holder(holder)) if holder != ticket => Some(holder.clone()),
            _ => None,
        };
        // The step records watch's `decision` event once the ticket ref is
        // confirmed under the lock, so the log never says watch continued a
        // ticket it left.
        let kind = StepKind::Answer {
            slot_held_by: held_by,
        };
        let envelope = self.envelope(memory, ticket, kind, &seen);
        let end = self.step(memory, sink, out, continued, envelope, None);
        if end == StepEnd::Stale {
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
            &end,
            progress.as_ref(),
            holds_now,
        ) {
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
            Some(Admit::Paused { until }) => self.line(
                out,
                &format!("no ticket is continued before the usage limit resets at {until}"),
            ),
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
        // another ticket's turn holds it back, told once per holder.
        if shown == VisibleStage::Working {
            match memory.turn.clone() {
                Some(Turn::Holder(holder)) if holder != *ticket => {
                    return self.wait_for_slot(memory, sink, out, ticket, vec![holder]);
                }
                Some(Turn::Unknown(unread)) => {
                    return self.wait_for_slot(memory, sink, out, ticket, unread);
                }
                _ => {
                    memory.slot_waits.remove(ticket);
                }
            }
        }
        let seen = Seen::at(at, None);
        if memory.holds.admit(ticket, &seen, now) != Admit::Go {
            return;
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
        let end = self.step(memory, sink, out, continued, envelope, None);
        if end == StepEnd::Stale {
            // The ref moved, or the ticket was moved, since the reads: the
            // next pass reads it again.
            return;
        }
        let after = self.read_after(ticket);
        let progress = after.as_ref().map(Stored::progress);
        let holds_now = after.as_ref().is_some_and(holds);
        match self.after(
            memory,
            sink,
            out,
            ticket,
            seen,
            &end,
            progress.as_ref(),
            holds_now,
        ) {
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
            Some(Admit::Paused { until }) => self.line(
                out,
                &format!("no ticket is continued before the usage limit resets at {until}"),
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
        // A ref that holds the slot now takes a free turn, or waits for the
        // holder's, told in this very pass; a holder that left the slot
        // gives it at the next pass, at once.
        match memory.turn.clone().unwrap_or(Turn::Free) {
            Turn::Free if holds_now => {
                memory.turn = Some(Turn::Holder(ticket.clone()));
                memory.slot_waits.remove(ticket);
            }
            Turn::Holder(holder) if holder == *ticket => {
                if holds_now {
                    memory.slot_waits.remove(ticket);
                } else if moved {
                    memory.stepped = true;
                }
            }
            Turn::Holder(holder) if holds_now => {
                self.wait_for_slot(memory, sink, out, ticket, vec![holder]);
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
        if memory.slot_waits.get(ticket) == Some(&holders) {
            return;
        }
        let names: Vec<&str> = holders.iter().map(TicketId::as_str).collect();
        sink.emit(
            ticket,
            None,
            EventKind::Decision,
            data([
                ("watch", json!("waits")),
                ("reason", json!("slot")),
                ("tickets", json!(names)),
            ]),
        );
        self.line(
            out,
            &format!(
                "{ticket} waits: the project's slot is held by {}, and its next stage runs once \
                 the slot is its turn (`owlshift doctor` says how to clear a stale one)",
                names.join(", ")
            ),
        );
        memory.slot_waits.insert(ticket.clone(), holders);
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
                self.on_demand.dirs.unverified_file().display()
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

    /// A usage limit with a reset time holds every ticket until then; one
    /// without falls under the ref rule.
    #[test]
    fn a_usage_limit_pauses_every_continue_until_it_resets() {
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
            Some(Admit::Paused { until: at(30) })
        );
        assert_eq!(
            holds.admit(&two, &seen("d1", 0), at(29)),
            Admit::Paused { until: at(30) }
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
        holds.after(&ticket, seen("p1", 0), &end(not_kept), Some(&p("p1")), at(6));
        assert_eq!(
            holds.admit(&ticket, &seen_at("spent", "p1", 0), at(24 * 60)),
            Admit::Held {
                until: Until::Person
            }
        );
        assert_eq!(holds.admit(&ticket, &seen("p2", 0), at(24 * 60)), Admit::Go);
    }
}
