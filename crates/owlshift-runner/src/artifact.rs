//! Reading the artifacts a role names in its `result.json`.
//!
//! The contract already refuses absolute paths and `..`; it cannot see the
//! file system, so a role could still name a symbolic link that leads out of
//! its worktree, or a hard link to a file outside it. Every artifact is read
//! through [`read_confined`], which refuses any symbolic link on the way and
//! any file with a second name, and stops
//! reading past [`MAX_ARTIFACT_BYTES`] so that a huge, sparse or growing file
//! cannot exhaust the runner's memory or keep it reading.

use std::error::Error;
use std::fmt;
use std::path::Path;

use owlshift_contracts::ids::RelativePath;
use owlshift_contracts::result::Artifacts;
use owlshift_platform::confined::{ConfinedError, Refusal, read_confined};

/// The most bytes an artifact may hold: 1 MiB. Artifacts are text a model
/// writes (a plan, a ledger, findings, a report), far below this; a larger
/// one fails the run.
pub const MAX_ARTIFACT_BYTES: u64 = 1024 * 1024;

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

impl ArtifactError {
    /// Whether the run that named the artifact is the one to fix it, so its
    /// next run is told (OWL-184): a path that is not plain, a link, a
    /// second name, something other than a regular file, a missing or too
    /// large file. A file deleted while the runner read it, a worktree the
    /// runner cannot use or a failing file system is not the run's mistake.
    pub fn is_the_runs(&self) -> bool {
        match &self.source {
            ConfinedError::InvalidPath { .. } => true,
            ConfinedError::Refused { reason, .. } => match reason {
                Refusal::SymbolicLink
                | Refusal::NotADirectory
                | Refusal::NotFound
                | Refusal::NotARegularFile(_)
                | Refusal::TooLarge(_)
                | Refusal::HardLink(_) => true,
                Refusal::Unlinked => false,
            },
            ConfinedError::InvalidRoot { .. }
            | ConfinedError::Root { .. }
            | ConfinedError::Io { .. } => false,
        }
    }
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
                read_confined(worktree, path.as_str(), MAX_ARTIFACT_BYTES)
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
