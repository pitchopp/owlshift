//! The background service (OWL-435, architecture section 5, "The background
//! service"): `owlshift daemon`, one process per data directory, which runs
//! one `owlshift watch --drain --served <repo>` per served project as its
//! child and keeps it alive, holding what one process per machine must hold.
//!
//! - *One instance* ([`instance`]): `<data>/service/daemon.lock`, held alone
//!   for the daemon's life, and `daemon.json`, its record (pid, version,
//!   binary, since).
//! - *The served lock* (`served::take_served`): taken for a project before
//!   its first watch and held across the watch's restarts; a watch typed in
//!   a terminal shares it, so each keeps the other off the project.
//! - *Restarts* (`owlshift_core::restart`): a watch that ends without being
//!   asked starts again after a delay doubling from 10 seconds to an hour,
//!   and the operator is told once at the third end in a row. A watch that
//!   ends with [`UNSERVABLE_EXIT`], a refusal no restart mends, pauses its
//!   project instead: it waits for you (OWL-650). `<project data>/pause` is
//!   written with the reason, said once and notified once, and the project
//!   tried again quietly at each pass, its watch's lines held until it ends
//!   for another reason or is past its start, when the pause is lifted: its
//!   watch record stands, or it still runs a pass later (OWL-651). The pause
//!   says the watch cannot start and holds none, so a watch so tried
//!   dispatches at its first pass.
//! - *Stops* ([`requests`]): `owlshift stop` writes `<data>/service/stop`,
//!   which the daemon relays as the stop file of every watch it runs (a
//!   drain), then exits once they have ended; a Ctrl-C does the same. A
//!   SIGTERM is a checked stop: the platform sends each watch SIGTERM, and
//!   the daemon exits once they have ended. A project no longer listed has
//!   its watch drained the same way.
//! - *An upgrade* ([`upgrade`], OWL-436): a newer binary writes
//!   `<data>/service/drain`, which the daemon relays as a stop does; once
//!   its watches have ended it exits with [`UPGRADE_EXIT`], so the service
//!   manager starts it again on the new binary. A stop asked meanwhile wins.
//!   Without a command (OWL-616): the daemon looks at the binary at the path
//!   it was started from ([`WatchedBinary`]) at each pass and writes that
//!   drain itself for a newer build; one the service did not start then
//!   hands its lock to that binary's daemon ([`successor`]).
//! - *The digest* ([`digest`]): once a day at the personal file's
//!   `digest_hour`, one notification, `<data>/service/digest.json` keeping
//!   when it was made so it is sent once at wake, never twice.
//!
//! - *The web UI* (OWL-467): the daemon binds `127.0.0.1:<[ui] port>` itself
//!   and keeps the socket for as long as it runs, handing it to each
//!   `owlshift ui --daemon` it starts, with a run secret it makes once per
//!   run, so the port is never free while it runs and a new daemon run ends
//!   every session. The UI starts again after an end as a watch does; a
//!   taken port is said, and tried again with the same delays. At a stop or
//!   an upgrade's drain it serves until every watch has ended, then is
//!   stopped; a SIGTERM stops it at once, with the runs.
//!
//! The watch's command is the caller's ([`Daemon::watch`]), and so is the
//! web UI's ([`Daemon::ui`]): this binary in `owlshift daemon`, a stand-in
//! on the test bench.

pub(crate) mod child;
pub mod digest;
pub mod instance;
pub mod requests;
pub mod successor;
mod supervise;
pub mod upgrade;

use std::cell::Cell;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use owlshift_adapters::forge::Repo;
use owlshift_adapters::notifier::Notifier;
use owlshift_contracts::service::{BuildIdentity, DaemonRecord};
use owlshift_core::restart::RestartPolicy;

pub use instance::{Held, Instance, pinned_dir, running, service_dir};
pub use requests::ask_stop;

use supervise::{Turn, say};
use upgrade::{Binary, Probed, Stamp, supersedes};

/// How long `owlshift start` waits for a daemon it launched to take its
/// lock; with [`upgrade::VERSION_WAIT`], how long a daemon waits for its
/// successor's record (OWL-608, OWL-616).
pub const LAUNCH_WAIT: Duration = Duration::from_secs(10);

/// The exit code of a served watch that refused to start for a reason no
/// restart mends (a `Refusal` that is lasting): the daemon pauses its
/// project rather than start it again on backoff. `EX_CONFIG` of
/// `sysexits.h`: something must be set up first.
pub const UNSERVABLE_EXIT: u8 = 78;

/// The exit code of a daemon whose upgrade's drain is done: the service
/// manager starts it again, on the new binary (architecture section 5,
/// "Restarts"). `EX_TEMPFAIL` of `sysexits.h`.
pub const UPGRADE_EXIT: u8 = 75;

/// How often the daemon does what.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timing {
    /// Between two reads of the served projects, and between two tries of a
    /// paused project.
    pub pass: Duration,
    /// Between two looks at the watches and the stop request.
    pub tick: Duration,
    /// When a watch that ended starts again.
    pub restarts: RestartPolicy,
}

impl Timing {
    /// The service's: a pass every 60 seconds, a look every half second, the
    /// service's restart rule.
    pub const SERVICE: Self = Self {
        pass: Duration::from_secs(60),
        tick: Duration::from_millis(500),
        restarts: RestartPolicy::SERVICE,
    };
}

