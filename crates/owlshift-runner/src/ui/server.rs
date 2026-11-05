//! The web UI's server (OWL-467, architecture section 5, "The local web
//! UI"): synchronous, on tiny_http and threads, serving on the `127.0.0.1`
//! socket it is handed, one thread per request.
//!
//! Every request is checked first (`check`): its `Host` must name the
//! loopback (`127.0.0.1`, `localhost` or `[::1]`, any port, so an SSH
//! tunnel's passes), a `POST` must come from the same origin when it says
//! one, carry `application/json` and at most [`MAX_BODY`] bytes, and any
//! method but `GET` and `POST` is refused.
//!
//! The routes:
//!
//! - `GET /`, `/app.js`, `/app.css`: the page's frame and assets, carried in
//!   the binary, served without a session;
//! - `POST /api/proof` `{"challenge"}` → `{"proof"}`, the listener's proof
//!   for `owlshift ui` and `owlshift doctor`, which reveals nothing;
//! - `POST /api/login` `{"code"}` → `{"id", "secret"}`: a one-time code
//!   traded for a session;
//! - `GET /api/header` and `GET /api/stream`, under the session sent in
//!   `X-Owlshift-Session`: the header, once, or as a stream of lines.
//!
//! Every response, errors included, carries [`SECURITY_HEADERS`], and none
//! a CORS header or a cookie. JSON goes out through [`json_text`], which
//! writes `<`, `>`, `&`, U+2028 and U+2029 as `\u` escapes, so no text an
//! agent or a comment wrote reads as markup whatever reads it.
//!
//! A stream writes `{"header":…}` at once and whenever the header changes,
//! `{"keepalive":true}` every keepalive period, and `{"closed":"superseded"}`
//! before it ends when a newer stream took its place: at most
//! [`Options::streams`] run at once, the oldest closed beyond that. One
//! refresher thread reads the header every refresh period while a stream
//! runs; the streams share its latest value.

use std::collections::VecDeque;
use std::convert::Infallible;
use std::fmt;
use std::io::{self, Cursor, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use jiff::Timestamp;
use owlshift_contracts::config::PersonalConfig;
use owlshift_contracts::format::Format;
use owlshift_contracts::service::{BuildIdentity, UiRecord};
use owlshift_core::vocab::Harness;
use serde_json::{Value, json};
use tiny_http::{Header as HttpHeader, Method, Request, Response, Server, StatusCode};

use super::auth::{self, CHALLENGE_LEN, RunSecret, UsedCodes};
use super::{header, record, token};
use crate::config::FileState;
use crate::status;

/// The page's frame.
pub const INDEX_HTML: &str = include_str!("assets/index.html");
/// The page's script.
pub const APP_JS: &str = include_str!("assets/app.js");
/// The page's style sheet.
pub const APP_CSS: &str = include_str!("assets/app.css");

/// The header a session is sent in.
pub const SESSION_HEADER: &str = "X-Owlshift-Session";

/// The most bytes a `POST`'s body may hold.
pub const MAX_BODY: usize = 4096;

/// The headers every response carries.
pub const SECURITY_HEADERS: [(&str, &str); 5] = [
    (
        "Content-Security-Policy",
        "default-src 'self'; frame-ancestors 'none'",
    ),
    ("X-Frame-Options", "DENY"),
    ("X-Content-Type-Options", "nosniff"),
    ("Referrer-Policy", "no-referrer"),
    ("Cache-Control", "no-store"),
];

/// The line a stream writes before it ends when a newer stream took its
/// place, so the page does not take it back.
pub const SUPERSEDED_LINE: &str = r#"{"closed":"superseded"}"#;

/// The figures of the personal file the header needs: the machine's run
/// slots and each harness's usage cap.
type Figures = (u32, Vec<(Harness, u8)>);

/// How the server runs.
#[derive(Clone)]
pub struct Options {
    /// How often the refresher reads the header while a stream runs.
    pub refresh: Duration,
    /// How often a stream writes a keepalive line.
    pub keepalive: Duration,
    /// How many streams run at once at most.
    pub streams: usize,
    /// The personal file as it is now, read again at each read of the
    /// header, so a change to `concurrent_runs` or a cap shows without a
    /// restart: [`super::personal_now`] in production.
    pub personal: Arc<dyn Fn() -> FileState<PersonalConfig> + Send + Sync>,
}

impl Default for Options {
    /// A refresh every 2 s, a keepalive every 15 s, 16 streams, the personal
    /// file where it lives. Each refresh probes the run slots with one
    /// `try_lock` each, as `owlshift status` does; `take`'s retry rounds
    /// absorb a slot held for that moment.
    fn default() -> Self {
        Self {
            refresh: Duration::from_secs(2),
            keepalive: Duration::from_secs(15),
            streams: 16,
            personal: Arc::new(super::personal_now),
        }
    }
}

impl fmt::Debug for Options {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Options")
            .field("refresh", &self.refresh)
            .field("keepalive", &self.keepalive)
            .field("streams", &self.streams)
            .finish_non_exhaustive()
    }
}

