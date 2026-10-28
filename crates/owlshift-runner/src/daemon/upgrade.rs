//! An upgrade (OWL-436, architecture section 5, "An upgrade"): a newer
//! `owlshift`, at `start` or at `doctor`, finds the daemon of an older
//! version from `daemon.json` and writes `<data>/service/drain`; the daemon
//! relays it to each watch as a stop does, and once they have ended exits
//! with [`UPGRADE_EXIT`], so the service manager starts
//! it again on the new binary.
//!
//! Only a daemon the manager would start again on this very binary is
//! handed over ([`managed`]): one the installed service's launcher started,
//! that service naming this binary. Any other is told, never drained, since
//! its drain would be a stop nobody asked for.

use std::fs;
use std::path::{Path, PathBuf};

use jiff::Timestamp;
use owlshift_contracts::service::{DaemonRecord, Request};
use semver::Version;

use super::instance::{self, service_dir};
use super::{UPGRADE_EXIT, requests};
use crate::install;

/// The file the service's launcher writes its pid to just before it
/// executes the daemon, which keeps that pid.
pub fn launched_file(data: &Path) -> PathBuf {
    service_dir(data).join("launched")
}

/// Whether the binary of version `own` supersedes a daemon of version
/// `running`: strictly newer by semver precedence, build metadata ignored.
/// A version that does not parse supersedes nothing and is superseded by
/// nothing. The one seam where a build identity (OWL-462) sharpens what
/// "newer" means.
pub fn supersedes(own: &str, running: &str) -> bool {
    match (Version::parse(own), Version::parse(running)) {
        (Ok(own), Ok(running)) => own.cmp_precedence(&running).is_gt(),
        _ => false,
    }
}

/// Why no service manager would start the running daemon again on this
/// binary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Unmanaged {
    /// No service at login is installed for the data directory.
    NotInstalled,
    /// The install manifest does not read.
    Unread(String),
    /// The service starts another binary.
    OtherBinary(String),
    /// The daemon was not started by the service's launcher: by a plain
    /// `owlshift start`, or by a launcher older than OWL-436.
    NotLaunched,
}

/// Whether the service manager would start the daemon `running` again on
/// the binary `own` once it ends: a service is installed for `data`, its
/// binary is `own` (both resolved to their files), and its launcher started
/// `running` (the marker names its pid).
pub fn managed(data: &Path, running: &DaemonRecord, own: &Path) -> Result<(), Unmanaged> {
    let service = install::installed(data)
        .map_err(Unmanaged::Unread)?
        .ok_or(Unmanaged::NotInstalled)?;
    let same = match (fs::canonicalize(&service.binary), fs::canonicalize(own)) {
        (Ok(theirs), Ok(ours)) => theirs == ours,
        _ => false,
    };
    if !same {
        return Err(Unmanaged::OtherBinary(service.binary));
    }
    let launched = fs::read_to_string(launched_file(data))
        .ok()
        .and_then(|text| text.trim().parse::<u32>().ok());
    if launched != Some(running.pid) {
        return Err(Unmanaged::NotLaunched);
    }
    Ok(())
}

/// What a newer binary does about the daemon of its data directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Handover {
    /// No daemon runs.
    NoDaemon,
    /// A daemon runs whose record does not read: its version is unknown,
    /// so it is left as it is.
    Unknown,
    /// A stop is asked of it already.
    Stopping,
    /// It runs this version or a newer one.
    Current(DaemonRecord),
    /// Its upgrade's drain is asked already.
    AlreadyAsked(DaemonRecord),
    /// It is older, but no manager would start this binary after it.
    Unmanaged(DaemonRecord, Unmanaged),
    /// It is older and managed: its drain is to be asked, or was.
    Ask(DaemonRecord),
    /// It ended while its drain was being asked, which was then withdrawn.
    EndedMeanwhile,
}

/// What `facts` call for, the binary being of version `own`: decided
/// without reading or writing anything.
pub fn decide(
    running: Option<Option<DaemonRecord>>,
    own: &str,
    stop_asked: bool,
    drain_asked: bool,
    managed: impl FnOnce(&DaemonRecord) -> Result<(), Unmanaged>,
) -> Handover {
    let record = match running {
        None => return Handover::NoDaemon,
        Some(None) => return Handover::Unknown,
        Some(Some(record)) => record,
    };
    if stop_asked {
        return Handover::Stopping;
    }
    if !supersedes(own, &record.version) {
        return Handover::Current(record);
    }
    if drain_asked {
        return Handover::AlreadyAsked(record);
    }
    match managed(&record) {
        Ok(()) => Handover::Ask(record),
        Err(why) => Handover::Unmanaged(record, why),
    }
}

