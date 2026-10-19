//! Settling a review pass (architecture, section 5, "The quality
//! pipeline"): what becomes of each finding that stands after a pass at
//! Design review or Verify, and where the ticket goes.
//!
//! A finding stands once confirmed: by the arbiter's ruling, or, when the
//! author answers the findings (`pipeline.findings = "author"`), because
//! the author did not reject it. Which findings stand is the runner's to
//! read; what follows from them is decided here.

use crate::gate::PlanGate;
use crate::state::{Event, is_last_review_pass};
use crate::vocab::{Severity, Stage};

/// A finding that stands after a review pass: what its fate depends on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Standing {
    pub severity: Severity,
    /// Whether it bears on one of the ticket's success criteria.
    pub on_criterion: bool,
}

/// What becomes of one standing finding.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Fate {
    /// Its author fixes it: the next Design run or Verify fix run gets it,
    /// and the next pass reviews the result.
    Fix,
    /// It goes to the decider in the pass's question round, one question
    /// per finding, without the resolver.
    Ask,
    /// A `minor` one at Design review, at any pass (OWL-356): listed in the
    /// PLAN comment and handed to the Build as a note, never a revision of
    /// the plan. Nothing settles it, so `review.json` keeps it with no
    /// outcome.
    Note,
    /// A `major` one after Verify's last pass that is not on a criterion: a
    /// proposed follow-up, filed as P4 files them, with the reviewer as its
    /// source.
    Propose,
    /// A `minor` one at Verify that no `blocker` or `major` of the pass
    /// sends back for another pass (OWL-356): fixed in one last Build,
    /// which the runner's gate checks, with no Verify pass after it, and
    /// listed in the DELIVERY report. When the pass asks the decider, the
    /// Build after the round fixes it, and the pass the round buys reviews
    /// it.
    LastFix,
}

/// Where a review pass sends the ticket.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PassOutcome {
    /// Nothing is left to fix or ask: the stage completes.
    Passed,
    /// A pass before the last left findings: back to the author, Design or
    /// Build, for one more pass.
    Revise,
    /// The last pass left findings for the decider: a question round,
    /// which returns to the author ([`crate::pipeline::Pipeline::round_return`]).
    Ask,
    /// At Verify, only `minor` findings are left to fix (OWL-356): one last
    /// Build fixes them, and the delivery follows its gate with no Verify
    /// pass. Never at Design review.
    LastFix,
}

impl PassOutcome {
    /// The state machine's event for this outcome. At Design review, the
    /// plan-approval gate comes on top of a passed pass:
    /// [`PassOutcome::design_review_event`].
    pub const fn event(self) -> Event {
        match self {
            Self::Passed => Event::Completed,
            Self::Revise => Event::LoopBack,
            Self::Ask => Event::Questions,
            Self::LastFix => Event::LastFix,
        }
    }

    /// The event for this outcome of a Design review pass, given the plan
    /// gate of the plan it reviewed (OWL-32): a passed pass whose gate is
    /// shut opens the approval round, a [`Event::Questions`] straight to
    /// the decider, which counts the pass and returns to Design; with the
    /// gate open it completes, to Build
    /// ([`crate::state::TicketState::apply_gated`]).
    pub const fn design_review_event(self, gate: PlanGate) -> Event {
        match self {
            Self::Passed if !gate.is_open() => Event::Questions,
            _ => self.event(),
        }
    }
}

/// How a review pass settles: its outcome, and the fate of each standing
/// finding, in the order given.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PassVerdict {
    pub outcome: PassOutcome,
    pub fates: Vec<Fate>,
}

