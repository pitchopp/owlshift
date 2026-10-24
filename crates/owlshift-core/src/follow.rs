//! What a read of a delivered pull request decides (OWL-209): the table of
//! architecture section 5, "Watch after delivery", as a pure function over
//! plain data. The runner does the reading, keeps the times and maps the
//! forge's answer onto [`Read`] and [`BaseRead`]; OWL-112 and OWL-210 run the
//! fix and the rebase it decides. On a project that opted into merge on green
//! (D14), [`merge_due`] decides on top of a green read whether the runner
//! merges, and [`confirm_merge`] whether the merge still goes on at the last
//! read before the call.

use std::time::Duration;

use crate::floor::MergeGrant;
use crate::gate::{GatePolicy, Route};
use crate::vocab::{MergeMethod, MergeSwitch};

/// How long watch waits on a failed check beside one still unfinished before
/// it decides on the failures it has: one hour since it first read a failure
/// on that head.
pub const OVERDUE_AFTER: Duration = Duration::from_secs(60 * 60);

/// The pull request's state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrState {
    Open,
    Closed,
    Merged,
}

/// Whether it merges without conflicts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mergeable {
    Mergeable,
    Conflicting,
    /// Not computed yet, or a value the forge adapter does not know.
    Unknown,
}

/// What one check says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckOutcome {
    Passed,
    /// Not finished yet.
    Pending,
    /// Failed by the code: a check run concluding `FAILURE` or `TIMED_OUT`,
    /// or a commit status `FAILURE` or `ERROR`.
    CodeFailure,
    /// Failed otherwise: `CANCELLED`, `STALE`, `STARTUP_FAILURE`,
    /// `ACTION_REQUIRED`, or a value the adapter does not know. A person
    /// re-runs such a check.
    OtherFailure,
}

impl CheckOutcome {
    /// A failure, by the code or not.
    pub fn failed(self) -> bool {
        matches!(self, Self::CodeFailure | Self::OtherFailure)
    }

    /// How bad, for checks of one name: failed, then running, then passed.
    fn rank(self) -> u8 {
        match self {
            Self::Passed => 0,
            Self::Pending => 1,
            Self::CodeFailure | Self::OtherFailure => 2,
        }
    }
}

/// One check, by its name (a check run's, or a commit status's context).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckRead {
    pub name: String,
    pub outcome: CheckOutcome,
}

/// The pull request as read: its state, head, mergeability and the complete
/// check set of that head.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Read {
    pub state: PrState,
    pub head: String,
    pub mergeable: Mergeable,
    pub checks: Vec<CheckRead>,
}

/// What `delivery.json` says of Owlshift's pushes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Followed<'a> {
    pub pushed: &'a str,
    pub pushing: Option<&'a str>,
    pub rebased_onto: Option<&'a str>,
}

/// The base branch's head, its checks, and the checks of `base_at_push`
/// when they could be read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BaseRead {
    pub head: String,
    pub at_head: Vec<CheckRead>,
    pub at_push: Option<Vec<CheckRead>>,
}

/// Why watch stops following.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EndReason {
    Merged,
    Closed,
    /// The head is neither Owlshift's pushed commit nor its push under way:
    /// a person pushed.
    HeadMoved,
}

/// What a read decides.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Stop following.
    End(EndReason),
    /// Keep following; `adopt` when the head is Owlshift's push under way,
    /// which becomes its pushed commit.
    Follow { adopt: bool, action: Action },
}

/// What watch does about a pull request it follows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// Mergeability is not computed yet: read again at the next pass.
    WaitMergeability,
    /// The decision depends on the base branch: read it, then decide again.
    NeedsBase,
    /// Checks still run, or there are none yet.
    WaitChecks,
    /// The red is the base's: nothing is run or counted.
    WaitBaseRed,
    /// Every check passed with no conflict: the fix passes are cleared.
    Green,
    /// A rebase run onto `onto`; `refresh` when there is no conflict but
    /// the checks failed only on the base the head was pushed onto.
    Rebase { onto: String, refresh: bool },
    /// The pull request conflicts with the base head the last rebase run
    /// used: that rebase failed, and no second one runs onto the same head.
    RebaseFailed { onto: String },
    /// A fix run. `overdue` names the checks still unfinished an hour after
    /// a failure was first read beside them.
    Fix { overdue: Vec<String> },
    /// Only failures a person re-runs: park.
    Park,
}

