//! Verify (OWL-304; architecture section 5, "The quality pipeline";
//! build plan, "P5 · Quality pipeline", part G): once a Build is done, its
//! gate passed under the runner and the thread read again for late
//! comments, the variant's roster reviews the Build's diff before the push.
//!
//! - *The pass.* [`OnDemand::verify_pass`] writes the diff from the commit
//!   the Build started from to the Build's commit beside the brief, then
//!   runs the pass Design review runs ([`OnDemand::run_review_pass`]): the
//!   roster of the variant at Verify, the arbiter or the author's answers,
//!   the core's settling.
//! - *The fix loop.* A pass before the last that leaves findings loops back
//!   to Build: a Verify fix run, given the findings ([`verify_input`]), the
//!   gate, then the next pass. At most two passes.
//! - *The residue.* After the last pass, a `blocker` or a finding on a
//!   success criterion is asked of the decider, without the resolver, in a
//!   round that returns to Build; any other becomes a follow-up of the
//!   delivery ([`review_followups`]), filed with the DELIVERY report as P4
//!   files them.
//! - *What the Build gave.* The Build's delivery data is kept in
//!   `review.json` ([`KeptBuild`]) once its gate passed, so that `continue`
//!   on a ticket cut short or parked at Verify runs the whole pass again and
//!   delivers, as it does at Design review (part F). A pass that passed
//!   leads to the delivery in the same command.

use owlshift_contracts::brief::{GivenDiff, GivenFindings};
use owlshift_contracts::config::FindingsConfirmation;
use owlshift_contracts::result::FollowupSource;
use owlshift_contracts::review::{
    Finding, FindingOutcome, FindingsMode, KeptBuild, KeptFinding, ReviewPass, TicketReview,
};
use owlshift_core::review::PassOutcome;

use super::design::{self, PassRun, settle_pass, standing};
use super::*;
use crate::executor::{DIFF_PATH, write_diff};
use crate::writer::ReportedReview;

/// What a Verify pass came to, when it does not ask the decider.
pub(super) enum Verified {
    /// Nothing is left to fix or ask: the ticket is delivered, in this
    /// state, still at Verify.
    Passed(TicketState),
    /// Findings go back to Build for a Verify fix run, in this state.
    Revise(TicketState),
}

/// The most characters of a finding's claim a follow-up's title keeps.
const FOLLOWUP_TITLE_CHARS: usize = 120;

/// What the Build run due at Build in `state` is given of the latest Verify
/// pass (OWL-304), as a design run is given Design review's
/// ([`design::design_input`]): nothing before a Verify pass ended in this
/// loop; after a pass that asked a revision, its findings, settled again
/// under `confirmation`, the standing ones to fix when the arbiter confirms
/// them, every one to answer when the author does; after a pass that asked
/// the decider, nothing, the answers being in the thread. A review record
/// whose latest Verify pass is not the state's latest is an error.
pub(crate) fn verify_input(
    state: &TicketState,
    review: Option<&TicketReview>,
    confirmation: FindingsConfirmation,
) -> Result<Option<GivenFindings>, String> {
    let passes = state.review_passes();
    if passes == 0 || state.stage() != Stage::Build {
        return Ok(None);
    }
    let last = review
        .and_then(|review| {
            review
                .passes
                .iter()
                .rev()
                .find(|pass| pass.stage == Stage::Verify)
        })
        .filter(|last| last.pass.get() == passes)
        .ok_or_else(|| {
            format!(
                "the ticket's state counts {passes} verify passes but its review record does not \
                 end with pass {passes}"
            )
        })?;
    let mut findings = last.findings.clone();
    let verdict =
        settle_pass(Stage::Verify, passes, &mut findings, confirmation).expect("verify settles");
    if verdict.outcome != PassOutcome::Revise {
        return Ok(None);
    }
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
    Ok(Some(GivenFindings {
        mode,
        findings: given,
        diff: None,
    }))
}

