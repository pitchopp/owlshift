//! `owlshift status` (OWL-438, architecture section 5, "`owlshift status`"):
//! what the background service serves and does, read from its files and
//! nothing else, with no channel to the daemon.
//!
//! It reads `<data>/service/daemon.json`, `projects.json`, the stop request
//! and the pauses, each served project's `served.json`, stop file and
//! ticket refs, the run slots' holders, the usage files, `digest.json`, and
//! the daemon's `watch_started` and `watch_ended` events, from which a
//! watch is running or restarting and why. It waits on no lock: whether a
//! daemon runs is the instant probe of its lock (`daemon::running`), as
//! `owlshift start` and `stop` read it, and the slots are counted as
//! `owlshift why` counts them, each free one tried for a moment.
//!
//! [`gather`] does the reading; [`watch_state`] and [`render`] are pure.

use std::fmt::Write as _;
use std::path::Path;

use jiff::Timestamp;
use jiff::tz::TimeZone;
use owlshift_adapters::forge::Repo;
use owlshift_contracts::event::{Event, EventKind};
use owlshift_contracts::ids::TicketId;
use owlshift_contracts::service::{BY_DAEMON, DaemonRecord, DigestRecord, Request, ServedConfig};
use owlshift_core::state::Status;
use owlshift_core::vocab::Harness;
use serde_json::Value;

use crate::daemon::digest::{self, HarnessUsage};
use crate::daemon::{self, requests};
use crate::events::{EventLog, printable};
use crate::project::ProjectDirs;
use crate::served::{self, Registry};
use crate::slots::{HeldSlot, MachineSlots};
use crate::{watch, why};

/// Everything `owlshift status` shows.
#[derive(Clone, Debug)]
pub struct Report {
    pub daemon: Daemon,
    /// The daemon's stop request, while it stands: the daemon drains.
    pub stop_asked: Option<Request>,
    /// The pause of every project, while it stands; `None` within for a
    /// file that does not read.
    pub machine_pause: Option<Option<Request>>,
    pub projects: Vec<Project>,
    /// The machine's run slots held now, with their holders, and
    /// `concurrent_runs`; `None` when they cannot be probed.
    pub slots: Option<(Vec<HeldSlot>, u32)>,
    pub usage: Vec<(Harness, HarnessUsage)>,
    /// The latest digest's record.
    pub digest: Option<DigestRecord>,
    /// What could not be read.
    pub notes: Vec<String>,
}

/// Whether a daemon runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Daemon {
    NotRunning,
    /// One holds the lock: its record, `None` when it does not read.
    Running(Option<DaemonRecord>),
    /// Its lock could not be probed, for this reason.
    Unknown(String),
}

/// One served project.
#[derive(Clone, Debug)]
pub struct Project {
    /// Its repository, as `projects.json` lists it.
    pub repository: String,
    /// The checkout it was registered from.
    pub checkout: String,
    /// The project file it runs on, or why none is: its `served.json`, an
    /// empty one while its watch has read nothing; why it cannot be read.
    pub config: Result<ServedConfig, String>,
    pub watch: Watch,
    /// Its own pause, while it stands; `None` within for a file that does
    /// not read.
    pub pause: Option<Option<Request>>,
    /// Whether its stop file stands: its watch drains.
    pub draining: bool,
    /// The tickets whose questions wait for a person, and the parked ones.
    pub waiting: Vec<TicketId>,
    pub parked: Vec<TicketId>,
}

/// A served project's watch, as the daemon's events tell it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Watch {
    /// No daemon runs, so no watch is served.
    NoDaemon,
    /// The daemon running now has not started it yet.
    NotStarted,
    /// Started at `since`, process `pid`.
    Running { pid: Option<u64>, since: Timestamp },
    /// It ended without being asked at `ended`, and starts again at `at`.
    Restarting {
        ended: Timestamp,
        at: Option<Timestamp>,
        why: String,
        in_a_row: Option<u64>,
    },
    /// It ended at `at` on a refusal no restart mends: the daemon paused
    /// the project, and tries it again at each of its passes.
    Unservable { at: Timestamp, why: String },
    /// It ended as asked, at `at`.
    Ended { at: Timestamp },
}

