//! OWL-330's acceptance: a Node fixture (pnpm) and a Python fixture (uv,
//! `--group lint --group test`) pass a confined gate with their dependencies
//! installed by `stack.setup` from a seeded read-only cache, and no registry.
//!
//! Each fixture in `tests/fixtures/<name>/` holds the project (`repo/`),
//! seeded into a local bare remote, and its packages (`vendor/`), seeded by
//! the test, unconfined, into the cache folder of a home of the bench's own:
//! a pnpm store filled from a registry the test serves on the loopback, or a
//! folder of wheels. The cache is declared in the fixture's `stack.caches`
//! and allowed. The first pass is `Executor::run_gate` on a fresh worktree;
//! the second, a Build `Executor::run` with the fake harness over that kept
//! worktree, with a new run temporary folder: its setup finds the first
//! install, the harness sees it, and the gate after its `done` passes.
//!
//! The confined tests need pnpm or uv and the sandbox: without them they are
//! skipped, unless `OWLSHIFT_REQUIRE_FIXTURE_TOOLS` is set, as on CI's Linux
//! and macOS legs ([`fixture_gate`]). The same recipes also run unconfined,
//! needing the tools alone, with the cache made read-only by its modes: a
//! confined run of the Build cannot nest the sandbox, and still runs them.
#![cfg(unix)]

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use tempfile::TempDir;

use owlshift_contracts::Role;
use owlshift_contracts::brief::{
    Author, Brief, PermissionLevel, Permissions, Relation, TicketBrief,
};
use owlshift_contracts::config::ProjectConfig;
use owlshift_contracts::format::Format;
use owlshift_contracts::ids::{RelativePath, TicketId};
use owlshift_platform::process::find_executable_in;
use owlshift_runner::agent_env::AgentEnv;
use owlshift_runner::config::{Effective, FileState};
use owlshift_runner::executor::{Executor, GateRun, GateSpec, Git, Outcome, RunReport, RunSpec};
use owlshift_runner::system::HostSystem;
use owlshift_testkit::fixture_gate::{FixtureGate, REQUIRE_FIXTURE_TOOLS, fixture_gate};
use owlshift_testkit::git::{GitEnv, Remote, seed};
use owlshift_testkit::harness::FakeHarness;
use owlshift_testkit::reply::Reply;

/// One fixture project.
struct Fixture {
    /// Its folder in `tests/fixtures/`.
    name: &'static str,
    /// The folder its `stack.caches` declares, from the home.
    cache: &'static str,
    /// Where the test marks the first install.
    marker: &'static str,
    /// The programs its setup and gate run.
    tools: &'static [&'static str],
    /// What its gate prints when the setup did not install the dependency:
    /// the module a Node project cannot find, the venv a Python project's
    /// gate runs, which only the setup makes.
    missing: &'static str,
}

const NODE: Fixture = Fixture {
    name: "node-project",
    cache: "Library/pnpm",
    marker: "node_modules/.owlshift-marker",
    tools: &["pnpm", "node"],
    missing: "Cannot find module 'owlshift-fixture-dep'",
};

const PYTHON: Fixture = Fixture {
    name: "python-project",
    cache: ".cache/owlshift-wheels",
    marker: ".venv/.owlshift-marker",
    tools: &["uv", "python3"],
    missing: ".venv/bin/python",
};

const DONE: &str = r#"{"format":12,"status":"done","summary":"s","pr":{"branch":"owlshift/T-1","title":"t","body":"b"}}"#;

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// Whether every tool of `fixture` is on this process's `PATH`; the missing
/// ones named.
fn tools_present(fixture: &Fixture) -> Result<(), String> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let missing: Vec<&str> = fixture
        .tools
        .iter()
        .copied()
        .filter(|tool| find_executable_in(tool, &path).is_none())
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!("{} not on the PATH", missing.join(" and ")))
    }
}

