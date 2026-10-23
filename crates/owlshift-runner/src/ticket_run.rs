//! Isolation among steps (OWL-400; architecture section 8, "Isolation among
//! steps"): what keeps one run's isolation check from mistaking a sibling
//! ticket's legitimate writes for a breach, or missing a real one, once
//! several tickets' steps run at once.
//!
//! - **Obligations.** A run writes its own obligation, a [`RunObligation`]
//!   in `unverified/<ticket>-<run>` of the project's folder, before it
//!   starts, linked in whole so no reader finds it half written (OWL-425),
//!   and removes only that one once its check passed. One whose
//!   ticket lock is free belongs to a run that ended without its check, a
//!   crash or a stop: the first command or watch pass that finds it writes
//!   the project's quarantine, naming it ([`look`]).
//! - **The run file.** Each ticket's `run.json`, a [`TicketRun`], written only
//!   under its ticket lock, replaced whole (written, synced, renamed): the
//!   tip of its branch its last check verified, the value of its ticket ref
//!   the runner last wrote, the worktree entry its worktree owns, and a
//!   generation raised at every write. A write of the ref ([`write_ref`],
//!   [`forget_ref`]) or a move of the branch outside a run ([`move_branch`])
//!   records its target before it and its result after, so a write cut off
//!   between the two is finished by the next holder of the lock, whichever
//!   value the ref or the branch then shows; any other value is a breach.
//!   Production code writes a ticket ref only here.
//! - **The start of a step.** [`open`] checks, under the ticket's lock and
//!   before its first run, that the branch, the ticket ref and the kept
//!   worktree's entry are what the run file records, and gives a ticket from
//!   before P6 its run file from its refs as they are.
//! - **The check among steps.** [`Siblings`] gives a run's check the other
//!   tickets in flight or ended since its snapshot, from their run files and
//!   their locks ([`Concurrent`]).
//!
//! Since OWL-401 the project's lock is held for short sections only, so
//! several tickets' runs overlap and the check among steps is what keeps
//! each from taking a sibling's writes for a breach.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use jiff::Timestamp;

use owlshift_contracts::format::Format;
use owlshift_contracts::ids::TicketId;
use owlshift_contracts::refs::ticket_ref;
use owlshift_contracts::step::{RunObligation, RunTarget, TicketRun, is_run_id};
use owlshift_platform::files;

use crate::executor::Git;
use crate::executor::isolation::Concurrent;
use crate::on_demand::branch_for;
use crate::project::{ProjectDirs, TicketLock, component};
use crate::ticket_ref::{self, Forgot, ProposalsFile, RefGuard, TicketRecord};

/// The most bytes an obligation or a run file is read up to.
const RECORD_CAP: u64 = 64 * 1024;

/// The file name of the obligation of `run` of `ticket`: the ticket's folder
/// name and the run's id, which is unique within its ticket only; `gate-`
/// and the run's id for `owlshift gate`, which runs for no ticket.
pub fn obligation_name(ticket: Option<&TicketId>, run: &str) -> String {
    match ticket {
        Some(ticket) => format!("{}-{run}", component(ticket.as_str())),
        None => format!("gate-{run}"),
    }
}

/// Writes the obligation of `run` of `ticket` (none for `owlshift gate`),
/// described by `what`, before the run starts; returns its name, which
/// [`remove_obligation`] takes. Never replaces one: a name taken is an
/// error.
///
/// A step writes its obligation without the project's lock, while a watch
/// pass holding it may list the folder at any moment, and an obligation
/// that does not read quarantines the project (OWL-425). So it is written
/// whole first, synced, as `unverified-<name>.partial` in the project's
/// folder, which nothing lists, then linked into `unverified/`: a hard link
/// fails on a name taken, where a rename would replace it. A partial left
/// by a crash before its removal takes its name as a written obligation
/// does; a run's id never comes again, and nothing else reads it. The
/// project's folder needs hard links, which every file system the data
/// directory is expected on has.
pub fn write_obligation(
    dirs: &ProjectDirs,
    ticket: Option<&TicketId>,
    run: &str,
    what: &str,
) -> io::Result<String> {
    if !is_run_id(run) {
        return Err(io::Error::other(format!("{run:?} is not a run id")));
    }
    let record = RunObligation {
        format: Format,
        ticket: ticket.cloned(),
        run: run.to_owned(),
        started: Timestamp::now(),
        what: one_line(what),
    };
    let folder = dirs.obligations_dir();
    fs::create_dir_all(&folder)?;
    let name = obligation_name(ticket, run);
    let partial = dirs.root().join(format!("unverified-{name}.partial"));
    // Never opened over an existing file: one there may be linked in already.
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&partial)?;
    let linked = file
        .write_all(record.render().as_bytes())
        .and_then(|()| file.sync_all())
        .and_then(|()| fs::hard_link(&partial, folder.join(&name)));
    drop(file);
    let _ = fs::remove_file(&partial);
    linked.map(|()| name)
}