/// Hands the daemon of `data` over to this binary, of version `own` at
/// `binary`, when it is older and managed: writes its drain, asked `by`
/// this command. A daemon that ended, or was replaced, while the drain was
/// written has it withdrawn, so it never stops the next one.
pub fn hand_over(data: &Path, own: &str, binary: &Path, by: &str) -> Result<Handover, String> {
    let shown = |error: std::io::Error| format!("{}: {error}", service_dir(data).display());
    let running = instance::running(data).map_err(shown)?;
    let decided = decide(
        running,
        own,
        requests::stop_asked(data).is_some(),
        requests::drain_asked(data).is_some(),
        |record| managed(data, record, binary),
    );
    let Handover::Ask(record) = &decided else {
        return Ok(decided);
    };
    let why = format!(
        "an upgrade from version {} to {own}, at {}",
        record.version,
        binary.display()
    );
    requests::ask_drain(data, &Request::new(by, Timestamp::now(), why))
        .map_err(|e| format!("the drain request: {e}"))?;
    let same = |now: &DaemonRecord| now.pid == record.pid && now.since == record.since;
    if !matches!(instance::running(data), Ok(Some(Some(now))) if same(&now)) {
        let _ = requests::remove_drain_request(data);
        return Ok(Handover::EndedMeanwhile);
    }
    Ok(decided)
}

