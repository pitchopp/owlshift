//! A host that reaches nothing of the machine (P8 part 7, OWL-472): the
//! `System` the web UI's Orchestrator runs `owlshift doctor`'s checks on when
//! the bench daemon serves the real UI. Every method of the trait is
//! overridden here with a scripted answer, since the trait's defaults reach
//! the keychain, the network, git and the data directory; a test scans the
//! trait's method names against this file's, so one added later fails it.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use owlshift_adapters::forge::Repo;
use owlshift_contracts::ids::TicketId;
use owlshift_runner::secrets::TrackerAccounts;
use owlshift_runner::system::{
    AdmissionFacts, AppError, AppUser, Captured, DataDirSource, DefaultBranch, DefaultBranchFile,
    MarkerState, RunError, RunningBuilds, StatesError, System, TokenTryError,
};
#[cfg(unix)]
use owlshift_runner::system::{SentinelProbe, SentinelStatus};
use owlshift_runner::ticket_ref::RefState;

/// What a scripted host answers.
#[derive(Debug)]
pub struct ScriptedSystem {
    data: PathBuf,
    /// The checkout the repository is asked of; `git rev-parse` answers it.
    checkout: PathBuf,
    origin: Repo,
    calls: Mutex<Vec<String>>,
    /// How long each probe of a program takes: lets a test hold a run of
    /// the checks open.
    delay: Duration,
}

impl ScriptedSystem {
    /// A host with the data directory `data`, whose repository at
    /// `checkout` is `origin`'s.
    pub fn bench(data: &Path, checkout: &Path, origin: &str) -> Self {
        Self {
            data: data.to_owned(),
            checkout: checkout.to_owned(),
            origin: Repo::parse(origin).expect("a repository"),
            calls: Mutex::new(Vec::new()),
            delay: Duration::ZERO,
        }
    }

    /// The same host, each program lookup taking `delay`.
    #[must_use]
    pub fn with_delay(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }

    /// The calls made, each `method path`, in order.
    pub fn calls(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn note(&self, method: &str, path: &Path) {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(format!("{method} {}", path.display()));
    }

    /// The repository whose dedicated checkout is `root`
    /// (`<data>/projects/<host>/<owner>/<name>/checkout`), else the one the
    /// host was made for.
    fn origin_of(&self, root: &Path) -> Repo {
        let folder = root.parent();
        let name = folder.and_then(Path::file_name);
        let owner = folder.and_then(Path::parent).and_then(Path::file_name);
        match (owner, name) {
            (Some(owner), Some(name))
                if root.file_name().is_some_and(|last| last == "checkout") =>
            {
                Repo::parse(&format!(
                    "{}/{}",
                    owner.to_string_lossy(),
                    name.to_string_lossy()
                ))
                .unwrap_or_else(|_| self.origin.clone())
            }
            _ => self.origin.clone(),
        }
    }

    fn scripted(what: &str) -> String {
        format!("scripted host: {what} is not available")
    }
}

impl System for ScriptedSystem {
    fn locate(&self, program: &str) -> Option<PathBuf> {
        std::thread::sleep(self.delay);
        matches!(program, "git" | "claude" | "codex")
            .then(|| PathBuf::from(format!("/scripted/bin/{program}")))
    }

    fn run(&self, program: &Path, args: &[&str], cwd: Option<&Path>) -> Result<Captured, RunError> {
        let name = program
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        self.note(
            &format!("run {name} {}", args.join(" ")),
            cwd.unwrap_or(Path::new("")),
        );
        let say = |stdout: String| Captured {
            code: Some(0),
            stdout: stdout.into_bytes(),
            stderr: Vec::new(),
        };
        Ok(match (name.as_str(), args) {
            ("git", ["rev-parse", "--show-toplevel"]) => {
                say(format!("{}\n", cwd.unwrap_or(&self.checkout).display()))
            }
            ("git", ["remote", "get-url", "origin"]) => say(format!(
                "git@github.com:{}.git\n",
                self.origin_of(cwd.unwrap_or(&self.checkout))
            )),
            (_, ["--version"]) => say(format!("{name} 1.0.0 (scripted)\n")),
            _ => Captured {
                code: Some(1),
                stdout: Vec::new(),
                stderr: b"scripted host: not available\n".to_vec(),
            },
        })
    }

