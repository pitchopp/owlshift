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

use owlshift_contracts::base::{BaseWait, KeptDone, KeptFailing};
use owlshift_core::base::{After, Before, Entry, Failing, HeldBranch, Leave, Recorded, Verdict};

use super::*;
use crate::base;
use crate::gate_once::{self, GateOnce, Place};
use crate::notify;

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
        let wait = self.valid_wait(p, dispatched, state);
        let failing = wait.as_ref().map(|wait| wait.failing.failing());
        match self.settle(p, dispatched, failing.as_ref(), sink)? {
            Settled::Go => {
                self.drop_wait(&p.ticket, sink);
                Ok(Resume::Go(wait.and_then(|wait| wait.done)))
            }
            Settled::Held { command: failed } => {
                match self.leave_held(p, command, dispatched, state, failed.as_deref(), sink)? {
                    Some((rebased, followups)) => {
                        // A done Build's result is kept at the rebased head
                        // and the commit the ticket is configured at now, so
                        // a stop before its regate still finds it; a held
                        // failure with none behind it is spent.
                        let done = wait.and_then(|wait| {
                            let done = wait.done?;
                            self.keep_wait(
                                p,
                                &rebased,
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
                            dispatched: Box::new(rebased),
                            done,
                            followups,
                        })
                    }
                    None => Err(self.base_held(p, dispatched, failed, false, sink)),
                }
            }
            Settled::Pending { command: failed } => {
                Err(self.base_held(p, dispatched, failed, true, sink))
            }
        }
    }

    /// A Build held at the commit `dispatched` is configured at, whose
    /// check still fails at the default branch's head (OWL-427): when its
    /// branch has commits of its own and a clean worktree, no pull request
    /// is open, and the head contains that commit and passes its own check,
    /// one `rebase` role run moves the branch onto the head
    /// ([`Self::rebase_held`]), and the ticket is dispatched again for
    /// `command`, configured there. `None` when it stays held; `failed` is
    /// the command the held commit fails, for a park's reason.
    fn leave_held(
        &self,
        p: &mut Prepared,
        command: Command,
        dispatched: &Dispatched,
        state: &TicketState,
        failed: Option<&str>,
        sink: &mut EventSink<'_>,
    ) -> Result<Option<(Dispatched, Vec<Followup>)>, Stop> {
        let held = dispatched.configured_at.commit.clone();
        let head = p.base.commit.clone();
        let (branch, start) = self.held_branch(p, &head)?;
        let head_has_held = head != held && self.is_ancestor(p, &held, &head);
        // Whether a head that passes would move it at all, before anything
        // is read at the head.
        if owlshift_core::base::leave(&held, &head, head_has_held, Some(&Verdict::Passed), branch)
            != Leave::Rebase
        {
            if branch == HeldBranch::Uncommitted && head_has_held {
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
            return Ok(None);
        }
        let checks = match base::read(self.dirs) {
            Ok(checks) => checks,
            Err(why) => {
                self.base_unchecked(&p.ticket, &why, sink);
                return Ok(None);
            }
        };
        // The held commit checked again at this head first, still failing:
        // a check that could not run yet leaves an environment fix possible,
        // which lifts the hold with no rebase.
        if !matches!(
            checks.verdict_at(&held, &commands_key(dispatched), &head),
            Some(Verdict::Failed(_))
        ) {
            return Ok(None);
        }
        // The head's own setup and gate, which its check runs and the
        // rebase run is briefed with (OWL-331: a rebase takes the base it
        // rebases onto).
        let configured = self.dispatch_rules(p, Command::Rebase, sink)?;
        let key = base::commands_key(configured.rules.setup(), configured.rules.gate());
        let kept = checks.verdict_at(&head, &key, &head).cloned();
        let verdict =
            match owlshift_core::base::leave(&held, &head, head_has_held, kept.as_ref(), branch) {
                Leave::Stay => return Ok(None),
                Leave::Rebase => Verdict::Passed,
                Leave::CheckHead => match self.run_check(p, &head, None, sink)? {
                    CheckRun::Ran {
                        verdict: Some(verdict),
                        ..
                    } => verdict,
                    CheckRun::Ran { verdict: None, .. }
                    | CheckRun::Busy
                    | CheckRun::Unchecked
                    | CheckRun::NoGate => return Ok(None),
                },
            };
        if owlshift_core::base::leave(&held, &head, head_has_held, Some(&verdict), branch)
            != Leave::Rebase
        {
            return Ok(None);
        }
        let followups = self.rebase_held(
            p,
            state,
            (&held, failed),
            &configured.rules,
            &start,
            dispatched,
            sink,
        )?;
        // Its merge-base with the base is the head now: configured there.
        let rebased = self.dispatch(p, command, sink)?;
        Ok(Some((rebased, followups)))
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
        (held, failed): (&str, Option<&str>),
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
        sink.emit(
            &p.ticket,
            None,
            EventKind::Decision,
            data([
                ("base", json!("rebase")),
                ("from", json!(held)),
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
            broken: Some(held.to_owned()),
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
        if let Outcome::UsageLimit { resets_at } = ran.report.outcome {
            return Err(Stop::UsageLimit { resets_at });
        }
        let parked = match state.apply(p.pipeline, Event::RebaseFailed) {
            Ok(Transition::Parked { state: parked, .. }) => parked,
            other => return Err(core_error(other)),
        };
        let failing =
            failed.map_or_else(|| "its gate".to_owned(), |command| format!("`{command}`"));
        let detail = format!(
            "{ticket}'s branch was made from {held_short}, which fails {failing}; {base}'s head \
             {onto_short} passes, and the rebase onto it was not taken: {why}. The branch is \
             left at {start_short}. Say on the ticket what the rebase needs, if anything, then \
             `owlshift continue {ticket}` runs it again.",
            ticket = p.ticket,
            held_short = base::short(held),
            base = p.base.branch,
            onto_short = base::short(&onto),
            start_short = base::short(start),
        );
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
    /// it failed on a command its base fails too, or whose check is due and
    /// cannot run yet, the ticket stays as it is ([`Event::BaseBroken`]),
    /// what a done Build reported is kept for its gate to run again, and the
    /// command stops held. `None` for any other failure, which takes today's
    /// path.
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
        let Some(failing) = base::failing(&ran.report) else {
            return Ok(None);
        };
        let (command, pending) = match self.settle(p, dispatched, Some(&failing), sink)? {
            Settled::Go => return Ok(None),
            Settled::Held { command } => (command, false),
            Settled::Pending { command } => (command, true),
        };
        match state.apply(p.pipeline, Event::BaseBroken) {
            Ok(Transition::To(next)) => *state = next,
            other => return Err(core_error(other)),
        }
        let decider = p
            .questions()
            .latest()
            .map(|ask| person(&ask.decider, &p.found));
        let done = ran.report.gated.as_ref().map(|result| KeptDone {
            result: (**result).clone(),
            notes: gathered.decisions.clone(),
            followups: gathered.followups.clone(),
            seen: decider
                .as_ref()
                .and_then(|decider| answer_check::newest_decider_edit(comments, decider)),
            integrating,
        });
        self.keep_wait(
            p,
            dispatched,
            state.round(),
            (KeptFailing::from(&failing), done),
            Some(&ran.run),
            sink,
        );
        Ok(Some(self.base_held(p, dispatched, command, pending, sink)))
    }

    /// Keeps the ticket's wait file for a Build configured as `dispatched`
    /// in `round`, at its branch's head now: the held failure and a done
    /// Build's result. A branch with no head keeps none; a write that fails
    /// is a `warning` for `run`.
    fn keep_wait(
        &self,
        p: &Prepared,
        dispatched: &Dispatched,
        round: u32,
        (failing, done): (KeptFailing, Option<KeptDone>),
        run: Option<&str>,
        sink: &mut EventSink<'_>,
    ) {
        let Some(head) = self.branch_head(p) else {
            self.drop_wait(&p.ticket, sink);
            return;
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
        };
        if let Err(error) = base::write_wait(self.dirs, &p.ticket, &wait) {
            sink.emit(
                &p.ticket,
                run,
                EventKind::Warning,
                data([
                    ("what", json!("base_wait_not_kept")),
                    ("reason", json!(error.to_string())),
                ]),
            );
        }
    }

    /// A done Build's gate run again alone, its kept result judged by it
    /// (OWL-418): as [`Self::run_role_once`], with no budget check, no
    /// session, no usage admission and no cost, since no harness runs. The
    /// wait is spent whatever the gate says: a red gate is classified again
    /// as any Build run's.
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
        self.drop_wait(&p.ticket, sink);
        let ran = self
            .execute_inner(
                p,
                self.executor,
                (harness, setup),
                brief,
                attempt,
                (None, Some(kept)),
                sink,
            )?
            .map_err(|error| refused("the gate could not run again", error))?;
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

    /// The ticket's wait file when it is valid for this run: the ticket at
    /// Build in the wait's round, configured at its commit with its
    /// commands, its branch's head the one the failure was found on. One
    /// that is not is removed.
    fn valid_wait(
        &self,
        p: &Prepared,
        dispatched: &Dispatched,
        state: &TicketState,
    ) -> Option<BaseWait> {
        let wait = base::read_wait(self.dirs, &p.ticket);
        let valid = wait.filter(|wait| {
            wait.round == state.round()
                && wait.configured_at == dispatched.configured_at.commit
                && wait.commands == commands_key(dispatched)
                && self.branch_head(p).as_deref() == Some(wait.head.as_str())
        });
        if valid.is_none() {
            let _ = base::remove_wait(self.dirs, &p.ticket);
        }
        valid
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
                // A hold not told yet, its first telling having found the
                // project's lock taken, is told now.
                if let Some(Entry::Checked {
                    verdict: Verdict::Failed(failure),
                    hold: Some(hold),
                    ..
                }) = checks.get(commit, &key).map(|check| &check.entry)
                    && !hold.told
                {
                    self.tell_hold(p, commit, &key, &failure.failing, sink);
                }
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
            if let (Recorded::Opened, Verdict::Failed(failure)) = (recorded, verdict) {
                self.tell_hold(p, commit, &kept.commands, &failure.failing, sink);
            }
        }
        Ok(CheckRun::Ran {
            verdict: kept.verdict,
            head: checked.plan.head,
        })
    }

    /// A check that could not run yet, another ticket's step in flight: a
    /// hold standing goes on holding; a failure waits for it, pending, as
    /// does every Build configured there; with neither, nothing holds.
    fn check_waits(
        &self,
        dispatched: &Dispatched,
        failing: Option<&Failing>,
        held: Option<String>,
    ) -> Settled {
        if let Some(command) = held {
            return Settled::Held {
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

    /// Tells the operator of a hold on `commit` once (its told mark taken
    /// under the project's lock), with the ticket's link.
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
        let claimed = checks.claim_told(commit, key) && base::write(self.dirs, &checks).is_ok();
        drop(section);
        if !claimed {
            return;
        }
        let line = base::line(&p.ticket, commit, failing);
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
