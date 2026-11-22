//! The quality pipeline's contracts (OWL-300, architecture section 5, "The
//! quality pipeline"): the plan a design run returns, the findings a review
//! run returns and how they are settled, and the two files the ticket ref
//! keeps for them, [`TicketPlan`] in [`PLAN_FILE`](crate::refs::PLAN_FILE)
//! and [`TicketReview`] in [`REVIEW_FILE`](crate::refs::REVIEW_FILE).

use std::num::NonZeroU32;

use jiff::Timestamp;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::brief::{PullRequestReview, pull_request_review_problem};
use crate::format::{self, ContractError, Format, PLAN_FORMAT, REVIEW_FORMAT};
use crate::ids::{FindingId, QuestionId, check_name, is_commit_id, is_lower_hex};
use crate::result::{Decision, Followup, PullRequest};
use crate::{Harness, Role, Stage};

/// The most bytes a plan weighs, rendered as JSON: it is kept whole in the
/// ticket ref, posted in a PLAN comment and handed to every later run.
pub const MAX_PLAN_BYTES: usize = 64 * 1024;

/// The most findings one review run returns.
pub const MAX_FINDINGS: usize = 64;

/// The most findings a brief gives or a review pass keeps: those of the
/// roster's three reviewers at most.
pub const MAX_PASS_FINDINGS: usize = 3 * MAX_FINDINGS;

/// The most bytes one finding weighs, rendered as JSON.
pub const MAX_FINDING_BYTES: usize = 8 * 1024;

/// The plan a design run returns (architecture section 5, "Design"): what
/// the Build follows and the reviewers read. Every text is not blank.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    /// What the ticket's work achieves.
    pub goal: String,
    /// The success criteria, taken from the ticket.
    #[schemars(length(min = 1))]
    pub criteria: Vec<String>,
    /// The approach chosen.
    pub approach: String,
    /// The approaches set aside, each with why.
    #[serde(default)]
    pub alternatives: Vec<Alternative>,
    /// The files the work touches, from the repository's root.
    #[serde(default)]
    pub files: Vec<String>,
    /// The ordered steps, each with its test.
    #[schemars(length(min = 1))]
    pub steps: Vec<PlanStep>,
    #[serde(default)]
    pub risks: Vec<String>,
    #[serde(default)]
    pub out_of_scope: Vec<String>,
    /// The choices of how the design made between approaches that would
    /// each deliver the ticket; `on-fork` plan approval reads them.
    #[serde(default)]
    pub forks: Vec<Fork>,
}

/// A part of a [`Plan`], in the plan's order (OWL-350): what the PLAN
/// comment names when it says what changed since the previous version.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PlanPart {
    Goal,
    Criteria,
    Approach,
    Steps,
    Forks,
    Alternatives,
    Files,
    Risks,
    OutOfScope,
}

impl PlanPart {
    /// Every part, in the plan's order.
    pub const ALL: [Self; 9] = [
        Self::Goal,
        Self::Criteria,
        Self::Approach,
        Self::Steps,
        Self::Forks,
        Self::Alternatives,
        Self::Files,
        Self::Risks,
        Self::OutOfScope,
    ];
}

/// An approach the plan set aside.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Alternative {
    pub approach: String,
    /// Why it was set aside.
    pub reason: String,
}

/// One step of a plan.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PlanStep {
    /// What the step does; for a person's step, what the person does and on
    /// what.
    pub what: String,
    /// The test that shows the step done; for a person's step, the result
    /// the person reports.
    pub test: String,
    /// Who does the step (format 2 of `plan.json`, OWL-619): the agent when
    /// absent, which an agent's step never writes, so a plan of agent steps
    /// alone keeps the text, and every hash, it had before.
    #[serde(default, skip_serializing_if = "StepBy::is_agent")]
    pub by: StepBy,
    /// For a person's step, and only for one: whether its result is due
    /// before the merge or acts on what the merge deploys.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<StepWhen>,
}

impl PlanStep {
    /// An agent's step.
    pub fn agent(what: impl Into<String>, test: impl Into<String>) -> Self {
        Self {
            what: what.into(),
            test: test.into(),
            by: StepBy::Agent,
            when: None,
        }
    }

    /// A person's step, due `when`.
    pub fn person(what: impl Into<String>, test: impl Into<String>, when: StepWhen) -> Self {
        Self {
            what: what.into(),
            test: test.into(),
            by: StepBy::Person,
            when: Some(when),
        }
    }

    /// Whether the step is a person's (architecture section 4, "A person's
    /// steps").
    pub fn is_person(&self) -> bool {
        self.by == StepBy::Person
    }
}

/// Who does a step of the plan (OWL-619, architecture section 4, "A
/// person's steps").
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StepBy {
    /// A run does it: the Build.
    #[default]
    Agent,
    /// Only a person can: no run can do it with the permissions a Build's
    /// brief gives, such as a check in a browser, a call that needs a
    /// credential no run holds, or an operation on a deployed environment.
    Person,
}

impl StepBy {
    /// Whether it is [`StepBy::Agent`], which `plan.json` leaves out.
    pub fn is_agent(&self) -> bool {
        *self == Self::Agent
    }
}

/// When a person's step is due (OWL-619).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StepWhen {
    /// Its result says whether the change is right: merge on green waits
    /// for it.
    BeforeMerge,
    /// It acts on what the merge deploys: it holds nothing.
    AfterMerge,
}

/// A step a ticket owes a person, or that waits on such a step's result
/// ([`TicketPlan::owed_steps`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OwedStep {
    /// Its place in the plan, from 1.
    pub number: NonZeroU32,
    pub step: PlanStep,
    /// A person's step's own; an agent's step that waits on one takes that
    /// of the nearest person's step before it.
    pub when: StepWhen,
}

/// A fork of the plan: its choice, the options it chose among, and why.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Fork {
    pub choice: String,
    #[schemars(length(min = 1))]
    pub options: Vec<String>,
    pub reason: String,
}

impl Plan {
    /// The plan as compact JSON, its fields in declaration order: the text
    /// whose SHA-256 is the plan's hash, kept with an approval
    /// ([`KeptApproval::plan_sha256`]).
    pub fn canonical(&self) -> String {
        serde_json::to_string(self).expect("a plan always serializes to JSON")
    }

    /// One part of the plan as compact JSON: the text whose SHA-256 a
    /// [`PublishedPlan`] keeps, to tell which parts a later plan changed.
    pub fn part_canonical(&self, part: PlanPart) -> String {
        fn json<T: Serialize + ?Sized>(value: &T) -> String {
            serde_json::to_string(value).expect("a plan's part always serializes to JSON")
        }
        match part {
            PlanPart::Goal => json(&self.goal),
            PlanPart::Criteria => json(&self.criteria),
            PlanPart::Approach => json(&self.approach),
            PlanPart::Steps => json(&self.steps),
            PlanPart::Forks => json(&self.forks),
            PlanPart::Alternatives => json(&self.alternatives),
            PlanPart::Files => json(&self.files),
            PlanPart::Risks => json(&self.risks),
            PlanPart::OutOfScope => json(&self.out_of_scope),
        }
    }

    /// Checks the rules the types do not carry: a criterion and a step at
    /// least, a fork with an option at least, no blank text, and at most
    /// [`MAX_PLAN_BYTES`] rendered.
    pub(crate) fn check(&self, contract: &'static str) -> Result<(), ContractError> {
        let invalid = |reason: String| Err(ContractError::invalid(contract, reason));
        if self.criteria.is_empty() {
            return invalid("the plan names no success criterion".to_owned());
        }
        if self.steps.is_empty() {
            return invalid("the plan has no step".to_owned());
        }
        if let Some(n) = self.forks.iter().position(|f| f.options.is_empty()) {
            return invalid(format!("fork {} of the plan names no option", n + 1));
        }
        self.check_people(contract)?;
        let texts = [("goal", &self.goal), ("approach", &self.approach)]
            .into_iter()
            .map(|(field, text)| (field.to_owned(), text))
            .chain(list("criteria", &self.criteria))
            .chain(list("files", &self.files))
            .chain(list("risks", &self.risks))
            .chain(list("out_of_scope", &self.out_of_scope))
            .chain(self.alternatives.iter().enumerate().flat_map(|(n, a)| {
                [
                    (format!("alternatives[{n}].approach"), &a.approach),
                    (format!("alternatives[{n}].reason"), &a.reason),
                ]
            }))
            .chain(self.steps.iter().enumerate().flat_map(|(n, s)| {
                [
                    (format!("steps[{n}].what"), &s.what),
                    (format!("steps[{n}].test"), &s.test),
                ]
            }))
            .chain(self.forks.iter().enumerate().flat_map(|(n, f)| {
                [
                    (format!("forks[{n}].choice"), &f.choice),
                    (format!("forks[{n}].reason"), &f.reason),
                ]
                .into_iter()
                .chain(list(format!("forks[{n}].options"), &f.options))
            }));
        for (field, text) in texts {
            if text.trim().is_empty() {
                return invalid(format!("the plan's {field} is blank"));
            }
        }
        let size = format::render_json(self).len();
        if size > MAX_PLAN_BYTES {
            return invalid(format!(
                "the plan weighs {size} bytes rendered as JSON, past the {MAX_PLAN_BYTES} a plan \
                 may weigh"
            ));
        }
        Ok(())
    }

    /// The rules of a person's steps (OWL-619): a person's step says when
    /// it is due and an agent's step does not; no step due before the merge
    /// follows one due after it. A plan of a person's steps alone is one:
    /// the runner hands its ticket back to a person ([`Plan::has_agent_step`]).
    fn check_people(&self, contract: &'static str) -> Result<(), ContractError> {
        let invalid = |reason: String| Err(ContractError::invalid(contract, reason));
        let mut after_merge = None;
        for (n, step) in self.steps.iter().enumerate() {
            match (step.by, step.when) {
                (StepBy::Person, None) => {
                    return invalid(format!(
                        "steps[{n}] is a person's but says no `when`: before_merge or \
                         after_merge"
                    ));
                }
                (StepBy::Agent, Some(_)) => {
                    return invalid(format!(
                        "steps[{n}] is the agent's but says a `when`, which only a person's \
                         step says"
                    ));
                }
                (StepBy::Person, Some(StepWhen::AfterMerge)) => {
                    after_merge = after_merge.or(Some(n))
                }
                (StepBy::Person, Some(StepWhen::BeforeMerge)) => {
                    if let Some(earlier) = after_merge {
                        return invalid(format!(
                            "steps[{n}] is due before the merge but follows steps[{earlier}], \
                             due after it"
                        ));
                    }
                }
                (StepBy::Agent, None) => {}
            }
        }
        Ok(())
    }

    /// Whether a step of the plan is the agent's: a plan without one is a
    /// person's work whole, whose ticket the runner hands back (OWL-619,
    /// architecture section 4, "A ticket with no step for a run").
    pub fn has_agent_step(&self) -> bool {
        self.steps.iter().any(|step| !step.is_person())
    }

    /// The steps that are a person's, with their place from 1.
    pub fn person_steps(&self) -> impl Iterator<Item = (NonZeroU32, &PlanStep)> {
        self.steps
            .iter()
            .enumerate()
            .filter(|(_, step)| step.is_person())
            .map(|(n, step)| (step_number(n), step))
    }
}

/// The place from 1 of the step at index `n`.
fn step_number(n: usize) -> NonZeroU32 {
    NonZeroU32::new(u32::try_from(n + 1).unwrap_or(u32::MAX)).unwrap_or(NonZeroU32::MIN)
}

