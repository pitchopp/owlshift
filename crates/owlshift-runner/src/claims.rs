//! Claims on the git remote (P10, OWL-586; build plan "P10 · Teams",
//! "Claims"; architecture section 7, "Claims"): under `[policy] claims =
//! "remote"`, which runner works a ticket, so that two machines never work
//! one.
//!
//! A claim is `refs/owlshift/claims/<ticket>` on `origin`, a commit without
//! a parent whose tree holds `claim.json` ([`Claim`], format 2). The runner
//! makes that commit in the dedicated checkout with git's plumbing, moving no
//! ref there, since a ref that moved during a run would breach the isolation
//! check, and pushes it by its hash, every push leased on what `origin` must
//! hold (live checks C3 and T1): a take only while the ref is absent, a
//! renewal, a new held set and a delete only while it holds the commit last
//! pushed. That commit is kept in `claimed.json` in the ticket's folder of
//! the data directory ([`TicketClaimed`]), written before each push with
//! the commit under way, so that a crash at any point reads back:
//!
//! - a take never confirmed, its claim absent on `origin`, is taken afresh;
//! - a claim at the commit confirmed or the one under way is this runner's;
//! - a claim absent while a delete was under way was deleted;
//! - any other claim whose `runner` is this runner's is adopted, its commit
//!   recorded; a claim absent or another's is lost ([`TicketClaimLost`],
//!   `claim_lost.json`).
//!
//! A `claim.lock` beside the record serializes a command's take and held
//! set, watch's renewal and its sweep, each read of the record, push and
//! write of it. It is taken after the ticket's lock, never before, and is
//! never held under the project's lock: every call here may reach the
//! network, each git command bounded by the runner's git deadline. A claim
//! another runner holds is read into `refs/remotes/owlshift/claims/<ticket>`,
//! which the isolation check leaves out (live check T2).

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use jiff::{SignedDuration, Timestamp};

use owlshift_adapters::forge::CommitId;
use owlshift_adapters::forge::push::{
    PushError, RefPushed, RefUpdate, read_ref_push_parts, ref_push_command,
};
use owlshift_contracts::format::{ContractError, Format};
use owlshift_contracts::ids::TicketId;
use owlshift_contracts::refs::{
    CLAIM_FILE, Claim, ClaimHeld, ClaimHolder, MAX_CLAIM_BYTES, TicketClaimLost, TicketClaimed,
    claim_ref,
};
use owlshift_core::floor::{self, Action, HumanApproval, Origin};

use crate::executor::GIT_TIMEOUT;
use crate::executor::Git;
use crate::project::{self, ProjectDirs};
use crate::step::write_atomically;

pub mod repair;

/// This install's runner identifier, at the data directory's root.
pub const RUNNER_FILE: &str = "runner";
/// The record of a ticket's claim this runner last pushed, in its folder.
pub const CLAIMED_FILE: &str = "claimed.json";
/// The record of a ticket whose claim this runner lost, in its folder.
pub const LOST_FILE: &str = "claim_lost.json";
/// The lock that serializes a ticket's claim's record and pushes.
const LOCK_FILE: &str = "claim.lock";
/// How long a claim holds after its last renewal.
pub const LEASE: SignedDuration = SignedDuration::from_hours(24);
/// How often a running watch renews its claims.
pub const RENEW_EVERY: Duration = Duration::from_secs(60 * 60);
/// How long a renewal that could not be made waits before it is tried
/// again: watch's pass, at most once a minute.
pub const RENEW_RETRY: Duration = Duration::from_secs(60);
/// How long a command waits for the claim's lock: a renewal under way holds
/// it for one push, at most the runner git's deadline, and its reads.
const LOCK_WAIT: Duration = Duration::from_secs(GIT_TIMEOUT.as_secs() + 30);
const LOCK_POLL: Duration = Duration::from_millis(50);
/// The remote claims live on.
const REMOTE: &str = "origin";
/// Where a claim read from `origin` is fetched to: a remote-tracking
/// ref, which the isolation check leaves out (live check T2).
pub const FETCHED_PREFIX: &str = "refs/remotes/owlshift/claims/";
/// The refspec that fetches every claim of `origin` to [`FETCHED_PREFIX`]
/// (live check T2): every fetch of the dedicated checkout adds it, and
/// watch's own fetch of the claims is it alone (OWL-587).
pub const CLAIMS_REFSPEC: &str = "+refs/owlshift/claims/*:refs/remotes/owlshift/claims/*";
/// How often a watch fetches the claims of `origin` on its own (OWL-587):
/// at most once a minute, as a served watch fetches its default branch.
pub const FETCH_EVERY: SignedDuration = SignedDuration::from_secs(60);

/// A claim of `origin` as the dedicated checkout last fetched it, under
/// [`FETCHED_PREFIX`] (OWL-587): its ticket, by its ref's name, the commit
/// it is at, and what it says or why it does not read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FetchedClaim {
    pub ticket: TicketId,
    pub commit: String,
    pub claim: Result<Claim, ClaimUnread>,
}

impl FetchedClaim {
    /// Whether this runner's: a claim that does not read names no runner,
    /// so it is another's (build plan "Other runners' tickets").
    pub fn is_of(&self, runner: &RunnerId) -> bool {
        self.claim
            .as_ref()
            .is_ok_and(|claim| claim.runner == runner.as_str())
    }
}

/// Why a fetched claim does not read: what it holds is never named, and
/// it is never taken (build plan "Other runners' tickets", "A claim this
/// binary cannot read").
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClaimUnread {
    pub reason: String,
    /// Written in a later format than this binary reads: an upgrade reads it.
    pub newer: bool,
}

/// Fetches every claim of `origin` alone into [`FETCHED_PREFIX`], the
/// claims deleted there pruned, nothing else touched (live check T2): what
/// watch makes once a minute under `remote` (OWL-587), within the runner
/// git's deadline. A failed fetch says why, in git's last line.
pub fn fetch(git: &Git, checkout: &Path) -> Result<(), String> {
    fetch_within(git, checkout, None)
}

/// [`fetch`] within `deadline`, the runner git's when `None`: a first
/// clone's fetch of the claims shares the clone's own.
pub fn fetch_within(git: &Git, checkout: &Path, deadline: Option<Duration>) -> Result<(), String> {
    let args = [
        "fetch",
        "--quiet",
        "--no-tags",
        "--prune",
        "--no-write-fetch-head",
        REMOTE,
        CLAIMS_REFSPEC,
    ];
    let out = match deadline {
        Some(deadline) => git.output_within(checkout, &args, None, deadline),
        None => git.output(checkout, &args, None),
    }
    .map_err(|e| format!("fetching {REMOTE}'s claims: {e}"))?;
    if out.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    let last = stderr.lines().rev().find(|line| !line.trim().is_empty());
    Err(format!(
        "fetching {REMOTE}'s claims: git fetch failed: {}",
        last.unwrap_or("no error output").trim()
    ))
}

