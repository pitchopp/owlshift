//! `owlshift daemon`, `owlshift start` and `owlshift stop` (OWL-435,
//! architecture section 5, "The background service"; runtime and
//! operations, "Install, lifecycle & uninstall"): the service itself, which
//! `owlshift_runner::daemon` runs, and the two commands a person types to
//! serve a project and to stop the service. With `--at-login` they install
//! and remove the service at login, and `owlshift uninstall` removes what
//! the install manifest lists (OWL-437, `owlshift_runner::install`).

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
use owlshift_platform::keychain::Keychain;
use owlshift_platform::service::{HostManager, Layout, Manager};
use owlshift_runner::config::{Effective, FileState, OWLSHIFT_VERSION};
use owlshift_runner::daemon::{self, Daemon, Held, Timing, requests};
use owlshift_runner::events::printable;
use owlshift_runner::install::uninstall::{self, Personal, Stopped};
use owlshift_runner::install::{self, Install, Installed};
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
/// starts the daemon for this login session when none runs; with
/// `at_login`, installs the service at login first (OWL-437). An installed
/// service is started through its manager, never as a second daemon.
pub fn start(config: &Effective, at_login: bool) -> ExitCode {
    if cfg!(windows) {
        return fail(&format!("`owlshift start`: {ON_WINDOWS}"));
    }
    if at_login && owlshift_platform::service::runs_as_root() {
        return fail(
            "`owlshift start --at-login` installs the service of the user who runs it: run it as \
             yourself, not as root",
        );
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
    let manager = HostManager::for_this_machine();
    if at_login {
        let (layout, binary) = match this_layout().and_then(|l| Ok((l, binary_for_service()?))) {
            Ok(found) => found,
            Err(error) => return fail(&error),
        };
        let config_dir = personal_folder();
        let path = std::env::var_os("PATH");
        let spec = Install {
            data: &data,
            layout: &layout,
            binary: &binary,
            config_dir: config_dir.as_deref(),
            path: path.as_deref(),
        };
        match install_at_login(&spec, &manager) {
            Ok(said) => println!("{said}"),
            Err(error) => {
                println!("{served}.");
                return fail(&error);
            }
        }
    }
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
            if at_login {
                println!(
                    "The daemon that runs now{pid} goes on: the service at login takes over at \
                     its next start, at the latest at your next login (`owlshift stop`, then \
                     `owlshift start`, hands over now)."
                );
            }
            ExitCode::SUCCESS
        }
        Ok(None) => {
            // A request left by a daemon that ended before it read it would
            // stop the new one at its start.
            if let Err(error) = requests::remove_stop_request(&data) {
                return fail(&format!("the stop request left behind: {error}"));
            }
            match install::installed(&data) {
                Ok(Some(service)) => {
                    return start_by_manager(&data, &install::layout_of(&service), &manager)
                        .map_or_else(
                            |error| {
                                println!("{served}.");
                                fail(&format!(
                                    "the service at login is installed but did not start: \
                                     {error}. It starts at your next login; `owlshift doctor` \
                                     says more"
                                ))
                            },
                            |record| {
                                println!(
                                    "{served}. Started the service at login (pid {}): its \
                                     output is in {}; `owlshift stop` stops it, `owlshift stop \
                                     --at-login` removes it.",
                                    record.pid,
                                    log_file(&data).display()
                                );
                                ExitCode::SUCCESS
                            },
                        );
                }
                Ok(None) => {}
                Err(error) => return fail(&error),
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

/// Where this machine's service manager keeps Owlshift's unit and launcher.
fn this_layout() -> Result<Layout, String> {
    let home = owlshift_platform::paths::home_dir()
        .ok_or_else(|| "this system names no home folder for the service's files".to_owned())?;
    Ok(Layout::for_this_machine(
        &home,
        owlshift_platform::paths::config_home().as_deref(),
    ))
}

/// The binary the service starts: this one, by the path it was run by when
/// that is the same file, so a package manager's stable link is named
/// rather than its versioned folder.
fn binary_for_service() -> Result<PathBuf, String> {
    let current = std::env::current_exe()
        .map_err(|e| format!("this program's own path, which the service starts: {e}"))?;
    let cwd = std::env::current_dir()
        .map_err(|e| format!("the current directory, against which this program ran: {e}"))?;
    let invoked = std::env::args_os().next();
    Ok(owlshift_platform::service::service_binary(
        invoked.as_deref(),
        &cwd,
        &current,
        &|name| owlshift_platform::process::find_executable(&name.to_string_lossy()),
    ))
}

/// The folder of the personal file, named to the daemon, when there is one.
fn personal_folder() -> Option<PathBuf> {
    owlshift_platform::paths::personal_config_file()
        .and_then(|file| file.parent().map(Path::to_owned))
}

/// Installs the service at login and registers it with `manager` for the
/// next logins; what to say. A manager that refuses it leaves nothing
/// installed.
fn install_at_login(spec: &Install<'_>, manager: &dyn Manager) -> Result<String, String> {
    let installed = install::install(spec)?;
    if let Err(error) = manager.enable(spec.layout) {
        let undone = match install::remove_service(spec.data, manager) {
            Ok(_) => "nothing stays installed".to_owned(),
            Err(left) => format!("and removing it failed too: {left}"),
        };
        return Err(format!(
            "the service manager did not take the service at login ({error}): {undone}"
        ));
    }
    let unit = spec.layout.unit();
    Ok(match installed {
        Installed::New => format!(
            "Installed the service at login: {} runs {}, which starts {} daemon on {}; `owlshift \
             stop --at-login` removes it.",
            unit.display(),
            spec.layout.launcher().display(),
            spec.binary.display(),
            spec.data.display()
        ),
        Installed::Updated => format!(
            "Updated the service at login ({}): it starts {} daemon.",
            unit.display(),
            spec.binary.display()
        ),
        Installed::Unchanged => format!(
            "The service at login is installed already ({}).",
            unit.display()
        ),
    })
}

/// Starts the installed service through its manager, then waits for its
/// daemon to run.
fn start_by_manager(
    data: &Path,
    layout: &Layout,
    manager: &dyn Manager,
) -> Result<DaemonRecord, String> {
    /// A daemon its manager starts: only its record tells it runs.
    struct ByManager;

    impl Launched for ByManager {
        fn ended(&mut self) -> Option<String> {
            None
        }
    }

    let since = Timestamp::now();
    manager.start(layout)?;
    wait_running(data, since, &mut ByManager)
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
/// `forget`, stops serving the project of the working directory only; with
/// `at_login`, also waits for the daemon to end, however long its steps under
/// way take, then unloads and removes the service at login (OWL-437).
pub fn stop(config: &Effective, forget: bool, at_login: bool) -> ExitCode {
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
    match running {
        None => println!("No daemon runs on {}.", data.display()),
        Some(record) => {
            let request = Request::new(BY_STOP, Timestamp::now(), "asked by `owlshift stop`");
            if let Err(error) = daemon::ask_stop(&data, &request) {
                return fail(&format!("the stop request: {error}"));
            }
            // A daemon that ended meanwhile would leave the request to stop
            // the next one at its start.
            if matches!(daemon::running(&data), Ok(None)) {
                let _ = requests::remove_stop_request(&data);
                println!("No daemon runs on {}.", data.display());
            } else {
                let pid =
                    record.map_or_else(String::new, |record| format!(" (pid {})", record.pid));
                println!(
                    "Stopping the daemon{pid}: each watch ends once its steps under way finish, \
                     then the daemon exits."
                );
                if at_login {
                    println!(
                        "Waiting for it to end before removing the service at login. Ctrl-C \
                         stops the wait: the service then stays installed while the drain goes \
                         on, and `owlshift stop --at-login` again removes it."
                    );
                    if let Err(error) = wait_ended(&data) {
                        return fail(&error);
                    }
                }
            }
        }
    }
    if !at_login {
        return ExitCode::SUCCESS;
    }
    match remove_at_login(&data, &HostManager::for_this_machine()) {
        Ok(said) => {
            for line in said {
                println!("{line}");
            }
            ExitCode::SUCCESS
        }
        Err(error) => fail(&error),
    }
}

/// How often `owlshift stop --at-login` looks whether the daemon ended.
const END_POLL: Duration = Duration::from_millis(500);

/// Waits, with no bound, until no daemon runs on `data`.
fn wait_ended(data: &Path) -> Result<(), String> {
    loop {
        match daemon::running(data) {
            Ok(None) => return Ok(()),
            Ok(Some(_)) => thread::sleep(END_POLL),
            Err(error) => {
                return Err(format!("{}: {error}", daemon::service_dir(data).display()));
            }
        }
    }
}

/// Unloads and removes the service at login the install manifest of
/// `data` lists, once no daemon runs; what to say.
fn remove_at_login(data: &Path, manager: &dyn Manager) -> Result<Vec<String>, String> {
    let removal = install::remove_service(data, manager)?;
    let mut said: Vec<String> = removal
        .warnings
        .iter()
        .map(|warning| format!("Note: {warning}."))
        .collect();
    said.push(match removal.removed {
        Some(service) => format!(
            "Removed the service at login: {}.",
            service.files().join(", ")
        ),
        None => format!(
            "No service at login to remove: the install manifest of {} lists none.",
            data.display()
        ),
    });
    Ok(said)
}

/// `owlshift uninstall`: stops the daemon checked, then removes exactly what
/// the install manifest lists; with `purge`, also Owlshift's entries of the
/// data directory and the personal file (OWL-437).
pub fn uninstall(purge: bool) -> ExitCode {
    if cfg!(windows) {
        return fail(&format!("`owlshift uninstall`: {ON_WINDOWS}"));
    }
    let data = match data_dir() {
        Ok(data) => data,
        Err(error) => return fail(&error),
    };
    let personal_file = owlshift_platform::paths::personal_config_file();
    let personal = personal_file.as_deref().map(|file| Personal {
        file,
        own_folder: owlshift_platform::paths::personal_file_in_own_folder(file),
    });
    let home = owlshift_platform::paths::home_dir();
    let outcome = uninstall_with(&Uninstall {
        data: &data,
        home: home.as_deref(),
        manager: &HostManager::for_this_machine(),
        terminate: &owlshift_platform::service::terminate,
        keychain: &|| Keychain::system().map_err(|e| e.to_string()),
        purge: purge.then_some(personal.as_ref()),
        stop_wait: uninstall::STOP_WAIT,
    });
    for line in &outcome.said {
        println!("{line}");
    }
    if outcome.failed.is_empty() {
        return ExitCode::SUCCESS;
    }
    for error in &outcome.failed {
        eprintln!("owlshift: {}", printable(error));
    }
    ExitCode::FAILURE
}

/// What `owlshift uninstall` works with.
struct Uninstall<'a> {
    data: &'a Path,
    home: Option<&'a Path>,
    manager: &'a dyn Manager,
    terminate: &'a dyn Fn(u32) -> io::Result<()>,
    keychain: &'a dyn Fn() -> Result<Keychain, String>,
    /// With `--purge`: the personal file, when there is one.
    purge: Option<Option<&'a Personal<'a>>>,
    stop_wait: Duration,
}

/// What `owlshift uninstall` did: what to say, and what failed.
#[derive(Debug, Default)]
struct Outcome {
    said: Vec<String>,
    failed: Vec<String>,
}

/// The daemon stopped checked, the purge's refusals told before anything is
/// removed, then the manifest's entries, then the purge.
fn uninstall_with(how: &Uninstall<'_>) -> Outcome {
    let data = how.data;
    let mut outcome = Outcome::default();
    if how.purge.is_some()
        && let Some(refusal) = uninstall::purge_refusal(data, how.home)
    {
        outcome.failed.push(refusal);
        return outcome;
    }
    let running = || daemon::running(data);
    match uninstall::stop_checked(
        data,
        &running,
        how.terminate,
        how.stop_wait,
        uninstall::STOP_POLL,
    ) {
        Ok(Stopped::NotRunning) => {}
        Ok(Stopped::Ended(pid)) => outcome.said.push(format!(
            "Stopped the daemon (pid {pid}), its runs checked: a ticket it cut resumes at the \
             next start."
        )),
        Err(refusal) => {
            outcome.failed.push(refusal);
            return outcome;
        }
    }
    if how.purge.is_some() {
        match uninstall::busy(data) {
            Ok(held) if held.is_empty() => {}
            Ok(held) => {
                outcome.failed.push(format!(
                    "another Owlshift command works on {} in {}: nothing was removed. Let it \
                     end, then run `owlshift uninstall --purge` again",
                    held.join(", "),
                    data.display()
                ));
                return outcome;
            }
            Err(error) => {
                outcome.failed.push(error);
                return outcome;
            }
        }
    }
    match uninstall::remove_listed(data, how.manager, how.keychain) {
        Ok(report) => {
            outcome.said.extend(
                report
                    .warnings
                    .iter()
                    .map(|warning| format!("Note: {warning}.")),
            );
            outcome
                .said
                .extend(report.removed.iter().map(|what| format!("Removed {what}.")));
            if report.removed.is_empty() && report.left.is_empty() {
                outcome.said.push(format!(
                    "The install manifest of {} lists nothing to remove.",
                    data.display()
                ));
            }
            outcome.failed.extend(
                report.left.iter().map(|what| {
                    format!("not removed: {what}; the install manifest still lists it")
                }),
            );
        }
        Err(error) => {
            outcome.failed.push(error);
            return outcome;
        }
    }
    if let Some(personal) = how.purge {
        let purged = uninstall::purge(data, personal);
        if purged.removed.iter().any(|path| path == data) {
            outcome.said.push(format!(
                "Removed the data directory {}: the projects' clones, worktrees and run logs, \
                 the event log and the service's files.",
                data.display()
            ));
        } else if !purged.removed.is_empty() {
            outcome
                .said
                .push(format!("Removed Owlshift's files in {}.", data.display()));
        }
        if let Some(personal) = personal
            && purged.removed.iter().any(|path| path == personal.file)
        {
            outcome.said.push(format!(
                "Removed the personal file {}.",
                personal.file.display()
            ));
        }
        if !purged.left.is_empty() {
            let left: Vec<String> = purged
                .left
                .iter()
                .map(|path| path.display().to_string())
                .collect();
            outcome.said.push(format!(
                "Left in place, not Owlshift's or still listing what was not removed: {}.",
                left.join(", ")
            ));
        }
        outcome.failed.extend(purged.failed);
    }
    outcome
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

    /// OWL-437: the service at login, on temp folders and a recording
    /// manager, never `launchctl` or `systemctl`.
    #[cfg(unix)]
    mod at_login {
        use std::cell::RefCell;
        use std::fs;

        use owlshift_contracts::service::KeychainEntry;
        use owlshift_platform::keychain::Secret;
        use owlshift_platform::service::Kind;
        use owlshift_runner::install::Manifests;

        use super::super::*;

        /// A service manager that records what it is asked, failing `enable`
        /// when told to.
        #[derive(Default)]
        struct Recorded {
            calls: RefCell<Vec<&'static str>>,
            refuses_enable: bool,
        }

        impl Recorded {
            fn take(&self) -> Vec<&'static str> {
                std::mem::take(&mut self.calls.borrow_mut())
            }
        }

        impl Manager for Recorded {
            fn enable(&self, _: &Layout) -> Result<(), String> {
                self.calls.borrow_mut().push("enable");
                if self.refuses_enable {
                    Err("no user bus".to_owned())
                } else {
                    Ok(())
                }
            }
            fn start(&self, _: &Layout) -> Result<(), String> {
                self.calls.borrow_mut().push("start");
                Ok(())
            }
            fn unload(&self, _: &Layout) -> Result<(), String> {
                self.calls.borrow_mut().push("unload");
                Ok(())
            }
            fn reload(&self, _: &Layout) -> Result<(), String> {
                self.calls.borrow_mut().push("reload");
                Ok(())
            }
        }

        fn spec<'a>(data: &'a Path, layout: &'a Layout, binary: &'a Path) -> Install<'a> {
            Install {
                data,
                layout,
                binary,
                config_dir: None,
                path: None,
            }
        }

        /// OWL-437: `start --at-login` installs and enables; a manager that
        /// refuses leaves nothing installed. `stop --at-login`, with no daemon
        /// running, still unloads and removes it.
        #[test]
        fn the_service_is_installed_at_login_and_removed_with_stop() {
            let root = tempfile::tempdir().unwrap();
            let data = root.path().join("data");
            let layout = Layout {
                kind: Kind::Systemd,
                dir: root.path().join("units"),
            };
            let binary = root.path().join("owlshift");
            let manager = Recorded::default();
            let said = install_at_login(&spec(&data, &layout, &binary), &manager).unwrap();
            assert!(
                said.starts_with("Installed the service at login: "),
                "{said}"
            );
            assert_eq!(manager.take(), ["enable"]);
            assert!(layout.unit().exists() && layout.launcher().exists());
            let said = install_at_login(&spec(&data, &layout, &binary), &manager).unwrap();
            assert!(said.contains("installed already"), "{said}");
            manager.take();

            let said = remove_at_login(&data, &manager).unwrap();
            assert_eq!(manager.take(), ["unload", "reload"]);
            assert!(
                said[0].starts_with("Removed the service at login: "),
                "{said:?}"
            );
            assert!(!layout.unit().exists() && !layout.launcher().exists());
            let said = remove_at_login(&data, &manager).unwrap();
            assert!(
                said[0].starts_with("No service at login to remove"),
                "{said:?}"
            );

            let refusing = Recorded {
                refuses_enable: true,
                ..Recorded::default()
            };
            let error = install_at_login(&spec(&data, &layout, &binary), &refusing).unwrap_err();
            assert!(
                error.contains("(no user bus): nothing stays installed"),
                "{error}"
            );
            assert!(!layout.unit().exists() && !Manifests::in_data_dir(&data).file().exists());
        }

        /// OWL-437: `uninstall --purge` with no daemon running removes the
        /// service, the keychain entries `init` stored, the data directory and
        /// the personal file; a folder that is not Owlshift's data directory is
        /// refused before anything is removed.
        #[test]
        fn uninstall_removes_what_the_manifest_lists_then_purges() {
            let root = tempfile::tempdir().unwrap();
            let data = root.path().join("data");
            let layout = Layout {
                kind: Kind::Launchd,
                dir: root.path().join("agents"),
            };
            let binary = root.path().join("owlshift");
            let manager = Recorded::default();
            install_at_login(&spec(&data, &layout, &binary), &manager).unwrap();
            Manifests::in_data_dir(&data)
                .record_keychain(&[KeychainEntry {
                    service: "owlshift".to_owned(),
                    account: "claude-agent".to_owned(),
                }])
                .unwrap();
            fs::create_dir_all(data.join("projects/github.com/acme/web/checkout")).unwrap();
            fs::write(data.join("events.lock"), "").unwrap();
            let personal_file = root.path().join("config.toml");
            fs::write(&personal_file, "").unwrap();
            let personal = Personal {
                file: &personal_file,
                own_folder: false,
            };
            let terminate =
                |pid: u32| -> io::Result<()> { panic!("no daemon runs, yet pid {pid}") };
            let keychain = || {
                let keychain = Keychain::in_memory();
                keychain
                    .store("claude-agent", &Secret::new("token"))
                    .unwrap();
                Ok(keychain)
            };
            let not_ours = root.path().join("not-ours");
            fs::create_dir_all(not_ours.join("projects")).unwrap();
            let refused = uninstall_with(&Uninstall {
                data: &not_ours,
                home: None,
                manager: &manager,
                terminate: &terminate,
                keychain: &keychain,
                purge: Some(None),
                stop_wait: Duration::ZERO,
            });
            assert!(
                refused.failed[0].contains("does not look like Owlshift's data directory"),
                "{refused:?}"
            );
            assert!(not_ours.join("projects").exists());

            let outcome = uninstall_with(&Uninstall {
                data: &data,
                home: Some(root.path()),
                manager: &manager,
                terminate: &terminate,
                keychain: &keychain,
                purge: Some(Some(&personal)),
                stop_wait: Duration::ZERO,
            });
            assert!(outcome.failed.is_empty(), "{outcome:?}");
            let said = outcome.said.join("\n");
            for line in [
                "Removed the service at login (",
                "Removed the keychain entry `claude-agent` (service `owlshift`).",
                "Removed the data directory ",
                "Removed the personal file ",
            ] {
                assert!(said.contains(line), "{line}\n{said}");
            }
            assert!(!data.exists() && !personal_file.exists() && !layout.unit().exists());
        }
    }

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
