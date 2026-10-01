//! The `owlshift` binary, end to end. Only `git` is needed on the host: the
//! tests that run `doctor` give it a fake `git` and interrupt it before it
//! looks at the harness CLIs installed.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

/// Runs `owlshift` in `dir`, with the personal configuration directory and
/// git's own environment overrides pointed away from the host's.
///
/// `config_dir` becomes `OWLSHIFT_CONFIG_DIR`, the directory that holds the
/// personal `config.toml` directly (`owlshift_platform::paths`) — this
/// redirects the personal file deterministically on every platform,
/// including Windows, where `dirs::config_dir()` reads the OS known-folder
/// API and ignores environment variables. `HOME` and `XDG_CONFIG_HOME` are
/// set too, but only to isolate the real `git rev-parse` subprocess this
/// binary shells out to from the host's own git configuration — git reads
/// `$XDG_CONFIG_HOME/git/config` independently of `HOME`, so both are needed
/// for that isolation; neither plays a part in resolving the personal
/// configuration file anymore. `OWLSHIFT_DATA_DIR` is `<config_dir>/data`.
///
/// No test here reaches the system keychain: `init` runs with
/// `--skip-secrets`, `do` is refused before it opens the keychain, and
/// `doctor` asks it for the agent runs' token only when `claude` is on the
/// `PATH`, which these tests keep to a fake `git` and the system folders.
fn owlshift(dir: &Path, config_dir: &Path, args: &[&str]) -> Output {
    command(dir, config_dir, args).output().unwrap()
}

fn command(dir: &Path, config_dir: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_owlshift"));
    command
        .args(args)
        .current_dir(dir)
        .env("HOME", config_dir)
        .env("XDG_CONFIG_HOME", config_dir)
        .env("OWLSHIFT_CONFIG_DIR", config_dir)
        .env("OWLSHIFT_DATA_DIR", config_dir.join("data"))
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_CEILING_DIRECTORIES");
    command
}

fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn git_init(dir: &Path) {
    let status = Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(dir)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .status()
        .unwrap();
    assert!(status.success());
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn version_lists_the_format_versions() {
    let config_dir = tempfile::tempdir().unwrap();
    let output = owlshift(config_dir.path(), config_dir.path(), &["--version"]);
    assert!(output.status.success());
    assert_eq!(
        stdout(&output),
        concat!(
            "owlshift ",
            env!("CARGO_PKG_VERSION"),
            "\nformats: brief 3, result 1, event 1, claim 1, ticket state 2, comment footer 1\n"
        )
    );
}

#[test]
fn config_show_gives_each_value_its_file() {
    let config_dir = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    git_init(repo.path());
    fs::write(
        repo.path().join("owlshift.toml"),
        r#"requires = ">=0.0"
[tracker]
kind = "markdown"
admit = "delegation"
states = { ready = "ready", working = "working", needs_input = "needs input", review = "review" }
[stack]
gate = ["cargo test"]
[pipeline]
default = "trivial"
plan_approval = "never"
[models]
[policy]
always_human = []
"#,
    )
    .unwrap();
    // From a nested directory with a space in its name.
    let nested = repo.path().join("src dir");
    fs::create_dir(&nested).unwrap();

    let output = owlshift(&nested, config_dir.path(), &["config", "show"]);
    let shown = stdout(&output);
    assert!(output.status.success(), "{shown}");
    assert!(shown.contains("tracker.kind = \"markdown\"  ("), "{shown}");
    assert!(shown.contains("owlshift.toml)\n"), "{shown}");

    fs::write(repo.path().join("owlshift.toml"), "requires = \">=99\"\n").unwrap();
    let output = owlshift(&nested, config_dir.path(), &["config", "show"]);
    assert!(!output.status.success());
    assert!(stdout(&output).contains("upgrade Owlshift"));
}

/// The acceptance criterion for OWL-30: a personal file present under
/// `OWLSHIFT_CONFIG_DIR` is actually read, on every platform (including
/// Windows, where nothing but this override redirects
/// `owlshift_platform::paths::personal_config_file`).
#[test]
fn personal_config_file_is_read_from_the_override_directory() {
    let config_dir = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    git_init(repo.path());

    let personal_file = config_dir.path().join("config.toml");
    fs::write(&personal_file, "concurrent_runs = 2\n").unwrap();

    let output = owlshift(repo.path(), config_dir.path(), &["config", "show"]);
    let shown = stdout(&output);
    assert!(output.status.success(), "{shown}");
    assert!(
        shown.contains(&format!("personal file: {}\n", personal_file.display())),
        "{shown}"
    );
    assert!(
        shown.contains(&format!(
            "concurrent_runs = 2  ({})",
            personal_file.display()
        )),
        "{shown}"
    );
    // Pins that the personal file was actually loaded, not merely that its
    // path looks right: the project file is absent in this fixture too (no
    // `owlshift.toml` written), and legitimately says "not found at" on its
    // own line, so the check is scoped to the personal-file line alone.
    let personal_line = shown
        .lines()
        .find(|line| line.starts_with("personal file:"))
        .unwrap_or_default();
    assert!(!personal_line.contains("not found at"), "{shown}");
}

/// `owlshift doctor` with only a fake `git` and the system folders on the
/// `PATH`, so no `claude` or `codex` of the host is found and the agent
/// login, which needs `claude`, is not asked of the keychain.
#[cfg(unix)]
fn doctor(args: &[&str]) -> Output {
    use std::os::unix::fs::PermissionsExt;

    let bin = tempfile::tempdir().unwrap();
    let git = bin.path().join("git");
    fs::write(
        &git,
        "#!/bin/sh\n\
         if [ \"$1\" = --version ]; then echo 'git version 2.54.0'; exit 0; fi\n\
         echo 'fatal: not a git repository' >&2\n\
         exit 128\n",
    )
    .unwrap();
    fs::set_permissions(&git, fs::Permissions::from_mode(0o755)).unwrap();
    let config_dir = tempfile::tempdir().unwrap();
    let mut all = vec!["doctor"];
    all.extend_from_slice(args);
    command(config_dir.path(), config_dir.path(), &all)
        .env("PATH", format!("{}:/usr/bin:/bin", bin.path().display()))
        .env_remove("NO_COLOR")
        .output()
        .unwrap()
}

/// OWL-99: redirected, the report has no colour, even with `NO_COLOR`
/// unset; it is grouped in sections, and closes on what to fix. `claude`
/// is missing, so the exit status is 1.
#[cfg(unix)]
#[test]
fn doctor_redirected_is_plain_text_by_section() {
    let output = doctor(&[]);
    let shown = stdout(&output);
    assert_eq!(output.status.code(), Some(1), "{shown}");
    assert!(!shown.contains('\x1b'), "{shown}");
    for heading in ["\nTools\n", "\nAgent isolation\n", "\nProject\n"] {
        assert!(shown.contains(heading), "{shown}");
    }
    assert!(shown.contains("  ✓ git "), "{shown}");
    assert!(shown.contains("  ✗ claude "), "{shown}");
    assert!(shown.contains("to fix before `owlshift do`"), "{shown}");
    assert!(shown.contains("owlshift doctor    (to check)"), "{shown}");
}

/// OWL-99: `--json` prints the same report as JSON, and keeps the exit
/// status.
#[cfg(unix)]
#[test]
fn doctor_json_is_the_same_report() {
    let output = doctor(&["--json"]);
    assert_eq!(output.status.code(), Some(1), "{}", stdout(&output));
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["ready"], false);
    assert!(report["problems"].as_u64().unwrap() >= 1);
    assert_eq!(report["next"], serde_json::Value::Null);
    let checks = report["checks"].as_array().unwrap();
    let claude = checks
        .iter()
        .find(|check| check["subject"] == "claude")
        .unwrap();
    assert_eq!(claude["status"], "fail");
    assert_eq!(claude["section"], "tools");
    assert_eq!(claude["detail"], "not found on the PATH");
    assert!(claude["why"].is_string());
    assert!(claude["fix"][0]["do"].is_string());
    for check in checks {
        for key in ["status", "section", "subject", "detail", "why", "fix"] {
            assert!(check.get(key).is_some(), "{key} missing: {check}");
        }
    }
}

/// The acceptance criterion for OWL-43: Ctrl-C on `doctor` while a probe
/// hangs stops the probe and the process it started, although the probe runs
/// in a process group of its own, out of the terminal's reach; and the CLI
/// ends promptly, killed by the signal it received.
#[cfg(unix)]
#[test]
fn ctrl_c_on_doctor_stops_a_hung_probe_and_its_child() {
    use std::os::unix::process::ExitStatusExt;

    const SIGINT: i32 = 2;

    let mut hung = HungProbe::start();
    hung.signal(&["-INT", &hung.owlshift.id().to_string()]);
    let status = hung.ended("SIGINT");
    assert_eq!(status.signal(), Some(SIGINT), "{status}");
    hung.assert_probe_gone();
}

/// The acceptance criterion for OWL-86: killing `doctor` outright while a
/// probe hangs, which runs no handler, still stops the probe and the process
/// it started, within five seconds. The whole process group `owlshift` leads
/// is killed, so what stops the probe, the sentinel, cannot be in that group.
#[cfg(unix)]
#[test]
fn a_hard_kill_of_doctor_stops_a_hung_probe_and_its_child() {
    use std::os::unix::process::ExitStatusExt;

    const SIGKILL: i32 = 9;

    let mut hung = HungProbe::start();
    // Owlshift tells the sentinel about a tree right after the spawn
    // returns, well under a millisecond after the probe started; this is
    // margin for a loaded runner, since a kill before that escapes.
    std::thread::sleep(std::time::Duration::from_millis(300));
    let group = format!("-{}", hung.owlshift.id());
    hung.signal(&["-s", "KILL", "--", &group]);
    let status = hung.ended("SIGKILL");
    assert_eq!(status.signal(), Some(SIGKILL), "{status}");
    hung.assert_probe_gone();
}

