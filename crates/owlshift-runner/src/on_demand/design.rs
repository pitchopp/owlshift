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
//!   seat after the other on its seat's routing, the critic on Codex when
//!   Codex can run it, else standing in on Claude Code; the findings are
//!   numbered across the seats ([`pass_findings`]), confirmed by one arbiter
//!   run or left to the author's answers (`pipeline.findings`), and the pass
//!   is settled by the core ([`settle_pass`]). A pass that asks a revision
//!   loops back to Design; one that ends posts the PLAN comment, then goes to
//!   Build when the plan gate is open, or asks the decider: the approval
//!   ([`approval_question`]) or the findings left ([`residue_questions`]),
//!   without the resolver.
//!
//! A refused design, review or arbiter result is a failed run, as intake's
//! is: their briefs carry no refused result to fix. A failed review seat or
//! arbiter run is run again, the pass's other findings kept, until the
//! stage's failed-run bound parks the ticket. A failed PLAN post stops the
//! command before the pass is kept, so `continue` runs the pass again.

use std::num::NonZeroU32;

use sha2::{Digest, Sha256};

use owlshift_contracts::brief::{GivenFindings, GivenPlan, ReviewBrief};
use owlshift_contracts::config::FindingsConfirmation;
use owlshift_contracts::ids::FindingId;
use owlshift_contracts::result::{
    ApprovalOutcome, MAX_QUESTION_BYTES, MAX_QUESTIONS, MAX_QUESTIONS_BYTES,
};
use owlshift_contracts::review::{
    Finding, FindingOutcome, FindingsMode, KeptApproval, KeptFinding, KeptResponse, KeptRuling,
    Plan, ResponseOutcome, ReviewPass, Reviewer, RulingOutcome, Severity, TicketPlan, TicketReview,
};
use owlshift_core::review::{Fate, PassOutcome, PassVerdict, Standing, settle};
use owlshift_core::roster;
use owlshift_core::routing::seat_routing;
use owlshift_core::vocab::Harness as HarnessKind;

use super::*;
use crate::routing::codex_model;
use crate::ticket_ref::text_sha256;
use crate::writer::{PlanComment, PlanNext, PlanReviewer};

/// The category of the plan-approval question (architecture section 5,
/// "Plan approval"), which goes to the decider without the resolver.
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

