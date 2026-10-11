//! What a read of a delivered pull request decides (OWL-209): the table of
//! architecture section 5, "Watch after delivery", as a pure function over
//! plain data. The runner does the reading, keeps the times and maps the
//! forge's answer onto [`Read`] and [`BaseRead`]; OWL-112 and OWL-210 run the
//! fix and the rebase it decides.

use std::time::Duration;

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
    fn failed(self) -> bool {
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
    let on_head = |name: &str| at_head.iter().find(|c| c.name == name).map(|c| c.outcome);
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

/// Whether `read` shows a failed check: the runner's first-failure time
/// starts at the first such read of a head and is forgotten at the first
/// read without one.
pub fn has_failure(read: &Read) -> bool {
    read.checks.iter().any(|c| c.outcome.failed())
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
    }
}
