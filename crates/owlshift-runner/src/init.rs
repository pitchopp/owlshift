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
use owlshift_core::stack::{Detection, MANIFESTS, RootFiles};
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
    /// What [`owlshift_core::stack::detect`] read from the root: its gate
    /// is written unless `gate` is given, its setup always.
    pub detected: Detection,
    /// The account `init` signed in to the tracker as, written as the
    /// admitter where the tracker names one (GitHub Issues, OWL-537); `None`
    /// when the tracker was not asked.
    pub admitter: Option<String>,
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
    text.push_str(
        &crate::tracker::of(options.tracker).project_file_lines(options.team.as_deref())?,
    );
    if options.tracker == TrackerKind::Markdown {
        text.push_str(&format!(
            "\
# The Markdown tracker serves one machine: run `owlshift do TICKET` and
# `owlshift continue TICKET` in a checkout. Owlshift commits none of its
# files, and the daemon does not serve it, so `owlshift start` refuses such
# a project. Its format: {MARKDOWN_PAGE}
"
        ));
    }
    // A gate given by hand wins over the detected one, and only over it.
    let by_hand = !options.gate.is_empty();
    let mut gate = String::new();
    for found in &options.detected.found {
        if (!by_hand && !found.gate.is_empty()) || !found.setup.is_empty() {
            for note in &found.notes {
                gate.push_str(&format!("# note: {}\n", one_line(note)));
            }
        }
    }
    let gate_lines: Vec<(Option<&str>, &str)> = if by_hand {
        options.gate.iter().map(|c| (None, c.as_str())).collect()
    } else {
        options
            .detected
            .gate()
            .map(|l| (Some(l.read_from.as_str()), l.command.as_str()))
            .collect()
    };
    gate.push_str(&commands_block("gate", &gate_lines, &quote));
    let setup_lines: Vec<(Option<&str>, &str)> = options
        .detected
        .setup()
        .map(|l| (Some(l.read_from.as_str()), l.command.as_str()))
        .collect();
    let setup = if setup_lines.is_empty() {
        "# setup = [\"pnpm install --frozen-lockfile\"]\n".to_owned()
    } else {
        commands_block("setup", &setup_lines, &quote)
    };
    let admission =
        crate::tracker::of(options.tracker).admission_lines(options.admitter.as_deref());
    text.push_str(&format!(
        "\
{admission}
[stack]
# The project's full gate: lint, formatter and tests, run in order from the
# repository root. Owlshift runs it on the commit it delivers; the build
# role first runs the commands that cover its change. With no command,
# `owlshift do` refuses to start a ticket.
{gate}# Agents start from an almost empty environment. Name here the variables of
# your environment the gate needs, such as a feature flag; the whole agent
# run sees them. Credential variables are refused, and each machine that
# runs Owlshift passes only the names its operator lists in `allow_gate_env`
# in the personal configuration. A path under your home stays unreadable to
# the sandboxed run unless it is on the PATH.
# gate_env = [\"FEATURE_FLAGS\"]
# The commands that install the gate's dependencies, run in order with the
# network open before a Build's agent starts and again before the gate. The
# run's temporary folder is kept between them; the worktree may change only
# in ignored files, so install with a frozen or locked lockfile.
{setup}# Folders of your home, such as a package store, that the setup, the agent
# and the gate read, never write. Each machine that runs Owlshift opens only
# the folders its operator lists in `allow_caches` in the personal
# configuration; a folder holding credentials is refused.
# caches = [\"Library/pnpm\"]
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
# default model, but `fast` falls back to `standard`. A ticket's Build runs
# on the tier of its routing verdict, or of its `tier:` label. For example:
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

/// Writes the project file of `options` at `root` once the tracker
/// answered (OWL-537; build plan "P9 · Open-source readiness", part 6):
/// `connected` is what [`crate::tracker::Kind::connect`] gave, its account
/// written as the admitter. When the tracker did not answer, nothing is
/// written and the error says why.
pub fn write_once_connected(
    root: &Path,
    options: &InitOptions,
    connected: Result<&crate::tracker::Connected, &str>,
) -> Result<Written, String> {
    let connected = connected.map_err(|reason| {
        format!(
            "the tracker did not answer: {reason}. {PROJECT_FILE} was not written: fix that, then \
             run `owlshift init` again"
        )
    })?;
    let options = InitOptions {
        admitter: connected.admitter.clone(),
        ..options.clone()
    };
    let text = project_file(&options)?;
    write_project_file(root, &text).map_err(|error| format!("{PROJECT_FILE}: {error}"))
}

/// A `key = [...]` block, each command preceded by the comment saying what it
/// was read from; an empty list is `key = []`.
fn commands_block(
    key: &str,
    lines: &[(Option<&str>, &str)],
    quote: &dyn Fn(&str) -> String,
) -> String {
    if lines.is_empty() {
        return format!("{key} = []\n");
    }
    let mut out = format!("{key} = [\n");
    for (from, command) in lines {
        if let Some(from) = from {
            out.push_str(&format!("    # read from {}\n", one_line(from)));
        }
        out.push_str(&format!("    {},\n", quote(command)));
    }
    out.push_str("]\n");
    out
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The most a manifest read for stack detection may weigh.
const MANIFEST_LIMIT: u64 = 1 << 20;

/// Reads the root of a repository for [`owlshift_core::stack::detect`]: the
/// names of its files, and the contents of the manifests it recognises. A
/// manifest over 1 MiB or not UTF-8 is left unread, which detection reports as
/// skipped; only a root that cannot be listed is an error.
pub fn read_root_files(root: &Path) -> io::Result<RootFiles> {
    let mut names = Vec::new();
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        if let Some(name) = entry.file_name().to_str() {
            names.push(name.to_owned());
        }
    }
    names.sort();
    let mut files = Vec::new();
    for name in MANIFESTS {
        if !names.iter().any(|n| n == name) {
            continue;
        }
        let path = root.join(name);
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        if !meta.is_file() || meta.len() > MANIFEST_LIMIT {
            continue;
        }
        if let Ok(text) = std::fs::read_to_string(&path) {
            files.push((name.to_owned(), text));
        }
    }
    Ok(RootFiles { names, files })
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

/// Where the Markdown tracker's format and limits are published.
pub const MARKDOWN_PAGE: &str =
    "https://github.com/pitchopp/owlshift/blob/main/docs/markdown-tracker.md";

/// The Markdown tracker's limits in one line, which `init` prints.
pub const MARKDOWN_LIMITS: &str = "The Markdown tracker serves one machine: run `owlshift do` and `owlshift continue` in a \
     checkout. Owlshift commits none of its files, and the daemon does not serve it, so \
     `owlshift start` refuses such a project.";

/// The folder of the first ticket `init` writes for a Markdown project.
pub const EXAMPLE_TICKET: &str = "DEMO-1";

/// The placeholder decider of [`example_ticket`], for the user to replace.
pub const EXAMPLE_ASSIGNEE: &str = "maintainer";

/// The first ticket of a Markdown project: its folder under `tickets/` and
/// its `ticket.md`. Ready for `owlshift do` once its assignee, the ticket's
/// decider, is the user's own name.
pub fn example_ticket() -> (&'static str, String) {
    let text = format!(
        "\
+++
title = \"Try Owlshift on a first ticket\"
author = \"{EXAMPLE_ASSIGNEE}\"
stage = \"Todo\"
assignee = \"{EXAMPLE_ASSIGNEE}\"
labels = [\"agent\"]
+++

This ticket is an example that `owlshift init` wrote.

1. Replace `{EXAMPLE_ASSIGNEE}` in the `assignee` line above, and in `author`, with your own
   name (ASCII letters, digits, `_` or `-`). The assignee is the ticket's decider: the person
   Owlshift asks when it needs a decision.
2. Replace this text with what you want built.
3. Run `owlshift do {EXAMPLE_TICKET}` in this checkout.

{MARKDOWN_LIMITS}
The format is described at {MARKDOWN_PAGE}
"
    );
    (EXAMPLE_TICKET, text)
}

/// Writes [`example_ticket`] under `root`'s `tickets/`, unless that folder
/// already holds a ticket (or anything): then nothing is written.
pub fn write_example_ticket(root: &Path) -> io::Result<Written> {
    let tickets = root.join("tickets");
    match std::fs::read_dir(&tickets) {
        Ok(mut entries) => {
            if entries.next().is_some() {
                return Ok(Written::Kept);
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => std::fs::create_dir_all(&tickets)?,
        Err(error) => return Err(error),
    }
    let (folder, text) = example_ticket();
    let folder = tickets.join(folder);
    match std::fs::create_dir(&folder) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => return Ok(Written::Kept),
        Err(error) => return Err(error),
    }
    std::fs::write(folder.join("ticket.md"), text)?;
    Ok(Written::Created)
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

/// The personal GitHub token: since OWL-336 the expert option, asked for
/// only with `--github-token`; otherwise `init` signs in through the GitHub
/// App ([`crate::github_sign_in`]).
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
    let mut specs = crate::tracker::of(tracker).secret_specs().to_vec();
    specs.extend([GITHUB_TOKEN, CLAUDE_TOKEN]);
    specs
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
            detected: Detection::default(),
            admitter: None,
        }
    }

    /// OWL-537: a GitHub Issues project names its stages as labels, its
    /// admitter by numeric id when `init` signed in, a placeholder when it
    /// did not; its priorities are commented out and valid once uncommented.
    /// Linear's lines are the ones written before.
    #[test]
    fn a_github_project_names_its_labels_and_its_admitter() {
        let github = |admitter: Option<&str>| {
            project_file(&InitOptions {
                admitter: admitter.map(ToOwned::to_owned),
                ..options(TrackerKind::Github, None, &[])
            })
            .unwrap()
        };
        let signed_in = github(Some("583231"));
        let config = ProjectConfig::parse(&signed_in).unwrap();
        assert_eq!(config.tracker.kind, TrackerKind::Github);
        assert_eq!(
            config.tracker.admitters.as_deref(),
            Some(&["583231".to_owned()][..])
        );
        assert_eq!(config.tracker.states.ready, "agent:ready");
        assert_eq!(config.tracker.states.needs_input, "agent:needs-input");
        assert_eq!(config.tracker.priorities, None);
        let prioritised = signed_in.replace("# [tracker.priorities]", "[tracker.priorities]");
        let prioritised = ["urgent", "high", "medium", "low"]
            .iter()
            .fold(prioritised, |text, p| {
                text.replace(&format!("# {p} = "), &format!("{p} = "))
            });
        let priorities = ProjectConfig::parse(&prioritised)
            .unwrap()
            .tracker
            .priorities;
        assert_eq!(
            priorities.unwrap().urgent.as_deref(),
            Some("priority:urgent")
        );

        let unchecked = github(None);
        assert_eq!(
            ProjectConfig::parse(&unchecked).unwrap().tracker.admitters,
            None
        );
        assert!(unchecked.contains("# admitters = ["), "{unchecked}");

        let linear = project_file(&options(TrackerKind::Linear, Some("OWL"), &[])).unwrap();
        assert!(
            linear.contains(&format!("{}\n[stack]\n", crate::tracker::ADMISSION_LINES)),
            "{linear}"
        );
    }

    /// OWL-537: the project file is written only once the tracker answered,
    /// with the account it gave as the admitter.
    #[test]
    fn the_project_file_is_written_only_once_the_tracker_answered() {
        let root = tempfile::tempdir().unwrap();
        let options = options(TrackerKind::Github, None, &[]);
        let error =
            write_once_connected(root.path(), &options, Err("HTTP 403 refused")).unwrap_err();
        assert!(
            error.contains("HTTP 403 refused") && error.contains("was not written"),
            "{error}"
        );
        assert!(!root.path().join(PROJECT_FILE).exists());

        let connected = crate::tracker::Connected {
            admitter: Some("7".into()),
            created: Vec::new(),
        };
        assert_eq!(
            write_once_connected(root.path(), &options, Ok(&connected)),
            Ok(Written::Created)
        );
        let text = std::fs::read_to_string(root.path().join(PROJECT_FILE)).unwrap();
        assert!(text.contains("admitters = [\"7\"]"), "{text}");
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

        // OWL-330: the setup and cache examples are valid once uncommented.
        let uncommented = linear
            .replace("# setup = ", "setup = ")
            .replace("# caches = ", "caches = ");
        let config = ProjectConfig::parse(&uncommented).unwrap();
        assert_eq!(config.stack.setup, ["pnpm install --frozen-lockfile"]);
        assert_eq!(config.stack.caches, ["Library/pnpm"]);

        let markdown = project_file(&options(TrackerKind::Markdown, None, &[])).unwrap();
        let config = ProjectConfig::parse(&markdown).unwrap();
        assert_eq!(config.tracker.kind, TrackerKind::Markdown);
        assert!(config.stack.gate.is_empty());

        let error = project_file(&options(TrackerKind::Linear, Some(" "), &[])).unwrap_err();
        assert!(error.contains("--team"), "{error}");
    }

    fn detected(names: &[&str], files: &[(&str, &str)]) -> Detection {
        owlshift_core::stack::detect(&RootFiles {
            names: names
                .iter()
                .chain(files.iter().map(|(n, _)| n))
                .map(|n| (*n).to_owned())
                .collect(),
            files: files
                .iter()
                .map(|(n, c)| ((*n).to_owned(), (*c).to_owned()))
                .collect(),
        })
    }

    fn with(detected: Detection, gate: &[&str]) -> InitOptions {
        InitOptions {
            detected,
            ..options(TrackerKind::Markdown, None, gate)
        }
    }

    #[test]
    fn nothing_detected_writes_the_template_as_before() {
        let text = project_file(&with(Detection::default(), &[])).unwrap();
        assert!(
            text.contains(
                "\n# `owlshift do` refuses to start a ticket.\ngate = []\n# Agents start"
            )
        );
        assert!(text.contains("# network open before"));
        assert!(
            text.contains(
                "\n# setup = [\"pnpm install --frozen-lockfile\"]\n# Folders of your home"
            )
        );
        assert!(!text.contains("read from") && !text.contains("# note:"));
    }

    #[test]
    fn a_detected_cargo_and_npm_root_is_written_with_its_sources_and_loads() {
        let d = detected(
            &["Cargo.lock", "package-lock.json"],
            &[
                ("Cargo.toml", "[package]\nname = \"x\"\n"),
                ("package.json", r#"{"scripts":{"test":"jest"}}"#),
            ],
        );
        let text = project_file(&with(d, &[])).unwrap();
        let config = ProjectConfig::parse(&text).unwrap();
        assert_eq!(
            config.stack.gate,
            [
                "cargo fmt --all -- --check",
                "cargo clippy --workspace --all-targets --locked -- -D warnings",
                "cargo test --workspace --locked",
                "npm run test"
            ]
        );
        assert_eq!(config.stack.setup, ["npm ci"]);
        assert_eq!(text.matches("    # read from ").count(), 5, "{text}");
        assert!(
            text.contains(
                "    # read from the `test` script of package.json\n    \"npm run test\","
            )
        );
        assert!(
            text.contains("# read from package-lock.json, the lock file of npm\n    \"npm ci\",")
        );
    }

    #[test]
    fn a_python_root_carries_its_note_and_still_loads() {
        let d = detected(&[], &[("pyproject.toml", "[tool.ruff]\n")]);
        let text = project_file(&with(d, &[])).unwrap();
        assert!(
            text.contains("# note: pyproject.toml's tools are run bare"),
            "{text}"
        );
        let config = ProjectConfig::parse(&text).unwrap();
        assert_eq!(config.stack.gate, ["ruff check ."]);
        assert!(config.stack.setup.is_empty());
    }

    #[test]
    fn a_gate_given_by_hand_wins_and_the_detected_setup_stays() {
        let d = detected(
            &["pnpm-lock.yaml"],
            &[("package.json", r#"{"scripts":{"test":"vitest"}}"#)],
        );
        let text = project_file(&with(d, &["make ci"])).unwrap();
        let config = ProjectConfig::parse(&text).unwrap();
        assert_eq!(config.stack.gate, ["make ci"]);
        assert_eq!(config.stack.setup, ["pnpm install --frozen-lockfile"]);
        assert!(!text.contains("pnpm run test"));
        assert!(text.contains("# read from pnpm-lock.yaml, the lock file of pnpm\n"));
    }

    #[test]
    fn the_markdown_project_file_names_the_limits_and_the_page() {
        let text = project_file(&options(TrackerKind::Markdown, None, &[])).unwrap();
        assert!(text.contains("serves one machine"), "{text}");
        assert!(text.contains("`owlshift do TICKET` and\n# `owlshift continue TICKET`"));
        assert!(text.contains("commits none of its\n# files"), "{text}");
        assert!(text.contains("`owlshift start` refuses"), "{text}");
        assert!(text.contains(MARKDOWN_PAGE), "{text}");
        let linear = project_file(&options(TrackerKind::Linear, Some("OWL"), &[])).unwrap();
        assert!(!linear.contains("serves one machine"));
    }

    #[test]
    fn the_example_ticket_loads_and_is_written_once() {
        use owlshift_adapters::tracker::markdown::MarkdownTracker;
        use owlshift_contracts::ids::TicketId;

        let (folder, text) = example_ticket();
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(write_example_ticket(dir.path()).unwrap(), Written::Created);
        let path = dir.path().join("tickets").join(folder).join("ticket.md");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
        let ticket = MarkdownTracker::new(dir.path())
            .ticket(&TicketId::new(folder).unwrap())
            .unwrap();
        assert_eq!(ticket.stage, "Todo");
        assert_eq!(ticket.labels, ["agent"]);
        assert_eq!(ticket.assignee.as_deref(), Some(EXAMPLE_ASSIGNEE));
        assert!(ticket.description.contains("owlshift do DEMO-1"));
        assert!(ticket.description.contains(MARKDOWN_LIMITS));

        // An edit survives a second write, which keeps the ticket.
        std::fs::write(&path, "edited").unwrap();
        assert_eq!(write_example_ticket(dir.path()).unwrap(), Written::Kept);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "edited");
    }

    #[test]
    fn the_example_ticket_is_not_written_beside_other_tickets() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("tickets/OWL-1")).unwrap();
        assert_eq!(write_example_ticket(dir.path()).unwrap(), Written::Kept);
        assert!(!dir.path().join("tickets").join(EXAMPLE_TICKET).exists());
    }

    #[test]
    fn the_root_is_read_for_its_names_and_its_manifests() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\n").unwrap();
        std::fs::write(dir.path().join("package.json"), vec![b'x'; (1 << 20) + 1]).unwrap();
        std::fs::write(dir.path().join("pyproject.toml"), [0xff, 0xfe]).unwrap();
        std::fs::write(dir.path().join("pnpm-lock.yaml"), "lock").unwrap();
        std::fs::create_dir(dir.path().join("Makefile")).unwrap();
        let root = read_root_files(dir.path()).unwrap();
        assert_eq!(
            root.names,
            [
                "Cargo.toml",
                "Makefile",
                "package.json",
                "pnpm-lock.yaml",
                "pyproject.toml"
            ]
        );
        assert_eq!(
            root.files,
            [("Cargo.toml".to_owned(), "[package]\n".to_owned())]
        );
        let d = owlshift_core::stack::detect(&root);
        assert_eq!(d.skipped.len(), 3, "{:?}", d.skipped);
        assert_eq!(d.found.len(), 1);
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
        // OWL-382: nor with a GitHub App token of its own, which it uses
        // before any personal token.
        keychain
            .store(
                &secrets::ForgeAccount::project_app(&repo()).token,
                &Secret::new("{\"v\":1,\"access_token\":\"ghu_x\"}"),
            )
            .unwrap();
        assert_eq!(wanted(&keychain, &[GITHUB_TOKEN])[0].shared, None);
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
