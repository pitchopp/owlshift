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
//!
//! Under `policy.overlap = "rebase"`, an overlap of one of this runner's
//! tickets with another runner's ticket admitted after it is recorded by
//! that runner alone, in a ticket ref this one never reads. Each pass weighs
//! this runner's tickets against the claims, by what each publishes it
//! holds, and records such an overlap in its own ticket's record (OWL-636;
//! build plan "Other runners' tickets", "Overlaps"), which keeps counting it
//! once that claim is deleted after its merge (OWL-613).

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::Write;
use std::time::SystemTime;

use jiff::Timestamp;
use serde_json::json;

use owlshift_contracts::config::Claims;
use owlshift_contracts::event::EventKind;
use owlshift_contracts::ids::TicketId;
use owlshift_contracts::refs::TicketHeld;
use owlshift_core::collision::{self, InFlight, Shared};
use owlshift_core::resource::NamedResources;
use owlshift_core::vocab::Overlap;

use super::{Memory, Watch};
use crate::claims::{self, FetchedClaim};
use crate::events::{EventSink, data};
use crate::held;
use crate::in_flight;
use crate::project::{self, DefaultBranchFile};
use crate::ticket_ref::{self, ProposalsFile, RefRead, Stored};

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

    /// Records the overlaps of this runner's tickets with other runners'
    /// tickets that only those runners recorded (OWL-636; build plan "Other
    /// runners' tickets", "Overlaps"): under `rebase` and remote claims, each
    /// ticket ref of `read` whose step's child does not run is weighed
    /// against the claims of other runners that read ([`claims_overlaps`]).
    /// An overlap its record lacks is written under the project's and the
    /// ticket's lock while its ref is still the one read
    /// ([`Self::locked_at`]), decided again from the record read there, with
    /// a `decision` event (`overlap` = `recorded`, `by` = `claim`); the ref
    /// read again then replaces it in `read`, so the pass's collisions and
    /// refreshes read it. A project busy or a ref moved leaves it to the next
    /// pass; a write that fails is warned of once.
    pub(super) fn record_claims_overlaps(
        &self,
        memory: &mut Memory,
        sink: &mut EventSink<'_>,
        read: &mut [RefRead],
        overlap: Overlap,
    ) {
        if overlap != Overlap::Rebase || !memory.claimed.remote {
            return;
        }
        let on_demand = self.on_demand;
        let named = on_demand.config.stack.named_resources().unwrap_or_default();
        // Other runners' tickets alone, by their claims. Pull requests and
        // people's tickets are never an overlap's sibling, and could only
        // hold the ticket back, which hides its overlaps.
        let refs = read
            .iter()
            .map(|(ticket, stored)| (ticket, stored.as_ref().ok()));
        let colleagues = in_flight::Outside::default()
            .holders(refs, &memory.claimed.elsewhere, &named)
            .others;
        if colleagues.is_empty() {
            return;
        }
        let times = claims::claimed_times(on_demand.dirs);
        let now = (on_demand.clock)();
        let overlaps_of = |ticket: &TicketId, stored: &Stored| {
            claims_overlaps(ticket, stored, &colleagues, &times, &named)
        };
        let running: Vec<TicketId> = memory
            .running
            .iter()
            .map(|running| running.ticket().clone())
            .collect();
        for (ticket, stored) in read.iter_mut() {
            let seen = match stored.as_ref() {
                Ok(found) if !running.contains(ticket) => {
                    // Weighed on a copy first: a pass that finds nothing new
                    // takes no lock.
                    let overlaps = overlaps_of(ticket, found);
                    let new = found.record.held.clone().is_some_and(|mut held| {
                        !held::record(&mut held, &overlaps, now).kept.is_empty()
                    });
                    if !new {
                        continue;
                    }
                    found.commit.clone()
                }
                _ => continue,
            };
            match self.write_claims_overlaps(sink, ticket, &seen, &overlaps_of, now) {
                Ok(Some(written)) => *stored = Ok(written),
                Ok(None) => {}
                Err(reason) => self.unreadable(memory, sink, ticket, reason),
            }
        }
    }

    /// [`Self::record_claims_overlaps`] for `ticket`, its ref read at
    /// `seen`, its overlaps found by `overlaps_of`: the ref as written,
    /// `None` when nothing was.
    fn write_claims_overlaps(
        &self,
        sink: &mut EventSink<'_>,
        ticket: &TicketId,
        seen: &str,
        overlaps_of: &dyn Fn(&TicketId, &Stored) -> Vec<Shared<String>>,
        now: Timestamp,
    ) -> Result<Option<Stored>, String> {
        let Some((_project, ticket_lock)) = self.locked_at(ticket, seen)? else {
            return Ok(None);
        };
        let git = &self.on_demand.executor.git;
        let checkout = self.on_demand.dirs.checkout();
        let Some(current) = ticket_ref::read(git, &checkout, ticket)? else {
            return Ok(None);
        };
        let overlaps = overlaps_of(ticket, &current);
        let mut record = current.record;
        let Some(held) = record.held.as_mut() else {
            return Ok(None);
        };
        let recorded = held::record(held, &overlaps, now);
        if recorded.kept.is_empty() {
            return Ok(None);
        }
        self.write_ref(&ticket_lock, ticket, &record, ProposalsFile::Keep, seen)?;
        for kept in &recorded.kept {
            sink.emit(
                ticket,
                None,
                EventKind::Decision,
                held::overlap_event("claim", kept),
            );
        }
        ticket_ref::read(git, &checkout, ticket)
    }
}

