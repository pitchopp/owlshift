//! What a read of a delivered pull request decides (OWL-209): the table of
//! architecture section 5, "Watch after delivery", as a pure function over
//! plain data. The runner does the reading, keeps the times and maps the
//! forge's answer onto [`Read`] and [`BaseRead`]; OWL-112 and OWL-210 run the
//! fix and the rebase it decides. On a project that opted into merge on green
//! (D14), [`merge_due`] decides on top of a green read whether the runner
//! merges, and [`confirm_merge`] whether the merge still goes on at the last
//! read before the call.

use std::time::Duration;

use crate::floor::{FloorCategory, MergeGrant, names_floor_category};
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
    /// The check run's id, a GitHub Actions job's id (OWL-513): a re-run's
    /// new attempt of a job has a greater one.
    pub id: Option<u64>,
    /// The workflow run of a GitHub Actions check run, when read.
    pub run: Option<u64>,
    /// Whether the runner can re-run it: a GitHub Actions job whose id and
    /// workflow run were read (OWL-513).
    pub rerunnable: bool,
}

impl CheckRead {
    /// A check known by its name and outcome alone: no id, no workflow run,
    /// not re-runnable.
    pub fn named(name: impl Into<String>, outcome: CheckOutcome) -> Self {
        Self {
            name: name.into(),
            outcome,
            id: None,
            run: None,
            rerunnable: false,
        }
    }
}

/// The first attempt of one job the runner re-ran (OWL-513): its check
/// run's id, its workflow run and its name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FirstAttempt {
    pub id: u64,
    pub run: u64,
    pub name: String,
}

/// The runner's re-run of a head (OWL-513, architecture section 5, "The
/// automatic re-run"): the head, the first attempts of the jobs it re-ran,
/// empty when GitHub refused the re-run, and how long ago it was asked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RerunMark<'a> {
    pub head: &'a str,
    pub first: &'a [FirstAttempt],
    pub since: Duration,
}

/// The checks of `checks` that are newer attempts of `first`'s job: the
/// same name, the same workflow run and a greater id (OWL-513). A check of
/// the same name from another workflow run, or with no run or no id, never
/// is. The one rule the first-attempt view, [`replaced_attempts`],
/// [`rerun_outcome`] and the runner's links all read.
pub fn newer_attempts<'c>(
    checks: &'c [CheckRead],
    first: &FirstAttempt,
) -> impl Iterator<Item = &'c CheckRead> {
    let (name, run, id) = (first.name.clone(), first.run, first.id);
    checks.iter().filter(move |c| {
        c.name == name && c.run == Some(run) && c.id.is_some_and(|newer| newer > id)
    })
}

/// Whether `check` is `first`'s own attempt, by its id.
fn is_first(check: &CheckRead, first: &FirstAttempt) -> bool {
    check.id == Some(first.id)
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

/// What `delivery.json` says of Owlshift's pushes, and what the runner read
/// of the ticket's overlaps (OWL-449).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Followed<'a> {
    pub pushed: &'a str,
    pub pushing: Option<&'a str>,
    pub rebased_onto: Option<&'a str>,
    /// The base branch's head, when it changes a path one of the ticket's
    /// overlaps names and the pushed head does not contain it (architecture
    /// section 5, "Tolerated overlap", "The refresh"): the runner's reading,
    /// from git. `None` otherwise.
    pub overlap: Option<&'a str>,
    /// The runner's re-run kept in `delivery.json` (OWL-513), of whatever
    /// head: only one of the read's head counts.
    pub rerun: Option<RerunMark<'a>>,
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
    /// A rebase run onto `onto`, for `cause`.
    Rebase { onto: String, cause: RebaseCause },
    /// The pull request conflicts with the base head the last rebase run
    /// used: that rebase failed, and no second one runs onto the same head.
    RebaseFailed { onto: String },
    /// Every failed check a code failure of a re-runnable GitHub Actions
    /// job, with no re-run made on this head yet: the runner re-runs their
    /// workflow runs' failed jobs once (OWL-513). Nothing is counted.
    Rerun,
    /// A fix run. `overdue` names the checks still unfinished an hour after
    /// a failure was first read beside them.
    Fix { overdue: Vec<String> },
    /// Only failures a person re-runs: park.
    Park,
}

/// Why a rebase run is due (OWL-449: `rebase_run` events name it, so that
/// the runs are counted by cause).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RebaseCause {
    /// The pull request conflicts with its base.
    Conflict,
    /// Its checks failed only on the base it was pushed onto, and pass on
    /// the base now: a refresh, CI not running again by itself.
    BaseFixed,
    /// Merge on green found it green on a base that moved since the push
    /// (OWL-445): a refresh, which the runner decides.
    MergeBase,
    /// Its base now changes a path one of its overlaps names (OWL-449): a
    /// refresh, so its checks run on the combined code.
    Overlap,
}

impl RebaseCause {
    /// Whether the forge reports no conflict: the run refreshes the branch.
    pub const fn refresh(self) -> bool {
        !matches!(self, Self::Conflict)
    }

