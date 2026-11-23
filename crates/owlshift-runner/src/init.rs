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

use std::ffi::OsString;
use std::fmt;
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use semver::Version;

use owlshift_adapters::forge::Repo;
use owlshift_contracts::config::{ProjectConfig, TrackerKind};
use owlshift_core::stack::{Detection, HomeManager, MANIFESTS, RootFiles};
use owlshift_platform::keychain::{Keychain, KeychainError, SERVICE, Secret};

use crate::config::{OWLSHIFT_VERSION, PROJECT_FILE};
use crate::executor::harness::CLAUDE_AGENT_ACCOUNT;
use crate::forge::{GITHUB_ACCOUNT, GITHUB_TOKEN_HELP};
use crate::secrets::{self, SecretSet, Source, TrackerAccounts};
use crate::tracker::{LINEAR_ACCOUNT, LINEAR_APP_ID_ACCOUNT, LINEAR_APP_SECRET_ACCOUNT, SignedIn};

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
#
# Every key that applies to this tracker is listed, with what it decides, its
# values and its default. A key commented out with its default value changes
# nothing once uncommented; one with an example value is unset until set.

# The Owlshift versions that can read this file: a semantic version
# requirement. Required.
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
        // A source's notes are written above what it gives the file, and
        // above nothing when it gives no line, since they say what to add
        // (OWL-624); not above a gate `--gate` replaced, the only line given.
        let kept = !found.setup.is_empty() || !found.run_caches.is_empty();
        let none = !kept && found.gate.is_empty();
        if (!by_hand && !found.gate.is_empty()) || kept || none {
            let own = (!by_hand && !found.gate.is_empty()).then_some(&found.gate_notes);
            for note in found.notes.iter().chain(own.into_iter().flatten()) {
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
    // The caches the detected managers write (OWL-617), a dotted key each
    // under its source, or the example.
    let mut run_caches = String::new();
    for cache in options.detected.run_caches() {
        run_caches.push_str(&format!(
            "# read from {}\nrun_caches.{} = {}\n",
            one_line(&cache.read_from),
            cache.variable,
            quote(&cache.folder)
        ));
    }
    if run_caches.is_empty() {
        run_caches.push_str("# run_caches = { CARGO_HOME = \"cargo\" }\n");
    }
    let admission =
        crate::tracker::of(options.tracker).admission_lines(options.admitter.as_deref());
    text.push_str(&format!(
        "\
{admission}
[stack]
# The project's full gate, run in order from the repository root on the
# commit Owlshift delivers: lint, formatter and tests. Required: with no
# command, `owlshift do` refuses to start a ticket.
{gate}# Variables of your environment the gate needs, such as a feature flag, seen
# by the whole agent run; credential variables are refused, and a machine
# passes only those its personal `allow_gate_env` lists. Default: none.
# gate_env = [\"FEATURE_FLAGS\"]
# Commands that install the gate's dependencies, run in order with the
# network open before a Build's agent and before the gate; they may change
# only ignored files, so install from a frozen lockfile. Default: none.
{setup}# Folders of your home, such as pyenv's Pythons, that the setup, the agent
# and the gate read, never write; a machine opens only those its personal
# `allow_caches` lists, never one holding credentials. Default: none.
# caches = [\".pyenv\"]
# Caches of Owlshift's own that the setup, the agent and the gate write, so
# an install fetches what is missing and keeps it for the next runs: the
# variable a package manager reads for its cache, and a folder's name. Each
# machine makes the folder in its cache directory, one tree per project, and
# deleting the tree empties it. Credential, `GIT_` and loader variables are
# refused. Default: none.
{run_caches}# The files agents receive as the project's rules, taken whole from the
# default branch, at most 32 and 64 KiB together, each required to exist.
# Left out, the root AGENTS.md when there is one; [] sends no rule.
# rules = [\"AGENTS.md\", \".claude/rules/testing.md\"]
# Named resources a ticket can hold, each with the paths it covers (`*` one
# path part, `**` any number); two tickets holding one never run together.
# Default: none.
# [stack.resources]
# migrations = \"backend/*/migrations/**\"

[pipeline]
# A ticket's pipeline variant, unless intake or a label picks another:
# \"trivial\" (build and verify), \"standard\" (design, design review, build
# and verify) or \"risky\" (standard, a person approving the plan). Required.
default = \"standard\"
# When a person approves the plan before Build: \"always\", \"on-fork\" (when
# the plan leaves a choice open) or \"never\". Required.
plan_approval = \"on-fork\"
# Who confirms a review finding before it is acted on: \"author\" (the next
# design or fix run answers it) or \"arbiter\" (one arbiter run per review
# pass). Default: `\"author\"`.
# findings = \"author\"
# Whether every design review and verify pass adds a Codex reviewer beside
# the Claude Code ones, a Codex that cannot serve dropping its seat: true or
# false. Default: `false`.
# codex_reviewer = false

[models]
# The model of each tier, per harness (`claude`, `codex`). Left out, a harness
# runs its own default model, but Claude's `fast` falls back to `standard`.
# A ticket's Build runs on its routing verdict's tier, or its `tier:` label's.
# deep = {{ claude = \"claude-opus-5-5\", codex = \"<a Codex model>\" }}
# standard = {{ claude = \"claude-sonnet-5\", codex = \"<a Codex model>\" }}
# fast = {{ claude = \"claude-haiku-4-5\", codex = \"<a Codex model>\" }}

[policy]
# Question categories always left to a person, on top of those the delegation
# level keeps and the floor's, `security` and `irreversible`, which reach a
# person at every level. Required.
always_human = []
# How long the decider's latest comment must stay unedited before their
# answer counts: whole minutes, 1 to 1440; ending it with `go` counts it at
# once. Default: `10`.
# quiet_window_minutes = 10
# How many of the project's tickets may be worked at once, between Ready and
# Watch: 1 to 10; each machine's personal `concurrent_runs` caps it too.
# Default: `1`.
# max_in_flight = 1
# How many pull requests Owlshift delivered may be open at once before the
# drain starts no new Build: 1 to 20. Default: `3`.
# max_open_prs = 3
# Whether two Owlshift tickets may build on the same file at once:
# \"exclusive\" (the later one waits) or \"rebase\" (both build, the later one
# refreshed onto the first's merge). Default: `\"exclusive\"`.
# overlap = \"exclusive\"
# Whether a default branch that breaks the gate files one repair ticket,
# which Owlshift admits itself and runs before the others: true or false.
# Default: `true`.
# base_repair = true
# Where a ticket is claimed, so that one runner alone works it: \"local\" (the
# ticket locks of each machine alone; nothing is pushed) or \"remote\" (a claim
# ref on origin, for a team's runners or one person's two machines; the
# default branch's file decides it). Under \"remote\", a command whose operator cannot be
# read, the tracker key's account or, on the Markdown tracker,
# `identity.tracker`, is refused: a claim names its operator. A person
# releases a claim with `owlshift release`; set back to \"local\", the claims
# on origin expire, a person's to release. Default: `\"local\"`.
# claims = \"local\"
# What the resolver decides without the decider: \"none\", \"facts\",
# \"discoverable\", \"ticket\" (also scope questions the ticket's text answers)
# or \"all\" (everything but the floor). Default: `\"discoverable\"`.
# resolver_delegation = \"discoverable\"
# The runs in a row the resolver may unblock in one command: 1 to 50, or
# \"unlimited\", which needs resolver_budget_usd. Default: `5`.
# resolver_passes = 5
# A ceiling, in dollars, on what the resolver runs of one chain cost; past
# it, the next questions go to the decider. A positive number. Default:
# none, no ceiling; required with resolver_passes = \"unlimited\".
# resolver_budget_usd = 20.0
# What one ticket's runs may cost in all, in dollars, every run counted
# across its commands; reached, the ticket parks until a person restarts it.
# A positive number. Default: none, no budget.
# ticket_budget_usd = 100.0

# The forge. Merge on green: the runner merges a pull request it follows,
# once green, into the default branch; read at that branch's head, never
# from a ticket's branch or the personal file.
# [forge]
# Which pull requests the runner merges once green: \"never\", \"labeled\"
# (a ticket carrying the label `owlshift:merge`) or \"all\". Default: `\"never\"`.
# merge = \"never\"
# How the runner merges: \"squash\", \"merge\" or \"rebase\". Default: `\"squash\"`.
# merge_method = \"squash\"
# The forge logins whose reviews on Owlshift's pull requests are read, each
# mapped to its tracker account: the decider's sends the ticket back to
# Build. Default: none, no review is read. Refused beside [people].
# [forge.reviewers]
# octocat = \"<account id>\"

# Code zones, keyed by their folder from the repository root: `owner` is the
# tracker account (or [people] handle) that decides the tickets without an
# assignee touching the zone. Default: none.
# [zones.\"backend/billing\"]
# owner = \"<account id>\"

# The project's people, each once under a handle (a lowercase letter, then up
# to 31 lowercase letters, digits, `-` or `_`): `tracker`, their account as the
# tracker reports it, and `forge`, their login (optional). Default: none.
# With it, admitters and owners name handles, every person with a `forge`
# login is a reviewer, and [forge.reviewers] is refused.
# [people.ann]
# tracker = \"<account id>\"
# forge = \"<forge login>\"

# Rules added to one role's brief alone, after stack.rules and within its
# limits: build, resolver, intake, design, design_review, verify or arbiter.
# Default: none.
# [roles.verify]
# rules = [\"docs/review.md\"]
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

/// What `init` prints of `[people]` for the accounts it signed in as
/// ([`people_entry`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PeopleEntry {
    /// An entry to add: no account read is listed. Its handle is a
    /// placeholder for the person to choose.
    Add(String),
    /// The entry of `handle`, which lists one of the accounts read, with
    /// the other corrected: it replaces that entry, as a table is defined
    /// once.
    Replace { handle: String, entry: String },
    /// The tracker account is listed under the handle `tracker` and the
    /// forge login under `forge`: one person in two entries, which a person
    /// sorts out by hand.
    Split { tracker: String, forge: String },
}

/// What `init` prints for a project file kept as it is whose `[people]`
/// does not list the accounts it signed in as (OWL-582, build plan "P10 ·
/// Teams", "People"): the file changes through a pull request, so `init`
/// writes nothing to it. `None` without the table, when nothing was read,
/// and when every account read is listed, both under one handle.
pub fn people_entry(config: &ProjectConfig, signed_in: &SignedIn) -> Option<PeopleEntry> {
    let people = config.people.as_ref()?;
    if signed_in.tracker.is_none() && signed_in.forge.is_none() {
        return None;
    }
    let by_tracker = signed_in
        .tracker
        .as_deref()
        .and_then(|account| config.handle_of(account));
    let by_forge = signed_in.forge.as_deref().and_then(|login| {
        people
            .iter()
            .find(|(_, person)| {
                person
                    .forge
                    .as_deref()
                    .is_some_and(|listed| listed.eq_ignore_ascii_case(login))
            })
            .map(|(handle, _)| handle.as_str())
    });
    let listed = match (&signed_in.tracker, &signed_in.forge) {
        (Some(_), Some(_)) => by_tracker.is_some() && by_tracker == by_forge,
        (Some(_), None) => by_tracker.is_some(),
        (None, _) => by_forge.is_some(),
    };
    if listed {
        return None;
    }
    if let (Some(tracker), Some(forge)) = (by_tracker, by_forge)
        && tracker != forge
    {
        return Some(PeopleEntry::Split {
            tracker: tracker.to_owned(),
            forge: forge.to_owned(),
        });
    }
    let handle = by_tracker.or(by_forge);
    let person = handle.and_then(|handle| people.get(handle));
    let quote = |text: &str| toml::Value::String(text.to_owned()).to_string();
    let mut entry = format!("[people.{}]\n", handle.unwrap_or("<handle>"));
    match signed_in
        .tracker
        .as_deref()
        .or(person.map(|p| p.tracker.as_str()))
    {
        Some(tracker) => entry.push_str(&format!("tracker = {}\n", quote(tracker))),
        None => entry.push_str("tracker = \"<your tracker account>\"\n"),
    }
    if let Some(forge) = signed_in
        .forge
        .as_deref()
        .or(person.and_then(|p| p.forge.as_deref()))
    {
        entry.push_str(&format!("forge = {}\n", quote(forge)));
    }
    Some(match handle {
        Some(handle) => PeopleEntry::Replace {
            handle: handle.to_owned(),
            entry,
        },
        None => PeopleEntry::Add(entry),
    })
}

/// The cache folders of `folders` this machine does not open yet, given the
/// folders its personal file allows (`allowed`, from
/// [`crate::config::Effective::allowed_caches`]), in order (OWL-593). None
/// when the personal file could not be read or parsed: its own error comes
/// first, and nothing here can be added to it.
pub fn caches_to_allow<'a>(
    folders: impl IntoIterator<Item = &'a str>,
    allowed: Option<&[&str]>,
) -> Vec<String> {
    let Some(allowed) = allowed else {
        return Vec::new();
    };
    folders
        .into_iter()
        .filter(|folder| !allowed.contains(folder))
        .map(ToOwned::to_owned)
        .collect()
}

/// The personal file's lines that let the project of `repo` open `folders`
/// as caches (OWL-593): its `[repositories."github.com/<owner>/<name>"]`
/// table, or the machine-wide `allow_caches` without a GitHub repository.
/// `init` prints them and `doctor` gives them as the fix; neither writes
/// the personal file, the operator's to change.
pub fn allow_caches_lines(repo: Option<&Repo>, folders: &[String]) -> String {
    let quote = |text: &str| toml::Value::String(text.to_owned()).to_string();
    let list: Vec<String> = folders.iter().map(|folder| quote(folder)).collect();
    let key = format!("allow_caches = [{}]\n", list.join(", "));
    match repo {
        Some(repo) => format!(
            "[repositories.{}]\n{key}",
            quote(&format!("github.com/{repo}"))
        ),
        None => key,
    }
}

/// How to add [`allow_caches_lines`] to the personal file at `personal`
/// (shown as the caller shows paths), without defining a key or a table
/// twice, which would make the file invalid (OWL-593).
pub fn allow_caches_advice(repo: Option<&Repo>, folders: &[String], personal: &str) -> String {
    let lines = allow_caches_lines(repo, folders);
    let place = if repo.is_some() {
        "when the file has that table already, add the folders to its `allow_caches`"
    } else {
        "at the top of the file, before any table; when it has `allow_caches` already, add the \
         folders to it"
    };
    format!("Add to {personal} ({place}):\n\n{lines}")
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
/// file over 1 MiB or not UTF-8 is left unread, which detection reports as
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
    Ok(RootFiles {
        names,
        files,
        home_managers: vec![],
    })
}

/// A version manager [`home_managers`] looks for (OWL-631).
struct ManagerSpec {
    /// Its executable on `PATH`, one of [`owlshift_core::stack::HOME_MANAGERS`].
    name: &'static str,
    /// The variable that moves its installs folder.
    variable: &'static str,
    /// Its installs folder without the variable, relative to the home: the
    /// first that exists.
    defaults: &'static [&'static str],
}

/// The version managers whose installs may lie in the home. pipx's folders
/// are read from its `paths.py` (1.18): `PIPX_HOME`, then the legacy
/// `~/.local/pipx` when it exists, then platformdirs' data folder, checked
/// live on Linux; the others are their documented defaults.
const MANAGER_SPECS: [ManagerSpec; 4] = [
    ManagerSpec {
        name: "pyenv",
        variable: "PYENV_ROOT",
        defaults: &[".pyenv"],
    },
    ManagerSpec {
        name: "rbenv",
        variable: "RBENV_ROOT",
        defaults: &[".rbenv"],
    },
    ManagerSpec {
        name: "asdf",
        variable: "ASDF_DATA_DIR",
        defaults: &[".asdf"],
    },
    ManagerSpec {
        name: "pipx",
        variable: "PIPX_HOME",
        defaults: &[
            ".local/pipx",
            ".local/share/pipx",
            "Library/Application Support/pipx",
        ],
    },
];

/// The version managers on `PATH` whose installs folder lies in the home
/// (OWL-631), read through `var` from the environment: an executable of
/// that name in a `PATH` folder, and its folder, from its variable when set
/// or else its first default that exists, a folder under `HOME`. Homebrew's
/// pyenv lives outside the home and installs into `~/.pyenv`, so the
/// installs folder decides, not where the executable lies.
pub fn home_managers(var: impl Fn(&str) -> Option<OsString>) -> Vec<HomeManager> {
    let Some(home) = var("HOME").filter(|h| !h.is_empty()).map(PathBuf::from) else {
        return vec![];
    };
    let Ok(home) = home.canonicalize() else {
        return vec![];
    };
    let path: Vec<PathBuf> = var("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    let mut found = vec![];
    for spec in &MANAGER_SPECS {
        if !path.iter().any(|dir| dir.join(spec.name).is_file()) {
            continue;
        }
        let folder = match var(spec.variable).filter(|v| !v.is_empty()) {
            Some(set) => Some(PathBuf::from(set)),
            None => spec
                .defaults
                .iter()
                .map(|d| home.join(d))
                .find(|f| f.is_dir()),
        };
        let Some(Ok(folder)) = folder.filter(|f| f.is_dir()).map(|f| f.canonicalize()) else {
            continue;
        };
        // Joined with `/` whatever the platform, as `caches` and
        // `allow_caches` write folders.
        let Some(relative) = folder
            .strip_prefix(&home)
            .ok()
            .and_then(|r| {
                r.components()
                    .map(|c| c.as_os_str().to_str())
                    .collect::<Option<Vec<_>>>()
            })
            .map(|parts| parts.join("/"))
            .filter(|r| !r.is_empty())
        else {
            continue;
        };
        found.push(HomeManager {
            name: spec.name.to_owned(),
            folder: relative,
        });
    }
    found
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

/// Where adopting a project on a team is published (OWL-590): the people
/// table, the label gesture and claims, which `doctor`'s team lines point to.
pub const TEAM_PAGE: &str = "https://github.com/pitchopp/owlshift/blob/main/docs/adopt/team.md";

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
/// as `owlshift do` resolves it ([`secrets`]), a tracker secret within the
/// tracker's set `tracker` ([`crate::tracker::Kind::secret_set`]). Without
/// a repository, the project's secrets cannot be named: they come back
/// apart, and only the machine's are wanted.
pub fn wanted_secrets(
    keychain: &Keychain,
    specs: &[SecretSpec],
    tracker: Option<&'static SecretSet>,
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
        let set = tracker.filter(|set| set.entries.contains(&spec.account));
        let shared = if let Some(set) = set {
            let found = secrets::tracker(repo, set, &mut look)?;
            (found.accounts.source == Source::Shared && found.get(spec.account).is_some())
                .then(|| found.accounts.of(spec.account).to_owned())
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
    let (id_account, secret_account) = (
        accounts.of(LINEAR_APP_ID_ACCOUNT),
        accounts.of(LINEAR_APP_SECRET_ACCOUNT),
    );
    let paired = contains(id_account)? && contains(secret_account)?;
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
    let Some(id) = given(wanted(LINEAR_APP_ID, id_account)) else {
        return Ok(if paired {
            AppStored::Kept
        } else {
            AppStored::Skipped
        });
    };
    let Some(secret) = given(wanted(LINEAR_APP_SECRET, secret_account)) else {
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
    let (id_account, secret_account) = (
        accounts.of(LINEAR_APP_ID_ACCOUNT),
        accounts.of(LINEAR_APP_SECRET_ACCOUNT),
    );
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
mod reference;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tracker::LINEAR_SECRETS;

    fn options(tracker: TrackerKind, team: Option<&str>, gate: &[&str]) -> InitOptions {
        InitOptions {
            tracker,
            team: team.map(ToOwned::to_owned),
            gate: gate.iter().map(|c| (*c).to_owned()).collect(),
            detected: Detection::default(),
            admitter: None,
        }
    }

    /// OWL-582: beside a kept `[people]`, the entry printed for the accounts
    /// signed in: none when they are listed under one handle or nothing was
    /// read; the listed handle when one account is; a placeholder else.
    #[test]
    fn the_people_entry_names_the_accounts_signed_in() {
        let text = project_file(&options(TrackerKind::Markdown, None, &["make"])).unwrap();
        let kept = ProjectConfig::parse(&format!(
            "{text}\n[people.ann]\ntracker = \"u-ann\"\nforge = \"Ann-Dev\"\n\
             [people.bob]\ntracker = \"u-bob\"\nforge = \"bob-dev\"\n"
        ))
        .unwrap();
        let add = |entry: &str| Some(PeopleEntry::Add(entry.to_owned()));
        let signed = |tracker: Option<&str>, forge: Option<&str>| SignedIn {
            tracker: tracker.map(ToOwned::to_owned),
            forge: forge.map(ToOwned::to_owned),
        };
        for listed in [
            signed(Some("u-ann"), Some("ann-dev")),
            signed(Some("u-ann"), None),
            signed(None, Some("ANN-DEV")),
            signed(None, None),
        ] {
            assert_eq!(people_entry(&kept, &listed), None, "{listed:?}");
        }
        assert_eq!(
            people_entry(&kept, &signed(Some("u-cy"), Some("cy"))),
            add("[people.<handle>]\ntracker = \"u-cy\"\nforge = \"cy\"\n")
        );
        assert_eq!(
            people_entry(&kept, &signed(None, Some("cy"))),
            add("[people.<handle>]\ntracker = \"<your tracker account>\"\nforge = \"cy\"\n")
        );
    }

    /// OWL-582: one account read is listed and the other is new or changed:
    /// that person's entry is replaced, never defined a second time; the two
    /// listed under two handles are named, and no merged entry is printed.
    #[test]
    fn a_listed_person_is_replaced_and_two_handles_are_named() {
        let text = project_file(&options(TrackerKind::Markdown, None, &["make"])).unwrap();
        let kept = ProjectConfig::parse(&format!(
            "{text}\n[people.ann]\ntracker = \"u-ann\"\nforge = \"Ann-Dev\"\n\
             [people.bob]\ntracker = \"u-bob\"\n"
        ))
        .unwrap();
        let signed = |tracker: Option<&str>, forge: Option<&str>| SignedIn {
            tracker: tracker.map(ToOwned::to_owned),
            forge: forge.map(ToOwned::to_owned),
        };
        let replace = |handle: &str, entry: &str| {
            Some(PeopleEntry::Replace {
                handle: handle.to_owned(),
                entry: entry.to_owned(),
            })
        };
        assert_eq!(
            people_entry(&kept, &signed(Some("u-ann"), Some("ann2"))),
            replace(
                "ann",
                "[people.ann]\ntracker = \"u-ann\"\nforge = \"ann2\"\n"
            )
        );
        assert_eq!(
            people_entry(&kept, &signed(Some("u-bob"), Some("bob-dev"))),
            replace(
                "bob",
                "[people.bob]\ntracker = \"u-bob\"\nforge = \"bob-dev\"\n"
            )
        );
        assert_eq!(
            people_entry(&kept, &signed(Some("u-new"), Some("ann-dev"))),
            replace(
                "ann",
                "[people.ann]\ntracker = \"u-new\"\nforge = \"ann-dev\"\n"
            )
        );
        assert_eq!(
            people_entry(&kept, &signed(Some("u-bob"), Some("ann-dev"))),
            Some(PeopleEntry::Split {
                tracker: "bob".to_owned(),
                forge: "ann".to_owned(),
            })
        );
        let without = ProjectConfig::parse(&text).unwrap();
        assert_eq!(people_entry(&without, &signed(Some("u-bob"), None)), None);
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
        assert_eq!(config.stack.caches, [".pyenv"]);

        let markdown = project_file(&options(TrackerKind::Markdown, None, &[])).unwrap();
        let config = ProjectConfig::parse(&markdown).unwrap();
        assert_eq!(config.tracker.kind, TrackerKind::Markdown);
        assert!(config.stack.gate.is_empty());

        let error = project_file(&options(TrackerKind::Linear, Some(" "), &[])).unwrap_err();
        assert!(error.contains("--team"), "{error}");
    }

    pub(super) const TRACKERS: [TrackerKind; 3] = [
        TrackerKind::Linear,
        TrackerKind::Github,
        TrackerKind::Markdown,
    ];

    /// The project file `init` writes for `kind`, with a gate.
    pub(super) fn written(kind: TrackerKind) -> String {
        let team = (kind == TrackerKind::Linear).then_some("OWL");
        project_file(&options(kind, team, &["make test"])).unwrap()
    }

    /// The keys of the project file that do not apply to `kind`'s tracker.
    fn not_applying(kind: TrackerKind) -> &'static [&'static str] {
        match kind {
            TrackerKind::Linear => &["tracker.priorities"],
            TrackerKind::Github => &["tracker.team"],
            TrackerKind::Markdown => &["tracker.team", "tracker.priorities"],
        }
    }

    /// Whether the dotted key `key` is `of` or a key under it, a `*` of `of`
    /// standing for any one part (`people.ann.forge` is under `people.*`).
    fn is_under(key: &str, of: &str) -> bool {
        let key: Vec<&str> = key.split('.').collect();
        let of: Vec<&str> = of.split('.').collect();
        key.len() >= of.len() && of.iter().zip(&key).all(|(o, k)| *o == "*" || o == k)
    }

    /// The leaf keys of the JSON schema `stem` (`project-config`,
    /// `personal-config`), dotted, `*` for a key a person names
    /// (`zones.*.owner`).
    pub(super) fn schema_keys(stem: &str) -> Vec<String> {
        let (_, schema) = owlshift_contracts::schema::all()
            .into_iter()
            .find(|(name, _)| *name == stem)
            .unwrap();
        let root = schema.as_value();
        let mut keys = Vec::new();
        schema_leaves(root, &root["$defs"], "", &mut keys);
        keys
    }

    /// The leaves of `node`: a reference is followed, an optional value
    /// (one branch besides null) read as that branch, an object's keys
    /// walked, a map's under `*`; anything else, a choice included, is a
    /// leaf.
    fn schema_leaves(
        node: &serde_json::Value,
        defs: &serde_json::Value,
        prefix: &str,
        out: &mut Vec<String>,
    ) {
        let null = serde_json::json!("null");
        let mut node = node;
        loop {
            if let Some(name) = node
                .get("$ref")
                .and_then(serde_json::Value::as_str)
                .and_then(|reference| reference.strip_prefix("#/$defs/"))
            {
                node = &defs[name];
                continue;
            }
            let branches: Vec<&serde_json::Value> = ["anyOf", "oneOf"]
                .iter()
                .filter_map(|word| node.get(*word).and_then(serde_json::Value::as_array))
                .flatten()
                .filter(|branch| branch.get("type") != Some(&null))
                .collect();
            match branches[..] {
                [one] => node = one,
                _ => break,
            }
        }
        let under = |key: &str| {
            if prefix.is_empty() {
                key.to_owned()
            } else {
                format!("{prefix}.{key}")
            }
        };
        if let Some(properties) = node
            .get("properties")
            .and_then(serde_json::Value::as_object)
        {
            for (name, inner) in properties {
                schema_leaves(inner, defs, &under(name), out);
            }
        } else if let Some(map) = node.get("additionalProperties").filter(|v| v.is_object()) {
            schema_leaves(map, defs, &under("*"), out);
        } else {
            out.push(prefix.to_owned());
        }
    }

    /// The leaves of a parsed TOML table, dotted under `prefix`; an empty
    /// table is a leaf.
    pub(super) fn toml_leaves(table: &toml::Table, prefix: &str, out: &mut Vec<String>) {
        for (key, value) in table {
            let path = if prefix.is_empty() {
                key.clone()
            } else {
                format!("{prefix}.{key}")
            };
            match value {
                toml::Value::Table(inner) if !inner.is_empty() => toml_leaves(inner, &path, out),
                _ => out.push(path),
            }
        }
    }

    /// One line of a project file, one leading `# ` taken off, read as TOML:
    /// `Some((true, table))` for a table header, `Some((false, keys))` for
    /// a key line (`key = [` opening an array counting as `key = []`),
    /// `None` for prose and the rest.
    pub(super) fn toml_line(line: &str) -> Option<(bool, toml::Table)> {
        let bare = line.strip_prefix("# ").unwrap_or(line);
        let bare = match bare.strip_suffix(" = [") {
            Some(key) => format!("{key} = []"),
            None => bare.to_owned(),
        };
        let table = bare.parse::<toml::Table>().ok().filter(|t| !t.is_empty())?;
        Some((bare.starts_with('['), table))
    }

    /// The keys `text` names, set or commented out, dotted under their
    /// table.
    pub(super) fn file_keys(text: &str) -> Vec<String> {
        let mut table = String::new();
        let mut keys = Vec::new();
        for line in text.lines() {
            let Some((header, parsed)) = toml_line(line) else {
                continue;
            };
            let mut found = Vec::new();
            if header {
                toml_leaves(&parsed, "", &mut found);
                table = found.pop().unwrap();
            } else {
                toml_leaves(&parsed, &table, &mut found);
                keys.extend(found);
            }
        }
        keys
    }

    /// OWL-594: the file `init` writes names every key of the project
    /// file's schema that applies to its tracker, set or commented out, and
    /// none that does not apply or the schema lacks: a key added to the
    /// schema fails here until the file documents it.
    #[test]
    fn every_key_that_applies_is_in_the_file() {
        let schema = schema_keys("project-config");
        for key in ["policy.max_in_flight", "forge.merge", "people.*.forge"] {
            assert!(schema.iter().any(|k| k == key), "{key}: {schema:?}");
        }
        for kind in TRACKERS {
            let text = written(kind);
            let file = file_keys(&text);
            for key in &schema {
                let applies = !not_applying(kind).iter().any(|out| is_under(key, out));
                let named = file.iter().any(|f| is_under(f, key));
                assert_eq!(named, applies, "{kind:?}, {key}:\n{text}");
            }
            for key in &file {
                assert!(
                    schema.iter().any(|s| is_under(key, s)),
                    "{kind:?}: {key} is no key of the project file"
                );
            }
        }
    }

    /// `text` with each commented-out key whose comment gives its value as
    /// the default (``Default: `value`.``) uncommented, with its commented
    /// table header; and the keys uncommented, in order.
    fn with_defaults_uncommented(text: &str) -> (String, Vec<String>) {
        let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
        let mut header = None;
        let mut prose = String::new();
        let mut keys = Vec::new();
        for i in 0..lines.len() {
            let line = lines[i].clone();
            match toml_line(&line) {
                Some((true, _)) => header = Some(i),
                Some((false, _)) => {
                    let (key, value) = line.split_once(" = ").unwrap();
                    if let Some(key) = key.strip_prefix("# ")
                        && prose.contains(&format!("Default: `{value}`"))
                    {
                        lines[i] = line[2..].to_owned();
                        if let Some(at) = header
                            && let Some(bare) = lines[at].strip_prefix("# ")
                        {
                            lines[at] = bare.to_owned();
                        }
                        keys.push(key.to_owned());
                    }
                }
                None => {
                    if let Some(words) = line.strip_prefix('#') {
                        prose.push(' ');
                        prose.push_str(words.trim());
                        continue;
                    }
                }
            }
            prose.clear();
        }
        (lines.join("\n") + "\n", keys)
    }

    /// What `config` decides: the values its keys with a default take, read
    /// through their accessors, and the rest of it with those keys left out.
    /// A `[forge]` decides only its merge, method and reviewers.
    fn effective(mut config: ProjectConfig) -> (String, ProjectConfig) {
        let forge = config.forge.take().unwrap_or_default();
        let (policy, pipeline) = (&config.policy, &config.pipeline);
        let decided = format!(
            "{:?}",
            (
                policy.quiet_window(),
                policy.max_in_flight(),
                policy.max_open_prs(),
                policy.overlap(),
                policy.base_repair(),
                policy.claims(),
                policy.resolver(),
                pipeline.findings(),
                pipeline.codex_reviewer(),
                (forge.merge(), forge.merge_method(), forge.reviewers),
            )
        );
        let policy = &mut config.policy;
        policy.quiet_window_minutes = None;
        policy.max_in_flight = None;
        policy.max_open_prs = None;
        policy.overlap = None;
        policy.base_repair = None;
        policy.claims = None;
        policy.resolver_delegation = None;
        policy.resolver_passes = None;
        config.pipeline.findings = None;
        config.pipeline.codex_reviewer = None;
        (decided, config)
    }

    /// OWL-594: uncommenting every key the file gives with its default
    /// leaves the effective configuration as it was.
    #[test]
    fn uncommenting_the_defaults_changes_nothing() {
        for kind in TRACKERS {
            let text = written(kind);
            let (uncommented, keys) = with_defaults_uncommented(&text);
            assert_eq!(
                keys,
                [
                    "findings",
                    "codex_reviewer",
                    "quiet_window_minutes",
                    "max_in_flight",
                    "max_open_prs",
                    "overlap",
                    "base_repair",
                    "claims",
                    "resolver_delegation",
                    "resolver_passes",
                    "merge",
                    "merge_method",
                ],
                "{kind:?}"
            );
            let before = ProjectConfig::parse(&text).unwrap();
            let after =
                ProjectConfig::parse(&uncommented).unwrap_or_else(|e| panic!("{e}\n{uncommented}"));
            assert_ne!(before, after, "{kind:?}");
            assert_eq!(effective(before), effective(after), "{kind:?}");
        }
    }

    /// OWL-594: the file sets the values it set before every key was
    /// listed, so `doctor` and every command read the same configuration.
    #[test]
    fn the_file_sets_what_it_set_before() {
        let version = Version::parse(OWLSHIFT_VERSION).unwrap();
        let labels = "states = { ready = \"Todo\", working = \"In Progress\", needs_input = \
                      \"Needs Input\", review = \"In Review\" }";
        for (kind, tracker) in [
            (
                TrackerKind::Linear,
                format!("kind = \"linear\"\nteam = \"OWL\"\nadmit = {{ label = \"agent\" }}\n{labels}"),
            ),
            (
                TrackerKind::Github,
                "kind = \"github\"\nadmit = \"ready\"\nstates = { ready = \"agent:ready\", working = \
                 \"agent:working\", needs_input = \"agent:needs-input\", review = \"agent:review\" }"
                    .to_owned(),
            ),
            (
                TrackerKind::Markdown,
                format!("kind = \"markdown\"\nadmit = {{ label = \"agent\" }}\n{labels}"),
            ),
        ] {
            let before = format!(
                "requires = \">={}.{}\"\n[tracker]\n{tracker}\n[stack]\ngate = [\"make test\"]\n\
                 [pipeline]\ndefault = \"standard\"\nplan_approval = \"on-fork\"\n[models]\n\
                 [policy]\nalways_human = []\n",
                version.major, version.minor
            );
            assert_eq!(
                ProjectConfig::parse(&written(kind)).unwrap(),
                ProjectConfig::parse(&before).unwrap(),
                "{kind:?}"
            );
        }
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
            ..RootFiles::default()
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
        assert!(text.contains(
            "\n# command, `owlshift do` refuses to start a ticket.\ngate = []\n# Variables of"
        ));
        assert!(text.contains("# network open before"));
        assert!(
            text.contains("\"standard\" (design, design review, build\n# and verify)")
                && !text.contains("build stage only")
        );
        assert!(
            text.contains(
                "\n# setup = [\"pnpm install --frozen-lockfile\"]\n# Folders of your home"
            )
        );
        assert!(text.contains("\n# run_caches = { CARGO_HOME = \"cargo\" }\n"));
        assert!(!text.contains("read from") && !text.contains("# note:"));
    }

    /// OWL-631: a pinned tool chain's note is written above the gate, and
    /// the file still loads.
    #[test]
    fn a_pinned_python_writes_its_note_above_the_gate() {
        let text = project_file(&with(detected(&[".python-version"], &[]), &[])).unwrap();
        assert!(
            text.contains("\n# note: `.python-version` names a Python: installed by"),
            "{text}"
        );
        ProjectConfig::parse(&text).unwrap();
    }

    /// OWL-631: a manager counts when it is on `PATH` and its installs
    /// folder, from its variable or its first default that exists, lies in
    /// the home.
    #[test]
    fn a_manager_on_path_counts_with_its_installs_in_the_home() {
        let home = tempfile::tempdir().unwrap();
        let bin = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        for name in ["pyenv", "rbenv", "pipx"] {
            std::fs::write(bin.path().join(name), "").unwrap();
        }
        // A folder named like a manager is not its executable.
        std::fs::create_dir(bin.path().join("asdf")).unwrap();
        for folder in [".pyenv", ".asdf", ".local/pipx", ".local/share/pipx"] {
            std::fs::create_dir_all(home.path().join(folder)).unwrap();
        }
        let found = |vars: &[(&str, &std::path::Path)]| {
            let vars: Vec<(String, OsString)> = [("HOME", home.path()), ("PATH", bin.path())]
                .iter()
                .chain(vars)
                .map(|(k, v)| (k.to_string(), v.as_os_str().to_owned()))
                .collect();
            home_managers(|name| {
                vars.iter()
                    .rev()
                    .find(|(k, _)| k == name)
                    .map(|(_, v)| v.clone())
            })
            .into_iter()
            .map(|m| (m.name, m.folder))
            .collect::<Vec<_>>()
        };
        let pair = |n: &str, f: &str| (n.to_owned(), f.to_owned());
        // rbenv has no folder; asdf is no executable; pipx's legacy folder
        // wins over the platform's.
        assert_eq!(
            found(&[]),
            [pair("pyenv", ".pyenv"), pair("pipx", ".local/pipx")]
        );
        // A variable outside the home, or the home itself, gives nothing.
        assert_eq!(
            found(&[("PYENV_ROOT", outside.path()), ("PIPX_HOME", home.path())]),
            []
        );
        let rubies = home.path().join("rubies");
        std::fs::create_dir(&rubies).unwrap();
        // Off `PATH`, no manager counts.
        assert_eq!(
            found(&[("RBENV_ROOT", &rubies), ("PATH", outside.path())]),
            []
        );
        assert_eq!(
            found(&[("RBENV_ROOT", &rubies)]),
            [
                pair("pyenv", ".pyenv"),
                pair("rbenv", "rubies"),
                pair("pipx", ".local/pipx")
            ]
        );
        assert!(home_managers(|_| None).is_empty());
        assert_eq!(
            MANAGER_SPECS.map(|s| s.name),
            owlshift_core::stack::HOME_MANAGERS
        );
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
        // OWL-617: npm installs online into a run cache, as cargo fetches
        // into its own; no folder of the home is read.
        assert_eq!(config.stack.setup, ["npm ci"]);
        assert!(config.stack.caches.is_empty());
        assert_eq!(
            config.stack.run_cache_entries(),
            [("CARGO_HOME", "cargo"), ("npm_config_cache", "npm")]
        );
        assert_eq!(text.matches("    # read from ").count(), 5, "{text}");
        assert!(
            text.contains(
                "    # read from the `test` script of package.json\n    \"npm run test\","
            )
        );
        assert!(
            text.contains(
                "    # read from package-lock.json, the lock file of npm\n    \"npm ci\","
            )
        );
        assert!(
            text.contains(
                "# read from Cargo.toml\nrun_caches.CARGO_HOME = \"cargo\"\n# read from \
                 package-lock.json, the lock file of npm\nrun_caches.npm_config_cache = \"npm\"\n"
            ),
            "{text}"
        );
        assert!(text.contains("# caches = "), "{text}");
        assert!(!text.contains("# run_caches = "), "{text}");
    }

    /// OWL-593: the lines `init` prints and `doctor` gives let a project
    /// declaring a cache of the home (yarn 4's, as `init` wrote it before
    /// OWL-629) load once added to the personal file, with or without its
    /// repository: the key is the one the loader reads.
    #[test]
    fn the_allow_caches_lines_let_the_project_load() {
        let project = project_file(&with(Detection::default(), &["make"]))
            .unwrap()
            .replace("# caches = [\".pyenv\"]\n", "caches = [\".yarn/berry\"]\n");
        let repo = Repo::parse("acme/web").unwrap();
        let dir = tempfile::tempdir().unwrap();
        let personal = dir.path().join("config.toml");
        let load = || {
            crate::config::Effective::served(
                &repo,
                dir.path().join(PROJECT_FILE),
                &project,
                Some(personal.clone()),
            )
        };
        let wanted = vec![".yarn/berry".to_owned()];
        assert_eq!(caches_to_allow([".yarn/berry"], Some(&[])), wanted);
        assert_eq!(caches_to_allow([".yarn/berry"], None), Vec::<String>::new());
        assert!(matches!(
            load().project,
            crate::config::FileState::Invalid { .. }
        ));
        for scope in [Some(&repo), None] {
            std::fs::write(&personal, allow_caches_lines(scope, &wanted)).unwrap();
            let loaded = load();
            assert!(
                matches!(loaded.project, crate::config::FileState::Loaded { .. }),
                "{:?}",
                loaded.project
            );
            let allowed = loaded.allowed_caches(Some(&repo)).unwrap();
            assert!(caches_to_allow([".yarn/berry"], Some(&allowed)).is_empty());
        }
        assert!(
            allow_caches_advice(Some(&repo), &wanted, "~/x.toml").contains(
                "Add to ~/x.toml (when the file has that table already, add the folders to its \
             `allow_caches`):\n\n[repositories.\"github.com/acme/web\"]\nallow_caches = \
             [\".yarn/berry\"]\n"
            )
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

    /// OWL-624: a Go module and a Gradle build are written with their
    /// sources and load; a source that gives no line, a Gemfile with neither
    /// lock nor `.rspec`, still has its notes written, as does Gradle's own
    /// line under a gate given by hand.
    #[test]
    fn a_go_and_gradle_root_is_written_and_a_lineless_source_keeps_its_notes() {
        let d = detected(&["go.mod", "gradlew", "build.gradle.kts", "Gemfile"], &[]);
        let text = project_file(&with(d.clone(), &[])).unwrap();
        let config = ProjectConfig::parse(&text).unwrap();
        assert_eq!(
            config.stack.gate,
            [
                "! gofmt -l . | grep .",
                "go vet ./...",
                "go test ./...",
                "./gradlew check --no-daemon"
            ]
        );
        assert_eq!(config.stack.setup, ["go mod download"]);
        assert_eq!(
            config.stack.run_cache_entries(),
            [
                ("GOCACHE", "go-build"),
                ("GOMODCACHE", "go-mod"),
                ("GRADLE_USER_HOME", "gradle")
            ]
        );
        assert!(text.contains(
            "# read from go.mod\nrun_caches.GOMODCACHE = \"go-mod\"\n# read from go.mod\n\
             run_caches.GOCACHE = \"go-build\"\n# read from gradlew, the Gradle wrapper\n\
             run_caches.GRADLE_USER_HOME = \"gradle\"\n"
        ));
        assert!(text.contains("    # read from go.mod\n    \"! gofmt -l . | grep .\","));
        assert!(text.contains("# note: Go keeps the modules it downloads read-only"));
        assert!(text.contains("# note: Gemfile has no lock file"), "{text}");
        assert!(text.contains("# note: no Ruby gate was written"), "{text}");
        let by_hand = project_file(&with(d, &["make ci"])).unwrap();
        assert_eq!(
            ProjectConfig::parse(&by_hand).unwrap().stack.gate,
            ["make ci"]
        );
        assert!(by_hand.contains("run_caches.GRADLE_USER_HOME = \"gradle\""));
        assert!(by_hand.contains("# note: Gemfile has no lock file"));
    }

    /// OWL-627: Maven's line, which holds quotes and `$MAVEN_USER_HOME`,
    /// loads back as written, with its note; Composer and .NET give their
    /// setup and gate in the fixed order.
    #[test]
    fn a_maven_composer_and_dotnet_root_is_written_and_loads() {
        let d = detected(
            &[
                "pom.xml",
                "composer.json",
                "composer.lock",
                "phpunit.xml.dist",
                "Probe.slnx",
            ],
            &[],
        );
        let text = project_file(&with(d, &[])).unwrap();
        let config = ProjectConfig::parse(&text).unwrap();
        assert_eq!(
            config.stack.gate,
            [
                "mvn -B -Dmaven.repo.local=\"$MAVEN_USER_HOME/repository\" verify",
                "vendor/bin/phpunit",
                "DOTNET_SYSTEM_NET_SECURITY_USENETWORKFRAMEWORK=1 dotnet test -m:1 \
                 -nodeReuse:false Probe.slnx"
            ]
        );
        assert_eq!(
            config.stack.setup,
            [
                "composer install --no-interaction",
                "DOTNET_SYSTEM_NET_SECURITY_USENETWORKFRAMEWORK=1 dotnet restore -m:1 \
                 -nodeReuse:false Probe.slnx"
            ]
        );
        assert_eq!(
            config.stack.run_cache_entries(),
            [("DOTNET_CLI_HOME", "dotnet"), ("MAVEN_USER_HOME", "maven")]
        );
        assert!(
            text.contains("# note: the Maven line reads `$MAVEN_USER_HOME`"),
            "{text}"
        );
    }

    /// OWL-628: Elixir and Deno lines load back from the file, with their
    /// run caches; Swift's note is written above nothing.
    #[test]
    fn an_elixir_and_deno_root_is_written_and_loads() {
        let d = detected(
            &[
                "mix.exs",
                "mix.lock",
                ".formatter.exs",
                "deno.json",
                "deno.lock",
                "Package.swift",
            ],
            &[],
        );
        let text = project_file(&with(d, &[])).unwrap();
        let config = ProjectConfig::parse(&text).unwrap();
        assert_eq!(
            config.stack.gate,
            [
                "mix format --check-formatted",
                "mix test",
                "deno fmt --check",
                "deno lint",
                "deno test --frozen --permit-no-files"
            ]
        );
        assert_eq!(
            config.stack.setup,
            [
                "[ ! -f /etc/ssl/cert.pem ] || export HEX_CACERTS_PATH=/etc/ssl/cert.pem; mix \
                 local.hex --force --if-missing && mix local.rebar --force --if-missing",
                "mix deps.get --check-locked",
                "deno install --frozen"
            ]
        );
        assert_eq!(
            config.stack.run_cache_entries(),
            [
                ("DENO_DIR", "deno"),
                ("HEX_HOME", "hex"),
                ("MIX_HOME", "mix")
            ]
        );
        assert!(text.contains("# note: Swift was not written"), "{text}");
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
        // OWL-617, OWL-629: pnpm's plain install, its home, its lock folder
        // and corepack's run caches, which load back from the file.
        assert_eq!(config.stack.setup, ["pnpm install --frozen-lockfile"]);
        assert!(config.stack.caches.is_empty());
        assert_eq!(
            config.stack.run_cache_entries(),
            [
                ("COREPACK_HOME", "corepack"),
                ("PNPM_HOME", "pnpm"),
                ("XDG_RUNTIME_DIR", "pnpm-locks")
            ]
        );
        assert!(!text.contains("pnpm run test"));
        assert!(text.contains(
            "# read from pnpm-lock.yaml, the lock file of pnpm\nrun_caches.PNPM_HOME = \"pnpm\"\n"
        ));
        assert!(
            text.contains("# note: `XDG_RUNTIME_DIR` is the folder pnpm 12"),
            "{text}"
        );
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
        wanted_secrets(keychain, specs, Some(&LINEAR_SECRETS), Some(&repo()))
            .unwrap()
            .0
    }

    fn specs(wanted: &[Wanted]) -> Vec<SecretSpec> {
        wanted.iter().map(|one| one.spec).collect()
    }

    #[test]
    fn secrets_are_asked_only_when_missing_and_never_reported() {
        const SENTINEL: &str = "lin_api_SENTINEL_never_shown";
        let keychain = Keychain::in_memory();
        let own = TrackerAccounts::project(&repo(), &LINEAR_SECRETS);
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
        assert_eq!(asked, [own.of(LINEAR_ACCOUNT), CLAUDE_AGENT_ACCOUNT]);
        assert_eq!(specs(&report.stored), [LINEAR_KEY, CLAUDE_TOKEN]);
        assert_eq!(specs(&report.kept), [GITHUB_TOKEN]);
        assert!(report.missing.is_empty());
        // Stored trimmed, and absent from everything a person is shown.
        assert_eq!(
            keychain
                .read(own.of(LINEAR_ACCOUNT))
                .unwrap()
                .unwrap()
                .expose(),
            SENTINEL
        );
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
        let own = TrackerAccounts::project(&repo(), &LINEAR_SECRETS);
        assert_eq!(
            keychain
                .read(own.of(LINEAR_ACCOUNT))
                .unwrap()
                .unwrap()
                .expose(),
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

    /// A shared set holding the Linear app's pair but no key is the set the
    /// project reads, yet no shared key is: the key's own entry decides.
    #[test]
    fn a_shared_app_without_a_shared_key_is_no_shared_key() {
        let keychain = Keychain::in_memory();
        for account in [LINEAR_APP_ID_ACCOUNT, LINEAR_APP_SECRET_ACCOUNT] {
            keychain.store(account, &Secret::new("x")).unwrap();
        }
        let key = wanted(&keychain, &[LINEAR_KEY]);
        assert_eq!(key[0].shared, None);
        assert_eq!(
            key[0].account,
            TrackerAccounts::project(&repo(), &LINEAR_SECRETS).of(LINEAR_ACCOUNT)
        );
    }

    /// Without a GitHub repository, only the machine's secret is wanted:
    /// the project's cannot be named.
    #[test]
    fn without_a_repository_only_the_machines_secret_is_wanted() {
        let keychain = Keychain::in_memory();
        let (wanted, unnamed) = wanted_secrets(
            &keychain,
            &required_secrets(TrackerKind::Linear),
            Some(&LINEAR_SECRETS),
            None,
        )
        .unwrap();
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
        let own = TrackerAccounts::project(&repo(), &LINEAR_SECRETS);
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
            (
                AppStored::Skipped,
                vec![own.of(LINEAR_APP_ID_ACCOUNT).to_owned()]
            )
        );
        assert_eq!(store(false, [Some("id"), None]).0, AppStored::Incomplete);
        assert_eq!(
            store(false, [Some("id"), Some("two\nlines")]).0,
            AppStored::Refused
        );
        assert!(!is_stored(own.of(LINEAR_APP_ID_ACCOUNT)));
        assert!(!is_stored(own.of(LINEAR_APP_SECRET_ACCOUNT)));

        assert_eq!(
            store(false, [Some(" id "), Some("secret")]),
            (
                AppStored::Stored,
                vec![
                    own.of(LINEAR_APP_ID_ACCOUNT).to_owned(),
                    own.of(LINEAR_APP_SECRET_ACCOUNT).to_owned()
                ]
            )
        );
        assert_eq!(
            keychain
                .read(own.of(LINEAR_APP_ID_ACCOUNT))
                .unwrap()
                .unwrap()
                .expose(),
            "id"
        );
        assert!(!is_stored(LINEAR_APP_ID_ACCOUNT));
        assert_eq!(
            store(false, [Some("x"), Some("y")]),
            (AppStored::Kept, vec![])
        );
        assert_eq!(store(true, [None, None]).0, AppStored::Kept);
        assert_eq!(store(true, [Some("id"), None]).0, AppStored::Incomplete);
        assert_eq!(
            keychain
                .read(own.of(LINEAR_APP_SECRET_ACCOUNT))
                .unwrap()
                .unwrap()
                .expose(),
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
            if account
                == TrackerAccounts::project(&repo(), &LINEAR_SECRETS).of(LINEAR_APP_SECRET_ACCOUNT)
            {
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
        let own = TrackerAccounts::project(&repo(), &LINEAR_SECRETS);
        let id_of = |k: &SecretFails| {
            Entries::read(&k.inner, own.of(LINEAR_APP_ID_ACCOUNT))
                .unwrap()
                .map(|s| s.expose().to_owned())
        };

        let none = keychain(false);
        let error = store_pair(&none, &own, &id, &secret).unwrap_err();
        assert_eq!(error, "keychain: the secret's write failed");
        assert_eq!(id_of(&none), None);

        let pair = keychain(false);
        pair.inner
            .store(own.of(LINEAR_APP_ID_ACCOUNT), &Secret::new("old-id"))
            .unwrap();
        pair.inner
            .store(
                own.of(LINEAR_APP_SECRET_ACCOUNT),
                &Secret::new("old-secret"),
            )
            .unwrap();
        let error = store_pair(&pair, &own, &id, &secret).unwrap_err();
        assert_eq!(error, "keychain: the secret's write failed");
        assert_eq!(id_of(&pair).as_deref(), Some("old-id"));

        for before in [None, Some("old-id")] {
            let stuck = keychain(true);
            if let Some(before) = before {
                stuck
                    .inner
                    .store(own.of(LINEAR_APP_ID_ACCOUNT), &Secret::new(before))
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
