//! The runner reads the artifacts named in `result.json` and fails on one
//! that leaves the worktree through a symbolic link, naming its field.

use std::fs;
use std::path::Path;

use owlshift_contracts::ids::RelativePath;
use owlshift_contracts::result::Artifacts;
use owlshift_platform::confined::{ConfinedError, Refusal};
use owlshift_runner::artifact::read_artifacts;

fn path(path: &str) -> Option<RelativePath> {
    Some(RelativePath::new(path).unwrap())
}

#[test]
fn every_named_artifact_is_read() {
    let wt = tempfile::tempdir().unwrap();
    fs::create_dir(wt.path().join("docs")).unwrap();
    for (name, contents) in [
        ("plan.md", "the plan"),
        ("ledger.md", "the ledger"),
        ("docs/findings.md", "the findings"),
        ("report.md", "the report"),
    ] {
        fs::write(wt.path().join(name), contents).unwrap();
    }

    let all = Artifacts {
        plan: path("plan.md"),
        ledger: path("ledger.md"),
        findings: path("docs/findings.md"),
        report: path("report.md"),
    };
    let contents = read_artifacts(wt.path(), &all).unwrap();
    assert_eq!(contents.plan.as_deref(), Some(&b"the plan"[..]));
    assert_eq!(contents.ledger.as_deref(), Some(&b"the ledger"[..]));
    assert_eq!(contents.findings.as_deref(), Some(&b"the findings"[..]));
    assert_eq!(contents.report.as_deref(), Some(&b"the report"[..]));

    let only_plan = Artifacts {
        plan: path("plan.md"),
        ..Artifacts::default()
    };
    let contents = read_artifacts(wt.path(), &only_plan).unwrap();
    assert!(contents.plan.is_some());
    assert_eq!(
        (contents.ledger, contents.findings, contents.report),
        (None, None, None)
    );
}

#[test]
fn a_symlinked_artifact_fails_the_run_naming_its_field() {
    let base = tempfile::tempdir().unwrap();
    let wt = base.path().join("wt");
    fs::create_dir(&wt).unwrap();
    let secret = base.path().join("secret.txt");
    fs::write(&secret, "outside secret").unwrap();
    if !symlink_file(&secret, &wt.join("plan.md")) {
        return;
    }

    let artifacts = Artifacts {
        plan: path("plan.md"),
        ..Artifacts::default()
    };
    let error = read_artifacts(&wt, &artifacts).unwrap_err();
    assert_eq!(error.field, "plan");
    assert!(matches!(
        error.source,
        ConfinedError::Refused {
            reason: Refusal::SymbolicLink,
            ..
        }
    ));
    assert_eq!(
        error.to_string(),
        "artifact `plan`: `plan.md` is a symbolic link"
    );
}

#[test]
fn the_refused_field_is_the_one_named() {
    let wt = tempfile::tempdir().unwrap();
    fs::write(wt.path().join("plan.md"), "the plan").unwrap();
    let artifacts = Artifacts {
        plan: path("plan.md"),
        report: path("report.md"),
        ..Artifacts::default()
    };
    let error = read_artifacts(wt.path(), &artifacts).unwrap_err();
    assert_eq!(error.field, "report");
    assert_eq!(
        error.to_string(),
        "artifact `report`: `report.md` does not exist"
    );
}

/// Creates a symbolic link to a file. On Windows this needs Developer Mode or
/// the symbolic-link privilege: without it the test is skipped locally, and
/// fails under CI, which must prove the case.
fn symlink_file(target: &Path, link: &Path) -> bool {
    #[cfg(unix)]
    let created = std::os::unix::fs::symlink(target, link);
    #[cfg(windows)]
    let created = std::os::windows::fs::symlink_file(target, link);
    match created {
        Ok(()) => true,
        // ERROR_PRIVILEGE_NOT_HELD
        Err(error) if cfg!(windows) && error.raw_os_error() == Some(1314) => {
            assert!(
                std::env::var_os("CI").is_none(),
                "creating a symbolic link needs the symbolic-link privilege on CI: {error}"
            );
            eprintln!("skipped: creating a symbolic link needs Developer Mode on Windows");
            false
        }
        Err(error) => panic!("cannot create a symbolic link: {error}"),
    }
}
