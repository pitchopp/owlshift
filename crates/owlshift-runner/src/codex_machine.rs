//! The machine's Codex configuration (OWL-314): the files an administrator
//! writes in `/etc/codex`, which apply to every Codex agent run whatever the
//! adapter's flags (`docs/design/build-plan.md`, OWL-131's "Managed
//! configuration"). Checked with Codex 0.154.0 on 2026-10-15: the MCP
//! servers `config.toml` and `managed_config.toml` declare start in an agent
//! run and their hooks run untrusted, and a `requirements.toml` that does not
//! allow `danger-full-access` stops every Codex run, since the adapter turns
//! Codex's sandbox off.
//!
//! This module reads text only; the caller reads the files, but for
//! [`codex_critic`], which `owlshift do` asks once per command (OWL-303). A
//! value whose shape Codex's own files do not have is never guessed at: it
//! makes the whole file [`Unreadable::Shape`], so a report says it cannot tell
//! what the file does rather than naming a server or a mode that may not be
//! one.

use std::fmt;
use std::path::{Path, PathBuf};

use toml::{Table, Value};

use crate::agent_env::AgentEnv;
use crate::system::System;

/// The machine's Codex configuration.
pub const CONFIG: &str = "/etc/codex/config.toml";

/// The machine's managed Codex configuration.
pub const MANAGED_CONFIG: &str = "/etc/codex/managed_config.toml";

/// What the machine requires of every Codex run.
pub const REQUIREMENTS: &str = "/etc/codex/requirements.toml";

/// The sandbox mode a Codex agent run needs: the adapter turns Codex's own
/// sandbox off, Owlshift confining the run itself (architecture section 5,
/// "The Codex harness").
pub const FULL_ACCESS: &str = "danger-full-access";

/// Codex's sandbox modes.
const SANDBOX_MODES: [&str; 3] = ["read-only", "workspace-write", FULL_ACCESS];

/// Why a file's effect on Codex runs cannot be told, in Owlshift's words:
/// never a piece of the file, which can hold header or variable values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Unreadable {
    /// Not TOML; the line of the error, when the parser gives one.
    Syntax { line: Option<usize> },
    /// TOML, holding a value of a shape Codex's files do not have.
    Shape(&'static str),
}

impl fmt::Display for Unreadable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Syntax { line: Some(line) } => write!(f, "is not valid TOML (line {line})"),
            Self::Syntax { line: None } => f.write_str("is not valid TOML"),
            Self::Shape(what) => write!(f, "holds {what}, a shape Owlshift does not know"),
        }
    }
}

/// What a configuration file declares that reaches every Codex agent run,
/// each list in key order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Declared {
    /// The MCP servers, but those set `enabled = false`.
    pub mcp_servers: Vec<String>,
    /// The events of `[hooks]` that hold at least one hook.
    pub hooks: Vec<String>,
}

impl Declared {
    pub fn is_empty(&self) -> bool {
        self.mcp_servers.is_empty() && self.hooks.is_empty()
    }
}

/// What `config.toml` or `managed_config.toml`, given as `text`, declares.
pub fn declared(text: &str) -> Result<Declared, Unreadable> {
    let table = parse(text)?;
    let mut found = Declared::default();
    match table.get("mcp_servers") {
        None => {}
        Some(Value::Table(servers)) => {
            for (name, server) in servers {
                let Value::Table(server) = server else {
                    return Err(Unreadable::Shape("an MCP server that is not a table"));
                };
                match server.get("enabled") {
                    None | Some(Value::Boolean(true)) => found.mcp_servers.push(name.clone()),
                    Some(Value::Boolean(false)) => {}
                    Some(_) => {
                        return Err(Unreadable::Shape(
                            "an MCP server whose `enabled` is not true or false",
                        ));
                    }
                }
            }
        }
        Some(_) => return Err(Unreadable::Shape("an `mcp_servers` that is not a table")),
    }
    match table.get("hooks") {
        None => {}
        Some(Value::Table(hooks)) => {
            for (event, handlers) in hooks {
                // An event's hooks are an array of tables, as the 2026-10-15
                // probe's `[[hooks.SessionStart]]`; an empty one runs none.
                match handlers {
                    Value::Array(handlers) if handlers.iter().all(Value::is_table) => {
                        if !handlers.is_empty() {
                            found.hooks.push(event.clone());
                        }
                    }
                    _ => {
                        return Err(Unreadable::Shape(
                            "a hook event that is not a list of tables",
                        ));
                    }
                }
            }
        }
        Some(_) => return Err(Unreadable::Shape("a `hooks` that is not a table")),
    }
    Ok(found)
}

