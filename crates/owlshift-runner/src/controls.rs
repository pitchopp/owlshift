//! The operator's controls of the machine and its projects (OWL-438,
//! OWL-473, architecture section 5, "Pause" and "The local web UI",
//! "Actions"): pause and resume every project of the machine or one
//! project, and stop serving a project. `owlshift pause`, `owlshift resume`
//! and `owlshift stop --forget` call these, and so do the web UI's routes,
//! so a gesture has one path whoever makes it: the same files, under the
//! same locks, in the same words. Each returns the lines the command
//! prints, or why it could not act.
//!
//! The writing is [`crate::daemon::requests`] for a pause and
//! [`Registry::forget`] for the list of served projects. A pause and a
//! resume that lifts one also record a `decision` event (OWL-528), `pause` =
//! `asked` or `lifted`, so the log says why nothing was dispatched for a
//! while; the others write none.

use std::path::Path;

use jiff::tz::TimeZone;
use owlshift_adapters::forge::Repo;
use owlshift_contracts::event::EventKind;
use owlshift_contracts::service::{BY_DAEMON, BY_PAUSE, BY_UI, Request};
use serde_json::{Value, json};

use crate::daemon::{self, requests};
use crate::events::{Data, EventLog, EventSink};
use crate::project::ProjectDirs;
use crate::served::Registry;
use crate::voice::Voice;

/// The project of the events of a pause or a resume of the whole machine.
pub const MACHINE_PROJECT: &str = "machine";

/// A pause that stood, as an event's data holds it: `{by, at, why}`, or `{}`
/// when its file did not read.
fn standing(request: Option<&Request>) -> Value {
    request.map_or_else(
        || json!({}),
        |request| json!({ "by": request.by, "at": request.at, "why": request.why }),
    )
}

/// Appends a `decision` event of the pause of `target` holding `pairs`, and
/// returns the line that says so when it could not be written: the pause
/// already stands (or is lifted), so a failed append never fails the gesture.
fn record(
    data_dir: &Path,
    target: Option<&Repo>,
    pairs: Vec<(&str, Value)>,
    by: Option<&str>,
) -> Option<String> {
    let mut data: Data = pairs
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value))
        .collect();
    data.insert(
        "scope".to_owned(),
        json!(if target.is_some() {
            "project"
        } else {
            "machine"
        }),
    );
    if let Some(by) = by {
        data.insert("by".to_owned(), json!(by));
    }
    let mut out = Vec::new();
    let mut sink = EventSink::new(
        target.map_or_else(|| MACHINE_PROJECT.to_owned(), ToString::to_string),
        EventLog::in_dir(data_dir),
        &mut out,
    );
    sink.emit_project(None, EventKind::Decision, data);
    // The sink writes a warning line first when the append failed, then the
    // event's own line, which the gesture does not print.
    String::from_utf8_lossy(&out)
        .lines()
        .next()
        .filter(|line| line.starts_with("owlshift: warning:"))
        .map(ToOwned::to_owned)
}

