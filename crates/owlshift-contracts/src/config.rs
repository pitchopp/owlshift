//! Project and personal configuration, in TOML.
//!
//! Only the keys the design names so far exist; later work adds keys, and a
//! project file's `requires` tells an older binary to upgrade. Read
//! `requires` with [`peek_requires`] before the strict parse, so an old binary
//! reports "upgrade" rather than an unknown key.

use std::collections::BTreeMap;
use std::num::NonZeroU32;

use schemars::JsonSchema;
use semver::VersionReq;
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

    pub fn validate(&self) -> Result<(), ContractError> {
        if self.tracker.kind == TrackerKind::Linear && self.tracker.team.is_none() {
            return Err(ContractError::invalid(
                PROJECT,
                "tracker.team is required for Linear",
            ));
        }
        Ok(())
    }
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

    pub fn validate(&self) -> Result<(), ContractError> {
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
            contract: "configuration",
            source,
        })
}
