//! `result.json`: what a role leaves for the runner at the end of a run.

use std::num::NonZeroU32;

use schemars::{JsonSchema, Schema};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::brief::{Brief, MAX_CHOICE_BYTES};
use crate::format::{self, ContractError, Format, RESULT_FORMAT};
use crate::ids::{QuestionId, RelativePath, TicketId};
use crate::refs::cut_to;
use crate::review::{
    Finding, FindingResponse, FindingsMode, MAX_FINDINGS, Plan, Ruling, check_answers,
    check_findings,
};
use crate::{BasisKind, Role, Variant};

/// The answer check's classes live in the core, which folds them into one
/// state-machine event.
pub use owlshift_core::vocab::AnswerClass;
/// The routing verdict and its effort levels live in the core, which
/// resolves each run's routing from them (OWL-301).
pub use owlshift_core::vocab::{Effort, Routing};

pub(crate) const CONTRACT: &str = "result.json";

/// How much of an unasked choice's question a refusal quotes (OWL-193):
/// the run has every choice whole in its brief's `refused_choices`, so the
/// quote only helps a person reading the reason, and comes after the
/// instruction and the places, which the reason's cut keeps.
const UNASKED_QUOTE_BYTES: usize = 80;

/// What a Build run whose result listed decisions is told to do, before the
/// choices: the advice of a refusal of decisions, and of what a held Build
/// run is told of every one kept (`BuildRefusal::told`, OWL-198).
pub(crate) const DECISIONS_ADVICE: &str = "decisions are given but the run's role is build, \
     which takes no decision of its own: ask each of these choices as a question, filed under \
     `scope` when its answer changes what the ticket delivers, even one the thread, a rule or the \
     repository settles, its `context` then naming what settles it; they stay open even though \
     the work is committed, and the next run's result is refused if it is `done`";

/// The most bytes one question weighs (OWL-213), rendered as JSON, so that
/// escaping counts: a quote takes two bytes, a control character up to six.
/// A question's text, context and options take about 1 KB in practice
/// (build-plan.md, "`owlshift forget TICKET`"); this leaves room for the
/// two 512-byte texts of a refused choice ([`MAX_CHOICE_BYTES`]) in one
/// question, even escaped at their worst.
pub const MAX_QUESTION_BYTES: usize = 8 * 1024;

/// The most bytes the questions of one result weigh together (OWL-213),
/// rendered as JSON like [`MAX_QUESTION_BYTES`]: room for a retry that
/// restates the 64 choices of a full hold ([`crate::brief::MAX_REFUSED_CHOICES`]) with
/// twice the room it needs, and an eighth of the ticket ref's file cap, so
/// an ask and its re-asks cannot fill it alone.
pub const MAX_QUESTIONS_BYTES: usize = 128 * 1024;

/// The most questions one result asks (OWL-213): a hold of
/// [`crate::brief::MAX_REFUSED_CHOICES`] choices needs one question at least, as one
/// question may cover several choices. The count bounds the JSON syntax a
/// byte bound cannot see, such as the indentation of a thousand empty
/// questions.
pub const MAX_QUESTIONS: usize = 64;

