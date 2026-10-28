//! What each ticket in flight waits on (OWL-459, architecture section 5,
//! "`owlshift status`"): the document `owlshift status --waits --json`
//! prints, [`StatusWaits`], the one source the local web UI's graph view
//! reads (P8). It is made from what Owlshift keeps, never stored: the
//! machine's run slots and pause, each project's ticket refs, its pause and
//! quarantine, the project file it is served on, and the latest wait the
//! event log tells for each ticket.
//!
//! Every field is always written, `null` standing for what is not known or
//! not there, so a reader sees one shape. A wait's [`Wait::reason`] is an
//! open set: a reader shows [`Wait::text`] for a key it does not know.

use jiff::Timestamp;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::Stage;
use crate::format::{self, ContractError, Format, WAITS_FORMAT};
use crate::ids::TicketId;

/// The wait graph of the projects asked about, at one time.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(title = "Owlshift status waits")]
pub struct StatusWaits {
    pub format: Format<WAITS_FORMAT>,
    /// When it was read.
    pub at: Timestamp,
    /// The pause of every project of the machine (`<data>/service/pause`),
    /// while it stands: no ticket is dispatched and no step starts.
    #[serde(default)]
    pub machine_pause: Option<Paused>,
    /// The machine's run slots (`concurrent_runs`); `null` when they cannot
    /// be probed.
    #[serde(default)]
    pub runs: Option<RunSlots>,
    /// The served projects, or the one asked about.
    pub projects: Vec<ProjectWaits>,
    /// What could not be read, in words for a person.
    #[serde(default)]
    pub notes: Vec<String>,
}

/// A pause file that stands, with who asked, when and why; each `null` when
/// the file does not read.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Paused {
    #[serde(default)]
    pub by: Option<String>,
    #[serde(default)]
    pub at: Option<Timestamp>,
    #[serde(default)]
    pub why: Option<String>,
}

/// The machine's run slots: every step's child of every project takes one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RunSlots {
    /// The slots held now.
    pub in_use: u32,
    /// `concurrent_runs`.
    pub of: u32,
}

/// One project's tickets in flight or admitted.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProjectWaits {
    /// `owner/repo`, as the served projects or the command name it.
    pub repository: String,
    /// Whether the background service serves it.
    pub served: bool,
    /// Its capacity, `policy.max_in_flight` of the project file it is
    /// served on; `null` when no such file is kept (a project not served).
    #[serde(default)]
    pub max_in_flight: Option<u8>,
    /// The tickets that count against that capacity now: those whose
    /// [`TicketWaits::holds_capacity`] is true.
    pub in_flight: u32,
    /// Its own pause, while it stands.
    #[serde(default)]
    pub paused: Option<Paused>,
    /// Whether its quarantine marker stands: every command refuses it.
    pub quarantined: bool,
    /// Its tickets, in identifier order.
    pub tickets: Vec<TicketWaits>,
}

/// One ticket in flight or admitted.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TicketWaits {
    pub ticket: TicketId,
    /// Its stage, from its ticket ref; `null` without a ref that reads.
    #[serde(default)]
    pub stage: Option<Stage>,
    pub ref_status: RefStatus,
    /// Whether it counts against the project's capacity: its ref is past
    /// Ready and before Watch, neither waiting for an answer nor parked, or
    /// cannot be read.
    pub holds_capacity: bool,
    /// The machine run slot it holds now: a step of it is under way.
    #[serde(default)]
    pub run: Option<HeldRun>,
    /// What it waits on; `null` when it waits on nothing.
    #[serde(default)]
    pub waits: Option<Wait>,
}

/// What a ticket's ref says of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RefStatus {
    /// It is worked at its stage.
    Active,
    /// It waits for its decider's answer.
    NeedsInput,
    /// It is parked: only a person's `owlshift continue` restarts it.
    Parked,
    /// It has no ticket ref: admitted, and not dispatched.
    NoRef,
    /// Its ref cannot be read.
    Unread,
}

/// A machine run slot a ticket's step holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HeldRun {
    pub slot: u32,
    /// The step's process.
    pub pid: u32,
    /// When it took the slot.
    pub since: Timestamp,
}

