//! The Orchestrator, Spend and Config screens' routes (P8 part 7, OWL-472),
//! each test on a listener bound to port 0 and a data directory of its own,
//! prepared with the runner's own writers: what each route answers is
//! compared with what `owlshift status`, `logs` and `config show` print
//! from.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use jiff::Timestamp;
use jiff::tz::TimeZone;
use owlshift_adapters::forge::Repo;
use owlshift_contracts::config::PersonalConfig;
use owlshift_contracts::event::EventKind;
use owlshift_contracts::format::Format;
use owlshift_contracts::service::{BY_PAUSE, DaemonRecord, Request, ServedProject, ServedProjects};
use owlshift_core::usage::Window;
use owlshift_core::vocab::Harness;
use owlshift_runner::config::FileState;
use owlshift_runner::daemon::{self, requests};
use owlshift_runner::events::{self, EventLog, EventSink, data};
use owlshift_runner::project::ProjectDirs;
use owlshift_runner::served::Registry;
use owlshift_runner::slots::{Holder, MachineSlots};
use owlshift_runner::ui::auth::{self, RunSecret, Session};
use owlshift_runner::ui::server::{self, Options, Serving};
use owlshift_runner::ui::token;
use owlshift_runner::usage::UsageStore;
use owlshift_runner::{status, why};
use serde_json::{Value, json};

/// A response as the wire carries it.
struct Reply {
    status: u16,
    text: String,
}

impl Reply {
    fn json(&self) -> Value {
        serde_json::from_str(&self.text).unwrap_or_else(|e| panic!("{e}: {}", self.text))
    }
}

fn send(port: u16, method: &str, path: &str, session: Option<&Session>, body: &str) -> Reply {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    let mut text =
        format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n");
    if let Some(session) = session {
        text.push_str(&format!("X-Owlshift-Session: {}\r\n", session.header()));
    }
    if method == "POST" {
        text.push_str(&format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            body.len()
        ));
    }
    text.push_str("\r\n");
    text.push_str(body);
    stream.write_all(text.as_bytes()).unwrap();
    let mut answer = Vec::new();
    stream.read_to_end(&mut answer).unwrap();
    let split = answer
        .windows(4)
        .position(|four| four == b"\r\n\r\n")
        .expect("a response's head");
    let head = String::from_utf8(answer[..split].to_vec()).unwrap();
    let status = head.split(' ').nth(1).unwrap().parse().unwrap();
    Reply {
        status,
        text: String::from_utf8(answer[split + 4..].to_vec()).unwrap(),
    }
}

/// A served UI over a data directory, a session on it, and the personal file
/// it reads.
struct Ground {
    data: tempfile::TempDir,
    ui: Serving,
    session: Session,
    personal: Arc<std::sync::Mutex<FileState<PersonalConfig>>>,
}

fn absent() -> FileState<PersonalConfig> {
    FileState::Absent(PathBuf::from("/nowhere/owlshift.toml"))
}

impl Ground {
    fn new() -> Self {
        let data = tempfile::tempdir().unwrap();
        let personal = Arc::new(std::sync::Mutex::new(absent()));
        let reader = personal.clone();
        let options = Options {
            personal: Arc::new(move || {
                let state = reader.lock().unwrap();
                match &*state {
                    FileState::Absent(path) => FileState::Absent(path.clone()),
                    FileState::Loaded { path, .. } => owlshift_runner::ui::personal_at(path),
                    other => panic!("not a state the tests set: {other:?}"),
                }
            }),
            ..Options::default()
        };
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let ui = server::start(
            data.path(),
            listener,
            RunSecret::new().unwrap(),
            None,
            options,
        )
        .unwrap();
        let token = token::read_token(data.path()).unwrap();
        let code = auth::mint_code(&token, server::now_ms()).unwrap();
        let login = send(
            ui.port(),
            "POST",
            "/api/login",
            None,
            &json!({ "code": code }).to_string(),
        );
        assert_eq!(login.status, 200, "{}", login.text);
        let value = login.json();
        let session = Session {
            id: value["id"].as_str().unwrap().to_owned(),
            secret: value["secret"].as_str().unwrap().to_owned(),
        };
        Self {
            data,
            ui,
            session,
            personal,
        }
    }

    fn data(&self) -> &Path {
        self.data.path()
    }

    fn get(&self, path: &str) -> Reply {
        send(self.ui.port(), "GET", path, Some(&self.session), "")
    }

    fn json(&self, path: &str) -> Value {
        let reply = self.get(path);
        assert_eq!(reply.status, 200, "{path}: {}", reply.text);
        reply.json()
    }

    /// Serves the projects `repos` (checkouts under /home/ada).
    fn serve(&self, repos: &[&str]) {
        let served = ServedProjects {
            format: Format,
            projects: repos
                .iter()
                .map(|repository| ServedProject {
                    repository: (*repository).to_owned(),
                    checkout: format!("/home/ada/{repository}"),
                })
                .collect(),
        };
        let file = Registry::in_data_dir(self.data()).file();
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, served.render()).unwrap();
    }

    /// An event of `project` the way the runner writes it.
    fn event(&self, project: &str, kind: EventKind, details: serde_json::Map<String, Value>) {
        let mut nowhere = std::io::sink();
        EventSink::new(project, EventLog::in_dir(self.data()), &mut nowhere)
            .emit_project(None, kind, details);
    }

    /// Appends `text` to the event log.
    fn append_log(&self, text: &str) {
        let path = EventLog::in_dir(self.data()).path().to_owned();
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        file.write_all(text.as_bytes()).unwrap();
    }
}

fn ts(text: &str) -> Timestamp {
    text.parse().unwrap()
}

/// The lines of `status` the Orchestrator shows, from the files and the
/// event log as `owlshift status` reads them.
fn reference(data: &Path, tz: &TimeZone) -> Vec<(status::Part, String)> {
    let mut report = status::gather_files(
        data,
        owlshift_contracts::config::DEFAULT_CONCURRENT_RUNS,
        &[],
        Timestamp::now(),
    );
    let (events, _) = why::read_events(EventLog::in_dir(data).path()).unwrap_or_default();
    if let status::Daemon::Running(record) = &report.daemon {
        let since = record.as_ref().map_or(Timestamp::MIN, |r| r.since);
        for project in &mut report.projects {
            project.watch = status::watch_state(&events, &project.repository, since);
        }
    }
    status::parts(&report, tz)
}

fn daemon_record(since: Timestamp) -> DaemonRecord {
    DaemonRecord {
        format: Format,
        pid: 4242,
        version: "0.4.0".to_owned(),
        binary: "/bin/owlshift".to_owned(),
        since,
        build: None,
    }
}

/// A bench data directory: a running daemon, two served projects, a restarting
/// watch, a machine pause, a project pause, a draining project, a quarantine,
/// a held slot and a usage window.
fn bench(ground: &Ground) -> daemon::instance::Instance {
    ground.serve(&["acme/web", "acme/api"]);
    let instance =
        daemon::instance::take(ground.data(), &daemon_record(ts("2026-01-01T00:00:00Z")))
            .unwrap()
            .unwrap();
    ground.event(
        "acme/web",
        EventKind::Decision,
        data([("service", json!("watch_started")), ("pid", json!(11))]),
    );
    ground.event(
        "acme/api",
        EventKind::Decision,
        data([
            ("service", json!("watch_ended")),
            ("end", json!("exit status 1")),
            ("asked", json!(false)),
            ("restart_in_ms", json!(60_000)),
            ("in_a_row", json!(3)),
        ]),
    );
    let web = ProjectDirs::new(ground.data(), &Repo::parse("acme/web").unwrap());
    let api = ProjectDirs::new(ground.data(), &Repo::parse("acme/api").unwrap());
    std::fs::create_dir_all(web.root()).unwrap();
    std::fs::write(
        web.quarantine_file(),
        "\nRun r1 of OWL-1 broke isolation:\n- the hooks changed\n",
    )
    .unwrap();
    std::fs::create_dir_all(api.root()).unwrap();
    std::fs::write(owlshift_runner::watch::stop::file(&api), "stop\n").unwrap();
    requests::ask_pause(
        ground.data(),
        None,
        &Request::new(BY_PAUSE, Timestamp::now(), "lunch"),
    )
    .unwrap();
    requests::ask_pause(
        ground.data(),
        Some(&web),
        &Request::new(BY_PAUSE, Timestamp::now(), "web is being moved"),
    )
    .unwrap();
    let ends = Timestamp::now()
        .checked_add(jiff::SignedDuration::from_hours(2))
        .unwrap();
    UsageStore::in_data_dir(ground.data())
        .update(Harness::Claude, |state| {
            state.windows.insert(
                "five_hour".to_owned(),
                Window {
                    share: 0.42,
                    resets_at_ms: ends.as_millisecond(),
                    read_at_ms: Timestamp::now().as_millisecond(),
                },
            );
        })
        .unwrap();
    instance
}

