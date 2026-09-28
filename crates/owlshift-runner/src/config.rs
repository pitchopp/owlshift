//! The effective configuration: the project file and the personal file, and
//! the file each value comes from.
//!
//! The project file is `owlshift.toml` at the root of the git repository
//! holding the current directory; the personal file is found by
//! `owlshift_platform::paths::personal_config_file`. The two files hold
//! disjoint keys, so no value overrides another yet.

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use owlshift_contracts::ContractError;
use owlshift_contracts::config::{PersonalConfig, ProjectConfig, check_requires, entries};
use semver::Version;

use crate::system::System;

/// This binary's version.
pub const OWLSHIFT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The project file's name, at the root of the repository.
pub const PROJECT_FILE: &str = "owlshift.toml";

/// What is known about one configuration file.
#[derive(Debug)]
pub enum FileState<T> {
    /// There is no place to look for it, and that is normal: outside a git
    /// repository, or on a system with no configuration directory.
    NotApplicable(String),
    /// Where to look could not be determined; the configuration is unknown.
    Unavailable(String),
    /// Nothing at the expected path.
    Absent(PathBuf),
    Loaded {
        path: PathBuf,
        config: T,
        /// Every key the file sets and its value, as written.
        entries: Vec<(String, String)>,
    },
    Invalid {
        path: PathBuf,
        error: String,
    },
}

impl<T> FileState<T> {
    fn is_error(&self) -> bool {
        matches!(self, Self::Unavailable(_) | Self::Invalid { .. })
    }

    /// Reads and checks the file at `path`: its `requires` first, then the
    /// strict parse.
    pub(crate) fn load(path: PathBuf, parse: fn(&str) -> Result<T, ContractError>) -> Self {
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Self::Absent(path),
            Err(error) => {
                return Self::Invalid {
                    error: format!("cannot read it: {error}"),
                    path,
                };
            }
        };
        let current = Version::parse(OWLSHIFT_VERSION).expect("the crate version is semver");
        let checked = check_requires(&text, &current)
            .and_then(|()| entries(&text))
            .and_then(|entries| parse(&text).map(|config| (config, entries)));
        match checked {
            Ok((config, entries)) => Self::Loaded {
                path,
                config,
                entries,
            },
            Err(error) => Self::Invalid {
                error: error.to_string(),
                path,
            },
        }
    }
}

/// The configuration in effect for a directory.
#[derive(Debug)]
pub struct Effective {
    pub project: FileState<ProjectConfig>,
    pub personal: FileState<PersonalConfig>,
}

impl Effective {
    /// Loads the project file of the repository holding `cwd`, and the
    /// personal file at `personal` (`None`: the platform has no place for it).
    pub fn load(system: &dyn System, cwd: &Path, personal: Option<PathBuf>) -> Self {
        let project = match project_root(system, cwd) {
            Ok(Some(root)) => FileState::load(root.join(PROJECT_FILE), ProjectConfig::parse),
            Ok(None) => FileState::NotApplicable("not in a git repository".to_owned()),
            Err(reason) => FileState::Unavailable(reason),
        };
        let personal = match personal {
            Some(path) => FileState::load(path, PersonalConfig::parse),
            None => FileState::NotApplicable("this system has no configuration directory".into()),
        };
        Self { project, personal }
    }

    /// Whether both files could be read and are valid, or are simply absent.
    pub fn is_valid(&self) -> bool {
        !self.project.is_error() && !self.personal.is_error()
    }
}

/// The root of the git repository holding `cwd`: `Ok(None)` outside a
/// repository, `Err` when git could not tell.
fn project_root(system: &dyn System, cwd: &Path) -> Result<Option<PathBuf>, String> {
    let git = system
        .locate("git")
        .ok_or_else(|| "git is not on the PATH, so the repository is unknown".to_owned())?;
    let captured = system
        .run(&git, &["rev-parse", "--show-toplevel"], Some(cwd))
        .map_err(|error| format!("`git rev-parse --show-toplevel`: {error}"))?;
    if captured.code == Some(0) {
        let mut out = captured.stdout;
        while out.last().is_some_and(|b| *b == b'\n' || *b == b'\r') {
            out.pop();
        }
        if out.is_empty() {
            return Err("`git rev-parse --show-toplevel` printed no path".to_owned());
        }
        return path_from_bytes(out).map(Some);
    }
    let stderr = String::from_utf8_lossy(&captured.stderr);
    if stderr.contains("not a git repository") {
        return Ok(None);
    }
    let first_line = stderr.lines().next().unwrap_or("").trim();
    Err(format!(
        "`git rev-parse --show-toplevel` failed ({}): {first_line}",
        exit_text(captured.code)
    ))
}

