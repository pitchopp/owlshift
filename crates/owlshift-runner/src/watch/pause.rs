//! An operator's pause (OWL-438, architecture section 5, "Pause"):
//! `<data>/service/pause` for every project of the machine, `<project
//! data>/pause` for one, written by `owlshift pause` or, for a project whose
//! watch could not start, by the daemon. While one stands a watch
//! dispatches nothing and starts no step, Owlshift's own merge on green
//! included; the steps under way finish, and its passes go on reading
//! answers, pull requests and stages. Removed, the next pass goes on.
//!
//! The pause is read at each pass and before each step, so one written in
//! the middle of a pass starts no further step. Each pause, told apart by
//! its scope and what its file holds, is said once, and its lift once.

use std::io::Write;

use serde_json::json;

use owlshift_contracts::event::EventKind;

use super::{Memory, Watch};
use crate::daemon::requests::{self, PauseScope};
use crate::events::{EventSink, data};

impl Watch<'_> {
    /// Whether an operator's pause stands on this watch's project now. A
    /// pause found, or another one replacing it, is one line and one
    /// project `decision` event (`watch` `paused`, with its `scope`, `by`,
    /// `at` and `why`); its lift likewise (`watch` `resumed`).
    pub(super) fn operator_paused(
        &self,
        memory: &mut Memory,
        sink: &mut EventSink<'_>,
        out: &mut dyn Write,
    ) -> bool {
        let standing = requests::operator_pause(self.on_demand.dirs);
        if memory.operator_pause == standing {
            return standing.is_some();
        }
        match &standing {
            Some((scope, request)) => {
                let whose = match scope {
                    PauseScope::Machine => "every project of this machine is paused",
                    PauseScope::Project => "this project is paused",
                };
                let asked = request.as_ref().map_or_else(
                    || "its pause file does not say who asked".to_owned(),
                    |request| format!("by {} at {}: {}", request.by, request.at, request.why),
                );
                self.line(
                    out,
                    &format!(
                        "{whose} ({asked}): no ticket is dispatched and no step starts, nor a \
                         merge on green; the steps under way finish, and answers, pull requests \
                         and stages are still read. `owlshift resume` lifts it"
                    ),
                );
                let mut event = data([("watch", json!("paused")), ("scope", json!(scope.key()))]);
                if let Some(request) = request {
                    event.insert("by".to_owned(), json!(request.by));
                    event.insert("at".to_owned(), json!(request.at.to_string()));
                    event.insert("why".to_owned(), json!(request.why));
                }
                sink.emit_project(None, EventKind::Decision, event);
            }
            None => {
                self.line(
                    out,
                    "the pause is lifted: tickets are dispatched and steps start again",
                );
                sink.emit_project(
                    None,
                    EventKind::Decision,
                    data([("watch", json!("resumed"))]),
                );
            }
        }
        let paused = standing.is_some();
        memory.operator_pause = standing;
        paused
    }
}
