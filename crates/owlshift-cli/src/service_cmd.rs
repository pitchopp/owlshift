//! `owlshift daemon`, `owlshift start` and `owlshift stop` (OWL-435,
//! architecture section 5, "The background service"; runtime and
//! operations, "Install, lifecycle & uninstall"): the service itself, which
//! `owlshift_runner::daemon` runs, and the two commands a person types to
//! serve a project and to stop the service.

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::thread;
use std::time::{Duration, Instant};

use jiff::Timestamp;
use owlshift_adapters::forge::Repo;
use owlshift_adapters::notifier::Notifier;
use owlshift_contracts::format::Format;
use owlshift_contracts::service::{BY_STOP, DaemonRecord, Request};
use owlshift_runner::config::{Effective, FileState, OWLSHIFT_VERSION};
use owlshift_runner::daemon::{self, Daemon, Held, Timing, requests};
use owlshift_runner::notify::{self, DesktopNotifier};
use owlshift_runner::served::Registry;
use owlshift_runner::system::System;
use owlshift_runner::watch::drain_refusal;
use owlshift_runner::{on_demand, project};

use crate::do_cmd::loaded_project;
use crate::{data_dir, fail};

/// Why the service is refused on native Windows.
const ON_WINDOWS: &str = "the background service runs on macOS and Linux, and on Windows through \
     WSL2: native Windows runs no agent (D9)";

/// How long `owlshift start` waits for the daemon it launched to run.
const LAUNCH_WAIT: Duration = Duration::from_secs(10);

/// How often that wait looks.
const LAUNCH_POLL: Duration = Duration::from_millis(100);

/// `owlshift daemon`: the background service, in the foreground, until a
/// stop asked of it is done.
pub fn daemon(system: &dyn System, config: &Effective) -> ExitCode {
    if cfg!(windows) {
        return fail(&format!("`owlshift daemon`: {ON_WINDOWS}"));
    }
    let data = match data_dir() {
        Ok(data) => data,
        Err(error) => return fail(&error),
    };
    // The binary every watch runs, resolved now (architecture section 5,
    // "An upgrade").
    let own = match owlshift_platform::process::own_program() {
        Ok(own) => own,
        Err(error) => {
            return fail(&format!(
                "this program's own path, which runs every watch: {error}"
            ));
        }
    };
    let record = DaemonRecord {
        format: Format,
        pid: std::process::id(),
        version: OWLSHIFT_VERSION.to_owned(),
        binary: own.path.display().to_string(),
        since: Timestamp::now(),
    };
    let instance = match daemon::instance::take(&data, &record) {
        Ok(Ok(instance)) => instance,
        Ok(Err(Held(holder))) => return fail(&already_runs(&data, holder.as_ref())),
        Err(error) => {
            return fail(&format!(
                "{}: {error}",
                daemon::service_dir(&data).display()
            ));
        }
    };
    let desktop = notify::desktop_enabled(&config.personal)
        .then(|| DesktopNotifier::for_this_machine(system))
        .flatten();
    let watch = |repo: &Repo| watch_command(&own.spawn, repo);
    let daemon = Daemon {
        data: &data,
        watch: &watch,
        notifier: desktop.as_ref().map(|d| d as &dyn Notifier),
        timing: Timing::SERVICE,
    };
    let daemon::Ended::Stopped = daemon.run(&mut io::stdout());
    drop(instance);
    ExitCode::SUCCESS
}

/// The watch of the served project `repo`: `program`, this binary, as
/// `owlshift watch --drain --served <OWNER/REPO>` (OWL-434).
fn watch_command(program: &Path, repo: &Repo) -> Command {
    let mut command = Command::new(program);
    command.args(["watch", "--drain", "--served", &repo.to_string()]);
    command
}

/// Why a second daemon on `data` does not start.
fn already_runs(data: &Path, holder: Option<&DaemonRecord>) -> String {
    let which = holder.map_or_else(String::new, |record| {
        format!(
            " (pid {}, version {}, since {})",
            record.pid, record.version, record.since
        )
    });
    format!(
        "a daemon already runs on {}{which}: `owlshift stop` stops it",
        data.display()
    )
}