/// Each text of a list, named by its place.
fn list(field: impl Into<String>, texts: &[String]) -> impl Iterator<Item = (String, &String)> {
    let field = field.into();
    texts
        .iter()
        .enumerate()
        .map(move |(n, text)| (format!("{field}[{n}]"), text))
}

/// A finding of a review run (architecture section 5, "Design review").
/// Every text is not blank.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Finding {
    pub id: FindingId,
    pub severity: Severity,
    /// What is wrong.
    pub claim: String,
    /// What shows it: a `file:line`, a step of the plan, a line of the
    /// ticket.
    pub evidence: String,
    /// The success criterion it bears on, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub criterion: Option<String>,
    /// What would settle it.
    pub settle: String,
}

/// A finding's severity and a reviewer's mandates live in the core, which
/// settles a review pass and draws its roster from them (OWL-301).
pub use owlshift_core::vocab::{Mandate, Severity};

/// What a run does with the findings its brief gives.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FindingsMode {
    /// The arbiter rules on each: confirmed or rejected.
    Rule,
    /// The author answers each, fixed or rejected with its reason
    /// (`pipeline.findings = "author"`): a design or build run.
    Answer,
    /// The author settles each, as confirmed by the arbiter, and answers
    /// each (format 13, OWL-377): fixed, disagreed with, or, from the Build
    /// and for a `minor` on no criterion, deferred; a design or build run.
    Fix,
    /// The Build weighs each, `minor` findings the plan's review noted for
    /// it (format 21, OWL-356), and answers none.
    Note,
}

/// The arbiter's ruling on one finding it was given.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Ruling {
    pub finding: FindingId,
    pub outcome: RulingOutcome,
    /// Why, resting on what the arbiter checked in the repository; not
    /// blank.
    pub reason: String,
}

/// What the arbiter ruled.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RulingOutcome {
    Confirmed,
    Rejected,
}

/// The author's answer to one finding it was given: in author mode
/// (`pipeline.findings = "author"`), or, since OWL-377, to one the arbiter
/// confirmed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FindingResponse {
    pub finding: FindingId,
    pub outcome: ResponseOutcome,
    /// Why; not blank.
    pub reason: String,
}

/// What the author did with a finding (architecture section 5, "A finding
/// goes to its author").
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ResponseOutcome {
    /// The work settles it.
    Fixed,
    /// In author mode: the claim does not hold.
    Rejected,
    /// The arbiter confirmed it and the author keeps its choice, saying why
    /// (format 13, OWL-377).
    Disagreed,
    /// A `minor` on no success criterion the Build leaves for a follow-up or
    /// a note, its reason saying which (format 13, OWL-377).
    Deferred,
}

impl ResponseOutcome {
    /// The finding's outcome this answer gives it.
    pub const fn outcome(self) -> FindingOutcome {
        match self {
            Self::Fixed => FindingOutcome::Fixed,
            Self::Rejected => FindingOutcome::Rejected,
            Self::Disagreed => FindingOutcome::Disagreed,
            Self::Deferred => FindingOutcome::Deferred,
        }
    }

    /// Why a run given `finding` in `mode` may not answer it so, if it may
    /// not: in answer mode the answer is `fixed` or `rejected`; in fix mode
    /// `fixed` or `disagreed`, or `deferred` for a `minor` that names no
    /// criterion, which S13 forbids deferring otherwise.
    pub fn refused(self, mode: FindingsMode, finding: &Finding) -> Option<&'static str> {
        match (mode, self) {
            (FindingsMode::Answer, Self::Fixed | Self::Rejected)
            | (FindingsMode::Fix, Self::Fixed | Self::Disagreed) => None,
            (FindingsMode::Fix, Self::Deferred)
                if finding.severity == Severity::Minor && finding.criterion.is_none() =>
            {
                None
            }
            (FindingsMode::Fix, Self::Deferred) => Some(
                "only a minor finding that names no criterion may be deferred: fix it or \
                 disagree with it",
            ),
            (FindingsMode::Fix, Self::Rejected) => Some(
                "the arbiter confirmed it: answer fixed, disagreed or, for a minor on no \
                 criterion, deferred",
            ),
            (FindingsMode::Answer, _) => {
                Some("no one judged it: answer fixed, or rejected when its claim does not hold")
            }
            (FindingsMode::Rule | FindingsMode::Note, _) => {
                Some("the brief gives no finding to answer")
            }
        }
    }
}

/// The rules of findings: at most `max`, each not blank and at most
/// [`MAX_FINDING_BYTES`], their ids `F1..Fn` in order when `numbered`
/// (a review run's result) or else in increasing order.
pub(crate) fn check_findings(
    contract: &'static str,
    findings: &[Finding],
    max: usize,
    numbered: bool,
) -> Result<(), ContractError> {
    let invalid = |reason: String| Err(ContractError::invalid(contract, reason));
    if findings.len() > max {
        return invalid(format!(
            "{} findings are given, past the {max} allowed",
            findings.len()
        ));
    }
    for (n, finding) in findings.iter().enumerate() {
        let id = &finding.id;
        if numbered {
            let expected = FindingId::nth(u32::try_from(n + 1).unwrap_or(u32::MAX));
            if *id != expected {
                return invalid(format!("finding {id} is out of order: expected {expected}"));
            }
        } else if n > 0 && findings[n - 1].id.number() >= id.number() {
            return invalid(format!("finding {id} is repeated or out of order"));
        }
        let blank = [
            ("claim", Some(&finding.claim)),
            ("evidence", Some(&finding.evidence)),
            ("criterion", finding.criterion.as_ref()),
            ("settle", Some(&finding.settle)),
        ]
        .into_iter()
        .find(|(_, text)| text.is_some_and(|t| t.trim().is_empty()));
        if let Some((field, _)) = blank {
            return invalid(format!("finding {id} has a blank {field}"));
        }
        let size = format::render_json(finding).len();
        if size > MAX_FINDING_BYTES {
            return invalid(format!(
                "finding {id} weighs {size} bytes rendered as JSON, past the \
                 {MAX_FINDING_BYTES} a finding may weigh"
            ));
        }
    }
    Ok(())
}

/// The rules of rulings or responses: in increasing finding order, each
/// with a reason that is not blank.
pub(crate) fn check_answers<'a>(
    contract: &'static str,
    what: &str,
    answers: impl IntoIterator<Item = (&'a FindingId, &'a str)>,
) -> Result<(), ContractError> {
    let mut previous: Option<&FindingId> = None;
    for (id, reason) in answers {
        if previous.is_some_and(|p| p.number() >= id.number()) {
            return Err(ContractError::invalid(
                contract,
                format!("the {what} of {id} is repeated or out of order"),
            ));
        }
        if reason.trim().is_empty() {
            return Err(ContractError::invalid(
                contract,
                format!("the {what} of {id} has no reason"),
            ));
        }
        previous = Some(id);
    }
    Ok(())
}

/// Checks a list of distinct mandates, at least one.
pub(crate) fn check_mandates(
    contract: &'static str,
    mandates: &[Mandate],
) -> Result<(), ContractError> {
    if mandates.is_empty() {
        return Err(ContractError::invalid(
            contract,
            "no review mandate is given",
        ));
    }
    if let Some(n) = (1..mandates.len()).find(|&n| mandates[..n].contains(&mandates[n])) {
        return Err(ContractError::invalid(
            contract,
            format!("review mandate {:?} is given twice", mandates[n]),
        ));
    }
    Ok(())
}

/// A ticket's plan, in [`PLAN_FILE`](crate::refs::PLAN_FILE): the latest
/// plan of its design runs, the text and the base commit it was written on,
/// and the decider's approval once given.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(title = "Owlshift ticket plan")]
pub struct TicketPlan {
    pub format: Format<PLAN_FORMAT>,
    pub plan: Plan,
    /// The SHA-256 of the ticket's title, a NUL byte and its description,
    /// as 64 lowercase hexadecimal digits, as `intake.json` keeps it.
    #[schemars(regex(pattern = r"^[0-9a-f]{64}$"))]
    pub text_sha256: String,
    /// The base commit the plan was written on, a full commit id.
    pub base: String,
    /// The decider's approval, once given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval: Option<KeptApproval>,
    /// The plan as last published in the ticket's document (OWL-350), once
    /// one was: kept across the plans of later design runs, so that each
    /// PLAN comment updates the same document and says what changed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published: Option<PublishedPlan>,
    /// The agent's steps of this plan its Build left because they need a
    /// person's step's result (format 2, OWL-619): the `owed` of its `done`
    /// results, gathered over the Build runs of this plan, never fewer. Each
    /// is a step of the plan, the agent's, after a person's step, in
    /// increasing order. A new plan starts with none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub owed: Vec<NonZeroU32>,
    /// The person's steps due before the merge that a round at Watch asked
    /// its decider (format 3, OWL-619; architecture section 4, "A person's
    /// steps"): one at most a step, in the plan's order, with how its
    /// decider answered once the round was. A check answered a defect is
    /// dropped once the run after it lands, so its step is asked again; one
    /// answered `passed` stays, and its step is never asked again. A new
    /// plan starts with none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checks: Vec<StepCheck>,
    /// The agent's steps of this plan the resolver handed to a person, the
    /// verdict `person_step` on a Build that stopped `blocked` there (format
    /// 4, OWL-620; architecture section 4, "A `blocked` through the
    /// resolver"): each read as a person's step due as it says
    /// ([`TicketPlan::effective`]), the plan's own text, and every hash, left
    /// as written. In increasing order, each once. A new plan starts with
    /// none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub handed: Vec<HandedStep>,
}

/// An agent's step of the plan the resolver handed to a person (OWL-620).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HandedStep {
    /// Its place in the plan, from 1: an agent's step there.
    pub step: NonZeroU32,
    /// When its result is due.
    pub when: StepWhen,
}

/// A person's step a round at Watch asked its decider (OWL-619).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StepCheck {
    /// The step's place in the plan, from 1: a person's step due before
    /// the merge.
    pub step: NonZeroU32,
    /// The round that asked it.
    pub round: NonZeroU32,
    /// Its question's id in that round.
    pub question: QuestionId,
    /// How its decider answered, once the round was answered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer: Option<StepAnswer>,
}

/// How a person's step was answered (OWL-619).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StepAnswer {
    /// The step gave the result it names: the option the runner
    /// recommends.
    Passed,
    /// Anything else: the other option, or the decider's own words, which
    /// the next run at Watch follows.
    Defect,
}

/// What the run at Watch after a person's steps were answered follows
/// ([`TicketPlan::steps_run`], OWL-619).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StepsRun {
    /// Every answered check, in the plan's order: the results the run
    /// reads.
    pub answered: Vec<StepCheck>,
    /// The steps answered a defect, which the run fixes.
    pub defects: Vec<NonZeroU32>,
    /// The agent's steps owed before the merge whose person's steps before
    /// them all passed, which the run builds.
    pub build: Vec<NonZeroU32>,
}

/// The plan as last published in the ticket's document (OWL-350,
/// architecture section 5, "Kept and shown"), and what its PLAN comment
/// told. The approval never reads it: it binds the plan of [`TicketPlan`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PublishedPlan {
    /// The tracker's identifier of the document, which the next PLAN
    /// comment updates.
    #[schemars(regex(pattern = r"\S"))]
    pub document: String,
    /// Where a person opens it, as the tracker gave it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// The plan's version, from 1: one more each time a published plan's
    /// hash differs from the one before.
    pub version: NonZeroU32,
    /// The Design review pass whose end published it.
    pub pass: NonZeroU32,
    /// The SHA-256 of the published plan's [`Plan::canonical`] text.
    #[schemars(regex(pattern = r"^[0-9a-f]{64}$"))]
    pub plan_sha256: String,
    /// The SHA-256 of each part's [`Plan::part_canonical`] text, in
    /// [`PlanPart::ALL`]'s order.
    #[schemars(length(min = 9, max = 9))]
    pub parts: Vec<String>,
    /// What its PLAN comment said changed.
    pub change: PlanChange,
}

