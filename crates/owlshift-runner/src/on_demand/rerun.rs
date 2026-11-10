//! The automatic re-run of a red check (OWL-513; architecture section 5,
//! "The automatic re-run"): when every failed check of a followed pull
//! request is a GitHub Actions job's code failure the base does not
//! explain, the runner re-runs those jobs once per head before any fix run.
//! The claim is kept in `delivery.json` before the call, so a head is
//! re-run once whatever happens next; a refusal goes on to the fix run in
//! the same repair.

use super::*;
use owlshift_adapters::forge::RerunAnswer;
use owlshift_contracts::refs::{Rerun, RerunJob, RerunOutcomeKind};

use crate::writer::{RerunComment, WriteError, Writer};

/// What [`OnDemand::rerun_checks`] leaves to its repair when it does not
/// stop it: GitHub refused the re-run, and the fix run follows on the
/// delivery as the ref now keeps it.
pub(super) struct RerunRefused {
    pub delivery: TicketDelivery,
}

impl OnDemand<'_> {
    /// Re-runs the failed jobs of `fresh`, the read of `pr` at `pushed`
    /// under this command's lock, once per workflow run: the claim first,
    /// written to the ref, then one call per run through the Writer. Every
    /// run started stops the repair with a look, nothing run or counted;
    /// a call with no verdict keeps the claim, warned of, and stops the
    /// same way, the hour's `unseen` outcome bounding the wait; a spent
    /// budget before any run started gives the claim back. A refusal keeps
    /// the outcome `refused` with GitHub's words, warned of, and returns
    /// the delivery as kept, for the fix run that follows.
    pub(super) fn rerun_checks(
        &self,
        p: &mut Prepared,
        state: &TicketState,
        delivery: &TicketDelivery,
        pr: &DeliveredPullRequest,
        fresh: &follow::Fresh,
        sink: &mut EventSink<'_>,
    ) -> Result<RerunRefused, Stop> {
        let ticket = p.ticket.clone();
        let head = fresh.checks.head.to_string();
        let jobs: Vec<RerunJob> = fresh
            .checks
            .checks
            .iter()
            .filter(|c| c.state == CheckState::Failed)
            .filter_map(|c| {
                Some(RerunJob {
                    name: c.name.clone(),
                    id: c.id?,
                    run: c.run?,
                    url: c.url.clone(),
                    again: None,
                })
            })
            .collect();
        // An outcome of an earlier head still to be told gets one more try
        // before this claim replaces it, then is dropped.
        self.tell_replaced_rerun(&ticket, delivery, sink);
        let mut runs: Vec<u64> = jobs.iter().map(|job| job.run).collect();
        runs.sort_unstable();
        runs.dedup();
        if runs.is_empty() {
            return Err(Stop::Refused(format!(
                "pull request #{}'s failed checks name no workflow run to re-run",
                pr.number
            )));
        }
        let mut claimed = delivery.clone();
        claimed.rerun = Some(Rerun {
            head: head.clone(),
            at: (self.clock)(),
            runs: runs.clone(),
            jobs,
            outcome: None,
            reason: None,
            told: false,
        });
        self.keep_rerun(p, state, &claimed)?;
        let writer = Writer::new(self.tracker);
        let mut started = Vec::new();
        for run in &runs {
            let answer = match writer.rerun_failed_jobs(self.forge, *run) {
                Ok(answer) => answer,
                // Nothing ran: the claim is given back, for the next read.
                Err(WriteError::Forge(error))
                    if error.kind == ErrorKind::RateLimited && started.is_empty() =>
                {
                    self.keep_rerun(p, state, delivery)?;
                    return Err(Stop::Refused(format!(
                        "re-running pull request #{}'s failed checks: GitHub's rate limit is \
                         spent ({error}); the next read tries again",
                        pr.number
                    )));
                }
                Err(error) => RerunAnswer::Refused {
                    permission: false,
                    status: 0,
                    message: error.to_string(),
                },
            };
            match answer {
                RerunAnswer::Started => started.push(*run),
                RerunAnswer::NoVerdict { reason } => {
                    sink.emit(
                        &ticket,
                        None,
                        EventKind::Warning,
                        data([
                            ("what", json!("rerun_unanswered")),
                            ("pull_request", json!(pr.number)),
                            ("head", json!(head)),
                            ("run", json!(run)),
                            ("reason", json!(reason)),
                        ]),
                    );
                }
                RerunAnswer::Refused {
                    permission,
                    status,
                    message,
                } if started.is_empty() => {
                    let reason = if status == 0 {
                        message.clone()
                    } else {
                        format!("HTTP {status} {message}")
                    };
                    if let Some(rerun) = claimed.rerun.as_mut() {
                        rerun.outcome = Some(RerunOutcomeKind::Refused);
                        rerun.reason = Some(reason.clone());
                    }
                    self.keep_rerun(p, state, &claimed)?;
                    sink.emit(
                        &ticket,
                        None,
                        EventKind::Warning,
                        data([
                            ("what", json!("rerun_refused")),
                            ("pull_request", json!(pr.number)),
                            ("head", json!(head)),
                            ("run", json!(run)),
                            ("permission", json!(permission)),
                            ("reason", json!(reason)),
                        ]),
                    );
                    return Ok(RerunRefused { delivery: claimed });
                }
                // A later run refused after one started: its jobs show no
                // newer attempt, and the hour's `unseen` outcome sends the
                // head to a fix run.
                RerunAnswer::Refused {
                    permission,
                    status,
                    message,
                } => {
                    sink.emit(
                        &ticket,
                        None,
                        EventKind::Warning,
                        data([
                            ("what", json!("rerun_refused")),
                            ("pull_request", json!(pr.number)),
                            ("head", json!(head)),
                            ("run", json!(run)),
                            ("permission", json!(permission)),
                            ("reason", json!(format!("HTTP {status} {message}"))),
                        ]),
                    );
                }
            }
        }
        let checks: Vec<&str> = claimed
            .rerun
            .as_ref()
            .map(|rerun| rerun.jobs.iter().map(|job| job.name.as_str()).collect())
            .unwrap_or_default();
        sink.emit(
            &ticket,
            None,
            EventKind::Decision,
            data([
                ("watch", json!("rerun")),
                ("pull_request", json!(pr.number)),
                ("head", json!(head)),
                ("runs", json!(started)),
                ("checks", json!(checks)),
            ]),
        );
        Err(Stop::Watched {
            ticket,
            seen: Box::new(fresh.seen(pr.number, &pr.url)),
            merge: Some(
                "GitHub re-runs its failed jobs once; watch's next passes read the new attempts."
                    .to_owned(),
            ),
        })
    }

    /// Keeps `delivery`, its re-run as changed, in the ticket ref.
    fn keep_rerun(
        &self,
        p: &mut Prepared,
        state: &TicketState,
        delivery: &TicketDelivery,
    ) -> Result<(), Stop> {
        let questions = p.questions();
        self.store_delivery(p, state, questions, Some(delivery.clone()))
            .map_err(|e| {
                Stop::Refused(format!(
                    "keeping the re-run of the pull request's checks in the ticket's ref: {e}"
                ))
            })
    }

    /// The RERUN comment of `delivery`'s re-run of an earlier head, its
    /// outcome known and not told: tried once more before a new claim
    /// replaces it, then dropped whatever the tracker answers, with a
    /// `rerun_report_dropped` warning when refused.
    fn tell_replaced_rerun(
        &self,
        ticket: &TicketId,
        delivery: &TicketDelivery,
        sink: &mut EventSink<'_>,
    ) {
        let (Some(pr), Some(rerun)) = (&delivery.pull_request, &delivery.rerun) else {
            return;
        };
        if !tells(rerun) || rerun.told {
            return;
        }
        if let Err(reason) = self.post_rerun(ticket, pr, rerun, sink) {
            sink.emit(
                ticket,
                None,
                EventKind::Warning,
                data([
                    ("what", json!("rerun_report_dropped")),
                    ("head", json!(rerun.head)),
                    ("reason", json!(reason)),
                ]),
            );
        }
    }

    /// Posts the RERUN comment of `rerun` once ([`Writer::post_rerun`]),
    /// a new post being a `tracker_write` event; the tracker's refusal
    /// otherwise.
    fn post_rerun(
        &self,
        ticket: &TicketId,
        pr: &DeliveredPullRequest,
        rerun: &Rerun,
        sink: &mut EventSink<'_>,
    ) -> Result<(), String> {
        let comment = RerunComment {
            ticket: ticket.clone(),
            pull_request: pr.clone(),
            rerun: rerun.clone(),
        };
        let reported = Writer::new(self.tracker)
            .post_rerun(&comment)
            .map_err(|e| e.to_string())?;
        if reported.new {
            sink.emit(
                ticket,
                None,
                EventKind::TrackerWrite,
                data([
                    ("target", json!("tracker")),
                    ("action", json!("comment")),
                    ("kind", json!("RERUN")),
                    ("comment", json!(reported.comment.id)),
                ]),
            );
        }
        Ok(())
    }

    /// Settles the re-run `delivery` keeps of its pushed head (OWL-513;
    /// architecture section 5, "The automatic re-run", "The outcome"): with
    /// no outcome yet, the pull request is read again, and the core's
    /// [`follow::rerun_outcome`] on that read, once known, is kept with each
    /// job's newer attempt's link; an outcome known and not told is posted
    /// as the RERUN comment once and kept as told, a refused post warned of
    /// once per head and reason. What changed is the caller's to keep.
    fn settle_rerun_of(
        &self,
        ticket: &TicketId,
        delivery: &TicketDelivery,
        following: &mut Following,
        sink: &mut EventSink<'_>,
    ) -> RerunSettled {
        let (Some(pr), Some(pushed)) = (&delivery.pull_request, &delivery.pushed) else {
            return RerunSettled::default();
        };
        let Some(kept) = delivery.rerun.as_ref().filter(|r| r.head == *pushed) else {
            return RerunSettled::default();
        };
        if !owes(kept) {
            return RerunSettled::default();
        }
        let mut rerun = kept.clone();
        if rerun.outcome.is_none() {
            let checks = match self.forge.follow(pr.number) {
                Ok(read) => read.checks,
                // The look's own read tells of a forge that fails.
                Err(_) => return RerunSettled::default(),
            };
            if checks.head.as_str() != rerun.head {
                return RerunSettled {
                    delivery: None,
                    moved: true,
                };
            }
            let read = follow::read_of(&checks);
            let Some(mark) = follow::rerun_mark(delivery, &read.head, (self.clock)()) else {
                return RerunSettled::default();
            };
            let Some(outcome) = owlshift_core::follow::rerun_outcome(&read, &mark.mark()) else {
                return RerunSettled::default();
            };
            rerun.outcome = Some(follow::outcome_kind(outcome));
            for (job, first) in rerun.jobs.iter_mut().zip(&mark.first) {
                job.again = owlshift_core::follow::newer_attempts(&read.checks, first)
                    .filter_map(|attempt| attempt.id)
                    .max()
                    .and_then(|id| checks.checks.iter().find(|c| c.id == Some(id)))
                    .and_then(|check| check.url.clone());
            }
            sink.emit(
                ticket,
                None,
                EventKind::Decision,
                data([
                    ("watch", json!("rerun_outcome")),
                    ("pull_request", json!(pr.number)),
                    ("head", json!(rerun.head)),
                    (
                        "outcome",
                        json!(rerun.outcome.map(RerunOutcomeKind::as_str)),
                    ),
                ]),
            );
        }
        if tells(&rerun) && !rerun.told {
            match self.post_rerun(ticket, pr, &rerun, sink) {
                Ok(()) => {
                    rerun.told = true;
                    following.rerun_settled(ticket);
                }
                Err(reason) => {
                    if following.rerun_unposted(ticket, &rerun.head, &reason) {
                        sink.emit(
                            ticket,
                            None,
                            EventKind::Warning,
                            data([
                                ("what", json!("rerun_report_unposted")),
                                ("head", json!(rerun.head)),
                                ("reason", json!(reason)),
                            ]),
                        );
                    }
                }
            }
        }
        if rerun == *kept {
            return RerunSettled::default();
        }
        RerunSettled {
            delivery: Some(TicketDelivery {
                rerun: Some(rerun),
                ..delivery.clone()
            }),
            moved: false,
        }
    }

    /// For `owlshift watch`, right after the owed report: settles the
    /// re-run of the head `seen` read ([`Self::settle_rerun_of`]) under the
    /// project's lock and the ticket's, while the ticket ref is still at
    /// `at`'s commit, and keeps what changed. Nothing is locked or read
    /// unless the ref, as `stored` keeps it, has that head's re-run with an
    /// outcome to read or to tell. Where the ref is after it: `at` when a
    /// lock is busy, a quarantine stands or the ref moved, for the next pass;
    /// `None` when the pull request's head moved since the look, which ends
    /// the pass.
    pub fn post_rerun_report(
        &self,
        ticket: &TicketId,
        stored: &Stored,
        seen: &Seen,
        at: RefAt,
        following: &mut Following,
        sink: &mut EventSink<'_>,
    ) -> Option<RefAt> {
        let due = matches!(seen.decision, FollowDecision::Follow { .. })
            && stored
                .record
                .delivery
                .as_ref()
                .and_then(|delivery| delivery.rerun.as_ref())
                .is_some_and(|rerun| rerun.head == seen.head && owes(rerun));
        if !due {
            return Some(at);
        }
        let Ok(Some((_project, ticket_lock))) = self.dirs.lock_for_write(ticket) else {
            return Some(at);
        };
        if !matches!(self.dirs.quarantine(), Ok(None)) {
            return Some(at);
        }
        let git = &self.executor.git;
        let checkout = self.dirs.checkout();
        let current = match ticket_ref::read(git, &checkout, ticket) {
            Ok(Some(current)) if current.commit == at.commit => current,
            _ => return Some(at),
        };
        let Some(delivery) = &current.record.delivery else {
            return Some(at);
        };
        let settled = self.settle_rerun_of(ticket, delivery, following, sink);
        if settled.moved {
            return None;
        }
        let Some(delivery) = settled.delivery else {
            return Some(at);
        };
        let record = TicketRecord {
            delivery: Some(delivery),
            ..current.record
        };
        match ticket_run::write_ref(
            git,
            self.dirs,
            &checkout,
            &ticket_lock,
            ticket,
            &record,
            ProposalsFile::Keep,
            Some(&at.commit),
        )
        .map_err(|fault| self.ref_fault(ticket, fault))
        {
            Ok(commit) => Some(RefAt {
                commit,
                progress: ticket_ref::progress(&record, current.proposals.as_ref()),
            }),
            Err(reason) => {
                sink.emit(
                    ticket,
                    None,
                    EventKind::Warning,
                    data([("what", json!("rerun_unkept")), ("reason", json!(reason))]),
                );
                Some(at)
            }
        }
    }

    /// For `owlshift continue` at Watch, under its lock, right after the
    /// owed report: settles the re-run of the pushed head
    /// ([`Self::settle_rerun_of`]) and keeps what changed, so the repair
    /// that follows reads it. A head that moved since stops the command
    /// before any repair: the next read decides.
    pub(super) fn settle_rerun(
        &self,
        p: &mut Prepared,
        following: &mut Following,
        sink: &mut EventSink<'_>,
    ) -> Result<(), Stop> {
        let Some(stored) = p.stored.clone() else {
            return Ok(());
        };
        let Some(delivery) = &stored.record.delivery else {
            return Ok(());
        };
        let settled = self.settle_rerun_of(&p.ticket, delivery, following, sink);
        if settled.moved {
            return Err(Stop::Refused(format!(
                "the head of {}'s pull request moved while its re-run was read: the next read \
                 decides",
                p.ticket
            )));
        }
        if let Some(delivery) = settled.delivery {
            let state = TicketState::try_from(&stored.record.state).map_err(core_error)?;
            self.store_delivery(p, &state, stored.record.questions, Some(delivery))
                .map_err(|e| refused("keeping the re-run's outcome in the ticket's ref", e))?;
        }
        Ok(())
    }
}

/// What [`OnDemand::settle_rerun_of`] settled.
#[derive(Default)]
struct RerunSettled {
    /// The delivery to keep, its re-run's outcome or told changed.
    delivery: Option<TicketDelivery>,
    /// The pull request's head moved since: nothing was kept.
    moved: bool,
}

/// Whether a re-run's outcome is one a RERUN comment tells.
fn tells(rerun: &Rerun) -> bool {
    matches!(
        rerun.outcome,
        Some(RerunOutcomeKind::Passed | RerunOutcomeKind::Failed | RerunOutcomeKind::Unseen)
    )
}

/// Whether a re-run still has something to settle: its outcome to read, or
/// to tell.
fn owes(rerun: &Rerun) -> bool {
    rerun.outcome.is_none() || (tells(rerun) && !rerun.told)
}
