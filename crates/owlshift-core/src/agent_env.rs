//! The environment an agent process gets: none of the operator's tracker,
//! forge or cloud credentials (architecture section 8, OWL-22).
//!
//! An agent, a harness CLI and every command it runs from its shell, starts
//! from an empty environment. It inherits only what a program needs to run
//! and what its harness needs to find its own login ([`INHERITED`]), plus the
//! variables the project declares for its gate; then [`OVERRIDES`] leave git
//! and gh without a credential. Each override rests on a live check recorded
//! in the build plan (results, "OWL-22").
//!
//! This closes what an agent reaches without going around Owlshift. A process
//! that goes looking in the operator's files or keychain is stopped by the
//! operating system's sandbox the runner wraps every agent run in (OWL-41,
//! `owlshift_platform::sandbox`).
//!
//! The runner reads its own environment, applies the result to the command
//! it spawns and probes it (`owlshift_runner::agent_env`).

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fmt;

use crate::floor::{self, FloorViolation};

/// The value Owlshift gives a token variable instead of a credential. A tool
/// that reads it fails to authenticate, rather than falling back to a login
/// it finds elsewhere, such as gh's in the system keyring.
pub const NO_CREDENTIAL: &str = "owlshift-agent-has-no-credential";

/// The variables an agent inherits from the runner, when they are set. Names
/// compare ASCII case-insensitively, as on Windows.
///
/// Left out on purpose, among others: every token variable, `SSH_AUTH_SOCK`,
/// every `GIT_*` variable, `XDG_RUNTIME_DIR`, `DBUS_SESSION_BUS_ADDRESS` and
/// `DISPLAY` (they locate the Secret Service), and the variables a parent
/// Claude Code session sets (`CLAUDECODE`, `ANTHROPIC_*`, `CLAUDE_*` but the
/// configuration directory).
pub const INHERITED: &[&str] = &[
    // Running programs.
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "TERM",
    "TZ",
    "TMPDIR",
    "LANG",
    "LANGUAGE",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_CACHE_HOME",
    "XDG_STATE_HOME",
    // Running programs on Windows.
    "SYSTEMROOT",
    "SYSTEMDRIVE",
    "WINDIR",
    "COMSPEC",
    "PATHEXT",
    "TEMP",
    "TMP",
    "USERPROFILE",
    "USERNAME",
    "USERDOMAIN",
    "HOMEDRIVE",
    "HOMEPATH",
    "APPDATA",
    "LOCALAPPDATA",
    "PROGRAMDATA",
    "PROGRAMFILES",
    "PROGRAMFILES(X86)",
    "PROGRAMW6432",
    "COMMONPROGRAMFILES",
    "COMMONPROGRAMFILES(X86)",
    "COMMONPROGRAMW6432",
    "ALLUSERSPROFILE",
    "PUBLIC",
    "OS",
    "PROCESSOR_ARCHITECTURE",
    "NUMBER_OF_PROCESSORS",
    "COMPUTERNAME",
    // Where each harness keeps its own login.
    "CLAUDE_CONFIG_DIR",
    "CODEX_HOME",
    // Reaching the network through the operator's proxy and certificates.
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "NO_PROXY",
    "ALL_PROXY",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "NODE_EXTRA_CA_CERTS",
];

/// Prefixes of inherited names: the locale categories.
pub const INHERITED_PREFIXES: &[&str] = &["LC_"];

/// Set on every agent, after everything else. The build plan records the
/// live check behind each one (results, "OWL-22").
pub const OVERRIDES: &[(&str, &str)] = &[
    // gh reads these before its configuration and the system keyring; an
    // empty configuration directory does not stop the keyring fallback.
    ("GH_TOKEN", NO_CREDENTIAL),
    ("GH_ENTERPRISE_TOKEN", NO_CREDENTIAL),
    // No git credential helper: an empty `credential.helper` in the command
    // scope, read last, resets the list, helpers scoped to a URL included.
    ("GIT_CONFIG_COUNT", "1"),
    ("GIT_CONFIG_KEY_0", "credential.helper"),
    ("GIT_CONFIG_VALUE_0", ""),
    // No askpass program: an empty `GIT_ASKPASS` makes git skip
    // `core.askPass` and `SSH_ASKPASS` too. No terminal prompt either.
    ("GIT_ASKPASS", ""),
    ("GIT_TERMINAL_PROMPT", "0"),
    // SSH for git offers no key, no agent and no Kerberos ticket, and never
    // prompts. An ssh that rejects an option refuses to run: still no login.
    (
        "GIT_SSH_COMMAND",
        "ssh -o BatchMode=yes -o IdentityAgent=none -o PubkeyAuthentication=no \
         -o GSSAPIAuthentication=no",
    ),
];

