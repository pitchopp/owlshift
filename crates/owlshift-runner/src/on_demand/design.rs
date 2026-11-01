//! Design and Design review (OWL-303; architecture section 5, "The quality
//! pipeline"; build plan, "P5 · Quality pipeline", part F): a `standard` or
//! `risky` ticket dispatched from Ready goes to Design, whose plan Design
//! review reviews before Build.
//!
//! [`OnDemand::drive`] runs a ticket through its stages: Design and Design
//! review here, Build in `build`, which hands the ticket back here when a
//! settled `replan` sends it to Design. Entering Build from Design or Design
//! review is a dispatch of its own: the base fetched again, the branch
//! fast-forwarded to it (OWL-274), the rules read at it.
//!
//! - *Design.* The design role runs read-only on the ticket's routing; its
//!   questions go through the resolver as Build's do; its plan is kept in
//!   `plan.json` with the text's hash and the commit it read, unapproved.
//!   What a design run is given is read from the state's review passes and
//!   the latest Design review pass in `review.json`, settled again
//!   ([`design_input`]): the findings to fix or answer after a pass that
//!   asked a revision, or that a last pass left for the last revision
//!   (OWL-377), and the approval round after a pass that asked for it. A
//!   last revision's plan is kept with a Design review pass of its own that
//!   no reviewer runs ([`revision_due`]).
//! - *Design review.* The roster of the variant ([`roster::roster`]) runs one
//!   seat after the other on Claude Code, on another model than the
//!   author's when one is configured (OWL-347), then the opt-in Codex
//!   reviewer when the project adds it and Codex can serve (OWL-348); the
//!   findings are
//!   numbered across the seats ([`pass_findings`]), confirmed by one arbiter
//!   run or left to the author's answers (`pipeline.findings`), and the pass
//!   is settled by the core ([`settle_pass`]). A pass that asks a revision,
//!   or leaves findings for the last revision, loops back to Design; one
//!   that ends, or the last revision's, posts the PLAN comment, then goes to
//!   Build when the plan gate is open, or puts the approval
//!   ([`approval_question`]) to the resolver first (OWL-342), the decider's
//!   alone under `always`: an approval it decides, option A, takes the
//!   ticket to Build; a change it decides goes back to Design with the
//!   decision; what it leaves goes to the decider. No finding is asked
//!   (OWL-377). A revision on the resolver's change that touched no fork
//!   and not the work's scope, and answered no `blocker` or `major`
//!   ([`unreviewed_change`], OWL-463), gets no review pass: kept with a pass
//!   no reviewer runs and approved by the change's DECISION comment, it
//!   leaves its review at once, to Build.
//!
//! A refused design, review or arbiter result is a failed run, as intake's
//! is: their briefs carry no refused result to fix. A failed review seat or
//! arbiter run is run again, the pass's other findings kept, until the
//! stage's failed-run bound parks the ticket. A failed PLAN post stops the
//! command before the pass is kept, so `continue` runs the pass again.

use std::num::NonZeroU32;

use sha2::{Digest, Sha256};

use owlshift_adapters::tracker;

use owlshift_contracts::brief::{
    EarlierFinding, GivenDiff, GivenFindings, GivenPlan, ReviewBrief, ReviewSince,
};
use owlshift_contracts::config::FindingsConfirmation;
use owlshift_contracts::ids::FindingId;
use owlshift_contracts::result::{
    ApprovalOutcome, MAX_QUESTION_BYTES, QuestionOption, Recommendation,
};
use owlshift_contracts::review::{
    DroppedReviewer, Finding, FindingOutcome, FindingsMode, KeptApproval, KeptFinding,
    KeptResponse, KeptRuling, MAX_PASS_FINDINGS, Mandate, Plan, PlanChange, PlanPart, PlanPrint,
    PublishedPlan, ResponseOutcome, ReviewPass, Reviewer, RulingOutcome, Severity, TicketPlan,
    TicketReview,
};
use owlshift_core::review::{
    Carried, Fate, PassOutcome, PassVerdict, Standing, resolver_change_reviewed, settle,
};
use owlshift_core::roster;
use owlshift_core::routing::{codex_reviewer_routing, reviewer_routing};
use owlshift_core::vocab::Harness as HarnessKind;

use super::*;
use crate::routing::codex_model;
use crate::ticket_ref::text_sha256;
use crate::writer::{
    CONTEXT_CHARS, DocumentPass, PlanComment, PlanDocument, PlanNext, PlanPointer, PlanReviewer,
    QuestionNote, ROUND_BUDGET_CHARS, ROUND_FRAME_CHARS, Unreviewed, WriteError, chars,
    document_link, flatten, question_block,
};

/// The category of the plan-approval question (architecture section 5,
/// "Plan approval"): through the resolver under `on-fork`, the decider's
/// alone under `always` (OWL-342).
pub(crate) const APPROVAL_CATEGORY: &str = "plan_approval";

/// The first option of the approval question.
const APPROVE: &str = "Approve the plan as it is";

/// What Build did with a ticket.
pub(super) enum Built {
    /// It delivered the ticket.
    Delivered(Delivered),
    /// A `replan` the resolver settled sent the ticket back to Design, in
    /// this state (OWL-303).
    Replan(TicketState),
}

/// What [`OnDemand::reopened`] did before a stage ran.
pub(super) enum Reopened {
    /// No reply reversed a decision the ticket returns for.
    Nothing,
    /// A reply was followed at the stage the ticket is at: it runs, given
    /// the reply.
    Stayed,
    /// The ticket returned to Design, in this state.
    Moved(TicketState),
}

/// What a design run is given, beside the kept plan ([`design_input`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct DesignInput {
    /// The findings of the pass that asked a revision, to fix or answer.
    pub findings: Option<GivenFindings>,
    /// The plan was put to the decider, whose reply the thread holds.
    pub approval: bool,
}

/// What a seat of a review pass ran as, and what it found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SeatOutput {
    pub reviewer: Reviewer,
    /// The reviewer's own model, `None` for its harness's default.
    pub model: Option<String>,
    /// A reviewer on Claude Code on the design run's own model.
    pub same_model: bool,
    pub findings: Vec<Finding>,
}

/// One review pass as it ran ([`OnDemand::run_review_pass`]), settled and
/// not kept yet: what Design review and Verify each make of it.
pub(super) struct PassRun {
    /// The pass, from 1.
    pub pass: u32,
    pub outputs: Vec<SeatOutput>,
    /// The pass's findings, numbered across the seats, each with what the
    /// pass made of it.
    pub findings: Vec<KeptFinding>,
    pub verdict: PassVerdict,
    /// The pass's last run, its arbiter's when one ran.
    pub last_run: Option<String>,
    /// What the pass's runs reported they cost, summed, for the chain its
    /// round counts (OWL-342): `None` when one did not report it.
    pub cost: Option<f64>,
    /// The ticket's review record with the pass, checked.
    pub review: TicketReview,
}

/// How the plan leaves its review ([`OnDemand::plan_leaves_review`]):
/// after a pass that passed, or the pass of a revision no reviewer runs,
/// Design's last revision (OWL-377) or a resolver's change on the plan's
/// approval (OWL-463).
pub(super) struct PlanEnd {
    /// The pass that ended, from 1.
    pub pass: u32,
    pub reviewers: Vec<PlanReviewer>,
    /// The pass's findings; for a revision, those of the pass before, which
    /// the revision answered or which the approval round carried.
    pub findings: Vec<KeptFinding>,
    /// What the PLAN comment lists of the loop's earlier passes
    /// ([`earlier_left`]).
    pub earlier: Vec<(u32, KeptFinding)>,
    /// The pass is a revision's, which no reviewer ran, and which.
    pub revision: Option<Unreviewed>,
    /// The pass's last run, or the design run of the revision.
    pub run: Option<String>,
    /// What the pass's runs cost, as [`PassRun::cost`] sums it.
    pub cost: Option<f64>,
    /// The ticket's review record with the pass, checked.
    pub review: TicketReview,
}

/// The kept plan as a brief gives it.
fn kept_plan(p: &Prepared) -> Option<GivenPlan> {
    p.plan.as_ref().map(|kept| GivenPlan {
        base: kept.base.clone(),
        plan: kept.plan.clone(),
    })
}

/// The plan the stages after Design read (architecture section 5, "Kept and
/// shown"): the kept plan with its base, in a pipeline with Design; none on
/// `trivial`, which has no plan.
pub(super) fn given_plan(p: &Prepared) -> Option<GivenPlan> {
    kept_plan(p).filter(|_| p.pipeline.contains(Stage::Design))
}

/// The plan a Build follows (OWL-365): [`given_plan`] without its `forks`
/// and `alternatives`, how the design chose among approaches, whose choices
/// its approach and steps carry. A copy for the brief: the kept plan, and
/// the hash its approval binds, are untouched.
pub(super) fn build_plan(p: &Prepared) -> Option<GivenPlan> {
    given_plan(p).map(|mut given| {
        given.plan.forks.clear();
        given.plan.alternatives.clear();
        given
    })
}

