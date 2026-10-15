//! The runner's view of the host: finding programs and running them. Behind
//! a trait so that checks can be tested without the real CLIs installed.

use std::path::{Path, PathBuf};
use std::time::Duration;

use owlshift_contracts::ids::TicketId;
pub use owlshift_platform::paths::DataDirSource;
pub use owlshift_platform::process::{Captured, RunError};
#[cfg(unix)]
pub use owlshift_platform::process::{SentinelProbe, SentinelStatus};
use owlshift_platform::sandbox::SandboxError;

use owlshift_adapters::forge::Repo;

use crate::secrets::TrackerAccounts;
use crate::ticket_ref::{self, RefState};
pub use crate::tracker::{AppError, StatesError};
pub use owlshift_adapters::tracker::linear::{AccountFact, AdmissionFacts, AppUser};

/// How long a probe such as `git --version` may take. C8 measured about
/// 0.1 s for the harness status commands.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a test sentinel may take to stop its test process group once
/// its input ended (OWL-90): the bound the OWL-86 tests check, within a
/// probe's deadline. It takes a few milliseconds.
pub const SENTINEL_PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const _: () = assert!(SENTINEL_PROBE_TIMEOUT.as_millis() < PROBE_TIMEOUT.as_millis());

pub trait System {
    /// Finds a program on the `PATH`.
    fn locate(&self, program: &str) -> Option<PathBuf>;
    /// Runs a program with no input, within [`PROBE_TIMEOUT`].
    fn run(&self, program: &Path, args: &[&str], cwd: Option<&Path>) -> Result<Captured, RunError>;
    /// The user's home folder, which reports write `~` (OWL-99).
    fn home(&self) -> Option<PathBuf> {
        owlshift_platform::paths::home_dir()
    }
    /// The data directory and where it comes from, `None` when the system
    /// has none (OWL-109). Only the path: the folder is never opened.
    fn data_dir(&self) -> Option<(PathBuf, DataDirSource)> {
        owlshift_platform::paths::data_dir_and_source()
    }
    /// The file a Codex agent run copies its login from (OWL-299), as
    /// `executor::codex::user_login` resolves it: `auth.json` in
    /// `CODEX_HOME`, or in `~/.codex`.
    fn codex_login_file(&self) -> Option<PathBuf> {
        crate::executor::codex::login_file(
            std::env::var_os("CODEX_HOME").as_deref(),
            self.home().as_deref(),
        )
    }
    /// Whether `path` is a regular file, links followed.
    fn is_file(&self, path: &Path) -> bool {
        path.is_file()
    }
    /// Whether agent runs can be confined here (OWL-41).
    fn sandbox(&self) -> Result<(), SandboxError> {
        owlshift_platform::sandbox::available()
    }
    /// Whether the system keychain holds a secret for `account`, under
    /// Owlshift's service: its presence only, never its value. The error
    /// says why the keychain could not be read.
    fn secret_stored(&self, account: &str) -> Result<bool, String> {
        owlshift_platform::keychain::Keychain::system()
            .and_then(|keychain| keychain.contains(account))
            .map_err(|error| error.to_string())
    }
    /// The GitHub repository of the `origin` remote of the checkout at
    /// `root`, read as `owlshift do` reads it: what names a project's
    /// secrets (OWL-308).
    fn origin_repo(&self, root: &Path) -> Result<Repo, String> {
        crate::config::origin_repo(self, root)
    }
    /// The names of the workflow states of the Linear team `team`, read
    /// over the network with the Linear API key stored under `accounts`
    /// (OWL-147). Only `owlshift doctor` asks, and only for a Linear project.
    fn linear_states(
        &self,
        accounts: &TrackerAccounts,
        team: &str,
    ) -> Result<Vec<String>, StatesError> {
        owlshift_platform::keychain::Keychain::system()
            .map_err(|error| StatesError::Keychain(error.to_string()))
            .and_then(|keychain| crate::tracker::linear_team_states(&keychain, accounts, team))
    }
    /// What the admission and proposals setup of the Linear team `team`
    /// looks like (OWL-155): which of `labels` the team can carry, which of
    /// `people` the workspace knows, and where a proposal would be filed,
    /// given the ready state `ready`. Read over the network with the key
    /// stored under `accounts`. Only `owlshift doctor` asks, and only for a
    /// Linear project.
    fn linear_admission(
        &self,
        accounts: &TrackerAccounts,
        team: &str,
        ready: &str,
        labels: &[&str],
        people: &[&str],
    ) -> Result<AdmissionFacts, StatesError> {
        owlshift_platform::keychain::Keychain::system()
            .map_err(|error| StatesError::Keychain(error.to_string()))
            .and_then(|keychain| {
                crate::tracker::linear_admission(&keychain, accounts, team, ready, labels, people)
            })
    }
    /// The Linear app user Owlshift writes as, with the credentials stored
    /// under `accounts`, checked over the network as `owlshift do` checks it
    /// for team `team`, its token revoked after: `None` when no app is
    /// stored (OWL-157). Only `owlshift doctor` asks, and only for a Linear
    /// project.
    fn linear_app(
        &self,
        accounts: &TrackerAccounts,
        team: &str,
    ) -> Result<Option<AppUser>, AppError> {
        owlshift_platform::keychain::Keychain::system()
            .map_err(|error| AppError::Keychain(error.to_string()))
            .and_then(|keychain| crate::tracker::linear_app_user(&keychain, accounts, team))
    }
    /// The ticket refs of the project whose repository is at `root`, each
    /// with its state ([`ticket_ref::states`]), read in its dedicated
    /// checkout as `owlshift forget` finds it, without the project's lock:
    /// `None` when the project has no dedicated checkout yet. The error says
    /// why they could not be read, in Owlshift's words only (OWL-291). Only
    /// `owlshift doctor` asks.
    fn ticket_refs(&self, root: &Path) -> Result<Option<Vec<(TicketId, RefState)>>, String> {
        let git = crate::project::runner_git();
        let repo = crate::on_demand::check_origin(&crate::project::origin_url(&git, root)?)?;
        let (data_dir, _) = self
            .data_dir()
            .ok_or_else(|| "this system has no data directory".to_owned())?;
        let checkout = crate::project::ProjectDirs::new(&data_dir, &repo).checkout();
        if !checkout_exists(&checkout)? {
            return Ok(None);
        }
        // git's own words stay out of the report (live check C8).
        ticket_ref::states(&git, &checkout)
            .map(Some)
            .map_err(|_| "git could not list them in the dedicated checkout".to_owned())
    }
    /// Whether the sentinel still protects the live process trees from a
    /// hard kill of Owlshift (OWL-86, OWL-88, OWL-91).
    #[cfg(unix)]
    fn sentinel(&self) -> SentinelStatus {
        owlshift_platform::process::sentinel_status()
    }
    /// Whether a sentinel stops a process group it was told of when its
    /// input ends, tried on a test sentinel and a test group (OWL-90).
    #[cfg(unix)]
    fn sentinel_probe(&self) -> SentinelProbe {
        owlshift_platform::process::probe_sentinel(SENTINEL_PROBE_TIMEOUT)
    }
}

