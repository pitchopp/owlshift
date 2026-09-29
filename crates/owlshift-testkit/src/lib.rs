//! Owlshift's test bench: a ticket played end to end with no model, no
//! network and no tracker account.
//!
//! - [`gh`]: the real gh, started once before the credential probes time it.
//! - [`git`]: git with none of the host's configuration, and a project
//!   seeded into a local bare remote.
//! - [`reply`]: what the fake harness (`owlshift-fake-harness`) does in one
//!   run.
//! - [`harness`]: the fake harness as the executor drives it.
//! - [`scenario`]: scenario files, their runner, and the stand-in driver that
//!   runs roles through the executor and plays the writer until it exists.
//!
//! This crate is test only: it is never published and no shipped crate
//! depends on it. See "The test bench" in `docs/design/build-plan.md`.

pub mod gh;
pub mod git;
pub mod harness;
pub mod reply;
pub mod scenario;
