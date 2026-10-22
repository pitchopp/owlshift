//! The shared vocabulary: the closed sets of names that the core decides on
//! and that the contracts carry on the wire.
//!
//! These types only derive their serialized form and their JSON Schema; the
//! core model built on them (ticket, pipeline, state machine) is separate.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// A stage of the pipeline, or `ready` for an admitted ticket not yet started.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    Intake,
    Ready,
    Design,
    DesignReview,
    Build,
    Verify,
    Deliver,
    Watch,
}

// The declaration order above is the pipeline's order, so `Ord` sorts stages
// the way a ticket goes through them. (A doc comment on `Stage` itself would
// change the committed JSON Schemas.)
impl Stage {
    /// Every stage, in pipeline order.
    pub const ALL: [Self; 8] = [
        Self::Intake,
        Self::Ready,
        Self::Design,
        Self::DesignReview,
        Self::Build,
        Self::Verify,
        Self::Deliver,
        Self::Watch,
    ];
}

/// A ticket's priority, as the tracker reports it. `Ord` sorts the most urgent
/// first; a ticket without a priority comes last.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    Urgent,
    High,
    Medium,
    Low,
    Unset,
}

/// What a run executes: a stage's role, or one of the runner's helper roles.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    Intake,
    Resolver,
    AnswerCheck,
    Design,
    DesignReview,
    Build,
    Verify,
    Rebase,
    Arbiter,
}

impl Role {
    /// The name this role serializes as, and the stem of its prompt file
    /// under `roles/` (`Role::Build` names `roles/build.md`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Intake => "intake",
            Self::Resolver => "resolver",
            Self::AnswerCheck => "answer_check",
            Self::Design => "design",
            Self::DesignReview => "design_review",
            Self::Build => "build",
            Self::Verify => "verify",
            Self::Rebase => "rebase",
            Self::Arbiter => "arbiter",
        }
    }

    /// Whether a run of this role is given the web tools (WebFetch,
    /// WebSearch) unless the operator turns them off (OWL-415,
    /// architecture section 8): the read-only planning and review roles and
    /// the resolver, which check external facts. Intake reads text anyone
    /// the tracker lets write a ticket wrote, and the answer check keeps
    /// its permissions; Build and rebase keep their briefs' as they were.
    pub fn takes_web_tools(self) -> bool {
        match self {
            Self::Design | Self::DesignReview | Self::Verify | Self::Arbiter | Self::Resolver => {
                true
            }
            Self::Intake | Self::AnswerCheck | Self::Build | Self::Rebase => false,
        }
    }
}

/// A model tier; the configuration maps each tier to a model per harness.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    Deep,
    Standard,
    Fast,
}

/// A harness: a vendor CLI that runs one role headless.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Harness {
    Claude,
    Codex,
}

/// A pipeline variant.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Variant {
    Trivial,
    Standard,
    Risky,
}

/// When a human approves the plan before Build.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "kebab-case")]
pub enum PlanApproval {
    Always,
    OnFork,
    Never,
}

/// The routing verdict: the harness, tier and effort of the ticket's runs
/// (section 9, "One routing verdict per ticket").
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Routing {
    pub harness: Harness,
    pub tier: Tier,
    pub effort: Effort,
}

/// How hard a model thinks, as `claude --effort` names its levels (checked
/// on 2026-10-13 with Claude Code 2.1.296: `claude --help` lists `low`,
/// `medium`, `high`, `xhigh` and `max`). A harness with other levels maps
/// from these when routing follows the verdict (P5).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Effort {
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl Effort {
    /// The value `claude --effort` takes.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }
}

/// How much a finding weighs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Blocker,
    Major,
    Minor,
}

/// A review mandate (architecture section 5, "The roster").
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Mandate {
    /// What breaks the work: a premise that does not hold, a case left out,
    /// a simpler way.
    Critic,
    /// The plan or the diff against the ticket's criteria, the decider's
    /// answers and the project's rules.
    Conformance,
    /// What the change does to the code around it.
    Debt,
}

/// What a project lets the resolver decide without its decider
/// (`policy.resolver_delegation`, architecture section 4, "Delegation
/// level", OWL-328). `Ord` sorts the levels from nothing to everything but
/// the floor.
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
    JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum DelegationLevel {
    /// Nothing: no resolver run, every question goes to the decider.
    None,
    /// What the resolver checks in the repository and its history.
    Facts,
    /// Facts, and what the ticket, the decider's comments, earlier
    /// decisions and the project's rules settle.
    #[default]
    Discoverable,
    /// The same, and a `scope` question the ticket's own text answers,
    /// quoted.
    Ticket,
    /// Everything but the floor, on its own judgment where nothing settles
    /// a question (OWL-342).
    All,
}

impl DelegationLevel {
    /// Every level, from nothing to everything but the floor.
    pub const ALL: [Self; 5] = [
        Self::None,
        Self::Facts,
        Self::Discoverable,
        Self::Ticket,
        Self::All,
    ];

    /// The name the project file and the brief write.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Facts => "facts",
            Self::Discoverable => "discoverable",
            Self::Ticket => "ticket",
            Self::All => "all",
        }
    }

    /// Whether the resolver may rest a decision on a basis of this kind at
    /// this level: `facts` allows `fact`, `discoverable` adds `settled`,
    /// `ticket` adds `ticket`, and `all` adds `judgment` (OWL-342) and the
    /// runner's own `recommendation` (OWL-375); `none` allows nothing.
    pub fn allows(self, kind: BasisKind) -> bool {
        match kind {
            BasisKind::Fact => self >= Self::Facts,
            BasisKind::Settled => self >= Self::Discoverable,
            BasisKind::Ticket => self >= Self::Ticket,
            BasisKind::Judgment | BasisKind::Recommendation => self == Self::All,
        }
    }
}