/// What `requirements.toml` requires, as far as Codex agent runs go.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Requirements {
    /// `allowed_sandbox_modes`; `None`: every mode is allowed.
    pub allowed_sandbox_modes: Option<Vec<String>>,
    /// `[hooks] managed_dir`, a folder of the administrator's hooks: whether
    /// they run in an agent run was not tried, behind the refusal below.
    pub managed_hooks_dir: Option<String>,
}

impl Requirements {
    /// Whether every Codex agent run stops before it starts: the allowed
    /// modes leave out [`FULL_ACCESS`].
    pub fn refuses_full_access(&self) -> bool {
        self.allowed_sandbox_modes
            .as_ref()
            .is_some_and(|modes| !modes.iter().any(|mode| mode == FULL_ACCESS))
    }
}

/// What `requirements.toml`, given as `text`, requires.
pub fn requirements(text: &str) -> Result<Requirements, Unreadable> {
    let table = parse(text)?;
    let allowed_sandbox_modes = match table.get("allowed_sandbox_modes") {
        None => None,
        Some(Value::Array(modes)) => Some(
            modes
                .iter()
                .map(|mode| match mode {
                    Value::String(mode) if SANDBOX_MODES.contains(&mode.as_str()) => {
                        Ok(mode.clone())
                    }
                    _ => Err(Unreadable::Shape(
                        "an `allowed_sandbox_modes` that is not a list of Codex's sandbox modes",
                    )),
                })
                .collect::<Result<Vec<_>, _>>()?,
        ),
        Some(_) => {
            return Err(Unreadable::Shape(
                "an `allowed_sandbox_modes` that is not a list of Codex's sandbox modes",
            ));
        }
    };
    let managed_hooks_dir = match table.get("hooks") {
        None => None,
        Some(Value::Table(hooks)) => match hooks.get("managed_dir") {
            None => None,
            Some(Value::String(dir)) => Some(dir.clone()),
            Some(_) => {
                return Err(Unreadable::Shape(
                    "a `hooks.managed_dir` that is not a path",
                ));
            }
        },
        Some(_) => return Err(Unreadable::Shape("a `hooks` that is not a table")),
    };
    Ok(Requirements {
        allowed_sandbox_modes,
        managed_hooks_dir,
    })
}

/// What a Codex critic run needs, found on this machine (OWL-303).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodexCritic {
    /// The `codex` program.
    pub program: PathBuf,
    /// The user's login file, which each run copies.
    pub login: PathBuf,
}

/// Whether Codex can run the critic of the review roles on this machine
/// (architecture section 5, "Without Codex"): `codex` on the `PATH`, the
/// login file a run copies ([`crate::executor::codex::user_login`]), and no
/// [`REQUIREMENTS`] that refuses [`FULL_ACCESS`]. Otherwise why not, and the
/// critic stands in on Claude Code. A requirements file that cannot be read
/// or told counts as refusing, unlike `owlshift doctor`'s warning: a run
/// that would stop before it starts is not tried.
pub fn codex_critic(system: &dyn System, agent: &AgentEnv) -> Result<CodexCritic, String> {
    critic_readiness(
        system.locate("codex"),
        crate::executor::codex::user_login(agent),
        |path| system.is_file(path),
        system.read_text_file(Path::new(REQUIREMENTS)),
    )
}

