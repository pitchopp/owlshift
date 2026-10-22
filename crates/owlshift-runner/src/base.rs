//! A broken base (OWL-418; architecture section 5, "Parallel", "A broken
//! base"): the project's checks of its bases, `base.json` in its folder
//! ([`ProjectBase`]), read without a lock, replaced whole, and written only
//! by a holder of the project's lock; each ticket's failure waiting for its
//! base, `base_wait.json` in the ticket's folder ([`BaseWait`]); and what a
//! gate run's report says of whose a failure is. The rules are the core's
//! ([`owlshift_core::base`]); [`crate::gate_once`] runs the checks,
//! [`crate::on_demand`] holds the Builds and [`crate::watch`] and
//! [`crate::why`] read the file.

use std::fs;
use std::io;
use std::path::PathBuf;

use serde_json::json;
use sha2::{Digest, Sha256};

use owlshift_contracts::base::{BaseWait, ProjectBase};
use owlshift_contracts::ids::TicketId;
use owlshift_core::base::{Checks, Entry, Failing, Failure, Recorded, Step, Verdict};

use crate::events::{Data, data};
use crate::executor::gate::{GateReport, SETUP_PREFIX, refused_uncommitted};
use crate::executor::{Failure as RunFailure, Outcome, RunReport};
use crate::project::ProjectDirs;
use crate::step::write_atomically;

/// The project's checks of its bases, in its folder.
pub const FILE: &str = "base.json";

/// A ticket's failure waiting for its base, in the ticket's folder.
pub const WAIT_FILE: &str = "base_wait.json";

/// The most bytes of a failing command's output the record and the event
/// keep, from its end.
pub const TAIL_BYTES: usize = 2 * 1024;

/// The lines of that output `owlshift why` shows.
pub const WHY_LINES: usize = 5;

/// The project's base checks file.
pub fn file(dirs: &ProjectDirs) -> PathBuf {
    dirs.root().join(FILE)
}

/// What the project's file holds, without any lock: none when there is no
/// file. A file that does not read, a newer Owlshift's included, is an
/// error, which holds nothing: the caller warns and goes on as today.
pub fn read(dirs: &ProjectDirs) -> Result<Checks, String> {
    let path = file(dirs);
    match fs::read_to_string(&path) {
        Ok(text) => ProjectBase::parse(&text)
            .map(|base| base.checks())
            .map_err(|e| format!("{}: {e}", path.display())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Checks::default()),
        Err(error) => Err(format!("{}: {error}", path.display())),
    }
}

/// Replaces the project's file with `checks`. The caller holds the
/// project's lock: every writer does.
pub fn write(dirs: &ProjectDirs, checks: &Checks) -> Result<(), String> {
    let path = file(dirs);
    write_atomically(&path, &ProjectBase::from_checks(checks).render())
        .map_err(|e| format!("{}: {e}", path.display()))
}

/// The key of a project's setup and gate commands, as a check keeps it: the
/// SHA-256, in lowercase hex, of each setup line, then each gate line, each
/// after its step's mark and ended by a 0 byte.
pub fn commands_key(setup: &[String], gate: &[String]) -> String {
    let mut hash = Sha256::new();
    for (mark, lines) in [(b's', setup), (b'g', gate)] {
        for line in lines {
            hash.update([mark]);
            hash.update(line.as_bytes());
            hash.update([0]);
        }
    }
    hash.finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The command a Build run's gate or setup failed on, when the failure is
/// one a base check can reproduce: a command that failed the project's way,
/// before the harness (`Failure::Setup`) or in the runner's gate after a
/// `done`. `None` for every other outcome, a gate refused for uncommitted
/// changes, a failure around the commands and a runner's fault included.
pub fn failing(report: &RunReport) -> Option<Failing> {
    let (failure, gate) = match &report.outcome {
        Outcome::Failed(RunFailure::Gate(failure)) if !refused_uncommitted(failure) => {
            (failure, report.gate.as_ref())
        }
        Outcome::Failed(RunFailure::Setup(failure)) => (failure, report.setup.as_ref()),
        _ => return None,
    };
    if gate.is_some_and(|gate| gate.runner_fault) {
        return None;
    }
    let command = failure.command.clone()?;
    let step = if failure.reason.starts_with(SETUP_PREFIX) {
        Step::Setup
    } else {
        Step::Gate
    };
    Some(Failing { step, command })
}

/// What a base check's report found: its verdict, or why it says nothing of
/// the commands, a runner's fault or a failure around them, so the check is
/// unchecked.
pub fn verdict(report: &GateReport) -> Result<Verdict, String> {
    let Some(failure) = &report.failure else {
        return Ok(Verdict::Passed);
    };
    if report.runner_fault {
        return Err(format!("the runner's own failure: {}", failure.reason));
    }
    let Some(command) = failure.command.clone() else {
        return Err(failure.reason.clone());
    };
    let step = if failure.reason.starts_with(SETUP_PREFIX) {
        Step::Setup
    } else {
        Step::Gate
    };
    Ok(Verdict::Failed(Failure {
        failing: Failing { step, command },
        reason: failure.reason.clone(),
        tail: tail(&failure.output),
    }))
}

/// The last [`TAIL_BYTES`] of `output`, cut on a character boundary.
pub fn tail(output: &str) -> String {
    let mut start = output.len().saturating_sub(TAIL_BYTES);
    while !output.is_char_boundary(start) {
        start += 1;
    }
    output[start..].to_owned()
}

/// A commit id's first 12 characters.
pub fn short(commit: &str) -> &str {
    commit.get(..12).unwrap_or(commit)
}

/// The step as people read it.
pub fn step_name(step: Step) -> &'static str {
    match step {
        Step::Setup => "setup",
        Step::Gate => "gate",
    }
}

