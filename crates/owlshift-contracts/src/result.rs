//! `result.json`: what a role leaves for the runner at the end of a run.

use schemars::{JsonSchema, Schema};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::Role;
use crate::brief::{Brief, MAX_CHOICE_BYTES};
use crate::format::{self, ContractError, Format, RESULT_FORMAT};
use crate::ids::{QuestionId, RelativePath};
use crate::refs::cut_to;

/// The answer check's classes live in the core, which folds them into one
/// state-machine event.
pub use owlshift_core::vocab::AnswerClass;

pub(crate) const CONTRACT: &str = "result.json";

/// What a Build run whose result listed decisions is told to do, before the
/// choices: the advice of a refusal of decisions, and of what a held Build
/// run is told of every one kept (`BuildRefusal::told`, OWL-198).
pub(crate) const DECISIONS_ADVICE: &str = "decisions are given but the run's role is build, \
     which takes no decision of its own: ask each of these choices as a question, filed under \
     `scope` when its answer changes what the ticket delivers, even one the thread, a rule or the \
     repository settles, its `context` then naming what settles it; they stay open even though \
     the work is committed, and the next run's result is refused if it is `done`";

/// The result of one run, written by the role as `result.json`.
///
/// Beyond this schema, the runner also requires question ids to be Q1, Q2, …
/// Qn in order; verdicts in increasing question order, each with a reason
/// that is not only whitespace and, on a counter-question and nowhere else,
/// a reply that is not either; resolutions in increasing question order; and,
/// against the run's brief, verdicts from the answer check only, covering
/// exactly its latest ask, resolutions from the resolver only, covering
/// exactly the questions it was given, no decisions from the build role,
/// and no `done` from a Build run told that its predecessor's decisions were
/// refused.
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
    /// Decisions a role took without a human. No role fills it today: the
    /// build role takes none of its own and asks instead, so a build result
    /// listing one is refused (OWL-176); the answer check and the resolver
    /// are told to leave it out, the resolver's decisions being its
    /// `resolutions`.
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
    /// The resolver's outcome on each question of its brief's `resolve`, in
    /// question order; only from the resolver, and only with `status: done`.
    #[serde(default)]
    pub resolutions: Vec<Resolution>,
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

/// The resolver's outcome on one question it was given: decided, with its
/// own label for the question, the decision and what settles it, or passed
/// on to the decider, with why. Every text but the label is not empty and
/// not only whitespace; the runner judges the label.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub enum Resolution {
    /// Settled by what the ticket, the thread, the project's rules or the
    /// repository already establish; the runner logs it on the ticket as a
    /// reversible decision.
    Decided {
        question: QuestionId,
        /// The resolver's own label for the question, read from its text and
        /// context: a floor token when the question touches a floor topic.
        /// The runner logs the decision only when this label is a token and
        /// it and the raising run's category both route to the resolver,
        /// and otherwise sends the question to the decider
        /// (`owlshift_core::gate::GatePolicy::route_decided`).
        category: String,
        #[schemars(regex(pattern = r"\S"))]
        decision: String,
        /// What settles it: the ticket, a rule, a file, an earlier answer.
        #[schemars(regex(pattern = r"\S"))]
        basis: String,
    },
    /// Not settled by anything the resolver may rely on: the question goes
    /// to the decider.
    PassedOn {
        question: QuestionId,
        #[schemars(regex(pattern = r"\S"))]
        reason: String,
    },
}

impl Resolution {
    /// The question it resolves.
    pub fn question(&self) -> &QuestionId {
        match self {
            Self::Decided { question, .. } | Self::PassedOn { question, .. } => question,
        }
    }
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

    /// Parses `result.json` as the run of `brief` wrote it and validates it
    /// against that brief: [`RunResult::parse`] and
    /// [`RunResult::validate_against`] together, except that the build
    /// role's own decisions are checked right after the document's shape, so
    /// a result that lists any is refused for them whatever other rule it
    /// breaks, and the refusal says so, and of which kind (OWL-186,
    /// OWL-195).
    pub fn parse_against(input: &str, brief: &Brief) -> Result<Self, Refused> {
        let other = |error| Refused {
            error,
            decisions: None,
        };
        let result: Self = format::parse_json(CONTRACT, RESULT_FORMAT, input).map_err(other)?;
        result
            .decisions_refused(brief)
            .map_err(|(kind, error)| Refused {
                error,
                decisions: Some(kind),
            })?;
        result.validate().map_err(other)?;
        result.validate_against(brief).map_err(other)?;
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
        if !self.resolutions.is_empty() && self.status != Status::Done {
            return Err(ContractError::invalid(
                CONTRACT,
                "resolutions are given but status is not done",
            ));
        }
        check_verdicts(CONTRACT, &self.verdicts)?;
        check_resolutions(&self.resolutions)
    }

