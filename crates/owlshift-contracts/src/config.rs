//! Project and personal configuration, in TOML.
//!
//! Only the keys the design names so far exist; later work adds keys, and a
//! project file's `requires` tells an older binary to upgrade. Read
//! `requires` with [`peek_requires`] before the strict parse, so an old binary
//! reports "upgrade" rather than an unknown key.

use std::collections::BTreeMap;
use std::num::NonZeroU32;

use owlshift_core::agent_env::{check_declared, check_names};
use schemars::JsonSchema;
use semver::{Version, VersionReq};
use serde::{Deserialize, Serialize};

use crate::format::ContractError;
use crate::{Harness, PlanApproval, Variant};

const PROJECT: &str = "owlshift.toml";
const PERSONAL: &str = "personal configuration";

/// The project file, `owlshift.toml`, committed in the repository.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(title = "Owlshift project configuration (owlshift.toml)")]
pub struct ProjectConfig {
    /// The Owlshift versions that can read this file: a semantic version
    /// requirement such as `>=0.4`.
    #[schemars(with = "String")]
    pub requires: VersionReq,
    pub tracker: Tracker,
    pub stack: Stack,
    pub pipeline: Pipeline,
    pub models: Models,
    pub policy: Policy,
}

/// The tracker and how Owlshift maps onto it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Tracker {
    pub kind: TrackerKind,
    /// The tracker's team key; required for Linear.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team: Option<String>,
    /// The gesture by which a person admits a ticket to the queue.
    pub admit: Admit,
    /// The tracker states that show a ticket's visible stage.
    pub states: States,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TrackerKind {
    Linear,
    Markdown,
}

