//! Merge on green (D14, OWL-445; architecture section 5, "Merge on green"):
//! what watch's merge step ([`crate::watch`]) and `owlshift continue` at
//! Watch share. The decisions are `owlshift_core::follow`'s, `merge_due` and
//! `confirm_merge`; the merge itself is the Writer's
//! ([`Writer::merge_pull_request`]), behind the floor's grant. Here: the
//! project's switch read at a commit of the base branch, the ticket's
//! questions with who settled each (OWL-465), the MERGED comment, the events
//! and the one notification a final refusal gives.

use serde_json::json;

use owlshift_adapters::forge::CommitId;
use owlshift_contracts::event::EventKind;
use owlshift_contracts::ids::TicketId;
use owlshift_contracts::refs::{DeliveredPullRequest, Merging, TicketDelivery, TicketQuestions};
use owlshift_contracts::result::Question;
use owlshift_core::follow::{MergeBlocker, MergeQuestion, Settled};
use owlshift_core::vocab::{AnswerClass, MergeSwitch};

use crate::answer_check;
use crate::events::{Data, EventSink, data};
use crate::follow::Switch;
use crate::notify;
use crate::on_demand::OnDemand;
use crate::project::{self, Base};
use crate::writer::{MergedComment, Writer};

/// The ticket's questions as merge on green reads them (OWL-465): every
/// round's, in order, under the category it was asked under, settled by its
/// decider when the latest verdict an answer check kept on it is
/// `answered` ([`answer_check::understood`], what a RESUME restates), a
/// thumbs-up's included, with the resolver's label its round's ask kept for
/// it (OWL-482); then every kept decision's, settled by no person.
/// A category may come more than once, settled differently: each entry
/// counts on its own.
pub(crate) fn questions(questions: &TicketQuestions) -> Vec<MergeQuestion> {
    let mut rounds = Vec::new();
    for ask in &questions.asks {
        if !rounds.contains(&ask.round) {
            rounds.push(ask.round);
        }
    }
    let asked = rounds.into_iter().flat_map(|round| {
        let labelled = move |question: &Question| {
            questions
                .asks
                .iter()
                .filter(|ask| ask.round == round)
                .find_map(|ask| ask.labels.get(&question.id).cloned())
        };
        answer_check::understood(questions.round_asks(round))
            .into_iter()
            .map(move |(question, verdict)| MergeQuestion {
                labelled: labelled(&question),
                category: question.category,
                settled: if verdict.is_some_and(|verdict| verdict.class == AnswerClass::Answered) {
                    Settled::Decider
                } else {
                    Settled::Open
                },
            })
    });
    let decided = questions.decisions.iter().map(|decision| MergeQuestion {
        category: decision.question.category.clone(),
        labelled: None,
        settled: Settled::Resolver,
    });
    asked.chain(decided).collect()
}

/// The base `commit` of `branch` as [`project::forge_merge_at`] reads it.
fn base_at(branch: &str, commit: &str) -> Base {
    Base {
        remote_ref: format!("origin/{branch}"),
        branch: branch.to_owned(),
        commit: commit.to_owned(),
    }
}

/// `[forge] merge` and `merge_method` as the project file reads at
/// `commit` of `branch` in the dedicated checkout, never a ticket's
/// branch: `None` when the checkout does not hold that commit, for the
/// caller to fetch. An unreadable project file is an error naming it.
pub(crate) fn switch_in_checkout(
    on_demand: &OnDemand<'_>,
    branch: &str,
    commit: &str,
) -> Result<Option<Switch>, String> {
    let git = &on_demand.executor.git;
    let checkout = on_demand.dirs.checkout();
    let object = format!("{commit}^{{commit}}");
    let held = git
        .output(
            &checkout,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                "--end-of-options",
                &object,
            ],
            None,
        )
        .is_ok_and(|out| out.success());
    if !held {
        return Ok(None);
    }
    project::forge_merge_at(git, &checkout, &base_at(branch, commit)).map(Some)
}

/// The project's `[forge] merge` at `base`, a commit the dedicated checkout
/// fetched, for the texts that say who merges: `never` when it cannot be
/// read, so a text never says Owlshift merges when it may not.
pub(crate) fn switch_at(on_demand: &OnDemand<'_>, base: &Base) -> MergeSwitch {
    let checkout = on_demand.dirs.checkout();
    project::forge_merge_at(&on_demand.executor.git, &checkout, base)
        .map(|(switch, _)| switch)
        .unwrap_or_default()
}

