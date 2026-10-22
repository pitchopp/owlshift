//! A broken base (OWL-418; architecture section 5, "Parallel", "A broken
//! base"): whether a Build's gate or setup failure is its branch's or the
//! base's, and the project's checks of its bases, from which a Build waits.
//! The runner keeps one [`Checks`] per project in a file, written under the
//! project's lock, and runs the checks; this module only decides. Times
//! are milliseconds since the Unix epoch.
//!
//! - *Whose.* A failure is the base's when the base, the commit the run was
//!   configured at, checked in a fresh worktree, fails at the same command
//!   of the same step, setup or gate ([`whose`]).
//! - *A verdict is not a hold.* A check's verdict is kept per commit and
//!   commands with the default branch's head it was checked at. A hold
//!   opens only once a ticket's failure matches a failed verdict
//!   ([`Checks::after_failure`], [`Checks::record_check`]): a failed
//!   verdict that no failure matched holds nothing.
//! - *Before a Build.* A Build configured at a held commit waits; it is
//!   checked again once the default branch's head moved since its check,
//!   and at once when a check is due ([`Checks::before_build`]).
//! - *Pending.* A check that cannot run yet, another ticket's step being in
//!   flight, leaves the failure pending ([`Checks::mark_pending`]): every
//!   Build configured there waits until the check runs.

/// The most entries kept; past it the oldest without a hold goes first.
pub const MAX_CHECKS: usize = 16;

/// The part of a gate run a command belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Step {
    /// The project's `stack.setup`, before the gate's commands or the Build's
    /// harness.
    Setup,
    /// The gate's own commands.
    Gate,
}

/// A command a gate run failed on.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Failing {
    pub step: Step,
    /// The command, as the project file writes it.
    pub command: String,
}

/// A base's failed check, as kept: the command, then what the operator is
/// told of it, which nothing here reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    pub failing: Failing,
    pub reason: String,
    /// The end of the command's output.
    pub tail: String,
}

/// What a check of a base found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    Passed,
    Failed(Failure),
}

impl Verdict {
    fn failing(&self) -> Option<&Failing> {
        match self {
            Verdict::Passed => None,
            Verdict::Failed(failure) => Some(&failure.failing),
        }
    }
}

/// Whose a branch's failure is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Whose {
    Branch,
    Base,
}

/// Whose `branch`'s failure is, given the verdict of its base: the base's
/// when the base failed at the same step and the same command.
pub fn whose(branch: &Failing, base: &Verdict) -> Whose {
    if base.failing() == Some(branch) {
        Whose::Base
    } else {
        Whose::Branch
    }
}

/// A hold on a commit: since when, and whether the operator was told.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hold {
    pub since_ms: i64,
    pub told: bool,
}

/// What is known of one commit and its commands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Entry {
    /// A ticket's failure waits for a check that could not run yet.
    Pending { failing: Failing, since_ms: i64 },
    /// A check ran at `head`, the default branch's head then.
    Checked {
        head: String,
        checked_ms: i64,
        verdict: Verdict,
        hold: Option<Hold>,
    },
}

impl Entry {
    fn at_ms(&self) -> i64 {
        match self {
            Entry::Pending { since_ms, .. } => *since_ms,
            Entry::Checked { checked_ms, .. } => *checked_ms,
        }
    }

    fn hold(&self) -> Option<Hold> {
        match self {
            Entry::Checked { hold, .. } => *hold,
            Entry::Pending { .. } => None,
        }
    }

    /// Whether it holds the Builds configured at its commit: a hold, or a
    /// check due.
    pub fn holds(&self) -> bool {
        matches!(self, Entry::Pending { .. }) || self.hold().is_some()
    }
}

/// One commit's entry: `commands` is the runner's key of the setup and gate
/// commands checked, opaque here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Check {
    pub commit: String,
    pub commands: String,
    pub entry: Entry,
}

/// What a Build configured at a commit does before it starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Before {
    /// Nothing holds it.
    Go,
    /// It waits: the commit is held, checked at this head.
    Held,
    /// A check is due before it may start: one is pending, or the commit is
    /// held and the default branch's head moved since its check.
    CheckDue,
}

/// What a branch's failure comes to against what is kept.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum After {
    /// The base's verdict at this head matches it: held, the hold opened
    /// now or standing.
    Matched(Recorded),
    /// The base's verdict at this head does not match it: the branch's.
    Branch,
    /// No verdict at this head: a check is due.
    CheckDue,
}

/// How a hold changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Recorded {
    Opened,
    StillHeld,
    Lifted,
    /// No hold before and none after: the verdict is kept as it is.
    Cached,
}