/// Why an agent environment cannot be built.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AgentEnvError {
    /// Declared variables that carry a credential.
    Floor(FloorViolation),
    /// A declared variable that would change how git or gh authenticate: a
    /// `GIT_` name or an override.
    Reserved(String),
    /// A declared variable that makes the dynamic loader load code, such as
    /// `LD_PRELOAD` or a `DYLD_` name: the sandbox program would load it
    /// before the sandbox applies (OWL-41).
    Loader(String),
}

impl fmt::Display for AgentEnvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Floor(violation) => violation.fmt(f),
            Self::Reserved(name) => write!(
                f,
                "{name} cannot be passed to an agent: Owlshift sets how git and gh \
                 authenticate there"
            ),
            Self::Loader(name) => write!(
                f,
                "{name} cannot be passed to an agent: it makes the dynamic loader load \
                 code into the sandbox program before the sandbox applies"
            ),
        }
    }
}

/// Names that make the dynamic loader load code into a program: they never
/// reach an agent. Every `DYLD_` name is refused too.
pub const LOADER_VARIABLES: &[&str] = &["LD_PRELOAD", "LD_AUDIT", "LD_LIBRARY_PATH"];

impl std::error::Error for AgentEnvError {}

/// The environment of an agent, sorted by name: the [`INHERITED`] variables
/// of `parent` and those named in `declared`, then the [`OVERRIDES`].
///
/// `parent` is the runner's own environment; `declared` names the variables
/// the project declares for its gate. A declared credential variable, or one
/// that is a `GIT_` name or an override, is refused, in any letter case. A
/// declared variable absent from `parent` is simply not set.
pub fn agent_environment(
    parent: impl IntoIterator<Item = (OsString, OsString)>,
    declared: &[&str],
) -> Result<Vec<(OsString, OsString)>, AgentEnvError> {
    floor::check_agent_environment(declared.iter().copied()).map_err(AgentEnvError::Floor)?;
    if let Some(name) = declared.iter().find(|name| is_reserved(name)) {
        return Err(AgentEnvError::Reserved((*name).to_owned()));
    }
    if let Some(name) = declared.iter().find(|name| is_loader(name)) {
        return Err(AgentEnvError::Loader((*name).to_owned()));
    }
    let mut vars = BTreeMap::new();
    for (name, value) in parent {
        let kept = name.to_str().is_some_and(|name| {
            is_inherited(name) || declared.iter().any(|d| d.eq_ignore_ascii_case(name))
        });
        if kept {
            vars.insert(name, value);
        }
    }
    for (name, value) in OVERRIDES {
        vars.insert(OsString::from(name), OsString::from(value));
    }
    Ok(vars.into_iter().collect())
}

/// Checks an environment an agent would get: every credential variable in
/// it is reported, except a token variable set to [`NO_CREDENTIAL`].
///
/// This is the check for a whole environment, such as the one
/// [`agent_environment`] builds, whose gh token variables hold
/// [`NO_CREDENTIAL`]; [`floor::check_agent_environment`] checks names alone,
/// for the variables a project declares.
pub fn check_agent_variables<'a>(
    vars: impl IntoIterator<Item = (&'a OsStr, &'a OsStr)>,
) -> Result<(), FloorViolation> {
    let carried: Vec<&str> = vars
        .into_iter()
        .filter(|(_, value)| *value != OsStr::new(NO_CREDENTIAL))
        .filter_map(|(name, _)| name.to_str())
        .collect();
    floor::check_agent_environment(carried)
}

fn is_inherited(name: &str) -> bool {
    INHERITED.iter().any(|kept| kept.eq_ignore_ascii_case(name))
        || INHERITED_PREFIXES
            .iter()
            .any(|prefix| starts_with_ignore_case(name, prefix))
}

fn is_reserved(name: &str) -> bool {
    starts_with_ignore_case(name, "GIT_")
        || OVERRIDES
            .iter()
            .any(|(overridden, _)| overridden.eq_ignore_ascii_case(name))
}

fn is_loader(name: &str) -> bool {
    starts_with_ignore_case(name, "DYLD_")
        || LOADER_VARIABLES
            .iter()
            .any(|loader| loader.eq_ignore_ascii_case(name))
}

