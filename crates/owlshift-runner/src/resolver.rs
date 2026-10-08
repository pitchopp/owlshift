//! The resolver (P2, architecture section 4, "Resolver first"; scenario S7):
//! what the runner does with the questions a run raises before any of them
//! reaches the decider.
//!
//! Each question is routed by the project's gate policy ([`route`]): a
//! question of an always-human category, the floor's or one the project
//! adds, goes to the decider and never to the resolver. The rest go to a run
//! of the `resolver` role (`roles/resolver.md`), read-only and without
//! network, whose brief lists them in `resolve`. Its outcome ([`outcome`])
//! either decides or passes on each question, or, when the run gave no usable
//! result, sends them all to the decider ([`Fallback`]): no other harness can
//! take over yet. The resolver's brief lists its questions without their
//! category ([`unlabelled`]), and the resolver labels each one it decides
//! itself. Of its decisions, the runner keeps only those on questions it gave
//! the resolver whose two labels, the raising run's and the resolver's, both
//! route there ([`settle`], OWL-144), posts each as a DECISION comment
//! ([`crate::writer::DecisionComment`]) and keeps it in the ticket ref; the
//! decider gets the rest as a round ([`left_for_decider`]).
//! `owlshift do` and `owlshift continue` run it ([`crate::on_demand`]); the
//! test bench's stand-in driver plays the same pieces in the scenarios.

use std::time::Duration;

use owlshift_contracts::brief::ToResolve;
use owlshift_contracts::ids::QuestionId;
use owlshift_contracts::refs::KeptDecision;
use owlshift_contracts::result::{Decision, Question, Resolution, Status};
use owlshift_core::gate::{GatePolicy, Route};

use crate::executor::Outcome;

/// The executor's deadline for a resolver run: a short role, like the answer
/// check, that reads what it needs and decides.
pub const RESOLVER_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// The questions of a run, split by who settles them, each list in the run's
/// order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Routed {
    /// Always-human questions: the decider's alone.
    pub to_decider: Vec<Question>,
    /// The rest: the resolver's first.
    pub to_resolver: Vec<Question>,
}

/// Routes each question by its category ([`GatePolicy::route`]). The floor's
/// categories, a blank one and the project's additions go to the decider; a
/// category is all the runner reads, so a question filed under the wrong one
/// is routed by that one; [`settle`] then checks the resolver's own label
/// before any decision on it is logged.
pub fn route(policy: &GatePolicy, questions: &[Question]) -> Routed {
    let mut routed = Routed::default();
    for question in questions {
        match policy.route(&question.category) {
            Route::Human => routed.to_decider.push(question.clone()),
            Route::Resolver => routed.to_resolver.push(question.clone()),
        }
    }
    routed
}

/// The questions as the resolver's brief lists them, without the category
/// the raising run gave them (OWL-144): the resolver labels each question it
/// decides from its text and context, so its label is its own reading and
/// not a copy of the one [`decisions`] checks it against.
pub fn unlabelled(questions: &[Question]) -> Vec<ToResolve> {
    questions.iter().map(ToResolve::from).collect()
}

/// Why the questions meant for the resolver went to the decider; the
/// QUESTIONS comment says so (architecture section 9: a fallback is logged on
/// the ticket).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Fallback {
    /// The resolver's harness reached its usage limit.
    UsageLimit,
    /// The resolver run failed, or left no valid result.
    Failed,
    /// The resolver already settled every question of
    /// [`owlshift_core::state::MAX_RESOLVED_PASSES`] runs in a row.
    PassLimit,
}

impl Fallback {
    /// The name events give it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UsageLimit => "usage_limit",
            Self::Failed => "failed",
            Self::PassLimit => "pass_limit",
        }
    }
}

/// What a resolver run left.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resolved<'a> {
    /// A `done` result, checked against its brief by the executor: one
    /// resolution for each question the resolver was given.
    Done(&'a [Resolution]),
    /// Nothing usable: every question goes to the decider.
    Fallback(Fallback),
    /// The run broke isolation: the floor quarantines it.
    Quarantined,
}

