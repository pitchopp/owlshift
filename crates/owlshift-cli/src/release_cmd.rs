//! `owlshift release TICKET` (P10 part 8, OWL-588; build plan "P10 ·
//! Teams", "Releasing a claim"): opens what releasing a ticket's claim
//! needs, the project's dedicated checkout, its tracker through the
//! keychain, this machine's operator and the event log, and hands it to
//! `owlshift_runner::release`. With `--served`, the web UI's Release runs it
//! on a served project, on the project file the project is served on.

use std::io;
use std::path::Path;
use std::process::ExitCode;

use owlshift_adapters::forge::Repo;
use owlshift_contracts::ids::TicketId;
use owlshift_platform::keychain::{Keychain, KeychainError};
use owlshift_runner::config::{Effective, FileState};
use owlshift_runner::events::{EventLog, EventSink, printable};
use owlshift_runner::on_demand;
use owlshift_runner::operator;
use owlshift_runner::project::{self, ProjectDirs};
use owlshift_runner::release::{self, Operator, Release};
use owlshift_runner::served::{self, Served};

use crate::do_cmd::Secrets;
use crate::fail;

/// `owlshift release TICKET`, on the served project `served` when given,
/// released from the web UI when `from_ui`.
pub fn run(
    config: &Effective,
    ticket: &str,
    served: Option<&Repo>,
    from_ui: bool,
    open_keychain: fn() -> Result<Keychain, KeychainError>,
) -> ExitCode {
    match release(config, ticket, served, from_ui, open_keychain) {
        Ok(released) => {
            for line in &released.lines {
                println!("{}", printable(line));
            }
            if let Some(warning) = &released.warning {
                eprintln!("owlshift: warning: {}", printable(warning));
            }
            ExitCode::SUCCESS
        }
        Err(refusal) => fail(&refusal),
    }
}

fn release(
    config: &Effective,
    ticket: &str,
    served: Option<&Repo>,
    from_ui: bool,
    open_keychain: fn() -> Result<Keychain, KeychainError>,
) -> Result<release::Released, String> {
    let ticket = TicketId::new(ticket).map_err(|error| error.to_string())?;
    let data_dir = crate::data_dir()?;
    let git = project::runner_git();
    // The served project's own file, or the one of the working directory.
    let effective;
    let located;
    let (config, root, remote_url, repo): (&Effective, &Path, String, Repo) = match served {
        Some(repo) => {
            located = Served::locate(&git, &data_dir, repo)?;
            effective = served::effective(
                repo,
                &located.dirs,
                owlshift_platform::paths::personal_config_file(),
            )?;
            (
                &effective,
                located.checkout.as_path(),
                located.remote_url.clone(),
                located.repo.clone(),
            )
        }
        None => {
            let (project_file, _) = crate::do_cmd::loaded_project(config, "`owlshift release`")?;
            let root = project_file.parent().unwrap_or(project_file);
            let remote_url = project::origin_url(&git, root)?;
            let repo = on_demand::check_origin(&remote_url)?;
            (config, root, remote_url, repo)
        }
    };
    let (_, project) = crate::do_cmd::loaded_project(config, "`owlshift release`")?;
    on_demand::check_team(project, &ticket)?;
    let dirs = ProjectDirs::new(&data_dir, &repo);
    let secrets = Secrets::read(open_keychain, project, &repo, &dirs, &config.personal)
        .map_err(|refusal| refusal.message)?;
    let tracker = secrets
        .opener(project, root, &repo)
        .and_then(|opener| opener.open())
        .map_err(|refusal| refusal.message)?;
    let personal = match &config.personal {
        FileState::Loaded { config, .. } => Some(config),
        _ => None,
    };
    // Who releases: a release names them, and they decide what it may
    // release.
    let account = operator::known(tracker.as_ref(), project.tracker.kind, personal)
        .map_err(|why| format!("{why}: a release names this machine's operator"))?
        .ok_or_else(|| {
            "the Markdown tracker's key names no account, and a release names this machine's \
             operator: set `identity.tracker` in the personal file to your tracker account"
                .to_owned()
        })?;
    let operator = Operator::of(project, &account);
    let clock = on_demand::system_clock;
    let release = Release {
        git: &git,
        dirs: &dirs,
        remote_url: &remote_url,
        config: project,
        tracker: tracker.as_ref(),
        operator: &operator,
        clock: &clock,
        step_wait: release::STEP_WAIT,
        from_ui,
    };
    let mut stdout = io::stdout();
    let mut sink = EventSink::new(repo.to_string(), EventLog::in_dir(&data_dir), &mut stdout);
    release::release(&release, &ticket, &mut sink)
}
