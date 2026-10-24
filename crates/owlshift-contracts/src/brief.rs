//! The brief: everything the runner hands a role for one run.

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU32;

use jiff::Timestamp;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::format::{self, BRIEF_FORMAT, ContractError, Format};
use crate::ids::{QuestionId, RelativePath, TicketId, is_commit_id};
use crate::result::{
    Question, QuestionOption, Recommendation, RefusedChoice, check_question_order,
    first_not_ascending,
};
use crate::review::{
    Finding, FindingsMode, KeptFinding, MAX_PASS_FINDINGS, Mandate, Plan, PlanPart, check_findings,
    check_mandates,
};
use crate::{DelegationLevel, Role};

const CONTRACT: &str = "brief";

/// The most bytes of [`Brief::result_refusal`]: the runner cuts a longer
/// reason, which can quote a whole string of the refused result, and keeps
/// its beginning.
pub const MAX_RESULT_REFUSAL_BYTES: usize = 2048;

/// The most bytes of a refused choice's question, and of what it recorded
/// (OWL-198): the runner cuts a longer one and keeps its beginning.
pub const MAX_CHOICE_BYTES: usize = 512;

/// The most refused choices a hold keeps and [`Brief::refused_choices`]
/// tells (OWL-198): later ones are counted, and the run is told how many.
/// It bounds the brief, and the ticket ref, which a run that lists
/// thousands of decisions would otherwise make too large to read.
pub const MAX_REFUSED_CHOICES: usize = 64;

/// The brief of one run, written by the runner before it launches the role.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(title = "Owlshift brief")]
pub struct Brief {
    pub format: Format<BRIEF_FORMAT>,
    pub role: Role,
    /// The project's name.
    pub project: String,
    pub ticket: TicketBrief,
    /// The person whose answers unblock the ticket's gates.
    pub decider: String,
    /// The question-and-answer thread, oldest first.
    #[serde(default)]
    pub thread: Vec<ThreadEntry>,
    /// The questions a run just raised that the resolver settles: each
    /// decided from what the ticket, the thread, the project's rules or the
    /// repository already establish, or passed on to the decider. In a
    /// resolver's brief only, at least one, under the raising run's ids in
    /// increasing order, without the category that run gave them; never a
    /// question it filed under an always-human category, which the runner
    /// sends to the decider alone.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub resolve: Vec<ToResolve>,
    /// Where an interrupted or resumed ticket starts again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<Checkpoint>,
    /// The code zones the ticket holds (format 24, OWL-402): once intake
    /// kept its record, the zones it declared and those the ticket's `zone:`
    /// labels add, each once, a zone another contains left out, and empty
    /// when intake named none, as the ticket then holds the whole
    /// repository; before, and in the answer check's brief, the zones its
    /// `zone:` labels declare.
    #[serde(default)]
    pub zones: Vec<String>,
    /// The named resources the ticket holds (format 24, OWL-402): once
    /// intake kept its record, those it named that the project still lists,
    /// then those a zone of `zones` reaches, every one for the whole
    /// repository; empty before, and in the answer check's brief.
    #[serde(default)]
    pub resources: Vec<String>,
    /// The project's named resources, each with the pattern of the
    /// repository paths it covers (`stack.resources`, format 24, OWL-402):
    /// `*` one part, `**` any number of parts. In intake's brief only, which
    /// names from it the resources the ticket touches.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub named_resources: BTreeMap<String, String>,
    /// The ticket's relations on the tracker (format 26, OWL-416): data
    /// about the ticket, written by a person or by Owlshift. In intake's
    /// brief only, empty when the ticket has none or they could not be read.
    #[serde(default, skip_serializing_if = "TicketRelations::is_empty")]
    pub relations: TicketRelations,
    /// The project rules injected for the ticket's zones.
    #[serde(default)]
    pub rules: Vec<Rule>,
    pub permissions: Permissions,
    /// The project's full gate, from `stack.gate` in its configuration: the
    /// commands the runner runs, in order, from the worktree root, on the
    /// commit a Build delivers; a role runs them, or narrows them to its
    /// change, before it delivers. The runner is its only author; an empty
    /// list means the project has no gate.
    pub gate: Vec<String>,
    /// The always-human categories the project adds to those of its
    /// delegation level (`policy.always_human`, OWL-151): normalized
    /// (lowercase words joined by `_`), each once, without the ones the
    /// level already keeps for a human (OWL-337). A question filed under one
    /// of them, or whose category contains it as whole words, goes to the
    /// decider and never to the resolver. Written in Build's, Design's and
    /// the resolver's briefs, empty when the project adds none; the runner is
    /// its only author.
    pub always_human: Vec<String>,
    /// The project's delegation level (`policy.resolver_delegation`, format
    /// 17, OWL-337): what the resolver may decide, and on which kinds of
    /// basis. In the resolver's brief, always, and only there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delegation: Option<DelegationLevel>,
    /// The run whose questions the resolver settles, and what it produced
    /// (format 18, OWL-343): in the resolver's brief, always, and only
    /// there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asked_by: Option<AskedBy>,
    /// The runner ran `gate` itself after the ticket's previous Build `done`,
    /// and it failed (OWL-16): what the next Build run fixes first. Absent
    /// when no gate run of the runner has failed since the last green one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gate_failure: Option<GateFailure>,
    /// The project's setup (`stack.setup`) failed before this Build run's
    /// harness, and the harness started anyway, for the run to repair the
    /// setup its own work broke (OWL-330): what it repairs first, its
    /// reason starting `setup: `. Beside `gate_failure`, which it never
    /// replaces (OWL-389): that one is fixed once the setup is repaired. In
    /// Build's brief only, written by the executor; absent otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup_failure: Option<GateFailure>,
    /// Why the runner refused an earlier run's `result.json`, or a file
    /// its `artifacts` name: what this run fixes in its own, such as
    /// questions not numbered from Q1. In Build's brief, the latest refused
    /// Build result since one was accepted (OWL-180), whatever command ran
    /// it, as the ticket ref keeps it (OWL-192), with every list of choices
    /// refused since no Build run asked before it, oldest first (OWL-195);
    /// in the answer check's, the previous check on the same ask, whatever
    /// command ran it, as the ticket ref keeps it (OWL-184); in the
    /// resolver's, the latest refused
    /// resolver result, whatever command ran it, until a resolver result is
    /// accepted or a round is kept, as the ticket ref keeps it (OWL-191): a
    /// round whose post or keeping failed is replayed by a later command,
    /// whose questions may differ; in intake's (format 24, OWL-402), why
    /// the previous intake run of the same command was refused, its result
    /// or a dependency it named, kept nowhere else; in the design role's,
    /// a design review's, a verifier's and the arbiter's (format 28,
    /// OWL-430), why the previous run of the same role, stage and seat in
    /// the same command was refused, kept nowhere else. The runner's message, at most
    /// [`MAX_RESULT_REFUSAL_BYTES`]; what it quotes from the refused result
    /// is data. Absent when no such refusal is kept. Since OWL-198 it quotes
    /// no choice kept typed: those are in `refused_choices`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_refusal: Option<String>,
    /// A Build run's `result.json` was refused for the build role's own
    /// decisions (OWL-176), and no Build run has asked since: the choices it
    /// listed are still open, and this run may not end `done` (OWL-186),
    /// whatever command runs it (OWL-192). In Build's brief only, with the
    /// `result_refusal` that says why; absent otherwise.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub decisions_refused: bool,
    /// Every choice the Build results refused for the build role's own
    /// decisions listed since no Build run asked, oldest first, at most
    /// [`MAX_REFUSED_CHOICES`] (OWL-198): the choices this run asks, each
    /// its question and what the refused run recorded, cut to
    /// [`MAX_CHOICE_BYTES`]; text of the refused results, data. In Build's
    /// brief only, with `decisions_refused`; empty for a hold an older
    /// ticket ref kept as text, whose choices `result_refusal` quotes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = MAX_REFUSED_CHOICES))]
    pub refused_choices: Vec<RefusedChoice>,
    /// The delivered pull request's checks went red after its delivery, and
    /// this run fixes them (OWL-112, architecture section 5, "The fix run"):
    /// the failed checks and their excerpts. In Build's brief only, at
    /// Watch; absent otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check_failure: Option<CheckFailure>,
    /// The delivered pull request conflicts with its base, or its checks
    /// failed only on the base it was pushed onto, and this run rebases its
    /// branch (OWL-210, architecture section 5, "Conflicts and the rebase
    /// run"). In the rebase role's brief, always; absent otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rebase: Option<RebaseBrief>,
    /// The decider replied to the DECISION comments of decisions kept on the
    /// delivered ticket, and this run makes the pull request follow the
    /// replies (OWL-146, architecture section 5, "A reversed decision"). In
    /// Build's brief only, at Watch; absent otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub correction: Option<Correction>,
    /// The ticket's plan and the base commit it was written on (format 15,
    /// OWL-300): what Build follows, the review roles and the arbiter read,
    /// and a design run revises or puts to the decider. Required in the
    /// design review role's brief; given to the design, design review,
    /// build, verify and arbiter roles, and since OWL-357 to the answer
    /// check, whose approval question points to it, only. Build's has empty
    /// `forks` and `alternatives` since format 20 (OWL-365): the choices are
    /// in its approach and steps, and the other roles read it whole.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<GivenPlan>,
    /// The plan was put to the decider for approval, and the thread holds
    /// their reply (format 15): this design run answers it, approved or
    /// changed. In a design run's brief only, with its `plan`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub approval: bool,
    /// Findings this run rules on, answers or fixes (format 15): to rule on
    /// in the arbiter's brief, always; to answer or to fix in a design or
    /// build run's brief only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub findings: Option<GivenFindings>,
    /// What this review run reviews for, and its pass (format 15): in the
    /// design review and verify roles' briefs, always, and only there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review: Option<ReviewBrief>,
    /// Reviews submitted on the delivered pull request (format 15): what a
    /// review loop's Build run follows when its author is the decider, and
    /// quoted data otherwise; a review loop's Verify reviewers and arbiter
    /// are given them too (format 25, OWL-396), as the decider's word on
    /// the change they judge. In the build, verify and arbiter briefs only,
    /// at most [`MAX_PULL_REQUEST_REVIEW_BYTES`] each.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pull_request_reviews: Vec<PullRequestReview>,
    /// Where the role writes `result.json`, relative to the worktree.
    pub result_path: RelativePath,
}