/// The SHA-256 of a plan's [`Plan::canonical`] text, in lowercase hex: the
/// hash an approval keeps (OWL-32).
pub(crate) fn plan_sha256(plan: &Plan) -> String {
    Sha256::digest(plan.canonical().as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The findings of `findings` that stand: confirmed by the arbiter's ruling,
/// or, when the author answers them, not rejected by the author. A finding
/// the arbiter has not ruled on stands, as one in doubt does.
pub(crate) fn standing(
    findings: &[KeptFinding],
    confirmation: FindingsConfirmation,
) -> Vec<(usize, Standing)> {
    findings
        .iter()
        .enumerate()
        .filter(|(_, kept)| match confirmation {
            FindingsConfirmation::Arbiter => kept
                .ruling
                .as_ref()
                .is_none_or(|ruling| ruling.outcome == RulingOutcome::Confirmed),
            // A ruling kept before the mode changed (a record written under
            // the arbiter, OWL-391) still stands as the arbiter's word.
            FindingsConfirmation::Author => {
                kept.ruling
                    .as_ref()
                    .is_none_or(|ruling| ruling.outcome == RulingOutcome::Confirmed)
                    && kept
                        .response
                        .as_ref()
                        .is_none_or(|response| response.outcome == ResponseOutcome::Fixed)
            }
        })
        .map(|(n, kept)| {
            (
                n,
                Standing {
                    severity: kept.finding.severity,
                },
            )
        })
        .collect()
}

/// Settles review pass number `pass` at `stage` over its `findings`
/// (OWL-301's [`settle`]) and records each finding's outcome: `rejected` for
/// one the arbiter rejected; the others keep none until their author
/// answers them, a noted `minor` never. No finding is asked or proposed
/// (OWL-377).
pub(crate) fn settle_pass(
    stage: Stage,
    pass: u32,
    findings: &mut [KeptFinding],
    confirmation: FindingsConfirmation,
) -> Option<PassVerdict> {
    let core: Vec<Standing> = standing(findings, confirmation)
        .iter()
        .map(|(_, standing)| *standing)
        .collect();
    let verdict = settle(stage, pass, &core)?;
    for kept in findings.iter_mut() {
        if kept
            .ruling
            .as_ref()
            .is_some_and(|ruling| ruling.outcome == RulingOutcome::Rejected)
        {
            kept.outcome = Some(FindingOutcome::Rejected);
        }
    }
    Some(verdict)
}

/// The findings of `findings` whose fate in `verdict`, their pass's, is
/// one of `fates`, given to their author: to fix when the arbiter confirms
/// them, to answer when the author does (OWL-356, a `minor` at Design
/// review never given).
pub(crate) fn given_to_author(
    findings: &[KeptFinding],
    verdict: &PassVerdict,
    confirmation: FindingsConfirmation,
    fates: &[Fate],
) -> Option<GivenFindings> {
    let given: Vec<Finding> = standing(findings, confirmation)
        .iter()
        .zip(&verdict.fates)
        .filter(|(_, fate)| fates.contains(fate))
        .map(|((n, _), _)| findings[*n].finding.clone())
        .collect();
    let mode = match confirmation {
        FindingsConfirmation::Arbiter => FindingsMode::Fix,
        FindingsConfirmation::Author => FindingsMode::Answer,
    };
    (!given.is_empty()).then_some(GivenFindings {
        mode,
        findings: given,
        diff: None,
    })
}

/// The last pass of `review` when pass `pass` at `stage` runs again in its
/// place: the same pass of the same stage, above 1, such as a Verify pass
/// that passed before its delivery failed (OWL-304). A pass 1 run again is
/// appended, which the numbering allows.
pub(crate) fn replaced_pass(review: &TicketReview, stage: Stage, pass: u32) -> Option<&ReviewPass> {
    review
        .passes
        .last()
        .filter(|last| last.stage == stage && last.pass.get() == pass && pass > 1)
}

/// Keeps `kept` as the latest pass of `review`: in place of the pass it
/// runs again ([`replaced_pass`]), else appended. A last fix under way ends
/// with it (OWL-356).
pub(crate) fn push_pass(review: &mut TicketReview, kept: ReviewPass) {
    if replaced_pass(review, kept.stage, kept.pass.get()).is_some() {
        review.passes.pop();
    }
    review.passes.push(kept);
    review.last_fix = None;
}

/// Records what the run given `given`, the latest pass's findings at
/// `stage`, did with them, on that pass: the author's response to each,
/// its outcome the response's, in answer mode (OWL-303, OWL-304) and in fix
/// mode (OWL-377), beside the arbiter's ruling.
pub(crate) fn mark_answered(
    review: &mut TicketReview,
    stage: Stage,
    given: &GivenFindings,
    result: &RunResult,
) {
    let Some(last) = review
        .passes
        .iter_mut()
        .rev()
        .find(|pass| pass.stage == stage)
    else {
        return;
    };
    for kept in &mut last.findings {
        if !given.findings.iter().any(|f| f.id == kept.finding.id) {
            continue;
        }
        match given.mode {
            FindingsMode::Answer | FindingsMode::Fix => {
                if let Some(response) = result
                    .finding_responses
                    .iter()
                    .find(|response| response.finding == kept.finding.id)
                {
                    kept.response = Some(KeptResponse {
                        outcome: response.outcome,
                        reason: response.reason.clone(),
                    });
                    kept.outcome = Some(response.outcome.outcome());
                }
            }
            // The arbiter's findings and the notes are answered by no
            // author (OWL-356).
            FindingsMode::Rule | FindingsMode::Note => {}
        }
    }
}

/// A pass's findings as `review.json` keeps them: every seat's, in seat
/// order, numbered `F1..Fn` across the pass, each with its reviewer and,
/// when the arbiter ruled, its ruling, matched by the pass's ids.
pub(crate) fn pass_findings(
    seats: &[SeatOutput],
    rulings: Option<&[owlshift_contracts::review::Ruling]>,
) -> Vec<KeptFinding> {
    let mut kept = Vec::new();
    for seat in seats {
        for finding in &seat.findings {
            let n = u32::try_from(kept.len() + 1).unwrap_or(u32::MAX);
            let id = FindingId::nth(n);
            let ruling = rulings
                .and_then(|rulings| rulings.iter().find(|ruling| ruling.finding == id))
                .map(|ruling| KeptRuling {
                    outcome: ruling.outcome,
                    reason: ruling.reason.clone(),
                });
            kept.push(KeptFinding {
                finding: Finding {
                    id,
                    ..finding.clone()
                },
                reviewer: seat.reviewer.clone(),
                ruling,
                response: None,
                outcome: None,
            });
        }
    }
    kept
}

/// The passes at `stage` of the review loop the record ends with, oldest
/// first, from the loop's pass 1 up to pass `upto`; a pass numbered above
/// it at the record's end, the one being run or run again in place, is
/// left out; none when the record does not end with that loop. The one
/// walker of a loop's passes (OWL-356): the plan's document logs them
/// (OWL-350), a later pass is told their findings and what changed since
/// the last of them, and the DELIVERY report counts them.
/// The passes of `passes` that ran without the opt-in Codex reviewer, each
/// with why (OWL-348), as the PLAN comment and the DELIVERY report list
/// them.
pub(crate) fn dropped_passes(passes: &[&ReviewPass]) -> Vec<(u32, DroppedReviewer)> {
    passes
        .iter()
        .filter_map(|kept| Some((kept.pass.get(), kept.dropped.clone()?)))
        .collect()
}

pub(crate) fn loop_passes(review: &TicketReview, stage: Stage, upto: u32) -> Vec<&ReviewPass> {
    let mut passes: Vec<&ReviewPass> = Vec::new();
    let mut expected = upto;
    for kept in review.passes.iter().rev() {
        if kept.stage != stage {
            break;
        }
        if passes.is_empty() && kept.pass.get() > upto {
            continue;
        }
        if expected == 0 || kept.pass.get() != expected {
            break;
        }
        passes.push(kept);
        expected -= 1;
    }
    passes.reverse();
    passes
}

/// The SHA-256 of `text`, in lowercase hex.
fn sha256_hex(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The findings of the loop's Design review passes before `pass` that the
/// PLAN comment still lists, oldest pass first, each with its pass: those
/// rejected, with their reasons, those a revision disagreed with or
/// deferred, with the author's (OWL-377), and the `minor` ones noted for
/// the Build (OWL-356), which nothing settles.
pub(crate) fn earlier_left(review: &TicketReview, pass: u32) -> Vec<(u32, KeptFinding)> {
    loop_passes(review, Stage::DesignReview, pass.saturating_sub(1))
        .into_iter()
        .flat_map(|kept| {
            kept.findings
                .iter()
                .filter(|finding| match finding.outcome {
                    Some(outcome) => matches!(
                        outcome,
                        FindingOutcome::Rejected
                            | FindingOutcome::Disagreed
                            | FindingOutcome::Deferred
                    ),
                    None => finding.finding.severity == Severity::Minor,
                })
                .map(|finding| (kept.pass.get(), finding.clone()))
        })
        .collect()
}

/// The findings of the loop's passes at `stage` before `pass`, as a review
/// brief gives them (OWL-356): each as the record keeps it, with its pass,
/// oldest first, at most [`MAX_PASS_FINDINGS`], the latest kept.
/// The number of a Verify pass on a resolution (OWL-449): the one after
/// the record's last Verify pass, 1 when it has none.
fn resolution_pass(review: Option<&TicketReview>) -> u32 {
    review
        .and_then(|review| {
            review
                .passes
                .iter()
                .rfind(|pass| pass.stage == Stage::Verify)
        })
        .map_or(1, |last| last.pass.get().saturating_add(1))
}

pub(crate) fn earlier_findings(
    review: Option<&TicketReview>,
    stage: Stage,
    pass: u32,
) -> Vec<EarlierFinding> {
    let Some(review) = review else {
        return Vec::new();
    };
    let mut earlier: Vec<EarlierFinding> = loop_passes(review, stage, pass.saturating_sub(1))
        .into_iter()
        .flat_map(|kept| {
            kept.findings.iter().map(|finding| EarlierFinding {
                pass: kept.pass,
                kept: finding.clone(),
            })
        })
        .collect();
    let over = earlier.len().saturating_sub(MAX_PASS_FINDINGS);
    earlier.drain(..over);
    earlier
}

/// The pass of Design's last revision that the Design review stage reaches
/// in `state` (OWL-377), by the design run it names: the record's last
/// pass, at Design review, the pass due, kept with the revision's plan.
pub(crate) fn revision_due(state: &TicketState, review: &TicketReview) -> Option<String> {
    let last = review.passes.last()?;
    (state.status() == Status::Active(Stage::DesignReview)
        && last.stage == Stage::DesignReview
        && last.pass.get() == state.review_pass_due())
    .then(|| last.revision.clone())
    .flatten()
}

/// Whether the design run due in `state` is Design's last revision
/// (OWL-377): the mark of the last fix names the state's pass, the
/// record's last, a Design review pass.
pub(crate) fn last_revision_marked(state: &TicketState, review: &TicketReview) -> bool {
    state.status() == Status::Active(Stage::Design)
        && review
            .last_fix
            .is_some_and(|pass| pass.get() == state.review_passes())
        && review
            .passes
            .last()
            .is_some_and(|last| last.stage == Stage::DesignReview)
}

/// The findings the last revision of pass `pass`'s record answered: those
/// of the pass before it at Design review, as the record keeps them.
pub(crate) fn revised_findings(review: &TicketReview, pass: u32) -> Vec<KeptFinding> {
    loop_passes(review, Stage::DesignReview, pass)
        .iter()
        .rev()
        .nth(1)
        .map(|before| before.findings.clone())
        .unwrap_or_default()
}

/// What changed for Design review pass `pass` since the last pass of its
/// loop that kept a plan's print (OWL-356): the last revision's pass, which
/// no reviewer ran, keeps none (OWL-377), so the pass after it is told what
/// changed since the last plan reviewed. `None` for a first pass, or a loop
/// whose passes kept no print.
pub(crate) fn design_since(
    review: Option<&TicketReview>,
    pass: u32,
    plan: &Plan,
) -> Option<ReviewSince> {
    loop_passes(review?, Stage::DesignReview, pass.saturating_sub(1))
        .iter()
        .rev()
        .find_map(|previous| {
            let print = previous.plan.as_ref()?;
            Some(plan_since(previous.pass, print, plan))
        })
}

/// The hash of each part of `plan`, in [`PlanPart::ALL`]'s order.
fn part_hashes(plan: &Plan) -> Vec<String> {
    PlanPart::ALL
        .iter()
        .map(|part| sha256_hex(&plan.part_canonical(*part)))
        .collect()
}

/// The print of `plan` a Design review pass keeps (OWL-356): its parts'
/// hashes and its steps', so the next pass is told what changed.
pub(crate) fn plan_print(plan: &Plan) -> PlanPrint {
    PlanPrint {
        parts: part_hashes(plan),
        steps: plan
            .steps
            .iter()
            .map(|step| sha256_hex(&serde_json::to_string(step).expect("a step serializes")))
            .collect(),
    }
}

/// What changed from the plan printed `before` to `plan` (OWL-356), for
/// the pass after `pass`: the parts whose hash differs, the steps of `plan`
/// whose text `before` did not hold, matched by content so that a step an
/// insertion renumbers is not new, and how many of `before`'s steps `plan`
/// no longer holds. Steps are matched as a multiset: each step of `before`
/// matches one step of `plan` at most, so two steps of one text count as
/// two.
pub(crate) fn plan_since(pass: NonZeroU32, before: &PlanPrint, plan: &Plan) -> ReviewSince {
    let now = plan_print(plan);
    let parts = PlanPart::ALL
        .iter()
        .zip(&now.parts)
        .zip(&before.parts)
        .filter(|((_, now), then)| now != then)
        .map(|((part, _), _)| *part)
        .collect();
    // The previous steps not matched yet, each consumed by its first match.
    let mut unmatched: Vec<&String> = before.steps.iter().collect();
    let mut steps = Vec::new();
    for (n, hash) in now.steps.iter().enumerate() {
        match unmatched.iter().position(|then| *then == hash) {
            Some(at) => {
                unmatched.swap_remove(at);
            }
            None => steps.extend(u32::try_from(n + 1).ok().and_then(NonZeroU32::new)),
        }
    }
    let removed = unmatched.len();
    ReviewSince {
        pass,
        diff: None,
        parts,
        steps,
        removed: u32::try_from(removed).unwrap_or(u32::MAX),
    }
}

/// The notes the Build after Design review is given (OWL-356): at Build
/// before any Verify pass, outside a review loop, the `minor` findings that
/// stand in the Design review passes the record ends with and that nothing
/// settled, oldest pass first, numbered again F1..Fn, at most
/// [`MAX_PASS_FINDINGS`], the latest kept.
pub(crate) fn design_notes(
    state: &TicketState,
    review: Option<&TicketReview>,
    confirmation: FindingsConfirmation,
) -> Option<GivenFindings> {
    let review = review?;
    let last = review.passes.last()?;
    if state.stage() != Stage::Build
        || state.review_passes() != 0
        || last.stage != Stage::DesignReview
    {
        return None;
    }
    let mut notes: Vec<Finding> = loop_passes(review, Stage::DesignReview, last.pass.get())
        .into_iter()
        .flat_map(|kept| {
            standing(&kept.findings, confirmation)
                .into_iter()
                .map(|(n, _)| &kept.findings[n])
                .filter(|finding| {
                    finding.finding.severity == Severity::Minor && finding.outcome.is_none()
                })
                .map(|finding| finding.finding.clone())
                .collect::<Vec<_>>()
        })
        .collect();
    let over = notes.len().saturating_sub(MAX_PASS_FINDINGS);
    notes.drain(..over);
    for (n, note) in notes.iter_mut().enumerate() {
        note.id = FindingId::nth(u32::try_from(n + 1).unwrap_or(u32::MAX));
    }
    (!notes.is_empty()).then_some(GivenFindings {
        mode: FindingsMode::Note,
        findings: notes,
        diff: None,
    })
}

/// What the design run due at Design in `state` is given (assumption A4 of
/// OWL-303): nothing before a review pass ended in this loop; else, by the
/// latest Design review pass of `review`, settled again under
/// `confirmation`: a pass that asked a revision, or that left findings for
/// the last revision (OWL-377), gives its `blocker` and `major` findings, a
/// `minor` being a note for the Build (OWL-356), the standing ones to fix
/// when the arbiter confirms them, every one to answer when the author
/// does; a pass that passed, the last revision's included, was followed by
/// the approval round, given while `approval_required` still holds. A
/// review record that does not end with the state's latest pass is an
/// error.
pub(crate) fn design_input(
    state: &TicketState,
    review: Option<&TicketReview>,
    confirmation: FindingsConfirmation,
    approval_required: bool,
) -> Result<DesignInput, String> {
    let passes = state.review_passes();
    if passes == 0 {
        return Ok(DesignInput::default());
    }
    let last = review
        .and_then(|review| {
            review
                .passes
                .iter()
                .rev()
                .find(|pass| pass.stage == Stage::DesignReview)
        })
        .filter(|last| last.pass.get() == passes)
        .ok_or_else(|| {
            format!(
                "the ticket's state counts {passes} design review passes but its review record \
                 does not end with pass {passes}"
            )
        })?;
    let mut findings = last.findings.clone();
    let verdict = settle_pass(Stage::DesignReview, passes, &mut findings, confirmation)
        .expect("design review settles");
    Ok(match verdict.outcome {
        // The findings it fixes, never a `minor`, noted for the Build
        // (OWL-356).
        PassOutcome::Revise => DesignInput {
            findings: given_to_author(&last.findings, &verdict, confirmation, &[Fate::Fix]),
            approval: false,
        },
        PassOutcome::LastFix => DesignInput {
            findings: given_to_author(&last.findings, &verdict, confirmation, &[Fate::LastFix]),
            approval: false,
        },
        PassOutcome::Passed => DesignInput {
            findings: None,
            approval: approval_required,
        },
    })
}

/// The parts of a plan a resolver's change on the plan's approval must leave
/// as they were to go to Build with no review pass (OWL-463): its goal and
/// criteria, the work's scope, and its forks and the alternatives it set
/// aside, the design's choices of approach; each compared whole.
const TOUCHED_PARTS: [PlanPart; 4] = [
    PlanPart::Goal,
    PlanPart::Criteria,
    PlanPart::Forks,
    PlanPart::Alternatives,
];

/// The latest decision on the plan's approval newer than the latest ask:
/// the resolver's word on the approval round a design run answers, when it
/// decided the round (OWL-342).
fn approval_decision(questions: &TicketQuestions) -> Option<&KeptDecision> {
    questions
        .decisions
        .iter()
        .rev()
        .find(|kept| kept.question.category == APPROVAL_CATEGORY)
        .filter(|kept| questions.latest().is_none_or(|ask| kept.at > ask.at))
}

/// The findings the approval round after the record's latest Design review
/// pass, numbered `pass`, carried to the resolver, as
/// [`OnDemand::plan_leaves_review`] gives them: that pass's, or, when it is
/// Design's last revision, those of the pass before, which it answered.
/// `None` when the record does not end with that pass.
fn carried(review: &TicketReview, pass: u32) -> Option<Vec<Carried>> {
    let last = review
        .passes
        .iter()
        .rev()
        .find(|kept| kept.stage == Stage::DesignReview)
        .filter(|kept| kept.pass.get() == pass)?;
    let findings = if last.revision.is_some() {
        revised_findings(review, pass)
    } else {
        last.findings.clone()
    };
    Some(
        findings
            .iter()
            .map(|kept| Carried {
                severity: kept.finding.severity,
                fixed: kept.outcome == Some(FindingOutcome::Fixed),
            })
            .collect(),
    )
}

/// The DECISION comment of the resolver's change on the plan's approval
/// whose revision, from `before` to `after`, goes to Build with no review
/// pass (OWL-463; [`resolver_change_reviewed`]): the approval round's latest
/// decision, by the resolver, not reversed, and no comment of the decider's
/// newer than it in the design run's `thread`, which the run may have
/// followed instead; the revision leaving [`TOUCHED_PARTS`] as they were; no
/// `blocker` or `major` the round carried left unfixed. `passes` is the
/// state's count of review passes, the round's pass the last of them.
/// `None` when the revision gets its pass: the decider's change, or any
/// other.
pub(crate) fn unreviewed_change(
    thread: &[ThreadEntry],
    questions: &TicketQuestions,
    review: Option<&TicketReview>,
    passes: u32,
    before: &Plan,
    after: &Plan,
) -> Option<String> {
    let decision = approval_decision(questions).filter(|kept| kept.reversed.is_none())?;
    let decider_since = thread.iter().any(|entry| {
        matches!(entry, ThreadEntry::Comment { at, author, .. }
            if author.relation == Relation::Decider && *at > decision.at)
    });
    if decider_since {
        return None;
    }
    let carried = carried(review?, passes)?;
    let touched = TOUCHED_PARTS
        .iter()
        .any(|part| before.part_canonical(*part) != after.part_canonical(*part));
    (!resolver_change_reviewed(touched, &carried)).then(|| decision.comment.clone())
}

/// `text` cut to at most `chars` characters, a mark of the cut kept.
fn clip(text: &str, chars: usize) -> String {
    if text.chars().count() <= chars {
        return text.to_owned();
    }
    let mut kept: String = text.chars().take(chars.saturating_sub(1)).collect();
    kept.push('…');
    kept
}

/// What a question weighs rendered as JSON, as the contracts weigh it
/// against [`MAX_QUESTION_BYTES`]: pretty, with a final newline.
fn weight(question: &Question) -> usize {
    serde_json::to_string_pretty(question).map_or(usize::MAX, |json| json.len() + 1)
}

/// The longest texts a question takes, tried in turn until it fits.
const CAPS: [usize; 7] = [2_000, 1_000, 500, 250, 120, 60, 30];

/// The first of `make`'s questions, over [`CAPS`], that weighs at most
/// `budget` bytes rendered as JSON; the last one tried otherwise.
fn fitting(budget: usize, make: impl Fn(usize) -> Question) -> Question {
    let mut question = make(CAPS[0]);
    for cap in CAPS {
        question = make(cap);
        if weight(&question) <= budget {
            break;
        }
    }
    question
}

/// Where the plan's document is, for the runner's questions to link to
/// (OWL-352): the ticket names it when the tracker gave no link.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PlanLink<'a> {
    pub(crate) ticket: &'a TicketId,
    pub(crate) url: Option<&'a str>,
}

impl PlanLink<'_> {
    /// A link to the document, `what` its text.
    fn to(self, what: &str) -> String {
        document_link(what, self.url, self.ticket)
    }
}

/// The link to `plan`'s document, once the plan is published (OWL-350): on
/// a tracker with documents only.
pub(crate) fn plan_link<'a>(ticket: &'a TicketId, plan: &'a TicketPlan) -> Option<PlanLink<'a>> {
    plan.published.as_ref().map(|published| PlanLink {
        ticket,
        url: published.url.as_deref(),
    })
}

/// The approval question (architecture section 5, "Plan approval", How): one
/// question, whether the decider approves the plan, recommending approval,
/// option A (OWL-324): the plan passed its review. With the plan's document
/// (`document`, OWL-352), two options, approve or change it, the forks in the
/// document with their options lettered, and listed under the question in
/// its QUESTIONS comment ([`approval_note`]). Without, the forks' other
/// options are the options to change ([`per_fork_approval`]).
pub(crate) fn approval_question(plan: &Plan, document: Option<PlanLink<'_>>) -> Question {
    let Some(link) = document else {
        return per_fork_approval(plan);
    };
    let context = |doc: &str| {
        if plan.forks.is_empty() {
            format!("The plan passed its design review, with no fork; {doc} shows it.")
        } else {
            format!(
                "The plan passed its design review. Its forks are in {doc}, each fork's options \
                 lettered A, B, C… in their order."
            )
        }
    };
    let linked = context(&link.to("the plan's document"));
    let context = if chars(&linked) <= CONTEXT_CHARS {
        linked
    } else {
        context("the plan's document")
    };
    let change = if plan.forks.is_empty() {
        "say what in your own words"
    } else {
        "say what in your own words, such as `B, fork 1 option C`"
    };
    Question {
        id: QuestionId::nth(1),
        category: APPROVAL_CATEGORY.to_owned(),
        context,
        text: "Do you approve the plan as it is, or change it?".to_owned(),
        options: vec![
            QuestionOption::new(APPROVE),
            QuestionOption {
                label: "Change it".to_owned(),
                detail: Some(change.to_owned()),
            },
        ],
        recommendation: Some(Recommendation {
            option: Some("A".to_owned()),
            why: "The plan passed its design review.".to_owned(),
        }),
        replan: false,
    }
}

/// The longest choice a fork line shows, tried in turn until the lines fit.
const NOTE_CAPS: [usize; 4] = [120, 80, 50, 30];

/// The lines under the approval question in its QUESTIONS comment
/// (OWL-352): one per fork, its choice and a link to the plan's document,
/// fitted to what [`ROUND_BUDGET_CHARS`] leaves past [`ROUND_FRAME_CHARS`]
/// and the question, each choice clipped over [`NOTE_CAPS`]; past that, one
/// line linking the document. None for a plan with no fork.
pub(crate) fn approval_note(
    question: &Question,
    plan: &Plan,
    link: PlanLink<'_>,
) -> Option<QuestionNote> {
    if plan.forks.is_empty() {
        return None;
    }
    let room = ROUND_BUDGET_CHARS
        .saturating_sub(ROUND_FRAME_CHARS)
        .saturating_sub(chars(&question_block(question)));
    let options = link.to("its options");
    let lines = |cap: usize| -> Vec<String> {
        let mut lines = vec!["**The plan's forks**, each with its choice:".to_owned()];
        lines.extend(plan.forks.iter().enumerate().map(|(n, fork)| {
            format!(
                "- Fork {}: {} · {options}",
                n + 1,
                clip(&flatten(&fork.choice), cap)
            )
        }));
        lines
    };
    let lines = NOTE_CAPS
        .iter()
        .map(|cap| lines(*cap))
        .find(|lines| chars(&lines.join("\n")) <= room)
        .unwrap_or_else(|| {
            vec![format!(
                "The plan's {} forks, each fork's options lettered, are in {}.",
                plan.forks.len(),
                link.to("the plan's document")
            )]
        });
    Some(QuestionNote {
        question: question.id.clone(),
        lines,
    })
}

/// The approval question without the plan's document: its forks' other
/// options as the options to change, cut so the question holds in
/// [`MAX_QUESTION_BYTES`]; past that, one option per fork, its options left
/// to the PLAN comment.
fn per_fork_approval(plan: &Plan) -> Question {
    let budget = MAX_QUESTION_BYTES - 256;
    let question = |options: Vec<String>| Question {
        id: QuestionId::nth(1),
        category: APPROVAL_CATEGORY.to_owned(),
        context: "The plan of this ticket passed its design review. The PLAN comment above shows \
                  it whole, or links to the plan's document, with its forks and the findings its \
                  reviewers left."
            .to_owned(),
        text: "Do you approve the plan the PLAN comment shows as it is, or change it, such as \
               choosing a fork otherwise?"
            .to_owned(),
        options: options.into_iter().map(QuestionOption::new).collect(),
        recommendation: Some(Recommendation {
            option: Some("A".to_owned()),
            why: "The plan passed its design review.".to_owned(),
        }),
        replan: false,
    };
    let each = |cap: usize| {
        let mut options = vec![APPROVE.to_owned()];
        for (n, fork) in plan.forks.iter().enumerate() {
            for option in fork.options.iter().filter(|option| **option != fork.choice) {
                options.push(format!(
                    "Fork {}: {} instead of {}",
                    n + 1,
                    clip(option, cap),
                    clip(&fork.choice, cap)
                ));
            }
        }
        question(options)
    };
    let detailed = fitting(budget, each);
    if weight(&detailed) <= budget {
        return detailed;
    }
    let mut options = vec![APPROVE.to_owned()];
    options.extend(
        (1..=plan.forks.len()).map(|n| {
            format!("Change fork {n}: its options are in the plan the PLAN comment shows")
        }),
    );
    let per_fork = question(options);
    if weight(&per_fork) <= budget {
        return per_fork;
    }
    question(vec![
        APPROVE.to_owned(),
        "Change it: say how in your answer".to_owned(),
    ])
}

fn harness_key(harness: HarnessKind) -> &'static str {
    match harness {
        HarnessKind::Claude => "claude",
        HarnessKind::Codex => "codex",
    }
}

