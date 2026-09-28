//! The `owlshift` binary, end to end. Only `git` is needed on the host: the
//! one test that runs `doctor` gives it a fake `git` and interrupts it before
//! it looks at the harness CLIs installed.

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
/// configuration file anymore.
fn owlshift(dir: &Path, config_dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_owlshift"))
        .args(args)
        .current_dir(dir)
        .env("HOME", config_dir)
        .env("XDG_CONFIG_HOME", config_dir)
        .env("OWLSHIFT_CONFIG_DIR", config_dir)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_CEILING_DIRECTORIES")
        .output()
        .unwrap()
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

/// The acceptance criterion for OWL-43: Ctrl-C on `doctor` while a probe
/// hangs stops the probe and the process it started, although the probe runs
/// in a process group of its own, out of the terminal's reach; and the CLI
/// ends promptly, killed by the signal it received.
#[cfg(unix)]
#[test]
fn ctrl_c_on_doctor_stops_a_hung_probe_and_its_child() {
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::process::ExitStatusExt;
    use std::process::Stdio;
    use std::thread;
    use std::time::{Duration, Instant};

    const SIGINT: i32 = 2;

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
    let mut owlshift = Command::new(env!("CARGO_BIN_EXE_owlshift"))
        .arg("doctor")
        .current_dir(config_dir.path())
        .env("PATH", format!("{}:/usr/bin:/bin", bin.path().display()))
        .env("HOME", config_dir.path())
        .env("XDG_CONFIG_HOME", config_dir.path())
        .env("OWLSHIFT_CONFIG_DIR", config_dir.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
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
    let pids: Vec<u32> = fs::read_to_string(&pids)
        .unwrap()
        .split_whitespace()
        .map(|pid| pid.parse().unwrap())
        .collect();

    let interrupted = Command::new("kill")
        .args(["-INT", &owlshift.id().to_string()])
        .status()
        .unwrap();
    assert!(interrupted.success());

    // Promptly: well before the probe's own ten-second deadline.
    let ended = Instant::now() + Duration::from_secs(5);
    let status = loop {
        if let Some(status) = owlshift.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= ended {
            let _ = owlshift.kill();
            panic!("owlshift still running 5 s after SIGINT");
        }
        thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(status.signal(), Some(SIGINT), "{status}");

    // Killed processes take a moment to be gone: an orphan is reaped by the
    // system.
    let settled = Instant::now() + Duration::from_secs(5);
    while pids.iter().any(|&pid| is_alive(pid)) {
        assert!(Instant::now() < settled, "still running: {pids:?}");
        thread::sleep(Duration::from_millis(50));
    }
}

/// Whether a process is still running; a zombie is not.
#[cfg(unix)]
fn is_alive(pid: u32) -> bool {
    let ps = Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    let state = String::from_utf8_lossy(&ps.stdout);
    let state = state.trim();
    !state.is_empty() && !state.starts_with('Z')
}