/// When each claim this runner recorded in the project was taken, in
/// milliseconds: the order key of its ticket against another runner's
/// claim (OWL-587; build plan "Other runners' tickets", "Which goes on").
pub fn claimed_times(dirs: &ProjectDirs) -> BTreeMap<TicketId, i64> {
    recorded(dirs)
        .into_iter()
        .map(|record| (record.ticket, record.claim.claimed_at.as_millisecond()))
        .collect()
}

/// What a check of this runner's claims against a fetch found (OWL-587).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Checked {
    /// Found this runner's at another commit, now recorded.
    pub adopted: Vec<TicketId>,
    /// Found gone from `origin`, or another's: their records removed.
    pub lost: Vec<(TicketId, Option<ClaimHolder>)>,
    /// Why some could not be checked: nothing was lost for it.
    pub failed: Option<String>,
}

/// Checks this runner's claims against `fetched`, the claims a fetch that
/// started at `since` left, by ticket and commit (OWL-587; build plan
/// "Claims", "Lost", at a pass). A claim confirmed and not being deleted,
/// whose record was written before the fetch started, and that the fetch
/// shows gone or at a commit its record does not know, is read again under
/// its claim's lock, a single try: one `ls-remote` for all such, then as a
/// renewal reads a refusal. A record written since, a command
/// having just taken or pushed it, or any read that fails, loses nothing.
pub fn check_fetched(
    at: ClaimsAt<'_>,
    fetched: &BTreeMap<TicketId, String>,
    since: std::time::SystemTime,
) -> Checked {
    let checkout = at.dirs.checkout();
    let mut checked = Checked::default();
    let runner = match runner_id(at.dirs.data_dir()) {
        Ok(runner) => runner,
        Err(error) => {
            checked.failed = Some(format!("this machine's runner identifier: {error}"));
            return checked;
        }
    };
    let known = |record: &TicketClaimed, oid: &str| {
        record.commit.as_deref() == Some(oid) || record.next.as_deref() == Some(oid)
    };
    let mut locks = Vec::new();
    let mut due = Vec::new();
    for record in recorded(at.dirs) {
        let ticket = record.ticket.clone();
        let Some(commit) = record.commit.clone().filter(|_| !record.deleting) else {
            continue;
        };
        if record.claim.runner != runner.as_str()
            || fetched.get(&ticket).is_some_and(|oid| known(&record, oid))
        {
            continue;
        }
        let written = fs::metadata(record_file(at.dirs, &ticket)).and_then(|meta| meta.modified());
        if !written.is_ok_and(|written| written < since) {
            continue;
        }
        match lock(at.dirs, &ticket, Duration::ZERO) {
            Ok(Some(held)) => locks.push(held),
            Ok(None) => continue,
            Err(error) => {
                checked.failed = Some(error.to_string());
                continue;
            }
        }
        // Read again under its lock: a command may have just written it.
        let Some(record) = read_record(at.dirs, &ticket) else {
            continue;
        };
        if record.commit.as_deref() == Some(commit.as_str()) && !record.deleting {
            due.push(record);
        }
    }
    if due.is_empty() {
        return checked;
    }
    let names: Vec<String> = due.iter().map(|record| claim_ref(&record.ticket)).collect();
    let heads = match ls_remote(at.git, &checkout, &names) {
        Ok(heads) => heads,
        Err(why) => {
            checked.failed = Some(why);
            return checked;
        }
    };
    for record in due {
        let head = heads.get(&claim_ref(&record.ticket));
        if head.is_some_and(|oid| known(&record, oid)) {
            continue;
        }
        let ticket = record.ticket;
        match settle(at, &checkout, &ticket, head.map(String::as_str), &runner) {
            Ok(Settled::Adopted) => checked.adopted.push(ticket),
            Ok(Settled::Lost(holder)) => checked.lost.push((ticket, holder)),
            Err(why) => checked.failed = Some(why),
        }
    }
    drop(locks);
    checked
}

/// What `origin`'s claim of a ticket says of this runner's, at none of the
/// commits its record knows.
enum Settled {
    /// This runner's at another commit, a crash having come between a push
    /// and its record: now recorded.
    Adopted,
    /// Gone, or another's: the record removed.
    Lost(Option<ClaimHolder>),
}

/// Settles `ticket`'s claim, which `origin` holds at `head` (`None`: gone),
/// a commit its record does not know, for `runner`: the claim read, adopted
/// when it is this runner's, lost otherwise, its record removed. A read
/// that fails settles nothing. A renewal's refusal and a pass's check read
/// it so (OWL-586, OWL-587).
fn settle(
    at: ClaimsAt<'_>,
    checkout: &Path,
    ticket: &TicketId,
    head: Option<&str>,
    runner: &RunnerId,
) -> Result<Settled, String> {
    let Some(oid) = head else {
        remove_record(at.dirs, ticket);
        return Ok(Settled::Lost(None));
    };
    match read_remote(at.git, checkout, ticket)? {
        Some(claim) if claim.runner == runner.as_str() => {
            adopt(at.dirs, ticket, oid, claim)?;
            Ok(Settled::Adopted)
        }
        found => {
            remove_record(at.dirs, ticket);
            Ok(Settled::Lost(found.map(|claim| claim.holder())))
        }
    }
}

/// This install's runner identifier: 16 lowercase hexadecimal digits, made
/// once per data directory, never a host name, which a public repository
/// would publish.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RunnerId(String);

