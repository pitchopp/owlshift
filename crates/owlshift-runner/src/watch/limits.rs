//! Watch and a harness's usage limit (OWL-404; architecture section 5,
//! "Parallel", "Usage limits", "Where it is checked"): before it starts a
//! step, watch reads the machine's usage state, and a step whose first run
//! would get no harness ([`effective_harness`]) does not start; a step whose
//! first action runs no model always goes. The step's child checks each of
//! its runs again ([`crate::on_demand::OnDemand`]), so this only spares a
//! child that would stop at its first check: a step taken for one that runs
//! a model when it runs none, or the other way round, costs nothing more.
//!
//! What watch read is one line when it changes, and, when a harness is held,
//! one `decision` event of the project (`watch` = `waits`, `reason` =
//! `usage_limit`).

use std::io::Write;

use jiff::Timestamp;
use serde_json::json;

use owlshift_contracts::Role;
use owlshift_contracts::event::EventKind;
use owlshift_core::state::TicketState;
use owlshift_core::usage::{Effective, Standing, effective_harness, own_harness};
use owlshift_core::vocab::{Harness, Stage};

use super::{Memory, Watch};
use crate::events::{EventSink, data};
use crate::on_demand;
use crate::ticket_ref::Stored;
use crate::usage as machine_usage;

/// The role of the first run of a resume at `stage` of the ticket whose ref
/// is `stored`: `None` for a Verify that delivers with no pass, a last fix's
/// or a commit whose pass passed and whose delivery a collision held back.
pub(super) fn resume_role(stage: Stage, stored: &Stored) -> Option<Role> {
    match stage {
        Stage::Intake => Some(Role::Intake),
        Stage::Design => Some(Role::Design),
        Stage::DesignReview => Some(Role::DesignReview),
        Stage::Verify if delivers_without_pass(stored) => None,
        Stage::Verify => Some(Role::Verify),
        Stage::Ready | Stage::Build | Stage::Deliver | Stage::Watch => Some(Role::Build),
    }
}

/// Whether a ticket at Verify delivers its commit with no Verify pass, as
/// the step itself decides it ([`on_demand::delivers_without_pass`]),
/// taking the commit held back as the branch's head.
fn delivers_without_pass(stored: &Stored) -> bool {
    TicketState::try_from(&stored.record.state)
        .is_ok_and(|state| on_demand::delivers_without_pass(&state, &stored.record))
}

impl Watch<'_> {
    /// Whether a step whose first run is of `role` waits for a harness's
    /// usage limit: never one that runs no model first (`None`).
    pub(super) fn usage_waits(
        &self,
        memory: &mut Memory,
        sink: &mut EventSink<'_>,
        out: &mut dyn Write,
        role: Option<Role>,
    ) -> bool {
        self.usage_wait(memory, sink, out, role).is_some()
    }

    /// [`Watch::usage_waits`], giving back the harness the role runs on by
    /// itself and, in milliseconds, until when the step waits at least.
    pub(super) fn usage_wait(
        &self,
        memory: &mut Memory,
        sink: &mut EventSink<'_>,
        out: &mut dyn Write,
        role: Option<Role>,
    ) -> Option<(Harness, i64)> {
        let role = role?;
        let now = (self.on_demand.clock)();
        let own = own_harness(role);
        let standing = self.usage_standing(sink, own, now);
        self.tell_usage(memory, sink, out, own, standing);
        let store = self.on_demand.usage;
        // A review role at Claude Code's limit goes on the declared fallback
        // when it can serve (OWL-406): its step starts.
        let fallback = self.on_demand.review_fallback(role).map(|_| Harness::Codex);
        let effective = effective_harness(role, own, fallback, |harness| {
            if harness == own {
                return standing;
            }
            store
                .standing(harness, machine_usage::ms(now))
                .unwrap_or(Standing::Free)
        });
        match effective {
            Effective::Wait { until_ms } => Some((own, until_ms)),
            Effective::Run(_) => None,
        }
    }

    /// Reads, at a pass's start, the standing of each harness a role runs on
    /// by itself, so a change is told even when no step asks.
    pub(super) fn read_usage(
        &self,
        memory: &mut Memory,
        sink: &mut EventSink<'_>,
        out: &mut dyn Write,
    ) {
        let now = (self.on_demand.clock)();
        let standing = self.usage_standing(sink, own_harness(Role::Build), now);
        self.tell_usage(memory, sink, out, own_harness(Role::Build), standing);
    }

    /// `harness`'s standing at `now`: free when its state cannot be read,
    /// a `warning` once per process.
    fn usage_standing(
        &self,
        sink: &mut EventSink<'_>,
        harness: Harness,
        now: Timestamp,
    ) -> Standing {
        let store = self.on_demand.usage;
        match store.standing(harness, machine_usage::ms(now)) {
            Ok(standing) => standing,
            Err(reason) => {
                let key = machine_usage::key(harness);
                if store.first_time(format!("usage_unread {key} {reason}")) {
                    sink.emit_project(
                        None,
                        EventKind::Warning,
                        data([
                            ("what", json!("usage_unread")),
                            ("harness", json!(key)),
                            ("reason", json!(reason)),
                        ]),
                    );
                }
                Standing::Free
            }
        }
    }

    /// One line, and an event when held, when `harness`'s standing changed
    /// since watch last told it.
    fn tell_usage(
        &self,
        memory: &mut Memory,
        sink: &mut EventSink<'_>,
        out: &mut dyn Write,
        harness: Harness,
        standing: Standing,
    ) {
        let told = memory
            .usage
            .get(&harness)
            .copied()
            .unwrap_or(Standing::Free);
        if told == standing {
            return;
        }
        memory.usage.insert(harness, standing);
        let shown = machine_usage::shown(harness);
        let line = match standing {
            Standing::Limited { until_ms } => {
                let until = machine_usage::at(until_ms);
                sink.emit_project(
                    None,
                    EventKind::Decision,
                    data([
                        ("watch", json!("waits")),
                        ("reason", json!("usage_limit")),
                        ("harness", json!(machine_usage::key(harness))),
                        ("until", json!(until)),
                    ]),
                );
                format!(
                    "{shown} is at its usage limit until {until}: no step that runs it starts \
                     before then"
                )
            }
            Standing::ProbeDue => format!(
                "{shown} gave no reset time and its deadline passed: the next step that runs it \
                 tries it"
            ),
            Standing::Free => format!("{shown} runs again"),
        };
        self.line(out, &line);
    }
}
