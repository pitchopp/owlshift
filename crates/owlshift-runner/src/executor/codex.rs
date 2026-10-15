//! Codex, run with `codex exec` as the critic of a review pass, through
//! `owlshift_adapters::harness::codex` (architecture section 5, "The Codex
//! harness").
//!
//! The run is confined by the read-only profile (OWL-298) with Codex's own
//! sandbox off, and refused when it cannot be confined. Its `CODEX_HOME` is
//! a folder of the run's own holding only a copy of the user's `auth.json`
//! ([`SandboxNeeds::home`]): the user's `config.toml`, `.env`, `AGENTS.md`
//! and skills stay out, and nothing the run writes there outlives it but a
//! refreshed login that [`codex_login_kept`] accepts. Codex cannot write
//! `result.json`: the run's last message is the result, which the executor
//! writes once the run is over ([`SandboxNeeds::result_in_message`]).

use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};

use serde_json::Value;

use owlshift_adapters::harness::codex::{self, Effort};
use owlshift_platform::confined::read_confined;
use owlshift_platform::keychain::Secret;

use super::harness::{
    Harness, HarnessEnd, HarnessError, HarnessHome, HarnessRun, HarnessStatus, SandboxNeeds,
    program_folders,
};
use super::{MAX_RESULT_BYTES, RunLog};
use crate::agent_env::AgentEnv;

/// The variable naming Codex's home folder.
pub const CODEX_HOME_ENV: &str = "CODEX_HOME";

/// Codex's login file in its home folder.
pub const LOGIN_FILE: &str = "auth.json";

/// The output schema's file in the run's own files.
pub const SCHEMA_FILE: &str = "schema.json";

/// Where Codex writes its last message (`-o`), in the run's own files.
pub const LAST_MESSAGE_FILE: &str = "last-message.json";

/// Codex as the critic. Nothing runs it in the pipeline yet: Design review
/// and Verify take it up with OWL-303.
#[derive(Clone, Debug, PartialEq)]
pub struct CodexHarness {
    /// The `codex` program.
    pub program: PathBuf,
    /// The role's prompt; the harness adds where the brief is and how the
    /// run ends.
    pub prompt: String,
    pub model: Option<String>,
    pub effort: Option<Effort>,
    /// The user's Codex login file, copied into the run's home
    /// ([`user_login`]).
    pub login: PathBuf,
}

/// The user's Codex login file: `auth.json` in the agent environment's
/// `CODEX_HOME`, or in `$HOME/.codex`. `None` without either variable.
pub fn user_login(agent: &AgentEnv) -> Option<PathBuf> {
    login_file(agent.var(CODEX_HOME_ENV), agent.var("HOME").map(Path::new))
}

/// Codex's login file given its `CODEX_HOME` and the user's home folder, as
/// Codex resolves them: `auth.json` in `CODEX_HOME`, or in `<home>/.codex`.
/// `owlshift doctor` looks for the same file (OWL-299).
pub fn login_file(codex_home: Option<&OsStr>, home: Option<&Path>) -> Option<PathBuf> {
    let folder = match codex_home {
        Some(folder) if !folder.is_empty() => PathBuf::from(folder),
        _ => home?.join(".codex"),
    };
    Some(folder.join(LOGIN_FILE))
}

/// Why a Codex run cannot start.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CodexRefusal {
    /// The agent environment is not confined: with its own sandbox off,
    /// nothing would keep Codex from writing the worktree.
    Unconfined,
    /// No login file where Codex keeps it.
    NoLogin(PathBuf),
}

impl fmt::Display for CodexRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unconfined => f.write_str(
                "Codex runs only inside Owlshift's read-only confinement, which keeps it from \
                 writing the worktree once its own sandbox is off; this agent environment is \
                 not confined",
            ),
            Self::NoLogin(path) => write!(
                f,
                "Codex has no login file at {}: run `codex login`. Owlshift copies that file \
                 into each run, so a login Codex keeps elsewhere, such as in the system \
                 keyring, is not used",
                path.display()
            ),
        }
    }
}

impl std::error::Error for CodexRefusal {}

impl Harness for CodexHarness {
    fn command(&self, run: &HarnessRun<'_>) -> Result<Command, HarnessError> {
        let files = run_files(run)?;
        let schema = files.join(SCHEMA_FILE);
        let rendered = serde_json::to_string_pretty(&codex::review_schema())?;
        fs::write(&schema, rendered)?;
        let request = codex::Request {
            workdir: run.worktree.to_owned(),
            model: self.model.clone(),
            effort: self.effort,
            role: run.brief.role,
            permissions: run.brief.permissions.clone(),
            schema_file: schema,
            last_message_file: files.join(LAST_MESSAGE_FILE),
        };
        Ok(codex::command(&self.program, &request)?)
    }

