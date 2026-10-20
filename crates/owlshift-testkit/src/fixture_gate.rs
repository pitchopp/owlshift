//! Whether a fixture project's bench test runs, skips or fails (OWL-330).
//!
//! The Node and Python fixtures need pnpm, uv and the sandbox. Where one is
//! missing the test is skipped with a message, as the confinement tests are
//! without a sandbox, unless the environment requires the fixtures
//! (`OWLSHIFT_REQUIRE_FIXTURE_TOOLS`, set on CI's Linux and macOS legs):
//! there the test fails instead, so a missing tool never passes for a run.

/// The variable that makes a fixture test fail rather than skip.
pub const REQUIRE_FIXTURE_TOOLS: &str = "OWLSHIFT_REQUIRE_FIXTURE_TOOLS";

/// What a fixture test does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FixtureGate {
    /// Everything it needs is here.
    Run,
    /// Something is missing and nothing requires it: the test returns.
    Skip,
    /// Something is missing and the environment requires it: the test
    /// fails, naming what.
    Fail,
}

/// What a fixture test does, from whether its tools are on the `PATH`,
/// whether the sandbox can confine a run here, and whether the environment
/// requires the fixtures.
pub fn fixture_gate(tools_present: bool, sandbox_ok: bool, require: bool) -> FixtureGate {
    match (tools_present && sandbox_ok, require) {
        (true, _) => FixtureGate::Run,
        (false, false) => FixtureGate::Skip,
        (false, true) => FixtureGate::Fail,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every combination: a run with the tools and the sandbox, whatever is
    /// required; otherwise a skip, or a failure where it is required.
    #[test]
    fn a_fixture_test_runs_skips_or_fails() {
        for tools in [false, true] {
            for sandbox in [false, true] {
                for require in [false, true] {
                    let expected = if tools && sandbox {
                        FixtureGate::Run
                    } else if require {
                        FixtureGate::Fail
                    } else {
                        FixtureGate::Skip
                    };
                    assert_eq!(
                        fixture_gate(tools, sandbox, require),
                        expected,
                        "tools {tools}, sandbox {sandbox}, require {require}"
                    );
                }
            }
        }
    }
}
