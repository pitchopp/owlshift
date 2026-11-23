//! A broken base (OWL-418; architecture section 5, "Parallel", "A broken
//! base"): a Build at Build configured at a commit whose gate is broken
//! waits before it starts ([`OnDemand::before_base`]); a Build run whose gate
//! or setup fails on a command the base fails too counts nothing and holds
//! that commit ([`OnDemand::base_broken`]); and a done Build whose gate was
//! held runs that gate again alone once the base passes, no harness spawned
//! ([`OnDemand::regate_role`]). The base is checked alone, as `owlshift
//! gate` runs, through [`GateOnce`] at its base place, which keeps the
//! verdict under the project's lock; a check that cannot run yet, another
//! ticket's step being in flight, leaves the failure pending. A failure a
//! check says nothing of, a runner's fault or one around the commands, takes
//! today's path with a `warning` event (`base_unchecked`).
//!
//! A held Build whose branch has commits of its own leaves its held commit
//! once the default branch's head passes (OWL-427, architecture section 5,
//! "Leaving a broken commit"): one `rebase` role run onto that head in the
//! Build's own worktree, its gate empty, nothing pushed
//! ([`OnDemand::leave_held`]); a rebase the runner does not accept moves the
//! branch back to where it was and parks the ticket.
//!
//! Before any base check, a done Build's red gate the machine cut, its sleep
//! or the network out (OWL-480, "The machine's"), counts nothing and keeps
//! the Build's result with no failure, so that its next step runs only its
//! gate, no check owed ([`OnDemand::machine_cut`]). A fix run's at Watch
//! that the machine cut likewise counts nothing and keeps its result, with
//! the pull request's head it fixes, for the next fix read on that head to
//! run only its gate (OWL-486, [`WaitAt::Fix`]); a rebase run's too, its
//! rebased commit kept while its branch goes back to that head, for the
//! next rebase onto the same base to gate that commit alone (OWL-490,
//! [`WaitAt::Rebase`]). Watch runs no base check.

use owlshift_contracts::base::{BaseWait, KeptDone, KeptFailing};
use owlshift_contracts::config::Admit;
use owlshift_core::base::{
    After, Before, Entry, Failing, HeldBranch, Leave, Recorded, Repair, Verdict,
};

use super::*;
use crate::base::{self, WaitAt};
use crate::gate_once::{self, GateOnce, Place};
use crate::notify;
use crate::ticket_ref::text_sha256;

/// How a Build at Build starts once its base is read (OWL-418, OWL-427).
pub(super) enum Resume {
    /// It runs as dispatched, a done Build's kept result given when a hold
    /// kept one, for its gate to run again alone.
    Go(Option<KeptDone>),
    /// Its branch was rebased off its held commit onto the default branch's
    /// head, which passes: configured there now, with a done Build's kept
    /// result, and what the rebase run proposed.
    Rebased {
        dispatched: Box<Dispatched>,
        done: Option<KeptDone>,
        followups: Vec<Followup>,
    },
}

/// What a base check run from a ticket's step came to.
enum CheckRun {
    /// It ran, or its verdict was kept at the head already: the verdict,
    /// `None` when the run said nothing of the commands, and the default
    /// branch's head it was checked at.
    Ran {
        verdict: Option<Verdict>,
        head: String,
    },
    /// Another ticket's step is in flight.
    Busy,
    /// It could not run, said in a `base_unchecked` warning.
    Unchecked,
    /// The commit has no gate command.
    NoGate,
}

/// What a Build configured at a commit does, once its base checks are read
/// and, when due, made.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Settled {
    /// Nothing holds it: it starts, or its failure is its branch's.
    Go,
    /// The commit is held: the failing command, when known.
    Held { command: Option<String> },
    /// A check is due that could not run yet.
    Pending { command: Option<String> },
    /// The commit is held and its check again is due but could not run
    /// yet (OWL-431): the failing command.
    Due { command: Option<String> },
}

/// What a Build's branch is rebased off ([`OnDemand::rebase_held`]).
#[derive(Clone, Copy, Debug)]
enum Off<'a> {
    /// A held commit, which fails its gate at `failed` when known (OWL-427).
    Held {
        commit: &'a str,
        failed: Option<&'a str>,
    },
    /// The commit where a yielded ticket's branch parts from the base, which
    /// has since changed what its overlaps reach (OWL-621).
    Yielded { from: &'a str },
    /// The commit where the branch of a Build that stopped `blocked` parts
    /// from the base, which the resolver's `rebase` verdict moves it off
    /// (OWL-620).
    Unblocked { from: &'a str },
}

/// A Build moved onto the base this command fetched (OWL-620): dispatched
/// again there, with what the rebase run proposed.
pub(super) type Moved = (Box<Dispatched>, Vec<Followup>);

/// Why a Build's branch moves onto the base this command fetched before its
/// next run ([`OnDemand::onto_base`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Onto {
    /// A yielded ticket admitted again (OWL-621).
    Yielded,
    /// The resolver's `rebase` verdict on a `blocked` (OWL-620).
    Unblocked,
}

impl Onto {
    /// The key its events name it by.
    fn key(self) -> &'static str {
        match self {
            Self::Yielded => "yield",
            Self::Unblocked => "unblocked",
        }
    }
}

/// Why no repair ticket was kept for a hold (OWL-622), each a
/// `repair_unfiled` warning.
enum NotKept {
    /// None was filed: tried for this hold, not again until it opens again.
    Unfiled(String),
    /// An open one was found whose text could not be read: the hold stays
    /// untried, and its next step tries again.
    Unread(String),
}

/// Why a hold's repair is filed ([`OnDemand::file_repair`]).
enum Filing {
    /// The hold opened now.
    Opened,
    /// No repair was tried for it yet: its first try found the project's
    /// lock taken, or an open repair it could not read.
    Untried,
    /// It stands on the default branch's head, and the repair it holds, this
    /// ticket, is completed on the tracker: a repair merged with the base
    /// still failing, whose completion came after the hold took it again
    /// (OWL-639).
    Outlived(String),
}

/// A repair kept for a hold ([`OnDemand::repair_for`]).
struct Kept {
    ticket: TicketId,
    /// The hash of its title and description as the tracker answers them.
    text: String,
    /// An open one taken again, not filed now.
    reused: bool,
    /// What went wrong once it was filed.
    problems: Vec<tracker::Error>,
}

/// What a held Build does about the default branch's newer head (OWL-427).
enum Left {
    /// It stays held; `due` when the head's check could not run yet
    /// (OWL-431).
    Stay { due: bool },
    /// Its branch was rebased onto the head: dispatched again there, with
    /// what the rebase run proposed.
    Rebased(Box<Dispatched>, Vec<Followup>),
}

