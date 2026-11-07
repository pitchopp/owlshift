//! The web UI's server (OWL-467), each test on a listener bound to port 0, a
//! data directory of its own and a run secret of its own: the architecture's
//! "Who may drive it" as requests see it.

use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use jiff::Timestamp;
use owlshift_adapters::forge::Repo;
use owlshift_contracts::event::{Event, EventKind};
use owlshift_contracts::format::Format;
use owlshift_contracts::ids::TicketId;
use owlshift_contracts::ready::{Outcome, ReadySnapshot, ReadyTicket};
use owlshift_contracts::service::{BY_PAUSE, Request};
use owlshift_runner::daemon::requests;
use owlshift_runner::events::EventLog;
use owlshift_runner::project::ProjectDirs;
use owlshift_runner::served::Registry;
use owlshift_runner::ui::auth::{self, RunSecret, Session};
use owlshift_runner::ui::server::{
    self, APP_CSS, APP_JS, INDEX_HTML, LOGGED_OUT_LINE, OPS_JS, Options, SECURITY_HEADERS,
    SUPERSEDED_LINE, ServeError, Serving,
};
use owlshift_runner::ui::{personal_at, probe, record, token};
use serde_json::{Value, json};

/// A response as the wire carries it.
#[derive(Debug)]
struct Reply {
    status: u16,
    head: String,
    body: Vec<u8>,
}

impl Reply {
    fn header(&self, name: &str) -> Option<String> {
        self.head.lines().skip(1).find_map(|line| {
            let (field, value) = line.split_once(':')?;
            field
                .eq_ignore_ascii_case(name)
                .then(|| value.trim().to_owned())
        })
    }

    fn text(&self) -> String {
        String::from_utf8(self.body.clone()).unwrap()
    }

    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap()
    }

    fn assert_secured(&self) {
        assert_eq!(
            self.header("Content-Security-Policy").as_deref(),
            Some("default-src 'self'; frame-ancestors 'none'"),
            "{}",
            self.head
        );
        assert_eq!(self.header("X-Frame-Options").as_deref(), Some("DENY"));
        assert_eq!(
            self.header("X-Content-Type-Options").as_deref(),
            Some("nosniff")
        );
    }
}

/// The body of a chunked response, its chunks joined.
fn dechunk(mut body: &[u8]) -> Vec<u8> {
    let mut joined = Vec::new();
    loop {
        let Some(end) = body.windows(2).position(|pair| pair == b"\r\n") else {
            return joined;
        };
        let size =
            usize::from_str_radix(std::str::from_utf8(&body[..end]).unwrap().trim(), 16).unwrap();
        if size == 0 {
            return joined;
        }
        joined.extend_from_slice(&body[end + 2..end + 2 + size]);
        body = &body[end + 2 + size + 2..];
    }
}

/// Sends `raw` to `port` as it is and reads the answer to its end. Every
/// response of the suite goes through here, so none may carry a CORS header
/// or a cookie.
fn raw(port: u16, raw: &[u8]) -> Reply {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    stream.write_all(raw).unwrap();
    let mut answer = Vec::new();
    stream.read_to_end(&mut answer).unwrap();
    let split = answer
        .windows(4)
        .position(|four| four == b"\r\n\r\n")
        .expect("a response's head");
    let head = String::from_utf8(answer[..split].to_vec()).unwrap();
    let mut body = answer[split + 4..].to_vec();
    let status = head.split(' ').nth(1).unwrap().parse().unwrap();
    let lower = head.to_ascii_lowercase();
    assert!(!lower.contains("access-control-"), "{head}");
    assert!(!lower.contains("set-cookie"), "{head}");
    if lower.contains("transfer-encoding: chunked") {
        body = dechunk(&body);
    }
    Reply { status, head, body }
}

/// A request to `port` with `Host: 127.0.0.1:<port>` unless `headers` names
/// a `Host`, and `Connection: close`.
fn send(port: u16, method: &str, path: &str, headers: &[(&str, &str)], body: &str) -> Reply {
    raw(port, &request_bytes(port, method, path, headers, body))
}

