//! A ticket's step, in watch's own process or in a child process (OWL-399,
//! architecture section 5, "Parallel", "The step, in a child process").
//!
//! Watch decides what a step is and writes it as an envelope
//! ([`StepEnvelope`]); [`run`] carries it out through the entry points of
//! `continue`'s and the dispatch's own paths, which hold the ticket's lock
//! for the step's whole length, the project's for short sections only since
//! OWL-401, and refuse, running nothing, a ticket ref or a visible stage
//! that moved since watch read it. `owlshift watch --drain`
//! runs it in a child, `owlshift continue <TICKET> --step <envelope>`, whose
//! [`child`] writes the step's outcome record last ([`StepOutcome`],
//! `outcome-<attempt>.json` in the ticket's folder), which watch reads when
//! it reaps the child ([`Children`]); plain `owlshift watch` and the test
//! bench run it in watch's own process.
//!
//! What a step carries to the next, the resolver's chain and the late-comment
//! bound (architecture section 5, "What a step carries"), is kept in
//! `carry.json` beside the records ([`TicketCarry`]), tied to the ticket-ref
//! commit the step left and a generation; a resume uses it only while both
//! are those its envelope names. A step that ran nothing leaves it as it is;
//! one that stopped [`Stop::Stepped`] keeps it while it still holds its
//! ticket's lock, written when it moved the ref's progress, so that a
//! person's `do` or `continue` after it, which removes it, has the last
//! word; any other removes it.

mod children;

pub use children::{Children, Running};

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use jiff::Timestamp;
use serde_json::json;

use owlshift_adapters::forge::ReviewsRead;
use owlshift_contracts::event::EventKind;
use owlshift_contracts::format::Format;
use owlshift_contracts::ids::TicketId;
use owlshift_contracts::step::{
    KeptChain, LandedKind, RefAfter, Shown, StepEnd, StepEnvelope, StepHold, StepKind, StepOutcome,
    StepOutside, TicketCarry,
};

use crate::events::{Data, EventSink, data};
use crate::follow::Following;
use crate::in_flight::Outside;
use crate::on_demand::{self, Carried, Delivered, DispatchSeen, Landed, OnDemand, Stop, Taken};
use crate::project::ProjectDirs;
use crate::slots::{Holder, MachineSlots, Slot};
use crate::ticket_ref::{self, Progress, RefAt};
use crate::watch::{self, Seen};
use crate::writer::VisibleStage;

/// The file in a ticket's folder that keeps what its last step carries.
pub const CARRY_FILE: &str = "carry.json";

/// How a step ended, as [`run`] tells it.
#[derive(Clone, Debug, PartialEq)]
pub enum Stepped {
    /// The ticket ref, or the stage watch read, moved since: nothing ran.
    Stale,
    /// The drain's dispatch found the open pull requests at their cap.
    Capped { open_prs: u32, max_open_prs: u32 },
    /// The drain's gesture was not an admitter's.
    NotAdmitted { account: String, reason: String },
    /// The step ran, or was refused, with this outcome, as `continue`'s.
    Ran(Result<Delivered, Stop>),
}

/// Carries out `envelope`'s step of its ticket, with watch's memory of the
/// pull requests it follows, `following`, and, for a review loop's start,
/// the reviews watch read (`None`: read again). What the step carries is
/// read from `carry.json` and kept there after it.
pub fn run(
    on_demand: &OnDemand<'_>,
    envelope: &StepEnvelope,
    following: &mut Following,
    reviews: Option<ReviewsRead>,
    outside: Option<Outside>,
    sink: &mut EventSink<'_>,
) -> Stepped {
    let stepped = step(on_demand, envelope, following, reviews, outside, sink);
    keep_carry(on_demand, envelope, &stepped, sink);
    stepped
}