/// `what`, cut to one line of at most the bytes an obligation keeps.
fn one_line(what: &str) -> String {
    let mut line: String = what.chars().filter(|c| !c.is_control()).collect();
    let max = owlshift_contracts::step::MAX_OBLIGATION_WHAT;
    if line.len() > max {
        let mut end = max;
        while !line.is_char_boundary(end) {
            end -= 1;
        }
        line.truncate(end);
    }
    line
}

/// Removes the obligation `name`, if it is there.
pub fn remove_obligation(dirs: &ProjectDirs, name: &str) -> io::Result<()> {
    match fs::remove_file(dirs.obligations_dir().join(name)) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

/// An obligation found in the project's folder: its name, and its record or
/// why it cannot be read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    pub name: String,
    pub record: Result<RunObligation, String>,
}

/// Every obligation of the project, by name; none when the folder is not
/// there, or is the marker of an Owlshift before P6, which
/// [`ProjectDirs::quarantine`] reads; one removed between the listing and
/// its read is not there either, nor one Windows refused while it was being
/// deleted. A name that is not UTF-8, an entry that is not a plain file, a
/// record past 64 KiB or one that does not parse is kept with why: it
/// cannot be read, and a lost record never widens what a check leaves out.
pub fn obligations(dirs: &ProjectDirs) -> io::Result<Vec<Found>> {
    let folder = dirs.obligations_dir();
    match fs::symlink_metadata(&folder) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => return Ok(Vec::new()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    }
    let mut found = Vec::new();
    for entry in fs::read_dir(&folder)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        // One removed since the listing, its run's check passed, is gone
        // (OWL-425), not unreadable; so is one Windows refused while its run
        // was deleting it (OWL-426): when asked, it is still being deleted,
        // or that has ended and it is gone.
        let record = match read_capped(&path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error)
                if error.kind() == io::ErrorKind::PermissionDenied
                    && (files::delete_pending(&folder, &entry.file_name()) || gone(&path)) =>
            {
                continue;
            }
            read => read.map_err(|e| e.to_string()),
        }
        .and_then(|text| RunObligation::parse(&text).map_err(|e| e.to_string()));
        found.push(Found { name, record });
    }
    found.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(found)
}

/// Whether nothing is at `path` any more.
fn gone(path: &Path) -> bool {
    fs::symlink_metadata(path).is_err_and(|error| error.kind() == io::ErrorKind::NotFound)
}

