//! `owlshift do TICKET`, `owlshift continue TICKET` and `owlshift watch`:
//! open what a run needs, in this order, and hand it to
//! `owlshift_runner::on_demand`, or to `owlshift_runner::watch`, which
//! continues the tickets whose questions wait until Ctrl-C. What any of them
//! can refuse without a credential, a host where agent runs cannot be
//! confined included, is refused before the keychain is opened.

use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::thread;
use std::time::Duration;

use owlshift_adapters::notifier::Notifier;
use owlshift_adapters::tracker::Tracker;
use owlshift_adapters::tracker::markdown::MarkdownTracker;
use owlshift_contracts::Role;
use owlshift_contracts::config::{ProjectConfig, TrackerKind};
use owlshift_contracts::format::strip_role_front_matter;
use owlshift_contracts::ids::TicketId;
use owlshift_platform::keychain::{Keychain, KeychainError};
use owlshift_runner::agent_env::AgentEnv;
use owlshift_runner::config::{Effective, FileState};
use owlshift_runner::events::{EventLog, EventSink, printable};
use owlshift_runner::executor::harness::{CLAUDE_AGENT_ACCOUNT, ClaudeHarness};
use owlshift_runner::notify::{self, DesktopNotifier};
use owlshift_runner::on_demand::{self, Delivered, OnDemand, Stop};
use owlshift_runner::project::{self, ProjectDirs};
use owlshift_runner::roles::{ANSWER_CHECK_ROLE, BUILD_ROLE, RESOLVER_ROLE};
use owlshift_runner::system::System;
use owlshift_runner::watch::{POLL_INTERVAL, Watch};
use owlshift_runner::{forge, tracker};

use crate::fail;

/// How long to wait between two reads of a pull request's head after a
/// push.
const HEAD_WAIT: Duration = Duration::from_secs(2);

/// Which command runs: `owlshift do` from Ready, `owlshift continue` from
/// where the ticket's ref left it, each on its ticket, or `owlshift watch`
/// over the project's tickets whose questions wait.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode<'a> {
    Do(&'a str),
    Continue(&'a str),
    Watch,
}

impl<'a> Mode<'a> {
    fn command(self) -> &'static str {
        match self {
            Mode::Do(_) => "`owlshift do`",
            Mode::Continue(_) => "`owlshift continue`",
            Mode::Watch => "`owlshift watch`",
        }
    }

    /// The ticket a command names; `watch` names none.
    fn ticket(self) -> Option<&'a str> {
        match self {
            Mode::Do(ticket) | Mode::Continue(ticket) => Some(ticket),
            Mode::Watch => None,
        }
    }
}

pub fn run(system: &dyn System, config: &Effective, mode: Mode<'_>) -> ExitCode {
    run_with(system, config, mode, Keychain::system).unwrap_or_else(|refusal| fail(&refusal))
}

