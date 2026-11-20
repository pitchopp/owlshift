//! Owlshift's test bench: a ticket played end to end with no model, no
//! network and no tracker account.
//!
//! - [`bin_copy`] and [`binary!`]: the bench's binaries, started from a copy
//!   no cargo command replaces (OWL-611).
//! - [`capability`]: probes that skip a test where root cannot honour what
//!   it relies on (OWL-555).
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
//! - [`issues`]: GitHub's Issues API held in memory, for the GitHub Issues
//!   tracker on the bench (OWL-536).
//! - [`scenario`]: scenario files, their runner, and the stand-in driver that
//!   runs roles through the executor and plays the writer until it exists.
//! - [`stand_in`]: executables a test makes, started once before anything
//!   times them (OWL-614).
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

pub mod bin_copy;
pub mod capability;
pub mod fixture_gate;
pub mod gh;
pub mod git;
pub mod harness;
pub mod issues;
pub mod release;
pub mod reply;
pub mod scenario;
pub mod stand_in;
pub mod step_child;
pub mod system;
pub mod ui_bench;
pub mod wait;

/// The binary `name` of this package, as a `&'static Path` to the copy of it
/// that [`bin_copy`] makes, for an integration test: `CARGO_BIN_EXE_<name>`
/// and `CARGO_TARGET_TMPDIR` are read where the macro is called, the only
/// place cargo sets them. The process holds the copy's folder from the first
/// call on (OWL-611).
#[macro_export]
macro_rules! binary {
    ($name:literal) => {{
        static BINARY: ::std::sync::OnceLock<(::std::path::PathBuf, ::std::fs::File)> =
            ::std::sync::OnceLock::new();
        BINARY
            .get_or_init(|| {
                $crate::bin_copy::make(
                    env!(concat!("CARGO_BIN_EXE_", $name)),
                    env!("CARGO_TARGET_TMPDIR"),
                )
            })
            .0
            .as_path()
    }};
}
