//! What an Owlshift ticket holds against the project's other tickets
//! (OWL-403; architecture section 5, "Parallel", "What a ticket holds" and
//! "The in-flight set", 1): which ticket refs hold, what each holds as the
//! core's collision rule weighs it ([`owlshift_core::collision`]), what a
//! step into Build admits (OWL-448, "Resources admitted at Build"), and the
//! files a ticket's branch changed, which widen its `held.json`.
//!
//! A ticket is in flight from its dispatch until its pull request ends,
//! waiting and parked included, and holds nothing before its Build's
//! admission. A ref with no `held.json`, dispatched before it was kept,
//! holds the whole repository and comes before every other dispatch; so
//! does a ref that cannot be read, since nothing tells what it holds.

use std::path::Path;

use jiff::Timestamp;
use serde_json::{Value, json};

use owlshift_contracts::drain::TicketIntake;
use owlshift_contracts::ids::TicketId;
use owlshift_contracts::refs::{EndedWhy, MAX_HELD_FILES, TicketHeld};
use owlshift_contracts::review::Plan;
use owlshift_core::collision::{self, Dispatched, InFlight, Shared};
use owlshift_core::decider::brief_zones;
use owlshift_core::pipeline::Pipeline;
use owlshift_core::resource::{self, Held, NamedResources, Resource};
use owlshift_core::state::TicketState;
use owlshift_core::vocab::{Stage, Variant};

use crate::events::{Data, data};
use crate::executor::Git;
use crate::on_demand::{delivery_followed, plan_sha256};
use crate::routing::{route_labels, ticket_variant};
use crate::ticket_ref::{RefRead, Stored, TicketRecord};

/// Whether the ticket ref `stored` is in flight, holding what its ticket
/// holds: at a stage after Ready and before Watch, working, waiting or
/// parked there, or at Watch while its pull request is followed. A state
/// that does not read holds, since nothing tells it does not. A ticket
/// whose Build has not been admitted is in flight all the same, holding
/// nothing (OWL-448): it is Owlshift's, never a person's working ticket.
pub fn holds(stored: &Stored) -> bool {
    let Ok(state) = TicketState::try_from(&stored.record.state) else {
        return true;
    };
    match state.stage() {
        Stage::Watch => delivery_followed(stored),
        stage => stage > Stage::Ready,
    }
}

/// `ticket`'s ref `stored` as the rule weighs it, closed with `named`: its
/// pull request open or not, and what its `held.json` holds, the whole
/// repository first dispatched without one.
pub fn in_flight(ticket: &TicketId, stored: &Stored, named: &NamedResources) -> InFlight<String> {
    of(
        ticket,
        stored.record.held.as_ref(),
        delivery_followed(stored),
        named,
    )
}

/// When the ticket of `stored` was dispatched, as its `held.json` keeps it:
/// none for a ref from before P6, which comes first (OWL-401, the turn by
/// dispatch time). A ticket not yet admitted has its dispatch time too.
pub fn dispatched(stored: &Stored) -> Dispatched {
    stored
        .record
        .held
        .as_ref()
        .and_then(|held| held.dispatched_at)
        .map_or(Dispatched::Unrecorded, |at| {
            Dispatched::At(at.as_millisecond())
        })
}

/// A ticket ref that cannot be read: the whole repository, dispatched
/// first, its pull request taken as not open.
pub fn unread(ticket: &TicketId, named: &NamedResources) -> InFlight<String> {
    of(ticket, None, false, named)
}

/// `ticket` holding `held`, `delivered` when its pull request is open; with
/// none, the whole repository, dispatched before any time kept. A record
/// with nothing admitted holds only the files a widening added, which no
/// step makes before the admission, never the whole repository its empty
/// zones would read as (OWL-448).
pub fn of(
    ticket: &TicketId,
    held: Option<&TicketHeld>,
    delivered: bool,
    named: &NamedResources,
) -> InFlight<String> {
    let (dispatched, held) = match held {
        Some(held) => (
            held.dispatched_at.map_or(Dispatched::Unrecorded, |at| {
                Dispatched::At(at.as_millisecond())
            }),
            if held.admitted() {
                resource::closed(&held.zones, &held.files, &held.resources, named)
            } else {
                files_held(&held.files, named)
            },
        ),
        None => (
            Dispatched::Unrecorded,
            resource::closed(&[], &[], &[], named),
        ),
    };
    InFlight {
        id: ticket.as_str().to_owned(),
        dispatched,
        delivered,
        outside: false,
        held,
    }
}