/// The watch of `project` the events say, oldest first, since `since`, the
/// start of the daemon running now: the latest `watch_started` or
/// `watch_ended` decision of the daemon's on that project.
pub fn watch_state(events: &[Event], project: &str, since: Timestamp) -> Watch {
    let latest = events.iter().rev().find(|event| {
        event.kind == EventKind::Decision
            && event.at >= since
            && event.project.eq_ignore_ascii_case(project)
            && matches!(
                event.data.get("service").and_then(Value::as_str),
                Some("watch_started" | "watch_ended")
            )
    });
    let Some(event) = latest else {
        return Watch::NotStarted;
    };
    let text = |key: &str| {
        event
            .data
            .get(key)
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
    };
    let number = |key: &str| event.data.get(key).and_then(Value::as_u64);
    if event.data.get("service").and_then(Value::as_str) == Some("watch_started") {
        return Watch::Running {
            pid: number("pid"),
            since: event.at,
        };
    }
    if let Some(why) = text("unservable") {
        return Watch::Unservable { at: event.at, why };
    }
    if event.data.get("asked").and_then(Value::as_bool) == Some(true) {
        return Watch::Ended { at: event.at };
    }
    let at = number("restart_in_ms")
        .and_then(|ms| i64::try_from(ms).ok())
        .and_then(|ms| {
            event
                .at
                .checked_add(jiff::SignedDuration::from_millis(ms))
                .ok()
        });
    Watch::Restarting {
        ended: event.at,
        at,
        why: text("end").unwrap_or_else(|| "it ended".to_owned()),
        in_a_row: number("in_a_row"),
    }
}

/// Reads what `owlshift status` shows of the data directory `data`, its
/// slots counted up to `concurrent_runs`, its harnesses' caps `caps`, at
/// `now`.
pub fn gather(data: &Path, concurrent_runs: u32, caps: &[(Harness, u8)], now: Timestamp) -> Report {
    let mut notes = Vec::new();
    let daemon = match daemon::running(data) {
        Ok(None) => Daemon::NotRunning,
        Ok(Some(record)) => Daemon::Running(record),
        Err(error) => Daemon::Unknown(error.to_string()),
    };
    let events = match why::read_events(EventLog::in_dir(data).path()) {
        Ok((events, bad)) => {
            notes.extend(bad.into_iter().map(|why| format!("the event log's {why}")));
            events
        }
        Err(error) => {
            notes.push(format!("the event log cannot be read: {error}"));
            Vec::new()
        }
    };
    let listed = match Registry::in_data_dir(data).read() {
        Ok(served) => served.projects,
        Err(error) => {
            notes.push(format!("the served projects cannot be read: {error}"));
            Vec::new()
        }
    };
    let mut projects = Vec::new();
    for entry in listed {
        let repo = match Repo::parse(&entry.repository) {
            Ok(repo) => repo,
            Err(error) => {
                notes.push(format!("{} is not a repository: {error}", entry.repository));
                continue;
            }
        };
        let dirs = ProjectDirs::new(data, &repo);
        let config = served::read_record(&dirs)
            .map(|record| record.unwrap_or_else(|| ServedConfig::new(repo.to_string())));
        let watch = match &daemon {
            Daemon::Running(record) => {
                // A record that does not read leaves the daemon's start
                // unknown: every event counts.
                let since = record
                    .as_ref()
                    .map_or(Timestamp::MIN, |record| record.since);
                watch_state(&events, &entry.repository, since)
            }
            _ => Watch::NoDaemon,
        };
        let (mut waiting, mut parked) = (Vec::new(), Vec::new());
        for (ticket, status) in why::statuses(&dirs, &mut notes) {
            match status {
                Status::NeedsInput { .. } => waiting.push(ticket),
                Status::Parked { .. } => parked.push(ticket),
                Status::Active(_) => {}
            }
        }
        projects.push(Project {
            repository: entry.repository,
            checkout: entry.checkout,
            config,
            watch,
            pause: requests::pause(&dirs),
            draining: watch::stop::stands(&dirs),
            waiting,
            parked,
        });
    }
    let slots = match MachineSlots::new(data, concurrent_runs).holders() {
        Ok(held) => Some((held, concurrent_runs)),
        Err(error) => {
            notes.push(format!("the run slots cannot be probed: {error}"));
            None
        }
    };
    let usage = digest::usage(data, caps, now, &mut notes);
    let digest = match digest::read(data) {
        Ok(record) => record,
        Err(digest::Unread::Newer) => {
            notes.push("the digest's record is of a newer Owlshift's format".to_owned());
            None
        }
        Err(digest::Unread::Bad(why)) => {
            notes.push(format!("the digest's record does not read: {why}"));
            None
        }
    };
    Report {
        daemon,
        stop_asked: requests::stop_asked(data),
        machine_pause: requests::machine_pause(data),
        projects,
        slots,
        usage,
        digest,
        notes,
    }
}

