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
use crate::ids::{FindingId, check_name, is_commit_id, is_lower_hex};
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
    pub what: String,
    /// The test that shows the step done.
    pub test: String,
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
    /// The author fixes each, as confirmed by the arbiter: a design or
    /// build run.
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

/// The author's answer to one finding it was given, in author mode
/// (`pipeline.findings = "author"`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FindingResponse {
    pub finding: FindingId,
    pub outcome: ResponseOutcome,
    /// Why; not blank.
    pub reason: String,
}

/// What the author did with a finding.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ResponseOutcome {
    Fixed,
    Rejected,
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
    /// approval (OWL-343), its DECISION comment, kept with the decision in
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

    /// Parses and validates a `plan.json`.
    pub fn parse(input: &str) -> Result<Self, ContractError> {
        let plan: Self = format::parse_json(Self::CONTRACT, PLAN_FORMAT, input)?;
        plan.validate()?;
        Ok(plan)
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
        Ok(())
    }
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
}

impl From<ReviewerForm> for Reviewer {
    fn from(form: ReviewerForm) -> Self {
        Self {
            role: form.role,
            mandates: form.mandates,
            harness: form.harness,
            model: form.model,
            same_model: form.same_model,
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
    /// It went to the decider in a question round.
    Asked,
    /// It became a proposed follow-up.
    Proposed,
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
            build: None,
            last_fix: None,
        }
    }

    /// Parses and validates a `review.json`.
    pub fn parse(input: &str) -> Result<Self, ContractError> {
        let review: Self = format::parse_json(Self::CONTRACT, REVIEW_FORMAT, input)?;
        review.validate()?;
        Ok(review)
    }

    pub fn render(&self) -> String {
        format::render_json(self)
    }

    /// Checks each pass is at Design review or Verify, its findings keep a
    /// result's rules with ids increasing, each raised by a reviewer of the
    /// pass's role with its mandates distinct and a model named, and none
    /// both ruled on and responded to, nor given an outcome its ruling or
    /// response contradicts (a rejected ruling or response gives
    /// `rejected` only, a confirmed ruling anything but `rejected`, a fixed
    /// response `fixed` only); each reviewer a pass names is of its role,
    /// with its mandates distinct and a model named; each answered review is
    /// named once; each review under way once, not answered, with a body or
    /// a comment, each comment with a path and a body, within the bytes a
    /// brief gives it; the kept build's commit is a full commit id; a pass's
    /// `head` only at Verify, a full commit id, and its plan print only at
    /// Design review, 9 part hashes and a hash per step in lowercase hex;
    /// and `last_fix` names the record's last pass, at Verify (OWL-356).
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
                if kept.ruling.is_some() && kept.response.is_some() {
                    return invalid(format!(
                        "finding {id} of {at} keeps both a ruling and a response"
                    ));
                }
                let reason = kept
                    .ruling
                    .as_ref()
                    .map(|r| &r.reason)
                    .or(kept.response.as_ref().map(|r| &r.reason));
                if reason.is_some_and(|r| r.trim().is_empty()) {
                    return invalid(format!("finding {id} of {at} keeps a blank reason"));
                }
                use FindingOutcome::{Asked, Fixed, Proposed, Rejected};
                let (judgment, allowed): (&str, &[FindingOutcome]) =
                    match (&kept.ruling, &kept.response) {
                        (Some(ruling), _) if ruling.outcome == RulingOutcome::Rejected => {
                            ("rejected ruling", &[Rejected])
                        }
                        (Some(_), _) => ("confirmed ruling", &[Fixed, Asked, Proposed]),
                        (_, Some(response)) if response.outcome == ResponseOutcome::Rejected => {
                            ("rejected response", &[Rejected])
                        }
                        (_, Some(_)) => ("fixed response", &[Fixed]),
                        (None, None) => ("", &[Fixed, Rejected, Asked, Proposed]),
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
        if let Some(build) = &self.build
            && !is_commit_id(&build.commit)
        {
            return invalid(format!(
                "the kept build's commit {:?} is not a full commit id",
                build.commit
            ));
        }
        if let Some(pass) = self.last_fix
            && !self
                .passes
                .last()
                .is_some_and(|last| last.stage == Stage::Verify && last.pass == pass)
        {
            return invalid(format!(
                "the last fix names verify pass {pass}, which is not the record's last pass"
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

    fn ticket_plan() -> Value {
        json!({
            "format": 1,
            "plan": plan(),
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
            "format": 1,
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
                (&|d| d["format"] = 2.into(), "upgrade Owlshift"),
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
                        d["passes"][0]["findings"][0]["response"] =
                            json!({ "outcome": "fixed", "reason": "r" })
                    },
                    "both a ruling and a response",
                ),
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
                (&|d| d["format"] = 2.into(), "upgrade Owlshift"),
            ],
        );
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
                error.contains("its fixed response contradicts"),
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
                error.contains("its rejected response contradicts"),
                "{kept}: {error}"
            );
        }
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
}
