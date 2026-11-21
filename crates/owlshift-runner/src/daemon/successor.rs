//! A daemon's lock handed to the daemon of a newer binary (OWL-616,
//! architecture section 5, "An upgrade", "Without a command"): once its
//! drain is done, a daemon the service did not start removes its record,
//! keeping the lock, starts `<path> daemon --lock-fd <n>` with a copy of the
//! lock's descriptor, and waits for that daemon's record. The lock is never
//! free: both processes hold one open file description while both live.
//!
//! The successor that writes its record takes over; one that ends at its
//! start with success found a stop asked, and this daemon ends too; one that
//! fails, cannot start, or has not recorded itself within the wait leaves
//! this daemon serving, its record written again. A signal asked of this
//! daemon meanwhile is sent on to the successor, and this daemon ends once
//! the successor has.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use owlshift_contracts::service::DaemonRecord;
use owlshift_platform::process;

use super::instance::{self, Instance};
use super::supervise::say;
use super::{LAUNCH_WAIT, upgrade};

/// How long a daemon waits for its successor's record: `start`'s wait for a
/// daemon's lock and the pin check's bound, as `start` waits for a daemon it
/// launched (OWL-608).
pub const HANDOVER_WAIT: Duration =
    Duration::from_secs(LAUNCH_WAIT.as_secs() + upgrade::VERSION_WAIT.as_secs());

/// How often the wait looks.
const POLL: Duration = Duration::from_millis(100);

/// The binary the lock is handed to.
pub struct To<'a> {
    /// Its path, the daemon's own.
    pub path: &'a Path,
    /// Its version and build, in words.
    pub shown: String,
}

/// How a handover ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Handed {
    /// The successor took over, or the stop asked of either is done: this
    /// daemon ends.
    Done,
    /// The successor did not take over: this daemon's record is written
    /// again, and it serves on.
    Failed,
}

/// What the wait does next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Next {
    Wait,
    /// Send the signal asked of this daemon on to the successor.
    Forward,
    Done,
    Failed,
    /// Stop the successor, the wait being over.
    Kill,
}

/// What the wait does next, from the facts alone: whether the successor's
/// record stands, whether it ended and with success, whether a signal was
/// asked of this daemon and sent on already, and whether the wait is over.
fn next(recorded: bool, ended: Option<bool>, asked: bool, forwarded: bool, over: bool) -> Next {
    if asked {
        return match (forwarded, ended) {
            (false, _) => Next::Forward,
            (true, Some(_)) => Next::Done,
            // Recorded, it stops by its own bounds; this daemon goes.
            (true, None) if over && recorded => Next::Done,
            (true, None) if over => Next::Kill,
            (true, None) => Next::Wait,
        };
    }
    match (recorded, ended) {
        (true, _) | (false, Some(true)) => Next::Done,
        (false, Some(false)) => Next::Failed,
        (false, None) if over => Next::Kill,
        (false, None) => Next::Wait,
    }
}

