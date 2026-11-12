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
//! - `GET /`, `/app.js`, `/ops.js`, `/app.css`, `/favicon.svg` and `/fonts/<name>.woff2`:
//!   the page's frame and assets, carried in the binary, served without a
//!   session;
//! - `POST /api/proof` `{"challenge"}` → `{"proof"}`, the listener's proof
//!   for `owlshift ui` and `owlshift doctor`, which reveals nothing;
//! - `POST /api/login` `{"code"}` → `{"id", "secret"}`: a one-time code
//!   traded for a session;
//! - `GET /api/header` and `GET /api/stream`, under the session sent in
//!   `X-Owlshift-Session`: the header, once, or as a stream of lines;
//! - `GET /api/overview` and `GET /api/since[?at=<RFC 3339>]` (OWL-470), under
//!   the session as well: the overview ([`super::overview`]) and what
//!   happened after `at` (the last 24 hours when the request has none), read
//!   from the event log's tail ([`super::eventtail`]);
//! - `GET /api/ticket?project=owner/repo&ticket=OWL-1` and
//!   `GET /api/ticket/log?project&ticket&run&file=stdout|stderr&from=N&line=L`
//!   (OWL-471), under the session as well: a ticket's snapshot
//!   ([`super::ticket`]) and the records of a run's log from a byte offset
//!   ([`super::tail`], [`super::runlog`]). The query is split on `&` and the
//!   first `=` and percent-decoded, a repeated key refused; the project must
//!   be one the served registry lists, the ticket a ticket id, the run a
//!   run folder's name and the file one of two literals, so no text of the
//!   query becomes a path, and a run folder or a log that is a link is not
//!   followed.
//!
//! - `GET /api/orchestrator[?gen=G&after=O][&log=O]` (OWL-472), under the
//!   session as well: the daemon, the watches, the usage windows, the
//!   decisions and warnings after a cursor and the daemon's log after an
//!   offset ([`super::orchestrator`]), read from the event log's index
//!   ([`super::index`]).
//!
//! - `GET /api/config[?rev=<hash>]`, `GET /api/spend[?days=7|30|90|all]
//!   [&project=owner/repo][&rev=<hash>]` and
//!   `POST /api/doctor` (OWL-472), under the session as well: the cost per
//!   day, ticket, role and harness against the budgets and usage caps
//!   ([`super::spend`]), the effective configuration ([`super::configuration`])
//!   and `owlshift doctor`'s checks run on demand.
//!
//! - `GET /api/graph` (OWL-460), under the session: the drain's wait graph
//!   ([`super::graph`]), built by [`graph_read`] from the waits of
//!   `owlshift status --waits` over the overview's tail of the event log
//!   ([`super::eventtail`]) and each served project's ready snapshot. `GET
//!   /api/stream?graph=1` adds `{"graph":…}` lines to the stream: one at
//!   once, and one whenever the graph changes with its `at` (the time of a
//!   read) left out. Any other value of `graph` is ignored.
//! - `POST /api/reply` `{"project", "ticket", "ask", "answers", "go"}`
//!   (OWL-475), under the session as well: the decider's reply to the open
//!   ask of a ticket, composed and posted through the operator's tracker key
//!   ([`super::reply`]), only when the key's account is that ask's decider.
//!   One reply at a time per ticket (409), and none to a round that changed
//!   since the page read it (409).
//! - `POST /api/pause`, `/api/resume`, `/api/forget`, `/api/cancel` and
//!   `/api/restart` (OWL-473), under the session as well: the operator's
//!   actions, each through its command's own code and locks, recorded with
//!   `by` = `ui`; and `POST /api/config/personal` `{"path", "value", "rev"}`,
//!   one key of the personal file edited ([`crate::personal_edit`]), the
//!   edit recorded as an event. Starting or stopping the service and
//!   `owlshift unquarantine` have no route: they stay in the terminal.
//!
//! Every response, errors included, carries [`SECURITY_HEADERS`], and none
//! a CORS header or a cookie. JSON goes out through [`json_text`], which
//! writes `<`, `>`, `&`, U+2028 and U+2029 as `\u` escapes, so no text an
//! agent or a comment wrote reads as markup whatever reads it.
//!
//! A stream writes `{"header":…}` at once and whenever the header changes,
//! `{"overview":…}` after it, on the overview's own slower period
//! ([`Options::overview_refresh`]) and when it changes,
//! `{"keepalive":true}` every keepalive period, and `{"closed":"superseded"}`
//! before it ends when a newer stream took its place: at most
//! [`Options::streams`] run at once, the oldest closed beyond that. It
//! checks its session again, under the token read again, before each line
//! and at least every refresh period, and ends with `{"closed":"logged_out"}`
//! once the session no longer holds, so a new token ends a stream too. One
//! refresher thread reads the header every refresh period while a stream
//! runs; the streams share its latest value.

use std::collections::{BTreeMap, HashSet, VecDeque};
use std::convert::Infallible;
use std::fmt;
use std::io::{self, Cursor, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use jiff::Timestamp;
use owlshift_adapters::forge::Repo;
use owlshift_contracts::config::PersonalConfig;
use owlshift_contracts::event::EventKind;
use owlshift_contracts::format::Format;
use owlshift_contracts::ids::TicketId;
use owlshift_contracts::service::{BY_UI, BuildIdentity, Request as Asked, UiRecord};
use owlshift_core::vocab::Harness;
use serde_json::{Value, json};
use tiny_http::{Header as HttpHeader, Method, Request, Response, Server, StatusCode};

use super::auth::{self, CHALLENGE_LEN, RunSecret, UsedCodes};
use super::eventtail::EventTail;
use super::graph::{self, Graph, ReadyInput};
use super::index::EventIndex;
use super::reply::{self, KeyOpener, KeyState, Keys};
use super::{
    configuration, events, header, orchestrator, overview, record, runlog, spend, tail, ticket,
    token,
};
use crate::config::FileState;
use crate::events::{EventLog, EventSink};
use crate::project::ProjectDirs;
use crate::served::{self, Registry};
use crate::status::{self, waits};
use crate::system::{HostSystem, System};
use crate::voice::Voice;
use crate::{cancel, controls, on_demand, personal_edit};

/// The page's frame.
pub const INDEX_HTML: &str = include_str!("assets/index.html");
/// The page's script.
pub const APP_JS: &str = include_str!("assets/app.js");
/// The operations screens' script (OWL-472).
pub const OPS_JS: &str = include_str!("assets/ops.js");
/// The page's style sheet.
pub const APP_CSS: &str = include_str!("assets/app.css");
/// The Bricolage Grotesque face, Latin, a variable font (visual identity
/// section 5), carried in the binary.
pub const BRICOLAGE_WOFF2: &[u8] = include_bytes!("assets/fonts/BricolageGrotesque-latin.woff2");
/// The JetBrains Mono Regular face, Latin.
pub const MONO_REGULAR_WOFF2: &[u8] =
    include_bytes!("assets/fonts/JetBrainsMono-Regular-latin.woff2");
/// The JetBrains Mono Medium face, Latin.
pub const MONO_MEDIUM_WOFF2: &[u8] =
    include_bytes!("assets/fonts/JetBrainsMono-Medium-latin.woff2");
/// The mark, the page's favicon: the brand's own file, not a copy.
pub const FAVICON_SVG: &str = include_str!("../../../../assets/brand/owlshift-mark.svg");

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

/// The line a stream writes before it ends when its session no longer
/// holds, a new token having ended it, so the page shows it logged out.
pub const LOGGED_OUT_LINE: &str = r#"{"closed":"logged_out"}"#;

/// The figures of the personal file the header needs: the machine's run
/// slots and each harness's usage cap.
type Figures = (u32, Vec<(Harness, u8)>);

/// How the server runs.
#[derive(Clone)]
pub struct Options {
    /// How often the refresher reads the header while a stream runs, and
    /// how long a stream goes at most without checking its session again.
    pub refresh: Duration,
    /// How often the refresher builds the overview while a stream runs: a
    /// build reads every project's ticket refs (five git commands each), so it
    /// is slower than the header (OWL-470).
    pub overview_refresh: Duration,
    /// How often a stream writes a keepalive line.
    pub keepalive: Duration,
    /// How many streams run at once at most.
    pub streams: usize,
    /// The personal file as it is now, read again at each read of the
    /// header, so a change to `concurrent_runs` or a cap shows without a
    /// restart: [`super::personal_now`] in production.
    pub personal: Arc<dyn Fn() -> FileState<PersonalConfig> + Send + Sync>,
    /// The host `owlshift doctor`'s checks probe when the Orchestrator runs
    /// them on demand (OWL-472): [`HostSystem`] in production; the bench
    /// passes a scripted one that reaches nothing of the machine.
    pub system: Arc<dyn System + Send + Sync>,
    /// Opens a served project's tracker key and reads its account, once per
    /// project (OWL-475): [`reply::open_key`] for `owlshift ui --daemon`. The
    /// default opens none, so a bench or a test never reads the system
    /// keychain: the page then names the decider and offers no form.
    pub keys: KeyOpener,
    /// Restarts a served project's parked ticket (OWL-473): [`spawn_restart`],
    /// which runs `owlshift continue --to-watch` in a child of this binary;
    /// a test passes a scripted one.
    pub restart: Restarter,
}

/// The web UI's Restart of a served project's ticket (OWL-473).
pub type Restarter = Arc<dyn Fn(&Repo, &TicketId) -> Restarted + Send + Sync>;

/// What a restart from the web UI came to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Restarted {
    /// The command ended in success: what it printed.
    Done(Vec<String>),
    /// The command refused or failed: why, and what it printed.
    Refused(String, Vec<String>),
    /// The command still ran when the wait ended: it goes on, never killed.
    GoesOn,
    /// The command did not start.
    NotStarted(String),
}

