//! One real critic run on Codex through the executor (OWL-299): the verify
//! role, confined by the read-only profile, on the user's own Codex login,
//! with the adapter's own command line (the prompt on standard input, the
//! strict output schema, `-o`). It spends a little of the user's Codex
//! usage and writes their `auth.json` back when Codex refreshed it, so it
//! never runs in CI and first copies that file to a backup the caller names,
//! readable by the user alone:
//!
//! ```sh
//! OWLSHIFT_LIVE_CODEX=1 OWLSHIFT_CODEX_BACKUP=/path/to/new/backup.json \
//!   cargo test -p owlshift-testkit --test codex_live -- --ignored --nocapture
//! ```
//!
//! With `OWLSHIFT_RECORD_CODEX=1` it also writes the run's stream as the
//! recording `fixtures/codex/critic` of `owlshift-adapters`, scrubbed;
//! `tests/codex_fixtures.rs` then checks it like the others.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use owlshift_adapters::harness::codex::{self, Effort};
use owlshift_contracts::Role;
use owlshift_contracts::brief::{
    Author, Brief, GivenDiff, PermissionLevel, Permissions, Relation, ReviewBrief, TicketBrief,
};
use owlshift_contracts::format::{Format, strip_role_front_matter};
use owlshift_contracts::ids::{RelativePath, TicketId};
use owlshift_contracts::result::Status;
use owlshift_contracts::review::Mandate;
use owlshift_runner::agent_env::AgentEnv;
use owlshift_runner::executor::codex::{codex_login_kept, user_login};
use owlshift_runner::executor::{CodexHarness, Executor, Git, Outcome, RunReport, RunSpec};
use owlshift_runner::roles::VERIFY_ROLE;
use owlshift_testkit::git::{GitEnv, seed};

/// The change under review: a division with no guard against zero.
const CHANGE: &str = "pub fn ratio(done: u32, total: u32) -> u32 {\n    done * 100 / total\n}\n";

const DIFF: &str = "diff --git a/src/lib.rs b/src/lib.rs\n\
new file mode 100644\n\
--- /dev/null\n\
+++ b/src/lib.rs\n\
@@ -0,0 +1,3 @@\n\
+pub fn ratio(done: u32, total: u32) -> u32 {\n\
+    done * 100 / total\n\
+}\n";