fn outcome_name(outcome: PassOutcome) -> &'static str {
    match outcome {
        PassOutcome::Passed => "passed",
        PassOutcome::Revise => "revise",
        PassOutcome::LastFix => "last_fix",
    }
}

pub(crate) fn finding_ids<'k>(findings: impl IntoIterator<Item = &'k KeptFinding>) -> Value {
    json!(
        findings
            .into_iter()
            .map(|kept| kept.finding.id.as_str().to_owned())
            .collect::<Vec<_>>()
    )
}

impl OnDemand<'_> {
    /// Runs the ticket from `state` through the stages of its pipeline until
    /// it delivers or stops (OWL-303): Design and Design review here, Build
    /// in `build`. Entering Build from Design or Design review is the
    /// dispatch to Build ([`Self::dispatch_to_build`]); a settled `replan`
    /// brings the ticket back to Design.
    pub(super) fn drive(
        &self,
        p: &mut Prepared,
        command: Command,
        mut dispatched: Dispatched,
        mut state: TicketState,
        sink: &mut EventSink<'_>,
    ) -> Result<Delivered, Stop> {
        loop {
            // The decider's replies to the resolver's approval come first
            // (OWL-345): the ticket returns to Design, or runs on with the
            // reply as `reversed`.
            if let Reopened::Moved(next) = self.reopened(p, state, sink)? {
                state = next;
            }
            match state.status() {
                Status::Active(Stage::Design) | Status::Active(Stage::DesignReview) => {
                    let next = match state.stage() {
                        Stage::Design => self.design_run(p, &dispatched, state, sink)?,
                        _ => self.review_pass(p, &dispatched, state, sink)?,
                    };
                    // Under watch the stage's run is the step (OWL-367): the
                    // dispatch to Build is the next step's.
                    if p.stepping {
                        return Err(self.stepped(p, &next, sink));
                    }
                    if next.status() == Status::Active(Stage::Build) {
                        dispatched = self.dispatch_to_build(p, command, sink)?;
                    }
                    state = next;
                }
                Status::Active(Stage::Build) => {
                    match self.build(p, command, &dispatched, state, sink)? {
                        Built::Delivered(delivered) => return Ok(delivered),
                        Built::Replan(next) if p.stepping => {
                            return Err(self.stepped(p, &next, sink));
                        }
                        Built::Replan(next) => state = next,
                    }
                }
                // A Verify pass `continue` runs again on the kept Build
                // (OWL-304), then the delivery or a Verify fix run.
                Status::Active(Stage::Verify) => {
                    match self.verify_pass(p, &dispatched, state, sink)? {
                        Verified::Passed(verified) => {
                            return self.deliver_verified(p, &dispatched, &verified, sink);
                        }
                        Verified::Revise(next) if p.stepping => {
                            return Err(self.stepped(p, &next, sink));
                        }
                        Verified::Revise(next) => state = next,
                    }
                }
                other => return Err(core_error(other)),
            }
        }
    }

    /// The dispatch to Build after Design (architecture section 5,
    /// "Design", The run): the base fetched again, the ticket's branch,
    /// which Design and its review left without a commit, fast-forwarded to
    /// it (OWL-274), then the dispatch's forge check, rules and events.
    pub(super) fn dispatch_to_build(
        &self,
        p: &mut Prepared,
        command: Command,
        sink: &mut EventSink<'_>,
    ) -> Result<Dispatched, Stop> {
        // The fetch and the branch's move in one of the project's short
        // sections (OWL-401).
        let section = self.section()?;
        p.base = project::sync_checkout(&self.executor.git, self.dirs, self.remote_url)
            .map_err(Stop::Refused)?;
        self.start_from_base(p)?;
        drop(section);
        self.dispatch(p, command, sink)
    }

    /// A `replan` the resolver settled with the rest of a Build run's
    /// questions (OWL-301's `ReplanDecided`): the ticket goes back to
    /// Design, whose new plan is reviewed again.
    pub(super) fn replanned(
        &self,
        p: &mut Prepared,
        state: TicketState,
        run: &str,
        sink: &mut EventSink<'_>,
    ) -> Result<Built, Stop> {
        let next = match state.apply(p.pipeline, Event::ReplanDecided) {
            Ok(Transition::To(next)) => next,
            other => return Err(core_error(other)),
        };
        self.keep(p, &next)?;
        sink.emit(
            &p.ticket,
            Some(run),
            EventKind::Decision,
            data([("replan", json!("decided")), ("stage", json!("design"))]),
        );
        Ok(Built::Replan(next))
    }

    /// Applies the decider's replies to the resolver's plan approval before
    /// a stage runs (OWL-345; architecture section 5, "A reversed
    /// decision"): a reply to the decision kept as the plan's approval drops
    /// the approval and returns the ticket to Design. The reversal is marked
    /// followed in the same write as the move, so the run that follows is
    /// given it as `reversed`, with the decider's reply in its thread.
    ///
    /// Nothing for a ticket in a review loop: it is delivered, with its pull
    /// request open, and the core has no way back from Design to Watch, so
    /// the correction run keeps those replies. A reply still inside the
    /// quiet window is left unfollowed.
    pub(super) fn reopened(
        &self,
        p: &mut Prepared,
        state: TicketState,
        sink: &mut EventSink<'_>,
    ) -> Result<Reopened, Stop> {
        let Status::Active(
            stage @ (Stage::Design | Stage::DesignReview | Stage::Build | Stage::Verify),
        ) = state.status()
        else {
            return Ok(Reopened::Nothing);
        };
        let questions = p.questions();
        if p.stored.is_none()
            || in_review_loop(p)
            || !questions
                .decisions
                .iter()
                .any(|kept| reversal::reopens(kept, p.plan.as_ref()))
        {
            return Ok(Reopened::Nothing);
        }
        let comments = self.comments(&p.ticket)?;
        let current = person(p.current()?, &p.found);
        let mut reversed =
            reversal::reversals(&comments, &questions, reversal::Current::Known(&current));
        reversed.retain(|r| {
            questions
                .decisions
                .get(r.decision)
                .is_some_and(|kept| reversal::reopens(kept, p.plan.as_ref()))
        });
        if reversed.is_empty() {
            return Ok(Reopened::Nothing);
        }
        let window = self.config.policy.quiet_window();
        if reversal::counts_at(&reversed, (self.clock)(), window).is_some() {
            return Ok(Reopened::Nothing);
        }
        let event = reversal::return_event(stage);
        let next = match event {
            None => state,
            // A pipeline without the destination does not return.
            Some(event) => match state.apply(p.pipeline, event) {
                Ok(Transition::To(next)) => next,
                _ => return Ok(Reopened::Nothing),
            },
        };
        let mut questions = questions;
        reversal::followed(&mut questions, &reversed);
        // A Build that follows the reply is no longer a last fix alone: a
        // Verify pass follows it (OWL-356).
        if let Some(review) = p.review.as_mut() {
            review.last_fix = None;
        }
        let kept_plan = p.plan.clone();
        if let Some(plan) = p.plan.as_mut() {
            plan.approval = None;
        }
        if let Err(e) = self.store(p, &next, questions.clone()) {
            p.plan = kept_plan;
            return Err(refused("keeping the reopened ticket in its ref", e));
        }
        let ids: Vec<QuestionId> = reversed
            .iter()
            .filter_map(|r| questions.decisions.get(r.decision))
            .map(|kept| kept.question.id.clone())
            .collect();
        sink.emit(
            &p.ticket,
            None,
            EventKind::Decision,
            data([
                ("reversed", json!("reopened")),
                ("from", json!(stage)),
                ("to", json!(Stage::Design)),
                ("approval_dropped", json!(true)),
                ("questions", question_ids(&ids)),
                (
                    "replies",
                    json!(
                        reversed
                            .iter()
                            .flat_map(|r| r.replies.iter().map(|c| c.id.as_str()))
                            .collect::<Vec<_>>()
                    ),
                ),
            ]),
        );
        Ok(if event.is_some() {
            Reopened::Moved(next)
        } else {
            Reopened::Stayed
        })
    }

    /// The plan gate of `kept` (OWL-32): whether its approval is required,
    /// and whether `plan.json` holds it for this plan's hash.
    fn plan_gate(&self, p: &Prepared, kept: &TicketPlan) -> owlshift_core::gate::PlanGate {
        p.policy.plan_gate(
            p.pipeline,
            !kept.plan.forks.is_empty(),
            &plan_sha256(&kept.plan),
            kept.approval
                .as_ref()
                .map(|approval| approval.plan_sha256.as_str()),
        )
    }

    /// One run of a read-only role of the review stages from `brief` on
    /// `harness`, run again after a failure until it gives a result or the
    /// stage's failed-run bound parks the ticket. A run that asks or finds
    /// a false premise is a failed one: these roles ask nothing. A usage
    /// limit stops the command; `state` keeps what the failures counted.
    /// A run whose result was refused tells the seat's next run why
    /// (`result_refusal`, OWL-430), kept nowhere else, and an untold
    /// refusal never spends the stage's last attempt
    /// ([`Event::ResultRefused`]).
    fn review_run(
        &self,
        p: &mut Prepared,
        harness: &dyn Harness,
        state: &mut TicketState,
        brief: &Brief,
        sink: &mut EventSink<'_>,
    ) -> Result<(Ran, RunResult), Stop> {
        let mut attempt = 0u32;
        let mut brief = brief.clone();
        loop {
            attempt += 1;
            let (ran, event) = self.run_role_once(
                p,
                (harness, &[]),
                state,
                &brief,
                attempt,
                &mut Gathered::default(),
                sink,
            )?;
            brief.result_refusal = result_refusal(&ran.report.outcome);
            let event = match (event, ran.result()) {
                (Event::Completed, Some(result)) => {
                    let result = result.clone();
                    return Ok((ran, result));
                }
                (
                    Event::Interrupted | Event::Quarantined | Event::Blocked | Event::ResultRefused,
                    _,
                ) => event,
                _ => Event::RunFailed,
            };
            self.review_run_failed(p, state, event, &ran, sink)?;
        }
    }

    /// What a review stage's run that gave no result does to the ticket:
    /// the core's transition for `event`, kept; a usage limit stops the
    /// command, and the failed-run bound parks the ticket.
    fn review_run_failed(
        &self,
        p: &mut Prepared,
        state: &mut TicketState,
        event: Event,
        ran: &Ran,
        sink: &mut EventSink<'_>,
    ) -> Result<(), Stop> {
        match state.apply(p.pipeline, event) {
            Ok(Transition::To(next)) => {
                *state = next;
                self.keep(p, state)?;
                if let Some(stop) = interruption(&ran.report.outcome) {
                    return Err(stop);
                }
                Ok(())
            }
            Ok(Transition::Parked {
                state: parked,
                reason,
            }) => {
                let questions = p.questions();
                Err(self.park(p, &parked, reason, ran, Some(questions), Vec::new(), sink))
            }
            other => Err(core_error(other)),
        }
    }

    /// One run of the opt-in Codex reviewer (OWL-348): its result, or why
    /// the pass goes on without it. It is never run again and counts toward
    /// no failed-run bound: a harness that cannot build its command or
    /// spawn, a usage limit, a failure, a refused result, a `blocked` result
    /// or questions drop the seat for this pass, and a usage limit for the
    /// command's later passes too. Two guardrails stay any run's: the
    /// ticket's budget is checked first, and a run that broke isolation is a
    /// failed review run, which the core parks. What every run shares stops
    /// the command as for any run: a credential within the agent's reach,
    /// the worktree, the run's files, the run directory, the project's
    /// marker. Codex held by the machine's usage state drops the seat too,
    /// told nothing: it stands in no ticket's way (OWL-404).
    fn codex_review_run(
        &self,
        p: &mut Prepared,
        harness: &dyn Harness,
        state: &mut TicketState,
        brief: &Brief,
        sink: &mut EventSink<'_>,
    ) -> Result<Result<(Ran, RunResult), String>, Stop> {
        self.within_budget(p, state, sink)?;
        let ran =
            match self.execute_or_unstarted(p, self.executor, (harness, &[]), brief, 1, None, sink)
            {
                Err(Stop::UsageLimit { resets_at }) => {
                    let reason =
                        codex_drop_reason(&Outcome::UsageLimit { resets_at }, Event::Interrupted);
                    p.codex = CodexSeat::Dropped(reason.clone());
                    return Ok(Err(reason));
                }
                Err(stop) => return Err(stop),
                Ok(Ok(ran)) => ran,
                Ok(Err(ExecutorError::Command(_) | ExecutorError::Spawn(_))) => {
                    return Ok(Err("Codex's run could not start.".to_owned()));
                }
                Ok(Err(error)) => return Err(refused("the run could not start", error)),
            };
        // A checked stop ends the pass, the seat kept (OWL-161).
        if matches!(ran.report.outcome, Outcome::Stopped) {
            return Err(Stop::Stopped);
        }
        // A refused result may read as `ResultRefused` (OWL-430): never
        // applied, as the seat is dropped, never retried or told.
        let event = if ran.breaches.is_empty() {
            stage_event(&ran.report.outcome, brief).0
        } else {
            Event::Quarantined
        };
        if let (Event::Completed, Some(result)) = (event, ran.result()) {
            let result = result.clone();
            return Ok(Ok((ran, result)));
        }
        if event == Event::Quarantined {
            self.review_run_failed(p, state, event, &ran, sink)?;
            return Ok(Err("Codex's run broke isolation.".to_owned()));
        }
        let reason = codex_drop_reason(&ran.report.outcome, event);
        if matches!(ran.report.outcome, Outcome::UsageLimit { .. }) {
            p.codex = CodexSeat::Dropped(reason.clone());
        }
        Ok(Err(reason))
    }

    /// Runs the design role at Design until its plan is kept, an approval is
    /// recorded, or it stops: a failed run gets one more before the core
    /// parks the ticket; questions go through the resolver, all decided,
    /// it runs again, else a round opens. A run whose result was refused
    /// tells the next one why (`result_refusal`, OWL-430), kept nowhere
    /// else, as intake's loop does (OWL-402), and an untold refusal never
    /// spends the stage's last attempt ([`Event::ResultRefused`]).
    fn design_run(
        &self,
        p: &mut Prepared,
        dispatched: &Dispatched,
        mut state: TicketState,
        sink: &mut EventSink<'_>,
    ) -> Result<TicketState, Stop> {
        let current = person(p.current()?, &p.found);
        let routing = role_routing(Role::Design, dispatched.routing)
            .expect("the design role follows the ticket");
        let harness = self.design.harness(routing);
        let confirmation = self.config.pipeline.findings();
        let mut attempt = 0u32;
        // Why the previous run of this loop was refused, which the next one
        // is told (OWL-430), so that a missing answer to a finding is not
        // missed again before the ticket parks.
        let mut told: Option<String> = None;
        loop {
            attempt += 1;
            let required = p.plan.as_ref().is_some_and(|kept| {
                p.policy
                    .plan_approval_required(p.pipeline, !kept.plan.forks.is_empty())
            });
            let input =
                design_input(&state, p.review.as_ref(), confirmation, required).map_err(|e| {
                    Stop::Refused(format!(
                        "{e}: `owlshift forget {}` starts its record over",
                        p.ticket
                    ))
                })?;
            let comments = self.comments(&p.ticket)?;
            let mut brief = self.brief(
                p,
                Role::Design,
                &comments,
                &dispatched.rules,
                None,
                &current,
            );
            brief.plan = kept_plan(p);
            brief.approval = input.approval && brief.plan.is_some();
            brief.findings = input.findings.filter(|_| brief.plan.is_some());
            brief.result_refusal = told.take();
            let (ran, event) = self.run_role_once(
                p,
                (&*harness, &[]),
                &state,
                &brief,
                attempt,
                &mut Gathered::default(),
                sink,
            )?;
            told = result_refusal(&ran.report.outcome);
            // A last revision that asks is no longer the last alone: the
            // round buys a reviewed pass, as after Verify's last fix
            // (OWL-356, OWL-377).
            if event == Event::Questions && drop_last_fix(p) {
                self.keep(p, &state)?;
            }
            let result = ran.result();
            // A false premise that asks nothing parks the ticket
            // (architecture section 5, "A false premise"); its questions go
            // through the resolver as any (OWL-342).
            if event == Event::Questions
                && let Some(result) = result
                && !result.asks()
            {
                return Err(self.park_false_premise(
                    p,
                    p.pipeline,
                    &state,
                    "design run",
                    &ran.run,
                    result,
                    sink,
                ));
            }
            if event == Event::Questions
                && let Some(result) = result
            {
                let asking = Asked {
                    plan: kept_plan(p),
                    ..Asked::run(Role::Design, &ran, result, &[])
                };
                match self.resolve(p, &state, &asking, &dispatched.rules, &current, sink)? {
                    Routing::Settled(_) => {
                        settle_session(p, &ran.run);
                        // Kept now: a next run that cannot start leaves it
                        // settled for the next command (OWL-394).
                        self.keep(p, &state)?;
                        continue;
                    }
                    Routing::Ask(round) => {
                        let asked = match state.apply(p.pipeline, Event::Questions) {
                            Ok(Transition::To(asked)) => asked,
                            other => return Err(core_error(other)),
                        };
                        return Err(self.ask(
                            p,
                            &asked,
                            result.status,
                            &result.summary,
                            round,
                            &ran.run,
                            sink,
                        ));
                    }
                }
            }
            if event == Event::Completed
                && let Some(result) = result
            {
                let result = result.clone();
                return self.designed(p, dispatched, state, &brief, &result, &comments, &ran, sink);
            }
            match state.apply(p.pipeline, event) {
                Ok(Transition::To(next)) => {
                    state = next;
                    self.keep(p, &state)?;
                    if let Some(stop) = interruption(&ran.report.outcome) {
                        return Err(stop);
                    }
                }
                Ok(Transition::Parked {
                    state: parked,
                    reason,
                }) => {
                    // The park makes the ticket's ref when it has none, so
                    // `continue` restarts the design.
                    let questions = p.questions();
                    return Err(self.park(
                        p,
                        &parked,
                        reason,
                        &ran,
                        Some(questions),
                        Vec::new(),
                        sink,
                    ));
                }
                other => return Err(core_error(other)),
            }
        }
    }

    /// Keeps what a `done` design run gave: an approval of the kept plan,
    /// recorded with the decider's comment and the plan's hash, which takes
    /// the ticket to Build (OWL-32's `PlanApproved`); or a plan, new and
    /// unapproved, the findings its brief gave marked with the author's
    /// answers, which takes it to Design review. A last revision's plan
    /// (OWL-377, [`last_revision_marked`]) is kept with a Design review pass
    /// that names the run and no reviewer runs. One write keeps it with the
    /// state. A revision on a resolver's change that gets no review pass
    /// (OWL-463, [`unreviewed_change`]) leaves its review at once
    /// ([`Self::applied_unreviewed`]).
    #[allow(clippy::too_many_arguments)]
    fn designed(
        &self,
        p: &mut Prepared,
        dispatched: &Dispatched,
        state: TicketState,
        brief: &Brief,
        result: &RunResult,
        comments: &[Comment],
        ran: &Ran,
        sink: &mut EventSink<'_>,
    ) -> Result<TicketState, Stop> {
        let ticket = p.ticket.clone();
        if brief.approval && result.approval == Some(ApprovalOutcome::Approved) {
            let mut kept = p
                .plan
                .clone()
                .ok_or_else(|| Stop::Refused("an approval of no plan".to_owned()))?;
            let questions = p.questions();
            // Whose word the run answered (OWL-342): the resolver's, when
            // its decision on the approval is newer than any ask, kept with
            // its DECISION comment; else the decider's answer: their newest
            // comment, or the round's comment their thumbs-up answered
            // (OWL-371).
            let decided = approval_decision(&questions);
            let comment = match decided {
                Some(decision) => decision.comment.clone(),
                None => {
                    let ask = questions.latest().ok_or_else(|| {
                        Stop::Refused("an approval round the ref keeps no ask of".to_owned())
                    })?;
                    let approver = person(&ask.decider, &p.found);
                    answer_check::answering_comment(comments, &approver, &questions).ok_or_else(
                        || {
                            Stop::Refused(format!(
                                "the plan of {ticket} is approved, but no comment of its decider \
                                 holds the approval"
                            ))
                        },
                    )?
                }
            };
            let sha = plan_sha256(&kept.plan);
            kept.approval = Some(KeptApproval {
                comment: comment.clone(),
                plan_sha256: sha.clone(),
            });
            let gate = self.plan_gate(p, &kept);
            let next = match state.apply_gated(p.pipeline, Event::PlanApproved, gate) {
                Ok(Transition::To(next)) => next,
                other => return Err(core_error(other)),
            };
            p.plan = Some(kept);
            let questions = p.questions();
            self.store(p, &next, questions)
                .map_err(|e| refused("keeping the plan's approval in the ticket's ref", e))?;
            sink.emit(
                &ticket,
                Some(&ran.run),
                EventKind::Decision,
                data([
                    ("plan", json!("approved")),
                    (
                        "by",
                        json!(if decided.is_some() {
                            "resolver"
                        } else {
                            "decider"
                        }),
                    ),
                    ("comment", json!(comment)),
                    ("plan_sha256", json!(sha)),
                ]),
            );
            return Ok(next);
        }
        let plan = result
            .plan
            .clone()
            .ok_or_else(|| Stop::Refused("a design run done without a plan".to_owned()))?;
        let revision = p
            .review
            .as_ref()
            .is_some_and(|review| last_revision_marked(&state, review));
        // A resolver's change on the plan's approval for its notes alone:
        // to Build with no review pass (OWL-463).
        if brief.approval
            && !revision
            && let Some(before) = p.plan.as_ref()
        {
            let questions = p.questions();
            let unreviewed = unreviewed_change(
                &brief.thread,
                &questions,
                p.review.as_ref(),
                state.review_passes(),
                &before.plan,
                &plan,
            );
            if let Some(comment) = unreviewed {
                return self.applied_unreviewed(p, dispatched, state, plan, comment, ran, sink);
            }
        }
        // What the run did with the findings it was given, on the pass that
        // raised them.
        if let (Some(given), Some(review)) = (&brief.findings, p.review.as_mut()) {
            mark_answered(review, Stage::DesignReview, given, result);
        }
        // The last revision: its pass, reviewed by no one, ends the mark
        // (OWL-377).
        if revision && let Some(review) = p.review.as_mut() {
            let pass = NonZeroU32::new(state.review_pass_due()).expect("a pass due counts from 1");
            push_pass(
                review,
                ReviewPass {
                    stage: Stage::DesignReview,
                    pass,
                    findings: Vec::new(),
                    reviewers: Vec::new(),
                    head: None,
                    plan: None,
                    revision: Some(ran.run.clone()),
                    dropped: None,
                },
            );
            review.validate().map_err(|e| {
                Stop::Refused(format!(
                    "the review record of {ticket} would not read back ({e}): `owlshift forget \
                     {ticket}` starts it over"
                ))
            })?;
        }
        let base = self.branch_head(p).unwrap_or_else(|| p.base.commit.clone());
        let kept = TicketPlan {
            format: Format,
            plan,
            text_sha256: text_sha256(&p.found.title, &p.found.description),
            base: base.clone(),
            // A plan a design run writes starts unapproved (OWL-32).
            approval: None,
            // Its document stays the ticket's (OWL-350).
            published: p.plan.as_ref().and_then(|plan| plan.published.clone()),
        };
        let sha = plan_sha256(&kept.plan);
        let forks = kept.plan.forks.len();
        let gate = self.plan_gate(p, &kept);
        let next = match state.apply_gated(p.pipeline, Event::Completed, gate) {
            Ok(Transition::To(next)) => next,
            other => return Err(core_error(other)),
        };
        p.plan = Some(kept);
        let questions = p.questions();
        self.store(p, &next, questions)
            .map_err(|e| refused("keeping the plan in the ticket's ref", e))?;
        sink.emit(
            &ticket,
            Some(&ran.run),
            EventKind::Decision,
            data([
                ("plan", json!("kept")),
                ("plan_sha256", json!(sha)),
                ("base", json!(base)),
                ("forks", json!(forks)),
                ("last_revision", json!(revision)),
            ]),
        );
        Ok(next)
    }

    /// The revision a design run wrote on a resolver's change to the plan,
    /// decided on its approval, that gets no review pass (OWL-463,
    /// [`unreviewed_change`]): the plan kept, approved by the change's
    /// DECISION comment with the revised plan's hash, with a Design review
    /// pass that names the run and no reviewer runs, as Design's last
    /// revision's is; then it leaves its review as after a pass that passed
    /// ([`Self::plan_leaves_review`]), the PLAN comment first, the approval
    /// opening the gate to Build. Nothing is kept when the PLAN comment
    /// cannot be posted, so `continue` runs the design again.
    #[allow(clippy::too_many_arguments)]
    fn applied_unreviewed(
        &self,
        p: &mut Prepared,
        dispatched: &Dispatched,
        state: TicketState,
        plan: Plan,
        comment: String,
        ran: &Ran,
        sink: &mut EventSink<'_>,
    ) -> Result<TicketState, Stop> {
        let ticket = p.ticket.clone();
        let base = self.branch_head(p).unwrap_or_else(|| p.base.commit.clone());
        let sha = plan_sha256(&plan);
        let forks = plan.forks.len();
        let kept = TicketPlan {
            format: Format,
            plan,
            text_sha256: text_sha256(&p.found.title, &p.found.description),
            base: base.clone(),
            approval: Some(KeptApproval {
                comment: comment.clone(),
                plan_sha256: sha.clone(),
            }),
            published: p.plan.as_ref().and_then(|plan| plan.published.clone()),
        };
        let pass = NonZeroU32::new(state.review_pass_due()).expect("a pass due counts from 1");
        let mut review = p.review.clone().unwrap_or_default();
        push_pass(
            &mut review,
            ReviewPass {
                stage: Stage::DesignReview,
                pass,
                findings: Vec::new(),
                reviewers: Vec::new(),
                head: None,
                plan: None,
                revision: Some(ran.run.clone()),
                dropped: None,
            },
        );
        review.validate().map_err(|e| {
            Stop::Refused(format!(
                "the review record of {ticket} would not read back ({e}): `owlshift forget \
                 {ticket}` starts it over"
            ))
        })?;
        let reviewing = match state.apply(p.pipeline, Event::Completed) {
            Ok(Transition::To(next)) => next,
            other => return Err(core_error(other)),
        };
        let gate = self.plan_gate(p, &kept);
        let end = PlanEnd {
            pass: pass.get(),
            reviewers: Vec::new(),
            earlier: earlier_left(&review, pass.get().saturating_sub(1)),
            findings: revised_findings(&review, pass.get()),
            revision: Some(Unreviewed::Change),
            run: Some(ran.run.clone()),
            cost: Some(0.0),
            review,
        };
        p.plan = Some(kept.clone());
        let next = self.plan_leaves_review(p, dispatched, reviewing, kept, gate, end, sink)?;
        sink.emit(
            &ticket,
            Some(&ran.run),
            EventKind::Decision,
            data([
                ("plan", json!("kept")),
                ("plan_sha256", json!(sha)),
                ("base", json!(base)),
                ("forks", json!(forks)),
                ("last_revision", json!(false)),
            ]),
        );
        sink.emit(
            &ticket,
            Some(&ran.run),
            EventKind::Decision,
            data([
                ("plan", json!("approved")),
                ("by", json!("resolver")),
                ("unreviewed", json!(true)),
                ("comment", json!(comment)),
                ("plan_sha256", json!(sha)),
            ]),
        );
        Ok(next)
    }

    /// One review pass at `stage`, Design review or Verify, shared by both
    /// (OWL-303, OWL-304): each seat of the roster on Claude Code, on the
    /// reviewers' routing, another model than the author's when one is
    /// configured (OWL-347); then the opt-in Codex reviewer, or the reason
    /// the pass runs without it, kept as the pass's `dropped` (OWL-348); the
    /// arbiter when the pass has findings the project confirms so; the pass
    /// settled by the
    /// core; its `ruling`, `pass` and `residue` events, `next` naming what
    /// each outcome leads to; and the review record with the pass, checked,
    /// not kept yet. The seats read `plan` and, at Verify, `diff`; from pass
    /// 2, what changed `since` the previous pass and the loop's earlier
    /// findings (OWL-356). The pass keeps what it reviewed: `head` at
    /// Verify, the plan's print at Design review.
    ///
    /// With `resolution`, the rebase whose resolution of a conflict on an
    /// overlap it reviews at Watch (OWL-449, architecture section 5, "A
    /// review after a resolution"), a Verify pass of its own: numbered after
    /// the record's last Verify pass, so it never stands in for one, given no
    /// earlier finding, its seats told the rebase (`rebase`), settled as a
    /// last pass, and its `review` event marked `resolution`.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn run_review_pass(
        &self,
        p: &mut Prepared,
        dispatched: &Dispatched,
        state: &mut TicketState,
        stage: Stage,
        plan: Option<GivenPlan>,
        diff: Option<GivenDiff>,
        since: Option<ReviewSince>,
        head: Option<String>,
        next: &dyn Fn(PassOutcome) -> &'static str,
        resolution: Option<&RebaseBrief>,
        sink: &mut EventSink<'_>,
    ) -> Result<PassRun, Stop> {
        let ticket = p.ticket.clone();
        let current = person(p.current()?, &p.found);
        let (role, router) = if stage == Stage::Verify {
            (Role::Verify, self.verify)
        } else {
            (Role::DesignReview, self.design_review)
        };
        let stage_key = stage_name(stage);
        let pass = match resolution {
            Some(_) => resolution_pass(p.review.as_ref()),
            None => state.review_pass_due(),
        };
        let settled_at = match resolution {
            Some(_) => pass.max(owlshift_core::state::MAX_REVIEW_PASSES),
            None => pass,
        };
        let pass_id = NonZeroU32::new(pass).expect("a pass due counts from 1");
        let models = &self.config.models;
        let routed = reviewer_routing(dispatched.routing, |tier| claude_model(models, tier));
        let model = claude_model(models, routed.routing.tier);
        let seats = roster::roster(p.pipeline.variant(), stage);
        let earlier = match resolution {
            Some(_) => Vec::new(),
            None => earlier_findings(p.review.as_ref(), stage, pass),
        };
        let seat_brief = |p: &Prepared, mandates: &[Mandate]| -> Result<Brief, Stop> {
            let comments = self.comments(&ticket)?;
            let mut brief = self.brief(p, role, &comments, &dispatched.rules, None, &current);
            brief.plan = plan.clone();
            // A review loop's Verify pass is given the reviews the loop
            // answers (OWL-396); Design review and a first delivery's none.
            if stage == Stage::Verify {
                brief.pull_request_reviews = loop_reviews(p);
            }
            brief.review = Some(ReviewBrief {
                mandates: mandates.to_vec(),
                pass: pass_id,
                diff: diff.clone(),
                since: since.clone(),
                earlier: earlier.clone(),
            });
            brief.rebase = resolution.cloned();
            Ok(brief)
        };
        let mut outputs: Vec<SeatOutput> = Vec::new();
        let mut routings: Vec<RunRouting> = Vec::new();
        let mut last_run = None;
        let mut cost = Some(0.0_f64);
        // Claude Code at its usage limit, the declared fallback takes the
        // whole roster for this pass (OWL-406): another model family than the
        // author's, on the opt-in Codex reviewer's routing. The opt-in seat
        // is then not added, the roster being on that harness already.
        let fallback = self.review_fallback(role);
        let (seat_router, seat_routing, seat_model, seat_same) = match fallback {
            Some(fallback) => {
                let routing = codex_reviewer_routing(dispatched.routing);
                let router = if stage == Stage::Verify {
                    fallback.verify
                } else {
                    fallback.design_review
                };
                (router, routing, codex_model(models, routing.tier), false)
            }
            None => (router, routed.routing, model, routed.same_model),
        };
        if fallback.is_some() {
            let until = self.limited_until(HarnessKind::Claude);
            sink.emit(
                &ticket,
                None,
                EventKind::Decision,
                data([
                    ("routing", json!("fallback")),
                    ("reason", json!("usage_limit")),
                    ("role", json!(role.as_str())),
                    ("stage", json!(stage_key)),
                    ("pass", json!(pass)),
                    ("harness", json!(seat_routing.harness)),
                    ("in_place_of", json!(HarnessKind::Claude)),
                    ("until", json!(until)),
                ]),
            );
        }
        for seat in seats {
            let harness = seat_router.harness(seat_routing);
            let brief = seat_brief(p, seat.mandates)?;
            let (ran, result) = self.review_run(p, &*harness, state, &brief, sink)?;
            last_run = Some(ran.run.clone());
            if fallback.is_none() {
                cost = cost.zip(run_cost(&ran)).map(|(sum, run)| sum + run);
            }
            routings.push(seat_routing);
            outputs.push(SeatOutput {
                reviewer: Reviewer {
                    role,
                    mandates: seat.mandates.to_vec(),
                    harness: seat_routing.harness,
                    model: seat_model.unwrap_or("default").to_owned(),
                    same_model: seat_same,
                    fallback: fallback.is_some(),
                },
                model: seat_model.map(str::to_owned),
                same_model: seat_same,
                findings: result.findings,
            });
        }
        // The opt-in Codex reviewer, after the roster's seats (OWL-348). Its
        // cost, which Codex does not report, stays out of the pass's sum, so
        // a Codex seat leaves the chain's figure known (OWL-342).
        let mut dropped = None;
        if let (Some(codex), None) = (self.codex, fallback) {
            let seat = roster::CODEX_REVIEWER;
            let reason = match p.codex.clone() {
                CodexSeat::Off => None,
                CodexSeat::Dropped(why) => Some(why),
                CodexSeat::Ready => {
                    let routing = codex_reviewer_routing(dispatched.routing);
                    let model = codex_model(models, routing.tier);
                    let router = if stage == Stage::Verify {
                        codex.verify
                    } else {
                        codex.design_review
                    };
                    let harness = router.harness(routing);
                    let brief = seat_brief(p, seat.mandates)?;
                    match self.codex_review_run(p, &*harness, state, &brief, sink)? {
                        Ok((ran, result)) => {
                            let run = ran.run.clone();
                            let mut findings = result.findings;
                            // The pass keeps the roster's findings whole:
                            // Codex's fill the room left.
                            let room = MAX_PASS_FINDINGS
                                .saturating_sub(outputs.iter().map(|o| o.findings.len()).sum());
                            if findings.len() > room {
                                sink.emit(
                                    &ticket,
                                    Some(&run),
                                    EventKind::Warning,
                                    data([
                                        ("what", json!("codex_findings_cut")),
                                        ("stage", json!(stage_key)),
                                        ("pass", json!(pass)),
                                        ("found", json!(findings.len())),
                                        ("kept", json!(room)),
                                    ]),
                                );
                                findings.truncate(room);
                            }
                            last_run = Some(run);
                            routings.push(routing);
                            outputs.push(SeatOutput {
                                reviewer: Reviewer {
                                    role,
                                    mandates: seat.mandates.to_vec(),
                                    harness: routing.harness,
                                    model: model.unwrap_or("default").to_owned(),
                                    same_model: false,
                                    fallback: false,
                                },
                                model: model.map(str::to_owned),
                                same_model: false,
                                findings,
                            });
                            None
                        }
                        Err(why) => Some(why),
                    }
                }
            };
            if let Some(reason) = reason {
                sink.emit(
                    &ticket,
                    None,
                    EventKind::Warning,
                    data([
                        ("what", json!("codex_reviewer_dropped")),
                        ("stage", json!(stage_key)),
                        ("pass", json!(pass)),
                        ("reason", json!(reason)),
                    ]),
                );
                dropped = Some(DroppedReviewer {
                    harness: HarnessKind::Codex,
                    mandates: seat.mandates.to_vec(),
                    reason,
                });
            }
        }
        let confirmation = self.config.pipeline.findings();
        let mut findings = pass_findings(&outputs, None);
        if confirmation == FindingsConfirmation::Arbiter && !findings.is_empty() {
            let comments = self.comments(&ticket)?;
            let mut brief = self.brief(
                p,
                Role::Arbiter,
                &comments,
                &dispatched.rules,
                None,
                &current,
            );
            brief.plan = plan.clone();
            if stage == Stage::Verify {
                brief.pull_request_reviews = loop_reviews(p);
            }
            brief.rebase = resolution.cloned();
            brief.findings = Some(GivenFindings {
                mode: FindingsMode::Rule,
                findings: findings.iter().map(|kept| kept.finding.clone()).collect(),
                diff: diff.clone(),
            });
            let routing =
                role_routing(Role::Arbiter, dispatched.routing).expect("the arbiter has its layer");
            let harness = self.arbiter.harness(routing);
            let (ran, result) = self.review_run(p, &*harness, state, &brief, sink)?;
            cost = cost.zip(run_cost(&ran)).map(|(sum, run)| sum + run);
            findings = pass_findings(&outputs, Some(&result.rulings));
            let ruled = |outcome: RulingOutcome| {
                finding_ids(findings.iter().filter(|kept| {
                    kept.ruling
                        .as_ref()
                        .is_some_and(|ruling| ruling.outcome == outcome)
                }))
            };
            sink.emit(
                &ticket,
                Some(&ran.run),
                EventKind::Decision,
                data([
                    ("review", json!("ruling")),
                    ("stage", json!(stage_key)),
                    ("pass", json!(pass)),
                    ("confirmed", ruled(RulingOutcome::Confirmed)),
                    ("rejected", ruled(RulingOutcome::Rejected)),
                ]),
            );
            last_run = Some(ran.run.clone());
        }
        let verdict = settle_pass(stage, settled_at, &mut findings, confirmation)
            .expect("a review stage settles");
        let reviewers_data: Vec<Value> = outputs
            .iter()
            .zip(&routings)
            .map(|(output, routing)| {
                json!({
                    "mandates": output.reviewer.mandates,
                    "harness": harness_key(output.reviewer.harness),
                    "tier": routing.tier,
                    "effort": routing.effort,
                    "model": output.model,
                    "same_model": output.same_model,
                    "fallback": output.reviewer.fallback,
                    "findings": output.findings.len(),
                })
            })
            .collect();
        let mut pass_data = data([
            ("review", json!("pass")),
            ("stage", json!(stage_key)),
            ("pass", json!(pass)),
            ("outcome", json!(outcome_name(verdict.outcome))),
            ("next", json!(next(verdict.outcome))),
            ("findings", json!(findings.len())),
            ("standing", json!(standing(&findings, confirmation).len())),
            ("reviewers", json!(reviewers_data)),
        ]);
        if resolution.is_some() {
            pass_data.insert("resolution".to_owned(), json!(true));
        }
        if let Some(dropped) = &dropped {
            pass_data.insert(
                "dropped".to_owned(),
                json!({
                    "harness": harness_key(dropped.harness),
                    "mandates": dropped.mandates,
                    "reason": dropped.reason,
                }),
            );
        }
        sink.emit(&ticket, last_run.as_deref(), EventKind::Decision, pass_data);
        if verdict.outcome != PassOutcome::Revise {
            // Design review notes a `minor` for the Build (OWL-356); what a
            // last pass leaves goes to its author's last run, and Verify's
            // `minor` findings to the last fix (OWL-356, OWL-377). No finding
            // is asked: `asked` stays empty, a role's own question being in
            // its run's events.
            let lists: &[(&str, Fate)] = if stage == Stage::Verify {
                &[("last_fix", Fate::LastFix)]
            } else {
                &[("noted", Fate::Note), ("last_fix", Fate::LastFix)]
            };
            let standing = standing(&findings, confirmation);
            let left = |fate: Fate| -> Vec<&KeptFinding> {
                standing
                    .iter()
                    .zip(&verdict.fates)
                    .filter(|(_, f)| **f == fate)
                    .map(|((n, _), _)| &findings[*n])
                    .collect()
            };
            if lists.iter().any(|(_, fate)| !left(*fate).is_empty()) {
                let mut residue = data([
                    ("review", json!("residue")),
                    ("stage", json!(stage_key)),
                    ("pass", json!(pass)),
                    ("asked", json!([])),
                ]);
                for (label, fate) in lists {
                    residue.insert((*label).to_owned(), finding_ids(left(*fate)));
                }
                sink.emit(&ticket, last_run.as_deref(), EventKind::Decision, residue);
            }
        }
        let mut review = p.review.clone().unwrap_or_default();
        push_pass(
            &mut review,
            ReviewPass {
                stage,
                pass: pass_id,
                findings: findings.clone(),
                reviewers: outputs
                    .iter()
                    .map(|output| output.reviewer.clone())
                    .collect(),
                head,
                plan: plan
                    .as_ref()
                    .filter(|_| stage == Stage::DesignReview)
                    .map(|given| plan_print(&given.plan)),
                revision: None,
                dropped,
            },
        );
        review.validate().map_err(|e| {
            Stop::Refused(format!(
                "the review record of {ticket} would not read back ({e}): `owlshift forget \
                 {ticket}` starts it over"
            ))
        })?;
        Ok(PassRun {
            pass,
            outputs,
            findings,
            verdict,
            last_run,
            cost,
            review,
        })
    }

    /// One Design review pass on the kept plan ([`Self::run_review_pass`]),
    /// then a loop back to Design, Design's last revision (OWL-377), or the
    /// plan leaving its review ([`Self::plan_leaves_review`]). The pass of a
    /// last revision ([`revision_due`]) runs no seat: the revised plan
    /// leaves its review as it is.
    fn review_pass(
        &self,
        p: &mut Prepared,
        dispatched: &Dispatched,
        mut state: TicketState,
        sink: &mut EventSink<'_>,
    ) -> Result<TicketState, Stop> {
        let ticket = p.ticket.clone();
        let kept = p.plan.clone().ok_or_else(|| {
            Stop::Refused(format!(
                "{ticket} is at design review but its ref keeps no plan: `owlshift forget \
                 {ticket}` starts its record over"
            ))
        })?;
        let gate = self.plan_gate(p, &kept);
        if let Some(run) = p
            .review
            .as_ref()
            .and_then(|review| revision_due(&state, review))
        {
            let review = p.review.clone().unwrap_or_default();
            let pass = state.review_pass_due();
            let answered = revised_findings(&review, pass);
            sink.emit(
                &ticket,
                Some(&run),
                EventKind::Decision,
                data([
                    ("review", json!("last_revision")),
                    ("stage", json!("design_review")),
                    ("pass", json!(pass)),
                    (
                        "fixed",
                        finding_ids(
                            answered
                                .iter()
                                .filter(|kept| kept.outcome == Some(FindingOutcome::Fixed)),
                        ),
                    ),
                    (
                        "disagreed",
                        finding_ids(
                            answered
                                .iter()
                                .filter(|kept| kept.outcome == Some(FindingOutcome::Disagreed)),
                        ),
                    ),
                    (
                        "next",
                        json!(if gate.is_open() { "build" } else { "approval" }),
                    ),
                ]),
            );
            let end = PlanEnd {
                pass,
                reviewers: Vec::new(),
                earlier: earlier_left(&review, pass.saturating_sub(1)),
                findings: answered,
                revision: Some(Unreviewed::LastRevision),
                run: Some(run),
                cost: Some(0.0),
                review,
            };
            return self.plan_leaves_review(p, dispatched, state, kept, gate, end, sink);
        }
        let given = GivenPlan {
            base: kept.base.clone(),
            plan: kept.plan.clone(),
        };
        let next = |outcome: PassOutcome| match outcome.design_review_event(gate) {
            Event::LoopBack | Event::LastFix => "design",
            Event::Completed => "build",
            _ => "approval",
        };
        let since = design_since(p.review.as_ref(), state.review_pass_due(), &kept.plan);
        let PassRun {
            pass,
            outputs,
            findings,
            verdict,
            last_run,
            cost,
            mut review,
        } = self.run_review_pass(
            p,
            dispatched,
            &mut state,
            Stage::DesignReview,
            Some(given),
            None,
            since,
            None,
            &next,
            None,
            sink,
        )?;
        let event = verdict.outcome.design_review_event(gate);
        if matches!(event, Event::LoopBack | Event::LastFix) {
            // What a last pass leaves goes to Design's last revision, the
            // mark kept with the pass (OWL-377).
            if event == Event::LastFix {
                review.last_fix = NonZeroU32::new(pass);
            }
            let next = match state.apply(p.pipeline, event) {
                Ok(Transition::To(next)) => next,
                other => return Err(core_error(other)),
            };
            p.review = Some(review);
            let questions = p.questions();
            self.store(p, &next, questions)
                .map_err(|e| refused("keeping the review pass in the ticket's ref", e))?;
            return Ok(next);
        }
        let end = PlanEnd {
            pass,
            reviewers: outputs
                .iter()
                .map(|output| PlanReviewer {
                    mandates: output.reviewer.mandates.clone(),
                    harness: output.reviewer.harness,
                    model: output.model.clone(),
                    same_model: output.same_model,
                    fallback: output.reviewer.fallback,
                })
                .collect(),
            earlier: earlier_left(&review, pass),
            findings,
            revision: None,
            run: last_run,
            cost,
            review,
        };
        self.plan_leaves_review(p, dispatched, state, kept, gate, end, sink)
    }

    /// The plan leaves its review, after a pass that passed or the pass of
    /// Design's last revision (`end`): the PLAN comment first, so nothing
    /// of the pass is kept when it cannot be posted, then Build when the
    /// plan gate is open, else the approval round, through the resolver
    /// first under `on-fork` (OWL-342) and the decider's alone under
    /// `always`. No finding is asked (OWL-377).
    #[allow(clippy::too_many_arguments)]
    fn plan_leaves_review(
        &self,
        p: &mut Prepared,
        dispatched: &Dispatched,
        state: TicketState,
        mut kept: TicketPlan,
        gate: owlshift_core::gate::PlanGate,
        end: PlanEnd,
        sink: &mut EventSink<'_>,
    ) -> Result<TicketState, Stop> {
        let ticket = p.ticket.clone();
        let event = PassOutcome::Passed.design_review_event(gate);
        let PlanEnd {
            pass,
            reviewers,
            earlier,
            findings,
            revision,
            run,
            cost,
            review,
        } = end;
        let comment = PlanComment {
            ticket: ticket.clone(),
            plan: kept.plan.clone(),
            pass,
            reviewers,
            findings: findings.clone(),
            earlier,
            revision,
            dropped: dropped_passes(&loop_passes(&review, Stage::DesignReview, pass)),
            next: if event == Event::Completed {
                PlanNext::Build
            } else {
                PlanNext::Approval
            },
            run: run.clone(),
        };
        // On a tracker with documents, the plan goes in the ticket's document
        // and the comment points to it (OWL-350).
        let posted = if self
            .tracker
            .capabilities()
            .contains(&tracker::Capability::Documents)
        {
            let pointer = self.publish_plan(p, &state, &comment, &review, sink)?;
            kept.published = p.plan.as_ref().and_then(|plan| plan.published.clone());
            Writer::new(self.tracker).post_plan_pointer(&pointer)
        } else {
            Writer::new(self.tracker).post_plan(&comment)
        }
        .map_err(|e| refused("posting the PLAN comment on the ticket", e))?;
        sink.emit(
            &ticket,
            run.as_deref(),
            EventKind::TrackerWrite,
            comment_written("PLAN", &posted),
        );
        self.fold(&ticket, run.as_deref(), &posted, sink);
        if event == Event::Completed {
            p.review = Some(review);
            let next = match state.apply_gated(p.pipeline, Event::Completed, gate) {
                Ok(Transition::To(next)) => next,
                other => return Err(core_error(other)),
            };
            let questions = p.questions();
            self.store(p, &next, questions)
                .map_err(|e| refused("keeping the review pass in the ticket's ref", e))?;
            return Ok(next);
        }
        let summary = match revision {
            Some(Unreviewed::LastRevision) => format!(
                "The plan's last revision, after design review pass {}, waits for your \
                 approval: the PLAN comment above shows it and what it answered.",
                pass.saturating_sub(1)
            ),
            Some(Unreviewed::Change) => format!(
                "The plan, revised on the resolver's change after design review pass {}, waits \
                 for your approval: the PLAN comment above shows it.",
                pass.saturating_sub(1)
            ),
            None => format!(
                "The plan passed design review pass {pass} and waits for your approval: the \
                 PLAN comment above shows it."
            ),
        };
        let asked = vec![approval_question(&kept.plan, plan_link(&ticket, &kept))];
        // The approval is the decider's alone under `always` (OWL-342).
        let direct: Vec<QuestionId> = if p.policy.approval_is_human(p.pipeline) {
            vec![QuestionId::nth(1)]
        } else {
            Vec::new()
        };
        let run =
            run.ok_or_else(|| Stop::Refused("a design review pass that ran no run".to_owned()))?;
        let current = person(p.current()?, &p.found);
        let asking = Asked {
            role: Role::DesignReview,
            run: &run,
            cost,
            status: result::Status::Questions,
            summary: &summary,
            questions: &asked,
            reopened: &[],
            direct: &direct,
            kept_findings: &findings,
            plan: Some(GivenPlan {
                base: kept.base.clone(),
                plan: kept.plan.clone(),
            }),
        };
        // Through the resolver first (OWL-342), the ref keeping the review
        // record without this pass until the pass's outcome is kept with
        // the state, so a command cut short runs the pass again.
        let routing = self.resolve(p, &state, &asking, &dispatched.rules, &current, sink)?;
        p.review = Some(review);
        match routing {
            Routing::Settled(decided) => {
                // The plan approved, option A: kept with the DECISION comment
                // and the plan's hash, and the gate takes it to Build.
                let approval = decided.iter().find(|d| d.option.as_deref() == Some("A"));
                if let Some(approval) = approval {
                    return self.approved_by_resolver(p, state, kept, approval, &run, sink);
                }
                // A plan changed: the next design run follows the decision,
                // as it follows the answer to a round.
                let next = match state.apply(p.pipeline, Event::RoundDecided) {
                    Ok(Transition::To(next)) => next,
                    other => return Err(core_error(other)),
                };
                let questions = p.questions();
                self.store(p, &next, questions)
                    .map_err(|e| refused("keeping the review pass in the ticket's ref", e))?;
                Ok(next)
            }
            Routing::Ask(round) => {
                let waiting = match state.apply(p.pipeline, Event::Questions) {
                    Ok(Transition::To(waiting)) => waiting,
                    other => return Err(core_error(other)),
                };
                Err(self.ask(
                    p,
                    &waiting,
                    result::Status::Questions,
                    &summary,
                    round,
                    &run,
                    sink,
                ))
            }
        }
    }
    /// Publishes the plan of `comment` in the ticket's document (OWL-350,
    /// architecture section 5, "Kept and shown") and returns the PLAN
    /// comment that points to it. The ticket's [`PublishedPlan`] says which
    /// document and version: a plan whose hash is the published one keeps
    /// its version, and, at the same pass, a `continue` running the pass
    /// again, the whole record; another plan is the next version, its
    /// changed parts named. A document the tracker no longer shows is made
    /// again. A document just made is kept in the ref at once, before the
    /// comment, so a comment that cannot be posted never leaves a second
    /// one: the state is as the ref keeps it, every change of it in the pass
    /// being kept when made, and the review record does not take the pass
    /// yet, so `continue` runs it again. `p.plan` keeps the new record.
    fn publish_plan(
        &self,
        p: &mut Prepared,
        state: &TicketState,
        comment: &PlanComment,
        review: &TicketReview,
        sink: &mut EventSink<'_>,
    ) -> Result<PlanPointer, Stop> {
        let ticket = p.ticket.clone();
        let kept = p
            .plan
            .as_ref()
            .ok_or_else(|| Stop::Refused(format!("{ticket} has no plan to publish")))?;
        let pass = NonZeroU32::new(comment.pass)
            .ok_or_else(|| Stop::Refused("a review pass numbered 0".to_owned()))?;
        let plan_sha256 = plan_sha256(&kept.plan);
        let parts = part_hashes(&kept.plan);
        let prior = kept.published.clone();
        let (version, change) = match &prior {
            None => (NonZeroU32::MIN, PlanChange::First),
            Some(prior) if prior.plan_sha256 == plan_sha256 && prior.pass == pass => {
                (prior.version, prior.change.clone())
            }
            Some(prior) if prior.plan_sha256 == plan_sha256 => {
                (prior.version, PlanChange::Unchanged)
            }
            Some(prior) => {
                let changed: Vec<PlanPart> = PlanPart::ALL
                    .iter()
                    .zip(&parts)
                    .zip(&prior.parts)
                    .filter(|((_, now), then)| now != then)
                    .map(|((part, _), _)| *part)
                    .collect();
                let next = prior.version.checked_add(1).ok_or_else(|| {
                    Stop::Refused(format!("{ticket}'s plan has no version number left"))
                })?;
                let change = if changed.is_empty() {
                    PlanChange::Unchanged
                } else {
                    PlanChange::Changed {
                        since: prior.version,
                        parts: changed,
                    }
                };
                (next, change)
            }
        };
        let document = PlanDocument {
            ticket: ticket.clone(),
            plan: kept.plan.clone(),
            version: version.get(),
            passes: loop_passes(review, Stage::DesignReview, comment.pass)
                .into_iter()
                .map(|kept| DocumentPass {
                    pass: kept.pass.get(),
                    reviewers: kept.reviewers.iter().map(PlanReviewer::from).collect(),
                    findings: kept.findings.clone(),
                    // A resolver's change applied with no pass is the
                    // loop's last pass (OWL-463); an earlier revision is
                    // Design's last revision (OWL-377).
                    revision: kept.revision.as_ref().map(|_| match comment.revision {
                        Some(kind) if kept.pass.get() == comment.pass => kind,
                        _ => Unreviewed::LastRevision,
                    }),
                    dropped: kept.dropped.clone(),
                })
                .collect(),
        };
        let writer = Writer::new(self.tracker);
        let existing = prior.as_ref().map(|prior| prior.document.as_str());
        let published = match writer.publish_plan(existing, &document) {
            Err(WriteError::Tracker(e))
                if existing.is_some() && e.kind == tracker::ErrorKind::NotFound =>
            {
                // A person deleted it: the plan gets a document again.
                writer.publish_plan(None, &document)
            }
            other => other,
        }
        .map_err(|e| refused("publishing the plan's document on the ticket", e))?;
        let made = existing != Some(published.id.as_str());
        let record = PublishedPlan {
            document: published.id.clone(),
            url: published.url.clone(),
            version,
            pass,
            plan_sha256,
            parts,
            change: change.clone(),
        };
        if let Some(plan) = p.plan.as_mut() {
            plan.published = Some(record);
        }
        sink.emit(
            &ticket,
            comment.run.as_deref(),
            EventKind::TrackerWrite,
            data([
                ("target", json!("tracker")),
                ("action", json!("document")),
                ("kind", json!("PLAN")),
                ("document", json!(published.id)),
                ("made", json!(made)),
                ("version", json!(version.get())),
            ]),
        );
        if made {
            let questions = p.questions();
            self.store(p, state, questions)
                .map_err(|e| refused("keeping the plan's document in the ticket's ref", e))?;
        }
        Ok(PlanPointer {
            ticket,
            version: version.get(),
            pass: comment.pass,
            url: published.url,
            change,
            forks: comment.plan.forks.len(),
            open: comment.open_findings(),
            next: comment.next,
            run: comment.run.clone(),
        })
    }

    /// The plan the resolver approved (OWL-342, option A of the approval
    /// question): kept in `plan.json` with its DECISION comment and the
    /// plan's hash, where the decider's approval keeps their comment, then
    /// the plan gate, which holds the hash as for any approval (OWL-32),
    /// takes the ticket to Build. One write keeps it with the state. At the
    /// resolver's bound at `all`, the runner approves it the same way on the
    /// approval question's recommendation (OWL-375), which its `plan` event
    /// says.
    fn approved_by_resolver(
        &self,
        p: &mut Prepared,
        state: TicketState,
        mut kept: TicketPlan,
        approval: &DecidedOn,
        run: &str,
        sink: &mut EventSink<'_>,
    ) -> Result<TicketState, Stop> {
        let sha = plan_sha256(&kept.plan);
        let comment = approval.comment.as_str();
        let by = if approval.kind == BasisKind::Recommendation {
            "recommendation"
        } else {
            "resolver"
        };
        kept.approval = Some(KeptApproval {
            comment: comment.to_owned(),
            plan_sha256: sha.clone(),
        });
        let gate = self.plan_gate(p, &kept);
        let next = match state.apply_gated(p.pipeline, Event::Completed, gate) {
            Ok(Transition::To(next)) => next,
            other => return Err(core_error(other)),
        };
        p.plan = Some(kept);
        let questions = p.questions();
        self.store(p, &next, questions)
            .map_err(|e| refused("keeping the plan's approval in the ticket's ref", e))?;
        sink.emit(
            &p.ticket,
            Some(run),
            EventKind::Decision,
            data([
                ("plan", json!("approved")),
                ("by", json!(by)),
                ("comment", json!(comment)),
                ("plan_sha256", json!(sha)),
            ]),
        );
        Ok(next)
    }
}