/// How long the restart route waits for its command before it answers that
/// the restart goes on.
pub const RESTART_WAIT: Duration = Duration::from_secs(120);

/// The command of the web UI's Restart of `ticket` on the served project
/// `repo`, `program` being this binary: `owlshift continue <TICKET>
/// --to-watch --served <OWNER/REPO> --from-ui`, its input closed and its
/// outputs captured. Its code, its ticket lock, its tracker credentials and
/// its events stay in that process: the UI never opens the keychain.
pub fn restart_command(program: &Path, repo: &Repo, ticket: &TicketId) -> Command {
    let mut command = Command::new(program);
    command
        .args(["continue", ticket.as_str(), "--to-watch", "--served"])
        .arg(repo.to_string())
        .arg("--from-ui")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

/// Runs `command` and waits `wait` at most: [`Restarted::Done`] with the
/// lines it printed when it ends in success; [`Restarted::Refused`] with
/// why and those lines otherwise, why being the last line of its error
/// output that is not a warning (its `owlshift: ` dropped), else the last
/// line of its output, where `continue` prints a refusal of its own; and
/// [`Restarted::GoesOn`] when it still runs, a thread of its own then
/// waiting for its end, so it is never killed between the stage it shows
/// and the ref it writes.
pub fn run_restart(mut command: Command, wait: Duration) -> Restarted {
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => return Restarted::NotStarted(format!("the restart did not start: {error}")),
    };
    fn reader<R: Read + Send + 'static>(pipe: Option<R>) -> JoinHandle<String> {
        thread::spawn(move || {
            let mut bytes = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut bytes);
            }
            String::from_utf8_lossy(&bytes).into_owned()
        })
    }
    let out = reader(child.stdout.take());
    let err = reader(child.stderr.take());
    let deadline = Instant::now() + wait;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(50)),
            other => {
                let failed = other.err();
                let _ = thread::Builder::new()
                    .name("ui-restart".into())
                    .spawn(move || {
                        let _ = child.wait();
                        let _ = (out.join(), err.join());
                    });
                return match failed {
                    None => Restarted::GoesOn,
                    Some(error) => Restarted::NotStarted(format!(
                        "the restart's end could not be read: {error}"
                    )),
                };
            }
        }
    };
    let stdout = out.join().unwrap_or_default();
    let stderr = err.join().unwrap_or_default();
    let said = |text: &str| -> Vec<String> {
        text.lines()
            .map(str::trim_end)
            .filter(|line| !line.trim().is_empty())
            .map(crate::events::printable)
            .collect()
    };
    let lines: Vec<String> = said(&stdout).into_iter().chain(said(&stderr)).collect();
    if status.success() {
        return Restarted::Done(lines);
    }
    let refusal = said(&stderr)
        .into_iter()
        .rev()
        .find(|line| !line.starts_with("owlshift: warning:"))
        .map(|line| line.strip_prefix("owlshift: ").unwrap_or(&line).to_owned())
        .or_else(|| said(&stdout).pop())
        .unwrap_or_else(|| match status.code() {
            Some(code) => format!("the restart ended with exit status {code}"),
            None => "the restart was ended by a signal".to_owned(),
        });
    Restarted::Refused(refusal, lines)
}

/// The restart of [`Options::default`]: [`restart_command`] on this binary,
/// waited for [`RESTART_WAIT`] at most.
pub fn spawn_restart(repo: &Repo, ticket: &TicketId) -> Restarted {
    match owlshift_platform::process::own_program() {
        Ok(own) => run_restart(restart_command(&own.spawn, repo, ticket), RESTART_WAIT),
        Err(error) => Restarted::NotStarted(format!(
            "this program's own path, which runs the restart: {error}"
        )),
    }
}

/// The opener of [`Options::default`]: no key.
fn no_key(_: &Path, _: &Repo) -> Result<reply::Key, String> {
    Err("this web UI was started without the tracker keys".into())
}

impl Default for Options {
    /// A refresh every 2 s, a keepalive every 15 s, 16 streams, the personal
    /// file where it lives. Each refresh probes the run slots with one
    /// `try_lock` each, as `owlshift status` does; `take`'s retry rounds
    /// absorb a slot held for that moment.
    fn default() -> Self {
        Self {
            refresh: Duration::from_secs(2),
            overview_refresh: Duration::from_secs(5),
            keepalive: Duration::from_secs(15),
            streams: 16,
            personal: Arc::new(super::personal_now),
            system: Arc::new(HostSystem),
            keys: Arc::new(no_key),
            restart: Arc::new(spawn_restart),
        }
    }
}