#[test]
fn the_orchestrator_needs_a_session() {
    let ground = Ground::new();
    let none = send(ground.ui.port(), "GET", "/api/orchestrator", None, "");
    assert_eq!(none.status, 401);
    let made_up = send(
        ground.ui.port(),
        "GET",
        "/api/orchestrator",
        Some(&Session {
            id: "x".into(),
            secret: "y".into(),
        }),
        "",
    );
    assert_eq!(made_up.status, 401);
}

#[test]
fn a_note_of_status_is_among_the_lines_once_and_not_among_the_notes() {
    let ground = Ground::new();
    // A served-projects file that does not read gives `status` a note.
    let file = Registry::in_data_dir(ground.data()).file();
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(file, "{ not json").unwrap();
    let answer = ground.json("/api/orchestrator");
    let warnings: Vec<&str> = answer["lines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|line| line.as_str().unwrap())
        .filter(|line| line.starts_with("warning: the served projects cannot be read"))
        .collect();
    assert_eq!(warnings.len(), 1, "{answer}");
    assert_eq!(answer["notes"], json!([]));
}

#[test]
fn the_orchestrators_lines_are_the_status_lines_it_shows_in_order() {
    let ground = Ground::new();
    let _daemon = bench(&ground);
    // A held slot gives the Slots kind a line, which the Orchestrator leaves out.
    let _slot = MachineSlots::new(ground.data(), 2)
        .take(&Holder {
            project: "acme/web".to_owned(),
            ticket: owlshift_contracts::ids::TicketId::new("OWL-1").unwrap(),
            pid: std::process::id(),
            since: Timestamp::now(),
        })
        .unwrap();
    let answer = ground.json("/api/orchestrator");
    let tz = TimeZone::system();
    let want = reference(ground.data(), &tz);
    let shown: Vec<&str> = want
        .iter()
        .filter(|(part, _)| part.on_the_page())
        .map(|(_, line)| line.as_str())
        .collect();
    let got: Vec<&str> = answer["lines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|line| line.as_str().unwrap())
        .collect();
    assert_eq!(got, shown);
    // The words of a restarting watch, a pause, a quarantine and a drain.
    let text = got.join("\n");
    for needle in [
        "Daemon: running since",
        "Paused: every project of this machine",
        "restarting at",
        "3 ends in a row",
        "watch: running since",
        "quarantined: Run r1 of OWL-1 broke isolation",
        "draining: its watch ends",
        "paused by",
        "Usage: claude 42 %",
    ] {
        assert!(text.contains(needle), "{needle:?} in {text}");
    }
    // The kinds it leaves out are named, and none of their lines is there.
    let left_out: Vec<&str> = answer["left_out"]
        .as_array()
        .unwrap()
        .iter()
        .map(|name| name.as_str().unwrap())
        .collect();
    assert_eq!(
        left_out,
        [
            "waiting",
            "parked",
            "merges",
            "drain_steps",
            "slots",
            "digest"
        ]
    );
    for (part, line) in &want {
        if !part.on_the_page() {
            assert!(!got.contains(&line.as_str()), "{line}");
        }
    }
    assert!(want.iter().any(|(part, _)| *part == status::Part::Slots));
    // The structure carries the same facts.
    assert_eq!(answer["daemon"]["state"], "running");
    assert_eq!(answer["daemon"]["pid"], 4242);
    let projects = answer["projects"].as_array().unwrap();
    let web = projects
        .iter()
        .find(|p| p["repository"] == "acme/web")
        .unwrap();
    assert_eq!(web["watch"]["state"], "running");
    assert_eq!(web["watch"]["pid"], 11);
    assert!(
        web["quarantined"]
            .as_str()
            .unwrap()
            .contains("broke isolation")
    );
    let api = projects
        .iter()
        .find(|p| p["repository"] == "acme/api")
        .unwrap();
    assert_eq!(api["watch"]["state"], "restarting");
    assert_eq!(api["watch"]["in_a_row"], 3);
    assert_eq!(api["draining"], true);
}

#[test]
fn the_usage_windows_come_with_their_share_reset_and_cap() {
    let ground = Ground::new();
    let _daemon = bench(&ground);
    let file = ground.data().join("personal.toml");
    std::fs::write(&file, "[harnesses.claude]\nusage_cap_percent = 40\n").unwrap();
    *ground.personal.lock().unwrap() = FileState::Loaded {
        path: file,
        config: PersonalConfig::parse("").unwrap(),
        entries: Vec::new(),
    };
    let answer = ground.json("/api/orchestrator");
    let claude = answer["usage"]
        .as_array()
        .unwrap()
        .iter()
        .find(|usage| usage["name"] == "claude")
        .unwrap();
    assert_eq!(claude["cap_percent"], 40);
    let window = &claude["windows"][0];
    assert_eq!(window["name"], "five_hour");
    assert!((window["share"].as_f64().unwrap() - 0.42).abs() < 1e-9);
    assert_eq!(window["holds"], true);
    assert!(window["resets_at"].as_str().unwrap().ends_with('Z'));
    // Under the cap the window does not hold, and a harness with none is free.
    std::fs::write(
        ground.data().join("personal.toml"),
        "[harnesses.claude]\nusage_cap_percent = 80\n",
    )
    .unwrap();
    let answer = ground.json("/api/orchestrator");
    let claude = answer["usage"]
        .as_array()
        .unwrap()
        .iter()
        .find(|usage| usage["name"] == "claude")
        .unwrap();
    assert_eq!(claude["windows"][0]["holds"], false);
    let codex = answer["usage"]
        .as_array()
        .unwrap()
        .iter()
        .find(|usage| usage["name"] == "codex")
        .unwrap();
    assert!(codex["windows"].as_array().unwrap().is_empty());
}

fn entries(answer: &Value) -> Vec<Value> {
    answer["events"]["entries"].as_array().unwrap().clone()
}

fn cursor(answer: &Value) -> String {
    format!(
        "gen={}&after={}",
        answer["events"]["generation"], answer["events"]["cursor"]
    )
}

#[test]
fn decisions_and_warnings_arrive_with_their_cursor_and_their_logs_line() {
    let ground = Ground::new();
    ground.serve(&["acme/web"]);
    ground.event(
        "acme/web",
        EventKind::Decision,
        data([("note", json!("first"))]),
    );
    // Other kinds are not on the feed.
    ground.event("acme/web", EventKind::Scan, data([]));
    let first = ground.json("/api/orchestrator");
    let seen = entries(&first);
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0]["kind"], "decision");
    assert_eq!(seen[0]["project"], "acme/web");
    assert_eq!(seen[0]["details"]["note"], "first");

    // Nothing new, nothing returned.
    let again = ground.json(&format!("/api/orchestrator?{}", cursor(&first)));
    assert!(entries(&again).is_empty());
    assert_eq!(again["events"]["reset"], false);

    // A decision and a warning appended after come back with the cursor.
    ground.event(
        "acme/web",
        EventKind::Warning,
        data([("note", json!("second"))]),
    );
    ground.event(
        "acme/web",
        EventKind::Decision,
        data([("note", json!("third"))]),
    );
    let next = ground.json(&format!("/api/orchestrator?{}", cursor(&first)));
    let new = entries(&next);
    assert_eq!(new.len(), 2);
    assert_eq!(new[0]["kind"], "warning");
    assert_eq!(new[1]["details"]["note"], "third");
    // Each line is the one `owlshift logs` prints for the event.
    let (events, _) = why::read_events(EventLog::in_dir(ground.data()).path()).unwrap();
    let printed: Vec<String> = events
        .iter()
        .filter(|event| matches!(event.kind, EventKind::Decision | EventKind::Warning))
        .map(events::format_line)
        .collect();
    let mut shown: Vec<String> = seen
        .iter()
        .chain(&new)
        .map(|entry| entry["line"].as_str().unwrap().to_owned())
        .collect();
    shown.sort();
    let mut printed_sorted = printed.clone();
    printed_sorted.sort();
    assert_eq!(shown, printed_sorted);
    for entry in seen.iter().chain(&new) {
        assert!(entry["at"].as_str().is_some());
        assert!(entry["offset"].as_u64().is_some());
    }
}

