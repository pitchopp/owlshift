//! Gates: where a human decision may be needed (architecture, section 4).
//!
//! An open gate is a ticket waiting for input (see
//! [`crate::state::Status::NeedsInput`]). This module holds a project's gate
//! policy: which questions go to a human rather than the resolver, and when a
//! human approves the plan.

use crate::floor::{
    FloorCategory, is_human_category, is_token, matches_token, names_floor_category, normalize,
};
use crate::pipeline::Pipeline;
use crate::state::MAX_RESOLVED_PASSES;
use crate::vocab::{BasisKind, DelegationLevel, PlanApproval, Variant};

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
/// The default always-human categories are not filtered out here, so a
/// caller that shows the list as written can still tell which ones the level
/// covers ([`is_human_category`]); [`GatePolicy::additions`] leaves them
/// out.
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

/// How many runs in a row the resolver may unblock in one command
/// (`policy.resolver_passes`, OWL-328).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PassLimit {
    /// At most this many runs in a row whose questions it all decided.
    Runs(u32),
    /// No count: the chain's dollar ceiling bounds it instead.
    Unlimited,
}

impl Default for PassLimit {
    /// [`MAX_RESOLVED_PASSES`] runs.
    fn default() -> Self {
        Self::Runs(MAX_RESOLVED_PASSES)
    }
}

/// What a project lets the resolver do (architecture section 4,
/// "Delegation level" and "Resolver passes", OWL-328): the level, the pass
/// limit and the dollar ceiling on one chain of runs. The default is the
/// behaviour before OWL-328, with the default pass limit raised to
/// [`MAX_RESOLVED_PASSES`]: `discoverable`, no ceiling.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct ResolverPolicy {
    pub level: DelegationLevel,
    pub passes: PassLimit,
    /// The ceiling on one chain's cost, in millionths of a dollar
    /// ([`usd_micros`]); `None` without one.
    pub ceiling_micros: Option<u64>,
}

/// A dollar amount in millionths of a dollar, rounded: `None` for an
/// amount that is negative or not a finite number, which no ceiling can be
/// checked against.
pub fn usd_micros(usd: f64) -> Option<u64> {
    if !usd.is_finite() || usd < 0.0 {
        return None;
    }
    // `as` saturates a float past the integer's range.
    Some((usd * 1_000_000.0).round() as u64)
}

/// What the resolver runs of one chain cost so far, since the command's
/// last stop (architecture section 4, "Resolver passes"); the asking runs
/// do not count (OWL-376).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ChainCost {
    /// The sum of every resolver run's reported cost, in millionths of a
    /// dollar.
    Known(u64),
    /// A resolver run of the chain reported no cost: the ceiling cannot be
    /// checked.
    Unreported,
}

/// One chain of runs as the runner counts it, for [`GatePolicy::chain_end`]:
/// the runs in a row whose questions the resolver all decided, what the
/// chain's resolver runs cost, which the ceiling bounds, and what its asking
/// runs cost, which is only measured (OWL-376). The core does no I/O: the
/// runner reports each run's cost.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Chain {
    settled: u32,
    cost: ChainCost,
    asking: ChainCost,
}

impl Default for Chain {
    fn default() -> Self {
        Self {
            settled: 0,
            cost: ChainCost::Known(0),
            asking: ChainCost::Known(0),
        }
    }
}

/// `sum` with one more reported cost in dollars: unreported for good once a
/// cost is `None`, negative or not a finite number.
fn add_cost(sum: ChainCost, cost_usd: Option<f64>) -> ChainCost {
    match (sum, cost_usd.and_then(usd_micros)) {
        (ChainCost::Known(sum), Some(cost)) => ChainCost::Known(sum.saturating_add(cost)),
        _ => ChainCost::Unreported,
    }
}

impl Chain {
    /// The runs in a row whose questions the resolver all decided.
    pub const fn settled(self) -> u32 {
        self.settled
    }

    /// What the chain's resolver runs cost so far.
    pub const fn cost(self) -> ChainCost {
        self.cost
    }

    /// What the chain's asking runs whose questions reached the resolver
    /// cost so far: not every run of the chain, since a run that asked
    /// nothing is not counted. Measured, never bounded (OWL-376).
    pub const fn asking_cost(self) -> ChainCost {
        self.asking
    }