/// Why the server does not serve.
#[derive(Debug)]
pub enum ServeError {
    /// The listener it was handed is not on `127.0.0.1`.
    NotLoopback(SocketAddr),
    /// The listener's address, the token or the record failed.
    Io(String, io::Error),
    /// tiny_http refused the listener.
    Server(String),
    /// The loop that takes requests ended.
    Ended,
}

impl fmt::Display for ServeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotLoopback(address) => write!(
                f,
                "the web UI serves on 127.0.0.1 only, and was handed a socket on {address}"
            ),
            Self::Io(what, error) => write!(f, "{what}: {error}"),
            Self::Server(why) => write!(f, "the web UI's server does not start: {why}"),
            Self::Ended => write!(f, "the web UI's server stopped taking requests"),
        }
    }
}

impl std::error::Error for ServeError {}

/// Milliseconds since the Unix epoch, now.
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        })
}

/// What every request's thread shares.
struct Context {
    data: PathBuf,
    run: RunSecret,
    used: Mutex<UsedCodes>,
    options: Options,
    /// The last figures of a personal file that loaded.
    figures: Mutex<Option<Figures>>,
    hub: Hub,
}

/// A serving web UI. Dropping it stops taking requests.
pub struct Serving {
    context: Arc<Context>,
    server: Arc<Server>,
    stopping: Arc<AtomicBool>,
    accept: Option<JoinHandle<()>>,
    port: u16,
}

impl fmt::Debug for Serving {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Serving")
            .field("port", &self.port)
            .finish_non_exhaustive()
    }
}

impl Serving {
    /// The port it serves on.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// How many streams run now.
    pub fn streams(&self) -> usize {
        self.context.hub.live()
    }

    /// Waits for the loop that takes requests, which ends only when the
    /// server is stopped.
    fn wait(mut self) -> ServeError {
        if let Some(accept) = self.accept.take() {
            let _ = accept.join();
        }
        ServeError::Ended
    }
}

impl Drop for Serving {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::SeqCst);
        self.server.unblock();
        if let Some(accept) = self.accept.take() {
            let _ = accept.join();
        }
    }
}

/// Serves on `listener` until the process ends: [`start`], then the loop
/// that takes requests on this thread's watch.
pub fn serve(
    data: &Path,
    listener: TcpListener,
    run: RunSecret,
    build: Option<BuildIdentity>,
    options: Options,
) -> Result<Infallible, ServeError> {
    Err(start(data, listener, run, build, options)?.wait())
}