/// The overlaps of `ticket` (OWL-449; architecture section 5, "Tolerated
/// overlap"), from both sides: those its own record `own` keeps, and those
/// the records of `refs`, the project's ticket refs, keep naming it, each
/// recorded once by the ticket whose admission or widening found it. One
/// entry per sibling, by identifier, its paths sorted, each once. A sibling
/// counts while its ref holds, and once its following ended merged or by
/// someone's push, whose change may still reach the base; not once its pull
/// request was closed unmerged, at Ready, or once its ref is gone. A ref
/// that cannot be read counts, since nothing tells it does not.
pub fn overlaps_of(
    ticket: &TicketId,
    own: Option<&TicketHeld>,
    refs: &[RefRead],
) -> Vec<(TicketId, Vec<String>)> {
    let counts = |sibling: &TicketId| match refs.iter().find(|(id, _)| id == sibling) {
        None => false,
        Some((_, Err(_))) => true,
        Some((_, Ok(stored))) => {
            holds(stored)
                || stored
                    .record
                    .delivery
                    .as_ref()
                    .and_then(|delivery| delivery.ended.as_ref())
                    .is_some_and(|ended| ended.why != EndedWhy::Closed)
        }
    };
    let mut found: Vec<(TicketId, Vec<String>)> = Vec::new();
    let mut add = |sibling: &TicketId, paths: &[String]| {
        if sibling == ticket || !counts(sibling) {
            return;
        }
        let at = match found.iter().position(|(id, _)| id == sibling) {
            Some(at) => at,
            None => {
                found.push((sibling.clone(), Vec::new()));
                found.len() - 1
            }
        };
        let kept = &mut found[at].1;
        for path in paths {
            if !kept.contains(path) {
                kept.push(path.clone());
            }
        }
    };
    for overlap in own.map_or(&[][..], |held| held.overlaps.as_slice()) {
        add(&overlap.ticket, &overlap.paths);
    }
    for (recorder, read) in refs {
        let Ok(stored) = read else { continue };
        let Some(held) = stored.record.held.as_ref() else {
            continue;
        };
        for overlap in held.overlaps.iter().filter(|o| o.ticket == *ticket) {
            add(recorder, &overlap.paths);
        }
    }
    for (_, paths) in &mut found {
        paths.sort();
    }
    found.sort_by(|a, b| a.0.cmp(&b.0));
    found
}

/// What [`record`] made of the overlaps a check found (OWL-449).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Recorded {
    /// Each sibling recorded, with the paths its record gained, for the
    /// `overlap` events.
    pub kept: Vec<(TicketId, Vec<String>)>,
    /// The overlaps with new siblings the record could not take, keeping
    /// [`MAX_OVERLAPS`] already: no refresh would cover them, so an
    /// admission waits for them and a widening warns of them.
    ///
    /// [`MAX_OVERLAPS`]: owlshift_contracts::refs::MAX_OVERLAPS
    pub unkept: Vec<Shared<String>>,
}

/// Records in `held` the overlaps of `found` it lacks, at `at` (OWL-449):
/// each sibling that is a ticket identifier, with the paths its record
/// gained, and apart those of new siblings past
/// [`MAX_OVERLAPS`](owlshift_contracts::refs::MAX_OVERLAPS).
pub fn record(held: &mut TicketHeld, found: &[Shared<String>], at: Timestamp) -> Recorded {
    let mut recorded = Recorded::default();
    for shared in found {
        let Ok(sibling) = TicketId::new(&shared.ticket) else {
            continue;
        };
        match held.record_overlap(&sibling, &shared.paths, at) {
            Some(added) if added.is_empty() => {}
            Some(added) => recorded.kept.push((sibling, added)),
            None => recorded.unkept.push(shared.clone()),
        }
    }
    recorded
}

/// The `decision` event of an overlap recorded by `by`, `admission` or
/// `widening` (OWL-449): `overlap` = `recorded`, the sibling in `tickets`,
/// the paths the record gained.
pub fn overlap_event(by: &str, sibling: &TicketId, paths: &[String]) -> Data {
    data([
        ("overlap", json!("recorded")),
        ("by", json!(by)),
        ("tickets", json!([sibling.as_str()])),
        ("paths", json!(paths)),
    ])
}

/// Whether `base`, the base branch's head, is a new head that changes a
/// path of `overlaps` the pull request's head `pushed` does not contain yet
/// (OWL-449, architecture section 5, "The refresh"): `base` is no ancestor
/// of `pushed`, and a file it changed since `since`, the base head read at
/// the push, when `base` descends from it, else since its merge-base with
/// `pushed` (`git diff --name-only --no-renames -z from...base`), is, holds
/// or lies in one of the paths, compared as zones are
/// ([`collision::on_overlap`]). A change the base held at the push needs no
/// refresh: GitHub's checks ran on the merge with it. Watch and a repair
/// read it the same way, so they never disagree.
pub fn base_moved_on(
    git: &Git,
    checkout: &Path,
    pushed: &str,
    since: Option<&str>,
    base: &str,
    overlaps: &[String],
) -> Result<bool, String> {
    if overlaps.is_empty() {
        return Ok(false);
    }
    if is_ancestor(git, checkout, base, pushed)? {
        return Ok(false);
    }
    let from = match since {
        Some(since) if is_ancestor(git, checkout, since, base)? => since,
        _ => pushed,
    };
    let changed = changed_files(git, checkout, from, base)?;
    Ok(collision::on_overlap(&changed, overlaps))
}

