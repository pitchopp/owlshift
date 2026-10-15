//! `owlshift init`: the project file, and the secrets `owlshift do` needs on
//! this machine (build plan, OWL-20).
//!
//! The project file is written from a commented template once, and never
//! overwritten: it is committed, and a person edits it from there. The
//! secrets go to the system keychain, never to a file, an argument, an
//! event or a message: the tracker's (a Linear API key, when the tracker is
//! Linear, and optionally the client ID and secret of the Linear app Owlshift
//! writes as, OWL-157), the forge's (a GitHub token) and the token agent runs
//! log in to Claude Code with (OWL-94). The command line asks for them on a terminal
//! only; this module decides which are missing and stores what it is given.
//! The tracker's and the forge's are the project's, under accounts named by
//! its GitHub repository; the Claude Code token is one per machine (OWL-308,
//! [`crate::secrets`]).

use std::fmt;
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::Path;

use semver::Version;

use owlshift_adapters::forge::Repo;
use owlshift_contracts::config::{ProjectConfig, TrackerKind};
use owlshift_platform::keychain::{Keychain, KeychainError, SERVICE, Secret};

use crate::config::{OWLSHIFT_VERSION, PROJECT_FILE};
use crate::executor::harness::CLAUDE_AGENT_ACCOUNT;
use crate::forge::{GITHUB_ACCOUNT, GITHUB_TOKEN_HELP};
use crate::secrets::{self, Source, TrackerAccounts};
use crate::tracker::{LINEAR_ACCOUNT, LINEAR_APP_ID_ACCOUNT, LINEAR_APP_SECRET_ACCOUNT};

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
# forge credentials, and the token agent runs log in to Claude Code with,
# live in each machine's system keychain (`owlshift init` stores them).

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
    /// The keychain account, under service [`SERVICE`], of a secret kept one
    /// per machine; for a project's secret, the shared account its own
    /// account's name ends with ([`secrets::project_account`]).
    pub account: &'static str,
    /// Whether each project keeps its own (OWL-308).
    pub per_project: bool,
    /// What to call it when asking for it.
    pub label: &'static str,
    /// Where it comes from and what it must allow.
    pub help: &'static str,
}

/// The Linear API key.
pub const LINEAR_KEY: SecretSpec = SecretSpec {
    account: LINEAR_ACCOUNT,
    per_project: true,
    label: "Linear API key",
    help: "a personal API key, created in Linear's settings under Security & access",
};

/// The GitHub token.
pub const GITHUB_TOKEN: SecretSpec = SecretSpec {
    account: GITHUB_ACCOUNT,
    per_project: true,
    label: "GitHub token",
    help: GITHUB_TOKEN_HELP,
};

/// The Claude Code token agent runs log in with (OWL-94): confined, they
/// cannot reach the Keychain, where the operator's own login lives. One
/// per machine: a model login, not a project's secret.
pub const CLAUDE_TOKEN: SecretSpec = SecretSpec {
    account: CLAUDE_AGENT_ACCOUNT,
    per_project: false,
    label: "Claude Code token for agent runs",
    help: "a long-lived token of your Claude subscription, which agent runs log in with: run \
           `claude setup-token` and paste the token it prints",
};

/// The client ID of the Linear OAuth app Owlshift writes as (decision D8,
/// OWL-157): optional, and asked with [`LINEAR_APP_SECRET`] by
/// [`store_app_credentials`].
pub const LINEAR_APP_ID: SecretSpec = SecretSpec {
    account: LINEAR_APP_ID_ACCOUNT,
    per_project: true,
    label: "Linear app client ID",
    help: "optional: the client ID of the OAuth application a workspace admin creates in \
           Linear's API settings, with client credentials on, so that Owlshift comments as its \
           app user and Linear notifies the decider; leave it empty to comment through the API \
           key, whose holder Linear does not notify",
};

/// That app's client secret.
pub const LINEAR_APP_SECRET: SecretSpec = SecretSpec {
    account: LINEAR_APP_SECRET_ACCOUNT,
    per_project: true,
    label: "Linear app client secret",
    help: "the client secret of that OAuth application",
};

