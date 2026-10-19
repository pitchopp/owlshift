//! `owlshift gate [--at COMMIT]`: the project's gate, run once as a Build's
//! gate runs it (OWL-329), through `owlshift_runner::gate_once`. Its agent
//! environment is built as `do` builds it; it opens no keychain, tracker or
//! forge.

use std::io::{self, Write};
use std::process::ExitCode;

use owlshift_runner::agent_env::AgentEnv;
use owlshift_runner::config::Effective;
use owlshift_runner::events::printable;
use owlshift_runner::gate_once::{GateOnce, render};
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
    let gate = GateOnce {
        executor: &executor,
        dirs: &dirs,
        remote_url: &remote_url,
        local: project,
        at,
    };
    let mut starting = |plan: &_| {
        let mut stdout = io::stdout();
        let _ = write!(stdout, "{}", printable(&format!("{plan}")));
        let _ = stdout.flush();
    };
    let checked = gate.run(&mut starting).map_err(|stop| stop.to_string())?;
    print!("{}", printable(&render(&checked)));
    Ok(if checked.passed() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}
