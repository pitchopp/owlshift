//! A ticket's page, as one snapshot (P8 part 6, OWL-471, runtime and
//! operations, "Observability & the local web UI", "Ticket"): everything the
//! page draws of a ticket but its runs' logs, read from the files the runner
//! keeps and never from the client.
//!
//! The snapshot is versioned ([`FORMAT`]) and holds, each as a section that
//! answers `{"error": why}` in its place when it cannot be read, so one
//! unreadable file never blanks the page:
//!
//! - `timeline`: the ticket ref's history, one entry where the stage,
//!   `waiting`, round, review passes, plan, delivery or park state differs
//!   from the commit before, and whether the newest [`MAX_HISTORY`] commits
//!   left older ones out (`truncated`);
//! - `runs`: each folder of the ticket's runs joined with its `run_started`,
//!   `run_ended` and `usage` events, with whether it `ended` (a `run_ended`
//!   event names it) and how big and how old its log is;
//! - `cost`: the ticket's spend since its last restart against
//!   `policy.ticket_budget_usd` of the project file the project is served on;
//! - `questions`: the asks and the decisions of `questions.json`, and
//!   `reply`, the reply form of the open ask, offered when the operator's
//!   tracker key is its decider's ([`super::reply::section`], OWL-475),
//!   its decider named once the name is read (OWL-517);
//! - `plan`: the current plan, the distinct plans of the history with each
//!   one's diff against the one before, per part, and the review passes;
//! - `waits`: what the ticket waits on, as `owlshift status --waits` tells it;
//! - `actions` (format 2, OWL-473): whether the page offers Restart and
//!   Cancel, each where its command would act, and the cancel that stands.
//!
//! A commit of the ref never changes, so what the history read of one is kept
//! in the [`Cache`] by commit, and the waits, which read the whole event log
//! and every ref of the project, are kept a few seconds: the page asks again
//! every 3 to 10 seconds for as long as it is open.

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

use jiff::Timestamp;
use owlshift_adapters::forge::Repo;
use owlshift_contracts::config::ProjectConfig;
use owlshift_contracts::event::{Event, EventKind};
use owlshift_contracts::ids::TicketId;
use owlshift_contracts::review::{Plan, PlanPart};
use owlshift_core::state::{Status, TicketState};
use serde_json::{Map, Value, json};

use super::events::Reader;
use super::reply::{self, KeyState};
use crate::executor::Git;
use crate::project::ProjectDirs;
use crate::ticket_ref::{self, Commits, Revision, TicketRecord};
use crate::{cancel, served, status};

/// The snapshot's format: 2 adds `actions` (OWL-473).
pub const FORMAT: u32 = 2;

/// The most commits of a ticket ref the timeline reads.
pub const MAX_HISTORY: usize = 400;

/// How long the waits of a ticket are kept before they are read again.
pub const WAITS_KEPT: Duration = Duration::from_secs(5);

/// The most records the cache of commits holds before it starts again.
const MAX_CACHED: usize = 4000;

/// `value` as JSON; null for what has none.
macro_rules! as_json {
    ($value:expr) => {
        serde_json::to_value($value).unwrap_or(Value::Null)
    };
}

type Key = (String, String);

/// What a snapshot keeps between two of its builds.
pub struct Cache {
    /// The records of ticket-ref commits, by project, ticket and commit.
    records: Mutex<HashMap<(String, String, String), TicketRecord>>,
    /// The waits of a ticket and when they were read.
    waits: Mutex<HashMap<Key, (Instant, Value)>>,
    waits_kept: Duration,
}

impl Cache {
    /// A cache that keeps the waits of a ticket for `waits_kept`.
    pub fn new(waits_kept: Duration) -> Self {
        Self {
            records: Mutex::new(HashMap::new()),
            waits: Mutex::new(HashMap::new()),
            waits_kept,
        }
    }
}

fn locked<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// What a snapshot is read with.
pub struct Inputs<'a> {
    pub data: &'a Path,
    pub git: &'a Git,
    pub events: &'a Reader,
    pub cache: &'a Cache,
    /// The machine's run slots, for the waits.
    pub concurrent_runs: u32,
    pub now: Timestamp,
    /// The project's tracker key, for the reply form (OWL-475).
    pub key: &'a KeyState,
    /// The name read so far of a decider's account, which a read key that
    /// is not theirs is asked for (OWL-517): never waits on a read.
    pub names: &'a dyn Fn(&str) -> Option<String>,
}

