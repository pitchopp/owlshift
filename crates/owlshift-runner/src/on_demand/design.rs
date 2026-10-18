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
//!   asked a revision, the approval round after a pass that asked for it,
//!   the answers in its thread after a residue round.
//! - *Design review.* The roster of the variant ([`roster::roster`]) runs one
//!   seat after the other on Claude Code, on another model than the
//!   author's when one is configured (OWL-347); the findings are
//!   numbered across the seats ([`pass_findings`]), confirmed by one arbiter
//!   run or left to the author's answers (`pipeline.findings`), and the pass
//!   is settled by the core ([`settle_pass`]). A pass that asks a revision
//!   loops back to Design; one that ends posts the PLAN comment, then goes to
//!   Build when the plan gate is open, or puts the approval
//!   ([`approval_question`]) or the findings left ([`residue_questions`]) to
//!   the resolver first (OWL-342), the approval the decider's alone under
//!   `always`: an approval it decides, option A, takes the ticket to Build;
//!   any other round it decides whole goes back to Design with the
//!   decisions; what it leaves goes to the decider.
//!
//! A refused design, review or arbiter result is a failed run, as intake's
//! is: their briefs carry no refused result to fix. A failed review seat or
//! arbiter run is run again, the pass's other findings kept, until the
//! stage's failed-run bound parks the ticket. A failed PLAN post stops the
//! command before the pass is kept, so `continue` runs the pass again.

use std::num::NonZeroU32;

use sha2::{Digest, Sha256};

use owlshift_contracts::brief::{GivenDiff, GivenFindings, GivenPlan, ReviewBrief};
use owlshift_contracts::config::FindingsConfirmation;
use owlshift_contracts::ids::FindingId;
use owlshift_contracts::result::{
    ApprovalOutcome, MAX_QUESTION_BYTES, MAX_QUESTIONS, MAX_QUESTIONS_BYTES, QuestionOption,
    Recommendation,
};
use owlshift_contracts::review::{
    Finding, FindingOutcome, FindingsMode, KeptApproval, KeptFinding, KeptResponse, KeptRuling,
    Plan, ResponseOutcome, ReviewPass, Reviewer, RulingOutcome, Severity, TicketPlan, TicketReview,
};
use owlshift_core::review::{Fate, PassOutcome, PassVerdict, Standing, settle};
use owlshift_core::roster;
use owlshift_core::routing::reviewer_routing;
use owlshift_core::vocab::Harness as HarnessKind;

use super::*;
use crate::ticket_ref::text_sha256;
use crate::writer::{PlanComment, PlanNext, PlanReviewer};

/// The category of the plan-approval question (architecture section 5,
/// "Plan approval"): through the resolver under `on-fork`, the decider's
/// alone under `always` (OWL-342).
pub(crate) const APPROVAL_CATEGORY: &str = "plan_approval";

/// The category of a residue question: a finding two review passes left.
pub(crate) const FINDING_CATEGORY: &str = "review_finding";

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
    /// The ticket returned to its author stage, in this state.
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
    /// The residue round's questions when the pass asks, each with its
    /// finding.
    pub questions: Vec<(Question, FindingId)>,
    /// Findings the round could not ask, past the questions it holds.
    pub not_asked: Vec<FindingId>,
    /// The pass's last run, its arbiter's when one ran.
    pub last_run: Option<String>,
    /// What the pass's runs reported they cost, summed, for the chain its
    /// round counts (OWL-342): `None` when one did not report it.
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

/// The plan a Build follows (architecture section 5, "Kept and shown"): the
/// kept plan with its base, in a pipeline with Design; none on `trivial`,
/// which has no plan.
pub(super) fn given_plan(p: &Prepared) -> Option<GivenPlan> {
    kept_plan(p).filter(|_| p.pipeline.contains(Stage::Design))
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
            FindingsConfirmation::Author => kept
                .response
                .as_ref()
                .is_none_or(|response| response.outcome == ResponseOutcome::Fixed),
        })
        .map(|(n, kept)| {
            (
                n,
                Standing {
                    severity: kept.finding.severity,
                    on_criterion: kept.finding.criterion.is_some(),
                },
            )
        })
        .collect()
}

