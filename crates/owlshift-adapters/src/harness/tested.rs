//! The harness CLI versions Owlshift was tested with.
//!
//! Harness CLIs update themselves silently, and a flag or a message can
//! change between two releases (`docs/design/runtime-and-operations.md`,
//! "Updates & versions"). `owlshift doctor` warns on an installed version
//! outside these lists; it never refuses one.
//!
//! A version enters a list once the harness's contract tests pass on output
//! recorded with it. `tests/claude_contract.rs` fails when a recorded Claude
//! fixture carries a version missing here, so re-recording the fixtures with
//! a new release forces this list to follow. `tests/claude_live.rs` fails the
//! same way on the version the real CLI reports, so a silent self-update
//! does not pass quietly either. The lists only grow: a version tested once
//! stays tested after newer fixtures replace its recordings.
//!
//! Matching is exact: a patch release can drop a flag, so `2.1.285` is not
//! covered by `2.1.284`.

use owlshift_contracts::Harness;

/// Claude Code releases the contract fixtures were recorded with.
const CLAUDE: &[&str] = &["2.1.283", "2.1.284"];

/// Codex has no contract tests yet, so no version is tested. C8 ran its login
/// status command on 0.154.0, which says nothing about how it runs a role.
const CODEX: &[&str] = &[];

/// The tested versions of a harness, oldest first.
pub fn tested_versions(harness: Harness) -> &'static [&'static str] {
    match harness {
        Harness::Claude => CLAUDE,
        Harness::Codex => CODEX,
    }
}

/// Whether `version`, a dotted version such as `2.1.283`, was tested.
pub fn is_tested(harness: Harness, version: &str) -> bool {
    tested_versions(harness).contains(&version)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matching_is_exact() {
        assert!(is_tested(Harness::Claude, "2.1.283"));
        assert!(is_tested(Harness::Claude, "2.1.284"));
        for other in ["2.1.285", "2.1", "2.1.2830", "2.1.283-beta.1", ""] {
            assert!(!is_tested(Harness::Claude, other), "{other}");
        }
        assert!(!is_tested(Harness::Codex, "0.154.0"));
    }
}
