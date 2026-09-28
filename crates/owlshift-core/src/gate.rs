//! Gates: where a human decision may be needed (architecture, section 4).
//!
//! An open gate is a ticket waiting for input (see
//! [`crate::state::Status::NeedsInput`]). This module holds a project's gate
//! policy: which questions go to a human rather than the resolver, and when a
//! human approves the plan.

use crate::floor::{is_floor_category, matches_token, normalize};
use crate::pipeline::Pipeline;
use crate::vocab::{PlanApproval, Variant};

/// Who settles a question a role raised.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Route {
    /// The ticket's decider.
    Human,
    /// A resolver run, which decides what is discoverable and logs it as a
    /// reversible decision, and passes the rest to the decider.
    Resolver,
}

/// A project's gate policy: the always-human categories it adds to the
/// floor, and its plan-approval mode. Both come from the project file
/// (`policy.always_human`, `pipeline.plan_approval`).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct GatePolicy {
    /// The project's additions, normalized; the floor's categories are always
    /// included on top.
    always_human: Vec<String>,
    plan_approval: PlanApproval,
}

impl GatePolicy {
    /// A policy adding `always_human` to the floor's categories. There is no
    /// way to remove a floor category.
    pub fn new<S: AsRef<str>>(
        always_human: impl IntoIterator<Item = S>,
        plan_approval: PlanApproval,
    ) -> Self {
        let always_human = always_human
            .into_iter()
            .map(|category| normalize(category.as_ref()))
            .filter(|category| !category.is_empty())
            .collect();
        Self {
            always_human,
            plan_approval,
        }
    }

    /// Whether a question of this category always goes to a human: a floor
    /// category (a blank one included), or one the project added.
    pub fn always_human(&self, category: &str) -> bool {
        if is_floor_category(category) {
            return true;
        }
        let category = normalize(category);
        self.always_human
            .iter()
            .any(|token| matches_token(&category, token))
    }

    /// Who settles a question of this category: always-human categories go to
    /// the decider, the rest to the resolver first.
    pub fn route(&self, category: &str) -> Route {
        if self.always_human(category) {
            Route::Human
        } else {
            Route::Resolver
        }
    }

    /// Whether a human approves the plan before Build. The risky variant
    /// always requires it; the trivial variant has no plan to approve; the
    /// standard variant follows the project's mode, where `on-fork` requires
    /// it only when the design has a real fork.
    pub fn plan_approval_required(&self, pipeline: Pipeline, design_has_fork: bool) -> bool {
        match pipeline.variant() {
            Variant::Risky => true,
            Variant::Trivial => false,
            Variant::Standard => match self.plan_approval {
                PlanApproval::Always => true,
                PlanApproval::OnFork => design_has_fork,
                PlanApproval::Never => false,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::floor::FloorCategory;

    const MODES: [PlanApproval; 3] = [
        PlanApproval::Always,
        PlanApproval::OnFork,
        PlanApproval::Never,
    ];

    #[test]
    fn floor_categories_go_to_a_human_whatever_the_project_adds() {
        let bare = GatePolicy::new(Vec::<String>::new(), PlanApproval::Never);
        for category in FloorCategory::ALL {
            assert_eq!(bare.route(category.token()), Route::Human, "{category:?}");
        }
        assert_eq!(bare.route(""), Route::Human);
        assert_eq!(bare.route("testing"), Route::Resolver);
    }

    #[test]
    fn project_categories_are_added_to_the_floor() {
        let policy = GatePolicy::new(["Billing", "auth", "  "], PlanApproval::Never);
        for category in ["billing", "billing_address", "AUTH", "security"] {
            assert_eq!(policy.route(category), Route::Human, "{category:?}");
        }
        assert_eq!(policy.route("naming"), Route::Resolver);
        assert_eq!(policy.route("authoring"), Route::Resolver);
    }

    #[test]
    fn plan_approval_follows_the_variant_then_the_project_mode() {
        for mode in MODES {
            let policy = GatePolicy::new(Vec::<String>::new(), mode);
            for fork in [false, true] {
                assert!(policy.plan_approval_required(Pipeline::new(Variant::Risky), fork));
                assert!(!policy.plan_approval_required(Pipeline::new(Variant::Trivial), fork));
            }
        }
        let standard = Pipeline::new(Variant::Standard);
        let cases = [
            (PlanApproval::Always, false, true),
            (PlanApproval::Always, true, true),
            (PlanApproval::OnFork, false, false),
            (PlanApproval::OnFork, true, true),
            (PlanApproval::Never, false, false),
            (PlanApproval::Never, true, false),
        ];
        for (mode, fork, required) in cases {
            let policy = GatePolicy::new(Vec::<String>::new(), mode);
            assert_eq!(
                policy.plan_approval_required(standard, fork),
                required,
                "{mode:?}, fork {fork}"
            );
        }
    }
}
