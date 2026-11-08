//! `owlshift-ui-bench` (OWL-508) on the bench the browsers' checklist runs on:
//! it serves the real web UI on a data directory and nothing else, and its
//! two mutators change what a page open in a browser shows.

use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::TcpStream;
use std::process::Stdio;
use std::time::Instant;

use owlshift_runner::ui::auth;
use owlshift_runner::ui::server::{self, SESSION_HEADER};
use owlshift_runner::ui::token;

use super::overview_scenarios::browsers_bench;
use super::*;

const BIN: &str = env!("CARGO_BIN_EXE_owlshift-ui-bench");

/// The server, ended with the test.
struct Serving(Child);

impl Drop for Serving {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// One request, answered to its end: the status and the JSON body.
fn request(port: u16, method: &str, path: &str, session: Option<&str>, body: &str) -> (u16, Value) {
    let mut text = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n");
    if let Some(session) = session {
        text.push_str(&format!("{SESSION_HEADER}: {session}\r\n"));
    }
    if method == "POST" {
        text.push_str(&format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            body.len()
        ));
    }
    text.push_str("Connection: close\r\n\r\n");
    text.push_str(body);
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    stream.write_all(text.as_bytes()).unwrap();
    let mut answer = Vec::new();
    stream.read_to_end(&mut answer).unwrap();
    let split = answer
        .windows(4)
        .position(|four| four == b"\r\n\r\n")
        .expect("a response's head");
    let head = String::from_utf8_lossy(&answer[..split]).into_owned();
    let status = head.split(' ').nth(1).unwrap().parse().unwrap();
    let mut rest = &answer[split + 4..];
    let mut body = Vec::new();
    if head
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked")
    {
        while let Some(end) = rest.windows(2).position(|pair| pair == b"\r\n") {
            let size = usize::from_str_radix(std::str::from_utf8(&rest[..end]).unwrap().trim(), 16)
                .unwrap();
            if size == 0 {
                break;
            }
            body.extend_from_slice(&rest[end + 2..end + 2 + size]);
            rest = &rest[end + 2 + size + 2..];
        }
    } else {
        body = rest.to_vec();
    }
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

fn bin(command: &str, data: &Path) -> std::process::Output {
    Command::new(BIN)
        .arg(command)
        .arg(data)
        .output()
        .expect("the bench binary runs")
}

fn until<T>(what: &str, mut poll: impl FnMut() -> Option<T>) -> T {
    let start = Instant::now();
    loop {
        if let Some(found) = poll() {
            return found;
        }
        assert!(start.elapsed() < Duration::from_secs(30), "{what}");
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[test]
fn the_bench_binary_serves_the_overview_and_its_mutators_change_it() {
    let bench = browsers_bench(None);
    let data = bench.data.clone();

    let mut child = Command::new(BIN)
        .arg("serve")
        .arg(&data)
        .args(["--port", "0"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("the server starts");
    let stdout = child.stdout.take().unwrap();
    let _serving = Serving(child);
    let mut lines = BufReader::new(stdout).lines();
    let link = lines
        .by_ref()
        .map(Result::unwrap)
        .find(|line| line.starts_with("http://"))
        .expect("the server prints a login link");
    let (address, code) = link
        .strip_prefix("http://127.0.0.1:")
        .and_then(|rest| rest.split_once("/#login="))
        .expect("a login link");
    let port: u16 = address.parse().unwrap();

    let login = |code: &str| {
        request(
            port,
            "POST",
            "/api/login",
            None,
            &json!({ "code": code }).to_string(),
        )
    };
    let (status, session) = login(code);
    assert_eq!(status, 200, "{session}");
    let session = format!(
        "{}.{}",
        session["id"].as_str().unwrap(),
        session["secret"].as_str().unwrap()
    );
    // A code older than a minute is refused, and so is the one used.
    let old = auth::mint_code(
        &token::read_token(&data).unwrap(),
        server::now_ms() - 120_000,
    )
    .unwrap();
    assert_eq!(login(&old).0, 401);
    assert_eq!(login(code).0, 401);

    // The seeded cards, the hostile title kept as text.
    let overview = |session: &str| request(port, "GET", "/api/overview", Some(session), "").1;
    let first = overview(&session);
    let cards: Vec<String> = first["projects"][0]["cards"]
        .as_array()
        .unwrap()
        .iter()
        .map(|card| card["ticket"].as_str().unwrap().to_owned())
        .collect();
    for ticket in ["DEMO-1", "DEMO-2", "DEMO-3", "DEMO-4", "DEMO-5", "DEMO-6"] {
        assert!(cards.contains(&ticket.to_owned()), "{ticket}: {cards:?}");
    }
    assert!(!cards.contains(&"DEMO-8".to_owned()));
    assert!(first.to_string().contains("<img src=x onerror=alert(1)>"));
    assert_eq!(
        first["projects"][0]["ready"]["stale"],
        json!(false),
        "{first}"
    );

    // `touch-ready` adds DEMO-8 to what the page shows.
    let touched = bin("touch-ready", &data);
    assert!(touched.status.success(), "{touched:?}");
    let added = until("DEMO-8 shows in the overview", || {
        let built = overview(&session);
        built
            .to_string()
            .contains("Added while you watch")
            .then_some(built)
    });
    assert!(added.to_string().contains("DEMO-8"));

    // `append-event` puts a park of DEMO-2 in what happened since.
    let since = |session: &str| request(port, "GET", "/api/since", Some(session), "").1;
    assert!(!since(&session).to_string().contains("DEMO-2"));
    let appended = bin("append-event", &data);
    assert!(appended.status.success(), "{appended:?}");
    let parked = until("the park shows in what happened since", || {
        let built = since(&session);
        built["parked"]
            .to_string()
            .contains("DEMO-2")
            .then_some(built)
    });
    assert!(!parked["parked"].to_string().contains("DEMO-4"), "{parked}");

    // `link` prints a login link.
    let again = bin("link", &data);
    let text = String::from_utf8(again.stdout).unwrap();
    assert!(text.contains("#login="), "{text}");
}