impl OnDemand<'_> {
    /// Before a Build's first run of `command` (OWL-418): held when the
    /// commit it is configured at is held, or a check of it is due and
    /// cannot run yet; otherwise the done Build whose gate a hold kept, for
    /// that gate to run again alone ([`Self::regate_role`]). A failure that
    /// waited for its check with no done Build behind it lets the Build run.
    /// A held branch with commits of its own may leave its held commit
    /// first, rebased onto a newer head that passes ([`Self::leave_held`],
    /// OWL-427).
    pub(super) fn before_base(
        &self,
        p: &mut Prepared,
        command: Command,
        dispatched: &Dispatched,
        state: &TicketState,
        sink: &mut EventSink<'_>,
    ) -> Result<Resume, Stop> {
        if state.status() != Status::Active(Stage::Build) || p.repairing {
            return Ok(Resume::Go(None));
        }
        // The repair of a broken base is held by no hold that names it,
        // whatever commit it is configured at: its work is to make that
        // command pass (OWL-622).
        if let Some(failing) = self.repair_of(&p.ticket) {
            p.repair_hold = Some(failing);
            self.drop_wait(&p.ticket, sink);
            return Ok(Resume::Go(None));
        }
        let moved = self.moved_off_hold(p, dispatched, state);
        let wait = self.valid_wait(p, dispatched, state, WaitAt::Build);
        // A wait the machine's cut left has no failure: no check is owed it
        // (OWL-480), only the holds standing on the commit.
        let failing = wait
            .as_ref()
            .and_then(|wait| wait.failing.as_ref())
            .map(KeptFailing::failing)
            .or(moved);
        match self.settle(p, dispatched, failing.as_ref(), sink)? {
            Settled::Go => {
                // A yielded ticket admitted again is rebased first onto what
                // the tickets that took its paths merged (OWL-621), a done
                // Build's result kept at the rebased head, as below.
                if let Left::Rebased(rebased, followups) =
                    self.after_yield(p, command, dispatched, state, sink)?
                {
                    let done = wait.and_then(|wait| {
                        let done = wait.done?;
                        self.keep_wait(
                            p,
                            &rebased,
                            WaitAt::Build,
                            state.round(),
                            (wait.failing, Some(done.clone())),
                            None,
                            sink,
                        );
                        Some(done)
                    });
                    if done.is_none() {
                        self.drop_wait(&p.ticket, sink);
                    }
                    return Ok(Resume::Rebased {
                        dispatched: rebased,
                        done,
                        followups,
                    });
                }
                // A done Build's result stays kept until its gate has run
                // again ([`Self::regate_role`]), so a checked stop before
                // then leaves it for the next start (OWL-161).
                let done = wait.and_then(|wait| wait.done);
                if done.is_none() {
                    self.drop_wait(&p.ticket, sink);
                }
                Ok(Resume::Go(done))
            }
            Settled::Held { command: failed } => {
                match self.leave_held(p, command, dispatched, state, failed.as_deref(), sink)? {
                    Left::Rebased(rebased, followups) => {
                        // A done Build's result is kept at the rebased head
                        // and the commit the ticket is configured at now, so
                        // a stop before its regate still finds it; a held
                        // failure with none behind it is spent.
                        let done = wait.and_then(|wait| {
                            let done = wait.done?;
                            self.keep_wait(
                                p,
                                &rebased,
                                WaitAt::Build,
                                state.round(),
                                (wait.failing, Some(done.clone())),
                                None,
                                sink,
                            );
                            Some(done)
                        });
                        if done.is_none() {
                            self.drop_wait(&p.ticket, sink);
                        }
                        Ok(Resume::Rebased {
                            dispatched: rebased,
                            done,
                            followups,
                        })
                    }
                    Left::Stay { due } => {
                        Err(self.base_held(p, dispatched, failed, false, due, sink))
                    }
                }
            }
            Settled::Pending { command: failed } => {
                Err(self.base_held(p, dispatched, failed, true, false, sink))
            }
            Settled::Due { command: failed } => {
                Err(self.base_held(p, dispatched, failed, false, true, sink))
            }
        }
    }

    /// The failing command a Build moved off its held commit brings to the
    /// commit it is configured at now (OWL-639): watch's step into Build
    /// fast-forwards a branch with no commits of its own (OWL-274), and the
    /// ticket's wait, kept at the held commit, says what it waited for.
    /// Settled there as the ticket's failure, that commit is checked before
    /// the Build starts: a pass of the head ends the break, a failure at the
    /// same command holds it. `None` for any other wait.
    fn moved_off_hold(
        &self,
        p: &Prepared,
        dispatched: &Dispatched,
        state: &TicketState,
    ) -> Option<Failing> {
        let wait = base::read_wait(self.dirs, &p.ticket)?;
        let at = dispatched.configured_at.commit.as_str();
        if wait.round != state.round() || wait.pushed.is_some() || wait.configured_at == at {
            return None;
        }
        let failing = wait.failing.as_ref()?.failing();
        let held = base::read(self.dirs)
            .ok()?
            .holds_commit(&wait.configured_at);
        (held && self.is_ancestor(p, &wait.configured_at, at)).then_some(failing)
    }

    /// A Build held at the commit `dispatched` is configured at, whose
    /// check still fails at the default branch's head (OWL-427): the head,
    /// when it contains that commit, is checked whatever the branch, matched
    /// with the held commit's failing command, so that a head still failing
    /// there is held and files its repair (OWL-639); a hold standing on it is
    /// followed ([`Self::follow_hold`]). When the branch has commits of its
    /// own and a clean worktree, no pull request is open, and the head
    /// passes its own check, one `rebase` role run moves the branch onto the
    /// head ([`Self::rebase_held`]), and the ticket is dispatched again for
    /// `command`, configured there. [`Left::Stay`] when it stays held, `due`
    /// when the head's check could not run beside the other steps; `failed`
    /// is the command the held commit fails, for a park's reason.
    fn leave_held(
        &self,
        p: &mut Prepared,
        command: Command,
        dispatched: &Dispatched,
        state: &TicketState,
        failed: Option<&str>,
        sink: &mut EventSink<'_>,
    ) -> Result<Left, Stop> {
        let stay = || Ok(Left::Stay { due: false });
        let held = dispatched.configured_at.commit.clone();
        let head = p.base.commit.clone();
        if head == held || !self.is_ancestor(p, &held, &head) {
            return stay();
        }
        let (branch, start) = self.held_branch(p, &head)?;
        if branch == HeldBranch::Uncommitted {
            sink.emit(
                &p.ticket,
                None,
                EventKind::Warning,
                data([
                    ("what", json!("base_not_rebased")),
                    ("commit", json!(held)),
                    ("onto", json!(head)),
                    ("reason", json!(start)),
                ]),
            );
        }
        let checks = match base::read(self.dirs) {
            Ok(checks) => checks,
            Err(why) => {
                self.base_unchecked(&p.ticket, &why, sink);
                return stay();
            }
        };
        // The held commit checked again at this head first, still failing:
        // a check that could not run yet leaves an environment fix possible,
        // which lifts the hold with no rebase.
        let Some(Verdict::Failed(failure)) =
            checks.verdict_at(&held, &commands_key(dispatched), &head)
        else {
            return stay();
        };
        let failing = failure.failing.clone();
        // The head's own setup and gate, which its check runs and the
        // rebase run is briefed with (OWL-331: a rebase takes the base it
        // rebases onto).
        let configured = self.dispatch_rules(p, Command::Rebase, sink)?;
        let key = base::commands_key(configured.rules.setup(), configured.rules.gate());
        let kept = checks.verdict_at(&head, &key, &head).cloned();
        let head_held = checks.holds(&head, &key);
        let decide = |verdict: Option<&Verdict>, head_held: bool| {
            owlshift_core::base::leave(&held, &failing, &head, verdict, true, head_held, branch)
        };
        let verdict = match decide(kept.as_ref(), head_held) {
            Leave::Stay => {
                if head_held {
                    self.follow_hold(p, &head, &key, sink);
                }
                return stay();
            }
            Leave::Rebase => Verdict::Passed,
            Leave::CheckHead => match self.run_check(p, &head, Some(&failing), sink)? {
                CheckRun::Ran {
                    verdict: Some(verdict),
                    ..
                } => {
                    self.not_followed(p, (&held, &head), &failing, &verdict, sink);
                    verdict
                }
                // Given the priority a first check gets (OWL-431).
                CheckRun::Busy => return Ok(Left::Stay { due: true }),
                CheckRun::Ran { verdict: None, .. } | CheckRun::Unchecked | CheckRun::NoGate => {
                    return stay();
                }
            },
        };
        // A failed verdict never rebases: matched, the check held the head.
        if decide(Some(&verdict), true) != Leave::Rebase {
            return stay();
        }
        let followups = self.rebase_held(
            p,
            state,
            Off::Held {
                commit: &held,
                failed,
            },
            &configured.rules,
            &start,
            dispatched,
            sink,
        )?;
        // Its merge-base with the base is the head now: configured there.
        let rebased = self.dispatch(p, command, sink)?;
        // Onto the head: what a yield's rebase would have done (OWL-621).
        self.yield_rebase_decided(p, state)?;
        Ok(Left::Rebased(Box::new(rebased), followups))
    }

    /// A yielded ticket admitted again whose branch's rebase is due
    /// ([`TicketHeld::rebase_due`], OWL-621; architecture section 5, "A
    /// stopped holder yields", "The resume"): decided at this Build step,
    /// then no longer due. When the base this command fetched changed what
    /// its overlaps reach ([`held::Reach`]), and under remote claims what it
    /// holds itself ([`held::resume_reach`]), since its branch parted from
    /// it, a branch with commits of its own and a clean worktree is rebased
    /// onto that base by one `rebase` role run, as a held Build's is
    /// ([`Self::rebase_held`], its brief `yielded`), and one with none is
    /// fast-forwarded ([`Self::start_from_base`]); either is dispatched again
    /// there, given back with what the run proposed. A worktree with
    /// uncommitted paths is not rebased, a `warning` (`yield_not_rebased`)
    /// naming them; a delivered ticket is Watch's refresh's. A base git
    /// cannot read leaves it due, for the next Build step.
    fn after_yield(
        &self,
        p: &mut Prepared,
        command: Command,
        dispatched: &Dispatched,
        state: &TicketState,
        sink: &mut EventSink<'_>,
    ) -> Result<Left, Stop> {
        let stay = Left::Stay { due: false };
        if !p.held.as_ref().is_some_and(|held| held.rebase_due) {
            return Ok(stay);
        }
        let head = p.base.commit.clone();
        // Under remote claims its own set too: another runner's taking
        // records no overlap here (OWL-621).
        let named = self.config.stack.named_resources().unwrap_or_default();
        let reach = held::resume_reach(
            &p.ticket,
            p.held.as_ref(),
            &self.sharings(p),
            &named,
            p.claims,
        );
        let git = self.executor.git.clone();
        let checkout = p.checkout.clone();
        let moved = |start: &str| held::base_moved_on(&git, &checkout, start, None, &head, &reach);
        let Some(rebased) =
            self.onto_base(p, command, dispatched, state, Onto::Yielded, &moved, sink)?
        else {
            return Ok(stay);
        };
        self.yield_rebase_decided(p, state)?;
        Ok(rebased)
    }

    /// The ticket's branch moved onto the base this command fetched before
    /// its Build's next run, `why` saying for what (OWL-621, OWL-620), when
    /// `moved` reads that the base moved on what matters since the branch's
    /// head: a branch with commits of its own and a clean worktree rebased
    /// by one `rebase` role run ([`Self::rebase_held`]), one with none
    /// fast-forwarded ([`Self::start_from_base`]), either dispatched again
    /// there and given back with what the run proposed. A worktree with
    /// uncommitted paths is not moved, a `warning` (`<why>_not_rebased`)
    /// naming them; a delivered ticket is Watch's. `None` when `moved` could
    /// not read the base.
    #[allow(clippy::too_many_arguments)]
    fn onto_base(
        &self,
        p: &mut Prepared,
        command: Command,
        dispatched: &Dispatched,
        state: &TicketState,
        why: Onto,
        moved: &dyn Fn(&str) -> Result<bool, String>,
        sink: &mut EventSink<'_>,
    ) -> Result<Option<Left>, Stop> {
        let stay = Left::Stay { due: false };
        let head = p.base.commit.clone();
        let (branch, start) = self.held_branch(p, &head)?;
        let rebased = match branch {
            HeldBranch::Delivered => stay,
            HeldBranch::NoCommitsOfItsOwn if start.is_empty() => stay,
            HeldBranch::Uncommitted => {
                sink.emit(
                    &p.ticket,
                    None,
                    EventKind::Warning,
                    data([
                        ("what", json!(format!("{}_not_rebased", why.key()))),
                        ("onto", json!(head)),
                        ("reason", json!(start)),
                    ]),
                );
                stay
            }
            HeldBranch::NoCommitsOfItsOwn => match moved(&start) {
                Err(_) => return Ok(None),
                Ok(false) => stay,
                Ok(true) => {
                    let section = self.section()?;
                    self.start_from_base(p)?;
                    drop(section);
                    sink.emit(
                        &p.ticket,
                        None,
                        EventKind::Decision,
                        data([
                            (why.key(), json!("rebase")),
                            ("from", json!(start)),
                            ("onto", json!(head)),
                            ("fast_forward", json!(true)),
                        ]),
                    );
                    Left::Rebased(Box::new(self.dispatch(p, command, sink)?), Vec::new())
                }
            },
            HeldBranch::OwnCommits => match moved(&start) {
                Err(_) => return Ok(None),
                Ok(false) => stay,
                Ok(true) => {
                    let from = self.merge_base(p, &start, &head).unwrap_or_default();
                    let configured = self.dispatch_rules(p, Command::Rebase, sink)?;
                    let off = match why {
                        Onto::Yielded => Off::Yielded { from: &from },
                        Onto::Unblocked => Off::Unblocked { from: &from },
                    };
                    let followups = self.rebase_held(
                        p,
                        state,
                        off,
                        &configured.rules,
                        &start,
                        dispatched,
                        sink,
                    )?;
                    Left::Rebased(Box::new(self.dispatch(p, command, sink)?), followups)
                }
            },
        };
        Ok(Some(rebased))
    }

    /// Whether the `rebase` verdict is open on a Build at Build that stopped
    /// `blocked` (OWL-620): outside a review loop, its branch not delivered,
    /// with a clean worktree, and the base's head, as this command fetched
    /// it, not in the branch. A branch git cannot read is not rebased.
    pub(super) fn unblock_rebase_open(&self, p: &Prepared) -> bool {
        if in_review_loop(p) {
            return false;
        }
        let head = p.base.commit.as_str();
        match self.held_branch(p, head) {
            Ok((HeldBranch::OwnCommits | HeldBranch::NoCommitsOfItsOwn, start)) => {
                !start.is_empty() && !self.is_ancestor(p, head, &start)
            }
            Ok((HeldBranch::Delivered | HeldBranch::Uncommitted, _)) | Err(_) => false,
        }
    }

    /// The `rebase` verdict's act on a Build at Build that stopped `blocked`
    /// (OWL-620): its branch moved onto the base this command fetched, as
    /// [`Self::onto_base`] moves it, the base's head not in the branch being
    /// what moved; the Build dispatched again there, with what the rebase
    /// run proposed. `None` when the branch stays where it was.
    pub(super) fn unblock_rebase(
        &self,
        p: &mut Prepared,
        command: Command,
        dispatched: &Dispatched,
        state: &TicketState,
        sink: &mut EventSink<'_>,
    ) -> Result<Option<Moved>, Stop> {
        let head = p.base.commit.clone();
        let git = self.executor.git.clone();
        let checkout = p.checkout.clone();
        let moved = |start: &str| {
            git.output(
                &checkout,
                &[
                    "merge-base",
                    "--is-ancestor",
                    "--end-of-options",
                    &head,
                    start,
                ],
                None,
            )
            .map(|out| !out.success())
            .map_err(|e| e.to_string())
        };
        Ok(
            match self.onto_base(p, command, dispatched, state, Onto::Unblocked, &moved, sink)? {
                Some(Left::Rebased(rebased, followups)) => Some((rebased, followups)),
                Some(Left::Stay { .. }) | None => None,
            },
        )
    }

    /// A yielded ticket's rebase on resume decided (OWL-621): no longer due,
    /// kept.
    fn yield_rebase_decided(&self, p: &mut Prepared, state: &TicketState) -> Result<(), Stop> {
        match p.held.as_mut() {
            Some(held) if held.rebase_due => held.rebase_due = false,
            _ => return Ok(()),
        }
        self.keep(p, state)
    }

    /// The commit where `start` and `head` part, in the dedicated
    /// checkout; `None` when git cannot say.
    fn merge_base(&self, p: &Prepared, start: &str, head: &str) -> Option<String> {
        self.executor
            .git
            .output(
                &p.checkout,
                &["merge-base", "--end-of-options", start, head],
                None,
            )
            .ok()
            .filter(|out| out.success())
            .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_owned())
    }

    /// The held ticket's branch as [`owlshift_core::base::leave`] reads it,
    /// against `head`, with the branch's head; for a worktree with
    /// uncommitted paths, why it is not rebased instead, naming them.
    fn held_branch(&self, p: &Prepared, head: &str) -> Result<(HeldBranch, String), Stop> {
        if p.stored.as_ref().is_some_and(drain::delivery_followed) {
            return Ok((HeldBranch::Delivered, String::new()));
        }
        let Some(start) = self.branch_head(p) else {
            return Ok((HeldBranch::NoCommitsOfItsOwn, String::new()));
        };
        if self.is_ancestor(p, &start, head) {
            return Ok((HeldBranch::NoCommitsOfItsOwn, start));
        }
        if fs::symlink_metadata(&p.worktree).is_ok() {
            let dirty = self
                .executor
                .uncommitted(&p.worktree)
                .map_err(|e| refused("reading the ticket's worktree before a rebase", e))?;
            if !dirty.is_empty() {
                return Ok((
                    HeldBranch::Uncommitted,
                    format!("its worktree has uncommitted changes: {dirty}"),
                ));
            }
        }
        Ok((HeldBranch::OwnCommits, start))
    }

    /// Whether `ancestor` is `of` or an ancestor of it, in the dedicated
    /// checkout; `false` when git cannot say.
    fn is_ancestor(&self, p: &Prepared, ancestor: &str, of: &str) -> bool {
        self.executor
            .git
            .output(
                &p.checkout,
                &[
                    "merge-base",
                    "--is-ancestor",
                    "--end-of-options",
                    ancestor,
                    of,
                ],
                None,
            )
            .is_ok_and(|out| out.success())
    }

    /// One `rebase` role run moving the ticket's branch, at `start`, off
    /// `held` (whose failing command is given) onto the default branch's
    /// head (OWL-427): on the fast tier when the merge is trivial, in the
    /// Build's own worktree, briefed with `broken` and no pull request, its
    /// gate empty and no setup, so the gate after its `done` only checks
    /// that everything is committed on the branch. A `done` whose commit
    /// [`rebase_keeps_delivery`] accepts is the branch now; its follow-ups
    /// are given back. A breach parks as any run's; any other end first
    /// moves the branch back to `start` when the run moved it, then stops
    /// at a usage limit, or parks ([`Event::RebaseFailed`]). Nothing is
    /// pushed.
    #[allow(clippy::too_many_arguments)]
    fn rebase_held(
        &self,
        p: &mut Prepared,
        state: &TicketState,
        off: Off<'_>,
        rules: &ProjectRules,
        start: &str,
        dispatched: &Dispatched,
        sink: &mut EventSink<'_>,
    ) -> Result<Vec<Followup>, Stop> {
        let git = &self.executor.git;
        let onto = p.base.commit.clone();
        let classified = rebase::classify(git, &p.checkout, &onto, start, false);
        let fast = classified.trivial;
        let harness = self.rebase_harness(fast, dispatched);
        let (key, from) = match off {
            Off::Held { commit, .. } => ("base", commit.to_owned()),
            // OWL-621: from the commit the branch parted from the base.
            Off::Yielded { from } => ("yield", from.to_owned()),
            // OWL-620: the same, on the resolver's `rebase` verdict.
            Off::Unblocked { from } => ("unblocked", from.to_owned()),
        };
        sink.emit(
            &p.ticket,
            None,
            EventKind::Decision,
            data([
                (key, json!("rebase")),
                ("from", json!(from)),
                ("onto", json!(onto)),
                ("trivial", json!(classified.trivial)),
                ("tier", json!(rebase_tier(fast))),
                ("conflicting", json!(classified.paths)),
            ]),
        );
        let current = person(p.current()?, &p.found);
        let comments = self.comments(&p.ticket)?;
        let mut brief = self.brief(p, Role::Rebase, &comments, rules, None, &current);
        // The Build's gate judges the rebased branch after this run.
        brief.gate = Vec::new();
        brief.rebase = Some(RebaseBrief {
            pull_request: None,
            url: None,
            branch: p.branch.clone(),
            head: start.to_owned(),
            base_branch: p.base.branch.clone(),
            onto: onto.clone(),
            refresh: false,
            conflicting: classified.paths.clone(),
            broken: match off {
                Off::Held { commit, .. } => Some(commit.to_owned()),
                Off::Yielded { .. } | Off::Unblocked { .. } => None,
            },
            yielded: matches!(off, Off::Yielded { .. }),
            unblocked: matches!(off, Off::Unblocked { .. }),
        });
        let mut gathered = Gathered::default();
        let (ran, event) =
            self.run_role_once(p, (&*harness, &[]), state, &brief, 1, &mut gathered, sink)?;
        let why = match event {
            Event::Quarantined => {
                return match state.apply(p.pipeline, Event::Quarantined) {
                    Ok(Transition::Parked {
                        state: parked,
                        reason,
                    }) => Err(self.park(p, &parked, reason, &ran, None, Vec::new(), sink)),
                    other => Err(core_error(other)),
                };
            }
            Event::Completed => match rebased_commit(&ran) {
                Some(commit) => match (CommitId::new(start), CommitId::new(onto.as_str())) {
                    (Ok(start), Ok(onto)) => {
                        match rebase_keeps_delivery(git, &p.checkout, &start, &onto, &commit) {
                            Ok(()) => return Ok(gathered.followups),
                            Err(why) => why,
                        }
                    }
                    _ => "the branch's head or the base is no commit id".to_owned(),
                },
                None => "the rebase run ended done without a committed rebase".to_owned(),
            },
            _ => rebase_said(&ran),
        };
        // The branch where it was before the run, its worktree's plan and
        // ledger kept, before anything else is said of it.
        self.restore_branch(p, start)?;
        if let Some(stop) = interruption(&ran.report.outcome) {
            return Err(stop);
        }
        let parked = match state.apply(p.pipeline, Event::RebaseFailed) {
            Ok(Transition::Parked { state: parked, .. }) => parked,
            other => return Err(core_error(other)),
        };
        let detail = match off {
            Off::Held { commit, failed } => {
                let failing =
                    failed.map_or_else(|| "its gate".to_owned(), |command| format!("`{command}`"));
                format!(
                    "{ticket}'s branch was made from {held_short}, which fails {failing}; \
                     {base}'s head {onto_short} passes, and the rebase onto it was not taken: \
                     {why}. The branch is left at {start_short}. Say on the ticket what the \
                     rebase needs, if anything, then `owlshift continue {ticket}` runs it again.",
                    ticket = p.ticket,
                    held_short = base::short(commit),
                    base = p.base.branch,
                    onto_short = base::short(&onto),
                    start_short = base::short(start),
                )
            }
            Off::Yielded { from } => format!(
                "{ticket} yielded what it held and was admitted again; {base}'s head \
                 {onto_short} changed what the tickets that took it share with it since its \
                 branch left {from_short}, and the rebase onto it was not taken: {why}. The \
                 branch is left at {start_short}. Say on the ticket what the rebase needs, if \
                 anything, then `owlshift continue {ticket}` runs it again.",
                ticket = p.ticket,
                base = p.base.branch,
                onto_short = base::short(&onto),
                from_short = base::short(from),
                start_short = base::short(start),
            ),
            Off::Unblocked { from } => format!(
                "{ticket}'s Build stopped blocked, and the resolver gave its branch to the \
                 rebase run; the rebase onto {base}'s head {onto_short}, from {from_short}, was \
                 not taken: {why}. The branch is left at {start_short}. Say on the ticket what \
                 the rebase needs, if anything, then `owlshift continue {ticket}` runs the Build \
                 again.",
                ticket = p.ticket,
                base = p.base.branch,
                onto_short = base::short(&onto),
                from_short = base::short(from),
                start_short = base::short(start),
            ),
        };
        Err(self.park_with(
            p,
            &parked,
            ParkReason::Rebase,
            Some(&ran.run),
            detail,
            None,
            Vec::new(),
            sink,
        ))
    }

    /// Moves the ticket's branch back to `start` after a rebase run the
    /// runner did not take, when the run moved it (OWL-427), through its
    /// run file: `git reset --hard` in its worktree, which keeps the
    /// untracked plan and ledger.
    fn restore_branch(&self, p: &Prepared, start: &str) -> Result<(), Stop> {
        if self.branch_head(p).as_deref() == Some(start) {
            return Ok(());
        }
        let moved = ticket_run::move_branch(
            &self.executor.git,
            self.dirs,
            &p.checkout,
            &p._ticket_lock,
            &p.ticket,
            Some(start),
            || {
                self.executor
                    .restore_branch(&p.checkout, &p.worktree, &p.branch, start)
            },
        );
        match moved {
            Ok(restored) => restored.map_err(|e| {
                refused(
                    &format!("moving the ticket's branch back to {start} after its rebase run"),
                    e,
                )
            }),
            Err(fault) => Err(self.run_fault(&p.ticket, fault)),
        }
    }

    /// After a Build run at Build whose outcome counts as a failed run: when
    /// the machine cut its red gate ([`Self::machine_cut`], OWL-480), or it
    /// failed on a command its base fails too, or whose check is due and
    /// cannot run yet, the ticket stays as it is ([`Event::Interrupted`],
    /// [`Event::BaseBroken`]), what a done Build reported is kept for its
    /// gate to run again, and the command stops. `None` for any other
    /// failure, which takes today's path.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn base_broken(
        &self,
        p: &mut Prepared,
        dispatched: &Dispatched,
        state: &mut TicketState,
        ran: &Ran,
        gathered: &Gathered,
        comments: &[Comment],
        integrating: bool,
        sink: &mut EventSink<'_>,
    ) -> Result<Option<Stop>, Stop> {
        if state.status() != Status::Active(Stage::Build) || p.repairing {
            return Ok(None);
        }
        let done = self.kept_done(p, ran, gathered, comments, integrating);
        if let Some(cut) =
            self.machine_cut(p, dispatched, WaitAt::Build, state, ran, done.clone(), sink)?
        {
            return Ok(Some(cut));
        }
        // A repair's failure is its own branch's, never a hold (OWL-622).
        if p.repair_hold.is_some() {
            return Ok(None);
        }
        let Some(failing) = base::failing(&ran.report) else {
            return Ok(None);
        };
        let (command, pending) = match self.settle(p, dispatched, Some(&failing), sink)? {
            Settled::Go => return Ok(None),
            Settled::Held { command } => (command, (false, false)),
            Settled::Pending { command } => (command, (true, false)),
            Settled::Due { command } => (command, (false, true)),
        };
        match state.apply(p.pipeline, Event::BaseBroken) {
            Ok(Transition::To(next)) => *state = next,
            other => return Err(core_error(other)),
        }
        self.keep_wait(
            p,
            dispatched,
            WaitAt::Build,
            state.round(),
            (Some(KeptFailing::from(&failing)), done),
            Some(&ran.run),
            sink,
        );
        let (pending, due) = pending;
        Ok(Some(
            self.base_held(p, dispatched, command, pending, due, sink),
        ))
    }

    /// What a done Build reported, kept for its gate to run again alone:
    /// its result, the notes and follow-ups gathered for the delivery, the
    /// newest decider edit its brief held and whether it was the run given
    /// late comments. `None` for a run that reported no `done`.
    pub(super) fn kept_done(
        &self,
        p: &Prepared,
        ran: &Ran,
        gathered: &Gathered,
        comments: &[Comment],
        integrating: bool,
    ) -> Option<KeptDone> {
        let result = ran.report.gated.as_ref()?;
        let decider = p
            .questions()
            .latest()
            .map(|ask| person(&ask.decider, &p.found));
        Some(KeptDone {
            result: (**result).clone(),
            notes: gathered.decisions.clone(),
            followups: gathered.followups.clone(),
            seen: decider
                .as_ref()
                .and_then(|decider| answer_check::newest_decider_edit(comments, decider)),
            integrating,
        })
    }

    /// A Build's red gate the machine cut (OWL-480; architecture section 5,
    /// "A broken base", "The machine's"), read before any base check, or a
    /// fix run's (OWL-486) or a rebase run's (OWL-490) at Watch, read `at`
    /// that repair: a command that
    /// failed while the machine slept through the gate, or, the machine
    /// awake, one the tracker gives no answer about right after
    /// ([`base::cut`]). The ticket stays as it is ([`Event::Interrupted`]),
    /// the done run's result is kept in its wait file with no failure, so
    /// that no check is owed it and its next step runs only its gate
    /// ([`Self::regate_role`]), and the command stops
    /// [`Stop::MachineCut`]. `None` when the gate is not the machine's, or
    /// the result cannot be kept, which takes the base's check and today's
    /// path: a cut is never a run again for nothing counted.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn machine_cut(
        &self,
        p: &Prepared,
        dispatched: &Dispatched,
        at: WaitAt<'_>,
        state: &mut TicketState,
        ran: &Ran,
        done: Option<KeptDone>,
        sink: &mut EventSink<'_>,
    ) -> Result<Option<Stop>, Stop> {
        let Some(cut) = base::cut(&ran.report, || {
            matches!(
                self.tracker.comments(&p.ticket),
                Err(error) if error.kind == tracker::ErrorKind::Unreachable
            )
        }) else {
            return Ok(None);
        };
        if done.is_none()
            || !self.keep_wait(
                p,
                dispatched,
                at,
                state.round(),
                (None, done),
                Some(&ran.run),
                sink,
            )
        {
            return Ok(None);
        }
        match state.apply(p.pipeline, Event::Interrupted) {
            Ok(Transition::To(next)) => *state = next,
            other => return Err(core_error(other)),
        }
        sink.emit(
            &p.ticket,
            Some(&ran.run),
            EventKind::Decision,
            data([
                ("gate", json!("waits")),
                ("reason", json!("machine_cut")),
                ("command", json!(cut.failing.command)),
                (
                    "slept_ms",
                    json!(u64::try_from(cut.slept.as_millis()).unwrap_or(u64::MAX)),
                ),
                (
                    "tracker",
                    json!(if cut.unreachable {
                        "unreachable"
                    } else {
                        "unread"
                    }),
                ),
            ]),
        );
        Ok(Some(Stop::MachineCut {
            ticket: p.ticket.clone(),
            command: cut.failing.command,
            slept: cut.slept,
            unreachable: cut.unreachable,
        }))
    }

    /// Keeps the ticket's wait file for a run configured as `dispatched` in
    /// `round`, to be read `at` its stage, at its branch's head now: the held
    /// failure, none for a gate the machine cut, and a done run's result.
    /// For a rebase run's, that head is the rebased commit, read before the
    /// rebase's stop moves the branch back to the pull request's head
    /// (OWL-490). Whether it was kept: a branch with no head keeps none; a
    /// write that fails is a `warning` for `run`.
    #[allow(clippy::too_many_arguments)]
    fn keep_wait(
        &self,
        p: &Prepared,
        dispatched: &Dispatched,
        at: WaitAt<'_>,
        round: u32,
        (failing, done): (Option<KeptFailing>, Option<KeptDone>),
        run: Option<&str>,
        sink: &mut EventSink<'_>,
    ) -> bool {
        let Some(head) = self.branch_head(p) else {
            self.drop_wait(&p.ticket, sink);
            return false;
        };
        let wait = BaseWait {
            format: Format,
            ticket: p.ticket.clone(),
            configured_at: dispatched.configured_at.commit.clone(),
            commands: commands_key(dispatched),
            round,
            head,
            failing,
            done,
            pushed: at.pushed().map(str::to_owned),
            rebase: at.rebase(),
        };
        let Err(error) = base::write_wait(self.dirs, &p.ticket, &wait) else {
            return true;
        };
        sink.emit(
            &p.ticket,
            run,
            EventKind::Warning,
            data([
                ("what", json!("base_wait_not_kept")),
                ("reason", json!(error.to_string())),
            ]),
        );
        false
    }

    /// A done Build's gate run again alone, its kept result judged by it
    /// (OWL-418), or a fix run's (OWL-486) or a rebase run's (OWL-490) at
    /// Watch whose gate the machine cut: as [`Self::run_role_once`], with no budget check, no
    /// session, no usage admission and no cost, since no harness runs. The
    /// wait is spent whatever the gate says, a red gate classified again as
    /// any Build run's, but for a checked stop (OWL-161): the gate stopped
    /// or not started, the wait is kept for the next start.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn regate_role(
        &self,
        p: &mut Prepared,
        (harness, setup): (&dyn Harness, &[String]),
        brief: &Brief,
        attempt: u32,
        kept: &RunResult,
        gathered: &mut Gathered,
        sink: &mut EventSink<'_>,
    ) -> Result<(Ran, Event), Stop> {
        let executed = self.execute_inner(
            p,
            self.executor,
            (harness, setup),
            brief,
            attempt,
            (None, Some(kept)),
            sink,
        );
        let stopped = match &executed {
            Err(stop) => matches!(stop, Stop::Stopped),
            Ok(Ok(ran)) => matches!(ran.report.outcome, Outcome::Stopped),
            Ok(Err(_)) => false,
        };
        if !stopped {
            self.drop_wait(&p.ticket, sink);
        }
        let ran = executed?.map_err(|error| refused("the gate could not run again", error))?;
        if let Some(gate) = &ran.report.gate {
            gathered.gate_failure = gate.failure.clone();
        }
        let event = if ran.breaches.is_empty() {
            stage_event(&ran.report.outcome, brief).0
        } else {
            Event::Quarantined
        };
        Ok((ran, event))
    }

    /// The ticket's wait file when it is valid for this run, read `at` its
    /// stage ([`base::wait_holds`]): the ticket in the wait's round,
    /// configured at its commit with its commands, its branch's head the one
    /// the failure was found on, and a repair's on the pull request's head
    /// it repairs; a rebase run's only while git still holds the rebased
    /// commit it kept, which no ref keeps (OWL-490). One that is not is
    /// removed, a wait of another stage too.
    pub(super) fn valid_wait(
        &self,
        p: &Prepared,
        dispatched: &Dispatched,
        state: &TicketState,
        at: WaitAt<'_>,
    ) -> Option<BaseWait> {
        let wait = base::read_wait(self.dirs, &p.ticket);
        let valid = wait.filter(|wait| {
            base::wait_holds(
                wait,
                at,
                state.round(),
                (&dispatched.configured_at.commit, &commands_key(dispatched)),
                self.branch_head(p).as_deref(),
            ) && (!at.rebase() || self.holds_commit(p, &wait.head))
        });
        if valid.is_none() {
            let _ = base::remove_wait(self.dirs, &p.ticket);
        }
        valid
    }

    /// Whether the dedicated checkout holds `commit`, a commit git may have
    /// pruned once no ref reached it.
    fn holds_commit(&self, p: &Prepared, commit: &str) -> bool {
        let object = format!("{commit}^{{commit}}");
        self.executor
            .git
            .run(
                &p.checkout,
                &[
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    "--end-of-options",
                    &object,
                ],
            )
            .is_ok()
    }

    /// Removes the ticket's wait file; a removal that fails is a `warning`.
    fn drop_wait(&self, ticket: &TicketId, sink: &mut EventSink<'_>) {
        if let Err(error) = base::remove_wait(self.dirs, ticket) {
            sink.emit(
                ticket,
                None,
                EventKind::Warning,
                data([
                    ("what", json!("base_wait_not_kept")),
                    ("reason", json!(error.to_string())),
                ]),
            );
        }
    }

    /// What a Build configured as `dispatched` says does, given `failing`,
    /// the failure of the ticket's run, or the one its wait kept: the base
    /// checks read, a match kept, and a check made when one is due.
    fn settle(
        &self,
        p: &Prepared,
        dispatched: &Dispatched,
        failing: Option<&Failing>,
        sink: &mut EventSink<'_>,
    ) -> Result<Settled, Stop> {
        let commit = dispatched.configured_at.commit.as_str();
        let key = commands_key(dispatched);
        let head = p.base.commit.as_str();
        let checks = match base::read(self.dirs) {
            Ok(checks) => checks,
            Err(why) => {
                self.base_unchecked(&p.ticket, &why, sink);
                return Ok(Settled::Go);
            }
        };
        let held_command = held_command(&checks, commit, &key);
        match checks.before_build(commit, &key, head) {
            Before::Held => {
                self.follow_hold(p, commit, &key, sink);
                return Ok(Settled::Held {
                    command: held_command,
                });
            }
            Before::CheckDue => return self.check_base(p, dispatched, failing, held_command, sink),
            Before::Go => {}
        }
        let Some(failing) = failing else {
            return Ok(Settled::Go);
        };
        // Matched against a verdict kept at this head, under the lock.
        let section = match self.section() {
            Ok(section) => section,
            Err(stop) if stop.is_busy() => {
                return Ok(Settled::Pending {
                    command: Some(failing.command.clone()),
                });
            }
            Err(stop) => return Err(stop),
        };
        let mut checks = base::read(self.dirs).unwrap_or_default();
        let now = (self.clock)().as_millisecond();
        let after = checks.after_failure(commit, &key, head, failing, now);
        let kept = match after {
            After::Matched(_) => base::write(self.dirs, &checks).err(),
            After::Branch | After::CheckDue => None,
        };
        drop(section);
        if let Some(why) = kept {
            self.base_unchecked(&p.ticket, &why, sink);
        }
        match after {
            After::Matched(recorded) => {
                if recorded == Recorded::Opened {
                    self.file_repair(p, commit, &key, &Filing::Opened, sink);
                    self.tell_hold(p, commit, &key, failing, sink);
                }
                Ok(Settled::Held {
                    command: Some(failing.command.clone()),
                })
            }
            After::Branch => Ok(Settled::Go),
            After::CheckDue => self.check_base(p, dispatched, Some(failing), None, sink),
        }
    }

    /// Checks the commit `dispatched` is configured at, alone, as `owlshift
    /// gate` runs, for `failing`: the verdict is kept by the check itself,
    /// under the project's lock. `held` is the command of a hold standing on
    /// it, which a check that cannot run keeps holding.
    fn check_base(
        &self,
        p: &Prepared,
        dispatched: &Dispatched,
        failing: Option<&Failing>,
        held: Option<String>,
        sink: &mut EventSink<'_>,
    ) -> Result<Settled, Stop> {
        let commit = dispatched.configured_at.commit.as_str();
        let key = commands_key(dispatched);
        let head = match self.run_check(p, commit, failing, sink)? {
            CheckRun::Ran { head, .. } => head,
            CheckRun::Busy => return Ok(self.check_waits(dispatched, failing, held)),
            CheckRun::Unchecked => {
                return Ok(held.map_or(Settled::Go, |command| Settled::Held {
                    command: Some(command),
                }));
            }
            // No gate command: no Build is dispatched on one (OWL-332).
            CheckRun::NoGate => return Ok(Settled::Go),
        };
        // The file, kept under the lock, says what holds, at the head the
        // check fetched.
        let checks = base::read(self.dirs).unwrap_or_default();
        Ok(match checks.before_build(commit, &key, &head) {
            Before::Held => Settled::Held {
                command: held_command(&checks, commit, &key),
            },
            Before::Go => Settled::Go,
            // The check said nothing: a hold standing goes on holding, and
            // a failure takes today's path.
            Before::CheckDue => held.map_or(Settled::Go, |command| Settled::Held {
                command: Some(command),
            }),
        })
    }

    /// Checks `commit` alone, as `owlshift gate` runs, for `failing` when a
    /// ticket's failure is behind it, from `p`'s step: the verdict is kept
    /// by the check itself, under the project's lock, and told, a hold it
    /// opens notified. A breach stops the command as a quarantine.
    fn run_check(
        &self,
        p: &Prepared,
        commit: &str,
        failing: Option<&Failing>,
        sink: &mut EventSink<'_>,
    ) -> Result<CheckRun, Stop> {
        let gate = GateOnce {
            executor: self.executor,
            dirs: self.dirs,
            remote_url: self.remote_url,
            local: self.config,
            at: Some(commit),
            place: Place::Base,
            except: Some(&p.ticket),
            initiating: failing,
            now: (self.clock)(),
        };
        let checked = match gate.run(&mut |_| {}) {
            Ok(checked) => checked,
            Err(gate_once::Stop::Busy(_)) => return Ok(CheckRun::Busy),
            Err(gate_once::Stop::Unverified { marker, text }) => {
                return Err(Stop::Unverified { marker, text });
            }
            Err(gate_once::Stop::Refused(why)) => {
                self.base_unchecked(&p.ticket, &why, sink);
                return Ok(CheckRun::Unchecked);
            }
            Err(gate_once::Stop::Stopped) => return Err(Stop::Stopped),
        };
        if checked.marker.is_some() {
            return Err(self
                .refuse_if_quarantined()
                .err()
                .unwrap_or_else(|| Stop::Refused("the base check broke isolation".to_owned())));
        }
        for warning in &checked.warnings {
            sink.emit(
                &p.ticket,
                None,
                EventKind::Warning,
                data([("what", json!("base_check")), ("reason", json!(warning))]),
            );
        }
        let Some(kept) = checked.base else {
            return Ok(CheckRun::NoGate);
        };
        if let Some(why) = &kept.unkept {
            self.base_unchecked(&p.ticket, why, sink);
        }
        if let (Some(verdict), Some(recorded)) = (&kept.verdict, kept.recorded)
            && (!kept.reused || recorded != Recorded::StillHeld)
        {
            sink.emit_project(
                None,
                EventKind::Decision,
                base::checked_event(commit, &checked.plan.head, verdict, recorded),
            );
            match (recorded, verdict) {
                (Recorded::Opened, Verdict::Failed(failure)) => {
                    self.file_repair(p, commit, &kept.commands, &Filing::Opened, sink);
                    self.tell_hold(p, commit, &kept.commands, &failure.failing, sink);
                }
                (Recorded::Lifted, _) => self.tell_lifted(p, commit, &kept.commands, sink),
                _ => {}
            }
        }
        Ok(CheckRun::Ran {
            verdict: kept.verdict,
            head: checked.plan.head,
        })
    }

    /// A check that could not run yet, another ticket's step in flight: a
    /// hold standing goes on holding, its check again due (OWL-431); a
    /// failure waits for it, pending, as does every Build configured there;
    /// with neither, nothing holds.
    fn check_waits(
        &self,
        dispatched: &Dispatched,
        failing: Option<&Failing>,
        held: Option<String>,
    ) -> Settled {
        if let Some(command) = held {
            return Settled::Due {
                command: Some(command),
            };
        }
        let commit = dispatched.configured_at.commit.as_str();
        let key = commands_key(dispatched);
        if let Some(failing) = failing
            && let Ok(section) = self.section()
        {
            let mut checks = base::read(self.dirs).unwrap_or_default();
            let now = (self.clock)().as_millisecond();
            if checks.mark_pending(commit, &key, failing, now) {
                let _ = base::write(self.dirs, &checks);
            }
            drop(section);
        }
        let command = failing.map(|failing| failing.command.clone()).or_else(|| {
            base::read(self.dirs)
                .ok()
                .and_then(|checks| match &checks.get(commit, &key)?.entry {
                    Entry::Pending { failing, .. } => Some(failing.command.clone()),
                    Entry::Checked { .. } => None,
                })
        });
        Settled::Pending { command }
    }

    /// The failing command of a hold that names `ticket` as its repair
    /// (OWL-622), a setup command's first: the ticket is the repair of a
    /// broken base, which no such hold holds. `None` for any other ticket,
    /// and when the project's file does not read.
    pub(super) fn repair_of(&self, ticket: &TicketId) -> Option<Failing> {
        let checks = base::read(self.dirs).ok()?;
        let failing: Vec<Failing> = checks
            .held_for(ticket.as_str())
            .filter_map(|check| match &check.entry {
                Entry::Checked {
                    verdict: Verdict::Failed(failure),
                    ..
                } => Some(failure.failing.clone()),
                Entry::Checked { .. } | Entry::Pending { .. } => None,
            })
            .collect();
        failing
            .iter()
            .find(|failing| failing.step == owlshift_core::base::Step::Setup)
            .or(failing.first())
            .cloned()
    }

    /// Files the repair ticket of the hold on `commit` and `key` (OWL-622;
    /// architecture section 5, "The repair ticket"), as `p`'s step found it
    /// ([`Filing`]): unless `policy.base_repair` at the base this command
    /// fetched is `false`. One section of the project's lock holds the whole
    /// filing, the tracker's calls included, so two steps never file twice; a
    /// section that cannot be had leaves the hold untried, for its next held
    /// step. An open repair kept for this hold or one on the same command is
    /// taken again, else one found by its title or marker, else one filed
    /// while its break filed fewer than
    /// [`owlshift_core::base::MAX_BREAK_REPAIRS`] (OWL-639); what
    /// the tracker refuses, or a break whose repairs are spent, is a
    /// `repair_unfiled` warning, the hold told as before, and nothing is
    /// filed for it until it opens again. The repair kept joins its break.
    fn file_repair(
        &self,
        p: &Prepared,
        commit: &str,
        key: &str,
        filing: &Filing,
        sink: &mut EventSink<'_>,
    ) {
        match project::base_repair_at(&self.executor.git, &p.checkout, &p.base) {
            Ok(true) => {}
            Ok(false) => return,
            Err(why) => return self.repair_unfiled(p, commit, &why, sink),
        }
        let Ok(section) = self.section() else {
            return;
        };
        let Ok(mut checks) = base::read(self.dirs) else {
            return;
        };
        let failure = match checks.get(commit, key).map(|check| &check.entry) {
            Some(Entry::Checked {
                verdict: Verdict::Failed(failure),
                hold: Some(_),
                repair,
                ..
            }) if match filing {
                Filing::Opened => true,
                Filing::Untried => repair.is_none(),
                Filing::Outlived(ticket) => {
                    repair.as_ref().and_then(Repair::ticket) == Some(ticket.as_str())
                }
            } =>
            {
                failure.clone()
            }
            _ => return,
        };
        let kept = self.repair_for(p, commit, key, &checks, &failure);
        let (repair, attempt) = match &kept {
            Ok(kept) => (
                Some(Repair::Filed {
                    ticket: kept.ticket.as_str().to_owned(),
                    text_sha256: kept.text.clone(),
                }),
                checks.keep_break_repair(&failure.failing, kept.ticket.as_str()),
            ),
            Err(NotKept::Unfiled(_)) => (Some(Repair::Unfiled), 0),
            // Kept as never tried: the hold's next step tries again.
            Err(NotKept::Unread(_)) => (None, 0),
        };
        let written = match repair {
            Some(repair) => {
                checks.set_repair(commit, key, repair);
                base::write(self.dirs, &checks)
            }
            None => Ok(()),
        };
        drop(section);
        match kept {
            Ok(Kept {
                ticket,
                reused,
                problems,
                ..
            }) => {
                sink.emit_project(
                    None,
                    EventKind::Decision,
                    base::repair_event(commit, &failure.failing, &ticket, reused, attempt),
                );
                for problem in problems {
                    sink.emit(
                        &p.ticket,
                        None,
                        EventKind::Warning,
                        data([
                            ("what", json!("repair_problem")),
                            ("ticket", json!(ticket)),
                            ("reason", json!(problem.message)),
                        ]),
                    );
                }
                if let Err(why) = written {
                    let reason = format!("{ticket} was filed, but {why}");
                    self.repair_unfiled(p, commit, &reason, sink);
                }
            }
            Err(NotKept::Unfiled(why) | NotKept::Unread(why)) => {
                self.repair_unfiled(p, commit, &why, sink);
            }
        }
    }

    /// The repair kept for the hold on `commit` and `key`, which `failure`
    /// fails, under the project's lock the caller holds; or why none was
    /// kept. An open ticket taken again whose text cannot be read now, nor
    /// was kept, is [`NotKept::Unread`]: an empty hash would never admit it.
    /// None is filed once the break of `failure`'s command filed its last
    /// (OWL-639); a new one is told the repairs its break filed before it.
    fn repair_for(
        &self,
        p: &Prepared,
        commit: &str,
        key: &str,
        checks: &owlshift_core::base::Checks,
        failure: &owlshift_core::base::Failure,
    ) -> Result<Kept, NotKept> {
        let failing = &failure.failing;
        // The text kept with a repair, for one the tracker cannot read now.
        let kept_text = |ticket: &TicketId| {
            checks.checks.iter().find_map(|check| match &check.entry {
                Entry::Checked {
                    repair:
                        Some(Repair::Filed {
                            ticket: kept,
                            text_sha256,
                        }),
                    ..
                } if kept == ticket.as_str() => Some(text_sha256.clone()),
                _ => None,
            })
        };
        let text_of =
            |ticket: &TicketId, fallback: Option<String>| match self.tracker.ticket(ticket) {
                Ok(read) => Ok(text_sha256(&read.title, &read.description)),
                Err(error) => fallback.ok_or_else(|| {
                    NotKept::Unread(format!(
                        "{ticket}, the repair taken again, could not be read: {error}"
                    ))
                }),
            };
        for id in checks.reusable(commit, key, failing) {
            let Ok(ticket) = TicketId::new(&id) else {
                continue;
            };
            match self.tracker.state_type(&ticket) {
                Ok(Some(
                    tracker::StateType::Completed
                    | tracker::StateType::Canceled
                    | tracker::StateType::Duplicate,
                )) => continue,
                Err(error) if error.kind == tracker::ErrorKind::NotFound => continue,
                _ => {
                    let text = text_of(&ticket, kept_text(&ticket))?;
                    return Ok(Kept {
                        ticket,
                        text,
                        reused: true,
                        problems: Vec::new(),
                    });
                }
            }
        }
        let title = base::repair_title(commit, failing);
        let marker = base::repair_marker(commit, failing);
        // One a colleague's runner filed for the same break, or this one
        // before its file kept it: found by its title or marker, which a
        // completed ticket is not.
        if let Ok(found) =
            self.tracker
                .proposed(self.config.tracker.team.as_deref(), &title, &marker)
            && let Some(found) = found.into_iter().next()
        {
            let text = text_of(&found.id, None)?;
            return Ok(Kept {
                ticket: found.id,
                text,
                reused: true,
                problems: Vec::new(),
            });
        }
        // A break that filed its last repair files none (OWL-639).
        let before = checks.break_repairs(failing);
        if checks.break_spent(failing) {
            return Err(NotKept::Unfiled(base::repairs_spent(before)));
        }
        let assignee = self.repair_assignee(p).ok_or_else(|| {
            NotKept::Unfiled(
                "no operator account to assign the repair to: neither the tracker key's \
                 account nor the held ticket's assignee"
                    .to_owned(),
            )
        })?;
        let since = checks
            .last_passed(key)
            .map(|passed| (passed.to_owned(), self.commits_since(p, passed, commit)));
        let description = base::repair_description(
            commit,
            failure,
            since
                .as_ref()
                .map(|(passed, commits)| (passed.as_str(), commits.as_slice())),
            before,
        );
        let label = match &self.config.tracker.admit {
            Admit::Label(label) => Some(label.as_str()),
            Admit::State(_) | Admit::Delegation => None,
        };
        let filed = self
            .tracker
            .file_repair(&tracker::Repair {
                held: &p.ticket,
                title: &title,
                description: &description,
                ready: &self.config.tracker.states.ready,
                label,
                assignee: Some(&assignee),
            })
            .map_err(|error| NotKept::Unfiled(format!("the tracker did not file it: {error}")))?;
        // Just filed: what was sent stands for what a failed read cannot give.
        let text = text_of(&filed.id, Some(text_sha256(&title, &description)))?;
        Ok(Kept {
            ticket: filed.id,
            text,
            reused: false,
            problems: filed.problems,
        })
    }

    /// A standing hold on `commit` that `p`'s step finds (OWL-622, OWL-639):
    /// its repair filed when none was tried yet, its first try having found
    /// the project's lock taken or an open repair it could not read; on the
    /// default branch's head this step fetched, the next one filed once the
    /// repair it holds is completed on the tracker, the head still failing;
    /// then told when it was not yet.
    fn follow_hold(&self, p: &Prepared, commit: &str, key: &str, sink: &mut EventSink<'_>) {
        let Ok(checks) = base::read(self.dirs) else {
            return;
        };
        let Some(Entry::Checked {
            verdict: Verdict::Failed(failure),
            hold: Some(hold),
            repair,
            ..
        }) = checks.get(commit, key).map(|check| &check.entry)
        else {
            return;
        };
        match repair {
            None => self.file_repair(p, commit, key, &Filing::Untried, sink),
            Some(Repair::Filed { ticket, .. }) if commit == p.base.commit => {
                if let Ok(id) = TicketId::new(ticket)
                    && matches!(
                        self.tracker.state_type(&id),
                        Ok(Some(tracker::StateType::Completed))
                    )
                {
                    self.file_repair(p, commit, key, &Filing::Outlived(ticket.clone()), sink);
                }
            }
            Some(_) => {}
        }
        if !hold.told {
            self.tell_hold(p, commit, key, &failure.failing, sink);
        }
    }

    /// The `warning` event of a held commit's command that the default
    /// branch's head, checked for it, does not fail, though it fails another
    /// (OWL-639): no hold follows the head, whose own failure waits for a
    /// Build there to match it.
    fn not_followed(
        &self,
        p: &Prepared,
        (held, head): (&str, &str),
        failing: &Failing,
        verdict: &Verdict,
        sink: &mut EventSink<'_>,
    ) {
        let Verdict::Failed(failure) = verdict else {
            return;
        };
        if failure.failing == *failing {
            return;
        }
        sink.emit(
            &p.ticket,
            None,
            EventKind::Warning,
            data([
                ("what", json!("base_not_followed")),
                ("commit", json!(held)),
                ("command", json!(failing.command)),
                ("head", json!(head)),
                ("head_command", json!(failure.failing.command)),
            ]),
        );
    }

    /// Who a repair is assigned to, its decider (OWL-622): the operator this
    /// runner takes tickets for, else the account of the tracker's key when
    /// it is no app's, else the assignee of the ticket whose failure held
    /// the base.
    fn repair_assignee(&self, p: &Prepared) -> Option<String> {
        if let Some(operator) = self.takes.operator() {
            return Some(operator.to_owned());
        }
        if let Ok(Some(account)) = self.tracker.operator()
            && !account.app
        {
            return Some(account.id);
        }
        p.found.assignee.as_ref().map(|person| person.id.clone())
    }

    /// The default branch's commits from `passed` to `commit`, each as its
    /// short id and subject, at most [`base::REPAIR_COMMITS`]; none when git
    /// cannot say.
    fn commits_since(&self, p: &Prepared, passed: &str, commit: &str) -> Vec<String> {
        let range = format!("{passed}..{commit}");
        let limit = format!("--max-count={}", base::REPAIR_COMMITS);
        self.executor
            .git
            .output(
                &p.checkout,
                &["log", "--format=%h %s", &limit, "--end-of-options", &range],
                None,
            )
            .ok()
            .filter(|out| out.success())
            .map(|out| {
                String::from_utf8_lossy(&out.stdout)
                    .lines()
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Tells the repair ticket of the hold on `commit` and `key` that the
    /// hold lifted before it merged (OWL-622): a LIFTED comment, the ticket
    /// left to its decider. Nothing for a hold with no repair, nor for one
    /// the tracker shows completed or canceled; a comment that cannot be
    /// posted is a `warning`.
    fn tell_lifted(&self, p: &Prepared, commit: &str, key: &str, sink: &mut EventSink<'_>) {
        let Some(ticket) = base::read(self.dirs).ok().and_then(|checks| {
            checks
                .repair(commit, key)
                .and_then(Repair::ticket)
                .and_then(|ticket| TicketId::new(ticket).ok())
        }) else {
            return;
        };
        if matches!(
            self.tracker.state_type(&ticket),
            Ok(Some(
                tracker::StateType::Completed
                    | tracker::StateType::Canceled
                    | tracker::StateType::Duplicate
            ))
        ) {
            return;
        }
        if let Err(error) = self
            .tracker
            .post_comment(&ticket, &base::lifted_comment(&ticket, commit))
        {
            sink.emit(
                &p.ticket,
                None,
                EventKind::Warning,
                data([
                    ("what", json!("repair_not_told")),
                    ("ticket", json!(ticket)),
                    ("reason", json!(error.to_string())),
                ]),
            );
        }
    }

    /// The `warning` event of a repair not filed for the hold on `commit`
    /// (OWL-622): the hold stays told as before.
    fn repair_unfiled(&self, p: &Prepared, commit: &str, reason: &str, sink: &mut EventSink<'_>) {
        sink.emit(
            &p.ticket,
            None,
            EventKind::Warning,
            data([
                ("what", json!("repair_unfiled")),
                ("commit", json!(commit)),
                ("reason", json!(reason)),
            ]),
        );
    }

    /// Leaves out of the repair's Build admission what the tickets held by
    /// a broken base hold (OWL-622, "First, and not held"): each Owlshift
    /// holder not delivered whose branch was made from a commit `base.json`
    /// holds, or whose branch git cannot read, which never makes the repair
    /// wait. Under `rebase` it is weighed as a yielded ticket is, so what
    /// the repair takes of it is recorded (OWL-621); under `exclusive` it is
    /// left out. They go on by their own rebase once the repair merged.
    pub(super) fn leave_base_held(
        &self,
        p: &Prepared,
        in_flight: &mut Vec<InFlight<String>>,
        overlap: Overlap,
    ) {
        let checks = base::read(self.dirs).unwrap_or_default();
        in_flight.retain_mut(|holder| {
            if holder.outside || holder.delivered || holder.id == p.ticket.as_str() {
                return true;
            }
            let held = TicketId::new(&holder.id).map_or(true, |id| {
                self.merge_base(
                    p,
                    &format!("refs/heads/{}", branch_for(&id)),
                    &p.base.commit,
                )
                .is_none_or(|made_from| checks.holds_commit(&made_from))
            });
            match (held, overlap) {
                (false, _) => true,
                (true, Overlap::Rebase) => {
                    holder.yielded = true;
                    true
                }
                (true, Overlap::Exclusive) => false,
            }
        });
    }

    /// Tells the operator of a hold on `commit`, with the ticket's link: its
    /// told mark taken under the project's lock once a hold, the line shown
    /// once a break, at its first hold, and once more at a hold that finds
    /// its repairs spent, saying so (OWL-639); any other hold of a break is
    /// marked told with nothing shown, its repair reaching the operator as
    /// an assigned ticket.
    fn tell_hold(
        &self,
        p: &Prepared,
        commit: &str,
        key: &str,
        failing: &Failing,
        sink: &mut EventSink<'_>,
    ) {
        let Some(notifier) = self.notifier else {
            return;
        };
        let Ok(section) = self.section() else {
            return;
        };
        let mut checks = base::read(self.dirs).unwrap_or_default();
        if !checks.claim_told(commit, key) {
            return;
        }
        let spent = matches!(checks.repair(commit, key), Some(Repair::Unfiled))
            && checks.break_spent(failing);
        let first = checks.claim_break_told(failing);
        let shown = (spent && checks.claim_spent_told(failing)) || first;
        let claimed = base::write(self.dirs, &checks).is_ok();
        drop(section);
        if !claimed || !shown {
            return;
        }
        let mut line = base::line(&p.ticket, commit, failing);
        if spent {
            line.push(' ');
            line.push_str(&base::repairs_spent(checks.break_repairs(failing)));
        }
        if let Err(error) = notifier.notify(&notify::with_link(line, self.tracker, &p.ticket)) {
            sink.emit(
                &p.ticket,
                None,
                EventKind::Warning,
                notify::not_shown(&error),
            );
        }
    }

    /// The stop of a Build held for its base, after its `decision` event
    /// (`base` = `waits`), which `owlshift why` reads.
    fn base_held(
        &self,
        p: &Prepared,
        dispatched: &Dispatched,
        command: Option<String>,
        pending: bool,
        due: bool,
        sink: &mut EventSink<'_>,
    ) -> Stop {
        let commit = dispatched.configured_at.commit.clone();
        sink.emit(
            &p.ticket,
            None,
            EventKind::Decision,
            data([
                ("base", json!("waits")),
                ("reason", json!("base_broken")),
                ("commit", json!(commit)),
                ("command", json!(command)),
                ("pending", json!(pending)),
            ]),
        );
        Stop::BaseHeld {
            ticket: p.ticket.clone(),
            commit,
            command,
            pending,
            due,
        }
    }

    /// The `warning` event of a base check that says nothing: today's path
    /// follows.
    fn base_unchecked(&self, ticket: &TicketId, reason: &str, sink: &mut EventSink<'_>) {
        sink.emit(
            ticket,
            None,
            EventKind::Warning,
            data([("what", json!("base_unchecked")), ("reason", json!(reason))]),
        );
    }
}

/// The commit a held Build's rebase run left, committed on its branch: the
/// one its empty gate passed on.
fn rebased_commit(ran: &Ran) -> Option<CommitId> {
    ran.report
        .gate
        .as_ref()
        .filter(|gate| gate.passed())
        .and_then(|gate| gate.commit.as_deref())
        .and_then(|commit| CommitId::new(commit).ok())
}

/// What a held Build's rebase run said, for its park: its summary and its
/// questions, or how it ended without a result.
fn rebase_said(ran: &Ran) -> String {
    match ran.result() {
        Some(result) => {
            let mut said = format!("the run said: {}", result.summary.trim());
            for question in &result.questions {
                said.push_str(&format!(" {}: {}", question.id.as_str(), question.text));
            }
            said
        }
        None => format!("the run ended so: {}", park_detail(&ran.report.outcome)),
    }
}

/// The key of the setup and gate commands a Build configured as
/// `dispatched` runs.
fn commands_key(dispatched: &Dispatched) -> String {
    base::commands_key(dispatched.rules.setup(), dispatched.rules.gate())
}

/// The command a hold standing on `commit` failed at, if one stands; a
/// pending check is no hold.
fn held_command(checks: &owlshift_core::base::Checks, commit: &str, key: &str) -> Option<String> {
    match &checks.get(commit, key)?.entry {
        Entry::Checked {
            verdict: Verdict::Failed(failure),
            hold: Some(_),
            ..
        } => Some(failure.failing.command.clone()),
        Entry::Checked { .. } | Entry::Pending { .. } => None,
    }
}
