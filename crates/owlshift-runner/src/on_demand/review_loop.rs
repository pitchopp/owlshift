//! The review loop (OWL-307; architecture section 5, "Review comments on
//! the pull request"; build plan, "P5 · Quality pipeline", part J): a
//! review of the decider's on a delivered pull request sends its ticket from
//! Watch back to Build, on the delivered branch, with the reviews due in
//! the Build's brief; then the gate, Verify, a fast-forward push to the same
//! pull request and a DELIVERY report naming the reviews, whose ids are
//! recorded as answered.
//!
//! - *The start.* [`OnDemand::review_seen`] for watch, and `continue` at
//!   Watch, read the pull request again under the lock: only a read that
//!   asks for no repair goes on. The reviews due ([`pr_reviews::due`]) are
//!   judged with `[forge].reviewers` at the base commit and the ticket's
//!   decider resolved now: none of the decider's starts nothing. Otherwise
//!   the ticket loops back to Build, the reviews due kept in `review.json`
//!   (`answering`) in the same write, which marks the loop: each of its
//!   Build runs, Verify's fix runs included, is given them as read then.
//! - *The end.* Verify passed, the commit is pushed as a fix run's is
//!   ([`OnDemand::push_repair`], [`RepairKind::Review`]), leased on
//!   Owlshift's last push; once it lands, the ticket is back at Watch and
//!   the reviews answered. A Build that commits nothing ends the loop with no
//!   Verify pass and no push, its summary in a report saying so
//!   ([`OnDemand::review_unchanged`]).
//! - *On the way.* Its questions are Build's, through the resolver, the
//!   round returning to Build; a `replan` is a plain question there.
//!   `continue` on a loop first reads the pull request
//!   ([`OnDemand::loop_outlived`]): a merge, a close or someone else's push
//!   ends it, and a push of its own a crash cut short is adopted. A Verify
//!   that cannot resume restarts the loop's Build
//!   ([`OnDemand::rebuild_loop`]), since `do` is refused while the pull
//!   request is followed.

use super::*;
use crate::pr_reviews;

/// Whether `p`'s ticket is in a review loop: its review record keeps the
/// reviews the loop answers, and its delivery is still followed.
pub(super) fn in_review_loop(p: &Prepared) -> bool {
    p.review
        .as_ref()
        .is_some_and(|review| !review.answering.is_empty())
        && followed_delivery(p).is_some()
}

/// The delivery `p`'s ref keeps when watch follows it.
fn followed_delivery(p: &Prepared) -> Option<&TicketDelivery> {
    p.stored
        .as_ref()
        .and_then(|stored| stored.record.delivery.as_ref())
        .filter(|delivery| delivery.followed())
}

/// The commit Owlshift last pushed to the pull request of a ticket in a
/// review loop: the commit its Build started from.
pub(super) fn loop_base(p: &Prepared) -> Option<CommitId> {
    if !in_review_loop(p) {
        return None;
    }
    followed_delivery(p)
        .and_then(|delivery| delivery.pushed.as_deref())
        .and_then(|pushed| CommitId::new(pushed).ok())
}

/// The reviews a review loop's Build runs are given: none outside one.
pub(super) fn loop_reviews(p: &Prepared) -> Vec<PullRequestReview> {
    if !in_review_loop(p) {
        return Vec::new();
    }
    p.review
        .as_ref()
        .map(|review| review.answering.clone())
        .unwrap_or_default()
}