/// The result of one run, written by the role as `result.json`.
///
/// Beyond this schema, the runner also requires question ids to be Q1, Q2, …
/// Qn in order; on each question, options with a label that is not blank
/// and a recommendation whose `why` is not blank and which names, by its
/// capital letter, an option the question offers, or none when it offers
/// none (OWL-324); verdicts in increasing question order, each with a reason
/// that is not only whitespace and, on a counter-question and nowhere else,
/// a reply that is not either; resolutions in increasing question order; and,
/// against the run's brief, verdicts from the answer check only, covering
/// exactly its latest ask, resolutions from the resolver only, covering
/// exactly the questions it was given, no decisions from the build role,
/// no `done` from a Build run told that its predecessor's decisions were
/// refused, from such a run when it asks, every refused choice of its
/// brief named in `refused_choices_asked` (OWL-193), `intake` from the
/// intake role only, which gives it whenever it ends `done` or `questions`
/// (OWL-245), and the quality pipeline's fields from their roles only
/// (OWL-300, [`RunResult::validate_against`]).
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
    /// For each choice of the brief's `refused_choices`, by its place from
    /// 1, the question of this result that asks it (OWL-193): every place,
    /// in order, when a Build run told of refused choices asks. Only in a
    /// result that asks ([`RunResult::asks`]), and only when the brief gives
    /// refused choices.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub refused_choices_asked: Vec<RefusedChoiceAsked>,
    /// The intake role's record (format 8, OWL-245): required from it when
    /// it ends `done` or `questions`, refused with any other status and from
    /// any other role.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intake: Option<IntakeVerdict>,
    /// The design role's plan (format 9, OWL-300): with `done`, from a
    /// design run not answering an approval round, or beside `approval:
    /// changed`; refused from any other role and with any other status.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<Plan>,
    /// A design run's answer to an approval round (format 9): `approved`
    /// leaves the plan as it was, `changed` gives the revised `plan`
    /// beside it. Required with `done` from a design run whose brief has
    /// `approval`, refused otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval: Option<ApprovalOutcome>,
    /// A review run's findings, F1..Fn in order, at most
    /// [`crate::review::MAX_FINDINGS`] (format 9): only from the design
    /// review and verify roles, and only with `done`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = crate::review::MAX_FINDINGS))]
    pub findings: Vec<Finding>,
    /// The arbiter's ruling on each finding of its brief, in finding order
    /// (format 9): only from the arbiter, and only with `done`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rulings: Vec<Ruling>,
    /// The author's answer to each finding of its brief, in finding order
    /// (format 9): only from a design or build run whose brief gives
    /// findings to answer, or since format 13 to fix (OWL-377), and only
    /// with `done`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub finding_responses: Vec<FindingResponse>,
}

/// A design run's answer to an approval round (format 9, OWL-300).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalOutcome {
    /// The decider approved the plan as it was.
    Approved,
    /// The decider's answer changes the plan: the result's `plan` is the
    /// revised one.
    Changed,
}

/// The most dependencies one intake record names (OWL-245): the record is
/// kept in the ticket ref, whose files are read up to 1 MiB.
pub const MAX_DEPENDENCIES: usize = 64;

/// The most bytes a dependency's quote weighs (OWL-245): the words of the
/// ticket that name the dependency, not a passage.
pub const MAX_QUOTE_BYTES: usize = 512;

/// What the intake role found on a ticket (architecture section 5,
/// "Intake"): its pipeline variant, its routing verdict and the tickets it
/// depends on. In P4 the verdict is recorded and shown, and the Build runs
/// as `do`'s does; per-ticket routing follows it from P5.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IntakeVerdict {
    pub variant: Variant,
    pub routing: Routing,
    /// The tickets this one depends on, each named by its identifier in the
    /// ticket's text or thread; at most [`MAX_DEPENDENCIES`]. A dependency
    /// named without an identifier is a question, never a guess.
    #[serde(default)]
    #[schemars(length(max = MAX_DEPENDENCIES))]
    pub dependencies: Vec<IntakeDependency>,
}

/// A ticket the intake role found this one depends on: its identifier and
/// the words of the ticket that name it, not blank, on one line and at most
/// [`MAX_QUOTE_BYTES`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct IntakeDependency {
    pub ticket: TicketId,
    #[schemars(regex(pattern = r"\S"))]
    pub quote: String,
}

/// The rules of an intake record's dependencies, in `result.json` and in
/// `intake.json` alike: at most [`MAX_DEPENDENCIES`], each quote not blank,
/// without a control character and at most [`MAX_QUOTE_BYTES`].
pub(crate) fn check_dependencies<'a>(
    contract: &'static str,
    dependencies: impl ExactSizeIterator<Item = (&'a TicketId, &'a str)>,
) -> Result<(), ContractError> {
    if dependencies.len() > MAX_DEPENDENCIES {
        return Err(ContractError::invalid(
            contract,
            format!(
                "{} dependencies are named, past the {MAX_DEPENDENCIES} an intake record holds",
                dependencies.len()
            ),
        ));
    }
    for (ticket, quote) in dependencies {
        if let Err(broken) = check_quote(quote, "name the dependency") {
            return Err(ContractError::invalid(
                contract,
                format!("the quote of dependency {ticket} {broken}"),
            ));
        }
    }
    Ok(())
}

