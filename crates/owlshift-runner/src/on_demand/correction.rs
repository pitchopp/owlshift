//! The correction run (scenario S7, OWL-146; architecture section 5, "A
//! reversed decision"): once the ticket is delivered, the decider's replies
//! to the DECISION comments of decisions it keeps get a run of the build role
//! at Watch, which makes the pull request follow them. Its commit is pushed
//! as a fix run's is, with the lease on Owlshift's last push, and reported;
//! it counts no fix pass and no repair. Which replies count is
//! [`crate::reversal`]'s.

use super::*;
use crate::reversal::{self, Current, Reversed};

impl OnDemand<'_> {
    /// A correction for `owlshift watch`: run only while the ticket ref, read
    /// under the project's lock, is still `seen`, the commit watch read it at,
    /// and keeps a ticket at Watch. `None`, with nothing run, kept or
    /// recorded, when the project is busy, or the ref moved or no longer
    /// keeps a ticket at Watch: the next pass reads it again.
    pub fn correct_seen(
        &self,
        ticket: &TicketId,
        seen: &str,
        following: &mut Following,
        sink: &mut EventSink<'_>,
    ) -> Option<Result<Delivered, Stop>> {
        let mut p = match self.prepare(ticket, Command::Correct) {
            Ok(p) => p,
            Err(Stop::Busy(_)) => return None,
            Err(stop) => return Some(Err(stop)),
        };
        let stored = p.stored.as_ref()?;
        if stored.commit != seen
            || !TicketState::try_from(&stored.record.state)
                .is_ok_and(|state| state.status() == Status::Active(Stage::Watch))
        {
            return None;
        }
        Some(self.correct(&mut p, following, sink))
    }

    /// Corrects the followed pull request of a ticket at Watch, under the
    /// project's lock, after the fetch. The pull request is read again: only
    /// a read that asks for no repair goes on (green, checks running, or a red
    /// the base's), so a conflict or a red is repaired first, and anything
    /// else is told as a look. Then the ticket's stage is read
    /// ([`repairs_in`]), and the decider's replies: none newer than those a
    /// run followed is a look too, and a reply within the quiet window waits
    /// ([`Stop::Settling`]). A correction whose push had no verdict is pushed
    /// again without a run. Otherwise the build role runs at Watch with the
    /// replies in its brief's `correction`; a `done` on a new commit is
    /// pushed and reported, a `done` on Owlshift's last push changed nothing
    /// and is reported so. Either way the decisions keep the replies as
    /// followed, and only once the push has landed.
    pub(super) fn correct(
        &self,
        p: &mut Prepared,
        following: &mut Following,
        sink: &mut EventSink<'_>,
    ) -> Result<Delivered, Stop> {
        let ticket = p.ticket.clone();
        let stored = p
            .stored
            .clone()
            .ok_or_else(|| nothing_to_continue(&ticket))?;
        let delivery = stored
            .record
            .delivery
            .clone()
            .ok_or_else(|| nothing_to_continue(&ticket))?;
        let (Some(pr), Some(pushed)) = (delivery.pull_request.clone(), delivery.pushed.clone())
        else {
            return Err(Stop::Refused(format!(
                "{ticket}'s delivery has no pull request or pushed commit to correct"
            )));
        };
        let state = TicketState::try_from(&stored.record.state).map_err(core_error)?;
        let fresh = follow::read(self.forge, &delivery, pr.number, &pushed, |_| None)
            .map_err(|e| refused("reading the pull request again", e))?;
        let look = || Stop::Watched {
            ticket: ticket.clone(),
            seen: Box::new(fresh.seen(pr.number, &pr.url)),
        };
        if !matches!(
            fresh.decision,
            FollowDecision::Follow {
                adopt: false,
                action: FollowAction::Green | FollowAction::WaitChecks | FollowAction::WaitBaseRed,
            }
        ) {
            return Err(look());
        }
        let comments = self.comments(&ticket)?;
        let current = person(p.current()?, &p.found);
        let questions = p.questions();
        let reversed = reversal::reversals(&comments, &questions, Current::Known(&current));
        let retry = delivery
            .pushing
            .as_ref()
            .filter(|pushing| pushing.kind == PushKind::Correction);
        if reversed.is_empty() && retry.is_none() {
            return Err(look());
        }
        let stage = &self
            .tracker
            .stage(&ticket)
            .map_err(|e| refused(&format!("reading the stage of {ticket}"), e))?;
        if !repairs_in(stage, &self.config.tracker.states) {
            return Err(Stop::Refused(format!(
                "{ticket} shows {stage:?} on the tracker, which is not a state Owlshift works \
                 in: pull request #{} gets no correction while it stays there",
                pr.number
            )));
        }
        let pushed_id =
            CommitId::new(pushed.as_str()).map_err(|e| refused("the pushed commit", e))?;
        let local = self.fix_branch(p, &pushed_id)?;
        // A correction whose push had no verdict is pushed again, without a
        // run, before anything else: its replies are not kept as followed.
        if let Some(pushing) = retry
            && local.as_deref() == Some(pushing.commit.as_str())
        {
            let commit = CommitId::new(pushing.commit.as_str())
                .map_err(|e| refused("the commit under way", e))?;
            let (repaired, followups) = Repaired::again(commit, pushing, "correction");
            return self.push_correction(
                p, &delivery, &pushed_id, repaired, state, followups, &reversed, following, sink,
            );
        }
        if reversed.is_empty() {
            return Err(look());
        }
        let window = self.config.policy.quiet_window();
        if let Some(counts_at) = reversal::counts_at(&reversed, (self.clock)(), window) {
            return Err(Stop::Settling {
                ticket,
                decider: current.name.clone(),
                window,
                counts_at,
            });
        }
        let correction = reversal::correction(&reversed, &questions, pr.number, &pr.url, &pushed);
        let dispatched = self.dispatch(p, Command::Correct, sink)?;
        let build = |_: u32| self.build;
        let decorate = |brief: &mut Brief, questions: &TicketQuestions| {
            brief.correction = Some(correction.clone());
            tell_build(brief, questions);
        };
        // A `done` on Owlshift's last push found nothing to change.
        let accept = |_: &CommitId| Ok(());
        let (done, followups) = self.repair_runs(
            p,
            state,
            &RepairRun {
                role: Role::Build,
                what: "correction",
                pull_request: pr.number,
                rules: &dispatched.rules,
                harness: &build,
                before: &|_, _, _| Ok(()),
                decorate: &decorate,
                accept: &accept,
            },
            sink,
        )?;
        if done.repaired.commit == pushed_id {
            return self
                .already_follows(p, &delivery, &pushed_id, done, followups, &reversed, sink);
        }
        self.push_correction(
            p,
            &delivery,
            &pushed_id,
            done.repaired,
            done.state,
            followups,
            &reversed,
            following,
            sink,
        )
    }

    /// Pushes a correction's commit ([`Self::push_repair`]), then keeps the
    /// replies as followed once the ref says the push landed, whatever the
    /// report did, and records the outcome.
    #[allow(clippy::too_many_arguments)]
    fn push_correction(
        &self,
        p: &mut Prepared,
        delivery: &TicketDelivery,
        pushed: &CommitId,
        repaired: Repaired,
        state: TicketState,
        followups: Vec<Followup>,
        reversed: &[Reversed<'_>],
        following: &mut Following,
        sink: &mut EventSink<'_>,
    ) -> Result<Delivered, Stop> {
        let commit = repaired.commit.to_string();
        let kind = RepairKind::Correction {
            questions: question_texts(&p.questions(), reversed),
        };
        let outcome = self.push_repair(
            p, state, delivery, pushed, repaired, kind, followups, following, sink,
        );
        let landed = p
            .stored
            .as_ref()
            .and_then(|stored| stored.record.delivery.as_ref())
            .is_some_and(|kept| kept.pushed.as_deref() == Some(commit.as_str()));
        if landed {
            self.keep_followed(p, reversed)?;
            self.corrected_event(p, "pushed", reversed, sink);
        }
        outcome
    }

    /// A correction run that changed nothing: `Corrected`, the replies kept
    /// as followed, and a DELIVERY report saying so, since the decider
    /// otherwise sees nothing come of their reply.
    #[allow(clippy::too_many_arguments)]
    fn already_follows(
        &self,
        p: &mut Prepared,
        delivery: &TicketDelivery,
        pushed: &CommitId,
        done: RepairDone,
        followups: Vec<Followup>,
        reversed: &[Reversed<'_>],
        sink: &mut EventSink<'_>,
    ) -> Result<Delivered, Stop> {
        let ticket = p.ticket.clone();
        let state = match done.state.apply(PIPELINE, Event::Corrected) {
            Ok(Transition::To(next)) => next,
            other => return Err(core_error(other)),
        };
        let mut questions = p.questions();
        reversal::followed(&mut questions, reversed);
        self.store(p, &state, questions)
            .map_err(|e| refused("keeping the replies the correction followed", e))?;
        self.corrected_event(p, "unchanged", reversed, sink);
        let head = Branch::new(&delivery.branch).map_err(|e| refused("the ticket's branch", e))?;
        let base = Branch::new(&delivery.base_branch).map_err(|e| refused("the base branch", e))?;
        let pull_request = self
            .forge
            .find_open_pull_request(&head, &base)
            .map_err(|e| e.to_string())
            .and_then(|open| open.ok_or_else(|| "the pull request is no longer open".to_owned()))
            .map_err(|reason| {
                Stop::Delivery(format!(
                    "the correction run found the pull request already following the replies, \
                     but it could not be read for the report: {reason}"
                ))
            })?;
        let checks = self.forge.checks(pull_request.number, pushed);
        let verdict = checks.as_ref().ok().map(CheckSet::verdict);
        let corrected = question_texts(&p.questions(), reversed);
        let report = DeliveryReport {
            ticket: ticket.clone(),
            summary: done.repaired.summary,
            pull_request: pull_request.clone(),
            checks,
            gate: done.repaired.gate,
            decisions: p
                .questions()
                .decisions
                .iter()
                .map(|kept| resolver::reported_reversing(kept, &corrected))
                .collect(),
            followups,
            fix_pass: None,
            rebase: None,
            correction: Some(Corrected {
                questions: corrected.clone(),
                pushed: false,
            }),
        };
        let comment = Writer::new(self.tracker)
            .post_delivery_report(&report)
            .map_err(|e| {
                Stop::Delivery(format!(
                    "the correction run found the pull request already following the replies, \
                     but its report could not be posted: {e}"
                ))
            })?;
        sink.emit(
            &ticket,
            None,
            EventKind::TrackerWrite,
            data([
                ("target", json!("tracker")),
                ("action", json!("comment")),
                ("kind", json!("DELIVERY")),
                ("comment", json!(comment.id)),
                ("corrected", json!(reversed.len())),
            ]),
        );
        Ok(Delivered {
            pull_request,
            opened: false,
            verdict,
            comment: comment.id,
        })
    }

    /// Keeps the replies of `reversed` as followed, with the state the ref
    /// keeps now.
    fn keep_followed(&self, p: &mut Prepared, reversed: &[Reversed<'_>]) -> Result<(), Stop> {
        let mut questions = p.questions();
        if !reversal::followed(&mut questions, reversed) {
            return Ok(());
        }
        let state = p
            .stored
            .as_ref()
            .map(|stored| TicketState::try_from(&stored.record.state))
            .ok_or_else(|| nothing_to_continue(&p.ticket))?
            .map_err(core_error)?;
        self.store(p, &state, questions)
            .map_err(|e| refused("keeping the replies the correction followed", e))
    }

    /// The `decision` event of a correction's outcome: `pushed` or
    /// `unchanged`, the questions of the reversed decisions and the replies.
    fn corrected_event(
        &self,
        p: &Prepared,
        outcome: &str,
        reversed: &[Reversed<'_>],
        sink: &mut EventSink<'_>,
    ) {
        let questions = p.questions();
        let ids: Vec<&str> = reversed
            .iter()
            .filter_map(|r| questions.decisions.get(r.decision))
            .map(|kept| kept.question.id.as_str())
            .collect();
        let replies: Vec<&str> = reversed
            .iter()
            .flat_map(|r| r.replies.iter().map(|c| c.id.as_str()))
            .collect();
        sink.emit(
            &p.ticket,
            None,
            EventKind::Decision,
            data([
                ("watch", json!("correction")),
                ("outcome", json!(outcome)),
                ("questions", json!(ids)),
                ("replies", json!(replies)),
            ]),
        );
    }
}

/// The texts of the questions of the decisions `reversed` names.
fn question_texts(questions: &TicketQuestions, reversed: &[Reversed<'_>]) -> Vec<String> {
    reversed
        .iter()
        .filter_map(|r| questions.decisions.get(r.decision))
        .map(|kept| kept.question.text.clone())
        .collect()
}