/// What a ticket waits on.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Wait {
    /// Why, as a key. From the event log, the reasons `owlshift why` reads:
    /// `blocked_by`, `blocker_canceled`, `blocker_unknown` (its blockers),
    /// `collision` (a resource another holds), `slot` (the project's
    /// capacity), `run_cap` (the machine's run slots), `usage_limit`,
    /// `quarantine`, `pr_cap`, `base_broken`, `in_flight_unread`,
    /// `to_watch`, `no_decider`, `no_intake`, `stale_intake`, `not_ready`.
    /// From the ticket's ref: `needs_input` (its decider's answer) and
    /// `parked`. The set is open: a reader shows [`Wait::text`] for a key
    /// it does not know.
    pub reason: String,
    /// When it was told; `null` for a wait its ref says.
    #[serde(default)]
    pub since: Option<Timestamp>,
    /// What it waits behind, as the wait named it when told: the blockers,
    /// the holder of a resource (both, when two tickets share it), or the
    /// tickets that held the project's capacity.
    #[serde(default)]
    pub behind: Vec<Behind>,
    /// The resource of a `collision`.
    #[serde(default)]
    pub resource: Option<WaitResource>,
    /// The harness of a `usage_limit`, such as `claude`.
    #[serde(default)]
    pub harness: Option<String>,
    /// The time a `usage_limit` ends, when the harness gave one.
    #[serde(default)]
    pub until: Option<Timestamp>,
    /// The wait in words for a person, as `owlshift why` says it: for
    /// display only.
    pub text: String,
}

/// What a ticket waits behind.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Behind {
    /// A ticket: one of Owlshift's, or, with `person`, a ticket a person
    /// works on, which holds no run slot of Owlshift's.
    Ticket { id: TicketId, person: bool },
    /// A pull request Owlshift does not follow.
    PullRequest { number: u64 },
}

/// A resource a ticket would take.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WaitResource {
    /// A zone or a file, as a path of the repository; empty for the whole
    /// repository.
    Zone { path: String },
    /// A named resource of `stack.resources`.
    Named { name: String },
    /// The browser.
    Browser,
}

impl StatusWaits {
    const CONTRACT: &str = "status waits";

    pub fn parse(input: &str) -> Result<Self, ContractError> {
        format::parse_json(Self::CONTRACT, WAITS_FORMAT, input)
    }

    /// Pretty JSON, with a final newline.
    pub fn render(&self) -> String {
        format::render_json(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_report_reads_back_and_a_newer_one_is_refused() {
        let at: Timestamp = "2026-10-28T10:00:00Z".parse().unwrap();
        let waits = StatusWaits {
            format: Format,
            at,
            machine_pause: None,
            runs: Some(RunSlots { in_use: 1, of: 2 }),
            projects: vec![ProjectWaits {
                repository: "acme/web".to_owned(),
                served: true,
                max_in_flight: Some(1),
                in_flight: 1,
                paused: Some(Paused {
                    by: None,
                    at: None,
                    why: None,
                }),
                quarantined: false,
                tickets: vec![TicketWaits {
                    ticket: TicketId::new("DEMO-2").unwrap(),
                    stage: Some(Stage::Build),
                    ref_status: RefStatus::Active,
                    holds_capacity: true,
                    run: None,
                    waits: Some(Wait {
                        reason: "collision".to_owned(),
                        since: Some(at),
                        behind: vec![
                            Behind::Ticket {
                                id: TicketId::new("DEMO-1").unwrap(),
                                person: false,
                            },
                            Behind::PullRequest { number: 12 },
                        ],
                        resource: Some(WaitResource::Zone {
                            path: "app/one.rs".to_owned(),
                        }),
                        harness: None,
                        until: None,
                        text: "DEMO-1 holds zone `app/one.rs`".to_owned(),
                    }),
                }],
            }],
            notes: Vec::new(),
        };
        let text = waits.render();
        assert!(text.contains("\"kind\": \"pull_request\""), "{text}");
        assert!(text.contains("\"stage\": \"build\""), "{text}");
        assert!(text.contains("\"machine_pause\": null"), "{text}");
        assert_eq!(StatusWaits::parse(&text).unwrap(), waits);
        let newer = text.replace("\"format\": 1", "\"format\": 2");
        assert!(matches!(
            StatusWaits::parse(&newer),
            Err(ContractError::NewerFormat { found: 2, .. })
        ));
    }
}
