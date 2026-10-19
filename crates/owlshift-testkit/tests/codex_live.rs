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
//!   cargo test -p owlshift-testkit --test codex_live -- --ignored --nocapture \
//!   --test-threads=1
//! ```
//!
//! One thread, since the tests share the user's login; each makes its own
//! backup, the apps-server check's with `.apps` appended to the name, the
//! tools check's with `.tools`, the sub-agent check's with `.collab` and the
//! depth check's with `.depth`. Two tests run two short critic runs each that
//! list their tools: OWL-132's with and without `--disable apps`, OWL-316's
//! with and without the flags in [`TOOLS_OFF`]. OWL-317's runs one that
//! starts a sub-agent; OWL-320's runs two that start one, with and without
//! [`DEPTH_LIMIT`], and records neither.
//!
//! With `OWLSHIFT_RECORD_CODEX=1` the others also write their runs' streams
//! as the recordings `critic`, `apps_disabled`, `apps_disabled_control`,
//! `critic_tools`, `critic_tools_control` and `critic_collaboration` of
//! `owlshift-adapters`' `fixtures/codex/`, scrubbed; `tests/codex_fixtures.rs`
//! then checks them like the others.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
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

/// A scratch project seeded as the runner seeds one, the executor on the
/// user's own environment, and the user's Codex login, backed up first.
struct Live {
    tmp: tempfile::TempDir,
    main: PathBuf,
    base: String,
    executor: Executor,
    login: PathBuf,
    original: Vec<u8>,
}

impl Live {
    /// Refuses to start without `OWLSHIFT_LIVE_CODEX=1` and a backup path in
    /// `OWLSHIFT_CODEX_BACKUP`, to which `suffix` is appended so that each
    /// test makes a backup of its own.
    fn new(suffix: &str) -> Self {
        assert_eq!(
            std::env::var("OWLSHIFT_LIVE_CODEX").as_deref(),
            Ok("1"),
            "this test spends Codex usage: set OWLSHIFT_LIVE_CODEX=1 to run it"
        );
        let mut backup = std::env::var_os("OWLSHIFT_CODEX_BACKUP")
            .expect("name a new file for the backup of auth.json in OWLSHIFT_CODEX_BACKUP");
        backup.push(suffix);
        let agent = AgentEnv::from_runner(&[], &[], &[]).unwrap();
        let login = user_login(&agent).expect("no CODEX_HOME or HOME");
        let original = fs::read(&login).expect("no Codex login: run `codex login`");
        back_up(Path::new(&backup), &original);

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
        Self {
            tmp,
            main: remote.checkout,
            base,
            executor,
            login,
            original,
        }
    }

    /// The critic on Codex, at effort `low`, with `prompt`.
    fn harness(&self, prompt: String) -> CodexHarness {
        CodexHarness {
            // Found on the PATH.
            program: "codex".into(),
            prompt,
            model: None,
            effort: Some(Effort::Low),
            login: self.login.clone(),
        }
    }

    /// Runs `harness` on the verify brief, on a branch and in a worktree
    /// named after `id`, then checks the user's login: unchanged, or a
    /// refresh of the same ChatGPT login.
    fn run(&self, harness: &CodexHarness, id: &str) -> RunReport {
        let brief = brief(self.base.clone());
        let spec = RunSpec {
            main: &self.main,
            worktree: &self.tmp.path().join(format!("worktree-{id}")),
            branch: &format!("owlshift/{id}"),
            base: "origin/main",
            run_dir: &self.tmp.path().join(format!("run-{id}")),
            brief: &brief,
        };
        let report = self.executor.run(&spec, harness).unwrap();
        let after = fs::read(&self.login).unwrap();
        let login_state = if after == self.original {
            "unchanged"
        } else {
            codex_login_kept(&self.original, &after)
                .expect("auth.json no longer holds the same ChatGPT login: restore the backup");
            "refreshed and written back"
        };
        eprintln!(
            "{id} outcome: {}\nusage: {:?}\nwarnings: {:?}\nauth.json: {login_state}",
            describe(&report),
            report.usage,
            report.warnings
        );
        report
    }