/// A plain file of at most [`RECORD_CAP`] bytes of UTF-8, read without
/// following a link.
fn read_capped(path: &Path) -> io::Result<String> {
    let meta = fs::symlink_metadata(path)?;
    if !meta.file_type().is_file() {
        return Err(io::Error::other("it is not a plain file"));
    }
    let mut bytes = Vec::new();
    File::open(path)?
        .take(RECORD_CAP + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > RECORD_CAP {
        return Err(io::Error::other(format!("it is past {RECORD_CAP} bytes")));
    }
    String::from_utf8(bytes).map_err(|_| io::Error::other("it is not UTF-8 text"))
}

/// Whether an obligation stands for a run still in flight: its ticket's
/// lock held elsewhere, that ticket not `own`, the command's own (whose
/// lock it holds itself, so an obligation of its ticket is one a run of it
/// left behind). One that names no ticket, `owlshift gate`'s, is never in
/// flight while the caller holds the project's lock, which `gate` holds for
/// its whole length.
fn in_flight(dirs: &ProjectDirs, found: &Found, own: Option<&TicketId>) -> io::Result<bool> {
    let Ok(record) = &found.record else {
        return Ok(false);
    };
    let Some(ticket) = &record.ticket else {
        return Ok(false);
    };
    if own.is_some_and(|own| dirs.ticket_dir(own) == dirs.ticket_dir(ticket)) {
        return Ok(false);
    }
    dirs.ticket_lock_held(ticket)
}

/// The folder of a ticket whose lock is held elsewhere now, the first in
/// the folders' order: a command or a step works it (OWL-401). What
/// `owlshift gate`, which holds the project's lock for its whole length and
/// keeps the check of a run alone, answers busy to, so that its run and a
/// step's never overlap: every step holds its ticket's lock for its whole
/// length and starts with one of the project's sections. One try each, let
/// go at once; a folder with no lock file was never locked.
pub fn ticket_in_flight(dirs: &ProjectDirs) -> io::Result<Option<PathBuf>> {
    ticket_in_flight_except(dirs, None)
}

/// [`ticket_in_flight`], `except`'s folder left out: the ticket whose step
/// runs a base check holds its own lock (OWL-418).
pub fn ticket_in_flight_except(
    dirs: &ProjectDirs,
    except: Option<&TicketId>,
) -> io::Result<Option<PathBuf>> {
    let own = except.map(|ticket| dirs.ticket_dir(ticket));
    let listing = match fs::read_dir(dirs.tickets()) {
        Ok(listing) => listing,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let mut folders = Vec::new();
    for entry in listing {
        let entry = entry?;
        if entry.file_type()?.is_dir() && entry.path().join("lock").exists() {
            folders.push(entry.path());
        }
    }
    folders.sort();
    for folder in folders {
        if own.as_ref() == Some(&folder) {
            continue;
        }
        if dirs.ticket_lock_held_in(&folder)? {
            return Ok(Some(folder));
        }
    }
    Ok(None)
}

/// The line a quarantine, and `owlshift unquarantine`, say of an obligation
/// left behind.
pub fn describe(dirs: &ProjectDirs, found: &Found) -> String {
    match &found.record {
        Ok(record) => format!(
            "{}, started at {}, ended before its isolation check passed (a crash or a stop): \
             nobody checked what it changed.\n",
            record.what, record.started
        ),
        Err(why) => format!(
            "The obligation {} cannot be read ({why}): a run's isolation check may not have \
             passed.\n",
            dirs.obligations_dir().join(&found.name).display()
        ),
    }
}

/// Where the project stands for a command or a watch pass, once it holds
/// the project's lock ([`look`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Look {
    /// The quarantine stands, in the file at `path`, saying `text`.
    Quarantined { path: PathBuf, text: String },
    /// No quarantine; `running` when an obligation of a run in flight
    /// stands.
    Clear { running: bool },
}

/// Where the project stands, read by a holder of the project's lock working
/// `own` (none for watch and `owlshift gate`): the quarantine when it
/// stands; otherwise, any obligation left behind (unreadable, `gate`'s,
/// `own`'s, or one whose ticket's lock is free) writes the quarantine,
/// naming each, which then stands; otherwise clear.
pub fn look(dirs: &ProjectDirs, own: Option<&TicketId>) -> io::Result<Look> {
    if let Some((path, text)) = dirs.quarantine_at()? {
        return Ok(Look::Quarantined { path, text });
    }
    let mut left = String::new();
    let mut running = false;
    for found in obligations(dirs)? {
        if in_flight(dirs, &found, own)? {
            running = true;
        } else {
            left.push_str(&describe(dirs, &found));
        }
    }
    if left.is_empty() {
        return Ok(Look::Clear { running });
    }
    let text = dirs.write_quarantine(&left)?;
    Ok(Look::Quarantined {
        path: dirs.quarantine_file(),
        text,
    })
}

/// An obligation as `owlshift doctor` and `owlshift unquarantine` show it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Standing {
    pub found: Found,
    /// Whether its ticket's lock is held: a run still in flight.
    pub running: bool,
}

/// Every obligation, each told running or left behind, looked at without
/// holding any lock: a ticket's lock, or for `owlshift gate`'s the
/// project's, is tried once and let go. A lock just released can read as
/// held a moment longer.
pub fn standing(dirs: &ProjectDirs) -> io::Result<Vec<Standing>> {
    obligations(dirs)?
        .into_iter()
        .map(|found| {
            let gate = found.record.as_ref().is_ok_and(|r| r.ticket.is_none());
            let running = if gate {
                dirs.try_lock()?.is_none()
            } else {
                in_flight(dirs, &found, None)?
            };
            Ok(Standing { found, running })
        })
        .collect()
}

/// Why the run file of a ticket stopped a command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunFault {
    /// What the ticket's branch, ref or entry hold is not what its run file
    /// records, or the run file cannot be read: a breach, which quarantines
    /// the project.
    Breach(String),
    /// A ticket from before P6 has no run file yet, and a run of another
    /// ticket is in flight: its baseline waits for none to stand.
    Busy,
    /// A git command or a write failed: nothing is known to be wrong.
    Failed(String),
}

impl std::fmt::Display for RunFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Breach(why) | Self::Failed(why) => f.write_str(why),
            Self::Busy => f.write_str(
                "the ticket's run file is made once no run of another ticket is in flight",
            ),
        }
    }
}

/// The run file of `ticket`, `None` when it has none; one that cannot be
/// read or does not parse is an error.
fn read_run(dirs: &ProjectDirs, ticket: &TicketId) -> Result<Option<TicketRun>, String> {
    read_run_at(&dirs.run_file(ticket))
}