/// `owlshift start`: serves the project of the working directory, and
/// starts the daemon for this login session when none runs.
pub fn start(config: &Effective) -> ExitCode {
    if cfg!(windows) {
        return fail(&format!("`owlshift start`: {ON_WINDOWS}"));
    }
    let (project_file, project) = match loaded_project(config, "`owlshift start`") {
        Ok(loaded) => loaded,
        Err(refusal) => return fail(&refusal),
    };
    // A served watch drains: a project file that does not suit the drain
    // is told now, before the service reads it from the default branch.
    if let Some(refusal) = drain_refusal(project) {
        return fail(&refusal);
    }
    let checkout = project_file.parent().unwrap_or(project_file);
    let repo = match repo_of(checkout) {
        Ok(repo) => repo,
        Err(refusal) => return fail(&refusal),
    };
    let data = match data_dir() {
        Ok(data) => data,
        Err(error) => return fail(&error),
    };
    let changed = match Registry::in_data_dir(&data).register(&repo, checkout) {
        Ok(changed) => changed,
        Err(error) => return fail(&error),
    };
    let served = if changed {
        format!("{repo} is served from {}", checkout.display())
    } else {
        format!("{repo} was already served from {}", checkout.display())
    };
    match daemon::running(&data) {
        Ok(Some(record)) => {
            let pid = record.map_or_else(String::new, |record| format!(" (pid {})", record.pid));
            if requests::stop_asked(&data).is_some() {
                println!("{served}.");
                return fail(&format!(
                    "the daemon{pid} is stopping, asked by `owlshift stop`, and serves nothing \
                     more: run `owlshift start` again once it has ended"
                ));
            }
            if changed {
                println!(
                    "{served}: the daemon{pid} serves it from its next pass, within a minute."
                );
            } else {
                println!("{served}, by the daemon{pid}.");
            }
            ExitCode::SUCCESS
        }
        Ok(None) => {
            // A request left by a daemon that ended before it read it would
            // stop the new one at its start.
            if let Err(error) = requests::remove_stop_request(&data) {
                return fail(&format!("the stop request left behind: {error}"));
            }
            let program = match std::env::current_exe() {
                Ok(program) => program,
                Err(error) => {
                    return fail(&format!(
                        "this program's own path, which runs the daemon: {error}"
                    ));
                }
            };
            let launch = Launch {
                program: &program,
                data: &data,
                config_dir: owlshift_platform::paths::personal_config_file()
                    .and_then(|file| file.parent().map(Path::to_owned)),
            };
            let since = Timestamp::now();
            let started = launch_session_daemon(&launch)
                .and_then(|mut launched| wait_running(&data, since, launched.as_mut()));
            match started {
                Ok(record) => {
                    println!(
                        "{served}. Started the daemon for this session (pid {}): its output is in \
                         {}; `owlshift stop` stops it.",
                        record.pid,
                        log_file(&data).display()
                    );
                    ExitCode::SUCCESS
                }
                Err(error) => {
                    println!("{served}.");
                    fail(&format!("the daemon did not start: {error}"))
                }
            }
        }
        Err(error) => fail(&format!(
            "{}: {error}",
            daemon::service_dir(&data).display()
        )),
    }
}

/// What `owlshift start` needs to run the daemon for this login session.
struct Launch<'a> {
    /// This binary.
    program: &'a Path,
    /// The data directory, named to the daemon explicitly.
    data: &'a Path,
    /// The folder of the personal file, named to the daemon explicitly, when
    /// there is one.
    config_dir: Option<PathBuf>,
}

/// A daemon launched for the session.
trait Launched {
    /// Why it already ended, when it did and that can be told; `None`
    /// otherwise.
    fn ended(&mut self) -> Option<String>;
}

/// The daemon's output, for the session's daemon.
fn log_file(data: &Path) -> PathBuf {
    daemon::service_dir(data).join("daemon.log")
}

/// Starts `owlshift daemon` for the login session: a process of its own,
/// out of the reach of the terminal's signals, in a process group of its
/// own, its standard input closed and its output appended to
/// `<data>/service/daemon.log`, readable by the user alone; it lives until
/// a stop, a logout or a shutdown, and nothing starts it again after a crash
/// before the next `owlshift start`.
#[cfg(unix)]
fn launch_session_daemon(launch: &Launch<'_>) -> Result<Box<dyn Launched>, String> {
    use std::fs::{self, OpenOptions};
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::process::CommandExt;
    use std::process::{Child, Stdio};

    struct Detached {
        child: Child,
        log: PathBuf,
    }

    impl Launched for Detached {
        fn ended(&mut self) -> Option<String> {
            match self.child.try_wait() {
                Ok(Some(status)) => Some(format!(
                    "it ended at once ({status}): see {}",
                    self.log.display()
                )),
                Ok(None) => None,
                Err(error) => Some(format!("its end could not be read: {error}")),
            }
        }
    }

    let dir = daemon::service_dir(launch.data);
    fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let log = log_file(launch.data);
    let opened = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&log)
        .map_err(|e| format!("{}: {e}", log.display()))?;
    let err = opened
        .try_clone()
        .map_err(|e| format!("{}: {e}", log.display()))?;
    let mut command = Command::new(launch.program);
    command
        .arg("daemon")
        .current_dir(&dir)
        .env("OWLSHIFT_DATA_DIR", launch.data)
        .stdin(Stdio::null())
        .stdout(opened)
        .stderr(err)
        .process_group(0);
    if let Some(config_dir) = &launch.config_dir {
        command.env("OWLSHIFT_CONFIG_DIR", config_dir);
    }
    let child = command
        .spawn()
        .map_err(|e| format!("{} could not start: {e}", launch.program.display()))?;
    Ok(Box::new(Detached { child, log }))
}