    /// Runs `codex exec` on `prompt` with the adapter's command line, less
    /// each run of arguments in `cut` and with each run in `add` ([`with`]),
    /// outside the executor: through it, the run without `--disable apps`
    /// started its turn and never ended it, confined or not (2026-10-16,
    /// twice each). The run gets the environment of OWL-130's checks
    /// (`HOME`, `PATH`, `USER`, `LANG`) and a `CODEX_HOME` of its own holding
    /// a copy of the user's login, brought back under `codex_login_kept`, as
    /// the executor does.
    fn run_direct(
        &self,
        id: &str,
        cut: &[&[&str]],
        add: &[&[&str]],
        prompt: &str,
    ) -> (codex::Run, Logs) {
        let files = self.tmp.path().join(format!("run-{id}"));
        let home = files.join("codex-home");
        fs::create_dir_all(&home).unwrap();
        let copied = fs::read(&self.login).unwrap();
        fs::write(home.join("auth.json"), &copied).unwrap();
        let schema = files.join("schema.json");
        let rendered = serde_json::to_string_pretty(&codex::review_schema()).unwrap();
        fs::write(&schema, rendered).unwrap();
        let request = codex::Request {
            workdir: self.main.clone(),
            model: None,
            effort: Some(Effort::Low),
            role: Role::Verify,
            permissions: read_only(),
            schema_file: schema,
            last_message_file: files.join("last-message.json"),
        };
        let adapter = codex::command(Path::new("codex"), &request).unwrap();
        let mut args: Vec<String> = adapter
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        for flags in cut {
            args = without(args, flags);
        }
        for flags in add {
            args = with(args, flags);
        }
        let mut command = Command::new("codex");
        command
            .args(&args)
            .current_dir(&self.main)
            .env_clear()
            .env("CODEX_HOME", &home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for name in ["HOME", "PATH", "USER", "LANG"] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        let mut child = command.spawn().unwrap();
        // Stopped after five minutes, as a hung run would be.
        let pid = child.id().to_string();
        let (ended, wait) = std::sync::mpsc::channel::<()>();
        let watchdog = std::thread::spawn(move || {
            if wait.recv_timeout(Duration::from_secs(300)).is_err() {
                let _ = Command::new("kill").arg(&pid).status();
            }
        });
        let mut stdout = Vec::new();
        // No stop at a collaboration call: the live checks watch whole runs
        // (OWL-317, OWL-320).
        let run = codex::drive(
            &mut child,
            prompt,
            |line| {
                stdout.extend_from_slice(line.strip_suffix(b"\n").unwrap_or(line));
                stdout.push(b'\n');
            },
            || {},
        )
        .unwrap();
        let _ = ended.send(());
        watchdog.join().unwrap();
        let logs = Logs {
            stdout: files.join("stdout.log"),
            stderr: files.join("stderr.log"),
            exit_code: run.exit_code,
        };
        fs::write(&logs.stdout, &stdout).unwrap();
        fs::write(&logs.stderr, &run.stderr).unwrap();
        let updated = fs::read(home.join("auth.json")).unwrap_or_default();
        if updated != copied {
            let unchanged = fs::read(&self.login).unwrap() == copied;
            match codex_login_kept(&copied, &updated) {
                Ok(()) if unchanged => fs::write(&self.login, &updated).unwrap(),
                Ok(()) => eprintln!("{id}: auth.json changed during the run: copy dropped"),
                Err(why) => eprintln!("{id}: the run's login was dropped: {why}"),
            }
        }
        eprintln!("{id} outcome: {:?}, usage: {:?}", run.outcome, run.usage);
        (run, logs)
    }

    /// The login's tokens, key and account id, before the runs and now:
    /// what a recording must never hold.
    fn login_values(&self) -> Vec<String> {
        let now = fs::read(&self.login).unwrap();
        [self.original.as_slice(), now.as_slice()]
            .into_iter()
            .filter_map(|bytes| serde_json::from_slice::<serde_json::Value>(bytes).ok())
            .flat_map(|login| {
                let tokens = &login["tokens"];
                ["id_token", "access_token", "refresh_token", "account_id"]
                    .into_iter()
                    .filter_map(|key| tokens[key].as_str().map(str::to_owned))
                    .chain(login["OPENAI_API_KEY"].as_str().map(str::to_owned))
                    .collect::<Vec<_>>()
            })
            .filter(|value| !value.is_empty())
            .collect()
    }
}

fn recording_asked() -> bool {
    std::env::var("OWLSHIFT_RECORD_CODEX").as_deref() == Ok("1")
}

#[test]
#[ignore = "live: runs `codex exec` on your Codex login; set OWLSHIFT_LIVE_CODEX=1"]
fn a_real_critic_runs_on_codex_through_the_executor() {
    let live = Live::new("");
    let harness = live.harness(strip_role_front_matter(Role::Verify, VERIFY_ROLE).unwrap());
    let report = live.run(&harness, "LIVE-2");
    if recording_asked() {
        record(
            &Logs::from(&report),
            live.tmp.path(),
            &Recording {
                name: "critic",
                argv: adapter_argv(),
                prompt: "the verify role (roles/verify.md) and the brief's path, on standard input",
                confinement: "the executor's read-only profile, through Executor::run (OWL-299)",
                setup: None,
            },
            &live.login_values(),
        );
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

/// The prompt of the apps-server check: the model lists its tools.
const TOOLS_PROMPT: &str = "Do not run any command and do not call any tool. Your last \
message is the result JSON the output schema describes, with status `done`, no finding, and \
as `summary` a JSON array of the exact names of every tool or function you can call in this \
session, each with its namespace or server prefix as you see it, MCP, connector, app and \
plugin tools included.";

/// The tools the ChatGPT apps server adds to a run, seen on 2026-10-16
/// (OWL-132).
const APPS_TOOLS: [&str; 4] = [
    "list_mcp_resources",
    "list_mcp_resource_templates",
    "read_mcp_resource",
    "request_plugin_install",
];

/// OWL-132: on a ChatGPT login, the adapter's command line offers the model
/// none of the ChatGPT apps server's tools, while the same run without
/// `--disable apps` offers them all. Two short runs outside the executor
/// (`Live::run_direct` says why); with `OWLSHIFT_RECORD_CODEX=1` they are
/// recorded as `apps_disabled` and `apps_disabled_control`.
#[test]
#[ignore = "live: runs `codex exec` twice on your Codex login; set OWLSHIFT_LIVE_CODEX=1"]
fn a_live_critic_run_is_offered_no_tool_of_the_apps_server() {
    let live = Live::new(".apps");
    let (adapter, adapter_logs) = live.run_direct("LIVE-APPS", &[], &[], TOOLS_PROMPT);
    let (control, control_logs) =
        live.run_direct("LIVE-APPS-CONTROL", &[&APPS_OFF], &[], TOOLS_PROMPT);
    if recording_asked() {
        let values = live.login_values();
        let setup = "a scratch repository; codex exec spawned by the test with the adapter's \
                     command line (the control: without `--disable apps`) and the prompt on \
                     standard input, and a CODEX_HOME of the run's own holding a copy of the \
                     user's ChatGPT login";
        for (logs, name, argv) in [
            (&adapter_logs, "apps_disabled", adapter_argv()),
            (
                &control_logs,
                "apps_disabled_control",
                without(adapter_argv(), &APPS_OFF),
            ),
        ] {
            record(
                logs,
                live.tmp.path(),
                &Recording {
                    name,
                    argv,
                    prompt: TOOLS_PROMPT,
                    confinement: "none: env -i with HOME, PATH, USER, LANG and CODEX_HOME",
                    setup: Some(setup),
                },
                &values,
            );
        }
    }
    let (adapter, control) = (tools(&adapter), tools(&control));
    for tool in APPS_TOOLS {
        assert!(offered(&control, tool), "control lacks {tool}: {control:?}");
        assert!(
            !offered(&adapter, tool),
            "adapter offers {tool}: {adapter:?}"
        );
    }
}

/// The flags OWL-316 passes to take image generation and web search away
/// from a critic run.
const TOOLS_OFF: [&[&str]; 2] = [
    &["--disable", "image_generation"],
    &["-c", "web_search=\"disabled\""],
];

/// The tools those flags take away, as the model named them on 2026-10-16
/// in OWL-132's recordings.
const TOOLS_TURNED_OFF: [&str; 2] = ["web__run", "image_gen__imagegen"];

/// OWL-316: the adapter's command line offers a critic run none of the
/// tools in [`TOOLS_TURNED_OFF`], while the same run without the flags in
/// [`TOOLS_OFF`] offers them all. Two short runs outside the executor, as
/// OWL-132's; with `OWLSHIFT_RECORD_CODEX=1` they are recorded as
/// `critic_tools` and `critic_tools_control`.
#[test]
#[ignore = "live: runs `codex exec` twice on your Codex login; set OWLSHIFT_LIVE_CODEX=1"]
fn a_live_critic_run_is_offered_only_the_tools_it_keeps() {
    let live = Live::new(".tools");
    let (adapter, adapter_logs) = live.run_direct("LIVE-TOOLS", &[], &[], TOOLS_PROMPT);
    let (control, control_logs) =
        live.run_direct("LIVE-TOOLS-CONTROL", &TOOLS_OFF, &[], TOOLS_PROMPT);
    if recording_asked() {
        let values = live.login_values();
        let setup = "a scratch repository; codex exec spawned by the test with the adapter's \
                     command line (the control: without `--disable image_generation` and \
                     `-c web_search=\"disabled\"`) and the prompt on standard input, and a \
                     CODEX_HOME of the run's own holding a copy of the user's ChatGPT login";
        let control_argv = TOOLS_OFF
            .iter()
            .fold(adapter_argv(), |argv, flags| without(argv, flags));
        for (logs, name, argv) in [
            (&adapter_logs, "critic_tools", adapter_argv()),
            (&control_logs, "critic_tools_control", control_argv),
        ] {
            record(
                logs,
                live.tmp.path(),
                &Recording {
                    name,
                    argv,
                    prompt: TOOLS_PROMPT,
                    confinement: "none: env -i with HOME, PATH, USER, LANG and CODEX_HOME",
                    setup: Some(setup),
                },
                &values,
            );
        }
    }
    let (adapter, control) = (tools(&adapter), tools(&control));
    for tool in TOOLS_TURNED_OFF {
        assert!(offered(&control, tool), "control lacks {tool}: {control:?}");
        assert!(
            !offered(&adapter, tool),
            "adapter offers {tool}: {adapter:?}"
        );
    }
    // Kept because no flag removes it: gone, the build plan is out of date.
    assert!(offered(&adapter, "spawn_agent"), "{adapter:?}");
}

/// The prompt of OWL-317's check: the model starts a sub-agent, as a critic
/// run may on its own, and waits for it.
const COLLABORATION_PROMPT: &str = "Do not run any shell command. Use the spawn_agent tool \
once, to start one sub-agent whose only task is to reply with the single word PONG. Then call \
list_agents once, then wait for that sub-agent with wait_agent. Your last message is the result \
JSON the output schema describes, with status `done`, no finding, and as `summary` the \
sub-agent's reply.";

/// OWL-317: a critic run asked to start a sub-agent and wait for it, with
/// the adapter's command line, reports its `wait_agent` call, the only one
/// of its collaboration calls that leaves an item. One run outside the
/// executor, as OWL-132's; with `OWLSHIFT_RECORD_CODEX=1` it is recorded as
/// `critic_collaboration` before the checks, so that a run that hangs after
/// its wait, as two did on 2026-10-16, is kept to look at but fails here.
#[test]
#[ignore = "live: runs `codex exec` once on your Codex login; set OWLSHIFT_LIVE_CODEX=1"]
fn a_live_critic_run_that_starts_a_sub_agent_is_seen() {
    let live = Live::new(".collab");
    let (run, logs) = live.run_direct("LIVE-COLLAB", &[], &[], COLLABORATION_PROMPT);
    if recording_asked() {
        record(
            &logs,
            live.tmp.path(),
            &Recording {
                name: "critic_collaboration",
                argv: adapter_argv(),
                prompt: COLLABORATION_PROMPT,
                confinement: "none: env -i with HOME, PATH, USER, LANG and CODEX_HOME",
                setup: Some(
                    "a scratch repository; codex exec spawned by the test with the adapter's \
                     command line and the prompt on standard input, and a CODEX_HOME of the \
                     run's own holding a copy of the user's ChatGPT login",
                ),
            },
            &live.login_values(),
        );
    }
    assert!(
        matches!(run.outcome, codex::Outcome::Completed { .. }),
        "{:?}",
        run.outcome
    );
    assert!(
        run.collaboration.iter().any(|tool| tool == "wait"),
        "{:?}",
        run.collaboration
    );
}

/// The flag OWL-320 tries: Codex's bound on how deep a thread may start
/// sub-agents, the critic's thread being at depth 0.
const DEPTH_LIMIT: [&str; 2] = ["-c", "agents.max_depth=0"];

/// The prompt of OWL-320's check. The sub-agent's task is one the parent
/// cannot do itself: Codex gives every shell command the running thread's id
/// as `CODEX_THREAD_ID` (the 0.154.0 binary's exec environment), so only
/// another thread writes an id other than the parent's to [`SPAWN_FILE`],
/// which the test reads whatever the run came to, a hang after `wait`
/// included. `fork_turns` `"none"`: in the first pair of 2026-10-16 the
/// control's spawn failed with "collab spawn failed: no thread with id",
/// most likely a fork looking up the parent's thread, which `--ephemeral`
/// keeps nowhere.
const SPAWN_PROMPT: &str = "Do not run any shell command yourself. Call the spawn_agent tool \
once, with fork_turns set to \"none\", to start one sub-agent whose only task is to run the \
shell command `echo \"$CODEX_THREAD_ID\" >> spawned.txt` in the current folder and then to \
reply DONE. Then wait for that sub-agent with wait_agent. If spawn_agent returns an error, do \
not retry and do not run the command yourself: your summary is then that error's text, word \
for word. Your last message is the result JSON the output schema describes, with status \
`done`, no finding, and as `summary` the sub-agent's reply.";

/// The file [`SPAWN_PROMPT`]'s command appends to, in the run's folder.
const SPAWN_FILE: &str = "spawned.txt";

/// What Codex says when it refuses a call: the depth limit's message, the
/// only one the 0.154.0 binary holds for it, found next to the spawn
/// handlers and in no instruction the model is given; and the refusal of a
/// tool the run does not offer, as `trusted_repo_mcp` shows for an MCP tool.
const DEPTH_REFUSAL: &str = "Agent depth limit reached";
const UNOFFERED_SPAWN: &str = "unsupported call: spawn_agent";

/// Who did the sub-agent's task in a run of [`SPAWN_PROMPT`] (OWL-320).
#[derive(Debug, PartialEq)]
enum Spawn {
    /// [`SPAWN_FILE`] or the reply holds a thread id that is not the
    /// parent's and that was made during the run: a UUIDv7 whose time lies
    /// between the parent's and the run's end. No model makes one up.
    AtWork { child: String },
    /// Codex refused the spawn: on standard error, or `relayed` when only
    /// the model's own text carries the refusal's message.
    Refused { relayed: bool },
    /// Neither: the parent did the task itself, no id came back, or the
    /// spawn failed for another reason. The pair settles nothing.
    Unsettled(String),
}

/// Reads a run of [`SPAWN_PROMPT`]: its raw `--json` stream, its standard
/// error, its last message when it completed, what it left in
/// [`SPAWN_FILE`], and when it ended, in milliseconds since the Unix epoch.
fn spawn_verdict(
    stream: &str,
    stderr: &str,
    last: Option<&str>,
    written: &str,
    ended_ms: u64,
) -> Spawn {
    let events: Vec<serde_json::Value> = stream
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    let Some(parent) = events
        .iter()
        .find(|event| event["type"] == "thread.started")
        .and_then(|event| event["thread_id"].as_str())
    else {
        return Spawn::Unsettled("no thread started".into());
    };
    let Some(started_ms) = uuid_v7_ms(parent) else {
        return Spawn::Unsettled(format!("the thread id {parent} is not a UUIDv7"));
    };
    let summary = last
        .map(|last| {
            serde_json::from_str::<serde_json::Value>(last)
                .ok()
                .and_then(|result| result["summary"].as_str().map(str::to_owned))
                .unwrap_or_else(|| last.to_owned())
        })
        .unwrap_or_default();
    let child = uuids(written)
        .into_iter()
        .chain(uuids(&summary))
        .find(|id| {
            *id != parent && uuid_v7_ms(id).is_some_and(|ms| (started_ms..=ended_ms).contains(&ms))
        });
    let messages: Vec<&str> = events
        .iter()
        .filter(|event| event["item"]["type"] == "agent_message")
        .filter_map(|event| event["item"]["text"].as_str())
        .collect();
    let machine = stderr.contains(DEPTH_REFUSAL) || stderr.contains(UNOFFERED_SPAWN);
    let relayed =
        summary.contains(DEPTH_REFUSAL) || messages.iter().any(|text| text.contains(DEPTH_REFUSAL));
    match child {
        Some(child) if machine || relayed => Spawn::Unsettled(format!(
            "a sub-agent's id {child} and a refusal in the same run"
        )),
        Some(child) => Spawn::AtWork {
            child: child.to_owned(),
        },
        None if machine => Spawn::Refused { relayed: false },
        None if relayed => Spawn::Refused { relayed: true },
        None => {
            let itself = written.contains(parent)
                || events.iter().any(|event| {
                    event["item"]["type"] == "command_execution"
                        && event["item"]["aggregated_output"]
                            .as_str()
                            .is_some_and(|output| output.contains(parent))
                });
            let errors: Vec<&str> = stderr
                .lines()
                .filter(|line| line.contains("ERROR"))
                .collect();
            Spawn::Unsettled(if itself {
                "the parent wrote its own thread id".into()
            } else if last.is_none() {
                format!("the run did not complete, no sub-agent's id, no refusal: {errors:?}")
            } else {
                format!("no sub-agent's id and no refusal: {summary:?}, {errors:?}")
            })
        }
    }
}

/// The time a UUIDv7 carries, in milliseconds since the Unix epoch: its
/// first 48 bits. `None` for another version.
fn uuid_v7_ms(id: &str) -> Option<u64> {
    if id.len() != 36 || id.as_bytes()[14] != b'7' {
        return None;
    }
    u64::from_str_radix(&format!("{}{}", &id[..8], &id[9..13]), 16).ok()
}

/// Every UUID-shaped substring of `text`, in order.
fn uuids(text: &str) -> Vec<&str> {
    let bytes = text.as_bytes();
    (0..bytes.len().saturating_sub(35))
        .filter(|&i| uuid_at(&bytes[i..]))
        .map(|i| &text[i..i + 36])
        .collect()
}

/// Whether `bytes` opens with 8-4-4-4-12 hexadecimal digits.
fn uuid_at(bytes: &[u8]) -> bool {
    bytes.len() >= 36
        && bytes[..36].iter().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => *c == b'-',
            _ => c.is_ascii_hexdigit(),
        })
}

fn now_ms() -> u64 {
    let since = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap();
    u64::try_from(since.as_millis()).unwrap()
}

/// The verdict of a live run of [`SPAWN_PROMPT`] that ended at `ended_ms`
/// and left `written` in [`SPAWN_FILE`], printed with the run's first event
/// and that file for the build plan.
fn spawn_of(id: &str, run: &codex::Run, logs: &Logs, written: &str, ended_ms: u64) -> Spawn {
    let stream = fs::read_to_string(&logs.stdout).unwrap();
    let last = match &run.outcome {
        codex::Outcome::Completed { text } => Some(text.as_str()),
        _ => None,
    };
    let verdict = spawn_verdict(
        &stream,
        &String::from_utf8_lossy(&run.stderr),
        last,
        written,
        ended_ms,
    );
    eprintln!(
        "{id}: {:?}, {SPAWN_FILE}: {written:?}, verdict: {verdict:?}",
        stream.lines().next()
    );
    verdict
}

#[test]
fn a_spawn_is_judged_from_codex_records() {
    // A parent started at 0x019a_0000_0000 ms, a run ended a minute later.
    let parent = "019a0000-0000-7000-8000-000000000001";
    let child = "019a0000-0001-7000-8000-000000000002";
    let later = "019b0000-0000-7000-8000-000000000003";
    let ended = 0x019a_0000_0000_u64 + 60_000;
    let started = format!(r#"{{"type":"thread.started","thread_id":"{parent}"}}"#);
    let result = |summary: &str| serde_json::json!({ "summary": summary }).to_string();
    let verdict = |stream: &str, stderr: &str, summary: &str| {
        spawn_verdict(stream, stderr, Some(&result(summary)), "", ended)
    };
    let at_work = Spawn::AtWork {
        child: child.into(),
    };
    assert_eq!(verdict(&started, "", child), at_work);
    // The file is read even when the run hung after its wait.
    let hung = |written: &str| spawn_verdict(&started, "", None, written, ended);
    assert_eq!(hung(&format!("{child}\n")), at_work);
    assert!(matches!(hung(&format!("{parent}\n")), Spawn::Unsettled(why) if why.contains("own")));
    let failed = verdict(
        &started,
        "ERROR codex_core::tools::router: error=collab spawn failed: no thread with id",
        "collab spawn failed",
    );
    assert!(matches!(failed, Spawn::Unsettled(why) if why.contains("no thread with id")));
    assert_eq!(
        verdict(
            &started,
            "ERROR codex_core: Agent depth limit reached.",
            "none"
        ),
        Spawn::Refused { relayed: false }
    );
    assert_eq!(
        verdict(
            &started,
            "",
            "Agent depth limit reached. Solve the task yourself."
        ),
        Spawn::Refused { relayed: true }
    );
    let echoed = format!(
        "{started}\n{}",
        serde_json::json!({
            "type": "item.completed",
            "item": { "type": "command_execution", "aggregated_output": format!("{parent}\n") },
        })
    );
    assert!(matches!(verdict(&echoed, "", parent), Spawn::Unsettled(_)));
    // An id made after the run, or none at all, is no sub-agent.
    assert!(matches!(verdict(&started, "", later), Spawn::Unsettled(_)));
    assert!(matches!(hung(""), Spawn::Unsettled(_)));
    assert_eq!(uuid_v7_ms(parent), Some(0x019a_0000_0000));
    assert_eq!(uuid_v7_ms("019a0000-0000-4000-8000-000000000001"), None);
}

/// OWL-320: `-c agents.max_depth=0` does not make Codex refuse a critic's
/// `spawn_agent`, checked live on 2026-10-16. Two runs of [`SPAWN_PROMPT`]
/// outside the executor, as OWL-132's: the probe on the adapter's command
/// line with [`DEPTH_LIMIT`], the control without it, each in a folder
/// without [`SPAWN_FILE`]. The control must show a sub-agent at work, or the
/// probe settles nothing; the probe showed one too ([`spawn_verdict`]), so
/// the adapter does not pass the flag. A release where the probe stops
/// showing one fails here and sends the reader to the build plan (OWL-317's
/// entry). Both runs hung after their wait on 2026-10-16, so the test takes
/// the watchdog's ten minutes; the runs are not recorded, a hung stream
/// being no fixture.
#[test]
#[ignore = "live: runs `codex exec` twice on your Codex login; set OWLSHIFT_LIVE_CODEX=1"]
fn a_depth_limit_of_zero_does_not_stop_a_live_critic_run_from_starting_a_sub_agent() {
    let live = Live::new(".depth");
    let file = live.main.join(SPAWN_FILE);
    // Each run starts without the file and leaves it to be read.
    let spawn_run = |id: &str, add: &[&[&str]]| {
        let _ = fs::remove_file(&file);
        let (run, logs) = live.run_direct(id, &[], add, SPAWN_PROMPT);
        let ended = now_ms();
        let written = fs::read_to_string(&file).unwrap_or_default();
        (run, logs, written, ended)
    };
    let (probe, probe_logs, probe_written, probe_ended) = spawn_run("LIVE-DEPTH", &[&DEPTH_LIMIT]);
    let (control, control_logs, control_written, control_ended) =
        spawn_run("LIVE-DEPTH-CONTROL", &[]);
    let probe = spawn_of(
        "LIVE-DEPTH",
        &probe,
        &probe_logs,
        &probe_written,
        probe_ended,
    );
    let control = spawn_of(
        "LIVE-DEPTH-CONTROL",
        &control,
        &control_logs,
        &control_written,
        control_ended,
    );
    assert!(
        matches!(control, Spawn::AtWork { .. }),
        "the control shows no sub-agent at work, so the probe settles nothing: {control:?}"
    );
    // Not refused on 2026-10-16: refused, the build plan is out of date.
    assert!(matches!(probe, Spawn::AtWork { .. }), "{probe:?}");
}

/// The tool names a run's result lists as its summary, which must include
/// the run's own shell, so that an empty or unrelated list fails.
fn tools(run: &codex::Run) -> Vec<String> {
    let codex::Outcome::Completed { text } = &run.outcome else {
        panic!("{:?}", run.outcome);
    };
    let result: serde_json::Value = serde_json::from_str(text).unwrap();
    let names: Vec<String> = serde_json::from_str(result["summary"].as_str().unwrap())
        .unwrap_or_else(|e| panic!("the summary is not a JSON array of names: {e}"));
    assert!(
        names.iter().any(|name| name.ends_with("exec_command")),
        "{names:?}"
    );
    names
}

/// Whether `names` holds `tool`, whatever namespace prefix the model gave it
/// and whether it wrote `.` or `__` between the parts of a name, as
/// `codex_fixtures.rs` reads the recordings.
fn offered(names: &[String], tool: &str) -> bool {
    let key = |name: &str| name.to_lowercase().replace("__", "_").replace('.', "_");
    let tool = key(tool);
    names.iter().map(|name| key(name)).any(|name| {
        name == tool
            || name
                .strip_suffix(tool.as_str())
                .is_some_and(|prefix| prefix.ends_with('_'))
    })
}

/// The files of a run's two streams, and its exit code.
struct Logs {
    stdout: PathBuf,
    stderr: PathBuf,
    exit_code: Option<i32>,
}

impl From<&RunReport> for Logs {
    fn from(report: &RunReport) -> Self {
        Self {
            stdout: report.stdout_log.clone(),
            stderr: report.stderr_log.clone(),
            exit_code: report.exit_code,
        }
    }
}

fn read_only() -> Permissions {
    Permissions {
        level: PermissionLevel::ReadOnly,
        network: false,
        browser: false,
    }
}

/// The flag OWL-132 passes to keep the ChatGPT apps server out.
const APPS_OFF: [&str; 2] = ["--disable", "apps"];

/// `args` without the one run of arguments `cut`, which it must hold.
fn without(mut args: Vec<String>, cut: &[&str]) -> Vec<String> {
    let at = args
        .windows(cut.len())
        .position(|run| run == cut)
        .unwrap_or_else(|| panic!("the adapter does not pass {cut:?}"));
    args.drain(at..at + cut.len());
    args
}

/// `args` with the run of arguments `add` right before `--output-schema`,
/// where the adapter's own flags end.
fn with(mut args: Vec<String>, add: &[&str]) -> Vec<String> {
    let at = args
        .iter()
        .position(|arg| arg == "--output-schema")
        .expect("the adapter passes --output-schema");
    args.splice(at..at, add.iter().map(|arg| (*arg).to_owned()));
    args
}

/// The adapter's command line for a live run, with the run's files named by
/// placeholders, so a recording carries the flags of its day.
fn adapter_argv() -> Vec<String> {
    let request = codex::Request {
        workdir: PathBuf::from("<worktree>"),
        model: None,
        effort: Some(Effort::Low),
        role: Role::Verify,
        permissions: read_only(),
        schema_file: PathBuf::from("<run-files>/schema.json"),
        last_message_file: PathBuf::from("<run-files>/last-message.json"),
    };
    let command = codex::command(Path::new("codex"), &request).unwrap();
    std::iter::once("codex".to_owned())
        .chain(
            command
                .get_args()
                .map(|arg| arg.to_string_lossy().into_owned()),
        )
        .collect()
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
        review: Some(ReviewBrief {
            mandates: vec![Mandate::Critic],
            pass: std::num::NonZeroU32::MIN,
            diff: Some(GivenDiff {
                base,
                path: RelativePath::new("change.diff").unwrap(),
            }),
            since: None,
            earlier: Vec::new(),
        }),
        pull_request_reviews: Vec::new(),
    }
}

/// `text` with every UUID-shaped substring, 8-4-4-4-12 hexadecimal digits,
/// replaced with the placeholder `codex_fixtures.rs` checks for.
fn without_uuids(text: &str) -> String {
    const PLACEHOLDER: &str = "00000000-0000-4000-8000-000000000000";
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < bytes.len() {
        if uuid_at(&bytes[i..]) {
            out.push_str(PLACEHOLDER);
            i += 36;
        } else {
            let c = text[i..].chars().next().unwrap();
            out.push(c);
            i += c.len_utf8();
        }
    }
    out
}

/// What a recording is named and says of its run.
struct Recording<'a> {
    name: &'a str,
    argv: Vec<String>,
    prompt: &'a str,
    confinement: &'a str,
    setup: Option<&'a str>,
}

