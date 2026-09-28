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
//! | Resource | [`resource`] |
//! | Scheduler | [`schedule`] |
//! | Policy floor | [`floor`] |

pub mod floor;
pub mod gate;
pub mod pipeline;
pub mod resource;
pub mod schedule;
pub mod state;
pub mod ticket;
pub mod vocab;