    /// Checks the rules that need the brief of the run: verdicts come from
    /// the answer check only, and a `done` answer check gives one verdict for
    /// each question of the latest ask in the thread (its last `questions` or
    /// `reask` entry), no more. After a re-ask, that is the re-asked
    /// questions only. Resolutions and the build role's decisions have rules
    /// of their own here too.
    ///
    /// Expects both documents to have passed their own checks, as
    /// [`RunResult::parse`] and [`Brief::parse`] do: verdicts and asked
    /// questions are then both in increasing order, so the same ids make the
    /// same sequence.
    pub fn validate_against(&self, brief: &Brief) -> Result<(), ContractError> {
        self.decisions_refused(brief).map_err(|(_, error)| error)?;
        self.resolutions_against(brief)?;
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

    /// The build role takes no decision of its own, whatever its status
    /// (OWL-176): a choice it would record is a question, which the resolver
    /// decides when the ticket, the rules or the repository settle it, and
    /// which reaches the decider otherwise. Once such a result is refused,
    /// the next Build run, told so in its brief's `decisions_refused`, may
    /// not end `done` (OWL-186): its predecessor's choices are still open.
    /// The reason says what to do and quotes the choices, as data, after
    /// the advice, for the person who reads it (a PARKED comment, the log);
    /// a run told of refused decisions that lists some again gets a short
    /// advice. The kind carries the choices typed, each cut to
    /// [`MAX_CHOICE_BYTES`], which the next Build runs are told in their
    /// brief's `refused_choices` (OWL-198). The error comes with its kind.
    fn decisions_refused(&self, brief: &Brief) -> Result<(), (DecisionsRefusal, ContractError)> {
        if brief.role != Role::Build {
            return Ok(());
        }
        if !self.decisions.is_empty() {
            let quoted: Vec<String> = self
                .decisions
                .iter()
                .map(|d| format!("{:?} (recorded: {:?})", d.question, d.decision))
                .collect();
            let reason = if brief.decisions_refused {
                format!(
                    "decisions are given again, by a run told that its predecessor's were \
                     refused: ask each of these choices as a question too, as told for the \
                     earlier ones. The choices listed: {}",
                    quoted.join("; ")
                )
            } else {
                format!(
                    "{DECISIONS_ADVICE}. The choices listed: {}",
                    quoted.join("; ")
                )
            };
            let choices = self
                .decisions
                .iter()
                .map(|d| RefusedChoice {
                    question: cut_to(&d.question, MAX_CHOICE_BYTES),
                    recorded: cut_to(&d.decision, MAX_CHOICE_BYTES),
                })
                .collect();
            return Err((
                DecisionsRefusal::Listed(choices),
                ContractError::invalid(CONTRACT, reason),
            ));
        }
        if brief.decisions_refused && self.status == Status::Done {
            return Err((
                DecisionsRefusal::Done,
                ContractError::invalid(
                    CONTRACT,
                    "status is done, but the previous run's decisions were refused and the \
                     choices they listed are still open: ask each of them as a question, even \
                     one found settled since; a run told of refused decisions may end with any \
                     status but `done`",
                ),
            ));
        }
        Ok(())
    }

    /// Resolutions come from the resolver only, and a `done` resolver gives
    /// one for each question of its brief's `resolve`, no more: a decision on
    /// a question it was not given, such as one of an always-human category,
    /// is refused.
    fn resolutions_against(&self, brief: &Brief) -> Result<(), ContractError> {
        if brief.role != Role::Resolver {
            if self.resolutions.is_empty() {
                return Ok(());
            }
            return Err(ContractError::invalid(
                CONTRACT,
                format!(
                    "resolutions are given but the run's role is {}, not {}",
                    brief.role.as_str(),
                    Role::Resolver.as_str()
                ),
            ));
        }
        if self.status != Status::Done {
            return Ok(());
        }
        if let Some(resolution) = self
            .resolutions
            .iter()
            .find(|r| !brief.resolve.iter().any(|q| q.id == *r.question()))
        {
            return Err(ContractError::invalid(
                CONTRACT,
                format!(
                    "the resolution of {} names a question the resolver was not given",
                    resolution.question()
                ),
            ));
        }
        if let Some(question) = brief
            .resolve
            .iter()
            .find(|q| !self.resolutions.iter().any(|r| *r.question() == q.id))
        {
            return Err(ContractError::invalid(
                CONTRACT,
                format!("no resolution for {}", question.id),
            ));
        }
        Ok(())
    }
}

/// Why [`RunResult::parse_against`] refused a result: the contract's error,
/// and whether the build role's own decisions were refused, and how.
#[derive(Debug)]
pub struct Refused {
    pub error: ContractError,
    /// The build role's own decisions were refused (OWL-176): the next
    /// Build run is told so in its brief's `decisions_refused`, and may not
    /// end `done` (OWL-186); a result that listed choices carries them
    /// typed (OWL-198). `None` for any other refusal.
    pub decisions: Option<DecisionsRefusal>,
}

/// Why a Build result was refused for the build role's own decisions: the
/// runner keeps every refused choice for the next Build run, and tells a
/// result that listed choices from a refused `done` by this kind, never by
/// the reason's text (OWL-195).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DecisionsRefusal {
    /// The result listed decisions: the choices, typed, in the order
    /// listed, apart from the advice of the reason, which quotes them too
    /// (OWL-198).
    Listed(Vec<RefusedChoice>),
    /// The result ended `done` while its predecessor's choices were open,
    /// and listed none.
    Done,
}