/// `owlshift doctor` whose git probe hangs with a child, `owlshift` leading a
/// process group of its own, so the tests can kill that group without
/// killing themselves.
#[cfg(unix)]
struct HungProbe {
    owlshift: std::process::Child,
    /// The probe's pid and its child's.
    pids: Vec<u32>,
    /// Where `doctor` writes its report.
    report: std::path::PathBuf,
    _bin: tempfile::TempDir,
    _config_dir: tempfile::TempDir,
}

#[cfg(unix)]
impl HungProbe {
    /// Starts `doctor` and waits until the probe and its child run.
    fn start() -> Self {
        use std::os::unix::fs::PermissionsExt;
        use std::os::unix::process::CommandExt;
        use std::process::Stdio;
        use std::thread;
        use std::time::{Duration, Instant};

        // A `git` whose `--version` starts a long-lived child, records both
        // pids, and waits on the child; anything else answers as git does
        // outside a repository, so loading the configuration does not hang.
        let bin = tempfile::tempdir().unwrap();
        let pids = bin.path().join("pids");
        let git = bin.path().join("git");
        fs::write(
            &git,
            format!(
                "#!/bin/sh\n\
                 if [ \"$1\" = --version ]; then\n\
                 \x20 sleep 30 &\n\
                 \x20 echo \"$$ $!\" > '{pids}.tmp'\n\
                 \x20 mv '{pids}.tmp' '{pids}'\n\
                 \x20 wait\n\
                 fi\n\
                 echo 'fatal: not a git repository' >&2\n\
                 exit 128\n",
                pids = pids.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&git, fs::Permissions::from_mode(0o755)).unwrap();

        let config_dir = tempfile::tempdir().unwrap();
        let report = config_dir.path().join("report");
        let mut owlshift = Command::new(env!("CARGO_BIN_EXE_owlshift"))
            .arg("doctor")
            .current_dir(config_dir.path())
            .env("PATH", format!("{}:/usr/bin:/bin", bin.path().display()))
            .env("HOME", config_dir.path())
            .env("XDG_CONFIG_HOME", config_dir.path())
            .env("OWLSHIFT_CONFIG_DIR", config_dir.path())
            .stdout(fs::File::create(&report).unwrap())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap();

        let probe_started = Instant::now() + Duration::from_secs(10);
        while !pids.exists() {
            if let Some(status) = owlshift.try_wait().unwrap() {
                panic!("owlshift ended before its probe started: {status}");
            }
            assert!(Instant::now() < probe_started, "the probe never started");
            thread::sleep(Duration::from_millis(20));
        }
        let pids = fs::read_to_string(&pids)
            .unwrap()
            .split_whitespace()
            .map(|pid| pid.parse().unwrap())
            .collect();
        Self {
            owlshift,
            pids,
            report,
            _bin: bin,
            _config_dir: config_dir,
        }
    }

    /// The pid of `owlshift`'s sentinel, its one child named so.
    fn sentinel(&self) -> u32 {
        let found = Command::new("pgrep")
            .args([
                "-P",
                &self.owlshift.id().to_string(),
                "-f",
                "owlshift-sentinel",
            ])
            .output()
            .unwrap();
        let found = String::from_utf8(found.stdout).unwrap();
        let sentinel: Vec<&str> = found.split_whitespace().collect();
        assert_eq!(sentinel.len(), 1, "sentinels found: {found:?}");
        sentinel[0].parse().unwrap()
    }

    /// Runs `kill` with `args`.
    fn signal(&self, args: &[&str]) {
        let sent = Command::new("kill").args(args).status().unwrap();
        assert!(sent.success(), "kill {args:?}");
    }

    /// Waits for `owlshift` to end after `signal`, promptly: well before the
    /// probe's own ten-second deadline.
    fn ended(&mut self, signal: &str) -> std::process::ExitStatus {
        use std::thread;
        use std::time::{Duration, Instant};

        let ended = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = self.owlshift.try_wait().unwrap() {
                return status;
            }
            if Instant::now() >= ended {
                let _ = self.owlshift.kill();
                panic!("owlshift still running 5 s after {signal}");
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// Checks the probe and its child are gone within five seconds: killed
    /// processes take a moment to be gone, as an orphan is reaped by the
    /// system.
    fn assert_probe_gone(&self) {
        use std::thread;
        use std::time::{Duration, Instant};

        let settled = Instant::now() + Duration::from_secs(5);
        while self.pids.iter().any(|&pid| is_alive(pid)) {
            assert!(Instant::now() < settled, "still running: {:?}", self.pids);
            thread::sleep(Duration::from_millis(50));
        }
    }
}

/// Cleans up after a failed test: nothing it started keeps running. Only
/// then: once a test passed, the probe's pids may already be another
/// process's.
#[cfg(unix)]
impl Drop for HungProbe {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            return;
        }
        for pid in &self.pids {
            let _ = Command::new("kill")
                .args(["-s", "KILL", &pid.to_string()])
                .status();
        }
        let _ = self.owlshift.kill();
        let _ = self.owlshift.wait();
    }
}

/// OWL-88: `doctor` reports a sentinel killed while it runs, with how it
/// ended, as a warning; and writing to the dead sentinel's pipe, when the
/// probe ends, does not end Owlshift.
#[cfg(unix)]
#[test]
fn doctor_reports_a_sentinel_killed_while_it_runs() {
    use std::os::unix::process::ExitStatusExt;

    let mut hung = HungProbe::start();
    let sentinel = hung.sentinel();
    hung.signal(&["-s", "KILL", &sentinel.to_string()]);
    // Gone before the probe ends and `doctor` reads its state; well within
    // the probe's ten-second deadline.
    wait_for("the sentinel to be gone", || !is_alive(sentinel));
    assert!(
        hung.owlshift.try_wait().unwrap().is_none(),
        "doctor ended early"
    );
    for pid in hung.pids.clone() {
        hung.signal(&["-s", "KILL", &pid.to_string()]);
    }

    let status = hung.ended("the end of its probe");
    assert_eq!(status.signal(), None, "{status}");
    let report = fs::read_to_string(&hung.report).unwrap();
    let line = sentinel_line(&report);
    assert_eq!(line.split_whitespace().next(), Some("!"), "{report}");
    assert!(line.contains("ended (signal: 9"), "{report}");
}

/// OWL-91: `doctor` reports a sentinel stopped while it runs, although it
/// still runs, as a warning.
#[cfg(unix)]
#[test]
fn doctor_reports_a_sentinel_stopped_while_it_runs() {
    use std::os::unix::process::ExitStatusExt;

    let mut hung = HungProbe::start();
    let sentinel = hung.sentinel();
    let _left = KillIfStopped(sentinel);
    hung.signal(&["-s", "STOP", &sentinel.to_string()]);
    wait_for("the sentinel to stop", || state(sentinel).starts_with('T'));
    for pid in hung.pids.clone() {
        hung.signal(&["-s", "KILL", &pid.to_string()]);
    }

    let status = hung.ended("the end of its probe");
    assert_eq!(status.signal(), None, "{status}");
    let report = fs::read_to_string(&hung.report).unwrap();
    let line = sentinel_line(&report);
    assert_eq!(line.split_whitespace().next(), Some("!"), "{report}");
    assert!(
        line.contains(&format!("stopped (pid {sentinel})")),
        "{report}"
    );
}

/// Kills, when dropped, a sentinel left stopped: only while it is
/// stopped and its arguments name a sentinel, so that its pid, not reaped, is
/// still its own. The system ends it once `owlshift` ended, where that
/// leaves its process group orphaned (OWL-91), not everywhere.
#[cfg(unix)]
struct KillIfStopped(u32);

#[cfg(unix)]
impl Drop for KillIfStopped {
    fn drop(&mut self) {
        // Nothing here may panic: it also runs while a failed test unwinds.
        let sentinel = test_proc::command(self.0).contains("owlshift-sentinel");
        let stopped = state(self.0).starts_with('T');
        if sentinel && stopped {
            let _ = Command::new("kill")
                .args(["-s", "KILL", &self.0.to_string()])
                .status();
        }
    }
}

/// The `sentinel` line of a `doctor` report.
#[cfg(unix)]
fn sentinel_line(report: &str) -> &str {
    report
        .lines()
        .find(|line| line.split_whitespace().nth(1) == Some("sentinel"))
        .unwrap_or_else(|| panic!("no sentinel line:\n{report}"))
}

/// Waits until `done` holds, 5 s at most.
#[cfg(unix)]
fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    use std::thread;
    use std::time::{Duration, Instant};

