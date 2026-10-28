//! `owlshift pause [OWNER/REPO]` and `owlshift resume [OWNER/REPO]`
//! (OWL-438, architecture section 5, "Pause"): write and remove the pause
//! of every project of the machine, `<data>/service/pause`, or of one
//! project, `<project data>/pause`. Every watch reads them at each pass and
//! before each step: while one stands it dispatches nothing and starts no
//! step, nor a merge on green, and the steps under way finish. The writing
//! is `owlshift_runner::daemon::requests`.

use std::path::Path;
use std::process::ExitCode;

use jiff::Timestamp;
use jiff::tz::TimeZone;
use owlshift_adapters::forge::Repo;
use owlshift_contracts::service::{BY_DAEMON, BY_PAUSE, Request};
use owlshift_runner::daemon::requests;
use owlshift_runner::events::printable;
use owlshift_runner::project::ProjectDirs;
use owlshift_runner::served::Registry;

use crate::{data_dir, fail};

/// Who asked `request`, when and why, its time local.
fn asked(request: Option<&Request>) -> String {
    request.map_or_else(
        || "a pause file that does not say who wrote it".to_owned(),
        |request| {
            format!(
                "by {} at {}: {}",
                request.by,
                request
                    .at
                    .to_zoned(TimeZone::system())
                    .strftime("%Y-%m-%d %H:%M"),
                request.why
            )
        },
    )
}

/// The project `repo` names, and its folder in `data`.
fn project(data: &Path, repo: &str) -> Result<(Repo, ProjectDirs), String> {
    let repo = Repo::parse(repo).map_err(|error| error.to_string())?;
    let dirs = ProjectDirs::new(data, &repo);
    Ok((repo, dirs))
}

/// `owlshift pause [OWNER/REPO] [--reason TEXT]`.
pub fn pause(repo: Option<&str>, reason: Option<&str>) -> ExitCode {
    let data = match data_dir() {
        Ok(data) => data,
        Err(error) => return fail(&error),
    };
    let why = reason.unwrap_or("asked by `owlshift pause`");
    let request = Request::new(BY_PAUSE, Timestamp::now(), why);
    let target = match repo.map(|repo| project(&data, repo)).transpose() {
        Ok(target) => target,
        Err(error) => return fail(&error),
    };
    let replaced = match requests::ask_pause(&data, target.as_ref().map(|(_, dirs)| dirs), &request)
    {
        Ok(replaced) => replaced,
        Err(error) => return fail(&format!("the pause could not be written: {error}")),
    };
    let what = match &target {
        Some((repo, _)) => format!("{repo} is paused"),
        None => "Every project of this machine is paused".to_owned(),
    };
    println!(
        "{}",
        printable(&format!(
            "{what}: from each watch's next pass, no ticket is dispatched and no step starts, \
             nor a merge on green; the steps under way finish, and answers, pull requests and \
             stages are still read. `owlshift resume{}` lifts it.",
            target
                .as_ref()
                .map_or_else(String::new, |(repo, _)| format!(" {repo}"))
        ))
    );
    if let Some(before) = replaced {
        println!(
            "{}",
            printable(&format!(
                "It replaces the pause {}.",
                asked(before.as_ref())
            ))
        );
    }
    if let Some((repo, _)) = &target
        && matches!(Registry::in_data_dir(&data).find(repo), Ok(None))
    {
        println!(
            "{}",
            printable(&format!(
                "{repo} is not served by the background service; a watch typed in a terminal on \
                 it is paused too."
            ))
        );
    }
    ExitCode::SUCCESS
}

/// `owlshift resume [OWNER/REPO]`.
pub fn resume(repo: Option<&str>) -> ExitCode {
    let data = match data_dir() {
        Ok(data) => data,
        Err(error) => return fail(&error),
    };
    let target = match repo.map(|repo| project(&data, repo)).transpose() {
        Ok(target) => target,
        Err(error) => return fail(&error),
    };
    let lifted = match requests::lift_pause(&data, target.as_ref().map(|(_, dirs)| dirs)) {
        Ok(lifted) => lifted,
        Err(error) => return fail(&format!("the pause could not be removed: {error}")),
    };
    let mut said = Vec::new();
    match (&target, &lifted) {
        (Some((repo, _)), Some(before)) => {
            said.push(format!(
                "{repo}'s pause is lifted (it was {}): its watch goes on at its next pass.",
                asked(before.as_ref())
            ));
            if before
                .as_ref()
                .is_some_and(|request| request.by == BY_DAEMON)
            {
                said.push(
                    "That pause was the daemon's own: it pauses the project again while its \
                     watch still cannot start."
                        .to_owned(),
                );
            }
        }
        (Some((repo, _)), None) => said.push(format!("{repo} was not paused.")),
        (None, Some(before)) => said.push(format!(
            "The pause of every project is lifted (it was {}): each watch goes on at its next \
             pass.",
            asked(before.as_ref())
        )),
        (None, None) => said.push("Every project of this machine was not paused.".to_owned()),
    }
    match &target {
        Some(_) => {
            if requests::machine_pause(&data).is_some() {
                said.push(
                    "Every project of this machine is still paused: `owlshift resume` lifts that."
                        .to_owned(),
                );
            }
        }
        None => {
            let still: Vec<String> = Registry::in_data_dir(&data)
                .read()
                .map(|served| served.projects)
                .unwrap_or_default()
                .into_iter()
                .filter(|entry| {
                    project(&data, &entry.repository)
                        .is_ok_and(|(_, dirs)| requests::pause(&dirs).is_some())
                })
                .map(|entry| entry.repository)
                .collect();
            if !still.is_empty() {
                said.push(format!(
                    "Still paused on their own: {} (`owlshift resume <owner/repo>`).",
                    still.join(", ")
                ));
            }
        }
    }
    for line in said {
        println!("{}", printable(&line));
    }
    ExitCode::SUCCESS
}
