//! `owlshift doctor`: whether this machine is ready, and how to fix what is
//! not.
//!
//! Nothing a probed program prints reaches the report except a version
//! rebuilt from its digits and a login label from a fixed list (live check
//! C8 in `docs/design/build-plan.md`).
//!
//! The one check that leaves the machine is a Linear project's
//! `tracker states` (OWL-147): it reads the Linear API key in the system
//! keychain and the team's workflow states from Linear, and reports state
//! names and Linear's error text only, never the key.
//!
//! The same goes for the two lines that follow it, `drain admission` and
//! `proposals inbox` (OWL-155): they read the labels, the accounts and the
//! team's inbox from Linear in one more call.
//!
//! Each check carries its own texts: a failure says why the check exists and
//! the steps that fix it, a warning says why it does not block `owlshift do`.
//! How the report is laid out, as text or JSON, is [`render`]'s (OWL-99).

pub mod render;

use std::path::{MAIN_SEPARATOR, Path};

use owlshift_adapters::forge::{ErrorKind as ForgeErrorKind, Repo};
use owlshift_adapters::harness::{self, Login, tested};
use owlshift_adapters::tracker::linear::{
    AdmissionFacts, LinearTracker, StateMismatch, find_state,
};
use owlshift_adapters::tracker::markdown::MarkdownTracker;
use owlshift_adapters::tracker::{Capability, ErrorKind, PROPOSED_LABEL};
use owlshift_contracts::config::{Admit, ProjectConfig, States, Tracker, TrackerKind};
use owlshift_contracts::ids::TicketId;
use owlshift_contracts::{DelegationLevel, Harness};
use owlshift_core::gate::PassLimit;
use owlshift_core::schedule::{holds_slot, natural_cmp};
use owlshift_core::vocab::Stage;
use owlshift_platform::keychain::SERVICE;
use owlshift_platform::sandbox::{BWRAP_APPARMOR_PROFILE, SandboxError};
use serde_json::Value;

use crate::codex_machine::{self, Declared, FULL_ACCESS};
use crate::config::{Effective, FileState, exit_text};
use crate::executor::harness::CLAUDE_AGENT_ACCOUNT;
use crate::executor::{
    MINIMUM_GIT_VERSION, git_floor_reason, git_install_advice, git_recheck_note,
};
use crate::forge::GITHUB_TOKEN_HELP;
use crate::secrets::{self, ForgeSecret, Source, TrackerAccounts, TrackerSecrets};
use crate::system::{
    AppError, DataDirSource, DefaultBranch, DefaultBranchFile, MarkerState, RunError, StatesError,
    System, TokenTryError, exact_version_of, older_than, version_of,
};
#[cfg(unix)]
use crate::system::{SentinelProbe, SentinelStatus};
use crate::ticket_ref::RefState;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Ok,
    Info,
    Warn,
    Fail,
}

impl Status {
    /// Its name in the JSON report.
    pub fn id(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Fail => "fail",
        }
    }
}

/// The part of the report a check belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Section {
    /// git and the harness CLIs.
    Tools,
    /// What keeps agent runs apart from the rest of the machine: the
    /// sandbox, the sentinel, the agent runs' own login.
    AgentIsolation,
    /// The configuration files and the tracker they name.
    Project,
}

impl Section {
    /// In the order the report shows them.
    pub const ALL: [Self; 3] = [Self::Tools, Self::AgentIsolation, Self::Project];

    /// Its heading in the text report.
    pub fn title(self) -> &'static str {
        match self {
            Self::Tools => "Tools",
            Self::AgentIsolation => "Agent isolation",
            Self::Project => "Project",
        }
    }

    /// Its name in the JSON report.
    pub fn id(self) -> &'static str {
        match self {
            Self::Tools => "tools",
            Self::AgentIsolation => "agent_isolation",
            Self::Project => "project",
        }
    }
}

/// One step of a fix.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Step {
    /// A command to copy and run as is: one line, with what to expect or
    /// what to do alongside it.
    Run {
        command: String,
        note: Option<String>,
    },
    /// Something to do that is not one command.
    Do(String),
}

impl Step {
    fn run(command: &str) -> Self {
        Self::Run {
            command: command.to_owned(),
            note: None,
        }
    }

    fn run_noting(command: &str, note: &str) -> Self {
        Self::Run {
            command: command.to_owned(),
            note: Some(note.to_owned()),
        }
    }

    fn act(text: impl Into<String>) -> Self {
        Self::Do(text.into())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Check {
    pub section: Section,
    pub subject: String,
    pub status: Status,
    pub detail: String,
    /// A failure: what breaks and why the check exists. A warning: why it
    /// does not block `owlshift do`, and what to do about it if anything.
    pub why: Option<String>,
    /// A failure: the steps that fix it, in order. Rendering adds a last
    /// one, running `owlshift doctor` again.
    pub fix: Vec<Step>,
}

impl Check {
    fn ok(section: Section, subject: &str, detail: impl Into<String>) -> Self {
        Self::new(section, subject, Status::Ok, detail)
    }

    fn info(section: Section, subject: &str, detail: impl Into<String>) -> Self {
        Self::new(section, subject, Status::Info, detail)
    }

    fn warn(section: Section, subject: &str, detail: impl Into<String>, why: String) -> Self {
        Self {
            why: Some(why),
            ..Self::new(section, subject, Status::Warn, detail)
        }
    }

    /// A failure always says why it matters and how to fix it.
    fn fail(
        section: Section,
        subject: &str,
        detail: impl Into<String>,
        why: &str,
        fix: Vec<Step>,
    ) -> Self {
        debug_assert!(!fix.is_empty(), "a failure without a fix: {subject}");
        Self {
            why: Some(why.to_owned()),
            fix,
            ..Self::new(section, subject, Status::Fail, detail)
        }
    }

    fn new(section: Section, subject: &str, status: Status, detail: impl Into<String>) -> Self {
        Self {
            section,
            subject: subject.to_owned(),
            status,
            detail: detail.into(),
            why: None,
            fix: Vec::new(),
        }
    }
}

/// What to run once the machine is ready.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Next {
    /// The project has its `owlshift.toml`: `owlshift do TICKET`.
    Do,
    /// The repository has no `owlshift.toml` yet: `owlshift init`, which
    /// also stores the secrets `owlshift do` needs.
    Init,
    /// Not in a git repository: go to the project's first.
    FromRepository,
}

impl Next {
    /// Its name in the JSON report.
    pub fn id(self) -> &'static str {
        match self {
            Self::Do => "do",
            Self::Init => "init",
            Self::FromRepository => "from_repository",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    pub checks: Vec<Check>,
    pub next: Next,
}

impl Report {
    /// Ready when no check failed; warnings do not count.
    pub fn ready(&self) -> bool {
        self.failures() == 0
    }

