//! Per-ticket routing in the runner (OWL-305, architecture section 5,
//! "Per-ticket routing"): the `tier:` and `effort:` labels read off the
//! ticket at dispatch, the model of each tier, and the [`Router`] that gives
//! a run the harness its routing names.
//!
//! The rules are the core's ([`owlshift_core::routing`]): the ticket's
//! routing from intake's verdict under the labels' values, then each role's
//! layer. The runner parses the labels, maps a tier to a model, and builds
//! the harness.

use owlshift_contracts::config::Models;
use owlshift_core::routing::RunRouting;
use owlshift_core::vocab::{Effort, Tier};

use crate::executor::Harness;
use crate::executor::harness::ClaudeHarness;

/// The prefix of a label that sets the ticket's tier, matched without
/// regard to case, as `zone:` is.
pub const TIER_LABEL_PREFIX: &str = "tier:";
/// The prefix of a label that sets the ticket's effort.
pub const EFFORT_LABEL_PREFIX: &str = "effort:";

/// Gives a run the harness its routing names. One router serves one harness
/// family: the caller picks it by the routing's `harness`, which the core's
/// `role_routing` gives as Claude Code for every authoring role in P5 (D4).
/// The review roles of Design review and Verify go through it with
/// `seat_routing`'s routing, on a router of their seat's harness.
pub trait Router {
    /// The harness of a run routed `routing`.
    fn harness(&self, routing: RunRouting) -> Box<dyn Harness + '_>;
}

/// The Claude Code model of `tier` in `[models]`, `None` for the harness's
/// own default: a tier the project leaves out runs on it, except `fast`,
/// which falls back to the standard tier's model (architecture section 5,
/// "The tiers").
pub fn claude_model(models: &Models, tier: Tier) -> Option<&str> {
    let of = |tier: Tier| {
        match tier {
            Tier::Deep => &models.deep,
            Tier::Standard => &models.standard,
            Tier::Fast => &models.fast,
        }
        .as_ref()
        .and_then(|models| models.claude.as_deref())
    };
    of(tier).or_else(|| match tier {
        Tier::Fast => of(Tier::Standard),
        Tier::Deep | Tier::Standard => None,
    })
}

/// Claude Code, routed: one role's harness, given the model of the
/// routing's tier and its effort for each run.
#[derive(Clone, Debug, PartialEq)]
pub struct ClaudeRouter {
    /// The role's harness: its program, prompt, budget and login.
    pub harness: ClaudeHarness,
    /// The project's `[models]`.
    pub models: Models,
}

impl ClaudeRouter {
    /// The harness of a run routed `routing`.
    pub fn claude(&self, routing: RunRouting) -> ClaudeHarness {
        ClaudeHarness {
            model: claude_model(&self.models, routing.tier).map(str::to_owned),
            effort: routing.effort,
            ..self.harness.clone()
        }
    }
}

impl Router for ClaudeRouter {
    fn harness(&self, routing: RunRouting) -> Box<dyn Harness + '_> {
        Box::new(self.claude(routing))
    }
}

/// What a ticket's `tier:` and `effort:` labels give its routing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RouteLabels {
    pub tier: Option<Tier>,
    pub effort: Option<Effort>,
    /// The labels left out, in the labels' order, each with the reason: a
    /// value that names no tier or effort, or one that two labels of the
    /// same prefix disagree on, both then left out. The runner tells them
    /// as `warning` events.
    pub ignored: Vec<IgnoredLabel>,
}

/// A routing label left out, and why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IgnoredLabel {
    pub label: String,
    pub reason: String,
}

const TIERS: [(&str, Tier); 3] = [
    ("deep", Tier::Deep),
    ("standard", Tier::Standard),
    ("fast", Tier::Fast),
];

const EFFORTS: [Effort; 5] = [
    Effort::Low,
    Effort::Medium,
    Effort::High,
    Effort::Xhigh,
    Effort::Max,
];

/// Reads the `tier:` and `effort:` labels among `labels`: the prefix
/// without regard to case, the value trimmed and without regard to case.
/// Other labels play no part. A label repeating a value already read
/// counts once.
pub fn route_labels<'a>(labels: impl IntoIterator<Item = &'a str>) -> RouteLabels {
    let labels: Vec<&str> = labels.into_iter().collect();
    let mut ignored = Vec::new();
    let tier = read_prefix(&labels, TIER_LABEL_PREFIX, &mut ignored, |value| {
        TIERS
            .iter()
            .find(|(name, _)| *name == value)
            .map(|(_, tier)| *tier)
            .ok_or_else(|| format!("`{value}` is not a tier: deep, standard or fast"))
    });
    let effort = read_prefix(&labels, EFFORT_LABEL_PREFIX, &mut ignored, |value| {
        EFFORTS
            .into_iter()
            .find(|effort| effort.as_str() == value)
            .ok_or_else(|| format!("`{value}` is not an effort: low, medium, high, xhigh or max"))
    });
    // In the labels' order, whichever prefix left each out.
    ignored.sort_by_key(|left: &IgnoredLabel| labels.iter().position(|label| *label == left.label));
    RouteLabels {
        tier,
        effort,
        ignored,
    }
}