/// What a PLAN comment says of its plan beside the version before
/// (OWL-350).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PlanChange {
    /// The ticket's first published plan.
    First,
    /// A new version: these parts differ from version `since`'s.
    Changed {
        since: NonZeroU32,
        #[schemars(length(min = 1, max = 9))]
        parts: Vec<PlanPart>,
    },
    /// The plan published again unchanged, after a later pass.
    Unchanged,
}

/// The decider's approval of a plan.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct KeptApproval {
    /// The tracker's identifier of the comment that approved it: the
    /// decider's, or the approval round's own when the decider answered it
    /// with a thumbs-up (OWL-371), or, when the resolver decided the
    /// approval (OWL-343), or a change Design applied with no review pass
    /// (OWL-463), its DECISION comment, kept with the decision in
    /// `questions.json`.
    #[schemars(regex(pattern = r"\S"))]
    pub comment: String,
    /// The SHA-256 of the approved plan's [`Plan::canonical`] text, as 64
    /// lowercase hexadecimal digits: a Build runs only on a plan with this
    /// hash when approval is required (OWL-32).
    #[schemars(regex(pattern = r"^[0-9a-f]{64}$"))]
    pub plan_sha256: String,
}

impl TicketPlan {
    const CONTRACT: &str = "ticket plan";

    /// Parses and validates a `plan.json`. A format-1 one, written before a
    /// plan could hold a person's step (OWL-619), is read with every step
    /// the agent's and nothing owed, and refused when it says otherwise; a
    /// format-2 one, written before a round at Watch asked a person's steps,
    /// is read with no check, and refused when it keeps one; a format-3 one,
    /// written before the resolver handed a step to a person (OWL-620), is
    /// read with none handed, and refused when it keeps one; the next write
    /// gives each the current format.
    pub fn parse(input: &str) -> Result<Self, ContractError> {
        let older = format::parse_json_older(Self::CONTRACT, 1, PLAN_FORMAT, "owed", input)
            .map(|read| (1, read))
            .or_else(|| {
                format::parse_json_older(Self::CONTRACT, 2, PLAN_FORMAT, "checks", input)
                    .map(|read| (2, read))
            })
            .or_else(|| {
                format::parse_json_older(Self::CONTRACT, 3, PLAN_FORMAT, "handed", input)
                    .map(|read| (3, read))
            });
        let plan: Self = match older {
            Some((older, read)) => {
                let plan: Self = read?;
                if !plan.handed.is_empty() {
                    return Err(ContractError::invalid(
                        Self::CONTRACT,
                        format!("format {older} records no step handed to a person"),
                    ));
                }
                if older == 1
                    && let Some(n) = plan
                        .plan
                        .steps
                        .iter()
                        .position(|step| step.by != StepBy::Agent || step.when.is_some())
                {
                    return Err(ContractError::invalid(
                        Self::CONTRACT,
                        format!("format 1 records no person's step, but steps[{n}] says one"),
                    ));
                }
                if older < 3 && !plan.checks.is_empty() {
                    return Err(ContractError::invalid(
                        Self::CONTRACT,
                        format!("format {older} records no checks"),
                    ));
                }
                plan
            }
            None => format::parse_json(Self::CONTRACT, PLAN_FORMAT, input)?,
        };
        plan.validate()?;
        Ok(plan)
    }

    /// The plan as its steps stand (OWL-620): the steps of
    /// [`TicketPlan::handed`] a person's, due as handed; the plan's own text
    /// otherwise. What the Build follows and what is owed read it; the
    /// approval and every hash bind [`TicketPlan::plan`] as written.
    pub fn effective(&self) -> Plan {
        let mut plan = self.plan.clone();
        for handed in &self.handed {
            let index = usize::try_from(handed.step.get() - 1).unwrap_or(usize::MAX);
            if let Some(step) = plan.steps.get_mut(index) {
                step.by = StepBy::Person;
                step.when = Some(handed.when);
            }
        }
        plan
    }

    /// Hands the agent's step `step` to a person, due `when` (OWL-620): kept
    /// in [`TicketPlan::handed`], in order, and no longer among the agent's
    /// steps it owes, being a person's now. A step already handed keeps its
    /// first `when`.
    pub fn hand(&mut self, step: NonZeroU32, when: StepWhen) {
        if self.handed.iter().any(|handed| handed.step == step) {
            return;
        }
        self.handed.push(HandedStep { step, when });
        self.handed.sort_by_key(|handed| handed.step);
        self.owed.retain(|number| *number != step);
    }

    /// The steps the ticket owes a person, in the plan's order (OWL-619,
    /// architecture section 4, "A person's steps"): every person's step, one
    /// the resolver handed included (OWL-620), and every agent's step of
    /// [`TicketPlan::owed`], which takes the `when` of the nearest person's
    /// step before it.
    pub fn owed_steps(&self) -> Vec<OwedStep> {
        let mut owed = Vec::new();
        let mut when = None;
        for (n, step) in self.effective().steps.iter().enumerate() {
            let number = step_number(n);
            if step.is_person() {
                when = step.when;
            }
            let Some(due) = when else {
                continue;
            };
            if step.is_person() || self.owed.contains(&number) {
                owed.push(OwedStep {
                    number,
                    step: step.clone(),
                    when: due,
                });
            }
        }
        owed
    }

    /// How many of [`TicketPlan::owed_steps`] due before the merge are still
    /// owed (OWL-619): a person's step its decider has not answered as
    /// passed, and an agent's step no run at Watch has built yet. Merge on
    /// green waits while any is.
    pub fn owed_before_merge(&self) -> usize {
        self.owed_steps()
            .iter()
            .filter(|owed| owed.when == StepWhen::BeforeMerge)
            .filter(|owed| !owed.step.is_person() || !self.passed(owed.number))
            .count()
    }

    /// The check kept for step `number`, if one is.
    fn check(&self, number: NonZeroU32) -> Option<&StepCheck> {
        self.checks.iter().find(|check| check.step == number)
    }

    /// Whether step `number`'s decider answered it as passed.
    fn passed(&self, number: NonZeroU32) -> bool {
        self.check(number)
            .is_some_and(|check| check.answer == Some(StepAnswer::Passed))
    }

    /// The person's steps due before the merge that a round at Watch asks
    /// next (OWL-619), in the plan's order: those with no check, or a check
    /// no answer settled (an ask a round no longer waits on). Never one
    /// answered as passed, nor one answered a defect, whose run comes
    /// first.
    pub fn steps_to_ask(&self) -> Vec<(NonZeroU32, PlanStep)> {
        self.effective()
            .person_steps()
            .filter(|(_, step)| step.when == Some(StepWhen::BeforeMerge))
            .filter(|(number, _)| self.check(*number).is_none_or(|c| c.answer.is_none()))
            .map(|(number, step)| (number, step.clone()))
            .collect()
    }

    /// What the run at Watch after a round of a person's steps follows
    /// (OWL-619), `None` when no run is due: the steps answered a defect,
    /// which it fixes, and the agent's steps owed before the merge whose
    /// person's steps before them all passed, which it builds. A defect
    /// never releases an agent's step: its own step is asked again first.
    pub fn steps_run(&self) -> Option<StepsRun> {
        let plan = self.effective();
        let answered: Vec<StepCheck> = plan
            .person_steps()
            .filter_map(|(number, _)| self.check(number))
            .filter(|check| check.answer.is_some())
            .cloned()
            .collect();
        let defects: Vec<NonZeroU32> = answered
            .iter()
            .filter(|check| check.answer == Some(StepAnswer::Defect))
            .map(|check| check.step)
            .collect();
        let build: Vec<NonZeroU32> = self
            .owed_steps()
            .into_iter()
            .filter(|owed| owed.when == StepWhen::BeforeMerge && !owed.step.is_person())
            .filter(|owed| {
                plan.person_steps()
                    .take_while(|(number, _)| *number < owed.number)
                    .all(|(number, _)| self.passed(number))
            })
            .map(|owed| owed.number)
            .collect();
        (!defects.is_empty() || !build.is_empty()).then_some(StepsRun {
            answered,
            defects,
            build,
        })
    }

    /// What the run [`TicketPlan::steps_run`] gives settles once it lands,
    /// or ends with nothing to change (OWL-619): its defects' checks are
    /// dropped, so those steps are asked again, and the agent's steps it
    /// built leave `owed`. Read from the plan alone, so the push's adoption
    /// after a crash settles what its run was given.
    pub fn settle_steps_run(&mut self) {
        let Some(run) = self.steps_run() else {
            return;
        };
        self.checks
            .retain(|check| check.answer != Some(StepAnswer::Defect));
        self.owed.retain(|number| !run.build.contains(number));
    }

    /// Keeps how the checks asked in `round` were answered, `answer` giving
    /// each by its question's id; a check with no answer given stays as it
    /// is. Whether any changed.
    pub fn answer_round(
        &mut self,
        round: NonZeroU32,
        answer: impl Fn(&QuestionId) -> Option<StepAnswer>,
    ) -> bool {
        let mut changed = false;
        for check in self
            .checks
            .iter_mut()
            .filter(|check| check.round == round && check.answer.is_none())
        {
            if let Some(given) = answer(&check.question) {
                check.answer = Some(given);
                changed = true;
            }
        }
        changed
    }

    /// Keeps that `round` asks `asked`, each step with its question's id,
    /// in place of any check of those steps an earlier ask left unanswered.
    pub fn ask_steps(&mut self, round: NonZeroU32, asked: &[(NonZeroU32, QuestionId)]) {
        self.checks
            .retain(|check| !asked.iter().any(|(step, _)| *step == check.step));
        self.checks
            .extend(asked.iter().map(|(step, question)| StepCheck {
                step: *step,
                round,
                question: question.clone(),
                answer: None,
            }));
        self.checks.sort_by_key(|check| check.step);
    }

    pub fn render(&self) -> String {
        format::render_json(self)
    }

    /// Checks the plan's own rules, the hashes are SHA-256 in lowercase
    /// hex, the base a full commit id and the approving comment a name
    /// written exactly.
    pub fn validate(&self) -> Result<(), ContractError> {
        let invalid = |reason: String| Err(ContractError::invalid(Self::CONTRACT, reason));
        self.plan.check(Self::CONTRACT)?;
        if !is_lower_hex(&self.text_sha256, 64) {
            return invalid(format!(
                "text_sha256 {:?} is not a SHA-256 in lowercase hex",
                self.text_sha256
            ));
        }
        if !is_commit_id(&self.base) {
            return invalid(format!("base {:?} is not a full commit id", self.base));
        }
        if let Some(approval) = &self.approval {
            if let Err(broken) = check_name(&approval.comment) {
                return invalid(format!("approval.comment {:?} {broken}", approval.comment));
            }
            if !is_lower_hex(&approval.plan_sha256, 64) {
                return invalid(format!(
                    "approval.plan_sha256 {:?} is not a SHA-256 in lowercase hex",
                    approval.plan_sha256
                ));
            }
        }
        if let Some(published) = &self.published {
            published.check().map_err(|reason| {
                ContractError::invalid(Self::CONTRACT, format!("published: {reason}"))
            })?;
        }
        check_handed(&self.plan, &self.handed).map_err(|reason| {
            ContractError::invalid(Self::CONTRACT, format!("handed: {reason}"))
        })?;
        // A handed step is a person's: what is owed and checked reads it so.
        let effective = self.effective();
        check_owed(&effective, &self.owed)
            .map_err(|reason| ContractError::invalid(Self::CONTRACT, format!("owed: {reason}")))?;
        check_checks(&effective, &self.checks)
            .map_err(|reason| ContractError::invalid(Self::CONTRACT, format!("checks: {reason}")))
    }
}