/// Whether `text` is a run's folder name, as `new_run_dir` makes it:
/// `YYYYMMDDTHHMMSSZ`, then `-` and digits when the second was taken.
pub fn is_run_id(text: &str) -> bool {
    let (stamp, suffix) = match text.split_once('-') {
        Some((stamp, suffix)) => (stamp, Some(suffix)),
        None => (text, None),
    };
    let bytes = stamp.as_bytes();
    bytes.len() == 16
        && bytes[..8].iter().all(u8::is_ascii_digit)
        && bytes[8] == b'T'
        && bytes[9..15].iter().all(u8::is_ascii_digit)
        && bytes[15] == b'Z'
        && suffix
            .is_none_or(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
}

fn section(read: Result<Value, String>) -> Value {
    read.unwrap_or_else(|why| json!({ "error": why }))
}

/// The snapshot of `ticket` of `repo`.
pub fn snapshot(inputs: &Inputs, repo: &Repo, ticket: &TicketId) -> Value {
    let dirs = ProjectDirs::new(inputs.data, repo);
    let history = revisions(inputs, repo, &dirs, ticket);
    let latest: Result<Option<&TicketRecord>, String> = match &history {
        Err(why) => Err(why.clone()),
        Ok((revisions, _)) => match revisions.last() {
            None => Ok(None),
            Some(revision) => match &revision.record {
                Ok(record) => Ok(Some(record)),
                Err(why) => Err(why.clone()),
            },
        },
    };
    let on_record = |build: &dyn Fn(&TicketRecord) -> Value| -> Value {
        match &latest {
            Ok(Some(record)) => build(record),
            Ok(None) => Value::Null,
            Err(why) => json!({ "error": why }),
        }
    };
    let timeline = match &history {
        Ok((revisions, truncated)) => timeline(revisions, *truncated),
        Err(why) => json!({ "error": why }),
    };
    let plan = match (&history, &latest) {
        (Ok((revisions, _)), Ok(record)) => plan_section(revisions, *record),
        (_, Err(why)) => json!({ "error": why }),
        (Err(why), _) => json!({ "error": why }),
    };
    json!({
        "format": FORMAT,
        "project": repo.to_string(),
        "ticket": ticket.as_str(),
        "at": inputs.now.to_string(),
        "ref": matches!(&latest, Ok(Some(_)) | Err(_)),
        "timeline": timeline,
        "runs": section(runs(inputs, repo, &dirs, ticket)),
        "cost": on_record(&|record| cost(&dirs, record)),
        "questions": on_record(&|record| questions(record, inputs, ticket)),
        "plan": plan,
        "waits": section(waits(inputs, repo, ticket)),
        "actions": actions(&dirs, ticket, &latest),
    })
}

/// What the page offers on the ticket (OWL-473): `restart` while its latest
/// record is parked, `owlshift continue --to-watch`'s own case; `cancel`
/// when `owlshift cancel` would write a cancel of it now
/// ([`cancel::decide`]), `cancel_refusal` saying why not otherwise; and
/// `cancel_standing`, the cancel that stands on it (`by`, `at`, `why`, or
/// `{"unreadable": true}`), or null.
fn actions(
    dirs: &ProjectDirs,
    ticket: &TicketId,
    latest: &Result<Option<&TicketRecord>, String>,
) -> Value {
    let (restart, cancel) = match latest {
        Err(why) => (false, Err(why.clone())),
        Ok(record) => {
            let restart = record
                .and_then(|record| TicketState::try_from(&record.state).ok())
                .is_some_and(|state| matches!(state.status(), Status::Parked { .. }));
            let cancel = dirs
                .ticket_lock_held(ticket)
                .map_err(|e| format!("{ticket}'s lock could not be read: {e}"))
                .and_then(|running| cancel::decide(ticket, *record, running));
            (restart, cancel)
        }
    };
    let standing = match cancel::read(dirs, ticket) {
        None => Value::Null,
        Some(Some(request)) => json!({
            "by": request.by,
            "at": request.at.to_string(),
            "why": request.why,
        }),
        Some(None) => json!({ "unreadable": true }),
    };
    json!({
        "restart": restart,
        "cancel": cancel.is_ok(),
        "cancel_refusal": cancel.err(),
        "cancel_standing": standing,
    })
}

/// The history of the ticket ref, oldest first, with the records of the
/// commits it has read before taken from the cache.
fn revisions(
    inputs: &Inputs,
    repo: &Repo,
    dirs: &ProjectDirs,
    ticket: &TicketId,
) -> Result<(Vec<Revision>, bool), String> {
    let checkout = dirs.checkout();
    if !checkout.is_dir() {
        return Ok((Vec::new(), false));
    }
    let Commits {
        mut listed,
        truncated,
    } = ticket_ref::commits(inputs.git, &checkout, ticket, MAX_HISTORY)?;
    listed.reverse();
    let name = (
        repo.to_string().to_ascii_lowercase(),
        ticket.as_str().to_owned(),
    );
    let key = |commit: &str| (name.0.clone(), name.1.clone(), commit.to_owned());
    let missing: Vec<String> = {
        let cached = locked(&inputs.cache.records);
        listed
            .iter()
            .filter(|(commit, _)| !cached.contains_key(&key(commit)))
            .map(|(commit, _)| commit.clone())
            .collect()
    };
    let mut fresh: HashMap<String, Result<TicketRecord, String>> = HashMap::new();
    if !missing.is_empty() {
        let read = ticket_ref::read_commits(inputs.git, &checkout, ticket, &missing);
        let mut cached = locked(&inputs.cache.records);
        if cached.len() > MAX_CACHED {
            cached.clear();
        }
        for (commit, record) in missing.into_iter().zip(read) {
            // A commit that read is final; one that did not may be a git
            // that failed for a moment.
            if let Ok(record) = &record {
                cached.insert(key(&commit), record.clone());
            }
            fresh.insert(commit, record);
        }
    }
    let cached = locked(&inputs.cache.records);
    let revisions = listed
        .into_iter()
        .map(|(commit, at)| {
            let record = match cached.get(&key(&commit)) {
                Some(record) => Ok(record.clone()),
                None => fresh
                    .remove(&commit)
                    .unwrap_or_else(|| Err("the commit was not read".to_owned())),
            };
            Revision { commit, at, record }
        })
        .collect();
    Ok((revisions, truncated))
}

/// What the timeline compares between two commits.
#[derive(PartialEq)]
struct Marks {
    stage: Value,
    waiting: Value,
    round: u32,
    review_passes: u32,
    plan: Option<String>,
    delivery: Option<String>,
}

fn marks(record: &TicketRecord) -> Marks {
    Marks {
        stage: as_json!(&record.state.stage),
        waiting: as_json!(&record.state.waiting),
        round: record.state.round,
        review_passes: record.state.review_passes,
        plan: record.plan.as_ref().map(|plan| plan.plan.canonical()),
        delivery: record.delivery.as_ref().map(|delivery| delivery.render()),
    }
}

/// The names of what differs between `before` and `after`.
fn changed(before: &Marks, after: &Marks) -> Vec<&'static str> {
    let mut names = Vec::new();
    if before.stage != after.stage {
        names.push("stage");
    }
    if before.waiting != after.waiting {
        names.push("waiting");
    }
    if before.round != after.round {
        names.push("round");
    }
    if before.review_passes != after.review_passes {
        names.push("review_passes");
    }
    if before.plan != after.plan {
        names.push("plan");
    }
    if before.delivery != after.delivery {
        names.push("delivery");
    }
    names
}

/// The timeline of `revisions`, oldest first: an entry for the first, and
/// for each that differs from the one before it.
fn timeline(revisions: &[Revision], truncated: bool) -> Value {
    let mut entries = Vec::new();
    let mut before: Option<Marks> = None;
    for revision in revisions {
        match &revision.record {
            Err(why) => {
                entries.push(json!({
                    "commit": revision.commit,
                    "at": revision.at,
                    "error": why,
                }));
                before = None;
            }
            Ok(record) => {
                let now = marks(record);
                let names = match &before {
                    None => vec!["first"],
                    Some(before) => changed(before, &now),
                };
                if !names.is_empty() {
                    entries.push(json!({
                        "commit": revision.commit,
                        "at": revision.at,
                        "stage": now.stage,
                        "waiting": now.waiting,
                        "round": now.round,
                        "reasks": record.state.reasks,
                        "failed_runs": record.state.failed_runs,
                        "review_passes": now.review_passes,
                        "has_plan": record.plan.is_some(),
                        "delivery": record.delivery.as_ref().map_or(Value::Null, |d| as_json!(d)),
                        "changed": names,
                    }));
                }
                before = Some(now);
            }
        }
    }
    json!({ "entries": entries, "truncated": truncated, "max": MAX_HISTORY })
}

