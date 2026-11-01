//! A ticket's cancel (OWL-474, architecture section 5, "The local web UI",
//! "Cancel"): `owlshift cancel TICKET`, and later the UI's Cancel, write
//! `<project data>/tickets/<ticket>/cancel` ([`ask`]), one line of JSON
//! saying who asked, when and why, as a pause file. The ticket's step under
//! way watches the file ([`Watcher`]) and, on finding it, stops itself
//! checked (`owlshift_platform::process::stop_step`), its process going on;
//! the ticket's watch parks the ticket at its next pass
//! (`OnDemand::cancel_seen`) and removes the file.
//!
//! The file asks by standing. Only a file named `cancel` in a ticket's
//! folder is ever read or removed here: the folder holds other files.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use owlshift_contracts::ids::TicketId;
use owlshift_contracts::service::Request;
use owlshift_core::state::{Status, TicketState};
use owlshift_core::vocab::Stage;
use owlshift_platform::process;

use crate::daemon::requests;
use crate::executor::Git;
use crate::project::ProjectDirs;
use crate::ticket_ref::{self, Stored};
use crate::watch::presence;

/// The name of the cancel file in a ticket's folder.
pub const FILE: &str = "cancel";

/// How often a step looks at its ticket's cancel file: the stop then cuts
/// its runs well within the 5 seconds the design gives.
pub const POLL: Duration = Duration::from_millis(250);

/// The cancel file of `ticket` in `dirs`'s project.
pub fn file(dirs: &ProjectDirs, ticket: &TicketId) -> PathBuf {
    dirs.ticket_dir(ticket).join(FILE)
}

/// Whether a cancel stands on `ticket`, whatever its file holds.
pub fn stands(dirs: &ProjectDirs, ticket: &TicketId) -> bool {
    fs::symlink_metadata(file(dirs, ticket)).is_ok()
}

/// The cancel that stands on `ticket`, if any: what it holds, `None`
/// within for a file that does not read as a request.
pub fn read(dirs: &ProjectDirs, ticket: &TicketId) -> Option<Option<Request>> {
    read_at(&file(dirs, ticket))
}

/// The cancel file at `path`, read as [`read`] reads one. One that is not a
/// plain file stands but is never followed: its words reach the tracker.
pub(crate) fn read_at(path: &Path) -> Option<Option<Request>> {
    let metadata = fs::symlink_metadata(path).ok()?;
    if !metadata.is_file() {
        return Some(None);
    }
    requests::standing(path)
}

/// Removes the cancel file at `path`, if it is there.
pub(crate) fn remove_at(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

/// Why a cancel parks nothing of a ticket that has a ref.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotParked {
    /// It is parked already.
    Parked,
    /// It waits in the ready column: its admission is the person's.
    Ready,
    /// Its delivery ended.
    Ended,
}

impl NotParked {
    /// What it is, in words for a person.
    pub fn words(self, ticket: &TicketId) -> String {
        match self {
            Self::Parked => {
                format!("{ticket} is already parked: `owlshift continue {ticket}` restarts it")
            }
            Self::Ready => format!(
                "{ticket} waits in the ready column, where nothing of it runs: removing its \
                 admission keeps it there"
            ),
            Self::Ended => format!("{ticket}'s delivery has ended: nothing of it runs"),
        }
    }
}

/// Why a cancel parks nothing of a ticket in `status` whose delivery watch
/// follows when `followed`, if it parks nothing: the core parks one only
/// where a run can be in flight (`Event::Canceled`), and a ticket at Watch
/// whose pull request ended runs nothing more.
fn not_parked(status: Status, followed: bool) -> Option<NotParked> {
    match status {
        Status::Parked { .. } => Some(NotParked::Parked),
        Status::Active(Stage::Ready) => Some(NotParked::Ready),
        _ if status.stage() == Stage::Watch && !followed => Some(NotParked::Ended),
        _ => None,
    }
}

/// Whether a cancel parks the ticket in `state` whose ref keeps `stored`.
pub fn parkable(state: &TicketState, stored: &Stored) -> Result<(), NotParked> {
    let followed = stored
        .record
        .delivery
        .as_ref()
        .is_some_and(|delivery| delivery.followed());
    not_parked(state.status(), followed).map_or(Ok(()), Err)
}