impl OnDemand<'_> {
    /// A review loop for `owlshift watch`: run only while the ticket ref,
    /// read under the project's lock, is still `seen`, the commit watch read
    /// it at, and keeps a ticket at Watch. `read` is watch's read of the
    /// pull request's reviews, judged again here. `None`, with nothing run,
    /// kept or recorded, when the project is busy, or the ref moved or no
    /// longer keeps a ticket at Watch.
    pub fn review_seen(
        &self,
        ticket: &TicketId,
        seen: &str,
        read: ReviewsRead,
        sink: &mut EventSink<'_>,
    ) -> Option<Result<Delivered, Stop>> {
        let mut p = match self.prepare_at_watch(ticket, seen, Command::Continue)? {
            Ok(p) => p,
            Err(stop) => return Some(Err(stop)),
        };
        Some(self.review_loop(&mut p, Some(read), sink))
    }

    /// Starts a review loop on the followed pull request of a ticket at
    /// Watch, under the project's lock, after the fetch: the pull request
    /// read again, its reviews (`read`, or read now), the decider's due
    /// among them; then the loop back to Build and its runs to the push.
    /// Anything that starts no loop is told as a look.
    pub(super) fn review_loop(
        &self,
        p: &mut Prepared,
        read: Option<ReviewsRead>,
        sink: &mut EventSink<'_>,
    ) -> Result<Delivered, Stop> {
        let ticket = p.ticket.clone();
        let stored = p
            .stored
            .clone()
            .ok_or_else(|| nothing_to_continue(&ticket))?;
        let delivery = followed_delivery(p)
            .cloned()
            .ok_or_else(|| nothing_to_continue(&ticket))?;
        let (Some(pr), Some(pushed)) = (delivery.pull_request.clone(), delivery.pushed.clone())
        else {
            return Err(nothing_to_continue(&ticket));
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
        ) || state.status() != Status::Active(Stage::Watch)
        {
            return Err(look());
        }
        // A delivery made before OWL-307 is dated now: the reviews already
        // on its pull request were never this delivery's to answer.
        let Some(delivered_at) = delivery.delivered_at else {
            let mut dated = delivery.clone();
            dated.delivered_at = Some((self.clock)());
            let questions = p.questions();
            self.store_delivery(p, &state, questions, Some(dated))
                .map_err(|e| refused("dating the delivery in the ticket's ref", e))?;
            return Err(look());
        };
        let reviewers = project::forge_reviewers_at(&self.executor.git, &p.checkout, &p.base)
            .map_err(Stop::Refused)?;
        let Some(reviewers) = reviewers else {
            return Err(look());
        };
        let read = match read {
            Some(read) => read,
            None => self
                .forge
                .reviews(pr.number)
                .map_err(|e| refused("reading the pull request's reviews", e))?,
        };
        let answered = p
            .review
            .as_ref()
            .map(|review| review.answered_reviews.clone())
            .unwrap_or_default();
        let due = pr_reviews::due(&read.reviews, &reviewers, delivered_at, &answered);
        let decider = p.current()?.account.clone();
        if !pr_reviews::instructed(&due, &decider) {
            return Err(look());
        }
        let stage = self
            .tracker
            .stage(&ticket)
            .map_err(|e| refused(&format!("reading the stage of {ticket}"), e))?;
        if !repairs_in(&stage, &self.config.tracker.states) {
            return Err(Stop::Refused(format!(
                "{ticket} shows {stage:?} on the tracker, which is not a state Owlshift works \
                 in: the reviews on pull request #{} start nothing while it stays there",
                pr.number
            )));
        }
        let given = pr_reviews::given(&due, &decider);
        let pushed_id =
            CommitId::new(pushed.as_str()).map_err(|e| refused("the pushed commit", e))?;
        self.fix_branch(p, &pushed_id)?;
        let building = match state.apply(PIPELINE, Event::LoopBack) {
            Ok(Transition::To(building)) => building,
            other => return Err(core_error(other)),
        };
        sink.emit(
            &ticket,
            None,
            EventKind::Decision,
            loop_event("loop", &given, None),
        );
        let review = p.review.get_or_insert_with(TicketReview::new);
        review.answering = given;
        review.build = None;
        review.last_fix = None;
        self.keep(p, &building)?;
        self.show(&ticket, None, VisibleStage::Working, sink);
        let dispatched = self.dispatch(p, Command::Continue, sink)?;
        self.drive(p, Command::Continue, dispatched, building, sink)
    }

    /// `continue` on a review loop, under its lock, before anything runs:
    /// its pull request read for an end, or for a push of the loop's own a
    /// crash cut short. Either moves the ticket to Watch, as watch's read
    /// would (`follow::changed`); the look is then the command's stop.
    /// `None` when the loop goes on.
    pub(super) fn loop_outlived(
        &self,
        p: &mut Prepared,
        sink: &mut EventSink<'_>,
    ) -> Result<Option<Stop>, Stop> {
        let ticket = p.ticket.clone();
        let Some(stored) = p.stored.clone() else {
            return Ok(None);
        };
        let Some(delivery) = followed_delivery(p).cloned() else {
            return Ok(None);
        };
        let state = TicketState::try_from(&stored.record.state).map_err(core_error)?;
        let mut following = Following::default();
        let now = (self.clock)();
        let (look, warnings) = following.look_ending(self.forge, &ticket, &delivery, now);
        for warning in warnings {
            sink.emit(&ticket, None, EventKind::Warning, warning);
        }
        let seen = match look {
            Look::Read(seen) => seen,
            Look::Paused { until } => {
                return Err(Stop::Refused(format!(
                    "GitHub's rate limit is spent until {until}: run `owlshift continue \
                     {ticket}` again then"
                )));
            }
            Look::Unread { reason, .. } => {
                return Err(refused("reading the pull request", reason));
            }
        };
        let ended = matches!(seen.decision, FollowDecision::End(_));
        let adopted = matches!(seen.decision, FollowDecision::Follow { adopt: true, .. });
        if !ended && !adopted {
            return Ok(None);
        }
        let Some(changed) =
            follow::changed(&stored.record, PIPELINE, &seen, now).map_err(Stop::Refused)?
        else {
            return Ok(None);
        };
        if ended {
            self.review_after_end(&ticket, state.status(), sink)
                .unwrap_or_else(|reason| {
                    self.stage_not_moved(&ticket, None, VisibleStage::Review, &reason, sink);
                });
        }
        let record = changed.record;
        let next = TicketState::try_from(&record.state).map_err(core_error)?;
        p.review = record.review;
        self.store_delivery(p, &next, record.questions, record.delivery)
            .map_err(|e| refused("keeping what the read changed in the ticket's ref", e))?;
        if let Some(replaced) = &changed.replaced {
            self.replaced(&ticket, replaced, delivery.pushed.as_deref(), sink);
        }
        if adopted {
            self.review_after_working(&ticket, sink);
        }
        if let Some(event) = following.events(&ticket, &seen) {
            sink.emit(&ticket, None, EventKind::Decision, event);
        }
        if adopted {
            self.settle_owed_report(p, &mut following, sink)?;
        }
        Ok(Some(Stop::Watched {
            ticket,
            seen: Box::new(seen),
        }))
    }

    /// A review loop whose Verify pass cannot run again (`why`, from
    /// [`Self::verify_resumable`]: a decider comment its Build did not
    /// read, or a branch no longer at its Build's commit): its Build runs
    /// again, the loop restarted from Watch with the same reviews, its kept
    /// Build dropped. `do`, which builds a ticket again elsewhere, is
    /// refused while the pull request is followed.
    pub(super) fn rebuild_loop(
        &self,
        p: &mut Prepared,
        state: TicketState,
        why: Stop,
        sink: &mut EventSink<'_>,
    ) -> Result<TicketState, Stop> {
        let pushed = loop_base(p).ok_or_else(|| nothing_to_continue(&p.ticket))?;
        self.fix_branch(p, &pushed)?;
        let watching =
            TicketState::restore(Status::Active(Stage::Watch), state.round(), 0, 0, 0, 0)
                .map_err(core_error)?;
        let building = match watching.apply(PIPELINE, Event::LoopBack) {
            Ok(Transition::To(building)) => building,
            other => return Err(core_error(other)),
        };
        if let Some(review) = p.review.as_mut() {
            review.build = None;
            review.last_fix = None;
        }
        self.keep(p, &building)?;
        let given = loop_reviews(p);
        sink.emit(
            &p.ticket,
            None,
            EventKind::Decision,
            loop_event("rebuild", &given, Some(&why.to_string())),
        );
        self.show(&p.ticket, None, VisibleStage::Working, sink);
        Ok(building)
    }

    /// Delivers a review loop's Build that Verify passed, as `review.json`
    /// keeps it: its commit pushed to the followed pull request as a fix
    /// run's is ([`Self::push_repair`]), leased on Owlshift's last push, the
    /// reviews answered in the write that keeps it landed. Once the ref is
    /// back at Watch the stage shows review again.
    pub(super) fn deliver_review(
        &self,
        p: &mut Prepared,
        state: &TicketState,
        sink: &mut EventSink<'_>,
    ) -> Result<Delivered, Stop> {
        let ticket = p.ticket.clone();
        let built = p
            .review
            .as_ref()
            .and_then(|review| review.build.clone())
            .ok_or_else(|| {
                Stop::Delivery("the ticket's ref keeps no Build to deliver".to_owned())
            })?;
        let delivery = followed_delivery(p)
            .cloned()
            .ok_or_else(|| nothing_to_continue(&ticket))?;
        let pushed = loop_base(p).ok_or_else(|| nothing_to_continue(&ticket))?;
        let commit = CommitId::new(built.commit.as_str())
            .map_err(|e| Stop::Delivery(format!("the kept Build's commit: {e}")))?;
        let repaired = Repaired {
            commit,
            summary: built.summary,
            gate: Gate::Passed(built.gate),
        };
        let kind = RepairKind::Review {
            reviews: pr_reviews::answered(&loop_reviews(p)),
            notes: built.notes,
        };
        let mut following = Following::default();
        let outcome = self.push_repair(
            p,
            *state,
            &delivery,
            &pushed,
            repaired,
            kind,
            built.followups,
            &mut following,
            sink,
        );
        self.review_shown_after(p, sink);
        outcome
    }

    /// A review loop's Build that committed nothing (architecture section 5:
    /// "a Build that commits nothing ends the loop with its summary in that
    /// report, and is no failure"): no Verify pass and no push; from `state`,
    /// at Verify, the ticket goes back to Watch, the reviews answered and
    /// `questions` kept in the write that owes the report, unchanged, with
    /// the Build's summary; then the report.
    pub(super) fn review_unchanged(
        &self,
        p: &mut Prepared,
        state: TicketState,
        questions: TicketQuestions,
        built: KeptBuild,
        sink: &mut EventSink<'_>,
    ) -> Result<Delivered, Stop> {
        let ticket = p.ticket.clone();
        let delivery = followed_delivery(p)
            .cloned()
            .ok_or_else(|| nothing_to_continue(&ticket))?;
        let pushed = loop_base(p).ok_or_else(|| nothing_to_continue(&ticket))?;
        let mut watching = state;
        while watching.stage() != Stage::Watch {
            watching = match watching.apply(PIPELINE, Event::Completed) {
                Ok(Transition::To(next)) => next,
                other => return Err(core_error(other)),
            };
        }
        let reviews = pr_reviews::answered(&loop_reviews(p));
        let owed = OwedReport {
            kind: PushKind::Review,
            count: 1,
            report: Some(KeptReport {
                summary: built.summary,
                gate: Some(built.gate),
                checks: Vec::new(),
                questions: Vec::new(),
                notes: built.notes,
                followups: built.followups,
                reviews: reviews.clone(),
            }),
            unchanged: true,
        };
        answer_reviews(p);
        let mut following = Following::default();
        let kept = self.owe_unchanged(
            p,
            &delivery,
            &watching,
            questions,
            owed,
            "keeping the reviews the loop answered",
            sink,
        )?;
        sink.emit(
            &ticket,
            None,
            EventKind::Decision,
            data([
                ("watch", json!("review")),
                ("outcome", json!("unchanged")),
                (
                    "reviews",
                    json!(reviews.iter().map(|r| r.id).collect::<Vec<_>>()),
                ),
            ]),
        );
        self.review_shown_after(p, sink);
        self.post_unchanged(
            p,
            kept,
            &pushed,
            &watching,
            "the review loop's Build found nothing to change, and the reviews were kept as \
             answered",
            &mut following,
            sink,
        )
    }

    /// Once a review loop's end is kept, the ref back at Watch, the stage
    /// shows review again while it still shows working.
    fn review_shown_after(&self, p: &Prepared, sink: &mut EventSink<'_>) {
        let at_watch = p
            .stored
            .as_ref()
            .and_then(|stored| TicketState::try_from(&stored.record.state).ok())
            .is_some_and(|state| state.status() == Status::Active(Stage::Watch));
        if at_watch {
            self.review_after_working(&p.ticket, sink);
        }
    }
}

/// The reviews a review loop answers recorded as answered in `p`'s review
/// record, the loop's mark and its kept Build dropped: kept with the write
/// that keeps its push landed or its report owed.
pub(super) fn answer_reviews(p: &mut Prepared) {
    if let Some(review) = p.review.as_mut() {
        for answered in std::mem::take(&mut review.answering) {
            if !review.answered_reviews.contains(&answered.id) {
                review.answered_reviews.push(answered.id);
            }
        }
        review.build = None;
        review.last_fix = None;
    }
}

/// The `decision` event of a review loop's `outcome`: the reviews it
/// answers, those of the decider's apart, and why when it says why.
fn loop_event(outcome: &str, given: &[PullRequestReview], why: Option<&str>) -> Data {
    let ids = |decider: bool| -> Vec<u64> {
        given
            .iter()
            .filter(|review| (review.author.relation == Relation::Decider) == decider)
            .map(|review| review.id)
            .collect()
    };
    let mut event = data([
        ("watch", json!("review")),
        ("outcome", json!(outcome)),
        ("instructions", json!(ids(true))),
        ("context", json!(ids(false))),
    ]);
    if let Some(why) = why {
        event.insert("reason".to_owned(), json!(why));
    }
    event
}
