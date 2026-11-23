//! A break's repair claim on the git remote (OWL-642; architecture section
//! 5, "The repair ticket", "Not twice"): under `[policy] claims = "remote"`,
//! which runner files the repair of a broken base and which repair it
//! filed, so that runners whose tracker does not show a filing yet file one
//! repair for one break.
//!
//! The claim is `refs/owlshift/repairs/<key>` on `origin`, `key` naming the
//! failing step and command ([`crate::base::repair_key`]): a commit without
//! a parent whose tree holds `repair.json` ([`RepairClaim`]), made in the
//! dedicated checkout and pushed by its hash as a ticket's claim is, every
//! push leased on what `origin` must hold (live checks C3 and T1): a take
//! only while the ref is absent, a replacement, a naming and a giving back
//! only while it holds the commit read or pushed. It is read through
//! `refs/remotes/owlshift/repairs/<key>`, which the isolation check leaves
//! out. No record is kept here: the filing runs under the project's lock,
//! and the claim names the runner that took it.

use owlshift_adapters::forge::CommitId;
use owlshift_adapters::forge::push::{PushError, RefUpdate};
use owlshift_contracts::refs::{REPAIR_CLAIM_FILE, RepairClaim, repair_claim_ref};

use super::{ClaimUnread, ClaimsAt, commit_file, fetch_file, ls_remote, push, unread_claim};

/// Where a break's repair claim read from `origin` is fetched to: a
/// remote-tracking ref, which the isolation check leaves out.
pub const FETCHED_PREFIX: &str = "refs/remotes/owlshift/repairs/";

/// A break's repair claim as `origin` holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Read {
    /// The commit read, which a replacement is leased on.
    pub commit: String,
    /// What it says, or why it does not read.
    pub claim: Result<RepairClaim, ClaimUnread>,
}

/// The repair claim `key` on `origin`: one `ls-remote`, then the claim
/// fetched and read. `None` when `origin` holds none; an error when either
/// read failed, which tells nothing.
pub fn read(at: ClaimsAt<'_>, key: &str) -> Result<Option<Read>, String> {
    let checkout = at.dirs.checkout();
    let name = repair_claim_ref(key);
    let heads = ls_remote(at.git, &checkout, std::slice::from_ref(&name))?;
    if !heads.contains_key(&name) {
        return Ok(None);
    }
    let fetched = format!("{FETCHED_PREFIX}{key}");
    let (commit, text) = fetch_file(
        at.git,
        &checkout,
        (&name, &fetched),
        REPAIR_CLAIM_FILE,
        &format!("the repair claim {key}"),
    )?;
    let claim = text.and_then(|text| RepairClaim::parse(&text).map_err(unread_claim));
    Ok(Some(Read { commit, claim }))
}

/// What a push of a repair claim did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Pushed {
    /// `origin` now holds this commit.
    Landed(CommitId),
    /// `origin` refused it: a claim stands where none was expected, or
    /// another commit than the one expected.
    Refused,
}

/// Pushes `claim` as the repair claim `key`: created when `expected` is
/// `None`, which `origin` refuses while a claim stands; otherwise in place
/// of the commit `expected`, which it refuses once another landed. A push
/// that gets no verdict is an error.
pub fn push_claim(
    at: ClaimsAt<'_>,
    key: &str,
    claim: &RepairClaim,
    expected: Option<&str>,
) -> Result<Pushed, String> {
    claim.check()?;
    let checkout = at.dirs.checkout();
    let message = format!("{key}: repair claimed by runner {}", claim.runner);
    let commit = commit_file(
        at.git,
        &checkout,
        REPAIR_CLAIM_FILE,
        claim.render().as_bytes(),
        &message,
    )?;
    let name = repair_claim_ref(key);
    let expected = expected
        .map(CommitId::new)
        .transpose()
        .map_err(|e| e.to_string())?;
    let update = match &expected {
        None => RefUpdate::Create {
            name: &name,
            commit: &commit,
        },
        Some(expected) => RefUpdate::Update {
            name: &name,
            commit: &commit,
            expected,
        },
    };
    match push(at.git, &checkout, &[update], false).remove(0) {
        Ok(_) => Ok(Pushed::Landed(commit)),
        Err(PushError::Rejected { .. }) => Ok(Pushed::Refused),
        Err(PushError::Failed { message }) => Err(message),
    }
}

/// Puts the repair claim `key` back as it was before this runner's commit
/// `ours` landed, leased on it: deleted when `origin` held none, the commit
/// `previous` otherwise, which `origin` still has. What a filing the tracker
/// refused leaves, so that another runner may file at once.
pub fn give_back(
    at: ClaimsAt<'_>,
    key: &str,
    ours: &CommitId,
    previous: Option<&str>,
) -> Result<(), String> {
    let checkout = at.dirs.checkout();
    let name = repair_claim_ref(key);
    let previous = previous
        .map(CommitId::new)
        .transpose()
        .map_err(|e| e.to_string())?;
    let update = match &previous {
        None => RefUpdate::Delete {
            name: &name,
            expected: ours,
        },
        Some(previous) => RefUpdate::Update {
            name: &name,
            commit: previous,
            expected: ours,
        },
    };
    push(at.git, &checkout, &[update], false)
        .remove(0)
        .map(|_| ())
        .map_err(|error| error.to_string())
}
