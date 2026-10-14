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
//! Since OWL-273, a ticket an answered intake round left at Intake
//! (`left_at_intake`), its second intake cut off by a usage limit or a
//! refusal, is resumed while the tracker still shows it needs input: its
//! intake runs again through [`OnDemand::continue_shown`], held as a
//! continue is. One shown elsewhere, in the ready column (the drain's) or
//! moved by a person, is left alone, its stage read again once its ref moves
//! or after [`RETRY_AFTER`].
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

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::time::Duration;

use jiff::Timestamp;
use serde_json::json;

use owlshift_adapters::tracker::Person;
use owlshift_contracts::event::EventKind;
use owlshift_contracts::ids::TicketId;
use owlshift_core::follow::{Action, Decision};
use owlshift_core::state::{Status, TicketState};
use owlshift_core::vocab::Stage;

use crate::answer_check::{self, Readiness};
use crate::events::{Data, EventSink, data, printable};
use crate::follow::{self, Following, Look};
use crate::on_demand::{self, Delivered, Landed, OnDemand, PIPELINE, Stop};
use crate::project::ProjectLock;
use crate::reversal::{self, Current};
use crate::ticket_ref::{self, Stored, TicketRecord};
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
/// ended, [`outlived`]), and one at a stage waits for no answer.
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

/// Whether watch resumes a ticket in `state` once the tracker shows it still
/// needs input (OWL-273): at Intake after a question round, as an answered
/// intake round leaves it while intake runs again, and a cut-off run leaves
/// it. A ticket at Intake that asked no round is the drain's.
fn left_at_intake(state: &TicketState) -> bool {
    state.status() == Status::Active(Stage::Intake) && state.round() > 0
}

