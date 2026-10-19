//! The answer check (P2): what the runner does with a run of the
//! `answer_check` role (`roles/answer_check.md`), which classifies each
//! question of a ticket's latest ask once its decider has replied
//! (architecture section 4, scenario S2).
//!
//! It runs once answers arrive ([`new_answer`]) and the decider's reply
//! counts ([`counts_at`]: the quiet window, or `go`). Its verdicts fold into one
//! core event ([`event`]): the ticket resumes, with a RESUME comment of what
//! was understood ([`understood`], [`crate::writer::ResumeComment`]), the
//! questions left open are asked again ([`open_questions`], posted as a RE-ASK comment,
//! [`crate::writer::ReaskComment`]), or the decider's counter-question gets
//! the check's reply ([`counter_replies`], posted as a REPLY comment,
//! [`crate::writer::ReplyComment`]). Before delivery, the decider's comments
//! a Build run did not see are its late comments ([`late_comments`]).
//! `owlshift continue` runs it ([`crate::on_demand`]); the test
//! bench's stand-in driver plays the same pieces in the scenarios.
//!
//! A thumbs-up (OWL-371) answers a question asked alone without the check:
//! the decider's `+1` on the latest ask's comment ([`thumbs_up`]) is their
//! answer when they typed none ([`answer`]), it waits through the quiet
//! window as a typed answer does, and once it counts the question is
//! answered on its recommendation ([`thumbs_up_verdicts`]).

use std::time::Duration;

use jiff::Timestamp;
use owlshift_adapters::tracker::{Comment, Person, Reaction};
use owlshift_contracts::brief::{Brief, Relation};
use owlshift_contracts::refs::TicketQuestions;
use owlshift_contracts::result::{self, AnswerClass, Question, RunResult, Verdict, option_letter};
use owlshift_core::reply::{self, Reply};
use owlshift_core::state::Event;

use crate::executor::Outcome;
use crate::on_demand::{account_author, comment_author, core_event, freed_refusal, result_refusal};

/// Whether answers arrived: the time of the decider's new answer
/// ([`answer`]), `None` otherwise. Only the tracker's times are compared, at
/// their own precision, never this machine's clock: they order the comments
/// of one second (OWL-136), so an answer posted in its ask's second is
/// newer. A comment is the decider's as the brief marks it: their account's,
/// and not a marked comment.
pub fn new_answer(
    comments: &[Comment],
    decider: &Person,
    asked: &TicketQuestions,
) -> Option<Timestamp> {
    answer(comments, decider, asked).map(|answer| answer.at())
}

/// The decider's new answer to the latest ask.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Answer<'c> {
    /// Their comments, the newest last edited at this time.
    Typed(Timestamp),
    /// Their thumbs-up on the ask's comment (OWL-371).
    ThumbsUp(&'c Reaction),
}

impl Answer<'_> {
    /// When it was given: what the check that reads it reads through.
    pub fn at(&self) -> Timestamp {
        match self {
            Self::Typed(at) => *at,
            Self::ThumbsUp(reaction) => reaction.created_at,
        }
    }
}

/// The decider's new answer to the latest ask: their comments when the
/// newest last edit among them is later than the latest ask and than what
/// the last answer check to give its verdicts read
/// ([`TicketQuestions::answers_after`]); else, when it came after that,
/// their thumbs-up on the ask's comment ([`thumbs_up`], OWL-371). Words win:
/// a thumbs-up newer than them is not read with them, and stays an answer
/// of its own once they are judged. `None` when neither is new.
pub fn answer<'c>(
    comments: &'c [Comment],
    decider: &Person,
    asked: &TicketQuestions,
) -> Option<Answer<'c>> {
    let after = asked.answers_after()?;
    if let Some(typed) = newest_decider_edit(comments, decider).filter(|newest| *newest > after) {
        return Some(Answer::Typed(typed));
    }
    thumbs_up(comments, decider, asked)
        .map(|(_, reaction)| reaction)
        .filter(|reaction| reaction.created_at > after)
        .map(Answer::ThumbsUp)
}

/// Whether a thumbs-up can answer an ask of `questions` (OWL-371): one
/// question, carrying a recommendation, which the thumbs-up takes as `ok`
/// does. An ask of several questions needs words, since a thumbs-up meaning
/// "seen" would take every recommendation at once.
pub fn answerable_by_thumbs_up(questions: &[Question]) -> bool {
    matches!(questions, [question] if question.recommendation.is_some())
}

/// The decider's thumbs-up on the latest ask (OWL-371), with the ask's
/// comment it is on: a [`Reaction::THUMBS_UP`] put after the ask on the
/// comment the ask keeps (its round's QUESTIONS comment, the last part when
/// in parts, or the RE-ASK), by `decider`'s account, matched by identifier
/// as their comments are, an app user's never; the newest when there are
/// several. `None` when the ask cannot be answered so
/// ([`answerable_by_thumbs_up`]) or its comment is not among `comments`.
pub fn thumbs_up<'c>(
    comments: &'c [Comment],
    decider: &Person,
    asked: &TicketQuestions,
) -> Option<(&'c Comment, &'c Reaction)> {
    let ask = asked.latest()?;
    if !answerable_by_thumbs_up(&ask.questions) {
        return None;
    }
    let comment = comments.iter().find(|comment| comment.id == ask.comment)?;
    let reaction = comment
        .reactions
        .iter()
        .filter(|reaction| {
            reaction.is_thumbs_up()
                && reaction.created_at > ask.at
                && account_author(&reaction.author, decider).relation == Relation::Decider
        })
        .max_by_key(|reaction| reaction.created_at)?;
    Some((comment, reaction))
}

/// The verdicts a thumbs-up gives (OWL-371): each question of the ask
/// `answered` on its recommendation, as `ok` takes it, the reason saying so
/// in the RESUME and in the round's `answered` entry of later briefs.
pub fn thumbs_up_verdicts(questions: &[Question]) -> Vec<Verdict> {
    questions
        .iter()
        .map(|question| Verdict {
            question: question.id.clone(),
            class: AnswerClass::Answered,
            reason: thumbs_up_reading(question),
            reply: None,
        })
        .collect()
}

/// What a thumbs-up on `question` was understood to answer.
fn thumbs_up_reading(question: &Question) -> String {
    const LEAD: &str = "The decider's thumbs-up on the question: its recommendation";
    match (question.recommended_option(), &question.recommendation) {
        (Some(index), _) => format!(
            "{LEAD}, {}, {}.",
            option_letter(index),
            question.options[index].label.trim_end_matches('.')
        ),
        (None, Some(recommendation)) => format!("{LEAD}: {}", recommendation.why),
        (None, None) => format!("{LEAD}."),
    }
}

