//! OWL-435: the background service on the bench (architecture section 5,
//! "The background service"): the daemon `owlshift daemon` runs, with
//! stand-in watches (`owlshift-daemon-bench`) and short timings. Two served
//! projects each under its served lock, one kept off by a watch typed in a
//! terminal and one forgotten; a watch killed and started again with a
//! doubling delay, told at the third end; a hard kill of the daemon leaving
//! no watch or step; a stop drained and a SIGTERM checked, a hung watch
//! stopped by its bound; a watch that cannot serve pausing its project.
//!
//! OWL-441: scenario S11 with a real watch under the daemon (the bench's
//! `drain`), its steps in children and its runs on the fake harness: a
//! SIGTERM checks and resumes two Builds, a SIGKILL leaves the quarantine,
//! notified, each cut Build told once that it waits (OWL-456), and no
//! ticket is dispatched twice across the restarts.
//!
//! OWL-467: the web UI as the daemon's child, the bench's stand-in on a
//! socket the daemon binds and keeps: started again after a kill, the port
//! held meanwhile; a new run secret for each daemon run; told at the third
//! end; a taken port said and tried again; stopped once the watches end,
//! and gone after a SIGTERM or a hard kill; no watch holding its socket.
#![cfg(unix)]

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use jiff::Timestamp;
use owlshift_adapters::forge::Repo;
use owlshift_adapters::tracker::markdown::MarkdownTracker;
use owlshift_contracts::event::EventKind;
use owlshift_contracts::ids::TicketId;
use owlshift_contracts::service::{BY_DAEMON, BY_STOP, BuildIdentity, Request};
use owlshift_platform::service::{Kind, Layout};
use owlshift_runner::config::OWLSHIFT_VERSION;
use owlshift_runner::daemon::upgrade::{self, Binary, Handover};
use owlshift_runner::daemon::{self, requests};
use owlshift_runner::install::{self, Install};
use owlshift_runner::project::ProjectDirs;
use owlshift_runner::quarantine;
use owlshift_runner::served::{self, Registry};
use owlshift_runner::ui::{record, token};
use owlshift_runner::voice::Voice;
use owlshift_runner::watch;
use owlshift_testkit::step_child::{
    ChildProject, REPO, build_asks, ended_as, events, holding_harness, intake_declaring, reply_for,
};
use owlshift_testkit::wait;

const BENCH: &str = env!("CARGO_BIN_EXE_owlshift-daemon-bench");
/// The fake harness.
const FAKE: &str = env!("CARGO_BIN_EXE_owlshift-fake-harness");

/// The bench's timings, in milliseconds: a pass a second, a look every
/// 50 ms, restarts after 300 ms doubling up to 1.2 s, reset after 5 s.
const TIMING: &str = "1000,50,300,1200,5000";

const WEB: &str = "acme/web";
const API: &str = "acme/api";

struct Bench {
    tmp: tempfile::TempDir,
    daemon: Option<Child>,
    /// The project file of the real watch the daemon runs (OWL-441); none:
    /// the stand-in.
    project: Option<PathBuf>,
    /// The build the daemon records (OWL-462): its commit and its build
    /// time in seconds since the epoch; none: version "bench", no build.
    build: Option<(String, i64)>,
    /// The port of the web UI's stand-in (OWL-467); none: no UI.
    ui_port: Option<u16>,
    /// Whether the stand-in ends at once, each time.
    ui_exits: bool,
    /// Whether the daemon's UI is the real one, on a scripted host (OWL-472).
    ui_real: bool,
}

impl Bench {
    fn new() -> Self {
        Self {
            tmp: tempfile::tempdir().unwrap(),
            daemon: None,
            project: None,
            build: None,
            ui_port: None,
            ui_exits: false,
            ui_real: false,
        }
    }

    fn data(&self) -> PathBuf {
        self.tmp.path().join("data")
    }

    fn dirs(&self, repo: &str) -> ProjectDirs {
        ProjectDirs::new(&self.data(), &Repo::parse(repo).unwrap())
    }

    fn registry(&self) -> Registry {
        Registry::in_data_dir(&self.data())
    }

    /// Serves `repo`, registered from a folder of its own.
    fn serve(&self, repo: &str) {
        let checkout = self.tmp.path().join(repo.replace('/', "-"));
        fs::create_dir_all(&checkout).unwrap();
        self.registry()
            .register(&Repo::parse(repo).unwrap(), &checkout)
            .unwrap();
    }

    fn bench_dir(&self, repo: &str) -> PathBuf {
        self.data().join("bench").join(repo.replace('/', "-"))
    }

    /// Starts a daemon, its output added to what the ones before it wrote.
    fn start(&mut self) {
        fs::create_dir_all(self.data()).unwrap();
        let output = |name: &str| {
            fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(self.tmp.path().join(name))
                .unwrap()
        };
        let mut daemon = Command::new(BENCH);
        daemon
            .arg("daemon")
            .env("OWLSHIFT_DATA_DIR", self.data())
            .env("OWLSHIFT_BENCH_TIMING", TIMING)
            .env("OWLSHIFT_BENCH_NOTES", self.notes_file())
            .stdin(Stdio::null())
            .stdout(output("out"))
            .stderr(output("err"));
        if let Some(project) = &self.project {
            daemon.env("OWLSHIFT_BENCH_PROJECT", project);
        }
        if let Some((commit, built_at)) = &self.build {
            daemon
                .env("OWLSHIFT_BENCH_COMMIT", commit)
                .env("OWLSHIFT_BENCH_BUILT_AT", built_at.to_string());
        }
        if let Some(port) = self.ui_port {
            daemon.env("OWLSHIFT_BENCH_UI_PORT", port.to_string());
        }
        if self.ui_exits {
            daemon.env("OWLSHIFT_BENCH_UI", "exit");
        }
        if self.ui_real {
            daemon.env("OWLSHIFT_BENCH_UI", "real");
        }
        self.daemon = Some(daemon.spawn().unwrap());
    }

    fn notes_file(&self) -> PathBuf {
        self.tmp.path().join("notes")
    }

    fn notes(&self) -> Vec<String> {
        fs::read_to_string(self.notes_file())
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    /// How many passes the real watches of [`REPO`] made so far.
    fn passes(&self) -> u64 {
        fs::metadata(self.bench_dir(REPO).join("passes")).map_or(0, |file| file.len())
    }

    /// The daemon's output so far, for a failure.
    fn output(&self) -> String {
        let read = |name: &str| fs::read_to_string(self.tmp.path().join(name)).unwrap_or_default();
        format!("--- out\n{}--- err\n{}", read("out"), read("err"))
    }

    /// Every stand-in watch `repo` had, oldest first: its pid and its step's.
    fn watches(&self, repo: &str) -> Vec<(u32, u32)> {
        let Ok(entries) = fs::read_dir(self.bench_dir(repo)) else {
            return Vec::new();
        };
        let mut found: Vec<(Duration, u32, u32)> = entries
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let name = entry.file_name().into_string().ok()?;
                name.parse::<u32>().ok()?;
                let text = fs::read_to_string(entry.path()).ok()?;
                let mut pids = text.split_whitespace().map(|pid| pid.parse().unwrap());
                let at = entry
                    .metadata()
                    .ok()?
                    .modified()
                    .ok()?
                    .duration_since(std::time::UNIX_EPOCH)
                    .ok()?;
                Some((at, pids.next()?, pids.next()?))
            })
            .collect();
        found.sort();
        found
            .into_iter()
            .map(|(_, watch, step)| (watch, step))
            .collect()
    }

    /// The stand-in watch of `repo` running now, once there is one.
    fn running(&self, repo: &str) -> (u32, u32) {
        let mut seen = None;
        self.wait_for(&format!("a watch of {repo}"), || {
            seen = self
                .watches(repo)
                .into_iter()
                .rev()
                .find(|(watch, _)| alive(*watch));
            seen.is_some()
        });
        seen.unwrap()
    }

    /// Waits until `done` holds, the bench's output in a failure.
    fn wait_for(&self, what: &str, done: impl FnMut() -> bool) {
        wait::until_with(what, || self.output(), done);
    }

    /// [`Bench::wait_for`] within `bound`, which only a bound the design sets
    /// justifies: the caller cites it.
    fn wait_within(&self, what: &str, bound: Duration, done: impl FnMut() -> bool) {
        wait::within(bound, what, || self.output(), done);
    }

    /// The daemon's end.
    fn ended(&mut self) -> ExitStatus {
        let mut status = None;
        let daemon = self.daemon.as_mut().unwrap();
        let ended = wait::reached(wait::REACHED, || {
            status = daemon.try_wait().unwrap();
            status.is_some()
        });
        assert!(ended, "the daemon still runs\n{}", self.output());
        status.unwrap()
    }

    fn signal(&self, signal: &str, pid: u32) {
        let sent = Command::new("kill")
            .args(["-s", signal, &pid.to_string()])
            .status()
            .unwrap();
        assert!(sent.success(), "kill -s {signal} {pid}");
    }

    fn daemon_pid(&self) -> u32 {
        self.daemon.as_ref().unwrap().id()
    }

    /// The pid the UI's stand-in wrote last.
    fn ui_pid(&self) -> Option<u32> {
        fs::read_to_string(self.data().join("bench-ui.pid"))
            .ok()?
            .trim()
            .parse()
            .ok()
    }

    /// The answer of the UI's stand-in on `port` other than `not`'s, once
    /// there is one: its pid and its run secret's fingerprint.
    fn ui(&self, port: u16, not: Option<u32>) -> (u32, String) {
        let mut seen = None;
        self.wait_for("the web UI to answer", || {
            seen = ui_answer(port).filter(|(pid, _)| Some(*pid) != not);
            seen.is_some()
        });
        seen.unwrap()
    }

    /// Waits until the port is free and the UI `pid` gone.
    fn ui_gone(&self, port: u16, pid: u32) {
        self.wait_for("the web UI to be gone and its port free", || {
            !alive(pid) && TcpListener::bind(("127.0.0.1", port)).is_ok()
        });
    }
}

