//! The files that ask the daemon and its watches for something, each one
//! line of JSON saying who asked, when and why ([`Request`]), each asking by
//! standing (architecture section 5, "Three ways to stop" and "Pause"):
//!
//! - `<data>/service/stop`, which `owlshift stop` writes: the daemon drains
//!   every watch it runs, writing each one's stop file, then exits;
//! - `<project data>/stop`, a watch's drain (OWL-161, `watch::stop`), which
//!   the daemon writes for a watch it stops;
//! - `<project data>/pause`, which the daemon writes, with its reason, for a
//!   project whose watch could not start for a reason no restart mends;
//! - `<data>/service/pause`, the pause of every project, which the daily
//!   digest reads (OWL-440).
//!
//! `owlshift pause` and `owlshift resume` (OWL-438) write and remove both
//! pauses ([`ask_pause`], [`lift_pause`]), and every watch reads them at each
//! pass and before each step ([`operator_pause`]): while one stands it
//! dispatches nothing and starts no step.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use owlshift_contracts::service::{BY_DAEMON, Request};

use super::instance::service_dir;
use crate::project::ProjectDirs;
use crate::{step, watch};

/// The daemon's stop request in `data`.
pub fn stop_request_file(data: &Path) -> PathBuf {
    service_dir(data).join("stop")
}

/// Asks the daemon of `data` to drain and exit.
pub fn ask_stop(data: &Path, request: &Request) -> io::Result<()> {
    step::write_atomically(&stop_request_file(data), &request.render())
}

/// What asks the daemon of `data` to stop, when a request stands: what it
/// holds, or, when that does not read, a request of nobody known.
pub fn stop_asked(data: &Path) -> Option<Request> {
    let path = stop_request_file(data);
    fs::symlink_metadata(&path).ok()?;
    let text = fs::read_to_string(&path).unwrap_or_default();
    Some(Request::parse(&text).unwrap_or_else(|| {
        Request::new(
            "an unknown writer",
            jiff::Timestamp::now(),
            "a stop request that does not read",
        )
    }))
}

/// Removes the daemon's stop request in `data`, if there is one.
pub fn remove_stop_request(data: &Path) -> io::Result<()> {
    remove_if_any(&stop_request_file(data))
}

/// Asks the watch of `dirs`'s project to drain, by its stop file.
pub fn ask_watch_stop(dirs: &ProjectDirs, request: &Request) -> io::Result<()> {
    step::write_atomically(&watch::stop::file(dirs), &request.render())
}

/// The pause file of `dirs`'s project.
pub fn pause_file(dirs: &ProjectDirs) -> PathBuf {
    dirs.root().join("pause")
}

/// The pause that stands on `dirs`'s project, if any: what it holds, `None`
/// for a file that does not read as a request.
pub fn pause(dirs: &ProjectDirs) -> Option<Option<Request>> {
    standing(&pause_file(dirs))
}

/// The pause of every project of the machine, `<data>/service/pause`
/// (architecture section 5, "Pause").
pub fn machine_pause_file(data: &Path) -> PathBuf {
    service_dir(data).join("pause")
}

/// The pause that stands on every project of `data`, if any, read as
/// [`pause`] reads a project's.
pub fn machine_pause(data: &Path) -> Option<Option<Request>> {
    standing(&machine_pause_file(data))
}

/// The data directory `dirs` lies in: `ProjectDirs::new` places a project at
/// `<data>/projects/github.com/<owner>/<name>`, which a test pins.
pub fn data_of(dirs: &ProjectDirs) -> &Path {
    let root = dirs.root();
    root.ancestors().nth(4).unwrap_or(root)
}

/// What an operator's pause covers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PauseScope {
    /// Every project of the machine: `<data>/service/pause`.
    Machine,
    /// One project: `<project data>/pause`.
    Project,
}

impl PauseScope {
    /// Its name in events.
    pub fn key(self) -> &'static str {
        match self {
            Self::Machine => "machine",
            Self::Project => "project",
        }
    }
}

/// The pause that stands on `dirs`'s project, the machine's first: what it
/// covers and what it holds, `None` within for a file that does not read.
pub fn operator_pause(dirs: &ProjectDirs) -> Option<(PauseScope, Option<Request>)> {
    machine_pause(data_of(dirs))
        .map(|request| (PauseScope::Machine, request))
        .or_else(|| pause(dirs).map(|request| (PauseScope::Project, request)))
}

/// The pause file of `dirs`'s project, or the machine's of `data` when
/// `dirs` is `None`.
fn pause_path(data: &Path, dirs: Option<&ProjectDirs>) -> PathBuf {
    dirs.map_or_else(|| machine_pause_file(data), pause_file)
}

/// Writes `request` as the pause of `dirs`'s project, or of every project of
/// `data` when `dirs` is `None`, replacing whatever stood, a daemon's own
/// pause included, so the daemon never lifts it: what stood before.
pub fn ask_pause(
    data: &Path,
    dirs: Option<&ProjectDirs>,
    request: &Request,
) -> io::Result<Option<Option<Request>>> {
    let path = pause_path(data, dirs);
    let before = standing(&path);
    step::write_atomically(&path, &request.render())?;
    Ok(before)
}

