//! The drain's ready snapshot (OWL-468, architecture section 5, "The local
//! web UI", "The ready snapshot"): what one pass of `owlshift watch
//! --drain` read of the ready column and made of each ticket,
//! [`ReadySnapshot`] in `<project data>/ready.json`, replaced whole at each
//! pass that read the column. The local web UI reads it for the tickets not
//! yet started, their order and their blockers, with no tracker call.
//!
//! A wait is the shape `owlshift status --waits --json` prints
//! ([`Wait`]), worded by the same functions, its `since` always `null`:
//! the snapshot's [`ReadySnapshot::at`] is when it was so.

use jiff::Timestamp;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::Priority;
use crate::format::{self, ContractError, Format, READY_FORMAT};
use crate::ids::TicketId;
use crate::waits::Wait;

/// What one drain pass read of the ready column.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(title = "Owlshift ready snapshot")]
pub struct ReadySnapshot {
    pub format: Format<READY_FORMAT>,
    /// When it was written, at the end of the pass's drain: a step watch
    /// ran in its own process during the pass is behind it.
    pub at: Timestamp,
    /// The watch's interval between two idle passes, in milliseconds: a
    /// snapshot older than two of them is stale.
    pub interval_ms: u64,
    /// The tickets of the column, in the order the drain takes them:
    /// priority, then the first created, then the identifier.
    pub tickets: Vec<ReadyTicket>,
}

/// A ticket of the ready column.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadyTicket {
    pub ticket: TicketId,
    /// Its title, as the tracker shows it: for display only, escaped.
    pub title: String,
    pub priority: Priority,
    /// What blocks it, as the drain weighed it: its relations, and the
    /// "Blocked by" lines of a ticket whose intake holds its current text.
    pub blockers: Vec<ReadyBlocker>,
    /// What the drain made of it.
    pub drain: Outcome,
}

/// One blocker of a ticket.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadyBlocker {
    /// The blocker, as the relation or the line names it: a "Blocked by"
    /// line may name what is no identifier.
    pub ticket: String,
    pub state: BlockerState,
    /// Whether its ticket ref records its pull request merged, which
    /// releases the ticket as `completed` does.
    pub merged: bool,
}

/// What the tracker shows of a blocker.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BlockerState {
    /// Neither completed nor canceled; or a read that failed.
    Open,
    Completed,
    /// Canceled or a duplicate: holds the ticket until a person acts.
    Canceled,
    /// The tracker knows no ticket by that identifier.
    Unknown,
}

/// What the drain made of a ticket in its pass.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Outcome {
    /// Its intake ran or started in this pass.
    TakenIn,
    /// A step of it runs as the watch's child: the pass left it.
    Running,
    /// Its dispatch ran or started in this pass.
    Dispatched,
    /// Admitted, and not dispatched: why. The reason is the drain's own
    /// (`owlshift why`'s keys), or one only a pass tells: `held` (its last
    /// step left its ticket ref as it was), `next_pass` (chosen, it did not
    /// start in this pass), `ref_unread` (its ticket ref cannot be read),
    /// `refs_unread` (another ticket's ref cannot be read, which stops every
    /// dispatch). The set is open: a reader shows [`Wait::text`].
    Waits { wait: Wait },
    /// Not admitted: its gesture is not from an account `admitters` lists
    /// (`not_admitted`), or it carries the exclusion label (`excluded`).
    NotAdmitted { wait: Wait },
}

impl ReadySnapshot {
    const CONTRACT: &str = "ready snapshot";

    pub fn parse(input: &str) -> Result<Self, ContractError> {
        format::parse_json(Self::CONTRACT, READY_FORMAT, input)
    }

    /// Pretty JSON, with a final newline.
    pub fn render(&self) -> String {
        format::render_json(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::waits::Behind;

    fn wait(reason: &str, text: &str) -> Wait {
        Wait {
            reason: reason.to_owned(),
            since: None,
            behind: Vec::new(),
            resource: None,
            harness: None,
            until: None,
            text: text.to_owned(),
        }
    }

    #[test]
    fn a_snapshot_reads_back_and_a_newer_one_is_refused() {
        let id = |id: &str| TicketId::new(id).unwrap();
        let ticket = |n: &str, drain: Outcome| ReadyTicket {
            ticket: id(n),
            title: format!("Ticket {n}"),
            priority: Priority::High,
            blockers: Vec::new(),
            drain,
        };
        let mut blocked = wait("blocked_by", "blocked by DEMO-1");
        blocked.behind = vec![Behind::Ticket {
            id: id("DEMO-1"),
            person: false,
        }];
        let mut waiting = ticket("DEMO-2", Outcome::Waits { wait: blocked });
        waiting.blockers = vec![ReadyBlocker {
            ticket: "DEMO-1".to_owned(),
            state: BlockerState::Open,
            merged: false,
        }];
        let snapshot = ReadySnapshot {
            format: Format,
            at: "2026-10-31T10:00:00Z".parse().unwrap(),
            interval_ms: 60_000,
            tickets: vec![
                ticket("DEMO-1", Outcome::Dispatched),
                waiting,
                ticket(
                    "DEMO-3",
                    Outcome::NotAdmitted {
                        wait: wait("excluded", "it carries the exclusion label"),
                    },
                ),
                ticket("DEMO-4", Outcome::TakenIn),
                ticket("DEMO-5", Outcome::Running),
            ],
        };
        let text = snapshot.render();
        assert!(text.contains("\"kind\": \"waits\""), "{text}");
        assert!(text.contains("\"kind\": \"taken_in\""), "{text}");
        assert!(text.contains("\"state\": \"open\""), "{text}");
        assert_eq!(ReadySnapshot::parse(&text).unwrap(), snapshot);
        let newer = text.replace("\"format\": 1", "\"format\": 2");
        assert!(matches!(
            ReadySnapshot::parse(&newer),
            Err(ContractError::NewerFormat { found: 2, .. })
        ));
    }
}
