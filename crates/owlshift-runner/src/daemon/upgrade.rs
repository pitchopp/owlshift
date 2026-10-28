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

use std::cmp::Ordering;
use std::fs;
use std::path::{Path, PathBuf};

use jiff::Timestamp;
use owlshift_contracts::service::{BuildIdentity, DaemonRecord, Request};
use semver::Version;

use super::instance::{self, service_dir};
use super::{UPGRADE_EXIT, requests};
use crate::install;

/// The file the service's launcher writes its pid to just before it
/// executes the daemon, which keeps that pid.
pub fn launched_file(data: &Path) -> PathBuf {
    service_dir(data).join("launched")
}

/// What a binary is, as far as an upgrade compares two: its version and,
/// when known, its build (OWL-462).
#[derive(Clone, Copy, Debug)]
pub struct Binary<'a> {
    pub version: &'a str,
    pub build: Option<&'a BuildIdentity>,
}

impl<'a> Binary<'a> {
    /// The binary a daemon's record names.
    pub fn of(record: &'a DaemonRecord) -> Self {
        Self {
            version: &record.version,
            build: record.build.as_ref(),
        }
    }

    /// Its version, then its build in a few words when known.
    pub fn shown(&self) -> String {
        match self.build {
            Some(build) => format!("{} ({})", self.version, build.short()),
            None => format!("{} (unknown build)", self.version),
        }
    }

    /// Whether its build tells its commit.
    fn identified(&self) -> bool {
        self.build.is_some_and(|build| build.commit.is_some())
    }
}