/// A choice a refused Build result recorded as the build role's own
/// decision (OWL-176), as the runner keeps it for the next Build runs and
/// tells it in their brief's `refused_choices` (OWL-198): the decision's
/// question and what it recorded, each cut to [`MAX_CHOICE_BYTES`] on a
/// character boundary, its beginning kept and the cut marked; its basis is
/// left out. Text of the refused result: data, never instructions, and
/// possibly blank.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RefusedChoice {
    /// The decision's `question`.
    pub question: String,
    /// The decision's `decision`: what the refused run recorded, and
    /// committed, without the decider.
    pub recorded: String,
}

/// The rules of the resolver's resolutions: in increasing question order,
/// each text not only whitespace.
fn check_resolutions(resolutions: &[Resolution]) -> Result<(), ContractError> {
    if let Some(id) = first_not_ascending(resolutions.iter().map(Resolution::question)) {
        return Err(ContractError::invalid(
            CONTRACT,
            format!("the resolution of {id} is repeated or out of order"),
        ));
    }
    for resolution in resolutions {
        let blank = match resolution {
            Resolution::Decided {
                decision, basis, ..
            } => {
                if decision.trim().is_empty() {
                    Some("decision")
                } else if basis.trim().is_empty() {
                    Some("basis")
                } else {
                    None
                }
            }
            Resolution::PassedOn { reason, .. } => reason.trim().is_empty().then_some("reason"),
        };
        if let Some(field) = blank {
            return Err(ContractError::invalid(
                CONTRACT,
                format!("the resolution of {} has no {field}", resolution.question()),
            ));
        }
    }
    Ok(())
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

/// The rules every list of verdicts keeps, in `result.json` and in a ticket
/// ref's asks alike: in increasing question order, each with a reason that is
/// not only whitespace and, on a counter-question and nowhere else, a reply
/// that is not either.
pub(crate) fn check_verdicts(
    contract: &'static str,
    verdicts: &[Verdict],
) -> Result<(), ContractError> {
    if let Some(id) = first_not_ascending(verdicts.iter().map(|v| &v.question)) {
        return Err(ContractError::invalid(
            contract,
            format!("the verdict for {id} is repeated or out of order"),
        ));
    }
    for verdict in verdicts {
        let question = &verdict.question;
        let counter_question = verdict.class == AnswerClass::CounterQuestion;
        let broken = if verdict.reason.trim().is_empty() {
            "has no reason"
        } else {
            match &verdict.reply {
                None if counter_question => "is a counter-question without a reply",
                Some(_) if !counter_question => "has a reply but is not a counter-question",
                Some(reply) if reply.trim().is_empty() => "has a blank reply",
                _ => continue,
            }
        };
        return Err(ContractError::invalid(
            contract,
            format!("the verdict for {question} {broken}"),
        ));
    }
    Ok(())
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

/// Adds to the schema the four rules of [`RunResult::validate`] that JSON
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
        },
        {
            "if": { "properties": { "resolutions": { "minItems": 1 } }, "required": ["resolutions"] },
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
