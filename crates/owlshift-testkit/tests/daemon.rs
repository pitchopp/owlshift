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
#![cfg(unix)]

use std::collections::BTreeMap;
use std::fs;
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
use owlshift_runner::watch;
use owlshift_testkit::step_child::{
    ChildProject, REPO, build_asks, ended_as, events, holding_harness, intake_declaring, reply_for,
};

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
}

impl Bench {
    fn new() -> Self {
        Self {
            tmp: tempfile::tempdir().unwrap(),
            daemon: None,
            project: None,
            build: None,
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
        self.wait_for(&format!("a watch of {repo}"), 10, || {
            seen = self
                .watches(repo)
                .into_iter()
                .rev()
                .find(|(watch, _)| alive(*watch));
            seen.is_some()
        });
        seen.unwrap()
    }

    fn wait_for(&self, what: &str, seconds: u64, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(seconds);
        while !done() {
            assert!(
                Instant::now() < deadline,
                "still waiting for {what}\n{}",
                self.output()
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// The daemon's end, within `seconds`.
    fn ended(&mut self, seconds: u64) -> ExitStatus {
        let daemon = self.daemon.as_mut().unwrap();
        let deadline = Instant::now() + Duration::from_secs(seconds);
        loop {
            if let Some(status) = daemon.try_wait().unwrap() {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "the daemon still runs\n{}",
                self.output()
            );
            thread::sleep(Duration::from_millis(20));
        }
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
    bench.wait_for("the daemon to say acme/api is held", 5, || {
        bench
            .output()
            .contains("acme/api: a watch typed in a terminal runs on it")
    });
    assert!(bench.watches(API).is_empty(), "{}", bench.output());

    drop(terminal);
    let (api, _) = bench.running(API);
    assert!(served::served_now(&bench.dirs(API)).unwrap());

    bench.registry().forget(&Repo::parse(WEB).unwrap()).unwrap();
    bench.wait_for("acme/web's watch to drain", 10, || !alive(web));
    bench.wait_for("acme/web to be let go", 5, || {
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
    assert!(bench.ended(10).success(), "{}", bench.output());
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
        bench.wait_for("the watch to start again", 10, || {
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
    assert!(bench.ended(10).success(), "{}", bench.output());
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
    let _ = bench.ended(5);
    let pids: Vec<u32> = started
        .iter()
        .flat_map(|(watch, step)| [*watch, *step])
        .collect();
    bench.wait_for("every watch and step to be gone", 5, || {
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
    let status = bench.ended(10);
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
    let status = bench.ended(15);
    assert!(status.success(), "{status}\n{}", bench.output());
    assert!(
        sent.elapsed() >= Duration::from_secs(2),
        "the hung watch held the daemon until its bound"
    );
    for (watch, step) in watches {
        bench.wait_for("the watches and their steps to be gone", 5, || {
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
    bench.wait_for("the pause", 5, || requests::pause(&dirs).is_some());
    let pause = requests::pause(&dirs).unwrap().unwrap();
    assert_eq!(pause.by, BY_DAEMON);
    assert_eq!(pause.why, "acme/web cannot be served on the bench");
    // Tried again at the next passes, told once.
    bench.wait_for("two more tries", 10, || {
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
    bench.wait_for("the pause to be lifted", 10, || {
        requests::pause(&dirs).is_none()
    });
    assert!(alive(watch), "{}", bench.output());
    assert!(bench.output().contains("acme/web is served again"));
    bench.signal("TERM", bench.daemon_pid());
    assert!(bench.ended(10).success(), "{}", bench.output());
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
    bench.wait_for("the wait told", 10, || {
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
    bench.wait_for("a pass", 10, || bench.passes() > 0);
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
    assert!(bench.ended(20).success(), "{}", bench.output());
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
    bench.wait_for("the pause", 10, || requests::pause(&dirs).is_some());
    let pause = requests::pause(&dirs).unwrap().unwrap();
    assert_eq!(pause.by, BY_DAEMON);
    assert!(pause.why.contains("invalid_client"), "{}", pause.why);
    assert!(!bench.output().contains("waits for the network"));
    bench.signal("TERM", bench.daemon_pid());
    assert!(bench.ended(20).success(), "{}", bench.output());
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
        "exec sleep 60",
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
        bench.wait_for("both Builds under way", 120, || {
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
    let status = bench.ended(60);
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
    let _ = bench.ended(10);
    bench.wait_for("the cut runs to be gone", 10, || {
        runs.iter().all(|run| !alive(*run))
    });
    assert!(dirs.obligations_stand(), "{}", bench.output());
    for file in &pid_files {
        fs::remove_file(file).unwrap();
    }
    bench.start();
    let found = quarantine::found_line(REPO);
    bench.wait_for("the quarantine, notified", 60, || {
        dirs.quarantine().unwrap().is_some() && bench.notes().contains(&found)
    });
    // Three passes more, none of which may start a run.
    let passes = bench.passes();
    bench.wait_for("three more passes of the watch", 30, || {
        bench.passes() >= passes + 3
    });
    bench.signal("TERM", bench.daemon_pid());
    let status = bench.ended(60);
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
/// - its run is stopped within 5 seconds and checked, its obligation
///   retired, nothing quarantined, while DEMO-2's runs on and the watch
///   goes on passing;
/// - DEMO-1 is parked at Build within 10 seconds, the PARKED comment naming
///   `owlshift cancel` and why, the cancel file gone;
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
        "exec sleep 60",
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
    bench.wait_for("both Builds under way", 120, || {
        pid_files.iter().all(|file| file.exists())
    });
    let (one, two) = (pid(&pid_files[0]), pid(&pid_files[1]));

    let asked = Instant::now();
    let why = "it rewrites the wrong module";
    cancel(&project, "DEMO-1", why);
    bench.wait_for("DEMO-1's run stopped", 5, || !alive(one));
    bench.wait_for("DEMO-1 parked", 10, || parked_at_build(&project, "DEMO-1"));
    assert!(asked.elapsed() < Duration::from_secs(10));
    assert!(alive(two), "DEMO-2's run was stopped\n{}", bench.output());
    let passes = bench.passes();
    bench.wait_for("three more passes of the watch", 30, || {
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
    bench.wait_for("DEMO-1's Build resumed", 60, || pid_files[0].exists());
    assert_eq!(
        build_starts(&bench.data(), "DEMO-1"),
        [false, true],
        "{}",
        bench.output()
    );
    bench.signal("TERM", bench.daemon_pid());
    let status = bench.ended(60);
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
    bench.wait_for("both Builds under way", 120, || {
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
    bench.wait_for("the watch to drain on its stop file", 30, || {
        events(&bench.data()).iter().any(|event| {
            event.data.get("watch") == Some(&serde_json::json!("stop"))
                && event.data.get("asked") == Some(&serde_json::json!("stop_file"))
        })
    });
    fs::remove_file(plays.join("hold")).unwrap();
    let status = bench.ended(60);
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
    let status = bench.ended(10);
    assert_eq!(status.code(), Some(75), "{status}\n{}", bench.output());
    assert!(!alive(watch) && !alive(step));
    assert_eq!(bench.watches(WEB).len(), 1, "nothing started again");
    assert!(requests::drain_asked(&data).is_none());
    assert!(!watch::stop::stands(&bench.dirs(WEB)));

    // The watch holds its drain while `linger` stands, as for a step.
    let linger = bench.bench_dir(WEB).join("linger");
    let lingering = |bench: &Bench, times: usize| {
        bench.wait_for("the watch to hold its drain", 10, || {
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
    let status = bench.ended(10);
    assert!(status.success(), "{status}\n{}", bench.output());
    assert!(requests::drain_asked(&data).is_none());
    assert!(requests::stop_asked(&data).is_none());

    fs::write(&linger, "").unwrap();
    bench.start();
    bench.running(WEB);
    requests::ask_drain(&data, &upgrade_drain()).unwrap();
    lingering(&bench, 2);
    bench.signal("TERM", bench.daemon_pid());
    let status = bench.ended(15);
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