/// The follow-ups of the findings a Verify pass proposed (architecture
/// section 5, "Verify", The residue): each with the reviewer as its source,
/// its claim as its reason, cut for its title, its evidence, and what would
/// settle it as when it is done.
pub(crate) fn review_followups(findings: &[KeptFinding]) -> Vec<Followup> {
    findings
        .iter()
        .filter(|kept| kept.outcome == Some(FindingOutcome::Proposed))
        .map(|kept| {
            let finding = &kept.finding;
            let claim = finding
                .claim
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            let title = if claim.chars().count() <= FOLLOWUP_TITLE_CHARS {
                claim.clone()
            } else {
                let mut cut: String = claim.chars().take(FOLLOWUP_TITLE_CHARS - 1).collect();
                cut.push('…');
                cut
            };
            Followup {
                title,
                why: finding.claim.clone(),
                evidence: vec![finding.evidence.clone()],
                source: FollowupSource::Reviewer,
                done_when: finding.settle.clone(),
                blocked_by_parent: false,
            }
        })
        .collect()
}

/// The Verify passes of the review loop the record ends with, oldest
/// first: from its last pass, when at Verify, back while each is the one
/// before it at Verify, down to its pass 1; none when the record ends with
/// a Design review pass. A loop starts again at 1 after a ticket is
/// dispatched again or reviewed on its pull request, so an earlier loop's
/// passes are never counted with it (OWL-304).
pub(crate) fn verify_loop(review: &TicketReview) -> Vec<&ReviewPass> {
    let mut passes: Vec<&ReviewPass> = Vec::new();
    let mut expected: Option<u32> = None;
    for kept in review.passes.iter().rev() {
        if kept.stage != Stage::Verify || expected.is_some_and(|n| kept.pass.get() != n) {
            break;
        }
        passes.push(kept);
        if kept.pass.get() == 1 {
            break;
        }
        expected = Some(kept.pass.get() - 1);
    }
    passes.reverse();
    passes
}

/// What the DELIVERY report says of the review loop `review` ends with at
/// Verify ([`verify_loop`]): its passes, the reviewers of the last, the
/// findings rejected and those proposed over them; none when it ends with
/// no Verify pass.
pub(crate) fn reported_review(review: &TicketReview) -> Option<ReportedReview> {
    let passes = verify_loop(review);
    let last = passes.last()?;
    let of = |outcome: FindingOutcome| -> Vec<(u32, KeptFinding)> {
        passes
            .iter()
            .flat_map(|pass| {
                pass.findings
                    .iter()
                    .filter(move |kept| kept.outcome == Some(outcome))
                    .map(|kept| (pass.pass.get(), kept.clone()))
            })
            .collect()
    };
    Some(ReportedReview {
        passes: u32::try_from(passes.len()).unwrap_or(u32::MAX),
        reviewers: last.reviewers.clone(),
        rejected: of(FindingOutcome::Rejected),
        proposed: of(FindingOutcome::Proposed),
    })
}

