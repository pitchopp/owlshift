//! Project and personal configuration, in TOML.
//!
//! Only the keys the design names so far exist; later work adds keys, and a
//! project file's `requires` tells an older binary to upgrade. Read
//! `requires` with [`peek_requires`] before the strict parse, so an old binary
//! reports "upgrade" rather than an unknown key.

use std::collections::BTreeMap;
use std::num::NonZeroU32;
use std::time::Duration;

use owlshift_core::agent_env::{check_declared, check_home_folders, check_names};
use owlshift_core::decider::ZoneOwners;
use owlshift_core::gate::{GatePolicy, PassLimit, ResolverPolicy, usd_micros};
use owlshift_core::reply::DEFAULT_QUIET_WINDOW;
use owlshift_core::resource::NamedResources;
use owlshift_core::vocab::DelegationLevel;
use schemars::JsonSchema;
use semver::{Version, VersionReq};
use serde::{Deserialize, Serialize};

use crate::format::ContractError;
use crate::ids::{check_name, is_repository_part};
use crate::{Harness, PlanApproval, Role, Variant};

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
    /// The forge's settings (OWL-300); optional. Without it, no review on a
    /// pull request is read. A file using it requires an Owlshift that reads
    /// it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forge: Option<Forge>,
    /// Code zones, keyed by their folder from the repository's root, such as
    /// `backend/billing`: who owns each.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub zones: BTreeMap<String, ZoneSettings>,
    /// Settings per role (OWL-312), keyed by the role's name as its prompt
    /// file is (`build`, `resolver`, `intake`, `design`, `design_review`,
    /// `verify`, `arbiter`): rules added to that role's brief alone. Only a role whose
    /// brief carries rules is accepted ([`RULE_ROLES`]). A file using it
    /// requires an Owlshift that reads it.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub roles: BTreeMap<String, RoleSettings>,
}

/// The settings of one role (OWL-312).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RoleSettings {
    /// The files whose text reaches this role's brief alone, after
    /// `stack.rules`: paths from the repository's root with `/` between
    /// folders, in order, each read at the base commit, taken whole (a
    /// `SKILL.md` included, its front matter too) and required to exist
    /// there. Files only, none also in `stack.rules` (the default counting as
    /// `["AGENTS.md"]`), and at most 32 and 64 KiB together with it.
    pub rules: Vec<String>,
}

/// The roles whose brief carries rules, and so accept `[roles.<role>]`.
/// Each role the quality pipeline adds joins this list as it is built: the
/// design, design review and arbiter roles did with OWL-318 and `verify`
/// with OWL-321; `answer_check` never does, since it judges the decider's
/// answers from the brief alone.
pub const RULE_ROLES: [Role; 7] = [
    Role::Build,
    Role::Resolver,
    Role::Intake,
    Role::Design,
    Role::DesignReview,
    Role::Verify,
    Role::Arbiter,
];

/// The rule file read when `stack.rules` is left out.
pub const DEFAULT_RULE_FILE: &str = "AGENTS.md";

/// The tracker and how Owlshift maps onto it.
// Read through `TrackerForm`, which is what the file may say: `admit =
// "ready"` is resolved there to the state gesture on `states.ready`
// (OWL-322), so a parsed `Tracker` never holds it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(from = "TrackerForm")]
pub struct Tracker {
    pub kind: TrackerKind,
    /// The tracker's team key; required for Linear.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team: Option<String>,
    /// The gesture by which a person admits a ticket to the queue. A state
    /// gesture is the move to the ready column, so it names
    /// `states.ready`; `"ready"` says so without repeating it.
    pub admit: Admit,
    /// The tracker accounts whose gesture admits a ticket to the drain
    /// (`owlshift watch --drain`), each by its identifier as the tracker
    /// reports it (a Linear user's id, the name on the Markdown tracker),
    /// written exactly, as a zone's owner is. Optional for the file;
    /// `--drain` refuses to start without it. When set, at least one, each
    /// once, none blank, padded or holding a control character. A file
    /// using it requires an Owlshift that reads it (OWL-245).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1))]
    pub admitters: Option<Vec<String>>,
    /// A label that keeps a ticket out of the drain, whatever else it
    /// carries; optional, and never the admission label (OWL-245).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude: Option<String>,
    /// The tracker states that show a ticket's visible stage.
    pub states: States,
}

/// What `[tracker]` may say, before `admit = "ready"` is resolved: the same
/// keys as [`Tracker`] with [`AdmitForm`] for `admit`. It is also the
/// schema of `[tracker]`, so the docs of the keys are written here too.
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(
    rename = "Tracker",
    description = "The tracker and how Owlshift maps onto it."
)]
struct TrackerForm {
    kind: TrackerKind,
    /// The tracker's team key; required for Linear.
    team: Option<String>,
    /// The gesture by which a person admits a ticket to the queue. A state
    /// gesture is the move to the ready column, so it names
    /// `states.ready`; `"ready"` says so without repeating it.
    admit: AdmitForm,
    /// The tracker accounts whose gesture admits a ticket to the drain
    /// (`owlshift watch --drain`), each by its identifier as the tracker
    /// reports it (a Linear user's id, the name on the Markdown tracker),
    /// written exactly, as a zone's owner is. Optional for the file;
    /// `--drain` refuses to start without it. When set, at least one, each
    /// once, none blank, padded or holding a control character. A file
    /// using it requires an Owlshift that reads it (OWL-245).
    #[schemars(length(min = 1))]
    admitters: Option<Vec<String>>,
    /// A label that keeps a ticket out of the drain, whatever else it
    /// carries; optional, and never the admission label (OWL-245).
    exclude: Option<String>,
    /// The tracker states that show a ticket's visible stage.
    states: States,
}

impl From<TrackerForm> for Tracker {
    fn from(form: TrackerForm) -> Self {
        let admit = match form.admit {
            AdmitForm::Delegation => Admit::Delegation,
            AdmitForm::Label(label) => Admit::Label(label),
            AdmitForm::State(state) => Admit::State(state),
            AdmitForm::Ready => Admit::State(form.states.ready.clone()),
        };
        Self {
            kind: form.kind,
            team: form.team,
            admit,
            admitters: form.admitters,
            exclude: form.exclude,
            states: form.states,
        }
    }
}

/// The admission gesture: delegating the ticket to Owlshift, a label, or a
/// state. `"ready"` (OWL-322) is the state gesture on `states.ready` written
/// without repeating the state; a file using it requires an Owlshift that
/// reads it.
#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
#[schemars(rename = "Admit")]
enum AdmitForm {
    /// The ticket is delegated to Owlshift's agent identity.
    Delegation,
    /// The ticket is moved to `states.ready`.
    Ready,
    /// The ticket carries this label.
    Label(String),
    /// The ticket is in this state, which must be `states.ready`.
    State(String),
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
    /// The state a parked ticket shows; optional. Without it, a parked
    /// ticket shows `needs_input`, its PARKED comment saying why.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parked: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Stack {
    /// The project's full gate, run before delivery.
    pub gate: Vec<String>,
    /// Named resources, such as a migration chain, each with the pattern of
    /// the repository paths it covers: a path from the repository's root,
    /// `/` between its parts, where a part `*` stands for exactly one part
    /// and `**` for any number of them (`backend/*/migrations/**`). Names
    /// hold only ASCII letters, digits, `.`, `_` and `-`. Checked when the
    /// file is loaded ([`owlshift_core::resource::Pattern::new`]). Intake
    /// names the ones a ticket touches, and a zone a pattern reaches holds
    /// its resource (OWL-402).
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
    /// Shell commands that install the gate's dependencies, run in order by
    /// the runner, confined as the gate is and with the network open: before
    /// a Build's harness starts, on every Build run, and before the gate's
    /// commands, after a Build or a Rebase `done` and in `owlshift gate`.
    /// They may write only into ignored folders, and must leave the work in
    /// the worktree as they found it. A failing command fails the run, and
    /// they share a third of the gate's deadline.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub setup: Vec<String>,
    /// Folders of the home, from the home with `/` between their parts,
    /// that the setup, the harness and the gate read, never write: a tool
    /// chain's package store or cache, such as `Library/pnpm`. Each must be
    /// allowed by the operator in the personal `allow_caches`; a folder that
    /// is, lies in or holds a credential path is refused.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub caches: Vec<String>,
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

