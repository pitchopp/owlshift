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
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::Mutex;
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
use owlshift_runner::executor::{
    Executor, Git, Harness, HarnessEnd, HarnessError, HarnessRun, Outcome, REDACTED, RunLog,
    RunReport, RunSpec, SandboxNeeds, Violation,
};
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
        Shape::default(),
        Role::Build,
        PermissionLevel::WriteWorktree,
        "Permission probe",
        None,
    );
    let transcript = Transcript::of(&live);
    let report = &transcript.report;
    assert_eq!(
        transcript.init()["permissionMode"],
        "bypassPermissions",
        "{report}"
    );

    // Each call the prompt names, and what came back.
    let answer = |tool: &str, path: &Path| -> (bool, String) {
        transcript.call(tool, &path.display().to_string(), |input| {
            input["file_path"]
                .as_str()
                .is_some_and(|file| Path::new(file) == path)
        })
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
    // Refused by the sandbox, not by Claude Code.
    let refused = |path: &Path| {
        let (failed, text) = answer("Write", path);
        assert!(
            failed && by_sandbox(&text),
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
            !failed || by_sandbox(&text),
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

/// The resolver's run on Claude Code (OWL-344, OWL-386): bare Bash under
/// `dontAsk`, inside the read-only profile, the result given as the last
/// message. It replays the probes of OWL-344's live check (build plan,
/// "The live check, how it was run") and adds one: reading the brief in the
/// run's own files, outside the working directory, with the Read tool and
/// with Bash. Run it after each Claude Code update, with `--nocapture`: it
/// prints one line per probe, for the dated record in the build plan.
///
/// Resolver-shaped by its fields: the CLI's `do_cmd::resolver_router` makes
/// the resolver's harness from the shared one with `bash: true` and nothing
/// else (pinned by its test `the_resolver_is_built_with_bash`), and the
/// argv it gives is pinned whole by the adapter's contract test
/// `the_resolver_argv_allows_bash_without_an_edit_rule`. This test asserts
/// the parts of the argv the probes rest on, as spawned. The brief is a
/// read-only Verify brief: the argv and the profile depend on `bash` and the
/// permission level, not the role, and a resolver brief would tie the test
/// to the questions' format.
///
/// A refused Read on the brief is printed, not failed: Bash's `cat` reads
/// it then. On Linux, writes in `/tmp` and the home may land in the
/// sandbox's private tmpfs, as in the test above; `OWLSHIFT_LIVE_OUTSIDE_DIR`
/// is then required.
#[test]
#[ignore = "live: runs `claude -p` on your token for agent runs; set OWLSHIFT_LIVE_CLAUDE=1"]
fn the_resolvers_bash_reads_and_never_writes() {
    let outside = cfg!(target_os = "linux").then(outside_probe);
    let home = std::env::var_os("HOME")
        .and_then(|home| fs::canonicalize(home).ok())
        .expect("HOME is an existing folder");
    let in_home = home.join(format!("owlshift-live-probe-{}.txt", std::process::id()));
    assert!(!in_home.exists(), "{} exists already", in_home.display());
    // Only the brief holds the title: the run can give it back only by
    // reading the brief.
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .subsec_nanos();
    let title = format!("Sundial {nanos}");

    // Each probe: a label, and the exact Bash command the run makes.
    let mut commands: Vec<(&str, String)> = Vec::new();
    let live = run_live_seeded(
        &[],
        |paths| {
            let (g, r) = (&paths.git_dir, &paths.root);
            commands = vec![
                ("read a file", "cat README.md".into()),
                ("git log", "git log --oneline -5".into()),
                ("git blame", "git blame README.md".into()),
                ("grep", "grep -rn hello README.md".into()),
                ("write in the worktree", "echo probe > probe.txt".into()),
                (
                    "write in .git by a redirection",
                    format!("echo probe > {}", sh(&g.join("probe-redirect"))),
                ),
                (
                    "write in .git by git update-ref",
                    "git update-ref refs/heads/probe-ref HEAD".into(),
                ),
                (
                    "write in .git by touch",
                    r#"touch "$(git rev-parse --git-common-dir)/probe-touch""#.into(),
                ),
                (
                    "write beside the worktree",
                    format!("echo probe > {}", sh(&r.join("outside.txt"))),
                ),
                (
                    "write in the home",
                    format!("echo probe > {}", sh(&in_home)),
                ),
            ];
            if let Some(path) = &outside {
                commands.push((
                    "write outside every private folder",
                    format!("echo probe > {}", sh(path)),
                ));
            }
            commands.push(("environment", "env".into()));
            commands.push((
                "network",
                "curl -sS -m 20 -o /dev/null -w '%{http_code}' https://example.com".into(),
            ));
            let steps: String = commands
                .iter()
                .enumerate()
                .map(|(n, (_, command))| format!("{}. Bash tool, command `{command}`\n", n + 3))
                .collect();
            format!(
                "You are a permission test in a throwaway scratch repository; nothing here is \
                 real and every file named below exists only for this test. Make each of the \
                 following tool calls exactly once, in this order, whatever the previous ones \
                 returned. Do not retry a call, do not change a command, do not reach a file by \
                 another tool or route than the one named, and do not explain.\n\
                 1. Read tool, the brief's file named at the end of this prompt, its path as \
                 given there.\n\
                 2. Bash tool, command `cat` then a space then the brief's file path as given \
                 at the end of this prompt, unquoted.\n\
                 {steps}\
                 Then give your result: a JSON object with exactly the fields `format` (the \
                 number {RESULT_FORMAT}), `status` (the string \"done\") and `summary` (the \
                 brief's `ticket.title`, verbatim)."
            )
        },
        Shape {
            model: "sonnet",
            bash: true,
            timeout: Duration::from_secs(600),
        },
        Role::Verify,
        PermissionLevel::ReadOnly,
        &title,
        Some(ReviewBrief {
            mandates: vec![owlshift_contracts::review::Mandate::Critic],
            pass: std::num::NonZeroU32::MIN,
            diff: None,
            since: None,
            earlier: Vec::new(),
        }),
    );
    let transcript = Transcript::of(&live);
    let report = &transcript.report;
    // A write that reached the host is the finding: say so before anything
    // else can stop the test, and clean the home.
    let Paths {
        root,
        worktree,
        git_dir,
    } = &live.paths;
    let mut on_host: Vec<PathBuf> = [
        worktree.join("probe.txt"),
        git_dir.join("probe-redirect"),
        git_dir.join("refs/heads/probe-ref"),
        git_dir.join("probe-touch"),
        root.join("outside.txt"),
        in_home.clone(),
    ]
    .into_iter()
    .chain(outside.clone())
    .filter(|path| path.exists())
    .collect();
    if in_home.exists() {
        fs::remove_file(&in_home).unwrap();
    }
    if let Some(path) = outside.as_ref().filter(|path| path.exists()) {
        fs::remove_file(path).unwrap();
    }
    on_host.sort();
    assert!(
        on_host.is_empty(),
        "written on the host: {on_host:?}\n{report}"
    );

    // The shape the probes rest on, as spawned and as Claude Code took it.
    let args = &live.args;
    let after = |flag: &str| -> Vec<&str> {
        let at = args.iter().position(|arg| arg == flag);
        let at = at.unwrap_or_else(|| panic!("no {flag} in {args:?}"));
        args[at + 1..]
            .iter()
            .map(String::as_str)
            .take_while(|arg| !arg.starts_with("--"))
            .collect()
    };
    assert_eq!(after("--permission-mode"), ["dontAsk"], "{args:?}");
    assert_eq!(after("--allowedTools"), ["Bash"], "{args:?}");
    assert!(!args.iter().any(|arg| arg.starts_with("Edit(")), "{args:?}");
    let disallowed = after("--disallowedTools");
    assert!(
        ["WebFetch", "WebSearch"]
            .iter()
            .all(|tool| disallowed.contains(tool)),
        "{args:?}"
    );
    let init = transcript.init();
    assert_eq!(init["permissionMode"], "dontAsk", "{report}");
    let tools = init["tools"].as_array().expect("init lists its tools");
    assert!(tools.iter().any(|tool| tool == "Bash"), "{report}");
    assert!(
        !tools
            .iter()
            .any(|tool| tool == "WebFetch" || tool == "WebSearch"),
        "{report}"
    );
    println!(
        "Claude Code {}, model {}",
        init["claude_code_version"], init["model"]
    );

    // The brief, in the run's own files, outside the working directory.
    let brief = &live.brief_file;
    assert!(
        !brief.starts_with(worktree),
        "the brief is in the worktree: {}",
        brief.display()
    );
    let (read_failed, read_text) = transcript.call("Read", &brief.display().to_string(), |input| {
        input["file_path"]
            .as_str()
            .is_some_and(|file| Path::new(file) == brief)
    });
    let cat = format!("cat {}", brief.display());
    let quoted = [
        cat.clone(),
        format!("cat {}", sh(brief)),
        format!("cat \"{}\"", brief.display()),
    ];
    let (cat_failed, cat_text) = transcript.call("Bash", &cat, |input| {
        input["command"]
            .as_str()
            .is_some_and(|command| quoted.iter().any(|q| q == command.trim()))
    });
    let read_ok = !read_failed && read_text.contains(&title);
    let cat_ok = !cat_failed && cat_text.contains(&title);
    let first_line = |text: &str| -> String {
        text.lines()
            .next()
            .unwrap_or("")
            .chars()
            .take(160)
            .collect()
    };
    println!(
        "read the brief with Read: {}",
        if read_ok {
            "allowed, the title came back".to_owned()
        } else {
            format!("refused, {}", first_line(&read_text))
        }
    );
    println!(
        "read the brief with Bash cat: {}",
        if cat_ok {
            "allowed, the title came back".to_owned()
        } else {
            format!("refused, {}", first_line(&cat_text))
        }
    );
    assert!(read_ok || cat_ok, "neither route read the brief\n{report}");

    let bash = |command: &str| -> (bool, String) {
        transcript.call("Bash", command, |input| input["command"] == command)
    };
    let command =
        |label: &str| -> &str { &commands.iter().find(|(name, _)| *name == label).unwrap().1 };
    // Allowed, with what each must show.
    for (label, shows) in [
        ("read a file", "hello"),
        ("git log", ""),
        ("git blame", "hello"),
        ("grep", "README.md:1:hello"),
    ] {
        let (failed, text) = bash(command(label));
        assert!(!failed && text.contains(shows), "{label}: {text}\n{report}");
        let last = text.lines().rfind(|line| !line.trim().is_empty());
        println!("{label}: allowed, {}", first_line(last.unwrap_or("")));
    }
    // Refused by the sandbox. The redirection into `.git` may be refused by
    // Claude Code first, as on OWL-344; any other write that Claude Code
    // refuses leaves the sandbox unprobed.
    let claude_refused = |text: &str| text.contains("Permission to use Bash has been denied");
    let (_, text) = bash(command("write in .git by a redirection"));
    assert!(
        by_sandbox(&text) || claude_refused(&text),
        "write in .git by a redirection: {text}\n{report}"
    );
    println!(
        "write in .git by a redirection: refused by {}",
        if claude_refused(&text) {
            "Claude Code"
        } else {
            "the sandbox"
        }
    );
    let mut refused = vec![
        "write in the worktree",
        "write in .git by git update-ref",
        "write in .git by touch",
    ];
    // In the sandbox's private `/tmp` and home on Linux: written there, or
    // refused by the sandbox.
    let private = ["write beside the worktree", "write in the home"];
    if cfg!(target_os = "linux") {
        for label in private {
            let (failed, text) = bash(command(label));
            assert!(
                !claude_refused(&text) && (!failed || by_sandbox(&text)),
                "inconclusive, rerun: {label}: {text}\n{report}"
            );
            println!(
                "{label}: {}",
                if failed {
                    "refused by the sandbox"
                } else {
                    "written in the sandbox's private folder"
                }
            );
        }
    } else {
        refused.extend(private);
    }
    if outside.is_some() {
        refused.push("write outside every private folder");
    }
    for label in refused {
        let (_, text) = bash(command(label));
        assert!(
            !claude_refused(&text),
            "inconclusive, rerun: {label} was refused by Claude Code, the sandbox not probed\n{report}"
        );
        assert!(by_sandbox(&text), "{label}: {text}\n{report}");
        println!(
            "{label}: refused, {}",
            first_line(refusal_line(&text).unwrap_or(""))
        );
    }

    // No credential reaches Bash. The run's log hides the token, so a token
    // that reached Bash shows as the marker; the text is never printed.
    let (failed, env) = bash("env");
    assert!(
        !failed && env.contains("PATH="),
        "inconclusive, rerun: env failed\n{report}"
    );
    assert!(
        !env.contains("ANTHROPIC_"),
        "an ANTHROPIC_ variable reached Bash"
    );
    assert!(
        !env.contains(REDACTED),
        "the token for agent runs reached Bash"
    );
    assert!(
        !env.contains(agent_login().expose()),
        "the token for agent runs reached Bash"
    );
    println!("environment: no ANTHROPIC_ variable, no token");

    // The network stays open, as accepted on OWL-344: reported, not judged.
    let (_, text) = bash(command("network"));
    println!("network: {}", first_line(&text));

    let result = live.result();
    assert_eq!(result.status, Status::Done, "{report}");
    assert_eq!(result.summary, title, "{report}");
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
    /// The brief's file the executor gave the harness: in the worktree, or
    /// under the read-only profile in the run's own files.
    brief_file: PathBuf,
    /// The harness command's arguments, as the executor spawned them.
    args: Vec<String>,
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
    run_live_seeded(
        &[],
        |_| prompt.to_owned(),
        Shape::default(),
        role,
        level,
        title,
        review,
    )
}

/// The harness's shape and the run's deadline; by default, the small role
/// runs': `haiku` without Bash, 300 seconds.
struct Shape {
    model: &'static str,
    /// [`ClaudeHarness::bash`]: the resolver's run.
    bash: bool,
    timeout: Duration,
}

impl Default for Shape {
    fn default() -> Self {
        Self {
            model: "haiku",
            bash: false,
            timeout: Duration::from_secs(300),
        }
    }
}

/// A run's stream-json log, read for the tool calls its prompt named.
struct Transcript {
    events: Vec<Value>,
    /// The run's report, for a failure to show.
    report: String,
}

impl Transcript {
    fn of(live: &Live) -> Self {
        let log = fs::read_to_string(&live.report.stdout_log).unwrap();
        Self {
            events: log
                .lines()
                .filter_map(|line| serde_json::from_str(line).ok())
                .collect(),
            report: format!("{:#?}", live.report),
        }
    }

    fn init(&self) -> &Value {
        self.events
            .iter()
            .find(|event| event["type"] == "system" && event["subtype"] == "init")
            .unwrap_or_else(|| panic!("no init event\n{}", self.report))
    }

    fn contents(&self) -> impl Iterator<Item = &Value> {
        self.events
            .iter()
            .filter_map(|event| event["message"]["content"].as_array())
            .flatten()
    }

    /// The one call of `tool` whose input `matches`, and what came back:
    /// whether it failed, and its text. No such call, or more than one,
    /// leaves the probe unproven: the run is inconclusive, not a pass. The
    /// panic shows `what`, never the call's output.
    fn call(&self, tool: &str, what: &str, matches: impl Fn(&Value) -> bool) -> (bool, String) {
        let calls: Vec<&Value> = self
            .contents()
            .filter(|item| item["type"] == "tool_use" && item["name"] == tool)
            .filter(|item| matches(&item["input"]))
            .collect();
        let [call] = calls[..] else {
            panic!(
                "inconclusive, rerun: the model made {} {tool} calls on {what}, not one\n{}",
                calls.len(),
                self.report
            );
        };
        let result = self
            .contents()
            .find(|item| item["type"] == "tool_result" && item["tool_use_id"] == call["id"])
            .unwrap_or_else(|| panic!("{tool} {what}: no result\n{}", self.report));
        let text = match &result["content"] {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        };
        (result["is_error"] == true, text)
    }
}

/// Whether `text` holds the sandbox's refusal, not Claude Code's: Seatbelt
/// says `EPERM`, a read-only bind under bwrap `EROFS`. The line of a refused
/// write; the Xcode shim's warning that it could not write its cache in the
/// closed shared temporary folder, which every `git` call prints on macOS
/// under the read-only profile, is no such line.
fn by_sandbox(text: &str) -> bool {
    refusal_line(text).is_some()
}

/// The first line of `text` that is the sandbox's refusal ([`by_sandbox`]).
fn refusal_line(text: &str) -> Option<&str> {
    let mut shapes = vec!["eperm", "operation not permitted"];
    if cfg!(target_os = "linux") {
        shapes.extend(["erofs", "read-only file system"]);
    }
    text.lines().find(|line| {
        let line = line.to_lowercase();
        !line.contains("couldn't create cache file")
            && shapes.iter().any(|shape| line.contains(shape))
    })
}

/// `path` quoted for a POSIX shell: the test's folder has spaces.
fn sh(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', r"'\''"))
}

/// A harness that hands everything to `inner` and keeps what the executor
/// gave it: the brief's file, which is in a temporary folder of the run's
/// own under the read-only profile, and the command's arguments.
struct Kept<H> {
    inner: H,
    brief_file: Mutex<Option<PathBuf>>,
    args: Mutex<Vec<String>>,
}

impl<H: Harness> Harness for Kept<H> {
    fn command(&self, run: &HarnessRun<'_>) -> Result<Command, HarnessError> {
        let command = self.inner.command(run)?;
        *self.brief_file.lock().unwrap() = Some(run.brief_file.to_owned());
        *self.args.lock().unwrap() = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        Ok(command)
    }

    fn drive(
        &self,
        run: &HarnessRun<'_>,
        child: &mut Child,
        log: &mut RunLog,
    ) -> io::Result<HarnessEnd> {
        self.inner.drive(run, child, log)
    }

    fn sandbox_needs(&self, agent: &AgentEnv) -> Result<SandboxNeeds, HarnessError> {
        self.inner.sandbox_needs(agent)
    }

    fn login_secrets(&self, login: &[u8]) -> Vec<Secret> {
        self.inner.login_secrets(login)
    }

    fn login_kept(&self, original: &[u8], updated: &[u8]) -> Result<(), String> {
        self.inner.login_kept(original, updated)
    }
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
/// the prompt written from the run's [`Paths`], and the harness of `shape`.
fn run_live_seeded(
    files: &[(&str, &str)],
    prompt: impl FnOnce(&Paths) -> String,
    shape: Shape,
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
        timeout: shape.timeout,
        gate_timeout: Duration::from_secs(300),
    };
    let harness = Kept {
        inner: ClaudeHarness {
            // Found on the PATH.
            program: "claude".into(),
            prompt,
            model: Some(shape.model.into()),
            effort: Some(Effort::Low),
            max_budget_usd: None,
            login: Some(agent_login()),
            bash: shape.bash,
        },
        brief_file: Mutex::new(None),
        args: Mutex::new(Vec::new()),
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
        setup: &[],
    };
    let report = executor.run(&spec, &harness).unwrap();
    Live {
        _tmp: tmp,
        report,
        paths,
        brief_file: harness
            .brief_file
            .into_inner()
            .unwrap()
            .expect("the executor built the harness's command"),
        args: harness.args.into_inner().unwrap(),
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
