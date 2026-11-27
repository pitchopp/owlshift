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

use std::path::{MAIN_SEPARATOR, Path, PathBuf};

use owlshift_adapters::forge::{ErrorKind as ForgeErrorKind, Repo};
use owlshift_adapters::harness::{self, Login, tested};
#[cfg(test)]
use owlshift_adapters::tracker::ErrorKind;
use owlshift_adapters::tracker::linear::AdmissionFacts;
use owlshift_adapters::tracker::{Capability, KeyAccount};
use owlshift_contracts::config::{
    Admit, Claims, PersonalConfig, ProjectConfig, States, Takes, Tracker, TrackerKind,
};
use owlshift_contracts::ids::TicketId;
use owlshift_contracts::service::{BY_DAEMON, BuildIdentity};
use owlshift_contracts::{DelegationLevel, Harness};
use owlshift_core::floor::same_account;
use owlshift_core::gate::PassLimit;
use owlshift_core::schedule::{holds_slot, natural_cmp};
use owlshift_core::vocab::Stage;
use owlshift_platform::keychain::SERVICE;
use owlshift_platform::sandbox::{BWRAP_APPARMOR_PROFILE, SandboxError};
use owlshift_platform::service::{Kind, LABEL, SYSTEMD_UNIT};
use serde_json::Value;

