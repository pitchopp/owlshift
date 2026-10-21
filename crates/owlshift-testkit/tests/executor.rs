//! What the executor refuses before it spawns anything: a credential within
//! the agent's reach, a worktree on another branch or whose `.git` was
//! redirected, and run files reached through a link. A harness that must
//! never run stands in. Then what it does after a run that redirects its
//! `.git` link: a quarantine, with no runner git run through the link. And
//! when a read-only run left no result but gave it as its last message
//! (OWL-323).

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tempfile::TempDir;

use owlshift_contracts::Role;
use owlshift_contracts::brief::GateFailure;
use owlshift_contracts::brief::{
    Author, Brief, PermissionLevel, Permissions, RebaseBrief, Relation, TicketBrief,
};
use owlshift_contracts::format::{Format, RESULT_FORMAT};
use owlshift_contracts::ids::{RelativePath, SessionId, TicketId};
use owlshift_platform::keychain::Secret;
use owlshift_runner::agent_env::{AgentEnv, CredentialFinding};
use owlshift_runner::executor::harness::drive_plain;
use owlshift_runner::executor::{
    Executor, ExecutorError, Failure, GateReport, Git, Harness, HarnessEnd, HarnessError,
    HarnessLogin, HarnessRun, HarnessStatus, Outcome, ResumeFrom, RunLog, RunReport, RunSpec,
    SETUP_LOG, SandboxNeeds, SessionPlan, Violation,
};
use owlshift_testkit::git::{GitEnv, seed};
use owlshift_testkit::harness::FakeHarness;
use owlshift_testkit::reply::Reply;

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
            agent: AgentEnv::new(env.agent_parent())
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
            resolve: Vec::new(),
            checkpoint: None,
            zones: Vec::new(),
            resources: Vec::new(),
            named_resources: Default::default(),
            relations: Default::default(),
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
            setup_failure: None,
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
            setup: &[],
            session: None,
            siblings: None,
        };
        executor.run(&spec, harness)
    }

    /// A run whose worktree is `worktree`.
    fn run_in(&self, worktree: &Path) -> ExecutorError {
        self.run_from(worktree, BRANCH, "origin/main")
    }

    /// A run whose worktree is `worktree`, on `branch` started from `base`.
    fn run_from(&self, worktree: &Path, branch: &str, base: &str) -> ExecutorError {
        let spec = RunSpec {
            main: &self.main,
            worktree,
            branch,
            base,
            run_dir: &self.tmp.path().join("run"),
            brief: &self.brief,
            setup: &[],
            session: None,
            siblings: None,
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
    // The dispatch's start of the branch refuses it as the run does
    // (OWL-274).
    let started = bench.start("origin/main");
    let error = bench.run();
    for error in [&started.expect_err("refused"), &error] {
        assert!(
            matches!(error, ExecutorError::Worktree(reason) if reason.contains("refs/heads/other")),
            "{error:?}"
        );
    }
}

impl Bench {
    /// What `rev` resolves to in the main checkout.
    fn commit(&self, rev: &str) -> String {
        let out = self
            .env
            .run(&self.main, &["rev-parse", "--verify", rev])
            .unwrap();
        String::from_utf8(out).unwrap().trim().to_owned()
    }

    /// A new commit on top of the main checkout's `HEAD`, on no branch.
    fn stray_commit(&self) -> String {
        let out = self
            .env
            .run(
                &self.main,
                &["commit-tree", "-p", "HEAD", "-m", "planted", "HEAD^{tree}"],
            )
            .unwrap();
        String::from_utf8(out).unwrap().trim().to_owned()
    }

    fn head_of(&self, worktree: &Path) -> String {
        let out = self.env.run(worktree, &["rev-parse", "HEAD"]).unwrap();
        String::from_utf8(out).unwrap().trim().to_owned()
    }
}

/// A base given by name starts from the remote-tracking ref, not from a
/// local branch named like it; a base given as a full commit id starts
/// there (OWL-66).
#[test]
fn a_local_branch_named_like_the_base_does_not_shadow_it() {
    let bench = Bench::new();
    let remote = bench.commit("refs/remotes/origin/main");
    let planted = bench.stray_commit();
    assert_ne!(planted, remote);
    bench
        .env
        .run(
            &bench.main,
            &["update-ref", "refs/heads/origin/main", &planted],
        )
        .unwrap();

    // Each run creates its worktree and branch, then stops at the command.
    let error = bench.run();
    assert!(matches!(error, ExecutorError::Command(_)), "{error:?}");
    assert_eq!(bench.head_of(&bench.worktree), remote);

    let by_id = bench.tmp.path().join("by-id");
    let error = bench.run_from(&by_id, "owlshift/T-2", &planted);
    assert!(matches!(error, ExecutorError::Command(_)), "{error:?}");
    assert_eq!(bench.head_of(&by_id), planted);
}

/// A base that is not exactly a remote-tracking ref or a full commit id is
/// refused before any worktree is made: a missing remote-tracking ref,
/// even with a local branch at its full name, an abbreviated id, and a
/// revision expression (OWL-66).
/// A rebase run's isolation check accepts a descent from the base it was
/// told (OWL-210): a brief whose base is not the run's own is refused before
/// anything is written.
#[test]
fn a_rebase_onto_another_base_than_the_runs_is_refused() {
    let mut bench = Bench::new();
    let onto = "1".repeat(40);
    bench.brief.role = Role::Rebase;
    bench.brief.rebase = Some(RebaseBrief {
        pull_request: 1,
        url: "https://github.com/demo/project/pull/1".into(),
        branch: BRANCH.into(),
        head: "2".repeat(40),
        base_branch: "main".into(),
        onto: onto.clone(),
        refresh: false,
        conflicting: Vec::new(),
    });
    let error = bench.run_from(&bench.worktree, BRANCH, &"3".repeat(40));
    assert!(
        matches!(&error, ExecutorError::Spec(reason) if reason.contains(&onto)),
        "{error:?}"
    );
    assert!(!bench.worktree.exists());
}

#[test]
fn a_base_that_is_not_a_remote_tracking_ref_or_a_full_id_is_refused() {
    let bench = Bench::new();
    let remote = bench.commit("refs/remotes/origin/main");
    bench
        .env
        .run(
            &bench.main,
            &["update-ref", "refs/heads/refs/remotes/origin/gone", &remote],
        )
        .unwrap();
    for base in ["origin/gone", &remote[..12], "origin/main~0"] {
        let error = bench.run_from(&bench.worktree, BRANCH, base);
        assert!(
            matches!(&error, ExecutorError::Spec(reason) if reason.contains(base)),
            "{base}: {error:?}"
        );
        assert!(!bench.worktree.exists(), "{base}");
    }
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

/// A harness that sleeps, and whose driver stops the run itself when
/// `stops`, then reports a failure of its own.
struct StopsItself {
    stops: bool,
}

impl Harness for StopsItself {
    fn command(&self, _run: &HarnessRun<'_>) -> Result<Command, HarnessError> {
        let mut command = Command::new("sh");
        command.arg("-c").arg("sleep 30");
        Ok(command)
    }

    fn drive(
        &self,
        run: &HarnessRun<'_>,
        child: &mut Child,
        log: &mut RunLog,
    ) -> io::Result<HarnessEnd> {
        if self.stops {
            run.stop.expect("a driven run has its stop").stop();
        }
        let run = drive_plain(child, log)?;
        Ok(HarnessEnd::new(
            run.exit_code,
            HarnessStatus::Failed("stopped by its driver".to_owned()),
        ))
    }
}

/// OWL-319: a run its driver stopped is judged by its harness's end, even
/// when the deadline passed first; one it did not stop timed out.
#[test]
fn a_run_its_driver_stopped_did_not_time_out() {
    let bench = Bench::new();
    // The deadline passes at once, before or while the run is driven.
    let executor = Executor {
        timeout: Duration::ZERO,
        ..bench.executor.clone()
    };
    for stops in [true, false] {
        let report = bench
            .run_with(&executor, &StopsItself { stops }, &bench.brief)
            .unwrap();
        match (&report.outcome, stops) {
            (Outcome::Failed(Failure::Harness(reason)), true) => {
                assert_eq!(reason, "stopped by its driver");
            }
            (Outcome::Failed(Failure::TimedOut), false) => {}
            (other, _) => panic!("stops: {stops}: {other:?}"),
        }
        assert!(report.elapsed < Duration::from_secs(20), "{report:?}");
    }
}

/// A harness that is a shell script, run in the worktree, whose driver
/// reports a completed run ending on `message` with one `Write` refused, as
/// Claude Code's driver reports a read-only run (OWL-323); logged in with
/// `login` when given; asking for the read-only profile and giving its
/// result as its last message with `in_message`, as Codex and the
/// resolver's Claude Code run with Bash do (OWL-344).
struct EndsOn {
    script: String,
    message: String,
    login: Option<Secret>,
    in_message: bool,
}

impl Harness for EndsOn {
    fn command(&self, _run: &HarnessRun<'_>) -> Result<Command, HarnessError> {
        let mut command = Command::new("sh");
        command.arg("-c").arg(&self.script);
        Ok(command)
    }

    fn drive(
        &self,
        _: &HarnessRun<'_>,
        child: &mut Child,
        log: &mut RunLog,
    ) -> io::Result<HarnessEnd> {
        let run = drive_plain(child, log)?;
        let mut end = HarnessEnd::new(run.exit_code, HarnessStatus::Completed);
        end.message = Some(self.message.clone());
        end.permission_denials = vec!["Write".to_owned()];
        Ok(end)
    }

    fn sandbox_needs(&self, _agent: &AgentEnv) -> Result<SandboxNeeds, HarnessError> {
        Ok(SandboxNeeds {
            login: self.login.clone().map(|token| HarnessLogin {
                descriptor_variable: "OWLSHIFT_TEST_TOKEN_FD",
                token,
                config_variable: "OWLSHIFT_TEST_CONFIG_DIR",
            }),
            read_only: self.in_message,
            result_in_message: self.in_message,
            ..SandboxNeeds::default()
        })
    }
}

/// A valid `blocked` result.
fn blocked(summary: &str) -> String {
    format!(r#"{{"format":{RESULT_FORMAT},"status":"blocked","summary":"{summary}"}}"#)
}

impl Bench {
    /// The bench's brief, for a read-only design run.
    fn read_only_brief(&self) -> Brief {
        let mut brief = self.brief.clone();
        brief.role = Role::Design;
        brief.permissions.level = PermissionLevel::ReadOnly;
        brief
    }

    /// A run of [`EndsOn`], not logged in.
    fn ends_on(&self, brief: &Brief, script: &str, message: &str) -> RunReport {
        let harness = EndsOn {
            script: script.to_owned(),
            message: message.to_owned(),
            login: None,
            in_message: false,
        };
        self.run_with(&self.executor, &harness, brief).unwrap()
    }
}

/// The summary of a finished run.
fn summary(report: &RunReport) -> &str {
    match &report.outcome {
        Outcome::Finished { result, .. } => &result.summary,
        other => panic!("{other:?}"),
    }
}

/// OWL-323: a read-only run that left no result is read from its last
/// message, with a warning naming the refused tools; a result file it left
/// wins, with no warning; an invalid result there is refused as any; a
/// write-in-worktree run, and a run past its deadline, get no such reading.
#[test]
fn a_read_only_run_without_a_result_is_read_from_its_last_message() {
    let bench = Bench::new();
    let read_only = bench.read_only_brief();
    let message = format!(
        "I could not write it.\n\n```json\n{}\n```",
        blocked("from the message")
    );

    let report = bench.ends_on(&read_only, "true", &message);
    assert_eq!(summary(&report), "from the message");
    assert_eq!(
        report.warnings,
        [
            "the role left no .owlshift/run/result.json: its result was read from its last \
             message (the harness refused: Write)"
        ]
    );

    let written = format!(
        "printf '%s' '{}' > .owlshift/run/result.json",
        blocked("from the file")
    );
    let report = bench.ends_on(&read_only, &written, &message);
    assert_eq!(summary(&report), "from the file");
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);

    let no_plan = format!(r#"{{"format":{RESULT_FORMAT},"status":"done","summary":"s"}}"#);
    let report = bench.ends_on(&read_only, "true", &no_plan);
    assert!(
        matches!(&report.outcome, Outcome::Failed(Failure::InvalidResult(reason)) if reason.contains("no plan")),
        "{:?}",
        report.outcome
    );

    let report = bench.ends_on(&bench.brief, "true", &message);
    assert!(
        matches!(report.outcome, Outcome::Failed(Failure::NoResult)),
        "{:?}",
        report.outcome
    );
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);

    let late = Executor {
        timeout: Duration::ZERO,
        ..bench.executor.clone()
    };
    let harness = EndsOn {
        script: "sleep 30".into(),
        message,
        login: None,
        in_message: false,
    };
    let report = bench.run_with(&late, &harness, &read_only).unwrap();
    assert!(
        matches!(report.outcome, Outcome::Failed(Failure::TimedOut)),
        "{:?}",
        report.outcome
    );
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    assert!(!bench.worktree.join(".owlshift/run/result.json").exists());
}

/// OWL-344: the executor writes the last message of a run whose result it
/// is whole, as Codex's: JSON is its result, with no warning; prose, even
/// ending on a closing JSON block, is refused as an invalid result. Reading
/// such a block is the Claude Code harness's (`claude_end`), not the
/// executor's.
#[test]
fn a_result_given_as_the_last_message_is_written_whole() {
    let bench = Bench::new();
    let read_only = bench.read_only_brief();
    let run = |message: String| {
        let harness = EndsOn {
            script: "true".into(),
            message,
            login: None,
            in_message: true,
        };
        bench
            .run_with(&bench.executor, &harness, &read_only)
            .unwrap()
    };

    let report = run(format!("\n{}\n", blocked("bare")));
    assert_eq!(summary(&report), "bare");
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);

    for prose in [
        format!(
            "I checked the history.\n\n```json\n{}\n```\n",
            blocked("from the block")
        ),
        "I could not decide.".to_owned(),
    ] {
        let report = run(prose);
        assert!(
            matches!(report.outcome, Outcome::Failed(Failure::InvalidResult(_))),
            "{:?}",
            report.outcome
        );
        assert!(!bench.worktree.join(".owlshift/run/result.json").exists());
    }
}

/// OWL-323: a result read from the last message goes through the secret
/// check, and is never written through a link.
#[cfg(unix)]
#[test]
fn a_result_from_the_last_message_holds_no_secret_and_follows_no_link() {
    let bench = Bench::new();
    let read_only = bench.read_only_brief();
    let message = blocked("from the message");

    let token = "sk-ant-oat01-owl323-FAKE";
    let harness = EndsOn {
        script: "true".into(),
        message: blocked(token),
        login: Some(Secret::new(token)),
        in_message: false,
    };
    let report = bench
        .run_with(&bench.executor, &harness, &read_only)
        .unwrap();
    assert!(
        matches!(
            &report.outcome,
            Outcome::Failed(Failure::Credentials(findings))
                if matches!(findings[..], [CredentialFinding::LoginCopied { .. }])
        ),
        "{:?}",
        report.outcome
    );

    // A link left at the result's place is not written over, and refused.
    let outside = bench.tmp.path().join("outside");
    fs::create_dir(&outside).unwrap();
    let planted = format!(
        "ln -s '{}' .owlshift/run/result.json",
        outside.join("planted.json").display()
    );
    let report = bench.ends_on(&read_only, &planted, &message);
    assert!(
        matches!(report.outcome, Outcome::Failed(Failure::InvalidResult(_))),
        "{:?}",
        report.outcome
    );
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);

    // The run's folder made a link: nothing is written through it.
    let linked = format!(
        "rm -rf .owlshift/run && ln -s '{}' .owlshift/run",
        outside.display()
    );
    let report = bench.ends_on(&read_only, &linked, &message);
    assert!(
        matches!(report.outcome, Outcome::Failed(_)),
        "{:?}",
        report.outcome
    );
    assert!(
        report.warnings.len() == 1 && report.warnings[0].contains("could not be written there"),
        "{:?}",
        report.warnings
    );
    assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
}

