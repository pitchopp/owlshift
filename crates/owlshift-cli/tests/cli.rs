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
/// `doctor` also reads the Linear key, and Linear, in a Linear project
/// (OWL-147): these tests run it outside any project, and must never in one.
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
    let text = stdout(&output);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(
        lines[0],
        concat!("owlshift ", env!("CARGO_PKG_VERSION")),
        "{text}"
    );
    // OWL-462: the build, whichever tree this test was built in.
    assert!(
        lines[1].starts_with("build: ") && lines[1].contains(", built "),
        "{text}"
    );
    assert_eq!(
        lines[2..],
        [
            "formats: brief 30, result 14, event 2, claim 1, ticket state 4, ticket questions 18, \
             comment footer 1"
        ],
        "{text}"
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

/// OWL-434: the service's watch finds its project in the list of served
/// projects, never in the current directory, and refuses one it does not
/// serve; `--served` goes with `--drain` only.
#[test]
fn a_served_watch_is_refused_for_a_project_not_served() {
    let config_dir = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let output = owlshift(
        elsewhere.path(),
        config_dir.path(),
        &["watch", "--drain", "--served", "acme/app"],
    );
    // OWL-435: a refusal no restart mends, which the daemon pauses on.
    assert_eq!(output.status.code(), Some(78), "{}", stderr(&output));
    assert!(
        stderr(&output).contains(
            "acme/app is not served on this machine: `owlshift start` in its checkout serves it"
        ),
        "{}",
        stderr(&output)
    );

    let output = owlshift(
        elsewhere.path(),
        config_dir.path(),
        &["watch", "--served", "acme/app"],
    );
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert!(stderr(&output).contains("--drain"), "{}", stderr(&output));
}

/// A project file `owlshift watch --drain` accepts, of a Linear project.
#[cfg(unix)]
const DRAINED_PROJECT: &str = r#"requires = ">=0.0"
[tracker]
kind = "linear"
team = "WEB"
admit = { label = "agent" }
admitters = ["u1"]
states = { ready = "Todo", working = "Doing", needs_input = "Asked", review = "Review" }
[stack]
gate = ["true"]
[pipeline]
default = "trivial"
plan_approval = "never"
[models]
[policy]
always_human = []
"#;

/// A port of 127.0.0.1 free now.
#[cfg(unix)]
fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Writes the personal file of `config_dir`: no notification shown, and the
/// web UI on `port` (OWL-467), so a daemon the test starts binds a port of
/// its own.
#[cfg(unix)]
fn personal_file(config_dir: &Path, port: u16) {
    fs::write(
        config_dir.join("config.toml"),
        format!("[notifications]\ndesktop = false\n\n[ui]\nport = {port}\n"),
    )
    .unwrap();
}

/// A checkout of `acme/web` holding [`DRAINED_PROJECT`].
#[cfg(unix)]
fn web_checkout() -> tempfile::TempDir {
    let checkout = tempfile::tempdir().unwrap();
    git_init(checkout.path());
    git(
        checkout.path(),
        &["remote", "add", "origin", "https://github.com/acme/web.git"],
    );
    fs::write(checkout.path().join("owlshift.toml"), DRAINED_PROJECT).unwrap();
    checkout
}

/// OWL-435: the real daemon, which the tests run on a data directory of
/// their own with no notification shown. Its one served project's checkout
/// is gone, so its watch refuses before any credential, with the code no
/// restart mends: the project is paused, with the reason. A second daemon
/// exits at once naming the first; `start` in a checkout registers it for
/// the running daemon; `stop` ends the daemon, `stop --forget` stops serving
/// the project.
#[cfg(unix)]
#[test]
fn the_daemon_runs_once_pauses_a_project_it_cannot_serve_and_stops() {
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;

    let config_dir = tempfile::tempdir().unwrap();
    personal_file(config_dir.path(), free_port());
    let data = config_dir.path().join("data");
    let service = data.join("service");
    fs::create_dir_all(&service).unwrap();
    let gone = config_dir.path().join("gone");
    fs::write(
        service.join("projects.json"),
        format!(
            "{{\"format\": 1, \"projects\": [{{\"repository\": \"acme/gone\", \"checkout\": {}}}]}}",
            serde_json::Value::String(gone.display().to_string())
        ),
    )
    .unwrap();
    let log = config_dir.path().join("daemon.log");
    let mut daemon = command(config_dir.path(), config_dir.path(), &["daemon"])
        .stdin(Stdio::null())
        .stdout(fs::File::create(&log).unwrap())
        .stderr(fs::File::create(config_dir.path().join("daemon.err")).unwrap())
        .process_group(0)
        .spawn()
        .unwrap();
    let said = || {
        format!(
            "{}{}",
            fs::read_to_string(&log).unwrap_or_default(),
            fs::read_to_string(config_dir.path().join("daemon.err")).unwrap_or_default()
        )
    };
    let record = service.join("daemon.json");
    wait::until_with("the daemon's record", said, || record.exists());

    let second = owlshift(config_dir.path(), config_dir.path(), &["daemon"]);
    assert_eq!(second.status.code(), Some(1), "{}", stderr(&second));
    assert!(
        stderr(&second).contains(&format!("a daemon already runs on {}", data.display())),
        "{}",
        stderr(&second)
    );
    assert!(
        stderr(&second).contains(&format!("(pid {}", daemon.id())),
        "{}",
        stderr(&second)
    );

    let pause = data.join("projects/github.com/acme/gone/pause");
    wait::until_with("acme/gone's pause", said, || pause.exists());
    let paused = fs::read_to_string(&pause).unwrap();
    assert!(paused.contains("\"by\":\"owlshift daemon\""), "{paused}");
    assert!(paused.contains("which is gone"), "{paused}");

    let checkout = web_checkout();
    let started = owlshift(checkout.path(), config_dir.path(), &["start"]);
    assert!(started.status.success(), "{}", stderr(&started));
    assert!(
        stdout(&started).contains("acme/web is served from"),
        "{}",
        stdout(&started)
    );
    assert!(
        stdout(&started).contains("serves it from its next pass"),
        "{}",
        stdout(&started)
    );

    let stopped = owlshift(config_dir.path(), config_dir.path(), &["stop"]);
    assert!(stopped.status.success(), "{}", stderr(&stopped));
    assert!(
        stdout(&stopped).contains(&format!("Stopping the daemon (pid {})", daemon.id())),
        "{}",
        stdout(&stopped)
    );
    let mut status = None;
    if !wait::reached(wait::REACHED, || {
        status = daemon.try_wait().unwrap();
        status.is_some()
    }) {
        let _ = daemon.kill();
        panic!("the daemon still runs after `owlshift stop`\n{}", said());
    }
    let status = status.unwrap();
    assert!(status.success(), "{status}\n{}", said());
    assert!(!record.exists());
    assert!(!service.join("stop").exists());

    let again = owlshift(config_dir.path(), config_dir.path(), &["stop"]);
    assert!(again.status.success());
    assert!(
        stdout(&again).starts_with("No daemon runs on"),
        "{}",
        stdout(&again)
    );
    let forgot = owlshift(checkout.path(), config_dir.path(), &["stop", "--forget"]);
    assert!(forgot.status.success(), "{}", stderr(&forgot));
    assert_eq!(stdout(&forgot), "acme/web is no longer served.\n");
}

/// OWL-437: `uninstall` with an install manifest listing nothing removes
/// nothing; `--purge` removes Owlshift's entries of the data directory and
/// the personal file, and names a file of the person's it leaves. No service
/// and no keychain entry is listed, so neither the service manager nor the
/// keychain is reached.
#[cfg(unix)]
#[test]
fn uninstall_purges_owlshifts_entries_and_leaves_the_rest() {
    let config_dir = tempfile::tempdir().unwrap();
    let personal = config_dir.path().join("config.toml");
    fs::write(&personal, "").unwrap();
    let data = config_dir.path().join("data");
    let nothing = owlshift(config_dir.path(), config_dir.path(), &["uninstall"]);
    assert!(nothing.status.success(), "{}", stderr(&nothing));
    assert!(
        stdout(&nothing).contains("lists nothing to remove"),
        "{}",
        stdout(&nothing)
    );

    for file in [
        "projects/github.com/acme/web/checkout/HEAD",
        "projects/github.com/acme/web/worktrees/OWL-1/file",
        "projects/github.com/acme/web/runs/OWL-1/run.log",
        "events.jsonl",
        "events.lock",
        "notes.txt",
    ] {
        let path = data.join(file);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "x").unwrap();
    }
    let purged = owlshift(
        config_dir.path(),
        config_dir.path(),
        &["uninstall", "--purge"],
    );
    assert!(purged.status.success(), "{}", stderr(&purged));
    let said = stdout(&purged);
    assert!(
        said.contains(&format!("Removed the personal file {}", personal.display()))
            && said.contains(&format!(
                "Left in place, not Owlshift's or still listing what was not removed: {}",
                data.join("notes.txt").display()
            )),
        "{said}"
    );
    let left: Vec<String> = fs::read_dir(&data)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(left, ["notes.txt"]);
    assert!(!personal.exists());
}