use crate::codex_machine::{self, Declared, FULL_ACCESS};
use crate::config::{Effective, FileState, OWLSHIFT_VERSION, PROJECT_FILE, exit_text};
use crate::daemon::requests;
use crate::daemon::upgrade::{Binary, supersedes};
use crate::executor::harness::CLAUDE_AGENT_ACCOUNT;
use crate::executor::{
    MINIMUM_GIT_VERSION, git_floor_reason, git_install_advice, git_recheck_note,
};
use crate::forge::{AppGrants, GITHUB_TOKEN_HELP, sign_in_notice};
use crate::init::TEAM_PAGE;
use crate::install::ServiceState;
use crate::project::ProjectDirs;
#[cfg(test)]
use crate::secrets::TrackerAccounts;
use crate::secrets::{self, ForgeSecret, Source, TrackerSecrets};
use crate::served;
use crate::system::{
    DataDirSource, DefaultBranch, DefaultBranchFile, MarkerState, RunError, Running, RunningBuilds,
    StatesError, System, TokenTryError, exact_version_of, older_than, version_of,
};
#[cfg(unix)]
use crate::system::{SentinelProbe, SentinelStatus};
use crate::ticket_ref::RefState;
use crate::tracker::Operator;
#[cfg(test)]
use crate::tracker::{LINEAR_ACCOUNT, LINEAR_APP_ID_ACCOUNT, LINEAR_APP_SECRET_ACCOUNT};
use crate::ui::probe::Probe;

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
    pub(crate) fn run(command: &str) -> Self {
        Self::Run {
            command: command.to_owned(),
            note: None,
        }
    }

    pub(crate) fn run_noting(command: &str, note: &str) -> Self {
        Self::Run {
            command: command.to_owned(),
            note: Some(note.to_owned()),
        }
    }

    pub(crate) fn act(text: impl Into<String>) -> Self {
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
    pub(crate) fn ok(section: Section, subject: &str, detail: impl Into<String>) -> Self {
        Self::new(section, subject, Status::Ok, detail)
    }

    pub(crate) fn info(section: Section, subject: &str, detail: impl Into<String>) -> Self {
        Self::new(section, subject, Status::Info, detail)
    }

    pub(crate) fn warn(
        section: Section,
        subject: &str,
        detail: impl Into<String>,
        why: String,
    ) -> Self {
        Self {
            why: Some(why),
            ..Self::new(section, subject, Status::Warn, detail)
        }
    }

    /// A failure always says why it matters and how to fix it.
    pub(crate) fn fail(
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
pub(crate) const DOES_NOT_BLOCK: &str = "Does not block `owlshift do`:";

/// Runs every check against the host and the loaded configuration, this
/// binary's build being `own` (OWL-462).
pub fn run(system: &dyn System, config: &Effective, own: &BuildIdentity) -> Report {
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
    checks.extend(caches_check(system, config, home));
    checks.extend(run_caches_check(system, config, home));
    checks.extend(served_check(system, config));
    checks.extend(service_pause_check(system, config));
    checks.push(file_check("personal config", &config.personal, home));
    checks.push(tracker_check(config));
    let stored = project_secrets(system, config);
    // The folder a fix that runs `owlshift init` names, the one the
    // service's pause names (OWL-650, OWL-658).
    let folder = init_folder(system, config, stored.as_ref());
    checks.extend(stored.as_ref().map(|found| secrets_check(found, &folder)));
    checks.extend(
        stored
            .as_ref()
            .and_then(|found| forge_access_check(system, config, found)),
    );
    // The GitHub App's grants, read once for the lines that need them
    // (OWL-445, OWL-513, OWL-665).
    let grants = stored
        .as_ref()
        .and_then(|found| app_grants(system, config, found));
    checks.extend(
        stored
            .as_ref()
            .zip(grants.as_ref())
            .and_then(|(found, grants)| merge_permission_check(config, found, grants)),
    );
    checks.extend(
        stored
            .as_ref()
            .zip(grants.as_ref())
            .and_then(|(found, grants)| rerun_permission_check(config, found, grants)),
    );
    checks.extend(
        stored
            .as_ref()
            .zip(grants.as_ref())
            .and_then(|(found, grants)| gate_status_permission_check(config, found, grants)),
    );
    let accounts = match &stored {
        Some(Ok(found)) => found.tracker.as_ref().map(|tracker| &tracker.accounts),
        _ => None,
    };
    // The tracker's own lines, for a loaded project only.
    let tracker = match &config.project {
        FileState::Loaded {
            config: project, ..
        } => Some((crate::tracker::of(project.tracker.kind), project)),
        _ => None,
    };
    let repo = match &stored {
        Some(Ok(found)) => Some(&found.repo),
        _ => None,
    };
    let doctored =
        tracker.map(|(kind, project)| kind.doctor(system, project, accounts, repo, &folder));
    let facts = doctored.and_then(|doctored| {
        checks.extend(doctored.lines);
        doctored.facts
    });
    let words = tracker.map_or(&crate::tracker::LINEAR_WORDS, |(kind, _)| {
        kind.admission_words()
    });
    checks.extend(drain_admission_check(config, facts.as_ref(), words));
    checks
        .extend(tracker.and_then(|(kind, project)| kind.proposals_inbox(project, facts.as_ref())));
    let operator = tracker.and_then(|(kind, project)| {
        project.people.as_ref()?;
        if project.tracker.kind == TrackerKind::Markdown {
            return Some(personal_operator(&config.personal));
        }
        kind.operator(system, project, accounts, repo)
    });
    checks.extend(people_check(config, operator.as_ref()));
    checks.extend(team_gesture_check(config));
    checks.extend(claims_check(config));
    checks.extend(unread_claims_check(system, config));
    checks.extend(markdown_takes_check(config, repo));
    checks.extend(forge_reviewers_check(config));
    checks.extend(resolver_check(config));
    checks.extend(codex_reviewer_check(system, config));
    checks.extend(review_fallback_check(system, config));
    checks.extend(usage_check(system, config));
    checks.extend(budget_check(config));
    checks.extend(drain_slot_check(system, config));
    checks.extend(quarantine_check(system, config, home));
    checks.extend(system.service().map(|state| service_check(&state, home)));
    if let Some((data, _)) = system.data_dir() {
        let port = crate::ui::probe::port_to_check(&data, &config.personal);
        let log = shown(&crate::daemon::service_dir(&data).join("daemon.log"), home);
        checks.extend(
            system
                .ui(&data, port)
                .map(|probe| ui_check(&probe, port, &log)),
        );
    }
    checks.extend(
        system
            .running_builds()
            .and_then(|running| builds_check(system, running, own, home)),
    );
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
        // The trial run opens no run cache: kept for the match.
        SandboxError::Refused(why) => (why.clone(), vec![Step::act("Do what it says")]),
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

/// The step that fixes a keychain doctor cannot read: the words the
/// service's pause gives for one its watch cannot read (OWL-658).
pub(crate) const KEYCHAIN_FIX: &str = secrets::UNLOCK_KEYCHAIN;

/// The harnesses doctor checks (scenario S15): Claude Code, which runs every
/// role, and Codex only when the personal file declares it (OWL-347) or the
/// project adds the opt-in Codex reviewer (OWL-348). Codex that is not ready
/// is only a warning: see [`codex_not_ready_is_a_warning`].
fn required_harnesses(config: &Effective) -> Vec<Harness> {
    let codex = matches!(
        &config.personal,
        FileState::Loaded { config, .. } if config.harnesses.codex.is_some()
    ) || codex_reviewer_asked(config)
        || review_fallback(config).is_some();
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
             is ready, and runs without it otherwise. A `fallback = \"codex\"` under \
             `[harnesses.claude]` runs the review roles on it while Claude Code is at its usage \
             limit."
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

/// The cache folders the project file lists and the personal file does not
/// allow for its repository (OWL-593), which make the project file invalid
/// on this machine: the line gives the personal file's lines that allow
/// them, where the `project config` line's error names only the key. The
/// file is read again, its load having kept only the error; no line when it
/// loaded, does not parse, lists no refused folder, or when the personal
/// file cannot be read, its own line failing then.
fn caches_check(system: &dyn System, config: &Effective, home: Option<&Path>) -> Option<Check> {
    let FileState::Invalid { path, .. } = &config.project else {
        return None;
    };
    let text = std::fs::read_to_string(path).ok()?;
    let project = ProjectConfig::parse(&text).ok()?;
    let repo = system.origin_repo(path.parent().unwrap_or(path)).ok();
    let allowed = config.allowed_caches(repo.as_ref());
    let refused = crate::init::caches_to_allow(project.stack.cache_folders(), allowed.as_deref());
    if refused.is_empty() {
        return None;
    }
    let personal = match &config.personal {
        FileState::Loaded { path, .. } | FileState::Absent(path) => shown(path, home),
        _ => "the personal configuration".to_owned(),
    };
    let listed: Vec<String> = refused.iter().map(|folder| format!("{folder:?}")).collect();
    let scope = match &repo {
        Some(repo) => format!("github.com/{repo}"),
        None => "this repository".to_owned(),
    };
    Some(Check::fail(
        Section::Project,
        "caches",
        format!(
            "{} in {PROJECT_FILE}'s `stack.caches`, not allowed for {scope} in the personal \
             configuration",
            listed.join(", ")
        ),
        "No run and no gate starts while the project lists a folder of the home this machine \
         does not allow: the setup, the agents and the gate read it, read-only.",
        vec![Step::act(crate::init::allow_caches_advice(
            repo.as_ref(),
            &refused,
            &personal,
        ))],
    ))
}

/// The caches the project's runs write (OWL-617): the tree's path, what it
/// holds and that deleting it empties it. A line when the loaded project
/// file declares run caches or the tree is there; none when the
/// repository is unknown. A failure where no run could start: no cache
/// directory, or a tree on a path holding a space.
fn run_caches_check(system: &dyn System, config: &Effective, home: Option<&Path>) -> Option<Check> {
    const SUBJECT: &str = "run caches";
    let FileState::Loaded { path, config, .. } = &config.project else {
        return None;
    };
    let declared = !config.stack.run_caches.is_empty();
    let repo = system.origin_repo(path.parent().unwrap_or(path)).ok()?;
    let why = "No run and no gate starts while the caches `stack.run_caches` declares cannot be \
               made: the setup, the agents and the gate write them.";
    let fix = || {
        vec![Step::act(
            "Set OWLSHIFT_CACHE_DIR to an absolute path without a space",
        )]
    };
    let Some(cache_dir) = system.run_cache_dir() else {
        return declared.then(|| {
            Check::fail(
                Section::Project,
                SUBJECT,
                "the project declares `stack.run_caches`, and this system has no cache directory",
                why,
                fix(),
            )
        });
    };
    let tree = crate::run_caches::tree(&cache_dir, &repo);
    let shown_tree = shown(&tree, home);
    if declared && let Err(error) = crate::run_caches::check_tree(&tree) {
        return Some(Check::fail(Section::Project, SUBJECT, error, why, fix()));
    }
    if !tree.is_dir() {
        return declared.then(|| {
            Check::info(
                Section::Project,
                SUBJECT,
                format!("{shown_tree}, not made yet: the next run makes it"),
            )
        });
    }
    let size = crate::run_caches::size(&tree, crate::run_caches::SIZE_WALK_LIMIT);
    let held = format!(
        "{}{}",
        if size.complete { "" } else { "at least " },
        crate::run_caches::shown_size(size.bytes)
    );
    Some(Check::info(
        Section::Project,
        SUBJECT,
        if declared {
            format!(
                "{shown_tree} holds {held}; deleting it empties it, the next run makes it again"
            )
        } else {
            format!(
                "{shown_tree} holds {held}, and {PROJECT_FILE} declares no run cache: delete it to \
                 free the space"
            )
        },
    ))
}

/// The project file the service runs this checkout's project on, when it
/// serves it (OWL-434): the one kept from the default branch, never this
/// working tree's, which the other lines check for the commands typed here.
/// No line for a project the service does not serve.
fn served_check(system: &dyn System, config: &Effective) -> Option<Check> {
    const SUBJECT: &str = "served project file";
    let (FileState::Loaded { path, .. }
    | FileState::Invalid { path, .. }
    | FileState::Absent(path)) = &config.project
    else {
        return None;
    };
    let repo = system.origin_repo(path.parent()?).ok()?;
    let (data, _) = system.data_dir()?;
    let record = match served::standing(&data, &repo) {
        Ok(Some(record)) => record,
        Ok(None) => return None,
        Err(error) => {
            return Some(Check::warn(
                Section::Project,
                SUBJECT,
                format!("not read: {error}"),
                format!(
                    "{DOES_NOT_BLOCK} it says which file the service runs this project on, which \
                     `owlshift start` writes and its watch keeps."
                ),
            ));
        }
    };
    let fix = format!(
        "{DOES_NOT_BLOCK} the service keeps running this project on the file it kept until \
         one is accepted: fix {PROJECT_FILE} and merge it to the default branch."
    );
    Some(match (&record.kept, &record.refused) {
        (Some(kept), None) => Check::ok(
            Section::Project,
            SUBJECT,
            format!(
                "the service runs it on {}, not this working tree",
                served::kept_name(kept)
            ),
        ),
        (Some(kept), Some(refused)) => Check::warn(
            Section::Project,
            SUBJECT,
            format!(
                "the service runs it on {}, not this working tree; the default branch's is \
                 refused since {}: {}",
                served::kept_name(kept),
                refused.since,
                refused.reason
            ),
            fix,
        ),
        (None, Some(refused)) => Check::warn(
            Section::Project,
            SUBJECT,
            format!("not served: {}", refused.reason),
            fix,
        ),
        (None, None) => Check::info(
            Section::Project,
            SUBJECT,
            "registered with `owlshift start`; its watch has read no project file yet",
        ),
    })
}

/// The folder `owlshift init` signs in to GitHub again from for the project
/// of `repo` (OWL-650), as its watch picks it: the checkout the list of
/// served projects registers, and `true`, when the service serves it; the
/// project file's folder otherwise, and `false`. The `true` is the notice's
/// last sentence: the service picks the project up again by itself.
fn sign_in_folder(system: &dyn System, config: &Effective, repo: &Repo) -> (PathBuf, bool) {
    let registered = system.data_dir().and_then(|(data, _)| {
        served::Registry::in_data_dir(&data)
            .find(repo)
            .ok()
            .flatten()
    });
    match registered {
        Some(entry) => (PathBuf::from(entry.checkout), true),
        None => (project_folder(config), false),
    }
}

/// The project file's folder, or `.` when there is none.
fn project_folder(config: &Effective) -> PathBuf {
    let (FileState::Loaded { path, .. }
    | FileState::Invalid { path, .. }
    | FileState::Absent(path)) = &config.project
    else {
        return PathBuf::from(".");
    };
    path.parent()
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
}

/// The folder the fixes that run `owlshift init` name (OWL-658): the
/// [`sign_in_folder`] of the project's repository when its secrets were
/// resolved, the project file's folder otherwise.
fn init_folder(
    system: &dyn System,
    config: &Effective,
    stored: Option<&Result<ProjectSecrets, Unresolved>>,
) -> PathBuf {
    match stored {
        Some(Ok(found)) => sign_in_folder(system, config, &found.repo).0,
        _ => project_folder(config),
    }
}

/// The step that signs in to GitHub again from `folder` (OWL-650).
fn sign_in_step(folder: &Path) -> Step {
    Step::run_noting(
        "owlshift init",
        &format!("in {}, the folder the reason names", folder.display()),
    )
}

/// The pause the service wrote on this checkout's project because its watch
/// could not start (OWL-650): the project waits for you, and its reason says
/// what to do. A failure, since none of its tickets moves until a person
/// acts. No line without the daemon's pause: a person's is theirs.
fn service_pause_check(system: &dyn System, config: &Effective) -> Option<Check> {
    const SUBJECT: &str = "service pause";
    let (FileState::Loaded { path, .. }
    | FileState::Invalid { path, .. }
    | FileState::Absent(path)) = &config.project
    else {
        return None;
    };
    let repo = system.origin_repo(path.parent()?).ok()?;
    let (data, _) = system.data_dir()?;
    let pause = requests::pause(&ProjectDirs::new(&data, &repo))??;
    if pause.by != BY_DAEMON {
        return None;
    }
    let (folder, resumes) = sign_in_folder(system, config, &repo);
    let fix = if pause.why == sign_in_notice(&repo, &folder, resumes) {
        sign_in_step(&folder)
    } else {
        Step::act(
            "Fix what the reason names; the service tries the project every minute and lifts \
             its pause once its watch runs",
        )
    };
    Some(Check::fail(
        Section::Project,
        SUBJECT,
        pause.why,
        "The service starts none of this project's tickets while it waits. Once the reason \
         is fixed, it tries the project again within a minute, and lifts the pause once that \
         watch has run a minute more.",
        vec![fix],
    ))
}

/// The configured tracker's adapter and the capabilities it declares
/// (architecture section 6). A required capability this build does not
/// implement yet is a warning, not a failure: `owlshift do` needs only to
/// read tickets and comments and move the visible stage, and refusing a project is `init`'s job. The
/// check reads the adapter's constants: it opens neither the tracker nor the
/// keychain; the tracker's own lines do, for a Linear or GitHub Issues project
/// ([`crate::tracker::Kind::doctor`]).
fn tracker_check(config: &Effective) -> Check {
    const SUBJECT: &str = "tracker";
    let FileState::Loaded { config, .. } = &config.project else {
        return Check::info(
            Section::Project,
            SUBJECT,
            "no project configuration, no adapter to check",
        );
    };
    let tracker = crate::tracker::of(config.tracker.kind);
    let (kind, declared) = (tracker.name(), tracker.capabilities());
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

/// The operator of a Markdown project, whose tracker has no key: the
/// personal file's `identity.tracker` (build plan "P10 · Teams", "The
/// operator").
fn personal_operator(personal: &FileState<PersonalConfig>) -> Operator {
    let FileState::Loaded { config, .. } = personal else {
        return Err("no personal configuration is loaded".to_owned());
    };
    let id = config
        .identity
        .tracker
        .clone()
        .ok_or("the personal configuration sets no `identity.tracker`")?;
    Ok(KeyAccount {
        name: id.clone(),
        id,
        app: false,
        workspace: String::new(),
    })
}

/// The project's team as `doctor`'s team lines name it (OWL-590): the
/// people `[people]` lists in the working tree, or, without the table, the
/// accounts `[tracker].admitters` lists. The `people`, `team gesture` and
/// `claims` lines count and name it the same way, so that they read as one
/// story with the team adoption page they point to ([`TEAM_PAGE`]).
struct Team {
    count: usize,
    table: bool,
}

impl std::fmt::Display for Team {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (list, one, many) = if self.table {
            ("`[people]`", "person", "people")
        } else {
            ("`[tracker].admitters`", "account", "accounts")
        };
        let noun = if self.count == 1 { one } else { many };
        write!(f, "{list} lists {} {noun}", self.count)
    }
}

/// The team of `config`, as [`Team`] names it.
fn team_size(config: &ProjectConfig) -> Team {
    match &config.people {
        Some(people) => Team {
            count: people.len(),
            table: true,
        },
        None => Team {
            count: config.tracker.admitters.as_ref().map_or(0, Vec::len),
            table: false,
        },
    }
}

/// How each team line's fix ends: the project file changes through a pull
/// request, and the team adoption page says why.
fn team_fix(change: &str) -> String {
    format!(
        "{DOES_NOT_BLOCK} {change}, through a pull request to the default branch. The team \
         adoption page says why: {TEAM_PAGE}"
    )
}

/// The project's people (OWL-582, build plan "P10 · Teams", "People"):
/// how many `[people]` lists in the working tree, and whether this
/// machine's operator, as [`crate::tracker::Kind::operator`] read it, is
/// one of them, a warning when not. An operator that could not be read,
/// or a key that is an app's, says so and warns of nothing. None without
/// the table or a project configuration.
fn people_check(config: &Effective, operator: Option<&Operator>) -> Option<Check> {
    const SUBJECT: &str = "people";
    let FileState::Loaded { config, .. } = &config.project else {
        return None;
    };
    config.people.as_ref()?;
    let count = format!(
        "{} in the working tree (watch reads the base commit)",
        team_size(config)
    );
    let ok = |detail: String| Check::ok(Section::Project, SUBJECT, detail);
    Some(match operator {
        Some(Ok(account)) if account.app => ok(format!(
            "{count}; the tracker key is the account of an app, {:?}, so this machine's \
             operator was not read",
            account.name
        )),
        Some(Ok(account)) => match config.handle_of(&account.id) {
            Some(handle) => ok(format!("{count}; this machine's operator is {handle}")),
            None => Check::warn(
                Section::Project,
                SUBJECT,
                format!(
                    "{count}, and this machine's operator, {:?} ({:?}), is not one of them",
                    account.name, account.id
                ),
                team_fix(&format!(
                    "the people of the project file are who admit, decide and review. Add this \
                     machine's operator as `[people.<handle>]` with `tracker = {:?}`, the entry \
                     `owlshift init` prints",
                    account.id
                )),
            ),
        },
        Some(Err(reason)) => ok(format!(
            "{count}; this machine's operator could not be read: {reason}"
        )),
        None => ok(format!("{count}; this machine's operator was not read")),
    })
}

/// A state gesture in a project file that lists several people (OWL-583,
/// build plan "P10 · Teams", "The gesture on a team"): its account is
/// whoever moved the ticket to the ready column, often the person who
/// grooms the backlog, whose runner then takes it. A note pointing to a
/// label, which each developer puts on their own tickets. Several people:
/// [`team_size`] counts two or more.
fn team_gesture_check(config: &Effective) -> Option<Check> {
    let FileState::Loaded { config, .. } = &config.project else {
        return None;
    };
    if !matches!(config.tracker.admit, Admit::State(_)) {
        return None;
    }
    let team = team_size(config);
    (team.count >= 2).then(|| {
        Check::warn(
            Section::Project,
            "team gesture",
            format!(
                "tickets are admitted by a move to the ready state, and {team}: a runner takes \
                 a ticket whose move is its own operator's, often the person who grooms the \
                 backlog"
            ),
            team_fix(
                "on a team, admit with a label each developer puts on their own tickets: \
                 `admit = { label = \"agent\" }` under `[tracker]`",
            ),
        )
    })
}

/// Claims kept local in a project file that lists several people (OWL-586,
/// build plan "P10 · Teams", "Claims"): only the ticket locks of one machine
/// keep a ticket to one runner, so two people's runners, or one person's two
/// machines, can work one ticket. Several people as [`team_gesture_check`]
/// counts them. A warning, read from the working tree; the runner reads the
/// base commit.
fn claims_check(config: &Effective) -> Option<Check> {
    let FileState::Loaded { config, .. } = &config.project else {
        return None;
    };
    if config.policy.claims() != Claims::Local {
        return None;
    }
    let team = team_size(config);
    (team.count >= 2).then(|| {
        Check::warn(
            Section::Project,
            "claims",
            format!(
                "claims are local, and {team}: two machines can work one ticket, each holding \
                 only its own ticket locks"
            ),
            team_fix(
                "on a team, claim tickets on the git remote: `claims = \"remote\"` under \
                 `[policy]`",
            ),
        )
    })
}

/// The claims of other runners this binary cannot read, under `claims =
/// "remote"` (OWL-587; build plan "Other runners' tickets", "A claim this
/// binary cannot read"), as the dedicated checkout last fetched them: each
/// is never taken and holds nothing this runner can name. A warning that
/// says to upgrade when one is of a later format; nothing when all read or
/// none was fetched.
fn unread_claims_check(system: &dyn System, config: &Effective) -> Option<Check> {
    const SUBJECT: &str = "claims";
    let FileState::Loaded {
        path,
        config: project,
        ..
    } = &config.project
    else {
        return None;
    };
    if project.policy.claims() != Claims::Remote {
        return None;
    }
    let unread = match system.unread_claims(path.parent()?) {
        Ok(Some(unread)) if !unread.is_empty() => unread,
        Ok(_) => return None,
        Err(why) => {
            return Some(Check::info(
                Section::Project,
                SUBJECT,
                format!("the claims of other runners were not read: {why}"),
            ));
        }
    };
    let tickets: Vec<&str> = unread.iter().map(|(ticket, _)| ticket.as_str()).collect();
    let newer = unread.iter().any(|(_, newer)| *newer);
    let fix = if newer {
        "upgrade Owlshift: a later version wrote them, and this one cannot read them".to_owned()
    } else {
        format!(
            "their claims on origin do not read: the people whose runners hold {} release them \
             or claim them again",
            tickets.join(", ")
        )
    };
    Some(Check::warn(
        Section::Project,
        SUBJECT,
        format!(
            "{} claimed on origin by a claim this runner cannot read: never taken here, and \
             holding nothing it can name",
            tickets.join(", ")
        ),
        format!("{DOES_NOT_BLOCK} {fix}."),
    ))
}

/// A Markdown project whose personal file names no `identity.tracker`
/// (OWL-583): with no operator to compare, its drain takes every admitted
/// ticket, as before P10, even under `takes = "mine"`. Said once, as a note.
fn markdown_takes_check(config: &Effective, repo: Option<&Repo>) -> Option<Check> {
    let FileState::Loaded {
        config: project, ..
    } = &config.project
    else {
        return None;
    };
    if project.tracker.kind != TrackerKind::Markdown {
        return None;
    }
    let personal = match &config.personal {
        FileState::Loaded { config, .. } => Some(config),
        _ => None,
    };
    let key = repo.map(|repo| format!("github.com/{repo}"));
    let takes = personal.map_or(Takes::Mine, |personal| personal.takes_for(key.as_deref()));
    let named = personal.and_then(|personal| personal.identity.tracker.as_deref());
    (takes == Takes::Mine && named.is_none_or(|id| id.trim().is_empty())).then(|| {
        Check::info(
            Section::Project,
            "takes",
            "the personal configuration sets no `identity.tracker`, the operator of a Markdown \
             project, so the drain takes every admitted ticket; set it to take only the tickets \
             whose author it is",
        )
    })
}

/// Whose reviews on Owlshift's pull requests are read (OWL-307, build plan
/// "P5 · Quality pipeline", "Contracts and formats"): the logins
/// `[forge].reviewers` lists, or the people of `[people]` with a forge
/// login (OWL-582), read from the working tree here, while watch reads the
/// base commit. When none is listed no review is read, which blocks
/// nothing: a warning. None without a project configuration.
fn forge_reviewers_check(config: &Effective) -> Option<Check> {
    const SUBJECT: &str = "forge reviewers";
    let FileState::Loaded { config, .. } = &config.project else {
        return None;
    };
    let reviewers = config
        .forge
        .as_ref()
        .map_or(0, |forge| forge.reviewers.len());
    // With `[people]`, the reviewers are its people with a forge login.
    if config.people.is_some() {
        return Some(if reviewers > 0 {
            Check::ok(
                Section::Project,
                SUBJECT,
                format!(
                    "{reviewers} reviewer(s) from `[people]` in the working tree (watch reads \
                     the base commit): a review of the ticket's decider on a delivered pull \
                     request goes back to Build"
                ),
            )
        } else {
            Check::warn(
                Section::Project,
                SUBJECT,
                "no person of `[people]` has a forge login: no review on Owlshift's pull \
                 requests is read",
                format!(
                    "{DOES_NOT_BLOCK} to have the decider's reviews on a delivered pull request \
                     handled, give each person who reviews their forge login, as `forge = \
                     \"login\"` under their `[people.<handle>]`."
                ),
            )
        });
    }
    Some(match &config.forge {
        Some(forge) if !forge.reviewers.is_empty() => Check::ok(
            Section::Project,
            SUBJECT,
            format!(
                "{} reviewer(s) in the working tree (watch reads the base commit): a review of \
                 the ticket's decider on a delivered pull request goes back to Build",
                forge.reviewers.len()
            ),
        ),
        // No `[forge]` table, or one that sets only other keys such as `merge`.
        _ => Check::warn(
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

/// The harness the personal file names as Claude Code's fallback
/// (`[harnesses.claude].fallback`), if any (OWL-406).
fn review_fallback(config: &Effective) -> Option<Harness> {
    match &config.personal {
        FileState::Loaded { config, .. } => config
            .harnesses
            .claude
            .as_ref()
            .and_then(|claude| claude.fallback),
        _ => None,
    }
}

/// The fallback of `[harnesses.claude]` (OWL-406): which roles it serves,
/// the review roles alone, and whether it can serve here, by the check each
/// command makes at its start. A warning when it cannot: the review then
/// waits for Claude Code's reset like any other run. None when the personal
/// file names none.
fn review_fallback_check(system: &dyn System, config: &Effective) -> Option<Check> {
    const SUBJECT: &str = "review fallback";
    let fallback = review_fallback(config)?;
    let name = harness_name(fallback);
    let scope = "used by the review roles only: Design review's and Verify's reviewers run on it \
                 while Claude Code is at its usage limit; the authoring roles, the arbiter, the \
                 resolver, intake and the answer check wait for the reset";
    let ready = match fallback {
        Harness::Codex => codex_machine::critic_readiness(
            system.locate("codex"),
            system.codex_login_file(),
            |path| system.is_file(path),
            system.read_text_file(Path::new(codex_machine::REQUIREMENTS)),
        )
        .map(|_| ()),
        // Claude Code never falls back to itself (the personal file refuses it).
        Harness::Claude => Ok(()),
    };
    Some(match ready {
        Ok(()) => Check::ok(Section::Project, SUBJECT, format!("{name}, {scope}")),
        Err(why) => Check::warn(
            Section::Project,
            SUBJECT,
            format!("{name}, {scope}, but it cannot serve here: {why}"),
            format!(
                "{DOES_NOT_BLOCK} a review at Claude Code's limit waits for its reset. The \
                 `codex` lines say how to get Codex ready."
            ),
        ),
    })
}

/// Each harness's usage cap and the shares its runs reported of its usage
/// windows (OWL-405, architecture section 5, "Usage limits"), read from the
/// machine's usage files: a note, which blocks nothing. A window at the cap
/// that has not reset holds the harness. None when no harness has a cap or a
/// window on record.
fn usage_check(system: &dyn System, config: &Effective) -> Option<Check> {
    let caps = crate::usage::caps_of(&config.personal);
    let (dir, _) = system.data_dir()?;
    let now = crate::usage::ms(jiff::Timestamp::now());
    let mut lines = Vec::new();
    for harness in [Harness::Claude, Harness::Codex] {
        let cap = caps
            .iter()
            .find(|(h, _)| *h == harness)
            .map(|(_, cap)| *cap);
        let name = harness_name(harness);
        let file = dir
            .join("usage")
            .join(format!("{}.json", crate::usage::key(harness)));
        let text = match system.read_text_file(&file) {
            Ok(text) => text,
            Err(why) => {
                lines.push(format!("{name}: its usage file was not read: {why}"));
                continue;
            }
        };
        let windows = match text
            .as_deref()
            .map(owlshift_contracts::usage::HarnessUsage::parse)
        {
            Some(Ok(usage)) => usage.state().windows,
            Some(Err(_)) => {
                lines.push(format!("{name}: its usage file does not read"));
                continue;
            }
            None => Default::default(),
        };
        if cap.is_none() && windows.is_empty() {
            continue;
        }
        let cap_text = cap.map_or_else(|| "no cap".to_owned(), |cap| format!("cap {cap}%"));
        let seen = if windows.is_empty() {
            "no window reported yet".to_owned()
        } else {
            windows
                .iter()
                .map(|(window, w)| {
                    let reset = crate::usage::at(w.resets_at_ms);
                    let share = (w.share * 100.0).round();
                    let held = cap.is_some_and(|cap| {
                        owlshift_core::usage::at_cap(w.share, cap) && w.resets_at_ms > now
                    });
                    if w.resets_at_ms <= now {
                        format!("{window} {share}% (reset at {reset})")
                    } else if held {
                        format!("{window} {share}% (holds the harness until it resets at {reset})")
                    } else {
                        format!("{window} {share}% (resets at {reset})")
                    }
                })
                .collect::<Vec<_>>()
                .join(", ")
        };
        lines.push(format!("{name}, {cap_text}: {seen}"));
    }
    if lines.is_empty() {
        return None;
    }
    Some(Check::info(
        Section::Project,
        "usage",
        format!(
            "{}; the latest share each run reported, a harness whose runs report none is never \
             held by its cap",
            lines.join("; ")
        ),
    ))
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
    words: &crate::tracker::AdmissionWords,
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
            fixes.push(words.delegation_fix);
        }
        Admit::Label(_) | Admit::State(_) => {}
    }
    if tracker.admitters.is_none() {
        problems.push("`admitters` is not set under `[tracker]`".to_owned());
        fixes.push(words.admitters_unset_fix);
    }
    let checked = matches!(facts, Some(Ok(_)));
    match facts {
        Some(Ok(found)) => {
            let mut missing_label = |what: &str, name: &str| {
                if !found.labels.iter().any(|l| l == name) {
                    problems.push(format!("the {what} label {name:?} {}", words.label_missing));
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
                fixes.push(words.label_fix);
            }
            let mut accounts = false;
            for account in tracker.admitters.iter().flatten() {
                // Named as the file names it: by handle beside `[people]`.
                let id = config.handle_of(account).unwrap_or(account);
                match found.accounts.iter().find(|a| same_account(&a.id, account)) {
                    None => problems.push(format!("admitter {id:?} {}", words.no_account)),
                    Some(a) if a.app => {
                        problems.push(format!("admitter {id:?} {}", words.app_account));
                    }
                    Some(a) if !a.active => {
                        problems.push(format!("admitter {id:?} {}", words.deactivated));
                    }
                    Some(_) => continue,
                }
                accounts = true;
            }
            if accounts {
                fixes.push(words.accounts_fix);
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
            words.checked
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

/// Each key set under `[tracker].states` and the state it names, keys in
/// alphabetical order. Read from the configuration as it serializes, so a
/// key added later is checked once set and an optional one left unset is
/// not.
pub(crate) fn configured_states(states: &States) -> Vec<(String, String)> {
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

/// The project's tracker and forge secrets as doctor finds them (OWL-308):
/// the repository that names them and what is stored, presence only.
struct ProjectSecrets {
    repo: Repo,
    /// The kind of the project's tracker, which describes its set.
    kind: &'static dyn crate::tracker::Kind,
    /// For a tracker with a set of its own ([`crate::tracker::Kind::secret_set`]).
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
        let tracker = crate::tracker::keychain_set(config, &repo, &mut look)?;
        let forge = secrets::forge(&repo, &mut look)?;
        Ok((tracker, forge))
    })();
    Some(match found {
        Ok((tracker, forge)) => Ok(ProjectSecrets {
            repo,
            kind: crate::tracker::of(config.tracker.kind),
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
    config: &Effective,
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
    // A token only a new sign-in mends names the notice the service shows,
    // its folder and its command (OWL-650), never the keychain.
    let sign_in = |found: &str| {
        let (folder, resumes) = sign_in_folder(system, config, repo);
        Some(Check::fail(
            Section::Project,
            SUBJECT,
            sign_in_notice(repo, &folder, resumes),
            &format!("{why} What doctor found: {found}."),
            vec![sign_in_step(&folder)],
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
        Err(TokenTryError::SignIn(found)) => sign_in(&found),
        // The refresh doctor does not make would renew it (GA4): the same
        // command does, from the same folder.
        Err(TokenTryError::Expired(found)) => {
            let (folder, _) = sign_in_folder(system, config, repo);
            Some(Check::fail(
                Section::Project,
                SUBJECT,
                format!(
                    "not checked: the GitHub App's token for {repo} is past its expiry, and \
                     doctor does not renew it"
                ),
                &format!("{why} What doctor found: {found}."),
                vec![Step::run_noting(
                    "owlshift init",
                    &format!(
                        "in {}: it renews the token, or shows a code to enter on github.com \
                         when GitHub refuses it, and keeps your owlshift.toml, your other stored \
                         keys and your tickets",
                        folder.display()
                    ),
                )],
            ))
        }
        Err(TokenTryError::Forge(error)) => match error.kind {
            ForgeErrorKind::Unauthorized => fail(
                format!("GitHub refused {place}: {error}"),
                format!(
                    "Run `owlshift init --replace-secrets` and paste a valid token: \
                     {GITHUB_TOKEN_HELP}"
                ),
            ),
            // The refresh-failure rule (OWL-384, OWL-650).
            ForgeErrorKind::SignInAgain => sign_in(&error.to_string()),
            ForgeErrorKind::NotFound => fail(
                format!("{place} does not see github.com/{repo}: {error}"),
                format!(
                    "Give the token access to github.com/{repo}: {edit}, after adding the \
                     repository under Repository access.{shared}"
                ),
            ),
            ForgeErrorKind::HeadMoved
            | ForgeErrorKind::RateLimited
            | ForgeErrorKind::Unreachable
            | ForgeErrorKind::Other => Some(Check::warn(
                Section::Project,
                SUBJECT,
                format!("not checked: {error}"),
                format!(
                    "{DOES_NOT_BLOCK} doctor could not ask GitHub whether the token reads \
                         the branches of github.com/{repo}. Without contents (read), a delivery \
                         stops after its push."
                ),
            )),
        },
    }
}

/// Whether the project's GitHub credential can make merge on green's merge
/// (D14, OWL-445; architecture section 5, "Merge on green", "Who merges, on
/// GitHub"), for a project whose `[forge] merge`, read from the working
/// tree here while watch reads the base branch's head, is not `never`: the
/// GitHub App's installation must have granted Contents (read and write),
/// which a read cannot show, so doctor reads the installation's granted
/// permissions. A personal token's cannot be read, nor an installation not
/// found: information. None for `never`, or when the secrets are unresolved
/// or no token is stored: the `secrets` line says so.
fn merge_permission_check(
    config: &Effective,
    found: &Result<ProjectSecrets, Unresolved>,
    grants: &Result<AppGrants, TokenTryError>,
) -> Option<Check> {
    const SUBJECT: &str = "merge on green";
    let FileState::Loaded {
        config: project, ..
    } = &config.project
    else {
        return None;
    };
    let switch = project.forge.as_ref().map(|forge| forge.merge())?;
    if switch == owlshift_core::vocab::MergeSwitch::Never {
        return None;
    }
    let Ok(found) = found else {
        return None;
    };
    let app = github_app(config);
    let repo = &found.repo;
    let why = "watch merges a green pull request through the GitHub App's token, which needs \
               Contents (read and write) on the repository; GitHub refuses the merge \
               otherwise, a final refusal that is notified.";
    Some(match grants {
        Ok(AppGrants::Installed {
            contents_write: true,
            ..
        }) => Check::ok(
            Section::Project,
            SUBJECT,
            format!(
                "the GitHub App's installation on github.com/{repo} grants Contents (read and \
                 write), which watch's merge needs"
            ),
        ),
        Ok(AppGrants::Installed { installation, .. }) => Check::fail(
            Section::Project,
            SUBJECT,
            format!(
                "the GitHub App's installation {installation} on github.com/{repo} has not \
                 granted Contents (read and write), which watch's merge needs"
            ),
            why,
            vec![Step::act(format!(
                "Accept the app's permissions on GitHub, in the installation's settings \
                 (Settings > Applications > Installed GitHub Apps > {} > Configure), then run \
                 `owlshift doctor` again",
                app.slug
            ))],
        ),
        Ok(AppGrants::NotInstalled) => Check::info(
            Section::Project,
            SUBJECT,
            format!(
                "not checked: no installation of the GitHub App lists github.com/{repo}; \
                 install it from {}",
                app.install_link()
            ),
        ),
        Ok(AppGrants::PersonalToken) => Check::info(
            Section::Project,
            SUBJECT,
            "not checked: GitHub does not say what a personal token may do; its merge needs \
             Contents (read and write), and a merge GitHub refuses is notified",
        ),
        Err(TokenTryError::Credential(reason)) => {
            Check::info(Section::Project, SUBJECT, format!("not checked: {reason}"))
        }
        Err(TokenTryError::SignIn(_)) => Check::info(
            Section::Project,
            SUBJECT,
            "not checked: the GitHub App's token needs a new sign-in; the forge access line \
             says how",
        ),
        Err(TokenTryError::Expired(_)) => Check::info(
            Section::Project,
            SUBJECT,
            "not checked: the GitHub App's token is past its expiry; the forge access line \
             says how",
        ),
        Err(TokenTryError::Forge(error)) => Check::warn(
            Section::Project,
            SUBJECT,
            format!("not checked: {error}"),
            format!(
                "{DOES_NOT_BLOCK} doctor could not read the GitHub App's installation on \
                 github.com/{repo}. Watch's merge needs Contents (read and write)."
            ),
        ),
    })
}

/// The GitHub App of the personal file, or Owlshift's own.
fn github_app(config: &Effective) -> crate::forge::GitHubApp {
    let personal = match &config.personal {
        FileState::Loaded { config, .. } => Some(config),
        _ => None,
    };
    crate::forge::GitHubApp::from_personal(personal)
}

/// What the project's GitHub credential was granted, for the lines that
/// read it ([`merge_permission_check`], [`rerun_permission_check`],
/// [`gate_status_permission_check`]): read
/// once, only when a project file is loaded and a token is stored.
fn app_grants(
    system: &dyn System,
    config: &Effective,
    found: &Result<ProjectSecrets, Unresolved>,
) -> Option<Result<AppGrants, TokenTryError>> {
    let FileState::Loaded { .. } = &config.project else {
        return None;
    };
    let Ok(found) = found else {
        return None;
    };
    found.forge.token?;
    Some(system.github_app_grants(&found.repo, &github_app(config).slug))
}

/// The `check re-runs` line (OWL-513; architecture section 5, "The
/// automatic re-run"): watch re-runs a red check's failed GitHub Actions
/// jobs once per head through the GitHub App's token, which needs Actions
/// (read and write). Without it every re-run is refused and the red check
/// gets a fix run as before, so a missing grant is a warning that blocks
/// nothing. A personal token's cannot be read, nor an installation not
/// found: information.
fn rerun_permission_check(
    config: &Effective,
    found: &Result<ProjectSecrets, Unresolved>,
    grants: &Result<AppGrants, TokenTryError>,
) -> Option<Check> {
    const SUBJECT: &str = "check re-runs";
    let Ok(found) = found else {
        return None;
    };
    let app = github_app(config);
    let repo = &found.repo;
    Some(match grants {
        Ok(AppGrants::Installed {
            actions_write: true,
            ..
        }) => Check::ok(
            Section::Project,
            SUBJECT,
            format!(
                "the GitHub App's installation on github.com/{repo} grants Actions (read and \
                 write), which watch's re-run of a red check needs"
            ),
        ),
        Ok(AppGrants::Installed { installation, .. }) => Check {
            fix: vec![Step::act(format!(
                "Accept the app's permissions on GitHub, in the installation's settings \
                 (Settings > Applications > Installed GitHub Apps > {} > Configure), then run \
                 `owlshift doctor` again",
                app.slug
            ))],
            ..Check::warn(
                Section::Project,
                SUBJECT,
                format!(
                    "the GitHub App's installation {installation} on github.com/{repo} has not \
                     granted Actions (read and write): watch cannot re-run a failed check; a red \
                     check gets a fix run as before"
                ),
                format!(
                    "{DOES_NOT_BLOCK} watch re-runs a red check's failed GitHub Actions jobs \
                     once before any fix run, which needs Actions (read and write); GitHub \
                     refuses the re-run otherwise."
                ),
            )
        },
        Ok(AppGrants::NotInstalled) => Check::info(
            Section::Project,
            SUBJECT,
            format!(
                "not checked: no installation of the GitHub App lists github.com/{repo}; \
                 install it from {}",
                app.install_link()
            ),
        ),
        Ok(AppGrants::PersonalToken) => Check::info(
            Section::Project,
            SUBJECT,
            "not checked: GitHub does not say what a personal token may do; a re-run needs \
             Actions (read and write), and a red check gets a fix run when GitHub refuses it",
        ),
        Err(TokenTryError::Credential(reason)) => {
            Check::info(Section::Project, SUBJECT, format!("not checked: {reason}"))
        }
        Err(TokenTryError::SignIn(_)) => Check::info(
            Section::Project,
            SUBJECT,
            "not checked: the GitHub App's token needs a new sign-in; the forge access line \
             says how",
        ),
        Err(TokenTryError::Expired(_)) => Check::info(
            Section::Project,
            SUBJECT,
            "not checked: the GitHub App's token is past its expiry; the forge access line \
             says how",
        ),
        Err(TokenTryError::Forge(error)) => Check::warn(
            Section::Project,
            SUBJECT,
            format!("not checked: {error}"),
            format!(
                "{DOES_NOT_BLOCK} doctor could not read the GitHub App's installation on \
                 github.com/{repo}. Watch's re-run of a red check needs Actions (read and \
                 write)."
            ),
        ),
    })
}

/// The `gate status` line (OWL-665; architecture section 5, "The gate's
/// status"), for a project whose `[forge] publish_gate_status`, read from
/// the working tree here while the runner reads the base's, is on: the
/// runner sets the commit status through the GitHub App's token, which
/// needs Commit statuses (read and write). Without it no status is set and
/// the project's CI runs its suite, so a missing grant is a warning that
/// blocks nothing. A personal token's cannot be read, nor an installation
/// not found: information. None when the switch is off, or when the secrets
/// are unresolved or no token is stored: the `secrets` line says so.
fn gate_status_permission_check(
    config: &Effective,
    found: &Result<ProjectSecrets, Unresolved>,
    grants: &Result<AppGrants, TokenTryError>,
) -> Option<Check> {
    const SUBJECT: &str = "gate status";
    let FileState::Loaded {
        config: project, ..
    } = &config.project
    else {
        return None;
    };
    if !project
        .forge
        .as_ref()
        .is_some_and(|forge| forge.publish_gate_status())
    {
        return None;
    }
    let Ok(found) = found else {
        return None;
    };
    let app = github_app(config);
    let repo = &found.repo;
    Some(match grants {
        Ok(AppGrants::Installed {
            statuses_write: true,
            ..
        }) => Check::ok(
            Section::Project,
            SUBJECT,
            format!(
                "the GitHub App's installation on github.com/{repo} grants Commit statuses \
                 (read and write), which the gate's status needs"
            ),
        ),
        Ok(AppGrants::Installed { installation, .. }) => Check {
            fix: vec![Step::act(format!(
                "Accept the app's permissions on GitHub, in the installation's settings \
                 (Settings > Applications > Installed GitHub Apps > {} > Configure), then run \
                 `owlshift doctor` again",
                app.slug
            ))],
            ..Check::warn(
                Section::Project,
                SUBJECT,
                format!(
                    "the GitHub App's installation {installation} on github.com/{repo} has not \
                     granted Commit statuses (read and write): no gate status is set, and the \
                     project's CI runs its whole suite"
                ),
                format!(
                    "{DOES_NOT_BLOCK} forge.publish_gate_status has the runner set the commit \
                     status `owlshift/gate` on each commit it pushes once its gate passed, which \
                     needs Commit statuses (read and write); GitHub refuses it otherwise."
                ),
            )
        },
        Ok(AppGrants::NotInstalled) => Check::info(
            Section::Project,
            SUBJECT,
            format!(
                "not checked: no installation of the GitHub App lists github.com/{repo}; \
                 install it from {}",
                app.install_link()
            ),
        ),
        Ok(AppGrants::PersonalToken) => Check::info(
            Section::Project,
            SUBJECT,
            "not checked: GitHub does not say what a personal token may do; the gate's status \
             needs commit statuses (write), and a status GitHub refuses is a warning",
        ),
        Err(TokenTryError::Credential(reason)) => {
            Check::info(Section::Project, SUBJECT, format!("not checked: {reason}"))
        }
        Err(TokenTryError::SignIn(_)) => Check::info(
            Section::Project,
            SUBJECT,
            "not checked: the GitHub App's token needs a new sign-in; the forge access line \
             says how",
        ),
        Err(TokenTryError::Expired(_)) => Check::info(
            Section::Project,
            SUBJECT,
            "not checked: the GitHub App's token is past its expiry; the forge access line \
             says how",
        ),
        Err(TokenTryError::Forge(error)) => Check::warn(
            Section::Project,
            SUBJECT,
            format!("not checked: {error}"),
            format!(
                "{DOES_NOT_BLOCK} doctor could not read the GitHub App's installation on \
                 github.com/{repo}. The gate's status needs Commit statuses (read and write)."
            ),
        ),
    })
}

/// Which keychain account each tracker and forge secret of the project is
/// read from (OWL-308). A secret read from an account every project of the
/// machine shares is a warning: the project works, on a key or token that
/// may be another project's. No GitHub token fails, as `owlshift do`
/// refuses then, and its fix runs `owlshift init` in `folder`; a missing
/// Linear key is the `tracker states` line's.
fn secrets_check(found: &Result<ProjectSecrets, Unresolved>, folder: &Path) -> Check {
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
    let mut parts = Vec::new();
    let mut shared = false;
    if let Some(tracker) = &found.tracker {
        shared |= tracker.accounts.source == Source::Shared;
        parts.extend(found.kind.secrets_detail(tracker));
    }
    let forge = &found.forge;
    parts.push(match forge.token {
        Some(()) => {
            shared |= forge.account.source == Source::Shared;
            format!(
                "GitHub token under {}",
                secrets::named(&forge.account.token, forge.account.source)
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
            // The sign-in the service's pause names, in the same folder
            // (OWL-658).
            vec![Step::run_noting(
                "owlshift init",
                &format!(
                    "in {}: it signs in to GitHub with a code to enter on github.com; your \
                     owlshift.toml, your other stored keys and your tickets are kept",
                    folder.display()
                ),
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

/// Whether ticket refs hold the project's capacity (OWL-291, OWL-401): one
/// between Ready and Watch, one whose state does not read, or one that
/// cannot be read each holds one of `policy.max_in_flight`, and as many stop
/// every dispatch of `owlshift watch --drain` and every `owlshift do`. A
/// warning, once the capacity is held, naming each and the command that
/// clears it; one a command is working now frees its share when its work
/// ends. Read from the dedicated checkout without the project's lock. Only
/// the identifiers and stages reach the report, never why a ref did not
/// read.
fn drain_slot_check(system: &dyn System, config: &Effective) -> Option<Check> {
    const SUBJECT: &str = "drain slot";
    let FileState::Loaded {
        path,
        config: project,
        ..
    } = &config.project
    else {
        return None;
    };
    let capacity = usize::from(project.policy.max_in_flight());
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
                // Its Build waits for its admission (OWL-448): `why` names
                // what holds its resources.
                RefState::Unadmitted(_) => (
                    format!(
                        "{ticket} at Build, waiting for its resources (`owlshift why {ticket}` \
                         names what holds them)"
                    ),
                    format!(
                        "`owlshift continue {ticket}` to finish its Build or `owlshift forget \
                         {ticket}` to start it over"
                    ),
                ),
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
    let count = held.len();
    let (what, clear): (Vec<String>, Vec<String>) = held
        .into_iter()
        .map(|(_, what, clear)| (what, clear))
        .unzip();
    if count < capacity {
        return Some(Check::ok(
            Section::Project,
            SUBJECT,
            format!("{count} of {capacity} held, by {}", what.join("; ")),
        ));
    }
    Some(Check::warn(
        Section::Project,
        SUBJECT,
        format!("held by {}", what.join("; ")),
        format!(
            "{DOES_NOT_BLOCK} on a ticket that holds it, only on the others: `owlshift watch \
             --drain` dispatches nothing, and `owlshift do` waits, while {capacity} ticket refs \
             (`policy.max_in_flight`) are between Ready and Watch or cannot be read. Unless a \
             command is working on it now, run {}.",
            clear.join("; ")
        ),
    ))
}

/// Whether a run's isolation check left the project quarantined (OWL-368):
/// a failure while the quarantine stands, or while a run's obligation was
/// left behind by a crash or a stop, which the next command or watch pass
/// turns into the quarantine (OWL-400), since every command then refuses the
/// project; information for the runs still in flight, whose ticket's lock is
/// held. Read for a project file that loads or is invalid: the quarantine
/// does not depend on it.
fn quarantine_check(system: &dyn System, config: &Effective, home: Option<&Path>) -> Option<Check> {
    const SUBJECT: &str = "quarantine";
    let (FileState::Loaded { path, .. } | FileState::Invalid { path, .. }) = &config.project else {
        return None;
    };
    let root = path.parent()?;
    let state: MarkerState = match system.quarantine(root) {
        Ok(state) => state,
        Err(why) => {
            return Some(Check::info(
                Section::Project,
                SUBJECT,
                format!("the quarantine was not read: {why}"),
            ));
        }
    };
    let named = |standing: &crate::ticket_run::Standing| match &standing.found.record {
        Ok(record) => record.what.clone(),
        Err(_) => format!("{} (unreadable)", standing.found.name),
    };
    let running: Vec<String> = state.running().map(named).collect();
    let in_flight = if running.is_empty() {
        String::new()
    } else {
        format!("in flight: {}", running.join("; "))
    };
    let left: Vec<String> = state.left_behind().map(named).collect();
    if state.quarantine.is_none() && left.is_empty() {
        return Some(if running.is_empty() {
            Check::ok(Section::Project, SUBJECT, "not quarantined")
        } else {
            Check::info(
                Section::Project,
                SUBJECT,
                format!("not quarantined; runs {in_flight}"),
            )
        });
    }
    let mut said = Vec::new();
    let folder = match &state.quarantine {
        Some((path, text)) => {
            let lines: Vec<&str> = text
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .collect();
            said.push(format!("{} says: {}", shown(path, home), lines.join(" ")));
            path.parent().map_or_else(String::new, |p| shown(p, home))
        }
        None => String::new(),
    };
    if !left.is_empty() {
        said.push(format!(
            "runs ended before their isolation check passed: {}",
            left.join("; ")
        ));
    }
    if !in_flight.is_empty() {
        said.push(format!("runs {in_flight}"));
    }
    let folder = if folder.is_empty() {
        "".to_owned()
    } else {
        format!(", in the project's folder {folder}")
    };
    Some(Check::fail(
        Section::Project,
        SUBJECT,
        said.join("; "),
        "A run's isolation check did not pass, or did not finish, so the project's dedicated \
         checkout may hold what it changed outside its worktree. Until a person clears the \
         quarantine, Owlshift refuses the whole project on this machine: no ticket of it is \
         run, continued, taken in or built.",
        vec![
            Step::act(format!("Look at what the quarantine names{folder}")),
            Step::run_noting(
                "owlshift unquarantine",
                "in the project's repository: it shows the quarantine and the runs left behind, \
                 then clears them once you type the project's name",
            ),
            Step::act(
                "Or, to start from a fresh clone, delete the `checkout` and `worktrees` folders \
                 there first, which loses the ticket refs, then run `owlshift unquarantine`",
            ),
        ],
    ))
}

/// The web UI the daemon runs (OWL-467), while a daemon runs: proved on
/// `127.0.0.1:<port>`, or a warning naming the port, and `[ui] port` when
/// another program may hold it. `log` is the daemon's log, shown. It never
/// blocks `owlshift do`.
fn ui_check(probe: &Probe, port: u16, log: &str) -> Check {
    const SUBJECT: &str = "web UI";
    let held = || {
        format!(
            "{DOES_NOT_BLOCK} free the port, which the daemon tries again, or set another `[ui] \
             port` in the personal file and restart the service."
        )
    };
    match probe {
        Probe::Proved(Some(record)) => Check::ok(
            Section::Project,
            SUBJECT,
            format!(
                "listens on 127.0.0.1:{port} (pid {}, since {})",
                record.pid, record.since
            ),
        ),
        Probe::Proved(None) => Check::ok(
            Section::Project,
            SUBJECT,
            format!("listens on 127.0.0.1:{port}"),
        ),
        Probe::Refused => Check::warn(
            Section::Project,
            SUBJECT,
            format!("nothing listens on 127.0.0.1:{port}"),
            format!(
                "{DOES_NOT_BLOCK} the daemon binds the port again at its next try; {log} says \
                 why."
            ),
        ),
        Probe::Silent { recorded: true } => Check::warn(
            Section::Project,
            SUBJECT,
            format!("the web UI on 127.0.0.1:{port} does not answer"),
            format!("{DOES_NOT_BLOCK} the daemon starts it again after an end; {log} says why."),
        ),
        Probe::Silent { recorded: false } => Check::warn(
            Section::Project,
            SUBJECT,
            format!(
                "port {port} of 127.0.0.1, `[ui] port` of the personal file, takes connections \
                 and answers nothing, and no web UI recorded itself there"
            ),
            format!("{} {log} says why.", held()),
        ),
        Probe::NotProved(why) => Check::warn(
            Section::Project,
            SUBJECT,
            format!(
                "port {port} of 127.0.0.1, `[ui] port` of the personal file, is held by a program \
                 that did not prove itself the web UI ({why})"
            ),
            held(),
        ),
        Probe::Unread(why) => Check::warn(
            Section::Project,
            SUBJECT,
            format!("the web UI on 127.0.0.1:{port} could not be checked: {why}"),
            format!("{DOES_NOT_BLOCK} `owlshift ui` checks it the same way."),
        ),
    }
}

/// The background service installed at login (OWL-437; runtime and
/// operations, "Install, lifecycle & uninstall"): installed or not, the
/// binary it starts, the daemon that runs; a warning, with the commands that
/// remove it, for a unit or launcher at Owlshift's paths the install manifest
/// does not list, a listed binary or file that is gone, or a manifest that
/// does not read. It never cleans up itself, and never blocks `owlshift do`.
fn service_check(state: &ServiceState, home: Option<&Path>) -> Check {
    const SUBJECT: &str = "service";
    let daemon = match &state.daemon {
        Ok(None) => "no daemon runs".to_owned(),
        Ok(Some(Some(record))) => {
            let drain = state.drain.as_ref().map_or_else(String::new, |drain| {
                format!(
                    ", draining for an upgrade ({}: {}): the service at login starts it again \
                     once its steps under way finish",
                    drain.by, drain.why
                )
            });
            format!(
                "the daemon runs (pid {}, version {}, since {}){drain}",
                record.pid, record.version, record.since
            )
        }
        Ok(Some(None)) => "a daemon runs, its record unread".to_owned(),
        Err(error) => format!("whether a daemon runs was not read: {error}"),
    };
    let listed = match &state.listed {
        Ok(listed) => listed,
        Err(error) => {
            return Check::warn(
                Section::Project,
                SUBJECT,
                format!("the install manifest does not read: {error}; {daemon}"),
                format!(
                    "{DOES_NOT_BLOCK} `owlshift uninstall` removes only what the manifest lists: \
                     fix or remove {}.",
                    shown(&state.manifest, home)
                ),
            );
        }
    };
    let mut problems = Vec::new();
    if !state.unlisted.is_empty() {
        let files: Vec<String> = state
            .unlisted
            .iter()
            .map(|path| shown(path, home))
            .collect();
        problems.push(format!(
            "{} {} Owlshift's but not listed in this data directory's install manifest, so \
             another data directory installed {} or it was left behind: remove {} with `{}`",
            files.join(" and "),
            if files.len() == 1 { "is" } else { "are" },
            if files.len() == 1 { "it" } else { "them" },
            if files.len() == 1 { "it" } else { "them" },
            state.layout.removal_commands().join("; ")
        ));
    }
    let detail = match listed {
        None => {
            format!("not installed at login (`owlshift start --at-login` installs it); {daemon}")
        }
        Some(service) => {
            let how = match state.layout.kind {
                Kind::Launchd => format!("the user LaunchAgent `{LABEL}`"),
                Kind::Systemd => format!("the systemd user unit `{SYSTEMD_UNIT}`"),
            };
            let binary = shown(Path::new(&service.binary), home);
            if !state.binary_there {
                problems.push(format!(
                    "its binary {binary} is gone, so its launcher removes the service at its next \
                     start: `owlshift start --at-login` names this binary instead, or `owlshift \
                     uninstall` removes it"
                ));
            }
            if !state.gone.is_empty() {
                let gone: Vec<String> = state
                    .gone
                    .iter()
                    .map(|file| shown(Path::new(file), home))
                    .collect();
                problems.push(format!(
                    "the install manifest lists {}, which {} gone: `owlshift start --at-login` \
                     writes the service again, or `owlshift uninstall` drops it from the manifest",
                    gone.join(" and "),
                    if gone.len() == 1 { "is" } else { "are" }
                ));
            }
            format!("installed at login as {how}, starting {binary}; {daemon}")
        }
    };
    match (listed, problems.is_empty()) {
        (_, false) => Check::warn(
            Section::Project,
            SUBJECT,
            detail,
            format!("{DOES_NOT_BLOCK} {}.", problems.join("; ")),
        ),
        (None, true) => Check::info(Section::Project, SUBJECT, detail),
        (Some(_), true) => Check::ok(Section::Project, SUBJECT, detail),
    }
}

/// How far a build's commit is behind the tree it was built from.
struct Behind {
    commits: u64,
    reference: &'static str,
    source: String,
}

/// Whether `build`'s commit is behind `origin/HEAD`, else `origin/main`, of
/// the tree it was built from, as that tree last fetched it: nothing is
/// fetched. `None` when it is not, or when git, the tree, the reference or
/// the commit cannot be read.
fn behind(system: &dyn System, build: &BuildIdentity) -> Option<Behind> {
    let (commit, source) = (build.commit.as_deref()?, build.source.as_deref()?);
    let git = system.locate("git")?;
    for reference in ["origin/HEAD", "origin/main"] {
        let range = format!("{commit}..{reference}");
        match system.run(
            &git,
            &["rev-list", "--count", &range],
            Some(Path::new(source)),
        ) {
            Ok(out) if out.code == Some(0) => {
                let commits: u64 = String::from_utf8_lossy(&out.stdout).trim().parse().ok()?;
                return (commits > 0).then(|| Behind {
                    commits,
                    reference,
                    source: source.to_owned(),
                });
            }
            _ => continue,
        }
    }
    None
}

/// Which build each daemon and watch that runs on the data directory runs
/// (OWL-462; architecture section 5, "An upgrade"): a warning, saying what
/// to do, for each one whose build is not this binary's `own` or is
/// unknown, that was started from a file since replaced on disk, or whose
/// commit is behind the tree it was built from as last fetched ([`behind`]).
/// No line when none runs; it never blocks `owlshift do`.
fn builds_check(
    system: &dyn System,
    running: Result<RunningBuilds, String>,
    own: &BuildIdentity,
    home: Option<&Path>,
) -> Option<Check> {
    const SUBJECT: &str = "running builds";
    let running = match running {
        Ok(running) => running,
        Err(error) => {
            return Some(Check::warn(
                Section::Project,
                SUBJECT,
                format!("not read: {error}"),
                format!(
                    "{DOES_NOT_BLOCK} which build the daemon and the watches run is not known."
                ),
            ));
        }
    };
    if running.processes.is_empty() {
        return None;
    }
    let mine = Binary {
        version: OWLSHIFT_VERSION,
        build: Some(own),
    };
    // One look per tree and commit: the daemon and its watches share one.
    let mut looked: Vec<(&BuildIdentity, Option<Behind>)> = Vec::new();
    let mut current = Vec::new();
    let mut problems = Vec::new();
    for process in &running.processes {
        let who = who_runs(process);
        let mut reasons = Vec::new();
        match (&process.build, &process.version) {
            (None, Some(_)) => {
                reasons.push("records no build, being from before OWL-462".to_owned())
            }
            (None, None) => {
                reasons.push("has a record that does not read, its build unknown".to_owned())
            }
            (Some(build), _) if !build.same_build(own) => reasons.push(format!(
                "runs {}, not this binary's {}",
                build.short(),
                own.short()
            )),
            (Some(_), _) => {}
        }
        if process.replaced {
            reasons.push("was started from a file since replaced on disk".to_owned());
        }
        let index = process.build.as_ref().map(|build| {
            let known = looked
                .iter()
                .position(|(seen, _)| seen.commit == build.commit && seen.source == build.source);
            known.unwrap_or_else(|| {
                looked.push((build, behind(system, build)));
                looked.len() - 1
            })
        });
        let behind = index.and_then(|index| looked[index].1.as_ref());
        if let Some(behind) = behind {
            reasons.push(format!(
                "is {} commit{} behind {} of {}, as that tree last fetched it",
                behind.commits,
                if behind.commits == 1 { "" } else { "s" },
                behind.reference,
                shown(Path::new(&behind.source), home)
            ));
        }
        if reasons.is_empty() {
            current.push(who);
            continue;
        }
        let todo = what_to_do(
            process,
            mine,
            running.drain.is_some(),
            running.moves_itself,
            behind,
            home,
        );
        problems.push(format!("{who} {}: {todo}", reasons.join(", ")));
    }
    if problems.is_empty() {
        return Some(Check::ok(
            Section::Project,
            SUBJECT,
            format!(
                "{} {} this binary's build ({})",
                current.join(" and "),
                if current.len() == 1 { "runs" } else { "run" },
                own.short()
            ),
        ));
    }
    Some(Check::warn(
        Section::Project,
        SUBJECT,
        format!(
            "{} of {} running {} not on this binary's build ({}) or behind its source",
            problems.len(),
            running.processes.len(),
            if running.processes.len() == 1 {
                "process is"
            } else {
                "processes are"
            },
            own.short()
        ),
        format!("{DOES_NOT_BLOCK} {}.", problems.join("; ")),
    ))
}

/// The daemon or the watch `process`, in words.
fn who_runs(process: &Running) -> String {
    match (&process.project, process.pid) {
        (None, Some(pid)) => format!("the daemon (pid {pid})"),
        (None, None) => "the daemon".to_owned(),
        (Some(project), pid) => format!(
            "the watch of {project} (pid {}, {})",
            pid.map_or_else(|| "unknown".to_owned(), |pid| pid.to_string()),
            if process.served {
                "run by the daemon"
            } else {
                "typed in a terminal"
            }
        ),
    }
}

/// What to do about `process`, a daemon or a watch not on this binary
/// `mine`'s build: rebuild first when it is `behind` its source; restart it
/// (a typed watch in its terminal, the daemon and its watches by `owlshift
/// stop` then `start`), unless its upgrade's drain is `draining` it or the
/// daemon `moves_itself` onto this binary (OWL-616); and when it runs a
/// newer build than this binary, this binary is the stale one.
fn what_to_do(
    process: &Running,
    mine: Binary<'_>,
    draining: bool,
    moves_itself: bool,
    behind: Option<&Behind>,
    home: Option<&Path>,
) -> String {
    let theirs = process.version.as_deref().map(|version| Binary {
        version,
        build: process.build.as_ref(),
    });
    if theirs.is_some_and(|theirs| supersedes(theirs, mine)) {
        return "this binary is the older one: run the `owlshift` you mean to keep, or rebuild \
                this one"
            .to_owned();
    }
    let restart = match (&process.project, process.served, draining, moves_itself) {
        (Some(_), false, _, _) => {
            "stop it in its terminal (a first Ctrl-C ends it once its steps under way finish) and \
             start it again"
        }
        (_, _, true, _) => {
            "let the upgrade's drain asked of the daemon end: the daemon comes back on the new \
             binary once its steps under way finish"
        }
        (_, _, _, true) => {
            "the daemon moves onto this binary by itself at its next pass, within a minute, once \
             its steps under way finish (its log says so if it does not)"
        }
        _ => "run `owlshift stop`, then `owlshift start`",
    };
    match behind {
        Some(behind) => format!(
            "rebuild and install `owlshift` from {} once that tree is up to date with its origin, \
             then {restart}",
            shown(Path::new(&behind.source), home)
        ),
        None => restart.to_owned(),
    }
}

/// Where `owlshift do` keeps its event log, dedicated checkouts and
/// worktrees (OWL-109). Information only, never a failure or a warning: what
/// blocks a run is `do`'s own refusal. Only the path is reported: the folder
/// is never opened, created or listed.
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
        TrackerAccounts::project(
            &Repo::parse("pitchopp/owlshift").unwrap(),
            &crate::tracker::LINEAR_SECRETS,
        )
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
        let report = super::run(system, config, &built('a', false, 0));
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

    /// OWL-593: a cache folder the project file lists and the personal file
    /// does not allow fails a line of its own, whose fix holds the lines to
    /// add for the checkout's repository; once allowed, there is no line. The
    /// project file declares yarn 4's cache, as `init` wrote it before
    /// OWL-629.
    #[test]
    fn a_cache_the_personal_file_does_not_allow_is_named_with_its_lines() {
        let system = logged_in(with_harnesses(with_git(FakeSystem::default())));
        let text = crate::init::project_file(&crate::init::InitOptions {
            tracker: TrackerKind::Markdown,
            team: None,
            gate: vec!["make".into()],
            detected: Default::default(),
            admitter: None,
        })
        .unwrap()
        .replace("# caches = [\".pyenv\"]\n", "caches = [\".yarn/berry\"]\n");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("owlshift.toml");
        std::fs::write(&path, &text).unwrap();
        let personal = PathBuf::from("/home/ada/.config/owlshift/config.toml");
        let refused = Effective {
            project: FileState::Invalid {
                path: path.clone(),
                error: "stack.caches: \".yarn/berry\" may not be read".into(),
            },
            personal: FileState::Absent(personal.clone()),
        };
        let report = run(&system, &refused);
        let found = line(&report, "caches");
        assert_eq!(found.status, Status::Fail);
        assert!(found.detail.contains("\".yarn/berry\""), "{found:?}");
        assert!(found.detail.contains("github.com/pitchopp/owlshift"));
        let [Step::Do(fix)] = &found.fix[..] else {
            panic!("{found:?}");
        };
        assert!(fix.contains(
            "[repositories.\"github.com/pitchopp/owlshift\"]\nallow_caches = [\".yarn/berry\"]\n"
        ));
        let allowed = Effective {
            personal: FileState::Loaded {
                path: personal,
                config: PersonalConfig::parse(&crate::init::allow_caches_lines(
                    None,
                    &[".yarn/berry".to_owned()],
                ))
                .unwrap(),
                entries: Vec::new(),
            },
            ..refused
        };
        assert!(caches_check(&system, &allowed, None).is_none());
    }

    /// OWL-617: the run caches' line gives the tree's path and size, says
    /// it is not made yet, or that nothing declares it any more; it fails
    /// where no run could make it; no line when neither the file nor the
    /// disk has one.
    #[test]
    fn the_run_caches_line_gives_the_trees_path_and_size() {
        let dir = tempfile::tempdir().unwrap();
        let loaded = |stack: &str| {
            let path = dir.path().join("owlshift.toml");
            std::fs::write(
                &path,
                format!(
                    "requires = \">=0.0\"\n[tracker]\nkind = \"markdown\"\nadmit = {{ label = \
                     \"x\" }}\nstates = {{ ready = \"Todo\", working = \"Doing\", needs_input = \
                     \"Asked\", review = \"Review\" }}\n[stack]\ngate = [\"make\"]\n{stack}\n\
                     [pipeline]\ndefault = \"trivial\"\nplan_approval = \"never\"\n[models]\n\
                     [policy]\nalways_human = []\n"
                ),
            )
            .unwrap();
            Effective {
                project: FileState::load(path, owlshift_contracts::config::ProjectConfig::parse),
                personal: FileState::NotApplicable("no configuration directory".into()),
            }
        };
        let declared = loaded("run_caches = { CARGO_HOME = \"cargo\" }");
        let bare = loaded("");
        let caches = dir.path().join("caches");
        let system = FakeSystem::default().run_cache_dir_is(&caches);
        let tree = caches.join("github.com").join("pitchopp").join("owlshift");
        let detail = |config: &Effective| run_caches_check(&system, config, None).map(|c| c.detail);

        assert_eq!(detail(&bare), None);
        assert!(
            detail(&declared)
                .unwrap()
                .ends_with("not made yet: the next run makes it")
        );
        std::fs::create_dir_all(tree.join("cargo")).unwrap();
        std::fs::write(tree.join("cargo/x"), "12345").unwrap();
        let held = detail(&declared).unwrap();
        assert!(
            held.starts_with(&tree.display().to_string())
                && held.ends_with("holds 5 B; deleting it empties it, the next run makes it again"),
            "{held}"
        );
        assert!(
            detail(&bare)
                .unwrap()
                .contains("declares no run cache: delete it")
        );
        for system in [
            FakeSystem::default(),
            FakeSystem::default().run_cache_dir_is(&dir.path().join("with space")),
        ] {
            let found = run_caches_check(&system, &declared, None).unwrap();
            assert_eq!(found.status, Status::Fail, "{found:?}");
            assert!(found.detail.contains("cache directory") || found.detail.contains("space"));
            assert!(
                matches!(&found.fix[..], [Step::Do(fix)] if fix.contains("OWLSHIFT_CACHE_DIR"))
            );
        }
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
            (
                ready().unstored(own().of(LINEAR_ACCOUNT)),
                &["owlshift init"],
            ),
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
                    .unstored(own().of(LINEAR_APP_SECRET_ACCOUNT)),
                &["owlshift init"],
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

    /// OWL-658: each line that finds a credential the service's pause would
    /// name gives the pause's fix, in the folder the pause names: the
    /// registered checkout when the service serves the project.
    #[test]
    fn the_credential_lines_give_the_pauses_fix_in_its_folder() {
        use crate::tracker::{AppFault, AppFaultKind};
        use owlshift_adapters::tracker::Error;

        let (dir, owl) = linear_project();
        let repo = Repo::parse("pitchopp/owlshift").unwrap();
        let data = tempfile::tempdir().unwrap();
        let checkout = dir.path().join("my checkout");
        served::Registry::in_data_dir(data.path())
            .register(&repo, &checkout)
            .unwrap();
        let ready = || {
            logged_in(with_harnesses(with_git(FakeSystem::default())))
                .data_dir_is(Some((data.path().to_owned(), DataDirSource::Platform)))
        };
        let fix_of = |system: &FakeSystem, subject: &str| {
            let report = run(system, &owl);
            let check = line(&report, subject).clone();
            assert_eq!(check.status, Status::Fail, "{check:?}");
            check.fix
        };
        let in_checkout = format!("in {}: ", checkout.display());

        // No GitHub token: `owlshift init` signs in from the checkout.
        let no_token = ready()
            .unstored(&secrets::ForgeAccount::project(&repo).token)
            .unstored(&secrets::ForgeAccount::shared().token);
        let [Step::Run { command, note }] = &fix_of(&no_token, "secrets")[..] else {
            panic!("one command");
        };
        assert_eq!(command, "owlshift init");
        let note = note.as_deref().unwrap();
        assert!(note.starts_with(&in_checkout), "{note}");
        assert!(
            note.contains("signs in to GitHub") && note.contains("your owlshift.toml"),
            "{note}"
        );

        // No Linear API key: the key the notice asks for, from the checkout.
        let no_key = ready().unstored(own().of(LINEAR_ACCOUNT));
        assert_eq!(
            fix_of(&no_key, "tracker states"),
            [crate::tracker::linear_key_step(&checkout)]
        );

        // The Linear app: the notice's choices, in the same folder.
        let fault = |kind| AppFault {
            kind,
            accounts: own(),
        };
        let half = ready()
            .linear_app_is(Ok("owlshiftbot"))
            .unstored(own().of(LINEAR_APP_SECRET_ACCOUNT));
        let refused = Error::new(ErrorKind::Unauthorized, "refused");
        let blind = Error::new(ErrorKind::NotFound, "the app does not see team \"OWL\"");
        for (system, kind, commands_given) in [
            (half, AppFaultKind::Half, &["owlshift init"][..]),
            (
                ready().linear_app_is(Err(refused.clone())),
                AppFaultKind::Refused,
                &["owlshift init --replace-secrets"],
            ),
            (
                ready().linear_app_is(Err(blind)),
                AppFaultKind::NoTeam("OWL".into()),
                &[],
            ),
        ] {
            let fix = fix_of(&system, "tracker identity");
            assert_eq!(fix, fault(kind).steps(&checkout));
            assert_eq!(commands(&fix), commands_given, "{fix:?}");
            let Some(Step::Do(last)) = fix.last() else {
                panic!("{fix:?}");
            };
            assert!(
                last.contains("delete the keychain entries")
                    && last.contains("github.com/pitchopp/owlshift/linear-app-client-id"),
                "{last}"
            );
            for step in &fix {
                if let Step::Run { note, .. } = step {
                    assert!(
                        note.as_deref().unwrap().starts_with(&in_checkout),
                        "{fix:?}"
                    );
                }
            }
        }

        // A shared set: this project's own key and app, never a deletion.
        let shared = fix_of(
            &ready().only_shared().linear_app_is(Err(refused)),
            "tracker identity",
        );
        assert_eq!(commands(&shared), ["owlshift init"], "{shared:?}");
        assert!(!format!("{shared:?}").contains("delete"), "{shared:?}");

        // A keychain doctor cannot read: the words the pause gives.
        assert_eq!(KEYCHAIN_FIX, crate::secrets::UNLOCK_KEYCHAIN);
        assert!(crate::secrets::keychain_notice(&repo, true).contains(KEYCHAIN_FIX));
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
        let report = run(&ready().unstored(own().of(LINEAR_ACCOUNT)), &config);
        assert_eq!(line(&report, "drain admission").status, Status::Ok);
    }

    /// OWL-582: with `[people]`, the `people` line counts them and says
    /// whether the Linear key's account, this machine's operator, is one;
    /// the reviewers are the people with a forge login; the admission line
    /// names an admitter by its handle. No `people` line without the table.
    #[test]
    fn the_people_line_names_the_operator_or_warns() {
        use owlshift_adapters::tracker::Error;

        let ready = || logged_in(with_harnesses(with_git(FakeSystem::default())));
        let tracker = LABELLED_TRACKER.replace(r#"["u-ann"]"#, r#"["ann"]"#);
        let (_dir, config) = project_tracked_with(
            &tracker,
            "[people.ann]\ntracker = \"u-ann\"\nforge = \"ann-dev\"\n[people.bob]\ntracker = \"u-bob\"",
        );
        let account = |id: &str, app: bool| KeyAccount {
            id: id.to_owned(),
            name: format!("name of {id}"),
            app,
            workspace: "owl".to_owned(),
        };
        let people = |system: FakeSystem| {
            let report = run(&system, &config);
            assert!(report.ready(), "{report}");
            (
                line(&report, "people").clone(),
                line(&report, "forge reviewers").clone(),
                line(&report, "drain admission").clone(),
            )
        };

        let (listed, reviewers, admission) = people(
            ready()
                .linear_operator_is(Ok(account("u-bob", false)))
                .linear_admission_is(Ok(facts(Ok(false), &["agent", "hold"], &[]))),
        );
        assert_eq!(listed.status, Status::Ok, "{listed:?}");
        assert!(
            listed
                .detail
                .starts_with("`[people]` lists 2 people in the working tree")
                && listed.detail.ends_with("this machine's operator is bob"),
            "{listed:?}"
        );
        assert_eq!(reviewers.status, Status::Ok);
        assert!(
            reviewers
                .detail
                .starts_with("1 reviewer(s) from `[people]`"),
            "{reviewers:?}"
        );
        assert!(
            admission.detail.contains("admitter \"ann\""),
            "{admission:?}"
        );

        let (unlisted, ..) = people(ready().linear_operator_is(Ok(account("u-eve", false))));
        assert_eq!(unlisted.status, Status::Warn);
        assert!(
            unlisted.detail.ends_with(
                "this machine's operator, \"name of u-eve\" (\"u-eve\"), is not one of them"
            ),
            "{unlisted:?}"
        );
        assert!(
            unlisted
                .why
                .as_deref()
                .unwrap_or_default()
                .contains("tracker = \"u-eve\""),
            "{unlisted:?}"
        );

        let (app, ..) = people(ready().linear_operator_is(Ok(account("app-1", true))));
        assert_eq!(app.status, Status::Ok);
        assert!(app.detail.contains("operator was not read"), "{app:?}");

        let refused = Error::new(ErrorKind::Unauthorized, "Linear: no").to_string();
        let (unread, ..) = people(ready().linear_operator_is(Err(refused)));
        assert_eq!(unread.status, Status::Ok);
        assert!(
            unread
                .detail
                .ends_with("this machine's operator could not be read: Linear: no"),
            "{unread:?}"
        );

        let (_dir, without) = project_tracked_by(LABELLED_TRACKER);
        let report = run(&ready(), &without);
        assert!(
            report.checks.iter().all(|c| c.subject != "people"),
            "{report}"
        );
    }

    /// OWL-582: a Markdown project's operator is the personal file's
    /// `identity.tracker`; nobody with a forge login reads no review.
    #[test]
    fn a_markdown_operator_is_the_personal_identity() {
        let system = logged_in(with_harnesses(with_git(FakeSystem::default())));
        let tracker = r#"kind = "markdown"
admit = "delegation"
states = { ready = "Todo", working = "Doing", needs_input = "Asked", review = "Review" }"#;
        let (dir, _) = project_tracked_with(tracker, "[people.ann]\ntracker = \"ann\"");
        let with_identity = |text: &str| Effective {
            project: FileState::load(
                dir.path().join("owlshift.toml"),
                owlshift_contracts::config::ProjectConfig::parse,
            ),
            personal: FileState::Loaded {
                path: PathBuf::from("/home/ada/.config/owlshift/config.toml"),
                config: owlshift_contracts::config::PersonalConfig::parse(text).unwrap(),
                entries: Vec::new(),
            },
        };
        let report = run(&system, &with_identity("[identity]\ntracker = \"ann\"\n"));
        let listed = line(&report, "people");
        assert!(listed.detail.ends_with("operator is ann"), "{listed:?}");
        let silent = line(&report, "forge reviewers");
        assert_eq!(silent.status, Status::Warn);
        assert!(
            silent
                .detail
                .starts_with("no person of `[people]` has a forge login"),
            "{silent:?}"
        );
        let report = run(&system, &with_identity(""));
        let unread = line(&report, "people");
        assert!(
            unread
                .detail
                .ends_with("the personal configuration sets no `identity.tracker`"),
            "{unread:?}"
        );
    }

    /// OWL-583: a state gesture in a file that lists several people is
    /// noted, pointing to a label; one person, or a label, says nothing.
    #[test]
    fn a_state_gesture_is_noted_when_several_people_are_listed() {
        let ready = LABELLED_TRACKER.replace("admit = { label = \"agent\" }", "admit = \"ready\"");
        let two = ready.replace(r#"["u-ann"]"#, r#"["u-ann", "u-bob"]"#);
        let (_dir, config) = project_tracked_by(&two);
        let noted = team_gesture_check(&config).unwrap();
        assert_eq!(noted.status, Status::Warn);
        assert!(
            noted
                .detail
                .contains("`[tracker].admitters` lists 2 accounts"),
            "{noted:?}"
        );
        assert!(
            noted
                .why
                .as_deref()
                .is_some_and(|why| why.contains("label")),
            "{noted:?}"
        );
        for quiet in [
            ready,
            LABELLED_TRACKER.replace(r#"["u-ann"]"#, r#"["u-ann", "u-bob"]"#),
        ] {
            let (_dir, config) = project_tracked_by(&quiet);
            assert!(team_gesture_check(&config).is_none(), "{quiet}");
        }
    }

    /// OWL-586: claims left local in a file that lists several people are
    /// warned of; remote claims, or one person, say nothing.
    #[test]
    fn local_claims_are_warned_of_when_several_people_are_listed() {
        let two = LABELLED_TRACKER.replace(r#"["u-ann"]"#, r#"["u-ann", "u-bob"]"#);
        let (_dir, config) = project_tracked_by(&two);
        let warned = claims_check(&config).unwrap();
        assert_eq!(warned.status, Status::Warn);
        assert!(
            warned
                .detail
                .contains("`[tracker].admitters` lists 2 accounts"),
            "{warned:?}"
        );
        let (_dir, config) = project_tracked_with(&two, "claims = \"remote\"");
        assert!(claims_check(&config).is_none());
        let (_dir, config) = project_tracked_by(LABELLED_TRACKER);
        assert!(claims_check(&config).is_none());
    }

    /// OWL-590: a team's first mistakes, the operator missing from
    /// `[people]`, a state gesture and local claims, are told in that order
    /// and as one story: the same count of the same table, each fix through
    /// a pull request, each pointing to the team adoption page.
    #[test]
    fn the_team_lines_tell_one_story() {
        let ready = || logged_in(with_harnesses(with_git(FakeSystem::default())));
        let tracker = LABELLED_TRACKER
            .replace("admit = { label = \"agent\" }", "admit = \"ready\"")
            .replace(r#"["u-ann"]"#, r#"["ann", "bob"]"#);
        let (_dir, config) = project_tracked_with(
            &tracker,
            "[people.ann]\ntracker = \"u-ann\"\n[people.bob]\ntracker = \"u-bob\"",
        );
        let eve = KeyAccount {
            id: "u-eve".to_owned(),
            name: "Eve".to_owned(),
            app: false,
            workspace: "owl".to_owned(),
        };
        let report = run(&ready().linear_operator_is(Ok(eve)), &config);
        let subjects: Vec<&str> = report.checks.iter().map(|c| c.subject.as_str()).collect();
        let at = |subject: &str| subjects.iter().position(|s| *s == subject).unwrap();
        assert!(
            at("people") < at("team gesture") && at("team gesture") < at("claims"),
            "{subjects:?}"
        );
        for subject in ["people", "team gesture", "claims"] {
            let told = line(&report, subject);
            assert_eq!(told.status, Status::Warn, "{told:?}");
            assert!(
                told.detail.contains("`[people]` lists 2 people"),
                "{told:?}"
            );
            let why = told.why.as_deref().unwrap_or_default();
            assert!(
                why.contains("through a pull request to the default branch")
                    && why.ends_with(TEAM_PAGE),
                "{told:?}"
            );
        }
    }

    /// OWL-590: the team lines' page is a page of the repository.
    #[test]
    fn the_team_page_exists() {
        let path = TEAM_PAGE
            .split_once("/blob/main/")
            .map(|(_, path)| path)
            .unwrap();
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        assert!(root.join(path).is_file(), "{path}");
    }

    /// OWL-587: under remote claims, a claim of a later format says to
    /// upgrade; claims that all read, or a project whose claims are local,
    /// say nothing.
    #[test]
    fn a_claim_of_a_later_format_says_to_upgrade() {
        let (_dir, remote) = project_tracked_with(LABELLED_TRACKER, "claims = \"remote\"");
        let unread =
            |answer: Vec<(TicketId, bool)>| FakeSystem::default().unread_claims_are(Ok(answer));
        let demo = TicketId::new("DEMO-7").unwrap();
        let warned = unread_claims_check(&unread(vec![(demo.clone(), true)]), &remote).unwrap();
        assert_eq!(warned.status, Status::Warn);
        assert!(
            warned.detail.starts_with("DEMO-7 claimed on origin"),
            "{warned:?}"
        );
        assert!(
            warned.why.as_deref().unwrap().contains("upgrade Owlshift"),
            "{warned:?}"
        );
        assert!(unread_claims_check(&unread(Vec::new()), &remote).is_none());
        let (_dir, local) = project_tracked_by(LABELLED_TRACKER);
        assert!(unread_claims_check(&unread(vec![(demo, true)]), &local).is_none());
    }

    /// OWL-583: a Markdown project whose personal file names no
    /// `identity.tracker` takes every admitted ticket, which doctor says;
    /// with one, or under `takes = "all"`, it says nothing.
    #[test]
    fn a_markdown_drain_without_an_identity_is_said_to_take_every_ticket() {
        let tracker = r#"kind = "markdown"
admit = { label = "agent" }
admitters = ["ann"]
states = { ready = "Todo", working = "Doing", needs_input = "Asked", review = "Review" }"#;
        let (dir, _) = project_tracked_by(tracker);
        let with = |text: &str| Effective {
            project: FileState::load(
                dir.path().join("owlshift.toml"),
                owlshift_contracts::config::ProjectConfig::parse,
            ),
            personal: FileState::Loaded {
                path: PathBuf::from("/home/ada/.config/owlshift/config.toml"),
                config: owlshift_contracts::config::PersonalConfig::parse(text).unwrap(),
                entries: Vec::new(),
            },
        };
        let said = markdown_takes_check(&with(""), None).unwrap();
        assert!(
            said.detail.contains("takes every admitted ticket"),
            "{said:?}"
        );
        assert!(markdown_takes_check(&with("[identity]\ntracker = \"ann\"\n"), None).is_none());
        assert!(markdown_takes_check(&with("takes = \"all\"\n"), None).is_none());
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

    /// OWL-600: a `[forge]` table that sets no reviewer (only `merge`, say)
    /// warns like a project without the table: nobody is asked to review.
    #[test]
    fn forge_table_without_reviewers_is_warned_of() {
        let system = logged_in(with_harnesses(with_git(FakeSystem::default())));
        let tracker = r#"kind = "markdown"
admit = "delegation"
states = { ready = "Todo", working = "Doing", needs_input = "Asked", review = "Review" }"#;
        let (_dir, only_merge) =
            project_tracked_by(&format!("{tracker}\n[forge]\nmerge = \"never\""));
        let report = run(&system, &only_merge);
        let warned = line(&report, "forge reviewers");
        assert_eq!(warned.status, Status::Warn);
        assert_eq!(
            warned.detail,
            "`[forge].reviewers` is not set: no review on Owlshift's pull requests is read"
        );
    }

    /// OWL-445: a project whose `[forge] merge` is not `never` gets a
    /// `merge on green` line from the GitHub App's installation: ok when it
    /// granted Contents (read and write), a failure naming it otherwise,
    /// information for a personal token or no installation; none under
    /// `never`.
    #[test]
    fn merge_on_green_names_a_missing_contents_write() {
        let installed = |contents_write| AppGrants::Installed {
            installation: 9,
            contents_write,
            actions_write: true,
            statuses_write: false,
        };

        let system = || logged_in(with_harnesses(with_git(FakeSystem::default())));
        let tracker = r#"kind = "markdown"
admit = "delegation"
states = { ready = "Todo", working = "Doing", needs_input = "Asked", review = "Review" }"#;
        let (_dir, never) = project_tracked_by(&format!("{tracker}\n[forge]\nmerge = \"never\""));
        let report = run(&system(), &never);
        assert!(
            !report.checks.iter().any(|c| c.subject == "merge on green"),
            "{report:?}"
        );
        let (_dir, all) = project_tracked_by(&format!("{tracker}\n[forge]\nmerge = \"all\""));
        for (permission, status, says) in [
            (
                installed(true),
                Status::Ok,
                "grants Contents (read and write)",
            ),
            (
                installed(false),
                Status::Fail,
                "installation 9 on github.com/pitchopp/owlshift has not granted Contents (read \
                 and write)",
            ),
            (AppGrants::NotInstalled, Status::Info, "no installation"),
            (AppGrants::PersonalToken, Status::Info, "personal token"),
        ] {
            let report = run(&system().github_app_grants_are(Ok(permission)), &all);
            let found = line(&report, "merge on green");
            assert_eq!(found.status, status, "{found:?}");
            assert!(found.detail.contains(says), "{found:?}");
            assert_eq!(report.ready(), status != Status::Fail, "{permission:?}");
        }
    }

    /// OWL-513: every project with a stored token gets a `check re-runs`
    /// line from the GitHub App's installation: ok when it granted Actions
    /// (read and write), a warning naming it and the installation's
    /// settings otherwise, which blocks nothing, information for a personal
    /// token or no installation.
    #[test]
    fn check_reruns_name_a_missing_actions_write() {
        let system = || logged_in(with_harnesses(with_git(FakeSystem::default())));
        let tracker = r#"kind = "markdown"
admit = "delegation"
states = { ready = "Todo", working = "Doing", needs_input = "Asked", review = "Review" }"#;
        let (_dir, never) = project_tracked_by(&format!("{tracker}\n[forge]\nmerge = \"never\""));
        let installed = |actions_write| AppGrants::Installed {
            installation: 9,
            contents_write: false,
            actions_write,
            statuses_write: false,
        };
        for (grants, status, says) in [
            (
                installed(true),
                Status::Ok,
                "grants Actions (read and write), which watch's re-run of a red check needs",
            ),
            (
                installed(false),
                Status::Warn,
                "installation 9 on github.com/pitchopp/owlshift has not granted Actions (read \
                 and write): watch cannot re-run a failed check; a red check gets a fix run as \
                 before",
            ),
            (AppGrants::NotInstalled, Status::Info, "no installation"),
            (AppGrants::PersonalToken, Status::Info, "personal token"),
        ] {
            let report = run(&system().github_app_grants_are(Ok(grants)), &never);
            let found = line(&report, "check re-runs");
            assert_eq!(found.status, status, "{found:?}");
            assert!(found.detail.contains(says), "{found:?}");
            if status == Status::Warn {
                assert!(
                    matches!(&found.fix[..], [Step::Do(step)] if step.contains("Installed GitHub Apps")),
                    "{found:?}"
                );
                assert!(
                    found
                        .why
                        .as_deref()
                        .is_some_and(|why| why.contains("Actions (read and write)")),
                    "{found:?}"
                );
                assert!(report.ready(), "{report:?}");
            }
        }
    }

    /// OWL-665: a project whose `[forge] publish_gate_status` is on gets a
    /// `gate status` line from the GitHub App's installation: ok when it
    /// granted Commit statuses (read and write), a warning that blocks
    /// nothing otherwise, naming the installation's settings, information
    /// for a personal token or no installation; none with the switch off.
    #[test]
    fn the_gate_status_line_names_a_missing_statuses_write() {
        let system = || logged_in(with_harnesses(with_git(FakeSystem::default())));
        let tracker = r#"kind = "markdown"
admit = "delegation"
states = { ready = "Todo", working = "Doing", needs_input = "Asked", review = "Review" }"#;
        let (_dir, off) =
            project_tracked_by(&format!("{tracker}\n[forge]\npublish_gate_status = false"));
        let report = run(&system(), &off);
        assert!(
            !report.checks.iter().any(|c| c.subject == "gate status"),
            "{report:?}"
        );
        let (_dir, on) =
            project_tracked_by(&format!("{tracker}\n[forge]\npublish_gate_status = true"));
        let installed = |statuses_write| AppGrants::Installed {
            installation: 9,
            contents_write: false,
            actions_write: true,
            statuses_write,
        };
        for (grants, status, says) in [
            (
                installed(true),
                Status::Ok,
                "grants Commit statuses (read and write), which the gate's status needs",
            ),
            (
                installed(false),
                Status::Warn,
                "installation 9 on github.com/pitchopp/owlshift has not granted Commit statuses \
                 (read and write): no gate status is set",
            ),
            (AppGrants::NotInstalled, Status::Info, "no installation"),
            (AppGrants::PersonalToken, Status::Info, "personal token"),
        ] {
            let report = run(&system().github_app_grants_are(Ok(grants)), &on);
            let found = line(&report, "gate status");
            assert_eq!(found.status, status, "{found:?}");
            assert!(found.detail.contains(says), "{found:?}");
            if status == Status::Warn {
                assert!(
                    matches!(&found.fix[..], [Step::Do(step)] if step.contains("Installed GitHub Apps")),
                    "{found:?}"
                );
                assert!(report.ready(), "{report:?}");
            }
        }
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

    /// OWL-406: a `[harnesses.claude].fallback` in the personal file adds the
    /// `review fallback` line, which says the review roles alone use it and
    /// whether Codex can serve here, a warning when it cannot; none named,
    /// no line.
    #[test]
    fn the_review_fallback_line_says_who_uses_it_and_whether_it_can_serve() {
        let personal = |text: &str| Effective {
            project: FileState::NotApplicable("no project here".into()),
            personal: FileState::Loaded {
                path: PathBuf::from("/home/ada/.config/owlshift/config.toml"),
                config: owlshift_contracts::config::PersonalConfig::parse(text).unwrap(),
                entries: Vec::new(),
            },
        };
        let named = personal("[harnesses.claude]\nfallback = \"codex\"\n");
        let ready = logged_in(with_harnesses(with_git(FakeSystem::default())));
        let report = run(&ready, &personal("[harnesses.claude]\nbudget_usd = 5\n"));
        assert!(
            report
                .checks
                .iter()
                .all(|check| check.subject != "review fallback")
        );

        let report = run(&ready, &named);
        assert_eq!(line(&report, "codex").status, Status::Ok, "{report}");
        let fallback = line(&report, "review fallback");
        assert_eq!(fallback.status, Status::Ok, "{report}");
        assert!(
            fallback
                .detail
                .starts_with("Codex, used by the review roles only"),
            "{report}"
        );
        assert!(fallback.detail.contains("wait for the reset"), "{report}");

        let missing = logged_in(with_git(FakeSystem::default()).install("claude").answer(
            "claude --version",
            Answer::Exit(0, "2.1.283 (Claude Code)\n", ""),
        ));
        let report = run(&missing, &named);
        assert!(report.ready(), "{report}");
        let fallback = line(&report, "review fallback");
        assert_eq!(fallback.status, Status::Warn, "{report}");
        assert!(
            fallback
                .detail
                .ends_with("but it cannot serve here: `codex` is not on the PATH"),
            "{report}"
        );
    }

    /// OWL-405: the `usage` line shows each harness's cap and the shares its
    /// runs reported, and says which window holds the harness; nothing to
    /// show, no line.
    #[test]
    fn the_usage_line_shows_the_cap_and_the_shares_seen() {
        let personal = |text: &str| Effective {
            project: FileState::NotApplicable("no project here".into()),
            personal: FileState::Loaded {
                path: PathBuf::from("/home/ada/.config/owlshift/config.toml"),
                config: owlshift_contracts::config::PersonalConfig::parse(text).unwrap(),
                entries: Vec::new(),
            },
        };
        let file = owlshift_contracts::usage::HarnessUsage::from_state(
            Harness::Claude,
            &owlshift_core::usage::State {
                windows: [("five_hour", 0.85), ("seven_day", 0.3)]
                    .into_iter()
                    .map(|(name, share)| {
                        (
                            name.to_owned(),
                            owlshift_core::usage::Window {
                                share,
                                resets_at_ms: 4_102_444_800_000,
                                read_at_ms: 1,
                            },
                        )
                    })
                    .collect(),
                ..Default::default()
            },
        )
        .render();
        // The path as the check joins it, whatever the platform's separator.
        let path = Path::new("/home/ada/.local/share/owlshift")
            .join("usage")
            .join("claude.json");
        let usage = || {
            logged_in(with_harnesses(with_git(FakeSystem::default())))
                .text_file(path.to_str().unwrap(), Ok(&file))
        };
        let capped = personal("[harnesses.claude]\nusage_cap_percent = 80\n");
        let report = run(&usage(), &capped);
        let shown = line(&report, "usage");
        assert_eq!(shown.status, Status::Info, "{report}");
        assert!(
            shown.detail.starts_with(
                "Claude Code, cap 80%: five_hour 85% (holds the harness until it resets at \
                 2100-01-01T00:00:00Z), seven_day 30% (resets at 2100-01-01T00:00:00Z)"
            ),
            "{report}"
        );
        // No cap: the shares are still shown, none holds.
        let report = run(&usage(), &personal(""));
        let shown = line(&report, "usage");
        assert!(
            shown
                .detail
                .starts_with("Claude Code, no cap: five_hour 85% (resets"),
            "{report}"
        );
        // Nothing on record, no cap: no line.
        let report = run(
            &logged_in(with_harnesses(with_git(FakeSystem::default()))),
            &personal(""),
        );
        assert!(report.checks.iter().all(|check| check.subject != "usage"));
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

    /// OWL-537: a GitHub Issues project's lines, from one read of its
    /// repository: the Issues permission refused fails with its fix; a label
    /// the project names that the repository lacks, and an admitter GitHub
    /// does not know, warn, in GitHub's words; the proposals inbox is the
    /// `agent:proposed` label.
    #[test]
    fn a_github_operator_is_the_credentials_account() {
        let ready = || logged_in(with_harnesses(with_git(FakeSystem::default())));
        let (_dir, config) = project_tracked_with(
            r#"kind = "github"
admit = "ready"
admitters = ["ann"]
states = { ready = "agent:ready", working = "agent:working", needs_input = "agent:needs-input", review = "agent:review" }"#,
            "[people.ann]\ntracker = \"583231\"\nforge = \"ann\"",
        );
        let account = KeyAccount {
            id: "583231".to_owned(),
            name: "ann".to_owned(),
            app: false,
            workspace: String::new(),
        };
        let report = run(&ready().github_operator_is(Ok(account)), &config);
        let listed = line(&report, "people");
        assert!(listed.detail.ends_with("operator is ann"), "{listed:?}");
    }

    #[test]
    fn a_github_project_names_a_missing_permission_label_and_admitter() {
        use crate::tracker::GithubFacts;
        use owlshift_adapters::tracker::github::IssuesAccess;
        let ready = || logged_in(with_harnesses(with_git(FakeSystem::default())));
        let (_dir, config) = project_tracked_by(
            r#"kind = "github"
admit = "ready"
admitters = ["1", "404"]
states = { ready = "agent:ready", working = "agent:working", needs_input = "agent:needs-input", review = "agent:review" }"#,
        );
        let report = run(&ready(), &config);
        assert_eq!(line(&report, "tracker access").status, Status::Ok);
        assert_eq!(line(&report, "tracker labels").status, Status::Ok);
        assert_eq!(line(&report, "proposals inbox").status, Status::Ok);
        let admission = line(&report, "drain admission");
        assert!(
            admission.detail.ends_with("exist on GitHub"),
            "{admission:?}"
        );

        let refused = run(
            &ready().github_facts_are(Ok(GithubFacts {
                access: IssuesAccess::Refused(
                    "HTTP 403 Resource not accessible by integration".into(),
                ),
                labels: None,
                accounts: None,
            })),
            &config,
        );
        let access = line(&refused, "tracker access");
        assert_eq!(access.status, Status::Fail);
        assert!(access.detail.contains("HTTP 403"), "{access:?}");
        assert!(
            format!("{:?}", access.fix).contains("Issues (read and write) permission"),
            "{access:?}"
        );
        assert!(!refused.ready());
        assert!(refused.checks.iter().all(|c| c.subject != "tracker labels"));

        let lacking = run(
            &ready().github_facts_are(Ok(GithubFacts {
                access: IssuesAccess::Granted,
                labels: Some(vec![
                    "Agent:Ready".into(),
                    "agent:working".into(),
                    "agent:review".into(),
                ]),
                accounts: Some(vec![crate::system::AccountFact {
                    id: "1".into(),
                    app: false,
                    active: true,
                }]),
            })),
            &config,
        );
        let labels = line(&lacking, "tracker labels");
        assert_eq!(labels.status, Status::Warn);
        assert_eq!(
            labels.detail,
            "github.com/pitchopp/owlshift has no label \"agent:needs-input\", \"agent:proposed\""
        );
        let admission = line(&lacking, "drain admission");
        assert_eq!(admission.status, Status::Warn);
        assert_eq!(admission.detail, "admitter \"404\" is no GitHub account");
        assert!(
            admission.why.as_deref().unwrap().contains("numeric ids"),
            "{admission:?}"
        );
        assert_eq!(line(&lacking, "proposals inbox").status, Status::Warn);
        assert!(lacking.ready(), "{lacking}");
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
        // OWL-448: a Build waiting for its admission is named as such.
        let mut refs = refs;
        let (owl_174, RefState::Read(Some(building))) = at("OWL-174", Stage::Build) else {
            unreachable!("a ref at Build")
        };
        refs.push((owl_174, RefState::Unadmitted(building)));
        let report = run(&ready().ticket_refs_are(Ok(refs.clone())), &config);
        assert!(report.ready(), "{report}");
        let warned = line(&report, "drain slot");
        assert_well_formed(warned);
        assert_eq!(warned.status, Status::Warn);
        assert_eq!(
            warned.detail,
            "held by OWL-9, whose ticket ref cannot be read; OWL-80, whose state does not read; \
             OWL-173 at Build; OWL-174 at Build, waiting for its resources (`owlshift why \
             OWL-174` names what holds them); OWL-175 at Verify"
        );
        let why = warned.why.as_deref().unwrap();
        for fix in [
            "`owlshift forget OWL-9` to start it over",
            "`owlshift forget OWL-80` to start it over",
            "`owlshift continue OWL-173` to finish its Build or `owlshift forget OWL-173`",
            "`owlshift continue OWL-174` to finish its Build or `owlshift forget OWL-174`",
            "`owlshift do OWL-175` to run it again or `owlshift forget OWL-175`",
        ] {
            assert!(why.contains(fix), "{fix}: {why}");
        }
        assert!(!why.contains("not a commit"), "{why}");

        // OWL-401: under a capacity of 6, five holders are no warning.
        let mut config = config;
        if let FileState::Loaded {
            config: project, ..
        } = &mut config.project
        {
            project.policy.max_in_flight = Some(6);
        }
        let report = run(&ready().ticket_refs_are(Ok(refs)), &config);
        let found = line(&report, "drain slot");
        assert_eq!(found.status, Status::Ok);
        assert!(
            found.detail.starts_with("5 of 6 held, by OWL-9"),
            "{found:?}"
        );
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

    /// OWL-368, OWL-400: a quarantine that stands fails the report, showing
    /// what it says and how to clear it, and so does a run's obligation left
    /// behind, which the next command turns into one; a run in flight, or a
    /// quarantine that cannot be read, is information; none is ok.
    #[test]
    fn a_quarantine_fails_with_its_marker_and_how_to_clear_it() {
        use crate::ticket_run::{Found, Standing};
        use owlshift_contracts::step::RunObligation;

        let ready = || logged_in(with_harnesses(with_git(FakeSystem::default())));
        let (_dir, config) = markdown_project();
        let path =
            PathBuf::from("/home/ada/.local/share/owlshift/projects/github.com/o/r/quarantine");
        let marked = MarkerState {
            quarantine: Some((
                path.clone(),
                "Run r1 of OWL-160 broke isolation:\n- the main checkout's HEAD moved\n".to_owned(),
            )),
            obligations: Vec::new(),
        };
        let report = run(&ready().quarantine_is(Ok(marked)), &config);
        assert!(!report.ready(), "{report}");
        let failed = line(&report, "quarantine");
        assert_eq!(failed.status, Status::Fail);
        // The quarantine's path as the report writes every path: `~` for the
        // fake home where that home is absolute, as on Unix.
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
            "~/.local/share/owlshift/projects/github.com/o/r/quarantine"
        );
        assert!(
            failed
                .fix
                .iter()
                .any(|step| matches!(step, Step::Run { command, .. } if command == "owlshift unquarantine")),
            "{failed:?}"
        );
        let obligation = |what: &str, running: bool| Standing {
            found: Found {
                name: "owl-3-r3".to_owned(),
                record: Ok(RunObligation {
                    format: owlshift_contracts::format::Format,
                    ticket: Some(owlshift_contracts::ids::TicketId::new("OWL-3").unwrap()),
                    run: "r3".to_owned(),
                    started: "2026-10-21T08:00:00Z".parse().unwrap(),
                    what: what.to_owned(),
                }),
            },
            running,
        };
        let only = |standing: Standing| MarkerState {
            quarantine: None,
            obligations: vec![standing],
        };
        let left = run(
            &ready().quarantine_is(Ok(only(obligation("Run r3 of OWL-3", false)))),
            &config,
        );
        let failed = line(&left, "quarantine");
        assert_eq!(failed.status, Status::Fail, "{left}");
        assert_eq!(
            failed.detail,
            "runs ended before their isolation check passed: Run r3 of OWL-3"
        );
        for (system, status, detail) in [
            (ready(), Status::Ok, "not quarantined"),
            (
                ready().quarantine_is(Ok(only(obligation("Run r3 of OWL-3", true)))),
                Status::Info,
                "not quarantined; runs in flight: Run r3 of OWL-3",
            ),
            (
                ready().quarantine_is(Err("no remote".to_owned())),
                Status::Info,
                "the quarantine was not read: no remote",
            ),
        ] {
            let report = run(&system, &config);
            let found = line(&report, "quarantine");
            assert_eq!(
                (found.status, found.detail.as_str()),
                (status, detail),
                "{report}"
            );
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
        let no_app = ready()
            .unstored(own().of(LINEAR_APP_ID_ACCOUNT))
            .unstored(own().of(LINEAR_APP_SECRET_ACCOUNT));
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

    /// OWL-650: the daemon's pause on this checkout's project fails a
    /// `service pause` line with its reason whole; a sign-in notice for the
    /// registered checkout gets the command and its folder, any other reason
    /// a step to do. A person's pause, or none, gives no line.
    #[test]
    fn the_daemons_pause_says_what_to_do() {
        use jiff::Timestamp;
        use owlshift_contracts::service::{BY_PAUSE, Request};

        let data = tempfile::tempdir().unwrap();
        let system = FakeSystem::default()
            .data_dir_is(Some((data.path().to_owned(), DataDirSource::Platform)));
        let config = Effective {
            project: FileState::Absent(PathBuf::from("/home/ada/p/owlshift.toml")),
            personal: FileState::NotApplicable("none".to_owned()),
        };
        let repo = Repo::parse("pitchopp/owlshift").unwrap();
        let checkout = data.path().join("my checkout");
        served::Registry::in_data_dir(data.path())
            .register(&repo, &checkout)
            .unwrap();
        let dirs = ProjectDirs::new(data.path(), &repo);
        assert_eq!(service_pause_check(&system, &config), None);

        let pause = |by: &str, why: &str| {
            requests::remove_own_pause(&dirs).unwrap();
            let _ = std::fs::remove_file(requests::pause_file(&dirs));
            requests::write_pause(&dirs, &Request::new(by, Timestamp::now(), why)).unwrap();
            service_pause_check(&system, &config)
        };
        let notice = sign_in_notice(&repo, &checkout, true);
        let check = pause(BY_DAEMON, &notice).unwrap();
        assert_eq!(check.status, Status::Fail);
        assert_eq!(check.detail, notice);
        assert_eq!(check.fix, [sign_in_step(&checkout)]);
        let Step::Run { command, note } = &check.fix[0] else {
            panic!("{check:?}");
        };
        assert_eq!(command, "owlshift init");
        assert!(
            note.as_deref()
                .unwrap()
                .contains(&checkout.display().to_string()),
            "{note:?}"
        );
        assert_well_formed(&check);

        let check = pause(BY_DAEMON, "the keychain refused").unwrap();
        assert_eq!(check.detail, "the keychain refused");
        assert!(matches!(&check.fix[..], [Step::Do(_)]), "{check:?}");
        assert_well_formed(&check);

        assert_eq!(pause(BY_PAUSE, "lunch"), None);
    }

    /// OWL-650: doctor's forge access line, for a GitHub App token only a
    /// new sign-in mends, gives the notice the service shows, for the
    /// registered checkout or else the project file's folder, and the
    /// command from there; an access token past its expiry names the same
    /// command. Neither gives the keychain's fix.
    #[test]
    fn a_needed_sign_in_names_the_notice_and_its_folder() {
        let ready = || logged_in(with_harnesses(with_git(FakeSystem::default())));
        let (dir, owl) = markdown_project();
        let folder = match &owl.project {
            FileState::Loaded { path, .. } => path.parent().unwrap().to_owned(),
            other => panic!("{other:?}"),
        };
        let repo = Repo::parse("pitchopp/owlshift").unwrap();
        let technical =
            crate::forge::sign_in_again("github.com/pitchopp/owlshift/github-app", "does not read");
        let access = |system: &FakeSystem| {
            let report = run(system, &owl);
            (line(&report, "forge access").clone(), report.ready())
        };
        let no_keychain_fix = |check: &Check| {
            for step in &check.fix {
                let (Step::Run { command: text, .. } | Step::Do(text)) = step;
                assert!(!text.contains(KEYCHAIN_FIX), "{check:?}");
            }
        };

        // Served: the registered checkout, and the notice's last sentence.
        let data = tempfile::tempdir().unwrap();
        let checkout = dir.path().join("registered");
        served::Registry::in_data_dir(data.path())
            .register(&repo, &checkout)
            .unwrap();
        let served =
            || ready().data_dir_is(Some((data.path().to_owned(), DataDirSource::Platform)));
        for answer in [
            Err(TokenTryError::SignIn(technical.clone())),
            Err(TokenTryError::Forge(owlshift_adapters::forge::Error::new(
                ForgeErrorKind::SignInAgain,
                technical.clone(),
            ))),
        ] {
            let (check, ready_to_run) = access(&served().github_default_branch_is(answer));
            assert_eq!(check.status, Status::Fail, "{check:?}");
            assert!(!ready_to_run);
            assert_eq!(check.detail, sign_in_notice(&repo, &checkout, true));
            assert_eq!(check.fix, [sign_in_step(&checkout)]);
            let why = check.why.as_deref().unwrap();
            assert!(
                why.contains(&format!("What doctor found: {technical}.")),
                "{why}"
            );
            no_keychain_fix(&check);
        }

        // Not served: the project file's folder, without the service's
        // sentence.
        let (check, _) =
            access(&ready().github_default_branch_is(Err(TokenTryError::SignIn(technical))));
        assert_eq!(check.detail, sign_in_notice(&repo, &folder, false));
        assert!(!check.detail.contains("within a minute"), "{check:?}");
        assert_eq!(check.fix, [sign_in_step(&folder)]);

        // Past its expiry: the same command, in plain words.
        let expired =
            crate::forge::sign_in_again("github.com/pitchopp/owlshift/github-app", "has expired");
        let (check, ready_to_run) =
            access(&ready().github_default_branch_is(Err(TokenTryError::Expired(expired))));
        assert_eq!(check.status, Status::Fail, "{check:?}");
        assert!(!ready_to_run);
        assert!(check.detail.contains("past its expiry"), "{check:?}");
        let [Step::Run { command, note }] = &check.fix[..] else {
            panic!("{check:?}");
        };
        assert_eq!(command, "owlshift init");
        let note = note.as_deref().unwrap();
        assert!(note.contains(&folder.display().to_string()), "{note}");
        assert!(note.contains("keeps your owlshift.toml"), "{note}");
        no_keychain_fix(&check);
    }

    /// OWL-650: the lines that read the app's grants point at the forge
    /// access line for a token that needs a new sign-in or is past its
    /// expiry, without its technical words.
    #[test]
    fn the_grants_lines_point_at_the_forge_access_line() {
        let system = || logged_in(with_harnesses(with_git(FakeSystem::default())));
        let tracker = r#"kind = "markdown"
admit = "delegation"
states = { ready = "Todo", working = "Doing", needs_input = "Asked", review = "Review" }"#;
        let (_dir, all) = project_tracked_by(&format!("{tracker}\n[forge]\nmerge = \"all\""));
        for (error, says) in [
            (
                TokenTryError::SignIn("SENTINEL".to_owned()),
                "not checked: the GitHub App's token needs a new sign-in; the forge access line \
                 says how",
            ),
            (
                TokenTryError::Expired("SENTINEL".to_owned()),
                "not checked: the GitHub App's token is past its expiry; the forge access line \
                 says how",
            ),
        ] {
            let report = run(&system().github_app_grants_are(Err(error)), &all);
            let found = line(&report, "merge on green");
            assert_eq!(found.status, Status::Info, "{found:?}");
            assert_eq!(found.detail, says);
        }
    }

    /// OWL-434: doctor names the file the service runs a served project on,
    /// the one kept from the default branch, and says nothing of a project
    /// it does not serve.
    #[test]
    fn a_served_project_names_the_file_the_service_runs_it_on() {
        use jiff::Timestamp;
        use owlshift_contracts::service::{KeptFile, RefusedFile, ServedConfig};

        let data = tempfile::tempdir().unwrap();
        let system = FakeSystem::default()
            .data_dir_is(Some((data.path().to_owned(), DataDirSource::Platform)));
        let config = Effective {
            project: FileState::Absent(PathBuf::from("/home/ada/p/owlshift.toml")),
            personal: FileState::NotApplicable("none".to_owned()),
        };
        let repo = Repo::parse("pitchopp/owlshift").unwrap();
        assert_eq!(served_check(&system, &config), None);

        served::Registry::in_data_dir(data.path())
            .register(&repo, &data.path().join("p"))
            .unwrap();
        let check = served_check(&system, &config).unwrap();
        assert_eq!(check.status, Status::Info);
        assert!(check.detail.contains("has read no project file yet"));

        let dirs = crate::project::ProjectDirs::new(data.path(), &repo);
        let write = |record: &ServedConfig| {
            crate::step::write_atomically(&served::record_file(&dirs), &record.render()).unwrap();
        };
        let refused = RefusedFile {
            commit: None,
            reason: "owlshift.toml at origin/main (bbbbbbbbbbbb): invalid".to_owned(),
            since: "2026-10-24T09:00:00Z".parse::<Timestamp>().unwrap(),
        };
        let mut record = ServedConfig::new("pitchopp/owlshift");
        record.refused = Some(refused.clone());
        write(&record);
        let check = served_check(&system, &config).unwrap();
        assert_eq!(check.status, Status::Warn);
        assert_eq!(
            check.detail,
            "not served: owlshift.toml at origin/main (bbbbbbbbbbbb): invalid"
        );
        assert_well_formed(&check);

        record.kept = Some(KeptFile {
            branch: "main".to_owned(),
            commit: "a".repeat(40),
            text: String::new(),
            since: "2026-10-24T08:00:00Z".parse::<Timestamp>().unwrap(),
        });
        write(&record);
        let check = served_check(&system, &config).unwrap();
        assert_eq!(check.status, Status::Warn);
        assert!(
            check.detail.starts_with(
                "the service runs it on owlshift.toml at origin/main (aaaaaaaaaaaa), not this \
                 working tree; the default branch's is refused since 2026-10-24T09:00:00Z: "
            ),
            "{}",
            check.detail
        );
        assert_well_formed(&check);

        record.refused = None;
        write(&record);
        let check = served_check(&system, &config).unwrap();
        assert_eq!(check.status, Status::Ok);
        assert_eq!(
            check.detail,
            "the service runs it on owlshift.toml at origin/main (aaaaaaaaaaaa), not this \
             working tree"
        );
    }

    #[cfg(unix)]
    /// OWL-437: the service line says whether the service is installed, the
    /// binary it starts and the daemon that runs, and warns, with what removes
    /// it, of a unit the manifest does not list, a binary or a listed file
    /// that is gone, or a manifest that does not read.
    #[test]
    fn the_service_line_reports_the_install_and_its_leftovers() {
        use owlshift_contracts::format::Format;
        use owlshift_contracts::service::{DaemonRecord, InstalledService, ServiceManager};
        use owlshift_platform::service::Layout;

        let layout = Layout {
            kind: Kind::Launchd,
            dir: PathBuf::from("/home/ada/Library/LaunchAgents"),
        };
        let nothing = ServiceState {
            layout: layout.clone(),
            manifest: PathBuf::from("/home/ada/data/service/manifest.json"),
            listed: Ok(None),
            gone: Vec::new(),
            binary_there: true,
            unlisted: Vec::new(),
            daemon: Ok(None),
            drain: None,
        };
        let home = Some(Path::new("/home/ada"));
        let check = service_check(&nothing, home);
        assert_eq!(check.status, Status::Info);
        assert_eq!(
            check.detail,
            "not installed at login (`owlshift start --at-login` installs it); no daemon runs"
        );

        let service = InstalledService {
            manager: ServiceManager::Launchd,
            unit: layout.unit().display().to_string(),
            launcher: layout.launcher().display().to_string(),
            link: None,
            binary: "/opt/homebrew/bin/owlshift".to_owned(),
        };
        let running = ServiceState {
            listed: Ok(Some(service)),
            daemon: Ok(Some(Some(DaemonRecord {
                format: Format,
                pid: 4242,
                version: "0.4.0".to_owned(),
                binary: "/opt/homebrew/bin/owlshift".to_owned(),
                since: "2026-10-25T08:00:00Z".parse().unwrap(),
                build: None,
                watches: None,
            }))),
            ..nothing.clone()
        };
        let check = service_check(&running, home);
        assert_eq!(check.status, Status::Ok);
        assert_eq!(
            check.detail,
            "installed at login as the user LaunchAgent `dev.owlshift.daemon`, starting \
             /opt/homebrew/bin/owlshift; the daemon runs (pid 4242, version 0.4.0, since \
             2026-10-25T08:00:00Z)"
        );
        // OWL-436: an upgrade's drain asked of it.
        let draining = ServiceState {
            drain: Some(owlshift_contracts::service::Request::new(
                "owlshift doctor",
                "2026-10-28T08:00:00Z".parse().unwrap(),
                "an upgrade from version 0.4.0 to 0.5.0, at /opt/homebrew/bin/owlshift",
            )),
            ..running.clone()
        };
        let check = service_check(&draining, home);
        assert_eq!(check.status, Status::Ok);
        assert!(
            check.detail.ends_with(
                "since 2026-10-25T08:00:00Z), draining for an upgrade (owlshift doctor: an \
                 upgrade from version 0.4.0 to 0.5.0, at /opt/homebrew/bin/owlshift): the service \
                 at login starts it again once its steps under way finish"
            ),
            "{}",
            check.detail
        );

        let broken = ServiceState {
            binary_there: false,
            gone: vec![layout.launcher().display().to_string()],
            ..running.clone()
        };
        let check = service_check(&broken, home);
        assert_eq!(check.status, Status::Warn);
        let why = check.why.clone().unwrap();
        assert!(
            why.contains("its binary /opt/homebrew/bin/owlshift is gone")
                && why.contains("lists ~/Library/LaunchAgents/owlshift-launcher.sh, which is gone"),
            "{why}"
        );
        assert_well_formed(&check);

        let leftover = ServiceState {
            unlisted: vec![layout.unit()],
            ..nothing.clone()
        };
        let check = service_check(&leftover, home);
        assert_eq!(check.status, Status::Warn);
        let why = check.why.clone().unwrap();
        assert!(
            why.contains(
                "~/Library/LaunchAgents/dev.owlshift.daemon.plist is Owlshift's but not \
                 listed"
            ) && why.contains("launchctl bootout gui/$(id -u)/dev.owlshift.daemon; rm -f"),
            "{why}"
        );

        let unread = ServiceState {
            listed: Err("invalid install manifest".to_owned()),
            ..nothing
        };
        let check = service_check(&unread, home);
        assert_eq!(check.status, Status::Warn);
        assert!(
            check
                .why
                .unwrap()
                .contains("fix or remove ~/data/service/manifest.json")
        );

        let system = logged_in(with_harnesses(with_git(FakeSystem::default())));
        let report = run(&system.service_is(running), &no_config());
        assert!(report.checks.iter().any(|c| c.subject == "service"));
    }

    /// A build of the commit `commit` repeated, `minute` minutes after a
    /// fixed time, with no source tree; this binary's is `built('a', false,
    /// 0)`.
    fn built(commit: char, dirty: bool, minute: i64) -> BuildIdentity {
        BuildIdentity {
            commit: Some(commit.to_string().repeat(40)),
            dirty,
            built_at: jiff::Timestamp::from_second(1_761_600_000 + minute * 60).unwrap(),
            source: None,
        }
    }

    fn process(
        project: Option<&str>,
        served: bool,
        pid: u32,
        build: Option<BuildIdentity>,
    ) -> Running {
        Running {
            project: project.map(str::to_owned),
            served,
            pid: Some(pid),
            version: Some(OWLSHIFT_VERSION.to_owned()),
            build,
            replaced: false,
        }
    }

    /// The line `owlshift doctor` shows for what runs, with no other
    /// answer scripted than git's own.
    fn builds_line(running: RunningBuilds) -> Option<Check> {
        let system = logged_in(with_harnesses(with_git(FakeSystem::default())));
        let report = run(&system.running_builds_are(running), &no_config());
        report
            .checks
            .into_iter()
            .find(|check| check.subject == "running builds")
    }

    /// OWL-462: a watch on another build is warned, naming its project and
    /// both builds, with what to do; one on this build gets nothing; a
    /// daemon's record from before OWL-462 is an unknown build, not this
    /// one; a file replaced on disk is told even on this build; a draining
    /// daemon is left to its drain. Never a failure, and no line when
    /// nothing runs.
    #[test]
    fn a_running_build_other_than_doctors_is_warned_with_both_identities() {
        let own = built('a', false, 0);
        let daemon = process(None, false, 10, Some(built('a', false, 5)));
        let older = process(Some("acme/web"), false, 77, Some(built('b', true, -10)));
        let mut replaced = process(Some("acme/api"), true, 78, Some(own.clone()));
        replaced.replaced = true;
        let check = builds_line(RunningBuilds {
            processes: vec![daemon.clone(), older, replaced],
            drain: None,
            moves_itself: false,
        })
        .unwrap();
        assert_eq!(check.status, Status::Warn, "{check:?}");
        let why = check.why.unwrap();
        assert!(
            why.contains(&format!(
                "the watch of acme/web (pid 77, typed in a terminal) runs {}, not this binary's \
                 {}: stop it in its terminal",
                built('b', true, -10).short(),
                own.short()
            )),
            "{why}"
        );
        assert!(
            why.contains(
                "the watch of acme/api (pid 78, run by the daemon) was started from a file since \
                 replaced on disk: run `owlshift stop`, then `owlshift start`"
            ),
            "{why}"
        );
        assert!(
            !why.contains("pid 10"),
            "a clean rebuild of one commit: {why}"
        );

        let matching = builds_line(RunningBuilds {
            processes: vec![daemon],
            drain: None,
            moves_itself: false,
        })
        .unwrap();
        assert_eq!(matching.status, Status::Ok, "{matching:?}");
        assert_eq!(
            matching.detail,
            format!(
                "the daemon (pid 10) runs this binary's build ({})",
                own.short()
            )
        );

        let unknown = builds_line(RunningBuilds {
            processes: vec![process(None, false, 10, None)],
            drain: None,
            moves_itself: false,
        })
        .unwrap();
        assert_eq!(unknown.status, Status::Warn);
        assert!(unknown.why.unwrap().contains(
            "the daemon (pid 10) records no build, being from before OWL-462: run `owlshift \
                 stop`, then `owlshift start`"
        ));

        let draining = builds_line(RunningBuilds {
            processes: vec![process(None, false, 10, Some(built('c', false, -5)))],
            drain: Some(owlshift_contracts::service::Request::new(
                "owlshift doctor",
                jiff::Timestamp::now(),
                "an upgrade",
            )),
            moves_itself: true,
        })
        .unwrap();
        assert!(
            draining
                .why
                .unwrap()
                .contains("let the upgrade's drain asked of the daemon end"),
        );
        // OWL-616: a daemon that watches this binary moves by itself, its
        // served watches with it: nothing to type.
        let moving = builds_line(RunningBuilds {
            processes: vec![
                process(None, false, 10, Some(built('c', false, -5))),
                process(Some("acme/web"), true, 77, Some(built('c', false, -5))),
            ],
            drain: None,
            moves_itself: true,
        })
        .unwrap();
        let why = moving.why.unwrap();
        assert_eq!(
            why.matches("the daemon moves onto this binary by itself")
                .count(),
            2,
            "{why}"
        );
        assert!(!why.contains("`owlshift stop`"), "{why}");

        let newer = builds_line(RunningBuilds {
            processes: vec![process(
                Some("acme/web"),
                false,
                77,
                Some(built('d', false, 9)),
            )],
            drain: None,
            moves_itself: false,
        })
        .unwrap();
        assert!(newer.why.unwrap().contains("this binary is the older one"));

        assert_eq!(builds_line(RunningBuilds::default()), None);
    }

    /// OWL-462: a running build whose commit is behind `origin/HEAD` of the
    /// tree it was built from, as last fetched, is warned with what to
    /// rebuild; `origin/main` stands in for a missing `origin/HEAD`; git
    /// failing on both, or missing, says nothing. One look per tree and
    /// commit.
    #[test]
    fn a_running_build_behind_its_source_is_warned() {
        let from_tree = BuildIdentity {
            source: Some("/home/ada/owlshift".to_owned()),
            ..built('a', false, 0)
        };
        let running = RunningBuilds {
            processes: vec![
                process(None, false, 10, Some(from_tree.clone())),
                process(Some("acme/web"), true, 77, Some(from_tree.clone())),
            ],
            drain: None,
            moves_itself: false,
        };
        let head = format!("git rev-list --count {}..origin/HEAD", "a".repeat(40));
        let main = format!("git rev-list --count {}..origin/main", "a".repeat(40));
        let system = |head_answer: Answer, main_answer: Answer| {
            logged_in(with_harnesses(with_git(FakeSystem::default())))
                .answer(&head, head_answer)
                .answer(&main, main_answer)
                .running_builds_are(running.clone())
        };
        let line = |system: &FakeSystem| {
            run(system, &no_config())
                .checks
                .into_iter()
                .find(|check| check.subject == "running builds")
                .unwrap()
        };

        let behind = line(&system(Answer::Exit(0, "3\n", ""), Answer::TimedOut));
        assert_eq!(behind.status, Status::Warn);
        let why = behind.why.unwrap();
        // `~/owlshift` on Unix; the whole path where `/home/ada` is no
        // absolute home.
        let tree = shown(
            Path::new("/home/ada/owlshift"),
            Some(Path::new("/home/ada")),
        );
        assert!(
            why.contains(&format!(
                "the daemon (pid 10) is 3 commits behind origin/HEAD of {tree}, as that tree \
                 last fetched it: rebuild and install `owlshift` from {tree} once that tree is up \
                 to date with its origin, then run `owlshift stop`, then `owlshift start`"
            )),
            "{why}"
        );
        assert!(why.contains("the watch of acme/web (pid 77, run by the daemon) is 3"));

        let fallback = line(&system(
            Answer::Exit(128, "", "fatal: bad revision"),
            Answer::Exit(0, "1\n", ""),
        ));
        assert!(
            fallback
                .why
                .unwrap()
                .contains("is 1 commit behind origin/main")
        );

        let failing = || Answer::Exit(128, "", "fatal: bad revision");
        let silent = line(&system(failing(), failing()));
        assert_eq!(silent.status, Status::Ok, "{silent:?}");

        let no_git = builds_check(
            &FakeSystem::default(),
            Ok(running),
            &built('a', false, 0),
            None,
        );
        assert_eq!(no_git.unwrap().status, Status::Ok);
    }

    /// OWL-467: while a daemon runs, the web UI's line: ok once proved,
    /// naming 127.0.0.1 and the port; a warning, never a failure, naming
    /// the port and `[ui] port` when another program may hold it, and the
    /// daemon's log when the UI is down; no line without a daemon.
    #[test]
    fn the_web_ui_is_checked_while_a_daemon_runs() {
        let system = || logged_in(with_harnesses(with_git(FakeSystem::default())));
        let report = run(&system(), &no_config());
        assert!(
            report.checks.iter().all(|check| check.subject != "web UI"),
            "{report}"
        );

        let record = owlshift_contracts::service::UiRecord {
            format: owlshift_contracts::format::Format,
            pid: 4242,
            port: 18466,
            since: "2026-11-05T09:00:00Z".parse().unwrap(),
            build: None,
        };
        let report = run(&system().ui_is(Probe::Proved(Some(record))), &no_config());
        let check = line(&report, "web UI");
        assert_eq!(check.status, Status::Ok, "{report}");
        assert_eq!(
            check.detail,
            "listens on 127.0.0.1:18466 (pid 4242, since 2026-11-05T09:00:00Z)"
        );
        let json = report.to_json();
        let entry = json["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|check| check["subject"] == "web UI")
            .unwrap();
        assert_eq!(entry["section"], "project");
        assert_eq!(entry["status"], "ok");

        for probe in [
            Probe::NotProved("its answer is not the web UI's proof".into()),
            Probe::Silent { recorded: false },
        ] {
            let report = run(&system().ui_is(probe), &no_config());
            let check = line(&report, "web UI");
            assert_eq!(check.status, Status::Warn, "{report}");
            assert!(
                check.detail.contains("port 18466 of 127.0.0.1")
                    && check.detail.contains("`[ui] port`"),
                "{}",
                check.detail
            );
            assert!(report.ready(), "{report}");
        }
        // The fake's home is not absolute on Windows: the log shows whole.
        let log = shown(
            &crate::daemon::service_dir(Path::new("/home/ada/.local/share/owlshift"))
                .join("daemon.log"),
            Some(Path::new("/home/ada")),
        );
        #[cfg(unix)]
        assert_eq!(log, "~/.local/share/owlshift/service/daemon.log");
        for probe in [Probe::Silent { recorded: true }, Probe::Refused] {
            let report = run(&system().ui_is(probe), &no_config());
            let check = line(&report, "web UI");
            assert_eq!(check.status, Status::Warn, "{report}");
            assert!(check.detail.contains("127.0.0.1:18466"), "{}", check.detail);
            let why = check.why.as_deref().unwrap();
            assert!(why.contains(&log), "{why}");
            assert!(report.ready(), "{report}");
        }
        let report = run(
            &system().ui_is(Probe::Unread("its answer was cut short".into())),
            &no_config(),
        );
        let check = line(&report, "web UI");
        assert_eq!(check.status, Status::Warn, "{report}");
        assert!(check.detail.contains("cut short"), "{}", check.detail);
    }
}