/// The rules of a quote from a ticket, an intake dependency's (OWL-245) or
/// a resolver's `ticket` basis (OWL-337) alike: not blank, without a
/// control character and at most [`MAX_QUOTE_BYTES`]. `Err` says what is
/// broken, `words` naming what the quote's words are for.
fn check_quote(quote: &str, words: &str) -> Result<(), String> {
    if quote.trim().is_empty() {
        Err("is blank".to_owned())
    } else if quote.chars().any(char::is_control) {
        Err("holds a control character: quote the words on one line".to_owned())
    } else if quote.len() > MAX_QUOTE_BYTES {
        Err(format!(
            "weighs {} bytes, past the {MAX_QUOTE_BYTES} a quote may weigh: quote only the words \
             that {words}",
            quote.len()
        ))
    } else {
        Ok(())
    }
}

/// Where a Build run told of refused choices asks one of them (OWL-193):
/// the choice's place in its brief's `refused_choices`, from 1, and the id
/// of the question of the same result that asks it. Several choices may
/// name one question.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RefusedChoiceAsked {
    pub choice: NonZeroU32,
    pub question: QuestionId,
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
    /// What the question is about, such as `scope` or `security`: what the
    /// runner routes it by. No comment shows it (OWL-324).
    pub category: String,
    /// What the reader needs to know to answer: at most two sentences, not
    /// restating the ticket.
    pub context: String,
    /// The question itself, on one line.
    pub text: String,
    /// The answers to choose from, which the runner letters A, B, C… in
    /// this order (format 10, OWL-324: [`option_letter`]).
    #[serde(default)]
    pub options: Vec<QuestionOption>,
    /// The answer the asking run recommends: required in `result.json`
    /// (format 10, OWL-324), where the runner refuses a question without
    /// one; absent only from a question a ticket ref kept before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recommendation: Option<Recommendation>,
    /// The Build cannot follow its plan, and asks how (format 9, OWL-300):
    /// once answered or decided, the ticket goes back to Design. Only on a
    /// build run's question.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub replan: bool,
}

/// One answer a question offers (format 10, OWL-324): a short label, and
/// at most one line of detail when the label needs it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct QuestionOption {
    /// A few words, not blank.
    #[schemars(regex(pattern = r"\S"))]
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl QuestionOption {
    /// An option with a label alone.
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            detail: None,
        }
    }
}

/// The answer the asking run recommends (format 10, OWL-324): with
/// options, the letter of one of them and why; without, the proposed
/// answer itself as `why`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Recommendation {
    /// The letter of the recommended option, as the runner letters them
    /// ([`option_letter`]): required when the question has options, refused
    /// when it has none. A recommendation a ticket ref kept before format
    /// 10 has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(regex(pattern = r"^[A-Z]+$"))]
    pub option: Option<String>,
    /// Why that option, or, without options, the proposed answer; not
    /// blank.
    #[schemars(regex(pattern = r"\S"))]
    pub why: String,
}

/// The letter of the option at `index` from 0, as the runner shows it: A to
/// Z, then AA, AB… as spreadsheet columns go.
pub fn option_letter(index: usize) -> String {
    let mut letters = Vec::new();
    let mut n = index + 1;
    while n > 0 {
        let rem = (n - 1) % 26;
        letters.push(char::from(b'A' + u8::try_from(rem).expect("below 26")));
        n = (n - 1) / 26;
    }
    letters.iter().rev().collect()
}

/// The place, from 0, of the option of `options` lettered `letter`
/// ([`option_letter`]), compared as written: a lowercase letter names none.
pub fn option_index(options: &[QuestionOption], letter: &str) -> Option<usize> {
    (0..options.len()).find(|&index| option_letter(index) == letter)
}