    /// One more resolver run of the chain, with its reported cost in
    /// dollars: `None`, or a cost that is negative or not a finite number,
    /// leaves the chain's cost unreported for good.
    pub fn resolved(&mut self, cost_usd: Option<f64>) {
        self.cost = add_cost(self.cost, cost_usd);
    }

    /// One more asking run whose questions reach the resolver, with its
    /// reported cost in dollars, which [`Chain::asking_cost`] sums: the
    /// ceiling does not read it, since it bounds the resolver's delegation,
    /// not the work it unblocks (OWL-376).
    pub fn asked(&mut self, cost_usd: Option<f64>) {
        self.asking = add_cost(self.asking, cost_usd);
    }

    /// The resolver decided every question of the chain's latest asking run.
    pub fn settled_once(&mut self) {
        self.settled = self.settled.saturating_add(1);
    }
}

/// Why a chain ends, the next run's questions going to the decider without
/// the resolver ([`GatePolicy::chain_end`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ChainEnd {
    /// The pass limit, this many runs in a row, is reached.
    Passes(u32),
    /// The chain's resolver runs cost this much, in millionths of a dollar,
    /// at or above the ceiling, which is also given.
    Budget {
        spent_micros: u64,
        ceiling_micros: u64,
    },
    /// A resolver run of the chain reported no cost, so its ceiling could
    /// not be checked.
    CostUnreported,
}

/// A project's gate policy: the always-human categories it adds, its
/// plan-approval mode, and what it lets the resolver do. All come from the
/// project file (`policy.always_human`, `pipeline.plan_approval`,
/// `policy.resolver_*`).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct GatePolicy {
    /// The project's categories, normalized, each once, blank ones dropped (a
    /// default always-human category may appear: [`GatePolicy::additions`]
    /// leaves out what the level covers); the level's are always included
    /// on top.
    always_human: Vec<String>,
    plan_approval: PlanApproval,
    resolver: ResolverPolicy,
}

impl GatePolicy {
    /// A policy adding `always_human` to the level's categories, with the
    /// default resolver settings ([`ResolverPolicy::default`]). There is no
    /// way to remove a floor category.
    pub fn new<S: AsRef<str>>(
        always_human: impl IntoIterator<Item = S>,
        plan_approval: PlanApproval,
    ) -> Self {
        Self {
            always_human: normalized_categories(always_human),
            plan_approval,
            resolver: ResolverPolicy::default(),
        }
    }

    /// The same policy with the project's resolver settings.
    pub fn with_resolver(self, resolver: ResolverPolicy) -> Self {
        Self { resolver, ..self }
    }

    /// The project's resolver settings.
    pub fn resolver(&self) -> ResolverPolicy {
        self.resolver
    }

    /// The project's delegation level.
    pub fn level(&self) -> DelegationLevel {
        self.resolver.level
    }

    /// The categories the project adds to its level's always-human set, as
    /// the gate reads them: normalized, in the project's order, each once,
    /// with the blank ones and those the level's set already covers left
    /// out. What the runner tells a run, so it can file a question under the
    /// word the gate matches: at `all`, a project's `money` is told, since
    /// the level hands `money` to the resolver and the project keeps it.
    pub fn additions(&self) -> Vec<String> {
        self.always_human
            .iter()
            .filter(|category| !is_human_category(category, self.level()))
            .cloned()
            .collect()
    }

    /// Whether a question of this category always goes to a human at the
    /// project's level: one of the level's categories (the floor's at every
    /// level), or one the project added. A blank category is neither: it
    /// reaches the resolver (OWL-342).
    pub fn always_human(&self, category: &str) -> bool {
        if is_human_category(category, self.level()) {
            return true;
        }
        let category = normalize(category);
        self.always_human
            .iter()
            .any(|token| matches_token(&category, token))
    }

    /// Who settles a question of this category, as the asking run filed it:
    /// always-human categories go to the decider, the rest, a blank one
    /// included, to the resolver first (OWL-342). At `none` every question
    /// goes to the decider, and no resolver run starts.
    pub fn route(&self, category: &str) -> Route {
        if self.level() == DelegationLevel::None || self.always_human(category) {
            Route::Human
        } else {
            Route::Resolver
        }
    }

