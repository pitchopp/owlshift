//! Pipelines: the stages a ticket goes through, per variant, and where three
//! of them loop back (architecture, section 4).

use crate::vocab::{Role, Stage, Variant};

/// Every stage: the `standard` and `risky` variants.
const FULL: &[Stage] = &Stage::ALL;

/// The `trivial` variant skips Design and Design review.
const TRIVIAL: &[Stage] = &[
    Stage::Intake,
    Stage::Ready,
    Stage::Build,
    Stage::Verify,
    Stage::Deliver,
    Stage::Watch,
];

/// The sequence of stages a ticket of one variant goes through.
///
/// `standard` is the full sequence; `trivial` skips Design and Design review;
/// `risky` has the stages of `standard` and differs by its gates (plan
/// approval is mandatory, see [`crate::gate::GatePolicy`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Pipeline {
    variant: Variant,
}

impl Pipeline {
    pub const fn new(variant: Variant) -> Self {
        Self { variant }
    }

    pub const fn variant(self) -> Variant {
        self.variant
    }

    /// The stages of this pipeline, in order.
    pub const fn stages(self) -> &'static [Stage] {
        match self.variant {
            Variant::Trivial => TRIVIAL,
            Variant::Standard | Variant::Risky => FULL,
        }
    }

    pub fn contains(self, stage: Stage) -> bool {
        self.stages().contains(&stage)
    }

    /// The stage that follows `stage`: the first stage after it, in pipeline
    /// order, that this pipeline includes. It is defined for a stage the
    /// pipeline skips too, so a ticket whose variant changed while it was in
    /// such a stage still moves forward. `None` after Watch: a human merges,
    /// or the runner under merge on green (D14).
    pub fn next(self, stage: Stage) -> Option<Stage> {
        self.stages().iter().copied().find(|&s| s > stage)
    }

    /// Where a stage loops back to, if it does and this pipeline includes the
    /// target: Design review to Design (revise the plan), Verify to Build (fix)
    /// and Watch to Build (review comments on the pull request, from P5; a red
    /// check or a merge conflict gets a fix or rebase run that keeps the ticket
    /// at Watch).
    pub fn loop_back(self, stage: Stage) -> Option<Stage> {
        let target = match stage {
            Stage::DesignReview => Stage::Design,
            Stage::Verify | Stage::Watch => Stage::Build,
            Stage::Intake | Stage::Ready | Stage::Design | Stage::Build | Stage::Deliver => {
                return None;
            }
        };
        self.contains(target).then_some(target)
    }

    /// The stage a question round asked at `stage` returns to once it is
    /// answered (architecture, section 5, "The quality pipeline"): Design for
    /// one asked at Design review, since the design role runs with the
    /// answers and its plan is reviewed again; Build for one asked at
    /// Verify, a Verify fix run with the answers. Every other stage's round
    /// returns to it. A round at a stage whose loop-back target this
    /// pipeline skips returns to the stage itself.
    pub fn round_return(self, stage: Stage) -> Stage {
        match stage {
            Stage::DesignReview | Stage::Verify => self.loop_back(stage).unwrap_or(stage),
            _ => stage,
        }
    }

    /// Where a Build goes once its `replan` question is settled, answered
    /// or decided, whatever the answer: Design, whose new plan is reviewed
    /// again. `trivial` has no Design, so its `replan` is a plain question
    /// and the Build runs again.
    pub fn replan(self) -> Stage {
        if self.contains(Stage::Design) {
            Stage::Design
        } else {
            Stage::Build
        }
    }
}