fn read_run_at(path: &Path) -> Result<Option<TicketRun>, String> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        _ => {}
    }
    let text = read_capped(path).map_err(|e| format!("{}: {e}", path.display()))?;
    TicketRun::parse(&text)
        .map(Some)
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// Writes `run` as the run file of its ticket, its generation raised: whole,
/// through a partial file synced then renamed over it, and the folder
/// synced on Unix. The caller holds the ticket's lock.
fn save(dirs: &ProjectDirs, run: &mut TicketRun) -> io::Result<()> {
    run.generation += 1;
    let path = dirs.run_file(&run.ticket);
    let folder = dirs.ticket_dir(&run.ticket);
    fs::create_dir_all(&folder)?;
    let partial = folder.join("run.json.partial");
    let mut file = File::create(&partial)?;
    file.write_all(run.render().as_bytes())?;
    file.sync_all()?;
    drop(file);
    rename_over(&partial, &path)?;
    #[cfg(unix)]
    File::open(&folder)?.sync_all()?;
    Ok(())
}

/// Renames `from` over `to`. On Windows a rename over a file another
/// process or thread has open for a moment, a sibling's check reading it, is
/// refused: it is tried again for up to half a second.
fn rename_over(from: &Path, to: &Path) -> io::Result<()> {
    let mut tries = 0;
    loop {
        match fs::rename(from, to) {
            Err(error)
                if cfg!(windows)
                    && error.kind() == io::ErrorKind::PermissionDenied
                    && tries < 50 =>
            {
                tries += 1;
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            done => return done,
        }
    }
}

/// What a ticket's branch and ref hold in the dedicated checkout.
struct Actual {
    tip: Option<String>,
    /// The ref the branch points to when it is a symbolic ref: a run's
    /// commits through it would move that other ref, so it is never a value
    /// a run file records, and always a breach.
    tip_alias: Option<String>,
    ticket_ref: Option<String>,
}

/// The ref `refs/heads/<branch>` points to in `checkout` when it is a
/// symbolic ref, one level only; `None` for a plain branch or none.
fn branch_alias(git: &Git, checkout: &Path, branch: &str) -> Result<Option<String>, String> {
    let name = format!("refs/heads/{branch}");
    let out = git
        .output(
            checkout,
            &["symbolic-ref", "--quiet", "--no-recurse", &name],
            None,
        )
        .map_err(|e| e.to_string())?;
    match out.code {
        Some(0) => Ok(Some(String::from_utf8_lossy(&out.stdout).trim().to_owned())),
        Some(1) => Ok(None),
        _ => Err(format!(
            "reading the branch {branch}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )),
    }
}

/// The commit `refs/heads/<branch>` is at in `checkout`, `None` when there is
/// no such branch.
fn branch_head(git: &Git, checkout: &Path, branch: &str) -> Result<Option<String>, String> {
    let name = format!("refs/heads/{branch}^{{commit}}");
    let out = git
        .output(
            checkout,
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                "--end-of-options",
                &name,
            ],
            None,
        )
        .map_err(|e| e.to_string())?;
    match out.code {
        Some(0) => Ok(Some(String::from_utf8_lossy(&out.stdout).trim().to_owned())),
        Some(1) => Ok(None),
        _ => Err(format!(
            "reading the branch {branch}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )),
    }
}

fn actual(git: &Git, checkout: &Path, ticket: &TicketId) -> Result<Actual, String> {
    let branch = branch_for(ticket);
    Ok(Actual {
        tip: branch_head(git, checkout, &branch)?,
        tip_alias: branch_alias(git, checkout, &branch)?,
        ticket_ref: ticket_ref::pointed(git, checkout, ticket)?,
    })
}

/// The breach of a ticket's branch that is a symbolic ref, whatever commit
/// it resolves to (OWL-400, Codex P1 on #377): the ticket's worktree commits
/// through it into the ref it names. A symbolic ticket ref is not this
/// breach: `ticket_ref::read` refuses it as unreadable before anything runs,
/// and the runner's moves of it never follow it.
fn alias_breach(ticket: &TicketId, actual: &Actual) -> Result<(), RunFault> {
    match &actual.tip_alias {
        Some(target) => Err(RunFault::Breach(format!(
            "the branch {} of {ticket} is a symbolic ref to {target}: it was changed outside its \
             runs",
            branch_for(ticket)
        ))),
        None => Ok(()),
    }
}

/// A commit id as messages show it.
fn short(value: Option<&str>) -> String {
    match value {
        Some(id) => id.get(..12).unwrap_or(id).to_owned(),
        None => "none".to_owned(),
    }
}

/// Settles one value of a run file against what git shows (`actual`): a
/// write under way to `target` is finished at whichever of the recorded
/// value and the target is shown; with none, the value must be the one
/// recorded. Returns whether the run file changed, or the breach.
fn settle_one(
    what: &str,
    recorded: &mut Option<String>,
    target: &mut Option<RunTarget>,
    actual: &Option<String>,
) -> Result<bool, String> {
    match target {
        Some(to) if actual == recorded || *actual == to.to => {
            *recorded = actual.clone();
            *target = None;
            Ok(true)
        }
        Some(to) => Err(format!(
            "{what} is at {}, where its run file has {}, or {} for a write under way",
            short(actual.as_deref()),
            short(recorded.as_deref()),
            short(to.to.as_deref())
        )),
        None if actual == recorded => Ok(false),
        None => Err(format!(
            "{what} is at {}, where its run file has {}",
            short(actual.as_deref()),
            short(recorded.as_deref())
        )),
    }
}

/// Settles both values of `run` against `actual`; the breach names the
/// ticket.
fn settle(run: &mut TicketRun, actual: &Actual) -> Result<bool, RunFault> {
    let ticket = run.ticket.clone();
    alias_breach(&ticket, actual)?;
    let breach = |why: String| RunFault::Breach(format!("{why}: it was changed outside its runs"));
    let tip = settle_one(
        &format!("the branch {} of {ticket}", branch_for(&ticket)),
        &mut run.tip,
        &mut run.writing.tip,
        &actual.tip,
    )
    .map_err(breach)?;
    let reference = settle_one(
        &format!("the ticket ref of {ticket}"),
        &mut run.ticket_ref,
        &mut run.writing.ticket_ref,
        &actual.ticket_ref,
    )
    .map_err(breach)?;
    Ok(tip || reference)
}

/// The run file of `ticket` from its refs as they are: what a ticket from
/// before P6 gets, and what `owlshift unquarantine` takes afresh. A branch
/// that is a symbolic ref is recorded as none, so the ticket's next start
/// still finds it a breach until a person removes the alias.
fn baseline(ticket: &TicketId, actual: Actual, entry: Option<String>) -> TicketRun {
    TicketRun {
        tip: actual.tip.filter(|_| actual.tip_alias.is_none()),
        ticket_ref: actual.ticket_ref,
        entry,
        ..TicketRun::new(ticket.clone())
    }
}

/// The run file of `ticket`, settled against its refs as they are: read, a
/// write under way finished, and kept when that changed it; or, for a ticket
/// with none, its baseline, while no obligation stands. A value that is
/// neither, or a file that cannot be read, is [`RunFault::Breach`].
fn load(
    git: &Git,
    dirs: &ProjectDirs,
    checkout: &Path,
    ticket: &TicketId,
    entry: Option<&str>,
) -> Result<TicketRun, RunFault> {
    let actual = actual(git, checkout, ticket).map_err(RunFault::Failed)?;
    let read = read_run(dirs, ticket).map_err(|why| {
        RunFault::Breach(format!("the run file of {ticket} cannot be read: {why}"))
    })?;
    // A ticket of which nothing exists yet, no ref, no branch, no kept
    // worktree, gets its empty baseline whatever runs (OWL-401): what a run
    // in flight would make of it is found at its next start. Only a ticket
    // from before P6 waits for no obligation to stand.
    let nothing = actual.tip.is_none()
        && actual.tip_alias.is_none()
        && actual.ticket_ref.is_none()
        && entry.is_none();
    let mut run = match read {
        Some(run) => run,
        None if !nothing && dirs.obligations_stand() => return Err(RunFault::Busy),
        None => {
            alias_breach(ticket, &actual)?;
            let mut run = baseline(ticket, actual, entry.map(str::to_owned));
            save(dirs, &mut run).map_err(|e| RunFault::Failed(e.to_string()))?;
            return Ok(run);
        }
    };
    if dirs.ticket_dir(&run.ticket) != dirs.ticket_dir(ticket) {
        return Err(RunFault::Breach(format!(
            "the run file of {ticket} names {}",
            run.ticket
        )));
    }
    // One taken afresh from its folder's name alone may write the id in
    // another case ([`rebaseline`]): the command's own is kept.
    let renamed = run.ticket != *ticket;
    run.ticket = ticket.clone();
    if settle(&mut run, &actual)? || renamed {
        save(dirs, &mut run).map_err(|e| RunFault::Failed(e.to_string()))?;
    }
    Ok(run)
}

/// The start of a step of `ticket` (architecture section 8, "Starting a
/// step"), under its lock, before its first run: its branch and its ticket
/// ref must be what its run file records, a write under way counting, and
/// its kept worktree's entry, `kept_entry` when it has one, the one the run
/// file names. A ticket from before P6 gets its run file now
/// ([`RunFault::Busy`] while an obligation stands). Returns the run file.
pub fn open(
    git: &Git,
    dirs: &ProjectDirs,
    checkout: &Path,
    _lock: &TicketLock,
    ticket: &TicketId,
    kept_entry: Option<&str>,
) -> Result<TicketRun, RunFault> {
    let mut run = load(git, dirs, checkout, ticket, kept_entry)?;
    match (&run.entry, kept_entry) {
        (Some(recorded), Some(kept)) if recorded != kept => {
            return Err(RunFault::Breach(format!(
                "the worktree of {ticket} is the entry {kept}, where its run file names {recorded}"
            )));
        }
        (None, Some(kept)) => {
            run.entry = Some(kept.to_owned());
            save(dirs, &mut run).map_err(|e| RunFault::Failed(e.to_string()))?;
        }
        _ => {}
    }
    Ok(run)
}

/// The guard of a write of a ticket's ref through its run file
/// ([`write_ref`], [`forget_ref`]).
struct Guarded<'a> {
    git: &'a Git,
    dirs: &'a ProjectDirs,
    checkout: &'a Path,
    ticket: &'a TicketId,
    run: Option<TicketRun>,
    fault: Option<RunFault>,
}

impl Guarded<'_> {
    fn fail(&mut self, fault: RunFault) -> String {
        let message = fault.to_string();
        self.fault = Some(fault);
        message
    }
}

impl RefGuard for Guarded<'_> {
    fn before(&mut self, from: Option<&str>, to: Option<&str>) -> Result<(), String> {
        let mut run = match load(self.git, self.dirs, self.checkout, self.ticket, None) {
            Ok(run) => run,
            Err(fault) => return Err(self.fail(fault)),
        };
        if run.ticket_ref.as_deref() != from {
            let why = format!(
                "the ticket ref of {} was read at {}, where its run file has {}",
                self.ticket,
                short(from),
                short(run.ticket_ref.as_deref())
            );
            return Err(self.fail(RunFault::Failed(why)));
        }
        run.writing.ticket_ref = Some(RunTarget {
            to: to.map(str::to_owned),
        });
        if let Err(error) = save(self.dirs, &mut run) {
            return Err(self.fail(RunFault::Failed(format!("its run file: {error}"))));
        }
        self.run = Some(run);
        Ok(())
    }

    fn after(&mut self, to: Option<&str>, moved: bool) -> Result<(), String> {
        let Some(mut run) = self.run.take() else {
            return Ok(());
        };
        if moved {
            run.ticket_ref = to.map(str::to_owned);
            run.writing.ticket_ref = None;
        } else {
            let actual = actual(self.git, self.checkout, self.ticket)
                .map_err(|e| self.fail(RunFault::Failed(e)))?;
            settle(&mut run, &actual).map_err(|fault| self.fail(fault))?;
        }
        save(self.dirs, &mut run)
            .map_err(|e| self.fail(RunFault::Failed(format!("its run file was not kept: {e}"))))
    }
}

/// [`ticket_ref::write_proposals`] through the run file of `ticket`, whose
/// lock the caller holds: the ref must be what the run file records, the
/// target is kept before the ref moves and the result after. The error is a
/// [`RunFault::Breach`] when the ref, the branch or the run file is not what
/// it should be.
#[allow(clippy::too_many_arguments)]
pub fn write_ref(
    git: &Git,
    dirs: &ProjectDirs,
    checkout: &Path,
    _lock: &TicketLock,
    ticket: &TicketId,
    record: &TicketRecord,
    proposals: ProposalsFile<'_>,
    previous: Option<&str>,
) -> Result<String, RunFault> {
    let mut guard = Guarded {
        git,
        dirs,
        checkout,
        ticket,
        run: None,
        fault: None,
    };
    ticket_ref::write_proposals_guarded(
        git, checkout, ticket, record, proposals, previous, &mut guard,
    )
    .map_err(|message| guard.fault.take().unwrap_or(RunFault::Failed(message)))
}

/// [`ticket_ref::forget`] through the run file of `ticket`, as
/// [`write_ref`].
pub fn forget_ref(
    git: &Git,
    dirs: &ProjectDirs,
    checkout: &Path,
    _lock: &TicketLock,
    ticket: &TicketId,
) -> Result<Option<Forgot>, RunFault> {
    let mut guard = Guarded {
        git,
        dirs,
        checkout,
        ticket,
        run: None,
        fault: None,
    };
    ticket_ref::forget_guarded(git, checkout, ticket, &mut guard)
        .map_err(|message| guard.fault.take().unwrap_or(RunFault::Failed(message)))
}

/// A move of `ticket`'s branch to `to` the runner makes outside a run,
/// `moving`, under the ticket's lock: the target kept before, the branch's
/// head after, which must then be either its recorded tip or the target.
/// Returns what `moving` returned.
pub fn move_branch<T>(
    git: &Git,
    dirs: &ProjectDirs,
    checkout: &Path,
    _lock: &TicketLock,
    ticket: &TicketId,
    to: Option<&str>,
    moving: impl FnOnce() -> T,
) -> Result<T, RunFault> {
    let mut run = load(git, dirs, checkout, ticket, None)?;
    run.writing.tip = Some(RunTarget {
        to: to.map(str::to_owned),
    });
    save(dirs, &mut run).map_err(|e| RunFault::Failed(format!("its run file: {e}")))?;
    let moved = moving();
    let actual = actual(git, checkout, ticket).map_err(RunFault::Failed)?;
    settle(&mut run, &actual)?;
    save(dirs, &mut run).map_err(|e| RunFault::Failed(format!("its run file: {e}")))?;
    Ok(moved)
}

/// A run of `ticket` starts: its run file's generation raised, so a
/// sibling's check that took its snapshot before reads it as ended since.
pub fn run_started(
    git: &Git,
    dirs: &ProjectDirs,
    checkout: &Path,
    _lock: &TicketLock,
    ticket: &TicketId,
) -> Result<(), RunFault> {
    let mut run = load(git, dirs, checkout, ticket, None)?;
    save(dirs, &mut run).map_err(|e| RunFault::Failed(format!("its run file: {e}")))
}

/// A run of `ticket` ended, its check made, or never started: the run file
/// records its branch's head, which the check verified, and the entry of its
/// worktree, `worktree`, once it exists. Nothing of the run is running any
/// more, and the ticket ref is left as the run file has it: a run that moved
/// it is found by the next write. Recorded whatever the check found, so the
/// writes that keep a breaching run's cost and park go on as before; the
/// quarantine that breach wrote stands, and `owlshift unquarantine` takes
/// every run file afresh.
pub fn run_ended(
    git: &Git,
    dirs: &ProjectDirs,
    checkout: &Path,
    _lock: &TicketLock,
    ticket: &TicketId,
    worktree: &Path,
) -> Result<(), String> {
    let mut run = read_run(dirs, ticket)?.unwrap_or_else(|| TicketRun::new(ticket.clone()));
    // A run that made its branch a symbolic ref broke its own check (its
    // worktree is no longer on its branch): none is recorded, so the next
    // start finds the alias a breach too.
    let branch = branch_for(ticket);
    run.tip = match branch_alias(git, checkout, &branch)? {
        Some(_) => None,
        None => branch_head(git, checkout, &branch)?,
    };
    run.writing.tip = None;
    if fs::symlink_metadata(worktree).is_ok() {
        run.entry = entry_of(checkout, worktree);
    }
    save(dirs, &mut run).map_err(|e| format!("its run file: {e}"))
}

/// The name of `worktree`'s entry in the common git folder of `checkout`,
/// as its `.git` names it; `None` when it cannot be read.
pub fn entry_of(checkout: &Path, worktree: &Path) -> Option<String> {
    crate::executor::own_entry(&checkout.join(".git"), worktree)
        .ok()?
        .file_name()?
        .to_str()
        .map(str::to_owned)
}

/// Every ticket folder of the project, with its run file or why it cannot
/// be read; folders with none are left out.
fn run_files(dirs: &ProjectDirs) -> io::Result<Vec<(PathBuf, Result<TicketRun, String>)>> {
    let folder = dirs.tickets();
    let listing = match fs::read_dir(&folder) {
        Ok(listing) => listing,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut found = Vec::new();
    for entry in listing {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let dir = entry.path();
        match read_run_at(&dir.join("run.json")) {
            Ok(None) => {}
            Ok(Some(run)) => found.push((dir, Ok(run))),
            Err(why) => found.push((dir, Err(why))),
        }
    }
    found.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(found)
}

/// Brings every ticket's run file to its refs as they are, for `owlshift
/// unquarantine` once the person typed the project's name (architecture
/// section 8, "Obligations and the quarantine"): the checkout is kept as it
/// is, so the next step starts from it. Each ticket's lock is tried once; a
/// ticket whose lock is held is left as it is and named in the answer. With
/// no dedicated checkout, which a person deleted for a fresh clone, the run
/// files go, the ticket refs having gone with it. A run file that cannot be
/// read is replaced; its ticket is the one whose ref its folder names.
pub fn rebaseline(git: &Git, dirs: &ProjectDirs) -> Result<Vec<String>, String> {
    let checkout = dirs.checkout();
    let checked_out = fs::symlink_metadata(&checkout).is_ok();
    let listed = if checked_out {
        ticket_ref::list(git, &checkout)?
    } else {
        Vec::new()
    };
    let mut skipped = Vec::new();
    for (dir, read) in run_files(dirs).map_err(|e| format!("the tickets' folders: {e}"))? {
        let folder = dir
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let ticket = match &read {
            Ok(run) => run.ticket.clone(),
            Err(_) => match listed.iter().find(|t| component(t.as_str()) == folder) {
                Some(ticket) => ticket.clone(),
                None => {
                    TicketId::new(folder.clone()).map_err(|e| format!("{}: {e}", dir.display()))?
                }
            },
        };
        let Some(_lock) = dirs
            .try_ticket_lock(&ticket)
            .map_err(|e| format!("the lock of {ticket}: {e}"))?
        else {
            skipped.push(ticket.to_string());
            continue;
        };
        if !checked_out {
            match fs::remove_file(dirs.run_file(&ticket)) {
                Err(error) if error.kind() != io::ErrorKind::NotFound => {
                    return Err(format!("the run file of {ticket}: {error}"));
                }
                _ => continue,
            }
        }
        let worktree = dirs.worktree(&ticket);
        let entry = fs::symlink_metadata(&worktree)
            .ok()
            .and_then(|_| entry_of(&checkout, &worktree));
        let mut run = baseline(&ticket, actual(git, &checkout, &ticket)?, entry);
        if let Ok(old) = &read {
            run.generation = old.generation;
        }
        save(dirs, &mut run).map_err(|e| format!("the run file of {ticket}: {e}"))?;
    }
    Ok(skipped)
}

/// The generation of each other ticket's run file when a run took its
/// snapshot, by the ticket's folder.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Marks(BTreeMap<PathBuf, u64>);

/// What a run of `own` is told of the project's other tickets for its check
/// (architecture section 8, "The check among steps"): [`Siblings::marks`]
/// just before its snapshot, [`Siblings::concurrent`] at each of the check's
/// two reads.
#[derive(Clone, Copy, Debug)]
pub struct Siblings<'a> {
    dirs: &'a ProjectDirs,
    own: &'a TicketId,
}

impl<'a> Siblings<'a> {
    pub fn new(dirs: &'a ProjectDirs, own: &'a TicketId) -> Self {
        Self { dirs, own }
    }

    /// The other tickets' run files, `own`'s left out; one that cannot be
    /// read is an error.
    fn others(&self) -> Result<Vec<(PathBuf, TicketRun)>, String> {
        let own = self.dirs.ticket_dir(self.own);
        run_files(self.dirs)
            .map_err(|e| format!("the tickets' folders: {e}"))?
            .into_iter()
            .filter(|(dir, _)| *dir != own)
            .map(|(dir, read)| read.map(|run| (dir, run)))
            .collect()
    }

    /// Each other ticket's generation, read just before the snapshot.
    pub fn marks(&self) -> Result<Marks, String> {
        Ok(Marks(
            self.others()?
                .into_iter()
                .map(|(dir, run)| (dir, run.generation))
                .collect(),
        ))
    }

    /// One read of the other tickets: one whose lock is held is in flight,
    /// its branch, ticket ref and entry left out; one whose lock is free and
    /// whose generation moved since `marks` ended since the snapshot, held to
    /// what its run file records, its entry left out. A ticket with no run
    /// file gives nothing: whatever its holder changes is preceded by a write
    /// of its run file, which the other read sees.
    pub fn concurrent(&self, marks: &Marks) -> Result<Concurrent, String> {
        let mut known = Concurrent::default();
        for (dir, run) in self.others()? {
            let held = self
                .dirs
                .ticket_lock_held_in(&dir)
                .map_err(|e| format!("the lock of {}: {e}", run.ticket))?;
            let moved = marks.0.get(&dir) != Some(&run.generation);
            if !held && !moved {
                continue;
            }
            let branch = format!("refs/heads/{}", branch_for(&run.ticket));
            let reference = ticket_ref(&run.ticket);
            if let Some(entry) = &run.entry {
                known.entries.insert(entry.clone());
            }
            if held {
                known.running.insert(branch);
                known.running.insert(reference);
                continue;
            }
            let values = |recorded: &Option<String>, target: &Option<RunTarget>| {
                let mut values = BTreeSet::from([recorded.clone()]);
                values.extend(target.as_ref().map(|target| target.to.clone()));
                values
            };
            known
                .ended
                .insert(branch, values(&run.tip, &run.writing.tip));
            known
                .ended
                .insert(reference, values(&run.ticket_ref, &run.writing.ticket_ref));
        }
        Ok(known)
    }
}

#[cfg(test)]
mod tests;
