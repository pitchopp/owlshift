//! The cancels watch reads at each pass (OWL-474, architecture section 5,
//! "The local web UI", "Cancel"): a ticket whose cancel file stands, written
//! by `owlshift cancel` ([`crate::cancel::ask`]), is parked once no step of
//! it runs. Its step under way stopped itself already, checked
//! ([`crate::cancel::Watcher`]); the pass after it comes at once.
//!
//! - *Parked:* a ticket whose ref the pass read goes to
//!   [`OnDemand::cancel_seen`], which parks it under its lock while the ref
//!   is still the one read; the file is then removed and the park said.
//! - *Dropped:* one already parked, at Ready, or whose delivery ended, and
//!   one with no ref while no step of it runs anywhere (its lock free):
//!   the file is removed, said in a line and a `decision` event (`cancel`
//!   `dropped`).
//! - *Waiting:* a ticket whose step runs, whose ref moved or does not read,
//!   which another command holds, or whose park was refused, keeps its
//!   file for a later pass, a refusal warned of once while it lasts.
//!
//! Every ticket a cancel stands on is left alone for the rest of the pass,
//! as one whose step's child runs. Only files named `cancel` in the
//! tickets' folders are read or removed: those folders hold other files.
//!
//! [`OnDemand::cancel_seen`]: crate::on_demand::OnDemand::cancel_seen

use std::fs;
use std::io::{self, Write};
use std::path::Path;

use serde_json::json;

use owlshift_contracts::event::EventKind;
use owlshift_contracts::ids::TicketId;

use super::{Memory, Seen, Watch};
use crate::cancel;
use crate::events::{Data, EventSink, data};
use crate::on_demand::{Canceled, Stop};
use crate::project;
use crate::step::{self, Stepped};
use crate::ticket_ref::Stored;

/// The key of a failing read of the tickets' folder in
/// `Memory::cancel_failing`, which no ticket's folder can be named.
const FOLDER: &str = "";