/// Removes the pause of `dirs`'s project, or of every project of `data`
/// when `dirs` is `None`: what stood, `None` when nothing did.
pub fn lift_pause(data: &Path, dirs: Option<&ProjectDirs>) -> io::Result<Option<Option<Request>>> {
    let path = pause_path(data, dirs);
    let before = standing(&path);
    if before.is_some() {
        remove_if_any(&path)?;
    }
    Ok(before)
}

/// The request the file at `path` holds, when it stands: `None` within for
/// a file that does not read as one.
fn standing(path: &Path) -> Option<Option<Request>> {
    fs::symlink_metadata(path).ok()?;
    Some(
        fs::read_to_string(path)
            .ok()
            .and_then(|text| Request::parse(&text)),
    )
}

/// Writes the daemon's own pause of `dirs`'s project.
pub fn write_pause(dirs: &ProjectDirs, request: &Request) -> io::Result<()> {
    step::write_atomically(&pause_file(dirs), &request.render())
}

/// Removes the pause of `dirs`'s project when it is the daemon's own: one a
/// person or another command wrote stays. `true` when it removed one.
pub fn remove_own_pause(dirs: &ProjectDirs) -> io::Result<bool> {
    match pause(dirs) {
        Some(Some(request)) if request.by == BY_DAEMON => {
            remove_if_any(&pause_file(dirs))?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

fn remove_if_any(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use jiff::Timestamp;
    use owlshift_adapters::forge::Repo;
    use owlshift_contracts::service::{BY_PAUSE, BY_STOP};

    use super::*;

    #[test]
    fn requests_ask_by_standing_and_only_the_daemons_pause_is_its_to_lift() {
        let data = tempfile::tempdir().unwrap();
        let data = data.path();
        assert_eq!(stop_asked(data), None);
        let request = Request::new(BY_STOP, Timestamp::now(), "asked");
        ask_stop(data, &request).unwrap();
        assert_eq!(stop_asked(data), Some(request.clone()));
        fs::write(stop_request_file(data), "stop").unwrap();
        assert_eq!(
            stop_asked(data).map(|r| r.by),
            Some("an unknown writer".to_owned())
        );
        remove_stop_request(data).unwrap();
        remove_stop_request(data).unwrap();
        assert_eq!(stop_asked(data), None);

        let dirs = ProjectDirs::new(data, &Repo::parse("acme/web").unwrap());
        ask_watch_stop(&dirs, &request).unwrap();
        assert!(watch::stop::stands(&dirs));

        assert_eq!(pause(&dirs), None);
        let person = Request::new("owlshift pause", Timestamp::now(), "lunch");
        write_pause(&dirs, &person).unwrap();
        assert!(!remove_own_pause(&dirs).unwrap(), "a person's pause stays");
        assert_eq!(pause(&dirs), Some(Some(person.clone())));
        let own = Request::new(BY_DAEMON, Timestamp::now(), "keychain locked");
        write_pause(&dirs, &own).unwrap();
        assert!(remove_own_pause(&dirs).unwrap());
        assert_eq!(pause(&dirs), None);

        assert_eq!(machine_pause(data), None);
        step::write_atomically(&machine_pause_file(data), &person.render()).unwrap();
        assert_eq!(machine_pause(data), Some(Some(person)));
    }

    /// OWL-438: `ProjectDirs::new`'s layout gives back its data directory.
    #[test]
    fn a_project_folder_names_its_data_directory() {
        let data = Path::new("/var/owlshift data");
        let dirs = ProjectDirs::new(data, &Repo::parse("acme/web").unwrap());
        assert_eq!(data_of(&dirs), data);
    }

    /// OWL-438: a person's pause replaces a daemon's own, which the daemon
    /// then never lifts; the machine's stands over a project's; a lift says
    /// what it removed.
    #[test]
    fn a_persons_pause_replaces_the_daemons_and_the_machines_stands_first() {
        let data = tempfile::tempdir().unwrap();
        let data = data.path();
        let dirs = ProjectDirs::new(data, &Repo::parse("acme/web").unwrap());
        assert_eq!(operator_pause(&dirs), None);
        assert_eq!(lift_pause(data, Some(&dirs)).unwrap(), None);

        let own = Request::new(BY_DAEMON, Timestamp::now(), "keychain locked");
        write_pause(&dirs, &own).unwrap();
        let person = Request::new(BY_PAUSE, Timestamp::now(), "lunch");
        let replaced = ask_pause(data, Some(&dirs), &person).unwrap();
        assert_eq!(replaced, Some(Some(own)));
        assert!(!remove_own_pause(&dirs).unwrap(), "the person's stays");
        assert_eq!(
            operator_pause(&dirs),
            Some((PauseScope::Project, Some(person.clone())))
        );

        let all = Request::new(BY_PAUSE, Timestamp::now(), "travel");
        assert_eq!(ask_pause(data, None, &all).unwrap(), None);
        assert_eq!(
            operator_pause(&dirs),
            Some((PauseScope::Machine, Some(all.clone())))
        );
        assert_eq!(lift_pause(data, None).unwrap(), Some(Some(all)));
        assert_eq!(
            lift_pause(data, Some(&dirs)).unwrap(),
            Some(Some(person))
        );
        assert_eq!(operator_pause(&dirs), None);
        // An empty file still pauses.
        fs::write(pause_file(&dirs), "").unwrap();
        assert_eq!(operator_pause(&dirs), Some((PauseScope::Project, None)));
    }
}