fn step(
    on_demand: &OnDemand<'_>,
    envelope: &StepEnvelope,
    following: &mut Following,
    reviews: Option<ReviewsRead>,
    outside: Option<Outside>,
    sink: &mut EventSink<'_>,
) -> Stepped {
    let ticket = &envelope.ticket;
    let ran =
        |outcome: Option<Result<Delivered, Stop>>| outcome.map_or(Stepped::Stale, Stepped::Ran);
    if let StepKind::Intake = envelope.step {
        return match on_demand.intake_seen(ticket, envelope.commit.as_deref(), sink) {
            None => Stepped::Stale,
            Some(Taken::NotAdmitted { account, reason }) => {
                Stepped::NotAdmitted { account, reason }
            }
            Some(Taken::Ran(outcome)) => Stepped::Ran(as_reported(ticket, outcome)),
        };
    }
    // Every other step starts from a ticket ref.
    let Some(commit) = envelope.commit.as_deref() else {
        return Stepped::Stale;
    };
    match &envelope.step {
        StepKind::Intake => Stepped::Stale,
        StepKind::Answer { slot_held_by } => ran(on_demand.continue_seen(
            ticket,
            commit,
            slot_held_by.as_ref(),
            decision(envelope),
            sink,
        )),
        StepKind::Dispatch => match on_demand.dispatch_seen(ticket, commit, outside, sink) {
            None => Stepped::Stale,
            Some(DispatchSeen::Capped {
                open_prs,
                max_open_prs,
            }) => Stepped::Capped {
                open_prs,
                max_open_prs,
            },
            Some(DispatchSeen::Taken(Taken::NotAdmitted { account, reason })) => {
                Stepped::NotAdmitted { account, reason }
            }
            Some(DispatchSeen::Taken(Taken::Ran(outcome))) => Stepped::Ran(outcome),
        },
        StepKind::Resume { shown, .. } => ran(on_demand.continue_shown(
            ticket,
            commit,
            visible(*shown),
            carried(on_demand.dirs, envelope),
            decision(envelope),
            sink,
        )),
        StepKind::Repair { failing_for_ms } => ran(on_demand.repair_seen(
            ticket,
            commit,
            failing_for_ms.map(Duration::from_millis),
            following,
            sink,
        )),
        StepKind::Correct => ran(on_demand.correct_seen(ticket, commit, following, sink)),
        StepKind::Review => {
            let read = match reviews {
                Some(read) => read,
                None => match read_reviews(on_demand, ticket, commit, following, sink) {
                    Ok(read) => read,
                    Err(stepped) => return stepped,
                },
            };
            ran(on_demand.review_seen(ticket, commit, read, sink))
        }
    }
}

/// How an intake's outcome is reported, as a continue's: a record kept is
/// the ticket back in the column, waiting for its dispatch.
fn as_reported<T>(ticket: &TicketId, outcome: Result<T, Stop>) -> Result<Delivered, Stop> {
    match outcome {
        Ok(_) => Err(Stop::Ready {
            ticket: ticket.clone(),
        }),
        Err(stop) => Err(stop),
    }
}

/// Watch's `decision` event for a continue it runs, recorded by the step
/// once the ticket ref, and the stage when one was read, are confirmed
/// under the lock.
fn decision(envelope: &StepEnvelope) -> Data {
    match &envelope.step {
        StepKind::Resume { stage, .. } => data([
            ("watch", json!("continue")),
            ("stage", json!(on_demand::stage_name(*stage))),
        ]),
        _ => data([
            ("watch", json!("continue")),
            ("answer", json!(envelope.answer)),
        ]),
    }
}

/// The stage the tracker must still show.
fn visible(shown: Shown) -> VisibleStage {
    match shown {
        Shown::Working => VisibleStage::Working,
        Shown::NeedsInput => VisibleStage::NeedsInput,
    }
}

/// The visible stage watch read, as an envelope keeps it.
pub fn shown(stage: VisibleStage) -> Shown {
    match stage {
        VisibleStage::NeedsInput => Shown::NeedsInput,
        _ => Shown::Working,
    }
}

/// The reviews of the pull request of `ticket`, read again for a child
/// that was not given watch's read: none, with the step stale, when the
/// ref is no longer at `commit`; refused when they cannot be read, a spent
/// budget pausing the forge in `following`.
fn read_reviews(
    on_demand: &OnDemand<'_>,
    ticket: &TicketId,
    commit: &str,
    following: &mut Following,
    sink: &mut EventSink<'_>,
) -> Result<ReviewsRead, Stepped> {
    let stored = ticket_ref::read(&on_demand.executor.git, &on_demand.dirs.checkout(), ticket);
    let number = match stored {
        Ok(Some(stored)) if stored.commit == commit => stored
            .record
            .delivery
            .and_then(|delivery| delivery.pull_request)
            .map(|pr| pr.number),
        Ok(_) => return Err(Stepped::Stale),
        Err(reason) => {
            return Err(Stepped::Ran(Err(Stop::Refused(format!(
                "reading {ticket}'s ticket ref: {reason}"
            )))));
        }
    };
    let Some(number) = number else {
        return Err(Stepped::Stale);
    };
    let (read, warnings) = following.reviews(on_demand.forge, ticket, number, (on_demand.clock)());
    for warning in warnings {
        sink.emit(ticket, None, EventKind::Warning, warning);
    }
    read.ok_or_else(|| {
        Stepped::Ran(Err(Stop::Refused(format!(
            "the reviews of pull request #{number} could not be read again"
        ))))
    })
}

