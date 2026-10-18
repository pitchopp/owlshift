//! The Owlshift core: ticket, pipeline, stage, gate and resource; the state
//! machine, the scheduler and the policy.
//!
//! Everything here is a pure function. This crate performs no I/O: no files,
//! network, processes, environment, standard streams or clock. Callers pass in
//! what the core needs to know, including the current time. The rule is
//! enforced by `clippy.toml` in this crate's directory.
//!
//! Where each concept of the design (architecture, section 3) lives:
//!
//! | Concept | Module |
//! | --- | --- |
//! | Ticket | [`ticket`] |
//! | Pipeline, stage | [`pipeline`], with the stages in [`vocab`] |
//! | Ticket state machine | [`state`] |
//! | Gate | [`gate`]; an open gate is [`state::Status::NeedsInput`] |
//! | Decider, who answers a ticket's gates | [`decider`] |
//! | When the decider's reply counts: the quiet window, `go` | [`reply`] |
//! | Resource | [`resource`] |
//! | Scheduler | [`schedule`]; dependencies to relations, [`dependency`] |
//! | Policy floor | [`floor`]; what an agent process inherits, [`agent_env`] |
//! | A follow-up proposal's title, for its fingerprint | [`proposal`] |
//! | A review pass settled: each finding's fate, where the ticket goes | [`review`] |
//! | Who reviews, per variant, and on which model | [`roster`] |
//! | Per-ticket routing: harness, tier and effort of each run | [`routing`] |

pub mod agent_env;
pub mod decider;
pub mod dependency;
pub mod floor;
pub mod follow;
pub mod gate;
pub mod pipeline;
pub mod proposal;
pub mod reply;
pub mod resource;
pub mod review;
pub mod roster;
pub mod routing;
pub mod schedule;
pub mod state;
pub mod ticket;
pub mod vocab;
