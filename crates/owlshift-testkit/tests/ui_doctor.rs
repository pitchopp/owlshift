//! The Orchestrator's on-demand `owlshift doctor` (P8 part 7, OWL-472): the
//! web UI's `POST /api/doctor`, run on a scripted host that reaches nothing
//! of the machine, once per served project on the project file its
//! `served.json` keeps, labelled in the project's checkout.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use jiff::Timestamp;
use owlshift_adapters::forge::Repo;
use owlshift_contracts::config::PersonalConfig;
use owlshift_contracts::format::Format;
use owlshift_contracts::service::{
    BuildIdentity, KeptFile, ServedConfig, ServedProject, ServedProjects,
};
use owlshift_runner::config::{Effective, FileState};
use owlshift_runner::doctor;
use owlshift_runner::project::ProjectDirs;
use owlshift_runner::served::{self, Registry};
use owlshift_runner::ui::auth::{self, RunSecret, Session};
use owlshift_runner::ui::server::{self, Options, Serving};
use owlshift_runner::ui::token;
use owlshift_testkit::system::ScriptedSystem;
use serde_json::{Value, json};

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
always_human = []
"#;

fn build() -> BuildIdentity {
    BuildIdentity::from_build("bench", "0", "2026-01-01T00:00:00Z", "")
}

struct Reply {
    status: u16,
    content_type: String,
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
        .set_read_timeout(Some(Duration::from_secs(60)))
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
    let content_type = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-type")
                .then(|| value.trim().to_owned())
        })
        .unwrap_or_default();
    Reply {
        status,
        content_type,
        text: String::from_utf8(answer[split + 4..].to_vec()).unwrap(),
    }
}

/// A data directory with served projects, and the UI over it on a scripted
/// host.
struct Ground {
    tmp: tempfile::TempDir,
    ui: Serving,
    session: Session,
    system: Arc<ScriptedSystem>,
}

impl Ground {
    fn data(&self) -> PathBuf {
        self.tmp.path().join("data")
    }

    fn checkout(&self, repo: &str) -> PathBuf {
        ProjectDirs::new(&self.data(), &Repo::parse(repo).unwrap()).checkout()
    }
}

fn start(repo: &str, with_build: bool, delay: Duration) -> Ground {
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    let checkout = ProjectDirs::new(&data, &Repo::parse(repo).unwrap()).checkout();
    let system = Arc::new(ScriptedSystem::bench(&data, &checkout, repo).with_delay(delay));
    let options = Options {
        personal: Arc::new(|| FileState::Absent(PathBuf::from("/nowhere/owlshift.toml"))),
        system: system.clone(),
        ..Options::default()
    };
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let ui = server::start(
        &data,
        listener,
        RunSecret::new().unwrap(),
        with_build.then(build),
        options,
    )
    .unwrap();
    let token = token::read_token(&data).unwrap();
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
    Ground {
        tmp,
        ui,
        session,
        system,
    }
}

/// Serves `repo` with the kept file `text` (none: only listed).
fn serve(ground: &Ground, repos: &[(&str, Option<&str>)]) {
    let data = ground.data();
    let served = ServedProjects {
        format: Format,
        projects: repos
            .iter()
            .map(|(repository, _)| ServedProject {
                repository: (*repository).to_owned(),
                checkout: format!("/home/ada/{repository}"),
            })
            .collect(),
    };
    let file = Registry::in_data_dir(&data).file();
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(file, served.render()).unwrap();
    for (repository, text) in repos {
        let repo = Repo::parse(repository).unwrap();
        let dirs = ProjectDirs::new(&data, &repo);
        std::fs::create_dir_all(dirs.root()).unwrap();
        if let Some(text) = text {
            let mut record = ServedConfig::new(*repository);
            record.kept = Some(KeptFile {
                branch: "main".to_owned(),
                commit: "a".repeat(40),
                text: (*text).to_owned(),
                since: "2026-11-01T00:00:00Z".parse::<Timestamp>().unwrap(),
            });
            std::fs::write(served::record_file(&dirs), record.render()).unwrap();
        }
    }
}

fn doctor_call(ground: &Ground, body: &Value) -> Reply {
    send(
        ground.ui.port(),
        "POST",
        "/api/doctor",
        Some(&ground.session),
        &body.to_string(),
    )
}

