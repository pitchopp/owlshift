//! Watch's merge on green (D14, OWL-445; architecture section 5, "Merge on
//! green"): at a pass whose read of a followed pull request is green and
//! that left nothing due, the facts `owlshift_core::follow::merge_due`
//! decides on, read without the lock but the stage and the ticket ref, read
//! again under it; then the last read, `confirm_merge`, the merge under way
//! kept in the ref, the Writer's merge and its answer. A base that moved
//! since the push gets a refresh rebase, a repair step of its own. Every
//! condition the core checks is the core's; this is the reading, the
//! locking, the writing and the telling.

use std::collections::HashMap;
use std::io::Write;

use jiff::Timestamp;
use serde_json::json;

use owlshift_adapters::forge::{self, Branch, CommitId, MergeAnswer, ReviewDecision};
use owlshift_contracts::Role;
use owlshift_contracts::event::EventKind;
use owlshift_contracts::ids::TicketId;
use owlshift_contracts::refs::{DeliveredPullRequest, Merging, TicketDelivery};
use owlshift_contracts::step::StepKind;
use owlshift_core::floor::MergeGrant;
use owlshift_core::follow::{
    self as core, Decision, EndReason, FirstGreen, LastRead, MergeBlocker, MergeDue, MergeFacts,
    MergePolicy, ReviewState,
};
use owlshift_core::state::{Status, TicketState};
use owlshift_core::vocab::{MergeSwitch, Stage};

use super::{Continued, Memory, Seen, Watch};
use crate::events::{EventSink, data};
use crate::follow::{self, MergeRefused, Switch};
use crate::merge;
use crate::on_demand::PIPELINE;
use crate::project::{self, ProjectLock, TicketLock};
use crate::proposals;
use crate::ticket_ref::{self, ProposalsFile, RefAt, Stored, TicketRecord};
use crate::writer::{WriteError, Writer};

/// The base heads merge on green read at this pass, by base branch: one
/// read a pass serves every pull request of the project (OWL-445).
pub(super) type BaseHeads = HashMap<String, Result<String, forge::Error>>;

/// What the merge step holds under the lock: the locks, the ticket ref read
/// again under them, its delivery, and the base head read at this pass.
struct Locked {
    _project: ProjectLock,
    ticket_lock: TicketLock,
    current: Stored,
    delivery: TicketDelivery,
    base_head: String,
}

