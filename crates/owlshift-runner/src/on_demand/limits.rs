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
        let state = match self.usage.read(kind) {
            Ok(state) => state,
            Err(reason) => {
                self.usage_warning(ticket, "usage_unread", kind, &reason, sink);
                return Ok(Some(admitted));
            }
        };
        let standing_of = |of: HarnessKind| {
            let read = if of == kind {
                Ok(state.clone())
            } else {
                self.usage.read(of)
            };
            read.map_or(Standing::Free, |state| {
                usage::standing(&state, machine_usage::ms(now))
            })
        };
        if let Effective::Run(own) = effective_harness(role, kind, standing_of)
            && standing_of(own) == Standing::Free
        {
            return Ok(Some(admitted));
        }
        let admission = self.usage.update(kind, |state| {
            usage::admit(state, &admitted.probe, machine_usage::ms(now))
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
                Ok(Some(admitted))
            }
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
        let recorded = self.usage.update(kind, |state| {
            usage::record(state, &reported, &admitted.probe, now)
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
        let claimed = self.usage.update(kind, |state| {
            let standing = usage::standing(state, machine_usage::ms(now));
            (claim_told(state, machine_usage::ms(now)), standing)
        });
        let line = match claimed {
            Ok((Some(episode), _)) => machine_usage::episode_line(ticket, kind, &episode),
            // Told already, or over already: a reset time past holds nothing.
            Ok((None, _)) => return,
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
