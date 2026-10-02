//! The environment an agent process gets: none of the operator's tracker,
//! forge or cloud credentials (architecture section 8, OWL-22).
//!
//! An agent, a harness CLI and every command it runs from its shell, starts
//! from an empty environment. It inherits only what a program needs to run
//! and where a harness other than Claude Code finds its own login
//! ([`INHERITED`]), the
//! locale ([`LOCALE_CATEGORIES`]) and the operator's proxy
//! ([`PROXY_VARIABLES`], refused when it holds a login), plus the variables
//! the project declares for its gate and the operator allows
//! ([`check_declared`]); then [`OVERRIDES`] leave git and gh without a
//! credential. Each override rests on a live check recorded
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
use crate::vocab::Harness;

/// The value Owlshift gives a token variable instead of a credential. A tool
/// that reads it fails to authenticate, rather than falling back to a login
/// it finds elsewhere, such as gh's in the system keyring.
pub const NO_CREDENTIAL: &str = "owlshift-agent-has-no-credential";

/// The variables an agent inherits from the runner, when they are set. Names
/// compare ASCII case-insensitively, as on Windows.
///
/// Left out on purpose, among others: every token variable, `SSH_AUTH_SOCK`,
/// every `GIT_*` variable, `XDG_RUNTIME_DIR`, `DBUS_SESSION_BUS_ADDRESS` and
/// `DISPLAY` (they locate the Secret Service), and every variable of Claude
/// Code (`CLAUDECODE`, `ANTHROPIC_*`, `CLAUDE_*`): a confined Claude Code run
/// gets its login and a configuration folder of its own from the runner, on
/// the harness command alone (OWL-94). Of Codex's own variables (`CODEX_*`,
/// `OPENAI_*`), only `CODEX_HOME` is inherited, so that a Codex run finds the
/// login of the user's Codex configuration. No project declares one of
/// those, `CODEX_HOME` included ([`HARNESS_VARIABLE_PREFIXES`]).
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
    // Where Codex keeps its own login.
    "CODEX_HOME",
    // Reaching the network through the operator's proxy, with the
    // `PROXY_VARIABLES` below, and certificates.
    "NO_PROXY",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "NODE_EXTRA_CA_CERTS",
];

/// The locale categories an agent inherits, POSIX's and glibc's, beside
/// `LANG` and `LANGUAGE` (OWL-76). Another `LC_` name, such as
/// `LC_TERMINAL`, is not inherited.
pub const LOCALE_CATEGORIES: &[&str] = &[
    "LC_ALL",
    "LC_COLLATE",
    "LC_CTYPE",
    "LC_MESSAGES",
    "LC_MONETARY",
    "LC_NUMERIC",
    "LC_TIME",
    "LC_ADDRESS",
    "LC_IDENTIFICATION",
    "LC_MEASUREMENT",
    "LC_NAME",
    "LC_PAPER",
    "LC_TELEPHONE",
];

/// The proxy variables an agent inherits, unless one holds a login (OWL-76):
/// a value holding `@` anywhere refuses the agent environment, whether the
/// variable is inherited or declared. `@` is what ends a login in a URL,
/// percent-encoded or not, with a scheme or without one, as curl reads it.
pub const PROXY_VARIABLES: &[&str] = &["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY"];

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
    /// `LD_PRELOAD`, an `LD_` or `DYLD_` name, or `GCONV_PATH`: the sandbox
    /// program would load it before the sandbox applies (OWL-41, OWL-125).
    Loader(String),
    /// A declared variable that makes an interpreter run code as it starts
    /// ([`STARTUP_CODE_VARIABLES`]): in a wrapper script standing for a
    /// program the runner starts outside the sandbox, or in the Codex harness
    /// (OWL-125).
    StartupCode(String),
    /// A declared variable of a harness's own ([`HARNESS_VARIABLE_PREFIXES`]),
    /// which can send a run to another provider, server or login (OWL-120,
    /// OWL-125).
    Harness {
        /// The harness whose variable it is.
        harness: Harness,
        /// The name as declared.
        name: String,
    },
    /// A declared name that cannot name a variable: empty, or holding `=` or
    /// NUL. A project declares names only; the values come from the runner's
    /// environment.
    Malformed(String),
    /// Declared variables the operator does not allow on this machine, each
    /// named once (OWL-63).
    NotAllowed(Vec<String>),
    /// A proxy variable whose value holds a login, named without its value
    /// (OWL-76). It comes from the runner's own environment.
    ProxyLogin(String),
}