/// Whether `ancestor` is an ancestor of `of`, itself included
/// (`git merge-base --is-ancestor`: 0 yes, 1 no).
fn is_ancestor(git: &Git, checkout: &Path, ancestor: &str, of: &str) -> Result<bool, String> {
    let read = git
        .output(
            checkout,
            &[
                "merge-base",
                "--is-ancestor",
                "--end-of-options",
                ancestor,
                of,
            ],
            None,
        )
        .map_err(|e| e.to_string())?;
    match read.code {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        code => Err(format!(
            "whether {of} contains {ancestor}: git exited with {code:?}: {}",
            String::from_utf8_lossy(&read.stderr).trim()
        )),
    }
}

/// What `files` hold alone: each the zone of its own path, with the named
/// resources of `named` one matches.
fn files_held(files: &[String], named: &NamedResources) -> Vec<Resource> {
    let mut held: Vec<Resource> = files.iter().cloned().map(Resource::Zone).collect();
    for (name, pattern) in named.iter() {
        if files.iter().any(|file| pattern.matches(file)) {
            held.push(Resource::Named(name.to_owned()));
        }
    }
    held
}

/// What `held`, a ticket ref's `held.json`, holds once its step into Build
/// admits what is due (OWL-448; architecture section 5, "Resources admitted
/// at Build"), at `at`; `None` when nothing is: no record, a ref from
/// before P6 holding the whole repository, or an admitted record whose plan
/// is the one it admitted, or none. `plan` is the plan the Build follows
/// with its hash, `intake` the ticket's intake record, `labels` the zones of
/// its `zone:` labels, `branch` the files the ticket's branch changed as the
/// step read them ([`changed_files`]; none when the branch is not there),
/// `None` when they were not read, `named` the project's named resources.
///
/// With nothing admitted, the plan's files ([`resource::planned`]) with the
/// labels' zones and the named resources they reach; with no plan or none
/// of its files usable, intake's zones and resources with the labels', as
/// before OWL-448. Once admitted, a plan of another hash is a return to
/// Design's, whose files are added. A record admitted without a plan
/// ([`TicketHeld::narrows`]: of format 1, or holding the whole repository
/// with no plan) is admitted again from a plan whose files are usable, in
/// place of what it held (OWL-461; architecture section 5, "Refs from
/// before OWL-428"): what a first admission takes, with the files it lists
/// and those of `branch`. That needs `branch` for a record holding the
/// whole repository, which lists no file; the step always reads it, and
/// watch, which does not, never predicts that narrowing. A plan none of
/// whose files is usable keeps only its hash.
pub fn at_build(
    held: Option<&TicketHeld>,
    plan: Option<(&Plan, &str)>,
    intake: Option<&TicketIntake>,
    labels: &[String],
    branch: Option<&[String]>,
    named: &NamedResources,
    at: Timestamp,
) -> Option<TicketHeld> {
    let held = held?;
    let hash = plan.map(|(_, hash)| hash.to_owned());
    let planned = plan.and_then(|(plan, _)| resource::planned(&plan.files, MAX_HELD_FILES));
    let mut next = held.clone();
    if !held.admitted() {
        let Held { zones, resources } = match (&planned, intake) {
            (Some(zones), _) => resource::held(zones, labels, &[], named),
            (None, Some(intake)) => resource::held(&intake.zones, labels, &intake.resources, named),
            (None, None) => resource::held(&[], labels, &[], named),
        };
        next.admit(zones, resources, hash, at);
        return Some(next);
    }
    let hash = hash?;
    if held.plan_sha256.as_deref() == Some(hash.as_str()) {
        return None;
    }
    match planned {
        Some(zones) if held.narrows() => {
            if held.whole() && branch.is_none() {
                return None;
            }
            let Held { zones, resources } = resource::held(&zones, labels, &[], named);
            let files = branch.unwrap_or_default().iter().cloned();
            next.readmit(zones, resources, files, Some(hash), at);
        }
        Some(zones) => {
            let Held { zones, resources } = resource::held(&zones, &[], &[], named);
            next.admit(zones, resources, Some(hash), at);
        }
        None => next.plan_sha256 = Some(hash),
    }
    Some(next)
}

/// Whether [`at_build`] admits `held` again from `plan` in place of what it
/// held (OWL-461): admitted without a plan, `plan` naming files it can hold.
/// A plan none of whose files it can hold only keeps its hash.
pub fn readmits(held: &TicketHeld, plan: &Plan) -> bool {
    held.narrows() && resource::planned(&plan.files, MAX_HELD_FILES).is_some()
}

/// [`at_build`] for the ticket ref `record`, as watch predicts its next
/// step (OWL-448): its plan when its pipeline, by its `labels`, its intake
/// record and the project's `default` variant, has Design, as the step
/// itself reads it; the zones of its `zone:` labels; `named`, the project's
/// named resources. Watch reads no branch: a narrowing it predicts holds the
/// files the record lists, and it predicts none for a record holding the
/// whole repository (OWL-461).
pub fn due(
    record: &TicketRecord,
    labels: &[String],
    default: Variant,
    named: &NamedResources,
    at: Timestamp,
) -> Option<TicketHeld> {
    let variant = ticket_variant(
        route_labels(labels.iter().map(String::as_str)).variant,
        record.intake.as_ref().map(|intake| intake.variant),
        default,
    );
    let plan = record
        .plan
        .as_ref()
        .filter(|_| Pipeline::new(variant).contains(Stage::Design))
        .map(|kept| (&kept.plan, plan_sha256(&kept.plan)));
    at_build(
        record.held.as_ref(),
        plan.as_ref().map(|(plan, hash)| (*plan, hash.as_str())),
        record.intake.as_ref(),
        &brief_zones(labels.iter().map(String::as_str)).zones,
        None,
        named,
        at,
    )
}

