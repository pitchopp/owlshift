//! OWL-650: a GitHub App entry that no longer reads, met at a watch's start,
//! end to end through the command line's own code. `owlshift-cli-bench` is
//! `owlshift_cli::main_with`, the whole of the shipped `owlshift` binary,
//! with a keychain held in memory that holds the entry this file gives it:
//! the refused sign-in of `github_token`, met with no network. The served
//! watch's last line is the pause reason the daemon keeps; a watch typed in
//! a terminal names its own folder and says nothing of the daemon.

use super::*;
use owlshift_runner::daemon::UNSERVABLE_EXIT;
use owlshift_runner::forge::sign_in_notice;
use owlshift_runner::served::Registry;
use owlshift_testkit::step_child::{ChildProject, REPO, served_parked_at_build};

fn cli_bench() -> &'static Path {
    owlshift_testkit::binary!("owlshift-cli-bench")
}
fn fake_harness() -> &'static Path {
    owlshift_testkit::binary!("owlshift-fake-harness")
}

/// The bench's GitHub App entry: plainly not one.
const NOT_AN_ENTRY: &str = "not an app token entry";

/// Whether agent runs can be confined here: `do`, `continue` and `watch`
/// refuse before their keychain otherwise, as inside another sandbox. A
/// host that sets `OWLSHIFT_REQUIRE_CONFINEMENT` fails instead of skipping.
fn confined_or_skip() -> bool {
    match owlshift_platform::sandbox::available() {
        Ok(()) => true,
        Err(error) => {
            assert!(
                std::env::var_os("OWLSHIFT_REQUIRE_CONFINEMENT").is_none(),
                "confinement is required here, but: {error}"
            );
            eprintln!("skipped: agent runs cannot be confined here: {error}");
            false
        }
    }
}

/// What a command of the bench under `root` ended with.
struct Said {
    code: Option<i32>,
    stderr: String,
}

impl Said {
    /// Stderr's non-empty lines.
    fn lines(&self) -> Vec<&str> {
        self.stderr
            .lines()
            .filter(|line| !line.trim().is_empty())
            .collect()
    }
}

/// `args` run by the bench in `dir`, on `project` under `root`, with the
/// GitHub App entry [`NOT_AN_ENTRY`] and no route to the network: git
/// reaches the bare remote through the bench's configuration, and anything
/// else goes to a proxy that answers nothing.
fn run(root: &Path, project: &ChildProject, dir: &Path, args: &[&str]) -> Said {
    // `claude` only has to be found: the watch refuses before any run.
    let bin = root.join("bin");
    fs::create_dir_all(&bin).unwrap();
    owlshift_testkit::stand_in::write_executable(&bin.join("claude"), "#!/bin/sh\nexit 1\n");
    let config = root.join("config");
    fs::create_dir_all(&config).unwrap();

    let mut command = Command::new(cli_bench());
    command
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    project.env().apply(&mut command);
    let path = std::env::var_os("PATH").unwrap_or_default();
    let mut paths = vec![bin];
    paths.extend(std::env::split_paths(&path));
    let dead = "http://127.0.0.1:9";
    command
        .current_dir(dir)
        .env("PATH", std::env::join_paths(paths).unwrap())
        .env("OWLSHIFT_DATA_DIR", &project.data)
        .env("OWLSHIFT_CONFIG_DIR", &config)
        .env("OWLSHIFT_BENCH_GITHUB_APP_ENTRY", NOT_AN_ENTRY)
        .env_remove("NO_PROXY")
        .env_remove("no_proxy");
    for name in [
        "HTTPS_PROXY",
        "https_proxy",
        "HTTP_PROXY",
        "http_proxy",
        "ALL_PROXY",
        "all_proxy",
    ] {
        command.env(name, dead);
    }
    let output = command.output().unwrap();
    Said {
        code: output.status.code(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

/// The daemon's watch ends unservable, its last line the notice for the
/// checkout projects.json registers, with the line that the daemon keeps as
/// the pause reason once its `owlshift: ` is stripped; the technical words,
/// which name the keychain entry, are on the line before it, never in it.
#[test]
fn a_served_drain_whose_github_app_entry_does_not_read_ends_with_the_sign_in_notice() {
    if !confined_or_skip() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let project = served_parked_at_build(root.path(), fake_harness().to_path_buf());
    let repo = Repo::parse(REPO).unwrap();
    let entry = Registry::in_data_dir(&project.data)
        .find(&repo)
        .unwrap()
        .expect("the bench serves its project");

    let said = run(
        root.path(),
        &project,
        root.path(),
        &["watch", "--drain", "--served", REPO],
    );

    assert_eq!(
        said.code,
        Some(i32::from(UNSERVABLE_EXIT)),
        "{}",
        said.stderr
    );
    let lines = said.lines();
    let [.., technical, notice] = lines.as_slice() else {
        panic!("fewer than two lines: {}", said.stderr);
    };
    assert_eq!(
        notice.strip_prefix("owlshift: "),
        Some(sign_in_notice(&repo, Path::new(&entry.checkout), true).as_str()),
        "{}",
        said.stderr
    );
    assert!(!notice.contains("github-app"), "{notice}");
    assert!(technical.starts_with("owlshift: "), "{technical}");
    assert!(
        technical.contains("github.com/demo/project/github-app"),
        "{technical}"
    );
    assert!(technical.contains("does not read"), "{technical}");
    assert!(!said.stderr.contains(NOT_AN_ENTRY), "{}", said.stderr);
}

/// A watch typed in a terminal names the folder of its own project file and
/// says nothing of the daemon picking the project up.
#[test]
fn a_typed_watch_whose_github_app_entry_does_not_read_names_its_own_folder() {
    if !confined_or_skip() {
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let project = served_parked_at_build(root.path(), fake_harness().to_path_buf());
    let repo = Repo::parse(REPO).unwrap();
    // The typed watch of a served project is refused while the service
    // holds its served lock, which nothing holds here.
    let said = run(root.path(), &project, &project.checkout, &["watch"]);

    assert_ne!(said.code, Some(0), "{}", said.stderr);
    let lines = said.lines();
    let notice = lines.last().expect("a line");
    // The project file is found from the working directory, which the
    // system gives with its links resolved.
    let folder = project.checkout.canonicalize().unwrap();
    assert_eq!(
        notice.strip_prefix("owlshift: "),
        Some(sign_in_notice(&repo, &folder, false).as_str()),
        "{}",
        said.stderr
    );
    assert!(!notice.contains("within a minute"), "{notice}");
}