/// Writes the run's stream and its record as `recording`, with the scratch
/// folders, the home folder and the thread id replaced by the placeholders
/// `codex_fixtures.rs` checks for; refused, before anything is written,
/// when either holds one of `login_values`.
fn record(logs: &Logs, scratch: &Path, recording: &Recording<'_>, login_values: &[String]) {
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
        // The sub-agents' thread ids too (OWL-317).
        without_uuids(&text)
    };
    let stream: Vec<String> = fs::read_to_string(&logs.stdout)
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
    let stderr: Vec<String> = fs::read_to_string(&logs.stderr)
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
    let mut meta = serde_json::json!({
        "recorded": jiff::Zoned::now().date().to_string(),
        "codex_version": version,
        "platform": std::env::consts::OS,
        "confinement": recording.confinement,
        "argv": recording.argv,
        "prompt": recording.prompt,
        "exit_status": logs.exit_code,
        "stderr": stderr,
        "output_last_message": message,
    });
    if let Some(setup) = recording.setup {
        meta["setup"] = setup.into();
    }
    let stream = stream.join("\n") + "\n";
    let meta = serde_json::to_string_pretty(&meta).unwrap() + "\n";
    // Checked before anything is written, and without naming the value.
    for value in login_values {
        assert!(
            !stream.contains(value.as_str()) && !meta.contains(value.as_str()),
            "the {} recording holds a value of the login: nothing was written",
            recording.name
        );
    }
    let dir =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../owlshift-adapters/tests/fixtures/codex");
    fs::write(dir.join(format!("{}.jsonl", recording.name)), stream).unwrap();
    fs::write(dir.join(format!("{}.meta.json", recording.name)), meta).unwrap();
}