/// The project `decision` event of a check of `commit` at `head` that found
/// `verdict` and changed the hold as `recorded`.
pub fn checked_event(commit: &str, head: &str, verdict: &Verdict, recorded: Recorded) -> Data {
    let change = match recorded {
        Recorded::Opened => "opened",
        Recorded::StillHeld => "still_held",
        Recorded::Lifted => "lifted",
        Recorded::Cached => "cached",
    };
    let mut event = data([
        ("base", json!("checked")),
        ("commit", json!(commit)),
        ("head", json!(head)),
        ("change", json!(change)),
    ]);
    match verdict {
        Verdict::Passed => {
            event.insert("verdict".to_owned(), json!("passed"));
        }
        Verdict::Failed(failure) => {
            event.insert("verdict".to_owned(), json!("failed"));
            event.insert("step".to_owned(), json!(step_name(failure.failing.step)));
            event.insert("command".to_owned(), json!(failure.failing.command));
            event.insert("reason".to_owned(), json!(failure.reason));
            event.insert("tail".to_owned(), json!(failure.tail));
        }
    }
    event
}

/// The line that tells the operator that `commit`, the base `ticket`'s
/// Build was made from, fails `failing` in a fresh worktree too. Fixed
/// words, the commit and the command, which the project file writes.
pub fn line(ticket: &TicketId, commit: &str, failing: &Failing) -> String {
    format!(
        "{ticket}: the base {} fails its {} command `{}` in a fresh worktree too: the Builds \
         made from it wait, counted as nothing. Once fixed, `owlshift gate --at {commit}` checks \
         it again, as does the next commit on the default branch.",
        short(commit),
        step_name(failing.step),
        failing.command
    )
}

/// The ticket's wait file.
pub fn wait_file(dirs: &ProjectDirs, ticket: &TicketId) -> PathBuf {
    dirs.ticket_dir(ticket).join(WAIT_FILE)
}

/// What the ticket's wait file holds; `None` when there is none or it does
/// not read, which starts the Build afresh as before OWL-418.
pub fn read_wait(dirs: &ProjectDirs, ticket: &TicketId) -> Option<BaseWait> {
    let text = fs::read_to_string(wait_file(dirs, ticket)).ok()?;
    BaseWait::parse(&text).ok()
}

/// Keeps `wait` as the ticket's, replacing any.
pub fn write_wait(dirs: &ProjectDirs, ticket: &TicketId, wait: &BaseWait) -> io::Result<()> {
    write_atomically(&wait_file(dirs, ticket), &wait.render())
}

/// Every ticket's wait file that reads, in the folders' order.
pub fn waits(dirs: &ProjectDirs) -> Vec<BaseWait> {
    let Ok(listing) = fs::read_dir(dirs.tickets()) else {
        return Vec::new();
    };
    let mut folders: Vec<PathBuf> = listing.flatten().map(|entry| entry.path()).collect();
    folders.sort();
    folders
        .into_iter()
        .filter_map(|folder| fs::read_to_string(folder.join(WAIT_FILE)).ok())
        .filter_map(|text| BaseWait::parse(&text).ok())
        .collect()
}

/// The ticket whose failure waits for a pending check of `checks`, its
/// wait matching the entry's commit and commands, the first of `waits` that
/// `skip` leaves: the ticket a watch gives the check's priority to, one
/// started again included (OWL-418).
pub fn pending_ticket(
    checks: &Checks,
    waits: &[BaseWait],
    skip: impl Fn(&TicketId) -> bool,
) -> Option<TicketId> {
    waits
        .iter()
        .filter(|wait| !skip(&wait.ticket))
        .find(|wait| {
            checks.standing().any(|check| {
                matches!(check.entry, Entry::Pending { .. })
                    && check.commit == wait.configured_at
                    && check.commands == wait.commands
            })
        })
        .map(|wait| wait.ticket.clone())
}