impl fmt::Display for AgentEnvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Floor(violation) => violation.fmt(f),
            Self::Malformed(name) => write!(
                f,
                "{name:?} is not a variable name: declare names only, the values come from \
                 the runner's environment"
            ),
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
            Self::StartupCode(name) => write!(
                f,
                "{name} cannot be passed to an agent: it makes an interpreter run code as it \
                 starts, in the harness or in a wrapper of git the runner starts outside the \
                 sandbox; set it in the gate command itself if the gate needs it"
            ),
            Self::Harness {
                harness: Harness::Claude,
                name,
            } => write!(
                f,
                "{name} cannot be passed to an agent: Claude Code reads it to choose which \
                 provider, gateway or login a run uses, and agent runs log in only with the \
                 subscription token `owlshift init` keeps"
            ),
            Self::Harness {
                harness: Harness::Codex,
                name,
            } => write!(
                f,
                "{name} cannot be passed to an agent: it is one of Codex's own variables \
                 (`CODEX_*`, `OPENAI_*`), which choose the server, login or token endpoint a \
                 Codex run uses, and agent runs use Codex's own servers with the login of the \
                 user's Codex (`CODEX_HOME`)"
            ),
            Self::NotAllowed(names) => write!(
                f,
                "{} may not reach an agent on this machine: the operator allows a name in \
                 the personal configuration, for every repository in `allow_gate_env`, or for \
                 one in the `allow_gate_env` of its `[repositories.\"github.com/<owner>/<name>\"]` \
                 table",
                names.join(", ")
            ),
            Self::ProxyLogin(name) => write!(
                f,
                "{name} holds a proxy login in the runner's environment, and no proxy login \
                 reaches an agent: use a proxy that needs none in its URL, such as a local \
                 forwarding proxy that holds the login"
            ),
        }
    }
}

/// The prefixes of the names that make the dynamic loader load code into a
/// program: they never reach an agent. glibc reads every `LD_` name as a
/// setting of its loader, and macOS's loader every `DYLD_` name.
pub const LOADER_PREFIXES: &[&str] = &["LD_", "DYLD_"];

/// Names outside [`LOADER_PREFIXES`] that make a program load code through
/// the dynamic loader: glibc loads its character-set converters from
/// `GCONV_PATH` (OWL-125).
pub const LOADER_VARIABLES: &[&str] = &["GCONV_PATH"];

/// Names that make an interpreter run code as it starts (OWL-125). The
/// runner starts git outside the sandbox with the agent environment, found
/// on the agent's `PATH`, where it can be a bash wrapper script (Nix's
/// `makeWrapper`, an asdf or pyenv shim), and bash runs the file `BASH_ENV`
/// names. Codex installed with npm starts as a Node launcher, which loads the
/// code `NODE_OPTIONS` names (`--require`) and hands its environment to the
/// Codex binary, so that code could set any of Codex's own variables
/// ([`HARNESS_VARIABLE_PREFIXES`]). Other interpreters' start-up variables
/// (`PYTHONPATH`, `PERL5OPT`, `RUBYOPT`, `ENV`) reach only interpreters that
/// start inside the sandbox and stay declarable: `ENV` is read by interactive
/// shells only, which the runner's `executor::git` tests check on Unix
/// (OWL-130).
pub const STARTUP_CODE_VARIABLES: &[&str] = &["BASH_ENV", "NODE_OPTIONS"];

