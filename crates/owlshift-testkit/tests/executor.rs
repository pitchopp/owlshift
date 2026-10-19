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
use owlshift_contracts::brief::{
    Author, Brief, PermissionLevel, Permissions, RebaseBrief, Relation, TicketBrief,
};
use owlshift_contracts::format::{Format, RESULT_FORMAT};
use owlshift_contracts::ids::{RelativePath, TicketId};
use owlshift_platform::keychain::Secret;
use owlshift_runner::agent_env::{AgentEnv, CredentialFinding};
use owlshift_runner::executor::harness::drive_plain;
use owlshift_runner::executor::{
    Executor, ExecutorError, Failure, Git, Harness, HarnessEnd, HarnessError, HarnessLogin,
    HarnessRun, HarnessStatus, Outcome, RunLog, RunReport, RunSpec, SandboxNeeds, Violation,
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
/// `login` when given.
struct EndsOn {
    script: String,
    message: String,
    login: Option<Secret>,
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
        let done = r#"{"format":12,"status":"done","summary":"s","pr":{"branch":"owlshift/T-1","title":"t","body":"b"}}"#;
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
    let done = r#"{"format":12,"status":"done","summary":"s","pr":{"branch":"owlshift/T-1","title":"t","body":"b"}}"#;
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