/// Why the opt-in Codex reviewer's run leaves its pass without it
/// (OWL-348), in the runner's words: never the harness's output, which the
/// tracker would publish.
fn codex_drop_reason(outcome: &Outcome, event: Event) -> String {
    match outcome {
        Outcome::UsageLimit { resets_at } => match resets_at {
            Some(at) => format!("Codex reached its usage limit, until {at}."),
            None => "Codex reached its usage limit.".to_owned(),
        },
        Outcome::Failed(Failure::TimedOut) => "Codex's run outlived its deadline.".to_owned(),
        Outcome::Failed(Failure::Credentials(_)) => {
            "Codex's run loaded something that hands it a credential.".to_owned()
        }
        Outcome::Failed(Failure::Harness(_) | Failure::Driver(_)) => {
            "Codex's run failed, or Owlshift stopped it.".to_owned()
        }
        Outcome::Failed(Failure::NoResult) => "Codex's run left no result.".to_owned(),
        Outcome::Failed(_) => "Codex's result was refused.".to_owned(),
        Outcome::Quarantined(_) => "Codex's run broke isolation.".to_owned(),
        Outcome::Stopped => "Codex's run was stopped by a checked stop.".to_owned(),
        Outcome::Finished { .. } => match event {
            Event::Questions => "Codex's run asked questions, which a reviewer may not.".to_owned(),
            Event::Blocked => "Codex's run ended blocked.".to_owned(),
            _ => "Codex's run gave no review.".to_owned(),
        },
    }
}