/// The secrets `owlshift do` needs for a project on `tracker`: the forge's
/// and the agent runs' Claude Code token always, since `do` runs Claude
/// Code, and the tracker's for Linear.
pub fn required_secrets(tracker: TrackerKind) -> Vec<SecretSpec> {
    match tracker {
        TrackerKind::Linear => vec![LINEAR_KEY, GITHUB_TOKEN, CLAUDE_TOKEN],
        TrackerKind::Markdown => vec![GITHUB_TOKEN, CLAUDE_TOKEN],
    }
}

/// A secret `init` makes sure of, with the account it is stored under.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Wanted {
    pub spec: SecretSpec,
    /// Where `init` stores it: the project's own account, or the machine's.
    pub account: String,
    /// The shared account the project reads it from today, having none of
    /// its own: an empty answer keeps using it.
    pub shared: Option<String>,
}

impl Wanted {
    /// `spec`, under the project's own account when it is a project's.
    pub fn new(spec: SecretSpec, repo: &Repo) -> Self {
        let account = if spec.per_project {
            secrets::project_account(repo, spec.account)
        } else {
            spec.account.to_owned()
        };
        Self {
            spec,
            account,
            shared: None,
        }
    }
}

impl fmt::Display for Wanted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} (keychain service `{SERVICE}`, account `{}`)",
            self.spec.label, self.account
        )
    }
}

/// What `init` makes sure of for `specs` in the project of `repo`, each
/// under the account it is stored under, with the shared account a project
/// secret is read from today when the project has none of its own, resolved
/// as `owlshift do` resolves it ([`secrets`]). Without a repository, the
/// project's secrets cannot be named: they come back apart, and only the
/// machine's are wanted.
pub fn wanted_secrets(
    keychain: &Keychain,
    specs: &[SecretSpec],
    repo: Option<&Repo>,
) -> Result<(Vec<Wanted>, Vec<SecretSpec>), String> {
    let mut wanted = Vec::new();
    let mut unnamed = Vec::new();
    for spec in specs {
        if !spec.per_project {
            wanted.push(Wanted {
                spec: *spec,
                account: spec.account.to_owned(),
                shared: None,
            });
            continue;
        }
        let Some(repo) = repo else {
            unnamed.push(*spec);
            continue;
        };
        let mut look = |account: &str| {
            keychain
                .contains(account)
                .map(|found| found.then_some(()))
                .map_err(|error| error.to_string())
        };
        let shared = if spec.account == LINEAR_ACCOUNT {
            let found = secrets::tracker(repo, &mut look)?;
            (found.accounts.source == Source::Shared && found.key.is_some())
                .then_some(found.accounts.key)
        } else if spec.account == GITHUB_ACCOUNT {
            let found = secrets::forge(repo, &mut look)?;
            (found.account.source == Source::Shared && found.token.is_some())
                .then_some(found.account.token)
        } else {
            None
        };
        wanted.push(Wanted {
            shared,
            ..Wanted::new(*spec, repo)
        });
    }
    Ok((wanted, unnamed))
}

/// What [`store_secrets`] did with each secret.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SecretsReport {
    /// Given and stored.
    pub stored: Vec<Wanted>,
    /// Already in the keychain, and left there.
    pub kept: Vec<Wanted>,
    /// Not given, the project reading the shared account it names in
    /// [`Wanted::shared`]: nothing written, and that account still read.
    pub kept_shared: Vec<Wanted>,
    /// Neither in the keychain nor given.
    pub missing: Vec<Wanted>,
    /// Given, but not one word: holding a space or a control character, as
    /// a secret pasted wrong does. Not stored; one already there is kept.
    pub refused: Vec<Wanted>,
}