/// Stops, on a test's way out, what it left running: on a failure only,
/// once its pids may not be its own any more otherwise.
impl Drop for Bench {
    fn drop(&mut self) {
        if !thread::panicking() {
            return;
        }
        if let Some(daemon) = &mut self.daemon {
            let _ = daemon.kill();
            let _ = daemon.wait();
        }
        if let Some(pid) = self.ui_pid() {
            let _ = Command::new("kill")
                .args(["-s", "KILL", &pid.to_string()])
                .status();
        }
        for repo in [WEB, API] {
            for (watch, step) in self.watches(repo) {
                for pid in [watch, step] {
                    let _ = Command::new("kill")
                        .args(["-s", "KILL", &pid.to_string()])
                        .status();
                }
            }
        }
    }
}

fn alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(Stdio::null())
        .status()
        .unwrap()
        .success()
}

/// Two served projects run, each under its served lock; a project a watch
/// typed in a terminal holds waits for it, then is served; a project no
/// longer listed has its watch drained and its lock let go, the others
/// going on.
#[test]
fn served_projects_run_each_under_its_lock_and_a_forgotten_one_is_drained() {
    let mut bench = Bench::new();
    bench.serve(WEB);
    bench.serve(API);
    // A watch typed in a terminal holds acme/api.
    let terminal = served::share_served(&bench.dirs(API)).unwrap().unwrap();
    bench.start();
    let (web, web_step) = bench.running(WEB);
    assert!(alive(web_step));
    assert!(served::served_now(&bench.dirs(WEB)).unwrap());
    bench.wait_for("the daemon to say acme/api is held", || {
        bench
            .output()
            .contains("acme/api: a watch typed in a terminal runs on it")
    });
    assert!(bench.watches(API).is_empty(), "{}", bench.output());

    drop(terminal);
    let (api, _) = bench.running(API);
    assert!(served::served_now(&bench.dirs(API)).unwrap());

    bench.registry().forget(&Repo::parse(WEB).unwrap()).unwrap();
    bench.wait_for("acme/web's watch to drain", || !alive(web));
    bench.wait_for("acme/web to be let go", || {
        !served::served_now(&bench.dirs(WEB)).unwrap()
    });
    assert!(!alive(web_step));
    assert!(!watch::stop::stands(&bench.dirs(WEB)));
    assert!(alive(api), "acme/api goes on\n{}", bench.output());
    assert_eq!(bench.watches(WEB).len(), 1, "never started again");

    requests::ask_stop(
        &bench.data(),
        &Request::new(BY_STOP, Timestamp::now(), "the test ends"),
    )
    .unwrap();
    assert!(bench.ended().success(), "{}", bench.output());
}

/// A watch that ends without being asked starts again after a delay that
/// doubles; the third end in a row is told once.
#[test]
fn a_killed_watch_starts_again_with_a_doubling_delay_told_at_the_third_end() {
    let mut bench = Bench::new();
    bench.serve(WEB);
    bench.start();
    let mut delays = Vec::new();
    for _ in 0..3 {
        let (watch, _) = bench.running(WEB);
        let started = bench.watches(WEB).len();
        bench.signal("KILL", watch);
        let killed = Instant::now();
        bench.wait_for("the watch to start again", || {
            bench.watches(WEB).len() > started
        });
        delays.push(killed.elapsed());
    }
    // 300 ms, 600 ms, 1.2 s: each at least its delay, each longer than the
    // one before less the bench's slack.
    for (delay, least) in delays.iter().zip([300, 600, 1200]) {
        assert!(
            *delay >= Duration::from_millis(least),
            "{delays:?}\n{}",
            bench.output()
        );
    }
    assert!(delays[2] > delays[0], "{delays:?}");
    let notes = bench.notes();
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(
        notes[0].starts_with("Owlshift: the watch of acme/web ended 3 times in a row"),
        "{notes:?}"
    );
    bench.signal("TERM", bench.daemon_pid());
    assert!(bench.ended().success(), "{}", bench.output());
}

/// A hard kill of the daemon leaves no watch and no step running: its
/// sentinel stops the watches, theirs their steps.
#[test]
fn a_hard_kill_of_the_daemon_leaves_no_watch_or_step() {
    let mut bench = Bench::new();
    bench.serve(WEB);
    bench.serve(API);
    bench.start();
    let started = [bench.running(WEB), bench.running(API)];
    bench.signal("KILL", bench.daemon_pid());
    let _ = bench.ended();
    let pids: Vec<u32> = started
        .iter()
        .flat_map(|(watch, step)| [*watch, *step])
        .collect();
    bench.wait_for("every watch and step to be gone", || {
        pids.iter().all(|&pid| !alive(pid))
    });
}

/// `owlshift stop`'s request drains: each watch gets its stop file and
/// ends, the daemon removes what it wrote and exits 0. A SIGTERM is a
/// checked stop: each watch is told, a hung one stopped by its bound, and
/// the daemon exits 0 once they ended.
#[test]
fn a_stop_drains_every_watch_and_a_sigterm_checks_them() {
    let mut bench = Bench::new();
    bench.serve(WEB);
    bench.serve(API);
    bench.start();
    let watches = [bench.running(WEB), bench.running(API)];
    requests::ask_stop(
        &bench.data(),
        &Request::new(BY_STOP, Timestamp::now(), "asked by the test"),
    )
    .unwrap();
    let status = bench.ended();
    assert!(status.success(), "{status}\n{}", bench.output());
    for (watch, step) in watches {
        assert!(!alive(watch) && !alive(step));
    }
    for repo in [WEB, API] {
        assert_eq!(bench.watches(repo).len(), 1, "nothing started again");
        assert!(!watch::stop::stands(&bench.dirs(repo)));
        assert!(!served::served_now(&bench.dirs(repo)).unwrap());
    }
    assert!(requests::stop_asked(&bench.data()).is_none());
    assert!(daemon::running(&bench.data()).unwrap().is_none());
    assert!(!daemon::instance::record_file(&bench.data()).exists());
    assert!(
        bench
            .output()
            .contains("Draining (owlshift stop: asked by the test)"),
        "{}",
        bench.output()
    );

    // Again, with acme/api's watch hung on a checked stop.
    fs::write(bench.bench_dir(API).join("hang"), "").unwrap();
    bench.start();
    let watches = [bench.running(WEB), bench.running(API)];
    let sent = Instant::now();
    bench.signal("TERM", bench.daemon_pid());
    let status = bench.ended();
    assert!(status.success(), "{status}\n{}", bench.output());
    assert!(
        sent.elapsed() >= Duration::from_secs(2),
        "the hung watch held the daemon until its bound"
    );
    for (watch, step) in watches {
        bench.wait_for("the watches and their steps to be gone", || {
            !alive(watch) && !alive(step)
        });
    }
}