    /// Drives Codex, then takes its last message as the result only when
    /// `-o`'s file holds the same text as the stream's last agent message:
    /// the stream comes from Codex itself, the file from a folder the run's
    /// shell can write.
    fn drive(
        &self,
        run: &HarnessRun<'_>,
        child: &mut Child,
        log: &mut RunLog,
    ) -> io::Result<HarnessEnd> {
        let prompt = prompt_with_brief_file(&self.prompt, run.brief_file);
        let ran = codex::drive(child, &prompt, |line| log.stdout(line))?;
        log.stderr(&ran.stderr);
        let mut end = codex_end(ran, self.model.as_deref());
        if let (HarnessStatus::Completed, Some(text)) = (end.status.clone(), end.message.take()) {
            match last_message(run) {
                Some(written) if written.trim_end() == text.trim_end() => end.message = Some(text),
                Some(_) => {
                    end.status = HarnessStatus::Failed(
                        "Codex's `-o` file does not hold its last message".to_owned(),
                    );
                }
                None => {
                    end.status =
                        HarnessStatus::Failed("Codex wrote no readable `-o` file".to_owned());
                }
            }
        }
        Ok(end)
    }

    /// Confined only; the folders `codex` is installed in, read; a home of
    /// the run's own seeded with the user's login; the read-only profile;
    /// the result from the last message.
    fn sandbox_needs(&self, agent: &AgentEnv) -> Result<SandboxNeeds, HarnessError> {
        if !agent.is_confined() {
            return Err(CodexRefusal::Unconfined.into());
        }
        if !self.login.is_file() {
            return Err(CodexRefusal::NoLogin(self.login.clone()).into());
        }
        Ok(SandboxNeeds {
            readable: program_folders(&self.program, agent),
            home: Some(HarnessHome {
                variable: CODEX_HOME_ENV,
                login: self.login.clone(),
            }),
            read_only: true,
            result_in_message: true,
            ..SandboxNeeds::default()
        })
    }

    /// The login's tokens and key: the critic's shell can read the copy
    /// (C2, finding 3), so they are hidden in the logs and refused in the
    /// result.
    fn login_secrets(&self, login: &[u8]) -> Vec<Secret> {
        let Ok(login) = serde_json::from_slice::<Value>(login) else {
            return Vec::new();
        };
        let tokens = &login["tokens"];
        ["id_token", "access_token", "refresh_token"]
            .into_iter()
            .filter_map(|key| tokens[key].as_str())
            .chain(login["OPENAI_API_KEY"].as_str())
            .filter(|value| !value.is_empty())
            .map(Secret::new)
            .collect()
    }

    fn login_kept(&self, original: &[u8], updated: &[u8]) -> Result<(), String> {
        codex_login_kept(original, updated)
    }
}

/// Whether a Codex login the run changed may replace the user's: it must
/// still be a ChatGPT login (`auth_mode` `chatgpt`, no `OPENAI_API_KEY`) of
/// the same account (`tokens.account_id`), with its three tokens, and add no
/// key the user's file did not have. The shape is the one of codex-cli
/// 0.154.0's `auth.json`, read on 2026-10-15 (key names only): `auth_mode`,
/// `OPENAI_API_KEY`, `tokens` (`id_token`, `access_token`, `refresh_token`,
/// `account_id`), `last_refresh`. A Codex release that adds a key makes a
/// refresh dropped, with its warning, until this follows.
pub fn codex_login_kept(original: &[u8], updated: &[u8]) -> Result<(), String> {
    let parse = |bytes: &[u8], which: &str| {
        serde_json::from_slice::<Value>(bytes)
            .ok()
            .filter(Value::is_object)
            .ok_or_else(|| format!("the {which} login is not a JSON object"))
    };
    let original = parse(original, "user's")?;
    let updated = parse(updated, "run's")?;
    let account = |login: &Value| {
        login["tokens"]["account_id"]
            .as_str()
            .filter(|id| !id.is_empty())
            .map(str::to_owned)
    };
    let chatgpt = |login: &Value| {
        login["auth_mode"] == "chatgpt"
            && login.get("OPENAI_API_KEY").is_none_or(Value::is_null)
            && ["id_token", "access_token", "refresh_token"]
                .iter()
                .all(|key| login["tokens"][key].as_str().is_some_and(|t| !t.is_empty()))
    };
    if !chatgpt(&original) {
        return Err("the user's login is not a ChatGPT login, which Codex never refreshes".into());
    }
    if !chatgpt(&updated) {
        return Err("it is no longer a ChatGPT login".into());
    }
    if account(&original).is_none() || account(&original) != account(&updated) {
        return Err("it is not a login of the same account".into());
    }
    let added = |at: &dyn Fn(&Value) -> &Value| {
        let known = at(&original).as_object().cloned().unwrap_or_default();
        at(&updated)
            .as_object()
            .is_some_and(|keys| keys.keys().any(|key| !known.contains_key(key)))
    };
    if added(&|login| login) || added(&|login| &login["tokens"]) {
        return Err("it holds a key the user's login does not".into());
    }
    Ok(())
}