#[cfg(test)]
mod tests {
    use owlshift_contracts::review::{Alternative, Fork, Mandate, PlanStep, Ruling};
    use owlshift_core::state::{Status, TicketState};

    use crate::writer::MAX_POINTER_URL_CHARS;

    use super::*;

    fn finding(n: u32, severity: Severity) -> Finding {
        Finding {
            id: FindingId::nth(n),
            severity,
            claim: format!("Claim {n}."),
            evidence: "src/lib.rs:1".to_owned(),
            criterion: None,
            settle: "Settle it.".to_owned(),
        }
    }

    fn reviewer(mandates: &[Mandate], harness: HarnessKind) -> Reviewer {
        Reviewer {
            role: Role::DesignReview,
            mandates: mandates.to_vec(),
            harness,
            model: "default".to_owned(),
            same_model: false,
            fallback: false,
        }
    }

    fn seat(mandates: &[Mandate], harness: HarnessKind, findings: Vec<Finding>) -> SeatOutput {
        SeatOutput {
            reviewer: reviewer(mandates, harness),
            model: None,
            same_model: false,
            findings,
        }
    }

    fn ruling(n: u32, outcome: RulingOutcome) -> Ruling {
        Ruling {
            finding: FindingId::nth(n),
            outcome,
            reason: format!("Checked {n}."),
        }
    }

