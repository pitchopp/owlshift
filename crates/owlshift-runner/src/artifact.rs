//! Reading the artifacts a role names in its `result.json`.
//!
//! The contract already refuses absolute paths and `..`; it cannot see the
//! file system, so a role could still name a symbolic link that leads out of
//! its worktree. Every artifact is read through
//! [`read_confined`], which refuses any symbolic link on the way.

use std::error::Error;
use std::fmt;
use std::path::Path;

use owlshift_contracts::ids::RelativePath;
use owlshift_contracts::result::Artifacts;
use owlshift_platform::confined::{ConfinedError, read_confined};

/// The contents of the artifacts a run named; `None` for one it did not name.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ArtifactContents {
    pub plan: Option<Vec<u8>>,
    pub ledger: Option<Vec<u8>>,
    pub findings: Option<Vec<u8>>,
    pub report: Option<Vec<u8>>,
}

/// An artifact that was refused or could not be read, which fails the run.
#[derive(Debug)]
pub struct ArtifactError {
    /// The `artifacts` field that named it: `plan`, `ledger`, `findings` or
    /// `report`.
    pub field: &'static str,
    pub source: ConfinedError,
}

impl fmt::Display for ArtifactError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "artifact `{}`: {}", self.field, self.source)
    }
}

impl Error for ArtifactError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.source)
    }
}

/// Reads every artifact `artifacts` names inside `worktree`, stopping at the
/// first one refused.
pub fn read_artifacts(
    worktree: &Path,
    artifacts: &Artifacts,
) -> Result<ArtifactContents, ArtifactError> {
    let read = |field, path: &Option<RelativePath>| {
        path.as_ref()
            .map(|path| {
                read_confined(worktree, path.as_str())
                    .map_err(|source| ArtifactError { field, source })
            })
            .transpose()
    };
    Ok(ArtifactContents {
        plan: read("plan", &artifacts.plan)?,
        ledger: read("ledger", &artifacts.ledger)?,
        findings: read("findings", &artifacts.findings)?,
        report: read("report", &artifacts.report)?,
    })
}