/// Starts serving on `listener`, which must be on `127.0.0.1`: makes the
/// token's folder and the token when they are missing, writes
/// `<data>/service/ui.json` with this process and the listener's port, and
/// takes requests on a thread of its own.
pub fn start(
    data: &Path,
    listener: TcpListener,
    run: RunSecret,
    build: Option<BuildIdentity>,
    options: Options,
) -> Result<Serving, ServeError> {
    let address = listener
        .local_addr()
        .map_err(|error| ServeError::Io("the socket has no address".into(), error))?;
    if address.ip() != IpAddr::V4(Ipv4Addr::LOCALHOST) {
        return Err(ServeError::NotLoopback(address));
    }
    let dir = token::token_dir(data);
    token::ensure_dir(&dir)
        .map_err(|error| ServeError::Io(format!("{} cannot be made", dir.display()), error))?;
    token::ensure_token(data).map_err(|error| {
        ServeError::Io(
            format!("{} cannot be made", token::token_file(data).display()),
            error,
        )
    })?;
    let started = now_ms();
    let record = UiRecord {
        format: Format,
        pid: std::process::id(),
        port: address.port(),
        since: Timestamp::now(),
        build,
    };
    record::write(data, &record).map_err(|error| {
        ServeError::Io(
            format!("{} cannot be written", record::file(data).display()),
            error,
        )
    })?;
    let server = Arc::new(
        Server::from_listener(listener, None)
            .map_err(|error| ServeError::Server(error.to_string()))?,
    );
    let context = Arc::new(Context {
        data: data.to_owned(),
        run,
        used: Mutex::new(UsedCodes::new(started)),
        options,
        figures: Mutex::new(None),
        hub: Hub::default(),
    });
    let stopping = Arc::new(AtomicBool::new(false));
    let accept = {
        let (server, context, stopping) = (server.clone(), context.clone(), stopping.clone());
        thread::Builder::new()
            .name("ui-accept".into())
            .spawn(move || accept_loop(&server, &context, &stopping))
            .map_err(|error| ServeError::Io("no thread takes requests".into(), error))?
    };
    Ok(Serving {
        context,
        server,
        stopping,
        accept: Some(accept),
        port: address.port(),
    })
}

