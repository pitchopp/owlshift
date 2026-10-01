//! The real gh, started once per test binary before its first credential
//! probe.
//!
//! The executor's credential probes (OWL-22) run `gh auth token` within
//! `PROBE_TIMEOUT`, 10 s, and a warm gh answers in about 0.1 s. The very
//! first start on a machine reads gh's 42 MB binary from disk: on fresh
//! Windows runners that took from 0.8 to 14 s over ten machines, one of them
//! past the probe's 10 s (OWL-56, measured on 2026-09-29, runs 36553384681
//! and 36553676102). So every test binary whose probes may find gh calls
//! [`warm_up`] first, and stays safe when it runs alone.
//!
//! It lives in the test bench, and the tests that need it with it: the
//! bench depends on the runner, so a runner test could reach it only through
//! a dependency cycle.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;
use std::time::Duration;

use owlshift_platform::process::{OUTPUT_CAP, find_executable_in, run_command};

/// How long gh's first start may take.
const FIRST_START: Duration = Duration::from_secs(120);

/// The gh a process run with `env` finds: the one on its `PATH`.
pub fn on_path(env: &[(OsString, OsString)]) -> Option<PathBuf> {
    let path = env
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("PATH"))
        .map(|(_, value)| value.clone())
        .unwrap_or_default();
    find_executable_in("gh", path)
}

/// Starts the gh found with `env` once for the whole test binary, so that
/// the probes time gh's answer and not its first start (`FIRST_START`).
/// Tests that arrive meanwhile wait here, off their probes' clocks. Without
/// gh there is nothing to start.
///
/// Pass the environment the probes will run with. The first call decides
/// which gh is started; the callers keep the process's own `PATH`, so they
/// all find the same one.
///
/// # Panics
///
/// When gh does not start and answer within `FIRST_START`.
pub fn warm_up(env: &[(OsString, OsString)]) {
    static STARTED: OnceLock<Result<(), String>> = OnceLock::new();
    let started = STARTED.get_or_init(|| {
        let Some(gh) = on_path(env) else {
            return Ok(());
        };
        let mut command = Command::new(&gh);
        command
            .arg("--version")
            .env_clear()
            .envs(env.iter().map(|(n, v)| (n, v)));
        match run_command(&mut command, None, FIRST_START, OUTPUT_CAP) {
            Ok(captured) if captured.success() => Ok(()),
            outcome => Err(format!("{} --version: {outcome:?}", gh.display())),
        }
    });
    if let Err(error) = started {
        panic!("gh did not start: {error}");
    }
}