/// Settles review pass number `pass` at `stage` over its `findings`
/// (OWL-301's [`settle`]) and records each finding's outcome: `rejected` for
/// one the arbiter rejected, `asked` for one the decider gets, `proposed`
/// for one Verify files as a follow-up (OWL-304); the others keep none
/// until a run fixes them, a noted `minor` never.
pub(crate) fn settle_pass(
    stage: Stage,
    pass: u32,
    findings: &mut [KeptFinding],
    confirmation: FindingsConfirmation,
) -> Option<PassVerdict> {
    let standing = standing(findings, confirmation);
    let core: Vec<Standing> = standing.iter().map(|(_, standing)| *standing).collect();
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
    for ((n, _), fate) in standing.iter().zip(&verdict.fates) {
        match fate {
            Fate::Ask => findings[*n].outcome = Some(FindingOutcome::Asked),
            Fate::Propose => findings[*n].outcome = Some(FindingOutcome::Proposed),
            Fate::Fix | Fate::Note => {}
        }
    }
    Some(verdict)
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
/// runs again ([`replaced_pass`]), else appended.
pub(crate) fn push_pass(review: &mut TicketReview, kept: ReviewPass) {
    if replaced_pass(review, kept.stage, kept.pass.get()).is_some() {
        review.passes.pop();
    }
    review.passes.push(kept);
}

/// Records what the run given `given`, the latest pass's findings at
/// `stage`, did with them, on that pass: the author's response to each in
/// answer mode, every one fixed in fix mode (OWL-303, OWL-304).
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
            FindingsMode::Answer => {
                if let Some(response) = result
                    .finding_responses
                    .iter()
                    .find(|response| response.finding == kept.finding.id)
                {
                    kept.response = Some(KeptResponse {
                        outcome: response.outcome,
                        reason: response.reason.clone(),
                    });
                    kept.outcome = Some(match response.outcome {
                        ResponseOutcome::Fixed => FindingOutcome::Fixed,
                        ResponseOutcome::Rejected => FindingOutcome::Rejected,
                    });
                }
            }
            FindingsMode::Fix | FindingsMode::Rule => {
                kept.outcome = Some(FindingOutcome::Fixed);
            }
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

/// The findings the loop's Design review passes before `pass` rejected,
/// oldest pass first, each with its pass: the passes that end `review`
/// before its last one, `pass` itself, back to the loop's pass 1.
pub(crate) fn earlier_rejected(review: &TicketReview, pass: u32) -> Vec<(u32, KeptFinding)> {
    let mut passes: Vec<&ReviewPass> = Vec::new();
    let mut expected = pass;
    for kept in review.passes.iter().rev() {
        if expected == 0 || kept.stage != Stage::DesignReview || kept.pass.get() != expected {
            break;
        }
        if expected < pass {
            passes.push(kept);
        }
        expected -= 1;
    }
    passes
        .into_iter()
        .rev()
        .flat_map(|kept| {
            kept.findings
                .iter()
                .filter(|finding| finding.outcome == Some(FindingOutcome::Rejected))
                .map(|finding| (kept.pass.get(), finding.clone()))
        })
        .collect()
}

/// What the design run due at Design in `state` is given (assumption A4 of
/// OWL-303): nothing before a review pass ended in this loop; else, by the
/// latest Design review pass of `review`, settled again under
/// `confirmation`: a pass that asked a revision gives its findings, the
/// standing ones to fix when the arbiter confirms them, every one to answer
/// when the author does; a pass that passed was followed by the approval
/// round, given while `approval_required` still holds; a pass that asked the
/// decider gives nothing, the answers being in the thread. A review record
/// that does not end with the state's latest pass is an error.
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
        PassOutcome::Revise => {
            let (mode, given): (FindingsMode, Vec<Finding>) = match confirmation {
                FindingsConfirmation::Arbiter => (
                    FindingsMode::Fix,
                    standing(&last.findings, confirmation)
                        .iter()
                        .map(|(n, _)| last.findings[*n].finding.clone())
                        .collect(),
                ),
                FindingsConfirmation::Author => (
                    FindingsMode::Answer,
                    last.findings
                        .iter()
                        .map(|kept| kept.finding.clone())
                        .collect(),
                ),
            };
            DesignInput {
                findings: Some(GivenFindings {
                    mode,
                    findings: given,
                    diff: None,
                }),
                approval: false,
            }
        }
        PassOutcome::Passed => DesignInput {
            findings: None,
            approval: approval_required,
        },
        PassOutcome::Ask => DesignInput::default(),
    })
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

