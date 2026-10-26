//! The daily digest (OWL-440, architecture section 5, "The digest"): once a
//! day, at `[service] digest_hour` of the personal file, local time, one
//! notification by the daemon's notifier saying what was delivered, what
//! waits for a person, what is parked, which projects are paused and how
//! the harnesses' usage stands, ending with `owlshift status`.
//!
//! `<data>/service/digest.json` ([`DigestRecord`]) keeps when the latest
//! digest was made. A digest is due once a digest hour has passed since
//! then ([`due`]), so a machine asleep at the hour, or a daemon not running,
//! sends it once at the next pass, and never twice: the record is written
//! before the notification is shown, and kept in memory too, so a record
//! that cannot be written does not make the same daemon send it again.
//! With no record yet, the daemon writes one at its first pass and sends
//! nothing: the first digest comes at the first digest hour after it.
//!
//! [`due`] and [`line()`] are pure; [`gather`] reads the event log, the
//! served projects' ticket refs, the pause files and the usage files.

use std::collections::BTreeSet;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use jiff::tz::TimeZone;
use jiff::{Timestamp, Zoned};
use owlshift_adapters::forge::Repo;
use owlshift_adapters::notifier::Notifier;
use owlshift_contracts::config::{DEFAULT_DIGEST_HOUR, PersonalConfig};
use owlshift_contracts::event::EventKind;
use owlshift_contracts::format::{ContractError, Format};
use owlshift_contracts::ids::TicketId;
use owlshift_contracts::service::{BY_DAEMON, DigestRecord};
use owlshift_core::state::Status;
use owlshift_core::vocab::Harness;
use serde_json::{Value, json};

use super::instance::service_dir;
use super::requests;
use crate::config::FileState;
use crate::events::{EventLog, EventSink, data as event_data, printable};
use crate::project::ProjectDirs;
use crate::usage::{self as machine_usage, UsageStore};
use crate::why::{self, Limit};
use crate::{notify, step};

/// The most tickets a digest names in one list; the rest are counted.
const NAMED: usize = 5;

/// The digest's record in `data`.
pub fn record_file(data: &Path) -> PathBuf {
    service_dir(data).join("digest.json")
}

/// What the personal file sets for the digest, read at the daemon's start.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    /// The hour it is sent, 0 to 23, local time.
    pub hour: u8,
    /// The harnesses' usage caps, which its usage applies.
    pub caps: Vec<(Harness, u8)>,
}

impl Settings {
    /// The digest's settings, `None` when `[service] digest = false`. A
    /// machine without a personal file sends it at 8.
    pub fn of(personal: &FileState<PersonalConfig>) -> Option<Self> {
        let hour = match personal {
            FileState::Loaded { config, .. } => config.service.digest_hour()?,
            _ => DEFAULT_DIGEST_HOUR,
        };
        Some(Self {
            hour,
            caps: machine_usage::caps_of(personal),
        })
    }
}

/// The latest time at or before `now` the clock of `now`'s time zone read
/// `hour`:00. A day whose `hour`:00 falls in a gap of daylight saving time
/// takes the first time after the gap.
pub fn latest_hour(now: &Zoned, hour: u8) -> Timestamp {
    let hour = i8::try_from(hour.min(23)).unwrap_or(0);
    let at = |date: jiff::civil::Date| {
        date.at(hour, 0, 0, 0)
            .to_zoned(now.time_zone().clone())
            .map(|zoned| zoned.timestamp())
    };
    match at(now.date()) {
        Ok(today) if today <= now.timestamp() => today,
        _ => now
            .date()
            .yesterday()
            .ok()
            .and_then(|yesterday| at(yesterday).ok())
            .unwrap_or(Timestamp::MIN),
    }
}

/// Whether a digest is due at `now` when the latest was made `at`: a digest
/// hour passed since.
pub fn due(at: Timestamp, now: &Zoned, hour: u8) -> bool {
    latest_hour(now, hour) > at
}

/// A harness's usage, as the digest tells it.
#[derive(Clone, Debug, PartialEq)]
pub enum Usage {
    /// At its usage limit or cap, until then when known.
    Limited(Option<Timestamp>),
    /// Free, the highest share used of a window that has not reset, from 0
    /// to 1.
    Share(f64),
}