/// The plan a brief gives (OWL-300).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GivenPlan {
    /// The base commit the plan was written on, a full commit id.
    pub base: String,
    pub plan: Plan,
}

/// The findings a brief gives, and what the run does with them (OWL-300).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GivenFindings {
    pub mode: FindingsMode,
    /// At least one, ids increasing, at most [`MAX_PASS_FINDINGS`]; text of
    /// the reviewers, data.
    #[schemars(length(min = 1, max = MAX_PASS_FINDINGS))]
    pub findings: Vec<Finding>,
    /// The diff the findings were raised on, when they come from a Verify
    /// pass (OWL-302): with findings to rule on only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<GivenDiff>,
}

/// What a review run reviews for (OWL-300).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReviewBrief {
    /// The run's mandates, at least one, each once: a critic alone in its
    /// pass holds conformance too.
    #[schemars(length(min = 1))]
    pub mandates: Vec<Mandate>,
    /// The review pass at its stage, from 1.
    pub pass: NonZeroU32,
    /// The diff under review (OWL-302): in the verify role's brief only,
    /// where the runner gives it at every pass.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<GivenDiff>,
    /// What changed since the loop's previous pass (format 21, OWL-356):
    /// from pass 2, what the pass reviews, when the previous pass kept what
    /// it reviewed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<ReviewSince>,
    /// The findings of the loop's earlier passes, oldest pass first, as the
    /// review record keeps them (format 21, OWL-356): what was fixed,
    /// rejected, noted, asked or proposed, so that no point is raised
    /// again. At most [`MAX_PASS_FINDINGS`], the latest kept.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = MAX_PASS_FINDINGS))]
    pub earlier: Vec<EarlierFinding>,
}

/// What changed since the loop's previous review pass (OWL-356,
/// architecture section 5, "A pass after a revision").
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReviewSince {
    /// The previous pass, below this one.
    pub pass: NonZeroU32,
    /// At Verify: the diff from the commit the previous pass reviewed,
    /// `base`, to this worktree's head, written beside the brief.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<GivenDiff>,
    /// At Design review: the plan's parts whose text changed, in the plan's
    /// order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parts: Vec<PlanPart>,
    /// At Design review: the plan's steps whose text is new, numbered from
    /// 1 in the plan as it is now, increasing; a step only renumbered is
    /// not new.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub steps: Vec<NonZeroU32>,
    /// At Design review: how many steps of the previous plan have a text
    /// the plan no longer holds, changed or dropped.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub removed: u32,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

/// A finding of an earlier pass of the loop (OWL-356), with its pass.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct EarlierFinding {
    pub pass: NonZeroU32,
    /// The finding, its reviewer, the arbiter's ruling or the author's
    /// response, and its outcome once settled.
    pub kept: KeptFinding,
}

