//! A `blocked` through the resolver (OWL-620; architecture section 4, "A
//! `blocked` through the resolver"): a Build-role or rebase run that ends
//! `blocked` gets one resolver run before a person is asked, which answers
//! with one verdict of a closed list ([`UnblockVerdict`]), each an act the
//! runner already makes: the stage again with the resolver's note in its
//! thread (`retry`), the branch to the rebase run (`rebase`), the step owed
//! to a person and the Build going on (`person_step`), or today's park, the
//! resolver's sentence on what the person must do first leading the PARKED
//! comment (`park`).
//!
//! The verdicts open at a stage are the core's ([`unblock::open`]); the
//! resolver's answer is held to the project's level as any decision of its
//! ([`resolver::settle`], on the runner's own question,
//! [`resolver::unblock_question`]), posted as a DECISION comment the
//! decider's reply reverses, and kept with its bound in the ticket ref
//! ([`OnDemand::log_decisions`]). Below `discoverable`, after the decider
//! closed a false premise, at the ticket's budget or the chain's bound,
//! the ticket parks as before.

use owlshift_contracts::UnblockVerdict;
use owlshift_contracts::brief::UnblockBrief;
use owlshift_core::unblock::{self, Unblocking};

use super::*;

/// A run that stopped `blocked`, as the resolver is asked about it.
pub(super) struct Blocked<'a> {
    pub(super) role: Role,
    pub(super) ran: &'a Ran,
    pub(super) result: &'a RunResult,
    /// Where it ran, and which acts its stage allows there.
    pub(super) at: Unblocking,
}

/// What the runner does about a run that stopped `blocked`.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Unblocked {
    /// The stage runs again, the DECISION in its thread.
    Retry,
    /// The branch goes to the rebase run, then the stage runs again.
    Rebase,
    /// A step of the plan is a person's now, kept with the plan: the stage
    /// runs again and passes it.
    PersonStep,
    /// The ticket parks, as before; the resolver's sentence leads the
    /// PARKED comment when its verdict was `park`.
    Park { first: Option<String> },
}