impl RunnerId {
    /// `text` when it is 16 lowercase hexadecimal digits.
    pub fn parse(text: &str) -> Option<Self> {
        (text.len() == 16 && text.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f')))
            .then(|| Self(text.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The runner identifier of `data_dir`, made the first time from 8 random
/// bytes. Two processes making it at once agree: the first link wins, and
/// the other reads it.
pub fn runner_id(data_dir: &Path) -> io::Result<RunnerId> {
    let path = data_dir.join(RUNNER_FILE);
    if let Some(id) = read_runner(&path)? {
        return Ok(id);
    }
    let mut bytes = [0u8; 8];
    getrandom::fill(&mut bytes).map_err(|e| io::Error::other(e.to_string()))?;
    let made: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    fs::create_dir_all(data_dir)?;
    let partial = data_dir.join(format!("{RUNNER_FILE}.{made}.partial"));
    fs::write(&partial, format!("{made}\n"))?;
    let linked = fs::hard_link(&partial, &path);
    let _ = fs::remove_file(&partial);
    match linked {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    read_runner(&path)?.ok_or_else(|| {
        io::Error::other(format!(
            "{} does not hold 16 lowercase hexadecimal digits",
            path.display()
        ))
    })
}

/// This data directory's runner identifier when one was made, read and
/// never made: what `owlshift status`, `owlshift why` and the web UI tell
/// this runner's claims by (OWL-588), which write nothing. One that does
/// not read is none.
pub fn this_runner(data_dir: &Path) -> Option<RunnerId> {
    read_runner(&data_dir.join(RUNNER_FILE)).ok().flatten()
}

fn read_runner(path: &Path) -> io::Result<Option<RunnerId>> {
    match fs::read_to_string(path) {
        Ok(text) => RunnerId::parse(text.trim()).map(Some).ok_or_else(|| {
            io::Error::other(format!(
                "{} does not hold 16 lowercase hexadecimal digits",
                path.display()
            ))
        }),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// Who this machine claims tickets as: its runner, and its operator, the
/// tracker account of the project's key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClaimIdentity {
    pub runner: RunnerId,
    pub account: String,
}

/// The dedicated checkout claims are made and pushed from, and the
/// project's folder their records live in.
#[derive(Clone, Copy)]
pub struct ClaimsAt<'a> {
    pub git: &'a Git,
    pub dirs: &'a ProjectDirs,
}

/// What a take found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Took {
    /// The ticket had no claim: this runner's is now on `origin`.
    Taken { commit: String },
    /// The claim on `origin` is the one this runner last pushed.
    Own,
    /// The claim on `origin` is this runner's at another commit than its
    /// record's, a crash having come between a push and the record: now
    /// recorded.
    Adopted { commit: String },
}

/// Why a take did not give this runner the claim. Nothing was written on
/// the tracker, the forge, the ticket ref or the branch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NotTaken {
    /// Another runner holds the ticket; `None` when its claim does not read.
    Claimed(Option<ClaimHolder>),
    /// This runner's claim is gone from `origin`, or another's now: lost.
    Lost(Option<ClaimHolder>),
    /// The ticket's branch is on `origin`, and this runner never pushed it:
    /// what an earlier runner left is a person's to close first.
    Branch { branch: String },
    /// `origin` could not be read or written: tried again at a later pass.
    Unreachable(String),
    /// Another process of this runner holds the claim's lock past the wait.
    Busy,
}

/// Takes the claim of `ticket` for `runner`, operated by `operator`,
/// holding `held`, at `now` (build plan "Claims", "Taken"): goes on when
/// the claim on `origin` is this runner's, takes it when there is none, and
/// refuses when it is another's. `branch`, given when the ticket's ref keeps
/// no delivery of this runner's, is the ticket's branch, whose presence on
/// `origin` refuses a new take.
pub fn take(
    at: ClaimsAt<'_>,
    ticket: &TicketId,
    runner: &RunnerId,
    operator: &str,
    held: ClaimHeld,
    now: Timestamp,
    branch: Option<&str>,
) -> Result<Took, NotTaken> {
    let checkout = at.dirs.checkout();
    let Some(_lock) =
        lock(at.dirs, ticket, LOCK_WAIT).map_err(|e| NotTaken::Unreachable(e.to_string()))?
    else {
        return Err(NotTaken::Busy);
    };
    let name = claim_ref(ticket);
    let mut names = vec![name.clone()];
    let record = read_record(at.dirs, ticket);
    if let Some(branch) = branch {
        names.push(format!("refs/heads/{branch}"));
    }
    let heads = ls_remote(at.git, &checkout, &names).map_err(NotTaken::Unreachable)?;
    let remote = heads.get(&name).cloned();
    // A record whose claim names another runner, this data directory's
    // identifier having been made again since, is not this runner's: lost
    // once confirmed, forgotten otherwise.
    let record = match record {
        Some(record) if record.claim.runner != runner.as_str() => {
            remove_record(at.dirs, ticket);
            if record.commit.is_some() {
                return Err(NotTaken::Lost(read_holder(at.git, &checkout, ticket)));
            }
            None
        }
        record => record,
    };
    if let Some(record) = record {
        let known = |oid: &str| {
            record.commit.as_deref() == Some(oid) || record.next.as_deref() == Some(oid)
        };
        match &remote {
            Some(oid) if known(oid) => {
                if record.commit.as_deref() != Some(oid) || record.next.is_some() || record.deleting
                {
                    let confirmed = TicketClaimed {
                        commit: Some(oid.clone()),
                        next: None,
                        deleting: false,
                        ..record
                    };
                    write_record(at.dirs, &confirmed).map_err(NotTaken::Unreachable)?;
                }
                return Ok(Took::Own);
            }
            Some(oid) => {
                let found =
                    read_remote(at.git, &checkout, ticket).map_err(NotTaken::Unreachable)?;
                let holder = match found {
                    Some(claim) if claim.runner == runner.as_str() => {
                        adopt(at.dirs, ticket, oid, claim).map_err(NotTaken::Unreachable)?;
                        return Ok(Took::Adopted {
                            commit: oid.clone(),
                        });
                    }
                    found => found.map(|claim| claim.holder()),
                };
                remove_record(at.dirs, ticket);
                // A take never confirmed lost nothing: another runner's
                // landed first.
                return Err(if record.commit.is_some() {
                    NotTaken::Lost(holder)
                } else {
                    NotTaken::Claimed(holder)
                });
            }
            None if record.commit.is_some() && !record.deleting => {
                remove_record(at.dirs, ticket);
                return Err(NotTaken::Lost(None));
            }
            // A take never confirmed, or a delete under way, found absent:
            // taken afresh.
            None => remove_record(at.dirs, ticket),
        }
    } else if let Some(oid) = &remote {
        let found = read_remote(at.git, &checkout, ticket).map_err(NotTaken::Unreachable)?;
        return match found {
            Some(claim) if claim.runner == runner.as_str() => {
                adopt(at.dirs, ticket, oid, claim).map_err(NotTaken::Unreachable)?;
                Ok(Took::Adopted {
                    commit: oid.clone(),
                })
            }
            found => Err(NotTaken::Claimed(found.map(|claim| claim.holder()))),
        };
    }
    if let Some(branch) = branch
        && heads.contains_key(&format!("refs/heads/{branch}"))
    {
        return Err(NotTaken::Branch {
            branch: branch.to_owned(),
        });
    }
    let claim = Claim {
        format: Format,
        runner: runner.as_str().to_owned(),
        operator: operator.to_owned(),
        claimed_at: now,
        expires_at: now.saturating_add(LEASE).unwrap_or(now),
        held,
    };
    claim.check().map_err(NotTaken::Unreachable)?;
    let commit = make_commit(at.git, &checkout, ticket, &claim).map_err(NotTaken::Unreachable)?;
    let pending = TicketClaimed {
        format: Format,
        ticket: ticket.clone(),
        commit: None,
        next: Some(commit.as_str().to_owned()),
        deleting: false,
        claim,
    };
    write_record(at.dirs, &pending).map_err(NotTaken::Unreachable)?;
    let update = RefUpdate::Create {
        name: &name,
        commit: &commit,
    };
    match push(at.git, &checkout, &[update], false).remove(0) {
        Ok(_) => {
            let taken = TicketClaimed {
                commit: Some(commit.as_str().to_owned()),
                next: None,
                ..pending
            };
            write_record(at.dirs, &taken).map_err(NotTaken::Unreachable)?;
            Ok(Took::Taken {
                commit: commit.as_str().to_owned(),
            })
        }
        // Another runner's take landed between the read and the push.
        Err(PushError::Rejected { .. }) => {
            remove_record(at.dirs, ticket);
            Err(NotTaken::Claimed(read_holder(at.git, &checkout, ticket)))
        }
        // The record of the push under way stays: the next take reads
        // whether it landed.
        Err(PushError::Failed { message }) => Err(NotTaken::Unreachable(message)),
    }
}

/// What a renewal did (build plan "Claims", "Renewed").
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Renewal {
    /// Renewed, or found renewed by an earlier push this runner made.
    pub renewed: Vec<TicketId>,
    /// Found this runner's at another commit, now recorded.
    pub adopted: Vec<TicketId>,
    /// Found gone from `origin`, or another's: their records removed.
    pub lost: Vec<(TicketId, Option<ClaimHolder>)>,
    /// Their lock was held by a command of this runner: left for the next.
    pub busy: Vec<TicketId>,
    /// Why some could not be renewed, when `origin` could not be read or
    /// written: tried again at a later pass.
    pub failed: Option<String>,
}

impl Renewal {
    /// Whether every claim was settled: none left for a later pass.
    pub fn settled(&self) -> bool {
        self.busy.is_empty() && self.failed.is_none()
    }
}

/// Renews every claim this runner holds in the project, at `now`: each for
/// [`LEASE`] more, holding what `held_of` says its ticket holds now, in one
/// atomic push leased on the commits last pushed (live check T1). A
/// refusal reads `origin` to tell the claims lost from those found
/// renewed or this runner's, which are pushed once more.
pub fn renew(
    at: ClaimsAt<'_>,
    held_of: &dyn Fn(&TicketId) -> ClaimHeld,
    now: Timestamp,
) -> Renewal {
    let checkout = at.dirs.checkout();
    let mut renewal = Renewal::default();
    let runner = match runner_id(at.dirs.data_dir()) {
        Ok(runner) => runner,
        Err(error) => {
            renewal.failed = Some(format!("this machine's runner identifier: {error}"));
            return renewal;
        }
    };
    let mut locks = Vec::new();
    let mut due: Vec<(TicketClaimed, String, CommitId)> = Vec::new();
    for record in recorded(at.dirs) {
        let ticket = record.ticket.clone();
        // A take never confirmed is its command's, or the sweep's; a delete
        // under way is the sweep's.
        let Some(expected) = record.commit.clone().filter(|_| !record.deleting) else {
            continue;
        };
        // A ticket this runner stopped working, its claim released here
        // and its delete not made (OWL-588), is not renewed: its claim is
        // left to expire.
        if lost(at.dirs, &ticket).is_some() {
            continue;
        }
        match lock(at.dirs, &ticket, Duration::ZERO) {
            Ok(Some(held)) => locks.push(held),
            Ok(None) => {
                renewal.busy.push(ticket);
                continue;
            }
            Err(error) => {
                renewal.failed = Some(error.to_string());
                continue;
            }
        }
        // Read again under its lock: a command may have just written it.
        let Some(record) = read_record(at.dirs, &ticket) else {
            continue;
        };
        // A record of another runner's identity is not this runner's: lost
        // once confirmed, as a take finds it.
        if record.claim.runner != runner.as_str() {
            remove_record(at.dirs, &ticket);
            let holder = read_holder(at.git, &checkout, &ticket);
            renewal.lost.push((ticket, holder));
            continue;
        }
        let Some(expected) = record.commit.clone().filter(|commit| *commit == expected) else {
            renewal.busy.push(ticket);
            continue;
        };
        let claim = Claim {
            expires_at: now.saturating_add(LEASE).unwrap_or(now),
            held: held_of(&ticket),
            ..record.claim.clone()
        };
        let commit = match make_commit(at.git, &checkout, &ticket, &claim) {
            Ok(commit) => commit,
            Err(why) => {
                renewal.failed = Some(why);
                continue;
            }
        };
        let pending = TicketClaimed {
            next: Some(commit.as_str().to_owned()),
            claim,
            ..record
        };
        if let Err(why) = write_record(at.dirs, &pending) {
            renewal.failed = Some(why);
            continue;
        }
        due.push((pending, expected, commit));
    }
    for round in 0..2 {
        if due.is_empty() {
            break;
        }
        let expected: Vec<CommitId> = due
            .iter()
            .map(|(_, expected, _)| CommitId::new(expected).expect("a record keeps full ids"))
            .collect();
        let names: Vec<String> = due
            .iter()
            .map(|(record, ..)| claim_ref(&record.ticket))
            .collect();
        let updates: Vec<RefUpdate<'_>> = due
            .iter()
            .zip(&names)
            .zip(&expected)
            .map(|(((_, _, commit), name), expected)| RefUpdate::Update {
                name,
                commit,
                expected,
            })
            .collect();
        let pushed = push(at.git, &checkout, &updates, true);
        if pushed.iter().all(Result::is_ok) {
            for (record, _, commit) in due.drain(..) {
                confirm(at.dirs, record, commit.as_str(), &mut renewal);
            }
            break;
        }
        if let Some(Err(PushError::Failed { message })) = pushed
            .iter()
            .find(|r| matches!(r, Err(PushError::Failed { .. })))
        {
            // The records keep the push under way: the next renewal reads
            // whether it landed.
            renewal.failed = Some(message.clone());
            break;
        }
        // Refused: which were lost, renewed already, or still due.
        let heads = match ls_remote(at.git, &checkout, &names) {
            Ok(heads) => heads,
            Err(why) => {
                renewal.failed = Some(why);
                break;
            }
        };
        let mut again = Vec::new();
        for (record, expected, commit) in due.drain(..) {
            let ticket = record.ticket.clone();
            match heads.get(&claim_ref(&ticket)) {
                Some(oid) if *oid == commit.as_str() => {
                    confirm(at.dirs, record, oid, &mut renewal);
                }
                Some(oid) if *oid == expected => again.push((record, expected, commit)),
                head => match settle(at, &checkout, &ticket, head.map(String::as_str), &runner) {
                    Ok(Settled::Adopted) => renewal.adopted.push(ticket),
                    Ok(Settled::Lost(holder)) => renewal.lost.push((ticket, holder)),
                    Err(why) => renewal.failed = Some(why),
                },
            }
        }
        due = again;
        if round == 1 && !due.is_empty() {
            renewal.failed = Some("origin refused the renewal twice".to_owned());
        }
    }
    drop(locks);
    renewal
}

fn confirm(dirs: &ProjectDirs, record: TicketClaimed, commit: &str, renewal: &mut Renewal) {
    let ticket = record.ticket.clone();
    let confirmed = TicketClaimed {
        commit: Some(commit.to_owned()),
        next: None,
        ..record
    };
    match write_record(dirs, &confirmed) {
        Ok(()) => renewal.renewed.push(ticket),
        Err(why) => renewal.failed = Some(why),
    }
}

/// Pushes `held` as what `ticket` holds, when its claim says otherwise, at
/// `now`, renewing it at once (build plan "Claims", "Its held set"):
/// `Ok(true)` once pushed, `Ok(false)` when there was nothing to push or no
/// claim of this runner's. A refusal or a failure is for the caller to warn
/// of: the next renewal pushes the held set again, and tells a lost claim.
pub fn update_held(
    at: ClaimsAt<'_>,
    ticket: &TicketId,
    held: ClaimHeld,
    now: Timestamp,
) -> Result<bool, String> {
    let checkout = at.dirs.checkout();
    let Some(_lock) = lock(at.dirs, ticket, LOCK_WAIT).map_err(|e| e.to_string())? else {
        return Err("another process of this runner holds the claim's lock".to_owned());
    };
    let Some(record) = read_record(at.dirs, ticket) else {
        return Ok(false);
    };
    let Some(expected) = record.commit.clone().filter(|_| !record.deleting) else {
        return Ok(false);
    };
    // A record of another runner's identity is not this runner's to push:
    // the next take or renewal tells it lost.
    let runner = runner_id(at.dirs.data_dir()).map_err(|e| e.to_string())?;
    if record.claim.runner != runner.as_str() {
        return Err(format!(
            "the claim's record names runner {}, not this runner, {}",
            record.claim.runner,
            runner.as_str()
        ));
    }
    if record.claim.held == held && record.next.is_none() {
        return Ok(false);
    }
    let claim = Claim {
        expires_at: now.saturating_add(LEASE).unwrap_or(now),
        held,
        ..record.claim.clone()
    };
    let commit = make_commit(at.git, &checkout, ticket, &claim)?;
    let pending = TicketClaimed {
        next: Some(commit.as_str().to_owned()),
        claim,
        ..record
    };
    write_record(at.dirs, &pending)?;
    let name = claim_ref(ticket);
    let expected = CommitId::new(&expected).map_err(|e| e.to_string())?;
    let update = RefUpdate::Update {
        name: &name,
        commit: &commit,
        expected: &expected,
    };
    match push(at.git, &checkout, &[update], false).remove(0) {
        Ok(_) => {
            let confirmed = TicketClaimed {
                commit: Some(commit.as_str().to_owned()),
                next: None,
                ..pending
            };
            write_record(at.dirs, &confirmed)?;
            Ok(true)
        }
        Err(error) => Err(error.to_string()),
    }
}

/// What a delete did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Deleted {
    /// This runner's claim was deleted from `origin`.
    Deleted,
    /// No claim of this runner's was on `origin`: gone already, or
    /// another's. Its record is removed all the same.
    NotOurs,
    /// This runner recorded no claim of the ticket.
    None,
}

/// Deletes this runner's claim of `ticket`, leased on the commit it last
/// pushed (build plan "Claims", "Deleted"), and removes its record. The
/// record says a delete is under way before the push, so a claim found
/// absent afterwards is never read as lost. A failure keeps the record, for
/// a later try.
pub fn delete(at: ClaimsAt<'_>, ticket: &TicketId) -> Result<Deleted, String> {
    let checkout = at.dirs.checkout();
    let Some(_lock) = lock(at.dirs, ticket, LOCK_WAIT).map_err(|e| e.to_string())? else {
        return Err("another process of this runner holds the claim's lock".to_owned());
    };
    let Some(record) = read_record(at.dirs, ticket) else {
        return Ok(Deleted::None);
    };
    let mut expected = record
        .commit
        .clone()
        .or_else(|| record.next.clone())
        .expect("a record names a commit");
    let deleting = TicketClaimed {
        deleting: true,
        ..record
    };
    write_record(at.dirs, &deleting)?;
    let name = claim_ref(ticket);
    // Once more at most: a claim of this runner's at a commit its record
    // missed is deleted at that commit.
    for _ in 0..2 {
        let oid = match push_delete(at.git, &checkout, &name, &expected)? {
            Lifted::Deleted => {
                remove_record(at.dirs, ticket);
                return Ok(Deleted::Deleted);
            }
            Lifted::Moved(None) => {
                remove_record(at.dirs, ticket);
                return Ok(Deleted::NotOurs);
            }
            Lifted::Moved(Some(oid)) => oid,
        };
        match read_remote(at.git, &checkout, ticket)? {
            Some(claim) if claim.runner == deleting.claim.runner => expected = oid,
            _ => {
                remove_record(at.dirs, ticket);
                return Ok(Deleted::NotOurs);
            }
        }
    }
    Err(format!("origin refused the delete of {name} twice"))
}

/// Where a leased delete of a claim left `origin` (OWL-588).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Lifted {
    /// The claim was deleted.
    Deleted,
    /// The delete was refused: `origin` holds another commit than the one
    /// expected, renewed or taken again since, which it keeps; `None` when
    /// the claim is gone.
    Moved(Option<String>),
}

/// Deletes `name` on `origin` from `checkout`, leased on `expected`
/// (live check T1): [`Lifted::Moved`], with what `origin` holds now, when
/// refused. A push that fails otherwise is an error, nothing deleted.
fn push_delete(git: &Git, checkout: &Path, name: &str, expected: &str) -> Result<Lifted, String> {
    let lease = CommitId::new(expected).map_err(|e| e.to_string())?;
    let update = RefUpdate::Delete {
        name,
        expected: &lease,
    };
    match push(git, checkout, &[update], false).remove(0) {
        Ok(_) => Ok(Lifted::Deleted),
        Err(PushError::Failed { message }) => Err(message),
        Err(PushError::Rejected { .. }) => {
            let heads = ls_remote(git, checkout, &[name.to_owned()])?;
            Ok(Lifted::Moved(heads.get(name).cloned()))
        }
    }
}

/// A claim of `origin` as a release reads it (OWL-588).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Found {
    /// The commit read, which the release's delete is leased on.
    pub commit: String,
    /// What it says, or why it does not read.
    pub claim: Result<Claim, ClaimUnread>,
    /// Whether the ticket's branch is on `origin`: what an earlier runner
    /// pushed, which a new take refuses (build plan "Claims", "Taken").
    pub branch: bool,
}

/// The claim `origin` holds of `ticket`, read for a release (OWL-588;
/// build plan "Releasing a claim"): one `ls-remote` of the claim and of the
/// ticket's `branch`, then the claim fetched to [`FETCHED_PREFIX`] and read
/// at the commit the fetch left there. `None` when `origin` holds none.
pub fn read_on_origin(
    git: &Git,
    checkout: &Path,
    ticket: &TicketId,
    branch: &str,
) -> Result<Option<Found>, String> {
    let name = claim_ref(ticket);
    let head = format!("refs/heads/{branch}");
    let heads = ls_remote(git, checkout, &[name.clone(), head.clone()])?;
    if !heads.contains_key(&name) {
        return Ok(None);
    }
    let (commit, claim) = fetch_claim(git, checkout, ticket)?;
    Ok(Some(Found {
        commit,
        claim,
        branch: heads.contains_key(&head),
    }))
}

/// Deletes `ticket`'s claim from `origin` for a release (OWL-588; build
/// plan "Releasing a claim"), leased on `expected`, the commit the release
/// just read: a claim moved since, its holder having renewed it, is kept.
/// Under the claim's lock, as every write of a claim here. A record of this
/// runner's that knows `expected` says a delete is under way before the
/// push, as [`delete`]'s does, so a crash after it is never read as a loss;
/// it goes once the claim is gone, and is restored as it was otherwise. The
/// claim fetched to [`FETCHED_PREFIX`] goes with the claim, so that what
/// this machine shows of `origin`'s claims no longer names it.
pub fn lift(at: ClaimsAt<'_>, ticket: &TicketId, expected: &str) -> Result<Lifted, String> {
    let checkout = at.dirs.checkout();
    let Some(_lock) = lock(at.dirs, ticket, LOCK_WAIT).map_err(|e| e.to_string())? else {
        return Err("another process of this runner holds the claim's lock".to_owned());
    };
    let record = read_record(at.dirs, ticket).filter(|record| {
        record.commit.as_deref() == Some(expected) || record.next.as_deref() == Some(expected)
    });
    if let Some(record) = &record {
        let deleting = TicketClaimed {
            deleting: true,
            ..record.clone()
        };
        write_record(at.dirs, &deleting)?;
    }
    let lifted = push_delete(at.git, &checkout, &claim_ref(ticket), expected);
    match (&lifted, &record) {
        (Ok(Lifted::Deleted | Lifted::Moved(None)), _) => {
            if record.is_some() {
                remove_record(at.dirs, ticket);
            }
            let _ = at.git.run(
                &checkout,
                &["update-ref", "-d", &format!("{FETCHED_PREFIX}{ticket}")],
            );
        }
        (_, Some(record)) => {
            let _ = write_record(at.dirs, record);
        }
        (_, None) => {}
    }
    lifted
}

/// The claims of `origin` as the dedicated checkout at `checkout` last
/// fetched them, read or not, nothing fetched (OWL-588): what `owlshift
/// status`, `owlshift why`, the web UI and `owlshift doctor` show, each
/// claim by its ref under [`FETCHED_PREFIX`]. The checkout must exist.
pub fn last_fetched(git: &Git, checkout: &Path) -> Result<Vec<FetchedClaim>, String> {
    let listing = crate::ticket_ref::listing_with(git, checkout, true)?;
    let listing = crate::ticket_ref::Listing {
        refs: Vec::new(),
        ..listing
    };
    Ok(crate::ticket_ref::read_listing(git, checkout, listing).1)
}

/// What the sweep did for one ticket.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Swept {
    pub ticket: TicketId,
    pub deleted: Result<Deleted, String>,
}