/// Whether the dedicated checkout at `checkout` exists: only its absence is
/// `false`; any other failure to look is an error, so a ref that holds the
/// slot is never hidden as no checkout (OWL-291).
fn checkout_exists(checkout: &Path) -> Result<bool, String> {
    match std::fs::symlink_metadata(checkout) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(_) => Err("the dedicated checkout could not be inspected".to_owned()),
    }
}

/// The machine Owlshift runs on.
pub struct HostSystem;

impl System for HostSystem {
    fn locate(&self, program: &str) -> Option<PathBuf> {
        owlshift_platform::process::find_executable(program)
    }

    fn run(&self, program: &Path, args: &[&str], cwd: Option<&Path>) -> Result<Captured, RunError> {
        owlshift_platform::process::run(program, args, cwd, PROBE_TIMEOUT)
    }
}

/// The first version number in a program's `--version` output, rebuilt from
/// its digits and dots only, so nothing else the program printed can pass.
///
/// `git version 2.54.0 (Apple Git-157)`, `2.1.283 (Claude Code)` and
/// `codex-cli 0.154.0` give `2.54.0`, `2.1.283` and `0.154.0`.
pub(crate) fn version_of(output: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(output);
    Some(rebuild(version_token(&text)?))
}

/// [`version_of`], only when the version holds nothing but digits and dots:
/// `2.1.283-beta.1` gives `None`, since it is not the release `2.1.283`.
pub(crate) fn exact_version_of(output: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(output);
    let token = version_token(&text)?;
    let version = rebuild(token);
    (version == token).then_some(version)
}

