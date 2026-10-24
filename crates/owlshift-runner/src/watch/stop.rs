//! How a watch is asked to stop (OWL-161, architecture section 5, "The
//! background service", "Three ways to stop"): by the signals of its own
//! process, which stops by degrees (`owlshift_platform::process::
//! stop_by_degrees`), or by the project's stop file, `<project data>/stop`,
//! a drain asked from outside the process (`owlshift stop`, an upgrade),
//! which the watch that drains on it removes.
//!
//! The file asks a drain by standing: what it holds is its writer's.

use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use serde_json::json;

use owlshift_contracts::event::EventKind;
use owlshift_platform::process::{self, Degree};

use super::{Memory, Watch};
use crate::events::{EventSink, data, printable};
use crate::project::ProjectDirs;

/// How often a watch that stops looks at its steps' children.
const POLL: Duration = Duration::from_millis(100);

/// How long a watch waits for its steps' children after a checked stop
/// before it stops them now (architecture section 5, "Three ways to stop").
pub const CHECKED_STOP_BOUND: Duration = Duration::from_secs(60);

/// The project's stop file.
pub fn file(dirs: &ProjectDirs) -> PathBuf {
    dirs.root().join("stop")
}

/// Whether the project's stop file stands, whatever it is.
pub fn stands(dirs: &ProjectDirs) -> bool {
    fs::symlink_metadata(file(dirs)).is_ok()
}

/// Removes the project's stop file, if it is there.
pub fn remove(dirs: &ProjectDirs) -> io::Result<()> {
    match fs::remove_file(file(dirs)) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

/// What asks a watch to stop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Asked {
    /// Its process's signals, this degree.
    Signal(Degree),
    /// The project's stop file: a drain.
    File,
}

impl Asked {
    /// The degree asked.
    pub(super) fn degree(self) -> Degree {
        match self {
            Self::Signal(degree) => degree,
            Self::File => Degree::Drain,
        }
    }
}

/// What asks the watch of `dirs` to stop, the strongest first: its
/// process's signals, then the stop file.
pub(super) fn asked(dirs: &ProjectDirs) -> Option<Asked> {
    match process::asked() {
        Some(degree) => Some(Asked::Signal(degree)),
        None => stands(dirs).then_some(Asked::File),
    }
}

impl Watch<'_> {
    /// What asks this watch to stop, if anything.
    pub(super) fn stopping(&self) -> Option<Asked> {
        asked(self.on_demand.dirs)
    }

    /// Ends the watch once `asked` asks it to stop: it starts nothing more,
    /// waits for its steps' children and applies the end of each, then
    /// removes the stop file, if one stands. Each degree reached is one line
    /// and one project `decision` event. A checked stop's children were
    /// sent SIGTERM by the platform as it was asked, which also stops the
    /// watch now past [`CHECKED_STOP_BOUND`].
    pub(super) fn stop(
        &self,
        memory: &mut Memory,
        sink: &mut EventSink<'_>,
        out: &mut dyn Write,
        asked: Asked,
    ) {
        self.told_stop(memory, sink, out, asked);
        while !memory.running.is_empty() {
            if let Some(asked) = self.stopping()
                && Some(asked.degree()) > memory.stop_told
            {
                self.told_stop(memory, sink, out, asked);
            }
            if !self.reap(memory, sink, out) {
                thread::sleep(POLL);
            }
        }
        let dirs = self.on_demand.dirs;
        if let Err(error) = remove(dirs) {
            let _ = writeln!(
                out,
                "{}",
                printable(&format!(
                    "owlshift: warning: the stop file {} could not be removed: {error}",
                    file(dirs).display()
                ))
            );
        }
        let ended = match memory.stop_told {
            Some(Degree::Checked) => "its steps stopped and their runs checked",
            _ => "drained",
        };
        let _ = writeln!(
            out,
            "Watch of {} ended: {ended}.",
            printable(sink.project())
        );
    }

    /// Says that `asked` asks this watch to stop, once per degree: a line
    /// and a project `decision` event.
    fn told_stop(
        &self,
        memory: &mut Memory,
        sink: &mut EventSink<'_>,
        out: &mut dyn Write,
        asked: Asked,
    ) {
        let degree = asked.degree();
        if memory.stop_told >= Some(degree) {
            return;
        }
        memory.stop_told = Some(degree);
        let project = printable(sink.project());
        let under_way = memory.running.len();
        let finish = match under_way {
            0 => "no step is under way, so watch ends now".to_owned(),
            1 => "the step under way finishes, then watch ends".to_owned(),
            n => format!("the {n} steps under way finish, then watch ends"),
        };
        let line = match asked {
            Asked::File => format!(
                "Draining {project}: its stop file asks it ({}). No new step starts; {finish}.",
                printable(&file(self.on_demand.dirs).display().to_string())
            ),
            Asked::Signal(Degree::Drain) if under_way == 0 => {
                format!("Draining {project}: no new step starts; {finish}.")
            }
            Asked::Signal(Degree::Drain) => format!(
                "Draining {project}: no new step starts; {finish}. A second Ctrl-C stops them, \
                 checking their runs; a third stops everything now, leaving their runs unchecked."
            ),
            Asked::Signal(Degree::Checked) if under_way == 0 => {
                format!("Stopping {project}: {finish}.")
            }
            Asked::Signal(Degree::Checked) => format!(
                "Stopping {project}: the steps under way stop their runs and check them, for at \
                 most {} seconds; a Ctrl-C now stops everything at once, leaving their runs \
                 unchecked.",
                CHECKED_STOP_BOUND.as_secs()
            ),
        };
        let _ = writeln!(out, "{line}");
        let (stop, by) = match asked {
            Asked::File => ("drain", "stop_file"),
            Asked::Signal(Degree::Drain) => ("drain", "signal"),
            Asked::Signal(Degree::Checked) => ("checked", "signal"),
        };
        sink.emit_project(
            None,
            EventKind::Decision,
            data([
                ("watch", json!("stop")),
                ("stop", json!(stop)),
                ("asked", json!(by)),
            ]),
        );
    }
}
