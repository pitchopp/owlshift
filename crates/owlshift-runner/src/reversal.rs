//! Which kept decisions the decider reversed by replying to their DECISION
//! comments (scenario S7, OWL-146; architecture section 5, "A reversed
//! decision").
//!
//! A reversal is a decider comment threaded under a DECISION comment the
//! ticket ref keeps, whose last edit is newer than the replies a run already
//! followed ([`Reversal::through`]): those the Build that delivered the ticket
//! read, or those a correction run at Watch was given. A top-level comment is
//! never one: it reaches any later Build as the decider's word, but starts no
//! correction. A comment is the decider's by the rule of a brief's thread
//! ([`crate::on_demand::thread`]): its author is the decider of the latest ask
//! before it, or, before any ask, the ticket's decider now.
//!
//! Since OWL-351 a DECISION comment holds the decisions of one resolver run,
//! D1..Dn: a reply reverses those it names, and every decision of the comment
//! when it names none ([`concerns`]).

use std::time::Duration;

use jiff::Timestamp;

use owlshift_adapters::tracker::{Author as TrackerAuthor, Comment, Person};
use owlshift_contracts::Stage;
use owlshift_contracts::brief::{Correction, MAX_REPLY_BYTES, ReversedDecision};
use owlshift_contracts::refs::{KeptDecision, Reversal, TicketQuestions};
use owlshift_contracts::review::TicketPlan;
use owlshift_core::reply::{self, Reply};
use owlshift_core::state::Event;

use crate::on_demand::APPROVAL_CATEGORY;

/// A kept decision with the decider's replies no run followed yet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reversed<'a> {
    /// Its place in the ticket ref's `decisions`.
    pub decision: usize,
    /// The replies, oldest first.
    pub replies: Vec<&'a Comment>,
}

/// Who the decider is for a reply: known, or, when no ask comes before it and
/// the ticket's decider now could not be resolved without the project's zone
/// owners, any tracker account, for a check under the lock to settle.
#[derive(Clone, Copy, Debug)]
pub enum Current<'a> {
    Known(&'a Person),
    Unresolved,
}

/// The kept decisions `comments` reverse, oldest decision first: for each,
/// the decider's replies to its DECISION comment newer than those a run
/// followed.
pub fn reversals<'a>(
    comments: &'a [Comment],
    questions: &TicketQuestions,
    current: Current<'_>,
) -> Vec<Reversed<'a>> {
    questions
        .decisions
        .iter()
        .enumerate()
        .filter_map(|(decision, kept)| {
            let through = kept.reversed.as_ref().map(|r| r.through);
            let replies: Vec<&Comment> = comments
                .iter()
                .filter(|c| c.parent.as_deref() == Some(kept.comment.as_str()))
                .filter(|c| through.is_none_or(|through| c.last_edit() > through))
                .filter(|c| by_decider(c, questions, current))
                .filter(|c| concerns(&c.body, kept, &questions.decisions))
                .collect();
            (!replies.is_empty()).then_some(Reversed { decision, replies })
        })
        .collect()
}

/// Whether a reply of body `body`, threaded under the DECISION comment of
/// `kept`, one of `decisions`, concerns it (OWL-351): the reply names its
/// number among those of the comment's decisions, or names none of them
/// ([`reply::named_decisions`]). A decision kept before OWL-351 is D1, alone
/// in its comment, so any reply concerns it.
pub fn concerns(body: &str, kept: &KeptDecision, decisions: &[KeptDecision]) -> bool {
    let named = reply::named_decisions(body, in_comment(kept, decisions));
    named.is_empty() || named.contains(&(kept.place().get() as usize))
}

/// How many decisions the DECISION comment of `kept`, one of `decisions`,
/// holds: the highest number among them, 1 for a comment of one decision.
pub(crate) fn in_comment(kept: &KeptDecision, decisions: &[KeptDecision]) -> usize {
    decisions
        .iter()
        .filter(|other| other.comment == kept.comment)
        .map(|other| other.place().get() as usize)
        .max()
        .unwrap_or(1)
}