/// Whether the test runs: see [`fixture_gate`]. `confined` asks for the
/// sandbox too.
fn runs(fixture: &Fixture, confined: bool) -> bool {
    let tools = tools_present(fixture);
    let sandbox = if confined {
        owlshift_platform::sandbox::available().map_err(|e| e.to_string())
    } else {
        Ok(())
    };
    let require = std::env::var_os(REQUIRE_FIXTURE_TOOLS).is_some();
    let gate = fixture_gate(tools.is_ok(), sandbox.is_ok(), require);
    let missing: Vec<String> = [tools.err(), sandbox.err()].into_iter().flatten().collect();
    match gate {
        FixtureGate::Run => true,
        FixtureGate::Skip => {
            eprintln!("skipped: {}", missing.join("; "));
            false
        }
        FixtureGate::Fail => panic!(
            "{REQUIRE_FIXTURE_TOOLS} is set, but: {}",
            missing.join("; ")
        ),
    }
}

/// A fixture seeded into a bare remote, with a home of the bench's own whose
/// cache holds the fixture's packages.
struct Bench {
    tmp: TempDir,
    env: GitEnv,
    remote: Remote,
    home: PathBuf,
    config: ProjectConfig,
    fixture: &'static Fixture,
    /// The cache made read-only by its modes, for an unconfined run: given
    /// back its write bits on drop, so the folder can be removed.
    locked: bool,
}

impl Drop for Bench {
    fn drop(&mut self) {
        if self.locked {
            set_writable(&self.home.join(self.fixture.cache), true);
        }
    }
}

impl Bench {
    fn new(fixture: &'static Fixture) -> Self {
        let tmp = tempfile::Builder::new()
            .prefix("owlshift-fixture-")
            .tempdir()
            .unwrap();
        let root = fs::canonicalize(tmp.path()).unwrap();
        let home = root.join("home");
        let env = GitEnv::create(home.clone()).unwrap();
        let folder = fixtures().join(fixture.name);
        let remote = seed(&env, &root, &folder.join("repo")).unwrap();
        let config =
            ProjectConfig::parse(&fs::read_to_string(folder.join("repo/owlshift.toml")).unwrap())
                .unwrap();
        assert_eq!(config.stack.caches, [fixture.cache]);
        let bench = Self {
            tmp,
            env,
            remote,
            home,
            config,
            fixture,
            locked: false,
        };
        bench.seed_cache(&folder.join("vendor"));
        bench
    }

    fn root(&self) -> PathBuf {
        fs::canonicalize(self.tmp.path()).unwrap()
    }

    /// Fills the cache from `vendor`, unconfined and offline.
    fn seed_cache(&self, vendor: &Path) {
        let cache = self.home.join(self.fixture.cache);
        if self.fixture.name == NODE.name {
            let lock = fs::read_to_string(self.remote.checkout.join("pnpm-lock.yaml")).unwrap();
            let tarball = fs::read(vendor.join("owlshift-fixture-dep-1.0.0.tgz")).unwrap();
            let registry = serve_registry(integrity(&lock), tarball);
            let out = Command::new("pnpm")
                .args(["store", "add", "owlshift-fixture-dep@1.0.0", "--store-dir"])
                .arg(cache.join("store"))
                .arg("--registry")
                .arg(&registry)
                .current_dir(self.root())
                .env("HOME", &self.home)
                .output()
                .unwrap();
            assert!(out.status.success(), "seeding the store: {out:?}");
        } else {
            fs::create_dir_all(&cache).unwrap();
            for entry in fs::read_dir(vendor).unwrap() {
                let path = entry.unwrap().path();
                fs::copy(&path, cache.join(path.file_name().unwrap())).unwrap();
            }
        }
    }

    /// Makes the cache read-only by its modes, as the sandbox's read-only
    /// bind makes it, for the unconfined runs.
    fn lock_cache(&mut self) {
        set_writable(&self.home.join(self.fixture.cache), false);
        self.locked = true;
    }

    /// The agent environment, with the fixture's cache unless `cache` is
    /// false, confined unless `confined` is false.
    fn agent(&self, cache: bool, confined: bool) -> AgentEnv {
        let caches: &[&str] = if cache { &[self.fixture.cache] } else { &[] };
        let agent = AgentEnv::new(self.env.agent_parent())
            .unwrap()
            .with_caches(caches);
        if confined {
            agent
        } else {
            agent.without_confinement()
        }
    }

