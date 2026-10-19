//! The resolver (P2, architecture section 4, "Resolver first"; scenario S7):
//! what the runner does with the questions a run raises before any of them
//! reaches the decider.
//!
//! Each question is routed by the project's gate policy ([`route`]): a
//! question of an always-human category, the floor's or one the project
//! adds, goes to the decider and never to the resolver, and so does the
//! plan-approval question under `plan_approval = "always"`. Since OWL-342
//! every other question every role raises goes to a run of the `resolver`
//! role (`roles/resolver.md`), read-only with Bash (OWL-344), whose brief
//! lists them in `resolve` beside the asking run's own output. Its outcome
//! ([`outcome`])
//! either decides or passes on each question, or, when the run gave no usable
//! result, sends them all to the decider ([`Fallback`]): no other harness can
//! take over yet. The resolver's brief lists its questions without their
//! category ([`unlabelled`]), and the resolver labels each one it decides
//! itself. Of its decisions, the runner keeps only those on questions it gave
//! the resolver whose two labels, the raising run's and the resolver's, both
//! route there (OWL-144), whose basis is of a kind the project's delegation
//! level allows and, for a `ticket` basis, quotes the ticket's text
//! ([`Quotable`], OWL-328), and, on a false premise's question, which takes
//! a re-scope under the level's `scope` rule and never the closure
//! ([`with_closure`], OWL-343) ([`settle`]), posts each as a DECISION comment
//! ([`crate::writer::DecisionComment`]) and keeps it in the ticket ref; the
//! decider gets the rest as a round ([`left_for_decider`]).
//! `owlshift do` and `owlshift continue` run it ([`crate::on_demand`]); the
//! test bench's stand-in driver plays the same pieces in the scenarios.

use std::time::Duration;

use owlshift_contracts::BasisKind;
use owlshift_contracts::brief::{Brief, Relation, ThreadEntry, ToResolve};
use owlshift_contracts::ids::{FindingId, QuestionId};
use owlshift_contracts::refs::KeptDecision;
use owlshift_contracts::result::{
    Decision, Question, QuestionOption, Resolution, Status, option_letter,
};
use owlshift_core::floor::FloorCategory;
use owlshift_core::gate::{ChainEnd, GatePolicy, Route};

use crate::executor::Outcome;
use crate::writer::{ReopenedChoice, flatten};

/// The executor's deadline for a resolver run, the ticket's
/// `resolver::DEADLINE` (OWL-344): longer than the answer check's, since the
/// resolver reads the code and its history with Bash and, at delegation
/// level `all`, weighs a trade-off on the deep tier.
pub const RESOLVER_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// The questions of a run, split by who settles them, each list in the run's
/// order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Routed {
    /// Always-human questions, and those the caller sends to the decider
    /// directly: the decider's alone.
    pub to_decider: Vec<Question>,
    /// The rest: the resolver's first.
    pub to_resolver: Vec<Question>,
}

