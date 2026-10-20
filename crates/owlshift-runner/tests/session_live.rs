//! Live check of OWL-394: a headless `claude -p` run resumed with `--resume`
//! under Owlshift's confinement and agent login, as the executor would run
//! it. Each run gets a fresh temporary folder and an empty configuration
//! folder; the only thing carried from the asking run to its resume is the
//! session's transcript, copied out after the first run and into the second
//! run's configuration folder at the same relative path.
//!
//! It measures, on the same small task: the asking run (A), its resume a
//! little later (B, as after a resolver decision), a fresh run of the same
//! task (C), and what the CLI does with a session it cannot find (D). It
//! spends subscription usage, so it never runs in CI:
//!
//! ```sh
//! OWLSHIFT_LIVE_CLAUDE=1 cargo test -p owlshift-runner --test session_live \
//!     -- --ignored --nocapture a_session_resumes_minutes_later
//! ```
//!
//! With `OWLSHIFT_LIVE_SESSION_KEEP=<folder>`, the scratch repository and the
//! transcript are kept there, and a later run of the second test resumes the
//! same session hours afterwards (B'), as after the decider's answer:
//!
//! ```sh
//! OWLSHIFT_LIVE_CLAUDE=1 OWLSHIFT_LIVE_SESSION_KEEP=<folder> cargo test -p owlshift-runner \
//!     --test session_live -- --ignored --nocapture a_kept_session_resumes_hours_later
//! ```
//!
//! `OWLSHIFT_LIVE_RESUME_WAIT_SECS` sets the wait between A and B (90 by
//! default). The figures are printed, one line per run; the token is never
//! printed.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use owlshift_adapters::harness::claude::{
    self, Effort, Outcome, Request, SessionId, command, drive, transcript_path,
};
use owlshift_contracts::brief::{PermissionLevel, Permissions};
use owlshift_contracts::ids::RelativePath;
use owlshift_platform::keychain::{Keychain, Secret};
use owlshift_runner::agent_env::{AgentEnv, RunPaths, mcp_findings};
use owlshift_runner::executor::harness::{CLAUDE_AGENT_ACCOUNT, ClaudeHarness, Harness};
use owlshift_runner::executor::{HARNESS_CONFIG_DIR, HARNESS_TEMP_DIR};

const MODEL: &str = "sonnet";

/// The files the task reads: a few thousand lines of this repository.
const FILES: &[(&str, &str)] = &[
    (
        "roadmap.md",
        include_str!("../../../docs/design/roadmap.md"),
    ),
    (
        "claude.rs",
        include_str!("../../owlshift-adapters/src/harness/claude.rs"),
    ),
    ("harness.rs", include_str!("../src/executor/harness.rs")),
];

const ASK: &str = "Read the files roadmap.md, claude.rs, harness.rs and nonce.txt of the \
     working directory, each in full, with the Read tool. Then, doing nothing else, write the \
     file result.json in the working directory holding exactly \
     {\"status\":\"questions\",\"question\":\"Which file should the summary cover: A claude.rs or \
     B harness.rs?\"} and stop.";

const ANSWER: &str = "The answer to your question is B. Without reading any file again, write \
     result.json in the working directory holding {\"status\":\"done\",\"summary\":\"<two \
     sentences on what harness.rs does>\",\"nonce\":\"<the word nonce.txt holds>\"} and stop.";

const FRESH: &str = "Read the files roadmap.md, claude.rs, harness.rs and nonce.txt of the \
     working directory, each in full, with the Read tool. The summary is to cover harness.rs. \
     Then write result.json in the working directory holding {\"status\":\"done\",\"summary\":\
     \"<two sentences on what harness.rs does>\",\"nonce\":\"<the word nonce.txt holds>\"} and \
     stop.";