impl Watch<'_> {
    /// The cancels standing on this project's tickets, given the refs this
    /// pass read: each parked, dropped or left waiting as the module says.
    /// Returns the tickets a cancel stood on, which the rest of the pass
    /// leaves alone.
    pub(super) fn cancels(
        &self,
        memory: &mut Memory,
        sink: &mut EventSink<'_>,
        out: &mut dyn Write,
        read: &[(TicketId, Result<Stored, String>)],
    ) -> Vec<TicketId> {
        let folder = self.on_demand.dirs.tickets();
        let entries = match fs::read_dir(&folder) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                memory.cancel_failing.clear();
                return Vec::new();
            }
            Err(error) => {
                let text = format!(
                    "the tickets' folder {} could not be read for their cancels: {error}",
                    folder.display()
                );
                self.cancel_waits(memory, sink, out, None, FOLDER, text);
                return Vec::new();
            }
        };
        let mut standing: Vec<(String, std::path::PathBuf)> = entries
            .flatten()
            .map(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                (name, entry.path().join(cancel::FILE))
            })
            .filter(|(_, path)| fs::symlink_metadata(path).is_ok())
            .collect();
        standing.sort();
        memory
            .cancel_failing
            .retain(|name, _| standing.iter().any(|(folder, _)| folder == name));
        let mut left = Vec::new();
        for (name, path) in standing {
            let named = |ticket: &TicketId| project::component(ticket.as_str()) == name;
            // Its step under way stops itself; the park follows its end.
            if let Some(running) = memory.running.iter().find(|step| named(step.ticket())) {
                left.push(running.ticket().clone());
                continue;
            }
            match read.iter().find(|(ticket, _)| named(ticket)) {
                Some((ticket, Ok(stored))) => {
                    left.push(ticket.clone());
                    self.cancel(memory, sink, out, ticket, stored, (&name, &path));
                }
                // Its unreadable ref is warned of by the pass.
                Some((ticket, Err(_))) => left.push(ticket.clone()),
                None => self.cancel_unkept(memory, sink, out, &name, &path),
            }
        }
        left
    }

    /// Parks `ticket`, whose ref this pass read as `stored`, its cancel file
    /// being `path` in the folder `name`; or drops the file, or leaves it.
    fn cancel(
        &self,
        memory: &mut Memory,
        sink: &mut EventSink<'_>,
        out: &mut dyn Write,
        ticket: &TicketId,
        stored: &Stored,
        (name, path): (&str, &Path),
    ) {
        let request = cancel::read_at(path).flatten();
        let canceled = self
            .on_demand
            .cancel_seen(ticket, &stored.commit, request.as_ref(), sink);
        match canceled {
            // Busy, or its ref moved: the next pass reads it again.
            None => {}
            Some(Ok(Canceled::Parked(stop))) => {
                memory.cancel_failing.remove(name);
                // The turn is chosen again at once without it.
                memory.stepped = true;
                // A park whose state was not kept holds the ticket until a
                // person acts, as a continue's (OWL-158): the comment is
                // not posted again.
                if matches!(stop, Stop::NotKept { .. }) {
                    let end = step::end(&Stepped::Ran(Err(stop.clone())));
                    let now = (self.on_demand.clock)();
                    let seen = Seen::at(stored.at(), None);
                    memory.holds.after(ticket, seen, &end, None, now);
                }
                self.line(
                    out,
                    &format!("{ticket}: {stop}; `owlshift continue {ticket}` restarts it"),
                );
                self.remove_cancel(sink, out, Some(ticket), path);
            }
            Some(Ok(Canceled::NotParked(not))) => {
                memory.cancel_failing.remove(name);
                self.cancel_dropped(sink, out, Some(ticket), name, &not.words(ticket), path);
            }
            Some(Err(stop)) => {
                let text = format!("the cancel of {ticket} waits: {stop}");
                self.cancel_waits(memory, sink, out, Some(ticket), name, text);
            }
        }
    }

    /// The cancel file `path` of the folder `name`, whose ticket has no ref:
    /// dropped once no step of it runs anywhere, its lock free; left while
    /// one does, its ref coming with that step's first run.
    fn cancel_unkept(
        &self,
        memory: &mut Memory,
        sink: &mut EventSink<'_>,
        out: &mut dyn Write,
        name: &str,
        path: &Path,
    ) {
        let folder = self.on_demand.dirs.tickets().join(name);
        match self.on_demand.dirs.ticket_lock_held_in(&folder) {
            Ok(true) => {}
            Ok(false) => {
                memory.cancel_failing.remove(name);
                let words = format!(
                    "Owlshift has nothing of {name} in flight: no step of it runs and it keeps no \
                     record of it"
                );
                self.cancel_dropped(sink, out, None, name, &words, path);
            }
            Err(error) => {
                let text =
                    format!("the cancel of {name} waits: its lock could not be read: {error}");
                self.cancel_waits(memory, sink, out, None, name, text);
            }
        }
    }

    /// Drops the cancel file `path` of the folder `name`, saying why in
    /// `words`: a line and a `decision` event.
    fn cancel_dropped(
        &self,
        sink: &mut EventSink<'_>,
        out: &mut dyn Write,
        ticket: Option<&TicketId>,
        name: &str,
        words: &str,
        path: &Path,
    ) {
        self.line(out, &format!("the cancel of {name} is dropped: {words}"));
        let mut event = data([("cancel", json!("dropped")), ("why", json!(words))]);
        if ticket.is_none() {
            event.insert("folder".to_owned(), json!(name));
        }
        emit(sink, ticket, EventKind::Decision, event);
        self.remove_cancel(sink, out, ticket, path);
    }

    /// Removes the cancel file `path`, a failure warned of: the next pass
    /// finds it again.
    fn remove_cancel(
        &self,
        sink: &mut EventSink<'_>,
        out: &mut dyn Write,
        ticket: Option<&TicketId>,
        path: &Path,
    ) {
        let Err(error) = cancel::remove_at(path) else {
            return;
        };
        let text = format!(
            "warning: the cancel file {} could not be removed: {error}",
            path.display()
        );
        self.line(out, &text);
        let event = data([
            ("what", json!("cancel_not_removed")),
            ("reason", json!(error.to_string())),
        ]);
        emit(sink, ticket, EventKind::Warning, event);
    }

    /// Tells that a cancel waits, `text` saying why, once while it says the
    /// same for the folder `name`: a line and a `warning` event.
    fn cancel_waits(
        &self,
        memory: &mut Memory,
        sink: &mut EventSink<'_>,
        out: &mut dyn Write,
        ticket: Option<&TicketId>,
        name: &str,
        text: String,
    ) {
        if memory.cancel_failing.get(name) == Some(&text) {
            return;
        }
        self.line(out, &text);
        let event = data([("what", json!("cancel_waits")), ("reason", json!(text))]);
        emit(sink, ticket, EventKind::Warning, event);
        memory.cancel_failing.insert(name.to_owned(), text);
    }
}

/// Records `event` of `kind` on `ticket` when there is one, else on the
/// project.
fn emit(sink: &mut EventSink<'_>, ticket: Option<&TicketId>, kind: EventKind, event: Data) {
    match ticket {
        Some(ticket) => {
            sink.emit(ticket, None, kind, event);
        }
        None => {
            sink.emit_project(None, kind, event);
        }
    }
}
