//! Settling a review pass (architecture, section 5, "The quality
//! pipeline"): what becomes of each finding that stands after a pass at
//! Design review or Verify, and where the ticket goes.
//!
//! A finding stands once confirmed: by the arbiter's ruling, or, when the
//! author answers the findings (`pipeline.findings = "author"`), because
//! the author did not reject it. Which findings stand is the runner's to
//! read; what follows from them is decided here.
//!
//! Every standing finding goes to the role that owns the work, Design for
//! the plan and Build for the code, and none becomes a question by itself
//! (OWL-377, "A finding goes to its author"): its author fixes it, or keeps
//! its choice and says why. Each finding's fate, by stage, severity and
//! pass:
//!
//! | Stage | Severity | Before the last pass | At a last pass |
//! | --- | --- | --- | --- |
//! | Design review | `blocker`, `major` | [`Fate::Fix`] | [`Fate::LastFix`] |
//! | Design review | `minor` | [`Fate::Note`] | [`Fate::Note`] |
//! | Verify | `blocker`, `major` | [`Fate::Fix`] | [`Fate::LastFix`] |
//! | Verify | `minor` | [`Fate::Fix`] beside a `blocker` or `major`, else [`Fate::LastFix`] | [`Fate::LastFix`] |

use crate::gate::PlanGate;
use crate::state::{Event, is_last_review_pass};
use crate::vocab::{Severity, Stage};

/// A finding that stands after a review pass: what its fate depends on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Standing {
    pub severity: Severity,
}

/// What becomes of one standing finding.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Fate {
    /// Its author fixes it, or keeps its choice and says why: the next
    /// Design run or Verify fix run gets it, and the next pass reviews the
    /// result.
    Fix,
    /// A `minor` one at Design review, at any pass (OWL-356): listed in the
    /// PLAN comment and handed to the Build as a note, never a revision of
    /// the plan. Nothing settles it, so `review.json` keeps it with no
    /// outcome.
    Note,
    /// One its author answers in one last run, with no review pass after
    /// it: at Design review, a `blocker` or `major` left by a last pass,
    /// which Design's last revision answers (OWL-377); at Verify, a
    /// `blocker` or `major` left by a last pass, or a `minor` that no
    /// `blocker` or `major` of a pass before the last sends back for another
    /// pass (OWL-356), which one last Build answers, the runner's gate then
    /// checking it. The PLAN comment or the DELIVERY report lists what the
    /// author answered. When that run asks the decider, the pass the round
    /// buys reviews its work.
    LastFix,
}

/// Where a review pass sends the ticket.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PassOutcome {
    /// Nothing is left to fix: the stage completes.
    Passed,
    /// A pass before the last left findings: back to the author, Design or
    /// Build, for one more pass.
    Revise,
    /// Findings are left for one last run of their author, Design's last
    /// revision or Verify's last fix, with no review pass after it (OWL-356,
    /// OWL-377).
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
/// Each finding's fate follows the module's table: no finding is asked
/// (OWL-377) and none is proposed as a follow-up by the runner. The pass
/// revises when one finding is fixed by its author before the last pass,
/// gives the last run when one is left for it, and passes otherwise.
/// Whether a finding bears on a success criterion changes no fate: its
/// author may not defer it (S13), which the contracts hold
/// (`owlshift_contracts::result`).
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
            _ if last => Fate::LastFix,
            _ => Fate::Fix,
        })
        .collect();
    let outcome = if fates.contains(&Fate::Fix) {
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

    const BLOCKER: Standing = Standing {
        severity: Severity::Blocker,
    };
    const MAJOR: Standing = Standing {
        severity: Severity::Major,
    };
    const MINOR: Standing = Standing {
        severity: Severity::Minor,
    };

    fn verdict(outcome: PassOutcome, fates: &[Fate]) -> Option<PassVerdict> {
        Some(PassVerdict {
            outcome,
            fates: fates.to_vec(),
        })
    }

    #[test]
    fn a_pass_settles_by_its_stage_its_number_and_its_findings() {
        use Fate::{Fix, LastFix, Note};
        use Stage::{DesignReview, Verify};
        let cases: [(Stage, u32, &[Standing], Option<PassVerdict>); 12] = [
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
                &[MAJOR, MINOR],
                verdict(PassOutcome::Revise, &[Fix, Note]),
            ),
            (
                DesignReview,
                1,
                &[MINOR, MINOR],
                verdict(PassOutcome::Passed, &[Note, Note]),
            ),
            // Design review's last pass: a blocker or a major goes to the
            // last revision (OWL-377); a minor is noted.
            (
                DesignReview,
                2,
                &[BLOCKER, MAJOR, MINOR],
                verdict(PassOutcome::LastFix, &[LastFix, LastFix, Note]),
            ),
            (
                DesignReview,
                2,
                &[MINOR],
                verdict(PassOutcome::Passed, &[Note]),
            ),
            // At Verify, minors alone get one last Build, at any pass
            // (OWL-356).
            (
                Verify,
                1,
                &[MINOR, MINOR],
                verdict(PassOutcome::LastFix, &[LastFix, LastFix]),
            ),
            // Verify's last pass: every finding goes to the last Build,
            // none asked nor proposed (OWL-377).
            (
                Verify,
                2,
                &[BLOCKER, MAJOR, MINOR],
                verdict(PassOutcome::LastFix, &[LastFix, LastFix, LastFix]),
            ),
            // The pass an answered round buys is a last one too.
            (
                Verify,
                3,
                &[MAJOR],
                verdict(PassOutcome::LastFix, &[LastFix]),
            ),
            (DesignReview, 3, &[], verdict(PassOutcome::Passed, &[])),
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
            assert_eq!(
                PassOutcome::LastFix.design_review_event(gate),
                Event::LastFix
            );
        }
    }
}
