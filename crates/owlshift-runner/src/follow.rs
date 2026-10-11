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
    PrState, PullRequestRead, Verdict,
};
use owlshift_contracts::ids::TicketId;
use owlshift_contracts::refs::{Ended, EndedWhy, TicketDelivery};
use owlshift_core::follow::{
    self, Action, BaseRead, CheckOutcome, CheckRead, Decision, EndReason, Followed, Read,
};
use owlshift_core::pipeline::Pipeline;
use owlshift_core::state::{Event, TicketState, Transition};

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
        checks: set.checks.iter().map(check_of).collect(),
    }
}

/// A check for the core: a code failure is a check run concluding `FAILURE`
/// or `TIMED_OUT`, or a commit status `FAILURE` or `ERROR`; any other failure
/// is one a person re-runs. Whether the check is required plays no part.
fn check_of(check: &Check) -> CheckRead {
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
        at_head: base.at_head.iter().map(check_of).collect(),
        at_push: base
            .at_push
            .as_ref()
            .map(|checks| checks.iter().map(check_of).collect()),
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
/// recorded again, and the hour a failure beside a running check waits
/// starts again.
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
        if let Some(until) = self.paused_until {
            if now < until {
                return (Look::Paused { until }, warnings);
            }
            self.paused_until = None;
        }
        let (Some(pr), Some(pushed)) = (&delivery.pull_request, &delivery.pushed) else {
            let look = Look::Unread {
                kind: "not_followed",
                reason: "its delivery has no pull request or pushed commit".to_owned(),
            };
            return (look, warnings);
        };
        let read = match forge.follow(pr.number) {
            Ok(read) => read,
            Err(error) => return self.failed(ticket, &error, now, warnings),
        };
        let PullRequestRead { checks, .. } = read;
        let core = read_of(&checks);
        let followed = Followed {
            pushed,
            pushing: delivery.pushing.as_ref().map(|p| p.commit.as_str()),
            rebased_onto: delivery.rebased_onto.as_deref(),
        };
        let failing_for = self.failing_for(ticket, &core, now);
        let mut base_head = None;
        let mut decision = follow::decide(&core, followed, None, failing_for);
        if matches!(
            decision,
            Decision::Follow {
                action: Action::NeedsBase,
                ..
            }
        ) {
            let base = Branch::new(&delivery.base_branch);
            let at_push = delivery
                .base_at_push
                .as_deref()
                .and_then(|c| CommitId::new(c).ok());
            let base = base.and_then(|base| forge.base_checks(&base, at_push.as_ref()));
            match base {
                Ok(base) => {
                    let base = base_read_of(&base);
                    decision = follow::decide(&core, followed, Some(&base), failing_for);
                    base_head = Some(base.head);
                }
                Err(error) => return self.failed(ticket, &error, now, warnings),
            }
        }
        self.unread.remove(ticket);
        if let Decision::Follow {
            action: Action::Fix { overdue },
            ..
        } = &decision
            && !overdue.is_empty()
            && self.overdue.get(ticket) != Some(&core.head)
        {
            self.overdue.insert(ticket.clone(), core.head.clone());
            warnings.push(data([
                ("what", json!("checks_overdue")),
                ("pull_request", json!(pr.number)),
                ("head", json!(core.head)),
                ("checks", json!(overdue)),
            ]));
        }
        let seen = Seen {
            pull_request: pr.number,
            url: pr.url.clone(),
            head: core.head,
            verdict: checks.verdict(),
            mergeable: checks.mergeable,
            decision,
            base_head,
        };
        (Look::Read(seen), warnings)
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
        let kind = match error.kind {
            ErrorKind::NotFound => "not_found",
            ErrorKind::Unauthorized => "unauthorized",
            ErrorKind::HeadMoved => "head_moved",
            ErrorKind::RateLimited => "rate_limited",
            ErrorKind::Other => "other",
        };
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

    /// Forgets everything kept of `ticket`.
    pub fn forget(&mut self, ticket: &TicketId) {
        self.recorded.remove(ticket);
        self.failing_since.remove(ticket);
        self.unread.remove(ticket);
        self.overdue.remove(ticket);
    }

    /// Forgets the tickets `kept` does not name.
    pub fn retain(&mut self, kept: &[TicketId]) {
        self.recorded.retain(|t, _| kept.contains(t));
        self.failing_since.retain(|t, _| kept.contains(t));
        self.unread.retain(|t, _| kept.contains(t));
        self.overdue.retain(|t, _| kept.contains(t));
    }
}

fn ended_why(reason: EndReason) -> EndedWhy {
    match reason {
        EndReason::Merged => EndedWhy::Merged,
        EndReason::Closed => EndedWhy::Closed,
        EndReason::HeadMoved => EndedWhy::HeadMoved,
    }
}

/// The record `seen` leaves in the ticket ref, `None` when it changes
/// nothing: the following ended, a push adopted, or the fix passes and
/// failed runs cleared by a green read (`ChecksGreen`). A fix, a rebase or a
/// park is decided only: OWL-112 and OWL-210 run them.
pub fn changed(
    record: &TicketRecord,
    pipeline: Pipeline,
    seen: &Seen,
    now: Timestamp,
) -> Result<Option<TicketRecord>, String> {
    let Some(delivery) = &record.delivery else {
        return Ok(None);
    };
    let mut next = record.clone();
    let mut delivery = delivery.clone();
    match &seen.decision {
        Decision::End(reason) => {
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
                delivery.pushed = Some(pushing.commit);
                delivery.base_at_push = Some(pushing.base);
            }
            if *action == Action::Green {
                let state = TicketState::try_from(&record.state).map_err(|e| e.to_string())?;
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
    Ok((next != *record).then_some(next))
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
    use owlshift_contracts::refs::{DeliveredPullRequest, PersistedState, PushKind, Pushing};
    use owlshift_core::state::Status;
    use owlshift_core::vocab::Stage;
    use serde_json::Value;

    use super::*;

    const PUSHED: &str = "1111111111111111111111111111111111111111";
    const PUSHING: &str = "2222222222222222222222222222222222222222";
    const BASE: &str = "3333333333333333333333333333333333333333";

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
            .unwrap();
        assert_eq!(cleared.state.failed_runs, 0);

        let mut pushing = delivery();
        pushing.pushing = Some(Pushing {
            commit: PUSHING.to_owned(),
            kind: PushKind::Fix,
            base: BASE.to_owned(),
        });
        let adopted = seen_with(
            Decision::Follow {
                adopt: true,
                action: Action::WaitChecks,
            },
            PUSHING,
        );
        let next = changed(&record(pushing, watching(0)), pipeline, &adopted, at(0))
            .unwrap()
            .unwrap()
            .delivery
            .unwrap();
        assert_eq!(next.pushed.as_deref(), Some(PUSHING));
        assert_eq!(next.base_at_push.as_deref(), Some(BASE));
        assert_eq!(next.pushing, None);

        let moved = seen_with(Decision::End(EndReason::HeadMoved), PUSHING);
        let ended = changed(&plain, pipeline, &moved, at(5))
            .unwrap()
            .unwrap()
            .delivery
            .unwrap();
        assert!(!ended.followed());
        let ended = ended.ended.unwrap();
        assert_eq!((ended.why, ended.at), (EndedWhy::HeadMoved, at(5)));
        assert_eq!(ended.head.as_deref(), Some(PUSHING));
    }
}