/// Deletes the claims this runner holds of tickets it no longer follows
/// (build plan "Claims", "Kept" and "Taken"): each whose ticket's lock is
/// free, taken with a single try and held across the read and the delete,
/// and whose ref, read under it, `follows` does not keep: gone, a crash
/// having come between the take and the ref's first write, or past its
/// following.
pub fn sweep(at: ClaimsAt<'_>, follows: &dyn Fn(&TicketId) -> Result<bool, String>) -> Vec<Swept> {
    let mut swept = Vec::new();
    for record in recorded(at.dirs) {
        let ticket = record.ticket;
        let Ok(Some(_ticket_lock)) = at.dirs.try_ticket_lock(&ticket) else {
            continue;
        };
        if !matches!(follows(&ticket), Ok(false)) {
            continue;
        }
        let deleted = delete(at, &ticket);
        swept.push(Swept { ticket, deleted });
    }
    swept
}

/// The file of `ticket`'s claim record.
pub fn record_file(dirs: &ProjectDirs, ticket: &TicketId) -> PathBuf {
    dirs.ticket_dir(ticket).join(CLAIMED_FILE)
}

/// The file of `ticket`'s lost claim, whose presence a step under way
/// watches (`crate::cancel::Watcher`).
pub fn lost_file(dirs: &ProjectDirs, ticket: &TicketId) -> PathBuf {
    dirs.ticket_dir(ticket).join(LOST_FILE)
}