    /// The folders of [`Stack::caches`], from the home.
    pub fn cache_folders(&self) -> Vec<&str> {
        names(&self.caches)
    }

    /// The named resources of [`Stack::resources`], checked; `Err` names
    /// the entry and what is wrong with it.
    pub fn named_resources(&self) -> Result<NamedResources, String> {
        NamedResources::new(&self.resources).map_err(|error| error.to_string())
    }

    /// Checks [`Stack::setup`]: no command blank.
    fn check_setup(&self) -> Result<(), String> {
        match self
            .setup
            .iter()
            .position(|command| command.trim().is_empty())
        {
            Some(i) => Err(format!("command {} is blank", i + 1)),
            None => Ok(()),
        }
    }

    /// Checks the paths of [`Stack::rules`]: each a path from the
    /// repository's root, `/` between its parts, none empty, `.` or `..`,
    /// no `\` or control character, none named twice (compared as written),
    /// and at most [`MAX_RULE_FILES`].
    fn check_rules(&self) -> Result<(), String> {
        let Some(paths) = &self.rules else {
            return Ok(());
        };
        check_rule_paths(&paths.iter().map(String::as_str).collect::<Vec<_>>(), &[])
    }

    /// The files `stack.rules` stands for: the ones it names, or the root
    /// `AGENTS.md` when it is left out.
    pub fn rule_files(&self) -> Vec<&str> {
        match &self.rules {
            Some(named) => named.iter().map(String::as_str).collect(),
            None => vec![DEFAULT_RULE_FILE],
        }
    }
}

/// Checks `paths`, rule files of one brief: at most [`MAX_RULE_FILES`]
/// together with `already`, the files of the same brief named in another
/// list; each a path from the repository's root, `/` between its parts, none
/// empty, `.` or `..`, no `\` or control character, none named twice, none in
/// `already` (compared as written).
fn check_rule_paths(paths: &[&str], already: &[&str]) -> Result<(), String> {
    let total = paths.len() + already.len();
    if total > MAX_RULE_FILES {
        return Err(format!(
            "{total} files named, over the {MAX_RULE_FILES} Owlshift takes as rules"
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
        if already.contains(path) {
            return Err(format!(
                "{path:?} is also a rule of `stack.rules`, which the brief already holds: \
                 name it in one list"
            ));
        }
    }
    Ok(())
}

impl ProjectConfig {
    /// The files `[roles.<role>]` names for `role`, none when it has no
    /// table.
    pub fn role_rule_files(&self, role: Role) -> &[String] {
        self.roles
            .get(role.as_str())
            .map_or(&[], |settings| settings.rules.as_slice())
    }

    /// Checks `[roles]`: each key a role whose brief carries rules, and its
    /// files by the rules of `stack.rules`, counted with it for the brief.
    fn check_roles(&self) -> Result<(), ContractError> {
        let stack_files = self.stack.rule_files();
        for (name, settings) in &self.roles {
            let refuse = |reason: String| {
                ContractError::invalid(PROJECT, format!("roles.{name}.rules: {reason}"))
            };
            if !RULE_ROLES.iter().any(|role| role.as_str() == name) {
                return Err(refuse(if name == Role::AnswerCheck.as_str() {
                    "the answer check classifies the decider's answers from its brief alone, \
                     and an instruction there could only sway a verdict: it takes no rules"
                        .to_owned()
                } else {
                    format!(
                        "`{name}` is not a role whose brief carries rules (a role joins as it is built); the roles that do: {}",
                        RULE_ROLES.map(Role::as_str).join(", ")
                    )
                }));
            }
            let paths: Vec<&str> = settings.rules.iter().map(String::as_str).collect();
            check_rule_paths(&paths, &stack_files).map_err(refuse)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Pipeline {
    pub default: Variant,
    pub plan_approval: PlanApproval,
    /// Who confirms a review finding before it is acted on (OWL-300,
    /// architecture section 5, "Confirming a finding"): `author`, the
    /// default when absent, or `arbiter`. A file using it requires an
    /// Owlshift that reads it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub findings: Option<FindingsConfirmation>,
}

/// Who confirms a review finding.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FindingsConfirmation {
    /// One arbiter run per review pass rules on all its findings. Opt-in:
    /// the P5 exit gate's drains ran it 90 times for 22 USD and it rejected
    /// 18 findings of 516 (OWL-391).
    Arbiter,
    /// The next design or fix run answers each finding, fixed or rejected.
    /// The default: the author already answers every finding it is given
    /// (OWL-377), so a ruling in front of it judges the same finding twice.
    #[default]
    Author,
}

impl Pipeline {
    /// Who confirms a finding: the project's choice, or the author.
    pub fn findings(&self) -> FindingsConfirmation {
        self.findings.unwrap_or_default()
    }
}

/// The forge and how Owlshift reads it (OWL-300).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Forge {
    /// The forge logins whose reviews on Owlshift's pull requests are read,
    /// each written exactly and mapped to its tracker account's identifier,
    /// written as a zone's owner is. A review is an instruction when that
    /// account is the ticket's decider, and context otherwise; anyone
    /// else's review is ignored. At least one, no login listed twice, even
    /// in another case.
    #[schemars(length(min = 1))]
    pub reviewers: BTreeMap<String, String>,
}

impl Forge {
    /// Checks the reviewers: at least one, each login and account a name
    /// written exactly, no login twice ignoring case (forge logins are
    /// compared without case).
    fn check_reviewers(&self) -> Result<(), String> {
        if self.reviewers.is_empty() {
            return Err(
                "forge.reviewers lists no reviewer: list at least one, or leave [forge] out"
                    .to_owned(),
            );
        }
        let mut seen: Vec<String> = Vec::new();
        for (login, account) in &self.reviewers {
            if let Err(broken) = check_name(login) {
                return Err(format!("forge.reviewers: the login {login:?} {broken}"));
            }
            if let Err(broken) = check_name(account) {
                return Err(format!(
                    "forge.reviewers: the account of {login:?}, {account:?}, {broken}"
                ));
            }
            let lower = login.to_lowercase();
            if seen.contains(&lower) {
                return Err(format!(
                    "forge.reviewers: {login:?} is listed twice, logins being compared without case"
                ));
            }
            seen.push(lower);
        }
        Ok(())
    }
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
    /// How long the decider's latest comment must be left unedited before
    /// their answer counts, in whole minutes from 1 to 1440 (a day). Absent:
    /// 10 minutes. Zero is refused: every edit would count at once, which is
    /// what ending a comment with `go` is for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1, max = 1440))]
    pub quiet_window_minutes: Option<u16>,
    /// How many pull requests Owlshift delivered may be open at once before
    /// the drain dispatches no new Build, a whole number from 1 to 20.
    /// Absent: 3 (OWL-245).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1, max = 20))]
    pub max_open_prs: Option<u8>,
    /// What the resolver may decide without the decider (OWL-328,
    /// architecture section 4, "Delegation level"): `none`, `facts`,
    /// `discoverable`, `ticket` or `all`. Absent: `discoverable`. `security`
    /// and `irreversible` questions, and those of `always_human`, go to a
    /// human at every level. A file using it requires an Owlshift that
    /// reads it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolver_delegation: Option<DelegationLevel>,
    /// The runs in a row the resolver may unblock in one command: a whole
    /// number from 1 to 50, or `"unlimited"`, which needs
    /// `resolver_budget_usd`. Absent: 5 (OWL-328). A file using it requires
    /// an Owlshift that reads it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolver_passes: Option<ResolverPasses>,
    /// A ceiling, in dollars, on what the resolver runs of one chain cost
    /// since the command's last stop, past which the next run's questions
    /// go to the decider; the asking runs do not count (OWL-376). A positive
    /// number; required with `resolver_passes = "unlimited"` (OWL-328). A
    /// file using it requires an Owlshift that reads it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolver_budget_usd: Option<Usd>,
    /// A budget, in dollars, on what one ticket's runs cost (OWL-378,
    /// architecture section 4, "Ticket budget"): every run counted once,
    /// across every command and stop, since the ticket's first run or a
    /// person's last restart of it; reached, no run of the ticket starts and
    /// it parks. A positive number. Absent: no budget. Independent of
    /// `resolver_budget_usd`. A file using it requires an Owlshift that
    /// reads it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ticket_budget_usd: Option<Usd>,
}

/// `policy.resolver_passes`: a whole number of runs, or `"unlimited"`
/// (OWL-328). A number outside 1 to 50 parses, and
/// [`ProjectConfig::validate`] refuses it, naming the key.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ResolverPasses {
    Runs(u32),
    Unlimited,
}

/// The one word `policy.resolver_passes` takes instead of a number.
const UNLIMITED: &str = "unlimited";

/// What a refusal of `policy.resolver_passes` says it must be.
const RESOLVER_PASSES_SHAPE: &str =
    "policy.resolver_passes must be a whole number from 1 to 50, or \"unlimited\"";

impl Serialize for ResolverPasses {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Runs(runs) => serializer.serialize_u32(*runs),
            Self::Unlimited => serializer.serialize_str(UNLIMITED),
        }
    }
}