/// The rules of the steps the resolver handed to a person (OWL-620): each
/// an agent's step of `plan` as written, in increasing order, each once.
fn check_handed(plan: &Plan, handed: &[HandedStep]) -> Result<(), String> {
    let mut previous: Option<NonZeroU32> = None;
    for handed in handed {
        let number = handed.step;
        if previous.is_some_and(|p| p >= number) {
            return Err(format!("step {number} is repeated or out of order"));
        }
        previous = Some(number);
        let index = usize::try_from(number.get() - 1).unwrap_or(usize::MAX);
        match plan.steps.get(index) {
            None => {
                return Err(format!(
                    "step {number} is not a step of the plan, which has {}",
                    plan.steps.len()
                ));
            }
            Some(step) if step.is_person() => {
                return Err(format!("step {number} is a person's already"));
            }
            Some(_) => {}
        }
    }
    Ok(())
}

/// The rules of the checks of a person's steps (OWL-619): each of a person's
/// step of `plan` due before the merge, in increasing order, each once, and
/// no two of one round under one question.
fn check_checks(plan: &Plan, checks: &[StepCheck]) -> Result<(), String> {
    let mut previous: Option<NonZeroU32> = None;
    for (n, check) in checks.iter().enumerate() {
        if previous.is_some_and(|p| p >= check.step) {
            return Err(format!("step {} is repeated or out of order", check.step));
        }
        previous = Some(check.step);
        let index = usize::try_from(check.step.get() - 1).unwrap_or(usize::MAX);
        let due_before = plan
            .steps
            .get(index)
            .is_some_and(|step| step.is_person() && step.when == Some(StepWhen::BeforeMerge));
        if !due_before {
            return Err(format!(
                "step {} is not a person's step due before the merge",
                check.step
            ));
        }
        if checks[..n]
            .iter()
            .any(|other| other.round == check.round && other.question == check.question)
        {
            return Err(format!(
                "round {} asks {} for two steps",
                check.round, check.question
            ));
        }
    }
    Ok(())
}

/// The rules of the agent's steps a Build owes (OWL-619): each a step of
/// `plan`, in increasing order, each once, the agent's and after a person's
/// step, since only a person's step's result can hold it back.
pub(crate) fn check_owed(plan: &Plan, owed: &[NonZeroU32]) -> Result<(), String> {
    let first_person = plan.steps.iter().position(PlanStep::is_person);
    let mut previous: Option<NonZeroU32> = None;
    for &number in owed {
        if previous.is_some_and(|p| p >= number) {
            return Err(format!("step {number} is repeated or out of order"));
        }
        previous = Some(number);
        let index = usize::try_from(number.get() - 1).unwrap_or(usize::MAX);
        let Some(step) = plan.steps.get(index) else {
            return Err(format!(
                "step {number} is not a step of the plan, which has {}",
                plan.steps.len()
            ));
        };
        if step.is_person() {
            return Err(format!(
                "step {number} is a person's, owed already: list only the agent's steps left"
            ));
        }
        if first_person.is_none_or(|first| index < first) {
            return Err(format!(
                "step {number} comes before any person's step, so no person's result holds it \
                 back: build it"
            ));
        }
    }
    Ok(())
}

impl PublishedPlan {
    /// The rules the types do not carry: a document named exactly, the
    /// hashes in lowercase hex, one per part, and a change that names parts
    /// once each and an earlier version.
    fn check(&self) -> Result<(), String> {
        if let Err(broken) = check_name(&self.document) {
            return Err(format!("document {:?} {broken}", self.document));
        }
        if !is_lower_hex(&self.plan_sha256, 64) {
            return Err(format!(
                "plan_sha256 {:?} is not a SHA-256 in lowercase hex",
                self.plan_sha256
            ));
        }
        if self.parts.len() != PlanPart::ALL.len()
            || !self.parts.iter().all(|h| is_lower_hex(h, 64))
        {
            return Err(format!(
                "parts must be {} SHA-256 in lowercase hex",
                PlanPart::ALL.len()
            ));
        }
        match &self.change {
            PlanChange::First if self.version.get() != 1 => {
                Err(format!("version {} is not a first one", self.version))
            }
            PlanChange::Changed { since, parts } => {
                if *since >= self.version {
                    return Err(format!("version {} changed since {since}", self.version));
                }
                let once = parts
                    .iter()
                    .enumerate()
                    .all(|(i, part)| !parts[..i].contains(part));
                if parts.is_empty() || !once {
                    return Err("a change names its parts, once each".to_owned());
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
}

/// A ticket's review record, in [`REVIEW_FILE`](crate::refs::REVIEW_FILE):
/// each review pass with its findings and what became of them, and the pull
/// request reviews answered.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(title = "Owlshift ticket review")]
pub struct TicketReview {
    pub format: Format<REVIEW_FORMAT>,
    /// Every review pass, oldest first.
    #[serde(default)]
    pub passes: Vec<ReviewPass>,
    /// The ids of the pull request reviews answered, each once.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(extend("uniqueItems" = true))]
    pub answered_reviews: Vec<u64>,
    /// The pull request reviews the review loop under way answers
    /// (OWL-307), as read when it started: each one an instruction of the
    /// decider or quoted data, as the loop's Build runs are given them.
    /// Present from the loop's start to its push landing, or its report
    /// owed, when their ids move to `answered_reviews`; dropped when watch
    /// stops following the pull request.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub answering: Vec<PullRequestReview>,
    /// The reviews of colleagues relayed to the ticket's decider (format 2,
    /// OWL-585): each listed reviewer's review that is neither the
    /// decider's nor an approval, asked about at Watch, with the decider's
    /// answer once the answer check read it. Kept from the ask until the
    /// review loop that answers them starts, or watch stops following the
    /// pull request; one set aside moves to `answered_reviews` at once.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub relayed: Vec<RelayedReview>,
    /// What the Build that Verify reviews gave for its delivery (OWL-304):
    /// kept once a Build is done and its gate passed, so that `continue`
    /// runs a Verify pass cut short again and delivers. Dropped when the
    /// ticket is delivered or dispatched again from Ready.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build: Option<KeptBuild>,
    /// The Verify pass, the record's last, whose `minor` findings one last
    /// Build fixes (OWL-356): that Build is delivered with no Verify pass
    /// after it. Kept with the pass; dropped when that Build asks, takes
    /// late comments or follows a decider's reply to a DECISION comment,
    /// when another pass is kept, and at the delivery.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_fix: Option<NonZeroU32>,
}

/// A colleague's review relayed to the ticket's decider (OWL-585): the
/// question that asks about it and, once answered, what the decider chose.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RelayedReview {
    /// The question round that asks about it.
    pub round: NonZeroU32,
    /// Its question in that round.
    pub question: QuestionId,
    /// What the decider chose, once the answer check read their answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer: Option<RelayAnswer>,
    /// The review as it was asked about, quoted data, within the bytes a
    /// brief gives one: "apply as written" gives a review loop this copy.
    pub review: PullRequestReview,
}

/// The decider's answer about a colleague's review that starts a review
/// loop (OWL-585); a review set aside is answered at once instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RelayAnswer {
    /// Applied as written: the loop's instruction, as the decider's own
    /// review is.
    Apply,
    /// The decider said what to do instead: their answer is the
    /// instruction, the review quoted data beside it.
    Instead,
}

/// One review pass: the roster's findings on the plan or the diff.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReviewPass {
    /// `design_review` or `verify`.
    pub stage: Stage,
    /// The pass at its stage, from 1.
    pub pass: NonZeroU32,
    /// The findings of every reviewer of the pass, ids increasing, at most
    /// [`MAX_PASS_FINDINGS`].
    #[serde(default)]
    #[schemars(length(max = MAX_PASS_FINDINGS))]
    pub findings: Vec<KeptFinding>,
    /// The reviewers that ran the pass, in seat order, those that found
    /// nothing included (OWL-304): what the DELIVERY report names.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reviewers: Vec<Reviewer>,
    /// At Verify, the commit the pass reviewed, a full commit id (OWL-356):
    /// the next pass of the loop reviews the diff since.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    /// At Design review, the print of the plan the pass reviewed (OWL-356):
    /// the next pass of the loop reviews what changed since, `plan.json`
    /// keeping only the current plan.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<PlanPrint>,
    /// At Design review, the design run whose revision the pass keeps, which
    /// no reviewer reviews: Design's last revision, which answered the
    /// findings of the pass before, the loop's last (OWL-377), or, since
    /// OWL-463, the revision that applied a resolver's change on the plan's
    /// approval with no review pass, the plan's approval naming that
    /// change's DECISION comment. Such a pass has no reviewer, no finding
    /// and no plan's print.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(regex(pattern = r"\S"))]
    pub revision: Option<String>,
    /// The opt-in Codex reviewer the pass ran without (OWL-348,
    /// `pipeline.codex_reviewer`), and why: the PLAN comment and the
    /// DELIVERY report say so. Never on a pass that names a Codex reviewer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dropped: Option<DroppedReviewer>,
}

/// The most characters a dropped reviewer's reason holds.
pub const MAX_DROPPED_REASON_CHARS: usize = 500;

/// A reviewer a pass ran without (OWL-348): the opt-in Codex reviewer, when
/// Codex could not serve the pass. The reason is the runner's own words,
/// never a harness's output.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DroppedReviewer {
    pub harness: Harness,
    /// The mandates it would have held, at least one, each once.
    #[schemars(length(min = 1))]
    pub mandates: Vec<Mandate>,
    /// Why it did not review the pass: not blank, at most
    /// [`MAX_DROPPED_REASON_CHARS`] characters.
    #[schemars(regex(pattern = r"\S"), length(max = MAX_DROPPED_REASON_CHARS))]
    pub reason: String,
}

/// A plan's print (OWL-356): the SHA-256 of each part's
/// [`Plan::part_canonical`] text, in [`PlanPart::ALL`]'s order, and of each
/// step's, in the plan's order, as lowercase hex.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PlanPrint {
    #[schemars(length(min = 9, max = 9))]
    pub parts: Vec<String>,
    #[schemars(length(min = 1))]
    pub steps: Vec<String>,
}

/// What a Build gave for its delivery, kept while Verify reviews it
/// (OWL-304, [`TicketReview::build`]): what the push, the pull request and
/// the DELIVERY report take, gathered over the delivery's Build runs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct KeptBuild {
    /// The commit the runner's gate passed on, a full commit id: what is
    /// pushed.
    pub commit: String,
    /// The Build run's `summary`.
    pub summary: String,
    /// The pull request the Build run proposed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr: Option<PullRequest>,
    /// The project gate's commands, all passed.
    #[serde(default)]
    pub gate: Vec<String>,
    /// The runner's own notes the report lists, such as the late comments a
    /// Build run was given (OWL-139).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<Decision>,
    /// The follow-ups the delivery's Build runs proposed, then those its
    /// Verify passes proposed from findings.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub followups: Vec<Followup>,
    /// The newest edit of the decider's comments the Build run's brief
    /// held, when it held one: a comment of theirs edited later is one the
    /// Build did not see (OWL-139).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seen: Option<Timestamp>,
}

