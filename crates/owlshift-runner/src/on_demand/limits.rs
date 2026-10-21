//! A harness's usage limit inside a step (OWL-404; architecture section 5,
//! "Parallel", "Usage limits", "Where it is checked" and "Told once"): every
//! run's harness is checked against the machine's usage state before the
//! run is made ([`OnDemand::admit_usage`]); what the run reported is kept
//! there after it ([`OnDemand::record_usage`]); and the operator is told of
//! a limit once per outage, as a ticket's own run meets it or is refused for
//! it ([`OnDemand::tell_usage`]), by the runner, as the quarantine is
//! (OWL-368). Each step is a `decision` event with `usage` = `waits`,
//! `probe`, `limited` or `free`; a file that cannot be read or kept is a
//! `warning`, once per process, and runs go on.

use std::cell::Cell;

use owlshift_core::usage::{
    self, Admission, Effective, Recorded, Standing, claim_told, effective_harness,
};
use owlshift_core::vocab::Harness as HarnessKind;

use super::*;
use crate::notify;

/// A run the usage state let start: its harness and the token it would
/// probe with. A probe's mark is cleared when this is dropped before its
/// report is kept ([`OnDemand::record_usage`]): whatever ended the step
/// before the run, or kept its report from the file, the next run probes
/// rather than waiting out a probe that never ran.
pub(super) struct Admitted<'s> {
    store: &'s UsageStore,
    harness: HarnessKind,
    probe: String,
    /// It marked itself the probe.
    probing: bool,
    /// Its report was kept.
    kept: Cell<bool>,
}

impl Drop for Admitted<'_> {
    fn drop(&mut self) {
        if self.probing && !self.kept.get() {
            let _ = self
                .store
                .update(self.harness, |state| usage::unstarted(state, &self.probe));
        }
    }
}

