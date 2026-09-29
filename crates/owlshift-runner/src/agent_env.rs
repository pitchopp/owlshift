//! Spawning agents with the environment of [`owlshift_core::agent_env`],
//! inside the operating system's sandbox (OWL-41), and checking that no
//! credential is left within their reach (OWL-22).
//!
//! For the executor (OWL-15): before a run, [`AgentEnv::from_runner`] builds
//! the environment, [`AgentEnv::sandbox_ready`] refuses a machine where
//! agents cannot be confined, and [`AgentEnv::check`] probes, from inside the
//! sandbox, what the agent could still reach in the worktree; a finding stops
//! the run before it starts. The harness command, and each gate command, is
//! spawned as [`AgentEnv::confine`] (or, for a shell line,
//! [`AgentEnv::confine_shell`]) returns it: wrapped in the sandbox, with
//! exactly the agent's variables. After the run, [`mcp_findings`] reads the
//! MCP servers the harness reported loading.
//!
//! The sandbox leaves the home unreadable but for the folders a run needs
//! ([`AgentEnv::policy`]), writes nowhere but the worktree, the repository's
//! git folder (not its hooks or configuration), the harness's own login
//! folder and the run's own temporary folder, closes the temporary folders
//! the user's other processes share, and closes the system credential store.
//! An agent environment is always confined: the only way out is
//! `without_confinement`, compiled for the test bench alone.
//!
//! The probes run git and gh as the agent would, with its environment and in
//! its directory. They never keep, log or display a secret: a finding names a
//! variable, a host or a redacted setting, never a value.

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use owlshift_adapters::harness::claude;
use owlshift_core::agent_env::{
    AgentEnvError, NO_CREDENTIAL, agent_environment, check_agent_variables,
};
use owlshift_core::floor::FloorViolation;
use owlshift_platform::paths;
use owlshift_platform::process::{Captured, OUTPUT_CAP, find_executable_in, run_command};
use owlshift_platform::sandbox::{self, Policy, SandboxError};

use crate::system::PROBE_TIMEOUT;

/// The environment an agent process gets, and the sandbox it runs in: see
/// the module documentation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentEnv {
    vars: Vec<(OsString, OsString)>,
    /// The names the project declares for its gate (`stack.gate_env`): the
    /// folders their values name in the home are opened, read-only
    /// ([`AgentEnv::policy`]).
    gate_env: Vec<String>,
    /// Whether every command spawned for the agent is confined. Always true
    /// outside the test bench.
    confined: bool,
}

/// The paths of one run the sandbox opens, besides what every agent gets.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunPaths {
    /// The worktree: the working directory, read and written.
    pub workdir: PathBuf,
    /// The repository's common git folder, read and written, but for its
    /// hooks and configuration: a commit writes objects and refs there.
    pub git_dir: Option<PathBuf>,
    /// More folders read, such as the harness's install folder.
    pub readable: Vec<PathBuf>,
    /// More folders read and written, such as the harness's login folder.
    pub writable: Vec<PathBuf>,
    /// Paths neither read nor written, such as the runner's run folder.
    pub hidden: Vec<PathBuf>,
    /// The run's own temporary folder, made by the runner and removed with
    /// the run: the only temporary folder the agent writes, which `TMPDIR`,
    /// `TMP` and `TEMP` name. `None`: no temporary folder is written.
    pub temp: Option<PathBuf>,
}

/// The variables that name a temporary folder.
const TEMP_VARIABLES: &[&str] = &["TMPDIR", "TMP", "TEMP"];

/// Credential files and folders under the home, closed even when a folder
/// that holds them is opened. Paths are relative to the home.
const HIDDEN_IN_HOME: &[&str] = &[
    ".ssh",
    ".aws",
    ".azure",
    ".kube",
    ".docker",
    ".gnupg",
    ".netrc",
    ".npmrc",
    ".pypirc",
    ".git-credentials",
    ".config/gh",
    ".config/gcloud",
    ".cargo/credentials",
    ".cargo/credentials.toml",
    ".local/share/keyrings",
    // Maven's and Gradle's repository passwords.
    ".m2/settings.xml",
    ".m2/settings-security.xml",
    ".gradle/gradle.properties",
    // The Keychain, which the macOS sandbox closes itself: listed so that no
    // folder holding it, `~/Library`, is opened for a variable.
    "Library/Keychains",
];

/// Tool-chain folders under the home an agent reads: installed compilers and
/// their package caches, never their credentials ([`HIDDEN_IN_HOME`]). The
/// folders the agent's `PATH` and declared variables name under the home are
/// read too ([`named_folder`]).
const TOOL_CHAINS: &[&str] = &[".rustup", ".cargo/bin", ".cargo/registry", ".cargo/git"];

impl AgentEnv {
    /// The agent environment built from `parent`, with no declared variable:
    /// `declared` is checked against an empty allow-list, so only an empty
    /// list builds. A project's run uses [`AgentEnv::for_project`]. It is
    /// confined.
    pub fn new(
        parent: impl IntoIterator<Item = (OsString, OsString)>,
        declared: &[&str],
    ) -> Result<Self, AgentEnvError> {
        Self::for_project(parent, declared, &[])
    }