/// Whether the binary `own` supersedes a daemon running `running`
/// (architecture section 5, "An upgrade", "Newer"): a version strictly
/// greater by semver precedence, build metadata ignored; at equal versions,
/// both builds known, not the same clean build, and `own` built strictly
/// later. A version that does not parse, or a missing or unknown build,
/// supersedes nothing and is superseded by nothing.
pub fn supersedes(own: Binary<'_>, running: Binary<'_>) -> bool {
    let (Ok(own_version), Ok(running_version)) =
        (Version::parse(own.version), Version::parse(running.version))
    else {
        return false;
    };
    match own_version.cmp_precedence(&running_version) {
        Ordering::Greater => true,
        Ordering::Less => false,
        Ordering::Equal => match (own.build, running.build) {
            (Some(ours), Some(theirs)) if own.identified() && running.identified() => {
                !ours.same_build(theirs) && ours.built_at > theirs.built_at
            }
            _ => false,
        },
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
    /// It runs this version or a newer one, or this build or a later one.
    Current(DaemonRecord),
    /// It runs this version, its record naming no build (format 1, from
    /// before OWL-462), so whether it is older cannot be told.
    Unidentified(DaemonRecord),
    /// Its upgrade's drain is asked already.
    AlreadyAsked(DaemonRecord),
    /// It is older, but no manager would start this binary after it.
    Unmanaged(DaemonRecord, Unmanaged),
    /// It is older and managed: its drain is to be asked, or was.
    Ask(DaemonRecord),
    /// It ended while its drain was being asked, which was then withdrawn.
    EndedMeanwhile,
}

/// What `facts` call for, the binary being `own`: decided without reading
/// or writing anything.
pub fn decide(
    running: Option<Option<DaemonRecord>>,
    own: Binary<'_>,
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
    if !supersedes(own, Binary::of(&record)) {
        let same_version = matches!(
            (Version::parse(own.version), Version::parse(&record.version)),
            (Ok(a), Ok(b)) if a.cmp_precedence(&b).is_eq()
        );
        if same_version && own.identified() && !Binary::of(&record).identified() {
            return Handover::Unidentified(record);
        }
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

/// Hands the daemon of `data` over to this binary, `own` at `binary`, when
/// it is older and managed: writes its drain, asked `by` this command. A
/// daemon that ended, or was replaced, while the drain was written has it
/// withdrawn, so it never stops the next one.
pub fn hand_over(
    data: &Path,
    own: Binary<'_>,
    binary: &Path,
    by: &str,
) -> Result<Handover, String> {
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
        "an upgrade from version {} to {}, at {}",
        Binary::of(record).shown(),
        own.shown(),
        binary.display()
    );
    let request = Request::new(by, Timestamp::now(), why).for_pid(record.pid);
    requests::ask_drain(data, &request).map_err(|e| format!("the drain request: {e}"))?;
    let same = |now: &DaemonRecord| now.pid == record.pid && now.since == record.since;
    if !matches!(instance::running(data), Ok(Some(Some(now))) if same(&now)) {
        let _ = requests::remove_drain_request(data);
        return Ok(Handover::EndedMeanwhile);
    }
    Ok(decided)
}

impl Handover {
    /// What to tell the person, this binary being `own`: `None` when there
    /// is nothing to tell.
    pub fn line(&self, own: Binary<'_>) -> Option<String> {
        let who = |record: &DaemonRecord| {
            format!(
                "the daemon (pid {}, version {})",
                record.pid,
                Binary::of(record).shown()
            )
        };
        let own = own.shown();
        match self {
            Self::Unidentified(record) => Some(format!(
                "warning: {} records no build, being from before OWL-462, so this binary ({own}) \
                 cannot tell whether it is older: `owlshift stop`, then `owlshift start`, moves \
                 it to this binary.",
                who(record)
            )),
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
            build: None,
        }
    }

    /// A build of the commit `commit` repeated, `minute` minutes after a
    /// fixed time.
    fn build(commit: char, dirty: bool, minute: i64) -> BuildIdentity {
        BuildIdentity {
            commit: Some(commit.to_string().repeat(40)),
            dirty,
            built_at: Timestamp::from_second(1_761_600_000 + minute * 60).unwrap(),
            source: None,
        }
    }

    fn of<'a>(version: &'a str, build: Option<&'a BuildIdentity>) -> Binary<'a> {
        Binary { version, build }
    }

    /// OWL-436's rule across versions, whatever the builds; at one version,
    /// OWL-462's: a later build of another commit, or a later dirty build,
    /// supersedes; the same identity, a rebuild of one clean commit, an
    /// older build and a missing or unknown build do not.
    #[test]
    fn a_newer_version_or_a_later_build_of_another_commit_supersedes() {
        for (own, running, newer) in [
            ("0.0.0", "0.0.0", false),
            ("0.2.0", "0.1.9", true),
            ("0.1.0", "0.2.0", false),
            ("1.0.0", "1.0.0-rc.1", true),
            ("0.2.0+b", "0.2.0+a", false),
            ("0.2.0", "bench", false),
            ("garbage", "0.1.0", false),
        ] {
            assert_eq!(
                supersedes(of(own, None), of(running, None)),
                newer,
                "{own} over {running}"
            );
        }
        let early = build('a', false, 0);
        let other = build('b', false, 10);
        let rebuilt = build('a', false, 10);
        let dirty = build('a', true, 10);
        let unknown = BuildIdentity {
            commit: None,
            ..build('c', false, 20)
        };
        for (own, running, newer, case) in [
            (&other, &early, true, "a later build of another commit"),
            (
                &dirty,
                &early,
                true,
                "a later dirty build of the same commit",
            ),
            (&early, &early, false, "the same identity"),
            (&rebuilt, &early, false, "one clean commit built again"),
            (&early, &other, false, "an older build"),
            (&unknown, &early, false, "an unknown own build"),
            (&other, &unknown, false, "an unknown running build"),
        ] {
            assert_eq!(
                supersedes(of("0.0.0", Some(own)), of("0.0.0", Some(running))),
                newer,
                "{case}"
            );
        }
        assert!(!supersedes(of("0.0.0", Some(&other)), of("0.0.0", None)));
        assert!(supersedes(
            of("0.1.0", Some(&early)),
            of("0.0.0", Some(&other))
        ));
        assert!(!supersedes(
            of("0.0.0", Some(&other)),
            of("0.1.0", Some(&early))
        ));
    }

    /// Every outcome, from the facts alone: only an older, managed daemon
    /// with nothing asked of it yet is asked to drain; one of this version
    /// whose record names no build is told, not taken as current.
    #[test]
    fn only_an_older_managed_daemon_is_asked_to_drain() {
        let ok = |_: &DaemonRecord| Ok(());
        let own = of("0.2.0", None);
        let old = record(7, "0.1.0");
        let decide_with = |running, stop, drain, managed: Result<(), Unmanaged>| {
            decide(running, own, stop, drain, |_| managed)
        };
        assert_eq!(decide(None, own, false, false, ok), Handover::NoDaemon);
        assert_eq!(decide(Some(None), own, false, false, ok), Handover::Unknown);
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
            let line = decided.line(own).unwrap();
            assert!(
                line.starts_with("warning: the daemon (pid 7, version 0.1.0 (unknown build))")
                    && line.contains("`owlshift stop`, then `owlshift start`"),
                "{line}"
            );
        }
        let asked = decide_with(Some(Some(old.clone())), false, false, Ok(()));
        assert_eq!(asked, Handover::Ask(old));
        assert!(asked.line(own).unwrap().contains("exits with 75"));
        assert_eq!(Handover::Current(record(1, "0.2.0")).line(own), None);
        // OWL-462: this binary's build known, the record's not.
        let built = build('a', false, 0);
        let identified = of("0.2.0", Some(&built));
        let same = record(7, "0.2.0");
        let unidentified = decide(Some(Some(same.clone())), identified, false, false, ok);
        assert_eq!(unidentified, Handover::Unidentified(same));
        let line = unidentified.line(identified).unwrap();
        assert!(
            line.contains("records no build") && line.contains("`owlshift stop`, then"),
            "{line}"
        );
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
        let own = of("0.2.0", None);
        let handover = hand_over(&data, own, &binary, "owlshift start").unwrap();
        assert_eq!(handover, Handover::Ask(me.clone()));
        let asked = requests::drain_asked(&data).unwrap();
        assert_eq!(asked.by, "owlshift start");
        assert_eq!(asked.pid, Some(me.pid), "it names the daemon it is for");
        assert!(
            asked
                .why
                .contains("from version 0.1.0 (unknown build) to 0.2.0 (unknown build)"),
            "{asked:?}"
        );
        assert_eq!(
            hand_over(&data, own, &binary, "owlshift doctor"),
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
        let own = of("0.2.0", None);
        assert_eq!(
            hand_over(data, own, &binary, "owlshift doctor"),
            Ok(Handover::NoDaemon)
        );
        let held = instance::take(data, &record(std::process::id(), "0.2.0"))
            .unwrap()
            .unwrap();
        let handover = hand_over(data, own, &binary, "owlshift doctor").unwrap();
        assert!(matches!(handover, Handover::Current(_)), "{handover:?}");
        assert!(requests::drain_asked(data).is_none());
        // Older, not managed: told, not drained.
        drop(held);
        let held = instance::take(data, &record(std::process::id(), "0.1.0"))
            .unwrap()
            .unwrap();
        let handover = hand_over(data, own, &binary, "owlshift doctor").unwrap();
        assert!(
            matches!(handover, Handover::Unmanaged(_, Unmanaged::NotInstalled)),
            "{handover:?}"
        );
        assert!(requests::drain_asked(data).is_none());
        drop(held);
    }
}
