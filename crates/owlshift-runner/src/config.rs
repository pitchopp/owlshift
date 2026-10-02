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

use owlshift_adapters::forge::Repo;
use owlshift_contracts::ContractError;
use owlshift_contracts::config::{PersonalConfig, ProjectConfig, check_requires, entries};
use owlshift_core::floor::{FloorCategory, is_floor_category};
use semver::Version;

use crate::system::System;
use crate::{on_demand, project};

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
        let mut effective = Self { project, personal };
        effective.check_gate_env(system);
        effective
    }

    /// The names the operator lets a project of `repository` pass to its
    /// agents: the personal `allow_gate_env`, and that of the repository's
    /// entry under `repositories` (OWL-75). None without a personal file,
    /// `None` when the personal file is unknown or invalid. `owlshift do`
    /// passes the repository it runs, so the agent environment is built for
    /// that one, whatever the load saw.
    pub fn allowed_gate_env(&self, repository: Option<&Repo>) -> Option<Vec<&str>> {
        let key = repository.map(repository_key);
        match &self.personal {
            FileState::Loaded { config, .. } => Some(config.allow_gate_env_names(key.as_deref())),
            FileState::Absent(_) | FileState::NotApplicable(_) => Some(Vec::new()),
            FileState::Unavailable(_) | FileState::Invalid { .. } => None,
        }
    }

    /// Refuses, as invalid, a project that declares for its gate a variable
    /// the operator does not allow (OWL-63), for this repository (OWL-75).
    /// The repository is read from the checkout's `origin`, only when the
    /// project declares a name; with none known, only the machine-wide names
    /// apply, and the refusal says why. With the personal file unknown or
    /// invalid, the configuration is invalid already and the project is left
    /// as it is.
    fn check_gate_env(&mut self, system: &dyn System) {
        let FileState::Loaded { path, config, .. } = &self.project else {
            return;
        };
        if config.stack.gate_env.is_empty() {
            return;
        }
        let repository = origin_repo(system, path.parent().unwrap_or(path));
        let Some(allowed) = self.allowed_gate_env(repository.as_ref().ok()) else {
            return;
        };
        // The file passed `check_names` when parsed: only a name the
        // operator does not allow fails here, so the note always fits.
        if let Err(error) = config.check_gate_env(&allowed) {
            let note = match &repository {
                Ok(repo) => format!("this repository is {}", repository_key(repo)),
                Err(reason) => format!("names scoped to a repository do not apply: {reason}"),
            };
            self.project = FileState::Invalid {
                path: path.clone(),
                error: format!("{error}; {note}"),
            };
        }
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

/// The key of `repo` under the personal `repositories`, as the data
/// directory places it: `github.com/<owner>/<name>`.
fn repository_key(repo: &Repo) -> String {
    format!("github.com/{repo}")
}

/// The GitHub repository of the `origin` remote of the checkout at `root`,
/// read as `owlshift do` reads it ([`project::origin_text`],
/// [`on_demand::check_origin`]). The remote lives in the checkout's git
/// configuration, which the committed project file cannot set.
fn origin_repo(system: &dyn System, root: &Path) -> Result<Repo, String> {
    let git = system
        .locate("git")
        .ok_or_else(|| "git is not on the PATH".to_owned())?;
    let captured = system
        .run(&git, &["remote", "get-url", "origin"], Some(root))
        .map_err(|error| format!("`git remote get-url origin`: {error}"))?;
    if captured.code != Some(0) {
        return Err("the repository has no remote `origin`".to_owned());
    }
    on_demand::check_origin(&project::origin_text(captured.stdout)?)
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
        self.write_always_human(f)
    }
}

