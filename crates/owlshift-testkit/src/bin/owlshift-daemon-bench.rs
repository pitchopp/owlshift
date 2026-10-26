//! The background service on the test bench (OWL-435):
//! `owlshift-daemon-bench daemon` runs the daemon `owlshift daemon` runs,
//! `owlshift_runner::daemon`, on the data directory `OWLSHIFT_DATA_DIR`,
//! with the timings `OWLSHIFT_BENCH_TIMING` gives (milliseconds: pass, tick,
//! first delay, longest delay, reset after), each notification appended to
//! the file `OWLSHIFT_BENCH_NOTES`, and as each served project's watch one
//! of this binary's two: the stand-in, `owlshift-daemon-bench watch
//! <OWNER/REPO>`, or, when `OWLSHIFT_BENCH_PROJECT` names a step child's
//! project file ([`ChildProject`]), a real watch, `owlshift-daemon-bench
//! drain demo/project` (OWL-441).
//!
//! The real watch is `owlshift watch --drain` on that project: the Markdown
//! tracker, the harness the file names, each step in a child,
//! `owlshift-step-child` beside this binary, taking the project's machine
//! slots. It stops by degrees as `owlshift watch` does, its checked stop
//! bounded by `CHECKED_STOP_BOUND`, and passes until a stop is asked of it,
//! adding a byte to `<data>/bench/demo-project/passes` after each pass.
//!
//! The stand-in stops as a watch does: by degrees, its checked stop bounded
//! by 2 seconds, its process trees stopping with it however it ends. It runs
//! one step, `sleep 600` in a tree of its own, and writes its pid and the
//! step's to `<data>/bench/<owner>-<repo>/<pid>`. It ends with the exit code
//! the file `exit` there holds, saying why on its standard error, when that
//! file stands; otherwise it ends, its step stopped, once its project's stop
//! file stands (removing it) or a stop is asked of it, unless the file
//! `hang` stands, when only its bound ends it.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::thread;
use std::time::Duration;

use jiff::Timestamp;
use owlshift_adapters::forge::Repo;
use owlshift_contracts::format::Format;
use owlshift_contracts::service::DaemonRecord;
use owlshift_core::restart::RestartPolicy;
use owlshift_platform::process::{self, AtCheckedStop, ProcessTree};
use owlshift_runner::daemon::{self, Daemon, Held, Timing};
use owlshift_runner::project::ProjectDirs;
use owlshift_runner::watch;
use owlshift_runner::watch::stop::CHECKED_STOP_BOUND;
use owlshift_testkit::step_child::{self, ChildProject, FileNotifier};

fn main() -> ExitCode {
    process::stop_trees_on_signal().expect("the signal policy");
    #[cfg(unix)]
    process::stop_trees_when_killed().expect("the sentinel");
    let args: Vec<String> = std::env::args().skip(1).collect();
    let data = PathBuf::from(std::env::var_os("OWLSHIFT_DATA_DIR").expect("OWLSHIFT_DATA_DIR"));
    match args.as_slice() {
        [command] if command == "daemon" => run_daemon(&data),
        [command, repo] if command == "watch" => run_watch(&data, &Repo::parse(repo).unwrap()),
        [command, repo] if command == "drain" => run_drain(&data, repo),
        _ => {
            eprintln!(
                "usage: owlshift-daemon-bench daemon | watch <OWNER/REPO> | drain <OWNER/REPO>"
            );
            ExitCode::from(2)
        }
    }
}

