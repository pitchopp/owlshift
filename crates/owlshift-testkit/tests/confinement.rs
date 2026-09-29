//! OWL-41's acceptance through the executor: a harness and the processes it
//! starts, spawned by `Executor::run`, run inside the OS sandbox. A harness
//! that is a shell script stands in, so its commands are the harness's own
//! descendants. The gate's commands are checked the same way, and in more
//! detail, by the unit tests of `owlshift-runner`'s `executor::gate`.
//!
//! Where agents cannot be confined (a Linux machine without bwrap or user
//! namespaces), the test is skipped with a message, unless
//! `OWLSHIFT_REQUIRE_CONFINEMENT` is set; native Windows must refuse.

use std::fs;
use std::io;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::Duration;

use tempfile::TempDir;

use owlshift_contracts::Role;
use owlshift_contracts::brief::{
    Author, Brief, PermissionLevel, Permissions, Relation, TicketBrief,
};
use owlshift_contracts::format::Format;
use owlshift_contracts::ids::{RelativePath, TicketId};
use owlshift_runner::agent_env::AgentEnv;
use owlshift_runner::executor::harness::drive_plain;
use owlshift_runner::executor::{
    Executor, ExecutorError, Git, Harness, HarnessEnd, HarnessError, HarnessRun, HarnessStatus,
    RunLog, RunReport, RunSpec,
};
use owlshift_testkit::git::{GitEnv, seed};

const BRANCH: &str = "owlshift/T-1";

/// A harness that is one shell script.
struct Script(String);

impl Harness for Script {
    fn command(&self, _run: &HarnessRun<'_>) -> Result<Command, HarnessError> {
        let mut command = Command::new("sh");
        command.arg("-c").arg(&self.0);
        Ok(command)
    }

    fn drive(
        &self,
        _run: &HarnessRun<'_>,
        child: &mut Child,
        log: &mut RunLog,
    ) -> io::Result<HarnessEnd> {
        let run = drive_plain(child, log)?;
        Ok(HarnessEnd::new(run.exit_code, HarnessStatus::Completed))
    }
}

struct Bench {
    tmp: TempDir,
    env: GitEnv,
    main: PathBuf,
    worktree: PathBuf,
}

impl Bench {
    /// A project, and fake secrets planted in the bench's home: a gh token
    /// file and another project's `.env`.
    fn new() -> Self {
        let tmp = tempfile::Builder::new()
            .prefix("owlshift confinement ")
            .tempdir()
            .unwrap();
        let project = tmp.path().join("project");
        fs::create_dir_all(&project).unwrap();
        fs::write(project.join("README.md"), "hello\n").unwrap();
        let home = tmp.path().join("home");
        let env = GitEnv::create(home.clone()).unwrap();
        let remote = seed(&env, tmp.path(), &project).unwrap();
        fs::create_dir_all(home.join(".config/gh")).unwrap();
        fs::create_dir_all(home.join("other-project")).unwrap();
        fs::write(
            home.join(".config/gh/hosts.yml"),
            "example.invalid:\n    oauth_token: gho_FAKE_owl41\n",
        )
        .unwrap();
        fs::write(home.join("other-project/.env"), "API_KEY=FAKE_owl41\n").unwrap();
        Self {
            worktree: tmp.path().join("worktree"),
            main: remote.checkout,
            tmp,
            env,
        }
    }

    fn agent(&self) -> AgentEnv {
        AgentEnv::new(self.env.agent_parent()).unwrap()
    }

    fn run(&self, agent: AgentEnv, script: &str) -> Result<RunReport, ExecutorError> {
        let runner = self.env.clone();
        let executor = Executor {
            git: Git::with_setup("git", move |command| runner.apply(command)),
            agent,
            forge_hosts: Vec::new(),
            timeout: Duration::from_secs(60),
            gate_timeout: Duration::from_secs(60),
        };
        let spec = RunSpec {
            main: &self.main,
            worktree: &self.worktree,
            branch: BRANCH,
            base: "origin/main",
            run_dir: &self.tmp.path().join("run"),
            brief: &brief(),
        };
        executor.run(&spec, &Script(script.to_owned()))
    }

    /// The probe's lines, `name=status`, written in the worktree.
    #[cfg(unix)]
    fn probe(&self) -> Vec<String> {
        fs::read_to_string(self.worktree.join("probe.txt"))
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect()
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
        gate_failure: None,
        result_path: RelativePath::new("result.json").unwrap(),
    }
}

/// The harness reads the planted files itself, then through a child of its
/// own, and leaves a grandchild that would write into the worktree two
/// seconds later, its output let go so the run does not wait for it.
#[cfg(unix)]
const PROBE: &str = r#"
cat "$HOME/.config/gh/hosts.yml" > /dev/null 2>&1; echo "token=$?" > probe.txt
cat "$HOME/other-project/.env" > /dev/null 2>&1; echo "env=$?" >> probe.txt
sh -c 'cat "$HOME/.config/gh/hosts.yml" > /dev/null 2>&1; echo "child=$?" >> probe.txt'
(sleep 2; echo late > late.txt) > /dev/null 2>&1 &
"#;

#[cfg(unix)]
fn sandbox_or_skip(agent: &AgentEnv) -> bool {
    match agent.sandbox_ready() {
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

/// The run reaches no planted secret, from the harness or its child, while
/// the bare control reads both; and the grandchild the run left behind is
/// stopped with it, so its late write never lands.
#[cfg(unix)]
#[test]
fn a_confined_run_and_its_descendants_reach_no_planted_secret() {
    let bench = Bench::new();
    let agent = bench.agent();
    if !sandbox_or_skip(&agent) {
        return;
    }

    bench
        .run(agent.clone().without_confinement(), PROBE)
        .unwrap();
    assert_eq!(bench.probe(), ["token=0", "env=0", "child=0"]);
    std::thread::sleep(Duration::from_secs(3));
    assert!(
        !bench.worktree.join("late.txt").exists(),
        "the bare grandchild outlived the run"
    );

    let report = bench.run(agent, PROBE).unwrap();
    let probe = bench.probe();
    for line in &probe {
        assert!(
            !line.ends_with("=0"),
            "a confined read succeeded: {probe:?}"
        );
    }
    assert_eq!(probe.len(), 3, "{probe:?}");
    std::thread::sleep(Duration::from_secs(3));
    assert!(
        !bench.worktree.join("late.txt").exists(),
        "a confined grandchild outlived the run"
    );
    // The run wrote only in its worktree, and left the rest as it was.
    assert!(
        !matches!(
            report.outcome,
            owlshift_runner::executor::Outcome::Quarantined(_)
        ),
        "{:?}",
        report.outcome
    );
}

/// Native Windows has no sandbox: the run is refused before anything is
/// written, and the error says to use WSL2.
#[cfg(windows)]
#[test]
fn a_run_on_native_windows_is_refused() {
    let bench = Bench::new();
    let error = bench.run(bench.agent(), "echo hi").unwrap_err();
    assert!(
        matches!(&error, ExecutorError::Spawn(e) if e.to_string().contains("WSL2")),
        "{error:?}"
    );
    assert!(!bench.worktree.exists(), "the worktree was created");
}
