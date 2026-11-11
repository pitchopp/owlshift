//! Merge on green (D14, OWL-445; architecture section 5, "Merge on green"):
//! what watch's merge step ([`crate::watch`]) and `owlshift continue` at
//! Watch share. The decisions are `owlshift_core::follow`'s, `merge_due` and
//! `confirm_merge`; the merge itself is the Writer's
//! ([`Writer::merge_pull_request`]), behind the floor's grant. Here: the
//! project's switch read at a commit of the base branch, the ticket's
//! questions with who settled each (OWL-465), the MERGED comment, the events
//! and the one notification a final refusal gives.

use std::path::Path;

use serde_json::{Value, json};

use owlshift_adapters::forge::CommitId;
use owlshift_contracts::event::EventKind;
use owlshift_contracts::ids::TicketId;
use owlshift_contracts::refs::{DeliveredPullRequest, Merging, TicketDelivery, TicketQuestions};
use owlshift_contracts::result::Question;
use owlshift_core::follow::{MergeBlocker, MergeQuestion, Settled, held_question};
use owlshift_core::vocab::{AnswerClass, MergeSwitch};

use crate::answer_check;
use crate::events::{Data, EventSink, data};
use crate::executor::Git;
use crate::follow::Switch;
use crate::held;
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
                id: question.id.to_string(),
                round: Some(round.get()),
                category: question.category,
                settled: if verdict.is_some_and(|verdict| verdict.class == AnswerClass::Answered) {
                    Settled::Decider
                } else {
                    Settled::Open
                },
            })
    });
    let decided = questions.decisions.iter().map(|decision| MergeQuestion {
        id: decision.question.id.to_string(),
        round: None,
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
/// and method, and the merge's commit when GitHub's answer gave it; with
/// `bases`, `base_at_push` and the base head the merge was decided on,
/// which differ for a merge on a moved base (OWL-520).
pub(crate) fn merged_event(
    ticket: &TicketId,
    number: u64,
    merging: &Merging,
    commit: Option<&CommitId>,
    bases: Option<(&str, &str)>,
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
    if let Some((at_push, base)) = bases {
        event.insert("base_at_push".to_owned(), json!(at_push));
        event.insert("base".to_owned(), json!(base));
    }
    sink.emit(ticket, None, EventKind::PrMerged, event);
}

/// What merge on green read of a base that moved since the push (OWL-520;
/// architecture section 5, "Merge on green", "The base the checks ran
/// on"): whether its move leaves the delivery's paths alone, so the merge
/// goes on without a refresh rebase.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum MovedBase {
    /// The move touches none of the delivery's paths.
    Disjoint,
    /// The delivery's paths the move touches.
    Shared(Vec<String>),
    /// Git could not tell, and why.
    Unread(String),
    /// GitHub refused a merge of this head on a moved base: it merges only
    /// on the base its checks ran on ("Its answers").
    Refused,
}

/// How many shared paths a `base_moved` event names.
const SHARED_TOLD: usize = 20;

impl MovedBase {
    /// The read of `base_head`'s move since `base_at_push` for the delivery
    /// `pushed`, in the dedicated `checkout` ([`held::shared_with_base`]).
    pub(crate) fn read(
        git: &Git,
        checkout: &Path,
        pushed: &str,
        base_at_push: &str,
        base_head: &str,
    ) -> Self {
        match held::shared_with_base(git, checkout, pushed, Some(base_at_push), base_head) {
            Ok(shared) if shared.is_empty() => Self::Disjoint,
            Ok(shared) => Self::Shared(shared),
            Err(why) => Self::Unread(why),
        }
    }

    /// Whether the merge goes on without a refresh rebase:
    /// `owlshift_core::follow::MergeFacts::disjoint`.
    pub(crate) fn disjoint(&self) -> bool {
        matches!(self, Self::Disjoint)
    }

    /// Its name in events.
    pub(crate) fn reason(&self) -> &'static str {
        match self {
            Self::Disjoint => "disjoint",
            Self::Shared(_) => "shared",
            Self::Unread(_) => "unread",
            Self::Refused => "refused",
        }
    }

    /// The `base_moved` decision event of pull request `number` at `head`,
    /// whose base moved from `base_at_push` to `base`: the reason, whether
    /// a refresh rebase comes first, the shared paths (at most
    /// [`SHARED_TOLD`]) and why git could not tell.
    pub(crate) fn data(&self, number: u64, head: &str, base_at_push: &str, base: &str) -> Data {
        let mut event = data([
            ("watch", json!("base_moved")),
            ("pull_request", json!(number)),
            ("head", json!(head)),
            ("base_at_push", json!(base_at_push)),
            ("base", json!(base)),
            ("reason", json!(self.reason())),
            ("refresh", json!(!self.disjoint())),
        ]);
        match self {
            Self::Shared(paths) => {
                let told: Vec<&String> = paths.iter().take(SHARED_TOLD).collect();
                event.insert("shared".to_owned(), json!(told));
            }
            Self::Unread(why) => {
                event.insert("why".to_owned(), json!(why));
            }
            Self::Disjoint | Self::Refused => {}
        }
        event
    }

    /// Watch's line for pull request `number`'s moved base.
    pub(crate) fn words(&self, number: u64) -> String {
        let moved = format!("pull request #{number}'s base moved since the push");
        let refresh = "a refresh rebase comes first";
        match self {
            Self::Disjoint => format!(
                "{moved}, on paths it does not change: no refresh rebase, it merges once read \
                 green twice"
            ),
            Self::Shared(paths) => {
                let named: Vec<&str> = paths.iter().take(3).map(String::as_str).collect();
                let more = match paths.len().saturating_sub(named.len()) {
                    0 => String::new(),
                    n => format!(" and {n} more"),
                };
                format!(
                    "{moved}, on paths it changes too ({}{more}): {refresh}",
                    named.join(", ")
                )
            }
            Self::Unread(why) => format!(
                "{moved}, and which of its paths the move touches could not be read ({why}): \
                 {refresh}"
            ),
            Self::Refused => {
                format!("{moved}, and GitHub refused to merge it on a moved base: {refresh}")
            }
        }
    }
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

/// Why merge on green will not merge the ticket's pull request, from its
/// `questions` alone, as a delivery says it (OWL-464): the question that
/// keeps it for a person, named, under the project's gate policy. `None`
/// under `never`, where Owlshift merges nothing anyway, and when no question
/// holds.
pub(crate) fn held_at_delivery(
    on_demand: &OnDemand<'_>,
    switch: MergeSwitch,
    questions: &TicketQuestions,
) -> Option<String> {
    if switch == MergeSwitch::Never {
        return None;
    }
    let questions = self::questions(questions);
    let gate = on_demand.config.gate_policy();
    held_question(&gate, &questions).map(|(question, blocker)| hold_words(&blocker, Some(question)))
}

/// Who merges the pull request a command just delivered at `base`, as its
/// line says it: [`switch_at`], and [`held_at_delivery`] on its ticket's
/// `questions` (OWL-464).
pub(crate) fn who_merges(
    on_demand: &OnDemand<'_>,
    base: &Base,
    questions: &TicketQuestions,
) -> (MergeSwitch, Option<String>) {
    let switch = switch_at(on_demand, base);
    (switch, held_at_delivery(on_demand, switch, questions))
}

/// The `merge_waits` decision event (OWL-464): the green pull request
/// `number` at `head` waits for a person, for `blocker`, named by its key,
/// with the question that holds, if one does, and the reason in a person's
/// words.
pub(crate) fn waits_data(
    number: u64,
    head: &str,
    blocker: &MergeBlocker,
    question: Option<&MergeQuestion>,
) -> Data {
    let category = match blocker {
        MergeBlocker::Security(category)
        | MergeBlocker::Unanswered(category)
        | MergeBlocker::ResolverDecided(category) => Some(category.as_str()),
        _ => None,
    };
    data([
        ("watch", json!("merge_waits")),
        ("pull_request", json!(number)),
        ("head", json!(head)),
        ("blocker", json!(blocker.kept_for_a_person())),
        ("category", json!(category)),
        ("question", json!(question.map(|question| &question.id))),
        ("round", json!(question.and_then(|question| question.round))),
        ("reason", json!(hold_words(blocker, question))),
    ])
}

/// Tells the operator that `ticket`'s green pull request `number` waits for
/// a person, Owlshift not merging it for `reason` (runtime and operations,
/// "Notifications", OWL-464), then the ticket's link. A line that cannot be
/// shown is a warning.
pub(crate) fn notify_waits(
    on_demand: &OnDemand<'_>,
    ticket: &TicketId,
    number: u64,
    reason: &str,
    sink: &mut EventSink<'_>,
) {
    let Some(notifier) = on_demand.notifier else {
        return;
    };
    let line = format!(
        "{ticket}: pull request #{number} is green and waits for a person: Owlshift will not \
         merge it, as {reason}"
    );
    let line = notify::with_link(line, on_demand.tracker, ticket);
    if let Err(error) = notifier.notify(&line) {
        sink.emit(ticket, None, EventKind::Warning, notify::not_shown(&error));
    }
}

/// Why the merge does not happen, in a person's words.
pub(crate) fn blocker_words(blocker: &MergeBlocker) -> String {
    hold_words(blocker, None)
}

/// How a person finds `question` on the ticket: its id, and its round when
/// it was asked on one (OWL-464).
fn question_name(question: &MergeQuestion) -> String {
    match question.round {
        Some(round) => format!("question {} of round {round}", question.id),
        None => format!("question {}", question.id),
    }
}

/// [`blocker_words`], naming `question` when it is the one that holds
/// ([`owlshift_core::follow::held_question`], OWL-464).
pub(crate) fn hold_words(blocker: &MergeBlocker, question: Option<&MergeQuestion>) -> String {
    let asked = question.map_or_else(|| "a question of the ticket".to_owned(), question_name);
    match blocker {
        // The resolver's label, not the category it was asked under.
        MergeBlocker::Security(category)
            if question.is_some_and(|question| question.category != *category) =>
        {
            let under = question.map_or("", |question| question.category.as_str());
            format!(
                "{asked}, asked under `{under}`, was labelled `{category}` by the resolver, and \
                 a person merges a security question whatever the answer"
            )
        }
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
            "{asked} was asked under `{category}`, and a person merges a security question \
             whatever the answer"
        ),
        MergeBlocker::Unanswered(category) => format!(
            "{asked} under `{category}`, a category the project keeps for a person, has no \
             answer of its decider on record"
        ),
        MergeBlocker::ResolverDecided(category) => format!(
            "{asked} under `{category}`, a category the project keeps for a person, was decided \
             by the resolver rather than answered by its decider"
        ),
        MergeBlocker::Draft => "the pull request is a draft".to_owned(),
        MergeBlocker::SomethingDue => {
            "the ticket owes its DELIVERY report on the tracker first".to_owned()
        }
        MergeBlocker::BaseUnknown => "the base of Owlshift's last push is not known".to_owned(),
        MergeBlocker::BaseMoved => "its base moved since the push on a path it changes, or in a \
                                    way that could not be read, so a refresh rebase comes first"
            .to_owned(),
        MergeBlocker::GreenOnce => "it was read green only once on this head".to_owned(),
        MergeBlocker::BaseUnchecked => {
            "its base moved since the push, and the base head's own checks are not read yet"
                .to_owned()
        }
        MergeBlocker::BaseRed(checks) => format!(
            "its base moved since the push, and the base head's own checks fail: {}",
            checks.join(", ")
        ),
        MergeBlocker::BasePending(checks) => format!(
            "its base moved since the push, and the base head's own checks still run: {}",
            checks.join(", ")
        ),
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

/// Whether `delivery` owes on the tracker what comes before merge on green
/// merges it or refreshes it, merge on green's `something_due` as far as the
/// ticket ref says, for watch and `continue` alike (architecture section 5,
/// "Merge on green", "Nothing is due first"): its DELIVERY report. Its owed
/// follow-ups do not count since OWL-501 ("An owed follow-up does not
/// hold"): the pass files them first, and one the tracker refuses is filed
/// after, the following's end included.
pub(crate) fn owes_first(delivery: &TicketDelivery) -> bool {
    delivery.owed.is_some()
}

/// The base head's checks a wait names (OWL-521), comma-separated.
fn checks_named(data: &Data) -> String {
    data.get("checks")
        .and_then(Value::as_array)
        .map(|checks| {
            checks
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default()
}

/// The `decision` event of a wait merge on green tells (OWL-499;
/// architecture section 5, "Merge on green", "Waits told"): `watch`
/// `waits`, its `reason`, the pull request and the head, then `fields`.
pub(crate) fn wait_data(number: u64, head: &str, reason: &str, fields: Data) -> Data {
    let mut event = data([
        ("watch", json!("waits")),
        ("reason", json!(reason)),
        ("pull_request", json!(number)),
        ("head", json!(head)),
    ]);
    event.extend(fields);
    event
}

/// The wait merge on green tells for `blocker`, one no person needs to lift
/// and no refusal tells (OWL-499): its reason and fields, `merge_owes_first`
/// the DELIVERY report owed ([`owes_first`], OWL-501). `None` for a hold
/// kept for a person, a review the last read refuses, and the switch off.
pub(crate) fn blocker_wait(blocker: &MergeBlocker) -> Option<(&'static str, Data)> {
    let reason = match blocker {
        MergeBlocker::SomethingDue => {
            return Some(("merge_owes_first", data([("report", json!(true))])));
        }
        MergeBlocker::NotGreen => "merge_not_green",
        MergeBlocker::StageMoved => "merge_stage_moved",
        MergeBlocker::Draft => "merge_draft",
        MergeBlocker::BaseUnknown => "merge_base_unknown",
        MergeBlocker::BaseMoved => "merge_base_moved",
        MergeBlocker::GreenOnce => "merge_green_once",
        // Unchecked after the read: the base moved again meanwhile.
        MergeBlocker::BaseUnchecked => "merge_base_moved",
        MergeBlocker::BaseRed(checks) => {
            return Some(("merge_base_red", data([("checks", json!(checks))])));
        }
        MergeBlocker::BasePending(checks) => {
            return Some(("merge_base_pending", data([("checks", json!(checks))])));
        }
        MergeBlocker::Closed => "merge_closed",
        MergeBlocker::HeadMoved => "merge_head_moved",
        MergeBlocker::BaseRetargeted => "merge_retargeted",
        MergeBlocker::SwitchOff
        | MergeBlocker::Excluded
        | MergeBlocker::Unlabeled
        | MergeBlocker::Security(_)
        | MergeBlocker::Unanswered(_)
        | MergeBlocker::ResolverDecided(_)
        | MergeBlocker::ReviewRequired
        | MergeBlocker::ChangesRequested => return None,
    };
    Some((reason, Data::new()))
}

/// A wait merge on green told, in a person's words (OWL-499), from its
/// event's `reason` and fields, for watch's line, `owlshift why` and
/// `owlshift status --waits`: `None` for a reason it does not tell.
pub(crate) fn wait_words(reason: &str, data: &Data) -> Option<String> {
    let text = |key: &str| data.get(key).and_then(Value::as_str);
    let pr = data
        .get("pull_request")
        .and_then(Value::as_u64)
        .map_or_else(
            || "its pull request".to_owned(),
            |n| format!("pull request #{n}"),
        );
    let merge = |words: &str| format!("{pr} is green and not merged yet: {words}");
    let refresh = |words: &str| {
        format!(
            "{pr} is green on a base that moved since the push, and its refresh rebase waits: \
             {words}"
        )
    };
    let words = match reason {
        "merge_owes_first" => merge(&owes_words(data)),
        "merge_due_first" if text("due") == Some("correction") => merge(
            "a correction a decider's reply asks for comes first, then the merge at the pass \
             after it ends",
        ),
        "merge_due_first" => merge(
            "a review loop the reviews on it ask for comes first, then the merge at the pass \
             after it ends",
        ),
        "merge_busy" => merge(
            "the project or the ticket was busy, or its ticket ref moved, when watch tried; the \
             next pass tries again",
        ),
        "merge_green_once" => merge(
            "it was read green once on this head; a second green read at least a minute later \
             merges it",
        ),
        "merge_not_green" => merge("the head it was read green at is not Owlshift's last push"),
        "merge_stage_moved" => merge(
            "the ticket is no longer in the project's review state on the tracker; moved back \
             there, it is merged",
        ),
        "merge_draft" => merge("it is a draft; marked ready for review, it is merged"),
        "merge_base_unknown" => merge(
            "the base of Owlshift's last push is not known, so whether its checks ran on the \
             base cannot be told: a person merges it",
        ),
        "merge_base_moved" => merge(
            "its base moved right before the merge call; the next pass decides again, a merge or \
             a refresh rebase",
        ),
        "merge_base_red" => merge(&format!(
            "its base moved since the push, and the base head's own checks fail ({}); it merges \
             once they pass",
            checks_named(data)
        )),
        "merge_base_pending" => merge(&format!(
            "its base moved since the push, and the base head's own checks still run ({}); it \
             merges once they pass",
            checks_named(data)
        )),
        "merge_closed" => merge("the last read before the merge found it no longer open"),
        "merge_head_moved" => merge(
            "the last read before the merge found another head than Owlshift's push; the next \
             pass reads it",
        ),
        "merge_retargeted" => merge(
            "it now targets another base branch than the one Owlshift pushed onto: a person \
             merges it, or targets it back",
        ),
        "refresh_held" => refresh(&match text("until") {
            Some(until) => format!(
                "its last step left its ticket ref as it was, so it is tried again at {until}, \
                 or sooner once its ref moves"
            ),
            None => "its last step left its ticket ref as it was and a person must act on the \
                     ticket first"
                .to_owned(),
        }),
        "refresh_usage_limit" => {
            let harness = text("harness").unwrap_or("its harness");
            refresh(&match text("until") {
                Some(until) => format!("{harness} is at its usage limit until {until}"),
                None => format!("{harness} is at its usage limit"),
            })
        }
        "refresh_run_cap" => refresh("every run slot of this machine is taken"),
        "refresh_base_check" => {
            let tickets: Vec<&str> = data
                .get("tickets")
                .and_then(Value::as_array)
                .map(|all| all.iter().filter_map(Value::as_str).collect())
                .unwrap_or_default();
            refresh(&format!(
                "the base check of {} runs alone first",
                tickets.join(", ")
            ))
        }
        "refresh_busy" => refresh(
            "the project or the ticket was busy when watch tried; the next pass tries again",
        ),
        "forge_paused" => format!(
            "GitHub's API budget is spent until {}: watch reads no pull request before then, so \
             {pr} is neither merged nor refreshed",
            text("until").unwrap_or("its reset")
        ),
        _ => return None,
    };
    Some(words)
}

/// What a ticket owes first, in a person's words: its DELIVERY report, and,
/// in an event written before OWL-501, the follow-ups it owed then, with
/// the tracker's last refusal.
fn owes_words(data: &Data) -> String {
    let report = data.get("report").and_then(Value::as_bool) == Some(true);
    let titles: Vec<String> = data
        .get("proposals")
        .and_then(Value::as_array)
        .map(|all| {
            all.iter()
                .filter_map(Value::as_str)
                .map(|title| format!("\"{title}\""))
                .collect()
        })
        .unwrap_or_default();
    let follow_ups = match titles.len() {
        0 => String::new(),
        1 => format!("the follow-up {}", titles[0]),
        n => format!("{n} follow-ups ({})", titles.join(", ")),
    };
    let owed = match (report, follow_ups.is_empty()) {
        (true, true) => "its DELIVERY report".to_owned(),
        (true, false) => format!("its DELIVERY report and {follow_ups}"),
        (false, _) => follow_ups,
    };
    let refusal = data
        .get("refusal")
        .and_then(Value::as_str)
        .map_or_else(String::new, |refusal| {
            format!("; the tracker refused the last filing: {refusal}")
        });
    format!(
        "the ticket owes {owed} on the tracker, which comes first{refusal}. Owlshift tries again \
         at each pass, then gets it a refresh rebase or merges it"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// OWL-499: each wait merge on green tells for a blocker has its words,
    /// and the holds a person lifts and the refusals are told otherwise.
    /// What is owed first is the DELIVERY report alone (OWL-501), and an
    /// event written before, naming follow-ups, is still worded.
    #[test]
    fn every_wait_told_for_a_blocker_has_its_words() {
        let told = [
            MergeBlocker::NotGreen,
            MergeBlocker::StageMoved,
            MergeBlocker::Draft,
            MergeBlocker::SomethingDue,
            MergeBlocker::BaseUnknown,
            MergeBlocker::BaseMoved,
            MergeBlocker::GreenOnce,
            MergeBlocker::BaseUnchecked,
            MergeBlocker::BaseRed(vec!["ci".to_owned()]),
            MergeBlocker::BasePending(vec!["ci".to_owned()]),
            MergeBlocker::Closed,
            MergeBlocker::HeadMoved,
            MergeBlocker::BaseRetargeted,
        ];
        for blocker in told {
            let (reason, fields) = blocker_wait(&blocker).expect("told");
            let event = wait_data(7, "p", reason, fields);
            let words = wait_words(reason, &event).expect("worded");
            assert!(words.starts_with("pull request #7 is green"), "{words}");
        }
        for blocker in [
            MergeBlocker::SwitchOff,
            MergeBlocker::Excluded,
            MergeBlocker::Unlabeled,
            MergeBlocker::Security("security".to_owned()),
            MergeBlocker::Unanswered("money".to_owned()),
            MergeBlocker::ResolverDecided("money".to_owned()),
            MergeBlocker::ReviewRequired,
            MergeBlocker::ChangesRequested,
        ] {
            assert_eq!(blocker_wait(&blocker), None, "{blocker:?}");
        }
        let (reason, fields) = blocker_wait(&MergeBlocker::SomethingDue).unwrap();
        assert_eq!(
            wait_words(reason, &wait_data(7, "p", reason, fields)).unwrap(),
            "pull request #7 is green and not merged yet: the ticket owes its DELIVERY report on \
             the tracker, which comes first. Owlshift tries again at each pass, then gets it a \
             refresh rebase or merges it"
        );
        let before = data([
            ("report", json!(false)),
            ("proposals", json!(["Lint the docs"])),
            ("refusal", json!("tracker: the plan holds 250 issues")),
        ]);
        assert_eq!(
            wait_words(reason, &wait_data(7, "p", reason, before)).unwrap(),
            "pull request #7 is green and not merged yet: the ticket owes the follow-up \"Lint \
             the docs\" on the tracker, which comes first; the tracker refused the last filing: \
             tracker: the plan holds 250 issues. Owlshift tries again at each pass, then gets it \
             a refresh rebase or merges it"
        );
        let (reason, fields) = blocker_wait(&MergeBlocker::BaseRed(vec![
            "ci".to_owned(),
            "lint".to_owned(),
        ]))
        .unwrap();
        assert_eq!(
            wait_words(reason, &wait_data(7, "p", reason, fields)).unwrap(),
            "pull request #7 is green and not merged yet: its base moved since the push, and the \
             base head's own checks fail (ci, lint); it merges once they pass"
        );
        assert_eq!(wait_words("slot", &Data::new()), None);
    }

    /// OWL-520: a moved base is told with its reason, whether a refresh
    /// comes first, at most 20 shared paths and why git could not tell.
    #[test]
    fn a_moved_base_is_told_with_its_reason() {
        let shared: Vec<String> = (0..25).map(|n| format!("app/{n}.rs")).collect();
        let cases = [
            (MovedBase::Disjoint, "disjoint", false),
            (MovedBase::Shared(shared.clone()), "shared", true),
            (
                MovedBase::Unread("git exited 128".to_owned()),
                "unread",
                true,
            ),
            (MovedBase::Refused, "refused", true),
        ];
        for (moved, reason, refresh) in cases {
            let event = moved.data(7, "h", "a", "b");
            assert_eq!(
                (&event["reason"], &event["refresh"], &event["base"]),
                (&json!(reason), &json!(refresh), &json!("b")),
                "{moved:?}"
            );
            assert_eq!(moved.disjoint(), !refresh);
            assert!(
                moved
                    .words(7)
                    .starts_with("pull request #7's base moved since the push")
            );
        }
        let event = MovedBase::Shared(shared).data(7, "h", "a", "b");
        assert_eq!(event["shared"].as_array().unwrap().len(), 20);
        assert_eq!(
            MovedBase::Unread("git exited 128".to_owned()).data(7, "h", "a", "b")["why"],
            json!("git exited 128")
        );
        assert_eq!(
            MovedBase::Shared(vec![
                "a".to_owned(),
                "b".to_owned(),
                "c".to_owned(),
                "d".to_owned()
            ])
            .words(7),
            "pull request #7's base moved since the push, on paths it changes too (a, b, c and 1 \
             more): a refresh rebase comes first"
        );
    }

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
        let settled = |(id, round): (&str, Option<u32>), category: &str, settled| MergeQuestion {
            id: id.to_owned(),
            round,
            category: category.to_owned(),
            labelled: None,
            settled,
        };
        assert_eq!(
            super::questions(&questions),
            [
                settled(("Q1", Some(1)), "irreversible", Settled::Decider),
                MergeQuestion {
                    labelled: Some("security".to_owned()),
                    ..settled(("Q2", Some(1)), "scope", Settled::Decider)
                },
                settled(("Q1", Some(2)), "money", Settled::Open),
                settled(("Q1", Some(3)), "legal", Settled::Decider),
                settled(("Q1", None), "naming", Settled::Resolver),
            ]
        );
        assert!(super::questions(&TicketQuestions::new()).is_empty());
    }

    /// The question that holds is named by its id and round, a resolver's
    /// decision by its id, and one the resolver labelled `security` by the
    /// category it was asked under and the label (OWL-464).
    #[test]
    fn the_hold_names_its_question() {
        let question = |round, category: &str| MergeQuestion {
            id: "Q2".to_owned(),
            round,
            category: category.to_owned(),
            labelled: None,
            settled: Settled::Open,
        };
        let money = MergeBlocker::Unanswered("money".to_owned());
        assert_eq!(
            hold_words(&money, Some(&question(Some(2), "money"))),
            "question Q2 of round 2 under `money`, a category the project keeps for a person, \
             has no answer of its decider on record"
        );
        assert_eq!(
            hold_words(
                &MergeBlocker::ResolverDecided("money".to_owned()),
                Some(&question(None, "money"))
            ),
            "question Q2 under `money`, a category the project keeps for a person, was decided \
             by the resolver rather than answered by its decider"
        );
        assert_eq!(
            hold_words(
                &MergeBlocker::Security("security".to_owned()),
                Some(&question(Some(1), "naming"))
            ),
            "question Q2 of round 1, asked under `naming`, was labelled `security` by the \
             resolver, and a person merges a security question whatever the answer"
        );
    }
}