/// Decides on `read` by architecture section 5's table, in its order.
/// `failing_for` is how long ago watch first read a failure on this head,
/// `None` when this read is the first. `base` is the base branch's read, when
/// one was made: a decision that needs it without it is
/// [`Action::NeedsBase`].
pub fn decide(
    read: &Read,
    followed: Followed<'_>,
    base: Option<&BaseRead>,
    failing_for: Option<Duration>,
) -> Decision {
    match read.state {
        PrState::Merged => return Decision::End(EndReason::Merged),
        PrState::Closed => return Decision::End(EndReason::Closed),
        PrState::Open => {}
    }
    let adopt = read.head != followed.pushed && followed.pushing == Some(read.head.as_str());
    if read.head != followed.pushed && !adopt {
        return Decision::End(EndReason::HeadMoved);
    }
    let action = act(read, followed, base, failing_for);
    Decision::Follow { adopt, action }
}

fn act(
    read: &Read,
    followed: Followed<'_>,
    base: Option<&BaseRead>,
    failing_for: Option<Duration>,
) -> Action {
    match read.mergeable {
        Mergeable::Unknown => return Action::WaitMergeability,
        Mergeable::Conflicting => {
            let Some(base) = base else {
                return Action::NeedsBase;
            };
            let onto = base.head.clone();
            return if followed.rebased_onto == Some(base.head.as_str()) {
                Action::RebaseFailed { onto }
            } else {
                Action::Rebase {
                    onto,
                    refresh: false,
                }
            };
        }
        Mergeable::Mergeable => {}
    }
    let checks = collapse(&read.checks);
    let failed: Vec<&CheckRead> = checks.iter().filter(|c| c.outcome.failed()).collect();
    let pending: Vec<String> = checks
        .iter()
        .filter(|c| c.outcome == CheckOutcome::Pending)
        .map(|c| c.name.clone())
        .collect();
    if failed.is_empty() {
        return if pending.is_empty() && !checks.is_empty() {
            Action::Green
        } else {
            Action::WaitChecks
        };
    }
    let overdue = if pending.is_empty() {
        Vec::new()
    } else if failing_for.is_some_and(|ago| ago >= OVERDUE_AFTER) {
        pending
    } else {
        return Action::WaitChecks;
    };
    if failed
        .iter()
        .all(|c| c.outcome == CheckOutcome::OtherFailure)
    {
        return Action::Park;
    }
    let Some(base) = base else {
        return Action::NeedsBase;
    };
    let at_head = collapse(&base.at_head);
    let base_running = at_head.iter().any(|c| c.outcome == CheckOutcome::Pending);
    let on_head = |name: &str| outcome_of(&at_head, name);
    // A check absent from the base head counts as running while any base
    // check is unfinished: an aggregate check exists only once its matrix
    // has ended.
    let red_on_base = failed.iter().all(|c| match on_head(&c.name) {
        Some(outcome) => outcome.failed() || outcome == CheckOutcome::Pending,
        None => base_running,
    });
    if red_on_base {
        return Action::WaitBaseRed;
    }
    if let Some(at_push) = &base.at_push {
        let at_push = collapse(at_push);
        let refreshed = failed.iter().all(|c| {
            let failed_then = at_push
                .iter()
                .any(|then| then.name == c.name && then.outcome.failed());
            failed_then && on_head(&c.name) == Some(CheckOutcome::Passed)
        });
        if refreshed {
            return Action::Rebase {
                onto: base.head.clone(),
                refresh: true,
            };
        }
    }
    Action::Fix { overdue }
}

/// The checks with each name once, at its worst outcome, in the order each
/// name first appears.
fn collapse(checks: &[CheckRead]) -> Vec<CheckRead> {
    let mut out: Vec<CheckRead> = Vec::new();
    for check in checks {
        match out.iter_mut().find(|c| c.name == check.name) {
            Some(seen) if check.outcome.rank() > seen.outcome.rank() => {
                seen.outcome = check.outcome;
            }
            Some(_) => {}
            None => out.push(check.clone()),
        }
    }
    out
}

/// The outcome of the checks named `name` in `checks`, at their worst, as
/// the table reads them; `None` when none has that name. A fix run's brief
/// gives each failed check's state on the base head by it (OWL-112).
pub fn outcome_of(checks: &[CheckRead], name: &str) -> Option<CheckOutcome> {
    checks
        .iter()
        .filter(|c| c.name == name)
        .map(|c| c.outcome)
        .max_by_key(|outcome| outcome.rank())
}

/// Whether `read` shows a failed check: the runner's first-failure time
/// starts at the first such read of a head and is forgotten at the first
/// read without one.
pub fn has_failure(read: &Read) -> bool {
    read.checks.iter().any(|c| c.outcome.failed())
}

/// The tracker label that lets the runner merge a ticket's pull request under
/// `[forge] merge = "labeled"` (D14): a fixed name, read by watch.
pub const MERGE_LABEL: &str = "owlshift:merge";