/// Whether a step's outcome is reported, printed and notified as
/// `continue`'s: every one but a busy project at the drain's intake or
/// dispatch, which the drain never told.
pub fn reported(step: &StepKind, outcome: &Result<Delivered, Stop>) -> bool {
    !(matches!(step, StepKind::Intake | StepKind::Dispatch)
        && matches!(outcome, Err(stop) if stop.is_busy()))
}

/// How a step ended, as watch tells ends apart. Every way to stop is named,
/// so that a new one is placed here.
pub fn end(stepped: &Stepped) -> StepEnd {
    let stop = match stepped {
        Stepped::Stale => return StepEnd::Stale,
        Stepped::Capped {
            open_prs,
            max_open_prs,
        } => {
            return StepEnd::Capped {
                open_prs: *open_prs,
                max_open_prs: *max_open_prs,
            };
        }
        Stepped::NotAdmitted { account, reason } => {
            return StepEnd::NotAdmitted {
                account: account.clone(),
                reason: reason.clone(),
            };
        }
        Stepped::Ran(Ok(_)) => return StepEnd::Other,
        Stepped::Ran(Err(stop)) => stop,
    };
    match stop {
        Stop::Busy(_) | Stop::TicketBusy(_) => StepEnd::Busy,
        Stop::UsageLimit { resets_at } => StepEnd::UsageLimit {
            resets_at: *resets_at,
        },
        Stop::Stopped => StepEnd::Stopped,
        Stop::BaseHeld {
            commit,
            command,
            pending,
            due,
            ..
        } => StepEnd::BaseHeld {
            commit: commit.clone(),
            command: command.clone(),
            // Any check that waits for the other steps gets watch's
            // priority (OWL-431).
            pending: *pending || *due,
        },
        Stop::NotKept { landed, .. } => StepEnd::NotKept {
            landed: landed_kind(*landed),
        },
        Stop::Stepped { stage, .. } => StepEnd::Stepped { stage: *stage },
        Stop::Refused(_)
        | Stop::Waits { .. }
        | Stop::Unverified { .. }
        | Stop::NeedsInput { .. }
        | Stop::Waiting { .. }
        | Stop::Settling { .. }
        | Stop::Reasked { .. }
        | Stop::CounterQuestion { .. }
        | Stop::CheckFailed { .. }
        | Stop::Parked { .. }
        | Stop::Ready { .. }
        | Stop::Delivery(_)
        | Stop::Watched { .. }
        | Stop::ToWatch { .. } => StepEnd::Other,
    }
}

fn landed_kind(landed: Landed) -> LandedKind {
    match landed {
        Landed::Resume => LandedKind::Resume,
        Landed::Questions => LandedKind::Questions,
        Landed::PremiseFalse => LandedKind::PremiseFalse,
        Landed::Reask => LandedKind::Reask,
        Landed::Reply => LandedKind::Reply,
        Landed::Parked => LandedKind::Parked,
        Landed::Intake => LandedKind::Intake,
        Landed::ParkedUnposted => LandedKind::ParkedUnposted,
        Landed::Quarantined => LandedKind::Quarantined,
    }
}

/// Whether a step ran nothing: what it carried stands, and no hold is due.
fn ran_nothing(stepped: &Stepped) -> bool {
    match stepped {
        Stepped::Stale | Stepped::Capped { .. } | Stepped::NotAdmitted { .. } => true,
        Stepped::Ran(Err(stop)) => stop.is_busy(),
        Stepped::Ran(Ok(_)) => false,
    }
}

/// The ticket's `carry.json`.
pub fn carry_file(dirs: &ProjectDirs, ticket: &TicketId) -> PathBuf {
    dirs.ticket_dir(ticket).join(CARRY_FILE)
}

/// What the ticket's last step carries; `None` when nothing is kept, or
/// what is kept does not read, which starts afresh as a person's `continue`
/// does.
pub fn read_carry(dirs: &ProjectDirs, ticket: &TicketId) -> Option<TicketCarry> {
    let text = fs::read_to_string(carry_file(dirs, ticket)).ok()?;
    TicketCarry::parse(&text).ok()
}

/// The generation of what the ticket's last step carries when it is tied
/// to `commit`, the ref watch read; 0 otherwise.
pub fn carry_generation(dirs: &ProjectDirs, ticket: &TicketId, commit: &str) -> u64 {
    read_carry(dirs, ticket)
        .filter(|carry| carry.commit == commit)
        .map_or(0, |carry| carry.generation)
}