    /// Findings are numbered across the seats in seat order, each with its
    /// reviewer and its ruling matched by the pass's id.
    #[test]
    fn a_pass_numbers_its_findings_across_seats_with_their_rulings() {
        let seats = [
            seat(
                &[Mandate::Critic],
                HarnessKind::Codex,
                vec![finding(1, Severity::Major), finding(2, Severity::Minor)],
            ),
            seat(
                &[Mandate::Conformance],
                HarnessKind::Claude,
                vec![finding(1, Severity::Blocker)],
            ),
        ];
        let rulings = [
            ruling(1, RulingOutcome::Confirmed),
            ruling(2, RulingOutcome::Rejected),
            ruling(3, RulingOutcome::Confirmed),
        ];
        let kept = pass_findings(&seats, Some(&rulings));
        let ids: Vec<&str> = kept.iter().map(|k| k.finding.id.as_str()).collect();
        assert_eq!(ids, ["F1", "F2", "F3"]);
        assert_eq!(kept[2].finding.severity, Severity::Blocker);
        assert_eq!(kept[2].reviewer.harness, HarnessKind::Claude);
        assert_eq!(
            kept[1]
                .ruling
                .as_ref()
                .map(|r| (r.outcome, r.reason.as_str())),
            Some((RulingOutcome::Rejected, "Checked 2."))
        );
        assert!(
            pass_findings(&seats, None)
                .iter()
                .all(|k| k.ruling.is_none())
        );
    }