/// Makes sure each of `wanted` is in `keychain`, under its account. One
/// already there is kept, unless `replace`; for any other, `ask` is called
/// and what it gives is stored, trimmed, when it is one word
/// ([`Secret::is_one_word`]), and refused otherwise. `ask` answers `None`
/// when it cannot ask, or was given nothing: the secret is then kept when
/// it exists, kept in its shared account when the project reads one,
/// missing otherwise. A shared account is never written. A secret is never
/// part of the report or of an error.
pub fn store_secrets(
    keychain: &Keychain,
    wanted: &[Wanted],
    replace: bool,
    ask: &mut dyn FnMut(&Wanted) -> Option<Secret>,
) -> Result<SecretsReport, KeychainError> {
    let mut report = SecretsReport::default();
    for one in wanted {
        let present = keychain.contains(&one.account)?;
        if present && !replace {
            report.kept.push(one.clone());
            continue;
        }
        let given = ask(one)
            .map(|secret| Secret::new(secret.expose().trim()))
            .filter(|secret| !secret.expose().is_empty());
        match given {
            Some(secret) if !secret.is_one_word() => report.refused.push(one.clone()),
            Some(secret) => {
                keychain.store(&one.account, &secret)?;
                report.stored.push(one.clone());
            }
            None if present => report.kept.push(one.clone()),
            None if one.shared.is_some() => report.kept_shared.push(one.clone()),
            None => report.missing.push(one.clone()),
        }
    }
    Ok(report)
}

/// What [`store_app_credentials`] did with the Linear app's pair.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppStored {
    /// Both given, and stored.
    Stored,
    /// Both already in the keychain, and left there.
    Kept,
    /// No client ID given: nothing stored, and Owlshift writes through the
    /// API key unless a pair is in the keychain.
    Skipped,
    /// A client ID given without its secret: neither stored.
    Incomplete,
    /// One of the two not one word ([`Secret::is_one_word`]): neither stored.
    Refused,
}

/// Stores the Linear app's client ID and secret under `accounts`, the
/// project's own, together or not at all (OWL-157). A stored pair is kept
/// unless `replace`. Otherwise `ask` gives the client ID, then, when one
/// was given, the secret; both, each one word, are stored, and anything less
/// stores nothing, keeping what the keychain holds. Whether it then holds a
/// usable pair is [`crate::tracker::app_credentials`]'s to say. Neither
/// value is ever part of the answer or of an error.
///
/// The client ID is written first: when writing the secret then fails, the
/// client ID the keychain held before is put back, or the new one removed
/// when there was none, and the error is the write's. Should that fail too,
/// the error says the pair may no longer go together, and how to store it
/// again.
pub fn store_app_credentials(
    keychain: &Keychain,
    accounts: &TrackerAccounts,
    replace: bool,
    ask: &mut dyn FnMut(&Wanted) -> Option<Secret>,
) -> Result<AppStored, String> {
    let contains = |account: &str| keychain.contains(account).map_err(|e| e.to_string());
    let paired = contains(&accounts.app_id)? && contains(&accounts.app_secret)?;
    if paired && !replace {
        return Ok(AppStored::Kept);
    }
    let wanted = |spec: SecretSpec, account: &str| Wanted {
        spec,
        account: account.to_owned(),
        shared: None,
    };
    let mut given = |one: Wanted| {
        ask(&one)
            .map(|secret| Secret::new(secret.expose().trim()))
            .filter(|secret| !secret.expose().is_empty())
    };
    let Some(id) = given(wanted(LINEAR_APP_ID, &accounts.app_id)) else {
        return Ok(if paired {
            AppStored::Kept
        } else {
            AppStored::Skipped
        });
    };
    let Some(secret) = given(wanted(LINEAR_APP_SECRET, &accounts.app_secret)) else {
        return Ok(AppStored::Incomplete);
    };
    if !(id.is_one_word() && secret.is_one_word()) {
        return Ok(AppStored::Refused);
    }
    store_pair(keychain, accounts, &id, &secret)?;
    Ok(AppStored::Stored)
}

/// What [`store_pair`] needs of a keychain, so that a write that fails can
/// be tested. Errors are the keychain's messages, which carry no value.
trait Entries {
    fn read(&self, account: &str) -> Result<Option<Secret>, String>;
    fn store(&self, account: &str, secret: &Secret) -> Result<(), String>;
    fn delete(&self, account: &str) -> Result<(), String>;
}

impl Entries for Keychain {
    fn read(&self, account: &str) -> Result<Option<Secret>, String> {
        Keychain::read(self, account).map_err(|e| e.to_string())
    }

    fn store(&self, account: &str, secret: &Secret) -> Result<(), String> {
        Keychain::store(self, account, secret).map_err(|e| e.to_string())
    }