/// Whether `version`, as [`version_of`] gives it, is older than `floor`.
/// Compared part by part as numbers, a missing part counting as 0, so
/// `2.39` is `2.39.0` and `2.100.0` is newer than `2.39.0`. `None` when a
/// part is not a number that fits: the version cannot be compared.
pub(crate) fn older_than(version: &str, floor: &str) -> Option<bool> {
    let parts = |version: &str| {
        version
            .split('.')
            .map(|part| part.parse::<u64>().ok())
            .collect::<Option<Vec<u64>>>()
    };
    let (mut version, mut floor) = (parts(version)?, parts(floor)?);
    let len = version.len().max(floor.len());
    version.resize(len, 0);
    floor.resize(len, 0);
    Some(version < floor)
}

fn version_token(text: &str) -> Option<&str> {
    text.split_whitespace()
        .find(|token| token.starts_with(|c: char| c.is_ascii_digit()))
}

fn rebuild(token: &str) -> String {
    let mut version = String::new();
    for part in token.split('.') {
        let digits: String = part.chars().take_while(char::is_ascii_digit).collect();
        if digits.is_empty() {
            break;
        }
        if !version.is_empty() {
            version.push('.');
        }
        version.push_str(&digits);
        if digits.len() != part.len() {
            break;
        }
    }
    version
}

#[cfg(test)]
pub(crate) mod fake {
    //! A scripted host for tests.

    use std::collections::HashMap;

    use super::*;

    pub(crate) enum Answer {
        Exit(i32, &'static str, &'static str),
        TimedOut,
    }

    #[derive(Default)]
    pub(crate) struct FakeSystem {
        located: HashMap<String, PathBuf>,
        answers: HashMap<String, Answer>,
        /// The data directory; `None`: the platform's, `~/.local/share/owlshift`.
        data_dir: Option<Option<(PathBuf, DataDirSource)>>,
        /// Why agent runs cannot be confined; `None`: they can.
        sandbox: Option<SandboxError>,
        /// Keychain accounts with no secret; every other one has one.
        unstored: Vec<String>,
        /// Why the keychain cannot be read; `None`: it can.
        keychain_error: Option<String>,
        /// What Linear answers for a team's workflow states, the key being
        /// stored; `None`: it cannot be reached.
        linear_states: Option<Result<Vec<String>, owlshift_adapters::tracker::Error>>,
        /// What Linear answers for a team's admission facts, the key being
        /// stored; `None`: it cannot be reached.
        linear_admission: Option<Result<AdmissionFacts, owlshift_adapters::tracker::Error>>,
        /// What checking the Linear app gives, both of its entries being
        /// stored; `None`: no app is stored.
        linear_app: Option<Result<String, owlshift_adapters::tracker::Error>>,
        /// The repository `origin` names; `None`: `pitchopp/owlshift`.
        origin: Option<Result<Repo, String>>,
        /// What reading the project's ticket refs gives; `None`: no
        /// dedicated checkout.
        ticket_refs: Option<Result<Vec<(TicketId, RefState)>, String>>,
        /// Files that are not there; every other one is.
        missing_files: Vec<PathBuf>,
        /// How the sentinel is; `None`: it runs.
        #[cfg(unix)]
        sentinel: Option<SentinelStatus>,
        /// How the sentinel probe ends; `None`: the test sentinel works.
        #[cfg(unix)]
        sentinel_probe: Option<SentinelProbe>,
    }

