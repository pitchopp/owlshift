//! The Owlshift contracts: the brief, `result.json`, project and personal
//! configuration, events, the git ref layout and marked comments, with their
//! format versions and the JSON Schemas generated from these types.
//!
//! Serde is the authoritative validator: every type rejects unknown fields,
//! and each contract's `parse` adds the rules a schema cannot carry. Parsing
//! and rendering work on strings; this crate reads and writes no file.

pub mod brief;
pub mod comment;
pub mod config;
pub mod event;
pub mod format;
pub mod ids;
pub mod refs;
pub mod result;
pub mod schema;

pub use format::ContractError;
pub use owlshift_core::vocab::{Harness, PlanApproval, Role, Stage, Tier, Variant};