/// The identifier of the comment that holds the decider's answer to the
/// latest ask, which a plan's approval keeps (OWL-32): the ask's own
/// comment when their thumbs-up on it resumed the ticket (OWL-371), which
/// the verdicts kept on the ask tell ([`resumed_by_thumbs_up`]), so a
/// reaction removed since, or a design run retried, changes nothing; else
/// their newest comment, as before.
pub fn answering_comment(
    comments: &[Comment],
    decider: &Person,
    asked: &TicketQuestions,
) -> Option<String> {
    if let Some(ask) = asked.latest()
        && resumed_by_thumbs_up(&ask.verdicts, &ask.questions)
    {
        return Some(ask.comment.clone());
    }
    newest_decider_comment(comments, decider).map(|comment| comment.id.clone())
}

/// Whether `verdicts`, kept on an ask of `questions`, are those a thumbs-up
/// gave ([`thumbs_up_verdicts`]): what the ticket ref keeps of a resume by
/// thumbs-up, the reaction itself being read live only.
pub fn resumed_by_thumbs_up(verdicts: &[Verdict], questions: &[Question]) -> bool {
    !verdicts.is_empty() && verdicts == thumbs_up_verdicts(questions).as_slice()
}

/// The newest last edit among the decider's comments, if they wrote any.
pub fn newest_decider_edit(comments: &[Comment], decider: &Person) -> Option<Timestamp> {
    newest_decider_comment(comments, decider).map(Comment::last_edit)
}

/// The decider's comment with the newest last edit, if they wrote any. Of
/// comments edited last at the same time, the one created last, then the
/// last the tracker lists.
pub fn newest_decider_comment<'c>(
    comments: &'c [Comment],
    decider: &Person,
) -> Option<&'c Comment> {
    decider_comments(comments, decider)
        .max_by_key(|comment| (comment.last_edit(), comment.created_at))
}

/// The decider's comments a Build run did not see (OWL-139): those of `now`
/// whose last edit is newer than the newest decider edit among `read`, the
/// comments its brief was built from; all of them when `read` held none. A
/// comment of the same time as that edit is missed, as by [`new_answer`].
pub fn late_comments<'c>(
    read: &[Comment],
    now: &'c [Comment],
    decider: &Person,
) -> Vec<&'c Comment> {
    edited_after(newest_decider_edit(read, decider), now, decider)
}

/// The decider's comments of `now` last edited after `seen`, the newest
/// decider edit a run's brief held, all of them when it held none: those
/// the run did not see ([`late_comments`]), read again from a kept mark
/// when the run's own comments are gone (OWL-304).
pub fn edited_after<'c>(
    seen: Option<Timestamp>,
    now: &'c [Comment],
    decider: &Person,
) -> Vec<&'c Comment> {
    decider_comments(now, decider)
        .filter(|comment| seen.is_none_or(|seen| comment.last_edit() > seen))
        .collect()
}

/// The decider's own comments, as the brief marks them: their account's,
/// and not a marked comment.
fn decider_comments<'c>(
    comments: &'c [Comment],
    decider: &Person,
) -> impl Iterator<Item = &'c Comment> {
    comments
        .iter()
        .filter(move |comment| comment_author(comment, decider).relation == Relation::Decider)
}

/// When the decider's reply counts, if it does not yet at `now`: the quiet
/// window ([`owlshift_core::reply`], `window`, the project's policy) after the last edit of their newest
/// comment, unless that comment ends with `go`. `None` when it counts, or
/// when the decider wrote nothing. `now` is this machine's clock, the one
/// time here that is not the tracker's; a last edit after it counts as left
/// unedited for no time at all, so the whole window applies from that edit.
pub fn counts_at(
    comments: &[Comment],
    decider: &Person,
    now: Timestamp,
    window: Duration,
) -> Option<Timestamp> {
    let newest = newest_decider_comment(comments, decider)?;
    settles_until(
        newest.last_edit(),
        reply::ends_with_go(&newest.body),
        now,
        window,
    )
}

/// When an answer given at `at` counts, if it does not yet at `now`: the
/// quiet `window` after `at`, unless it ended with `go`. A time after `now`
/// counts as left for no time at all.
fn settles_until(at: Timestamp, go: bool, now: Timestamp, window: Duration) -> Option<Timestamp> {
    let quiet_for = Duration::try_from(now.duration_since(at)).unwrap_or(Duration::ZERO);
    match reply::counts(quiet_for, go, window) {
        Reply::Counts => None,
        Reply::Settling => Some(at.checked_add(window).unwrap_or(Timestamp::MAX)),
    }
}

/// Where the decider's reply to the questions stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Readiness {
    /// No comment of the decider is newer than `since`, the latest ask or
    /// what the last answer check read, and no thumbs-up of theirs
    /// ([`new_answer`]).
    Waiting { since: Timestamp },
    /// The decider answered, but may still be writing: the reply counts at
    /// `counts_at` ([`counts_at`]). A thumbs-up waits the same way, from
    /// its own time, and can never end with `go` (`thumbs_up`, OWL-371).
    Settling {
        counts_at: Timestamp,
        thumbs_up: bool,
    },
    /// The answer check may read the reply, or the thumbs-up resumes the
    /// ticket ([`answer`] tells which).
    Counts,
}

/// Whether the answer check may run on the decider's reply to `asked`, at
/// `now` by this machine's clock with the project's quiet `window`: what
/// `owlshift continue` checks before it runs one, and `owlshift watch`
/// before it continues a ticket (OWL-152). `None` when nothing was asked.
pub fn readiness(
    comments: &[Comment],
    decider: &Person,
    asked: &TicketQuestions,
    now: Timestamp,
    window: Duration,
) -> Option<Readiness> {
    let since = asked.answers_after()?;
    let (settling, thumbs_up) = match answer(comments, decider, asked) {
        None => return Some(Readiness::Waiting { since }),
        Some(Answer::Typed(_)) => (counts_at(comments, decider, now, window), false),
        Some(Answer::ThumbsUp(reaction)) => {
            (settles_until(reaction.created_at, false, now, window), true)
        }
    };
    Some(match settling {
        Some(counts_at) => Readiness::Settling {
            counts_at,
            thumbs_up,
        },
        None => Readiness::Counts,
    })
}

/// The core event an answer check's outcome maps onto, given the brief it
/// ran with, and its result when it left a valid one.
///
/// A `done` result folds its verdicts into one event
/// ([`Event::from_answer_check`]). Any other status is a failed run: the
/// role returns `done` or `failed`, and questions, a block or a false premise
/// mean nothing while the ticket waits for its decider. A refused result,
/// when the brief told the check of no earlier refusal
/// ([`freed_refusal`]), is [`Event::ResultRefused`]: it never spends the
/// last attempt, since the next check is told why (OWL-190). Every other
/// outcome maps as for any run ([`core_event`]): a usage limit is an
/// interruption, a run without a valid result a failed run, a breach a
/// quarantine. A failed check counts toward the ticket's failed runs, so a
/// second one parks it, save a freed refusal.
pub fn event<'a>(outcome: &'a Outcome, brief: &Brief) -> (Event, Option<&'a RunResult>) {
    if freed_refusal(outcome, brief) {
        return (Event::ResultRefused, None);
    }
    match outcome {
        Outcome::Finished { result, .. } => {
            let event = match result.status {
                // A result checked against its brief has a verdict for each
                // question of the latest ask, so the fold always gives an
                // event; none would be a check that checked nothing.
                result::Status::Done => {
                    Event::from_answer_check(result.verdicts.iter().map(|v| v.class))
                        .unwrap_or(Event::RunFailed)
                }
                result::Status::Failed
                | result::Status::Questions
                | result::Status::Blocked
                | result::Status::PremiseFalse => Event::RunFailed,
            };
            (event, Some(result))
        }
        _ => core_event(outcome),
    }
}

