//! `owlshift why` (OWL-407, architecture section 5, "Parallel",
//! "`owlshift why`"): why each admitted ticket of a project is not running,
//! read from what Owlshift keeps and nothing else.
//!
//! It reads the event log's `decision` events (`drain`, `watch`, `usage` or
//! `base` = `waits`), the project's ticket refs, the machine's usage files,
//! the project's base checks (OWL-418), the quarantine marker and the run
//! slots. It takes no lock the writers hold:
//! the project's lock and the tickets' locks are never tried, so it runs
//! while `owlshift watch` does. [`fold`] and [`render`] are pure; [`gather`]
//! does the reading. The one thing it touches that a writer can notice is
//! the slot files: counting the slots held tries each free one for a
//! moment, as watch's own count does, and removes a dead holder's record
//! under it (`MachineSlots::holders`). The holders it names are read from
//! the slots' holder records (OWL-417), never from the events: a run that
//! began and never ended may have died, and a slot is held through its
//! step's gate, when no run is open.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::Path;

use jiff::Timestamp;
use owlshift_contracts::event::{Event, EventKind};
use owlshift_contracts::ids::TicketId;
use owlshift_core::base::{Check, Entry, Verdict};
use owlshift_core::state::Status;
use owlshift_core::usage::Standing;
use owlshift_core::vocab::{Harness, Stage};
use serde_json::Value;

use crate::base;
use crate::events::{Data, printable};
use crate::project::ProjectDirs;
use crate::slots::{HeldSlot, MachineSlots};
use crate::ticket_ref::{self, RefState};
use crate::usage::{self as machine_usage, UsageStore};

/// One time a ticket was told to wait: a `decision` event.
#[derive(Clone, Debug, PartialEq)]
pub struct Waiting {
    pub at: Timestamp,
    pub ticket: TicketId,
    /// The reason's name: `Wait::key()` for the drain's, `quarantine`,
    /// `slot`, `collision`, `in_flight_unread`, `usage_limit`...
    pub reason: String,
    pub data: Data,
}

/// What the events say.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Folded {
    /// Each ticket of the project whose latest wait is still its latest
    /// word: no dispatch or run followed it.
    pub current: BTreeMap<TicketId, Waiting>,
    /// Every wait of every ticket of the project, oldest first.
    pub history: BTreeMap<TicketId, Vec<Waiting>>,
    /// People's tickets whose latest event is the `zones_unknown` warning.
    pub people: BTreeMap<TicketId, Timestamp>,
}

/// Folds `events`, oldest first, for the project `project`.
pub fn fold(events: &[Event], project: &str) -> Folded {
    let mut folded = Folded::default();
    for event in events {
        if event.project != project {
            continue;
        }
        let Some(ticket) = &event.ticket else {
            continue;
        };
        let zones_unknown = event.kind == EventKind::Warning
            && event.data.get("what").and_then(Value::as_str) == Some("zones_unknown");
        if zones_unknown {
            folded.people.insert(ticket.clone(), event.at);
        } else {
            folded.people.remove(ticket);
        }
        if let Some(waiting) = waiting(event, ticket) {
            folded
                .history
                .entry(ticket.clone())
                .or_default()
                .push(waiting.clone());
            folded.current.insert(ticket.clone(), waiting);
        } else if matches!(
            event.kind,
            EventKind::Dispatch | EventKind::RunStarted | EventKind::PrMerged
        ) {
            folded.current.remove(ticket);
        }
    }
    folded
}