    /// The arbiter's confirmed findings stand, or the author's unrejected
    /// ones; the settled pass marks a rejected ruling and an asked finding.
    #[test]
    fn a_pass_settles_on_its_standing_findings_and_marks_their_outcomes() {
        let seats = [seat(
            &[Mandate::Critic, Mandate::Conformance],
            HarnessKind::Claude,
            vec![
                finding(1, Severity::Major),
                finding(2, Severity::Major),
                finding(3, Severity::Minor),
            ],
        )];
        let rulings = [
            ruling(1, RulingOutcome::Confirmed),
            ruling(2, RulingOutcome::Rejected),
            ruling(3, RulingOutcome::Confirmed),
        ];
        // Before the last pass: the confirmed major is fixed, a revision,
        // and the minor noted (OWL-356).
        let mut first = pass_findings(&seats, Some(&rulings));
        let verdict = settle_pass(
            Stage::DesignReview,
            1,
            &mut first,
            FindingsConfirmation::Arbiter,
        )
        .unwrap();
        assert_eq!(verdict.outcome, PassOutcome::Revise);
        assert_eq!(verdict.fates, [Fate::Fix, Fate::Note]);
        let outcomes: Vec<_> = first.iter().map(|k| k.outcome).collect();
        assert_eq!(outcomes, [None, Some(FindingOutcome::Rejected), None]);
        // The last pass: the major goes to the last revision, never asked
        // (OWL-377), the minor noted.
        let mut last = pass_findings(&seats, Some(&rulings));
        let verdict = settle_pass(
            Stage::DesignReview,
            2,
            &mut last,
            FindingsConfirmation::Arbiter,
        )
        .unwrap();
        assert_eq!(verdict.outcome, PassOutcome::LastFix);
        assert_eq!(verdict.fates, [Fate::LastFix, Fate::Note]);
        let outcomes: Vec<_> = last.iter().map(|k| k.outcome).collect();
        assert_eq!(outcomes, [None, Some(FindingOutcome::Rejected), None]);
        // A record kept under the arbiter, read under the author's mode
        // after an upgrade (OWL-391): the rejected ruling still holds.
        assert_eq!(standing(&last, FindingsConfirmation::Author).len(), 2);
        // The author answers after the pass: every finding stands at it.
        let mut authored = pass_findings(&seats, None);
        assert_eq!(standing(&authored, FindingsConfirmation::Author).len(), 3);
        authored[0].response = Some(KeptResponse {
            outcome: ResponseOutcome::Rejected,
            reason: "No.".to_owned(),
        });
        assert_eq!(standing(&authored, FindingsConfirmation::Author).len(), 2);
    }

    fn state(stage: Stage, waiting: bool, passes: u32) -> TicketState {
        let status = if waiting {
            Status::NeedsInput { return_to: stage }
        } else {
            Status::Active(stage)
        };
        TicketState::restore(status, u32::from(waiting), 0, 0, 0, 0)
            .unwrap()
            .with_review_passes(passes)
            .unwrap()
    }

    fn review(passes: &[(u32, Vec<KeptFinding>)]) -> TicketReview {
        TicketReview {
            format: Format,
            passes: passes
                .iter()
                .map(|(pass, findings)| ReviewPass {
                    stage: Stage::DesignReview,
                    pass: NonZeroU32::new(*pass).unwrap(),
                    findings: findings.clone(),
                    reviewers: Vec::new(),
                    head: None,
                    plan: None,
                    revision: None,
                    dropped: None,
                })
                .collect(),
            ..TicketReview::new()
        }
    }

    /// What a design run is given follows from the state's passes and the
    /// latest pass, settled again (A4 of OWL-303).
    #[test]
    fn a_design_run_is_given_what_the_latest_pass_left() {
        let arbiter = FindingsConfirmation::Arbiter;
        let author = FindingsConfirmation::Author;
        let at_design = state(Stage::Design, false, 0);
        // Nothing before a pass of this loop.
        assert_eq!(
            design_input(&at_design, None, arbiter, true),
            Ok(DesignInput::default())
        );
        let seats = [seat(
            &[Mandate::Critic],
            HarnessKind::Codex,
            vec![finding(1, Severity::Major), finding(2, Severity::Minor)],
        )];
        let rulings = [
            ruling(1, RulingOutcome::Confirmed),
            ruling(2, RulingOutcome::Rejected),
        ];
        let ruled = review(&[(1, pass_findings(&seats, Some(&rulings)))]);
        let after_one = state(Stage::Design, false, 1);
        // A revision: the confirmed one to fix.
        let fix = design_input(&after_one, Some(&ruled), arbiter, true).unwrap();
        let given = fix.findings.unwrap();
        assert_eq!(given.mode, FindingsMode::Fix);
        assert_eq!(given.findings.len(), 1);
        assert_eq!(given.findings[0].id, FindingId::nth(1));
        assert!(!fix.approval);
        // The author answers every one it fixes, never a minor, a note for
        // the Build (OWL-356).
        let unjudged = review(&[(1, pass_findings(&seats, None))]);
        let answer = design_input(&after_one, Some(&unjudged), author, true).unwrap();
        let given = answer.findings.unwrap();
        assert_eq!(
            (given.mode, given.findings.len()),
            (FindingsMode::Answer, 1)
        );
        assert_eq!(given.findings[0].id, FindingId::nth(1));
        // A pass with minors alone asks no revision (OWL-356).
        let minors = [seat(
            &[Mandate::Critic],
            HarnessKind::Codex,
            vec![finding(1, Severity::Minor)],
        )];
        let noted = review(&[(1, pass_findings(&minors, None))]);
        assert_eq!(
            design_input(&after_one, Some(&noted), arbiter, false),
            Ok(DesignInput::default())
        );
        // A clean pass was followed by the approval round, while it is still
        // required (the variant may have changed since).
        let clean = review(&[(1, Vec::new())]);
        let approval = DesignInput {
            findings: None,
            approval: true,
        };
        assert_eq!(
            design_input(&after_one, Some(&clean), arbiter, true),
            Ok(approval.clone())
        );
        assert_eq!(
            design_input(&after_one, Some(&clean), arbiter, false),
            Ok(DesignInput::default())
        );
        // The last pass's major goes to the last revision, to fix and
        // answer (OWL-377); the revision's pass, which no reviewer ran, was
        // followed by the approval round.
        let after_two = state(Stage::Design, false, 2);
        let mut last = review(&[
            (1, pass_findings(&seats, Some(&rulings))),
            (2, pass_findings(&seats, Some(&rulings))),
        ]);
        let revision = design_input(&after_two, Some(&last), arbiter, true).unwrap();
        let given = revision.findings.unwrap();
        assert_eq!(given.mode, FindingsMode::Fix);
        assert_eq!(given.findings[0].id, FindingId::nth(1));
        assert!(!revision.approval);
        last.passes.push(ReviewPass {
            revision: Some("run-9".to_owned()),
            ..review(&[(3, Vec::new())]).passes.remove(0)
        });
        assert_eq!(
            design_input(&state(Stage::Design, false, 3), Some(&last), arbiter, true),
            Ok(approval)
        );
        // A record that does not end with the state's pass is refused.
        assert!(design_input(&after_two, Some(&clean), arbiter, true).is_err());
        assert!(design_input(&after_one, None, arbiter, true).is_err());
    }

    /// Design's last revision (OWL-377): the design run due while the mark
    /// names the state's pass is it; its pass, kept with the plan, is the
    /// one the Design review stage reaches next, which answers the findings
    /// of the pass before it; the pass after an approval round that changed
    /// the plan is told what changed since the last pass that kept a print,
    /// the revision's keeping none.
    #[test]
    fn the_last_revision_is_kept_as_a_pass_no_reviewer_runs() {
        let seats = [seat(
            &[Mandate::Critic],
            HarnessKind::Claude,
            vec![finding(1, Severity::Major)],
        )];
        let printed = |n: u32, plan: &Plan| ReviewPass {
            plan: Some(plan_print(plan)),
            ..review(&[(n, pass_findings(&seats, None))]).passes.remove(0)
        };
        let before = plan(Vec::new());
        let mut record = TicketReview {
            passes: vec![printed(1, &before), printed(2, &before)],
            last_fix: NonZeroU32::new(2),
            ..TicketReview::new()
        };
        assert!(last_revision_marked(
            &state(Stage::Design, false, 2),
            &record
        ));
        assert!(!last_revision_marked(
            &state(Stage::Design, false, 1),
            &record
        ));
        assert!(!last_revision_marked(
            &state(Stage::DesignReview, false, 2),
            &record
        ));
        assert_eq!(
            revision_due(&state(Stage::DesignReview, false, 2), &record),
            None
        );
        push_pass(
            &mut record,
            ReviewPass {
                revision: Some("run-9".to_owned()),
                ..review(&[(3, Vec::new())]).passes.remove(0)
            },
        );
        assert_eq!(record.last_fix, None, "the revision's pass ends the mark");
        record.validate().unwrap();
        assert_eq!(
            revision_due(&state(Stage::DesignReview, false, 2), &record),
            Some("run-9".to_owned())
        );
        assert_eq!(
            revision_due(&state(Stage::DesignReview, false, 3), &record),
            None
        );
        let answered = revised_findings(&record, 3);
        assert_eq!(answered.len(), 1);
        assert_eq!(answered[0].finding.id, FindingId::nth(1));
        // After the approval round: what changed since pass 2's print.
        let mut after = before.clone();
        after.approach = "Another approach.".to_owned();
        let since = design_since(Some(&record), 4, &after).expect("a print to compare");
        assert_eq!(since.pass.get(), 2);
        assert_eq!(since.parts, [PlanPart::Approach]);
        assert!(design_since(Some(&record), 1, &after).is_none());
    }

