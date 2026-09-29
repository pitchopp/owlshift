//! An agent run reaches no tracker or forge credential (OWL-22).
//!
//! A host is planted with the credentials an agent would otherwise inherit:
//! token variables, an SSH agent socket, the variables of a parent Claude
//! Code session, a git credential helper in the global configuration and a
//! gh login in gh's configuration. The check finds each of them in the host's
//! environment as it is, and none in the agent environment built from it,
//! whose child processes do not see them either. Git and gh are the real
//! ones; the system git configuration is read in the agent's case, so a
//! helper it names (osxkeychain, Git Credential Manager) is reset too.
//!
//! The fixture writes its own home rather than using the testkit's hermetic
//! git environment, which removes every `GIT_*` variable and the system
//! configuration: the very things the agent environment has to overcome.
//!
//! gh ships on the CI runners and is required there; elsewhere a missing gh
//! only skips its part, with a message. gh is started once before any probe
//! runs ([`owlshift_testkit::gh::warm_up`]): its first start on a machine can
//! take longer than a probe is given. Git for Windows' `sh`, which the
//! fixture's credential helper starts, took 0.3 to 0.7 s on fresh runners
//! (OWL-56): it needs no warm-up.
//!
//! The agent environment is checked bare (`without_confinement`, behind the
//! runner's `testkit` feature): this is the environment layer, which must
//! hold on its own; the sandbox wrapped around it (OWL-41) is checked in
//! `confinement.rs`. The test lives in the bench for the gh warm-up, which
//! the runner's own tests cannot reach.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

use owlshift_core::agent_env::NO_CREDENTIAL;
use owlshift_runner::agent_env::{AgentEnv, CredentialFinding, check_environment};
use owlshift_testkit::gh;

const FORGE: &[&str] = &["github.com"];

/// The variables planted in the host's environment, all credentials or
/// traces of a parent harness session.
const PLANTED: &[(&str, &str)] = &[
    ("LINEAR_API_KEY", "fixture-linear-secret"),
    ("GITHUB_TOKEN", "fixture-github-secret"),
    ("AWS_SECRET_ACCESS_KEY", "fixture-aws-secret"),
    ("SSH_AUTH_SOCK", "/nonexistent/owlshift-fixture-agent.sock"),
    ("CLAUDECODE", "1"),
    ("ANTHROPIC_BASE_URL", "http://127.0.0.1:9"),
];

struct Host {
    _tmp: TempDir,
    /// The runner's environment on that host.
    parent: Vec<(OsString, OsString)>,
    /// A repository with nothing in its own configuration.
    clean: PathBuf,
    /// A repository whose configuration carries credentials.
    leaky: PathBuf,
}

fn host() -> Host {
    let tmp = tempfile::Builder::new()
        .prefix("owlshift agent env ")
        .tempdir()
        .unwrap();
    let home = tmp.path().join("home");
    let config = home.join(".config");
    fs::create_dir_all(config.join("gh")).unwrap();
    fs::write(
        home.join(".gitconfig"),
        "[credential]\n\
         \thelper = \"!f() { echo username=fixture; echo password=fixture-git-secret; }; f\"\n\
         [user]\n\tname = Fixture\n\temail = fixture@owlshift.invalid\n\
         [init]\n\tdefaultBranch = main\n",
    )
    .unwrap();
    // gh's layout since its multi-account migration, which then leaves the
    // files alone.
    fs::write(
        config.join("gh").join("hosts.yml"),
        "github.com:\n    users:\n        fixture-user:\n            oauth_token: fixture-gh-secret\n    \
         git_protocol: https\n    oauth_token: fixture-gh-secret\n    user: fixture-user\n",
    )
    .unwrap();
    fs::write(config.join("gh").join("config.yml"), "version: \"1\"\n").unwrap();

    // The real environment runs the programs (PATH, and on Windows its system
    // variables); what the fixture sets, and anything git or gh would read
    // instead of it, is replaced.
    let replaced = |name: &OsStr| {
        let name = name.to_string_lossy().to_ascii_uppercase();
        ["HOME", "USERPROFILE", "XDG_CONFIG_HOME"].contains(&name.as_str())
            || PLANTED.iter().any(|(planted, _)| *planted == name)
            || ["GIT_", "GH_", "GITHUB_"]
                .iter()
                .any(|prefix| name.starts_with(prefix))
    };
    let mut parent: Vec<(OsString, OsString)> = std::env::vars_os()
        .filter(|(name, _)| !replaced(name))
        .collect();
    let set = |name: &str, value: &OsStr| (OsString::from(name), value.to_owned());
    parent.extend([
        set("HOME", home.as_os_str()),
        set("USERPROFILE", home.as_os_str()),
        set("XDG_CONFIG_HOME", config.as_os_str()),
        // The host's own environment reads the fixture's configuration only,
        // so the planted helper answers and no system helper waits for a
        // person; the agent's case drops both and reads the system's too.
        set("GIT_CONFIG_NOSYSTEM", OsStr::new("1")),
        set("GIT_TERMINAL_PROMPT", OsStr::new("0")),
    ]);
    parent.extend(
        PLANTED
            .iter()
            .map(|(name, value)| set(name, OsStr::new(value))),
    );
    gh::warm_up(&parent);

    let clean = tmp.path().join("clean");
    let leaky = tmp.path().join("leaky");
    git(&parent, tmp.path(), &["init", "--quiet", "clean"]);
    git(&parent, tmp.path(), &["init", "--quiet", "leaky"]);
    for (key, value) in [
        (
            "remote.origin.url",
            "https://fixture-user:fixture-secret@example.invalid/r.git",
        ),
        (
            "url.https://fixture-token@example.invalid/.insteadOf",
            "https://example.invalid/",
        ),
        (
            "http.https://example.invalid/.extraHeader",
            "AUTHORIZATION: basic Zml4dHVyZQ==",
        ),
    ] {
        git(&parent, &leaky, &["config", key, value]);
    }
    Host {
        _tmp: tmp,
        parent,
        clean,
        leaky,
    }
}

