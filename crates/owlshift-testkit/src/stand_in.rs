//! Executables a test makes, started once to their end before anything
//! times them (OWL-614): a stand-in such as a fake `git`, by `write` (Unix),
//! and each copy of a binary under test, by `bin_copy` through
//! [`start_once`].
//!
//! macOS holds the first start of a new executable until the system has
//! scanned it, and scans one file at a time, so the suite's own new files (a
//! daemon's pinned copy, a build's copies, other tests' stand-ins) make the
//! wait. Measured on 2026-11-20, under three `cargo nextest run` at once: a
//! script's first start took up to 34.8 s, its second 8 to 21 ms, and the
//! same script read by `/bin/sh` 8 to 25 ms. A stand-in handed at once to
//! `owlshift doctor`'s 10 s probe, or to `Git::require_minimum_version`'s,
//! was timed in that queue, not on its answer. See "First starts in tests"
//! under "The test bench" in `docs/design/build-plan.md`.
//!
//! The standard library only: the CLI's and the runner's tests include this
//! file with `#[path]`, as they do `bin_copy.rs`.

use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// How long a first start may take: as long as the gh a test binary starts
/// first (`gh::warm_up`), far past the longest scan measured.
const FIRST_START: Duration = Duration::from_secs(120);

/// Writes `script` to `path` as an executable stand-in, then starts it once
/// with `start_with` ([`start_once`], in `path`'s folder). Use it, rather than
/// a write and a mode of its own, for every stand-in a test hands to a
/// bound: what times the stand-in then times its answer.
///
/// `start_with` are arguments the stand-in answers at once and with no side
/// effect, such as `--version` for a fake `git`: what it returns is not
/// looked at.
///
/// # Panics
///
/// When the file cannot be written or started, or the start outlives
/// `FIRST_START`.
#[cfg(unix)]
pub fn write(path: &Path, script: &str, start_with: &[&str]) {
    use std::os::unix::fs::PermissionsExt;

    std::fs::write(path, script).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    let folder = path.parent().expect("a stand-in in a folder");
    start_once(Command::new(path).args(start_with).current_dir(folder));
}

/// Starts `command` with no input and its output dropped, and waits for its
/// end, `FIRST_START` at most, whatever it returns: on macOS the first start
/// of a new executable waits for the system's scan of it, and the starts
/// after it do not.
///
/// # Panics
///
/// When it cannot be started, or has not ended within `FIRST_START`, after
/// killing it.
pub fn start_once(command: &mut Command) {
    let program = command.get_program().to_owned();
    let named = || Path::new(&program).display().to_string();
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap_or_else(|error| panic!("{}: {error}, at its first start (OWL-614)", named()));
    let deadline = Instant::now() + FIRST_START;
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "{} did not end within {FIRST_START:?} of its first start (OWL-614)",
                named()
            );
        }
        thread::sleep(Duration::from_millis(20));
    }
}
