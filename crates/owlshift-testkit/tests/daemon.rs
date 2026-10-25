//! OWL-435: the background service on the bench (architecture section 5,
//! "The background service"): the daemon `owlshift daemon` runs, with
//! stand-in watches (`owlshift-daemon-bench`) and short timings. Two served
//! projects each under its served lock, one kept off by a watch typed in a
//! terminal and one forgotten; a watch killed and started again with a
//! doubling delay, told at the third end; a hard kill of the daemon leaving
//! no watch or step; a stop drained and a SIGTERM checked, a hung watch
//! stopped by its bound; a watch that cannot serve pausing its project.
#![cfg(unix)]

use std::fs;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use jiff::Timestamp;
use owlshift_adapters::forge::Repo;
use owlshift_contracts::service::{BY_DAEMON, BY_STOP, Request};
use owlshift_runner::daemon::{self, requests};
use owlshift_runner::project::ProjectDirs;
use owlshift_runner::served::{self, Registry};
use owlshift_runner::watch;

const BENCH: &str = env!("CARGO_BIN_EXE_owlshift-daemon-bench");

/// The bench's timings, in milliseconds: a pass a second, a look every
/// 50 ms, restarts after 300 ms doubling up to 1.2 s, reset after 5 s.
const TIMING: &str = "1000,50,300,1200,5000";

const WEB: &str = "acme/web";
const API: &str = "acme/api";

struct Bench {
    tmp: tempfile::TempDir,
    daemon: Option<Child>,
}

impl Bench {
    fn new() -> Self {
        Self {
            tmp: tempfile::tempdir().unwrap(),
            daemon: None,
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

    fn start(&mut self) {
        fs::create_dir_all(self.data()).unwrap();
        let daemon = Command::new(BENCH)
            .arg("daemon")
            .env("OWLSHIFT_DATA_DIR", self.data())
            .env("OWLSHIFT_BENCH_TIMING", TIMING)
            .env("OWLSHIFT_BENCH_NOTES", self.notes_file())
            .stdin(Stdio::null())
            .stdout(fs::File::create(self.tmp.path().join("out")).unwrap())
            .stderr(fs::File::create(self.tmp.path().join("err")).unwrap())
            .spawn()
            .unwrap();
        self.daemon = Some(daemon);
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