#[test]
#[ignore = "live: runs `codex exec` on your Codex login; set OWLSHIFT_LIVE_CODEX=1"]
fn a_real_critic_runs_on_codex_through_the_executor() {
    assert_eq!(
        std::env::var("OWLSHIFT_LIVE_CODEX").as_deref(),
        Ok("1"),
        "this test spends Codex usage: set OWLSHIFT_LIVE_CODEX=1 to run it"
    );
    let backup = PathBuf::from(
        std::env::var_os("OWLSHIFT_CODEX_BACKUP")
            .expect("name a new file for the backup of auth.json in OWLSHIFT_CODEX_BACKUP"),
    );
    let agent = AgentEnv::from_runner(&[], &[]).unwrap();
    let login = user_login(&agent).expect("no CODEX_HOME or HOME");
    let original = fs::read(&login).expect("no Codex login: run `codex login`");
    back_up(&backup, &original);

    let tmp = tempfile::Builder::new()
        .prefix("owlshift-live-codex-")
        .tempdir()
        .unwrap();
    let project = tmp.path().join("project");
    fs::create_dir_all(project.join("src")).unwrap();
    fs::write(project.join("README.md"), "Progress ratios.\n").unwrap();
    fs::write(project.join("src/lib.rs"), CHANGE).unwrap();
    fs::write(project.join("change.diff"), DIFF).unwrap();
    let env = GitEnv::create(tmp.path().join("home")).unwrap();
    let remote = seed(&env, tmp.path(), &project).unwrap();
    let base = String::from_utf8(env.run(&remote.checkout, &["rev-parse", "HEAD"]).unwrap())
        .unwrap()
        .trim()
        .to_owned();
    let runner = env.clone();
    let executor = Executor {
        git: Git::with_setup("git", move |command| runner.apply(command)),
        agent,
        forge_hosts: vec!["github.com".into()],
        timeout: Duration::from_secs(600),
        gate_timeout: Duration::from_secs(60),
    };
    let harness = CodexHarness {
        // Found on the PATH.
        program: "codex".into(),
        prompt: strip_role_front_matter(Role::Verify, VERIFY_ROLE).unwrap(),
        model: None,
        effort: Some(Effort::Low),
        login: login.clone(),
    };
    let brief = brief(base);
    let spec = RunSpec {
        main: &remote.checkout,
        worktree: &tmp.path().join("worktree"),
        branch: "owlshift/LIVE-2",
        base: "origin/main",
        run_dir: &tmp.path().join("run"),
        brief: &brief,
    };

    let report = executor.run(&spec, &harness).unwrap();
    let after = fs::read(&login).unwrap();
    let login_state = if after == original {
        "unchanged"
    } else {
        codex_login_kept(&original, &after)
            .expect("auth.json no longer holds the same ChatGPT login: restore the backup");
        "refreshed and written back"
    };
    eprintln!(
        "outcome: {}\nusage: {:?}\nwarnings: {:?}\nauth.json: {login_state}",
        describe(&report),
        report.usage,
        report.warnings
    );
    if std::env::var("OWLSHIFT_RECORD_CODEX").as_deref() == Ok("1") {
        record(&report, tmp.path());
    }
    let Outcome::Finished { result, .. } = &report.outcome else {
        panic!(
            "{}\nstderr: {}",
            describe(&report),
            fs::read_to_string(&report.stderr_log).unwrap_or_default()
        );
    };
    assert!(
        matches!(result.status, Status::Done | Status::Blocked),
        "{result:?}"
    );
    assert!(report.usage.is_some(), "{report:?}");
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
}

/// Copies the login to `backup`, a new file readable by the user alone.
fn back_up(backup: &Path, login: &[u8]) {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(backup)
        .unwrap_or_else(|e| panic!("{}: {e}", backup.display()));
    file.write_all(login).unwrap();
    file.sync_all().unwrap();
}

/// The outcome, without the result's text.
fn describe(report: &RunReport) -> String {
    match &report.outcome {
        Outcome::Finished { result, .. } => format!(
            "finished, {:?}, {} finding(s): {}",
            result.status,
            result.findings.len(),
            result.summary
        ),
        other => format!("{other:?}"),
    }
}

fn brief(base: String) -> Brief {
    Brief {
        format: Format,
        role: Role::Verify,
        project: "live".into(),
        ticket: TicketBrief {
            id: TicketId::new("LIVE-2").unwrap(),
            title: "Show progress as a percentage".into(),
            url: None,
            labels: Vec::new(),
            author: Author {
                name: "maintainer".into(),
                relation: Relation::Decider,
            },
            description: "Add `ratio(done, total)`, the share of done items as a whole \
                          percentage. A project may have no items yet."
                .into(),
        },
        decider: "maintainer".into(),
        thread: Vec::new(),
        resolve: Vec::new(),
        checkpoint: None,
        zones: Vec::new(),
        resources: Vec::new(),
        rules: Vec::new(),
        permissions: Permissions {
            level: PermissionLevel::ReadOnly,
            network: false,
            browser: false,
        },
        gate: Vec::new(),
        always_human: Vec::new(),
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
        review: Some(ReviewBrief {
            mandates: vec![Mandate::Critic],
            pass: std::num::NonZeroU32::MIN,
            diff: Some(GivenDiff {
                base,
                path: RelativePath::new("change.diff").unwrap(),
            }),
        }),
        pull_request_reviews: Vec::new(),
    }
}