/// The seconds from `time` to `now`, 0 for a time in the future.
fn seconds_since(now: Timestamp, time: SystemTime) -> u64 {
    let now = SystemTime::from(now);
    now.duration_since(time).map_or(0, |span| span.as_secs())
}

fn text_of<'a>(data: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    data.get(key).and_then(Value::as_str)
}

/// What a `usage` event measured, under the names the page reads: `input`,
/// `output`, `cache_read` and `cache_write` in counts of what the harness
/// reports, `cost_usd`, `turns`, `duration_ms` and `models`.
fn usage_of(data: &Map<String, Value>) -> Value {
    let mut usage = Map::new();
    for (from, to) in [
        ("input_tokens", "input"),
        ("output_tokens", "output"),
        ("cache_read_input_tokens", "cache_read"),
        ("cache_creation_input_tokens", "cache_write"),
        ("cost_usd", "cost_usd"),
        ("num_turns", "turns"),
        ("duration_ms", "duration_ms"),
        ("models", "models"),
    ] {
        if let Some(value) = data.get(from) {
            usage.insert(to.into(), value.clone());
        }
    }
    Value::Object(usage)
}

/// A log's size and the seconds since it last grew: `None` when there is
/// no such regular file.
fn log_facts(path: &Path, now: Timestamp) -> Value {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_file() => json!({
            "size": meta.len(),
            "idle_s": meta.modified().map_or(Value::Null, |m| json!(seconds_since(now, m))),
        }),
        _ => Value::Null,
    }
}