/// Whether the kept plan is the one its decider approved.
pub(super) fn plan_approved(p: &Prepared) -> bool {
    p.plan.as_ref().is_some_and(|kept| {
        kept.approval
            .as_ref()
            .is_some_and(|approval| approval.plan_sha256 == plan_sha256(&kept.plan))
    })
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
/// one the arbiter rejected, `asked` for one the decider gets; the others
/// keep none until a run fixes them, a noted `minor` never.
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
        if *fate == Fate::Ask {
            findings[*n].outcome = Some(FindingOutcome::Asked);
        }
    }
    Some(verdict)
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
/// its options left to the PLAN comment.
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
        options,
        recommendation: None,
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
/// The bound): one per finding `findings` asks, blockers first, then by id,
/// at most [`MAX_QUESTIONS`], each cut so the round holds in
/// [`MAX_QUESTION_BYTES`] each and [`MAX_QUESTIONS_BYTES`] together. Returns
/// the questions, each with the finding it asks, and the findings past the
/// cap, left for the pass after the round.
pub(crate) fn residue_questions(
    findings: &[KeptFinding],
    pass: u32,
) -> (Vec<(Question, FindingId)>, Vec<FindingId>) {
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
                        "Finding {} of design review pass {pass}, {}, raised by the {} reviewer \
                         on {}{}, still stands after the passes the design allows. Evidence: \
                         {}. {judged}{criterion}",
                        finding.id,
                        severity_name(finding.severity),
                        mandate_names(&kept.reviewer.mandates),
                        harness_name(kept.reviewer.harness),
                        if kept.reviewer.stand_in {
                            ", standing in for Codex"
                        } else {
                            ""
                        },
                        clip(&finding.evidence, cap),
                    ),
                    text: format!(
                        "{} How should the plan settle it?",
                        clip(&finding.claim, cap)
                    ),
                    options: vec![
                        format!("Settle it: {}", clip(&finding.settle, cap)),
                        "Leave the plan as it is".to_owned(),
                    ],
                    recommendation: None,
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
            // (architecture section 5, "A false premise"); with questions it
            // is a round, all to the decider.
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
                match self.resolve(
                    p,
                    &state,
                    result,
                    &[],
                    &[],
                    &dispatched.rules,
                    &current,
                    p.settled,
                    &ran.run,
                    sink,
                )? {
                    Routing::Settled => {
                        p.settled += 1;
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
            let ask = questions.latest().ok_or_else(|| {
                Stop::Refused("an approval round the ref keeps no ask of".to_owned())
            })?;
            let approver = person(&ask.decider, &p.found);
            let comment =
                answer_check::newest_decider_comment(comments, &approver).ok_or_else(|| {
                    Stop::Refused(format!(
                        "the plan of {ticket} is approved, but no comment of its decider holds the \
                         approval"
                    ))
                })?;
            let sha = plan_sha256(&kept.plan);
            kept.approval = Some(KeptApproval {
                comment: comment.id.clone(),
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
                    ("comment", json!(comment.id)),
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
        if let (Some(given), Some(review)) = (&brief.findings, p.review.as_mut())
            && let Some(last) = review
                .passes
                .iter_mut()
                .rev()
                .find(|pass| pass.stage == Stage::DesignReview)
        {
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

    /// One Design review pass on the kept plan: each seat of the roster,
    /// the arbiter when findings are to be confirmed by it, the pass settled
    /// by the core, then a loop back to Design, or the PLAN comment and
    /// Build or a round for the decider.
    fn review_pass(
        &self,
        p: &mut Prepared,
        dispatched: &Dispatched,
        mut state: TicketState,
        sink: &mut EventSink<'_>,
    ) -> Result<TicketState, Stop> {
        let ticket = p.ticket.clone();
        let current = person(p.current()?, &p.found);
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
        let pass = state.review_pass_due();
        let pass_id = NonZeroU32::new(pass).expect("a pass due counts from 1");
        let codex = self.design_review.codex.filter(|_| p.critic.is_none());
        if let (Some(_), Some(why)) = (self.design_review.codex, &p.critic) {
            sink.emit(
                &ticket,
                None,
                EventKind::Warning,
                data([("what", json!("critic_stands_in")), ("reason", json!(why))]),
            );
        }
        let models = &self.config.models;
        let seats = roster::roster(p.pipeline.variant(), Stage::DesignReview, codex.is_some());
        let mut outputs: Vec<SeatOutput> = Vec::new();
        let mut routings = Vec::new();
        let mut last_run = None;
        for seat in seats {
            let routed = seat_routing(seat, dispatched.routing, |tier| claude_model(models, tier));
            let (router, model) = match (seat.harness, codex) {
                (HarnessKind::Codex, Some(codex)) => {
                    (codex, codex_model(models, routed.routing.tier))
                }
                _ => (
                    self.design_review.claude,
                    claude_model(models, routed.routing.tier),
                ),
            };
            let harness = router.harness(routed.routing);
            let comments = self.comments(&ticket)?;
            let mut brief = self.brief(
                p,
                Role::DesignReview,
                &comments,
                &dispatched.rules,
                None,
                &current,
            );
            brief.plan = Some(given.clone());
            brief.review = Some(ReviewBrief {
                mandates: seat.mandates.to_vec(),
                pass: pass_id,
                diff: None,
            });
            let (ran, result) = self.review_run(p, &*harness, &mut state, &brief, sink)?;
            last_run = Some(ran.run.clone());
            routings.push(routed);
            outputs.push(SeatOutput {
                reviewer: Reviewer {
                    role: Role::DesignReview,
                    mandates: seat.mandates.to_vec(),
                    harness: seat.harness,
                    model: model.unwrap_or("default").to_owned(),
                    stand_in: seat.stand_in,
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
            brief.plan = Some(given.clone());
            brief.findings = Some(GivenFindings {
                mode: FindingsMode::Rule,
                findings: findings.iter().map(|kept| kept.finding.clone()).collect(),
                diff: None,
            });
            let routing =
                role_routing(Role::Arbiter, dispatched.routing).expect("the arbiter has its layer");
            let harness = self.arbiter.harness(routing);
            let (ran, result) = self.review_run(p, &*harness, &mut state, &brief, sink)?;
            findings = pass_findings(&outputs, Some(&result.rulings));
            sink.emit(
                &ticket,
                Some(&ran.run),
                EventKind::Decision,
                data([
                    ("review", json!("ruling")),
                    ("stage", json!("design_review")),
                    ("pass", json!(pass)),
                    (
                        "confirmed",
                        finding_ids(findings.iter().filter(|kept| {
                            kept.ruling
                                .as_ref()
                                .is_some_and(|r| r.outcome == RulingOutcome::Confirmed)
                        })),
                    ),
                    (
                        "rejected",
                        finding_ids(findings.iter().filter(|kept| {
                            kept.ruling
                                .as_ref()
                                .is_some_and(|r| r.outcome == RulingOutcome::Rejected)
                        })),
                    ),
                ]),
            );
            last_run = Some(ran.run.clone());
        }
        let verdict = settle_pass(Stage::DesignReview, pass, &mut findings, confirmation)
            .expect("design review settles");
        let gate = self.plan_gate(p, &kept);
        let event = verdict.outcome.design_review_event(gate);
        // The residue round asks what it can; the rest waits for the pass
        // after the round (OWL-303).
        let (questions, not_asked) = if verdict.outcome == PassOutcome::Ask {
            residue_questions(&findings, pass)
        } else {
            (Vec::new(), Vec::new())
        };
        for kept in &mut findings {
            if not_asked.contains(&kept.finding.id) {
                kept.outcome = None;
            }
        }
        let next_name = match event {
            Event::LoopBack => "design",
            Event::Completed => "build",
            _ if verdict.outcome == PassOutcome::Passed => "approval",
            _ => "questions",
        };
        let reviewers: Vec<Value> = outputs
            .iter()
            .zip(&routings)
            .map(|(output, routed)| {
                json!({
                    "mandates": output.reviewer.mandates,
                    "harness": harness_key(output.reviewer.harness),
                    "tier": routed.routing.tier,
                    "effort": routed.routing.effort,
                    "model": output.model,
                    "stand_in": output.reviewer.stand_in,
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
                ("stage", json!("design_review")),
                ("pass", json!(pass)),
                ("outcome", json!(outcome_name(verdict.outcome))),
                ("next", json!(next_name)),
                ("findings", json!(findings.len())),
                ("standing", json!(standing(&findings, confirmation).len())),
                ("reviewers", json!(reviewers)),
            ]),
        );
        if verdict.outcome != PassOutcome::Revise {
            let noted: Vec<&KeptFinding> = standing(&findings, confirmation)
                .iter()
                .zip(&verdict.fates)
                .filter(|(_, fate)| **fate == Fate::Note)
                .map(|((n, _), _)| &findings[*n])
                .collect();
            if verdict.outcome == PassOutcome::Ask || !noted.is_empty() {
                sink.emit(
                    &ticket,
                    last_run.as_deref(),
                    EventKind::Decision,
                    data([
                        ("review", json!("residue")),
                        ("stage", json!("design_review")),
                        ("pass", json!(pass)),
                        (
                            "asked",
                            finding_ids(
                                findings
                                    .iter()
                                    .filter(|kept| kept.outcome == Some(FindingOutcome::Asked)),
                            ),
                        ),
                        ("noted", finding_ids(noted)),
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
        });
        review.passes.push(ReviewPass {
            stage: Stage::DesignReview,
            pass: pass_id,
            findings: findings.clone(),
        });
        review.validate().map_err(|e| {
            Stop::Refused(format!(
                "the review record of {ticket} would not read back ({e}): `owlshift forget \
                 {ticket}` starts it over"
            ))
        })?;
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
                    stand_in: output.reviewer.stand_in,
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
        p.review = Some(review);
        if event == Event::Completed {
            let next = match state.apply_gated(p.pipeline, Event::Completed, gate) {
                Ok(Transition::To(next)) => next,
                other => return Err(core_error(other)),
            };
            let questions = p.questions();
            self.store(p, &next, questions)
                .map_err(|e| refused("keeping the review pass in the ticket's ref", e))?;
            return Ok(next);
        }
        let (summary, round) = if verdict.outcome == PassOutcome::Passed {
            (
                format!(
                    "The plan passed design review pass {pass} and waits for your approval: the \
                     PLAN comment above holds it."
                ),
                vec![(QuestionId::nth(1), approval_question(&kept.plan))],
            )
        } else {
            (
                format!(
                    "Design review pass {pass} left findings that the plan's revisions did not \
                     settle: each is a question below, and the PLAN comment above lists them \
                     with the plan."
                ),
                questions
                    .into_iter()
                    .map(|(question, _)| (question.id.clone(), question))
                    .collect(),
            )
        };
        let asked = match state.apply(p.pipeline, Event::Questions) {
            Ok(Transition::To(asked)) => asked,
            other => return Err(core_error(other)),
        };
        let round = Round {
            questions: round,
            decided: 0,
            fallback: None,
            reopened: Vec::new(),
        };
        Err(self.ask(
            p,
            &asked,
            result::Status::Questions,
            &summary,
            round,
            last_run.as_deref().unwrap_or_default(),
            sink,
        ))
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
            stand_in: false,
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
                })
                .collect(),
            answered_reviews: Vec::new(),
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
        let question = approval_question(&plan(vec![fork("Markdown", "Plain text")]));
        assert_eq!(question.category, APPROVAL_CATEGORY);
        assert_eq!(
            question.options,
            [APPROVE, "Fork 1: Plain text instead of Markdown"]
        );
        let none = approval_question(&plan(Vec::new()));
        assert_eq!(none.options, [APPROVE]);
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
        let (questions, over) = residue_questions(&findings, 2);
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
        assert_eq!(first.options[1], "Leave the plan as it is");

        let many: Vec<KeptFinding> = (1..=70).map(|n| asked(n, Severity::Major, 2_000)).collect();
        let (questions, over) = residue_questions(&many, 3);
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