    fn delete(&self, account: &str) -> Result<(), String> {
        Keychain::delete(self, account)
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

/// Writes the client ID, then the secret, under `accounts`; when the
/// secret's write fails, puts back the client ID found before, or removes
/// the new one.
fn store_pair(
    keychain: &dyn Entries,
    accounts: &TrackerAccounts,
    id: &Secret,
    secret: &Secret,
) -> Result<(), String> {
    let (id_account, secret_account) = (&accounts.app_id, &accounts.app_secret);
    let before = keychain.read(id_account)?;
    keychain.store(id_account, id)?;
    let Err(stored) = keychain.store(secret_account, secret) else {
        return Ok(());
    };
    let restored = match &before {
        Some(before) => keychain.store(id_account, before),
        None => keychain.delete(id_account),
    };
    match restored {
        Ok(()) => Err(stored),
        Err(restored) => Err(format!(
            "storing the Linear app's client secret failed ({stored}), and so did putting back \
             its client ID ({restored}): the keychain may now hold a client ID and a secret that \
             do not go together (service `{SERVICE}`, accounts `{id_account}` and \
             `{secret_account}`). Run `owlshift init --replace-secrets` and give both again"
        )),
    }
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

    fn repo() -> Repo {
        Repo::parse("pitchopp/owlshift").unwrap()
    }

    fn wanted(keychain: &Keychain, specs: &[SecretSpec]) -> Vec<Wanted> {
        wanted_secrets(keychain, specs, Some(&repo())).unwrap().0
    }

    fn specs(wanted: &[Wanted]) -> Vec<SecretSpec> {
        wanted.iter().map(|one| one.spec).collect()
    }

    #[test]
    fn secrets_are_asked_only_when_missing_and_never_reported() {
        const SENTINEL: &str = "lin_api_SENTINEL_never_shown";
        let keychain = Keychain::in_memory();
        let own = TrackerAccounts::project(&repo());
        let token = secrets::ForgeAccount::project(&repo()).token;
        keychain
            .store(&token, &Secret::new("ghp_SENTINEL_never_shown"))
            .unwrap();
        let mut asked = Vec::new();
        let all = wanted(&keychain, &required_secrets(TrackerKind::Linear));
        let report = store_secrets(&keychain, &all, false, &mut |one| {
            asked.push(one.account.clone());
            Some(Secret::new(format!("  {SENTINEL}\n")))
        })
        .unwrap();
        // The project's secrets under its own accounts, the Claude Code
        // token under the machine's.
        assert_eq!(asked, [own.key.as_str(), CLAUDE_AGENT_ACCOUNT]);
        assert_eq!(specs(&report.stored), [LINEAR_KEY, CLAUDE_TOKEN]);
        assert_eq!(specs(&report.kept), [GITHUB_TOKEN]);
        assert!(report.missing.is_empty());
        // Stored trimmed, and absent from everything a person is shown.
        assert_eq!(keychain.read(&own.key).unwrap().unwrap().expose(), SENTINEL);
        assert_eq!(keychain.read(LINEAR_ACCOUNT).unwrap(), None);
        assert_eq!(
            keychain
                .read(CLAUDE_AGENT_ACCOUNT)
                .unwrap()
                .unwrap()
                .expose(),
            SENTINEL
        );
        let shown = format!(
            "{report:?} {}",
            all.iter().map(ToString::to_string).collect::<String>()
        );
        assert!(!shown.contains("SENTINEL"), "{shown}");
        assert!(
            shown.contains(
                "Linear API key (keychain service `owlshift`, account \
                 `github.com/pitchopp/owlshift/linear`)"
            ),
            "{shown}"
        );
        assert!(CLAUDE_TOKEN.help.contains("`claude setup-token`"));
        assert_eq!(
            required_secrets(TrackerKind::Markdown),
            [GITHUB_TOKEN, CLAUDE_TOKEN]
        );

        // Replacing asks again; an empty answer keeps what is there, and a
        // secret nobody gives is missing.
        let key = wanted(&keychain, &[LINEAR_KEY]);
        let report = store_secrets(&keychain, &key, true, &mut |_| Some(Secret::new(""))).unwrap();
        assert_eq!(specs(&report.kept), [LINEAR_KEY]);
        keychain.delete(&token).unwrap();
        let github = wanted(&keychain, &[GITHUB_TOKEN]);
        let report = store_secrets(&keychain, &github, false, &mut |_| None).unwrap();
        assert_eq!(specs(&report.missing), [GITHUB_TOKEN]);

        // A secret pasted across two lines is refused, and the one stored
        // is kept.
        let claude = wanted(&keychain, &[CLAUDE_TOKEN]);
        let report = store_secrets(&keychain, &claude, true, &mut |_| {
            Some(Secret::new("sk-ant-oat01-SENTINEL\nrest"))
        })
        .unwrap();
        assert_eq!(specs(&report.refused), [CLAUDE_TOKEN]);
        assert!(report.stored.is_empty());
        assert_eq!(
            keychain
                .read(CLAUDE_AGENT_ACCOUNT)
                .unwrap()
                .unwrap()
                .expose(),
            SENTINEL
        );
    }

    /// A project reading a shared secret is asked for its own; an empty
    /// answer keeps reading the shared one, and a shared account is never
    /// written, even when replacing.
    #[test]
    fn a_shared_secret_is_kept_in_use_and_never_written() {
        let keychain = Keychain::in_memory();
        keychain
            .store(LINEAR_ACCOUNT, &Secret::new("shared-key"))
            .unwrap();
        keychain
            .store(GITHUB_ACCOUNT, &Secret::new("shared-token"))
            .unwrap();
        let both = wanted(&keychain, &[LINEAR_KEY, GITHUB_TOKEN]);
        assert_eq!(both[0].shared.as_deref(), Some(LINEAR_ACCOUNT));
        assert_eq!(both[1].shared.as_deref(), Some(GITHUB_ACCOUNT));
        for replace in [false, true] {
            let mut asked = 0;
            let report = store_secrets(&keychain, &both, replace, &mut |_| {
                asked += 1;
                None
            })
            .unwrap();
            assert_eq!(asked, 2);
            assert_eq!(specs(&report.kept_shared), [LINEAR_KEY, GITHUB_TOKEN]);
            assert!(report.missing.is_empty() && report.stored.is_empty());
        }
        let report = store_secrets(&keychain, &both[..1], true, &mut |_| {
            Some(Secret::new("own-key"))
        })
        .unwrap();
        assert_eq!(specs(&report.stored), [LINEAR_KEY]);
        let own = TrackerAccounts::project(&repo());
        assert_eq!(
            keychain.read(&own.key).unwrap().unwrap().expose(),
            "own-key"
        );
        assert_eq!(
            keychain.read(LINEAR_ACCOUNT).unwrap().unwrap().expose(),
            "shared-key"
        );
        // With a key of its own, the project reads no shared one.
        assert_eq!(wanted(&keychain, &[LINEAR_KEY])[0].shared, None);
    }

    /// Without a GitHub repository, only the machine's secret is wanted:
    /// the project's cannot be named.
    #[test]
    fn without_a_repository_only_the_machines_secret_is_wanted() {
        let keychain = Keychain::in_memory();
        let (wanted, unnamed) =
            wanted_secrets(&keychain, &required_secrets(TrackerKind::Linear), None).unwrap();
        assert_eq!(specs(&wanted), [CLAUDE_TOKEN]);
        assert_eq!(wanted[0].account, CLAUDE_AGENT_ACCOUNT);
        assert_eq!(unnamed, [LINEAR_KEY, GITHUB_TOKEN]);
    }

    /// The Linear app's pair is stored whole or not at all, under the
    /// project's accounts: a client ID alone, or one pasted across lines,
    /// stores nothing; no answer skips the app; a stored pair is kept
    /// unless replaced.
    #[test]
    fn the_linear_apps_pair_is_stored_whole_or_not_at_all() {
        let keychain = Keychain::in_memory();
        let own = TrackerAccounts::project(&repo());
        let store = |replace, answers: [Option<&str>; 2]| {
            let mut answers = answers.into_iter();
            let mut asked = Vec::new();
            let stored = store_app_credentials(&keychain, &own, replace, &mut |one| {
                asked.push(one.account.clone());
                answers.next().flatten().map(Secret::new)
            })
            .unwrap();
            (stored, asked)
        };
        let is_stored = |account: &str| keychain.contains(account).unwrap();

        assert_eq!(
            store(false, [None, None]),
            (AppStored::Skipped, vec![own.app_id.clone()])
        );
        assert_eq!(store(false, [Some("id"), None]).0, AppStored::Incomplete);
        assert_eq!(
            store(false, [Some("id"), Some("two\nlines")]).0,
            AppStored::Refused
        );
        assert!(!is_stored(&own.app_id));
        assert!(!is_stored(&own.app_secret));

        assert_eq!(
            store(false, [Some(" id "), Some("secret")]),
            (
                AppStored::Stored,
                vec![own.app_id.clone(), own.app_secret.clone()]
            )
        );
        assert_eq!(keychain.read(&own.app_id).unwrap().unwrap().expose(), "id");
        assert!(!is_stored(LINEAR_APP_ID_ACCOUNT));
        assert_eq!(
            store(false, [Some("x"), Some("y")]),
            (AppStored::Kept, vec![])
        );
        assert_eq!(store(true, [None, None]).0, AppStored::Kept);
        assert_eq!(store(true, [Some("id"), None]).0, AppStored::Incomplete);
        assert_eq!(
            keychain.read(&own.app_secret).unwrap().unwrap().expose(),
            "secret"
        );
    }

    /// The in-memory keychain, whose writes of the secret fail, and, when
    /// `rollback_fails`, whatever puts the client ID back.
    struct SecretFails {
        inner: Keychain,
        rollback_fails: bool,
        id_writes: std::cell::Cell<u32>,
    }

    impl Entries for SecretFails {
        fn read(&self, account: &str) -> Result<Option<Secret>, String> {
            Entries::read(&self.inner, account)
        }

        fn store(&self, account: &str, secret: &Secret) -> Result<(), String> {
            if account == TrackerAccounts::project(&repo()).app_secret {
                return Err("keychain: the secret's write failed".to_owned());
            }
            self.id_writes.set(self.id_writes.get() + 1);
            if self.rollback_fails && self.id_writes.get() > 1 {
                return Err("keychain: the rollback failed".to_owned());
            }
            Entries::store(&self.inner, account, secret)
        }

        fn delete(&self, account: &str) -> Result<(), String> {
            if self.rollback_fails {
                return Err("keychain: the rollback failed".to_owned());
            }
            Entries::delete(&self.inner, account)
        }
    }

    /// A secret whose write fails leaves the pair as it was: the client ID
    /// found before put back, or the new one removed. A rollback that fails
    /// too says the pair may no longer go together, and how to fix it.
    #[test]
    fn a_failed_secret_write_leaves_the_pair_as_it_was() {
        let keychain = |rollback_fails| SecretFails {
            inner: Keychain::in_memory(),
            rollback_fails,
            id_writes: std::cell::Cell::new(0),
        };
        let (id, secret) = (Secret::new("new-id"), Secret::new("new-secret"));
        let own = TrackerAccounts::project(&repo());
        let id_of = |k: &SecretFails| {
            Entries::read(&k.inner, &own.app_id)
                .unwrap()
                .map(|s| s.expose().to_owned())
        };

        let none = keychain(false);
        let error = store_pair(&none, &own, &id, &secret).unwrap_err();
        assert_eq!(error, "keychain: the secret's write failed");
        assert_eq!(id_of(&none), None);

        let pair = keychain(false);
        pair.inner
            .store(&own.app_id, &Secret::new("old-id"))
            .unwrap();
        pair.inner
            .store(&own.app_secret, &Secret::new("old-secret"))
            .unwrap();
        let error = store_pair(&pair, &own, &id, &secret).unwrap_err();
        assert_eq!(error, "keychain: the secret's write failed");
        assert_eq!(id_of(&pair).as_deref(), Some("old-id"));

        for before in [None, Some("old-id")] {
            let stuck = keychain(true);
            if let Some(before) = before {
                stuck
                    .inner
                    .store(&own.app_id, &Secret::new(before))
                    .unwrap();
            }
            let error = store_pair(&stuck, &own, &id, &secret).unwrap_err();
            assert!(
                error.contains("the secret's write failed")
                    && error.contains("the rollback failed")
                    && error.contains("do not go together")
                    && error.ends_with("Run `owlshift init --replace-secrets` and give both again"),
                "{error}"
            );
            assert!(
                !error.contains("new-id") && !error.contains("old-id"),
                "{error}"
            );
        }
    }
}
