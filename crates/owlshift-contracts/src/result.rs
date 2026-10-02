//! `result.json`: what a role leaves for the runner at the end of a run.

use schemars::{JsonSchema, Schema};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::Role;
use crate::brief::Brief;
use crate::format::{self, ContractError, Format, RESULT_FORMAT};
use crate::ids::{QuestionId, RelativePath};

/// The answer check's classes live in the core, which folds them into one
/// state-machine event.
pub use owlshift_core::vocab::AnswerClass;

const CONTRACT: &str = "result.json";

/// The result of one run, written by the role as `result.json`.
///
/// Beyond this schema, the runner also requires question ids to be Q1, Q2, …
/// Qn in order; verdicts in increasing question order, each with a reason
/// that is not only whitespace and, on a counter-question and nowhere else,
/// a reply that is not either; and, against the run's brief, verdicts from the
/// answer check only, covering exactly its latest ask.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(title = "Owlshift result.json", transform = result_invariants)]
pub struct RunResult {
    pub format: Format<RESULT_FORMAT>,
    pub status: Status,
    /// One or two sentences on what the run did.
    pub summary: String,
    /// Questions for the decider, numbered Q1..Qn; at least one when `status`
    /// is `questions`.
    #[serde(default)]
    pub questions: Vec<Question>,
    /// Decisions taken without a human, each reversible and logged on the
    /// ticket.
    #[serde(default)]
    pub decisions: Vec<Decision>,
    /// Follow-up tickets proposed to a human; never admitted automatically.
    #[serde(default)]
    pub followups: Vec<Followup>,
    /// Paths, relative to the worktree, of the artifacts the run left.
    #[serde(default)]
    pub artifacts: Artifacts,
    /// The pull request to open; only with `status: done`.
    #[serde(default)]
    pub pr: Option<PullRequest>,
    /// The answer check's verdict on each question of the latest ask, in
    /// question order; only from the answer check, and only with `status:
    /// done`.
    #[serde(default)]
    pub verdicts: Vec<Verdict>,
}

/// How a run ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Done,
    Questions,
    Blocked,
    PremiseFalse,
    Failed,
}

/// A question for the decider, understandable without any transcript.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Question {
    pub id: QuestionId,
    /// What the question is about, such as `scope` or `security`.
    pub category: String,
    /// What the reader needs to know to answer.
    pub context: String,
    pub text: String,
    #[serde(default)]
    pub options: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recommendation: Option<String>,
}

/// The answer check's verdict on one question: how the decider's answers
/// left it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(transform = reply_with_counter_question_only)]
pub struct Verdict {
    pub question: QuestionId,
    pub class: AnswerClass,
    /// Why, from the answers: what settled the question, or what is still
    /// missing. Not empty and not only whitespace.
    #[schemars(regex(pattern = r"\S"))]
    pub reason: String,
    /// The answer to the decider's counter-question, which the runner posts
    /// in the thread as written. Required with `class: counter_question`,
    /// refused with any other class; not empty and not only whitespace.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present_string"
    )]
    #[schemars(with = "String", regex(pattern = r"\S"))]
    pub reply: Option<String>,
}

/// A field that may be left out but, when present, is a string: `null` is
/// refused, as the schema refuses it.
fn present_string<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    String::deserialize(deserializer).map(Some)
}

/// A decision the role took on its own.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Decision {
    pub question: String,
    pub decision: String,
    /// What the decision rests on: the ticket, the code, the docs, a fact.
    pub basis: String,
}

/// A follow-up ticket proposed to a human.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Followup {
    pub title: String,
    pub why: String,
    #[serde(default)]
    pub evidence: Vec<String>,
    pub source: FollowupSource,
    /// When the follow-up counts as done.
    pub done_when: String,
    /// Whether the follow-up waits for the current ticket to be done.
    #[serde(default)]
    pub blocked_by_parent: bool,
}

/// Who found the follow-up.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FollowupSource {
    Agent,
    Reviewer,
    Ci,
}

/// Artifacts left by the run, as paths relative to the worktree that cannot
/// leave it. Their content formats are defined by the roles that write them.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Artifacts {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<RelativePath>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ledger: Option<RelativePath>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub findings: Option<RelativePath>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report: Option<RelativePath>,
}

/// The pull request the runner opens for a finished ticket.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PullRequest {
    pub branch: String,
    pub title: String,
    pub body: String,
}

impl RunResult {
    /// Parses and validates `result.json`.
    pub fn parse(input: &str) -> Result<Self, ContractError> {
        let result: Self = format::parse_json(CONTRACT, RESULT_FORMAT, input)?;
        result.validate()?;
        Ok(result)
    }

    /// Renders `result.json`.
    pub fn render(&self) -> String {
        format::render_json(self)
    }

