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
//! - *Leaving a broken commit* (OWL-427). A held Build whose branch has
//!   commits of its own is rebased onto a newer default-branch head once
//!   that head passes its own check ([`leave`]). Whatever its branch, a held
//!   Build checks that head, matched with its held commit's command, so a
//!   head that still fails there is held too (OWL-639).
//! - *The repair ticket* (OWL-622). A checked entry keeps the ticket the
//!   runner filed for its hold ([`Repair`]), kept across its checks, a lift
//!   included: the runner admits a repair while an entry naming it holds
//!   ([`Checks::repairs`]), and a hold that opens again, or another on the
//!   same command, takes an open one again ([`Checks::reusable`]).
//! - *A break* (OWL-639). The holds on one step and command since a check of
//!   the default branch's head last passed are one break ([`Break`]), told
//!   once, which files at most [`MAX_BREAK_REPAIRS`] repair tickets.

/// The most entries kept; past it the oldest without a hold goes first.
pub const MAX_CHECKS: usize = 16;

/// The most repair tickets one break files (OWL-639): a repair that merged
/// with the base still failing is followed by one more; past it, a person.
pub const MAX_BREAK_REPAIRS: usize = 2;

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

/// A held Build's branch, as its step reads it (OWL-427).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeldBranch {
    /// Its head is an ancestor of the default branch's head: its next step
    /// into Build fast-forwards it, no rebase needed.
    NoCommitsOfItsOwn,
    /// Commits of its own, its worktree clean: a rebase can move it.
    OwnCommits,
    /// Its worktree lists uncommitted paths, which moving the branch back
    /// after a failed rebase would drop.
    Uncommitted,
    /// Its delivery is followed: its pull request is open, whose history
    /// only Watch's rebase push replaces (decided on 2026-10-23).
    Delivered,
}

/// What a Build held at a commit that still fails does about a newer head
/// of the default branch (OWL-427).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Leave {
    /// It stays held, as before.
    Stay,
    /// The head is checked first, matched with the held commit's failing
    /// command: or its kept verdict is, when it fails there and no hold
    /// stands on it yet (OWL-639).
    CheckHead,
    /// Its branch is rebased onto the head.
    Rebase,
}

/// What a Build held at `held`, which still fails `held_failing`, does about
/// `head`, the default branch's head. A head that contains `held`
/// (`head_has_held`) is checked when no verdict of it is kept, whatever the
/// branch (OWL-639), and again matched when its kept verdict fails at
/// `held_failing` with no hold on it (`head_held`), which holds it. A branch
/// with commits of its own and a clean worktree is rebased onto such a head
/// that passes its own check (`head_verdict`, OWL-427). Anything else stays
/// held: an environment fix lifts the hold through `held`'s own check, and a
/// head that does not contain `held` was rewritten, so a rebase onto it
/// would replay `held`'s own commits.
pub fn leave(
    held: &str,
    held_failing: &Failing,
    head: &str,
    head_verdict: Option<&Verdict>,
    head_has_held: bool,
    head_held: bool,
    branch: HeldBranch,
) -> Leave {
    if head == held || !head_has_held {
        return Leave::Stay;
    }
    match head_verdict {
        None => Leave::CheckHead,
        Some(Verdict::Passed) if branch == HeldBranch::OwnCommits => Leave::Rebase,
        Some(Verdict::Passed) => Leave::Stay,
        Some(verdict) if !head_held && whose(held_failing, verdict) == Whose::Base => {
            Leave::CheckHead
        }
        Some(Verdict::Failed(_)) => Leave::Stay,
    }
}

/// A hold on a commit: since when, and whether the operator was told.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hold {
    pub since_ms: i64,
    pub told: bool,
}

/// The ticket the runner filed for a hold (OWL-622; architecture section 5,
/// "The repair ticket"): tried once per hold opened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Repair {
    /// The tracker refused it, or no operator could be named: the hold is
    /// told as before, and no ticket is filed until a hold opens again.
    Unfiled,
    /// Filed, or an open one taken again: its identifier, and the hash of
    /// its title and description as the tracker answered them, which its
    /// admission by the runner requires unchanged.
    Filed { ticket: String, text_sha256: String },
}

