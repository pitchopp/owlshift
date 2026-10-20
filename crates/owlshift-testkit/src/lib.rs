//! Owlshift's test bench: a ticket played end to end with no model, no
//! network and no tracker account.
//!
//! - [`fixture_gate`]: whether a fixture project's test runs, skips or fails
//!   (OWL-330).
//! - [`gh`]: the real gh, started once before the credential probes time it.
//! - [`git`]: git with none of the host's configuration, and a project
//!   seeded into a local bare remote.
//! - [`reply`]: what the fake harness (`owlshift-fake-harness`) does in one
//!   run.
//! - [`harness`]: the fake harness as the executor drives it.
//! - [`scenario`]: scenario files, their runner, and the stand-in driver that
//!   runs roles through the executor and plays the writer until it exists.
//! - [`step_child`]: a project whose steps run in a real child process, and
//!   `owlshift-step-child`, the child (OWL-399).
//!
//! This crate is test only: it is never published and no shipped crate
//! depends on it. See "The test bench" in `docs/design/build-plan.md`.

pub mod fixture_gate;
pub mod gh;
pub mod git;
pub mod harness;
pub mod reply;
pub mod scenario;
pub mod step_child;