/// [`switch_at`] at the base of `delivery`'s last push, which the
/// dedicated checkout fetched before it.
pub(crate) fn switch_at_push(on_demand: &OnDemand<'_>, delivery: &TicketDelivery) -> MergeSwitch {
    let Some(commit) = &delivery.base_at_push else {
        return MergeSwitch::Never;
    };
    switch_in_checkout(on_demand, &delivery.base_branch, commit)
        .ok()
        .flatten()
        .map_or(MergeSwitch::Never, |(switch, _)| switch)
}

/// Posts the MERGED comment of the runner's merge of `pull_request`,
/// `merging` saying which head and method, once
/// ([`Writer::post_merged`]): with `commit`, the merge's own, when GitHub's
/// answer gave it. A post is a `tracker_write` event.
pub(crate) fn tell_merged(
    on_demand: &OnDemand<'_>,
    ticket: &TicketId,
    pull_request: &DeliveredPullRequest,
    merging: &Merging,
    commit: Option<CommitId>,
    sink: &mut EventSink<'_>,
) -> Result<(), String> {
    let head = CommitId::new(merging.commit.as_str()).map_err(|e| e.to_string())?;
    let comment = MergedComment {
        ticket: ticket.clone(),
        pull_request: pull_request.clone(),
        head,
        method: merging.method,
        commit,
    };
    let reported = Writer::new(on_demand.tracker)
        .post_merged(&comment)
        .map_err(|e| e.to_string())?;
    if reported.new {
        sink.emit(
            ticket,
            None,
            EventKind::TrackerWrite,
            data([
                ("target", json!("tracker")),
                ("action", json!("comment")),
                ("kind", json!("MERGED")),
                ("comment", json!(reported.comment.id)),
            ]),
        );
    }
    Ok(())
}

/// The `pr_merged` event of the runner's merge: which pull request, head
/// and method, and the merge's commit when GitHub's answer gave it.
pub(crate) fn merged_event(
    ticket: &TicketId,
    number: u64,
    merging: &Merging,
    commit: Option<&CommitId>,
    sink: &mut EventSink<'_>,
) {
    let mut event = data([
        ("watch", json!("merged")),
        ("pull_request", json!(number)),
        ("head", json!(merging.commit)),
        ("method", json!(merging.method.as_str())),
    ]);
    if let Some(commit) = commit {
        event.insert("commit".to_owned(), json!(commit.as_str()));
    }
    sink.emit(ticket, None, EventKind::PrMerged, event);
}

/// The `merge_refused` decision event: `final` when it will not pass on
/// that head, with GitHub's status when it answered one.
pub(crate) fn refused_data(
    number: u64,
    head: &str,
    final_: bool,
    status: Option<u16>,
    reason: &str,
) -> Data {
    data([
        ("watch", json!("merge_refused")),
        ("pull_request", json!(number)),
        ("head", json!(head)),
        ("final", json!(final_)),
        ("status", json!(status)),
        ("reason", json!(reason)),
    ])
}

/// Tells the operator that the merge of `ticket`'s pull request `number`
/// was refused for good (runtime and operations, "Notifications"): the
/// ticket, the pull request and GitHub's reason, then the ticket's link. A
/// line that cannot be shown is a warning.
pub(crate) fn notify_refused(
    on_demand: &OnDemand<'_>,
    ticket: &TicketId,
    number: u64,
    reason: &str,
    sink: &mut EventSink<'_>,
) {
    let Some(notifier) = on_demand.notifier else {
        return;
    };
    let line = format!("{ticket}: GitHub refused to merge pull request #{number}: {reason}");
    let line = notify::with_link(line, on_demand.tracker, ticket);
    if let Err(error) = notifier.notify(&line) {
        sink.emit(ticket, None, EventKind::Warning, notify::not_shown(&error));
    }
}

