//! The Spend screen's answer (P8 part 7, OWL-472, runtime and operations,
//! "Observability & the local web UI"): what the runs cost, per day, ticket,
//! role and harness, against each ticket's budget and each harness's usage
//! cap.
//!
//! `GET /api/spend?days=7|30|90|all&project=owner/repo&rev=<hash>`, the
//! last 30 days by default (decision of Q3):
//!
//! - the *events' sums* fold the `usage` events inside the period by local
//!   day (the machine's time zone), ticket, role and harness, joining each
//!   to its run's `run_started` by (project, ticket, run id); a run recorded
//!   before `run_started` named its harness is bucketed `unrecorded`, a
//!   usage event with no matching `run_started` role `unknown`;
//! - *tickets* also carry the exact total of each ticket's `cost.json`
//!   (every run, whatever the period: `spent_micros`, `runs`,
//!   `unreported_runs`), the part the budget counts (`counted_micros`, what
//!   was spent since the last restart) and the served project's
//!   `ticket_budget_usd`. The events' sums and `cost.json` are two columns,
//!   never added: a run whose harness reports no cost adds nothing to
//!   either, and `unreported_runs` says how many;
//! - `usage`: each harness's cap and windows ([`orchestrator::usage_windows`]).
//!
//! The answer carries `rev`, the SHA-256 of the whole body without it. A
//! request passing the same `rev` is answered `{"unchanged": true, "rev"}`,
//! which saves the page a redraw and never hides a change: the caps,
//! windows, budgets, totals and the sliding period are all in the hash.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use jiff::Timestamp;
use jiff::tz::TimeZone;
use owlshift_adapters::forge::Repo;
use owlshift_contracts::config::PersonalConfig;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::index::EventIndex;
use super::orchestrator::{path_of, usage_windows};
use crate::config::FileState;
use crate::project::{ProjectDirs, runner_git};
use crate::served::{self, Registry};
use crate::ticket_ref;

/// How far back the screen counts when the request names no period.
pub const DEFAULT_DAYS: u32 = 30;

/// How long the tickets' cost totals are kept between two requests: reading
/// them is five git commands per served project.
const REFS_KEPT: Duration = Duration::from_secs(10);

/// The period a request asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Period {
    Days(u32),
    All,
}

impl Period {
    /// `7`, `30`, `90` or `all`.
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "all" => Some(Self::All),
            "7" => Some(Self::Days(7)),
            "30" => Some(Self::Days(30)),
            "90" => Some(Self::Days(90)),
            _ => None,
        }
    }

    fn name(self) -> String {
        match self {
            Self::All => "all".to_owned(),
            Self::Days(days) => days.to_string(),
        }
    }
}

/// What a request asks.
#[derive(Clone, Debug)]
pub struct Query {
    pub period: Period,
    /// `owner/repo`, in any case: only that project.
    pub project: Option<String>,
    /// The `rev` of the answer the page holds.
    pub rev: Option<String>,
}

impl Default for Query {
    fn default() -> Self {
        Self {
            period: Period::Days(DEFAULT_DAYS),
            project: None,
            rev: None,
        }
    }
}