fn run_daemon(data: &Path) -> ExitCode {
    // As `owlshift daemon`: no bound of its own, its watches' applying.
    process::stop_by_degrees(None).expect("the stop by degrees");
    let ms: Vec<u64> = std::env::var("OWLSHIFT_BENCH_TIMING")
        .expect("OWLSHIFT_BENCH_TIMING")
        .split(',')
        .map(|n| n.trim().parse().unwrap())
        .collect();
    let [pass, tick, first, max, reset] = ms[..] else {
        panic!("five timings: {ms:?}");
    };
    let timing = Timing {
        pass: Duration::from_millis(pass),
        tick: Duration::from_millis(tick),
        restarts: RestartPolicy {
            first: Duration::from_millis(first),
            max: Duration::from_millis(max),
            reset_after: Duration::from_millis(reset),
            notify_after: RestartPolicy::SERVICE.notify_after,
        },
    };
    let exe = std::env::current_exe().unwrap();
    let record = DaemonRecord {
        format: Format,
        pid: std::process::id(),
        version: "bench".to_owned(),
        binary: exe.display().to_string(),
        since: Timestamp::now(),
    };
    let instance = match daemon::instance::take(data, &record).unwrap() {
        Ok(instance) => instance,
        Err(Held(holder)) => {
            eprintln!(
                "a daemon already runs (pid {:?})",
                holder.map(|record| record.pid)
            );
            return ExitCode::FAILURE;
        }
    };
    let notes = FileNotifier(PathBuf::from(
        std::env::var_os("OWLSHIFT_BENCH_NOTES").expect("OWLSHIFT_BENCH_NOTES"),
    ));
    // The real watch when a project file is given, else the stand-in.
    let real = std::env::var_os("OWLSHIFT_BENCH_PROJECT").is_some();
    let watch = |repo: &Repo| {
        let mut command = Command::new(&exe);
        command.args([if real { "drain" } else { "watch" }, &repo.to_string()]);
        command
    };
    let daemon = Daemon {
        data,
        watch: &watch,
        notifier: Some(&notes),
        timing,
        digest: None,
    };
    let daemon::Ended::Stopped = daemon.run(&mut std::io::stdout());
    drop(instance);
    ExitCode::SUCCESS
}

/// The real watch of the project `OWLSHIFT_BENCH_PROJECT` names, which must
/// be `repo` in the data directory `data`.
fn run_drain(data: &Path, repo: &str) -> ExitCode {
    // As `owlshift watch`.
    process::stop_by_degrees(Some(CHECKED_STOP_BOUND)).expect("the stop by degrees");
    let file =
        PathBuf::from(std::env::var_os("OWLSHIFT_BENCH_PROJECT").expect("OWLSHIFT_BENCH_PROJECT"));
    let project = ChildProject::load(&file);
    if repo != step_child::REPO || project.data != data {
        eprintln!(
            "owlshift: the bench's project is {} in {}, not {repo} in {}",
            step_child::REPO,
            project.data.display(),
            data.display()
        );
        return ExitCode::from(2);
    }
    let exe = std::env::current_exe().unwrap();
    let child = exe.with_file_name(format!(
        "owlshift-step-child{}",
        std::env::consts::EXE_SUFFIX
    ));
    // A byte for each pass, so a test knows the watch passed since it looked.
    let bench = bench_dir(data, &Repo::parse(repo).unwrap());
    fs::create_dir_all(&bench).unwrap();
    let passes = bench.join("passes");
    let mut between = || {
        use std::io::Write;
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&passes)
            .unwrap();
        file.write_all(b".").unwrap();
        true
    };
    println!("watching {repo}");
    project.watch_until_stopped(Some(step_child::step_children(&file, &child)), &mut between);
    println!("{repo} ended");
    ExitCode::SUCCESS
}

/// The bench's folder of `repo`'s watches.
fn bench_dir(data: &Path, repo: &Repo) -> PathBuf {
    data.join("bench").join(repo.to_string().replace('/', "-"))
}

fn run_watch(data: &Path, repo: &Repo) -> ExitCode {
    process::stop_by_degrees(Some(Duration::from_secs(2))).expect("the stop by degrees");
    let bench = bench_dir(data, repo);
    fs::create_dir_all(&bench).unwrap();
    if let Ok(code) = fs::read_to_string(bench.join("exit")) {
        eprintln!("owlshift: {repo} cannot be served on the bench");
        return ExitCode::from(code.trim().parse::<u8>().unwrap());
    }
    let mut sleep = Command::new("sleep");
    sleep.arg("600");
    let (mut step, tree) = ProcessTree::spawn_for(&mut sleep, AtCheckedStop::Stopped).unwrap();
    let pid = std::process::id();
    let partial = bench.join(format!("{pid}.partial"));
    fs::write(&partial, format!("{pid} {}", step.id())).unwrap();
    fs::rename(&partial, bench.join(pid.to_string())).unwrap();
    println!("watching {repo}");
    let dirs = ProjectDirs::new(data, repo);
    let hang = bench.join("hang").exists();
    loop {
        let drained = watch::stop::stands(&dirs);
        if drained {
            watch::stop::remove(&dirs).unwrap();
        }
        if drained || (process::asked().is_some() && !hang) {
            let _ = tree.kill();
            let _ = tree.reap(&mut step);
            println!("{repo} ended");
            return ExitCode::SUCCESS;
        }
        thread::sleep(Duration::from_millis(50));
    }
}