/// This runner's claim record of `ticket`; `None` when there is none or it
/// does not read, which counts as none.
pub fn read_record(dirs: &ProjectDirs, ticket: &TicketId) -> Option<TicketClaimed> {
    let text = fs::read_to_string(record_file(dirs, ticket)).ok()?;
    TicketClaimed::parse(&text)
        .ok()
        .filter(|record| record.ticket == *ticket)
}

/// Every claim record of the project, in ticket order.
pub fn recorded(dirs: &ProjectDirs) -> Vec<TicketClaimed> {
    let mut found: Vec<TicketClaimed> =
        in_folders(dirs, CLAIMED_FILE, |text| TicketClaimed::parse(text).ok());
    found.sort_by(|a, b| a.ticket.cmp(&b.ticket));
    found
}

/// The lost claim of `ticket`, when one stands. A record that does not read
/// still stands: what it holds only names the holder.
pub fn lost(dirs: &ProjectDirs, ticket: &TicketId) -> Option<Option<TicketClaimLost>> {
    let path = lost_file(dirs, ticket);
    fs::symlink_metadata(&path).ok()?;
    Some(
        fs::read_to_string(&path)
            .ok()
            .and_then(|text| TicketClaimLost::parse(&text).ok()),
    )
}

/// The tickets of the project whose claim this runner lost, by their
/// records.
pub fn lost_tickets(dirs: &ProjectDirs) -> BTreeSet<TicketId> {
    in_folders(dirs, LOST_FILE, |text| TicketClaimLost::parse(text).ok())
        .into_iter()
        .map(|record| record.ticket)
        .collect()
}