/// A UUID in its text form, from a seed, version 4 and variant bits set.
fn uuid(seed: &str) -> String {
    let digest = Sha256::digest(seed.as_bytes());
    let mut b: [u8; 16] = digest[..16].try_into().unwrap();
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let hex: String = b.iter().map(|byte| format!("{byte:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

fn now_seed() -> String {
    let at = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    format!("{}-{}", at.as_nanos(), std::process::id())
}

fn live() {
    assert_eq!(
        std::env::var("OWLSHIFT_LIVE_CLAUDE").as_deref(),
        Ok("1"),
        "this test spends subscription usage: set OWLSHIFT_LIVE_CLAUDE=1 to run it"
    );
}

fn token() -> Secret {
    Keychain::system()
        .unwrap()
        .read(CLAUDE_AGENT_ACCOUNT)
        .unwrap()
        .expect("no token for agent runs: run `claude setup-token`, then `owlshift init`")
}

/// The scratch repository: the task's files and a nonce only the asking
/// run reads.
fn scratch(at: &Path, nonce: &str) {
    fs::create_dir_all(at).unwrap();
    let init = Command::new("git")
        .args(["init", "-q"])
        .current_dir(at)
        .status()
        .unwrap();
    assert!(init.success());
    for (name, text) in FILES {
        fs::write(at.join(name), text).unwrap();
    }
    fs::write(at.join("nonce.txt"), format!("{nonce}\n")).unwrap();
}

/// How a run starts its session.
enum Session<'a> {
    Start(&'a str),
    Resume(&'a str),
}

/// The tools removed from every run (the adapter's `BEYOND_RUN_TOOLS`), and
/// the web tools of a run without network: none may show in a resumed run's
/// `init`.
const REMOVED_TOOLS: &[&str] = &[
    "RemoteTrigger",
    "PushNotification",
    "DesignSync",
    "Workflow",
    "CronCreate",
    "CronDelete",
    "CronList",
    "SendMessage",
    "ListAgents",
    "EnterWorktree",
    "ExitWorktree",
    "Agent",
    "Task",
    "WebFetch",
    "WebSearch",
];

/// What one run reported, as printed.
struct Measured {
    outcome: Outcome,
    init: Value,
    result_record: Value,
    reads: usize,
    result_json: Option<Value>,
    config_files: Vec<String>,
    stderr: String,
}

/// Runs `claude -p` once, confined as the executor confines it, with a
/// fresh temporary folder whose empty configuration folder is first seeded
/// by `seed`. Returns what it reported and, through `keep`, a look at the
/// configuration folder before it is removed.
fn run_once(
    workdir: &Path,
    session: Session<'_>,
    prompt: &str,
    budget: Option<f64>,
    seed: impl FnOnce(&Path),
    keep: impl FnOnce(&Path),
) -> Measured {
    let agent = AgentEnv::from_runner(&[], &[], &[]).unwrap();
    let harness = ClaudeHarness {
        program: "claude".into(),
        prompt: String::new(),
        model: None,
        effort: None,
        max_budget_usd: None,
        login: Some(token()),
        bash: false,
    };
    let needs = harness.sandbox_needs(&agent).unwrap();
    let login = needs.login.expect("a confined run logs in with the token");
    let temp = tempfile::tempdir().unwrap();
    let config = temp.path().join(HARNESS_CONFIG_DIR);
    fs::create_dir(&config).unwrap();
    seed(&config);
    let paths = RunPaths {
        workdir: workdir.to_path_buf(),
        readable: needs.readable,
        writable: vec![config.clone()],
        temp: Some(temp.path().to_path_buf()),
        ..RunPaths::default()
    };
    let _ = fs::remove_file(workdir.join("result.json"));
    let request = Request {
        workdir: workdir.to_path_buf(),
        model: Some(MODEL.into()),
        effort: Some(Effort::Low),
        permissions: Permissions {
            level: PermissionLevel::ReadOnly,
            network: false,
            browser: false,
        },
        result_path: RelativePath::new("result.json").unwrap(),
        json_schema: None,
        max_budget_usd: budget,
        bash: false,
        session: Some(match session {
            Session::Start(id) => claude::Session::Start(SessionId::new(id).unwrap()),
            Session::Resume(id) => claude::Session::Resume(SessionId::new(id).unwrap()),
        }),
    };
    let built = command(Path::new("claude"), &request).unwrap();
    let mut inner = Command::new(built.get_program());
    inner.args(built.get_args());
    let mut command = agent.confine(inner, &paths).unwrap();
    login.apply(&mut command, &config).unwrap();
    command.env(
        needs.temp_variable.unwrap(),
        temp.path().join(HARNESS_TEMP_DIR),
    );
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().expect("`claude` is on the PATH");
    drop(command);
    let mut lines: Vec<Value> = Vec::new();
    let run = drive(&mut child, prompt, |line| {
        if let Ok(value) = serde_json::from_slice::<Value>(line) {
            lines.push(value);
        }
    })
    .unwrap();
    let init = lines
        .iter()
        .find(|l| l["type"] == "system" && l["subtype"] == "init")
        .cloned()
        .unwrap_or(Value::Null);
    let result_record = lines
        .iter()
        .rev()
        .find(|l| l["type"] == "result")
        .cloned()
        .unwrap_or(Value::Null);
    let reads = lines
        .iter()
        .filter(|l| l["type"] == "assistant")
        .flat_map(|l| {
            l["message"]["content"]
                .as_array()
                .cloned()
                .unwrap_or_default()
        })
        .filter(|c| c["type"] == "tool_use" && c["name"] == "Read")
        .count();
    let result_json = fs::read_to_string(workdir.join("result.json"))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok());
    let mut config_files = Vec::new();
    list(&config, &config, &mut config_files);
    keep(&config);
    assert_eq!(mcp_findings(&run), None, "an MCP server was loaded");
    let stderr = String::from_utf8_lossy(&run.stderr).replace(login.token.expose(), "<redacted>");
    Measured {
        outcome: run.outcome,
        init,
        result_record,
        reads,
        result_json,
        config_files,
        stderr,
    }
}

fn list(root: &Path, dir: &Path, into: &mut Vec<String>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            list(root, &path, into);
        } else {
            let size = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            into.push(format!(
                "{} ({size} B)",
                path.strip_prefix(root).unwrap().display()
            ));
        }
    }
}

/// The session's transcript in a configuration folder, as the adapter
/// finds it: its path relative to the folder.
fn transcript(config: &Path, id: &str) -> Option<PathBuf> {
    transcript_path(config, &SessionId::new(id).unwrap())
        .unwrap()
        .map(PathBuf::from)
}

/// Copies the file `from` to `to`, its folders made.
fn copy_file(from: &Path, to: &Path) {
    fs::create_dir_all(to.parent().unwrap()).unwrap();
    fs::copy(from, to).unwrap();
}

fn print(label: &str, m: &Measured) {
    let r = &m.result_record;
    let usage = &r["usage"];
    println!(
        "{}",
        json!({
            "run": label,
            "completed": matches!(m.outcome, Outcome::Completed { .. }),
            "outcome": format!("{:?}", m.outcome).chars().take(300).collect::<String>(),
            "version": m.init["claude_code_version"],
            "model": m.init["model"],
            "init_session_id": m.init["session_id"],
            "result_session_id": r["session_id"],
            "permission_mode": m.init["permissionMode"],
            "mcp_servers": m.init["mcp_servers"],
            "slash_commands": m.init["slash_commands"],
            "skills": m.init["skills"],
            "api_key_source": m.init["apiKeySource"],
            "reads": m.reads,
            "num_turns": r["num_turns"],
            "duration_ms": r["duration_ms"],
            "total_cost_usd": r["total_cost_usd"],
            "input_tokens": usage["input_tokens"],
            "cache_creation_input_tokens": usage["cache_creation_input_tokens"],
            "cache_read_input_tokens": usage["cache_read_input_tokens"],
            "cache_creation": usage["cache_creation"],
            "output_tokens": usage["output_tokens"],
            "model_usage": r["modelUsage"],
            "result_json": m.result_json,
            "config_files": m.config_files,
            "stderr": m.stderr.chars().take(400).collect::<String>(),
        })
    );
}

/// Where the asking run's session is kept between the runs: the
/// transcript under its relative path, and what B needs to know.
struct Kept {
    work: PathBuf,
    store: PathBuf,
    _dir: Option<tempfile::TempDir>,
}

fn kept_dirs() -> Kept {
    match std::env::var_os("OWLSHIFT_LIVE_SESSION_KEEP") {
        Some(root) => {
            let root = PathBuf::from(root);
            Kept {
                work: root.join("work"),
                store: root.join("store"),
                _dir: None,
            }
        }
        None => {
            let dir = tempfile::tempdir().unwrap();
            Kept {
                work: dir.path().join("work"),
                store: dir.path().join("store"),
                _dir: Some(dir),
            }
        }
    }
}

/// Resumes the kept session from `store` in a fresh configuration folder.
fn resume(kept: &Kept, budget: Option<f64>, label: &str) -> Measured {
    let meta: Value =
        serde_json::from_str(&fs::read_to_string(kept.store.join("meta.json")).unwrap()).unwrap();
    let id = meta["session"].as_str().unwrap().to_owned();
    let relative = PathBuf::from(meta["transcript"].as_str().unwrap());
    let saved = kept.store.join("session");
    let m = run_once(
        &kept.work,
        Session::Resume(&id),
        ANSWER,
        budget,
        |config| copy_file(&saved.join(&relative), &config.join(&relative)),
        |_| {},
    );
    print(label, &m);
    let nonce = meta["nonce"].as_str().unwrap();
    assert!(
        matches!(m.outcome, Outcome::Completed { .. }),
        "{label} did not complete"
    );
    assert_eq!(
        m.result_json.as_ref().and_then(|r| r["nonce"].as_str()),
        Some(nonce),
        "{label} did not carry the asking run's context"
    );
    // The guardrails come from the resumed run's own command line.
    assert_eq!(m.init["mcp_servers"], json!([]), "{label}");
    assert_eq!(m.init["skills"], json!([]), "{label}");
    assert_eq!(m.init["slash_commands"], json!([]), "{label}");
    assert_eq!(m.init["permissionMode"], "dontAsk", "{label}");
    let tools: Vec<&str> = m.init["tools"]
        .as_array()
        .map(|tools| tools.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    for removed in REMOVED_TOOLS {
        assert!(!tools.contains(removed), "{label} was offered {removed}");
    }
    m
}

#[test]
#[ignore = "live: runs `claude -p` four times on your token for agent runs; set OWLSHIFT_LIVE_CLAUDE=1"]
fn a_session_resumes_minutes_later() {
    live();
    let kept = kept_dirs();
    let seed = now_seed();
    let nonce = format!("owl{}", &uuid(&format!("nonce-{seed}"))[..8]);
    scratch(&kept.work, &nonce);
    let id = uuid(&format!("session-{seed}"));

    // A: the asking run, which starts the session under an id of its own.
    let saved = kept.store.join("session");
    let mut found = None;
    let a = run_once(
        &kept.work,
        Session::Start(&id),
        ASK,
        None,
        |_| {},
        |config| {
            found = transcript(config, &id);
            if let Some(relative) = &found {
                copy_file(&config.join(relative), &saved.join(relative));
            }
        },
    );
    print("A ask", &a);
    assert!(matches!(a.outcome, Outcome::Completed { .. }), "A failed");
    let relative = found.expect("A left no transcript named by its session id");
    fs::write(
        kept.store.join("meta.json"),
        json!({
            "session": id,
            "transcript": relative.to_string_lossy(),
            "nonce": nonce,
            "asked_at": jiff::Timestamp::now().to_string(),
            "a_cost_usd": a.result_record["total_cost_usd"],
        })
        .to_string(),
    )
    .unwrap();

    let wait = std::env::var("OWLSHIFT_LIVE_RESUME_WAIT_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(90);
    std::thread::sleep(Duration::from_secs(wait));

    // B: the resume, its budget below A's cost, so a budget counting the
    // whole session would stop it.
    let budget = a.result_record["total_cost_usd"]
        .as_f64()
        .map(|cost| (cost * 0.8 * 100.0).round() / 100.0);
    println!("B budget: {budget:?}, after {wait} s");
    let b = resume(&kept, budget, "B resume");
    // The resumed session reports the whole session's cost, A's included,
    // which the runner counts down to the run's own (OWL-394).
    let cost = |m: &Measured| m.result_record["total_cost_usd"].as_f64().unwrap();
    assert!(
        cost(&b) >= cost(&a),
        "B's total no longer includes A's: the runner's subtraction is wrong"
    );

    // C: a fresh run of the same task, answer given up front.
    let fresh = uuid(&format!("fresh-{seed}"));
    let c = run_once(
        &kept.work,
        Session::Start(&fresh),
        FRESH,
        None,
        |_| {},
        |_| {},
    );
    print("C fresh", &c);

    // D: a session the configuration folder does not hold.
    let missing = uuid(&format!("missing-{seed}"));
    let d = run_once(
        &kept.work,
        Session::Resume(&missing),
        ANSWER,
        None,
        |_| {},
        |_| {},
    );
    print("D missing", &d);
    assert!(
        !matches!(d.outcome, Outcome::Completed { .. }),
        "a resume of a session nobody kept completed"
    );
}

#[test]
#[ignore = "live: resumes the session OWLSHIFT_LIVE_SESSION_KEEP kept; set OWLSHIFT_LIVE_CLAUDE=1"]
fn a_kept_session_resumes_hours_later() {
    live();
    assert!(
        std::env::var_os("OWLSHIFT_LIVE_SESSION_KEEP").is_some(),
        "set OWLSHIFT_LIVE_SESSION_KEEP to the folder the first test kept"
    );
    let kept = kept_dirs();
    let meta: Value =
        serde_json::from_str(&fs::read_to_string(kept.store.join("meta.json")).unwrap()).unwrap();
    println!(
        "asked at {}, resumed at {}",
        meta["asked_at"],
        jiff::Timestamp::now()
    );
    resume(&kept, None, "B' resume later");
}
