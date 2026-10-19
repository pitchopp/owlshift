//! Tiny real role runs through the executor on Claude Code: worktree,
//! brief, agent environment, process tree, result and isolation, confined
//! and on the token for agent runs that `owlshift init` stored (OWL-94).
//! They spend a little of the subscription, so they never run in CI:
//!
//! ```sh
//! OWLSHIFT_LIVE_CLAUDE=1 cargo test -p owlshift-testkit --test executor_live -- --ignored
//! ```

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use owlshift_adapters::harness::claude::Effort;
use owlshift_contracts::Role;
use owlshift_contracts::brief::{
    Author, Brief, PermissionLevel, Permissions, Relation, ReviewBrief, TicketBrief,
};
use owlshift_contracts::format::{Format, RESULT_FORMAT};
use owlshift_contracts::ids::{RelativePath, TicketId};
use owlshift_contracts::result::{RunResult, Status};
use owlshift_platform::keychain::{Keychain, Secret};
use owlshift_runner::agent_env::AgentEnv;
use owlshift_runner::executor::harness::{CLAUDE_AGENT_ACCOUNT, ClaudeHarness};
use owlshift_runner::executor::{Executor, Git, Outcome, RunReport, RunSpec, Violation};
use owlshift_testkit::git::{GitEnv, seed};
use serde_json::Value;

#[test]
#[ignore = "live: runs `claude -p` on your token for agent runs; set OWLSHIFT_LIVE_CLAUDE=1"]
fn a_real_role_runs_through_the_executor() {
    // A role small enough for a fast model: read the brief, answer in
    // `result.json`.
    let role = format!(
        "You are a test role. Read the brief, a JSON file. Then write the file named by its \
         `result_path`: a JSON object with exactly the fields `format` (the number \
         {RESULT_FORMAT}), `status` (the string \"done\") and `summary` (the brief's \
         `ticket.title`, verbatim). Do nothing else."
    );
    let title = "Heliotrope forty-two";
    let review = ReviewBrief {
        mandates: vec![owlshift_contracts::review::Mandate::Critic],
        pass: std::num::NonZeroU32::MIN,
        diff: None,
        since: None,
        earlier: Vec::new(),
    };
    let live = run_live(
        &role,
        Role::Verify,
        PermissionLevel::ReadOnly,
        title,
        Some(review),
    );
    let result = live.result();
    assert_eq!(result.status, Status::Done);
    assert_eq!(result.summary, title);
    assert!(live.report.usage.is_some(), "{:?}", live.report);
    assert!(fs::metadata(&live.report.stdout_log).unwrap().len() > 0);
}