/// Settles review pass number `pass` (from 1, as
/// [`crate::state::TicketState::review_pass_due`] gives it) at `stage`,
/// from the findings that stand after it. `None` for a stage that is not
/// Design review or Verify.
///
/// A `minor` never sends the ticket back for another pass (OWL-356): at
/// Design review it is noted for the Build, at any pass; at Verify it is
/// fixed with the `blocker` and `major` findings of a pass before the last,
/// and otherwise in one last Build with no Verify pass after it. Before the
/// last pass, every `blocker` and `major` is fixed, and any one sends the
/// ticket back to its author. After a last pass ([`is_last_review_pass`]):
/// at Design review a `blocker` or `major` is asked; at Verify a `blocker`,
/// or a `major` on a success criterion (which S13 forbids deferring), is
/// asked and any other `major` proposed as a follow-up. The pass asks when
/// one finding is asked, revises when one is fixed by its author, gives
/// the last fix when one is left for it, and passes otherwise.
pub fn settle(stage: Stage, pass: u32, findings: &[Standing]) -> Option<PassVerdict> {
    if !matches!(stage, Stage::DesignReview | Stage::Verify) {
        return None;
    }
    let last = is_last_review_pass(pass);
    let revises = !last
        && findings
            .iter()
            .any(|finding| finding.severity != Severity::Minor);
    let fates: Vec<Fate> = findings
        .iter()
        .map(|finding| match (stage, finding.severity) {
            (Stage::DesignReview, Severity::Minor) => Fate::Note,
            (_, Severity::Minor) if revises => Fate::Fix,
            (_, Severity::Minor) => Fate::LastFix,
            _ if !last => Fate::Fix,
            (Stage::DesignReview, _) | (_, Severity::Blocker) => Fate::Ask,
            _ if finding.on_criterion => Fate::Ask,
            _ => Fate::Propose,
        })
        .collect();
    let outcome = if fates.contains(&Fate::Ask) {
        PassOutcome::Ask
    } else if fates.contains(&Fate::Fix) {
        PassOutcome::Revise
    } else if fates.contains(&Fate::LastFix) {
        PassOutcome::LastFix
    } else {
        PassOutcome::Passed
    };
    Some(PassVerdict { outcome, fates })
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn standing(severity: Severity, on_criterion: bool) -> Standing {
        Standing {
            severity,
            on_criterion,
        }
    }

    const BLOCKER: Standing = standing(Severity::Blocker, false);
    const MAJOR: Standing = standing(Severity::Major, false);
    const MAJOR_ON_CRITERION: Standing = standing(Severity::Major, true);
    const MINOR: Standing = standing(Severity::Minor, false);
    const MINOR_ON_CRITERION: Standing = standing(Severity::Minor, true);

    fn verdict(outcome: PassOutcome, fates: &[Fate]) -> Option<PassVerdict> {
        Some(PassVerdict {
            outcome,
            fates: fates.to_vec(),
        })
    }

    #[test]
    fn a_pass_settles_by_its_stage_its_number_and_its_findings() {
        use Fate::{Ask, Fix, LastFix, Note, Propose};
        use Stage::{DesignReview, Verify};
        let cases: [(Stage, u32, &[Standing], Option<PassVerdict>); 13] = [
            // Before the last pass: a blocker or a major fixed.
            (DesignReview, 1, &[], verdict(PassOutcome::Passed, &[])),
            (
                Verify,
                1,
                &[MINOR, BLOCKER],
                verdict(PassOutcome::Revise, &[Fix, Fix]),
            ),
            // A minor never revises the plan: a note, at any pass
            // (OWL-356).
            (
                DesignReview,
                1,
                &[MAJOR, MINOR_ON_CRITERION],
                verdict(PassOutcome::Revise, &[Fix, Note]),
            ),
            (
                DesignReview,
                1,
                &[MINOR, MINOR],
                verdict(PassOutcome::Passed, &[Note, Note]),
            ),
            // Design review's last pass: blocker and major asked, minor noted.
            (
                DesignReview,
                2,
                &[BLOCKER, MAJOR_ON_CRITERION, MINOR_ON_CRITERION],
                verdict(PassOutcome::Ask, &[Ask, Ask, Note]),
            ),
            (
                DesignReview,
                2,
                &[MINOR],
                verdict(PassOutcome::Passed, &[Note]),
            ),
            // At Verify, minors alone get one last Build, at any pass, one on
            // a criterion included (OWL-356).
            (
                Verify,
                1,
                &[MINOR, MINOR_ON_CRITERION],
                verdict(PassOutcome::LastFix, &[LastFix, LastFix]),
            ),
            // Verify's last pass: a blocker or a major on a criterion asked,
            // any other major proposed, a minor given the last fix.
            (
                Verify,
                2,
                &[BLOCKER, MAJOR, MINOR_ON_CRITERION],
                verdict(PassOutcome::Ask, &[Ask, Propose, LastFix]),
            ),
            (
                Verify,
                2,
                &[MAJOR_ON_CRITERION],
                verdict(PassOutcome::Ask, &[Ask]),
            ),
            (
                Verify,
                2,
                &[MAJOR, MINOR],
                verdict(PassOutcome::LastFix, &[Propose, LastFix]),
            ),
            // The pass an answered round buys is a last one too.
            (
                Verify,
                3,
                &[MAJOR],
                verdict(PassOutcome::Passed, &[Propose]),
            ),
            // Not a review stage.
            (Stage::Build, 1, &[MAJOR], None),
            (Stage::Design, 2, &[], None),
        ];
        for (stage, pass, findings, expected) in cases {
            assert_eq!(
                settle(stage, pass, findings),
                expected,
                "{stage:?} pass {pass}: {findings:?}"
            );
        }
    }

    #[test]
    fn each_outcome_is_the_machines_event() {
        assert_eq!(PassOutcome::Passed.event(), Event::Completed);
        assert_eq!(PassOutcome::Revise.event(), Event::LoopBack);
        assert_eq!(PassOutcome::Ask.event(), Event::Questions);
        assert_eq!(PassOutcome::LastFix.event(), Event::LastFix);
    }

    /// A passed Design review pass whose plan gate is shut opens the
    /// approval round; the gate changes no other outcome.
    #[test]
    fn a_passed_design_review_with_its_gate_shut_asks_for_approval() {
        use crate::gate::GatePolicy;
        use crate::pipeline::Pipeline;
        use crate::vocab::{PlanApproval, Variant};
        let policy = GatePolicy::new(Vec::<String>::new(), PlanApproval::Never);
        let plan = "a".repeat(64);
        let shut = policy.plan_gate(Pipeline::new(Variant::Risky), false, &plan, None);
        let open = policy.plan_gate(Pipeline::new(Variant::Risky), false, &plan, Some(&plan));
        assert_eq!(
            PassOutcome::Passed.design_review_event(shut),
            Event::Questions
        );
        assert_eq!(
            PassOutcome::Passed.design_review_event(open),
            Event::Completed
        );
        for gate in [shut, open] {
            assert_eq!(
                PassOutcome::Revise.design_review_event(gate),
                Event::LoopBack
            );
            assert_eq!(PassOutcome::Ask.design_review_event(gate), Event::Questions);
        }
    }
}
