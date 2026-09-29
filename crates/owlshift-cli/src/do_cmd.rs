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
use owlshift_runner::on_demand::{self, OnDemand};
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
    let agent = match AgentEnv::from_runner(&project.stack.gate_env_names()) {
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
    let mut stdout = io::stdout();
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
