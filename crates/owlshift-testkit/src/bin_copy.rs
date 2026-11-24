//! The binaries under test, as copies of cargo's that no cargo command
//! replaces (OWL-610, OWL-611), made by the first test of a build and started
//! by every test after it.
//!
//! The testkit's tests reach them through `owlshift_testkit::binary!`, and
//! `crates/owlshift-cli/tests/cli.rs` compiles this same file through
//! `#[path]`, since the testkit depends on the command line and not the
//! other way round. This file therefore uses the standard library only and
//! names nothing of either crate, but `stand_in.rs`, which an includer
//! includes beside it as `stand_in`.
//!
//! `CARGO_BIN_EXE_<name>` is cargo's `target/debug/<name>`, which every cargo
//! command building its package in the same target directory removes and
//! writes again, on macOS even when it rebuilds nothing: cargo copies the
//! binary out of `deps/` there rather than hard-linking it (cargo issue
//! 10060), so it never finds it already in place. A process started in
//! between fails with "No such file or directory", and so may the binary's
//! runs of itself, which start the file it was started from (`start`'s
//! daemon, the copy the daemon pins, `doctor`'s sentinel, the daemon bench).
//! The copy is made as cargo makes its own, while the test holds cargo's lock
//! on that directory shared: cargo replaces the binary only while it holds
//! that lock exclusively. Its folder is named by the binary's size and
//! modification time, which cargo's copy keeps, so a build that compiles
//! nothing finds the copy in place. Every test process holds a shared lock on
//! the folder of the copy it starts for as long as it runs, and a new copy
//! removes the copies of other builds of the same binary that no process
//! holds. Cargo's lock, held from before the binary is looked at until that
//! removal, keeps every process that looks at it meanwhile on the same build.
//! Each binary has a folder of copies of its own: cargo's lock keeps the
//! processes on one build of each binary, not on one folder for all of them,
//! so a copy of one binary removing another binary's folder could remove it
//! between its creation and its lock.
//!
//! Under nextest every test is a process of its own and makes this check
//! once; only the test that finds no started copy takes the lock that makes
//! it, so the others never wait on one another once the copy is in place.
//!
//! The test that makes the copy also starts it once, to its end, before it
//! writes the folder's `started` and lets the others have it (OWL-614): on
//! macOS the first start of a new executable waits for the system's scan of
//! it, one file at a time, which took up to 37 s under load, while a step
//! child's first start was counted against a test's passes. Started with
//! `--version`, which every binary of the testkit and the CLI answers or
//! refuses before it reads, writes or locks anything, with an empty
//! environment. It is started where it stays: a debug binary started once,
//! then renamed, took 0.38 s again at its next start (2026-11-20, macOS
//! 26.6.2). See "First starts in tests" in `docs/design/build-plan.md`.
//!
//! Checked on 2026-11-20 with cargo 1.98.1 on macOS. A build that compiled
//! nothing gave `target/debug/owlshift` a new inode, the path missing for 25
//! to 186 µs each time, with the size and modification time of
//! `deps/owlshift-<hash>` to the nanosecond. Cargo held `.cargo-artifact-lock`
//! exclusively from its start until after it replaced the binary; a shared
//! lock held on it made a build wait ("Blocking waiting for file lock on
//! artifact directory"), and `.cargo-lock` no longer did. Neither `cargo
//! test` nor nextest holds it while the tests run. Starting the binary in a
//! loop while cargo rebuilt nothing every 2.5 seconds, 14 of 140,047 starts of
//! `target/debug/owlshift` failed so, and none of 254,785 of
//! `deps/owlshift-<hash>`. A fresh hard link of the binary, instead of a
//! copy, was killed by the system 13 times in 12,576 starts.

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::UNIX_EPOCH;