/// The prefixes of the harnesses' own variables: no name starting with one,
/// nor `CLAUDECODE`, reaches an agent through a declaration. A prefix, not a
/// list, since new names come with new releases. The runner sets the few a
/// run needs itself.
///
/// Claude Code (OWL-120) reads from its prefixes where a run sends its
/// requests and with which login: `ANTHROPIC_BASE_URL` names a gateway,
/// which then receives the run's subscription token, and
/// `CLAUDE_CODE_USE_BEDROCK`, `_VERTEX` and `_FOUNDRY` switch to another
/// provider's account; none of these shows in the run's `apiKeySource`.
///
/// Codex (OWL-125) sends the refresh token of the user's login to the host
/// `CODEX_REFRESH_TOKEN_URL_OVERRIDE` names, takes its login from
/// `CODEX_API_KEY` and hands its commands to the server
/// `CODEX_EXEC_SERVER_URL` names; its binary holds dozens more `CODEX_` and
/// `OPENAI_` names, and `OPENAI_BASE_URL`, its documented base URL, was
/// ignored by 0.154.0. `CODEX_HOME` is inherited ([`INHERITED`]), never
/// declared.
pub const HARNESS_VARIABLE_PREFIXES: &[(&str, Harness)] = &[
    ("ANTHROPIC_", Harness::Claude),
    ("CLAUDE_", Harness::Claude),
    ("CODEX_", Harness::Codex),
    ("OPENAI_", Harness::Codex),
];

impl std::error::Error for AgentEnvError {}

/// The environment of an agent, sorted by name: the inherited variables of
/// `parent` ([`INHERITED`], [`LOCALE_CATEGORIES`], [`PROXY_VARIABLES`]) and
/// those named in `declared`, then the [`OVERRIDES`].
///
/// `parent` is the runner's own environment; `declared` names the variables
/// the project declares for its gate and `allowed` those the operator lets
/// reach an agent, checked as [`check_declared`] says. A declared variable
/// absent from `parent` is simply not set. A proxy variable holding a login
/// is refused, the first by name ([`AgentEnvError::ProxyLogin`]).
pub fn agent_environment(
    parent: impl IntoIterator<Item = (OsString, OsString)>,
    declared: &[&str],
    allowed: &[&str],
) -> Result<Vec<(OsString, OsString)>, AgentEnvError> {
    check_declared(declared, allowed)?;
    let mut vars = BTreeMap::new();
    for (name, value) in parent {
        let kept = name
            .to_str()
            .is_some_and(|name| is_inherited(name) || contains_name(declared, name));
        if kept {
            vars.insert(name, value);
        }
    }
    let proxy_login = vars.iter().find(|(name, value)| {
        name.to_str().is_some_and(is_proxy) && value.as_encoded_bytes().contains(&b'@')
    });
    if let Some((name, _)) = proxy_login {
        return Err(AgentEnvError::ProxyLogin(
            name.to_string_lossy().into_owned(),
        ));
    }
    for (name, value) in OVERRIDES {
        vars.insert(OsString::from(name), OsString::from(value));
    }
    Ok(vars.into_iter().collect())
}

/// Checks the names a project declares for its gate (`stack.gate_env`)
/// against those the operator allows (the personal `allow_gate_env`), as
/// [`agent_environment`] does before it builds anything, and as the
/// configuration does when it is loaded (OWL-63).
///
/// What [`check_names`] refuses is refused first, allowed or not. Then every
/// declared name the operator does not allow is refused: a project's file,
/// which any contributor can change, never widens what reaches an agent.
/// Names compare in any letter case.
pub fn check_declared(declared: &[&str], allowed: &[&str]) -> Result<(), AgentEnvError> {
    check_names(declared)?;
    let mut refused: Vec<String> = Vec::new();
    for name in declared {
        if !contains_name(allowed, name) && !refused.iter().any(|r| r.eq_ignore_ascii_case(name)) {
            refused.push((*name).to_owned());
        }
    }
    if refused.is_empty() {
        Ok(())
    } else {
        Err(AgentEnvError::NotAllowed(refused))
    }
}