/// Hands the lock of `instance`, the daemon of `data` recorded as `record`,
/// to the daemon of `to`, waiting up to `wait` for its record.
pub fn hand_over(
    data: &Path,
    instance: &Instance,
    record: &DaemonRecord,
    to: &To<'_>,
    wait: Duration,
    out: &mut dyn Write,
) -> Handed {
    let path = to.path.display();
    let serves_on = |out: &mut dyn Write, why: &str| {
        say(
            out,
            &format!(
                "{why}: this daemon serves on, on its own binary, and tries {path} again once it changes."
            ),
        );
        if let Err(error) = instance.republish(record) {
            say(
                out,
                &format!("warning: this daemon's record could not be written again: {error}"),
            );
        }
        Handed::Failed
    };
    if let Err(error) = instance.unpublish() {
        return serves_on(out, &format!("Its record could not be removed ({error})"));
    }
    let mut command = Command::new(to.path);
    command.arg("daemon").stdin(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let spawned = process::hand_over_file(&mut command, instance.lock()).and_then(|fd| {
        command.arg("--lock-fd").arg(fd.to_string());
        command.spawn()
    });
    drop(command);
    let mut child = match spawned {
        Ok(child) => child,
        Err(error) => {
            return serves_on(
                out,
                &format!("The daemon of {path} could not be started ({error})"),
            );
        }
    };
    let pid = child.id();
    say(
        out,
        &format!(
            "Handing the lock to the daemon of {path}, version {} (pid {pid}): this daemon ends \
             once it has written its record.",
            to.shown
        ),
    );
    let start = Instant::now();
    let mut forwarded = false;
    let mut ended = None;
    loop {
        let recorded = instance::read(data).is_some_and(|now| now.pid == pid);
        if ended.is_none() {
            ended = child.try_wait().ok().flatten();
        }
        let asked = process::asked().is_some();
        let over = start.elapsed() >= wait;
        match next(
            recorded,
            ended.map(|status| status.success()),
            asked,
            forwarded,
            over,
        ) {
            Next::Wait => thread::sleep(POLL),
            Next::Forward => {
                forwarded = true;
                let sent = owlshift_platform::service::terminate(pid);
                say(
                    out,
                    &format!(
                        "A stop was asked of this daemon while it handed its lock over: {}; it \
                         ends once the new daemon (pid {pid}) has.",
                        match sent {
                            Ok(()) => "sent on to the new daemon".to_owned(),
                            Err(error) => format!("not sent on to the new daemon ({error})"),
                        }
                    ),
                );
            }
            Next::Done if asked => {
                say(
                    out,
                    "The new daemon ended or stops: this daemon ends, as asked.",
                );
                return Handed::Done;
            }
            Next::Done if recorded => {
                say(
                    out,
                    &format!(
                        "The daemon of {path} (pid {pid}) took over: this daemon ends, its lock \
                         held by the new one."
                    ),
                );
                return Handed::Done;
            }
            Next::Done => {
                say(
                    out,
                    &format!(
                        "The daemon of {path} (pid {pid}) found a stop asked at its start and \
                         ended: this daemon ends too."
                    ),
                );
                return Handed::Done;
            }
            Next::Failed => {
                let status = ended.map_or_else(String::new, |status| status.to_string());
                return serves_on(
                    out,
                    &format!(
                        "The daemon of {path} (pid {pid}) ended at its start ({status}) without \
                         taking over"
                    ),
                );
            }
            Next::Kill => {
                let _ = child.kill();
                let _ = child.wait();
                if asked {
                    say(
                        out,
                        &format!(
                            "The new daemon (pid {pid}) was stopped {} seconds after the stop: \
                             this daemon ends, as asked.",
                            wait.as_secs()
                        ),
                    );
                    return Handed::Done;
                }
                return serves_on(
                    out,
                    &format!(
                        "The daemon of {path} (pid {pid}) had not written its record {} seconds \
                         later and was stopped",
                        wait.as_secs()
                    ),
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use jiff::Timestamp;
    use owlshift_contracts::format::Format;

    use super::*;

    /// Every outcome of the wait, from the facts alone.
    #[test]
    fn the_wait_ends_by_a_record_an_end_a_signal_or_its_bound() {
        use Next::*;
        for (recorded, ended, asked, forwarded, over, then) in [
            (false, None, false, false, false, Wait),
            (true, None, false, false, false, Done),
            (false, Some(true), false, false, false, Done),
            (false, Some(false), false, false, false, Failed),
            (false, None, false, false, true, Kill),
            (true, None, true, false, false, Forward),
            (false, None, true, true, false, Wait),
            (false, Some(false), true, true, false, Done),
            (true, None, true, true, true, Done),
            (false, None, true, true, true, Kill),
        ] {
            assert_eq!(
                next(recorded, ended, asked, forwarded, over),
                then,
                "{recorded} {ended:?} {asked} {forwarded} {over}"
            );
        }
    }

    /// A successor that cannot be started leaves the daemon serving, its
    /// record written again.
    #[test]
    fn a_successor_that_cannot_start_leaves_the_daemon_recorded() {
        let data = tempfile::tempdir().unwrap();
        let data = data.path();
        let record = DaemonRecord {
            format: Format,
            pid: std::process::id(),
            version: "0.2.0".to_owned(),
            binary: "/bin/owlshift".to_owned(),
            since: Timestamp::now(),
            build: None,
            watches: Some("/bin/owlshift".to_owned()),
        };
        let held = instance::take(data, &record).unwrap().unwrap();
        let gone = data.join("gone/owlshift");
        let to = To {
            path: &gone,
            shown: "0.3.0".to_owned(),
        };
        let mut out = Vec::new();
        let handed = hand_over(data, &held, &record, &to, HANDOVER_WAIT, &mut out);
        let said = String::from_utf8_lossy(&out);
        assert_eq!(handed, Handed::Failed, "{said}");
        assert!(said.contains("could not be started"), "{said}");
        assert_eq!(instance::read(data), Some(record));
    }
}