    /// The cause's name in events.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Conflict => "conflict",
            Self::BaseFixed => "base_fixed",
            Self::MergeBase => "merge",
            Self::Overlap => "overlap",
        }
    }
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
                    cause: RebaseCause::Conflict,
                }
            };
        }
        Mergeable::Mergeable => {}
    }
    // A base that changes a path an overlap names, before the checks: a
    // merge git makes cleanly does not prove two changes agree (OWL-449).
    if let Some(onto) = followed.overlap
        && followed.rebased_onto != Some(onto)
    {
        return Action::Rebase {
            onto: onto.to_owned(),
            cause: RebaseCause::Overlap,
        };
    }
    // The runner's re-run of this head: its jobs' first attempts give way
    // to their newer ones, and read as running until those show.
    let mark = followed.rerun.filter(|mark| mark.head == read.head);
    let listed = match mark {
        Some(mark) => first_attempt_view(&read.checks, &mark),
        None => read.checks.clone(),
    };
    let checks = collapse(&listed);
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
                cause: RebaseCause::BaseFixed,
            };
        }
    }
    // The checks as listed, not collapsed by name: two workflows' jobs of
    // one name are each read as they are.
    let rerunnable = listed
        .iter()
        .filter(|c| c.outcome.failed())
        .all(|c| c.outcome == CheckOutcome::CodeFailure && c.rerunnable);
    if mark.is_none() && overdue.is_empty() && rerunnable {
        return Action::Rerun;
    }
    Action::Fix { overdue }
}

/// `checks` as the table reads them on the head `mark` re-ran (OWL-513):
/// each re-run job's listed first attempt is left out; with a newer
/// attempt, those stand; with none yet, one check of its name stands in,
/// running until an hour after the call, then failed as its first attempt
/// read. A job the read does not list at all, such as an aggregate check
/// whose legs run again, waits the same way.
fn first_attempt_view(checks: &[CheckRead], mark: &RerunMark<'_>) -> Vec<CheckRead> {
    let mut out: Vec<CheckRead> = checks
        .iter()
        .filter(|c| !mark.first.iter().any(|first| is_first(c, first)))
        .cloned()
        .collect();
    for first in mark.first {
        if newer_attempts(checks, first).next().is_none() {
            out.push(CheckRead {
                name: first.name.clone(),
                outcome: if mark.since >= OVERDUE_AFTER {
                    CheckOutcome::CodeFailure
                } else {
                    CheckOutcome::Pending
                },
                id: Some(first.id),
                run: Some(first.run),
                rerunnable: true,
            });
        }
    }
    out
}

/// One flag per check of `checks`, in order: whether it is the first
/// attempt of a job `mark` re-ran that a newer attempt replaces (OWL-513).
/// A fix run's brief leaves those out.
pub fn replaced_attempts(checks: &[CheckRead], mark: &RerunMark<'_>) -> Vec<bool> {
    checks
        .iter()
        .map(|check| {
            mark.first.iter().any(|first| {
                is_first(check, first) && newer_attempts(checks, first).next().is_some()
            })
        })
        .collect()
}

/// What the runner's re-run of a head came to (OWL-513).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RerunOutcome {
    /// Every re-run job's newer attempt passed: a flake.
    Passed,
    /// A newer attempt failed.
    Failed,
    /// A job showed no newer attempt within an hour of the call.
    Unseen,
}

/// The outcome of `mark`'s re-run on `read`, by [`newer_attempts`]:
/// `None` while a newer attempt runs, or while a job has none and the hour
/// has not passed, or when the read is of another head or the re-run was
/// refused (no first attempt). Then failed when a newer attempt failed,
/// unseen when a job had none, passed otherwise.
pub fn rerun_outcome(read: &Read, mark: &RerunMark<'_>) -> Option<RerunOutcome> {
    if read.head != mark.head || mark.first.is_empty() {
        return None;
    }
    let (mut failed, mut unseen) = (false, false);
    for first in mark.first {
        let mut newer = newer_attempts(&read.checks, first).peekable();
        if newer.peek().is_none() {
            if mark.since < OVERDUE_AFTER {
                return None;
            }
            unseen = true;
        }
        for attempt in newer {
            match attempt.outcome {
                CheckOutcome::Pending => return None,
                CheckOutcome::Passed => {}
                CheckOutcome::CodeFailure | CheckOutcome::OtherFailure => failed = true,
            }
        }
    }
    Some(if failed {
        RerunOutcome::Failed
    } else if unseen {
        RerunOutcome::Unseen
    } else {
        RerunOutcome::Passed
    })
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

/// Who settled a question of the ticket, as [`merge_due`] reads it
/// (OWL-465, architecture section 5, "Merge on green", "No question left for
/// a person").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Settled {
    /// Its decider: the latest verdict an answer check kept on it, on an ask
    /// of its round, is `answered`, a thumbs-up's included.
    Decider,
    /// No person: a decision kept without its decider, the resolver's or the
    /// asking run's recommendation the runner took at a chain's bound,
    /// reversed by the decider or not.
    Resolver,
    /// Asked, and no kept verdict says its decider answered it.
    Open,
}