/// A watch that cannot serve its project pauses it: the daemon's pause file
/// with the reason, one notification however often it is tried again at
/// each pass; once a watch so tried still runs a pass later, the pause is
/// lifted.
#[test]
fn a_watch_that_cannot_serve_pauses_its_project_until_one_serves() {
    let mut bench = Bench::new();
    bench.serve(WEB);
    fs::create_dir_all(bench.bench_dir(WEB)).unwrap();
    fs::write(
        bench.bench_dir(WEB).join("exit"),
        daemon::UNSERVABLE_EXIT.to_string(),
    )
    .unwrap();
    bench.start();
    let dirs = bench.dirs(WEB);
    bench.wait_for("the pause", || requests::pause(&dirs).is_some());
    let pause = requests::pause(&dirs).unwrap().unwrap();
    assert_eq!(pause.by, BY_DAEMON);
    assert_eq!(pause.why, "acme/web cannot be served on the bench");
    // Tried again at the next passes, told once.
    bench.wait_for("two more tries", || {
        bench.output().matches("Paused acme/web").count() >= 3
    });
    let notes = bench.notes();
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(
        notes[0].starts_with("Owlshift: acme/web is paused: acme/web cannot be served"),
        "{notes:?}"
    );

    fs::remove_file(bench.bench_dir(WEB).join("exit")).unwrap();
    let (watch, _) = bench.running(WEB);
    bench.wait_for("the pause to be lifted", || {
        requests::pause(&dirs).is_none()
    });
    assert!(alive(watch), "{}", bench.output());
    // The daemon removes the pause file before it says so: wait for the line.
    bench.wait_for("the served-again line", || {
        bench.output().contains("acme/web is served again")
    });
    bench.signal("TERM", bench.daemon_pid());
    assert!(bench.ended().success(), "{}", bench.output());
}

/// A real watch of [`REPO`] under the daemon (OWL-479), on a project whose
/// drain admits nothing: what its start does, not its steps.
fn real_watch(bench: &mut Bench) {
    let root = bench.tmp.path().to_owned();
    let machine = root.join("machine");
    let mut project = ChildProject::seed_parallel(&root, PathBuf::from(FAKE), 0, 1, (&machine, 1));
    project.notes = Some(bench.notes_file());
    let file = root.join("project.toml");
    project.save(&file);
    bench
        .registry()
        .register(&Repo::parse(REPO).unwrap(), &project.checkout)
        .unwrap();
    bench.project = Some(file);
    fs::create_dir_all(bench.bench_dir(REPO)).unwrap();
}

/// OWL-479: a watch the daemon starts while its tracker gives no answer
/// waits, told once, without ending or being restarted, then starts once
/// the tracker answers.
#[test]
fn a_watch_started_offline_waits_then_starts_once_its_tracker_answers() {
    let mut bench = Bench::new();
    real_watch(&mut bench);
    let offline = bench.bench_dir(REPO).join("offline");
    fs::write(&offline, "").unwrap();
    bench.start();
    bench.wait_for("the wait told", || {
        bench.output().contains("waits for the network")
    });
    // Several tries later: still waiting, told once, never restarted.
    thread::sleep(Duration::from_millis(1500));
    let output = bench.output();
    assert_eq!(
        output.matches("waits for the network").count(),
        1,
        "{output}"
    );
    assert!(
        output.contains("Linear's token endpoint: io: failed to lookup address information"),
        "{output}"
    );
    assert!(!output.contains("watching demo/project"), "{output}");
    assert_eq!(bench.passes(), 0);
    assert!(bench.notes().is_empty(), "{:?}", bench.notes());

    fs::remove_file(&offline).unwrap();
    // The watch's lines reach the output through the daemon's relay, a tick
    // after the pass is counted: wait for them too.
    bench.wait_for("a pass and its lines", || {
        let output = bench.output();
        bench.passes() > 0
            && output.contains("reached its tracker and forge")
            && output.contains("watching demo/project")
    });
    let output = bench.output();
    assert_eq!(
        output.matches("reached its tracker and forge").count(),
        1,
        "{output}"
    );
    assert_eq!(
        output.matches("watching demo/project").count(),
        1,
        "{output}"
    );
    bench.signal("TERM", bench.daemon_pid());
    assert!(bench.ended().success(), "{}", bench.output());
}

/// OWL-479: a tracker that refuses its credentials still ends the watch as
/// unservable, and the daemon pauses the project.
#[test]
fn a_refused_token_at_a_watchs_start_still_pauses_its_project() {
    let mut bench = Bench::new();
    real_watch(&mut bench);
    fs::write(bench.bench_dir(REPO).join("refused"), "").unwrap();
    bench.start();
    let dirs = bench.dirs(REPO);
    bench.wait_for("the pause", || requests::pause(&dirs).is_some());
    let pause = requests::pause(&dirs).unwrap().unwrap();
    assert_eq!(pause.by, BY_DAEMON);
    assert!(pause.why.contains("invalid_client"), "{}", pause.why);
    assert!(!bench.output().contains("waits for the network"));
    bench.signal("TERM", bench.daemon_pid());
    assert!(bench.ended().success(), "{}", bench.output());
}

/// The two tickets of scenario S11.
const TICKETS: [&str; 2] = ["DEMO-1", "DEMO-2"];

/// What a held Build writes in its worktree before it is cut: a plan and a
/// ledger in its run folder, the checkpoint the next run resumes from.
const CHECKPOINT: &str = "mkdir -p .owlshift/run && echo '# Plan' > .owlshift/run/plan.md && \
                          echo '{\"steps\": []}' > .owlshift/run/ledger.json";

/// Whether each Build run of `ticket` in the log of `data` started from a
/// checkpoint, oldest first.
fn build_starts(data: &Path, ticket: &str) -> Vec<bool> {
    events(data)
        .into_iter()
        .filter(|event| {
            event.kind == EventKind::RunStarted
                && event.ticket.as_ref().is_some_and(|t| t.as_str() == ticket)
                && event.data.get("role").and_then(|role| role.as_str()) == Some("build")
        })
        .map(|event| event.data.get("resumes_plan") == Some(&serde_json::json!(true)))
        .collect()
}

/// The WAITING comments on `ticket` in the tracker of `project`.
fn waiting_on(project: &ChildProject, ticket: &str) -> usize {
    MarkdownTracker::new(&project.checkout)
        .comments(&TicketId::new(ticket).unwrap())
        .unwrap()
        .iter()
        .filter(|comment| comment.body.starts_with("[owlshift] WAITING\n"))
        .count()
}

/// No double dispatch, as the exit gate reads it from the events of `data`
/// (architecture section 5, "Dispatch and resume"): no run of a ticket
/// starts before every earlier run of it has ended, and each of `tickets`,
/// like any other, has one `dispatch` with the command `drain`.
fn assert_no_double_dispatch(data: &Path, tickets: &[&str]) {
    let mut open: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut drains: BTreeMap<String, usize> = BTreeMap::new();
    for event in events(data) {
        let Some(ticket) = event.ticket.as_ref().map(|t| t.as_str().to_owned()) else {
            continue;
        };
        match event.kind {
            EventKind::RunStarted => {
                let runs = open.entry(ticket.clone()).or_default();
                assert!(
                    runs.is_empty(),
                    "{ticket}: run {:?} started while {runs:?} had not ended",
                    event.run
                );
                runs.extend(event.run);
            }
            EventKind::RunEnded => {
                if let Some(runs) = open.get_mut(&ticket) {
                    runs.retain(|run| Some(run) != event.run.as_ref());
                }
            }
            EventKind::Dispatch
                if event.data.get("command") == Some(&serde_json::json!("drain")) =>
            {
                *drains.entry(ticket).or_default() += 1;
            }
            _ => {}
        }
    }
    for ticket in tickets {
        assert!(drains.contains_key(*ticket), "{ticket}: {drains:?}");
    }
    assert!(drains.values().all(|&count| count == 1), "{drains:?}");
}