/// Who asked `request`, when and why, its time local.
pub fn asked(request: Option<&Request>) -> String {
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

/// Pauses `target`, or every project of the machine of `data` when `None`,
/// with `request`, which replaces whatever pause stood: what follows, in
/// `owlshift pause`'s words, the way to lift it being the command's for a
/// terminal and the Orchestrator screen's Resume button for the UI (OWL-529).
pub fn pause(
    data: &Path,
    target: Option<&Repo>,
    request: &Request,
    voice: Voice,
) -> Result<Vec<String>, String> {
    let dirs = target.map(|repo| ProjectDirs::new(data, repo));
    let replaced = requests::ask_pause(data, dirs.as_ref(), request)
        .map_err(|error| format!("the pause could not be written: {error}"))?;
    let mut pairs = vec![("pause", json!("asked")), ("why", json!(request.why))];
    if let Some(before) = &replaced {
        pairs.push(("replaced", standing(before.as_ref())));
    }
    let warning = record(
        data,
        target,
        pairs,
        Some(request.by.as_str()).filter(|by| *by != BY_PAUSE),
    );
    let what = match target {
        Some(repo) => format!("{repo} is paused"),
        None => "Every project of this machine is paused".to_owned(),
    };
    let lifts = match voice {
        Voice::Terminal => format!(
            "`owlshift resume{}` lifts it.",
            target.map_or_else(String::new, |repo| format!(" {repo}"))
        ),
        Voice::Ui => "The Resume button of the Orchestrator screen lifts it.".to_owned(),
    };
    let mut lines = vec![format!(
        "{what}: from each watch's next pass, no ticket is dispatched and no step starts, nor a \
         merge on green; the steps under way finish, and answers, pull requests and stages are \
         still read. {lifts}"
    )];
    if let Some(before) = replaced {
        lines.push(format!("It replaces the pause {}.", asked(before.as_ref())));
    }
    if let Some(repo) = target
        && matches!(Registry::in_data_dir(data).find(repo), Ok(None))
    {
        lines.push(format!(
            "{repo} is not served by the background service; a watch typed in a terminal on it \
             is paused too."
        ));
    }
    lines.extend(warning);
    Ok(lines)
}

/// Lifts the pause of `target`, or of every project of the machine of
/// `data` when `None`: what follows, in `owlshift resume`'s words, with the
/// pauses that still stand, which it names as `pause` names how to lift one. The
/// event's `by` follows `voice`, `ui` for the UI and none for the terminal, as
/// a pause's follows its request; a pause that stood is recorded as lifted,
/// none otherwise.
pub fn resume(data: &Path, target: Option<&Repo>, voice: Voice) -> Result<Vec<String>, String> {
    let dirs = target.map(|repo| ProjectDirs::new(data, repo));
    let lifted = requests::lift_pause(data, dirs.as_ref())
        .map_err(|error| format!("the pause could not be removed: {error}"))?;
    let warning = lifted.as_ref().and_then(|before| {
        let pairs = vec![
            ("pause", json!("lifted")),
            ("was", standing(before.as_ref())),
        ];
        let by = match voice {
            Voice::Ui => Some(BY_UI),
            Voice::Terminal => None,
        };
        record(data, target, pairs, by)
    });
    let mut said = Vec::new();
    match (target, &lifted) {
        (Some(repo), Some(before)) => {
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
        (Some(repo), None) => said.push(format!("{repo} was not paused.")),
        (None, Some(before)) => said.push(format!(
            "The pause of every project is lifted (it was {}): each watch goes on at its next \
             pass.",
            asked(before.as_ref())
        )),
        (None, None) => said.push("Every project of this machine was not paused.".to_owned()),
    }
    match target {
        Some(_) => {
            if requests::machine_pause(data).is_some() {
                said.push(match voice {
                    Voice::Terminal => {
                        "Every project of this machine is still paused: `owlshift resume` lifts that."
                    }
                    Voice::Ui => {
                        "Every project of this machine is still paused: the machine's Resume button lifts that."
                    }
                }
                .to_owned());
            }
        }
        None => {
            let still: Vec<String> = Registry::in_data_dir(data)
                .read()
                .map(|served| served.projects)
                .unwrap_or_default()
                .into_iter()
                .filter(|entry| {
                    Repo::parse(&entry.repository)
                        .is_ok_and(|repo| requests::pause(&ProjectDirs::new(data, &repo)).is_some())
                })
                .map(|entry| entry.repository)
                .collect();
            if !still.is_empty() {
                let lifts = match voice {
                    Voice::Terminal => "`owlshift resume <owner/repo>`",
                    Voice::Ui => "each has its own Resume button",
                };
                said.push(format!(
                    "Still paused on their own: {} ({lifts}).",
                    still.join(", ")
                ));
            }
        }
    }
    said.extend(warning);
    Ok(said)
}

/// Stops serving `repo` on the machine of `data`, as `owlshift stop
/// --forget` does, under the list's lock: what follows, the daemon that
/// runs draining its watch.
pub fn forget(data: &Path, repo: &Repo) -> Result<Vec<String>, String> {
    let running = daemon::running(data)
        .map_err(|error| format!("{}: {error}", daemon::service_dir(data).display()))?;
    let line = match Registry::in_data_dir(data).forget(repo)? {
        true if running.is_some() => format!(
            "{repo} is no longer served: the daemon drains its watch at its next pass, within a \
             minute."
        ),
        true => format!("{repo} is no longer served."),
        false => format!("{repo} was not served."),
    };
    Ok(vec![line])
}

#[cfg(test)]
mod tests {
    use jiff::Timestamp;
    use owlshift_contracts::service::{BY_PAUSE, BY_UI};

    use super::*;

    fn repo() -> Repo {
        Repo::parse("acme/web").unwrap()
    }

    fn serve(data: &Path, repo: &Repo) {
        Registry::in_data_dir(data)
            .register(repo, &data.join("web"))
            .unwrap();
    }

    /// A project's pause writes its file holding the request and says what
    /// follows; a second one says what it replaced.
    #[test]
    fn a_project_pause_writes_its_file_and_says_what_it_replaced() {
        let data = tempfile::tempdir().unwrap();
        let repo = repo();
        serve(data.path(), &repo);
        let dirs = ProjectDirs::new(data.path(), &repo);
        let first = Request::new(BY_UI, Timestamp::now(), "asked from the web UI");
        let lines = pause(data.path(), Some(&repo), &first, Voice::Terminal).unwrap();
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].starts_with("acme/web is paused: from each watch's next pass"));
        assert!(lines[0].ends_with("`owlshift resume acme/web` lifts it."));
        assert_eq!(requests::pause(&dirs), Some(Some(first.clone())));
        let second = Request::new(BY_PAUSE, Timestamp::now(), "lunch");
        let lines = pause(data.path(), Some(&repo), &second, Voice::Terminal).unwrap();
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(
            lines[1].starts_with("It replaces the pause by ui at "),
            "{lines:?}"
        );
        assert!(lines[1].ends_with(": asked from the web UI."), "{lines:?}");
        assert_eq!(requests::pause(&dirs), Some(Some(second)));
    }

    /// A pause of a project the service does not serve says a typed watch
    /// is paused too; the machine's pause writes the service's file.
    #[test]
    fn the_machine_pause_and_an_unserved_projects_say_so() {
        let data = tempfile::tempdir().unwrap();
        let request = Request::new(BY_UI, Timestamp::now(), "asked from the web UI");
        let lines = pause(data.path(), Some(&repo()), &request, Voice::Terminal).unwrap();
        assert!(
            lines[1].starts_with("acme/web is not served by the background service"),
            "{lines:?}"
        );
        let lines = pause(data.path(), None, &request, Voice::Terminal).unwrap();
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].starts_with("Every project of this machine is paused:"));
        assert!(lines[0].ends_with("`owlshift resume` lifts it."));
        assert_eq!(requests::machine_pause(data.path()), Some(Some(request)));
    }

    /// Resume lifts a pause and says when it was the daemon's own, and what
    /// still stands: the machine's pause over a project, the projects paused
    /// on their own under the machine's.
    #[test]
    fn resume_lifts_a_pause_and_names_what_still_stands() {
        let data = tempfile::tempdir().unwrap();
        let repo = repo();
        serve(data.path(), &repo);
        let dirs = ProjectDirs::new(data.path(), &repo);
        let own = Request::new(BY_DAEMON, Timestamp::now(), "its watch cannot start");
        requests::ask_pause(data.path(), Some(&dirs), &own).unwrap();
        let machine = Request::new(BY_UI, Timestamp::now(), "asked from the web UI");
        requests::ask_pause(data.path(), None, &machine).unwrap();
        let lines = resume(data.path(), Some(&repo), Voice::Terminal).unwrap();
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert!(lines[0].starts_with("acme/web's pause is lifted (it was by owlshift daemon at "));
        assert!(lines[1].starts_with("That pause was the daemon's own"));
        assert_eq!(
            lines[2],
            "Every project of this machine is still paused: `owlshift resume` lifts that."
        );
        assert_eq!(requests::pause(&dirs), None);
        assert_eq!(
            resume(data.path(), Some(&repo), Voice::Terminal).unwrap()[0],
            "acme/web was not paused."
        );
        requests::ask_pause(data.path(), Some(&dirs), &machine).unwrap();
        let lines = resume(data.path(), None, Voice::Terminal).unwrap();
        assert!(lines[0].starts_with("The pause of every project is lifted (it was by ui at "));
        assert_eq!(
            lines[1],
            "Still paused on their own: acme/web (`owlshift resume <owner/repo>`)."
        );
        assert_eq!(requests::machine_pause(data.path()), None);
        assert_eq!(
            resume(data.path(), None, Voice::Terminal).unwrap()[0],
            "Every project of this machine was not paused."
        );
    }

    /// The UI's pause lines name the Resume button where the terminal's name
    /// `owlshift resume`, whole, and name no command to lift it (OWL-529).
    #[test]
    fn a_pause_from_the_ui_names_the_resume_button() {
        let data = tempfile::tempdir().unwrap();
        let repo = repo();
        serve(data.path(), &repo);
        let request = Request::new(BY_UI, Timestamp::now(), "asked from the web UI");
        let button = "The Resume button of the Orchestrator screen lifts it.";
        let lines = pause(data.path(), Some(&repo), &request, Voice::Ui).unwrap();
        assert!(lines[0].starts_with("acme/web is paused: from each watch's next pass"));
        assert!(lines[0].ends_with(button), "{lines:?}");
        let lines = pause(data.path(), None, &request, Voice::Ui).unwrap();
        assert!(lines[0].starts_with("Every project of this machine is paused:"));
        assert!(lines[0].ends_with(button), "{lines:?}");
        // The rest of a pause's lines is the same in both voices.
        let terminal = pause(data.path(), Some(&repo), &request, Voice::Terminal).unwrap();
        let ui = pause(data.path(), Some(&repo), &request, Voice::Ui).unwrap();
        assert_eq!(terminal.len(), ui.len());
        assert_eq!(terminal[1..], ui[1..]);
        let unserved = Request::new(BY_UI, Timestamp::now(), "x");
        let other = Repo::parse("acme/api").unwrap();
        for voice in [Voice::Terminal, Voice::Ui] {
            let lines = pause(data.path(), Some(&other), &unserved, voice).unwrap();
            assert!(
                lines.iter().any(
                    |line| line.starts_with("acme/api is not served by the background service")
                ),
                "{lines:?}"
            );
        }
    }

    /// The UI's resume lines name the Resume buttons, whole, for the machine's
    /// pause that stands over a project and for the projects paused on their
    /// own under it; none names `owlshift resume`.
    #[test]
    fn a_resume_from_the_ui_names_the_resume_buttons() {
        let data = tempfile::tempdir().unwrap();
        let repo = repo();
        serve(data.path(), &repo);
        let dirs = ProjectDirs::new(data.path(), &repo);
        let machine = Request::new(BY_UI, Timestamp::now(), "asked from the web UI");
        requests::ask_pause(data.path(), Some(&dirs), &machine).unwrap();
        requests::ask_pause(data.path(), None, &machine).unwrap();
        let lines = resume(data.path(), Some(&repo), Voice::Ui).unwrap();
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert_eq!(
            lines[1],
            "Every project of this machine is still paused: the machine's Resume button lifts that."
        );
        requests::ask_pause(data.path(), Some(&dirs), &machine).unwrap();
        let lines = resume(data.path(), None, Voice::Ui).unwrap();
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert_eq!(
            lines[1],
            "Still paused on their own: acme/web (each has its own Resume button)."
        );
        for line in &lines {
            assert!(!line.contains("owlshift resume"), "{line}");
        }
    }

    /// The events of the log in `data`, as JSON.
    fn events(data: &Path) -> Vec<Value> {
        std::fs::read_to_string(EventLog::in_dir(data).path())
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    /// A pause and a resume each leave one `decision` event naming the
    /// target, the reason, what the pause replaced or lifted, and `by` only
    /// when it is not the terminal; a resume with nothing standing leaves
    /// none.
    #[test]
    fn a_pause_and_a_resume_are_recorded() {
        let data = tempfile::tempdir().unwrap();
        let repo = repo();
        let first = Request::new(BY_UI, Timestamp::now(), "asked from the web UI");
        let lines = pause(data.path(), Some(&repo), &first, Voice::Terminal).unwrap();
        assert_eq!(lines.len(), 2, "{lines:?}");
        let second = Request::new(BY_PAUSE, Timestamp::now(), "lunch");
        pause(data.path(), Some(&repo), &second, Voice::Terminal).unwrap();
        let seen = events(data.path());
        assert_eq!(seen.len(), 2, "{seen:?}");
        assert_eq!(seen[0]["kind"], "decision");
        assert_eq!(seen[0]["project"], "acme/web");
        assert_eq!(seen[0]["data"]["pause"], "asked");
        assert_eq!(seen[0]["data"]["scope"], "project");
        assert_eq!(seen[0]["data"]["by"], "ui");
        assert!(seen[0]["data"].get("replaced").is_none());
        assert_eq!(seen[1]["data"]["why"], "lunch");
        assert!(seen[1]["data"].get("by").is_none(), "{seen:?}");
        assert_eq!(seen[1]["data"]["replaced"]["by"], "ui");
        assert_eq!(seen[1]["data"]["replaced"]["why"], "asked from the web UI");

        resume(data.path(), Some(&repo), Voice::Ui).unwrap();
        resume(data.path(), Some(&repo), Voice::Ui).unwrap();
        let seen = events(data.path());
        assert_eq!(seen.len(), 3, "nothing standing records nothing: {seen:?}");
        assert_eq!(seen[2]["data"]["pause"], "lifted");
        assert_eq!(seen[2]["data"]["by"], "ui");
        assert_eq!(seen[2]["data"]["was"]["why"], "lunch");
    }

    /// The machine's pause and resume are events of [`MACHINE_PROJECT`], and
    /// a resume from the terminal carries no `by`.
    #[test]
    fn the_machines_pause_and_resume_are_recorded() {
        let data = tempfile::tempdir().unwrap();
        let request = Request::new(BY_PAUSE, Timestamp::now(), "night");
        pause(data.path(), None, &request, Voice::Terminal).unwrap();
        resume(data.path(), None, Voice::Terminal).unwrap();
        let seen = events(data.path());
        assert_eq!(seen.len(), 2, "{seen:?}");
        for event in &seen {
            assert_eq!(event["project"], MACHINE_PROJECT);
            assert_eq!(event["data"]["scope"], "machine");
            assert!(event["data"].get("by").is_none(), "{event}");
        }
        assert_eq!(seen[1]["data"]["pause"], "lifted");
    }

    /// A pause file that does not read still stands: the events hold `{}`
    /// for what it replaced and what was lifted.
    #[test]
    fn an_unreadable_pause_file_is_recorded_as_an_empty_object() {
        let data = tempfile::tempdir().unwrap();
        let file = requests::machine_pause_file(data.path());
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "not json").unwrap();
        let request = Request::new(BY_PAUSE, Timestamp::now(), "night");
        pause(data.path(), None, &request, Voice::Terminal).unwrap();
        std::fs::write(&file, "not json").unwrap();
        resume(data.path(), None, Voice::Terminal).unwrap();
        let seen = events(data.path());
        assert_eq!(seen.len(), 2, "{seen:?}");
        assert_eq!(seen[0]["data"]["replaced"], json!({}));
        assert_eq!(seen[1]["data"]["was"], json!({}));
    }

    /// An event that cannot be appended adds a warning line and fails
    /// nothing.
    #[test]
    fn a_failed_append_adds_a_warning() {
        let data = tempfile::tempdir().unwrap();
        std::fs::create_dir(EventLog::in_dir(data.path()).path()).unwrap();
        let request = Request::new(BY_PAUSE, Timestamp::now(), "night");
        let lines = pause(data.path(), None, &request, Voice::Terminal).unwrap();
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(lines[1].starts_with("owlshift: warning: the event was not recorded"));
        assert!(requests::machine_pause(data.path()).is_some());
        let lines = resume(data.path(), None, Voice::Terminal).unwrap();
        assert!(lines.last().unwrap().starts_with("owlshift: warning:"));
        assert_eq!(requests::machine_pause(data.path()), None);
    }

    /// Forget removes the project from the list, and says it was not served
    /// the second time.
    #[test]
    fn forget_stops_serving_once() {
        let data = tempfile::tempdir().unwrap();
        let repo = repo();
        serve(data.path(), &repo);
        assert_eq!(
            forget(data.path(), &repo).unwrap(),
            vec!["acme/web is no longer served.".to_owned()]
        );
        assert!(
            Registry::in_data_dir(data.path())
                .find(&repo)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            forget(data.path(), &repo).unwrap(),
            vec!["acme/web was not served.".to_owned()]
        );
    }
}
