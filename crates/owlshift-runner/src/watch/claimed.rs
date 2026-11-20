//! Other runners' claims at watch's passes (P10, OWL-587; build plan "P10 ·
//! Teams", "Other runners' tickets"): under `policy.claims = "remote"`, read
//! in the project file at `origin/HEAD` as last fetched, watch fetches the
//! claims of `origin` on its own at most once a minute
//! ([`claims::FETCH_EVERY`]), in a section of its own under a single try of
//! the project's lock, as a served watch fetches its default branch; none
//! while a quarantine marker stands. Each snapshot reads them with the
//! ticket refs ([`crate::ticket_ref::read_listing`]) and keeps this runner's
//! apart from the others'.
//!
//! After a pass whose fetch succeeded, this runner's claims are checked
//! against it ([`claims::check_fetched`]): one found lost is told as at a
//! renewal. A claim that does not read is one `warning` (`claim_unread`) and
//! one line per ticket, told again when its commit changes. A fetch that
//! fails is one `warning` (`claims_unfetched`) and one line, told again when
//! its reason changes.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::Write;
use std::time::SystemTime;

use jiff::Timestamp;
use serde_json::json;

use owlshift_contracts::config::Claims;
use owlshift_contracts::event::EventKind;
use owlshift_contracts::ids::TicketId;

use super::{Memory, Watch};
use crate::claims::{self, FetchedClaim};
use crate::events::{EventSink, data};
use crate::project::{self, DefaultBranchFile};

/// What watch keeps of the claims of `origin`.
#[derive(Debug, Default)]
pub(super) struct ClaimedMemory {
    /// Whether the project claims its tickets on the remote at this pass.
    pub(super) remote: bool,
    /// When watch last tried its own fetch of the claims.
    fetched_at: Option<Timestamp>,
    /// Why the last fetch failed, as told.
    failing: Option<String>,
    /// When this pass's fetch, which succeeded, started: the claims the
    /// snapshot reads after it are checked against this runner's.
    pub(super) fresh: Option<SystemTime>,
    /// This runner's claims as the last snapshot read them, by commit.
    pub(super) ours: BTreeMap<TicketId, String>,
    /// Other runners' claims as the last snapshot read them, read or not.
    pub(super) elsewhere: Vec<FetchedClaim>,
    /// The claims warned of as unreadable, with the commit told.
    unread: HashMap<TicketId, String>,
    /// Why the last check of this runner's claims could not be made, as
    /// told.
    check_failing: Option<String>,
}

impl ClaimedMemory {
    /// The claim another runner holds of `ticket`, read or not.
    pub(super) fn of(&self, ticket: &TicketId) -> Option<&FetchedClaim> {
        self.elsewhere.iter().find(|claim| claim.ticket == *ticket)
    }

    /// The tickets other runners claim.
    pub(super) fn tickets(&self) -> Vec<TicketId> {
        self.elsewhere
            .iter()
            .map(|claim| claim.ticket.clone())
            .collect()
    }
}