    fn executor(&self, agent: AgentEnv) -> Executor {
        let runner = self.env.clone();
        Executor {
            git: Git::with_setup("git", move |command| runner.apply(command)),
            agent,
            forge_hosts: Vec::new(),
            timeout: Duration::from_secs(300),
            gate_timeout: Duration::from_secs(300),
        }
    }

    fn worktree(&self, case: &str) -> PathBuf {
        self.root().join(format!("worktree-{case}"))
    }

    /// The gate alone, on a fresh worktree for `case`, with `setup`.
    fn gate(&self, agent: AgentEnv, case: &str, setup: &[String]) -> GateRun {
        let worktree = self.worktree(case);
        let run_dir = self.root().join(format!("run-{case}"));
        let branch = format!("owlshift/{case}");
        self.executor(agent)
            .run_gate(&GateSpec {
                main: &self.remote.checkout,
                worktree: &worktree,
                branch: &branch,
                base: "origin/main",
                run_dir: &run_dir,
                commands: &self.config.stack.gate,
                setup,
            })
            .unwrap()
    }

    /// A Build run of the fake harness over `case`'s kept worktree: it
    /// observes the setup counter and the marker, then reports `done`.
    fn build(&self, agent: AgentEnv, case: &str) -> RunReport {
        let replies = self.root().join("replies");
        fs::create_dir_all(&replies).unwrap();
        fs::write(replies.join("done.json"), DONE).unwrap();
        let reply = Reply {
            observe: vec![".setup-count".to_owned(), self.fixture.marker.to_owned()],
            observe_into: Some(RelativePath::new(".owlshift/run/observed.txt").unwrap()),
            result: Some(replies.join("done.json")),
            ..Reply::default()
        };
        fs::write(replies.join("reply.toml"), reply.render()).unwrap();
        let harness = FakeHarness {
            program: PathBuf::from(env!("CARGO_BIN_EXE_owlshift-fake-harness")),
            reply: replies.join("reply.toml"),
            readable: self.env.agent_readable(),
        };
        let mut brief = brief();
        brief.gate = self.config.stack.gate.clone();
        let worktree = self.worktree(case);
        let branch = format!("owlshift/{case}");
        let run_dir = self.root().join(format!("run-{case}-build"));
        self.executor(agent)
            .run(
                &RunSpec {
                    main: &self.remote.checkout,
                    worktree: &worktree,
                    branch: &branch,
                    base: "origin/main",
                    run_dir: &run_dir,
                    brief: &brief,
                    setup: &self.config.stack.setup,
                },
                &harness,
            )
            .unwrap()
    }

    fn read(&self, case: &str, path: &str) -> String {
        fs::read_to_string(self.worktree(case).join(path)).unwrap()
    }
}

/// The integrity a lockfile gives the fixture's package.
fn integrity(lock: &str) -> String {
    let start = lock.find("integrity: ").unwrap() + "integrity: ".len();
    let end = start + lock[start..].find('}').unwrap();
    lock[start..end].trim().to_owned()
}

/// Serves the fixture's package on the loopback, its metadata and its
/// tarball, for the seeding alone; returns the registry's URL. The thread
/// ends with the test process.
fn serve_registry(integrity: String, tarball: Vec<u8>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    let packument = format!(
        r#"{{"name":"owlshift-fixture-dep","dist-tags":{{"latest":"1.0.0"}},"versions":{{"1.0.0":{{"name":"owlshift-fixture-dep","version":"1.0.0","main":"index.js","dist":{{"tarball":"{url}tarballs/owlshift-fixture-dep-1.0.0.tgz","integrity":"{integrity}"}}}}}}}}"#
    );
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request = String::new();
            let _ = reader.read_line(&mut request);
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
            }
            let path = request.split_whitespace().nth(1).unwrap_or_default();
            let (status, body, kind) = if path.ends_with(".tgz") {
                ("200 OK", tarball.clone(), "application/octet-stream")
            } else if path.trim_start_matches('/') == "owlshift-fixture-dep" {
                ("200 OK", packument.clone().into_bytes(), "application/json")
            } else {
                ("404 Not Found", Vec::new(), "text/plain")
            };
            let head = format!(
                "HTTP/1.1 {status}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(&body);
        }
    });
    url
}