/// Routes each question by the category the asking run filed it under
/// ([`GatePolicy::route`]): the level's always-human categories and the
/// project's additions go to the decider; every other, a blank one included,
/// to the resolver first (OWL-342). A category is all the runner reads, so
/// a question filed under the wrong one is routed by that one; [`settle`]
/// then checks the resolver's own label before any decision on it is
/// logged. The questions of `direct`, under the run's ids, go to the decider
/// whatever their category: the plan-approval question under
/// `plan_approval = "always"`, whose setting names a human.
pub fn route(policy: &GatePolicy, questions: &[Question], direct: &[QuestionId]) -> Routed {
    let mut routed = Routed::default();
    for question in questions {
        if direct.contains(&question.id) {
            routed.to_decider.push(question.clone());
            continue;
        }
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
/// not a copy of the one [`decisions`] checks it against. A residue
/// question names the finding it asks about (`findings`, by the run's
/// question ids, OWL-343).
pub fn unlabelled(questions: &[Question], findings: &[(QuestionId, FindingId)]) -> Vec<ToResolve> {
    questions
        .iter()
        .map(|question| ToResolve {
            finding: findings
                .iter()
                .find(|(asked, _)| *asked == question.id)
                .map(|(_, finding)| finding.clone()),
            ..ToResolve::from(question)
        })
        .collect()
}

/// The option the runner adds to every question of a false premise
/// (OWL-343): closing the ticket, the decider's act in the tracker, which
/// no resolver decides.
pub const CLOSURE: &str = "Close the ticket: its premise is false";

/// The questions of a `premise_false` result as the runner asks them, each
/// with the closure ([`CLOSURE`]) as its last option, so that the runner
/// knows that option's letter whatever the asking run wrote; a question that
/// already ends with it, kept from an earlier round, is left as it is.
pub fn with_closure(questions: &[Question]) -> Vec<Question> {
    questions
        .iter()
        .map(|question| {
            let mut asked = question.clone();
            if closure_letter(&asked).is_none() {
                asked.options.push(QuestionOption::new(CLOSURE));
            }
            asked
        })
        .collect()
}

/// The letter of a false premise's question's closure: its last option's,
/// when that option is [`CLOSURE`].
fn closure_letter(question: &Question) -> Option<String> {
    let last = question.options.len().checked_sub(1)?;
    (question.options[last].label == CLOSURE).then(|| option_letter(last))
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
    /// The resolver already settled every question of this many runs in a
    /// row, the project's pass limit (`policy.resolver_passes`, OWL-328).
    PassLimit(u32),
    /// The chain's resolver runs cost this much, in millionths of a dollar,
    /// reaching the project's ceiling, also given
    /// (`policy.resolver_budget_usd`, OWL-328; the resolver runs alone since
    /// OWL-376).
    Budget {
        spent_micros: u64,
        ceiling_micros: u64,
    },
    /// A resolver run of the chain reported no cost, so the project's
    /// ceiling could not be checked (OWL-337).
    CostUnreported,
}

impl Fallback {
    /// The name events give it: a chain ended by its pass limit or by its
    /// ceiling is a `pass_limit`, whose [`Fallback::reason`] says which.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UsageLimit => "usage_limit",
            Self::Failed => "failed",
            Self::PassLimit(_) | Self::Budget { .. } | Self::CostUnreported => "pass_limit",
        }
    }

    /// Why a chain ended at its ceiling, as the `gate` event's `reason`
    /// gives it: `budget` when its cost reached the ceiling,
    /// `cost_unreported` when a run's cost was not reported. `None` for
    /// every other fallback, a pass limit reached included.
    pub fn reason(self) -> Option<&'static str> {
        match self {
            Self::Budget { .. } => Some("budget"),
            Self::CostUnreported => Some("cost_unreported"),
            Self::UsageLimit | Self::Failed | Self::PassLimit(_) => None,
        }
    }
}

impl From<ChainEnd> for Fallback {
    fn from(end: ChainEnd) -> Self {
        match end {
            ChainEnd::Passes(limit) => Self::PassLimit(limit),
            ChainEnd::Budget {
                spent_micros,
                ceiling_micros,
            } => Self::Budget {
                spent_micros,
                ceiling_micros,
            },
            ChainEnd::CostUnreported => Self::CostUnreported,
        }
    }
}

/// The ticket's text a `ticket` basis may quote (OWL-337), as the
/// resolver's brief gave it: its title, its description and the thread's
/// comments of the decider, nobody else's. The runner looks for the quote
/// there, spaces collapsed, and refuses a decision whose quote it does not
/// find.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Quotable {
    texts: Vec<String>,
}

impl Quotable {
    /// What `brief` lets a `ticket` basis quote.
    pub fn of(brief: &Brief) -> Self {
        let mut texts = vec![
            flatten(&brief.ticket.title),
            flatten(&brief.ticket.description),
        ];
        texts.extend(brief.thread.iter().filter_map(|entry| match entry {
            ThreadEntry::Comment { author, body, .. } if author.relation == Relation::Decider => {
                Some(flatten(body))
            }
            _ => None,
        }));
        Self { texts }
    }

    /// Whether `quote`, spaces collapsed, is in one of the texts: not blank,
    /// and whole within a single text.
    pub fn holds(&self, quote: &str) -> bool {
        let quote = flatten(quote);
        !quote.is_empty() && self.texts.iter().any(|text| text.contains(&quote))
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

/// A decision the runner logs: its own copy of the question, the decision,
/// its basis and the basis's kind, and the option it takes, when it names
/// one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Logged<'a> {
    pub question: &'a Question,
    pub decision: &'a str,
    pub basis: &'a str,
    pub kind: BasisKind,
    pub option: Option<&'a str>,
}

/// What the runner makes of a resolver's resolutions, each list in question
/// order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Settled<'a> {
    /// The decisions it logs.
    pub logged: Vec<Logged<'a>>,
    /// The questions whose decision it refused, which go to the decider.
    pub refused: Vec<&'a QuestionId>,
}