/// The daemon of one data directory.
pub struct Daemon<'a> {
    /// The data directory, whose daemon lock the caller holds.
    pub data: &'a Path,
    /// The command of a served project's watch.
    pub watch: &'a dyn Fn(&Repo) -> Command,
    /// Where the operator is told of a project paused and of a watch that
    /// keeps ending; `None` tells no one.
    pub notifier: Option<&'a dyn Notifier>,
    pub timing: Timing,
    /// The daily digest's settings; `None` sends none.
    pub digest: Option<digest::Settings>,
    /// The web UI to run as a child; `None` runs none.
    pub ui: Option<UiLaunch<'a>>,
    /// The binary the daemon watches (OWL-616); `None` watches none.
    pub binary: Option<&'a WatchedBinary>,
}

/// The binary a daemon watches (OWL-616, architecture section 5, "An
/// upgrade", "Without a command"): the file at the path it was started
/// from, its own version and build, and the stamp it last saw there.
#[derive(Debug)]
pub struct WatchedBinary {
    pub path: PathBuf,
    pub version: String,
    pub build: Option<BuildIdentity>,
    /// The stamp seen last: the running file's at the start, then each one
    /// asked its `--version`, so a file is asked once per change.
    seen: Cell<Option<Stamp>>,
}

impl WatchedBinary {
    /// The binary at `path` as the daemon starts, before it pins: its stamp
    /// is the running file's, on Linux `/proc/self/exe`, elsewhere the path's,
    /// the pin's check then catching a file replaced as the daemon starts.
    pub fn at_start(path: PathBuf, version: String, build: Option<BuildIdentity>) -> Self {
        let running = Path::new("/proc/self/exe");
        let seen = if cfg!(target_os = "linux") && running.exists() {
            Stamp::of(running)
        } else {
            Stamp::of(&path)
        };
        Self {
            path,
            version,
            build,
            seen: Cell::new(seen.ok()),
        }
    }

    /// The daemon's own binary, as [`supersedes`] compares it.
    fn own(&self) -> Binary<'_> {
        Binary {
            version: &self.version,
            build: self.build.as_ref(),
        }
    }
}

/// A newer binary a daemon found at its path, whose drain it asked.
#[derive(Clone, Debug)]
pub(crate) struct Found {
    /// What it said for `--version`.
    probed: Probed,
    /// Its stamp when it said so.
    stamp: Stamp,
}

/// The web UI the daemon runs (OWL-467).
#[derive(Clone, Copy)]
pub struct UiLaunch<'a> {
    /// The port of `127.0.0.1` the daemon binds: `[ui] port`.
    pub port: u16,
    /// The UI's command, `owlshift ui --daemon`, to which the daemon adds
    /// `--listener-fd <n> --run-secret-fd <m>`.
    pub command: &'a dyn Fn() -> Command,
}

/// How the daemon ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ended {
    /// A stop asked of it is done: its watches ended, nothing restarts them.
    Stopped,
    /// An upgrade's drain is done, no stop asked meanwhile: its watches
    /// ended, and the daemon exits with [`UPGRADE_EXIT`].
    Upgraded,
    /// Its lock is handed to the daemon of the newer binary at its path
    /// (OWL-616), which runs on.
    Moved,
}

impl Ended {
    /// The daemon's exit code: [`UPGRADE_EXIT`] for the service manager to
    /// start the new binary, 0 otherwise.
    pub fn exit_code(self) -> u8 {
        match self {
            Self::Stopped | Self::Moved => 0,
            Self::Upgraded => UPGRADE_EXIT,
        }
    }
}

impl Daemon<'_> {
    /// Serves the listed projects until a stop asked of the daemon is done,
    /// saying what it does on `out`: the daemon of `instance`, recorded as
    /// `record`. An upgrade's drain that ends in a handover (OWL-616) hands
    /// the lock to the successor; one that did not take over leaves this
    /// daemon serving again, as at a restart.
    pub fn serve(&self, instance: &Instance, record: &DaemonRecord, out: &mut dyn Write) -> Ended {
        loop {
            let found = match self.run(out) {
                Turn::Ended(ended) => return ended,
                Turn::HandOver(found) => found,
            };
            let Some((binary, found)) = self.binary.zip(self.still_newer(found, out)) else {
                continue;
            };
            let to = successor::To {
                path: &binary.path,
                shown: found.probed.binary().shown(),
            };
            match successor::hand_over(
                self.data,
                instance,
                record,
                &to,
                successor::HANDOVER_WAIT,
                out,
            ) {
                successor::Handed::Done => return Ended::Moved,
                successor::Handed::Failed => {}
            }
        }
    }

    /// `found`, when the binary at the daemon's path is still that newer
    /// one: one changed again during the drain is asked its `--version`
    /// again, and taken only when it still supersedes this daemon.
    fn still_newer(&self, found: Found, out: &mut dyn Write) -> Option<Found> {
        let binary = self.binary?;
        let path = binary.path.display();
        let now = match Stamp::of(&binary.path) {
            Ok(now) if now == found.stamp => return Some(found),
            Ok(now) => now,
            Err(error) => {
                say(
                    out,
                    &format!(
                        "warning: the binary at {path} cannot be read any more ({error}): the \
                         daemon serves on, on its own binary."
                    ),
                );
                return None;
            }
        };
        binary.seen.set(Some(now));
        match upgrade::probe(&binary.path) {
            Ok(probed) if supersedes(probed.binary(), binary.own()) => {
                Some(Found { probed, stamp: now })
            }
            Ok(probed) => {
                say(
                    out,
                    &format!(
                        "The binary at {path} changed again during the drain, to version {}, \
                         which does not supersede this daemon's {}: it serves on.",
                        probed.binary().shown(),
                        binary.own().shown()
                    ),
                );
                None
            }
            Err(error) => {
                say(
                    out,
                    &format!(
                        "warning: the binary at {path} changed again during the drain and did not \
                         tell its version ({error}): the daemon serves on, on its own binary."
                    ),
                );
                None
            }
        }
    }
}