/// A project's checks of its bases.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Checks {
    pub checks: Vec<Check>,
}

impl Checks {
    /// The entry of `commit` and `commands`.
    pub fn get(&self, commit: &str, commands: &str) -> Option<&Check> {
        self.checks
            .iter()
            .find(|check| check.commit == commit && check.commands == commands)
    }

    /// The verdict kept for `commit` and `commands`, checked at `head`: a
    /// check that need not run again.
    pub fn verdict_at(&self, commit: &str, commands: &str, head: &str) -> Option<&Verdict> {
        match &self.get(commit, commands)?.entry {
            Entry::Checked {
                head: checked,
                verdict,
                ..
            } if checked == head => Some(verdict),
            _ => None,
        }
    }

    fn position(&self, commit: &str, commands: &str) -> Option<usize> {
        self.checks
            .iter()
            .position(|check| check.commit == commit && check.commands == commands)
    }

    /// What a Build configured at `commit` with `commands` does before it
    /// starts, the default branch's head being `head`.
    pub fn before_build(&self, commit: &str, commands: &str, head: &str) -> Before {
        match self.get(commit, commands).map(|check| &check.entry) {
            None => Before::Go,
            Some(Entry::Pending { .. }) => Before::CheckDue,
            Some(Entry::Checked {
                hold: Some(_),
                head: checked,
                ..
            }) => {
                if checked == head {
                    Before::Held
                } else {
                    Before::CheckDue
                }
            }
            Some(Entry::Checked { hold: None, .. }) => Before::Go,
        }
    }

    /// A branch's `failing` against the verdict of `commit` at `head`, at
    /// `now`: a match holds the commit, opening the hold when none stands.
    pub fn after_failure(
        &mut self,
        commit: &str,
        commands: &str,
        head: &str,
        failing: &Failing,
        now_ms: i64,
    ) -> After {
        let Some(at) = self.position(commit, commands) else {
            return After::CheckDue;
        };
        let Entry::Checked {
            head: checked,
            verdict,
            hold,
            ..
        } = &mut self.checks[at].entry
        else {
            return After::CheckDue;
        };
        if checked != head {
            return After::CheckDue;
        }
        match (whose(failing, verdict), hold.is_some()) {
            (Whose::Branch, _) => After::Branch,
            (Whose::Base, true) => After::Matched(Recorded::StillHeld),
            (Whose::Base, false) => {
                *hold = Some(Hold {
                    since_ms: now_ms,
                    told: false,
                });
                After::Matched(Recorded::Opened)
            }
        }
    }

    /// Keeps the check of `commit` and `commands` at `head`, at `now`, which
    /// found `verdict`. `initiating` is the failure it was run for, if any;
    /// a pending entry's failure counts as one. A failed verdict holds the
    /// commit when a hold stood, a re-check that fails at another command
    /// included, or when the failure it is matched with is the base's;
    /// a passing one lifts a hold.
    pub fn record_check(
        &mut self,
        (commit, commands, head): (&str, &str, &str),
        now_ms: i64,
        verdict: Verdict,
        initiating: Option<&Failing>,
    ) -> Recorded {
        let previous = self
            .position(commit, commands)
            .map(|at| self.checks.remove(at));
        let before = previous.as_ref().and_then(|check| check.entry.hold());
        let pending = previous.as_ref().and_then(|check| match &check.entry {
            Entry::Pending { failing, .. } => Some(failing.clone()),
            Entry::Checked { .. } => None,
        });
        let initiating = initiating.or(pending.as_ref());
        let failed = matches!(verdict, Verdict::Failed(_));
        let matched = initiating.is_some_and(|failing| whose(failing, &verdict) == Whose::Base);
        let after = (failed && (before.is_some() || matched)).then(|| {
            before.unwrap_or(Hold {
                since_ms: now_ms,
                told: false,
            })
        });
        self.checks.push(Check {
            commit: commit.to_owned(),
            commands: commands.to_owned(),
            entry: Entry::Checked {
                head: head.to_owned(),
                checked_ms: now_ms,
                verdict,
                hold: after,
            },
        });
        self.trim();
        match (before.is_some(), after.is_some()) {
            (false, true) => Recorded::Opened,
            (true, true) => Recorded::StillHeld,
            (true, false) => Recorded::Lifted,
            (false, false) => Recorded::Cached,
        }
    }