/// The admission gesture: delegating the ticket to Owlshift, a label, or a
/// state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Admit {
    /// The ticket is delegated to Owlshift's agent identity.
    Delegation,
    /// The ticket carries this label.
    Label(String),
    /// The ticket is in this state.
    State(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct States {
    pub ready: String,
    pub working: String,
    pub needs_input: String,
    pub review: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Stack {
    /// The project's full gate, run before delivery.
    pub gate: Vec<String>,
    /// Named resources and the path globs they cover.
    #[serde(default)]
    pub resources: BTreeMap<String, String>,
    /// Names of variables of the runner's environment the gate needs, such as
    /// a feature flag. Names only: each takes its value, as is, from the
    /// runner's environment, and one the runner lacks is not set. They reach
    /// the whole agent run, the harness and the commands it starts as well as
    /// the gate. The sandbox does not follow them: a path under the home is
    /// readable only if the run already opens it (the known tool-chain
    /// folders and the `PATH`'s folders under the home), so a tool chain
    /// installed elsewhere under the home stays unreadable. A credential
    /// variable, a `GIT_` name, a variable Owlshift overrides or one that
    /// makes the dynamic loader load code is refused, and so is a name the
    /// operator does not allow in the personal `allow_gate_env`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gate_env: Vec<String>,
    /// The files whose text reaches agents as the project's rules, paths
    /// from the repository's root with `/` between folders, in order. Each is
    /// read at the base commit, taken whole, and must exist there; a blank
    /// file gives no rule. Files only, at most 32, and 64 KiB for all of them
    /// together. Left out, the rules are the root `AGENTS.md` when the base
    /// commit has one; `[]` gives no rule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rules: Option<Vec<String>>,
}

/// The most files [`Stack::rules`] may name.
pub const MAX_RULE_FILES: usize = 32;

impl Stack {
    /// The names of [`Stack::gate_env`], as the agent environment takes them.
    pub fn gate_env_names(&self) -> Vec<&str> {
        self.gate_env.iter().map(String::as_str).collect()
    }

    /// Checks the paths of [`Stack::rules`]: each a path from the
    /// repository's root, `/` between its parts, none empty, `.` or `..`,
    /// no `\` or control character, none named twice (compared as written),
    /// and at most [`MAX_RULE_FILES`].
    fn check_rules(&self) -> Result<(), String> {
        let Some(paths) = &self.rules else {
            return Ok(());
        };
        if paths.len() > MAX_RULE_FILES {
            return Err(format!(
                "{} files named, over the {MAX_RULE_FILES} Owlshift takes as rules",
                paths.len()
            ));
        }
        for (i, path) in paths.iter().enumerate() {
            if path.chars().any(char::is_control) {
                return Err(format!(
                    "{path:?} holds a control character: name a file by its path"
                ));
            }
            if path.contains('\\') {
                return Err(format!("{path:?} holds `\\`: separate folders with `/`"));
            }
            if path.starts_with('/') {
                return Err(format!(
                    "{path:?} starts with `/`: name a file from the repository's root"
                ));
            }
            if path.split('/').any(|part| matches!(part, "" | "." | "..")) {
                return Err(format!(
                    "{path:?} is not a file's path: no part of it may be empty, `.` or `..`"
                ));
            }
            if paths[..i].contains(path) {
                return Err(format!("{path:?} is named twice"));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Pipeline {
    pub default: Variant,
    pub plan_approval: PlanApproval,
}

/// The model for each tier, per harness.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Models {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deep: Option<TierModels>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub standard: Option<TierModels>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fast: Option<TierModels>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TierModels {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    /// Question categories always decided by a human, added to the floor.
    pub always_human: Vec<String>,
}

impl ProjectConfig {
    /// Parses and validates `owlshift.toml`.
    pub fn parse(input: &str) -> Result<Self, ContractError> {
        let config: Self = toml::from_str(input).map_err(|source| ContractError::Toml {
            contract: PROJECT,
            source,
        })?;
        config.validate()?;
        Ok(config)
    }

    /// Renders `owlshift.toml`, without comments.
    pub fn render(&self) -> String {
        toml::to_string(self).expect("contract types always serialize to TOML")
    }

    /// Checks what the file alone decides. Whether the operator allows the
    /// names of `stack.gate_env` depends on the personal file:
    /// [`Self::check_gate_env`] checks that where both are known.
    pub fn validate(&self) -> Result<(), ContractError> {
        if self.tracker.kind == TrackerKind::Linear && self.tracker.team.is_none() {
            return Err(ContractError::invalid(
                PROJECT,
                "tracker.team is required for Linear",
            ));
        }
        self.stack
            .check_rules()
            .map_err(|reason| ContractError::invalid(PROJECT, format!("stack.rules: {reason}")))?;
        check_names(&self.stack.gate_env_names()).map_err(gate_env_error)
    }

    /// Checks `stack.gate_env` against the names the operator allows, the
    /// personal `allow_gate_env`: the rule the agent environment applies
    /// (`owlshift_core::agent_env::check_declared`, OWL-63).
    pub fn check_gate_env(&self, allowed: &[&str]) -> Result<(), ContractError> {
        check_declared(&self.stack.gate_env_names(), allowed).map_err(gate_env_error)
    }
}

fn gate_env_error(error: owlshift_core::agent_env::AgentEnvError) -> ContractError {
    ContractError::invalid(PROJECT, format!("stack.gate_env: {error}"))
}

/// The personal file, in the user's configuration directory, never committed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(title = "Owlshift personal configuration")]
pub struct PersonalConfig {
    /// The Owlshift versions that can read this file: a semantic version
    /// requirement such as `>=0.4`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Option<String>")]
    pub requires: Option<VersionReq>,
    #[serde(default)]
    pub identity: Identity,
    /// The installed harnesses.
    #[serde(default)]
    pub harnesses: Harnesses,
    /// How many runs this machine runs at once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub concurrent_runs: Option<NonZeroU32>,
    /// Whether to keep the machine awake while runs are in flight.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_awake: Option<bool>,
    /// Names of variables of this machine's environment that a project may
    /// pass to its agents by declaring them in `stack.gate_env`, in any
    /// letter case. A project's declaration never widens this list: a name
    /// declared but not listed here is refused. It applies to every project
    /// run on this machine. A credential variable, a `GIT_` name, a variable
    /// Owlshift overrides or one that makes the dynamic loader load code
    /// cannot be listed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow_gate_env: Vec<String>,
}

/// The user's accounts on the tracker and the forge.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Identity {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tracker: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forge: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Harnesses {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claude: Option<HarnessSettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex: Option<HarnessSettings>,
}

impl Harnesses {
    /// The settings of each installed harness.
    pub fn iter(&self) -> impl Iterator<Item = (Harness, &HarnessSettings)> {
        [
            (Harness::Claude, self.claude.as_ref()),
            (Harness::Codex, self.codex.as_ref()),
        ]
        .into_iter()
        .filter_map(|(harness, settings)| settings.map(|s| (harness, s)))
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HarnessSettings {
    /// The harness to use when this one is at its usage limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback: Option<Harness>,
    /// A dollar budget, only for a harness configured for API billing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 0))]
    pub budget_usd: Option<f64>,
}

impl PersonalConfig {
    /// Parses and validates the personal file.
    pub fn parse(input: &str) -> Result<Self, ContractError> {
        let config: Self = toml::from_str(input).map_err(|source| ContractError::Toml {
            contract: PERSONAL,
            source,
        })?;
        config.validate()?;
        Ok(config)
    }

    /// Renders the personal file, without comments.
    pub fn render(&self) -> String {
        toml::to_string(self).expect("contract types always serialize to TOML")
    }

    /// The names of [`PersonalConfig::allow_gate_env`].
    pub fn allow_gate_env_names(&self) -> Vec<&str> {
        self.allow_gate_env.iter().map(String::as_str).collect()
    }

    pub fn validate(&self) -> Result<(), ContractError> {
        check_names(&self.allow_gate_env_names()).map_err(|error| {
            ContractError::invalid(PERSONAL, format!("allow_gate_env: {error}"))
        })?;
        for (harness, settings) in self.harnesses.iter() {
            if settings.fallback == Some(harness) {
                return Err(ContractError::invalid(
                    PERSONAL,
                    format!("harness {harness:?} falls back to itself"),
                ));
            }
            if let Some(budget) = settings.budget_usd
                && !(budget.is_finite() && budget >= 0.0)
            {
                return Err(ContractError::invalid(
                    PERSONAL,
                    format!("harness {harness:?}: budget_usd must be a number >= 0"),
                ));
            }
        }
        Ok(())
    }
}

/// Reads a configuration file's `requires` leniently, ignoring every other
/// key, so a binary can tell its user to upgrade before the strict parse
/// fails on keys a newer version added.
pub fn peek_requires(input: &str) -> Result<Option<VersionReq>, ContractError> {
    #[derive(Deserialize)]
    struct Peek {
        requires: Option<VersionReq>,
    }
    toml::from_str::<Peek>(input)
        .map(|peek| peek.requires)
        .map_err(|source| ContractError::Toml {
            contract: CONFIGURATION,
            source,
        })
}

const CONFIGURATION: &str = "configuration";

/// Checks a configuration file's `requires` against the running Owlshift
/// version. Call it before the strict parse: a file written for a newer
/// Owlshift is then refused with "upgrade", not with an unknown key.
pub fn check_requires(input: &str, current: &Version) -> Result<(), ContractError> {
    match peek_requires(input)? {
        Some(requires) if !requires.matches(current) => Err(ContractError::invalid(
            CONFIGURATION,
            format!(
                "it requires Owlshift {requires}; this is Owlshift {current}: upgrade Owlshift"
            ),
        )),
        _ => Ok(()),
    }
}

/// Every key a configuration file sets, as a dotted path and its value in
/// TOML syntax, in key order.
///
/// It reads the document as written, so a value the parser would fill in by
/// default never shows up as coming from the file. An empty table, such as
/// `[harnesses.claude]` with no settings, is kept as `{}`: declaring it is a
/// setting. A key that is not a bare TOML key is quoted.
pub fn entries(input: &str) -> Result<Vec<(String, String)>, ContractError> {
    let table: toml::Table = toml::from_str(input).map_err(|source| ContractError::Toml {
        contract: CONFIGURATION,
        source,
    })?;
    let mut out = Vec::new();
    flatten("", &table, &mut out);
    Ok(out)
}

fn flatten(prefix: &str, table: &toml::Table, out: &mut Vec<(String, String)>) {
    for (key, value) in table {
        let path = if prefix.is_empty() {
            key_segment(key)
        } else {
            format!("{prefix}.{}", key_segment(key))
        };
        match value {
            toml::Value::Table(inner) if !inner.is_empty() => flatten(&path, inner, out),
            toml::Value::Table(_) => out.push((path, "{}".to_owned())),
            other => out.push((path, other.to_string())),
        }
    }
}

fn key_segment(key: &str) -> String {
    let bare = !key.is_empty()
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if bare {
        key.to_owned()
    } else {
        toml::Value::String(key.to_owned()).to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requires_is_checked_against_the_running_version() {
        let current = Version::new(0, 4, 2);
        assert!(check_requires("requires = \">=0.4\"", &current).is_ok());
        assert!(check_requires("keep_awake = true", &current).is_ok());
        let error = check_requires("requires = \">=0.5\"\nnew_key = 1", &current)
            .unwrap_err()
            .to_string();
        assert!(error.contains("requires Owlshift >=0.5"), "{error}");
        assert!(
            error.contains("this is Owlshift 0.4.2: upgrade Owlshift"),
            "{error}"
        );
    }

    fn project_with_stack(stack: &str) -> Result<ProjectConfig, ContractError> {
        ProjectConfig::parse(&format!(
            r#"
            requires = ">=0.1"
            [tracker]
            kind = "markdown"
            admit = {{ label = "owlshift" }}
            states = {{ ready = "Todo", working = "Doing", needs_input = "Asked", review = "Review" }}
            [stack]
            gate = ["make test"]
            {stack}
            [pipeline]
            default = "trivial"
            plan_approval = "never"
            [models]
            [policy]
            always_human = []
            "#
        ))
    }

    fn project_with_gate_env(names: &str) -> Result<ProjectConfig, ContractError> {
        project_with_stack(&format!("gate_env = {names}"))
    }

    #[test]
    fn rules_name_files_from_the_repository_root() {
        // Left out, the key stays out of the rendered file; `[]` is kept, as
        // it means no rule rather than the default.
        let absent = project_with_stack("").unwrap();
        assert_eq!(absent.stack.rules, None);
        assert!(!absent.render().contains("rules"));
        let none = project_with_stack("rules = []").unwrap();
        assert_eq!(none.stack.rules, Some(Vec::new()));
        assert!(none.render().contains("rules = []"));
        let named =
            project_with_stack(r#"rules = ["AGENTS.md", ".claude/rules/testing.md"]"#).unwrap();
        assert_eq!(
            named.stack.rules.as_deref(),
            Some(
                &[
                    "AGENTS.md".to_owned(),
                    ".claude/rules/testing.md".to_owned()
                ][..]
            )
        );

        let many: Vec<String> = (0..=MAX_RULE_FILES)
            .map(|i| format!("\"r{i}.md\""))
            .collect();
        let many = format!("[{}]", many.join(", "));
        for (paths, reason) in [
            (r#"[""]"#, "no part of it may be empty"),
            (r#"["/etc/passwd"]"#, "starts with `/`"),
            (r#"["docs\\rules.md"]"#, "separate folders with `/`"),
            (
                r#"["docs/../AGENTS.md"]"#,
                "no part of it may be empty, `.` or `..`",
            ),
            (r#"["./AGENTS.md"]"#, "`.` or `..`"),
            (r#"["docs//rules.md"]"#, "may be empty"),
            (r#"["docs/"]"#, "may be empty"),
            (r#"["AGENTS.md\u0000"]"#, "control character"),
            (r#"["AGENTS.md", "AGENTS.md"]"#, "named twice"),
            (many.as_str(), "33 files named, over the 32"),
        ] {
            let error = project_with_stack(&format!("rules = {paths}"))
                .unwrap_err()
                .to_string();
            assert!(
                error.starts_with("invalid owlshift.toml: stack.rules: ") && error.contains(reason),
                "{paths}: {error}"
            );
        }
    }

    #[test]
    fn gate_env_declares_names_the_agent_may_receive() {
        let config = project_with_gate_env(r#"["JAVA_HOME", "feature_flag"]"#).unwrap();
        assert_eq!(config.stack.gate_env_names(), ["JAVA_HOME", "feature_flag"]);
        assert!(
            config
                .render()
                .contains("gate_env = [\"JAVA_HOME\", \"feature_flag\"]")
        );
    }

    #[test]
    fn gate_env_refuses_what_an_agent_must_not_receive() {
        for (names, reason) in [
            (
                r#"["JAVA_HOME", "github_token"]"#,
                "agents never receive credentials",
            ),
            (
                r#"["Git_Dir"]"#,
                "Owlshift sets how git and gh authenticate",
            ),
            (r#"["LD_PRELOAD"]"#, "dynamic loader"),
            (r#"["FEATURE=on"]"#, "declare names only"),
        ] {
            let error = project_with_gate_env(names).unwrap_err().to_string();
            assert!(
                error.starts_with("invalid owlshift.toml: stack.gate_env: ")
                    && error.contains(reason),
                "{names}: {error}"
            );
        }
    }

    #[test]
    fn gate_env_passes_only_what_the_operator_allows() {
        let config = project_with_gate_env(r#"["DATABASE_URL", "JAVA_HOME"]"#).unwrap();
        assert!(
            config
                .check_gate_env(&["java_home", "database_url"])
                .is_ok()
        );
        assert_eq!(
            config
                .check_gate_env(&["JAVA_HOME"])
                .unwrap_err()
                .to_string(),
            "invalid owlshift.toml: stack.gate_env: DATABASE_URL may not reach an agent on this \
             machine: the operator allows a name for every project run here by adding it to \
             `allow_gate_env` in the personal configuration"
        );

        let personal = PersonalConfig::parse("allow_gate_env = [\"DATABASE_URL\"]\n").unwrap();
        assert_eq!(personal.allow_gate_env_names(), ["DATABASE_URL"]);
        for (names, reason) in [
            (r#"["GH_TOKEN"]"#, "agents never receive credentials"),
            (r#"["A=B"]"#, "declare names only"),
        ] {
            let error = PersonalConfig::parse(&format!("allow_gate_env = {names}\n"))
                .unwrap_err()
                .to_string();
            assert!(
                error.starts_with("invalid personal configuration: allow_gate_env: ")
                    && error.contains(reason),
                "{names}: {error}"
            );
        }
    }

    #[test]
    fn entries_list_what_the_file_says() {
        let input = r#"
            requires = ">=0.1"
            [stack]
            gate = ["make lint", "make test"]
            resources = { "db.migrations" = "backend/**" }
            [harnesses.claude]
            [harnesses.codex]
            budget_usd = 20.0
        "#;
        let actual = entries(input).unwrap();
        let actual: Vec<(&str, &str)> = actual
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        let expected = [
            ("harnesses.claude", "{}"),
            ("harnesses.codex.budget_usd", "20.0"),
            ("requires", "\">=0.1\""),
            ("stack.gate", "[\"make lint\", \"make test\"]"),
            ("stack.resources.\"db.migrations\"", "\"backend/**\""),
        ];
        assert_eq!(actual, expected);
    }
}