#[test]
fn a_line_that_is_not_an_event_comes_back_as_raw_escaped_text() {
    let ground = Ground::new();
    ground.append_log("this is <b>not</b> an event\n");
    ground.event(
        "acme/web",
        EventKind::Decision,
        data([("note", json!("<script>alert(1)</script>"))]),
    );
    let reply = ground.get("/api/orchestrator");
    assert_eq!(reply.status, 200);
    for c in ['<', '>', '&'] {
        assert!(!reply.text.contains(c), "{c:?} in {}", reply.text);
    }
    assert!(
        reply.text.contains("\\u003cscript\\u003e"),
        "{}",
        reply.text
    );
    let answer = reply.json();
    let seen = entries(&answer);
    assert_eq!(seen[0]["raw"], "this is <b>not</b> an event");
    assert_eq!(seen[1]["details"]["note"], "<script>alert(1)</script>");
}

#[test]
fn a_cursor_past_the_end_or_of_a_replaced_log_or_behind_a_full_feed_resets() {
    let ground = Ground::new();
    for n in 0..3 {
        ground.event("acme/web", EventKind::Decision, data([("n", json!(n))]));
    }
    let first = ground.json("/api/orchestrator");
    let generation = first["events"]["generation"].as_u64().unwrap();
    let end = first["events"]["end"].as_u64().unwrap();

    // Past the end of the log.
    let past = ground.json(&format!(
        "/api/orchestrator?gen={generation}&after={}",
        end + 100
    ));
    assert_eq!(past["events"]["reset"], true);
    assert_eq!(entries(&past).len(), 3);

    // The log is replaced: the old generation is stale, the new file's events
    // come back and the generation moves.
    let log = EventLog::in_dir(ground.data()).path().to_owned();
    std::fs::remove_file(&log).unwrap();
    ground.event("acme/web", EventKind::Decision, data([("n", json!("new"))]));
    let replaced = ground.json(&format!(
        "/api/orchestrator?gen={generation}&after={}",
        first["events"]["cursor"]
    ));
    assert_eq!(replaced["events"]["reset"], true);
    assert_ne!(
        replaced["events"]["generation"].as_u64().unwrap(),
        generation
    );
    let new = entries(&replaced);
    assert_eq!(new.len(), 1);
    assert_eq!(new[0]["details"]["n"], "new");

    // More than the feed keeps: a cursor older than its oldest entry.
    let generation = replaced["events"]["generation"].as_u64().unwrap();
    let mut lines = String::new();
    for n in 0..520 {
        lines.push_str(&format!("raw {n}\n"));
    }
    ground.append_log(&lines);
    let behind = ground.json(&format!("/api/orchestrator?gen={generation}&after=0"));
    assert_eq!(behind["events"]["reset"], true);
    assert_eq!(behind["events"]["skipped"], Value::Null);
    assert_eq!(entries(&behind).len(), 100);
}

#[test]
fn the_daemons_log_is_read_at_its_end_and_then_followed() {
    let ground = Ground::new();
    let absent = ground.json("/api/orchestrator");
    assert!(
        absent["log"]["absent"]
            .as_str()
            .unwrap()
            .contains("does not exist")
    );

    let dir = daemon::service_dir(ground.data());
    std::fs::create_dir_all(&dir).unwrap();
    let log = dir.join("daemon.log");
    let mut text = String::new();
    for n in 0..300 {
        text.push_str(&format!("line {n}\n"));
    }
    std::fs::write(&log, text).unwrap();
    let first = ground.json("/api/orchestrator");
    let lines = first["log"]["lines"].as_array().unwrap();
    assert_eq!(lines.len(), 200);
    assert_eq!(lines[0], "line 100");
    assert_eq!(lines[199], "line 299");
    let next = first["log"]["next"].as_u64().unwrap();

    std::fs::OpenOptions::new()
        .append(true)
        .open(&log)
        .unwrap()
        .write_all(b"line 300\nhalf")
        .unwrap();
    let more = ground.json(&format!("/api/orchestrator?log={next}"));
    assert_eq!(more["log"]["lines"], json!(["line 300"]));
    assert_eq!(more["log"]["reset"], false);
    let next = more["log"]["next"].as_u64().unwrap();
    // A log that is replaced by a shorter one is read from its start.
    std::fs::write(&log, "fresh\n").unwrap();
    let reset = ground.json(&format!("/api/orchestrator?log={next}"));
    assert_eq!(reset["log"]["reset"], true);
    assert_eq!(reset["log"]["lines"], json!(["fresh"]));
}

#[test]
fn a_query_that_is_not_a_number_is_refused() {
    let ground = Ground::new();
    assert_eq!(ground.get("/api/orchestrator?after=x&gen=1").status, 400);
    assert_eq!(ground.get("/api/orchestrator?log=-1").status, 400);
}

// ---- Spend (OWL-472) ----

/// An event line as the runner writes it, at `at`.
fn event_line(
    at: &str,
    ticket: Option<&str>,
    run: Option<&str>,
    kind: &str,
    data: Value,
) -> String {
    let mut value = json!({
        "format": 2,
        "at": at,
        "project": "acme/web",
        "kind": kind,
        "data": data,
    });
    if let Some(ticket) = ticket {
        value["ticket"] = json!(ticket);
    }
    if let Some(run) = run {
        value["run"] = json!(run);
    }
    format!("{value}\n")
}

fn started(at: &str, ticket: &str, run: &str, role: &str, harness: Option<&str>) -> String {
    let mut data = json!({ "role": role });
    if let Some(harness) = harness {
        data["harness"] = json!(harness);
    }
    event_line(at, Some(ticket), Some(run), "run_started", data)
}

fn used(at: &str, ticket: &str, run: &str, cost: f64) -> String {
    event_line(
        at,
        Some(ticket),
        Some(run),
        "usage",
        json!({ "cost_usd": cost, "input_tokens": 100, "output_tokens": 10 }),
    )
}

fn days_ago(days: i64) -> String {
    Timestamp::now()
        .checked_sub(jiff::SignedDuration::from_hours(24 * days))
        .unwrap()
        .to_string()
}

fn sum(buckets: &Value, key: &str) -> std::collections::BTreeMap<String, u64> {
    buckets
        .as_array()
        .unwrap()
        .iter()
        .map(|b| {
            (
                b[key].as_str().unwrap().to_owned(),
                b["micros"].as_u64().unwrap(),
            )
        })
        .collect()
}

#[test]
fn spend_counts_the_last_30_days_by_default_and_the_period_selects_more_or_less() {
    let ground = Ground::new();
    ground.append_log(&started(
        &days_ago(40),
        "OWL-1",
        "old",
        "build",
        Some("claude"),
    ));
    ground.append_log(&used(&days_ago(40), "OWL-1", "old", 4.0));
    ground.append_log(&started(
        &days_ago(20),
        "OWL-1",
        "mid",
        "build",
        Some("claude"),
    ));
    ground.append_log(&used(&days_ago(20), "OWL-1", "mid", 2.0));
    ground.append_log(&started(
        &days_ago(1),
        "OWL-1",
        "new",
        "verify",
        Some("codex"),
    ));
    ground.append_log(&used(&days_ago(1), "OWL-1", "new", 1.0));
    let total = |path: &str| {
        ground.json(path)["events"]["total_micros"]
            .as_u64()
            .unwrap()
    };
    // No period: the last 30 days, which leave the 40-day-old event out.
    assert_eq!(total("/api/spend"), 3_000_000);
    assert_eq!(total("/api/spend?days=30"), 3_000_000);
    // 7 days leave out the 20-day-old one, all keeps every one.
    assert_eq!(total("/api/spend?days=7"), 1_000_000);
    assert_eq!(total("/api/spend?days=90"), 7_000_000);
    assert_eq!(total("/api/spend?days=all"), 7_000_000);
    assert_eq!(ground.get("/api/spend?days=12").status, 400);
    assert_eq!(ground.get("/api/spend?project=nope").status, 400);
    // Filtered to another project: nothing.
    assert_eq!(total("/api/spend?project=acme%2Fother"), 0);
}

