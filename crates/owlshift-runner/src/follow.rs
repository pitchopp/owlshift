//! Following a delivered pull request (OWL-209, architecture section 5,
//! "Watch after delivery"): the forge's read of it mapped onto the core's
//! table ([`owlshift_core::follow::decide`]), what the read changes in the
//! ticket ref, and the events it records. `owlshift watch` looks at each
//! followed pull request once a pass and keeps [`Following`] from one pass
//! to the next; `owlshift continue` on a ticket at Watch looks once, with a
//! memory of its own.
//!
//! The forge is read without the project's lock; the caller writes the ref
//! a look changes ([`changed`]) under it, from the ref the look started from,
//! and records the look's events ([`Following::events`]) only once that write
//! is done, so a crash between the two loses an event, never the change.

use std::collections::HashMap;
use std::fmt;
use std::time::Duration;

use jiff::Timestamp;
use serde_json::json;

use owlshift_adapters::forge::github::GitHubForge;
use owlshift_adapters::forge::{
    BaseChecks, Branch, Check, CheckKind, CheckSet, CheckState, CommitId, ErrorKind, Mergeable,
    PrState, PullRequestRead, ReviewsRead, Verdict,
};
use owlshift_contracts::ids::TicketId;
use owlshift_contracts::refs::{
    DeliveredPullRequest, Ended, EndedWhy, OwedReport, PushKind, Pushing, TicketDelivery,
};
use owlshift_core::follow::{
    self, Action, BaseRead, CheckOutcome, CheckRead, Decision, EndReason, Followed, Read,
};
use owlshift_core::pipeline::Pipeline;
use owlshift_core::state::{Event, Status, TicketState, Transition};
use owlshift_core::vocab::Stage;

use crate::events::{Data, data};
use crate::ticket_ref::TicketRecord;

/// How long a rate-limited answer that gives no reset holds the forge reads.
pub const RATE_LIMIT_FALLBACK: Duration = Duration::from_secs(5 * 60);

/// The core's read of a pull request's check set.
pub fn read_of(set: &CheckSet) -> Read {
    Read {
        state: match set.state {
            PrState::Open => follow::PrState::Open,
            PrState::Closed => follow::PrState::Closed,
            PrState::Merged => follow::PrState::Merged,
        },
        head: set.head.to_string(),
        mergeable: match set.mergeable {
            Mergeable::Mergeable => follow::Mergeable::Mergeable,
            Mergeable::Conflicting => follow::Mergeable::Conflicting,
            Mergeable::Unknown => follow::Mergeable::Unknown,
        },
        checks: set.checks.iter().map(check_read).collect(),
    }
}

/// A check for the core: a code failure is a check run concluding `FAILURE`
/// or `TIMED_OUT`, or a commit status `FAILURE` or `ERROR`; any other failure
/// is one a person re-runs. Whether the check is required plays no part.
pub(crate) fn check_read(check: &Check) -> CheckRead {
    let outcome = match check.state {
        CheckState::Passed => CheckOutcome::Passed,
        CheckState::Pending => CheckOutcome::Pending,
        CheckState::Failed => {
            let code = match check.kind {
                CheckKind::CheckRun { .. } => matches!(check.raw.as_str(), "FAILURE" | "TIMED_OUT"),
                CheckKind::Status => matches!(check.raw.as_str(), "FAILURE" | "ERROR"),
            };
            if code {
                CheckOutcome::CodeFailure
            } else {
                CheckOutcome::OtherFailure
            }
        }
    };
    CheckRead {
        name: check.name.clone(),
        outcome,
    }
}

fn base_read_of(base: &BaseChecks) -> BaseRead {
    BaseRead {
        head: base.head.to_string(),
        at_head: base.at_head.iter().map(check_read).collect(),
        at_push: base
            .at_push
            .as_ref()
            .map(|checks| checks.iter().map(check_read).collect()),
    }
}

/// A pull request read and decided: its checks, the base's when the
/// table needed them, and the decision.
#[derive(Clone, Debug)]
pub struct Fresh {
    pub checks: CheckSet,
    pub base: Option<BaseChecks>,
    pub decision: Decision,
    /// How long ago a failure was first read on this head, as the decision
    /// took it.
    pub failing_for: Option<Duration>,
}

impl Fresh {
    /// What a person and the events read of it.
    pub fn seen(&self, pull_request: u64, url: &str) -> Seen {
        Seen {
            pull_request,
            url: url.to_owned(),
            head: self.checks.head.to_string(),
            verdict: self.checks.verdict(),
            mergeable: self.checks.mergeable,
            decision: self.decision.clone(),
            base_head: self.base.as_ref().map(|base| base.head.to_string()),
            failing_for: self.failing_for,
        }
    }
}

/// Reads pull request `number` of `delivery`, whose last push is `pushed`,
/// and decides on it by the core's table, reading the base branch when the
/// table needs it. `failing_for` gives how long ago a failure was first read
/// on the head read. Watch's look and a repair's read under the lock
/// (OWL-112) both read through it.
pub fn read(
    forge: &GitHubForge,
    delivery: &TicketDelivery,
    number: u64,
    pushed: &str,
    mut failing_for: impl FnMut(&Read) -> Option<Duration>,
) -> Result<Fresh, owlshift_adapters::forge::Error> {
    let PullRequestRead { checks, .. } = forge.follow(number)?;
    let core = read_of(&checks);
    let followed = followed_of(delivery, pushed);
    let failing_for = failing_for(&core);
    let mut decision = follow::decide(&core, followed, None, failing_for);
    let mut base = None;
    if matches!(
        decision,
        Decision::Follow {
            action: Action::NeedsBase,
            ..
        }
    ) {
        let at_push = delivery
            .base_at_push
            .as_deref()
            .and_then(|c| CommitId::new(c).ok());
        let read = Branch::new(&delivery.base_branch)
            .and_then(|branch| forge.base_checks(&branch, at_push.as_ref()))?;
        decision = follow::decide(&core, followed, Some(&base_read_of(&read)), failing_for);
        base = Some(read);
    }
    Ok(Fresh {
        checks,
        base,
        decision,
        failing_for,
    })
}

/// What the core's table follows of `delivery`, whose last push is
/// `pushed`.
fn followed_of<'d>(delivery: &'d TicketDelivery, pushed: &'d str) -> Followed<'d> {
    Followed {
        pushed,
        pushing: delivery.pushing.as_ref().map(|p| p.commit.as_str()),
        rebased_onto: delivery.rebased_onto.as_deref(),
    }
}

/// What a look at a followed pull request found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Look {
    /// The forge's budget is spent: no read before `until`.
    Paused { until: Timestamp },
    /// The forge could not be read.
    Unread { kind: &'static str, reason: String },
    /// The read and what it decides.
    Read(Seen),
}

/// A pull request as a look read it, and what that decides.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Seen {
    pub pull_request: u64,
    pub url: String,
    pub head: String,
    pub verdict: Verdict,
    pub mergeable: Mergeable,
    pub decision: Decision,
    /// The base head the decision was made against, when it was read.
    pub base_head: Option<String>,
    /// How long ago watch first read a failure on this head, when it had
    /// read one before: a repair decides again with it (OWL-112).
    pub failing_for: Option<Duration>,
}