    /// The agent environment built from `parent`, with the variables named in
    /// `declared` (the project's `stack.gate_env`) that `allowed` (the
    /// operator's personal `allow_gate_env`) lets through; a declared name
    /// not allowed is refused (OWL-63). It is confined.
    pub fn for_project(
        parent: impl IntoIterator<Item = (OsString, OsString)>,
        declared: &[&str],
        allowed: &[&str],
    ) -> Result<Self, AgentEnvError> {
        agent_environment(parent, declared, allowed).map(|vars| Self {
            vars,
            gate_env: declared.iter().map(|name| (*name).to_owned()).collect(),
            confined: true,
        })
    }

    /// The agent environment built from the runner's own, as
    /// [`AgentEnv::for_project`] builds it, confined, with Claude Code
    /// pointed at the login made for agent runs
    /// ([`paths::claude_agent_login_dir`]): the sandbox closes the Keychain,
    /// where the operator's own login lives on macOS. Claude Code's inbox for
    /// the user's other sessions is switched off ([`claude::PEER_INBOX_ENV`],
    /// OWL-65), whatever the runner's environment and the declared variables
    /// say. Only this constructor sets either variable.
    pub fn from_runner(declared: &[&str], allowed: &[&str]) -> Result<Self, AgentEnvError> {
        let mut env = Self::for_project(std::env::vars_os(), declared, allowed)?;
        if let Some(dir) = paths::claude_agent_login_dir() {
            env.set("CLAUDE_CONFIG_DIR", dir.into_os_string());
        }
        let (name, value) = claude::PEER_INBOX_ENV;
        env.set(name, value.into());
        Ok(env)
    }

    /// The same environment, with nothing confined: the test bench's way to
    /// run the fake harness and the isolation scenarios bare. It exists only
    /// in test builds and with the `testkit` feature, which no shipped crate
    /// enables.
    #[cfg(any(test, feature = "testkit"))]
    pub fn without_confinement(mut self) -> Self {
        self.confined = false;
        self
    }

    /// Whether commands spawned for the agent are confined.
    pub fn is_confined(&self) -> bool {
        self.confined
    }

    /// The variables, sorted by name.
    pub fn vars(&self) -> &[(OsString, OsString)] {
        &self.vars
    }

    /// The value of a variable, its name compared ASCII case-insensitively.
    pub fn var(&self, name: &str) -> Option<&OsStr> {
        self.vars
            .iter()
            .find(|(n, _)| n.to_str().is_some_and(|n| n.eq_ignore_ascii_case(name)))
            .map(|(_, value)| value.as_os_str())
    }

    /// Sets a variable, replacing one of the same name in any letter case.
    fn set(&mut self, name: &str, value: OsString) {
        self.vars
            .retain(|(n, _)| !n.to_str().is_some_and(|n| n.eq_ignore_ascii_case(name)));
        self.vars.push((OsString::from(name), value));
        self.vars.sort();
    }

    /// Gives `command` exactly these variables. It replaces the command's
    /// whole environment, including anything set on it before. It confines
    /// nothing: agent commands are spawned as [`AgentEnv::confine`] returns
    /// them.
    pub fn apply(&self, command: &mut Command) {
        command
            .env_clear()
            .envs(self.vars.iter().map(|(n, v)| (n, v)));
    }

    /// Whether agents can be confined on this machine; a confined
    /// environment where they cannot refuses to run anything. The error says
    /// what to fix: install bwrap, allow it user namespaces, or use WSL2 on
    /// Windows.
    pub fn sandbox_ready(&self) -> Result<(), SandboxError> {
        if self.confined {
            sandbox::available()
        } else {
            Ok(())
        }
    }

    /// The sandbox for a run: see the module documentation.
    ///
    /// Opened in the home: git's own configuration, the tool chains
    /// ([`TOOL_CHAINS`]) and the folders the agent's `PATH` and declared
    /// variables name under it, read-only, never the home itself nor a
    /// folder that is, lies in or holds a path the run hides, closes or
    /// writes ([`named_folder`], OWL-68); then the run's own paths. Closed
    /// wherever they are: the credential files of [`HIDDEN_IN_HOME`] and the
    /// run's hidden paths. It reads the file system, to judge each named
    /// folder by its real path.
    pub fn policy(&self, run: &RunPaths) -> Policy {
        let home = self
            .var("HOME")
            .map(PathBuf::from)
            .filter(|home| home.is_absolute());
        let config_home = self
            .var("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|dir| dir.is_absolute())
            .or_else(|| home.as_ref().map(|home| home.join(".config")));

        let mut readable = Vec::new();
        let mut hidden = Vec::new();
        if let Some(home) = &home {
            readable.push(home.join(".gitconfig"));
            readable.extend(TOOL_CHAINS.iter().map(|dir| home.join(dir)));
            hidden.extend(HIDDEN_IN_HOME.iter().map(|path| home.join(path)));
        }
        if let Some(config_home) = &config_home {
            readable.push(config_home.join("git"));
            hidden.push(config_home.join("git").join("credentials"));
        }
        readable.extend(run.readable.iter().cloned());
        hidden.extend(run.hidden.iter().cloned());

        let mut writable = vec![run.workdir.clone()];
        let mut protected = Vec::new();
        if let Some(git_dir) = &run.git_dir {
            writable.push(git_dir.clone());
            protected.extend(
                ["hooks", "config", "config.worktree", "info"]
                    .iter()
                    .map(|name| git_dir.join(name)),
            );
        }
        writable.extend(run.writable.iter().cloned());

        // The temporary folders the runner inherited are shared with the
        // user's other processes: closed, never opened. Only the run's own
        // folder is written.
        let mut closed: Vec<PathBuf> = TEMP_VARIABLES
            .iter()
            .filter_map(|name| self.var(name).map(PathBuf::from))
            .chain([std::env::temp_dir()])
            .filter(|dir| dir.is_absolute() && dir.parent().is_some())
            .collect();
        closed.sort();
        closed.dedup();

        // The folders the agent's variables name in the home.
        let mut links = Vec::new();
        if let Some((home, real_home)) = home
            .as_ref()
            .and_then(|home| Some((home, std::fs::canonicalize(home).ok()?)))
        {
            let kept_out: Vec<PathBuf> = hidden
                .iter()
                .chain(&closed)
                .chain(&writable)
                .chain(&run.temp)
                .map(|path| sandbox::real(path))
                .filter(|path| path.starts_with(&real_home) && *path != real_home)
                .collect();
            let named = ["PATH"]
                .into_iter()
                .chain(self.gate_env.iter().map(String::as_str))
                .flat_map(|name| self.values_of(name))
                .flat_map(std::env::split_paths);
            // A folder named twice, by a name declared twice in two letter
            // cases, by `PATH` declared again, or by two variables, is
            // opened and linked once.
            for dir in named {
                let folder = named_folder(home, &real_home, &dir, &kept_out);
                if let Some(open) = folder.open
                    && !readable.contains(&open)
                {
                    readable.push(open);
                }
                if let Some(link) = folder.link
                    && !links.contains(&link)
                {
                    links.push(link);
                }
            }
        }

        Policy {
            home,
            closed,
            readable,
            writable,
            protected,
            hidden,
            temp: run.temp.iter().cloned().collect(),
            links,
            workdir: run.workdir.clone(),
        }
    }

    /// The values of every variable named `name`, in any letter case: on
    /// Unix, `JAVA_HOME` and `java_home` are two variables, and both reach
    /// the agent once either is declared.
    fn values_of<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a OsStr> + 'a {
        self.vars
            .iter()
            .filter(move |(n, _)| n.to_str().is_some_and(|n| n.eq_ignore_ascii_case(name)))
            .map(|(_, value)| value.as_os_str())
    }

