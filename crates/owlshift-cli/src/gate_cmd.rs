//! `owlshift gate [--at COMMIT]`: the project's gate, run once as a Build's
//! gate runs it (OWL-329), through `owlshift_runner::gate_once`. Its agent
//! environment is built as `do` builds it; it opens no keychain, tracker or
//! forge. When the gate fails on a command whose output names folders of the
//! home the sandbox refused, it says so, gives the lines that open them, and
//! at a terminal offers to write them (OWL-597, `owlshift_runner::cache_offer`).

use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use owlshift_adapters::forge::Repo;
use owlshift_contracts::config::ProjectConfig;
use owlshift_runner::agent_env::AgentEnv;
use owlshift_runner::cache_offer::{self, State};
use owlshift_runner::config::{Effective, FileState};
use owlshift_runner::events::printable;
use owlshift_runner::executor::Executor;
use owlshift_runner::gate_once::{Checked, GateOnce, render};
use owlshift_runner::on_demand;
use owlshift_runner::project::{self, ProjectDirs};
use owlshift_runner::system::System;

use crate::{do_cmd, fail};

pub fn run(system: &dyn System, config: &Effective, at: Option<&str>) -> ExitCode {
    run_checked(system, config, at).unwrap_or_else(|refusal| fail(&refusal))
}

fn run_checked(
    system: &dyn System,
    config: &Effective,
    at: Option<&str>,
) -> Result<ExitCode, String> {
    let (project_file, project) = do_cmd::loaded_project(config, "`owlshift gate`")?;
    let root = project_file.parent().unwrap_or(project_file);
    let git = project::runner_git();
    let remote_url = project::origin_url(&git, root)?;
    let repo = on_demand::check_origin(&remote_url)?;
    // Before anything is cloned, as for `do` (OWL-98).
    system.sandbox().map_err(|error| error.to_string())?;
    let data_dir = crate::data_dir()?;
    // As `do` builds it: the names declared, as far as the operator allows
    // them for this repository.
    let allowed = config.allowed_gate_env(Some(&repo)).unwrap_or_default();
    // The caches declared, as far as the operator allows them for this
    // repository: the load refused any other (OWL-330).
    let allowed_caches = config.allowed_caches(Some(&repo)).unwrap_or_default();
    let caches: Vec<&str> = project
        .stack
        .cache_folders()
        .into_iter()
        .filter(|cache| allowed_caches.contains(cache))
        .collect();
    let agent = AgentEnv::from_runner(&project.stack.gate_env_names(), &allowed, &caches)
        .map_err(|error| format!("the agent environment: {error}"))?;
    let executor = on_demand::executor(git, agent);
    let dirs = ProjectDirs::new(&data_dir, &repo);
    let gate = GateOnce::operator(&executor, &dirs, &remote_url, project, at);
    let mut starting = |plan: &_| {
        let mut stdout = io::stdout();
        let _ = write!(stdout, "{}", printable(&format!("{plan}")));
        let _ = stdout.flush();
    };
    let checked = gate.run(&mut starting).map_err(|stop| stop.to_string())?;
    print!("{}", printable(&render(&checked)));
    if let Some(output) = failed_output(&checked) {
        let gated = Gated {
            executor: &executor,
            config,
            project_file,
            project,
            repo: &repo,
            root,
        };
        gated.offer(output);
    }
    Ok(if checked.passed() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

/// The output of the command the gate failed on, when it failed on one and
/// broke no isolation.
fn failed_output(checked: &Checked) -> Option<&str> {
    let run = checked.run.as_ref()?;
    if !run.violations.is_empty() {
        return None;
    }
    let failure = run.report.failure.as_ref()?;
    failure.command.as_ref()?;
    Some(&failure.output)
}

/// What the offer of OWL-597 reads.
struct Gated<'a> {
    executor: &'a Executor,
    config: &'a Effective,
    project_file: &'a Path,
    project: &'a ProjectConfig,
    repo: &'a Repo,
    /// The person's checkout, never named.
    root: &'a Path,
}

impl Gated<'_> {
    /// Says which folders of the home the sandbox refused the failing
    /// command, from its `output`, and the lines that open the closed ones;
    /// at a terminal, asks whether to write them and writes them on yes.
    fn offer(&self, output: &str) {
        let own: Vec<PathBuf> = cache_offer::own_folders()
            .into_iter()
            .chain([self.root.to_owned()])
            .collect();
        let Some(context) = cache_offer::Context::for_agent(
            &self.executor.agent,
            &own,
            &cache_offer::host_is_folder,
        ) else {
            return;
        };
        let refused = cache_offer::refused(output, &context);
        if refused.is_empty() {
            return;
        }
        let closed: Vec<String> = refused
            .iter()
            .filter(|one| one.state == State::Closed)
            .map(|one| one.folder.clone())
            .collect();
        let allowed = self
            .config
            .allowed_caches(Some(self.repo))
            .unwrap_or_default();
        let lines = cache_offer::lines(
            &closed,
            &self.project.stack.caches,
            &self.config.scoped_caches(self.repo),
            &allowed,
            self.repo,
        );
        let personal = match &self.config.personal {
            FileState::Loaded { path, .. }
            | FileState::Absent(path)
            | FileState::Invalid { path, .. } => Some(path.as_path()),
            FileState::NotApplicable(_) | FileState::Unavailable(_) => None,
        };
        print!(
            "{}",
            printable(&cache_offer::gate_note(
                &refused,
                &lines,
                self.project_file,
                personal
            ))
        );
        let Some(personal) = personal.filter(|_| !lines.is_empty()) else {
            return;
        };
        match crate::confirm("Add these lines now? [y/N] ") {
            None => println!("At a terminal, `owlshift gate` offers to add them."),
            Some(false) => {}
            Some(true) => {
                match cache_offer::open(self.project_file, personal, self.repo, &closed) {
                    Ok(written) => {
                        if written.personal {
                            println!(
                                "Added to {}: the repository's allow_caches.",
                                personal.display()
                            );
                        }
                        if written.project {
                            println!(
                                "Added to {}: stack.caches. Commit it, so that the background \
                                 service's runs open the folder too.",
                                self.project_file.display()
                            );
                        }
                        println!("Run `owlshift gate` again.");
                    }
                    Err(error) => eprintln!("owlshift: nothing written: {}", printable(&error)),
                }
            }
        }
    }
}
