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
use owlshift_contracts::service::{BY_PAUSE, Request};
use owlshift_runner::daemon::requests;
use owlshift_runner::ui::auth::{self, RunSecret, Session};
use owlshift_runner::ui::server::{
    self, APP_CSS, APP_JS, INDEX_HTML, LOGGED_OUT_LINE, Options, SUPERSEDED_LINE, ServeError,
    Serving,
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
        ("/app.css", "text/css", APP_CSS),
    ] {
        let reply = get(ui.port(), path, &[]);
        assert_eq!(reply.status, 200, "{path}");
        reply.assert_secured();
        assert!(reply.header("Content-Type").unwrap().starts_with(kind));
        assert_eq!(reply.text(), body);
    }
    assert!(INDEX_HTML.contains(r#"<script src="/app.js" defer>"#));
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
    let stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    (&stream)
        .write_all(&request_bytes(
            port,
            "GET",
            "/api/stream",
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
    for path in ["/", "/app.js", "/app.css"] {
        assert!(!get(ui.port(), path, &[]).text().contains(&token), "{path}");
    }
    assert!(!APP_JS.to_ascii_lowercase().contains("token"));
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
    assert_eq!(
        authed,
        ["authedFetch(path", "authedFetch(\"/api/stream\""],
        "{authed:?}"
    );
    assert!(APP_JS.contains("body: JSON.stringify({ code: match[1] })"));
    assert!(APP_JS.contains("headers[SESSION_HEADER] = current.id + \".\" + current.secret"));
    assert_eq!(APP_JS.matches("body:").count(), 1);
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
    }
    assert!(APP_JS.contains("el.textContent ="));
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
    for (state, symbol) in [
        ("success", r#""\2713""#),
        ("warning", r#""!""#),
        ("danger", r#""\2717""#),
        ("information", r#""\00B7""#),
    ] {
        // Its own rule, a line of its own after a blank line, not the
        // grouped one that sets the symbols' spacing.
        let rule = block(APP_CSS, &format!("\n\n.state-{state}::before {{"), "");
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
    let logged_out = block(APP_JS, "function loggedOut() {", "");
    assert!(
        logged_out.contains(r#"clear(document.getElementById("header"));"#),
        "{logged_out}"
    );
    // A stream the session no longer holds logs the page out.
    assert!(APP_JS.contains(r#"message.closed === "logged_out""#));
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
    let next: Value = serde_json::from_str(&next_line(&mut stream).unwrap()).unwrap();
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
