//! What the executor refuses before it spawns anything: a credential within
//! the agent's reach, a worktree on another branch, and run files reached
//! through a link. A harness that must never run stands in.

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
use owlshift_runner::agent_env::{AgentEnv, CredentialFinding};
use owlshift_runner::executor::{
    Executor, ExecutorError, Git, Harness, HarnessEnd, HarnessError, HarnessRun, RunLog, RunSpec,
};
use owlshift_testkit::git::{GitEnv, seed};

const BRANCH: &str = "owlshift/T-1";

/// Stops every run at its command, just before the spawn.
struct NeverRuns;

impl Harness for NeverRuns {
    fn command(&self, _run: &HarnessRun<'_>) -> Result<Command, HarnessError> {
        Err("this harness never runs".into())
    }

    fn drive(&self, _: &HarnessRun<'_>, _: &mut Child, _: &mut RunLog) -> io::Result<HarnessEnd> {
        unreachable!("never spawned")
    }
}

struct Bench {
    tmp: TempDir,
    env: GitEnv,
    executor: Executor,
    main: PathBuf,
    worktree: PathBuf,
    brief: Brief,
}

impl Bench {
    fn new() -> Self {
        let tmp = tempfile::Builder::new()
            .prefix("owlshift executor ")
            .tempdir()
            .unwrap();
        let project = tmp.path().join("project");
        fs::create_dir_all(&project).unwrap();
        fs::write(project.join("README.md"), "hello\n").unwrap();
        let env = GitEnv::create(tmp.path().join("home")).unwrap();
        let remote = seed(&env, tmp.path(), &project).unwrap();
        let runner = env.clone();
        let executor = Executor {
            git: Git::with_setup("git", move |command| runner.apply(command)),
            agent: AgentEnv::new(env.agent_parent(), &[]).unwrap(),
            forge_hosts: Vec::new(),
            timeout: Duration::from_secs(60),
        };
        let author = Author {
            name: "maintainer".into(),
            relation: Relation::Decider,
        };
        let brief = Brief {
            format: Format,
            role: Role::Build,
            project: "bench".into(),
            ticket: TicketBrief {
                id: TicketId::new("T-1").unwrap(),
                title: "A ticket".into(),
                url: None,
                labels: Vec::new(),
                author,
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
            result_path: RelativePath::new("result.json").unwrap(),
        };
        Self {
            worktree: tmp.path().join("worktree"),
            main: remote.checkout,
            tmp,
            env,
            executor,
            brief,
        }
    }

    fn run(&self) -> ExecutorError {
        let spec = RunSpec {
            main: &self.main,
            worktree: &self.worktree,
            branch: BRANCH,
            base: "origin/main",
            run_dir: &self.tmp.path().join("run"),
            brief: &self.brief,
        };
        self.executor
            .run(&spec, &NeverRuns)
            .expect_err("the harness never runs")
    }
}

#[test]
fn a_credential_within_the_agents_reach_refuses_the_run() {
    let bench = Bench::new();
    bench
        .env
        .run(
            &bench.main,
            &[
                "config",
                "remote.origin.pushurl",
                "https://u:fixture-secret@example.invalid/r.git",
            ],
        )
        .unwrap();
    let error = bench.run();
    assert!(
        matches!(
            &error,
            ExecutorError::Credentials(findings)
                if findings == &[CredentialFinding::GitSetting { key: "remote.origin.pushurl".into() }]
        ),
        "{error:?}"
    );
    assert!(!error.to_string().contains("fixture-secret"), "{error}");
}

#[test]
fn a_worktree_on_another_branch_refuses_the_run() {
    let bench = Bench::new();
    bench
        .env
        .run(
            &bench.main,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "other",
                "../worktree",
                "origin/main",
            ],
        )
        .unwrap();
    let error = bench.run();
    assert!(
        matches!(&error, ExecutorError::Worktree(reason) if reason.contains("refs/heads/other")),
        "{error:?}"
    );
}

#[cfg(unix)]
#[test]
fn no_run_file_is_written_through_a_link() {
    use std::os::unix::fs::symlink;

    let bench = Bench::new();
    // A first run creates the worktree and its run files, then stops at the
    // command.
    assert!(matches!(bench.run(), ExecutorError::Command(_)));
    let run_dir = bench.worktree.join(".owlshift/run");
    let brief = run_dir.join("brief.json");
    assert!(
        fs::read_to_string(&brief)
            .unwrap()
            .contains(".owlshift/run/result.json")
    );

    // A brief left as a link by an earlier run is replaced, not followed.
    let outside = bench.tmp.path().join("outside.txt");
    fs::write(&outside, "keep\n").unwrap();
    fs::remove_file(&brief).unwrap();
    symlink(&outside, &brief).unwrap();
    assert!(matches!(bench.run(), ExecutorError::Command(_)));
    assert_eq!(fs::read_to_string(&outside).unwrap(), "keep\n");
    assert!(
        !fs::symlink_metadata(&brief)
            .unwrap()
            .file_type()
            .is_symlink()
    );

    // A run directory that is a link is refused.
    let elsewhere = bench.tmp.path().join("elsewhere");
    fs::create_dir(&elsewhere).unwrap();
    fs::remove_dir_all(&run_dir).unwrap();
    symlink(&elsewhere, &run_dir).unwrap();
    let error = bench.run();
    assert!(matches!(&error, ExecutorError::Layout(_)), "{error:?}");
    assert_eq!(fs::read_dir(&elsewhere).unwrap().count(), 0);
}