/// The ticket's runs: every folder of its runs and every run its events
/// name, oldest first.
fn runs(
    inputs: &Inputs,
    repo: &Repo,
    dirs: &ProjectDirs,
    ticket: &TicketId,
) -> Result<Value, String> {
    let folder = dirs.runs(ticket);
    let mut folders = Vec::new();
    match fs::read_dir(&folder) {
        Ok(entries) => {
            for entry in entries {
                let entry = entry.map_err(|e| format!("{}: {e}", folder.display()))?;
                let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                    continue;
                };
                let real = fs::symlink_metadata(entry.path()).is_ok_and(|m| m.is_dir());
                if real && is_run_id(&name) {
                    folders.push(name);
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("{}: {error}", folder.display())),
    }
    let rows = inputs.events.rows(&repo.to_string(), ticket)?;
    let mut ids: Vec<String> = folders.clone();
    for event in &rows.events {
        if let Some(run) = &event.run
            && !ids.contains(run)
        {
            ids.push(run.clone());
        }
    }
    ids.sort();
    let mut runs = Vec::new();
    for id in ids {
        let of_run = |kind: EventKind| -> Option<&Event> {
            rows.events
                .iter()
                .rev()
                .find(|event| event.kind == kind && event.run.as_deref() == Some(id.as_str()))
        };
        let started = of_run(EventKind::RunStarted);
        let ended = of_run(EventKind::RunEnded);
        let usage = of_run(EventKind::Usage);
        let mut run = Map::new();
        run.insert("id".into(), json!(id));
        run.insert("folder".into(), json!(folders.contains(&id)));
        run.insert("ended".into(), json!(ended.is_some()));
        if let Some(event) = started {
            run.insert("started_at".into(), json!(event.at.to_string()));
            for key in [
                "role",
                "attempt",
                "resumes_plan",
                "gate_failure",
                "result_refusal",
            ] {
                if let Some(value) = event.data.get(key) {
                    run.insert(key.into(), value.clone());
                }
            }
        }
        if let Some(event) = ended {
            run.insert("ended_at".into(), json!(event.at.to_string()));
            for key in [
                "outcome",
                "status",
                "summary",
                "reason",
                "gate",
                "exit_code",
                "elapsed_ms",
                "wall_ms",
            ] {
                if let Some(value) = event.data.get(key) {
                    run.insert(key.into(), value.clone());
                }
            }
        }
        if let Some(event) = usage {
            run.insert("usage".into(), usage_of(&event.data));
        }
        let dir = folder.join(&id);
        run.insert(
            "stdout".into(),
            log_facts(&dir.join("stdout.log"), inputs.now),
        );
        run.insert(
            "stderr".into(),
            log_facts(&dir.join("stderr.log"), inputs.now),
        );
        runs.push(Value::Object(run));
    }
    let warnings: Vec<Value> = rows
        .events
        .iter()
        .filter(|event| event.kind == EventKind::Warning)
        .map(|event| {
            json!({
                "at": event.at.to_string(),
                "run": event.run,
                "what": text_of(&event.data, "what"),
                "why": text_of(&event.data, "why"),
            })
        })
        .collect();
    Ok(json!({ "runs": runs, "warnings": warnings, "unparsable_events": rows.unparsable }))
}

/// `micros` as dollars.
fn usd(micros: u64) -> f64 {
    micros as f64 / 1_000_000.0
}

/// The project file the project of `dirs` is served on, as it was kept, or
/// why there is none to read: what the ticket's budget and the reply's key
/// ([`super::reply::open_key`]) are read from.
pub(crate) fn served_config(dirs: &ProjectDirs) -> Result<ProjectConfig, String> {
    let kept = match served::read_record(dirs) {
        Err(why) => return Err(format!("the served project file is unreadable: {why}")),
        Ok(record) => record.and_then(|record| record.kept),
    };
    let Some(kept) = kept else {
        return Err("no project file is kept for this project yet".into());
    };
    ProjectConfig::parse(&kept.text)
        .map_err(|error| format!("the project file it is served on does not read: {error}"))
}

/// The ticket's budget in micro-dollars, from the project file the project
/// is served on, and a note when there is none to read.
fn budget(dirs: &ProjectDirs) -> (Option<u64>, Option<String>) {
    match served_config(dirs) {
        Ok(config) => (config.policy.ticket_budget_micros(), None),
        Err(note) => (None, Some(note)),
    }
}

/// What the ticket's runs cost against its budget.
fn cost(dirs: &ProjectDirs, record: &TicketRecord) -> Value {
    let (budget, note) = budget(dirs);
    let Some(cost) = &record.cost else {
        return json!({
            "spent_usd": 0.0,
            "since_restart_usd": 0.0,
            "runs": 0,
            "unreported_runs": 0,
            "budget_usd": budget.map(usd),
            "note": note,
        });
    };
    json!({
        "spent_usd": usd(cost.spent_micros),
        "since_restart_usd": usd(cost.spent_micros.saturating_sub(cost.restarted_at_micros)),
        "restarted_at_usd": usd(cost.restarted_at_micros),
        "runs": cost.runs,
        "unreported_runs": cost.unreported_runs,
        "budget_usd": budget.map(usd),
        "note": note,
    })
}

/// The asks and the decisions of the ticket, and the reply form of its
/// open ask, null when it waits for no answer.
fn questions(record: &TicketRecord, inputs: &Inputs, ticket: &TicketId) -> Value {
    json!({
        "asks": as_json!(&record.questions.asks),
        "decisions": as_json!(&record.questions.decisions),
        "reply": reply::section(record, inputs.key, ticket, inputs.names),
    })
}

/// A plan's items of a list part, as JSON.
fn items(plan: &Plan, part: PlanPart) -> Vec<Value> {
    let value = match part {
        PlanPart::Criteria => as_json!(&plan.criteria),
        PlanPart::Steps => as_json!(&plan.steps),
        PlanPart::Forks => as_json!(&plan.forks),
        PlanPart::Alternatives => as_json!(&plan.alternatives),
        PlanPart::Files => as_json!(&plan.files),
        PlanPart::Risks => as_json!(&plan.risks),
        PlanPart::OutOfScope => as_json!(&plan.out_of_scope),
        PlanPart::Goal | PlanPart::Approach => Value::Null,
    };
    match value {
        Value::Array(items) => items,
        _ => Vec::new(),
    }
}

/// The parts of `after` that differ from `before`: for the goal and the
/// approach both texts, for a list the items added and the items removed
/// (`reordered` when only their order changed).
pub fn diff(before: &Plan, after: &Plan) -> Vec<Value> {
    let mut parts = Vec::new();
    for part in PlanPart::ALL {
        if before.part_canonical(part) == after.part_canonical(part) {
            continue;
        }
        let name = as_json!(&part);
        parts.push(match part {
            PlanPart::Goal => json!({ "part": name, "before": before.goal, "after": after.goal }),
            PlanPart::Approach => {
                json!({ "part": name, "before": before.approach, "after": after.approach })
            }
            _ => {
                let (old, new) = (items(before, part), items(after, part));
                let added: Vec<&Value> = new.iter().filter(|item| !old.contains(item)).collect();
                let removed: Vec<&Value> = old.iter().filter(|item| !new.contains(item)).collect();
                let reordered = added.is_empty() && removed.is_empty();
                json!({ "part": name, "added": added, "removed": removed, "reordered": reordered })
            }
        });
    }
    parts
}

/// The plan section: the current plan with its review passes, and the
/// distinct plans of the history with each one's diff against the one
/// before.
fn plan_section(revisions: &[Revision], record: Option<&TicketRecord>) -> Value {
    let mut distinct: Vec<(&Revision, &Plan)> = Vec::new();
    for revision in revisions {
        let Ok(record) = &revision.record else {
            continue;
        };
        let Some(plan) = &record.plan else {
            continue;
        };
        if distinct
            .last()
            .is_none_or(|(_, last)| last.canonical() != plan.plan.canonical())
        {
            distinct.push((revision, &plan.plan));
        }
    }
    let mut shown = Vec::new();
    for (at, (revision, plan)) in distinct.iter().enumerate() {
        let diff = at
            .checked_sub(1)
            .map(|before| diff(distinct[before].1, plan));
        shown.push(json!({
            "commit": revision.commit,
            "at": revision.at,
            "plan": as_json!(plan),
            "diff": diff,
        }));
    }
    let Some(record) = record else {
        return json!({ "current": null, "revisions": shown, "review": null });
    };
    json!({
        "current": record.plan.as_ref().map(|plan| json!({
            "plan": as_json!(&plan.plan),
            "base": plan.base,
            "approval": as_json!(&plan.approval),
            "published": as_json!(&plan.published),
        })),
        "revisions": shown,
        "review": record.review.as_ref().map(|review| json!({
            "passes": as_json!(&review.passes),
            "answered_reviews": review.answered_reviews,
        })),
    })
}

/// What the ticket waits on.
fn waits(inputs: &Inputs, repo: &Repo, ticket: &TicketId) -> Result<Value, String> {
    let key = (
        repo.to_string().to_ascii_lowercase(),
        ticket.as_str().to_owned(),
    );
    if let Some((read_at, value)) = locked(&inputs.cache.waits).get(&key)
        && read_at.elapsed() < inputs.cache.waits_kept
    {
        return Ok(value.clone());
    }
    let read = status::waits::gather(inputs.data, Some(repo), inputs.concurrent_runs, inputs.now);
    let built = status::waits::build(&read);
    let name = repo.to_string();
    let project = built
        .projects
        .iter()
        .find(|project| project.repository.eq_ignore_ascii_case(&name));
    let value = json!({
        "ticket": project
            .and_then(|project| project.tickets.iter().find(|row| row.ticket == *ticket))
            .map_or(Value::Null, |row| as_json!(row)),
        "project": project.map(|project| json!({
            "in_flight": project.in_flight,
            "max_in_flight": project.max_in_flight,
            "quarantined": project.quarantined,
            "paused": as_json!(&project.paused),
        })),
        "notes": built.notes,
    });
    locked(&inputs.cache.waits).insert(key, (Instant::now(), value.clone()));
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(goal: &str, steps: &[&str]) -> Plan {
        crate::ticket_ref::fixtures::plan(goal, steps).plan
    }

    #[test]
    fn a_run_id_is_what_a_run_folder_is_named() {
        for good in [
            "20261105T100000Z",
            "20261105T100000Z-1",
            "20261105T100000Z-23",
        ] {
            assert!(is_run_id(good), "{good}");
        }
        for bad in [
            "",
            "..",
            "../x",
            "20261105T100000Z-",
            "20261105T100000Z-a",
            "20261105T100000",
            "20261105T10000Z",
            "2026110ST100000Z",
            "20261105T100000Z/x",
            "20261105T100000Z-1-2",
            "x20261105T100000Z",
        ] {
            assert!(!is_run_id(bad), "{bad}");
        }
    }

    #[test]
    fn a_plan_diff_names_the_parts_and_the_items_that_changed() {
        let before = plan("goal", &["a", "b"]);
        let mut after = plan("goal", &["b", "c"]);
        after.approach = "another way".to_owned();
        let parts = diff(&before, &after);
        let names: Vec<&str> = parts.iter().map(|p| p["part"].as_str().unwrap()).collect();
        assert_eq!(names, ["approach", "steps"]);
        assert_eq!(parts[0]["before"], "directly");
        assert_eq!(parts[0]["after"], "another way");
        assert_eq!(parts[1]["added"].as_array().unwrap().len(), 1);
        assert_eq!(parts[1]["added"][0]["what"], "c");
        assert_eq!(parts[1]["removed"][0]["what"], "a");
        assert_eq!(parts[1]["reordered"], false);
        let reordered = diff(&plan("goal", &["a", "b"]), &plan("goal", &["b", "a"]));
        assert_eq!(reordered.len(), 1);
        assert_eq!(reordered[0]["reordered"], true);
        assert!(diff(&before, &before).is_empty());
    }

    use crate::events::{Data, EventLog, data};
    use crate::ticket_ref::fixtures::{init_at, put, record};
    use owlshift_contracts::format::Format;
    use owlshift_contracts::refs::{PersistedState, TicketCost, TicketQuestions};
    use owlshift_contracts::review::TicketReview;
    use owlshift_contracts::service::{KeptFile, ServedConfig};
    use owlshift_core::vocab::Stage;
    use std::path::PathBuf;

    const BUDGET_FILE: &str = r#"
        requires = ">=0.1"
        [tracker]
        kind = "markdown"
        admit = { label = "owlshift" }
        states = { ready = "Todo", working = "Doing", needs_input = "Asked", review = "Review" }
        [stack]
        gate = ["make test"]
        [pipeline]
        default = "trivial"
        plan_approval = "never"
        [models]
        [policy]
        always_human = []
        ticket_budget_usd = 5
    "#;

    const QUESTION: &str = r#"{"id":"Q1","category":"scope","context":"c","text":"Which one?",
        "options":[{"label":"Yes","detail":"the long way"},{"label":"No"}],
        "recommendation":{"option":"A","why":"because it is safer"}}"#;

    const WAITING: &str = r#"{"format":4,"stage":"build","waiting":"needs_input","round":1,
        "reasks":0,"failed_runs":0,"fix_passes":0,"repairs":0,"review_passes":0}"#;

    const FIRST_RUN: &str = "20261105T100000Z";
    const SECOND_RUN: &str = "20261105T110000Z";

    fn questions_text() -> String {
        format!(
            r#"{{"format":18,"asks":[{{"kind":"questions","round":1,"at":"2026-11-05T09:00:00Z",
            "comment":"c1","questions":[{QUESTION}],
            "decider":{{"account":"maintainer","by":"assignee"}}}}],
            "decisions":[{{"at":"2026-11-05T09:30:00Z","comment":"d1","question":{QUESTION},
            "decision":"go the long way","basis":"the ticket says so"}}]}}"#
        )
    }

    fn review_text() -> String {
        r#"{"format":1,"passes":[{"stage":"design_review","pass":1,"findings":[
            {"finding":{"id":"F1","severity":"minor","claim":"a claim","evidence":"e",
            "settle":"s"},
            "reviewer":{"role":"design_review","mandates":["critic"],"harness":"claude",
            "model":"m"}}]}]}"#
            .to_owned()
    }

    fn cost_text(spent: u64, restarted: u64) -> TicketCost {
        TicketCost::parse(&format!(
            r#"{{"format":1,"runs":3,"unreported_runs":0,"spent_micros":{spent},
            "restarted_at_micros":{restarted}}}"#
        ))
        .unwrap()
    }

    struct Bench {
        data: tempfile::TempDir,
        repo: Repo,
        dirs: ProjectDirs,
        git: Git,
        checkout: PathBuf,
        events: Reader,
        cache: Cache,
    }

    impl Bench {
        /// A served project whose project file is `file`, kept.
        fn new(file: Option<&str>) -> Self {
            let data = tempfile::tempdir().unwrap();
            let repo = Repo::parse("o/r").unwrap();
            let dirs = ProjectDirs::new(data.path(), &repo);
            let checkout = dirs.checkout();
            let git = init_at(&checkout);
            served::Registry::in_data_dir(data.path())
                .register(&repo, &checkout)
                .unwrap();
            let mut record = ServedConfig::new("o/r");
            record.kept = file.map(|text| KeptFile {
                branch: "main".to_owned(),
                commit: "a".repeat(40),
                text: text.to_owned(),
                since: "2026-11-01T00:00:00Z".parse().unwrap(),
            });
            fs::write(served::record_file(&dirs), record.render()).unwrap();
            Self {
                events: Reader::new(data.path()),
                cache: Cache::new(Duration::ZERO),
                data,
                repo,
                dirs,
                git,
                checkout,
            }
        }

        fn write(&self, ticket: &str, record: &TicketRecord, previous: Option<&str>) -> String {
            put(
                &self.git,
                &self.checkout,
                &TicketId::new(ticket).unwrap(),
                record,
                previous,
            )
        }

        fn snapshot(&self, ticket: &str) -> Value {
            self.snapshot_with(ticket, &KeyState::Unavailable("no key on the bench".into()))
        }

        /// The snapshot with the project's key `key`.
        fn snapshot_with(&self, ticket: &str, key: &KeyState) -> Value {
            let inputs = Inputs {
                data: self.data.path(),
                git: &self.git,
                events: &self.events,
                cache: &self.cache,
                concurrent_runs: 2,
                now: Timestamp::now(),
                key,
                names: &|_| None,
            };
            snapshot(&inputs, &self.repo, &TicketId::new(ticket).unwrap())
        }

        fn event(&self, ticket: &str, run: &str, kind: EventKind, details: Data) {
            let event = Event {
                format: Format,
                at: "2026-11-05T10:00:00Z".parse().unwrap(),
                project: "o/r".to_owned(),
                ticket: Some(TicketId::new(ticket).unwrap()),
                run: Some(run.to_owned()),
                kind,
                data: details,
            };
            EventLog::in_dir(self.data.path()).append(&event).unwrap();
        }

        fn run_folder(&self, ticket: &str, run: &str, stdout: &str) {
            let dir = self.dirs.runs(&TicketId::new(ticket).unwrap()).join(run);
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("stdout.log"), stdout).unwrap();
        }
    }

    /// A ticket that went through Design twice, a Build, and into a round of
    /// questions, with a review pass, a decision and a cost, its refs and
    /// its runs written the way the runner writes them.
    fn busy() -> Bench {
        let bench = Bench::new(Some(BUDGET_FILE));
        let mut one = record(Stage::Design);
        let c1 = bench.write("OWL-1", &one, None);
        one.plan = Some(crate::ticket_ref::fixtures::plan("goal", &["a", "b"]));
        let c2 = bench.write("OWL-1", &one, Some(&c1));
        let mut second_plan = crate::ticket_ref::fixtures::plan("goal", &["a", "c"]);
        second_plan.plan.approach = "another way".to_owned();
        one.plan = Some(second_plan);
        one.review = Some(TicketReview::parse(&review_text()).unwrap());
        let c3 = bench.write("OWL-1", &one, Some(&c2));
        let mut build = record(Stage::Build);
        build.plan = one.plan.clone();
        build.review = one.review.clone();
        build.cost = Some(cost_text(3_000_000, 0));
        let c4 = bench.write("OWL-1", &build, Some(&c3));
        // Only the cost moves: no entry of the timeline.
        build.cost = Some(cost_text(3_200_000, 0));
        let c5 = bench.write("OWL-1", &build, Some(&c4));
        build.state = PersistedState::parse(WAITING).unwrap();
        build.questions = TicketQuestions::parse(&questions_text()).unwrap();
        bench.write("OWL-1", &build, Some(&c5));
        // Another ticket that waits too: its wait is not this one's.
        let mut other = record(Stage::Build);
        other.state = PersistedState::parse(WAITING).unwrap();
        other.questions = TicketQuestions::parse(&questions_text()).unwrap();
        bench.write("OWL-2", &other, None);

        bench.run_folder("OWL-1", FIRST_RUN, "{\"type\":\"turn.started\"}\n");
        bench.run_folder("OWL-1", SECOND_RUN, "");
        bench.event(
            "OWL-1",
            FIRST_RUN,
            EventKind::RunStarted,
            data([("role", json!("build")), ("attempt", json!(1))]),
        );
        bench.event(
            "OWL-1",
            FIRST_RUN,
            EventKind::Usage,
            data([
                ("input_tokens", json!(10)),
                ("output_tokens", json!(20)),
                ("cost_usd", json!(0.12)),
            ]),
        );
        bench.event(
            "OWL-1",
            FIRST_RUN,
            EventKind::RunEnded,
            data([
                ("outcome", json!("finished")),
                ("summary", json!("Added the file.")),
                ("elapsed_ms", json!(900)),
            ]),
        );
        bench.event(
            "OWL-1",
            SECOND_RUN,
            EventKind::RunStarted,
            data([("role", json!("build")), ("attempt", json!(2))]),
        );
        bench
    }

    fn texts<'a>(value: &'a Value, key: &str) -> Vec<&'a str> {
        value
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry[key].as_str().unwrap())
            .collect()
    }

    #[test]
    fn the_timeline_lists_where_the_ticket_changed() {
        let bench = busy();
        let snap = bench.snapshot("OWL-1");
        assert_eq!(snap["format"], 2);
        assert_eq!(snap["project"], "o/r");
        assert_eq!(snap["ticket"], "OWL-1");
        let timeline = &snap["timeline"];
        assert_eq!(timeline["truncated"], false);
        let entries = &timeline["entries"];
        // Six commits, the fifth only a cost.
        assert_eq!(entries.as_array().unwrap().len(), 5, "{entries:#}");
        assert_eq!(
            texts(entries, "stage"),
            ["design", "design", "design", "build", "build"]
        );
        let changed: Vec<Vec<&str>> = entries
            .as_array()
            .unwrap()
            .iter()
            .map(|e| {
                e["changed"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|n| n.as_str().unwrap())
                    .collect()
            })
            .collect();
        assert_eq!(
            changed,
            [
                vec!["first"],
                vec!["plan"],
                vec!["plan"],
                vec!["stage"],
                vec!["waiting", "round"],
            ]
        );
        assert_eq!(entries[4]["waiting"], "needs_input");
        assert_eq!(entries[4]["round"], 1);
    }

    #[test]
    fn the_plan_diff_names_what_changed_between_revisions() {
        let snap = busy().snapshot("OWL-1");
        let plan = &snap["plan"];
        let revisions = plan["revisions"].as_array().unwrap();
        assert_eq!(revisions.len(), 2);
        assert!(revisions[0]["diff"].is_null());
        let parts = revisions[1]["diff"].as_array().unwrap();
        assert_eq!(texts(&revisions[1]["diff"], "part"), ["approach", "steps"]);
        assert_eq!(parts[1]["added"][0]["what"], "c");
        assert_eq!(parts[1]["removed"][0]["what"], "b");
        assert_eq!(plan["current"]["plan"]["approach"], "another way");
        let pass = &plan["review"]["passes"][0];
        assert_eq!(pass["findings"][0]["finding"]["id"], "F1");
        assert_eq!(pass["findings"][0]["finding"]["claim"], "a claim");
    }

    #[test]
    fn the_cost_is_shown_against_the_budget_and_counts_from_the_restart() {
        let bench = busy();
        let cost = &bench.snapshot("OWL-1")["cost"];
        assert_eq!(cost["spent_usd"], 3.2);
        assert_eq!(cost["since_restart_usd"], 3.2);
        assert_eq!(cost["budget_usd"], 5.0);
        assert_eq!(cost["runs"], 3);
        // A restart at 1 dollar: the budget counts what was spent since.
        let tip = ticket_ref::read(
            &bench.git,
            &bench.checkout,
            &TicketId::new("OWL-1").unwrap(),
        )
        .unwrap()
        .unwrap();
        let mut restarted = tip.record.clone();
        restarted.cost = Some(cost_text(3_200_000, 1_000_000));
        bench.write("OWL-1", &restarted, Some(&tip.commit));
        let cost = &bench.snapshot("OWL-1")["cost"];
        assert_eq!(cost["spent_usd"], 3.2);
        assert_eq!(cost["since_restart_usd"], 2.2);
        // No project file kept: no budget, and a note says why.
        let bare = Bench::new(None);
        let mut one = record(Stage::Build);
        one.cost = Some(cost_text(1_000_000, 0));
        bare.write("OWL-1", &one, None);
        let cost = &bare.snapshot("OWL-1")["cost"];
        assert!(cost["budget_usd"].is_null());
        assert!(cost["note"].as_str().unwrap().contains("no project file"));
    }

    #[test]
    fn the_questions_and_decisions_carry_their_options_and_recommendation() {
        let snap = busy().snapshot("OWL-1");
        let ask = &snap["questions"]["asks"][0];
        assert_eq!(ask["decider"]["account"], "maintainer");
        assert_eq!(ask["questions"][0]["text"], "Which one?");
        assert_eq!(ask["questions"][0]["options"][0]["label"], "Yes");
        assert_eq!(ask["questions"][0]["recommendation"]["option"], "A");
        let decision = &snap["questions"]["decisions"][0];
        assert_eq!(decision["decision"], "go the long way");
        assert_eq!(
            decision["question"]["recommendation"]["why"],
            "because it is safer"
        );
    }

    /// The questions section carries the reply form of the open ask: not
    /// offered without a key, offered to the ask's decider, and null for a
    /// ticket that waits for no answer.
    #[test]
    fn the_reply_form_is_the_open_asks() {
        let bench = busy();
        let reply = &bench.snapshot("OWL-1")["questions"]["reply"];
        assert_eq!(reply["offered"], false);
        assert_eq!(reply["approval"], false);
        assert!(reply["link"].is_null());
        assert_eq!(reply["decider"]["account"], "maintainer");
        let snap = bench.snapshot_with("OWL-1", &reply::tests::ready("maintainer"));
        let reply = &snap["questions"]["reply"];
        assert_eq!(reply["offered"], true, "{reply:#}");
        assert_eq!(reply["ask"], "c1");
        let quiet = Bench::new(Some(BUDGET_FILE));
        quiet.write("OWL-1", &record(Stage::Build), None);
        let snap = quiet.snapshot_with("OWL-1", &reply::tests::ready("maintainer"));
        assert!(snap["questions"]["reply"].is_null());
    }

    /// A ticket waiting on its plan's approval: the approval question is the
    /// open ask, and the plan section holds the plan and its diff with the
    /// plan before, which the page draws the form beside.
    #[test]
    fn an_approval_ask_is_offered_beside_the_plan_and_its_diff() {
        use owlshift_contracts::review::Fork;
        let fork = Fork {
            choice: "Markdown".to_owned(),
            options: vec!["Markdown".to_owned(), "Plain text".to_owned()],
            reason: "R".to_owned(),
        };
        let approval = |with_document: bool, plans: usize| {
            let bench = Bench::new(Some(BUDGET_FILE));
            let mut design = record(Stage::Design);
            let mut previous = None;
            for n in 0..plans {
                let mut plan = crate::ticket_ref::fixtures::plan("goal", &["a"]);
                plan.plan.forks = vec![fork.clone()];
                if n > 0 {
                    plan.plan.approach = format!("revision {n}");
                }
                design.plan = Some(plan);
                previous = Some(bench.write("OWL-1", &design, previous.as_deref()));
            }
            let ticket = TicketId::new("OWL-1").unwrap();
            let plan = design.plan.as_ref().unwrap().plan.clone();
            let link = crate::on_demand::PlanLink {
                ticket: &ticket,
                url: Some("https://linear.app/owlshift/document/plan-owl-1"),
            };
            let question =
                crate::on_demand::approval_question(&plan, with_document.then_some(link));
            let question = serde_json::to_string(&question).unwrap();
            design.state = PersistedState::parse(&WAITING.replace("build", "design")).unwrap();
            design.questions = TicketQuestions::parse(&format!(
                r#"{{"format":18,"asks":[{{"kind":"questions","round":1,
                "at":"2026-11-05T09:00:00Z","comment":"c1","questions":[{question}],
                "decider":{{"account":"maintainer","by":"assignee"}}}}],"decisions":[]}}"#
            ))
            .unwrap();
            bench.write("OWL-1", &design, previous.as_deref());
            bench.snapshot_with("OWL-1", &reply::tests::ready("maintainer"))
        };
        let snap = approval(true, 2);
        let reply = &snap["questions"]["reply"];
        assert_eq!(reply["offered"], true);
        assert_eq!(reply["approval"], true);
        assert_eq!(reply["words"], json!({ "Q1": ["B"] }));
        let revisions = snap["plan"]["revisions"].as_array().unwrap();
        let newest = revisions.last().unwrap();
        assert!(!newest["diff"].as_array().unwrap().is_empty());
        assert_eq!(newest["plan"], snap["plan"]["current"]["plan"]);
        // A first plan, without its document: the fork's other option names
        // its change, and there is no plan before, so no diff.
        let snap = approval(false, 1);
        let reply = &snap["questions"]["reply"];
        assert_eq!(reply["approval"], true);
        assert_eq!(reply["words"], json!({ "Q1": [] }));
        assert!(snap["plan"]["revisions"][0]["diff"].is_null());
    }

    #[test]
    fn the_runs_show_their_role_outcome_summary_tokens_and_whether_they_ended() {
        let snap = busy().snapshot("OWL-1");
        let runs = snap["runs"]["runs"].as_array().unwrap();
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0]["id"], FIRST_RUN);
        assert_eq!(runs[0]["role"], "build");
        assert_eq!(runs[0]["outcome"], "finished");
        assert_eq!(runs[0]["summary"], "Added the file.");
        assert_eq!(runs[0]["usage"]["input"], 10);
        assert_eq!(runs[0]["usage"]["output"], 20);
        assert_eq!(runs[0]["usage"]["cost_usd"], 0.12);
        assert_eq!(runs[0]["elapsed_ms"], 900);
        assert_eq!(runs[0]["ended"], true);
        assert_eq!(runs[0]["stdout"]["size"], 24);
        assert_eq!(runs[1]["id"], SECOND_RUN);
        assert_eq!(runs[1]["attempt"], 2);
        assert_eq!(runs[1]["ended"], false);
        assert!(runs[1].get("outcome").is_none());
    }

    #[test]
    fn a_run_that_starts_after_a_snapshot_is_listed_by_the_next_one() {
        let bench = Bench::new(Some(BUDGET_FILE));
        bench.write("OWL-1", &record(Stage::Build), None);
        let first = bench.snapshot("OWL-1");
        assert!(first["runs"]["runs"].as_array().unwrap().is_empty());
        bench.run_folder("OWL-1", FIRST_RUN, "{\"type\":\"turn.started\"}\n");
        bench.event(
            "OWL-1",
            FIRST_RUN,
            EventKind::RunStarted,
            data([("role", json!("build")), ("attempt", json!(1))]),
        );
        let second = bench.snapshot("OWL-1");
        let runs = second["runs"]["runs"].as_array().unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0]["id"], FIRST_RUN);
        assert_eq!(runs[0]["ended"], false);
    }

    /// OWL-473: Restart is offered on a parked ticket, Cancel where `owlshift
    /// cancel` would write one: never on a parked ticket, its lock held or
    /// not; on a ticket at Ready or with no ref only while a step of it runs;
    /// and the cancel that stands is shown.
    #[test]
    fn the_actions_are_where_their_commands_act() {
        use owlshift_contracts::service::{BY_UI, Request};
        let bench = Bench::new(Some(BUDGET_FILE));
        let id = |name: &str| TicketId::new(name).unwrap();
        let mut parked = record(Stage::Build);
        let state = TicketState::restore(
            Status::Parked {
                at: Stage::Build,
                awaiting_input: false,
            },
            1,
            0,
            0,
            0,
            0,
        )
        .unwrap();
        parked.state = PersistedState::from(&state);
        let commit = bench.write("OWL-1", &parked, None);
        let actions = bench.snapshot("OWL-1")["actions"].clone();
        assert_eq!(actions["restart"], true);
        assert_eq!(actions["cancel"], false);
        assert!(
            actions["cancel_refusal"]
                .as_str()
                .unwrap()
                .contains("OWL-1 is already parked"),
            "{actions}"
        );
        let held = bench.dirs.ticket_lock(&id("OWL-1")).unwrap().unwrap();
        assert_eq!(bench.snapshot("OWL-1")["actions"]["cancel"], false);
        drop(held);

        bench.write("OWL-1", &record(Stage::Build), Some(&commit));
        let actions = bench.snapshot("OWL-1")["actions"].clone();
        assert_eq!(actions["restart"], false);
        assert_eq!(actions["cancel"], true);
        assert_eq!(actions["cancel_refusal"], Value::Null);
        assert_eq!(actions["cancel_standing"], Value::Null);
        let request = Request::new(BY_UI, Timestamp::now(), "asked from the web UI");
        crate::step::write_atomically(&cancel::file(&bench.dirs, &id("OWL-1")), &request.render())
            .unwrap();
        let standing = bench.snapshot("OWL-1")["actions"]["cancel_standing"].clone();
        assert_eq!(standing["by"], "ui");
        assert_eq!(standing["why"], "asked from the web UI");

        bench.write("OWL-2", &record(Stage::Ready), None);
        for ticket in ["OWL-2", "OWL-3"] {
            let actions = bench.snapshot(ticket)["actions"].clone();
            assert_eq!(actions["cancel"], false, "{ticket}");
            assert_eq!(actions["restart"], false, "{ticket}");
            let held = bench.dirs.ticket_lock(&id(ticket)).unwrap().unwrap();
            assert_eq!(
                bench.snapshot(ticket)["actions"]["cancel"],
                true,
                "{ticket}"
            );
            drop(held);
        }
        assert!(
            bench.snapshot("OWL-3")["actions"]["cancel_refusal"]
                .as_str()
                .unwrap()
                .contains("Owlshift has nothing of OWL-3 in flight")
        );
    }

    #[test]
    fn the_waits_are_the_tickets_own() {
        let bench = busy();
        let snap = bench.snapshot("OWL-1");
        let row = &snap["waits"]["ticket"];
        assert_eq!(row["ticket"], "OWL-1");
        assert_eq!(row["waits"]["reason"], "needs_input");
        assert_eq!(snap["waits"]["project"]["quarantined"], false);
        // A ticket with no wait of its own has no row of another's.
        let quiet = Bench::new(Some(BUDGET_FILE));
        quiet.write("OWL-1", &record(Stage::Design), None);
        let mut other = record(Stage::Build);
        other.state = PersistedState::parse(WAITING).unwrap();
        other.questions = TicketQuestions::parse(&questions_text()).unwrap();
        quiet.write("OWL-2", &other, None);
        let snap = quiet.snapshot("OWL-1");
        assert_eq!(snap["waits"]["ticket"]["ticket"], "OWL-1");
        assert!(snap["waits"]["ticket"]["waits"].is_null());
    }

    #[test]
    fn a_ref_with_a_corrupt_file_errors_in_its_sections_only() {
        let bench = busy();
        let ticket = TicketId::new("OWL-1").unwrap();
        let name = owlshift_contracts::refs::ticket_ref(&ticket);
        let tip = bench
            .git
            .run(&bench.checkout, &["rev-parse", &name])
            .unwrap();
        let tip = String::from_utf8(tip).unwrap().trim().to_owned();
        let bad = bench.checkout.join("bad.json");
        fs::write(&bad, "{not json").unwrap();
        let blob = bench
            .git
            .run(
                &bench.checkout,
                &["hash-object", "-w", bad.to_str().unwrap()],
            )
            .unwrap();
        let blob = String::from_utf8(blob).unwrap().trim().to_owned();
        let git = |args: &[&str]| {
            let out = bench.git.run(&bench.checkout, args).unwrap();
            String::from_utf8(out).unwrap().trim().to_owned()
        };
        git(&["read-tree", &tip]);
        git(&[
            "update-index",
            "--cacheinfo",
            &format!("100644,{blob},questions.json"),
        ]);
        let tree = git(&["write-tree"]);
        let broken = git(&[
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@example.invalid",
            "commit-tree",
            "--no-gpg-sign",
            &tree,
            "-p",
            &tip,
            "-m",
            "x",
        ]);
        git(&["update-ref", &name, &broken]);
        let snap = bench.snapshot("OWL-1");
        for section in ["cost", "questions", "plan"] {
            let why = snap[section]["error"]
                .as_str()
                .unwrap_or_else(|| panic!("{section}"));
            assert!(why.contains("questions.json"), "{section}: {why}");
        }
        let entries = snap["timeline"]["entries"].as_array().unwrap();
        assert!(entries.last().unwrap()["error"].is_string());
        assert_eq!(snap["runs"]["runs"].as_array().unwrap().len(), 2);
        assert!(snap["waits"]["error"].is_null());
    }

    #[test]
    fn a_history_past_the_cap_says_the_timeline_starts_later() {
        // The cap is MAX_HISTORY; `commits` reports the cut, which the
        // timeline carries.
        let bench = Bench::new(Some(BUDGET_FILE));
        let one = record(Stage::Design);
        let first = bench.write("OWL-1", &one, None);
        let mut two = record(Stage::Build);
        two.cost = Some(cost_text(1, 0));
        bench.write("OWL-1", &two, Some(&first));
        let ticket = TicketId::new("OWL-1").unwrap();
        let (revisions, cut) =
            ticket_ref::history(&bench.git, &bench.checkout, &ticket, 1).unwrap();
        let value = timeline(&revisions, cut);
        assert_eq!(value["truncated"], true);
        assert_eq!(value["entries"].as_array().unwrap().len(), 1);
        assert_eq!(value["entries"][0]["changed"][0], "first");
        let whole = bench.snapshot("OWL-1");
        assert_eq!(whole["timeline"]["truncated"], false);
    }

    #[test]
    fn the_snapshot_holds_no_raw_markup() {
        let bench = Bench::new(Some(BUDGET_FILE));
        let mut one = record(Stage::Design);
        one.plan = Some(crate::ticket_ref::fixtures::plan(
            "<script>alert(1)</script> & </b>",
            &["<b>"],
        ));
        bench.write("OWL-1", &one, None);
        let text = crate::ui::server::json_text(&bench.snapshot("OWL-1"));
        for c in ['<', '>', '&', '\u{2028}', '\u{2029}'] {
            assert!(!text.contains(c), "{c:?}");
        }
        let back: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            back["plan"]["current"]["plan"]["goal"],
            "<script>alert(1)</script> & </b>"
        );
    }
}