/// What a resume goes on from: what `carry.json` keeps, only while it is
/// tied to the envelope's commit and generation.
fn carried(dirs: &ProjectDirs, envelope: &StepEnvelope) -> Carried {
    read_carry(dirs, &envelope.ticket)
        .filter(|carry| {
            Some(carry.commit.as_str()) == envelope.commit.as_deref()
                && carry.generation == envelope.generation
        })
        .map(|carry| Carried {
            chain: carry.chain.chain(),
            integrated: carry.integrated,
        })
        .unwrap_or_default()
}

/// Removes what the ticket's steps carry, if anything.
pub fn remove_carry(dirs: &ProjectDirs, ticket: &TicketId) -> io::Result<()> {
    match fs::remove_file(carry_file(dirs, ticket)) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

/// Keeps what a step carries after it, once its lock is released: as it
/// was when the step ran nothing, or when it stopped [`Stop::Stepped`],
/// which kept it under its lock ([`OnDemand::stepped`]); removed otherwise,
/// which changes nothing to a person's command that took the lock since,
/// none of them writing it. A removal that fails is a `warning` event
/// (`carry_not_kept`): the carry left is tied to a ref the step moved, or
/// was the step's own to go on from.
fn keep_carry(
    on_demand: &OnDemand<'_>,
    envelope: &StepEnvelope,
    stepped: &Stepped,
    sink: &mut EventSink<'_>,
) {
    if !removes_carry(stepped) {
        return;
    }
    let ticket = &envelope.ticket;
    if let Err(error) = remove_carry(on_demand.dirs, ticket) {
        sink.emit(
            ticket,
            None,
            EventKind::Warning,
            data([
                ("what", json!("carry_not_kept")),
                ("reason", json!(error.to_string())),
            ]),
        );
    }
}

/// Whether `carry.json` goes after a step that ended `stepped`: not when
/// it ran nothing, nor when it stopped at its next stage, which kept it
/// under its lock.
fn removes_carry(stepped: &Stepped) -> bool {
    !ran_nothing(stepped) && !matches!(stepped, Stepped::Ran(Err(Stop::Stepped { .. })))
}

/// Ties `carried` to `commit`, the ticket ref a stage step left, its
/// generation one more than the last one's: done under the step's lock
/// ([`OnDemand::stepped`], OWL-399).
pub(crate) fn write_carry(
    dirs: &ProjectDirs,
    ticket: &TicketId,
    commit: &str,
    carried: Carried,
) -> io::Result<()> {
    let generation = read_carry(dirs, ticket).map_or(0, |carry| carry.generation);
    let carry = TicketCarry {
        format: Format,
        commit: commit.to_owned(),
        generation: generation.saturating_add(1),
        chain: KeptChain::from(carried.chain),
        integrated: carried.integrated,
    };
    write_atomically(&carry_file(dirs, ticket), &carry.render())
}

/// Writes `text` at `path` whole or not at all: to a file beside it, then
/// renamed over it, its folder made first.
pub(crate) fn write_atomically(path: &Path, text: &str) -> io::Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let mut partial = path.as_os_str().to_owned();
    partial.push(".partial");
    let partial = PathBuf::from(partial);
    fs::write(&partial, text)?;
    fs::rename(&partial, path)
}

/// The outcome record of `attempt`, in the ticket's folder.
pub fn outcome_file(dirs: &ProjectDirs, ticket: &TicketId, attempt: &str) -> PathBuf {
    dirs.ticket_dir(ticket)
        .join(format!("outcome-{attempt}.json"))
}

/// Writes `record` in its ticket's folder, whole or not at all.
pub fn write_outcome(dirs: &ProjectDirs, record: &StepOutcome) -> io::Result<()> {
    write_atomically(
        &outcome_file(dirs, &record.ticket, &record.attempt),
        &record.render(),
    )
}

/// Reads the outcome record at `path`, then removes it: `Ok(None)` when
/// there is none; why it does not read otherwise, removed all the same.
pub fn take_outcome(path: &Path) -> Result<Option<StepOutcome>, String> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            let _ = fs::remove_file(path);
            return Err(error.to_string());
        }
    };
    let _ = fs::remove_file(path);
    StepOutcome::parse(&text)
        .map(Some)
        .map_err(|error| error.to_string())
}

/// The work outside Owlshift handed to `attempt`, a dispatch's step, in
/// the ticket's folder (OWL-401).
pub fn outside_file(dirs: &ProjectDirs, ticket: &TicketId, attempt: &str) -> PathBuf {
    dirs.ticket_dir(ticket)
        .join(format!("outside-{attempt}.json"))
}