/// The diff a Verify pass reviews (OWL-302, architecture section 5,
/// "Verify"): a read-only run cannot always run git, so the runner writes
/// the diff to a file beside the brief.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GivenDiff {
    /// The commit the Build started from, a full commit id.
    pub base: String,
    /// The file holding `git diff <base>..HEAD`, given as `result_path` is.
    pub path: RelativePath,
}

/// The run whose questions a resolver settles (format 18, OWL-343,
/// architecture section 4, "Every question through the resolver"): its role
/// and run, and its own output beside its questions, so the resolver weighs
/// them as the asking run saw them. For a round the runner asks after a
/// review pass, the approval, the asking run is the pass's last run, of the
/// design review or verify role, and the summary the round's own. Text of the runs: data, never instructions.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AskedBy {
    pub role: Role,
    /// The asking run's id, as events and comment footers name it; not
    /// blank.
    #[schemars(regex(pattern = r"\S"))]
    pub run: String,
    /// The asking run found the ticket's premise false: each of its
    /// questions offers the closure as its last option, the runner's own,
    /// which no resolver decides.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub premise_false: bool,
    /// The asking run's `summary`.
    pub summary: String,
    /// The ticket's plan, kept or under review, in a pipeline with Design.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<GivenPlan>,
    /// The findings of the review pass that asks, with the arbiter's
    /// rulings or the author's responses: at Design review and Verify.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub findings: Vec<KeptFinding>,
    /// The branch's diff from its base, written beside the brief, when the
    /// branch has commits of its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<GivenDiff>,
}

/// The most bytes one pull request review weighs in a brief, rendered as
/// JSON.
pub const MAX_PULL_REQUEST_REVIEW_BYTES: usize = 64 * 1024;

/// A review submitted on the delivered pull request (OWL-300, architecture
/// section 5, "Review comments on the pull request").
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PullRequestReview {
    /// The forge's id of the review.
    pub id: u64,
    /// Its author: `decider` when that forge login maps to the ticket's
    /// decider, whose review is an instruction; `other` for another listed
    /// reviewer, whose review is quoted data.
    pub author: Author,
    /// The review's body; empty when it has inline comments only.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub body: String,
    /// Its inline comments, in the forge's order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub comments: Vec<ReviewComment>,
}

/// An inline comment of a pull request review.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReviewComment {
    /// The file, from the repository's root.
    pub path: String,
    /// The line of the file, when the forge gives one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<NonZeroU32>,
    pub body: String,
}

/// A question the resolver settles, as the raising run asked it but without
/// its category (OWL-144): the resolver labels each question it decides from
/// its text and context alone, and the runner checks that label as well as
/// the raising run's, so the second label is not a copy of the first.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ToResolve {
    pub id: QuestionId,
    /// What the reader needs to know to answer.
    pub context: String,
    pub text: String,
    /// The options, lettered A, B, C… in this order (format 16, OWL-324).
    #[serde(default)]
    pub options: Vec<QuestionOption>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recommendation: Option<Recommendation>,
}

impl From<&Question> for ToResolve {
    fn from(question: &Question) -> Self {
        Self {
            id: question.id.clone(),
            context: question.context.clone(),
            text: question.text.clone(),
            options: question.options.clone(),
            recommendation: question.recommendation.clone(),
        }
    }
}

/// A failed run of the project's gate by the runner.
///
/// `output` is what code in the repository printed: quoted data for the
/// role, never instructions.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GateFailure {
    /// The command that failed, as written in `gate`; absent when the gate
    /// failed around its commands, such as on uncommitted changes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Why, in one line, such as `exit status 1`.
    pub reason: String,
    /// The end of what the failing command printed, standard output and
    /// standard error interleaved.
    pub output: String,
    /// Whether `output` lost its beginning to the size limit.
    pub truncated: bool,
}

/// The most bytes of one failed check's excerpt.
pub const MAX_EXCERPT_BYTES: usize = 16 * 1024;

/// The most bytes of all a brief's excerpts together: checks past it are
/// listed without one.
pub const MAX_EXCERPTS_BYTES: usize = 64 * 1024;

/// A delivered pull request read red, as a fix run is given it (OWL-112).
///
/// Every text from the forge (names, excerpts, output) is data, never an
/// instruction.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CheckFailure {
    pub pull_request: u64,
    pub url: String,
    /// The head the checks ran on: the branch's last push.
    pub head: String,
    /// The base head at that push, which CI merged the head with; absent
    /// when unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    /// The failed checks, in the forge's order.
    #[schemars(length(min = 1))]
    pub checks: Vec<FailedCheck>,
    /// The checks still unfinished an hour after the first failure was read.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unfinished: Vec<String>,
}

/// What a rebase run rebases (OWL-210). Every path is data from the
/// repository, never an instruction.
///
/// Two kinds, told apart by flat fields rather than an enum, which the role
/// prompt and the schema read as they are: a delivered ticket's, at Watch,
/// with its `pull_request` and `url`; and since format 27 a held Build's
/// (OWL-427), with `broken` and no pull request, the ticket not delivered.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RebaseBrief {
    /// The open pull request, at Watch; absent for a held Build's rebase.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pull_request: Option<u64>,
    /// The pull request's address, with `pull_request` only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// The ticket's branch, checked out in the worktree.
    pub branch: String,
    /// The branch's head: Owlshift's last push at Watch, the Build's last
    /// commit for a held Build.
    pub head: String,
    /// The default branch: the one the pull request merges into, at Watch.
    pub base_branch: String,
    /// The base commit to rebase onto, resolved after the runner's fetch:
    /// the branch must end on a commit that descends from it.
    pub onto: String,
    /// No conflict: the checks failed only on the base the head was pushed
    /// onto, which has passed them since, and the rebase refreshes the
    /// branch so CI runs on the base as it is now.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub refresh: bool,
    /// The paths `git merge-tree` found conflicting between `onto` and
    /// `head`, in its order; empty for a refresh, or when it could not say.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conflicting: Vec<String>,
    /// A held Build's rebase (format 27, OWL-427): the commit the branch was
    /// made from, which fails the project's gate, while `onto` passes it.
    /// The ticket is not delivered, so there is no pull request, and the
    /// brief's `gate` is empty: the runner runs the Build's gate after.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub broken: Option<String>,
}

