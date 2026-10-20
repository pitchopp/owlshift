//! A test run alone in a fresh copy of the test binary (OWL-414).
//!
//! Under `cargo test`, the tests of one binary share a process and its
//! threads, and a child spawned by one thread holds the locks another thread
//! opened until it execs: a lock a test releases and takes again, or that the
//! code under test takes again, can then look held. A test that does so runs
//! its body as an ignored helper, alone in a fresh process where nothing else
//! spawns (build plan, check (e), "Rechecked on 2026-10-20"). Under nextest,
//! every test is a process of its own already; the copy costs one process.

use std::process::Command;

/// Runs the ignored test `helper`, named by its path in the crate
/// (`project::tests::helper_…`), alone in a fresh copy of this test binary
/// with one test thread, and fails with its output unless it ran and passed.
pub(crate) fn run_alone(helper: &str) {
    let output = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            helper,
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("running 1 test"),
        "{output:?}"
    );
}

/// Whether this process was asked for one test by its exact name, as
/// [`run_alone`] asks: a helper returns at once otherwise, so that
/// `cargo test -- --ignored` does not run it among the other tests.
pub(crate) fn running_alone() -> bool {
    std::env::args().any(|arg| arg == "--exact")
}