/// What a digest tells.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Summary {
    /// The tickets whose pull request a delivery opened or pushed since the
    /// latest digest.
    pub delivered: BTreeSet<TicketId>,
    /// The tickets that wait for a person's answer.
    pub waiting: BTreeSet<TicketId>,
    pub parked: BTreeSet<TicketId>,
    /// Whether `<data>/service/pause` stands.
    pub all_paused: bool,
    /// The served projects whose pause file stands.
    pub paused: Vec<String>,
    pub usage: Vec<(Harness, Usage)>,
}

/// Reads what the digest tells of `projects`, the served ones: what was
/// delivered after `since`, from the event log, and what stands at `now`.
/// What cannot be read is said on `out` and left out.
pub fn gather(
    data: &Path,
    projects: &[(&Repo, &ProjectDirs)],
    caps: &[(Harness, u8)],
    since: Timestamp,
    now: Timestamp,
    out: &mut dyn Write,
) -> Summary {
    let mut summary = Summary::default();
    let mut notes = Vec::new();
    let served = |project: &str| {
        projects
            .iter()
            .any(|(repo, _)| repo.to_string().eq_ignore_ascii_case(project))
    };
    match why::read_events(EventLog::in_dir(data).path()) {
        Ok((events, bad)) => {
            // A delivery on a line that is not an event is not counted:
            // said, as `why` says it.
            notes.extend(bad.into_iter().map(|why| format!("the event log's {why}")));
            for event in events {
                let says = |key: &str, value: &str| {
                    event.data.get(key).and_then(Value::as_str) == Some(value)
                };
                if event.kind == EventKind::TrackerWrite
                    && event.at > since
                    && event.at <= now
                    && says("target", "forge")
                    && says("action", "pull_request")
                    && served(&event.project)
                    && let Some(ticket) = event.ticket
                {
                    summary.delivered.insert(ticket);
                }
            }
        }
        Err(error) => notes.push(format!("the event log cannot be read: {error}")),
    }
    for (repo, dirs) in projects {
        for (ticket, status) in why::statuses(dirs, &mut notes) {
            match status {
                Status::NeedsInput { .. } => {
                    summary.waiting.insert(ticket);
                }
                Status::Parked { .. } => {
                    summary.parked.insert(ticket);
                }
                Status::Active(_) => {}
            }
        }
        if requests::pause(dirs).is_some() {
            summary.paused.push(repo.to_string());
        }
    }
    summary.all_paused = requests::machine_pause(data).is_some();
    let store = UsageStore::in_data_dir(data).with_caps(caps.iter().copied());
    let now_ms = machine_usage::ms(now);
    for (harness, limit) in why::limits(data, caps, now) {
        let usage = match limit {
            Limit::Until(until) => Usage::Limited(Some(until)),
            Limit::ProbeDue => Usage::Limited(None),
            Limit::Unread(why) => {
                notes.push(format!("the usage of {harness:?} cannot be read: {why}"));
                continue;
            }
            Limit::Free => {
                let share = store.read_at(harness, now_ms).ok().and_then(|state| {
                    state
                        .windows
                        .values()
                        .filter(|window| window.resets_at_ms > now_ms)
                        .map(|window| window.share)
                        .reduce(f64::max)
                });
                match share {
                    Some(share) => Usage::Share(share),
                    None => continue,
                }
            }
        };
        summary.usage.push((harness, usage));
    }
    for note in notes {
        say(
            out,
            &format!("warning: the digest leaves out what it cannot read: {note}"),
        );
    }
    summary
}