/// What a resolver's decision rests on (OWL-328): the runner allows a kind
/// by the project's delegation level ([`DelegationLevel::allows`]).
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum BasisKind {
    /// Checked in the repository and its history: a file, an existing
    /// pattern, a documented behaviour.
    Fact,
    /// Settled by the ticket, the decider's comments, earlier decisions or
    /// the project's rules.
    Settled,
    /// The ticket's own text, which the decision quotes.
    Ticket,
    /// The resolver's own weighing where nothing settles the question: a
    /// trade-off, a priority, an intent the ticket does not state
    /// (OWL-342). At `all` only.
    Judgment,
    /// The asking run's own recommendation, which the runner takes once the
    /// resolver's chain reached its bound and no resolver runs (OWL-375): the
    /// runner's kind, never a resolver's. At `all` only.
    Recommendation,
}

impl BasisKind {
    /// Every kind, in the order of the lowest level that allows each.
    pub const ALL: [Self; 5] = [
        Self::Fact,
        Self::Settled,
        Self::Ticket,
        Self::Judgment,
        Self::Recommendation,
    ];

    /// The name `result.json` and the ticket ref write.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Fact => "fact",
            Self::Settled => "settled",
            Self::Ticket => "ticket",
            Self::Judgment => "judgment",
            Self::Recommendation => "recommendation",
        }
    }
}

/// How the decider's answers left one question.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AnswerClass {
    /// The answers settle the question.
    Answered,
    /// The answers settle part of the question; the rest is missing.
    Partial,
    /// No answer from the decider addresses the question.
    Unanswered,
    /// The decider answered with a question of their own.
    CounterQuestion,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stage_all_lists_every_stage_in_pipeline_order() {
        // The match has no wildcard: a new stage fails to compile here until
        // it is given its position, and the assertion then requires it in ALL.
        fn position(stage: Stage) -> usize {
            match stage {
                Stage::Intake => 0,
                Stage::Ready => 1,
                Stage::Design => 2,
                Stage::DesignReview => 3,
                Stage::Build => 4,
                Stage::Verify => 5,
                Stage::Deliver => 6,
                Stage::Watch => 7,
            }
        }
        for (index, stage) in Stage::ALL.into_iter().enumerate() {
            assert_eq!(position(stage), index, "{stage:?}");
        }
        assert!(Stage::ALL.is_sorted_by(|a, b| a < b));
    }

    #[test]
    fn role_as_str_matches_the_serde_name() {
        for role in [
            Role::Intake,
            Role::Resolver,
            Role::AnswerCheck,
            Role::Design,
            Role::DesignReview,
            Role::Build,
            Role::Verify,
            Role::Rebase,
            Role::Arbiter,
        ] {
            let serialized = serde_json::to_value(role).expect("Role always serializes");
            assert_eq!(
                serialized,
                serde_json::Value::String(role.as_str().to_owned()),
                "{role:?}"
            );
        }
    }

    /// OWL-415: the planning and review roles and the resolver take the web
    /// tools; intake, the answer check, Build and rebase do not.
    #[test]
    fn only_the_planning_review_and_resolver_roles_take_web_tools() {
        let takes: Vec<Role> = [
            Role::Intake,
            Role::Resolver,
            Role::AnswerCheck,
            Role::Design,
            Role::DesignReview,
            Role::Build,
            Role::Verify,
            Role::Rebase,
            Role::Arbiter,
        ]
        .into_iter()
        .filter(|role| role.takes_web_tools())
        .collect();
        assert_eq!(
            takes,
            [
                Role::Resolver,
                Role::Design,
                Role::DesignReview,
                Role::Verify,
                Role::Arbiter
            ]
        );
    }

    #[test]
    fn delegation_names_match_the_serde_names() {
        for level in DelegationLevel::ALL {
            let serialized = serde_json::to_value(level).expect("a level always serializes");
            assert_eq!(serialized, level.as_str(), "{level:?}");
        }
        assert!(DelegationLevel::ALL.is_sorted());
        assert_eq!(DelegationLevel::default(), DelegationLevel::Discoverable);
        for kind in BasisKind::ALL {
            let serialized = serde_json::to_value(kind).expect("a kind always serializes");
            assert_eq!(serialized, kind.as_str(), "{kind:?}");
        }
    }

    /// The kinds each level allows, as build-plan.md's "Delegation and
    /// passes" lists them.
    #[test]
    fn each_level_allows_its_basis_kinds() {
        use BasisKind::{Fact, Judgment, Recommendation, Settled, Ticket};
        let cases: [(DelegationLevel, &[BasisKind]); 5] = [
            (DelegationLevel::None, &[]),
            (DelegationLevel::Facts, &[Fact]),
            (DelegationLevel::Discoverable, &[Fact, Settled]),
            (DelegationLevel::Ticket, &[Fact, Settled, Ticket]),
            (
                DelegationLevel::All,
                &[Fact, Settled, Ticket, Judgment, Recommendation],
            ),
        ];
        for (level, allowed) in cases {
            for kind in BasisKind::ALL {
                assert_eq!(
                    level.allows(kind),
                    allowed.contains(&kind),
                    "{level:?} {kind:?}"
                );
            }
        }
    }

    #[test]
    fn priority_sorts_the_most_urgent_first_and_unset_last() {
        let mut priorities = [
            Priority::Unset,
            Priority::Low,
            Priority::Urgent,
            Priority::Medium,
            Priority::High,
        ];
        priorities.sort();
        assert_eq!(
            priorities,
            [
                Priority::Urgent,
                Priority::High,
                Priority::Medium,
                Priority::Low,
                Priority::Unset,
            ]
        );
    }
}
