//! Settling a review pass (architecture, section 5, "The quality
//! pipeline"): what becomes of each finding that stands after a pass at
//! Design review or Verify, and where the ticket goes.
//!
//! A finding stands once confirmed: by the arbiter's ruling, or, when the
//! author answers the findings (`pipeline.findings = "author"`), because
//! the author did not reject it. Which findings stand is the runner's to
//! read; what follows from them is decided here.

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
    /// A `minor` one after Design review's last pass: listed in the PLAN
    /// comment and handed to the Build as a note. Nothing settles it, so
    /// `review.json` keeps it with no outcome.
    Note,
    /// One after Verify's last pass that is neither a `blocker` nor on a
    /// criterion: a proposed follow-up, filed as P4 files them, with the
    /// reviewer as its source.
    Propose,
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
}

impl PassOutcome {
    /// The state machine's event for this outcome. The plan-approval gate
    /// (OWL-32) comes on top of a passed Design review.
    pub const fn event(self) -> Event {
        match self {
            Self::Passed => Event::Completed,
            Self::Revise => Event::LoopBack,
            Self::Ask => Event::Questions,
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
/// Before the last pass, every standing finding is fixed, whatever its
/// severity, and any one sends the ticket back to its author. After a last
/// pass ([`is_last_review_pass`]): at Design review a `blocker` or `major`
/// is asked and a `minor` noted; at Verify a `blocker`, or a finding on a
/// success criterion (which S13 forbids deferring), is asked and any other
/// proposed as a follow-up. The pass asks when one finding is asked, and
/// passes otherwise.
pub fn settle(stage: Stage, pass: u32, findings: &[Standing]) -> Option<PassVerdict> {
    if !matches!(stage, Stage::DesignReview | Stage::Verify) {
        return None;
    }
    let last = is_last_review_pass(pass);
    let fates: Vec<Fate> = findings
        .iter()
        .map(|finding| match (last, stage, finding.severity) {
            (false, _, _) => Fate::Fix,
            (true, Stage::DesignReview, Severity::Blocker | Severity::Major) => Fate::Ask,
            (true, Stage::DesignReview, Severity::Minor) => Fate::Note,
            (true, _, Severity::Blocker) => Fate::Ask,
            (true, _, _) if finding.on_criterion => Fate::Ask,
            (true, _, _) => Fate::Propose,
        })
        .collect();
    let outcome = if fates.contains(&Fate::Ask) {
        PassOutcome::Ask
    } else if fates.contains(&Fate::Fix) {
        PassOutcome::Revise
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
        use Fate::{Ask, Fix, Note, Propose};
        use Stage::{DesignReview, Verify};
        let cases: [(Stage, u32, &[Standing], Option<PassVerdict>); 9] = [
            // Before the last pass: fixed, whatever they weigh.
            (DesignReview, 1, &[], verdict(PassOutcome::Passed, &[])),
            (
                Verify,
                1,
                &[MINOR, BLOCKER],
                verdict(PassOutcome::Revise, &[Fix, Fix]),
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
            // Verify's last pass: a blocker or a finding on a criterion
            // asked, any other proposed.
            (
                Verify,
                2,
                &[BLOCKER, MAJOR, MINOR_ON_CRITERION],
                verdict(PassOutcome::Ask, &[Ask, Propose, Ask]),
            ),
            (
                Verify,
                2,
                &[MAJOR, MINOR],
                verdict(PassOutcome::Passed, &[Propose, Propose]),
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
    }
}