/// The overlaps `ticket`'s ref `stored` has with `colleagues`, other
/// runners' tickets by their claims (OWL-636): none when it does not hold
/// ([`held::holds`]), keeps no record of what it holds, or yielded it, its
/// resume reading its own set (OWL-621); else those [`collision::weigh`]
/// tells it under `rebase`, as a widening weighs it, at its own claim's time
/// when `times`, this runner's claims, has one ([`held::by_claim`]), as
/// watch's collisions order it. None either while a colleague holds it
/// back: its next step waits, and a later pass records them.
fn claims_overlaps(
    ticket: &TicketId,
    stored: &Stored,
    colleagues: &[InFlight<String>],
    times: &BTreeMap<TicketId, i64>,
    named: &NamedResources,
) -> Vec<Shared<String>> {
    let yielded = stored.record.held.as_ref().is_none_or(TicketHeld::yielded);
    if yielded || !held::holds(stored) {
        return Vec::new();
    }
    let own = held::by_claim(held::in_flight(ticket, stored, named), times);
    collision::weigh(&own, colleagues, Overlap::Rebase).overlaps
}

#[cfg(test)]
mod tests {
    use owlshift_contracts::refs::{PersistedState, TicketDelivery};
    use owlshift_core::collision::Dispatched;
    use owlshift_core::resource::Resource;
    use owlshift_core::state::{Status, TicketState};
    use owlshift_core::vocab::Stage;

    use super::*;
    use crate::ticket_ref::blank;

    /// DEMO-1's ref at `stage`, its pull request open when `delivered`,
    /// dispatched two minutes after the epoch and holding `app/one.rs` since
    /// its Build's admission.
    fn admitted(stage: Stage, delivered: bool) -> Stored {
        let round = u32::from(stage != Stage::Ready);
        let state = TicketState::restore(Status::Active(stage), round, 0, 0, 0, 0).unwrap();
        let mut record = blank(None);
        record.state = PersistedState::from(&state);
        record.delivery = delivered.then(|| {
            TicketDelivery::parse(include_str!(
                "../../../owlshift-contracts/tests/fixtures/ticket-delivery.json"
            ))
            .unwrap()
        });
        let at = Timestamp::from_millisecond(120_000).unwrap();
        let mut held = TicketHeld::dispatched(at);
        held.admit(vec!["app/one.rs".to_owned()], Vec::new(), None, at);
        record.held = Some(held);
        Stored {
            record,
            commit: "c".repeat(40),
            proposals: None,
        }
    }

    /// Another runner's ticket, claimed a minute after the epoch, holding
    /// `held`.
    fn colleague(held: Vec<Resource>) -> InFlight<String> {
        InFlight {
            id: "DEMO-2".to_owned(),
            dispatched: Dispatched::At(60_000),
            delivered: false,
            outside: false,
            yielded: false,
            held,
        }
    }

    fn shared(path: &str) -> Shared<String> {
        Shared {
            ticket: "DEMO-2".to_owned(),
            paths: vec![path.to_owned()],
            resources: Vec::new(),
            yielded: false,
        }
    }

    /// OWL-636: a ticket followed here overlaps a colleague's ticket whose
    /// claim holds its file; not once it yielded, nor once it no longer
    /// holds. A ticket not delivered is weighed at its own claim's time: one
    /// claimed before the colleague's goes on, and is told the overlap,
    /// where its dispatch time, after that claim, has it wait for the named
    /// resource both hold.
    #[test]
    fn a_ticket_here_overlaps_a_colleagues_claim_while_it_holds() {
        let demo1 = TicketId::new("DEMO-1").unwrap();
        let file = colleague(vec![Resource::Zone("app/one.rs".to_owned())]);
        let none = NamedResources::default();
        let (unclaimed, claimed) = (BTreeMap::new(), BTreeMap::from([(demo1.clone(), 0)]));
        let found = |stored: &Stored, against: &InFlight<String>, times, named| {
            claims_overlaps(&demo1, stored, std::slice::from_ref(against), times, named)
        };
        let followed = admitted(Stage::Watch, true);
        assert_eq!(
            found(&followed, &file, &unclaimed, &none),
            [shared("app/one.rs")]
        );
        let mut yielded = followed;
        let at = Timestamp::UNIX_EPOCH;
        assert!(yielded.record.held.as_mut().unwrap().yield_at(at));
        assert!(found(&yielded, &file, &unclaimed, &none).is_empty());
        let ready = admitted(Stage::Ready, true);
        assert!(found(&ready, &file, &unclaimed, &none).is_empty());

        let named = NamedResources::new([("migrations", "app/**")]).unwrap();
        let both = colleague(vec![
            Resource::Zone("app/one.rs".to_owned()),
            Resource::Named("migrations".to_owned()),
        ]);
        let verifying = admitted(Stage::Verify, false);
        assert!(found(&verifying, &both, &unclaimed, &named).is_empty());
        assert_eq!(
            found(&verifying, &both, &claimed, &named),
            [shared("app/one.rs")]
        );
    }
}
