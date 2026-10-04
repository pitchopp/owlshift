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
//! Each check carries its own texts: a failure says why the check exists and
//! the steps that fix it, a warning says why it does not block `owlshift do`.
//! How the report is laid out, as text or JSON, is [`render`]'s (OWL-99).

pub mod render;

use std::path::{MAIN_SEPARATOR, Path};

use owlshift_adapters::harness::{self, Login, tested};
use owlshift_adapters::tracker::linear::{LinearTracker, StateMismatch, find_state};
use owlshift_adapters::tracker::markdown::MarkdownTracker;
use owlshift_adapters::tracker::{Capability, ErrorKind};
use owlshift_contracts::Harness;
use owlshift_contracts::config::{States, TrackerKind};
use owlshift_platform::keychain::SERVICE;
use owlshift_platform::sandbox::{BWRAP_APPARMOR_PROFILE, SandboxError};
use serde_json::Value;

use crate::config::{Effective, FileState, exit_text};
use crate::executor::harness::CLAUDE_AGENT_ACCOUNT;
use crate::system::{
    AppError, DataDirSource, RunError, StatesError, System, exact_version_of, version_of,
};
#[cfg(unix)]
use crate::system::{SentinelProbe, SentinelStatus};
use crate::tracker::{LINEAR_ACCOUNT, LINEAR_APP_ID_ACCOUNT, LINEAR_APP_SECRET_ACCOUNT};

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
    checks.push(file_check("project config", &config.project, home));
    checks.push(file_check("personal config", &config.personal, home));
    checks.push(tracker_check(config));
    checks.extend(tracker_states_check(system, config));
    checks.extend(tracker_identity_check(system, config));
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
        Ok(out) if out.code == Some(0) => match version_of(&out.stdout) {
            Some(version) => Check::ok(
                Section::Tools,
                SUBJECT,
                format!("{version} ({})", shown(&path, home)),
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

/// The step that fixes a keychain doctor cannot read.
const KEYCHAIN_FIX: &str = "Make the system keychain available: unlock it on macOS; on Linux, \
                            start a Secret Service such as GNOME Keyring";

/// The harnesses declared in the personal file, or both when it declares
/// none (scenario S15). Codex that is not ready is only a warning until P5: see
/// [`not_needed_before_review_roles`].
fn required_harnesses(config: &Effective) -> Vec<Harness> {
    if let FileState::Loaded { config, .. } = &config.personal {
        let declared: Vec<Harness> = config.harnesses.iter().map(|(h, _)| h).collect();
        if !declared.is_empty() {
            return declared;
        }
    }
    vec![Harness::Claude, Harness::Codex]
}

/// Why a harness must be ready at all.
fn harness_why(harness: Harness) -> &'static str {
    match harness {
        Harness::Claude => {
            "`owlshift do` runs every agent in Claude Code, so it must be installed. \
             Agent runs log in with the token the `claude agent login` line checks."
        }
        Harness::Codex => "Codex runs the review roles from P5; `owlshift do` does not use it yet.",
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
        return not_needed_before_review_roles(missing, harness);
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
    flag_untested(&mut result, harness, exact.as_deref());
    not_needed_before_review_roles(result, harness)
}

/// Codex that is missing, logged out or unreadable is a warning, not a
/// failure: `owlshift do` does not run it before the review roles (P5), so
/// it is not a problem to fix before `do` (OWL-102). The decision is made
/// here and only here.
///
/// P5 must replace the unconditional downgrade with a condition: Codex stays
/// a failure once a configured role uses it. No configuration maps roles to
/// harnesses yet, so there is nothing to read today.
fn not_needed_before_review_roles(mut check: Check, harness: Harness) -> Check {
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
        "{DOES_NOT_BLOCK} it does not use Codex yet; the review roles will, from P5, and \
         then Codex must be ready. To get it ready: {}.",
        steps.join(", then ")
    ));
    check.fix = Vec::new();
    check
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
        let mut why = match harness {
            Harness::Codex => format!(
                "{DOES_NOT_BLOCK} it does not use Codex yet; the review roles will, from P5."
            ),
            Harness::Claude => format!("{DOES_NOT_BLOCK} an untested version usually works."),
        };
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

/// Whether each state named under `[tracker].states` is exactly one
/// workflow state of the project's Linear team (OWL-147): the rule by which
/// the stage moves. A name that matches none or several only keeps the stage
/// from moving, which never stops `owlshift do`, so it is a warning; what
/// keeps `do` from reading the ticket at all (no key, a rejected key, a
/// keychain it cannot read, a team Linear does not know) is a failure. None
/// for a Markdown project or none loaded: only a Linear project opens the
/// keychain and the network here, once each.
fn tracker_states_check(system: &dyn System, config: &Effective) -> Option<Check> {
    const SUBJECT: &str = "tracker states";
    let FileState::Loaded { config, .. } = &config.project else {
        return None;
    };
    let (TrackerKind::Linear, Some(team)) = (config.tracker.kind, &config.tracker.team) else {
        return None;
    };
    let place = format!("in the system keychain (service `{SERVICE}`, account `{LINEAR_ACCOUNT}`)");
    let fail = |detail: String, why: &str, fix: Vec<Step>| {
        Some(Check::fail(Section::Project, SUBJECT, detail, why, fix))
    };
    let names = match system.linear_states(team) {
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
    let configured = configured_states(&config.tracker.states);
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
fn tracker_identity_check(system: &dyn System, config: &Effective) -> Option<Check> {
    const SUBJECT: &str = "tracker identity";
    let FileState::Loaded { config, .. } = &config.project else {
        return None;
    };
    let (TrackerKind::Linear, Some(team)) = (config.tracker.kind, &config.tracker.team) else {
        return None;
    };
    let place = format!(
        "in the system keychain (service `{SERVICE}`, accounts `{LINEAR_APP_ID_ACCOUNT}` and \
         `{LINEAR_APP_SECRET_ACCOUNT}`)"
    );
    let why = "With the Linear app stored, `owlshift do` writes as its app user, checked before \
               any work, and refuses rather than write through the API key.";
    let store_both = || {
        vec![Step::run_noting(
            "owlshift init --replace-secrets",
            "give the Linear app's client ID and secret when asked",
        )]
    };
    let fail =
        |detail: String, fix: Vec<Step>| Check::fail(Section::Project, SUBJECT, detail, why, fix);
    Some(match system.linear_app(team) {
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
        Err(AppError::NoKey) => Check::info(
            Section::Project,
            SUBJECT,
            "not checked: no Linear API key, which the Linear app's check needs",
        ),
        Err(AppError::Keychain(reason)) => fail(
            format!("not checked: could not read the Linear app's credentials {place}: {reason}"),
            vec![Step::act(KEYCHAIN_FIX)],
        ),
        Err(AppError::Incomplete { missing }) => fail(
            format!(
                "half of the Linear app is stored: nothing under service `{SERVICE}`, account \
                 `{missing}`"
            ),
            store_both(),
        ),
        Err(AppError::Tracker(error)) => fail(
            format!("the Linear app stored {place} cannot be used: {error}"),
            match error.kind {
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
        let report = run(&system, &no_config());
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

    /// Each check sits in its section, and the sections come in order.
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
            (Section::Tools, "codex"),
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
            let report = run(&system, &no_config());
            let codex = line(&report, "codex");
            assert_eq!(codex.status, Status::Warn, "{report}");
            assert_eq!(codex.fix, []);
            let why = codex.why.as_deref().unwrap();
            assert!(why.starts_with(DOES_NOT_BLOCK), "{why}");
            assert!(why.contains("from P5"), "{why}");
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
                   If a run misbehaves, install a tested version: claude 2.1.283, 2.1.284.";
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
    fn a_tested_version_is_quiet_and_codex_has_none_yet() {
        let system = logged_in(with_harnesses(with_git(FakeSystem::default())));
        let report = run(&system, &no_config());
        let claude = line(&report, "claude");
        assert_eq!(claude.status, Status::Ok, "{report}");
        assert!(!claude.detail.contains("tested"), "{report}");
        assert_eq!(claude.why, None);

        // Codex has no contract tests, so no version of it is tested.
        let codex = line(&report, "codex");
        assert!(report.ready(), "{report}");
        assert_eq!(codex.status, Status::Warn);
        assert_eq!(
            codex.detail,
            "0.154.0 (/fake/bin/codex), logged in (API key); no version tested with Owlshift yet"
        );
        assert_eq!(
            codex.why.as_deref(),
            Some(
                "Does not block `owlshift do`: it does not use Codex yet; the review roles \
                 will, from P5."
            )
        );
        assert_eq!(codex.fix, []);
    }

    /// A Codex that is not ready at an untested version (every Codex version
    /// is untested) is still the warning of OWL-102, and its detail says the
    /// version is untested; an informational line becomes a warning.
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
            .answer(CODEX_STATUS, Answer::Exit(1, "", "Not logged in\n"));
        let report = run(&system, &no_config());
        let codex = line(&report, "codex");
        assert_eq!(codex.status, Status::Warn);
        assert_eq!(
            codex.detail,
            "0.154.0 (/fake/bin/codex), not logged in; no version tested with Owlshift yet"
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
                 misbehaves, install a tested version: claude 2.1.283, 2.1.284."
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
            .answer(CLAUDE_STATUS, Answer::Exit(0, CLAUDE_LOGGED_IN, ""));
        let report = run(&system, &config);

        assert!(report.ready(), "{report}");
        assert!(report.checks.iter().all(|check| check.subject != "codex"));
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
    fn the_tracker_line_names_what_the_adapter_does_and_does_not_do_yet() {
        let (_dir, config) = linear_project();
        let tracker = tracker_check(&config);
        assert_well_formed(&tracker);
        assert_eq!(tracker.status, Status::Warn);
        assert_eq!(
            tracker.detail,
            "`linear`: read a ticket, read and post comments, visible stage; \
             not built yet: list admitted tickets"
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
        let failures: [(FakeSystem, &[&str]); 4] = [
            (ready().unstored(LINEAR_ACCOUNT), &["owlshift init"]),
            (ready().keychain_fails("locked"), &[]),
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
                    .unstored(LINEAR_APP_SECRET_ACCOUNT),
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
}