    let deadline = Instant::now() + Duration::from_secs(5);
    while !done() {
        assert!(Instant::now() < deadline, "5 s without {what}");
        thread::sleep(Duration::from_millis(20));
    }
}

// A process's state, with no `ps`: it is setuid on macOS, which a confined
// process may not run (OWL-110).
#[cfg(unix)]
#[path = "../../owlshift-platform/src/test_proc.rs"]
mod test_proc;
#[cfg(unix)]
use test_proc::{is_alive, state};

/// The acceptance criterion for OWL-47: on Windows, a console Ctrl-C on
/// `doctor` while its git probe hangs stops the probe and the process it
/// started, although both ignore Ctrl-C; and the CLI ends promptly, as Ctrl-C
/// ends a console program.
#[cfg(windows)]
#[test]
fn ctrl_c_on_doctor_stops_a_probe_that_ignores_it_and_its_child() {
    windows_ctrl_c::interrupt_doctor();
}

/// Helper, run in a console of its own by `interrupt_doctor`.
#[cfg(windows)]
#[test]
#[ignore = "helper, run by the Windows Ctrl-C test"]
fn helper_interrupt_doctor() {
    if windows_ctrl_c::helper_requested() {
        windows_ctrl_c::interrupt_in_this_console();
    }
}

/// Helper, run as `git --version` by the batch file `interrupt_doctor`
/// writes.
#[cfg(windows)]
#[test]
#[ignore = "helper, run by the Windows Ctrl-C test"]
fn helper_hung_git() {
    if windows_ctrl_c::helper_requested() {
        windows_ctrl_c::hang_with_a_child();
    }
}