/// The wait a `decision` event tells, if it tells one.
fn waiting(event: &Event, ticket: &TicketId) -> Option<Waiting> {
    if event.kind != EventKind::Decision {
        return None;
    }
    let says = |key: &str| event.data.get(key).and_then(Value::as_str) == Some("waits");
    let reason = if says("drain") || says("watch") {
        event.data.get("reason")?.as_str()?.to_owned()
    } else if says("usage") {
        "usage_limit".to_owned()
    } else if says("base") {
        "base_broken".to_owned()
    } else if event.data.get("drain").and_then(Value::as_str) == Some("pr_cap") {
        // The drain tells the cap once, for the first ticket it holds
        // (`tell_cap`), with the counts and no `waits`.
        "pr_cap".to_owned()
    } else if event.data.get("to_watch").and_then(Value::as_bool) == Some(true)
        && event.data.get("takes").and_then(Value::as_str) == Some("resume")
    {
        // A restart left to watch's resume (OWL-420): its word until watch
        // tells a wait or the ticket runs.
        "to_watch".to_owned()
    } else {
        return None;
    };
    Some(Waiting {
        at: event.at,
        ticket: ticket.clone(),
        reason,
        data: event.data.clone(),
    })
}

/// The phrase a wait is shown as.
pub fn phrase(wait: &Waiting) -> String {
    let data = &wait.data;
    let list = |key: &str| -> String {
        data.get(key)
            .and_then(Value::as_array)
            .map(|ids| {
                ids.iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default()
    };
    match wait.reason.as_str() {
        "blocked_by" => format!("blocked by {}", list("tickets")),
        "blocker_canceled" => format!(
            "its blocker {} was canceled: a person removes the relation or the line",
            list("tickets")
        ),
        "blocker_unknown" => format!(
            "its \"Blocked by\" line names {}, which the tracker does not know",
            list("tickets")
        ),
        "slot" => format!("the project's slot is held by {}", list("tickets")),
        "collision" => {
            let holder = if let Some(number) = data.get("pull_request").and_then(Value::as_u64) {
                format!("pull request #{number}")
            } else if data.get("person").and_then(Value::as_bool) == Some(true) {
                format!("{}, which a person works on,", list("tickets"))
            } else {
                list("tickets")
            };
            let resource = data.get("resource").map_or_else(String::new, resource_text);
            // A Build's admission that waits (OWL-448).
            if data.get("stage").and_then(Value::as_str) == Some("build") {
                format!("its Build waits: {holder} holds {resource}, which its Build would take")
            } else {
                format!("{holder} holds {resource}, which it would touch")
            }
        }
        "pr_cap" => match (
            data.get("open_prs").and_then(Value::as_u64),
            data.get("max_open_prs").and_then(Value::as_u64),
        ) {
            (Some(open), Some(cap)) => {
                format!("the open pull requests reached their cap ({open} of {cap})")
            }
            _ => "the open pull requests reached their cap".to_owned(),
        },
        "duplicate" => "the column lists it twice; the first is taken".to_owned(),
        "not_admitted" => {
            "its admission gesture is not from an account `[tracker].admitters` lists".to_owned()
        }
        "excluded" => "it carries the exclusion label".to_owned(),
        "no_decider" => {
            "it has no decider: give it an assignee, or a `zone:` label whose zone has an owner"
                .to_owned()
        }
        "no_intake" => "its intake has not run yet".to_owned(),
        "stale_intake" => "its text changed since its intake, which runs again".to_owned(),
        "not_ready" => {
            "it is being worked, waits for an answer, is parked, or its pull request is followed"
                .to_owned()
        }
        "run_cap" => "every run slot of this machine is taken".to_owned(),
        "in_flight_unread" => "the work in flight outside Owlshift could not be read".to_owned(),
        "quarantine" => "the project is quarantined (`owlshift unquarantine`)".to_owned(),
        "to_watch" => "restarted by `owlshift continue --to-watch`: `owlshift watch` resumes it \
                       at its next pass"
            .to_owned(),
        "usage_limit" => {
            let harness = data
                .get("harness")
                .and_then(Value::as_str)
                .unwrap_or("a harness");
            match data.get("until").and_then(Value::as_str) {
                Some(until) => format!("{harness} is at its usage limit until {until}"),
                None => format!("{harness} is at its usage limit"),
            }
        }
        "base_broken" => {
            let commit = data.get("commit").and_then(Value::as_str).unwrap_or("");
            let failing = data
                .get("command")
                .and_then(Value::as_str)
                .map_or_else(|| "its gate".to_owned(), |command| format!("`{command}`"));
            if data.get("pending").and_then(Value::as_bool) == Some(true) {
                format!(
                    "whether the base its branch was made from, {}, fails {failing} too is \
                     checked once no other ticket's step runs",
                    base::short(commit)
                )
            } else {
                format!(
                    "the base its branch was made from, {}, fails {failing} in a fresh \
                     worktree too, so its Build waits for that base to pass",
                    base::short(commit)
                )
            }
        }
        other => other.replace('_', " "),
    }
}

/// A resource as the events name it (`held::resource_data`).
fn resource_text(value: &Value) -> String {
    if let Some(zone) = value.get("zone").and_then(Value::as_str) {
        if zone.is_empty() {
            "the whole repository".to_owned()
        } else {
            format!("zone `{zone}`")
        }
    } else if let Some(name) = value.get("named").and_then(Value::as_str) {
        format!("resource `{name}`")
    } else if value.get("browser").is_some() {
        "the browser".to_owned()
    } else {
        "a resource".to_owned()
    }
}

/// A harness's standing, as the report says it.
#[derive(Clone, Debug, PartialEq)]
pub enum Limit {
    Free,
    ProbeDue,
    Until(Timestamp),
    Unread(String),
}

/// Everything `why` reads, ready to be shown.
#[derive(Clone, Debug)]
pub struct Report {
    pub project: String,
    /// The ticket asked about, if one.
    pub only: Option<TicketId>,
    pub folded: Folded,
    /// Each ticket's stage, from its ref.
    pub stages: HashMap<TicketId, String>,
    pub quarantined: bool,
    /// Why part of the picture could not be read.
    pub notes: Vec<String>,
    pub limits: Vec<(Harness, Limit)>,
    /// The machine's slots held now, each with its holder as its record
    /// says, and `concurrent_runs`; `None` when the slots cannot be probed.
    pub slots: Option<(Vec<HeldSlot>, u32)>,
    /// The project's base checks that hold its Builds: holds and checks due
    /// (OWL-418).
    pub bases: Vec<Check>,
}

/// Reads every event of the log at `path`; the lines that are not events
/// are named in the second part. A missing log is an empty one.
pub fn read_events(path: &Path) -> io::Result<(Vec<Event>, Vec<String>)> {
    let text = match fs::read(path) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok((Vec::new(), vec![])),
        Err(error) => return Err(error),
    };
    let mut events = Vec::new();
    let mut bad = Vec::new();
    for (n, line) in text
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.is_empty())
    {
        match Event::parse(line.trim_end_matches('\r')) {
            Ok(event) => events.push(event),
            Err(error) => bad.push(format!("line {} is not an event: {error}", n + 1)),
        }
    }
    Ok((events, bad))
}