/// Hands `outside`, what a pass read of the work outside Owlshift, to the
/// dispatch step `envelope`, before its child starts: written whole or not
/// at all.
pub fn write_outside(
    dirs: &ProjectDirs,
    envelope: &StepEnvelope,
    outside: &Outside,
) -> io::Result<()> {
    write_atomically(
        &outside_file(dirs, &envelope.ticket, &envelope.attempt),
        &outside.to_step().render(),
    )
}

/// What a dispatch step `envelope` was handed of the work outside Owlshift,
/// read and removed; `None` for another step, or when none was handed or
/// it does not read, the dispatch then reading it afresh.
pub fn take_outside(dirs: &ProjectDirs, envelope: &StepEnvelope) -> Option<Outside> {
    if !matches!(envelope.step, StepKind::Dispatch) {
        return None;
    }
    let path = outside_file(dirs, &envelope.ticket, &envelope.attempt);
    let text = fs::read_to_string(&path).ok();
    let _ = fs::remove_file(&path);
    StepOutside::parse(&text?).ok().map(Outside::from_step)
}

/// Removes what was handed to the steps of the project's tickets and never
/// taken: by a watch that ended before its child read it.
pub fn remove_outside_leftovers(dirs: &ProjectDirs) {
    let Ok(tickets) = fs::read_dir(dirs.tickets()) else {
        return;
    };
    for file in tickets
        .flatten()
        .filter_map(|ticket| fs::read_dir(ticket.path()).ok())
        .flat_map(|files| files.flatten())
    {
        let name = file.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("outside-") && name.ends_with(".json") {
            let _ = fs::remove_file(file.path());
        }
    }
}

/// The outcome records left in the project's ticket folders, by a watch
/// that ended before it reaped its child.
pub fn leftovers(dirs: &ProjectDirs) -> Vec<PathBuf> {
    let Ok(tickets) = fs::read_dir(dirs.root().join("tickets")) else {
        return Vec::new();
    };
    let mut found: Vec<PathBuf> = tickets
        .flatten()
        .filter_map(|ticket| fs::read_dir(ticket.path()).ok())
        .flat_map(|files| files.flatten().map(|file| file.path()))
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("outcome-") && name.ends_with(".json"))
        })
        .collect();
    found.sort();
    found
}

/// What watch read, as the step's envelope says.
pub fn seen(envelope: &StepEnvelope) -> Seen {
    Seen {
        commit: envelope.commit.clone().unwrap_or_default(),
        progress: Progress(envelope.progress.clone()),
        answer: envelope.answer,
    }
}

/// Where the ticket ref of `ticket` is after a step.
pub fn ref_after(on_demand: &OnDemand<'_>, ticket: &TicketId) -> RefAfter {
    match ticket_ref::read(&on_demand.executor.git, &on_demand.dirs.checkout(), ticket) {
        Ok(Some(stored)) => RefAfter::Present {
            commit: stored.commit.clone(),
            progress: stored.progress().0,
        },
        Ok(None) => RefAfter::Absent,
        Err(_) => RefAfter::Unread,
    }
}

/// The progress watch compares after a step of `kind`, from where its ref
/// is: the drain's intake and dispatch read an absent ref as the one a
/// first run makes for its cost alone ([`RefAt::none`]), every other step as
/// none, as an unread one.
pub fn progress_after(kind: &StepKind, after: &RefAfter) -> Option<Progress> {
    match after {
        RefAfter::Present { progress, .. } => Some(Progress(progress.clone())),
        RefAfter::Absent if matches!(kind, StepKind::Intake | StepKind::Dispatch) => {
            Some(RefAt::none().progress)
        }
        RefAfter::Absent | RefAfter::Unread => None,
    }
}