/// Helper, the child `helper_hung_git` starts.
#[cfg(windows)]
#[test]
#[ignore = "helper, run by the Windows Ctrl-C test"]
fn helper_sleep() {
    if windows_ctrl_c::helper_requested() {
        std::thread::sleep(std::time::Duration::from_secs(30));
    }
}

/// A real console Ctrl-C on `owlshift doctor`, on Windows.
///
/// Ctrl-C reaches every process attached to a console, so it cannot be sent
/// from the test process, whose console the test runner and the other tests
/// share. The test starts a copy of itself in a new console with no window,
/// the driver, which runs `owlshift doctor` there, sends that console a
/// Ctrl-C (`GenerateConsoleCtrlEvent`) once the probe has started, and stays
/// attached to it, as the user's shell does, while it watches the probe.
#[cfg(windows)]
mod windows_ctrl_c {
    use std::fs::{self, File};
    use std::io;
    use std::os::windows::process::CommandExt;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;
    use std::time::{Duration, Instant};

    use windows_sys::Win32::Foundation::STATUS_CONTROL_C_EXIT;
    use windows_sys::Win32::System::Console::{
        CTRL_C_EVENT, GenerateConsoleCtrlEvent, SetConsoleCtrlHandler,
    };
    use windows_sys::Win32::System::Threading::CREATE_NO_WINDOW;

    /// The folder holding the fake `git`, for the driver.
    const BIN: &str = "OWLSHIFT_TEST_BIN";
    /// The personal configuration folder, for the driver.
    const CONFIG_DIR: &str = "OWLSHIFT_TEST_CONFIG_DIR";
    /// Where the hung probe writes its pid and its child's.
    const PIDS: &str = "OWLSHIFT_TEST_PIDS";

    /// The helpers act only when run alone, never in a plain
    /// `cargo test -- --ignored`.
    pub(super) fn helper_requested() -> bool {
        std::env::args().any(|arg| arg == "--exact")
    }

    fn helper(name: &str) -> Command {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command.args([
            "--exact",
            name,
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ]);
        command
    }

    fn var(name: &str) -> PathBuf {
        std::env::var_os(name)
            .unwrap_or_else(|| panic!("{name} is not set"))
            .into()
    }

    /// The pids the hung probe wrote, its own then its child's.
    fn read_pids(pids: &Path) -> Vec<u32> {
        fs::read_to_string(pids)
            .unwrap_or_default()
            .split_whitespace()
            .filter_map(|word| word.parse().ok())
            .collect()
    }

