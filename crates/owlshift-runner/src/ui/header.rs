//! The web UI's header (OWL-467, runtime and operations, "Observability &
//! the local web UI"): each served project and the run slots it uses, the
//! machine's run slots, each harness's usage and reset, how many tickets
//! wait for you, and the daemon's state. It is read from the files `owlshift
//! status` reads, through the same reader (`status::gather_files`), never
//! from the event log.
//!
//! Its JSON is the UI's own, versioned with the binary:
//!
//! - `daemon`: `{"state": "running", "pid", "version", "since"}`,
//!   `"stopping"` or `"upgrading"` with the request (`by`, `at`, `why`),
//!   `"not_running"`, or `"unknown"` with `why`;
//! - `machine_pause`: the machine's pause (`by`, `at`, `why`), or null;
//! - `slots`: `{"held", "of"}`, or null when they cannot be probed;
//! - `usage`: per harness, `{"harness", "state": "limited", "until"}`
//!   (`until` null when unknown) or `{"harness", "state": "share", "share",
//!   "resets_at"}`, times as RFC 3339 text;
//! - `waiting`: `{"questions", "reviews", "parked"}`, summed over projects:
//!   the tickets whose questions wait (plans to approve included), the pull
//!   requests to review, the parked tickets;
//! - `projects`: each `{"repository", "slots_held", "slot_holders",
//!   "paused", "quarantined", "waiting"}`, `slot_holders` being the tickets
//!   whose steps hold the project's run slots, in slot order, and
//!   `slots_held` their count (OWL-530);
//! - `notes`: what could not be read.

use jiff::Timestamp;
use owlshift_contracts::service::Request;
use serde_json::{Value, json};

use crate::daemon::digest::HarnessUsage;
use crate::status::{Daemon, Report};

/// The header, as the page reads it.
#[derive(Clone, Debug, PartialEq)]
pub struct Header(pub Value);

impl Header {
    /// Its JSON.
    pub fn json(&self) -> &Value {
        &self.0
    }
}

fn time(at: Timestamp) -> Value {
    Value::String(at.to_string())
}

/// A request as the header shows it; one whose file does not read keeps its
/// place with no words.
fn request(request: Option<&Request>) -> Value {
    match request {
        Some(request) => json!({
            "by": request.by,
            "at": time(request.at),
            "why": request.why,
        }),
        None => json!({ "by": null, "at": null, "why": "its file does not read" }),
    }
}

/// The header of `report`.
pub fn of(report: &Report) -> Header {
    let daemon = match &report.daemon {
        Daemon::Running(record) => match (&report.stop_asked, &report.drain_asked) {
            (Some(stop), _) => {
                let mut state = request(Some(stop));
                state["state"] = json!("stopping");
                state
            }
            (None, Some(drain)) => {
                let mut state = request(Some(drain));
                state["state"] = json!("upgrading");
                state
            }
            (None, None) => json!({
                "state": "running",
                "pid": record.as_ref().map(|record| record.pid),
                "version": record.as_ref().map(|record| record.version.clone()),
                "since": record.as_ref().map(|record| time(record.since)),
            }),
        },
        Daemon::NotRunning => json!({ "state": "not_running" }),
        Daemon::Unknown(why) => json!({ "state": "unknown", "why": why }),
    };
    let machine_pause = match &report.machine_pause {
        None => Value::Null,
        Some(standing) => request(standing.as_ref()),
    };
    let slots = report.slots.as_ref().map_or(
        Value::Null,
        |(held, of)| json!({ "held": held.len(), "of": of }),
    );
    let usage: Vec<Value> = report
        .usage
        .iter()
        .map(|(harness, usage)| match usage {
            HarnessUsage::Limited(until) => json!({
                "harness": harness,
                "state": "limited",
                "until": until.map(time),
            }),
            HarnessUsage::Share { share, resets_at } => json!({
                "harness": harness,
                "state": "share",
                "share": share,
                "resets_at": time(*resets_at),
            }),
        })
        .collect();
    let mut total = (0, 0, 0);
    let projects: Vec<Value> = report
        .projects
        .iter()
        .map(|project| {
            let slot_holders: Vec<&str> = report.slots.as_ref().map_or(Vec::new(), |(held, _)| {
                held.iter()
                    .filter_map(|slot| slot.holder.as_ref())
                    .filter(|holder| holder.project.eq_ignore_ascii_case(&project.repository))
                    .map(|holder| holder.ticket.as_str())
                    .collect()
            });
            let counts = (
                project.waiting.len(),
                project.in_review.len(),
                project.parked.len(),
            );
            total = (total.0 + counts.0, total.1 + counts.1, total.2 + counts.2);
            json!({
                "repository": project.repository,
                "slots_held": slot_holders.len(),
                "slot_holders": slot_holders,
                "paused": project
                    .pause
                    .as_ref()
                    .map_or(Value::Null, |standing| request(standing.as_ref())),
                "quarantined": project.quarantine.as_ref().map(|line| match line {
                    Ok(line) => line.clone(),
                    Err(why) => format!("its file cannot be read: {why}"),
                }),
                "waiting": waiting(counts),
            })
        })
        .collect();
    Header(json!({
        "daemon": daemon,
        "machine_pause": machine_pause,
        "slots": slots,
        "usage": usage,
        "waiting": waiting(total),
        "projects": projects,
        "notes": report.notes,
    }))
}