/// A question of the ticket as [`merge_due`] reads it: the category it was
/// asked under, the resolver's own label for it when the resolver decided it
/// and the runner sent it to the decider (OWL-482), and who settled it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MergeQuestion {
    /// The question's id in its run, and the round of the ask it was asked
    /// on, `None` for a decision kept without its decider: what names it to
    /// a person when it holds the merge (OWL-464). [`merge_due`] reads
    /// neither.
    pub id: String,
    pub round: Option<u32>,
    pub category: String,
    /// Read for the `security` hold alone: the resolver reads the question's
    /// text, and may see `security` where the asking run did not.
    pub labelled: Option<String>,
    pub settled: Settled,
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
    /// Whether the base's move from `base_at_push` to `base_head` leaves
    /// every path the delivery changed alone, as the runner read it from
    /// git (OWL-520; architecture section 5, "Merge on green", "The base the
    /// checks ran on"): then the merge goes on without a refresh rebase.
    /// False when not read, when git could not tell, and for a head whose
    /// merge on a moved base the forge refused: a moved base then gets its
    /// refresh. The runner reads it once [`merge_due`] waits on
    /// [`MergeBlocker::BaseMoved`] alone.
    pub disjoint: bool,
    /// Whether the ticket carries [`MERGE_LABEL`].
    pub labeled: bool,
    /// Whether the ticket carries the project's `[tracker].exclude` label: a
    /// ticket kept out is never merged, label or not.
    pub excluded: bool,
    /// The ticket's questions: every ask's, under the asking run's
    /// category and with the resolver's label kept on its round's ask, then
    /// every decision kept without its decider.
    pub questions: &'a [MergeQuestion],
    /// Whether the ticket's stage is still one the project maps for review.
    pub at_review: bool,
    /// Whether the pull request is a draft.
    pub draft: bool,
    /// Whether anything the same pass reads first is due or running: the
    /// DELIVERY report the ref owes, a correction, a review loop, a decider
    /// reply, a human review comment. Never a follow-up the ticket owes on
    /// the tracker (OWL-501), which is filed after the merge if need be.
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
    /// A question of the ticket was asked under this category, which names
    /// `security`, or the resolver labelled it so, the token `security` then
    /// given: a person merges it, whatever the answer.
    Security(String),
    /// A question of the ticket under this category, which the project
    /// sends to a human, has no answer of its decider on record.
    Unanswered(String),
    /// A question of the ticket under this category, which the project
    /// sends to a human, was settled by a decision kept without its
    /// decider.
    ResolverDecided(String),
    /// The pull request is a draft.
    Draft,
    /// Something is due or running first.
    SomethingDue,
    /// The base Owlshift pushed onto is not known: no merge, and nothing
    /// more is known about the base.
    BaseUnknown,
    /// The base moved since the push, and the move touches a path the
    /// delivery changed, or was not read as leaving them alone
    /// ([`MergeFacts::disjoint`], OWL-520): a refresh rebase is due. From
    /// [`confirm_merge`], the base moved again since [`merge_due`] decided.
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
    if let Some((_, blocker)) = held_question(policy.gate, facts.questions) {
        return Some(blocker);
    }
    if facts.draft {
        return Some(MergeBlocker::Draft);
    }
    if facts.something_due {
        return Some(MergeBlocker::SomethingDue);
    }
    match facts.base_at_push {
        None => return Some(MergeBlocker::BaseUnknown),
        Some(at_push) if at_push != facts.base_head && !facts.disjoint => {
            return Some(MergeBlocker::BaseMoved);
        }
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

/// The question that keeps the ticket for a person, if one does (OWL-465):
/// one under `security` first, whatever the answer, since no answer lifts it,
/// or one the resolver labelled `security` (OWL-482); else, among those under
/// a category the project sends to a human, the first, in the order given,
/// that its decider did not answer. A question its decider answered holds
/// nothing. With the question, which names it to a person (OWL-464).
pub fn held_question<'q>(
    gate: &GatePolicy,
    questions: &'q [MergeQuestion],
) -> Option<(&'q MergeQuestion, MergeBlocker)> {
    let security = |category: &str| names_floor_category(category, FloorCategory::Security);
    if let Some(held) = questions.iter().find_map(|question| {
        if security(&question.category) {
            Some((question, question.category.clone()))
        } else {
            question
                .labelled
                .as_deref()
                .is_some_and(security)
                .then(|| (question, FloorCategory::Security.token().to_owned()))
        }
    }) {
        let (question, category) = held;
        return Some((question, MergeBlocker::Security(category)));
    }
    let mut human = questions
        .iter()
        .filter(|question| gate.route(&question.category) == Route::Human);
    human.find_map(|question| match question.settled {
        Settled::Decider => None,
        Settled::Resolver => Some((
            question,
            MergeBlocker::ResolverDecided(question.category.clone()),
        )),
        Settled::Open => Some((
            question,
            MergeBlocker::Unanswered(question.category.clone()),
        )),
    })
}