/// OWL-441's acceptance, scenario S11 on the bench (architecture section 5,
/// "Three ways to stop", "A cut run", "Dispatch and resume"): the daemon
/// serves the project of DEMO-1 and DEMO-2 with a real watch, each step in
/// a child, and both Builds, once under way, have left a checkpoint.
///
/// - A SIGTERM to the daemon is a checked stop: it exits 0, both runs are
///   stopped and checked, their obligations retired, nothing quarantined.
/// - At the next start both Builds resume from their checkpoints.
/// - A SIGKILL of the daemon while they run cuts them: the next start
///   quarantines the project, notified once, and nothing runs. Each cut
///   Build, In Progress at its stage, is told on the ticket that it waits:
///   one WAITING comment over the passes (OWL-456).
/// - Across the three starts, no ticket has two runs at once, nor a second
///   `drain` dispatch.
#[test]
fn s11_a_checked_stop_resumes_both_builds_and_a_kill_quarantines() {
    let mut bench = Bench::new();
    let root = bench.tmp.path().to_owned();
    let plays = root.join("plays");
    fs::create_dir_all(&plays).unwrap();
    for (id, zone) in [("DEMO-1", "a"), ("DEMO-2", "b")] {
        reply_for(&plays, "intake", Some(id), &intake_declaring(&[zone]));
    }
    reply_for(&plays, "build", None, &build_asks());
    fs::write(plays.join("hold"), "").unwrap();
    let harness = holding_harness(
        &plays,
        Path::new(FAKE),
        "build",
        CHECKPOINT,
        &format!("exec sleep {}", wait::OUTLASTS.as_secs()),
    );
    let machine = root.join("machine");
    let mut project = ChildProject::seed_parallel(&root, harness, 2, 2, (&machine, 2));
    assert_eq!(project.data, bench.data());
    project.notes = Some(bench.notes_file());
    let file = root.join("project.toml");
    project.save(&file);
    project.move_ticket("DEMO-9", "Done");
    bench
        .registry()
        .register(&Repo::parse(REPO).unwrap(), &project.checkout)
        .unwrap();
    bench.project = Some(file);
    let dirs = project.dirs();
    let pid_files = TICKETS.map(|id| plays.join(format!("build-{id}.pid")));
    // Both Builds under way, held: their runs' pids.
    let held = |bench: &Bench| -> Vec<u32> {
        bench.wait_for("both Builds under way", || {
            pid_files.iter().all(|file| file.exists())
        });
        pid_files
            .iter()
            .map(|file| fs::read_to_string(file).unwrap().trim().parse().unwrap())
            .collect()
    };

    // A checked stop.
    bench.start();
    let runs = held(&bench);
    bench.signal("TERM", bench.daemon_pid());
    let status = bench.ended();
    assert!(status.success(), "{status}\n{}", bench.output());
    for run in &runs {
        assert!(!alive(*run), "a Build's run outlived the checked stop");
    }
    assert_eq!(dirs.quarantine().unwrap(), None, "{}", bench.output());
    assert!(!dirs.obligations_stand(), "{}", bench.output());
    assert_eq!(ended_as(&project, "stopped"), TICKETS, "{}", bench.output());
    for id in TICKETS {
        assert_eq!(waiting_on(&project, id), 0, "{id}\n{}", bench.output());
    }

    // Resumed from their checkpoints at the next start.
    for file in &pid_files {
        fs::remove_file(file).unwrap();
    }
    bench.start();
    let runs = held(&bench);
    for id in TICKETS {
        assert_eq!(
            build_starts(&bench.data(), id),
            [false, true],
            "{id}\n{}",
            bench.output()
        );
    }

    // A hard kill cuts them: the quarantine, notified once.
    bench.signal("KILL", bench.daemon_pid());
    let _ = bench.ended();
    bench.wait_for("the cut runs to be gone", || {
        runs.iter().all(|run| !alive(*run))
    });
    assert!(dirs.obligations_stand(), "{}", bench.output());
    for file in &pid_files {
        fs::remove_file(file).unwrap();
    }
    bench.start();
    let found = quarantine::found_line(REPO);
    bench.wait_for("the quarantine, notified", || {
        dirs.quarantine().unwrap().is_some() && bench.notes().contains(&found)
    });
    // Three passes more, none of which may start a run.
    let passes = bench.passes();
    bench.wait_for("three more passes of the watch", || {
        bench.passes() >= passes + 3
    });
    bench.signal("TERM", bench.daemon_pid());
    let status = bench.ended();
    assert!(status.success(), "{status}\n{}", bench.output());
    let notes = bench.notes();
    assert_eq!(
        notes.iter().filter(|line| **line == found).count(),
        1,
        "{notes:?}"
    );
    for (id, file) in TICKETS.iter().zip(&pid_files) {
        assert!(!file.exists(), "{id} ran while quarantined");
        assert_eq!(build_starts(&bench.data(), id).len(), 2, "{id}");
        // Told once, whatever the passes (OWL-456).
        assert_eq!(waiting_on(&project, id), 1, "{id}\n{}", bench.output());
    }
    assert_no_double_dispatch(&bench.data(), &TICKETS);
}

/// OWL-474's acceptance under the daemon (architecture section 5, "The
/// local web UI", "Cancel"): DEMO-1's and DEMO-2's Builds are under way,
/// each in a child of the real watch, and have left a checkpoint. DEMO-1 is
/// canceled:
///
/// - its run is gone within 5 seconds of the cancel, the bound the design
///   gives the cut of the runs ("As built"), and checked, its obligation
///   retired, nothing quarantined, while DEMO-2's runs on and the watch
///   goes on passing;
/// - DEMO-1 is then parked at Build, a pass the design does not bound and
///   load slows (OWL-491), the PARKED comment naming `owlshift cancel` and
///   why, the cancel file gone;
/// - `owlshift continue --to-watch DEMO-1` restarts it, and the watch
///   resumes its Build from its checkpoint.
#[test]
fn a_canceled_build_is_stopped_checked_and_parked_then_resumed_from_its_checkpoint() {
    use owlshift_runner::on_demand::Stop;
    use owlshift_testkit::step_child::{cancel, parked_at_build};

    let mut bench = Bench::new();
    let root = bench.tmp.path().to_owned();
    let plays = root.join("plays");
    fs::create_dir_all(&plays).unwrap();
    for (id, zone) in [("DEMO-1", "a"), ("DEMO-2", "b")] {
        reply_for(&plays, "intake", Some(id), &intake_declaring(&[zone]));
    }
    reply_for(&plays, "build", None, &build_asks());
    fs::write(plays.join("hold"), "").unwrap();
    let harness = holding_harness(
        &plays,
        Path::new(FAKE),
        "build",
        CHECKPOINT,
        &format!("exec sleep {}", wait::OUTLASTS.as_secs()),
    );
    let machine = root.join("machine");
    let mut project = ChildProject::seed_parallel(&root, harness, 2, 2, (&machine, 2));
    project.notes = Some(bench.notes_file());
    let file = root.join("project.toml");
    project.save(&file);
    project.move_ticket("DEMO-9", "Done");
    bench
        .registry()
        .register(&Repo::parse(REPO).unwrap(), &project.checkout)
        .unwrap();
    bench.project = Some(file);
    let dirs = project.dirs();
    let pid_files = TICKETS.map(|id| plays.join(format!("build-{id}.pid")));
    let pid = |file: &Path| -> u32 { fs::read_to_string(file).unwrap().trim().parse().unwrap() };
    bench.start();
    bench.wait_for("both Builds under way", || {
        pid_files.iter().all(|file| file.exists())
    });
    let (one, two) = (pid(&pid_files[0]), pid(&pid_files[1]));

    let why = "it rewrites the wrong module";
    cancel(&project, "DEMO-1", why);
    // The bound the design gives the cut (architecture section 5, "Cancel",
    // "As built").
    bench.wait_within("DEMO-1's run stopped", Duration::from_secs(5), || {
        !alive(one)
    });
    bench.wait_for("DEMO-1 parked", || parked_at_build(&project, "DEMO-1"));
    assert!(alive(two), "DEMO-2's run was stopped\n{}", bench.output());
    let passes = bench.passes();
    bench.wait_for("three more passes of the watch", || {
        bench.passes() >= passes + 3
    });
    assert!(alive(two), "DEMO-2's run was stopped\n{}", bench.output());
    let demo1 = TicketId::new("DEMO-1").unwrap();
    assert!(!owlshift_runner::cancel::stands(&dirs, &demo1));
    assert_eq!(dirs.quarantine().unwrap(), None, "{}", bench.output());
    let left = fs::read_dir(dirs.obligations_dir())
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .filter(|name| name.starts_with("demo-1-"))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    assert!(left.is_empty(), "{left:?}\n{}", bench.output());
    assert_eq!(
        ended_as(&project, "stopped"),
        ["DEMO-1"],
        "{}",
        bench.output()
    );
    let parked: Vec<String> = MarkdownTracker::new(&project.checkout)
        .comments(&demo1)
        .unwrap()
        .into_iter()
        .map(|comment| comment.body)
        .filter(|body| body.starts_with("[owlshift] PARKED\n"))
        .collect();
    assert_eq!(parked.len(), 1, "{parked:?}");
    assert!(
        parked[0].contains("Canceled by `owlshift cancel` at ") && parked[0].contains(why),
        "{}",
        parked[0]
    );

    // Restarted and left to the watch, which resumes it from its checkpoint.
    fs::remove_file(&pid_files[0]).unwrap();
    let mut out = Vec::new();
    let restarted = project.with_on_demand(&mut out, |od, sink| od.continue_to_watch(&demo1, sink));
    assert!(
        matches!(
            restarted,
            Err(Stop::ToWatch {
                restarted: true,
                ..
            })
        ),
        "{restarted:?}"
    );
    bench.wait_for("DEMO-1's Build resumed", || pid_files[0].exists());
    assert_eq!(
        build_starts(&bench.data(), "DEMO-1"),
        [false, true],
        "{}",
        bench.output()
    );
    bench.signal("TERM", bench.daemon_pid());
    let status = bench.ended();
    assert!(status.success(), "{status}\n{}", bench.output());
}

