//! The JSON Schemas generated from the contract types.

use schemars::{JsonSchema, Schema, schema_for};

use crate::brief::Brief;
use crate::comment::Footer;
use crate::config::{PersonalConfig, ProjectConfig};
use crate::drain::{TicketIntake, TicketProposals};
use crate::event::Event;
use crate::refs::{
    Claim, PersistedState, TicketCost, TicketDelivery, TicketHeld, TicketQuestions, TicketSession,
};
use crate::result::RunResult;
use crate::review::{TicketPlan, TicketReview};
use crate::step::{StepEnvelope, StepOutcome, TicketCarry};
use crate::usage::HarnessUsage;

/// Every published schema, by file stem: `schemas/<stem>.schema.json`.
pub fn all() -> Vec<(&'static str, Schema)> {
    vec![
        ("brief", schema::<Brief>()),
        ("result", schema::<RunResult>()),
        ("project-config", schema::<ProjectConfig>()),
        ("personal-config", schema::<PersonalConfig>()),
        ("event", schema::<Event>()),
        ("claim", schema::<Claim>()),
        ("ticket-state", schema::<PersistedState>()),
        ("ticket-questions", schema::<TicketQuestions>()),
        ("ticket-delivery", schema::<TicketDelivery>()),
        ("ticket-intake", schema::<TicketIntake>()),
        ("ticket-proposals", schema::<TicketProposals>()),
        ("ticket-plan", schema::<TicketPlan>()),
        ("ticket-review", schema::<TicketReview>()),
        ("ticket-cost", schema::<TicketCost>()),
        ("ticket-session", schema::<TicketSession>()),
        ("ticket-held", schema::<TicketHeld>()),
        ("comment-footer", schema::<Footer>()),
        ("step-envelope", schema::<StepEnvelope>()),
        ("step-outcome", schema::<StepOutcome>()),
        ("ticket-carry", schema::<TicketCarry>()),
        ("harness-usage", schema::<HarnessUsage>()),
    ]
}

/// Renders a schema as the committed file: pretty JSON, final newline.
pub fn render(schema: &Schema) -> String {
    let mut out = serde_json::to_string_pretty(schema).expect("schemas serialize to JSON");
    out.push('\n');
    out
}

fn schema<T: JsonSchema>() -> Schema {
    schema_for!(T)
}