impl RebaseBrief {
    /// Why this rebase is refused: a pull request and `broken` together or
    /// neither, a `url` without its pull request or the reverse, a held
    /// Build's rebase as a refresh, or a blank `broken`.
    fn refusal(&self) -> Option<&'static str> {
        if self.pull_request.is_some() == self.broken.is_some() {
            return Some("a rebase names either its pull request or its broken base, and not both");
        }
        if self.pull_request.is_some() != self.url.is_some() {
            return Some("a rebase's pull request and its url go together");
        }
        match &self.broken {
            Some(_) if self.refresh => Some("a held Build's rebase is no refresh"),
            Some(broken) if broken.trim().is_empty() => Some("a rebase's broken base is blank"),
            _ => None,
        }
    }
}

/// The most bytes of the replies to one DECISION a correction is given.
pub const MAX_REPLY_BYTES: usize = 16 * 1024;

/// The decisions the decider reversed on a delivered ticket, as a correction
/// run is given them (OWL-146). The replies are the decider's words, the
/// instruction this run follows; everything else here is the runner's
/// record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Correction {
    pub pull_request: u64,
    pub url: String,
    /// The branch's head: Owlshift's last push.
    pub head: String,
    /// Each reversed decision, oldest first.
    #[schemars(length(min = 1))]
    pub reversed: Vec<ReversedDecision>,
}

/// One decision a [`Correction`] reverses.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReversedDecision {
    /// The question the decision settled, under the raising run's id.
    pub question: Question,
    /// The decision the replies reverse.
    pub decision: String,
    /// The decider's replies to its DECISION comment that no run followed
    /// yet, oldest first, separated by blank lines, at most
    /// [`MAX_REPLY_BYTES`].
    pub reply: String,
}

/// One failed check of a [`CheckFailure`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FailedCheck {
    pub name: String,
    pub kind: FailedCheckKind,
    /// The forge's conclusion, such as `FAILURE` or `TIMED_OUT`.
    pub conclusion: String,
    /// Whether branch protection requires it.
    pub required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Its state on the base branch's head.
    pub on_base: BaseState,
    /// What the check printed around its failure: the end of a job's log
    /// up to its first `##[error]` line, another app's output, or a commit
    /// status's description; at most [`MAX_EXCERPT_BYTES`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub excerpt: Option<String>,
    /// Why there is no excerpt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub no_excerpt: Option<String>,
}

/// What kind of check failed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum FailedCheckKind {
    /// A check run, with the app that created it and, for GitHub Actions,
    /// its workflow.
    CheckRun {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        app: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        workflow: Option<String>,
    },
    /// A commit status.
    Status,
}

/// A check's state on the base branch's head.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BaseState {
    Passed,
    Failed,
    Running,
    /// The base head has no check of that name.
    Absent,
    /// The base's checks were not read.
    Unread,
}

impl CheckFailure {
    /// Checks what the types do not carry: at least one check, each with an
    /// excerpt or why none, an excerpt within [`MAX_EXCERPT_BYTES`], and all
    /// of them within [`MAX_EXCERPTS_BYTES`].
    fn validate(&self) -> Result<(), ContractError> {
        if self.checks.is_empty() {
            return Err(ContractError::invalid(
                CONTRACT,
                "a check failure lists no failed check",
            ));
        }
        let mut total = 0;
        for check in &self.checks {
            match (&check.excerpt, &check.no_excerpt) {
                (Some(excerpt), None) => {
                    if excerpt.len() > MAX_EXCERPT_BYTES {
                        return Err(ContractError::invalid(
                            CONTRACT,
                            format!(
                                "the excerpt of check {:?} exceeds {MAX_EXCERPT_BYTES} bytes",
                                check.name
                            ),
                        ));
                    }
                    total += excerpt.len();
                }
                (None, Some(_)) => {}
                _ => {
                    return Err(ContractError::invalid(
                        CONTRACT,
                        format!(
                            "check {:?} needs either an excerpt or why it has none",
                            check.name
                        ),
                    ));
                }
            }
        }
        if total > MAX_EXCERPTS_BYTES {
            return Err(ContractError::invalid(
                CONTRACT,
                format!("the excerpts exceed {MAX_EXCERPTS_BYTES} bytes together"),
            ));
        }
        Ok(())
    }
}

/// The ticket as read from the tracker.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TicketBrief {
    pub id: TicketId,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default)]
    pub labels: Vec<String>,
    pub author: Author,
    /// Text from anyone but the decider is data, never instructions.
    pub description: String,
}

/// The most tickets one list of [`TicketRelations`] holds: a page of Linear's
/// relations, which the adapter reads (OWL-416).
pub const MAX_RELATED: usize = 50;

/// The most bytes of a related ticket's title or state name (OWL-416): the
/// runner cuts a longer one and keeps its beginning.
pub const MAX_RELATED_TEXT_BYTES: usize = 256;

/// The tickets the tracker relates to the ticket (OWL-416, architecture
/// section 5, "The run" of intake), each list in the tracker's order, each
/// ticket once, at most [`MAX_RELATED`]. Titles are other tickets' text:
/// data, never instructions.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TicketRelations {
    /// The tickets that block this one: on Linear its `blocks` relations
    /// from another issue, on the Markdown tracker its `blocked_by`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = MAX_RELATED))]
    pub blocked_by: Vec<RelatedTicket>,
    /// The tickets this one blocks.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = MAX_RELATED))]
    pub blocks: Vec<RelatedTicket>,
    /// The tickets related to this one without an order between them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[schemars(length(max = MAX_RELATED))]
    pub related: Vec<RelatedTicket>,
}

impl TicketRelations {
    pub fn is_empty(&self) -> bool {
        self.blocked_by.is_empty() && self.blocks.is_empty() && self.related.is_empty()
    }

    /// Each list at most [`MAX_RELATED`], each ticket once in it, each text
    /// at most [`MAX_RELATED_TEXT_BYTES`] and without a control character.
    fn validate(&self) -> Result<(), ContractError> {
        for (name, list) in [
            ("blocked_by", &self.blocked_by),
            ("blocks", &self.blocks),
            ("related", &self.related),
        ] {
            if list.len() > MAX_RELATED {
                return Err(ContractError::invalid(
                    CONTRACT,
                    format!("relations.{name} lists more than {MAX_RELATED} tickets"),
                ));
            }
            for (n, ticket) in list.iter().enumerate() {
                if list[..n].iter().any(|earlier| earlier.id == ticket.id) {
                    return Err(ContractError::invalid(
                        CONTRACT,
                        format!("relations.{name} lists {} twice", ticket.id),
                    ));
                }
                let texts = ticket
                    .title
                    .iter()
                    .chain(ticket.state.as_ref().map(|state| &state.name));
                for text in texts {
                    if text.len() > MAX_RELATED_TEXT_BYTES || text.chars().any(char::is_control) {
                        return Err(ContractError::invalid(
                            CONTRACT,
                            format!(
                                "relations.{name}: {} has a text over {MAX_RELATED_TEXT_BYTES} \
                                 bytes or with a control character",
                                ticket.id
                            ),
                        ));
                    }
                }
            }
        }
        Ok(())
    }
}