/// How old the first green read of a head must be before a later green read
/// of it may merge: two green reads at least a minute apart (D14).
pub const GREEN_TWICE_AFTER: Duration = Duration::from_secs(60);

/// What the project lets the runner merge, read at the base branch's head:
/// `[forge] merge` and `merge_method`, and the gate policy that says which
/// question categories the project sends to a human.
#[derive(Clone, Copy, Debug)]
pub struct MergePolicy<'a> {
    pub switch: MergeSwitch,
    pub method: MergeMethod,
    pub gate: &'a GatePolicy,
}

/// The first green read watch keeps for a followed pull request
/// (`Following`): the head it was on and how long ago it was made.
#[derive(Clone, Copy, Debug)]
pub struct FirstGreen<'a> {
    pub head: &'a str,
    pub ago: Duration,
}

/// What the runner read for [`merge_due`] at a watch pass. The pass's own
/// conditions are the runner's to hold before it asks: the ticket is at
/// Watch, no round waits, it is not parked, the project is not quarantined,
/// the ticket ref was read again under the project lock, and a failed read
/// of the label, the stage or the questions means no merge at that pass
/// (architecture section 5, "Merge on green").
#[derive(Clone, Copy, Debug)]
pub struct MergeFacts<'a> {
    /// What [`decide`] made of this pass's read.
    pub decision: &'a Decision,
    /// The commit Owlshift last pushed (`delivery.json`'s `pushed`): the head
    /// the merge expects.
    pub pushed: &'a str,
    /// The first green read of a head, `None` when this read is the first.
    pub first_green: Option<FirstGreen<'a>>,
    /// The base branch's head read at this pass.
    pub base_head: &'a str,
    /// `delivery.json`'s `base_at_push`: the base CI merged the head with.
    pub base_at_push: Option<&'a str>,
    /// Whether the ticket carries [`MERGE_LABEL`].
    pub labeled: bool,
    /// Whether the ticket carries the project's `[tracker].exclude` label: a
    /// ticket kept out is never merged, label or not.
    pub excluded: bool,
    /// The categories the ticket's questions were asked under, the
    /// resolver's labels among them when the runner counts them.
    pub categories: &'a [String],
    /// Whether the ticket's stage is still one the project maps for review.
    pub at_review: bool,
    /// Whether the pull request is a draft.
    pub draft: bool,
    /// Whether anything the same pass reads first is due or running: the
    /// DELIVERY report the ref owes, a correction, a review loop, a decider
    /// reply, a human review comment.
    pub something_due: bool,
}

/// Why [`merge_due`] does not merge at this pass.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MergeBlocker {
    /// The read did not decide [`Action::Green`] on Owlshift's pushed head.
    NotGreen,
    /// `[forge] merge` is `never`.
    SwitchOff,
    /// The ticket carries the project's exclusion label.
    Excluded,
    /// `[forge] merge` is `labeled` and the ticket lacks [`MERGE_LABEL`].
    Unlabeled,
    /// A person moved the ticket out of the review stage.
    StageMoved,
    /// A question of the ticket was asked under this category, which the
    /// project sends to a human.
    HumanCategory(String),
    /// The pull request is a draft.
    Draft,
    /// Something is due or running first.
    SomethingDue,
    /// The base Owlshift pushed onto is not known: no merge, and nothing
    /// more is known about the base.
    BaseUnknown,
    /// The base moved since the push: the checks ran on a merge with the old
    /// base, so a refresh rebase is due.
    BaseMoved,
    /// The head was not read green twice at least [`GREEN_TWICE_AFTER`]
    /// apart.
    GreenOnce,
    /// The last read found the pull request closed or merged ([`confirm_merge`]).
    Closed,
    /// The last read found another head than the grant's.
    HeadMoved,
    /// The last read found the pull request targeting another base branch,
    /// even one at the same commit.
    BaseRetargeted,
    /// The last read found the review decision a review required, or one
    /// Owlshift does not know: a refusal that will not pass on that head,
    /// notified once, until the head or the base changes, a person's
    /// `continue`, or the review decision clearing on that same head.
    ReviewRequired,
    /// The last read found changes requested: the same refusal as
    /// [`Self::ReviewRequired`].
    ChangesRequested,
}

/// What [`merge_due`] decides.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MergeDue {
    /// Merge, under the grant the floor checks.
    Merge(MergeGrant),
    /// No merge at this pass.
    Wait(MergeBlocker),
}

/// Whether the runner merges the pull request at this pass (D14,
/// architecture section 5, "Merge on green"): only when `facts.decision` is
/// [`Action::Green`] on the head `read` shows, that head is Owlshift's pushed
/// commit and every condition of the design holds. `Green` keeps its
/// meaning; this decides on top of it. The grant carries the pushed head as
/// the merge's expected head.
pub fn merge_due(policy: &MergePolicy<'_>, facts: &MergeFacts<'_>, read: &Read) -> MergeDue {
    match blocker(policy, facts, read) {
        Some(blocker) => MergeDue::Wait(blocker),
        None => MergeDue::Merge(MergeGrant::new(facts.pushed, policy.method)),
    }
}

