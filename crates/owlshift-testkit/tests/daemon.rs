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

fn daemon_bench() -> &'static Path {
    owlshift_testkit::binary!("owlshift-daemon-bench")
}
/// The step's child the daemon bench's real watch starts.
fn step_child_binary() -> &'static Path {
    owlshift_testkit::binary!("owlshift-step-child")
}
/// The fake harness.
fn fake() -> &'static Path {
    owlshift_testkit::binary!("owlshift-fake-harness")
}

/// The bench's timings, in milliseconds: a pass a second, a look every
/// 50 ms, restarts after 300 ms doubling up to 1.2 s, reset after 5 s.
const TIMING: &str = "1000,50,300,1200,5000";

/// [`TIMING`] with a pass of 5 s, for the tests of a paused project
/// (OWL-667). The daemon lifts a pause once a watch tried again still runs
/// a pass after its start, so a watch that cannot serve must start and end
/// within a pass: a bench stand-in took 3.9 s once, while three full test
/// runs shared the machine, and a 1 s pass then lifted and paused again.
const PAUSE_TIMING: &str = "5000,50,300,1200,5000";

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
    /// The copy of the bench's binary the daemon is started from, which a
    /// test replaces (OWL-616); none: the bench's binary itself.
    program: Option<PathBuf>,
    /// The service's launcher, run as its manager would run it (OWL-616);
    /// none: the daemon is started directly.
    launcher: Option<PathBuf>,
    /// The daemon's timings: [`TIMING`] unless a test sets others.
    timing: &'static str,
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
            program: None,
            launcher: None,
            timing: TIMING,
        }
    }

    /// The path the daemon is started from, once [`Bench::install_binary`]
    /// put a copy there.
    fn binary_path(&self) -> PathBuf {
        self.tmp.path().join("bin").join("owlshift-daemon-bench")
    }

    /// Puts a copy of the bench's binary at [`Bench::binary_path`], a build
    /// of the commit `commit` repeated made at `built_at` seconds, and starts
    /// the daemon from it from now on (OWL-616).
    fn install_binary(&mut self, commit: char, built_at: i64) -> PathBuf {
        fs::create_dir_all(self.binary_path().parent().unwrap()).unwrap();
        self.replace(commit, built_at);
        self.program = Some(self.binary_path());
        self.binary_path()
    }

    /// Replaces the binary at [`Bench::binary_path`] as an installer does: a
    /// new copy, beside its build, renamed over it (OWL-616).
    fn replace(&self, commit: char, built_at: i64) {
        let path = self.binary_path();
        let staged = path.with_file_name("staged");
        owlshift_testkit::stand_in::copy_executable(daemon_bench(), &staged);
        let build = path.with_file_name("owlshift-daemon-bench.build");
        let partial = path.with_file_name("build.partial");
        fs::write(
            &partial,
            format!("{} {built_at}", commit.to_string().repeat(40)),
        )
        .unwrap();
        fs::rename(&partial, &build).unwrap();
        fs::rename(&staged, &path).unwrap();
    }

    /// Replaces the binary at [`Bench::binary_path`] by a script that does
    /// not answer `--version` (OWL-616).
    fn replace_with_failing_script(&self) {
        let path = self.binary_path();
        let staged = path.with_file_name("staged");
        owlshift_testkit::stand_in::write_executable(&staged, "#!/bin/sh\nexit 3\n");
        fs::rename(&staged, &path).unwrap();
    }

    /// The record of the daemon that runs, when one runs and its record
    /// reads.
    fn record(&self) -> Option<owlshift_contracts::service::DaemonRecord> {
        daemon::running(&self.data()).ok().flatten().flatten()
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
        let mut daemon = match (&self.launcher, &self.program) {
            (Some(launcher), _) => {
                let mut command = Command::new("/bin/sh");
                command.arg(launcher);
                command
            }
            (None, Some(program)) => {
                let mut command = Command::new(program);
                command.arg("daemon");
                command
            }
            (None, None) => {
                let mut command = Command::new(daemon_bench());
                command.arg("daemon");
                command
            }
        };
        daemon
            .env("OWLSHIFT_DATA_DIR", self.data())
            .env("OWLSHIFT_BENCH_TIMING", self.timing)
            .env("OWLSHIFT_BENCH_NOTES", self.notes_file())
            .stdin(Stdio::null())
            .stdout(output("out"))
            .stderr(output("err"));
        if let Some(project) = &self.project {
            daemon
                .env("OWLSHIFT_BENCH_PROJECT", project)
                .env("OWLSHIFT_BENCH_STEP_CHILD", step_child_binary());
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

    /// The daemon's output so far, for a failure: the service's launcher
    /// appends it to the daemon's log (OWL-616).
    fn output(&self) -> String {
        let read = |path: PathBuf| fs::read_to_string(path).unwrap_or_default();
        let at = |name: &str| read(self.tmp.path().join(name));
        let log = read(daemon::service_dir(&self.data()).join("daemon.log"));
        format!(
            "--- out\n{}--- err\n{}--- daemon.log\n{log}",
            at("out"),
            at("err")
        )
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
        self.wait_through(1, what, done);
    }

    /// [`Bench::wait_for`] a state reached through `steps` steps one after
    /// the other ([`wait::through`]), such as the daemon's start, its watch's,
    /// each intake and each Build's start (OWL-667).
    fn wait_through(&self, steps: u32, what: &str, done: impl FnMut() -> bool) {
        wait::through(steps, what, || self.output(), done);
    }

    /// [`Bench::wait_for`] within `bound`, which only a bound the design sets
    /// justifies: the caller cites it.
    fn wait_within(&self, what: &str, bound: Duration, done: impl FnMut() -> bool) {
        wait::within(bound, what, || self.output(), done);
    }

    /// Waits until the daemon says it started `pid`, a watch or the web UI,
    /// which it says once its sentinel was told of it (OWL-671). A hard kill
    /// before then leaves that process running, the sentinel's window
    /// between a start and its line (`stop_trees_when_killed`): load once
    /// held the daemon there while its UI started and answered, and the UI
    /// outlived the kill.
    fn told_its_sentinel_of(&self, pid: u32) {
        let said = format!("(pid {pid}).");
        self.wait_for(&format!("the daemon to say it started {pid}"), || {
            self.output()
                .lines()
                .any(|line| line.starts_with("Started the ") && line.ends_with(&said))
        });
    }

    /// The daemon's end.
    fn ended(&mut self) -> ExitStatus {
        self.ended_through(1)
    }

    /// [`Bench::ended`], reached through `steps` steps one after the other,
    /// such as the steps under way stopped, the watch's end and the daemon's
    /// (OWL-667).
    fn ended_through(&mut self, steps: u32) -> ExitStatus {
        let mut status = None;
        let daemon = self.daemon.as_mut().unwrap();
        let ended = wait::reached(wait::REACHED * steps, || {
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

    /// Waits until the UI `pid` is gone and its port free. The UI is gone
    /// once no process runs at its pid as the UI's stand-in ([`ui_runs`]),
    /// not once `kill -0` fails: that finds a zombie, and a pid another
    /// process took meanwhile (OWL-671). A timeout says which half is
    /// missing, what runs at the pid and who listens on the port.
    fn ui_gone(&self, port: u16, pid: u32) {
        wait::until_with(
            "the web UI to be gone and its port free",
            || format!("{}\n{}", ui_diagnosis(port, pid), self.output()),
            || !ui_runs(pid) && TcpListener::bind(("127.0.0.1", port)).is_ok(),
        );
    }
}

/// Whether the UI's stand-in runs at `pid`: `ps` shows a process there that
/// is not a zombie, whose command is the stand-in's (`ui --listener-fd`). On
/// macOS `kill -0` finds a zombie until launchd reads its end, and under load
/// the pid space went round every 20 s, so a pid can be another process's
/// (OWL-671). A `ps` that fails otherwise than by finding no process tells
/// nothing: the UI counts as running, and the wait goes on.
fn ui_runs(pid: u32) -> bool {
    let output = Command::new("ps")
        .args(["-ww", "-o", "stat=,command=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    let row = String::from_utf8_lossy(&output.stdout);
    let row = row.trim();
    if row.is_empty() {
        return !output.stderr.is_empty();
    }
    !row.starts_with('Z') && row.contains(" ui --listener-fd ")
}

/// For a wait on the UI's end that timed out (OWL-671): which half is
/// missing, what `ps` says of `pid`, and who listens on `port` as `lsof`
/// says, or why it could not.
fn ui_diagnosis(port: u16, pid: u32) -> String {
    let run = |program: &str, args: &[&str]| match Command::new(program).args(args).output() {
        Ok(output) => format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ),
        Err(error) => format!("{program}: {error}\n"),
    };
    let bind = match TcpListener::bind(("127.0.0.1", port)) {
        Ok(_) => "free".to_owned(),
        Err(error) => format!("not free: {error}"),
    };
    let runs = ui_runs(pid);
    let pid = pid.to_string();
    format!(
        "the UI runs: {runs}; port {port}: {bind}\n--- ps, pid {pid}\n{}--- lsof, port {port}\n{}",
        run(
            "ps",
            &["-ww", "-o", "pid,ppid,stat,lstart,command", "-p", &pid]
        ),
        run("lsof", &["-nP", &format!("-iTCP:{port}"), "-sTCP:LISTEN"]),
    )
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
        // A daemon the test's own handed its lock to (OWL-616).
        if let Some(record) = self.record() {
            let _ = Command::new("kill")
                .args(["-s", "KILL", &record.pid.to_string()])
                .status();
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
    for _ in 0..3 {
        let (watch, _) = bench.running(WEB);
        let started = bench.watches(WEB).len();
        bench.signal("KILL", watch);
        bench.wait_for("the watch to start again", || {
            bench.watches(WEB).len() > started
        });
    }
    // Each restart timed by the daemon's own events (OWL-667): from its
    // record of the end, written once it found the watch dead and chose the
    // delay, to its record of the next start. A clock in the test counts
    // `kill`'s own time, which load stretches: from `kill`'s end, a next start
    // came 139 ms after it on a 600 ms delay; from before the signal, a
    // daemon that started again at once could pass.
    let gaps = || {
        let mut ended: Option<(Timestamp, u64)> = None;
        let mut gaps = Vec::new();
        for event in events(&bench.data()) {
            match event.data.get("service").and_then(|s| s.as_str()) {
                Some("watch_ended") => {
                    let delay = event.data.get("restart_in_ms").and_then(|d| d.as_u64());
                    ended = delay.map(|delay| (event.at, delay));
                }
                Some("watch_started") => {
                    if let Some((at, delay)) = ended.take() {
                        gaps.push((event.at.duration_since(at), delay));
                    }
                }
                _ => {}
            }
        }
        gaps
    };
    bench.wait_for("the daemon's records of the three starts again", || {
        gaps().len() == 3
    });
    let gaps = gaps();
    // 300 ms, 600 ms, 1.2 s, each chosen and each waited at least.
    assert_eq!(
        gaps.iter().map(|(_, delay)| *delay).collect::<Vec<_>>(),
        [300, 600, 1200],
        "{gaps:?}\n{}",
        bench.output()
    );
    for (gap, delay) in &gaps {
        assert!(
            *gap >= jiff::SignedDuration::from_millis(i64::try_from(*delay).unwrap()),
            "{gaps:?}\n{}",
            bench.output()
        );
    }
    assert!(gaps[2].0 > gaps[0].0, "{gaps:?}");
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
    for (watch, _) in started {
        bench.told_its_sentinel_of(watch);
    }
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
/// with the reason, one notification that it waits for you however often
/// it is tried again at each pass, and the pause said once (OWL-650): the
/// quiet retries add nothing to the output while the reason stays the same.
/// Once a watch so tried still runs a pass later, the pause is lifted. Its
/// passes are [`PAUSE_TIMING`]'s.
#[test]
fn a_watch_that_cannot_serve_pauses_its_project_until_one_serves() {
    let mut bench = Bench::new();
    bench.timing = PAUSE_TIMING;
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
    // Tried again at the next passes, quietly, told once.
    let unservable = || {
        events(&bench.data())
            .iter()
            .filter(|event| {
                event.data.get("service").and_then(|s| s.as_str()) == Some("watch_ended")
                    && event.data.get("unservable").is_some()
            })
            .count()
    };
    bench.wait_for("two more tries", || unservable() >= 3);
    let output = bench.output();
    for (line, said) in [
        ("Paused acme/web, waiting for you: ", 1),
        ("acme/web cannot be served on the bench", 2),
        ("Started the watch of acme/web", 1),
        ("Tried again", 0),
    ] {
        assert_eq!(output.matches(line).count(), said, "{line}: {output}");
    }
    let notes = bench.notes();
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(
        notes[0].starts_with(
            "Owlshift: acme/web waits for you: acme/web cannot be served on the bench"
        ),
        "{notes:?}"
    );
    assert!(!notes[0].contains("again"), "{notes:?}");

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
    // The lifted watch's held line is said, before the served-again one.
    let output = bench.output();
    let watching = output.find("watching acme/web").expect(&output);
    assert!(
        watching < output.find("acme/web is served again").unwrap(),
        "{output}"
    );
    bench.signal("TERM", bench.daemon_pid());
    assert!(bench.ended().success(), "{}", bench.output());
}

/// A paused project whose watch, tried again, cannot serve it for another
/// reason (OWL-650): the daemon writes its pause file again with the new
/// reason, says the pause once more with it and the retried watch's line,
/// then stays quiet while the new reason holds; the operator was notified
/// once, at the first pause. Its passes are [`PAUSE_TIMING`]'s.
#[test]
fn a_paused_project_whose_reason_changes_is_paused_again_with_it() {
    const FIRST: &str = "acme/web cannot be served on the bench";
    const NEXT: &str = "acme/web cannot reach GitHub on the bench";
    let mut bench = Bench::new();
    bench.timing = PAUSE_TIMING;
    bench.serve(WEB);
    fs::create_dir_all(bench.bench_dir(WEB)).unwrap();
    fs::write(
        bench.bench_dir(WEB).join("exit"),
        daemon::UNSERVABLE_EXIT.to_string(),
    )
    .unwrap();
    bench.start();
    let dirs = bench.dirs(WEB);
    let tries = |reason: &str| {
        events(&bench.data())
            .iter()
            .filter(|event| {
                event.data.get("service").and_then(|s| s.as_str()) == Some("watch_ended")
                    && event.data.get("unservable").and_then(|s| s.as_str()) == Some(reason)
            })
            .count()
    };
    bench.wait_for("the first pause, tried again", || tries(FIRST) >= 2);
    let pause = requests::pause(&dirs).unwrap().unwrap();
    assert_eq!((pause.by.as_str(), pause.why.as_str()), (BY_DAEMON, FIRST));

    fs::write(bench.bench_dir(WEB).join("why"), NEXT).unwrap();
    bench.wait_for("the new reason, tried again", || tries(NEXT) >= 3);
    let pause = requests::pause(&dirs).unwrap().unwrap();
    assert_eq!((pause.by.as_str(), pause.why.as_str()), (BY_DAEMON, NEXT));
    let output = bench.output();
    for (line, said) in [
        ("Paused acme/web, waiting for you: ", 2),
        (&format!("Paused acme/web, waiting for you: {FIRST}."), 1),
        (&format!("Paused acme/web, waiting for you: {NEXT}."), 1),
        // The retried watch's held line, on the standard error, once per
        // reason, and the pause that says it.
        (&format!("acme/web: owlshift: {FIRST}"), 1),
        (&format!("acme/web: owlshift: {NEXT}"), 1),
        (FIRST, 2),
        (NEXT, 2),
        ("Tried again", 0),
    ] {
        assert_eq!(output.matches(line).count(), said, "{line}: {output}");
    }
    let notes = bench.notes();
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(
        notes[0].starts_with(&format!("Owlshift: acme/web waits for you: {FIRST}")),
        "{notes:?}"
    );
    bench.signal("TERM", bench.daemon_pid());
    assert!(bench.ended().success(), "{}", bench.output());
}

/// A real watch of [`REPO`] under the daemon (OWL-479), on a project whose
/// drain admits nothing: what its start does, not its steps.
fn real_watch(bench: &mut Bench) {
    let root = bench.tmp.path().to_owned();
    let machine = root.join("machine");
    let mut project = ChildProject::seed_parallel(&root, fake().to_path_buf(), 0, 1, (&machine, 1));
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
///
/// OWL-651: once the credentials are mended, the watch tried again is never
/// held by the daemon's pause, so it dispatches at its first pass, and the
/// pause is lifted once its record says it is past its start, well within
/// the pass after which it would be lifted otherwise.
#[test]
fn a_refused_token_at_a_watchs_start_still_pauses_its_project() {
    let mut bench = Bench::new();
    real_watch(&mut bench);
    let refused = bench.bench_dir(REPO).join("refused");
    fs::write(&refused, "").unwrap();
    bench.start();
    let dirs = bench.dirs(REPO);
    bench.wait_for("the pause", || requests::pause(&dirs).is_some());
    let pause = requests::pause(&dirs).unwrap().unwrap();
    assert_eq!(pause.by, BY_DAEMON);
    assert!(pause.why.contains("invalid_client"), "{}", pause.why);
    assert!(!bench.output().contains("waits for the network"));

    fs::remove_file(&refused).unwrap();
    bench.wait_for("the pause lifted and said", || {
        requests::pause(&dirs).is_none() && bench.output().contains("is served again")
    });
    bench.wait_for("a pass of the watch and its lines", || {
        bench.passes() > 0 && bench.output().contains("watching demo/project")
    });
    let output = bench.output();
    assert!(!output.contains("this project is paused"), "{output}");
    let at = |service: &str| {
        events(&bench.data())
            .iter()
            .rev()
            .find(|event| event.data.get("service").and_then(|s| s.as_str()) == Some(service))
            .map(|event| event.at)
            .unwrap_or_else(|| panic!("no {service} event: {output}"))
    };
    // A pass of the bench is 1000 ms (`TIMING`): the lift came before it.
    let lifted_after = at("pause_lifted").duration_since(at("watch_started"));
    assert!(
        lifted_after < jiff::SignedDuration::from_millis(1000),
        "lifted {lifted_after:?} after the start: {output}"
    );
    let waits = bench
        .notes()
        .iter()
        .filter(|note| note.contains("waits for you"))
        .count();
    assert_eq!(waits, 1, "{:?}", bench.notes());
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
        fake(),
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
    // Both Builds under way, held, through `steps` steps: their runs' pids.
    let held = |bench: &Bench, steps: u32| -> Vec<u32> {
        bench.wait_through(steps, "both Builds under way", || {
            pid_files.iter().all(|file| file.exists())
        });
        pid_files
            .iter()
            .map(|file| fs::read_to_string(file).unwrap().trim().parse().unwrap())
            .collect()
    };

    // A checked stop. The daemon's start, its watch's, two intakes and two
    // Builds' starts (OWL-667).
    bench.start();
    let runs = held(&bench, 6);
    bench.signal("TERM", bench.daemon_pid());
    // Two Builds stopped and checked, the watch's end and the daemon's.
    let status = bench.ended_through(4);
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

    // Resumed from their checkpoints at the next start: the daemon's start,
    // its watch's and two Builds' resumes.
    for file in &pid_files {
        fs::remove_file(file).unwrap();
    }
    bench.start();
    let runs = held(&bench, 4);
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
    // The daemon's start and its watch's, which finds the quarantine.
    bench.wait_through(2, "the quarantine, notified", || {
        dirs.quarantine().unwrap().is_some() && bench.notes().contains(&found)
    });
    // Three passes more, none of which may start a run.
    let passes = bench.passes();
    bench.wait_through(3, "three more passes of the watch", || {
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
/// - at the park, DEMO-1's run slot is already free and DEMO-2's held, as
///   the web UI's header shows them (OWL-530);
/// - `owlshift continue --to-watch DEMO-1` restarts it, and the watch
///   resumes its Build from its checkpoint.
#[test]
fn a_canceled_build_is_stopped_checked_and_parked_then_resumed_from_its_checkpoint() {
    use owlshift_runner::on_demand::Stop;
    use owlshift_runner::status::gather_files;
    use owlshift_testkit::step_child::{cancel, parked_at_build};
    use serde_json::json;

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
        fake(),
        "build",
        CHECKPOINT,
        &format!("exec sleep {}", wait::OUTLASTS.as_secs()),
    );
    // The slots in the data directory the header reads (OWL-530).
    let mut project = ChildProject::seed_parallel(&root, harness, 2, 2, (&bench.data(), 2));
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
    // The daemon's start, its watch's, two intakes and two Builds' starts
    // (OWL-667).
    bench.wait_through(6, "both Builds under way", || {
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
    // The header read now, at the park (OWL-530). The watch's own probes
    // each hold a free slot for a moment, a slot then held with no holder
    // record: such a read alone is read again, at once, a few times.
    let mut report = gather_files(&bench.data(), 2, &[], Timestamp::now());
    for _ in 0..5 {
        let probed = report
            .slots
            .as_ref()
            .is_some_and(|(held, _)| held.iter().any(|slot| slot.holder.is_none()));
        if !probed {
            break;
        }
        report = gather_files(&bench.data(), 2, &[], Timestamp::now());
    }
    let header = owlshift_runner::ui::header::of(&report);
    let header = header.json();
    assert_eq!(
        header["slots"],
        json!({ "held": 1, "of": 2 }),
        "{}",
        bench.output()
    );
    assert_eq!(header["projects"][0]["slots_held"], 1, "{}", bench.output());
    assert_eq!(
        header["projects"][0]["slot_holders"],
        json!(["DEMO-2"]),
        "{}",
        bench.output()
    );
    assert_eq!(header["waiting"]["parked"], 1, "{}", bench.output());
    assert!(alive(two), "DEMO-2's run was stopped\n{}", bench.output());
    let passes = bench.passes();
    bench.wait_through(3, "three more passes of the watch", || {
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
    // The pass that dispatches it and its Build's start.
    bench.wait_through(2, "DEMO-1's Build resumed", || pid_files[0].exists());
    assert_eq!(
        build_starts(&bench.data(), "DEMO-1"),
        [false, true],
        "{}",
        bench.output()
    );
    bench.signal("TERM", bench.daemon_pid());
    // Two Builds stopped and checked, the watch's end and the daemon's.
    let status = bench.ended_through(4);
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
        fake(),
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
    // The daemon's start, its watch's, two intakes and two Builds' starts
    // (OWL-667).
    bench.wait_through(6, "both Builds under way", || {
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
        binary: daemon_bench(),
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
        upgrade::hand_over(&data, own, daemon_bench(), "owlshift start"),
        Ok(Handover::Ask(running))
    );
    bench.wait_for("the watch to drain on its stop file", || {
        events(&bench.data()).iter().any(|event| {
            event.data.get("watch") == Some(&serde_json::json!("stop"))
                && event.data.get("asked") == Some(&serde_json::json!("stop_file"))
        })
    });
    fs::remove_file(plays.join("hold")).unwrap();
    // Two Builds' ends, the watch's and the daemon's.
    let status = bench.ended_through(4);
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

/// Polls the daemon's lock of `data` every 10 ms until dropped, counting
/// the looks that found it free (OWL-616).
struct Prober {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<thread::JoinHandle<u32>>,
}

impl Prober {
    fn start(data: &Path) -> Self {
        use std::sync::atomic::Ordering;
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (asked, data) = (stop.clone(), data.to_owned());
        let thread = thread::spawn(move || {
            let mut free = 0;
            while !asked.load(Ordering::Relaxed) {
                if daemon::running(&data).unwrap().is_none() {
                    free += 1;
                }
                thread::sleep(Duration::from_millis(10));
            }
            free
        });
        Self {
            stop,
            thread: Some(thread),
        }
    }

    /// How many looks found the lock free.
    fn free(mut self) -> u32 {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        self.thread.take().unwrap().join().unwrap()
    }
}

/// The commit `commit` repeated, as the bench's sidecar names it.
fn commit(commit: char) -> Option<String> {
    Some(commit.to_string().repeat(40))
}

/// OWL-616 (architecture section 5, "An upgrade", "Without a command"): a
/// binary replaced at the daemon's path by the same build, then by a file
/// that does not answer `--version`, is told once each and changes nothing;
/// a newer one whose daemon fails at its start leaves this daemon serving,
/// its record back and its watch started again; a newer one during whose
/// drain a stop is asked ends the daemon with 0 and no successor.
#[test]
fn a_replaced_binary_not_newer_failing_or_stopped_leaves_the_daemon_as_it_was() {
    let mut bench = Bench::new();
    bench.serve(WEB);
    let data = bench.data();
    bench.install_binary('a', 1_761_600_000);
    bench.start();
    let (first, _) = bench.running(WEB);
    let pid = bench.daemon_pid();

    bench.replace('a', 1_761_600_000);
    bench.wait_for("the same build told", || {
        bench.output().contains("which does not supersede")
    });
    bench.replace_with_failing_script();
    bench.wait_for("the failing file told", || {
        bench.output().contains("did not tell its version")
    });
    assert!(requests::drain_asked(&data).is_none(), "{}", bench.output());
    assert!(alive(first), "{}", bench.output());

    let bench_dir = data.join("bench");
    fs::write(bench_dir.join("successor-fails"), "").unwrap();
    bench.replace('b', 1_761_600_600);
    bench.wait_for("the successor to fail", || {
        bench.output().contains("without taking over")
    });
    bench.wait_for("the daemon recorded again", || {
        bench.record().is_some_and(|record| record.pid == pid)
    });
    bench.wait_for("a new watch of the project", || {
        bench
            .watches(WEB)
            .iter()
            .any(|(watch, _)| *watch != first && alive(*watch))
    });
    fs::remove_file(bench_dir.join("successor-fails")).unwrap();

    let linger = bench.bench_dir(WEB).join("linger");
    fs::write(&linger, "").unwrap();
    bench.replace('c', 1_761_601_200);
    bench.wait_for("the watch to hold its drain", || {
        bench.output().contains("drains, its step under way")
    });
    requests::ask_stop(
        &data,
        &Request::new(BY_STOP, Timestamp::now(), "asked by the test"),
    )
    .unwrap();
    fs::remove_file(&linger).unwrap();
    let status = bench.ended();
    assert!(status.success(), "{status}\n{}", bench.output());
    assert!(daemon::running(&data).unwrap().is_none());
    let output = bench.output();
    // Each file asked once, though many passes went by; one handover tried.
    assert_eq!(
        output.matches("which does not supersede").count(),
        1,
        "{output}"
    );
    assert_eq!(
        output.matches("did not tell its version").count(),
        1,
        "{output}"
    );
    assert_eq!(output.matches("Handing the lock").count(), 1, "{output}");
    assert!(output.contains("A stop was asked during the upgrade's drain"));
}

/// OWL-616's acceptance without a service: a newer binary renamed over the
/// one the daemon was started from, while a step is under way, drains the
/// daemon, the step finishing; the daemon then hands its lock to that
/// binary's daemon, the lock never free, and ends 0; the new daemon records
/// the new build and serves the project. A signal asked of a daemon while
/// its successor starts is sent on, and both end.
#[test]
fn a_session_daemon_moves_onto_a_newer_binary_with_no_gap() {
    let mut bench = Bench::new();
    bench.serve(WEB);
    let data = bench.data();
    let path = bench.install_binary('a', 1_761_600_000);
    bench.start();
    let (watch, step) = bench.running(WEB);
    let first = bench.daemon_pid();
    let linger = bench.bench_dir(WEB).join("linger");
    fs::write(&linger, "").unwrap();
    let prober = Prober::start(&data);

    bench.replace('b', 1_761_600_600);
    bench.wait_for("the watch to hold its drain", || {
        bench.output().contains("drains, its step under way")
    });
    assert!(
        alive(step),
        "the step under way is not cut\n{}",
        bench.output()
    );
    fs::remove_file(&linger).unwrap();
    let status = bench.ended();
    assert!(status.success(), "{status}\n{}", bench.output());
    let mut moved = None;
    bench.wait_for("the new daemon's record", || {
        moved = bench.record().filter(|record| record.pid != first);
        moved.is_some()
    });
    assert_eq!(prober.free(), 0, "the lock was free\n{}", bench.output());
    let moved = moved.unwrap();
    assert_eq!(
        moved.build.as_ref().and_then(|build| build.commit.clone()),
        commit('b')
    );
    assert_eq!(moved.watches, Some(path.display().to_string()));
    bench.wait_for("the new daemon's watch", || {
        bench
            .watches(WEB)
            .iter()
            .any(|(pid, _)| *pid != watch && alive(*pid))
    });

    fs::write(data.join("bench/successor-waits"), "").unwrap();
    bench.replace('c', 1_761_601_200);
    bench.wait_for("the second handover to start", || {
        bench.output().matches("Handing the lock").count() == 2
    });
    bench.signal("TERM", moved.pid);
    bench.wait_for("both daemons to end", || {
        !alive(moved.pid) && daemon::running(&data).unwrap().is_none()
    });
    assert!(
        bench.output().contains("sent on to the new daemon"),
        "{}",
        bench.output()
    );
}

/// OWL-616's acceptance with a service: the daemon its launcher started
/// drains for a newer binary renamed over its own, the step under way
/// finishing, and exits 75; the launcher, started again as the manager does,
/// runs the new build.
#[test]
fn a_service_daemon_drains_then_exits_75_and_its_launcher_starts_the_new_binary() {
    let mut bench = Bench::new();
    bench.serve(WEB);
    let data = bench.data();
    let path = bench.install_binary('a', 1_761_600_000);
    let layout = Layout {
        kind: Kind::Systemd,
        dir: bench.tmp.path().join("units"),
    };
    install::install(&Install {
        data: &data,
        layout: &layout,
        binary: &path,
        config_dir: None,
        path: None,
    })
    .unwrap();
    bench.launcher = Some(layout.launcher());
    bench.start();
    let (_, step) = bench.running(WEB);
    let linger = bench.bench_dir(WEB).join("linger");
    fs::write(&linger, "").unwrap();

    bench.replace('b', 1_761_600_600);
    bench.wait_for("the watch to hold its drain", || {
        bench.output().contains("drains, its step under way")
    });
    assert!(
        alive(step),
        "the step under way is not cut\n{}",
        bench.output()
    );
    fs::remove_file(&linger).unwrap();
    let status = bench.ended();
    assert_eq!(status.code(), Some(75), "{status}\n{}", bench.output());
    assert!(
        !bench.output().contains("Handing the lock"),
        "{}",
        bench.output()
    );

    bench.start();
    bench.wait_for("the new build's record", || {
        bench
            .record()
            .is_some_and(|record| record.build.and_then(|build| build.commit) == commit('b'))
    });
    bench.running(WEB);
    requests::ask_stop(
        &data,
        &Request::new(BY_STOP, Timestamp::now(), "asked by the test"),
    )
    .unwrap();
    let status = bench.ended();
    assert!(status.success(), "{status}\n{}", bench.output());
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
        // The clock starts before the signal (OWL-672): after `kill` ended,
        // it missed what load added to `kill`'s own time, and a next start
        // on a 300 ms delay came 270 ms after it. From before, a correct
        // daemon is never measured short; one that started again at once
        // is caught where `kill` and a start take less than the delay, as
        // on a machine at rest.
        let killed = Instant::now();
        bench.signal("KILL", last);
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
    bench.told_its_sentinel_of(second);
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

/// The processes whose parent is `pid`, as `pgrep -P` lists them.
fn children(pid: u32) -> Vec<u32> {
    let output = Command::new("pgrep")
        .args(["-P", &pid.to_string()])
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout)
        .split_whitespace()
        .map(|child| child.parse().unwrap())
        .collect()
}

/// OWL-467: the daemon's copies of the UI's socket are close-on-exec: a
/// watch started while it holds the socket, and its step, hold none; the
/// UI does, and the sentinel the UI started does not (OWL-671).
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
        // Nor the UI's sentinel, which it starts before it adopts the socket
        // (OWL-671): it held it, and the port stayed bound until it ended.
        let children = children(ui);
        assert!(!children.is_empty(), "the UI's sentinel runs");
        for child in children {
            assert_eq!(
                holds_socket(child, port),
                Some(false),
                "the UI's child {child}"
            );
        }
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
    assert!(
        text.contains("the machine's Resume button lifts it."),
        "{text}"
    );
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
            "handed_back",
            "merges",
            "claims",
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