#[test]
fn spend_splits_by_role_harness_ticket_and_keeps_runs_with_the_same_id_apart() {
    let ground = Ground::new();
    // Two tickets whose runs share a run id; one run has no harness recorded;
    // one usage event has no run_started at all.
    ground.append_log(&started(
        &days_ago(1),
        "OWL-1",
        "r1",
        "build",
        Some("claude"),
    ));
    ground.append_log(&started(&days_ago(1), "OWL-2", "r1", "verify", None));
    ground.append_log(&used(&days_ago(1), "OWL-1", "r1", 1.5));
    ground.append_log(&used(&days_ago(1), "OWL-2", "r1", 0.5));
    ground.append_log(&used(&days_ago(1), "OWL-3", "r9", 0.25));
    let answer = ground.json("/api/spend");
    let roles = sum(&answer["roles"], "role");
    assert_eq!(roles["build"], 1_500_000);
    assert_eq!(roles["verify"], 500_000);
    assert_eq!(roles["unknown"], 250_000);
    let harnesses = sum(&answer["harnesses"], "harness");
    assert_eq!(harnesses["claude"], 1_500_000);
    assert_eq!(harnesses["unrecorded"], 750_000);
    let tickets: Vec<(String, u64)> = answer["tickets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| {
            (
                t["ticket"].as_str().unwrap().to_owned(),
                t["events_micros"].as_u64().unwrap(),
            )
        })
        .collect();
    assert!(tickets.contains(&("OWL-1".to_owned(), 1_500_000)));
    assert!(tickets.contains(&("OWL-2".to_owned(), 500_000)));
    // The events' total and cost.json's are separate fields: no ticket ref
    // here, so no cost.json (the next test has some).
    assert!(
        answer["tickets"]
            .as_array()
            .unwrap()
            .iter()
            .all(|t| t["cost"].is_null())
    );
    assert_eq!(answer["events"]["total_micros"], 2_250_000);
}

#[test]
fn spend_buckets_by_the_local_day_across_midnight() {
    use owlshift_runner::ui::index::EventIndex;
    use owlshift_runner::ui::spend::{self, Cache, Query};
    let data = tempfile::tempdir().unwrap();
    let log = EventLog::in_dir(data.path()).path().to_owned();
    // UTC+2: 21:59 UTC is 23:59 local, 22:01 UTC is 00:01 local the next day.
    std::fs::write(
        &log,
        [
            started(
                "2026-11-05T21:00:00Z",
                "OWL-1",
                "r1",
                "build",
                Some("claude"),
            ),
            used("2026-11-05T21:59:00Z", "OWL-1", "r1", 1.0),
            used("2026-11-05T22:01:00Z", "OWL-1", "r1", 2.0),
        ]
        .concat(),
    )
    .unwrap();
    let index = std::sync::Mutex::new(EventIndex::new(data.path()));
    let cache = Cache::default();
    let personal = absent();
    let inputs = spend::Inputs {
        data: data.path(),
        index: &index,
        personal: &personal,
        cache: &cache,
        now: ts("2026-11-06T10:00:00Z"),
        tz: TimeZone::fixed(jiff::tz::offset(2)),
    };
    let answer = spend::answer(&inputs, &Query::default());
    assert_eq!(
        sum(&answer["days"], "day"),
        [
            ("2026-11-05".to_owned(), 1_000_000),
            ("2026-11-06".to_owned(), 2_000_000)
        ]
        .into_iter()
        .collect()
    );
}

#[test]
fn spends_rev_changes_with_everything_the_answer_shows_and_with_nothing_else() {
    use owlshift_runner::ui::index::EventIndex;
    use owlshift_runner::ui::spend::{self, Cache, Query};
    let data = tempfile::tempdir().unwrap();
    let log = EventLog::in_dir(data.path()).path().to_owned();
    let write_log = |extra: &str| {
        let mut text = started(
            "2026-11-05T09:00:00Z",
            "OWL-1",
            "r1",
            "build",
            Some("claude"),
        );
        text.push_str(&used("2026-11-05T09:30:00Z", "OWL-1", "r1", 1.0));
        text.push_str(extra);
        std::fs::write(&log, text).unwrap();
    };
    write_log("");
    let resets = ts("2026-11-05T12:00:00Z");
    UsageStore::in_data_dir(data.path())
        .update(Harness::Claude, |state| {
            state.windows.insert(
                "five_hour".to_owned(),
                Window {
                    share: 0.5,
                    resets_at_ms: resets.as_millisecond(),
                    read_at_ms: ts("2026-11-05T09:30:00Z").as_millisecond(),
                },
            );
        })
        .unwrap();
    let personal_file = data.path().join("personal.toml");
    let index = std::sync::Mutex::new(EventIndex::new(data.path()));
    let cache = Cache::default();
    let ask = |now: &str, rev: Option<&str>, personal: &FileState<PersonalConfig>| {
        let inputs = spend::Inputs {
            data: data.path(),
            index: &index,
            personal,
            cache: &cache,
            now: ts(now),
            tz: TimeZone::UTC,
        };
        spend::answer(
            &inputs,
            &Query {
                rev: rev.map(str::to_owned),
                ..Query::default()
            },
        )
    };
    let none = absent();
    let first = ask("2026-11-05T10:00:00Z", None, &none);
    let rev = first["rev"].as_str().unwrap().to_owned();
    // The same question, the same rev: nothing to redraw.
    let same = ask("2026-11-05T10:00:10Z", Some(&rev), &none);
    assert_eq!(same["unchanged"], true);
    // A usage cap changed in the personal file, no event appended.
    std::fs::write(
        &personal_file,
        "[harnesses.claude]\nusage_cap_percent = 40\n",
    )
    .unwrap();
    let capped = owlshift_runner::ui::personal_at(&personal_file);
    let changed = ask("2026-11-05T10:00:10Z", Some(&rev), &capped);
    assert_ne!(changed["rev"], rev.as_str());
    assert!(changed.get("unchanged").is_none());
    let rev_capped = changed["rev"].as_str().unwrap().to_owned();
    // A window that resets, its time passing with no file changed.
    let reset = ask("2026-11-05T13:00:00Z", Some(&rev_capped), &capped);
    assert!(reset.get("unchanged").is_none());
    // The local day passing with no file changed.
    let next_day = ask("2026-11-06T10:00:00Z", Some(&rev_capped), &capped);
    assert!(next_day.get("unchanged").is_none());
    // An appended usage event.
    let before = ask("2026-11-05T10:00:00Z", None, &capped);
    write_log(&used("2026-11-05T09:45:00Z", "OWL-1", "r1", 0.5));
    let after = ask("2026-11-05T10:00:00Z", before["rev"].as_str(), &capped);
    assert!(after.get("unchanged").is_none());
    assert_eq!(after["events"]["total_micros"], 1_500_000);
}

/// Writes `cost` as the cost of `ticket` in the ticket ref of the checkout of
/// the served project `repo` (made if need be), after `previous`.
fn put_cost(
    ground: &Ground,
    repo: &str,
    ticket: &str,
    cost: &str,
    previous: Option<&str>,
) -> String {
    let dirs = ProjectDirs::new(ground.data(), &Repo::parse(repo).unwrap());
    let checkout = dirs.checkout();
    let git = owlshift_runner::project::runner_git();
    if !checkout.exists() {
        std::fs::create_dir_all(&checkout).unwrap();
        for args in [
            &["init", "-q", "-b", "main"][..],
            &["config", "user.name", "Ada"],
            &["config", "user.email", "ada@example.com"],
        ] {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(&checkout)
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?}");
        }
    }
    let record = owlshift_runner::ticket_ref::blank(Some(
        owlshift_contracts::refs::TicketCost::parse(cost).unwrap(),
    ));
    owlshift_runner::ticket_ref::write(
        &git,
        &checkout,
        &owlshift_contracts::ids::TicketId::new(ticket).unwrap(),
        &record,
        previous,
    )
    .unwrap()
}

fn cost_json(runs: u32, unreported: u32, spent: u64, restarted: u64) -> String {
    format!(
        r#"{{"format":1,"runs":{runs},"unreported_runs":{unreported},"spent_micros":{spent},"restarted_at_micros":{restarted}}}"#
    )
}