/// What a resolver run's outcome leaves. The role returns `done` or
/// `failed`; any other status, a run without a valid result, or a deadline
/// reached is a failed run, and a usage limit is the harness's.
pub fn outcome(outcome: &Outcome) -> Resolved<'_> {
    match outcome {
        Outcome::Finished { result, .. } if result.status == Status::Done => {
            Resolved::Done(&result.resolutions)
        }
        Outcome::Finished { .. } | Outcome::Failed(_) => Resolved::Fallback(Fallback::Failed),
        Outcome::UsageLimit { .. } => Resolved::Fallback(Fallback::UsageLimit),
        Outcome::Quarantined(_) => Resolved::Quarantined,
    }
}

/// What the ticket ref keeps of the resolver's refusals after a resolver
/// run that did not break isolation, given what it kept before (OWL-191),
/// for `owlshift do`, `continue` and the scenarios' stand-in driver alike:
/// a refused result ([`crate::on_demand::result_refusal`]) takes the place
/// of the kept reason; an accepted `done` clears it; any other outcome (a
/// usage limit, a run that failed with nothing told, an accepted `failed`)
/// leaves it, so the next resolver run is told the same. A round kept
/// clears it too, which the callers do with the round's ask.
pub fn refusal_after(kept: Option<&str>, ran: &Outcome) -> Option<String> {
    if let Some(reason) = crate::on_demand::result_refusal(ran) {
        return Some(reason);
    }
    match outcome(ran) {
        Resolved::Done(_) => None,
        Resolved::Fallback(_) | Resolved::Quarantined => kept.map(str::to_owned),
    }
}

/// What the runner makes of a resolver's resolutions, each list in question
/// order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Settled<'a> {
    /// The decisions it logs: the runner's own copy of the question, the
    /// decision and its basis.
    pub logged: Vec<(&'a Question, &'a str, &'a str)>,
    /// The questions whose decision it refused, which go to the decider.
    pub refused: Vec<&'a QuestionId>,
}

/// Sorts the `decided` resolutions on questions of `given`, the questions
/// the runner gave the resolver, by [`GatePolicy::route_decided`] on the
/// category the raising run gave the question and the resolver's own label
/// for it (OWL-144). A decision is logged only when both route to the
/// resolver, the label a token; otherwise it is refused, whatever the
/// resolver decided, and its question goes to the decider. A resolution on
/// a question not in `given` is dropped, in neither list: the executor
/// already refuses a result that names a question its brief did not give,
/// and this keeps the floor independent of that check.
pub fn settle<'a>(
    policy: &GatePolicy,
    given: &'a [Question],
    resolutions: &'a [Resolution],
) -> Settled<'a> {
    let mut settled = Settled::default();
    for resolution in resolutions {
        let Resolution::Decided {
            question,
            category,
            decision,
            basis,
        } = resolution
        else {
            continue;
        };
        let Some(asked) = given.iter().find(|asked| asked.id == *question) else {
            continue;
        };
        match policy.route_decided(&asked.category, category) {
            Route::Resolver => settled
                .logged
                .push((asked, decision.as_str(), basis.as_str())),
            Route::Human => settled.refused.push(&asked.id),
        }
    }
    settled
}

/// The decisions the runner logs, in question order, as [`settle`] sorts
/// them.
pub fn decisions<'a>(
    policy: &GatePolicy,
    given: &'a [Question],
    resolutions: &'a [Resolution],
) -> Vec<(&'a Question, &'a str, &'a str)> {
    settle(policy, given, resolutions).logged
}

/// The ids of the questions passed on, in question order.
pub fn passed_on(resolutions: &[Resolution]) -> Vec<&QuestionId> {
    resolutions
        .iter()
        .filter(|resolution| matches!(resolution, Resolution::PassedOn { .. }))
        .map(Resolution::question)
        .collect()
}

/// The round the decider gets: every question the run raised but those
/// decided and logged, in the run's order, numbered Q1..Qn again as a round
/// must be, each with the id the run gave it.
pub fn left_for_decider(
    raised: &[Question],
    decided: &[QuestionId],
) -> Vec<(QuestionId, Question)> {
    raised
        .iter()
        .filter(|question| !decided.contains(&question.id))
        .enumerate()
        .map(|(index, question)| {
            let number = u32::try_from(index + 1).unwrap_or(u32::MAX);
            let renumbered = Question {
                id: QuestionId::nth(number),
                ..question.clone()
            };
            (question.id.clone(), renumbered)
        })
        .collect()
}