/// The approval question (architecture section 5, "Plan approval", How): one
/// question, whether the decider approves the plan of the PLAN comment,
/// with its forks' other options as the options to change, cut so the
/// question holds in [`MAX_QUESTION_BYTES`]; past that, one option per fork,
/// its options left to the PLAN comment. It recommends approval, option A
/// (OWL-324): the plan passed its review.
pub(crate) fn approval_question(plan: &Plan) -> Question {
    let budget = MAX_QUESTION_BYTES - 256;
    let question = |options: Vec<String>| Question {
        id: QuestionId::nth(1),
        category: APPROVAL_CATEGORY.to_owned(),
        context: "The plan of this ticket passed its design review. The PLAN comment above holds \
                  it whole, with its forks and the findings its reviewers left."
            .to_owned(),
        text: "Do you approve the plan of the PLAN comment as it is, or change it, such as \
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
        (1..=plan.forks.len())
            .map(|n| format!("Change fork {n}: its options are in the PLAN comment")),
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

/// The residue round's questions (architecture section 5, "Design review",
/// The bound, and "Verify", The residue): one per finding `findings` of
/// review pass `pass` at `stage` asks, blockers first, then by id, at most
/// [`MAX_QUESTIONS`], each cut so the round holds in [`MAX_QUESTION_BYTES`]
/// each and [`MAX_QUESTIONS_BYTES`] together. A Design review finding asks
/// how the plan settles it, a Verify one how the change does (OWL-304);
/// each recommends settling it, option A (OWL-324).
/// Returns the questions, each with the finding it asks, and the findings
/// past the cap, left for the pass after the round.
pub(crate) fn residue_questions(
    findings: &[KeptFinding],
    stage: Stage,
    pass: u32,
) -> (Vec<(Question, FindingId)>, Vec<FindingId>) {
    let (pass_name, work) = if stage == Stage::Verify {
        ("verify", "change")
    } else {
        ("design review", "plan")
    };
    let mut asked: Vec<&KeptFinding> = findings
        .iter()
        .filter(|kept| kept.outcome == Some(FindingOutcome::Asked))
        .collect();
    asked.sort_by_key(|kept| {
        (
            kept.finding.severity != Severity::Blocker,
            kept.finding.id.number(),
        )
    });
    let over: Vec<FindingId> = asked
        .iter()
        .skip(MAX_QUESTIONS)
        .map(|kept| kept.finding.id.clone())
        .collect();
    asked.truncate(MAX_QUESTIONS);
    let budget = (MAX_QUESTIONS_BYTES / asked.len().max(1)).min(MAX_QUESTION_BYTES) - 256;
    let questions = asked
        .iter()
        .enumerate()
        .map(|(n, kept)| {
            let finding = &kept.finding;
            let make = |cap: usize| {
                let judged = match (&kept.ruling, &kept.response) {
                    (Some(ruling), _) => {
                        format!("The arbiter confirmed it: {}", clip(&ruling.reason, cap))
                    }
                    _ => "No one judged it: its author answers findings after a pass, and none \
                          follows this one."
                        .to_owned(),
                };
                let criterion = finding
                    .criterion
                    .as_deref()
                    .map(|criterion| {
                        format!(" It bears on the criterion: {}.", clip(criterion, cap))
                    })
                    .unwrap_or_default();
                Question {
                    id: QuestionId::nth(u32::try_from(n + 1).unwrap_or(u32::MAX)),
                    category: FINDING_CATEGORY.to_owned(),
                    context: format!(
                        "Finding {} of {pass_name} pass {pass}, {}, raised by the {} reviewer \
                         on {}, still stands after the passes the design allows. Evidence: \
                         {}. {judged}{criterion}",
                        finding.id,
                        severity_name(finding.severity),
                        mandate_names(&kept.reviewer.mandates),
                        harness_name(kept.reviewer.harness),
                        clip(&finding.evidence, cap),
                    ),
                    text: format!(
                        "{} How should the {work} settle it?",
                        clip(&finding.claim, cap)
                    ),
                    options: vec![
                        QuestionOption::new(format!("Settle it: {}", clip(&finding.settle, cap))),
                        QuestionOption::new(format!("Leave the {work} as it is")),
                    ],
                    recommendation: Some(Recommendation {
                        option: Some("A".to_owned()),
                        why: "The finding still stands after the passes the design allows."
                            .to_owned(),
                    }),
                    replan: false,
                }
            };
            (fitting(budget, make), finding.id.clone())
        })
        .collect();
    (questions, over)
}

fn severity_name(severity: Severity) -> &'static str {
    match severity {
        Severity::Blocker => "a blocker",
        Severity::Major => "major",
        Severity::Minor => "minor",
    }
}

fn mandate_names(mandates: &[owlshift_contracts::review::Mandate]) -> String {
    use owlshift_contracts::review::Mandate;
    mandates
        .iter()
        .map(|mandate| match mandate {
            Mandate::Critic => "critic",
            Mandate::Conformance => "conformance",
            Mandate::Debt => "debt",
        })
        .collect::<Vec<_>>()
        .join(" and ")
}

fn harness_name(harness: HarnessKind) -> &'static str {
    match harness {
        HarnessKind::Claude => "Claude Code",
        HarnessKind::Codex => "Codex",
    }
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
        PassOutcome::Ask => "ask",
    }
}