/// Gives every folder and file under `path` its owner's write bit, or takes
/// every write bit away.
fn set_writable(path: &Path, writable: bool) {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return;
    };
    if meta.file_type().is_symlink() {
        return;
    }
    let mut mode = meta.permissions().mode();
    mode = if writable {
        mode | 0o200
    } else {
        mode & !0o222
    };
    if writable {
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }
    if meta.is_dir() {
        for entry in fs::read_dir(path).unwrap() {
            set_writable(&entry.unwrap().path(), writable);
        }
    }
    if !writable {
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }
}

fn brief() -> Brief {
    Brief {
        format: Format,
        role: Role::Build,
        project: "bench".into(),
        ticket: TicketBrief {
            id: TicketId::new("T-1").unwrap(),
            title: "A ticket".into(),
            url: None,
            labels: Vec::new(),
            author: Author {
                name: "maintainer".into(),
                relation: Relation::Decider,
            },
            description: "Do it.".into(),
        },
        decider: "maintainer".into(),
        thread: Vec::new(),
        resolve: Vec::new(),
        checkpoint: None,
        zones: Vec::new(),
        resources: Vec::new(),
        rules: Vec::new(),
        permissions: Permissions {
            level: PermissionLevel::WriteWorktree,
            network: false,
            browser: false,
        },
        gate: Vec::new(),
        always_human: Vec::new(),
        delegation: None,
        asked_by: None,
        gate_failure: None,
        result_refusal: None,
        decisions_refused: false,
        refused_choices: Vec::new(),
        check_failure: None,
        correction: None,
        rebase: None,
        result_path: RelativePath::new("result.json").unwrap(),
        plan: None,
        approval: false,
        findings: None,
        review: None,
        pull_request_reviews: Vec::new(),
    }
}

/// The two passes: the gate alone on a fresh worktree, then a Build over
/// that kept worktree with a new run temporary folder, whose setup finds the
/// first install and whose harness sees it, and the gate after its `done`.
fn installs_twice_keeping_the_first(bench: &Bench, confined: bool) {
    let fixture = bench.fixture;
    let first = bench.gate(
        bench.agent(true, confined),
        "T-1",
        &bench.config.stack.setup,
    );
    assert!(first.passed(), "{}: {:?}", fixture.name, first);
    assert_eq!(first.report.setup.len(), 1);
    fs::write(bench.worktree("T-1").join(fixture.marker), "first\n").unwrap();

    let report = bench.build(bench.agent(true, confined), "T-1");
    assert!(
        matches!(report.outcome, Outcome::Finished { .. }),
        "{}: {:?}",
        fixture.name,
        report.outcome
    );
    let setup = report
        .setup
        .as_ref()
        .expect("the setup ran before the harness");
    assert!(setup.passed(), "{setup:?}");
    assert_eq!(
        bench.read("T-1", ".owlshift/run/observed.txt"),
        format!(".setup-count=2\n{}=first\n", fixture.marker)
    );
    let gate = report.gate.as_ref().expect("the gate ran after the done");
    assert!(gate.passed(), "{gate:?}");
    assert_eq!(bench.read("T-1", ".setup-count"), "3\n");
    assert_eq!(bench.read("T-1", fixture.marker), "first\n");
}

/// Without the setup, the gate fails because the dependency is absent, not
/// because a tool is missing.
fn fails_without_the_setup(bench: &Bench, confined: bool) {
    let run = bench.gate(bench.agent(true, confined), "no-setup", &[]);
    let failure = run.report.failure.as_ref().expect("the gate failed");
    assert_eq!(failure.command.as_ref(), bench.config.stack.gate.first());
    assert!(
        failure.output.contains(bench.fixture.missing)
            && !failure.output.contains("command not found"),
        "{failure:?}"
    );
}