/// A ticket of [`TicketRelations`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RelatedTicket {
    pub id: TicketId,
    /// Absent when the tracker knows no such ticket (a Markdown
    /// `blocked_by` naming a folder that is not there).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The state it shows; absent as `title` is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<RelatedState>,
}

/// The state a related ticket shows: its name on the tracker and, when the
/// tracker gives one, its type, `completed` for a ticket that is done.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RelatedState {
    pub name: String,
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<StateKind>,
}

/// The type of a tracker state, as Linear types its workflow states.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StateKind {
    Triage,
    Backlog,
    Unstarted,
    Started,
    Completed,
    Canceled,
    Duplicate,
}

/// Who wrote a ticket or a comment, relative to the ticket.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Author {
    pub name: String,
    pub relation: Relation,
}

/// Whether a text comes from the decider, from the runner, or from someone
/// else (whose text is quoted as data).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Relation {
    Decider,
    Owlshift,
    Other,
}

/// One entry of the thread.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ThreadEntry {
    /// A round of questions, numbered Q1..Qn.
    Questions {
        round: NonZeroU32,
        at: Timestamp,
        #[schemars(length(min = 1))]
        questions: Vec<Question>,
    },
    /// A re-ask of the questions of a round left unanswered; they keep their
    /// original ids.
    Reask {
        round: NonZeroU32,
        at: Timestamp,
        #[schemars(length(min = 1))]
        questions: Vec<Question>,
    },
    /// A round the decider answered (format 19, OWL-359): every question's
    /// latest verdict is `answered`. It takes the place of the round's
    /// `questions` entry, and its re-asks are folded into it. Each question
    /// shows what was asked and what the answer check understood of the
    /// decider's answer, which the RESUME comment told them; the options the
    /// decider chose among are on the ticket, in the round's QUESTIONS
    /// comment. An answer check never judges such a round: it is not an ask.
    Answered {
        round: NonZeroU32,
        at: Timestamp,
        #[schemars(length(min = 1))]
        questions: Vec<AnsweredQuestion>,
    },
    /// A comment on the ticket.
    Comment {
        at: Timestamp,
        author: Author,
        body: String,
    },
    /// A question a run raised that the resolver decided without the
    /// decider, in the place of the DECISION comment that logged it. The
    /// runner writes it from its own record, never from a comment's text, so
    /// a comment that only looks like a decision stays a `comment`. It
    /// settles that question and nothing else, and yields to the decider.
    Decision {
        at: Timestamp,
        /// The question as the raising run asked it, under that run's id:
        /// its id and text since format 19 (OWL-359), the decision saying
        /// what was chosen, and whole when the decider replied to the
        /// DECISION comment, whose reply may name an option.
        question: DecidedQuestion,
        decision: String,
        /// What settles it: the ticket, a rule, a file, an earlier answer.
        basis: String,
        /// The decider reversed it by replying to its DECISION comment
        /// (OWL-146): the reply, a `comment` of the thread, is followed and
        /// this decision never is.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        reversed: bool,
    },
}

/// A question of an [`ThreadEntry::Answered`] round (OWL-359).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AnsweredQuestion {
    /// Its id in the round.
    pub id: QuestionId,
    /// The question, as asked.
    pub text: String,
    /// The reason of its latest `answered` verdict: what the answer check
    /// understood the decider's answer to be.
    pub understood: String,
}

/// The question of a [`ThreadEntry::Decision`] (format 19, OWL-359): whole,
/// or its id and text only.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum DecidedQuestion {
    /// As the raising run asked it, its context and options included.
    Whole(Question),
    /// Its id and text: the decision says what was chosen.
    Short(ShortQuestion),
}

impl DecidedQuestion {
    pub fn id(&self) -> &QuestionId {
        match self {
            Self::Whole(question) => &question.id,
            Self::Short(question) => &question.id,
        }
    }

    pub fn text(&self) -> &str {
        match self {
            Self::Whole(question) => &question.text,
            Self::Short(question) => &question.text,
        }
    }
}

/// A question shown by its id and text alone (OWL-359).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ShortQuestion {
    pub id: QuestionId,
    pub text: String,
}

/// Where the ticket starts again: paths relative to the worktree.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Checkpoint {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<RelativePath>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ledger: Option<RelativePath>,
}

/// A project rule injected into the brief.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    /// The zones the rule applies to; empty, the whole repository.
    pub applies_to: Vec<String>,
    /// The file the rule comes from, relative to the repository's root, as
    /// the base commit holds it.
    pub source: String,
    pub text: String,
}

/// What the role is allowed to do.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Permissions {
    pub level: PermissionLevel,
    pub network: bool,
    pub browser: bool,
}

/// A role's permission level.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PermissionLevel {
    ReadOnly,
    WriteWorktree,
}

impl Brief {
    /// Parses and validates a brief.
    pub fn parse(input: &str) -> Result<Self, ContractError> {
        let brief: Self = format::parse_json(CONTRACT, BRIEF_FORMAT, input)?;
        brief.validate()?;
        Ok(brief)
    }

    /// Renders a brief.
    pub fn render(&self) -> String {
        format::render_json(self)
    }

    /// The round and the questions of the thread's latest ask: its last
    /// `questions` or `reask` entry, so after a re-ask the re-asked
    /// questions only. `None` when the thread asks nothing.
    pub fn latest_ask(&self) -> Option<(NonZeroU32, &[Question])> {
        self.thread.iter().rev().find_map(|entry| match entry {
            ThreadEntry::Questions {
                round, questions, ..
            }
            | ThreadEntry::Reask {
                round, questions, ..
            } => Some((*round, questions.as_slice())),
            ThreadEntry::Answered { .. }
            | ThreadEntry::Comment { .. }
            | ThreadEntry::Decision { .. } => None,
        })
    }