    pub(super) fn interrupt_doctor() {
        let bin = tempfile::tempdir().unwrap();
        let pids = bin.path().join("pids");
        // This binary cannot answer `git --version` itself, since the test
        // harness rejects the option: a batch file, the form npm installs a
        // CLI in, starts the hung probe for `--version` and answers anything
        // else as git does outside a repository, so loading the
        // configuration does not hang.
        let exe = std::env::current_exe().unwrap();
        fs::write(
            bin.path().join("git.cmd"),
            format!(
                "@echo off\r\n\
                 if not \"%~1\"==\"--version\" goto other\r\n\
                 \"{exe}\" --exact helper_hung_git --ignored --nocapture --test-threads=1\r\n\
                 exit /b %errorlevel%\r\n\
                 :other\r\n\
                 echo fatal: not a git repository 1>&2\r\n\
                 exit /b 128\r\n",
                exe = exe.display()
            ),
        )
        .unwrap();

        let config_dir = tempfile::tempdir().unwrap();
        let log = bin.path().join("driver.log");
        let mut driver = helper("helper_interrupt_doctor");
        driver
            .env(BIN, bin.path())
            .env(CONFIG_DIR, config_dir.path())
            .env(PIDS, &pids)
            .creation_flags(CREATE_NO_WINDOW)
            .stdin(Stdio::null())
            .stdout(File::create(&log).unwrap())
            .stderr(File::create(bin.path().join("driver.err")).unwrap());
        let mut driver = driver.spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(60);
        let status = loop {
            if let Some(status) = driver.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                // With `owlshift` and the probe, if still there.
                stop(&[driver.id()]);
                break driver.wait().unwrap();
            }
            thread::sleep(Duration::from_millis(50));
        };
        let report = format!(
            "driver {status}\n--- stdout\n{}\n--- stderr\n{}",
            fs::read_to_string(&log).unwrap_or_default(),
            fs::read_to_string(bin.path().join("driver.err")).unwrap_or_default()
        );
        eprintln!("{report}");
        if !status.success() {
            // The driver stops what it started; this is for a driver that
            // did not get that far.
            stop(&read_pids(&pids));
            panic!("{report}");
        }
    }

    /// Stops what a failed run left, best effort.
    fn stop(pids: &[u32]) {
        for pid in pids {
            let _ = Command::new("taskkill")
                .args(["/F", "/T", "/PID", &pid.to_string()])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    }

    static CTRL_C_SEEN: AtomicBool = AtomicBool::new(false);

    unsafe extern "system" fn note_ctrl_c(event: u32) -> windows_sys::core::BOOL {
        if event == CTRL_C_EVENT {
            CTRL_C_SEEN.store(true, Ordering::SeqCst);
        }
        // Handled: the driver outlives the Ctrl-C it sends.
        1
    }

    /// The driver: runs `owlshift doctor` in this console, sends the console
    /// a Ctrl-C once the probe has started, checks how `owlshift` ended, and
    /// that the probe and its child are gone within 5 s.
    pub(super) fn interrupt_in_this_console() {
        let bin = var(BIN);
        let config_dir = var(CONFIG_DIR);
        let pids = var(PIDS);

        // Ctrl-C is processed here and in the processes started from here,
        // whatever this process inherited: a parent can turn it off for its
        // descendants.
        // SAFETY: no handler routine is passed.
        let enabled = unsafe { SetConsoleCtrlHandler(None, 0) };
        assert_ne!(enabled, 0, "{}", io::Error::last_os_error());

        let system_root = var("SystemRoot");
        let path = std::env::join_paths([
            bin.clone(),
            system_root.join("System32"),
            system_root.clone(),
        ])
        .unwrap();
        let mut owlshift = Command::new(env!("CARGO_BIN_EXE_owlshift"))
            .arg("doctor")
            .current_dir(&config_dir)
            .env("PATH", path)
            .env("HOME", &config_dir)
            .env("XDG_CONFIG_HOME", &config_dir)
            .env("OWLSHIFT_CONFIG_DIR", &config_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();

        // Once `owlshift` runs: this process survives its own Ctrl-C and
        // records it, proving the console got one.
        // SAFETY: `note_ctrl_c` has the signature of a handler routine and
        // stays valid for the life of the process.
        let noted = unsafe { SetConsoleCtrlHandler(Some(note_ctrl_c), 1) };
        assert_ne!(noted, 0, "{}", io::Error::last_os_error());

        let probe_started = Instant::now() + Duration::from_secs(10);
        while !pids.exists() {
            if let Some(status) = owlshift.try_wait().unwrap() {
                panic!("owlshift ended before its probe started: {status}");
            }
            assert!(Instant::now() < probe_started, "the probe never started");
            thread::sleep(Duration::from_millis(20));
        }
        println!("probe: {}", fs::read_to_string(&pids).unwrap());
        let started = read_pids(&pids);
        assert_eq!(started.len(), 2, "{started:?}");
        // So that a probe gone for another reason is never taken for one the
        // Ctrl-C stopped.
        let early: Vec<u32> = started
            .iter()
            .copied()
            .filter(|&pid| !is_alive(pid))
            .collect();
        if !early.is_empty() {
            let _ = owlshift.kill();
            stop(&started);
            panic!("gone before the Ctrl-C: {early:?} of the probe and its child {started:?}");
        }

        let sent = Instant::now();
        // SAFETY: no pointer argument; group 0 is every process attached to
        // this console.
        let generated = unsafe { GenerateConsoleCtrlEvent(CTRL_C_EVENT, 0) };
        assert_ne!(generated, 0, "{}", io::Error::last_os_error());

        // Promptly: well before the probe's own ten-second deadline.
        let status = loop {
            if let Some(status) = owlshift.try_wait().unwrap() {
                break status;
            }
            if sent.elapsed() >= Duration::from_secs(5) {
                let _ = owlshift.kill();
                stop(&started);
                panic!(
                    "owlshift still running 5 s after Ctrl-C (seen by the driver: {})",
                    CTRL_C_SEEN.load(Ordering::SeqCst)
                );
            }
            thread::sleep(Duration::from_millis(20));
        };
        let ended = sent.elapsed();
        // The driver's own handler runs on a thread of its own, maybe later.
        while !CTRL_C_SEEN.load(Ordering::SeqCst) && sent.elapsed() < ended + Duration::from_secs(1)
        {
            thread::sleep(Duration::from_millis(10));
        }
        let seen = CTRL_C_SEEN.load(Ordering::SeqCst);
        println!(
            "owlshift ended {ended:?} after Ctrl-C: {status}; Ctrl-C seen by the driver: {seen}"
        );

        // A process ends asynchronously once stopped.
        let mut gone = [None, None];
        while gone.iter().any(Option::is_none) && sent.elapsed() < ended + Duration::from_secs(5) {
            for (at, &pid) in gone.iter_mut().zip(&started) {
                if at.is_none() && !is_alive(pid) {
                    *at = Some(sent.elapsed());
                }
            }
            thread::sleep(Duration::from_millis(20));
        }
        println!(
            "after Ctrl-C, gone: probe {:?}, child {:?}",
            gone[0], gone[1]
        );
        let alive: Vec<u32> = started
            .iter()
            .copied()
            .filter(|&pid| is_alive(pid))
            .collect();
        stop(&alive);

        assert!(seen, "the console got no Ctrl-C");
        assert_eq!(
            status.code().map(|code| code as u32),
            Some(STATUS_CONTROL_C_EXIT as u32),
            "{status}"
        );
        assert!(
            alive.is_empty(),
            "still running 5 s after owlshift ended: {alive:?} of the probe and its child {started:?}"
        );
    }

    /// The hung probe: ignores Ctrl-C, which the child it then starts
    /// inherits, records both pids and waits on the child.
    pub(super) fn hang_with_a_child() {
        // SAFETY: no handler routine is passed.
        let ignored = unsafe { SetConsoleCtrlHandler(None, 1) };
        assert_ne!(ignored, 0, "{}", io::Error::last_os_error());
        let mut child = helper("helper_sleep").spawn().unwrap();
        let pids = var(PIDS);
        let staged = pids.with_extension("tmp");
        fs::write(&staged, format!("{} {}", std::process::id(), child.id())).unwrap();
        fs::rename(&staged, &pids).unwrap();
        child.wait().unwrap();
    }

    /// Whether a process is still running.
    fn is_alive(pid: u32) -> bool {
        use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
        use windows_sys::Win32::Foundation::{
            ERROR_INVALID_PARAMETER, WAIT_OBJECT_0, WAIT_TIMEOUT,
        };
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
        };

        // SAFETY: no pointer argument.
        let raw = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
        if raw.is_null() {
            // The only expected failure: no process has this id any more.
            let error = io::Error::last_os_error();
            assert_eq!(
                error.raw_os_error(),
                Some(ERROR_INVALID_PARAMETER as i32),
                "{error}"
            );
            return false;
        }
        // SAFETY: the call succeeded, so `raw` is a new handle owned here.
        let process = unsafe { OwnedHandle::from_raw_handle(raw) };
        // SAFETY: `process` is open for the duration of the call.
        match unsafe { WaitForSingleObject(process.as_raw_handle(), 0) } {
            WAIT_TIMEOUT => true,
            WAIT_OBJECT_0 => false,
            other => panic!("wait returned {other}: {}", io::Error::last_os_error()),
        }
    }
}