    /// Who settles a question the resolver decided, from the category the
    /// asking run gave it (`raised`), the resolver's own label for it
    /// (`labelled`, written without seeing `raised`) and the kind of the
    /// decision's basis: the resolver only when both route to it, the label
    /// is a token ([`is_token`]) and the level allows the kind
    /// ([`DelegationLevel::allows`]); at `ticket`, a question either label
    /// names `scope` only on a `ticket` basis (OWL-328). Anything else sends
    /// the question to the decider: the runner draws that consequence,
    /// whatever the resolver decided. A `ticket` basis's quote is the
    /// runner's to find in the ticket's text.
    pub fn route_decided(&self, raised: &str, labelled: &str, kind: BasisKind) -> Route {
        let level = self.level();
        let scope = |category: &str| names_floor_category(category, FloorCategory::Scope);
        if self.route(raised) == Route::Human
            || !is_token(labelled)
            || self.always_human(labelled)
            || !level.allows(kind)
            || (level == DelegationLevel::Ticket
                && kind != BasisKind::Ticket
                && (scope(raised) || scope(labelled)))
        {
            Route::Human
        } else {
            Route::Resolver
        }
    }

    /// Whether the chain ends before its next resolver run, and why: the
    /// pass limit reached; then, with a ceiling, a resolver run's cost
    /// unreported or their sum reaching it. Whichever comes first ends the chain; without a
    /// ceiling the cost is not read. The runner asks before each resolver
    /// run.
    pub fn chain_end(&self, chain: Chain) -> Option<ChainEnd> {
        if let PassLimit::Runs(limit) = self.resolver.passes
            && chain.settled >= limit
        {
            return Some(ChainEnd::Passes(limit));
        }
        let ceiling = self.resolver.ceiling_micros?;
        match chain.cost {
            ChainCost::Unreported => Some(ChainEnd::CostUnreported),
            ChainCost::Known(spent) if spent >= ceiling => Some(ChainEnd::Budget {
                spent_micros: spent,
                ceiling_micros: ceiling,
            }),
            ChainCost::Known(_) => None,
        }
    }

