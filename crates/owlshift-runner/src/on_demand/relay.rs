//! A colleague's review relayed to the ticket's decider (OWL-585; build
//! plan, "P10 · Teams", "A colleague's review"; architecture section 5,
//! "Review comments on the pull request"): at the pass that would start a
//! review loop, a listed reviewer's review that is neither the decider's
//! nor an approval asks the decider instead, the runner's own question, not
//! through the resolver, in one round at Watch that returns to Watch.
//!
//! - *The ask.* [`OnDemand::relay`], from the review loop's start
//!   ([`OnDemand::review_loop`]): one question a review, naming the
//!   reviewer, the pull request and what the review holds, with three
//!   options: apply it as written (the recommendation), set it aside, or say
//!   what to do instead. The reviews are kept in `review.json` (`relayed`),
//!   as they were asked about, in the write that keeps the round.
//! - *The answer.* Read by the answer check, as any round's; once the round
//!   is answered, [`settle`] reads each question's verdict in the RESUME's
//!   write: "apply" and the decider's own words wait for the next pass's
//!   loop, which gives the review as an instruction or as quoted data
//!   beside their answer; "set aside" answers it at once, and it reaches no
//!   later brief.

use super::*;
use crate::pr_reviews::Due;
use owlshift_contracts::review::{RelayAnswer, RelayedReview};

/// The option that applies the review as written, the recommendation.
const APPLY: &str = "A";
/// The option that sets the review aside.
const SET_ASIDE: &str = "B";

impl OnDemand<'_> {
    /// Relays `to_relay`, reviews of colleagues on pull request `pr` of a
    /// ticket at Watch in `state`, to its decider: the round that asks about
    /// them, its reviews kept in `review.json` with it. Returns the stop that
    /// says so, as [`Self::ask`] does.
    pub(super) fn relay(
        &self,
        p: &mut Prepared,
        state: &TicketState,
        to_relay: &[Due<'_>],
        pr: &DeliveredPullRequest,
        sink: &mut EventSink<'_>,
    ) -> Stop {
        let waiting = match state.apply(PIPELINE, Event::Questions) {
            Ok(Transition::To(waiting)) => waiting,
            other => return core_error(other),
        };
        let Some(round) = NonZeroU32::new(waiting.round()) else {
            return Stop::Refused("no question round is open".to_owned());
        };
        let questions = relay_questions(to_relay, pr);
        let review = p.review.get_or_insert_with(TicketReview::new);
        for (due, question) in to_relay.iter().zip(&questions) {
            review
                .relayed
                .retain(|relayed| relayed.review.id != due.review.id);
            review.relayed.push(RelayedReview {
                round,
                question: question.id.clone(),
                answer: None,
                review: due.quoted(Relation::Other),
            });
        }
        let ids: Vec<u64> = to_relay.iter().map(|due| due.review.id).collect();
        sink.emit(
            &p.ticket,
            None,
            EventKind::Decision,
            data([
                ("watch", json!("review")),
                ("outcome", json!("relay")),
                ("reviews", json!(ids)),
            ]),
        );
        let summary = format!(
            "{} on pull request #{}, which Owlshift follows. Only your word directs the change, \
             so Owlshift asks you what to do with {}.",
            reviewed_by(to_relay),
            pr.number,
            if to_relay.len() == 1 { "it" } else { "each" }
        );
        let round_of = Round {
            questions: questions
                .into_iter()
                .map(|question| (question.id.clone(), question))
                .collect(),
            decided: 0,
            fallback: None,
            reopened: Vec::new(),
            labels: BTreeMap::new(),
        };
        self.ask(
            p,
            &waiting,
            result::Status::Questions,
            &summary,
            round_of,
            None,
            sink,
        )
    }
}

/// "bob reviewed", "bob and carol reviewed" or "bob reviewed twice": who
/// wrote the reviews `to_relay`.
fn reviewed_by(to_relay: &[Due<'_>]) -> String {
    let mut logins: Vec<String> = Vec::new();
    for due in to_relay {
        let login = format!("`{}`", login(due));
        if !logins.contains(&login) {
            logins.push(login);
        }
    }
    let who = match logins.split_last() {
        Some((last, [])) => last.clone(),
        Some((last, rest)) => format!("{} and {last}", rest.join(", ")),
        None => "A colleague".to_owned(),
    };
    if to_relay.len() > logins.len() {
        format!("{who} left {} reviews", to_relay.len())
    } else {
        format!("{who} reviewed")
    }
}

/// The login of the review's author, as the forge gave it.
fn login<'a>(due: &Due<'a>) -> &'a str {
    due.review.author.as_deref().unwrap_or("someone")
}