/// Keeps in `questions` what an answer check leaves for the next one, given
/// its core `event` ([`event`], or a quarantine) and its `outcome`, for
/// `owlshift continue` and the stand-in driver alike. A check that gave its
/// verdicts keeps what it read through and its verdicts
/// ([`TicketQuestions::keep_check`]). The latest ask keeps why the check's
/// result was refused ([`result_refusal`]), for the next check on it, which
/// is told (OWL-184), whether it counted or was freed (OWL-190); any other
/// outcome clears it, a quarantine included. A
/// check interrupted by a usage limit keeps nothing, so the next one is told
/// what this one was. Returns the verdicts kept, none when it gave none.
pub fn keep(
    questions: &mut TicketQuestions,
    read_through: Option<Timestamp>,
    event: Event,
    outcome: &Outcome,
) -> Vec<Verdict> {
    let verdicts = match (event, outcome) {
        (Event::Interrupted, _) => return Vec::new(),
        (
            Event::Answered | Event::Incomplete | Event::CounterQuestion,
            Outcome::Finished { result, .. },
        ) => {
            // The answers this check read are judged: only a newer comment
            // of the decider is a new answer.
            questions.keep_check(read_through, &result.verdicts);
            result.verdicts.clone()
        }
        _ => Vec::new(),
    };
    let refusal = match event {
        Event::Quarantined => None,
        _ => result_refusal(outcome),
    };
    questions.keep_refusal(refusal);
    verdicts
}

/// The questions an incomplete answer asks again: those of the brief's
/// latest ask whose verdict is `partial` or `unanswered`, in question order,
/// each with its verdict, whose reason says what is missing. They keep their
/// ids, so the re-ask's `reask` entry names them as asked.
pub fn open_questions(brief: &Brief, verdicts: &[Verdict]) -> Vec<(Question, Verdict)> {
    let Some((_, asked)) = brief.latest_ask() else {
        return Vec::new();
    };
    asked
        .iter()
        .filter_map(|question| {
            verdicts
                .iter()
                .find(|verdict| {
                    verdict.question == question.id
                        && matches!(
                            verdict.class,
                            AnswerClass::Partial | AnswerClass::Unanswered
                        )
                })
                .map(|verdict| (question.clone(), verdict.clone()))
        })
        .collect()
}

/// What a REPLY comment answers ([`crate::writer::ReplyComment`]): the
/// questions of the brief's latest ask whose verdict is a counter-question,
/// in question order, each with the reply the check wrote on that verdict.
/// A valid result has a reply on every counter-question verdict
/// (`result::RunResult::validate`), so none is left out.
pub fn counter_replies(brief: &Brief, verdicts: &[Verdict]) -> Vec<(Question, String)> {
    let Some((_, asked)) = brief.latest_ask() else {
        return Vec::new();
    };
    asked
        .iter()
        .filter_map(|question| {
            verdicts
                .iter()
                .find(|verdict| {
                    verdict.question == question.id && verdict.class == AnswerClass::CounterQuestion
                })
                .and_then(|verdict| verdict.reply.clone())
                .map(|reply| (question.clone(), reply))
        })
        .collect()
}

/// What a RESUME comment restates ([`crate::writer::ResumeComment`]): every
/// question of a round, from its first ask, with the latest verdict an
/// answer check gave on it. `asks` are the round's asks in order, each with
/// the verdicts kept on it; a re-ask's verdicts follow its round's, so a
/// question asked again reads its last verdict. `None` for a question no
/// kept verdict names (an ask read from format 2 of `questions.json`).
pub fn understood<'a>(
    asks: impl IntoIterator<Item = (&'a [Question], &'a [Verdict])>,
) -> Vec<(Question, Option<Verdict>)> {
    let mut asks = asks.into_iter();
    let Some((questions, first)) = asks.next() else {
        return Vec::new();
    };
    let mut understood: Vec<(Question, Option<Verdict>)> = questions
        .iter()
        .map(|question| {
            let verdict = first.iter().find(|v| v.question == question.id).cloned();
            (question.clone(), verdict)
        })
        .collect();
    for (_, verdicts) in asks {
        for verdict in verdicts {
            if let Some((_, kept)) = understood
                .iter_mut()
                .find(|(question, _)| question.id == verdict.question)
            {
                *kept = Some(verdict.clone());
            }
        }
    }
    understood
}

#[cfg(test)]
mod tests {
    use owlshift_contracts::format::BRIEF_FORMAT;

    use super::*;
    use crate::artifact::ArtifactContents;
    use crate::executor::Failure;
    use owlshift_adapters::tracker::Author;