impl Seen {
    /// What a person reads of it.
    pub fn describe(&self) -> String {
        match &self.decision {
            Decision::End(EndReason::Merged) => {
                "it was merged: watch no longer follows it".to_owned()
            }
            Decision::End(EndReason::Closed) => {
                "it was closed unmerged: watch no longer follows it".to_owned()
            }
            Decision::End(EndReason::HeadMoved) => format!(
                "someone else pushed to its branch ({}): watch no longer follows it",
                self.head
            ),
            Decision::Follow { action, .. } => describe(action).to_owned(),
        }
    }
}

fn describe(action: &Action) -> &'static str {
    match action {
        Action::WaitMergeability => "GitHub has not computed whether it merges yet",
        Action::NeedsBase => "its base branch is still to be read",
        Action::WaitChecks => "its checks are still running, or none has reported yet",
        Action::WaitBaseRed => "its failed checks fail on its base branch too: nothing to fix here",
        Action::Green => "every check passed",
        Action::Rebase { refresh: false, .. } => "it conflicts with its base: a rebase is due",
        Action::Rebase { refresh: true, .. } => {
            "its checks failed only on the base it was pushed onto and pass on the base now: a \
             rebase is due"
        }
        Action::RebaseFailed { .. } => {
            "it still conflicts with the base head its last rebase used: that rebase failed"
        }
        Action::Fix { .. } => "a check failed: a fix run is due",
        Action::Park => {
            "only checks a person re-runs failed: re-run them, then `owlshift continue`"
        }
    }
}

/// The name of an action in events.
fn action_name(decision: &Decision) -> &'static str {
    match decision {
        Decision::End(_) => "end",
        Decision::Follow { action, .. } => match action {
            Action::WaitMergeability => "wait_mergeability",
            Action::NeedsBase => "needs_base",
            Action::WaitChecks => "wait_checks",
            Action::WaitBaseRed => "wait_base_red",
            Action::Green => "green",
            Action::Rebase { refresh: false, .. } => "rebase",
            Action::Rebase { refresh: true, .. } => "refresh_rebase",
            Action::RebaseFailed { .. } => "rebase_failed",
            Action::Fix { .. } => "fix",
            Action::Park => "park",
        },
    }
}

fn verdict_name(verdict: Verdict) -> &'static str {
    match verdict {
        Verdict::Green => "green",
        Verdict::Pending => "pending",
        Verdict::Red => "red",
        Verdict::NoChecks => "no_checks",
        Verdict::NotOpen => "not_open",
    }
}

fn mergeable_name(mergeable: Mergeable) -> &'static str {
    match mergeable {
        Mergeable::Mergeable => "mergeable",
        Mergeable::Conflicting => "conflicting",
        Mergeable::Unknown => "unknown",
    }
}

fn end_name(why: EndedWhy) -> &'static str {
    match why {
        EndedWhy::Merged => "merged",
        EndedWhy::Closed => "closed",
        EndedWhy::HeadMoved => "head_moved",
    }
}

/// What the events last recorded of a pull request.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Recorded {
    head: String,
    verdict: Verdict,
    mergeable: Mergeable,
    action: &'static str,
}

/// What watch keeps of the pull requests it follows from one pass to the
/// next. Restarting watch forgets it: the first look of each pull request is
/// recorded again, the stage of a ticket a person moved is recorded again
/// (OWL-219), and the hour a failure beside a running check waits starts
/// again.
#[derive(Debug, Default)]
pub struct Following {
    /// No forge read before then: a rate-limited answer.
    paused_until: Option<Timestamp>,
    /// What the events last recorded, per ticket.
    recorded: HashMap<TicketId, Recorded>,
    /// When a failure was first read on the head, per ticket.
    failing_since: HashMap<TicketId, (String, Timestamp)>,
    /// The kind of the last forge failure warned of, per ticket.
    unread: HashMap<TicketId, &'static str>,
    /// The head whose overdue checks were warned of, per ticket.
    overdue: HashMap<TicketId, String>,
    /// The stage last recorded as moved out of Owlshift's states, per ticket
    /// (OWL-219).
    moved: HashMap<TicketId, String>,
    /// The commit and the refusal of the owed report last warned of, per
    /// ticket (OWL-232).
    unposted: HashMap<TicketId, (String, String)>,
    /// The kind of the last failure of a reviews read warned of, per ticket
    /// (OWL-307).
    reviews_unread: HashMap<TicketId, &'static str>,
}

/// A warning a look records, beside the events of what it read.
pub type Warning = Data;

impl Following {
    /// Reads the pull request `delivery` follows and decides on it at `now`.
    /// Returns the look, and the warnings to record now, each once: a spent
    /// budget once per pause, a forge failure once per kind until a read
    /// succeeds, overdue checks once per head.
    pub fn look(
        &mut self,
        forge: &GitHubForge,
        ticket: &TicketId,
        delivery: &TicketDelivery,
        now: Timestamp,
    ) -> (Look, Vec<Warning>) {
        let mut warnings = Vec::new();
        let (pr, pushed) = match self.readable(delivery, now) {
            Ok(readable) => readable,
            Err(look) => return (*look, warnings),
        };
        let failing = |core: &Read| self.failing_for(ticket, core, now);
        let fresh = match read(forge, delivery, pr.number, pushed, failing) {
            Ok(fresh) => fresh,
            Err(error) => return self.failed(ticket, &error, now, warnings),
        };
        let seen = fresh.seen(pr.number, &pr.url);
        self.unread.remove(ticket);
        if let Decision::Follow {
            action: Action::Fix { overdue },
            ..
        } = &seen.decision
            && !overdue.is_empty()
            && self.overdue.get(ticket) != Some(&seen.head)
        {
            self.overdue.insert(ticket.clone(), seen.head.clone());
            warnings.push(data([
                ("what", json!("checks_overdue")),
                ("pull_request", json!(pr.number)),
                ("head", json!(seen.head)),
                ("checks", json!(overdue)),
            ]));
        }
        (Look::Read(seen), warnings)
    }

    /// A look at the pull request of a ticket whose round waits at Watch
    /// (OWL-242), for whether the following ended: one read of it, never of
    /// its base, decided with no base and no failure clock, since the core
    /// decides an end before it needs either. Only the forge failure warned
    /// of is kept of the ticket, the rest forgotten as a waiting ticket's was
    /// before, so the read after the round's answer starts afresh. The
    /// decision of a pull request still open is no action's to take: it may
    /// want a base it was not given.
    pub fn look_ending(
        &mut self,
        forge: &GitHubForge,
        ticket: &TicketId,
        delivery: &TicketDelivery,
        now: Timestamp,
    ) -> (Look, Vec<Warning>) {
        let unread = self.unread.get(ticket).copied();
        self.forget(ticket);
        if let Some(kind) = unread {
            self.unread.insert(ticket.clone(), kind);
        }
        let warnings = Vec::new();
        let (pr, pushed) = match self.readable(delivery, now) {
            Ok(readable) => readable,
            Err(look) => return (*look, warnings),
        };
        let checks = match forge.follow(pr.number) {
            Ok(PullRequestRead { checks, .. }) => checks,
            Err(error) => return self.failed(ticket, &error, now, warnings),
        };
        let decision = follow::decide(&read_of(&checks), followed_of(delivery, pushed), None, None);
        let fresh = Fresh {
            checks,
            base: None,
            decision,
            failing_for: None,
        };
        self.unread.remove(ticket);
        (Look::Read(fresh.seen(pr.number, &pr.url)), warnings)
    }

