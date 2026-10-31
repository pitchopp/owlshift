//! A watch the daemon starts while the network is down (OWL-479,
//! architecture section 5, "The background service", "Offline at its
//! start"): opening its tracker and forge, it waits for an answer rather
//! than end, so a laptop that boots offline comes up quietly instead of
//! leaning on the daemon's restarts.
//!
//! Only a refusal that is no answer at all waits ([`Refusal::unanswered`]);
//! any other ends the watch as before. The wait is told once, and its end
//! once, each a line and a project `decision` event; a stop asked meanwhile,
//! by a signal or the project's stop file, ends it at once.

use std::io::Write;
use std::time::{Duration, Instant};

use serde_json::json;

use owlshift_contracts::event::EventKind;
use owlshift_platform::process::{self, Degree};

use super::stop::{self, Asked};
use crate::events::{EventSink, data, printable};
use crate::project::ProjectDirs;
use crate::refusal::Refusal;

/// How often a waiting watch looks at its stop file; a signal wakes it at
/// once.
const LOOK: Duration = Duration::from_millis(500);

/// The delays between two tries: the first, doubling up to the longest,
/// which bounds them. The wait itself has no end.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Backoff {
    pub first: Duration,
    pub longest: Duration,
}

impl Backoff {
    /// The service's: 5 seconds doubling up to a minute.
    pub const SERVICE: Self = Self {
        first: Duration::from_secs(5),
        longest: Duration::from_secs(60),
    };

    /// The delay after the `failed`-th try that had no answer, from 1.
    pub fn delay(self, failed: u32) -> Duration {
        2u32.checked_pow(failed.saturating_sub(1))
            .and_then(|factor| self.first.checked_mul(factor))
            .map_or(self.longest, |delay| delay.min(self.longest))
    }
}

/// How the opening ended.
#[derive(Debug)]
pub enum Opened<T> {
    /// It opened.
    Open(T),
    /// A stop was asked while it waited: the watch starts nothing and ends.
    Stopped,
}

/// Opens with `open` for the watch of `dirs`, waiting for the network while
/// it gets no answer at all: tries again after each delay of `backoff`, and
/// returns what it opened, [`Opened::Stopped`] once a stop is asked
/// meanwhile, or the first refusal that is not unanswered. The wait is told
/// on `out` and `sink` once, and its end once.
pub fn open<T>(
    dirs: &ProjectDirs,
    backoff: Backoff,
    sink: &mut EventSink<'_>,
    out: &mut dyn Write,
    open: &mut dyn FnMut() -> Result<T, Refusal>,
) -> Result<Opened<T>, Refusal> {
    let mut failed = 0u32;
    loop {
        let refusal = match open() {
            Ok(opened) => {
                if failed > 0 {
                    told_back(sink, out);
                }
                return Ok(Opened::Open(opened));
            }
            Err(refusal) if refusal.unanswered() => refusal,
            Err(refusal) => return Err(refusal),
        };
        failed = failed.saturating_add(1);
        if failed == 1 {
            told_waiting(sink, out, &refusal, backoff);
        }
        if let Some(asked) = wait(dirs, backoff.delay(failed)) {
            told_stopped(dirs, sink, out, asked);
            return Ok(Opened::Stopped);
        }
    }
}

/// Waits `delay`, or less once a stop is asked: what asked it.
fn wait(dirs: &ProjectDirs, delay: Duration) -> Option<Asked> {
    let until = Instant::now() + delay;
    loop {
        if let Some(asked) = stop::asked(dirs) {
            return Some(asked);
        }
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return None;
        }
        process::wait_asked(left.min(LOOK));
    }
}

fn told_waiting(
    sink: &mut EventSink<'_>,
    out: &mut dyn Write,
    refusal: &Refusal,
    backoff: Backoff,
) {
    let _ = writeln!(
        out,
        "{}",
        printable(&format!(
            "{} waits for the network: {refusal}. It tries again, at most {} seconds apart, \
             and starts once its tracker and forge answer.",
            sink.project(),
            backoff.longest.as_secs()
        ))
    );
    sink.emit_project(
        None,
        EventKind::Decision,
        data([
            ("watch", json!("waits_for_network")),
            ("reason", json!(refusal.message)),
        ]),
    );
}

fn told_back(sink: &mut EventSink<'_>, out: &mut dyn Write) {
    let _ = writeln!(
        out,
        "{}",
        printable(&format!(
            "{} reached its tracker and forge: watch starts.",
            sink.project()
        ))
    );
    sink.emit_project(
        None,
        EventKind::Decision,
        data([("watch", json!("network_back"))]),
    );
}

/// Says that `asked` ended the wait: a line and the project `decision`
/// event any stop is told with; a stop file is removed, as a watch that
/// finds one at its start removes it.
fn told_stopped(dirs: &ProjectDirs, sink: &mut EventSink<'_>, out: &mut dyn Write, asked: Asked) {
    let project = sink.project().to_owned();
    let (stop, by, line) = match asked {
        Asked::File => {
            if let Err(error) = stop::remove(dirs) {
                let _ = writeln!(
                    out,
                    "{}",
                    printable(&format!(
                        "owlshift: warning: the stop file {} could not be removed: {error}",
                        stop::file(dirs).display()
                    ))
                );
            }
            (
                "drain",
                "stop_file",
                format!(
                    "Draining {project}: its stop file asks it while it waits for the network; \
                     nothing started, so watch ends now."
                ),
            )
        }
        Asked::Signal(Degree::Drain) => (
            "drain",
            "signal",
            format!("Draining {project}: nothing started, so watch ends now."),
        ),
        Asked::Signal(Degree::Checked) => (
            "checked",
            "signal",
            format!("Stopping {project}: nothing started, so watch ends now."),
        ),
    };
    let _ = writeln!(out, "{}", printable(&line));
    sink.emit_project(
        None,
        EventKind::Decision,
        data([
            ("watch", json!("stop")),
            ("stop", json!(stop)),
            ("asked", json!(by)),
        ]),
    );
}