/// [`run`], with the keychain opened by `open_keychain`: a refusal before
/// the run is returned, for the caller to print.
fn run_with(
    system: &dyn System,
    config: &Effective,
    mode: Mode<'_>,
    open_keychain: fn() -> Result<Keychain, KeychainError>,
) -> Result<ExitCode, String> {
    let ticket = mode
        .ticket()
        .map(TicketId::new)
        .transpose()
        .map_err(|error| error.to_string())?;
    let (project_file, project) = loaded_project(config, mode.command())?;
    if let Some(ticket) = &ticket {
        on_demand::check_team(project, ticket)?;
    }
    let root = project_file.parent().unwrap_or(project_file);
    let git = project::runner_git();
    let remote_url = project::origin_url(&git, root)?;
    let repo = on_demand::check_origin(&remote_url)?;
    // OWL-98: a host where agent runs cannot be confined is refused before
    // the keychain is opened, so it never prompts for or loads a secret for
    // a run that cannot happen. `do`, `continue` and `watch` always confine
    // their agents (`AgentEnv::from_runner`); `OnDemand` checks again.
    system.sandbox().map_err(|error| error.to_string())?;
    let data_dir = crate::data_dir()?;
    let Some(claude) = system.locate("claude") else {
        return Err(
            "`claude` is not on the PATH: install Claude Code (agent runs log in with the token `owlshift init` stores; `owlshift doctor` checks it)"
                .to_owned(),
        );
    };
    let prompt = strip_role_front_matter(Role::Build, BUILD_ROLE)
        .map_err(|error| format!("the built-in build role: {error}"))?;
    let check_prompt = strip_role_front_matter(Role::AnswerCheck, ANSWER_CHECK_ROLE)
        .map_err(|error| format!("the built-in answer-check role: {error}"))?;
    let resolver_prompt = strip_role_front_matter(Role::Resolver, RESOLVER_ROLE)
        .map_err(|error| format!("the built-in resolver role: {error}"))?;
    // The personal file is valid here; were it not, no name would be allowed.
    // The names are those allowed for the repository this run delivers to.
    let allowed = config.allowed_gate_env(Some(&repo)).unwrap_or_default();
    let agent = AgentEnv::from_runner(&project.stack.gate_env_names(), &allowed)
        .map_err(|error| format!("the agent environment: {error}"))?;

    // From here on, the credentials: the runner's own, never an agent's.
    let keychain = open_keychain().map_err(|error| error.to_string())?;
    // With the Linear app stored, its token is requested here, before any
    // work, and revoked when the tracker is dropped (OWL-157).
    let tracker: Box<dyn Tracker> = match project.tracker.kind {
        TrackerKind::Linear => {
            let team = project
                .tracker
                .team
                .as_deref()
                .ok_or("a Linear project names its team in `tracker.team`")?;
            Box::new(tracker::linear(&keychain, team)?)
        }
        TrackerKind::Markdown => Box::new(MarkdownTracker::new(root)),
    };
    let forge = forge::github(&keychain, repo.clone())?;
    // The token agent runs log in with (OWL-94), read with the other
    // secrets and set on the harness command alone. None stored refuses the
    // run, with the fix, before anything is cloned.
    let login = keychain
        .read(CLAUDE_AGENT_ACCOUNT)
        .map_err(|error| error.to_string())?;
    let budget = match &config.personal {
        FileState::Loaded { config, .. } => config
            .harnesses
            .claude
            .as_ref()
            .and_then(|claude| claude.budget_usd),
        _ => None,
    };
    let harness = ClaudeHarness {
        program: claude,
        prompt,
        model: project
            .models
            .standard
            .as_ref()
            .and_then(|tier| tier.claude.clone()),
        effort: None,
        max_budget_usd: budget,
        login,
    };
    // The answer check runs on the same harness, model and login, with its
    // own prompt.
    let check = ClaudeHarness {
        prompt: check_prompt,
        ..harness.clone()
    };
    let resolver = ClaudeHarness {
        prompt: resolver_prompt,
        ..harness.clone()
    };
    let executor = on_demand::executor(git, agent);
    let dirs = ProjectDirs::new(&data_dir, &repo);
    let on_demand = OnDemand {
        executor: &executor,
        tracker: tracker.as_ref(),
        forge: &forge,
        build: &harness,
        answer_check: &check,
        resolver: &resolver,
        remote_url: &remote_url,
        config: project,
        dirs: &dirs,
        head_wait: HEAD_WAIT,
        clock: &on_demand::system_clock,
    };
    let mut stdout = io::stdout();
    let mut sink = EventSink::new(repo.to_string(), EventLog::in_dir(&data_dir), &mut stdout);
    let desktop = notify::desktop_enabled(&config.personal)
        .then(|| DesktopNotifier::for_this_machine(system))
        .flatten();
    let desktop = desktop.as_ref().map(|d| d as &dyn Notifier);
    let (ticket, outcome) = match (mode, &ticket) {
        (Mode::Do(_), Some(ticket)) => (ticket, on_demand.run(ticket, &mut sink)),
        (Mode::Continue(_), Some(ticket)) => (ticket, on_demand.continue_ticket(ticket, &mut sink)),
        _ => {
            // OWL-152: until Ctrl-C ends the process; each continue's outcome
            // is reported as `continue`'s.
            let _ = writeln!(
                io::stdout(),
                "Watching {repo}: each ticket whose questions wait is continued once its \
                 decider's reply counts, checked every {} seconds. Ctrl-C stops.",
                POLL_INTERVAL.as_secs()
            );
            let watch = Watch {
                on_demand: &on_demand,
                interval: POLL_INTERVAL,
            };
            watch.run(
                &mut sink,
                &mut io::stdout(),
                &mut |ticket, outcome, sink| {
                    report(system, desktop, tracker.as_ref(), ticket, outcome, sink);
                },
                &mut |interval| {
                    thread::sleep(interval);
                    true
                },
            );
            return Ok(ExitCode::SUCCESS);
        }
    };
    Ok(report(
        system,
        desktop,
        tracker.as_ref(),
        ticket,
        &outcome,
        &mut sink,
    ))
}