impl OnDemand<'_> {
    /// `harness`'s standing at the clock's time, free when its state cannot
    /// be read (`admit_usage` warns of that).
    pub fn usage_standing(&self, harness: HarnessKind) -> Standing {
        let now = machine_usage::ms((self.clock)());
        self.usage.standing(harness, now).unwrap_or(Standing::Free)
    }

    /// When `harness`'s limit ends, `None` when it is not limited.
    pub(super) fn limited_until(&self, harness: HarnessKind) -> Option<Timestamp> {
        match self.usage_standing(harness) {
            Standing::Limited { until_ms } => Some(machine_usage::at(until_ms)),
            Standing::Free | Standing::ProbeDue => None,
        }
    }

    /// The harness a review role (Design review, Verify) falls back to while
    /// Claude Code is at its usage limit (OWL-406, architecture section 5,
    /// "The effective harness"): the one the personal file names, when it is
    /// installed, logged in for agent runs and able to run in the sandbox
    /// here (the opt-in Codex reviewer's own check, OWL-348) and not itself
    /// limited. `None` for any other role, when none is named, when Claude
    /// Code is not limited, or when the fallback cannot serve: the review
    /// then waits for Claude Code's reset.
    pub fn review_fallback(&self, role: Role) -> Option<&CodexReviewer<'_>> {
        let fallback = self.review_fallback.as_ref()?;
        let own = usage::own_harness(role);
        // Asked of the machine only once there is a limit to fall back from.
        if !usage::is_review_role(role)
            || !matches!(self.usage_standing(own), Standing::Limited { .. })
        {
            return None;
        }
        let routing = RunRouting {
            harness: HarnessKind::Codex,
            ..PROJECT_DEFAULT
        };
        let usable = (fallback.ready)().is_ok()
            && fallback
                .design_review
                .harness(routing)
                .sandbox_needs(&self.executor.agent)
                .is_ok();
        let offered = usable.then_some(HarnessKind::Codex);
        matches!(
            effective_harness(role, own, offered, |harness| self.usage_standing(harness)),
            Effective::Run(other) if other != own
        )
        .then_some(fallback)
    }

    /// Checks the harness of a run of `role` on `harness` for `ticket`
    /// before anything of the run is made: `Ok(None)` for a harness whose
    /// usage nothing tracks; the run's admission when it may start, a run
    /// past a deadline marking itself the probe; [`Stop::UsageLimit`] until
    /// the harness's limit ends when it may not. The state is read first
    /// without the lock, which only a harness that is not free takes. A
    /// state that cannot be read or kept lets the run start.
    pub(super) fn admit_usage(
        &self,
        ticket: &TicketId,
        harness: &dyn Harness,
        role: Role,
        sink: &mut EventSink<'_>,
    ) -> Result<Option<Admitted<'_>>, Stop> {
        let Some(kind) = harness.usage_harness() else {
            return Ok(None);
        };
        let now = (self.clock)();
        let mut admitted = Admitted {
            store: self.usage,
            harness: kind,
            probe: format!(
                "{}-{ticket}-{}",
                std::process::id(),
                Timestamp::now().as_nanosecond()
            ),
            probing: false,
            kept: Cell::new(false),
        };
        let state = match self.usage.read_at(kind, machine_usage::ms(now)) {
            Ok(state) => state,
            Err(reason) => {
                self.usage_warning(ticket, "usage_unread", kind, &reason, sink);
                return Ok(Some(admitted));
            }
        };
        let standing_of = |of: HarnessKind| {
            if of == kind {
                usage::standing(&state, machine_usage::ms(now))
            } else {
                self.usage
                    .standing(of, machine_usage::ms(now))
                    .unwrap_or(Standing::Free)
            }
        };
        // The harness is already the run's: a review that falls back got its
        // fallback's router before it came here ([`OnDemand::review_fallback`]).
        if let Effective::Run(own) = effective_harness(role, kind, None, standing_of)
            && standing_of(own) == Standing::Free
        {
            return Ok(Some(admitted));
        }
        let admission = self
            .usage
            .update_at(kind, machine_usage::ms(now), |state| {
                usage::admit(state, &admitted.probe, machine_usage::ms(now))
            })
            .map(|(admission, opened)| {
                self.emit_capped(ticket, kind, &opened, None, sink);
                admission
            });
        let key = json!(machine_usage::key(kind));
        match admission {
            Ok(Admission::Run) => Ok(Some(admitted)),
            Ok(Admission::Probe) => {
                admitted.probing = true;
                sink.emit(
                    ticket,
                    None,
                    EventKind::Decision,
                    data([
                        ("usage", json!("probe")),
                        ("harness", key),
                        ("role", json!(role.as_str())),
                    ]),
                );
                Ok(Some(admitted))
            }
            Ok(Admission::Wait { until_ms }) => {
                let until = machine_usage::at(until_ms);
                sink.emit(
                    ticket,
                    None,
                    EventKind::Decision,
                    data([
                        ("usage", json!("waits")),
                        ("harness", key),
                        ("role", json!(role.as_str())),
                        ("until", json!(until)),
                    ]),
                );
                Err(Stop::UsageLimit {
                    resets_at: Some(until),
                })
            }
            Err(reason) => {
                self.usage_warning(ticket, "usage_unread", kind, &reason, sink);
                // A pause the personal file's cap opens needs no write to
                // hold: the provider would not refuse the run for it.
                match capped_until(&state, machine_usage::ms(now)) {
                    Some(until_ms) => Err(Stop::UsageLimit {
                        resets_at: Some(machine_usage::at(until_ms)),
                    }),
                    None => Ok(Some(admitted)),
                }
            }
        }
    }

    /// The `decision` event `usage` = `capped` for each pause of a usage cap
    /// `opened` on `harness` (OWL-405), as run `run` of `ticket` reported.
    fn emit_capped(
        &self,
        ticket: &TicketId,
        harness: HarnessKind,
        opened: &[usage::Episode],
        run: Option<&str>,
        sink: &mut EventSink<'_>,
    ) {
        for episode in opened {
            sink.emit(
                ticket,
                run,
                EventKind::Decision,
                data([
                    ("usage", json!("capped")),
                    ("harness", json!(machine_usage::key(harness))),
                    ("until", json!(machine_usage::at(episode.until_ms))),
                ]),
            );
        }
    }

    /// Keeps what run `run` of `ticket`, admitted as `admitted`, reported:
    /// its windows, its limit, and, for the probe, whether its harness
    /// answered. A `decision` event says what changed.
    pub(super) fn record_usage(
        &self,
        ticket: &TicketId,
        run: &str,
        admitted: &Admitted<'_>,
        report: &RunReport,
        sink: &mut EventSink<'_>,
    ) {
        let kind = admitted.harness;
        let now = machine_usage::ms((self.clock)());
        let reported = machine_usage::reported(report);
        let recorded = self
            .usage
            .update_at(kind, now, |state| {
                usage::record(state, &reported, &admitted.probe, now)
            })
            .map(|(recorded, opened)| {
                self.emit_capped(ticket, kind, &opened, Some(run), sink);
                recorded
            });
        admitted.kept.set(recorded.is_ok());
        let key = json!(machine_usage::key(kind));
        let event = match recorded {
            Ok(Recorded::Limited(episode)) => data([
                ("usage", json!("limited")),
                ("harness", key),
                ("until", json!(machine_usage::at(episode.until_ms))),
                (
                    "reset",
                    json!(episode.kind == usage::EpisodeKind::Limit { reset: true }),
                ),
            ]),
            Ok(Recorded::Freed) => data([("usage", json!("free")), ("harness", key)]),
            Ok(Recorded::Nothing | Recorded::Unproven) => return,
            Err(reason) => {
                return self.usage_warning(ticket, "usage_not_kept", kind, &reason, sink);
            }
        };
        sink.emit(ticket, Some(run), EventKind::Decision, event);
    }

    /// Tells the operator that `harness`'s runs wait, `ticket`'s first, once
    /// per outage: the episode that holds it is marked told under its lock
    /// and shown, unless it was told. A machine that shows no notification
    /// marks nothing, so that one that can still tells it; a notification
    /// that cannot be shown is a warning and stays told, as the quarantine's
    /// (OWL-368). A state that cannot be kept tells the limit unmarked, from
    /// `resets_at`; one where nothing holds the harness, its reset already
    /// past, tells nothing.
    pub(super) fn tell_usage(
        &self,
        ticket: &TicketId,
        harness: &dyn Harness,
        resets_at: Option<Timestamp>,
        sink: &mut EventSink<'_>,
    ) {
        let (Some(notifier), Some(kind)) = (self.notifier, harness.usage_harness()) else {
            return;
        };
        let now = (self.clock)();
        let claimed = self.usage.update_at(kind, machine_usage::ms(now), |state| {
            let standing = usage::standing(state, machine_usage::ms(now));
            (claim_told(state, machine_usage::ms(now)), standing)
        });
        let line = match claimed {
            Ok(((Some(episode), _), _)) => machine_usage::episode_line(ticket, kind, &episode),
            // Told already, or over already: a reset time past holds nothing.
            Ok(((None, _), _)) => return,
            Err(reason) => {
                self.usage_warning(ticket, "usage_not_kept", kind, &reason, sink);
                let until = resets_at.filter(|at| *at > now).unwrap_or_else(|| {
                    machine_usage::at(machine_usage::ms(now) + usage::DEADLINE_MS)
                });
                machine_usage::line(ticket, kind, until, resets_at.is_some_and(|at| at > now))
            }
        };
        if let Err(error) = notifier.notify(&notify::with_link(line, self.tracker, ticket)) {
            sink.emit(ticket, None, EventKind::Warning, notify::not_shown(&error));
        }
    }

    /// The `warning` event `what` of `harness`'s state, for `reason`: once
    /// per process.
    fn usage_warning(
        &self,
        ticket: &TicketId,
        what: &str,
        kind: HarnessKind,
        reason: &str,
        sink: &mut EventSink<'_>,
    ) {
        let key = machine_usage::key(kind);
        if !self.usage.first_time(format!("{what} {key} {reason}")) {
            return;
        }
        sink.emit(
            ticket,
            None,
            EventKind::Warning,
            data([
                ("what", json!(what)),
                ("harness", json!(key)),
                ("reason", json!(reason)),
            ]),
        );
    }
}

/// When the pauses of a usage cap in `state` end, the latest of them, if one
/// holds the harness at `now`.
fn capped_until(state: &usage::State, now: i64) -> Option<i64> {
    state
        .episodes
        .iter()
        .filter(|episode| matches!(episode.kind, usage::EpisodeKind::Cap { .. }))
        .map(|episode| episode.until_ms)
        .filter(|until| *until > now)
        .max()
}