    #[test]
    fn answers_arrive_with_a_decider_comment_newer_than_the_ask_and_the_last_check() {
        let decider = Person {
            id: "u1".into(),
            name: "Maintainer".into(),
        };
        let at = |minute: u32| -> Timestamp {
            format!("2026-10-02T10:{minute:02}:00Z").parse().unwrap()
        };
        let by = |id: &str, created: u32, edited: Option<u32>, body: &str| Comment {
            id: format!("c{created}"),
            author: Author::Account(Person {
                id: id.into(),
                name: "someone".into(),
            }),
            created_at: at(created),
            edited_at: edited.map(at),
            body: body.into(),
            parent: None,
            reactions: Vec::new(),
        };
        let mut asked = TicketQuestions::parse(
            r#"{"format":2,"asks":[{"kind":"questions","round":1,"at":"2026-10-02T10:10:00Z",
                "comment":"c10","questions":[{"id":"Q1","category":"scope","context":"c","text":"t"}],
                "decider":{"account":"u1","by":"assignee"}}]}"#,
        )
        .unwrap();
        let marked = "[owlshift] DELIVERY\n\nDone.\n";
        let cases = [
            (vec![], None),
            (vec![by("u1", 9, None, "Early.")], None),
            (vec![by("u1", 10, None, "Same time.")], None),
            (
                vec![by("u1", 9, None, "a"), by("u1", 11, None, "b")],
                Some(11),
            ),
            (vec![by("u1", 9, Some(12), "Edited after.")], Some(12)),
            (vec![by("u2", 11, None, "Not the decider.")], None),
            (vec![by("u1", 11, None, marked)], None),
        ];
        for (comments, expected) in cases {
            assert_eq!(
                new_answer(&comments, &decider, &asked),
                expected.map(at),
                "{comments:?}"
            );
        }
        // An answer the last check already read is not new.
        asked.checked_through = Some(at(12));
        assert_eq!(
            new_answer(&[by("u1", 11, None, "Read.")], &decider, &asked),
            None
        );
        assert_eq!(
            new_answer(&[by("u1", 13, None, "New.")], &decider, &asked),
            Some(at(13))
        );
        assert_eq!(new_answer(&[], &decider, &TicketQuestions::new()), None);
    }

    /// A round posted in parts (OWL-223), on Linear by the key's account,
    /// which may be the decider's: no part is an answer, an answer counts
    /// after the last part, the one the ask keeps, and a brief's thread
    /// holds the round once, in its `questions` entry; a later comment that
    /// copies a part's marker stays a comment.
    #[test]
    fn a_round_posted_in_parts_is_never_an_answer() {
        use std::num::NonZeroU32;

        use owlshift_contracts::brief::ThreadEntry;
        use owlshift_contracts::ids::{QuestionId, TicketId};

        use crate::on_demand::thread;
        use crate::writer::QuestionsComment;

        let decider = Person {
            id: "u1".into(),
            name: "Maintainer".into(),
        };
        let at = |minute: u32| -> Timestamp {
            format!("2026-10-02T10:{minute:02}:00Z").parse().unwrap()
        };
        let question = |n: u32| Question {
            id: QuestionId::new(format!("Q{n}")).unwrap(),
            category: "scope".into(),
            context: "c".repeat(300),
            text: "t".into(),
            options: Vec::new(),
            recommendation: None,
            replan: false,
        };
        let parts = QuestionsComment {
            ticket: TicketId::new("OWL-1").unwrap(),
            round: NonZeroU32::new(1).unwrap(),
            summary: String::new(),
            questions: vec![question(1), question(2)],
            premise_false: false,
            decided: 0,
            fallback: None,
            reopened: Vec::new(),
            notes: Vec::new(),
        }
        .parts(600);
        assert_eq!(parts.len(), 2);
        let by_decider = |created: u32, body: &str| Comment {
            id: format!("c{created}"),
            author: Author::Account(decider.clone()),
            created_at: at(created),
            edited_at: None,
            body: body.into(),
            parent: None,
            reactions: Vec::new(),
        };
        let mut comments = vec![by_decider(9, &parts[0]), by_decider(10, &parts[1])];
        let asked = TicketQuestions::parse(
            r#"{"format":2,"asks":[{"kind":"questions","round":1,"at":"2026-10-02T10:10:00Z",
                "comment":"c10","questions":[{"id":"Q1","category":"scope","context":"c","text":"t"}],
                "decider":{"account":"u1","by":"assignee"}}]}"#,
        )
        .unwrap();
        assert_eq!(new_answer(&comments, &decider, &asked), None);
        comments.push(by_decider(11, "Q1: yes."));
        assert_eq!(new_answer(&comments, &decider, &asked), Some(at(11)));

        let entries = thread(&comments, &asked.asks, &[], &decider);
        assert!(
            matches!(&entries[..], [
                ThreadEntry::Questions { .. },
                ThreadEntry::Comment { author: answer, .. },
            ] if answer.relation == Relation::Decider),
            "{entries:?}"
        );

        // A copy of a part posted after the ask, or of another round, is a
        // comment still.
        comments.push(by_decider(12, &parts[0]));
        comments.insert(0, by_decider(8, &parts[0].replace("round 1", "round 2")));
        let entries = thread(&comments, &asked.asks, &[], &decider);
        assert!(
            matches!(
                &entries[..],
                [
                    ThreadEntry::Comment { .. },
                    ThreadEntry::Questions { .. },
                    ThreadEntry::Comment { .. },
                    ThreadEntry::Comment { .. },
                ]
            ),
            "{entries:?}"
        );
    }

    /// An attempt at a round posted in parts that was abandoned (its ref
    /// write failed), then the round posted again and kept: the thread
    /// folds the kept attempt's earlier part only, the abandoned attempt
    /// stays comments as a round not kept does, and the answer check still
    /// counts an answer only after the kept last part.
    #[test]
    fn only_the_kept_attempts_parts_are_folded() {
        use std::num::NonZeroU32;

        use owlshift_contracts::brief::ThreadEntry;
        use owlshift_contracts::ids::{QuestionId, TicketId};

        use crate::on_demand::thread;
        use crate::writer::QuestionsComment;

        let decider = Person {
            id: "u1".into(),
            name: "Maintainer".into(),
        };
        let at = |minute: u32| -> Timestamp {
            format!("2026-10-02T10:{minute:02}:00Z").parse().unwrap()
        };
        let question = |n: u32| Question {
            id: QuestionId::new(format!("Q{n}")).unwrap(),
            category: "scope".into(),
            context: "c".repeat(300),
            text: "t".into(),
            options: Vec::new(),
            recommendation: None,
            replan: false,
        };
        let parts = QuestionsComment {
            ticket: TicketId::new("OWL-1").unwrap(),
            round: NonZeroU32::new(1).unwrap(),
            summary: String::new(),
            questions: vec![question(1), question(2)],
            premise_false: false,
            decided: 0,
            fallback: None,
            reopened: Vec::new(),
            notes: Vec::new(),
        }
        .parts(600);
        assert_eq!(parts.len(), 2);
        let by_decider = |created: u32, body: &str| Comment {
            id: format!("c{created}"),
            author: Author::Account(decider.clone()),
            created_at: at(created),
            edited_at: None,
            body: body.into(),
            parent: None,
            reactions: Vec::new(),
        };
        // The abandoned attempt at 5 and 6, the kept one at 9 and 10.
        let mut comments = vec![
            by_decider(5, &parts[0]),
            by_decider(6, &parts[1]),
            by_decider(9, &parts[0]),
            by_decider(10, &parts[1]),
        ];
        let asked = TicketQuestions::parse(
            r#"{"format":2,"asks":[{"kind":"questions","round":1,"at":"2026-10-02T10:10:00Z",
                "comment":"c10","questions":[{"id":"Q1","category":"scope","context":"c","text":"t"}],
                "decider":{"account":"u1","by":"assignee"}}]}"#,
        )
        .unwrap();
        assert_eq!(new_answer(&comments, &decider, &asked), None);
        comments.push(by_decider(11, "Q1: yes."));
        assert_eq!(new_answer(&comments, &decider, &asked), Some(at(11)));

        let entries = thread(&comments, &asked.asks, &[], &decider);
        assert!(
            matches!(
                &entries[..],
                [
                    ThreadEntry::Comment { .. },
                    ThreadEntry::Comment { .. },
                    ThreadEntry::Questions { .. },
                    ThreadEntry::Comment { author: answer, .. },
                ] if answer.relation == Relation::Decider
            ),
            "{entries:?}"
        );

        // An attempt stopped after its first part, then the round posted
        // in parts again with a person's comment between them: the stray
        // first part stays a comment, the kept attempt is folded.
        let comments = vec![
            by_decider(5, &parts[0]),
            by_decider(8, &parts[0]),
            by_decider(9, "Looking."),
            by_decider(10, &parts[1]),
        ];
        let entries = thread(&comments, &asked.asks, &[], &decider);
        assert!(
            matches!(
                &entries[..],
                [
                    ThreadEntry::Comment { .. },
                    ThreadEntry::Comment { .. },
                    ThreadEntry::Questions { .. },
                ]
            ),
            "{entries:?}"
        );
    }

    /// OWL-228: a RE-ASK, a REPLY and a PARKED comment cut to hold in one
    /// comment, posted on Linear by the key's account, which may be the
    /// decider's, are still Owlshift's: none is an answer, and the RE-ASK
    /// the ask keeps takes the place of its comment in the thread.
    #[test]
    fn a_comment_cut_to_hold_is_never_an_answer() {
        use std::num::NonZeroU32;

        use owlshift_contracts::brief::ThreadEntry;
        use owlshift_contracts::ids::{QuestionId, TicketId};
        use owlshift_core::state::ParkReason;

        use crate::on_demand::thread;
        use crate::writer::{
            MAX_COMMENT_CHARS, ParkedComment, ReaskComment, ReplyComment, Restart,
        };

        let decider = Person {
            id: "u1".into(),
            name: "Maintainer".into(),
        };
        let at = |minute: u32| -> Timestamp {
            format!("2026-10-02T10:{minute:02}:00Z").parse().unwrap()
        };
        let ticket = TicketId::new("OWL-1").unwrap();
        let round = NonZeroU32::new(1).unwrap();
        let question = Question {
            id: QuestionId::new("Q1").unwrap(),
            category: "scope".into(),
            context: String::new(),
            text: "t".into(),
            options: Vec::new(),
            recommendation: None,
            replan: false,
        };
        let verdict = Verdict {
            question: question.id.clone(),
            class: AnswerClass::Partial,
            reason: "m".repeat(2 * MAX_COMMENT_CHARS),
            reply: None,
        };
        let open = vec![(question.clone(), verdict.clone())];
        let bodies = [
            ReaskComment {
                ticket: ticket.clone(),
                round,
                reask: 1,
                open: open.clone(),
            }
            .render(),
            ReplyComment {
                ticket: ticket.clone(),
                replies: vec![(question, verdict.reason)],
            }
            .render(),
            ParkedComment {
                ticket,
                reason: ParkReason::Reasks,
                detail: String::new(),
                round: Some(round),
                open,
                restart: Restart::Continue,
            }
            .render(),
        ];
        let comments: Vec<Comment> = bodies
            .iter()
            .zip(10..)
            .map(|(body, minute)| {
                assert!(body.chars().count() <= MAX_COMMENT_CHARS);
                assert!(body.contains(" … (cut: "));
                Comment {
                    id: format!("c{minute}"),
                    author: Author::Account(decider.clone()),
                    created_at: at(minute),
                    edited_at: None,
                    body: body.clone(),
                    parent: None,
                    reactions: Vec::new(),
                }
            })
            .collect();
        let asked = TicketQuestions::parse(
            r#"{"format":2,"asks":[{"kind":"questions","round":1,"at":"2026-10-02T10:09:00Z",
                "comment":"c9","questions":[{"id":"Q1","category":"scope","context":"","text":"t"}],
                "decider":{"account":"u1","by":"assignee"}},
                {"kind":"reask","round":1,"at":"2026-10-02T10:10:00Z",
                "comment":"c10","questions":[{"id":"Q1","category":"scope","context":"","text":"t"}],
                "decider":{"account":"u1","by":"assignee"}}]}"#,
        )
        .unwrap();
        assert_eq!(new_answer(&comments, &decider, &asked), None);
        let entries = thread(&comments, &asked.asks, &[], &decider);
        assert!(
            matches!(&entries[..], [
                ThreadEntry::Questions { .. },
                ThreadEntry::Reask { .. },
                ThreadEntry::Comment { author: reply, .. },
                ThreadEntry::Comment { author: parked, .. },
            ] if reply.relation == Relation::Owlshift && parked.relation == Relation::Owlshift),
            "{entries:?}"
        );
    }

    #[test]
    fn the_deciders_newest_comment_counts_after_the_quiet_window_or_with_go() {
        let decider = Person {
            id: "u1".into(),
            name: "Maintainer".into(),
        };
        let at = |minute: u32| -> Timestamp {
            format!("2026-10-02T10:{minute:02}:00Z").parse().unwrap()
        };
        let by = |id: &str, created: u32, edited: Option<u32>, body: &str| Comment {
            id: format!("c{created}"),
            author: Author::Account(Person {
                id: id.into(),
                name: "someone".into(),
            }),
            created_at: at(created),
            edited_at: edited.map(at),
            body: body.into(),
            parent: None,
            reactions: Vec::new(),
        };
        let marked = "[owlshift] DELIVERY\n\nDone. go\n";
        let cases = [
            // Within the window, then at its end.
            (vec![by("u1", 10, None, "Q1: yes.")], 15, Some(20)),
            (vec![by("u1", 10, None, "Q1: yes.")], 20, None),
            // `go` counts at once.
            (vec![by("u1", 10, None, "Q1: yes. Go.")], 10, None),
            // An edit restarts the window, of an older comment too, and an
            // older `go` does not cover a newer comment.
            (vec![by("u1", 10, Some(14), "Q1: yes.")], 20, Some(24)),
            (
                vec![
                    by("u1", 5, Some(14), "Q1: yes."),
                    by("u1", 10, None, "Q2: no."),
                ],
                20,
                Some(24),
            ),
            (
                vec![
                    by("u1", 10, None, "Q1: yes. go"),
                    by("u1", 12, None, "Q2: no."),
                ],
                15,
                Some(22),
            ),
            // Edited last at the same time: the one created last decides.
            (
                vec![
                    by("u1", 12, None, "Q2: no. go"),
                    by("u1", 5, Some(12), "Q1: yes."),
                ],
                15,
                None,
            ),
            // Only the decider's own comments: not another person's, not a
            // marked comment, whatever they end with.
            (
                vec![by("u1", 10, None, "Q1: yes."), by("u2", 12, None, "go")],
                15,
                Some(20),
            ),
            (
                vec![by("u1", 10, None, "Q1: yes."), by("u1", 12, None, marked)],
                15,
                Some(20),
            ),
            (vec![by("u2", 10, None, "Q1: yes.")], 10, None),
            // A last edit after this machine's clock waits the whole window
            // from that edit, the time a retry then counts at.
            (vec![by("u1", 30, None, "Q1: yes.")], 25, Some(40)),
            (vec![by("u1", 30, None, "Q1: yes.")], 40, None),
        ];
        let window = reply::DEFAULT_QUIET_WINDOW;
        for (comments, now, expected) in cases {
            assert_eq!(
                counts_at(&comments, &decider, at(now), window),
                expected.map(at),
                "{comments:?} at {now}"
            );
        }

        // A project's own window replaces the default.
        let comments = [by("u1", 10, None, "Q1: yes.")];
        let hour = Duration::from_secs(60 * 60);
        assert_eq!(
            counts_at(&comments, &decider, at(15), hour),
            Some(at(10) + hour)
        );
        let two = Duration::from_secs(2 * 60);
        assert_eq!(counts_at(&comments, &decider, at(12), two), None);
    }

    /// OWL-371: the decider, a moment of the bench's day, and an ask of
    /// `questions` posted at 10:10 as comment `c10`, kept with the checks
    /// read through `checked`.
    mod thumbs {
        use std::num::NonZeroU32;

        use owlshift_adapters::tracker::{Author, Reaction};
        use owlshift_contracts::ids::QuestionId;
        use owlshift_contracts::refs::{Ask, AskDecider, AskKind};
        use owlshift_contracts::result::{QuestionOption, Recommendation};
        use owlshift_core::decider::DeciderRule;

        use super::*;

        pub fn decider() -> Person {
            Person {
                id: "u1".into(),
                name: "Maintainer".into(),
            }
        }

        pub fn at(minute: u32) -> Timestamp {
            format!("2026-10-19T10:{minute:02}:00Z").parse().unwrap()
        }

        /// One question, B recommended among Yes and No.
        pub fn lettered(id: &str) -> Question {
            Question {
                id: QuestionId::new(id).unwrap(),
                category: "scope".into(),
                context: String::new(),
                text: "Should the README link to the greeting?".into(),
                options: vec![QuestionOption::new("Yes"), QuestionOption::new("No.")],
                recommendation: Some(Recommendation {
                    option: Some("B".into()),
                    why: "The README lists code only.".into(),
                }),
                replan: false,
            }
        }

        pub fn asked(questions: Vec<Question>, checked: Option<u32>) -> TicketQuestions {
            let mut asked = TicketQuestions::new();
            asked.asks.push(Ask {
                kind: AskKind::Questions,
                round: NonZeroU32::new(1).unwrap(),
                at: at(10),
                comment: "c10".into(),
                questions,
                decider: AskDecider {
                    account: "u1".into(),
                    by: DeciderRule::Assignee,
                },
                verdicts: Vec::new(),
                result_refusal: None,
            });
            asked.checked_through = checked.map(at);
            asked
        }

        pub fn account(id: &str) -> Author {
            Author::Account(Person {
                id: id.into(),
                name: "someone".into(),
            })
        }

        pub fn reaction(author: Author, emoji: &str, minute: u32) -> Reaction {
            Reaction {
                emoji: emoji.into(),
                author,
                created_at: at(minute),
            }
        }

        /// A comment `c<minute>` by `author`, with `reactions`.
        pub fn comment(author: Author, minute: u32, reactions: Vec<Reaction>) -> Comment {
            Comment {
                id: format!("c{minute}"),
                author,
                created_at: at(minute),
                edited_at: None,
                body: format!("Comment of minute {minute}."),
                parent: None,
                reactions,
            }
        }

        /// The ask's comment, Owlshift's, with `reactions`.
        pub fn round(reactions: Vec<Reaction>) -> Comment {
            Comment {
                body: "[owlshift] QUESTIONS · round 1".into(),
                ..comment(
                    Author::Other {
                        name: "Owlshift".into(),
                    },
                    10,
                    reactions,
                )
            }
        }
    }

    #[test]
    fn a_thumbs_up_answers_only_as_the_rule_says() {
        use thumbs::*;
        let up = |minute| reaction(account("u1"), "+1", minute);
        let two = vec![lettered("Q1"), lettered("Q2")];
        let unrecommended = Question {
            recommendation: None,
            ..lettered("Q1")
        };
        // A thumbs-up's answer, or a typed one, at a minute.
        const UP: bool = true;
        const TYPED: bool = false;
        // (comments, ask's questions, checked through, expected answer)
        type Case = (
            Vec<Comment>,
            Vec<Question>,
            Option<u32>,
            Option<(bool, u32)>,
        );
        let cases: Vec<Case> = vec![
            (vec![round(vec![])], vec![lettered("Q1")], None, None),
            (
                vec![round(vec![up(11)])],
                vec![lettered("Q1")],
                None,
                Some((UP, 11)),
            ),
            // Nobody else's: another account, an app user, an external one.
            (
                vec![round(vec![reaction(account("u2"), "+1", 11)])],
                vec![lettered("Q1")],
                None,
                None,
            ),
            (
                vec![round(vec![reaction(
                    Author::Other {
                        name: "Owlshift".into(),
                    },
                    "+1",
                    11,
                )])],
                vec![lettered("Q1")],
                None,
                None,
            ),
            // No other emoji, no other comment (an earlier part or round).
            (
                vec![round(vec![reaction(account("u1"), "heart", 11)])],
                vec![lettered("Q1")],
                None,
                None,
            ),
            (
                vec![comment(account("u2"), 9, vec![up(11)]), round(vec![])],
                vec![lettered("Q1")],
                None,
                None,
            ),
            // No ask of several questions, nor one without a
            // recommendation (a ref kept before format 10).
            (vec![round(vec![up(11)])], two, None, None),
            (vec![round(vec![up(11)])], vec![unrecommended], None, None),
            // Not before what the last check read.
            (
                vec![round(vec![up(11)])],
                vec![lettered("Q1")],
                Some(12),
                None,
            ),
            (
                vec![round(vec![up(13)])],
                vec![lettered("Q1")],
                Some(12),
                Some((UP, 13)),
            ),
            // Words win, older or newer than the thumbs-up.
            (
                vec![round(vec![up(11)]), comment(account("u1"), 12, vec![])],
                vec![lettered("Q1")],
                None,
                Some((TYPED, 12)),
            ),
            (
                vec![round(vec![up(13)]), comment(account("u1"), 11, vec![])],
                vec![lettered("Q1")],
                None,
                Some((TYPED, 11)),
            ),
        ];
        for (comments, questions, checked, expected) in &cases {
            let asked = asked(questions.clone(), *checked);
            let found = answer(comments, &decider(), &asked).map(|answer| {
                (
                    matches!(answer, Answer::ThumbsUp(reaction) if reaction.is_thumbs_up()),
                    answer.at(),
                )
            });
            let expected = expected.map(|(up, minute)| (up, at(minute)));
            assert_eq!(found, expected, "{comments:?} {questions:?}");
            assert_eq!(
                new_answer(comments, &decider(), &asked),
                expected.map(|(_, at)| at)
            );
        }
    }

    #[test]
    fn a_thumbs_up_settles_through_the_quiet_window() {
        use thumbs::*;
        let window = reply::DEFAULT_QUIET_WINDOW;
        let asked = asked(vec![lettered("Q1")], None);
        let up = |minute| vec![round(vec![reaction(account("u1"), "+1", minute)])];
        let readiness_at = |comments: &[Comment], now| {
            readiness(comments, &decider(), &asked, at(now), window).unwrap()
        };
        assert_eq!(
            readiness_at(&up(11), 15),
            Readiness::Settling {
                counts_at: at(21),
                thumbs_up: true
            }
        );
        assert_eq!(readiness_at(&up(11), 21), Readiness::Counts);
        // Removed within the window: nothing to wait for.
        assert_eq!(
            readiness_at(&[round(vec![])], 15),
            Readiness::Waiting { since: at(10) }
        );
        // Put back later, or seen ahead of this machine's clock: the whole
        // window from its own time.
        assert_eq!(
            readiness_at(&up(30), 25),
            Readiness::Settling {
                counts_at: at(40),
                thumbs_up: true
            }
        );
        // A typed answer settles as before, and is no thumbs-up.
        let typed = [round(vec![]), thumbs::comment(account("u1"), 11, vec![])];
        assert_eq!(
            readiness_at(&typed, 15),
            Readiness::Settling {
                counts_at: at(21),
                thumbs_up: false
            }
        );
    }

    #[test]
    fn a_thumbs_up_takes_the_recommendation() {
        use thumbs::*;
        let unlettered = Question {
            options: Vec::new(),
            recommendation: Some(owlshift_contracts::result::Recommendation {
                option: None,
                why: "GREETING.md, the simplest reading.".into(),
            }),
            ..lettered("Q1")
        };
        let verdicts = thumbs_up_verdicts(&[lettered("Q1"), unlettered]);
        let read: Vec<(&str, AnswerClass, &str)> = verdicts
            .iter()
            .map(|v| (v.question.as_str(), v.class, v.reason.as_str()))
            .collect();
        assert_eq!(
            read,
            [
                (
                    "Q1",
                    AnswerClass::Answered,
                    "The decider's thumbs-up on the question: its recommendation, B, No."
                ),
                (
                    "Q1",
                    AnswerClass::Answered,
                    "The decider's thumbs-up on the question: its recommendation: GREETING.md, \
                     the simplest reading."
                ),
            ]
        );
        assert!(verdicts.iter().all(|v| v.reply.is_none()));
    }

    #[test]
    fn the_approval_names_the_comment_that_answered() {
        use thumbs::*;
        let questions = vec![lettered("Q1")];
        // The ask as the ticket ref keeps it once a check or a thumbs-up
        // resumed it, with the verdicts that resumed it.
        let resumed = |verdicts: Vec<Verdict>, checked| {
            let mut asked = asked(questions.clone(), Some(checked));
            asked.asks[0].verdicts = verdicts;
            asked
        };
        let named = |comments: &[Comment], asked: &TicketQuestions| {
            answering_comment(comments, &decider(), asked)
        };
        let by_thumbs_up = resumed(thumbs_up_verdicts(&questions), 11);
        let earlier = comment(account("u1"), 5, vec![]);
        let up = reaction(account("u1"), "+1", 11);
        // Resumed by the thumbs-up, the decider having spoken in an
        // earlier round: the round's comment, never that older one, the
        // reaction still there, taken back since, or the round's comment
        // gone from the listing.
        for comments in [
            vec![earlier.clone(), round(vec![up.clone()])],
            vec![earlier.clone(), round(vec![])],
            vec![earlier.clone()],
        ] {
            assert_eq!(named(&comments, &by_thumbs_up).as_deref(), Some("c10"));
        }
        // A counter-question replied to, then the thumbs-up resumed it.
        let counter = comment(account("u1"), 11, vec![]);
        let after_counter = resumed(thumbs_up_verdicts(&questions), 13);
        assert_eq!(
            named(&[round(vec![]), counter], &after_counter).as_deref(),
            Some("c10")
        );
        // The words a check judged, a thumbs-up after them: the words.
        let words = comment(account("u1"), 12, vec![]);
        let by_words = resumed(
            vec![Verdict {
                question: owlshift_contracts::ids::QuestionId::new("Q1").unwrap(),
                class: AnswerClass::Answered,
                reason: "Approved as it is.".into(),
                reply: None,
            }],
            12,
        );
        assert_eq!(
            named(&[earlier, round(vec![up]), words], &by_words).as_deref(),
            Some("c12")
        );
        assert!(!resumed_by_thumbs_up(&[], &questions));
    }

    #[test]
    fn late_comments_are_the_deciders_edited_after_what_the_run_read() {
        let decider = Person {
            id: "u1".into(),
            name: "Maintainer".into(),
        };
        let at = |minute: u32| -> Timestamp {
            format!("2026-10-02T10:{minute:02}:00Z").parse().unwrap()
        };
        let by = |id: &str, created: u32, edited: Option<u32>, body: &str| Comment {
            id: format!("c{created}"),
            author: Author::Account(Person {
                id: id.into(),
                name: "someone".into(),
            }),
            created_at: at(created),
            edited_at: edited.map(at),
            body: body.into(),
            parent: None,
            reactions: Vec::new(),
        };
        let ids = |late: Vec<&Comment>| -> Vec<String> {
            late.into_iter().map(|c| c.id.clone()).collect()
        };
        let read = [by("u1", 5, None, "Q1: yes."), by("u2", 6, None, "Nice.")];
        // Nothing new, and what others or the runner wrote since: none.
        assert!(late_comments(&read, &read, &decider).is_empty());
        let mut now = read.to_vec();
        now.push(by("u2", 8, None, "Also French?"));
        now.push(by("u1", 9, None, "[owlshift] DELIVERY\n\nDone.\n"));
        assert!(late_comments(&read, &now, &decider).is_empty());
        // A new comment of the decider, and an edit of an old one.
        now.push(by("u1", 10, None, "Also say Bonjour."));
        assert_eq!(ids(late_comments(&read, &now, &decider)), ["c10"]);
        now[0] = by("u1", 5, Some(11), "Q1: yes, in bold.");
        assert_eq!(ids(late_comments(&read, &now, &decider)), ["c5", "c10"]);
        // A brief that held no comment of the decider saw none of them.
        assert_eq!(ids(late_comments(&[], &now, &decider)), ["c5", "c10"]);
    }

    fn finished(result: &str) -> Outcome {
        Outcome::Finished {
            result: Box::new(RunResult::parse(result).unwrap()),
            artifacts: ArtifactContents::default(),
        }
    }

    fn done(classes: &[&str]) -> Outcome {
        let verdicts: Vec<String> = classes
            .iter()
            .enumerate()
            .map(|(n, class)| {
                let reply = if *class == "counter_question" {
                    r#","reply":"r""#
                } else {
                    ""
                };
                format!(
                    r#"{{"question":"Q{}","class":"{class}","reason":"r"{reply}}}"#,
                    n + 1
                )
            })
            .collect();
        finished(&format!(
            r#"{{"format":12,"status":"done","summary":"s","verdicts":[{}]}}"#,
            verdicts.join(",")
        ))
    }

    /// An answer check's brief, told of an earlier refusal or not.
    fn check_brief(refusal: Option<&str>) -> Brief {
        let refusal = refusal.map_or(String::new(), |reason| {
            format!(r#","result_refusal":"{reason}""#)
        });
        Brief::parse(&format!(
            r#"{{"format":{BRIEF_FORMAT},"role":"answer_check","project":"p",
                "ticket":{{"id":"T-1","title":"t","author":{{"name":"a","relation":"decider"}},"description":"d"}},
                "decider":"a","thread":[],
                "permissions":{{"level":"read_only","network":false,"browser":false}},
                "gate":[],"always_human":[],"result_path":"result.json"{refusal}}}"#
        ))
        .unwrap()
    }

    #[test]
    fn a_done_check_folds_its_verdicts_and_anything_else_maps_as_a_run() {
        let untold = check_brief(None);
        let event_of = |outcome: &Outcome| event(outcome, &untold).0;
        assert_eq!(event_of(&done(&["answered"])), Event::Answered);
        assert_eq!(event_of(&done(&["answered", "partial"])), Event::Incomplete);
        assert_eq!(
            event_of(&done(&["unanswered", "counter_question"])),
            Event::CounterQuestion
        );
        // A check that checked nothing, and the statuses the role never
        // returns, are failed runs.
        assert_eq!(event_of(&done(&[])), Event::RunFailed);
        for status in ["failed", "blocked", "premise_false"] {
            let outcome = finished(&format!(
                r#"{{"format":12,"status":"{status}","summary":"s"}}"#
            ));
            assert_eq!(event_of(&outcome), Event::RunFailed, "{status}");
        }
        let asking = finished(
            r#"{"format":12,"status":"questions","summary":"s","questions":[
                {"id":"Q1","category":"scope","context":"c","text":"t",
                "recommendation":{"why":"r"}}]}"#,
        );
        assert_eq!(event_of(&asking), Event::RunFailed);
        assert!(
            event(&asking, &untold).1.is_some(),
            "the result is still returned"
        );

        assert_eq!(
            event_of(&Outcome::UsageLimit { resets_at: None }),
            Event::Interrupted
        );
        assert_eq!(
            event_of(&Outcome::Failed(Failure::NoResult)),
            Event::RunFailed
        );
        assert_eq!(
            event_of(&Outcome::Quarantined(Vec::new())),
            Event::Quarantined
        );
    }

    /// OWL-190: a refused result is freed from the last attempt when the
    /// check was told of no earlier refusal, and counts when it was, so two
    /// refusals in a row never both go free. A failure the check is not the
    /// one to fix is a failed run either way.
    #[test]
    fn a_refused_check_is_freed_unless_it_was_told() {
        let refused = Outcome::Failed(Failure::InvalidResult("the verdict for Q3".to_owned()));
        let told = check_brief(Some("the verdict for Q2"));
        assert_eq!(
            event(&refused, &check_brief(None)),
            (Event::ResultRefused, None)
        );
        assert_eq!(event(&refused, &told), (Event::RunFailed, None));
        for brief in [check_brief(None), told] {
            assert_eq!(
                event(&Outcome::Failed(Failure::NoResult), &brief).0,
                Event::RunFailed
            );
        }
    }

    #[test]
    fn the_open_questions_and_the_replies_follow_the_latest_asks_verdicts() {
        let question = |id: &str| {
            format!(r#"{{"id":"{id}","category":"scope","context":"c {id}","text":"t {id}"}}"#)
        };
        let brief = |thread: &str| {
            Brief::parse(&format!(
                r#"{{"format":{BRIEF_FORMAT},"role":"answer_check","project":"p",
                    "ticket":{{"id":"T-1","title":"t","author":{{"name":"a","relation":"decider"}},"description":"d"}},
                    "decider":"a","thread":[{thread}],
                    "permissions":{{"level":"read_only","network":false,"browser":false}},
                    "gate":[],"always_human":[],"result_path":"result.json"}}"#
            ))
            .unwrap()
        };
        let reasked = brief(&format!(
            r#"{{"type":"questions","round":1,"at":"2026-09-28T09:00:00Z","questions":[{},{},{}]}},
               {{"type":"reask","round":1,"at":"2026-09-28T10:00:00Z","questions":[{},{}]}}"#,
            question("Q1"),
            question("Q2"),
            question("Q3"),
            question("Q2"),
            question("Q3"),
        ));
        let verdicts = RunResult::parse(
            r#"{"format":12,"status":"done","summary":"s","verdicts":[
                {"question":"Q2","class":"answered","reason":"Settled."},
                {"question":"Q3","class":"partial","reason":"The tone is missing."}]}"#,
        )
        .unwrap()
        .verdicts;
        let open = open_questions(&reasked, &verdicts);
        let ids: Vec<&str> = open.iter().map(|(q, _)| q.id.as_str()).collect();
        assert_eq!(ids, ["Q3"]);
        assert_eq!(open[0].1.reason, "The tone is missing.");
        assert_eq!(open[0].0.text, "t Q3");

        assert!(open_questions(&brief(""), &verdicts).is_empty());

        // A REPLY answers the latest ask's counter-questions, in question
        // order, each with its own reply, and nothing else.
        let asked = brief(&format!(
            r#"{{"type":"questions","round":1,"at":"2026-09-28T09:00:00Z","questions":[{},{},{}]}}"#,
            question("Q1"),
            question("Q2"),
            question("Q3"),
        ));
        let verdicts = RunResult::parse(
            r#"{"format":12,"status":"done","summary":"s","verdicts":[
                {"question":"Q1","class":"counter_question","reason":"Asks back.","reply":"R1"},
                {"question":"Q2","class":"unanswered","reason":"Nothing."},
                {"question":"Q3","class":"counter_question","reason":"Asks back.","reply":"R3"}]}"#,
        )
        .unwrap()
        .verdicts;
        let replies = counter_replies(&asked, &verdicts);
        let pairs: Vec<(&str, &str)> = replies
            .iter()
            .map(|(q, reply)| (q.id.as_str(), reply.as_str()))
            .collect();
        assert_eq!(pairs, [("Q1", "R1"), ("Q3", "R3")]);
        assert_eq!(replies[1].0.text, "t Q3");
        assert!(counter_replies(&brief(""), &verdicts).is_empty());
    }

    #[test]
    fn a_round_is_understood_from_the_last_verdict_on_each_question() {
        let question = |id: &str| Question {
            id: owlshift_contracts::ids::QuestionId::new(id).unwrap(),
            category: "scope".into(),
            context: "c".into(),
            text: format!("t {id}"),
            options: Vec::new(),
            recommendation: None,
            replan: false,
        };
        let verdict = |id: &str, class, reason: &str| Verdict {
            question: owlshift_contracts::ids::QuestionId::new(id).unwrap(),
            class,
            reason: reason.into(),
            reply: None,
        };
        let round = [question("Q1"), question("Q2"), question("Q3")];
        let first = [
            verdict("Q1", AnswerClass::Answered, "One."),
            verdict("Q2", AnswerClass::Partial, "Half."),
        ];
        let reasked = [question("Q2")];
        let second = [verdict("Q2", AnswerClass::Answered, "Two.")];
        let all = understood([(&round[..], &first[..]), (&reasked[..], &second[..])]);
        let read: Vec<(&str, Option<&str>)> = all
            .iter()
            .map(|(q, v)| (q.id.as_str(), v.as_ref().map(|v| v.reason.as_str())))
            .collect();
        assert_eq!(
            read,
            [("Q1", Some("One.")), ("Q2", Some("Two.")), ("Q3", None)]
        );
        assert!(understood([]).is_empty());
    }
}