impl Question {
    /// The place, from 0, of the option its recommendation names, when it
    /// names one that exists.
    pub fn recommended_option(&self) -> Option<usize> {
        let letter = self.recommendation.as_ref()?.option.as_deref()?;
        option_index(&self.options, letter)
    }
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
/// own label for the question, the decision, what settles it and the kind
/// of that basis, or passed on to the decider, with why. Every text but the
/// label is not empty and not only whitespace; a `ticket` basis has a quote,
/// and no other kind has one; an `option`, when given, names an option of its
/// question; the runner judges the label, the kind against the project's
/// delegation level and the quote against the ticket.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub enum Resolution {
    /// Settled by what the ticket, the thread, the project's rules or the
    /// repository already establish, or at `all` by the resolver's own
    /// judgment (OWL-343); the runner logs it on the ticket as a reversible
    /// decision.
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
        /// The kind of that basis (format 11, OWL-337): `fact`, `settled`,
        /// `ticket` or, since format 12, `judgment` (OWL-343), its `basis`
        /// then holding the reasons and the cost of the choice. The runner
        /// refuses a kind the project's delegation level does not allow.
        basis_kind: BasisKind,
        /// The letter of the question's option the decision takes, as the
        /// runner letters them ([`option_letter`]), when it takes one
        /// (format 12, OWL-343); refused when it names none of the options
        /// the brief gave the question. The runner reads it where the option
        /// decides what follows: the approval of a plan, and the re-scope of
        /// a false premise, whose closure, the runner's own last option, it
        /// refuses.
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "present_string"
        )]
        #[schemars(with = "String", regex(pattern = r"^[A-Z]+$"))]
        option: Option<String>,
        /// With a `ticket` basis only, and then required: the words of the
        /// ticket's title, description or a comment of its decider that
        /// settle the question, on one line and at most
        /// [`MAX_QUOTE_BYTES`]. The runner refuses a decision whose quote it
        /// does not find there.
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "present_string"
        )]
        #[schemars(with = "String", regex(pattern = r"\S"))]
        quote: Option<String>,
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

    /// The option letter a decision takes, when it names one.
    pub fn option(&self) -> Option<&str> {
        match self {
            Self::Decided { option, .. } => option.as_deref(),
            Self::PassedOn { .. } => None,
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
        check_options(&self.questions)?;
        check_questions_size(&self.questions)?;
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
        if !self.refused_choices_asked.is_empty() && !self.asks() {
            return Err(ContractError::invalid(
                CONTRACT,
                "refused choices are named as asked but the result asks no question",
            ));
        }
        if let Some(named) = self
            .refused_choices_asked
            .iter()
            .find(|named| !self.questions.iter().any(|q| q.id == named.question))
        {
            return Err(ContractError::invalid(
                CONTRACT,
                format!(
                    "refused choice {} is named as asked by {}, which the result does not ask",
                    named.choice, named.question
                ),
            ));
        }
        if self.intake.is_some() && !matches!(self.status, Status::Done | Status::Questions) {
            return Err(ContractError::invalid(
                CONTRACT,
                "intake is given but status is neither done nor questions",
            ));
        }
        if let Some(intake) = &self.intake {
            check_dependencies(
                CONTRACT,
                intake
                    .dependencies
                    .iter()
                    .map(|d| (&d.ticket, d.quote.as_str())),
            )?;
        }
        self.check_quality_pipeline()?;
        check_verdicts(CONTRACT, &self.verdicts)?;
        check_resolutions(&self.resolutions)
    }

    /// The rules of the quality pipeline's fields (format 9) that need no
    /// brief: each only with `done`; `approval: changed` with a plan beside
    /// it and `approved` without one; the plan's own rules; findings F1..Fn
    /// within their bounds; rulings and responses in finding order, each
    /// with a reason.
    fn check_quality_pipeline(&self) -> Result<(), ContractError> {
        let given = [
            ("plan", self.plan.is_some()),
            ("approval", self.approval.is_some()),
            ("findings", !self.findings.is_empty()),
            ("rulings", !self.rulings.is_empty()),
            ("finding_responses", !self.finding_responses.is_empty()),
        ];
        if self.status != Status::Done
            && let Some((field, _)) = given.iter().find(|(_, given)| *given)
        {
            return Err(ContractError::invalid(
                CONTRACT,
                format!("{field} is given but status is not done"),
            ));
        }
        match (self.approval, &self.plan) {
            (Some(ApprovalOutcome::Changed), None) => {
                return Err(ContractError::invalid(
                    CONTRACT,
                    "approval is changed but no revised plan is given",
                ));
            }
            (Some(ApprovalOutcome::Approved), Some(_)) => {
                return Err(ContractError::invalid(
                    CONTRACT,
                    "approval is approved but a plan is given: an approved plan stays as it was",
                ));
            }
            _ => {}
        }
        if let Some(plan) = &self.plan {
            plan.check(CONTRACT)?;
        }
        check_findings(CONTRACT, &self.findings, MAX_FINDINGS, true)?;
        check_answers(
            CONTRACT,
            "ruling",
            self.rulings.iter().map(|r| (&r.finding, r.reason.as_str())),
        )?;
        check_answers(
            CONTRACT,
            "response",
            self.finding_responses
                .iter()
                .map(|r| (&r.finding, r.reason.as_str())),
        )
    }

    /// Whether the result asks: `questions`, or `premise_false` with at
    /// least one question. Only such a result clears a hold of refused
    /// decisions (OWL-192, OWL-193), and only such a result names where it
    /// asks the refused choices.
    pub fn asks(&self) -> bool {
        matches!(self.status, Status::Questions | Status::PremiseFalse)
            && !self.questions.is_empty()
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
        self.refused_choices_against(brief)?;
        self.resolutions_against(brief)?;
        self.intake_against(brief)?;
        self.quality_pipeline_against(brief)?;
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

    /// A Build run told of refused choices, which may not end `done`
    /// (OWL-186), names, when it asks, the question that asks each of them
    /// (OWL-193): every place of its brief's `refused_choices`, from 1, in
    /// order, so a retry that asks some and silently drops others is
    /// refused. The runner checks what the run declares, not that the
    /// question named truly asks the choice. A result names none when its
    /// brief gives no refused choice: a hold an older ticket ref kept as
    /// text has no place to name. The reason gives the instruction and the
    /// places first, so the cut of a kept reason keeps them.
    fn refused_choices_against(&self, brief: &Brief) -> Result<(), ContractError> {
        let refused = &brief.refused_choices;
        if refused.is_empty() {
            if self.refused_choices_asked.is_empty() {
                return Ok(());
            }
            return Err(ContractError::invalid(
                CONTRACT,
                "refused choices are named as asked but the brief gives none",
            ));
        }
        if !self.asks() {
            return Ok(());
        }
        let named: Vec<usize> = self
            .refused_choices_asked
            .iter()
            .map(|n| usize::try_from(n.choice.get()).unwrap_or(usize::MAX))
            .collect();
        if named.iter().copied().eq(1..=refused.len()) {
            return Ok(());
        }
        let list = |places: &[usize]| {
            places
                .iter()
                .map(usize::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        };
        let unasked: Vec<usize> = (1..=refused.len())
            .filter(|place| !named.contains(place))
            .collect();
        let stray: Vec<usize> = named
            .iter()
            .enumerate()
            .filter(|&(i, &place)| place > refused.len() || named[..i].iter().any(|&p| p >= place))
            .map(|(_, &place)| place)
            .collect();
        let mut reason = String::from(
            "the refused choices are not all named as asked: a run told of refused decisions \
             asks every choice of `refused_choices`, and names in `refused_choices_asked`, for \
             each in order, its place from 1 and the question that asks it",
        );
        if !unasked.is_empty() {
            reason.push_str(&format!(". Not named: {}", list(&unasked)));
        }
        if !stray.is_empty() {
            reason.push_str(&format!(
                ". Repeated, out of order or past the {} given: {}",
                refused.len(),
                list(&stray)
            ));
        }
        if !unasked.is_empty() {
            let quoted: Vec<String> = unasked
                .iter()
                .map(|&place| {
                    let question = &refused[place - 1].question;
                    format!("{place} {:?}", cut_to(question, UNASKED_QUOTE_BYTES))
                })
                .collect();
            reason.push_str(&format!(". The choices not named: {}", quoted.join("; ")));
        }
        Err(ContractError::invalid(CONTRACT, reason))
    }

    /// The intake record comes from the intake role only, which gives it
    /// whenever it ends `done` or `questions` (OWL-245).
    fn intake_against(&self, brief: &Brief) -> Result<(), ContractError> {
        let ends_with_record = matches!(self.status, Status::Done | Status::Questions);
        match (brief.role, &self.intake) {
            (Role::Intake, None) if ends_with_record => Err(ContractError::invalid(
                CONTRACT,
                "status is done or questions but no intake is given: the intake role records \
                 the variant, the routing and the dependencies it found",
            )),
            (role, Some(_)) if role != Role::Intake => Err(ContractError::invalid(
                CONTRACT,
                format!(
                    "intake is given but the run's role is {}, not {}",
                    role.as_str(),
                    Role::Intake.as_str()
                ),
            )),
            _ => Ok(()),
        }
    }

    /// The rules of format 9 that need the brief (OWL-300): `premise_false`
    /// from the intake, design and build roles only; a question's `replan`
    /// from the build role only; the plan and the approval from the design
    /// role only, a `done` design run giving `approval` exactly when its
    /// brief has `approval` and a plan otherwise; findings from the design
    /// review and verify roles only; rulings from the arbiter only, one for
    /// each finding of its brief when `done`; responses from a design or
    /// build run whose brief gives findings to answer or to fix only, one
    /// for each when `done`, each with an outcome its brief's mode and the
    /// finding allow ([`crate::review::ResponseOutcome::refused`], format 13, OWL-377).
    fn quality_pipeline_against(&self, brief: &Brief) -> Result<(), ContractError> {
        let role = brief.role;
        let refused = |field: &str, roles: &str| {
            Err(ContractError::invalid(
                CONTRACT,
                format!(
                    "{field} is given but the run's role is {}, not {roles}",
                    role.as_str()
                ),
            ))
        };
        if self.status == Status::PremiseFalse
            && !matches!(role, Role::Intake | Role::Design | Role::Build)
        {
            return refused("premise_false", "intake, design or build");
        }
        if role != Role::Build
            && let Some(question) = self.questions.iter().find(|q| q.replan)
        {
            return refused(&format!("replan on {}", question.id), "build");
        }
        if role != Role::Design {
            if self.plan.is_some() {
                return refused("plan", "design");
            }
            if self.approval.is_some() {
                return refused("approval", "design");
            }
        } else if self.status == Status::Done {
            match (brief.approval, self.approval, &self.plan) {
                (true, None, _) => {
                    return Err(ContractError::invalid(
                        CONTRACT,
                        "status is done but no approval is given: a design run after an \
                         approval round answers it, approved or changed",
                    ));
                }
                (false, Some(_), _) => {
                    return Err(ContractError::invalid(
                        CONTRACT,
                        "approval is given but the brief asks for no approval",
                    ));
                }
                (false, None, None) => {
                    return Err(ContractError::invalid(
                        CONTRACT,
                        "status is done but no plan is given: the design role returns its plan",
                    ));
                }
                _ => {}
            }
        }
        if !self.findings.is_empty() && !matches!(role, Role::DesignReview | Role::Verify) {
            return refused("findings", "design_review or verify");
        }
        let given: Vec<_> = brief
            .findings
            .iter()
            .flat_map(|given| given.findings.iter().map(|f| &f.id))
            .collect();
        let covers = |answered: Vec<&crate::ids::FindingId>, what: &str| {
            if answered == given {
                Ok(())
            } else {
                Err(ContractError::invalid(
                    CONTRACT,
                    format!(
                        "the {what} name {} but the brief gives {}: give one for each finding, \
                         in order",
                        ids(&answered),
                        ids(&given)
                    ),
                ))
            }
        };
        if role != Role::Arbiter {
            if !self.rulings.is_empty() {
                return refused("rulings", "arbiter");
            }
        } else if self.status == Status::Done {
            covers(self.rulings.iter().map(|r| &r.finding).collect(), "rulings")?;
        }
        let answered = brief.findings.as_ref().filter(|given| {
            matches!(role, Role::Design | Role::Build)
                && matches!(given.mode, FindingsMode::Answer | FindingsMode::Fix)
        });
        let Some(given) = answered else {
            if !self.finding_responses.is_empty() {
                return Err(ContractError::invalid(
                    CONTRACT,
                    format!(
                        "finding_responses are given but the run's brief gives no finding to \
                         answer or fix (role {})",
                        role.as_str()
                    ),
                ));
            }
            return Ok(());
        };
        if self.status == Status::Done {
            covers(
                self.finding_responses.iter().map(|r| &r.finding).collect(),
                "finding_responses",
            )?;
        }
        for response in &self.finding_responses {
            let refused = given
                .findings
                .iter()
                .find(|finding| finding.id == response.finding)
                .and_then(|finding| response.outcome.refused(given.mode, finding));
            if let Some(refused) = refused {
                return Err(ContractError::invalid(
                    CONTRACT,
                    format!(
                        "finding_responses answers {} {}: {refused}",
                        response.finding,
                        serde_json::to_value(response.outcome)
                            .ok()
                            .and_then(|v| v.as_str().map(str::to_owned))
                            .unwrap_or_default()
                    ),
                ));
            }
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
        // An option letter names an option the brief gave its question
        // (OWL-343).
        for resolution in &self.resolutions {
            let Some(letter) = resolution.option() else {
                continue;
            };
            let given = brief
                .resolve
                .iter()
                .find(|q| q.id == *resolution.question())
                .is_some_and(|q| option_index(&q.options, letter).is_some());
            if !given {
                return Err(ContractError::invalid(
                    CONTRACT,
                    format!(
                        "the resolution of {} takes option {letter}, which its question does not \
                         offer",
                        resolution.question()
                    ),
                ));
            }
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
/// each text not only whitespace; an option written as a letter (OWL-343);
/// a quote with a `ticket` basis, keeping a quote's rules ([`check_quote`]),
/// and none with another kind (OWL-337).
fn check_resolutions(resolutions: &[Resolution]) -> Result<(), ContractError> {
    if let Some(id) = first_not_ascending(resolutions.iter().map(Resolution::question)) {
        return Err(ContractError::invalid(
            CONTRACT,
            format!("the resolution of {id} is repeated or out of order"),
        ));
    }
    for resolution in resolutions {
        let id = resolution.question();
        let broken = match resolution {
            Resolution::Decided {
                decision,
                basis,
                basis_kind,
                quote,
                option,
                ..
            } => {
                let letters = |letter: &str| {
                    !letter.is_empty() && letter.bytes().all(|b| b.is_ascii_uppercase())
                };
                if decision.trim().is_empty() {
                    Some("has no decision".to_owned())
                } else if basis.trim().is_empty() {
                    Some("has no basis".to_owned())
                } else if let Some(option) = option.as_deref().filter(|option| !letters(option)) {
                    Some(format!(
                        "takes option {option:?}, which is not an option's letter: A, B, C…"
                    ))
                } else {
                    match (basis_kind, quote) {
                        (BasisKind::Ticket, None) => Some(
                            "has a `ticket` basis but no quote: quote the ticket's words that \
                             settle it"
                                .to_owned(),
                        ),
                        (BasisKind::Ticket, Some(quote)) => check_quote(quote, "settle it")
                            .err()
                            .map(|broken| format!("has a quote that {broken}")),
                        (kind, Some(_)) => Some(format!(
                            "has a quote but a `{}` basis: only a `ticket` basis quotes",
                            kind.as_str()
                        )),
                        (_, None) => None,
                    }
                }
            }
            Resolution::PassedOn { reason, .. } => {
                reason.trim().is_empty().then(|| "has no reason".to_owned())
            }
        };
        if let Some(broken) = broken {
            return Err(ContractError::invalid(
                CONTRACT,
                format!("the resolution of {id} {broken}"),
            ));
        }
    }
    Ok(())
}

/// Finding ids as a refusal names them: `F1, F2`, or `none`.
fn ids(ids: &[&crate::ids::FindingId]) -> String {
    if ids.is_empty() {
        return "none".to_owned();
    }
    ids.iter()
        .map(|id| id.as_str())
        .collect::<Vec<_>>()
        .join(", ")
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

/// The questions of a result are copied whole into the ticket ref, which
/// refuses a file past its read cap, and `owlshift forget` cannot make room
/// for a result that asks more than the cap holds: a result over the bounds
/// is refused, and its retry is told why (OWL-213). Build's questions are
/// the only ones kept; a refusal of the decisions the build role listed
/// comes first, whatever the size.
fn check_questions_size(questions: &[Question]) -> Result<(), ContractError> {
    if questions.len() > MAX_QUESTIONS {
        return Err(ContractError::invalid(
            CONTRACT,
            format!(
                "{} questions are asked, past the {MAX_QUESTIONS} a result may ask: ask fewer \
                 questions, one question may cover several choices or points",
                questions.len()
            ),
        ));
    }
    if let Some(question) = questions
        .iter()
        .find(|q| format::render_json(q).len() > MAX_QUESTION_BYTES)
    {
        return Err(ContractError::invalid(
            CONTRACT,
            format!(
                "question {} weighs {} bytes rendered as JSON, past the {MAX_QUESTION_BYTES} a \
                 question may weigh: shorten its context, text and options, or split it",
                question.id,
                format::render_json(question).len()
            ),
        ));
    }
    let total = format::render_json(&questions).len();
    if total > MAX_QUESTIONS_BYTES {
        return Err(ContractError::invalid(
            CONTRACT,
            format!(
                "the questions weigh {total} bytes rendered as JSON, past the \
                 {MAX_QUESTIONS_BYTES} a result's questions may weigh together: ask shorter \
                 questions, or paraphrase the refused choices, as one question may cover \
                 several"
            ),
        ));
    }
    Ok(())
}

/// Every question of a result offers options with a label and recommends
/// an answer (format 10, OWL-324): with options, the capital letter of one
/// of them, as the runner letters them; without, the proposed answer alone.
/// Each reason names the question and what to write, for the retry.
fn check_options(questions: &[Question]) -> Result<(), ContractError> {
    for question in questions {
        let id = &question.id;
        let refused = |why: String| Err(ContractError::invalid(CONTRACT, format!("{id} {why}")));
        if let Some(n) = question
            .options
            .iter()
            .position(|o| o.label.trim().is_empty())
        {
            return refused(format!(
                "has a blank label on option {}: give each option a short label",
                option_letter(n)
            ));
        }
        let Some(recommendation) = &question.recommendation else {
            return refused(
                "has no recommendation: give every question one, the letter of the option you \
                 recommend and why, or your proposed answer as `why` when it has no options"
                    .to_owned(),
            );
        };
        if recommendation.why.trim().is_empty() {
            return refused("has a recommendation with a blank `why`".to_owned());
        }
        match (&recommendation.option, question.options.is_empty()) {
            (None, false) => {
                return refused(format!(
                    "recommends no option: name one by its capital letter, A to {}, in the \
                     recommendation's `option`",
                    option_letter(question.options.len() - 1)
                ));
            }
            (Some(letter), true) => {
                return refused(format!(
                    "recommends option {letter:?} but offers no options: leave `option` out and \
                     give the proposed answer as `why`"
                ));
            }
            (Some(letter), false) if option_index(&question.options, letter).is_none() => {
                return refused(format!(
                    "recommends option {letter:?}, which it does not offer: name one by its \
                     capital letter, A for the first, up to {} for the last of its {}",
                    option_letter(question.options.len() - 1),
                    question.options.len()
                ));
            }
            _ => {}
        }
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

/// Adds to the schema the ten rules of [`RunResult::validate`] that JSON
/// Schema can express.
fn result_invariants(schema: &mut Schema) {
    let rules = json!([
        {
            "properties": {
                "questions": {
                    "items": {
                        "properties": { "recommendation": { "type": "object" } },
                        "required": ["recommendation"]
                    }
                }
            }
        },
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
        },
        {
            "properties": {
                "resolutions": {
                    "items": {
                        "if": { "properties": { "basis_kind": { "const": "ticket" } }, "required": ["basis_kind"] },
                        "then": { "required": ["quote"] },
                        "else": { "not": { "required": ["quote"] } }
                    }
                }
            }
        },
        {
            "if": { "properties": { "refused_choices_asked": { "minItems": 1 } }, "required": ["refused_choices_asked"] },
            "then": {
                "properties": {
                    "status": { "enum": ["questions", "premise_false"] },
                    "questions": { "minItems": 1 }
                },
                "required": ["questions"]
            }
        },
        {
            "if": { "properties": { "intake": { "type": "object" } }, "required": ["intake"] },
            "then": { "properties": { "status": { "enum": ["done", "questions"] } } }
        },
        {
            "if": {
                "anyOf": [
                    { "properties": { "plan": { "type": "object" } }, "required": ["plan"] },
                    { "properties": { "approval": { "type": "string" } }, "required": ["approval"] },
                    { "properties": { "findings": { "minItems": 1 } }, "required": ["findings"] },
                    { "properties": { "rulings": { "minItems": 1 } }, "required": ["rulings"] },
                    { "properties": { "finding_responses": { "minItems": 1 } }, "required": ["finding_responses"] }
                ]
            },
            "then": { "properties": { "status": { "const": "done" } } }
        },
        {
            "if": { "properties": { "approval": { "const": "changed" } }, "required": ["approval"] },
            "then": { "required": ["plan"] }
        },
        {
            "if": { "properties": { "approval": { "const": "approved" } }, "required": ["approval"] },
            "then": { "not": { "required": ["plan"] } }
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