    /// Leaves `failing` waiting for a check of `commit` and `commands` that
    /// could not run yet, at `now`: nothing changes on a commit already held,
    /// which holds its Builds anyway. Returns whether it changed anything.
    pub fn mark_pending(
        &mut self,
        commit: &str,
        commands: &str,
        failing: &Failing,
        now_ms: i64,
    ) -> bool {
        match self.position(commit, commands) {
            Some(at) if self.checks[at].entry.holds() => false,
            found => {
                if let Some(at) = found {
                    self.checks.remove(at);
                }
                self.checks.push(Check {
                    commit: commit.to_owned(),
                    commands: commands.to_owned(),
                    entry: Entry::Pending {
                        failing: failing.clone(),
                        since_ms: now_ms,
                    },
                });
                self.trim();
                true
            }
        }
    }

    /// Whether an operator's check of `commit` and `commands` is kept: only
    /// one that holds or waits is, so `owlshift gate` never opens a hold.
    pub fn holds(&self, commit: &str, commands: &str) -> bool {
        self.get(commit, commands)
            .is_some_and(|check| check.entry.holds())
    }

    /// Whether any entry of `commit` holds its Builds, whatever its
    /// commands.
    pub fn holds_commit(&self, commit: &str) -> bool {
        self.checks
            .iter()
            .any(|check| check.commit == commit && check.entry.holds())
    }

    /// Marks the hold of `commit` and `commands` told: `true` the first time
    /// only, so the operator is told once per hold.
    pub fn claim_told(&mut self, commit: &str, commands: &str) -> bool {
        let Some(at) = self.position(commit, commands) else {
            return false;
        };
        match &mut self.checks[at].entry {
            Entry::Checked {
                hold: Some(hold), ..
            } if !hold.told => {
                hold.told = true;
                true
            }
            _ => false,
        }
    }

    /// The entries that hold their Builds: holds and pending checks.
    pub fn standing(&self) -> impl Iterator<Item = &Check> {
        self.checks.iter().filter(|check| check.entry.holds())
    }

