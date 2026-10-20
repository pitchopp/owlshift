//! Per-ticket routing: the harness, tier and effort of each run, resolved
//! in section 9's order (architecture, section 5, "Per-ticket routing"):
//! the project default, the role, the ticket's verdict with its labels
//! overriding it, then what the machine has.
//!
//! The first three layers are here; the machine's changes nothing in P5,
//! every run being on Claude Code, the reviewers of the roster
//! ([`crate::roster::roster`]) included (OWL-347), but the opt-in Codex
//! reviewer ([`codex_reviewer_routing`], OWL-348), which a Codex that
//! cannot serve drops. Reading the `tier:` and
//! `effort:` labels off the ticket is the runner's; the core takes their
//! values.

use crate::roster::other_model;
use crate::vocab::{DelegationLevel, Effort, Harness, Role, Routing, Tier};

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
/// review and Verify, whose reviewers are routed alike
/// ([`reviewer_routing`]).
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

/// The routing of a resolver run at the project's delegation `level` on a
/// ticket routed `ticket` (OWL-344): at `all`, where it may decide on its
/// own judgment (OWL-342), Claude Code on the deep tier at the ticket's
/// effort, whatever the ticket's tier; below `all`, [`role_routing`]'s, the
/// standard tier without an effort.
pub fn resolver_routing(level: DelegationLevel, ticket: TicketRouting) -> RunRouting {
    match level {
        DelegationLevel::All => RunRouting {
            harness: Harness::Claude,
            tier: Tier::Deep,
            effort: ticket.effort,
        },
        _ => role_routing(Role::Resolver, ticket).expect("the resolver has a routing of its own"),
    }
}

/// A reviewer's routing, and whether it runs on the author's model.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ReviewerRouting {
    pub routing: RunRouting,
    /// A reviewer whose configuration names no other model than the
    /// author's: a fresh run of the same model, which the report says
    /// ([`other_model`]).
    pub same_model: bool,
}

/// The routing of every reviewer of the roster on a ticket routed `ticket`,
/// whose authoring runs ran on its tier: on Claude Code, whatever its seat
/// (OWL-347), on another model than the author's ([`other_model`], `model`
/// giving the Claude Code model of each tier). Every reviewer takes the
/// ticket's effort (the maintainer's decision of 2026-10-15, OWL-301).
pub fn reviewer_routing<'a>(
    ticket: TicketRouting,
    model: impl Fn(Tier) -> Option<&'a str>,
) -> ReviewerRouting {
    let other = other_model(ticket.tier, model);
    ReviewerRouting {
        routing: RunRouting {
            harness: Harness::Claude,
            tier: other.tier,
            effort: ticket.effort,
        },
        same_model: other.same_model,
    }
}

/// The routing of the opt-in Codex reviewer (OWL-348,
/// [`crate::roster::CODEX_REVIEWER`]) on a ticket routed `ticket`: Codex on
/// the standard tier, as the Codex critic ran until OWL-347, at the
/// ticket's effort, as every reviewer (OWL-301). Another model family than
/// the author's, so never the author's model.
pub fn codex_reviewer_routing(ticket: TicketRouting) -> RunRouting {
    RunRouting {
        harness: Harness::Codex,
        tier: Tier::Standard,
        effort: ticket.effort,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// OWL-344: deep at the ticket's effort at `all`, whatever the ticket's
    /// tier; the project default at every level below.
    #[test]
    fn the_resolver_is_deep_at_the_ticket_effort_only_at_all() {
        for level in DelegationLevel::ALL {
            for tier in [Tier::Standard, Tier::Deep] {
                for effort in [Some(Effort::Max), None] {
                    let expected = if level == DelegationLevel::All {
                        RunRouting {
                            harness: Harness::Claude,
                            tier: Tier::Deep,
                            effort,
                        }
                    } else {
                        PROJECT_DEFAULT
                    };
                    assert_eq!(
                        resolver_routing(level, ticket(tier, effort, true)),
                        expected,
                        "{level:?} {tier:?} {effort:?}"
                    );
                }
            }
        }
    }

    /// OWL-347: every reviewer, the critic included, on Claude Code, on
    /// another model than the author's when one is named, at the ticket's
    /// effort; the author's own model, flagged, when none is.
    #[test]
    fn reviewers_run_on_claude_code_on_another_model_at_the_tickets_effort() {
        let routed = ticket(Tier::Standard, Some(Effort::Xhigh), true);
        let reviewer = |tier, same_model| ReviewerRouting {
            routing: RunRouting {
                harness: Harness::Claude,
                tier,
                effort: Some(Effort::Xhigh),
            },
            same_model,
        };
        let model = |tier| match tier {
            Tier::Deep => Some("opus"),
            Tier::Standard => Some("sonnet"),
            Tier::Fast => None,
        };
        assert_eq!(reviewer_routing(routed, model), reviewer(Tier::Deep, false));
        assert_eq!(
            reviewer_routing(routed, |_| None),
            reviewer(Tier::Standard, true)
        );
    }

    /// OWL-348: the Codex reviewer on Codex, on the standard tier whatever
    /// the ticket's, at the ticket's effort.
    #[test]
    fn the_codex_reviewer_runs_on_the_standard_tier_at_the_tickets_effort() {
        let routed = ticket(Tier::Deep, Some(Effort::High), true);
        assert_eq!(
            codex_reviewer_routing(routed),
            RunRouting {
                harness: Harness::Codex,
                tier: Tier::Standard,
                effort: Some(Effort::High),
            }
        );
    }
}