/// The digest's text, its times in `tz`.
pub fn line(summary: &Summary, tz: &TimeZone) -> String {
    let mut parts = Vec::new();
    parts.push(if summary.delivered.is_empty() {
        "nothing delivered".to_owned()
    } else {
        format!(
            "{} delivered ({})",
            summary.delivered.len(),
            named(&summary.delivered)
        )
    });
    if !summary.waiting.is_empty() {
        parts.push(format!(
            "{} waiting for a person ({})",
            summary.waiting.len(),
            named(&summary.waiting)
        ));
    }
    if !summary.parked.is_empty() {
        parts.push(format!(
            "{} parked ({})",
            summary.parked.len(),
            named(&summary.parked)
        ));
    }
    if summary.all_paused {
        parts.push("every project paused".to_owned());
    } else if !summary.paused.is_empty() {
        parts.push(format!("paused: {}", summary.paused.join(", ")));
    }
    if !summary.usage.is_empty() {
        let usage: Vec<String> = summary
            .usage
            .iter()
            .map(|(harness, usage)| {
                let name = machine_usage::key(*harness);
                match usage {
                    Usage::Limited(Some(until)) => format!(
                        "{name} at its limit until {}",
                        until.to_zoned(tz.clone()).strftime("%a %H:%M")
                    ),
                    Usage::Limited(None) => format!("{name} at its limit"),
                    Usage::Share(share) => format!("{name} {:.0} %", share * 100.0),
                }
            })
            .collect();
        parts.push(format!("usage: {}", usage.join(", ")));
    }
    format!(
        "Owlshift digest: {}. More: `owlshift status`.",
        parts.join("; ")
    )
}

/// Up to [`NAMED`] tickets, then how many more.
fn named(tickets: &BTreeSet<TicketId>) -> String {
    let mut text = tickets
        .iter()
        .take(NAMED)
        .map(TicketId::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    if tickets.len() > NAMED {
        text.push_str(&format!(" and {} more", tickets.len() - NAMED));
    }
    text
}

/// What a look at the digest did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Checked {
    /// No digest was made before: the record now counts from this look.
    Counting,
    /// No digest hour passed since the latest.
    NotDue,
    /// The record is of a newer format, a newer Owlshift's: none is sent.
    Newer,
    /// A digest was made, with this text; shown when a notifier was given.
    Sent(String),
}

/// The daemon's memory of its digests across its passes.
#[derive(Debug, Default)]
pub struct Digests {
    /// When the latest digest was made, once read or written.
    last: Option<Timestamp>,
    /// Whether a record of a newer format was said.
    newer_told: bool,
}

impl Digests {
    /// Sends the digest of `projects` at `now` when it is due, through
    /// `notifier` when given, saying what it does on `out`.
    pub fn check(
        &mut self,
        data: &Path,
        settings: &Settings,
        projects: &[(&Repo, &ProjectDirs)],
        notifier: Option<&dyn Notifier>,
        now: &Zoned,
        out: &mut dyn Write,
    ) -> Checked {
        let last = match self.last {
            Some(last) => last,
            None => match read(data) {
                Ok(Some(record)) => *self.last.insert(record.at),
                Ok(None) => return self.count_from(data, settings, now, None, out),
                Err(Unread::Newer) => {
                    if !std::mem::replace(&mut self.newer_told, true) {
                        say(
                            out,
                            &format!(
                                "warning: {} is of a newer Owlshift's format: this daemon sends \
                                 no digest while it stands",
                                record_file(data).display()
                            ),
                        );
                    }
                    return Checked::Newer;
                }
                Err(Unread::Bad(why)) => {
                    return self.count_from(data, settings, now, Some(&why), out);
                }
            },
        };
        if !due(last, now, settings.hour) {
            return Checked::NotDue;
        }
        let summary = gather(data, projects, &settings.caps, last, now.timestamp(), out);
        let text = line(&summary, now.time_zone());
        // Kept in memory first: a record that cannot be written does not
        // make this daemon send it again.
        self.last = Some(now.timestamp());
        let record = DigestRecord {
            format: Format,
            at: now.timestamp(),
            line: Some(text.clone()),
        };
        if let Err(error) = step::write_atomically(&record_file(data), &record.render()) {
            say(
                out,
                &format!(
                    "warning: the digest's record {} could not be written: {error}",
                    record_file(data).display()
                ),
            );
        }
        let mut nowhere = io::sink();
        let mut sink = EventSink::new(BY_DAEMON, EventLog::in_dir(data), &mut nowhere);
        sink.emit_project(
            None,
            EventKind::Decision,
            event_data([("service", json!("digest")), ("line", json!(text))]),
        );
        let shown = notifier.is_some();
        if let Some(notifier) = notifier
            && let Err(error) = notifier.notify(&text)
        {
            sink.emit_project(None, EventKind::Warning, notify::not_shown(&error));
        }
        let how = if shown { "Sent" } else { "Made (no notifier)" };
        say(out, &format!("{how} the daily digest: {text}"));
        Checked::Sent(text)
    }

