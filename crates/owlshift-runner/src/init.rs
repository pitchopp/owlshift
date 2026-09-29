//! `owlshift init`: the project file, and the secrets `owlshift do` needs on
//! this machine (build plan, OWL-20).
//!
//! The project file is written from a commented template once, and never
//! overwritten: it is committed, and a person edits it from there. The
//! secrets go to the system keychain, never to a file, an argument, an
//! event or a message: the tracker's (a Linear API key, when the tracker is
//! Linear) and the forge's (a GitHub token). The command line asks for them
//! on a terminal only; this module decides which are missing and stores
//! what it is given.

use std::fmt;
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::Path;

use semver::Version;

use owlshift_contracts::config::{ProjectConfig, TrackerKind};
use owlshift_platform::keychain::{Keychain, KeychainError, SERVICE, Secret};

use crate::config::{OWLSHIFT_VERSION, PROJECT_FILE};
use crate::forge::{GITHUB_ACCOUNT, GITHUB_TOKEN_HELP};
use crate::tracker::LINEAR_ACCOUNT;

/// The values of a new project file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InitOptions {
    pub tracker: TrackerKind,
    /// The Linear team key; required for Linear.
    pub team: Option<String>,
    /// The project's gate commands, in order.
    pub gate: Vec<String>,
}

/// The commented project file for `options`, checked to parse as the
/// contract says before it is returned.
pub fn project_file(options: &InitOptions) -> Result<String, String> {
    let quote = |text: &str| toml::Value::String(text.to_owned()).to_string();
    let version = Version::parse(OWLSHIFT_VERSION).expect("the crate version is semver");
    let mut text = format!(
        "\
# Owlshift project configuration. Commit it: every developer and every
# runner of the project read this file. It holds no secret: the tracker and
# forge credentials live in each machine's system keychain (`owlshift init`
# stores them), and the model logins stay with each harness CLI.

# The Owlshift versions that can read this file.
requires = \">={}.{}\"

[tracker]
",
        version.major, version.minor
    );
    match options.tracker {
        TrackerKind::Linear => {
            let team = options
                .team
                .as_deref()
                .map(str::trim)
                .filter(|team| !team.is_empty())
                .ok_or("a Linear project needs its team key: pass --team, such as --team OWL")?;
            text.push_str(&format!(
                "\
# Where the tickets live: \"linear\", or \"markdown\" for tickets kept in
# this repository, one folder per ticket under tickets/.
kind = \"linear\"
# The Linear team key: the prefix of its ticket ids, OWL in OWL-12.
team = {}
",
                quote(team)
            ));
        }
        TrackerKind::Markdown => text.push_str(
            "\
# Where the tickets live: \"markdown\" for tickets kept in this repository,
# one folder per ticket under tickets/, or \"linear\" with its `team` key.
kind = \"markdown\"
",
        ),
    }
    let gate = if options.gate.is_empty() {
        "gate = []\n".to_owned()
    } else {
        let commands: Vec<String> = options
            .gate
            .iter()
            .map(|command| format!("    {},\n", quote(command)))
            .collect();
        format!("gate = [\n{}]\n", commands.concat())
    };
    text.push_str(&format!(
        "\
# How a person admits a ticket to Owlshift's queue: {{ label = \"...\" }},
# {{ state = \"...\" }} or \"delegation\". `owlshift do TICKET` does not use
# it: it runs the ticket it is given.
admit = {{ label = \"agent\" }}
# The tracker states that show where a ticket stands.
states = {{ ready = \"Todo\", working = \"In Progress\", needs_input = \"Needs Input\", review = \"In Review\" }}

[stack]
# The project's full gate: lint, formatter and tests, run in order from the
# repository root. The build role runs it before it reports done, then
# Owlshift runs it again on the commit it delivers. With no command, the
# build role stops with `blocked`.
{gate}# Agents start from an almost empty environment. Name here the variables of
# your environment the gate needs, such as a feature flag; the whole agent
# run sees them. Credential variables are refused, and each machine that
# runs Owlshift passes only the names its operator lists in `allow_gate_env`
# in the personal configuration. A path under your home stays unreadable to
# the sandboxed run unless it is on the PATH.
# gate_env = [\"FEATURE_FLAGS\"]
# The files agents receive as the project's rules, taken whole from the
# default branch. Left out, the root AGENTS.md is used when there is one.
# Once set, every file listed must exist there; [] sends no rule.
# rules = [\"AGENTS.md\", \".claude/rules/testing.md\"]

[pipeline]
# A ticket's pipeline variant, unless intake or a label picks another:
# \"trivial\", \"standard\" or \"risky\". In this version, `owlshift do` runs
# the build stage only.
default = \"standard\"
# When a human approves the plan: \"always\", \"on-fork\" or \"never\".
plan_approval = \"on-fork\"

[models]
# The model of each tier, per harness; left out, a harness runs its own
# default model. `owlshift do` builds on the standard tier. For example:
# deep = {{ claude = \"claude-opus-5-5\" }}
# standard = {{ claude = \"claude-sonnet-5\" }}
# fast = {{ claude = \"claude-haiku-4-5\" }}

[policy]
# Question categories always left to a human, on top of the floor's
# security, data_loss, money, legal, irreversible and scope.
always_human = []
"
    ));
    ProjectConfig::parse(&text)
        .map_err(|error| format!("the project file this would write is invalid: {error}"))?;
    Ok(text)
}

/// What [`write_project_file`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Written {
    Created,
    /// A project file was already there; it is left as it is.
    Kept,
}

/// Writes `text` as the project file at `root`, unless one exists.
pub fn write_project_file(root: &Path, text: &str) -> io::Result<Written> {
    let created = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(root.join(PROJECT_FILE));
    match created {
        Ok(mut file) => {
            file.write_all(text.as_bytes())?;
            Ok(Written::Created)
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(Written::Kept),
        Err(error) => Err(error),
    }
}

/// A secret `owlshift do` reads from the keychain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SecretSpec {
    /// The keychain account, under service [`SERVICE`].
    pub account: &'static str,
    /// What to call it when asking for it.
    pub label: &'static str,
    /// Where it comes from and what it must allow.
    pub help: &'static str,
}

impl fmt::Display for SecretSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} (keychain service `{SERVICE}`, account `{}`)",
            self.label, self.account
        )
    }
}