/// The first condition of [`merge_due`] that does not hold, in the design's
/// order.
fn blocker(policy: &MergePolicy<'_>, facts: &MergeFacts<'_>, read: &Read) -> Option<MergeBlocker> {
    let green = matches!(
        facts.decision,
        Decision::Follow {
            adopt: false,
            action: Action::Green
        }
    );
    if !green || read.state != PrState::Open || read.head != facts.pushed {
        return Some(MergeBlocker::NotGreen);
    }
    match policy.switch {
        MergeSwitch::Never => return Some(MergeBlocker::SwitchOff),
        _ if facts.excluded => return Some(MergeBlocker::Excluded),
        MergeSwitch::Labeled if !facts.labeled => return Some(MergeBlocker::Unlabeled),
        MergeSwitch::Labeled | MergeSwitch::All => {}
    }
    if !facts.at_review {
        return Some(MergeBlocker::StageMoved);
    }
    if let Some(category) = facts
        .categories
        .iter()
        .find(|category| policy.gate.route(category) == Route::Human)
    {
        return Some(MergeBlocker::HumanCategory(category.clone()));
    }
    if facts.draft {
        return Some(MergeBlocker::Draft);
    }
    if facts.something_due {
        return Some(MergeBlocker::SomethingDue);
    }
    match facts.base_at_push {
        None => return Some(MergeBlocker::BaseUnknown),
        Some(at_push) if at_push != facts.base_head => return Some(MergeBlocker::BaseMoved),
        Some(_) => {}
    }
    let green_twice = facts
        .first_green
        .is_some_and(|first| first.head == facts.pushed && first.ago >= GREEN_TWICE_AFTER);
    if !green_twice {
        return Some(MergeBlocker::GreenOnce);
    }
    None
}

/// The forge's review decision for a pull request (GitHub's
/// `reviewDecision`), which reads a review required whatever the merging
/// token may bypass (OWL-446, C19).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReviewState {
    /// No review is required.
    NotRequired,
    Approved,
    /// A required review is missing, or the decision is one Owlshift does
    /// not know.
    Required,
    ChangesRequested,
}

/// The read of the pull request the runner makes once [`merge_due`] said
/// merge, right before the call (architecture section 5, "Merge on green",
/// "The last read"): never the pass's own read, which the merge would then
/// trust across the rest of the pass.
#[derive(Clone, Copy, Debug)]
pub struct LastRead<'a> {
    pub state: PrState,
    pub head: &'a str,
    /// The base branch the pull request targets.
    pub base: &'a str,
    /// That branch's head.
    pub base_head: &'a str,
    pub draft: bool,
    pub review: ReviewState,
}

