//! The shared vocabulary: the closed sets of names that the core decides on
//! and that the contracts carry on the wire.
//!
//! These types only derive their serialized form and their JSON Schema; the
//! core model built on them (ticket, pipeline, state machine) is separate.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A stage of the pipeline, or `ready` for an admitted ticket not yet started.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    Intake,
    Ready,
    Design,
    DesignReview,
    Build,
    Verify,
    Deliver,
    Watch,
}

// The declaration order above is the pipeline's order, so `Ord` sorts stages
// the way a ticket goes through them. (A doc comment on `Stage` itself would
// change the committed JSON Schemas.)
impl Stage {
    /// Every stage, in pipeline order.
    pub const ALL: [Self; 8] = [
        Self::Intake,
        Self::Ready,
        Self::Design,
        Self::DesignReview,
        Self::Build,
        Self::Verify,
        Self::Deliver,
        Self::Watch,
    ];
}

/// A ticket's priority, as the tracker reports it. `Ord` sorts the most urgent
/// first; a ticket without a priority comes last.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    Urgent,
    High,
    Medium,
    Low,
    Unset,
}

/// What a run executes: a stage's role, or one of the runner's helper roles.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Intake,
    Resolver,
    AnswerCheck,
    Design,
    DesignReview,
    Build,
    Verify,
    Rebase,
}

impl Role {
    /// The name this role serializes as, and the stem of its prompt file
    /// under `roles/` (`Role::Build` names `roles/build.md`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Intake => "intake",
            Self::Resolver => "resolver",
            Self::AnswerCheck => "answer_check",
            Self::Design => "design",
            Self::DesignReview => "design_review",
            Self::Build => "build",
            Self::Verify => "verify",
            Self::Rebase => "rebase",
        }
    }
}

/// A model tier; the configuration maps each tier to a model per harness.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    Deep,
    Standard,
    Fast,
}

/// A harness: a vendor CLI that runs one role headless.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Harness {
    Claude,
    Codex,
}

/// A pipeline variant.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Variant {
    Trivial,
    Standard,
    Risky,
}

/// When a human approves the plan before Build.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "kebab-case")]
pub enum PlanApproval {
    Always,
    OnFork,
    Never,
}

/// How the decider's answers left one question.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AnswerClass {
    /// The answers settle the question.
    Answered,
    /// The answers settle part of the question; the rest is missing.
    Partial,
    /// No answer from the decider addresses the question.
    Unanswered,
    /// The decider answered with a question of their own.
    CounterQuestion,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stage_all_lists_every_stage_in_pipeline_order() {
        // The match has no wildcard: a new stage fails to compile here until
        // it is given its position, and the assertion then requires it in ALL.
        fn position(stage: Stage) -> usize {
            match stage {
                Stage::Intake => 0,
                Stage::Ready => 1,
                Stage::Design => 2,
                Stage::DesignReview => 3,
                Stage::Build => 4,
                Stage::Verify => 5,
                Stage::Deliver => 6,
                Stage::Watch => 7,
            }
        }
        for (index, stage) in Stage::ALL.into_iter().enumerate() {
            assert_eq!(position(stage), index, "{stage:?}");
        }
        assert!(Stage::ALL.is_sorted_by(|a, b| a < b));
    }

    #[test]
    fn role_as_str_matches_the_serde_name() {
        for role in [
            Role::Intake,
            Role::Resolver,
            Role::AnswerCheck,
            Role::Design,
            Role::DesignReview,
            Role::Build,
            Role::Verify,
            Role::Rebase,
        ] {
            let serialized = serde_json::to_value(role).expect("Role always serializes");
            assert_eq!(
                serialized,
                serde_json::Value::String(role.as_str().to_owned()),
                "{role:?}"
            );
        }
    }

    #[test]
    fn priority_sorts_the_most_urgent_first_and_unset_last() {
        let mut priorities = [
            Priority::Unset,
            Priority::Low,
            Priority::Urgent,
            Priority::Medium,
            Priority::High,
        ];
        priorities.sort();
        assert_eq!(
            priorities,
            [
                Priority::Urgent,
                Priority::High,
                Priority::Medium,
                Priority::Low,
                Priority::Unset,
            ]
        );
    }
}