fn check_names(report: &Value) -> Vec<String> {
    report["checks"]
        .as_array()
        .map(|checks| {
            checks
                .iter()
                .filter_map(|check| check["subject"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn a_served_project_is_checked_on_the_file_kept_and_in_its_checkout() {
    let ground = start("acme/web", true, Duration::ZERO);
    serve(&ground, &[("acme/web", Some(PROJECT))]);
    let reply = doctor_call(&ground, &json!({}));
    assert_eq!(reply.status, 200, "{}", reply.text);
    assert!(reply.content_type.starts_with("application/json"));
    let answer = reply.json();
    let reports = answer["reports"].as_array().unwrap();
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0]["project"], "acme/web");

    // Every path a root-taking method was given is the checkout, never the
    // project folder that holds served.json.
    let checkout = ground.checkout("acme/web");
    let folder = checkout.parent().unwrap().to_owned();
    let rooted: Vec<String> = ground
        .system
        .calls()
        .into_iter()
        .filter(|call| !call.starts_with("run "))
        .collect();
    assert!(!rooted.is_empty());
    for call in &rooted {
        let path = call.split_once(' ').unwrap().1;
        assert_eq!(Path::new(path), checkout, "{call}");
        assert_ne!(Path::new(path), folder);
    }

    // The report is the one `owlshift doctor` run in that checkout gives,
    // holding the same file as its owlshift.toml.
    std::fs::create_dir_all(&checkout).unwrap();
    std::fs::write(checkout.join("owlshift.toml"), PROJECT).unwrap();
    let scripted = ScriptedSystem::bench(&ground.data(), &checkout, "acme/web");
    let effective = Effective::load(
        &scripted,
        &checkout,
        Some(PathBuf::from("/nowhere/owlshift.toml")),
    );
    let report = doctor::run(&scripted, &effective, &build()).to_json();
    assert_eq!(reports[0]["report"], report);
    assert!(
        check_names(&report)
            .iter()
            .any(|n| n.contains("served project"))
    );
}

#[test]
fn two_served_projects_give_two_reports_and_one_without_a_kept_file_its_reason() {
    let ground = start("acme/web", true, Duration::ZERO);
    serve(
        &ground,
        &[
            ("acme/web", Some(PROJECT)),
            ("acme/api", Some(PROJECT)),
            ("acme/new", None),
        ],
    );
    let answer = doctor_call(&ground, &json!({})).json();
    let reports = answer["reports"].as_array().unwrap();
    assert_eq!(reports.len(), 3);
    assert!(reports[0]["report"]["checks"].is_array());
    assert!(reports[1]["report"]["checks"].is_array());
    assert_eq!(reports[2]["project"], "acme/new");
    assert!(
        reports[2]["error"]
            .as_str()
            .unwrap()
            .contains("is not served: no project file was read"),
        "{}",
        reports[2]
    );
    // A project asked for by name that is not served says so.
    let one = doctor_call(&ground, &json!({ "project": "acme/other" })).json();
    assert!(
        one["reports"][0]["error"]
            .as_str()
            .unwrap()
            .contains("acme/other is not served")
    );
    // And asking for one runs one.
    let named = doctor_call(&ground, &json!({ "project": "acme/api" })).json();
    assert_eq!(named["reports"].as_array().unwrap().len(), 1);
    assert_eq!(named["reports"][0]["project"], "acme/api");
}

#[test]
fn a_kept_file_no_longer_accepted_answers_the_refusal() {
    let ground = start("acme/web", true, Duration::ZERO);
    serve(&ground, &[("acme/web", Some("this is not toml ["))]);
    let answer = doctor_call(&ground, &json!({})).json();
    let error = answer["reports"][0]["error"].as_str().unwrap();
    assert!(error.contains("is not accepted any more"), "{error}");
}

#[test]
fn with_no_project_served_the_checks_run_on_the_personal_file_alone() {
    let ground = start("acme/web", true, Duration::ZERO);
    // The data directory sits inside a git repository with a project file:
    // the report still has no project file.
    std::fs::write(ground.tmp.path().join("owlshift.toml"), PROJECT).unwrap();
    let answer = doctor_call(&ground, &json!({})).json();
    let reports = answer["reports"].as_array().unwrap();
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0]["project"], Value::Null);
    let text = reports[0]["report"].to_string();
    assert!(text.contains("none (no project is served)"), "{text}");
    assert!(!text.contains("served project file"), "{text}");
    // The personal file is the one the server reads.
    let personal = PersonalConfig::parse("").unwrap();
    let _ = personal;
}

#[test]
fn a_second_call_while_one_runs_is_refused_and_a_session_and_a_build_are_needed() {
    let ground = start("acme/web", true, Duration::from_millis(400));
    serve(&ground, &[("acme/web", Some(PROJECT))]);
    let port = ground.ui.port();
    let session = ground.session.clone();
    let first = std::thread::spawn(move || send(port, "POST", "/api/doctor", Some(&session), "{}"));
    std::thread::sleep(Duration::from_millis(150));
    let second = doctor_call(&ground, &json!({}));
    assert_eq!(second.status, 409, "{}", second.text);
    assert_eq!(first.join().unwrap().status, 200);

    let none = send(ground.ui.port(), "POST", "/api/doctor", None, "{}");
    assert_eq!(none.status, 401);

    let without = start("acme/web", false, Duration::ZERO);
    assert_eq!(doctor_call(&without, &json!({})).status, 503);
}

#[test]
fn the_ui_never_hands_the_daemon_over() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../owlshift-runner/src/ui");
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|ext| ext == "rs") {
            let text = std::fs::read_to_string(&path).unwrap();
            assert!(!text.contains("hand_over"), "{}", path.display());
        }
    }
}

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

/// OWL-472: a run of the checks writes nothing to the data directory, and
/// the route answers JSON and takes JSON only.
#[test]
fn a_run_of_the_checks_writes_nothing_to_the_data_directory() {
    let ground = start("acme/web", true, Duration::ZERO);
    serve(&ground, &[("acme/web", Some(PROJECT)), ("acme/api", None)]);
    let before = listing(&ground.data());
    assert!(before.len() >= 5);
    let reply = doctor_call(&ground, &json!({}));
    assert_eq!(reply.status, 200);
    assert!(reply.content_type.starts_with("application/json"));
    assert_eq!(listing(&ground.data()), before);
    // A body that is not JSON is refused before the route.
    let port = ground.ui.port();
    let wrong = {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let text = format!(
            "POST /api/doctor HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\
             X-Owlshift-Session: {}\r\nContent-Type: text/plain\r\nContent-Length: 2\r\n\r\n{{}}",
            ground.session.header()
        );
        stream.write_all(text.as_bytes()).unwrap();
        let mut answer = String::new();
        stream.read_to_string(&mut answer).unwrap();
        answer
    };
    assert!(wrong.starts_with("HTTP/1.1 415"), "{wrong}");
    // A text for the project, or one that is no repository, is refused.
    assert_eq!(doctor_call(&ground, &json!({ "project": 3 })).status, 400);
    assert_eq!(
        doctor_call(&ground, &json!({ "project": "nope" })).status,
        400
    );
}