fn request_bytes(
    port: u16,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> Vec<u8> {
    let mut text = format!("{method} {path} HTTP/1.1\r\n");
    if !headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("host"))
    {
        text.push_str(&format!("Host: 127.0.0.1:{port}\r\n"));
    }
    for (name, value) in headers {
        if !value.is_empty() {
            text.push_str(&format!("{name}: {value}\r\n"));
        }
    }
    if method == "POST" {
        text.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    text.push_str("Connection: close\r\n\r\n");
    text.push_str(body);
    text.into_bytes()
}

fn get(port: u16, path: &str, headers: &[(&str, &str)]) -> Reply {
    send(port, "GET", path, headers, "")
}

fn post_json(port: u16, path: &str, body: &Value) -> Reply {
    send(
        port,
        "POST",
        path,
        &[("Content-Type", "application/json")],
        &body.to_string(),
    )
}

fn options() -> Options {
    Options {
        refresh: Duration::from_millis(100),
        overview_refresh: Duration::from_millis(100),
        personal: Arc::new(|| {
            owlshift_runner::config::FileState::Absent(PathBuf::from("/nowhere/owlshift.toml"))
        }),
        ..Options::default()
    }
}

fn serve_with(data: &Path, run: RunSecret, options: Options) -> Serving {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    server::start(data, listener, run, None, options).unwrap()
}

fn serve(data: &Path, run: RunSecret) -> Serving {
    serve_with(data, run, options())
}

fn code(data: &Path) -> String {
    let token = token::read_token(data).unwrap();
    auth::mint_code(&token, server::now_ms()).unwrap()
}

fn login(port: u16, code: &str) -> Reply {
    post_json(port, "/api/login", &json!({ "code": code }))
}

fn session(port: u16, data: &Path) -> Session {
    let reply = login(port, &code(data));
    assert_eq!(reply.status, 200, "{}", reply.text());
    let value = reply.json();
    Session {
        id: value["id"].as_str().unwrap().to_owned(),
        secret: value["secret"].as_str().unwrap().to_owned(),
    }
}

fn header(port: u16, session: &Session) -> Reply {
    get(
        port,
        "/api/header",
        &[("X-Owlshift-Session", &session.header())],
    )
}

#[test]
fn the_frame_and_its_assets_answer_without_a_session() {
    let data = tempfile::tempdir().unwrap();
    let ui = serve(data.path(), RunSecret::new().unwrap());
    for (path, kind, body) in [
        ("/", "text/html", INDEX_HTML),
        ("/app.js", "text/javascript", APP_JS),
        ("/ops.js", "text/javascript", OPS_JS),
        ("/app.css", "text/css", APP_CSS),
    ] {
        let reply = get(ui.port(), path, &[]);
        assert_eq!(reply.status, 200, "{path}");
        reply.assert_secured();
        assert!(reply.header("Content-Type").unwrap().starts_with(kind));
        assert_eq!(reply.text(), body);
    }
    assert!(INDEX_HTML.contains(r#"<script src="/app.js" defer>"#));
    assert!(INDEX_HTML.contains(r#"<script src="/ops.js" defer>"#));
    assert!(INDEX_HTML.contains(r#"<link rel="icon" href="/favicon.svg" type="image/svg+xml">"#));
    let brand = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/brand/owlshift-mark.svg");
    let mut carried = vec![(
        "/favicon.svg".to_owned(),
        "image/svg+xml",
        std::fs::read(brand).unwrap(),
    )];
    for name in FONT_FILES {
        carried.push((
            format!("/fonts/{name}"),
            "font/woff2",
            std::fs::read(font_dir().join(name)).unwrap(),
        ));
    }
    for (path, kind, bytes) in carried {
        let reply = get(ui.port(), &path, &[]);
        assert_eq!(reply.status, 200, "{path}");
        reply.assert_secured();
        assert_eq!(
            reply.header("Content-Type").as_deref(),
            Some(kind),
            "{path}"
        );
        assert_eq!(reply.body, bytes, "{path}");
    }
    let missing = get(ui.port(), "/fonts/missing.woff2", &[]);
    assert_eq!(missing.status, 404);
    missing.assert_secured();
}

#[test]
fn starting_makes_the_token_and_writes_the_record() {
    let data = tempfile::tempdir().unwrap();
    let ui = serve(data.path(), RunSecret::new().unwrap());
    assert!(token::read_token_if_any(data.path()).unwrap().is_some());
    let record = record::read(data.path()).unwrap().unwrap();
    assert_eq!(record.port, ui.port());
    assert_eq!(record.pid, std::process::id());
}

#[test]
fn the_header_needs_a_session_from_a_code() {
    let data = tempfile::tempdir().unwrap();
    let ui = serve(data.path(), RunSecret::new().unwrap());
    let none = get(ui.port(), "/api/header", &[]);
    assert_eq!(none.status, 401);
    none.assert_secured();
    let made_up = Session {
        id: "00".repeat(16),
        secret: "11".repeat(32),
    };
    assert_eq!(header(ui.port(), &made_up).status, 401);
    assert_eq!(
        get(
            ui.port(),
            "/api/header",
            &[("X-Owlshift-Session", "nonsense")]
        )
        .status,
        401
    );
    let session = session(ui.port(), data.path());
    let reply = header(ui.port(), &session);
    assert_eq!(reply.status, 200, "{}", reply.text());
    reply.assert_secured();
    let value = reply.json();
    assert_eq!(value["daemon"]["state"], "not_running");
    for key in ["slots", "usage", "waiting", "projects", "notes"] {
        assert!(value.get(key).is_some(), "{key}: {value}");
    }
}

#[test]
fn a_code_is_taken_once() {
    let data = tempfile::tempdir().unwrap();
    let ui = serve(data.path(), RunSecret::new().unwrap());
    let code = code(data.path());
    assert_eq!(login(ui.port(), &code).status, 200);
    assert_eq!(login(ui.port(), &code).status, 401);
    assert_eq!(login(ui.port(), "made.up.code").status, 401);
}

#[test]
fn sessions_end_with_the_daemons_run_and_the_token() {
    let data = tempfile::tempdir().unwrap();
    let run = RunSecret::new().unwrap();
    let first = serve(data.path(), run.clone());
    let session = session(first.port(), data.path());
    assert_eq!(header(first.port(), &session).status, 200);
    drop(first);
    // A restart of the UI under the same daemon: the same run secret.
    let second = serve(data.path(), run);
    assert_eq!(header(second.port(), &session).status, 200);
    // A new daemon run: a new run secret.
    let third = serve(data.path(), RunSecret::new().unwrap());
    assert_eq!(header(third.port(), &session).status, 401);
    // A new token ends it under the same run as well.
    token::new_token(data.path()).unwrap();
    assert_eq!(header(second.port(), &session).status, 401);
}

#[test]
fn a_foreign_host_is_refused() {
    let data = tempfile::tempdir().unwrap();
    let ui = serve(data.path(), RunSecret::new().unwrap());
    for host in [
        "evil.example:18466",
        "127.0.0.1.evil.example",
        "127.0.0.1:x",
    ] {
        let reply = get(ui.port(), "/", &[("Host", host)]);
        assert_eq!(reply.status, 403, "{host}");
        reply.assert_secured();
    }
    let missing = raw(ui.port(), b"GET / HTTP/1.0\r\n\r\n");
    assert_eq!(missing.status, 403);
    let port = ui.port().to_string();
    for host in [
        format!("localhost:{port}"),
        format!("LOCALHOST:{port}"),
        format!("[::1]:{port}"),
        "127.0.0.1:18566".to_owned(),
    ] {
        assert_eq!(
            get(ui.port(), "/", &[("Host", &host)]).status,
            200,
            "{host}"
        );
    }
}

#[test]
fn a_post_from_another_origin_or_of_another_kind_is_refused() {
    let data = tempfile::tempdir().unwrap();
    let ui = serve(data.path(), RunSecret::new().unwrap());
    let body = json!({ "code": "made.up.code" }).to_string();
    let same = format!("http://127.0.0.1:{}", ui.port());
    for (origin, status) in [
        ("http://127.0.0.1:9999", 403),
        ("null", 403),
        (same.as_str(), 401),
        ("", 401),
    ] {
        let reply = send(
            ui.port(),
            "POST",
            "/api/login",
            &[("Content-Type", "application/json"), ("Origin", origin)],
            &body,
        );
        assert_eq!(reply.status, status, "{origin}");
    }
    let plain = send(
        ui.port(),
        "POST",
        "/api/login",
        &[("Content-Type", "text/plain")],
        &body,
    );
    assert_eq!(plain.status, 415);
    let long = send(
        ui.port(),
        "POST",
        "/api/login",
        &[("Content-Type", "application/json")],
        &"x".repeat(server::MAX_BODY + 1),
    );
    assert_eq!(long.status, 413);
}

#[test]
fn other_methods_and_paths_are_refused() {
    let data = tempfile::tempdir().unwrap();
    let ui = serve(data.path(), RunSecret::new().unwrap());
    let options = send(ui.port(), "OPTIONS", "/api/header", &[], "");
    assert_eq!(options.status, 405);
    options.assert_secured();
    assert_eq!(post_json(ui.port(), "/api/hello", &json!({})).status, 404);
    assert_eq!(get(ui.port(), "/api/hello", &[]).status, 404);
}

#[test]
fn the_proof_answers_a_challenge_under_the_token() {
    let data = tempfile::tempdir().unwrap();
    let ui = serve(data.path(), RunSecret::new().unwrap());
    let challenge = [7u8; auth::CHALLENGE_LEN];
    let reply = post_json(
        ui.port(),
        "/api/proof",
        &json!({ "challenge": auth::hex(&challenge) }),
    );
    assert_eq!(reply.status, 200);
    let token = token::read_token(data.path()).unwrap();
    assert!(auth::check_proof(
        &token,
        &challenge,
        reply.json()["proof"].as_str().unwrap()
    ));
    let bad = post_json(ui.port(), "/api/proof", &json!({ "challenge": "00" }));
    assert_eq!(bad.status, 400);
}

/// A relay that forwards each connection to `port` and records every byte a
/// client sends through it.
fn relay(port: u16) -> (u16, Arc<Mutex<Vec<u8>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let relay_port = listener.local_addr().unwrap().port();
    let recorded = Arc::new(Mutex::new(Vec::new()));
    let recording = recorded.clone();
    thread::spawn(move || {
        for client in listener.incoming() {
            let Ok(mut client) = client else { return };
            let mut upstream = TcpStream::connect(("127.0.0.1", port)).unwrap();
            let mut back_to = client.try_clone().unwrap();
            let mut from = upstream.try_clone().unwrap();
            thread::spawn(move || {
                let _ = std::io::copy(&mut from, &mut back_to);
                let _ = back_to.shutdown(Shutdown::Write);
            });
            let recording = recording.clone();
            thread::spawn(move || {
                let mut buffer = [0u8; 4096];
                loop {
                    match client.read(&mut buffer) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            recording.lock().unwrap().extend_from_slice(&buffer[..n]);
                            if upstream.write_all(&buffer[..n]).is_err() {
                                break;
                            }
                        }
                    }
                }
                let _ = upstream.shutdown(Shutdown::Write);
            });
        }
    });
    (relay_port, recorded)
}

/// Opens `/api/stream` on `port` and reads its head: the stream, positioned
/// at its first chunk.
fn open_stream(port: u16, session: &Session) -> BufReader<TcpStream> {
    open_stream_on(port, session, "/api/stream")
}

/// [`open_stream`] on `path`, a query included.
fn open_stream_on(port: u16, session: &Session, path: &str) -> BufReader<TcpStream> {
    let stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    (&stream)
        .write_all(&request_bytes(
            port,
            "GET",
            path,
            &[("X-Owlshift-Session", &session.header())],
            "",
        ))
        .unwrap();
    let mut reader = BufReader::new(stream);
    let mut head = String::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        if line == "\r\n" || line.is_empty() {
            break;
        }
        head.push_str(&line);
    }
    assert!(head.starts_with("HTTP/1.1 200"), "{head}");
    let lower = head.to_ascii_lowercase();
    assert!(
        lower.contains("content-type: application/x-ndjson"),
        "{head}"
    );
    assert!(
        lower.contains("content-security-policy: default-src 'self'; frame-ancestors 'none'"),
        "{head}"
    );
    assert!(lower.contains("x-content-type-options: nosniff"), "{head}");
    assert!(!lower.contains("access-control-"), "{head}");
    assert!(!lower.contains("set-cookie"), "{head}");
    reader
}

/// The next line of a stream, `None` at its end.
fn next_line(reader: &mut BufReader<TcpStream>) -> Option<String> {
    let mut size = String::new();
    reader.read_line(&mut size).ok()?;
    let size = usize::from_str_radix(size.trim(), 16).ok()?;
    if size == 0 {
        return None;
    }
    let mut chunk = vec![0u8; size + 2];
    reader.read_exact(&mut chunk).ok()?;
    let line = String::from_utf8(chunk[..size].to_vec()).unwrap();
    Some(line.trim_end_matches('\n').to_owned())
}

#[test]
fn the_token_never_appears_in_a_request() {
    let data = tempfile::tempdir().unwrap();
    let run = RunSecret::new().unwrap();
    let ui = serve(data.path(), run.clone());
    let (port, recorded) = relay(ui.port());
    let session = session(port, data.path());
    assert_eq!(header(port, &session).status, 200);
    let mut stream = open_stream(port, &session);
    assert!(next_line(&mut stream).unwrap().starts_with(r#"{"header":"#));
    drop(stream);
    // The listener check of `owlshift ui` and `owlshift doctor`.
    assert!(matches!(
        probe::probe(data.path(), port),
        probe::Probe::Proved(Some(_))
    ));
    let token = token::read_token(data.path()).unwrap();
    let recorded = recorded.lock().unwrap().clone();
    let text = String::from_utf8_lossy(&recorded);
    for path in ["/api/proof", "/api/login", "/api/header", "/api/stream"] {
        assert!(text.contains(path), "{path}");
    }
    assert!(!text.contains(&token.to_hex()));
    assert!(
        !recorded
            .windows(token.as_bytes().len())
            .any(|window| window == token.as_bytes())
    );
    assert!(!text.contains(&run.to_hex()));
}

#[test]
fn no_asset_holds_the_token_and_the_page_sends_only_the_code_and_the_session() {
    let data = tempfile::tempdir().unwrap();
    let ui = serve(data.path(), RunSecret::new().unwrap());
    let token = token::read_token(data.path()).unwrap().to_hex();
    for path in ["/", "/app.js", "/ops.js", "/app.css"] {
        assert!(!get(ui.port(), path, &[]).text().contains(&token), "{path}");
    }
    assert!(!APP_JS.to_ascii_lowercase().contains("token"));
    assert!(!OPS_JS.to_ascii_lowercase().contains("token"));
    // The operations screens (OWL-472) fetch through one function that sends
    // the session, and nothing else.
    let ops_fetches: Vec<&str> = OPS_JS
        .match_indices("fetch(")
        .map(|(at, _)| OPS_JS[at..].split(',').next().unwrap())
        .collect();
    assert_eq!(ops_fetches, ["fetch(path"], "{ops_fetches:?}");
    assert!(OPS_JS.contains("headers[SESSION_HEADER] = sent;"));
    assert!(OPS_JS.contains("const sent = sessionHeader(own);"));
    // The page's requests: the login, with the code, and the session's
    // routes through `authedFetch`, with the session's header.
    let fetches: Vec<&str> = APP_JS
        .match_indices("fetch(")
        .map(|(at, _)| APP_JS[at..].split(',').next().unwrap())
        .collect();
    assert_eq!(
        fetches,
        ["fetch(\"/api/login\"", "fetch(path"],
        "{fetches:?}"
    );
    let authed: Vec<&str> = APP_JS
        .match_indices("authedFetch(")
        .map(|(at, _)| APP_JS[at..].split(')').next().unwrap())
        .collect();
    // The header's stream, which carries the graph too (OWL-460), and the
    // ticket page's snapshot and run logs (OWL-471): each a route of the
    // session, none a path the client chooses beyond the ticket's own.
    assert_eq!(
        authed,
        [
            "authedFetch(path, signal",
            "authedFetch(streamPath(mine.graph",
            "authedFetch(snapshotPath(handle",
            "authedFetch(logPath(handle, run.id, which, log.from, log.line",
            // The overview's "since" panel (OWL-470).
            "authedFetch(path",
        ],
        "{authed:?}"
    );
    assert!(APP_JS.contains("body: JSON.stringify({ code: match[1] })"));
    assert!(APP_JS.contains("return current.id + \".\" + current.secret;"));
    assert!(APP_JS.contains("const sent = sessionHeader(current);"));
    assert!(APP_JS.contains("headers[SESSION_HEADER] = sent;"));
    assert_eq!(APP_JS.matches("body:").count(), 1);
}

/// OWL-472: the Orchestrator sends back the generation and the cursors it was
/// given, Spend and Config the `rev` of the answer they hold, and each refresh
/// is one timer that stops when the screen is left.
#[test]
fn the_operations_screens_send_back_their_cursors_and_revs_and_stop_their_timer() {
    for sent in [
        r#"query.push("gen=" + state.gen);"#,
        r#"query.push("after=" + state.after);"#,
        r#"query.push("log=" + state.log);"#,
        r#"query.push("rev=" + encodeURIComponent(state.rev));"#,
        r#"? "?rev=" + encodeURIComponent(state.rev)"#,
    ] {
        assert!(OPS_JS.contains(sent), "ops.js does not send {sent}");
    }
    // The answer's cursors are the ones kept for the next request.
    assert!(OPS_JS.contains("state.gen = events.generation;"));
    assert!(OPS_JS.contains("state.after = events.cursor;"));
    assert!(OPS_JS.contains("state.log = log.next;"));
    assert!(OPS_JS.contains("state.rev = answer.rev;"));
    // Nothing is redrawn for an answer that says `unchanged`.
    assert_eq!(OPS_JS.matches("!answer.value.unchanged").count(), 2);
    // The timer: set after a tick, cleared when the screen is left.
    let ops = lf(OPS_JS);
    let hide = block(&ops, "  function hide() {", "  ");
    assert!(hide.contains("window.clearTimeout(timer);"), "{hide}");
    assert!(OPS_JS.contains("ORCHESTRATOR_MS = 2000"));
    assert!(OPS_JS.contains("SLOW_MS = 10000"));
    // The app routes the three addresses to it, and drops the screen when
    // the session ends.
    for address in ["#/orchestrator", "#/spend", "#/config"] {
        assert!(APP_JS.contains(address), "{address}");
    }
    let app = lf(APP_JS);
    let logged_out = block(&app, "function loggedOut(sent) {", "");
    assert!(logged_out.contains("window.owlshiftOps.hide();"));
}

#[test]
fn markup_in_the_header_comes_back_as_escaped_json_text() {
    let data = tempfile::tempdir().unwrap();
    let why = "<script>alert(1)</script><b>x</b> & \u{2028}";
    requests::ask_pause(
        data.path(),
        None,
        &Request::new(BY_PAUSE, Timestamp::now(), why),
    )
    .unwrap();
    let ui = serve(data.path(), RunSecret::new().unwrap());
    let session = session(ui.port(), data.path());
    let reply = header(ui.port(), &session);
    assert_eq!(reply.status, 200);
    reply.assert_secured();
    assert!(
        reply
            .header("Content-Type")
            .unwrap()
            .starts_with("application/json")
    );
    let text = reply.text();
    for c in ['<', '>', '&', '\u{2028}'] {
        assert!(!text.contains(c), "{text}");
    }
    assert_eq!(reply.json()["machine_pause"]["why"], why);
    let mut stream = open_stream(ui.port(), &session);
    let first = next_line(&mut stream).unwrap();
    for c in ['<', '>', '&', '\u{2028}'] {
        assert!(!first.contains(c), "{first}");
    }
    let line: Value = serde_json::from_str(&first).unwrap();
    assert_eq!(line["header"]["machine_pause"]["why"], why);
}

#[test]
fn the_page_sets_every_text_as_text() {
    for banned in [
        "innerHTML",
        "outerHTML",
        "insertAdjacentHTML",
        "document.write",
        "eval(",
        "new Function",
        "javascript:",
    ] {
        assert!(!APP_JS.contains(banned), "app.js holds {banned}");
        assert!(!OPS_JS.contains(banned), "ops.js holds {banned}");
    }
    assert!(APP_JS.contains("el.textContent ="));
    // The operations screens set text through app.js's `text` and `make`,
    // never their own markup, and no inline handler.
    for banned in [
        ".onclick",
        ".onchange",
        ".oninput",
        "setAttribute(\"on",
        "srcdoc",
    ] {
        assert!(!OPS_JS.contains(banned), "ops.js holds {banned}");
    }
    let html = INDEX_HTML.to_ascii_lowercase();
    for (at, _) in html.match_indices("<script") {
        let tag = &html[at..at + html[at..].find('>').unwrap()];
        assert!(tag.contains(" src="), "an inline script: {tag}");
        assert!(html[at..].starts_with(&format!("{tag}></script>")), "{tag}");
    }
    assert!(!html.contains("<style"));
    assert!(!html.contains("style="));
    let mut rest = html.as_str();
    while let Some(at) = rest.find(" on") {
        let attribute: String = rest[at + 1..]
            .chars()
            .take_while(char::is_ascii_alphabetic)
            .collect();
        assert!(
            !rest[at + 1 + attribute.len()..].starts_with('='),
            "an event attribute: {attribute}"
        );
        rest = &rest[at + 3..];
    }
}

/// `asset` with LF line ends: a checkout on Windows may give the assets CRLF
/// ones, which the tests that match across lines do not mean.
fn lf(asset: &str) -> String {
    asset.replace("\r\n", "\n")
}

/// The text of the block of `css` that opens with `opening`, up to its
/// closing brace at `indent`.
fn block<'a>(css: &'a str, opening: &str, indent: &str) -> &'a str {
    let start = css.find(opening).unwrap_or_else(|| panic!("{opening}")) + opening.len();
    let end = css[start..].find(&format!("\n{indent}}}")).unwrap();
    &css[start..start + end]
}

#[test]
fn the_style_sheet_holds_each_theme_role_light_and_dark() {
    // Visual identity section 4, "Themes" and "States".
    let roles = [
        ("page", "#FFF8F2", "#1C0E25"),
        ("surface", "#FFFFFF", "#2B1638"),
        ("raised", "#FFF1E6", "#3D2150"),
        ("text", "#2B1638", "#FFF1E6"),
        ("text-secondary", "#5A3A68", "#C9B3D6"),
        ("text-muted", "#725480", "#A692AE"),
        ("border-hairline", "#EBDDE4", "#4A2D5C"),
        ("border-strong", "#8E7899", "#8E7899"),
        ("action-fill", "#2B1638", "#FF8E5E"),
        ("action-text", "#FFF1E6", "#2B1638"),
        ("link", "#A83E17", "#FFB38F"),
        ("focus", "#D9572A", "#FF8E5E"),
        ("badge-fill", "#FFDCCB", "#5A2A1E"),
        ("badge-text", "#7A2E10", "#FFD2BD"),
        ("success", "#1E7A52", "#7FE0B0"),
        ("warning", "#8A5A00", "#FFD166"),
        ("danger", "#B8283F", "#FF6B81"),
        ("information", "#5A3A68", "#C9B3D6"),
    ];
    let light = block(APP_CSS, ":root {", "");
    let system_dark = block(APP_CSS, ":root:not([data-theme=light]) {", "  ");
    let chosen_dark = block(APP_CSS, ":root[data-theme=dark] {", "");
    for (role, on_light, on_dark) in roles {
        assert!(light.contains(&format!("--{role}: {on_light};")), "{role}");
        assert!(
            system_dark.contains(&format!("--{role}: {on_dark};")),
            "{role}"
        );
        assert!(
            chosen_dark.contains(&format!("--{role}: {on_dark};")),
            "{role}"
        );
    }
    assert!(APP_CSS.contains("@media (prefers-color-scheme: dark)"));
}

#[test]
fn each_state_shows_its_symbol_beside_its_colour() {
    // Visual identity section 4, "States": ✓ ! ✗ ·.
    let app_css = lf(APP_CSS);
    for (state, symbol) in [
        ("success", r#""\2713""#),
        ("warning", r#""!""#),
        ("danger", r#""\2717""#),
        ("information", r#""\00B7""#),
    ] {
        // Its own rule, a line of its own after a blank line, not the
        // grouped one that sets the symbols' spacing.
        let rule = block(&app_css, &format!("\n\n.state-{state}::before {{"), "");
        assert!(
            rule.contains(&format!("content: {symbol};")),
            "{state}: {rule}"
        );
        let colour = block(APP_CSS, &format!(".state-{state} {{"), "");
        assert!(
            colour.contains(&format!("color: var(--{state});")),
            "{state}"
        );
    }
}

#[test]
fn a_hidden_element_stays_hidden_and_a_logout_empties_the_header() {
    // `.bar-row` sets `display: flex`, which beats the browser's own
    // `[hidden]` rule: the sheet's rule must win over any class.
    assert!(
        block(APP_CSS, "[hidden] {", "").contains("display: none !important;"),
        "no [hidden] rule"
    );
    let logged_out = block(APP_JS, "function loggedOut(sent) {", "");
    assert!(
        logged_out.contains(r#"clear(document.getElementById("header"));"#),
        "{logged_out}"
    );
    // A stream the session no longer holds logs the page out.
    assert!(APP_JS.contains(r#"message.closed === "logged_out""#));
}

#[test]
fn a_logout_removes_the_stored_session_only_while_it_is_the_refused_one() {
    // Every tab of the origin shares the storage: an old tab told its
    // session ended must not remove the one a newer tab stored since.
    let app_js = lf(APP_JS);
    let logged_out = block(&app_js, "function loggedOut(sent) {", "");
    let guarded =
        "  if (current && sessionHeader(current) === sent) {\n    store(SESSION_KEY, null);\n  }";
    assert!(logged_out.contains(guarded), "{logged_out}");
    assert_eq!(logged_out.matches("store(SESSION_KEY, null)").count(), 1);
    // Each call names the session its request sent, or none when it sent
    // none, which removes nothing.
    let calls: Vec<&str> = APP_JS
        .match_indices("loggedOut(")
        .map(|(at, _)| APP_JS[at..].split(')').next().unwrap())
        .collect();
    assert_eq!(
        calls,
        [
            "loggedOut(sent",
            "loggedOut(null",
            "loggedOut(sent",
            "loggedOut(sent",
            "loggedOut(null"
        ],
        "{calls:?}"
    );
    let follow = block(APP_JS, "async function follow() {", "");
    assert!(follow.contains("sent = answer.sent;"), "{follow}");
}

#[test]
fn a_stream_starts_at_once_and_the_seventeenth_supersedes_the_first() {
    let data = tempfile::tempdir().unwrap();
    let ui = serve(data.path(), RunSecret::new().unwrap());
    let session = session(ui.port(), data.path());
    let started = Instant::now();
    let mut first = open_stream(ui.port(), &session);
    let line = next_line(&mut first).unwrap();
    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(line.starts_with(r#"{"header":"#), "{line}");
    let mut others: Vec<_> = (0..15)
        .map(|_| {
            let mut stream = open_stream(ui.port(), &session);
            next_line(&mut stream).unwrap();
            stream
        })
        .collect();
    assert_eq!(ui.streams(), 16);
    let mut seventeenth = open_stream(ui.port(), &session);
    next_line(&mut seventeenth).unwrap();
    others.push(seventeenth);
    first
        .get_ref()
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut last = None;
    while let Some(line) = next_line(&mut first) {
        last = Some(line);
    }
    assert_eq!(last.as_deref(), Some(SUPERSEDED_LINE));
    assert_eq!(ui.streams(), 16);
    // The others stay open: none of them ended.
    for stream in &mut others {
        stream
            .get_ref()
            .set_read_timeout(Some(Duration::from_millis(50)))
            .unwrap();
        let mut byte = [0u8; 1];
        match stream.get_mut().peek(&mut byte) {
            Err(error) => assert!(
                matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut),
                "{error}"
            ),
            Ok(n) => assert!(n > 0, "a stream ended"),
        }
    }
}

#[test]
fn a_stream_writes_the_header_again_when_it_changes() {
    let data = tempfile::tempdir().unwrap();
    let ui = serve(data.path(), RunSecret::new().unwrap());
    let session = session(ui.port(), data.path());
    let mut stream = open_stream(ui.port(), &session);
    let first: Value = serde_json::from_str(&next_line(&mut stream).unwrap()).unwrap();
    assert_eq!(first["header"]["machine_pause"], Value::Null);
    requests::ask_pause(
        data.path(),
        None,
        &Request::new(BY_PAUSE, Timestamp::now(), "lunch"),
    )
    .unwrap();
    // The overview line the stream writes after the header is skipped.
    let next = next_with(&mut stream, "header");
    assert_eq!(next["header"]["machine_pause"]["why"], "lunch");
}

#[test]
fn a_new_token_ends_a_stream_already_open() {
    let data = tempfile::tempdir().unwrap();
    let ui = serve(data.path(), RunSecret::new().unwrap());
    let session = session(ui.port(), data.path());
    let mut stream = open_stream(ui.port(), &session);
    assert!(next_line(&mut stream).unwrap().starts_with(r#"{"header":"#));
    token::new_token(data.path()).unwrap();
    let replaced = Instant::now();
    stream
        .get_ref()
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut last = None;
    while let Some(line) = next_line(&mut stream) {
        last = Some(line);
    }
    // The refresh period of these tests is 100 ms: the stream checks its
    // session at least that often.
    assert!(replaced.elapsed() < Duration::from_secs(2));
    assert_eq!(last.as_deref(), Some(LOGGED_OUT_LINE));
    let deadline = Instant::now() + Duration::from_secs(2);
    while ui.streams() > 0 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(ui.streams(), 0);
}

#[test]
fn the_header_reads_the_personal_file_again_and_keeps_its_last_good_figures() {
    let data = tempfile::tempdir().unwrap();
    let personal = data.path().join("owlshift.toml");
    std::fs::write(&personal, "concurrent_runs = 2\n").unwrap();
    let path = personal.clone();
    let ui = serve_with(
        data.path(),
        RunSecret::new().unwrap(),
        Options {
            personal: Arc::new(move || personal_at(&path)),
            ..options()
        },
    );
    let session = session(ui.port(), data.path());
    assert_eq!(header(ui.port(), &session).json()["slots"]["of"], 2);
    std::fs::write(&personal, "concurrent_runs = 3\n").unwrap();
    assert_eq!(header(ui.port(), &session).json()["slots"]["of"], 3);
    std::fs::write(&personal, "concurrent_runs = \"many\"\n").unwrap();
    let value = header(ui.port(), &session).json();
    assert_eq!(value["slots"]["of"], 3);
    let notes = value["notes"].to_string();
    assert!(notes.contains("does not load"), "{notes}");
}

#[test]
fn a_listener_off_the_loopback_is_refused() {
    let data = tempfile::tempdir().unwrap();
    let Ok(listener) = TcpListener::bind("0.0.0.0:0") else {
        return;
    };
    match server::start(
        data.path(),
        listener,
        RunSecret::new().unwrap(),
        None,
        options(),
    ) {
        Err(ServeError::NotLoopback(address)) => assert!(address.ip().is_unspecified()),
        other => panic!("{other:?}"),
    }
    assert!(record::read(data.path()).unwrap().is_none());
}

/// The three typefaces the UI carries, by file name.
const FONT_FILES: [&str; 3] = [
    "BricolageGrotesque-latin.woff2",
    "JetBrainsMono-Regular-latin.woff2",
    "JetBrainsMono-Medium-latin.woff2",
];

/// The most bytes one carried font may take.
const FONT_CAP: usize = 120 * 1024;

fn font_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src/ui/assets/fonts")
}

/// The colour variables of a theme block: `--name: #RRGGBB;` lines.
fn colours(block: &str) -> std::collections::BTreeMap<String, [f64; 3]> {
    let mut found = std::collections::BTreeMap::new();
    for line in block.lines() {
        let Some((name, value)) = line.trim().strip_prefix("--").and_then(|rest| {
            let (name, value) = rest.split_once(':')?;
            Some((name.trim(), value.trim().strip_suffix(';')?.trim()))
        }) else {
            continue;
        };
        let Some(hex) = value.strip_prefix('#').filter(|hex| hex.len() == 6) else {
            continue;
        };
        let channel = |at: usize| f64::from(u8::from_str_radix(&hex[at..at + 2], 16).unwrap());
        found.insert(name.to_owned(), [channel(0), channel(2), channel(4)]);
    }
    found
}

/// The WCAG 2.2 contrast ratio of two colours.
fn contrast(a: [f64; 3], b: [f64; 3]) -> f64 {
    let luminance = |rgb: [f64; 3]| {
        let [r, g, b] = rgb.map(|value| {
            let value = value / 255.0;
            if value <= 0.04045 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        });
        0.2126 * r + 0.7152 * g + 0.0722 * b
    };
    let (high, low) = (
        luminance(a).max(luminance(b)),
        luminance(a).min(luminance(b)),
    );
    (high + 0.05) / (low + 0.05)
}

#[test]
fn every_text_pair_of_the_contrast_table_holds_in_both_themes() {
    // Visual identity section 4, "Contrast": page · surface · raised, light
    // then dark. The table lists two decimals, hence the tolerance.
    let text_rows: [(&str, [f64; 3], [f64; 3]); 7] = [
        ("text", [15.66, 16.48, 14.89], [16.66, 14.89, 12.39]),
        ("text-secondary", [8.91, 9.38, 8.47], [9.57, 8.55, 7.12]),
        ("text-muted", [6.06, 6.37, 5.76], [6.45, 5.76, 4.79]),
        ("link", [5.93, 6.24, 5.64], [10.62, 9.49, 7.90]),
        ("success", [5.04, 5.30, 4.79], [11.58, 10.35, 8.61]),
        ("warning", [5.63, 5.93, 5.36], [12.79, 11.43, 9.51]),
        ("danger", [5.85, 6.15, 5.56], [6.74, 6.02, 5.01]),
    ];
    let graphic_rows: [(&str, [f64; 3], [f64; 3]); 2] = [
        ("border-strong", [3.77, 3.96, 3.58], [4.65, 4.16, 3.46]),
        // The ring's table lists it on page and surface only; on raised it
        // is held to the 3:1 floor below.
        ("focus", [3.73, 3.93, 3.0], [8.15, 7.28, 3.0]),
    ];
    let light = colours(block(APP_CSS, ":root {", ""));
    let system_dark = colours(block(APP_CSS, ":root:not([data-theme=light]) {", "  "));
    let chosen_dark = colours(block(APP_CSS, ":root[data-theme=dark] {", ""));
    for role in system_dark.keys() {
        assert_eq!(system_dark[role], chosen_dark[role], "{role}");
    }
    let backgrounds = ["page", "surface", "raised"];
    let mut checked = 0;
    for (theme, vars, column) in [
        ("light", &light, 0),
        ("dark", &system_dark, 1),
        ("dark", &chosen_dark, 1),
    ] {
        let at = |role: &str| *vars.get(role).unwrap_or_else(|| panic!("{theme} {role}"));
        let pick = |row: &[f64; 3], index: usize| row[index];
        for (role, on_light, on_dark) in text_rows {
            let listed = if column == 0 { on_light } else { on_dark };
            for (index, background) in backgrounds.iter().enumerate() {
                let ratio = contrast(at(role), at(background));
                assert!(
                    (ratio - pick(&listed, index)).abs() <= 0.005 && ratio >= 4.5,
                    "{theme} {role} on {background}: {ratio:.3}"
                );
                checked += 1;
            }
        }
        for (role, on_light, on_dark) in graphic_rows {
            let listed = if column == 0 { on_light } else { on_dark };
            for (index, background) in backgrounds.iter().enumerate() {
                let ratio = contrast(at(role), at(background));
                assert!(ratio >= 3.0, "{theme} {role} on {background}: {ratio:.3}");
                if pick(&listed, index) > 3.0 {
                    assert!(
                        (ratio - pick(&listed, index)).abs() <= 0.005,
                        "{theme} {role} on {background}: {ratio:.3}"
                    );
                }
                checked += 1;
            }
        }
        let action = contrast(at("action-text"), at("action-fill"));
        let listed = if column == 0 { 14.89 } else { 7.28 };
        assert!(
            (action - listed).abs() <= 0.005,
            "{theme} action: {action:.3}"
        );
        let badge = contrast(at("badge-text"), at("badge-fill"));
        let listed = if column == 0 { 7.35 } else { 8.52 };
        assert!((badge - listed).abs() <= 0.005, "{theme} badge: {badge:.3}");
        // Section 4 has no row for the information state: it shares the
        // secondary text's colour, so its row is that one.
        assert_eq!(at("information"), at("text-secondary"), "{theme}");
    }
    assert_eq!(checked, 3 * (7 + 2) * 3);
}

/// The rules of `css` outside the theme blocks and the font faces, as
/// (selector, declarations), the at-rule `@media` looked into.
fn style_rules(css: &str) -> Vec<(String, Vec<(String, String)>)> {
    // Comments carry no rule.
    let mut plain = String::new();
    let mut rest = css;
    while let Some(start) = rest.find("/*") {
        plain.push_str(&rest[..start]);
        rest = &rest[start + 2..];
        rest = &rest[rest.find("*/").expect("a closed comment") + 2..];
    }
    plain.push_str(rest);
    let mut rules = Vec::new();
    let mut queue = vec![plain];
    while let Some(text) = queue.pop() {
        let mut rest = text.trim();
        while !rest.is_empty() {
            let open = rest.find('{').expect("a rule without a block");
            let selector = rest[..open].trim().to_owned();
            let mut depth = 0;
            let mut close = None;
            for (at, c) in rest[open..].char_indices() {
                match c {
                    '{' => depth += 1,
                    '}' => {
                        depth -= 1;
                        if depth == 0 {
                            close = Some(open + at);
                            break;
                        }
                    }
                    _ => {}
                }
            }
            let close = close.expect("a closed block");
            let inner = &rest[open + 1..close];
            rest = rest[close + 1..].trim();
            if selector.starts_with("@media") {
                queue.push(inner.to_owned());
            } else if selector.starts_with("@") {
                assert_eq!(selector, "@font-face", "an at-rule the walk does not know");
            } else if !selector.starts_with(":root") {
                assert!(
                    !inner.contains('{'),
                    "nested rules are not walked: {selector}"
                );
                let declarations = inner
                    .split(';')
                    .filter_map(|declaration| {
                        let (name, value) = declaration.split_once(':')?;
                        Some((name.trim().to_owned(), value.trim().to_owned()))
                    })
                    .collect();
                rules.push((selector, declarations));
            }
        }
    }
    rules
}

/// The role names a rule's declaration uses, `var(--role)`.
fn roles(value: &str) -> Vec<&str> {
    value
        .split("var(--")
        .skip(1)
        .filter_map(|part| part.split(')').next())
        .collect()
}

/// What a rule outside the themes may colour with: text in a role the
/// contrast table lists on page, surface and raised, a fill the table pairs
/// with its text, and a border or outline of the strong or focus role
/// (3:1), the hairline exempt. Any other colour is an unchecked pair.
fn audit(css: &str) -> Result<(), String> {
    const TEXT: [&str; 7] = [
        "text",
        "text-secondary",
        "text-muted",
        "link",
        "success",
        "warning",
        "danger",
    ];
    // `information` shares the secondary text's colour, which the contrast
    // test asserts.
    const TEXT_LIKE: [&str; 1] = ["information"];
    const FILLS: [&str; 3] = ["page", "surface", "raised"];
    for (selector, declarations) in style_rules(css) {
        let value_of = |wanted: &str| {
            declarations
                .iter()
                .find(|(name, _)| name == wanted)
                .map(|(_, value)| value.as_str())
        };
        // A fill the table pairs with its text carries that text and no other,
        // also when the rule sets no `color` (the inherited text is unchecked).
        for (fill, text) in [("badge-fill", "badge-text"), ("action-fill", "action-text")] {
            let on_fill = declarations.iter().any(|(name, value)| {
                name.starts_with("background") && roles(value).contains(&fill)
            });
            if on_fill && value_of("color") != Some(&format!("var(--{text})")) {
                return Err(format!(
                    "{selector}: --{fill} without --{text} as its color"
                ));
            }
        }
        for (name, value) in &declarations {
            let coloured = name == "color"
                || name.starts_with("background")
                || (name.starts_with("border") && name != "border-radius")
                || name.starts_with("outline") && !name.ends_with("offset");
            // Any other property that carries a colour has no pair in the
            // table, whatever its name.
            let holds_colour = value.contains("var(--")
                || value.contains('#')
                || value.contains("rgb")
                || value.contains("hsl");
            if !coloured && holds_colour {
                return Err(format!("{selector}: {name} colours outside the table"));
            }
            if !coloured {
                continue;
            }
            if value.contains('#') || value.contains("rgb") || value.contains("hsl") {
                return Err(format!(
                    "{selector}: {name} sets a colour outside the themes"
                ));
            }
            for role in roles(value) {
                let fine = if name == "color" {
                    TEXT.contains(&role)
                        || TEXT_LIKE.contains(&role)
                        || (role == "badge-text"
                            && value_of("background") == Some("var(--badge-fill)"))
                        || (role == "action-text"
                            && value_of("background") == Some("var(--action-fill)"))
                } else if name.starts_with("background") {
                    FILLS.contains(&role) || role == "badge-fill" || role == "action-fill"
                } else {
                    // The hairline only separates areas that differ otherwise.
                    role == "border-hairline" || role == "border-strong" || role == "focus"
                };
                if !fine {
                    return Err(format!(
                        "{selector}: {name} uses --{role}, a pair the contrast table does not list"
                    ));
                }
            }
        }
    }
    Ok(())
}

#[test]
fn every_colour_the_style_sheet_uses_is_a_pair_of_the_contrast_table() {
    audit(APP_CSS).unwrap();
    // A rule added later cannot slip an unchecked pair in.
    for css in [
        ".new { color: var(--border-strong); }",
        ".new { color: var(--badge-text); }",
        ".new { color: var(--action-text); background: var(--surface); }",
        ".new { background: var(--text); }",
        ".new { border: 1px solid var(--page); }",
        ".new { color: #FFFFFF; }",
        ".new { border-color: rgb(0 0 0); }",
        ".bar { outline: 2px solid var(--link); }",
        ".new { background: var(--action-fill); }",
        ".new { color: var(--text-muted); background: var(--badge-fill); }",
        ".new { color: var(--text); background: var(--action-fill); }",
        ".new { box-shadow: 0 0 0 2px var(--border-hairline); }",
        ".new { column-rule-color: var(--border-hairline); }",
        ".new { text-decoration: underline var(--text-muted); }",
        ".new { -webkit-text-stroke: 1px var(--text); }",
        ".new { text-emphasis-color: #000; }",
    ] {
        assert!(audit(css).is_err(), "{css}");
    }
    for css in [
        ".new { color: var(--text-muted); }",
        ".new { color: var(--badge-text); background: var(--badge-fill); }",
        ".new { border-bottom: 1px solid var(--border-hairline); }",
        ".new { border: 1px solid var(--border-strong); border-radius: 4px; }",
        ".new { outline: 2px solid var(--focus); outline-offset: 2px; }",
    ] {
        assert!(audit(css).is_ok(), "{css}");
    }
}

/// Why `text` names a place off the origin, if it does: an absolute or
/// protocol-relative URL, an `@import`, or a `url(` that is not a carried
/// font. A `//` starts a URL only right after a quote, a bracket or `=`;
/// a `// comment` does not.
fn egress(text: &str) -> Option<String> {
    for scheme in ["http://", "https://", "ws://", "wss://", "ftp://", "data:"] {
        if text.contains(scheme) {
            return Some(scheme.to_owned());
        }
    }
    if text.contains("@import") {
        return Some("@import".to_owned());
    }
    for opener in ["\"//", "'//", "`//", "(//", "=//", "url(//", "src=//"] {
        if text.contains(opener) {
            return Some(opener.to_owned());
        }
    }
    for part in text.split("url(").skip(1) {
        if !part.starts_with("/fonts/") {
            return Some(format!("url({}", part.chars().take(20).collect::<String>()));
        }
    }
    None
}

#[test]
fn no_asset_names_a_place_off_the_origin_and_the_policy_allows_none() {
    // The one address the page names: the prefix of a pull request's link
    // (OWL-470), which a click navigates to. The page fetches nothing from it,
    // and the policy allows no fetch.
    const PULL_REQUEST_LINK: &str = "https://github.com/";
    assert_eq!(APP_JS.matches(PULL_REQUEST_LINK).count(), 1);
    let script = APP_JS.replace(PULL_REQUEST_LINK, "");
    for (name, text) in [
        ("index.html", INDEX_HTML),
        ("app.js", script.as_str()),
        ("ops.js", OPS_JS),
        ("app.css", APP_CSS),
    ] {
        assert_eq!(egress(text), None, "{name}");
    }
    // The mark is served too; its XML namespace is the one URL it may hold.
    let mark = owlshift_runner::ui::server::FAVICON_SVG
        .replace("xmlns=\"http://www.w3.org/2000/svg\"", "");
    assert_eq!(egress(&mark), None, "favicon.svg");
    // The check tells a URL from a comment.
    assert!(egress(r#"fetch("//example.com/x")"#).is_some());
    assert!(egress("img.src = '//example.com/x';").is_some());
    assert!(egress("x { background: url(//example.com/f.woff2) }").is_some());
    assert!(egress("x { background: url(/img/a.png) }").is_some());
    assert!(egress("@import 'a.css';").is_some());
    assert!(egress("a { src: url(https://example.com/f) }").is_some());
    assert_eq!(
        egress("// a comment about the page\nlet x = 1; // another"),
        None
    );
    assert_eq!(egress("x { src: url(/fonts/a.woff2) }"), None);
    // Nothing in the page points at another origin: every `src` and `href`
    // is a path of the page.
    for attribute in ["src=\"", "href=\""] {
        for part in INDEX_HTML.split(attribute).skip(1) {
            assert!(part.starts_with('/'), "{attribute}{part:.30}");
            assert!(!part.starts_with("//"), "{attribute}{part:.30}");
        }
    }
    let policy = SECURITY_HEADERS
        .iter()
        .find(|(name, _)| *name == "Content-Security-Policy")
        .unwrap()
        .1;
    assert_eq!(policy, "default-src 'self'; frame-ancestors 'none'");
}

const SANS_STACK: &str =
    r#""Bricolage Grotesque", system-ui, -apple-system, "Segoe UI", sans-serif"#;
const MONO_STACK: &str = r#""JetBrains Mono", ui-monospace, "SF Mono", Menlo, Consolas, monospace"#;

#[test]
fn the_style_sheet_sets_both_typefaces_from_the_binary_with_the_identity_stacks() {
    let css = lf(APP_CSS);
    for (family, file) in [
        ("Bricolage Grotesque", "BricolageGrotesque-latin.woff2"),
        ("JetBrains Mono", "JetBrainsMono-Regular-latin.woff2"),
        ("JetBrains Mono", "JetBrainsMono-Medium-latin.woff2"),
    ] {
        let face = css
            .split("@font-face {")
            .skip(1)
            .find(|face| face.contains(&format!("url(/fonts/{file})")))
            .unwrap_or_else(|| panic!("no @font-face for {file}"));
        assert!(
            face.contains(&format!("font-family: \"{family}\";")),
            "{file}"
        );
        assert!(face.contains("font-display: swap;"), "{file}");
        assert!(face.contains("unicode-range: U+0020-007E"), "{file}");
        assert!(FONT_FILES.contains(&file));
    }
    assert!(css.contains("font-weight: 200 800;"));
    assert!(block(&css, "\nbody {", "").contains(&format!("font-family: {SANS_STACK};")));
    assert!(block(&css, "\ncode {", "").contains(&format!("font-family: {MONO_STACK};")));
    assert!(block(&css, "\n.wordmark {", "").contains(&format!("font-family: {MONO_STACK};")));
    assert!(block(&css, "\nbody {", "").contains("line-height: 1.5;"));
    assert!(block(&css, "\nh1,\nh2,\nh3 {", "").contains("line-height: 1.15;"));
    // Section 5's weights: 400, 600 and 800 for the sans, 400 and 500 for the
    // mono; nothing else, not even 700.
    for rule in css.split('}') {
        let mono = rule.contains(&format!("font-family: {MONO_STACK};"));
        for line in rule
            .lines()
            .filter(|line| line.trim().starts_with("font-weight:"))
        {
            let weight = line.trim().trim_start_matches("font-weight:").trim();
            let allowed: &[&str] = if rule.contains("@font-face") {
                &["200 800", "400", "500"]
            } else if mono {
                &["400;", "500;"]
            } else {
                &["400;", "600;", "800;"]
            };
            assert!(
                allowed
                    .iter()
                    .any(|ok| weight.starts_with(ok) || weight == *ok),
                "{line}"
            );
        }
    }
    // Every font the sheet names is a route that answers.
    let data = tempfile::tempdir().unwrap();
    let ui = serve(data.path(), RunSecret::new().unwrap());
    for part in css.split("url(").skip(1) {
        let path = part.split(')').next().unwrap();
        assert_eq!(get(ui.port(), path, &[]).status, 200, "{path}");
    }
}

#[test]
fn the_carried_fonts_are_latin_woff2_of_bounded_size_and_documented() {
    let readme = std::fs::read_to_string(font_dir().join("README.md")).unwrap();
    for name in FONT_FILES {
        let bytes = std::fs::read(font_dir().join(name)).unwrap();
        assert_eq!(&bytes[..4], b"wOF2", "{name}");
        assert!(bytes.len() < FONT_CAP, "{name}: {} bytes", bytes.len());
        assert!(readme.contains(name), "{name} is not in the README");
    }
    for licence in ["OFL-BricolageGrotesque.txt", "OFL-JetBrainsMono.txt"] {
        assert!(font_dir().join(licence).is_file(), "{licence}");
    }
}

/// A data directory that serves `o/r`, a ticket page's ground.
struct Ground {
    data: tempfile::TempDir,
    ui: Serving,
    session: Session,
}

const RUN: &str = "20261105T100000Z";

impl Ground {
    fn new() -> Self {
        Self::with_options(options())
    }

    fn with_options(options: Options) -> Self {
        let data = tempfile::tempdir().unwrap();
        let repo = Repo::parse("o/r").unwrap();
        let checkout = data.path().join("checkout-of-o-r");
        std::fs::create_dir_all(&checkout).unwrap();
        Registry::in_data_dir(data.path())
            .register(&repo, &checkout)
            .unwrap();
        let ui = serve_with(data.path(), RunSecret::new().unwrap(), options);
        let session = session(ui.port(), data.path());
        Self { data, ui, session }
    }

    fn runs(&self, ticket: &str) -> PathBuf {
        ProjectDirs::new(self.data.path(), &Repo::parse("o/r").unwrap())
            .runs(&TicketId::new(ticket).unwrap())
    }

    /// The folder of run `run` of `OWL-1`, made.
    fn run_dir(&self, run: &str) -> PathBuf {
        let dir = self.runs("OWL-1").join(run);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn get(&self, path: &str) -> Reply {
        get(
            self.ui.port(),
            path,
            &[("X-Owlshift-Session", &self.session.header())],
        )
    }

    fn log(&self, extra: &str) -> Reply {
        self.get(&format!(
            "/api/ticket/log?project=o%2Fr&ticket=OWL-1&run={RUN}&file=stdout{extra}"
        ))
    }

    fn append(&self, file: &str, text: &str) {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.run_dir(RUN).join(file))
            .unwrap();
        file.write_all(text.as_bytes()).unwrap();
    }
}

#[test]
fn the_ticket_routes_need_a_session() {
    let ground = Ground::new();
    for path in [
        "/api/ticket?project=o%2Fr&ticket=OWL-1",
        "/api/ticket/log?project=o%2Fr&ticket=OWL-1&run=20261105T100000Z&file=stdout",
    ] {
        let none = get(ground.ui.port(), path, &[]);
        assert_eq!(none.status, 401, "{path}");
        none.assert_secured();
        let made_up = get(
            ground.ui.port(),
            path,
            &[("X-Owlshift-Session", "nonsense")],
        );
        assert_eq!(made_up.status, 401, "{path}");
    }
}

#[test]
fn the_ticket_routes_refuse_what_names_no_served_ticket() {
    let ground = Ground::new();
    ground.run_dir(RUN);
    let log = |query: &str| ground.get(&format!("/api/ticket/log?{query}"));
    let page = |query: &str| ground.get(&format!("/api/ticket?{query}"));
    // An unserved project, a malformed one, a missing one.
    assert_eq!(page("project=o%2Fother&ticket=OWL-1").status, 404);
    assert_eq!(page("project=nonsense&ticket=OWL-1").status, 400);
    assert_eq!(page("ticket=OWL-1").status, 400);
    assert_eq!(page("project=o%2Fr").status, 400);
    // A ticket that is no ticket id.
    for ticket in ["..%2Fx", "../x", "OWL-1%2F..", "OWL%201", ""] {
        let reply = page(&format!("project=o%2Fr&ticket={ticket}"));
        assert_eq!(reply.status, 400, "{ticket}");
        reply.assert_secured();
    }
    // A repeated key and a broken escape.
    assert_eq!(page("project=o%2Fr&ticket=OWL-1&ticket=OWL-2").status, 400);
    assert_eq!(page("project=o%2Fr&ticket=OWL-%zz").status, 400);
    // A run, a file and an offset that name nothing the route reads.
    let base = "project=o%2Fr&ticket=OWL-1";
    for run in ["..", "..%2F..", "20261105T100000Z-x", "x", ""] {
        assert_eq!(
            log(&format!("{base}&run={run}&file=stdout")).status,
            400,
            "{run}"
        );
    }
    for file in ["result", "stdout.log", "..%2Fstdout", ""] {
        assert_eq!(
            log(&format!("{base}&run={RUN}&file={file}")).status,
            400,
            "{file}"
        );
    }
    for from in ["-1", "x", "1.5", ""] {
        let reply = log(&format!("{base}&run={RUN}&file=stdout&from={from}"));
        assert_eq!(reply.status, 400, "{from}");
    }
    // A run that has no folder.
    assert_eq!(
        log(&format!("{base}&run=20261105T110000Z&file=stdout")).status,
        404
    );
    // A run folder that is a link is not followed.
    #[cfg(unix)]
    {
        let elsewhere = ground.data.path().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::write(
            elsewhere.join("stdout.log"),
            "{\"type\":\"turn.started\"}\n",
        )
        .unwrap();
        std::os::unix::fs::symlink(&elsewhere, ground.runs("OWL-1").join("20261105T120000Z"))
            .unwrap();
        let reply = log(&format!("{base}&run=20261105T120000Z&file=stdout"));
        assert_eq!(reply.status, 404);
        // Nor a log that is a link.
        std::os::unix::fs::symlink(
            elsewhere.join("stdout.log"),
            ground.run_dir(RUN).join("stdout.log"),
        )
        .unwrap();
        assert_eq!(log(&format!("{base}&run={RUN}&file=stdout")).status, 404);
    }
}

#[test]
fn the_ticket_snapshot_takes_a_percent_encoded_project() {
    let ground = Ground::new();
    for query in [
        "project=o%2Fr&ticket=OWL-1",
        "ticket=OWL-1&project=o/r",
        "project=O%2FR&ticket=OWL-1",
    ] {
        let reply = ground.get(&format!("/api/ticket?{query}"));
        assert_eq!(reply.status, 200, "{query}: {}", reply.text());
        reply.assert_secured();
        let value = reply.json();
        assert_eq!(value["format"], 1);
        assert_eq!(value["ticket"], "OWL-1");
        assert!(value["runs"]["runs"].as_array().unwrap().is_empty());
    }
}

#[test]
fn a_recorded_log_is_served_as_records_to_its_end() {
    let ground = Ground::new();
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../owlshift-adapters/tests/fixtures/claude/success.jsonl");
    let bytes = std::fs::read(&fixture).unwrap();
    std::fs::write(ground.run_dir(RUN).join("stdout.log"), &bytes).unwrap();
    let reply = ground.log("&from=0");
    assert_eq!(reply.status, 200, "{}", reply.text());
    reply.assert_secured();
    let value = reply.json();
    assert_eq!(value["next"], bytes.len());
    assert_eq!(value["size"], bytes.len());
    assert_eq!(value["reset"], false);
    assert_eq!(value["ended"], false);
    let lines = bytes
        .split(|&b| b == b'\n')
        .filter(|l| !l.is_empty())
        .count();
    let records = value["records"].as_array().unwrap();
    assert!(records.len() >= lines, "{} < {lines}", records.len());
    assert_eq!(value["next_line"], lines);
    assert!(records.iter().all(|r| r["kind"] != "unknown"), "{value}");
    // Asked again from `next`, with or without the line count: nothing new.
    for extra in ["&from=SIZE", "&from=SIZE&line=LINES"] {
        let extra = extra
            .replace("SIZE", &bytes.len().to_string())
            .replace("LINES", &lines.to_string());
        let again = ground.log(&extra).json();
        assert!(again["records"].as_array().unwrap().is_empty());
        assert_eq!(again["next"], bytes.len());
    }
}

#[test]
fn a_log_holding_markup_is_served_as_text() {
    let ground = Ground::new();
    let hostile = "<script>alert(1)</script> <b>bold</b> & \u{2028}\u{2029}";
    let assistant = json!({
        "type": "assistant",
        "message": { "content": [{ "type": "text", "text": hostile }] }
    });
    ground.append(
        "stdout.log",
        &format!("{assistant}\n<b>not json</b> & \u{2028}\n{{\"type\":\"<i>new</i>\"}}\n"),
    );
    ground.append("stderr.log", &format!("{hostile}\n"));
    for file in ["stdout", "stderr"] {
        let reply = ground.get(&format!(
            "/api/ticket/log?project=o%2Fr&ticket=OWL-1&run={RUN}&file={file}"
        ));
        assert_eq!(reply.status, 200);
        let text = reply.text();
        for c in ['<', '>', '&', '\u{2028}', '\u{2029}'] {
            assert!(!text.contains(c), "{file}: {c:?} in {text}");
        }
        let value: Value = serde_json::from_str(&text).unwrap();
        let records = value["records"].as_array().unwrap();
        if file == "stdout" {
            assert_eq!(records.len(), 3);
            assert_eq!(records[0]["kind"], "text");
            assert_eq!(records[0]["text"], hostile);
            assert_eq!(records[1]["kind"], "unknown");
            assert_eq!(records[1]["text"], "<b>not json</b> & \u{2028}");
            assert_eq!(records[2]["kind"], "unknown");
            assert_eq!(records[2]["text"], "{\"type\":\"<i>new</i>\"}");
        } else {
            assert_eq!(records[0]["text"], hostile);
        }
    }
}

#[test]
fn a_ticket_route_survives_the_login_and_is_read_after_it() {
    let app_js = lf(APP_JS);
    // Only a `#login=` fragment is replaced: a ticket page's route stays.
    let login = block(&app_js, "async function login() {", "");
    let guarded = "  if (window.location.hash.startsWith(\"#login=\")) {\n    \
                   window.history.replaceState(null, \"\", window.location.pathname);\n  }";
    assert!(login.contains(guarded), "{login}");
    assert_eq!(login.matches("replaceState").count(), 1);
    // `start()` reads the route after the login, and a hash change reads it
    // again.
    let start = block(&app_js, "async function start() {", "");
    let logged_in = start.find("await login();").unwrap();
    let routed = start.find("\n  route();").unwrap();
    assert!(logged_in < routed, "{start}");
    assert!(start.contains(r#"window.addEventListener("hashchange", route);"#));
    // A route ends the page open before it draws another, and a ticket's is
    // `#/ticket/<owner>/<repo>/<TICKET>`.
    let route = block(&app_js, "function route() {", "");
    assert!(route.find("closeTicket();").unwrap() < route.find("openTicket(target);").unwrap());
    assert!(app_js.contains(r##"const prefix = "#/ticket/";"##));
    assert!(app_js.contains("parts.length !== 3"));
    // The page's container is in the frame, hidden until a route shows it.
    assert!(INDEX_HTML.contains(r#"<section id="ticket" class="ticket" hidden></section>"#));
}

/// JavaScript has no test setup here: these pin the structure that keeps a
/// ticket page's loops from outliving their route or their session (the
/// manual checks are in the pull request).
#[test]
fn the_ticket_page_stops_what_it_scheduled_with_its_route_and_its_session() {
    let app_js = lf(APP_JS);
    // A logout empties the ticket page like the header.
    let logged_out = block(&app_js, "function loggedOut(sent) {", "");
    assert!(logged_out.contains("closeTicket();"), "{logged_out}");
    assert!(logged_out.contains(r#"clear(document.getElementById("header"));"#));
    // The one teardown: stops every timer of the page, bumps the
    // generation, empties and hides the container.
    let close = block(&app_js, "function closeTicket() {", "");
    for part in [
        "generation += 1;",
        "window.clearTimeout(openPage.snapshotTimer);",
        "window.clearTimeout(log.timer);",
        "clear(el);",
        "el.hidden = true;",
        "openPage = null;",
    ] {
        assert!(close.contains(part), "{part}: {close}");
    }
    // A hash change goes to `route`, which ends the page open before it
    // draws another.
    let route = block(&app_js, "function route() {", "");
    assert!(route.starts_with("\n  closeTicket();"), "{route}");
    // Every continuation after an await is for the page that is open only:
    // the loops compare the generation, through `alive`, after each await,
    // and a null answer (a session that ended, which `authedFetch` already
    // told the page of) ends the loop without a call of its own.
    let alive = block(&app_js, "function alive(handle) {", "");
    assert!(alive.contains("openPage === handle && handle.generation === generation"));
    for name in [
        "async function refresh(handle) {",
        "async function pollLog(handle, run, which) {",
    ] {
        let function = block(&app_js, name, "");
        let continuations: Vec<&str> = function.split("await ").skip(1).collect();
        assert!(continuations.len() >= 2, "{name}");
        for continuation in continuations {
            assert!(
                continuation.contains("alive(handle)"),
                "{name}: {continuation}"
            );
        }
        assert!(
            function.contains("if (!alive(handle) || !answer) {\n    return;\n  }"),
            "{name}"
        );
        assert!(function.contains("\n  if (!alive(handle)"), "{name}");
    }
    // The pace: the snapshot every 3 s while a run has no end, every 10 s
    // otherwise, again at once when the page shows; a log every second,
    // every 5 s after 30 s without data.
    for pace in [
        "const SNAPSHOT_LIVE_MS = 3000;",
        "const SNAPSHOT_IDLE_MS = 10000;",
        "const LOG_FAST_MS = 1000;",
        "const LOG_SLOW_MS = 5000;",
        "const LOG_SLOW_AFTER_MS = 30000;",
    ] {
        assert!(app_js.contains(pace), "{pace}");
    }
    assert!(app_js.contains(r#"document.addEventListener("visibilitychange""#));
    // A refresh keeps what is drawn: a run already listed is not made again,
    // and a log is drawn from where it stopped.
    assert!(app_js.contains("let run = handle.runs.get(info.id);\n    if (!run) {"));
    assert!(app_js.contains("log.from = value.next;"));
    assert!(app_js.contains("if (value.ended && value.next >= value.size) {"));
}

/// The contracts leave an ask's `verdicts` out of its JSON until a check
/// gives them, which is every ask still waiting for its answer: the page
/// must read them as absent, and one section that throws must not stop the
/// refresh or the runs.
#[test]
fn the_ticket_page_reads_what_the_contracts_leave_out_and_survives_a_section_that_throws() {
    use owlshift_contracts::refs::TicketQuestions;
    let waiting = TicketQuestions::parse(
        r#"{"format":18,"asks":[{"kind":"questions","round":1,"at":"2026-11-05T09:00:00Z",
        "comment":"c1","questions":[{"id":"Q1","category":"scope","context":"c","text":"t",
        "options":[]}],"decider":{"account":"maintainer","by":"assignee"}}]}"#,
    )
    .unwrap();
    let json = serde_json::to_value(&waiting).unwrap();
    assert!(json["asks"][0].get("verdicts").is_none(), "{json}");

    let app_js = lf(APP_JS);
    assert!(app_js.contains("(ask.verdicts || []).find("));
    assert!(!app_js.contains("ask.verdicts.find("));
    // Each section is drawn in its own try/catch, the runs too, and the next
    // refresh is scheduled whatever the drawing did.
    let snapshot = block(&app_js, "function renderSnapshot(handle, snapshot) {", "");
    assert!(
        snapshot.contains("    try {\n      if (part && part.error) {"),
        "{snapshot}"
    );
    assert!(snapshot.contains("draw(handle, body, part, snapshot);"));
    assert!(snapshot.contains("    renderRuns(handle, snapshot.runs);\n  } catch (error) {"));
    let refresh = block(&app_js, "async function refresh(handle) {", "");
    let (drawn, scheduled) = refresh
        .split_once("renderSnapshot(handle, value);")
        .expect("refresh draws the snapshot");
    assert!(drawn.ends_with("    try {\n      "), "{drawn}");
    assert!(scheduled.contains("} catch (error) {"));
    assert!(scheduled.contains("handle.snapshotTimer = window.setTimeout"));
}

/// A run only the events name has no folder: its log is not followed, and a
/// log the server says is not there stops being asked for once the run ended.
#[test]
fn the_ticket_page_follows_no_log_a_run_cannot_have() {
    let app_js = lf(APP_JS);
    assert!(app_js.contains("noLog: info.folder === false,"));
    let start = block(&app_js, "function startLog(handle, run, which) {", "");
    let (no_log, followed) = start.split_once("log.started = true;").unwrap();
    assert!(no_log.contains("if (run.noLog) {"), "{start}");
    assert!(no_log.contains("return;"));
    assert!(followed.contains("pollLog(handle, run, which);"));
    let poll = block(&app_js, "async function pollLog(handle, run, which) {", "");
    assert!(poll.contains(
        "(status === 404 || status === 400) && run.info && run.info.ended) {\n      log.finished = true;\n      return;"
    ));
}

impl Ground {
    /// Appends an event of `OWL-1`'s run `RUN` to the event log.
    fn event(&self, kind: EventKind) {
        let event = Event {
            format: Format,
            at: "2026-11-05T10:00:00Z".parse().unwrap(),
            project: "o/r".to_owned(),
            ticket: Some(TicketId::new("OWL-1").unwrap()),
            run: Some(RUN.to_owned()),
            kind,
            data: serde_json::Map::new(),
        };
        EventLog::in_dir(self.data.path()).append(&event).unwrap();
    }

    fn snapshot(&self) -> Value {
        let reply = self.get("/api/ticket?project=o%2Fr&ticket=OWL-1");
        assert_eq!(reply.status, 200, "{}", reply.text());
        reply.json()
    }
}

/// One line of an assistant's text, as Claude Code writes it.
fn said(text: &str) -> String {
    format!(
        "{}\n",
        json!({ "type": "assistant", "message": { "content": [{ "type": "text", "text": text }] } })
    )
}

fn texts(reply: &Reply) -> Vec<String> {
    reply.json()["records"]
        .as_array()
        .unwrap()
        .iter()
        .map(|record| record["text"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn a_live_logs_new_lines_are_served_as_written() {
    let ground = Ground::new();
    ground.event(EventKind::RunStarted);
    ground.append("stdout.log", &(said("one") + &said("two")));
    let first = ground.log("&from=0");
    assert_eq!(texts(&first), ["one", "two"]);
    let value = first.json();
    assert_eq!(value["ended"], false);
    let next = value["next"].as_u64().unwrap();
    assert_eq!(next, value["size"].as_u64().unwrap());
    assert_eq!(value["next_line"], 2);

    // A third line and half of a fourth: only the third is served.
    let fourth = said("four");
    let (head, tail) = fourth.split_at(fourth.len() / 2);
    ground.append("stdout.log", &(said("three") + head));
    let second = ground.log(&format!("&from={next}&line=2"));
    assert_eq!(texts(&second), ["three"]);
    assert_eq!(second.json()["records"][0]["line"], 3);
    assert_eq!(second.json()["records"][0]["offset"], next);
    let next = second.json()["next"].as_u64().unwrap();
    assert!(next < second.json()["size"].as_u64().unwrap());

    // The fourth completes: it is served whole, its line counted from the
    // file when the page does not say.
    ground.append("stdout.log", tail);
    let third = ground.log(&format!("&from={next}"));
    assert_eq!(texts(&third), ["four"]);
    assert_eq!(third.json()["records"][0]["line"], 4);
    let next = third.json()["next"].as_u64().unwrap();
    assert_eq!(next, third.json()["size"].as_u64().unwrap());

    // A last line with no newline waits while the run goes on.
    ground.append(
        "stdout.log",
        "{\"type\":\"result\",\"subtype\":\"success\"}",
    );
    let waiting = ground.log(&format!("&from={next}"));
    assert!(texts(&waiting).is_empty());
    assert_eq!(waiting.json()["ended"], false);
    assert_eq!(waiting.json()["next"], next);
    let live = ground.snapshot();
    assert_eq!(live["runs"]["runs"][0]["ended"], false);

    // Once the run ended, it never gets its newline: it is served, and
    // `next` reaches the size.
    ground.event(EventKind::RunEnded);
    let last = ground.log(&format!("&from={next}"));
    let value = last.json();
    assert_eq!(value["ended"], true);
    assert_eq!(value["records"][0]["kind"], "result");
    assert_eq!(value["records"][0]["line"], 5);
    assert_eq!(value["next"], value["size"]);
    assert_eq!(ground.snapshot()["runs"]["runs"][0]["ended"], true);
    // And a poll after it has nothing more.
    let after = ground.log(&format!("&from={}", value["next"]));
    assert!(texts(&after).is_empty());
    assert_eq!(after.json()["next"], value["size"]);
}

#[test]
fn a_run_that_starts_after_the_first_snapshot_is_listed() {
    let ground = Ground::new();
    assert!(
        ground.snapshot()["runs"]["runs"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    ground.append("stdout.log", &said("hello"));
    ground.event(EventKind::RunStarted);
    let runs = ground.snapshot()["runs"]["runs"].clone();
    let runs = runs.as_array().unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0]["id"], RUN);
    assert_eq!(runs[0]["ended"], false);
    assert_eq!(texts(&ground.log("&from=0")), ["hello"]);
}

// ---------------------------------------------------------------------------
// The overview (OWL-470): `/api/overview`, `/api/since` and the stream's
// `overview` line, on the data directory of `Ground`, which serves `o/r` with
// a checkout that holds no refs.

impl Ground {
    fn repo(&self) -> Repo {
        Repo::parse("o/r").unwrap()
    }

    /// Rewrites `o/r`'s `ready.json` with these `(ticket, title)`, all
    /// waiting for the next pass, written at `at`.
    fn ready(&self, tickets: &[(&str, &str)], at: Timestamp) {
        use owlshift_contracts::ready::{Outcome, ReadySnapshot, ReadyTicket};
        use owlshift_contracts::waits::Wait;
        let dirs = ProjectDirs::new(self.data.path(), &self.repo());
        std::fs::create_dir_all(dirs.root()).unwrap();
        let snapshot = ReadySnapshot {
            format: Format,
            at,
            interval_ms: 60_000,
            tickets: tickets
                .iter()
                .map(|(ticket, title)| ReadyTicket {
                    ticket: TicketId::new(*ticket).unwrap(),
                    title: (*title).to_owned(),
                    priority: owlshift_core::vocab::Priority::High,
                    blockers: Vec::new(),
                    drain: Outcome::Waits {
                        wait: Wait {
                            reason: "next_pass".to_owned(),
                            since: None,
                            behind: Vec::new(),
                            resource: None,
                            harness: None,
                            until: None,
                            text: "chosen, it starts at the next pass".to_owned(),
                        },
                    },
                })
                .collect(),
        };
        owlshift_runner::ready::write(&dirs, &snapshot).unwrap();
    }

    /// Writes the ticket ref of `ticket` in the checkout of `o/r`, made a
    /// repository on the first call, with the ticket at Build (in flight, so
    /// it has a card whether or not `ready.json` lists it).
    fn building(&self, ticket: &str) {
        use owlshift_contracts::refs::PersistedState;
        use owlshift_core::state::{Status, TicketState};
        use owlshift_core::vocab::Stage;
        use owlshift_runner::ticket_ref::{self, TicketRecord};
        // Where the status reader looks for the ticket refs of `o/r`.
        let checkout = ProjectDirs::new(self.data.path(), &self.repo()).checkout();
        std::fs::create_dir_all(&checkout).unwrap();
        let git = owlshift_runner::project::runner_git();
        if !checkout.join(".git").exists() {
            let init = std::process::Command::new("git")
                .args(["init", "-q"])
                .current_dir(&checkout)
                .output()
                .unwrap();
            assert!(init.status.success(), "{init:?}");
        }
        let ticket = TicketId::new(ticket).unwrap();
        let state = TicketState::restore(Status::Active(Stage::Build), 0, 0, 0, 0, 0).unwrap();
        let record = TicketRecord {
            state: PersistedState::from(&state),
            ..ticket_ref::blank(None)
        };
        let previous = ticket_ref::read(&git, &checkout, &ticket)
            .unwrap()
            .map(|stored| stored.commit);
        ticket_ref::write(&git, &checkout, &ticket, &record, previous.as_deref()).unwrap();
    }

    /// Appends an event of the project `o/r` that happened at `at`.
    fn event_at(&self, at: Timestamp, ticket: &str, kind: EventKind) {
        let event = Event {
            format: Format,
            at,
            project: "o/r".to_owned(),
            ticket: Some(TicketId::new(ticket).unwrap()),
            run: None,
            kind,
            data: serde_json::Map::new(),
        };
        EventLog::in_dir(self.data.path()).append(&event).unwrap();
    }

    fn overview(&self) -> Value {
        let reply = self.get("/api/overview");
        assert_eq!(reply.status, 200, "{}", reply.text());
        reply.assert_secured();
        reply.json()
    }

    fn stream(&self) -> BufReader<TcpStream> {
        open_stream(self.ui.port(), &self.session)
    }
}

/// The next line of `stream` that holds `key`, the others skipped.
fn next_with(stream: &mut BufReader<TcpStream>, key: &str) -> Value {
    loop {
        let line = next_line(stream).expect("the stream ended");
        let value: Value = serde_json::from_str(&line).unwrap();
        if value.get(key).is_some() {
            return value;
        }
    }
}

fn hours_ago(hours: i64) -> Timestamp {
    Timestamp::now()
        .checked_sub(jiff::SignedDuration::from_hours(hours))
        .unwrap()
}

fn card_tickets(overview: &Value, column: &str) -> Vec<String> {
    overview["projects"][0]["cards"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|card| card["column"] == column)
        .map(|card| card["ticket"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn the_overview_routes_need_a_session() {
    let ground = Ground::new();
    for path in ["/api/overview", "/api/since"] {
        let none = get(ground.ui.port(), path, &[]);
        assert_eq!(none.status, 401, "{path}");
        none.assert_secured();
        let made_up = get(
            ground.ui.port(),
            path,
            &[("X-Owlshift-Session", "nonsense")],
        );
        assert_eq!(made_up.status, 401, "{path}");
        let reply = ground.get(path);
        assert_eq!(reply.status, 200, "{path}: {}", reply.text());
        reply.assert_secured();
        assert!(
            reply
                .header("Content-Type")
                .unwrap()
                .starts_with("application/json")
        );
    }
}

#[test]
fn the_overview_lists_the_admitted_tickets_of_ready_json_not_started() {
    let ground = Ground::new();
    ground.ready(
        &[("OWL-1", "First"), ("OWL-2", "<script>alert(1)</script>")],
        Timestamp::now(),
    );
    let reply = ground.get("/api/overview");
    assert_eq!(reply.status, 200);
    let text = reply.text();
    assert!(!text.contains('<') && !text.contains('>'), "{text}");
    let overview = reply.json();
    assert_eq!(overview["projects"][0]["repository"], "o/r");
    assert_eq!(card_tickets(&overview, "not_started"), ["OWL-1", "OWL-2"]);
    assert_eq!(
        overview["projects"][0]["cards"][1]["title"],
        "<script>alert(1)</script>"
    );
    assert_eq!(overview["projects"][0]["ready"]["stale"], false);
    // A snapshot older than two of its intervals is stale (F5).
    ground.ready(&[("OWL-1", "First")], hours_ago(2));
    assert_eq!(ground.overview()["projects"][0]["ready"]["stale"], true);
}

#[test]
fn a_stream_writes_the_overview_again_when_a_ref_a_pause_ready_json_or_an_event_changes() {
    let ground = Ground::new();
    ground.ready(&[("OWL-1", "First")], Timestamp::now());
    let mut stream = ground.stream();
    let first = next_with(&mut stream, "overview");
    let first = &first["overview"];
    assert_eq!(card_tickets(first, "not_started"), ["OWL-1"]);
    assert_eq!(first["projects"][0]["paused"], Value::Null);
    let highlights = first["highlights"].as_u64().unwrap();

    // A new ready.json shows without a request.
    ground.ready(&[("OWL-1", "First"), ("OWL-3", "Third")], Timestamp::now());
    let second = next_with(&mut stream, "overview");
    assert_eq!(
        card_tickets(&second["overview"], "not_started"),
        ["OWL-1", "OWL-3"]
    );

    // A project's pause shows as `paused`.
    let dirs = ProjectDirs::new(ground.data.path(), &ground.repo());
    requests::ask_pause(
        ground.data.path(),
        Some(&dirs),
        &Request::new(BY_PAUSE, Timestamp::now(), "lunch"),
    )
    .unwrap();
    let third = next_with(&mut stream, "overview");
    assert_eq!(third["overview"]["projects"][0]["paused"]["why"], "lunch");

    // A new ticket ref shows as a card in its stage's column.
    ground.building("OWL-7");
    let fourth = loop {
        let line = next_with(&mut stream, "overview");
        if card_tickets(&line["overview"], "build").contains(&"OWL-7".to_owned()) {
            break line;
        }
    };
    assert_eq!(
        card_tickets(&fourth["overview"], "build"),
        ["OWL-7"],
        "{fourth}"
    );

    // A new event that changes no ref, no ready.json and no wait still
    // moves the line: its highlights counter goes up.
    ground.event_at(Timestamp::now(), "OWL-1", EventKind::PrMerged);
    let fifth = next_with(&mut stream, "overview");
    assert!(
        fifth["overview"]["highlights"].as_u64().unwrap() > highlights,
        "{fifth}"
    );
}

/// A stream that starts after the last one closed, with the overview kept
/// from before changed meanwhile, gets the new one. This covers the
/// closed-stream path end to end; the race between the first line and the
/// generation read is pinned by `server.rs`'s unit test
/// `the_stored_overview_and_its_generation_are_read_together`.
#[test]
fn a_stream_that_starts_after_another_closed_gets_the_overview_as_it_is_now() {
    let ground = Ground::with_options(Options {
        keepalive: Duration::from_millis(50),
        ..options()
    });
    ground.ready(&[("OWL-1", "First")], Timestamp::now());
    let mut first = ground.stream();
    assert_eq!(
        card_tickets(
            &next_with(&mut first, "overview")["overview"],
            "not_started"
        ),
        ["OWL-1"]
    );
    drop(first);
    // The server notices the closed stream at its next keepalive, and the
    // refresher goes once no stream is left.
    thread::sleep(Duration::from_millis(500));
    ground.ready(&[("OWL-1", "First"), ("OWL-9", "Ninth")], Timestamp::now());
    let mut second = ground.stream();
    for _ in 0..40 {
        let line = next_with(&mut second, "overview");
        if card_tickets(&line["overview"], "not_started") == ["OWL-1", "OWL-9"] {
            return;
        }
    }
    panic!("the new stream never got OWL-9");
}

#[test]
fn since_counts_the_last_24_hours_when_it_is_given_no_time() {
    let ground = Ground::new();
    ground.event_at(hours_ago(2), "OWL-1", EventKind::PrMerged);
    ground.event_at(hours_ago(30), "OWL-2", EventKind::PrMerged);
    let reply = ground.get("/api/since");
    assert_eq!(reply.status, 200, "{}", reply.text());
    reply.assert_secured();
    let since = reply.json();
    let merged: Vec<_> = since["merged"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["ticket"].as_str().unwrap())
        .collect();
    assert_eq!(merged, ["OWL-1"]);
    assert_eq!(since["merged"][0]["project"], "o/r");
    let from: Timestamp = since["from"].as_str().unwrap().parse().unwrap();
    let back = Timestamp::now().as_second() - from.as_second();
    assert!(
        (24 * 3600 - 120..=24 * 3600 + 120).contains(&back),
        "from is {back} s back"
    );
    assert_eq!(since["truncated"], false);
    // An event after the baseline of a load, with that `at` percent-encoded
    // as the page sends it (F2).
    let at = hours_ago(1).to_string();
    let encoded = at.replace(':', "%3A").replace('+', "%2B");
    ground.event_at(Timestamp::now(), "OWL-3", EventKind::PrMerged);
    let reply = ground.get(&format!("/api/since?at={encoded}"));
    assert_eq!(reply.status, 200, "{}", reply.text());
    assert_eq!(reply.json()["merged"][0]["ticket"], "OWL-3");
    assert_eq!(reply.json()["merged"].as_array().unwrap().len(), 1);
    for bad in [
        "at=",
        "at=yesterday",
        "at=2026-13-45T00:00:00Z",
        "at=1&at=2",
    ] {
        let reply = ground.get(&format!("/api/since?{bad}"));
        assert_eq!(reply.status, 400, "{bad}");
        reply.assert_secured();
    }
}

/// The page's part of the overview, by what it carries: the browser is not
/// driven here (the pull request records the manual check), and everything
/// the page only draws (the list's order, the strip, the 24-hour default) is
/// tested on the server.
#[test]
fn the_overview_page_links_only_a_guarded_pull_request_and_keeps_the_view_and_the_visit() {
    let app_js = lf(APP_JS);
    // The one `.href` assignment is the pull request's, guarded by the
    // repository and number patterns; the other links are ticket pages' hash
    // routes, set as attributes from encoded parts.
    assert_eq!(app_js.matches(".href =").count(), 1);
    let link = block(&app_js, "function pullRequestLink(project, number) {", "");
    let (guard, assigned) = link.split_once(".href =").unwrap();
    assert!(guard.contains("REPOSITORY_PATTERN.test(project)"), "{link}");
    assert!(guard.contains("Number.isInteger(number)"), "{link}");
    assert!(guard.contains("return make(\"span\""), "{link}");
    assert!(
        assigned.starts_with(" \"https://github.com/\" + project + \"/pull/\" + number;"),
        "{link}"
    );
    assert!(link.contains("link.rel = \"noopener noreferrer\";"));
    assert!(app_js.contains(r#"const REPOSITORY_PATTERN = /^[A-Za-z0-9._-]+\/[A-Za-z0-9._-]+$/;"#));
    for part in app_js.split("setAttribute(\"href\"").skip(1) {
        assert!(
            part.trim_start().starts_with(", \"#/ticket/\" +"),
            "{part:.60}"
        );
    }
    // The view is kept under `owlshift.view`, columns by default.
    assert!(app_js.contains("const VIEW_KEY = \"owlshift.view\";"));
    assert!(app_js.contains("return stored(VIEW_KEY) === \"list\" ? \"list\" : \"columns\";"));
    assert!(app_js.contains("store(VIEW_KEY, view);"));
    // The visit: the stored time is read before this load's time is stored,
    // and the request carries `at` only when one was read.
    assert!(app_js.contains("const VISIT_KEY = \"owlshift.lastVisit\";"));
    let visit = block(&app_js, "function startVisit() {", "");
    let read = visit.find("stored(VISIT_KEY)").unwrap();
    let write = visit
        .find("store(VISIT_KEY, new Date().toISOString())")
        .unwrap();
    assert!(read < write, "{visit}");
    assert!(
        app_js.contains("(previousVisit ? \"?at=\" + encodeURIComponent(previousVisit) : \"\");")
    );
    assert_eq!(app_js.matches("store(VISIT_KEY").count(), 1);
    // The panel is asked for again when the count of highlights moved.
    assert!(app_js.contains("if (overview.highlights !== lastHighlights) {"));
    // The overview's notes are drawn as text, as the header's are (F7), and a
    // stale snapshot is marked with its age (F5).
    assert!(app_js.contains("for (const note of overview.notes) {"));
    assert!(app_js.contains("body.appendChild(make(\"p\", \"fact state-information\", note));"));
    assert!(app_js.contains("ready.json is stale: the drain last read the column "));
    // The frame: the three sections and the switch, no inline script or style.
    for id in [
        "id=\"for-you\"",
        "id=\"overview\"",
        "id=\"since\"",
        "id=\"view-columns\"",
        "id=\"view-list\"",
    ] {
        assert!(INDEX_HTML.contains(id), "{id}");
    }
    // Only the viewing of columns and list is offered: the graph is OWL-460.
    assert!(!INDEX_HTML.to_ascii_lowercase().contains("graph"));
}

// ---------------------------------------------------------------------------
// The drain's graph (OWL-460): `/api/graph` and `/api/stream?graph=1`.

/// A served project `o/r` whose ready snapshot lists DEMO-1 and DEMO-2, with
/// a machine pause, and a session on a UI over it.
struct Drain {
    data: tempfile::TempDir,
    ui: Serving,
    session: Session,
}

impl Drain {
    fn new() -> Self {
        let data = tempfile::tempdir().unwrap();
        let repo = Repo::parse("o/r").unwrap();
        let checkout = data.path().join("checkout-of-o-r");
        std::fs::create_dir_all(&checkout).unwrap();
        Registry::in_data_dir(data.path())
            .register(&repo, &checkout)
            .unwrap();
        requests::ask_pause(
            data.path(),
            None,
            &Request::new(BY_PAUSE, Timestamp::now(), "lunch"),
        )
        .unwrap();
        let drain = {
            let ui = serve(data.path(), RunSecret::new().unwrap());
            let session = session(ui.port(), data.path());
            Self { data, ui, session }
        };
        drain.ready("DEMO-1 title", "DEMO-2 title");
        drain
    }

    fn ready(&self, first: &str, second: &str) {
        let ticket = |id: &str, title: &str| ReadyTicket {
            ticket: TicketId::new(id).unwrap(),
            title: title.to_owned(),
            priority: owlshift_core::vocab::Priority::Medium,
            blockers: Vec::new(),
            drain: Outcome::TakenIn,
        };
        let snapshot = ReadySnapshot {
            format: Format,
            at: Timestamp::now(),
            interval_ms: 60_000,
            tickets: vec![ticket("DEMO-1", first), ticket("DEMO-2", second)],
        };
        let dirs = ProjectDirs::new(self.data.path(), &Repo::parse("o/r").unwrap());
        owlshift_runner::ready::write(&dirs, &snapshot).unwrap();
    }

    fn event(&self, ticket: &str, kind: EventKind, data: Value) {
        let event = Event {
            format: Format,
            at: Timestamp::now(),
            project: "o/r".to_owned(),
            ticket: Some(TicketId::new(ticket).unwrap()),
            run: None,
            kind,
            data: data.as_object().unwrap().clone(),
        };
        EventLog::in_dir(self.data.path()).append(&event).unwrap();
    }

    /// The drain tells `ticket` waits blocked by `behind`.
    fn blocked(&self, ticket: &str, behind: &str) {
        self.event(
            ticket,
            EventKind::Decision,
            json!({ "drain": "waits", "reason": "blocked_by", "tickets": [behind] }),
        );
    }

    fn stream(&self) -> BufReader<TcpStream> {
        open_stream_on(self.ui.port(), &self.session, "/api/stream?graph=1")
    }

    fn get(&self, path: &str) -> Reply {
        get(
            self.ui.port(),
            path,
            &[("X-Owlshift-Session", &self.session.header())],
        )
    }

    /// The graph a whole read of the directory gives now: what the tail's
    /// incremental one must equal.
    fn whole(&self) -> Value {
        let mut tail = owlshift_runner::events::EventTail::in_dir(self.data.path());
        let graph = server::graph_read(self.data.path(), &mut tail, 2, Timestamp::now());
        serde_json::to_value(graph).unwrap()
    }
}

/// The next line of `stream` that is a graph, the header lines skipped.
fn next_graph(stream: &mut BufReader<TcpStream>) -> Value {
    loop {
        let line = next_line(stream).expect("a graph line");
        let value: Value = serde_json::from_str(&line).unwrap();
        if value.get("graph").is_some() {
            return value["graph"].clone();
        }
    }
}

fn edges(graph: &Value, layer: &str) -> Vec<(String, String)> {
    graph["projects"][0][layer]["edges"]
        .as_array()
        .unwrap()
        .iter()
        .map(|edge| {
            (
                edge["from"].as_str().unwrap().to_owned(),
                edge["to"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

#[test]
fn the_graph_needs_a_session_and_comes_back_as_escaped_json_text() {
    let drain = Drain::new();
    let none = get(drain.ui.port(), "/api/graph", &[]);
    assert_eq!(none.status, 401);
    none.assert_secured();
    let made_up = get(
        drain.ui.port(),
        "/api/graph",
        &[("X-Owlshift-Session", "nonsense")],
    );
    assert_eq!(made_up.status, 401);
    let title = "<script>alert(1)</script> & x";
    drain.ready("DEMO-1 title", title);
    drain.blocked("DEMO-2", "DEMO-1");
    let reply = drain.get("/api/graph");
    assert_eq!(reply.status, 200, "{}", reply.text());
    reply.assert_secured();
    assert!(
        reply
            .header("Content-Type")
            .unwrap()
            .starts_with("application/json")
    );
    let text = reply.text();
    for c in ['<', '>', '&', '\u{2028}'] {
        assert!(!text.contains(c), "{text}");
    }
    let graph = reply.json();
    assert_eq!(graph["machine_pause"]["why"], "lunch");
    assert_eq!(graph["projects"][0]["repository"], "o/r");
    assert_eq!(graph["projects"][0]["served"], true);
    let blockers = &graph["projects"][0]["blockers"]["nodes"];
    let titles: Vec<&str> = blockers
        .as_array()
        .unwrap()
        .iter()
        .map(|node| node["title"].as_str().unwrap())
        .collect();
    assert_eq!(titles, ["DEMO-1 title", title]);
    assert_eq!(
        edges(&graph, "live"),
        [("DEMO-2".to_owned(), "DEMO-1".to_owned())]
    );
    assert_eq!(graph["projects"][0]["ready"]["stale"], false);
}

#[test]
fn a_graph_stream_follows_the_waits_through_the_event_tail() {
    let drain = Drain::new();
    let mut stream = drain.stream();
    let first: Value = serde_json::from_str(&next_line(&mut stream).unwrap()).unwrap();
    assert!(first.get("header").is_some(), "{first}");
    let graph = next_graph(&mut stream);
    // No ticket ref and no wait: the ready tickets are in the blockers
    // layer only, the live one has nothing to draw.
    assert!(
        graph["projects"][0]["live"]["nodes"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        graph["projects"][0]["blockers"]["nodes"]
            .as_array()
            .unwrap()
            .len(),
        2
    );

    // A wait told, with `ready.json` untouched: the next line has its edge,
    // and nodes and edges are what a whole read of the directory builds.
    drain.blocked("DEMO-2", "DEMO-1");
    let told = next_graph(&mut stream);
    assert_eq!(
        edges(&told, "live"),
        [("DEMO-2".to_owned(), "DEMO-1".to_owned())]
    );
    let whole = drain.whole();
    assert_eq!(told["projects"], whole["projects"]);

    // The drain's dispatch ends the wait: the following line has no edge.
    drain.event(
        "DEMO-2",
        EventKind::Dispatch,
        json!({ "branch": "owlshift/demo-2" }),
    );
    let cleared = next_graph(&mut stream);
    assert!(edges(&cleared, "live").is_empty(), "{cleared}");
    assert_eq!(cleared["projects"], drain.whole()["projects"]);
}

#[test]
fn a_changed_ready_snapshot_alone_gives_a_line_and_an_unchanged_directory_none() {
    let drain = Drain::new();
    let mut stream = drain.stream();
    next_line(&mut stream).unwrap();
    let first = next_graph(&mut stream);
    let title = |graph: &Value| {
        graph["projects"][0]["blockers"]["nodes"][0]["title"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    assert_eq!(title(&first), "DEMO-1 title");
    drain.ready("DEMO-1 renamed", "DEMO-2 title");
    let next = next_graph(&mut stream);
    assert_eq!(title(&next), "DEMO-1 renamed");
    // Nothing changed since: no graph line within many refresh periods
    // (100 ms here), the keepalive being 15 s away.
    stream
        .get_ref()
        .set_read_timeout(Some(Duration::from_millis(600)))
        .unwrap();
    assert_eq!(next_line(&mut stream), None);
}

#[test]
fn a_stream_without_graph_one_never_carries_a_graph_line() {
    let drain = Drain::new();
    for path in ["/api/stream", "/api/stream?graph=2", "/api/stream?graph=0"] {
        let mut stream = open_stream_on(drain.ui.port(), &drain.session, path);
        let first = next_line(&mut stream).unwrap();
        assert!(first.starts_with(r#"{"header":"#), "{path}: {first}");
        drain.blocked("DEMO-2", "DEMO-1");
        stream
            .get_ref()
            .set_read_timeout(Some(Duration::from_millis(500)))
            .unwrap();
        while let Some(line) = next_line(&mut stream) {
            assert!(!line.contains(r#""graph""#), "{path}: {line}");
        }
    }
}

#[test]
fn a_new_token_ends_a_graph_stream_with_the_logged_out_line() {
    let drain = Drain::new();
    let mut stream = drain.stream();
    next_line(&mut stream).unwrap();
    next_graph(&mut stream);
    token::new_token(drain.data.path()).unwrap();
    stream
        .get_ref()
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut last = None;
    while let Some(line) = next_line(&mut stream) {
        last = Some(line);
    }
    assert_eq!(last.as_deref(), Some(LOGGED_OUT_LINE));
}

// The graph's page. JavaScript has no test setup here: these pin its
// structure, and what it draws is checked by a person (see the pull
// request).

#[test]
fn the_graph_is_a_route_of_its_own_next_to_the_ticket_page() {
    let app_js = lf(APP_JS);
    // The route table: the graph, a ticket's page, and what is neither.
    let parse = block(&app_js, "function parseRoute(hash) {", "");
    assert!(parse.contains(r##"if (hash === "#/graph") {"##), "{parse}");
    assert!(parse.contains(r#"return "graph";"#));
    assert!(parse.contains(r##"const prefix = "#/ticket/";"##));
    // It ends the pages open before it draws one, the graph is the page
    // without a route, and a hash change reads the route again.
    let route = block(&app_js, "function route() {", "");
    assert!(
        route.starts_with("\n  closeTicket();\n  closeGraph();"),
        "{route}"
    );
    assert!(route.find("closeGraph();").unwrap() < route.find("openGraph();").unwrap());
    assert!(route.contains("target === null || target === \"graph\""));
    // A logout empties the graph like the header, and forgets it.
    let logged_out = block(&app_js, "function loggedOut(sent) {", "");
    assert!(logged_out.contains("closeGraph();"), "{logged_out}");
    assert!(logged_out.contains("lastGraph = null;"));
    // The stream that carries the header carries the graph only while the
    // graph's page is the route (the server computes it only while a stream
    // asks), and a change of route that changes the answer ends the stream
    // and opens the other, which is no failure and no newer tab.
    let path = block(&app_js, "function streamPath(graph) {", "");
    assert!(
        path.contains(r#"graph ? "/api/stream?graph=1" : "/api/stream""#),
        "{path}"
    );
    assert_eq!(APP_JS.matches(r#""/api/stream?graph=1""#).count(), 1);
    let wants = block(&app_js, "function wantsGraph() {", "");
    assert!(
        wants.contains(r#"target === null || target === "graph""#),
        "{wants}"
    );
    let sync = block(&app_js, "function syncStream() {", "");
    assert!(sync.contains("stream.graph === wantsGraph()"), "{sync}");
    assert!(sync.contains("endStream();") && sync.contains("follow();"));
    assert!(sync.contains("lastGraph = null;"));
    let end = block(&app_js, "function endStream() {", "");
    assert!(end.contains("stream.controller.abort();"), "{end}");
    let follow = block(&app_js, "async function follow() {", "");
    assert!(follow.contains("mine.controller.signal"), "{follow}");
    // A stream the page ended returns before it says a failure or retries.
    let catch = follow.split("} catch (error) {").nth(1).unwrap();
    assert!(
        catch.starts_with("\n    if (stream !== mine) {\n      return;\n    }\n    problem("),
        "{catch}"
    );
    assert!(follow.contains("streamTimer = window.setTimeout(follow, retry);"));
    // A route reads the stream again, and a logout ends it.
    assert!(route.contains("\n  syncStream();"), "{route}");
    assert!(logged_out.contains("endStream();"), "{logged_out}");
    assert!(APP_JS.contains("} else if (message.graph) {\n          lastGraph = message.graph;"));
    // The frame: a link to the graph, and the container the graph draws in.
    assert!(INDEX_HTML.contains(r##"<a href="/#/graph">Graph</a>"##));
    assert!(INDEX_HTML.contains(r#"<section id="view" class="view" hidden></section>"#));
    assert!(INDEX_HTML.contains(r#"<section id="ticket" class="ticket" hidden></section>"#));
}

#[test]
fn the_layer_switch_is_kept_by_the_browser_and_the_project_switch_filters_the_graph() {
    let app_js = lf(APP_JS);
    assert!(app_js.contains(r#"const LAYER_KEY = "owlshift.graph.layer";"#));
    // The layer is read from, and written to, the page's storage helpers.
    let layer = block(&app_js, "function graphLayer() {", "");
    assert!(layer.contains("stored(LAYER_KEY)"), "{layer}");
    assert!(layer.contains(r#""blockers" ? "blockers" : "live""#));
    let open = block(&app_js, "function openGraph() {", "");
    assert!(
        open.contains("store(LAYER_KEY, name);\n      drawGraph();"),
        "{open}"
    );
    assert!(open.contains("\"Live waits\"") && open.contains("\"Blockers\""));
    // The graph draws the projects the header's switch chose, again when it
    // is changed.
    let draw = block(&app_js, "function drawGraph() {", "");
    assert!(
        draw.contains(r#"document.getElementById("project").value"#),
        "{draw}"
    );
    assert!(draw.contains("!chosen || project.repository === chosen"));
    assert!(draw.contains("aria-pressed"));
    let start = block(&app_js, "async function start() {", "");
    assert!(start.contains("store(PROJECT_KEY, project.value || null);"));
    assert!(start.contains("    drawGraph();\n  });"), "{start}");
}

#[test]
fn a_node_opens_its_ticket_and_every_text_of_the_graph_goes_through_text() {
    let app_js = lf(APP_JS);
    // A node is a link to the ticket page's route, its parts encoded, so a
    // click, Enter and a middle click work; only a ticket's own node is one.
    let href = block(&app_js, "function ticketHref(repository, ticket) {", "");
    assert!(
        href.contains(r##"return "#/ticket/" + parts.join("/")"##),
        "{href}"
    );
    assert!(
        href.contains("encodeURIComponent(part)") && href.contains("encodeURIComponent(ticket)")
    );
    let node = block(&app_js, "function drawNode(svg, node, repository) {", "");
    assert!(
        node.contains(r#"svgEl("a", { href: ticketHref(repository, node.id) })"#),
        "{node}"
    );
    assert!(node.contains("node.kind === \"ticket\" && !node.person && TICKET_ID.test(node.id)"));
    // The ticket page reads the route the graph links to: owner, repository
    // and ticket.
    assert!(app_js.contains("parts.length !== 3"));
    // Texts are set as text and colours come from classes: nothing the
    // graph draws is a style attribute or markup.
    assert!(!APP_JS.contains(r#"setAttribute("style""#));
    assert!(!APP_JS.contains(".style."));
    let svg = block(&app_js, "function svgEl(tag, attributes, value) {", "");
    assert!(svg.contains("text(el, value);"), "{svg}");
    // The classes of a node: its state and, for a ghost, its blocker's.
    assert!(app_js.contains(r#"classes.push("node-" + node.state);"#));
    assert!(app_js.contains(r#"classes.push("ghost-" + node.blocker_state);"#));
}

#[test]
fn each_state_the_server_sends_has_its_colour_and_each_ghost_its_own() {
    use owlshift_runner::ui::graph::{BLOCKER_STATES, NODE_STATES};
    for state in NODE_STATES {
        let rule = block(APP_CSS, &format!("\n.node-{state} {{"), "");
        assert!(rule.contains("color: var(--"), "{state}: {rule}");
    }
    for state in BLOCKER_STATES {
        let rule = block(APP_CSS, &format!("\n.ghost-{state} {{"), "");
        assert!(rule.contains("color: var(--"), "{state}: {rule}");
    }
    // The states the ticket names apart do not merge a zone collision with
    // a usage limit: the decider's and the usage limit's share a role, as the
    // ticket groups them, and no other two of those six do.
    let role = |state: &str| {
        block(APP_CSS, &format!("\n.node-{state} {{"), "")
            .split("color: var(--")
            .nth(1)
            .and_then(|rest| rest.split(')').next())
            .unwrap()
            .to_owned()
    };
    let named = [
        "running",
        "resource",
        "ticket",
        "slot",
        "decider",
        "usage_limit",
    ];
    for (i, a) in named.iter().enumerate() {
        for b in &named[i + 1..] {
            let together = [*a, *b] == ["decider", "usage_limit"];
            assert_eq!(role(a) == role(b), together, "{a} and {b}");
        }
    }
    // A ghost is dashed, and a shape takes its colour from its group's.
    let ghost = block(APP_CSS, "\n.node-ghost .node-box {", "");
    assert!(ghost.contains("stroke-dasharray"));
    let shape = block(APP_CSS, "\n.node-box {", "");
    assert!(shape.contains("fill: currentColor;") && shape.contains("stroke: currentColor;"));
    // Both are in the audited sheet: a colour outside the themes, a pair
    // outside the contrast table, would fail `audit`.
    audit(APP_CSS).unwrap();
}

/// What `owlshift` serialises of a graph with a node, an edge, a queue and a
/// blockers layer: the keys a page may read.
fn sample_graph() -> Value {
    use owlshift_contracts::waits::{
        Behind, ProjectWaits, RefStatus, RunSlots, StatusWaits, TicketWaits, Wait,
    };
    use owlshift_runner::ui::graph::{self, ReadyInput};
    let ticket = |name: &str, wait: Option<Wait>| TicketWaits {
        ticket: TicketId::new(name).unwrap(),
        stage: None,
        ref_status: RefStatus::Active,
        holds_capacity: true,
        run: None,
        waits: wait,
    };
    let wait = |reason: &str| Wait {
        reason: reason.to_owned(),
        since: None,
        behind: vec![Behind::Ticket {
            id: TicketId::new("DEMO-1").unwrap(),
            person: false,
        }],
        resource: None,
        harness: None,
        until: None,
        text: reason.to_owned(),
    };
    let waits = StatusWaits {
        format: Format,
        at: Timestamp::now(),
        machine_pause: None,
        runs: Some(RunSlots { in_use: 0, of: 2 }),
        projects: vec![ProjectWaits {
            repository: "o/r".to_owned(),
            served: true,
            max_in_flight: Some(1),
            in_flight: 1,
            paused: None,
            quarantined: false,
            tickets: vec![
                ticket("DEMO-1", None),
                ticket("DEMO-2", Some(wait("blocked_by"))),
                ticket("DEMO-3", Some(wait("slot"))),
            ],
        }],
        notes: Vec::new(),
    };
    let ready = std::collections::BTreeMap::from([(
        "o/r".to_owned(),
        ReadyInput::Read {
            snapshot: ReadySnapshot {
                format: Format,
                at: Timestamp::now(),
                interval_ms: 1000,
                tickets: Vec::new(),
            },
            stale: false,
        },
    )]);
    serde_json::to_value(graph::of(&waits, &ready)).unwrap()
}

/// The names the script reads off `prefix` (`node.`) in `source`.
fn read_keys(source: &str, prefix: &str) -> std::collections::BTreeSet<String> {
    let mut keys = std::collections::BTreeSet::new();
    for (at, _) in source.match_indices(prefix) {
        let before = source[..at].chars().next_back();
        if before.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.') {
            continue;
        }
        let key: String = source[at + prefix.len()..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        if !key.is_empty() {
            keys.insert(key);
        }
    }
    keys
}

/// The page never reads a key the server does not send: a missing one is
/// `undefined`, which the drawing would show as such.
#[test]
fn the_graph_page_reads_only_keys_the_server_sends() {
    let graph = sample_graph();
    let project = &graph["projects"][0];
    let drawing = {
        let start = APP_JS.find("// The drain's graph (OWL-460)").unwrap();
        let end = APP_JS.find("async function start() {").unwrap();
        &APP_JS[start..end]
    };
    let keys = |value: &Value| -> std::collections::BTreeSet<String> {
        value.as_object().unwrap().keys().cloned().collect()
    };
    for (prefix, sent) in [
        ("node.", keys(&project["live"]["nodes"][0])),
        ("project.", keys(project)),
        ("lastGraph.", keys(&graph)),
        ("layer.", keys(&project["live"])),
        ("entry.", keys(&project["queue"][0])),
        ("edge.", keys(&project["live"]["edges"][0])),
        ("project.ready.", keys(&project["ready"])),
    ] {
        for key in read_keys(drawing, prefix) {
            // `entry.since` and the like are read; `node.layer` is a key;
            // methods are not keys.
            if matches!(key.as_str(), "length" | "push" | "slice") {
                continue;
            }
            assert!(sent.contains(&key), "{prefix}{key} is not sent: {sent:?}");
        }
    }
    // The run a node holds, and the machine's pause.
    assert!(project["live"]["nodes"][0].get("run").is_some());
    for key in read_keys(drawing, "node.run.") {
        assert!(["slot", "pid", "since"].contains(&key.as_str()), "{key}");
    }
}
