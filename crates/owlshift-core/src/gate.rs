//! Gates: where a human decision may be needed (architecture, section 4).
//!
//! An open gate is a ticket waiting for input (see
//! [`crate::state::Status::NeedsInput`]). This module holds a project's gate
//! policy: which questions go to a human rather than the resolver, and when a
//! human approves the plan.

use crate::floor::{is_floor_category, is_token, matches_token, normalize};
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

/// The categories of a project's `policy.always_human` as the gate reads
/// them: normalized, in the project's order, each once, blank ones dropped.
/// The floor's categories are not filtered out here, so a caller that shows
/// the list as written can still tell which ones the floor covers
/// ([`is_floor_category`]); [`GatePolicy::additions`] leaves them out.
pub fn normalized_categories<S: AsRef<str>>(
    always_human: impl IntoIterator<Item = S>,
) -> Vec<String> {
    let mut kept: Vec<String> = Vec::new();
    for category in always_human {
        let category = normalize(category.as_ref());
        if !category.is_empty() && !kept.contains(&category) {
            kept.push(category);
        }
    }
    kept
}

/// A project's gate policy: the always-human categories it adds to the
/// floor, and its plan-approval mode. Both come from the project file
/// (`policy.always_human`, `pipeline.plan_approval`).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct GatePolicy {
    /// The project's categories, normalized, each once, blank ones dropped (a
    /// floor category may appear: [`GatePolicy::additions`] leaves it out); the
    /// floor's categories are always included on top.
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
        Self {
            always_human: normalized_categories(always_human),
            plan_approval,
        }
    }

    /// The categories the project adds to the floor, as the gate reads them:
    /// normalized, in the project's order, each once, with the blank ones and
    /// those the floor already covers left out. What the runner tells a run,
    /// so it can file a question under the word the gate matches.
    pub fn additions(&self) -> Vec<String> {
        self.always_human
            .iter()
            .filter(|category| !is_floor_category(category))
            .cloned()
            .collect()
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

    /// Who settles a question the resolver decided, from the category the
    /// asking run gave it (`raised`) and the resolver's own label for it
    /// (`labelled`, written without seeing `raised`): the resolver only when
    /// both route to it and the label is a token ([`is_token`]). An
    /// always-human category on either side, or a label the matcher could
    /// misread, sends the question to the decider: the runner draws that
    /// consequence, whatever the resolver decided.
    pub fn route_decided(&self, raised: &str, labelled: &str) -> Route {
        if !is_token(labelled) || self.always_human(raised) || self.always_human(labelled) {
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

    /// The plan-approval gate of the ticket's current plan (OWL-32): whether
    /// it must be approved ([`GatePolicy::plan_approval_required`]), and
    /// whether the approval kept in the ticket ref is of this very plan,
    /// `approved_sha256` being the hash it keeps and `plan_sha256` the
    /// current plan's, both the SHA-256 of the plan's canonical text, which
    /// the runner computes. An empty hash is never approved.
    pub fn plan_gate(
        &self,
        pipeline: Pipeline,
        design_has_fork: bool,
        plan_sha256: &str,
        approved_sha256: Option<&str>,
    ) -> PlanGate {
        PlanGate {
            required: self.plan_approval_required(pipeline, design_has_fork),
            approved: !plan_sha256.is_empty() && approved_sha256 == Some(plan_sha256),
        }
    }
}

/// The plan-approval gate between the design loop and Build (architecture,
/// section 5, "Plan approval"): when approval is required, a Build is never
/// entered on a plan whose hash is not the approved one. Only
/// [`GatePolicy::plan_gate`] builds one, from the policy and the plan's
/// facts, and the state machine takes it to enter Build from Design or
/// Design review ([`crate::state::TicketState::apply_gated`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PlanGate {
    required: bool,
    approved: bool,
}

impl PlanGate {
    /// Whether the plan must be approved before Build.
    pub const fn required(self) -> bool {
        self.required
    }

    /// Whether the kept approval is of the current plan.
    pub const fn approved(self) -> bool {
        self.approved
    }

    /// Whether a Build may start on the current plan: no approval is
    /// required, or the plan is the approved one.
    pub const fn is_open(self) -> bool {
        !self.required || self.approved
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
    fn normalized_categories_keep_floor_ones_but_not_blank_or_repeated() {
        let kept = normalized_categories(["Billing", " ", "billing", "security", "auth"]);
        assert_eq!(kept, ["billing", "security", "auth"]);
    }

    #[test]
    fn additions_are_what_the_project_added_beyond_the_floor() {
        let policy = GatePolicy::new(
            [
                "Billing!",
                "Data Loss",
                "",
                "billing",
                "tone",
                "legal_wording",
            ],
            PlanApproval::Never,
        );
        assert_eq!(policy.additions(), ["billing", "tone"]);
        let bare = GatePolicy::new(Vec::<String>::new(), PlanApproval::Never);
        assert!(bare.additions().is_empty());
    }

    /// A decision stands only when Build's category and the resolver's own
    /// label both route to the resolver, the label written as a token: an
    /// always-human or malformed label on either side sends the question to
    /// the decider.
    #[test]
    fn a_decision_needs_both_labels_to_route_to_the_resolver() {
        let policy = GatePolicy::new(["billing"], PlanApproval::Never);
        let cases = [
            // Labels that merely differ say nothing about the floor.
            ("naming", "naming", Route::Resolver),
            ("naming", "file_layout", Route::Resolver),
            ("Naming", "tone2", Route::Resolver),
            // Build's own always-human category.
            ("security", "naming", Route::Human),
            ("", "naming", Route::Human),
            // The resolver's label names the floor or a project addition,
            // as a whole-word token.
            ("cleanup", "data_loss", Route::Human),
            ("cleanup", "risk_of_data_loss", Route::Human),
            ("cleanup", "scope_change", Route::Human),
            ("cleanup", "billing_address", Route::Human),
            // A label the matcher could misread is refused, never read.
            ("cleanup", "Data-Loss", Route::Human),
            ("cleanup", "dataLoss", Route::Human),
            ("cleanup", "sécurité", Route::Human),
            ("cleanup", "data loss", Route::Human),
            ("cleanup", "data__loss", Route::Human),
            ("cleanup", "_naming", Route::Human),
            ("cleanup", "", Route::Human),
            ("cleanup", " ", Route::Human),
        ];
        for (raised, labelled, route) in cases {
            assert_eq!(
                policy.route_decided(raised, labelled),
                route,
                "{raised:?} labelled {labelled:?}"
            );
        }
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

    /// The gate opens on a plan that needs no approval, or on the very plan
    /// approved: another plan's approval, none, or an empty hash keep it
    /// shut.
    #[test]
    fn the_plan_gate_opens_only_on_the_approved_hash_when_approval_is_required() {
        let plan = "a".repeat(64);
        let other = "b".repeat(64);
        let risky = Pipeline::new(Variant::Risky);
        for mode in MODES {
            let policy = GatePolicy::new(Vec::<String>::new(), mode);
            let cases = [
                (plan.as_str(), Some(plan.as_str()), true),
                (plan.as_str(), Some(other.as_str()), false),
                (plan.as_str(), None, false),
                ("", Some(""), false),
            ];
            for (current, kept, approved) in cases {
                let gate = policy.plan_gate(risky, false, current, kept);
                assert!(gate.required(), "{mode:?}");
                assert_eq!(gate.approved(), approved, "{mode:?} {kept:?}");
                assert_eq!(gate.is_open(), approved, "{mode:?} {kept:?}");
            }
            let trivial = policy.plan_gate(Pipeline::new(Variant::Trivial), true, &plan, None);
            assert!(!trivial.required() && trivial.is_open(), "{mode:?}");
        }
        let on_fork = GatePolicy::new(Vec::<String>::new(), PlanApproval::OnFork);
        let standard = Pipeline::new(Variant::Standard);
        assert!(on_fork.plan_gate(standard, false, &plan, None).is_open());
        assert!(!on_fork.plan_gate(standard, true, &plan, None).is_open());
        assert!(
            on_fork
                .plan_gate(standard, true, &plan, Some(&plan))
                .is_open()
        );
    }
}
