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
//!   project instead: `<project data>/pause` written with the reason, told
//!   once, tried again at each pass, and lifted once a watch tried so still
//!   runs a pass later.
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
//! - *The digest* ([`digest`]): once a day at the personal file's
//!   `digest_hour`, one notification, `<data>/service/digest.json` keeping
//!   when it was made so it is sent once at wake, never twice.
//!
//! The watch's command is the caller's ([`Daemon::watch`]): this binary in
//! `owlshift daemon`, a stand-in on the test bench.

pub(crate) mod child;
pub mod digest;
pub mod instance;
pub mod requests;
mod supervise;
pub mod upgrade;

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use owlshift_adapters::forge::Repo;
use owlshift_adapters::notifier::Notifier;
use owlshift_core::restart::RestartPolicy;

pub use instance::{Held, Instance, running, service_dir};
pub use requests::ask_stop;

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
}

/// How the daemon ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ended {
    /// A stop asked of it is done: its watches ended, nothing restarts them.
    Stopped,
    /// An upgrade's drain is done, no stop asked meanwhile: its watches
    /// ended, and the daemon exits with [`UPGRADE_EXIT`].
    Upgraded,
}