/// `refs` without the tickets whose claim this runner lost: their local
/// state is out of every count (build plan "Claims", "Lost").
pub fn drop_lost<T>(dirs: &ProjectDirs, refs: Vec<(TicketId, T)>) -> Vec<(TicketId, T)> {
    let lost = lost_tickets(dirs);
    if lost.is_empty() {
        return refs;
    }
    refs.into_iter()
        .filter(|(ticket, _)| !lost.contains(ticket))
        .collect()
}

/// Records that this runner lost the claim of `ticket`, held by `holder`,
/// found at `at`: `Ok(true)` the first time, written whole; `Ok(false)` when
/// a record stood already, so the operator is told once.
pub fn record_lost(
    dirs: &ProjectDirs,
    ticket: &TicketId,
    holder: Option<ClaimHolder>,
    at: Timestamp,
) -> io::Result<bool> {
    let record = TicketClaimLost {
        format: Format,
        ticket: ticket.clone(),
        at,
        holder,
    };
    let path = lost_file(dirs, ticket);
    fs::create_dir_all(dirs.ticket_dir(ticket))?;
    let partial = path.with_extension("json.partial");
    fs::write(&partial, record.render())?;
    let linked = fs::hard_link(&partial, &path);
    let _ = fs::remove_file(&partial);
    match linked {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(false),
        Err(error) => Err(error),
    }
}

