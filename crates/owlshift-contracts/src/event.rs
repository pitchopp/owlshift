//! Events: the one stream behind logs, `why`, the UI and usage reports.

use jiff::Timestamp;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::format::{self, ContractError, EVENT_FORMAT, Format};
use crate::ids::TicketId;

const CONTRACT: &str = "event";

/// One recorded event.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(title = "Owlshift event")]
pub struct Event {
    pub format: Format<EVENT_FORMAT>,
    pub at: Timestamp,
    /// The project's name.
    pub project: String,
    /// The ticket concerned; absent for events about no single ticket, such as
    /// a scan.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ticket: Option<TicketId>,
    /// The run concerned; absent for events outside a run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,
    pub kind: EventKind,
    /// Kind-specific details.
    pub data: serde_json::Map<String, serde_json::Value>,
}

/// What happened.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    /// The tracker and the forge were polled.
    Scan,
    /// The scheduler or the policy decided something.
    Decision,
    /// A ticket was claimed and a run dispatched.
    Dispatch,
    RunStarted,
    RunEnded,
    /// Usage measured for a run, or a usage limit reported by a harness.
    Usage,
    /// A gate opened or closed.
    Gate,
    /// The writer wrote to the tracker or the forge.
    TrackerWrite,
    TicketAdmitted,
    AnswerPosted,
    PrMerged,
    CheckFailed,
}

impl Event {
    /// Parses an event.
    pub fn parse(input: &str) -> Result<Self, ContractError> {
        format::parse_json(CONTRACT, EVENT_FORMAT, input)
    }

    /// Renders an event as pretty JSON.
    pub fn render(&self) -> String {
        format::render_json(self)
    }
}