impl Watch<'_> {
    /// Whether the project claims its tickets on the remote, as the project
    /// file at `origin/HEAD` reads as the dedicated checkout last fetched
    /// it: `local` when none reads.
    pub(super) fn claims_switch(&self) -> bool {
        let on_demand = self.on_demand;
        let checkout = on_demand.dirs.checkout();
        if fs::symlink_metadata(&checkout).is_err() {
            return false;
        }
        matches!(
            project::project_file_at_origin_head(&on_demand.executor.git, &checkout),
            Ok(DefaultBranchFile::Read { file: Some(file), .. })
                if file.policy.claims() == Claims::Remote
        )
    }

    /// Watch's own fetch of the claims, when the project claims on the
    /// remote and the last try is a minute old: a single try of the
    /// project's lock, none while a quarantine marker stands. A busy project
    /// is tried again at the next pass.
    pub(super) fn fetch_claims(
        &self,
        memory: &mut Memory,
        sink: &mut EventSink<'_>,
        out: &mut dyn Write,
    ) {
        memory.claimed.fresh = None;
        if !memory.claimed.remote {
            return;
        }
        let on_demand = self.on_demand;
        let now = (on_demand.clock)();
        let due = memory.claimed.fetched_at.is_none_or(|at| {
            at.checked_add(claims::FETCH_EVERY)
                .is_ok_and(|next| now >= next)
        });
        let dirs = on_demand.dirs;
        let checkout = dirs.checkout();
        if !due
            || fs::symlink_metadata(&checkout).is_err()
            || fs::symlink_metadata(dirs.quarantine_file()).is_ok()
        {
            return;
        }
        let Ok(Some(lock)) = dirs.try_lock() else {
            return;
        };
        memory.claimed.fetched_at = Some(now);
        let since = SystemTime::now();
        let fetched = claims::fetch(&on_demand.executor.git, &checkout);
        drop(lock);
        match fetched {
            Ok(()) => {
                memory.claimed.fresh = Some(since);
                if memory.claimed.failing.take().is_some() {
                    self.line(out, "the claims of origin are fetched again");
                }
            }
            Err(why) => {
                if memory.claimed.failing.as_ref() != Some(&why) {
                    self.line(
                        out,
                        &format!(
                            "the claims of origin could not be fetched, the last fetched read \
                             instead: {why}"
                        ),
                    );
                    sink.emit_project(
                        None,
                        EventKind::Warning,
                        data([("what", json!("claims_unfetched")), ("reason", json!(why))]),
                    );
                }
                memory.claimed.failing = Some(why);
            }
        }
    }

    /// Keeps the claims a snapshot read: this runner's by commit, the
    /// others' as they read.
    pub(super) fn keep_claims(&self, memory: &mut Memory, fetched: Vec<FetchedClaim>) {
        memory.claimed.ours.clear();
        memory.claimed.elsewhere.clear();
        if fetched.is_empty() {
            return;
        }
        let Ok(runner) = claims::runner_id(self.on_demand.dirs.data_dir()) else {
            // No identifier to tell this runner's: every claim counts as
            // another's, the safe side for the collisions.
            memory.claimed.elsewhere = fetched;
            return;
        };
        for claim in fetched {
            if claim.is_of(&runner) {
                memory.claimed.ours.insert(claim.ticket, claim.commit);
            } else {
                memory.claimed.elsewhere.push(claim);
            }
        }
    }

    /// After a pass whose fetch succeeded, this runner's claims checked
    /// against it ([`claims::check_fetched`]): one lost is told as at a
    /// renewal, and its ticket leaves the pass.
    pub(super) fn check_claims(
        &self,
        memory: &mut Memory,
        sink: &mut EventSink<'_>,
        out: &mut dyn Write,
        at: claims::ClaimsAt<'_>,
    ) {
        let Some(since) = memory.claimed.fresh.take() else {
            return;
        };
        let checked = claims::check_fetched(at, &memory.claimed.ours, since);
        for (ticket, holder) in checked.lost {
            self.line(
                out,
                &format!(
                    "{ticket}: its claim on origin was lost; this runner writes nothing more on \
                     it, and `owlshift forget {ticket}` starts it over here"
                ),
            );
            self.on_demand.claim_lost(&ticket, holder, sink);
        }
        match &checked.failed {
            Some(why) if memory.claimed.check_failing.as_ref() != Some(why) => {
                self.line(
                    out,
                    &format!("this runner's claims could not be checked against origin: {why}"),
                );
            }
            _ => {}
        }
        memory.claimed.check_failing = checked.failed;
    }

    /// Each claim another runner pushed that this binary cannot read, told
    /// once per ticket and commit: one `warning` (`claim_unread`) and one
    /// line (build plan "Other runners' tickets", "A claim this binary
    /// cannot read").
    pub(super) fn warn_unread_claims(
        &self,
        memory: &mut Memory,
        sink: &mut EventSink<'_>,
        out: &mut dyn Write,
    ) {
        let mut unread = HashMap::new();
        for claim in &memory.claimed.elsewhere {
            let Err(why) = &claim.claim else {
                continue;
            };
            let ticket = &claim.ticket;
            unread.insert(ticket.clone(), claim.commit.clone());
            if memory.claimed.unread.get(ticket) == Some(&claim.commit) {
                continue;
            }
            sink.emit(
                ticket,
                None,
                EventKind::Warning,
                data([
                    ("what", json!("claim_unread")),
                    ("reason", json!(why.reason)),
                    ("newer", json!(why.newer)),
                ]),
            );
            let upgrade = if why.newer {
                "; upgrade Owlshift to read it"
            } else {
                ""
            };
            self.line(
                out,
                &format!(
                    "{ticket} is claimed on origin by a claim this runner cannot read ({}): it is \
                     never taken here, and holds nothing it can name{upgrade}",
                    why.reason
                ),
            );
        }
        memory.claimed.unread = unread;
    }
}
