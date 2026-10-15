//! Per-ticket routing: the harness, tier and effort of each run, resolved
//! in section 9's order (architecture, section 5, "Per-ticket routing"):
//! the project default, the role, the ticket's verdict with its labels
//! overriding it, then what the machine has.
//!
//! The first three layers are here; the machine's is the roster's
//! ([`crate::roster::roster`]), where a critic without a usable Codex
//! stands in on Claude Code. Reading the `tier:` and `effort:` labels off
//! the ticket is the runner's; the core takes their values.

use crate::roster::{Seat, other_model};
use crate::vocab::{Effort, Harness, Role, Routing, Tier};

/// What a run is given: its harness, its tier, and its effort, `None` for
/// the harness's default.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RunRouting {
    pub harness: Harness,
    pub tier: Tier,
    pub effort: Option<Effort>,
}

/// The project default, the first layer: P5 has no key that changes it.
pub const PROJECT_DEFAULT: RunRouting = RunRouting {
    harness: Harness::Claude,
    tier: Tier::Standard,
    effort: None,
};

/// The ticket's own routing, what its verdict and labels leave: the tier
/// and effort its authoring runs and its reviewers follow.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TicketRouting {
    pub tier: Tier,
    pub effort: Option<Effort>,
    /// False when the verdict named a harness the authoring runs do not
    /// take: they stay on Claude Code in P5 (D4), and the INTAKE comment and
    /// the `routing` event say `not_followed`. True without a verdict.
    pub harness_followed: bool,
}

/// The ticket's routing from intake's verdict, `None` for a ticket without
/// one, and the values of its `tier:` and `effort:` labels, which override
/// the verdict's. Without a verdict, the project default's tier and effort
/// under the labels.
pub fn ticket_routing(
    verdict: Option<Routing>,
    tier_label: Option<Tier>,
    effort_label: Option<Effort>,
) -> TicketRouting {
    TicketRouting {
        tier: tier_label
            .or(verdict.map(|v| v.tier))
            .unwrap_or(PROJECT_DEFAULT.tier),
        effort: effort_label
            .or(verdict.map(|v| v.effort))
            .or(PROJECT_DEFAULT.effort),
        harness_followed: verdict.is_none_or(|v| v.harness == Harness::Claude),
    }
}

/// The routing of a run of `role` on a ticket routed `ticket`: Design and
/// Build follow the ticket (the Verify fix run, the Build of a review loop
/// and Watch's fix runs are Build runs), on Claude Code; intake, the
/// resolver, the answer check and the arbiter run on the standard tier,
/// rebase on the fast one, all without an effort (the maintainer's
/// decision of 2026-10-15, OWL-301). A rebase's escalation to the Build's
/// routing on a non-trivial conflict is the runner's. `None` for Design
/// review and Verify, whose reviewers are routed per seat
/// ([`seat_routing`]).
pub fn role_routing(role: Role, ticket: TicketRouting) -> Option<RunRouting> {
    match role {
        Role::Design | Role::Build => Some(RunRouting {
            harness: Harness::Claude,
            tier: ticket.tier,
            effort: ticket.effort,
        }),
        Role::Intake | Role::Resolver | Role::AnswerCheck | Role::Arbiter => Some(PROJECT_DEFAULT),
        Role::Rebase => Some(RunRouting {
            tier: Tier::Fast,
            ..PROJECT_DEFAULT
        }),
        Role::DesignReview | Role::Verify => None,
    }
}

/// A reviewer's routing, and whether it runs on the author's model.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SeatRouting {
    pub routing: RunRouting,
    /// A reviewer on Claude Code whose configuration names no other model
    /// than the author's: a fresh run of the same model, which the report
    /// says ([`other_model`]).
    pub same_model: bool,
}

