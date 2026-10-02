//! The answer check (P2): what the runner does with a run of the
//! `answer_check` role (`roles/answer_check.md`), which classifies each
//! question of a ticket's latest ask once its decider has replied
//! (architecture section 4, scenario S2).
//!
//! It runs once answers arrive ([`new_answer`]). Its verdicts fold into one
//! core event ([`event`]): the ticket resumes, the questions left open are
//! asked again ([`open_questions`], posted as a RE-ASK comment,
//! [`crate::writer::ReaskComment`]), or the decider's counter-question waits
//! for a reply. `owlshift resume` runs it ([`crate::on_demand`]); the test
//! bench's stand-in driver plays the same pieces in the scenarios.

use jiff::Timestamp;
use owlshift_adapters::tracker::{Comment, Person};
use owlshift_contracts::brief::{Brief, Relation};
use owlshift_contracts::refs::TicketQuestions;
use owlshift_contracts::result::{self, AnswerClass, Question, RunResult, Verdict};
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
    comments
        .iter()
        .filter(|comment| comment_author(comment, decider).relation == Relation::Decider)
        .map(Comment::last_edit)
        .max()
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
            r#"{{"format":3,"status":"done","summary":"s","verdicts":[{}]}}"#,
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
                r#"{{"format":3,"status":"{status}","summary":"s"}}"#
            ));
            assert_eq!(event_of(&outcome), Event::RunFailed, "{status}");
        }
        let asking = finished(
            r#"{"format":3,"status":"questions","summary":"s","questions":[
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
    fn the_open_questions_are_the_latest_asks_partial_and_unanswered_ones() {
        let question = |id: &str| {
            format!(r#"{{"id":"{id}","category":"scope","context":"c {id}","text":"t {id}"}}"#)
        };
        let brief = |thread: &str| {
            Brief::parse(&format!(
                r#"{{"format":{BRIEF_FORMAT},"role":"answer_check","project":"p",
                    "ticket":{{"id":"T-1","title":"t","author":{{"name":"a","relation":"decider"}},"description":"d"}},
                    "decider":"a","thread":[{thread}],
                    "permissions":{{"level":"read_only","network":false,"browser":false}},
                    "gate":[],"result_path":"result.json"}}"#
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
            r#"{"format":3,"status":"done","summary":"s","verdicts":[
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
    }
}