impl fmt::Debug for Options {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Options")
            .field("refresh", &self.refresh)
            .field("overview_refresh", &self.overview_refresh)
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
    /// The event log's incremental reader (OWL-471): what the ticket page's
    /// snapshot and its run logs' `ended` read.
    events: events::Reader,
    /// What a ticket's snapshot keeps between two builds.
    tickets: ticket::Cache,
    /// The event log's tail for the overview (OWL-470) and the drain's graph
    /// (OWL-460): held while either is built, so the two share one offset and
    /// one copy of the log's folds.
    tail: Mutex<EventTail>,
    /// The event log indexed for the Orchestrator and Spend screens
    /// (OWL-472): held while a request brings it up to date.
    index: Mutex<EventIndex>,
    /// The build this server runs, which `doctor` compares the running
    /// builds with; `None` answers the doctor route 503.
    build: Option<BuildIdentity>,
    /// Held while a doctor run is under way: one at a time.
    doctor: Mutex<()>,
    /// What the Spend screen keeps between two requests.
    spend: spend::Cache,
    /// The served projects' tracker keys, each read once (OWL-475).
    keys: Arc<Keys>,
    /// The tickets a reply is being posted to, by lower-cased project and
    /// ticket: one at a time each.
    replying: Mutex<HashSet<(String, String)>>,
    /// Held while an edit of the personal file is under way (OWL-473): one
    /// at a time.
    personal_edit: Mutex<()>,
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
        build: build.clone(),
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
    let options_keys = options.keys.clone();
    let context = Arc::new(Context {
        data: data.to_owned(),
        run,
        used: Mutex::new(UsedCodes::new(started)),
        options,
        figures: Mutex::new(None),
        hub: Hub::default(),
        events: events::Reader::new(data),
        tickets: ticket::Cache::new(ticket::WAITS_KEPT),
        tail: Mutex::new(EventTail::new(data)),
        index: Mutex::new(EventIndex::new(data)),
        build,
        doctor: Mutex::new(()),
        spend: spend::Cache::default(),
        keys: Keys::new(options_keys),
        replying: Mutex::new(HashSet::new()),
        personal_edit: Mutex::new(()),
    });
    // The keys of the projects served now, read once on a thread of their
    // own; a project served later is read at its first ticket page.
    let served: Vec<Repo> = Registry::in_data_dir(data)
        .read()
        .map(|served| {
            served
                .projects
                .iter()
                .filter_map(|project| Repo::parse(&project.repository).ok())
                .collect()
        })
        .unwrap_or_default();
    context.keys.read_all(data, served);
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

fn asset(bytes: &'static [u8], kind: &str) -> Response<Cursor<Vec<u8>>> {
    with_security(
        Response::from_data(bytes.to_vec()).with_header(http_header("Content-Type", kind)),
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
    let url = request.url().to_owned();
    let path = url.split('?').next().unwrap_or("").to_owned();
    let response = match (request.method(), path.as_str()) {
        (Method::Get, "/" | "/index.html") => {
            asset(INDEX_HTML.as_bytes(), "text/html; charset=utf-8")
        }
        (Method::Get, "/app.js") => asset(APP_JS.as_bytes(), "text/javascript; charset=utf-8"),
        (Method::Get, "/ops.js") => asset(OPS_JS.as_bytes(), "text/javascript; charset=utf-8"),
        (Method::Get, "/app.css") => asset(APP_CSS.as_bytes(), "text/css; charset=utf-8"),
        (Method::Get, "/favicon.svg") => asset(FAVICON_SVG.as_bytes(), "image/svg+xml"),
        (Method::Get, "/fonts/BricolageGrotesque-latin.woff2") => {
            asset(BRICOLAGE_WOFF2, "font/woff2")
        }
        (Method::Get, "/fonts/JetBrainsMono-Regular-latin.woff2") => {
            asset(MONO_REGULAR_WOFF2, "font/woff2")
        }
        (Method::Get, "/fonts/JetBrainsMono-Medium-latin.woff2") => {
            asset(MONO_MEDIUM_WOFF2, "font/woff2")
        }
        (Method::Post, "/api/proof") => proof(context, &body),
        (Method::Post, "/api/login") => login(context, &body),
        (Method::Get, "/api/header" | "/api/stream") => {
            if !in_session(context, &request) {
                error_response(401, "not logged in: run `owlshift ui` in a terminal")
            } else if path == "/api/header" {
                json_response(200, header_now(context).json())
            } else {
                let graph = query_of(&url).is_ok_and(|pairs| param(&pairs, "graph") == Some("1"));
                stream(context, request, graph);
                return;
            }
        }
        (Method::Get, "/api/overview" | "/api/since") => {
            if !in_session(context, &request) {
                error_response(401, "not logged in: run `owlshift ui` in a terminal")
            } else if path == "/api/overview" {
                json_response(200, &overview_now(context).json())
            } else {
                since_page(context, &url)
            }
        }
        (Method::Get, "/api/graph") => {
            if !in_session(context, &request) {
                error_response(401, "not logged in: run `owlshift ui` in a terminal")
            } else {
                json_response(
                    200,
                    &serde_json::to_value(graph_now(context)).unwrap_or(Value::Null),
                )
            }
        }
        (Method::Post, "/api/reply") => {
            if !in_session(context, &request) {
                error_response(401, "not logged in: run `owlshift ui` in a terminal")
            } else {
                reply_page(context, &body)
            }
        }
        (Method::Get, "/api/ticket" | "/api/ticket/log") => {
            if !in_session(context, &request) {
                error_response(401, "not logged in: run `owlshift ui` in a terminal")
            } else if path == "/api/ticket" {
                ticket_page(context, &url)
            } else {
                ticket_log(context, &url)
            }
        }
        (Method::Post, "/api/doctor") => {
            if !in_session(context, &request) {
                error_response(401, "not logged in: run `owlshift ui` in a terminal")
            } else {
                doctor_page(context, &body)
            }
        }
        (Method::Get, "/api/config") => {
            if !in_session(context, &request) {
                error_response(401, "not logged in: run `owlshift ui` in a terminal")
            } else {
                config_page(context, &url)
            }
        }
        (Method::Post, "/api/config/personal") => {
            if !in_session(context, &request) {
                error_response(401, "not logged in: run `owlshift ui` in a terminal")
            } else {
                personal_edit_page(context, &body)
            }
        }
        (Method::Get, "/api/spend") => {
            if !in_session(context, &request) {
                error_response(401, "not logged in: run `owlshift ui` in a terminal")
            } else {
                spend_page(context, &url)
            }
        }
        (Method::Get, "/api/orchestrator") => {
            if !in_session(context, &request) {
                error_response(401, "not logged in: run `owlshift ui` in a terminal")
            } else {
                orchestrator_page(context, &url)
            }
        }
        (
            Method::Post,
            "/api/pause" | "/api/resume" | "/api/forget" | "/api/cancel" | "/api/restart",
        ) => {
            if !in_session(context, &request) {
                error_response(401, "not logged in: run `owlshift ui` in a terminal")
            } else {
                action_page(context, &path, &body)
            }
        }
        _ => error_response(404, "no such page"),
    };
    let _ = request.respond(response);
}

/// The guard of `lock` when no run holds it. A run that panicked poisoned it
/// and holds nothing any more: it counts as free, so that one panic does not
/// answer 409 until the UI restarts.
fn try_lock_free(lock: &Mutex<()>) -> Option<std::sync::MutexGuard<'_, ()>> {
    match lock.try_lock() {
        Ok(guard) => Some(guard),
        Err(std::sync::TryLockError::Poisoned(poisoned)) => Some(poisoned.into_inner()),
        Err(std::sync::TryLockError::WouldBlock) => None,
    }
}

/// `POST /api/doctor` `{"project": "owner/name"}` or `{}` (OWL-472):
/// `owlshift doctor`'s checks, run on demand, once for the project named, once
/// per served project when it names none, or once on the personal file alone
/// when none is served. One run at a time (409), and none without a build
/// to compare (503). The handover of an older daemon, which the command line
/// does before its report, is not here: the server only calls
/// `doctor::run`.
fn doctor_page(context: &Context, body: &[u8]) -> Response<Cursor<Vec<u8>>> {
    let Some(build) = &context.build else {
        return error_response(
            503,
            "this server knows no build to check the running ones against",
        );
    };
    let asked = if body.iter().all(u8::is_ascii_whitespace) {
        None
    } else {
        match serde_json::from_slice::<Value>(body) {
            Ok(value) if value.is_object() => value
                .get("project")
                .map(|project| project.as_str().map(str::to_owned))
                .map(|project| project.ok_or(())),
            _ => return error_response(400, "the body is a JSON object"),
        }
    };
    let project = match asked {
        None => None,
        Some(Ok(project)) => match Repo::parse(&project) {
            Ok(repo) => Some(repo),
            Err(_) => return error_response(400, "the project is not owner/name"),
        },
        Some(Err(())) => return error_response(400, "the project is a text"),
    };
    let Some(_running) = try_lock_free(&context.doctor) else {
        return error_response(409, "a run of the checks is under way: wait for it");
    };
    let personal = (context.options.personal)();
    match orchestrator::doctor(
        &*context.options.system,
        &context.data,
        &personal,
        build,
        project.as_ref(),
    ) {
        Ok(value) => json_response(200, &value),
        Err(why) => error_response(503, &why),
    }
}

/// The reason a pause or a cancel from the web UI records when its field is
/// empty (OWL-473).
pub const UI_REASON: &str = "asked from the web UI";

/// A route's answer: a response either way.
type Answer = Result<Response<Cursor<Vec<u8>>>, Response<Cursor<Vec<u8>>>>;

/// The JSON object `body`, refused 400 when it is none.
fn body_object(body: &[u8]) -> Result<serde_json::Map<String, Value>, Response<Cursor<Vec<u8>>>> {
    match serde_json::from_slice::<Value>(body) {
        Ok(Value::Object(map)) => Ok(map),
        _ => Err(error_response(400, "the body is a JSON object")),
    }
}

/// The text field `key` of `map`: `None` when absent or null, refused 400
/// when it is not a text.
fn text_field(
    map: &serde_json::Map<String, Value>,
    key: &str,
) -> Result<Option<String>, Response<Cursor<Vec<u8>>>> {
    match map.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(error_response(400, &format!("{key} is a text"))),
    }
}