impl MergeBlocker {
    /// The hold's key when only a person lifts it, so that the pull request
    /// waits for one rather than for a later pass (OWL-464): a question kept
    /// for a person, the exclusion label, the label [`MERGE_LABEL`] missing.
    /// `None` for a wait the next pass settles and for the last read's.
    pub fn kept_for_a_person(&self) -> Option<&'static str> {
        match self {
            Self::Security(_) => Some("security"),
            Self::Unanswered(_) => Some("unanswered"),
            Self::ResolverDecided(_) => Some("resolver_decided"),
            Self::Excluded => Some("excluded"),
            Self::Unlabeled => Some("unlabeled"),
            Self::NotGreen
            | Self::SwitchOff
            | Self::StageMoved
            | Self::Draft
            | Self::SomethingDue
            | Self::BaseUnknown
            | Self::BaseMoved
            | Self::GreenOnce
            | Self::Closed
            | Self::HeadMoved
            | Self::BaseRetargeted
            | Self::ReviewRequired
            | Self::ChangesRequested => None,
        }
    }
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
/// (OWL-446): open, at the grant's head, on `base` still at `base_head`,
/// the base head [`merge_due`] decided on (`base_at_push`, or one whose move
/// left the delivery's paths alone, OWL-520), not a draft, and a review
/// decision that clears, in that order, so a base that moved again is found
/// before a review and decided again at the next pass. The same grant
/// comes back when it goes on. Neither GitHub merge call refuses a closed
/// pull request or takes a base to expect, so this narrows them to the time
/// between the read and the call; it does not pin them.
pub fn confirm_merge(
    grant: MergeGrant,
    base: &str,
    base_head: &str,
    last: &LastRead<'_>,
) -> MergeDue {
    let blocker = if last.state != PrState::Open {
        Some(MergeBlocker::Closed)
    } else if last.head != grant.head() {
        Some(MergeBlocker::HeadMoved)
    } else if last.base != base {
        Some(MergeBlocker::BaseRetargeted)
    } else if last.base_head != base_head {
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
        CheckRead::named(name, outcome)
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
            overlap: None,
            rerun: None,
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
    /// OWL-449: a base that changes a path an overlap names gets a refresh
    /// rebase before the checks are read, red or green, once per base head;
    /// a conflict still comes first.
    #[test]
    fn a_base_changing_an_overlap_path_is_a_refresh_rebase() {
        let moved = Followed {
            overlap: Some("b3"),
            ..followed()
        };
        let refresh = Action::Rebase {
            onto: "b3".to_owned(),
            cause: RebaseCause::Overlap,
        };
        for checks in [
            &[("ci", Passed)][..],
            &[("ci", CodeFailure)],
            &[("ci", Pending)],
            &[],
        ] {
            let r = read(Mergeable::Mergeable, checks);
            assert_eq!(
                decide(&r, moved, None, None),
                Decision::Follow {
                    adopt: false,
                    action: refresh.clone()
                },
                "{checks:?}"
            );
        }
        assert!(RebaseCause::Overlap.refresh() && !RebaseCause::Conflict.refresh());
        let conflicting = read(Mergeable::Conflicting, &[("ci", Passed)]);
        assert_eq!(
            decide(&conflicting, moved, None, None),
            Decision::Follow {
                adopt: false,
                action: Action::NeedsBase
            }
        );
        let used = Followed {
            rebased_onto: Some("b3"),
            ..moved
        };
        assert_eq!(
            decide(
                &read(Mergeable::Mergeable, &[("ci", Passed)]),
                used,
                None,
                None
            ),
            Decision::Follow {
                adopt: false,
                action: Action::Green
            }
        );
    }

    #[test]
    fn a_conflict_is_a_rebase_unless_onto_the_head_already_used() {
        let r = read(Mergeable::Conflicting, &[("ci", CodeFailure)]);
        assert_eq!(action(&r, None, None), Action::NeedsBase);
        let b = base("b2", &[], None);
        assert_eq!(
            action(&r, Some(&b), None),
            Action::Rebase {
                onto: "b2".to_owned(),
                cause: RebaseCause::Conflict
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
                cause: RebaseCause::BaseFixed
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

    /// A job of workflow run `run`, check run `id` (OWL-513).
    fn job(name: &str, outcome: CheckOutcome, id: u64, run: u64) -> CheckRead {
        CheckRead {
            name: name.to_owned(),
            outcome,
            id: Some(id),
            run: Some(run),
            rerunnable: true,
        }
    }

    fn first(name: &str, id: u64, run: u64) -> FirstAttempt {
        FirstAttempt {
            id,
            run,
            name: name.to_owned(),
        }
    }

    fn jobs(checks: Vec<CheckRead>) -> Read {
        Read {
            state: PrState::Open,
            head: PUSHED.to_owned(),
            mergeable: Mergeable::Mergeable,
            checks,
        }
    }

    /// The read's action with `mark`, re-run of `head`, `since` ago, and a
    /// base where every check passes.
    fn rerun_action(read: &Read, head: &str, first: &[FirstAttempt], since: Duration) -> Action {
        let mark = RerunMark { head, first, since };
        let followed = Followed {
            rerun: Some(mark),
            ..followed()
        };
        let base = base("b", &[], None);
        match decide(read, followed, Some(&base), None) {
            Decision::Follow { action, .. } => action,
            other => panic!("{other:?}"),
        }
    }

    const SOON: Duration = Duration::from_secs(60);

    /// A red of Actions jobs alone is re-run, once per head: a mark of this
    /// head gives the fix, one of another head the re-run again.
    #[test]
    fn a_red_of_actions_jobs_is_rerun_once_per_head() {
        let r = jobs(vec![
            job("check (ubuntu-latest)", CodeFailure, 10, 7),
            job("check (macos-latest)", Passed, 11, 7),
            job("ci", CodeFailure, 12, 7),
        ]);
        let b = base("b", &[], None);
        assert_eq!(action(&r, Some(&b), None), Action::Rerun);
        // Refused for this head: the checks read as they are.
        assert_eq!(
            rerun_action(&r, PUSHED, &[], SOON),
            Action::Fix { overdue: vec![] }
        );
        assert_eq!(rerun_action(&r, "older", &[], SOON), Action::Rerun);
        // Re-run on this head, its newer attempts failed again.
        let again = jobs(vec![
            job("check (ubuntu-latest)", CodeFailure, 20, 7),
            job("check (macos-latest)", Passed, 11, 7),
            job("ci", CodeFailure, 21, 7),
        ]);
        let ran = [first("check (ubuntu-latest)", 10, 7), first("ci", 12, 7)];
        assert_eq!(
            rerun_action(&again, PUSHED, &ran, SOON),
            Action::Fix { overdue: vec![] }
        );
        assert_eq!(rerun_action(&again, "older", &ran, SOON), Action::Rerun);
    }

    /// The base rows come first: a red the base shares waits, one the base
    /// has fixed since is a refresh, re-runnable jobs or not.
    #[test]
    fn the_base_rows_come_before_the_rerun() {
        let r = jobs(vec![job("ci", CodeFailure, 10, 7)]);
        let red = base("b", &[("ci", CodeFailure)], None);
        assert_eq!(action(&r, Some(&red), None), Action::WaitBaseRed);
        let fixed = base("b2", &[("ci", Passed)], Some(&[("ci", CodeFailure)]));
        assert_eq!(
            action(&r, Some(&fixed), None),
            Action::Rebase {
                onto: "b2".to_owned(),
                cause: RebaseCause::BaseFixed
            }
        );
    }

    /// Only finished code failures of re-runnable jobs are re-run.
    #[test]
    fn only_finished_code_failures_of_actions_jobs_are_rerun() {
        let b = base("b", &[], None);
        let mut cancelled = job("ci", OtherFailure, 10, 7);
        let other = jobs(vec![cancelled.clone()]);
        assert_eq!(action(&other, Some(&b), None), Action::Park);
        let mixed = jobs(vec![cancelled.clone(), job("t", CodeFailure, 11, 7)]);
        assert_eq!(
            action(&mixed, Some(&b), None),
            Action::Fix { overdue: vec![] }
        );
        // A status, or a job whose run was not read, beside a re-runnable one.
        cancelled.outcome = CodeFailure;
        cancelled.rerunnable = false;
        let status = jobs(vec![cancelled, job("t", CodeFailure, 11, 7)]);
        assert_eq!(
            action(&status, Some(&b), None),
            Action::Fix { overdue: vec![] }
        );
        // A check still running an hour after the failure: the fix.
        let overdue = jobs(vec![
            job("t", CodeFailure, 11, 7),
            job("win", Pending, 12, 7),
        ]);
        assert_eq!(action(&overdue, Some(&b), None), Action::WaitChecks);
        assert_eq!(
            action(&overdue, Some(&b), Some(OVERDUE_AFTER)),
            Action::Fix {
                overdue: vec!["win".to_owned()]
            }
        );
        // Two workflows' jobs of one name: the failing one is not re-runnable,
        // whatever the passing one listed first is.
        let mut status_ci = check("ci", CodeFailure);
        status_ci.run = None;
        let named = jobs(vec![job("ci", Passed, 9, 6), status_ci]);
        assert_eq!(
            action(&named, Some(&b), None),
            Action::Fix { overdue: vec![] }
        );
    }

    /// A re-run job's first attempt waits for its newer one, in both
    /// listings: the first attempt still listed, or the newest alone.
    #[test]
    fn first_attempts_wait_for_the_rerun_then_give_way() {
        let ran = [first("ci", 10, 7)];
        let at = |checks: Vec<CheckRead>, since| rerun_action(&jobs(checks), PUSHED, &ran, since);
        assert_eq!(
            at(vec![job("ci", CodeFailure, 10, 7)], SOON),
            Action::WaitChecks
        );
        assert_eq!(at(vec![job("ci", Passed, 11, 7)], SOON), Action::Green);
        assert_eq!(
            at(
                vec![job("ci", CodeFailure, 10, 7), job("ci", Passed, 11, 7)],
                SOON
            ),
            Action::Green
        );
        assert_eq!(
            at(vec![job("ci", Pending, 11, 7)], SOON),
            Action::WaitChecks
        );
        assert_eq!(
            at(vec![job("ci", CodeFailure, 11, 7)], SOON),
            Action::Fix { overdue: vec![] }
        );
        assert_eq!(
            at(vec![job("ci", CodeFailure, 10, 7)], OVERDUE_AFTER),
            Action::Fix { overdue: vec![] }
        );
    }

    /// A re-run job the read does not list yet, such as an aggregate `ci`
    /// whose legs run again, keeps the read waiting: never green on the
    /// legs alone.
    #[test]
    fn a_rerun_job_not_listed_yet_keeps_the_read_waiting() {
        let ran = [first("check (ubuntu-latest)", 10, 7), first("ci", 12, 7)];
        let legs = vec![
            job("check (ubuntu-latest)", Passed, 20, 7),
            job("check (macos-latest)", Passed, 11, 7),
        ];
        assert_eq!(
            rerun_action(&jobs(legs.clone()), PUSHED, &ran, SOON),
            Action::WaitChecks
        );
        assert_eq!(
            rerun_action(&jobs(legs.clone()), PUSHED, &ran, OVERDUE_AFTER),
            Action::Fix { overdue: vec![] }
        );
        let mut done = legs;
        done.push(job("ci", Passed, 21, 7));
        assert_eq!(rerun_action(&jobs(done), PUSHED, &ran, SOON), Action::Green);
    }

    /// Only a check of the same name, the same workflow run and a greater id
    /// is a newer attempt.
    #[test]
    fn a_same_named_check_of_another_run_does_not_replace_the_first_attempt() {
        let ran = [first("ci", 10, 7)];
        let failed = job("ci", CodeFailure, 10, 7);
        let mut no_run = job("ci", Passed, 11, 7);
        no_run.run = None;
        for other in [job("ci", Passed, 11, 8), job("ci", Passed, 9, 7), no_run] {
            let r = jobs(vec![failed.clone(), other.clone()]);
            assert_eq!(
                rerun_action(&r, PUSHED, &ran, SOON),
                Action::WaitChecks,
                "{other:?}"
            );
            assert_eq!(newer_attempts(&r.checks, &ran[0]).count(), 0, "{other:?}");
        }
    }

    /// A refused re-run keeps no first attempt: the checks read as they are.
    #[test]
    fn a_refused_rerun_reads_the_checks_as_they_are() {
        let r = jobs(vec![job("ci", CodeFailure, 10, 7)]);
        assert_eq!(
            rerun_action(&r, PUSHED, &[], SOON),
            Action::Fix { overdue: vec![] }
        );
        let mark = RerunMark {
            head: PUSHED,
            first: &[],
            since: OVERDUE_AFTER,
        };
        assert_eq!(rerun_outcome(&r, &mark), None);
    }

    #[test]
    fn rerun_outcome_reads_passed_failed_or_unseen() {
        let ran = [first("check", 10, 7), first("ci", 12, 7)];
        let outcome = |checks: Vec<CheckRead>, since| {
            let mark = RerunMark {
                head: PUSHED,
                first: &ran,
                since,
            };
            rerun_outcome(&jobs(checks), &mark)
        };
        let pending = vec![job("check", Pending, 20, 7), job("ci", Passed, 21, 7)];
        assert_eq!(outcome(pending, SOON), None);
        let passed = vec![job("check", Passed, 20, 7), job("ci", Passed, 21, 7)];
        assert_eq!(outcome(passed.clone(), SOON), Some(RerunOutcome::Passed));
        let failed = vec![job("check", CodeFailure, 20, 7), job("ci", Passed, 21, 7)];
        assert_eq!(outcome(failed, SOON), Some(RerunOutcome::Failed));
        // `ci` with no newer attempt: none within the hour, unseen after.
        let first_only = vec![job("check", Passed, 20, 7), job("ci", CodeFailure, 12, 7)];
        assert_eq!(outcome(first_only.clone(), SOON), None);
        assert_eq!(
            outcome(first_only, OVERDUE_AFTER),
            Some(RerunOutcome::Unseen)
        );
        // Another run's `ci` passing is no newer attempt.
        let other_run = vec![job("check", Passed, 20, 7), job("ci", Passed, 21, 8)];
        assert_eq!(outcome(other_run.clone(), SOON), None);
        assert_eq!(
            outcome(other_run, OVERDUE_AFTER),
            Some(RerunOutcome::Unseen)
        );
        // `ci` neither listed nor re-attempted.
        assert_eq!(outcome(vec![job("check", Passed, 20, 7)], SOON), None);
        // Another head's read says nothing of this re-run.
        let mut moved = jobs(passed);
        moved.head = "n".to_owned();
        let mark = RerunMark {
            head: PUSHED,
            first: &ran,
            since: SOON,
        };
        assert_eq!(rerun_outcome(&moved, &mark), None);
    }

    #[test]
    fn replaced_attempts_flags_only_the_same_jobs_older_attempt() {
        let ran = [first("ci", 10, 7), first("t", 30, 7)];
        let mark = RerunMark {
            head: PUSHED,
            first: &ran,
            since: SOON,
        };
        let checks = [
            job("ci", CodeFailure, 10, 7),
            job("ci", CodeFailure, 11, 7),
            job("ci", CodeFailure, 5, 8),
            // A first attempt with no newer one stays.
            job("t", CodeFailure, 30, 7),
        ];
        assert_eq!(
            replaced_attempts(&checks, &mark),
            [true, false, false, false]
        );
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

        fn question(category: &str, settled: Settled) -> MergeQuestion {
            MergeQuestion {
                id: "Q1".to_owned(),
                round: Some(1),
                category: category.to_owned(),
                labelled: None,
                settled,
            }
        }

        /// Every condition met: `labeled` with the label, a minute between
        /// the green reads.
        fn facts<'a>(decision: &'a Decision, questions: &'a [MergeQuestion]) -> MergeFacts<'a> {
            MergeFacts {
                decision,
                pushed: HEAD,
                first_green: Some(FirstGreen {
                    head: HEAD,
                    ago: GREEN_TWICE_AFTER,
                }),
                base_head: BASE,
                base_at_push: Some(BASE),
                disjoint: false,
                labeled: true,
                excluded: false,
                questions,
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
            let asked = [question("naming", Settled::Open)];
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
            let human = [question("Possible security issue", Settled::Decider)];
            let added = [question("billing_change", Settled::Open)];
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
                    Security(human[0].category.clone()),
                ),
                (
                    MergeSwitch::All,
                    facts(&GREEN, &added),
                    green_read(),
                    Unanswered(added[0].category.clone()),
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

        /// OWL-520: a base that moved since the push merges with no refresh
        /// once read as leaving the delivery's paths alone, under every other
        /// condition, two green reads included; unread, or read otherwise,
        /// the move is a refresh. A base that did not move, or one not known,
        /// ignores the reading.
        #[test]
        fn a_moved_base_that_leaves_the_delivery_alone_merges_without_a_refresh() {
            use MergeBlocker::*;
            let gate = gate(DelegationLevel::Discoverable);
            let at_push = "fedcba9876543210fedcba9876543210fedcba98";
            let wait = |facts: &MergeFacts<'_>| due(MergeSwitch::All, &gate, facts, &green_read());
            let moved = MergeFacts {
                base_at_push: Some(at_push),
                ..facts(&GREEN, &[])
            };
            assert_eq!(wait(&moved), MergeDue::Wait(BaseMoved));
            let apart = MergeFacts {
                disjoint: true,
                ..moved
            };
            assert_eq!(
                wait(&apart),
                MergeDue::Merge(MergeGrant::new(HEAD, MergeMethod::Rebase))
            );
            let cases = [
                (
                    MergeFacts {
                        first_green: None,
                        ..apart
                    },
                    GreenOnce,
                ),
                (
                    MergeFacts {
                        something_due: true,
                        ..apart
                    },
                    SomethingDue,
                ),
                (
                    MergeFacts {
                        base_at_push: None,
                        ..apart
                    },
                    BaseUnknown,
                ),
            ];
            for (facts, blocker) in cases {
                assert_eq!(wait(&facts), MergeDue::Wait(blocker.clone()), "{blocker:?}");
            }
            let unmoved = MergeFacts {
                disjoint: true,
                ..facts(&GREEN, &[])
            };
            assert!(matches!(wait(&unmoved), MergeDue::Merge(_)));
        }

        /// OWL-520: a conflict with a moved base is a rebase run, cause
        /// `conflict`, never a merge, whatever the reading of the move.
        #[test]
        fn a_conflict_with_a_moved_base_is_a_rebase_never_a_merge() {
            let gate = gate(DelegationLevel::Discoverable);
            let conflicting = Read {
                mergeable: Mergeable::Conflicting,
                ..green_read()
            };
            let pushed = Followed {
                pushed: HEAD,
                pushing: None,
                rebased_onto: None,
                overlap: None,
                rerun: None,
            };
            let base = BaseRead {
                head: BASE.to_owned(),
                at_head: Vec::new(),
                at_push: None,
            };
            let decision = decide(&conflicting, pushed, Some(&base), None);
            assert_eq!(
                decision,
                Decision::Follow {
                    adopt: false,
                    action: Action::Rebase {
                        onto: BASE.to_owned(),
                        cause: RebaseCause::Conflict,
                    },
                }
            );
            let moved = MergeFacts {
                base_at_push: Some("fedcba9876543210fedcba9876543210fedcba98"),
                disjoint: true,
                ..facts(&decision, &[])
            };
            assert_eq!(
                due(MergeSwitch::All, &gate, &moved, &conflicting),
                MergeDue::Wait(MergeBlocker::NotGreen)
            );
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
        /// none required; OWL-520: on the base head `merge_due` decided on,
        /// a moved one included.
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
            let decided = "fedcba9876543210fedcba9876543210fedcba98";
            let grant = MergeGrant::new(HEAD, MergeMethod::Squash);
            let moved = LastRead {
                base_head: decided,
                ..last()
            };
            assert_eq!(
                confirm_merge(grant.clone(), "main", decided, &moved),
                MergeDue::Merge(grant)
            );
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

        /// A question its decider did not answer blocks when the project
        /// sends its category to a human: the level's set, the project's
        /// additions, and every one at `none`; once answered it does not.
        #[test]
        fn a_human_category_follows_the_projects_level() {
            let blocks = |level, category: &str, settled| {
                let questions = [question(category, settled)];
                matches!(
                    due(
                        MergeSwitch::All,
                        &gate(level),
                        &facts(&GREEN, &questions),
                        &green_read()
                    ),
                    MergeDue::Wait(MergeBlocker::Unanswered(_))
                )
            };
            let open = Settled::Open;
            assert!(blocks(DelegationLevel::Discoverable, "scope change", open));
            assert!(!blocks(DelegationLevel::Ticket, "scope change", open));
            assert!(!blocks(DelegationLevel::All, "testing", open));
            assert!(!blocks(DelegationLevel::Discoverable, "", open));
            assert!(blocks(DelegationLevel::None, "testing", open));
            assert!(blocks(DelegationLevel::None, "", open));
            // At `none` every category is a person's, and their answer lifts it.
            assert!(!blocks(DelegationLevel::None, "testing", Settled::Decider));
            assert!(!blocks(DelegationLevel::None, "", Settled::Decider));
            // Whole-word tokens: `scoped` names no category.
            assert!(!blocks(DelegationLevel::Discoverable, "scoped", open));
        }

        /// Who settled a question under a human category decides whether it
        /// holds (OWL-465): `security` whatever the answer; the floor's
        /// `irreversible`, a category the level keeps and one the project
        /// added until their decider answers them; a category the resolver
        /// may decide never.
        #[test]
        fn who_settled_a_human_question_decides_whether_it_holds() {
            use MergeBlocker::*;
            use Settled::*;
            let gate = gate(DelegationLevel::Discoverable);
            let held = |category: &str| {
                [
                    Some(Security(category.to_owned())),
                    Some(Security(category.to_owned())),
                    Some(Security(category.to_owned())),
                ]
            };
            let lifted_once_answered = |category: &str| {
                [
                    None,
                    Some(ResolverDecided(category.to_owned())),
                    Some(Unanswered(category.to_owned())),
                ]
            };
            let cases = [
                ("security", held("security")),
                ("irreversible", lifted_once_answered("irreversible")),
                ("money", lifted_once_answered("money")),
                ("billing", lifted_once_answered("billing")),
                ("naming", [None, None, None]),
            ];
            for (category, expected) in cases {
                for (settled, expected) in [Decider, Resolver, Open].into_iter().zip(expected) {
                    let questions = [question(category, settled)];
                    let got = due(
                        MergeSwitch::All,
                        &gate,
                        &facts(&GREEN, &questions),
                        &green_read(),
                    );
                    let want = expected.map_or_else(
                        || MergeDue::Merge(MergeGrant::new(HEAD, MergeMethod::Rebase)),
                        MergeDue::Wait,
                    );
                    assert_eq!(got, want, "{category} {settled:?}");
                }
            }
            // LOC-1037: its `irreversible` question answered, a decision on
            // another topic: merged.
            let loc_1037 = [
                question("naming", Resolver),
                question("irreversible", Decider),
            ];
            assert!(matches!(
                due(
                    MergeSwitch::All,
                    &gate,
                    &facts(&GREEN, &loc_1037),
                    &green_read()
                ),
                MergeDue::Merge(_)
            ));
            // A `security` hold, which no answer lifts, is told first.
            let both = [question("money", Open), question("security", Decider)];
            assert_eq!(
                due(
                    MergeSwitch::All,
                    &gate,
                    &facts(&GREEN, &both),
                    &green_read()
                ),
                MergeDue::Wait(Security("security".to_owned()))
            );
            // With the question that holds, which names it to a person
            // (OWL-464).
            let held = held_question(&gate, &both).unwrap();
            assert!(std::ptr::eq(held.0, &both[1]), "{held:?}");
            assert_eq!(held.1.kept_for_a_person(), Some("security"));
            assert_eq!(MergeBlocker::GreenOnce.kept_for_a_person(), None);
        }

        /// OWL-482: a question asked under another category that the
        /// resolver labelled `security` holds whatever the answer, at every
        /// level; any other label of the resolver changes nothing.
        #[test]
        fn a_question_the_resolver_labelled_security_holds_whatever_the_answer() {
            use MergeBlocker::*;
            use Settled::*;
            let labelled = |category: &str, label: &str, settled| MergeQuestion {
                labelled: Some(label.to_owned()),
                ..question(category, settled)
            };
            let held = Some(Security("security".to_owned()));
            let cases = [
                ("naming", "security", held.clone()),
                ("naming", "auth_security", held.clone()),
                ("money", "security", held),
                // The asking run's own `security` category is the one told.
                (
                    "Security",
                    "security",
                    Some(Security("Security".to_owned())),
                ),
                ("naming", "irreversible", None),
                ("naming", "naming", None),
            ];
            for level in [DelegationLevel::None, DelegationLevel::All] {
                for (category, label, expected) in &cases {
                    for settled in [Decider, Open] {
                        let questions = [labelled(category, label, settled)];
                        let got = due(
                            MergeSwitch::All,
                            &gate(level),
                            &facts(&GREEN, &questions),
                            &green_read(),
                        );
                        let lifted = settled == Decider
                            || (level != DelegationLevel::None && *category == "naming");
                        let want = match expected {
                            Some(blocker) => MergeDue::Wait(blocker.clone()),
                            None if lifted => {
                                MergeDue::Merge(MergeGrant::new(HEAD, MergeMethod::Rebase))
                            }
                            None => MergeDue::Wait(Unanswered((*category).to_owned())),
                        };
                        assert_eq!(got, want, "{level:?} {category} {label} {settled:?}");
                    }
                }
            }
        }
    }
}