    /// The pull request and the pushed commit to read, or the look without
    /// a read: the forge's budget spent, or a delivery with nothing to read.
    fn readable<'d>(
        &mut self,
        delivery: &'d TicketDelivery,
        now: Timestamp,
    ) -> Result<(&'d DeliveredPullRequest, &'d str), Box<Look>> {
        if let Some(until) = self.paused_until {
            if now < until {
                return Err(Box::new(Look::Paused { until }));
            }
            self.paused_until = None;
        }
        match (&delivery.pull_request, &delivery.pushed) {
            (Some(pr), Some(pushed)) => Ok((pr, pushed)),
            _ => Err(Box::new(Look::Unread {
                kind: "not_followed",
                reason: "its delivery has no pull request or pushed commit".to_owned(),
            })),
        }
    }

    /// How long ago a failure was first read on `read`'s head; a read with
    /// none forgets it.
    fn failing_for(&mut self, ticket: &TicketId, read: &Read, now: Timestamp) -> Option<Duration> {
        if !follow::has_failure(read) {
            self.failing_since.remove(ticket);
            return None;
        }
        match self.failing_since.get(ticket) {
            Some((head, since)) if *head == read.head => {
                Duration::try_from(now.duration_since(*since)).ok()
            }
            _ => {
                self.failing_since
                    .insert(ticket.clone(), (read.head.clone(), now));
                None
            }
        }
    }

    fn failed(
        &mut self,
        ticket: &TicketId,
        error: &owlshift_adapters::forge::Error,
        now: Timestamp,
        mut warnings: Vec<Warning>,
    ) -> (Look, Vec<Warning>) {
        if error.kind == ErrorKind::RateLimited {
            let until = error.resets_at.filter(|at| *at > now).unwrap_or_else(|| {
                now.checked_add(RATE_LIMIT_FALLBACK)
                    .unwrap_or(Timestamp::MAX)
            });
            self.paused_until = Some(until);
            warnings.push(data([
                ("what", json!("forge_rate_limited")),
                ("until", json!(until)),
                ("reason", json!(error.message)),
            ]));
            return (Look::Paused { until }, warnings);
        }
        let kind = unread_kind(error.kind);
        if self.unread.get(ticket) != Some(&kind) {
            self.unread.insert(ticket.clone(), kind);
            warnings.push(data([
                ("what", json!("watch_forge_unreadable")),
                ("kind", json!(kind)),
                ("reason", json!(error.message)),
            ]));
        }
        let look = Look::Unread {
            kind,
            reason: error.message.clone(),
        };
        (look, warnings)
    }

    /// The `decision` events of `seen`, once its change is kept: one when
    /// what the events last recorded of the pull request changed, or when
    /// the following ends or adopts a push; none otherwise. `watch` names
    /// the first change among `ended`, `adopted`, `mergeable`, `verdict` and
    /// `action`, and `read` the first look in this memory.
    pub fn events(&mut self, ticket: &TicketId, seen: &Seen) -> Option<Data> {
        let now = Recorded {
            head: seen.head.clone(),
            verdict: seen.verdict,
            mergeable: seen.mergeable,
            action: action_name(&seen.decision),
        };
        let previous = self.recorded.get(ticket);
        let what = match (&seen.decision, previous) {
            (Decision::End(_), _) => "ended",
            (Decision::Follow { adopt: true, .. }, _) => "adopted",
            (_, None) => "read",
            (_, Some(previous)) if *previous == now => return None,
            (_, Some(previous)) if previous.mergeable != now.mergeable => "mergeable",
            (_, Some(previous)) if previous.verdict != now.verdict => "verdict",
            (_, Some(_)) => "action",
        };
        let mut fields = vec![
            ("watch", json!(what)),
            ("pull_request", json!(seen.pull_request)),
            ("head", json!(seen.head)),
            ("verdict", json!(verdict_name(seen.verdict))),
            ("mergeable", json!(mergeable_name(seen.mergeable))),
            ("action", json!(now.action)),
        ];
        if let Decision::End(reason) = seen.decision {
            fields.push(("reason", json!(end_name(ended_why(reason)))));
            self.forget(ticket);
        } else {
            self.recorded.insert(ticket.clone(), now);
        }
        if let Some(base) = &seen.base_head {
            fields.push(("base", json!(base)));
        }
        Some(
            fields
                .into_iter()
                .map(|(key, value)| (key.to_owned(), value))
                .collect(),
        )
    }

    /// Whether the move of `ticket` to `stage`, a stage Owlshift does not work
    /// in, is to be recorded: it is when the last stage recorded for the
    /// ticket differs, or none was (OWL-219). Keeps `stage` as recorded.
    pub fn stage_moved(&mut self, ticket: &TicketId, stage: &str) -> bool {
        if self.moved.get(ticket).is_some_and(|last| last == stage) {
            return false;
        }
        self.moved.insert(ticket.clone(), stage.to_owned());
        true
    }

    /// The ticket was read in a stage Owlshift works in: a later move out of
    /// it is recorded, even to the stage it was last recorded at.
    pub fn stage_back(&mut self, ticket: &TicketId) {
        self.moved.remove(ticket);
    }

    /// Whether the refusal of the report owed for `commit` is to be warned
    /// of: it is the first one, or `reason` differs from the one last warned
    /// of for the ticket, or the report is another commit's (OWL-232). Keeps
    /// them as warned of.
    pub fn report_unposted(&mut self, ticket: &TicketId, commit: &str, reason: &str) -> bool {
        let warned = (commit.to_owned(), reason.to_owned());
        if self.unposted.get(ticket) == Some(&warned) {
            return false;
        }
        self.unposted.insert(ticket.clone(), warned);
        true
    }

    /// The report owed was posted or dropped: a later refusal is warned of
    /// again (OWL-232).
    pub fn report_settled(&mut self, ticket: &TicketId) {
        self.unposted.remove(ticket);
    }

    /// Forgets everything kept of `ticket`.
    pub fn forget(&mut self, ticket: &TicketId) {
        self.recorded.remove(ticket);
        self.failing_since.remove(ticket);
        self.unread.remove(ticket);
        self.overdue.remove(ticket);
        self.moved.remove(ticket);
        self.unposted.remove(ticket);
        self.reviews_unread.remove(ticket);
    }

    /// Forgets the tickets `kept` does not name.
    pub fn retain(&mut self, kept: &[TicketId]) {
        self.recorded.retain(|t, _| kept.contains(t));
        self.failing_since.retain(|t, _| kept.contains(t));
        self.unread.retain(|t, _| kept.contains(t));
        self.overdue.retain(|t, _| kept.contains(t));
        self.moved.retain(|t, _| kept.contains(t));
        self.unposted.retain(|t, _| kept.contains(t));
        self.reviews_unread.retain(|t, _| kept.contains(t));
    }

    /// Reads the reviews of pull request `number` at `now` (OWL-307), one
    /// request, at a pass whose read of it asked for no repair. Returns the
    /// read, or none, and the warnings to record now: a spent budget pauses
    /// every forge read, as a look's does; any other failure is a
    /// `reviews_unreadable` warning, once per kind until a read succeeds.
    pub fn reviews(
        &mut self,
        forge: &GitHubForge,
        ticket: &TicketId,
        number: u64,
        now: Timestamp,
    ) -> (Option<ReviewsRead>, Vec<Warning>) {
        let mut warnings = Vec::new();
        if self.paused_until.is_some_and(|until| now < until) {
            return (None, warnings);
        }
        match forge.reviews(number) {
            Ok(read) => {
                self.reviews_unread.remove(ticket);
                (Some(read), warnings)
            }
            Err(error) if error.kind == ErrorKind::RateLimited => {
                let (_, warned) = self.failed(ticket, &error, now, warnings);
                (None, warned)
            }
            Err(error) => {
                let kind = unread_kind(error.kind);
                if self.reviews_unread.get(ticket) != Some(&kind) {
                    self.reviews_unread.insert(ticket.clone(), kind);
                    warnings.push(data([
                        ("what", json!("reviews_unreadable")),
                        ("kind", json!(kind)),
                        ("reason", json!(error.message)),
                    ]));
                }
                (None, warnings)
            }
        }
    }
}