/// Claude Code's bundled skills do not reach a confined run, read-only or
/// write (OWL-333). Without `--disable-slash-commands`, both runs listed the
/// CLI's own skills in `init`, were offered `Skill`, and got `dataviz`'s
/// text when they invoked it. The `init` checks hold whatever the model
/// does; the check on `Skill` calls covers a model that tries anyway. It
/// prints counts only, never a skill's name or text.
#[test]
#[ignore = "live: runs `claude -p` on your token for agent runs; set OWLSHIFT_LIVE_CLAUDE=1"]
fn bundled_skills_do_not_reach_a_confined_run() {
    let role = format!(
        "You are a test role. First invoke the skill named `dataviz` with the `Skill` tool, \
         with no arguments; if no `Skill` tool is offered, look for it once with any tool \
         search you have, then go on. Do not follow a skill's instructions. Then write the file \
         named by the brief's `result_path`: a JSON object with exactly the fields `format` \
         (the number {RESULT_FORMAT}), `status` (the string \"done\") and `summary` (one \
         sentence: whether the skill's text came back, or why not). Do nothing else."
    );
    for level in [PermissionLevel::ReadOnly, PermissionLevel::WriteWorktree] {
        let live = run_live(&role, Role::Build, level, "Invoke a skill", None);
        assert_eq!(live.result().status, Status::Done, "{level:?}");

        let log = fs::read_to_string(&live.report.stdout_log).unwrap();
        let events: Vec<Value> = log
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let inits: Vec<&Value> = events
            .iter()
            .filter(|event| event["type"] == "system" && event["subtype"] == "init")
            .collect();
        assert!(!inits.is_empty(), "{level:?}: no init event");
        for init in &inits {
            let listed = |key: &str| match &init[key] {
                Value::Null => 0,
                Value::Array(items) => items.len(),
                other => panic!("{level:?}: `{key}` is not a list: {other}"),
            };
            let tools = init["tools"].as_array().expect("init lists its tools");
            println!(
                "{level:?}: claude_code_version {}, skills {}, slash_commands {}, tools {}",
                init["claude_code_version"],
                listed("skills"),
                listed("slash_commands"),
                tools.len()
            );
            assert_eq!(listed("skills"), 0, "{level:?}");
            assert_eq!(listed("slash_commands"), 0, "{level:?}");
            assert!(!tools.iter().any(|tool| tool == "Skill"), "{level:?}");
        }

        // A `Skill` call, if the model makes one anyway, must have failed.
        let contents = || {
            events
                .iter()
                .filter_map(|event| event["message"]["content"].as_array())
                .flatten()
        };
        let results: HashMap<&str, &Value> = contents()
            .filter(|item| item["type"] == "tool_result")
            .filter_map(|item| Some((item["tool_use_id"].as_str()?, item)))
            .collect();
        for call in contents().filter(|item| item["type"] == "tool_use" && item["name"] == "Skill")
        {
            let id = call["id"].as_str().unwrap();
            let result = results
                .get(id)
                .unwrap_or_else(|| panic!("{level:?}: a Skill call has no result"));
            assert_eq!(
                result["is_error"], true,
                "{level:?}: a Skill call succeeded"
            );
        }
    }
}