/// Reads what `why` shows. No lock is tried but the slots' probe and the
/// usage files are read without theirs.
pub fn gather(
    data_dir: &Path,
    dirs: &ProjectDirs,
    project: &str,
    only: Option<TicketId>,
    concurrent_runs: u32,
    caps: &[(Harness, u8)],
) -> io::Result<Report> {
    let (events, bad) = read_events(crate::events::EventLog::in_dir(data_dir).path())?;
    let mut notes = bad;
    let folded = fold(&events, project);
    let mut stages = HashMap::new();
    if dirs.checkout().exists() {
        match ticket_ref::states(&crate::project::runner_git(), &dirs.checkout()) {
            Ok(refs) => {
                for (ticket, state) in refs {
                    match state {
                        RefState::Read(Some(state)) | RefState::Unadmitted(state) => {
                            stages.insert(ticket, stage_text(state.status()));
                        }
                        RefState::Read(None) => {}
                        RefState::Unread(why) => {
                            notes.push(format!("the ref of {ticket} cannot be read: {why}"));
                        }
                    }
                }
            }
            Err(why) => notes.push(format!("the ticket refs cannot be read: {why}")),
        }
    }
    let quarantined = match dirs.quarantine() {
        Ok(marker) => marker.is_some(),
        Err(error) => {
            notes.push(format!("the quarantine cannot be read: {error}"));
            false
        }
    };
    // A quarantine lifted since is not a reason any more.
    let mut folded = folded;
    if !quarantined {
        folded.current.retain(|_, wait| wait.reason != "quarantine");
    }
    let store = UsageStore::in_data_dir(data_dir).with_caps(caps.iter().copied());
    let limits = [Harness::Claude, Harness::Codex]
        .into_iter()
        .map(|harness| {
            let limit = match store.standing(harness, machine_usage::ms(Timestamp::now())) {
                Ok(standing) => match standing {
                    Standing::Free => Limit::Free,
                    Standing::ProbeDue => Limit::ProbeDue,
                    Standing::Limited { until_ms } => Limit::Until(machine_usage::at(until_ms)),
                },
                Err(why) => Limit::Unread(why),
            };
            (harness, limit)
        })
        .collect();
    let slots = MachineSlots::new(data_dir, concurrent_runs)
        .holders()
        .ok()
        .map(|held| (held, concurrent_runs));
    let bases = match base::read(dirs) {
        Ok(checks) => checks.standing().cloned().collect(),
        Err(why) => {
            notes.push(format!("the base checks cannot be read: {why}"));
            Vec::new()
        }
    };
    Ok(Report {
        project: project.to_owned(),
        only,
        folded,
        stages,
        quarantined,
        notes,
        limits,
        slots,
        bases,
    })
}