#[test]
fn spend_shows_each_tickets_cost_json_against_the_budget_and_follows_the_budget_and_a_restart() {
    use owlshift_runner::ui::index::EventIndex;
    use owlshift_runner::ui::spend::{self, Cache, Query};
    let ground = Ground::new();
    ground.serve(&["acme/web"]);
    keep(&ground, "acme/web", PROJECT, None);
    // Three runs, one of which reported no cost, one restart at 2.5.
    let first = put_cost(
        &ground,
        "acme/web",
        "OWL-1",
        &cost_json(3, 1, 2_750_000, 2_500_000),
        None,
    );
    put_cost(
        &ground,
        "acme/web",
        "OWL-2",
        &cost_json(1, 0, 400_000, 0),
        None,
    );
    ground.append_log(&started(
        "2026-11-05T09:00:00Z",
        "OWL-1",
        "r1",
        "build",
        Some("claude"),
    ));
    ground.append_log(&used("2026-11-05T09:30:00Z", "OWL-1", "r1", 0.25));
    let index = std::sync::Mutex::new(EventIndex::new(ground.data()));
    // Every request reads the refs again, as a request after the 10 s the
    // refs are kept does.
    let cache = Cache::keeping_refs_for(Duration::ZERO);
    let none = absent();
    let ask = |rev: Option<&str>| {
        spend::answer(
            &spend::Inputs {
                data: ground.data(),
                index: &index,
                personal: &none,
                cache: &cache,
                now: ts("2026-11-05T10:00:00Z"),
                tz: TimeZone::UTC,
            },
            &Query {
                rev: rev.map(str::to_owned),
                ..Query::default()
            },
        )
    };
    let row = |answer: &Value, ticket: &str| {
        answer["tickets"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["ticket"] == ticket)
            .unwrap_or_else(|| panic!("no row for {ticket}: {answer}"))
            .clone()
    };
    let answer = ask(None);
    let one = row(&answer, "OWL-1");
    assert_eq!(
        one["cost"],
        json!({
            "runs": 3,
            "unreported_runs": 1,
            "spent_micros": 2_750_000,
            "restarted_at_micros": 2_500_000,
            // The spend since the restart, as the budget counts it.
            "counted_micros": 250_000,
            "budget_micros": 5_000_000,
        })
    );
    // The events' sum is its own column.
    assert_eq!(one["events_micros"], 250_000);
    let two = row(&answer, "OWL-2");
    assert_eq!(two["cost"]["spent_micros"], 400_000);
    assert_eq!(two["cost"]["counted_micros"], 400_000);
    assert_eq!(two["cost"]["unreported_runs"], 0);
    assert_eq!(two["events_micros"], 0);
    // The counted part is what the budget's own TicketSpend counts.
    let spend = owlshift_core::gate::TicketSpend::try_from(
        &owlshift_contracts::refs::TicketCost::parse(&cost_json(3, 1, 2_750_000, 2_500_000))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(one["cost"]["counted_micros"], spend.counted_micros());
    // The same question, the same rev: nothing to redraw.
    let rev = answer["rev"].as_str().unwrap().to_owned();
    assert_eq!(ask(Some(&rev))["unchanged"], true);
    // The budget changed in the file the project runs on.
    keep(
        &ground,
        "acme/web",
        &PROJECT.replace("ticket_budget_usd = 5.0", "ticket_budget_usd = 8.0"),
        None,
    );
    let budget = ask(Some(&rev));
    assert!(budget.get("unchanged").is_none(), "{budget}");
    assert_eq!(row(&budget, "OWL-1")["cost"]["budget_micros"], 8_000_000);
    assert_eq!(row(&budget, "OWL-2")["cost"]["budget_micros"], 8_000_000);
    let rev = budget["rev"].as_str().unwrap().to_owned();
    assert_eq!(ask(Some(&rev))["unchanged"], true);
    // A person restarted the ticket: the ref moves its restart to the spend.
    put_cost(
        &ground,
        "acme/web",
        "OWL-1",
        &cost_json(3, 1, 2_750_000, 2_750_000),
        Some(&first),
    );
    let restarted = ask(Some(&rev));
    assert!(restarted.get("unchanged").is_none(), "{restarted}");
    assert_eq!(row(&restarted, "OWL-1")["cost"]["counted_micros"], 0);
    assert_eq!(
        row(&restarted, "OWL-1")["cost"]["restarted_at_micros"],
        2_750_000
    );
    // A project file without a budget shows none.
    keep(
        &ground,
        "acme/web",
        &PROJECT.replace("ticket_budget_usd = 5.0\n", ""),
        None,
    );
    let unbudgeted = ask(None);
    assert!(row(&unbudgeted, "OWL-1")["cost"]["budget_micros"].is_null());
}

#[test]
fn spend_needs_a_session() {
    let ground = Ground::new();
    let none = send(ground.ui.port(), "GET", "/api/spend", None, "");
    assert_eq!(none.status, 401);
}

// ---- Config (OWL-472) ----

const PROJECT: &str = r#"requires = ">=0.0"

[tracker]
kind = "linear"
team = "WEB"
admit = { label = "agent" }
admitters = ["u1"]
states = { ready = "Todo", working = "Doing", needs_input = "Asked", review = "Review" }

[stack]
gate = ["cargo test"]

[pipeline]
default = "trivial"
plan_approval = "never"

[models]

[policy]
always_human = ["money"]
ticket_budget_usd = 5.0
"#;

/// A host that only knows git: the repository root and its origin, which is
/// all `Effective::load` asks.
struct GitOnly {
    root: PathBuf,
    origin: String,
}

impl owlshift_runner::system::System for GitOnly {
    fn locate(&self, program: &str) -> Option<PathBuf> {
        (program == "git").then(|| PathBuf::from("/scripted/git"))
    }

    fn run(
        &self,
        _program: &Path,
        args: &[&str],
        _cwd: Option<&Path>,
    ) -> Result<owlshift_runner::system::Captured, owlshift_runner::system::RunError> {
        let say = |text: String| owlshift_runner::system::Captured {
            code: Some(0),
            stdout: text.into_bytes(),
            stderr: Vec::new(),
        };
        match args {
            ["rev-parse", "--show-toplevel"] => Ok(say(format!("{}\n", self.root.display()))),
            ["remote", "get-url", "origin"] => {
                Ok(say(format!("git@github.com:{}.git\n", self.origin)))
            }
            other => panic!("unscripted git {other:?}"),
        }
    }
}

fn keep(ground: &Ground, repo: &str, text: &str, refused: Option<&str>) {
    use owlshift_contracts::service::{KeptFile, RefusedFile, ServedConfig};
    let dirs = ProjectDirs::new(ground.data(), &Repo::parse(repo).unwrap());
    std::fs::create_dir_all(dirs.root()).unwrap();
    let mut record = ServedConfig::new(repo);
    record.kept = Some(KeptFile {
        branch: "main".to_owned(),
        commit: "b".repeat(40),
        text: text.to_owned(),
        since: ts("2026-11-01T00:00:00Z"),
    });
    record.refused = refused.map(|reason| RefusedFile {
        commit: Some("c".repeat(40)),
        reason: reason.to_owned(),
        since: ts("2026-11-04T00:00:00Z"),
    });
    std::fs::write(owlshift_runner::served::record_file(&dirs), record.render()).unwrap();
}

fn lines_of(value: &Value) -> Vec<String> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|line| line.as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn a_served_projects_config_is_what_config_show_prints_in_its_checkout() {
    let ground = Ground::new();
    ground.serve(&["acme/web"]);
    keep(
        &ground,
        "acme/web",
        PROJECT,
        Some("requires a newer Owlshift"),
    );
    let answer = ground.json("/api/config");
    let project = &answer["projects"][0];
    assert_eq!(project["repository"], "acme/web");
    assert_eq!(project["record"]["kept"]["branch"], "main");
    assert_eq!(project["record"]["kept"]["commit"], "b".repeat(40));
    assert_eq!(
        project["record"]["refused"]["reason"],
        "requires a newer Owlshift"
    );
    assert!(
        project["record"]["kept"]["name"]
            .as_str()
            .unwrap()
            .contains("owlshift.toml at origin/main")
    );

    // The reference: `owlshift config show` run in a checkout holding the same
    // file as its owlshift.toml.
    let checkout = ProjectDirs::new(ground.data(), &Repo::parse("acme/web").unwrap()).checkout();
    std::fs::create_dir_all(&checkout).unwrap();
    std::fs::write(checkout.join("owlshift.toml"), PROJECT).unwrap();
    let host = GitOnly {
        root: checkout.clone(),
        origin: "acme/web".to_owned(),
    };
    let effective = owlshift_runner::config::Effective::load(
        &host,
        &checkout,
        Some(PathBuf::from("/nowhere/owlshift.toml")),
    );
    let want: Vec<String> = effective.to_string().lines().map(str::to_owned).collect();
    assert_eq!(lines_of(&project["config"]["lines"]), want);
    // The origin of each value is the checkout's file, and the budget and the
    // always-human addition come with theirs.
    let text = want.join("\n");
    let label = checkout.join("owlshift.toml").display().to_string();
    assert!(
        text.contains(&format!("ticket_budget_usd = 5.0  ({label})")),
        "{text}"
    );
    assert!(text.contains(&format!("money  ({label})")), "{text}");
    assert_eq!(project["refusal"], Value::Null);
    // The machine's report is the personal file's.
    assert!(
        answer["machine"]["personal_file"]
            .as_str()
            .unwrap()
            .contains("not found at")
    );
}

#[test]
fn a_kept_file_no_longer_accepted_shows_the_refusal_and_an_invalid_personal_file_its_error() {
    let ground = Ground::new();
    ground.serve(&["acme/web", "acme/new"]);
    keep(&ground, "acme/web", "not toml [", None);
    let answer = ground.json("/api/config");
    let web = &answer["projects"][0];
    assert_eq!(web["config"], Value::Null);
    assert!(
        web["refusal"]
            .as_str()
            .unwrap()
            .contains("is not accepted any more")
    );
    // Older than the first file read: nothing kept yet.
    let new = &answer["projects"][1];
    assert!(new["refusal"].as_str().unwrap().contains("is not served"));
    assert_eq!(new["record"]["kept"], Value::Null);

    let file = ground.data().join("personal.toml");
    std::fs::write(&file, "concurrent_runs = \"many\"\n").unwrap();
    *ground.personal.lock().unwrap() = FileState::Loaded {
        path: file,
        config: PersonalConfig::parse("").unwrap(),
        entries: Vec::new(),
    };
    let answer = ground.json("/api/config");
    assert!(
        answer["machine"]["personal_file"]
            .as_str()
            .unwrap()
            .contains("is invalid")
    );
}

#[test]
fn configs_rev_is_the_answers_and_changes_with_the_record_and_the_personal_file() {
    let ground = Ground::new();
    ground.serve(&["acme/web"]);
    keep(&ground, "acme/web", PROJECT, None);
    let first = ground.json("/api/config");
    let rev = first["rev"].as_str().unwrap().to_owned();
    let same = ground.json(&format!("/api/config?rev={rev}"));
    assert_eq!(same["unchanged"], true);
    // served.json rewritten.
    keep(
        &ground,
        "acme/web",
        PROJECT,
        Some("a newer file is refused"),
    );
    let rewritten = ground.json(&format!("/api/config?rev={rev}"));
    assert!(rewritten.get("unchanged").is_none());
    let rev = rewritten["rev"].as_str().unwrap().to_owned();
    // The personal file edited.
    let file = ground.data().join("personal.toml");
    std::fs::write(&file, "concurrent_runs = 3\n").unwrap();
    *ground.personal.lock().unwrap() = FileState::Loaded {
        path: file,
        config: PersonalConfig::parse("").unwrap(),
        entries: Vec::new(),
    };
    let edited = ground.json(&format!("/api/config?rev={rev}"));
    assert!(edited.get("unchanged").is_none());
    assert!(
        edited["machine"]["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["key"] == "concurrent_runs")
    );
}

#[test]
fn config_needs_a_session() {
    let ground = Ground::new();
    assert_eq!(
        send(ground.ui.port(), "GET", "/api/config", None, "").status,
        401
    );
}

// ---- Security (OWL-472) ----

fn listing(dir: &Path) -> std::collections::BTreeMap<PathBuf, (u64, std::time::SystemTime)> {
    let mut found = std::collections::BTreeMap::new();
    let mut pending = vec![dir.to_owned()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let entry = entry.unwrap();
            let meta = std::fs::symlink_metadata(entry.path()).unwrap();
            found.insert(entry.path(), (meta.len(), meta.modified().unwrap()));
            if meta.is_dir() {
                pending.push(entry.path());
            }
        }
    }
    found
}

#[test]
fn every_new_route_needs_a_session() {
    let ground = Ground::new();
    for path in [
        "/api/orchestrator",
        "/api/orchestrator?gen=0&after=0&log=0",
        "/api/spend",
        "/api/spend?days=all&rev=x",
        "/api/config",
        "/api/config?rev=x",
    ] {
        assert_eq!(
            send(ground.ui.port(), "GET", path, None, "").status,
            401,
            "{path}"
        );
    }
    assert_eq!(
        send(ground.ui.port(), "POST", "/api/doctor", None, "{}").status,
        401
    );
}

#[test]
fn no_new_route_writes_to_the_data_directory() {
    let ground = Ground::new();
    let _daemon = bench(&ground);
    keep(
        &ground,
        "acme/web",
        PROJECT,
        Some("a newer file is refused"),
    );
    ground.append_log(&started(
        &days_ago(1),
        "OWL-1",
        "r1",
        "build",
        Some("claude"),
    ));
    ground.append_log(&used(&days_ago(1), "OWL-1", "r1", 1.0));
    ground.append_log("not an event\n");
    let dir = daemon::service_dir(ground.data());
    std::fs::write(dir.join("daemon.log"), "a line\n").unwrap();
    let before = listing(ground.data());
    assert!(before.len() > 10);
    for path in [
        "/api/orchestrator",
        "/api/orchestrator?log=0",
        "/api/spend?days=all",
        "/api/spend?days=7&project=acme%2Fweb",
        "/api/config",
    ] {
        ground.json(path);
    }
    // Appending to the log is the test's, not the route's: nothing else changed.
    assert_eq!(listing(ground.data()), before);
}

// ---------------------------------------------------------------------------
// The actions (OWL-473): Cancel, on `acme/web` served on `PROJECT`, whose
// Linear team is `WEB`, and the Orchestrator's controls.

impl Ground {
    fn post(&self, path: &str, body: &Value) -> Reply {
        send(
            self.ui.port(),
            "POST",
            path,
            Some(&self.session),
            &body.to_string(),
        )
    }

    fn web(&self) -> ProjectDirs {
        ProjectDirs::new(self.data(), &Repo::parse("acme/web").unwrap())
    }

    /// The ticket ref of `ticket` in `acme/web`'s checkout, at `status`, its
    /// delivery followed by no watch.
    fn put_ref(&self, ticket: &str, status: owlshift_core::state::Status) {
        use owlshift_contracts::refs::PersistedState;
        use owlshift_core::state::TicketState;
        use owlshift_runner::ticket_ref::{self, TicketRecord};
        let checkout = self.web().checkout();
        std::fs::create_dir_all(&checkout).unwrap();
        if !checkout.join(".git").exists() {
            let init = std::process::Command::new("git")
                .args(["init", "-q"])
                .current_dir(&checkout)
                .output()
                .unwrap();
            assert!(init.status.success(), "{init:?}");
        }
        let git = owlshift_runner::project::runner_git();
        let ticket = owlshift_contracts::ids::TicketId::new(ticket).unwrap();
        let state = TicketState::restore(status, 1, 0, 0, 0, 0).unwrap();
        let record = TicketRecord {
            state: PersistedState::from(&state),
            ..ticket_ref::blank(None)
        };
        let previous = ticket_ref::read(&git, &checkout, &ticket)
            .unwrap()
            .map(|stored| stored.commit);
        ticket_ref::write(&git, &checkout, &ticket, &record, previous.as_deref()).unwrap();
    }

    /// The cancel standing on `ticket` of `acme/web`.
    fn cancel_of(&self, ticket: &str) -> Option<Option<Request>> {
        owlshift_runner::cancel::read(
            &self.web(),
            &owlshift_contracts::ids::TicketId::new(ticket).unwrap(),
        )
    }

    fn cancel(&self, body: Value) -> Reply {
        let mut body = body;
        body["project"] = json!("acme/web");
        self.post("/api/cancel", &body)
    }
}

/// Cancel refuses what `owlshift cancel` refuses, in its words, writing
/// nothing: a ticket parked already, one whose delivery ended, one at Ready
/// or with no ref while its lock is free, another team's ticket.
#[test]
fn cancel_refuses_what_the_command_refuses() {
    use owlshift_core::state::Status;
    use owlshift_core::vocab::Stage;
    let ground = Ground::new();
    ground.serve(&["acme/web"]);
    keep(&ground, "acme/web", PROJECT, None);
    ground.put_ref(
        "WEB-1",
        Status::Parked {
            at: Stage::Build,
            awaiting_input: false,
        },
    );
    ground.put_ref("WEB-2", Status::Active(Stage::Watch));
    ground.put_ref("WEB-3", Status::Active(Stage::Ready));
    for (ticket, words) in [
        (
            "WEB-1",
            "WEB-1 is already parked: `owlshift continue WEB-1` restarts it",
        ),
        ("WEB-2", "WEB-2's delivery has ended: nothing of it runs"),
        ("WEB-3", "WEB-3 waits in the ready column"),
        ("WEB-4", "Owlshift has nothing of WEB-4 in flight"),
        ("OWL-1", "OWL-1 is not a ticket of team WEB"),
    ] {
        let reply = ground.cancel(json!({ "ticket": ticket }));
        assert_eq!(reply.status, 409, "{ticket}: {}", reply.text);
        let error = reply.json()["error"].as_str().unwrap().to_owned();
        assert!(error.starts_with(words), "{ticket}: {error}");
        assert_eq!(ground.cancel_of(ticket), None, "{ticket}");
    }
}

/// Cancel writes the command's file by `ui`, with the reason given or the
/// UI's, and says what the command says; a ticket at Ready is cancelled
/// while a step of it runs.
#[test]
fn cancel_writes_the_commands_file_by_ui() {
    use owlshift_core::state::Status;
    use owlshift_core::vocab::Stage;
    let ground = Ground::new();
    ground.serve(&["acme/web"]);
    keep(&ground, "acme/web", PROJECT, None);
    ground.put_ref("WEB-5", Status::Active(Stage::Build));
    let reply = ground.cancel(json!({ "ticket": "WEB-5" }));
    assert_eq!(reply.status, 200, "{}", reply.text);
    let said = lines_of(&reply.json()["lines"]);
    assert!(said[0].starts_with("Asked to cancel WEB-5: its step under way stops"));
    assert!(
        said.iter()
            .any(|line| line.starts_with("No watch runs on this project now"))
    );
    let asked = ground.cancel_of("WEB-5").unwrap().unwrap();
    assert_eq!(asked.by, "ui");
    assert_eq!(asked.why, "asked from the web UI");
    let reply = ground.cancel(json!({ "ticket": "WEB-5", "reason": "wrong module" }));
    let said = lines_of(&reply.json()["lines"]);
    assert!(
        said[1].starts_with("It replaces the cancel asked by ui at "),
        "{said:?}"
    );
    assert_eq!(
        ground.cancel_of("WEB-5").unwrap().unwrap().why,
        "wrong module"
    );

    ground.put_ref("WEB-3", Status::Active(Stage::Ready));
    let ticket = owlshift_contracts::ids::TicketId::new("WEB-3").unwrap();
    let held = ground.web().ticket_lock(&ticket).unwrap().unwrap();
    let reply = ground.cancel(json!({ "ticket": "WEB-3" }));
    drop(held);
    assert_eq!(reply.status, 200, "{}", reply.text);
    assert!(
        lines_of(&reply.json()["lines"])
            .iter()
            .any(|line| line.starts_with("Owlshift keeps no record of WEB-3 yet")),
        "{}",
        reply.text
    );
    assert!(ground.cancel_of("WEB-3").is_some());
}

/// A project whose kept file the personal file no longer accepts is refused
/// with that reason, as its watch is, and nothing is written.
#[test]
fn cancel_on_a_project_whose_kept_file_is_refused_says_so() {
    use owlshift_core::state::Status;
    use owlshift_core::vocab::Stage;
    let ground = Ground::new();
    ground.serve(&["acme/web"]);
    let declaring = PROJECT.replace(
        "gate = [\"cargo test\"]",
        "gate = [\"cargo test\"]\ngate_env = [\"DATABASE_URL\"]",
    );
    keep(&ground, "acme/web", &declaring, None);
    ground.put_ref("WEB-5", Status::Active(Stage::Build));
    let file = ground.data().join("personal.toml");
    std::fs::write(&file, "allow_gate_env = [\"DATABASE_URL\"]\n").unwrap();
    *ground.personal.lock().unwrap() = FileState::Loaded {
        path: file.clone(),
        config: PersonalConfig::parse("").unwrap(),
        entries: Vec::new(),
    };
    std::fs::write(&file, "").unwrap();
    let reply = ground.cancel(json!({ "ticket": "WEB-5" }));
    assert_eq!(reply.status, 409, "{}", reply.text);
    let error = reply.json()["error"].as_str().unwrap().to_owned();
    assert!(
        error.starts_with("acme/web is not served: owlshift.toml at origin/main")
            && error.contains("is not accepted any more")
            && error.contains("DATABASE_URL"),
        "{error}"
    );
    assert_eq!(ground.cancel_of("WEB-5"), None);
    // Allowed again, the cancel is written.
    std::fs::write(&file, "allow_gate_env = [\"DATABASE_URL\"]\n").unwrap();
    assert_eq!(ground.cancel(json!({ "ticket": "WEB-5" })).status, 200);
    assert!(ground.cancel_of("WEB-5").is_some());
}

/// The Orchestrator's controls show the machine's pause and each served
/// project's, from their files.
#[test]
fn the_controls_show_each_pause() {
    let ground = Ground::new();
    ground.serve(&["acme/web", "acme/api"]);
    let api = ProjectDirs::new(ground.data(), &Repo::parse("acme/api").unwrap());
    let request = Request::new(BY_PAUSE, Timestamp::now(), "lunch break");
    requests::ask_pause(ground.data(), Some(&api), &request).unwrap();
    let controls = ground.json("/api/orchestrator")["controls"].clone();
    assert_eq!(controls["machine_pause"], Value::Null);
    assert_eq!(controls["projects"][0]["repository"], "acme/web");
    assert_eq!(controls["projects"][0]["paused"], Value::Null);
    assert_eq!(controls["projects"][1]["repository"], "acme/api");
    assert_eq!(controls["projects"][1]["paused"]["why"], "lunch break");
    assert_eq!(controls["projects"][1]["paused"]["by"], "owlshift pause");
    requests::ask_pause(ground.data(), None, &request).unwrap();
    let controls = ground.json("/api/orchestrator")["controls"].clone();
    assert_eq!(controls["machine_pause"]["why"], "lunch break");
}

// ---------------------------------------------------------------------------
// The personal file's edit (OWL-473): `POST /api/config/personal` on a
// personal file of the ground's own.

const PERSONAL: &str = "\
# This machine.
concurrent_runs = 2 # two at once
allow_gate_env = [\"DATABASE_URL\"]
";

impl Ground {
    /// The personal file, holding `text`, made the ground's.
    fn personal_file(&self, text: &str) -> PathBuf {
        let file = self.data().join("config").join("config.toml");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, text).unwrap();
        *self.personal.lock().unwrap() = FileState::Loaded {
            path: file.clone(),
            config: PersonalConfig::parse("").unwrap(),
            entries: Vec::new(),
        };
        file
    }

    /// The editor's part of the Config answer.
    fn editor(&self) -> Value {
        self.json("/api/config")["personal_edit"].clone()
    }

    /// `key` (dotted, no quoted part) set to `value` on the rev the page
    /// reads now.
    fn edit(&self, key: &str, value: Value) -> Reply {
        let rev = self.editor()["rev"].clone();
        self.edit_at(key, value, rev)
    }

    fn edit_at(&self, key: &str, value: Value, rev: Value) -> Reply {
        let path: Vec<&str> = key.split('.').collect();
        self.post(
            "/api/config/personal",
            &json!({ "path": path, "value": value, "rev": rev }),
        )
    }

    /// The decisions the event log holds about the personal file.
    fn edits(&self) -> Vec<serde_json::Map<String, Value>> {
        let text =
            std::fs::read_to_string(EventLog::in_dir(self.data()).path()).unwrap_or_default();
        text.lines()
            .map(|line| owlshift_contracts::event::Event::parse(line).unwrap())
            .filter(|event| event.kind == EventKind::Decision && event.data.contains_key("config"))
            .map(|event| {
                assert_eq!(event.project, "owlshift ui");
                assert_eq!(event.ticket, None);
                event.data
            })
            .collect()
    }
}

/// An accepted edit writes the key alone, its comments kept, records it,
/// and says when it applies.
#[test]
fn an_accepted_edit_is_written_and_recorded() {
    let ground = Ground::new();
    let file = ground.personal_file(PERSONAL);
    let reply = ground.edit("concurrent_runs", json!(4));
    assert_eq!(reply.status, 200, "{}", reply.text);
    let answer = reply.json();
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        PERSONAL.replace("= 2 #", "= 4 #")
    );
    assert_eq!(answer["applies"], "next_start");
    assert_eq!(
        lines_of(&answer["lines"]),
        [format!(
            "concurrent_runs = 4 is written to {}: it applies at the next start of the service.",
            file.display()
        )]
    );
    assert_eq!(answer["refused_projects"], json!([]));
    assert_eq!(answer["rev"], ground.editor()["rev"]);
    let reply = ground.edit("web_tools", json!(false));
    assert_eq!(reply.json()["applies"], "next_pass");
    let edits = ground.edits();
    assert_eq!(edits.len(), 2);
    assert_eq!(
        Value::Object(edits[0].clone()),
        json!({
            "config": "personal",
            "key": "concurrent_runs",
            "value": "4",
            "by": "ui",
            "applies": "next_start",
        })
    );
    assert_eq!(edits[1]["key"], "web_tools");
    assert_eq!(edits[1]["applies"], "next_pass");
    // A key removed is recorded with no value.
    let reply = ground.edit("web_tools", Value::Null);
    assert!(lines_of(&reply.json()["lines"])[0].starts_with("web_tools is removed from "));
    assert_eq!(ground.edits()[2]["value"], Value::Null);
}