impl<'de> Deserialize<'de> for ResolverPasses {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl serde::de::Visitor<'_> for Visitor {
            type Value = ResolverPasses;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a whole number from 1 to 50, or \"unlimited\"")
            }

            fn visit_i64<E: serde::de::Error>(self, value: i64) -> Result<Self::Value, E> {
                u32::try_from(value)
                    .map(ResolverPasses::Runs)
                    .map_err(|_| E::custom(RESOLVER_PASSES_SHAPE))
            }

            fn visit_u64<E: serde::de::Error>(self, value: u64) -> Result<Self::Value, E> {
                u32::try_from(value)
                    .map(ResolverPasses::Runs)
                    .map_err(|_| E::custom(RESOLVER_PASSES_SHAPE))
            }

            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
                if value == UNLIMITED {
                    Ok(ResolverPasses::Unlimited)
                } else {
                    Err(E::custom(RESOLVER_PASSES_SHAPE))
                }
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}

impl JsonSchema for ResolverPasses {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "ResolverPasses".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "description": "A whole number of runs from 1 to 50, or \"unlimited\".",
            "oneOf": [
                { "type": "integer", "minimum": 1, "maximum": 50 },
                { "const": UNLIMITED }
            ]
        })
    }
}

/// An amount in dollars, as `policy.resolver_budget_usd` and
/// `policy.ticket_budget_usd` write it: a positive finite number of at least
/// [`MIN_USD`] once [`ProjectConfig::validate`] passed it.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Usd(pub f64);

// A parsed file's amount is finite ([`ProjectConfig::validate`]), so
// equality is reflexive.
impl Eq for Usd {}

impl JsonSchema for Usd {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "Usd".into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "description": "A positive number of dollars.",
            "type": "number",
            "exclusiveMinimum": 0
        })
    }
}

/// The smallest dollar amount a `policy` key takes: the millionth of a
/// dollar the runner counts in, below which a ceiling would round to
/// nothing.
pub const MIN_USD: f64 = 0.000_001;

/// Checks the dollar amount of `policy.<key>`, when set: a finite number of
/// at least [`MIN_USD`], read as written, never rounded first.
fn check_usd(key: &str, amount: Option<Usd>) -> Result<(), String> {
    match amount {
        Some(Usd(usd)) if !(usd.is_finite() && usd >= MIN_USD) => Err(format!(
            "policy.{key} must be a positive number of dollars, at least 0.000001"
        )),
        _ => Ok(()),
    }
}

/// A dollar amount in millionths of a dollar, for a ceiling: none for an
/// amount that is not a positive number, and an amount below a millionth,
/// possible only in a configuration built in code, raised to that
/// millionth.
fn usd_ceiling(amount: Option<Usd>) -> Option<u64> {
    amount
        .filter(|usd| usd.0 > 0.0)
        .and_then(|usd| usd_micros(usd.0))
        .map(|micros| micros.max(1))
}

/// The range of `policy.resolver_passes` as a number of runs (the schema
/// repeats it as literals).
const RESOLVER_PASSES: std::ops::RangeInclusive<u32> = 1..=50;

/// The range of `policy.quiet_window_minutes`, in minutes (the schema
/// attribute on the field repeats it as literals).
const QUIET_WINDOW_MINUTES: std::ops::RangeInclusive<u16> = 1..=1440;

/// The range of `policy.max_open_prs` (the schema attribute on the field
/// repeats it as literals).
const MAX_OPEN_PRS: std::ops::RangeInclusive<u8> = 1..=20;

/// The cap on open pull requests when `policy.max_open_prs` is absent.
pub const DEFAULT_MAX_OPEN_PRS: u8 = 3;

impl Policy {
    /// The quiet window: the project's, or the default. A value outside
    /// the range, possible only in a configuration built in code, is
    /// clamped into it: the window is never zero.
    pub fn quiet_window(&self) -> Duration {
        self.quiet_window_minutes
            .map_or(DEFAULT_QUIET_WINDOW, |minutes| {
                let minutes =
                    minutes.clamp(*QUIET_WINDOW_MINUTES.start(), *QUIET_WINDOW_MINUTES.end());
                Duration::from_secs(u64::from(minutes) * 60)
            })
    }

    /// The cap on open pull requests: the project's, or
    /// [`DEFAULT_MAX_OPEN_PRS`]. A value outside the range, possible only in
    /// a configuration built in code, is clamped into it.
    pub fn max_open_prs(&self) -> u8 {
        self.max_open_prs.map_or(DEFAULT_MAX_OPEN_PRS, |cap| {
            cap.clamp(*MAX_OPEN_PRS.start(), *MAX_OPEN_PRS.end())
        })
    }

    /// Checks the resolver's keys (OWL-328): a number of passes from 1 to
    /// 50, a ceiling that is a positive number of dollars, at least the
    /// millionth of a dollar the gate counts in (a smaller one would round
    /// to a ceiling of nothing, which ends every chain before its first
    /// resolver run), and a ceiling beside `"unlimited"`, which nothing else
    /// bounds.
    fn check_resolver(&self) -> Result<(), String> {
        if let Some(ResolverPasses::Runs(runs)) = self.resolver_passes
            && !RESOLVER_PASSES.contains(&runs)
        {
            return Err(RESOLVER_PASSES_SHAPE.to_owned());
        }
        check_usd("resolver_budget_usd", self.resolver_budget_usd)?;
        if self.resolver_passes == Some(ResolverPasses::Unlimited)
            && self.resolver_budget_usd.is_none()
        {
            return Err(
                "policy.resolver_passes = \"unlimited\" needs policy.resolver_budget_usd: an \
                 unlimited chain of runs is bounded by a dollar ceiling instead"
                    .to_owned(),
            );
        }
        Ok(())
    }

    /// What the project lets the resolver do (OWL-328): its level, pass
    /// limit and ceiling, each the project's or its default
    /// ([`ResolverPolicy::default`]). A number of passes outside the range,
    /// possible only in a configuration built in code, is clamped into it;
    /// so is an amount below a millionth of a dollar, to that millionth, and
    /// an amount that is not a positive number sets no ceiling.
    pub fn resolver(&self) -> ResolverPolicy {
        let passes = match self.resolver_passes {
            None => PassLimit::default(),
            Some(ResolverPasses::Runs(runs)) => {
                PassLimit::Runs(runs.clamp(*RESOLVER_PASSES.start(), *RESOLVER_PASSES.end()))
            }
            Some(ResolverPasses::Unlimited) => PassLimit::Unlimited,
        };
        ResolverPolicy {
            level: self.resolver_delegation.unwrap_or_default(),
            passes,
            ceiling_micros: usd_ceiling(self.resolver_budget_usd),
        }
    }

    /// The budget on one ticket's runs (OWL-378), in millionths of a
    /// dollar: `None` without one. An amount below a millionth of a dollar,
    /// possible only in a configuration built in code, is that millionth.
    pub fn ticket_budget_micros(&self) -> Option<u64> {
        usd_ceiling(self.ticket_budget_usd)
    }
}