/// Native Windows runs no daemon (D9): `start` refuses before this.
#[cfg(windows)]
fn launch_session_daemon(launch: &Launch<'_>) -> Result<Box<dyn Launched>, String> {
    let _ = (launch.program, launch.data, &launch.config_dir);
    Err(ON_WINDOWS.to_owned())
}

/// Waits until a daemon started at `since` or later runs on `data`, its
/// record written: refused once `launched` ended, or after [`LAUNCH_WAIT`].
fn wait_running(
    data: &Path,
    since: Timestamp,
    launched: &mut dyn Launched,
) -> Result<DaemonRecord, String> {
    let deadline = Instant::now() + LAUNCH_WAIT;
    loop {
        if let Ok(Some(Some(record))) = daemon::running(data)
            && record.since >= since
        {
            return Ok(record);
        }
        if let Some(ended) = launched.ended() {
            return Err(ended);
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "it was not running {} seconds later: see {}",
                LAUNCH_WAIT.as_secs(),
                log_file(data).display()
            ));
        }
        thread::sleep(LAUNCH_POLL);
    }
}

/// `owlshift stop`: drains every watch of the daemon, which then exits; with
/// `forget`, stops serving the project of the working directory only.
pub fn stop(config: &Effective, forget: bool) -> ExitCode {
    if cfg!(windows) {
        return fail(&format!("`owlshift stop`: {ON_WINDOWS}"));
    }
    let data = match data_dir() {
        Ok(data) => data,
        Err(error) => return fail(&error),
    };
    let running = match daemon::running(&data) {
        Ok(running) => running,
        Err(error) => {
            return fail(&format!(
                "{}: {error}",
                daemon::service_dir(&data).display()
            ));
        }
    };
    if forget {
        let repo = match checkout_of(config).and_then(|checkout| repo_of(&checkout)) {
            Ok(repo) => repo,
            Err(refusal) => return fail(&refusal),
        };
        return match Registry::in_data_dir(&data).forget(&repo) {
            Ok(true) if running.is_some() => {
                println!(
                    "{repo} is no longer served: the daemon drains its watch at its next pass, \
                     within a minute."
                );
                ExitCode::SUCCESS
            }
            Ok(true) => {
                println!("{repo} is no longer served.");
                ExitCode::SUCCESS
            }
            Ok(false) => {
                println!("{repo} was not served.");
                ExitCode::SUCCESS
            }
            Err(error) => fail(&error),
        };
    }
    let Some(record) = running else {
        println!("No daemon runs on {}.", data.display());
        return ExitCode::SUCCESS;
    };
    let request = Request::new(BY_STOP, Timestamp::now(), "asked by `owlshift stop`");
    if let Err(error) = daemon::ask_stop(&data, &request) {
        return fail(&format!("the stop request: {error}"));
    }
    // A daemon that ended meanwhile would leave the request to stop the
    // next one at its start.
    if matches!(daemon::running(&data), Ok(None)) {
        let _ = requests::remove_stop_request(&data);
        println!("No daemon runs on {}.", data.display());
        return ExitCode::SUCCESS;
    }
    let pid = record.map_or_else(String::new, |record| format!(" (pid {})", record.pid));
    println!(
        "Stopping the daemon{pid}: each watch ends once its steps under way finish, then the \
         daemon exits."
    );
    ExitCode::SUCCESS
}

/// The checkout of the working directory's project, whether its project
/// file loads or not.
fn checkout_of(config: &Effective) -> Result<PathBuf, String> {
    let file = match &config.project {
        FileState::Loaded { path, .. } | FileState::Invalid { path, .. } => path,
        FileState::Absent(path) => path,
        FileState::NotApplicable(reason) => {
            return Err(format!(
                "`owlshift stop --forget` runs in a git repository: {reason}"
            ));
        }
        FileState::Unavailable(reason) => return Err(reason.clone()),
    };
    Ok(file.parent().unwrap_or(file).to_owned())
}

/// The GitHub repository of `checkout`, from its `origin`.
fn repo_of(checkout: &Path) -> Result<Repo, String> {
    let url = project::origin_url(&project::runner_git(), checkout)?;
    on_demand::check_origin(&url)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The daemon's watch of a served project is this binary's served drain
    /// (OWL-434).
    #[test]
    fn the_daemons_watch_is_the_served_drain() {
        let repo = Repo::parse("acme/web").unwrap();
        let command = watch_command(Path::new("/proc/self/exe"), &repo);
        assert_eq!(command.get_program(), "/proc/self/exe");
        let args: Vec<_> = command.get_args().collect();
        assert_eq!(args, ["watch", "--drain", "--served", "acme/web"]);
    }
}