/// Whether the merge [`merge_due`] granted still goes on, on the last read
/// (OWL-446): open, at the grant's head, on `base` still at `base_at_push`,
/// not a draft, and a review decision that clears, in that order, so a base
/// that moved is found before a review and gets its refresh rebase. The
/// same grant comes back when it goes on. Neither GitHub merge call refuses
/// a closed pull request or takes a base to expect, so this narrows them to
/// the time between the read and the call; it does not pin them.
pub fn confirm_merge(
    grant: MergeGrant,
    base: &str,
    base_at_push: &str,
    last: &LastRead<'_>,
) -> MergeDue {
    let blocker = if last.state != PrState::Open {
        Some(MergeBlocker::Closed)
    } else if last.head != grant.head() {
        Some(MergeBlocker::HeadMoved)
    } else if last.base != base {
        Some(MergeBlocker::BaseRetargeted)
    } else if last.base_head != base_at_push {
        Some(MergeBlocker::BaseMoved)
    } else if last.draft {
        Some(MergeBlocker::Draft)
    } else {
        match last.review {
            ReviewState::NotRequired | ReviewState::Approved => None,
            ReviewState::Required => Some(MergeBlocker::ReviewRequired),
            ReviewState::ChangesRequested => Some(MergeBlocker::ChangesRequested),
        }
    };
    match blocker {
        Some(blocker) => MergeDue::Wait(blocker),
        None => MergeDue::Merge(grant),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use CheckOutcome::*;

    const PUSHED: &str = "p";

    fn check(name: &str, outcome: CheckOutcome) -> CheckRead {
        CheckRead {
            name: name.to_owned(),
            outcome,
        }
    }

    fn read(mergeable: Mergeable, checks: &[(&str, CheckOutcome)]) -> Read {
        Read {
            state: PrState::Open,
            head: PUSHED.to_owned(),
            mergeable,
            checks: checks.iter().map(|(n, o)| check(n, *o)).collect(),
        }
    }

    fn followed() -> Followed<'static> {
        Followed {
            pushed: PUSHED,
            pushing: None,
            rebased_onto: None,
        }
    }

    fn base(
        head: &str,
        at_head: &[(&str, CheckOutcome)],
        at_push: Option<&[(&str, CheckOutcome)]>,
    ) -> BaseRead {
        let list = |l: &[(&str, CheckOutcome)]| l.iter().map(|(n, o)| check(n, *o)).collect();
        BaseRead {
            head: head.to_owned(),
            at_head: list(at_head),
            at_push: at_push.map(list),
        }
    }

    fn action(read: &Read, base: Option<&BaseRead>, failing_for: Option<Duration>) -> Action {
        match decide(read, followed(), base, failing_for) {
            Decision::Follow {
                adopt: false,
                action,
            } => action,
            other => panic!("{other:?}"),
        }
    }

    /// The state comes before anything else, then the head.
    #[test]
    fn a_merged_closed_or_pushed_to_pull_request_is_no_longer_followed() {
        let mut merged = read(Mergeable::Unknown, &[("ci", CodeFailure)]);
        merged.state = PrState::Merged;
        // Merged and closed are read even at someone else's head.
        merged.head = "someone".to_owned();
        assert_eq!(
            decide(&merged, followed(), None, None),
            Decision::End(EndReason::Merged)
        );
        merged.state = PrState::Closed;
        assert_eq!(
            decide(&merged, followed(), None, None),
            Decision::End(EndReason::Closed)
        );
        merged.state = PrState::Open;
        assert_eq!(
            decide(&merged, followed(), None, None),
            Decision::End(EndReason::HeadMoved)
        );
    }

    /// A head equal to the push under way is adopted, and the rest of the
    /// read decides; the pushed commit itself is never "adopted".
    #[test]
    fn a_head_equal_to_the_push_under_way_is_adopted() {
        let mut at_pushing = read(Mergeable::Mergeable, &[("ci", Passed)]);
        at_pushing.head = "n".to_owned();
        let pushing = Followed {
            pushing: Some("n"),
            ..followed()
        };
        assert_eq!(
            decide(&at_pushing, pushing, None, None),
            Decision::Follow {
                adopt: true,
                action: Action::Green
            }
        );
        let at_pushed = read(Mergeable::Mergeable, &[("ci", Passed)]);
        assert_eq!(
            decide(&at_pushed, pushing, None, None),
            Decision::Follow {
                adopt: false,
                action: Action::Green
            }
        );
    }

    #[test]
    fn unknown_mergeability_waits_before_any_check_counts() {
        let r = read(Mergeable::Unknown, &[("ci", CodeFailure)]);
        assert_eq!(action(&r, None, None), Action::WaitMergeability);
    }

    /// A conflict comes before the checks; a second rebase onto the base
    /// head the last one used is a failed run of that rebase.
    #[test]
    fn a_conflict_is_a_rebase_unless_onto_the_head_already_used() {
        let r = read(Mergeable::Conflicting, &[("ci", CodeFailure)]);
        assert_eq!(action(&r, None, None), Action::NeedsBase);
        let b = base("b2", &[], None);
        assert_eq!(
            action(&r, Some(&b), None),
            Action::Rebase {
                onto: "b2".to_owned(),
                refresh: false
            }
        );
        let used = Followed {
            rebased_onto: Some("b2"),
            ..followed()
        };
        assert_eq!(
            decide(&r, used, Some(&b), None),
            Decision::Follow {
                adopt: false,
                action: Action::RebaseFailed {
                    onto: "b2".to_owned()
                }
            }
        );
    }

    /// A failure beside an unfinished check waits, for an hour since the
    /// first failure read on that head, then goes on with the unfinished
    /// check named.
    #[test]
    fn a_failure_beside_a_running_check_waits_an_hour() {
        let r = read(
            Mergeable::Mergeable,
            &[("ci", CodeFailure), ("win", Pending)],
        );
        let b = base("b", &[("ci", Passed), ("win", Passed)], None);
        for ago in [None, Some(Duration::from_secs(59 * 60))] {
            assert_eq!(action(&r, Some(&b), ago), Action::WaitChecks, "{ago:?}");
        }
        assert_eq!(
            action(&r, Some(&b), Some(OVERDUE_AFTER)),
            Action::Fix {
                overdue: vec!["win".to_owned()]
            }
        );
    }

    /// Failures none of which the code made park; one code failure among
    /// them goes on.
    #[test]
    fn failures_no_code_made_park() {
        let r = read(
            Mergeable::Mergeable,
            &[("ci", OtherFailure), ("lint", OtherFailure), ("ok", Passed)],
        );
        assert_eq!(action(&r, None, None), Action::Park);
        let mixed = read(
            Mergeable::Mergeable,
            &[("ci", OtherFailure), ("t", CodeFailure)],
        );
        assert_eq!(action(&mixed, None, None), Action::NeedsBase);
    }

    /// The base's red is not the pull request's: failed or running on the
    /// base head, or absent there while a base check runs.
    #[test]
    fn a_red_the_base_shares_waits() {
        let r = read(
            Mergeable::Mergeable,
            &[("ci", CodeFailure), ("t", CodeFailure)],
        );
        let failing = base("b", &[("ci", CodeFailure), ("t", Pending)], None);
        assert_eq!(action(&r, Some(&failing), None), Action::WaitBaseRed);
        let absent_running = base("b", &[("ci", CodeFailure), ("x", Pending)], None);
        assert_eq!(action(&r, Some(&absent_running), None), Action::WaitBaseRed);
        // Absent with every base check finished: the pull request's own.
        let absent_done = base("b", &[("ci", CodeFailure)], None);
        assert_eq!(
            action(&r, Some(&absent_done), None),
            Action::Fix { overdue: vec![] }
        );
    }

    /// Failed on the base the head was pushed onto and passing on the base
    /// head now: a rebase refreshes it, since CI does not run again.
    #[test]
    fn a_red_the_base_has_fixed_since_is_a_refresh_rebase() {
        let r = read(Mergeable::Mergeable, &[("ci", CodeFailure), ("ok", Passed)]);
        let fixed = base("b2", &[("ci", Passed)], Some(&[("ci", CodeFailure)]));
        assert_eq!(
            action(&r, Some(&fixed), None),
            Action::Rebase {
                onto: "b2".to_owned(),
                refresh: true
            }
        );
        // Passing then too: the pull request's own failure.
        let own = base("b2", &[("ci", Passed)], Some(&[("ci", Passed)]));
        assert_eq!(
            action(&r, Some(&own), None),
            Action::Fix { overdue: vec![] }
        );
        // `base_at_push`'s checks unreadable: no refresh, a fix.
        let unknown = base("b2", &[("ci", Passed)], None);
        assert_eq!(
            action(&r, Some(&unknown), None),
            Action::Fix { overdue: vec![] }
        );
    }

    #[test]
    fn running_checks_or_none_wait_and_every_check_passed_is_green() {
        let running = read(Mergeable::Mergeable, &[("ci", Passed), ("t", Pending)]);
        assert_eq!(action(&running, None, None), Action::WaitChecks);
        let none = read(Mergeable::Mergeable, &[]);
        assert_eq!(action(&none, None, None), Action::WaitChecks);
        let green = read(Mergeable::Mergeable, &[("ci", Passed), ("t", Passed)]);
        assert_eq!(action(&green, None, None), Action::Green);
    }

    /// Checks of one name count as their worst, on the pull request and on
    /// the base.
    #[test]
    fn checks_of_one_name_count_as_their_worst() {
        let r = read(Mergeable::Mergeable, &[("ci", Passed), ("ci", CodeFailure)]);
        let b = base("b", &[("ci", CodeFailure), ("ci", Passed)], None);
        assert_eq!(action(&r, Some(&b), None), Action::WaitBaseRed);
        let running = read(Mergeable::Mergeable, &[("ci", Passed), ("ci", Pending)]);
        assert_eq!(action(&running, None, None), Action::WaitChecks);
        assert!(has_failure(&r));
        assert!(!has_failure(&running));
        assert_eq!(outcome_of(&b.at_head, "ci"), Some(CodeFailure));
        assert_eq!(outcome_of(&running.checks, "ci"), Some(Pending));
        assert_eq!(outcome_of(&running.checks, "lint"), None);
    }

    mod merge {
        use super::super::*;
        use crate::vocab::{DelegationLevel, PlanApproval};

        const HEAD: &str = "0123456789abcdef0123456789abcdef01234567";
        const BASE: &str = "89abcdef0123456789abcdef0123456789abcdef";
        const GREEN: Decision = Decision::Follow {
            adopt: false,
            action: Action::Green,
        };

        fn gate(level: DelegationLevel) -> GatePolicy {
            let resolver = crate::gate::ResolverPolicy {
                level,
                ..Default::default()
            };
            GatePolicy::new(["billing"], PlanApproval::Never).with_resolver(resolver)
        }

        fn green_read() -> Read {
            Read {
                state: PrState::Open,
                head: HEAD.to_owned(),
                mergeable: Mergeable::Mergeable,
                checks: Vec::new(),
            }
        }

        /// Every condition met: `labeled` with the label, a minute between
        /// the green reads.
        fn facts<'a>(decision: &'a Decision, categories: &'a [String]) -> MergeFacts<'a> {
            MergeFacts {
                decision,
                pushed: HEAD,
                first_green: Some(FirstGreen {
                    head: HEAD,
                    ago: GREEN_TWICE_AFTER,
                }),
                base_head: BASE,
                base_at_push: Some(BASE),
                labeled: true,
                excluded: false,
                categories,
                at_review: true,
                draft: false,
                something_due: false,
            }
        }

        fn due(
            switch: MergeSwitch,
            gate: &GatePolicy,
            facts: &MergeFacts<'_>,
            read: &Read,
        ) -> MergeDue {
            let policy = MergePolicy {
                switch,
                method: MergeMethod::Rebase,
                gate,
            };
            merge_due(&policy, facts, read)
        }

        #[test]
        fn all_conditions_met_merge_at_the_pushed_head_with_the_method() {
            let gate = gate(DelegationLevel::Discoverable);
            let asked = ["naming".to_owned()];
            for switch in [MergeSwitch::Labeled, MergeSwitch::All] {
                let MergeDue::Merge(grant) =
                    due(switch, &gate, &facts(&GREEN, &asked), &green_read())
                else {
                    panic!("{switch:?}");
                };
                assert_eq!((grant.head(), grant.method()), (HEAD, MergeMethod::Rebase));
            }
            // `all` needs no label.
            let unlabeled = MergeFacts {
                labeled: false,
                ..facts(&GREEN, &[])
            };
            assert!(matches!(
                due(MergeSwitch::All, &gate, &unlabeled, &green_read()),
                MergeDue::Merge(_)
            ));
        }

        /// Each condition, broken alone, stops the merge with its reason.
        #[test]
        fn each_condition_alone_blocks_the_merge() {
            use MergeBlocker::*;
            let gate = gate(DelegationLevel::Discoverable);
            let other = "fedcba9876543210fedcba9876543210fedcba98";
            let adopted = Decision::Follow {
                adopt: true,
                action: Action::Green,
            };
            let waiting = Decision::Follow {
                adopt: false,
                action: Action::WaitChecks,
            };
            let ended = Decision::End(EndReason::Merged);
            let human = ["Possible security issue".to_owned()];
            let added = ["billing_change".to_owned()];
            let base = facts(&GREEN, &[]);
            let mut moved_head = green_read();
            moved_head.head = other.to_owned();
            let mut closed = green_read();
            closed.state = PrState::Closed;
            let cases: Vec<(MergeSwitch, MergeFacts<'_>, Read, MergeBlocker)> = vec![
                (
                    MergeSwitch::All,
                    facts(&adopted, &[]),
                    green_read(),
                    NotGreen,
                ),
                (
                    MergeSwitch::All,
                    facts(&waiting, &[]),
                    green_read(),
                    NotGreen,
                ),
                (MergeSwitch::All, facts(&ended, &[]), green_read(), NotGreen),
                (MergeSwitch::All, base, moved_head, NotGreen),
                (MergeSwitch::All, base, closed, NotGreen),
                (MergeSwitch::Never, base, green_read(), SwitchOff),
                (
                    MergeSwitch::All,
                    MergeFacts {
                        excluded: true,
                        ..base
                    },
                    green_read(),
                    Excluded,
                ),
                (
                    MergeSwitch::Labeled,
                    MergeFacts {
                        labeled: false,
                        ..base
                    },
                    green_read(),
                    Unlabeled,
                ),
                (
                    MergeSwitch::All,
                    MergeFacts {
                        at_review: false,
                        ..base
                    },
                    green_read(),
                    StageMoved,
                ),
                (
                    MergeSwitch::All,
                    facts(&GREEN, &human),
                    green_read(),
                    HumanCategory(human[0].clone()),
                ),
                (
                    MergeSwitch::All,
                    facts(&GREEN, &added),
                    green_read(),
                    HumanCategory(added[0].clone()),
                ),
                (
                    MergeSwitch::All,
                    MergeFacts {
                        draft: true,
                        ..base
                    },
                    green_read(),
                    Draft,
                ),
                (
                    MergeSwitch::All,
                    MergeFacts {
                        something_due: true,
                        ..base
                    },
                    green_read(),
                    SomethingDue,
                ),
                (
                    MergeSwitch::All,
                    MergeFacts {
                        base_at_push: None,
                        ..base
                    },
                    green_read(),
                    BaseUnknown,
                ),
                (
                    MergeSwitch::All,
                    MergeFacts {
                        base_at_push: Some(other),
                        ..base
                    },
                    green_read(),
                    BaseMoved,
                ),
                (
                    MergeSwitch::All,
                    MergeFacts {
                        first_green: None,
                        ..base
                    },
                    green_read(),
                    GreenOnce,
                ),
                (
                    MergeSwitch::All,
                    MergeFacts {
                        first_green: Some(FirstGreen {
                            head: HEAD,
                            ago: GREEN_TWICE_AFTER - Duration::from_secs(1),
                        }),
                        ..base
                    },
                    green_read(),
                    GreenOnce,
                ),
                (
                    MergeSwitch::All,
                    MergeFacts {
                        first_green: Some(FirstGreen {
                            head: other,
                            ago: GREEN_TWICE_AFTER,
                        }),
                        ..base
                    },
                    green_read(),
                    GreenOnce,
                ),
            ];
            for (switch, facts, read, blocker) in cases {
                assert_eq!(
                    due(switch, &gate, &facts, &read),
                    MergeDue::Wait(blocker.clone()),
                    "{blocker:?}"
                );
            }
        }

        fn last() -> LastRead<'static> {
            LastRead {
                state: PrState::Open,
                head: HEAD,
                base: "main",
                base_head: BASE,
                draft: false,
                review: ReviewState::NotRequired,
            }
        }

        fn confirm(last: &LastRead<'_>) -> MergeDue {
            confirm_merge(
                MergeGrant::new(HEAD, MergeMethod::Squash),
                "main",
                BASE,
                last,
            )
        }

        /// OWL-446: the grant goes on, unchanged, on a review approved or
        /// none required.
        #[test]
        fn the_last_read_passes_the_grant_on_a_review_that_clears() {
            for review in [ReviewState::NotRequired, ReviewState::Approved] {
                let last = LastRead { review, ..last() };
                assert_eq!(
                    confirm(&last),
                    MergeDue::Merge(MergeGrant::new(HEAD, MergeMethod::Squash)),
                    "{review:?}"
                );
            }
        }

        /// OWL-446: each condition of the last read, broken alone, stops the
        /// merge with its reason; a base that moved is found before a review.
        #[test]
        fn each_condition_of_the_last_read_alone_blocks_the_merge() {
            use MergeBlocker::*;
            let other = "fedcba9876543210fedcba9876543210fedcba98";
            let cases = [
                (
                    LastRead {
                        state: PrState::Closed,
                        ..last()
                    },
                    Closed,
                ),
                (
                    LastRead {
                        state: PrState::Merged,
                        ..last()
                    },
                    Closed,
                ),
                (
                    LastRead {
                        head: other,
                        ..last()
                    },
                    HeadMoved,
                ),
                // Retargeted onto a branch at the same commit.
                (
                    LastRead {
                        base: "release",
                        ..last()
                    },
                    BaseRetargeted,
                ),
                (
                    LastRead {
                        base_head: other,
                        ..last()
                    },
                    BaseMoved,
                ),
                (
                    LastRead {
                        draft: true,
                        ..last()
                    },
                    Draft,
                ),
                (
                    LastRead {
                        review: ReviewState::Required,
                        ..last()
                    },
                    ReviewRequired,
                ),
                (
                    LastRead {
                        review: ReviewState::ChangesRequested,
                        ..last()
                    },
                    ChangesRequested,
                ),
                (
                    LastRead {
                        base_head: other,
                        review: ReviewState::Required,
                        ..last()
                    },
                    BaseMoved,
                ),
            ];
            for (last, blocker) in cases {
                assert_eq!(
                    confirm(&last),
                    MergeDue::Wait(blocker.clone()),
                    "{blocker:?}"
                );
            }
        }

        /// A category blocks when the project sends it to a human: the
        /// level's set, the project's additions, and every one at `none`.
        #[test]
        fn a_human_category_follows_the_projects_level() {
            let scope = ["scope change".to_owned()];
            let testing = ["testing".to_owned()];
            let blank = [String::new()];
            let blocks = |level, categories: &[String]| {
                matches!(
                    due(
                        MergeSwitch::All,
                        &gate(level),
                        &facts(&GREEN, categories),
                        &green_read()
                    ),
                    MergeDue::Wait(MergeBlocker::HumanCategory(_))
                )
            };
            assert!(blocks(DelegationLevel::Discoverable, &scope));
            assert!(!blocks(DelegationLevel::Ticket, &scope));
            assert!(!blocks(DelegationLevel::All, &testing));
            assert!(!blocks(DelegationLevel::Discoverable, &blank));
            assert!(blocks(DelegationLevel::None, &testing));
            assert!(blocks(DelegationLevel::None, &blank));
            // Whole-word tokens: `scoped` names no category.
            assert!(!blocks(
                DelegationLevel::Discoverable,
                &["scoped".to_owned()]
            ));
        }
    }
}