/// OWL-435: `start` with no daemon running registers the project and starts
/// the daemon for the session, which outlives `start`, its output in the
/// data directory; `stop` ends it. Its watch's first clone is refused at
/// once (`GIT_ALLOW_PROTOCOL`), never reaching the network.
#[cfg(unix)]
#[test]
fn start_runs_the_daemon_for_the_session_and_stop_ends_it() {
    let config_dir = tempfile::tempdir().unwrap();
    personal_file(config_dir.path(), free_port());
    let checkout = web_checkout();
    let started = command(checkout.path(), config_dir.path(), &["start"])
        .env("GIT_ALLOW_PROTOCOL", "file")
        .output()
        .unwrap();
    let said = stdout(&started);
    assert!(started.status.success(), "{said}{}", stderr(&started));
    let pid: u32 = said
        .split("Started the daemon for this session (pid ")
        .nth(1)
        .and_then(|rest| rest.split(')').next())
        .and_then(|pid| pid.parse().ok())
        .unwrap_or_else(|| panic!("{said}"));
    let service = config_dir.path().join("data/service");
    let log = service.join("daemon.log");
    let read_log = || fs::read_to_string(&log).unwrap_or_default();
    assert!(is_alive(pid), "{}", read_log());
    assert!(service.join("daemon.json").exists());

    let stopped = owlshift(config_dir.path(), config_dir.path(), &["stop"]);
    assert!(stopped.status.success(), "{}", stderr(&stopped));
    if !wait::reached(wait::REACHED, || !is_alive(pid)) {
        let _ = Command::new("kill")
            .args(["-s", "KILL", &pid.to_string()])
            .status();
        panic!(
            "the daemon still runs after `owlshift stop`\n{}",
            read_log()
        );
    }
    assert!(!service.join("daemon.json").exists(), "{}", read_log());
    assert!(read_log().contains("The daemon stopped"), "{}", read_log());
}

/// OWL-435: a watch typed in a terminal on a project the background service
/// serves refuses at once, before any credential, naming `owlshift stop`;
/// `start` and `stop --forget` need a checkout.
#[cfg(unix)]
#[test]
fn a_terminal_watch_on_a_served_project_refuses() {
    let config_dir = tempfile::tempdir().unwrap();
    let checkout = web_checkout();
    let project = config_dir.path().join("data/projects/github.com/acme/web");
    fs::create_dir_all(&project).unwrap();
    // As the daemon holds it.
    let served = fs::File::create(project.join("served.lock")).unwrap();
    served.try_lock().unwrap();
    for args in [&["watch"][..], &["watch", "--drain"]] {
        let output = owlshift(checkout.path(), config_dir.path(), args);
        assert_eq!(
            output.status.code(),
            Some(1),
            "{args:?}: {}",
            stderr(&output)
        );
        let said = stderr(&output);
        assert!(
            said.contains("acme/web is served by the background service on this machine"),
            "{args:?}: {said}"
        );
        assert!(said.contains("`owlshift stop`"), "{said}");
    }
    drop(served);

    let elsewhere = tempfile::tempdir().unwrap();
    for args in [&["start"][..], &["stop", "--forget"]] {
        let output = owlshift(elsewhere.path(), config_dir.path(), args);
        assert_eq!(output.status.code(), Some(1), "{args:?}");
        assert!(
            stderr(&output).contains("runs in a git repository"),
            "{}",
            stderr(&output)
        );
    }
}

/// OWL-368: `owlshift unquarantine` lifts a security marker only for a
/// person at a terminal. Run from a script, its standard input no terminal,
/// it is refused and the marker stays, whatever the project file says: here
/// there is none.
#[test]
fn unquarantine_is_refused_without_a_terminal() {
    let config_dir = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    git_init(repo.path());
    git(
        repo.path(),
        &["remote", "add", "origin", "https://github.com/acme/app.git"],
    );
    let project = config_dir.path().join("data/projects/github.com/acme/app");
    fs::create_dir_all(&project).unwrap();
    let marker = project.join("unverified");
    fs::write(
        &marker,
        "Run r1 of OWL-7 broke isolation:\n- the main checkout's HEAD moved\n",
    )
    .unwrap();

    let output = owlshift(repo.path(), config_dir.path(), &["unquarantine"]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("confirms it at a terminal"),
        "{}",
        stderr(&output)
    );
    assert!(marker.exists());
}

#[test]
fn config_show_lists_the_always_human_categories_with_their_origin() {
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
always_human = ["Billing!", "Data Loss", "", "billing"]
"#,
    )
    .unwrap();

    let output = owlshift(repo.path(), config_dir.path(), &["config", "show"]);
    let shown = stdout(&output);
    assert!(output.status.success(), "{shown}");
    // The floor, then what the default delegation level keeps (OWL-328).
    for token in ["security", "irreversible"] {
        assert!(shown.contains(&format!("  {token}  (floor)\n")), "{shown}");
    }
    for token in ["data_loss", "money", "legal", "scope"] {
        assert!(
            shown.contains(&format!("  {token}  (delegation level discoverable)\n")),
            "{shown}"
        );
    }
    // The tempdir may be reached through a symlink (`/private` on macOS), so
    // the origin is checked by its file name, not its full path.
    let billing = shown
        .lines()
        .find(|line| line.starts_with("  billing  ("))
        .unwrap_or_else(|| panic!("{shown}"));
    assert!(billing.ends_with("owlshift.toml)"), "{shown}");
    // Normalized as the gate reads it, listed once, blank dropped; an
    // addition the delegation level already covers says so.
    assert_eq!(shown.matches("\n  billing  (").count(), 1, "{shown}");
    assert!(
        shown.contains("  data_loss  (")
            && shown.contains("owlshift.toml)  already covered by the delegation level\n"),
        "{shown}"
    );
    assert!(!shown.contains("\n    ("), "{shown}");
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
    let config_dir = tempfile::tempdir().unwrap();
    doctor_in(config_dir.path(), args)
}

