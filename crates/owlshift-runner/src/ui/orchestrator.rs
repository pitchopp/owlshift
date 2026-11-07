//! The Orchestrator screen's answer (P8 part 7, OWL-472, runtime and
//! operations, "Observability & the local web UI"): what `owlshift status`
//! and `owlshift logs` tell about the daemon and its watches, for the page.
//!
//! `GET /api/orchestrator?gen=<generation>&after=<offset>&log=<offset>`
//! answers, as JSON:
//!
//! - `lines`: the lines of `owlshift status`'s text the Orchestrator shows
//!   ([`Part::on_the_page`]), in its order and its words, built by the same
//!   function ([`status::parts`]); `left_out` names the kinds of line it
//!   leaves to other screens;
//! - `daemon`, `machine_pause` and `projects` (each with its `watch`, record,
//!   `paused`, `quarantined` and `draining`), the header's own structure
//!   ([`header::of`]) with the watch state added;
//! - `usage`: each harness's windows, with their share, reset time, the
//!   usage cap and whether a window holds the harness;
//! - `events`: the decisions, warnings and lines that are not events the
//!   index holds, each with its offset (the cursor), its fields and the line
//!   `owlshift logs` prints for it ([`events::format_line`]); `reset` says
//!   the page's cursors do not hold (the log was replaced since its
//!   generation, `after` is past the log's end, or entries it did not see
//!   were dropped) and `entries` are then the latest, to read afresh;
//! - `log`: the daemon's log (`<data>/service/daemon.log`), its last lines
//!   at first and then the lines that follow `log`, or why it is absent.
//!
//! The event log is never read whole by a request: the index reads what was
//! appended since the last one ([`EventIndex`]), so the watches come from
//! `status::gather_files` and the index, not from `status::gather`.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::Mutex;

use jiff::Timestamp;
use jiff::tz::TimeZone;
use owlshift_contracts::config::PersonalConfig;
use owlshift_core::usage::at_cap;
use owlshift_core::vocab::Harness;
use serde_json::{Value, json};

use super::header;
use super::index::{EventIndex, Item};
use super::tail::LineTail;
use crate::config::FileState;
use crate::daemon::digest::HarnessUsage;
use crate::events;
use crate::status::{self, Daemon, Part, Report, Watch};
use crate::usage::{self as machine_usage, UsageStore};

/// How many feed entries a first answer, or a reset one, carries.
pub const FIRST_ENTRIES: usize = 100;

/// The most bytes of the daemon's log one answer carries.
pub const LOG_ANSWER: usize = 256 * 1024;

/// The longest line of the daemon's log an answer carries whole.
pub const LOG_LINE: usize = 4 * 1024;

/// The bytes of the log's end a first answer reads, and the lines it keeps.
const LOG_FIRST_BYTES: u64 = 64 * 1024;
const LOG_FIRST_LINES: usize = 200;

/// What a page passes back.
#[derive(Clone, Copy, Debug, Default)]
pub struct Cursors {
    /// The generation and the offset of the last feed entry it holds.
    pub events: Option<(u64, u64)>,
    /// The offset of the daemon's log it read to.
    pub log: Option<u64>,
}

/// What the answer is made from.
pub struct Inputs<'a> {
    pub data: &'a Path,
    pub index: &'a Mutex<EventIndex>,
    pub personal: &'a FileState<PersonalConfig>,
    pub now: Timestamp,
    pub tz: TimeZone,
}

fn time(at: Timestamp) -> Value {
    Value::String(at.to_string())
}

/// A watch as the page reads it.
pub fn watch_json(watch: &Watch) -> Value {
    match watch {
        Watch::NoDaemon => json!({ "state": "no_daemon" }),
        Watch::NotRead => json!({ "state": "not_read" }),
        Watch::NotStarted => json!({ "state": "not_started" }),
        Watch::Running { pid, since } => {
            json!({ "state": "running", "pid": pid, "since": time(*since) })
        }
        Watch::Restarting {
            ended,
            at,
            why,
            in_a_row,
        } => json!({
            "state": "restarting",
            "ended": time(*ended),
            "restart_at": at.map(time),
            "why": why,
            "in_a_row": in_a_row,
        }),
        Watch::Unservable { at, why } => {
            json!({ "state": "unservable", "at": time(*at), "why": why })
        }
        Watch::Ended { at } => json!({ "state": "ended", "at": time(*at) }),
    }
}