/// A kept decision as the delivery report lists it, among the decisions
/// taken without a human.
pub fn reported(kept: &KeptDecision) -> Decision {
    Decision {
        question: kept.question.text.clone(),
        decision: kept.decision.clone(),
        basis: kept.basis.clone(),
    }
}

#[cfg(test)]
mod tests {
    use owlshift_contracts::result::RunResult;
    use owlshift_core::floor::FloorCategory;
    use owlshift_core::vocab::PlanApproval;

    use super::*;
    use crate::artifact::ArtifactContents;
    use crate::executor::Failure;

    fn question(id: &str, category: &str) -> Question {
        Question {
            id: QuestionId::new(id).unwrap(),
            category: category.into(),
            context: format!("context of {id}"),
            text: format!("text of {id}"),
            options: Vec::new(),
            recommendation: None,
        }
    }

    fn ids(questions: &[Question]) -> Vec<&str> {
        questions.iter().map(|q| q.id.as_str()).collect()
    }

    /// The floor's categories, in any spelling, a blank one and the
    /// project's additions never reach the resolver.
    #[test]
    fn always_human_questions_never_reach_the_resolver() {
        let policy = GatePolicy::new(["billing"], PlanApproval::Never);
        let mut asked: Vec<Question> = FloorCategory::ALL
            .iter()
            .map(|floor| question("Q1", floor.token()))
            .collect();
        for category in [
            "Scope change",
            "risk of data loss",
            "",
            " ",
            "Billing address",
        ] {
            asked.push(question("Q1", category));
        }
        let routed = route(&policy, &asked);
        assert!(routed.to_resolver.is_empty(), "{:?}", routed.to_resolver);
        assert_eq!(routed.to_decider.len(), asked.len());

        let mixed = [
            question("Q1", "naming"),
            question("Q2", "security"),
            question("Q3", "testing"),
        ];
        let routed = route(&policy, &mixed);
        assert_eq!(ids(&routed.to_resolver), ["Q1", "Q3"]);
        assert_eq!(ids(&routed.to_decider), ["Q2"]);
    }

    /// A decision is logged only when Build's category and the resolver's
    /// own label both route to the resolver, the label a token (OWL-144);
    /// any other decision on a question the resolver was given is refused.
    /// Even a result deciding an always-human question, which the executor
    /// refuses already, logs nothing on it, and one on a question it was not
    /// given is dropped.
    #[test]
    fn a_decision_is_logged_only_on_a_question_the_resolver_may_decide() {
        let policy = GatePolicy::new(["billing"], PlanApproval::Never);
        let given = [
            question("Q1", "naming"),
            question("Q2", "money"),
            question("Q3", "billing"),
            question("Q4", "testing"),
            question("Q5", "cleanup"),
            question("Q6", "cleanup"),
            question("Q7", "cleanup"),
            question("Q8", "naming"),
            question("Q9", "cleanup"),
        ];
        let decided = |id: &str, category: &str| Resolution::Decided {
            question: QuestionId::new(id).unwrap(),
            category: category.into(),
            decision: format!("decision on {id}"),
            basis: "the ticket".into(),
        };
        let resolutions = [
            decided("Q1", "naming"),
            decided("Q2", "naming"),
            decided("Q3", "naming"),
            Resolution::PassedOn {
                question: QuestionId::new("Q4").unwrap(),
                reason: "a matter of taste".into(),
            },
            // The resolver's label names the floor, a spelling the matcher
            // would misread, or nothing.
            decided("Q5", "risk_of_data_loss"),
            decided("Q6", "dataLoss"),
            decided("Q7", ""),
            // A label that merely differs from Build's.
            decided("Q8", "file_layout"),
            decided("Q9", "billing"),
            decided("Q10", "naming"),
        ];
        let settled = settle(&policy, &given, &resolutions);
        let found: Vec<(&str, &str)> = settled
            .logged
            .iter()
            .map(|(question, decision, _)| (question.id.as_str(), *decision))
            .collect();
        assert_eq!(found, [("Q1", "decision on Q1"), ("Q8", "decision on Q8")]);
        // The runner's own copy of the question, not the model's echo.
        assert_eq!(settled.logged[0].0.text, "text of Q1");
        let refused: Vec<&str> = settled.refused.iter().map(|id| id.as_str()).collect();
        assert_eq!(refused, ["Q2", "Q3", "Q5", "Q6", "Q7", "Q9"]);
        let passed: Vec<&str> = passed_on(&resolutions)
            .iter()
            .map(|id| id.as_str())
            .collect();
        assert_eq!(passed, ["Q4"]);
        // `decisions`, which the stand-in driver plays, logs the same.
        assert_eq!(decisions(&policy, &given, &resolutions), settled.logged);
    }