    /// Checks the rules the types alone do not carry.
    pub fn validate(&self) -> Result<(), ContractError> {
        check_question_order(CONTRACT, &self.questions)?;
        if self.status == Status::Questions && self.questions.is_empty() {
            return Err(ContractError::invalid(
                CONTRACT,
                "status is questions but no question is given",
            ));
        }
        if self.pr.is_some() && self.status != Status::Done {
            return Err(ContractError::invalid(
                CONTRACT,
                "pr is given but status is not done",
            ));
        }
        if !self.verdicts.is_empty() && self.status != Status::Done {
            return Err(ContractError::invalid(
                CONTRACT,
                "verdicts are given but status is not done",
            ));
        }
        if let Some(id) = first_not_ascending(self.verdicts.iter().map(|v| &v.question)) {
            return Err(ContractError::invalid(
                CONTRACT,
                format!("the verdict for {id} is repeated or out of order"),
            ));
        }
        if let Some(verdict) = self.verdicts.iter().find(|v| v.reason.trim().is_empty()) {
            return Err(ContractError::invalid(
                CONTRACT,
                format!("the verdict for {} has no reason", verdict.question),
            ));
        }
        for verdict in &self.verdicts {
            let counter_question = verdict.class == AnswerClass::CounterQuestion;
            match &verdict.reply {
                None if counter_question => {
                    return Err(ContractError::invalid(
                        CONTRACT,
                        format!(
                            "the verdict for {} is a counter-question without a reply",
                            verdict.question
                        ),
                    ));
                }
                Some(_) if !counter_question => {
                    return Err(ContractError::invalid(
                        CONTRACT,
                        format!(
                            "the verdict for {} has a reply but is not a counter-question",
                            verdict.question
                        ),
                    ));
                }
                Some(reply) if reply.trim().is_empty() => {
                    return Err(ContractError::invalid(
                        CONTRACT,
                        format!("the verdict for {} has a blank reply", verdict.question),
                    ));
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Checks the rules that need the brief of the run: verdicts come from
    /// the answer check only, and a `done` answer check gives one verdict for
    /// each question of the latest ask in the thread (its last `questions` or
    /// `reask` entry), no more. After a re-ask, that is the re-asked
    /// questions only.
    ///
    /// Expects both documents to have passed their own checks, as
    /// [`RunResult::parse`] and [`Brief::parse`] do: verdicts and asked
    /// questions are then both in increasing order, so the same ids make the
    /// same sequence.
    pub fn validate_against(&self, brief: &Brief) -> Result<(), ContractError> {
        if brief.role != Role::AnswerCheck {
            if !self.verdicts.is_empty() {
                return Err(ContractError::invalid(
                    CONTRACT,
                    format!(
                        "verdicts are given but the run's role is {}, not {}",
                        brief.role.as_str(),
                        Role::AnswerCheck.as_str()
                    ),
                ));
            }
            return Ok(());
        }
        if self.status != Status::Done {
            return Ok(());
        }
        let Some((round, asked)) = brief.latest_ask() else {
            return Err(ContractError::invalid(
                CONTRACT,
                "the answer check ran on a thread that asks no question",
            ));
        };
        if let Some(verdict) = self
            .verdicts
            .iter()
            .find(|v| !asked.iter().any(|q| q.id == v.question))
        {
            return Err(ContractError::invalid(
                CONTRACT,
                format!(
                    "the verdict for {} names a question the latest ask of round {round} did not ask",
                    verdict.question
                ),
            ));
        }
        if let Some(question) = asked
            .iter()
            .find(|q| !self.verdicts.iter().any(|v| v.question == q.id))
        {
            return Err(ContractError::invalid(
                CONTRACT,
                format!("no verdict for {} of round {round}", question.id),
            ));
        }
        Ok(())
    }
}

/// The first id not greater than the one before it: repeated or out of
/// order.
pub(crate) fn first_not_ascending<'a>(
    ids: impl IntoIterator<Item = &'a QuestionId>,
) -> Option<&'a QuestionId> {
    let mut previous: Option<&QuestionId> = None;
    for id in ids {
        if previous.is_some_and(|p| p.number() >= id.number()) {
            return Some(id);
        }
        previous = Some(id);
    }
    None
}

/// Requires question ids Q1..Qn in order.
pub(crate) fn check_question_order(
    contract: &'static str,
    questions: &[Question],
) -> Result<(), ContractError> {
    for (index, question) in questions.iter().enumerate() {
        let expected = QuestionId::nth(u32::try_from(index + 1).unwrap_or(u32::MAX));
        if question.id != expected {
            return Err(ContractError::invalid(
                contract,
                format!(
                    "question {} is out of order: expected {expected}",
                    question.id
                ),
            ));
        }
    }
    Ok(())
}

/// Adds to the schema the three rules of [`RunResult::validate`] that JSON
/// Schema can express.
fn result_invariants(schema: &mut Schema) {
    let rules = json!([
        {
            "if": { "properties": { "status": { "const": "questions" } }, "required": ["status"] },
            "then": { "properties": { "questions": { "minItems": 1 } }, "required": ["questions"] }
        },
        {
            "if": { "properties": { "pr": { "type": "object" } }, "required": ["pr"] },
            "then": { "properties": { "status": { "const": "done" } } }
        },
        {
            "if": { "properties": { "verdicts": { "minItems": 1 } }, "required": ["verdicts"] },
            "then": { "properties": { "status": { "const": "done" } } }
        }
    ]);
    schema.insert("allOf".to_owned(), rules);
}

/// Adds to the verdict's schema the rule of [`RunResult::validate`] on
/// `reply`: present with `class: counter_question`, absent otherwise.
fn reply_with_counter_question_only(schema: &mut Schema) {
    schema.insert(
        "if".to_owned(),
        json!({ "properties": { "class": { "const": "counter_question" } }, "required": ["class"] }),
    );
    schema.insert("then".to_owned(), json!({ "required": ["reply"] }));
    schema.insert(
        "else".to_owned(),
        json!({ "not": { "required": ["reply"] } }),
    );
}