/// What a review holds, as its question says it: its body and its inline
/// comments, as a brief would give them.
fn holds(due: &Due<'_>) -> String {
    let given = due.quoted(Relation::Other);
    let comments = match given.comments.len() {
        0 => None,
        1 => Some("1 inline comment".to_owned()),
        n => Some(format!("{n} inline comments")),
    };
    match (given.body.trim().is_empty(), comments) {
        (false, Some(comments)) => format!("a body and {comments}"),
        (false, None) => "a body and no inline comment".to_owned(),
        (true, Some(comments)) => format!("{comments} and no body"),
        (true, None) => "nothing".to_owned(),
    }
}

/// One question a review of `to_relay`, Q1 to Qn in their order.
fn relay_questions(to_relay: &[Due<'_>], pr: &DeliveredPullRequest) -> Vec<Question> {
    to_relay
        .iter()
        .enumerate()
        .map(|(n, due)| {
            let login = login(due);
            Question {
                id: QuestionId::nth(u32::try_from(n + 1).unwrap_or(u32::MAX)),
                category: "scope".to_owned(),
                context: format!(
                    "`{login}`, a listed reviewer who is not this ticket's decider, submitted \
                     review {} on pull request #{} ({}): it holds {}. A colleague's review \
                     directs nothing until you say so. Answer here, on the ticket; a review of \
                     yours on the pull request is read once this question is answered.",
                    due.review.id,
                    pr.number,
                    pr.url,
                    holds(due)
                ),
                text: format!(
                    "What should Owlshift do with `{login}`'s review of pull request #{}, which \
                     holds {}?",
                    pr.number,
                    holds(due)
                ),
                options: vec![
                    result::QuestionOption {
                        label: "Apply it as written".to_owned(),
                        detail: Some(
                            "Owlshift changes the pull request as the review asks, and answers it."
                                .to_owned(),
                        ),
                    },
                    result::QuestionOption {
                        label: "Set it aside".to_owned(),
                        detail: Some(
                            "Owlshift does not act on it, and never asks about it again."
                                .to_owned(),
                        ),
                    },
                    result::QuestionOption {
                        label: "Say what to do instead".to_owned(),
                        detail: Some(
                            "Write it in your reply: your words direct the change, the review \
                             beside them."
                                .to_owned(),
                        ),
                    },
                ],
                recommendation: Some(result::Recommendation {
                    option: Some(APPLY.to_owned()),
                    why: "A colleague reviewed the change; applying the review answers it on the \
                          pull request."
                        .to_owned(),
                }),
                replan: false,
            }
        })
        .collect()
}

/// What the decider answered about the reviews relayed in `round`
/// (OWL-585), read from the verdicts of every ask of the round, `asks`, the
/// latest on each question: option A applies a review, B sets it aside,
/// which answers it at once, and any other answer, the decider's own words,
/// is what to do instead. Kept in `review`; returns the reviews set aside.
/// An entry of the round with no answered verdict reads as the decider's
/// own words, so that it is never asked about again in a loop.
pub(super) fn settle<'a>(
    review: &mut TicketReview,
    round: NonZeroU32,
    asks: impl IntoIterator<Item = (&'a [Question], &'a [AnswerVerdict])>,
) -> Vec<u64> {
    if !review.relayed.iter().any(|relayed| relayed.round == round) {
        return Vec::new();
    }
    let understood = answer_check::understood(asks);
    let mut set_aside = Vec::new();
    for relayed in review
        .relayed
        .iter_mut()
        .filter(|relayed| relayed.round == round && relayed.answer.is_none())
    {
        let option = understood
            .iter()
            .find(|(question, _)| question.id == relayed.question)
            .and_then(|(_, verdict)| verdict.as_ref())
            .filter(|verdict| verdict.class == AnswerClass::Answered)
            .and_then(|verdict| verdict.option.as_deref());
        relayed.answer = match option {
            Some(APPLY) => Some(RelayAnswer::Apply),
            Some(SET_ASIDE) => {
                set_aside.push(relayed.review.id);
                None
            }
            _ => Some(RelayAnswer::Instead),
        };
    }
    review
        .relayed
        .retain(|relayed| !set_aside.contains(&relayed.review.id));
    for id in &set_aside {
        if !review.answered_reviews.contains(id) {
            review.answered_reviews.push(*id);
        }
    }
    set_aside
}