/// The report with each project's watch read from the index, as
/// `status::gather` reads it from the whole log.
pub fn report(inputs: &Inputs<'_>, index: &EventIndex) -> Report {
    let runs = machine_usage::concurrent_runs(inputs.personal);
    let caps = machine_usage::caps_of(inputs.personal);
    let mut report = status::gather_files(inputs.data, runs, &caps, inputs.now);
    // A record that does not read leaves the daemon's start unknown: every
    // event counts.
    let since = match &report.daemon {
        Daemon::Running(record) => Some(
            record
                .as_ref()
                .map_or(Timestamp::MIN, |record| record.since),
        ),
        _ => None,
    };
    if let Some(since) = since {
        for project in &mut report.projects {
            let latest: Vec<_> = index
                .watch_event(&project.repository)
                .cloned()
                .into_iter()
                .collect();
            project.watch = status::watch_state(&latest, &project.repository, since);
        }
    }
    report
}

/// The usage windows of every harness: each window's share, reset, the
/// moment it was read, and whether it holds the harness under the usage
/// cap; the standing the harness has in `owlshift status` beside them.
fn usage(inputs: &Inputs<'_>, report: &Report) -> Vec<Value> {
    let caps = machine_usage::caps_of(inputs.personal);
    let store = UsageStore::in_data_dir(inputs.data).with_caps(caps.iter().copied());
    let now_ms = machine_usage::ms(inputs.now);
    [Harness::Claude, Harness::Codex]
        .into_iter()
        .map(|harness| {
            let cap = caps
                .iter()
                .find(|(other, _)| *other == harness)
                .map(|(_, cap)| *cap);
            let windows: Vec<Value> = match store.read_at(harness, now_ms) {
                Ok(state) => state
                    .windows
                    .iter()
                    .map(|(name, window)| {
                        let standing = window.resets_at_ms > now_ms;
                        json!({
                            "name": name,
                            "share": window.share,
                            "resets_at": time(machine_usage::at(window.resets_at_ms)),
                            "read_at": time(machine_usage::at(window.read_at_ms)),
                            "under_way": standing,
                            "holds": standing && cap.is_some_and(|cap| at_cap(window.share, cap)),
                        })
                    })
                    .collect(),
                Err(_) => Vec::new(),
            };
            let standing = report
                .usage
                .iter()
                .find(|(other, _)| *other == harness)
                .map(|(_, usage)| match usage {
                    HarnessUsage::Limited(until) => {
                        json!({ "state": "limited", "until": until.map(time) })
                    }
                    HarnessUsage::Share { share, resets_at } => {
                        json!({ "state": "share", "share": share, "resets_at": time(*resets_at) })
                    }
                });
            json!({
                "harness": harness,
                "name": machine_usage::key(harness),
                "cap_percent": cap,
                "windows": windows,
                "standing": standing,
            })
        })
        .collect()
}

/// A feed entry as the page reads it.
fn entry_json(entry: &super::index::Entry) -> Value {
    match &entry.item {
        Item::Event(event) => json!({
            "offset": entry.offset,
            "at": time(event.at),
            "project": event.project,
            "ticket": event.ticket,
            "run": event.run,
            "kind": events::kind_name(event.kind),
            "details": event.data,
            "line": events::format_line(event),
        }),
        Item::Line(text) => json!({ "offset": entry.offset, "raw": text }),
    }
}

/// The last lines of the daemon's log, and where to follow it from.
fn log_first(path: &Path) -> std::io::Result<(Vec<(u64, String)>, u64)> {
    let mut file = File::open(path)?;
    let size = file.metadata()?.len();
    let start = size.saturating_sub(LOG_FIRST_BYTES);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.take(size - start).read_to_end(&mut bytes)?;
    // Only whole lines: the first may start before the window, the last may
    // still be written.
    let end = bytes
        .iter()
        .rposition(|&b| b == b'\n')
        .map_or(0, |at| at + 1);
    let mut from = 0;
    if start > 0 {
        from = bytes[..end]
            .iter()
            .position(|&b| b == b'\n')
            .map_or(end, |at| at + 1);
    }
    let mut lines = Vec::new();
    let mut at = start + from as u64;
    for line in bytes[from..end].split_inclusive(|&b| b == b'\n') {
        let text = line.strip_suffix(b"\n").unwrap_or(line);
        let text = text.strip_suffix(b"\r").unwrap_or(text);
        let mut shown = String::from_utf8_lossy(text).into_owned();
        if shown.len() > LOG_LINE {
            let cut = (0..=LOG_LINE).rev().find(|n| shown.is_char_boundary(*n));
            shown.truncate(cut.unwrap_or(0));
            shown.push('…');
        }
        lines.push((at, shown));
        at += line.len() as u64;
    }
    if lines.len() > LOG_FIRST_LINES {
        lines.drain(..lines.len() - LOG_FIRST_LINES);
    }
    Ok((lines, start + end as u64))
}