/// Forgets what this runner keeps of `ticket`'s claim, for `owlshift
/// forget`: its claim deleted from `origin` when it is still this runner's,
/// then its records removed, the lost one included. The delete's error is
/// the caller's to warn of; the records go whatever it said.
pub fn forget(at: ClaimsAt<'_>, ticket: &TicketId) -> Result<Deleted, String> {
    let deleted = delete(at, ticket);
    remove_record(at.dirs, ticket);
    let _ = fs::remove_file(lost_file(at.dirs, ticket));
    deleted
}

fn in_folders<T>(dirs: &ProjectDirs, file: &str, parse: impl Fn(&str) -> Option<T>) -> Vec<T> {
    let Ok(folders) = project::subdirectories(&dirs.tickets()) else {
        return Vec::new();
    };
    folders
        .iter()
        .filter_map(|folder| fs::read_to_string(folder.join(file)).ok())
        .filter_map(|text| parse(&text))
        .collect()
}

fn write_record(dirs: &ProjectDirs, record: &TicketClaimed) -> Result<(), String> {
    let path = record_file(dirs, &record.ticket);
    write_atomically(&path, &record.render()).map_err(|e| format!("{}: {e}", path.display()))
}

fn remove_record(dirs: &ProjectDirs, ticket: &TicketId) {
    let _ = fs::remove_file(record_file(dirs, ticket));
}

fn adopt(dirs: &ProjectDirs, ticket: &TicketId, commit: &str, claim: Claim) -> Result<(), String> {
    write_record(
        dirs,
        &TicketClaimed {
            format: Format,
            ticket: ticket.clone(),
            commit: Some(commit.to_owned()),
            next: None,
            deleting: false,
            claim,
        },
    )
}

/// The claim's lock of `ticket`, waited for `wait` at most.
fn lock(dirs: &ProjectDirs, ticket: &TicketId, wait: Duration) -> io::Result<Option<fs::File>> {
    fs::create_dir_all(dirs.ticket_dir(ticket))?;
    project::lock_file_within(&dirs.ticket_dir(ticket).join(LOCK_FILE), wait, LOCK_POLL)
}

/// The commit of a claim: a tree holding `claim.json` alone, no parent,
/// made in `checkout` and moving no ref there.
fn make_commit(
    git: &Git,
    checkout: &Path,
    ticket: &TicketId,
    claim: &Claim,
) -> Result<CommitId, String> {
    let message = format!("{ticket}: claimed by runner {}", claim.runner);
    commit_file(
        git,
        checkout,
        CLAIM_FILE,
        claim.render().as_bytes(),
        &message,
    )
}

/// A commit without a parent whose tree holds `file` alone, with `bytes`,
/// made in `checkout` and moving no ref there: a claim's, a ticket's or a
/// break's repair's (OWL-642).
fn commit_file(
    git: &Git,
    checkout: &Path,
    file: &str,
    bytes: &[u8],
    message: &str,
) -> Result<CommitId, String> {
    let blob = crate::ticket_ref::blob(git, checkout, bytes)?;
    let tree = crate::ticket_ref::with_input(
        git,
        checkout,
        &["mktree", "-z"],
        format!("100644 blob {blob}\t{file}\0").as_bytes(),
    )?;
    let commit = git
        .run(
            checkout,
            &[
                "-c",
                "user.name=Owlshift",
                "-c",
                "user.email=owlshift@localhost",
                "commit-tree",
                "--no-gpg-sign",
                &tree,
                "-m",
                message,
            ],
        )
        .map_err(|e| e.to_string())?;
    CommitId::new(String::from_utf8_lossy(&commit).trim()).map_err(|e| e.to_string())
}

/// What `origin` holds at each of `names`, read with `git ls-remote`: a ref
/// absent there is absent from the map.
fn ls_remote(
    git: &Git,
    checkout: &Path,
    names: &[String],
) -> Result<BTreeMap<String, String>, String> {
    let mut args = vec!["ls-remote", REMOTE];
    args.extend(names.iter().map(String::as_str));
    let out = git
        .run(checkout, &args)
        .map_err(|e| format!("reading {REMOTE}'s claims: {e}"))?;
    Ok(String::from_utf8_lossy(&out)
        .lines()
        .filter_map(|line| {
            let (oid, name) = line.trim_end_matches('\r').split_once('\t')?;
            names
                .iter()
                .any(|wanted| wanted == name)
                .then(|| (name.to_owned(), oid.to_owned()))
        })
        .collect())
}

