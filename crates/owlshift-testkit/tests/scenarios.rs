//! The scenarios of `tests/scenarios/`, played on the fake harness.

use std::path::{Path, PathBuf};

use owlshift_testkit::scenario::{play, play_str};

fn fake_harness() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_owlshift-fake-harness"))
}

fn scenarios() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/scenarios")
}

#[test]
fn smoke() {
    if let Err(error) = play(&scenarios().join("smoke.toml"), fake_harness()) {
        panic!("{error}");
    }
}

#[test]
fn a_wrong_expectation_fails_and_names_the_step() {
    let wrong = r#"
        description = "Dispatch to the trivial pipeline's first stage, expecting another."
        ticket = "DEMO-1"
        start = "2026-09-28T09:00:00Z"

        [[step]]
        dispatch = true
        expect = { stage = "verify" }
    "#;
    let error = play_str("wrong", wrong, &scenarios().join("smoke"), fake_harness()).unwrap_err();
    assert_eq!(
        error.to_string(),
        "wrong, step 1 (dispatch): expected stage verify, found build"
    );
}
