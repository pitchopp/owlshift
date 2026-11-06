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
    self, APP_CSS, APP_JS, INDEX_HTML, LOGGED_OUT_LINE, Options, SECURITY_HEADERS, SUPERSEDED_LINE,
    ServeError, Serving,
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
    assert!(APP_JS.contains("return current.id + \".\" + current.secret;"));
    assert!(APP_JS.contains("const sent = sessionHeader(current);"));
    assert!(APP_JS.contains("headers[SESSION_HEADER] = sent;"));
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
    for (name, text) in [
        ("index.html", INDEX_HTML),
        ("app.js", APP_JS),
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