/// The claim `origin` holds of `ticket`, fetched to
/// `refs/remotes/owlshift/claims/<ticket>` and read: `None` when it is gone
/// or does not read as a claim, past [`MAX_CLAIM_BYTES`] included.
fn read_remote(git: &Git, checkout: &Path, ticket: &TicketId) -> Result<Option<Claim>, String> {
    fetch_claim(git, checkout, ticket).map(|(_, claim)| claim.ok())
}

/// Fetches `ticket`'s claim from `origin` to
/// `refs/remotes/owlshift/claims/<ticket>` and reads it at the commit the
/// fetch left there: that commit, and the claim or why it does not read
/// (past [`MAX_CLAIM_BYTES`], or of a later format). A fetch that failed,
/// the network or the claim gone since it was listed, tells nothing: an
/// error, never read as another's claim, which would lose this runner's.
fn fetch_claim(
    git: &Git,
    checkout: &Path,
    ticket: &TicketId,
) -> Result<(String, Result<Claim, ClaimUnread>), String> {
    let fetched = format!("{FETCHED_PREFIX}{ticket}");
    let (commit, text) = fetch_file(
        git,
        checkout,
        (&claim_ref(ticket), &fetched),
        CLAIM_FILE,
        &format!("{ticket}'s claim"),
    )?;
    let claim = text.and_then(|text| Claim::parse(&text).map_err(unread_claim));
    Ok((commit, claim))
}

/// Why a claim's text does not read as its contract: a later format among
/// the reasons, which an upgrade reads.
fn unread_claim(error: ContractError) -> ClaimUnread {
    ClaimUnread {
        newer: matches!(error, ContractError::NewerFormat { .. }),
        reason: error.to_string(),
    }
}

/// Fetches `remote` of `origin` to `local`, a ref under `refs/remotes/`,
/// and reads `file` in the commit the fetch left there: that commit, and
/// the file's text or why it holds none to read (past [`MAX_CLAIM_BYTES`]
/// among the reasons). `what` names the claim in an error. A fetch that
/// failed tells nothing: an error.
fn fetch_file(
    git: &Git,
    checkout: &Path,
    (remote, local): (&str, &str),
    file: &str,
    what: &str,
) -> Result<(String, Result<String, ClaimUnread>), String> {
    let refspec = format!("+{remote}:{local}");
    let out = git
        .output(
            checkout,
            &[
                "fetch",
                "--quiet",
                "--no-tags",
                "--no-write-fetch-head",
                REMOTE,
                &refspec,
            ],
            None,
        )
        .map_err(|e| format!("reading {what} on {REMOTE}: {e}"))?;
    if !out.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let last = stderr.lines().rev().find(|line| !line.trim().is_empty());
        return Err(format!(
            "reading {what} on {REMOTE}: git fetch failed: {}",
            last.unwrap_or("no error output").trim()
        ));
    }
    let commit = git
        .run(
            checkout,
            &["rev-parse", "--verify", &format!("{local}^{{commit}}")],
        )
        .map_err(|e| format!("reading {what} on {REMOTE}: {e}"))?;
    let commit = String::from_utf8_lossy(&commit).trim().to_owned();
    let path = format!("{commit}:{file}");
    let unread = |reason: String| ClaimUnread {
        reason,
        newer: false,
    };
    let size = git
        .run(checkout, &["cat-file", "-s", &path])
        .ok()
        .and_then(|size| String::from_utf8_lossy(&size).trim().parse::<usize>().ok());
    let text = match size {
        None => Err(unread(format!("its commit holds no {file}"))),
        Some(size) if size > MAX_CLAIM_BYTES => Err(unread(format!(
            "its {file} holds {size} bytes, past the {MAX_CLAIM_BYTES} a claim may hold"
        ))),
        Some(_) => {
            let text = git
                .run(checkout, &["cat-file", "blob", &path])
                .map_err(|e| e.to_string())?;
            Ok(String::from_utf8_lossy(&text).into_owned())
        }
    };
    Ok((commit, text))
}

/// Who holds the claim of `ticket` on `origin` now, when it reads.
fn read_holder(git: &Git, checkout: &Path, ticket: &TicketId) -> Option<ClaimHolder> {
    read_remote(git, checkout, ticket)
        .ok()
        .flatten()
        .map(|claim| claim.holder())
}

/// Pushes `updates` from `checkout` to `origin`, hooks skipped, as the
/// Writer pushes a branch: one verdict per update, in order. The floor's
/// check comes first: pushing Owlshift's own refs is the runner's.
fn push(
    git: &Git,
    checkout: &Path,
    updates: &[RefUpdate<'_>],
    atomic: bool,
) -> Vec<Result<RefPushed, PushError>> {
    let all = |error: PushError| vec![Err(error); updates.len()];
    if let Err(violation) =
        floor::check_from(Origin::Runner, Action::PushRef, HumanApproval::Absent)
    {
        return all(PushError::Failed {
            message: format!("{violation:?}"),
        });
    }
    let command = match ref_push_command(Path::new("git"), checkout, REMOTE, updates, atomic) {
        Ok(command) => command,
        Err(error) => return all(error),
    };
    let mut args: Vec<std::ffi::OsString> = Vec::new();
    let mut given = command.get_args();
    args.extend(given.next().map(std::ffi::OsStr::to_owned));
    args.push("--no-verify".into());
    args.extend(given.map(std::ffi::OsStr::to_owned));
    let output = match git.output(checkout, &args, None) {
        Ok(output) => output,
        Err(error) => {
            return all(PushError::Failed {
                message: error.to_string(),
            });
        }
    };
    let ended = match output.code {
        Some(code) => format!("exit status: {code}"),
        None => "a signal".to_owned(),
    };
    read_ref_push_parts(
        updates,
        output.success(),
        &ended,
        &output.stdout,
        &output.stderr,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_runner_id_is_made_once_and_read_back() {
        let data = tempfile::tempdir().unwrap();
        let made = runner_id(data.path()).unwrap();
        assert_eq!(made.as_str().len(), 16);
        assert_eq!(runner_id(data.path()).unwrap(), made);
        assert!(RunnerId::parse("ada-laptop").is_none());
        fs::write(data.path().join(RUNNER_FILE), "not hex\n").unwrap();
        assert!(runner_id(data.path()).is_err());
    }
}