/// The `sh` command that points the worktree's `.git` at `evil`: the
/// harness script's.
fn redirect(evil: &Path) -> String {
    format!("printf 'gitdir: %s\\n' '{}' > .git", evil.display())
}

/// The same, as a gate command: the gate runs through `cmd` on Windows,
/// whose redirection cannot overwrite the hidden file git makes of `.git`
/// there, so the file is unhidden first.
fn redirect_in_gate(evil: &Path) -> String {
    if cfg!(windows) {
        format!("attrib -h .git && echo gitdir: {}> .git", evil.display())
    } else {
        redirect(evil)
    }
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
        let evil_link = format!("gitdir: {}", evil.display());
        let executor = Executor {
            git: Git::with_setup("git", move |command| {
                runner.apply(command);
                if fs::read_to_string(&link).is_ok_and(|text| text.starts_with(&evil_link)) {
                    seen.lock().unwrap().push(format!("{command:?}"));
                }
            }),
            ..bench.executor.clone()
        };
        let done = r#"{"format":14,"status":"done","summary":"s","pr":{"branch":"owlshift/T-1","title":"t","body":"b"}}"#;
        let write_result = format!("printf '%s' '{done}' > .owlshift/run/result.json");
        let mut brief = bench.brief.clone();
        let script = if case == "the harness" {
            format!("{} && {write_result}", redirect(&evil))
        } else {
            brief.gate = vec![redirect_in_gate(&evil)];
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

/// The build role's `done`, as a prepared result.
const DONE: &str = r#"{"format":14,"status":"done","summary":"s","pr":{"branch":"owlshift/T-1","title":"t","body":"b"}}"#;

impl Bench {
    /// The fake harness playing `reply`, its prepared result `DONE`.
    fn fake(&self, mut reply: Reply) -> FakeHarness {
        let replies = self.tmp.path().join("replies");
        fs::create_dir_all(&replies).unwrap();
        let done = replies.join("done.json");
        fs::write(&done, DONE).unwrap();
        reply.result = Some(done);
        let path = replies.join("reply.toml");
        fs::write(&path, reply.render()).unwrap();
        FakeHarness {
            program: PathBuf::from(env!("CARGO_BIN_EXE_owlshift-fake-harness")),
            reply: path,
            readable: Vec::new(),
            sessions: false,
        }
    }

    /// What the fake harness observed in the last run.
    fn observed(&self) -> String {
        fs::read_to_string(self.worktree.join(".owlshift/run/observed.txt")).unwrap()
    }
}

/// OWL-330: the fake harness writes what it observes, a committed file, a
/// missing one and a missing file of its temporary folder, in order, into
/// the run's own folder, which git ignores, so the gate after its `done`
/// finds the worktree clean.
#[test]
fn the_fake_harness_observes_the_worktree_and_its_temporary_folder() {
    let bench = Bench::new();
    let harness = bench.fake(Reply {
        observe: vec![
            "README.md".to_owned(),
            "missing.txt".to_owned(),
            "$TMPDIR/none".to_owned(),
        ],
        observe_into: Some(RelativePath::new(".owlshift/run/observed.txt").unwrap()),
        ..Reply::default()
    });
    let mut brief = bench.brief.clone();
    brief.gate = vec!["git status --porcelain".to_owned()];
    let report = bench.run_with(&bench.executor, &harness, &brief).unwrap();
    assert!(
        matches!(report.outcome, Outcome::Finished { .. }),
        "{:?}",
        report.outcome
    );
    assert!(
        report.gate.as_ref().is_some_and(|gate| gate.passed()),
        "{report:?}"
    );
    assert_eq!(
        bench.observed(),
        "README.md=hello\nmissing.txt absent\n$TMPDIR/none absent\n"
    );
}

/// OWL-394: a resume whose transcript cannot be put in place starts a
/// session of the run's own, said in a warning, and its run counts at the
/// whole cost it reports, never less what the other session had cost; a
/// session given to a harness that keeps none is refused before anything
/// runs.
#[test]
fn a_resume_without_its_transcript_starts_the_runs_own_session() {
    let bench = Bench::new();
    let mut harness = bench.fake(Reply {
        cost_micros: Some(300_000),
        ..Reply::default()
    });
    let own = SessionId::new("11111111-1111-4111-8111-111111111111").unwrap();
    let plan = SessionPlan {
        own: own.clone(),
        resume: Some(ResumeFrom {
            id: SessionId::new("22222222-2222-4222-8222-222222222222").unwrap(),
            folder: bench.tmp.path().join("no-such-session"),
            cost_micros: 5_000_000,
        }),
    };
    let run = |harness: &FakeHarness| {
        let spec = RunSpec {
            main: &bench.main,
            worktree: &bench.worktree,
            branch: BRANCH,
            base: "origin/main",
            run_dir: &bench.tmp.path().join("run"),
            brief: &bench.brief,
            setup: &[],
            session: Some(&plan),
            siblings: None,
        };
        bench.executor.run(&spec, harness)
    };
    assert!(matches!(run(&harness), Err(ExecutorError::Spec(_))));

    harness.sessions = true;
    let report = run(&harness).unwrap();
    assert_eq!(report.resumed, Some(false));
    assert!(
        report
            .warnings
            .iter()
            .any(|warning| warning.contains("could not be resumed")
                && warning.contains("starts a session of its own")),
        "{:?}",
        report.warnings
    );
    assert_eq!(report.usage.and_then(|usage| usage.cost_usd), Some(0.3));
    // A `done` keeps no transcript: only a run that asked does.
    assert_eq!(report.session, None);
}

/// OWL-369: a run that amends a commit its branch held at the start is
/// quarantined, its valid result notwithstanding, and the breach names the
/// rewrite.
#[test]
fn a_run_that_rewrites_its_start_is_quarantined_and_named() {
    let bench = Bench::new();
    bench.kept();
    fs::write(bench.worktree.join("GREETING.md"), "hi\n").unwrap();
    bench
        .env
        .run(&bench.worktree, &["add", "GREETING.md"])
        .unwrap();
    bench
        .env
        .run(&bench.worktree, &["commit", "--quiet", "-m", "Mine"])
        .unwrap();
    let start = bench.head_of(&bench.worktree);
    let done = r#"{"format":14,"status":"done","summary":"s","pr":{"branch":"owlshift/T-1","title":"t","body":"b"}}"#;
    let script = format!(
        "git commit --quiet --amend -m 'Mine, again' && printf '%s' '{done}' > .owlshift/run/result.json"
    );
    let report = bench
        .run_with(&bench.executor, &Script(script), &bench.brief)
        .unwrap();
    let end = bench.head_of(&bench.worktree);
    assert_ne!(end, start);
    match &report.outcome {
        Outcome::Quarantined(violations) => {
            assert_eq!(
                violations.as_slice(),
                [Violation::History {
                    start: start.clone(),
                    end: end.clone(),
                    onto: None,
                    rewritten: true,
                }]
            );
            assert!(
                violations[0]
                    .to_string()
                    .ends_with(": the run rewrote its branch's history (rebase, amend or reset)"),
                "{}",
                violations[0]
            );
        }
        other => panic!("{other:?}"),
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

impl Bench {
    /// A commit on the main checkout's `main` writing `files`: the base a
    /// later dispatch fetched. Returns its full id.
    fn new_base(&self, files: &[(&str, &str)]) -> String {
        for (path, text) in files {
            let path = self.main.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }
        let mut add = vec!["add", "--force", "--"];
        add.extend(files.iter().map(|(path, _)| *path));
        self.env.run(&self.main, &add).unwrap();
        self.env
            .run(&self.main, &["commit", "--quiet", "-m", "A merge"])
            .unwrap();
        self.commit("HEAD")
    }

    /// The ticket's worktree and branch made at `origin/main` by an earlier
    /// run, such as its intake; returns the branch's head.
    fn kept(&self) -> String {
        let error = self.run();
        assert!(matches!(error, ExecutorError::Command(_)), "{error:?}");
        self.head_of(&self.worktree)
    }

    /// The dispatch's start of the ticket's branch from `base`.
    fn start(&self, base: &str) -> Result<(), ExecutorError> {
        self.executor
            .start_from_base(&self.main, &self.worktree, BRANCH, base)
    }

    fn branch_head(&self) -> String {
        self.commit(&format!("refs/heads/{BRANCH}"))
    }

    fn read(&self, path: &str) -> String {
        fs::read_to_string(self.worktree.join(path)).unwrap()
    }
}

/// A kept branch with no commits of its own moves to the base fetched at
/// the dispatch, its worktree's files with it (OWL-274): an edit the base
/// does not touch, an untracked file and the ignored plan under the run
/// folder stay as they were. Started again, nothing moves.
#[test]
fn a_kept_branch_with_no_commits_of_its_own_moves_to_the_base() {
    let bench = Bench::new();
    bench.kept();
    fs::write(bench.worktree.join("README.md"), "mine\n").unwrap();
    fs::write(bench.worktree.join("mine.txt"), "untracked\n").unwrap();
    fs::write(bench.worktree.join(".owlshift/run/plan.md"), "1. Greet.\n").unwrap();
    let base = bench.new_base(&[("MERGED.md", "merged\n")]);

    for _ in 0..2 {
        bench.start(&base).unwrap();
        assert_eq!(bench.branch_head(), base);
        assert_eq!(bench.head_of(&bench.worktree), base);
        assert_eq!(bench.read("MERGED.md"), "merged\n");
        assert_eq!(bench.read("README.md"), "mine\n");
        assert_eq!(bench.read("mine.txt"), "untracked\n");
        assert_eq!(bench.read(".owlshift/run/plan.md"), "1. Greet.\n");
    }
}

/// A kept branch with commits of its own is kept as it is: on an older base
/// than the dispatch's, and ahead of it (OWL-274).
#[test]
fn a_kept_branch_with_commits_of_its_own_is_kept() {
    let bench = Bench::new();
    let older = bench.kept();
    bench
        .env
        .run(
            &bench.worktree,
            &["commit", "--quiet", "--allow-empty", "-m", "Mine"],
        )
        .unwrap();
    let own = bench.head_of(&bench.worktree);
    let base = bench.new_base(&[("MERGED.md", "merged\n")]);
    for base in [base.as_str(), older.as_str()] {
        bench.start(base).unwrap();
        assert_eq!(bench.branch_head(), own, "{base}");
        assert_eq!(bench.head_of(&bench.worktree), own, "{base}");
    }
}

/// A fast-forward that would overwrite a local change moves nothing and
/// names the file (OWL-274): a tracked edit, an untracked file and an
/// ignored one under the run folder, each where the base writes.
#[test]
fn a_start_that_would_overwrite_a_local_change_moves_nothing() {
    for path in ["README.md", "MERGED.md", ".owlshift/run/plan.md"] {
        let bench = Bench::new();
        let head = bench.kept();
        fs::write(bench.worktree.join(path), "mine\n").unwrap();
        let base = bench.new_base(&[(path, "merged\n")]);
        let error = bench.start(&base).expect_err(path);
        assert!(
            matches!(&error, ExecutorError::Worktree(reason) if reason.contains(path)),
            "{path}: {error:?}"
        );
        assert_eq!(bench.branch_head(), head, "{path}");
        assert_eq!(bench.head_of(&bench.worktree), head, "{path}");
        assert_eq!(bench.read(path), "mine\n", "{path}");
    }
}

/// Without a worktree, a missing branch is left for the run to create, and
/// a kept one moves to the base alone (OWL-274).
#[test]
fn a_branch_without_a_worktree_moves_and_a_missing_one_is_left() {
    let bench = Bench::new();
    let base = bench.new_base(&[("MERGED.md", "merged\n")]);
    let branch = format!("refs/heads/{BRANCH}");
    bench.start(&base).unwrap();
    let missing = bench
        .env
        .run(&bench.main, &["show-ref", "--verify", "--quiet", &branch]);
    assert!(missing.is_err());

    bench
        .env
        .run(&bench.main, &["branch", BRANCH, "origin/main"])
        .unwrap();
    bench.start(&base).unwrap();
    assert_eq!(bench.branch_head(), base);
    assert!(!bench.worktree.exists());
}

/// OWL-330: the project's setup before a Build's harness and in its gate.
mod setup {
    use super::*;

    /// A harness that leaves a mark in the run's own files, out of git, and
    /// no result.
    const MARKS: &str = "echo ran > .owlshift/run/harness-ran";

    /// `unix`, or `windows` where the setup and the gate run through `cmd`,
    /// which expands `%TMPDIR%` and not `$TMPDIR`. A line both shells run
    /// alike needs neither: `&&` rather than `;`.
    fn by_shell(unix: &'static str, windows: &'static str) -> &'static str {
        if cfg!(windows) { windows } else { unix }
    }

    /// `text` with the line ends `cmd`'s `echo` writes made `\n`.
    fn lf(text: String) -> String {
        text.replace("\r\n", "\n")
    }

    impl Bench {
        /// A run of `harness` with `setup`, from `base`.
        fn run_set_up(
            &self,
            executor: &Executor,
            harness: &dyn Harness,
            brief: &Brief,
            setup: &[&str],
            base: &str,
        ) -> RunReport {
            let setup: Vec<String> = setup.iter().map(|line| (*line).to_owned()).collect();
            let spec = RunSpec {
                main: &self.main,
                worktree: &self.worktree,
                branch: BRANCH,
                base,
                run_dir: &self.tmp.path().join("run"),
                brief,
                setup: &setup,
                session: None,
                siblings: None,
            };
            executor.run(&spec, harness).unwrap()
        }

        /// A Build run of the marking harness with `setup`.
        fn build_set_up(&self, setup: &[&str], base: &str) -> RunReport {
            self.run_set_up(
                &self.executor,
                &Script(MARKS.to_owned()),
                &self.brief,
                setup,
                base,
            )
        }

        fn harness_ran(&self) -> bool {
            self.worktree.join(".owlshift/run/harness-ran").exists()
        }

        fn worktree_brief(&self) -> String {
            fs::read_to_string(self.worktree.join(".owlshift/run/brief.json")).unwrap()
        }

        /// A commit of the branch's own in the worktree, writing `files`.
        fn own_commit(&self, files: &[(&str, &str)]) {
            for (path, text) in files {
                fs::write(self.worktree.join(path), text).unwrap();
            }
            let mut add = vec!["add", "--"];
            add.extend(files.iter().map(|(path, _)| *path));
            self.env.run(&self.worktree, &add).unwrap();
            self.env
                .run(
                    &self.worktree,
                    &["commit", "--quiet", "--allow-empty", "-m", "own"],
                )
                .unwrap();
        }

        /// `origin/main` moved to a new commit, as a fetch moves it.
        fn origin_ahead(&self) -> String {
            let ahead = self.new_base(&[("other.txt", "other\n")]);
            self.env
                .run(
                    &self.main,
                    &["update-ref", "refs/remotes/origin/main", &ahead],
                )
                .unwrap();
            ahead
        }
    }

    /// The setup failed and the harness never started.
    fn failed_before_the_harness(bench: &Bench, report: &RunReport) -> GateFailure {
        let Outcome::Failed(Failure::Setup(failure)) = &report.outcome else {
            panic!("{:?}", report.outcome);
        };
        assert!(!bench.harness_ran(), "the harness ran");
        assert!(report.exit_code.is_none() && report.usage.is_none());
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
        let brief = bench.worktree_brief();
        assert!(!brief.contains("gate_failure") && !brief.contains("setup_failure"));
        (**failure).clone()
    }

    /// A red setup on a worktree with no work of the Build's own ends the
    /// run before the harness, whether the base is a full id or
    /// `origin/main`, and whether the kept branch sits at the base or at an
    /// older one the remote has moved past; once the branch holds a commit
    /// of its own, the same setup lets the harness start, told.
    #[test]
    fn a_red_setup_fails_a_pristine_worktree_and_tells_a_resumed_build() {
        for by_name in [false, true] {
            let bench = Bench::new();
            let base = |bench: &Bench| {
                if by_name {
                    "origin/main".to_owned()
                } else {
                    bench.commit("refs/remotes/origin/main")
                }
            };
            let report = bench.build_set_up(&["echo partial && exit 1"], &base(&bench));
            let failure = failed_before_the_harness(&bench, &report);
            assert_eq!(failure.command.as_deref(), Some("echo partial && exit 1"));
            assert_eq!(failure.reason, "setup: exit status 1");
            assert!(failure.output.contains("partial"));
            assert!(bench.tmp.path().join("run").join(SETUP_LOG).exists());
            assert_eq!(report.setup.as_ref().unwrap().setup.len(), 1);

            // The kept branch, with no commit of its own, behind the base.
            bench.origin_ahead();
            let report = bench.build_set_up(&["exit 1"], &base(&bench));
            failed_before_the_harness(&bench, &report);

            bench.own_commit(&[("own.txt", "own\n")]);
            let report = bench.build_set_up(&["exit 1"], &base(&bench));
            assert!(bench.harness_ran(), "{report:?}");
            assert_eq!(
                report.warnings,
                ["the project setup failed before the harness: exit 1"]
            );
            let told = Brief::parse(&bench.worktree_brief()).unwrap();
            assert_eq!(
                told.setup_failure.map(|f| f.reason).as_deref(),
                Some("setup: exit status 1")
            );
            assert!(told.gate_failure.is_none());
        }
    }

    /// A Build fixing the runner's gate failure whose setup then fails on
    /// its own work is told both (OWL-389): the setup's failure beside the
    /// gate's, which it no longer replaces; only an earlier failure of the
    /// same setup command is left out, the fresh one telling it again.
    #[test]
    fn a_build_fixing_a_gate_failure_is_told_its_setup_failure_beside_it() {
        let bench = Bench::new();
        let base = bench.commit("refs/remotes/origin/main");
        bench.kept();
        bench.own_commit(&[("own.txt", "own\n")]);
        let failure = |command: &str, reason: &str| GateFailure {
            command: Some(command.to_owned()),
            reason: reason.to_owned(),
            output: "earlier output\n".to_owned(),
            truncated: false,
        };
        let tests_failed = failure("cargo test", "exit status 101");
        let other_setup = failure("sh other.sh", "setup: exit status 2");
        for (earlier, kept) in [
            (tests_failed.clone(), Some(tests_failed)),
            (other_setup.clone(), Some(other_setup)),
            (failure("exit 1", "setup: exit status 1"), None),
        ] {
            let mut brief = bench.brief.clone();
            brief.gate_failure = Some(earlier);
            let report = bench.run_set_up(
                &bench.executor,
                &Script(MARKS.to_owned()),
                &brief,
                &["exit 1"],
                &base,
            );
            assert!(bench.harness_ran(), "{report:?}");
            let told = Brief::parse(&bench.worktree_brief()).unwrap();
            let setup = told
                .setup_failure
                .clone()
                .expect("the setup's failure is told");
            assert_eq!(
                (setup.command.as_deref(), setup.reason.as_str()),
                (Some("exit 1"), "setup: exit status 1")
            );
            assert_eq!(told.gate_failure, kept);
            let handed = fs::read_to_string(bench.tmp.path().join("run/brief.json")).unwrap();
            assert_eq!(Brief::parse(&handed).unwrap(), told);
        }
    }

    /// A setup that breaks isolation quarantines the run before the harness,
    /// whatever its exit status: a branch made, a hook written.
    #[test]
    fn a_setup_that_breaks_isolation_is_quarantined_before_the_harness() {
        for setup in [
            "git branch other && exit 1",
            "git branch other",
            by_shell(
                "echo 'touch hooked' > \"$(git rev-parse --git-common-dir)/hooks/post-checkout\"",
                // The main checkout's, beside the worktree.
                "echo touch hooked> ..\\checkout\\.git\\hooks\\post-checkout",
            ),
        ] {
            let bench = Bench::new();
            let base = bench.commit("refs/remotes/origin/main");
            let report = bench.build_set_up(&[setup], &base);
            assert!(
                matches!(report.outcome, Outcome::Quarantined(_)),
                "{setup}: {:?}",
                report.outcome
            );
            assert!(!bench.harness_ran(), "{setup}");
        }
    }

    /// What the setup leaves in the run's temporary folder is the
    /// harness's and the gate's, and the gate runs the setup again.
    #[test]
    fn the_setup_shares_the_runs_temporary_folder_and_runs_again_in_the_gate() {
        let bench = Bench::new();
        let base = bench.commit("refs/remotes/origin/main");
        let harness = bench.fake(Reply {
            observe: vec!["$TMPDIR/x".to_owned(), ".owlshift/run/count".to_owned()],
            observe_into: Some(RelativePath::new(".owlshift/run/observed.txt").unwrap()),
            ..Reply::default()
        });
        let mut brief = bench.brief.clone();
        brief.gate = vec![
            by_shell(
                "test \"$(cat \"$TMPDIR/x\")\" = set",
                "findstr set \"%TMPDIR%\\x\"",
            )
            .to_owned(),
        ];
        let report = bench.run_set_up(
            &bench.executor,
            &harness,
            &brief,
            &[
                by_shell("echo set > \"$TMPDIR/x\"", "echo set> \"%TMPDIR%\\x\""),
                "echo ran>> .owlshift/run/count",
            ],
            &base,
        );
        assert!(
            matches!(report.outcome, Outcome::Finished { .. }),
            "{:?}",
            report.outcome
        );
        assert!(report.setup.as_ref().is_some_and(GateReport::passed));
        let gate = report.gate.as_ref().unwrap();
        assert!(gate.passed(), "{gate:?}");
        assert_eq!(gate.setup.len(), 2);
        assert_eq!(
            lf(bench.observed()),
            "$TMPDIR/x=set\n.owlshift/run/count=ran\n"
        );
        assert_eq!(
            lf(fs::read_to_string(bench.worktree.join(".owlshift/run/count")).unwrap()),
            "ran\nran\n"
        );
    }

    /// A resumed Build's unfinished work, tracked, staged and untracked,
    /// comes out of a setup as it went in, and its harness starts.
    #[test]
    fn a_resumed_builds_unfinished_work_survives_the_setup() {
        let bench = Bench::new();
        let base = bench.commit("refs/remotes/origin/main");
        bench.kept();
        fs::write(bench.worktree.join("README.md"), "edited\n").unwrap();
        fs::write(bench.worktree.join("staged.txt"), "staged\n").unwrap();
        bench
            .env
            .run(&bench.worktree, &["add", "staged.txt"])
            .unwrap();
        fs::write(bench.worktree.join("new.txt"), "new\n").unwrap();
        let setup = by_shell(
            "echo set up > \"$TMPDIR/x\"",
            "echo set up> \"%TMPDIR%\\x\"",
        );
        let report = bench.build_set_up(&[setup], &base);
        assert!(bench.harness_ran(), "{report:?}");
        assert!(report.setup.as_ref().is_some_and(GateReport::passed));
        let read = |path: &str| fs::read_to_string(bench.worktree.join(path)).unwrap();
        assert_eq!(
            (read("README.md"), read("staged.txt"), read("new.txt")),
            ("edited\n".into(), "staged\n".into(), "new\n".into())
        );
    }

    /// A Build that broke its setup script in an earlier run is told by its
    /// brief, repairs it and passes the gate; a script stopped at the
    /// setup's deadline, a third of the gate's and not of the run's, is
    /// told the same way.
    #[test]
    fn a_build_that_broke_its_setup_repairs_it() {
        for (script, said) in [
            ("exit 1\n", "setup: exit status 1"),
            (
                "sleep 20\n",
                "setup: stopped at the setup's deadline, a third of the gate's, 1 s",
            ),
        ] {
            let bench = Bench::new();
            let base = bench.commit("refs/remotes/origin/main");
            bench.kept();
            bench.own_commit(&[("setup.sh", script)]);
            let executor = Executor {
                gate_timeout: Duration::from_secs(3),
                ..bench.executor.clone()
            };
            let mut files = std::collections::BTreeMap::new();
            files.insert(RelativePath::new("setup.sh").unwrap(), "true\n".to_owned());
            let harness = bench.fake(Reply {
                observe: vec![".owlshift/run/brief.json".to_owned()],
                observe_into: Some(RelativePath::new(".owlshift/run/observed.txt").unwrap()),
                files,
                commit: Some("Repair the setup".to_owned()),
                ..Reply::default()
            });
            let mut brief = bench.brief.clone();
            brief.gate = vec!["true".to_owned()];
            let report = bench.run_set_up(&executor, &harness, &brief, &["sh setup.sh"], &base);
            assert!(
                matches!(report.outcome, Outcome::Finished { .. }),
                "{script}: {:?}",
                report.outcome
            );
            assert_eq!(
                report.warnings,
                ["the project setup failed before the harness: sh setup.sh"]
            );
            assert!(bench.observed().contains(said), "{}", bench.observed());
            let gate = report.gate.as_ref().unwrap();
            assert!(gate.passed() && gate.setup.len() == 1, "{gate:?}");
        }
    }

    /// A setup that fails for something other than its command's own
    /// failure fails a resumed Build too: one that changes the work, one
    /// whose log cannot be written, one whose process kept the output open
    /// past the setup's deadline, a third of the gate's 6 s. The last leaves
    /// its process group with `setpgrp`, which Windows has not.
    #[test]
    fn a_setup_failure_a_build_cannot_repair_fails_the_run() {
        let cases: &[&str] = if cfg!(unix) {
            &["changes", "log", "holds"]
        } else {
            &["changes", "log"]
        };
        for &case in cases {
            let bench = Bench::new();
            let base = bench.commit("refs/remotes/origin/main");
            bench.kept();
            bench.own_commit(&[("own.txt", "own\n")]);
            let setup = match case {
                "changes" => "echo changed > README.md && exit 1",
                "log" => {
                    fs::create_dir_all(bench.tmp.path().join("run").join(SETUP_LOG)).unwrap();
                    "exit 1"
                }
                // Out of the tree's group before the shell ends, so stopping
                // the tree does not stop it.
                _ => "perl -e 'setpgrp(0, 0); sleep 8' & sleep 0.5; exit 1",
            };
            let executor = Executor {
                gate_timeout: Duration::from_secs(6),
                ..bench.executor.clone()
            };
            let harness = Script(MARKS.to_owned());
            let report = bench.run_set_up(&executor, &harness, &bench.brief, &[setup], &base);
            let failure = failed_before_the_harness(&bench, &report);
            match case {
                "changes" => assert_eq!(
                    failure.reason,
                    "the setup changed unfinished work: README.md"
                ),
                "log" => assert!(
                    failure
                        .reason
                        .starts_with("the runner could not write the setup log"),
                    "{failure:?}"
                ),
                _ => assert_eq!(failure.reason, "setup: exit status 1"),
            }
        }
    }

    /// A rebase runs no setup before its harness, but its gate does; a
    /// role other than Build or rebase runs none at all.
    #[test]
    fn only_a_builds_harness_waits_for_the_setup() {
        let bench = Bench::new();
        let onto = bench.commit("refs/remotes/origin/main");
        let mut brief = bench.brief.clone();
        brief.role = Role::Rebase;
        brief.gate = vec!["true".to_owned()];
        brief.rebase = Some(RebaseBrief {
            pull_request: 1,
            url: "https://github.com/demo/project/pull/1".into(),
            branch: BRANCH.into(),
            head: onto.clone(),
            base_branch: "main".into(),
            onto: onto.clone(),
            refresh: false,
            conflicting: Vec::new(),
        });
        let harness = bench.fake(Reply {
            observe: vec![".owlshift/run/count".to_owned()],
            observe_into: Some(RelativePath::new(".owlshift/run/observed.txt").unwrap()),
            ..Reply::default()
        });
        let count = ["echo ran>> .owlshift/run/count"];
        let report = bench.run_set_up(&bench.executor, &harness, &brief, &count, &onto);
        assert!(
            matches!(report.outcome, Outcome::Finished { .. }),
            "{:?}",
            report.outcome
        );
        assert!(report.setup.is_none());
        assert_eq!(bench.observed(), ".owlshift/run/count absent\n");
        assert!(report.gate.as_ref().is_some_and(GateReport::passed));
        assert_eq!(
            lf(fs::read_to_string(bench.worktree.join(".owlshift/run/count")).unwrap()),
            "ran\n"
        );

        let bench = Bench::new();
        let base = bench.commit("refs/remotes/origin/main");
        let brief = bench.read_only_brief();
        let report = bench.run_set_up(
            &bench.executor,
            &Script(MARKS.to_owned()),
            &brief,
            &count,
            &base,
        );
        assert!(report.setup.is_none() && report.gate.is_none());
        assert!(!bench.worktree.join(".owlshift/run/count").exists());
    }
}

/// Two runs at once through the executor (OWL-400; architecture section 8,
/// "Isolation among steps"): each ticket's check is told the other from
/// its run file and its lock, as `owlshift do` tells it.
mod among {
    use std::sync::Barrier;
    use std::thread;
    use std::time::Instant;

    use owlshift_adapters::forge::Repo;
    use owlshift_runner::on_demand::branch_for;
    use owlshift_runner::project::{ProjectDirs, TicketLock};
    use owlshift_runner::ticket_ref::{self, ProposalsFile, Unguarded};
    use owlshift_runner::ticket_run::{self, Siblings};

    use super::*;

    /// A harness whose run is `during`, called with the worktree while its
    /// process (`git --version`) runs; it leaves no result.
    struct During<F>(F);

    impl<F: Fn(&Path)> Harness for During<F> {
        fn command(&self, _run: &HarnessRun<'_>) -> Result<Command, HarnessError> {
            let mut command = Command::new("git");
            command.arg("--version");
            Ok(command)
        }

        fn drive(
            &self,
            run: &HarnessRun<'_>,
            child: &mut Child,
            log: &mut RunLog,
        ) -> io::Result<HarnessEnd> {
            (self.0)(run.worktree);
            let ended = drive_plain(child, log)?;
            Ok(HarnessEnd::new(ended.exit_code, HarnessStatus::Completed))
        }
    }

    /// The executor's bench with a project folder for the run files, and
    /// two tickets, each with its worktree and its ticket ref.
    struct Two {
        bench: Bench,
        dirs: ProjectDirs,
        a: TicketId,
        b: TicketId,
    }

    impl Two {
        fn new() -> Self {
            let bench = Bench::new();
            let dirs = ProjectDirs::new(
                &bench.tmp.path().join("data"),
                &Repo::parse("acme/app").unwrap(),
            );
            let two = Self {
                bench,
                dirs,
                a: TicketId::new("T-1").unwrap(),
                b: TicketId::new("T-2").unwrap(),
            };
            let base = two.at("origin/main");
            for ticket in [&two.a, &two.b] {
                let worktree = two.worktree(ticket);
                two.bench
                    .env
                    .run(
                        &two.bench.main,
                        &[
                            "worktree",
                            "add",
                            "--quiet",
                            "-b",
                            &branch_for(ticket),
                            worktree.to_str().unwrap(),
                            &base,
                        ],
                    )
                    .unwrap();
                ticket_ref::write_proposals_guarded(
                    &two.bench.executor.git,
                    &two.bench.main,
                    ticket,
                    &ticket_ref::blank(None),
                    ProposalsFile::Keep,
                    None,
                    &mut Unguarded,
                )
                .unwrap();
                let lock = two.lock(ticket);
                two.open(&lock, ticket);
            }
            two
        }

        fn worktree(&self, ticket: &TicketId) -> PathBuf {
            self.bench
                .tmp
                .path()
                .join(format!("wt-{}", ticket.as_str().to_lowercase()))
        }

        fn at(&self, rev: &str) -> String {
            let out = self
                .bench
                .env
                .run(&self.bench.main, &["rev-parse", rev])
                .unwrap();
            String::from_utf8(out).unwrap().trim().to_owned()
        }

        fn lock(&self, ticket: &TicketId) -> TicketLock {
            self.dirs
                .ticket_lock(ticket)
                .unwrap()
                .expect("the ticket's lock")
        }

        /// The start of a step: the run file checked, or made.
        fn open(&self, lock: &TicketLock, ticket: &TicketId) {
            let entry = ticket_run::entry_of(&self.bench.main, &self.worktree(ticket));
            ticket_run::open(
                &self.bench.executor.git,
                &self.dirs,
                &self.bench.main,
                lock,
                ticket,
                entry.as_deref(),
            )
            .unwrap();
        }

        /// A run of `ticket`, under its `lock`, through the executor, told of
        /// the other ticket; recorded in its run file after, as
        /// `owlshift do` records it.
        fn run(&self, lock: &TicketLock, ticket: &TicketId, during: &dyn Fn(&Path)) -> RunReport {
            let git = &self.bench.executor.git;
            ticket_run::run_started(git, &self.dirs, &self.bench.main, lock, ticket).unwrap();
            let siblings = Siblings::new(&self.dirs, ticket);
            let worktree = self.worktree(ticket);
            let branch = branch_for(ticket);
            let run_dir = self
                .bench
                .tmp
                .path()
                .join(format!("run-{}", ticket.as_str().to_lowercase()));
            let spec = RunSpec {
                main: &self.bench.main,
                worktree: &worktree,
                branch: &branch,
                base: "origin/main",
                run_dir: &run_dir,
                brief: &self.bench.brief,
                setup: &[],
                session: None,
                siblings: Some(&siblings),
            };
            let report = self.bench.executor.run(&spec, &During(during)).unwrap();
            ticket_run::run_ended(git, &self.dirs, &self.bench.main, lock, ticket, &worktree)
                .unwrap();
            report
        }

        /// A write of `ticket`'s ref through its run file, as the runner
        /// makes between its runs.
        fn write_ref(&self, lock: &TicketLock, ticket: &TicketId) {
            let git = &self.bench.executor.git;
            let previous = ticket_ref::read(git, &self.bench.main, ticket)
                .unwrap()
                .map(|stored| stored.commit);
            let mut record = ticket_ref::blank(None);
            record.state.round += 1;
            ticket_run::write_ref(
                git,
                &self.dirs,
                &self.bench.main,
                lock,
                ticket,
                &record,
                ProposalsFile::Keep,
                previous.as_deref(),
            )
            .unwrap();
        }

        /// Waits for `ticket`'s lock to read free: a child spawned while it
        /// was held can keep it a moment after its release (OWL-414).
        fn until_free(&self, ticket: &TicketId) {
            let started = Instant::now();
            while self.dirs.ticket_lock_held(ticket).unwrap() {
                assert!(started.elapsed() < Duration::from_secs(5), "still held");
                thread::sleep(Duration::from_millis(10));
            }
        }

        fn commit_in(&self, worktree: &Path, message: &str) {
            self.bench
                .env
                .run(
                    worktree,
                    &["commit", "--quiet", "--allow-empty", "-m", message],
                )
                .unwrap();
        }
    }

    fn quarantined(report: &RunReport) -> Vec<Violation> {
        match &report.outcome {
            Outcome::Quarantined(violations) => violations.clone(),
            _ => Vec::new(),
        }
    }

    /// Acceptance: a sibling's ref write and its worktree entry are no
    /// breach. Both tickets run at once, each in a thread of its own; while
    /// both runs are in flight each commits in its own worktree, then B's
    /// runner writes B's ref through its run file, and A's check runs either
    /// while B's command still holds its lock or once B ended; B's check
    /// ran while A's run was in flight. Neither is a breach.
    #[test]
    fn a_siblings_ref_write_and_worktree_entry_are_no_breach() {
        for b_ended in [false, true] {
            let two = Two::new();
            let both_running = Barrier::new(2);
            let b_wrote = Barrier::new(2);
            let a_checked = Barrier::new(2);
            let (a_report, b_report) = thread::scope(|scope| {
                let b = scope.spawn(|| {
                    let lock = two.lock(&two.b);
                    two.open(&lock, &two.b);
                    let report = two.run(&lock, &two.b, &|worktree| {
                        both_running.wait();
                        two.commit_in(worktree, "B's work");
                    });
                    two.write_ref(&lock, &two.b);
                    if b_ended {
                        drop(lock);
                        b_wrote.wait();
                    } else {
                        b_wrote.wait();
                        a_checked.wait();
                    }
                    report
                });
                let lock = two.lock(&two.a);
                two.open(&lock, &two.a);
                let report = two.run(&lock, &two.a, &|worktree| {
                    both_running.wait();
                    two.commit_in(worktree, "A's work");
                    b_wrote.wait();
                    if b_ended {
                        two.until_free(&two.b);
                    }
                });
                if !b_ended {
                    a_checked.wait();
                }
                (report, b.join().unwrap())
            });
            assert_eq!(quarantined(&a_report), [], "b ended: {b_ended}");
            assert_eq!(quarantined(&b_report), [], "b ended: {b_ended}");
        }
    }

    /// A ticket that ended since a run's snapshot is held to what its run
    /// file records: a run that moves its branch, or its ref, after that is
    /// found by the run's check, and at that ticket's next start.
    #[test]
    fn an_ended_siblings_branch_or_ref_moved_by_a_run_is_found() {
        for moved in ["branch", "ticket ref"] {
            let two = Two::new();
            let git = &two.bench.executor.git;
            let lock = two.lock(&two.a);
            let b_branch = format!("refs/heads/{}", branch_for(&two.b));
            let b_ref = owlshift_contracts::refs::ticket_ref(&two.b);
            let report = two.run(&lock, &two.a, &|_| {
                // B's runner writes its ref during A's run, so B ended since
                // A's snapshot; then A's run moves B's branch or ref.
                let b_lock = two.lock(&two.b);
                two.write_ref(&b_lock, &two.b);
                drop(b_lock);
                two.until_free(&two.b);
                let target = if moved == "branch" { &b_branch } else { &b_ref };
                let planted = two
                    .bench
                    .env
                    .run(
                        &two.bench.main,
                        &["commit-tree", "HEAD^{tree}", "-p", "HEAD", "-m", "planted"],
                    )
                    .unwrap();
                let planted = String::from_utf8(planted).unwrap().trim().to_owned();
                two.bench
                    .env
                    .run(&two.bench.main, &["update-ref", target, &planted])
                    .unwrap();
            });
            let expected = if moved == "branch" { &b_branch } else { &b_ref };
            assert!(
                quarantined(&report)
                    .iter()
                    .any(|v| matches!(v, Violation::Refs(refs) if refs.contains(expected))),
                "{moved}: {:?}",
                report.outcome
            );
            drop(lock);
            let b_lock = two.lock(&two.b);
            let entry = ticket_run::entry_of(&two.bench.main, &two.worktree(&two.b));
            let opened = ticket_run::open(
                git,
                &two.dirs,
                &two.bench.main,
                &b_lock,
                &two.b,
                entry.as_deref(),
            );
            assert!(
                matches!(opened, Err(ticket_run::RunFault::Breach(_))),
                "{moved}: {opened:?}"
            );
        }
    }

    /// A sibling's run file that cannot be read, as a run takes its
    /// snapshot, is that run's check failing: a lost record never widens
    /// what a check leaves out.
    #[test]
    fn an_unreadable_sibling_run_file_fails_the_check() {
        let two = Two::new();
        fs::write(two.dirs.run_file(&two.b), "{").unwrap();
        let lock = two.lock(&two.a);
        let report = two.run(&lock, &two.a, &|_| {});
        assert!(
            quarantined(&report)
                .iter()
                .any(|v| matches!(v, Violation::CheckFailed(why) if why.contains("run.json"))),
            "{:?}",
            report.outcome
        );
    }
}