/// The Linear API key.
pub const LINEAR_KEY: SecretSpec = SecretSpec {
    account: LINEAR_ACCOUNT,
    label: "Linear API key",
    help: "a personal API key, created in Linear's settings under Security & access",
};

/// The GitHub token.
pub const GITHUB_TOKEN: SecretSpec = SecretSpec {
    account: GITHUB_ACCOUNT,
    label: "GitHub token",
    help: GITHUB_TOKEN_HELP,
};

/// The secrets `owlshift do` needs for a project on `tracker`: the forge's
/// always, the tracker's for Linear.
pub fn required_secrets(tracker: TrackerKind) -> Vec<SecretSpec> {
    match tracker {
        TrackerKind::Linear => vec![LINEAR_KEY, GITHUB_TOKEN],
        TrackerKind::Markdown => vec![GITHUB_TOKEN],
    }
}

/// What [`store_secrets`] did with each secret.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SecretsReport {
    /// Given and stored.
    pub stored: Vec<SecretSpec>,
    /// Already in the keychain, and left there.
    pub kept: Vec<SecretSpec>,
    /// Neither in the keychain nor given.
    pub missing: Vec<SecretSpec>,
}

/// Makes sure each of `specs` is in `keychain`. One already there is kept,
/// unless `replace`; for any other, `ask` is called and what it gives is
/// stored. `ask` answers `None` when it cannot ask, or was given nothing:
/// the secret is then kept when it exists, missing otherwise. A secret is
/// never part of the report or of an error.
pub fn store_secrets(
    keychain: &Keychain,
    specs: &[SecretSpec],
    replace: bool,
    ask: &mut dyn FnMut(&SecretSpec) -> Option<Secret>,
) -> Result<SecretsReport, KeychainError> {
    let mut report = SecretsReport::default();
    for spec in specs {
        let present = keychain.read(spec.account)?.is_some();
        if present && !replace {
            report.kept.push(*spec);
            continue;
        }
        let given = ask(spec).filter(|secret| !secret.expose().trim().is_empty());
        match given {
            Some(secret) => {
                keychain.store(spec.account, &Secret::new(secret.expose().trim()))?;
                report.stored.push(*spec);
            }
            None if present => report.kept.push(*spec),
            None => report.missing.push(*spec),
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(tracker: TrackerKind, team: Option<&str>, gate: &[&str]) -> InitOptions {
        InitOptions {
            tracker,
            team: team.map(ToOwned::to_owned),
            gate: gate.iter().map(|c| (*c).to_owned()).collect(),
        }
    }

    #[test]
    fn the_template_parses_for_either_tracker_with_or_without_a_gate() {
        let quoted = r#"sh -c "echo \"hi\"" \ done"#;
        let linear = project_file(&options(
            TrackerKind::Linear,
            Some("OWL"),
            &["cargo test", quoted],
        ))
        .unwrap();
        let config = ProjectConfig::parse(&linear).unwrap();
        assert_eq!(config.tracker.team.as_deref(), Some("OWL"));
        assert_eq!(config.stack.gate, ["cargo test", quoted]);
        assert!(
            linear.lines().filter(|l| l.starts_with('#')).count() > 10,
            "{linear}"
        );

        let markdown = project_file(&options(TrackerKind::Markdown, None, &[])).unwrap();
        let config = ProjectConfig::parse(&markdown).unwrap();
        assert_eq!(config.tracker.kind, TrackerKind::Markdown);
        assert!(config.stack.gate.is_empty());

        let error = project_file(&options(TrackerKind::Linear, Some(" "), &[])).unwrap_err();
        assert!(error.contains("--team"), "{error}");
    }

    #[test]
    fn an_existing_project_file_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            write_project_file(dir.path(), "a = 1\n").unwrap(),
            Written::Created
        );
        assert_eq!(
            write_project_file(dir.path(), "b = 2\n").unwrap(),
            Written::Kept
        );
        let text = std::fs::read_to_string(dir.path().join(PROJECT_FILE)).unwrap();
        assert_eq!(text, "a = 1\n");
    }

    #[test]
    fn secrets_are_asked_only_when_missing_and_never_reported() {
        const SENTINEL: &str = "lin_api_SENTINEL_never_shown";
        let keychain = Keychain::in_memory();
        keychain
            .store(GITHUB_ACCOUNT, &Secret::new("ghp_SENTINEL_never_shown"))
            .unwrap();
        let mut asked = Vec::new();
        let report = store_secrets(
            &keychain,
            &required_secrets(TrackerKind::Linear),
            false,
            &mut |spec| {
                asked.push(spec.account);
                Some(Secret::new(format!("  {SENTINEL}\n")))
            },
        )
        .unwrap();
        assert_eq!(asked, [LINEAR_ACCOUNT]);
        assert_eq!(report.stored, [LINEAR_KEY]);
        assert_eq!(report.kept, [GITHUB_TOKEN]);
        assert!(report.missing.is_empty());
        // Stored trimmed, and absent from everything a person is shown.
        assert_eq!(
            keychain.read(LINEAR_ACCOUNT).unwrap().unwrap().expose(),
            SENTINEL
        );
        let shown = format!("{report:?} {} {}", LINEAR_KEY, GITHUB_TOKEN);
        assert!(!shown.contains("SENTINEL"), "{shown}");

        // Replacing asks again; an empty answer keeps what is there, and a
        // secret nobody gives is missing.
        let report = store_secrets(&keychain, &[LINEAR_KEY], true, &mut |_| {
            Some(Secret::new(""))
        })
        .unwrap();
        assert_eq!(report.kept, [LINEAR_KEY]);
        keychain.delete(GITHUB_ACCOUNT).unwrap();
        let report = store_secrets(&keychain, &[GITHUB_TOKEN], false, &mut |_| None).unwrap();
        assert_eq!(report.missing, [GITHUB_TOKEN]);
    }
}
