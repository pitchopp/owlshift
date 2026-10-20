//! The collision rule between Owlshift's tickets (OWL-403; architecture
//! section 5, "Parallel", "The collision rule"): a ticket that would touch
//! what another ticket in flight holds waits, from plain data, against the
//! in-flight set as the caller read it. The runner reads what each ticket
//! holds from its ticket ref and closes it with the project's named
//! resources ([`resource::closed`](crate::resource::closed)); this module
//! only weighs the sets.

use std::cmp::Ordering;

use crate::resource::Resource;
use crate::schedule::natural_cmp;

/// When an Owlshift ticket was dispatched, which orders two tickets not yet
/// delivered: the one dispatched first goes on. Sorted in that order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Dispatched {
    /// Before its dispatch time was kept: a ticket ref from before P6, which
    /// came before every dispatch whose time is kept.
    Unrecorded,
    /// At this time, in milliseconds since the Unix epoch.
    At(i64),
    /// Not yet: the ticket a dispatch from Ready weighs, after every holder.
    Now,
}

/// An Owlshift ticket of the project, as the rule weighs it: from its
/// dispatch until its pull request ends, waiting and parked included
/// (architecture section 5, "The in-flight set", 1).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct InFlight<K> {
    pub id: K,
    pub dispatched: Dispatched,
    /// Its pull request is open: it never waits, since its repairs are what
    /// lets it merge, and every ticket not yet delivered waits for it. Work
    /// Owlshift does not run, another open pull request or a person's
    /// ticket, is weighed so too (OWL-410): against it, Owlshift's ticket
    /// waits.
    pub delivered: bool,
    /// What it holds, closed with the named resources its zones and files
    /// reach.
    pub held: Vec<Resource>,
}

/// Why a ticket waits: `holder` holds `resource`, which one of the ticket's
/// resources conflicts with.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Collision<K> {
    pub holder: K,
    pub resource: Resource,
}

/// Whether `ticket` waits for one of `others`, and for whom: `None` when it
/// goes on. A delivered ticket never waits. Otherwise each other ticket that
/// is delivered, or was dispatched before it (then in identifier order,
/// [`natural_cmp`]), holds it back when one of its resources conflicts with
/// one of the ticket's ([`Resource::conflicts_with`]): the first such holder
/// in identifier order, with the first of its resources that conflicts.
/// Entries with the ticket's own identifier are left out.
pub fn collision<K: Clone + Ord + AsRef<str>>(
    ticket: &InFlight<K>,
    others: &[InFlight<K>],
) -> Option<Collision<K>> {
    if ticket.delivered {
        return None;
    }
    let mut holders: Vec<&InFlight<K>> = others
        .iter()
        .filter(|other| other.id != ticket.id)
        .filter(|other| other.delivered || first(other, ticket) == Ordering::Less)
        .collect();
    holders.sort_by(|a, b| natural_cmp(a.id.as_ref(), b.id.as_ref()).then_with(|| a.id.cmp(&b.id)));
    holders.into_iter().find_map(|holder| {
        holder
            .held
            .iter()
            .find(|theirs| ticket.held.iter().any(|mine| mine.conflicts_with(theirs)))
            .map(|resource| Collision {
                holder: holder.id.clone(),
                resource: resource.clone(),
            })
    })
}

/// The order of two tickets' dispatches: by when, then by identifier.
fn first<K: Ord + AsRef<str>>(a: &InFlight<K>, b: &InFlight<K>) -> Ordering {
    a.dispatched
        .cmp(&b.dispatched)
        .then_with(|| natural_cmp(a.id.as_ref(), b.id.as_ref()))
        .then_with(|| a.id.cmp(&b.id))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zone(path: &str) -> Resource {
        Resource::Zone(path.to_owned())
    }

    fn flight(
        id: &'static str,
        dispatched: Dispatched,
        delivered: bool,
        held: &[Resource],
    ) -> InFlight<&'static str> {
        InFlight {
            id,
            dispatched,
            delivered,
            held: held.to_vec(),
        }
    }

    #[test]
    fn the_first_dispatched_goes_on_and_the_later_waits() {
        let early = flight("OWL-2", Dispatched::At(10), false, &[zone("app")]);
        let late = flight("OWL-1", Dispatched::At(20), false, &[zone("app/x.rs")]);
        assert_eq!(collision(&early, std::slice::from_ref(&late)), None);
        assert_eq!(
            collision(&late, std::slice::from_ref(&early)),
            Some(Collision {
                holder: "OWL-2",
                resource: zone("app"),
            })
        );
        // At the same time, the lower identifier is first.
        let twin = flight("OWL-10", Dispatched::At(10), false, &[zone("app")]);
        assert!(collision(&early, std::slice::from_ref(&twin)).is_none());
        assert!(collision(&twin, std::slice::from_ref(&early)).is_some());
        // A ref from before P6 comes before any kept time; a dispatch from
        // Ready after every holder.
        let old = flight("OWL-9", Dispatched::Unrecorded, false, &[zone("")]);
        assert!(collision(&old, std::slice::from_ref(&early)).is_none());
        assert!(collision(&early, std::slice::from_ref(&old)).is_some());
        let ready = flight("OWL-3", Dispatched::Now, false, &[zone("app")]);
        assert!(collision(&ready, std::slice::from_ref(&late)).is_some());
    }

    #[test]
    fn an_open_pull_request_never_waits_and_holds_every_ticket_not_delivered() {
        let delivered = flight("OWL-5", Dispatched::At(30), true, &[zone("app/x.rs")]);
        let early = flight("OWL-1", Dispatched::At(10), false, &[zone("app")]);
        assert_eq!(
            collision(&early, std::slice::from_ref(&delivered)),
            Some(Collision {
                holder: "OWL-5",
                resource: zone("app/x.rs"),
            })
        );
        assert_eq!(collision(&delivered, std::slice::from_ref(&early)), None);
        let both = flight("OWL-6", Dispatched::At(5), true, &[zone("app")]);
        assert_eq!(collision(&both, std::slice::from_ref(&delivered)), None);
        // Work outside Owlshift (OWL-410) holds a ticket dispatched before
        // its own read, and a ticket with an open pull request goes on.
        let person = flight("OWL-7", Dispatched::Unrecorded, true, &[zone("app/y")]);
        assert_eq!(
            collision(&early, std::slice::from_ref(&person)),
            Some(Collision {
                holder: "OWL-7",
                resource: zone("app/y"),
            })
        );
        assert_eq!(collision(&both, std::slice::from_ref(&person)), None);
    }

    #[test]
    fn the_first_holder_by_identifier_is_named_with_its_first_conflicting_resource() {
        let ticket = flight(
            "OWL-30",
            Dispatched::Now,
            false,
            &[zone("web"), Resource::Named("migrations".to_owned())],
        );
        let others = [
            flight("OWL-10", Dispatched::At(1), false, &[zone("docs")]),
            flight(
                "OWL-9",
                Dispatched::At(2),
                false,
                &[
                    zone("db"),
                    Resource::Named("migrations".to_owned()),
                    zone("web/a"),
                ],
            ),
            flight("OWL-8", Dispatched::At(3), true, &[zone("api")]),
            flight("OWL-30", Dispatched::At(0), true, &[zone("web")]),
        ];
        assert_eq!(
            collision(&ticket, &others),
            Some(Collision {
                holder: "OWL-9",
                resource: Resource::Named("migrations".to_owned()),
            })
        );
        let apart = flight("OWL-31", Dispatched::Now, false, &[zone("cli")]);
        assert_eq!(collision(&apart, &others), None);
    }
}
