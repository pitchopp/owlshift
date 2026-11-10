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

use crate::writer::{WriteError, Writer};

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
    #[allow(clippy::too_many_arguments)]
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
}