/// A finding as the ticket ref keeps it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct KeptFinding {
    pub finding: Finding,
    pub reviewer: Reviewer,
    /// The arbiter's ruling (`pipeline.findings = "arbiter"`); never with a
    /// response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ruling: Option<KeptRuling>,
    /// The author's response (`pipeline.findings = "author"`); never with a
    /// ruling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response: Option<KeptResponse>,
    /// What became of it, once settled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<FindingOutcome>,
}

/// The reviewer that raised a finding.
// Read through `ReviewerForm`, which still accepts the `stand_in` key a
// file written before OWL-347 holds, so a parsed `Reviewer` never has it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(from = "ReviewerForm")]
pub struct Reviewer {
    /// The reviewer's role: `design_review` or `verify`, the pass's stage.
    pub role: Role,
    /// Its mandates, at least one, each once.
    pub mandates: Vec<Mandate>,
    pub harness: Harness,
    /// The model it ran on, as the harness was given it.
    pub model: String,
    /// A reviewer that ran on the author's own model, no other one being
    /// configured (OWL-304): the reports say so.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub same_model: bool,
    /// A reviewer that ran on the harness declared as Claude Code's
    /// fallback, Claude Code being at its usage limit (OWL-406): the
    /// reports say so.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub fallback: bool,
}

/// What a kept reviewer may say: [`Reviewer`]'s keys, and `stand_in`, which
/// marked the critic standing in on Claude Code for Codex until OWL-347
/// (architecture section 5, "Codex"). It is read and ignored, never written,
/// and left out of the schema. It is also the schema of a reviewer, so the
/// docs of the keys are written here too.
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(
    rename = "Reviewer",
    description = "The reviewer that raised a finding."
)]
struct ReviewerForm {
    /// The reviewer's role: `design_review` or `verify`, the pass's stage.
    role: Role,
    /// Its mandates, at least one, each once.
    #[schemars(length(min = 1))]
    mandates: Vec<Mandate>,
    harness: Harness,
    /// The model it ran on, as the harness was given it.
    #[schemars(regex(pattern = r"\S"))]
    model: String,
    /// Written before OWL-347 only; ignored.
    #[serde(default, rename = "stand_in")]
    #[schemars(skip)]
    _stand_in: bool,
    /// A reviewer that ran on the author's own model, no other one being
    /// configured (OWL-304): the reports say so.
    #[serde(default)]
    same_model: bool,
    /// A reviewer that ran on the harness declared as Claude Code's
    /// fallback, Claude Code being at its usage limit (OWL-406): the
    /// reports say so.
    #[serde(default)]
    fallback: bool,
}

impl From<ReviewerForm> for Reviewer {
    fn from(form: ReviewerForm) -> Self {
        Self {
            role: form.role,
            mandates: form.mandates,
            harness: form.harness,
            model: form.model,
            same_model: form.same_model,
            fallback: form.fallback,
        }
    }
}

/// The arbiter's ruling, as kept with its finding.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct KeptRuling {
    pub outcome: RulingOutcome,
    #[schemars(regex(pattern = r"\S"))]
    pub reason: String,
}

/// The author's response, as kept with its finding.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct KeptResponse {
    pub outcome: ResponseOutcome,
    #[schemars(regex(pattern = r"\S"))]
    pub reason: String,
}

/// What became of a finding.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FindingOutcome {
    /// A later run fixed it.
    Fixed,
    /// The arbiter or the author rejected it.
    Rejected,
    /// It went to the decider in a question round; no pass asks one since
    /// OWL-377, a record written before keeps it.
    Asked,
    /// It became a proposed follow-up; the runner proposes none since
    /// OWL-377, a record written before keeps it.
    Proposed,
    /// The author kept its choice against it, saying why (OWL-377).
    Disagreed,
    /// The Build left it, a `minor` on no criterion, for a follow-up or a
    /// note (OWL-377).
    Deferred,
}

impl Default for TicketReview {
    fn default() -> Self {
        Self::new()
    }
}

impl TicketReview {
    const CONTRACT: &str = "ticket review";

    /// A record with nothing in it yet.
    pub fn new() -> Self {
        Self {
            format: Format,
            passes: Vec::new(),
            answered_reviews: Vec::new(),
            answering: Vec::new(),
            relayed: Vec::new(),
            build: None,
            last_fix: None,
        }
    }

    /// Parses and validates a `review.json`. A format-1 one, written before
    /// a colleague's review was relayed (OWL-585), is read as the current
    /// format.
    pub fn parse(input: &str) -> Result<Self, ContractError> {
        let review: Self =
            match format::parse_json_older(Self::CONTRACT, 1, REVIEW_FORMAT, "relayed", input) {
                Some(read) => read?,
                None => format::parse_json(Self::CONTRACT, REVIEW_FORMAT, input)?,
            };
        review.validate()?;
        Ok(review)
    }

    pub fn render(&self) -> String {
        format::render_json(self)
    }

