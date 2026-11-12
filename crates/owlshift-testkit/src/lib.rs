//! Owlshift's test bench: a ticket played end to end with no model, no
//! network and no tracker account.
//!
//! - [`fixture_gate`]: whether a fixture project's test runs, skips or fails
//!   (OWL-330).
//! - [`gh`]: the real gh, started once before the credential probes time it.
//! - [`git`]: git with none of the host's configuration, and a project
//!   seeded into a local bare remote.
//! - [`release`]: the key the bench trusts for `owlshift self-update`
//!   (OWL-541).
//! - [`reply`]: what the fake harness (`owlshift-fake-harness`) does in one
//!   run.
//! - [`harness`]: the fake harness as the executor drives it.
//! - [`scenario`]: scenario files, their runner, and the stand-in driver that
//!   runs roles through the executor and plays the writer until it exists.
//! - [`step_child`]: a project whose steps run in a real child process, and
//!   `owlshift-step-child`, the child (OWL-399); the same project served, a
//!   ticket parked at Build, for `owlshift-cli-bench`, the command line's own
//!   entry with a keychain held in memory (OWL-522).
//! - [`system`]: a host that reaches nothing of the machine, for `owlshift
//!   doctor` on the bench (OWL-472).
//! - [`ui_bench`]: what a check of the web UI in a browser changes in a bench
//!   data directory (OWL-508).
//! - [`wait`]: how a test waits for a state, as long as load could make it
//!   take (OWL-493).
//!
//! This crate is test only: it is never published and no shipped crate
//! depends on it. See "The test bench" in `docs/design/build-plan.md`.

pub mod fixture_gate;
pub mod gh;
pub mod git;
pub mod harness;
pub mod release;
pub mod reply;
pub mod scenario;
pub mod step_child;
pub mod system;
pub mod ui_bench;
pub mod wait;