/// Sorts the `decided` resolutions on questions of `given`, the questions
/// the runner gave the resolver, by [`GatePolicy::route_decided`] on the
/// category the raising run gave the question, the resolver's own label for
/// it (OWL-144) and the kind of its basis (OWL-328), and finds a `ticket`
/// basis's quote in `quotable`. A decision is logged only when both labels
/// route to the resolver, the label a token, the project's delegation level
/// allows the kind, a `judgment` at `all` alone (OWL-343), and a `ticket`
/// basis's quote is found. On the questions of a run that found the
/// premise false (`premise_false`), it must also take an option other than
/// the closure ([`with_closure`]), a re-scope, which the level's `scope`
/// rule holds as a decision on a `scope` question. Otherwise it is refused,
/// whatever the resolver decided, and its question goes to the decider. A
/// resolution on a question not in `given` is dropped, in neither list: the
/// executor already refuses a result that names a question its brief did not
/// give, and this keeps the floor independent of that check.
pub fn settle<'a>(
    policy: &GatePolicy,
    given: &'a [Question],
    resolutions: &'a [Resolution],
    quotable: &Quotable,
    premise_false: bool,
) -> Settled<'a> {
    let mut settled = Settled::default();
    for resolution in resolutions {
        let Resolution::Decided {
            question,
            category,
            decision,
            basis,
            basis_kind,
            quote,
            option,
        } = resolution
        else {
            continue;
        };
        let Some(asked) = given.iter().find(|asked| asked.id == *question) else {
            continue;
        };
        // The contract requires a quote with a `ticket` basis; a missing
        // one is refused here too, so the floor does not lean on it.
        let quoted = *basis_kind != BasisKind::Ticket
            || quote.as_deref().is_some_and(|quote| quotable.holds(quote));
        // A false premise's question: a re-scope only, held as `scope`.
        let rescope = !premise_false
            || (option
                .as_deref()
                .is_some_and(|letter| Some(letter) != closure_letter(asked).as_deref())
                && policy.route_decided(FloorCategory::Scope.token(), category, *basis_kind)
                    == Route::Resolver);
        match policy.route_decided(&asked.category, category, *basis_kind) {
            Route::Resolver if quoted && rescope => settled.logged.push(Logged {
                question: asked,
                decision: decision.as_str(),
                basis: basis.as_str(),
                kind: *basis_kind,
                option: option.as_deref(),
            }),
            Route::Resolver | Route::Human => settled.refused.push(&asked.id),
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
    quotable: &Quotable,
    premise_false: bool,
) -> Vec<Logged<'a>> {
    settle(policy, given, resolutions, quotable, premise_false).logged
}