impl Tracker {
    /// Checks admission: a state gesture names `states.ready`; the
    /// admitters, when listed, are at least one, each once and each a name
    /// written exactly; the exclusion label is one too, and not the
    /// admission label in any case, which would exclude every ticket.
    fn check_admission(&self) -> Result<(), String> {
        if let Admit::State(state) = &self.admit
            && *state != self.states.ready
        {
            return Err(format!(
                "tracker.admit names the state {state:?}, but a state gesture is the move to \
                 the ready column: it must be tracker.states.ready, {:?}",
                self.states.ready
            ));
        }
        if let Some(admitters) = &self.admitters {
            if admitters.is_empty() {
                return Err(
                    "tracker.admitters lists no account: list at least one, or leave \
                            the key out"
                        .to_owned(),
                );
            }
            for (i, account) in admitters.iter().enumerate() {
                if let Err(broken) = check_name(account) {
                    return Err(format!("tracker.admitters: {account:?} {broken}"));
                }
                if admitters[..i].contains(account) {
                    return Err(format!("tracker.admitters: {account:?} is listed twice"));
                }
            }
        }
        if let Some(exclude) = &self.exclude {
            if let Err(broken) = check_name(exclude) {
                return Err(format!("tracker.exclude {exclude:?} {broken}"));
            }
            if let Admit::Label(label) = &self.admit
                && label.to_lowercase() == exclude.to_lowercase()
            {
                return Err(format!(
                    "tracker.exclude {exclude:?} is the admission label: no ticket could be \
                     taken in"
                ));
            }
        }
        Ok(())
    }
}

/// The settings of one code zone.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ZoneSettings {
    /// The tracker account that decides the tickets without an assignee
    /// touching this zone: its identifier as the tracker reports it (a
    /// Linear user's id, the name on the Markdown tracker), written exactly.
    pub owner: String,
}

