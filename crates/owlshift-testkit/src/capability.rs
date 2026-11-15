//! Whether a test can run where it is (OWL-555).
//!
//! A few tests rely on what a cloud container does not give: a folder mode
//! that refuses a write (root bypasses it). Each probes the capability and,
//! where it is missing, skips with a stated reason, unless the environment
//! requires the fixtures (`OWLSHIFT_REQUIRE_FIXTURE_TOOLS`, set on CI's Linux
//! and macOS legs): there a failed probe panics, so CI runs the test or fails.
//! The probe of a git taking `--end-of-options` after `reset` went with
//! OWL-572, the runner no longer passing it there.

use crate::fixture_gate::REQUIRE_FIXTURE_TOOLS;

/// What a test does after probing a capability.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Gate {
    /// The capability is here.
    Run,
    /// It is missing and nothing requires it: the test returns, saying why.
    Skip(String),
    /// It is missing and the environment requires it: the test fails.
    Fail(String),
}

/// What a test does, from its probe and whether the environment requires it.
pub fn gate(probe: Result<(), String>, require: bool) -> Gate {
    match (probe, require) {
        (Ok(()), _) => Gate::Run,
        (Err(why), false) => Gate::Skip(why),
        (Err(why), true) => Gate::Fail(why),
    }
}

/// Whether the test goes on: false, after printing `skipped: <reason>`, when
/// the probe failed; a panic where the fixtures are required.
pub fn skip_unless(probe: Result<(), String>) -> bool {
    let require = std::env::var_os(REQUIRE_FIXTURE_TOOLS).is_some();
    match gate(probe, require) {
        Gate::Run => true,
        Gate::Skip(why) => {
            eprintln!("skipped: {why}");
            false
        }
        Gate::Fail(why) => panic!("{REQUIRE_FIXTURE_TOOLS} is set, but: {why}"),
    }
}

/// Whether a folder with mode 0o500 refuses a write: it does not for root.
/// Panics when the probe's own setup fails.
#[cfg(unix)]
pub fn refuses_writes_to_read_only_folder() -> Result<(), String> {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().expect("a scratch folder");
    let folder = dir.path().join("shut");
    fs::create_dir(&folder).expect("a scratch subfolder");
    fs::set_permissions(&folder, fs::Permissions::from_mode(0o500)).expect("a read-only mode");
    let wrote = fs::write(folder.join("probe"), "").is_ok();
    fs::set_permissions(&folder, fs::Permissions::from_mode(0o700)).expect("restoring the mode");
    if wrote {
        Err("running as root: a read-only folder does not refuse writes".to_owned())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every combination of probe and requirement.
    #[test]
    fn a_probe_runs_skips_or_fails() {
        assert_eq!(gate(Ok(()), false), Gate::Run);
        assert_eq!(gate(Ok(()), true), Gate::Run);
        assert_eq!(gate(Err("why".into()), false), Gate::Skip("why".into()));
        assert_eq!(gate(Err("why".into()), true), Gate::Fail("why".into()));
    }

    /// The write probe agrees with a direct write into a read-only folder.
    #[cfg(unix)]
    #[test]
    fn the_write_probe_matches_a_direct_write() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("shut");
        std::fs::create_dir(&folder).unwrap();
        std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o500)).unwrap();
        let refused = std::fs::write(folder.join("x"), "").is_err();
        std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o700)).unwrap();
        let probe = refuses_writes_to_read_only_folder();
        assert_eq!(probe.is_ok(), refused);
        if let Err(why) = probe {
            assert!(why.contains("root"), "{why}");
        }
    }

    /// The real probe runs on the machine: it passes, or skips with a reason
    /// where the machine cannot, and fails where the fixtures are required.
    #[cfg(unix)]
    #[test]
    fn the_probe_runs_on_this_machine() {
        let _ = skip_unless(refuses_writes_to_read_only_folder());
    }
}
