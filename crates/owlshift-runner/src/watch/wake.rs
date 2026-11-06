//! A dark wake held (OWL-485, architecture section 5, "Dark wake"): while
//! macOS keeps the machine up without graphics, which is how it runs
//! maintenance and Power Nap wakes, with no network for the most part, a
//! watch dispatches nothing and starts no step. The steps under way finish
//! and the runs inside them go on; the passes go on reading answers, pull
//! requests and stages, and Owlshift's merge on green still merges, as it
//! starts no run and spends nothing. The first pass after a full wake
//! decides everything again.
//!
//! The machine is read ([`owlshift_platform::wake::read`]) once per
//! iteration of the watch loop and again before each step starts, so a
//! machine that fell into a dark wake in the middle of a pass starts
//! nothing further. A reading that cannot be made ([`Wake::Unknown`])
//! leaves the state as it was: it holds nothing on a machine never known to
//! be in a dark wake, and lifts nothing on one known to be. Each change is
//! said once, as a line and a project `decision` event.

use std::io::Write;

use serde_json::json;

use owlshift_contracts::event::EventKind;
use owlshift_platform::wake::Wake;

use super::{Memory, Watch};
use crate::events::{EventSink, data};

impl Watch<'_> {
    /// Whether a dispatch or a step's start is held back now: an operator's
    /// pause stands ([`Self::operator_paused`]) or the machine is in a dark
    /// wake ([`Self::dark_wake`]). `fresh` reads the machine again instead
    /// of using this iteration's reading.
    pub(super) fn held_back(
        &self,
        memory: &mut Memory,
        sink: &mut EventSink<'_>,
        out: &mut dyn Write,
        fresh: bool,
    ) -> bool {
        self.operator_paused(memory, sink, out) || self.dark_wake(memory, sink, out, fresh)
    }

    /// Whether the machine is in a dark wake, as last known. A change is one
    /// line and one project `decision` event (`watch` `dark_wake` /
    /// `full_wake`).
    pub(super) fn dark_wake(
        &self,
        memory: &mut Memory,
        sink: &mut EventSink<'_>,
        out: &mut dyn Write,
        fresh: bool,
    ) -> bool {
        let reading = match memory.wake_read {
            Some(reading) if !fresh => reading,
            _ => {
                let reading = (self.on_demand.wake)();
                memory.wake_read = Some(reading);
                reading
            }
        };
        let dark = match reading {
            Wake::Dark => true,
            Wake::Full => false,
            Wake::Unknown => memory.dark_wake,
        };
        if dark != memory.dark_wake {
            memory.dark_wake = dark;
            if dark {
                self.line(
                    out,
                    "the machine is in a dark wake (macOS keeps it up without graphics): no \
                     ticket is dispatched and no step starts; the steps under way finish, and \
                     answers, pull requests and stages are still read, and a merge on green \
                     still happens. The next full wake lifts it",
                );
                sink.emit_project(
                    None,
                    EventKind::Decision,
                    data([("watch", json!("dark_wake"))]),
                );
            } else {
                self.line(
                    out,
                    "the machine is in a full wake again: tickets are dispatched and steps start \
                     again",
                );
                sink.emit_project(
                    None,
                    EventKind::Decision,
                    data([("watch", json!("full_wake"))]),
                );
            }
        }
        if dark {
            memory.dark_wake_held = true;
        }
        dark
    }
}