/// The served project `text` names, refused 400 when it is not owner/name
/// and 404 when the service does not serve it.
fn served_repo(context: &Context, text: &str) -> Result<Repo, Response<Cursor<Vec<u8>>>> {
    let pairs = [
        ("project".to_owned(), text.to_owned()),
        ("ticket".to_owned(), "X-1".to_owned()),
    ];
    target(context, &pairs).map(|(repo, _)| as_listed(context, repo))
}

/// `repo` as the list of served projects writes it, in any letter case:
/// the words of an action and the restart's `--served` name it so.
fn as_listed(context: &Context, repo: Repo) -> Repo {
    Registry::in_data_dir(&context.data)
        .find(&repo)
        .ok()
        .flatten()
        .and_then(|entry| Repo::parse(&entry.repository).ok())
        .unwrap_or(repo)
}

/// The reason of a pause or a cancel: the field `reason` when it holds
/// anything, [`UI_REASON`] otherwise.
fn reason_of(map: &serde_json::Map<String, Value>) -> Result<String, Response<Cursor<Vec<u8>>>> {
    Ok(text_field(map, "reason")?
        .map(|reason| reason.trim().to_owned())
        .filter(|reason| !reason.is_empty())
        .unwrap_or_else(|| UI_REASON.to_owned()))
}

/// The lines a command says, answered 200, or its refusal, answered 409.
fn said(done: Result<Vec<String>, String>) -> Response<Cursor<Vec<u8>>> {
    match done {
        Ok(lines) => json_response(200, &json!({ "lines": lines })),
        Err(why) => error_response(409, &why),
    }
}

/// The action routes (OWL-473), each through its command's own code and
/// locks, every request it writes made by [`BY_UI`]:
///
/// - `POST /api/pause` `{"project"?, "reason"?}` and `POST /api/resume`
///   `{"project"?}`: `owlshift pause` and `owlshift resume`, of the machine
///   when no project is named ([`controls`]);
/// - `POST /api/forget` `{"project"}`: `owlshift stop --forget`'s change;
/// - `POST /api/cancel` `{"project", "ticket", "reason"?}`: `owlshift
///   cancel` on the served project's kept project file;
/// - `POST /api/restart` `{"project", "ticket"}`: `owlshift continue
///   --to-watch`, in a child ([`Options::restart`]).
///
/// Each answers `{"lines"}`, the command's own words, or `{"error"}`, its
/// refusal, 409; a body that does not read is refused 400, a project the
/// service does not serve 404.
fn action_page(context: &Context, path: &str, body: &[u8]) -> Response<Cursor<Vec<u8>>> {
    let answer = || -> Answer {
        let map = body_object(body)?;
        let project = text_field(&map, "project")?;
        let data = context.data.as_path();
        match path {
            "/api/pause" | "/api/resume" => {
                let target = project
                    .map(|project| served_repo(context, &project))
                    .transpose()?;
                if path == "/api/resume" {
                    return Ok(said(controls::resume(data, target.as_ref())));
                }
                let request = Asked::new(BY_UI, Timestamp::now(), reason_of(&map)?);
                Ok(said(controls::pause(data, target.as_ref(), &request)))
            }
            "/api/forget" => {
                let project =
                    project.ok_or_else(|| error_response(400, "a stop names its project"))?;
                let repo = served_repo(context, &project)?;
                Ok(said(controls::forget(data, &repo)))
            }
            _ => {
                let ticket = text_field(&map, "ticket")?;
                let (Some(project), Some(ticket)) = (project, ticket) else {
                    return Err(error_response(400, "it names its project and its ticket"));
                };
                let pairs = [
                    ("project".to_owned(), project),
                    ("ticket".to_owned(), ticket),
                ];
                let (repo, ticket) = target(context, &pairs)?;
                let repo = as_listed(context, repo);
                if path == "/api/cancel" {
                    let reason = reason_of(&map)?;
                    return Ok(said(cancel_ui(context, &repo, &ticket, reason)));
                }
                Ok(match (context.options.restart)(&repo, &ticket) {
                    Restarted::Done(lines) => json_response(200, &json!({ "lines": lines })),
                    Restarted::Refused(why, lines) => {
                        json_response(409, &json!({ "error": why, "lines": lines }))
                    }
                    Restarted::GoesOn => json_response(
                        202,
                        &json!({ "lines": [format!(
                            "The restart of {ticket} goes on: `owlshift why {ticket}` tells its \
                             outcome."
                        )] }),
                    ),
                    Restarted::NotStarted(why) => error_response(500, &why),
                })
            }
        }
    };
    answer().unwrap_or_else(|response| response)
}

/// `owlshift cancel` of `ticket` on the served project `repo`, as
/// `cancel_cmd` runs it in a checkout: the project file is the one the
/// project is served on, its team checked, then [`cancel::ask`] and its
/// words. A project whose kept file the personal file no longer accepts is
/// refused with that reason, as its watch would be.
fn cancel_ui(
    context: &Context,
    repo: &Repo,
    ticket: &TicketId,
    reason: String,
) -> Result<Vec<String>, String> {
    let dirs = ProjectDirs::new(&context.data, repo);
    let personal = (context.options.personal)();
    let effective = served::effective(repo, &dirs, orchestrator::path_of(&personal))?;
    let FileState::Loaded {
        config: project, ..
    } = &effective.project
    else {
        return Err(format!(
            "{repo} is not served: its project file does not load"
        ));
    };
    on_demand::check_team(project, ticket)?;
    let request = Asked::new(BY_UI, Timestamp::now(), reason);
    let asked = cancel::ask(
        &crate::project::runner_git(),
        &dirs,
        ticket,
        &request,
        Voice::Ui,
    )?;
    Ok(cancel::said(ticket, &asked, Voice::Ui))
}

/// The project an event of the web UI's own is recorded under, as the
/// daemon's machine-wide events are under `owlshift daemon` (OWL-473).
pub const UI_PROJECT: &str = "owlshift ui";

/// The served projects whose kept project file the personal file at `file`
/// makes refused, with why, read as their watch reads them
/// ([`served::effective`]).
fn refused_projects(data: &Path, file: &Path) -> BTreeMap<String, String> {
    Registry::in_data_dir(data)
        .read()
        .map(|served| served.projects)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|entry| {
            let repo = Repo::parse(&entry.repository).ok()?;
            let dirs = ProjectDirs::new(data, &repo);
            // A project never kept is refused whatever the personal file.
            served::read_record(&dirs).ok()??.kept?;
            served::effective(&repo, &dirs, Some(file.to_owned()))
                .err()
                .map(|why| (entry.repository, why))
        })
        .collect()
}