    /// Whether the plan-approval question is the decider's alone, without a
    /// resolver run (OWL-342): under `pipeline.plan_approval = "always"`,
    /// which the risky variant forces, the setting names a human. Under
    /// `on-fork` the question reaches the resolver first, as any other.
    pub fn approval_is_human(&self, pipeline: Pipeline) -> bool {
        pipeline.variant() == Variant::Risky || self.plan_approval == PlanApproval::Always
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
        // A blank category reaches the resolver (OWL-342).
        assert_eq!(bare.route(""), Route::Resolver);
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
            // A blank category is routed by the resolver's label (OWL-342).
            ("", "naming", Route::Resolver),
            ("", "security", Route::Human),
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
                policy.route_decided(raised, labelled, BasisKind::Settled),
                route,
                "{raised:?} labelled {labelled:?}"
            );
        }
    }

    /// A policy at `level`, the project adding `billing`.
    fn at(level: DelegationLevel) -> GatePolicy {
        GatePolicy::new(["billing"], PlanApproval::Never).with_resolver(ResolverPolicy {
            level,
            ..ResolverPolicy::default()
        })
    }

    /// Each category at each level, as architecture section 4's table sets
    /// it: the floor and the project's additions go to the decider at every
    /// level, `all` included; `scope` reaches the resolver from `ticket`,
    /// `data_loss`, `money` and `legal` at `all`, a blank category at every
    /// level but `none` (OWL-342); `none` sends everything to the decider.
    #[test]
    fn each_level_routes_its_categories() {
        use DelegationLevel as L;
        use Route::{Human as H, Resolver as R};
        let categories = [
            "security",
            "irreversible",
            "",
            "billing",
            "billing_address",
            "scope",
            "scope_change",
            "data_loss",
            "money",
            "legal_wording",
            "naming",
        ];
        let table: [(L, [Route; 11]); 5] = [
            (L::None, [H, H, H, H, H, H, H, H, H, H, H]),
            (L::Facts, [H, H, R, H, H, H, H, H, H, H, R]),
            (L::Discoverable, [H, H, R, H, H, H, H, H, H, H, R]),
            (L::Ticket, [H, H, R, H, H, R, R, H, H, H, R]),
            (L::All, [H, H, R, H, H, R, R, R, R, R, R]),
        ];
        for (level, routes) in table {
            let policy = at(level);
            for (category, route) in categories.into_iter().zip(routes) {
                assert_eq!(policy.route(category), route, "{category:?} at {level:?}");
            }
        }
    }

    /// The project's additions stay human at every level, so a run is told
    /// those its level does not already keep: at `all`, a project's `money`
    /// is told.
    #[test]
    fn additions_are_told_when_the_level_does_not_keep_them() {
        let added = ["money", "scope_change", "security", "brand_voice"];
        for level in DelegationLevel::ALL {
            let policy =
                GatePolicy::new(added, PlanApproval::Never).with_resolver(ResolverPolicy {
                    level,
                    ..ResolverPolicy::default()
                });
            let expected: &[&str] = match level {
                DelegationLevel::All => &["money", "scope_change", "brand_voice"],
                DelegationLevel::Ticket => &["scope_change", "brand_voice"],
                _ => &["brand_voice"],
            };
            assert_eq!(policy.additions(), expected, "{level:?}");
            for category in added {
                assert_eq!(
                    policy.route(category),
                    Route::Human,
                    "{category} at {level:?}"
                );
            }
        }
    }

    /// A decision stands only on a basis the level allows, a `judgment` at
    /// `all` alone (OWL-342); at `ticket`, a question either label names
    /// `scope` only on a `ticket` basis; at `none` nothing stands.
    #[test]
    fn a_decision_needs_a_basis_the_level_allows() {
        use BasisKind::{Fact, Judgment, Settled, Ticket};
        use DelegationLevel as L;
        let cases = [
            (L::None, "naming", "naming", Fact, Route::Human),
            (L::Facts, "naming", "naming", Fact, Route::Resolver),
            (L::Facts, "naming", "naming", Settled, Route::Human),
            (L::Facts, "naming", "naming", Ticket, Route::Human),
            (
                L::Discoverable,
                "naming",
                "naming",
                Settled,
                Route::Resolver,
            ),
            (L::Discoverable, "naming", "naming", Ticket, Route::Human),
            (L::Discoverable, "naming", "scope", Ticket, Route::Human),
            (L::Ticket, "naming", "naming", Ticket, Route::Resolver),
            (L::Ticket, "naming", "naming", Settled, Route::Resolver),
            (L::Ticket, "scope", "scope", Ticket, Route::Resolver),
            (L::Ticket, "scope", "naming", Ticket, Route::Resolver),
            (L::Ticket, "scope", "naming", Settled, Route::Human),
            (L::Ticket, "naming", "scope_change", Fact, Route::Human),
            (L::Ticket, "naming", "money", Ticket, Route::Human),
            (L::All, "scope", "scope", Settled, Route::Resolver),
            (L::All, "money", "legal", Fact, Route::Resolver),
            (L::All, "naming", "security", Ticket, Route::Human),
            (L::All, "irreversible", "naming", Ticket, Route::Human),
            (L::All, "naming", "billing", Ticket, Route::Human),
            (L::All, "naming", "Scope", Ticket, Route::Human),
            (L::All, "naming", "naming", Judgment, Route::Resolver),
            (L::All, "scope", "trade_off", Judgment, Route::Resolver),
            (L::All, "naming", "security", Judgment, Route::Human),
            (L::Ticket, "naming", "naming", Judgment, Route::Human),
            (L::Discoverable, "naming", "naming", Judgment, Route::Human),
            (L::Facts, "naming", "naming", Judgment, Route::Human),
        ];
        for (level, raised, labelled, kind, route) in cases {
            assert_eq!(
                at(level).route_decided(raised, labelled, kind),
                route,
                "{raised:?} labelled {labelled:?} on {kind:?} at {level:?}"
            );
        }
    }

    /// A chain of `settled` passes whose resolver runs cost `costs` (in
    /// dollars, `None` unreported) under `passes` and `ceiling`.
    fn ends(
        passes: PassLimit,
        ceiling: Option<f64>,
        settled: u32,
        costs: &[Option<f64>],
    ) -> Option<ChainEnd> {
        let policy = GatePolicy::new(Vec::<String>::new(), PlanApproval::Never).with_resolver(
            ResolverPolicy {
                passes,
                ceiling_micros: ceiling.and_then(usd_micros),
                ..ResolverPolicy::default()
            },
        );
        let mut chain = Chain::default();
        for _ in 0..settled {
            chain.settled_once();
        }
        for cost in costs {
            chain.resolved(*cost);
        }
        policy.chain_end(chain)
    }

    /// The pass limit at its default, a set value and unlimited with its
    /// ceiling on the resolver runs' cost, which the end gives with the
    /// spend; a ceiling beside a number of passes, whichever comes first; a
    /// resolver run without a cost ends a chain only when a ceiling is set.
    #[test]
    fn a_chain_ends_at_its_pass_limit_or_its_ceiling() {
        use PassLimit::{Runs, Unlimited};
        let default = PassLimit::default();
        assert_eq!(default, Runs(5));
        assert_eq!(ends(default, None, 4, &[]), None);
        assert_eq!(ends(default, None, 5, &[]), Some(ChainEnd::Passes(5)));
        assert_eq!(ends(Runs(1), None, 0, &[]), None);
        assert_eq!(ends(Runs(1), None, 1, &[]), Some(ChainEnd::Passes(1)));
        // Without a ceiling the cost is not read.
        assert_eq!(ends(Runs(3), None, 0, &[None]), None);
        // Unlimited: the ceiling alone, reached at the ceiling.
        let some = |costs: &[f64]| costs.iter().map(|c| Some(*c)).collect::<Vec<_>>();
        assert_eq!(
            ends(Unlimited, Some(1.0), 49, &some(&[0.3, 0.3, 0.3])),
            None
        );
        let budget = |spent_micros| {
            Some(ChainEnd::Budget {
                spent_micros,
                ceiling_micros: 1_000_000,
            })
        };
        assert_eq!(
            ends(Unlimited, Some(1.0), 3, &some(&[0.3, 0.3, 0.4])),
            budget(1_000_000)
        );
        assert_eq!(
            ends(Unlimited, Some(1.0), 3, &some(&[0.3, 0.3, 0.5])),
            budget(1_100_000)
        );
        assert_eq!(
            ends(Unlimited, Some(1.0), 0, &[Some(0.1), None, Some(0.1)]),
            Some(ChainEnd::CostUnreported)
        );
        // A cost that is not a dollar amount is unreported.
        for broken in [-0.1, f64::NAN, f64::INFINITY] {
            assert_eq!(
                ends(Unlimited, Some(1.0), 0, &[Some(broken)]),
                Some(ChainEnd::CostUnreported),
                "{broken}"
            );
        }
        // Passes and a ceiling: whichever is reached first.
        assert_eq!(
            ends(Runs(2), Some(1.0), 2, &some(&[0.1])),
            Some(ChainEnd::Passes(2))
        );
        assert_eq!(
            ends(Runs(2), Some(1.0), 1, &some(&[1.5])),
            budget(1_500_000)
        );
        assert_eq!(ends(Runs(2), Some(1.0), 1, &some(&[0.5])), None);
        assert_eq!(usd_micros(0.4), Some(400_000));
        assert_eq!(usd_micros(-1.0), None);
        // The asking runs are measured, never bounded (OWL-376): $11.70 of
        // them, one unreported, beside a resolver run of $0.30 under a $1
        // ceiling end nothing.
        let policy = GatePolicy::new(Vec::<String>::new(), PlanApproval::Never).with_resolver(
            ResolverPolicy {
                passes: Unlimited,
                ceiling_micros: usd_micros(1.0),
                ..ResolverPolicy::default()
            },
        );
        let mut chain = Chain::default();
        chain.asked(Some(9.7));
        chain.asked(Some(2.0));
        assert_eq!(chain.asking_cost(), ChainCost::Known(11_700_000));
        chain.resolved(Some(0.3));
        chain.asked(None);
        assert_eq!(chain.asking_cost(), ChainCost::Unreported);
        assert_eq!(chain.cost(), ChainCost::Known(300_000));
        assert_eq!(policy.chain_end(chain), None);
    }

    /// The approval question is the decider's alone under `always` and on
    /// the risky variant, whatever the mode; under `on-fork` it reaches the
    /// resolver (OWL-342).
    #[test]
    fn the_approval_is_human_under_always_or_risky_only() {
        for (mode, variant, human) in [
            (PlanApproval::Always, Variant::Standard, true),
            (PlanApproval::OnFork, Variant::Standard, false),
            (PlanApproval::Never, Variant::Standard, false),
            (PlanApproval::OnFork, Variant::Risky, true),
            (PlanApproval::Never, Variant::Risky, true),
        ] {
            let policy = GatePolicy::new(Vec::<String>::new(), mode);
            assert_eq!(
                policy.approval_is_human(Pipeline::new(variant)),
                human,
                "{mode:?} on {variant:?}"
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
