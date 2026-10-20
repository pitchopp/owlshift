//! The roster: who reviews a ticket's plan and diff, per variant
//! (architecture, section 5, "The quality pipeline", "The roster").
//!
//! Every reviewer runs on Claude Code (OWL-347), each on another model than
//! the author's when the configuration has one ([`other_model`]); the
//! seats differ by their mandates. A project may add one Codex reviewer
//! after them ([`CODEX_REVIEWER`], OWL-348).

use crate::vocab::{Mandate, Stage, Tier, Variant};

/// One reviewer of a review pass, on Claude Code.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Seat {
    /// What it looks for, at least one, each once.
    pub mandates: &'static [Mandate],
}

/// A critic alone in its pass holds conformance too.
const CRITIC_ALONE: &[Mandate] = &[Mandate::Critic, Mandate::Conformance];
const CRITIC: &[Mandate] = &[Mandate::Critic];
const CONFORMANCE: &[Mandate] = &[Mandate::Conformance];
const DEBT: &[Mandate] = &[Mandate::Debt];

/// The opt-in Codex reviewer (OWL-348, `pipeline.codex_reviewer`): one
/// more seat after the roster's, on the critic's mandate, a reviewer of
/// another model family than the author's (scenario S17). It is never part
/// of [`roster`]: a Codex that cannot serve a pass drops it, and the pass
/// keeps the roster's seats.
pub const CODEX_REVIEWER: Seat = Seat { mandates: CRITIC };

/// The reviewers of a pass at `stage` for a ticket of `variant`, the critic
/// first; none at a stage that is not Design review or Verify, nor at
/// Design review for `trivial`.
///
/// | Variant | Design review | Verify |
/// | --- | --- | --- |
/// | `trivial` | none | the critic |
/// | `standard` | the critic; conformance | the critic |
/// | `risky` | the critic; conformance and debt | the critic; conformance |
pub fn roster(variant: Variant, stage: Stage) -> Vec<Seat> {
    let others: &[&'static [Mandate]] = match (stage, variant) {
        (Stage::DesignReview, Variant::Trivial) => return Vec::new(),
        (Stage::DesignReview, Variant::Standard) | (Stage::Verify, Variant::Risky) => {
            &[CONFORMANCE]
        }
        (Stage::DesignReview, Variant::Risky) => &[CONFORMANCE, DEBT],
        (Stage::Verify, Variant::Trivial | Variant::Standard) => &[],
        _ => return Vec::new(),
    };
    let critic = Seat {
        mandates: if others.is_empty() {
            CRITIC_ALONE
        } else {
            CRITIC
        },
    };
    std::iter::once(critic)
        .chain(others.iter().map(|&mandates| Seat { mandates }))
        .collect()
}

/// The tier a reviewer on Claude Code runs on, and whether its model is
/// the author's after all.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ReviewerModel {
    pub tier: Tier,
    /// No tier names another model than the author's: the reviewer is a
    /// fresh run of the author's model, on the author's tier, and the
    /// report says so.
    pub same_model: bool,
}

/// The model of a reviewer on Claude Code, the critic included: the
/// `deep` tier's, or the `standard` tier's when the author
/// ran on `deep`; when that tier names no model or the author's, the other
/// of the two; when both do, the author's own (`same_model`). `model`
/// gives the Claude Code model the configuration names for a tier, `None`
/// for the CLI's default, and `author` is the tier the author ran on.
pub fn other_model<'a>(author: Tier, model: impl Fn(Tier) -> Option<&'a str>) -> ReviewerModel {
    let author_model = model(author);
    let order = if author == Tier::Deep {
        [Tier::Standard, Tier::Deep]
    } else {
        [Tier::Deep, Tier::Standard]
    };
    order
        .into_iter()
        .find(|&tier| model(tier).is_some_and(|named| Some(named) != author_model))
        .map_or(
            ReviewerModel {
                tier: author,
                same_model: true,
            },
            |tier| ReviewerModel {
                tier,
                same_model: false,
            },
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn seat(mandates: &'static [Mandate]) -> Seat {
        Seat { mandates }
    }

    #[test]
    fn the_roster_follows_the_variant_and_the_stage() {
        use Stage::{DesignReview, Verify};
        use Variant::{Risky, Standard, Trivial};
        let critic = seat(CRITIC);
        let alone = seat(CRITIC_ALONE);
        let conformance = seat(CONFORMANCE);
        let debt = seat(DEBT);
        let cases: [(Variant, Stage, &[Seat]); 6] = [
            (Trivial, DesignReview, &[]),
            (Trivial, Verify, &[alone]),
            (Standard, DesignReview, &[critic, conformance]),
            (Standard, Verify, &[alone]),
            (Risky, DesignReview, &[critic, conformance, debt]),
            (Risky, Verify, &[critic, conformance]),
        ];
        for (variant, stage, expected) in cases {
            assert_eq!(roster(variant, stage), expected, "{variant:?} {stage:?}");
        }
        for stage in [Stage::Intake, Stage::Design, Stage::Build, Stage::Watch] {
            assert!(roster(Risky, stage).is_empty(), "{stage:?}");
        }
    }

    #[test]
    fn a_reviewer_on_claude_code_takes_another_model_than_the_authors() {
        use Tier::{Deep, Fast, Standard};
        let other = |tier| ReviewerModel {
            tier,
            same_model: false,
        };
        let same = |tier| ReviewerModel {
            tier,
            same_model: true,
        };
        // (author's tier, deep's model, standard's model, fast's model,
        // expected).
        let cases = [
            (Standard, Some("opus"), Some("sonnet"), None, other(Deep)),
            (
                Fast,
                Some("opus"),
                Some("sonnet"),
                Some("haiku"),
                other(Deep),
            ),
            (Deep, Some("opus"), Some("sonnet"), None, other(Standard)),
            // The preferred tier names the author's model, or none: the
            // other tier, when it names another.
            (
                Standard,
                Some("sonnet"),
                Some("sonnet"),
                None,
                same(Standard),
            ),
            (Fast, None, Some("sonnet"), Some("haiku"), other(Standard)),
            (Deep, Some("opus"), None, None, same(Deep)),
            (Deep, None, Some("sonnet"), None, other(Standard)),
            // Nothing named: the CLI's default for everyone.
            (Standard, None, None, None, same(Standard)),
        ];
        for (author, deep, standard, fast, expected) in cases {
            let model = |tier| match tier {
                Deep => deep,
                Standard => standard,
                Fast => fast,
            };
            assert_eq!(
                other_model(author, model),
                expected,
                "{author:?}: {deep:?} {standard:?} {fast:?}"
            );
        }
    }
}