/// `POST /api/config/personal` `{"path": [segments], "value", "rev"}`
/// (OWL-473): one key of the personal file set to `value`, or removed when
/// it is null, through [`personal_edit::apply`]; `rev` is the SHA-256 of the
/// file the page read, null for a file it read absent. One edit at a time.
///
/// Answers `{"lines", "applies", "rev", "refused_projects"}` once written:
/// `refused_projects` names each served project whose kept project file the
/// new personal file refuses, with why and whether it was `already` refused
/// before. The edit is recorded as a `decision` event of [`UI_PROJECT`] with
/// `config: "personal"`, the `key`, the `value` written (null for a key
/// removed), `by: "ui"` and `applies`; an event that could not be recorded
/// is a warning among the `lines`. A refused edit writes nothing and is
/// answered with why: 422 for the loader's error, 409 for a file changed
/// since the page read it or a system with no configuration folder, 400 for
/// a key outside the table or a value not of its kind.
fn personal_edit_page(context: &Context, body: &[u8]) -> Response<Cursor<Vec<u8>>> {
    let map = match body_object(body) {
        Ok(map) => map,
        Err(response) => return response,
    };
    let path: Option<Vec<String>> = map.get("path").and_then(Value::as_array).and_then(|items| {
        items
            .iter()
            .map(|item| item.as_str().map(str::to_owned))
            .collect()
    });
    let Some(path) = path.filter(|path| !path.is_empty()) else {
        return error_response(400, "an edit names its key's path, a list of texts");
    };
    let rev = match map.get("rev") {
        Some(Value::Null) => None,
        Some(Value::String(rev)) => Some(rev.clone()),
        _ => return error_response(400, "an edit names the rev of the file it read, or null"),
    };
    let Some(value) = map.get("value") else {
        return error_response(400, "an edit gives its value, null to remove the key");
    };
    let _editing = context
        .personal_edit
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let personal = (context.options.personal)();
    let file = orchestrator::path_of(&personal);
    let before = file
        .as_deref()
        .map(|file| refused_projects(&context.data, file))
        .unwrap_or_default();
    let applied = match personal_edit::apply(file.as_deref(), rev.as_deref(), &path, value) {
        Ok(applied) => applied,
        Err(refusal) => return error_response(refusal.status(), &refusal.message()),
    };
    let file = file.expect("an edit applied names its file");
    if !applied.changed {
        let said = match &applied.value {
            Some(value) => format!("{} = {value} is in {} already", applied.key, file.display()),
            None => format!("{} is not set in {}", applied.key, file.display()),
        };
        return json_response(
            200,
            &json!({
                "lines": [format!("{said}: nothing is written.")],
                "applies": applied.applies.key(),
                "rev": applied.rev,
                "refused_projects": refused_list(&before, &before),
            }),
        );
    }
    let mut warnings = Vec::new();
    {
        let mut sink = EventSink::new(UI_PROJECT, EventLog::in_dir(&context.data), &mut warnings);
        sink.emit_project(
            None,
            EventKind::Decision,
            crate::events::data([
                ("config", json!("personal")),
                ("key", json!(applied.key)),
                ("value", json!(applied.value)),
                ("by", json!(BY_UI)),
                ("applies", json!(applied.applies.key())),
            ]),
        );
    }
    let mut lines = vec![match &applied.value {
        Some(value) => format!(
            "{} = {value} is written to {}: it applies {}.",
            applied.key,
            file.display(),
            applied.applies.words()
        ),
        None => format!(
            "{} is removed from {}: its default applies {}.",
            applied.key,
            file.display(),
            applied.applies.words()
        ),
    }];
    lines.extend(
        String::from_utf8_lossy(&warnings)
            .lines()
            .filter(|line| line.starts_with("owlshift: warning:"))
            .map(str::to_owned),
    );
    let after = refused_projects(&context.data, &file);
    json_response(
        200,
        &json!({
            "lines": lines,
            "applies": applied.applies.key(),
            "rev": applied.rev,
            "refused_projects": refused_list(&after, &before),
        }),
    )
}

/// The served projects the personal file refuses now (`after`), each with
/// why and whether it was refused `before` the edit.
fn refused_list(after: &BTreeMap<String, String>, before: &BTreeMap<String, String>) -> Vec<Value> {
    after
        .iter()
        .map(|(repository, why)| {
            json!({
                "already": before.contains_key(repository),
                "repository": repository,
                "why": why,
            })
        })
        .collect()
}

/// `GET /api/config[?rev=<hash>]` (OWL-472): the effective configuration with
/// each value's file ([`configuration`]).
fn config_page(context: &Context, url: &str) -> Response<Cursor<Vec<u8>>> {
    let pairs = match query_of(url) {
        Ok(pairs) => pairs,
        Err(why) => return error_response(400, why),
    };
    let personal = (context.options.personal)();
    match configuration::answer(&context.data, &personal, param(&pairs, "rev")) {
        Ok(value) => json_response(200, &value),
        Err(why) => error_response(503, &format!("the served projects cannot be read: {why}")),
    }
}

/// `GET /api/spend[?days=7|30|90|all][&project=owner/repo][&rev=<hash>]`
/// (OWL-472): what the runs cost ([`spend`]), the last 30 days by default.
fn spend_page(context: &Context, url: &str) -> Response<Cursor<Vec<u8>>> {
    let pairs = match query_of(url) {
        Ok(pairs) => pairs,
        Err(why) => return error_response(400, why),
    };
    let period = match param(&pairs, "days") {
        None => spend::Period::Days(spend::DEFAULT_DAYS),
        Some(text) => match spend::Period::parse(text) {
            Some(period) => period,
            None => return error_response(400, "days is 7, 30, 90 or all"),
        },
    };
    let project = match param(&pairs, "project") {
        None | Some("") => None,
        Some(text) => match Repo::parse(text) {
            Ok(repo) => Some(repo.to_string()),
            Err(_) => return error_response(400, "the project is not owner/name"),
        },
    };
    let query = spend::Query {
        period,
        project,
        rev: param(&pairs, "rev").map(str::to_owned),
    };
    let personal = (context.options.personal)();
    let inputs = spend::Inputs {
        data: &context.data,
        index: &context.index,
        personal: &personal,
        cache: &context.spend,
        now: Timestamp::now(),
        tz: jiff::tz::TimeZone::system(),
    };
    json_response(200, &spend::answer(&inputs, &query))
}

/// A whole number query value, `None` when the key is absent, `Err` when it
/// is not one.
fn number_param(pairs: &[(String, String)], key: &str) -> Result<Option<u64>, ()> {
    match param(pairs, key) {
        None => Ok(None),
        Some(text) if !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()) => {
            text.parse().map(Some).map_err(|_| ())
        }
        Some(_) => Err(()),
    }
}

/// `GET /api/orchestrator[?gen=G&after=O][&log=O]` (OWL-472): the daemon, the
/// watches, the usage windows, the feed of decisions and warnings after the
/// cursor, and the daemon's log after its offset ([`orchestrator`]).
fn orchestrator_page(context: &Context, url: &str) -> Response<Cursor<Vec<u8>>> {
    let pairs = match query_of(url) {
        Ok(pairs) => pairs,
        Err(why) => return error_response(400, why),
    };
    let (Ok(generation), Ok(after), Ok(log)) = (
        number_param(&pairs, "gen"),
        number_param(&pairs, "after"),
        number_param(&pairs, "log"),
    ) else {
        return error_response(400, "gen, after and log are whole numbers");
    };
    let cursors = orchestrator::Cursors {
        events: generation.zip(after),
        log,
    };
    let personal = (context.options.personal)();
    let inputs = orchestrator::Inputs {
        data: &context.data,
        index: &context.index,
        personal: &personal,
        now: Timestamp::now(),
        tz: jiff::tz::TimeZone::system(),
    };
    json_response(200, &orchestrator::answer(&inputs, cursors))
}

/// The value of `text` with its `%XX` escapes and `+`s read, `None` when
/// they are not whole or the bytes are not UTF-8.
fn percent_decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        match bytes[at] {
            b'%' => {
                let pair = text.get(at + 1..at + 3)?;
                out.push(u8::from_str_radix(pair, 16).ok()?);
                at += 3;
            }
            b'+' => {
                out.push(b' ');
                at += 1;
            }
            byte => {
                out.push(byte);
                at += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

/// The query of `url` as `(key, value)` pairs, split on `&` and on the first
/// `=` of each, each decoded; a key that repeats, or text that does not
/// decode, is refused.
fn query_of(url: &str) -> Result<Vec<(String, String)>, &'static str> {
    let Some((_, query)) = url.split_once('?') else {
        return Ok(Vec::new());
    };
    let mut pairs: Vec<(String, String)> = Vec::new();
    for part in query.split('&').filter(|part| !part.is_empty()) {
        let (key, value) = part.split_once('=').unwrap_or((part, ""));
        let (Some(key), Some(value)) = (percent_decode(key), percent_decode(value)) else {
            return Err("the query is not percent-encoded UTF-8");
        };
        if pairs.iter().any(|(seen, _)| *seen == key) {
            return Err("a key of the query is repeated");
        }
        pairs.push((key, value));
    }
    Ok(pairs)
}

fn param<'a>(pairs: &'a [(String, String)], key: &str) -> Option<&'a str> {
    pairs
        .iter()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.as_str())
}

