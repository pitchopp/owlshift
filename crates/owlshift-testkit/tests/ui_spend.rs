//! The Spend screen over HTTP (P8 part 7, OWL-472, OWL-510): `GET /api/spend`
//! through the real server and a session, against ticket refs that hold a
//! `cost.json` and the budget of the project file the served project keeps.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use jiff::Timestamp;
use owlshift_adapters::forge::Repo;
use owlshift_contracts::format::Format;
use owlshift_contracts::ids::TicketId;
use owlshift_contracts::refs::TicketCost;
use owlshift_contracts::service::{KeptFile, ServedConfig, ServedProject, ServedProjects};
use owlshift_runner::config::FileState;
use owlshift_runner::executor::Git;
use owlshift_runner::project::ProjectDirs;
use owlshift_runner::served::{self, Registry};
use owlshift_runner::ticket_ref;
use owlshift_runner::ui::auth::{self, RunSecret, Session};
use owlshift_runner::ui::server::{self, Options, Serving};
use owlshift_runner::ui::token;
use owlshift_testkit::git::GitEnv;
use serde_json::{Value, json};

const REPO: &str = "acme/web";

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

/// The project file with `ticket_budget_usd` (none: no budget).
fn project(budget: Option<&str>) -> String {
    match budget {
        Some(usd) => PROJECT.replace(
            "always_human = []\n",
            &format!("always_human = []\nticket_budget_usd = {usd}\n"),
        ),
        None => PROJECT.to_owned(),
    }
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

/// A data directory holding one served project with a git checkout, and the
/// UI over it.
struct Ground {
    _tmp: tempfile::TempDir,
    git: Git,
    data: PathBuf,
    ui: Option<Serving>,
    session: Session,
}

impl Ground {
    fn checkout(&self) -> PathBuf {
        ProjectDirs::new(&self.data, &Repo::parse(REPO).unwrap()).checkout()
    }

    /// Writes the kept project file of the served project.
    fn keep(&self, text: &str) {
        let repo = Repo::parse(REPO).unwrap();
        let dirs = ProjectDirs::new(&self.data, &repo);
        let mut record = ServedConfig::new(REPO);
        record.kept = Some(KeptFile {
            branch: "main".to_owned(),
            commit: "a".repeat(40),
            text: text.to_owned(),
            since: "2026-11-01T00:00:00Z".parse::<Timestamp>().unwrap(),
        });
        std::fs::write(served::record_file(&dirs), record.render()).unwrap();
    }

    /// Plants the ticket's ref with the `cost.json` `cost`, on top of
    /// `previous`; the commit it makes.
    fn plant(&self, ticket: &str, cost: &Value, previous: Option<&str>) -> String {
        let cost = TicketCost::parse(&cost.to_string()).unwrap();
        ticket_ref::write(
            &self.git,
            &self.checkout(),
            &TicketId::new(ticket).unwrap(),
            &ticket_ref::blank(Some(cost)),
            previous,
        )
        .unwrap()
    }

    /// Stops the UI and serves the same data directory again: a fresh refs
    /// cache and a new session, as after a restart of the daemon.
    fn restart_ui(&mut self) {
        drop(self.ui.take());
        let (ui, session) = serve_ui(&self.data);
        self.ui = Some(ui);
        self.session = session;
    }

    fn spend(&self, query: &str) -> Reply {
        let port = self.ui.as_ref().unwrap().port();
        send(
            port,
            "GET",
            &format!("/api/spend{query}"),
            Some(&self.session),
            "",
        )
    }
}

fn serve_ui(data: &Path) -> (Serving, Session) {
    let options = Options {
        personal: Arc::new(|| FileState::Absent(PathBuf::from("/nowhere/owlshift.toml"))),
        ..Options::default()
    };
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let ui = server::start(data, listener, RunSecret::new().unwrap(), None, options).unwrap();
    let token = token::read_token(data).unwrap();
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
    (ui, session)
}

/// Serves `acme/web` on the kept file `text`, in a checkout that is a git
/// repository.
fn ground(text: &str) -> Ground {
    let tmp = tempfile::tempdir().unwrap();
    let env = GitEnv::create(tmp.path().join("home")).unwrap();
    let data = tmp.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    let served = ServedProjects {
        format: Format,
        projects: vec![ServedProject {
            repository: REPO.to_owned(),
            checkout: format!("/home/ada/{REPO}"),
        }],
    };
    let file = Registry::in_data_dir(&data).file();
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(file, served.render()).unwrap();
    let dirs = ProjectDirs::new(&data, &Repo::parse(REPO).unwrap());
    std::fs::create_dir_all(dirs.root()).unwrap();
    env.run(dirs.root(), &["init", "--quiet", "-b", "main", "checkout"])
        .unwrap();
    let git = Git::with_setup("git", move |command| env.apply(command));
    let (ui, session) = serve_ui(&data);
    let ground = Ground {
        _tmp: tmp,
        git,
        data,
        ui: Some(ui),
        session,
    };
    ground.keep(text);
    ground
}

#[test]
fn a_served_project_with_no_ticket_ref_shows_no_ticket() {
    let ground = ground(&project(Some("5.0")));
    let reply = ground.spend("");
    assert_eq!(reply.status, 200, "{}", reply.text);
    assert!(reply.content_type.starts_with("application/json"));
    let body = reply.json();
    assert_eq!(body["tickets"], json!([]));
    assert!(body["rev"].as_str().is_some_and(|rev| !rev.is_empty()));
    let port = ground.ui.as_ref().unwrap().port();
    assert_eq!(send(port, "GET", "/api/spend", None, "").status, 401);
}

fn cost(runs: u32, unreported: u32, spent: u64, restarted: u64) -> Value {
    json!({
        "format": 1,
        "runs": runs,
        "unreported_runs": unreported,
        "spent_micros": spent,
        "restarted_at_micros": restarted,
    })
}

/// The row of `ticket` in a Spend answer.
fn row(body: &Value, ticket: &str) -> Value {
    body["tickets"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["ticket"] == ticket)
        .unwrap_or_else(|| panic!("no row for {ticket}: {body}"))
        .clone()
}

fn rev(body: &Value) -> String {
    body["rev"].as_str().unwrap().to_owned()
}

/// OWL-1: three runs, one without a cost, restarted once after 2.5 dollars;
/// OWL-2: one run, never restarted.
fn plant_two(ground: &Ground) -> (String, String) {
    (
        ground.plant("OWL-1", &cost(3, 1, 2_750_000, 2_500_000), None),
        ground.plant("OWL-2", &cost(1, 0, 400_000, 0), None),
    )
}

#[test]
fn spend_over_http_shows_two_tickets_cost_json_against_the_budget() {
    let ground = ground(&project(Some("5.0")));
    plant_two(&ground);
    let reply = ground.spend("");
    assert_eq!(reply.status, 200, "{}", reply.text);
    let body = reply.json();
    assert_eq!(body["tickets"].as_array().unwrap().len(), 2);
    let one = row(&body, "OWL-1");
    assert_eq!(one["project"], REPO);
    // counted_micros is what was spent since the restart.
    assert_eq!(
        one["cost"],
        json!({
            "runs": 3,
            "unreported_runs": 1,
            "spent_micros": 2_750_000,
            "restarted_at_micros": 2_500_000,
            "counted_micros": 250_000,
            "budget_micros": 5_000_000,
        })
    );
    assert_eq!(
        row(&body, "OWL-2")["cost"],
        json!({
            "runs": 1,
            "unreported_runs": 0,
            "spent_micros": 400_000,
            "restarted_at_micros": 0,
            "counted_micros": 400_000,
            "budget_micros": 5_000_000,
        })
    );
    // The rows come largest spend first.
    assert_eq!(body["tickets"][0]["ticket"], "OWL-1");

    // The page's own rev answers unchanged, and the project filter keeps both.
    let same = ground.spend(&format!("?rev={}", rev(&body))).json();
    assert_eq!(same["unchanged"], true);
    let named = ground.spend("?project=acme%2Fweb").json();
    assert_eq!(named["tickets"], body["tickets"]);
    let other = ground.spend("?project=acme%2Fother").json();
    assert_eq!(other["tickets"], json!([]));
}

#[test]
fn a_budget_change_and_a_restart_each_give_a_new_rev() {
    let mut ground = ground(&project(Some("5.0")));
    let (one_commit, _) = plant_two(&ground);
    let first = ground.spend("").json();
    let first_rev = rev(&first);

    // The kept file is read on every request: a new budget shows at once on
    // the running server, with a new rev.
    ground.keep(&project(Some("8.0")));
    let budgeted = ground.spend(&format!("?rev={first_rev}")).json();
    assert!(budgeted.get("unchanged").is_none(), "{budgeted}");
    let budget_rev = rev(&budgeted);
    assert_ne!(budget_rev, first_rev);
    for ticket in ["OWL-1", "OWL-2"] {
        assert_eq!(row(&budgeted, ticket)["cost"]["budget_micros"], 8_000_000);
    }
    assert_eq!(
        ground.spend(&format!("?rev={budget_rev}")).json()["unchanged"],
        true
    );

    // A restart of the server with nothing changed keeps the rev, so a new
    // rev below is the ticket's restart and nothing else.
    ground.restart_ui();
    let again = ground.spend(&format!("?rev={budget_rev}")).json();
    assert_eq!(again["unchanged"], true, "{again}");

    // OWL-1 is restarted: its restart mark moves to what was spent. The new
    // server reads the refs afresh, as the daemon does after a restart.
    ground.plant(
        "OWL-1",
        &cost(3, 1, 2_750_000, 2_750_000),
        Some(&one_commit),
    );
    ground.restart_ui();
    let restarted = ground.spend(&format!("?rev={budget_rev}")).json();
    assert!(restarted.get("unchanged").is_none(), "{restarted}");
    assert_ne!(rev(&restarted), budget_rev);
    let one = row(&restarted, "OWL-1");
    assert_eq!(one["cost"]["restarted_at_micros"], 2_750_000);
    assert_eq!(one["cost"]["spent_micros"], 2_750_000);
    assert_eq!(one["cost"]["counted_micros"], 0);
    assert_eq!(one["cost"]["budget_micros"], 8_000_000);
    assert_eq!(
        row(&restarted, "OWL-2")["cost"],
        row(&budgeted, "OWL-2")["cost"]
    );

    // A file with no ticket_budget_usd leaves no budget.
    ground.keep(&project(None));
    let none = ground.spend(&format!("?rev={}", rev(&restarted))).json();
    assert!(none.get("unchanged").is_none(), "{none}");
    assert_eq!(row(&none, "OWL-1")["cost"]["budget_micros"], Value::Null);
    assert_eq!(row(&none, "OWL-2")["cost"]["budget_micros"], Value::Null);
}