/// Removes the ticket's wait file, if any.
pub fn remove_wait(dirs: &ProjectDirs, ticket: &TicketId) -> io::Result<()> {
    match fs::remove_file(wait_file(dirs, ticket)) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use owlshift_adapters::forge::Repo;
    use owlshift_contracts::brief::GateFailure;
    use owlshift_core::base::Step;

    use super::*;

    fn report(failure: Option<GateFailure>, runner_fault: bool) -> GateReport {
        GateReport {
            commands: vec!["make test".to_owned()],
            commit: None,
            log: PathBuf::new(),
            failure,
            breach: Vec::new(),
            steps: Vec::new(),
            setup: Vec::new(),
            runner_fault,
        }
    }

    fn failure(command: Option<&str>, reason: &str) -> GateFailure {
        GateFailure {
            command: command.map(ToOwned::to_owned),
            reason: reason.to_owned(),
            output: "x".repeat(3 * TAIL_BYTES),
            truncated: false,
        }
    }

    #[test]
    fn the_key_tells_setup_from_gate_and_order_apart() {
        let lines = |l: &[&str]| l.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        let key = commands_key(&lines(&["a"]), &lines(&["b"]));
        assert_eq!(key.len(), 64);
        assert_eq!(key, commands_key(&lines(&["a"]), &lines(&["b"])));
        assert_ne!(key, commands_key(&lines(&[]), &lines(&["a", "b"])));
        assert_ne!(key, commands_key(&lines(&["b"]), &lines(&["a"])));
        assert_ne!(key, commands_key(&lines(&["ab"]), &lines(&[])));
    }

    /// Only a command's own failure is a verdict: a runner's fault and a
    /// failure around the commands leave the check unchecked.
    #[test]
    fn a_check_says_nothing_of_a_runners_fault() {
        assert_eq!(verdict(&report(None, false)), Ok(Verdict::Passed));
        let Ok(Verdict::Failed(red)) = verdict(&report(
            Some(failure(Some("pnpm i"), "setup: exit status 1")),
            false,
        )) else {
            panic!("a verdict")
        };
        assert_eq!(
            red.failing,
            Failing {
                step: Step::Setup,
                command: "pnpm i".to_owned()
            }
        );
        assert_eq!(red.tail.len(), TAIL_BYTES);
        assert!(verdict(&report(Some(failure(Some("make test"), "x")), true)).is_err());
        assert!(verdict(&report(Some(failure(None, "the gate moved HEAD")), false)).is_err());
    }

    #[test]
    fn the_tail_is_cut_on_a_character_boundary() {
        let text = format!("{}é", "a".repeat(TAIL_BYTES));
        let cut = tail(&text);
        assert!(
            cut.len() <= TAIL_BYTES && cut.ends_with('é'),
            "{}",
            cut.len()
        );
    }

    #[test]
    fn a_wait_and_the_checks_are_kept_and_removed() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = ProjectDirs::new(dir.path(), &Repo::parse("acme/widgets").unwrap());
        assert_eq!(read(&dirs), Ok(Checks::default()));
        let mut checks = Checks::default();
        let failing = Failing {
            step: Step::Gate,
            command: "make test".to_owned(),
        };
        checks.mark_pending("c", "k", &failing, 1);
        write(&dirs, &checks).unwrap();
        assert_eq!(read(&dirs), Ok(checks));
        let ticket = TicketId::new("OWL-1").unwrap();
        assert!(read_wait(&dirs, &ticket).is_none());
        remove_wait(&dirs, &ticket).unwrap();
        fs::create_dir_all(dirs.ticket_dir(&ticket)).unwrap();
        fs::write(wait_file(&dirs, &ticket), "not json").unwrap();
        assert!(
            read_wait(&dirs, &ticket).is_none(),
            "an unreadable wait is none"
        );
        remove_wait(&dirs, &ticket).unwrap();
        assert!(!wait_file(&dirs, &ticket).exists());
    }

    /// The ticket a pending check's priority goes to: one whose wait matches
    /// the entry, and not one skipped; none for a check that ran.
    #[test]
    fn a_pending_check_is_matched_to_its_waiting_ticket() {
        use owlshift_contracts::base::KeptFailing;
        use owlshift_contracts::format::Format;
        let failing = Failing {
            step: Step::Setup,
            command: "sh setup.sh".to_owned(),
        };
        let wait = |ticket: &str, commit: &str| BaseWait {
            format: Format,
            ticket: TicketId::new(ticket).unwrap(),
            configured_at: commit.to_owned(),
            commands: "k".to_owned(),
            round: 0,
            head: "h".repeat(40),
            failing: KeptFailing::from(&failing),
            done: None,
        };
        let waits = [
            wait("OWL-1", "other"),
            wait("OWL-2", "base"),
            wait("OWL-3", "base"),
        ];
        let mut checks = Checks::default();
        assert_eq!(pending_ticket(&checks, &waits, |_| false), None);
        checks.mark_pending("base", "k", &failing, 1);
        let two = TicketId::new("OWL-2").unwrap();
        assert_eq!(
            pending_ticket(&checks, &waits, |_| false),
            Some(two.clone())
        );
        assert_eq!(
            pending_ticket(&checks, &waits, |ticket| *ticket == two),
            Some(TicketId::new("OWL-3").unwrap())
        );
        checks.record_check(("base", "k", "h"), 2, Verdict::Passed, None);
        assert_eq!(pending_ticket(&checks, &waits, |_| false), None);
    }
}
