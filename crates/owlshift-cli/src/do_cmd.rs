//! `owlshift do TICKET`: opens what one run needs, in this order, and hands
//! it to `owlshift_runner::on_demand`. Everything that can be refused
//! without a credential is refused before the keychain is opened.

use std::io::{self, Write};
use std::process::ExitCode;
use std::time::Duration;

use owlshift_adapters::tracker::Tracker;
use owlshift_adapters::tracker::markdown::MarkdownTracker;
use owlshift_contracts::Role;
use owlshift_contracts::config::TrackerKind;
use owlshift_contracts::format::strip_role_front_matter;
use owlshift_contracts::ids::TicketId;
use owlshift_platform::keychain::Keychain;
use owlshift_runner::agent_env::AgentEnv;
use owlshift_runner::config::{Effective, FileState};
use owlshift_runner::events::{EventLog, EventSink, printable};
use owlshift_runner::executor::harness::ClaudeHarness;
use owlshift_runner::on_demand::{self, Delivered, OnDemand, Stop};
use owlshift_runner::project::{self, ProjectDirs};
use owlshift_runner::roles::BUILD_ROLE;
use owlshift_runner::system::System;
use owlshift_runner::{forge, tracker};

use crate::fail;

/// How long to wait between two reads of a pull request's head after a
/// push.
const HEAD_WAIT: Duration = Duration::from_secs(2);

pub fn run(system: &dyn System, config: &Effective, ticket: &str) -> ExitCode {
    let ticket = match TicketId::new(ticket) {
        Ok(ticket) => ticket,
        Err(error) => return fail(&error.to_string()),
    };
    let (project_file, project) = match &config.project {
        FileState::Loaded { path, config, .. } => (path, config),
        FileState::Absent(path) => {
            return fail(&format!(
                "no project file at {}: run `owlshift init` first",
                path.display()
            ));
        }
        FileState::NotApplicable(reason) => {
            return fail(&format!("`owlshift do` runs in a git repository: {reason}"));
        }
        FileState::Unavailable(reason) => return fail(reason),
        FileState::Invalid { path, error } => {
            return fail(&format!("{} is invalid: {error}", path.display()));
        }
    };
    if !config.is_valid() {
        return fail("the personal configuration is invalid: see `owlshift config show`");
    }
    if let Err(error) = on_demand::check_team(project, &ticket) {
        return fail(&error);
    }
    let root = project_file.parent().unwrap_or(project_file);
    let git = project::runner_git();
    let remote_url = match project::origin_url(&git, root) {
        Ok(url) => url,
        Err(error) => return fail(&error),
    };
    let repo = match on_demand::check_origin(&remote_url) {
        Ok(repo) => repo,
        Err(error) => return fail(&error),
    };
    let Some(data_dir) = owlshift_platform::paths::data_dir() else {
        return fail(
            "this system has no data directory: set OWLSHIFT_DATA_DIR to an absolute path",
        );
    };
    let Some(claude) = system.locate("claude") else {
        return fail(
            "`claude` is not on the PATH: install Claude Code and log in (`owlshift doctor` checks it)",
        );
    };
    let prompt = match strip_role_front_matter(Role::Build, BUILD_ROLE) {
        Ok(prompt) => prompt,
        Err(error) => return fail(&format!("the built-in build role: {error}")),
    };
    // The personal file is valid here; were it not, no name would be allowed.
    // The names are those allowed for the repository this run delivers to.
    let allowed = config.allowed_gate_env(Some(&repo)).unwrap_or_default();
    let agent = match AgentEnv::from_runner(&project.stack.gate_env_names(), &allowed) {
        Ok(agent) => agent,
        Err(error) => return fail(&format!("the agent environment: {error}")),
    };

    // From here on, the credentials: the runner's own, never an agent's.
    let keychain = match Keychain::system() {
        Ok(keychain) => keychain,
        Err(error) => return fail(&error.to_string()),
    };
    let tracker: Box<dyn Tracker> = match project.tracker.kind {
        TrackerKind::Linear => match tracker::linear(&keychain) {
            Ok(linear) => Box::new(linear),
            Err(error) => return fail(&error),
        },
        TrackerKind::Markdown => Box::new(MarkdownTracker::new(root)),
    };
    let forge = match forge::github(&keychain, repo.clone()) {
        Ok(forge) => forge,
        Err(error) => return fail(&error),
    };
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
    };
    let executor = on_demand::executor(git, agent);
    let dirs = ProjectDirs::new(&data_dir, &repo);
    let on_demand = OnDemand {
        executor: &executor,
        tracker: tracker.as_ref(),
        forge: &forge,
        harness: &harness,
        remote_url: &remote_url,
        config: project,
        dirs: &dirs,
        head_wait: HEAD_WAIT,
    };
    let mut stdout = io::stdout();
    let mut sink = EventSink::new(repo.to_string(), EventLog::in_dir(&data_dir), &mut stdout);
    let outcome = on_demand.run(&ticket, &mut sink);
    finish(system, outcome, &mut io::stdout(), &mut io::stderr())
}

/// The end of a run: the warning of a sentinel that ended during it, or is
/// stopped at its end, then its outcome.
#[cfg_attr(not(unix), allow(unused_variables))]
fn finish(
    system: &dyn System,
    outcome: Result<Delivered, Stop>,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
) -> ExitCode {
    // OWL-88: a sentinel that ended during the run left it unprotected, and
    // is never restarted; one that never started was warned of in `main`.
    // OWL-91: one stopped protects nothing while it is.
    #[cfg(unix)]
    {
        use owlshift_runner::system::SentinelStatus;
        match system.sentinel() {
            SentinelStatus::Running { .. } | SentinelStatus::NotRunning => {}
            SentinelStatus::Stopped { .. } => {
                let _ = writeln!(
                    stderr,
                    "owlshift: warning: its sentinel is stopped at the end of this run: a hard \
                     kill of Owlshift while it was stopped would have left the processes it \
                     started running"
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
            Err(Stop::Refused("no ticket".to_owned())),
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
            "owlshift: warning: its sentinel is stopped at the end of this run: a hard kill of \
             Owlshift while it was stopped would have left the processes it started running\n"
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