/// Whether `comment` is the decider's: its author's account is the one of
/// the latest ask before it, or, before any ask, `current`'s.
fn by_decider(comment: &Comment, questions: &TicketQuestions, current: Current<'_>) -> bool {
    let TrackerAuthor::Account(author) = &comment.author else {
        return false;
    };
    if author.id.trim().is_empty() {
        return false;
    }
    let asked = questions
        .asks
        .iter()
        .rev()
        .find(|ask| ask.at < comment.created_at);
    match (asked, current) {
        (Some(ask), _) => ask.decider.account == author.id,
        (None, Current::Known(person)) => person.id == author.id,
        (None, Current::Unresolved) => true,
    }
}

/// The newest reply of `reversals`, by last edit: the one the quiet window
/// is counted from.
pub fn newest<'a>(reversals: &[Reversed<'a>]) -> Option<&'a Comment> {
    reversals
        .iter()
        .flat_map(|r| r.replies.iter().copied())
        .max_by_key(|c| c.last_edit())
}

/// When the replies count, as an answer does (architecture section 4, "No
/// premature resume"): `None` once the newest has been left unedited for
/// `window` at `now`, or ends with `go`; otherwise the time it will count.
pub fn counts_at(
    reversals: &[Reversed<'_>],
    now: Timestamp,
    window: Duration,
) -> Option<Timestamp> {
    let newest = newest(reversals)?;
    let edited = newest.last_edit();
    let quiet_for = Duration::try_from(now.duration_since(edited)).unwrap_or_default();
    match reply::counts(quiet_for, reply::ends_with_go(&newest.body), window) {
        Reply::Counts => None,
        Reply::Settling => edited.checked_add(window).ok(),
    }
}

/// Keeps in `questions` that a run followed `reversals`: each decision's
/// reversal moves through its newest reply. Returns whether any moved.
pub fn followed(questions: &mut TicketQuestions, reversals: &[Reversed<'_>]) -> bool {
    let mut moved = false;
    for reversed in reversals {
        let Some(newest) = reversed.replies.iter().max_by_key(|c| c.last_edit()) else {
            continue;
        };
        let Some(kept) = questions.decisions.get_mut(reversed.decision) else {
            continue;
        };
        let through = newest.last_edit();
        if kept.reversed.as_ref().is_none_or(|r| r.through < through) {
            kept.reversed = Some(Reversal {
                through,
                comment: newest.id.clone(),
            });
            moved = true;
        }
    }
    moved
}

/// Whether a reply to `kept` returns the ticket to Design before delivery
/// (OWL-345): the decision kept as the plan's approval, whatever option it
/// took. Any other decision is followed by the next run, as is one the ticket
/// ref kept as a residue decision before OWL-377 (OWL-379).
pub fn reopens(kept: &KeptDecision, plan: Option<&TicketPlan>) -> bool {
    kept.question.category == APPROVAL_CATEGORY
        && plan
            .and_then(|plan| plan.approval.as_ref())
            .is_some_and(|approval| approval.comment == kept.comment)
}

/// The core event that returns a ticket at `stage` to Design for the
/// reversed approval: none at Design itself, where the Design run is the
/// return, and none at a stage the return does not serve (Intake, Watch).
pub fn return_event(stage: Stage) -> Option<Event> {
    matches!(stage, Stage::DesignReview | Stage::Build | Stage::Verify)
        .then_some(Event::ReversedToDesign)
}

/// The brief's correction for `reversals` of the decisions `questions` keeps,
/// on pull request `pull_request` (`url`) at `head`. Each reply is the
/// replies' bodies, oldest first, cut to [`MAX_REPLY_BYTES`].
pub fn correction(
    reversals: &[Reversed<'_>],
    questions: &TicketQuestions,
    pull_request: u64,
    url: &str,
    head: &str,
) -> Correction {
    Correction {
        pull_request,
        url: url.to_owned(),
        head: head.to_owned(),
        reversed: reversals
            .iter()
            .filter_map(|reversed| {
                let kept = questions.decisions.get(reversed.decision)?;
                let mut joined: Vec<String> = Vec::new();
                // A comment of several decisions (OWL-351): the run is told
                // which one this is, as the replies name them.
                let count = in_comment(kept, &questions.decisions);
                if count > 1 {
                    joined.push(format!(
                        "[Owlshift: this decision is D{} of a DECISION comment holding D1 to \
                         D{count}; the replies below are those that name it or name none.]",
                        kept.place()
                    ));
                }
                joined.extend(reversed.replies.iter().map(|c| c.body.trim().to_owned()));
                Some(ReversedDecision {
                    question: kept.question.clone(),
                    decision: kept.decision.clone(),
                    reply: cut(&joined.join("\n\n")),
                })
            })
            .collect(),
    }
}

/// `text` within [`MAX_REPLY_BYTES`], cut on a character boundary and marked.
fn cut(text: &str) -> String {
    const CUT: &str = " [cut]";
    if text.len() <= MAX_REPLY_BYTES {
        return text.to_owned();
    }
    let mut end = MAX_REPLY_BYTES - CUT.len();
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{CUT}", &text[..end])
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;

    use owlshift_contracts::ids::QuestionId;
    use owlshift_contracts::refs::{Ask, AskDecider, AskKind, KeptDecision};
    use owlshift_contracts::result::Question;
    use owlshift_core::decider::DeciderRule;

    use super::*;

    fn at(minute: i64) -> Timestamp {
        Timestamp::from_second(1_790_000_000 + minute * 60).unwrap()
    }

    fn person(id: &str) -> Person {
        Person {
            id: id.into(),
            name: id.into(),
        }
    }

    fn reply(id: &str, by: &str, minute: i64, parent: Option<&str>) -> Comment {
        Comment {
            id: id.into(),
            author: TrackerAuthor::Account(person(by)),
            created_at: at(minute),
            edited_at: None,
            body: format!("{by}: no"),
            parent: parent.map(str::to_owned),
            reactions: Vec::new(),
        }
    }

    fn kept(minute: i64, comment: &str) -> KeptDecision {
        KeptDecision {
            at: at(minute),
            comment: comment.into(),
            question: Question {
                id: QuestionId::new("Q1").unwrap(),
                category: "naming".into(),
                context: "c".into(),
                text: "Which file?".into(),
                options: Vec::new(),
                recommendation: None,
                replan: false,
            },
            decision: "GREETING.md".into(),
            basis: "the ticket".into(),
            basis_kind: None,
            number: None,
            reversed: None,
        }
    }

    fn ids<'a>(reversals: &[Reversed<'a>]) -> Vec<(usize, Vec<&'a str>)> {
        reversals
            .iter()
            .map(|r| {
                (
                    r.decision,
                    r.replies.iter().map(|c| c.id.as_str()).collect(),
                )
            })
            .collect()
    }

    /// Only the decider's replies threaded under a kept DECISION reverse
    /// it: a top-level comment, a reply to another comment and a stranger's
    /// reply do not; once a run followed them, only a newer reply does.
    #[test]
    fn only_the_deciders_newer_replies_to_a_decision_reverse_it() {
        let mut questions = TicketQuestions {
            decisions: vec![kept(1, "d1"), kept(2, "d2")],
            ..TicketQuestions::default()
        };
        let comments = [
            reply("top", "ada", 3, None),
            reply("other", "ada", 4, Some("top")),
            reply("stranger", "bob", 5, Some("d1")),
            reply("r1", "ada", 6, Some("d1")),
            reply("r2", "ada", 7, Some("d2")),
        ];
        let ada = person("ada");
        let found = reversals(&comments, &questions, Current::Known(&ada));
        assert_eq!(ids(&found), [(0, vec!["r1"]), (1, vec!["r2"])]);
        // Unresolved, any account's reply is a candidate.
        let any = reversals(&comments, &questions, Current::Unresolved);
        assert_eq!(ids(&any), [(0, vec!["stranger", "r1"]), (1, vec!["r2"])]);

        assert!(followed(&mut questions, &found[..1]));
        assert!(!followed(&mut questions, &found[..1]));
        assert_eq!(
            questions.decisions[0].reversed,
            Some(Reversal {
                through: at(6),
                comment: "r1".into()
            })
        );
        let mut later = comments.to_vec();
        later.push(reply("r3", "ada", 9, Some("d1")));
        let found = reversals(&later, &questions, Current::Known(&ada));
        assert_eq!(ids(&found), [(0, vec!["r3"]), (1, vec!["r2"])]);
    }

    /// After an ask, a reply is the decider's when it is that ask's decider,
    /// whoever the ticket's decider is now.
    #[test]
    fn a_reply_after_an_ask_is_judged_by_that_asks_decider() {
        let ask = Ask {
            kind: AskKind::Questions,
            round: NonZeroU32::new(1).unwrap(),
            at: at(4),
            comment: "q1".into(),
            questions: Vec::new(),
            decider: AskDecider {
                account: "bob".into(),
                by: DeciderRule::Assignee,
            },
            verdicts: Vec::new(),
            result_refusal: None,
        };
        let questions = TicketQuestions {
            asks: vec![ask],
            decisions: vec![kept(1, "d1")],
            ..TicketQuestions::default()
        };
        let comments = [
            reply("before", "ada", 3, Some("d1")),
            reply("after-ada", "ada", 5, Some("d1")),
            reply("after-bob", "bob", 6, Some("d1")),
        ];
        let ada = person("ada");
        let found = reversals(&comments, &questions, Current::Known(&ada));
        assert_eq!(ids(&found), [(0, vec!["before", "after-bob"])]);
        let correction = correction(&found, &questions, 7, "https://x/7", "h");
        assert_eq!(correction.reversed[0].reply, "ada: no\n\nbob: no");
    }

    /// The fixture plan, approved by the comment `c-approval-1`.
    fn approved_plan() -> TicketPlan {
        TicketPlan::parse(include_str!(
            "../../owlshift-contracts/tests/fixtures/ticket-plan.json"
        ))
        .unwrap()
    }

    /// The decision kept as the plan's approval reopens Design whatever
    /// option it took; any other decision, a residue decision a ref kept
    /// before OWL-377 included, reopens nothing (OWL-379).
    #[test]
    fn a_decision_reopens_design_only_for_the_kept_approval() {
        let plan = approved_plan();
        let approval = plan.approval.as_ref().unwrap().comment.clone();
        let mut approving = kept(1, &approval);
        approving.question.category = "plan_approval".into();
        assert!(reopens(&approving, Some(&plan)));
        let mut other_option = kept(1, &approval);
        other_option.question.category = "plan_approval".into();
        other_option.decision = "Fork 1: another way".into();
        assert!(reopens(&other_option, Some(&plan)));
        // An approval-question decision that is not the kept approval has
        // none to drop.
        let mut superseded = kept(1, "d-old");
        superseded.question.category = "plan_approval".into();
        assert!(!reopens(&superseded, Some(&plan)));
        assert!(!reopens(&approving, None));

        // A residue decision, asked by either review, and a naming decision.
        let mut residue = kept(1, "d1");
        residue.question.category = "review_finding".into();
        assert!(!reopens(&residue, Some(&plan)));
        assert!(!reopens(&residue, None));
        assert!(!reopens(&kept(1, "d1"), Some(&plan)));
        // Not even one on the approval's DECISION comment.
        let mut residue_on_approval = kept(1, &approval);
        residue_on_approval.question.category = "review_finding".into();
        assert!(!reopens(&residue_on_approval, Some(&plan)));
    }

    /// The reversed approval returns the ticket to Design from Design review,
    /// Build and Verify with the one event, and from Design itself, Intake
    /// and Watch with none.
    #[test]
    fn the_approval_reversal_returns_to_design_by_one_event() {
        for stage in [Stage::Design, Stage::Intake, Stage::Watch] {
            assert_eq!(return_event(stage), None, "{stage:?}");
        }
        for stage in [Stage::DesignReview, Stage::Build, Stage::Verify] {
            assert_eq!(
                return_event(stage),
                Some(Event::ReversedToDesign),
                "{stage:?}"
            );
        }
    }

    /// Decision `number` of the DECISION comment `comment` (OWL-351).
    fn numbered(comment: &str, number: u32, id: &str) -> KeptDecision {
        let mut decision = kept(1, comment);
        decision.number = NonZeroU32::new(number);
        decision.question.id = QuestionId::new(id).unwrap();
        decision
    }

    fn says(id: &str, minute: i64, parent: &str, body: &str) -> Comment {
        Comment {
            body: body.to_owned(),
            ..reply(id, "ada", minute, Some(parent))
        }
    }

    /// OWL-351: a reply under a DECISION comment of several decisions
    /// reverses those it names, `Dn` or `Qn`, and every one when it names
    /// none of the comment's numbers; a decision kept alone before OWL-351
    /// takes any reply. The correction run is told which decision each is.
    #[test]
    fn a_reply_reverses_the_decisions_of_its_comment_it_names() {
        let questions = TicketQuestions {
            decisions: vec![
                kept(0, "old"),
                numbered("c", 1, "Q2"),
                numbered("c", 2, "Q4"),
                numbered("c", 3, "Q5"),
            ],
            ..TicketQuestions::default()
        };
        let ada = person("ada");
        let found = |comments: &[Comment]| {
            let found = reversals(comments, &questions, Current::Known(&ada));
            ids(&found)
                .into_iter()
                .map(|(decision, replies)| {
                    (
                        decision,
                        replies.into_iter().map(str::to_owned).collect::<Vec<_>>(),
                    )
                })
                .collect::<Vec<_>>()
        };
        let named = [says("r", 3, "c", "D2: HELLO.md instead")];
        assert_eq!(found(&named), [(2, vec!["r".to_owned()])]);
        let alias = [says("r", 3, "c", "q3 – colder")];
        assert_eq!(found(&alias), [(3, vec!["r".to_owned()])]);
        let both = [says("r", 3, "c", "Not D1, nor **D3**.")];
        assert_eq!(
            found(&both),
            [(1, vec!["r".to_owned()]), (3, vec!["r".to_owned()])]
        );
        for body in ["No, none of this.", "D7 and D0 are wrong"] {
            let all = [says("r", 3, "c", body)];
            assert_eq!(
                found(&all).iter().map(|(d, _)| *d).collect::<Vec<_>>(),
                [1, 2, 3],
                "{body}"
            );
        }
        let old = [says("r", 3, "old", "D2: no")];
        assert_eq!(found(&old), [(0, vec!["r".to_owned()])]);

        let replies = [
            says("r1", 3, "c", "D2: HELLO.md"),
            says("r2", 4, "c", "D3: colder"),
        ];
        let reversed = reversals(&replies, &questions, Current::Known(&ada));
        let correction = correction(&reversed, &questions, 7, "https://x/7", "h");
        assert_eq!(correction.reversed.len(), 2);
        assert_eq!(
            correction.reversed[0].reply,
            "[Owlshift: this decision is D2 of a DECISION comment holding D1 to D3; the \
             replies below are those that name it or name none.]\n\nD2: HELLO.md"
        );
        assert!(correction.reversed[1].reply.ends_with("D3 of a DECISION comment holding D1 to D3; the replies below are those that name it or name none.]\n\nD3: colder"));
    }

    /// OWL-351: the approval's DECISION comment holds the run's other
    /// decisions too; only the one on the approval question reopens Design
    /// and drops the approval.
    #[test]
    fn only_the_approval_question_of_its_comment_is_the_approval() {
        let plan = approved_plan();
        let approval = plan.approval.as_ref().unwrap().comment.clone();
        let mut approving = numbered(&approval, 1, "Q1");
        approving.question.category = APPROVAL_CATEGORY.into();
        let sibling = numbered(&approval, 2, "Q2");
        assert!(reopens(&approving, Some(&plan)));
        assert!(!reopens(&sibling, Some(&plan)));
        let questions = TicketQuestions {
            decisions: vec![approving, sibling],
            ..TicketQuestions::default()
        };
        let replies = [says("r", 5, &approval, "D2: no")];
        let ada = person("ada");
        let reversed = reversals(&replies, &questions, Current::Known(&ada));
        assert_eq!(ids(&reversed), [(1, vec!["r"])]);
    }

    #[test]
    fn a_long_reply_is_cut_on_a_character_boundary() {
        let long = "é".repeat(MAX_REPLY_BYTES);
        let kept = cut(&long);
        assert!(kept.len() <= MAX_REPLY_BYTES && kept.ends_with(" [cut]"));
    }
}