    /// The command to spawn for `inner`, an agent command built with its
    /// program and arguments only: wrapped in the sandbox of `run`, in the
    /// run's working directory, with exactly the agent's variables. The
    /// caller then sets its standard streams. `inner`'s own directory,
    /// environment and streams are not carried over.
    ///
    /// Without confinement (the test bench), `inner` itself comes back, with
    /// the agent's variables.
    pub fn confine(&self, inner: Command, run: &RunPaths) -> Result<Command, SandboxError> {
        let command = self.sandboxed(inner, run)?;
        Ok(self.environment(command, run))
    }

    /// [`AgentEnv::confine`] for a command line run through the platform's
    /// shell: `sh -c` on Unix, and on Windows `cmd.exe` (`COMSPEC`) with
    /// `/d /s /c "line"`, given verbatim, since its quoting rules are not
    /// those of other programs. `/s` makes it strip the outer quotes and keep
    /// everything between them. `confine` would quote that line again: it
    /// rebuilds the command from `get_args`, which hands back a raw argument
    /// as a plain one.
    pub fn confine_shell(&self, line: &str, run: &RunPaths) -> Result<Command, SandboxError> {
        #[cfg(unix)]
        let command = {
            let mut inner = Command::new("sh");
            inner.arg("-c").arg(line);
            self.sandboxed(inner, run)?
        };
        #[cfg(windows)]
        let command = {
            use std::os::windows::process::CommandExt;
            let shell = self.var("COMSPEC").unwrap_or(OsStr::new("cmd.exe"));
            let tail = format!("/d /s /c \"{line}\"");
            if self.confined {
                sandbox::wrap_line(&self.policy(run), shell, OsStr::new(&tail))?
            } else {
                let mut command = Command::new(shell);
                command.raw_arg(tail).current_dir(&run.workdir);
                command
            }
        };
        Ok(self.environment(command, run))
    }

    /// `inner` wrapped in the sandbox of `run`, or, without confinement,
    /// `inner` itself in the run's working directory.
    fn sandboxed(&self, inner: Command, run: &RunPaths) -> Result<Command, SandboxError> {
        if self.confined {
            sandbox::wrap(&self.policy(run), inner.get_program(), inner.get_args())
        } else {
            let mut inner = inner;
            inner.current_dir(&run.workdir);
            Ok(inner)
        }
    }

    /// Gives an agent command exactly the agent's variables, and the run's
    /// own temporary folder, never the one inherited.
    fn environment(&self, mut command: Command, run: &RunPaths) -> Command {
        self.apply(&mut command);
        match &run.temp {
            Some(dir) => {
                for name in TEMP_VARIABLES {
                    command.env(name, dir);
                }
            }
            None if self.confined => {
                for name in TEMP_VARIABLES {
                    command.env_remove(name);
                }
            }
            None => {}
        }
        command
    }

    /// [`check_environment`] with these variables, each probe run as the
    /// agent would run it: inside the sandbox, when confined, so a finding
    /// means a credential the agent could still reach.
    pub fn check(&self, workdir: &Path, forge_hosts: &[&str]) -> Vec<CredentialFinding> {
        if !self.confined {
            return check_environment(&self.vars, workdir, forge_hosts);
        }
        let git_dir = match self.git_common_dir(workdir) {
            Ok(dir) => dir,
            Err(reason) => {
                return vec![CredentialFinding::ProbeFailed {
                    probe: "git rev-parse",
                    reason,
                }];
            }
        };
        let run = RunPaths {
            workdir: workdir.to_owned(),
            git_dir: Some(git_dir),
            ..RunPaths::default()
        };
        probe_environment(&self.vars, forge_hosts, &|program, args| {
            let mut inner = Command::new(program);
            inner.args(args);
            self.confine(inner, &run).map_err(|error| error.to_string())
        })
    }