/// `owlshift continue <TICKET> --step <envelope>`: the step, with watch's
/// memory of the ticket's pull request as the envelope gives it, its
/// outcome reported by `report` as `continue`'s, then its record written
/// last, which is returned.
pub fn child(
    on_demand: &OnDemand<'_>,
    envelope: &StepEnvelope,
    sink: &mut EventSink<'_>,
    report: &mut dyn FnMut(&Result<Delivered, Stop>, &mut EventSink<'_>),
) -> io::Result<StepOutcome> {
    let ticket = &envelope.ticket;
    let mut following = Following::default();
    following.keep(ticket, envelope.kept.clone());
    // A dispatch weighs the work outside Owlshift its pass read (OWL-401).
    let outside = take_outside(on_demand.dirs, envelope);
    let stepped = run(on_demand, envelope, &mut following, None, outside, sink);
    if let Stepped::Ran(outcome) = &stepped
        && reported(&envelope.step, outcome)
    {
        report(outcome, sink);
    }
    let after = ref_after(on_demand, ticket);
    let end = end(&stepped);
    let now = (on_demand.clock)();
    let hold = if ran_nothing(&stepped) {
        None
    } else {
        let progress = progress_after(&envelope.step, &after);
        watch::hold_for(ticket, seen(envelope), &end, progress.as_ref(), now)
    };
    let record = StepOutcome {
        format: Format,
        ticket: ticket.clone(),
        attempt: envelope.attempt.clone(),
        commit: envelope.commit.clone(),
        progress: envelope.progress.clone(),
        answer: envelope.answer,
        end,
        after,
        hold,
        kept: following.kept(ticket),
        at: now,
    };
    write_outcome(on_demand.dirs, &record)?;
    Ok(record)
}

/// Takes one of the machine's run slots for `envelope`'s step (OWL-409),
/// before anything else of the step: the slot, kept to the step's end and
/// named after the step's ticket in `project` ([`slot_holder`], OWL-417);
/// or, when every slot is taken, `None` once the step's "no slot" outcome
/// record is written ([`no_slot_record`]), its ticket untouched.
pub fn take_slot(
    slots: &MachineSlots,
    dirs: &ProjectDirs,
    project: &str,
    envelope: &StepEnvelope,
    now: Timestamp,
) -> io::Result<Option<Slot>> {
    match slots.take(&slot_holder(project, envelope, now))? {
        Some(slot) => Ok(Some(slot)),
        None => no_slot_record(dirs, envelope, now).map(|_| None),
    }
}

/// Who holds the slot `envelope`'s step takes in `project` at `now`: this
/// process, for the step's ticket.
pub fn slot_holder(project: &str, envelope: &StepEnvelope, now: Timestamp) -> Holder {
    Holder {
        project: project.to_owned(),
        ticket: envelope.ticket.clone(),
        pid: std::process::id(),
        since: now,
    }
}

/// Writes the outcome record of a step that found no free run slot: it
/// ran and read nothing, so it calls for no hold and keeps watch's memory
/// of the ticket's pull request as the envelope gave it.
pub fn no_slot_record(
    dirs: &ProjectDirs,
    envelope: &StepEnvelope,
    now: Timestamp,
) -> io::Result<StepOutcome> {
    let record = StepOutcome {
        format: Format,
        ticket: envelope.ticket.clone(),
        attempt: envelope.attempt.clone(),
        commit: envelope.commit.clone(),
        progress: envelope.progress.clone(),
        answer: envelope.answer,
        end: StepEnd::NoSlot,
        after: RefAfter::Unread,
        hold: None,
        kept: envelope.kept.clone(),
        at: now,
    };
    write_outcome(dirs, &record)?;
    Ok(record)
}

/// The hold a predecessor's record calls for, as watch keeps it.
pub fn hold_of(record: &StepOutcome) -> Option<(Seen, StepHold)> {
    let hold = record.hold.clone()?;
    let seen = Seen {
        commit: record.commit.clone().unwrap_or_default(),
        progress: Progress(record.progress.clone()),
        answer: record.answer,
    };
    Some((seen, hold))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use jiff::Timestamp;
    use owlshift_adapters::forge::Repo;
    use owlshift_contracts::step::KeptFollowing;
    use owlshift_core::gate::Chain;
    use owlshift_core::vocab::Stage;

    use super::*;

    fn ticket() -> TicketId {
        TicketId::new("OWL-1").unwrap()
    }

    fn envelope(step: StepKind) -> StepEnvelope {
        StepEnvelope {
            format: Format,
            ticket: ticket(),
            attempt: "1-2-3".to_owned(),
            step,
            commit: Some("c1".to_owned()),
            progress: "p1".to_owned(),
            answer: None,
            generation: 2,
            kept: KeptFollowing::default(),
        }
    }

    fn chain() -> Carried {
        let mut chain = Chain::default();
        chain.settled_once();
        Carried {
            chain,
            integrated: true,
        }
    }

    fn stepped() -> Stepped {
        Stepped::Ran(Err(Stop::Stepped {
            ticket: ticket(),
            stage: Stage::Build,
            carried: chain(),
        }))
    }

    fn at(minute: i64) -> Timestamp {
        Timestamp::from_second(1_790_000_000 + minute * 60).unwrap()
    }

    /// OWL-399: once a step's lock is released, what it carried stays when
    /// it ran nothing, or when it stopped at its next stage, which kept it
    /// under that lock; anything else starts the next step afresh. A write
    /// raises the generation.
    #[test]
    fn a_step_leaves_its_carry_only_when_it_ran_nothing_or_kept_its_stage() {
        for kept in [
            Stepped::Stale,
            Stepped::Capped {
                open_prs: 3,
                max_open_prs: 3,
            },
            Stepped::NotAdmitted {
                account: "stranger".to_owned(),
                reason: "not listed".to_owned(),
            },
            Stepped::Ran(Err(Stop::Busy(PathBuf::from("/p")))),
            stepped(),
        ] {
            assert!(!removes_carry(&kept), "{kept:?}");
        }
        for gone in [
            Stepped::Ran(Err(Stop::Refused("no".to_owned()))),
            Stepped::Ran(Err(Stop::Ready { ticket: ticket() })),
        ] {
            assert!(removes_carry(&gone), "{gone:?}");
        }

        let tmp = tempfile::tempdir().unwrap();
        let dirs = ProjectDirs::new(tmp.path(), &Repo::parse("acme/app").unwrap());
        write_carry(&dirs, &ticket(), "c1", chain()).unwrap();
        write_carry(&dirs, &ticket(), "c2", chain()).unwrap();
        let carry = read_carry(&dirs, &ticket()).unwrap();
        assert_eq!((carry.commit.as_str(), carry.generation), ("c2", 2));
        assert_eq!(carry.chain.chain(), chain().chain);
    }

    /// OWL-417: the slot a step takes is named after its project, its
    /// ticket and this process.
    #[test]
    fn a_step_s_slot_is_held_for_its_ticket_by_this_process() {
        let holder = slot_holder("acme/app", &envelope(StepKind::Dispatch), at(0));
        assert_eq!(
            (
                holder.project.as_str(),
                &holder.ticket,
                holder.pid,
                holder.since
            ),
            ("acme/app", &ticket(), std::process::id(), at(0))
        );
    }

    /// A resume goes on from `carry.json` only while its commit and
    /// generation are the envelope's.
    #[test]
    fn a_resume_uses_the_carry_of_its_commit_and_generation_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = ProjectDirs::new(tmp.path(), &Repo::parse("acme/app").unwrap());
        let resume = envelope(StepKind::Resume {
            shown: Shown::Working,
            stage: Stage::Build,
        });
        assert_eq!(carried(&dirs, &resume), Carried::default());
        let write = |commit: &str, generation| {
            let carry = TicketCarry {
                format: Format,
                commit: commit.to_owned(),
                generation,
                chain: KeptChain::from(chain().chain),
                integrated: true,
            };
            write_atomically(&carry_file(&dirs, &ticket()), &carry.render()).unwrap();
        };
        write("c1", 2);
        assert_eq!(carried(&dirs, &resume), chain());
        assert_eq!(carry_generation(&dirs, &ticket(), "c1"), 2);
        assert_eq!(carry_generation(&dirs, &ticket(), "c9"), 0);
        write("c1", 3);
        assert_eq!(carried(&dirs, &resume), Carried::default());
        write("c9", 2);
        assert_eq!(carried(&dirs, &resume), Carried::default());
        fs::write(carry_file(&dirs, &ticket()), "{").unwrap();
        assert_eq!(carried(&dirs, &resume), Carried::default());
        remove_carry(&dirs, &ticket()).unwrap();
        remove_carry(&dirs, &ticket()).unwrap();
        assert!(!carry_file(&dirs, &ticket()).exists());
    }

    /// Every outcome record a watch left is found, read once and removed,
    /// one that does not read included.
    #[test]
    fn records_left_are_found_read_once_and_removed() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = ProjectDirs::new(tmp.path(), &Repo::parse("acme/app").unwrap());
        assert!(leftovers(&dirs).is_empty());
        let record = StepOutcome {
            format: Format,
            ticket: ticket(),
            attempt: "1-2-3".to_owned(),
            commit: Some("c1".to_owned()),
            progress: "p1".to_owned(),
            answer: None,
            end: StepEnd::Other,
            after: RefAfter::Absent,
            hold: Some(StepHold::Held {
                until: Some(at(10)),
            }),
            kept: KeptFollowing::default(),
            at: at(0),
        };
        write_outcome(&dirs, &record).unwrap();
        let other = TicketId::new("OWL-2").unwrap();
        let broken = outcome_file(&dirs, &other, "4-5-6");
        write_atomically(&broken, "{").unwrap();
        write_atomically(&carry_file(&dirs, &other), "{}").unwrap();
        let found = leftovers(&dirs);
        assert_eq!(
            found,
            [outcome_file(&dirs, &ticket(), "1-2-3"), broken.clone()]
        );
        assert_eq!(take_outcome(&found[0]).unwrap(), Some(record.clone()));
        assert!(take_outcome(&broken).is_err());
        assert!(leftovers(&dirs).is_empty());
        assert_eq!(take_outcome(&found[0]).unwrap(), None);
        let (seen, hold) = hold_of(&record).unwrap();
        assert_eq!(
            (seen.commit.as_str(), seen.progress.0.as_str()),
            ("c1", "p1")
        );
        assert_eq!(
            hold,
            StepHold::Held {
                until: Some(at(10))
            }
        );
    }

    /// The ends watch tells apart, and the drain's busy intake and
    /// dispatch, which it never reported.
    #[test]
    fn a_step_ends_as_watch_reads_it() {
        let cases = [
            (Stepped::Stale, StepEnd::Stale),
            (
                Stepped::Ran(Err(Stop::Busy(PathBuf::from("/p")))),
                StepEnd::Busy,
            ),
            (
                Stepped::Ran(Err(Stop::UsageLimit {
                    resets_at: Some(at(5)),
                })),
                StepEnd::UsageLimit {
                    resets_at: Some(at(5)),
                },
            ),
            (
                Stepped::Ran(Err(Stop::NotKept {
                    landed: Landed::Resume,
                    message: "locked".to_owned(),
                })),
                StepEnd::NotKept {
                    landed: LandedKind::Resume,
                },
            ),
            (
                stepped(),
                StepEnd::Stepped {
                    stage: Stage::Build,
                },
            ),
            (
                Stepped::Ran(Err(Stop::Ready { ticket: ticket() })),
                StepEnd::Other,
            ),
        ];
        for (stepped, ended) in cases {
            assert_eq!(end(&stepped), ended, "{stepped:?}");
        }
        let busy = Err(Stop::Busy(PathBuf::from("/p")));
        assert!(!reported(&StepKind::Intake, &busy));
        assert!(!reported(&StepKind::Dispatch, &busy));
        assert!(reported(&StepKind::Correct, &busy));
        let ready = Err(Stop::Ready { ticket: ticket() });
        assert!(reported(&StepKind::Intake, &ready));
    }

    /// The hold a record keeps for a watch that starts is watch's own: a
    /// child that left nothing behind reads as another end, held ten
    /// minutes on a ref as it was; a ref read moved holds nothing; a state
    /// not kept holds until a person acts; a usage limit pauses; a busy
    /// project holds nothing.
    #[test]
    fn a_record_keeps_the_hold_watch_would_set() {
        let seen = Seen {
            commit: "c1".to_owned(),
            progress: Progress("p1".to_owned()),
            answer: None,
        };
        let (p1, p2) = (Progress("p1".to_owned()), Progress("p2".to_owned()));
        let hold = |end: StepEnd, progress: Option<&Progress>| {
            watch::hold_for(&ticket(), seen.clone(), &end, progress, at(0))
        };
        assert_eq!(
            hold(StepEnd::Other, Some(&p1)),
            Some(StepHold::Held {
                until: Some(at(10))
            })
        );
        assert_eq!(hold(StepEnd::Other, Some(&p2)), None);
        let not_kept = StepEnd::NotKept {
            landed: LandedKind::Reask,
        };
        assert_eq!(hold(not_kept, None), Some(StepHold::Held { until: None }));
        let limited = StepEnd::UsageLimit {
            resets_at: Some(at(30)),
        };
        // OWL-404: its own ticket, until the reset, whatever the ref did.
        assert_eq!(
            hold(limited.clone(), Some(&p1)),
            Some(StepHold::Held {
                until: Some(at(30))
            })
        );
        assert_eq!(
            hold(limited, Some(&p2)),
            Some(StepHold::Held {
                until: Some(at(30))
            })
        );
        assert_eq!(hold(StepEnd::Busy, Some(&p1)), None);
    }

    /// The drain's intake and dispatch read an absent ref as one a first
    /// run makes for its cost alone; every other step as none.
    #[test]
    fn an_absent_ref_reads_as_the_drain_reads_it() {
        let absent = RefAfter::Absent;
        for drained in [StepKind::Intake, StepKind::Dispatch] {
            assert_eq!(
                progress_after(&drained, &absent),
                Some(RefAt::none().progress)
            );
        }
        assert_eq!(progress_after(&StepKind::Review, &absent), None);
        assert_eq!(progress_after(&StepKind::Intake, &RefAfter::Unread), None);
        let present = RefAfter::Present {
            commit: "c".to_owned(),
            progress: "p".to_owned(),
        };
        assert_eq!(
            progress_after(&StepKind::Correct, &present),
            Some(Progress("p".to_owned()))
        );
    }
}