/// What the PARKED comment says of the cancel `request`, `None` for a file
/// that does not read: the gesture that asked, never a person.
pub fn detail(request: Option<&Request>) -> String {
    match request {
        Some(request) => format!(
            "Canceled by `{}` at {} UTC: {}",
            request.by.replace('`', "'"),
            request.at.strftime("%Y-%m-%d %H:%M"),
            request.why
        ),
        None => "Canceled by a cancel file that does not say who wrote it".to_owned(),
    }
}

/// What [`ask`] wrote, and what its writer is told.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Asked {
    /// The cancel that stood before, which this one replaced.
    pub replaced: Option<Option<Request>>,
    /// A watch runs on the project, which parks the ticket at its next pass.
    pub watching: bool,
    /// The project is quarantined: no pass parks anything until it is
    /// lifted.
    pub quarantined: bool,
    /// A step of the ticket runs while Owlshift keeps no ref of it, or one
    /// at Ready: stopped before its first run keeps anything, nothing is
    /// parked.
    pub unkept: bool,
}

/// Writes the cancel `request` of `ticket` in `dirs`'s project, reading its
/// ref with `git` from the dedicated checkout. Refused, writing nothing, for
/// a ticket already parked or whose delivery ended, and for one of which
/// Owlshift keeps no ref, or a ref at Ready, while no step of it runs (its
/// lock free): a ticket in the ready column is its admission's. The caller
/// checks the ticket is the project's.
pub fn ask(
    git: &Git,
    dirs: &ProjectDirs,
    ticket: &TicketId,
    request: &Request,
) -> Result<Asked, String> {
    let checkout = dirs.checkout();
    let stored = if fs::symlink_metadata(&checkout).is_ok() {
        ticket_ref::read(git, &checkout, ticket)
            .map_err(|e| format!("{ticket}'s ticket ref could not be read: {e}"))?
    } else {
        None
    };
    let running = dirs
        .ticket_lock_held(ticket)
        .map_err(|e| format!("{ticket}'s lock could not be read: {e}"))?;
    let unkept = match &stored {
        Some(stored) => {
            let state = TicketState::try_from(&stored.record.state)
                .map_err(|e| format!("{ticket}'s ticket ref keeps no valid state: {e:?}"))?;
            match parkable(&state, stored) {
                Ok(()) => false,
                Err(NotParked::Ready) if running => true,
                Err(not) => return Err(not.words(ticket)),
            }
        }
        None if running => true,
        None => {
            return Err(format!(
                "Owlshift has nothing of {ticket} in flight: no step of it runs and it keeps no \
                 record of it"
            ));
        }
    };
    let path = file(dirs, ticket);
    let replaced = read_at(&path);
    crate::step::write_atomically(&path, &request.render())
        .map_err(|e| format!("the cancel file could not be written: {e}"))?;
    Ok(Asked {
        replaced,
        watching: presence::watching(dirs).unwrap_or(false),
        quarantined: dirs.quarantine().ok().flatten().is_some(),
        unkept,
    })
}

/// A step's watch over its ticket's cancel file (OWL-474), from
/// [`Watcher::start`] until it is dropped: on finding the file it asks the
/// step's checked stop, `owlshift_platform::process::stop_step`, and its
/// drop ends that stop. The stop is the process's, so at most one step of a
/// process is watched at a time: plain `owlshift watch` runs one step at a
/// time, and a step's child runs one.
pub struct Watcher {
    stop: Option<Sender<()>>,
    thread: Option<JoinHandle<()>>,
    fired: Arc<AtomicBool>,
    watching: bool,
}

impl Watcher {
    /// Starts watching `ticket`'s cancel file in `dirs`'s project: once now,
    /// before the step makes anything, then every [`POLL`] on a thread of
    /// its own. A thread that could not start leaves the step unwatched
    /// ([`Watcher::watching`]): its ticket is still parked at the next pass.
    pub fn start(dirs: &ProjectDirs, ticket: &TicketId) -> Self {
        let path = file(dirs, ticket);
        let fired = Arc::new(AtomicBool::new(false));
        if fs::symlink_metadata(&path).is_ok() {
            process::stop_step();
            fired.store(true, Ordering::SeqCst);
            return Self {
                stop: None,
                thread: None,
                fired,
                watching: true,
            };
        }
        let (stop, stopped) = mpsc::channel::<()>();
        let flag = Arc::clone(&fired);
        let thread = thread::Builder::new()
            .name("owlshift-cancel".to_owned())
            .spawn(move || {
                while let Err(RecvTimeoutError::Timeout) = stopped.recv_timeout(POLL) {
                    if fs::symlink_metadata(&path).is_ok() {
                        process::stop_step();
                        flag.store(true, Ordering::SeqCst);
                        return;
                    }
                }
            })
            .ok();
        let watching = thread.is_some();
        Self {
            stop: Some(stop),
            thread,
            fired,
            watching,
        }
    }