/// The project file `command` works with, and its path, from a valid
/// configuration: why it cannot otherwise.
pub(crate) fn loaded_project<'c>(
    config: &'c Effective,
    command: &str,
) -> Result<(&'c PathBuf, &'c ProjectConfig), String> {
    let loaded = match &config.project {
        FileState::Loaded { path, config, .. } => (path, config),
        FileState::Absent(path) => {
            return Err(format!(
                "no project file at {}: run `owlshift init` first",
                path.display()
            ));
        }
        FileState::NotApplicable(reason) => {
            return Err(format!("{command} runs in a git repository: {reason}"));
        }
        FileState::Unavailable(reason) => return Err(reason.clone()),
        FileState::Invalid { path, error } => {
            return Err(format!("{} is invalid: {error}", path.display()));
        }
    };
    if !config.is_valid() {
        return Err("the personal configuration is invalid: see `owlshift config show`".to_owned());
    }
    Ok(loaded)
}

/// The end of one run of a ticket, `do`'s, `continue`'s, or each continue
/// `watch` runs: its outcome printed ([`finish`]), then (OWL-140) a desktop
/// notification when it makes the operator the blocker, a failure of which
/// is a warning event.
fn report(
    system: &dyn System,
    desktop: Option<&dyn Notifier>,
    tracker: &dyn Tracker,
    ticket: &TicketId,
    outcome: &Result<Delivered, Stop>,
    sink: &mut EventSink<'_>,
) -> ExitCode {
    let code = finish(system, outcome, &mut io::stdout(), &mut io::stderr());
    notify::notify_blocker(desktop, tracker, ticket, outcome, sink);
    code
}

