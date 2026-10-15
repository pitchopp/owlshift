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

use std::time::Duration;

use jiff::Timestamp;

use owlshift_adapters::tracker::{Author as TrackerAuthor, Comment, Person};
use owlshift_contracts::brief::{Correction, MAX_REPLY_BYTES, ReversedDecision};
use owlshift_contracts::refs::{Reversal, TicketQuestions};
use owlshift_core::reply::{self, Reply};

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
                .collect();
            (!replies.is_empty()).then_some(Reversed { decision, replies })
        })
        .collect()
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
                let joined: Vec<&str> = reversed.replies.iter().map(|c| c.body.trim()).collect();
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

    #[test]
    fn a_long_reply_is_cut_on_a_character_boundary() {
        let long = "é".repeat(MAX_REPLY_BYTES);
        let kept = cut(&long);
        assert!(kept.len() <= MAX_REPLY_BYTES && kept.ends_with(" [cut]"));
    }
}