/// [`codex_critic`]'s verdict from what it read: the program found, the
/// login file's place, whether a path is a file, and the requirements file's
/// text, `None` when there is none.
pub fn critic_readiness(
    program: Option<PathBuf>,
    login: Option<PathBuf>,
    is_file: impl Fn(&Path) -> bool,
    requirements_text: Result<Option<String>, String>,
) -> Result<CodexCritic, String> {
    let program = program.ok_or("`codex` is not on the PATH")?;
    let login = login.ok_or("neither `CODEX_HOME` nor `HOME` names Codex's folder")?;
    if !is_file(&login) {
        return Err(format!(
            "Codex has no login file at {}: run `codex login`",
            login.display()
        ));
    }
    match requirements_text {
        Ok(None) => {}
        Ok(Some(text)) => match requirements(&text) {
            Ok(required) if !required.refuses_full_access() => {}
            Ok(_) => {
                return Err(format!(
                    "{REQUIREMENTS} does not allow `{FULL_ACCESS}`, which a Codex agent run needs"
                ));
            }
            Err(error) => return Err(format!("{REQUIREMENTS} {error}")),
        },
        Err(error) => return Err(format!("{REQUIREMENTS} could not be read: {error}")),
    }
    Ok(CodexCritic { program, login })
}

/// `text` as a TOML table; an error keeps only its line.
fn parse(text: &str) -> Result<Table, Unreadable> {
    text.parse::<Table>().map_err(|error| Unreadable::Syntax {
        line: error.span().map(|span| {
            let before = &text.as_bytes()[..span.start.min(text.len())];
            before.iter().filter(|&&byte| byte == b'\n').count() + 1
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// OWL-303: the critic runs on Codex only with the program, its login
    /// file and requirements that let a run start.
    #[test]
    fn codex_runs_the_critic_only_when_installed_logged_in_and_allowed() {
        let program = || Some(PathBuf::from("/bin/codex"));
        let login = || Some(PathBuf::from("/home/me/.codex/auth.json"));
        let ready = CodexCritic {
            program: PathBuf::from("/bin/codex"),
            login: PathBuf::from("/home/me/.codex/auth.json"),
        };
        let allows = r#"allowed_sandbox_modes = ["read-only", "danger-full-access"]"#;
        let refuses = r#"allowed_sandbox_modes = ["read-only"]"#;
        let verdict = |program, login, file: bool, text: Result<Option<&str>, &str>| {
            critic_readiness(
                program,
                login,
                |_| file,
                text.map(|t| t.map(str::to_owned)).map_err(str::to_owned),
            )
        };
        assert_eq!(verdict(program(), login(), true, Ok(None)), Ok(ready.clone()));
        assert_eq!(verdict(program(), login(), true, Ok(Some(allows))), Ok(ready));
        let refused = [
            (verdict(None, login(), true, Ok(None)), "not on the PATH"),
            (verdict(program(), None, true, Ok(None)), "names Codex's folder"),
            (verdict(program(), login(), false, Ok(None)), "no login file"),
            (
                verdict(program(), login(), true, Ok(Some(refuses))),
                "does not allow",
            ),
            (
                verdict(program(), login(), true, Ok(Some("allowed_sandbox_modes = 1"))),
                "a shape Owlshift does not know",
            ),
            (
                verdict(program(), login(), true, Err("permission denied")),
                "could not be read",
            ),
        ];
        for (verdict, why) in refused {
            assert!(verdict.as_ref().is_err_and(|e| e.contains(why)), "{why}: {verdict:?}");
        }
    }

    #[test]
    fn servers_and_hooks_are_named_in_key_order_but_disabled_servers() {
        let found = declared(
            r#"
model = "o3"

[mcp_servers.zeta]
command = "zeta-mcp"

[mcp_servers.alpha]
url = "https://mcp.example.com"
http_headers = { Authorization = "Bearer sk-secret" }

[mcp_servers.off]
command = "off-mcp"
enabled = false

[[hooks.SessionStart]]
hooks = [{ type = "command", command = "touch /tmp/mark" }]

[[hooks.PreToolUse]]
hooks = [{ type = "command", command = "true" }]
"#,
        )
        .unwrap();
        assert_eq!(found.mcp_servers, ["alpha", "zeta"]);
        assert_eq!(found.hooks, ["PreToolUse", "SessionStart"]);
        assert!(!found.is_empty());
        assert!(declared("model = \"o3\"\n").unwrap().is_empty());
        assert!(declared("").unwrap().is_empty());
    }

    #[test]
    fn a_shape_codex_does_not_have_is_never_guessed_at() {
        for (text, what) in [
            (
                "mcp_servers = false",
                "an `mcp_servers` that is not a table",
            ),
            (
                "[mcp_servers]\nsearch = \"command\"",
                "an MCP server that is not a table",
            ),
            (
                "[mcp_servers.search]\nenabled = \"false\"",
                "an MCP server whose `enabled` is not true or false",
            ),
            ("hooks = \"invalid\"", "a `hooks` that is not a table"),
        ] {
            assert_eq!(declared(text), Err(Unreadable::Shape(what)), "{text}");
        }
        for text in [
            "allowed_sandbox_modes = \"read-only\"",
            "allowed_sandbox_modes = [\"danger-full-access\", \"read_only\"]",
            "allowed_sandbox_modes = [1]",
            "hooks = 1",
            "[hooks]\nmanaged_dir = 1",
        ] {
            assert!(
                matches!(requirements(text), Err(Unreadable::Shape(_))),
                "{text}"
            );
        }
    }

    /// A hook event is named only when it holds hooks of Codex's shape.
    #[test]
    fn a_hook_event_must_be_a_list_of_tables() {
        for text in [
            "[hooks]\nSessionStart = false",
            "[hooks]\nSessionStart = [\"touch /tmp/mark\"]",
        ] {
            assert_eq!(
                declared(text),
                Err(Unreadable::Shape(
                    "a hook event that is not a list of tables"
                )),
                "{text}"
            );
        }
        assert!(declared("[hooks]\nSessionStart = []").unwrap().is_empty());
    }

    #[test]
    fn invalid_toml_keeps_only_its_line() {
        let text =
            "[mcp_servers.alpha]\nurl = \"https://mcp.example.com\"\nbearer = sk-secret-value\n";
        let error = declared(text).unwrap_err();
        assert_eq!(error, Unreadable::Syntax { line: Some(3) });
        assert_eq!(error.to_string(), "is not valid TOML (line 3)");
        assert!(matches!(
            requirements("allowed_sandbox_modes = [").unwrap_err(),
            Unreadable::Syntax { line: Some(1) }
        ));
    }

    #[test]
    fn requirements_refuse_full_access_unless_they_allow_it_or_say_nothing() {
        let read_only = requirements(
            "allowed_sandbox_modes = [\"read-only\"]\n\n[hooks]\nmanaged_dir = \"/etc/codex/hooks\"\n",
        )
        .unwrap();
        assert!(read_only.refuses_full_access());
        assert_eq!(
            read_only.managed_hooks_dir.as_deref(),
            Some("/etc/codex/hooks")
        );
        assert!(
            requirements("allowed_sandbox_modes = []")
                .unwrap()
                .refuses_full_access()
        );
        let allowed =
            requirements("allowed_sandbox_modes = [\"read-only\", \"danger-full-access\"]")
                .unwrap();
        assert!(!allowed.refuses_full_access());
        assert_eq!(allowed.managed_hooks_dir, None);
        let silent = requirements("allowed_approval_policies = [\"never\"]").unwrap();
        assert_eq!(silent, Requirements::default());
        assert!(!silent.refuses_full_access());
    }
}
