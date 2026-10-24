//! `owlshift continue` at Watch on a project that opted into merge on green
//! (D14, OWL-445; architecture section 5, "Merge on green", "`continue` at
//! Watch"): it never merges, since it has no earlier read to count as the
//! first green one. It keeps a person's asking for a merge, `merge_retry`,
//! which lifts a refusal watch keeps for the head, and says what watch's
//! next pass would do, by `owlshift_core::follow::merge_due` on the facts
//! it reads, the earlier green read taken as a minute old.

use owlshift_core::follow::{
    self as core, FirstGreen, GREEN_TWICE_AFTER, MergeDue, MergeFacts, MergePolicy,
};

use super::*;

impl OnDemand<'_> {
    /// What watch's next pass would do with the pull request `seen` read,
    /// on a project whose `[forge] merge`, read at the base this command
    /// fetched, is not `never`; `None` otherwise, which says that Owlshift
    /// never merges. Keeps `merge_retry` in the ref first.
    pub(super) fn merge_note(&self, p: &mut Prepared, seen: &Seen) -> Option<String> {
        let stored = p.stored.clone()?;
        let delivery = stored
            .record
            .delivery
            .clone()
            .filter(TicketDelivery::followed)?;
        let checkout = self.dirs.checkout();
        let (switch, method) =
            project::forge_merge_at(&self.executor.git, &checkout, &p.base).unwrap_or_default();
        if switch == MergeSwitch::Never {
            return None;
        }
        const NEVER: &str = "`continue` never merges";
        let mut asked = delivery.clone();
        asked.merge_retry = Some((self.clock)());
        let kept = TicketState::try_from(&stored.record.state)
            .map_err(|e| format!("{e:?}"))
            .and_then(|state| {
                self.store_delivery(p, &state, stored.record.questions.clone(), Some(asked))
            });
        if let Err(e) = kept {
            return Some(format!(
                "{NEVER}, and asking `owlshift watch` to try a refused merge again could not be \
                 kept in the ticket's ref: {e}."
            ));
        }
        let green = seen.decision
            == (FollowDecision::Follow {
                adopt: false,
                action: FollowAction::Green,
            });
        if !green {
            return Some(format!(
                "{NEVER}: `owlshift watch` merges it once it reads it green twice a minute apart \
                 and nothing is left for a person (merge on green)."
            ));
        }
        let would = self.would_merge(p, seen, &delivery, (switch, method));
        Some(match would {
            Ok(MergeDue::Merge(_)) => format!(
                "{NEVER}: the next pass of `owlshift watch` would merge it, once its read is \
                 green again (merge on green)."
            ),
            Ok(MergeDue::Wait(blocker)) => format!(
                "{NEVER}, and the next pass of `owlshift watch` would not merge it: {}.",
                crate::merge::blocker_words(&blocker)
            ),
            Err(e) => format!(
                "{NEVER}, and whether the next pass of `owlshift watch` would merge it could not \
                 be told: {e}."
            ),
        })
    }

    /// `merge_due` on the facts this command reads, as watch's next pass
    /// would read them, its first green read a minute old.
    fn would_merge(
        &self,
        p: &Prepared,
        seen: &Seen,
        delivery: &TicketDelivery,
        (switch, method): (MergeSwitch, owlshift_core::vocab::MergeMethod),
    ) -> Result<MergeDue, String> {
        let pushed = delivery
            .pushed
            .as_deref()
            .ok_or("its delivery keeps no pushed commit")?;
        let pr = delivery
            .pull_request
            .as_ref()
            .ok_or("its delivery keeps no pull request")?;
        if p.base.branch != delivery.base_branch {
            return Err(format!(
                "its pull request's base `{}` is not the branch this command fetched, `{}`",
                delivery.base_branch, p.base.branch
            ));
        }
        let stage = self
            .tracker
            .stage(&p.ticket)
            .map_err(|e| format!("reading the ticket's stage: {e}"))?;
        let draft = self
            .forge
            .merge_read(pr.number)
            .map_err(|e| format!("reading the pull request: {e}"))?
            .draft;
        let tracker = &self.config.tracker;
        let labels = &p.found.labels;
        let questions = p.questions();
        let categories = crate::merge::categories(&questions);
        let gate = self.config.gate_policy();
        let owes = delivery.owed.is_some()
            || (proposals::has_inbox(self.tracker)
                && proposals::owes(p.stored.as_ref().and_then(|s| s.proposals.as_ref())));
        let facts = MergeFacts {
            decision: &seen.decision,
            pushed,
            first_green: Some(FirstGreen {
                head: pushed,
                ago: GREEN_TWICE_AFTER,
            }),
            base_head: &p.base.commit,
            base_at_push: delivery.base_at_push.as_deref(),
            labeled: labels.iter().any(|label| label == core::MERGE_LABEL),
            excluded: tracker
                .exclude
                .as_ref()
                .is_some_and(|exclude| labels.contains(exclude)),
            categories: &categories,
            at_review: stage == tracker.states.review,
            draft,
            something_due: owes,
        };
        let read = core::Read {
            state: core::PrState::Open,
            head: seen.head.clone(),
            mergeable: core::Mergeable::Mergeable,
            checks: Vec::new(),
        };
        let policy = MergePolicy {
            switch,
            method,
            gate: &gate,
        };
        Ok(core::merge_due(&policy, &facts, &read))
    }
}
