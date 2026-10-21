//! `owlshift why` (OWL-407, architecture section 5, "Parallel",
//! "`owlshift why`"): why each admitted ticket of a project is not running,
//! read from what Owlshift keeps and nothing else.
//!
//! It reads the event log's `decision` events (`drain`, `watch` or `usage`
//! = `waits`), the project's ticket refs, the machine's usage files, the
//! quarantine marker and the run slots. It takes no lock the writers hold:
//! the project's lock and the tickets' locks are never tried, so it runs
//! while `owlshift watch` does. [`fold`] and [`render`] are pure; [`gather`]
//! does the reading. The one thing it touches that a writer can notice is
//! the slot files: counting the slots held tries each free one for a
//! moment, as watch's own count does (`MachineSlots::held`).

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::Path;

use jiff::Timestamp;
use owlshift_contracts::event::{Event, EventKind};
use owlshift_contracts::ids::TicketId;
use owlshift_core::state::Status;
use owlshift_core::usage::Standing;
use owlshift_core::vocab::{Harness, Stage};
use serde_json::Value;

use crate::events::{Data, printable};
use crate::project::ProjectDirs;
use crate::slots::MachineSlots;
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
    /// Runs started and not ended, in every project of the log, oldest
    /// first: project, ticket, start.
    pub started: Vec<(String, TicketId, Timestamp)>,
}

/// Folds `events`, oldest first, for the project `project`.
pub fn fold(events: &[Event], project: &str) -> Folded {
    let mut folded = Folded::default();
    let mut runs: Vec<(String, String, TicketId, Timestamp)> = Vec::new();
    for event in events {
        match event.kind {
            EventKind::RunStarted => {
                if let (Some(run), Some(ticket)) = (&event.run, &event.ticket) {
                    runs.push((event.project.clone(), run.clone(), ticket.clone(), event.at));
                }
            }
            EventKind::RunEnded => {
                if let Some(run) = &event.run {
                    runs.retain(|(p, r, _, _)| !(p == &event.project && r == run));
                }
            }
            _ => {}
        }
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
    folded.started = runs
        .into_iter()
        .map(|(project, _, ticket, at)| (project, ticket, at))
        .collect();
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
    } else if event.data.get("drain").and_then(Value::as_str) == Some("pr_cap") {
        // The drain tells the cap once, for the first ticket it holds
        // (`tell_cap`), with the counts and no `waits`.
        "pr_cap".to_owned()
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
            format!("{holder} holds {resource}, which it would touch")
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
    /// `(held, concurrent_runs)`; `None` when the slots cannot be counted.
    pub slots: Option<(u32, u32)>,
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
                        RefState::Read(Some(state)) => {
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
    let quarantined = match dirs.unverified() {
        Ok(marker) => marker.is_some(),
        Err(error) => {
            notes.push(format!("the quarantine marker cannot be read: {error}"));
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
        .held()
        .ok()
        .map(|held| (held, concurrent_runs));
    Ok(Report {
        project: project.to_owned(),
        only,
        folded,
        stages,
        quarantined,
        notes,
        limits,
        slots,
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
    match report.slots {
        Some((held, count)) => {
            let _ = writeln!(out, "machine slots: {held} of {count} in use");
        }
        None => {
            let _ = writeln!(out, "machine slots: cannot be counted");
        }
    }
    let running: Vec<_> = folded
        .started
        .iter()
        .rev()
        .take(report.slots.map_or(0, |(held, _)| held as usize))
        .collect();
    for (project, ticket, at) in running.into_iter().rev() {
        let _ = writeln!(
            out,
            "  {project} {ticket} since {at} (from the events: a run that began and never ended)"
        );
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
    for note in &report.notes {
        let _ = writeln!(out, "note: {note}");
    }
    printable(&out)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::events::data;

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

    #[test]
    fn started_runs_without_an_end_are_listed_across_projects() {
        let mut a = event(
            "2026-10-20T10:00:00Z",
            Some("OWL-1"),
            EventKind::RunStarted,
            Data::new(),
        );
        a.run = Some("r1".to_owned());
        let mut b = event(
            "2026-10-20T10:01:00Z",
            Some("OWL-2"),
            EventKind::RunStarted,
            Data::new(),
        );
        b.run = Some("r2".to_owned());
        b.project = "other/p".to_owned();
        let mut end = event(
            "2026-10-20T10:02:00Z",
            Some("OWL-1"),
            EventKind::RunEnded,
            Data::new(),
        );
        end.run = Some("r1".to_owned());
        let folded = fold(&[a, b, end], "o/r");
        assert_eq!(folded.started.len(), 1);
        assert_eq!(folded.started[0].0, "other/p");
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
        let mut report = Report {
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
            slots: Some((1, 2)),
        };
        report.folded.started = vec![
            (
                "o/r".to_owned(),
                id("OWL-8"),
                "2026-10-20T09:00:00Z".parse().unwrap(),
            ),
            (
                "o/r".to_owned(),
                id("OWL-9"),
                "2026-10-20T09:30:00Z".parse().unwrap(),
            ),
        ];
        let text = render(&report);
        assert!(text.contains("quarantine: a marker stands"), "{text}");
        assert!(
            text.contains("OWL-4 [verify, parked]: waits for a person"),
            "{text}"
        );
        assert!(text.contains("machine slots: 1 of 2 in use"), "{text}");
        // One slot held: the latest started run only.
        assert!(text.contains("o/r OWL-9 since"), "{text}");
        assert!(!text.contains("OWL-8"), "{text}");
        assert!(text.contains("until 2026-10-20T15:00:00Z"), "{text}");
        assert!(text.contains("deadline passed"), "{text}");
        assert!(text.contains("note: a note"), "{text}");
    }
}