    /// Checks each pass is at Design review or Verify, its findings keep a
    /// result's rules with ids increasing, each raised by a reviewer of the
    /// pass's role with its mandates distinct and a model named, each
    /// response one its judgment allows (none to a rejected ruling; to a
    /// confirmed one, an answer of fix mode since OWL-377; with no ruling,
    /// author mode, `fixed` or `rejected`), and none given an outcome its
    /// ruling or response contradicts (a rejected ruling gives `rejected`
    /// only; a response its own outcome; a confirmed ruling with no
    /// response `fixed`, `asked` or `proposed`); each reviewer a pass names is of its role,
    /// with its mandates distinct and a model named; each answered review is
    /// named once; each review under way once, not answered, with a body or
    /// a comment, each comment with a path and a body, within the bytes a
    /// brief gives it; the kept build's commit is a full commit id; a pass's
    /// `head` only at Verify, a full commit id, and its plan print only at
    /// Design review, 9 part hashes and a hash per step in lowercase hex; a
    /// pass's `revision` only at Design review, a name written exactly,
    /// with no reviewer, finding or print (OWL-377); a pass's `dropped`
    /// reviewer with its mandates distinct and a reason neither blank nor
    /// over [`MAX_DROPPED_REASON_CHARS`], on a pass that keeps no revision
    /// and names no reviewer on its harness (OWL-348); and `last_fix` names
    /// the record's last pass, at Design review or Verify (OWL-356,
    /// OWL-377).
    ///
    /// Passes are numbered from 1 in each run of consecutive passes at one
    /// stage, each pass the one before it plus 1, or 1 again: a review loop
    /// on the pull request starts its Verify passes over after the
    /// delivery's (architecture section 5, "Review comments on the pull
    /// request"). No order holds between stages: a settled `replan` sends a
    /// ticket from Verify back to Design, and its design review follows.
    pub fn validate(&self) -> Result<(), ContractError> {
        let invalid = |reason: String| Err(ContractError::invalid(Self::CONTRACT, reason));
        for (n, pass) in self.passes.iter().enumerate() {
            let follows = n
                .checked_sub(1)
                .map(|before| &self.passes[before])
                .filter(|before| before.stage == pass.stage)
                .map(|before| before.pass.get());
            let numbered =
                pass.pass.get() == 1 || follows.is_some_and(|p| pass.pass.get() == p + 1);
            if !numbered {
                return invalid(format!(
                    "{:?} pass {} follows {}: a run of passes at a stage counts from 1, by 1",
                    pass.stage,
                    pass.pass,
                    follows.map_or("another stage's pass".to_owned(), |p| format!("pass {p}"))
                ));
            }
            let role = match pass.stage {
                Stage::DesignReview => Role::DesignReview,
                Stage::Verify => Role::Verify,
                other => {
                    return invalid(format!(
                        "a review pass is kept at {other:?}, not at design review or verify"
                    ));
                }
            };
            let at = format!("{:?} pass {}", pass.stage, pass.pass);
            match (&pass.head, pass.stage) {
                (Some(_), Stage::DesignReview) => {
                    return invalid(format!("{at} keeps a reviewed commit"));
                }
                (Some(head), _) if !is_commit_id(head) => {
                    return invalid(format!("{at} keeps {head:?}, not a full commit id"));
                }
                _ => {}
            }
            if let Some(print) = &pass.plan {
                if pass.stage != Stage::DesignReview {
                    return invalid(format!("{at} keeps a plan's print"));
                }
                let hex = |hashes: &[String]| hashes.iter().all(|h| is_lower_hex(h, 64));
                if print.parts.len() != PlanPart::ALL.len()
                    || print.steps.is_empty()
                    || !hex(&print.parts)
                    || !hex(&print.steps)
                {
                    return invalid(format!(
                        "{at} keeps a plan's print that is not {} part hashes and a hash per step",
                        PlanPart::ALL.len()
                    ));
                }
            }
            if let Some(run) = &pass.revision {
                if pass.stage != Stage::DesignReview {
                    return invalid(format!("{at} keeps a revision"));
                }
                if let Err(broken) = check_name(run) {
                    return invalid(format!("the revision of {at} {broken}"));
                }
                if !pass.reviewers.is_empty() || !pass.findings.is_empty() || pass.plan.is_some() {
                    return invalid(format!(
                        "{at} keeps a revision, which no reviewer reviewed, with reviewers, \
                         findings or a plan's print"
                    ));
                }
            }
            if let Some(dropped) = &pass.dropped {
                check_mandates(Self::CONTRACT, &dropped.mandates)?;
                if dropped.reason.trim().is_empty()
                    || dropped.reason.chars().count() > MAX_DROPPED_REASON_CHARS
                {
                    return invalid(format!(
                        "{at} drops a reviewer with a reason that is blank or longer than \
                         {MAX_DROPPED_REASON_CHARS} characters"
                    ));
                }
                if pass.revision.is_some() {
                    return invalid(format!("{at} keeps a revision and drops a reviewer"));
                }
                if pass.reviewers.iter().any(|r| r.harness == dropped.harness) {
                    return invalid(format!(
                        "{at} drops a reviewer on {:?} and names one",
                        dropped.harness
                    ));
                }
            }
            for reviewer in &pass.reviewers {
                if reviewer.role != role {
                    return invalid(format!(
                        "{at} names a reviewer of role {}",
                        reviewer.role.as_str()
                    ));
                }
                check_mandates(Self::CONTRACT, &reviewer.mandates)?;
                if let Err(broken) = check_name(&reviewer.model) {
                    return invalid(format!("the model of a reviewer of {at} {broken}"));
                }
            }
            let findings: Vec<Finding> = pass.findings.iter().map(|k| k.finding.clone()).collect();
            check_findings(Self::CONTRACT, &findings, MAX_PASS_FINDINGS, false)?;
            for kept in &pass.findings {
                let id = &kept.finding.id;
                let reviewer = &kept.reviewer;
                if reviewer.role != role {
                    return invalid(format!(
                        "finding {id} of {at} names a reviewer of role {}",
                        reviewer.role.as_str()
                    ));
                }
                check_mandates(Self::CONTRACT, &reviewer.mandates)?;
                if let Err(broken) = check_name(&reviewer.model) {
                    return invalid(format!("the model of finding {id} of {at} {broken}"));
                }
                let blank = [
                    kept.ruling.as_ref().map(|r| &r.reason),
                    kept.response.as_ref().map(|r| &r.reason),
                ]
                .into_iter()
                .flatten()
                .any(|reason| reason.trim().is_empty());
                if blank {
                    return invalid(format!("finding {id} of {at} keeps a blank reason"));
                }
                use FindingOutcome::{Asked, Fixed, Proposed, Rejected};
                let confirmed = kept
                    .ruling
                    .as_ref()
                    .map(|ruling| ruling.outcome == RulingOutcome::Confirmed);
                if let Some(response) = &kept.response {
                    // A confirmed finding is answered as in fix mode, an
                    // unjudged one as in answer mode (OWL-377).
                    let mode = match confirmed {
                        Some(false) => {
                            return invalid(format!(
                                "finding {id} of {at} keeps a response to a rejected ruling"
                            ));
                        }
                        Some(true) => FindingsMode::Fix,
                        None => FindingsMode::Answer,
                    };
                    if let Some(refused) = response.outcome.refused(mode, &kept.finding) {
                        return invalid(format!(
                            "finding {id} of {at} keeps the response {:?}: {refused}",
                            response.outcome
                        ));
                    }
                }
                let (judgment, allowed): (&str, Vec<FindingOutcome>) =
                    match (confirmed, &kept.response) {
                        (_, Some(response)) => ("response", vec![response.outcome.outcome()]),
                        (Some(false), None) => ("rejected ruling", vec![Rejected]),
                        (Some(true), None) => ("confirmed ruling", vec![Fixed, Asked, Proposed]),
                        (None, None) => ("", vec![Fixed, Rejected, Asked, Proposed]),
                    };
                if let Some(outcome) = kept.outcome
                    && !allowed.contains(&outcome)
                {
                    return invalid(format!(
                        "finding {id} of {at} keeps the outcome {outcome:?}, which its {judgment} \
                         contradicts"
                    ));
                }
            }
        }
        if let Some(n) = (1..self.answered_reviews.len())
            .find(|&n| self.answered_reviews[..n].contains(&self.answered_reviews[n]))
        {
            return invalid(format!(
                "pull request review {} is answered twice",
                self.answered_reviews[n]
            ));
        }
        // The loop under way answers each review once, none already
        // answered, each as a Build brief would carry it (OWL-307).
        for (n, review) in self.answering.iter().enumerate() {
            let broken = if self.answering[..n].iter().any(|seen| seen.id == review.id) {
                Some("is answered twice by the loop under way")
            } else if self.answered_reviews.contains(&review.id) {
                Some("is under way although answered")
            } else {
                pull_request_review_problem(review)
            };
            if let Some(broken) = broken {
                return invalid(format!("pull request review {} {broken}", review.id));
            }
        }
        // Each review relayed once, neither answered nor under way, each
        // question asked about one review (OWL-585).
        for (n, relayed) in self.relayed.iter().enumerate() {
            let review = &relayed.review;
            let earlier = &self.relayed[..n];
            let broken = if earlier.iter().any(|seen| seen.review.id == review.id) {
                Some("is relayed twice")
            } else if earlier
                .iter()
                .any(|seen| (seen.round, &seen.question) == (relayed.round, &relayed.question))
            {
                Some("is relayed by a question that asks about another")
            } else if self.answered_reviews.contains(&review.id) {
                Some("is relayed although answered")
            } else if self.answering.iter().any(|under| under.id == review.id) {
                Some("is relayed although under way")
            } else {
                pull_request_review_problem(review)
            };
            if let Some(broken) = broken {
                return invalid(format!("pull request review {} {broken}", review.id));
            }
        }
        if let Some(build) = &self.build
            && !is_commit_id(&build.commit)
        {
            return invalid(format!(
                "the kept build's commit {:?} is not a full commit id",
                build.commit
            ));
        }
        if let Some(pass) = self.last_fix
            && !self.passes.last().is_some_and(|last| last.pass == pass)
        {
            return invalid(format!(
                "the last fix names pass {pass}, which is not the record's last pass"
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    fn plan() -> Value {
        json!({
            "goal": "G",
            "criteria": ["C1"],
            "approach": "A",
            "alternatives": [{ "approach": "B", "reason": "slower" }],
            "files": ["src/lib.rs"],
            "steps": [{ "what": "S1", "test": "T1" }],
            "risks": ["R"],
            "out_of_scope": ["O"],
            "forks": [{ "choice": "X", "options": ["Y"], "reason": "simpler" }]
        })
    }

    /// A `plan.json` whose plan holds a person's step before the merge, the
    /// agent's step after it owed, and a person's step after the merge
    /// (OWL-619).
    fn ticket_plan() -> Value {
        let mut plan = plan();
        plan["steps"] = json!([
            { "what": "S1", "test": "T1" },
            { "what": "Check it in a browser.", "test": "It shows.", "by": "person",
              "when": "before_merge" },
            { "what": "S3", "test": "T3" },
            { "what": "Check it deployed.", "test": "It serves.", "by": "person",
              "when": "after_merge" }
        ]);
        json!({
            "format": 3,
            "owed": [3],
            "checks": [{ "step": 2, "round": 1, "question": "Q1" }],
            "plan": plan,
            "text_sha256": "a".repeat(64),
            "base": "b".repeat(40),
            "approval": { "comment": "c-1", "plan_sha256": "d".repeat(64) },
            "published": {
                "document": "doc-1", "url": "https://linear.app/o/document/plan-1",
                "version": 2, "pass": 1, "plan_sha256": "e".repeat(64),
                "parts": vec!["f".repeat(64); 9],
                "change": { "kind": "changed", "since": 1, "parts": ["steps", "out_of_scope"] }
            }
        })
    }

    fn finding(id: &str) -> Value {
        json!({
            "id": id, "severity": "major", "claim": "C", "evidence": "src/lib.rs:1",
            "criterion": "C1", "settle": "S"
        })
    }

    fn ticket_review() -> Value {
        json!({
            "format": 2,
            "relayed": [{
                "round": 2, "question": "Q1", "answer": "apply",
                "review": { "id": 11, "author": { "name": "bob", "relation": "other" }, "body": "Rename it." }
            }],
            "passes": [{
                "stage": "design_review",
                "pass": 1,
                "findings": [{
                    "finding": finding("F1"),
                    "reviewer": {
                        "role": "design_review", "mandates": ["critic", "conformance"],
                        "harness": "claude", "model": "m", "stand_in": true
                    },
                    "ruling": { "outcome": "confirmed", "reason": "seen at src/lib.rs:1" },
                    "outcome": "fixed"
                }],
                "reviewers": [{
                    "role": "design_review", "mandates": ["critic", "conformance"],
                    "harness": "claude", "model": "m", "same_model": true
                }]
            }],
            "answered_reviews": [7, 9],
            "build": {
                "commit": "e".repeat(40),
                "summary": "Added the file.",
                "pr": { "branch": "owlshift/demo-1", "title": "T", "body": "B" },
                "gate": ["make test"],
                "notes": [{ "question": "Q", "decision": "D", "basis": "B" }],
                "followups": [{
                    "title": "T", "why": "W", "source": "reviewer", "done_when": "D"
                }],
                "seen": "2026-10-16T09:00:00Z"
            }
        })
    }

    /// An edit of a document, and what its refusal says.
    type Case<'a> = (&'a dyn Fn(&mut Value), &'a str);

    /// Each edit of `base` is refused by `parse` with an error holding its
    /// reason.
    fn refuses<T: std::fmt::Debug>(
        base: fn() -> Value,
        parse: fn(&str) -> Result<T, ContractError>,
        cases: &[Case<'_>],
    ) {
        for (edit, reason) in cases {
            let mut document = base();
            edit(&mut document);
            let error = parse(&document.to_string()).unwrap_err().to_string();
            assert!(error.contains(reason), "{reason}: {error}");
        }
    }

    #[test]
    fn a_ticket_plan_round_trips_and_refuses_what_it_must() {
        let parsed = TicketPlan::parse(&ticket_plan().to_string()).unwrap();
        assert_eq!(TicketPlan::parse(&parsed.render()).unwrap(), parsed);
        assert!(
            parsed
                .plan
                .canonical()
                .starts_with(r#"{"goal":"G","criteria":["C1"]"#)
        );
        assert!(parsed.plan.has_agent_step());
        // A plan of a person's steps alone is kept: its ticket is handed
        // back, not refused (OWL-619's third part).
        let mut whole = ticket_plan();
        for n in [0, 2] {
            whole["plan"]["steps"][n]["by"] = "person".into();
            whole["plan"]["steps"][n]["when"] = "before_merge".into();
        }
        whole["owed"] = json!([]);
        let whole = TicketPlan::parse(&whole.to_string()).unwrap();
        assert!(!whole.plan.has_agent_step());
        refuses(
            ticket_plan,
            TicketPlan::parse,
            &[
                (
                    &|d| d["plan"]["criteria"] = json!([]),
                    "no success criterion",
                ),
                (&|d| d["plan"]["steps"] = json!([]), "has no step"),
                (
                    &|d| d["plan"]["forks"][0]["options"] = json!([]),
                    "names no option",
                ),
                (
                    &|d| d["plan"]["steps"][0]["test"] = " ".into(),
                    "steps[0].test is blank",
                ),
                (&|d| d["plan"]["files"][0] = "".into(), "files[0] is blank"),
                (
                    &|d| d["plan"]["risks"] = json!(["r".repeat(MAX_PLAN_BYTES)]),
                    "past the 65536",
                ),
                (&|d| d["text_sha256"] = "A".repeat(64).into(), "text_sha256"),
                (&|d| d["base"] = "main".into(), "not a full commit id"),
                (&|d| d["approval"]["comment"] = " c".into(), "has a space"),
                (
                    &|d| d["approval"]["plan_sha256"] = "d".into(),
                    "plan_sha256",
                ),
                (&|d| d["plan"]["extra"] = 1.into(), "unknown field"),
                (&|d| d["format"] = 5.into(), "upgrade Owlshift"),
                // A person's step, its `when`, and the steps owed (OWL-619).
                (
                    &|d| d["plan"]["steps"][1]["by"] = "robot".into(),
                    "unknown variant",
                ),
                (
                    &|d| d["plan"]["steps"][1]["when"] = Value::Null,
                    "steps[1] is a person's but says no `when`",
                ),
                (
                    &|d| d["plan"]["steps"][0]["when"] = "after_merge".into(),
                    "steps[0] is the agent's but says a `when`",
                ),
                (
                    &|d| {
                        d["plan"]["steps"][1]["when"] = "after_merge".into();
                        d["plan"]["steps"][3]["when"] = "before_merge".into();
                    },
                    "steps[3] is due before the merge but follows steps[1]",
                ),
                (&|d| d["owed"] = json!([3, 3]), "owed: step 3 is repeated"),
                (&|d| d["owed"] = json!([5]), "owed: step 5 is not a step"),
                (&|d| d["owed"] = json!([2]), "owed: step 2 is a person's"),
                (&|d| d["owed"] = json!([1]), "owed: step 1 comes before"),
                // The checks of a person's steps asked at Watch.
                (
                    &|d| d["checks"][0]["step"] = 4.into(),
                    "checks: step 4 is not a person's step due before the merge",
                ),
                (
                    &|d| d["checks"][0]["step"] = 3.into(),
                    "checks: step 3 is not a person's step",
                ),
                (
                    &|d| d["checks"][0]["answer"] = "maybe".into(),
                    "unknown variant",
                ),
                (
                    &|d| {
                        let check = d["checks"][0].clone();
                        d["checks"].as_array_mut().unwrap().push(check);
                    },
                    "checks: step 2 is repeated",
                ),
                // Format 2 keeps no check; format 1 holds no person's step
                // and owes nothing.
                (&|d| d["format"] = 2.into(), "format 2 records no checks"),
                (&|d| d["format"] = 1.into(), "format 1 records no owed"),
                (
                    &|d| {
                        d["format"] = 1.into();
                        d.as_object_mut().unwrap().remove("owed");
                        d.as_object_mut().unwrap().remove("checks");
                    },
                    "format 1 records no person's step, but steps[1] says one",
                ),
                (&|d| d["published"]["document"] = "".into(), "is blank"),
                (
                    &|d| d["published"]["parts"] = json!(["f"]),
                    "parts must be 9",
                ),
                (
                    &|d| d["published"]["change"]["since"] = 2.into(),
                    "version 2 changed since 2",
                ),
                (
                    &|d| d["published"]["change"]["parts"] = json!(["goal", "goal"]),
                    "once each",
                ),
                (
                    &|d| d["published"]["change"] = json!({ "kind": "first" }),
                    "not a first one",
                ),
            ],
        );
        // A plan published unchanged, and one never published.
        let mut unchanged = ticket_plan();
        unchanged["published"]["change"] = json!({ "kind": "unchanged" });
        TicketPlan::parse(&unchanged.to_string()).unwrap();
        let mut never = ticket_plan();
        never.as_object_mut().unwrap().remove("published");
        assert!(
            !TicketPlan::parse(&never.to_string())
                .unwrap()
                .render()
                .contains("published")
        );
        // The steps owed: the person's two, and step 3, which takes the
        // `when` of step 2 before it (OWL-619).
        let owed: Vec<(u32, StepWhen)> = parsed
            .owed_steps()
            .iter()
            .map(|owed| (owed.number.get(), owed.when))
            .collect();
        assert_eq!(
            owed,
            [
                (2, StepWhen::BeforeMerge),
                (3, StepWhen::BeforeMerge),
                (4, StepWhen::AfterMerge)
            ]
        );
        assert_eq!(parsed.owed_before_merge(), 2);
    }

    /// A plan of the agent's steps alone renders as it did before a step
    /// could be a person's, so every hash over it is unchanged; a format-1
    /// `plan.json` is read as format 2 and written as such (OWL-619).
    #[test]
    fn an_agent_s_step_renders_as_before_and_format_1_is_read() {
        let mut older = ticket_plan();
        older["format"] = 1.into();
        older["plan"] = plan();
        older.as_object_mut().unwrap().remove("owed");
        older.as_object_mut().unwrap().remove("checks");
        let read = TicketPlan::parse(&older.to_string()).unwrap();
        assert!(read.owed_steps().is_empty());
        assert_eq!(read.owed_before_merge(), 0);
        assert!(read.render().contains("\"format\": 4"), "{}", read.render());
        // Format 2, written before a round asked a person's steps, is read
        // with no check.
        let mut second = ticket_plan();
        second["format"] = 2.into();
        second.as_object_mut().unwrap().remove("checks");
        let unchecked = TicketPlan::parse(&second.to_string()).unwrap();
        assert!(unchecked.checks.is_empty());
        assert_eq!(unchecked.owed_before_merge(), 2);
        assert_eq!(
            read.plan.part_canonical(PlanPart::Steps),
            r#"[{"what":"S1","test":"T1"}]"#
        );
        let person = PlanStep::person("Check it.", "It shows.", StepWhen::BeforeMerge);
        assert_eq!(
            serde_json::to_string(&person).unwrap(),
            r#"{"what":"Check it.","test":"It shows.","by":"person","when":"before_merge"}"#
        );
    }

    /// The life of a person's steps due before the merge (OWL-619): asked,
    /// answered, the run that follows the answers, and the hold it lifts. A
    /// defect never releases the agent's step after it, and a step answered
    /// as passed is never asked again. Whatever is still owed, a round or a
    /// run is due.
    #[test]
    fn a_person_s_steps_are_asked_answered_and_settled_by_their_run() {
        let n = |n: u32| NonZeroU32::new(n).unwrap();
        let mut document = ticket_plan();
        document["plan"]["steps"] = json!([
            { "what": "S1", "test": "T1" },
            { "what": "Check it.", "test": "It shows.", "by": "person", "when": "before_merge" },
            { "what": "S3", "test": "T3" },
            { "what": "Check more.", "test": "It holds.", "by": "person", "when": "before_merge" },
            { "what": "S5", "test": "T5" },
            { "what": "Check it deployed.", "test": "It serves.", "by": "person",
              "when": "after_merge" }
        ]);
        document["owed"] = json!([3, 5]);
        document.as_object_mut().unwrap().remove("checks");
        let mut plan = TicketPlan::parse(&document.to_string()).unwrap();
        // Something is due whenever something is owed before the merge.
        let due = |plan: &TicketPlan| !plan.steps_to_ask().is_empty() || plan.steps_run().is_some();
        let asked = |plan: &TicketPlan| -> Vec<u32> {
            plan.steps_to_ask().iter().map(|(n, _)| n.get()).collect()
        };
        assert_eq!((asked(&plan), plan.owed_before_merge()), (vec![2, 4], 4));
        assert!(plan.steps_run().is_none());

        let round = n(1);
        plan.ask_steps(
            round,
            &[(n(2), QuestionId::nth(1)), (n(4), QuestionId::nth(2))],
        );
        TicketPlan::parse(&plan.render()).unwrap();
        // Step 2 passes, step 4 reports a defect: the run fixes 4 and builds
        // 3, whose person's step passed, not 5, which waits on 4.
        let answers = |id: &QuestionId| match id.as_str() {
            "Q1" => Some(StepAnswer::Passed),
            _ => Some(StepAnswer::Defect),
        };
        assert!(plan.answer_round(round, answers));
        assert!(!plan.answer_round(round, answers), "answered once");
        let run = plan.steps_run().unwrap();
        assert_eq!((run.defects, run.build), (vec![n(4)], vec![n(3)]));
        assert_eq!(run.answered.len(), 2);
        assert_eq!(plan.owed_before_merge(), 3);
        assert!(asked(&plan).is_empty(), "a defect's run comes first");
        plan.settle_steps_run();
        assert_eq!(plan.owed, [n(5)]);
        assert_eq!((asked(&plan), plan.owed_before_merge()), (vec![4], 2));
        assert!(plan.steps_run().is_none());
        assert!(due(&plan));

        // Asked again, step 4 passes: the run builds step 5, and nothing is
        // owed once it lands; step 2 was never asked again.
        let round = n(2);
        plan.ask_steps(round, &[(n(4), QuestionId::nth(1))]);
        plan.answer_round(round, |_| Some(StepAnswer::Passed));
        let run = plan.steps_run().unwrap();
        assert_eq!((run.defects, run.build), (vec![], vec![n(5)]));
        plan.settle_steps_run();
        assert_eq!(
            (plan.owed_before_merge(), plan.owed.as_slice()),
            (0, &[][..])
        );
        assert!(!due(&plan));
        let rounds: Vec<(u32, u32)> = plan
            .checks
            .iter()
            .map(|check| (check.step.get(), check.round.get()))
            .collect();
        assert_eq!(rounds, [(2, 1), (4, 2)]);
        // A check an ask left unanswered is asked again, in its place.
        plan.checks[1].answer = None;
        assert_eq!(asked(&plan), [4]);
        TicketPlan::parse(&plan.render()).unwrap();
    }

    #[test]
    fn a_ticket_review_round_trips_and_refuses_what_it_must() {
        let parsed = TicketReview::parse(&ticket_review().to_string()).unwrap();
        assert_eq!(TicketReview::parse(&parsed.render()).unwrap(), parsed);
        // OWL-347: a reviewer written before it may say `stand_in`, read and
        // ignored, never written again.
        assert!(!parsed.render().contains("stand_in"), "{}", parsed.render());
        refuses(
            ticket_review,
            TicketReview::parse,
            &[
                (
                    &|d| d["passes"][0]["stage"] = "build".into(),
                    "not at design review",
                ),
                (
                    &|d| d["passes"][0]["findings"][0]["reviewer"]["role"] = "verify".into(),
                    "names a reviewer of role verify",
                ),
                (
                    &|d| d["passes"][0]["findings"][0]["reviewer"]["mandates"] = json!([]),
                    "no review mandate",
                ),
                (
                    &|d| {
                        d["passes"][0]["findings"][0]["reviewer"]["mandates"] =
                            json!(["debt", "debt"])
                    },
                    "given twice",
                ),
                (
                    &|d| d["passes"][0]["findings"][0]["reviewer"]["model"] = "".into(),
                    "is blank",
                ),
                (
                    &|d| d["passes"][0]["reviewers"][0]["standing"] = true.into(),
                    "unknown field `standing`",
                ),
                (
                    &|d| {
                        d["passes"][0]["findings"][0]["ruling"]["outcome"] = "rejected".into();
                        d["passes"][0]["findings"][0]["response"] =
                            json!({ "outcome": "fixed", "reason": "r" })
                    },
                    "a response to a rejected ruling",
                ),
                (
                    &|d| {
                        d["passes"][0]["findings"][0]["response"] =
                            json!({ "outcome": "rejected", "reason": "r" })
                    },
                    "keeps the response Rejected: the arbiter confirmed it",
                ),
                (
                    &|d| {
                        d["passes"][0]["findings"][0]["response"] =
                            json!({ "outcome": "deferred", "reason": "r" })
                    },
                    "keeps the response Deferred: only a minor finding",
                ),
                (
                    &|d| d["passes"][0]["revision"] = "run-1".into(),
                    "keeps a revision, which no reviewer reviewed, with reviewers",
                ),
                (
                    &|d| {
                        d["passes"] = json!([{
                            "stage": "verify", "pass": 1, "revision": "run-1"
                        }])
                    },
                    "keeps a revision",
                ),
                (&|d| d["last_fix"] = 2.into(), "not the record's last pass"),
                (
                    &|d| d["passes"][0]["findings"][0]["ruling"]["reason"] = " ".into(),
                    "blank reason",
                ),
                (
                    &|d| {
                        let kept = d["passes"][0]["findings"][0].clone();
                        d["passes"][0]["findings"] = json!([kept.clone(), kept]);
                    },
                    "repeated or out of order",
                ),
                (
                    &|d| d["passes"][0]["findings"][0]["finding"]["claim"] = "".into(),
                    "blank claim",
                ),
                (&|d| d["answered_reviews"] = json!([7, 7]), "answered twice"),
                (
                    &|d| d["passes"][0]["reviewers"][0]["role"] = "verify".into(),
                    "names a reviewer of role verify",
                ),
                (
                    &|d| d["passes"][0]["reviewers"][0]["model"] = " ".into(),
                    "the model of a reviewer",
                ),
                (
                    &|d| d["build"]["commit"] = "main".into(),
                    "not a full commit id",
                ),
                (&|d| d["build"]["extra"] = 1.into(), "unknown field"),
                (&|d| d["passes"][0]["pass"] = 0.into(), "nonzero"),
                (&|d| d["format"] = 3.into(), "upgrade Owlshift"),
                // A colleague's review relayed (OWL-585).
                (
                    &|d| {
                        let kept = d["relayed"][0].clone();
                        d["relayed"] = json!([kept.clone(), kept]);
                    },
                    "pull request review 11 is relayed twice",
                ),
                (
                    &|d| {
                        let mut other = d["relayed"][0].clone();
                        other["review"]["id"] = 12.into();
                        d["relayed"].as_array_mut().unwrap().push(other);
                    },
                    "pull request review 12 is relayed by a question that asks about another",
                ),
                (
                    &|d| d["answered_reviews"] = json!([7, 11]),
                    "pull request review 11 is relayed although answered",
                ),
                (
                    &|d| d["answering"] = json!([d["relayed"][0]["review"].clone()]),
                    "pull request review 11 is relayed although under way",
                ),
                (
                    &|d| d["relayed"][0]["review"]["body"] = "".into(),
                    "has neither a body nor a comment",
                ),
                (
                    &|d| d["relayed"][0]["answer"] = "set_aside".into(),
                    "unknown variant `set_aside`",
                ),
                (&|d| d["format"] = 1.into(), "format 1 records no relayed"),
            ],
        );
        // Format 1, before a review was relayed (OWL-585): read as format 2.
        let mut older = ticket_review();
        older["format"] = 1.into();
        older.as_object_mut().unwrap().remove("relayed");
        let read = TicketReview::parse(&older.to_string()).unwrap();
        assert!(read.relayed.is_empty());
        assert!(read.render().starts_with("{\n  \"format\": 2,"));
        assert_eq!(parsed.relayed[0].answer, Some(RelayAnswer::Apply));
    }

    /// The fixture's finding with `judgment` (`ruling` or `response`) of
    /// `outcome`, kept with the outcome `kept`, parsed.
    fn settled(judgment: &str, outcome: &str, kept: &str) -> Result<TicketReview, String> {
        let mut document = ticket_review();
        let finding = &mut document["passes"][0]["findings"][0];
        finding.as_object_mut().unwrap().remove("ruling");
        finding[judgment] = json!({ "outcome": outcome, "reason": "r" });
        finding["outcome"] = kept.into();
        TicketReview::parse(&document.to_string()).map_err(|e| e.to_string())
    }

    #[test]
    fn a_rejected_ruling_gives_only_rejected() {
        settled("ruling", "rejected", "rejected").unwrap();
        for kept in ["fixed", "asked", "proposed"] {
            let error = settled("ruling", "rejected", kept).unwrap_err();
            assert!(
                error.contains("its rejected ruling contradicts"),
                "{kept}: {error}"
            );
        }
    }

    #[test]
    fn a_confirmed_ruling_never_gives_rejected() {
        for kept in ["fixed", "asked", "proposed"] {
            settled("ruling", "confirmed", kept).unwrap_or_else(|e| panic!("{kept}: {e}"));
        }
        let error = settled("ruling", "confirmed", "rejected").unwrap_err();
        assert!(
            error.contains("its confirmed ruling contradicts"),
            "{error}"
        );
    }

    #[test]
    fn a_fixed_response_gives_only_fixed() {
        settled("response", "fixed", "fixed").unwrap();
        for kept in ["rejected", "asked", "proposed"] {
            let error = settled("response", "fixed", kept).unwrap_err();
            assert!(
                error.contains("its response contradicts"),
                "{kept}: {error}"
            );
        }
    }

    #[test]
    fn a_rejected_response_gives_only_rejected() {
        settled("response", "rejected", "rejected").unwrap();
        for kept in ["fixed", "asked", "proposed"] {
            let error = settled("response", "rejected", kept).unwrap_err();
            assert!(
                error.contains("its response contradicts"),
                "{kept}: {error}"
            );
        }
        // With no ruling, author mode: a disagreement is a rejection.
        let error = settled("response", "disagreed", "disagreed").unwrap_err();
        assert!(error.contains("no one judged it"), "{error}");
    }

    /// The author's answer to a finding the arbiter confirmed (OWL-377):
    /// kept beside the ruling, the finding's outcome its own; a `minor` on
    /// no criterion may be deferred.
    #[test]
    fn a_confirmed_finding_keeps_its_authors_answer() {
        let answered = |response: &str, kept: &str, minor: bool| {
            let mut document = ticket_review();
            let finding = &mut document["passes"][0]["findings"][0];
            finding["response"] = json!({ "outcome": response, "reason": "kept, see a.rs:2" });
            finding["outcome"] = kept.into();
            if minor {
                finding["finding"]["severity"] = "minor".into();
                finding["finding"]
                    .as_object_mut()
                    .unwrap()
                    .remove("criterion");
            }
            TicketReview::parse(&document.to_string()).map_err(|e| e.to_string())
        };
        let kept = answered("disagreed", "disagreed", false).unwrap();
        let finding = &kept.passes[0].findings[0];
        assert_eq!(finding.outcome, Some(FindingOutcome::Disagreed));
        assert!(finding.ruling.is_some());
        answered("fixed", "fixed", false).unwrap();
        answered("deferred", "deferred", true).unwrap();
        let error = answered("disagreed", "fixed", false).unwrap_err();
        assert!(error.contains("its response contradicts"), "{error}");
    }

    /// Design's last revision is kept as a pass no reviewer ran (OWL-377),
    /// naming its design run, and the last fix may name it.
    #[test]
    fn the_last_revision_is_a_pass_with_no_reviewer() {
        let mut document = ticket_review();
        document["passes"]
            .as_array_mut()
            .unwrap()
            .push(json!({ "stage": "design_review", "pass": 2, "revision": "run-7" }));
        document["last_fix"] = 2.into();
        let kept = TicketReview::parse(&document.to_string()).unwrap();
        assert_eq!(kept.passes[1].revision.as_deref(), Some("run-7"));
        assert_eq!(TicketReview::parse(&kept.render()).unwrap(), kept);
        document["passes"][1]["revision"] = " ".into();
        let error = TicketReview::parse(&document.to_string()).unwrap_err();
        assert!(error.to_string().contains("the revision of"), "{error}");
    }

    /// OWL-348: a pass keeps the opt-in Codex reviewer it ran without, with
    /// the runner's reason, never beside a Codex reviewer it names.
    #[test]
    fn a_pass_keeps_the_codex_reviewer_it_dropped_and_why() {
        let dropped = |reason: &str, codex_reviewer: bool| {
            let mut document = ticket_review();
            document["passes"][0]["dropped"] =
                json!({ "harness": "codex", "mandates": ["critic"], "reason": reason });
            if codex_reviewer {
                document["passes"][0]["reviewers"][0]["harness"] = "codex".into();
            }
            TicketReview::parse(&document.to_string()).map_err(|e| e.to_string())
        };
        let kept = dropped("Codex reached its usage limit.", false).unwrap();
        let reviewer = kept.passes[0].dropped.as_ref().unwrap();
        assert_eq!(
            (reviewer.harness, reviewer.mandates.as_slice()),
            (Harness::Codex, &[Mandate::Critic][..])
        );
        assert_eq!(TicketReview::parse(&kept.render()).unwrap(), kept);
        let long = "x".repeat(MAX_DROPPED_REASON_CHARS + 1);
        for reason in [" ", long.as_str()] {
            let error = dropped(reason, false).unwrap_err();
            assert!(error.contains("blank or longer"), "{error}");
        }
        let error = dropped("Codex reached its usage limit.", true).unwrap_err();
        assert!(error.contains("names one"), "{error}");
    }

    #[test]
    fn passes_count_from_1_by_1_within_a_run_at_one_stage() {
        let review = |passes: &[(&str, u32)]| {
            let mut document = ticket_review();
            let pass = document["passes"][0].clone();
            document["passes"] = passes
                .iter()
                .map(|&(stage, n)| {
                    let mut pass = pass.clone();
                    pass["stage"] = stage.into();
                    pass["pass"] = n.into();
                    for kept in pass["findings"].as_array_mut().unwrap() {
                        kept["reviewer"]["role"] = stage.into();
                    }
                    for reviewer in pass["reviewers"].as_array_mut().unwrap() {
                        reviewer["role"] = stage.into();
                    }
                    pass
                })
                .collect();
            TicketReview::parse(&document.to_string()).map_err(|e| e.to_string())
        };
        // Passes rise by 1; a review loop starts Verify over at 1; Design
        // review may follow Verify after a `replan`.
        review(&[
            ("design_review", 1),
            ("design_review", 2),
            ("design_review", 3),
            ("verify", 1),
            ("verify", 2),
            ("verify", 1),
            ("design_review", 1),
        ])
        .unwrap();
        for (passes, reason) in [
            (&[("design_review", 2)][..], "follows another stage's pass"),
            (
                &[("design_review", 1), ("verify", 2)][..],
                "follows another stage's pass",
            ),
            (&[("verify", 1), ("verify", 3)][..], "follows pass 1"),
            (
                &[("verify", 2), ("verify", 2)][..],
                "follows another stage's pass",
            ),
        ] {
            let error = review(passes).unwrap_err();
            assert!(error.contains(reason), "{passes:?}: {error}");
        }
    }

    #[test]
    fn findings_of_a_result_are_numbered_and_bounded() {
        let parse = |findings: Value| -> Vec<Finding> { serde_json::from_value(findings).unwrap() };
        let ordered = parse(json!([finding("F1"), finding("F2")]));
        check_findings("t", &ordered, MAX_FINDINGS, true).unwrap();
        let gap = parse(json!([finding("F1"), finding("F3")]));
        check_findings("t", &gap, MAX_FINDINGS, false).unwrap();
        let error = check_findings("t", &gap, MAX_FINDINGS, true).unwrap_err();
        assert!(error.to_string().contains("expected F2"), "{error}");
        let error = check_findings("t", &ordered, 1, true).unwrap_err();
        assert!(error.to_string().contains("past the 1"), "{error}");
        let mut heavy = finding("F1");
        heavy["claim"] = "c".repeat(MAX_FINDING_BYTES).into();
        let error = check_findings("t", &parse(json!([heavy])), MAX_FINDINGS, true).unwrap_err();
        assert!(error.to_string().contains("past the 8192"), "{error}");
    }

    /// OWL-620: a step the resolver handed to a person reads as a person's,
    /// due as handed, in what is owed and asked, the plan's text left as
    /// written; `hand` drops it from the agent's steps owed; a handed step
    /// is an agent's of the plan, once, in order; format 3 is read with none
    /// handed and refused with one.
    #[test]
    fn a_step_handed_to_a_person_is_one_and_the_plan_stays_as_written() {
        let mut plan = TicketPlan::parse(&ticket_plan().to_string()).unwrap();
        let written = plan.plan.clone();
        let step = |n: u32| NonZeroU32::new(n).unwrap();
        plan.hand(step(3), StepWhen::BeforeMerge);
        assert_eq!(plan.plan, written, "the text stays as written");
        assert!(plan.owed.is_empty(), "no longer the agent's to owe");
        let owed: Vec<(u32, bool)> = plan
            .owed_steps()
            .iter()
            .map(|owed| (owed.number.get(), owed.step.is_person()))
            .collect();
        assert_eq!(owed, [(2, true), (3, true), (4, true)]);
        let asked: Vec<u32> = plan.steps_to_ask().iter().map(|(n, _)| n.get()).collect();
        assert_eq!(asked, [2, 3], "the handed step is asked as a person's");
        // Handed again, it keeps its first `when`.
        plan.hand(step(3), StepWhen::AfterMerge);
        assert_eq!(
            plan.handed,
            [HandedStep {
                step: step(3),
                when: StepWhen::BeforeMerge
            }]
        );
        let rendered = plan.render();
        assert!(rendered.starts_with("{\n  \"format\": 4,"), "{rendered}");
        assert_eq!(TicketPlan::parse(&rendered).unwrap(), plan);

        let current = || {
            let mut plan = ticket_plan();
            plan["format"] = 4.into();
            plan
        };
        refuses(
            current,
            TicketPlan::parse,
            &[
                (
                    &|d| d["handed"] = json!([{ "step": 2, "when": "after_merge" }]),
                    "a person's already",
                ),
                (
                    &|d| d["handed"] = json!([{ "step": 9, "when": "after_merge" }]),
                    "not a step",
                ),
                (
                    &|d| {
                        d["handed"] = json!([
                            { "step": 3, "when": "after_merge" },
                            { "step": 1, "when": "after_merge" }
                        ]);
                    },
                    "repeated or out of order",
                ),
            ],
        );
        refuses(
            ticket_plan,
            TicketPlan::parse,
            &[(
                &|d| d["handed"] = json!([{ "step": 3, "when": "after_merge" }]),
                "format 3 records no handed",
            )],
        );
    }
}