/// What `ticket`'s admission from `before` to `after` adds, as the rule
/// weighs it with `named`: the resources `after` holds that nothing of
/// `before` holds already (OWL-448), which its check weighs. Its arguments
/// swapped, what a narrowing drops (OWL-461).
pub fn added(
    ticket: &TicketId,
    before: &TicketHeld,
    after: &TicketHeld,
    named: &NamedResources,
) -> Vec<Resource> {
    let before = of(ticket, Some(before), false, named).held;
    let after = of(ticket, Some(after), false, named).held;
    resource::added(&before, &after)
}

/// The `decision` event of a narrowing (OWL-461; architecture section 5,
/// "Refs from before OWL-428"): `held` = `narrowed`, the resources it
/// dropped (`dropped`, each as [`resource_data`] names it) and the count of
/// the files it keeps (`count`).
pub fn narrowed_event(dropped: &[Resource], files: usize) -> Data {
    data([
        ("held", json!("narrowed")),
        (
            "dropped",
            json!(dropped.iter().map(resource_data).collect::<Vec<_>>()),
        ),
        ("count", json!(files)),
    ])
}

/// The files the branch at `head` changed against its merge-base with
/// `base`, both paths of a rename (`git diff --name-only --no-renames -z
/// base...head`, checked on 2026-10-20 with git 2.54.0): what widens a
/// ticket's `held.json`. A path that is not UTF-8 is kept with its bytes
/// replaced, which only ever holds more.
pub fn changed_files(
    git: &Git,
    checkout: &Path,
    base: &str,
    head: &str,
) -> Result<Vec<String>, String> {
    let range = format!("{base}...{head}");
    let out = git
        .run(
            checkout,
            &["diff", "--name-only", "--no-renames", "-z", &range, "--"],
        )
        .map_err(|e| e.to_string())?;
    Ok(out
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| String::from_utf8_lossy(path).into_owned())
        .collect())
}

/// How an event names a held resource: `{"zone": path}`, the empty path for
/// the whole repository, `{"named": name}` or `{"browser": true}`.
pub fn resource_data(resource: &Resource) -> Value {
    match resource {
        Resource::Zone(zone) => json!({ "zone": zone }),
        Resource::Named(name) => json!({ "named": name }),
        Resource::Browser => json!({ "browser": true }),
    }
}

#[cfg(test)]
mod tests {
    use owlshift_contracts::refs::{PersistedState, TicketDelivery};
    use owlshift_contracts::review::TicketPlan;
    use owlshift_core::state::Status;

    use super::*;
    use crate::ticket_ref::blank;

    fn at(status: Status, delivery: Option<TicketDelivery>) -> Stored {
        let round = u32::from(status.stage() != Stage::Ready);
        let state = TicketState::restore(status, round, 0, 0, 0, 0).unwrap();
        let mut record = blank(None);
        record.state = PersistedState::from(&state);
        record.delivery = delivery;
        Stored {
            record,
            commit: "c".repeat(40),
            proposals: None,
        }
    }