/// A setup command that writes into the cache is refused, so the setup
/// fails at it, the install before it having passed; the gate's own
/// commands never run.
fn a_write_into_the_cache_fails_the_setup(bench: &Bench, confined: bool) {
    let write = format!("touch \"$HOME/{}/x\"", bench.fixture.cache);
    let mut setup = bench.config.stack.setup.clone();
    setup.push(write.clone());
    let run = bench.gate(bench.agent(true, confined), "writes", &setup);
    let failure = run.report.failure.as_ref().expect("the setup failed");
    assert_eq!(failure.command.as_deref(), Some(write.as_str()));
    assert!(
        failure.reason.starts_with("setup: exit status"),
        "{failure:?}"
    );
    assert_eq!(run.report.setup.len(), 2);
    assert_eq!(run.report.setup[0].failure, None);
    assert!(run.report.steps.is_empty());
    assert!(!bench.home.join(bench.fixture.cache).join("x").exists());
}

/// The confined acceptance of one fixture: both passes, then the negatives,
/// each on a fresh worktree: setup removed, cache dropped, and a setup
/// writing into the cache.
fn passes_a_confined_gate(fixture: &'static Fixture) {
    if !runs(fixture, true) {
        return;
    }
    let bench = Bench::new(fixture);
    installs_twice_keeping_the_first(&bench, true);
    fails_without_the_setup(&bench, true);

    // Without the cache, the sandbox keeps the install from reading it.
    let run = bench.gate(
        bench.agent(false, true),
        "no-cache",
        &bench.config.stack.setup,
    );
    let failure = run.report.failure.as_ref().expect("the setup failed");
    assert_eq!(failure.command.as_ref(), bench.config.stack.setup.first());
    assert!(
        failure.reason.starts_with("setup: exit status"),
        "{failure:?}"
    );
    assert!(!failure.output.contains("command not found"), "{failure:?}");

    a_write_into_the_cache_fails_the_setup(&bench, true);
}

#[test]
fn a_node_project_passes_a_confined_gate() {
    passes_a_confined_gate(&NODE);
}

#[test]
fn a_python_project_passes_a_confined_gate() {
    passes_a_confined_gate(&PYTHON);
}

/// The same recipes unconfined, the cache read-only by its modes: what a
/// run that cannot nest the sandbox can check of them.
fn keeps_its_install_unconfined(fixture: &'static Fixture) {
    if !runs(fixture, false) {
        return;
    }
    let mut bench = Bench::new(fixture);
    bench.lock_cache();
    installs_twice_keeping_the_first(&bench, false);
    fails_without_the_setup(&bench, false);
    a_write_into_the_cache_fails_the_setup(&bench, false);
}

#[test]
fn a_node_projects_recipe_keeps_its_install() {
    keeps_its_install_unconfined(&NODE);
}

#[test]
fn a_python_projects_recipe_keeps_its_install() {
    keeps_its_install_unconfined(&PYTHON);
}

/// A cache the fixture declares but the operator does not allow makes its
/// project file invalid when the configuration is loaded, naming
/// `allow_caches`, so no gate runs; listed, it loads.
#[test]
fn a_fixtures_cache_needs_the_operators_allowance() {
    for fixture in [&NODE, &PYTHON] {
        let tmp = tempfile::tempdir().unwrap();
        let env = GitEnv::create(tmp.path().join("home")).unwrap();
        let remote = seed(
            &env,
            tmp.path(),
            &fixtures().join(fixture.name).join("repo"),
        )
        .unwrap();
        let personal = tmp.path().join("personal.toml");
        fs::write(&personal, "").unwrap();
        let load = || Effective::load(&HostSystem, &remote.checkout, Some(personal.clone()));
        let refused = load();
        let FileState::Invalid { error, .. } = &refused.project else {
            panic!("{}: {refused}", fixture.name);
        };
        assert!(
            error.contains(&format!("stack.caches: {:?}", fixture.cache))
                && error.contains("`allow_caches`"),
            "{error}"
        );
        fs::write(&personal, format!("allow_caches = [{:?}]\n", fixture.cache)).unwrap();
        let allowed = load();
        assert!(allowed.is_valid(), "{}: {allowed}", fixture.name);
    }
}