/// A write-in-worktree run edits the repository's `.claude/` files, which
/// Claude Code refused as sensitive under `acceptEdits`, while the runner's
/// own guards still hold (OWL-346): the sandbox refuses a git hook and a
/// file outside the worktree, and the isolation check quarantines a ref the
/// run wrote in the git folder. Each call is asked for by name, on its exact
/// path; a model that skips one makes the run inconclusive, not a pass, and
/// the test says so.
///
/// On Linux, bwrap gives the run a private `/tmp`, a tmpfs, and the test's
/// folder is in `/tmp`: a write beside the worktree may land in that tmpfs
/// and vanish with the run, so there only its absence on the host is
/// asserted. The refusal is then probed in `OWLSHIFT_LIVE_OUTSIDE_DIR`, a
/// folder the user can write outside every private one, which the sandbox
/// sees through its read-only `/`: `EROFS`, where Seatbelt says `EPERM`.
#[test]
#[ignore = "live: runs `claude -p` on your token for agent runs; set OWLSHIFT_LIVE_CLAUDE=1"]
fn a_build_edits_claude_files_and_the_runners_guards_still_hold() {
    let files = [
        (".claude/rules/x.md", "rule one\n"),
        (".claude/settings.json", "{}\n"),
    ];
    let outside = cfg!(target_os = "linux").then(outside_probe);
    let live = run_live_seeded(
        &files,
        |paths| {
            let (w, g, r) = (
                paths.worktree.display(),
                paths.git_dir.display(),
                paths.root.display(),
            );
            let step_8 = match &outside {
                Some(path) => format!(
                    "8. Write tool, file `{}`, content `outside`.\n",
                    path.display()
                ),
                None => String::new(),
            };
            let head = Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(paths.git_dir.parent().unwrap())
                .output()
                .unwrap();
            let head = String::from_utf8(head.stdout).unwrap();
            format!(
                "You are a permission test in a throwaway scratch repository; nothing here is \
                 real and every file named below exists only for this test. Make each of the \
                 following tool calls exactly once, in this order, whatever the previous ones \
                 returned. Do not retry a call, do not reach a file by another tool or route \
                 than the one named, and do not explain.\n\
                 1. Read tool, file `{w}/.claude/rules/x.md`.\n\
                 2. Read tool, file `{w}/.claude/settings.json`.\n\
                 3. Edit tool, file `{w}/.claude/rules/x.md`: replace `rule one` with `rule one, edited`.\n\
                 4. Write tool, file `{w}/.claude/settings.json`, content `{{\"probe\": true}}`.\n\
                 5. Write tool, file `{g}/hooks/pre-commit`, content `#!/bin/sh` then a newline then `exit 0`.\n\
                 6. Write tool, file `{g}/refs/heads/probe-ref`, content `{}` then a newline.\n\
                 7. Write tool, file `{r}/outside.txt`, content `outside`.\n\
                 {step_8}\
                 Then write the file named by the brief's `result_path`: a JSON object with \
                 exactly the fields `format` (the number {RESULT_FORMAT}), `status` (the string \
                 \"done\") and `summary` (the string \"probed\").",
                head.trim()
            )
        },
        Role::Build,
        PermissionLevel::WriteWorktree,
        "Permission probe",
        None,
    );
    let report = format!("{:#?}", live.report);
    let log = fs::read_to_string(&live.report.stdout_log).unwrap();
    let events: Vec<Value> = log
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    let init = events
        .iter()
        .find(|event| event["type"] == "system" && event["subtype"] == "init")
        .unwrap_or_else(|| panic!("no init event\n{report}"));
    assert_eq!(init["permissionMode"], "bypassPermissions", "{report}");

    // Each call the prompt names, and what came back.
    let contents = || {
        events
            .iter()
            .filter_map(|event| event["message"]["content"].as_array())
            .flatten()
    };
    let results: HashMap<&str, &Value> = contents()
        .filter(|item| item["type"] == "tool_result")
        .filter_map(|item| Some((item["tool_use_id"].as_str()?, item)))
        .collect();
    let answer = |tool: &str, path: &Path| -> (bool, String) {
        let call = contents()
            .find(|item| {
                item["type"] == "tool_use"
                    && item["name"] == tool
                    && item["input"]["file_path"]
                        .as_str()
                        .is_some_and(|file| Path::new(file) == path)
            })
            .unwrap_or_else(|| {
                panic!(
                    "inconclusive, rerun: the model made no {tool} call on {}\n{report}",
                    path.display()
                )
            });
        let result = results
            .get(call["id"].as_str().unwrap())
            .unwrap_or_else(|| panic!("{tool} {}: no result\n{report}", path.display()));
        let text = match &result["content"] {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        };
        (result["is_error"] == true, text)
    };
    let Paths {
        root,
        worktree,
        git_dir,
    } = &live.paths;
    for (tool, path) in [
        ("Edit", worktree.join(".claude/rules/x.md")),
        ("Write", worktree.join(".claude/settings.json")),
        ("Write", git_dir.join("refs/heads/probe-ref")),
    ] {
        let (failed, text) = answer(tool, &path);
        assert!(!failed, "{tool} {} was refused: {text}", path.display());
    }
    // Refused by the sandbox, not by Claude Code: Seatbelt says `EPERM`, a
    // read-only bind under bwrap `EROFS`.
    let by_sandbox: &[&str] = if cfg!(target_os = "linux") {
        &[
            "EPERM",
            "operation not permitted",
            "EROFS",
            "read-only file system",
        ]
    } else {
        &["EPERM", "operation not permitted"]
    };
    let refused = |path: &Path| {
        let (failed, text) = answer("Write", path);
        assert!(
            failed && by_sandbox.iter().any(|shape| text.contains(shape)),
            "Write {}: {text}",
            path.display()
        );
    };
    refused(&git_dir.join("hooks/pre-commit"));
    let beside = root.join("outside.txt");
    if cfg!(target_os = "linux") {
        // In the sandbox's private `/tmp`: written there, or refused by the
        // sandbox; any other error leaves the probe unproven.
        let (failed, text) = answer("Write", &beside);
        assert!(
            !failed || by_sandbox.iter().any(|shape| text.contains(shape)),
            "inconclusive, rerun: Write {}: {text}",
            beside.display()
        );
        println!(
            "Write {}: {}",
            beside.display(),
            if failed {
                "refused by the sandbox"
            } else {
                "written in the sandbox's private /tmp"
            }
        );
    } else {
        refused(&beside);
    }
    if let Some(path) = &outside {
        refused(path);
        assert!(!path.exists(), "{} reached the host", path.display());
    }

    let read = |path: PathBuf| fs::read_to_string(path).ok();
    assert_eq!(
        read(worktree.join(".claude/rules/x.md")).as_deref(),
        Some("rule one, edited\n")
    );
    assert_eq!(
        read(worktree.join(".claude/settings.json"))
            .map(|text| serde_json::from_str::<Value>(&text).unwrap()),
        Some(serde_json::json!({"probe": true}))
    );
    assert!(!git_dir.join("hooks/pre-commit").exists());
    assert!(!beside.exists());
    let Outcome::Quarantined(violations) = &live.report.outcome else {
        panic!("the ref the run wrote was not caught\n{report}");
    };
    assert_eq!(
        violations,
        &[Violation::Refs(vec!["refs/heads/probe-ref".into()])],
        "{report}"
    );
}