/// Writes the run's stream and its record as the `critic` recording, with
/// the scratch folders, the home folder and the thread id replaced by the
/// placeholders `codex_fixtures.rs` checks for.
fn record(report: &RunReport, scratch: &Path) {
    let home = std::env::var("HOME").unwrap_or_default();
    // The longest first: the scratch folder lies in the temporary one.
    let mut replaced: Vec<(String, String)> = Vec::new();
    for path in [fs::canonicalize(scratch).unwrap(), scratch.to_path_buf()] {
        replaced.push((path.display().to_string(), "/tmp/owlshift-live".into()));
    }
    let temp = std::env::temp_dir();
    for path in [fs::canonicalize(&temp).unwrap(), temp] {
        let path = path.display().to_string();
        replaced.push((path.trim_end_matches('/').to_owned(), "/tmp".into()));
    }
    if !home.is_empty() {
        replaced.push((home, "/home/user".into()));
    }
    let scrub = |text: &str| {
        let mut text = text.to_owned();
        for (from, to) in &replaced {
            text = text.replace(from.as_str(), to);
        }
        text
    };
    let stream: Vec<String> = fs::read_to_string(&report.stdout_log)
        .unwrap()
        .lines()
        .map(|line| {
            let mut event: serde_json::Value = serde_json::from_str(line).unwrap();
            if event.get("thread_id").is_some() {
                event["thread_id"] = "00000000-0000-4000-8000-000000000000".into();
            }
            scrub(&event.to_string())
        })
        .collect();
    let stderr: Vec<String> = fs::read_to_string(&report.stderr_log)
        .unwrap()
        .lines()
        .map(|line| {
            // A log line opens with its timestamp, to the microsecond.
            let line = match line.get(..27) {
                Some(stamp) if stamp.starts_with("20") && stamp.ends_with('Z') => {
                    format!("2026-10-15T12:00:00.000000Z{}", &line[27..])
                }
                _ => line.to_owned(),
            };
            scrub(&line)
        })
        .collect();
    let version = std::process::Command::new("codex")
        .arg("--version")
        .output()
        .unwrap();
    let version = String::from_utf8_lossy(&version.stdout)
        .trim()
        .trim_start_matches("codex-cli ")
        .to_owned();
    // The last agent message, as `-o` wrote it.
    let message = stream
        .iter()
        .rev()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|event| event["item"]["type"] == "agent_message")
        .find_map(|event| event["item"]["text"].as_str().map(str::to_owned));
    // The adapter's command line for this run, with the run's files named
    // by placeholders, so a recording carries the flags of its day.
    let request = codex::Request {
        workdir: PathBuf::from("<worktree>"),
        model: None,
        effort: Some(Effort::Low),
        role: Role::Verify,
        permissions: Permissions {
            level: PermissionLevel::ReadOnly,
            network: false,
            browser: false,
        },
        schema_file: PathBuf::from("<run-files>/schema.json"),
        last_message_file: PathBuf::from("<run-files>/last-message.json"),
    };
    let command = codex::command(Path::new("codex"), &request).unwrap();
    let argv: Vec<String> = std::iter::once("codex".to_owned())
        .chain(
            command
                .get_args()
                .map(|arg| arg.to_string_lossy().into_owned()),
        )
        .collect();
    let meta = serde_json::json!({
        "recorded": jiff::Zoned::now().date().to_string(),
        "codex_version": version,
        "platform": std::env::consts::OS,
        "confinement": "the executor's read-only profile, through Executor::run (OWL-299)",
        "argv": argv,
        "prompt": "the verify role (roles/verify.md) and the brief's path, on standard input",
        "exit_status": report.exit_code,
        "stderr": stderr,
        "output_last_message": message,
    });
    let dir =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../owlshift-adapters/tests/fixtures/codex");
    fs::write(dir.join("critic.jsonl"), stream.join("\n") + "\n").unwrap();
    fs::write(
        dir.join("critic.meta.json"),
        serde_json::to_string_pretty(&meta).unwrap() + "\n",
    )
    .unwrap();
}