/// The run's own files: the folder of its result under the read-only
/// profile, which every Codex run has.
fn run_files<'a>(run: &HarnessRun<'a>) -> io::Result<&'a Path> {
    run.result_file
        .parent()
        .ok_or_else(|| io::Error::other("the run's result file has no folder"))
}

/// `-o`'s file, links refused and its size capped; `None` when it is not
/// there, not text or refused.
fn last_message(run: &HarnessRun<'_>) -> Option<String> {
    let files = run_files(run).ok()?;
    let bytes = read_confined(files, LAST_MESSAGE_FILE, MAX_RESULT_BYTES).ok()?;
    String::from_utf8(bytes).ok()
}

/// The role's prompt, then where its brief is and how the run ends: a
/// Codex run cannot write `result.json`, so its last message is the result.
pub fn prompt_with_brief_file(prompt: &str, brief_file: &Path) -> String {
    format!(
        "{}\n\nThe brief of this run is the file `{}`. This harness gives you no way to write \
         `result.json`: do not write it, and make your last message that JSON, whole, as the \
         output schema describes, with nothing after it.\n",
        prompt.trim_end(),
        brief_file.display()
    )
}

/// What a Codex run reported, as the executor reads it. Codex names neither
/// its version nor, by default, its model in the stream (C2): the model is
/// the one asked for, if any.
fn codex_end(run: codex::Run, model: Option<&str>) -> HarnessEnd {
    let (status, message) = match run.outcome {
        codex::Outcome::Completed { text } => (HarnessStatus::Completed, Some(text)),
        codex::Outcome::UsageLimit => (HarnessStatus::UsageLimit { resets_at: None }, None),
        codex::Outcome::Failed(failure) => (HarnessStatus::Failed(describe(&failure)), None),
    };
    let mut end = HarnessEnd::new(run.exit_code, status);
    end.usage = run.usage;
    end.model = model.map(str::to_owned);
    end.message = message;
    end
}

/// The most characters of Codex's failure message put in the reason.
const MESSAGE_CAP: usize = 300;

