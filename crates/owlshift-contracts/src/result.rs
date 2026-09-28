//! `result.json`: what a role leaves for the runner at the end of a run.

use schemars::{JsonSchema, Schema};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::format::{self, ContractError, Format, RESULT_FORMAT};
use crate::ids::{QuestionId, RelativePath};

const CONTRACT: &str = "result.json";

/// The result of one run, written by the role as `result.json`.
///
/// Beyond this schema, the runner also requires question ids to be Q1, Q2, …
/// Qn in order.
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
        Ok(())
    }
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

/// Adds to the schema the two rules of [`RunResult::validate`] that JSON Schema
/// can express.
fn result_invariants(schema: &mut Schema) {
    let rules = json!([
        {
            "if": { "properties": { "status": { "const": "questions" } }, "required": ["status"] },
            "then": { "properties": { "questions": { "minItems": 1 } }, "required": ["questions"] }
        },
        {
            "if": { "properties": { "pr": { "type": "object" } }, "required": ["pr"] },
            "then": { "properties": { "status": { "const": "done" } } }
        }
    ]);
    schema.insert("allOf".to_owned(), rules);
}
