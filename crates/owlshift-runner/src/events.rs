//! The event log: every event `owlshift do` records, one per line, and what
//! `owlshift logs` prints (build plan, OWL-20).
//!
//! P1's stand-in for the local store of the architecture (section 5): one
//! file of JSON Lines, [`EVENTS_FILE`] in Owlshift's data directory, shared
//! by every project of the machine. Each line is one contract
//! [`Event`] ([`Event::render_line`]). Appends from several processes are
//! serialized by an exclusive lock on a file of its own, [`LOCK_FILE`], so
//! no reader ever waits on the log itself (a lock on Windows would block its
//! reads), and a last line torn by a crash gets its line break before the
//! next event is written.
//!
//! What reaches a terminal goes through [`printable`] first: an event's text
//! comes from models, trackers and file names, and must not drive the
//! terminal with control sequences.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use jiff::Timestamp;
use serde_json::{Map, Value};

use owlshift_contracts::event::{Event, EventKind};
use owlshift_contracts::format::Format;
use owlshift_contracts::ids::TicketId;

/// The event log's file name, in the data directory.
pub const EVENTS_FILE: &str = "events.jsonl";

/// The file whose lock serializes appends, beside the log.
pub const LOCK_FILE: &str = "events.lock";

/// An event's details.
pub type Data = Map<String, Value>;

/// Builds an event's details from `(key, value)` pairs; they are kept, and
/// printed, in key order.
pub fn data<const N: usize>(pairs: [(&str, Value); N]) -> Data {
    pairs
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value))
        .collect()
}

/// The event log of a data directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EventLog {
    path: PathBuf,
    lock: PathBuf,
}

impl EventLog {
    /// The log in `data_dir`.
    pub fn in_dir(data_dir: &Path) -> Self {
        Self {
            path: data_dir.join(EVENTS_FILE),
            lock: data_dir.join(LOCK_FILE),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Appends `event` as one line, holding the log's lock.
    pub fn append(&self, event: &Event) -> io::Result<()> {
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir)?;
        }
        let lock = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&self.lock)?;
        // Released when `lock` is dropped, or when the process ends.
        lock.lock()?;
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(&self.path)?;
        let mut line = String::new();
        if ends_torn(&mut file)? {
            line.push('\n');
        }
        line.push_str(&event.render_line());
        line.push('\n');
        file.write_all(line.as_bytes())?;
        file.flush()
    }
}

/// Whether the file's last byte is not a line break: a line cut short.
fn ends_torn(file: &mut File) -> io::Result<bool> {
    let len = file.metadata()?.len();
    if len == 0 {
        return Ok(false);
    }
    file.seek(SeekFrom::Start(len - 1))?;
    let mut last = [0u8; 1];
    file.read_exact(&mut last)?;
    Ok(last[0] != b'\n')
}

/// Records the events of one `owlshift do`: each is appended to the log and
/// printed on `out` as [`format_line`] shows it. A failed append does not
/// stop the run: a warning line says so on `out`, right away.
pub struct EventSink<'a> {
    project: String,
    log: EventLog,
    out: &'a mut dyn Write,
}

impl<'a> EventSink<'a> {
    pub fn new(project: impl Into<String>, log: EventLog, out: &'a mut dyn Write) -> Self {
        Self {
            project: project.into(),
            log,
            out,
        }
    }

    /// Records one event about `ticket`, now, and returns it.
    pub fn emit(
        &mut self,
        ticket: &TicketId,
        run: Option<&str>,
        kind: EventKind,
        data: Data,
    ) -> Event {
        let event = Event {
            format: Format,
            at: Timestamp::now(),
            project: self.project.clone(),
            ticket: Some(ticket.clone()),
            run: run.map(ToOwned::to_owned),
            kind,
            data,
        };
        if let Err(error) = self.log.append(&event) {
            let warning = format!(
                "owlshift: warning: the event was not recorded in {}: {error}",
                self.log.path().display()
            );
            let _ = writeln!(self.out, "{}", printable(&warning));
        }
        let _ = writeln!(self.out, "{}", format_line(&event));
        event
    }
}

/// One event on one line: its time, ticket (`-` for none), kind, run, then
/// its details as `key=value`, in key order. A key or a text value that holds a
/// space, a quote, an `=` or a control character is printed as a JSON
/// string; other values as JSON. The line is [`printable`].
pub fn format_line(event: &Event) -> String {
    let mut line = format!(
        "{} {} {}",
        event.at,
        event.ticket.as_ref().map_or("-", TicketId::as_str),
        kind_name(event.kind)
    );
    if let Some(run) = &event.run {
        line.push_str(&format!(" run={}", word(run)));
    }
    for (key, value) in &event.data {
        let value = match value {
            Value::String(text) => word(text),
            other => other.to_string(),
        };
        line.push_str(&format!(" {}={value}", word(key)));
    }
    printable(&line)
}