/// A forge failure's kind, as warnings name it.
fn unread_kind(kind: ErrorKind) -> &'static str {
    match kind {
        ErrorKind::NotFound => "not_found",
        // A token that can no longer be refreshed is a refused one, in the
        // warnings' fixed set (OWL-384).
        ErrorKind::Unauthorized | ErrorKind::SignInAgain => "unauthorized",
        ErrorKind::HeadMoved => "head_moved",
        ErrorKind::RateLimited => "rate_limited",
        ErrorKind::Other => "other",
    }
}

fn ended_why(reason: EndReason) -> EndedWhy {
    match reason {
        EndReason::Merged => EndedWhy::Merged,
        EndReason::Closed => EndedWhy::Closed,
        EndReason::HeadMoved => EndedWhy::HeadMoved,
    }
}

/// What `delivery` keeps once Owlshift's push under way shows as the head,
/// whether its command saw it land or a later read adopted it: the pushed
/// commit, the base head read before the push, and for a rebase run's push
/// the base it rebased onto (OWL-210), which a conflict read against it
/// makes a failed run of that rebase.
pub fn landed(delivery: &mut TicketDelivery, pushing: Pushing) {
    delivery.pushed = Some(pushing.commit);
    delivery.base_at_push = Some(pushing.base);
    if let Some(onto) = pushing.onto {
        delivery.rebased_onto = Some(onto);
    }
}

/// [`landed`] for a fix, rebase or correction run's push, the `count`th of
/// its kind (from 1; for a correction, how many decisions it follows the
/// replies to), whose DELIVERY report is now owed until a pass posts it
/// (OWL-218). Returns the owed report it replaces, which is never posted.
pub fn landed_owing(
    delivery: &mut TicketDelivery,
    pushing: Pushing,
    count: u32,
) -> Option<OwedReport> {
    let owed = OwedReport {
        kind: pushing.kind,
        count: count.max(1),
        report: pushing.report.clone(),
        unchanged: false,
    };
    landed(delivery, pushing);
    delivery.owed.replace(owed)
}

/// Whether an end of the following brings a ticket in `status`, whose
/// delivery watch follows, back to Watch with the core's
/// [`Event::FollowingEnded`]: a round waiting at Watch (OWL-242), a park at
/// Watch (OWL-243), or a review loop at Build or Verify, working, waiting or
/// parked there (OWL-307), since only a review loop takes a followed
/// delivery's ticket there.
pub fn ends_with_following(status: Status) -> bool {
    matches!(
        status,
        Status::NeedsInput {
            return_to: Stage::Watch | Stage::Build
        } | Status::Parked {
            at: Stage::Watch | Stage::Build | Stage::Verify,
            ..
        } | Status::Active(Stage::Build | Stage::Verify)
    )
}

/// What [`changed`] leaves in the ticket ref, and the owed report it
/// replaced: never posted, so the caller tells it with a
/// `{kind}_report_replaced` warning once the record is kept (OWL-239).
#[derive(Debug, PartialEq, Eq)]
pub struct Changed {
    /// The record to keep.
    pub record: TicketRecord,
    /// The report the record's owed report replaced, if one was owed.
    pub replaced: Option<OwedReport>,
}