impl OnDemand<'_> {
    /// What a Build run `report` done with its gate passed gives its
    /// delivery, with the notes and follow-ups `gathered` over the
    /// delivery's runs, and the newest edit of the decider's comments its
    /// brief held, `read` (OWL-139's mark), when the ref keeps an ask.
    pub(super) fn kept_build(
        &self,
        p: &Prepared,
        report: &RunReport,
        gathered: &Gathered,
        read: &[Comment],
    ) -> Result<KeptBuild, Stop> {
        let (Outcome::Finished { result, .. }, Some(gate)) = (&report.outcome, &report.gate) else {
            return Err(Stop::Delivery(
                "the run did not report a passing gate".to_owned(),
            ));
        };
        let commit = gate
            .commit
            .as_deref()
            .filter(|_| gate.passed())
            .and_then(|commit| CommitId::new(commit).ok())
            .ok_or_else(|| Stop::Delivery("the gate's commit is unknown".to_owned()))?;
        let seen = p.questions().latest().and_then(|ask| {
            answer_check::newest_decider_edit(read, &person(&ask.decider, &p.found))
        });
        Ok(KeptBuild {
            commit: commit.to_string(),
            summary: result.summary.clone(),
            pr: result.pr.clone(),
            gate: gate.commands.clone(),
            notes: gathered.decisions.clone(),
            followups: gathered.followups.clone(),
            seen,
        })
    }

    /// The diff a Verify pass reviews (OWL-302): from the commit the Build
    /// started from, the merge base of the base fetched by this command and
    /// `commit`, which a branch fast-forwarded at its dispatch or kept by a
    /// `continue` both give, to `commit`; written at [`DIFF_PATH`] in the
    /// worktree, with no external diff, text conversion or colour.
    fn review_diff(&self, p: &Prepared, commit: &str) -> Result<GivenDiff, Stop> {
        let git = &self.executor.git;
        let base = git
            .run(&p.checkout, &["merge-base", &p.base.commit, commit])
            .map_err(|e| refused("reading the commit the Build started from", e))?;
        let base = String::from_utf8_lossy(&base).trim().to_owned();
        CommitId::new(base.as_str())
            .map_err(|e| refused("reading the commit the Build started from", e))?;
        let diff = git
            .run(
                &p.checkout,
                &[
                    "diff",
                    "--no-ext-diff",
                    "--no-textconv",
                    "--no-color",
                    &base,
                    commit,
                ],
            )
            .map_err(|e| refused("writing the diff Verify reviews", e))?;
        write_diff(&p.worktree, &diff)
            .map_err(|e| refused("writing the diff Verify reviews", e))?;
        Ok(GivenDiff {
            base,
            path: RelativePath::new(DIFF_PATH).expect("DIFF_PATH is a relative path"),
        })
    }

    /// One Verify pass on the Build `review.json` keeps (OWL-304): the diff
    /// written, the pass run as Design review's is, its proposed follow-ups
    /// added to the delivery's. A pass that asks a revision loops back to
    /// Build, kept; one that leaves findings for the decider asks them,
    /// which stops the command; one that passes leads to the delivery,
    /// which keeps it with its push.
    pub(super) fn verify_pass(
        &self,
        p: &mut Prepared,
        dispatched: &Dispatched,
        mut state: TicketState,
        sink: &mut EventSink<'_>,
    ) -> Result<Verified, Stop> {
        let ticket = p.ticket.clone();
        let built = p
            .review
            .as_ref()
            .and_then(|review| review.build.clone())
            .ok_or_else(|| {
                Stop::Refused(format!(
                    "{ticket} is at verify but its ref keeps no Build to review: `owlshift do \
                     {ticket}` builds it again"
                ))
            })?;
        let diff = self.review_diff(p, &built.commit)?;
        let next = |outcome: PassOutcome| match outcome {
            PassOutcome::Passed => "deliver",
            PassOutcome::Revise => "build",
            PassOutcome::Ask => "questions",
        };
        let PassRun {
            pass,
            findings,
            verdict,
            questions,
            last_run,
            mut review,
            ..
        } = self.run_review_pass(
            p,
            dispatched,
            &mut state,
            Stage::Verify,
            design::given_plan(p),
            Some(diff),
            &next,
            sink,
        )?;
        if let Some(build) = review.build.as_mut() {
            gather(&mut build.followups, &review_followups(&findings));
        }
        p.review = Some(review);
        match verdict.outcome {
            PassOutcome::Passed => Ok(Verified::Passed(state)),
            PassOutcome::Revise => {
                let next = match state.apply(p.pipeline, Event::LoopBack) {
                    Ok(Transition::To(next)) => next,
                    other => return Err(core_error(other)),
                };
                let questions = p.questions();
                self.store(p, &next, questions)
                    .map_err(|e| refused("keeping the verify pass in the ticket's ref", e))?;
                Ok(Verified::Revise(next))
            }
            PassOutcome::Ask => {
                let asked = match state.apply(p.pipeline, Event::Questions) {
                    Ok(Transition::To(asked)) => asked,
                    other => return Err(core_error(other)),
                };
                let round = Round {
                    questions: questions
                        .into_iter()
                        .map(|(question, _)| (question.id.clone(), question))
                        .collect(),
                    decided: 0,
                    fallback: None,
                    reopened: Vec::new(),
                };
                let summary = format!(
                    "Verify pass {pass} left findings on the change that its fix runs did not \
                     settle: each is a question below. A fix run follows your answers, then one \
                     more verify pass."
                );
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
    }

    /// Whether `continue` may run again the Verify pass of a ticket at
    /// Verify, parked there or cut short (OWL-304), checked before a park
    /// is restarted or anything kept: its ref keeps the Build to review, its
    /// branch is still at that Build's commit, and the decider of its latest
    /// ask has not commented since that Build read the thread, which only a
    /// Build run would follow (OWL-139). Otherwise `owlshift do` builds it
    /// again.
    pub(super) fn verify_resumable(&self, p: &Prepared) -> Result<(), Stop> {
        let ticket = &p.ticket;
        let again = format!("`owlshift do {ticket}` builds it again");
        let Some(built) = p.review.as_ref().and_then(|review| review.build.as_ref()) else {
            return Err(Stop::Refused(format!(
                "{ticket} is at verify but its ref keeps no Build to review: {again}"
            )));
        };
        let head = self.branch_head(p);
        if head.as_deref() != Some(built.commit.as_str()) {
            return Err(Stop::Refused(format!(
                "{ticket}'s branch is at {}, not at {}, the commit its Build delivered for \
                 review: {again}",
                head.as_deref().unwrap_or("no commit"),
                built.commit
            )));
        }
        if let Some(ask) = p.questions().latest() {
            let decider = person(&ask.decider, &p.found);
            let now = self.comments(ticket)?;
            let late = answer_check::edited_after(built.seen, &now, &decider);
            if !late.is_empty() {
                return Err(Stop::Refused(format!(
                    "{} commented on {ticket} after the Build that Verify reviews read the \
                     thread, at {}: {again}, following the comment",
                    decider.name,
                    late_times(&late.into_iter().cloned().collect::<Vec<_>>())
                )));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;

    use owlshift_contracts::ids::FindingId;
    use owlshift_contracts::review::{
        KeptResponse, KeptRuling, Mandate, ResponseOutcome, Reviewer, RulingOutcome, Severity,
    };
    use owlshift_core::state::{Status, TicketState};
    use owlshift_core::vocab::Harness as HarnessKind;

    use super::*;

    fn kept(n: u32, severity: Severity, ruling: Option<RulingOutcome>) -> KeptFinding {
        KeptFinding {
            finding: Finding {
                id: FindingId::nth(n),
                severity,
                claim: format!("Claim  {n}\nwrapped."),
                evidence: "src/lib.rs:1".to_owned(),
                criterion: None,
                settle: "Settle it.".to_owned(),
            },
            reviewer: Reviewer {
                role: Role::Verify,
                mandates: vec![Mandate::Critic, Mandate::Conformance],
                harness: HarnessKind::Codex,
                model: "default".to_owned(),
                stand_in: false,
                same_model: false,
            },
            ruling: ruling.map(|outcome| KeptRuling {
                outcome,
                reason: "Checked.".to_owned(),
            }),
            response: None,
            outcome: None,
        }
    }

    fn pass(stage: Stage, n: u32, findings: Vec<KeptFinding>) -> ReviewPass {
        ReviewPass {
            stage,
            pass: NonZeroU32::new(n).unwrap(),
            findings,
            reviewers: Vec::new(),
        }
    }

    fn review(passes: Vec<ReviewPass>) -> TicketReview {
        TicketReview {
            format: Format,
            passes,
            answered_reviews: Vec::new(),
            build: None,
        }
    }

    fn at_build(passes: u32) -> TicketState {
        TicketState::restore(Status::Active(Stage::Build), 0, 0, 0, 0, 0)
            .unwrap()
            .with_review_passes(passes)
            .unwrap()
    }

    /// A Verify fix run is given what the latest Verify pass left, settled
    /// again: the confirmed findings to fix, every one to answer, nothing
    /// after a round; a record that does not end with the state's pass is
    /// refused.
    #[test]
    fn a_verify_fix_run_is_given_what_the_latest_pass_left() {
        let arbiter = FindingsConfirmation::Arbiter;
        let author = FindingsConfirmation::Author;
        assert_eq!(verify_input(&at_build(0), None, arbiter), Ok(None));
        let ruled = review(vec![
            pass(Stage::DesignReview, 1, Vec::new()),
            pass(
                Stage::Verify,
                1,
                vec![
                    kept(1, Severity::Major, Some(RulingOutcome::Confirmed)),
                    kept(2, Severity::Minor, Some(RulingOutcome::Rejected)),
                ],
            ),
        ]);
        let fix = verify_input(&at_build(1), Some(&ruled), arbiter)
            .unwrap()
            .unwrap();
        assert_eq!(fix.mode, FindingsMode::Fix);
        let ids: Vec<&str> = fix.findings.iter().map(|f| f.id.as_str()).collect();
        assert_eq!(ids, ["F1"]);
        let unjudged = review(vec![pass(
            Stage::Verify,
            1,
            vec![
                kept(1, Severity::Major, None),
                kept(2, Severity::Minor, None),
            ],
        )]);
        let answer = verify_input(&at_build(1), Some(&unjudged), author)
            .unwrap()
            .unwrap();
        assert_eq!(
            (answer.mode, answer.findings.len()),
            (FindingsMode::Answer, 2)
        );
        // After the last pass's round, the answers are in the thread.
        let asked = review(vec![
            pass(Stage::Verify, 1, Vec::new()),
            pass(
                Stage::Verify,
                2,
                vec![kept(1, Severity::Blocker, Some(RulingOutcome::Confirmed))],
            ),
        ]);
        assert_eq!(verify_input(&at_build(2), Some(&asked), arbiter), Ok(None));
        assert!(verify_input(&at_build(2), Some(&ruled), arbiter).is_err());
        assert!(verify_input(&at_build(1), None, arbiter).is_err());
    }

    /// A proposed finding becomes a follow-up from the reviewer; the others
    /// do not.
    #[test]
    fn a_proposed_finding_becomes_a_reviewers_followup() {
        let mut proposed = kept(1, Severity::Major, Some(RulingOutcome::Confirmed));
        proposed.outcome = Some(FindingOutcome::Proposed);
        let mut long = kept(2, Severity::Minor, None);
        long.finding.claim = "x".repeat(300);
        long.outcome = Some(FindingOutcome::Proposed);
        long.response = Some(KeptResponse {
            outcome: ResponseOutcome::Fixed,
            reason: "r".to_owned(),
        });
        let asked = KeptFinding {
            outcome: Some(FindingOutcome::Asked),
            ..kept(3, Severity::Blocker, None)
        };
        let followups = review_followups(&[proposed, long, asked]);
        assert_eq!(followups.len(), 2);
        assert_eq!(followups[0].title, "Claim 1 wrapped.");
        assert_eq!(followups[0].why, "Claim  1\nwrapped.");
        assert_eq!(followups[0].evidence, ["src/lib.rs:1"]);
        assert_eq!(followups[0].done_when, "Settle it.");
        assert_eq!(followups[0].source, FollowupSource::Reviewer);
        assert_eq!(followups[1].title.chars().count(), FOLLOWUP_TITLE_CHARS);
    }

    /// The latest Verify loop is the last run of passes counting up from 1:
    /// a loop started again leaves the earlier one out.
    #[test]
    fn the_latest_verify_loop_counts_up_from_its_pass_1() {
        let numbers = |record: &TicketReview| -> Vec<(Stage, u32)> {
            verify_loop(record)
                .iter()
                .map(|kept| (kept.stage, kept.pass.get()))
                .collect()
        };
        let record = review(vec![
            pass(Stage::Verify, 1, Vec::new()),
            pass(Stage::Verify, 2, Vec::new()),
            pass(Stage::Verify, 1, Vec::new()),
        ]);
        assert_eq!(numbers(&record), [(Stage::Verify, 1)]);
        let record = review(vec![
            pass(Stage::Verify, 1, Vec::new()),
            pass(Stage::DesignReview, 1, Vec::new()),
            pass(Stage::Verify, 1, Vec::new()),
            pass(Stage::Verify, 2, Vec::new()),
            pass(Stage::Verify, 3, Vec::new()),
        ]);
        assert_eq!(
            numbers(&record),
            [(Stage::Verify, 1), (Stage::Verify, 2), (Stage::Verify, 3)]
        );
        let record = review(vec![
            pass(Stage::Verify, 1, Vec::new()),
            pass(Stage::DesignReview, 1, Vec::new()),
        ]);
        assert!(numbers(&record).is_empty());
    }
}
