//! The `owlshift` binary, end to end. Only `git` is needed on the host: no
//! test runs `doctor`, whose answer depends on the harness CLIs installed.

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
/// API and ignores environment variables. `HOME` is set too, but only to
/// isolate the real `git rev-parse` subprocess this binary shells out to
/// from the host's own `~/.gitconfig`; it plays no part in resolving the
/// personal configuration file anymore.
fn owlshift(dir: &Path, config_dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_owlshift"))
        .args(args)
        .current_dir(dir)
        .env("HOME", config_dir)
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
            "\nformats: brief 1, result 1, event 1, claim 1, ticket state 1, comment footer 1\n"
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