/// An upgrade's drain request, as a newer binary writes it.
fn upgrade_drain() -> Request {
    Request::new(
        "owlshift start",
        Timestamp::now(),
        "an upgrade from version 0.1.0 to 0.2.0",
    )
}

/// OWL-436's acceptance (architecture section 5, "An upgrade"): the daemon
/// serves DEMO-1 and DEMO-2 with a real watch, both Builds under way and
/// held. An upgrade's drain reaches the watch; the Builds, released, finish
/// as they would have, not `stopped`; no run of the old binary starts after
/// the drain; the daemon exits with 75, nothing left standing.
///
/// OWL-462: the drain is the real handover's, by a binary of the same
/// version whose build alone is later and of another commit, the service
/// at login installed for it and its launcher having started the daemon.
#[test]
fn an_upgrade_drain_lets_the_steps_under_way_finish_then_exits_75() {
    let mut bench = Bench::new();
    bench.build = Some(("a".repeat(40), 1_761_600_000));
    let root = bench.tmp.path().to_owned();
    let plays = root.join("plays");
    fs::create_dir_all(&plays).unwrap();
    for (id, zone) in [("DEMO-1", "a"), ("DEMO-2", "b")] {
        reply_for(&plays, "intake", Some(id), &intake_declaring(&[zone]));
    }
    reply_for(&plays, "build", None, &build_asks());
    fs::write(plays.join("hold"), "").unwrap();
    let harness = holding_harness(
        &plays,
        Path::new(FAKE),
        "build",
        CHECKPOINT,
        "while [ -e \"$dir/hold\" ]; do sleep 0.1; done",
    );
    let machine = root.join("machine");
    let mut project = ChildProject::seed_parallel(&root, harness, 2, 2, (&machine, 2));
    project.notes = Some(bench.notes_file());
    let file = root.join("project.toml");
    project.save(&file);
    project.move_ticket("DEMO-9", "Done");
    bench
        .registry()
        .register(&Repo::parse(REPO).unwrap(), &project.checkout)
        .unwrap();
    bench.project = Some(file);
    let dirs = project.dirs();
    bench.start();
    bench.wait_for("both Builds under way", || {
        TICKETS
            .iter()
            .all(|id| plays.join(format!("build-{id}.pid")).exists())
    });
    let started = |data: &Path| {
        events(data)
            .iter()
            .filter(|event| event.kind == EventKind::RunStarted)
            .count()
    };
    let before = started(&bench.data());

    let data = bench.data();
    let layout = Layout {
        kind: Kind::Systemd,
        dir: root.join("units"),
    };
    install::install(&Install {
        data: &data,
        layout: &layout,
        binary: Path::new(BENCH),
        config_dir: None,
        path: None,
    })
    .unwrap();
    let running = daemon::running(&data).unwrap().unwrap().unwrap();
    fs::write(upgrade::launched_file(&data), format!("{}\n", running.pid)).unwrap();
    let later = BuildIdentity::from_build(&"b".repeat(40), "0", "1761600600", "");
    let own = Binary {
        version: OWLSHIFT_VERSION,
        build: Some(&later),
    };
    assert_eq!(running.version, own.version, "one version");
    assert!(
        running.build.as_ref().is_some_and(|build| build != &later),
        "two builds: {running:?}"
    );
    assert_eq!(
        upgrade::hand_over(&data, own, Path::new(BENCH), "owlshift start"),
        Ok(Handover::Ask(running))
    );
    bench.wait_for("the watch to drain on its stop file", || {
        events(&bench.data()).iter().any(|event| {
            event.data.get("watch") == Some(&serde_json::json!("stop"))
                && event.data.get("asked") == Some(&serde_json::json!("stop_file"))
        })
    });
    fs::remove_file(plays.join("hold")).unwrap();
    let status = bench.ended();
    assert_eq!(status.code(), Some(75), "{status}\n{}", bench.output());

    assert_eq!(
        started(&bench.data()),
        before,
        "a run started after the drain\n{}",
        bench.output()
    );
    for id in TICKETS {
        assert_eq!(build_starts(&bench.data(), id).len(), 1, "{id}");
    }
    // Every run, both Builds included, ended, and none `stopped`.
    let runs = |kind: EventKind| -> Vec<String> {
        let mut runs: Vec<String> = events(&bench.data())
            .into_iter()
            .filter(|event| event.kind == kind)
            .filter_map(|event| event.run.map(|run| run.to_string()))
            .collect();
        runs.sort();
        runs
    };
    assert_eq!(
        runs(EventKind::RunStarted),
        runs(EventKind::RunEnded),
        "{}",
        bench.output()
    );
    assert!(
        ended_as(&project, "stopped").is_empty(),
        "{}",
        bench.output()
    );
    assert_eq!(dirs.quarantine().unwrap(), None, "{}", bench.output());
    assert!(!dirs.obligations_stand(), "{}", bench.output());
    assert!(requests::drain_asked(&bench.data()).is_none());
    assert!(!watch::stop::stands(&dirs));
    assert!(daemon::running(&bench.data()).unwrap().is_none());
    let watch_starts = events(&bench.data())
        .iter()
        .filter(|event| event.data.get("service") == Some(&serde_json::json!("watch_started")))
        .count();
    assert_eq!(watch_starts, 1, "the watch was not started again");
    assert!(
        bench.output().contains(&format!(
            "Draining for an upgrade (owlshift start: an upgrade from version {OWLSHIFT_VERSION} \
             (aaaaaaaaaaaa, built 2025-10-27T21:20:00Z) to {OWLSHIFT_VERSION} (bbbbbbbbbbbb, \
             built 2025-10-27T21:30:00Z)"
        )),
        "{}",
        bench.output()
    );
}