/// Why the merge does not happen, in a person's words.
pub(crate) fn blocker_words(blocker: &MergeBlocker) -> String {
    match blocker {
        MergeBlocker::NotGreen => "it is not green on Owlshift's last push".to_owned(),
        MergeBlocker::SwitchOff => "the project's `[forge] merge` is `never`".to_owned(),
        MergeBlocker::Excluded => "the ticket carries the project's exclusion label".to_owned(),
        MergeBlocker::Unlabeled => format!(
            "the ticket does not carry the label `{}`",
            owlshift_core::follow::MERGE_LABEL
        ),
        MergeBlocker::StageMoved => {
            "the ticket is no longer in the project's review state on the tracker".to_owned()
        }
        MergeBlocker::Security(category) => format!(
            "a question of the ticket was asked under `{category}`, and a person merges a \
             security question whatever the answer"
        ),
        MergeBlocker::Unanswered(category) => format!(
            "a question of the ticket under `{category}`, a category the project keeps for a \
             person, has no answer of its decider on record"
        ),
        MergeBlocker::ResolverDecided(category) => format!(
            "a question of the ticket under `{category}`, a category the project keeps for a \
             person, was decided by the resolver rather than answered by its decider"
        ),
        MergeBlocker::Draft => "the pull request is a draft".to_owned(),
        MergeBlocker::SomethingDue => {
            "the ticket owes a report or a follow-up on the tracker first".to_owned()
        }
        MergeBlocker::BaseUnknown => "the base of Owlshift's last push is not known".to_owned(),
        MergeBlocker::BaseMoved => {
            "its base moved since the push, so a refresh rebase comes first".to_owned()
        }
        MergeBlocker::GreenOnce => "it was read green only once on this head".to_owned(),
        MergeBlocker::Closed => "it is not open".to_owned(),
        MergeBlocker::HeadMoved => "its head is no longer Owlshift's last push".to_owned(),
        MergeBlocker::BaseRetargeted => {
            "it targets another base branch than the one Owlshift pushed onto".to_owned()
        }
        MergeBlocker::ReviewRequired => {
            "GitHub's review decision for it is a review required".to_owned()
        }
        MergeBlocker::ChangesRequested => "a review on it requests changes".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each round's questions with their latest verdict, a re-ask's and a
    /// thumbs-up's included and a later round's never borrowing an earlier
    /// one's, nor its resolver labels (OWL-482), then the resolver's
    /// decisions.
    #[test]
    fn the_questions_say_who_settled_each() {
        let mut questions = TicketQuestions::parse(
            r#"{"format":18,"asks":[{"kind":"questions","round":1,"at":"2026-10-24T09:00:00Z",
            "comment":"c1","decider":{"account":"ada","by":"assignee"},"questions":[
            {"id":"Q1","category":"irreversible","context":"c","text":"t","options":[]},
            {"id":"Q2","category":"scope","context":"c","text":"t","options":[]}],
            "verdicts":[{"question":"Q1","class":"answered","reason":"r"},
            {"question":"Q2","class":"partial","reason":"r"}],"labels":{"Q2":"security"}},
            {"kind":"reask","round":1,"at":"2026-10-24T10:00:00Z","comment":"c2",
            "decider":{"account":"ada","by":"assignee"},"questions":[
            {"id":"Q2","category":"scope","context":"c","text":"t","options":[]}],
            "verdicts":[{"question":"Q2","class":"answered","reason":"r"}]},
            {"kind":"questions","round":2,"at":"2026-10-24T11:00:00Z","comment":"c3",
            "decider":{"account":"ada","by":"assignee"},"questions":[
            {"id":"Q1","category":"money","context":"c","text":"t","options":[]}]},
            {"kind":"questions","round":3,"at":"2026-10-24T12:00:00Z","comment":"c4",
            "decider":{"account":"ada","by":"assignee"},"questions":[
            {"id":"Q1","category":"legal","context":"c","text":"t","options":[]}]}],
            "decisions":[{"at":"2026-10-24T09:00:00Z","comment":"c5","question":
            {"id":"Q1","category":"naming","context":"c","text":"t","options":[]},
            "decision":"d","basis":"b"}]}"#,
        )
        .unwrap();
        let legal = &mut questions.asks[3];
        legal.verdicts = answer_check::thumbs_up_verdicts(&legal.questions);
        let settled = |category: &str, settled| MergeQuestion {
            category: category.to_owned(),
            labelled: None,
            settled,
        };
        assert_eq!(
            super::questions(&questions),
            [
                settled("irreversible", Settled::Decider),
                MergeQuestion {
                    labelled: Some("security".to_owned()),
                    ..settled("scope", Settled::Decider)
                },
                settled("money", Settled::Open),
                settled("legal", Settled::Decider),
                settled("naming", Settled::Resolver),
            ]
        );
        assert!(super::questions(&TicketQuestions::new()).is_empty());
    }
}
