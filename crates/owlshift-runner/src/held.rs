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
use owlshift_contracts::refs::{MAX_HELD_FILES, TicketHeld};
use owlshift_contracts::review::Plan;
use owlshift_core::collision::{Dispatched, InFlight};
use owlshift_core::decider::brief_zones;
use owlshift_core::pipeline::Pipeline;
use owlshift_core::resource::{self, Held, NamedResources, Resource};
use owlshift_core::state::TicketState;
use owlshift_core::vocab::{Stage, Variant};

use crate::executor::Git;
use crate::on_demand::{delivery_followed, plan_sha256};
use crate::routing::{route_labels, ticket_variant};
use crate::ticket_ref::{Stored, TicketRecord};

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
        held,
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
/// its `zone:` labels, `named` the project's named resources.
///
/// With nothing admitted, the plan's files ([`resource::planned`]) with the
/// labels' zones and the named resources they reach; with no plan or none
/// of its files usable, intake's zones and resources with the labels', as
/// before OWL-448. Once admitted, a plan of another hash is a return to
/// Design's, whose files are added; a record of format 1, kept with no
/// plan, adds them too unless it was built (a widening kept), when only the
/// plan's hash is kept.
pub fn at_build(
    held: Option<&TicketHeld>,
    plan: Option<(&Plan, &str)>,
    intake: Option<&TicketIntake>,
    labels: &[String],
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
    let built = held.plan_sha256.is_none() && !held.widened_at.is_empty();
    match planned {
        Some(zones) if !built => {
            let Held { zones, resources } = resource::held(&zones, &[], &[], named);
            next.admit(zones, resources, Some(hash), at);
        }
        _ => next.plan_sha256 = Some(hash),
    }
    Some(next)
}

/// [`at_build`] for the ticket ref `record`, as watch predicts its next
/// step (OWL-448): its plan when its pipeline, by its `labels`, its intake
/// record and the project's `default` variant, has Design, as the step
/// itself reads it; the zones of its `zone:` labels; `named`, the project's
/// named resources.
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
        named,
        at,
    )
}

/// What `ticket`'s admission from `before` to `after` adds, as the rule
/// weighs it with `named`: the resources `after` holds that nothing of
/// `before` holds already (OWL-448), which its check weighs.
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
                &named,
                minute(2)
            ),
            None
        );
        assert_eq!(
            at_build(None, Some((&plan, &hash)), None, &labels, &named, minute(2)),
            None
        );
    }

    /// OWL-448: a return to Design's new plan adds its files, and only what
    /// they add is checked; a record of format 1 adds them while not built,
    /// and keeps only the plan's hash once a widening was kept.
    #[test]
    fn a_later_plan_adds_its_files_unless_a_format_1_ticket_was_built() {
        let named = NamedResources::default();
        let ticket = TicketId::new("DEMO-1").unwrap();
        let mut plan = TicketPlan::parse(include_str!(
            "../../owlshift-contracts/tests/fixtures/ticket-plan.json"
        ))
        .unwrap()
        .plan;
        plan.files = strings(&["app/a.rs", "web/b.rs"]);
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

        let old = TicketHeld::parse(&format!(
            r#"{{"format":1,"zones":["app"],"dispatched_at":"{}"}}"#,
            minute(0)
        ))
        .unwrap();
        let unbuilt = at_build(
            Some(&old),
            Some((&plan, &hash)),
            None,
            &[],
            &named,
            minute(3),
        )
        .unwrap();
        assert_eq!(unbuilt.zones, strings(&["app", "app/a.rs", "web/b.rs"]));
        let mut built = old.clone();
        built.widen(strings(&["app/c.rs"]), minute(1));
        let kept = at_build(
            Some(&built),
            Some((&plan, &hash)),
            None,
            &[],
            &named,
            minute(3),
        )
        .unwrap();
        assert_eq!(
            (&kept.zones, kept.plan_sha256.as_ref()),
            (&strings(&["app"]), Some(&hash))
        );
        assert!(added(&ticket, &built, &kept, &named).is_empty());
    }
}