/// OWL-436, on the stand-in: a drain left from before the daemon started is
/// removed and the daemon serves; a drain ends it with 75; a stop asked
/// during a drain, by `owlshift stop` or by SIGTERM, wins: it ends 0, so the
/// service manager does not start it again.
#[test]
fn an_upgrade_drain_ends_75_unless_a_stop_is_asked_meanwhile() {
    let mut bench = Bench::new();
    bench.serve(WEB);
    let data = bench.data();
    fs::create_dir_all(daemon::service_dir(&data)).unwrap();

    requests::ask_drain(&data, &upgrade_drain()).unwrap();
    bench.start();
    let (watch, step) = bench.running(WEB);
    assert!(requests::drain_asked(&data).is_none(), "{}", bench.output());

    requests::ask_drain(&data, &upgrade_drain()).unwrap();
    let status = bench.ended();
    assert_eq!(status.code(), Some(75), "{status}\n{}", bench.output());
    assert!(!alive(watch) && !alive(step));
    assert_eq!(bench.watches(WEB).len(), 1, "nothing started again");
    assert!(requests::drain_asked(&data).is_none());
    assert!(!watch::stop::stands(&bench.dirs(WEB)));

    // The watch holds its drain while `linger` stands, as for a step.
    let linger = bench.bench_dir(WEB).join("linger");
    let lingering = |bench: &Bench, times: usize| {
        bench.wait_for("the watch to hold its drain", || {
            bench.output().matches("drains, its step under way").count() >= times
        });
    };
    fs::write(&linger, "").unwrap();
    bench.start();
    bench.running(WEB);
    requests::ask_drain(&data, &upgrade_drain()).unwrap();
    lingering(&bench, 1);
    requests::ask_stop(
        &data,
        &Request::new(BY_STOP, Timestamp::now(), "asked by the test"),
    )
    .unwrap();
    fs::remove_file(&linger).unwrap();
    let status = bench.ended();
    assert!(status.success(), "{status}\n{}", bench.output());
    assert!(requests::drain_asked(&data).is_none());
    assert!(requests::stop_asked(&data).is_none());

    fs::write(&linger, "").unwrap();
    bench.start();
    bench.running(WEB);
    requests::ask_drain(&data, &upgrade_drain()).unwrap();
    lingering(&bench, 2);
    bench.signal("TERM", bench.daemon_pid());
    let status = bench.ended();
    assert!(status.success(), "{status}\n{}", bench.output());
    assert!(requests::drain_asked(&data).is_none());
    assert!(
        bench
            .output()
            .contains("A stop was asked during the upgrade's drain"),
        "{}",
        bench.output()
    );
}

/// A port of 127.0.0.1 free now.
fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// What the UI's stand-in on `port` answers, within 200 ms: its pid and its
/// run secret's fingerprint. `None` while the daemon holds the port with no
/// UI: the connection waits in the socket's queue.
fn ui_answer(port: u16) -> Option<(u32, String)> {
    let stream = TcpStream::connect(("127.0.0.1", port)).ok()?;
    stream
        .set_read_timeout(Some(Duration::from_millis(200)))
        .ok()?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).ok()?;
    let mut words = line.split_whitespace();
    (words.next()? == "bench-ui").then_some(())?;
    Some((words.next()?.parse().ok()?, words.next()?.to_owned()))
}

/// OWL-467: the UI starts with the daemon; killed, it starts again after the
/// bench's first delay, then after twice that, with the same run secret;
/// while it is down, the daemon holds its port. A record of an earlier run
/// is gone once the daemon starts.
#[test]
fn the_ui_starts_with_the_daemon_and_starts_again_after_a_kill() {
    let mut bench = Bench::new();
    let port = free_port();
    bench.ui_port = Some(port);
    let data = bench.data();
    fs::create_dir_all(daemon::service_dir(&data)).unwrap();
    fs::write(
        record::file(&data),
        r#"{"format":1,"pid":1,"port":1,"since":"2026-11-01T00:00:00Z"}"#,
    )
    .unwrap();
    assert!(record::read(&data).unwrap().is_some());
    bench.start();
    let (mut last, digest) = bench.ui(port, None);
    assert!(record::read(&data).unwrap().is_none(), "{}", bench.output());
    assert!(token::token_file(&data).exists());
    let mut delays = Vec::new();
    for _ in 0..2 {
        bench.signal("KILL", last);
        let killed = Instant::now();
        let mut answered = None;
        bench.wait_for("the web UI to start again", || {
            answered = ui_answer(port).filter(|(pid, _)| *pid != last);
            if answered.is_none() {
                let taken = TcpListener::bind(("127.0.0.1", port));
                assert!(
                    taken.is_err_and(|error| error.kind() == std::io::ErrorKind::AddrInUse),
                    "the port is free while the UI is down"
                );
            }
            answered.is_some()
        });
        delays.push(killed.elapsed());
        let (pid, again) = answered.unwrap();
        assert_eq!(again, digest, "the same daemon run, the same run secret");
        last = pid;
    }
    for (delay, least) in delays.iter().zip([300, 600]) {
        assert!(
            *delay >= Duration::from_millis(least),
            "{delays:?}\n{}",
            bench.output()
        );
    }
    assert!(
        bench
            .output()
            .contains(&format!("Started the web UI on 127.0.0.1:{port}")),
        "{}",
        bench.output()
    );
    bench.signal("TERM", bench.daemon_pid());
    assert!(bench.ended().success(), "{}", bench.output());
    bench.ui_gone(port, last);
}

/// OWL-467: each daemon run hands its UI a run secret of its own, so a new
/// daemon start ends every session.
#[test]
fn a_new_daemon_hands_over_a_new_run_secret() {
    let mut bench = Bench::new();
    let port = free_port();
    bench.ui_port = Some(port);
    bench.start();
    let (first, digest) = bench.ui(port, None);
    requests::ask_stop(
        &bench.data(),
        &Request::new(BY_STOP, Timestamp::now(), "asked by the test"),
    )
    .unwrap();
    assert!(bench.ended().success(), "{}", bench.output());
    bench.ui_gone(port, first);
    bench.start();
    let (second, again) = bench.ui(port, Some(first));
    assert_ne!(again, digest);
    bench.signal("TERM", bench.daemon_pid());
    assert!(bench.ended().success(), "{}", bench.output());
    bench.ui_gone(port, second);
}

/// OWL-467: a UI that keeps ending is started again each time, and told at
/// the third end in a row, with what it said.
#[test]
fn a_ui_that_keeps_ending_is_told_at_the_third_end() {
    let mut bench = Bench::new();
    bench.ui_port = Some(free_port());
    bench.ui_exits = true;
    bench.start();
    bench.wait_for("the third end to be told", || {
        bench
            .notes()
            .iter()
            .any(|note| note.starts_with("Owlshift: the web UI ended 3 times in a row"))
    });
    let output = bench.output();
    assert!(output.contains("ui: bench-ui exits as asked"), "{output}");
    assert!(
        output.contains(
            "The web UI ended (exit status: 1: bench-ui exits as asked): started again in"
        ),
        "{output}"
    );
    assert_eq!(bench.notes().len(), 1, "{:?}", bench.notes());
    bench.signal("TERM", bench.daemon_pid());
    assert!(bench.ended().success(), "{}", bench.output());
}

/// OWL-467: a port another program holds is said, naming it and `[ui]
/// port`, and tried again with the restart delays until it is free; the
/// token is made before the first try.
#[test]
fn a_taken_port_is_said_and_tried_again() {
    let mut bench = Bench::new();
    let holder = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = holder.local_addr().unwrap().port();
    bench.ui_port = Some(port);
    bench.start();
    let said = format!(
        "Port {port} of 127.0.0.1, `[ui] port` of the personal file, is taken by another \
         program: the web UI does not start; tried again in"
    );
    bench.wait_for("the taken port to be said", || {
        bench.output().contains(&said)
    });
    assert!(token::token_file(&bench.data()).exists());
    drop(holder);
    let (pid, _) = bench.ui(port, None);
    bench.signal("TERM", bench.daemon_pid());
    assert!(bench.ended().success(), "{}", bench.output());
    bench.ui_gone(port, pid);
}