fn finding_ids<'k>(findings: impl IntoIterator<Item = &'k KeptFinding>) -> Value {
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
            // The decider's replies to the resolver's approval and residue
            // decisions come first (OWL-345): the ticket returns to its
            // author stage, or runs on with the reply as `reversed`.
            if let Reopened::Moved(next) = self.reopened(p, state, sink)? {
                state = next;
            }
            match state.status() {
                Status::Active(Stage::Design) | Status::Active(Stage::DesignReview) => {
                    let next = match state.stage() {
                        Stage::Design => self.design_run(p, &dispatched, state, sink)?,
                        _ => self.review_pass(p, &dispatched, state, sink)?,
                    };
                    if next.status() == Status::Active(Stage::Build) {
                        dispatched = self.dispatch_to_build(p, command, sink)?;
                    }
                    state = next;
                }
                Status::Active(Stage::Build) => match self.build(p, &dispatched, state, sink)? {
                    Built::Delivered(delivered) => return Ok(delivered),
                    Built::Replan(next) => state = next,
                },
                // A Verify pass `continue` runs again on the kept Build
                // (OWL-304), then the delivery or a Verify fix run.
                Status::Active(Stage::Verify) => {
                    match self.verify_pass(p, &dispatched, state, sink)? {
                        Verified::Passed(verified) => {
                            return self.deliver_verified(p, &dispatched, &verified, sink);
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
    fn dispatch_to_build(
        &self,
        p: &mut Prepared,
        command: Command,
        sink: &mut EventSink<'_>,
    ) -> Result<Dispatched, Stop> {
        p.base = project::sync_checkout(&self.executor.git, self.dirs, self.remote_url)
            .map_err(Stop::Refused)?;
        self.executor
            .start_from_base(&p.checkout, &p.worktree, &p.branch, &p.base.commit)
            .map_err(|e| refused("starting the ticket's branch from the base", e))?;
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

    /// Applies the decider's replies to the resolver's plan approval and
    /// residue decisions before a stage runs (OWL-345; architecture section
    /// 5, "A reversed decision"): a reply to the decision kept as the plan's
    /// approval drops the approval and returns the ticket to Design, one to
    /// a Design review residue decision returns it to Design, one to a
    /// Verify residue decision to Build. The reversals are marked followed
    /// in the same write as the move, so the run that follows is given each
    /// as `reversed`, with the decider's reply in its thread.
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
                .any(|kept| reversal::reopens(kept, p.plan.as_ref()).is_some())
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
                .is_some_and(|kept| reversal::reopens(kept, p.plan.as_ref()).is_some())
        });
        let Some(reopening) = reversal::reopening(stage, &reversed, &questions, p.plan.as_ref())
        else {
            return Ok(Reopened::Nothing);
        };
        reversed.retain(|r| reopening.served.contains(&r.decision));
        let window = self.config.policy.quiet_window();
        if reversal::counts_at(&reversed, (self.clock)(), window).is_some() {
            return Ok(Reopened::Nothing);
        }
        let next = match reopening.event {
            None => state,
            // A pipeline without the destination does not return.
            Some(event) => match state.apply(p.pipeline, event) {
                Ok(Transition::To(next)) => next,
                _ => return Ok(Reopened::Nothing),
            },
        };
        let mut questions = questions;
        reversal::followed(&mut questions, &reversed);
        let kept_plan = p.plan.clone();
        if reopening.drops_approval
            && let Some(plan) = p.plan.as_mut()
        {
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
                ("to", json!(reopening.to)),
                ("approval_dropped", json!(reopening.drops_approval)),
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
        Ok(if reopening.event.is_some() {
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
    fn review_run(
        &self,
        p: &mut Prepared,
        harness: &dyn Harness,
        state: &mut TicketState,
        brief: &Brief,
        sink: &mut EventSink<'_>,
    ) -> Result<(Ran, RunResult), Stop> {
        let mut attempt = 0u32;
        loop {
            attempt += 1;
            let (ran, event) = self.run_role_once(
                p,
                harness,
                state,
                brief,
                attempt,
                &mut Gathered::default(),
                sink,
            )?;
            let event = match (event, ran.result()) {
                (Event::Completed, Some(result)) => {
                    let result = result.clone();
                    return Ok((ran, result));
                }
                (Event::Interrupted | Event::Quarantined | Event::Blocked, _) => event,
                _ => Event::RunFailed,
            };
            match state.apply(p.pipeline, event) {
                Ok(Transition::To(next)) => {
                    *state = next;
                    self.keep(p, state)?;
                    if let Outcome::UsageLimit { resets_at } = ran.report.outcome {
                        return Err(Stop::UsageLimit { resets_at });
                    }
                }
                Ok(Transition::Parked {
                    state: parked,
                    reason,
                }) => {
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

    /// Runs the design role at Design until its plan is kept, an approval is
    /// recorded, or it stops: a failed run gets one more before the core
    /// parks the ticket; questions go through the resolver, all decided,
    /// it runs again, else a round opens.
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
            let (ran, event) = self.run_role_once(
                p,
                &*harness,
                &state,
                &brief,
                attempt,
                &mut Gathered::default(),
                sink,
            )?;
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
                    Routing::Settled(_) => continue,
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
                return self.designed(p, state, &brief, &result, &comments, &ran, sink);
            }
            match state.apply(p.pipeline, event) {
                Ok(Transition::To(next)) => {
                    state = next;
                    self.keep(p, &state)?;
                    if let Outcome::UsageLimit { resets_at } = ran.report.outcome {
                        return Err(Stop::UsageLimit { resets_at });
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
    /// unapproved, the findings its brief gave marked fixed or answered,
    /// which takes it to Design review. One write keeps it with the state.
    #[allow(clippy::too_many_arguments)]
    fn designed(
        &self,
        p: &mut Prepared,
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
            // its DECISION comment; else the decider's newest comment.
            let decided = questions
                .decisions
                .iter()
                .rev()
                .find(|kept| kept.question.category == APPROVAL_CATEGORY)
                .filter(|kept| questions.latest().is_none_or(|ask| kept.at > ask.at));
            let comment = match decided {
                Some(decision) => decision.comment.clone(),
                None => {
                    let ask = questions.latest().ok_or_else(|| {
                        Stop::Refused("an approval round the ref keeps no ask of".to_owned())
                    })?;
                    let approver = person(&ask.decider, &p.found);
                    answer_check::newest_decider_comment(comments, &approver)
                        .ok_or_else(|| {
                            Stop::Refused(format!(
                                "the plan of {ticket} is approved, but no comment of its decider \
                                 holds the approval"
                            ))
                        })?
                        .id
                        .clone()
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
        // What the run did with the findings it was given, on the pass that
        // raised them.
        if let (Some(given), Some(review)) = (&brief.findings, p.review.as_mut()) {
            mark_answered(review, Stage::DesignReview, given, result);
        }
        let base = self.branch_head(p).unwrap_or_else(|| p.base.commit.clone());
        let kept = TicketPlan {
            format: Format,
            plan,
            text_sha256: text_sha256(&p.found.title, &p.found.description),
            base: base.clone(),
            // A plan a design run writes starts unapproved (OWL-32).
            approval: None,
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
            ]),
        );
        Ok(next)
    }

    /// One review pass at `stage`, Design review or Verify, shared by both
    /// (OWL-303, OWL-304): each seat of the roster on Claude Code, on the
    /// reviewers' routing, another model than the author's when one is
    /// configured (OWL-347); the arbiter when the pass has findings the project confirms so; the pass settled by the
    /// core; its `ruling`, `pass` and `residue` events, `next` naming what
    /// each outcome leads to; and the review record with the pass, checked,
    /// not kept yet. The seats read `plan` and, at Verify, `diff`.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn run_review_pass(
        &self,
        p: &mut Prepared,
        dispatched: &Dispatched,
        state: &mut TicketState,
        stage: Stage,
        plan: Option<GivenPlan>,
        diff: Option<GivenDiff>,
        next: &dyn Fn(PassOutcome) -> &'static str,
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
        let pass = state.review_pass_due();
        let pass_id = NonZeroU32::new(pass).expect("a pass due counts from 1");
        let models = &self.config.models;
        let routed = reviewer_routing(dispatched.routing, |tier| claude_model(models, tier));
        let model = claude_model(models, routed.routing.tier);
        let seats = roster::roster(p.pipeline.variant(), stage);
        let mut outputs: Vec<SeatOutput> = Vec::new();
        let mut last_run = None;
        let mut cost = Some(0.0_f64);
        for seat in seats {
            let harness = router.harness(routed.routing);
            let comments = self.comments(&ticket)?;
            let mut brief = self.brief(p, role, &comments, &dispatched.rules, None, &current);
            brief.plan = plan.clone();
            brief.review = Some(ReviewBrief {
                mandates: seat.mandates.to_vec(),
                pass: pass_id,
                diff: diff.clone(),
            });
            let (ran, result) = self.review_run(p, &*harness, state, &brief, sink)?;
            last_run = Some(ran.run.clone());
            cost = cost.zip(run_cost(&ran)).map(|(sum, run)| sum + run);
            outputs.push(SeatOutput {
                reviewer: Reviewer {
                    role,
                    mandates: seat.mandates.to_vec(),
                    harness: routed.routing.harness,
                    model: model.unwrap_or("default").to_owned(),
                    same_model: routed.same_model,
                },
                model: model.map(str::to_owned),
                same_model: routed.same_model,
                findings: result.findings,
            });
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
        let verdict =
            settle_pass(stage, pass, &mut findings, confirmation).expect("a review stage settles");
        // The residue round asks what it can; the rest waits for the pass
        // after the round (OWL-303).
        let (questions, not_asked) = if verdict.outcome == PassOutcome::Ask {
            residue_questions(&findings, stage, pass)
        } else {
            (Vec::new(), Vec::new())
        };
        for kept in &mut findings {
            if not_asked.contains(&kept.finding.id) {
                kept.outcome = None;
            }
        }
        let reviewers_data: Vec<Value> = outputs
            .iter()
            .map(|output| {
                json!({
                    "mandates": output.reviewer.mandates,
                    "harness": harness_key(output.reviewer.harness),
                    "tier": routed.routing.tier,
                    "effort": routed.routing.effort,
                    "model": output.model,
                    "same_model": output.same_model,
                    "findings": output.findings.len(),
                })
            })
            .collect();
        sink.emit(
            &ticket,
            last_run.as_deref(),
            EventKind::Decision,
            data([
                ("review", json!("pass")),
                ("stage", json!(stage_key)),
                ("pass", json!(pass)),
                ("outcome", json!(outcome_name(verdict.outcome))),
                ("next", json!(next(verdict.outcome))),
                ("findings", json!(findings.len())),
                ("standing", json!(standing(&findings, confirmation).len())),
                ("reviewers", json!(reviewers_data)),
            ]),
        );
        if verdict.outcome != PassOutcome::Revise {
            // Design review notes a `minor` for the Build; Verify proposes
            // what it does not ask as a follow-up (OWL-304).
            let (label, fate) = if stage == Stage::Verify {
                ("proposed", Fate::Propose)
            } else {
                ("noted", Fate::Note)
            };
            let left: Vec<&KeptFinding> = standing(&findings, confirmation)
                .iter()
                .zip(&verdict.fates)
                .filter(|(_, f)| **f == fate)
                .map(|((n, _), _)| &findings[*n])
                .collect();
            if verdict.outcome == PassOutcome::Ask || !left.is_empty() {
                sink.emit(
                    &ticket,
                    last_run.as_deref(),
                    EventKind::Decision,
                    data([
                        ("review", json!("residue")),
                        ("stage", json!(stage_key)),
                        ("pass", json!(pass)),
                        (
                            "asked",
                            finding_ids(
                                findings
                                    .iter()
                                    .filter(|kept| kept.outcome == Some(FindingOutcome::Asked)),
                            ),
                        ),
                        (label, finding_ids(left)),
                        (
                            "not_asked",
                            json!(not_asked.iter().map(FindingId::as_str).collect::<Vec<_>>()),
                        ),
                    ]),
                );
            }
        }
        let mut review = p.review.clone().unwrap_or_else(|| TicketReview {
            format: Format,
            passes: Vec::new(),
            answered_reviews: Vec::new(),
            answering: Vec::new(),
            build: None,
        });
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
            questions,
            not_asked,
            last_run,
            cost,
            review,
        })
    }

    /// One Design review pass on the kept plan ([`Self::run_review_pass`]),
    /// then a loop back to Design, or the PLAN comment and Build or a round
    /// for the decider.
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
        let given = GivenPlan {
            base: kept.base.clone(),
            plan: kept.plan.clone(),
        };
        let gate = self.plan_gate(p, &kept);
        let next = |outcome: PassOutcome| match outcome.design_review_event(gate) {
            Event::LoopBack => "design",
            Event::Completed => "build",
            _ if outcome == PassOutcome::Passed => "approval",
            _ => "questions",
        };
        let PassRun {
            pass,
            outputs,
            findings,
            verdict,
            questions,
            not_asked,
            last_run,
            cost,
            review,
        } = self.run_review_pass(
            p,
            dispatched,
            &mut state,
            Stage::DesignReview,
            Some(given),
            None,
            &next,
            sink,
        )?;
        let event = verdict.outcome.design_review_event(gate);
        if event == Event::LoopBack {
            let next = match state.apply(p.pipeline, Event::LoopBack) {
                Ok(Transition::To(next)) => next,
                other => return Err(core_error(other)),
            };
            p.review = Some(review);
            let questions = p.questions();
            self.store(p, &next, questions)
                .map_err(|e| refused("keeping the review pass in the ticket's ref", e))?;
            return Ok(next);
        }
        // The plan leaves its review: the PLAN comment first, so nothing of
        // the pass is kept when it cannot be posted.
        let comment = PlanComment {
            ticket: ticket.clone(),
            plan: kept.plan.clone(),
            pass,
            reviewers: outputs
                .iter()
                .map(|output| PlanReviewer {
                    mandates: output.reviewer.mandates.clone(),
                    harness: output.reviewer.harness,
                    model: output.model.clone(),
                    same_model: output.same_model,
                })
                .collect(),
            findings: findings.clone(),
            not_asked: not_asked.clone(),
            earlier: earlier_rejected(&review, pass),
            next: match (event, verdict.outcome) {
                (Event::Completed, _) => PlanNext::Build,
                (_, PassOutcome::Passed) => PlanNext::Approval,
                _ => PlanNext::Findings,
            },
            run: last_run.clone(),
        };
        let posted = Writer::new(self.tracker)
            .post_plan(&comment)
            .map_err(|e| refused("posting the PLAN comment on the ticket", e))?;
        sink.emit(
            &ticket,
            last_run.as_deref(),
            EventKind::TrackerWrite,
            comment_written("PLAN", &posted),
        );
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
        let approving = verdict.outcome == PassOutcome::Passed;
        let (summary, asked, asked_findings): (
            String,
            Vec<Question>,
            Vec<(QuestionId, FindingId)>,
        ) = if approving {
            (
                format!(
                    "The plan passed design review pass {pass} and waits for your approval: \
                         the PLAN comment above holds it."
                ),
                vec![approval_question(&kept.plan)],
                Vec::new(),
            )
        } else {
            let (asked, findings) = questions
                .into_iter()
                .map(|(question, finding)| {
                    let id = question.id.clone();
                    (question, (id, finding))
                })
                .unzip();
            (
                format!(
                    "Design review pass {pass} left findings that the plan's revisions did \
                         not settle: each is a question below, and the PLAN comment above lists \
                         them with the plan."
                ),
                asked,
                findings,
            )
        };
        // The approval is the decider's alone under `always` (OWL-342).
        let direct: Vec<QuestionId> = if approving && p.policy.approval_is_human(p.pipeline) {
            vec![QuestionId::nth(1)]
        } else {
            Vec::new()
        };
        let run = last_run
            .clone()
            .ok_or_else(|| Stop::Refused("a design review pass that ran no run".to_owned()))?;
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
            findings: &asked_findings,
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
                let approval = decided
                    .iter()
                    .find(|d| approving && d.option.as_deref() == Some("A"));
                if let Some(approval) = approval {
                    return self.approved_by_resolver(
                        p,
                        state,
                        kept,
                        &approval.comment,
                        &run,
                        sink,
                    );
                }
                // A plan changed, or findings settled: the next design run
                // follows the decisions, as it follows the answers to a
                // round.
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

    /// The plan the resolver approved (OWL-342, option A of the approval
    /// question): kept in `plan.json` with its DECISION comment and the
    /// plan's hash, where the decider's approval keeps their comment, then
    /// the plan gate, which holds the hash as for any approval (OWL-32),
    /// takes the ticket to Build. One write keeps it with the state.
    fn approved_by_resolver(
        &self,
        p: &mut Prepared,
        state: TicketState,
        mut kept: TicketPlan,
        comment: &str,
        run: &str,
        sink: &mut EventSink<'_>,
    ) -> Result<TicketState, Stop> {
        let sha = plan_sha256(&kept.plan);
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
                ("by", json!("resolver")),
                ("comment", json!(comment)),
                ("plan_sha256", json!(sha)),
            ]),
        );
        Ok(next)
    }
}

#[cfg(test)]
mod tests {
    use owlshift_contracts::review::{Alternative, Fork, Mandate, PlanStep, Ruling};
    use owlshift_core::state::{Status, TicketState};

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
        // Before the last pass: the confirmed ones are fixed, a revision.
        let mut first = pass_findings(&seats, Some(&rulings));
        let verdict = settle_pass(
            Stage::DesignReview,
            1,
            &mut first,
            FindingsConfirmation::Arbiter,
        )
        .unwrap();
        assert_eq!(verdict.outcome, PassOutcome::Revise);
        assert_eq!(verdict.fates, [Fate::Fix, Fate::Fix]);
        let outcomes: Vec<_> = first.iter().map(|k| k.outcome).collect();
        assert_eq!(outcomes, [None, Some(FindingOutcome::Rejected), None]);
        // The last pass: the major is asked, the minor noted.
        let mut last = pass_findings(&seats, Some(&rulings));
        let verdict = settle_pass(
            Stage::DesignReview,
            2,
            &mut last,
            FindingsConfirmation::Arbiter,
        )
        .unwrap();
        assert_eq!(verdict.outcome, PassOutcome::Ask);
        let outcomes: Vec<_> = last.iter().map(|k| k.outcome).collect();
        assert_eq!(
            outcomes,
            [
                Some(FindingOutcome::Asked),
                Some(FindingOutcome::Rejected),
                None
            ]
        );
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
                })
                .collect(),
            answered_reviews: Vec::new(),
            answering: Vec::new(),
            build: None,
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
        // The author answers every one.
        let unjudged = review(&[(1, pass_findings(&seats, None))]);
        let answer = design_input(&after_one, Some(&unjudged), author, true).unwrap();
        let given = answer.findings.unwrap();
        assert_eq!(
            (given.mode, given.findings.len()),
            (FindingsMode::Answer, 2)
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
            Ok(approval)
        );
        assert_eq!(
            design_input(&after_one, Some(&clean), arbiter, false),
            Ok(DesignInput::default())
        );
        // The residue round: the answers are in the thread.
        let after_two = state(Stage::Design, false, 2);
        let residue = review(&[
            (1, pass_findings(&seats, Some(&rulings))),
            (2, pass_findings(&seats, Some(&rulings))),
        ]);
        assert_eq!(
            design_input(&after_two, Some(&residue), arbiter, true),
            Ok(DesignInput::default())
        );
        // A record that does not end with the state's pass is refused.
        assert!(design_input(&after_two, Some(&clean), arbiter, true).is_err());
        assert!(design_input(&after_one, None, arbiter, true).is_err());
    }

    /// The PLAN comment lists the rejections of the loop's earlier passes,
    /// never those of an earlier loop.
    #[test]
    fn the_earlier_rejections_are_the_loops_own() {
        let seats = [seat(
            &[Mandate::Critic],
            HarnessKind::Codex,
            vec![finding(1, Severity::Major)],
        )];
        let mut rejected = pass_findings(&seats, Some(&[ruling(1, RulingOutcome::Rejected)]));
        settle_pass(
            Stage::DesignReview,
            1,
            &mut rejected,
            FindingsConfirmation::Arbiter,
        );
        let record = review(&[
            (1, rejected.clone()),
            (1, rejected.clone()),
            (2, Vec::new()),
            (3, Vec::new()),
        ]);
        let earlier = earlier_rejected(&record, 3);
        assert_eq!(earlier.len(), 1, "the first loop's pass is left out");
        assert_eq!(earlier[0].0, 1);
        assert!(earlier_rejected(&record, 1).is_empty());
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

    /// The approval question offers to approve, then each fork's other
    /// options, and holds in a question's bytes whatever the plan's forks.
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
        let question = approval_question(&plan(vec![fork("Markdown", "Plain text")]));
        assert_eq!(question.category, APPROVAL_CATEGORY);
        assert_eq!(
            labels(&question),
            [APPROVE, "Fork 1: Plain text instead of Markdown"]
        );
        // It recommends approval, option A (OWL-324).
        assert_eq!(question.recommended_option(), Some(0));
        let none = approval_question(&plan(Vec::new()));
        assert_eq!(labels(&none), [APPROVE]);
        // Alternatives past 8 KiB are cut, then left to the PLAN comment.
        let long = "x".repeat(3_000);
        let other = "y".repeat(3_000);
        let wide: Vec<Fork> = (0..4).map(|_| fork(&long, &other)).collect();
        let cut = approval_question(&plan(wide));
        assert!(weight(&cut) <= MAX_QUESTION_BYTES, "{}", weight(&cut));
        assert_eq!(cut.options.len(), 5);
        let many: Vec<Fork> = (0..400).map(|n| fork(&format!("a{n}"), &long)).collect();
        let folded = approval_question(&plan(many));
        assert!(weight(&folded) <= MAX_QUESTION_BYTES, "{}", weight(&folded));
    }

    fn asked(n: u32, severity: Severity, text: usize) -> KeptFinding {
        let mut finding = finding(n, severity);
        finding.claim = "c".repeat(text);
        finding.evidence = "e".repeat(text);
        finding.settle = "s".repeat(text);
        KeptFinding {
            finding,
            reviewer: reviewer(&[Mandate::Critic], HarnessKind::Codex),
            ruling: Some(KeptRuling {
                outcome: RulingOutcome::Confirmed,
                reason: "r".repeat(text),
            }),
            response: None,
            outcome: Some(FindingOutcome::Asked),
        }
    }

    /// One question per asked finding, blockers first, within a round's
    /// bounds; past the most questions a round holds, the rest wait.
    #[test]
    fn the_residue_round_asks_each_finding_within_a_rounds_bounds() {
        let findings = vec![
            asked(1, Severity::Major, 10),
            asked(2, Severity::Blocker, 10),
            KeptFinding {
                outcome: None,
                ..asked(3, Severity::Minor, 10)
            },
        ];
        let (questions, over) = residue_questions(&findings, Stage::DesignReview, 2);
        let asked_ids: Vec<&str> = questions.iter().map(|(_, id)| id.as_str()).collect();
        assert_eq!(asked_ids, ["F2", "F1"]);
        assert!(over.is_empty());
        let (first, _) = &questions[0];
        assert_eq!(first.id, QuestionId::nth(1));
        assert_eq!(first.category, FINDING_CATEGORY);
        assert!(
            first
                .context
                .contains("Finding F2 of design review pass 2, a blocker")
        );
        assert_eq!(first.options[1].label, "Leave the plan as it is");
        // It recommends settling it, option A (OWL-324).
        assert_eq!(first.recommended_option(), Some(0));
        // At Verify, the change settles it (OWL-304).
        let (questions, _) = residue_questions(&findings, Stage::Verify, 3);
        let (first, _) = &questions[0];
        assert!(
            first
                .context
                .contains("Finding F2 of verify pass 3, a blocker")
        );
        assert!(first.text.ends_with("How should the change settle it?"));
        assert_eq!(first.options[1].label, "Leave the change as it is");

        let many: Vec<KeptFinding> = (1..=70).map(|n| asked(n, Severity::Major, 2_000)).collect();
        let (questions, over) = residue_questions(&many, Stage::DesignReview, 3);
        assert_eq!(questions.len(), MAX_QUESTIONS);
        assert_eq!(over.len(), 6);
        assert_eq!(over[0], FindingId::nth(65));
        let total: usize = questions.iter().map(|(q, _)| weight(q)).sum();
        assert!(total <= MAX_QUESTIONS_BYTES, "{total}");
        assert!(
            questions
                .iter()
                .all(|(q, _)| weight(q) <= MAX_QUESTION_BYTES)
        );
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