fn starts_with_ignore_case(name: &str, prefix: &str) -> bool {
    name.get(..prefix.len())
        .is_some_and(|start| start.eq_ignore_ascii_case(prefix))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
        pairs
            .iter()
            .map(|(name, value)| (OsString::from(name), OsString::from(value)))
            .collect()
    }

    #[test]
    fn an_agent_inherits_the_listed_variables_and_gets_the_overrides() {
        let parent = env(&[
            ("PATH", "/bin"),
            ("HOME", "/home/op"),
            ("LC_CTYPE", "UTF-8"),
            ("SystemRoot", "C:\\Windows"),
            ("CLAUDE_CONFIG_DIR", "/home/op/.claude"),
            ("DATABASE_URL", "postgres://localhost/test"),
            ("NODE_ENV", "test"),
            ("LINEAR_API_KEY", "lin_api_x"),
            ("GH_TOKEN", "gho_x"),
            ("SSH_AUTH_SOCK", "/tmp/agent.sock"),
            ("GIT_DIR", "/elsewhere"),
            ("GIT_CONFIG_PARAMETERS", "'credential.helper'='store'"),
            ("XDG_RUNTIME_DIR", "/run/user/1000"),
            ("DBUS_SESSION_BUS_ADDRESS", "unix:path=/run/user/1000/bus"),
            ("CLAUDECODE", "1"),
            ("ANTHROPIC_BASE_URL", "http://127.0.0.1:1"),
        ]);
        let agent = agent_environment(parent, &["database_url"]).unwrap();
        let ssh = "ssh -o BatchMode=yes -o IdentityAgent=none -o PubkeyAuthentication=no \
                   -o GSSAPIAuthentication=no";
        let expected = env(&[
            ("CLAUDE_CONFIG_DIR", "/home/op/.claude"),
            ("DATABASE_URL", "postgres://localhost/test"),
            ("GH_ENTERPRISE_TOKEN", NO_CREDENTIAL),
            ("GH_TOKEN", NO_CREDENTIAL),
            ("GIT_ASKPASS", ""),
            ("GIT_CONFIG_COUNT", "1"),
            ("GIT_CONFIG_KEY_0", "credential.helper"),
            ("GIT_CONFIG_VALUE_0", ""),
            ("GIT_SSH_COMMAND", ssh),
            ("GIT_TERMINAL_PROMPT", "0"),
            ("HOME", "/home/op"),
            ("LC_CTYPE", "UTF-8"),
            ("PATH", "/bin"),
            ("SystemRoot", "C:\\Windows"),
        ]);
        assert_eq!(agent, expected);
        let vars = agent.iter().map(|(n, v)| (n.as_os_str(), v.as_os_str()));
        assert_eq!(check_agent_variables(vars), Ok(()));
    }

    #[test]
    fn declared_credentials_and_git_settings_are_refused_in_any_case() {
        for name in [
            "GITHUB_TOKEN",
            "gh_token",
            "Gh_Enterprise_Token",
            "ssh_auth_sock",
        ] {
            assert_eq!(
                agent_environment(Vec::new(), &[name]),
                Err(AgentEnvError::Floor(FloorViolation::CredentialVariables(
                    vec![name.to_owned()]
                ))),
                "{name}"
            );
        }
        for name in [
            "GIT_DIR",
            "git_ssh_command",
            "Git_Askpass",
            "git_config_count",
        ] {
            assert_eq!(
                agent_environment(Vec::new(), &[name]),
                Err(AgentEnvError::Reserved(name.to_owned())),
                "{name}"
            );
        }
        assert_eq!(
            AgentEnvError::Reserved("GIT_ASKPASS".into()).to_string(),
            "GIT_ASKPASS cannot be passed to an agent: Owlshift sets how git and gh \
             authenticate there"
        );
        for name in [
            "LD_PRELOAD",
            "ld_library_path",
            "DYLD_INSERT_LIBRARIES",
            "dyld_x",
        ] {
            assert_eq!(
                agent_environment(Vec::new(), &[name]),
                Err(AgentEnvError::Loader(name.to_owned())),
                "{name}"
            );
        }
    }

    #[test]
    fn only_the_placeholder_token_passes_the_check() {
        let check = |pairs: &[(&str, &str)]| {
            let vars = env(pairs);
            check_agent_variables(vars.iter().map(|(n, v)| (n.as_os_str(), v.as_os_str())))
        };
        assert_eq!(
            check(&[("GH_TOKEN", NO_CREDENTIAL), ("PATH", "/bin")]),
            Ok(())
        );
        assert_eq!(
            check(&[
                ("GH_TOKEN", "gho_x"),
                ("LINEAR_API_KEY", NO_CREDENTIAL),
                ("aws_secret_access_key", "x"),
            ]),
            Err(FloorViolation::CredentialVariables(vec![
                "GH_TOKEN".to_owned(),
                "aws_secret_access_key".to_owned(),
            ]))
        );
    }
}