/// Saving a key as it stands, or removing one that is not set, changes
/// nothing: the file, or its absence, and the event log are left as they
/// are, and the answer says nothing is written.
#[test]
fn an_edit_that_changes_nothing_writes_and_records_nothing() {
    let ground = Ground::new();
    let file = ground.personal_file(PERSONAL);
    let events = EventLog::in_dir(ground.data()).path().to_owned();
    let logged = std::fs::read(&events).ok();
    for (key, value, said) in [
        ("concurrent_runs", json!(2), "concurrent_runs = 2 is in "),
        ("web_tools", Value::Null, "web_tools is not set in "),
    ] {
        let rev = ground.editor()["rev"].clone();
        let reply = ground.edit(key, value);
        assert_eq!(reply.status, 200, "{}", reply.text);
        let answer = reply.json();
        let lines = lines_of(&answer["lines"]);
        assert!(lines[0].starts_with(said), "{lines:?}");
        assert!(lines[0].ends_with(": nothing is written."), "{lines:?}");
        assert_eq!(answer["rev"], rev);
    }
    assert_eq!(std::fs::read_to_string(&file).unwrap(), PERSONAL);
    assert_eq!(std::fs::read(&events).ok(), logged, "no event is written");

    // A removal from a personal file not there leaves it absent.
    std::fs::remove_file(&file).unwrap();
    *ground.personal.lock().unwrap() = FileState::Absent(file.clone());
    let reply = ground.edit_at("ui.port", Value::Null, Value::Null);
    assert_eq!(reply.status, 200, "{}", reply.text);
    assert_eq!(reply.json()["rev"], Value::Null);
    assert!(!file.exists());
    assert_eq!(std::fs::read(&events).ok(), logged, "no event is written");
}