impl Watch<'_> {
    /// Merge on green for `ticket`, whose ref the pass left at `at`, its
    /// pull request read green in `seen` with nothing due (OWL-445). Never
    /// on a project whose `[forge] merge`, read at the base branch's head
    /// at this pass, is `never`: then nothing more is read.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn merge(
        &self,
        memory: &mut Memory,
        sink: &mut EventSink<'_>,
        out: &mut dyn Write,
        continued: &mut Continued<'_>,
        ticket: &TicketId,
        stored: &Stored,
        at: RefAt,
        seen: &follow::Seen,
    ) {
        let on_demand = self.on_demand;
        let now = (on_demand.clock)();
        let Some(delivery) = &stored.record.delivery else {
            return;
        };
        let (Some(pr), Some(pushed)) = (&delivery.pull_request, &delivery.pushed) else {
            return;
        };
        if memory.following.paused(now) {
            return;
        }
        let base_head = match self.base_head(memory, &delivery.base_branch) {
            Ok(head) => head,
            Err(error) => {
                return self.merge_read_failed(memory, sink, ticket, ("base", &error), now);
            }
        };
        let (switch, method) = match self.switch(memory, &delivery.base_branch, &base_head) {
            Ok(Some(switch)) => switch,
            // The project is busy: the next pass reads it.
            Ok(None) => return,
            Err(reason) => return self.merge_unread(memory, sink, ticket, "switch", reason),
        };
        if switch == MergeSwitch::Never {
            return;
        }
        // A refusal that will not pass holds this head and base back, until
        // a person's `continue` asks again.
        if memory
            .following
            .merge_held(ticket, pushed, &base_head, delivery.merge_retry)
        {
            return;
        }
        let tracker = &on_demand.config.tracker;
        let labels = match on_demand.tracker.ticket(ticket) {
            Ok(found) => found.labels,
            Err(e) => {
                let reason = format!("reading the ticket's labels: {e}");
                return self.merge_unread(memory, sink, ticket, "label", reason);
            }
        };
        // Under the project's lock from here: the ticket ref read again, the
        // stage, the last read and the call (architecture section 5, "The
        // call").
        let Some(locked) = self.merge_lock(memory, sink, ticket, &at.commit, &base_head) else {
            return;
        };
        let current = &locked.current;
        let stage = match on_demand.tracker.stage(ticket) {
            Ok(stage) => stage,
            Err(e) => {
                let reason = format!("reading the ticket's stage: {e}");
                return self.merge_unread(memory, sink, ticket, "stage", reason);
            }
        };
        memory.following.merge_read(ticket);
        let categories = merge::categories(&current.record.questions);
        let gate = on_demand.config.gate_policy();
        let policy = MergePolicy {
            switch,
            method,
            gate: &gate,
        };
        let first = memory.following.first_green(ticket, now);
        let delivery = &locked.delivery;
        // The ref under the lock says which head the merge expects.
        let Some(pushed) = delivery.pushed.as_deref() else {
            return;
        };
        let owes = delivery.owed.is_some()
            || (proposals::has_inbox(on_demand.tracker)
                && proposals::owes(current.proposals.as_ref()));
        let mut facts = MergeFacts {
            decision: &seen.decision,
            pushed,
            first_green: first
                .as_ref()
                .map(|(head, ago)| FirstGreen { head, ago: *ago }),
            base_head: &base_head,
            base_at_push: delivery.base_at_push.as_deref(),
            labeled: labels.iter().any(|label| label == core::MERGE_LABEL),
            excluded: tracker
                .exclude
                .as_ref()
                .is_some_and(|exclude| labels.contains(exclude)),
            categories: &categories,
            at_review: stage == tracker.states.review,
            // Read below when it is the condition that decides: the pass's
            // own read (`FOLLOW_QUERY`, whose text fixtures recorded live
            // pin) does not say it.
            draft: false,
            something_due: owes,
        };
        let read = core_read(seen);
        let mut due = core::merge_due(&policy, &facts, &read);
        if needs_draft(&due) {
            let draft = match on_demand.forge.merge_read(pr.number) {
                Ok(read) => read.draft,
                Err(error) => {
                    return self.merge_read_failed(
                        memory,
                        sink,
                        ticket,
                        ("pull_request", &error),
                        now,
                    );
                }
            };
            facts.draft = draft;
            due = core::merge_due(&policy, &facts, &read);
        }
        let grant = match due {
            MergeDue::Merge(grant) => grant,
            MergeDue::Wait(MergeBlocker::BaseMoved) => {
                drop(locked);
                return self.refresh(memory, sink, out, continued, ticket, at);
            }
            MergeDue::Wait(_) => return,
        };
        // The last read, right before the call (OWL-446).
        let last = match on_demand.forge.merge_read(pr.number) {
            Ok(last) => last,
            Err(error) => {
                return self.merge_read_failed(memory, sink, ticket, ("pull_request", &error), now);
            }
        };
        let Some(at_push) = delivery.base_at_push.as_deref() else {
            return;
        };
        let confirmed = core::confirm_merge(
            grant,
            &delivery.base_branch,
            at_push,
            &LastRead {
                state: core_state(last.state),
                head: last.head.as_str(),
                base: last.base.as_str(),
                base_head: last.base_head.as_str(),
                draft: last.draft,
                review: review_state(last.review),
            },
        );
        let grant = match confirmed {
            MergeDue::Merge(grant) => grant,
            MergeDue::Wait(
                blocker @ (MergeBlocker::ReviewRequired | MergeBlocker::ChangesRequested),
            ) => {
                let reason = merge::blocker_words(&blocker);
                let refused = MergeRefused {
                    head: pushed.to_owned(),
                    base: base_head.clone(),
                    at: now,
                    review: true,
                };
                return self
                    .refused_for_good(memory, sink, out, ticket, pr, refused, None, &reason);
            }
            // The next pass reads what changed: a base that moved gets its
            // refresh rebase then.
            MergeDue::Wait(_) => return,
        };
        memory.following.review_cleared(ticket);
        self.merge_now(memory, sink, out, ticket, pr, locked, grant, seen, now);
    }

    /// The merge granted and confirmed: `merging` kept in the ref, the
    /// Writer's merge, and its answer (architecture section 5, "Its
    /// answers").
    #[allow(clippy::too_many_arguments)]
    fn merge_now(
        &self,
        memory: &mut Memory,
        sink: &mut EventSink<'_>,
        out: &mut dyn Write,
        ticket: &TicketId,
        pr: &DeliveredPullRequest,
        locked: Locked,
        grant: MergeGrant,
        seen: &follow::Seen,
        now: Timestamp,
    ) {
        let on_demand = self.on_demand;
        let merging = Merging {
            commit: grant.head().to_owned(),
            method: grant.method(),
        };
        let mut record = locked.current.record.clone();
        if let Some(delivery) = record.delivery.as_mut() {
            delivery.merging = Some(merging.clone());
        }
        let Some(commit) = self.keep_merge(memory, sink, ticket, &locked, &record) else {
            return;
        };
        let answer =
            Writer::new(on_demand.tracker).merge_pull_request(on_demand.forge, pr.number, &grant);
        let head = grant.head();
        // What the ref keeps once the merge did not happen: no merge under
        // way.
        let unmerged = |record: &TicketRecord| {
            let mut record = record.clone();
            if let Some(delivery) = record.delivery.as_mut() {
                delivery.merging = None;
            }
            record
        };
        match answer {
            Ok(MergeAnswer::Merged { commit: merged }) => {
                self.merged(
                    memory,
                    sink,
                    out,
                    ticket,
                    pr,
                    &locked,
                    (&record, &commit),
                    &merging,
                    merged,
                    seen,
                    now,
                );
            }
            Ok(MergeAnswer::HeadMoved { message }) => {
                let _ =
                    self.keep_merge_at(memory, sink, ticket, &locked, &unmerged(&record), &commit);
                let event = merge::refused_data(pr.number, head, false, Some(409), &message);
                sink.emit(ticket, None, EventKind::Decision, event);
            }
            Ok(MergeAnswer::Refused(refusal)) if refusal.may_pass => {
                let _ =
                    self.keep_merge_at(memory, sink, ticket, &locked, &unmerged(&record), &commit);
                let event = merge::refused_data(
                    pr.number,
                    head,
                    false,
                    Some(refusal.status),
                    &refusal.message,
                );
                sink.emit(ticket, None, EventKind::Decision, event);
            }
            Ok(MergeAnswer::Refused(refusal)) => {
                let _ =
                    self.keep_merge_at(memory, sink, ticket, &locked, &unmerged(&record), &commit);
                let refused = MergeRefused {
                    head: head.to_owned(),
                    base: locked.base_head.clone(),
                    at: now,
                    review: false,
                };
                self.refused_for_good(
                    memory,
                    sink,
                    out,
                    ticket,
                    pr,
                    refused,
                    Some(refusal.status),
                    &refusal.message,
                );
            }
            // It may have merged: `merging` stays, and the next read says.
            Ok(MergeAnswer::NoVerdict { reason }) => {
                sink.emit(
                    ticket,
                    None,
                    EventKind::Warning,
                    data([
                        ("what", json!("merge_unanswered")),
                        ("pull_request", json!(pr.number)),
                        ("head", json!(head)),
                        ("reason", json!(reason)),
                    ]),
                );
            }
            // The call ran nothing: a refused token, a spent budget, or the
            // floor.
            Err(error) => {
                let _ =
                    self.keep_merge_at(memory, sink, ticket, &locked, &unmerged(&record), &commit);
                match error {
                    WriteError::Forge(error) => {
                        self.merge_read_failed(memory, sink, ticket, ("merge", &error), now);
                    }
                    other => {
                        let reason = format!("merging pull request #{}: {other}", pr.number);
                        self.merge_unread(memory, sink, ticket, "merge", reason);
                    }
                }
            }
        }
    }

    /// GitHub merged it: the MERGED comment, then the ref ends the
    /// following, then the `pr_merged` event and the end's own. A comment
    /// that cannot be posted leaves `merging` in the ref, so the next read
    /// of the pull request merged posts it ([`Self::write_merged`]).
    #[allow(clippy::too_many_arguments)]
    fn merged(
        &self,
        memory: &mut Memory,
        sink: &mut EventSink<'_>,
        out: &mut dyn Write,
        ticket: &TicketId,
        pr: &DeliveredPullRequest,
        locked: &Locked,
        (record, commit): (&TicketRecord, &str),
        merging: &Merging,
        merged: CommitId,
        seen: &follow::Seen,
        now: Timestamp,
    ) {
        let on_demand = self.on_demand;
        if let Err(reason) =
            merge::tell_merged(on_demand, ticket, pr, merging, Some(merged.clone()), sink)
        {
            return self.merge_warning(memory, sink, ticket, "merged_unposted", "merged", reason);
        }
        let ended = follow::Seen {
            decision: Decision::End(EndReason::Merged),
            ..seen.clone()
        };
        let stored = Stored {
            record: record.clone(),
            commit: commit.to_owned(),
            proposals: locked.current.proposals.clone(),
        };
        let change = follow::changed(&stored.record, PIPELINE, &ended, now);
        let written = match change {
            Ok(Some(changed)) => {
                self.keep_merge_at(memory, sink, ticket, locked, &changed.record, commit)
            }
            Ok(None) => Some(commit.to_owned()),
            Err(reason) => {
                return self.merge_warning(memory, sink, ticket, "merge_unkept", "end", reason);
            }
        };
        if written.is_none() {
            return;
        }
        merge::merged_event(ticket, pr.number, merging, Some(&merged), sink);
        if let Some(event) = memory.following.events(ticket, &ended) {
            sink.emit(ticket, None, EventKind::Decision, event);
        }
        self.line(
            out,
            &format!(
                "{ticket}: pull request #{} {}: merged by Owlshift ({}) at {}, under merge on green",
                pr.number,
                pr.url,
                merging.method.as_str(),
                merging.commit
            ),
        );
    }

    /// A merge refused for good: one `merge_refused` event and one
    /// notification per head and base (and per review kept), none again
    /// while watch remembers it.
    #[allow(clippy::too_many_arguments)]
    fn refused_for_good(
        &self,
        memory: &mut Memory,
        sink: &mut EventSink<'_>,
        out: &mut dyn Write,
        ticket: &TicketId,
        pr: &DeliveredPullRequest,
        refused: MergeRefused,
        status: Option<u16>,
        reason: &str,
    ) {
        let head = refused.head.clone();
        if !memory.following.merge_refused(ticket, refused) {
            return;
        }
        let event = merge::refused_data(pr.number, &head, true, status, reason);
        sink.emit(ticket, None, EventKind::Decision, event);
        self.line(
            out,
            &format!(
                "{ticket}: merge on green of pull request #{} refused: {}. A person must act; \
                 `owlshift continue {ticket}` asks for another try",
                pr.number,
                reason.trim_end_matches('.')
            ),
        );
        merge::notify_refused(self.on_demand, ticket, pr.number, reason, sink);
    }

    /// A base that moved since the push, the pull request green and the
    /// merge otherwise due: a repair step of its own, the refresh rebase,
    /// held as a repair is.
    fn refresh(
        &self,
        memory: &mut Memory,
        sink: &mut EventSink<'_>,
        out: &mut dyn Write,
        continued: &mut Continued<'_>,
        ticket: &TicketId,
        at: RefAt,
    ) {
        let kind = StepKind::Repair {
            failing_for_ms: None,
            merge_refresh: true,
        };
        self.held_run(
            memory,
            sink,
            out,
            continued,
            ticket,
            Seen::at(at, None),
            (kind, Some(Role::Rebase)),
            None,
        );
    }

    /// The project's lock, then the ticket's, while the ref is still at
    /// `seen` and keeps a ticket at Watch whose delivery is followed: the
    /// ref read again under them. `None` when busy, moved, quarantined or
    /// no longer followed; a ref that cannot be read is warned of.
    fn merge_lock(
        &self,
        memory: &mut Memory,
        sink: &mut EventSink<'_>,
        ticket: &TicketId,
        seen: &str,
        base_head: &str,
    ) -> Option<Locked> {
        let on_demand = self.on_demand;
        let (project, ticket_lock) = match self.locked_at(ticket, seen) {
            Ok(Some(locks)) => locks,
            Ok(None) => return None,
            Err(reason) => {
                self.merge_unread(memory, sink, ticket, "questions", reason);
                return None;
            }
        };
        if !matches!(on_demand.dirs.quarantine(), Ok(None)) {
            return None;
        }
        let git = &on_demand.executor.git;
        let current = match ticket_ref::read(git, &on_demand.dirs.checkout(), ticket) {
            Ok(Some(current)) => current,
            Ok(None) => return None,
            Err(reason) => {
                let reason = format!("reading the ticket's ref: {reason}");
                self.merge_unread(memory, sink, ticket, "questions", reason);
                return None;
            }
        };
        let at_watch = TicketState::try_from(&current.record.state)
            .is_ok_and(|state| state.status() == Status::Active(Stage::Watch));
        let delivery = current
            .record
            .delivery
            .clone()
            .filter(TicketDelivery::followed)?;
        if !at_watch {
            return None;
        }
        Some(Locked {
            _project: project,
            ticket_lock,
            current,
            delivery,
            base_head: base_head.to_owned(),
        })
    }

    /// Writes `record` as the ticket's ref from the one `locked` read,
    /// under its locks; returns where the ref is then, `None` with a
    /// warning when it could not be kept.
    fn keep_merge(
        &self,
        memory: &mut Memory,
        sink: &mut EventSink<'_>,
        ticket: &TicketId,
        locked: &Locked,
        record: &TicketRecord,
    ) -> Option<String> {
        self.keep_merge_at(memory, sink, ticket, locked, record, &locked.current.commit)
    }

    /// [`Self::keep_merge`] from the ref at `from`.
    fn keep_merge_at(
        &self,
        memory: &mut Memory,
        sink: &mut EventSink<'_>,
        ticket: &TicketId,
        locked: &Locked,
        record: &TicketRecord,
        from: &str,
    ) -> Option<String> {
        match self.write_ref(
            &locked.ticket_lock,
            ticket,
            record,
            ProposalsFile::Keep,
            from,
        ) {
            Ok(commit) => Some(commit),
            Err(reason) => {
                let reason = format!("keeping the merge in the ticket's ref: {reason}");
                self.merge_warning(memory, sink, ticket, "merge_unkept", "ref", reason);
                None
            }
        }
    }

    /// A merge on green read that failed: the forge's warnings, as a look's.
    fn merge_read_failed(
        &self,
        memory: &mut Memory,
        sink: &mut EventSink<'_>,
        ticket: &TicketId,
        (fact, error): (&'static str, &forge::Error),
        now: Timestamp,
    ) {
        // A spent budget holds every forge read, told once a pause, as a
        // look's; anything else is this merge's, told once until it reads.
        if error.kind == forge::ErrorKind::RateLimited {
            for warning in memory.following.read_failed(ticket, error, now) {
                sink.emit(ticket, None, EventKind::Warning, warning);
            }
            return;
        }
        self.merge_unread(memory, sink, ticket, fact, error.to_string());
    }

    /// One `merge_facts_unread` warning for `fact`, until every fact reads:
    /// no merge at this pass (architecture section 5, "When").
    fn merge_unread(
        &self,
        memory: &mut Memory,
        sink: &mut EventSink<'_>,
        ticket: &TicketId,
        fact: &'static str,
        reason: String,
    ) {
        self.merge_warning(memory, sink, ticket, "merge_facts_unread", fact, reason);
    }

    /// A warning `what` about `fact`, once until every fact of the merge
    /// reads again.
    fn merge_warning(
        &self,
        memory: &mut Memory,
        sink: &mut EventSink<'_>,
        ticket: &TicketId,
        what: &'static str,
        fact: &'static str,
        reason: String,
    ) {
        if !memory.following.merge_unread(ticket, fact) {
            return;
        }
        sink.emit(
            ticket,
            None,
            EventKind::Warning,
            data([
                ("what", json!(what)),
                ("fact", json!(fact)),
                ("reason", json!(reason)),
            ]),
        );
    }

    /// The head of `branch` on the forge, read once a pass.
    fn base_head(&self, memory: &mut Memory, branch: &str) -> Result<String, forge::Error> {
        if let Some(read) = memory.base_heads.get(branch) {
            return read.clone();
        }
        let read = Branch::new(branch)
            .and_then(|branch| self.on_demand.forge.branch_head(&branch))
            .map(|head| head.to_string());
        memory.base_heads.insert(branch.to_owned(), read.clone());
        read
    }

    /// `[forge] merge` and `merge_method` at `commit` of `branch`, as kept
    /// or read in the dedicated checkout, fetched first under the project's
    /// lock when it does not hold that commit yet. `None` when the project
    /// is busy.
    fn switch(
        &self,
        memory: &mut Memory,
        branch: &str,
        commit: &str,
    ) -> Result<Option<Switch>, String> {
        if let Some(switch) = memory.following.switch_at(commit) {
            return Ok(Some(switch));
        }
        let on_demand = self.on_demand;
        let read = match merge::switch_in_checkout(on_demand, branch, commit)? {
            Some(switch) => switch,
            None => {
                let _lock = match on_demand.dirs.try_lock() {
                    Ok(Some(lock)) => lock,
                    Ok(None) => return Ok(None),
                    Err(e) => return Err(format!("the project's lock: {e}")),
                };
                project::sync_checkout(
                    &on_demand.executor.git,
                    on_demand.dirs,
                    on_demand.remote_url,
                )?;
                merge::switch_in_checkout(on_demand, branch, commit)?.ok_or_else(|| {
                    format!(
                        "the dedicated checkout does not hold {commit} of `{branch}` after a fetch"
                    )
                })?
            }
        };
        memory.following.keep_switch(commit, read);
        Ok(Some(read))
    }

    /// A read of `ticket`'s pull request that ended its following merged
    /// while the ref keeps the runner's merge under way (`merging`): the
    /// runner's merge, told once even after a crash between GitHub's
    /// answer and the ref write. Under the project's and the ticket's lock,
    /// while the ref is still the snapshot's: the MERGED comment, then
    /// `record`, the end, written. `None` with nothing written when busy or
    /// moved, or when the comment could not be posted (warned of), so the
    /// next read tries again.
    pub(super) fn write_merged(
        &self,
        memory: &mut Memory,
        sink: &mut EventSink<'_>,
        ticket: &TicketId,
        stored: &Stored,
        merging: &Merging,
        record: &TicketRecord,
    ) -> Result<Option<RefAt>, String> {
        let Some((_project, ticket_lock)) = self.locked_at(ticket, &stored.commit)? else {
            return Ok(None);
        };
        let delivery = stored.record.delivery.as_ref();
        let Some(pr) = delivery.and_then(|delivery| delivery.pull_request.as_ref()) else {
            return Ok(None);
        };
        if let Err(reason) = merge::tell_merged(self.on_demand, ticket, pr, merging, None, sink) {
            self.merge_warning(memory, sink, ticket, "merged_unposted", "merged", reason);
            return Ok(None);
        }
        let commit = self.write_ref(
            &ticket_lock,
            ticket,
            record,
            ProposalsFile::Keep,
            &stored.commit,
        )?;
        Ok(Some(RefAt {
            commit,
            progress: ticket_ref::progress(record, stored.proposals.as_ref()),
        }))
    }
}