impl Effective {
    /// The categories that always go to a human: the floor's, which no file
    /// removes, then the project's additions with their file. An addition
    /// the floor already covers (matching a floor token the way
    /// [`is_floor_category`] does) is shown as such.
    fn write_always_human(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(
            "\nalways-human categories (the floor's never go away; a project adds to them):\n",
        )?;
        for category in FloorCategory::ALL {
            writeln!(f, "  {}  (floor)", category.token())?;
        }
        if let FileState::Loaded { path, config, .. } = &self.project {
            for category in &config.policy.always_human {
                if is_floor_category(category) {
                    writeln!(
                        f,
                        "  {category}  ({}; already covered by the floor)",
                        path.display()
                    )?;
                } else {
                    writeln!(f, "  {category}  ({})", path.display())?;
                }
            }
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

    /// A repository whose `git rev-parse` answers with `root`, cloned from
    /// `acme/api` on GitHub.
    fn repository(root: &Path) -> FakeSystem {
        repository_from(root, Answer::Exit(0, "git@github.com:acme/api.git\n", ""))
    }

    /// A repository at `root` whose `git remote get-url origin` answers
    /// `origin`.
    fn repository_from(root: &Path, origin: Answer) -> FakeSystem {
        let stdout: &'static str = Box::leak(format!("{}\n", root.display()).into_boxed_str());
        FakeSystem::default()
            .install("git")
            .answer("git rev-parse --show-toplevel", Answer::Exit(0, stdout, ""))
            .answer("git remote get-url origin", origin)
    }

    #[test]
    fn values_come_with_their_file() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(PROJECT_FILE), PROJECT).unwrap();
        let personal = dir.path().join("personal.toml");
        let effective =
            Effective::load(&repository(dir.path()), dir.path(), Some(personal.clone()));

        assert!(effective.is_valid());
        assert!(matches!(&effective.personal, FileState::Absent(p) if *p == personal));
        let shown = effective.to_string();
        let origin = dir.path().join(PROJECT_FILE);
        assert!(
            shown.contains(&format!(
                "tracker.kind = \"linear\"  ({})",
                origin.display()
            )),
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

    /// OWL-63's acceptance: a project that declares a variable the operator
    /// does not allow is refused when the configuration is loaded.
    #[test]
    fn a_declared_variable_needs_the_operators_allowance() {
        let dir = tempfile::tempdir().unwrap();
        let declaring = PROJECT.replace(
            "gate = [\"cargo test\"]",
            "gate = [\"cargo test\"]\ngate_env = [\"DATABASE_URL\"]",
        );
        fs::write(dir.path().join(PROJECT_FILE), declaring).unwrap();
        let personal = dir.path().join("personal.toml");
        let load = || Effective::load(&repository(dir.path()), dir.path(), Some(personal.clone()));

        let effective = load();
        assert!(!effective.is_valid());
        let FileState::Invalid { error, .. } = &effective.project else {
            panic!("{effective}");
        };
        assert!(
            error.starts_with("invalid owlshift.toml: stack.gate_env: DATABASE_URL may not reach")
                && error.contains("`allow_gate_env`"),
            "{error}"
        );

        fs::write(&personal, "allow_gate_env = [\"database_url\"]\n").unwrap();
        let effective = load();
        assert!(effective.is_valid(), "{effective}");
        assert!(matches!(effective.project, FileState::Loaded { .. }));

        // An invalid personal file makes the configuration invalid on its own.
        fs::write(&personal, "allow_gate_env = \"DATABASE_URL\"\n").unwrap();
        assert!(!load().is_valid());
    }

    /// OWL-75's acceptance: a name the operator allows for one repository
    /// is refused, when the configuration is loaded, in a checkout of
    /// another, and in one whose repository is unknown.
    #[test]
    fn a_name_allowed_for_one_repository_is_refused_for_another() {
        let dir = tempfile::tempdir().unwrap();
        let declaring = PROJECT.replace(
            "gate = [\"cargo test\"]",
            "gate = [\"cargo test\"]\ngate_env = [\"DATABASE_URL\"]",
        );
        fs::write(dir.path().join(PROJECT_FILE), declaring).unwrap();
        let personal = dir.path().join("personal.toml");
        fs::write(
            &personal,
            "[repositories.\"github.com/acme/api\"]\nallow_gate_env = [\"DATABASE_URL\"]\n",
        )
        .unwrap();
        let load = |origin| {
            let system = repository_from(dir.path(), origin);
            Effective::load(&system, dir.path(), Some(personal.clone()))
        };
        let refusal = |effective: Effective| match effective.project {
            FileState::Invalid { error, .. } => error,
            _ => panic!("{effective}"),
        };

        let effective = load(Answer::Exit(0, "git@github.com:acme/api.git\n", ""));
        assert!(effective.is_valid(), "{effective}");
        let api = Repo::parse("acme/api").unwrap();
        let web = Repo::parse("acme/web").unwrap();
        assert_eq!(
            effective.allowed_gate_env(Some(&api)),
            Some(vec!["DATABASE_URL"])
        );
        assert_eq!(effective.allowed_gate_env(Some(&web)), Some(vec![]));
        assert_eq!(effective.allowed_gate_env(None), Some(vec![]));

        let error = refusal(load(Answer::Exit(
            0,
            "https://github.com/acme/web.git\n",
            "",
        )));
        assert!(
            error.starts_with("invalid owlshift.toml: stack.gate_env: DATABASE_URL may not reach")
                && error.ends_with("; this repository is github.com/acme/web"),
            "{error}"
        );

        let error = refusal(load(Answer::Exit(
            2,
            "",
            "error: No such remote 'origin'\n",
        )));
        assert!(
            error.ends_with(
                "; names scoped to a repository do not apply: the repository has no remote \
                 `origin`"
            ),
            "{error}"
        );
    }

    #[test]
    fn outside_a_repository_there_is_no_project() {
        let git_says_no = FakeSystem::default().install("git").answer(
            "git rev-parse --show-toplevel",
            Answer::Exit(
                128,
                "",
                "fatal: not a git repository (or any of the parent directories): .git\n",
            ),
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