fn describe(failure: &codex::Failure) -> String {
    match failure {
        codex::Failure::NoThread => {
            "Codex stopped before its thread started (see the run's standard error)".to_owned()
        }
        codex::Failure::Unfinished => "Codex's stream ended without the turn's end".to_owned(),
        codex::Failure::Signal => "a signal ended Codex".to_owned(),
        codex::Failure::PromptNotDelivered => "the prompt could not be sent".to_owned(),
        codex::Failure::Error {
            exit_code,
            api_status,
            message,
        } => {
            let mut text = format!("Codex reported an error (exit code {exit_code:?}");
            if let Some(status) = api_status {
                text.push_str(&format!(", API status {status}"));
            }
            text.push(')');
            if let Some(message) = message {
                let cut: String = message.chars().take(MESSAGE_CAP).collect();
                text.push_str(&format!(": {cut}"));
            }
            if *api_status == Some(401) {
                text.push_str("; the API refused the login: run `codex login`");
            }
            text
        }
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use serde_json::json;

    use super::*;

    fn login(account: &str) -> Value {
        json!({
            "auth_mode": "chatgpt",
            "OPENAI_API_KEY": null,
            "tokens": {
                "id_token": "id-SENTINEL",
                "access_token": "access-SENTINEL",
                "refresh_token": "refresh-SENTINEL",
                "account_id": account,
            },
            "last_refresh": "2026-10-15T12:00:00Z",
        })
    }

    fn kept(original: &Value, updated: &Value) -> Result<(), String> {
        codex_login_kept(
            original.to_string().as_bytes(),
            updated.to_string().as_bytes(),
        )
    }

    #[test]
    fn a_refreshed_login_of_the_same_account_is_kept() {
        let mut refreshed = login("acct-1");
        refreshed["tokens"]["access_token"] = json!("access-NEW");
        refreshed["last_refresh"] = json!("2026-10-16T12:00:00Z");
        assert_eq!(kept(&login("acct-1"), &refreshed), Ok(()));
    }

    #[test]
    fn any_other_change_is_refused() {
        let original = login("acct-1");
        let mut cases = vec![
            ("same account", login("acct-2")),
            ("ChatGPT", {
                let mut l = login("acct-1");
                l["OPENAI_API_KEY"] = json!("sk-planted");
                l
            }),
            ("ChatGPT", {
                let mut l = login("acct-1");
                l["auth_mode"] = json!("apikey");
                l
            }),
            ("ChatGPT", {
                let mut l = login("acct-1");
                l["tokens"]["refresh_token"] = json!("");
                l
            }),
            ("key", {
                let mut l = login("acct-1");
                l["base_url"] = json!("https://example.test");
                l
            }),
            ("key", {
                let mut l = login("acct-1");
                l["tokens"]["extra"] = json!("x");
                l
            }),
        ];
        cases.push(("same account", {
            let mut l = login("acct-1");
            l["tokens"].as_object_mut().unwrap().remove("account_id");
            l
        }));
        for (why, updated) in cases {
            let refused = kept(&original, &updated).unwrap_err();
            assert!(refused.contains(why), "{why}: {refused}");
        }
        let refused = codex_login_kept(original.to_string().as_bytes(), b"{\"auth_mo").unwrap_err();
        assert!(refused.contains("not a JSON object"), "{refused}");
        // An API-key login is never written back.
        let key = json!({"auth_mode": "apikey", "OPENAI_API_KEY": "sk-x", "tokens": null});
        assert!(kept(&key, &key).unwrap_err().contains("never refreshes"));
    }

    #[test]
    fn the_logins_tokens_are_secrets_of_the_run() {
        let harness = CodexHarness {
            program: PathBuf::from("codex"),
            prompt: String::new(),
            model: None,
            effort: None,
            login: PathBuf::from("/nowhere/auth.json"),
        };
        let secrets = harness.login_secrets(login("acct-1").to_string().as_bytes());
        let values: Vec<&str> = secrets.iter().map(Secret::expose).collect();
        assert_eq!(
            values,
            ["id-SENTINEL", "access-SENTINEL", "refresh-SENTINEL"]
        );
        assert!(harness.login_secrets(b"not json").is_empty());
    }

    #[test]
    fn the_users_login_is_found_where_codex_looks() {
        let env = |vars: &[(&str, &str)]| {
            AgentEnv::new(
                vars.iter()
                    .map(|(k, v)| (OsString::from(k), OsString::from(v)))
                    .collect::<Vec<_>>(),
            )
            .unwrap()
        };
        assert_eq!(
            user_login(&env(&[("HOME", "/home/op"), ("CODEX_HOME", "/data/codex")])),
            Some(PathBuf::from("/data/codex/auth.json"))
        );
        assert_eq!(
            user_login(&env(&[("HOME", "/home/op")])),
            Some(PathBuf::from("/home/op/.codex/auth.json"))
        );
        assert_eq!(user_login(&env(&[("PATH", "/usr/bin")])), None);
    }

    #[test]
    fn an_unconfined_or_logged_out_codex_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mut harness = CodexHarness {
            program: PathBuf::from("codex"),
            prompt: String::new(),
            model: None,
            effort: None,
            login: dir.path().join("auth.json"),
        };
        let agent = AgentEnv::new([(OsString::from("PATH"), OsString::from("/usr/bin"))]).unwrap();
        let bare = agent.clone().without_confinement();
        let refused = harness.sandbox_needs(&bare).unwrap_err().to_string();
        assert!(refused.contains("read-only confinement"), "{refused}");
        let refused = harness.sandbox_needs(&agent).unwrap_err().to_string();
        assert!(refused.contains("`codex login`"), "{refused}");

        fs::write(&harness.login, "{}").unwrap();
        let needs = harness.sandbox_needs(&agent).unwrap();
        assert!(needs.read_only && needs.result_in_message);
        assert_eq!(
            needs.home,
            Some(HarnessHome {
                variable: "CODEX_HOME",
                login: harness.login.clone()
            })
        );
        assert_eq!(needs.login, None);
        harness.login = dir.path().to_owned();
        assert!(
            harness.sandbox_needs(&agent).is_err(),
            "a folder is no login"
        );
    }

    #[test]
    fn a_failure_says_what_codex_said_and_a_refused_login_names_the_fix() {
        let error = |status, message: &str| codex::Failure::Error {
            exit_code: Some(1),
            api_status: status,
            message: Some(message.to_owned()),
        };
        let text = describe(&error(Some(401), "unexpected status 401 Unauthorized"));
        assert!(
            text.contains("API status 401") && text.contains("`codex login`"),
            "{text}"
        );
        let long = "x".repeat(1000);
        let text = describe(&error(Some(400), &long));
        assert!(!text.contains("codex login"), "{text}");
        assert!(text.len() < 400, "{}", text.len());
        assert!(describe(&codex::Failure::NoThread).contains("standard error"));
    }

    #[test]
    fn the_prompt_ends_with_the_brief_and_the_last_message() {
        let prompt = prompt_with_brief_file("# Verify\n\n", Path::new("/tmp/run/brief.json"));
        assert!(
            prompt.starts_with(
                "# Verify\n\nThe brief of this run is the file `/tmp/run/brief.json`."
            )
        );
        assert!(prompt.contains("do not write it"));
    }
}