    impl FakeSystem {
        /// There is no file at `path`.
        pub(crate) fn without_file(mut self, path: &str) -> Self {
            self.missing_files.push(PathBuf::from(path));
            self
        }

        /// Agent runs cannot be confined, for this reason.
        pub(crate) fn no_sandbox(mut self, error: SandboxError) -> Self {
            self.sandbox = Some(error);
            self
        }

        /// The data directory is so, or the system has none.
        #[cfg(unix)]
        pub(crate) fn data_dir_is(mut self, dir: Option<(PathBuf, DataDirSource)>) -> Self {
            self.data_dir = Some(dir);
            self
        }

        /// The sentinel is so.
        #[cfg(unix)]
        pub(crate) fn sentinel_is(mut self, status: SentinelStatus) -> Self {
            self.sentinel = Some(status);
            self
        }

        /// The sentinel probe ends so.
        #[cfg(unix)]
        pub(crate) fn sentinel_probe_is(mut self, probe: SentinelProbe) -> Self {
            self.sentinel_probe = Some(probe);
            self
        }

        /// No secret is stored for this keychain account.
        pub(crate) fn unstored(mut self, account: &str) -> Self {
            self.unstored.push(account.to_owned());
            self
        }

        /// The project's own tracker and forge accounts hold nothing, so
        /// its secrets come from the shared accounts (OWL-308).
        pub(crate) fn only_shared(mut self) -> Self {
            let repo = Repo::parse("pitchopp/owlshift").unwrap();
            let own = crate::secrets::TrackerAccounts::project(&repo);
            self.unstored.extend([
                own.key,
                own.app_id,
                own.app_secret,
                crate::secrets::ForgeAccount::project(&repo).token,
            ]);
            self
        }

        /// The repository `origin` names, or why it names none.
        pub(crate) fn origin_is(mut self, origin: Result<&str, &str>) -> Self {
            self.origin = Some(
                origin
                    .map(|repo| Repo::parse(repo).unwrap())
                    .map_err(str::to_owned),
            );
            self
        }

        /// The keychain cannot be read, for this reason.
        pub(crate) fn keychain_fails(mut self, reason: &str) -> Self {
            self.keychain_error = Some(reason.to_owned());
            self
        }

        /// Linear answers so for a team's workflow states.
        pub(crate) fn linear_states_are(
            mut self,
            answer: Result<&[&str], owlshift_adapters::tracker::Error>,
        ) -> Self {
            let answer = answer.map(|names| names.iter().map(|&n| n.to_owned()).collect());
            self.linear_states = Some(answer);
            self
        }

        /// Linear answers so for a team's admission facts.
        pub(crate) fn linear_admission_is(
            mut self,
            answer: Result<AdmissionFacts, owlshift_adapters::tracker::Error>,
        ) -> Self {
            self.linear_admission = Some(answer);
            self
        }

        /// Checking the Linear app gives its user's name, or this error.
        pub(crate) fn linear_app_is(
            mut self,
            answer: Result<&str, owlshift_adapters::tracker::Error>,
        ) -> Self {
            self.linear_app = Some(answer.map(str::to_owned));
            self
        }

        /// Reading the project's ticket refs gives these, or this error.
        pub(crate) fn ticket_refs_are(
            mut self,
            answer: Result<Vec<(TicketId, RefState)>, String>,
        ) -> Self {
            self.ticket_refs = Some(answer);
            self
        }

        /// Puts a program on the fake `PATH`.
        pub(crate) fn install(mut self, program: &str) -> Self {
            self.located.insert(
                program.to_owned(),
                PathBuf::from(format!("/fake/bin/{program}")),
            );
            self
        }

        /// Scripts the answer to `program args…`, written as one string.
        pub(crate) fn answer(mut self, command: &str, answer: Answer) -> Self {
            self.answers.insert(command.to_owned(), answer);
            self
        }
    }

    impl System for FakeSystem {
        fn locate(&self, program: &str) -> Option<PathBuf> {
            self.located.get(program).cloned()
        }

