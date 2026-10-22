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

use owlshift_contracts::base::{BaseWait, KeptDone, KeptFailing};
use owlshift_core::base::{After, Before, Entry, Failing, Recorded, Verdict};

use super::*;
use crate::base;
use crate::gate_once::{self, GateOnce, Place};
use crate::notify;

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
    /// Before a Build's first run of the command (OWL-418): held when the
    /// commit it is configured at is held, or a check of it is due and
    /// cannot run yet; otherwise the done Build whose gate a hold kept, for
    /// that gate to run again alone ([`Self::regate_role`]). A failure that
    /// waited for its check with no done Build behind it lets the Build run.
    pub(super) fn before_base(
        &self,
        p: &mut Prepared,
        dispatched: &Dispatched,
        state: &TicketState,
        sink: &mut EventSink<'_>,
    ) -> Result<Option<KeptDone>, Stop> {
        if state.status() != Status::Active(Stage::Build) || p.repairing {
            return Ok(None);
        }
        let wait = self.valid_wait(p, dispatched, state);
        let failing = wait.as_ref().map(|wait| wait.failing.failing());
        match self.settle(p, dispatched, failing.as_ref(), sink)? {
            Settled::Go => {
                self.drop_wait(&p.ticket, sink);
                Ok(wait.and_then(|wait| wait.done))
            }
            Settled::Held { command } => Err(self.base_held(p, dispatched, command, false, sink)),
            Settled::Pending { command } => Err(self.base_held(p, dispatched, command, true, sink)),
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
        match self.branch_head(p) {
            Some(head) => {
                let wait = BaseWait {
                    format: Format,
                    configured_at: dispatched.configured_at.commit.clone(),
                    commands: commands_key(dispatched),
                    round: state.round(),
                    head,
                    failing: KeptFailing::from(&failing),
                    done,
                };
                if let Err(error) = base::write_wait(self.dirs, &p.ticket, &wait) {
                    sink.emit(
                        &p.ticket,
                        Some(&ran.run),
                        EventKind::Warning,
                        data([
                            ("what", json!("base_wait_not_kept")),
                            ("reason", json!(error.to_string())),
                        ]),
                    );
                }
            }
            None => self.drop_wait(&p.ticket, sink),
        }
        Ok(Some(self.base_held(p, dispatched, command, pending, sink)))
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
            Err(gate_once::Stop::Busy(_)) => return Ok(self.check_waits(dispatched, failing, held)),
            Err(gate_once::Stop::Unverified { marker, text }) => {
                return Err(Stop::Unverified { marker, text });
            }
            Err(gate_once::Stop::Refused(why)) => {
                self.base_unchecked(&p.ticket, &why, sink);
                return Ok(held.map_or(Settled::Go, |command| Settled::Held {
                    command: Some(command),
                }));
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
            // No gate command: no Build is dispatched on one (OWL-332).
            return Ok(Settled::Go);
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
                self.tell_hold(p, commit, &key, &failure.failing, sink);
            }
        }
        // The file, kept under the lock, says what holds, at the head the
        // check fetched.
        let checks = base::read(self.dirs).unwrap_or_default();
        Ok(
            match checks.before_build(commit, &key, &checked.plan.head) {
                Before::Held => Settled::Held {
                    command: held_command(&checks, commit, &key),
                },
                Before::Go => Settled::Go,
                // The check said nothing: a hold standing goes on holding, and
                // a failure takes today's path.
                Before::CheckDue => held.map_or(Settled::Go, |command| Settled::Held {
                    command: Some(command),
                }),
            },
        )
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