/// An edit the loader refuses, a stale one, a key outside the table and a
/// body that does not read write nothing and record nothing.
#[test]
fn a_refused_edit_writes_and_records_nothing() {
    let ground = Ground::new();
    let file = ground.personal_file(PERSONAL);
    let refused = ground.edit("concurrent_runs", json!(40));
    assert_eq!(refused.status, 422, "{}", refused.text);
    assert!(
        refused.json()["error"]
            .as_str()
            .unwrap()
            .contains("concurrent_runs must be a whole number from 1 to 16")
    );
    let stale = ground.edit_at("concurrent_runs", json!(3), json!("0".repeat(64)));
    assert_eq!(stale.status, 409, "{}", stale.text);
    for key in ["foo", "ui.nope"] {
        assert_eq!(ground.edit(key, json!(1)).status, 400, "{key}");
    }
    assert_eq!(ground.edit("keep_awake", json!("yes")).status, 400);
    for body in [
        json!({ "path": [], "value": 1, "rev": null }),
        json!({ "path": "ui.port", "value": 1, "rev": null }),
        json!({ "path": ["ui", "port"], "rev": null }),
        json!({ "path": ["ui", "port"], "value": 1 }),
    ] {
        assert_eq!(
            ground.post("/api/config/personal", &body).status,
            400,
            "{body}"
        );
    }
    assert_eq!(std::fs::read_to_string(&file).unwrap(), PERSONAL);
    assert!(ground.edits().is_empty());
    assert_eq!(
        send(
            ground.ui.port(),
            "POST",
            "/api/config/personal",
            None,
            &json!({ "path": ["keep_awake"], "value": true, "rev": null }).to_string()
        )
        .status,
        401
    );
}