    #[test]
    fn the_decider_gets_what_is_left_renumbered_in_the_runs_order() {
        let raised = [
            question("Q1", "naming"),
            question("Q2", "scope"),
            question("Q3", "testing"),
            question("Q4", "money"),
        ];
        let decided = [
            QuestionId::new("Q1").unwrap(),
            QuestionId::new("Q3").unwrap(),
        ];
        let left = left_for_decider(&raised, &decided);
        let pairs: Vec<(&str, &str, &str)> = left
            .iter()
            .map(|(from, q)| (from.as_str(), q.id.as_str(), q.text.as_str()))
            .collect();
        assert_eq!(
            pairs,
            [("Q2", "Q1", "text of Q2"), ("Q4", "Q2", "text of Q4")]
        );
        assert!(left_for_decider(&raised[..1], &decided[..1]).is_empty());
        // Nothing decided: the round is the run's own questions.
        let all = left_for_decider(&raised, &[]);
        assert!(all.iter().all(|(from, q)| *from == q.id));
    }

    /// A run that gives no usable result sends every question to the
    /// decider, a usage limit named as such; a breach is a quarantine.
    #[test]
    fn a_resolver_without_a_usable_result_falls_back_to_the_decider() {
        let finished = |status: &str| Outcome::Finished {
            result: Box::new(
                RunResult::parse(&format!(
                    r#"{{"format":7,"status":"{status}","summary":"s"}}"#
                ))
                .unwrap(),
            ),
            artifacts: ArtifactContents::default(),
        };
        assert_eq!(outcome(&finished("done")), Resolved::Done(&[]));
        for status in ["failed", "blocked", "premise_false"] {
            assert_eq!(
                outcome(&finished(status)),
                Resolved::Fallback(Fallback::Failed),
                "{status}"
            );
        }
        for failure in [Failure::NoResult, Failure::TimedOut] {
            assert_eq!(
                outcome(&Outcome::Failed(failure)),
                Resolved::Fallback(Fallback::Failed)
            );
        }
        assert_eq!(
            outcome(&Outcome::UsageLimit { resets_at: None }),
            Resolved::Fallback(Fallback::UsageLimit)
        );
        assert_eq!(
            outcome(&Outcome::Quarantined(Vec::new())),
            Resolved::Quarantined
        );
    }

    /// OWL-191: a refused result is kept for the next resolver run, in the
    /// place of any earlier one; an accepted `done` clears it; every other
    /// outcome, an accepted `failed` included, leaves it.
    #[test]
    fn a_refused_resolver_result_is_kept_until_one_is_accepted() {
        let finished = |status: &str| Outcome::Finished {
            result: Box::new(
                RunResult::parse(&format!(
                    r#"{{"format":7,"status":"{status}","summary":"s"}}"#
                ))
                .unwrap(),
            ),
            artifacts: ArtifactContents::default(),
        };
        let refused = Outcome::Failed(Failure::InvalidResult("resolution on Q2".to_owned()));
        for kept in [None, Some("an earlier refusal")] {
            assert_eq!(
                refusal_after(kept, &refused).as_deref(),
                Some("resolution on Q2")
            );
            assert_eq!(refusal_after(kept, &finished("done")), None);
            for left in [
                finished("failed"),
                Outcome::Failed(Failure::NoResult),
                Outcome::Failed(Failure::TimedOut),
                Outcome::UsageLimit { resets_at: None },
            ] {
                assert_eq!(refusal_after(kept, &left).as_deref(), kept, "{left:?}");
            }
        }
    }
}
