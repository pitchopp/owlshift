//! What an Owlshift ticket holds against the project's other tickets
//! (OWL-403; architecture section 5, "Parallel", "What a ticket holds" and
//! "The in-flight set", 1): which ticket refs hold, what each holds as the
//! core's collision rule weighs it ([`owlshift_core::collision`]), and the
//! files a ticket's branch changed, which widen its `held.json`.
//!
//! A ticket holds from its dispatch until its pull request ends, waiting and
//! parked included. A ref with no `held.json`, dispatched before it was
//! kept, holds the whole repository and comes before every other dispatch;
//! so does a ref that cannot be read, since nothing tells what it holds.

use std::path::Path;

use serde_json::{Value, json};

use owlshift_contracts::ids::TicketId;
use owlshift_contracts::refs::TicketHeld;
use owlshift_core::collision::{Dispatched, InFlight};
use owlshift_core::resource::{self, NamedResources, Resource};
use owlshift_core::state::TicketState;
use owlshift_core::vocab::Stage;

use crate::executor::Git;
use crate::on_demand::delivery_followed;
use crate::ticket_ref::Stored;

/// Whether the ticket ref `stored` holds what its ticket holds: at a stage
/// after Ready and before Watch, working, waiting or parked there, or at
/// Watch while its pull request is followed. A state that does not read
/// holds, since nothing tells it does not.
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
/// dispatch time).
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
/// none, the whole repository, dispatched before any time kept.
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
            resource::closed(&held.zones, &held.files, &held.resources, named),
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

/// What a ticket of a ref from before P6 holds once a record must be kept
/// for it: the whole repository, with no dispatch time, so it stays first.
pub fn unrecorded() -> TicketHeld {
    TicketHeld::dispatched(Vec::new(), Vec::new(), None)
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
                &vec![
                    Resource::Zone(String::new()),
                    Resource::Named("lock".to_owned())
                ]
            )
        );
    }
}