/// An edit the loader accepts that a served project's kept file no longer
/// suits names the project and why, as its watch will be refused.
#[test]
fn an_edit_names_the_served_projects_it_refuses() {
    let ground = Ground::new();
    ground.serve(&["acme/web"]);
    let declaring = PROJECT.replace(
        "gate = [\"cargo test\"]",
        "gate = [\"cargo test\"]\ngate_env = [\"DATABASE_URL\"]",
    );
    keep(&ground, "acme/web", &declaring, None);
    ground.personal_file(PERSONAL);
    let reply = ground.edit("allow_gate_env", json!([]));
    assert_eq!(reply.status, 200, "{}", reply.text);
    let refused = reply.json()["refused_projects"].clone();
    assert_eq!(refused.as_array().unwrap().len(), 1, "{refused}");
    assert_eq!(refused[0]["repository"], "acme/web");
    assert_eq!(refused[0]["already"], false);
    assert!(
        refused[0]["why"].as_str().unwrap().contains("DATABASE_URL"),
        "{refused}"
    );
    // Still refused after another edit: already so.
    let reply = ground.edit("keep_awake", json!(true));
    assert_eq!(reply.json()["refused_projects"][0]["already"], true);
}

/// An event that cannot be recorded leaves the edit written and says so.
#[test]
fn an_edit_whose_event_cannot_be_recorded_says_so() {
    let ground = Ground::new();
    let file = ground.personal_file(PERSONAL);
    std::fs::create_dir_all(EventLog::in_dir(ground.data()).path()).unwrap();
    let reply = ground.edit("keep_awake", json!(false));
    assert_eq!(reply.status, 200, "{}", reply.text);
    let said = lines_of(&reply.json()["lines"]);
    assert_eq!(said.len(), 2, "{said:?}");
    assert!(
        said[1].starts_with("owlshift: warning: the event was not recorded"),
        "{said:?}"
    );
    assert!(
        std::fs::read_to_string(&file)
            .unwrap()
            .contains("keep_awake = false")
    );
}

/// The Config answer gives the editor every key of the table with its kind,
/// when it applies, who reads it and its value now, the repositories the
/// file names and the file's rev; a file not there yet is made by an edit.
#[test]
fn the_config_answer_gives_the_editor_every_key() {
    let ground = Ground::new();
    let file = ground.personal_file(
        "concurrent_runs = 3\n[repositories.\"github.com/acme/web\"]\nallow_caches = [\".m2\"]\n",
    );
    let editor = ground.editor();
    assert_eq!(editor["editable"], true);
    assert_eq!(editor["path"], file.display().to_string());
    let keys = editor["keys"].as_array().unwrap();
    let table = owlshift_runner::personal_edit::KEYS;
    let plain = table
        .iter()
        .filter(|info| !info.path.contains(&"*"))
        .count();
    assert_eq!(keys.len(), plain);
    for key in keys {
        assert!(
            ["next_pass", "next_start"].contains(&key["applies"].as_str().unwrap()),
            "{key}"
        );
        assert!(!key["about"].as_str().unwrap().is_empty(), "{key}");
    }
    let runs = keys
        .iter()
        .find(|key| key["key"] == "concurrent_runs")
        .unwrap();
    assert_eq!(runs["value"], 3);
    assert_eq!(runs["kind"], "integer");
    assert_eq!(runs["applies"], "next_start");
    let fallback = keys
        .iter()
        .find(|key| key["key"] == "harnesses.claude.fallback")
        .unwrap();
    assert_eq!(fallback["kind"], "harness");
    assert_eq!(fallback["value"], Value::Null);
    let repository = &editor["repositories"][0];
    assert_eq!(repository["repository"], "github.com/acme/web");
    assert_eq!(
        repository["keys"][1]["key"],
        "repositories.\"github.com/acme/web\".allow_caches"
    );
    assert_eq!(repository["keys"][1]["value"], json!([".m2"]));
    assert_eq!(editor["repository_keys"].as_array().unwrap().len(), 2);

    std::fs::remove_file(&file).unwrap();
    let editor = ground.editor();
    assert_eq!(editor["rev"], Value::Null);
    let reply = ground.edit_at("ui.port", json!(18500), Value::Null);
    assert_eq!(reply.status, 200, "{}", reply.text);
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "[ui]\nport = 18500\n"
    );
}