    /// How many checks failed.
    pub fn failures(&self) -> usize {
        self.checks
            .iter()
            .filter(|check| check.status == Status::Fail)
            .count()
    }
}

/// The note of every warning: none blocks `owlshift do`.
const DOES_NOT_BLOCK: &str = "Does not block `owlshift do`:";

/// Runs every check against the host and the loaded configuration.
pub fn run(system: &dyn System, config: &Effective) -> Report {
    let home = system.home();
    let home = home.as_deref();
    let mut checks = vec![git(system, home)];
    let harnesses = required_harnesses(config);
    checks.extend(
        harnesses
            .iter()
            .map(|harness| harness_check(system, *harness, home)),
    );
    checks.push(sandbox_check(system, home));
    #[cfg(unix)]
    checks.push(sentinel_check(&system.sentinel()));
    #[cfg(unix)]
    checks.push(sentinel_probe_check(&system.sentinel_probe()));
    // A missing `claude` fails its own line: its agent login is not asked.
    if harnesses.contains(&Harness::Claude) && system.locate("claude").is_some() {
        checks.push(agent_login_check(system));
    }
    if harnesses.contains(&Harness::Codex) && system.locate("codex").is_some() {
        checks.push(codex_machine_config_check(system));
        checks.extend(codex_requirements_check(system));
    }
    checks.push(project_config_check(system, &config.project, home));
    checks.push(file_check("personal config", &config.personal, home));
    checks.push(tracker_check(config));
    let stored = project_secrets(system, config);
    checks.extend(stored.as_ref().map(secrets_check));
    checks.extend(
        stored
            .as_ref()
            .and_then(|found| forge_access_check(system, found)),
    );
    let accounts = match &stored {
        Some(Ok(found)) => found.tracker.as_ref().map(|tracker| &tracker.accounts),
        _ => None,
    };
    checks.extend(tracker_states_check(system, config, accounts));
    checks.extend(tracker_identity_check(system, config, accounts));
    let facts = admission_facts(system, config, accounts);
    checks.extend(drain_admission_check(config, facts.as_ref()));
    checks.extend(proposals_inbox_check(config, facts.as_ref()));
    checks.extend(forge_reviewers_check(config));
    checks.extend(resolver_check(config));
    checks.extend(codex_reviewer_check(system, config));
    checks.extend(budget_check(config));
    checks.extend(drain_slot_check(system, config));
    checks.extend(quarantine_check(system, config, home));
    checks.push(data_dir_check(system, home));
    let next = match &config.project {
        FileState::Absent(_) => Next::Init,
        FileState::NotApplicable(_) => Next::FromRepository,
        FileState::Loaded { .. } | FileState::Unavailable(_) | FileState::Invalid { .. } => {
            Next::Do
        }
    };
    Report { checks, next }
}

/// `path`, with the home folder it is in written `~`. Whole components
/// only: `/home/adam` is not in `/home/ada`.
fn shown(path: &Path, home: Option<&Path>) -> String {
    let relative = home
        .filter(|home| home.is_absolute())
        .and_then(|home| path.strip_prefix(home).ok());
    match relative {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_owned(),
        Some(rest) => format!("~{MAIN_SEPARATOR}{}", rest.display()),
        None => path.display().to_string(),
    }
}

fn git(system: &dyn System, home: Option<&Path>) -> Check {
    const SUBJECT: &str = "git";
    const WHY: &str = "Owlshift runs every ticket in a git worktree and pushes its branch: nothing \
                       runs without git.";
    let install = || Step::act("Install git: https://git-scm.com/downloads");
    let see_why = || Step::run_noting("git --version", "to see why it fails");
    let Some(path) = system.locate("git") else {
        return Check::fail(
            Section::Tools,
            SUBJECT,
            "not found on the PATH",
            WHY,
            vec![install()],
        );
    };
    match system.run(&path, &["--version"], None) {
        Ok(out) if out.code == Some(0) => match version_of(&out.stdout)
            .and_then(|version| Some((older_than(&version, MINIMUM_GIT_VERSION)?, version)))
        {
            Some((false, version)) => Check::ok(
                Section::Tools,
                SUBJECT,
                format!("{version} ({})", shown(&path, home)),
            ),
            Some((true, version)) => Check::fail(
                Section::Tools,
                SUBJECT,
                format!(
                    "{version} ({}), older than {MINIMUM_GIT_VERSION}",
                    shown(&path, home)
                ),
                &git_floor_reason(),
                vec![
                    Step::act(git_install_advice()),
                    Step::run_noting("git --version", &git_recheck_note()),
                ],
            ),
            None => Check::warn(
                Section::Tools,
                SUBJECT,
                format!("unfamiliar `git --version` answer ({})", shown(&path, home)),
                format!("{DOES_NOT_BLOCK} git runs, only its version is unknown."),
            ),
        },
        Ok(out) => Check::fail(
            Section::Tools,
            SUBJECT,
            format!("`git --version` failed ({})", exit_text(out.code)),
            WHY,
            vec![see_why(), install()],
        ),
        Err(error) => Check::fail(
            Section::Tools,
            SUBJECT,
            format!("`git --version`: {error}"),
            WHY,
            vec![see_why(), install()],
        ),
    }
}

/// Whether agent runs can be confined (OWL-41): without it, `owlshift do`
/// refuses to start one. The fix is the sandbox's own: install bwrap, allow
/// it user namespaces, or use WSL2.
fn sandbox_check(system: &dyn System, home: Option<&Path>) -> Check {
    const SUBJECT: &str = "sandbox";
    const WHY: &str = "`owlshift do` does not start an agent it cannot confine: the sandbox keeps \
                       agent runs out of your home folder and away from your credentials.";
    match system.sandbox() {
        Ok(()) => Check::ok(
            Section::AgentIsolation,
            SUBJECT,
            if cfg!(target_os = "macos") {
                "agent runs are confined with sandbox-exec"
            } else {
                "agent runs are confined with bwrap"
            },
        ),
        Err(error) => {
            let (detail, fix) = sandbox_fix(&error, home);
            Check::fail(Section::AgentIsolation, SUBJECT, detail, WHY, fix)
        }
    }
}

/// What is wrong with the sandbox, and the steps that fix it, by program:
/// sandbox-exec on macOS, bwrap on Linux.
fn sandbox_fix(error: &SandboxError, home: Option<&Path>) -> (String, Vec<Step>) {
    match error {
        SandboxError::Unsupported => (
            "this system has no sandbox Owlshift can use (sandbox-exec on macOS, bwrap on Linux)"
                .to_owned(),
            vec![Step::act(
                "On Windows, run Owlshift under WSL2, where it uses bwrap: \
                 https://learn.microsoft.com/windows/wsl/install",
            )],
        ),
        SandboxError::Missing { program: "bwrap" } => (
            "bwrap is not installed".to_owned(),
            vec![Step::run_noting(
                "sudo apt install bubblewrap",
                "or your distribution's bubblewrap package",
            )],
        ),
        SandboxError::Missing {
            program: "sandbox-exec",
        } => (
            "/usr/bin/sandbox-exec is missing".to_owned(),
            vec![Step::act(
                "sandbox-exec ships with macOS: restore /usr/bin/sandbox-exec by updating or \
                 reinstalling macOS",
            )],
        ),
        SandboxError::Missing { program } => (
            format!("{program} is not installed"),
            vec![Step::act(format!("Install {program}"))],
        ),
        SandboxError::Blocked {
            program: "bwrap",
            reason,
        } => (
            format!("bwrap cannot confine a trial run: {reason}"),
            vec![
                Step::run_noting(
                    "bwrap --unshare-pid --ro-bind / / --dev /dev --proc /proc -- true",
                    "to see the error",
                ),
                Step::act(
                    "On Ubuntu 23.10 and later, AppArmor keeps unprivileged programs from \
                     creating user namespaces: the next two steps allow them for bwrap alone",
                ),
                Step::run(&apparmor_profile_command()),
                Step::run("sudo apparmor_parser -r /etc/apparmor.d/bwrap"),
            ],
        ),
        SandboxError::Blocked {
            program: "sandbox-exec",
            reason,
        } => (
            format!("sandbox-exec cannot confine a trial run: {reason}"),
            vec![
                Step::run_noting(
                    "/usr/bin/sandbox-exec -p '(version 1)(allow default)' -- /usr/bin/true",
                    "to see the error",
                ),
                Step::act(
                    "Look for what keeps sandbox-exec from running on this Mac, such as a \
                     security tool or a device management profile",
                ),
            ],
        ),
        SandboxError::Blocked { program, reason } => (
            format!("{program} cannot confine a trial run: {reason}"),
            vec![Step::act(format!(
                "Look for what keeps {program} from running"
            ))],
        ),
        SandboxError::Path(path) => (
            format!(
                "{} cannot be given to the sandbox: it is not valid UTF-8",
                shown(path, home)
            ),
            vec![Step::act(
                "Move the project to a folder whose path is valid UTF-8",
            )],
        ),
    }
}

/// One line that writes the bwrap AppArmor profile, as root, to
/// `/etc/apparmor.d/bwrap`: `printf` gives each line of the profile, single
/// quoted, which holds as long as the profile has no single quote.
fn apparmor_profile_command() -> String {
    format!(
        "{} | sudo tee /etc/apparmor.d/bwrap >/dev/null",
        apparmor_profile_printf()
    )
}

fn apparmor_profile_printf() -> String {
    let lines: Vec<String> = BWRAP_APPARMOR_PROFILE
        .lines()
        .map(|line| format!("'{line}'"))
        .collect();
    format!("printf '%s\\n' {}", lines.join(" "))
}

/// Whether the sentinel of this very command runs, the process that stops
/// the probes and agent runs Owlshift started when Owlshift is killed
/// outright (OWL-86). A warning, never a failure: it is a best effort, not a
/// guardrail, and it is never restarted (OWL-88). A stopped one warns too
/// (OWL-91), though it still stops the live trees at Owlshift's end, with
/// the exceptions its warning names (OWL-93, OWL-95).
#[cfg(unix)]
fn sentinel_check(status: &SentinelStatus) -> Check {
    const SUBJECT: &str = "sentinel";
    const UNPROTECTED: &str =
        "a hard kill of Owlshift would leave the processes it started running";
    let why = |what: &str| {
        format!("{DOES_NOT_BLOCK} the sentinel is a best effort, not a guardrail. {what}")
    };
    match status {
        SentinelStatus::Running { pid } => Check::ok(
            Section::AgentIsolation,
            SUBJECT,
            format!(
                "running (pid {pid}): a hard kill of Owlshift stops, best effort, the processes it started"
            ),
        ),
        SentinelStatus::Stopped { pid } => Check::warn(
            Section::AgentIsolation,
            SUBJECT,
            format!(
                "stopped (pid {pid}), it reads nothing until continued: the system continues it \
                 at Owlshift's end, a hard kill included, and it then stops, best effort, the \
                 processes Owlshift started, unless its input fills first, in which case \
                 Owlshift kills it and it stops nothing"
            ),
            why(
                "It also stops nothing if it was stopped in its first milliseconds, before it \
                 ignores SIGHUP, or, on Linux, if Owlshift's end leaves it to a subreaper in the \
                 same session, where it stays stopped until something continues it. To restore \
                 it, look for what stops the `/bin/sh` process whose command line ends in \
                 `owlshift-sentinel`.",
            ),
        ),
        SentinelStatus::Ended(how) => Check::warn(
            Section::AgentIsolation,
            SUBJECT,
            format!("ended ({how}): {UNPROTECTED}"),
            why(
                "To find out why, look for what ends the `/bin/sh` process whose command line \
                 ends in `owlshift-sentinel`.",
            ),
        ),
        SentinelStatus::NotRunning => Check::warn(
            Section::AgentIsolation,
            SUBJECT,
            format!("not running, it could not start: {UNPROTECTED}"),
            why(
                "Check that `/bin/sh` runs; Owlshift printed why the sentinel could not start \
                 on its standard error as it started.",
            ),
        ),
    }
}

/// Whether a sentinel works on this host, not only runs: a test sentinel,
/// told of a test process group, stops it when its input ends (OWL-90).
/// Independent of the `sentinel` line, and, like it, a warning at worst.
#[cfg(unix)]
fn sentinel_probe_check(probe: &SentinelProbe) -> Check {
    const SUBJECT: &str = "sentinel test";
    const UNPROTECTED: &str = "a hard kill of Owlshift may leave the processes it started running";
    let why = |what: &str| {
        format!("{DOES_NOT_BLOCK} the sentinel is a best effort, not a guardrail. {what}")
    };
    match probe {
        SentinelProbe::Works { elapsed } => Check::ok(
            Section::AgentIsolation,
            SUBJECT,
            format!(
                "a test sentinel stopped a test process group {} ms after its input ended",
                elapsed.as_millis()
            ),
        ),
        SentinelProbe::CannotStart(reason) => Check::warn(
            Section::AgentIsolation,
            SUBJECT,
            format!("could not start {reason}: {UNPROTECTED}"),
            why("Check that `/bin/sh` runs."),
        ),
        SentinelProbe::Fails(reason) => Check::warn(
            Section::AgentIsolation,
            SUBJECT,
            format!("{reason}: {UNPROTECTED}"),
            why(
                "Check that `/bin/sh` is a POSIX shell whose `kill -s KILL -- -<group>` stops a \
                 process group.",
            ),
        ),
    }
}

/// The commands that give agent runs their login, when none is stored:
/// those `executor::harness::AGENT_LOGIN_FIX` names too.
const AGENT_LOGIN_COMMANDS: [&str; 2] = ["claude setup-token", "owlshift init"];

/// Whether agent runs have their Claude Code login: a token made by
/// `claude setup-token`, in the system keychain (OWL-94). Confined, agent
/// runs cannot reach the Keychain, where the operator's own login lives.
/// Only the token's presence is asked, never its value; whether it still
/// works shows at the first run.
fn agent_login_check(system: &dyn System) -> Check {
    const SUBJECT: &str = "claude agent login";
    let place =
        format!("in the system keychain (service `{SERVICE}`, account `{CLAUDE_AGENT_ACCOUNT}`)");
    match system.secret_stored(CLAUDE_AGENT_ACCOUNT) {
        Ok(true) => Check::ok(
            Section::AgentIsolation,
            SUBJECT,
            format!("a token for agent runs is stored {place}"),
        ),
        Ok(false) => Check::fail(
            Section::AgentIsolation,
            SUBJECT,
            format!("no token for agent runs {place}"),
            "Agent runs are confined and cannot reach the Keychain, where your own Claude Code \
             login lives, so they log in with a long-lived token of your subscription, which \
             Owlshift keeps in the system keychain and hands to each run.",
            vec![
                Step::run_noting(AGENT_LOGIN_COMMANDS[0], "prints a token"),
                Step::run_noting(AGENT_LOGIN_COMMANDS[1], "paste the token when asked"),
            ],
        ),
        Err(error) => Check::fail(
            Section::AgentIsolation,
            SUBJECT,
            format!("could not tell whether a token for agent runs is stored {place}: {error}"),
            "Owlshift keeps the agent runs' Claude Code token in the system keychain, and \
             `owlshift do` reads it there before each run.",
            vec![Step::act(KEYCHAIN_FIX)],
        ),
    }
}

/// The MCP servers and hooks the machine's Codex configuration declares
/// (OWL-314). They are its administrator's, accepted as Claude Code's
/// managed settings are (`docs/design/runtime-and-operations.md`, "Codex
/// routing"), so only a warning: they reach every Codex agent run whatever
/// the adapter's flags. Each file is read on its own, and every finding and
/// every unreadable file is named.
fn codex_machine_config_check(system: &dyn System) -> Check {
    const SUBJECT: &str = "codex machine config";
    let mut declared = Vec::new();
    let mut unreadable = Vec::new();
    for path in [codex_machine::CONFIG, codex_machine::MANAGED_CONFIG] {
        match system.read_text_file(Path::new(path)) {
            Ok(None) => {}
            Ok(Some(text)) => match codex_machine::declared(&text) {
                Ok(found) if found.is_empty() => {}
                Ok(found) => declared.push(format!("{path} declares {}", declarations(&found))),
                Err(error) => unreadable.push(format!("{path} {error}")),
            },
            Err(error) => unreadable.push(format!("{path} could not be read: {error}")),
        }
    }
    if declared.is_empty() && unreadable.is_empty() {
        return Check::ok(
            Section::AgentIsolation,
            SUBJECT,
            format!(
                "no MCP server or hook in {} or {}",
                codex_machine::CONFIG,
                codex_machine::MANAGED_CONFIG
            ),
        );
    }
    let mut why = DOES_NOT_BLOCK.to_owned();
    if !declared.is_empty() {
        why.push_str(
            " the machine's Codex configuration is its administrator's, accepted as Claude \
             Code's managed settings are: its MCP servers start, and its hooks run with no trust \
             prompt, in every Codex agent run, whatever Owlshift's flags. Remove them from the \
             file if agent runs should not have them.",
        );
    }
    if !unreadable.is_empty() {
        why.push_str(
            " Codex reads these files in every run, so what an unreadable one declares may \
             reach every Codex agent run too.",
        );
    }
    Check::warn(
        Section::AgentIsolation,
        SUBJECT,
        [declared, unreadable].concat().join("; "),
        why,
    )
}

/// `found`'s servers and hooks, each name escaped.
fn declarations(found: &Declared) -> String {
    let names = |names: &[String]| {
        names
            .iter()
            .map(|name| format!("`{}`", name.escape_debug()))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut parts = Vec::new();
    if !found.mcp_servers.is_empty() {
        parts.push(format!("MCP servers {}", names(&found.mcp_servers)));
    }
    if !found.hooks.is_empty() {
        parts.push(format!("hooks on {}", names(&found.hooks)));
    }
    parts.join(" and ")
}

/// Whether the machine's Codex requirements let a Codex agent run start
/// (OWL-314): one that refuses [`FULL_ACCESS`] stops every run, so Codex is
/// not ready, which [`codex_not_ready_is_a_warning`] makes its warning.
/// `None` when the machine has no requirements file.
fn codex_requirements_check(system: &dyn System) -> Option<Check> {
    const SUBJECT: &str = "codex requirements";
    let path = codex_machine::REQUIREMENTS;
    let unknown = |detail: String| {
        Check::warn(
            Section::AgentIsolation,
            SUBJECT,
            detail,
            format!(
                "{DOES_NOT_BLOCK} Codex reads this file before every run; Owlshift could not \
                 tell whether it lets Codex agent runs start, which needs `{FULL_ACCESS}` in its \
                 `allowed_sandbox_modes` if it has one."
            ),
        )
    };
    let text = match system.read_text_file(Path::new(path)) {
        Ok(None) => return None,
        Ok(Some(text)) => text,
        Err(error) => return Some(unknown(format!("{path} could not be read: {error}"))),
    };
    let required = match codex_machine::requirements(&text) {
        Ok(required) => required,
        Err(error) => return Some(unknown(format!("{path} {error}"))),
    };
    let hooks = required
        .managed_hooks_dir
        .as_deref()
        .map(|dir| format!("; a managed hooks folder, `{}`", dir.escape_debug()))
        .unwrap_or_default();
    if required.refuses_full_access() {
        let allowed = match required
            .allowed_sandbox_modes
            .as_deref()
            .unwrap_or_default()
        {
            [] => "no sandbox mode".to_owned(),
            modes => format!(
                "only {}",
                modes
                    .iter()
                    .map(|mode| format!("`{mode}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        };
        let refused = Check::fail(
            Section::AgentIsolation,
            SUBJECT,
            format!(
                "{path} allows {allowed}, and a Codex agent run needs `{FULL_ACCESS}`, Owlshift \
                 confining it in Codex's stead: every Codex agent run stops before it \
                 starts{hooks}"
            ),
            harness_why(Harness::Codex),
            vec![Step::act(format!(
                "Have the machine's administrator add \"{FULL_ACCESS}\" to \
                 `allowed_sandbox_modes` in {path}"
            ))],
        );
        return Some(codex_not_ready_is_a_warning(refused, Harness::Codex));
    }
    let allows = match required.allowed_sandbox_modes {
        Some(_) => format!("{path} allows `{FULL_ACCESS}`"),
        None => format!("{path} sets no `allowed_sandbox_modes`"),
    };
    Some(if hooks.is_empty() {
        Check::ok(Section::AgentIsolation, SUBJECT, allows)
    } else {
        Check::warn(
            Section::AgentIsolation,
            SUBJECT,
            format!("{allows}{hooks}"),
            format!(
                "{DOES_NOT_BLOCK} the hooks of a managed hooks folder are the machine \
                 administrator's, as its configuration's are; whether they run in a Codex agent \
                 run was not tried, so take them to run in every one."
            ),
        )
    })
}

/// The step that fixes a keychain doctor cannot read.
const KEYCHAIN_FIX: &str = "Make the system keychain available: unlock it on macOS; on Linux, \
                            start a Secret Service such as GNOME Keyring";

/// The harnesses doctor checks (scenario S15): Claude Code, which runs every
/// role, and Codex only when the personal file declares it (OWL-347) or the
/// project adds the opt-in Codex reviewer (OWL-348). Codex that is not ready
/// is only a warning: see [`codex_not_ready_is_a_warning`].
fn required_harnesses(config: &Effective) -> Vec<Harness> {
    let codex = matches!(
        &config.personal,
        FileState::Loaded { config, .. } if config.harnesses.codex.is_some()
    ) || codex_reviewer_asked(config);
    let mut harnesses = vec![Harness::Claude];
    if codex {
        harnesses.push(Harness::Codex);
    }
    harnesses
}

/// Why a harness must be ready at all.
fn harness_why(harness: Harness) -> &'static str {
    match harness {
        Harness::Claude => {
            "`owlshift do` runs every agent in Claude Code, so it must be installed. \
             Agent runs log in with the token the `claude agent login` line checks."
        }
        Harness::Codex => {
            "No role needs Codex: every agent runs in Claude Code. With \
             `pipeline.codex_reviewer` on, each review pass adds a Codex reviewer while Codex \
             is ready, and runs without it otherwise."
        }
    }
}

fn harness_check(system: &dyn System, harness: Harness, home: Option<&Path>) -> Check {
    let program = harness::program(harness);
    let Some(path) = system.locate(program) else {
        let fix = vec![match harness::install_command(harness) {
            Some(command) => {
                Step::run_noting(command, &format!("see {}", harness::install_page(harness)))
            }
            None => Step::act(format!(
                "Install {}: {}",
                harness_name(harness),
                harness::install_page(harness)
            )),
        }];
        let missing = Check::fail(
            Section::Tools,
            program,
            "not found on the PATH",
            harness_why(harness),
            fix,
        );
        return codex_not_ready_is_a_warning(missing, harness);
    };
    let (version, exact) = match system.run(&path, &["--version"], None) {
        Ok(out) if out.code == Some(0) => (version_of(&out.stdout), exact_version_of(&out.stdout)),
        _ => (None, None),
    };
    let found = format!(
        "{} ({})",
        version.as_deref().unwrap_or("version unknown"),
        shown(&path, home)
    );
    let mut result = login_check(system, harness, &path, &found);
    if harness == Harness::Codex && result.status == Status::Ok {
        result = codex_login_file_check(system, result, home);
    }
    flag_untested(&mut result, harness, exact.as_deref());
    codex_not_ready_is_a_warning(result, harness)
}

/// Codex that is missing, logged out, unreadable or stopped by the machine's
/// requirements ([`codex_requirements_check`], OWL-314) is a warning, never a
/// failure: no role needs Codex (architecture section 5, "Codex", OWL-347),
/// and the opt-in Codex reviewer only drops its seat (OWL-348), so it is not
/// a problem to fix before `do` (OWL-102, OWL-299). The decision is made
/// here and only here.
fn codex_not_ready_is_a_warning(mut check: Check, harness: Harness) -> Check {
    if harness != Harness::Codex || check.status != Status::Fail {
        return check;
    }
    let steps: Vec<String> = check
        .fix
        .iter()
        .map(|step| match step {
            Step::Run {
                command,
                note: Some(note),
            } => format!("`{command}` ({note})"),
            Step::Run {
                command,
                note: None,
            } => format!("`{command}`"),
            Step::Do(text) => text.clone(),
        })
        .collect();
    check.status = Status::Warn;
    check.why = Some(format!(
        "{DOES_NOT_BLOCK} no role needs Codex. To get Codex ready: {}.",
        steps.join(", then ")
    ));
    check.fix = Vec::new();
    check
}

/// A Codex logged in, but not through the file a Codex agent run copies its
/// login from (OWL-299), such as a login kept in the system keyring: `codex
/// login status` says logged in, yet the run is refused. Not ready, so
/// [`codex_not_ready_is_a_warning`] makes it its warning.
fn codex_login_file_check(system: &dyn System, logged_in: Check, home: Option<&Path>) -> Check {
    let file = system.codex_login_file();
    if file.as_deref().is_some_and(|file| system.is_file(file)) {
        return logged_in;
    }
    let where_ = file.map_or_else(
        || "no login file (neither CODEX_HOME nor a home folder is set)".to_owned(),
        |file| format!("no login file at {}", shown(&file, home)),
    );
    Check::fail(
        Section::Tools,
        "codex",
        format!("{}, but {where_}", logged_in.detail),
        harness_why(Harness::Codex),
        vec![Step::run_noting(
            harness::login_command(Harness::Codex),
            "with Codex set to keep its login in auth.json, which Owlshift copies into each \
             Codex run",
        )],
    )
}

fn harness_name(harness: Harness) -> &'static str {
    match harness {
        Harness::Claude => "Claude Code",
        Harness::Codex => "Codex",
    }
}

fn login_check(system: &dyn System, harness: Harness, path: &Path, found: &str) -> Check {
    let program = harness::program(harness);
    let login_command = harness::login_command(harness);

    let status_args = harness::login_status_args(harness);
    let status_command = format!("{program} {}", status_args.join(" "));
    let (login, why) = match system.run(path, status_args, None) {
        Ok(out) => (
            harness::parse_login(harness, out.code, &out.stdout, &out.stderr),
            "gave an answer Owlshift does not know",
        ),
        Err(RunError::TimedOut) => (Login::Unknown, "did not answer in time"),
        Err(RunError::Io(_)) => (Login::Unknown, "could not be run"),
    };
    // Agent runs log in with the token `claude agent login` checks (OWL-94),
    // never with the operator's own Claude Code login: its state is only
    // information (OWL-97).
    if harness == Harness::Claude {
        const UNUSED: &str = "(agent runs do not use it)";
        return match login {
            Login::LoggedIn { method, plan } => {
                let plan = plan.map(|p| format!(", {p} plan")).unwrap_or_default();
                Check::ok(
                    Section::Tools,
                    program,
                    format!("{found}, your own login: logged in ({method}{plan})"),
                )
            }
            Login::LoggedOut => Check::info(
                Section::Tools,
                program,
                format!("{found}, your own login: not logged in {UNUSED}"),
            ),
            Login::Unknown => Check::info(
                Section::Tools,
                program,
                format!("{found}, your own login: unknown, `{status_command}` {why} {UNUSED}"),
            ),
        };
    }
    match login {
        Login::LoggedIn { method, plan } => {
            let plan = plan.map(|p| format!(", {p} plan")).unwrap_or_default();
            Check::ok(
                Section::Tools,
                program,
                format!("{found}, logged in ({method}{plan})"),
            )
        }
        Login::LoggedOut => Check::fail(
            Section::Tools,
            program,
            format!("{found}, not logged in"),
            harness_why(harness),
            vec![Step::run(login_command)],
        ),
        Login::Unknown => Check::fail(
            Section::Tools,
            program,
            format!("{found}, login state unknown: `{status_command}` {why}"),
            harness_why(harness),
            vec![
                Step::run_noting(&status_command, "to see why"),
                Step::run_noting(login_command, "if it says you are not logged in"),
            ],
        ),
    }
}

/// Warns on a harness version Owlshift was not tested with, or could not
/// read: the CLI may have changed a flag or a message under the user
/// (`docs/design/runtime-and-operations.md`, "Updates & versions"). Never a
/// failure, since an untested version may well work; a failed check keeps
/// its status and its fix, and an informational one becomes a warning.
fn flag_untested(check: &mut Check, harness: Harness, version: Option<&str>) {
    if version.is_some_and(|version| tested::is_tested(harness, version)) {
        return;
    }
    let tested = tested::tested_versions(harness);
    if tested.is_empty() {
        check
            .detail
            .push_str("; no version tested with Owlshift yet");
    } else {
        check.detail.push_str("; version not tested with Owlshift");
    }
    if matches!(check.status, Status::Ok | Status::Info) {
        check.status = Status::Warn;
        let mut why = format!("{DOES_NOT_BLOCK} an untested version usually works.");
        if !tested.is_empty() {
            why.push_str(&format!(
                " If a run misbehaves, install a tested version: {} {}.",
                harness::program(harness),
                tested.join(", ")
            ));
        }
        check.why = Some(why);
    }
}

fn file_check<T>(subject: &str, state: &FileState<T>, home: Option<&Path>) -> Check {
    match state {
        FileState::Loaded { path, .. } => Check::ok(Section::Project, subject, shown(path, home)),
        FileState::Absent(path) => Check::info(
            Section::Project,
            subject,
            format!("not found at {}", shown(path, home)),
        ),
        FileState::NotApplicable(reason) => {
            Check::info(Section::Project, subject, format!("none ({reason})"))
        }
        FileState::Unavailable(reason) => Check::fail(
            Section::Project,
            subject,
            reason.clone(),
            "Owlshift asks git for the root of the repository you are in, where the project's \
             owlshift.toml lives.",
            vec![Step::run_noting(
                "git rev-parse --show-toplevel",
                "should print the repository's root",
            )],
        ),
        FileState::Invalid { path, error } => Check::fail(
            Section::Project,
            subject,
            format!("{}: {error}", shown(path, home)),
            "`owlshift do` does not start on a configuration it cannot read or accept.",
            vec![Step::act(format!(
                "Do what the error above says about {}",
                shown(path, home)
            ))],
        ),
    }
}

/// The step every project config failure ends with: doctor reads the
/// default branch as the last fetch left it (OWL-332).
fn fetch_step() -> Step {
    Step::run_noting(
        "git fetch origin",
        "doctor reads the default branch as your last fetch left it",
    )
}

/// The project file, and the gate `owlshift do` would run (OWL-332): the
/// `stack.gate` of the file at the default branch, as the last fetch of the
/// person's checkout left `origin/HEAD`, or of the working tree's file when
/// that commit has none or the default branch is unknown here, as `do`
/// falls back to it. An empty gate fails, since `do` refuses a ticket then,
/// and so does a file at the default branch `do` would refuse. A file that
/// is not loaded is reported as [`file_check`] reports it.
fn project_config_check(
    system: &dyn System,
    state: &FileState<ProjectConfig>,
    home: Option<&Path>,
) -> Check {
    const SUBJECT: &str = "project config";
    let FileState::Loaded { path, config, .. } = state else {
        return file_check(SUBJECT, state, home);
    };
    let local = shown(path, home);
    let read = match path.parent() {
        Some(root) => system.default_branch_file(root),
        None => Ok(DefaultBranchFile::Unknown(
            "the file has no folder".to_owned(),
        )),
    };
    let short = |commit: &str| commit.get(..12).unwrap_or(commit).to_owned();
    let (gate, source) = match &read {
        Err(error) => {
            return Check::fail(
                Section::Project,
                SUBJECT,
                format!("{local}; at the default branch: {error}"),
                "`owlshift do` reads owlshift.toml at the default branch and does not start on \
                 one it cannot read or accept.",
                vec![
                    Step::act(
                        "Do what the error above says about owlshift.toml, then merge it to the \
                         default branch",
                    ),
                    fetch_step(),
                ],
            );
        }
        Ok(DefaultBranchFile::Read {
            base,
            file: Some(file),
        }) => (
            &file.stack.gate,
            format!(
                "owlshift.toml at {} ({})",
                base.remote_ref,
                short(&base.commit)
            ),
        ),
        Ok(DefaultBranchFile::Read { base, file: None }) => (
            &config.stack.gate,
            format!(
                "{local}, {} ({}) holding none",
                base.remote_ref,
                short(&base.commit)
            ),
        ),
        Ok(DefaultBranchFile::Unknown(reason)) => (
            &config.stack.gate,
            format!(
                "{local}, the default branch being unknown here ({reason}; `git remote set-head \
                 origin --auto` sets it)"
            ),
        ),
    };
    if gate.is_empty() {
        return Check::fail(
            Section::Project,
            SUBJECT,
            format!("`stack.gate` is empty in {source}"),
            "`owlshift do` does not start a ticket when the project declares no gate: the build \
             role would stop `blocked` after its whole plan.",
            vec![
                Step::act(
                    "List the project's lint, formatter and test commands under `stack.gate` in \
                     owlshift.toml, and merge it to the default branch",
                ),
                fetch_step(),
            ],
        );
    }
    Check::ok(
        Section::Project,
        SUBJECT,
        format!("{local}; gate of {} command(s) from {source}", gate.len()),
    )
}

/// The configured tracker's adapter and the capabilities it declares
/// (architecture section 6). A required capability this build does not
/// implement yet is a warning, not a failure: `owlshift do` needs only to
/// read tickets and comments and move the visible stage, and refusing a project is `init`'s job. The
/// check reads the adapter's constants: it opens neither the tracker nor the
/// keychain; [`tracker_states_check`] does, for a Linear project.
fn tracker_check(config: &Effective) -> Check {
    const SUBJECT: &str = "tracker";
    let FileState::Loaded { config, .. } = &config.project else {
        return Check::info(
            Section::Project,
            SUBJECT,
            "no project configuration, no adapter to check",
        );
    };
    let (kind, declared) = match config.tracker.kind {
        TrackerKind::Linear => ("linear", LinearTracker::CAPABILITIES),
        TrackerKind::Markdown => ("markdown", MarkdownTracker::CAPABILITIES),
    };
    let list = |capabilities: &[Capability]| {
        capabilities
            .iter()
            .map(|c| c.describe())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let missing: Vec<Capability> = Capability::ALL
        .into_iter()
        .filter(|c| c.required() && !declared.contains(c))
        .collect();
    let detail = format!("`{kind}`: {}", list(declared));
    if missing.is_empty() {
        Check::ok(Section::Project, SUBJECT, detail)
    } else {
        Check::warn(
            Section::Project,
            SUBJECT,
            format!("{detail}; not built yet: {}", list(&missing)),
            format!(
                "{DOES_NOT_BLOCK} it only reads the ticket, posts comments and moves the stage. The rest comes \
                 with later roadmap steps."
            ),
        )
    }
}

/// Whose reviews on Owlshift's pull requests are read (OWL-307, build plan
/// "P5 · Quality pipeline", "Contracts and formats"): the logins
/// `[forge].reviewers` lists, read from the working tree here, while watch
/// reads the base commit. Without `[forge]` no review is read, which blocks
/// nothing: a warning. None without a project configuration.
fn forge_reviewers_check(config: &Effective) -> Option<Check> {
    const SUBJECT: &str = "forge reviewers";
    let FileState::Loaded { config, .. } = &config.project else {
        return None;
    };
    Some(match &config.forge {
        Some(forge) => Check::ok(
            Section::Project,
            SUBJECT,
            format!(
                "{} reviewer(s) in the working tree (watch reads the base commit): a review of \
                 the ticket's decider on a delivered pull request goes back to Build",
                forge.reviewers.len()
            ),
        ),
        None => Check::warn(
            Section::Project,
            SUBJECT,
            "`[forge].reviewers` is not set: no review on Owlshift's pull requests is read",
            format!(
                "{DOES_NOT_BLOCK} to have the decider's reviews on a delivered pull request \
                 handled, map each forge login to its tracker account under `[forge]`, as \
                 `reviewers = {{ login = \"account\" }}`, written as a zone's owner is."
            ),
        ),
    })
}

/// What the project lets the resolver decide without its decider (OWL-328,
/// architecture section 4, "Delegation level" and "Resolver passes"): the
/// level, the pass limit and the ceiling on the resolver runs of one chain
/// (OWL-376), read from the project file `owlshift do` reads. A note: none
/// blocks `do`. None without a project configuration.
fn resolver_check(config: &Effective) -> Option<Check> {
    let FileState::Loaded { config, .. } = &config.project else {
        return None;
    };
    let resolver = config.policy.resolver();
    let level = match resolver.level {
        DelegationLevel::None => "delegation none: every question goes to the decider".to_owned(),
        level => format!("delegation {}", level.as_str()),
    };
    let passes = match resolver.passes {
        PassLimit::Runs(runs) => format!("{runs} run(s) in a row"),
        PassLimit::Unlimited => "unlimited runs in a row".to_owned(),
    };
    let ceiling = match config.policy.resolver_budget_usd {
        Some(usd) => format!(
            "a ceiling of {} USD on the resolver runs of one chain",
            usd.0
        ),
        None => "no ceiling".to_owned(),
    };
    Some(Check::info(
        Section::Project,
        "resolver",
        format!(
            "{level}; {passes}, {ceiling}; security and irreversible questions always go to the \
             decider"
        ),
    ))
}

/// Whether the project file `owlshift do` reads adds the opt-in Codex
/// reviewer (OWL-348); not when no project file is loaded.
fn codex_reviewer_asked(config: &Effective) -> bool {
    matches!(
        &config.project,
        FileState::Loaded { config, .. } if config.pipeline.codex_reviewer()
    )
}

/// The opt-in Codex reviewer (OWL-348, `pipeline.codex_reviewer`): when the
/// project asks for it, whether Codex can serve it here, by the check each
/// command makes at its start ([`codex_machine::critic_readiness`]). A
/// warning when it cannot: each review pass then runs without that seat and
/// says so. None when the key is off or no project file is loaded.
fn codex_reviewer_check(system: &dyn System, config: &Effective) -> Option<Check> {
    const SUBJECT: &str = "codex reviewer";
    if !codex_reviewer_asked(config) {
        return None;
    }
    let ready = codex_machine::critic_readiness(
        system.locate("codex"),
        system.codex_login_file(),
        |path| system.is_file(path),
        system.read_text_file(Path::new(codex_machine::REQUIREMENTS)),
    );
    Some(match ready {
        Ok(_) => Check::ok(
            Section::Project,
            SUBJECT,
            "on: each Design review and Verify pass adds a Codex reviewer, on the critic's \
             mandate, beside its Claude Code reviewers",
        ),
        Err(why) => Check::warn(
            Section::Project,
            SUBJECT,
            format!("on, but Codex cannot serve it here: {why}"),
            format!(
                "{DOES_NOT_BLOCK} each review pass runs with its Claude Code reviewers alone \
                 and says so on the ticket. The `codex` lines say how to get Codex ready."
            ),
        ),
    })
}

/// The budget on one ticket's runs (OWL-378, architecture section 4,
/// "Ticket budget"), read from the project file `owlshift do` reads: a
/// note, which blocks nothing. None without a project configuration.
fn budget_check(config: &Effective) -> Option<Check> {
    let FileState::Loaded { config, .. } = &config.project else {
        return None;
    };
    let detail = match config.policy.ticket_budget_usd {
        Some(usd) => format!(
            "{} USD on each ticket's runs, every run counted across its commands; reached, the \
             ticket parks until a person's `owlshift continue` counts it afresh",
            usd.0
        ),
        None => "none on a ticket's runs".to_owned(),
    };
    Some(Check::info(Section::Project, "budget", detail))
}

/// Whether each state named under `[tracker].states` is exactly one
/// workflow state of the project's Linear team (OWL-147): the rule by which
/// the stage moves. A name that matches none or several only keeps the stage
/// from moving, which never stops `owlshift do`, so it is a warning; what
/// keeps `do` from reading the ticket at all (no key, a rejected key, a
/// keychain it cannot read, a team Linear does not know) is a failure. None
/// for a Markdown project or none loaded: only a Linear project opens the
/// keychain and the network here, once each.
fn tracker_states_check(
    system: &dyn System,
    config: &Effective,
    accounts: Option<&TrackerAccounts>,
) -> Option<Check> {
    const SUBJECT: &str = "tracker states";
    let FileState::Loaded { config, .. } = &config.project else {
        return None;
    };
    let (TrackerKind::Linear, Some(team)) = (config.tracker.kind, &config.tracker.team) else {
        return None;
    };
    // Unresolved, the `secrets` line says why.
    let accounts = accounts?;
    let place = format!(
        "in the system keychain (service `{SERVICE}`, account `{}`)",
        accounts.key
    );
    let fail = |detail: String, why: &str, fix: Vec<Step>| {
        Some(Check::fail(Section::Project, SUBJECT, detail, why, fix))
    };
    let names = match system.linear_states(accounts, team) {
        Ok(names) => names,
        Err(StatesError::NoKey) => {
            return fail(
                format!("not checked: no Linear API key {place}"),
                "`owlshift do` reads the ticket with the Linear API key Owlshift keeps in the \
                 system keychain, and doctor reads the team's workflow states with it.",
                vec![Step::run_noting(
                    "owlshift init",
                    "paste the key when asked",
                )],
            );
        }
        Err(StatesError::Keychain(reason)) => {
            return fail(
                format!("not checked: could not read the Linear API key {place}: {reason}"),
                "`owlshift do` reads the Linear API key in the system keychain before each run.",
                vec![Step::act(KEYCHAIN_FIX)],
            );
        }
        Err(StatesError::Tracker(error)) => {
            return match error.kind {
                ErrorKind::Unauthorized => fail(
                    format!("not checked: Linear refused the API key stored {place}: {error}"),
                    "`owlshift do` reads the ticket with this key, so Linear would refuse it too.",
                    vec![Step::run_noting(
                        "owlshift init --replace-secrets",
                        "paste a valid Linear API key when asked",
                    )],
                ),
                ErrorKind::NotFound => fail(
                    format!("not checked: {error}, which `[tracker].team` names (`{team}`)"),
                    "`owlshift do` runs the tickets of the team `[tracker].team` names, and moves \
                     their stage among that team's workflow states.",
                    vec![Step::act(
                        "Set `team` under `[tracker]` in owlshift.toml to the key your team's \
                         issue identifiers start with, such as `OWL` for `OWL-12`",
                    )],
                ),
                ErrorKind::Other => Some(Check::warn(
                    Section::Project,
                    SUBJECT,
                    format!("not checked: {error}"),
                    format!(
                        "{DOES_NOT_BLOCK} doctor could not read the workflow states of team \
                         `{team}`. A name under `[tracker].states` that matches none only keeps \
                         the ticket's visible stage from moving."
                    ),
                )),
            };
        }
    };
    let mut configured = configured_states(&config.tracker.states);
    if let Admit::State(name) = &config.tracker.admit {
        configured.push(("admit.state".to_owned(), name.clone()));
    }
    let mut problems = Vec::new();
    let mut ambiguous = false;
    for (key, name) in &configured {
        match find_state(names.iter().map(String::as_str), name) {
            Ok(_) => {}
            Err(StateMismatch::Missing) => {
                problems.push(format!("no state named {name:?} (`{key}`)"));
            }
            Err(StateMismatch::Ambiguous) => {
                ambiguous = true;
                problems.push(format!("several named {name:?} (`{key}`)"));
            }
        }
    }
    let keys: Vec<&str> = configured.iter().map(|(key, _)| key.as_str()).collect();
    if problems.is_empty() {
        return Some(Check::ok(
            Section::Project,
            SUBJECT,
            format!(
                "each of {} is one workflow state of team `{team}`",
                keys.join(", ")
            ),
        ));
    }
    let mut why = format!(
        "{DOES_NOT_BLOCK} a move to a state that matches none, or several, fails with a \
         `stage_not_moved` warning event, and the ticket's visible stage stays where it was. Set \
         each under `[tracker].states` in owlshift.toml to one of the team's states, exactly, \
         case included: {}.",
        names.join(", ")
    );
    if ambiguous {
        why.push_str(" A name several states share must first be made unique in Linear.");
    }
    Some(Check::warn(
        Section::Project,
        SUBJECT,
        format!("team `{team}` has {}", problems.join("; ")),
        why,
    ))
}

/// What Linear says of the admission and proposals setup of a Linear
/// project: the labels it names, its admitters, and its proposals inbox
/// (OWL-155). None for a Markdown project or none loaded.
fn admission_facts(
    system: &dyn System,
    config: &Effective,
    secrets: Option<&TrackerAccounts>,
) -> Option<Result<AdmissionFacts, StatesError>> {
    let FileState::Loaded { config, .. } = &config.project else {
        return None;
    };
    let (TrackerKind::Linear, Some(team)) = (config.tracker.kind, &config.tracker.team) else {
        return None;
    };
    let secrets = secrets?;
    let tracker = &config.tracker;
    let mut labels = vec![PROPOSED_LABEL];
    if let Admit::Label(label) = &tracker.admit {
        labels.push(label);
    }
    labels.extend(tracker.exclude.as_deref());
    let accounts: Vec<&str> = tracker
        .admitters
        .iter()
        .flatten()
        .map(String::as_str)
        .collect();
    Some(system.linear_admission(secrets, team, &tracker.states.ready, &labels, &accounts))
}

/// Whether the project is set up to admit tickets to the drain (OWL-155,
/// build plan "P4 · The drain", part D): a gesture that can be made, the
/// admission and exclusion labels on the team or the workspace, and
/// admitters who are accounts of the workspace. Each fault is a warning,
/// since none blocks `do`; `--drain` refuses to start on them. Offline for a
/// Markdown project, which has only the faults that need no tracker. A
/// missing key or an unreadable keychain adds nothing: the tracker states
/// line fails on it. `admitters` is read from the working tree here; the
/// drain reads it at the base commit.
fn drain_admission_check(
    config: &Effective,
    facts: Option<&Result<AdmissionFacts, StatesError>>,
) -> Option<Check> {
    const SUBJECT: &str = "drain admission";
    let FileState::Loaded { config, .. } = &config.project else {
        return None;
    };
    let tracker: &Tracker = &config.tracker;
    let mut problems = Vec::new();
    let mut fixes = Vec::new();
    match &tracker.admit {
        Admit::Delegation => {
            problems.push("`admit = \"delegation\"` is not built".to_owned());
            fixes.push(
                "Decision D8's app takes no delegation scope, so nothing can be delegated to \
                 Owlshift: set `admit` under `[tracker]` to `{ label = … }` or `\"ready\"`.",
            );
        }
        Admit::Label(_) | Admit::State(_) => {}
    }
    if tracker.admitters.is_none() {
        problems.push("`admitters` is not set under `[tracker]`".to_owned());
        fixes.push(
            "List under `admitters` the Linear user ids whose gesture admits a ticket; with none, \
             no ticket is ever admitted.",
        );
    }
    let checked = matches!(facts, Some(Ok(_)));
    match facts {
        Some(Ok(found)) => {
            let mut missing_label = |what: &str, name: &str| {
                if !found.labels.iter().any(|l| l == name) {
                    problems.push(format!(
                        "the {what} label {name:?} exists on neither team nor workspace"
                    ));
                    return true;
                }
                false
            };
            let mut absent = false;
            if let Admit::Label(label) = &tracker.admit {
                absent |= missing_label("admission", label);
            }
            if let Some(exclude) = &tracker.exclude {
                absent |= missing_label("exclusion", exclude);
            }
            if absent {
                fixes.push(
                    "Create the label in Linear, on the team or the workspace, or set the name \
                     as Linear has it: names match exactly, case included.",
                );
            }
            let mut accounts = false;
            for id in tracker.admitters.iter().flatten() {
                match found.accounts.iter().find(|a| &a.id == id) {
                    None => {
                        problems.push(format!("admitter {id:?} is no account of the workspace"))
                    }
                    Some(a) if a.app => {
                        problems.push(format!(
                            "admitter {id:?} is an app user, which never admits"
                        ));
                    }
                    Some(a) if !a.active => {
                        problems.push(format!("admitter {id:?} is a deactivated account"));
                    }
                    Some(_) => continue,
                }
                accounts = true;
            }
            if accounts {
                fixes.push(
                    "Set `admitters` to the ids of active people of the workspace, as Linear \
                     gives them (a user's `id`).",
                );
            }
        }
        Some(Err(StatesError::Tracker(error))) => {
            problems.push(format!("labels and admitters not checked: {error}"));
        }
        Some(Err(StatesError::NoKey | StatesError::Keychain(_))) | None => {}
    }
    if problems.is_empty() {
        let gesture = match &tracker.admit {
            Admit::Label(label) => format!("label {label:?}"),
            Admit::State(state) => format!("state {state:?}"),
            Admit::Delegation => unreachable!("delegation is a problem"),
        };
        let admitters = tracker.admitters.iter().flatten().count();
        let scope = if checked {
            "labels and admitters exist in Linear"
        } else {
            "not checked against the tracker"
        };
        return Some(Check::ok(
            Section::Project,
            SUBJECT,
            format!(
                "admitted by {gesture}, {admitters} admitter(s) in the working tree (the drain \
                 reads the base commit); {scope}"
            ),
        ));
    }
    Some(Check::warn(
        Section::Project,
        SUBJECT,
        problems.join("; "),
        format!(
            "{DOES_NOT_BLOCK} `owlshift watch --drain` refuses to start, and would admit nothing, \
             until it is fixed. {}",
            fixes.join(" ")
        ),
    ))
}

/// Whether a follow-up Owlshift proposes can be filed (OWL-155): the team's
/// triage inbox, or the label fallback `agent:proposed` on its first backlog
/// state, chosen as the adapter chooses (`propose`). A warning when neither
/// works. For a Linear project, none unless its facts were read; a tracker
/// error is the admission line's to report. A Markdown project files at the
/// stage `Triage` (OWL-267), read from its file alone: a warning when that
/// is its ready state, where `propose` refuses every filing.
fn proposals_inbox_check(
    config: &Effective,
    facts: Option<&Result<AdmissionFacts, StatesError>>,
) -> Option<Check> {
    const SUBJECT: &str = "proposals inbox";
    let FileState::Loaded { config, .. } = &config.project else {
        return None;
    };
    if config.tracker.kind == TrackerKind::Markdown {
        let triage = MarkdownTracker::TRIAGE;
        return Some(if config.tracker.states.ready == triage {
            Check::warn(
                Section::Project,
                SUBJECT,
                format!("the ready state is {triage:?}, the stage proposals are filed at"),
                format!(
                    "{DOES_NOT_BLOCK} Owlshift cannot file the follow-ups it proposes, since a \
                     proposal there would enter the queue without a person. Name another \
                     state under `[tracker].states.ready`."
                ),
            )
        } else {
            Check::ok(
                Section::Project,
                SUBJECT,
                format!("proposals are filed as tickets at the stage {triage:?}"),
            )
        });
    }
    let team = config.tracker.team.as_deref()?;
    let Some(Ok(found)) = facts else {
        return None;
    };
    let labelled = match &found.inbox {
        Ok(false) => {
            return Some(Check::ok(
                Section::Project,
                SUBJECT,
                format!("team `{team}` files proposals in its triage inbox"),
            ));
        }
        Ok(true) if found.labels.iter().any(|l| l == PROPOSED_LABEL) => {
            return Some(Check::ok(
                Section::Project,
                SUBJECT,
                format!(
                    "team `{team}` has no triage inbox: proposals go to its first backlog state \
                     with the label {PROPOSED_LABEL:?}"
                ),
            ));
        }
        Ok(true) => true,
        Err(_) => false,
    };
    let detail = match &found.inbox {
        Err(reason) => reason.clone(),
        Ok(_) => format!(
            "team `{team}` has no triage inbox, and neither it nor the workspace has the label \
             {PROPOSED_LABEL:?} that marks a proposal in its backlog"
        ),
    };
    let fix = if labelled {
        format!("Create the label {PROPOSED_LABEL:?} in Linear, on the team or the workspace.")
    } else {
        "Turn Triage on for the team in Linear (team settings), or give it a backlog state \
         other than the ready state."
            .to_owned()
    };
    Some(Check::warn(
        Section::Project,
        SUBJECT,
        detail,
        format!(
            "{DOES_NOT_BLOCK} Owlshift cannot file the follow-ups it proposes, and `owlshift \
             watch --drain` refuses to start, until a proposals inbox exists. {fix}"
        ),
    ))
}

/// Each key set under `[tracker].states` and the state it names, keys in
/// alphabetical order. Read from the configuration as it serializes, so a
/// key added later is checked once set and an optional one left unset is
/// not.
fn configured_states(states: &States) -> Vec<(String, String)> {
    state_entries(serde_json::to_value(states).expect("the states are strings"))
}

/// The string fields of `[tracker].states` as JSON: an unset optional key is
/// `null`, or absent.
fn state_entries(states: Value) -> Vec<(String, String)> {
    let Value::Object(map) = states else {
        return Vec::new();
    };
    map.into_iter()
        .filter_map(|(key, value)| match value {
            Value::String(name) => Some((key, name)),
            _ => None,
        })
        .collect()
}

/// Where `owlshift do` keeps its event log, dedicated checkouts and
/// worktrees (OWL-109). Information only, never a failure or a warning: what
/// blocks a run is `do`'s own refusal. Only the path is reported: the folder
/// is never opened, created or listed.
/// Who Owlshift writes as on Linear (OWL-157): the Linear app's user,
/// checked as `owlshift do` checks it when it opens the tracker, or the API
/// key's account. No app is a warning: Owlshift then writes through the key,
/// as before. An app `do` would refuse fails: half of it stored, Linear
/// refusing it or not answering, since `do` never falls back to the key.
/// None for a Markdown project or none loaded.
fn tracker_identity_check(
    system: &dyn System,
    config: &Effective,
    accounts: Option<&TrackerAccounts>,
) -> Option<Check> {
    const SUBJECT: &str = "tracker identity";
    let FileState::Loaded { config, .. } = &config.project else {
        return None;
    };
    let (TrackerKind::Linear, Some(team)) = (config.tracker.kind, &config.tracker.team) else {
        return None;
    };
    // Unresolved, the `secrets` line says why.
    let accounts = accounts?;
    let place = format!(
        "in the system keychain (service `{SERVICE}`, accounts `{}` and `{}`)",
        accounts.app_id, accounts.app_secret
    );
    let why = "With the Linear app stored, `owlshift do` writes as its app user, checked before \
               any work, and refuses rather than write through the API key.";
    let shared = accounts.source == Source::Shared;
    let store_both = || {
        vec![Step::run_noting(
            "owlshift init --replace-secrets",
            if shared {
                "give this project a Linear key of its own, then its Linear app's client ID and \
                 secret, when asked"
            } else {
                "give the Linear app's client ID and secret when asked"
            },
        )]
    };
    let fail =
        |detail: String, fix: Vec<Step>| Check::fail(Section::Project, SUBJECT, detail, why, fix);
    Some(match system.linear_app(accounts, team) {
        Ok(Some(user)) => Check::ok(
            Section::Project,
            SUBJECT,
            format!("Owlshift writes on Linear as the app user `{}`", user.name),
        ),
        Ok(None) => Check::warn(
            Section::Project,
            SUBJECT,
            format!("no Linear app {place}: Owlshift writes through the API key"),
            format!(
                "{DOES_NOT_BLOCK} Linear does not notify a person of what their own API key \
                 writes, so a decider who holds the key learns of a question from this \
                 machine's desktop notification alone. A workspace admin can create an OAuth \
                 application with client credentials in Linear's API settings; `owlshift init` \
                 stores its client ID and secret."
            ),
        ),
        Err(AppError::NoKey { .. }) => Check::info(
            Section::Project,
            SUBJECT,
            "not checked: no Linear API key, which the Linear app's check needs",
        ),
        Err(AppError::Keychain(reason)) => fail(
            format!("not checked: could not read the Linear app's credentials {place}: {reason}"),
            vec![Step::act(KEYCHAIN_FIX)],
        ),
        Err(AppError::Incomplete { missing, .. }) => fail(
            format!(
                "half of the Linear app is stored: nothing under service `{SERVICE}`, account \
                 `{missing}`"
            ),
            store_both(),
        ),
        Err(AppError::Tracker { error, .. }) => fail(
            format!("the Linear app stored {place} cannot be used: {error}"),
            match error.kind {
                // The shared entries may serve another project: never
                // advise deleting them (OWL-308).
                ErrorKind::Other if shared => vec![Step::act(
                    "Check that Linear is reachable from this machine, then run `owlshift \
                     doctor` again",
                )],
                ErrorKind::Other => vec![Step::act(
                    "Check that Linear is reachable from this machine, then run `owlshift \
                     doctor` again; or delete both of the app's keychain entries to write \
                     through the API key",
                )],
                ErrorKind::Unauthorized | ErrorKind::NotFound => store_both(),
            },
        ),
    })
}

/// The project's tracker and forge secrets as doctor finds them (OWL-308):
/// the repository that names them and what is stored, presence only.
struct ProjectSecrets {
    repo: Repo,
    /// For a Linear project.
    tracker: Option<TrackerSecrets<()>>,
    forge: ForgeSecret<()>,
}

/// Why doctor could not tell which accounts hold the project's secrets.
enum Unresolved {
    /// `origin` names no GitHub repository, for this reason.
    Origin(String),
    /// The keychain could not be read, for this reason.
    Keychain(String),
}

/// The secrets of the loaded project, resolved as `owlshift do` resolves
/// them ([`secrets`]), each account's presence asked once: never a value.
/// None when no project is loaded.
fn project_secrets(
    system: &dyn System,
    config: &Effective,
) -> Option<Result<ProjectSecrets, Unresolved>> {
    let FileState::Loaded { path, config, .. } = &config.project else {
        return None;
    };
    let root = path.parent()?;
    let repo = match system.origin_repo(root) {
        Ok(repo) => repo,
        Err(reason) => return Some(Err(Unresolved::Origin(reason))),
    };
    let mut look = |account: &str| {
        system
            .secret_stored(account)
            .map(|found| found.then_some(()))
    };
    let found = (|| {
        let tracker = match config.tracker.kind {
            TrackerKind::Linear => Some(secrets::tracker(&repo, &mut look)?),
            TrackerKind::Markdown => None,
        };
        let forge = secrets::forge(&repo, &mut look)?;
        Ok((tracker, forge))
    })();
    Some(match found {
        Ok((tracker, forge)) => Ok(ProjectSecrets {
            repo,
            tracker,
            forge,
        }),
        Err(reason) => Err(Unresolved::Keychain(reason)),
    })
}

/// Whether the project's GitHub token reads its repository's default branch
/// (OWL-334): GitHub opens a pull request only on branches the token can
/// read, which a fine-grained token gets with contents (read), so without it
/// a delivery pushes its branch and then stops. Read-only, one query; the
/// pull request permission is not tried. None when the secrets are
/// unresolved or no token is stored: the `secrets` line says so.
fn forge_access_check(
    system: &dyn System,
    found: &Result<ProjectSecrets, Unresolved>,
) -> Option<Check> {
    const SUBJECT: &str = "forge access";
    let Ok(found) = found else {
        return None;
    };
    found.forge.token?;
    let repo = &found.repo;
    let account = &found.forge.account;
    let place = format!("the GitHub token under `{}`", account.token);
    let why = "`owlshift do` opens the pull request with this token once the ticket's branch is \
               pushed, and GitHub opens none on branches the token cannot read: the ticket would \
               stay undelivered.";
    let edit = "Edit the token on GitHub (Settings > Developer settings > Fine-grained tokens) \
                and save: the token keeps its value, so nothing needs storing again";
    let shared = if account.source == Source::Shared {
        " This is the shared token, which may serve other projects; `owlshift init \
         --replace-secrets` stores one of this project's own."
    } else {
        ""
    };
    let fail = |detail: String, fix: String| {
        Some(Check::fail(
            Section::Project,
            SUBJECT,
            detail,
            why,
            vec![Step::act(fix)],
        ))
    };
    match system.github_default_branch(repo) {
        Ok(DefaultBranch::Readable(branch)) => Some(Check::ok(
            Section::Project,
            SUBJECT,
            format!(
                "{place} reads `{branch}`, the default branch of github.com/{repo} (its pull \
                 request permission is not tried)"
            ),
        )),
        Ok(DefaultBranch::Empty) => Some(Check::info(
            Section::Project,
            SUBJECT,
            format!("not checked: github.com/{repo} has no default branch yet"),
        )),
        Ok(DefaultBranch::Forbidden(message)) => fail(
            format!(
                "{place} cannot read the branches of github.com/{repo}, which needs contents \
                 (read): {message}"
            ),
            format!(
                "Give the token contents (read): {edit}, after setting Repository permissions > \
                 Contents to Read-only.{shared}"
            ),
        ),
        Err(TokenTryError::Credential(reason)) => {
            fail(format!("not checked: {reason}"), KEYCHAIN_FIX.to_owned())
        }
        Err(TokenTryError::Forge(error)) => match error.kind {
            ForgeErrorKind::Unauthorized => fail(
                format!("GitHub refused {place}: {error}"),
                format!(
                    "Run `owlshift init --replace-secrets` and paste a valid token: \
                     {GITHUB_TOKEN_HELP}"
                ),
            ),
            // The refresh-failure rule (OWL-384): its error names the fix.
            ForgeErrorKind::SignInAgain => fail(
                format!("GitHub refused {place}: {error}"),
                "Run `owlshift init` in a terminal to sign in again.".to_owned(),
            ),
            ForgeErrorKind::NotFound => fail(
                format!("{place} does not see github.com/{repo}: {error}"),
                format!(
                    "Give the token access to github.com/{repo}: {edit}, after adding the \
                     repository under Repository access.{shared}"
                ),
            ),
            ForgeErrorKind::HeadMoved | ForgeErrorKind::RateLimited | ForgeErrorKind::Other => {
                Some(Check::warn(
                    Section::Project,
                    SUBJECT,
                    format!("not checked: {error}"),
                    format!(
                        "{DOES_NOT_BLOCK} doctor could not ask GitHub whether the token reads \
                         the branches of github.com/{repo}. Without contents (read), a delivery \
                         stops after its push."
                    ),
                ))
            }
        },
    }
}

/// Which keychain account each tracker and forge secret of the project is
/// read from (OWL-308). A secret read from an account every project of the
/// machine shares is a warning: the project works, on a key or token that
/// may be another project's. No GitHub token fails, as `owlshift do`
/// refuses then; a missing Linear key is the `tracker states` line's.
fn secrets_check(found: &Result<ProjectSecrets, Unresolved>) -> Check {
    const SUBJECT: &str = "secrets";
    let found = match found {
        Ok(found) => found,
        Err(Unresolved::Origin(reason)) => {
            return Check::fail(
                Section::Project,
                SUBJECT,
                format!("not checked: {reason}"),
                "Owlshift keeps a project's tracker and forge secrets under keychain accounts \
                 named by its GitHub repository, the one `origin` names, and `owlshift do` \
                 delivers to that repository.",
                vec![Step::act(
                    "Set the remote `origin` to the project's github.com repository, over SSH or \
                     with a git credential helper",
                )],
            );
        }
        Err(Unresolved::Keychain(reason)) => {
            return Check::fail(
                Section::Project,
                SUBJECT,
                format!("not checked: could not read the system keychain: {reason}"),
                "`owlshift do` reads the project's tracker and forge secrets in the system \
                 keychain before each run.",
                vec![Step::act(KEYCHAIN_FIX)],
            );
        }
    };
    let under = |account: &str, source: Source| match source {
        Source::Project => format!("`{account}`"),
        Source::Shared => format!("the shared `{account}`"),
    };
    let mut parts = Vec::new();
    let mut shared = false;
    if let Some(tracker) = &found.tracker {
        let accounts = &tracker.accounts;
        shared |= accounts.source == Source::Shared;
        parts.push(match tracker.key {
            Some(()) => format!(
                "Linear API key under {}",
                under(&accounts.key, accounts.source)
            ),
            None => format!("no Linear API key under `{}`", accounts.key),
        });
        parts.push(match (tracker.app_id, tracker.app_secret) {
            (None, None) => "no Linear app".to_owned(),
            (Some(()), Some(())) => format!(
                "Linear app under {} and `{}`",
                under(&accounts.app_id, accounts.source),
                accounts.app_secret
            ),
            _ => format!(
                "half of the Linear app under {} and `{}`",
                under(&accounts.app_id, accounts.source),
                accounts.app_secret
            ),
        });
    }
    let forge = &found.forge;
    parts.push(match forge.token {
        Some(()) => {
            shared |= forge.account.source == Source::Shared;
            format!(
                "GitHub token under {}",
                under(&forge.account.token, forge.account.source)
            )
        }
        None => format!("no GitHub token under `{}`", forge.account.token),
    });
    let detail = format!(
        "github.com/{} (service `{SERVICE}`): {}",
        found.repo,
        parts.join("; ")
    );
    if forge.token.is_none() {
        return Check::fail(
            Section::Project,
            SUBJECT,
            detail,
            "`owlshift do` opens the pull request with the project's GitHub token, kept in the \
             system keychain, and refuses to start without it.",
            vec![Step::run_noting(
                "owlshift init",
                "paste the token when asked",
            )],
        );
    }
    if shared {
        return Check::warn(
            Section::Project,
            SUBJECT,
            detail,
            format!(
                "{DOES_NOT_BLOCK} a shared account is the one every project of this machine \
                 used before secrets were kept per project, so another project's key or token \
                 may serve this one. Run `owlshift init --replace-secrets` in this repository to \
                 store its own; Owlshift keeps the shared accounts for the projects that still \
                 read them."
            ),
        );
    }
    Check::ok(Section::Project, SUBJECT, detail)
}

/// Whether a ticket ref holds the drain's one slot (OWL-291): one between
/// Ready and Watch, one whose state does not read, or one that cannot be
/// read, each of which stops every dispatch of `owlshift watch --drain`.
/// A warning naming each and the command that clears it, since none blocks
/// `do`; one a command is working now frees the slot when its work ends.
/// Read from the dedicated checkout without the project's lock. Only the
/// identifiers and stages reach the report, never why a ref did not read.
fn drain_slot_check(system: &dyn System, config: &Effective) -> Option<Check> {
    const SUBJECT: &str = "drain slot";
    let FileState::Loaded { path, .. } = &config.project else {
        return None;
    };
    let root = path.parent()?;
    let refs = match system.ticket_refs(root) {
        Ok(Some(refs)) => refs,
        Ok(None) => return Some(Check::ok(Section::Project, SUBJECT, "no ticket ref yet")),
        Err(why) => {
            return Some(Check::info(
                Section::Project,
                SUBJECT,
                format!("the ticket refs were not read: {why}"),
            ));
        }
    };
    let mut held: Vec<(TicketId, String, String)> = refs
        .into_iter()
        .filter_map(|(ticket, state)| {
            let (what, clear) = match state {
                RefState::Read(Some(state)) if holds_slot(&state) => {
                    let stage = state.stage();
                    let again = if stage == Stage::Build {
                        format!("`owlshift continue {ticket}` to finish its Build")
                    } else {
                        format!("`owlshift do {ticket}` to run it again")
                    };
                    (
                        format!("{ticket} at {stage:?}"),
                        format!("{again} or `owlshift forget {ticket}` to start it over"),
                    )
                }
                RefState::Read(Some(_)) => return None,
                RefState::Read(None) => (
                    format!("{ticket}, whose state does not read"),
                    format!("`owlshift forget {ticket}` to start it over"),
                ),
                RefState::Unread(_) => (
                    format!("{ticket}, whose ticket ref cannot be read"),
                    format!("`owlshift forget {ticket}` to start it over"),
                ),
            };
            Some((ticket, what, clear))
        })
        .collect();
    if held.is_empty() {
        return Some(Check::ok(
            Section::Project,
            SUBJECT,
            "no ticket ref holds it",
        ));
    }
    held.sort_by(|a, b| natural_cmp(a.0.as_str(), b.0.as_str()));
    let (what, clear): (Vec<String>, Vec<String>) = held
        .into_iter()
        .map(|(_, what, clear)| (what, clear))
        .unzip();
    Some(Check::warn(
        Section::Project,
        SUBJECT,
        format!("held by {}", what.join("; ")),
        format!(
            "{DOES_NOT_BLOCK} `owlshift watch --drain` dispatches nothing while a ticket ref is \
             between Ready and Watch or cannot be read. Unless a command is working on it now, \
             run {}.",
            clear.join("; ")
        ),
    ))
}

/// Whether a run's isolation check left the project quarantined (OWL-368):
/// a failure while its `unverified` marker stands and no command holds the
/// project's lock, since every command then refuses the project; with the
/// lock held, information, since every run writes the marker as it starts.
/// Read for a project file that loads or is invalid: the quarantine does
/// not depend on it.
fn quarantine_check(system: &dyn System, config: &Effective, home: Option<&Path>) -> Option<Check> {
    const SUBJECT: &str = "quarantine";
    let (FileState::Loaded { path, .. } | FileState::Invalid { path, .. }) = &config.project else {
        return None;
    };
    let root = path.parent()?;
    Some(match system.quarantine(root) {
        Ok(MarkerState::Clear) => Check::ok(Section::Project, SUBJECT, "not quarantined"),
        Ok(MarkerState::InFlight) => Check::info(
            Section::Project,
            SUBJECT,
            "a command is working the project, and a run writes the marker as it starts: run \
             `owlshift doctor` again once it ends",
        ),
        Ok(MarkerState::Marked { path, text }) => {
            let said: Vec<&str> = text
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .collect();
            let folder = path.parent().map_or_else(String::new, |p| shown(p, home));
            Check::fail(
                Section::Project,
                SUBJECT,
                format!("{} says: {}", shown(&path, home), said.join(" ")),
                "A run's isolation check did not pass, or did not finish, so the project's \
                 dedicated checkout may hold what it changed outside its worktree. Until a \
                 person clears the quarantine, Owlshift refuses the whole project on this \
                 machine: no ticket of it is run, continued, taken in or built.",
                vec![
                    Step::act(format!(
                        "Look at what the marker names, in the project's folder {folder}"
                    )),
                    Step::run_noting(
                        "owlshift unquarantine",
                        "in the project's repository: it shows the marker, then clears it once \
                         you type the project's name",
                    ),
                    Step::act(
                        "Or, to start from a fresh clone, delete the `checkout` and `worktrees` \
                         folders there first, which loses the ticket refs, then run `owlshift \
                         unquarantine`",
                    ),
                ],
            )
        }
        Err(why) => Check::info(
            Section::Project,
            SUBJECT,
            format!("the marker was not read: {why}"),
        ),
    })
}

fn data_dir_check(system: &dyn System, home: Option<&Path>) -> Check {
    const SUBJECT: &str = "data directory";
    match system.data_dir() {
        Some((path, DataDirSource::Override)) => Check::info(
            Section::Project,
            SUBJECT,
            format!("{}, from OWLSHIFT_DATA_DIR", shown(&path, home)),
        ),
        Some((path, DataDirSource::Platform)) => {
            Check::info(Section::Project, SUBJECT, shown(&path, home))
        }
        None => Check::info(
            Section::Project,
            SUBJECT,
            "none: this system has no data directory; set OWLSHIFT_DATA_DIR to an absolute path",
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::system::fake::{Answer, FakeSystem};

    /// The tracker accounts of the fake's repository, `pitchopp/owlshift`.
    fn own() -> TrackerAccounts {
        TrackerAccounts::project(&Repo::parse("pitchopp/owlshift").unwrap())
    }

    const CLAUDE_STATUS: &str = "claude auth status --json";
    const CODEX_STATUS: &str = "codex login status";

    /// Status answers carrying an e-mail address, an organisation and part
    /// of an API key, as C8 recorded them.
    const CLAUDE_LOGGED_IN: &str = r#"{"loggedIn": true, "authMethod": "claude.ai", "email": "ada@example.com", "orgId": "0f0e0d0c-org-id", "orgName": "Ada's Organization", "subscriptionType": "max"}"#;
    const CODEX_API_KEY: &str = "Logged in using an API key - sk-dummy***0fake\n";

    /// [`super::run`], and every check it made is well formed: every report
    /// a test builds goes through here, so a failure path that forgets its
    /// why or its fix fails the test that reaches it.
    fn run(system: &dyn System, config: &Effective) -> Report {
        let report = super::run(system, config);
        for check in &report.checks {
            assert_well_formed(check);
        }
        report
    }

    /// A failure says why and has at least one step; a warning says it does
    /// not block `owlshift do`; every command is one line.
    fn assert_well_formed(check: &Check) {
        match check.status {
            Status::Fail => {
                assert!(
                    check
                        .why
                        .as_deref()
                        .is_some_and(|why| !why.trim().is_empty()),
                    "a failure without a why: {check:?}"
                );
                assert!(!check.fix.is_empty(), "a failure without a fix: {check:?}");
            }
            Status::Warn => {
                assert!(
                    check
                        .why
                        .as_deref()
                        .is_some_and(|why| why.starts_with(DOES_NOT_BLOCK)),
                    "a warning that does not say it does not block: {check:?}"
                );
            }
            Status::Ok | Status::Info => {
                assert_eq!(check.why, None, "{check:?}");
            }
        }
        for step in &check.fix {
            match step {
                Step::Run { command, .. } => {
                    assert!(
                        !command.is_empty() && !command.contains('\n'),
                        "a command not on one line: {check:?}"
                    );
                }
                Step::Do(text) => assert!(!text.is_empty(), "{check:?}"),
            }
        }
    }

    fn no_config() -> Effective {
        Effective {
            project: FileState::NotApplicable("not in a git repository".into()),
            personal: FileState::Absent(PathBuf::from("/home/ada/.config/owlshift/config.toml")),
        }
    }

    /// A personal file that declares Codex, whose lines doctor then shows
    /// (OWL-347).
    fn declares_codex() -> Effective {
        let text = "[harnesses.codex]\n";
        Effective {
            project: FileState::NotApplicable("not in a git repository".into()),
            personal: FileState::Loaded {
                path: PathBuf::from("/home/ada/.config/owlshift/config.toml"),
                config: owlshift_contracts::config::PersonalConfig::parse(text).unwrap(),
                entries: Vec::new(),
            },
        }
    }

    fn with_git(system: FakeSystem) -> FakeSystem {
        system.install("git").answer(
            "git --version",
            Answer::Exit(0, "git version 2.54.0 (Apple Git-157)\n", ""),
        )
    }

    fn with_harnesses(system: FakeSystem) -> FakeSystem {
        system
            .install("claude")
            .install("codex")
            .answer(
                "claude --version",
                Answer::Exit(0, "2.1.283 (Claude Code)\n", ""),
            )
            .answer(
                "codex --version",
                Answer::Exit(0, "codex-cli 0.154.0\n", ""),
            )
    }

    fn fixes(report: &Report, subject: &str) -> Vec<Step> {
        report
            .checks
            .iter()
            .filter(|check| check.subject == subject && check.status == Status::Fail)
            .flat_map(|check| check.fix.clone())
            .collect()
    }

    fn commands(steps: &[Step]) -> Vec<&str> {
        steps
            .iter()
            .filter_map(|step| match step {
                Step::Run { command, .. } => Some(command.as_str()),
                Step::Do(_) => None,
            })
            .collect()
    }

    #[test]
    fn a_ready_machine_and_nothing_private_in_the_report() {
        let system = with_harnesses(with_git(FakeSystem::default()))
            .answer(CLAUDE_STATUS, Answer::Exit(0, CLAUDE_LOGGED_IN, ""))
            .answer(CODEX_STATUS, Answer::Exit(0, "", CODEX_API_KEY));
        let report = run(&system, &declares_codex());
        let shown = report.to_string();

        assert!(report.ready(), "{shown}");
        assert_eq!(line(&report, "git").detail, "2.54.0 (/fake/bin/git)");
        assert_eq!(
            line(&report, "claude").detail,
            "2.1.283 (/fake/bin/claude), your own login: logged in (claude.ai, max plan)"
        );
        assert!(
            line(&report, "codex")
                .detail
                .starts_with("0.154.0 (/fake/bin/codex), logged in (API key)"),
            "{shown}"
        );
        // Not in a repository: the next command is run from one.
        assert_eq!(report.next, Next::FromRepository);
        // Defense in depth: `Login` holds only fixed labels, by construction.
        for private in [
            "ada@",
            "example.com",
            "0f0e0d0c",
            "Organization",
            "sk-dummy",
            "0fake",
        ] {
            assert!(!shown.contains(private), "{private} leaked:\n{shown}");
            assert!(
                !report.to_json().to_string().contains(private),
                "{private} leaked into the JSON report"
            );
        }
    }

    /// Each check sits in its section, and the sections come in order;
    /// without a personal file, Codex has no line, installed or not
    /// (OWL-347).
    #[test]
    fn checks_are_grouped_by_section_in_order() {
        let report = run(
            &logged_in(with_harnesses(with_git(FakeSystem::default()))),
            &no_config(),
        );
        let sections: Vec<(Section, &str)> = report
            .checks
            .iter()
            .map(|check| (check.section, check.subject.as_str()))
            .collect();
        let mut expected = vec![
            (Section::Tools, "git"),
            (Section::Tools, "claude"),
            (Section::AgentIsolation, "sandbox"),
        ];
        if cfg!(unix) {
            expected.push((Section::AgentIsolation, "sentinel"));
            expected.push((Section::AgentIsolation, "sentinel test"));
        }
        expected.extend([
            (Section::AgentIsolation, "claude agent login"),
            (Section::Project, "project config"),
            (Section::Project, "personal config"),
            (Section::Project, "tracker"),
            (Section::Project, "data directory"),
        ]);
        assert_eq!(sections, expected);
    }

    /// Paths under the home folder are written `~`, whole components only.
    /// On Unix, where the fake home `/home/ada` is absolute.
    #[cfg(unix)]
    #[test]
    fn paths_under_the_home_are_shortened() {
        let home = Some(Path::new("/home/ada"));
        assert_eq!(shown(Path::new("/home/ada"), home), "~");
        assert_eq!(
            shown(Path::new("/home/ada/.config/owlshift/config.toml"), home),
            format!("~{MAIN_SEPARATOR}.config/owlshift/config.toml")
        );
        for elsewhere in ["/home/adam/x", "/mnt/home/ada/x", "relative/home/ada"] {
            assert_eq!(shown(Path::new(elsewhere), home), elsewhere);
        }
        assert_eq!(shown(Path::new("/home/ada/x"), None), "/home/ada/x");
        // A relative home is no home.
        assert_eq!(
            shown(Path::new("home/ada/x"), Some(Path::new("home/ada"))),
            "home/ada/x"
        );

        // The personal file the fake host reports is in its home.
        let report = run(
            &logged_in(with_harnesses(with_git(FakeSystem::default()))),
            &no_config(),
        );
        assert_eq!(
            line(&report, "personal config").detail,
            format!("not found at ~{MAIN_SEPARATOR}.config/owlshift/config.toml")
        );
    }

    /// A machine where agent runs cannot be confined is not ready, and the
    /// fix names what to do for the program that failed.
    #[test]
    fn a_missing_or_blocked_sandbox_is_not_ready() {
        let ready = || logged_in(with_harnesses(with_git(FakeSystem::default())));
        let missing = run(
            &ready().no_sandbox(SandboxError::Missing { program: "bwrap" }),
            &no_config(),
        );
        assert!(!missing.ready());
        assert_eq!(
            commands(&fixes(&missing, "sandbox")),
            ["sudo apt install bubblewrap"]
        );

        let blocked = run(
            &ready().no_sandbox(SandboxError::Blocked {
                program: "bwrap",
                reason: "setting up uid map: Permission denied".into(),
            }),
            &no_config(),
        );
        assert!(!blocked.ready());
        assert!(
            line(&blocked, "sandbox")
                .detail
                .ends_with("setting up uid map: Permission denied"),
            "{blocked}"
        );
        let steps = fixes(&blocked, "sandbox");
        let commands = commands(&steps);
        assert!(
            commands.contains(&"sudo apparmor_parser -r /etc/apparmor.d/bwrap"),
            "{blocked}"
        );
        assert!(
            commands.contains(&apparmor_profile_command().as_str()),
            "{blocked}"
        );

        // macOS: no AppArmor, no apt.
        for error in [
            SandboxError::Missing {
                program: "sandbox-exec",
            },
            SandboxError::Blocked {
                program: "sandbox-exec",
                reason: "sandbox_apply: Operation not permitted".into(),
            },
        ] {
            let report = run(&ready().no_sandbox(error), &no_config());
            let fix = format!("{:?}", fixes(&report, "sandbox"));
            assert!(fix.contains("sandbox-exec"), "{fix}");
            assert!(!fix.contains("apt") && !fix.contains("AppArmor"), "{fix}");
        }
    }

    /// The one-line command that writes the AppArmor profile writes exactly
    /// the profile the build plan checked.
    #[cfg(unix)]
    #[test]
    fn the_apparmor_command_writes_the_profile() {
        assert!(!BWRAP_APPARMOR_PROFILE.contains('\''));
        let out = std::process::Command::new("/bin/sh")
            .args(["-c", &apparmor_profile_printf()])
            .output()
            .unwrap();
        assert!(out.status.success());
        assert_eq!(
            String::from_utf8(out.stdout).unwrap(),
            BWRAP_APPARMOR_PROFILE
        );
    }

    /// Every failure path the fakes can reach says why and how to fix it
    /// (checked by [`run`]); the paths the other tests do not reach are here.
    #[test]
    fn every_failure_says_why_and_how_to_fix_it() {
        let ready = || logged_in(with_harnesses(with_git(FakeSystem::default())));
        for error in [
            SandboxError::Unsupported,
            SandboxError::Missing { program: "other" },
            SandboxError::Blocked {
                program: "other",
                reason: "no".into(),
            },
            SandboxError::Path(PathBuf::from("/home/ada/odd")),
        ] {
            let report = run(&ready().no_sandbox(error), &no_config());
            assert_eq!(line(&report, "sandbox").status, Status::Fail);
        }

        for answer in [Answer::Exit(1, "", "boom"), Answer::TimedOut] {
            let report = run(&ready().answer("git --version", answer), &no_config());
            assert_eq!(
                commands(&fixes(&report, "git")),
                ["git --version"],
                "{report}"
            );
        }
        let unfamiliar = run(
            &ready().answer("git --version", Answer::Exit(0, "git, surely\n", "")),
            &no_config(),
        );
        assert_eq!(line(&unfamiliar, "git").status, Status::Warn);

        let home = Some(Path::new("/home/ada"));
        for state in [
            FileState::<()>::Unavailable("git rev-parse failed".into()),
            FileState::Invalid {
                path: PathBuf::from("/home/ada/p/owlshift.toml"),
                error: "requires >=99: upgrade Owlshift".into(),
            },
        ] {
            let check = file_check("project config", &state, home);
            assert_eq!(check.status, Status::Fail);
            assert_well_formed(&check);
        }
    }

    /// OWL-199: a git older than the floor fails, since every `owlshift do`
    /// would stop on an option it lacks; at the floor or above, whatever
    /// the build adds after the version, it passes. A version too large to
    /// compare keeps the warning of an unfamiliar answer.
    #[test]
    fn a_git_older_than_the_floor_fails() {
        let git = |answer: &'static str| {
            let system = logged_in(with_harnesses(FakeSystem::default()))
                .install("git")
                .answer("git --version", Answer::Exit(0, answer, ""));
            let report = run(&system, &no_config());
            (report.ready(), line(&report, "git").clone())
        };

        let (ready, old) = git("git version 2.38.5\n");
        assert!(!ready);
        assert_eq!(old.status, Status::Fail);
        assert_eq!(old.detail, "2.38.5 (/fake/bin/git), older than 2.39.0");
        assert_eq!(
            old.fix,
            [
                Step::act("Install git 2.39.0 or later: https://git-scm.com/downloads"),
                Step::run_noting(
                    "git --version",
                    "should print 2.39.0 or later: an older git may come first on the PATH"
                ),
            ]
        );

        for answer in [
            "git version 2.39.0\n",
            "git version 2.39.5 (Apple Git-154)\n",
            "git version 2.47.1.windows.1\n",
            "git version 2.39.0.rc0\n",
        ] {
            let (ready, check) = git(answer);
            assert!(ready, "{answer}");
            assert_eq!(check.status, Status::Ok, "{answer}");
        }

        let (_, huge) = git("git version 2.99999999999999999999.0\n");
        assert_eq!(huge.status, Status::Warn);
    }

    /// OWL-88: a sentinel that ended or never started is a warning that says
    /// what is lost, never a failure; a running one is quiet. OWL-91: a
    /// stopped one warns too.
    #[cfg(unix)]
    #[test]
    fn a_missing_or_ended_sentinel_warns_without_failing() {
        let ready = || logged_in(with_harnesses(with_git(FakeSystem::default())));
        let running = run(&ready(), &no_config());
        let sentinel = line(&running, "sentinel");
        assert_eq!(sentinel.status, Status::Ok, "{running}");
        assert!(
            sentinel.detail.starts_with("running (pid 4242)"),
            "{running}"
        );

        for (status, detail, hint) in [
            (
                SentinelStatus::Ended("signal: 9 (SIGKILL)".into()),
                "ended (signal: 9 (SIGKILL)): a hard kill of Owlshift would leave",
                "`owlshift-sentinel`",
            ),
            (
                SentinelStatus::Stopped { pid: 4242 },
                "stopped (pid 4242), it reads nothing until continued: the system continues it at \
                 Owlshift's end, a hard kill included, and it then stops, best effort, the \
                 processes Owlshift started, unless its input fills first, in which case Owlshift \
                 kills it and it stops nothing",
                "stopped in its first milliseconds, before it ignores SIGHUP, or, on Linux, if \
                 Owlshift's end leaves it to a subreaper in the same session, where it stays \
                 stopped until something continues it. To restore it, look for what stops the \
                 `/bin/sh` process",
            ),
            (
                SentinelStatus::NotRunning,
                "not running, it could not start: a hard kill of Owlshift would leave",
                "Check that `/bin/sh` runs",
            ),
        ] {
            let report = run(&ready().sentinel_is(status), &no_config());
            let sentinel = line(&report, "sentinel");
            assert!(report.ready(), "{report}");
            assert_eq!(sentinel.status, Status::Warn, "{report}");
            assert!(sentinel.detail.starts_with(detail), "{report}");
            assert!(sentinel.why.as_deref().unwrap().contains(hint), "{report}");
        }
    }

    /// OWL-90: a test sentinel that does not stop its test group, or cannot
    /// be started, is a warning that says why, never a failure.
    #[cfg(unix)]
    #[test]
    fn a_sentinel_that_fails_its_test_warns_without_failing() {
        let ready = || logged_in(with_harnesses(with_git(FakeSystem::default())));
        let works = run(&ready(), &no_config());
        let probe = line(&works, "sentinel test");
        assert_eq!(probe.status, Status::Ok, "{works}");
        assert!(
            probe
                .detail
                .starts_with("a test sentinel stopped a test process group 3 ms"),
            "{works}"
        );

        for (probe, detail, hint) in [
            (
                SentinelProbe::Fails("the test group still ran 5000 ms".into()),
                "the test group still ran 5000 ms: a hard kill of Owlshift may leave",
                "`kill -s KILL -- -<group>`",
            ),
            (
                SentinelProbe::CannotStart("a test sentinel: no such file".into()),
                "could not start a test sentinel: no such file: a hard kill",
                "Check that `/bin/sh` runs",
            ),
        ] {
            let report = run(&ready().sentinel_probe_is(probe), &no_config());
            let tested = line(&report, "sentinel test");
            assert!(report.ready(), "{report}");
            assert_eq!(tested.status, Status::Warn, "{report}");
            assert!(tested.detail.starts_with(detail), "{report}");
            assert!(tested.why.as_deref().unwrap().contains(hint), "{report}");
        }
    }

    /// OWL-94: agent runs need their token in the keychain. Stored, the
    /// line is ok; absent, the fix is `claude setup-token` then `owlshift
    /// init`; an unreadable keychain says so. Without `claude`, it is not
    /// asked.
    #[test]
    fn the_agent_login_is_a_token_in_the_keychain() {
        let ready = || logged_in(with_harnesses(with_git(FakeSystem::default())));
        let stored = run(&ready(), &no_config());
        let login = line(&stored, "claude agent login");
        assert_eq!(login.status, Status::Ok, "{stored}");
        assert!(
            login
                .detail
                .contains("(service `owlshift`, account `claude-agent`)"),
            "{stored}"
        );

        let missing = run(&ready().unstored("claude-agent"), &no_config());
        assert!(!missing.ready());
        assert_eq!(
            commands(&fixes(&missing, "claude agent login")),
            AGENT_LOGIN_COMMANDS
        );
        assert!(!missing.to_string().contains("auth login"), "{missing}");

        let locked = run(&ready().keychain_fails("keychain: locked"), &no_config());
        let login = line(&locked, "claude agent login");
        assert_eq!(login.status, Status::Fail, "{locked}");
        assert!(login.detail.ends_with("keychain: locked"), "{locked}");

        let no_claude = with_git(FakeSystem::default()).install("codex").answer(
            "codex --version",
            Answer::Exit(0, "codex-cli 0.154.0\n", ""),
        );
        let report = run(&logged_in(no_claude), &no_config());
        assert!(
            report
                .checks
                .iter()
                .all(|check| check.subject != "claude agent login"),
            "{report}"
        );
    }

    /// Doctor's steps and the refusal of `owlshift do` give the same fix for
    /// a missing agent login: they cannot drift apart unnoticed.
    #[test]
    fn the_agent_login_fix_matches_the_run_refusal() {
        for command in AGENT_LOGIN_COMMANDS {
            assert!(
                crate::executor::harness::AGENT_LOGIN_FIX.contains(&format!("`{command}`")),
                "{command}"
            );
        }
    }

    #[test]
    fn a_missing_claude_fails_and_says_how_to_fix_it() {
        let system = with_git(FakeSystem::default())
            .install("codex")
            .answer(
                "codex --version",
                Answer::Exit(0, "codex-cli 0.154.0\n", ""),
            )
            .answer(CODEX_STATUS, Answer::Exit(0, "", CODEX_API_KEY));
        let report = run(&system, &no_config());

        assert!(!report.ready());
        assert_eq!(
            fixes(&report, "claude"),
            [Step::act(
                "Install Claude Code: https://code.claude.com/docs/en/setup"
            )]
        );
        assert_eq!(report.failures(), 1);
    }

    /// OWL-102: `owlshift do` does not run Codex before the review roles
    /// (P5), so a Codex that is logged out, missing or unreadable is a
    /// warning that says so, not a problem to fix.
    #[test]
    fn a_codex_that_is_not_ready_warns_while_claude_is_ready() {
        let claude_only = |system: FakeSystem| {
            system.install("claude").answer(
                "claude --version",
                Answer::Exit(0, "2.1.283 (Claude Code)\n", ""),
            )
        };
        let logged_out = claude_only(with_git(FakeSystem::default()))
            .install("codex")
            .answer(
                "codex --version",
                Answer::Exit(0, "codex-cli 0.154.0\n", ""),
            )
            .answer(CODEX_STATUS, Answer::Exit(1, "", "Not logged in\n"))
            .answer(CLAUDE_STATUS, Answer::Exit(0, CLAUDE_LOGGED_IN, ""));
        let unknown = claude_only(with_git(FakeSystem::default()))
            .install("codex")
            .answer(
                "codex --version",
                Answer::Exit(0, "codex-cli 0.154.0\n", ""),
            )
            .answer(
                CODEX_STATUS,
                Answer::Exit(0, "", "Signed in as ada@example.com\n"),
            )
            .answer(CLAUDE_STATUS, Answer::Exit(0, CLAUDE_LOGGED_IN, ""));
        let missing = logged_in(claude_only(with_git(FakeSystem::default())));

        for (system, step) in [
            (logged_out, "`codex login`"),
            (unknown, "`codex login status`"),
            (missing, "`npm install -g @openai/codex`"),
        ] {
            let report = run(&system, &declares_codex());
            let codex = line(&report, "codex");
            assert_eq!(codex.status, Status::Warn, "{report}");
            assert_eq!(codex.fix, []);
            let why = codex.why.as_deref().unwrap();
            assert!(why.starts_with(DOES_NOT_BLOCK), "{why}");
            assert!(why.contains("no role needs Codex"), "{why}");
            assert!(why.contains(step), "{why}");
            assert!(report.ready(), "{report}");
            assert_eq!(report.failures(), 0, "{report}");
            assert!(!report.to_string().contains("ada@example.com"), "{report}");
            assert!(
                !report
                    .checks
                    .iter()
                    .any(|c| c.subject == "codex" && c.status == Status::Fail),
                "{report}"
            );
        }
    }

    /// OWL-97: agent runs log in with the stored token, never with the
    /// operator's own Claude Code login, so that login logged out or
    /// unreadable is information, not a failure. The token stays the gate.
    #[test]
    fn the_own_claude_login_is_information_only() {
        for (answer, state) in [
            (
                Answer::Exit(1, r#"{"loggedIn": false, "authMethod": "none"}"#, ""),
                "not logged in",
            ),
            (
                Answer::TimedOut,
                "unknown, `claude auth status --json` did not answer in time",
            ),
            (
                Answer::Exit(0, "Signed in as ada@example.com\n", ""),
                "unknown, `claude auth status --json` gave an answer Owlshift does not know",
            ),
        ] {
            let system = logged_in(with_harnesses(with_git(FakeSystem::default())))
                .answer(CLAUDE_STATUS, answer);
            let report = run(&system, &no_config());
            let shown = report.to_string();
            let claude = line(&report, "claude");
            assert!(report.ready(), "{shown}");
            assert_eq!(claude.status, Status::Info, "{shown}");
            assert_eq!(
                claude.detail,
                format!(
                    "2.1.283 (/fake/bin/claude), your own login: {state} (agent runs do not use it)"
                )
            );
            assert_eq!(claude.fix, []);
            assert_eq!(line(&report, "claude agent login").status, Status::Ok);
            assert!(!shown.contains("auth login"), "{shown}");
            assert!(!shown.contains("ada@example.com"), "{shown}");
            let json = report.to_json();
            let entry = json["checks"]
                .as_array()
                .unwrap()
                .iter()
                .find(|check| check["subject"] == "claude")
                .unwrap();
            assert_eq!(entry["status"], "info");
            assert_eq!(entry["fix"], serde_json::json!([]));
            assert_eq!(json["problems"], 0);

            // Without the token, its own line fails; the own login does not.
            let report = run(&system.unstored("claude-agent"), &no_config());
            assert_eq!(report.failures(), 1, "{report}");
            assert_eq!(
                commands(&fixes(&report, "claude agent login")),
                AGENT_LOGIN_COMMANDS
            );
            assert_eq!(line(&report, "claude").status, Status::Info);
        }
    }

    fn line<'a>(report: &'a Report, subject: &str) -> &'a Check {
        report
            .checks
            .iter()
            .find(|check| check.subject == subject)
            .unwrap()
    }

    fn logged_in(system: FakeSystem) -> FakeSystem {
        system
            .answer(CLAUDE_STATUS, Answer::Exit(0, CLAUDE_LOGGED_IN, ""))
            .answer(CODEX_STATUS, Answer::Exit(0, "", CODEX_API_KEY))
    }

    #[test]
    fn an_untested_or_unreadable_version_warns_without_failing() {
        let why = "Does not block `owlshift do`: an untested version usually works. \
                   If a run misbehaves, install a tested version: claude 2.1.283, 2.1.284, 2.1.300, 2.1.302.";
        for answer in [
            Answer::Exit(0, "9.9.9 (Claude Code)\n", ""),
            Answer::Exit(0, "2.1.283-beta.1 (Claude Code)\n", ""),
            Answer::TimedOut,
        ] {
            let system = logged_in(with_harnesses(with_git(FakeSystem::default())))
                .answer("claude --version", answer);
            let report = run(&system, &no_config());
            let claude = line(&report, "claude");
            assert!(report.ready(), "{report}");
            assert_eq!(claude.status, Status::Warn, "{report}");
            assert!(
                claude
                    .detail
                    .ends_with("logged in (claude.ai, max plan); version not tested with Owlshift"),
                "{report}"
            );
            assert_eq!(claude.why.as_deref(), Some(why));
            assert_eq!(claude.fix, []);
        }
    }

    #[test]
    fn a_tested_version_is_quiet() {
        let system = logged_in(with_harnesses(with_git(FakeSystem::default())));
        let report = run(&system, &declares_codex());
        let claude = line(&report, "claude");
        assert_eq!(claude.status, Status::Ok, "{report}");
        assert!(!claude.detail.contains("tested"), "{report}");
        assert_eq!(claude.why, None);

        // Codex 0.154.0 is tested since its contract tests (OWL-299).
        let codex = line(&report, "codex");
        assert!(report.ready(), "{report}");
        assert_eq!(codex.status, Status::Ok, "{report}");
        assert_eq!(
            codex.detail,
            "0.154.0 (/fake/bin/codex), logged in (API key)"
        );
        assert_eq!(codex.why, None);
    }

    /// OWL-299: a Codex logged in without the login file a Codex run copies,
    /// such as a login kept in the keyring, is not ready: its warning says
    /// where the file was looked for and how to get it written.
    #[test]
    fn a_codex_login_without_its_file_is_the_warning() {
        let system = logged_in(with_harnesses(with_git(FakeSystem::default())))
            .without_file("/home/ada/.codex/auth.json");
        let report = run(&system, &declares_codex());
        let codex = line(&report, "codex");
        assert_eq!(codex.status, Status::Warn, "{report}");
        // Shown as every path under the home is: `~/…` where `/home/ada` is
        // absolute, as is on Windows.
        let file = shown(
            Path::new("/home/ada/.codex/auth.json"),
            Some(Path::new("/home/ada")),
        );
        assert_eq!(
            codex.detail,
            format!("0.154.0 (/fake/bin/codex), logged in (API key), but no login file at {file}")
        );
        #[cfg(unix)]
        assert_eq!(file, "~/.codex/auth.json");
        let why = codex.why.as_deref().unwrap();
        assert!(why.contains("no role needs Codex"), "{why}");
        assert!(
            why.contains("`codex login`") && why.contains("auth.json"),
            "{why}"
        );
        assert!(report.ready(), "{report}");
    }

    /// A Codex that is not ready at an untested version is still the warning
    /// of OWL-102, and its detail says the version is untested; an
    /// informational line becomes a warning.
    #[test]
    fn a_codex_not_ready_at_an_untested_version_is_one_warning() {
        let system = with_harnesses(with_git(FakeSystem::default()))
            .answer(
                "claude --version",
                Answer::Exit(0, "9.9.9 (Claude Code)\n", ""),
            )
            .answer(
                CLAUDE_STATUS,
                Answer::Exit(1, r#"{"loggedIn": false, "authMethod": "none"}"#, ""),
            )
            .answer(
                "codex --version",
                Answer::Exit(0, "codex-cli 0.155.0\n", ""),
            )
            .answer(CODEX_STATUS, Answer::Exit(1, "", "Not logged in\n"));
        let report = run(&system, &declares_codex());
        let codex = line(&report, "codex");
        assert_eq!(codex.status, Status::Warn);
        assert_eq!(
            codex.detail,
            "0.155.0 (/fake/bin/codex), not logged in; version not tested with Owlshift"
        );
        assert_eq!(codex.fix, []);

        let claude = line(&report, "claude");
        assert_eq!(report.failures(), 0, "{report}");
        assert_eq!(claude.status, Status::Warn);
        assert_eq!(
            claude.detail,
            "9.9.9 (/fake/bin/claude), your own login: not logged in (agent runs do not use \
             it); version not tested with Owlshift"
        );
        assert_eq!(
            claude.why.as_deref(),
            Some(
                "Does not block `owlshift do`: an untested version usually works. If a run \
                 misbehaves, install a tested version: claude 2.1.283, 2.1.284, 2.1.300, 2.1.302."
            )
        );
        assert_eq!(claude.fix, []);
    }

    #[test]
    fn a_missing_git_fails() {
        let system = with_harnesses(FakeSystem::default())
            .answer(CLAUDE_STATUS, Answer::Exit(0, CLAUDE_LOGGED_IN, ""))
            .answer(
                CODEX_STATUS,
                Answer::Exit(0, "", "Logged in using ChatGPT\n"),
            );
        let report = run(&system, &no_config());
        assert_eq!(
            fixes(&report, "git"),
            [Step::act("Install git: https://git-scm.com/downloads")]
        );
    }

    #[test]
    fn only_declared_harnesses_are_required() {
        let dir = tempfile::tempdir().unwrap();
        let personal = dir.path().join("config.toml");
        std::fs::write(&personal, "[harnesses.claude]\n").unwrap();
        let config = Effective {
            project: FileState::NotApplicable("not in a git repository".into()),
            personal: FileState::load(personal, owlshift_contracts::config::PersonalConfig::parse),
        };
        let system = with_git(FakeSystem::default())
            .install("claude")
            .answer(
                "claude --version",
                Answer::Exit(0, "2.1.283 (Claude Code)\n", ""),
            )
            .answer(CLAUDE_STATUS, Answer::Exit(0, CLAUDE_LOGGED_IN, ""))
            .text_file(
                codex_machine::REQUIREMENTS,
                Ok("allowed_sandbox_modes = [\"read-only\"]"),
            );
        let report = run(&system, &config);

        assert!(report.ready(), "{report}");
        assert!(
            report
                .checks
                .iter()
                .all(|check| !check.subject.starts_with("codex")),
            "{report}"
        );
    }

    /// OWL-347: Claude Code runs every role, so it is checked even when the
    /// personal file declares Codex alone, whose lines are warnings at most.
    #[test]
    fn claude_is_checked_whatever_the_personal_file_declares() {
        let report = run(&with_git(FakeSystem::default()), &declares_codex());
        assert_eq!(line(&report, "claude").status, Status::Fail, "{report}");
        assert_eq!(line(&report, "codex").status, Status::Warn, "{report}");
    }

    /// OWL-314: the MCP servers and hooks of `/etc/codex` are named as the
    /// operator's, each file read on its own; what an unreadable file holds
    /// never reaches the report, only its line.
    #[test]
    fn the_machines_codex_config_is_named_as_the_operators() {
        let system = logged_in(with_harnesses(with_git(FakeSystem::default())))
            .text_file(
                codex_machine::CONFIG,
                Ok("[mcp_servers.search]\ncommand = \"search-mcp\"\n\n\
                    [[hooks.SessionStart]]\nhooks = [{ type = \"command\", command = \"mark\" }]\n"),
            )
            .text_file(
                codex_machine::MANAGED_CONFIG,
                Ok("[mcp_servers.tracker]\nbearer = sk-secret-value\n"),
            )
            .text_file(codex_machine::REQUIREMENTS, Err("permission denied"));
        let report = run(&system, &declares_codex());

        assert!(report.ready(), "{report}");
        let config = line(&report, "codex machine config");
        assert_eq!(config.status, Status::Warn);
        assert_eq!(
            config.detail,
            "/etc/codex/config.toml declares MCP servers `search` and hooks on \
             `SessionStart`; /etc/codex/managed_config.toml is not valid TOML (line 2)"
        );
        assert!(config.why.as_deref().unwrap().contains("administrator's"));
        let requirements = line(&report, "codex requirements");
        assert_eq!(requirements.status, Status::Warn);
        assert_eq!(
            requirements.detail,
            "/etc/codex/requirements.toml could not be read: permission denied"
        );
        for shown in [report.to_string(), report.to_json().to_string()] {
            assert!(!shown.contains("sk-secret"), "{shown}");
        }
    }

    /// OWL-314: requirements refusing `danger-full-access` stop every Codex
    /// agent run, so a declared Codex is not ready, a warning; a managed
    /// hooks folder is named either way.
    #[test]
    fn requirements_refusing_full_access_make_codex_not_ready() {
        let hooks = "\n[hooks]\nmanaged_dir = \"/etc/codex/hooks\"\n";
        let refusing = format!("allowed_sandbox_modes = [\"read-only\"]\n{hooks}");
        let system = logged_in(with_harnesses(with_git(FakeSystem::default())))
            .text_file(codex_machine::REQUIREMENTS, Ok(refusing.as_str()));
        let report = run(&system, &declares_codex());

        assert!(report.ready(), "{report}");
        assert_eq!(line(&report, "codex machine config").status, Status::Ok);
        let refused = line(&report, "codex requirements");
        assert_eq!(refused.status, Status::Warn);
        assert_eq!(
            refused.detail,
            "/etc/codex/requirements.toml allows only `read-only`, and a Codex agent run needs \
             `danger-full-access`, Owlshift confining it in Codex's stead: every Codex agent run \
             stops before it starts; a managed hooks folder, `/etc/codex/hooks`"
        );
        assert_eq!(
            refused.why.as_deref(),
            Some(
                "Does not block `owlshift do`: no role needs Codex. To get Codex ready: Have the \
                 machine's administrator add \
                 \"danger-full-access\" to `allowed_sandbox_modes` in \
                 /etc/codex/requirements.toml."
            )
        );

        let allowing = format!("allowed_sandbox_modes = [\"danger-full-access\"]\n{hooks}");
        let system = logged_in(with_harnesses(with_git(FakeSystem::default())))
            .text_file(codex_machine::REQUIREMENTS, Ok(allowing.as_str()));
        let allowed = run(&system, &declares_codex());
        let allowed = line(&allowed, "codex requirements");
        assert_eq!(allowed.status, Status::Warn);
        assert!(
            allowed.detail.ends_with("`/etc/codex/hooks`"),
            "{allowed:?}"
        );
    }

    /// The next command follows the project file: `owlshift do` with one,
    /// `owlshift init` in a repository without one.
    #[test]
    fn the_next_command_follows_the_project_file() {
        let system = logged_in(with_harnesses(with_git(FakeSystem::default())));
        let absent = Effective {
            project: FileState::Absent(PathBuf::from("/home/ada/p/owlshift.toml")),
            ..no_config()
        };
        assert_eq!(run(&system, &absent).next, Next::Init);
        let (_dir, loaded) = linear_project();
        assert_eq!(run(&system, &loaded).next, Next::Do);
    }

    /// OWL-332: the project config line checks the gate `owlshift do` would
    /// run: the default branch's, as the last fetch left it, else the
    /// working tree's. Empty, it fails, with `git fetch` as the last step of
    /// the fix; so does a file at the default branch `do` would refuse.
    #[test]
    fn an_empty_gate_where_do_reads_it_fails_the_project_config_line() {
        let config = |gate: &str| {
            ProjectConfig::parse(&format!(
                "requires = \">=0.0\"\n[tracker]\nkind = \"markdown\"\nadmit = \"delegation\"\n\
                 states = {{ ready = \"a\", working = \"b\", needs_input = \"c\", review = \"d\" }}\n\
                 [stack]\ngate = {gate}\n[pipeline]\ndefault = \"trivial\"\nplan_approval = \"never\"\n\
                 [models]\n[policy]\nalways_human = []\n"
            ))
            .unwrap()
        };
        let base = crate::project::Base {
            remote_ref: "origin/main".to_owned(),
            branch: "main".to_owned(),
            commit: "0123456789abcdef".to_owned(),
        };
        let at_base = |gate: Option<&str>| {
            Ok(DefaultBranchFile::Read {
                base: base.clone(),
                file: gate.map(|gate| Box::new(config(gate))),
            })
        };
        let check = |answer: Result<DefaultBranchFile, String>, local: &str| {
            let state = FileState::Loaded {
                path: PathBuf::from("/home/ada/p/owlshift.toml"),
                config: config(local),
                entries: Vec::new(),
            };
            let system = FakeSystem::default().default_branch_file_is(answer);
            project_config_check(&system, &state, Some(Path::new("/home/ada")))
        };
        const GATED: &str = r#"["cargo test"]"#;

        // Empty at the default branch: a failure, whatever the working tree
        // says, since `do` reads the commit's file.
        let empty = check(at_base(Some("[]")), GATED);
        assert_eq!(empty.status, Status::Fail);
        assert_eq!(
            empty.detail,
            "`stack.gate` is empty in owlshift.toml at origin/main (0123456789ab)"
        );
        assert_well_formed(&empty);
        assert_eq!(commands(&empty.fix), ["git fetch origin"]);

        // A gate at the default branch passes over an empty working tree.
        let gated = check(at_base(Some(GATED)), "[]");
        assert_eq!(gated.status, Status::Ok, "{gated:?}");
        assert!(
            gated
                .detail
                .ends_with("gate of 1 command(s) from owlshift.toml at origin/main (0123456789ab)"),
            "{gated:?}"
        );

        // No file at the default branch, or a default branch unknown here:
        // the working tree's gate, as `do` falls back to it, said so.
        let unknown = || {
            Ok(DefaultBranchFile::Unknown(
                "`origin/HEAD` is not set".into(),
            ))
        };
        for (answer, says) in [
            (at_base(None), "origin/main (0123456789ab) holding none"),
            (unknown(), "`git remote set-head origin --auto` sets it"),
        ] {
            let empty = check(answer.clone(), "[]");
            assert_eq!(empty.status, Status::Fail, "{empty:?}");
            assert!(empty.detail.contains(says), "{empty:?}");
            assert_eq!(check(answer, GATED).status, Status::Ok);
        }

        // A file at the default branch `do` would refuse.
        let refused = check(
            Err("owlshift.toml on origin/main: requires >=99".to_owned()),
            GATED,
        );
        assert_eq!(refused.status, Status::Fail);
        assert!(
            refused
                .detail
                .ends_with("at the default branch: owlshift.toml on origin/main: requires >=99"),
            "{refused:?}"
        );
        assert_well_formed(&refused);
    }

    fn linear_project() -> (tempfile::TempDir, Effective) {
        project_tracked_by(
            r#"kind = "linear"
team = "OWL"
admit = "delegation"
states = { ready = "Todo", working = "In Progress", needs_input = "Needs Input", review = "In Review" }"#,
        )
    }

    /// A loaded project whose `[tracker]` table is `tracker`.
    fn project_tracked_by(tracker: &str) -> (tempfile::TempDir, Effective) {
        project_tracked_with(tracker, "")
    }

    /// [`project_tracked_by`], with `policy` added to its `[policy]` table.
    fn project_tracked_with(tracker: &str, policy: &str) -> (tempfile::TempDir, Effective) {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("owlshift.toml");
        std::fs::write(
            &project,
            format!(
                r#"requires = ">=0.0"
[tracker]
{tracker}
[stack]
gate = ["cargo test"]
[pipeline]
default = "trivial"
plan_approval = "never"
[models]
[policy]
always_human = []
{policy}
"#
            ),
        )
        .unwrap();
        let config = Effective {
            project: FileState::load(project, owlshift_contracts::config::ProjectConfig::parse),
            personal: FileState::NotApplicable("no configuration directory".into()),
        };
        (dir, config)
    }

    /// OWL-109: the data directory is one information line: the default
    /// path, a path set by `OWLSHIFT_DATA_DIR`, or none with how to fix it.
    /// The report's readiness is the same in all three, in text and in JSON.
    #[cfg(unix)]
    #[test]
    fn the_data_directory_is_information_only() {
        let ready = || logged_in(with_harnesses(with_git(FakeSystem::default())));
        let cases = [
            (None, "~/.local/share/owlshift", "~/.local/share/owlshift"),
            (
                Some(Some((
                    PathBuf::from("/srv/owlshift-data"),
                    DataDirSource::Override,
                ))),
                "/srv/owlshift-data, from OWLSHIFT_DATA_DIR",
                "/srv/owlshift-data, from OWLSHIFT_DATA_DIR",
            ),
            (
                Some(None),
                "none: this system has no data directory; set OWLSHIFT_DATA_DIR to an absolute \
                 path",
                "set OWLSHIFT_DATA_DIR to an absolute path",
            ),
        ];
        for (dir, detail, text) in cases {
            let system = match dir {
                Some(dir) => ready().data_dir_is(dir),
                None => ready(),
            };
            let report = run(&system, &no_config());
            let check = line(&report, "data directory");
            assert_eq!(check.status, Status::Info, "{report}");
            assert_eq!(check.detail, detail);
            assert_eq!(check.fix, []);
            assert!(report.ready(), "{report}");
            assert_eq!(report.failures(), 0);
            // The text report wraps long lines.
            let printed = report.to_string();
            let flat = printed.split_whitespace().collect::<Vec<_>>().join(" ");
            assert!(flat.contains(text), "{report}");
            let json = report.to_json();
            let entry = json["checks"]
                .as_array()
                .unwrap()
                .iter()
                .find(|check| check["subject"] == "data directory")
                .unwrap();
            assert_eq!(entry["status"], "info");
            assert_eq!(entry["section"], "project");
            assert_eq!(entry["detail"], detail);
            assert_eq!(json["problems"], 0);
        }
    }

    #[test]
    fn the_tracker_line_names_what_the_adapter_does() {
        let (_dir, config) = linear_project();
        let tracker = tracker_check(&config);
        assert_well_formed(&tracker);
        assert_eq!(tracker.status, Status::Ok);
        assert_eq!(
            tracker.detail,
            "`linear`: read a ticket, list admitted tickets, read and post comments, \
             visible stage, blocked-by relations, proposals inbox, ticket documents, fold \
             superseded comments, comment reactions"
        );
    }

    /// Team `OWL`'s workflow states as Linear listed them on 2026-10-04.
    const OWL_STATES: &[&str] = &[
        "In Review",
        "Needs Input",
        "Triage",
        "Duplicate",
        "Done",
        "Backlog",
        "Todo",
        "Canceled",
        "In Progress",
    ];

    /// OWL-147: each name under `[tracker].states` must be one workflow
    /// state of the team. A misnamed or ambiguous one warns, with the valid
    /// names; what keeps `do` from reading the ticket fails, with its fix;
    /// Linear out of reach warns that nothing was checked.
    #[test]
    fn the_tracker_states_are_checked_against_the_linear_team() {
        use owlshift_adapters::tracker::Error;

        let ready = || logged_in(with_harnesses(with_git(FakeSystem::default())));
        let (_dir, owl) = linear_project();
        let states_line = |system: &FakeSystem, config: &Effective| {
            let report = run(system, config);
            line(&report, "tracker states").clone()
        };

        let found = states_line(&ready().linear_states_are(Ok(OWL_STATES)), &owl);
        assert_eq!(found.status, Status::Ok, "{found:?}");
        assert_eq!(
            found.detail,
            "each of needs_input, ready, review, working is one workflow state of team `OWL`"
        );

        let (_dir, misnamed) = project_tracked_by(
            r#"kind = "linear"
team = "OWL"
admit = "delegation"
states = { ready = "Todo", working = "Doing", needs_input = "Needs Input", review = "Done" }"#,
        );
        let twice: Vec<&str> = OWL_STATES.iter().copied().chain(["Done"]).collect();
        let report = run(&ready().linear_states_are(Ok(&twice)), &misnamed);
        let warned = line(&report, "tracker states");
        assert!(report.ready(), "{report}");
        assert_eq!(warned.status, Status::Warn);
        assert_eq!(
            warned.detail,
            "team `OWL` has several named \"Done\" (`review`); no state named \"Doing\" (`working`)"
        );
        let why = warned.why.as_deref().unwrap();
        assert!(
            why.contains("case included: In Review, Needs Input, Triage, Duplicate, Done,"),
            "{why}"
        );
        assert!(
            why.ends_with("must first be made unique in Linear."),
            "{why}"
        );

        let not_found = Error::new(
            ErrorKind::NotFound,
            "Linear has no team with the key \"OWL\"",
        );
        let refused = Error::new(ErrorKind::Unauthorized, "Linear: AUTHENTICATION_ERROR: no");
        let failures: [(FakeSystem, &[&str]); 3] = [
            (ready().unstored(&own().key), &["owlshift init"]),
            (
                ready().linear_states_are(Err(refused)),
                &["owlshift init --replace-secrets"],
            ),
            (ready().linear_states_are(Err(not_found)), &[]),
        ];
        for (system, expected) in failures {
            let report = run(&system, &owl);
            let failed = line(&report, "tracker states");
            assert_eq!(failed.status, Status::Fail, "{report}");
            assert!(failed.detail.starts_with("not checked: "), "{failed:?}");
            assert_eq!(commands(&failed.fix), expected, "{failed:?}");
        }

        // The fake answers as Linear out of reach unless told otherwise.
        let report = run(&ready(), &owl);
        let offline = line(&report, "tracker states");
        assert_eq!(offline.status, Status::Warn);
        assert_eq!(offline.detail, "not checked: Linear: no network in tests");
        assert!(report.ready(), "{report}");
    }

    /// OWL-157: the Linear app Owlshift writes as. None stored warns, since
    /// writing through the key still works; what `do` would refuse fails.
    #[test]
    fn the_github_token_is_tried_on_the_default_branch() {
        use owlshift_adapters::forge::{Branch, Error};

        let ready = || logged_in(with_harnesses(with_git(FakeSystem::default())));
        let (_dir, owl) = markdown_project();
        let access = |system: &FakeSystem| {
            let report = run(system, &owl);
            let found = report
                .checks
                .iter()
                .find(|check| check.subject == "forge access")
                .cloned();
            (found, report.ready())
        };

        let readable = Ok(DefaultBranch::Readable(Branch::new("develop").unwrap()));
        let (found, ready_to_run) = access(&ready().github_default_branch_is(readable));
        let found = found.unwrap();
        assert_eq!(found.status, Status::Ok, "{found:?}");
        assert!(
            found.detail.starts_with(
                "the GitHub token under `github.com/pitchopp/owlshift/github` reads `develop`"
            ),
            "{found:?}"
        );
        assert!(ready_to_run);

        let (empty, _) = access(&ready().github_default_branch_is(Ok(DefaultBranch::Empty)));
        assert_eq!(empty.unwrap().status, Status::Info);

        // The answer GitHub gave on 2026-10-17 to a token without contents
        // (read): the line names the permission, and editing the token is
        // the fix, never storing it again.
        let refusal = "GitHub: FORBIDDEN: Resource not accessible by personal access token";
        let forbidden = Ok(DefaultBranch::Forbidden(refusal.into()));
        let (failed, ready_to_run) = access(&ready().github_default_branch_is(forbidden.clone()));
        let failed = failed.unwrap();
        assert_eq!(failed.status, Status::Fail, "{failed:?}");
        assert!(
            failed.detail.contains("contents (read)") && failed.detail.ends_with(refusal),
            "{failed:?}"
        );
        assert_eq!(
            failed.fix.len(),
            1,
            "editing the token is the one step: {failed:?}"
        );
        let Step::Do(fix) = &failed.fix[0] else {
            panic!("{failed:?}");
        };
        assert!(
            fix.contains("Contents to Read-only") && fix.contains("keeps its value"),
            "{fix}"
        );
        assert!(!fix.contains("replace-secrets"), "{fix}");
        assert!(!ready_to_run);
        // The shared token may be another project's: the fix says so.
        let (shared, _) = access(&ready().only_shared().github_default_branch_is(forbidden));
        let Step::Do(fix) = &shared.unwrap().fix[0] else {
            panic!("a step to do");
        };
        assert!(fix.contains("shared token"), "{fix}");

        let forge_error = |kind, message| Err(TokenTryError::Forge(Error::new(kind, message)));
        for (answer, status, fix) in [
            (
                forge_error(ForgeErrorKind::Unauthorized, "GitHub refused the token"),
                Status::Fail,
                "owlshift init --replace-secrets",
            ),
            (
                forge_error(ForgeErrorKind::NotFound, "no repository"),
                Status::Fail,
                "Repository access",
            ),
            (
                Err(TokenTryError::Credential("the keychain is locked".into())),
                Status::Fail,
                KEYCHAIN_FIX,
            ),
            (
                forge_error(ForgeErrorKind::Other, "GitHub: connection refused"),
                Status::Warn,
                "",
            ),
            (
                forge_error(ForgeErrorKind::RateLimited, "GitHub's rate limit"),
                Status::Warn,
                "",
            ),
        ] {
            let (found, ready_to_run) = access(&ready().github_default_branch_is(answer));
            let found = found.unwrap();
            assert_eq!(found.status, status, "{found:?}");
            assert_eq!(ready_to_run, status != Status::Fail, "{found:?}");
            if status == Status::Fail {
                let Step::Do(step) = &found.fix[0] else {
                    panic!("{found:?}");
                };
                assert!(step.contains(fix), "{found:?}");
            }
        }

        // No token: the `secrets` line fails, and no token is tried.
        let repo = Repo::parse("pitchopp/owlshift").unwrap();
        let none = ready()
            .unstored(&secrets::ForgeAccount::project(&repo).token)
            .unstored(&secrets::ForgeAccount::shared().token);
        assert_eq!(access(&none).0, None);
        // OWL-382: the project's GitHub App token alone is a credential.
        let app_only = ready()
            .app_token_stored()
            .unstored(&secrets::ForgeAccount::project(&repo).token)
            .unstored(&secrets::ForgeAccount::shared().token);
        assert_eq!(access(&app_only).0.unwrap().status, Status::Ok);
    }

    #[test]
    fn the_linear_app_is_checked_as_do_checks_it() {
        use owlshift_adapters::tracker::Error;

        let ready = || logged_in(with_harnesses(with_git(FakeSystem::default())));
        let (_dir, owl) = linear_project();
        let identity = |system: &FakeSystem| {
            let report = run(system, &owl);
            (line(&report, "tracker identity").clone(), report.ready())
        };

        let (found, ready_to_run) = identity(&ready().linear_app_is(Ok("owlshiftbot")));
        assert_eq!(found.status, Status::Ok, "{found:?}");
        assert_eq!(
            found.detail,
            "Owlshift writes on Linear as the app user `owlshiftbot`"
        );
        assert!(ready_to_run);

        let (none, ready_to_run) = identity(&ready());
        assert_eq!(none.status, Status::Warn, "{none:?}");
        assert!(
            none.detail.ends_with("Owlshift writes through the API key"),
            "{none:?}"
        );
        assert!(ready_to_run);

        let refused = Error::new(ErrorKind::Unauthorized, "refused");
        let offline = Error::new(ErrorKind::Other, "Linear: no network");
        let failures: [(FakeSystem, &[&str]); 3] = [
            (
                ready()
                    .linear_app_is(Ok("owlshiftbot"))
                    .unstored(&own().app_secret),
                &["owlshift init --replace-secrets"],
            ),
            (
                ready().linear_app_is(Err(refused)),
                &["owlshift init --replace-secrets"],
            ),
            (ready().linear_app_is(Err(offline)), &[]),
        ];
        for (system, expected) in failures {
            let (failed, ready_to_run) = identity(&system);
            assert_eq!(failed.status, Status::Fail, "{failed:?}");
            assert_eq!(commands(&failed.fix), expected, "{failed:?}");
            assert!(!ready_to_run);
        }
    }

    /// A Markdown project names states the adapter writes as they are: no
    /// line, no keychain, no network (a project not loaded, as above).
    #[test]
    fn a_markdown_project_has_no_tracker_states_line() {
        let (_dir, markdown) = project_tracked_by(
            r#"kind = "markdown"
admit = "delegation"
states = { ready = "Todo", working = "Doing", needs_input = "Asked", review = "Review" }"#,
        );
        let system = logged_in(with_harnesses(with_git(FakeSystem::default())))
            .keychain_fails("must not be read");
        let report = run(&system, &markdown);
        assert!(report.checks.iter().all(|c| c.subject != "tracker states"));
    }

    /// Every key set under `[tracker].states` is checked, one added later
    /// included; an optional key left unset is not.
    #[test]
    fn every_state_key_set_is_checked() {
        let (_dir, owl) = linear_project();
        let FileState::Loaded { config, .. } = &owl.project else {
            panic!("{:?}", owl.project)
        };
        let keys: Vec<String> = configured_states(&config.tracker.states)
            .into_iter()
            .map(|(key, _)| key)
            .collect();
        assert_eq!(keys, ["needs_input", "ready", "review", "working"]);
        let later = serde_json::json!({ "ready": "Todo", "parked": "Parked", "gone": null });
        assert_eq!(
            state_entries(later),
            [
                ("parked".to_owned(), "Parked".to_owned()),
                ("ready".to_owned(), "Todo".to_owned())
            ]
        );
    }
    const LABELLED_TRACKER: &str = r#"kind = "linear"
team = "OWL"
admit = { label = "agent" }
admitters = ["u-ann"]
exclude = "hold"
states = { ready = "Todo", working = "In Progress", needs_input = "Needs Input", review = "In Review" }"#;

    fn facts(
        inbox: Result<bool, &str>,
        labels: &[&str],
        accounts: &[(&str, bool, bool)],
    ) -> AdmissionFacts {
        AdmissionFacts {
            inbox: inbox.map_err(str::to_owned),
            labels: labels.iter().map(|&l| l.to_owned()).collect(),
            accounts: accounts
                .iter()
                .map(|&(id, app, active)| crate::system::AccountFact {
                    id: id.to_owned(),
                    app,
                    active,
                })
                .collect(),
        }
    }

    /// OWL-155: the state gesture is checked with the other states, by the
    /// same rule, under the key `admit.state`.
    #[test]
    fn the_admission_state_is_a_tracker_state_too() {
        let ready = || logged_in(with_harnesses(with_git(FakeSystem::default())));
        let (_dir, config) = project_tracked_by(
            r#"kind = "linear"
team = "OWL"
admit = { state = "Todo" }
states = { ready = "Todo", working = "In Progress", needs_input = "Needs Input", review = "In Review" }"#,
        );
        let report = run(&ready().linear_states_are(Ok(OWL_STATES)), &config);
        let found = line(&report, "tracker states");
        assert_eq!(found.status, Status::Ok, "{found:?}");
        assert!(
            found
                .detail
                .ends_with("review, working, admit.state is one workflow state of team `OWL`")
                || found.detail.contains("admit.state"),
            "{found:?}"
        );

        let without_todo: Vec<&str> = OWL_STATES
            .iter()
            .copied()
            .filter(|s| *s != "Todo")
            .collect();
        let report = run(&ready().linear_states_are(Ok(&without_todo)), &config);
        let warned = line(&report, "tracker states");
        assert_eq!(warned.status, Status::Warn);
        assert!(
            warned
                .detail
                .contains("no state named \"Todo\" (`admit.state`)"),
            "{warned:?}"
        );
    }

    /// OWL-155: the admission label, the exclusion label and the admitters
    /// must exist; each fault is one warning that does not block `do`.
    #[test]
    fn the_admission_setup_is_checked_against_linear() {
        use owlshift_adapters::tracker::Error;

        let ready = || logged_in(with_harnesses(with_git(FakeSystem::default())));
        let (_dir, config) = project_tracked_by(LABELLED_TRACKER);
        let admission = |system: &FakeSystem| {
            let report = run(system, &config);
            assert!(report.ready(), "{report}");
            line(&report, "drain admission").clone()
        };

        let all = facts(Ok(false), &["agent", "hold"], &[("u-ann", false, true)]);
        let found = admission(&ready().linear_admission_is(Ok(all)));
        assert_eq!(found.status, Status::Ok, "{found:?}");
        assert!(
            found.detail.contains("label \"agent\", 1 admitter(s)"),
            "{found:?}"
        );
        assert!(found.detail.contains("exist in Linear"), "{found:?}");

        for (answer, expected) in [
            (
                facts(Ok(false), &["hold"], &[("u-ann", false, true)]),
                "the admission label \"agent\" exists on neither team nor workspace",
            ),
            (
                facts(Ok(false), &["agent"], &[("u-ann", false, true)]),
                "the exclusion label \"hold\" exists on neither team nor workspace",
            ),
            (
                facts(Ok(false), &["agent", "hold"], &[]),
                "admitter \"u-ann\" is no account of the workspace",
            ),
            (
                facts(Ok(false), &["agent", "hold"], &[("u-ann", true, true)]),
                "admitter \"u-ann\" is an app user, which never admits",
            ),
            (
                facts(Ok(false), &["agent", "hold"], &[("u-ann", false, false)]),
                "admitter \"u-ann\" is a deactivated account",
            ),
        ] {
            let warned = admission(&ready().linear_admission_is(Ok(answer)));
            assert_eq!(warned.status, Status::Warn, "{warned:?}");
            assert_eq!(warned.detail, expected);
            let why = warned.why.as_deref().unwrap();
            assert!(why.starts_with("Does not block `owlshift do`:"), "{why}");
            assert!(
                why.contains("`owlshift watch --drain` refuses to start"),
                "{why}"
            );
        }

        // Linear out of reach: one warning that nothing was checked. A
        // missing key adds nothing, the states line fails on it.
        let offline = admission(&ready());
        assert_eq!(offline.status, Status::Warn);
        assert_eq!(
            offline.detail,
            "labels and admitters not checked: Linear: no network in tests"
        );
        let refused = Error::new(ErrorKind::Unauthorized, "Linear: AUTHENTICATION_ERROR: no");
        let refused = admission(&ready().linear_admission_is(Err(refused)));
        assert!(
            refused.detail.contains("not checked: Linear: AUTH"),
            "{refused:?}"
        );
        let report = run(&ready().unstored(&own().key), &config);
        assert_eq!(line(&report, "drain admission").status, Status::Ok);
    }

    /// OWL-307: without `[forge].reviewers` no review is read, a warning
    /// that blocks nothing; with it, how many are listed.
    #[test]
    fn forge_reviewers_are_named_or_warned_of() {
        let system = logged_in(with_harnesses(with_git(FakeSystem::default())));
        let tracker = r#"kind = "markdown"
admit = "delegation"
states = { ready = "Todo", working = "Doing", needs_input = "Asked", review = "Review" }"#;
        let (_dir, without) = project_tracked_by(tracker);
        let report = run(&system, &without);
        let warned = line(&report, "forge reviewers");
        assert_eq!(warned.status, Status::Warn);
        assert_eq!(
            warned.detail,
            "`[forge].reviewers` is not set: no review on Owlshift's pull requests is read"
        );
        let (_dir, with) = project_tracked_by(&format!(
            "{tracker}\n[forge]\nreviewers = {{ ann-dev = \"u-ann\", bob = \"u-bob\" }}"
        ));
        let report = run(&system, &with);
        let named = line(&report, "forge reviewers");
        assert_eq!(named.status, Status::Ok);
        assert!(
            named.detail.starts_with("2 reviewer(s)"),
            "{}",
            named.detail
        );
    }

    /// OWL-378: the `budget` line names the ticket budget, or none, as a
    /// note that blocks nothing.
    #[test]
    fn the_budget_line_names_the_ticket_budget() {
        let system = logged_in(with_harnesses(with_git(FakeSystem::default())));
        let tracker = r#"kind = "markdown"
admit = "delegation"
states = { ready = "Todo", working = "Doing", needs_input = "Asked", review = "Review" }"#;
        for (policy, detail) in [
            ("", "none on a ticket's runs"),
            (
                "ticket_budget_usd = 100.0",
                "100 USD on each ticket's runs, every run counted across its commands; reached, \
                 the ticket parks until a person's `owlshift continue` counts it afresh",
            ),
        ] {
            let (_dir, config) = project_tracked_with(tracker, policy);
            let report = run(&system, &config);
            let budget = line(&report, "budget");
            assert_eq!(budget.status, Status::Info, "{policy}");
            assert_eq!(budget.detail, detail, "{policy}");
        }
    }

    /// OWL-348: with `pipeline.codex_reviewer` on, and no `[harnesses.codex]`
    /// in a personal file, Codex is checked and the `codex reviewer` line
    /// says whether it can serve, a warning when it cannot; off, neither.
    #[test]
    fn the_codex_reviewer_line_says_whether_codex_can_serve_it() {
        let tracker = r#"kind = "markdown"
admit = "delegation"
states = { ready = "Todo", working = "Doing", needs_input = "Asked", review = "Review" }"#;
        let (_off_dir, off) = project_tracked_with(tracker, "");
        let (dir, _) = project_tracked_with(tracker, "");
        let path = dir.path().join("owlshift.toml");
        let text = std::fs::read_to_string(&path).unwrap().replace(
            "plan_approval = \"never\"",
            "plan_approval = \"never\"\ncodex_reviewer = true",
        );
        std::fs::write(&path, text).unwrap();
        let on = Effective {
            project: FileState::load(path, owlshift_contracts::config::ProjectConfig::parse),
            personal: FileState::NotApplicable("no configuration directory".into()),
        };
        let ready = logged_in(with_harnesses(with_git(FakeSystem::default())));
        let report = run(&ready, &off);
        assert!(report.checks.iter().all(|check| check.subject != "codex"));
        assert!(
            report
                .checks
                .iter()
                .all(|check| check.subject != "codex reviewer")
        );

        let report = run(&ready, &on);
        assert_eq!(line(&report, "codex").status, Status::Ok, "{report}");
        let reviewer = line(&report, "codex reviewer");
        assert_eq!(reviewer.status, Status::Ok, "{report}");
        assert!(
            reviewer
                .detail
                .starts_with("on: each Design review and Verify pass adds a Codex reviewer")
        );

        let missing = logged_in(with_git(FakeSystem::default()).install("claude").answer(
            "claude --version",
            Answer::Exit(0, "2.1.283 (Claude Code)\n", ""),
        ));
        let report = run(&missing, &on);
        assert!(report.ready(), "{report}");
        assert_eq!(line(&report, "codex").status, Status::Warn, "{report}");
        let reviewer = line(&report, "codex reviewer");
        assert_eq!(reviewer.status, Status::Warn, "{report}");
        assert_eq!(
            reviewer.detail,
            "on, but Codex cannot serve it here: `codex` is not on the PATH"
        );
        assert!(
            reviewer
                .why
                .as_deref()
                .is_some_and(|why| why.contains("its Claude Code reviewers alone")),
            "{report}"
        );
    }

    /// OWL-328: the `resolver` line names the level, the pass limit and the
    /// ceiling, as a note that blocks nothing.
    #[test]
    fn the_resolver_line_names_the_level_passes_and_ceiling() {
        let system = logged_in(with_harnesses(with_git(FakeSystem::default())));
        let tracker = r#"kind = "markdown"
admit = "delegation"
states = { ready = "Todo", working = "Doing", needs_input = "Asked", review = "Review" }"#;
        let cases = [
            (
                "",
                "delegation discoverable; 5 run(s) in a row, no ceiling; security and \
                 irreversible questions always go to the decider",
            ),
            (
                "resolver_delegation = \"ticket\"\nresolver_passes = 2",
                "delegation ticket; 2 run(s) in a row, no ceiling; security and irreversible \
                 questions always go to the decider",
            ),
            (
                "resolver_delegation = \"none\"",
                "delegation none: every question goes to the decider; 5 run(s) in a row, no \
                 ceiling; security and irreversible questions always go to the decider",
            ),
            (
                "resolver_delegation = \"all\"\nresolver_passes = \"unlimited\"\n\
                 resolver_budget_usd = 2.5",
                "delegation all; unlimited runs in a row, a ceiling of 2.5 USD on the resolver \
                 runs of one chain; security and irreversible questions always go to the decider",
            ),
        ];
        for (policy, detail) in cases {
            let (_dir, config) = project_tracked_with(tracker, policy);
            let report = run(&system, &config);
            let resolver = line(&report, "resolver");
            assert_eq!(resolver.status, Status::Info, "{policy}");
            assert_eq!(resolver.detail, detail, "{policy}");
            assert!(report.ready() || resolver.status != Status::Fail);
        }
    }

    /// OWL-155: delegation and a missing `admitters` warn without the
    /// tracker, in a Markdown project, which reaches neither the
    /// keychain nor the network.
    #[test]
    fn delegation_and_missing_admitters_warn_offline() {
        let system = logged_in(with_harnesses(with_git(FakeSystem::default())));
        let (_dir, markdown) = project_tracked_by(
            r#"kind = "markdown"
admit = "delegation"
states = { ready = "Todo", working = "Doing", needs_input = "Asked", review = "Review" }"#,
        );
        let report = run(&system, &markdown);
        let warned = line(&report, "drain admission");
        assert_eq!(warned.status, Status::Warn);
        assert_eq!(
            warned.detail,
            "`admit = \"delegation\"` is not built; `admitters` is not set under `[tracker]`"
        );
        let inbox = line(&report, "proposals inbox");
        assert_eq!(inbox.status, Status::Ok);
        assert_eq!(
            inbox.detail,
            "proposals are filed as tickets at the stage \"Triage\""
        );
        assert!(report.ready(), "{report}");

        // OWL-267: a ready state named as the inbox refuses every filing.
        let (_dir, triage) = project_tracked_by(
            r#"kind = "markdown"
admit = "delegation"
states = { ready = "Triage", working = "Doing", needs_input = "Asked", review = "Review" }"#,
        );
        let report = run(&system, &triage);
        let refused = line(&report, "proposals inbox");
        assert_eq!(refused.status, Status::Warn);
        assert!(refused.detail.contains("\"Triage\""), "{refused:?}");
        assert!(report.ready(), "{report}");

        let (_dir, labelled) = project_tracked_by(
            r#"kind = "markdown"
admit = { label = "agent" }
admitters = ["ann"]
states = { ready = "Todo", working = "Doing", needs_input = "Asked", review = "Review" }"#,
        );
        let report = run(&system, &labelled);
        let found = line(&report, "drain admission");
        assert_eq!(found.status, Status::Ok, "{found:?}");
        assert!(
            found.detail.ends_with("not checked against the tracker"),
            "{found:?}"
        );

        // No project file: nothing to check.
        let report = run(&system, &no_config());
        assert!(report.checks.iter().all(|c| c.subject != "drain admission"));
    }

    /// OWL-155: a proposal goes to the triage inbox, else to the backlog
    /// with the label; neither is a warning, as `propose` would refuse.
    #[test]
    fn the_proposals_inbox_is_the_one_propose_would_use() {
        let ready = || logged_in(with_harnesses(with_git(FakeSystem::default())));
        let (_dir, config) = project_tracked_by(LABELLED_TRACKER);
        let inbox = |answer: AdmissionFacts| {
            let report = run(&ready().linear_admission_is(Ok(answer)), &config);
            assert!(report.ready(), "{report}");
            line(&report, "proposals inbox").clone()
        };

        let triage = inbox(facts(Ok(false), &[], &[]));
        assert_eq!(triage.status, Status::Ok);
        assert_eq!(
            triage.detail,
            "team `OWL` files proposals in its triage inbox"
        );

        let labelled = inbox(facts(Ok(true), &["agent:proposed"], &[]));
        assert_eq!(labelled.status, Status::Ok);
        assert!(
            labelled.detail.contains("first backlog state"),
            "{labelled:?}"
        );

        let no_label = inbox(facts(Ok(true), &[], &[]));
        assert_eq!(no_label.status, Status::Warn);
        assert!(
            no_label.detail.contains("\"agent:proposed\""),
            "{no_label:?}"
        );
        let why = no_label.why.as_deref().unwrap();
        assert!(why.contains("Create the label \"agent:proposed\""), "{why}");
        assert!(
            why.contains("`owlshift watch --drain` refuses to start"),
            "{why}"
        );

        let refused = inbox(facts(
            Err("team OWL has no triage inbox and no backlog state to file a proposal in"),
            &[],
            &[],
        ));
        assert_eq!(refused.status, Status::Warn);
        assert!(refused.detail.contains("no backlog state"), "{refused:?}");

        // Nothing read, nothing said: a tracker error is the admission
        // line's, a missing key the states line's.
        let report = run(&ready(), &config);
        assert!(report.checks.iter().all(|c| c.subject != "proposals inbox"));
    }

    fn at(ticket: &str, stage: Stage) -> (TicketId, RefState) {
        use owlshift_core::state::{Status, TicketState};
        let round = u32::from(stage != Stage::Intake);
        let state = TicketState::restore(Status::Active(stage), round, 0, 0, 0, 0).unwrap();
        (TicketId::new(ticket).unwrap(), RefState::Read(Some(state)))
    }

    fn markdown_project() -> (tempfile::TempDir, Effective) {
        project_tracked_by(
            r#"kind = "markdown"
admit = { label = "agent" }
admitters = ["ann"]
states = { ready = "Todo", working = "Doing", needs_input = "Asked", review = "Review" }"#,
        )
    }

    /// OWL-291: a ticket ref between Ready and Watch, one whose state does
    /// not read and one that cannot be read hold the drain's slot: one
    /// warning names each, in identifier order, with the command that
    /// clears it; the refs at Ready and Watch are not named.
    #[test]
    fn a_ticket_ref_holding_the_drain_slot_is_named_with_its_fix() {
        let ready = || logged_in(with_harnesses(with_git(FakeSystem::default())));
        let (_dir, config) = markdown_project();
        let id = |ticket| TicketId::new(ticket).unwrap();
        let refs = vec![
            at("OWL-175", Stage::Verify),
            at("OWL-12", Stage::Ready),
            (id("OWL-9"), RefState::Unread("not a commit".to_owned())),
            at("OWL-30", Stage::Watch),
            (id("OWL-80"), RefState::Read(None)),
            at("OWL-173", Stage::Build),
        ];
        let report = run(&ready().ticket_refs_are(Ok(refs)), &config);
        assert!(report.ready(), "{report}");
        let warned = line(&report, "drain slot");
        assert_well_formed(warned);
        assert_eq!(warned.status, Status::Warn);
        assert_eq!(
            warned.detail,
            "held by OWL-9, whose ticket ref cannot be read; OWL-80, whose state does not read; \
             OWL-173 at Build; OWL-175 at Verify"
        );
        let why = warned.why.as_deref().unwrap();
        for fix in [
            "`owlshift forget OWL-9` to start it over",
            "`owlshift forget OWL-80` to start it over",
            "`owlshift continue OWL-173` to finish its Build or `owlshift forget OWL-173`",
            "`owlshift do OWL-175` to run it again or `owlshift forget OWL-175`",
        ] {
            assert!(why.contains(fix), "{fix}: {why}");
        }
        assert!(!why.contains("not a commit"), "{why}");
    }

    /// OWL-291: no holder, no dedicated checkout and refs that cannot be
    /// listed are no warning.
    #[test]
    fn a_free_slot_is_ok_and_unread_refs_are_information() {
        let ready = || logged_in(with_harnesses(with_git(FakeSystem::default())));
        let (_dir, config) = markdown_project();
        for (system, status, detail) in [
            (
                ready().ticket_refs_are(Ok(vec![at("OWL-1", Stage::Ready)])),
                Status::Ok,
                "no ticket ref holds it",
            ),
            (ready(), Status::Ok, "no ticket ref yet"),
            (
                ready().ticket_refs_are(Err("no remote".to_owned())),
                Status::Info,
                "the ticket refs were not read: no remote",
            ),
        ] {
            let report = run(&system, &config);
            let found = line(&report, "drain slot");
            assert_eq!((found.status, found.detail.as_str()), (status, detail));
        }
        let report = run(&ready(), &no_config());
        assert!(report.checks.iter().all(|c| c.subject != "drain slot"));
    }

    /// OWL-368: a marker that stands with the project's lock free fails the
    /// report, showing what it says and how to clear it; one while a command
    /// works the project, or one that cannot be read, is information; none
    /// is ok.
    #[test]
    fn a_quarantine_fails_with_its_marker_and_how_to_clear_it() {
        let ready = || logged_in(with_harnesses(with_git(FakeSystem::default())));
        let (_dir, config) = markdown_project();
        let path =
            PathBuf::from("/home/ada/.local/share/owlshift/projects/github.com/o/r/unverified");
        let marked = MarkerState::Marked {
            path: path.clone(),
            text: "Run r1 of OWL-160 broke isolation:\n- the main checkout's HEAD moved\n"
                .to_owned(),
        };
        let report = run(&ready().quarantine_is(Ok(marked)), &config);
        assert!(!report.ready(), "{report}");
        let failed = line(&report, "quarantine");
        assert_eq!(failed.status, Status::Fail);
        // The marker's path as the report writes every path: `~` for the fake
        // home where that home is absolute, as on Unix.
        let shown_path = shown(&path, Some(Path::new("/home/ada")));
        assert_eq!(
            failed.detail,
            format!(
                "{shown_path} says: Run r1 of OWL-160 broke isolation: - the main checkout's HEAD \
                 moved"
            )
        );
        #[cfg(unix)]
        assert_eq!(
            shown_path,
            "~/.local/share/owlshift/projects/github.com/o/r/unverified"
        );
        assert!(
            failed
                .fix
                .iter()
                .any(|step| matches!(step, Step::Run { command, .. } if command == "owlshift unquarantine")),
            "{failed:?}"
        );
        for (system, status) in [
            (ready(), Status::Ok),
            (
                ready().quarantine_is(Ok(MarkerState::InFlight)),
                Status::Info,
            ),
            (
                ready().quarantine_is(Err("no remote".to_owned())),
                Status::Info,
            ),
        ] {
            let report = run(&system, &config);
            assert_eq!(line(&report, "quarantine").status, status, "{report}");
        }
    }

    /// OWL-308: the `secrets` line names the account each tracker and forge
    /// secret is read from, warns when one is shared, and fails when `do`
    /// would refuse. The tracker lines read the accounts it resolved.
    #[test]
    fn the_secrets_line_names_each_account() {
        use owlshift_adapters::tracker::Error;

        let ready = || logged_in(with_harnesses(with_git(FakeSystem::default())));
        let (_dir, owl) = linear_project();
        let secrets_line = |system: &FakeSystem| {
            let report = run(system, &owl);
            (line(&report, "secrets").clone(), report)
        };

        let (own_line, report) = secrets_line(&ready());
        assert_eq!(own_line.status, Status::Ok, "{own_line:?}");
        assert_eq!(
            own_line.detail,
            "github.com/pitchopp/owlshift (service `owlshift`): Linear API key under \
             `github.com/pitchopp/owlshift/linear`; Linear app under \
             `github.com/pitchopp/owlshift/linear-app-client-id` and \
             `github.com/pitchopp/owlshift/linear-app-client-secret`; GitHub token under \
             `github.com/pitchopp/owlshift/github`"
        );
        assert!(report.ready(), "{report}");
        let no_app = ready().unstored(&own().app_id).unstored(&own().app_secret);
        assert!(
            secrets_line(&no_app).0.detail.contains("; no Linear app; "),
            "the app's pair is reported missing"
        );

        let refused = Error::new(ErrorKind::Unauthorized, "refused");
        let shared = ready().only_shared().linear_app_is(Err(refused));
        let (shared_line, report) = secrets_line(&shared);
        assert_eq!(shared_line.status, Status::Warn, "{shared_line:?}");
        assert_eq!(
            shared_line.detail,
            "github.com/pitchopp/owlshift (service `owlshift`): Linear API key under the shared \
             `linear`; Linear app under the shared `linear-app-client-id` and \
             `linear-app-client-secret`; GitHub token under the shared `github`"
        );
        let why = shared_line.why.as_deref().unwrap();
        assert!(
            why.contains("`owlshift init --replace-secrets` in this repository"),
            "{why}"
        );
        // The tracker lines read the shared accounts too, and never advise
        // deleting them.
        let identity = line(&report, "tracker identity");
        assert!(
            identity
                .detail
                .contains("accounts `linear-app-client-id` and `linear-app-client-secret`"),
            "{identity:?}"
        );
        assert!(!format!("{identity:?}").contains("delete"), "{identity:?}");

        let no_token = ready()
            .unstored("github.com/pitchopp/owlshift/github")
            .unstored("github");
        let (failed, report) = secrets_line(&no_token);
        assert_eq!(failed.status, Status::Fail, "{failed:?}");
        assert!(
            failed
                .detail
                .ends_with("no GitHub token under `github.com/pitchopp/owlshift/github`"),
            "{failed:?}"
        );
        assert_eq!(commands(&failed.fix), ["owlshift init"]);
        assert!(!report.ready());

        // What keeps doctor from naming the accounts fails here alone: the
        // tracker lines are left out.
        for (system, fix) in [
            (ready().keychain_fails("locked"), &[] as &[&str]),
            (
                ready().origin_is(Err("the repository has no remote `origin`")),
                &[],
            ),
        ] {
            let (failed, report) = secrets_line(&system);
            assert_eq!(failed.status, Status::Fail, "{failed:?}");
            assert!(failed.detail.starts_with("not checked: "), "{failed:?}");
            assert_eq!(commands(&failed.fix), fix, "{failed:?}");
            assert!(
                !report
                    .checks
                    .iter()
                    .any(|check| check.subject.starts_with("tracker ")),
                "{report}"
            );
        }
    }

    /// A Markdown project has the GitHub token alone.
    #[test]
    fn a_markdown_projects_secrets_are_its_github_token() {
        let ready = || logged_in(with_harnesses(with_git(FakeSystem::default())));
        let (_dir, markdown) = markdown_project();
        let report = run(&ready(), &markdown);
        assert_eq!(
            line(&report, "secrets").detail,
            "github.com/pitchopp/owlshift (service `owlshift`): GitHub token under \
             `github.com/pitchopp/owlshift/github`"
        );
    }
}