/// A file the run is asked to write in `OWLSHIFT_LIVE_OUTSIDE_DIR`, which
/// must be a folder the user can write outside the folders the Linux sandbox
/// makes private: there a write meets the sandbox's read-only `/`, and its
/// absence on the host proves the refusal held.
fn outside_probe() -> PathBuf {
    let dir = std::env::var_os("OWLSHIFT_LIVE_OUTSIDE_DIR").map(PathBuf::from);
    let Some(dir) = dir.and_then(|dir| fs::canonicalize(dir).ok()) else {
        panic!(
            "set OWLSHIFT_LIVE_OUTSIDE_DIR to an existing folder you can write, outside /tmp, \
             /var/tmp, /run/user and your home"
        );
    };
    let home = std::env::var_os("HOME").and_then(|home| fs::canonicalize(home).ok());
    let private = ["/tmp", "/var/tmp", "/run/user"]
        .iter()
        .map(PathBuf::from)
        .chain(home)
        .find(|private| dir.starts_with(private));
    assert!(
        private.is_none(),
        "OWLSHIFT_LIVE_OUTSIDE_DIR {} is in {}, private to the sandbox",
        dir.display(),
        private.unwrap_or_default().display()
    );
    let probe = dir.join(format!("owlshift-live-outside-{}.txt", std::process::id()));
    assert!(!probe.exists(), "{} exists already", probe.display());
    fs::write(&probe, "").unwrap_or_else(|e| panic!("{} is not writable: {e}", dir.display()));
    fs::remove_file(&probe).unwrap();
    probe
}

/// One finished live run, and the folder that holds its files.
struct Live {
    _tmp: tempfile::TempDir,
    report: RunReport,
    paths: Paths,
}

impl Live {
    /// The run's validated result; panics when the run did not finish.
    fn result(&self) -> &RunResult {
        let Outcome::Finished { result, .. } = &self.report.outcome else {
            panic!(
                "{:?}\nstderr: {}",
                self.report.outcome,
                fs::read_to_string(&self.report.stderr_log).unwrap_or_default()
            );
        };
        result
    }
}

/// Runs `prompt` as `role` on a fresh one-commit repository through the
/// executor, confined, on `haiku` at effort `low`, with no network.
fn run_live(
    prompt: &str,
    role: Role,
    level: PermissionLevel,
    title: &str,
    review: Option<ReviewBrief>,
) -> Live {
    run_live_seeded(&[], |_| prompt.to_owned(), role, level, title, review)
}