/// Checks names that would reach an agent, whoever names them: a name that
/// is not a variable name, a credential variable, a `GIT_` name or an
/// override, a dynamic-loader variable, an interpreter's start-up code
/// ([`STARTUP_CODE_VARIABLES`]), or a variable of a harness's own
/// ([`HARNESS_VARIABLE_PREFIXES`]) is refused, in any letter case, in that
/// order. Neither a project's declaration nor the operator's allow-list
/// passes one.
pub fn check_names(names: &[&str]) -> Result<(), AgentEnvError> {
    if let Some(name) = names
        .iter()
        .find(|name| name.is_empty() || name.contains(['=', '\0']))
    {
        return Err(AgentEnvError::Malformed((*name).to_owned()));
    }
    floor::check_agent_environment(names.iter().copied()).map_err(AgentEnvError::Floor)?;
    if let Some(name) = names.iter().find(|name| is_reserved(name)) {
        return Err(AgentEnvError::Reserved((*name).to_owned()));
    }
    if let Some(name) = names.iter().find(|name| is_loader(name)) {
        return Err(AgentEnvError::Loader((*name).to_owned()));
    }
    if let Some(name) = names
        .iter()
        .find(|name| contains_name(STARTUP_CODE_VARIABLES, name))
    {
        return Err(AgentEnvError::StartupCode((*name).to_owned()));
    }
    if let Some((name, harness)) = names
        .iter()
        .find_map(|name| harness_variable(name).map(|harness| (name, harness)))
    {
        return Err(AgentEnvError::Harness {
            harness,
            name: (*name).to_owned(),
        });
    }
    Ok(())
}

fn contains_name(names: &[&str], name: &str) -> bool {
    names.iter().any(|n| n.eq_ignore_ascii_case(name))
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
    contains_name(INHERITED, name) || contains_name(LOCALE_CATEGORIES, name) || is_proxy(name)
}

fn is_proxy(name: &str) -> bool {
    contains_name(PROXY_VARIABLES, name)
}

fn is_reserved(name: &str) -> bool {
    starts_with_ignore_case(name, "GIT_")
        || OVERRIDES
            .iter()
            .any(|(overridden, _)| overridden.eq_ignore_ascii_case(name))
}

fn is_loader(name: &str) -> bool {
    LOADER_PREFIXES
        .iter()
        .any(|prefix| starts_with_ignore_case(name, prefix))
        || contains_name(LOADER_VARIABLES, name)
}