/// The project and ticket a ticket route names: the project must be one the
/// served registry lists, the ticket a ticket id. Nothing of the query
/// becomes a path but through these two.
fn target(
    context: &Context,
    pairs: &[(String, String)],
) -> Result<(Repo, TicketId), Response<Cursor<Vec<u8>>>> {
    let Some(project) = param(pairs, "project") else {
        return Err(error_response(400, "a ticket route names its project"));
    };
    let Some(ticket) = param(pairs, "ticket") else {
        return Err(error_response(400, "a ticket route names its ticket"));
    };
    let Ok(repo) = Repo::parse(project) else {
        return Err(error_response(400, "the project is not owner/name"));
    };
    let Ok(ticket) = TicketId::new(ticket) else {
        return Err(error_response(400, "the ticket is not a ticket identifier"));
    };
    match Registry::in_data_dir(&context.data).read() {
        Ok(served) if served.find(&repo.to_string()).is_some() => Ok((repo, ticket)),
        Ok(_) => Err(error_response(404, "no such served project")),
        Err(why) => Err(error_response(
            503,
            &format!("the served projects cannot be read: {why}"),
        )),
    }
}

/// `GET /api/ticket?project=owner/repo&ticket=OWL-1`: the ticket's snapshot.
fn ticket_page(context: &Context, url: &str) -> Response<Cursor<Vec<u8>>> {
    let pairs = match query_of(url) {
        Ok(pairs) => pairs,
        Err(why) => return error_response(400, why),
    };
    let (repo, ticket) = match target(context, &pairs) {
        Ok(target) => target,
        Err(response) => return response,
    };
    let git = crate::project::runner_git();
    let personal = (context.options.personal)();
    let key = context.keys.state(&context.data, &repo);
    let inputs = ticket::Inputs {
        data: &context.data,
        git: &git,
        events: &context.events,
        cache: &context.tickets,
        concurrent_runs: crate::usage::concurrent_runs(&personal),
        now: Timestamp::now(),
        key: &key,
        names: &|account| context.keys.decider_name(&repo, &key, account),
    };
    json_response(200, &ticket::snapshot(&inputs, &repo, &ticket))
}

/// The form `POST /api/reply` sends, read strictly: `answers` a list of
/// `{"question", "choice", "text"}`, `choice` an option's letter,
/// `"recommendation"` or null, `go` a boolean.
fn reply_form(value: &Value) -> Result<(String, String, reply::Form), &'static str> {
    let text = |key: &str| value.get(key).and_then(Value::as_str).map(str::to_owned);
    let (Some(project), Some(ticket), Some(ask)) = (text("project"), text("ticket"), text("ask"))
    else {
        return Err("a reply names its project, its ticket and the ask it answers");
    };
    let Some(go) = value.get("go").and_then(Value::as_bool) else {
        return Err("a reply says whether it counts now (go)");
    };
    let Some(listed) = value.get("answers").and_then(Value::as_array) else {
        return Err("a reply lists its answers");
    };
    let mut answers = Vec::new();
    for answer in listed {
        let Some(question) = answer.get("question").and_then(Value::as_str) else {
            return Err("each answer names its question");
        };
        let choice = match answer.get("choice") {
            None | Some(Value::Null) => None,
            Some(Value::String(choice)) if choice == "recommendation" => {
                Some(reply::Choice::Recommendation)
            }
            Some(Value::String(letter))
                if !letter.is_empty() && letter.bytes().all(|b| b.is_ascii_uppercase()) =>
            {
                Some(reply::Choice::Letter(letter.clone()))
            }
            Some(_) => return Err("a choice is an option's letter or the recommendation"),
        };
        let text = match answer.get("text") {
            None | Some(Value::Null) => String::new(),
            Some(Value::String(text)) => text.clone(),
            Some(_) => return Err("an answer's text is a text"),
        };
        answers.push(reply::Answer {
            question: question.to_owned(),
            choice,
            text,
        });
    }
    Ok((project, ticket, reply::Form { ask, answers, go }))
}

/// Removes its ticket from the replies under way when dropped.
struct Replying<'a> {
    context: &'a Context,
    key: (String, String),
}

impl Drop for Replying<'_> {
    fn drop(&mut self) {
        self.context
            .replying
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.key);
    }
}

/// `POST /api/reply` (OWL-475): the decider's reply to the open ask of a
/// ticket, posted through the operator's key. 403 when the form is not
/// offered or the floor refuses the reply, 409 for a round that changed or a
/// reply to the ticket under way, 400 for a form that does not compose, 502
/// for the tracker's error, and 200 with the comment once posted. One line on
/// the UI's error output names the project, the ticket, the round and the
/// comment, never the body.
fn reply_page(context: &Context, body: &[u8]) -> Response<Cursor<Vec<u8>>> {
    let Ok(value) = serde_json::from_slice::<Value>(body) else {
        return error_response(400, "the body is a JSON object");
    };
    let (project, ticket, form) = match reply_form(&value) {
        Ok(read) => read,
        Err(why) => return error_response(400, why),
    };
    let pairs = [
        ("project".to_owned(), project),
        ("ticket".to_owned(), ticket),
    ];
    let (repo, ticket) = match target(context, &pairs) {
        Ok(target) => target,
        Err(response) => return response,
    };
    let state = context.keys.state(&context.data, &repo);
    let key = match &*state {
        KeyState::Ready(key) => key,
        KeyState::Reading => {
            return error_response(
                403,
                "the account of the project's tracker key is still being read",
            );
        }
        KeyState::Unavailable(why) => {
            return error_response(403, &reply::unusable(why));
        }
    };
    let busy = (
        repo.to_string().to_ascii_lowercase(),
        ticket.as_str().to_owned(),
    );
    if !context
        .replying
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(busy.clone())
    {
        return error_response(409, "a reply to this ticket is being posted");
    }
    let _replying = Replying { context, key: busy };
    let git = crate::project::runner_git();
    let checkout = ProjectDirs::new(&context.data, &repo).checkout();
    let stored = match crate::ticket_ref::read(&git, &checkout, &ticket) {
        Ok(Some(stored)) => stored,
        Ok(None) => return error_response(409, "the ticket waits for no answer"),
        Err(why) => return error_response(503, &format!("the ticket cannot be read: {why}")),
    };
    let offer = reply::section(&stored.record, &state, &ticket, &|_| None);
    if offer["offered"] != Value::Bool(true) {
        if offer.is_null() || offer["ask"] != Value::String(form.ask.clone()) {
            return error_response(
                409,
                "the ticket no longer waits on the round this page shows: reload it",
            );
        }
        let why = offer["why_not"]
            .as_str()
            .unwrap_or("the form is not offered");
        return error_response(403, why);
    }
    let round = offer["round"].clone();
    match reply::send(
        key.tracker.as_ref(),
        &key.account,
        &stored.record,
        &ticket,
        &form,
    ) {
        Ok(comment) => {
            eprintln!(
                "The web UI posted the decider's reply to {repo} {ticket}, round {round}: \
                 comment {}",
                comment.id
            );
            json_response(
                200,
                &json!({
                    "comment": comment.id,
                    "at": comment.created_at.to_string(),
                    "counts_now": form.go,
                }),
            )
        }
        Err(reply::ReplyError::RoundChanged) => error_response(
            409,
            "the ticket no longer waits on the round this page shows: reload it",
        ),
        Err(reply::ReplyError::Compose(error)) => error_response(400, &error.to_string()),
        Err(reply::ReplyError::Write(crate::writer::WriteError::Floor(violation))) => {
            error_response(403, &format!("refused by the policy floor: {violation}"))
        }
        Err(reply::ReplyError::Write(error)) => {
            error_response(502, &format!("the reply was not posted: {error}"))
        }
    }
}