impl Stage {
    /// The role a stage runs by default. `None` for Ready (nothing runs until
    /// dispatch), Deliver (the runner opens the pull request itself) and Watch
    /// (it follows the pull request; its fix and rebase runs keep the ticket at
    /// Watch, and only review comments loop back to Build).
    pub const fn default_role(self) -> Option<Role> {
        match self {
            Stage::Intake => Some(Role::Intake),
            Stage::Design => Some(Role::Design),
            Stage::DesignReview => Some(Role::DesignReview),
            Stage::Build => Some(Role::Build),
            Stage::Verify => Some(Role::Verify),
            Stage::Ready | Stage::Deliver | Stage::Watch => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STANDARD: Pipeline = Pipeline::new(Variant::Standard);
    const TRIVIAL_PIPELINE: Pipeline = Pipeline::new(Variant::Trivial);
    const RISKY: Pipeline = Pipeline::new(Variant::Risky);

    /// Follows `next` from Intake to the end.
    fn walk(pipeline: Pipeline) -> Vec<Stage> {
        std::iter::successors(Some(Stage::Intake), |&s| pipeline.next(s)).collect()
    }

    #[test]
    fn standard_and_risky_go_through_every_stage() {
        assert_eq!(walk(STANDARD), Stage::ALL);
        assert_eq!(walk(RISKY), Stage::ALL);
        assert_eq!(STANDARD.stages(), Stage::ALL);
    }

    #[test]
    fn trivial_skips_design_and_design_review() {
        let expected = [
            Stage::Intake,
            Stage::Ready,
            Stage::Build,
            Stage::Verify,
            Stage::Deliver,
            Stage::Watch,
        ];
        assert_eq!(walk(TRIVIAL_PIPELINE), expected);
        assert!(!TRIVIAL_PIPELINE.contains(Stage::Design));
        assert!(!TRIVIAL_PIPELINE.contains(Stage::DesignReview));
    }

    #[test]
    fn next_moves_forward_from_a_stage_the_pipeline_skips() {
        assert_eq!(TRIVIAL_PIPELINE.next(Stage::Design), Some(Stage::Build));
        assert_eq!(
            TRIVIAL_PIPELINE.next(Stage::DesignReview),
            Some(Stage::Build)
        );
    }

    #[test]
    fn loop_backs() {
        for pipeline in [STANDARD, RISKY] {
            assert_eq!(pipeline.loop_back(Stage::DesignReview), Some(Stage::Design));
            assert_eq!(pipeline.loop_back(Stage::Verify), Some(Stage::Build));
            assert_eq!(pipeline.loop_back(Stage::Watch), Some(Stage::Build));
        }
        // Trivial has no Design stage to go back to.
        assert_eq!(TRIVIAL_PIPELINE.loop_back(Stage::DesignReview), None);
        assert_eq!(
            TRIVIAL_PIPELINE.loop_back(Stage::Verify),
            Some(Stage::Build)
        );
        assert_eq!(TRIVIAL_PIPELINE.loop_back(Stage::Watch), Some(Stage::Build));

        let looping = [Stage::DesignReview, Stage::Verify, Stage::Watch];
        for stage in Stage::ALL.into_iter().filter(|s| !looping.contains(s)) {
            assert_eq!(STANDARD.loop_back(stage), None, "{stage:?}");
        }
    }

    #[test]
    fn a_dispatch_from_ready_starts_design_or_for_trivial_build() {
        assert_eq!(STANDARD.next(Stage::Ready), Some(Stage::Design));
        assert_eq!(RISKY.next(Stage::Ready), Some(Stage::Design));
        assert_eq!(TRIVIAL_PIPELINE.next(Stage::Ready), Some(Stage::Build));
    }

    #[test]
    fn rounds_at_a_review_stage_return_to_its_author_and_others_to_themselves() {
        for pipeline in [STANDARD, RISKY] {
            assert_eq!(pipeline.round_return(Stage::DesignReview), Stage::Design);
            assert_eq!(pipeline.round_return(Stage::Verify), Stage::Build);
            for stage in [Stage::Intake, Stage::Design, Stage::Build, Stage::Watch] {
                assert_eq!(pipeline.round_return(stage), stage, "{stage:?}");
            }
            assert_eq!(pipeline.replan(), Stage::Design);
        }
        assert_eq!(TRIVIAL_PIPELINE.round_return(Stage::Verify), Stage::Build);
        // A stage trivial skips: no Design to return to.
        assert_eq!(
            TRIVIAL_PIPELINE.round_return(Stage::DesignReview),
            Stage::DesignReview
        );
        assert_eq!(TRIVIAL_PIPELINE.replan(), Stage::Build);
    }

    #[test]
    fn default_roles() {
        let roles: Vec<_> = Stage::ALL.into_iter().map(Stage::default_role).collect();
        assert_eq!(
            roles,
            [
                Some(Role::Intake),
                None,
                Some(Role::Design),
                Some(Role::DesignReview),
                Some(Role::Build),
                Some(Role::Verify),
                None,
                None,
            ]
        );
    }
}
