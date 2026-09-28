//! The JSON Schemas generated from the contract types.

use schemars::{JsonSchema, Schema, schema_for};

use crate::brief::Brief;
use crate::comment::Footer;
use crate::config::{PersonalConfig, ProjectConfig};
use crate::event::Event;
use crate::refs::{Claim, TicketState};
use crate::result::RunResult;

/// Every published schema, by file stem: `schemas/<stem>.schema.json`.
pub fn all() -> Vec<(&'static str, Schema)> {
    vec![
        ("brief", schema::<Brief>()),
        ("result", schema::<RunResult>()),
        ("project-config", schema::<ProjectConfig>()),
        ("personal-config", schema::<PersonalConfig>()),
        ("event", schema::<Event>()),
        ("claim", schema::<Claim>()),
        ("ticket-state", schema::<TicketState>()),
        ("comment-footer", schema::<Footer>()),
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