/// Whether the merge's draft condition is the one that decides: every
/// condition before it held (architecture section 5, "When", in its
/// order), so a read says whether the pull request is a draft.
fn needs_draft(due: &MergeDue) -> bool {
    matches!(
        due,
        MergeDue::Merge(_)
            | MergeDue::Wait(
                MergeBlocker::SomethingDue
                    | MergeBlocker::BaseUnknown
                    | MergeBlocker::BaseMoved
                    | MergeBlocker::GreenOnce
            )
    )
}

/// The pass's read as `merge_due` takes it: its state and head. A
/// `Follow` decision is only ever made on an open pull request.
fn core_read(seen: &follow::Seen) -> core::Read {
    let open = matches!(seen.decision, Decision::Follow { .. });
    core::Read {
        state: if open {
            core::PrState::Open
        } else {
            core::PrState::Closed
        },
        head: seen.head.clone(),
        mergeable: core::Mergeable::Mergeable,
        checks: Vec::new(),
    }
}

fn core_state(state: forge::PrState) -> core::PrState {
    match state {
        forge::PrState::Open => core::PrState::Open,
        forge::PrState::Closed => core::PrState::Closed,
        forge::PrState::Merged => core::PrState::Merged,
    }
}

fn review_state(review: ReviewDecision) -> ReviewState {
    match review {
        ReviewDecision::NotRequired => ReviewState::NotRequired,
        ReviewDecision::Approved => ReviewState::Approved,
        ReviewDecision::Required => ReviewState::Required,
        ReviewDecision::ChangesRequested => ReviewState::ChangesRequested,
    }
}