        fn run(
            &self,
            program: &Path,
            args: &[&str],
            _cwd: Option<&Path>,
        ) -> Result<Captured, RunError> {
            let name = program.file_name().unwrap().to_string_lossy();
            let command = std::iter::once(name.as_ref())
                .chain(args.iter().copied())
                .collect::<Vec<_>>()
                .join(" ");
            match self.answers.get(&command) {
                Some(Answer::Exit(code, stdout, stderr)) => Ok(Captured {
                    code: Some(*code),
                    stdout: stdout.as_bytes().to_vec(),
                    stderr: stderr.as_bytes().to_vec(),
                }),
                Some(Answer::TimedOut) => Err(RunError::TimedOut),
                None => panic!("unscripted command: {command}"),
            }
        }

        fn home(&self) -> Option<PathBuf> {
            Some(PathBuf::from("/home/ada"))
        }

        fn codex_login_file(&self) -> Option<PathBuf> {
            Some(PathBuf::from("/home/ada/.codex/auth.json"))
        }

        fn is_file(&self, path: &Path) -> bool {
            !self.missing_files.iter().any(|missing| missing == path)
        }

        fn data_dir(&self) -> Option<(PathBuf, DataDirSource)> {
            self.data_dir.clone().unwrap_or_else(|| {
                Some((
                    PathBuf::from("/home/ada/.local/share/owlshift"),
                    DataDirSource::Platform,
                ))
            })
        }

        fn sandbox(&self) -> Result<(), SandboxError> {
            self.sandbox.clone().map_or(Ok(()), Err)
        }

        /// No dedicated checkout unless [`Self::ticket_refs_are`] gave an
        /// answer; never git.
        fn ticket_refs(&self, _root: &Path) -> Result<Option<Vec<(TicketId, RefState)>>, String> {
            self.ticket_refs.clone().transpose()
        }

        fn secret_stored(&self, account: &str) -> Result<bool, String> {
            match &self.keychain_error {
                Some(reason) => Err(reason.clone()),
                None => Ok(!self.unstored.iter().any(|unstored| unstored == account)),
            }
        }

        fn origin_repo(&self, _root: &Path) -> Result<Repo, String> {
            self.origin
                .clone()
                .unwrap_or_else(|| Ok(Repo::parse("pitchopp/owlshift").unwrap()))
        }

        /// The key's presence follows [`Self::unstored`] and
        /// [`Self::keychain_fails`]; never the network.
        fn linear_states(
            &self,
            accounts: &TrackerAccounts,
            _team: &str,
        ) -> Result<Vec<String>, StatesError> {
            use owlshift_adapters::tracker::{Error, ErrorKind};
            match self.secret_stored(&accounts.key) {
                Err(reason) => Err(StatesError::Keychain(reason)),
                Ok(false) => Err(StatesError::NoKey),
                Ok(true) => self
                    .linear_states
                    .clone()
                    .unwrap_or_else(|| {
                        Err(Error::new(ErrorKind::Other, "Linear: no network in tests"))
                    })
                    .map_err(StatesError::Tracker),
            }
        }

        /// Follows [`Self::linear_states`]'s key rules; never the network.
        /// Only the labels and accounts the answer holds come back.
        fn linear_admission(
            &self,
            accounts: &TrackerAccounts,
            _team: &str,
            _ready: &str,
            labels: &[&str],
            people: &[&str],
        ) -> Result<AdmissionFacts, StatesError> {
            use owlshift_adapters::tracker::{Error, ErrorKind};
            match self.secret_stored(&accounts.key) {
                Err(reason) => Err(StatesError::Keychain(reason)),
                Ok(false) => Err(StatesError::NoKey),
                Ok(true) => self
                    .linear_admission
                    .clone()
                    .unwrap_or_else(|| {
                        Err(Error::new(ErrorKind::Other, "Linear: no network in tests"))
                    })
                    .map(|mut facts| {
                        facts.labels.retain(|l| labels.contains(&l.as_str()));
                        facts.accounts.retain(|a| people.contains(&a.id.as_str()));
                        facts
                    })
                    .map_err(StatesError::Tracker),
            }
        }