fn waiting((questions, reviews, parked): (usize, usize, usize)) -> Value {
    json!({ "questions": questions, "reviews": reviews, "parked": parked })
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use owlshift_adapters::forge::Repo;
    use owlshift_contracts::format::Format;
    use owlshift_contracts::ids::TicketId;
    use owlshift_contracts::service::{BY_PAUSE, DaemonRecord, ServedProject, ServedProjects};
    use owlshift_core::usage::Window;
    use owlshift_core::vocab::Harness;

    use super::*;
    use crate::daemon::{self, requests};
    use crate::project::ProjectDirs;
    use crate::served::Registry;
    use crate::slots::{Holder, MachineSlots};
    use crate::status::gather_files;
    use crate::usage::{self as machine_usage, UsageStore};

    fn ts(text: &str) -> Timestamp {
        text.parse().unwrap()
    }

    fn serve(data: &Path, repositories: &[&str]) {
        let served = ServedProjects {
            format: Format,
            projects: repositories
                .iter()
                .map(|repository| ServedProject {
                    repository: (*repository).to_owned(),
                    checkout: format!("/home/ada/{repository}"),
                })
                .collect(),
        };
        crate::step::write_atomically(&Registry::in_data_dir(data).file(), &served.render())
            .unwrap();
    }

    fn header(data: &Path, caps: &[(Harness, u8)], now: Timestamp) -> Value {
        of(&gather_files(data, 2, caps, now)).0
    }

    fn record(data: &Path) -> DaemonRecord {
        let _ = data;
        DaemonRecord {
            format: Format,
            pid: 4242,
            version: "0.4.0".to_owned(),
            binary: "/bin/owlshift".to_owned(),
            since: ts("2026-11-05T08:00:00Z"),
            build: None,
        }
    }

    #[test]
    fn the_header_reads_the_projects_and_their_slots() {
        let data = tempfile::tempdir().unwrap();
        let data = data.path();
        serve(data, &["acme/web", "acme/api"]);
        let slots = MachineSlots::new(data, 3);
        let take = |project: &str, ticket: &str| {
            slots
                .take(&Holder {
                    project: project.to_owned(),
                    ticket: TicketId::new(ticket).unwrap(),
                    pid: std::process::id(),
                    since: Timestamp::now(),
                })
                .unwrap()
                .unwrap()
        };
        // Taken in this order, so held in slot order; the third is an
        // unserved project's, counted for the machine only.
        let _held = [
            take("Acme/Web", "OWL-1"),
            take("Acme/Web", "OWL-2"),
            take("acme/other", "OWL-3"),
        ];
        let api = ProjectDirs::new(data, &Repo::parse("acme/api").unwrap());
        let reason = Request::new(BY_PAUSE, Timestamp::now(), "lunch break");
        requests::ask_pause(data, Some(&api), &reason).unwrap();

        let header = header(data, &[], Timestamp::now());
        assert_eq!(header["daemon"]["state"], "not_running");
        assert_eq!(header["slots"]["held"], 3);
        assert_eq!(header["projects"][0]["repository"], "acme/web");
        assert_eq!(header["projects"][0]["slots_held"], 2);
        assert_eq!(
            header["projects"][0]["slot_holders"],
            json!(["OWL-1", "OWL-2"])
        );
        assert_eq!(header["projects"][0]["paused"], Value::Null);
        assert_eq!(header["projects"][1]["slots_held"], 0);
        assert_eq!(header["projects"][1]["slot_holders"], json!([]));
        assert_eq!(header["projects"][1]["paused"]["why"], "lunch break");
        assert_eq!(header["projects"][1]["quarantined"], Value::Null);
        assert_eq!(header["machine_pause"], Value::Null);
    }

    #[test]
    fn the_header_counts_what_waits_for_a_person() {
        let data = tempfile::tempdir().unwrap();
        serve(data.path(), &["acme/web", "acme/api"]);
        let mut report = gather_files(data.path(), 2, &[], Timestamp::now());
        let id = |id: &str| TicketId::new(id).unwrap();
        report.projects[0].waiting = vec![id("OWL-1"), id("OWL-2")];
        report.projects[0].in_review = vec![id("OWL-3")];
        report.projects[1].parked = vec![id("OWL-4")];
        report.projects[1].in_review = vec![id("OWL-5")];
        let header = of(&report).0;
        assert_eq!(
            header["waiting"],
            json!({ "questions": 2, "reviews": 2, "parked": 1 })
        );
        assert_eq!(
            header["projects"][0]["waiting"],
            json!({ "questions": 2, "reviews": 1, "parked": 0 })
        );
        assert_eq!(
            header["projects"][1]["waiting"],
            json!({ "questions": 0, "reviews": 1, "parked": 1 })
        );
    }

    #[test]
    fn the_header_reads_each_harness_usage_and_reset() {
        let data = tempfile::tempdir().unwrap();
        let data = data.path();
        let now = ts("2026-11-05T09:00:00Z");
        let (r1, r2) = (ts("2026-11-05T12:00:00Z"), ts("2026-11-09T10:00:00Z"));
        for (harness, share, resets) in [(Harness::Claude, 0.9, r1), (Harness::Codex, 0.6, r2)] {
            UsageStore::in_data_dir(data)
                .update(harness, |state| {
                    state.windows.insert(
                        "seven_day".to_owned(),
                        Window {
                            share,
                            resets_at_ms: machine_usage::ms(resets),
                            read_at_ms: 0,
                        },
                    );
                })
                .unwrap();
        }
        let header = header(data, &[(Harness::Claude, 50)], now);
        assert_eq!(
            header["usage"],
            json!([
                { "harness": "claude", "state": "limited", "until": "2026-11-05T12:00:00Z" },
                {
                    "harness": "codex",
                    "state": "share",
                    "share": 0.6,
                    "resets_at": "2026-11-09T10:00:00Z"
                },
            ])
        );
    }

    #[test]
    fn the_header_reads_the_machine_pause() {
        let data = tempfile::tempdir().unwrap();
        let data = data.path();
        assert_eq!(
            header(data, &[], Timestamp::now())["machine_pause"],
            Value::Null
        );
        crate::step::write_atomically(
            &requests::machine_pause_file(data),
            &Request::new("owlshift pause", Timestamp::now(), "away").render(),
        )
        .unwrap();
        let header = header(data, &[], Timestamp::now());
        assert_eq!(header["machine_pause"]["why"], "away");
        assert_eq!(header["machine_pause"]["by"], "owlshift pause");
    }

    #[test]
    fn the_header_tells_a_running_stopping_and_upgrading_daemon() {
        let data = tempfile::tempdir().unwrap();
        let data = data.path();
        let _daemon = daemon::instance::take(data, &record(data))
            .unwrap()
            .unwrap();
        let running = header(data, &[], Timestamp::now());
        assert_eq!(running["daemon"]["state"], "running");
        assert_eq!(running["daemon"]["pid"], 4242);
        assert_eq!(running["daemon"]["version"], "0.4.0");
        assert_eq!(running["daemon"]["since"], "2026-11-05T08:00:00Z");
        let stop = Request::new("owlshift stop", Timestamp::now(), "for the night");
        requests::ask_stop(data, &stop).unwrap();
        let stopping = header(data, &[], Timestamp::now());
        assert_eq!(stopping["daemon"]["state"], "stopping");
        assert_eq!(stopping["daemon"]["why"], "for the night");

        let other = tempfile::tempdir().unwrap();
        let other = other.path();
        let _daemon = daemon::instance::take(other, &record(other))
            .unwrap()
            .unwrap();
        let drain = Request::new("owlshift upgrade", Timestamp::now(), "a newer binary");
        requests::ask_drain(other, &drain).unwrap();
        let upgrading = header(other, &[], Timestamp::now());
        assert_eq!(upgrading["daemon"]["state"], "upgrading");
        assert_eq!(upgrading["daemon"]["why"], "a newer binary");
    }
}