#[cfg(unix)]
fn path_from_bytes(bytes: Vec<u8>) -> Result<PathBuf, String> {
    use std::os::unix::ffi::OsStringExt;
    Ok(PathBuf::from(std::ffi::OsString::from_vec(bytes)))
}

#[cfg(not(unix))]
fn path_from_bytes(bytes: Vec<u8>) -> Result<PathBuf, String> {
    String::from_utf8(bytes)
        .map(PathBuf::from)
        .map_err(|_| "git printed a repository path that is not UTF-8".to_owned())
}

pub(crate) fn exit_text(code: Option<i32>) -> String {
    match code {
        Some(code) => format!("exit status {code}"),
        None => "ended by a signal".to_owned(),
    }
}

impl<T> FileState<T> {
    fn describe(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotApplicable(reason) => write!(f, "none ({reason})"),
            Self::Unavailable(reason) => write!(f, "unknown: {reason}"),
            Self::Absent(path) => write!(f, "not found at {}", path.display()),
            Self::Loaded { path, .. } => write!(f, "{}", path.display()),
            Self::Invalid { path, error } => write!(f, "{} is invalid: {error}", path.display()),
        }
    }

    fn write_entries(&self, f: &mut fmt::Formatter<'_>) -> Result<bool, fmt::Error> {
        let Self::Loaded { path, entries, .. } = self else {
            return Ok(false);
        };
        for (key, value) in entries {
            writeln!(f, "{key} = {value}  ({})", path.display())?;
        }
        Ok(!entries.is_empty())
    }
}

/// `owlshift config show`: where each file is, then every value and its file.
impl fmt::Display for Effective {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("project file:  ")?;
        self.project.describe(f)?;
        f.write_str("\npersonal file: ")?;
        self.personal.describe(f)?;
        f.write_str("\n\n")?;
        let project = self.project.write_entries(f)?;
        let personal = self.personal.write_entries(f)?;
        if !project && !personal {
            f.write_str("No configuration value is set.\n")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::system::fake::{Answer, FakeSystem};

    const PROJECT: &str = r#"
        requires = ">=0.0"
        [tracker]
        kind = "linear"
        team = "OWL"
        admit = { label = "agent" }
        states = { ready = "Todo", working = "In Progress", needs_input = "Needs Input", review = "In Review" }
        [stack]
        gate = ["cargo test"]
        [pipeline]
        default = "standard"
        plan_approval = "on-fork"
        [models]
        [policy]
        always_human = []
    "#;

    /// A repository whose `git rev-parse` answers with `root`.
    fn repository(root: &Path) -> FakeSystem {
        let stdout: &'static str = Box::leak(format!("{}\n", root.display()).into_boxed_str());
        FakeSystem::default()
            .install("git")
            .answer("git rev-parse --show-toplevel", Answer::Exit(0, stdout, ""))
    }

    #[test]
    fn values_come_with_their_file() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(PROJECT_FILE), PROJECT).unwrap();
        let personal = dir.path().join("personal.toml");
        let effective = Effective::load(&repository(dir.path()), dir.path(), Some(personal.clone()));

        assert!(effective.is_valid());
        assert!(matches!(&effective.personal, FileState::Absent(p) if *p == personal));
        let shown = effective.to_string();
        let origin = dir.path().join(PROJECT_FILE);
        assert!(
            shown.contains(&format!("tracker.kind = \"linear\"  ({})", origin.display())),
            "{shown}"
        );
        assert!(shown.contains("not found at"), "{shown}");
    }

    #[test]
    fn invalid_files_are_errors() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(PROJECT_FILE), "requires = \">=99\"\n").unwrap();
        let personal = dir.path().join("personal.toml");
        fs::write(&personal, "concurrent_runs = \"many\"\n").unwrap();
        let effective = Effective::load(&repository(dir.path()), dir.path(), Some(personal));

        assert!(!effective.is_valid());
        let shown = effective.to_string();
        assert!(shown.contains("upgrade Owlshift"), "{shown}");
        assert!(matches!(effective.personal, FileState::Invalid { .. }));
    }

    #[test]
    fn outside_a_repository_there_is_no_project() {
        let git_says_no = FakeSystem::default().install("git").answer(
            "git rev-parse --show-toplevel",
            Answer::Exit(128, "", "fatal: not a git repository (or any of the parent directories): .git\n"),
        );
        let effective = Effective::load(&git_says_no, Path::new("/tmp"), None);
        assert!(matches!(effective.project, FileState::NotApplicable(_)));
        assert!(effective.is_valid());

        let no_git = FakeSystem::default();
        let effective = Effective::load(&no_git, Path::new("/tmp"), None);
        assert!(matches!(effective.project, FileState::Unavailable(_)));
        assert!(!effective.is_valid());
    }
}