fn git(env: &[(OsString, OsString)], dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env_clear()
        .envs(env.iter().map(|(n, v)| (n, v)))
        .output()
        .unwrap();
    assert!(output.status.success(), "git {args:?}: {output:?}");
}

/// Whether gh is installed; on CI it must be.
fn gh_installed(env: &[(OsString, OsString)]) -> bool {
    let installed = gh::on_path(env).is_some();
    if !installed {
        assert!(
            std::env::var_os("CI").is_none(),
            "gh ships on the CI runners and this test needs it there"
        );
        eprintln!("gh is not installed: its login is not checked");
    }
    installed
}

#[test]
fn the_check_finds_every_planted_credential_in_the_host_environment() {
    let host = host();
    let findings = check_environment(&host.parent, &host.clean, FORGE);

    let Some(CredentialFinding::Variables(names)) = findings.first() else {
        panic!("no credential variable found: {findings:?}");
    };
    for credential in [
        "LINEAR_API_KEY",
        "GITHUB_TOKEN",
        "AWS_SECRET_ACCESS_KEY",
        "SSH_AUTH_SOCK",
    ] {
        assert!(
            names.iter().any(|n| n == credential),
            "{credential}: {names:?}"
        );
    }
    let found = |finding: CredentialFinding| findings.contains(&finding);
    assert!(
        found(CredentialFinding::GitCredential {
            host: "github.com".into()
        }),
        "{findings:?}"
    );
    if gh_installed(&host.parent) {
        assert!(
            found(CredentialFinding::GhLogin {
                host: "github.com".into()
            }),
            "{findings:?}"
        );
    }
    for finding in &findings {
        assert!(!finding.to_string().contains("secret"), "{finding}");
    }
}

#[test]
fn an_agent_reaches_none_of_them() {
    let host = host();
    let agent = AgentEnv::new(host.parent.clone())
        .unwrap()
        .without_confinement();
    // Part of the check: gh is asked, and answers with the placeholder.
    gh_installed(agent.vars());
    assert_eq!(agent.check(&host.clean, FORGE), []);

    // What a process spawned as an agent actually sees.
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.args([
        "--exact",
        "helper_print_the_environment",
        "--ignored",
        "--nocapture",
        "--test-threads=1",
    ]);
    agent.apply(&mut command);
    let output = command.output().unwrap();
    assert!(output.status.success(), "{output:?}");
    let seen: Vec<(String, String)> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.strip_prefix("ENV "))
        .filter_map(|pair| pair.split_once('='))
        .map(|(name, value)| (name.to_ascii_uppercase(), value.to_owned()))
        .collect();
    let value = |name: &str| {
        seen.iter()
            .find(|(seen, _)| seen == name)
            .map(|(_, value)| value.as_str())
    };
    assert!(value("PATH").is_some(), "{seen:?}");
    assert_eq!(value("GH_TOKEN"), Some(NO_CREDENTIAL));
    for (name, _) in PLANTED {
        assert_eq!(value(name), None, "{name}");
    }
    assert_eq!(value("GIT_CONFIG_NOSYSTEM"), None);
}

#[test]
fn credentials_in_the_repository_configuration_are_reported_redacted() {
    let host = host();
    let agent = AgentEnv::new(host.parent.clone())
        .unwrap()
        .without_confinement();
    let findings = agent.check(&host.leaky, FORGE);
    let keys: Vec<String> = findings
        .iter()
        .map(|finding| match finding {
            CredentialFinding::GitSetting { key } => key.clone(),
            other => panic!("unexpected finding: {other}"),
        })
        .collect();
    assert_eq!(
        keys,
        [
            "remote.origin.url",
            "url.https://***@example.invalid/.insteadof",
            "http.https://example.invalid/.extraheader",
        ]
    );
}

/// Prints the environment it was given, for [`an_agent_reaches_none_of_them`].
#[test]
#[ignore = "helper, run by an_agent_reaches_none_of_them"]
fn helper_print_the_environment() {
    if std::env::args().any(|arg| arg == "--exact") {
        for (name, value) in std::env::vars_os() {
            println!("ENV {}={}", name.to_string_lossy(), value.to_string_lossy());
        }
    }
}
