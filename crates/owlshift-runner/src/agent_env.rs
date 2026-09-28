//! Spawning agents with the environment of [`owlshift_core::agent_env`], and
//! checking that no credential is left within their reach (OWL-22).
//!
//! For the executor (OWL-15): before a run, [`AgentEnv::from_runner`] builds
//! the environment and [`AgentEnv::check`] probes it in the worktree, and a
//! finding stops the run before it starts; the harness command is spawned
//! after [`AgentEnv::apply`]. After the run, [`mcp_findings`] reads the MCP
//! servers the harness reported loading.
//!
//! The probes run git and gh as the agent would, with its environment and in
//! its directory. They never keep, log or display a secret: a finding names a
//! variable, a host or a redacted setting, never a value.

use std::ffi::OsString;
use std::fmt;
use std::path::Path;
use std::process::Command;

use owlshift_adapters::harness::claude;
use owlshift_core::agent_env::{
    AgentEnvError, NO_CREDENTIAL, agent_environment, check_agent_variables,
};
use owlshift_core::floor::FloorViolation;
use owlshift_platform::process::find_executable_in;

use crate::system::{Captured, PROBE_TIMEOUT, RunError, run_command};

/// The environment an agent process gets: see [`owlshift_core::agent_env`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentEnv {
    vars: Vec<(OsString, OsString)>,
}

impl AgentEnv {
    /// The agent environment built from `parent`, with the variables named in
    /// `declared` (the project's, for its gate).
    pub fn new(
        parent: impl IntoIterator<Item = (OsString, OsString)>,
        declared: &[&str],
    ) -> Result<Self, AgentEnvError> {
        agent_environment(parent, declared).map(|vars| Self { vars })
    }

    /// The agent environment built from the runner's own.
    pub fn from_runner(declared: &[&str]) -> Result<Self, AgentEnvError> {
        Self::new(std::env::vars_os(), declared)
    }

    /// The variables, sorted by name.
    pub fn vars(&self) -> &[(OsString, OsString)] {
        &self.vars
    }

    /// Gives `command` exactly these variables. It replaces the command's
    /// whole environment, including anything set on it before.
    pub fn apply(&self, command: &mut Command) {
        command
            .env_clear()
            .envs(self.vars.iter().map(|(n, v)| (n, v)));
    }

    /// [`check_environment`] with these variables.
    pub fn check(&self, workdir: &Path, forge_hosts: &[&str]) -> Vec<CredentialFinding> {
        check_environment(&self.vars, workdir, forge_hosts)
    }
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
    let probe = |program: &Path, args: &[&str], input: &[u8]| {
        let mut command = Command::new(program);
        command
            .args(args)
            .current_dir(workdir)
            .env_clear()
            .envs(vars.iter().map(|(n, v)| (n, v)));
        run_command(command, input, PROBE_TIMEOUT)
    };

    match find_executable_in("git", &search_path) {
        None => findings.push(CredentialFinding::ProbeFailed {
            probe: "git",
            reason: "git is not on the agent's PATH".to_owned(),
        }),
        Some(git) => {
            for host in forge_hosts {
                let request = format!("protocol=https\nhost={host}\n\n");
                match probe(&git, &["credential", "fill"], request.as_bytes()) {
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
            match probe(&git, &args, b"") {
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
            match probe(&gh, &["auth", "token", "--hostname", host], b"") {
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

fn failed(probe: &'static str, error: &RunError) -> CredentialFinding {
    CredentialFinding::ProbeFailed {
        probe,
        reason: error.to_string(),
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