    /// The common git folder of the repository `workdir` belongs to, as the
    /// agent's git reports it.
    fn git_common_dir(&self, workdir: &Path) -> Result<PathBuf, String> {
        let search_path = self.var("PATH").unwrap_or_default();
        let git = find_executable_in("git", search_path)
            .ok_or_else(|| "git is not on the agent's PATH".to_owned())?;
        let mut command = Command::new(git);
        command
            .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
            .current_dir(workdir);
        self.apply(&mut command);
        let captured = run_command(&mut command, None, PROBE_TIMEOUT, OUTPUT_CAP)
            .map_err(|error| error.to_string())?;
        if captured.code != Some(0) {
            return Err(format!("it exited with status {:?}", captured.code));
        }
        let dir = String::from_utf8_lossy(&captured.stdout).trim().to_owned();
        Ok(PathBuf::from(dir))
    }
}

/// What the sandbox makes of a folder a variable names: see [`named_folder`].
#[derive(Debug, Default, PartialEq, Eq)]
struct NamedFolder {
    /// Its real path, read-only, when it lies in the home.
    open: Option<PathBuf>,
    /// The link to recreate, as `(at, target)`, when the variable names the
    /// folder through a link in the home, such as sdkman's `current`.
    link: Option<(PathBuf, PathBuf)>,
}

/// What the sandbox makes of `dir`, a folder named by the agent's `PATH` or
/// by a declared variable, in the home `home`, whose real path is
/// `real_home` (OWL-68).
///
/// The folder is judged by its real path, so that neither `..` nor a link
/// can name the home or a credential, and that real path is what is opened,
/// so the check and the sandbox see the same folder. Nothing is opened for a
/// relative or missing path, a file, the home or a folder above it, or a
/// folder that is, lies in or holds a path of `kept_out`: the real paths the
/// run hides, closes or writes in the home. The last rule keeps a broad
/// folder closed, such as `~/.config`, which holds `.config/gh` and may hold
/// credentials no list names; it also keeps a folder the agent writes, where
/// it could swap a link, from being judged at all. A folder outside the home
/// is not opened either: the sandbox already leaves it readable, or closes
/// it on purpose.
///
/// When `dir` lies in the home but names its folder through a link, the
/// link is recreated where it is named, as long as that place is not kept
/// out: the sandbox's home starts empty on Linux.
fn named_folder(home: &Path, real_home: &Path, dir: &Path, kept_out: &[PathBuf]) -> NamedFolder {
    let mut folder = NamedFolder::default();
    if !dir.is_absolute() {
        return folder;
    }
    let Ok(real) = std::fs::canonicalize(dir) else {
        return folder;
    };
    if !real.is_dir() || real_home.starts_with(&real) {
        return folder;
    }
    let touches = |path: &Path| {
        kept_out
            .iter()
            .any(|kept| path.starts_with(kept) || kept.starts_with(path))
    };
    if real.starts_with(real_home) {
        if touches(&real) {
            return folder;
        }
        folder.open = Some(real.clone());
    }
    let at = dir
        .strip_prefix(home)
        .ok()
        .filter(|rel| rel.components().all(|c| matches!(c, Component::Normal(_))))
        .map(|rel| real_home.join(rel));
    if let Some(at) = at
        && at != real
        && !kept_out.iter().any(|kept| at.starts_with(kept))
    {
        folder.link = Some((at, real));
    }
    folder
}

/// A credential an agent could reach, or a probe that could not tell.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CredentialFinding {
    /// Credential variables set to something other than the placeholder.
    Variables(Vec<String>),
    /// `git credential fill` returned a password for `https://<host>`: a
    /// credential helper or an askpass program answered.
    GitCredential { host: String },
    /// `gh auth token` returned a login for the host.
    GhLogin { host: String },
    /// A git setting that carries a credential, its key redacted: a URL with
    /// user info, or an extra HTTP header.
    GitSetting { key: String },
    /// The harness loaded these MCP servers.
    McpServers(Vec<String>),
    /// A probe could not run or did not finish: what it looks for is unknown,
    /// so it counts as found.
    ProbeFailed { probe: &'static str, reason: String },
}

impl fmt::Display for CredentialFinding {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Variables(names) => write!(
                f,
                "credential variables would reach the agent: {}",
                names.join(", ")
            ),
            Self::GitCredential { host } => write!(
                f,
                "git hands the agent a credential for https://{host} (a credential helper \
                 or an askpass program)"
            ),
            Self::GhLogin { host } => write!(f, "gh hands the agent a login for {host}"),
            Self::GitSetting { key } => write!(f, "the git setting {key} carries a credential"),
            Self::McpServers(names) => {
                write!(f, "the harness loaded MCP servers: {}", names.join(", "))
            }
            Self::ProbeFailed { probe, reason } => write!(f, "could not check {probe}: {reason}"),
        }
    }
}

/// The git settings that can carry a forge credential: remote URLs, URL
/// rewrites and extra HTTP headers. Git matches it, extended syntax, against
/// keys whose section and name are lowercase.
const CREDENTIAL_SETTINGS: &str = "^(remote\\..*\\.(url|pushurl)|url\\..*\\.(insteadof|pushinsteadof)|http\\.(.*\\.)?extraheader)$";