#[test]
fn init_writes_a_commented_project_file_once() {
    let config_dir = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    git_init(repo.path());
    let args = [
        "init",
        "--tracker",
        "markdown",
        "--gate",
        "cargo fmt --check",
        "--gate",
        "cargo test",
        "--skip-secrets",
    ];

    let output = owlshift(repo.path(), config_dir.path(), &args);
    assert!(output.status.success(), "{}", stderr(&output));
    let path = repo.path().join("owlshift.toml");
    let written = fs::read_to_string(&path).unwrap();
    let config = owlshift_contracts::config::ProjectConfig::parse(&written).unwrap();
    assert_eq!(config.stack.gate, ["cargo fmt --check", "cargo test"]);
    assert!(
        written.starts_with("# Owlshift project configuration"),
        "{written}"
    );

    // Run again, with other values: the file stays as it was.
    let output = owlshift(
        repo.path(),
        config_dir.path(),
        &["init", "--team", "OWL", "--skip-secrets"],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stdout(&output).contains("exists: kept"),
        "{}",
        stdout(&output)
    );
    assert_eq!(fs::read_to_string(&path).unwrap(), written);

    // Linear needs its team; outside a repository there is no project.
    let other = tempfile::tempdir().unwrap();
    git_init(other.path());
    let output = owlshift(other.path(), config_dir.path(), &["init", "--skip-secrets"]);
    assert!(!output.status.success());
    assert!(stderr(&output).contains("--team"), "{}", stderr(&output));
    assert!(!other.path().join("owlshift.toml").exists());
    let outside = tempfile::tempdir().unwrap();
    let output = owlshift(
        outside.path(),
        config_dir.path(),
        &["init", "--skip-secrets"],
    );
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("git repository"),
        "{}",
        stderr(&output)
    );
}

/// What `do` refuses before it opens the keychain.
#[test]
fn do_refuses_a_project_it_cannot_deliver_before_any_credential() {
    let config_dir = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    git_init(repo.path());

    let output = owlshift(repo.path(), config_dir.path(), &["do", "OWL-1"]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("run `owlshift init`"),
        "{}",
        stderr(&output)
    );

    let init = ["init", "--tracker", "markdown", "--skip-secrets"];
    assert!(
        owlshift(repo.path(), config_dir.path(), &init)
            .status
            .success()
    );
    let bare = tempfile::tempdir().unwrap();
    let bare = bare.path().to_string_lossy().into_owned();
    git(repo.path(), &["remote", "add", "origin", &bare]);

    // OWL-63: a variable the project declares for its gate needs the
    // operator's allowance, or the project file is refused.
    let project_file = repo.path().join("owlshift.toml");
    let written = fs::read_to_string(&project_file).unwrap();
    let declaring = written.replace(
        "# gate_env = [\"FEATURE_FLAGS\"]",
        "gate_env = [\"DATABASE_URL\"]",
    );
    assert_ne!(declaring, written);
    fs::write(&project_file, declaring).unwrap();
    let output = owlshift(repo.path(), config_dir.path(), &["do", "OWL-1"]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("DATABASE_URL may not reach an agent on this machine")
            && stderr(&output).contains("`allow_gate_env`"),
        "{}",
        stderr(&output)
    );
    fs::write(
        config_dir.path().join("config.toml"),
        "allow_gate_env = [\"DATABASE_URL\"]\n",
    )
    .unwrap();

    let output = owlshift(repo.path(), config_dir.path(), &["do", "OWL-1"]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("not a github.com repository"),
        "{}",
        stderr(&output)
    );

    // A Linear project runs its own team's tickets only.
    let linear = tempfile::tempdir().unwrap();
    git_init(linear.path());
    let init = ["init", "--team", "OWL", "--skip-secrets"];
    assert!(
        owlshift(linear.path(), config_dir.path(), &init)
            .status
            .success()
    );
    let output = owlshift(linear.path(), config_dir.path(), &["do", "LOC-12"]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("LOC-12 is not a ticket of team OWL"),
        "{}",
        stderr(&output)
    );
    assert!(!config_dir.path().join("data").exists());
}