impl ProjectConfig {
    /// Parses and validates `owlshift.toml`.
    pub fn parse(input: &str) -> Result<Self, ContractError> {
        let config: Self = toml::from_str(input).map_err(|source| ContractError::Toml {
            contract: PROJECT,
            source: Box::new(source),
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
        self.tracker
            .check_admission()
            .map_err(|reason| ContractError::invalid(PROJECT, reason))?;
        self.stack
            .check_rules()
            .map_err(|reason| ContractError::invalid(PROJECT, format!("stack.rules: {reason}")))?;
        self.check_roles()?;
        if let Some(minutes) = self.policy.quiet_window_minutes
            && !QUIET_WINDOW_MINUTES.contains(&minutes)
        {
            return Err(ContractError::invalid(
                PROJECT,
                "policy.quiet_window_minutes must be a whole number of minutes from 1 to 1440",
            ));
        }
        if let Some(cap) = self.policy.max_open_prs
            && !MAX_OPEN_PRS.contains(&cap)
        {
            return Err(ContractError::invalid(
                PROJECT,
                "policy.max_open_prs must be a whole number from 1 to 20",
            ));
        }
        self.policy
            .check_resolver()
            .and_then(|()| check_usd("ticket_budget_usd", self.policy.ticket_budget_usd))
            .map_err(|reason| ContractError::invalid(PROJECT, reason))?;
        if let Some(forge) = &self.forge {
            forge
                .check_reviewers()
                .map_err(|reason| ContractError::invalid(PROJECT, reason))?;
        }
        self.zone_owners()?;
        self.stack.named_resources().map_err(|reason| {
            ContractError::invalid(PROJECT, format!("stack.resources: {reason}"))
        })?;
        self.stack
            .check_setup()
            .map_err(|reason| ContractError::invalid(PROJECT, format!("stack.setup: {reason}")))?;
        check_home_folders(&self.stack.cache_folders())
            .map_err(|reason| ContractError::invalid(PROJECT, format!("stack.caches: {reason}")))?;
        check_names(&self.stack.gate_env_names()).map_err(gate_env_error)
    }

    /// The project's gate policy (architecture section 4): `always_human`,
    /// the plan-approval mode and what the resolver may do, read from this
    /// file. `owlshift do` and the test bench's stand-in driver build theirs
    /// here alike.
    pub fn gate_policy(&self) -> GatePolicy {
        GatePolicy::new(&self.policy.always_human, self.pipeline.plan_approval)
            .with_resolver(self.policy.resolver())
    }

    /// The owners `zones` names, checked: each key a folder from the
    /// repository's root, no two naming the same zone, each owner neither
    /// blank nor padded (`owlshift_core::decider::ZoneOwners::new`). On a
    /// parsed file it cannot fail; it checks again for one built otherwise.
    pub fn zone_owners(&self) -> Result<ZoneOwners, ContractError> {
        ZoneOwners::new(
            self.zones
                .iter()
                .map(|(zone, settings)| (zone.as_str(), settings.owner.as_str())),
        )
        .map_err(|error| ContractError::invalid(PROJECT, format!("zones: {error}")))
    }

    /// Checks `stack.gate_env` against the names the operator allows for
    /// this project, from the personal file
    /// ([`PersonalConfig::allow_gate_env_names`]): the rule the agent
    /// environment applies (`owlshift_core::agent_env::check_declared`,
    /// OWL-63). The names already passed `check_names` in [`Self::validate`],
    /// so on a parsed file only a name the operator does not allow fails.
    pub fn check_gate_env(&self, allowed: &[&str]) -> Result<(), ContractError> {
        check_declared(&self.stack.gate_env_names(), allowed).map_err(gate_env_error)
    }

    /// Checks `stack.caches` against the folders the operator allows for
    /// this project, from the personal file
    /// ([`PersonalConfig::allow_caches_names`], OWL-330): a declared folder
    /// not listed there, compared as written, is refused, so a project's file
    /// never opens a folder of the operator's home on its own.
    pub fn check_caches(&self, allowed: &[&str]) -> Result<(), ContractError> {
        let refused: Vec<&str> = self
            .stack
            .cache_folders()
            .into_iter()
            .filter(|folder| !allowed.contains(folder))
            .collect();
        if refused.is_empty() {
            return Ok(());
        }
        let quoted: Vec<String> = refused.iter().map(|f| format!("{f:?}")).collect();
        Err(ContractError::invalid(
            PROJECT,
            format!(
                "stack.caches: {} may not be read by an agent on this machine: the operator \
                 allows a cache folder in the personal configuration, for every repository in \
                 `allow_caches`, or for one in the `allow_caches` of its \
                 `[repositories.\"github.com/<owner>/<name>\"]` table",
                quoted.join(", ")
            ),
        ))
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
    /// The client ID of the GitHub App `owlshift init` signs in through, in
    /// place of Owlshift's own (architecture D13), for a fork or a private
    /// app: letters, digits, `.`, `_` and `-`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub github_app_client_id: Option<String>,
    /// That app's slug, which makes the install link
    /// `https://github.com/apps/<slug>/installations/new`: lowercase letters,
    /// digits and inner `-`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub github_app_slug: Option<String>,
    /// How this machine tells its operator that they have become the
    /// blocker.
    #[serde(default)]
    pub notifications: Notifications,
    /// Names of variables of this machine's environment that a project may
    /// pass to its agents by declaring them in `stack.gate_env`, in any
    /// letter case. A project's declaration never widens this list: a name
    /// declared but not listed here is refused. It applies to every project
    /// run on this machine; `repositories` allows a name for one repository
    /// only. A credential variable, a `GIT_` name, a variable Owlshift
    /// overrides or one that makes the dynamic loader load code cannot be
    /// listed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow_gate_env: Vec<String>,
    /// Folders of the home, from the home with `/` between their parts, that
    /// a project may open read-only to its agents by declaring them in
    /// `stack.caches`, compared as written. A project's declaration never
    /// widens this list. It applies to every project run on this machine;
    /// `repositories` allows a folder for one repository only. A folder that
    /// is, lies in or holds a credential path cannot be listed.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow_caches: Vec<String>,
    /// Settings for one repository, keyed `github.com/<owner>/<name>` in any
    /// letter case. The repository of a project is read from its checkout's
    /// `origin` remote, which the committed project file cannot set.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub repositories: BTreeMap<String, RepositorySettings>,
}

/// The personal settings of one repository.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RepositorySettings {
    /// Names a project of this repository may pass to its agents, on top of
    /// the machine-wide `allow_gate_env`, under the same rules.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow_gate_env: Vec<String>,
    /// Folders a project of this repository may open read-only to its
    /// agents, on top of the machine-wide `allow_caches`, under the same
    /// rules.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow_caches: Vec<String>,
}

/// The host part of a repository key of [`PersonalConfig::repositories`]:
/// only GitHub repositories are delivered to so far.
const GITHUB: &str = "github.com";

/// Checks a key of [`PersonalConfig::repositories`]:
/// `github.com/<owner>/<name>`.
fn check_repository_key(key: &str) -> Result<(), String> {
    let parts: Vec<&str> = key.split('/').collect();
    match parts.as_slice() {
        [host, owner, name]
            if host.eq_ignore_ascii_case(GITHUB)
                && is_repository_part(owner)
                && is_repository_part(name) =>
        {
            Ok(())
        }
        _ => Err(format!(
            "a repository is written {GITHUB}/<owner>/<name>, as its `origin` remote names it"
        )),
    }
}

/// The most characters of a GitHub App's client ID or slug in the personal
/// file.
const MAX_GITHUB_APP_ID: usize = 100;

/// A GitHub App's client ID, such as `Iv23li5EhBqiuljIPbiK` or the older
/// `Iv1.0123abcd`.
fn is_github_app_client_id(id: &str) -> bool {
    (1..=MAX_GITHUB_APP_ID).contains(&id.len())
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

/// A GitHub App's slug, such as `owl-shift`.
fn is_github_app_slug(slug: &str) -> bool {
    (1..=MAX_GITHUB_APP_ID).contains(&slug.len())
        && !slug.starts_with('-')
        && !slug.ends_with('-')
        && slug
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// How this machine tells its operator that they have become the blocker: a
/// question, a re-ask, a parked ticket, every harness at its usage limit;
/// never progress.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Notifications {
    /// Whether to show a desktop notification on this machine: on unless set
    /// to `false`. A machine with no desktop session shows nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desktop: Option<bool>,
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
    /// A dollar cap on each run of a harness, not on a day's total, whatever
    /// the billing. Owlshift passes it to Claude Code as `--max-budget-usd`,
    /// which counts a run's cost at list price on a subscription too and
    /// stops the run once its cost is above the cap, so a run can end above
    /// it. A run stopped by it alone is a failed run. Codex has no such
    /// option, so nothing enforces it there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 0))]
    pub budget_usd: Option<f64>,
    /// A usage cap, in percent from 1 to 100: while the latest share seen of
    /// any usage window this harness's subscription reports is at or above
    /// it, and that window has not reset since, no new run starts on this
    /// harness, which counts as at its usage limit. A harness whose runs
    /// report no window is never held.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(range(min = 1, max = 100))]
    pub usage_cap_percent: Option<u8>,
}

impl PersonalConfig {
    /// Parses and validates the personal file.
    pub fn parse(input: &str) -> Result<Self, ContractError> {
        let config: Self = toml::from_str(input).map_err(|source| ContractError::Toml {
            contract: PERSONAL,
            source: Box::new(source),
        })?;
        config.validate()?;
        Ok(config)
    }

    /// Renders the personal file, without comments.
    pub fn render(&self) -> String {
        toml::to_string(self).expect("contract types always serialize to TOML")
    }

    /// The names the operator allows a project of `repository` to pass to
    /// its agents: the machine-wide [`PersonalConfig::allow_gate_env`], then
    /// those of the [`PersonalConfig::repositories`] entry whose key is
    /// `repository` (`github.com/<owner>/<name>`, compared in any letter
    /// case). With no repository known, only the machine-wide names.
    pub fn allow_gate_env_names(&self, repository: Option<&str>) -> Vec<&str> {
        let scoped = self
            .repositories
            .iter()
            .filter(|(key, _)| repository.is_some_and(|r| key.eq_ignore_ascii_case(r)))
            .flat_map(|(_, settings)| &settings.allow_gate_env);
        self.allow_gate_env
            .iter()
            .chain(scoped)
            .map(String::as_str)
            .collect()
    }

    /// The cache folders the operator allows a project of `repository` to
    /// open read-only (OWL-330): the machine-wide
    /// [`PersonalConfig::allow_caches`], then those of the
    /// [`PersonalConfig::repositories`] entry whose key is `repository`, as
    /// for [`PersonalConfig::allow_gate_env_names`].
    pub fn allow_caches_names(&self, repository: Option<&str>) -> Vec<&str> {
        let scoped = self
            .repositories
            .iter()
            .filter(|(key, _)| repository.is_some_and(|r| key.eq_ignore_ascii_case(r)))
            .flat_map(|(_, settings)| &settings.allow_caches);
        self.allow_caches
            .iter()
            .chain(scoped)
            .map(String::as_str)
            .collect()
    }

    pub fn validate(&self) -> Result<(), ContractError> {
        check_names(&names(&self.allow_gate_env)).map_err(|error| {
            ContractError::invalid(PERSONAL, format!("allow_gate_env: {error}"))
        })?;
        check_home_folders(&names(&self.allow_caches))
            .map_err(|error| ContractError::invalid(PERSONAL, format!("allow_caches: {error}")))?;
        if let Some(id) = &self.github_app_client_id
            && !is_github_app_client_id(id)
        {
            return Err(ContractError::invalid(
                PERSONAL,
                "github_app_client_id: a GitHub App's client ID holds only letters, digits, \
                 `.`, `_` and `-`",
            ));
        }
        // The slug goes into the install link: its characters keep the link
        // on the app's page.
        if let Some(slug) = &self.github_app_slug
            && !is_github_app_slug(slug)
        {
            return Err(ContractError::invalid(
                PERSONAL,
                "github_app_slug: a GitHub App's slug holds only lowercase letters, digits and \
                 `-`, neither first nor last",
            ));
        }
        for (key, settings) in &self.repositories {
            let quoted = toml::Value::String(key.clone());
            check_repository_key(key).map_err(|error| {
                ContractError::invalid(PERSONAL, format!("repositories.{quoted}: {error}"))
            })?;
            check_names(&names(&settings.allow_gate_env)).map_err(|error| {
                ContractError::invalid(
                    PERSONAL,
                    format!("repositories.{quoted}.allow_gate_env: {error}"),
                )
            })?;
            check_home_folders(&names(&settings.allow_caches)).map_err(|error| {
                ContractError::invalid(
                    PERSONAL,
                    format!("repositories.{quoted}.allow_caches: {error}"),
                )
            })?;
        }
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
            if let Some(cap) = settings.usage_cap_percent
                && !(1..=100).contains(&cap)
            {
                return Err(ContractError::invalid(
                    PERSONAL,
                    format!(
                        "harness {harness:?}: usage_cap_percent must be a whole number from 1 to 100"
                    ),
                ));
            }
        }
        Ok(())
    }
}

fn names(list: &[String]) -> Vec<&str> {
    list.iter().map(String::as_str).collect()
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
            source: Box::new(source),
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
        source: Box::new(source),
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
        project(stack, "")
    }

    /// A project file with `stack` added to its `[stack]` table and `tail`
    /// after its last table.
    fn project(stack: &str, tail: &str) -> Result<ProjectConfig, ContractError> {
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
            {tail}
            "#
        ))
    }

    #[test]
    fn the_parked_state_is_optional() {
        let absent = project("", "").unwrap();
        assert_eq!(absent.tracker.states.parked, None);
        assert!(!absent.render().contains("parked"));

        let input = r#"
            requires = ">=0.1"
            [tracker]
            kind = "markdown"
            admit = { label = "owlshift" }
            states = { ready = "Todo", working = "Doing", needs_input = "Asked", review = "Review", parked = "Parked" }
            [stack]
            gate = []
            [pipeline]
            default = "trivial"
            plan_approval = "never"
            [models]
            [policy]
            always_human = []
            "#;
        let set = ProjectConfig::parse(input).unwrap();
        assert_eq!(set.tracker.states.parked.as_deref(), Some("Parked"));
        assert_eq!(ProjectConfig::parse(&set.render()).unwrap(), set);
    }

    #[test]
    fn the_quiet_window_is_a_policy_key_with_a_default_and_a_range() {
        let absent = project("", "").unwrap();
        assert_eq!(absent.policy.quiet_window_minutes, None);
        assert_eq!(absent.policy.quiet_window(), Duration::from_secs(10 * 60));
        assert!(!absent.render().contains("quiet_window"));

        let set = project("", "quiet_window_minutes = 45").unwrap();
        assert_eq!(set.policy.quiet_window(), Duration::from_secs(45 * 60));
        assert_eq!(ProjectConfig::parse(&set.render()).unwrap(), set);

        let built = Policy {
            always_human: Vec::new(),
            quiet_window_minutes: Some(0),
            max_open_prs: None,
            resolver_delegation: None,
            resolver_passes: None,
            resolver_budget_usd: None,
            ticket_budget_usd: None,
        };
        assert_eq!(built.quiet_window(), Duration::from_secs(60));

        for ok in ["1", "1440"] {
            project("", &format!("quiet_window_minutes = {ok}")).unwrap();
        }
        for bad in ["0", "1441", "-5", "10.5", "\"10\""] {
            assert!(
                project("", &format!("quiet_window_minutes = {bad}")).is_err(),
                "{bad}"
            );
        }
        let error = project("", "quiet_window_minutes = 0")
            .unwrap_err()
            .to_string();
        assert!(error.contains("policy.quiet_window_minutes"), "{error}");
        let error = project("", "quiet_window_minutes = 1441")
            .unwrap_err()
            .to_string();
        assert!(error.contains("from 1 to 1440"), "{error}");
    }

    /// OWL-328: the resolver's three keys, their defaults, their refusals,
    /// each naming its key, and the policy they give the gate.
    #[test]
    fn the_resolvers_keys_have_defaults_and_are_checked() {
        let absent = project("", "").unwrap();
        assert_eq!(absent.policy.resolver(), ResolverPolicy::default());
        assert_eq!(absent.policy.resolver().passes, PassLimit::Runs(5));
        assert!(!absent.render().contains("resolver"));
        assert_eq!(absent.gate_policy().level(), DelegationLevel::Discoverable);

        for level in DelegationLevel::ALL {
            let set =
                project("", &format!("resolver_delegation = \"{}\"", level.as_str())).unwrap();
            assert_eq!(set.policy.resolver().level, level);
            assert_eq!(ProjectConfig::parse(&set.render()).unwrap(), set);
        }
        let set = project("", "resolver_passes = 50\nresolver_budget_usd = 2.5").unwrap();
        assert_eq!(
            set.policy.resolver(),
            ResolverPolicy {
                level: DelegationLevel::Discoverable,
                passes: PassLimit::Runs(50),
                ceiling_micros: Some(2_500_000),
            }
        );
        assert_eq!(ProjectConfig::parse(&set.render()).unwrap(), set);
        let unlimited = project(
            "",
            "resolver_passes = \"unlimited\"\nresolver_budget_usd = 5",
        )
        .unwrap();
        assert_eq!(unlimited.policy.resolver().passes, PassLimit::Unlimited);
        assert_eq!(unlimited.policy.resolver().ceiling_micros, Some(5_000_000));
        assert_eq!(
            ProjectConfig::parse(&unlimited.render()).unwrap(),
            unlimited
        );
        assert_eq!(
            unlimited.gate_policy().resolver(),
            unlimited.policy.resolver()
        );

        let refused = |lines: &str| project("", lines).unwrap_err().to_string();
        for bad in ["0", "51", "-1", "\"many\"", "2.5"] {
            let error = refused(&format!("resolver_passes = {bad}"));
            assert!(error.contains("resolver_passes"), "{bad}: {error}");
        }
        assert!(refused("resolver_passes = 51").contains("from 1 to 50"));
        for bad in ["0", "-1.5", "nan", "inf", "\"5\""] {
            let error = refused(&format!("resolver_budget_usd = {bad}"));
            assert!(error.contains("resolver_budget_usd"), "{bad}: {error}");
        }
        let error = refused("resolver_passes = \"unlimited\"");
        assert!(
            error.contains("policy.resolver_passes")
                && error.contains("policy.resolver_budget_usd"),
            "{error}"
        );
        assert!(refused("resolver_delegation = \"most\"").contains("resolver_delegation"));
    }

    /// A ceiling below the millionth of a dollar the gate counts in would
    /// round to nothing and end every chain before its first resolver run,
    /// or park a ticket before its first run: the file is refused, naming
    /// the key, the amount read as written (0.0000006 would round up to the
    /// millionth); the millionth itself is kept.
    #[test]
    fn a_ceiling_below_a_millionth_of_a_dollar_is_refused() {
        for key in ["resolver_budget_usd", "ticket_budget_usd"] {
            for tiny in ["1e-9", "0.0000004", "0.0000006"] {
                let error = project("", &format!("{key} = {tiny}"))
                    .unwrap_err()
                    .to_string();
                assert!(
                    error.contains(&format!("policy.{key}")) && error.contains("at least 0.000001"),
                    "{key} = {tiny}: {error}"
                );
            }
        }
        let smallest = project(
            "",
            "resolver_budget_usd = 0.000001\nticket_budget_usd = 0.000001",
        )
        .unwrap();
        assert_eq!(smallest.policy.resolver().ceiling_micros, Some(1));
        assert_eq!(smallest.policy.ticket_budget_micros(), Some(1));
    }

    /// OWL-378: the ticket budget is optional, a positive number of dollars,
    /// and does not stand in for the ceiling `"unlimited"` needs.
    #[test]
    fn the_ticket_budget_is_optional_and_checked() {
        let absent = project("", "").unwrap();
        assert_eq!(absent.policy.ticket_budget_micros(), None);
        assert!(!absent.render().contains("ticket_budget"));
        let set = project("", "ticket_budget_usd = 100").unwrap();
        assert_eq!(set.policy.ticket_budget_micros(), Some(100_000_000));
        assert_eq!(ProjectConfig::parse(&set.render()).unwrap(), set);
        for bad in ["0", "-1.5", "nan", "inf", "\"5\""] {
            let error = project("", &format!("ticket_budget_usd = {bad}"))
                .unwrap_err()
                .to_string();
            assert!(error.contains("ticket_budget_usd"), "{bad}: {error}");
        }
        let error = project(
            "",
            "resolver_passes = \"unlimited\"\nticket_budget_usd = 100",
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("policy.resolver_budget_usd"), "{error}");
    }

    /// A project file whose `[tracker]` admits by `admit` and adds `extra`.
    fn project_with_tracker(admit: &str, extra: &str) -> Result<ProjectConfig, ContractError> {
        ProjectConfig::parse(&format!(
            r#"
            requires = ">=0.1"
            [tracker]
            kind = "markdown"
            admit = {admit}
            {extra}
            states = {{ ready = "Todo", working = "Doing", needs_input = "Asked", review = "Review" }}
            [stack]
            gate = []
            [pipeline]
            default = "trivial"
            plan_approval = "never"
            [models]
            [policy]
            always_human = []
            "#
        ))
    }

    #[test]
    fn the_drains_tracker_keys_are_optional_and_checked() {
        let absent = project_with_tracker(r#"{ label = "agent" }"#, "").unwrap();
        assert_eq!(absent.tracker.admitters, None);
        assert_eq!(absent.tracker.exclude, None);
        let rendered = absent.render();
        assert!(
            !rendered.contains("admitters") && !rendered.contains("exclude"),
            "{rendered}"
        );

        // Identifiers are compared exactly, so these are two accounts.
        let set = project_with_tracker(
            r#"{ label = "agent" }"#,
            r#"admitters = ["u-ann", "U-ann"]
            exclude = "agent:hold""#,
        )
        .unwrap();
        assert_eq!(
            set.tracker.admitters.as_deref(),
            Some(&["u-ann".to_owned(), "U-ann".to_owned()][..])
        );
        assert_eq!(ProjectConfig::parse(&set.render()).unwrap(), set);

        for (admit, extra, reason) in [
            (
                r#"{ label = "agent" }"#,
                "admitters = []",
                "tracker.admitters lists no account",
            ),
            (
                r#"{ label = "agent" }"#,
                r#"admitters = ["u-ann", "u-ann"]"#,
                "tracker.admitters: \"u-ann\" is listed twice",
            ),
            (
                r#"{ label = "agent" }"#,
                r#"admitters = ["u-ann "]"#,
                "tracker.admitters: \"u-ann \" has a space at either end",
            ),
            (
                r#"{ label = "agent" }"#,
                r#"admitters = [" "]"#,
                "tracker.admitters: \" \" is blank",
            ),
            (
                r#"{ label = "agent" }"#,
                r#"admitters = ["u-ann\u0007"]"#,
                "holds a control character",
            ),
            (
                r#"{ label = "Ágent" }"#,
                r#"exclude = "ágent""#,
                "tracker.exclude \"ágent\" is the admission label",
            ),
            (
                r#"{ label = "agent" }"#,
                "exclude = ''",
                "tracker.exclude \"\" is blank",
            ),
            (
                r#"{ state = "Doing" }"#,
                "",
                "tracker.admit names the state \"Doing\", but a state gesture is the move to \
                 the ready column: it must be tracker.states.ready, \"Todo\"",
            ),
        ] {
            let error = project_with_tracker(admit, extra).unwrap_err().to_string();
            assert!(
                error.starts_with("invalid owlshift.toml: ") && error.contains(reason),
                "{extra}: {error}"
            );
        }

        // A state gesture naming the ready column is accepted, with any
        // exclusion label.
        let state = project_with_tracker(r#"{ state = "Todo" }"#, r#"exclude = "Todo""#).unwrap();
        assert_eq!(state.tracker.admit, Admit::State("Todo".into()));
    }

    #[test]
    fn the_open_pull_request_cap_is_a_policy_key_with_a_default_and_a_range() {
        let absent = project("", "").unwrap();
        assert_eq!(absent.policy.max_open_prs(), DEFAULT_MAX_OPEN_PRS);
        assert!(!absent.render().contains("max_open_prs"));

        let set = project("", "max_open_prs = 20").unwrap();
        assert_eq!(set.policy.max_open_prs(), 20);
        assert_eq!(ProjectConfig::parse(&set.render()).unwrap(), set);
        project("", "max_open_prs = 1").unwrap();

        for bad in ["0", "21", "300", "-1", "2.5", "\"3\""] {
            let error = project("", &format!("max_open_prs = {bad}"))
                .unwrap_err()
                .to_string();
            assert!(error.contains("max_open_prs"), "{bad}: {error}");
        }
        let error = project("", "max_open_prs = 0").unwrap_err().to_string();
        assert!(error.contains("from 1 to 20"), "{error}");
    }

    /// OWL-402: `stack.resources` is checked when the file is loaded, the
    /// core's refusal named under its key; a bad entry refuses the whole
    /// file, so every reading of it (gate, rules, intake) stops alike.
    #[test]
    fn named_resources_are_checked_when_the_file_is_loaded() {
        let config = project_with_stack(
            r#"resources = { migrations = "backend/*/migrations/**", "db.lock" = "db.lock" }"#,
        )
        .unwrap();
        let named = config.stack.named_resources().unwrap();
        let listed: Vec<(&str, &str)> = named.iter().map(|(n, p)| (n, p.as_str())).collect();
        assert_eq!(
            listed,
            [
                ("db.lock", "db.lock"),
                ("migrations", "backend/*/migrations/**")
            ]
        );
        assert!(
            project_with_stack("")
                .unwrap()
                .stack
                .named_resources()
                .unwrap()
                .is_empty()
        );
        for (entry, error) in [
            (
                r#"resources = { sql = "db/*.sql" }"#,
                "stack.resources: \"sql\": the name or its pattern has a part holding `*`",
            ),
            (
                r#"resources = { "a b" = "db" }"#,
                "stack.resources: \"a b\": the name or its pattern may hold only",
            ),
            (
                r#"resources = { m = "db/migrations/" }"#,
                "stack.resources: \"m\": the name or its pattern ends with `/`",
            ),
        ] {
            let actual = project_with_stack(entry).unwrap_err().to_string();
            assert!(
                actual.starts_with(&format!("invalid owlshift.toml: {error}")),
                "{entry}: {actual}"
            );
        }
    }

    #[test]
    fn zones_name_their_owners() {
        let absent = project_with_stack("").unwrap();
        assert!(absent.zones.is_empty());
        assert!(!absent.render().contains("zones"));

        let config = project(
            "",
            r#"
            [zones."backend/billing"]
            owner = "u-bob"
            [zones."backend/billing/tax"]
            owner = "u-tia"
            "#,
        )
        .unwrap();
        let owners = config.zone_owners().unwrap();
        assert_eq!(owners.owner_of("backend/billing/api"), Some("u-bob"));
        assert_eq!(owners.owner_of("backend/billing/tax/vat"), Some("u-tia"));
        let rendered = config.render();
        assert!(
            rendered.contains("[zones.\"backend/billing\"]\nowner = \"u-bob\""),
            "{rendered}"
        );
        assert_eq!(ProjectConfig::parse(&rendered).unwrap(), config);

        // The core's refusals reach the file's reader under `zones`.
        let error = project(
            "",
            r#"
            [zones.Billing]
            owner = "u-bob"
            [zones."billing/"]
            owner = "u-tia"
            "#,
        )
        .unwrap_err()
        .to_string();
        assert_eq!(
            error,
            "invalid owlshift.toml: zones: \"Billing\" and \"billing/\" name the same zone"
        );

        // A configuration built in code is checked when its owners are read.
        let mut built = config;
        built.zones.insert(
            "../secrets".to_owned(),
            ZoneSettings {
                owner: "u-eve".to_owned(),
            },
        );
        assert!(
            built
                .zone_owners()
                .unwrap_err()
                .to_string()
                .contains("`.` or `..`")
        );
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
    fn roles_add_rules_to_the_briefs_that_carry_them() {
        let absent = project_with_stack("").unwrap();
        assert!(absent.roles.is_empty());
        assert!(!absent.render().contains("roles"));
        assert!(absent.role_rule_files(Role::Build).is_empty());

        let tail = "[roles.build]\nrules = [\"docs/skills/frontend/SKILL.md\"]\n\
                    [roles.resolver]\nrules = []\n\
                    [roles.intake]\nrules = [\"docs/zones.md\"]\n\
                    [roles.design]\nrules = [\"docs/design.md\"]\n\
                    [roles.design_review]\nrules = [\"docs/review.md\"]\n\
                    [roles.verify]\nrules = [\"docs/verify.md\"]\n\
                    [roles.arbiter]\nrules = [\"docs/arbiter.md\"]";
        let named = project("", tail).unwrap();
        assert_eq!(
            named.role_rule_files(Role::Build),
            ["docs/skills/frontend/SKILL.md"]
        );
        assert!(named.role_rule_files(Role::Resolver).is_empty());
        assert_eq!(named.role_rule_files(Role::Intake), ["docs/zones.md"]);
        assert_eq!(named.role_rule_files(Role::Design), ["docs/design.md"]);
        assert_eq!(
            named.role_rule_files(Role::DesignReview),
            ["docs/review.md"]
        );
        assert_eq!(named.role_rule_files(Role::Verify), ["docs/verify.md"]);
        assert_eq!(named.role_rule_files(Role::Arbiter), ["docs/arbiter.md"]);
        assert_eq!(ProjectConfig::parse(&named.render()).unwrap(), named);

        // 32 files for the role alone fit only beside an empty `stack.rules`.
        let many: Vec<String> = (0..32).map(|i| format!("\"r{i}.md\"")).collect();
        let many = format!("[roles.build]\nrules = [{}]", many.join(", "));
        assert!(project("rules = []", &many).is_ok());
        let over = project("", &many).unwrap_err().to_string();
        assert!(
            over.contains("roles.build.rules: 33 files named, over the 32"),
            "{over}"
        );

        for (case, stack, tail, reason) in [
            (
                "answer_check",
                "",
                "[roles.answer_check]\nrules = [\"a.md\"]",
                "roles.answer_check.rules: the answer check classifies",
            ),
            (
                "an unknown role",
                "",
                "[roles.bulid]\nrules = [\"a.md\"]",
                "roles.bulid.rules: `bulid` is not a role whose brief carries rules \
                 (a role joins as it is built); the roles that do: build, resolver, intake, design, design_review, verify, arbiter",
            ),
            (
                "a path that climbs",
                "",
                "[roles.build]\nrules = [\"../a.md\"]",
                "roles.build.rules: \"../a.md\" is not a file's path",
            ),
            (
                "a path twice",
                "",
                "[roles.build]\nrules = [\"a.md\", \"a.md\"]",
                "roles.build.rules: \"a.md\" is named twice",
            ),
            (
                "a path of stack.rules",
                r#"rules = ["docs/x.md"]"#,
                "[roles.build]\nrules = [\"docs/x.md\"]",
                "roles.build.rules: \"docs/x.md\" is also a rule of `stack.rules`",
            ),
            (
                "the default path",
                "",
                "[roles.intake]\nrules = [\"AGENTS.md\"]",
                "roles.intake.rules: \"AGENTS.md\" is also a rule of `stack.rules`",
            ),
        ] {
            let error = project(stack, tail).unwrap_err().to_string();
            assert!(
                error.starts_with("invalid owlshift.toml: ") && error.contains(reason),
                "{case}: {error}"
            );
        }
        // `stack.rules = []` drops the default, so a role may name it.
        assert!(project("rules = []", "[roles.intake]\nrules = [\"AGENTS.md\"]").is_ok());

        // A table with no `rules`, or with another key, is refused.
        for tail in ["[roles.build]", "[roles.build]\nrules = []\nmodel = \"x\""] {
            assert!(project("", tail).is_err(), "{tail}");
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
            (r#"["NODE_OPTIONS"]"#, "run code as it starts"),
            (r#"["CODEX_REFRESH_TOKEN_URL_OVERRIDE"]"#, "Codex"),
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
             machine: the operator allows a name in the personal configuration, for every \
             repository in `allow_gate_env`, or for one in the `allow_gate_env` of its \
             `[repositories.\"github.com/<owner>/<name>\"]` table"
        );

        let personal = PersonalConfig::parse("allow_gate_env = [\"DATABASE_URL\"]\n").unwrap();
        assert_eq!(personal.allow_gate_env_names(None), ["DATABASE_URL"]);
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

    /// OWL-75: a name allowed for one repository is not allowed for another.
    #[test]
    fn a_name_can_be_allowed_for_one_repository() {
        let personal = PersonalConfig::parse(
            r#"
            allow_gate_env = ["JAVA_HOME"]
            [repositories."github.com/Acme/API"]
            allow_gate_env = ["DATABASE_URL"]
            "#,
        )
        .unwrap();
        assert_eq!(
            personal.allow_gate_env_names(Some("github.com/acme/api")),
            ["JAVA_HOME", "DATABASE_URL"]
        );
        assert_eq!(
            personal.allow_gate_env_names(Some("github.com/acme/web")),
            ["JAVA_HOME"]
        );
        assert_eq!(personal.allow_gate_env_names(None), ["JAVA_HOME"]);
        assert!(
            personal
                .render()
                .contains("[repositories.\"github.com/Acme/API\"]")
        );

        for (input, error) in [
            (
                "[repositories.\"acme/api\"]",
                "repositories.\"acme/api\": a repository is written github.com/<owner>/<name>",
            ),
            (
                "[repositories.\"github.com/acme/api/extra\"]",
                "repositories.\"github.com/acme/api/extra\": a repository is written",
            ),
            (
                // Every entry is checked, not only the current repository's.
                "[repositories.\"github.com/acme/web\"]\nallow_gate_env = [\"GH_TOKEN\"]",
                "repositories.\"github.com/acme/web\".allow_gate_env: agents never receive \
                 credentials",
            ),
        ] {
            let actual = PersonalConfig::parse(input).unwrap_err().to_string();
            assert!(
                actual.starts_with(&format!("invalid personal configuration: {error}")),
                "{input}: {actual}"
            );
        }
    }

    /// OWL-330: `stack.setup` and `stack.caches` round-trip, and a blank
    /// command or a cache folder that is not plain, or that would open a
    /// credential path, makes the file invalid.
    #[test]
    fn setup_and_caches_are_read_and_checked() {
        let config =
            project_with_stack("setup = [\"pnpm install\"]\ncaches = [\"Library/pnpm\"]").unwrap();
        assert_eq!(config.stack.setup, ["pnpm install"]);
        assert_eq!(config.stack.cache_folders(), ["Library/pnpm"]);
        let rendered = config.render();
        assert_eq!(ProjectConfig::parse(&rendered).unwrap(), config);
        let bare = project_with_stack("").unwrap();
        assert!(bare.stack.setup.is_empty() && bare.stack.caches.is_empty());
        assert!(!bare.render().contains("setup") && !bare.render().contains("caches"));

        let error = project_with_stack("setup = [\"make deps\", \" \"]")
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            "invalid owlshift.toml: stack.setup: command 2 is blank"
        );
        for cache in [
            "../x",
            "/abs",
            "~/x",
            "",
            ".ssh",
            ".config",
            ".config/gh/hosts.yml",
            "Library/Keychains",
        ] {
            let error = project_with_stack(&format!("caches = [{cache:?}]"))
                .unwrap_err()
                .to_string();
            assert!(
                error.starts_with("invalid owlshift.toml: stack.caches: "),
                "{cache:?}: {error}"
            );
        }
        let error = project_with_stack("caches = [\"a\", \"a\"]")
            .unwrap_err()
            .to_string();
        assert!(error.contains("named twice"), "{error}");
    }

    /// OWL-330: a declared cache passes only once the operator lists it,
    /// machine-wide or for the repository, compared as written; another
    /// repository's list does not count.
    #[test]
    fn caches_pass_only_what_the_operator_allows() {
        let config = project_with_stack("caches = [\"Library/pnpm\", \".cache/uv\"]").unwrap();
        let error = config
            .check_caches(&["Library/pnpm"])
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            "invalid owlshift.toml: stack.caches: \".cache/uv\" may not be read by an agent on \
             this machine: the operator allows a cache folder in the personal configuration, \
             for every repository in `allow_caches`, or for one in the `allow_caches` of its \
             `[repositories.\"github.com/<owner>/<name>\"]` table"
        );
        assert!(config.check_caches(&["library/pnpm", ".cache/uv"]).is_err());
        let personal = PersonalConfig::parse(
            r#"
            allow_caches = ["Library/pnpm"]
            [repositories."github.com/acme/api"]
            allow_caches = [".cache/uv"]
            [repositories."github.com/acme/web"]
            allow_caches = [".cache/pip"]
            "#,
        )
        .unwrap();
        assert_eq!(
            personal.allow_caches_names(Some("github.com/Acme/API")),
            ["Library/pnpm", ".cache/uv"]
        );
        assert_eq!(personal.allow_caches_names(None), ["Library/pnpm"]);
        config
            .check_caches(&personal.allow_caches_names(Some("github.com/acme/api")))
            .unwrap();
        assert!(
            config
                .check_caches(&personal.allow_caches_names(Some("github.com/acme/web")))
                .is_err()
        );
        project_with_stack("").unwrap().check_caches(&[]).unwrap();
    }

    /// OWL-330: the operator's list is held to the same rules, machine-wide
    /// and for each repository: listing a credential path does not open it.
    #[test]
    fn allow_caches_refuses_what_no_project_may_open() {
        for (input, error) in [
            (
                "allow_caches = [\".ssh\"]",
                "allow_caches: \".ssh\" would open",
            ),
            ("allow_caches = [\"../x\"]", "allow_caches: \"../x\" is not"),
            (
                "allow_caches = [\"a\", \"a\"]",
                "allow_caches: \"a\" is named twice",
            ),
            (
                "[repositories.\"github.com/acme/api\"]\nallow_caches = [\".config/gh\"]",
                "repositories.\"github.com/acme/api\".allow_caches: \".config/gh\" would open",
            ),
        ] {
            let actual = PersonalConfig::parse(input).unwrap_err().to_string();
            assert!(
                actual.starts_with(&format!("invalid personal configuration: {error}")),
                "{input}: {actual}"
            );
        }
    }

    /// OWL-382: the GitHub App's overrides (D13), checked before the slug
    /// reaches a link.
    #[test]
    fn the_github_app_overrides_are_read_and_checked() {
        let personal = PersonalConfig::parse(
            "github_app_client_id = \"Iv1.0123abcd\"\ngithub_app_slug = \"my-owlshift2\"\n",
        )
        .unwrap();
        assert_eq!(
            (
                personal.github_app_client_id.as_deref(),
                personal.github_app_slug.as_deref()
            ),
            (Some("Iv1.0123abcd"), Some("my-owlshift2"))
        );
        let none = PersonalConfig::parse("").unwrap();
        assert_eq!(
            (none.github_app_client_id, none.github_app_slug),
            (None, None)
        );

        for (input, key) in [
            ("github_app_client_id = \"\"", "github_app_client_id"),
            ("github_app_client_id = \"Iv1 x\"", "github_app_client_id"),
            ("github_app_slug = \"\"", "github_app_slug"),
            ("github_app_slug = \"My-App\"", "github_app_slug"),
            ("github_app_slug = \"-app\"", "github_app_slug"),
            ("github_app_slug = \"app/../x\"", "github_app_slug"),
        ] {
            let actual = PersonalConfig::parse(input).unwrap_err().to_string();
            assert!(
                actual.starts_with(&format!("invalid personal configuration: {key}:")),
                "{input}: {actual}"
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
