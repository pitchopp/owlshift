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