const EVENTS: &str = concat!(
    r#"{"format":1,"at":"2026-09-29T10:00:00Z","project":"demo/project","ticket":"OWL-1","kind":"dispatch","data":{"branch":"owlshift/owl-1"}}"#,
    "\n",
    r#"{"format":1,"at":"2026-09-29T10:01:00Z","project":"demo/project","ticket":"OWL-2","run":"r1","kind":"run_started","data":{"role":"build"}}"#,
    "\n",
);

#[test]
fn logs_prints_the_events_of_every_ticket_or_one() {
    let config_dir = tempfile::tempdir().unwrap();
    let output = owlshift(config_dir.path(), config_dir.path(), &["logs"]);
    assert!(output.status.success());
    assert!(
        stderr(&output).contains("no event recorded yet"),
        "{}",
        stderr(&output)
    );

    let data = config_dir.path().join("data");
    fs::create_dir_all(&data).unwrap();
    fs::write(data.join("events.jsonl"), EVENTS).unwrap();
    let all = stdout(&owlshift(config_dir.path(), config_dir.path(), &["logs"]));
    assert_eq!(
        all,
        "2026-09-29T10:00:00Z OWL-1 dispatch branch=owlshift/owl-1\n\
         2026-09-29T10:01:00Z OWL-2 run_started run=r1 role=build\n"
    );
    let one = stdout(&owlshift(
        config_dir.path(),
        config_dir.path(),
        &["logs", "OWL-2"],
    ));
    assert_eq!(
        one,
        "2026-09-29T10:01:00Z OWL-2 run_started run=r1 role=build\n"
    );
}

#[test]
fn logs_follow_prints_events_as_they_are_recorded() {
    use std::io::{BufRead, BufReader, Write};
    use std::process::Stdio;
    use std::sync::mpsc;
    use std::time::Duration;

    let config_dir = tempfile::tempdir().unwrap();
    let data = config_dir.path().join("data");
    fs::create_dir_all(&data).unwrap();
    let (first, second) = EVENTS.split_once('\n').unwrap();
    fs::write(data.join("events.jsonl"), format!("{first}\n")).unwrap();

    let mut child = command(config_dir.path(), config_dir.path(), &["logs", "--follow"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let (lines, received) = mpsc::channel();
    let reader = BufReader::new(child.stdout.take().unwrap());
    std::thread::spawn(move || {
        for line in reader.lines() {
            if lines.send(line.unwrap()).is_err() {
                break;
            }
        }
    });
    let next = || received.recv_timeout(Duration::from_secs(10));
    let seen = next();
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(data.join("events.jsonl"))
        .unwrap();
    file.write_all(second.as_bytes()).unwrap();
    let appended = next();
    // Ended by the test, as Ctrl-C would: `--follow` has no end of its own.
    child.kill().unwrap();
    child.wait().unwrap();
    assert_eq!(
        seen.as_deref(),
        Ok("2026-09-29T10:00:00Z OWL-1 dispatch branch=owlshift/owl-1")
    );
    assert_eq!(
        appended.as_deref(),
        Ok("2026-09-29T10:01:00Z OWL-2 run_started run=r1 role=build")
    );
}

/// The repository's own project file, which `owlshift do` reads on
/// Owlshift's tickets, parses and gates on what CI runs.
#[test]
fn the_repositorys_own_project_file_gates_on_what_ci_runs() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let text = fs::read_to_string(root.join("owlshift.toml")).unwrap();
    let config = owlshift_contracts::config::ProjectConfig::parse(&text).unwrap();
    assert_eq!(config.tracker.team.as_deref(), Some("OWL"));
    let ci = fs::read_to_string(root.join(".github/workflows/ci.yml")).unwrap();
    let runs: Vec<&str> = ci
        .lines()
        .filter_map(|line| line.trim().strip_prefix("run: "))
        .collect();
    assert!(!config.stack.gate.is_empty());
    for command in &config.stack.gate {
        assert!(
            runs.contains(&command.as_str()),
            "{command} is not a CI step"
        );
    }
    for step in ["cargo fmt", "cargo clippy", "cargo test"] {
        assert!(
            config.stack.gate.iter().any(|c| c.starts_with(step)),
            "the gate misses CI's {step}"
        );
    }
}