/// The harness whose own variable `name` is, if it is one.
fn harness_variable(name: &str) -> Option<Harness> {
    if name.eq_ignore_ascii_case("CLAUDECODE") {
        return Some(Harness::Claude);
    }
    HARNESS_VARIABLE_PREFIXES
        .iter()
        .find(|(prefix, _)| starts_with_ignore_case(name, prefix))
        .map(|&(_, harness)| harness)
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
            ("LC_TERMINAL", "iTerm2"),
            ("LC_MY_TOKEN", "secret"),
            ("https_proxy", "http://proxy:3128"),
            ("ALL_PROXY", "socks5://proxy:1080"),
            ("NO_PROXY", "localhost,.internal"),
            ("SystemRoot", "C:\\Windows"),
            ("CLAUDE_CONFIG_DIR", "/home/op/.claude"),
            ("CLAUDE_CODE_OAUTH_TOKEN", "sk-ant-oat01-x"),
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
            ("CODEX_HOME", "/home/op/.codex"),
            ("CODEX_REFRESH_TOKEN_URL_OVERRIDE", "http://127.0.0.1:1"),
            ("NODE_OPTIONS", "--require /tmp/x.js"),
        ]);
        let agent = agent_environment(parent, &["database_url"], &["DATABASE_URL"]).unwrap();
        let ssh = "ssh -o BatchMode=yes -o IdentityAgent=none -o PubkeyAuthentication=no \
                   -o GSSAPIAuthentication=no";
        let expected = env(&[
            ("ALL_PROXY", "socks5://proxy:1080"),
            ("CODEX_HOME", "/home/op/.codex"),
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
            ("NO_PROXY", "localhost,.internal"),
            ("PATH", "/bin"),
            ("SystemRoot", "C:\\Windows"),
            ("https_proxy", "http://proxy:3128"),
        ]);
        assert_eq!(agent, expected);
        let vars = agent.iter().map(|(n, v)| (n.as_os_str(), v.as_os_str()));
        assert_eq!(check_agent_variables(vars), Ok(()));
    }

    #[test]
    fn a_proxy_holding_a_login_never_reaches_an_agent() {
        let clean = ("HTTP_PROXY", "http://proxy:3128");
        for (name, value) in [
            ("http_proxy", "http://fake-user:fake-pass@proxy:3128"),
            ("HTTPS_PROXY", "fake-user:fake-pass@proxy:3128"),
            ("All_Proxy", "socks5://fake-user:fake-pass@proxy:1080"),
        ] {
            let parent = env(&[clean, (name, value), ("PATH", "/bin")]);
            let refused = agent_environment(parent, &[], &[]);
            assert_eq!(refused, Err(AgentEnvError::ProxyLogin(name.to_owned())));
            let message = refused.unwrap_err().to_string();
            assert!(message.starts_with(name) && !message.contains("fake-pass"));
        }
        // Declaring and allowing the variable does not pass it either.
        let parent = env(&[("https_proxy", "http://fake-user:fake-pass@proxy:3128")]);
        assert_eq!(
            agent_environment(parent, &["HTTPS_PROXY"], &["https_proxy"]),
            Err(AgentEnvError::ProxyLogin("https_proxy".to_owned()))
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_proxy_login_is_found_in_a_value_that_is_not_utf8() {
        use std::os::unix::ffi::OsStringExt;
        let value = OsString::from_vec(b"http://fake\xff:x@proxy:3128".to_vec());
        let parent = vec![(OsString::from("HTTP_PROXY"), value)];
        assert_eq!(
            agent_environment(parent, &[], &[]),
            Err(AgentEnvError::ProxyLogin("HTTP_PROXY".to_owned()))
        );
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
                agent_environment(Vec::new(), &[name], &[name]),
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
                agent_environment(Vec::new(), &[name], &[name]),
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
            "Ld_Profile",
            "DYLD_INSERT_LIBRARIES",
            "dyld_x",
            "gconv_path",
        ] {
            assert_eq!(
                agent_environment(Vec::new(), &[name], &[name]),
                Err(AgentEnvError::Loader(name.to_owned())),
                "{name}"
            );
        }
        // OWL-125: start-up code in the Codex launcher or a wrapper of git.
        for name in ["BASH_ENV", "node_options"] {
            assert_eq!(
                agent_environment(Vec::new(), &[name], &[name]),
                Err(AgentEnvError::StartupCode(name.to_owned())),
                "{name}"
            );
        }
        // OWL-120 and OWL-125: the harnesses' own variables, which route a
        // run to a gateway, another provider or another server, by prefix or
        // by name. A declared `CODEX_HOME` too, though it is inherited.
        for (name, harness) in [
            ("ANTHROPIC_BASE_URL", Harness::Claude),
            ("claude_code_use_bedrock", Harness::Claude),
            ("CLAUDE_", Harness::Claude),
            ("ClaudeCode", Harness::Claude),
            ("CODEX_REFRESH_TOKEN_URL_OVERRIDE", Harness::Codex),
            ("codex_exec_server_url", Harness::Codex),
            ("Codex_Home", Harness::Codex),
            ("OpenAI_Base_Url", Harness::Codex),
        ] {
            assert_eq!(
                agent_environment(Vec::new(), &[name], &[name]),
                Err(AgentEnvError::Harness {
                    harness,
                    name: name.to_owned()
                }),
                "{name}"
            );
        }
        // The floor's credentials among them keep the floor's refusal.
        for name in [
            "ANTHROPIC_API_KEY",
            "Anthropic_Auth_Token",
            "claude_code_oauth_token",
            "AWS_BEARER_TOKEN_BEDROCK",
            "Codex_Api_Key",
            "OPENAI_API_KEY",
        ] {
            assert_eq!(
                check_names(&[name]),
                Err(AgentEnvError::Floor(FloorViolation::CredentialVariables(
                    vec![name.to_owned()]
                ))),
                "{name}"
            );
        }
        assert_eq!(
            check_names(&[
                "MY_ANTHROPIC_FLAG",
                "CLAUDE",
                "ANTHROPICX",
                "CLAUDECODEX",
                "CODEX",
                "MY_OPENAI_FLAG",
                "LDFLAGS",
            ]),
            Ok(())
        );
        // OWL-125, decided: these start-up variables reach only interpreters
        // that start inside the sandbox, so a gate may still declare them.
        assert_eq!(
            check_names(&["PYTHONPATH", "PYTHONSTARTUP", "PERL5OPT", "RUBYOPT", "ENV"]),
            Ok(())
        );
        assert_eq!(
            AgentEnvError::Harness {
                harness: Harness::Claude,
                name: "ANTHROPIC_BASE_URL".into()
            }
            .to_string(),
            "ANTHROPIC_BASE_URL cannot be passed to an agent: Claude Code reads it to choose \
             which provider, gateway or login a run uses, and agent runs log in only with the \
             subscription token `owlshift init` keeps"
        );
        assert_eq!(
            AgentEnvError::Harness {
                harness: Harness::Codex,
                name: "CODEX_EXEC_SERVER_URL".into()
            }
            .to_string(),
            "CODEX_EXEC_SERVER_URL cannot be passed to an agent: it is one of Codex's own \
             variables (`CODEX_*`, `OPENAI_*`), which choose the server, login or token \
             endpoint a Codex run uses, and agent runs use Codex's own servers with the login \
             of the user's Codex (`CODEX_HOME`)"
        );
        assert_eq!(
            AgentEnvError::StartupCode("NODE_OPTIONS".into()).to_string(),
            "NODE_OPTIONS cannot be passed to an agent: it makes an interpreter run code as it \
             starts, in the harness or in a wrapper of git the runner starts outside the \
             sandbox; set it in the gate command itself if the gate needs it"
        );
        for name in ["", "FEATURE=on", "A\0B"] {
            assert_eq!(
                agent_environment(Vec::new(), &[name], &[name]),
                Err(AgentEnvError::Malformed(name.to_owned())),
                "{name:?}"
            );
        }
        assert_eq!(
            AgentEnvError::Malformed("FEATURE=on".into()).to_string(),
            "\"FEATURE=on\" is not a variable name: declare names only, the values come from \
             the runner's environment"
        );
    }

    #[test]
    fn a_declared_name_the_operator_does_not_allow_is_refused() {
        let parent = env(&[("DATABASE_URL", "postgres://db/test"), ("PATH", "/bin")]);
        assert_eq!(
            agent_environment(
                parent.clone(),
                &[
                    "DATABASE_URL",
                    "my_linear_token",
                    "My_Linear_Token",
                    "JAVA_HOME"
                ],
                &["java_home"],
            ),
            Err(AgentEnvError::NotAllowed(vec![
                "DATABASE_URL".to_owned(),
                "my_linear_token".to_owned(),
            ]))
        );
        assert_eq!(
            AgentEnvError::NotAllowed(vec!["DATABASE_URL".into(), "X".into()]).to_string(),
            "DATABASE_URL, X may not reach an agent on this machine: the operator allows a name \
             in the personal configuration, for every repository in `allow_gate_env`, or for \
             one in the `allow_gate_env` of its `[repositories.\"github.com/<owner>/<name>\"]` \
             table"
        );
        // Allowing a name does not pass it: only declaring it does.
        let agent = agent_environment(parent, &[], &["DATABASE_URL"]).unwrap();
        assert!(!agent.iter().any(|(name, _)| name == "DATABASE_URL"));
        // What the floor refuses stays refused, allowed or not.
        assert_eq!(
            check_declared(&["GH_TOKEN"], &["GH_TOKEN"]),
            Err(AgentEnvError::Floor(FloorViolation::CredentialVariables(
                vec!["GH_TOKEN".to_owned()]
            )))
        );
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