/// The decisions the runner takes on the asking run's recommendations once
/// the chain reached its bound at `all` and no resolver runs (OWL-375), as
/// resolutions for [`settle`], which holds them to the rules it holds the
/// resolver's to: one `decided` resolution for each question of `given` (the
/// questions the resolver would have been given) that reopens no refused
/// choice of `reopened` and carries a recommendation, labelled with the
/// asking run's own category, so that [`GatePolicy::route_decided`] holds
/// that category to the level's set and to the token rule; its decision the
/// recommended option's label, or the recommendation's `why` without one,
/// its basis that `why`, its option the recommended letter, its kind
/// [`BasisKind::Recommendation`]. A recommendation naming no option the
/// question offers names none.
pub fn recommended(given: &[Question], reopened: &[ReopenedChoice]) -> Vec<Resolution> {
    given
        .iter()
        .filter(|question| !reopened.iter().any(|choice| choice.question == question.id))
        .filter_map(|question| {
            let recommendation = question.recommendation.as_ref()?;
            let chosen = question.recommended_option();
            Some(Resolution::Decided {
                question: question.id.clone(),
                category: question.category.clone(),
                decision: chosen.map_or_else(
                    || recommendation.why.clone(),
                    |index| question.options[index].label.clone(),
                ),
                basis: recommendation.why.clone(),
                basis_kind: BasisKind::Recommendation,
                quote: None,
                option: chosen.map(option_letter),
            })
        })
        .collect()
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

/// The refused choices a round reopens (OWL-203): those of `asked`, under
/// the run's question ids, whose question the round asks, under its id in
/// the round (`left`, from [`left_for_decider`]). Since OWL-342 the
/// resolver is given such a question as any other: one it decided is not in
/// the round, and so not here.
pub fn reopened_in(
    left: &[(QuestionId, Question)],
    asked: &[ReopenedChoice],
) -> Vec<ReopenedChoice> {
    asked
        .iter()
        .filter_map(|reopened| {
            let (_, question) = left
                .iter()
                .find(|(raised, _)| *raised == reopened.question)?;
            Some(ReopenedChoice {
                question: question.id.clone(),
                ..reopened.clone()
            })
        })
        .collect()
}

/// A kept decision as the delivery report lists it, among the decisions
/// taken without a human; one the decider reversed says so (OWL-146).
pub fn reported(kept: &KeptDecision) -> Decision {
    reported_reversing(kept, &[])
}

/// [`reported`], for the report of a correction: a decision whose question
/// is among `corrected`, the questions the correction follows the replies
/// to, says it was reversed although the ticket's ref marks it only once the
/// push has landed (OWL-233). Nothing is written: the mark keeps its timing.
/// A decision taken on judgment says so after its basis (OWL-343).
pub fn reported_reversing(kept: &KeptDecision, corrected: &[String]) -> Decision {
    let reversed = kept.reversed.is_some() || corrected.contains(&kept.question.text);
    Decision {
        question: kept.question.text.clone(),
        decision: if reversed {
            format!("{} (reversed by the decider's reply)", kept.decision)
        } else {
            kept.decision.clone()
        },
        basis: if kept.basis_kind == Some(BasisKind::Judgment) {
            format!("{} (decided on judgment)", kept.basis)
        } else {
            kept.basis.clone()
        },
    }
}

/// The kept decisions as the delivery report lists them
/// ([`reported_reversing`]), but those taken on a recommendation, which
/// [`reported_recommended`] lists under their own heading (OWL-375): those
/// taken on judgment first, so the decider sees which ones to read
/// (OWL-343), each group in the order kept.
pub fn reported_all(kept: &[KeptDecision], corrected: &[String]) -> Vec<Decision> {
    let judged = |decision: &&KeptDecision| decision.basis_kind == Some(BasisKind::Judgment);
    let others = |decision: &&KeptDecision| !judged(decision) && !recommended_kind(decision);
    kept.iter()
        .filter(judged)
        .chain(kept.iter().filter(others))
        .map(|decision| reported_reversing(decision, corrected))
        .collect()
}

/// The kept decisions taken on the asking run's recommendation at the
/// resolver's bound (OWL-375), as the delivery report lists them under
/// their own heading, in the order kept.
pub fn reported_recommended(kept: &[KeptDecision], corrected: &[String]) -> Vec<Decision> {
    kept.iter()
        .filter(recommended_kind)
        .map(|decision| reported_reversing(decision, corrected))
        .collect()
}

/// Whether a kept decision was taken on a recommendation (OWL-375).
fn recommended_kind(decision: &&KeptDecision) -> bool {
    decision.basis_kind == Some(BasisKind::Recommendation)
}

#[cfg(test)]
mod tests {
    use owlshift_contracts::DelegationLevel;
    use owlshift_contracts::format::{BRIEF_FORMAT, RESULT_FORMAT};
    use owlshift_contracts::result::{Recommendation, RunResult};
    use owlshift_core::gate::ResolverPolicy;
    use owlshift_core::vocab::PlanApproval;

    use super::*;
    use crate::artifact::ArtifactContents;
    use crate::executor::Failure;

    /// OWL-344: half an hour, which `OnDemand::resolve` gives the executor.
    #[test]
    fn a_resolver_run_has_half_an_hour() {
        assert_eq!(RESOLVER_TIMEOUT, Duration::from_secs(30 * 60));
    }

    fn question(id: &str, category: &str) -> Question {
        Question {
            id: QuestionId::new(id).unwrap(),
            category: category.into(),
            context: format!("context of {id}"),
            text: format!("text of {id}"),
            options: Vec::new(),
            recommendation: None,
            replan: false,
        }
    }

    fn ids(questions: &[Question]) -> Vec<&str> {
        questions.iter().map(|q| q.id.as_str()).collect()
    }

    fn decided(id: &str, category: &str, kind: BasisKind, option: Option<&str>) -> Resolution {
        Resolution::Decided {
            question: QuestionId::new(id).unwrap(),
            category: category.into(),
            decision: format!("decision on {id}"),
            basis: "the ticket".into(),
            basis_kind: kind,
            quote: None,
            option: option.map(str::to_owned),
        }
    }

    fn logged_ids(settled: &Settled<'_>) -> Vec<String> {
        settled
            .logged
            .iter()
            .map(|logged| logged.question.id.as_str().to_owned())
            .collect()
    }

    /// The floor's categories, in any spelling, and the project's additions
    /// never reach the resolver; a blank category does (OWL-342), and so does
    /// every other, but those the caller sends to the decider directly.
    #[test]
    fn always_human_and_direct_questions_never_reach_the_resolver() {
        let policy = GatePolicy::new(["billing"], PlanApproval::Never);
        let mut asked: Vec<Question> = FloorCategory::ALL
            .iter()
            .map(|floor| question("Q1", floor.token()))
            .collect();
        for category in ["Scope change", "risk of data loss", "Billing address"] {
            asked.push(question("Q1", category));
        }
        let routed = route(&policy, &asked, &[]);
        assert!(routed.to_resolver.is_empty(), "{:?}", routed.to_resolver);
        assert_eq!(routed.to_decider.len(), asked.len());

        let mixed = [
            question("Q1", "naming"),
            question("Q2", "security"),
            question("Q3", ""),
            question("Q4", "plan_approval"),
        ];
        let routed = route(&policy, &mixed, &[]);
        assert_eq!(ids(&routed.to_resolver), ["Q1", "Q3", "Q4"]);
        assert_eq!(ids(&routed.to_decider), ["Q2"]);
        let direct = [QuestionId::new("Q4").unwrap()];
        let routed = route(&policy, &mixed, &direct);
        assert_eq!(ids(&routed.to_resolver), ["Q1", "Q3"]);
        assert_eq!(ids(&routed.to_decider), ["Q2", "Q4"]);
    }

    /// A false premise's question offers the closure last, once (OWL-343);
    /// its decision stands only on another option, a re-scope, under the
    /// level's `scope` rule: refused below `ticket`, on a quote at
    /// `ticket`, on any basis at `all`; the closure, or no option, is
    /// refused at every level.
    #[test]
    fn a_false_premise_decides_a_rescope_under_the_scope_rule_never_the_closure() {
        let mut asked = question("Q1", "premise");
        asked.options = vec![QuestionOption::new("Re-scope: create GREETING.md.")];
        let given = with_closure(std::slice::from_ref(&asked));
        assert_eq!(given[0].options.len(), 2);
        assert_eq!(given[0].options[1].label, CLOSURE);
        assert_eq!(with_closure(&given), given, "added once");
        let at = |level| {
            GatePolicy::new(Vec::<String>::new(), PlanApproval::Never).with_resolver(
                ResolverPolicy {
                    level,
                    ..ResolverPolicy::default()
                },
            )
        };
        let text = quotable("Add GREETING.md saying Hello.");
        let quoted = Resolution::Decided {
            question: QuestionId::new("Q1").unwrap(),
            category: "scope".into(),
            decision: "Create GREETING.md.".into(),
            basis: "The ticket says so.".into(),
            basis_kind: BasisKind::Ticket,
            quote: Some("Add GREETING.md saying Hello.".into()),
            option: Some("A".into()),
        };
        let cases = [
            (
                DelegationLevel::Discoverable,
                decided("Q1", "scope", BasisKind::Settled, Some("A")),
                false,
            ),
            (
                DelegationLevel::Ticket,
                decided("Q1", "scope", BasisKind::Settled, Some("A")),
                false,
            ),
            (DelegationLevel::Ticket, quoted, true),
            (
                DelegationLevel::All,
                decided("Q1", "rescope", BasisKind::Judgment, Some("A")),
                true,
            ),
            (
                DelegationLevel::All,
                decided("Q1", "rescope", BasisKind::Judgment, Some("B")),
                false,
            ),
            (
                DelegationLevel::All,
                decided("Q1", "rescope", BasisKind::Judgment, None),
                false,
            ),
        ];
        for (level, resolution, stands) in cases {
            let resolutions = [resolution];
            let settled = settle(&at(level), &given, &resolutions, &text, true);
            assert_eq!(
                settled.logged.len() == 1,
                stands,
                "{level:?} {resolutions:?}"
            );
            assert_eq!(settled.refused.len() == 1, !stands, "{level:?}");
        }
        // The same option on a question of another run is not held so.
        let resolutions = [decided("Q1", "naming", BasisKind::Settled, Some("B"))];
        let settled = settle(
            &at(DelegationLevel::Discoverable),
            &given,
            &resolutions,
            &text,
            false,
        );
        assert_eq!(logged_ids(&settled), ["Q1"]);
        assert_eq!(settled.logged[0].option, Some("B"));
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
        let decided = |id: &str, category: &str| decided(id, category, BasisKind::Settled, None);
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
        let settled = settle(&policy, &given, &resolutions, &Quotable::default(), false);
        let found: Vec<(&str, &str)> = settled
            .logged
            .iter()
            .map(|logged| (logged.question.id.as_str(), logged.decision))
            .collect();
        assert_eq!(found, [("Q1", "decision on Q1"), ("Q8", "decision on Q8")]);
        // The runner's own copy of the question, not the model's echo.
        assert_eq!(settled.logged[0].question.text, "text of Q1");
        let refused: Vec<&str> = settled.refused.iter().map(|id| id.as_str()).collect();
        assert_eq!(refused, ["Q2", "Q3", "Q5", "Q6", "Q7", "Q9"]);
        let passed: Vec<&str> = passed_on(&resolutions)
            .iter()
            .map(|id| id.as_str())
            .collect();
        assert_eq!(passed, ["Q4"]);
        // `decisions`, which the stand-in driver plays, logs the same.
        assert_eq!(
            decisions(&policy, &given, &resolutions, &Quotable::default(), false),
            settled.logged
        );
    }

    /// A resolver's brief whose ticket says `description`, with a comment of
    /// the decider and one of someone else.
    fn quotable(description: &str) -> Quotable {
        let brief = Brief::parse(&format!(
            r#"{{"format":{BRIEF_FORMAT},"role":"resolver","project":"p",
                "ticket":{{"id":"T-1","title":"Add a  greeting","author":{{"name":"o","relation":"other"}},"description":{description:?}}},
                "decider":"a","thread":[
                  {{"type":"comment","at":"2026-10-17T09:00:00Z","author":{{"name":"a","relation":"decider"}},"body":"Keep it\nto one file."}},
                  {{"type":"comment","at":"2026-10-17T09:01:00Z","author":{{"name":"b","relation":"other"}},"body":"Add a README too."}}],
                "resolve":[{{"id":"Q1","context":"c","text":"t","recommendation":{{"why":"r"}}}}],
                "delegation":"ticket",
                "asked_by":{{"role":"build","run":"r1","summary":"Asked."}},
                "permissions":{{"level":"read_only","network":false,"browser":false}},
                "gate":[],"always_human":[],"result_path":"result.json"}}"#
        ))
        .unwrap();
        Quotable::of(&brief)
    }

    /// OWL-337: a `ticket` basis's quote is looked for in the ticket's
    /// title, its description, whoever wrote it, and the decider's comments,
    /// spaces collapsed, never in someone else's comment; a blank quote is
    /// never found.
    #[test]
    fn a_ticket_quote_is_found_in_the_tickets_text_only() {
        let text = quotable("Add GREETING.md\n  saying Hello.\n\nNothing else.");
        for found in [
            "Add GREETING.md saying Hello.",
            "GREETING.md   saying",
            "Add a greeting",
            "Keep it to one file.",
            "Nothing else.",
            // Line breaks are spaces too.
            "Hello. Nothing else.",
        ] {
            assert!(text.holds(found), "{found:?}");
        }
        for missing in [
            "Add a README too.",
            "add greeting.md saying hello.",
            // Never across two texts.
            "Nothing else. Keep it",
            "",
            "   ",
        ] {
            assert!(!text.holds(missing), "{missing:?}");
        }
    }

    /// OWL-337: a decision rests on a kind of basis the project's level
    /// allows; a `ticket` basis on a quote found in the ticket's text; at
    /// `ticket`, a `scope` question on a `ticket` basis only. Each other one
    /// is refused, its question going to the decider.
    #[test]
    fn a_decision_needs_its_levels_basis_and_a_found_quote() {
        let at = |level| {
            GatePolicy::new(Vec::<String>::new(), PlanApproval::Never).with_resolver(
                ResolverPolicy {
                    level,
                    ..ResolverPolicy::default()
                },
            )
        };
        let given = [
            question("Q1", "naming"),
            question("Q2", "naming"),
            question("Q3", "scope"),
            question("Q4", "scope"),
            question("Q5", "testing"),
        ];
        let decided = |id: &str, kind: BasisKind, quote: Option<&str>| Resolution::Decided {
            question: QuestionId::new(id).unwrap(),
            category: "naming".into(),
            decision: format!("decision on {id}"),
            basis: "the ticket".into(),
            basis_kind: kind,
            quote: quote.map(str::to_owned),
            option: None,
        };
        let resolutions = [
            decided("Q1", BasisKind::Fact, None),
            decided("Q2", BasisKind::Settled, None),
            decided(
                "Q3",
                BasisKind::Ticket,
                Some("Add GREETING.md saying Hello."),
            ),
            decided("Q4", BasisKind::Settled, None),
            decided("Q5", BasisKind::Ticket, Some("Add a README too.")),
        ];
        let text = quotable("Add GREETING.md saying Hello.");
        let sorted = |level| {
            let settled = settle(&at(level), &given, &resolutions, &text, false);
            let ids = |list: Vec<&QuestionId>| -> Vec<String> {
                list.iter().map(|id| id.as_str().to_owned()).collect()
            };
            (
                ids(settled.logged.iter().map(|l| &l.question.id).collect()),
                ids(settled.refused),
            )
        };
        // Below `ticket`, Q3 and Q4, filed `scope`, never reach the
        // resolver; were they decided, they are refused.
        assert_eq!(
            sorted(DelegationLevel::Facts),
            (
                vec!["Q1".to_owned()],
                ["Q2", "Q3", "Q4", "Q5"].map(String::from).to_vec()
            )
        );
        assert_eq!(
            sorted(DelegationLevel::Discoverable),
            (
                vec!["Q1".to_owned(), "Q2".to_owned()],
                ["Q3", "Q4", "Q5"].map(String::from).to_vec()
            )
        );
        // At `ticket`: the quoted scope question stands, the scope question
        // settled otherwise does not, nor a quote found nowhere but in
        // another person's comment.
        assert_eq!(
            sorted(DelegationLevel::Ticket),
            (
                ["Q1", "Q2", "Q3"].map(String::from).to_vec(),
                ["Q4", "Q5"].map(String::from).to_vec()
            )
        );
        assert_eq!(
            sorted(DelegationLevel::All),
            (
                ["Q1", "Q2", "Q3", "Q4"].map(String::from).to_vec(),
                vec!["Q5".to_owned()]
            )
        );
        assert_eq!(
            sorted(DelegationLevel::None),
            (
                Vec::new(),
                ["Q1", "Q2", "Q3", "Q4", "Q5"].map(String::from).to_vec()
            )
        );
    }

    /// A chain ended by its pass limit or its ceiling is a `pass_limit`,
    /// the ceiling's saying why.
    #[test]
    fn a_chain_end_is_a_pass_limit_with_its_reason() {
        let cases = [
            (Fallback::from(ChainEnd::Passes(3)), "pass_limit", None),
            (
                Fallback::from(ChainEnd::Budget {
                    spent_micros: 2,
                    ceiling_micros: 1,
                }),
                "pass_limit",
                Some("budget"),
            ),
            (
                Fallback::from(ChainEnd::CostUnreported),
                "pass_limit",
                Some("cost_unreported"),
            ),
            (Fallback::UsageLimit, "usage_limit", None),
            (Fallback::Failed, "failed", None),
        ];
        for (fallback, name, reason) in cases {
            assert_eq!(fallback.as_str(), name, "{fallback:?}");
            assert_eq!(fallback.reason(), reason, "{fallback:?}");
        }
        assert_eq!(Fallback::from(ChainEnd::Passes(3)), Fallback::PassLimit(3));
        assert_eq!(
            Fallback::from(ChainEnd::Budget {
                spent_micros: 2,
                ceiling_micros: 1
            }),
            Fallback::Budget {
                spent_micros: 2,
                ceiling_micros: 1
            }
        );
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
                    r#"{{"format":{RESULT_FORMAT},"status":"{status}","summary":"s"}}"#
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
                    r#"{{"format":{RESULT_FORMAT},"status":"{status}","summary":"s"}}"#
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

    /// The delivery report lists the decisions taken on judgment first,
    /// each saying so, then the others, each group in the order kept
    /// (OWL-343); a decision kept before its kind was is among the others.
    #[test]
    fn judgment_decisions_are_reported_first() {
        let kept = |n: u32, kind: Option<BasisKind>| KeptDecision {
            at: "2026-10-18T09:00:00Z".parse().unwrap(),
            comment: format!("c{n}"),
            question: question(&format!("Q{n}"), "naming"),
            decision: format!("decision {n}"),
            basis: format!("basis {n}"),
            basis_kind: kind,
            asked_by: None,
            number: None,
            reversed: None,
        };
        let decisions = [
            kept(1, Some(BasisKind::Settled)),
            kept(2, Some(BasisKind::Judgment)),
            kept(3, None),
            kept(4, Some(BasisKind::Judgment)),
        ];
        let reported = reported_all(&decisions, &[]);
        let order: Vec<&str> = reported.iter().map(|d| d.decision.as_str()).collect();
        assert_eq!(
            order,
            ["decision 2", "decision 4", "decision 1", "decision 3"]
        );
        assert_eq!(reported[0].basis, "basis 2 (decided on judgment)");
        assert_eq!(reported[2].basis, "basis 1");

        // Those taken on a recommendation have a heading of their own
        // (OWL-375).
        let mut decisions = decisions.to_vec();
        decisions.push(kept(5, Some(BasisKind::Recommendation)));
        let reported = reported_all(&decisions, &[]);
        assert!(reported.iter().all(|d| d.decision != "decision 5"));
        let recommended = reported_recommended(&decisions, &[]);
        let order: Vec<&str> = recommended.iter().map(|d| d.decision.as_str()).collect();
        assert_eq!(order, ["decision 5"]);
    }

    /// OWL-375: at the bound, a decision on each question's recommendation,
    /// the recommended option's label or the proposed answer, held by
    /// `settle` to the level's set and the token rule; a question reopening a
    /// refused choice or without a recommendation is left to the decider.
    #[test]
    fn recommendations_are_taken_as_settle_holds_them() {
        let recommend = |mut q: Question, option: Option<&str>, why: &str| {
            q.recommendation = Some(Recommendation {
                option: option.map(str::to_owned),
                why: why.to_owned(),
            });
            q
        };
        let mut naming = question("Q1", "naming");
        naming.options = vec![QuestionOption::new("Short"), QuestionOption::new("Long")];
        let given = vec![
            recommend(naming, Some("B"), "Long names read better."),
            recommend(question("Q2", "layout"), None, "One file."),
            question("Q3", "naming"),
            recommend(question("Q4", "billing"), None, "Free."),
            recommend(question("Q5", "file layout"), None, "Flat."),
            recommend(question("Q6", "naming"), None, "Reopened."),
        ];
        let reopened = [ReopenedChoice {
            place: std::num::NonZeroU32::MIN,
            question: QuestionId::new("Q6").unwrap(),
            choice: owlshift_contracts::result::RefusedChoice {
                question: "Which name?".into(),
                recorded: "short".into(),
            },
        }];
        let built = recommended(&given, &reopened);
        let made: Vec<&str> = built.iter().map(|r| r.question().as_str()).collect();
        assert_eq!(made, ["Q1", "Q2", "Q4", "Q5"]);
        let Resolution::Decided {
            decision,
            basis,
            basis_kind,
            option,
            category,
            ..
        } = &built[0]
        else {
            panic!("{:?}", built[0]);
        };
        assert_eq!(
            (
                decision.as_str(),
                basis.as_str(),
                *basis_kind,
                option.as_deref()
            ),
            (
                "Long",
                "Long names read better.",
                BasisKind::Recommendation,
                Some("B")
            )
        );
        assert_eq!(category, "naming");
        assert!(
            matches!(&built[1], Resolution::Decided { decision, option: None, .. } if decision == "One file.")
        );

        let at = |level| {
            GatePolicy::new(["billing"], PlanApproval::Never).with_resolver(ResolverPolicy {
                level,
                ..ResolverPolicy::default()
            })
        };
        let sorted = settle(
            &at(DelegationLevel::All),
            &given,
            &built,
            &Quotable::default(),
            false,
        );
        let logged: Vec<&str> = sorted
            .logged
            .iter()
            .map(|l| l.question.id.as_str())
            .collect();
        assert_eq!(logged, ["Q1", "Q2"]);
        let refused: Vec<&str> = sorted.refused.iter().map(|id| id.as_str()).collect();
        assert_eq!(
            refused,
            ["Q4", "Q5"],
            "the project's addition, a malformed category"
        );
        let below = settle(
            &at(DelegationLevel::Ticket),
            &given,
            &built,
            &Quotable::default(),
            false,
        );
        assert!(below.logged.is_empty(), "the kind is `all`'s alone");
    }
}
