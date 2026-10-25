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
use owlshift_contracts::DelegationLevel;
use owlshift_contracts::config::{PersonalConfig, Policy, ProjectConfig, check_requires, entries};
use owlshift_core::floor::{FloorCategory, is_human_category};
use owlshift_core::gate::{PassLimit, ResolverPolicy, normalized_categories};
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
        match fs::read_to_string(&path) {
            Ok(text) => Self::from_text(path, &text, parse),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Self::Absent(path),
            Err(error) => Self::Invalid {
                error: format!("cannot read it: {error}"),
                path,
            },
        }
    }

    /// Checks `text`, read from `path` or kept for it, as [`FileState::load`]
    /// checks a file: its `requires` first, then the strict parse.
    pub(crate) fn from_text(
        path: PathBuf,
        text: &str,
        parse: fn(&str) -> Result<T, ContractError>,
    ) -> Self {
        let current = Version::parse(OWLSHIFT_VERSION).expect("the crate version is semver");
        let checked = check_requires(text, &current)
            .and_then(|()| entries(text))
            .and_then(|entries| parse(text).map(|config| (config, entries)));
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

/// Whether the personal file gives the read-only planning and review roles
/// and the resolver their web tools (OWL-415): yes unless `web_tools =
/// false`, and yes on a machine without a personal file. An unknown or
/// invalid file never reaches a run, since `owlshift do` refuses it; it
/// gives none all the same.
pub fn web_tools_enabled(personal: &FileState<PersonalConfig>) -> bool {
    match personal {
        FileState::Loaded { config, .. } => config.web_tools != Some(false),
        FileState::Absent(_) | FileState::NotApplicable(_) => true,
        FileState::Unavailable(_) | FileState::Invalid { .. } => false,
    }
}

/// Whether a drain's step child keeps the machine from idle sleep for its
/// life (OWL-439, architecture section 5, "Keep awake"): yes unless
/// `keep_awake = false`, and yes on a machine without a personal file; no
/// for a file that could not be read or parsed, which never reaches a run.
pub fn keep_awake_enabled(personal: &FileState<PersonalConfig>) -> bool {
    match personal {
        FileState::Loaded { config, .. } => config.keep_awake != Some(false),
        FileState::Absent(_) | FileState::NotApplicable(_) => true,
        FileState::Unavailable(_) | FileState::Invalid { .. } => false,
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
        let personal = personal_state(personal);
        let mut effective = Self { project, personal };
        effective.check_project(&|root: &Path| origin_repo(system, root));
        effective
    }

    /// The configuration a served project runs on (OWL-434): the project
    /// file `text`, kept at `label` from the default branch of `repo`, and
    /// the personal file at `personal`, checked as [`Effective::load`]
    /// checks them, the names scoped to `repo` applying.
    pub fn served(repo: &Repo, label: PathBuf, text: &str, personal: Option<PathBuf>) -> Self {
        let mut effective = Self {
            project: FileState::from_text(label, text, ProjectConfig::parse),
            personal: personal_state(personal),
        };
        effective.check_project(&|_: &Path| Ok(repo.clone()));
        effective
    }

    /// The operator's allowances applied to the project's declarations, the
    /// repository being `repository` of the file's folder, asked only when
    /// the project declares a variable or a cache folder.
    fn check_project(&mut self, repository: &dyn Fn(&Path) -> Result<Repo, String>) {
        self.check_gate_env(repository);
        self.check_caches(repository);
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

    /// The cache folders the operator lets a project of `repository` open
    /// read-only to its agents: the personal `allow_caches`, and that of the
    /// repository's entry (OWL-330), as for [`Effective::allowed_gate_env`].
    pub fn allowed_caches(&self, repository: Option<&Repo>) -> Option<Vec<&str>> {
        let key = repository.map(repository_key);
        match &self.personal {
            FileState::Loaded { config, .. } => Some(config.allow_caches_names(key.as_deref())),
            FileState::Absent(_) | FileState::NotApplicable(_) => Some(Vec::new()),
            FileState::Unavailable(_) | FileState::Invalid { .. } => None,
        }
    }

    /// Refuses, as invalid, a project that declares a cache folder the
    /// operator does not allow for this repository (OWL-330), as
    /// [`Effective::check_gate_env`] refuses a variable: no run and no gate
    /// starts with it. A project already invalid is left as it is.
    fn check_caches(&mut self, repository: &dyn Fn(&Path) -> Result<Repo, String>) {
        let FileState::Loaded { path, config, .. } = &self.project else {
            return;
        };
        if config.stack.caches.is_empty() {
            return;
        }
        let repository = repository(path.parent().unwrap_or(path));
        let Some(allowed) = self.allowed_caches(repository.as_ref().ok()) else {
            return;
        };
        if let Err(error) = config.check_caches(&allowed) {
            let note = match &repository {
                Ok(repo) => format!("this repository is {}", repository_key(repo)),
                Err(reason) => format!("folders scoped to a repository do not apply: {reason}"),
            };
            self.project = FileState::Invalid {
                path: path.clone(),
                error: format!("{error}; {note}"),
            };
        }
    }

    /// Refuses, as invalid, a project that declares for its gate a variable
    /// the operator does not allow (OWL-63), for this repository (OWL-75).
    /// The repository is read from the checkout's `origin`, only when the
    /// project declares a name; with none known, only the machine-wide names
    /// apply, and the refusal says why. With the personal file unknown or
    /// invalid, the configuration is invalid already and the project is left
    /// as it is.
    fn check_gate_env(&mut self, repository: &dyn Fn(&Path) -> Result<Repo, String>) {
        let FileState::Loaded { path, config, .. } = &self.project else {
            return;
        };
        if config.stack.gate_env.is_empty() {
            return;
        }
        let repository = repository(path.parent().unwrap_or(path));
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

/// The personal file at `personal`, `None` when the platform has no place
/// for it.
fn personal_state(personal: Option<PathBuf>) -> FileState<PersonalConfig> {
    match personal {
        Some(path) => FileState::load(path, PersonalConfig::parse),
        None => FileState::NotApplicable("this system has no configuration directory".into()),
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
pub(crate) fn origin_repo<S: System + ?Sized>(system: &S, root: &Path) -> Result<Repo, String> {
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
        self.write_always_human(f)?;
        self.write_resolver(f)?;
        self.write_budget(f)
    }
}

impl Effective {
    /// The project's resolver settings, as the gate reads them, and the
    /// file they come from: the defaults without a project file.
    fn resolver(&self) -> (ResolverPolicy, Option<(&Path, &Policy)>) {
        match &self.project {
            FileState::Loaded { path, config, .. } => {
                (config.policy.resolver(), Some((path, &config.policy)))
            }
            _ => (ResolverPolicy::default(), None),
        }
    }

    /// The categories that always go to a human: the floor's, which no file
    /// removes, those the project's delegation level keeps (OWL-328), then
    /// the project's additions with their file. An addition the level
    /// already covers (matching a token the way [`is_human_category`] does)
    /// is shown as such.
    fn write_always_human(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (resolver, project) = self.resolver();
        let level = resolver.level;
        f.write_str(
            "\nalways-human categories (the floor's never go away; the delegation level keeps \
             others; a project adds to them):\n",
        )?;
        for category in FloorCategory::ALL {
            if category.human_at(DelegationLevel::All) {
                writeln!(f, "  {}  (floor)", category.token())?;
            } else if category.human_at(level) {
                writeln!(
                    f,
                    "  {}  (delegation level {})",
                    category.token(),
                    level.as_str()
                )?;
            }
        }
        if let Some((path, policy)) = project {
            // As the gate reads them: normalized, each once, blank ones dropped.
            for category in normalized_categories(&policy.always_human) {
                let note = if is_human_category(&category, level) {
                    "  already covered by the delegation level"
                } else {
                    ""
                };
                writeln!(f, "  {category}  ({}){note}", path.display())?;
            }
        }
        Ok(())
    }

    /// What the resolver may do (OWL-328): its delegation level, its pass
    /// limit and its ceiling, each with the file it comes from, or
    /// `default`.
    fn write_resolver(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (resolver, project) = self.resolver();
        f.write_str("\nresolver (what it decides without the decider):\n")?;
        let source = |set: bool| match project {
            Some((path, _)) if set => path.display().to_string(),
            _ => "default".to_owned(),
        };
        let policy = project.map(|(_, policy)| policy);
        writeln!(
            f,
            "  resolver_delegation = \"{}\"  ({})",
            resolver.level.as_str(),
            source(policy.is_some_and(|p| p.resolver_delegation.is_some()))
        )?;
        let passes = match resolver.passes {
            PassLimit::Runs(runs) => runs.to_string(),
            PassLimit::Unlimited => "\"unlimited\"".to_owned(),
        };
        writeln!(
            f,
            "  resolver_passes = {passes}  ({})",
            source(policy.is_some_and(|p| p.resolver_passes.is_some()))
        )?;
        match policy.and_then(|p| p.resolver_budget_usd) {
            Some(usd) => writeln!(f, "  resolver_budget_usd = {}  ({})", usd.0, source(true)),
            None => writeln!(f, "  resolver_budget_usd: none, no ceiling  (default)"),
        }
    }

    /// The budget on one ticket's runs (OWL-378), with the file it comes
    /// from, or `default`: none.
    fn write_budget(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("\nbudget (what one ticket's runs may cost):\n")?;
        match self
            .resolver()
            .1
            .and_then(|(path, policy)| policy.ticket_budget_usd.map(|usd| (path, usd)))
        {
            Some((path, usd)) => {
                writeln!(f, "  ticket_budget_usd = {}  ({})", usd.0, path.display())
            }
            None => writeln!(f, "  ticket_budget_usd: none, no budget  (default)"),
        }
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

    /// OWL-328: `config show` prints the resolver's level, pass limit and
    /// ceiling with their source, and the always-human categories of the
    /// level, the floor's at every level.
    #[test]
    fn the_resolver_settings_are_shown_with_their_source() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(PROJECT_FILE);
        let personal = dir.path().join("personal.toml");
        let shown = |policy: &str| {
            fs::write(&file, PROJECT.replace("always_human = []", policy)).unwrap();
            let effective =
                Effective::load(&repository(dir.path()), dir.path(), Some(personal.clone()));
            assert!(effective.is_valid(), "{effective}");
            effective.to_string()
        };
        let path = file.display();

        let default = shown("always_human = []");
        for line in [
            "  resolver_delegation = \"discoverable\"  (default)".to_owned(),
            "  resolver_passes = 5  (default)".to_owned(),
            "  resolver_budget_usd: none, no ceiling  (default)".to_owned(),
            "  ticket_budget_usd: none, no budget  (default)".to_owned(),
            "  scope  (delegation level discoverable)".to_owned(),
            "  money  (delegation level discoverable)".to_owned(),
            "  security  (floor)".to_owned(),
        ] {
            assert!(default.contains(&line), "{line}\n---\n{default}");
        }

        let all = shown(
            "always_human = [\"money\"]\nresolver_delegation = \"all\"\n\
             resolver_passes = \"unlimited\"\nresolver_budget_usd = 2.5\nticket_budget_usd = 100",
        );
        for line in [
            format!("  resolver_delegation = \"all\"  ({path})"),
            format!("  resolver_passes = \"unlimited\"  ({path})"),
            format!("  resolver_budget_usd = 2.5  ({path})"),
            format!("  ticket_budget_usd = 100  ({path})"),
            "  security  (floor)".to_owned(),
            "  irreversible  (floor)".to_owned(),
            format!("  money  ({path})"),
        ] {
            assert!(all.contains(&line), "{line}\n---\n{all}");
        }
        for line in [
            "  scope  (delegation level",
            "  data_loss  (delegation level",
        ] {
            assert!(!all.contains(line), "{line}\n---\n{all}");
        }
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

    /// OWL-330: a project that declares a cache folder the operator does not
    /// allow for its repository is refused when the configuration is loaded,
    /// naming `allow_caches`; listed for that repository it loads, and listed
    /// for another it is refused again.
    #[test]
    fn a_declared_cache_needs_the_operators_allowance_for_the_repository() {
        let dir = tempfile::tempdir().unwrap();
        let declaring = PROJECT.replace(
            "gate = [\"cargo test\"]",
            "gate = [\"cargo test\"]\ncaches = [\"Library/pnpm\"]",
        );
        fs::write(dir.path().join(PROJECT_FILE), declaring).unwrap();
        let personal = dir.path().join("personal.toml");
        let load = || {
            let system = repository_from(
                dir.path(),
                Answer::Exit(0, "git@github.com:acme/api.git\n", ""),
            );
            Effective::load(&system, dir.path(), Some(personal.clone()))
        };
        let refusal = |effective: Effective| match effective.project {
            FileState::Invalid { error, .. } => error,
            _ => panic!("{effective}"),
        };

        let error = refusal(load());
        assert!(
            error.starts_with(
                "invalid owlshift.toml: stack.caches: \"Library/pnpm\" may not be read by an agent"
            ) && error.contains("`allow_caches`")
                && error.ends_with("; this repository is github.com/acme/api"),
            "{error}"
        );

        fs::write(
            &personal,
            "[repositories.\"github.com/acme/api\"]\nallow_caches = [\"Library/pnpm\"]\n",
        )
        .unwrap();
        let effective = load();
        assert!(effective.is_valid(), "{effective}");
        let api = Repo::parse("acme/api").unwrap();
        let web = Repo::parse("acme/web").unwrap();
        assert_eq!(
            effective.allowed_caches(Some(&api)),
            Some(vec!["Library/pnpm"])
        );
        assert_eq!(effective.allowed_caches(Some(&web)), Some(vec![]));

        fs::write(
            &personal,
            "[repositories.\"github.com/acme/web\"]\nallow_caches = [\"Library/pnpm\"]\n",
        )
        .unwrap();
        assert!(refusal(load()).contains("`allow_caches`"));

        fs::write(&personal, "allow_caches = [\"Library/pnpm\"]\n").unwrap();
        assert!(load().is_valid());
    }

    /// OWL-415: the web tools are on without a personal file and with one
    /// that leaves the key out or sets it `true`, off with `web_tools =
    /// false`, and off for a file that could not be read or parsed.
    /// Keep-awake likewise (OWL-439).
    #[test]
    fn the_web_tools_and_keep_awake_are_on_unless_the_personal_file_turns_them_off() {
        let loaded = |text: &str| FileState::Loaded {
            path: PathBuf::from("config.toml"),
            config: PersonalConfig::parse(text).unwrap(),
            entries: Vec::new(),
        };
        assert!(web_tools_enabled(&FileState::Absent("config.toml".into())));
        assert!(web_tools_enabled(&FileState::NotApplicable(String::new())));
        assert!(web_tools_enabled(&loaded("")));
        assert!(web_tools_enabled(&loaded("web_tools = true\n")));
        assert!(!web_tools_enabled(&loaded("web_tools = false\n")));
        assert!(!web_tools_enabled(&FileState::Invalid {
            path: PathBuf::from("config.toml"),
            error: String::new(),
        }));
        // OWL-439: keep-awake reads its key the same way.
        assert!(keep_awake_enabled(&FileState::Absent("config.toml".into())));
        assert!(keep_awake_enabled(&loaded("")));
        assert!(keep_awake_enabled(&loaded("keep_awake = true\n")));
        assert!(!keep_awake_enabled(&loaded("keep_awake = false\n")));
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