/// The copy of `cargo`, the binary `CARGO_BIN_EXE_<name>` names, in the
/// folder of copies of that binary under `target_tmpdir`, the test's
/// `CARGO_TARGET_TMPDIR`; and its folder's lock, which the caller holds for
/// as long as it may start the copy.
pub fn make(cargo: &str, target_tmpdir: &str) -> (PathBuf, fs::File) {
    let cargo = Path::new(cargo);
    let name = cargo.file_name().unwrap();
    let lock = cargo.parent().unwrap().join(".cargo-artifact-lock");
    // Released when dropped, once the copies are in order.
    let _cargo = Unlocking(
        fs::File::open(&lock)
            .and_then(|file| file.lock_shared().map(|()| file))
            .unwrap_or_else(|error| panic!("{}: {error}, cargo's lock (OWL-610)", lock.display())),
    );
    let built = fs::metadata(cargo).unwrap();
    let modified = built.modified().unwrap();
    let since = modified.duration_since(UNIX_EPOCH).unwrap();
    // In the target directory: on its file system, so that the copy is a
    // clone where cargo's is one (APFS), and gone with `cargo clean`.
    let copies = Path::new(target_tmpdir).join("bin-copies").join(name);
    let folder = copies.join(format!("{}-{}", built.len(), since.as_nanos()));
    let copy = folder.join(name);
    let started = folder.join("started");
    fs::create_dir_all(&folder).unwrap();
    let in_use = lock_file(&folder.join("in-use"));
    in_use.lock_shared().unwrap();
    if !(started.exists() && copy.exists()) {
        // One test copies and starts the copy, the others wait for it: they
        // all start one file, which macOS scans once, at its first start.
        // Released when dropped.
        let copying = lock_file(&folder.join("copying"));
        copying.lock().unwrap();
        let _copying = Unlocking(copying);
        if !copy.exists() {
            // A copy made anew has not been started.
            match fs::remove_file(&started) {
                Err(error) if error.kind() != ErrorKind::NotFound => {
                    panic!("{}: {error}", started.display())
                }
                _ => {}
            }
            let staged = folder.join(format!("staged-{}", std::process::id()));
            crate::stand_in::copy_executable(cargo, &staged);
            fs::rename(&staged, &copy).unwrap();
            for entry in fs::read_dir(&copies).unwrap().flatten() {
                if entry.path() != folder && unused(&entry.path()) {
                    let _ = fs::remove_dir_all(entry.path());
                }
            }
        }
        if !started.exists() {
            start_once(&copy, &folder);
            fs::write(&started, "").unwrap();
        }
    }
    (copy, in_use)
}

/// Starts `copy` once with `--version` in `folder`, to its end (OWL-614).
/// The environment is emptied, so that no variable a test process has set
/// makes a binary do more; Windows keeps `SystemRoot`, which a program may
/// need there to start. Not through the platform's `run_command`: this file
/// uses the standard library only.
fn start_once(copy: &Path, folder: &Path) {
    let mut command = Command::new(copy);
    command.arg("--version").env_clear().current_dir(folder);
    if let Some(root) = std::env::var_os("SystemRoot") {
        command.env("SystemRoot", root);
    }
    crate::stand_in::start_once(&mut command);
}

/// A file whose lock this process holds, let go explicitly when dropped,
/// before its descriptor closes (OWL-646): a child forked meanwhile by
/// another test of this process keeps a copy of the descriptor until it
/// starts its own program, and with it a lock released by the close alone.
/// The runner's `project::FileLock` does the same; this file uses the
/// standard library only.
struct Unlocking(fs::File);

impl Drop for Unlocking {
    fn drop(&mut self) {
        // A failed unlock leaves the release to the close that follows.
        let _ = self.0.unlock();
    }
}

/// The lock file `path`, opened for reading and writing: Windows locks
/// nothing through a handle with neither.
fn lock_file(path: &Path) -> fs::File {
    fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .unwrap()
}

/// Whether no test process holds the copy in `folder`. The lock taken to
/// tell is released before this returns, since Windows removes no file a
/// handle holds open.
fn unused(folder: &Path) -> bool {
    match fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(folder.join("in-use"))
    {
        Ok(file) => file.try_lock().map(|()| Unlocking(file)).is_ok(),
        Err(error) => error.kind() == ErrorKind::NotFound,
    }
}