/// `GET /api/ticket/log?project&ticket&run&file=stdout|stderr&from=N[&line=L]`:
/// the records of the whole lines of a run's log from byte `from`, `line`
/// being how many lines come before it (counted when absent).
fn ticket_log(context: &Context, url: &str) -> Response<Cursor<Vec<u8>>> {
    let pairs = match query_of(url) {
        Ok(pairs) => pairs,
        Err(why) => return error_response(400, why),
    };
    let (repo, ticket) = match target(context, &pairs) {
        Ok(target) => target,
        Err(response) => return response,
    };
    let Some(run) = param(&pairs, "run").filter(|run| ticket::is_run_id(run)) else {
        return error_response(400, "a run is named as its folder is");
    };
    let file = match param(&pairs, "file") {
        Some("stdout") => "stdout.log",
        Some("stderr") => "stderr.log",
        _ => return error_response(400, "the file is stdout or stderr"),
    };
    let number = |key: &str| -> Result<Option<u64>, ()> {
        match param(&pairs, key) {
            None => Ok(None),
            Some(text) if !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()) => {
                text.parse().map(Some).map_err(|_| ())
            }
            Some(_) => Err(()),
        }
    };
    let (Ok(from), Ok(line)) = (number("from"), number("line")) else {
        return error_response(400, "from and line are whole numbers");
    };
    let from = from.unwrap_or(0);
    let dir = ProjectDirs::new(&context.data, &repo)
        .runs(&ticket)
        .join(run);
    if !std::fs::symlink_metadata(&dir).is_ok_and(|meta| meta.is_dir()) {
        return error_response(404, "no such run");
    }
    let path = dir.join(file);
    match std::fs::symlink_metadata(&path) {
        Ok(meta) if !meta.is_file() => return error_response(404, "no such log"),
        _ => {}
    }
    // Asked before the file is read: a run's last bytes are flushed before
    // its `run_ended` is written, so a file read after it was seen is whole.
    let ended = context.events.ended(&repo.to_string(), &ticket, run);
    let read = match tail::read_lines(&path, from, tail::MAX_ANSWER, ended) {
        Ok(read) => read,
        Err(why) => return error_response(500, &why),
    };
    let start = if read.reset { 0 } else { from };
    let before = match (line, read.reset) {
        (_, true) => 0,
        (Some(line), false) => line,
        (None, false) => match tail::count_lines(&path, start) {
            Ok(count) => count,
            Err(why) => return error_response(500, &why),
        },
    };
    let plain = file == "stderr.log";
    let mut records = Vec::new();
    for (at, one) in read.lines.iter().enumerate() {
        let number = before + at as u64 + 1;
        let made = match one.cut {
            Some(length) => vec![runlog::cut(number, one.offset, &one.bytes, length)],
            None if plain => runlog::plain(number, one.offset, &one.bytes),
            None => runlog::classify(number, one.offset, &one.bytes),
        };
        records.extend(made.iter().map(runlog::ViewRecord::json));
    }
    json_response(
        200,
        &json!({
            "records": records,
            "next": read.next,
            "next_line": before + read.lines.len() as u64,
            "size": read.size,
            "reset": read.reset,
            "ended": ended,
        }),
    )
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
    session_holds(context, value)
}

/// Whether the session header value `value` holds under the token, read
/// again, and this daemon's run.
fn session_holds(context: &Context, value: &str) -> bool {
    token::read_token(&context.data)
        .is_ok_and(|token| auth::check_session(&token, &context.run, value))
}

/// The figures of the personal file read again, and, when it does not load,
/// the last figures of one that did with a note saying so.
fn figures_now(context: &Context) -> (Figures, Option<String>) {
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
    (figures, note)
}