/// Where a live run's files are, resolved, for a prompt to name them.
struct Paths {
    /// The folder that holds the others.
    root: PathBuf,
    /// The run's worktree, a linked worktree of the checkout.
    worktree: PathBuf,
    /// The repository's common git folder, the checkout's `.git`.
    git_dir: PathBuf,
}

/// [`run_live`], with `files` (path, content) committed beside the README,
/// and the prompt written from the run's [`Paths`].
fn run_live_seeded(
    files: &[(&str, &str)],
    prompt: impl FnOnce(&Paths) -> String,
    role: Role,
    level: PermissionLevel,
    title: &str,
    review: Option<ReviewBrief>,
) -> Live {
    assert_eq!(
        std::env::var("OWLSHIFT_LIVE_CLAUDE").as_deref(),
        Ok("1"),
        "this test spends subscription usage: set OWLSHIFT_LIVE_CLAUDE=1 to run it"
    );
    let tmp = tempfile::Builder::new()
        .prefix("owlshift live ")
        .tempdir()
        .unwrap();
    // Resolved: Claude Code matches paths once resolved, and a write named
    // through a link to the temporary folder can miss (build plan, OWL-327).
    let root = fs::canonicalize(tmp.path()).unwrap();
    let project = root.join("project");
    fs::create_dir_all(&project).unwrap();
    fs::write(project.join("README.md"), "hello\n").unwrap();
    for (path, content) in files {
        let path = project.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }
    let env = GitEnv::create(root.join("home")).unwrap();
    let remote = seed(&env, &root, &project).unwrap();
    let paths = Paths {
        worktree: root.join("worktree"),
        git_dir: remote.checkout.join(".git"),
        root,
    };
    let prompt = prompt(&paths);
    let runner = env.clone();
    let executor = Executor {
        git: Git::with_setup("git", move |command| runner.apply(command)),
        agent: AgentEnv::from_runner(&[], &[], &[]).unwrap(),
        forge_hosts: vec!["github.com".into()],
        timeout: Duration::from_secs(300),
        gate_timeout: Duration::from_secs(300),
    };
    let harness = ClaudeHarness {
        // Found on the PATH.
        program: "claude".into(),
        prompt,
        model: Some("haiku".into()),
        effort: Some(Effort::Low),
        max_budget_usd: None,
        login: Some(agent_login()),
        bash: false,
    };
    let brief = Brief {
        format: Format,
        role,
        project: "live".into(),
        ticket: TicketBrief {
            id: TicketId::new("LIVE-1").unwrap(),
            title: title.into(),
            url: None,
            labels: Vec::new(),
            author: Author {
                name: "maintainer".into(),
                relation: Relation::Decider,
            },
            description: "Report the title.".into(),
        },
        decider: "maintainer".into(),
        thread: Vec::new(),
        resolve: Vec::new(),
        checkpoint: None,
        zones: Vec::new(),
        resources: Vec::new(),
        rules: Vec::new(),
        permissions: Permissions {
            level,
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
        review,
        pull_request_reviews: Vec::new(),
    };
    let spec = RunSpec {
        main: &remote.checkout,
        worktree: &paths.worktree,
        branch: "owlshift/LIVE-1",
        base: "origin/main",
        run_dir: &paths.root.join("run"),
        brief: &brief,
    };
    let report = executor.run(&spec, &harness).unwrap();
    Live {
        _tmp: tmp,
        report,
        paths,
    }
}

/// The token for agent runs, from the system keychain.
fn agent_login() -> Secret {
    Keychain::system()
        .unwrap()
        .read(CLAUDE_AGENT_ACCOUNT)
        .unwrap()
        .expect("no token for agent runs: run `claude setup-token`, then `owlshift init`")
}