impl Repair {
    /// The ticket filed, if any.
    pub fn ticket(&self) -> Option<&str> {
        match self {
            Repair::Filed { ticket, .. } => Some(ticket),
            Repair::Unfiled => None,
        }
    }
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
        /// The repair ticket filed for a hold on it (OWL-622): `None` while
        /// none was tried. Kept by the checks that follow, a lift included.
        repair: Option<Repair>,
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

/// A break of the base (OWL-639): the holds on one step and command since a
/// check of the default branch's head last passed, kept apart from the
/// entries, so that neither a dropped entry nor a hold's repair taken again
/// resets what it filed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Break {
    pub failing: Failing,
    /// The operator was told of it, at its first hold.
    pub told: bool,
    /// The operator was told that it filed its last repair.
    pub told_spent: bool,
    /// The repair tickets filed or taken again for it, in order.
    pub repairs: Vec<String>,
}

/// A project's checks of its bases.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Checks {
    pub checks: Vec<Check>,
    /// The breaks standing, one a step and command (OWL-639), at most
    /// [`MAX_CHECKS`], the oldest dropped first.
    pub breaks: Vec<Break>,
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
    /// a passing one lifts a hold. A pass of the default branch's head itself
    /// ends every break (OWL-639).
    pub fn record_check(
        &mut self,
        (commit, commands, head): (&str, &str, &str),
        now_ms: i64,
        verdict: Verdict,
        initiating: Option<&Failing>,
    ) -> Recorded {
        if verdict == Verdict::Passed && commit == head {
            self.breaks.clear();
        }
        let previous = self
            .position(commit, commands)
            .map(|at| self.checks.remove(at));
        let before = previous.as_ref().and_then(|check| check.entry.hold());
        let (pending, repair) = match previous.map(|check| check.entry) {
            Some(Entry::Pending { failing, .. }) => (Some(failing), None),
            Some(Entry::Checked { repair, .. }) => (None, repair),
            None => (None, None),
        };
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
                repair,
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

    /// Drops the pending check of `commit` and `commands`, one that ran and
    /// said nothing of the commands: its failure takes the branch's path.
    /// Returns whether one was dropped; a hold is never dropped here.
    pub fn drop_pending(&mut self, commit: &str, commands: &str) -> bool {
        match self.position(commit, commands) {
            Some(at) if matches!(self.checks[at].entry, Entry::Pending { .. }) => {
                self.checks.remove(at);
                true
            }
            _ => false,
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

    /// The repair ticket kept on the entry of `commit` and `commands`
    /// (OWL-622): `None` when none was tried, or the entry is pending.
    pub fn repair(&self, commit: &str, commands: &str) -> Option<&Repair> {
        match &self.get(commit, commands)?.entry {
            Entry::Checked { repair, .. } => repair.as_ref(),
            Entry::Pending { .. } => None,
        }
    }

    /// Keeps `repair` on the entry of `commit` and `commands`, a checked one:
    /// whether there was one to keep it on.
    pub fn set_repair(&mut self, commit: &str, commands: &str, repair: Repair) -> bool {
        let Some(at) = self.position(commit, commands) else {
            return false;
        };
        match &mut self.checks[at].entry {
            Entry::Checked { repair: kept, .. } => {
                *kept = Some(repair);
                true
            }
            Entry::Pending { .. } => false,
        }
    }

    /// The repair tickets the runner admits (OWL-622): each filed for an
    /// entry that holds now, with the hash of its text when filed, once
    /// each, in the entries' order. A repair whose hold lifted is its
    /// decider's.
    pub fn repairs(&self) -> Vec<(&str, &str)> {
        let mut found: Vec<(&str, &str)> = Vec::new();
        for check in &self.checks {
            if let Entry::Checked {
                hold: Some(_),
                repair:
                    Some(Repair::Filed {
                        ticket,
                        text_sha256,
                    }),
                ..
            } = &check.entry
                && !found.iter().any(|(id, _)| id == ticket)
            {
                found.push((ticket, text_sha256));
            }
        }
        found
    }

    /// The holds that name `ticket` as their repair (OWL-622): what its own
    /// Build is not held by, whatever commit it is configured at.
    pub fn held_for(&self, ticket: &str) -> impl Iterator<Item = &Check> {
        self.checks.iter().filter(move |check| {
            matches!(
                &check.entry,
                Entry::Checked {
                    hold: Some(_),
                    repair: Some(Repair::Filed { ticket: named, .. }),
                    ..
                } if named == ticket
            )
        })
    }

    /// The repair tickets a hold on `commit` and `commands` that fails at
    /// `failing` may take again rather than file one (OWL-622): its own
    /// entry's first, then those of every entry whose verdict failed at the
    /// same step and command, held or lifted, once each. The caller keeps
    /// only one still open on the tracker.
    pub fn reusable(&self, commit: &str, commands: &str, failing: &Failing) -> Vec<String> {
        let own = self
            .repair(commit, commands)
            .and_then(Repair::ticket)
            .map(str::to_owned);
        let mut found: Vec<String> = own.into_iter().collect();
        for check in &self.checks {
            if let Entry::Checked {
                verdict: Verdict::Failed(failure),
                repair: Some(Repair::Filed { ticket, .. }),
                ..
            } = &check.entry
                && failure.failing == *failing
                && !found.contains(ticket)
            {
                found.push(ticket.clone());
            }
        }
        found
    }

    /// The last commit whose check of `commands` passed, by the time it was
    /// checked: where a repair's description counts the default branch's
    /// commits from (OWL-622).
    pub fn last_passed(&self, commands: &str) -> Option<&str> {
        self.checks
            .iter()
            .filter(|check| {
                check.commands == commands
                    && matches!(
                        check.entry,
                        Entry::Checked {
                            verdict: Verdict::Passed,
                            ..
                        }
                    )
            })
            .max_by_key(|check| check.entry.at_ms())
            .map(|check| check.commit.as_str())
    }

    /// The repair tickets the break of `failing` filed or took again
    /// (OWL-639), in order: none when no break of it stands.
    pub fn break_repairs(&self, failing: &Failing) -> &[String] {
        self.breaks
            .iter()
            .find(|standing| standing.failing == *failing)
            .map_or(&[], |standing| standing.repairs.as_slice())
    }

    /// Whether the break of `failing` filed its last repair (OWL-639): a hold
    /// in it files none.
    pub fn break_spent(&self, failing: &Failing) -> bool {
        self.break_repairs(failing).len() >= MAX_BREAK_REPAIRS
    }

    /// Keeps `ticket` among the repairs of the break of `failing`, once:
    /// its place in the break, 1 for its first.
    pub fn keep_break_repair(&mut self, failing: &Failing, ticket: &str) -> usize {
        let repairs = &mut self.break_of(failing).repairs;
        match repairs.iter().position(|kept| kept == ticket) {
            Some(at) => at + 1,
            None => {
                repairs.push(ticket.to_owned());
                repairs.len()
            }
        }
    }

    /// Marks the break of `failing` told: `true` the first time only, so the
    /// operator is told once a break (OWL-639).
    pub fn claim_break_told(&mut self, failing: &Failing) -> bool {
        !std::mem::replace(&mut self.break_of(failing).told, true)
    }

    /// Marks the break of `failing` told that it filed its last repair:
    /// `true` the first time only.
    pub fn claim_spent_told(&mut self, failing: &Failing) -> bool {
        let standing = self.break_of(failing);
        standing.told = true;
        !std::mem::replace(&mut standing.told_spent, true)
    }

    /// The break of `failing`, begun when none stands; past [`MAX_CHECKS`]
    /// breaks the oldest goes.
    fn break_of(&mut self, failing: &Failing) -> &mut Break {
        let at = match self
            .breaks
            .iter()
            .position(|standing| standing.failing == *failing)
        {
            Some(at) => at,
            None => {
                if self.breaks.len() >= MAX_CHECKS {
                    self.breaks.remove(0);
                }
                self.breaks.push(Break {
                    failing: failing.clone(),
                    told: false,
                    told_spent: false,
                    repairs: Vec::new(),
                });
                self.breaks.len() - 1
            }
        };
        &mut self.breaks[at]
    }

    /// Whether a check of `commands` at the default branch's head it fetched
    /// passed after `since_ms` (OWL-642): such a pass ends every break, so a
    /// repair claim taken before it counts an earlier break's repairs.
    pub fn passed_after(&self, commands: &str, since_ms: i64) -> bool {
        self.checks.iter().any(|check| {
            check.commands == commands
                && matches!(
                    &check.entry,
                    Entry::Checked {
                        head,
                        checked_ms,
                        verdict: Verdict::Passed,
                        ..
                    } if *head == check.commit && *checked_ms > since_ms
                )
        })
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

/// How long a break's repair claim naming no repair is a filing under way
/// (OWL-642), by its holder's clock against the reader's: past it, or dated
/// further than it ahead, the claim is taken over.
pub const REPAIR_CLAIM_LEASE_MS: i64 = 15 * 60 * 1000;

/// A break's repair claim as a runner read it on the remote (OWL-642), the
/// tracker asked whether the repair it names is still open.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClaimSeen {
    /// The repair it names and whether it is open; none while it is filed.
    pub ticket: Option<(String, bool)>,
    /// When it was taken, by its holder's clock.
    pub claimed_ms: i64,
    /// The repairs it says the break filed, the one it names or is filing
    /// included.
    pub repairs: u32,
}

/// What a filing does about its break's repair claim (OWL-642).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClaimMove {
    /// The claim names an open repair: it is taken again.
    TakeAgain(String),
    /// A repair is being filed under it: none is filed now.
    Wait,
    /// The claim is taken, or replaced, and a repair filed, `before` having
    /// been filed for the break.
    File { before: usize },
    /// The break filed its last repair, `before` in all: none is filed.
    Spent { before: usize },
}

/// What a filing does about `seen`, its break's repair claim on the remote,
/// `None` when there is none, at `now_ms` (OWL-642; architecture section 5,
/// "The repair ticket", "Not twice"). `local` is the repairs this runner's
/// break of the same command filed or took again; `passed_since` whether
/// this runner recorded a pass of the head after the claim was taken, which
/// ended the break the claim counts. A claim naming an open repair is taken
/// again; one naming none is a filing under way within
/// [`REPAIR_CLAIM_LEASE_MS`], whoever holds it; anything else is replaced,
/// and the break's repairs counted on both sides bound the filing, so that
/// [`MAX_BREAK_REPAIRS`] holds across runners.
pub fn claim_move(
    seen: Option<&ClaimSeen>,
    now_ms: i64,
    local: usize,
    passed_since: bool,
) -> ClaimMove {
    let before = match seen {
        None => local,
        Some(ClaimSeen {
            ticket: Some((ticket, true)),
            ..
        }) => return ClaimMove::TakeAgain(ticket.clone()),
        Some(seen)
            if seen.ticket.is_none()
                && now_ms < seen.claimed_ms.saturating_add(REPAIR_CLAIM_LEASE_MS)
                && seen.claimed_ms <= now_ms.saturating_add(REPAIR_CLAIM_LEASE_MS) =>
        {
            return ClaimMove::Wait;
        }
        Some(_) if passed_since => local,
        Some(seen) => local.max(usize::try_from(seen.repairs).unwrap_or(usize::MAX)),
    };
    if before >= MAX_BREAK_REPAIRS {
        ClaimMove::Spent { before }
    } else {
        ClaimMove::File { before }
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

    /// OWL-427: only a branch with commits of its own and a clean worktree
    /// leaves a held commit, onto a head that contains it and passes. OWL-639:
    /// whatever the branch, a head with no verdict is checked first, and one
    /// failing at the held command with no hold on it is matched.
    #[test]
    fn a_held_branch_with_commits_of_its_own_leaves_for_a_head_that_passes() {
        use HeldBranch::{Delivered, NoCommitsOfItsOwn, OwnCommits, Uncommitted};
        let install = setup("install");
        let broken = failed(&install);
        let elsewhere = failed(&setup("lint"));
        let passed = Verdict::Passed;
        let cases = [
            ("h", true, Some(&passed), false, OwnCommits, Leave::Rebase),
            ("h", true, None, false, OwnCommits, Leave::CheckHead),
            (
                "h",
                true,
                Some(&broken),
                false,
                OwnCommits,
                Leave::CheckHead,
            ),
            ("h", true, Some(&broken), true, OwnCommits, Leave::Stay),
            ("h", true, Some(&elsewhere), false, OwnCommits, Leave::Stay),
            // The head is the held commit: nothing newer to rebase onto.
            (X, true, Some(&passed), false, OwnCommits, Leave::Stay),
            (X, true, None, false, OwnCommits, Leave::Stay),
            // A rewritten default branch, the held commit not in it.
            ("h", false, Some(&passed), false, OwnCommits, Leave::Stay),
            ("h", false, None, false, Delivered, Leave::Stay),
        ];
        for (head, has_held, verdict, held, branch, expected) in cases {
            assert_eq!(
                leave(X, &install, head, verdict, has_held, held, branch),
                expected,
                "{head} {has_held} {verdict:?} {held} {branch:?}"
            );
        }
        // Any other branch is checked and held alike, never rebased.
        for branch in [NoCommitsOfItsOwn, Uncommitted, Delivered] {
            let read = |verdict, held| leave(X, &install, "h", verdict, true, held, branch);
            assert_eq!(read(None, false), Leave::CheckHead, "{branch:?}");
            assert_eq!(read(Some(&broken), false), Leave::CheckHead, "{branch:?}");
            assert_eq!(read(Some(&broken), true), Leave::Stay, "{branch:?}");
            assert_eq!(read(Some(&passed), false), Leave::Stay, "{branch:?}");
        }
    }

    /// OWL-622: a hold's repair is kept by every check of its entry, a lift
    /// included; the runner admits it only while the entry holds; a head
    /// that moves and still fails at the same command offers it again, and
    /// another command does not.
    #[test]
    fn a_holds_repair_is_kept_and_offered_again_on_the_same_command() {
        let install = setup("install");
        let filed = |ticket: &str| Repair::Filed {
            ticket: ticket.to_owned(),
            text_sha256: "t".to_owned(),
        };
        let mut checks = Checks::default();
        assert_eq!(
            record(&mut checks, "h1", 10, failed(&install), Some(&install)),
            Recorded::Opened
        );
        assert_eq!(checks.repair(X, KEY), None);
        assert!(checks.set_repair(X, KEY, filed("DEMO-7")));
        assert!(!checks.set_repair("unknown", KEY, filed("DEMO-8")));
        assert_eq!(checks.repairs(), [("DEMO-7", "t")]);
        assert_eq!(checks.held_for("DEMO-7").count(), 1);
        // The head moves, still failing: the same hold, the same repair.
        assert_eq!(
            record(&mut checks, "h2", 20, failed(&install), None),
            Recorded::StillHeld
        );
        assert_eq!(checks.repair(X, KEY), Some(&filed("DEMO-7")));
        // A newer commit failing the same command offers it; another
        // command does not.
        checks.record_check(("y", KEY, "h2"), 21, failed(&install), Some(&install));
        assert_eq!(checks.reusable("y", KEY, &install), ["DEMO-7"]);
        assert!(checks.reusable("y", KEY, &setup("other")).is_empty());
        // Lifted: kept on the entry, no longer the runner's to admit, and
        // still offered to a hold that opens again.
        assert_eq!(
            record(&mut checks, "h3", 30, Verdict::Passed, None),
            Recorded::Lifted
        );
        assert_eq!(checks.repair(X, KEY), Some(&filed("DEMO-7")));
        assert!(checks.repairs().is_empty());
        assert_eq!(checks.held_for("DEMO-7").count(), 0);
        assert_eq!(checks.reusable(X, KEY, &install), ["DEMO-7"]);
        assert_eq!(checks.last_passed(KEY), Some(X));
        assert_eq!(checks.last_passed("other"), None);
        // A refusal keeps no ticket.
        assert!(checks.set_repair("y", KEY, Repair::Unfiled));
        assert_eq!(checks.repair("y", KEY).and_then(Repair::ticket), None);
    }

    /// OWL-639: a break keeps its repairs apart from its entries, files two
    /// and then none, is told once and once more when spent, and ends only
    /// when a check of the default branch's head itself passes.
    #[test]
    fn a_break_files_two_repairs_then_none_until_the_head_passes() {
        let install = setup("install");
        let lint = setup("lint");
        let mut checks = Checks::default();
        assert!(checks.break_repairs(&install).is_empty());
        assert!(checks.claim_break_told(&install));
        assert!(!checks.claim_break_told(&install), "told once a break");
        assert_eq!(checks.keep_break_repair(&install, "DEMO-2"), 1);
        assert_eq!(
            checks.keep_break_repair(&install, "DEMO-2"),
            1,
            "taken again"
        );
        assert!(!checks.break_spent(&install));
        assert_eq!(checks.keep_break_repair(&install, "DEMO-3"), 2);
        assert!(checks.break_spent(&install));
        assert!(!checks.break_spent(&lint), "another command, another break");
        assert!(checks.claim_spent_told(&install));
        assert!(!checks.claim_spent_told(&install));
        // Neither a pass at an older commit nor dropped entries end it.
        checks.record_check(("old", KEY, "h1"), 1, Verdict::Passed, None);
        for n in 0..=MAX_CHECKS {
            let commit = format!("c{n}");
            checks.record_check((&commit, KEY, "h1"), 2, failed(&install), Some(&install));
        }
        assert_eq!(checks.break_repairs(&install), ["DEMO-2", "DEMO-3"]);
        // A pass at the head it fetched ends every break.
        checks.record_check(("h2", KEY, "h2"), 3, Verdict::Passed, None);
        assert!(checks.breaks.is_empty());
        assert!(checks.claim_break_told(&install), "a new break is told");
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

    /// OWL-642: a break's repair claim names the repair every runner takes
    /// again while it is open; one naming none holds the filing for its
    /// lease, whoever took it, and is taken over after it or when dated past
    /// it ahead; a closed one is replaced, the repairs counted on both sides
    /// bounding the break, unless a pass since ended the break it counts.
    #[test]
    fn a_repair_claim_is_taken_again_waited_on_or_replaced_within_the_bound() {
        const NOW: i64 = 100 * REPAIR_CLAIM_LEASE_MS;
        let seen = |ticket: Option<(&str, bool)>, claimed_ms: i64, repairs: u32| ClaimSeen {
            ticket: ticket.map(|(id, open)| (id.to_owned(), open)),
            claimed_ms,
            repairs,
        };
        assert_eq!(
            claim_move(None, NOW, 0, false),
            ClaimMove::File { before: 0 }
        );
        assert_eq!(
            claim_move(None, NOW, 2, false),
            ClaimMove::Spent { before: 2 }
        );
        let open = seen(Some(("DEMO-3", true)), 0, 2);
        assert_eq!(
            claim_move(Some(&open), NOW, 0, true),
            ClaimMove::TakeAgain("DEMO-3".to_owned())
        );
        // A filing under way, within its lease either side of the clock.
        for claimed in [
            NOW,
            NOW - REPAIR_CLAIM_LEASE_MS + 1,
            NOW + REPAIR_CLAIM_LEASE_MS,
        ] {
            let filing = seen(None, claimed, 1);
            assert_eq!(claim_move(Some(&filing), NOW, 0, true), ClaimMove::Wait);
        }
        // Stale, or dated too far ahead: taken over, its repair counted.
        for claimed in [NOW - REPAIR_CLAIM_LEASE_MS, NOW + REPAIR_CLAIM_LEASE_MS + 1] {
            let stale = seen(None, claimed, 1);
            assert_eq!(
                claim_move(Some(&stale), NOW, 0, false),
                ClaimMove::File { before: 1 }
            );
        }
        // A closed repair: the break's count is the larger side's.
        let closed = seen(Some(("DEMO-3", false)), 0, 1);
        assert_eq!(
            claim_move(Some(&closed), NOW, 0, false),
            ClaimMove::File { before: 1 }
        );
        let spent = seen(Some(("DEMO-4", false)), 0, 2);
        assert_eq!(
            claim_move(Some(&spent), NOW, 0, false),
            ClaimMove::Spent { before: 2 }
        );
        assert_eq!(
            claim_move(Some(&closed), NOW, 2, false),
            ClaimMove::Spent { before: 2 }
        );
        // A pass recorded since the claim ended the break it counts.
        assert_eq!(
            claim_move(Some(&spent), NOW, 0, true),
            ClaimMove::File { before: 0 }
        );
        let mut checks = Checks::default();
        checks.record_check(("c", KEY, "h"), 5, Verdict::Passed, None);
        assert!(!checks.passed_after(KEY, 4), "not the head it fetched");
        checks.record_check(("h", KEY, "h"), 5, Verdict::Passed, None);
        assert!(checks.passed_after(KEY, 4));
        assert!(!checks.passed_after(KEY, 5));
        assert!(!checks.passed_after("other", 4));
    }
}