    /// OWL-463: a resolver's change on the approval goes to Build with no
    /// pass only when its revision left the goal, criteria, forks and
    /// alternatives as they were, the round carried no `blocker` or `major`
    /// left unfixed, and the decision is the resolver's still: not reversed,
    /// no comment of the decider's after it, no ask after it.
    #[test]
    fn only_a_resolvers_change_for_the_notes_goes_unreviewed() {
        use owlshift_core::decider::DeciderRule;
        let at = |minute: i64| Timestamp::from_second(1_790_000_000 + minute * 60).unwrap();
        let decision = KeptDecision {
            at: at(10),
            comment: "decision-1".to_owned(),
            question: approval_question(&plan(Vec::new()), None),
            decision: "Change it: fold F1 into step 1.".to_owned(),
            basis: "The note is cheap to fold.".to_owned(),
            basis_kind: Some(BasisKind::Judgment),
            number: None,
            reversed: None,
        };
        let questions = |decision: KeptDecision| TicketQuestions {
            decisions: vec![decision],
            ..TicketQuestions::default()
        };
        let decided = questions(decision.clone());
        let mut asked_after = decided.clone();
        asked_after.asks.push(Ask {
            kind: AskKind::Questions,
            round: NonZeroU32::MIN,
            at: at(11),
            comment: "ask-1".to_owned(),
            questions: Vec::new(),
            decider: AskDecider {
                account: "ada".to_owned(),
                by: DeciderRule::Assignee,
            },
            verdicts: Vec::new(),
            result_refusal: None,
            labels: Default::default(),
        });
        let reversed = questions(KeptDecision {
            reversed: Some(owlshift_contracts::refs::Reversal {
                through: at(12),
                comment: "reply-1".to_owned(),
            }),
            ..decision
        });
        let comment = |minute: i64, relation: Relation| ThreadEntry::Comment {
            at: at(minute),
            author: Author {
                name: "ada".to_owned(),
                relation,
            },
            body: "No: keep step 1.".to_owned(),
        };
        let kept = |severity: Severity, outcome: Option<FindingOutcome>| KeptFinding {
            outcome,
            ..pass_findings(
                &[seat(
                    &[Mandate::Critic],
                    HarnessKind::Claude,
                    vec![finding(1, severity)],
                )],
                None,
            )
            .remove(0)
        };
        let minor = || kept(Severity::Minor, None);
        let passed = review(&[(1, vec![minor()])]);
        let rejected_major = review(&[(
            1,
            vec![
                kept(Severity::Major, Some(FindingOutcome::Rejected)),
                minor(),
            ],
        )]);
        let revised = |outcome: FindingOutcome| {
            let mut record = review(&[
                (1, vec![kept(Severity::Major, Some(FindingOutcome::Fixed))]),
                (2, vec![kept(Severity::Major, Some(outcome)), minor()]),
            ]);
            record.passes.push(ReviewPass {
                revision: Some("run-9".to_owned()),
                ..review(&[(3, Vec::new())]).passes.remove(0)
            });
            record
        };
        let fixed = revised(FindingOutcome::Fixed);
        let disagreed = revised(FindingOutcome::Disagreed);
        let before = plan(vec![Fork {
            choice: "Markdown.".to_owned(),
            options: vec!["Markdown.".to_owned(), "Plain text.".to_owned()],
            reason: "The docs are Markdown.".to_owned(),
        }]);
        let edit = |change: fn(&mut Plan)| {
            let mut after = before.clone();
            change(&mut after);
            after
        };
        let notes = edit(|plan| plan.steps[0].test = "T, the encoding checked.".to_owned());
        let fork = edit(|plan| plan.forks[0].choice = "Plain text.".to_owned());
        let reason = edit(|plan| plan.forks[0].reason = "Reworded.".to_owned());
        let alternative = edit(|plan| plan.alternatives.clear());
        let goal = edit(|plan| plan.goal = "G, wider.".to_owned());
        let criteria = edit(|plan| plan.criteria.push("C2".to_owned()));
        let approach = edit(|plan| plan.approach = "A, with a README line.".to_owned());
        let none: &[ThreadEntry] = &[];
        let later = [comment(11, Relation::Decider)];
        let earlier = [comment(9, Relation::Decider)];
        let others = [comment(11, Relation::Other)];
        type Case<'a> = (
            &'a [ThreadEntry],
            &'a TicketQuestions,
            &'a TicketReview,
            u32,
            &'a Plan,
            bool,
        );
        let cases: [Case<'_>; 15] = [
            // For the notes: no pass, whatever else the revision changed.
            (none, &decided, &passed, 1, &notes, true),
            (none, &decided, &passed, 1, &approach, true),
            (&earlier, &decided, &passed, 1, &notes, true),
            (&others, &decided, &passed, 1, &notes, true),
            // A fork or the scope touched, a reworded reason included.
            (none, &decided, &passed, 1, &fork, false),
            (none, &decided, &passed, 1, &reason, false),
            (none, &decided, &passed, 1, &alternative, false),
            (none, &decided, &passed, 1, &goal, false),
            (none, &decided, &passed, 1, &criteria, false),
            // A major the round carried: one the arbiter rejected, one the
            // last revision disagreed with; one it fixed is settled.
            (none, &decided, &rejected_major, 1, &notes, false),
            (none, &decided, &disagreed, 3, &notes, false),
            (none, &decided, &fixed, 3, &notes, true),
            // The decider's word: their comment after the decision, the
            // decision reversed, an ask after it.
            (&later, &decided, &passed, 1, &notes, false),
            (none, &reversed, &passed, 1, &notes, false),
            (none, &asked_after, &passed, 1, &notes, false),
        ];
        for (n, (thread, questions, record, passes, after, unreviewed)) in
            cases.into_iter().enumerate()
        {
            assert_eq!(
                unreviewed_change(thread, questions, Some(record), passes, &before, after),
                unreviewed.then(|| "decision-1".to_owned()),
                "case {n}"
            );
        }
        // No record, or one that does not end with the state's pass: the
        // pass is run.
        assert_eq!(
            unreviewed_change(none, &decided, None, 1, &before, &notes),
            None
        );
        assert_eq!(
            unreviewed_change(none, &decided, Some(&passed), 2, &before, &notes),
            None
        );
    }

    /// The PLAN comment lists the rejections and the notes of the loop's
    /// earlier passes, never those of an earlier loop; a later pass is told
    /// every earlier finding of its loop, a pass run again in place left
    /// out; the Build is given the loop's notes, numbered again (OWL-356).
    #[test]
    fn the_earlier_findings_are_the_loops_own() {
        let seats = [seat(
            &[Mandate::Critic],
            HarnessKind::Codex,
            vec![finding(1, Severity::Major), finding(2, Severity::Minor)],
        )];
        let rulings = [
            ruling(1, RulingOutcome::Rejected),
            ruling(2, RulingOutcome::Confirmed),
        ];
        let mut first = pass_findings(&seats, Some(&rulings));
        settle_pass(
            Stage::DesignReview,
            1,
            &mut first,
            FindingsConfirmation::Arbiter,
        );
        let noted = |n: u32| {
            let mut found = pass_findings(
                &[seat(
                    &[Mandate::Critic],
                    HarnessKind::Codex,
                    vec![finding(n, Severity::Minor)],
                )],
                None,
            );
            found[0].finding.id = FindingId::nth(1);
            found
        };
        let record = review(&[
            (1, first.clone()),
            (1, first.clone()),
            (2, noted(7)),
            (3, Vec::new()),
        ]);
        let left = earlier_left(&record, 3);
        let passes: Vec<u32> = left.iter().map(|(pass, _)| *pass).collect();
        assert_eq!(passes, [1, 1, 2], "the first loop's pass is left out");
        assert!(earlier_left(&record, 1).is_empty());
        let told = earlier_findings(Some(&record), Stage::DesignReview, 3);
        assert_eq!(told.len(), 3);
        // Pass 3 run again in place is told passes 1 and 2 alone.
        assert_eq!(
            earlier_findings(Some(&record), Stage::DesignReview, 3),
            earlier_findings(
                Some(&review(&record_passes(&record)[..3])),
                Stage::DesignReview,
                3
            )
        );
        assert!(earlier_findings(Some(&record), Stage::Verify, 2).is_empty());
        // The Build's notes: the confirmed minor of pass 1 and the minor of
        // pass 2, F1 and F2 again; the rejected major is none.
        let at_build = state(Stage::Build, false, 0);
        let notes =
            design_notes(&at_build, Some(&record), FindingsConfirmation::Arbiter).expect("notes");
        assert_eq!(notes.mode, FindingsMode::Note);
        let ids: Vec<&str> = notes.findings.iter().map(|f| f.id.as_str()).collect();
        assert_eq!(ids, ["F1", "F2"]);
        assert!(notes.findings[1].claim.contains('7'));
        assert!(
            design_notes(
                &state(Stage::Build, false, 1),
                Some(&record),
                FindingsConfirmation::Arbiter
            )
            .is_none(),
            "a Verify fix run is given Verify's findings"
        );
    }

    fn record_passes(record: &TicketReview) -> Vec<(u32, Vec<KeptFinding>)> {
        record
            .passes
            .iter()
            .map(|pass| (pass.pass.get(), pass.findings.clone()))
            .collect()
    }

    /// What changed in a plan, matched by content: a step inserted is new,
    /// the steps it renumbers are not, a changed step is new and its old
    /// text removed, and the parts whose text changed are named (OWL-356).
    #[test]
    fn a_later_pass_is_told_the_new_steps_and_parts() {
        let step = |text: &str| PlanStep {
            what: text.to_owned(),
            test: format!("{text} tested."),
        };
        let mut before = plan(Vec::new());
        before.steps = vec![step("A"), step("B")];
        let print = plan_print(&before);
        assert_eq!((print.parts.len(), print.steps.len()), (9, 2));
        let mut after = before.clone();
        after.steps.insert(0, step("Z"));
        after.steps[2] = step("B, revised");
        after.approach = "Another approach.".to_owned();
        let since = plan_since(NonZeroU32::MIN, &print, &after);
        assert_eq!(since.parts, [PlanPart::Approach, PlanPart::Steps]);
        let steps: Vec<u32> = since.steps.iter().map(|n| n.get()).collect();
        assert_eq!(steps, [1, 3]);
        assert_eq!(since.removed, 1);
        assert!(since.diff.is_none());
        let same = plan_since(NonZeroU32::MIN, &print, &before);
        assert!(same.parts.is_empty() && same.steps.is_empty() && same.removed == 0);
    }

    /// Two steps of one text count as two: a duplicate dropped is removed,
    /// a duplicate added is new (OWL-356).
    #[test]
    fn duplicated_steps_are_matched_one_for_one() {
        let step = |text: &str| PlanStep {
            what: text.to_owned(),
            test: format!("{text} tested."),
        };
        let mut twice = plan(Vec::new());
        twice.steps = vec![step("A"), step("A"), step("B")];
        let mut once = twice.clone();
        once.steps.remove(1);
        let dropped = plan_since(NonZeroU32::MIN, &plan_print(&twice), &once);
        assert!(dropped.steps.is_empty());
        assert_eq!(dropped.removed, 1);
        let added = plan_since(NonZeroU32::MIN, &plan_print(&once), &twice);
        let steps: Vec<u32> = added.steps.iter().map(|n| n.get()).collect();
        assert_eq!((steps, added.removed), (vec![2], 0));
    }

    fn plan(forks: Vec<Fork>) -> Plan {
        Plan {
            goal: "G".to_owned(),
            criteria: vec!["C".to_owned()],
            approach: "A".to_owned(),
            alternatives: vec![Alternative {
                approach: "B".to_owned(),
                reason: "R".to_owned(),
            }],
            files: Vec::new(),
            steps: vec![PlanStep {
                what: "S".to_owned(),
                test: "T".to_owned(),
            }],
            risks: Vec::new(),
            out_of_scope: Vec::new(),
            forks,
        }
    }

    /// Without the plan's document, the approval question offers to
    /// approve, then each fork's other options, and holds in a question's
    /// bytes whatever the plan's forks.
    #[test]
    fn the_approval_question_offers_the_forks_within_a_questions_bytes() {
        let fork = |choice: &str, other: &str| Fork {
            choice: choice.to_owned(),
            options: vec![choice.to_owned(), other.to_owned()],
            reason: "R".to_owned(),
        };
        let labels = |question: &Question| -> Vec<String> {
            question.options.iter().map(|o| o.label.clone()).collect()
        };
        let question = approval_question(&plan(vec![fork("Markdown", "Plain text")]), None);
        assert_eq!(question.category, APPROVAL_CATEGORY);
        assert_eq!(
            labels(&question),
            [APPROVE, "Fork 1: Plain text instead of Markdown"]
        );
        // It recommends approval, option A (OWL-324).
        assert_eq!(question.recommended_option(), Some(0));
        let none = approval_question(&plan(Vec::new()), None);
        assert_eq!(labels(&none), [APPROVE]);
        // Alternatives past 8 KiB are cut, then left to the PLAN comment.
        let long = "x".repeat(3_000);
        let other = "y".repeat(3_000);
        let wide: Vec<Fork> = (0..4).map(|_| fork(&long, &other)).collect();
        let cut = approval_question(&plan(wide), None);
        assert!(weight(&cut) <= MAX_QUESTION_BYTES, "{}", weight(&cut));
        assert_eq!(cut.options.len(), 5);
        let many: Vec<Fork> = (0..400).map(|n| fork(&format!("a{n}"), &long)).collect();
        let folded = approval_question(&plan(many), None);
        assert!(weight(&folded) <= MAX_QUESTION_BYTES, "{}", weight(&folded));
    }

    fn ticket_id() -> TicketId {
        TicketId::new("OWL-160").unwrap()
    }

    /// A Linear document link of the length OWL-350's live check saw.
    const URL: &str = "https://linear.app/owlshift/document/plan-owl-160-3f2a9c1b7d4e";

    fn link(url: &str) -> PlanLink<'_> {
        static TICKET: std::sync::OnceLock<TicketId> = std::sync::OnceLock::new();
        PlanLink {
            ticket: TICKET.get_or_init(ticket_id),
            url: Some(url),
        }
    }

    /// A QUESTIONS comment of the runner's, as `ask` posts it, and its
    /// characters with a mention of `mention` characters on its header.
    fn round(
        summary: &str,
        questions: Vec<Question>,
        notes: Vec<QuestionNote>,
        decided: usize,
        fallback: Option<crate::resolver::Fallback>,
        mention: usize,
    ) -> (String, usize) {
        let body = crate::writer::QuestionsComment {
            ticket: ticket_id(),
            round: NonZeroU32::new(12).unwrap(),
            summary: summary.to_owned(),
            questions,
            premise_false: false,
            decided,
            fallback,
            reopened: Vec::new(),
            notes,
        }
        .render();
        let total = chars(&body) + if mention > 0 { 3 + mention } else { 0 };
        (body, total)
    }

    /// OWL-160's approval round, replayed (OWL-352): 3 forks of 26 options
    /// in all, which the round before showed as 26 options, reads as 2
    /// options and 3 fork lines, each linking to the plan's document, in
    /// under 1,500 characters.
    #[test]
    fn the_owl_160_approval_round_fits_at_a_glance() {
        let fork = |n: usize, options: usize| {
            let options: Vec<String> = (0..options)
                .map(|i| {
                    format!(
                        "Option {i} of fork {n}: {}",
                        "a fairly long alternative ".repeat(3)
                    )
                })
                .collect();
            Fork {
                choice: format!(
                    "{} (fork {n})",
                    "Keep the whole plan in the ticket ref and ".repeat(2)
                ),
                options,
                reason: "R".to_owned(),
            }
        };
        let plan = plan(vec![fork(1, 12), fork(2, 9), fork(3, 5)]);
        let question = approval_question(&plan, Some(link(URL)));
        let labels: Vec<&str> = question.options.iter().map(|o| o.label.as_str()).collect();
        assert_eq!(labels, [APPROVE, "Change it"]);
        assert_eq!(question.recommended_option(), Some(0));
        assert!(question.context.contains(URL), "{}", question.context);
        let note = approval_note(&question, &plan, link(URL)).unwrap();
        let summary = "The plan passed design review pass 5 and waits for your approval: the \
                       PLAN comment above shows it.";
        let (body, total) = round(summary, vec![question], vec![note], 0, None, 45);
        let forks: Vec<&str> = body.lines().filter(|l| l.starts_with("- Fork ")).collect();
        assert_eq!(forks.len(), 3, "{body}");
        assert!(forks.iter().all(|line| line.contains(URL)), "{body}");
        assert!(!body.contains(" instead of "), "{body}");
        assert!(total < 1_500, "{total}\n{body}");
        // A plan of many forks gives one line linking the document, within
        // the budget.
        let many = Plan {
            forks: (1..=40).map(|n| fork(n, 3)).collect(),
            ..plan
        };
        let question = approval_question(&many, Some(link(URL)));
        let note = approval_note(&question, &many, link(URL)).unwrap();
        assert_eq!(note.lines.len(), 1, "{:?}", note.lines);
        let (body, total) = round(summary, vec![question], vec![note], 0, None, 45);
        assert!(total <= ROUND_BUDGET_CHARS, "{total}\n{body}");
        // No fork, no note.
        assert!(
            approval_note(&question_without_forks(), &plan_without_forks(), link(URL)).is_none()
        );
    }

    /// OWL-352: the approval round on a tracker with documents, exactly.
    #[test]
    fn the_approval_round_renders_as_the_target() {
        let plan = plan(vec![Fork {
            choice: "Markdown.".to_owned(),
            options: vec!["Markdown.".to_owned(), "Plain text.".to_owned()],
            reason: "R".to_owned(),
        }]);
        let question = approval_question(&plan, Some(link(URL)));
        let note = approval_note(&question, &plan, link(URL)).unwrap();
        let (body, _) = round("S.", vec![question], vec![note], 0, None, 0);
        let target = format!(
            "\
[owlshift] QUESTIONS · round 12

S.

### Q1 · Do you approve the plan as it is, or change it?
The plan passed its design review. Its forks are in [the plan's document]({URL}), each fork's \
             options lettered A, B, C… in their order.

- **A.** ⭐ Approve the plan as it is
- **B.** Change it — say what in your own words, such as `B, fork 1 option C`

**Recommendation: A.** The plan passed its design review.

**The plan's forks**, each with its choice:
- Fork 1: Markdown. · [its options]({URL})

---
Reply `Q1: A`, `ok` to take every recommendation, or in your own words.

<!-- owlshift:{{\"format\":1,\"kind\":\"QUESTIONS\",\"ticket\":\"OWL-160\",\"round\":12}} -->
"
        );
        assert_eq!(body, target);
    }

    fn plan_without_forks() -> Plan {
        plan(Vec::new())
    }

    fn question_without_forks() -> Question {
        approval_question(&plan_without_forks(), Some(link(URL)))
    }

    /// [`ROUND_FRAME_CHARS`] holds the longest frame a round of the
    /// runner's shows: the longest summary, mention, resolver lines and
    /// round number.
    #[test]
    fn the_round_frame_reserve_holds_the_longest_frame() {
        use crate::resolver::Fallback;
        let question = || Question {
            id: QuestionId::nth(1),
            category: APPROVAL_CATEGORY.to_owned(),
            context: String::new(),
            text: String::new(),
            options: vec![QuestionOption::new("x")],
            recommendation: None,
            replan: false,
        };
        let block = chars(&question_block(&question()));
        for fallback in [
            Fallback::UsageLimit,
            Fallback::Failed,
            Fallback::PassLimit(u32::MAX),
            Fallback::Budget {
                spent_micros: u64::MAX,
                ceiling_micros: u64::MAX,
            },
            Fallback::CostUnreported,
        ] {
            let (body, total) = round(
                &"s".repeat(2_000),
                vec![question()],
                Vec::new(),
                usize::MAX,
                Some(fallback),
                MAX_POINTER_URL_CHARS,
            );
            assert!(
                total - block <= ROUND_FRAME_CHARS,
                "{}\n{body}",
                total - block
            );
        }
    }

    #[test]
    fn a_plans_hash_is_the_sha256_of_its_canonical_text() {
        let plan = plan(Vec::new());
        let expected: String = Sha256::digest(plan.canonical().as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        assert_eq!(plan_sha256(&plan), expected);
        assert_eq!(plan_sha256(&plan).len(), 64);
    }
}