/// The end of a run: the warning of a sentinel that ended during it, or is
/// stopped at its end, then its outcome.
#[cfg_attr(not(unix), allow(unused_variables))]
fn finish(
    system: &dyn System,
    outcome: &Result<Delivered, Stop>,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> ExitCode {
    // OWL-88: a sentinel that ended during the run left it unprotected, and
    // is never restarted; one that never started was warned of in `main`.
    // OWL-91, OWL-95: one stopped at the end still stops the live trees once
    // continued at Owlshift's end, with the exceptions the warning names;
    // one the guard killed during the run is reported as ended.
    #[cfg(unix)]
    {
        use owlshift_runner::system::SentinelStatus;
        match system.sentinel() {
            SentinelStatus::Running { .. } | SentinelStatus::NotRunning => {}
            SentinelStatus::Stopped { .. } => {
                let _ = writeln!(
                    stderr,
                    "owlshift: warning: its sentinel is stopped at the end of this run: the \
                     system continues it as Owlshift ends, and it then stops, best effort, the \
                     processes Owlshift started, unless it was stopped in its first milliseconds \
                     (before it ignores SIGHUP) or, on Linux, Owlshift's end leaves it to a \
                     subreaper in the same session"
                );
            }
            SentinelStatus::Ended(how) => {
                let _ = writeln!(
                    stderr,
                    "owlshift: warning: its sentinel ended during this run ({}): a hard kill of \
                     Owlshift would have left the processes it started running",
                    printable(&how)
                );
            }
        }
    }
    match outcome {
        Ok(delivered) => {
            let _ = writeln!(stdout, "\n{}", printable(&delivered.to_string()));
            ExitCode::SUCCESS
        }
        Err(stop) => {
            let _ = writeln!(stdout, "\n{}", printable(&stop.to_string()));
            ExitCode::FAILURE
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::path::{Path, PathBuf};

    use owlshift_runner::system::{Captured, RunError, SentinelStatus};

    use super::*;

    /// A host whose sentinel is as said; a run's end asks it nothing else.
    struct Sentinel(SentinelStatus);

    impl System for Sentinel {
        fn locate(&self, _program: &str) -> Option<PathBuf> {
            unreachable!("the end of a run locates no program")
        }

        fn run(&self, _: &Path, _: &[&str], _: Option<&Path>) -> Result<Captured, RunError> {
            unreachable!("the end of a run runs no program")
        }

        fn sentinel(&self) -> SentinelStatus {
            self.0.clone()
        }
    }

    /// What the end of a refused run writes, on stdout and on stderr.
    fn finish_refused(sentinel: SentinelStatus) -> (ExitCode, String, String) {
        let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
        let code = finish(
            &Sentinel(sentinel),
            &Err(Stop::Refused("no ticket".to_owned())),
            &mut stdout,
            &mut stderr,
        );
        let text = |bytes| String::from_utf8(bytes).unwrap();
        (code, text(stdout), text(stderr))
    }

    #[test]
    fn a_sentinel_that_ended_during_the_run_is_warned_of() {
        let (code, stdout, stderr) =
            finish_refused(SentinelStatus::Ended("signal: 9 (SIGKILL)".to_owned()));
        assert_eq!(
            stderr,
            "owlshift: warning: its sentinel ended during this run (signal: 9 (SIGKILL)): a hard \
             kill of Owlshift would have left the processes it started running\n"
        );
        // The outcome still follows.
        assert_eq!(stdout, "\nNot run: no ticket\n");
        assert_eq!(code, ExitCode::FAILURE);

        let (_, _, stderr) = finish_refused(SentinelStatus::Ended("\u{1b}[2J".to_owned()));
        assert!(stderr.contains("(\\u{1b}[2J)"), "{stderr}");
    }

    #[test]
    fn a_sentinel_stopped_at_the_end_of_the_run_is_warned_of() {
        let (code, stdout, stderr) = finish_refused(SentinelStatus::Stopped { pid: 4242 });
        assert_eq!(
            stderr,
            "owlshift: warning: its sentinel is stopped at the end of this run: the system \
             continues it as Owlshift ends, and it then stops, best effort, the processes \
             Owlshift started, unless it was stopped in its first milliseconds (before it \
             ignores SIGHUP) or, on Linux, Owlshift's end leaves it to a subreaper in the same \
             session\n"
        );
        assert_eq!(stdout, "\nNot run: no ticket\n");
        assert_eq!(code, ExitCode::FAILURE);
    }

    #[test]
    fn a_sentinel_that_runs_or_never_started_is_not_warned_of() {
        for sentinel in [
            SentinelStatus::Running { pid: 4242 },
            // Warned of in `main`, when it failed to start.
            SentinelStatus::NotRunning,
        ] {
            let (code, stdout, stderr) = finish_refused(sentinel);
            assert_eq!(stderr, "");
            assert_eq!(stdout, "\nNot run: no ticket\n");
            assert_eq!(code, ExitCode::FAILURE);
        }
    }
}

#[cfg(test)]
mod refusals {
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use owlshift_contracts::config::ProjectConfig;
    use owlshift_platform::sandbox::SandboxError;
    use owlshift_runner::system::{Captured, RunError};

    use super::*;

    /// A host where agent runs cannot be confined, asked nothing else.
    struct NoSandbox;

    impl System for NoSandbox {
        fn locate(&self, _program: &str) -> Option<PathBuf> {
            unreachable!("a host that cannot confine is refused before `claude` is looked for")
        }

        fn run(&self, _: &Path, _: &[&str], _: Option<&Path>) -> Result<Captured, RunError> {
            unreachable!("a host that cannot confine is refused before any program runs")
        }

        fn sandbox(&self) -> Result<(), SandboxError> {
            Err(SandboxError::Unsupported)
        }

        fn secret_stored(&self, _account: &str) -> Result<bool, String> {
            unreachable!("a host that cannot confine is refused before any secret is looked at")
        }
    }

    fn no_keychain() -> Result<Keychain, KeychainError> {
        panic!("the keychain was opened on a host that cannot confine agent runs")
    }

    #[test]
    fn a_host_that_cannot_confine_is_refused_before_the_keychain_is_opened() {
        // A project `do` would otherwise go on with: a Linear project of
        // team OWL, in a repository whose origin is on github.com.
        let repo = tempfile::tempdir().unwrap();
        for args in [
            &["init", "--quiet"][..],
            &[
                "remote",
                "add",
                "origin",
                "https://github.com/owlshift/demo.git",
            ],
        ] {
            let status = Command::new("git")
                .args(args)
                .current_dir(repo.path())
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?}");
        }
        let project = ProjectConfig::parse(
            "requires = \">=0.0\"\n[tracker]\nkind = \"linear\"\nteam = \"OWL\"\n\
             admit = \"delegation\"\n\
             states = { ready = \"a\", working = \"b\", needs_input = \"c\", review = \"d\" }\n\
             [stack]\ngate = []\n[pipeline]\ndefault = \"trivial\"\nplan_approval = \"never\"\n\
             [models]\n[policy]\nalways_human = []\n",
        )
        .unwrap();
        let config = Effective {
            project: FileState::Loaded {
                path: repo.path().join("owlshift.toml"),
                config: project,
                entries: Vec::new(),
            },
            personal: FileState::Absent(repo.path().join("config.toml")),
        };

        for mode in [Mode::Do("OWL-1"), Mode::Continue("OWL-1"), Mode::Watch] {
            let refusal = run_with(&NoSandbox, &config, mode, no_keychain).unwrap_err();
            assert_eq!(refusal, SandboxError::Unsupported.to_string(), "{mode:?}");
        }
    }
}
