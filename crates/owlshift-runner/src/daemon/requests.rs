//! The files that ask the daemon and its watches for something, each one
//! line of JSON saying who asked, when and why ([`Request`]), each asking by
//! standing (architecture section 5, "Three ways to stop" and "Pause"):
//!
//! - `<data>/service/stop`, which `owlshift stop` writes: the daemon drains
//!   every watch it runs, writing each one's stop file, then exits;
//! - `<project data>/stop`, a watch's drain (OWL-161, `watch::stop`), which
//!   the daemon writes for a watch it stops;
//! - `<project data>/pause`, which the daemon writes, with its reason, for a
//!   project whose watch could not start for a reason no restart mends.

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
    let path = pause_file(dirs);
    fs::symlink_metadata(&path).ok()?;
    Some(
        fs::read_to_string(&path)
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
    use owlshift_contracts::service::BY_STOP;

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
        assert_eq!(pause(&dirs), Some(Some(person)));
        let own = Request::new(BY_DAEMON, Timestamp::now(), "keychain locked");
        write_pause(&dirs, &own).unwrap();
        assert!(remove_own_pause(&dirs).unwrap());
        assert_eq!(pause(&dirs), None);
    }
}