/// What the screen keeps between two requests.
#[derive(Default)]
pub struct Cache {
    refs: Mutex<Option<(Instant, Vec<Ticket>)>>,
    fold: Mutex<Option<(FoldKey, Fold)>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct FoldKey {
    generation: u64,
    offset: u64,
    period: Period,
    project: Option<String>,
    day: String,
}

#[derive(Clone, Debug, Default)]
struct Fold {
    total: u64,
    records: u64,
    days: BTreeMap<String, u64>,
    tickets: BTreeMap<(String, String), u64>,
    roles: BTreeMap<String, u64>,
    harnesses: BTreeMap<String, u64>,
}

/// One ticket's `cost.json` and its project's budget.
#[derive(Clone, Debug)]
struct Ticket {
    project: String,
    ticket: String,
    runs: u32,
    unreported_runs: u32,
    spent_micros: u64,
    restarted_at_micros: u64,
    counted_micros: u64,
    budget_micros: Option<u64>,
}

/// What the answer is made from.
pub struct Inputs<'a> {
    pub data: &'a Path,
    pub index: &'a Mutex<EventIndex>,
    pub personal: &'a FileState<PersonalConfig>,
    pub cache: &'a Cache,
    pub now: Timestamp,
    pub tz: TimeZone,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The tickets' cost totals of every served project, read at most every
/// [`REFS_KEPT`]; the notes say what could not be read.
fn tickets(inputs: &Inputs<'_>, notes: &mut Vec<String>) -> Vec<Ticket> {
    let mut kept = lock(&inputs.cache.refs);
    if let Some((at, rows)) = kept.as_ref()
        && at.elapsed() < REFS_KEPT
    {
        return rows.clone();
    }
    let mut rows = Vec::new();
    let served = match Registry::in_data_dir(inputs.data).read() {
        Ok(served) => served,
        Err(why) => {
            notes.push(format!("the served projects cannot be read: {why}"));
            return rows;
        }
    };
    let personal = path_of(inputs.personal);
    for entry in &served.projects {
        let Ok(repo) = Repo::parse(&entry.repository) else {
            continue;
        };
        let dirs = ProjectDirs::new(inputs.data, &repo);
        // The budget is the project file's that the project runs on.
        let budget = served::effective(&repo, &dirs, personal.clone())
            .ok()
            .and_then(|effective| match effective.project {
                FileState::Loaded { config, .. } => config.policy.ticket_budget_micros(),
                _ => None,
            });
        if !dirs.checkout().exists() {
            continue;
        }
        match ticket_ref::all(&runner_git(), &dirs.checkout()) {
            Ok(refs) => {
                for (ticket, stored) in refs {
                    let Ok(stored) = stored else { continue };
                    let Some(cost) = stored.record.cost else {
                        continue;
                    };
                    rows.push(Ticket {
                        project: entry.repository.clone(),
                        ticket: ticket.as_str().to_owned(),
                        runs: cost.runs,
                        unreported_runs: cost.unreported_runs,
                        spent_micros: cost.spent_micros,
                        restarted_at_micros: cost.restarted_at_micros,
                        counted_micros: cost.spent_micros.saturating_sub(cost.restarted_at_micros),
                        budget_micros: budget,
                    });
                }
            }
            Err(why) => notes.push(format!(
                "the ticket refs of {} cannot be read: {why}",
                entry.repository
            )),
        }
    }
    *kept = Some((Instant::now(), rows.clone()));
    rows
}

fn local_day(at: Timestamp, tz: &TimeZone) -> String {
    at.to_zoned(tz.clone()).date().to_string()
}

/// The usage events inside the period folded.
fn fold(inputs: &Inputs<'_>, index: &EventIndex, query: &Query) -> Fold {
    // The period counts whole local days: today and the days before it, so
    // the fold depends on the log and the day, and can be kept.
    let since_day = match query.period {
        Period::All => None,
        Period::Days(days) => inputs
            .now
            .to_zoned(inputs.tz.clone())
            .date()
            .checked_sub(jiff::Span::new().days(i64::from(days) - 1))
            .ok()
            .map(|day| day.to_string()),
    };
    let mut fold = Fold::default();
    for record in index.usage() {
        let day = local_day(record.at, &inputs.tz);
        if since_day.as_ref().is_some_and(|since| day < *since) {
            continue;
        }
        if let Some(only) = &query.project
            && !record.project.eq_ignore_ascii_case(only)
        {
            continue;
        }
        let facts = match (&record.ticket, &record.run) {
            (Some(ticket), Some(run)) => index.run(&record.project, ticket, run),
            _ => None,
        };
        let role = facts.map_or_else(|| "unknown".to_owned(), |facts| facts.role.clone());
        let harness = facts
            .and_then(|facts| facts.harness.clone())
            .unwrap_or_else(|| "unrecorded".to_owned());
        let cost = record.cost_micros;
        fold.total += cost;
        fold.records += 1;
        *fold.days.entry(day).or_default() += cost;
        *fold
            .tickets
            .entry((
                record.project.to_ascii_lowercase(),
                record.ticket.clone().unwrap_or_else(|| "-".to_owned()),
            ))
            .or_default() += cost;
        *fold.roles.entry(role).or_default() += cost;
        *fold.harnesses.entry(harness).or_default() += cost;
    }
    fold
}

fn buckets(map: &BTreeMap<String, u64>, key: &str) -> Vec<Value> {
    map.iter()
        .map(|(name, micros)| json!({ key: name, "micros": micros }))
        .collect()
}

/// The Spend answer. A request whose `rev` is the answer's gets
/// `{"unchanged": true}`.
pub fn answer(inputs: &Inputs<'_>, query: &Query) -> Value {
    let mut notes = Vec::new();
    let (folded, generation) = {
        let mut index = lock(inputs.index);
        if let Err(why) = index.update() {
            notes.push(format!("the event log cannot be read: {why}"));
        }
        let key = FoldKey {
            generation: index.generation(),
            offset: index.offset(),
            period: query.period,
            project: query.project.as_ref().map(|p| p.to_ascii_lowercase()),
            day: local_day(inputs.now, &inputs.tz),
        };
        let mut cached = lock(&inputs.cache.fold);
        let folded = match cached.as_ref() {
            Some((have, fold)) if *have == key => fold.clone(),
            _ => {
                let folded = fold(inputs, &index, query);
                *cached = Some((key, folded.clone()));
                folded
            }
        };
        (folded, index.generation())
    };
    let rows = tickets(inputs, &mut notes);
    let rows: Vec<&Ticket> = rows
        .iter()
        .filter(|row| {
            query
                .project
                .as_ref()
                .is_none_or(|only| row.project.eq_ignore_ascii_case(only))
        })
        .collect();
    // Every ticket either column knows, the events' sums beside cost.json.
    let mut tickets: BTreeMap<(String, String), Value> = BTreeMap::new();
    for ((project, ticket), micros) in &folded.tickets {
        tickets.insert(
            (project.clone(), ticket.clone()),
            json!({ "project": project, "ticket": ticket, "events_micros": micros, "cost": null }),
        );
    }
    for row in rows {
        let cost = json!({
            "runs": row.runs,
            "unreported_runs": row.unreported_runs,
            "spent_micros": row.spent_micros,
            "restarted_at_micros": row.restarted_at_micros,
            "counted_micros": row.counted_micros,
            "budget_micros": row.budget_micros,
        });
        let key = (row.project.to_ascii_lowercase(), row.ticket.clone());
        match tickets.get_mut(&key) {
            Some(entry) => entry["cost"] = cost,
            None => {
                tickets.insert(
                    key,
                    json!({
                        "project": row.project,
                        "ticket": row.ticket,
                        "events_micros": 0,
                        "cost": cost,
                    }),
                );
            }
        }
    }
    let mut tickets: Vec<Value> = tickets.into_values().collect();
    tickets.sort_by_key(|ticket| {
        std::cmp::Reverse(
            ticket["cost"]["spent_micros"]
                .as_u64()
                .unwrap_or(0)
                .max(ticket["events_micros"].as_u64().unwrap_or(0)),
        )
    });
    let body = json!({
        "period": query.period.name(),
        "project": query.project,
        "generation": generation,
        "events": { "total_micros": folded.total, "records": folded.records },
        "days": buckets(&folded.days, "day"),
        "tickets": tickets,
        "roles": buckets(&folded.roles, "role"),
        "harnesses": buckets(&folded.harnesses, "harness"),
        "usage": usage_windows(inputs.data, inputs.personal, inputs.now, None),
        "notes": notes,
    });
    let mut hash = Sha256::new();
    hash.update(super::server::json_text(&body).as_bytes());
    let rev = super::auth::hex(&hash.finalize());
    if query.rev.as_deref() == Some(rev.as_str()) {
        return json!({ "unchanged": true, "rev": rev });
    }
    let mut body = body;
    body["rev"] = json!(rev);
    body
}