/// [`doctor`] with the configuration directory `config_dir`.
#[cfg(unix)]
fn doctor_in(config_dir: &Path, args: &[&str]) -> Output {
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
    let mut all = vec!["doctor"];
    all.extend_from_slice(args);
    command(config_dir, config_dir, &all)
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
/// it started. The whole process group `owlshift` leads is killed, so what
/// stops the probe, the sentinel, cannot be in that group. The probe's child
/// would outlive the wait, so only a stop passes; the five seconds measured
/// when it was decided are no bound of the design's (OWL-493).
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

        // A `git` whose `--version` starts a child that outlives every wait,
        // so that only a stop ends it, records both pids, and waits on the
        // child; anything else answers as git does outside a repository, so
        // loading the configuration does not hang.
        let bin = tempfile::tempdir().unwrap();
        let pids = bin.path().join("pids");
        let git = bin.path().join("git");
        fs::write(
            &git,
            format!(
                "#!/bin/sh\n\
                 if [ \"$1\" = --version ]; then\n\
                 \x20 sleep {outlasts} &\n\
                 \x20 echo \"$$ $!\" > '{pids}.tmp'\n\
                 \x20 mv '{pids}.tmp' '{pids}'\n\
                 \x20 wait\n\
                 fi\n\
                 echo 'fatal: not a git repository' >&2\n\
                 exit 128\n",
                pids = pids.display(),
                outlasts = wait::OUTLASTS.as_secs()
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

        // Doctor tries its sentinel and loads the configuration first, which
        // a loaded machine slows (OWL-493).
        let mut ended = None;
        wait::reached(wait::REACHED, || {
            pids.exists() || {
                ended = owlshift.try_wait().unwrap();
                ended.is_some()
            }
        });
        if !pids.exists() {
            let _ = owlshift.kill();
            let _ = owlshift.wait();
            match ended {
                Some(status) => panic!("owlshift ended before its probe started: {status}"),
                None => panic!("the probe never started"),
            }
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

    /// Waits for `owlshift` to end after `signal`. What ended it, the signal
    /// or the probe's own ten-second deadline, the caller reads from the
    /// status; a failure leaves the kill to `Drop`.
    fn ended(&mut self, signal: &str) -> std::process::ExitStatus {
        let mut status = None;
        wait::until(&format!("owlshift to end after {signal}"), || {
            status = self.owlshift.try_wait().unwrap();
            status.is_some()
        });
        status.unwrap()
    }

    /// Checks the probe and its child are gone: killed processes take a
    /// moment to be gone, as an orphan is reaped by the system.
    fn assert_probe_gone(&self) {
        wait::until_with(
            "the probe and its child to be gone",
            || format!("still running among {:?}", self.pids),
            || !self.pids.iter().any(|&pid| is_alive(pid)),
        );
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
    // Gone before the probe ends and `doctor` reads its state. The bound is
    // the probe's own ten-second deadline (runtime-and-operations.md, "the
    // 10 s deadline of each doctor probe"), which the check that `doctor`
    // still runs enforces, not the wait.
    wait::until("the sentinel to be gone", || !is_alive(sentinel));
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
    wait::until("the sentinel to stop", || state(sentinel).starts_with('T'));
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
        let stopped = test_proc::try_state(self.0).starts_with('T');
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

// How a test waits for a state (OWL-493), shared with the test bench.
#[allow(dead_code)]
#[path = "../../owlshift-testkit/src/wait.rs"]
mod wait;

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
        // Longer than any wait, so that only a stop ends it (OWL-493).
        std::thread::sleep(wait::OUTLASTS);
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
    use std::time::Instant;

    use super::wait;
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
        // The driver's own waits, four, are each `REACHED` at most.
        let mut ended = None;
        let status = if wait::reached(wait::REACHED * 5, || {
            ended = driver.try_wait().unwrap();
            ended.is_some()
        }) {
            ended.unwrap()
        } else {
            // With `owlshift` and the probe, if still there.
            stop(&[driver.id()]);
            driver.wait().unwrap()
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
    /// that the probe and its child are gone, the child outliving every wait
    /// on its own.
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

        // Doctor loads the configuration first, which a loaded machine slows
        // (OWL-493).
        let mut ended = None;
        wait::reached(wait::REACHED, || {
            pids.exists() || {
                ended = owlshift.try_wait().unwrap();
                ended.is_some()
            }
        });
        if !pids.exists() {
            let _ = owlshift.kill();
            match ended {
                Some(status) => panic!("owlshift ended before its probe started: {status}"),
                None => panic!("the probe never started"),
            }
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

        // What ended it, the Ctrl-C or the probe's own ten-second deadline,
        // the status says (OWL-493).
        let mut status = None;
        if !wait::reached(wait::REACHED, || {
            status = owlshift.try_wait().unwrap();
            status.is_some()
        }) {
            let _ = owlshift.kill();
            stop(&started);
            panic!(
                "owlshift still running {:?} after Ctrl-C (seen by the driver: {})",
                wait::REACHED,
                CTRL_C_SEEN.load(Ordering::SeqCst)
            );
        }
        let status = status.unwrap();
        let ended = sent.elapsed();
        // The driver's own handler runs on a thread of its own, maybe later.
        let seen = wait::reached(wait::REACHED, || CTRL_C_SEEN.load(Ordering::SeqCst));
        println!(
            "owlshift ended {ended:?} after Ctrl-C: {status}; Ctrl-C seen by the driver: {seen}"
        );

        // A process ends asynchronously once stopped.
        let mut gone = [None, None];
        wait::reached(wait::REACHED, || {
            for (at, &pid) in gone.iter_mut().zip(&started) {
                if at.is_none() && !is_alive(pid) {
                    *at = Some(sent.elapsed());
                }
            }
            gone.iter().all(Option::is_some)
        });
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
            "still running {:?} after owlshift ended: {alive:?} of the probe and its child {started:?}",
            wait::REACHED
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

fn init_markdown(repo: &Path, config_dir: &Path, extra: &[&str]) -> String {
    let mut args = vec!["init", "--tracker", "markdown", "--skip-secrets"];
    args.extend_from_slice(extra);
    let output = owlshift(repo, config_dir, &args);
    assert!(output.status.success(), "{}", stderr(&output));
    stdout(&output)
}

fn project_config(repo: &Path) -> owlshift_contracts::config::ProjectConfig {
    let written = fs::read_to_string(repo.join("owlshift.toml")).unwrap();
    owlshift_contracts::config::ProjectConfig::parse(&written).unwrap()
}

#[test]
fn init_detects_the_stack_and_never_runs_it() {
    let config_dir = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    git_init(repo.path());
    fs::write(repo.path().join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
    fs::write(
        repo.path().join("package.json"),
        r#"{"scripts":{"test":"touch ran-by-init"}}"#,
    )
    .unwrap();
    fs::write(repo.path().join("pnpm-lock.yaml"), "").unwrap();
    let out = init_markdown(repo.path(), config_dir.path(), &[]);
    let config = project_config(repo.path());
    assert_eq!(config.stack.gate.len(), 4, "{:?}", config.stack.gate);
    assert_eq!(config.stack.gate[3], "pnpm run test");
    assert_eq!(config.stack.setup, ["pnpm install --frozen-lockfile"]);
    assert!(out.contains("Detected Cargo.toml"), "{out}");
    assert!(
        out.contains("Detected package.json (pnpm-lock.yaml)"),
        "{out}"
    );
    assert!(!out.contains("gate is empty"), "{out}");
    let last = out.lines().last().unwrap();
    assert!(last.contains("owlshift gate"), "{out}");
    assert!(!repo.path().join("ran-by-init").exists());
}

#[test]
fn init_says_what_a_python_root_needs() {
    let config_dir = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    git_init(repo.path());
    fs::write(repo.path().join("pyproject.toml"), "[tool.ruff]\n").unwrap();
    let out = init_markdown(repo.path(), config_dir.path(), &[]);
    assert!(
        out.contains("Note: pyproject.toml's tools are run bare"),
        "{out}"
    );
    assert_eq!(project_config(repo.path()).stack.gate, ["ruff check ."]);
}

#[test]
fn a_gate_given_with_init_wins_over_the_detected_one() {
    let config_dir = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    git_init(repo.path());
    fs::write(
        repo.path().join("package.json"),
        r#"{"scripts":{"test":"jest"}}"#,
    )
    .unwrap();
    fs::write(repo.path().join("package-lock.json"), "").unwrap();
    let out = init_markdown(repo.path(), config_dir.path(), &["--gate", "make ci"]);
    let config = project_config(repo.path());
    assert_eq!(config.stack.gate, ["make ci"]);
    assert_eq!(config.stack.setup, ["npm ci"]);
    assert!(out.contains("--gate wins"), "{out}");
}

#[test]
fn init_on_an_empty_root_leaves_the_gate_empty() {
    let config_dir = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    git_init(repo.path());
    let out = init_markdown(repo.path(), config_dir.path(), &[]);
    assert!(project_config(repo.path()).stack.gate.is_empty());
    assert!(out.contains("Its gate is empty"), "{out}");
    assert!(!out.contains("Detected"), "{out}");
}

#[test]
fn init_markdown_writes_an_example_ticket_and_says_the_limits() {
    let config_dir = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    git_init(repo.path());
    let out = init_markdown(repo.path(), config_dir.path(), &[]);
    let ticket = repo.path().join("tickets/DEMO-1/ticket.md");
    let text = fs::read_to_string(&ticket).unwrap();
    assert!(text.contains("assignee = \"maintainer\""), "{text}");
    assert!(out.contains("tickets"), "{out}");
    assert!(out.contains("replace its assignee `maintainer`"), "{out}");
    assert!(out.contains("serves one machine"), "{out}");
    assert!(out.contains("`owlshift start` refuses"), "{out}");
    let written = fs::read_to_string(repo.path().join("owlshift.toml")).unwrap();
    assert!(written.contains("serves one machine"), "{written}");
    assert!(written.contains("docs/markdown-tracker.md"), "{written}");

    // A rerun keeps the project file, so it writes no ticket: an edit stays.
    fs::write(&ticket, "edited").unwrap();
    init_markdown(repo.path(), config_dir.path(), &[]);
    assert_eq!(fs::read_to_string(&ticket).unwrap(), "edited");

    // Tickets already there are left alone, and Linear has no files to write.
    let other = tempfile::tempdir().unwrap();
    git_init(other.path());
    fs::create_dir_all(other.path().join("tickets/OWL-1")).unwrap();
    let out = init_markdown(other.path(), config_dir.path(), &[]);
    assert!(!other.path().join("tickets/DEMO-1").exists());
    assert!(out.contains("already holds tickets"), "{out}");
    let linear = tempfile::tempdir().unwrap();
    git_init(linear.path());
    let output = owlshift(
        linear.path(),
        config_dir.path(),
        &["init", "--team", "OWL", "--skip-secrets"],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(!linear.path().join("tickets").exists());
}

/// OWL-34: `start` refuses a Markdown project with the reason, before it
/// registers anything or starts a daemon. (On Windows `start` refuses every
/// project earlier, for another reason.)
#[cfg(unix)]
#[test]
fn start_refuses_a_markdown_project_and_registers_nothing() {
    let config_dir = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    git_init(repo.path());
    git(
        repo.path(),
        &["remote", "add", "origin", "https://github.com/acme/web.git"],
    );
    init_markdown(repo.path(), config_dir.path(), &[]);
    let output = owlshift(repo.path(), config_dir.path(), &["start"]);
    assert_eq!(output.status.code(), Some(1), "{}", stdout(&output));
    let said = stderr(&output);
    assert!(
        said.contains("the Markdown tracker serves one machine and is not served by the daemon"),
        "{said}"
    );
    assert!(said.contains("docs/markdown-tracker.md"), "{said}");
    let data = config_dir.path().join("data");
    assert!(!data.join("service").exists(), "a daemon was started");
    assert!(
        !data.join("service/projects.json").exists(),
        "a project was registered"
    );
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

/// OWL-383: `--github-token` asks for a personal token, so it goes with the
/// secrets and is refused with `--skip-secrets`, before any file is written.
#[test]
fn init_refuses_github_token_without_the_secrets() {
    let config_dir = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    git_init(repo.path());
    let output = owlshift(
        repo.path(),
        config_dir.path(),
        &[
            "init",
            "--tracker",
            "markdown",
            "--github-token",
            "--skip-secrets",
        ],
    );
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("--github-token"),
        "{}",
        stderr(&output)
    );
    assert!(!repo.path().join("owlshift.toml").exists());
}

/// OWL-250: `watch --drain` on a project whose file lists no admitters is
/// refused before any credential, `watch` alone is not refused for it.
#[test]
fn watch_drain_refuses_a_project_without_admitters_before_any_credential() {
    let config_dir = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    git_init(repo.path());
    let init = ["init", "--tracker", "markdown", "--skip-secrets"];
    assert!(
        owlshift(repo.path(), config_dir.path(), &init)
            .status
            .success()
    );
    let output = owlshift(repo.path(), config_dir.path(), &["watch", "--drain"]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("`owlshift watch --drain` needs `[tracker].admitters`"),
        "{}",
        stderr(&output)
    );
    let output = owlshift(repo.path(), config_dir.path(), &["watch"]);
    assert!(
        !stderr(&output).contains("admitters"),
        "{}",
        stderr(&output)
    );
}

/// OWL-399: `continue --step`, the child of a step of `watch --drain`, is
/// not shown in the help, and an envelope that does not read is refused
/// before any project, configuration or credential is read.
#[test]
fn continue_hides_its_step_and_refuses_an_envelope_that_does_not_read() {
    let config_dir = tempfile::tempdir().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let help = owlshift(dir.path(), config_dir.path(), &["continue", "--help"]);
    assert!(help.status.success());
    assert!(!stdout(&help).contains("--step"), "{}", stdout(&help));
    let output = owlshift(
        dir.path(),
        config_dir.path(),
        &["continue", "OWL-1", "--step", "{"],
    );
    assert!(!output.status.success());
    assert!(
        stderr(&output).starts_with("owlshift: the step's envelope: invalid step envelope"),
        "{}",
        stderr(&output)
    );
}

/// OWL-473: the web UI's Restart, `continue --to-watch --served --from-ui`,
/// runs on the served project, located from the list of served projects
/// and never from the working directory: one the list does not hold is
/// refused before any credential, and the hidden flags stay out of the help.
#[test]
fn the_uis_restart_runs_on_the_served_project() {
    let config_dir = tempfile::tempdir().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let help = owlshift(dir.path(), config_dir.path(), &["continue", "--help"]);
    assert!(!stdout(&help).contains("--from-ui"), "{}", stdout(&help));
    let output = owlshift(
        dir.path(),
        config_dir.path(),
        &[
            "continue",
            "OWL-1",
            "--to-watch",
            "--served",
            "acme/web",
            "--from-ui",
        ],
    );
    assert!(!output.status.success());
    assert_eq!(
        stderr(&output),
        "owlshift: acme/web is not served on this machine: `owlshift start` in its checkout \
         serves it\n"
    );
}

/// What `do` refuses before it opens the keychain.
#[test]
fn do_refuses_a_project_it_cannot_deliver_before_any_credential() {
    let config_dir = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    git_init(repo.path());

    for args in [&["do", "OWL-1"][..], &["continue", "OWL-1"], &["watch"]] {
        let output = owlshift(repo.path(), config_dir.path(), args);
        assert!(!output.status.success());
        assert!(
            stderr(&output).contains("run `owlshift init`"),
            "{args:?}: {}",
            stderr(&output)
        );
    }
    // OWL-127: `resume` is the background service's command (P7, OWL-438:
    // it lifts a pause), not a ticket's; the ticket command has no other
    // name, and a ticket given to it is refused.
    let output = owlshift(repo.path(), config_dir.path(), &["resume", "OWL-1"]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("\"OWL-1\" is not a repository of the form owner/name"),
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

/// OWL-329: `owlshift gate` is refused, before anything is cloned, without
/// a project file, without a GitHub `origin` and, on native Windows, where
/// a Build's gate cannot be confined (D9). What it runs is tested in the
/// runner (`gate_once`).
#[test]
fn gate_refuses_what_a_build_would_before_any_clone() {
    let config_dir = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    git_init(repo.path());
    let output = owlshift(repo.path(), config_dir.path(), &["gate"]);
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
    let output = owlshift(repo.path(), config_dir.path(), &["gate"]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("not a github.com repository"),
        "{}",
        stderr(&output)
    );

    if cfg!(windows) {
        let github = "https://github.com/owlshift/demo.git";
        git(repo.path(), &["remote", "set-url", "origin", github]);
        let output = owlshift(
            repo.path(),
            config_dir.path(),
            &["gate", "--at", "origin/x"],
        );
        assert!(!output.status.success());
        assert!(stderr(&output).contains("WSL2"), "{}", stderr(&output));
    }
    assert!(!config_dir.path().join("data").exists());
}

/// OWL-202: `owlshift forget` with no record kept on the ticket is refused,
/// and makes no data directory. What it forgets is tested in the testkit
/// (`tests/ticket_ref.rs`, `tests/on_demand.rs`).
#[test]
fn forget_refuses_a_ticket_owlshift_keeps_nothing_on() {
    let config_dir = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    git_init(repo.path());
    let init = ["init", "--tracker", "markdown", "--skip-secrets"];
    assert!(
        owlshift(repo.path(), config_dir.path(), &init)
            .status
            .success()
    );
    let origin = "https://github.com/owlshift/demo.git";
    git(repo.path(), &["remote", "add", "origin", origin]);

    let output = owlshift(repo.path(), config_dir.path(), &["forget", "DEMO-1"]);
    assert!(!output.status.success());
    assert_eq!(
        stderr(&output),
        "owlshift: Not run: nothing to forget: Owlshift keeps nothing on DEMO-1\n"
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
fn logs_last_prints_only_the_final_events() {
    let config_dir = tempfile::tempdir().unwrap();
    let data = config_dir.path().join("data");
    fs::create_dir_all(&data).unwrap();
    let event = |minute: u32, ticket: &str| {
        format!(
            "{{\"format\":1,\"at\":\"2026-09-29T10:0{minute}:00Z\",\"project\":\"demo/project\",\"ticket\":\"{ticket}\",\"kind\":\"dispatch\",\"data\":{{}}}}\n"
        )
    };
    let log: String = [
        (0, "OWL-1"),
        (1, "OWL-2"),
        (2, "OWL-1"),
        (3, "OWL-2"),
        (4, "OWL-1"),
    ]
    .iter()
    .map(|(m, t)| event(*m, t))
    .collect();
    fs::write(data.join("events.jsonl"), log).unwrap();
    let logs = |args: &[&str]| {
        let mut full = vec!["logs"];
        full.extend_from_slice(args);
        owlshift(config_dir.path(), config_dir.path(), &full)
    };
    let line =
        |minute: u32, ticket: &str| format!("2026-09-29T10:0{minute}:00Z {ticket} dispatch\n");

    assert_eq!(
        stdout(&logs(&["--last", "2"])),
        format!("{}{}", line(3, "OWL-2"), line(4, "OWL-1"))
    );
    assert_eq!(
        stdout(&logs(&["OWL-1", "--last", "2"])),
        format!("{}{}", line(2, "OWL-1"), line(4, "OWL-1"))
    );
    let everything = stdout(&logs(&[]));
    assert_eq!(stdout(&logs(&["--last", "99"])), everything);
    assert_eq!(everything.lines().count(), 5);

    let refused = logs(&["--last", "0"]);
    assert!(!refused.status.success());
    assert!(stdout(&refused).is_empty());
    assert!(stderr(&refused).contains("--last"), "{}", stderr(&refused));
    assert!(!logs(&["--last", "two"]).status.success());
}

#[test]
fn logs_follow_prints_events_as_they_are_recorded() {
    use std::io::{BufRead, BufReader, Write};
    use std::process::Stdio;
    use std::sync::mpsc;

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
    let next = || received.recv_timeout(wait::REACHED);
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

/// What CI appends to the test command so each `check` job runs its shard of
/// the tests (OWL-515).
const CI_PARTITION: &str = " --partition hash:${{ matrix.shard }}/${{ matrix.shards }}";

/// The repository's own project file, which `owlshift do` reads on
/// Owlshift's tickets, parses and gates on what CI runs.
#[test]
fn the_repositorys_own_project_file_gates_on_what_ci_runs() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let text = fs::read_to_string(root.join("owlshift.toml")).unwrap();
    let config = owlshift_contracts::config::ProjectConfig::parse(&text).unwrap();
    assert_eq!(config.tracker.team.as_deref(), Some("OWL"));
    let ci = fs::read_to_string(root.join(".github/workflows/ci.yml")).unwrap();
    let ci_lines: Vec<&str> = ci.lines().map(str::trim).collect();
    assert!(!config.stack.gate.is_empty());
    for command in &config.stack.gate {
        // The gate runs a line through `sh -c`, so a variable CI sets in a
        // step's `env:` is an inline `NAME='value' ` assignment there (OWL-260).
        // It must match that step's `env:` exactly.
        let (assignment, command) = match command.split_once("' ") {
            Some((head, rest))
                if head.split_once("='").is_some_and(|(name, _)| {
                    !name.is_empty() && name.chars().all(|c| c.is_ascii_uppercase() || c == '_')
                }) =>
            {
                (Some(head), rest)
            }
            _ => (None, command.as_str()),
        };
        if let Some(head) = assignment {
            let (name, value) = head.split_once("='").unwrap();
            let step = ci_lines
                .iter()
                .position(|line| line.strip_prefix("run: ") == Some(command))
                .unwrap_or_else(|| panic!("{command} is not a CI step"));
            assert_eq!(ci_lines[step + 1], "env:", "{command} has no env in CI");
            assert_eq!(
                ci_lines[step + 2],
                format!("{name}: {value}"),
                "{command}: the gate's variable differs from CI's"
            );
        }
        // Each gate command is a CI step, verbatim: since OWL-355 the gate
        // runs the tests under cargo-nextest and the doctests apart, as the
        // ubuntu and macOS legs do (OWL-241; the Windows leg filters
        // on_demand out, and no gate runs on Windows). Since OWL-515 CI
        // splits the tests across shards, so its test step is the gate's
        // command followed by the partition; the gate runs them all, and
        // every other gate command runs in shard 1 only.
        let step = ci_lines
            .iter()
            .position(|line| {
                line.strip_prefix("run: ")
                    .is_some_and(|run| run == command || run == format!("{command}{CI_PARTITION}"))
            })
            .unwrap_or_else(|| panic!("{command} is not a CI step"));
        let shard_one = ci_lines[step - 1] == "if: matrix.shard == 1";
        assert_eq!(
            !command.starts_with("cargo nextest run "),
            shard_one,
            "{command}: CI runs it in every shard, or a test step in shard 1 only"
        );
    }
    for step in [
        "cargo fmt",
        "cargo clippy",
        "cargo doc",
        "cargo nextest run --workspace --locked --no-fail-fast",
        "cargo test --workspace --doc --locked",
    ] {
        assert!(
            config.stack.gate.iter().any(|c| c.contains(step)),
            "the gate misses CI's {step}"
        );
    }
}

/// OWL-339: CI shows what the confinement tests printed when they pass, so
/// the dated live check of the sandbox can be read from the log: nextest's
/// config shows that binary's successful output, and CI still runs the tests
/// through `cargo nextest run`.
#[test]
fn ci_shows_the_confinement_tests_output() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let text = fs::read_to_string(root.join(".config/nextest.toml")).unwrap();
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.starts_with('#'))
        .collect();
    assert!(lines.contains(&"[[profile.default.overrides]]"), "{text}");
    assert!(
        lines.iter().any(|line| line.starts_with("filter = ")
            && line.contains("binary_id(owlshift-testkit::confinement)")),
        "{text}"
    );
    assert!(
        lines
            .iter()
            .any(|line| line.starts_with("success-output = ") && !line.contains("never")),
        "{text}"
    );
    let ci = fs::read_to_string(root.join(".github/workflows/ci.yml")).unwrap();
    let test_step =
        format!("run: cargo nextest run --workspace --locked --no-fail-fast{CI_PARTITION}");
    assert!(
        ci.lines().any(|line| line.trim() == test_step),
        "CI no longer runs the tests through cargo nextest run"
    );
}

/// OWL-339: the confinement tests must run, not skip, on both unix legs, so
/// the variable that makes them fail instead of skipping is set by a step of
/// its own for every OS but Windows, not by the Linux-only bwrap step.
#[test]
fn ci_requires_confinement_on_every_unix_leg() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let ci = fs::read_to_string(root.join(".github/workflows/ci.yml")).unwrap();
    let lines: Vec<&str> = ci.lines().map(str::trim).collect();
    let sets: Vec<usize> = (0..lines.len())
        .filter(|i| lines[*i].contains("OWLSHIFT_REQUIRE_CONFINEMENT=1"))
        .collect();
    assert_eq!(sets.len(), 1, "the variable is set once: {sets:?}");
    // The step the line belongs to: from its `- name:` to the next step's.
    let start = (0..=sets[0])
        .rev()
        .find(|i| lines[*i].starts_with("- name:"))
        .expect("the line sits in a step");
    let end = (sets[0]..lines.len())
        .find(|i| lines[*i].starts_with("- name:") && *i > start)
        .unwrap_or(lines.len());
    let step = &lines[start..end];
    assert!(
        step.contains(&"if: runner.os != 'Windows'"),
        "the step is not for every unix leg: {step:?}"
    );
    assert!(
        !step
            .iter()
            .any(|line| line.contains("runner.os == 'Linux'")),
        "{step:?}"
    );
}

/// OWL-407: `owlshift why` on the bench's events, while another process
/// holds the project's lock, as `owlshift watch` does.
#[test]
fn why_tells_each_waiting_ticket_its_reason_while_the_project_is_locked() {
    let config_dir = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    git_init(repo.path());
    let init = ["init", "--tracker", "markdown", "--skip-secrets"];
    assert!(
        owlshift(repo.path(), config_dir.path(), &init)
            .status
            .success()
    );
    git(
        repo.path(),
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/owlshift/demo.git",
        ],
    );
    let data = config_dir.path().join("data");
    let project = data.join("projects/github.com/owlshift/demo");
    fs::create_dir_all(&project).unwrap();
    let event = |at: &str, ticket: &str, kind: &str, details: &str| {
        format!(
            r#"{{"format":2,"at":"{at}","project":"owlshift/demo","ticket":"{ticket}","kind":"{kind}","data":{details}}}"#
        )
    };
    let other = r#"{"format":2,"at":"2026-10-20T10:03:00Z","project":"other/p","ticket":"ZED-1","run":"r9","kind":"run_started","data":{}}"#;
    let lines = [
        event(
            "2026-10-20T10:00:00Z",
            "OWL-1",
            "decision",
            r#"{"drain":"waits","reason":"blocked_by","tickets":["OWL-9"]}"#,
        ),
        event(
            "2026-10-20T10:01:00Z",
            "OWL-1",
            "decision",
            r#"{"drain":"waits","reason":"collision","tickets":["OWL-2"],"resource":{"zone":"leases/"}}"#,
        ),
        event(
            "2026-10-20T10:02:00Z",
            "OWL-3",
            "decision",
            r#"{"drain":"waits","reason":"pr_cap"}"#,
        ),
        event("2026-10-20T10:02:30Z", "OWL-3", "dispatch", "{}"),
        event(
            "2026-10-20T10:04:00Z",
            "OWL-5",
            "warning",
            r#"{"what":"zones_unknown"}"#,
        ),
        other.to_owned(),
    ];
    fs::write(data.join("events.jsonl"), lines.join("\n") + "\n").unwrap();
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(project.join("lock"))
        .unwrap();
    lock.lock().unwrap();

    // No snapshot: OWL-1 is not in flight, so its wait is left out (OWL-484).
    let unread = stdout(&owlshift(repo.path(), config_dir.path(), &["why"]));
    assert!(!unread.contains("OWL-1 last told"), "{unread}");
    assert!(
        unread.contains("1 ticket(s) with a told wait are left out: it is not in flight and no ready snapshot reads"),
        "{unread}"
    );
    // The drain's snapshot lists OWL-1 (its interval is long, so it is not stale).
    fs::write(
        project.join("ready.json"),
        format!(
            r#"{{"format":1,"at":"{}","interval_ms":4000000000000,"tickets":[{{"ticket":"OWL-1","title":"One","priority":"high","blockers":[],"drain":{{"kind":"taken_in"}}}}]}}"#,
            jiff::Timestamp::now()
        ),
    )
    .unwrap();

    let all = owlshift(repo.path(), config_dir.path(), &["why"]);
    assert!(all.status.success(), "{}", stderr(&all));
    let text = stdout(&all);
    assert!(
        text.contains("OWL-1 last told 2026-10-20T10:01:00Z: OWL-2 holds zone `leases/`"),
        "{text}"
    );
    assert!(!text.contains("OWL-3 waits"), "{text}");
    assert!(text.contains("people's tickets left out"), "{text}");
    assert!(text.contains("OWL-5: in the working state"), "{text}");
    assert!(text.contains("machine slots: 0 of 2 in use"), "{text}");
    assert!(text.contains("no harness is limited"), "{text}");

    let one = owlshift(repo.path(), config_dir.path(), &["why", "OWL-1"]);
    assert!(one.status.success(), "{}", stderr(&one));
    let text = stdout(&one);
    let blocked = text.find("blocked by OWL-9").expect(&text);
    let held = text.find("OWL-2 holds zone").expect(&text);
    assert!(blocked < held, "{text}");
    assert!(text.contains("now: OWL-2 holds zone `leases/`"), "{text}");

    let moved = stdout(&owlshift(repo.path(), config_dir.path(), &["why", "OWL-3"]));
    assert!(moved.contains("now: not waiting"), "{moved}");
    let unknown = stdout(&owlshift(
        repo.path(),
        config_dir.path(),
        &["why", "OWL-77"],
    ));
    assert!(unknown.contains("no wait recorded"), "{unknown}");
    assert!(
        !owlshift(repo.path(), config_dir.path(), &["why", "a b"])
            .status
            .success()
    );
}

/// A daemon a test started, killed if the test fails: its sentinel then
/// stops the web UI it ran.
#[cfg(unix)]
struct Daemon {
    child: std::process::Child,
    out: std::path::PathBuf,
    err: std::path::PathBuf,
}

#[cfg(unix)]
impl Daemon {
    /// `owlshift daemon` on `config_dir`, its output appended to the files
    /// the daemons before it wrote.
    fn start(config_dir: &Path) -> Self {
        use std::os::unix::process::CommandExt;
        use std::process::Stdio;

        let out = config_dir.join("daemon.out");
        let err = config_dir.join("daemon.err");
        let append = |path: &Path| {
            fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .unwrap()
        };
        let child = command(config_dir, config_dir, &["daemon"])
            .stdin(Stdio::null())
            .stdout(append(&out))
            .stderr(append(&err))
            .process_group(0)
            .spawn()
            .unwrap();
        Self { child, out, err }
    }

    fn said(&self) -> String {
        format!(
            "--- out\n{}--- err\n{}",
            fs::read_to_string(&self.out).unwrap_or_default(),
            fs::read_to_string(&self.err).unwrap_or_default()
        )
    }

    /// Sends SIGTERM and waits for the daemon's end: a success.
    fn stop(&mut self) {
        let sent = Command::new("kill")
            .args(["-s", "TERM", &self.child.id().to_string()])
            .status()
            .unwrap();
        assert!(sent.success());
        let mut status = None;
        let ended = wait::reached(wait::REACHED, || {
            status = self.child.try_wait().unwrap();
            status.is_some()
        });
        assert!(ended, "the daemon still runs\n{}", self.said());
        assert!(status.unwrap().success(), "{}", self.said());
    }
}

#[cfg(unix)]
impl Drop for Daemon {
    fn drop(&mut self) {
        if std::thread::panicking() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// The pid of the web UI `ui.json` names on `port`, once it names one that
/// runs other than `not`.
#[cfg(unix)]
fn ui_listening(daemon: &Daemon, data: &Path, port: u16, not: Option<u32>) -> u32 {
    let file = data.join("service/ui.json");
    let mut pid = None;
    wait::until_with(
        "the web UI's record",
        || daemon.said(),
        || {
            pid = fs::read_to_string(&file)
                .ok()
                .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
                .filter(|record| record["port"] == port)
                .and_then(|record| record["pid"].as_u64())
                .and_then(|pid| u32::try_from(pid).ok())
                .filter(|pid| Some(*pid) != not && is_alive(*pid));
            pid.is_some()
        },
    );
    pid.unwrap()
}

/// An HTTP/1.1 exchange with 127.0.0.1:`port`: the status, the head and the
/// body.
#[cfg(unix)]
fn http(
    port: u16,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> (u16, String, String) {
    use std::io::{Read, Write};

    let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(30)))
        .unwrap();
    let mut request = format!("{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n");
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    if method == "POST" {
        request.push_str(&format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            body.len()
        ));
    }
    request.push_str("Connection: close\r\n\r\n");
    request.push_str(body);
    stream.write_all(request.as_bytes()).unwrap();
    let mut answer = String::new();
    stream.read_to_string(&mut answer).unwrap();
    let (head, body) = answer.split_once("\r\n\r\n").unwrap();
    let status = head.split(' ').nth(1).unwrap().parse().unwrap();
    (status, head.to_owned(), body.to_owned())
}

/// The code of the link `owlshift ui --print` (with `extra`) prints for
/// `port`.
#[cfg(unix)]
fn login_code(config_dir: &Path, port: u16, extra: &[&str]) -> String {
    let mut args = vec!["ui", "--print"];
    args.extend_from_slice(extra);
    let printed = owlshift(config_dir, config_dir, &args);
    assert!(printed.status.success(), "{}", stderr(&printed));
    let link = stdout(&printed);
    let prefix = format!("http://127.0.0.1:{port}/#login=");
    link.trim()
        .strip_prefix(&prefix)
        .unwrap_or_else(|| panic!("{link}"))
        .to_owned()
}

/// Trades `code` for a session on `port`, from the UI's own origin: the
/// header value, `None` when the code is refused.
#[cfg(unix)]
fn log_in(port: u16, code: &str) -> Option<String> {
    let origin = format!("http://127.0.0.1:{port}");
    let (status, _, body) = http(
        port,
        "POST",
        "/api/login",
        &[("Origin", &origin)],
        &serde_json::json!({ "code": code }).to_string(),
    );
    if status != 200 {
        assert_eq!(status, 401, "{body}");
        return None;
    }
    let session: serde_json::Value = serde_json::from_str(&body).unwrap();
    Some(format!(
        "{}.{}",
        session["id"].as_str().unwrap(),
        session["secret"].as_str().unwrap()
    ))
}

/// `/api/header` on `port` with `session`: its status and body.
#[cfg(unix)]
fn header_with(port: u16, session: &str) -> (u16, String) {
    let (status, _, body) = http(
        port,
        "GET",
        "/api/header",
        &[("X-Owlshift-Session", session)],
        "",
    );
    (status, body)
}

/// OWL-467: the frame loads from `owlshift ui --print`'s link, served by the
/// daemon's child on `[ui] port`, and the code logs in once. A UI killed
/// keeps the port held by the daemon and its sessions once started again;
/// `--new-token` ends them, and so does a new daemon run.
#[cfg(unix)]
#[test]
fn the_frame_loads_from_owlshift_ui_print() {
    let config_dir = tempfile::tempdir().unwrap();
    let port = free_port();
    personal_file(config_dir.path(), port);
    let data = config_dir.path().join("data");
    let mut daemon = Daemon::start(config_dir.path());
    let first = ui_listening(&daemon, &data, port, None);

    let code = login_code(config_dir.path(), port, &[]);
    let (status, head, body) = http(port, "GET", "/", &[], "");
    assert_eq!(status, 200, "{head}");
    assert!(
        head.contains("Content-Security-Policy: default-src 'self'; frame-ancestors 'none'"),
        "{head}"
    );
    assert!(body.contains("/app.js"), "{body}");
    for asset in ["/app.js", "/app.css"] {
        assert_eq!(http(port, "GET", asset, &[], "").0, 200, "{asset}");
    }
    let session = log_in(port, &code).expect("the printed code logs in");
    let (status, body) = header_with(port, &session);
    assert_eq!(status, 200, "{body}");
    let header: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(header["daemon"]["state"], "running", "{header}");
    assert_eq!(header["daemon"]["pid"], daemon.child.id(), "{header}");
    for key in ["usage", "slots", "waiting", "projects"] {
        assert!(header.get(key).is_some(), "{key}: {header}");
    }
    assert_eq!(log_in(port, &code), None, "a code logs in once");

    // Killed, the UI starts again on the socket the daemon kept.
    let killed = Command::new("kill")
        .args(["-s", "KILL", &first.to_string()])
        .status()
        .unwrap();
    assert!(killed.success());
    assert!(
        std::net::TcpListener::bind(("127.0.0.1", port)).is_err(),
        "the daemon holds the port while the UI is down"
    );
    ui_listening(&daemon, &data, port, Some(first));
    assert_eq!(header_with(port, &session).0, 200, "{}", daemon.said());

    // A new token ends every session; its link logs in.
    let code = login_code(config_dir.path(), port, &["--new-token"]);
    assert_eq!(header_with(port, &session).0, 401);
    let session = log_in(port, &code).expect("the new link logs in");
    assert_eq!(header_with(port, &session).0, 200);

    // A new daemon run ends every session of the one before.
    let second = ui_listening(&daemon, &data, port, None);
    daemon.stop();
    let mut daemon = Daemon::start(config_dir.path());
    ui_listening(&daemon, &data, port, Some(second));
    assert_eq!(header_with(port, &session).0, 401);
    let code = login_code(config_dir.path(), port, &[]);
    let session = log_in(port, &code).expect("a new link logs in");
    assert_eq!(header_with(port, &session).0, 200);
    daemon.stop();
}

/// OWL-467: a port another program holds is named, with `[ui] port`, by the
/// daemon, by `owlshift ui`, which mints no code, and by `owlshift doctor`:
/// whether the program answers or stays silent. Without a daemon, `owlshift
/// ui` names `owlshift start`; the UI's own form needs its descriptors.
#[cfg(unix)]
#[test]
fn a_taken_ui_port_is_named() {
    use std::io::{Read, Write};
    use std::sync::{Arc, Mutex};

    let config_dir = tempfile::tempdir().unwrap();
    let no_daemon = owlshift(config_dir.path(), config_dir.path(), &["ui", "--print"]);
    assert_eq!(no_daemon.status.code(), Some(1));
    assert!(
        stderr(&no_daemon).contains("no daemon runs, so no web UI: `owlshift start` starts both"),
        "{}",
        stderr(&no_daemon)
    );
    let bare = owlshift(config_dir.path(), config_dir.path(), &["ui", "--daemon"]);
    assert_eq!(bare.status.code(), Some(2), "{}", stderr(&bare));

    // A program that answers, without the proof.
    let answering = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = answering.local_addr().unwrap().port();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let recorded = requests.clone();
    std::thread::spawn(move || {
        for mut stream in answering.incoming().flatten() {
            let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(500)));
            let mut buffer = [0u8; 4096];
            let read = stream.read(&mut buffer).unwrap_or(0);
            let text = String::from_utf8_lossy(&buffer[..read]).into_owned();
            recorded
                .lock()
                .unwrap()
                .push(text.lines().next().unwrap_or("").to_owned());
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok");
        }
    });
    // And one that takes connections and says nothing.
    let silent = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let quiet = silent.local_addr().unwrap().port();

    for (port, answers) in [(port, true), (quiet, false)] {
        personal_file(config_dir.path(), port);
        let mut daemon = Daemon::start(config_dir.path());
        let said = format!(
            "Port {port} of 127.0.0.1, `[ui] port` of the personal file, is taken by another \
             program"
        );
        wait::until_with(
            "the daemon to name the taken port",
            || daemon.said(),
            || daemon.said().contains(&said),
        );
        let refused = owlshift(config_dir.path(), config_dir.path(), &["ui", "--print"]);
        assert_eq!(refused.status.code(), Some(1), "{}", stderr(&refused));
        let why = stderr(&refused);
        assert!(
            why.contains(&format!("port {port} of 127.0.0.1")) && why.contains("`[ui] port`"),
            "{why}"
        );
        if answers {
            assert!(why.contains("did not prove itself the web UI"), "{why}");
        }
        assert!(!stdout(&refused).contains("login="), "{}", stdout(&refused));

        let report = doctor_in(config_dir.path(), &["--json"]);
        let report: serde_json::Value = serde_json::from_slice(&report.stdout).unwrap();
        let check = report["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|check| check["subject"] == "web UI")
            .unwrap_or_else(|| panic!("{report}"))
            .clone();
        assert_eq!(check["status"], "warn", "{check}");
        let detail = check["detail"].as_str().unwrap();
        assert!(
            detail.contains(&port.to_string()) && detail.contains("`[ui] port`"),
            "{detail}"
        );
        daemon.stop();
    }
    let requests = requests.lock().unwrap().clone();
    assert!(!requests.is_empty());
    assert!(
        requests
            .iter()
            .all(|line| line == "POST /api/proof HTTP/1.1"),
        "only the proof was asked: {requests:?}"
    );
    drop(silent);
}

/// OWL-528: `owlshift pause` and `owlshift resume` each leave one `decision`
/// event in the data directory's log, naming the project, and none carries
/// a `by`: the terminal is the default asker.
#[test]
fn pause_and_resume_leave_an_event_without_by() {
    let config_dir = tempfile::tempdir().unwrap();
    let paused = owlshift(
        config_dir.path(),
        config_dir.path(),
        &["pause", "acme/web", "--reason", "lunch"],
    );
    assert!(paused.status.success(), "{}", stderr(&paused));
    let resumed = owlshift(
        config_dir.path(),
        config_dir.path(),
        &["resume", "acme/web"],
    );
    assert!(resumed.status.success(), "{}", stderr(&resumed));
    let log = fs::read_to_string(config_dir.path().join("data/events.jsonl")).unwrap();
    let events: Vec<serde_json::Value> = log
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(events.len(), 2, "{log}");
    for (event, pause) in events.iter().zip(["asked", "lifted"]) {
        assert_eq!(event["kind"], "decision", "{log}");
        assert_eq!(event["project"], "acme/web", "{log}");
        assert_eq!(event["data"]["pause"], pause, "{log}");
        assert!(event["data"].get("by").is_none(), "{log}");
    }
    assert_eq!(events[0]["data"]["why"], "lunch", "{log}");
    assert_eq!(events[1]["data"]["was"]["why"], "lunch", "{log}");
}