    /// Checks the rules the types alone do not carry: rounds increase through
    /// the thread, a round's questions are Q1..Qn, and a re-ask names, in
    /// order, distinct questions of an earlier round; a resolver's brief has
    /// questions to `resolve`, in increasing order, a `delegation` level and
    /// the run that asked (`asked_by`, its plan's and diff's bases full
    /// commit ids), and no other brief has any; only the rebase role's
    /// brief has no `result_refusal` (since format 28, OWL-430: Build's, the
    /// answer check's and the resolver's, intake's since format 24, and
    /// now the design and review roles' have one), of at most [`MAX_RESULT_REFUSAL_BYTES`], and only
    /// Build's, with one, has `decisions_refused`, which `refused_choices`
    /// needs, at most [`MAX_REFUSED_CHOICES`] of them, each text of at most
    /// [`MAX_CHOICE_BYTES`]; the rebase role's brief, and only it, has a
    /// `rebase`; only Build's has a `setup_failure`, and a `correction`,
    /// which reverses at least one decision, each with a reply of at most
    /// [`MAX_REPLY_BYTES`]; only intake's has `named_resources`, and
    /// `relations`, which keep [`TicketRelations`]' bounds.
    pub fn validate(&self) -> Result<(), ContractError> {
        validate_thread(CONTRACT, &self.thread)?;
        if !self.named_resources.is_empty() && self.role != Role::Intake {
            return Err(ContractError::invalid(
                CONTRACT,
                format!(
                    "named resources are given but the role is {}, not {}",
                    self.role.as_str(),
                    Role::Intake.as_str()
                ),
            ));
        }
        if !self.relations.is_empty() && self.role != Role::Intake {
            return Err(ContractError::invalid(
                CONTRACT,
                format!(
                    "relations are given but the role is {}, not {}",
                    self.role.as_str(),
                    Role::Intake.as_str()
                ),
            ));
        }
        self.relations.validate()?;
        if let Some(failure) = &self.check_failure {
            if self.role != Role::Build {
                return Err(ContractError::invalid(
                    CONTRACT,
                    format!(
                        "a check failure is given but the role is {}, not {}",
                        self.role.as_str(),
                        Role::Build.as_str()
                    ),
                ));
            }
            failure.validate()?;
        }
        if self.setup_failure.is_some() && self.role != Role::Build {
            return Err(ContractError::invalid(
                CONTRACT,
                format!(
                    "a setup failure is given but the role is {}, not {}",
                    self.role.as_str(),
                    Role::Build.as_str()
                ),
            ));
        }
        if let Some(correction) = &self.correction {
            if self.role != Role::Build {
                return Err(ContractError::invalid(
                    CONTRACT,
                    format!(
                        "a correction is given but the role is {}, not {}",
                        self.role.as_str(),
                        Role::Build.as_str()
                    ),
                ));
            }
            if correction.reversed.is_empty() {
                return Err(ContractError::invalid(
                    CONTRACT,
                    "a correction reverses no decision",
                ));
            }
            if let Some(n) = correction
                .reversed
                .iter()
                .position(|r| r.reply.trim().is_empty() || r.reply.len() > MAX_REPLY_BYTES)
            {
                return Err(ContractError::invalid(
                    CONTRACT,
                    format!(
                        "reversed decision {} has a blank reply or one over {MAX_REPLY_BYTES} bytes",
                        n + 1
                    ),
                ));
            }
        }
        if self.rebase.is_some() != (self.role == Role::Rebase) {
            return Err(ContractError::invalid(
                CONTRACT,
                format!(
                    "the rebase role's brief, and only it, says what to rebase: the role is {} \
                     and a rebase is {}",
                    self.role.as_str(),
                    if self.rebase.is_some() {
                        "given"
                    } else {
                        "not given"
                    }
                ),
            ));
        }
        if let Some(why) = self.rebase.as_ref().and_then(RebaseBrief::refusal) {
            return Err(ContractError::invalid(CONTRACT, why));
        }
        if !self.refused_choices.is_empty() {
            if !self.decisions_refused {
                return Err(ContractError::invalid(
                    CONTRACT,
                    "refused choices are given without `decisions_refused`",
                ));
            }
            if self.refused_choices.len() > MAX_REFUSED_CHOICES {
                return Err(ContractError::invalid(
                    CONTRACT,
                    format!("more than {MAX_REFUSED_CHOICES} refused choices are given"),
                ));
            }
            if let Some(n) = self.refused_choices.iter().position(|c| {
                c.question.len() > MAX_CHOICE_BYTES || c.recorded.len() > MAX_CHOICE_BYTES
            }) {
                return Err(ContractError::invalid(
                    CONTRACT,
                    format!(
                        "refused choice {} has a text over {MAX_CHOICE_BYTES} bytes",
                        n + 1
                    ),
                ));
            }
        }
        if self.decisions_refused {
            if self.role != Role::Build {
                return Err(ContractError::invalid(
                    CONTRACT,
                    format!(
                        "refused decisions are given but the role is {}, not {}",
                        self.role.as_str(),
                        Role::Build.as_str()
                    ),
                ));
            }
            if self.result_refusal.is_none() {
                return Err(ContractError::invalid(
                    CONTRACT,
                    "refused decisions are given without the refused result's reason",
                ));
            }
        }
        if let Some(reason) = &self.result_refusal {
            // Every role listed, so that a new one takes a side.
            let told = match self.role {
                Role::Build
                | Role::AnswerCheck
                | Role::Resolver
                | Role::Intake
                | Role::Design
                | Role::DesignReview
                | Role::Verify
                | Role::Arbiter => true,
                Role::Rebase => false,
            };
            if !told {
                return Err(ContractError::invalid(
                    CONTRACT,
                    format!(
                        "a refused result is given but the role is {}, which is never told one",
                        self.role.as_str()
                    ),
                ));
            }
            if reason.len() > MAX_RESULT_REFUSAL_BYTES {
                return Err(ContractError::invalid(
                    CONTRACT,
                    format!("the refused result's reason exceeds {MAX_RESULT_REFUSAL_BYTES} bytes"),
                ));
            }
        }
        self.check_quality_pipeline()?;
        if self.delegation.is_some() != (self.role == Role::Resolver) {
            return Err(ContractError::invalid(
                CONTRACT,
                format!(
                    "the resolver's brief, and only it, gives the delegation level: the role is \
                     {} and a level is {}",
                    self.role.as_str(),
                    if self.delegation.is_some() {
                        "given"
                    } else {
                        "not given"
                    }
                ),
            ));
        }
        match &self.asked_by {
            Some(asked) if self.role == Role::Resolver => {
                if asked.run.trim().is_empty() {
                    return Err(ContractError::invalid(
                        CONTRACT,
                        "the asking run has a blank id",
                    ));
                }
                if let Some(given) = &asked.plan {
                    if !is_commit_id(&given.base) {
                        return Err(ContractError::invalid(
                            CONTRACT,
                            format!(
                                "the asking run's plan's base {:?} is not a full commit id",
                                given.base
                            ),
                        ));
                    }
                    given.plan.check(CONTRACT)?;
                }
                if let Some(diff) = &asked.diff {
                    check_diff(diff)?;
                }
            }
            None if self.role != Role::Resolver => {}
            _ => {
                return Err(ContractError::invalid(
                    CONTRACT,
                    format!(
                        "the resolver's brief, and only it, names the asking run: the role is {} \
                         and an asking run is {}",
                        self.role.as_str(),
                        if self.asked_by.is_some() {
                            "given"
                        } else {
                            "not given"
                        }
                    ),
                ));
            }
        }
        match (self.role == Role::Resolver, self.resolve.is_empty()) {
            (true, true) => Err(ContractError::invalid(
                CONTRACT,
                "a resolver's brief has no question to resolve",
            )),
            (false, false) => Err(ContractError::invalid(
                CONTRACT,
                format!(
                    "questions to resolve are given but the role is {}, not {}",
                    self.role.as_str(),
                    Role::Resolver.as_str()
                ),
            )),
            _ => match first_not_ascending(self.resolve.iter().map(|q| &q.id)) {
                Some(id) => Err(ContractError::invalid(
                    CONTRACT,
                    format!("question {id} to resolve is repeated or out of order"),
                )),
                None => Ok(()),
            },
        }
    }
}

