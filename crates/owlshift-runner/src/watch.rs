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
//! Nothing ends the loop but the operator: a ticket that cannot be read is a
//! `warning` event, once until it reads again, and the pass goes on. A
//! continue that left the ticket ref as it was is held for [`RETRY_AFTER`],
//! and a usage limit with a reset time holds every continue until then
//! ([`Holds`]), so a tracker that refuses writes does not cost an answer
//! check every pass.

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::time::Duration;

use jiff::Timestamp;
use serde_json::json;

use owlshift_adapters::tracker::Person;
use owlshift_contracts::event::EventKind;
use owlshift_contracts::ids::TicketId;
use owlshift_core::state::{Status, TicketState};

use crate::answer_check::{self, Readiness};
use crate::events::{EventSink, data, printable};
use crate::on_demand::{Delivered, OnDemand, Stop};
use crate::ticket_ref::{self, Stored};

/// How long watch sleeps after each pass.
pub const POLL_INTERVAL: Duration = Duration::from_secs(60);

/// How long a continue that left the ticket ref as it was holds that ticket
/// back, unless the decider edits a comment or the ref moves meanwhile.
pub const RETRY_AFTER: Duration = Duration::from_secs(10 * 60);

/// Whether watch continues a ticket in `status`: one whose questions wait
/// for its decider. A parked ticket waits for a person's `continue`, and one
/// at a stage waits for no answer.
pub fn watched(status: Status) -> bool {
    matches!(status, Status::NeedsInput { .. })
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
        until: Timestamp,
    },
    /// The harness reached its usage limit, which resets then.
    Paused {
        until: Timestamp,
    },
}

/// What holds continues back between passes. Pure: the caller reads the
/// clock and the ticket refs.
#[derive(Debug, Default)]
pub struct Holds {
    paused_until: Option<Timestamp>,
    held: HashMap<TicketId, (Seen, Timestamp)>,
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
            Some((held, until)) if held == seen && now < *until => Admit::Held { until: *until },
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
    /// pauses every continue until then. Otherwise a ticket ref left as it was
    /// holds the ticket for [`RETRY_AFTER`]; one that moved is progress, which
    /// the next pass reads. No other way to stop is told apart, so a way added
    /// later is held exactly when it leaves the ref as it was.
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
        if commit == Some(seen.commit.as_str()) {
            let until = now.checked_add(RETRY_AFTER).unwrap_or(Timestamp::MAX);
            self.held.insert(ticket.clone(), (seen, until));
            Some(Admit::Held { until })
        } else {
            self.held.remove(ticket);
            None
        }
    }

    /// Forgets the holds of the tickets `waiting` does not name.
    fn retain(&mut self, waiting: &[TicketId]) {
        self.held.retain(|ticket, _| waiting.contains(ticket));
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
    /// The tickets whose questions wait.
    Waiting(Vec<TicketId>),
}

/// What the loop keeps from one pass to the next.
#[derive(Default)]
struct Memory {
    holds: Holds,
    /// What was last printed about the project.
    project: Option<Project>,
    /// Why each ticket could not be read, as last warned of.
    failing: HashMap<TicketId, String>,
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
        let mut memory = Memory::default();
        loop {
            self.pass(&mut memory, sink, out, continued);
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
        for (ticket, stored) in read {
            let state = stored.and_then(|stored| {
                TicketState::try_from(&stored.record.state)
                    .map(|state| (stored, state))
                    .map_err(|e| format!("its ticket ref keeps no valid state: {e:?}"))
            });
            match state {
                Ok((stored, state)) if watched(state.status()) => waiting.push((ticket, stored)),
                Ok(_) => {
                    memory.failing.remove(&ticket);
                }
                Err(reason) => self.unreadable(memory, sink, &ticket, reason),
            }
        }
        let ids: Vec<TicketId> = waiting.iter().map(|(ticket, _)| ticket.clone()).collect();
        memory.holds.retain(&ids);
        self.note(memory, out, Project::Waiting(ids));
        for (ticket, stored) in waiting {
            self.look(memory, sink, out, continued, &ticket, &stored);
        }
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
            Some(Admit::Held { until }) => self.line(
                out,
                &format!(
                    "{ticket} is as it was before that continue: watch continues it again at \
                     {until}, or sooner once its decider edits a comment"
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
            Project::Waiting(tickets) if tickets.is_empty() => {
                "no ticket's questions wait for an answer".to_owned()
            }
            Project::Waiting(tickets) => format!(
                "questions wait for an answer on {}",
                tickets
                    .iter()
                    .map(TicketId::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use owlshift_contracts::Stage;

    use super::*;

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
        assert_eq!(held, Some(Admit::Held { until: at(12) }));
        assert_eq!(
            holds.admit(&ticket, &seen("c1", 0), at(11)),
            Admit::Held { until: at(12) }
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
            Some(Admit::Held { until: at(41) })
        );
        assert_eq!(holds.admit(&two, &seen("d1", 0), at(32)), Admit::Go);
    }
}