fn stage_text(status: Status) -> String {
    let stage: Stage = status.stage();
    let name = match serde_json::to_value(stage) {
        Ok(Value::String(name)) => name,
        _ => format!("{stage:?}"),
    };
    match status {
        Status::Active(_) => name.to_owned(),
        Status::NeedsInput { .. } => format!("{name}, needs input"),
        Status::Parked { .. } => format!("{name}, parked"),
    }
}

/// The report as text, safe for a terminal.
pub fn render(report: &Report) -> String {
    let mut out = String::new();
    let folded = &report.folded;
    let stage = |ticket: &TicketId| {
        report
            .stages
            .get(ticket)
            .map_or_else(String::new, |stage| format!(" [{stage}]"))
    };
    match &report.only {
        Some(ticket) => {
            let history = folded.history.get(ticket).map(Vec::as_slice).unwrap_or(&[]);
            let _ = writeln!(out, "{ticket}{} in {}", stage(ticket), report.project);
            if history.is_empty() {
                let _ = writeln!(out, "  no wait recorded for it");
            }
            for wait in history {
                let _ = writeln!(out, "  {} waits: {}", wait.at, phrase(wait));
            }
            match folded.current.get(ticket) {
                Some(wait) => {
                    let _ = writeln!(out, "now: {}", phrase(wait));
                }
                None if !history.is_empty() => {
                    let _ = writeln!(out, "now: not waiting (it was dispatched or ran since)");
                }
                None => {}
            }
        }
        None => {
            let _ = writeln!(out, "{}", report.project);
            if folded.current.is_empty() {
                let _ = writeln!(out, "  no admitted ticket waits");
            }
            for (ticket, wait) in &folded.current {
                let _ = writeln!(
                    out,
                    "  {ticket}{} last told {}: {}",
                    stage(ticket),
                    wait.at,
                    phrase(wait)
                );
            }
            // A ticket that waits for a person keeps what it holds
            // (architecture, section 5): named from its ref.
            for (ticket, stage) in &report.stages {
                if !folded.current.contains_key(ticket)
                    && (stage.ends_with("needs input") || stage.ends_with("parked"))
                {
                    let _ = writeln!(
                        out,
                        "  {ticket} [{stage}]: waits for a person (an answer, or \
                         `owlshift continue`) and keeps what it holds"
                    );
                }
            }
            if !folded.current.is_empty() {
                let _ = writeln!(
                    out,
                    "  (reasons are read from the events: the latest one told, until the \
                     ticket is dispatched or runs)"
                );
            }
        }
    }
    if report.quarantined {
        let _ = writeln!(
            out,
            "quarantine: a marker stands; every command refuses the project"
        );
    }
    let people: Vec<_> = folded
        .people
        .iter()
        .filter(|(ticket, _)| report.only.as_ref().is_none_or(|only| only == *ticket))
        .collect();
    if !people.is_empty() {
        let _ = writeln!(out, "people's tickets left out for want of zones:");
        for (ticket, at) in people {
            let _ = writeln!(
                out,
                "  {ticket}: in the working state with neither a pull request nor a `zone:` \
                 label, not counted in flight (warned {at}; read from the events, it may have \
                 changed since)"
            );
        }
    }
    match &report.slots {
        Some((held, count)) => {
            let _ = writeln!(out, "machine slots: {} of {count} in use", held.len());
            for HeldSlot { slot, holder } in held {
                match holder {
                    Some(holder) => {
                        let _ = writeln!(
                            out,
                            "  slot {slot}: {} {} since {} (pid {})",
                            holder.project, holder.ticket, holder.since, holder.pid
                        );
                    }
                    None => {
                        let _ = writeln!(out, "  slot {slot}: held, holder unknown");
                    }
                }
            }
        }
        None => {
            let _ = writeln!(out, "machine slots: cannot be counted");
        }
    }
    for (harness, limit) in &report.limits {
        let shown = machine_usage::shown(*harness);
        match limit {
            Limit::Free => {}
            Limit::ProbeDue => {
                let _ = writeln!(
                    out,
                    "{shown}: no reset time was given and its deadline passed"
                );
            }
            Limit::Until(until) => {
                let _ = writeln!(out, "{shown}: at its usage limit until {until}");
            }
            Limit::Unread(why) => {
                let _ = writeln!(out, "{shown}: its usage file cannot be read: {why}");
            }
        }
    }
    if report
        .limits
        .iter()
        .all(|(_, limit)| matches!(limit, Limit::Free))
    {
        let _ = writeln!(out, "no harness is limited");
    }
    for check in &report.bases {
        render_base(&mut out, check);
    }
    for note in &report.notes {
        let _ = writeln!(out, "note: {note}");
    }
    printable(&out)
}