    /// Starts counting from `now`, with no digest: the record is absent, or
    /// does not read (`error`).
    fn count_from(
        &mut self,
        data: &Path,
        settings: &Settings,
        now: &Zoned,
        error: Option<&str>,
        out: &mut dyn Write,
    ) -> Checked {
        self.last = Some(now.timestamp());
        if let Some(error) = error {
            say(
                out,
                &format!(
                    "warning: {} does not read ({error}): replaced, counting from now",
                    record_file(data).display()
                ),
            );
        }
        let record = DigestRecord {
            format: Format,
            at: now.timestamp(),
            line: None,
        };
        match step::write_atomically(&record_file(data), &record.render()) {
            Ok(()) => say(
                out,
                &format!(
                    "The daily digest comes at {}:00, local time.",
                    settings.hour
                ),
            ),
            Err(error) => say(
                out,
                &format!(
                    "warning: the digest's record {} could not be written: {error}",
                    record_file(data).display()
                ),
            ),
        }
        Checked::Counting
    }
}

/// Why the digest's record was not read.
enum Unread {
    /// It is of a newer format.
    Newer,
    /// It cannot be read, or does not parse, for this reason.
    Bad(String),
}

/// The digest's record in `data`: `None` when there is none.
fn read(data: &Path) -> Result<Option<DigestRecord>, Unread> {
    match fs::read_to_string(record_file(data)) {
        Ok(text) => match DigestRecord::parse(&text) {
            Ok(record) => Ok(Some(record)),
            Err(ContractError::NewerFormat { .. }) => Err(Unread::Newer),
            Err(error) => Err(Unread::Bad(error.to_string())),
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(Unread::Bad(error.to_string())),
    }
}

/// Writes `line` on `out`, safe for a terminal, never failing.
fn say(out: &mut dyn Write, line: &str) {
    let _ = writeln!(out, "{}", printable(line));
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use owlshift_adapters::notifier::Error;
    use owlshift_contracts::event::Event;
    use owlshift_contracts::service::Request;
    use owlshift_core::usage::Window;

    use super::*;

    /// Paris's rule: CET, CEST from the last Sunday of March to the last of
    /// October.
    fn paris() -> TimeZone {
        TimeZone::posix("CET-1CEST,M3.5.0,M10.5.0/3").unwrap()
    }

    fn local(text: &str) -> Zoned {
        text.parse::<jiff::civil::DateTime>()
            .unwrap()
            .to_zoned(paris())
            .unwrap()
    }

    fn ts(text: &str) -> Timestamp {
        local(text).timestamp()
    }

    #[test]
    fn a_digest_is_due_once_its_hour_passed_since_the_latest() {
        for (at, now, hour, expected) in [
            // The hour has not come yet today, nor passed since yesterday's.
            ("2026-10-25T08:00:30", "2026-10-26T07:59:59", 8, false),
            ("2026-10-25T08:00:30", "2026-10-26T08:00:00", 8, true),
            ("2026-10-26T08:00:00", "2026-10-26T08:00:59", 8, false),
            // A machine asleep for days sends one at wake.
            ("2026-10-23T09:00:00", "2026-10-26T11:00:00", 8, true),
            // Midnight, and the last hour of the day.
            ("2026-10-25T23:59:00", "2026-10-26T00:00:00", 0, true),
            ("2026-10-26T22:00:00", "2026-10-26T22:59:00", 23, false),
            ("2026-10-25T23:30:00", "2026-10-26T23:00:00", 23, true),
            // 02:00 does not exist on 2026-03-29 in Paris: 03:00 stands
            // for it.
            ("2026-03-28T02:30:00", "2026-03-29T03:30:00", 2, true),
            ("2026-03-29T03:00:00", "2026-03-29T03:30:00", 2, false),
        ] {
            assert_eq!(
                due(ts(at), &local(now), hour),
                expected,
                "{at} -> {now} at {hour}"
            );
        }
        // The day CEST ends, 8:00 is 7:00 UTC, an hour later than the day
        // before.
        assert_eq!(
            latest_hour(&local("2026-10-25T09:00:00"), 8).to_string(),
            "2026-10-25T07:00:00Z"
        );
    }

    fn ids(names: &[&str]) -> BTreeSet<TicketId> {
        names.iter().map(|n| TicketId::new(*n).unwrap()).collect()
    }

    #[test]
    fn the_line_names_what_waits_and_ends_with_status() {
        let until = ts("2026-10-26T14:05:00");
        let summary = Summary {
            delivered: ids(&[
                "OWL-1", "OWL-2", "OWL-3", "OWL-4", "OWL-5", "OWL-6", "OWL-7",
            ]),
            waiting: ids(&["OWL-8"]),
            parked: ids(&["OWL-9"]),
            all_paused: false,
            paused: vec!["acme/web".to_owned()],
            usage: vec![
                (Harness::Claude, Usage::Limited(Some(until))),
                (Harness::Codex, Usage::Share(0.424)),
            ],
        };
        assert_eq!(
            line(&summary, &paris()),
            "Owlshift digest: 7 delivered (OWL-1, OWL-2, OWL-3, OWL-4, OWL-5 and 2 more); 1 \
             waiting for a person (OWL-8); 1 parked (OWL-9); paused: acme/web; usage: claude at \
             its limit until Mon 14:05, codex 42 %. More: `owlshift status`."
        );
        let quiet = Summary {
            all_paused: true,
            ..Summary::default()
        };
        assert_eq!(
            line(&quiet, &paris()),
            "Owlshift digest: nothing delivered; every project paused. More: `owlshift status`."
        );
    }

    fn event(project: &str, ticket: &str, at: Timestamp, action: &str) -> Event {
        Event {
            format: Format,
            at,
            project: project.to_owned(),
            ticket: Some(TicketId::new(ticket).unwrap()),
            run: None,
            kind: EventKind::TrackerWrite,
            data: event_data([("target", json!("forge")), ("action", json!(action))]),
        }
    }

    #[test]
    fn gather_reads_deliveries_since_the_latest_pauses_and_usage() {
        let data = tempfile::tempdir().unwrap();
        let data = data.path();
        let web = Repo::parse("acme/web").unwrap();
        let api = Repo::parse("acme/api").unwrap();
        let (web_dirs, api_dirs) = (ProjectDirs::new(data, &web), ProjectDirs::new(data, &api));
        let since = ts("2026-10-25T08:00:00");
        let now = ts("2026-10-26T08:00:00");
        let log = EventLog::in_dir(data);
        for event in [
            event(
                "Acme/Web",
                "OWL-1",
                ts("2026-10-25T12:00:00"),
                "pull_request",
            ),
            event(
                "acme/web",
                "OWL-1",
                ts("2026-10-25T13:00:00"),
                "pull_request",
            ),
            event(
                "acme/web",
                "OWL-2",
                ts("2026-10-25T07:00:00"),
                "pull_request",
            ),
            event("acme/web", "OWL-3", ts("2026-10-25T12:00:00"), "push"),
            event(
                "acme/other",
                "OWL-4",
                ts("2026-10-25T12:00:00"),
                "pull_request",
            ),
        ] {
            log.append(&event).unwrap();
        }
        requests::write_pause(&api_dirs, &Request::new(BY_DAEMON, now, "keychain locked")).unwrap();
        UsageStore::in_data_dir(data)
            .update(Harness::Codex, |state| {
                for (name, share, resets) in [
                    ("five_hour", 0.3, "2026-10-26T10:00:00"),
                    ("seven_day", 0.6, "2026-10-29T10:00:00"),
                    ("old", 0.9, "2026-10-26T07:00:00"),
                ] {
                    state.windows.insert(
                        name.to_owned(),
                        Window {
                            share,
                            resets_at_ms: machine_usage::ms(ts(resets)),
                            read_at_ms: 0,
                        },
                    );
                }
            })
            .unwrap();
        let mut out = Vec::new();
        let projects = [(&web, &web_dirs), (&api, &api_dirs)];
        let summary = gather(data, &projects, &[], since, now, &mut out);
        assert_eq!(summary.delivered, ids(&["OWL-1"]));
        assert_eq!(summary.paused, vec!["acme/api".to_owned()]);
        assert!(!summary.all_paused);
        assert_eq!(summary.usage, vec![(Harness::Codex, Usage::Share(0.6))]);

        // A cap at 50 % holds codex until its week resets.
        let capped = gather(
            data,
            &projects,
            &[(Harness::Codex, 50)],
            since,
            now,
            &mut out,
        );
        assert_eq!(
            capped.usage,
            vec![(
                Harness::Codex,
                Usage::Limited(Some(ts("2026-10-29T10:00:00")))
            )]
        );

        step::write_atomically(
            &requests::machine_pause_file(data),
            &Request::new("owlshift pause", now, "away").render(),
        )
        .unwrap();
        assert!(gather(data, &projects, &[], since, now, &mut out).all_paused);
    }

    #[test]
    fn gather_says_the_event_log_lines_it_cannot_read() {
        let data = tempfile::tempdir().unwrap();
        let data = data.path();
        fs::write(EventLog::in_dir(data).path(), "{\"torn\": \n").unwrap();
        let mut out = Vec::new();
        let summary = gather(data, &[], &[], Timestamp::MIN, Timestamp::MAX, &mut out);
        assert!(summary.delivered.is_empty());
        let said = String::from_utf8(out).unwrap();
        assert!(
            said.contains("the event log's line 1 is not an event"),
            "{said}"
        );
    }

    /// A notifier that keeps what it was asked to show, or fails.
    #[derive(Default)]
    struct Shown {
        lines: RefCell<Vec<String>>,
        fail: bool,
    }

    impl Notifier for Shown {
        fn notify(&self, line: &str) -> Result<(), Error> {
            self.lines.borrow_mut().push(line.to_owned());
            if self.fail {
                Err(Error::new("no notification service"))
            } else {
                Ok(())
            }
        }
    }

    fn settings() -> Settings {
        Settings {
            hour: 8,
            caps: Vec::new(),
        }
    }

    #[test]
    fn one_digest_a_day_across_a_sleep_and_a_restart_never_twice() {
        let data = tempfile::tempdir().unwrap();
        let data = data.path();
        let shown = Shown::default();
        let mut out = Vec::new();
        let mut check = |digests: &mut Digests, at: &str| {
            digests.check(data, &settings(), &[], Some(&shown), &local(at), &mut out)
        };
        let mut digests = Digests::default();
        assert_eq!(
            check(&mut digests, "2026-10-25T07:00:00"),
            Checked::Counting
        );
        assert_eq!(check(&mut digests, "2026-10-25T07:59:00"), Checked::NotDue);
        assert!(matches!(
            check(&mut digests, "2026-10-25T08:00:10"),
            Checked::Sent(_)
        ));
        assert_eq!(check(&mut digests, "2026-10-25T08:01:10"), Checked::NotDue);
        // A restarted daemon reads the record: nothing more today.
        let mut digests = Digests::default();
        assert_eq!(check(&mut digests, "2026-10-25T09:00:00"), Checked::NotDue);
        // Asleep from the evening to three days later: one at wake.
        assert_eq!(check(&mut digests, "2026-10-25T20:00:00"), Checked::NotDue);
        assert!(matches!(
            check(&mut digests, "2026-10-28T11:00:00"),
            Checked::Sent(_)
        ));
        assert_eq!(check(&mut digests, "2026-10-28T11:01:00"), Checked::NotDue);
        assert_eq!(
            check(&mut Digests::default(), "2026-10-28T11:02:00"),
            Checked::NotDue
        );
        assert_eq!(shown.lines.borrow().len(), 2);
        let record = read(data).ok().flatten().unwrap();
        assert_eq!(record.at, ts("2026-10-28T11:00:00"));
        assert_eq!(
            record.line.as_deref(),
            shown.lines.borrow().last().map(String::as_str)
        );
    }

    #[test]
    fn a_record_that_cannot_be_written_or_a_failed_notification_sends_nothing_twice() {
        let data = tempfile::tempdir().unwrap();
        let data = data.path();
        // The service folder is a file: no record can be written.
        fs::write(service_dir(data), "").unwrap();
        let shown = Shown {
            fail: true,
            ..Shown::default()
        };
        let mut out = Vec::new();
        let mut digests = Digests::default();
        let mut check =
            |at: &str| digests.check(data, &settings(), &[], Some(&shown), &local(at), &mut out);
        assert_eq!(check("2026-10-25T07:00:00"), Checked::Counting);
        assert!(matches!(check("2026-10-25T08:00:00"), Checked::Sent(_)));
        assert_eq!(check("2026-10-25T08:01:00"), Checked::NotDue);
        assert_eq!(shown.lines.borrow().len(), 1);
        let (events, _) = why::read_events(EventLog::in_dir(data).path()).unwrap();
        let not_shown: Vec<&Event> = events
            .iter()
            .filter(|e| e.data.get("what") == Some(&json!("notification_not_shown")))
            .collect();
        assert_eq!(not_shown.len(), 1);
        assert_eq!(not_shown[0].project, BY_DAEMON);
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("could not be written"), "{out}");
    }

    #[test]
    fn a_newer_record_sends_nothing_and_a_broken_one_counts_from_now() {
        let data = tempfile::tempdir().unwrap();
        let data = data.path();
        fs::create_dir_all(service_dir(data)).unwrap();
        fs::write(
            record_file(data),
            "{\"format\": 2, \"at\": \"2026-10-20T08:00:00Z\"}",
        )
        .unwrap();
        let shown = Shown::default();
        let mut out = Vec::new();
        let mut digests = Digests::default();
        for at in ["2026-10-25T09:00:00", "2026-10-26T09:00:00"] {
            let checked = digests.check(data, &settings(), &[], Some(&shown), &local(at), &mut out);
            assert_eq!(checked, Checked::Newer);
        }
        assert!(shown.lines.borrow().is_empty());
        let said = String::from_utf8(out.clone()).unwrap();
        assert_eq!(said.matches("newer Owlshift's format").count(), 1, "{said}");

        fs::write(record_file(data), "not json").unwrap();
        let checked = Digests::default().check(
            data,
            &settings(),
            &[],
            Some(&shown),
            &local("2026-10-26T09:00:00"),
            &mut out,
        );
        assert_eq!(checked, Checked::Counting);
        assert_eq!(
            read(data).ok().flatten().unwrap().at,
            ts("2026-10-26T09:00:00")
        );
    }

    /// The daemon looks at its digest at each pass: one digest however many
    /// passes follow, none once a stop is asked before any pass. The daemon
    /// is refused on native Windows (D9).
    #[cfg(unix)]
    #[test]
    fn the_daemon_sends_its_digest_at_a_pass_once() {
        use std::process::Command;
        use std::time::Duration;

        use crate::daemon::{Daemon, Timing};

        let data = tempfile::tempdir().unwrap();
        let data = data.path();
        fs::create_dir_all(service_dir(data)).unwrap();
        let old = DigestRecord {
            format: Format,
            at: "2000-01-01T00:00:00Z".parse().unwrap(),
            line: None,
        };
        fs::write(record_file(data), old.render()).unwrap();
        let shown = Shown::default();
        let watch = |_: &Repo| Command::new("true");
        let daemon = Daemon {
            data,
            watch: &watch,
            notifier: Some(&shown),
            timing: Timing {
                pass: Duration::from_millis(30),
                tick: Duration::from_millis(10),
                ..Timing::SERVICE
            },
            digest: Some(settings()),
        };
        let stop = data.to_owned();
        let asker = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            requests::ask_stop(&stop, &Request::new("test", Timestamp::now(), "done")).unwrap();
        });
        let mut out = Vec::new();
        daemon.run(&mut out);
        asker.join().unwrap();
        assert_eq!(
            shown.lines.borrow().len(),
            1,
            "{}",
            String::from_utf8_lossy(&out)
        );

        fs::write(record_file(data), old.render()).unwrap();
        requests::ask_stop(data, &Request::new("test", Timestamp::now(), "done")).unwrap();
        daemon.run(&mut Vec::new());
        assert_eq!(shown.lines.borrow().len(), 1);
    }
}