impl Handover {
    /// What to tell the person, this binary being of version `own`: `None`
    /// when there is nothing to tell.
    pub fn line(&self, own: &str) -> Option<String> {
        let who = |record: &DaemonRecord| {
            format!(
                "the daemon (pid {}, version {})",
                record.pid, record.version
            )
        };
        match self {
            Self::Ask(record) => Some(format!(
                "Upgrade: {} drains for version {own}: its steps under way finish and no new one \
                 starts, then it exits with {UPGRADE_EXIT} and the service at login starts it \
                 again on this binary.",
                who(record)
            )),
            Self::AlreadyAsked(record) => Some(format!(
                "Upgrade: {} is already draining for a newer version; the service at login starts \
                 it again once its steps under way finish.",
                who(record)
            )),
            Self::Unmanaged(record, why) => {
                let why = match why {
                    Unmanaged::NotInstalled => {
                        "no service at login would start it again".to_owned()
                    }
                    Unmanaged::Unread(error) => {
                        format!("the install manifest does not read ({error})")
                    }
                    Unmanaged::OtherBinary(binary) => format!(
                        "the service at login starts {binary}, not this binary (`owlshift start \
                         --at-login` names this one)"
                    ),
                    Unmanaged::NotLaunched => "the service at login did not start it".to_owned(),
                };
                Some(format!(
                    "warning: {} is older than this binary ({own}) and {why}: `owlshift stop`, \
                     then `owlshift start`, moves it to this binary.",
                    who(record)
                ))
            }
            Self::NoDaemon
            | Self::Unknown
            | Self::Stopping
            | Self::Current(_)
            | Self::EndedMeanwhile => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use owlshift_contracts::format::Format;

    use super::*;

    fn record(pid: u32, version: &str) -> DaemonRecord {
        DaemonRecord {
            format: Format,
            pid,
            version: version.to_owned(),
            binary: "/bin/owlshift".to_owned(),
            since: Timestamp::now(),
        }
    }

    #[test]
    fn only_a_strictly_newer_version_supersedes() {
        for (own, running, newer) in [
            ("0.0.0", "0.0.0", false),
            ("0.2.0", "0.1.9", true),
            ("0.1.0", "0.2.0", false),
            ("1.0.0", "1.0.0-rc.1", true),
            ("0.2.0+b", "0.2.0+a", false),
            ("0.2.0", "bench", false),
            ("garbage", "0.1.0", false),
        ] {
            assert_eq!(supersedes(own, running), newer, "{own} over {running}");
        }
    }

    /// Every outcome, from the facts alone: only an older, managed daemon
    /// with nothing asked of it yet is asked to drain.
    #[test]
    fn only_an_older_managed_daemon_is_asked_to_drain() {
        let ok = |_: &DaemonRecord| Ok(());
        let old = record(7, "0.1.0");
        let decide_with = |running, stop, drain, managed: Result<(), Unmanaged>| {
            decide(running, "0.2.0", stop, drain, |_| managed)
        };
        assert_eq!(decide(None, "0.2.0", false, false, ok), Handover::NoDaemon);
        assert_eq!(
            decide(Some(None), "0.2.0", false, false, ok),
            Handover::Unknown
        );
        assert_eq!(
            decide_with(Some(Some(old.clone())), true, false, Ok(())),
            Handover::Stopping
        );
        let same = record(7, "0.2.0");
        assert_eq!(
            decide_with(Some(Some(same.clone())), false, false, Ok(())),
            Handover::Current(same)
        );
        assert_eq!(
            decide_with(Some(Some(old.clone())), false, true, Ok(())),
            Handover::AlreadyAsked(old.clone())
        );
        for why in [
            Unmanaged::NotInstalled,
            Unmanaged::OtherBinary("/elsewhere/owlshift".to_owned()),
            Unmanaged::NotLaunched,
        ] {
            let decided = decide_with(Some(Some(old.clone())), false, false, Err(why.clone()));
            assert_eq!(decided, Handover::Unmanaged(old.clone(), why));
            let line = decided.line("0.2.0").unwrap();
            assert!(
                line.starts_with("warning: the daemon (pid 7, version 0.1.0)")
                    && line.contains("`owlshift stop`, then `owlshift start`"),
                "{line}"
            );
        }
        let asked = decide_with(Some(Some(old.clone())), false, false, Ok(()));
        assert_eq!(asked, Handover::Ask(old));
        assert!(asked.line("0.2.0").unwrap().contains("exits with 75"));
        assert_eq!(Handover::Current(record(1, "0.2.0")).line("0.2.0"), None);
    }

    /// The managed test: a service installed for this binary, whose
    /// launcher started the running daemon.
    #[cfg(unix)]
    #[test]
    fn a_daemon_is_managed_only_when_the_service_started_it_on_this_binary() {
        use owlshift_platform::service::{Kind, Layout};

        let root = tempfile::tempdir().unwrap();
        let data = root.path().join("data");
        let binary = root.path().join("bin/owlshift");
        fs::create_dir_all(binary.parent().unwrap()).unwrap();
        fs::write(&binary, "").unwrap();
        let running = record(4242, "0.1.0");
        assert_eq!(
            managed(&data, &running, &binary),
            Err(Unmanaged::NotInstalled)
        );
        let layout = Layout {
            kind: Kind::Systemd,
            dir: root.path().join("units"),
        };
        install::install(&install::tests::spec(&data, &layout, &binary)).unwrap();
        assert_eq!(
            managed(&data, &running, &binary),
            Err(Unmanaged::NotLaunched),
            "no marker: a session daemon, or an older launcher"
        );
        fs::write(launched_file(&data), "4241\n").unwrap();
        assert_eq!(
            managed(&data, &running, &binary),
            Err(Unmanaged::NotLaunched)
        );
        fs::write(launched_file(&data), "4242\n").unwrap();
        assert_eq!(managed(&data, &running, &binary), Ok(()));
        // The same file by another path, as a package manager's link.
        let link = root.path().join("owlshift-link");
        std::os::unix::fs::symlink(&binary, &link).unwrap();
        assert_eq!(managed(&data, &running, &link), Ok(()));
        let other = root.path().join("bin/owlshift-dev");
        fs::write(&other, "").unwrap();
        assert_eq!(
            managed(&data, &running, &other),
            Err(Unmanaged::OtherBinary(binary.display().to_string()))
        );

        // A managed daemon that runs: its drain is written, once.
        let me = record(std::process::id(), "0.1.0");
        fs::write(launched_file(&data), format!("{}\n", me.pid)).unwrap();
        let held = instance::take(&data, &me).unwrap().unwrap();
        let handover = hand_over(&data, "0.2.0", &binary, "owlshift start").unwrap();
        assert_eq!(handover, Handover::Ask(me.clone()));
        let asked = requests::drain_asked(&data).unwrap();
        assert_eq!(asked.by, "owlshift start");
        assert!(
            asked.why.contains("from version 0.1.0 to 0.2.0"),
            "{asked:?}"
        );
        assert_eq!(
            hand_over(&data, "0.2.0", &binary, "owlshift doctor"),
            Ok(Handover::AlreadyAsked(me))
        );
        drop(held);
    }

    /// `hand_over` writes the drain only when it asks, and withdraws it
    /// when no daemon runs; a daemon of this version is left alone.
    #[test]
    fn a_handover_writes_the_drain_only_for_a_daemon_it_asks() {
        let data = tempfile::tempdir().unwrap();
        let data = data.path();
        let binary = data.join("owlshift");
        assert_eq!(
            hand_over(data, "0.2.0", &binary, "owlshift doctor"),
            Ok(Handover::NoDaemon)
        );
        let held = instance::take(data, &record(std::process::id(), "0.2.0"))
            .unwrap()
            .unwrap();
        let handover = hand_over(data, "0.2.0", &binary, "owlshift doctor").unwrap();
        assert!(matches!(handover, Handover::Current(_)), "{handover:?}");
        assert!(requests::drain_asked(data).is_none());
        // Older, not managed: told, not drained.
        drop(held);
        let held = instance::take(data, &record(std::process::id(), "0.1.0"))
            .unwrap()
            .unwrap();
        let handover = hand_over(data, "0.2.0", &binary, "owlshift doctor").unwrap();
        assert!(
            matches!(handover, Handover::Unmanaged(_, Unmanaged::NotInstalled)),
            "{handover:?}"
        );
        assert!(requests::drain_asked(data).is_none());
        drop(held);
    }
}
