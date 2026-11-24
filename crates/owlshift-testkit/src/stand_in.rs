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
//! Every executable a test writes or copies goes through [`write_executable`]
//! or [`copy_executable`] (OWL-646), which write it through a child process:
//! under `cargo test` the tests of a binary share its process, a child that
//! another test forks holds a copy of every descriptor open at that moment
//! until it starts its own program, and Linux refuses to run a file that a
//! descriptor still holds open for writing (`ETXTBSY`). Written by a child,
//! the file never has such a descriptor in the test process. Close-on-exec,
//! which the standard library sets on every descriptor, does not help: the
//! copy lives from the fork to the exec. A guard in this crate's tests
//! refuses an executable mode written anywhere else.
//!
//! The standard library only: the CLI's and the runner's tests, and the
//! platform's and the runner's unit tests, include this file with `#[path]`,
//! as the CLI's do `bin_copy.rs`.

use std::io;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// How long a first start may take: as long as the gh a test binary starts
/// first (`gh::warm_up`), far past the longest scan measured.
const FIRST_START: Duration = Duration::from_secs(120);

/// How long a new executable may be refused as busy at its first start
/// ([`start_once`]): the moment a process forked by another thread holds a
/// descriptor that wrote it, until it starts its own program.
const BUSY: Duration = Duration::from_secs(10);

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
    write_executable(path, script);
    let folder = path.parent().expect("a stand-in in a folder");
    start_once(Command::new(path).args(start_with).current_dir(folder));
}

/// Writes `contents` to `path`, replacing what is there, as an executable:
/// mode 0o755 on Unix, where a child, `cat`, writes the bytes, so that no
/// descriptor of this process ever holds the file open for writing (see the
/// module's comment). Elsewhere a plain write.
///
/// # Panics
///
/// When the file cannot be written: the child's failure is reported with
/// the path, and the mode is not set.
pub fn write_executable(path: &Path, contents: impl AsRef<[u8]>) {
    #[cfg(unix)]
    {
        use std::io::Write as _;

        let mut child = Command::new("/bin/sh")
            .args(["-c", "exec cat > \"$0\""])
            .arg(path)
            .stdin(Stdio::piped())
            .spawn()
            .unwrap_or_else(|error| panic!("writing {}: {error}", path.display()));
        let written = child
            .stdin
            .take()
            .expect("a piped input")
            .write_all(contents.as_ref());
        let ended = child.wait();
        match (written, ended) {
            (Ok(()), Ok(status)) if status.success() => {}
            (written, ended) => panic!("writing {}: {written:?}, {ended:?}", path.display()),
        }
        make_executable(path);
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, contents)
            .unwrap_or_else(|error| panic!("writing {}: {error}", path.display()));
    }
}

/// Copies the file `from` to `to`, replacing what is there, as an
/// executable: on Unix through a child, `cp`, then mode 0o755, as
/// [`write_executable`] writes one. Elsewhere a plain copy.
///
/// # Panics
///
/// When the copy fails: the child's failure is reported with both paths,
/// and the mode is not set.
pub fn copy_executable(from: &Path, to: &Path) {
    #[cfg(unix)]
    {
        let copied = Command::new("/bin/cp").arg("--").args([from, to]).status();
        if !copied.as_ref().is_ok_and(|status| status.success()) {
            panic!("copying {} to {}: {copied:?}", from.display(), to.display());
        }
        make_executable(to);
    }
    #[cfg(not(unix))]
    {
        if let Err(error) = std::fs::copy(from, to) {
            panic!("copying {} to {}: {error}", from.display(), to.display());
        }
    }
}

/// Gives the file at `path` mode 0o755, by its path: no descriptor opens it.
#[cfg(unix)]
fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .unwrap_or_else(|error| panic!("making {} executable: {error}", path.display()));
}

/// Starts `command` with no input and its output dropped, and waits for its
/// end, `FIRST_START` at most, whatever it returns: on macOS the first start
/// of a new executable waits for the system's scan of it, and the starts
/// after it do not.
///
/// A start refused because the file is busy is tried again, `BUSY` at most
/// (OWL-645). Under `cargo test` the tests of a binary share its process,
/// and a process another test forks while this file is open for writing
/// holds that descriptor until it starts its own program: Linux refuses to
/// run a file open for writing (`ETXTBSY`). Once a start went through, no
/// such descriptor is left, so the starts after it are not refused. What
/// [`write_executable`] and [`copy_executable`] write is never refused so
/// (OWL-646); an executable production code writes in the test's process,
/// such as the platform's pinned copy of a binary, can be.
///
/// # Panics
///
/// When it cannot be started, or has not ended within `FIRST_START`, after
/// killing it.
pub fn start_once(command: &mut Command) {
    let program = command.get_program().to_owned();
    let named = || Path::new(&program).display().to_string();
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let busy_until = Instant::now() + BUSY;
    let mut child = loop {
        match command.spawn() {
            Ok(child) => break child,
            Err(error)
                if error.kind() == io::ErrorKind::ExecutableFileBusy
                    && Instant::now() < busy_until =>
            {
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => panic!("{}: {error}, at its first start (OWL-614)", named()),
        }
    };
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