    fn minute(n: i64) -> Timestamp {
        Timestamp::from_second(1_790_000_000 + n * 60).unwrap()
    }

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_owned()).collect()
    }

    fn zone(path: &str) -> Resource {
        Resource::Zone(path.to_owned())
    }

    /// From the dispatch until the pull request ends, waiting and parked
    /// included; not at Ready, nor at Watch once the following ended.
    #[test]
    fn a_ref_holds_from_its_dispatch_until_its_pull_request_ends() {
        let delivery = TicketDelivery::parse(include_str!(
            "../../owlshift-contracts/tests/fixtures/ticket-delivery.json"
        ))
        .unwrap();
        let mut ended = delivery.clone();
        ended.pull_request = None;
        let holding = [
            at(Status::Active(Stage::Design), None),
            at(
                Status::NeedsInput {
                    return_to: Stage::Build,
                },
                None,
            ),
            at(
                Status::Parked {
                    at: Stage::Verify,
                    awaiting_input: false,
                },
                None,
            ),
            at(Status::Active(Stage::Watch), Some(delivery)),
        ];
        for stored in &holding {
            assert!(holds(stored), "{:?}", stored.record.state);
        }
        for stored in [
            at(Status::Active(Stage::Ready), None),
            at(
                Status::NeedsInput {
                    return_to: Stage::Intake,
                },
                None,
            ),
            at(Status::Active(Stage::Watch), Some(ended)),
        ] {
            assert!(!holds(&stored), "{:?}", stored.record.state);
        }
        // A ref from before P6 holds the whole repository, dispatched first.
        let named = NamedResources::new([("lock", "Cargo.lock")]).unwrap();
        let ticket = TicketId::new("DEMO-1").unwrap();
        let old = in_flight(&ticket, &holding[3], &named);
        assert_eq!(
            (old.dispatched, old.delivered, &old.held),
            (
                Dispatched::Unrecorded,
                true,
                &vec![zone(""), Resource::Named("lock".to_owned())]
            )
        );
    }

    /// OWL-448: a ticket in Design, dispatched and not admitted, holds
    /// nothing, never the whole repository its empty record would read as,
    /// yet keeps its dispatch time for the turn; a widening's files are all
    /// it could hold.
    #[test]
    fn a_ticket_not_admitted_holds_nothing_but_keeps_its_dispatch_time() {
        let named = NamedResources::new([("lock", "Cargo.lock")]).unwrap();
        let ticket = TicketId::new("DEMO-1").unwrap();
        let mut designing = at(Status::Active(Stage::Design), None);
        designing.record.held = Some(TicketHeld::dispatched(minute(0)));
        let weighed = in_flight(&ticket, &designing, &named);
        assert_eq!(
            (weighed.dispatched, weighed.held),
            (Dispatched::At(minute(0).as_millisecond()), Vec::new())
        );
        assert_eq!(
            dispatched(&designing),
            Dispatched::At(minute(0).as_millisecond())
        );
        let mut widened = TicketHeld::dispatched(minute(0));
        widened.widen(strings(&["Cargo.lock"]), minute(1));
        assert_eq!(
            of(&ticket, Some(&widened), false, &named).held,
            [zone("Cargo.lock"), Resource::Named("lock".to_owned())]
        );
    }

    /// OWL-448: the first step into Build admits the plan's files with the
    /// labels' zones and the named resources they reach, intake's zones
    /// and resources with the labels' when no plan names usable files; a
    /// plan whose files hold the whole repository holds it whatever the
    /// labels; an admitted plan is admitted once.
    #[test]
    fn a_first_admission_takes_the_plans_files_or_intakes_zones() {
        let named = NamedResources::new([("migrations", "db/*/migrations/**")]).unwrap();
        let intake = TicketIntake::parse(include_str!(
            "../../owlshift-contracts/tests/fixtures/ticket-intake.json"
        ))
        .unwrap();
        let mut plan = TicketPlan::parse(include_str!(
            "../../owlshift-contracts/tests/fixtures/ticket-plan.json"
        ))
        .unwrap()
        .plan;
        plan.files = strings(&["db/main/migrations/0002.sql", "web/a.rs"]);
        let nothing = TicketHeld::dispatched(minute(0));
        let labels = strings(&["docs"]);
        let hash = "a".repeat(64);
        let admit = |plan: Option<&Plan>, intake| {
            at_build(
                Some(&nothing),
                plan.map(|plan| (plan, hash.as_str())),
                intake,
                &labels,
                None,
                &named,
                minute(1),
            )
            .unwrap()
        };
        let planned = admit(Some(&plan), Some(&intake));
        assert_eq!(
            (&planned.zones, &planned.resources, planned.admitted_at),
            (
                &strings(&["db/main/migrations/0002.sql", "web/a.rs", "docs"]),
                &strings(&["migrations"]),
                Some(minute(1))
            )
        );
        assert_eq!(planned.plan_sha256.as_ref(), Some(&hash));
        let mut unread = plan.clone();
        unread.files = strings(&["web/a.rs (new)"]);
        for fallback in [
            admit(Some(&unread), Some(&intake)),
            admit(None, Some(&intake)),
        ] {
            assert_eq!(
                (&fallback.zones, &fallback.resources),
                (
                    &strings(&["backend/billing", "docs"]),
                    &strings(&["migrations"])
                )
            );
        }
        let mut whole = plan.clone();
        whole.files = strings(&["**/*.rs"]);
        assert!(admit(Some(&whole), Some(&intake)).whole());
        assert!(admit(None, None).whole());
        // Admitted: the same plan admits nothing; no record, nothing either.
        assert_eq!(
            at_build(
                Some(&planned),
                Some((&plan, &hash)),
                None,
                &labels,
                None,
                &named,
                minute(2)
            ),
            None
        );
        assert_eq!(
            at_build(
                None,
                Some((&plan, &hash)),
                None,
                &labels,
                None,
                &named,
                minute(2)
            ),
            None
        );
    }

    /// OWL-449: a ticket's overlaps from both sides, its own record's and
    /// those other refs keep naming it, one entry per sibling; a sibling
    /// closed unmerged, at Ready or gone counts no more, one merged or
    /// unreadable still does.
    #[test]
    fn overlaps_are_read_from_both_sides_while_the_sibling_counts() {
        let id = |name: &str| TicketId::new(name).unwrap();
        let delivery = TicketDelivery::parse(include_str!(
            "../../owlshift-contracts/tests/fixtures/ticket-delivery.json"
        ))
        .unwrap();
        let mut me = TicketHeld::dispatched(minute(0));
        me.admit(strings(&["app/one.rs"]), Vec::new(), None, minute(1));
        me.record_overlap(&id("DEMO-2"), &strings(&["app/one.rs"]), minute(1));
        me.record_overlap(&id("DEMO-5"), &strings(&["web/x.rs"]), minute(1));
        let sibling = |status: Status, delivery: Option<TicketDelivery>, names: &[&str]| {
            let mut stored = at(status, delivery);
            let mut held = TicketHeld::dispatched(minute(0));
            held.admit(strings(&["app"]), Vec::new(), None, minute(2));
            for name in names {
                held.record_overlap(&id("DEMO-1"), &strings(&[name]), minute(2));
            }
            stored.record.held = Some(held);
            stored
        };
        let mut merged = delivery.clone();
        merged.ended = Some(owlshift_contracts::refs::Ended {
            why: EndedWhy::Merged,
            at: minute(3),
            head: None,
        });
        let mut closed = merged.clone();
        closed.ended.as_mut().unwrap().why = EndedWhy::Closed;
        let refs: Vec<RefRead> = vec![
            (id("DEMO-1"), Ok(at(Status::Active(Stage::Build), None))),
            (
                id("DEMO-2"),
                Ok(sibling(
                    Status::Active(Stage::Watch),
                    Some(merged),
                    &["app/two.rs"],
                )),
            ),
            (
                id("DEMO-3"),
                Ok(sibling(
                    Status::Active(Stage::Build),
                    None,
                    &["app/three.rs"],
                )),
            ),
            (
                id("DEMO-4"),
                Ok(sibling(
                    Status::Active(Stage::Watch),
                    Some(closed),
                    &["app/four.rs"],
                )),
            ),
            (id("DEMO-6"), Err("unreadable".to_owned())),
        ];
        assert_eq!(
            overlaps_of(&id("DEMO-1"), Some(&me), &refs),
            [
                (id("DEMO-2"), strings(&["app/one.rs", "app/two.rs"])),
                (id("DEMO-3"), strings(&["app/three.rs"])),
            ]
        );
        let mut unread = me.clone();
        unread.record_overlap(&id("DEMO-6"), &strings(&["lib/a.rs"]), minute(4));
        assert_eq!(overlaps_of(&id("DEMO-1"), Some(&unread), &refs).len(), 3);
        assert!(overlaps_of(&id("DEMO-9"), None, &refs).is_empty());
    }

    /// OWL-449: what an admission or a widening records, the siblings it
    /// gained paths for apart from those its record cannot take, keeping
    /// the most it keeps, which the caller must not take as recorded.
    #[test]
    fn a_record_keeps_new_paths_and_sets_apart_what_it_cannot_take() {
        let shared = |ticket: &str, paths: &[&str]| Shared {
            ticket: ticket.to_owned(),
            paths: strings(paths),
        };
        let mut held = TicketHeld::dispatched(minute(0));
        held.admit(strings(&["app/one.rs"]), Vec::new(), None, minute(1));
        let found = [
            shared("DEMO-2", &["app/one.rs"]),
            shared("#7", &["app/one.rs"]),
        ];
        let recorded = record(&mut held, &found, minute(2));
        let id = |name: &str| TicketId::new(name).unwrap();
        assert_eq!(recorded.kept, [(id("DEMO-2"), strings(&["app/one.rs"]))]);
        assert!(recorded.unkept.is_empty());
        assert_eq!(record(&mut held, &found, minute(3)), Recorded::default());
        for n in 1..owlshift_contracts::refs::MAX_OVERLAPS {
            held.record_overlap(
                &id(&format!("DEMO-{}", 100 + n)),
                &strings(&["x"]),
                minute(3),
            );
        }
        let more = [
            shared("DEMO-3", &["app/one.rs"]),
            shared("DEMO-2", &["app/two.rs"]),
        ];
        let recorded = record(&mut held, &more, minute(4));
        assert_eq!(recorded.kept, [(id("DEMO-2"), strings(&["app/two.rs"]))]);
        assert_eq!(recorded.unkept, [more[0].clone()]);
    }

    /// OWL-449: a base that changes an overlap's path since the pushed head
    /// left it, in any case or inside a shared folder, refreshes; one that
    /// changes another path, or that the head already contains, does not.
    #[test]
    fn a_base_moved_on_an_overlap_path_is_read_from_git() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        let git = crate::project::runner_git();
        let commit = |files: &[(&str, &str)]| -> String {
            for (path, text) in files {
                let file = repo.join(path);
                std::fs::create_dir_all(file.parent().unwrap()).unwrap();
                std::fs::write(file, text).unwrap();
            }
            git.run(repo, &["add", "-A"]).unwrap();
            git.run(
                repo,
                &[
                    "-c",
                    "user.name=Bench",
                    "-c",
                    "user.email=bench@example.invalid",
                    "-c",
                    "commit.gpgsign=false",
                    "commit",
                    "-q",
                    "-m",
                    "c",
                ],
            )
            .unwrap();
            let head = git.run(repo, &["rev-parse", "HEAD"]).unwrap();
            String::from_utf8(head).unwrap().trim().to_owned()
        };
        git.run(repo, &["init", "-q", "-b", "main"]).unwrap();
        commit(&[("app/one.rs", "one\n"), ("docs/a.md", "a\n")]);
        git.run(repo, &["checkout", "-q", "-b", "owl-1"]).unwrap();
        let pushed = commit(&[("app/one.rs", "one, mine\n")]);
        git.run(repo, &["checkout", "-q", "main"]).unwrap();
        let other = commit(&[("docs/a.md", "b\n")]);
        let shared = commit(&[("app/one.rs", "one, theirs\n")]);
        let moved = |base: &str, paths: &[&str]| {
            base_moved_on(&git, repo, &pushed, None, base, &strings(paths)).unwrap()
        };
        assert!(!moved(&other, &["app/one.rs"]));
        assert!(moved(&shared, &["app/one.rs"]));
        assert!(moved(&shared, &["App/One.rs"]));
        assert!(moved(&shared, &["app"]));
        assert!(!moved(&shared, &["web"]));
        assert!(!moved(&shared, &[]));
        // A change the base held at the push already, its checks run on it.
        let at_push = |since: &str| {
            base_moved_on(
                &git,
                repo,
                &pushed,
                Some(since),
                &shared,
                &strings(&["app/one.rs"]),
            )
            .unwrap()
        };
        assert!(!at_push(&shared) && at_push(&other));
        // A head that contains the base's change already needs no refresh.
        git.run(repo, &["checkout", "-q", "owl-1"]).unwrap();
        git.run(
            repo,
            &[
                "-c",
                "user.name=Bench",
                "-c",
                "user.email=bench@example.invalid",
                "merge",
                "-q",
                "-X",
                "ours",
                "-m",
                "m",
                &shared,
            ],
        )
        .unwrap();
        let merged = git.run(repo, &["rev-parse", "HEAD"]).unwrap();
        let merged = String::from_utf8(merged).unwrap().trim().to_owned();
        let contained = base_moved_on(
            &git,
            repo,
            &merged,
            None,
            &shared,
            &strings(&["app/one.rs"]),
        );
        assert!(!contained.unwrap());
    }

    /// The plan of the ticket-plan fixture, naming `files`.
    fn plan_naming(files: &[&str]) -> Plan {
        let mut plan = TicketPlan::parse(include_str!(
            "../../owlshift-contracts/tests/fixtures/ticket-plan.json"
        ))
        .unwrap()
        .plan;
        plan.files = strings(files);
        plan
    }

    /// A record of format 1, as a binary older than OWL-448 wrote it:
    /// intake's `zones` and a named resource held from its dispatch, no
    /// plan.
    fn format_1(zones: &[&str]) -> TicketHeld {
        TicketHeld::parse(
            &json!({
                "format": 1,
                "zones": zones,
                "resources": ["migrations"],
                "dispatched_at": minute(0).to_string(),
            })
            .to_string(),
        )
        .unwrap()
    }

    /// OWL-448: a return to Design's new plan adds its files, and only what
    /// they add is checked.
    #[test]
    fn a_return_to_designs_plan_adds_its_files() {
        let named = NamedResources::default();
        let ticket = TicketId::new("DEMO-1").unwrap();
        let plan = plan_naming(&["app/a.rs", "web/b.rs"]);
        let mut admitted = TicketHeld::dispatched(minute(0));
        admitted.admit(
            strings(&["app"]),
            Vec::new(),
            Some("a".repeat(64)),
            minute(1),
        );
        let hash = "b".repeat(64);
        let next = at_build(
            Some(&admitted),
            Some((&plan, &hash)),
            None,
            &[],
            None,
            &named,
            minute(2),
        )
        .unwrap();
        assert_eq!(
            (&next.zones, next.admitted_at, next.plan_sha256.as_ref()),
            (
                &strings(&["app", "app/a.rs", "web/b.rs"]),
                Some(minute(2)),
                Some(&hash)
            )
        );
        assert_eq!(added(&ticket, &admitted, &next, &named), [zone("web/b.rs")]);
    }

    /// OWL-461: a record of format 1 holding intake's folders, built (a
    /// widening kept), is admitted again at its next Build from the plan it
    /// follows: the plan's files, the labels' zones and the named resources
    /// they reach, with the widened files and those the step read from its
    /// branch; no folder, nor intake's resource. A plan within its folders
    /// adds only the label's zone to check, and what it drops is the
    /// folders and the resource. Unbuilt, the same. A second Build of the
    /// plan changes nothing; a plan none of whose files is usable keeps only
    /// its hash.
    #[test]
    fn a_format_1_record_is_narrowed_to_its_plans_files_at_its_next_build() {
        let named = NamedResources::new([("lock", "app/Cargo.lock")]).unwrap();
        let ticket = TicketId::new("DEMO-1").unwrap();
        let plan = plan_naming(&["app/a.rs", "app/Cargo.lock"]);
        let hash = "c".repeat(64);
        let mut built = format_1(&["app", "web"]);
        built.widen(strings(&["web/w.rs"]), minute(1));
        let labels = strings(&["docs"]);
        let narrow = |held: &TicketHeld, branch: Option<&[String]>| {
            at_build(
                Some(held),
                Some((&plan, &hash)),
                None,
                &labels,
                branch,
                &named,
                minute(2),
            )
            .unwrap()
        };
        let branch = strings(&["app/b.rs"]);
        let narrowed = narrow(&built, Some(&branch));
        assert_eq!(
            (
                &narrowed.zones,
                &narrowed.resources,
                &narrowed.files,
                narrowed.admitted_at,
                narrowed.plan_sha256.as_ref()
            ),
            (
                &strings(&["app/a.rs", "app/Cargo.lock", "docs"]),
                &strings(&["lock"]),
                &strings(&["app/b.rs", "web/w.rs"]),
                Some(minute(2)),
                Some(&hash)
            )
        );
        assert_eq!(added(&ticket, &built, &narrowed, &named), [zone("docs")]);
        assert_eq!(
            added(&ticket, &narrowed, &built, &named),
            [
                zone("app"),
                zone("web"),
                Resource::Named("migrations".to_owned())
            ]
        );
        // Watch's prediction, with no branch read, holds the recorded files.
        assert_eq!(narrow(&built, None).files, strings(&["web/w.rs"]));
        let unbuilt = narrow(&format_1(&["app"]), Some(&[]));
        assert_eq!(
            (&unbuilt.zones, &unbuilt.files),
            (
                &strings(&["app/a.rs", "app/Cargo.lock", "docs"]),
                &Vec::new()
            )
        );
        assert_eq!(
            at_build(
                Some(&narrowed),
                Some((&plan, &hash)),
                None,
                &labels,
                Some(&branch),
                &named,
                minute(3)
            ),
            None
        );
        let unusable = plan_naming(&["web/a.rs (new)"]);
        let kept = at_build(
            Some(&built),
            Some((&unusable, &hash)),
            None,
            &labels,
            Some(&branch),
            &named,
            minute(2),
        )
        .unwrap();
        assert_eq!(
            (&kept.zones, kept.plan_sha256.as_ref()),
            (&strings(&["app", "web"]), Some(&hash))
        );
        // What the step tells as a narrowing: a re-admission, whatever it
        // dropped, never a plan's hash kept alone.
        assert!(readmits(&built, &plan));
        assert!(!readmits(&built, &unusable) && !readmits(&narrowed, &plan));
    }

    /// OWL-461: a record holding the whole repository with no plan narrows
    /// once the step read its branch, of which it lists no file; watch,
    /// reading none, predicts nothing. The narrowing adds nothing to check.
    #[test]
    fn a_whole_record_narrows_only_with_its_branch_read() {
        let named = NamedResources::default();
        let ticket = TicketId::new("DEMO-1").unwrap();
        let plan = plan_naming(&["app/a.rs"]);
        let hash = "d".repeat(64);
        let whole = format_1(&[]);
        assert!(whole.whole());
        let narrow = |branch: Option<&[String]>| {
            at_build(
                Some(&whole),
                Some((&plan, &hash)),
                None,
                &[],
                branch,
                &named,
                minute(2),
            )
        };
        assert_eq!(narrow(None), None);
        let branch = strings(&["lib/x.rs"]);
        let narrowed = narrow(Some(&branch)).unwrap();
        assert_eq!(
            (&narrowed.zones, &narrowed.files),
            (&strings(&["app/a.rs"]), &branch)
        );
        assert!(added(&ticket, &whole, &narrowed, &named).is_empty());
        assert_eq!(
            added(&ticket, &narrowed, &whole, &named),
            [zone(""), Resource::Named("migrations".to_owned())]
        );
    }

    /// OWL-461: a re-admission whose new file another ticket holds collides,
    /// as a first admission does, whatever the holder's dispatch time; a
    /// Build that waited on a folder the narrowing dropped no longer does.
    #[test]
    fn a_narrowing_is_checked_for_what_it_adds_and_frees_what_it_drops() {
        let named = NamedResources::default();
        let one = TicketId::new("DEMO-1").unwrap();
        let two = TicketId::new("DEMO-2").unwrap();
        let old = format_1(&["app"]);
        let hash = "e".repeat(64);
        let plan = plan_naming(&["app/a.rs", "web/b.rs"]);
        let narrowed = at_build(
            Some(&old),
            Some((&plan, &hash)),
            None,
            &[],
            Some(&[]),
            &named,
            minute(2),
        )
        .unwrap();
        let mut holder = TicketHeld::dispatched(minute(1));
        holder.admit(strings(&["web/b.rs"]), Vec::new(), None, minute(1));
        let weighed = InFlight {
            held: added(&one, &old, &narrowed, &named),
            dispatched: Dispatched::Now,
            ..of(&one, Some(&old), false, &named)
        };
        let found = collision::weigh(
            &weighed,
            &[of(&two, Some(&holder), false, &named)],
            Default::default(),
        );
        assert_eq!(
            found.collision.map(|c| (c.holder, c.resource)),
            Some(("DEMO-2".to_owned(), zone("web/b.rs")))
        );
        let waiting = InFlight {
            held: vec![zone("app/c.rs")],
            dispatched: Dispatched::Now,
            ..of(&two, None, false, &named)
        };
        let against = |held: &TicketHeld| {
            collision::weigh(
                &waiting,
                &[of(&one, Some(held), false, &named)],
                Default::default(),
            )
            .collision
        };
        assert!(against(&old).is_some());
        assert_eq!(against(&narrowed), None);
    }
}