    /// Whether it found the file and stopped the step.
    pub fn fired(&self) -> bool {
        self.fired.load(Ordering::SeqCst)
    }

    /// Whether it watches the file: its thread started, or the file stood
    /// at its start.
    pub fn watching(&self) -> bool {
        self.watching
    }
}

impl Drop for Watcher {
    /// Ends the watch, then the step's stop: the thread is told and joined
    /// first, so no stop is asked once it has ended.
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        process::end_step_stop();
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use owlshift_adapters::forge::Repo;
    use owlshift_contracts::service::BY_CANCEL;
    use owlshift_core::vocab::Stage;

    use super::*;

    /// A cancel parks a ticket wherever a run can be in flight, at Watch
    /// only while its delivery is followed; never one parked or at Ready.
    #[test]
    fn a_cancel_parks_where_a_run_can_be_in_flight() {
        let parked = |at, awaiting_input| Status::Parked { at, awaiting_input };
        assert_eq!(
            not_parked(parked(Stage::Build, false), true),
            Some(NotParked::Parked)
        );
        assert_eq!(
            not_parked(parked(Stage::Build, true), false),
            Some(NotParked::Parked)
        );
        assert_eq!(
            not_parked(Status::Active(Stage::Ready), false),
            Some(NotParked::Ready)
        );
        assert_eq!(
            not_parked(Status::Active(Stage::Watch), false),
            Some(NotParked::Ended)
        );
        assert_eq!(
            not_parked(
                Status::NeedsInput {
                    return_to: Stage::Watch
                },
                false
            ),
            Some(NotParked::Ended)
        );
        assert_eq!(not_parked(Status::Active(Stage::Watch), true), None);
        for stage in [
            Stage::Intake,
            Stage::Design,
            Stage::DesignReview,
            Stage::Build,
            Stage::Verify,
            Stage::Deliver,
        ] {
            assert_eq!(not_parked(Status::Active(stage), false), None, "{stage:?}");
            let waiting = Status::NeedsInput { return_to: stage };
            assert_eq!(not_parked(waiting, false), None, "{stage:?}");
        }
    }

    /// The PARKED comment names the gesture, the time and the reason, a
    /// backtick in the gesture kept from closing its code span.
    #[test]
    fn the_detail_names_the_gesture_the_time_and_why() {
        let at = "2026-10-31T20:05:09Z".parse().unwrap();
        let request = Request::new(BY_CANCEL, at, "it rewrites the wrong module");
        assert_eq!(
            detail(Some(&request)),
            "Canceled by `owlshift cancel` at 2026-10-31 20:05 UTC: it rewrites the wrong module"
        );
        let odd = Request::new("a`b", at, "why");
        assert!(detail(Some(&odd)).starts_with("Canceled by `a'b` at"));
        assert_eq!(
            detail(None),
            "Canceled by a cancel file that does not say who wrote it"
        );
    }

    /// A step's watcher asks the step's stop once the file appears, and its
    /// drop ends it. The stop is the process's: nextest runs each test in a
    /// process of its own.
    #[test]
    fn a_watcher_stops_the_step_once_the_file_appears() {
        let data = tempfile::tempdir().unwrap();
        let dirs = ProjectDirs::new(data.path(), &Repo::parse("acme/web").unwrap());
        let ticket = TicketId::new("WEB-1").unwrap();
        let watcher = Watcher::start(&dirs, &ticket);
        assert!(watcher.watching());
        thread::sleep(POLL * 2);
        assert!(!process::step_stop_asked());
        assert!(!watcher.fired());
        let request = Request::new(BY_CANCEL, jiff::Timestamp::now(), "why");
        crate::step::write_atomically(&file(&dirs, &ticket), &request.render()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !process::step_stop_asked() {
            assert!(Instant::now() < deadline, "the step was not stopped");
            thread::sleep(Duration::from_millis(20));
        }
        assert!(watcher.fired());
        assert_eq!(read(&dirs, &ticket), Some(Some(request)));
        drop(watcher);
        assert!(!process::step_stop_asked());
    }
}
