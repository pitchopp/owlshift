//! Loads a role's prompt from `roles/<role>.md`: the file the runner hands to
//! the harness, its `+++` front matter stripped and checked against the
//! contracts this binary writes.

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use owlshift_contracts::Role;
use owlshift_contracts::format::{self, ContractError};

/// Why a role's prompt file could not be loaded.
#[derive(Debug)]
pub enum RoleLoadError {
    /// The file could not be read.
    Io { path: PathBuf, source: io::Error },
    /// The file's front matter does not match the role or the formats this
    /// binary writes.
    Contract {
        path: PathBuf,
        source: Box<ContractError>,
    },
}

impl fmt::Display for RoleLoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => write!(f, "{}: {source}", path.display()),
            Self::Contract { path, source } => write!(f, "{}: {source}", path.display()),
        }
    }
}

impl std::error::Error for RoleLoadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Contract { source, .. } => Some(source),
        }
    }
}

/// The default build role prompt, `roles/build.md`, as this binary was built
/// with it: what `owlshift do` hands Claude Code, once
/// [`format::strip_role_front_matter`] has checked and removed its front
/// matter.
pub const BUILD_ROLE: &str = include_str!("../../../roles/build.md");

/// Reads `<roles_dir>/<role>.md` and strips its front matter, per
/// [`owlshift_contracts::format::strip_role_front_matter`].
pub fn load_role_prompt(roles_dir: &Path, role: Role) -> Result<String, RoleLoadError> {
    let path = roles_dir.join(format!("{}.md", role.as_str()));
    let raw = fs::read_to_string(&path).map_err(|source| RoleLoadError::Io {
        path: path.clone(),
        source,
    })?;
    format::strip_role_front_matter(role, &raw).map_err(|source| RoleLoadError::Contract {
        path,
        source: Box::new(source),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use owlshift_contracts::format::{BRIEF_FORMAT, RESULT_FORMAT};

    /// A well-formed role prompt file, built from this binary's own format
    /// constants so it never drifts from them.
    fn valid() -> String {
        format!(
            "+++\nrole = \"build\"\nbrief_format = {BRIEF_FORMAT}\nresult_format = {RESULT_FORMAT}\n+++\n\n# Build\n\nDo it.\n"
        )
    }

    #[test]
    fn loads_and_strips_a_role_prompt() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("build.md"), valid()).expect("write");
        let prompt = load_role_prompt(dir.path(), Role::Build).expect("load");
        assert_eq!(prompt, "# Build\n\nDo it.\n");
    }

    #[test]
    fn the_built_in_build_prompt_matches_this_binarys_contracts() {
        let prompt = format::strip_role_front_matter(Role::Build, BUILD_ROLE).expect("strips");
        assert!(prompt.trim_start().starts_with("# Build"), "{prompt}");
    }

    #[test]
    fn refuses_a_missing_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let err = load_role_prompt(dir.path(), Role::Build).unwrap_err();
        assert!(matches!(err, RoleLoadError::Io { .. }), "{err}");
    }

    #[test]
    fn refuses_a_mismatched_format() {
        let dir = tempfile::tempdir().expect("tempdir");
        let input = valid().replace(
            &format!("brief_format = {BRIEF_FORMAT}"),
            &format!("brief_format = {}", BRIEF_FORMAT + 1),
        );
        fs::write(dir.path().join("build.md"), input).expect("write");
        let err = load_role_prompt(dir.path(), Role::Build).unwrap_err();
        assert!(matches!(err, RoleLoadError::Contract { .. }), "{err}");
    }
}