/// The record `seen` leaves in the ticket ref, `None` when it changes
/// nothing: the following ended, closing a round that waited at Watch
/// (OWL-242), ending a park there (OWL-243) or ending a review loop
/// (OWL-307), a push adopted, a review loop's ending it, or the fix
/// passes and
/// failed runs cleared by a green read (`ChecksGreen`). A fix, a rebase or a
/// park is decided only: the repair runs them ([`OnDemand::repair_seen`]).
///
/// [`OnDemand::repair_seen`]: crate::on_demand::OnDemand::repair_seen
pub fn changed(
    record: &TicketRecord,
    pipeline: Pipeline,
    seen: &Seen,
    now: Timestamp,
) -> Result<Option<Changed>, String> {
    let Some(delivery) = &record.delivery else {
        return Ok(None);
    };
    let mut next = record.clone();
    let mut delivery = delivery.clone();
    let mut replaced = None;
    match &seen.decision {
        Decision::End(reason) => {
            // A round waiting at Watch, or a park there, ends with the
            // following (OWL-242, OWL-243), and so does a review loop
            // (OWL-307), its reviews never answered and its kept Build gone.
            let state = TicketState::try_from(&next.state).map_err(|e| e.to_string())?;
            if ends_with_following(state.status()) {
                match state.apply(pipeline, Event::FollowingEnded) {
                    Ok(Transition::To(closed)) => next.state = (&closed).into(),
                    other => return Err(format!("the core state machine: {other:?}")),
                }
                if state.stage() != Stage::Watch
                    && let Some(review) = next.review.as_mut()
                {
                    review.answering.clear();
                    review.build = None;
                }
            }
            let why = ended_why(*reason);
            delivery.pushing = None;
            delivery.ended = Some(Ended {
                why,
                at: now,
                head: (why == EndedWhy::HeadMoved).then(|| seen.head.clone()),
            });
        }
        Decision::Follow { adopt, action } => {
            if *adopt && let Some(pushing) = delivery.pushing.take() {
                let event = match pushing.kind {
                    PushKind::Fix => Some(Event::FixPushed),
                    PushKind::Rebase => Some(Event::RebasePushed),
                    PushKind::Correction => Some(Event::Corrected),
                    PushKind::Delivery | PushKind::Review => None,
                };
                match event {
                    // A review loop's push a crash cut short ends the loop
                    // as its command would have (OWL-307): from Verify to
                    // Watch, the reviews it answers answered, its report
                    // owed.
                    None if pushing.kind == PushKind::Review => {
                        let mut state =
                            TicketState::try_from(&next.state).map_err(|e| e.to_string())?;
                        while matches!(
                            state.status(),
                            Status::Active(Stage::Verify | Stage::Deliver)
                        ) {
                            state = match state.apply(pipeline, Event::Completed) {
                                Ok(Transition::To(completed)) => completed,
                                other => return Err(format!("the core state machine: {other:?}")),
                            };
                        }
                        next.state = (&state).into();
                        if let Some(review) = next.review.as_mut() {
                            let answered = pushing.report.iter().flat_map(|kept| &kept.reviews);
                            for answered in answered {
                                if !review.answered_reviews.contains(&answered.id) {
                                    review.answered_reviews.push(answered.id);
                                }
                            }
                            review.answering.clear();
                            review.build = None;
                        }
                        replaced = landed_owing(&mut delivery, pushing, 1);
                    }
                    None => landed(&mut delivery, pushing),
                    // A fix, rebase or correction push a crash cut short
                    // counts as the run would have (OWL-112, OWL-210,
                    // OWL-146), when the bounds still allow it: a stuck count
                    // never stops the following. Its report is owed, and the
                    // pass that keeps this posts it (OWL-218).
                    Some(event) => {
                        let state =
                            TicketState::try_from(&next.state).map_err(|e| e.to_string())?;
                        let state = match state.apply(pipeline, event) {
                            Ok(Transition::To(pushed)) => pushed,
                            _ => state,
                        };
                        next.state = (&state).into();
                        let count = match pushing.kind {
                            PushKind::Rebase => state.repairs(),
                            // A correction counts no fix pass: its report
                            // counts the decisions it follows the replies to.
                            PushKind::Correction => pushing
                                .report
                                .as_ref()
                                .map_or(1, |kept| kept.questions.len() as u32),
                            PushKind::Fix | PushKind::Delivery | PushKind::Review => {
                                state.fix_passes()
                            }
                        };
                        replaced = landed_owing(&mut delivery, pushing, count);
                    }
                }
            }
            if *action == Action::Green {
                let state = TicketState::try_from(&next.state).map_err(|e| e.to_string())?;
                if state.fix_passes() > 0 || state.failed_runs() > 0 {
                    match state.apply(pipeline, Event::ChecksGreen) {
                        Ok(Transition::To(green)) => next.state = (&green).into(),
                        other => return Err(format!("the core state machine: {other:?}")),
                    }
                }
            }
        }
    }
    next.delivery = Some(delivery);
    Ok((next != *record).then_some(Changed {
        record: next,
        replaced,
    }))
}