impl OnDemand<'_> {
    /// Sends a run that stopped `blocked` through the resolver (OWL-620),
    /// the ticket still at the stage that ran it. No resolver run below the
    /// delegation level `discoverable`, after the decider took a false
    /// premise's closure, at the ticket's budget (OWL-378) or at the chain's
    /// bound (`policy.resolver_passes`, the ceiling): the ticket parks. One
    /// otherwise, briefed with the run's summary, the plan as its steps
    /// stand, the branch's diff, the run's ledger and the verdicts open at
    /// the stage; its run counts in the chain and the budgets as any
    /// resolver run's. A result with no usable verdict, or one the level
    /// refuses, parks; `park` parks with its sentence; any other verdict is
    /// posted as a DECISION comment and kept with its bound (and, for
    /// `person_step`, the handed step) before the caller makes its act, one
    /// settled pass counted. Every verdict past the level's gate is a
    /// `decision` event (`unblock`).
    pub(super) fn unblock(
        &self,
        p: &mut Prepared,
        state: &TicketState,
        blocked: &Blocked<'_>,
        rules: &ProjectRules,
        current: &Person,
        sink: &mut EventSink<'_>,
    ) -> Result<Unblocked, Stop> {
        let level = p.policy.level();
        if !unblock::delegated(level) || resolver::closure_taken(&p.questions()) {
            return Ok(Unblocked::Park { first: None });
        }
        let ticket = p.ticket.clone();
        let parked = |reason: &str, resolver: Option<&str>, sink: &mut EventSink<'_>| {
            sink.emit(
                &ticket,
                Some(&blocked.ran.run),
                EventKind::Decision,
                unblock_event(blocked, UnblockVerdict::Park, resolver, Some(reason), None),
            );
            Ok(Unblocked::Park { first: None })
        };
        // The ticket's budget first (OWL-378), then the chain's bound.
        if let Some(bound) = self
            .budget_reached(p)
            .map(Fallback::from)
            .or_else(|| p.policy.chain_end(p.chain).map(Fallback::from))
        {
            return parked(bound.reason().unwrap_or(bound.as_str()), None, sink);
        }
        let stage = blocked.at.stage;
        let open = unblock::open(blocked.at, &p.questions().unblocks_at(stage));
        let diff = self.branch_diff(p)?;
        let comments = self.comments(&p.ticket)?;
        let mut brief = self.brief(p, Role::Resolver, &comments, rules, None, current);
        brief.delegation = Some(level);
        brief.asked_by = Some(AskedBy {
            role: blocked.role,
            run: blocked.ran.run.clone(),
            premise_false: false,
            summary: blocked.result.summary.clone(),
            plan: design::effective_plan(p),
            findings: Vec::new(),
            diff,
        });
        brief.unblock = Some(UnblockBrief {
            stage,
            verdicts: open.clone(),
            ledger: blocked.result.artifacts.ledger.clone(),
        });
        // Why an earlier resolver result was refused, from the ticket ref
        // (OWL-191).
        brief.result_refusal = p.questions().resolver_refusal;
        let quotable = Quotable::of(&brief);
        let ran = match self.run_resolver(p, state, &brief, sink)? {
            ResolverRan::Fallback(fallback, ran) => {
                return parked(fallback.as_str(), Some(&ran.run), sink);
            }
            ResolverRan::Done(ran) => ran,
        };
        let Some(answer) = ran.result().and_then(|result| result.unblock.clone()) else {
            return parked("failed", Some(&ran.run), sink);
        };
        if answer.verdict == UnblockVerdict::Park {
            sink.emit(
                &ticket,
                Some(&blocked.ran.run),
                EventKind::Decision,
                unblock_event(blocked, UnblockVerdict::Park, Some(&ran.run), None, None),
            );
            return Ok(Unblocked::Park {
                first: Some(crate::writer::flatten(&answer.note)),
            });
        }
        // Held to the level as any decision: the resolver's label, its
        // basis's kind and quote.
        let given = [resolver::unblock_question(
            blocked.role,
            &blocked.result.summary,
            &open,
        )];
        let resolutions = [resolver::unblock_resolution(&answer)];
        let sorted = resolver::settle(&p.policy, &given, &resolutions, &quotable, false);
        if sorted.logged.is_empty() {
            return parked("refused", Some(&ran.run), sink);
        }
        let kept = UnblockKept {
            kept: KeptUnblock {
                stage,
                verdict: answer.verdict,
                step: answer.step,
            },
            when: answer.when,
        };
        let (decided, _) = self.log_decisions(
            p,
            state,
            &sorted.logged,
            (&ran.run, DecidedBy::Resolver),
            Some(kept),
            sink,
        )?;
        // Never an act without its DECISION on the ticket.
        if decided.is_empty() {
            return parked("unposted", Some(&ran.run), sink);
        }
        p.chain.settled_once();
        sink.emit(
            &ticket,
            Some(&blocked.ran.run),
            EventKind::Decision,
            unblock_event(blocked, answer.verdict, Some(&ran.run), None, Some(&answer)),
        );
        Ok(match answer.verdict {
            UnblockVerdict::Retry => Unblocked::Retry,
            UnblockVerdict::Rebase => Unblocked::Rebase,
            UnblockVerdict::PersonStep => Unblocked::PersonStep,
            UnblockVerdict::Park => Unblocked::Park { first: None },
        })
    }
}

/// The `decision` event of a verdict on a `blocked` (OWL-620): the verdict
/// (`unblock`), the stage, the blocked run's role, the resolver's run when
/// one ran, and, for a park the resolver did not give, why (`reason`: the
/// bound reached, `failed`, `usage_limit`, `refused`, `unposted`); a
/// `person_step`'s step and when it is due.
fn unblock_event(
    blocked: &Blocked<'_>,
    verdict: UnblockVerdict,
    resolver: Option<&str>,
    reason: Option<&str>,
    answer: Option<&owlshift_contracts::result::UnblockAnswer>,
) -> Data {
    let mut event = data([
        ("unblock", json!(verdict.as_str())),
        ("stage", json!(blocked.at.stage)),
        ("role", json!(blocked.role.as_str())),
        ("resolver", json!(resolver)),
    ]);
    if let Some(reason) = reason {
        event.insert("reason".to_owned(), json!(reason));
    }
    if let Some(step) = answer.and_then(|answer| answer.step) {
        event.insert("step".to_owned(), json!(step));
        event.insert("when".to_owned(), json!(answer.and_then(|a| a.when)));
    }
    event
}
