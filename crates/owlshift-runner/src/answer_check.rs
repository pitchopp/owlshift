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

use std::time::Duration;

use jiff::Timestamp;
use owlshift_adapters::tracker::{Comment, Person};
use owlshift_contracts::brief::{Brief, Relation};
use owlshift_contracts::refs::TicketQuestions;
use owlshift_contracts::result::{self, AnswerClass, Question, RunResult, Verdict};
use owlshift_core::reply::{self, Reply};
use owlshift_core::state::Event;

use crate::executor::Outcome;
use crate::on_demand::{comment_author, core_event};

/// Whether answers arrived: the newest last edit among the decider's
/// comments when it is later than the latest ask and than what the last
/// answer check to give its verdicts read
/// ([`TicketQuestions::answers_after`]), `None` otherwise. Only the tracker's
/// times are compared, never this machine's clock. A comment is the
/// decider's as the brief marks it: their account's, and not a marked
/// comment.
pub fn new_answer(
    comments: &[Comment],
    decider: &Person,
    asked: &TicketQuestions,
) -> Option<Timestamp> {
    let after = asked.answers_after()?;
    newest_decider_edit(comments, decider).filter(|newest| *newest > after)
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
/// comment of the same second as that edit is missed, as by [`new_answer`].
pub fn late_comments<'c>(
    read: &[Comment],
    now: &'c [Comment],
    decider: &Person,
) -> Vec<&'c Comment> {
    let seen = newest_decider_edit(read, decider);
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
    let last_edit = newest.last_edit();
    let quiet_for = Duration::try_from(now.duration_since(last_edit)).unwrap_or(Duration::ZERO);
    match reply::counts(quiet_for, reply::ends_with_go(&newest.body), window) {
        Reply::Counts => None,
        Reply::Settling => Some(last_edit.checked_add(window).unwrap_or(Timestamp::MAX)),
    }
}

/// The core event an answer check's outcome maps onto, and its result when
/// it left a valid one.
///
/// A `done` result folds its verdicts into one event
/// ([`Event::from_answer_check`]). Any other status is a failed run: the
/// role returns `done` or `failed`, and questions, a block or a false premise
/// mean nothing while the ticket waits for its decider. Every other outcome
/// maps as for any run ([`core_event`]): a usage limit is an interruption, a
/// run without a valid result a failed run, a breach a quarantine. A failed
/// check counts toward the ticket's failed runs, so a second one parks it.
pub fn event(outcome: &Outcome) -> (Event, Option<&RunResult>) {
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
            (vec![by("u1", 10, None, "Same second.")], None),
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
            r#"{{"format":5,"status":"done","summary":"s","verdicts":[{}]}}"#,
            verdicts.join(",")
        ))
    }

    #[test]
    fn a_done_check_folds_its_verdicts_and_anything_else_maps_as_a_run() {
        let event_of = |outcome: &Outcome| event(outcome).0;
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
                r#"{{"format":5,"status":"{status}","summary":"s"}}"#
            ));
            assert_eq!(event_of(&outcome), Event::RunFailed, "{status}");
        }
        let asking = finished(
            r#"{"format":5,"status":"questions","summary":"s","questions":[
                {"id":"Q1","category":"scope","context":"c","text":"t"}]}"#,
        );
        assert_eq!(event_of(&asking), Event::RunFailed);
        assert!(event(&asking).1.is_some(), "the result is still returned");

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
            r#"{"format":5,"status":"done","summary":"s","verdicts":[
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
            r#"{"format":5,"status":"done","summary":"s","verdicts":[
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
