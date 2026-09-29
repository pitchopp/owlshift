//! What the executor refuses before it spawns anything: a credential within
//! the agent's reach, a worktree on another branch or whose `.git` was
//! redirected, and run files reached through a link. A harness that must
//! never run stands in. Then what it does after a run that redirects its
//! `.git` link: a quarantine, with no runner git run through the link.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tempfile::TempDir;

use owlshift_contracts::Role;
use owlshift_contracts::brief::{
    Author, Brief, PermissionLevel, Permissions, Relation, TicketBrief,
};
use owlshift_contracts::format::Format;
use owlshift_contracts::ids::{RelativePath, TicketId};
use owlshift_runner::agent_env::{AgentEnv, CredentialFinding};
use owlshift_runner::executor::harness::drive_plain;
use owlshift_runner::executor::{
    Executor, ExecutorError, Git, Harness, HarnessEnd, HarnessError, HarnessRun, HarnessStatus,
    Outcome, RunLog, RunReport, RunSpec, Violation,
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
        // Bare: these refusals come before the sandbox, whose own are in
        // `tests/confinement.rs`.
        let executor = Executor {
            git: Git::with_setup("git", move |command| runner.apply(command)),
            agent: AgentEnv::new(env.agent_parent(), &[])
                .unwrap()
                .without_confinement(),
            forge_hosts: Vec::new(),
            timeout: Duration::from_secs(60),
            gate_timeout: Duration::from_secs(60),
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
            gate_failure: None,
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
        self.run_in(&self.worktree)
    }

    /// A run of `harness` with `executor` and `brief`.
    fn run_with(
        &self,
        executor: &Executor,
        harness: &dyn Harness,
        brief: &Brief,
    ) -> Result<RunReport, ExecutorError> {
        let spec = RunSpec {
            main: &self.main,
            worktree: &self.worktree,
            branch: BRANCH,
            base: "origin/main",
            run_dir: &self.tmp.path().join("run"),
            brief,
        };
        executor.run(&spec, harness)
    }

    /// A run whose worktree is `worktree`.
    fn run_in(&self, worktree: &Path) -> ExecutorError {
        let spec = RunSpec {
            main: &self.main,
            worktree,
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

/// A worktree refused before any git runs in it: its `.git` is not a link
/// into the main checkout's repository.
fn not_linked(error: &ExecutorError) -> bool {
    matches!(error, ExecutorError::Worktree(reason) if reason.contains("the worktree's .git link"))
}

/// A clone of the same remote, on the ticket's branch at the commit the
/// branch has in the main checkout: another repository, whose shared git
/// files the isolation check would not watch.
#[test]
fn a_separate_clone_is_not_the_tickets_worktree() {
    let bench = Bench::new();
    bench
        .env
        .run(&bench.main, &["branch", BRANCH, "origin/main"])
        .unwrap();
    bench
        .env
        .run(
            bench.tmp.path(),
            &["clone", "--quiet", "remote.git", "worktree"],
        )
        .unwrap();
    bench
        .env
        .run(&bench.worktree, &["switch", "--quiet", "-c", BRANCH])
        .unwrap();
    let error = bench.run();
    assert!(not_linked(&error), "{error:?}");
}

/// The main checkout itself, on the ticket's branch.
#[test]
fn the_main_checkout_is_not_the_tickets_worktree() {
    let bench = Bench::new();
    bench
        .env
        .run(&bench.main, &["switch", "--quiet", "-c", BRANCH])
        .unwrap();
    let error = bench.run_in(&bench.main);
    assert!(not_linked(&error), "{error:?}");
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

/// A harness that is a shell script, run in the worktree.
struct Script(String);

impl Harness for Script {
    fn command(&self, _run: &HarnessRun<'_>) -> Result<Command, HarnessError> {
        let mut command = Command::new("sh");
        command.arg("-c").arg(&self.0);
        Ok(command)
    }

    fn drive(
        &self,
        _: &HarnessRun<'_>,
        child: &mut Child,
        log: &mut RunLog,
    ) -> io::Result<HarnessEnd> {
        let run = drive_plain(child, log)?;
        Ok(HarnessEnd::new(run.exit_code, HarnessStatus::Completed))
    }
}

/// The shell command that points the worktree's `.git` at `evil`.
fn redirect(evil: &Path) -> String {
    format!("printf 'gitdir: %s\\n' '{}' > .git", evil.display())
}

/// A run that points its worktree's `.git` elsewhere, from the harness or
/// from the project's gate, is quarantined by `Executor::run` itself, and
/// the runner's git never runs while the link is redirected.
#[test]
fn a_run_that_redirects_its_git_link_is_quarantined_before_any_git() {
    for case in ["the harness", "the gate"] {
        let bench = Bench::new();
        let evil = bench.tmp.path().join("evil");
        fs::create_dir_all(&evil).unwrap();
        let link = bench.worktree.join(".git");
        // Every runner git command made while the link leads to `evil`.
        let redirected: Arc<Mutex<Vec<String>>> = Arc::default();
        let runner = bench.env.clone();
        let seen = redirected.clone();
        let evil_link = format!("gitdir: {}\n", evil.display());
        let executor = Executor {
            git: Git::with_setup("git", move |command| {
                runner.apply(command);
                if fs::read_to_string(&link).is_ok_and(|text| text == evil_link) {
                    seen.lock().unwrap().push(format!("{command:?}"));
                }
            }),
            ..bench.executor.clone()
        };
        let done = r#"{"format":1,"status":"done","summary":"s","pr":{"branch":"owlshift/T-1","title":"t","body":"b"}}"#;
        let write_result = format!("printf '%s' '{done}' > .owlshift/run/result.json");
        let mut brief = bench.brief.clone();
        let script = if case == "the harness" {
            format!("{} && {write_result}", redirect(&evil))
        } else {
            brief.gate = vec![redirect(&evil)];
            write_result
        };
        let report = bench.run_with(&executor, &Script(script), &brief).unwrap();
        match &report.outcome {
            Outcome::Quarantined(violations) => assert!(
                matches!(violations.as_slice(), [Violation::WorktreeLink(reason)]
                    if reason.contains("the worktree's .git link was changed")),
                "{case}: {violations:?}"
            ),
            other => panic!("{case}: {other:?}"),
        }
        assert_eq!(report.gate.is_some(), case == "the gate", "{case}");
        assert_eq!(*redirected.lock().unwrap(), Vec::<String>::new(), "{case}");
    }
}

/// A kept worktree whose `.git` an earlier run redirected is refused before
/// any git runs in it.
#[test]
fn a_kept_worktree_with_a_redirected_link_refuses_the_run() {
    let bench = Bench::new();
    assert!(matches!(bench.run(), ExecutorError::Command(_)));
    let evil = bench.tmp.path().join("evil");
    fs::create_dir_all(&evil).unwrap();
    fs::write(
        bench.worktree.join(".git"),
        format!("gitdir: {}\n", evil.display()),
    )
    .unwrap();
    let error = bench.run();
    assert!(not_linked(&error), "{error:?}");
}

/// The runner's git runs no hook and no file-system monitor that the main
/// checkout's configuration names: neither `worktree add`'s `post-checkout`
/// nor `git status`'s monitor fires, where plain git runs both.
#[cfg(unix)]
#[test]
fn the_runners_git_runs_no_hook_and_no_monitor() {
    use std::os::unix::fs::PermissionsExt;

    let bench = Bench::new();
    let fired = bench.tmp.path().join("fired");
    fs::create_dir_all(&fired).unwrap();
    let hooks = bench.tmp.path().join("hooks");
    fs::create_dir_all(&hooks).unwrap();
    let script = |path: &Path, name: &str| {
        let sentinel = fired.join(name);
        fs::write(
            path,
            format!("#!/bin/sh\necho ran > '{}'\n", sentinel.display()),
        )
        .unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
        sentinel
    };
    let checkout_hook = script(&hooks.join("post-checkout"), "post-checkout");
    let monitor = script(&bench.tmp.path().join("monitor"), "fsmonitor");
    for (key, value) in [
        ("core.hooksPath", hooks.display().to_string()),
        // Git runs the monitor through the shell, and the bench's path holds
        // a space.
        (
            "core.fsmonitor",
            format!("'{}'", bench.tmp.path().join("monitor").display()),
        ),
    ] {
        bench
            .env
            .run(&bench.main, &["config", key, &value])
            .unwrap();
    }

    // The worktree is added, the snapshot reads the status, then the
    // command is refused.
    assert!(matches!(bench.run(), ExecutorError::Command(_)));
    assert!(!checkout_hook.exists(), "the post-checkout hook ran");
    assert!(!monitor.exists(), "the file-system monitor ran");

    // The control: plain git runs both.
    bench
        .env
        .run(
            &bench.main,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                "control",
                "../control",
                "origin/main",
            ],
        )
        .unwrap();
    bench
        .env
        .run(&bench.main, &["status", "--porcelain"])
        .unwrap();
    assert!(checkout_hook.exists(), "the control's hook never ran");
    assert!(monitor.exists(), "the control's monitor never ran");
}