impl OnDemand<'_> {
    /// [`settle`] on `p`'s review record, once the round `round` of
    /// `questions` is answered, in the write that keeps the answer: the
    /// reviews set aside are told.
    pub(super) fn settle_relayed(
        &self,
        p: &mut Prepared,
        round: NonZeroU32,
        questions: &TicketQuestions,
        sink: &mut EventSink<'_>,
    ) {
        let Some(review) = p.review.as_mut() else {
            return;
        };
        let set_aside = settle(review, round, questions.round_asks(round));
        if !set_aside.is_empty() {
            sink.emit(
                &p.ticket,
                None,
                EventKind::Decision,
                data([
                    ("watch", json!("review")),
                    ("outcome", json!("set_aside")),
                    ("reviews", json!(set_aside)),
                ]),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use owlshift_adapters::forge::Review;
    use owlshift_contracts::result::option_letter;

    use super::*;

    fn review(id: u64) -> Review {
        Review {
            id,
            author: Some("bob".to_owned()),
            state: "COMMENTED".to_owned(),
            submitted_at: Timestamp::from_second(1_791_000_000).unwrap(),
            body: "Rename it.".to_owned(),
            commit: None,
            comments: Vec::new(),
        }
    }

    fn verdict(id: &str, class: AnswerClass, option: Option<&str>) -> AnswerVerdict {
        AnswerVerdict {
            question: QuestionId::new(id).unwrap(),
            class,
            reason: "r".to_owned(),
            reply: None,
            option: option.map(str::to_owned),
        }
    }

    /// Each answer read from the round's latest verdict on its question,
    /// a re-ask's included: A applies, B sets aside, own words instead; a
    /// review of another round is left as it is.
    #[test]
    fn the_answers_of_a_relay_round_are_read_through_its_reasks() {
        let reviews = [review(1), review(2), review(3), review(4)];
        let due: Vec<Due<'_>> = reviews
            .iter()
            .map(|review| Due {
                review,
                account: "u-bob",
            })
            .collect();
        let pr = DeliveredPullRequest {
            number: 12,
            url: "https://github.com/o/r/pull/12".to_owned(),
        };
        let questions = relay_questions(&due[..3], &pr);
        assert_eq!(questions[2].id.as_str(), "Q3");
        assert!(
            questions[0]
                .text
                .contains("pull request #12, which holds a body")
        );
        assert_eq!(
            questions[0]
                .recommended_option()
                .map(option_letter)
                .as_deref(),
            Some("A")
        );
        let round = NonZeroU32::new(2).unwrap();
        let mut record = TicketReview::new();
        for (due, question) in due.iter().zip(&questions) {
            record.relayed.push(RelayedReview {
                round,
                question: question.id.clone(),
                answer: None,
                review: due.quoted(Relation::Other),
            });
        }
        record.relayed.push(RelayedReview {
            round: NonZeroU32::new(1).unwrap(),
            question: QuestionId::nth(1),
            answer: None,
            review: due[3].quoted(Relation::Other),
        });
        let first = [
            verdict("Q1", AnswerClass::Answered, Some("A")),
            verdict("Q2", AnswerClass::Partial, None),
            verdict("Q3", AnswerClass::Answered, None),
        ];
        let reasked = [verdict("Q2", AnswerClass::Answered, Some("B"))];
        let set_aside = settle(
            &mut record,
            round,
            [
                (&questions[..], &first[..]),
                (&questions[1..2], &reasked[..]),
            ],
        );
        assert_eq!(set_aside, [2]);
        assert_eq!(record.answered_reviews, [2]);
        let answers: Vec<(u64, Option<RelayAnswer>)> = record
            .relayed
            .iter()
            .map(|relayed| (relayed.review.id, relayed.answer))
            .collect();
        assert_eq!(
            answers,
            [
                (1, Some(RelayAnswer::Apply)),
                (3, Some(RelayAnswer::Instead)),
                (4, None)
            ]
        );
        TicketReview::parse(&record.render()).unwrap();
        // Read again, nothing changes: the round's answers are kept.
        let again = settle(&mut record, round, [(&questions[..], &first[..])]);
        assert!(again.is_empty());
        assert_eq!(record.answered_reviews, [2]);
    }
}