/// Probes, with exactly `vars` as environment and in `workdir`, whether an
/// agent could reach a credential, and returns every finding; none means none
/// was reachable. `forge_hosts` are the hosts git and gh are asked about,
/// such as `github.com`.
///
/// It checks the variables themselves ([`check_agent_variables`]), asks
/// `git credential fill` and `gh auth token` for each host, and reads the git
/// settings that can hold a credential. git and gh are the ones on the `PATH`
/// of `vars`; no gh means no gh login, while no git is a failed probe.
pub fn check_environment(
    vars: &[(OsString, OsString)],
    workdir: &Path,
    forge_hosts: &[&str],
) -> Vec<CredentialFinding> {
    probe_environment(vars, forge_hosts, &|program, args| {
        let mut command = Command::new(program);
        command
            .args(args)
            .current_dir(workdir)
            .env_clear()
            .envs(vars.iter().map(|(n, v)| (n, v)));
        Ok(command)
    })
}

/// Builds a probe's command from a program and its arguments, or says why
/// it cannot.
type BuildProbe<'a> = &'a dyn Fn(&Path, &[&str]) -> Result<Command, String>;

/// The probes of [`check_environment`], each command built by `build` from
/// a program and its arguments: bare, or inside the sandbox.
fn probe_environment(
    vars: &[(OsString, OsString)],
    forge_hosts: &[&str],
    build: BuildProbe<'_>,
) -> Vec<CredentialFinding> {
    let mut findings = Vec::new();
    let pairs = vars.iter().map(|(n, v)| (n.as_os_str(), v.as_os_str()));
    if let Err(FloorViolation::CredentialVariables(names)) = check_agent_variables(pairs) {
        findings.push(CredentialFinding::Variables(names));
    }

    let search_path = vars
        .iter()
        .find(|(name, _)| {
            name.to_str()
                .is_some_and(|n| n.eq_ignore_ascii_case("PATH"))
        })
        .map_or_else(OsString::new, |(_, value)| value.clone());
    let probe = |program: &Path, args: &[&str], input: Option<&[u8]>| {
        let mut command = build(program, args)?;
        run_command(&mut command, input, PROBE_TIMEOUT, OUTPUT_CAP)
            .map_err(|error| error.to_string())
    };

    match find_executable_in("git", &search_path) {
        None => findings.push(CredentialFinding::ProbeFailed {
            probe: "git",
            reason: "git is not on the agent's PATH".to_owned(),
        }),
        Some(git) => {
            for host in forge_hosts {
                let request = format!("protocol=https\nhost={host}\n\n");
                match probe(&git, &["credential", "fill"], Some(request.as_bytes())) {
                    Ok(captured) if holds_password(&captured.stdout) => {
                        findings.push(CredentialFinding::GitCredential {
                            host: (*host).to_owned(),
                        });
                    }
                    Ok(_) => {}
                    Err(error) => findings.push(failed("git credential fill", &error)),
                }
            }
            let args = ["config", "-z", "--get-regexp", CREDENTIAL_SETTINGS];
            match probe(&git, &args, None) {
                Ok(captured) if captured.code == Some(0) => findings.extend(
                    credential_settings(&captured.stdout)
                        .into_iter()
                        .map(|key| CredentialFinding::GitSetting { key }),
                ),
                // Status 1: no such setting.
                Ok(captured) if captured.code == Some(1) => {}
                Ok(captured) => findings.push(CredentialFinding::ProbeFailed {
                    probe: "git config",
                    reason: format!("it exited with status {:?}", captured.code),
                }),
                Err(error) => findings.push(failed("git config", &error)),
            }
        }
    }

    if let Some(gh) = find_executable_in("gh", &search_path) {
        for host in forge_hosts {
            match probe(&gh, &["auth", "token", "--hostname", host], None) {
                Ok(captured) if holds_gh_login(&captured) => {
                    findings.push(CredentialFinding::GhLogin {
                        host: (*host).to_owned(),
                    });
                }
                Ok(_) => {}
                Err(error) => findings.push(failed("gh auth token", &error)),
            }
        }
    }
    findings
}

/// A finding when the harness reported loading any MCP server: with C1's
/// guardrail flags, a run loads only servers passed with `--mcp-config`, and
/// Owlshift passes none.
pub fn mcp_findings(run: &claude::Run) -> Option<CredentialFinding> {
    (!run.mcp_servers.is_empty()).then(|| CredentialFinding::McpServers(run.mcp_servers.clone()))
}

fn failed(probe: &'static str, reason: &str) -> CredentialFinding {
    CredentialFinding::ProbeFailed {
        probe,
        reason: reason.to_owned(),
    }
}

/// Whether `git credential fill` printed a non-empty password.
fn holds_password(stdout: &[u8]) -> bool {
    stdout.split(|&byte| byte == b'\n').any(|line| {
        line.strip_prefix(b"password=")
            .is_some_and(|rest| !rest.trim_ascii().is_empty())
    })
}

/// Whether `gh auth token` printed a token other than the placeholder.
fn holds_gh_login(captured: &Captured) -> bool {
    let token = String::from_utf8_lossy(&captured.stdout);
    let token = token.trim();
    captured.code == Some(0) && !token.is_empty() && token != NO_CREDENTIAL
}