/// The one value the labels of `prefix` give, those naming none left out
/// in `ignored`; `None`, every valid one left out too, when they disagree.
fn read_prefix<T: Copy + PartialEq>(
    labels: &[&str],
    prefix: &str,
    ignored: &mut Vec<IgnoredLabel>,
    parse: impl Fn(&str) -> Result<T, String>,
) -> Option<T> {
    let mut read: Vec<(&str, T)> = Vec::new();
    for label in labels {
        let Some(head) = label.get(..prefix.len()) else {
            continue;
        };
        if !head.eq_ignore_ascii_case(prefix) {
            continue;
        }
        let value = label[prefix.len()..].trim().to_ascii_lowercase();
        match parse(&value) {
            Ok(value) => read.push((label, value)),
            Err(reason) => ignored.push(IgnoredLabel {
                label: (*label).to_owned(),
                reason,
            }),
        }
    }
    let first = read.first()?.1;
    if read.iter().all(|(_, value)| *value == first) {
        return Some(first);
    }
    let names: Vec<&str> = read.iter().map(|(label, _)| *label).collect();
    for (label, _) in &read {
        let others: Vec<String> = names
            .iter()
            .filter(|other| *other != label)
            .map(|other| format!("`{other}`"))
            .collect();
        ignored.push(IgnoredLabel {
            label: (*label).to_owned(),
            reason: format!("it disagrees with {}", others.join(", ")),
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use owlshift_contracts::config::TierModels;
    use owlshift_core::vocab::Harness;

    use super::*;

    fn labels(names: &[&str]) -> RouteLabels {
        route_labels(names.iter().copied())
    }

    fn ignored(label: &str, reason: &str) -> IgnoredLabel {
        IgnoredLabel {
            label: label.to_owned(),
            reason: reason.to_owned(),
        }
    }

    #[test]
    fn the_routing_labels_are_read_and_the_rest_ignored() {
        let read = labels(&["Feature", "Tier: Deep ", "effort:XHIGH", "zone:docs"]);
        assert_eq!(read.tier, Some(Tier::Deep));
        assert_eq!(read.effort, Some(Effort::Xhigh));
        assert_eq!(read.ignored, []);

        // A repeated value counts once.
        let read = labels(&["tier:fast", "TIER:fast"]);
        assert_eq!((read.tier, read.ignored.len()), (Some(Tier::Fast), 0));

        assert_eq!(
            labels(&["tiered", "effortless", "é"]),
            RouteLabels::default()
        );
    }

    #[test]
    fn an_unknown_or_disputed_value_is_left_out_with_its_reason() {
        let read = labels(&[
            "effort:huge",
            "tier:deep",
            "tier:",
            "tier:fast",
            "effort:low",
        ]);
        assert_eq!(read.tier, None, "two tiers disagree");
        assert_eq!(read.effort, Some(Effort::Low), "the valid one stands");
        assert_eq!(
            read.ignored,
            [
                ignored(
                    "effort:huge",
                    "`huge` is not an effort: low, medium, high, xhigh or max"
                ),
                ignored("tier:deep", "it disagrees with `tier:fast`"),
                ignored("tier:", "`` is not a tier: deep, standard or fast"),
                ignored("tier:fast", "it disagrees with `tier:deep`"),
            ]
        );
    }

    fn models(deep: Option<&str>, standard: Option<&str>, fast: Option<&str>) -> Models {
        let tier = |model: Option<&str>| {
            model.map(|model| TierModels {
                claude: Some(model.to_owned()),
                codex: None,
            })
        };
        Models {
            deep: tier(deep),
            standard: tier(standard),
            fast: tier(fast),
        }
    }

    #[test]
    fn a_tier_left_out_runs_the_harness_default_but_fast_falls_back_to_standard() {
        let all = models(Some("opus"), Some("sonnet"), Some("haiku"));
        let standard = models(None, Some("sonnet"), None);
        let none = models(None, None, None);
        let cases = [
            (&all, Tier::Deep, Some("opus")),
            (&all, Tier::Fast, Some("haiku")),
            (&standard, Tier::Deep, None),
            (&standard, Tier::Standard, Some("sonnet")),
            (&standard, Tier::Fast, Some("sonnet")),
            (&none, Tier::Fast, None),
        ];
        for (models, tier, expected) in cases {
            assert_eq!(claude_model(models, tier), expected, "{tier:?}");
        }
    }

    #[test]
    fn the_router_gives_the_tiers_model_and_the_effort() {
        let router = ClaudeRouter {
            harness: ClaudeHarness {
                program: PathBuf::from("claude"),
                prompt: "# Build".to_owned(),
                model: Some("set aside".to_owned()),
                effort: Some(Effort::Max),
                max_budget_usd: Some(5.0),
                login: None,
            },
            models: models(Some("opus"), Some("sonnet"), None),
        };
        let routed = router.claude(RunRouting {
            harness: Harness::Claude,
            tier: Tier::Deep,
            effort: Some(Effort::Low),
        });
        assert_eq!(
            routed,
            ClaudeHarness {
                model: Some("opus".to_owned()),
                effort: Some(Effort::Low),
                ..router.harness.clone()
            }
        );
        let default = router.claude(RunRouting {
            harness: Harness::Claude,
            tier: Tier::Fast,
            effort: None,
        });
        assert_eq!(
            (default.model.as_deref(), default.effort),
            (Some("sonnet"), None)
        );
    }
}