        /// No app unless [`Self::linear_app_is`] gave one; then its entries
        /// follow [`Self::unstored`] and [`Self::keychain_fails`]. Never the
        /// network.
        fn linear_app(
            &self,
            accounts: &TrackerAccounts,
            _team: &str,
        ) -> Result<Option<AppUser>, AppError> {
            let Some(answer) = self.linear_app.clone() else {
                return Ok(None);
            };
            let stored = |account| self.secret_stored(account).map_err(AppError::Keychain);
            let incomplete = |missing: &str| AppError::Incomplete {
                missing: missing.to_owned(),
                source: accounts.source,
            };
            match (stored(&accounts.app_id)?, stored(&accounts.app_secret)?) {
                (false, false) => Ok(None),
                (true, false) => Err(incomplete(&accounts.app_secret)),
                (false, true) => Err(incomplete(&accounts.app_id)),
                (true, true) => {
                    answer
                        .map(|name| Some(AppUser { name }))
                        .map_err(|error| AppError::Tracker {
                            error,
                            accounts: accounts.clone(),
                        })
                }
            }
        }

        #[cfg(unix)]
        fn sentinel(&self) -> SentinelStatus {
            self.sentinel
                .clone()
                .unwrap_or(SentinelStatus::Running { pid: 4242 })
        }

        #[cfg(unix)]
        fn sentinel_probe(&self) -> SentinelProbe {
            self.sentinel_probe.clone().unwrap_or(SentinelProbe::Works {
                elapsed: Duration::from_millis(3),
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// OWL-291: only an absent checkout is no checkout; a folder that cannot
    /// be looked into is an error, in Owlshift's words.
    #[test]
    fn only_an_absent_checkout_is_none() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(checkout_exists(dir.path()), Ok(true));
        assert_eq!(checkout_exists(&dir.path().join("checkout")), Ok(false));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let locked = dir.path().join("locked");
            std::fs::create_dir(&locked).unwrap();
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
            let looked = checkout_exists(&locked.join("checkout"));
            // A superuser looks through any folder: nothing to check then.
            let superuser = std::fs::read_dir(&locked).is_ok();
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
            if !superuser {
                assert_eq!(
                    looked,
                    Err("the dedicated checkout could not be inspected".to_owned())
                );
            }
        }
    }

    #[test]
    fn versions_are_rebuilt_from_digits() {
        assert_eq!(
            version_of(b"git version 2.54.0 (Apple Git-157)\n").as_deref(),
            Some("2.54.0")
        );
        assert_eq!(
            version_of(b"2.1.283 (Claude Code)\n").as_deref(),
            Some("2.1.283")
        );
        assert_eq!(
            version_of(b"codex-cli 0.154.0\n").as_deref(),
            Some("0.154.0")
        );
        assert_eq!(
            version_of(b"123@example.com 4.5-sk-x").as_deref(),
            Some("123")
        );
        assert_eq!(version_of(b"no version here"), None);
    }

    #[test]
    fn an_exact_version_has_no_suffix() {
        assert_eq!(
            exact_version_of(b"2.1.283 (Claude Code)\n").as_deref(),
            Some("2.1.283")
        );
        assert_eq!(exact_version_of(b"2.1.283-beta.1 (Claude Code)\n"), None);
        assert_eq!(exact_version_of(b"no version here"), None);
    }

    #[test]
    fn versions_compare_part_by_part_as_numbers() {
        assert_eq!(older_than("2.38.5", "2.39.0"), Some(true));
        assert_eq!(older_than("2.39", "2.39.0"), Some(false));
        assert_eq!(older_than("2.39.0", "2.39.0"), Some(false));
        assert_eq!(older_than("2.100.0", "2.39.0"), Some(false));
        assert_eq!(older_than("1.99.99.1", "2.39.0"), Some(true));
        assert_eq!(older_than("2.99999999999999999999.0", "2.39.0"), None);
    }
}