impl Brief {
    /// The rules of format 15's fields (OWL-300): the plan only for the
    /// design, design review, build, verify and arbiter roles, always for
    /// design review, its base a full commit id and the plan keeping its own
    /// rules; `approval` only for design, with a plan; findings to rule on
    /// for the arbiter, always, to answer or fix for design or build only,
    /// and to note for build only (OWL-356), ids increasing and each keeping
    /// a result's rules; `review` for design review and verify, always, and
    /// only them, with distinct mandates and its `since` and `earlier` as
    /// [`Brief::check_since`] has them; a diff (OWL-302) only in a verify
    /// run's review and with
    /// findings to rule on, its base a full commit id; pull request reviews
    /// for build only, each with a body or a comment, every comment naming
    /// a path and saying something, and at most
    /// [`MAX_PULL_REQUEST_REVIEW_BYTES`].
    fn check_quality_pipeline(&self) -> Result<(), ContractError> {
        let role = self.role;
        let mismatch = |field: &str, roles: &str| {
            Err(ContractError::invalid(
                CONTRACT,
                format!(
                    "{field}: the role is {}, and {field} goes to {roles}",
                    role.as_str()
                ),
            ))
        };
        match &self.plan {
            Some(given) => {
                if !matches!(
                    role,
                    Role::Design
                        | Role::DesignReview
                        | Role::Build
                        | Role::Verify
                        | Role::Arbiter
                        | Role::AnswerCheck
                ) {
                    return mismatch(
                        "a plan",
                        "the design, design review, build, verify, arbiter and answer check \
                         roles only",
                    );
                }
                if !is_commit_id(&given.base) {
                    return Err(ContractError::invalid(
                        CONTRACT,
                        format!("the plan's base {:?} is not a full commit id", given.base),
                    ));
                }
                given.plan.check(CONTRACT)?;
            }
            None if role == Role::DesignReview => {
                return mismatch("a plan", "the design review role always");
            }
            None => {}
        }
        if self.approval && (role != Role::Design || self.plan.is_none()) {
            return mismatch("an approval round", "the design role only, with its plan");
        }
        match &self.findings {
            Some(given) => {
                let allowed = match given.mode {
                    FindingsMode::Rule => role == Role::Arbiter,
                    FindingsMode::Answer | FindingsMode::Fix => {
                        matches!(role, Role::Design | Role::Build)
                    }
                    FindingsMode::Note => role == Role::Build,
                };
                if !allowed {
                    return mismatch(
                        "findings",
                        "the arbiter to rule on, design or build to answer or fix, and build \
                         to note",
                    );
                }
                if given.findings.is_empty() {
                    return Err(ContractError::invalid(CONTRACT, "no finding is given"));
                }
                check_findings(CONTRACT, &given.findings, MAX_PASS_FINDINGS, false)?;
                if let Some(diff) = &given.diff {
                    if given.mode != FindingsMode::Rule {
                        return mismatch(
                            "a diff with findings",
                            "the arbiter's findings to rule on only",
                        );
                    }
                    check_diff(diff)?;
                }
            }
            None if role == Role::Arbiter => {
                return mismatch("findings", "the arbiter always");
            }
            None => {}
        }
        let reviews = matches!(role, Role::DesignReview | Role::Verify);
        match &self.review {
            Some(review) if reviews => {
                check_mandates(CONTRACT, &review.mandates)?;
                if let Some(diff) = &review.diff {
                    if role != Role::Verify {
                        return mismatch("a diff to review", "the verify role only");
                    }
                    check_diff(diff)?;
                }
                self.check_since(review)?;
            }
            None if !reviews => {}
            _ => return mismatch("a review", "the design review and verify roles, always"),
        }
        if !self.pull_request_reviews.is_empty()
            && !matches!(role, Role::Build | Role::Verify | Role::Arbiter)
        {
            return mismatch(
                "pull request reviews",
                "the build, verify and arbiter roles",
            );
        }
        for review in &self.pull_request_reviews {
            if let Some(broken) = pull_request_review_problem(review) {
                return Err(ContractError::invalid(
                    CONTRACT,
                    format!("pull request review {} {broken}", review.id),
                ));
            }
        }
        Ok(())
    }
}