    /// Drops the oldest entries past [`MAX_CHECKS`], those that hold nothing
    /// first.
    fn trim(&mut self) {
        while self.checks.len() > MAX_CHECKS {
            let oldest = |keep_holding: bool| {
                self.checks
                    .iter()
                    .enumerate()
                    .filter(|(_, check)| !keep_holding || !check.entry.holds())
                    .min_by_key(|(_, check)| check.entry.at_ms())
                    .map(|(at, _)| at)
            };
            let Some(at) = oldest(true).or_else(|| oldest(false)) else {
                return;
            };
            self.checks.remove(at);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const X: &str = "x";
    const KEY: &str = "key";

    fn setup(command: &str) -> Failing {
        Failing {
            step: Step::Setup,
            command: command.to_owned(),
        }
    }

    fn failed(failing: &Failing) -> Verdict {
        Verdict::Failed(Failure {
            failing: failing.clone(),
            reason: "setup: exit status 1".to_owned(),
            tail: "ERR_PNPM_NO_OFFLINE_TARBALL".to_owned(),
        })
    }

    fn record(
        checks: &mut Checks,
        head: &str,
        now: i64,
        verdict: Verdict,
        by: Option<&Failing>,
    ) -> Recorded {
        checks.record_check((X, KEY, head), now, verdict, by)
    }

    #[test]
    fn a_failure_is_the_bases_only_at_the_same_command_of_the_same_step() {
        let install = setup("pnpm install --offline");
        assert_eq!(whose(&install, &failed(&install)), Whose::Base);
        assert_eq!(whose(&install, &Verdict::Passed), Whose::Branch);
        assert_eq!(whose(&install, &failed(&setup("uv sync"))), Whose::Branch);
        let as_gate = Failing {
            step: Step::Gate,
            command: install.command.clone(),
        };
        assert_eq!(whose(&as_gate, &failed(&install)), Whose::Branch);
    }

    /// Criteria 1 and 4: a matched failure opens the hold once; a verdict
    /// failing elsewhere, or passing, is kept and holds nothing.
    #[test]
    fn a_verdict_holds_only_once_a_failure_matches_it() {
        let install = setup("install");
        let test = Failing {
            step: Step::Gate,
            command: "make test".to_owned(),
        };
        let mut checks = Checks::default();
        assert_eq!(
            checks.after_failure(X, KEY, "h1", &test, 1),
            After::CheckDue
        );
        // The branch failed `make test`, the base its install: the branch's,
        // and the base's failure holds no one.
        assert_eq!(
            record(&mut checks, "h1", 1, failed(&install), Some(&test)),
            Recorded::Cached
        );
        assert_eq!(checks.before_build(X, KEY, "h1"), Before::Go);
        assert_eq!(checks.after_failure(X, KEY, "h1", &test, 2), After::Branch);
        // A second ticket failing the install matches the kept verdict: held,
        // with no check run.
        assert_eq!(
            checks.after_failure(X, KEY, "h1", &install, 3),
            After::Matched(Recorded::Opened)
        );
        assert_eq!(
            checks.after_failure(X, KEY, "h1", &install, 4),
            After::Matched(Recorded::StillHeld)
        );
        assert_eq!(checks.before_build(X, KEY, "h1"), Before::Held);
        // Another head: the verdict is stale.
        assert_eq!(
            checks.after_failure(X, KEY, "h2", &install, 5),
            After::CheckDue
        );
        assert_eq!(checks.before_build(X, KEY, "h2"), Before::CheckDue);
        // Other commands, another commit: nothing kept.
        assert_eq!(checks.before_build(X, "other", "h1"), Before::Go);
        assert_eq!(checks.before_build("y", KEY, "h1"), Before::Go);
    }

    /// Criterion 3: a re-check keeps the hold while the commit fails, at any
    /// command, its time and told mark kept, and a pass lifts it.
    #[test]
    fn a_hold_stands_until_a_check_passes() {
        let install = setup("install");
        let mut checks = Checks::default();
        assert_eq!(
            record(&mut checks, "h1", 10, failed(&install), Some(&install)),
            Recorded::Opened
        );
        assert!(checks.claim_told(X, KEY));
        assert!(!checks.claim_told(X, KEY));
        let other = setup("other");
        assert_eq!(
            record(&mut checks, "h2", 20, failed(&other), None),
            Recorded::StillHeld
        );
        let Entry::Checked {
            hold,
            head,
            verdict,
            ..
        } = &checks.get(X, KEY).unwrap().entry
        else {
            panic!("checked")
        };
        assert_eq!(
            (*hold, head.as_str()),
            (
                Some(Hold {
                    since_ms: 10,
                    told: true
                }),
                "h2"
            )
        );
        assert_eq!(verdict, &failed(&other));
        assert!(!checks.claim_told(X, KEY), "told once per hold");
        assert_eq!(
            record(&mut checks, "h3", 30, Verdict::Passed, None),
            Recorded::Lifted
        );
        assert_eq!(checks.before_build(X, KEY, "h3"), Before::Go);
        assert!(!checks.holds(X, KEY));
        // A new hold is told again.
        assert_eq!(
            record(&mut checks, "h4", 40, failed(&install), Some(&install)),
            Recorded::Opened
        );
        assert!(checks.claim_told(X, KEY));
    }

    /// Criterion 2 and the busy path: a pending failure holds every Build
    /// configured there and is the failure its check is matched with.
    #[test]
    fn a_pending_check_holds_until_it_runs() {
        let install = setup("install");
        let mut checks = Checks::default();
        assert!(checks.mark_pending(X, KEY, &install, 1));
        assert_eq!(checks.before_build(X, KEY, "h1"), Before::CheckDue);
        assert!(checks.holds(X, KEY) && checks.holds_commit(X));
        assert_eq!(
            checks.after_failure(X, KEY, "h1", &install, 2),
            After::CheckDue
        );
        assert_eq!(
            record(&mut checks, "h1", 3, failed(&install), None),
            Recorded::Opened
        );
        assert_eq!(checks.before_build(X, KEY, "h1"), Before::Held);
        // A held commit stays held, not pending.
        assert!(!checks.mark_pending(X, KEY, &install, 4));
        assert_eq!(checks.before_build(X, KEY, "h1"), Before::Held);
        // A pending failure the base does not give holds nothing once checked.
        let mut checks = Checks::default();
        checks.mark_pending(X, KEY, &install, 1);
        assert_eq!(
            record(&mut checks, "h1", 2, Verdict::Passed, None),
            Recorded::Cached
        );
        assert_eq!(checks.before_build(X, KEY, "h1"), Before::Go);
        assert!(!checks.holds_commit(X));
    }

    #[test]
    fn the_oldest_entry_holding_nothing_goes_first() {
        let install = setup("install");
        let mut checks = Checks::default();
        checks.record_check(("held", KEY, "h"), 0, failed(&install), Some(&install));
        for n in 1..=MAX_CHECKS {
            let commit = format!("c{n}");
            checks.record_check(
                (&commit, KEY, "h"),
                i64::try_from(n).unwrap(),
                Verdict::Passed,
                None,
            );
        }
        assert_eq!(checks.checks.len(), MAX_CHECKS);
        assert!(
            checks.holds("held", KEY),
            "the hold, oldest of all, is kept"
        );
        assert!(checks.get("c1", KEY).is_none());
        assert!(checks.get("c2", KEY).is_some());
        assert_eq!(checks.standing().count(), 1);
    }
}