/// OWL-467: at a stop by request, and at an upgrade's drain, the UI serves
/// until every watch has ended, then is stopped; the port is free once the
/// daemon has exited.
#[test]
fn a_stop_and_an_upgrade_end_the_ui_after_the_watches() {
    let mut bench = Bench::new();
    let port = free_port();
    bench.ui_port = Some(port);
    bench.serve(WEB);
    let linger = bench.bench_dir(WEB).join("linger");
    for (times, upgrade) in [(1, false), (2, true)] {
        fs::create_dir_all(bench.bench_dir(WEB)).unwrap();
        fs::write(&linger, "").unwrap();
        bench.start();
        bench.running(WEB);
        let (pid, _) = bench.ui(port, bench.ui_pid().filter(|pid| !alive(*pid)));
        if upgrade {
            requests::ask_drain(&bench.data(), &upgrade_drain()).unwrap();
        } else {
            requests::ask_stop(
                &bench.data(),
                &Request::new(BY_STOP, Timestamp::now(), "asked by the test"),
            )
            .unwrap();
        }
        bench.wait_for("the watch to hold its drain", || {
            bench.output().matches("drains, its step under way").count() >= times
        });
        assert!(alive(pid), "{}", bench.output());
        assert_eq!(ui_answer(port).map(|(answered, _)| answered), Some(pid));
        fs::remove_file(&linger).unwrap();
        let status = bench.ended();
        let expected = if upgrade { 75 } else { 0 };
        assert_eq!(
            status.code(),
            Some(expected),
            "{status}\n{}",
            bench.output()
        );
        bench.ui_gone(port, pid);
    }
    assert_eq!(
        bench.output().matches("The web UI ended (").count(),
        2,
        "{}",
        bench.output()
    );
}

/// OWL-467: a SIGTERM stops the UI with the daemon, and a hard kill of the
/// daemon leaves no UI: its sentinel stops it.
#[test]
fn a_sigterm_and_a_hard_kill_leave_no_ui() {
    let mut bench = Bench::new();
    let port = free_port();
    bench.ui_port = Some(port);
    bench.start();
    let (first, _) = bench.ui(port, None);
    bench.signal("TERM", bench.daemon_pid());
    assert!(bench.ended().success(), "{}", bench.output());
    bench.ui_gone(port, first);
    bench.start();
    let (second, _) = bench.ui(port, Some(first));
    bench.signal("KILL", bench.daemon_pid());
    let _ = bench.ended();
    bench.ui_gone(port, second);
}

/// Whether the process `pid` holds a socket on `port` of 127.0.0.1: `None`
/// when this system offers no way to tell.
fn holds_socket(pid: u32, port: u16) -> Option<bool> {
    match Command::new("lsof")
        .args(["-a", "-p", &pid.to_string(), &format!("-iTCP:{port}")])
        .stderr(Stdio::null())
        .output()
    {
        Ok(output) => return Some(!output.stdout.is_empty()),
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => panic!("lsof: {error}"),
        Err(_) => {}
    }
    // Linux without lsof: the listening socket's inode in /proc/net/tcp,
    // then the process's descriptors.
    let table = fs::read_to_string("/proc/net/tcp").ok()?;
    let local = format!("0100007F:{port:04X}");
    let inode = table.lines().skip(1).find_map(|line| {
        let fields: Vec<&str> = line.split_whitespace().collect();
        (fields.get(1) == Some(&local.as_str()) && fields.get(3) == Some(&"0A"))
            .then(|| fields.get(9).map(|inode| (*inode).to_owned()))
            .flatten()
    })?;
    let wanted = format!("socket:[{inode}]");
    let descriptors = fs::read_dir(format!("/proc/{pid}/fd")).ok()?;
    Some(descriptors.filter_map(Result::ok).any(|entry| {
        fs::read_link(entry.path()).is_ok_and(|target| target.to_string_lossy() == wanted)
    }))
}

/// OWL-467: the daemon's copies of the UI's socket are close-on-exec: a
/// watch started while it holds the socket, and its step, hold none; the
/// UI does.
#[test]
fn a_watch_holds_no_socket_of_the_ui() {
    let mut bench = Bench::new();
    let port = free_port();
    bench.ui_port = Some(port);
    bench.serve(WEB);
    bench.start();
    let (first, _) = bench.running(WEB);
    let (ui, _) = bench.ui(port, None);
    // The first watch may have started before the bind: kill it, so the
    // next starts while the daemon holds the socket.
    let started = bench.watches(WEB).len();
    bench.signal("KILL", first);
    bench.wait_for("the watch to start again", || {
        bench.watches(WEB).len() > started
    });
    let (watch, step) = bench.running(WEB);
    if let Some(held) = holds_socket(ui, port) {
        assert!(held, "the UI holds its socket");
        assert_eq!(holds_socket(watch, port), Some(false), "the watch");
        assert_eq!(holds_socket(step, port), Some(false), "its step");
    }
    bench.signal("TERM", bench.daemon_pid());
    assert!(bench.ended().success(), "{}", bench.output());
    bench.ui_gone(port, ui);
}

// ---- The real web UI on the bench (OWL-472) ----

/// A response of the real UI: its status and body.
fn ui_request(
    port: u16,
    method: &str,
    path: &str,
    session: Option<&str>,
    body: &str,
) -> Option<(u16, String)> {
    use std::io::{Read, Write};
    let mut stream = TcpStream::connect(("127.0.0.1", port)).ok()?;
    stream
        .set_read_timeout(Some(Duration::from_secs(60)))
        .ok()?;
    let mut text =
        format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n");
    if let Some(session) = session {
        text.push_str(&format!("X-Owlshift-Session: {session}\r\n"));
    }
    if method == "POST" {
        text.push_str(&format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            body.len()
        ));
    }
    text.push_str("\r\n");
    text.push_str(body);
    stream.write_all(text.as_bytes()).ok()?;
    let mut answer = Vec::new();
    stream.read_to_end(&mut answer).ok()?;
    let split = answer.windows(4).position(|four| four == b"\r\n\r\n")?;
    let head = String::from_utf8_lossy(&answer[..split]).into_owned();
    let status = head.split(' ').nth(1)?.parse().ok()?;
    Some((
        status,
        String::from_utf8_lossy(&answer[split + 4..]).into_owned(),
    ))
}

const KEPT: &str = r#"requires = ">=0.0"

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

/// The lines of `owlshift status` the Orchestrator shows, read as the command
/// reads them: the files, then the daemon's events for each watch.
fn status_lines(data: &Path) -> Vec<String> {
    use owlshift_runner::events::EventLog;
    use owlshift_runner::status::{self, Daemon};
    let mut report = status::gather_files(
        data,
        owlshift_contracts::config::DEFAULT_CONCURRENT_RUNS,
        &[],
        Timestamp::now(),
    );
    let (events, _) = owlshift_runner::why::read_events(EventLog::in_dir(data).path()).unwrap();
    if let Daemon::Running(record) = &report.daemon {
        let since = record
            .as_ref()
            .map_or(Timestamp::MIN, |record| record.since);
        for project in &mut report.projects {
            project.watch = status::watch_state(&events, &project.repository, since);
        }
    }
    status::parts(&report, &jiff::tz::TimeZone::system(), Voice::Ui)
        .into_iter()
        .filter(|(part, _)| part.on_the_page())
        .map(|(_, line)| line)
        .collect()
}