/// The routing of a reviewer of the roster on a ticket routed `ticket`,
/// whose authoring runs ran on its tier: the critic on Codex on the
/// standard tier; a reviewer on Claude Code, the critic standing in
/// included, on another model than the author's ([`other_model`], `model`
/// giving the Claude Code model of each tier). Every reviewer takes the
/// ticket's effort (the maintainer's decision of 2026-10-15, OWL-301).
pub fn seat_routing<'a>(
    seat: Seat,
    ticket: TicketRouting,
    model: impl Fn(Tier) -> Option<&'a str>,
) -> SeatRouting {
    let (tier, same_model) = match seat.harness {
        Harness::Codex => (Tier::Standard, false),
        Harness::Claude => {
            let other = other_model(ticket.tier, model);
            (other.tier, other.same_model)
        }
    };
    SeatRouting {
        routing: RunRouting {
            harness: seat.harness,
            tier,
            effort: ticket.effort,
        },
        same_model,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::roster::roster;
    use crate::vocab::{Stage, Variant};

    const fn verdict(harness: Harness, tier: Tier, effort: Effort) -> Option<Routing> {
        Some(Routing {
            harness,
            tier,
            effort,
        })
    }

    const fn ticket(tier: Tier, effort: Option<Effort>, harness_followed: bool) -> TicketRouting {
        TicketRouting {
            tier,
            effort,
            harness_followed,
        }
    }

    #[test]
    fn the_verdict_routes_the_ticket_under_its_labels() {
        use Effort::{High, Low};
        use Harness::{Claude, Codex};
        use Tier::{Deep, Fast, Standard};
        let cases = [
            // No verdict: the project default, under the labels.
            (None, None, None, ticket(Standard, None, true)),
            (None, Some(Fast), Some(Low), ticket(Fast, Some(Low), true)),
            // The verdict, each label overriding its part.
            (
                verdict(Claude, Deep, High),
                None,
                None,
                ticket(Deep, Some(High), true),
            ),
            (
                verdict(Claude, Deep, High),
                Some(Fast),
                None,
                ticket(Fast, Some(High), true),
            ),
            (
                verdict(Claude, Deep, High),
                None,
                Some(Low),
                ticket(Deep, Some(Low), true),
            ),
            // Codex named for the authoring runs is not followed (D4).
            (
                verdict(Codex, Deep, High),
                None,
                None,
                ticket(Deep, Some(High), false),
            ),
        ];
        for (verdict, tier, effort, expected) in cases {
            assert_eq!(
                ticket_routing(verdict, tier, effort),
                expected,
                "{verdict:?} {tier:?} {effort:?}"
            );
        }
    }

    #[test]
    fn each_role_takes_its_layer() {
        let routed = ticket(Tier::Deep, Some(Effort::Max), false);
        let follows = Some(RunRouting {
            harness: Harness::Claude,
            tier: Tier::Deep,
            effort: Some(Effort::Max),
        });
        let fast = Some(RunRouting {
            tier: Tier::Fast,
            ..PROJECT_DEFAULT
        });
        let cases = [
            (Role::Design, follows),
            (Role::Build, follows),
            (Role::Intake, Some(PROJECT_DEFAULT)),
            (Role::Resolver, Some(PROJECT_DEFAULT)),
            (Role::AnswerCheck, Some(PROJECT_DEFAULT)),
            (Role::Arbiter, Some(PROJECT_DEFAULT)),
            (Role::Rebase, fast),
            (Role::DesignReview, None),
            (Role::Verify, None),
        ];
        for (role, expected) in cases {
            assert_eq!(role_routing(role, routed), expected, "{role:?}");
        }
    }

    #[test]
    fn reviewers_take_their_tier_and_the_tickets_effort() {
        let routed = ticket(Tier::Standard, Some(Effort::Xhigh), true);
        let model = |tier| match tier {
            Tier::Deep => Some("opus"),
            Tier::Standard => Some("sonnet"),
            Tier::Fast => None,
        };
        let routings: Vec<_> = [true, false]
            .into_iter()
            .flat_map(|codex| roster(Variant::Standard, Stage::DesignReview, codex))
            .map(|seat| seat_routing(seat, routed, model))
            .collect();
        let seat = |harness, tier| SeatRouting {
            routing: RunRouting {
                harness,
                tier,
                effort: Some(Effort::Xhigh),
            },
            same_model: false,
        };
        assert_eq!(
            routings,
            [
                // With Codex: the critic on Codex, conformance on Claude Code.
                seat(Harness::Codex, Tier::Standard),
                seat(Harness::Claude, Tier::Deep),
                // Without: the critic's stand-in on another model too.
                seat(Harness::Claude, Tier::Deep),
                seat(Harness::Claude, Tier::Deep),
            ]
        );
        let alone = roster(Variant::Standard, Stage::Verify, false)[0];
        assert_eq!(
            seat_routing(alone, routed, |_| None),
            SeatRouting {
                routing: RunRouting {
                    harness: Harness::Claude,
                    tier: Tier::Standard,
                    effort: Some(Effort::Xhigh),
                },
                same_model: true,
            }
        );
    }
}