impl Brief {
    /// The rules of format 21's `since` and `earlier` (OWL-356): the
    /// previous pass and every earlier finding's pass below the pass
    /// reviewed, at most [`MAX_PASS_FINDINGS`] earlier findings; a diff
    /// since for verify, always, and only for it, its base a full commit
    /// id, a pass with nothing to compare with having no `since`; changed
    /// parts,
    /// steps and removed steps only for design review, the parts once each
    /// in the plan's order, the steps increasing within the plan's.
    fn check_since(&self, review: &ReviewBrief) -> Result<(), ContractError> {
        let invalid = |reason: String| Err(ContractError::invalid(CONTRACT, reason));
        if review.earlier.len() > MAX_PASS_FINDINGS {
            return invalid(format!(
                "{} earlier findings, at most {MAX_PASS_FINDINGS}",
                review.earlier.len()
            ));
        }
        if let Some(earlier) = review.earlier.iter().find(|e| e.pass >= review.pass) {
            return invalid(format!(
                "an earlier finding of pass {}, not before pass {}",
                earlier.pass, review.pass
            ));
        }
        let Some(since) = &review.since else {
            return Ok(());
        };
        if since.pass >= review.pass {
            return invalid(format!(
                "since pass {}, not before pass {}",
                since.pass, review.pass
            ));
        }
        let names_plan = !since.parts.is_empty() || !since.steps.is_empty() || since.removed > 0;
        if self.role == Role::Verify && names_plan {
            return invalid("verify's since names plan parts or steps".to_owned());
        }
        if self.role == Role::Verify && since.diff.is_none() {
            return invalid("verify's since names no diff".to_owned());
        }
        if self.role == Role::DesignReview && since.diff.is_some() {
            return invalid("design review's since names a diff".to_owned());
        }
        if let Some(diff) = &since.diff {
            check_diff(diff)?;
        }
        let place = |part: &PlanPart| PlanPart::ALL.iter().position(|p| p == part);
        if !since
            .parts
            .windows(2)
            .all(|pair| place(&pair[0]) < place(&pair[1]))
        {
            return invalid("since names plan parts twice or out of the plan's order".to_owned());
        }
        let steps = self.plan.as_ref().map_or(0, |given| given.plan.steps.len());
        let increasing = since.steps.windows(2).all(|pair| pair[0] < pair[1]);
        let within = since
            .steps
            .last()
            .is_none_or(|last| usize::try_from(last.get()).is_ok_and(|n| n <= steps));
        if !increasing || !within {
            return invalid(format!(
                "since names steps that do not increase within the plan's {steps}"
            ));
        }
        Ok(())
    }
}

/// What is wrong with a pull request review as a brief gives it, if
/// anything: neither a body nor a comment, a comment without a path or a
/// body, or more than [`MAX_PULL_REQUEST_REVIEW_BYTES`] rendered. The
/// review record keeps the reviews of a loop under way by the same rule
/// (OWL-307).
pub(crate) fn pull_request_review_problem(review: &PullRequestReview) -> Option<&'static str> {
    if review.body.trim().is_empty() && review.comments.is_empty() {
        Some("has neither a body nor a comment")
    } else if review
        .comments
        .iter()
        .any(|c| c.path.trim().is_empty() || c.body.trim().is_empty())
    {
        Some("has a comment without a path or a body")
    } else if format::render_json(review).len() > MAX_PULL_REQUEST_REVIEW_BYTES {
        Some("is over the bytes a review may weigh")
    } else {
        None
    }
}

/// A diff's base is a full commit id (OWL-302).
fn check_diff(diff: &GivenDiff) -> Result<(), ContractError> {
    if is_commit_id(&diff.base) {
        Ok(())
    } else {
        Err(ContractError::invalid(
            CONTRACT,
            format!("the diff's base {:?} is not a full commit id", diff.base),
        ))
    }
}

/// Checks the asks of a thread, for the brief and for the asks a ticket ref
/// keeps: rounds increase through the thread, a round's questions are
/// Q1..Qn, and a re-ask names, in order, distinct questions of an earlier
/// round.
pub(crate) fn validate_thread<'a>(
    contract: &'static str,
    thread: impl IntoIterator<Item = &'a ThreadEntry>,
) -> Result<(), ContractError> {
    // The number of questions of each round seen so far.
    let mut rounds: BTreeMap<NonZeroU32, usize> = BTreeMap::new();
    // The rounds shown as `answered` (OWL-359): settled, never asked again.
    let mut answered: BTreeSet<NonZeroU32> = BTreeSet::new();
    for entry in thread {
        match entry {
            ThreadEntry::Questions {
                round, questions, ..
            } => {
                if questions.is_empty() {
                    return Err(ContractError::invalid(
                        contract,
                        format!("round {round} has no question"),
                    ));
                }
                if let Some((last, _)) = rounds.last_key_value()
                    && last >= round
                {
                    return Err(ContractError::invalid(
                        contract,
                        format!("round {round} comes after round {last}"),
                    ));
                }
                check_question_order(contract, questions)?;
                rounds.insert(*round, questions.len());
            }
            ThreadEntry::Answered {
                round, questions, ..
            } => {
                if questions.is_empty() {
                    return Err(ContractError::invalid(
                        contract,
                        format!("answered round {round} has no question"),
                    ));
                }
                if let Some((last, _)) = rounds.last_key_value()
                    && last >= round
                {
                    return Err(ContractError::invalid(
                        contract,
                        format!("round {round} comes after round {last}"),
                    ));
                }
                for (index, question) in questions.iter().enumerate() {
                    let expected = QuestionId::nth(u32::try_from(index + 1).unwrap_or(u32::MAX));
                    if question.id != expected {
                        return Err(ContractError::invalid(
                            contract,
                            format!(
                                "answered round {round}: question {} is out of order: expected \
                                 {expected}",
                                question.id
                            ),
                        ));
                    }
                }
                // A re-ask of an answered round is folded into it: one after
                // it would show the round both settled and open, refused below.
                rounds.insert(*round, questions.len());
                answered.insert(*round);
            }
            ThreadEntry::Reask {
                round, questions, ..
            } => {
                if questions.is_empty() {
                    return Err(ContractError::invalid(
                        contract,
                        format!("re-ask of round {round} has no question"),
                    ));
                }
                let Some(&asked) = rounds.get(round) else {
                    return Err(ContractError::invalid(
                        contract,
                        format!("re-ask of round {round}, which is not earlier in the thread"),
                    ));
                };
                if answered.contains(round) {
                    return Err(ContractError::invalid(
                        contract,
                        format!("re-ask of round {round}, which the thread shows answered"),
                    ));
                }
                for question in questions {
                    if usize::try_from(question.id.number()).map_or(true, |n| n > asked) {
                        return Err(ContractError::invalid(
                            contract,
                            format!(
                                "re-ask of round {round}: question {} was not asked in that round",
                                question.id
                            ),
                        ));
                    }
                }
                if let Some(id) = first_not_ascending(questions.iter().map(|q| &q.id)) {
                    return Err(ContractError::invalid(
                        contract,
                        format!(
                            "re-ask of round {round}: question {id} is repeated or out of order"
                        ),
                    ));
                }
            }
            ThreadEntry::Comment { .. } | ThreadEntry::Decision { .. } => {}
        }
    }
    Ok(())
}
