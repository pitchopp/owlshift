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

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    /// What the line before a mode says when the mode is not that of an
    /// executable a test writes: a folder's, or production's own binary.
    const NOT_A_TEST_EXECUTABLE: &str = "Not an executable a test writes (OWL-646)";

    /// The `mode(0o…)` literals of `line` with an execute bit for the group
    /// or others: an executable's, where a folder a test makes is its owner's
    /// alone (0o700, 0o500).
    fn executable_modes(line: &str) -> Vec<u32> {
        line.match_indices("mode(0o")
            .filter_map(|(at, found)| {
                let digits: String = line[at + found.len()..]
                    .chars()
                    .take_while(|c| c.is_digit(8))
                    .collect();
                u32::from_str_radix(&digits, 8).ok()
            })
            .filter(|mode| mode & 0o011 != 0)
            .collect()
    }

    /// The Rust sources under `dir`, a build's output and packages left out.
    fn sources(dir: &Path, found: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                if !path.ends_with("target") && !path.ends_with("node_modules") {
                    sources(&path, found);
                }
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                found.push(path);
            }
        }
    }

    /// OWL-646: every executable a test writes goes through
    /// `stand_in::write_executable` or `copy_executable`, which write it
    /// through a child process, so no descriptor of the test's process holds
    /// it open for writing when another test's child forks. An executable
    /// mode written anywhere else in the workspace's sources is refused,
    /// unless the line before says why it is not one. A copy of an
    /// executable keeps its source's mode and names none: copies are
    /// audited, not guarded (`docs/design/records/OWL-646.md`).
    #[test]
    fn every_executable_a_test_writes_goes_through_stand_in() {
        let call = |mode: &str| format!("fs::Permissions::from_mode(0o{mode})");
        assert_eq!(executable_modes(&call("755")), [0o755]);
        assert_eq!(executable_modes(&call("711")), [0o711]);
        assert!(executable_modes(&call("700")).is_empty());
        assert!(executable_modes("permissions().mode() & 0o111 != 0").is_empty());

        let crates = fs::canonicalize(Path::new(env!("CARGO_MANIFEST_DIR")).join("..")).unwrap();
        let helper = crates.join("owlshift-testkit/src/stand_in.rs");
        let mut files = Vec::new();
        sources(&crates, &mut files);
        let mut offending = Vec::new();
        for file in files.iter().filter(|file| **file != helper) {
            let text = fs::read_to_string(file).unwrap();
            let mut before = "";
            for (n, line) in text.lines().enumerate() {
                if !line.trim_start().starts_with("//")
                    && !executable_modes(line).is_empty()
                    && !before.contains(NOT_A_TEST_EXECUTABLE)
                {
                    offending.push(format!("{}:{}", file.display(), n + 1));
                }
                before = line;
            }
        }
        assert!(
            offending.is_empty(),
            "write these through stand_in::write_executable or copy_executable, \
             or say on the line before why they are not executables: {offending:?}"
        );
        assert!(files.contains(&helper), "the sources were not found");
    }
}