/// The daemon's log for an answer.
fn log_json(data: &Path, from: Option<u64>) -> Value {
    let path = crate::daemon::service_dir(data).join("daemon.log");
    let absent = |why: String| {
        json!({
            "absent": why,
            "lines": [],
            "next": 0,
            "reset": from.is_some(),
        })
    };
    if !path.exists() {
        return absent(
            "the daemon's log does not exist: only a daemon started through the launcher \
             (`owlshift start`) writes one"
                .to_owned(),
        );
    }
    match from {
        None => match log_first(&path) {
            Ok((lines, next)) => json!({
                "lines": lines.iter().map(|(_, text)| text).collect::<Vec<_>>(),
                "next": next,
                "reset": false,
            }),
            Err(error) => absent(format!("{} cannot be read: {error}", path.display())),
        },
        Some(from) => {
            let mut tail = LineTail::new(&path);
            tail.seek(from);
            match tail.read(LOG_ANSWER, LOG_LINE) {
                Ok(read) => json!({
                    "lines": read.lines.iter().map(|line| &line.text).collect::<Vec<_>>(),
                    "next": tail.offset(),
                    "reset": read.restarted,
                }),
                Err(error) => absent(format!("{} cannot be read: {error}", path.display())),
            }
        }
    }
}

/// The Orchestrator's answer.
pub fn answer(inputs: &Inputs<'_>, cursors: Cursors) -> Value {
    let (report, feed, generation, offset, index_error) = {
        let mut index = inputs
            .index
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let error = index.update().err();
        let report = report(inputs, &index);
        let feed = index.feed(cursors.events, FIRST_ENTRIES);
        (report, feed, index.generation(), index.offset(), error)
    };
    let mut head = header::of(&report).0;
    if let (Some(why), Some(notes)) = (
        index_error,
        head.get_mut("notes").and_then(Value::as_array_mut),
    ) {
        notes.push(Value::String(format!(
            "the event log cannot be read: {why}"
        )));
    }
    let lines: Vec<String> = status::parts(&report, &inputs.tz)
        .into_iter()
        .filter(|(part, _)| part.on_the_page())
        .map(|(_, text)| text)
        .collect();
    let left_out: Vec<&str> = Part::ALL
        .into_iter()
        .filter(|part| !part.on_the_page())
        .map(Part::name)
        .collect();
    let watches: Vec<Value> = report
        .projects
        .iter()
        .map(|project| watch_json(&project.watch))
        .collect();
    if let Some(projects) = head.get_mut("projects").and_then(Value::as_array_mut) {
        for (project, (watch, source)) in projects
            .iter_mut()
            .zip(watches.into_iter().zip(&report.projects))
        {
            project["watch"] = watch;
            project["checkout"] = json!(source.checkout);
            project["draining"] = json!(source.draining);
            project["record"] = match &source.config {
                Ok(record) => json!({
                    "kept": record.kept.as_ref().map(|kept| json!({
                        "name": crate::served::kept_name(kept),
                        "branch": kept.branch,
                        "commit": kept.commit,
                        "since": time(kept.since),
                    })),
                    "refused": record.refused.as_ref().map(|refused| json!({
                        "reason": refused.reason,
                        "commit": refused.commit,
                        "since": time(refused.since),
                    })),
                }),
                Err(why) => json!({ "error": why }),
            };
        }
    }
    let entries: Vec<Value> = feed.entries.iter().map(entry_json).collect();
    let cursor = feed.entries.last().map(|entry| entry.offset);
    json!({
        "generation": generation,
        "lines": lines,
        "left_out": left_out,
        "daemon": head["daemon"],
        "machine_pause": head["machine_pause"],
        "projects": head["projects"],
        "notes": head["notes"],
        "usage": usage(inputs, &report),
        "events": {
            "entries": entries,
            "reset": feed.reset,
            "generation": feed.generation,
            "cursor": cursor,
            "end": offset,
            // The entries a page that fell behind missed cannot be counted.
            "skipped": if feed.reset { Value::Null } else { json!(0) },
        },
        "log": log_json(inputs.data, cursors.log),
    })
}