fn accept_loop(server: &Server, context: &Arc<Context>, stopping: &AtomicBool) {
    loop {
        match server.recv() {
            Ok(request) => {
                let context = context.clone();
                let spawned = thread::Builder::new()
                    .name("ui-request".into())
                    .spawn(move || handle(&context, request));
                if let Err(error) = spawned {
                    eprintln!("A request of the web UI found no thread: {error}");
                }
            }
            Err(_) if stopping.load(Ordering::SeqCst) => return,
            Err(error) => {
                eprintln!("The web UI could not take a connection: {error}");
                thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

/// A response refused before any route: its status and why.
#[derive(Debug, PartialEq, Eq)]
pub struct Refused(pub u16, pub &'static str);

/// The value of `request`'s header `name`, when it has exactly one.
fn header_value<'a>(request: &'a Request, name: &'static str) -> Result<Option<&'a str>, ()> {
    let mut found = request
        .headers()
        .iter()
        .filter(|header| header.field.equiv(name));
    let first = found.next();
    if found.next().is_some() {
        return Err(());
    }
    Ok(first.map(|header| header.value.as_str()))
}

/// Whether `host`, a `Host` header's value, names the loopback: `127.0.0.1`,
/// `localhost` in any letter case or `[::1]`, with an optional port.
pub fn is_loopback_host(host: &str) -> bool {
    let name = match host.rsplit_once(':') {
        Some((name, port)) if !name.ends_with(':') && !port.contains(']') => {
            if port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
                return false;
            }
            name
        }
        _ => host,
    };
    name == "127.0.0.1" || name.eq_ignore_ascii_case("localhost") || name == "[::1]"
}

/// Whether `origin`, an `Origin` header's value, is the origin of `host`.
pub fn is_same_origin(origin: &str, host: &str) -> bool {
    origin
        .strip_prefix("http://")
        .or_else(|| {
            origin
                .get(..7)
                .filter(|scheme| scheme.eq_ignore_ascii_case("http://"))
                .map(|_| &origin[7..])
        })
        .is_some_and(|rest| rest.eq_ignore_ascii_case(host))
}

/// The checks every request passes before its route, and the body of a
/// `POST`.
fn check(request: &mut Request) -> Result<Vec<u8>, Refused> {
    let host = match header_value(request, "Host") {
        Ok(Some(host)) if is_loopback_host(host) => host.to_owned(),
        _ => return Err(Refused(403, "this listener answers 127.0.0.1 only")),
    };
    match request.method() {
        Method::Get => Ok(Vec::new()),
        Method::Post => {
            match header_value(request, "Origin") {
                Ok(None) => {}
                Ok(Some(origin)) if is_same_origin(origin, &host) => {}
                _ => return Err(Refused(403, "a request from another origin")),
            }
            let json = matches!(
                header_value(request, "Content-Type"),
                Ok(Some(kind)) if kind
                    .split(';')
                    .next()
                    .is_some_and(|kind| kind.trim().eq_ignore_ascii_case("application/json"))
            );
            if !json {
                return Err(Refused(415, "a request's body is application/json"));
            }
            if request
                .body_length()
                .is_some_and(|length| length > MAX_BODY)
            {
                return Err(Refused(413, "a request's body is too long"));
            }
            let mut body = Vec::new();
            request
                .as_reader()
                .take(MAX_BODY as u64 + 1)
                .read_to_end(&mut body)
                .map_err(|_| Refused(400, "a request's body does not read"))?;
            if body.len() > MAX_BODY {
                return Err(Refused(413, "a request's body is too long"));
            }
            Ok(body)
        }
        _ => Err(Refused(405, "this listener takes GET and POST only")),
    }
}

/// `value` as JSON text, with `<`, `>`, `&`, U+2028 and U+2029 written as
/// `\u` escapes: those only ever stand inside strings, where the escape
/// reads back as the same text.
pub fn json_text(value: &Value) -> String {
    let text = value.to_string();
    let mut escaped = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '<' => escaped.push_str("\\u003c"),
            '>' => escaped.push_str("\\u003e"),
            '&' => escaped.push_str("\\u0026"),
            '\u{2028}' => escaped.push_str("\\u2028"),
            '\u{2029}' => escaped.push_str("\\u2029"),
            c => escaped.push(c),
        }
    }
    escaped
}

fn http_header(name: &str, value: &str) -> HttpHeader {
    HttpHeader::from_bytes(name.as_bytes(), value.as_bytes()).expect("a valid header")
}

fn with_security<R: Read>(mut response: Response<R>) -> Response<R> {
    for (name, value) in SECURITY_HEADERS {
        response.add_header(http_header(name, value));
    }
    response
}

/// A response of `status` whose body is `value` as [`json_text`].
pub fn json_response(status: u16, value: &Value) -> Response<Cursor<Vec<u8>>> {
    with_security(
        Response::from_data(json_text(value).into_bytes())
            .with_status_code(StatusCode(status))
            .with_header(http_header(
                "Content-Type",
                "application/json; charset=utf-8",
            )),
    )
}

fn error_response(status: u16, why: &str) -> Response<Cursor<Vec<u8>>> {
    json_response(status, &json!({ "error": why }))
}

fn asset(text: &'static str, kind: &str) -> Response<Cursor<Vec<u8>>> {
    with_security(
        Response::from_data(text.as_bytes().to_vec())
            .with_header(http_header("Content-Type", kind)),
    )
}

fn handle(context: &Arc<Context>, mut request: Request) {
    let body = match check(&mut request) {
        Ok(body) => body,
        Err(Refused(status, why)) => {
            let _ = request.respond(error_response(status, why));
            return;
        }
    };
    let path = request.url().split('?').next().unwrap_or("").to_owned();
    let response = match (request.method(), path.as_str()) {
        (Method::Get, "/" | "/index.html") => asset(INDEX_HTML, "text/html; charset=utf-8"),
        (Method::Get, "/app.js") => asset(APP_JS, "text/javascript; charset=utf-8"),
        (Method::Get, "/app.css") => asset(APP_CSS, "text/css; charset=utf-8"),
        (Method::Post, "/api/proof") => proof(context, &body),
        (Method::Post, "/api/login") => login(context, &body),
        (Method::Get, "/api/header" | "/api/stream") => {
            if !in_session(context, &request) {
                error_response(401, "not logged in: run `owlshift ui` in a terminal")
            } else if path == "/api/header" {
                json_response(200, header_now(context).json())
            } else {
                stream(context, request);
                return;
            }
        }
        _ => error_response(404, "no such page"),
    };
    let _ = request.respond(response);
}

/// The field `name` of the JSON object `body`, as text.
fn field(body: &[u8], name: &str) -> Option<String> {
    let value: Value = serde_json::from_slice(body).ok()?;
    value.get(name)?.as_str().map(str::to_owned)
}

fn proof(context: &Context, body: &[u8]) -> Response<Cursor<Vec<u8>>> {
    let Some(challenge) = field(body, "challenge")
        .and_then(|challenge| auth::from_hex_array::<CHALLENGE_LEN>(&challenge))
    else {
        return error_response(400, "a challenge is 32 bytes in hexadecimal");
    };
    match token::read_token(&context.data) {
        Ok(token) => json_response(200, &json!({ "proof": auth::prove(&token, &challenge) })),
        Err(_) => error_response(503, "the web UI's token cannot be read"),
    }
}

fn login(context: &Context, body: &[u8]) -> Response<Cursor<Vec<u8>>> {
    let Some(code) = field(body, "code") else {
        return error_response(400, "a login sends its code");
    };
    let Ok(token) = token::read_token(&context.data) else {
        return error_response(401, "the code is not valid: run `owlshift ui` again");
    };
    let taken = context
        .used
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take(&token, &code, now_ms());
    if taken.is_err() {
        return error_response(401, "the code is not valid: run `owlshift ui` again");
    }
    match auth::mint_session(&token, &context.run) {
        Ok(session) => json_response(200, &json!({ "id": session.id, "secret": session.secret })),
        Err(_) => error_response(503, "no session could be made"),
    }
}

/// Whether `request` carries a session under the token, read again, and
/// this daemon's run.
fn in_session(context: &Context, request: &Request) -> bool {
    let Ok(Some(value)) = header_value(request, SESSION_HEADER) else {
        return false;
    };
    token::read_token(&context.data)
        .is_ok_and(|token| auth::check_session(&token, &context.run, value))
}

/// The header now, read from the files `owlshift status` reads, with the
/// personal file read again; when it does not load, the last figures of
/// one that did, and a note saying so.
fn header_now(context: &Context) -> header::Header {
    let personal = (context.options.personal)();
    let mut note = None;
    let figures = {
        let mut last = context
            .figures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match &personal {
            FileState::Invalid { path, error } => {
                note = Some(format!(
                    "{} does not load ({error}): the run slots and the usage caps are its last \
                     figures that did",
                    path.display()
                ));
                last.clone()
                    .unwrap_or_else(|| (crate::usage::concurrent_runs(&personal), Vec::new()))
            }
            _ => {
                let figures = (
                    crate::usage::concurrent_runs(&personal),
                    crate::usage::caps_of(&personal),
                );
                *last = Some(figures.clone());
                figures
            }
        }
    };
    let report = status::gather_files(&context.data, figures.0, &figures.1, Timestamp::now());
    let mut header = header::of(&report);
    if let (Some(note), Some(notes)) = (
        note,
        header.0.get_mut("notes").and_then(Value::as_array_mut),
    ) {
        notes.push(Value::String(note));
    }
    header
}

/// The streams that run and the refresher's latest header.
#[derive(Default)]
struct Hub {
    state: Mutex<HubState>,
    changed: Condvar,
}

#[derive(Default)]
struct HubState {
    next: u64,
    streams: VecDeque<(u64, Arc<AtomicBool>)>,
    generation: u64,
    latest: Option<String>,
    refreshing: bool,
}

impl Hub {
    fn lock(&self) -> std::sync::MutexGuard<'_, HubState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn live(&self) -> usize {
        self.lock().streams.len()
    }
}

/// Admits a stream, superseding the oldest beyond the cap, and starts the
/// refresher when none runs: the stream's id and its superseded flag.
fn admit(context: &Arc<Context>) -> (u64, Arc<AtomicBool>) {
    let mut state = context.hub.lock();
    while state.streams.len() >= context.options.streams.max(1) {
        if let Some((_, oldest)) = state.streams.pop_front() {
            oldest.store(true, Ordering::SeqCst);
        }
    }
    let id = state.next;
    state.next += 1;
    let superseded = Arc::new(AtomicBool::new(false));
    state.streams.push_back((id, superseded.clone()));
    if !state.refreshing {
        state.refreshing = true;
        let context = context.clone();
        let spawned = thread::Builder::new()
            .name("ui-refresh".into())
            .spawn(move || refresh(&context));
        if spawned.is_err() {
            state.refreshing = false;
        }
    }
    drop(state);
    context.hub.changed.notify_all();
    (id, superseded)
}

fn leave(context: &Context, id: u64) {
    context.hub.lock().streams.retain(|(other, _)| *other != id);
}

/// Reads the header every refresh period while a stream runs, and shares
/// it with the streams.
fn refresh(context: &Arc<Context>) {
    loop {
        let line = json_text(&json!({ "header": header_now(context).json() }));
        let mut state = context.hub.lock();
        if state.streams.is_empty() {
            state.refreshing = false;
            return;
        }
        if state.latest.as_deref() != Some(line.as_str()) {
            state.latest = Some(line);
            state.generation += 1;
            context.hub.changed.notify_all();
        }
        let (state, _) = context
            .hub
            .changed
            .wait_timeout(state, context.options.refresh)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        drop(state);
    }
}

/// Writes `line` as one chunk of a chunked body, and flushes it.
fn chunk(writer: &mut dyn Write, line: &str) -> io::Result<()> {
    write!(writer, "{:x}\r\n{line}\n\r\n", line.len() + 1)?;
    writer.flush()
}

/// Serves `/api/stream`: its own status line and headers, then one chunk
/// per line until the page goes, or a newer stream supersedes it.
fn stream(context: &Arc<Context>, request: Request) {
    let (id, superseded) = admit(context);
    let mut writer = request.into_writer();
    let _ = write_stream(context, &mut *writer, &superseded);
    leave(context, id);
}

fn write_stream(
    context: &Context,
    writer: &mut dyn Write,
    superseded: &AtomicBool,
) -> io::Result<()> {
    let mut head = String::from(
        "HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson; charset=utf-8\r\n\
         Transfer-Encoding: chunked\r\nConnection: close\r\n",
    );
    for (name, value) in SECURITY_HEADERS {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    writer.write_all(head.as_bytes())?;
    let mut written = json_text(&json!({ "header": header_now(context).json() }));
    chunk(writer, &written)?;
    let mut seen = context.hub.lock().generation;
    let mut kept = Instant::now();
    loop {
        if superseded.load(Ordering::SeqCst) {
            chunk(writer, SUPERSEDED_LINE)?;
            break;
        }
        let latest = {
            let state = context.hub.lock();
            let (state, _) = context
                .hub
                .changed
                .wait_timeout(state, Duration::from_millis(250))
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if state.generation == seen {
                None
            } else {
                seen = state.generation;
                state.latest.clone()
            }
        };
        if superseded.load(Ordering::SeqCst) {
            continue;
        }
        if let Some(latest) = latest.filter(|latest| *latest != written) {
            chunk(writer, &latest)?;
            written = latest;
            kept = Instant::now();
        } else if kept.elapsed() >= context.options.keepalive {
            chunk(writer, r#"{"keepalive":true}"#)?;
            kept = Instant::now();
        }
    }
    writer.write_all(b"0\r\n\r\n")?;
    writer.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_loopback_host_passes_with_any_port() {
        for host in [
            "127.0.0.1",
            "127.0.0.1:18466",
            "127.0.0.1:18566",
            "localhost:18466",
            "LOCALHOST:1",
            "[::1]:18466",
            "[::1]",
        ] {
            assert!(is_loopback_host(host), "{host}");
        }
        for host in [
            "evil.example:18466",
            "127.0.0.1.evil.example",
            "127.0.0.1:",
            "127.0.0.1:x",
            "localhost.:1",
            "[::2]:1",
            "",
        ] {
            assert!(!is_loopback_host(host), "{host}");
        }
    }

    #[test]
    fn an_origin_is_the_same_when_it_is_the_hosts() {
        assert!(is_same_origin("http://127.0.0.1:18466", "127.0.0.1:18466"));
        assert!(is_same_origin("HTTP://LOCALHOST:1", "localhost:1"));
        assert!(!is_same_origin("http://127.0.0.1:9999", "127.0.0.1:18466"));
        assert!(!is_same_origin("null", "127.0.0.1:18466"));
        assert!(!is_same_origin(
            "https://127.0.0.1:18466",
            "127.0.0.1:18466"
        ));
    }

    #[test]
    fn json_text_escapes_markup_and_reads_back() {
        let line = "<script>alert(1)</script><b>x</b> & \u{2028}\u{2029}";
        let text = json_text(&json!({ "line": line }));
        for c in ['<', '>', '&', '\u{2028}', '\u{2029}'] {
            assert!(!text.contains(c), "{text}");
        }
        let back: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(back["line"], line);
    }
}