/// The keys, redacted, of the settings in `git config -z --get-regexp`
/// output that carry a credential: an http(s) URL with user info in a remote
/// URL, or in a rewrite's base or value, and any extra HTTP header, which is
/// how a token is usually passed.
fn credential_settings(output: &[u8]) -> Vec<String> {
    output
        .split(|&byte| byte == 0)
        .filter(|entry| !entry.is_empty())
        .filter_map(|entry| {
            let entry = String::from_utf8_lossy(entry);
            let (key, value) = entry.split_once('\n').unwrap_or((&entry, ""));
            let carries = if key.ends_with(".extraheader") {
                !value.trim().is_empty()
            } else {
                has_userinfo(value) || has_userinfo(key)
            };
            carries.then(|| redact(key))
        })
        .collect()
}

/// Whether `text` holds an http(s) URL with user info: `https://<info>@host`.
fn has_userinfo(text: &str) -> bool {
    userinfo_span(text).is_some()
}

/// `text` with the user info of its http(s) URL replaced by `***`.
fn redact(text: &str) -> String {
    match userinfo_span(text) {
        Some((start, end)) => format!("{}***{}", &text[..start], &text[end..]),
        None => text.to_owned(),
    }
}

/// Where the user info of the first http(s) URL in `text` starts and ends.
fn userinfo_span(text: &str) -> Option<(usize, usize)> {
    let lower = text.to_ascii_lowercase();
    let scheme = ["https://", "http://"]
        .into_iter()
        .filter_map(|scheme| lower.find(scheme).map(|at| at + scheme.len()))
        .min()?;
    let authority_end = text[scheme..]
        .find(['/', '?', '#'])
        .map_or(text.len(), |at| scheme + at);
    let at = text[scheme..authority_end].rfind('@')?;
    Some((scheme, scheme + at))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_with_user_info_are_found_and_redacted() {
        for url in [
            "https://user:secret@github.com/o/r.git",
            "https://ghp_token@github.com/o/r.git",
            "HTTP://u:p@example.invalid",
            "url.https://tok@github.com/.insteadof",
        ] {
            assert!(has_userinfo(url), "{url}");
            assert!(
                !redact(url).contains("secret") && !redact(url).contains("tok@"),
                "{url}"
            );
        }
        assert_eq!(
            redact("url.https://tok@github.com/.insteadof"),
            "url.https://***@github.com/.insteadof"
        );
        for url in [
            "https://github.com/o/r.git",
            "ssh://git@github.com/o/r.git",
            "git@github.com:o/r.git",
            "https://github.com/o/r.git?ref=a@b",
            "/srv/repos/r.git",
        ] {
            assert!(!has_userinfo(url), "{url}");
            assert_eq!(redact(url), url);
        }
    }

    #[test]
    fn credential_settings_are_read_from_git_config_output() {
        let output = b"remote.origin.url\nhttps://u:secret@example.invalid/r.git\0\
                       remote.upstream.url\ngit@example.invalid:r.git\0\
                       remote.mirror.pushurl\nhttps://example.invalid/r.git\0\
                       url.https://tok@example.invalid/.insteadof\nhttps://example.invalid/\0\
                       http.https://example.invalid/.extraheader\nAUTHORIZATION: basic eA==\0\
                       http.extraheader\n\0";
        assert_eq!(
            credential_settings(output),
            [
                "remote.origin.url",
                "url.https://***@example.invalid/.insteadof",
                "http.https://example.invalid/.extraheader",
            ]
        );
    }

    #[test]
    fn only_a_real_password_or_login_counts() {
        assert!(holds_password(
            b"protocol=https\nhost=h\nusername=u\npassword=p\n"
        ));
        assert!(!holds_password(b"protocol=https\nhost=h\npassword=\n"));
        assert!(!holds_password(b""));
        let gh = |code, stdout: &str| Captured {
            code: Some(code),
            stdout: stdout.as_bytes().to_vec(),
            stderr: Vec::new(),
        };
        assert!(holds_gh_login(&gh(0, "gho_x\n")));
        assert!(!holds_gh_login(&gh(0, &format!("{NO_CREDENTIAL}\n"))));
        assert!(!holds_gh_login(&gh(1, "")));
    }

    #[test]
    fn a_run_that_loaded_an_mcp_server_is_reported() {
        let mut transcript = claude::Transcript::default();
        transcript.feed(br#"{"type":"system","subtype":"init","mcp_servers":[{"name":"claude.ai Linear","status":"connected"}]}"#);
        let run = transcript.finish(Some(0), Vec::new());
        assert_eq!(
            mcp_findings(&run),
            Some(CredentialFinding::McpServers(vec![
                "claude.ai Linear".into()
            ]))
        );
        let run = claude::Transcript::default().finish(Some(0), Vec::new());
        assert_eq!(mcp_findings(&run), None);
    }

    fn env(pairs: &[(&str, &str)]) -> AgentEnv {
        let parent = pairs
            .iter()
            .map(|(name, value)| (OsString::from(name), OsString::from(value)));
        AgentEnv::new(parent, &[]).unwrap()
    }

    #[test]
    fn an_agent_environment_is_confined_unless_the_bench_says_otherwise() {
        let agent = env(&[("PATH", "/bin")]);
        assert!(agent.is_confined());
        assert!(!agent.without_confinement().is_confined());
    }

    /// The home is opened only where a run needs it, never whole, and the
    /// credential files stay closed even inside an opened folder.
    #[cfg(unix)]
    #[test]
    fn the_policy_opens_what_a_run_needs_and_nothing_of_the_home_itself() {
        let agent = env(&[
            ("HOME", "/home/op"),
            ("PATH", "/home/op/.cargo/bin:/home/op:relative:/usr/bin"),
            ("TMPDIR", "/tmp/op"),
        ]);
        let run = RunPaths {
            workdir: "/srv/wt".into(),
            git_dir: Some("/srv/repo/.git".into()),
            readable: vec!["/opt/claude".into()],
            writable: vec!["/home/op/login".into()],
            hidden: vec!["/srv/run".into()],
            temp: Some("/srv/run-temp".into()),
        };
        let policy = agent.policy(&run);
        let has = |list: &[PathBuf], path: &str| list.iter().any(|p| p == Path::new(path));
        assert_eq!(policy.home.as_deref(), Some(Path::new("/home/op")));
        // The PATH entry that is the home itself, and a relative one, open
        // nothing.
        assert!(!has(&policy.readable, "/home/op"));
        assert!(!has(&policy.readable, "relative"));
        for path in [
            "/home/op/.cargo/bin",
            "/home/op/.gitconfig",
            "/home/op/.config/git",
            "/opt/claude",
        ] {
            assert!(has(&policy.readable, path), "{path}: {:?}", policy.readable);
        }
        assert_eq!(
            policy.writable,
            [
                PathBuf::from("/srv/wt"),
                "/srv/repo/.git".into(),
                "/home/op/login".into()
            ]
        );
        for name in ["hooks", "config", "config.worktree", "info"] {
            assert!(
                has(&policy.protected, &format!("/srv/repo/.git/{name}")),
                "{name}"
            );
        }
        for path in [
            "/home/op/.ssh",
            "/home/op/.config/gh",
            "/home/op/.cargo/credentials.toml",
            "/home/op/.config/git/credentials",
            "/srv/run",
        ] {
            assert!(has(&policy.hidden, path), "{path}");
        }
        // The inherited temporary folder is closed; the run's own is the
        // only one written.
        assert!(has(&policy.closed, "/tmp/op"), "{:?}", policy.closed);
        assert!(has(&policy.closed, &std::env::temp_dir().to_string_lossy()));
        assert_eq!(policy.temp, [PathBuf::from("/srv/run-temp")]);
        assert!(!has(&policy.writable, "/tmp/op"));
        assert_eq!(policy.workdir, Path::new("/srv/wt"));
    }

    /// OWL-68: a folder the `PATH` or a declared variable names in the home
    /// is opened by its real path, and a link naming it is recreated; the
    /// home, a credential folder, one holding a credential, closed or
    /// written paths, a file, a missing path and a value that is no path
    /// open nothing, and an inherited variable's folder is not opened. A
    /// folder named again (`PATH` declared, a name declared in two letter
    /// cases, `PATH` and a variable naming one link) is opened and linked
    /// once.
    #[cfg(unix)]
    #[test]
    fn the_policy_opens_named_folders_by_their_real_path_and_never_a_credential() {
        use std::os::unix::fs::symlink;
        let base = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(base.path()).unwrap();
        let home = base.join("home");
        for dir in [
            ".sdkman/candidates/java/17",
            ".local/bin",
            "go",
            ".nvm",
            ".config/tool",
            ".aws/bin",
            "tools/tmp",
            "wt/sdk",
            "bin",
            "claude",
        ] {
            std::fs::create_dir_all(home.join(dir)).unwrap();
        }
        let java = home.join(".sdkman/candidates/java");
        symlink(java.join("17"), java.join("current")).unwrap();
        symlink(&home, home.join("homelink")).unwrap();
        std::fs::create_dir_all(base.join("opt/jdk")).unwrap();
        symlink(base.join("opt/jdk"), home.join("jdk")).unwrap();
        std::fs::write(home.join("tool.txt"), "").unwrap();

        let at = |path: &str| home.join(path).into_os_string();
        // A `:`-separated list, `~/` naming the home; a URL splits into
        // pieces that are no folder.
        let list = |paths: &[&str]| {
            let entries: Vec<String> = paths
                .iter()
                .map(|path| match path.strip_prefix("~/") {
                    Some(rest) => home.join(rest).display().to_string(),
                    None => (*path).to_owned(),
                })
                .collect();
            OsString::from(entries.join(":"))
        };
        let parent = [
            ("HOME".into(), home.clone().into_os_string()),
            ("TMPDIR".into(), at("tools/tmp")),
            (
                "PATH".into(),
                list(&[
                    "~/bin/..",
                    "~/.local/bin",
                    "~/.sdkman/candidates/java/current",
                    "/usr/bin",
                ]),
            ),
            ("CLAUDE_CONFIG_DIR".into(), at("claude")),
            ("SDK_A".into(), at(".sdkman/candidates/java/current")),
            ("sdk_a".into(), at("go")),
            (
                "TOOL_PATHS".into(),
                list(&[
                    "~/.nvm",
                    "~/homelink",
                    "~/.config",
                    "~/.aws/bin",
                    "~/tools",
                    "~/wt/sdk",
                    "~/tool.txt",
                    "~/missing",
                    "~/jdk",
                    "on",
                    "postgres://u:p@h/db",
                ]),
            ),
        ];
        let declared = ["SDK_A", "TOOL_PATHS", "sdk_a", "PATH"];
        let agent = AgentEnv::for_project(parent, &declared, &declared).unwrap();
        let run = RunPaths {
            workdir: home.join("wt"),
            ..RunPaths::default()
        };
        let policy = agent.policy(&run);

        let fixed = [
            ".gitconfig",
            ".rustup",
            ".cargo/bin",
            ".cargo/registry",
            ".cargo/git",
            ".config/git",
        ]
        .map(|path| home.join(path));
        let named: Vec<&PathBuf> = policy
            .readable
            .iter()
            .filter(|path| !fixed.contains(path))
            .collect();
        assert_eq!(
            named,
            [
                &home.join(".local/bin"),
                &java.join("17"),
                &home.join("go"),
                &home.join(".nvm"),
            ]
        );
        assert_eq!(
            policy.links,
            [
                (java.join("current"), java.join("17")),
                (home.join("jdk"), base.join("opt/jdk")),
            ]
        );
    }

    /// Whether agent runs can be confined here, as `executor::gate`'s tests
    /// ask: skipped with a message where they cannot, unless
    /// `OWLSHIFT_REQUIRE_CONFINEMENT` is set.
    #[cfg(unix)]
    fn sandbox_or_skip() -> bool {
        match sandbox::available() {
            Ok(()) => true,
            Err(error) => {
                assert!(
                    std::env::var_os("OWLSHIFT_REQUIRE_CONFINEMENT").is_none(),
                    "confinement is required here, but: {error}"
                );
                eprintln!("skipped: agent runs cannot be confined here: {error}");
                false
            }
        }
    }

    /// OWL-68's acceptance, with the real sandbox: a confined command, as the
    /// gate is spawned, reads a tool chain a declared variable names through
    /// sdkman's `current` link, and reads nothing in a declared folder that
    /// holds a credential folder or lies in one.
    #[cfg(unix)]
    #[test]
    fn a_confined_command_reads_a_declared_tool_chain_and_no_credential_folder() {
        use std::os::unix::fs::symlink;
        if !sandbox_or_skip() {
            return;
        }
        let base = tempfile::tempdir().unwrap();
        let home = base.path().join("home");
        let worktree = base.path().join("worktree");
        let java = home.join(".sdkman/candidates/java");
        let config = home.join(".config");
        let cloud_bin = home.join(".aws/bin");
        for dir in [
            &java.join("17/bin"),
            &config.join("tool"),
            &cloud_bin,
            &worktree,
        ] {
            std::fs::create_dir_all(dir).unwrap();
        }
        symlink(java.join("17"), java.join("current")).unwrap();
        std::fs::write(java.join("17/bin/java"), "JDK_owl68\n").unwrap();
        std::fs::write(config.join("tool/settings"), "FAKE_owl68\n").unwrap();
        std::fs::write(cloud_bin.join("tool"), "FAKE_owl68\n").unwrap();

        let declared = ["JAVA_HOME", "CONFIG_DIR", "CLOUD_BIN"];
        let parent = std::env::vars_os()
            .filter(|(name, _)| {
                let name = name.to_string_lossy();
                !["HOME", "TMPDIR", "XDG_CONFIG_HOME"].contains(&&*name)
                    && !declared.iter().any(|d| d.eq_ignore_ascii_case(&name))
            })
            .chain([
                ("HOME".into(), home.clone().into_os_string()),
                ("JAVA_HOME".into(), java.join("current").into_os_string()),
                ("CONFIG_DIR".into(), config.clone().into_os_string()),
                ("CLOUD_BIN".into(), cloud_bin.clone().into_os_string()),
            ]);
        let agent = AgentEnv::for_project(parent, &declared, &declared).unwrap();
        let run = RunPaths {
            workdir: worktree,
            ..RunPaths::default()
        };
        let cat = |file: &Path| {
            let mut inner = Command::new("/bin/cat");
            inner.arg(file);
            agent.confine(inner, &run).unwrap().output().unwrap()
        };

        let out = cat(&java.join("current/bin/java"));
        assert!(out.status.success(), "{out:?}");
        assert_eq!(out.stdout, b"JDK_owl68\n");
        for file in [config.join("tool/settings"), cloud_bin.join("tool")] {
            let out = cat(&file);
            assert!(!out.status.success(), "{}: {out:?}", file.display());
            assert!(!String::from_utf8_lossy(&out.stdout).contains("FAKE_owl68"));
        }
    }

    /// The shipped binary never turns confinement off: the switches, this
    /// one and the Windows launcher a test sets (OWL-71), are compiled for
    /// tests alone, and the CLI's code never names them.
    #[test]
    fn the_cli_never_turns_confinement_off() {
        let cli = Path::new(env!("CARGO_MANIFEST_DIR")).join("../owlshift-cli/src");
        let mut stack = vec![cli];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().is_some_and(|ext| ext == "rs") {
                    let text = std::fs::read_to_string(&path).unwrap();
                    for switch in ["without_confinement", "use_built_launcher"] {
                        assert!(!text.contains(switch), "{} names {switch}", path.display());
                    }
                }
            }
        }
    }

    #[test]
    fn findings_name_what_they_found_and_never_a_value() {
        let findings = [
            CredentialFinding::Variables(vec!["LINEAR_API_KEY".into()]),
            CredentialFinding::GitCredential {
                host: "github.com".into(),
            },
            CredentialFinding::GhLogin {
                host: "github.com".into(),
            },
            CredentialFinding::GitSetting {
                key: "remote.origin.url".into(),
            },
        ];
        let text: Vec<String> = findings.iter().map(ToString::to_string).collect();
        assert_eq!(
            text,
            [
                "credential variables would reach the agent: LINEAR_API_KEY",
                "git hands the agent a credential for https://github.com (a credential helper \
                 or an askpass program)",
                "gh hands the agent a login for github.com",
                "the git setting remote.origin.url carries a credential",
            ]
        );
    }
}