/// What a continue starts from: the ticket ref's commit and the decider's
/// newest edit when watch decided to continue.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Seen {
    pub commit: String,
    pub answer: Option<Timestamp>,
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
/// ticket ref moves first.
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
    /// another ticket ref or answer is lifted.
    pub fn admit(&mut self, ticket: &TicketId, seen: &Seen, now: Timestamp) -> Admit {
        if let Some(until) = self.paused_until {
            if now < until {
                return Admit::Paused { until };
            }
            self.paused_until = None;
        }
        match self.held.get(ticket) {
            Some((held, until)) if held == seen && until.holds_at(now) => {
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
    /// `commit`, its ticket ref's commit read after it (`None` when gone or
    /// unreadable), at `now`. Returns the hold or the pause it set, if any.
    ///
    /// A busy project ran nothing: no hold. A usage limit with a reset time
    /// pauses every continue until then. A comment, a RESUME included, or a
    /// park whose comment could not be posted, whose state was not kept
    /// ([`Stop::NotKept`]) holds the ticket until a person acts, unless the
    /// ref was read moved: an unreadable ref is no reason to post the comment
    /// or park again. Otherwise a ticket ref left as it was holds the ticket for
    /// [`RETRY_AFTER`]; one that moved is progress, which the next pass reads.
    /// No other way to stop is told apart, so a way added later is held
    /// exactly when it leaves the ref as it was.
    pub fn after(
        &mut self,
        ticket: &TicketId,
        seen: Seen,
        outcome: &Result<Delivered, Stop>,
        commit: Option<&str>,
        now: Timestamp,
    ) -> Option<Admit> {
        if matches!(outcome, Err(Stop::Busy(_))) {
            return None;
        }
        if let Err(Stop::UsageLimit {
            resets_at: Some(at),
        }) = outcome
            && *at > now
        {
            let until = self.paused_until.map_or(*at, |paused| paused.max(*at));
            self.paused_until = Some(until);
            return Some(Admit::Paused { until });
        }
        let as_it_was = commit == Some(seen.commit.as_str());
        let until = match outcome {
            Err(Stop::NotKept { .. }) if as_it_was || commit.is_none() => Until::Person,
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
    /// A run's isolation check did not pass: a person looks first.
    Quarantined,
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
struct Memory {
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
    /// The tickets left at Intake found shown in another stage than needs
    /// input (OWL-273): the commit their ref was at, and until when their
    /// stage is not read again unless the ref moves.
    elsewhere: HashMap<TicketId, (String, Timestamp)>,
}

/// What watch keeps, per ticket, of the stages a stop left at working
/// (OWL-265, architecture section 5, "The stage while a repair runs").
#[derive(Default)]
struct Stale {
    /// The ticket ref's commit when watch last moved its stage: no stage is
    /// read again, nor a person's working moved, until the ref moves.
    moved: HashMap<TicketId, String>,
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
        self.run_passes(false, sink, out, continued, sleep);
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
        self.run_passes(true, sink, out, continued, sleep);
    }

    fn run_passes(
        &self,
        draining: bool,
        sink: &mut EventSink<'_>,
        out: &mut dyn Write,
        continued: &mut Continued<'_>,
        sleep: &mut dyn FnMut(Duration) -> bool,
    ) {
        let mut memory = Memory::default();
        loop {
            self.pass(&mut memory, sink, out, continued);
            if draining {
                self.drain(&mut memory, sink, out, continued);
            }
            if !sleep(self.interval) {
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
        let read = match self.snapshot() {
            Ok(read) => read,
            Err(project) => return self.note(memory, out, project),
        };
        let mut waiting = Vec::new();
        let mut following = Vec::new();
        let mut resumable = Vec::new();
        // A ticket whose ref cannot be read now keeps its hold: only one
        // read and found not waiting, or gone from the list, loses it.
        let mut unread = Vec::new();
        for (ticket, stored) in read {
            let state = stored.and_then(|stored| {
                TicketState::try_from(&stored.record.state)
                    .map(|state| (stored, state))
                    .map_err(|e| format!("its ticket ref keeps no valid state: {e:?}"))
            });
            match state {
                // A ticket parked at Watch is read for an end alone (OWL-243),
                // its comments never: only a waiting ticket's are.
                Ok((stored, state))
                    if watched(state.status()) || outlived(state.status(), &stored) =>
                {
                    let outlived = outlived(state.status(), &stored);
                    waiting.push((ticket, stored, state.status(), outlived));
                }
                Ok((stored, state)) if followed(state.status(), &stored) => {
                    memory.failing.remove(&ticket);
                    following.push((ticket, stored));
                }
                // Its failing read is cleared once its stage reads (OWL-273).
                Ok((stored, state)) if left_at_intake(&state) => {
                    resumable.push((ticket, stored));
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
            .filter(|(_, _, status, _)| watched(*status))
            .map(|(ticket, _, _, _)| ticket.clone())
            .collect();
        // A waiting or parked ticket whose pull request is read keeps the
        // forge failure warned of (OWL-242, OWL-243).
        let read: Vec<TicketId> = waiting
            .iter()
            .filter(|(_, _, _, outlived)| *outlived)
            .map(|(ticket, _, _, _)| ticket.clone())
            .collect();
        let followed: Vec<TicketId> = following.iter().map(|(ticket, _)| ticket.clone()).collect();
        let left: Vec<TicketId> = resumable.iter().map(|(ticket, _)| ticket.clone()).collect();
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
        memory
            .elsewhere
            .retain(|ticket, _| left.contains(ticket) || unread.contains(ticket));
        memory
            .following
            .retain(&[&followed[..], &read[..], &unread[..]].concat());
        memory
            .stale
            .retain(&[&followed[..], &read[..], &unread[..]].concat());
        self.note(
            memory,
            out,
            Project::Watching {
                waiting: ids,
                followed,
            },
        );
        for (ticket, stored, status, outlived) in waiting {
            // A stage a stop left at working first (OWL-265): an end then
            // moves the stage it shows to review, which it would not do
            // from working, so the two may make two moves in one pass.
            if outlived {
                self.unstick(memory, sink, out, &ticket, &stored, status);
            }
            // A pass whose read ended the following reads no comment: an
            // answer posted with the merge starts nothing.
            if outlived && self.close_outlived(memory, sink, out, &ticket, &stored, status) {
                continue;
            }
            if watched(status) {
                self.look(memory, sink, out, continued, &ticket, &stored);
            }
        }
        for (ticket, stored) in resumable {
            self.resume_intake(memory, sink, out, continued, &ticket, &stored);
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
                    self.write_seen(ticket, &stored.commit, &changed.record)
                }
                None => Ok(Some(stored.commit.clone())),
            });
        let commit = match change {
            Ok(Some(commit)) => commit,
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
        let Some(commit) =
            on_demand.post_owed_report(ticket, stored, &seen, commit, &mut memory.following, sink)
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
            // (OWL-146).
            if matches!(
                seen.decision,
                Decision::Follow {
                    adopt: false,
                    action: Action::Green | Action::WaitChecks | Action::WaitBaseRed,
                }
            ) {
                self.correct(memory, sink, continued, ticket, stored, commit);
            }
            return;
        }
        let held = Seen {
            commit,
            answer: None,
        };
        if memory.holds.admit(ticket, &held, now) != Admit::Go {
            return;
        }
        let Some(outcome) = on_demand.repair_seen(
            ticket,
            &held.commit,
            seen.failing_for,
            &mut memory.following,
            sink,
        ) else {
            return;
        };
        continued(ticket, &outcome, sink);
        let git = &on_demand.executor.git;
        let after = ticket_ref::read(git, &on_demand.dirs.checkout(), ticket)
            .ok()
            .flatten()
            .map(|stored| stored.commit);
        let now = (on_demand.clock)();
        memory
            .holds
            .after(ticket, held, &outcome, after.as_deref(), now);
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
        let seen = match look {
            Look::Read(seen) if matches!(seen.decision, Decision::End(_)) => seen,
            _ => return false,
        };
        // The stage first: a round closed or a park ended in the ref is no
        // longer read, so a stage left at needs input would stay there.
        if let Err(reason) = on_demand.review_after_end(ticket, status, sink) {
            self.unreadable(memory, sink, ticket, reason);
            return true;
        }
        let change =
            follow::changed(&stored.record, PIPELINE, &seen, now).and_then(|change| match change {
                Some(changed) => self.write_seen(ticket, &stored.commit, &changed.record),
                None => Ok(None),
            });
        let commit = match change {
            Ok(Some(commit)) => commit,
            // Busy, or the ref moved: the next pass reads it again.
            Ok(None) => return true,
            Err(reason) => {
                let doing = if matches!(status, Status::Parked { .. }) {
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
        // Dropped, the following having ended.
        on_demand.post_owed_report(ticket, stored, &seen, commit, &mut memory.following, sink);
        true
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
        let Some(to) = VisibleStage::of(status) else {
            return;
        };
        let stale = &memory.stale;
        let now = (on_demand.clock)();
        if stale.moved.get(ticket) == Some(&stored.commit)
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
                    memory
                        .stale
                        .moved
                        .insert(ticket.clone(), stored.commit.clone());
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
    /// lock.
    fn correct(
        &self,
        memory: &mut Memory,
        sink: &mut EventSink<'_>,
        continued: &mut Continued<'_>,
        ticket: &TicketId,
        stored: &Stored,
        commit: String,
    ) {
        let on_demand = self.on_demand;
        let questions = &stored.record.questions;
        if questions.decisions.is_empty() {
            return;
        }
        let comments = match on_demand.tracker.comments(ticket) {
            Ok(comments) => comments,
            Err(e) => {
                let reason = format!("reading its comments: {e}");
                return self.unreadable(memory, sink, ticket, reason);
            }
        };
        if reversal::reversals(&comments, questions, Current::Unresolved).is_empty() {
            return;
        }
        let decider = match on_demand.tracker.ticket(ticket) {
            Ok(found) => on_demand::known_decider(&found),
            Err(e) => {
                let reason = format!("reading the ticket: {e}");
                return self.unreadable(memory, sink, ticket, reason);
            }
        };
        let current = decider.as_ref().map_or(Current::Unresolved, Current::Known);
        let reversed = reversal::reversals(&comments, questions, current);
        let now = (on_demand.clock)();
        let window = on_demand.config.policy.quiet_window();
        if reversed.is_empty() || reversal::counts_at(&reversed, now, window).is_some() {
            return;
        }
        let held = Seen {
            commit,
            answer: reversal::newest(&reversed).map(|reply| reply.last_edit()),
        };
        if memory.holds.admit(ticket, &held, now) != Admit::Go {
            return;
        }
        let Some(outcome) =
            on_demand.correct_seen(ticket, &held.commit, &mut memory.following, sink)
        else {
            return;
        };
        continued(ticket, &outcome, sink);
        let git = &on_demand.executor.git;
        let after = ticket_ref::read(git, &on_demand.dirs.checkout(), ticket)
            .ok()
            .flatten()
            .map(|stored| stored.commit);
        let now = (on_demand.clock)();
        memory
            .holds
            .after(ticket, held, &outcome, after.as_deref(), now);
    }

    /// Writes `record` as the ticket ref of `ticket` under the project's
    /// lock, only while the ref is still `seen`, the commit the snapshot
    /// read, and returns the ref's new commit: `None`, with nothing written,
    /// when the project is busy or the ref moved.
    fn write_seen(
        &self,
        ticket: &TicketId,
        seen: &str,
        record: &TicketRecord,
    ) -> Result<Option<String>, String> {
        let Some(_lock) = self.locked_at(ticket, seen)? else {
            return Ok(None);
        };
        let git = &self.on_demand.executor.git;
        ticket_ref::write(
            git,
            &self.on_demand.dirs.checkout(),
            ticket,
            record,
            Some(seen),
        )
        .map(Some)
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

    /// The ticket refs, read under the project's lock, once its marker says
    /// no run left it unverified. No dedicated checkout yet: nothing was
    /// asked.
    #[allow(clippy::type_complexity)]
    fn snapshot(&self) -> Result<Vec<(TicketId, Result<Stored, String>)>, Project> {
        let dirs = self.on_demand.dirs;
        let checkout = dirs.checkout();
        if fs::symlink_metadata(&checkout).is_err() {
            return Ok(Vec::new());
        }
        let _lock = match dirs.lock() {
            Ok(Some(lock)) => lock,
            Ok(None) => return Err(Project::Busy),
            Err(e) => return Err(Project::Unread(format!("the project's lock: {e}"))),
        };
        match dirs.unverified() {
            Ok(None) => {}
            Ok(Some(_)) => return Err(Project::Quarantined),
            Err(e) => return Err(Project::Unread(format!("the project's marker: {e}"))),
        }
        let git = &self.on_demand.executor.git;
        let tickets = ticket_ref::list(git, &checkout).map_err(Project::Unread)?;
        Ok(tickets
            .into_iter()
            .map(|ticket| {
                let stored = ticket_ref::read(git, &checkout, &ticket)
                    .and_then(|stored| stored.ok_or_else(|| "its ticket ref is gone".to_owned()));
                (ticket, stored)
            })
            .collect())
    }

    /// One waiting ticket: its comments, whether the reply counts, and the
    /// continue when it does.
    fn look(
        &self,
        memory: &mut Memory,
        sink: &mut EventSink<'_>,
        out: &mut dyn Write,
        continued: &mut Continued<'_>,
        ticket: &TicketId,
        stored: &Stored,
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
        let seen = Seen {
            commit: stored.commit.clone(),
            answer: answer_check::newest_decider_edit(&comments, &decider),
        };
        if memory.holds.admit(ticket, &seen, now) != Admit::Go {
            return;
        }
        // Recorded by `continue_seen` once the ticket ref is confirmed under
        // the lock, so the log never says watch continued a ticket it left.
        let decision = data([("watch", json!("continue")), ("answer", json!(seen.answer))]);
        let Some(outcome) = on_demand.continue_seen(ticket, &seen.commit, decision, sink) else {
            // The ticket ref moved since the snapshot: the next pass reads it.
            return;
        };
        continued(ticket, &outcome, sink);
        let git = &on_demand.executor.git;
        let commit = ticket_ref::read(git, &on_demand.dirs.checkout(), ticket)
            .ok()
            .flatten()
            .map(|stored| stored.commit);
        let now = (on_demand.clock)();
        match memory
            .holds
            .after(ticket, seen, &outcome, commit.as_deref(), now)
        {
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
                    outcome,
                    Err(Stop::NotKept {
                        landed: Landed::Resume,
                        ..
                    })
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

    /// A ticket left at Intake by an answered round whose second intake was
    /// cut off (OWL-273, architecture section 5, "Intake"): resumed, its
    /// intake run again, while the tracker still shows it needs input, held
    /// as a continue is ([`Holds`]). The stage is read first without the
    /// lock, so a ticket shown elsewhere costs no lock, checkout or ticket
    /// read, and [`OnDemand::continue_shown`] reads it again under the lock,
    /// so a person's move meanwhile is kept.
    fn resume_intake(
        &self,
        memory: &mut Memory,
        sink: &mut EventSink<'_>,
        out: &mut dyn Write,
        continued: &mut Continued<'_>,
        ticket: &TicketId,
        stored: &Stored,
    ) {
        let on_demand = self.on_demand;
        let now = (on_demand.clock)();
        if memory
            .elsewhere
            .get(ticket)
            .is_some_and(|(commit, until)| *commit == stored.commit && now < *until)
        {
            return;
        }
        let seen = Seen {
            commit: stored.commit.clone(),
            answer: None,
        };
        if memory.holds.admit(ticket, &seen, now) != Admit::Go {
            return;
        }
        let shown = match on_demand.tracker.stage(ticket) {
            Ok(shown) => shown,
            Err(e) => {
                let reason = format!("reading its stage: {e}");
                return self.unreadable(memory, sink, ticket, reason);
            }
        };
        memory.failing.remove(ticket);
        let needs_input = VisibleStage::NeedsInput;
        if shown != needs_input.state(&on_demand.config.tracker.states) {
            let until = now.checked_add(RETRY_AFTER).unwrap_or(Timestamp::MAX);
            memory
                .elsewhere
                .insert(ticket.clone(), (stored.commit.clone(), until));
            return;
        }
        memory.elsewhere.remove(ticket);
        // Recorded by `continue_shown` once the ticket ref and its stage are
        // confirmed under the lock.
        let decision = data([("watch", json!("continue")), ("stage", json!("intake"))]);
        let Some(outcome) =
            on_demand.continue_shown(ticket, &seen.commit, needs_input, decision, sink)
        else {
            // The ref moved, or the ticket was moved, since the reads: the
            // next pass reads it again.
            return;
        };
        continued(ticket, &outcome, sink);
        let repo = &on_demand.executor.git;
        let commit = ticket_ref::read(repo, &on_demand.dirs.checkout(), ticket)
            .ok()
            .flatten()
            .map(|stored| stored.commit);
        let now = (on_demand.clock)();
        match memory
            .holds
            .after(ticket, seen, &outcome, commit.as_deref(), now)
        {
            Some(Admit::Held {
                until: Until::At(until),
            }) => self.line(
                out,
                &format!(
                    "{ticket} is at Intake as it was before that resume: watch resumes it again \
                     at {until}, or sooner once a command moves its ticket ref"
                ),
            ),
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
            Project::Quarantined => format!(
                "a run's isolation check did not pass, so nothing is continued until a person \
                 looks: see {}",
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

/// What an end of the following adds to the `ended` event, and the words it
/// adds to watch's line, for a ticket in `status` whose ref keeps `round`:
/// `parked` for a park at Watch it ends (OWL-243), and the round for one
/// whose questions waited, closed unanswered (OWL-242).
fn ended_marks(status: Status, round: u32) -> (Data, String) {
    let mut marks = Data::new();
    let mut said = Vec::new();
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

    fn seen(commit: &str, answer: i64) -> Seen {
        Seen {
            commit: commit.to_owned(),
            answer: Some(at(answer)),
        }
    }

    fn refused() -> Result<Delivered, Stop> {
        Err(Stop::Refused("the tracker is down".to_owned()))
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
            holds.after(&ticket, seen("c1", 0), &busy, Some("c1"), at(1)),
            None
        );
        assert_eq!(holds.admit(&ticket, &seen("c1", 0), at(2)), Admit::Go);

        let held = holds.after(&ticket, seen("c1", 0), &refused(), Some("c1"), at(2));
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
        holds.after(&ticket, seen("c1", 0), &refused(), Some("c1"), at(2));
        assert_eq!(holds.admit(&ticket, &seen("c2", 0), at(3)), Admit::Go);
        // And the hold ends.
        holds.after(&ticket, seen("c1", 0), &refused(), Some("c1"), at(2));
        assert_eq!(holds.admit(&ticket, &seen("c1", 0), at(12)), Admit::Go);

        // A continue that moved the ref, a failed check kept for one, is
        // progress.
        assert_eq!(
            holds.after(&ticket, seen("c1", 0), &refused(), Some("c3"), at(13)),
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
            holds.after(&one, seen("c1", 0), &limited, Some("c1"), at(1)),
            Some(Admit::Paused { until: at(30) })
        );
        assert_eq!(
            holds.admit(&two, &seen("d1", 0), at(29)),
            Admit::Paused { until: at(30) }
        );
        assert_eq!(holds.admit(&one, &seen("c1", 0), at(30)), Admit::Go);

        let unknown = Err(Stop::UsageLimit { resets_at: None });
        assert_eq!(
            holds.after(&one, seen("c1", 0), &unknown, Some("c1"), at(31)),
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
            for commit in [Some("c1"), None] {
                let outcome = not_kept(landed);
                assert_eq!(
                    holds.after(&ticket, seen("c1", 0), &outcome, commit, at(1)),
                    person,
                    "{landed:?} {commit:?}"
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
            &not_kept(Landed::Reask),
            None,
            at(2),
        );
        assert_eq!(holds.admit(&ticket, &seen("c2", 0), at(3)), Admit::Go);

        // A round asked after a RESUME that was kept moved the ref.
        assert_eq!(
            holds.after(
                &ticket,
                seen("c1", 0),
                &not_kept(Landed::Questions),
                Some("c3"),
                at(4)
            ),
            None
        );
        assert_eq!(holds.admit(&ticket, &seen("c1", 0), at(5)), Admit::Go);
    }
}
