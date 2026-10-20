//! A harness's usage state on one machine (OWL-404, architecture section 5,
//! "Parallel", "Usage limits"): `<data>/usage/<harness>.json`, shared by every
//! project's commands on the machine, written under a lock of its own and
//! replaced whole. What it means is the core's
//! ([`owlshift_core::usage`]); this is its file.
//!
//! A window's `read_at` is the time its report was read from a run's stream,
//! by the machine's clock, and is compared only with another report's; the
//! episodes' and the probe's times come from the command's clock.

use std::collections::BTreeMap;

use jiff::Timestamp;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use owlshift_core::usage::{Episode, EpisodeKind, Probe, State, Window};

use crate::Harness;
use crate::format::{self, ContractError, Format, USAGE_FORMAT};

/// One harness's usage state.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(title = "Owlshift harness usage")]
pub struct HarnessUsage {
    pub format: Format<USAGE_FORMAT>,
    pub harness: Harness,
    /// The usage windows its runs reported, by name, such as `five_hour`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub windows: BTreeMap<String, KeptWindow>,
    /// The times it is held.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub episodes: Vec<KeptEpisode>,
    /// The run probing it past a deadline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probe: Option<KeptProbe>,
}

/// A usage window as the latest report that counts gave it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct KeptWindow {
    /// The share used, from 0 to 1.
    pub share: f64,
    pub resets_at: Timestamp,
    /// When the report was read from its run's stream.
    pub read_at: Timestamp,
}

/// A time the harness is held.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct KeptEpisode {
    pub kind: KeptKind,
    /// When the outage was first seen.
    pub since: Timestamp,
    /// Its end: the reset time the harness reported, a deadline, or a
    /// cap's window reset.
    pub until: Timestamp,
    /// The operator was notified of it.
    #[serde(default)]
    pub told: bool,
}

/// What holds the harness.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum KeptKind {
    /// The provider's usage limit; `reset` when `until` is the reset time it
    /// reported, otherwise a deadline after which a probe runs.
    Limit { reset: bool },
    /// A cap's pause on `window` (OWL-405).
    Cap { window: String },
}

/// The run probing the harness.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct KeptProbe {
    pub id: String,
    /// Until when every other run waits for it.
    pub deadline: Timestamp,
}

impl HarnessUsage {
    const CONTRACT: &str = "harness usage";

    pub fn parse(input: &str) -> Result<Self, ContractError> {
        let usage: Self = format::parse_json(Self::CONTRACT, USAGE_FORMAT, input)?;
        if usage
            .windows
            .values()
            .any(|window| !window.share.is_finite())
        {
            return Err(ContractError::invalid(
                Self::CONTRACT,
                "a window's share is not a number",
            ));
        }
        Ok(usage)
    }

    pub fn render(&self) -> String {
        format::render_json(self)
    }

    /// The file of `harness` holding `state`.
    pub fn from_state(harness: Harness, state: &State) -> Self {
        Self {
            format: Format,
            harness,
            windows: state
                .windows
                .iter()
                .map(|(name, window)| {
                    let kept = KeptWindow {
                        share: window.share,
                        resets_at: at(window.resets_at_ms),
                        read_at: at(window.read_at_ms),
                    };
                    (name.clone(), kept)
                })
                .collect(),
            episodes: state
                .episodes
                .iter()
                .map(|episode| KeptEpisode {
                    kind: match &episode.kind {
                        EpisodeKind::Limit { reset } => KeptKind::Limit { reset: *reset },
                        EpisodeKind::Cap { window } => KeptKind::Cap {
                            window: window.clone(),
                        },
                    },
                    since: at(episode.since_ms),
                    until: at(episode.until_ms),
                    told: episode.told,
                })
                .collect(),
            probe: state.probe.as_ref().map(|probe| KeptProbe {
                id: probe.id.clone(),
                deadline: at(probe.deadline_ms),
            }),
        }
    }

    /// The state it holds.
    pub fn state(&self) -> State {
        State {
            windows: self
                .windows
                .iter()
                .map(|(name, kept)| {
                    let window = Window {
                        share: kept.share,
                        resets_at_ms: kept.resets_at.as_millisecond(),
                        read_at_ms: kept.read_at.as_millisecond(),
                    };
                    (name.clone(), window)
                })
                .collect(),
            episodes: self
                .episodes
                .iter()
                .map(|kept| Episode {
                    kind: match &kept.kind {
                        KeptKind::Limit { reset } => EpisodeKind::Limit { reset: *reset },
                        KeptKind::Cap { window } => EpisodeKind::Cap {
                            window: window.clone(),
                        },
                    },
                    since_ms: kept.since.as_millisecond(),
                    until_ms: kept.until.as_millisecond(),
                    told: kept.told,
                })
                .collect(),
            probe: self.probe.as_ref().map(|kept| Probe {
                id: kept.id.clone(),
                deadline_ms: kept.deadline.as_millisecond(),
            }),
        }
    }
}

/// The instant `ms` milliseconds after the epoch, held to the range a
/// timestamp has.
fn at(ms: i64) -> Timestamp {
    Timestamp::from_millisecond(ms).unwrap_or(if ms < 0 {
        Timestamp::MIN
    } else {
        Timestamp::MAX
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_state_survives_its_file() {
        let state = State {
            windows: BTreeMap::from([(
                "five_hour".to_owned(),
                Window {
                    share: 0.31,
                    resets_at_ms: 1_792_425_600_000,
                    read_at_ms: 1_792_410_000_123,
                },
            )]),
            episodes: vec![
                Episode {
                    kind: EpisodeKind::Limit { reset: false },
                    since_ms: 1_792_400_000_000,
                    until_ms: 1_792_401_800_000,
                    told: true,
                },
                Episode {
                    kind: EpisodeKind::Cap {
                        window: "seven_day".to_owned(),
                    },
                    since_ms: 1_792_400_000_000,
                    until_ms: 1_792_562_400_000,
                    told: false,
                },
            ],
            probe: Some(Probe {
                id: "4242-OWL-1-17".to_owned(),
                deadline_ms: 1_792_403_600_000,
            }),
        };
        let file = HarnessUsage::from_state(Harness::Claude, &state).render();
        let read = HarnessUsage::parse(&file).unwrap();
        assert_eq!(read.harness, Harness::Claude);
        assert_eq!(read.state(), state);
        assert!(file.contains(r#""kind": "limit""#), "{file}");
    }

    #[test]
    fn an_empty_state_is_a_short_file_and_a_newer_one_is_refused() {
        let file = HarnessUsage::from_state(Harness::Codex, &State::default()).render();
        assert_eq!(file, "{\n  \"format\": 1,\n  \"harness\": \"codex\"\n}\n");
        let newer = file.replace("\"format\": 1", "\"format\": 2");
        assert!(matches!(
            HarnessUsage::parse(&newer),
            Err(ContractError::NewerFormat { found: 2, .. })
        ));
    }
}