/// The header now, read from the files `owlshift status` reads, with the
/// personal file read again; when it does not load, the last figures of
/// one that did, and a note saying so.
fn header_now(context: &Context) -> header::Header {
    let (figures, note) = figures_now(context);
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

/// The overview now: the event log's tail advanced, then the refs and the
/// ready snapshots read (OWL-470).
fn overview_now(context: &Context) -> overview::Overview {
    let personal = (context.options.personal)();
    let mut tail = context
        .tail
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    overview::gather(
        &context.data,
        &mut tail,
        crate::usage::concurrent_runs(&personal),
        Timestamp::now(),
    )
}

/// `GET /api/since[?at=<RFC 3339>]`: what happened after `at`, or in the
/// last 24 hours when the request has none (a browser's first visit). The
/// time is not a secret and is read as given: a bad one is a 400.
fn since_page(context: &Context, url: &str) -> Response<Cursor<Vec<u8>>> {
    let pairs = match query_of(url) {
        Ok(pairs) => pairs,
        Err(why) => return error_response(400, why),
    };
    let at = match param(&pairs, "at") {
        None => None,
        Some(text) => match text.parse::<Timestamp>() {
            Ok(at) => Some(at),
            Err(_) => return error_response(400, "at is a time, such as 2026-11-06T09:00:00Z"),
        },
    };
    let mut tail = context
        .tail
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // A read that fails leaves what was read before: the answer is of that.
    let _ = tail.advance();
    json_response(200, &tail.since(at, Timestamp::now()).json())
}

/// The drain's graph of the data directory `data` at `now`: `tail` advanced,
/// the waits of every served project as `owlshift status --waits` builds
/// them from the tail's folds (as [`overview::gather`] does), and each
/// project's ready snapshot as that read held it, laid out by
/// [`graph::of`]. The machine's run slots are counted up to
/// `concurrent_runs`.
pub fn graph_read(
    data: &Path,
    tail: &mut EventTail,
    concurrent_runs: u32,
    now: Timestamp,
) -> Graph {
    let mut notes = Vec::new();
    if let Err(why) = tail.advance() {
        notes.push(format!("the event log cannot be read: {why}"));
    }
    notes.extend(tail.notes());
    let chosen: Vec<(String, Repo, bool)> = status::served_projects(data, &mut notes)
        .into_iter()
        .map(|(entry, repo)| (entry.repository, repo, true))
        .collect();
    let tail: &EventTail = tail;
    let read = waits::gather_folded(
        data,
        chosen,
        concurrent_runs,
        now,
        &mut |repository| tail.folded(repository),
        notes,
    );
    let ready: BTreeMap<String, ReadyInput> = read
        .projects
        .iter()
        .map(|project| {
            let input = match &project.snapshot {
                Ok(Some(snapshot)) => ReadyInput::Read {
                    stale: crate::ready::stale(snapshot, now),
                    snapshot: snapshot.clone(),
                },
                Ok(None) => ReadyInput::Missing,
                Err(why) => ReadyInput::Unread(why.clone()),
            };
            (project.repository.clone(), input)
        })
        .collect();
    graph::of(&waits::build(&read), &ready)
}

/// The graph now, the personal file read again for `concurrent_runs`.
fn graph_now(context: &Context) -> Graph {
    let (figures, note) = figures_now(context);
    let mut graph = {
        let mut tail = context
            .tail
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        graph_read(&context.data, &mut tail, figures.0, Timestamp::now())
    };
    graph.notes.extend(note);
    graph
}

/// A graph as the stream's line and as the value that tells whether it
/// changed: its JSON without `at`, which is the time of each read.
fn graph_parts(graph: &Graph) -> (Value, String) {
    let mut value = serde_json::to_value(graph).unwrap_or(Value::Null);
    let line = json_text(&json!({ "graph": value }));
    if let Some(object) = value.as_object_mut() {
        object.remove("at");
    }
    (value, line)
}

/// The streams that run and the refresher's latest header, overview and graph.
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
    /// The refresher's latest overview line, and its own generation: the
    /// overview is built on a slower period than the header.
    overview: Option<String>,
    overview_generation: u64,
    refreshing: bool,
    /// The streams that asked for the graph (`?graph=1`): the refresher
    /// computes it only while there are any.
    graph_streams: HashSet<u64>,
    /// The refresher's latest graph, its JSON without `at` and its line, and
    /// how many times it changed.
    graph: Option<(Value, String)>,
    graph_generation: u64,
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
fn admit(context: &Arc<Context>, graph: bool) -> (u64, Arc<AtomicBool>) {
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
    if graph {
        state.graph_streams.insert(id);
    }
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
    let mut state = context.hub.lock();
    state.streams.retain(|(other, _)| *other != id);
    state.graph_streams.remove(&id);
}

/// The line a stream writes for `overview`.
fn overview_line(overview: &overview::Overview) -> String {
    json_text(&json!({ "overview": overview.json() }))
}

/// Reads the header every refresh period while a stream runs, and the
/// overview every overview period, and shares them with the streams.
fn refresh(context: &Arc<Context>) {
    let mut overview_read: Option<Instant> = None;
    let wait = context
        .options
        .refresh
        .min(context.options.overview_refresh)
        .max(Duration::from_millis(1));
    loop {
        let line = json_text(&json!({ "header": header_now(context).json() }));
        let overview_due =
            overview_read.is_none_or(|read| read.elapsed() >= context.options.overview_refresh);
        let overview = overview_due.then(|| overview_line(&overview_now(context)));
        if overview_due {
            overview_read = Some(Instant::now());
        }
        let wanted = !context.hub.lock().graph_streams.is_empty();
        let graph = wanted.then(|| graph_parts(&graph_now(context)));
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
        if let Some(overview) = overview
            && state.overview.as_deref() != Some(overview.as_str())
        {
            state.overview = Some(overview);
            state.overview_generation += 1;
            context.hub.changed.notify_all();
        }
        if let Some(graph) =
            graph.filter(|graph| state.graph.as_ref().map(|(value, _)| value) != Some(&graph.0))
        {
            state.graph = Some(graph);
            state.graph_generation += 1;
            context.hub.changed.notify_all();
        }
        let (state, _) = context
            .hub
            .changed
            .wait_timeout(state, wait)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        drop(state);
    }
}

/// The overview line a stream that starts writes, with the generation it
/// was read at, taken together under one lock: the refresher's latest, or
/// one built now when it has none yet (which it then shares). Reading the
/// generation apart would mark a newer line as already written.
fn first_overview(context: &Context) -> (String, u64) {
    if let Some(stored) = stored_overview(&context.hub) {
        return stored;
    }
    let line = overview_line(&overview_now(context));
    let mut state = context.hub.lock();
    if let Some(shared) = state.overview.clone() {
        return (shared, state.overview_generation);
    }
    state.overview = Some(line.clone());
    state.overview_generation += 1;
    let generation = state.overview_generation;
    drop(state);
    context.hub.changed.notify_all();
    (line, generation)
}

/// The refresher's latest overview line with the generation it was stored
/// at, read under one lock.
fn stored_overview(hub: &Hub) -> Option<(String, u64)> {
    let state = hub.lock();
    state
        .overview
        .clone()
        .map(|line| (line, state.overview_generation))
}

/// Writes `line` as one chunk of a chunked body, and flushes it.
fn chunk(writer: &mut dyn Write, line: &str) -> io::Result<()> {
    write!(writer, "{:x}\r\n{line}\n\r\n", line.len() + 1)?;
    writer.flush()
}

/// Serves `/api/stream`: its own status line and headers, then one chunk
/// per line until the page goes, or a newer stream supersedes it.
fn stream(context: &Arc<Context>, request: Request, graph: bool) {
    let session = match header_value(&request, SESSION_HEADER) {
        Ok(Some(value)) => value.to_owned(),
        _ => {
            let _ = request.respond(error_response(
                401,
                "not logged in: run `owlshift ui` in a terminal",
            ));
            return;
        }
    };
    let (id, superseded) = admit(context, graph);
    let mut writer = request.into_writer();
    let _ = write_stream(context, &mut *writer, &session, &superseded, graph);
    leave(context, id);
}

fn write_stream(
    context: &Context,
    writer: &mut dyn Write,
    session: &str,
    superseded: &AtomicBool,
    graph: bool,
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
    let (mut seen, mut seen_graph) = {
        let state = context.hub.lock();
        (state.generation, state.graph_generation)
    };
    let (mut written_overview, mut seen_overview) = first_overview(context);
    chunk(writer, &written_overview)?;
    // The graph the stream wrote last, without its `at`: what the
    // refresher's changes are compared with.
    let mut written_graph = None;
    if graph {
        let (value, line) = graph_parts(&graph_now(context));
        chunk(writer, &line)?;
        written_graph = Some(value);
    }
    let mut kept = Instant::now();
    let mut checked = Instant::now();
    loop {
        if superseded.load(Ordering::SeqCst) {
            chunk(writer, SUPERSEDED_LINE)?;
            break;
        }
        if checked.elapsed() >= context.options.refresh {
            if !session_holds(context, session) {
                chunk(writer, LOGGED_OUT_LINE)?;
                break;
            }
            checked = Instant::now();
        }
        let (latest, latest_overview, graph_latest) = {
            let state = context.hub.lock();
            let (state, _) = context
                .hub
                .changed
                .wait_timeout(state, Duration::from_millis(250))
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let header = if state.generation == seen {
                None
            } else {
                seen = state.generation;
                state.latest.clone()
            };
            let overview = if state.overview_generation == seen_overview {
                None
            } else {
                seen_overview = state.overview_generation;
                state.overview.clone()
            };
            let graph = if graph && state.graph_generation != seen_graph {
                seen_graph = state.graph_generation;
                state.graph.clone()
            } else {
                None
            };
            (header, overview, graph)
        };
        if superseded.load(Ordering::SeqCst) {
            continue;
        }
        let latest = latest.filter(|latest| *latest != written);
        let latest_overview = latest_overview.filter(|latest| *latest != written_overview);
        let graph_latest = graph_latest.filter(|(value, _)| Some(value) != written_graph.as_ref());
        let due = latest.is_some()
            || latest_overview.is_some()
            || graph_latest.is_some()
            || kept.elapsed() >= context.options.keepalive;
        if due && !session_holds(context, session) {
            chunk(writer, LOGGED_OUT_LINE)?;
            break;
        }
        if latest.is_some() || latest_overview.is_some() || graph_latest.is_some() {
            if let Some(latest) = latest {
                chunk(writer, &latest)?;
                written = latest;
            }
            if let Some(latest) = latest_overview {
                chunk(writer, &latest)?;
                written_overview = latest;
            }
            if let Some((value, line)) = graph_latest {
                chunk(writer, &line)?;
                written_graph = Some(value);
            }
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
    fn a_doctor_lock_poisoned_by_a_panic_is_free_and_a_held_one_is_not() {
        let lock = Mutex::new(());
        {
            let held = try_lock_free(&lock).expect("free at first");
            assert!(try_lock_free(&lock).is_none(), "held by a run");
            drop(held);
        }
        let shared = Arc::new(lock);
        let panicking = shared.clone();
        let _ = thread::spawn(move || {
            let _guard = panicking.lock().unwrap();
            panic!("a doctor run that panics");
        })
        .join();
        assert!(shared.is_poisoned());
        assert!(try_lock_free(&shared).is_some(), "a poisoned lock is free");
    }

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

    /// F1: the line a stream writes first and the generation it takes as seen
    /// are the pair the refresher stored under one lock, never a line with
    /// another generation (a newer one would hide the newer line from the
    /// stream). A writer bumps both under the lock while the reader reads;
    /// a reader that took them with two locks would see them apart.
    #[test]
    fn the_stored_overview_and_its_generation_are_read_together() {
        let hub = Arc::new(Hub::default());
        assert_eq!(stored_overview(&hub), None);
        let writer = {
            let hub = hub.clone();
            thread::spawn(move || {
                for generation in 1..=20_000_u64 {
                    let mut state = hub.lock();
                    state.overview = Some(format!("line {generation}"));
                    state.overview_generation = generation;
                }
            })
        };
        let mut reads = 0;
        while !writer.is_finished() || reads == 0 {
            if let Some((line, generation)) = stored_overview(&hub) {
                assert_eq!(line, format!("line {generation}"));
                reads += 1;
            }
        }
        writer.join().unwrap();
        assert_eq!(
            stored_overview(&hub),
            Some(("line 20000".to_owned(), 20_000))
        );
    }
}