/// An event kind's name, as the contract writes it: `run_started`.
pub fn kind_name(kind: EventKind) -> String {
    match serde_json::to_value(kind) {
        Ok(Value::String(name)) => name,
        _ => format!("{kind:?}"),
    }
}

/// `text` as is when it is one plain word, as a JSON string otherwise.
fn word(text: &str) -> String {
    let plain = !text.is_empty()
        && !text
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '"' | '=' | '\\'));
    if plain {
        text.to_owned()
    } else {
        Value::String(text.to_owned()).to_string()
    }
}

/// `text` safe to print on a terminal: every control character but the line
/// break and the tab, and every bidirectional override, is written as an
/// escape such as `\u{1b}`, so text from a model, a tracker or a file name
/// cannot move the cursor, recolour or hide what follows.
pub fn printable(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        let bidi = matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}');
        if (c.is_control() && c != '\n' && c != '\t') || bidi {
            out.extend(c.escape_unicode());
        } else {
            out.push(c);
        }
    }
    out
}

/// How [`print`] follows the log once it reached its end.
pub struct Follow<'a> {
    /// How long to wait between two looks at the file.
    pub poll: Duration,
    /// Asked after each look: `true` ends the follow.
    pub stop: &'a dyn Fn() -> bool,
}

/// Prints the events of the log at `path` in order, only those of `ticket`
/// when given, one [`format_line`] each, and returns how many it printed. A
/// line that is not an event is reported on `errors` with its number, and
/// skipped. With `follow`, it then keeps printing the events appended to the
/// file, waiting for the file to appear if needed, until `follow.stop` says
/// so; a line is read only once its line break is written, so an event
/// being appended is never printed half.
pub fn print(
    path: &Path,
    ticket: Option<&TicketId>,
    out: &mut dyn Write,
    errors: &mut dyn Write,
    follow: Option<Follow<'_>>,
) -> io::Result<usize> {
    let mut file = None;
    let mut pending = Vec::new();
    let mut line_number = 0usize;
    let mut printed = 0usize;
    loop {
        if file.is_none() {
            file = match File::open(path) {
                Ok(opened) => Some(opened),
                Err(error) if error.kind() == io::ErrorKind::NotFound => None,
                Err(error) => return Err(error),
            };
        }
        if let Some(file) = file.as_mut() {
            file.read_to_end(&mut pending)?;
            let complete = pending.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
            let lines: Vec<u8> = pending.drain(..complete).collect();
            for raw in lines.split(|b| *b == b'\n').filter(|raw| !raw.is_empty()) {
                line_number += 1;
                let text = String::from_utf8_lossy(raw);
                match Event::parse(text.trim_end_matches('\r')) {
                    Ok(event) if ticket.is_none_or(|id| event.ticket.as_ref() == Some(id)) => {
                        writeln!(out, "{}", format_line(&event))?;
                        printed += 1;
                    }
                    Ok(_) => {}
                    Err(error) => {
                        let message = format!(
                            "{}: line {line_number} is not an event: {error}",
                            path.display()
                        );
                        writeln!(errors, "{}", printable(&message))?;
                    }
                }
            }
            out.flush()?;
        }
        let Some(follow) = &follow else {
            return Ok(printed);
        };
        if (follow.stop)() {
            return Ok(printed);
        }
        thread::sleep(follow.poll);
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use serde_json::json;

    use super::*;

    fn event(ticket: &str, kind: EventKind, data: Data) -> Event {
        Event {
            format: Format,
            at: "2026-09-29T10:00:00Z".parse().unwrap(),
            project: "demo/project".to_owned(),
            ticket: Some(TicketId::new(ticket).unwrap()),
            run: Some("20260929T100000Z".to_owned()),
            kind,
            data,
        }
    }

    #[test]
    fn a_line_shows_every_detail_and_quotes_what_is_not_one_word() {
        let details = data([
            ("outcome", json!("finished")),
            ("summary", json!("Added the file; gate green.")),
            ("exit_code", json!(0)),
            ("gate", json!({ "passed": true })),
            ("weird key", json!("a=b")),
        ]);
        assert_eq!(
            format_line(&event("OWL-7", EventKind::RunEnded, details)),
            "2026-09-29T10:00:00Z OWL-7 run_ended run=20260929T100000Z exit_code=0 \
             gate={\"passed\":true} outcome=finished summary=\"Added the file; gate green.\" \
             \"weird key\"=\"a=b\""
        );
    }

    #[test]
    fn nothing_printed_can_drive_the_terminal() {
        let hostile = "\u{1b}[2J\u{1b}]0;title\u{7}\u{202e}txt.exe";
        let line = format_line(&event(
            "OWL-7",
            EventKind::RunEnded,
            data([("summary", json!(hostile)), (hostile, json!(1))]),
        ));
        assert!(!line.chars().any(|c| c.is_control()), "{line:?}");
        assert!(!line.contains('\u{202e}'), "{line:?}");
        // Free text keeps its lines, and loses its escapes.
        assert_eq!(printable("a\n\u{1b}[31mb\tc"), "a\n\\u{1b}[31mb\tc");
    }

    #[test]
    fn the_log_appends_whole_lines_and_mends_a_torn_one() {
        let dir = tempfile::tempdir().unwrap();
        let log = EventLog::in_dir(&dir.path().join("data"));
        let first = event("OWL-1", EventKind::Dispatch, Data::new());
        log.append(&first).unwrap();
        // A crash cut the next line short.
        let mut file = OpenOptions::new().append(true).open(log.path()).unwrap();
        file.write_all(b"{\"format\":1,\"at\"").unwrap();
        let second = event("OWL-2", EventKind::Dispatch, Data::new());
        log.append(&second).unwrap();

        let (mut out, mut errors) = (Vec::new(), Vec::new());
        let printed = print(log.path(), None, &mut out, &mut errors, None).unwrap();
        assert_eq!(printed, 2);
        let out = String::from_utf8(out).unwrap();
        assert_eq!(out, format!("{}\n{}\n", format_line(&first), format_line(&second)));
        let errors = String::from_utf8(errors).unwrap();
        assert!(errors.contains("line 2 is not an event"), "{errors}");

        let mut only = Vec::new();
        let id = TicketId::new("OWL-2").unwrap();
        print(log.path(), Some(&id), &mut only, &mut Vec::new(), None).unwrap();
        assert_eq!(String::from_utf8(only).unwrap(), format!("{}\n", format_line(&second)));
    }

    #[test]
    fn a_missing_log_prints_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(EVENTS_FILE);
        assert_eq!(print(&path, None, &mut Vec::new(), &mut Vec::new(), None).unwrap(), 0);
    }

    #[test]
    fn follow_prints_an_event_once_its_line_is_complete() {
        let dir = tempfile::tempdir().unwrap();
        let log = EventLog::in_dir(dir.path());
        let later = event("OWL-3", EventKind::RunStarted, Data::new());
        let line = format!("{}\n", later.render_line());
        let (head, tail) = line.split_at(10);
        let looks = Cell::new(0);
        // The file appears after the first look, half written at the second,
        // whole at the third.
        let stop = || {
            looks.set(looks.get() + 1);
            match looks.get() {
                1 => fs::write(log.path(), head).unwrap(),
                2 => {
                    let mut file = OpenOptions::new().append(true).open(log.path()).unwrap();
                    file.write_all(tail.as_bytes()).unwrap();
                }
                _ => {}
            }
            looks.get() > 3
        };
        let mut out = Vec::new();
        let follow = Follow {
            poll: Duration::ZERO,
            stop: &stop,
        };
        let printed = print(log.path(), None, &mut out, &mut Vec::new(), Some(follow)).unwrap();
        assert_eq!(printed, 1);
        assert_eq!(String::from_utf8(out).unwrap(), format!("{}\n", format_line(&later)));
    }

    #[test]
    fn the_sink_prints_what_it_records() {
        let dir = tempfile::tempdir().unwrap();
        let log = EventLog::in_dir(dir.path());
        let mut out = Vec::new();
        let id = TicketId::new("OWL-4").unwrap();
        let recorded = EventSink::new("demo/project", log.clone(), &mut out).emit(
            &id,
            Some("r1"),
            EventKind::Dispatch,
            data([("branch", json!("owlshift/owl-4"))]),
        );
        let printed = String::from_utf8(out).unwrap();
        assert_eq!(printed, format!("{}\n", format_line(&recorded)));
        let text = fs::read_to_string(log.path()).unwrap();
        assert_eq!(Event::parse(text.trim_end()).unwrap(), recorded);
    }
}