/// A base check that holds the project's Builds (OWL-418): its commit, its
/// command, the end of its output and the re-check.
fn render_base(out: &mut String, check: &Check) {
    let short = base::short(&check.commit);
    match &check.entry {
        Entry::Pending { failing, .. } => {
            let _ = writeln!(
                out,
                "base {short}: a check of its {} command `{}` is due, once no other ticket's \
                 step runs; the Builds made from it wait",
                base::step_name(failing.step),
                failing.command
            );
        }
        Entry::Checked {
            verdict: Verdict::Failed(failure),
            hold: Some(hold),
            checked_ms,
            ..
        } => {
            let at = |ms: i64| Timestamp::from_millisecond(ms).unwrap_or(Timestamp::UNIX_EPOCH);
            let _ = writeln!(
                out,
                "base {short}: fails its {} command `{}` ({}) since {}, last checked {}; the \
                 Builds made from it wait, and a branch with commits of its own on it is rebased \
                 onto the default branch's head once that head passes, unless its pull request \
                 is open: a person rebases that one",
                base::step_name(failure.failing.step),
                failure.failing.command,
                failure.reason,
                at(hold.since_ms),
                at(*checked_ms)
            );
            let lines: Vec<&str> = failure.tail.lines().collect();
            for line in &lines[lines.len().saturating_sub(base::WHY_LINES)..] {
                let _ = writeln!(out, "    {line}");
            }
            let _ = writeln!(
                out,
                "  once fixed: `owlshift gate --at {}`, or the next commit on the default branch",
                check.commit
            );
        }
        Entry::Checked { .. } => {}
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::events::data;
    use crate::slots::Holder;

    fn event(at: &str, ticket: Option<&str>, kind: EventKind, details: Data) -> Event {
        Event {
            format: Default::default(),
            at: at.parse().unwrap(),
            project: "o/r".to_owned(),
            ticket: ticket.map(|t| TicketId::new(t).unwrap()),
            run: None,
            kind,
            data: details,
        }
    }

    fn waits(at: &str, ticket: &str, details: Data) -> Event {
        event(at, Some(ticket), EventKind::Decision, details)
    }

    #[test]
    fn the_latest_wait_is_current_until_a_dispatch() {
        let events = [
            waits(
                "2026-10-20T10:00:00Z",
                "OWL-1",
                data([
                    ("drain", json!("waits")),
                    ("reason", json!("blocked_by")),
                    ("tickets", json!(["OWL-9"])),
                ]),
            ),
            waits(
                "2026-10-20T10:05:00Z",
                "OWL-1",
                data([
                    ("drain", json!("waits")),
                    ("reason", json!("collision")),
                    ("tickets", json!(["OWL-2"])),
                    ("resource", json!({"zone": "leases/"})),
                ]),
            ),
            waits(
                "2026-10-20T10:06:00Z",
                "OWL-3",
                data([("drain", json!("waits")), ("reason", json!("pr_cap"))]),
            ),
            event(
                "2026-10-20T10:07:00Z",
                Some("OWL-3"),
                EventKind::Dispatch,
                Data::new(),
            ),
        ];
        let folded = fold(&events, "o/r");
        assert_eq!(folded.history[&TicketId::new("OWL-1").unwrap()].len(), 2);
        assert_eq!(folded.current.len(), 1);
        let wait = &folded.current[&TicketId::new("OWL-1").unwrap()];
        assert_eq!(
            phrase(wait),
            "OWL-2 holds zone `leases/`, which it would touch"
        );
    }

    /// OWL-420: a restart left to watch's resume is the ticket's word until
    /// watch tells a wait, which replaces it, or the ticket runs; one whose
    /// round waits says nothing, the stage telling it.
    #[test]
    fn a_restart_left_to_watch_is_named_until_watch_speaks() {
        let id = TicketId::new("OWL-1").unwrap();
        let restart = |ticket, takes| {
            waits(
                "2026-10-21T09:00:00Z",
                ticket,
                data([
                    ("restarted", json!(true)),
                    ("stage", json!("build")),
                    ("to_watch", json!(true)),
                    ("takes", json!(takes)),
                ]),
            )
        };
        let mut events = vec![restart("OWL-1", "resume"), restart("OWL-2", "answers")];
        let folded = fold(&events, "o/r");
        assert_eq!(folded.current.len(), 1);
        assert_eq!(
            phrase(&folded.current[&id]),
            "restarted by `owlshift continue --to-watch`: `owlshift watch` resumes it at its \
             next pass"
        );
        events.push(waits(
            "2026-10-21T09:01:00Z",
            "OWL-1",
            data([
                ("watch", json!("waits")),
                ("reason", json!("slot")),
                ("tickets", json!(["OWL-7"])),
            ]),
        ));
        let folded = fold(&events, "o/r");
        assert_eq!(
            phrase(&folded.current[&id]),
            "the project's slot is held by OWL-7"
        );
        events.push(event(
            "2026-10-21T09:05:00Z",
            Some("OWL-1"),
            EventKind::RunStarted,
            Data::new(),
        ));
        assert!(fold(&events, "o/r").current.is_empty());
    }

    #[test]
    fn a_pull_request_and_a_person_are_named_as_such() {
        let pr = Waiting {
            at: "2026-10-20T10:00:00Z".parse().unwrap(),
            ticket: TicketId::new("OWL-1").unwrap(),
            reason: "collision".to_owned(),
            data: data([
                ("pull_request", json!(12)),
                ("resource", json!({"named": "db"})),
            ]),
        };
        assert_eq!(
            phrase(&pr),
            "pull request #12 holds resource `db`, which it would touch"
        );
        let person = Waiting {
            data: data([
                ("tickets", json!(["OWL-4"])),
                ("person", json!(true)),
                ("resource", json!({"zone": ""})),
            ]),
            ..pr
        };
        assert_eq!(
            phrase(&person),
            "OWL-4, which a person works on, holds the whole repository, which it would touch"
        );
        // A Build's admission names its wait at Build (OWL-448).
        let build = Waiting {
            data: data([
                ("tickets", json!(["OWL-2"])),
                ("stage", json!("build")),
                ("resource", json!({"zone": "app/a.rs"})),
            ]),
            ..person
        };
        assert_eq!(
            phrase(&build),
            "its Build waits: OWL-2 holds zone `app/a.rs`, which its Build would take"
        );
    }

    #[test]
    fn a_warning_makes_a_person_ticket_until_something_else_is_said() {
        let warn = |at, ticket| {
            event(
                at,
                Some(ticket),
                EventKind::Warning,
                data([("what", json!("zones_unknown"))]),
            )
        };
        let events = [
            warn("2026-10-20T10:00:00Z", "OWL-7"),
            warn("2026-10-20T10:00:00Z", "OWL-8"),
            event(
                "2026-10-20T10:01:00Z",
                Some("OWL-8"),
                EventKind::Dispatch,
                Data::new(),
            ),
        ];
        let folded = fold(&events, "o/r");
        assert_eq!(folded.people.len(), 1);
        assert!(folded.people.contains_key(&TicketId::new("OWL-7").unwrap()));
    }

    /// OWL-417: a run that began and never ended (a step child that died,
    /// or a person's `do`, which takes no slot) holds no slot, and a dead
    /// holder's record under a free slot is not a holder.
    #[test]
    fn runs_that_never_ended_and_dead_records_name_no_holder() {
        let tmp = tempfile::tempdir().unwrap();
        let log = crate::events::EventLog::in_dir(tmp.path());
        for (ticket, run) in [("OWL-8", "r1"), ("OWL-9", "r1")] {
            let mut started = event(
                "2026-10-13T16:50:59Z",
                Some(ticket),
                EventKind::RunStarted,
                Data::new(),
            );
            started.run = Some(run.to_owned());
            log.append(&started).unwrap();
        }
        let slots = tmp.path().join("slots");
        fs::create_dir_all(&slots).unwrap();
        fs::write(slots.join("0.lock"), "").unwrap();
        fs::write(
            slots.join("0.holder"),
            r#"{"project":"o/r","ticket":"OWL-8","pid":1,"since":"2026-10-13T16:50:59Z"}"#,
        )
        .unwrap();
        let dirs = ProjectDirs::new(
            tmp.path(),
            &owlshift_adapters::forge::Repo::parse("o/r").unwrap(),
        );
        let report = gather(tmp.path(), &dirs, "o/r", None, 3, &[]).unwrap();
        assert_eq!(report.slots, Some((vec![], 3)));
        let text = render(&report);
        assert!(text.contains("machine slots: 0 of 3 in use"), "{text}");
        assert!(!text.contains("OWL-8") && !text.contains("OWL-9"), "{text}");
    }

    #[test]
    fn a_usage_decision_is_a_usage_limit_wait() {
        let events = [waits(
            "2026-10-20T10:00:00Z",
            "OWL-1",
            data([
                ("usage", json!("waits")),
                ("harness", json!("claude")),
                ("until", json!("2026-10-20T15:00:00Z")),
            ]),
        )];
        let folded = fold(&events, "o/r");
        let wait = &folded.current[&TicketId::new("OWL-1").unwrap()];
        assert_eq!(
            phrase(wait),
            "claude is at its usage limit until 2026-10-20T15:00:00Z"
        );
    }

    /// OWL-418: a Build held for its base is a `base_broken` wait, held or
    /// pending, and the report names the hold, its command, the end of its
    /// output and the re-check.
    #[test]
    fn a_base_hold_is_a_wait_and_the_report_names_it() {
        use owlshift_core::base::{Checks, Failing, Failure, Step};
        let commit = "c".repeat(40);
        let held = |pending: bool| {
            waits(
                "2026-10-22T10:00:00Z",
                "OWL-1",
                data([
                    ("base", json!("waits")),
                    ("reason", json!("base_broken")),
                    ("commit", json!(commit)),
                    ("command", json!("sh ci/owlshift/setup.sh")),
                    ("pending", json!(pending)),
                ]),
            )
        };
        let folded = fold(&[held(false)], "o/r");
        let wait = &folded.current[&TicketId::new("OWL-1").unwrap()];
        assert_eq!(
            phrase(wait),
            "the base its branch was made from, cccccccccccc, fails `sh ci/owlshift/setup.sh` \
             in a fresh worktree too, so its Build waits for that base to pass"
        );
        let folded = fold(&[held(true)], "o/r");
        let wait = &folded.current[&TicketId::new("OWL-1").unwrap()];
        assert!(phrase(wait).contains("is checked once no other ticket's step runs"));

        let failing = Failing {
            step: Step::Setup,
            command: "sh ci/owlshift/setup.sh".to_owned(),
        };
        let mut checks = Checks::default();
        checks.record_check(
            (&commit, "k", "h"),
            1_000,
            Verdict::Failed(Failure {
                failing: failing.clone(),
                reason: "setup: exit status 1".to_owned(),
                tail: "one\ntwo\nERR_PNPM_NO_OFFLINE_TARBALL rollup@4.64.8\n".to_owned(),
            }),
            Some(&failing),
        );
        let mut out = String::new();
        render_base(&mut out, &checks.checks[0]);
        assert!(
            out.contains("base cccccccccccc: fails its setup command `sh ci/owlshift/setup.sh`")
                && out.contains("    ERR_PNPM_NO_OFFLINE_TARBALL rollup@4.64.8")
                && out.contains(&format!("`owlshift gate --at {commit}`")),
            "{out}"
        );
    }

    #[test]
    fn the_cap_is_read_as_the_drain_tells_it() {
        // `tell_cap`'s shape: no `waits`, the counts.
        let events = [waits(
            "2026-10-20T10:00:00Z",
            "OWL-1",
            data([
                ("drain", json!("pr_cap")),
                ("open_prs", json!(3)),
                ("max_open_prs", json!(3)),
            ]),
        )];
        let folded = fold(&events, "o/r");
        let wait = &folded.current[&TicketId::new("OWL-1").unwrap()];
        assert_eq!(
            phrase(wait),
            "the open pull requests reached their cap (3 of 3)"
        );
    }

    #[test]
    fn render_names_the_marker_the_slots_the_limits_and_a_waiting_person() {
        let id = |t| TicketId::new(t).unwrap();
        let report = Report {
            project: "o/r".to_owned(),
            only: None,
            folded: Folded::default(),
            stages: HashMap::from([(id("OWL-4"), "verify, parked".to_owned())]),
            quarantined: true,
            notes: vec!["a note".to_owned()],
            limits: vec![
                (
                    Harness::Claude,
                    Limit::Until("2026-10-20T15:00:00Z".parse().unwrap()),
                ),
                (Harness::Codex, Limit::ProbeDue),
            ],
            slots: Some((
                vec![
                    HeldSlot {
                        slot: 0,
                        holder: Some(Holder {
                            project: "other/p".to_owned(),
                            ticket: id("LOC-2"),
                            pid: 7,
                            since: "2026-10-20T09:00:00Z".parse().unwrap(),
                        }),
                    },
                    HeldSlot {
                        slot: 2,
                        holder: None,
                    },
                ],
                3,
            )),
            bases: Vec::new(),
        };
        let text = render(&report);
        assert!(text.contains("quarantine: a marker stands"), "{text}");
        assert!(
            text.contains("OWL-4 [verify, parked]: waits for a person"),
            "{text}"
        );
        assert!(text.contains("machine slots: 2 of 3 in use"), "{text}");
        assert!(
            text.contains("  slot 0: other/p LOC-2 since 2026-10-20T09:00:00Z (pid 7)"),
            "{text}"
        );
        assert!(text.contains("  slot 2: held, holder unknown"), "{text}");
        assert!(text.contains("until 2026-10-20T15:00:00Z"), "{text}");
        assert!(text.contains("deadline passed"), "{text}");
        assert!(text.contains("note: a note"), "{text}");
    }
}