impl fmt::Display for Seen {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "pull request #{} {}: {}",
            self.pull_request,
            self.url,
            self.describe()
        )
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use owlshift_adapters::forge::Repo;
    use owlshift_adapters::graphql::{RateHeaders, Response, Transport};
    use owlshift_contracts::refs::TicketQuestions;
    use owlshift_contracts::refs::{DeliveredPullRequest, KeptReport, PersistedState, Pushing};
    use owlshift_contracts::review::TicketReview;
    use owlshift_core::state::Status;
    use owlshift_core::vocab::Stage;
    use serde_json::Value;

    use super::*;

    const PUSHED: &str = "1111111111111111111111111111111111111111";
    const PUSHING: &str = "2222222222222222222222222222222222222222";
    const BASE: &str = "3333333333333333333333333333333333333333";
    const ONTO: &str = "4444444444444444444444444444444444444444";

    /// Answers with prepared responses, in order, counting the requests.
    #[derive(Clone, Default)]
    struct Canned(Arc<Mutex<(Vec<Response>, usize)>>);

    impl Transport for Canned {
        fn send(&self, _: &Value) -> Result<Response, String> {
            let mut canned = self.0.lock().unwrap();
            canned.1 += 1;
            Ok(canned.0.remove(0))
        }
    }

    impl Canned {
        fn push(&self, response: Response) {
            self.0.lock().unwrap().0.push(response);
        }

        fn sent(&self) -> usize {
            self.0.lock().unwrap().1
        }
    }

    fn pr(state: &str, head: &str, mergeable: &str, checks: &[(&str, &str)]) -> Response {
        let nodes: Vec<Value> = checks
            .iter()
            .map(|(name, conclusion)| {
                json!({ "__typename": "CheckRun", "name": name, "status": "COMPLETED",
                        "conclusion": conclusion })
            })
            .collect();
        Response::new(
            200,
            json!({ "data": { "repository": { "pullRequest": {
                "state": state, "headRefOid": head, "mergeable": mergeable,
                "mergeStateStatus": "CLEAN",
                "commits": { "nodes": [{ "commit": { "oid": head, "statusCheckRollup": {
                    "contexts": { "totalCount": nodes.len(),
                        "pageInfo": { "hasNextPage": false, "endCursor": null },
                        "nodes": nodes } } } }] } } } } })
            .to_string(),
        )
    }

    fn base(checks: &[(&str, &str)]) -> Response {
        let nodes: Vec<Value> = checks
            .iter()
            .map(|(name, conclusion)| {
                json!({ "__typename": "CheckRun", "name": name, "status": "COMPLETED",
                        "conclusion": conclusion })
            })
            .collect();
        let rollup = json!({ "contexts": { "totalCount": nodes.len(),
            "pageInfo": { "hasNextPage": false, "endCursor": null }, "nodes": nodes } });
        Response::new(
            200,
            json!({ "data": { "repository": {
                "head": { "oid": BASE, "statusCheckRollup": rollup },
                "atPush": null } } })
            .to_string(),
        )
    }

    fn delivery() -> TicketDelivery {
        TicketDelivery {
            pull_request: Some(DeliveredPullRequest {
                number: 7,
                url: "https://github.com/demo/project/pull/7".to_owned(),
            }),
            pushed: Some(PUSHED.to_owned()),
            ..TicketDelivery::new("owlshift/demo-1", "main")
        }
    }

    fn forge() -> (GitHubForge, Canned) {
        let canned = Canned::default();
        let forge =
            GitHubForge::with_transport(canned.clone(), Repo::parse("demo/project").unwrap());
        (forge, canned)
    }

    fn at(minute: i64) -> Timestamp {
        Timestamp::from_second(1_790_000_000 + minute * 60).unwrap()
    }

    fn ticket() -> TicketId {
        TicketId::new("DEMO-1").unwrap()
    }

    fn seen(look: (Look, Vec<Warning>)) -> Seen {
        match look.0 {
            Look::Read(seen) => seen,
            other => panic!("{other:?}"),
        }
    }

    /// Red, then conflicting, then merged: one event each, none for a read
    /// that changes nothing, and nothing kept once it ended.
    #[test]
    fn each_change_is_one_event() {
        let (forge, canned) = forge();
        let mut following = Following::default();
        let d = delivery();
        canned.push(pr("OPEN", PUSHED, "MERGEABLE", &[("ci", "SUCCESS")]));
        let green = seen(following.look(&forge, &ticket(), &d, at(0)));
        assert_eq!(
            following.events(&ticket(), &green).unwrap()["watch"],
            "read"
        );
        canned.push(pr("OPEN", PUSHED, "MERGEABLE", &[("ci", "SUCCESS")]));
        let again = seen(following.look(&forge, &ticket(), &d, at(1)));
        assert_eq!(following.events(&ticket(), &again), None);

        canned.push(pr("OPEN", PUSHED, "MERGEABLE", &[("ci", "FAILURE")]));
        canned.push(base(&[("ci", "SUCCESS")]));
        let red = seen(following.look(&forge, &ticket(), &d, at(2)));
        let event = following.events(&ticket(), &red).unwrap();
        assert_eq!(
            (&event["watch"], &event["verdict"], &event["action"]),
            (&json!("verdict"), &json!("red"), &json!("fix"))
        );
        canned.push(pr("OPEN", PUSHED, "CONFLICTING", &[("ci", "FAILURE")]));
        canned.push(base(&[("ci", "SUCCESS")]));
        let conflicting = seen(following.look(&forge, &ticket(), &d, at(3)));
        let event = following.events(&ticket(), &conflicting).unwrap();
        assert_eq!(
            (&event["watch"], &event["action"]),
            (&json!("mergeable"), &json!("rebase"))
        );
        canned.push(pr("MERGED", PUSHED, "UNKNOWN", &[]));
        let merged = seen(following.look(&forge, &ticket(), &d, at(4)));
        let event = following.events(&ticket(), &merged).unwrap();
        assert_eq!(
            (&event["watch"], &event["reason"]),
            (&json!("ended"), &json!("merged"))
        );
        assert!(following.recorded.is_empty());
        assert_eq!(canned.sent(), 7);
    }

    /// A spent budget holds every read until its reset, warned of once; one
    /// with no reset holds for the fallback.
    #[test]
    fn a_spent_budget_holds_the_reads() {
        let (forge, canned) = forge();
        let mut following = Following::default();
        let limited = |rate| Response {
            status: 403,
            body: r#"{"message":"API rate limit exceeded"}"#.to_owned(),
            rate,
        };
        canned.push(limited(RateHeaders {
            remaining: Some(0),
            reset: Some(at(30).as_second()),
            retry_after: None,
        }));
        let (look, warnings) = following.look(&forge, &ticket(), &delivery(), at(0));
        assert_eq!(look, Look::Paused { until: at(30) });
        assert_eq!(warnings[0]["what"], "forge_rate_limited");
        let (look, warnings) = following.look(&forge, &ticket(), &delivery(), at(29));
        assert_eq!(look, Look::Paused { until: at(30) });
        assert!(warnings.is_empty());
        assert_eq!(canned.sent(), 1);

        canned.push(Response::new(
            200,
            r#"{"data":null,"errors":[{"type":"RATE_LIMITED","message":"spent"}]}"#,
        ));
        let (look, _) = following.look(&forge, &ticket(), &delivery(), at(30));
        assert_eq!(look, Look::Paused { until: at(35) });
    }

    /// A forge failure is warned of once per kind, until a read succeeds.
    #[test]
    fn a_forge_failure_is_warned_of_once() {
        let (forge, canned) = forge();
        let mut following = Following::default();
        for _ in 0..2 {
            canned.push(Response::new(502, "<html>"));
        }
        let (_, first) = following.look(&forge, &ticket(), &delivery(), at(0));
        let (look, second) = following.look(&forge, &ticket(), &delivery(), at(1));
        assert_eq!(first.len(), 1);
        assert!(second.is_empty());
        assert!(matches!(look, Look::Unread { kind: "other", .. }));
        canned.push(pr("OPEN", PUSHED, "MERGEABLE", &[]));
        following.look(&forge, &ticket(), &delivery(), at(2));
        canned.push(Response::new(502, "<html>"));
        let (_, third) = following.look(&forge, &ticket(), &delivery(), at(3));
        assert_eq!(third.len(), 1);
    }

    /// A ticket's moved stage is recorded when it changes, and again after
    /// the ticket was read back in a stage Owlshift works in (OWL-219).
    /// An owed report's refusal is warned of once, again when its reason
    /// or commit changes, and again after the report is posted or dropped
    /// (OWL-232).
    #[test]
    fn an_unposted_report_is_warned_of_once_until_its_reason_changes_or_it_settles() {
        let mut following = Following::default();
        assert!(following.report_unposted(&ticket(), "c1", "down"));
        assert!(!following.report_unposted(&ticket(), "c1", "down"));
        assert!(following.report_unposted(&ticket(), "c1", "rate limited"));
        assert!(following.report_unposted(&ticket(), "c2", "rate limited"));
        following.report_settled(&ticket());
        assert!(following.report_unposted(&ticket(), "c2", "rate limited"));
        following.forget(&ticket());
        assert!(following.report_unposted(&ticket(), "c2", "rate limited"));
    }

    #[test]
    fn a_moved_stage_is_recorded_once_until_it_changes_or_the_ticket_is_back() {
        let mut following = Following::default();
        assert!(following.stage_moved(&ticket(), "Done"));
        assert!(!following.stage_moved(&ticket(), "Done"));
        assert!(following.stage_moved(&ticket(), "Backlog"));
        following.stage_back(&ticket());
        assert!(following.stage_moved(&ticket(), "Backlog"));
        following.forget(&ticket());
        assert!(following.stage_moved(&ticket(), "Backlog"));
        following.retain(&[]);
        assert!(following.stage_moved(&ticket(), "Backlog"));
    }

    /// The hour a failure beside a running check waits counts from the
    /// first failure read on that head, and starts again after a read
    /// without one.
    #[test]
    fn the_first_failure_is_kept_per_head_until_a_read_without_one() {
        let mut following = Following::default();
        let read = |head: &str, outcome| Read {
            state: follow::PrState::Open,
            head: head.to_owned(),
            mergeable: follow::Mergeable::Mergeable,
            checks: vec![CheckRead {
                name: "ci".to_owned(),
                outcome,
            }],
        };
        let failed = read(PUSHED, CheckOutcome::CodeFailure);
        assert_eq!(following.failing_for(&ticket(), &failed, at(0)), None);
        assert_eq!(
            following.failing_for(&ticket(), &failed, at(61)),
            Some(Duration::from_secs(61 * 60))
        );
        assert_eq!(
            following.failing_for(&ticket(), &read(PUSHED, CheckOutcome::Pending), at(62)),
            None
        );
        assert_eq!(following.failing_for(&ticket(), &failed, at(63)), None);
        assert_eq!(
            following.failing_for(&ticket(), &read(PUSHING, CheckOutcome::CodeFailure), at(64)),
            None
        );
    }

    fn record(delivery: TicketDelivery, state: TicketState) -> TicketRecord {
        TicketRecord {
            state: PersistedState::from(&state),
            questions: TicketQuestions::new(),
            delivery: Some(delivery),
            intake: None,
            plan: None,
            review: None,
            cost: None,
            session: None,
        }
    }

    fn watching(failed_runs: u32) -> TicketState {
        TicketState::restore(Status::Active(Stage::Watch), 0, 0, failed_runs, 0, 0).unwrap()
    }

    fn seen_with(decision: Decision, head: &str) -> Seen {
        Seen {
            pull_request: 7,
            url: "https://github.com/demo/project/pull/7".to_owned(),
            head: head.to_owned(),
            verdict: Verdict::Green,
            mergeable: Mergeable::Mergeable,
            decision,
            base_head: None,
            failing_for: None,
        }
    }

    /// The ref changes for an end, an adopted push and a green that clears
    /// counts; not for a decision OWL-112 or OWL-210 acts on.
    #[test]
    fn only_an_end_an_adoption_or_a_clearing_green_changes_the_ref() {
        let pipeline = Pipeline::new(owlshift_core::vocab::Variant::Trivial);
        let plain = record(delivery(), watching(0));
        let fix = seen_with(
            Decision::Follow {
                adopt: false,
                action: Action::Fix { overdue: vec![] },
            },
            PUSHED,
        );
        assert_eq!(changed(&plain, pipeline, &fix, at(0)).unwrap(), None);
        let green = seen_with(
            Decision::Follow {
                adopt: false,
                action: Action::Green,
            },
            PUSHED,
        );
        assert_eq!(changed(&plain, pipeline, &green, at(0)).unwrap(), None);
        let cleared = changed(&record(delivery(), watching(1)), pipeline, &green, at(0))
            .unwrap()
            .unwrap()
            .record;
        assert_eq!(cleared.state.failed_runs, 0);

        let mut pushing = delivery();
        pushing.pushing = Some(Pushing {
            commit: PUSHING.to_owned(),
            kind: PushKind::Fix,
            base: BASE.to_owned(),
            onto: None,
            report: None,
        });
        let adopted = seen_with(
            Decision::Follow {
                adopt: true,
                action: Action::WaitChecks,
            },
            PUSHING,
        );
        let next = changed(
            &record(pushing.clone(), watching(0)),
            pipeline,
            &adopted,
            at(0),
        )
        .unwrap()
        .unwrap()
        .record
        .delivery
        .unwrap();
        assert_eq!(next.pushed.as_deref(), Some(PUSHING));
        assert_eq!(next.base_at_push.as_deref(), Some(BASE));
        assert_eq!(next.pushing, None);
        // Its report is owed, for the pass that keeps this to post (OWL-218),
        // with what the run kept for it.
        let report = KeptReport {
            summary: "Kept the greeting.".to_owned(),
            gate: None,
            checks: vec!["ci".to_owned()],
            questions: vec![],
            notes: vec![],
            followups: vec![],
            reviews: Vec::new(),
        };
        assert_eq!(
            next.owed,
            Some(OwedReport {
                kind: PushKind::Fix,
                count: 1,
                report: None,
                unchanged: false,
            })
        );
        let mut reported = pushing.clone();
        reported.pushing.as_mut().unwrap().report = Some(report.clone());
        let next = changed(&record(reported, watching(0)), pipeline, &adopted, at(0))
            .unwrap()
            .unwrap()
            .record
            .delivery
            .unwrap();
        assert_eq!(next.owed.unwrap().report, Some(report));
        // A fix push adopted counts its pass; past the bound it counts none,
        // and the following goes on.
        let fix = pushing;
        let fix_under_way = fix.clone();
        let counted = changed(&record(fix.clone(), watching(1)), pipeline, &adopted, at(0))
            .unwrap()
            .unwrap();
        let state = TicketState::try_from(&counted.record.state).unwrap();
        assert_eq!((state.fix_passes(), state.failed_runs()), (1, 0));
        let bound = TicketState::restore(Status::Active(Stage::Watch), 0, 0, 0, 2, 2).unwrap();
        let kept = changed(&record(fix, bound), pipeline, &adopted, at(0))
            .unwrap()
            .unwrap();
        assert_eq!(TicketState::try_from(&kept.record.state).unwrap(), bound);
        assert_eq!(kept.record.delivery.unwrap().owed.unwrap().count, 2);
        // An adopted push replaces the report already owed, which is handed
        // back to be told (OWL-239); with none owed, nothing is.
        let mut owing = fix_under_way.clone();
        owing.owed = Some(OwedReport {
            kind: PushKind::Rebase,
            count: 3,
            report: None,
            unchanged: false,
        });
        owing.rebased_onto = Some(ONTO.to_owned());
        let replacing = changed(&record(owing, watching(0)), pipeline, &adopted, at(0))
            .unwrap()
            .unwrap();
        assert_eq!(
            replacing.replaced.map(|owed| (owed.kind, owed.count)),
            Some((PushKind::Rebase, 3))
        );
        let nothing = changed(
            &record(fix_under_way, watching(0)),
            pipeline,
            &adopted,
            at(0),
        )
        .unwrap()
        .unwrap();
        assert_eq!(nothing.replaced, None);
        // A delivery's own push adopted owes nothing here: `do` reports it.
        let mut first = delivery();
        first.pushing = Some(Pushing {
            commit: PUSHING.to_owned(),
            kind: PushKind::Delivery,
            base: BASE.to_owned(),
            onto: None,
            report: None,
        });
        let first = changed(&record(first, watching(0)), pipeline, &adopted, at(0))
            .unwrap()
            .unwrap();
        assert_eq!(first.record.delivery.unwrap().owed, None);
        // A rebase push adopted counts a repair and keeps the base it was
        // rebased onto (OWL-210).
        let mut rebase = delivery();
        rebase.pushing = Some(Pushing {
            commit: PUSHING.to_owned(),
            kind: PushKind::Rebase,
            base: BASE.to_owned(),
            onto: Some(ONTO.to_owned()),
            report: None,
        });
        let rebased = changed(&record(rebase, watching(1)), pipeline, &adopted, at(0))
            .unwrap()
            .unwrap();
        let state = TicketState::try_from(&rebased.record.state).unwrap();
        assert_eq!((state.repairs(), state.failed_runs()), (1, 0));
        let rebased = rebased.record.delivery.unwrap();
        assert_eq!(
            (
                rebased.base_at_push.as_deref(),
                rebased.rebased_onto.as_deref()
            ),
            (Some(BASE), Some(ONTO))
        );
        assert_eq!(
            rebased.owed.as_ref().map(|owed| (owed.kind, owed.count)),
            Some((PushKind::Rebase, 1))
        );

        let moved = seen_with(Decision::End(EndReason::HeadMoved), PUSHING);
        let ended = changed(&plain, pipeline, &moved, at(5))
            .unwrap()
            .unwrap()
            .record
            .delivery
            .unwrap();
        assert!(!ended.followed());
        let ended = ended.ended.unwrap();
        assert_eq!((ended.why, ended.at), (EndedWhy::HeadMoved, at(5)));
        assert_eq!(ended.head.as_deref(), Some(PUSHING));
        // An end keeps an owed report: the pass that keeps it drops it, with
        // a warning (OWL-218).
        let mut owing = delivery();
        owing.owed = Some(OwedReport {
            kind: PushKind::Fix,
            count: 1,
            report: None,
            unchanged: false,
        });
        let ended = changed(&record(owing, watching(0)), pipeline, &moved, at(5))
            .unwrap()
            .unwrap()
            .record
            .delivery
            .unwrap();
        assert!(ended.ended.is_some() && ended.owed.is_some());
    }

    /// An end closes a round waiting at Watch (OWL-242): the ticket is back
    /// at Watch with its counts, whatever ended the following.
    #[test]
    fn an_end_closes_a_round_waiting_at_watch() {
        let pipeline = Pipeline::new(owlshift_core::vocab::Variant::Trivial);
        let waiting = TicketState::restore(
            Status::NeedsInput {
                return_to: Stage::Watch,
            },
            1,
            1,
            1,
            1,
            2,
        )
        .unwrap();
        for reason in [EndReason::Merged, EndReason::Closed, EndReason::HeadMoved] {
            let closed = changed(
                &record(delivery(), waiting),
                pipeline,
                &seen_with(Decision::End(reason), PUSHING),
                at(5),
            )
            .unwrap()
            .unwrap()
            .record;
            assert_eq!(
                closed.delivery.unwrap().ended.unwrap().why,
                ended_why(reason)
            );
            let state = TicketState::try_from(&closed.state).unwrap();
            assert_eq!(
                (
                    state.status(),
                    state.round(),
                    state.reasks(),
                    state.failed_runs(),
                    state.fix_passes(),
                    state.repairs()
                ),
                (Status::Active(Stage::Watch), 1, 0, 1, 1, 2)
            );
        }
    }

    /// An end ends a park at Watch (OWL-243), whether it waited for answers
    /// or not: back at Watch with its round, fix passes and repairs, its
    /// failed runs and re-asks cleared.
    #[test]
    fn an_end_ends_a_park_at_watch() {
        let pipeline = Pipeline::new(owlshift_core::vocab::Variant::Trivial);
        let parked = |awaiting_input, reasks, failed_runs| {
            let status = Status::Parked {
                at: Stage::Watch,
                awaiting_input,
            };
            TicketState::restore(status, 2, reasks, failed_runs, 2, 3).unwrap()
        };
        for (park, reason) in [
            (parked(false, 0, 2), EndReason::Merged),
            (parked(true, 3, 1), EndReason::HeadMoved),
        ] {
            let ended = changed(
                &record(delivery(), park),
                pipeline,
                &seen_with(Decision::End(reason), PUSHING),
                at(5),
            )
            .unwrap()
            .unwrap()
            .record;
            assert_eq!(
                ended.delivery.unwrap().ended.unwrap().why,
                ended_why(reason)
            );
            assert_eq!(
                TicketState::try_from(&ended.state).unwrap(),
                TicketState::restore(Status::Active(Stage::Watch), 2, 0, 0, 2, 3).unwrap(),
                "{park:?}"
            );
        }
    }

    /// A review loop under way (OWL-307): its reviews and kept Build.
    fn under_way() -> TicketReview {
        TicketReview {
            answered_reviews: vec![3],
            answering: vec![owlshift_contracts::brief::PullRequestReview {
                id: 9,
                author: owlshift_contracts::brief::Author {
                    name: "ann-dev".to_owned(),
                    relation: owlshift_contracts::brief::Relation::Decider,
                },
                body: "Rename it.".to_owned(),
                comments: Vec::new(),
            }],
            build: Some(owlshift_contracts::review::KeptBuild {
                commit: PUSHING.to_owned(),
                summary: "Renamed it.".to_owned(),
                pr: None,
                gate: Vec::new(),
                notes: Vec::new(),
                followups: Vec::new(),
                seen: None,
            }),
            ..TicketReview::new()
        }
    }

    /// The end of the following ends a review loop wherever it is, working,
    /// waiting or parked at Build or Verify (OWL-307): back at Watch, its
    /// reviews never answered and its kept Build dropped.
    #[test]
    fn an_end_ends_a_review_loop() {
        let pipeline = Pipeline::new(owlshift_core::vocab::Variant::Trivial);
        for status in [
            Status::Active(Stage::Build),
            Status::Active(Stage::Verify),
            Status::NeedsInput {
                return_to: Stage::Build,
            },
            Status::Parked {
                at: Stage::Verify,
                awaiting_input: false,
            },
        ] {
            let state = TicketState::restore(status, 1, 0, 0, 0, 0).unwrap();
            let mut looping = record(delivery(), state);
            looping.review = Some(under_way());
            let ended = changed(
                &looping,
                pipeline,
                &seen_with(Decision::End(EndReason::Merged), PUSHED),
                at(5),
            )
            .unwrap()
            .unwrap()
            .record;
            let state = TicketState::try_from(&ended.state).unwrap();
            assert_eq!(state.status(), Status::Active(Stage::Watch), "{status:?}");
            let review = ended.review.unwrap();
            assert!(review.answering.is_empty() && review.build.is_none());
            assert_eq!(review.answered_reviews, [3]);
            assert_eq!(ended.delivery.unwrap().ended.unwrap().why, EndedWhy::Merged);
        }
    }

    /// A review loop's push a crash cut short, adopted by a read (OWL-307),
    /// ends the loop as its command would have: from Verify to Watch, the
    /// reviews its report names answered, its report owed, counting 1.
    #[test]
    fn an_adopted_review_push_ends_its_loop() {
        let pipeline = Pipeline::new(owlshift_core::vocab::Variant::Trivial);
        let verifying = TicketState::restore(Status::Active(Stage::Verify), 1, 0, 0, 0, 0).unwrap();
        let mut d = delivery();
        d.pushing = Some(Pushing {
            commit: PUSHING.to_owned(),
            kind: PushKind::Review,
            base: BASE.to_owned(),
            onto: None,
            report: Some(KeptReport {
                summary: "Renamed it.".to_owned(),
                gate: Some(Vec::new()),
                checks: Vec::new(),
                questions: Vec::new(),
                notes: Vec::new(),
                followups: Vec::new(),
                reviews: vec![owlshift_contracts::refs::AnsweredReview {
                    id: 9,
                    author: "ann-dev".to_owned(),
                }],
            }),
        });
        let mut looping = record(d, verifying);
        looping.review = Some(under_way());
        let adopted = changed(
            &looping,
            pipeline,
            &seen_with(
                Decision::Follow {
                    adopt: true,
                    action: Action::WaitChecks,
                },
                PUSHING,
            ),
            at(5),
        )
        .unwrap()
        .unwrap()
        .record;
        let state = TicketState::try_from(&adopted.state).unwrap();
        assert_eq!(state.status(), Status::Active(Stage::Watch));
        assert_eq!((state.fix_passes(), state.repairs()), (0, 0));
        let review = adopted.review.unwrap();
        assert_eq!(review.answered_reviews, [3, 9]);
        assert!(review.answering.is_empty() && review.build.is_none());
        let delivery = adopted.delivery.unwrap();
        assert_eq!(delivery.pushed.as_deref(), Some(PUSHING));
        let owed = delivery.owed.unwrap();
        assert_eq!((owed.kind, owed.count), (PushKind::Review, 1));
        TicketReview::parse(&review.render()).unwrap();
    }
}