#[cfg(test)]
mod tests {
    use std::fs;

    use owlshift_adapters::forge::Repo;
    use owlshift_contracts::event::Event;

    use super::*;
    use crate::events::EventLog;

    const SHORT: Backoff = Backoff {
        first: Duration::from_millis(1),
        longest: Duration::from_millis(4),
    };

    fn dirs(root: &std::path::Path) -> ProjectDirs {
        let dirs = ProjectDirs::new(root, &Repo::parse("acme/web").unwrap());
        fs::create_dir_all(dirs.root()).unwrap();
        dirs
    }

    /// The decision events of the log in `data`, as their `watch` value.
    fn decisions(data: &std::path::Path) -> Vec<String> {
        fs::read_to_string(EventLog::in_dir(data).path())
            .unwrap_or_default()
            .lines()
            .map(|line| Event::parse(line).unwrap())
            .filter(|event| event.kind == EventKind::Decision)
            .map(|event| event.data["watch"].as_str().unwrap().to_owned())
            .collect()
    }

    #[test]
    fn the_delays_double_from_the_first_up_to_the_longest() {
        let delays: Vec<u64> = (1..=6)
            .map(|n| Backoff::SERVICE.delay(n).as_secs())
            .collect();
        assert_eq!(delays, [5, 10, 20, 40, 60, 60]);
        assert_eq!(Backoff::SERVICE.delay(u32::MAX), Duration::from_secs(60));
    }

    /// No answer twice, then an answer: told once that it waits, once that
    /// it starts, and what opened is returned.
    #[test]
    fn no_answer_waits_told_once_then_opens() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = dirs(tmp.path());
        let mut printed = Vec::new();
        let mut out = Vec::new();
        let mut sink = EventSink::new("acme/web", EventLog::in_dir(tmp.path()), &mut printed);
        let mut tries = 0;
        let opened = open(&dirs, SHORT, &mut sink, &mut out, &mut || {
            tries += 1;
            if tries < 3 {
                Err(Refusal::unreachable("Linear's token endpoint: io: x"))
            } else {
                Ok(tries)
            }
        })
        .unwrap();
        assert!(matches!(opened, Opened::Open(3)), "{opened:?}");
        let out = String::from_utf8(out).unwrap();
        assert_eq!(out.matches("waits for the network").count(), 1, "{out}");
        assert!(
            out.contains("acme/web waits for the network: Linear's token endpoint: io: x."),
            "{out}"
        );
        assert_eq!(out.matches("watch starts").count(), 1, "{out}");
        assert_eq!(decisions(tmp.path()), ["waits_for_network", "network_back"]);
    }

    /// An answer at once tells nothing; a refusal that is an answer ends
    /// the opening at once, whatever mends it.
    #[test]
    fn an_answer_at_once_or_a_refusal_never_waits() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = dirs(tmp.path());
        let mut printed = Vec::new();
        let mut out = Vec::new();
        let mut sink = EventSink::new("acme/web", EventLog::in_dir(tmp.path()), &mut printed);
        let opened = open(&dirs, SHORT, &mut sink, &mut out, &mut || Ok(())).unwrap();
        assert!(matches!(opened, Opened::Open(())));
        for refusal in [
            Refusal::from("invalid_client"),
            Refusal::passing("HTTP 503"),
        ] {
            let mut tries = 0;
            let error = open::<()>(&dirs, SHORT, &mut sink, &mut out, &mut || {
                tries += 1;
                Err(refusal.clone())
            })
            .unwrap_err();
            assert_eq!(error, refusal);
            assert_eq!(tries, 1);
        }
        assert!(out.is_empty());
        assert!(decisions(tmp.path()).is_empty());
    }

    /// A stop file found while it waits ends the wait, told as a drain,
    /// and is removed.
    #[test]
    fn a_stop_file_ends_the_wait_and_is_removed() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = dirs(tmp.path());
        let mut printed = Vec::new();
        let mut out = Vec::new();
        let mut sink = EventSink::new("acme/web", EventLog::in_dir(tmp.path()), &mut printed);
        let mut tries = 0;
        let opened = open::<()>(&dirs, SHORT, &mut sink, &mut out, &mut || {
            tries += 1;
            if tries == 2 {
                fs::write(stop::file(&dirs), "{}").unwrap();
            }
            Err(Refusal::unreachable("GitHub: connection refused"))
        })
        .unwrap();
        assert!(matches!(opened, Opened::Stopped));
        assert_eq!(tries, 2);
        assert!(!stop::stands(&dirs));
        let out = String::from_utf8(out).unwrap();
        assert!(
            out.contains("Draining acme/web: its stop file asks it"),
            "{out}"
        );
        assert_eq!(decisions(tmp.path()), ["waits_for_network", "stop"]);
    }
}