/// OWL-472: with the bench daemon serving the real UI, each screen answers
/// what its command prints for the bench's data directory: `status`'s lines
/// (the kinds the Orchestrator leaves to other screens named), `logs`'s lines
/// for the decisions and warnings, `doctor --json`, `config show`, and the
/// sums of the usage events.
#[test]
fn the_real_ui_answers_what_the_commands_print_for_the_bench() {
    use owlshift_contracts::service::{KeptFile, ServedConfig};
    use owlshift_runner::config::{Effective, PROJECT_FILE};
    use owlshift_runner::events::{self, EventLog};
    use owlshift_runner::ui::auth;
    use owlshift_testkit::system::ScriptedSystem;

    let mut bench = Bench::new();
    let port = free_port();
    bench.ui_port = Some(port);
    bench.ui_real = true;
    bench.serve(WEB);
    bench.start();
    let data = bench.data();
    bench.wait_for("the real UI to answer", || {
        ui_request(port, "GET", "/", None, "").is_some_and(|(status, _)| status == 200)
    });
    let (_, _) = bench.running(WEB);

    // The bench's data: the project file the project runs on, in its record
    // and, as the CLI reads it, in its checkout; a machine pause; a
    // quarantine; a usage window; a daemon log; events.
    let dirs = bench.dirs(WEB);
    let mut record = ServedConfig::new(WEB);
    record.kept = Some(KeptFile {
        branch: "main".to_owned(),
        commit: "d".repeat(40),
        text: KEPT.to_owned(),
        since: "2026-11-01T00:00:00Z".parse().unwrap(),
    });
    fs::create_dir_all(dirs.root()).unwrap();
    fs::write(served::record_file(&dirs), record.render()).unwrap();
    // A git repository with no ticket ref, as a dedicated checkout is before
    // its first run: `status` reads its refs and finds none.
    fs::create_dir_all(dirs.checkout()).unwrap();
    let init = Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(dirs.checkout())
        .status()
        .unwrap();
    assert!(init.success());
    fs::write(dirs.checkout().join(PROJECT_FILE), KEPT).unwrap();
    requests::ask_pause(
        &data,
        None,
        &Request::new(BY_STOP, Timestamp::now(), "a bench pause"),
    )
    .unwrap();
    fs::write(
        dirs.quarantine_file(),
        "\nRun r1 of OWL-1 broke isolation:\n- x\n",
    )
    .unwrap();
    fs::write(
        daemon::service_dir(&data).join("daemon.log"),
        "daemon line 1\ndaemon line 2\n",
    )
    .unwrap();
    let log = EventLog::in_dir(&data).path().to_owned();
    let mut lines = fs::read_to_string(&log).unwrap_or_default();
    let line = |at: &str, ticket: &str, run: &str, kind: &str, data: &str| {
        format!(
            "{{\"format\":2,\"at\":\"{at}\",\"project\":\"{WEB}\",\"ticket\":\"{ticket}\",\
             \"run\":\"{run}\",\"kind\":\"{kind}\",\"data\":{data}}}\n"
        )
    };
    let stamp = |ago: i64| {
        Timestamp::now()
            .checked_sub(jiff::SignedDuration::from_hours(ago))
            .unwrap()
            .to_string()
    };
    lines.push_str(&line(
        &stamp(3),
        "OWL-1",
        "r1",
        "run_started",
        r#"{"role":"build","harness":"claude"}"#,
    ));
    lines.push_str(&line(
        &stamp(3),
        "OWL-1",
        "r1",
        "usage",
        r#"{"cost_usd":1.25,"input_tokens":1,"output_tokens":1}"#,
    ));
    lines.push_str(&line(
        &stamp(2),
        "OWL-1",
        "r2",
        "run_started",
        r#"{"role":"verify"}"#,
    ));
    lines.push_str(&line(
        &stamp(2),
        "OWL-1",
        "r2",
        "usage",
        r#"{"cost_usd":0.5,"input_tokens":1,"output_tokens":1}"#,
    ));
    lines.push_str(&line(
        &stamp(1),
        "OWL-1",
        "r2",
        "decision",
        r#"{"note":"a decision <b>"}"#,
    ));
    lines.push_str(&line(
        &stamp(1),
        "OWL-1",
        "r2",
        "warning",
        r#"{"note":"a warning"}"#,
    ));
    fs::write(&log, lines).unwrap();

    // A session: the page trades a code for it.
    let token = token::read_token(&data).unwrap();
    let code = auth::mint_code(&token, now_ms()).unwrap();
    let (status, body) = ui_request(
        port,
        "POST",
        "/api/login",
        None,
        &serde_json::json!({ "code": code }).to_string(),
    )
    .unwrap();
    assert_eq!(status, 200, "{body}");
    let login: serde_json::Value = serde_json::from_str(&body).unwrap();
    let session = format!(
        "{}.{}",
        login["id"].as_str().unwrap(),
        login["secret"].as_str().unwrap()
    );
    let get = |path: &str| -> serde_json::Value {
        let (status, body) = ui_request(port, "GET", path, Some(&session), "").unwrap();
        assert_eq!(status, 200, "{path}: {body}");
        serde_json::from_str(&body).unwrap()
    };

    // The Orchestrator. The stand-in watches keep writing events, so the
    // comparison waits for a steady bench: the route and the command agree.
    let mut orchestrator = serde_json::Value::Null;
    bench.wait_for("the Orchestrator to show what status shows", || {
        orchestrator = get("/api/orchestrator");
        let shown: Vec<String> = orchestrator["lines"]
            .as_array()
            .unwrap()
            .iter()
            .map(|line| line.as_str().unwrap().to_owned())
            .collect();
        let want = status_lines(&data);
        shown == want
    });
    let text = orchestrator["lines"].to_string();
    assert!(
        text.contains("Paused: every project of this machine"),
        "{text}"
    );
    // The real UI names its Resume button for the machine's pause, not the
    // command (OWL-529): the Orchestrator passes the UI's voice.
    assert!(text.contains("its Resume button lifts it."), "{text}");
    assert!(!text.contains("owlshift resume"), "{text}");
    assert!(
        text.contains("quarantined: Run r1 of OWL-1 broke isolation"),
        "{text}"
    );
    assert_eq!(
        orchestrator["left_out"],
        serde_json::json!([
            "waiting",
            "parked",
            "merges",
            "drain_steps",
            "slots",
            "digest"
        ])
    );
    // The feed: the lines `owlshift logs` prints for the decisions and warnings.
    let (all, _) = owlshift_runner::why::read_events(&log).unwrap();
    let printed: Vec<String> = all
        .iter()
        .filter(|event| matches!(event.kind, EventKind::Decision | EventKind::Warning))
        .map(events::format_line)
        .collect();
    let shown: Vec<String> = orchestrator["events"]["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["line"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(shown, printed);
    // The daemon's log.
    assert_eq!(
        orchestrator["log"]["lines"],
        serde_json::json!(["daemon line 1", "daemon line 2"])
    );

    // Doctor and Config, as the command line builds them in the checkout.
    let scripted = ScriptedSystem::bench(&data, &data.join("bench-checkout"), "bench/none");
    let personal = Some(data.join("bench-personal.toml"));
    let checkout = dirs.checkout();
    let effective = Effective::load(&scripted, &checkout, personal);
    let build = BuildIdentity::from_build("bench", "0", "2026-01-01T00:00:00Z", "");
    let want = owlshift_runner::doctor::run(&scripted, &effective, &build).to_json();
    let (status, body) = ui_request(port, "POST", "/api/doctor", Some(&session), "{}").unwrap();
    assert_eq!(status, 200, "{body}");
    let doctor: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(doctor["reports"][0]["project"], WEB);
    assert_eq!(doctor["reports"][0]["report"], want);
    let config = get("/api/config");
    let shown: Vec<String> = config["projects"][0]["config"]["lines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|line| line.as_str().unwrap().to_owned())
        .collect();
    let want: Vec<String> = effective.to_string().lines().map(str::to_owned).collect();
    assert_eq!(shown, want);

    // Spend: the sums of the usage events, split by role and harness.
    let spend = get("/api/spend?days=all");
    assert_eq!(spend["events"]["total_micros"], 1_750_000);
    assert_eq!(spend["roles"][0]["role"], "build");
    let harnesses: Vec<(String, u64)> = spend["harnesses"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| {
            (
                h["harness"].as_str().unwrap().to_owned(),
                h["micros"].as_u64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        harnesses,
        [
            ("claude".to_owned(), 1_250_000),
            ("unrecorded".to_owned(), 500_000)
        ]
    );

    bench.signal("TERM", bench.daemon_pid());
    assert!(bench.ended().success(), "{}", bench.output());
}

fn now_ms() -> u64 {
    owlshift_runner::ui::server::now_ms()
}
