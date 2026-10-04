//! The runner's view of the host: finding programs and running them. Behind
//! a trait so that checks can be tested without the real CLIs installed.

use std::path::{Path, PathBuf};
use std::time::Duration;

pub use owlshift_platform::paths::DataDirSource;
pub use owlshift_platform::process::{Captured, RunError};
#[cfg(unix)]
pub use owlshift_platform::process::{SentinelProbe, SentinelStatus};
use owlshift_platform::sandbox::SandboxError;

pub use crate::tracker::StatesError;

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
    /// The names of the workflow states of the Linear team `team`, read
    /// over the network with the Linear API key in the system keychain
    /// (OWL-147). Only `owlshift doctor` asks, and only for a Linear project.
    fn linear_states(&self, team: &str) -> Result<Vec<String>, StatesError> {
        owlshift_platform::keychain::Keychain::system()
            .map_err(|error| StatesError::Keychain(error.to_string()))
            .and_then(|keychain| crate::tracker::linear_team_states(&keychain, team))
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
        /// How the sentinel is; `None`: it runs.
        #[cfg(unix)]
        sentinel: Option<SentinelStatus>,
        /// How the sentinel probe ends; `None`: the test sentinel works.
        #[cfg(unix)]
        sentinel_probe: Option<SentinelProbe>,
    }

    impl FakeSystem {
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

        fn secret_stored(&self, account: &str) -> Result<bool, String> {
            match &self.keychain_error {
                Some(reason) => Err(reason.clone()),
                None => Ok(!self.unstored.iter().any(|unstored| unstored == account)),
            }
        }

        /// The key's presence follows [`Self::unstored`] and
        /// [`Self::keychain_fails`]; never the network.
        fn linear_states(&self, _team: &str) -> Result<Vec<String>, StatesError> {
            use owlshift_adapters::tracker::{Error, ErrorKind};
            match self.secret_stored(crate::tracker::LINEAR_ACCOUNT) {
                Err(reason) => Err(StatesError::Keychain(reason)),
                Ok(false) => Err(StatesError::NoKey),
                Ok(true) => self
                    .linear_states
                    .clone()
                    .unwrap_or_else(|| {
                        Err(Error::new(
                            ErrorKind::Other,
                            "Linear: no network in tests",
                        ))
                    })
                    .map_err(StatesError::Tracker),
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
}