/// `at` in `tz`, to the minute.
fn time(at: Timestamp, tz: &TimeZone) -> String {
    at.to_zoned(tz.clone())
        .strftime("%Y-%m-%d %H:%M")
        .to_string()
}

/// Who asked `request`, when and why; a file that does not read says so.
fn asked(request: Option<&Request>, tz: &TimeZone) -> String {
    request.map_or_else(
        || "its file does not say who asked".to_owned(),
        |request| {
            format!(
                "by {} at {}: {}",
                request.by,
                time(request.at, tz),
                request.why
            )
        },
    )
}

fn tickets(tickets: &[TicketId]) -> String {
    tickets
        .iter()
        .map(TicketId::as_str)
        .collect::<Vec<_>>()
        .join(", ")
}

/// The report as text, its times in `tz`, safe for a terminal.
pub fn render(report: &Report, tz: &TimeZone) -> String {
    let mut out = String::new();
    let mut line = |text: String| {
        let _ = writeln!(out, "{}", printable(&text));
    };
    line(match &report.daemon {
        Daemon::NotRunning => {
            "Daemon: not running, so no project is served (`owlshift start` starts it).".to_owned()
        }
        Daemon::Running(Some(record)) => format!(
            "Daemon: running since {} (pid {}, version {}).",
            time(record.since, tz),
            record.pid,
            record.version
        ),
        Daemon::Running(None) => "Daemon: running (its record does not read).".to_owned(),
        Daemon::Unknown(why) => format!("Daemon: unknown, its lock cannot be probed: {why}."),
    });
    if let Some(request) = &report.stop_asked {
        line(format!(
            "  Stopping, asked {}: each watch ends once its steps under way finish.",
            asked(Some(request), tz)
        ));
    }
    if let Some(request) = &report.machine_pause {
        line(format!(
            "Paused: every project of this machine, {}. Nothing is dispatched and no step \
             starts; `owlshift resume` lifts it.",
            asked(request.as_ref(), tz)
        ));
    }
    if report.projects.is_empty() {
        line("No project is served (`owlshift start`, in a checkout, serves it).".to_owned());
    }
    for project in &report.projects {
        line(format!(
            "{} (from {})",
            project.repository, project.checkout
        ));
        match &project.config {
            Ok(record) => {
                if let Some(kept) = &record.kept {
                    line(format!("  runs on {}", served::kept_name(kept)));
                    if let Some(refused) = &record.refused {
                        line(format!(
                            "  a newer project file is refused since {}: {}",
                            time(refused.since, tz),
                            refused.reason
                        ));
                    }
                } else {
                    let reason = record.refused.as_ref().map_or(
                        "no project file was read from its default branch yet",
                        |refused| refused.reason.as_str(),
                    );
                    line(format!("  not served: {reason}"));
                }
            }
            Err(why) => line(format!("  its served record cannot be read: {why}")),
        }
        line(format!(
            "  watch: {}",
            match &project.watch {
                Watch::NoDaemon => "none, no daemon runs".to_owned(),
                Watch::NotStarted => "not started yet".to_owned(),
                Watch::Running { pid, since } => format!(
                    "running since {}{}",
                    time(*since, tz),
                    pid.map_or_else(String::new, |pid| format!(" (pid {pid})"))
                ),
                Watch::Restarting {
                    ended,
                    at,
                    why,
                    in_a_row,
                } => {
                    let when =
                        at.map_or_else(|| "later".to_owned(), |at| format!("at {}", time(at, tz)));
                    let row = match in_a_row {
                        Some(n) if *n > 1 => format!(", {n} ends in a row"),
                        _ => String::new(),
                    };
                    format!(
                        "restarting {when}: it ended at {} ({why}){row}",
                        time(*ended, tz)
                    )
                }
                Watch::Unservable { at, why } => {
                    format!("ended at {} and cannot start: {why}", time(*at, tz))
                }
                Watch::Ended { at } => format!("ended as asked at {}", time(*at, tz)),
            }
        ));
        if let Some(request) = &project.pause {
            let more = match request {
                Some(request) if request.by == BY_DAEMON => {
                    "; the daemon tries it again at each of its passes"
                }
                _ => "; `owlshift resume <owner/repo>` lifts it",
            };
            line(format!("  paused {}{more}", asked(request.as_ref(), tz)));
        }
        if project.draining {
            line("  draining: its watch ends once its steps under way finish".to_owned());
        }
        if !project.waiting.is_empty() {
            line(format!(
                "  waiting for a person: {}",
                tickets(&project.waiting)
            ));
        }
        if !project.parked.is_empty() {
            line(format!("  parked: {}", tickets(&project.parked)));
        }
    }
    match &report.slots {
        Some((held, count)) if held.is_empty() => {
            line(format!("Steps under way: none (0 of {count} run slots)."));
        }
        Some((held, count)) => {
            line(format!(
                "Steps under way: {} of {count} run slots.",
                held.len()
            ));
            for slot in held {
                line(match &slot.holder {
                    Some(holder) => format!(
                        "  {} {} (pid {}, since {})",
                        holder.project,
                        holder.ticket,
                        holder.pid,
                        time(holder.since, tz)
                    ),
                    None => format!("  slot {}: its holder is not recorded", slot.slot),
                });
            }
        }
        None => {}
    }
    if report.usage.is_empty() {
        line("Usage: nothing under way recorded.".to_owned());
    }
    for (harness, usage) in &report.usage {
        let name = crate::usage::key(*harness);
        line(match usage {
            HarnessUsage::Limited(Some(until)) => {
                format!("Usage: {name} at its limit until {}.", time(*until, tz))
            }
            HarnessUsage::Limited(None) => {
                format!("Usage: {name} at its limit, until a probe shows it reset.")
            }
            HarnessUsage::Share { share, resets_at } => format!(
                "Usage: {name} {:.0} % of a window that resets at {}.",
                share * 100.0,
                time(*resets_at, tz)
            ),
        });
    }
    match &report.digest {
        Some(DigestRecord {
            at,
            line: Some(text),
            ..
        }) => line(format!("Latest digest, {}: {text}", time(*at, tz))),
        _ => line("Latest digest: none yet.".to_owned()),
    }
    for note in &report.notes {
        line(format!("warning: {note}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use std::io;

    use owlshift_contracts::format::Format;
    use owlshift_contracts::service::{BY_PAUSE, KeptFile, ServedProject, ServedProjects};
    use serde_json::json;

    use super::*;
    use crate::events::{EventSink, data};

    fn record(since: Timestamp) -> DaemonRecord {
        DaemonRecord {
            format: Format,
            pid: 4242,
            version: "0.4.0".to_owned(),
            binary: "/bin/owlshift".to_owned(),
            since,
        }
    }

    /// The daemon's own event of `repo`, now.
    fn service(data_dir: &Path, repo: &str, pairs: Vec<(&str, Value)>) {
        let mut nowhere = io::sink();
        let mut details = data([]);
        for (key, value) in pairs {
            details.insert(key.to_owned(), value);
        }
        EventSink::new(repo, EventLog::in_dir(data_dir), &mut nowhere).emit_project(
            None,
            EventKind::Decision,
            details,
        );
    }

    /// The acceptance: a running daemon serving two projects, one whose
    /// watch ended and restarts, saying why, the other paused by a person,
    /// with the reason, and the machine's pause.
    #[test]
    fn status_shows_a_restarting_watch_and_a_paused_project_with_its_reason() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path();
        let since = Timestamp::now()
            .checked_sub(jiff::SignedDuration::from_mins(5))
            .unwrap();
        let _daemon = daemon::instance::take(data_dir, &record(since))
            .unwrap()
            .unwrap();
        let served = ServedProjects {
            format: Format,
            projects: ["acme/web", "acme/api"]
                .into_iter()
                .map(|repository| ServedProject {
                    repository: repository.to_owned(),
                    checkout: format!("/home/ada/{repository}"),
                })
                .collect(),
        };
        crate::step::write_atomically(&Registry::in_data_dir(data_dir).file(), &served.render())
            .unwrap();
        let web = ProjectDirs::new(data_dir, &Repo::parse("acme/web").unwrap());
        let mut kept = ServedConfig::new("acme/web");
        kept.kept = Some(KeptFile {
            branch: "main".to_owned(),
            commit: "a".repeat(40),
            text: String::new(),
            since,
        });
        crate::step::write_atomically(&served::record_file(&web), &kept.render()).unwrap();
        service(
            data_dir,
            "acme/web",
            vec![("service", json!("watch_started")), ("pid", json!(77))],
        );
        service(
            data_dir,
            "acme/web",
            vec![
                ("service", json!("watch_ended")),
                (
                    "end",
                    json!("exit status: 1: owlshift: the tracker did not answer"),
                ),
                ("asked", json!(false)),
                ("restart_in_ms", json!(20_000)),
                ("in_a_row", json!(2)),
            ],
        );
        service(
            data_dir,
            "acme/api",
            vec![("service", json!("watch_started")), ("pid", json!(78))],
        );
        let api = ProjectDirs::new(data_dir, &Repo::parse("acme/api").unwrap());
        let lunch = Request::new(BY_PAUSE, since, "lunch break");
        requests::ask_pause(data_dir, Some(&api), &lunch).unwrap();
        let all = Request::new(BY_PAUSE, since, "travelling");
        requests::ask_pause(data_dir, None, &all).unwrap();

        let report = gather(data_dir, 2, &[], Timestamp::now());
        assert!(report.notes.is_empty(), "{:?}", report.notes);
        assert!(
            matches!(&report.projects[0].watch, Watch::Restarting { why, .. }
            if why.ends_with("the tracker did not answer"))
        );
        let text = render(&report, &TimeZone::UTC);
        let has = |part: &str| assert!(text.contains(part), "{part:?} missing in\n{text}");
        has("Daemon: running since");
        has("(pid 4242, version 0.4.0)");
        has("Paused: every project of this machine, by owlshift pause at");
        has(": travelling.");
        has("acme/web (from /home/ada/acme/web)");
        has("  runs on owlshift.toml at origin/main (aaaaaaaaaaaa)");
        has("  watch: restarting at ");
        has("(exit status: 1: owlshift: the tracker did not answer), 2 ends in a row");
        has("acme/api (from /home/ada/acme/api)");
        has("  not served: no project file was read from its default branch yet");
        has("  watch: running since ");
        has("(pid 78)");
        has("  paused by owlshift pause at ");
        has(": lunch break; `owlshift resume <owner/repo>` lifts it");
        has("Steps under way: none (0 of 2 run slots).");
        has("Latest digest: none yet.");
    }

    /// A watch the daemon paused for a refusal no restart mends is never
    /// read as running, nor any watch of a daemon that does not run, nor an
    /// event of an earlier daemon.
    #[test]
    fn a_watch_is_read_from_the_daemons_events_since_it_started() {
        let tmp = tempfile::tempdir().unwrap();
        let data_dir = tmp.path();
        service(
            data_dir,
            "acme/web",
            vec![("service", json!("watch_started")), ("pid", json!(1))],
        );
        let (events, _) = why::read_events(EventLog::in_dir(data_dir).path()).unwrap();
        let later = Timestamp::now()
            .checked_add(jiff::SignedDuration::from_mins(1))
            .unwrap();
        assert_eq!(watch_state(&events, "acme/web", later), Watch::NotStarted);
        assert!(matches!(
            watch_state(&events, "ACME/web", Timestamp::MIN),
            Watch::Running { pid: Some(1), .. }
        ));
        assert_eq!(
            watch_state(&events, "acme/api", Timestamp::MIN),
            Watch::NotStarted
        );
        service(
            data_dir,
            "acme/web",
            vec![
                ("service", json!("watch_ended")),
                ("end", json!("exit status: 78")),
                ("asked", json!(false)),
                ("unservable", json!("the keychain refused")),
            ],
        );
        service(
            data_dir,
            "acme/web",
            vec![("service", json!("pause_lifted"))],
        );
        let (events, _) = why::read_events(EventLog::in_dir(data_dir).path()).unwrap();
        assert!(matches!(
            watch_state(&events, "acme/web", Timestamp::MIN),
            Watch::Unservable { why, .. } if why == "the keychain refused"
        ));
        let report = gather(data_dir, 1, &[], Timestamp::now());
        assert_eq!(report.daemon, Daemon::NotRunning);
        assert!(report.projects.is_empty());
        let text = render(&report, &TimeZone::UTC);
        assert!(text.starts_with("Daemon: not running"), "{text}");
        assert!(text.contains("No project is served"), "{text}");
    }
}