    fn home(&self) -> Option<PathBuf> {
        Some(PathBuf::from("/home/bench"))
    }

    fn data_dir(&self) -> Option<(PathBuf, DataDirSource)> {
        Some((self.data.clone(), DataDirSource::Platform))
    }

    fn codex_login_file(&self) -> Option<PathBuf> {
        None
    }

    fn is_file(&self, _path: &Path) -> bool {
        false
    }

    fn read_text_file(&self, _path: &Path) -> Result<Option<String>, String> {
        Ok(None)
    }

    fn sandbox(&self) -> Result<(), owlshift_platform::sandbox::SandboxError> {
        Ok(())
    }

    fn secret_stored(&self, _account: &str) -> Result<bool, String> {
        Ok(false)
    }

    fn origin_repo(&self, root: &Path) -> Result<Repo, String> {
        self.note("origin_repo", root);
        Ok(self.origin_of(root))
    }

    fn linear_states(
        &self,
        _accounts: &TrackerAccounts,
        _team: &str,
    ) -> Result<Vec<String>, StatesError> {
        Err(StatesError::Keychain(Self::scripted("the keychain")))
    }

    fn linear_admission(
        &self,
        _accounts: &TrackerAccounts,
        _team: &str,
        _ready: &str,
        _labels: &[&str],
        _people: &[&str],
    ) -> Result<AdmissionFacts, StatesError> {
        Err(StatesError::Keychain(Self::scripted("the keychain")))
    }

    fn linear_app(
        &self,
        _accounts: &TrackerAccounts,
        _team: &str,
    ) -> Result<Option<AppUser>, AppError> {
        Err(AppError::Keychain(Self::scripted("the keychain")))
    }

    fn github_default_branch(&self, _repo: &Repo) -> Result<DefaultBranch, TokenTryError> {
        Err(TokenTryError::Credential(Self::scripted("the keychain")))
    }

    fn github_app_grants(
        &self,
        _repo: &Repo,
        _slug: &str,
    ) -> Result<owlshift_runner::forge::AppGrants, TokenTryError> {
        Err(TokenTryError::Credential(Self::scripted("the keychain")))
    }

    fn ticket_refs(&self, root: &Path) -> Result<Option<Vec<(TicketId, RefState)>>, String> {
        self.note("ticket_refs", root);
        Ok(None)
    }

    fn quarantine(&self, root: &Path) -> Result<MarkerState, String> {
        self.note("quarantine", root);
        Ok(MarkerState::default())
    }

    fn default_branch_file(&self, root: &Path) -> Result<DefaultBranchFile, String> {
        self.note("default_branch_file", root);
        Ok(DefaultBranchFile::Unknown(Self::scripted("origin/HEAD")))
    }

    fn service(&self) -> Option<owlshift_runner::install::ServiceState> {
        None
    }

    fn ui(&self, _data: &Path, _port: u16) -> Option<owlshift_runner::ui::probe::Probe> {
        None
    }

    fn running_builds(&self) -> Option<Result<RunningBuilds, String>> {
        None
    }

    #[cfg(unix)]
    fn sentinel(&self) -> SentinelStatus {
        SentinelStatus::NotRunning
    }

    #[cfg(unix)]
    fn sentinel_probe(&self) -> SentinelProbe {
        SentinelProbe::Works {
            elapsed: Duration::ZERO,
        }
    }
}

#[cfg(test)]
mod tests {
    /// The names of the methods of the `System` trait, in the runner's file.
    fn trait_methods() -> Vec<String> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../owlshift-runner/src/system.rs");
        // A Windows checkout may carry CRLF line ends.
        let text = std::fs::read_to_string(path).unwrap().replace("\r\n", "\n");
        let start = text.find("pub trait System {").expect("the trait");
        let body = &text[start..];
        let end = body.find("\n}\n").expect("its end");
        body[..end]
            .lines()
            .filter_map(|line| line.strip_prefix("    fn "))
            .filter_map(|rest| rest.split(['(', '<']).next())
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn the_scripted_host_overrides_every_method_of_the_system_trait() {
        let own = include_str!("system.rs");
        let methods = trait_methods();
        assert!(methods.len() >= 20, "{methods:?}");
        for method in methods {
            assert!(
                own.contains(&format!("    fn {method}(")),
                "ScriptedSystem does not override `{method}`"
            );
        }
    }
}
