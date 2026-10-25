//! The collision rule between Owlshift's tickets (OWL-403; architecture
//! section 5, "Parallel", "The collision rule"): a ticket that would touch
//! what another ticket in flight holds waits, from plain data, against the
//! in-flight set as the caller read it. The runner reads what each ticket
//! holds from its ticket ref and closes it with the project's named
//! resources ([`resource::closed`](crate::resource::closed)); this module
//! only weighs the sets. Since OWL-448 a ticket holds nothing before its
//! Build's admission, which weighs it [`Dispatched::Now`]; the dispatch
//! order settles the checks of admitted tickets before each step.
//!
//! Under `policy.overlap = "rebase"` (OWL-449, "Tolerated overlap"),
//! [`weigh`] lets two Owlshift tickets hold the same file or zone, and
//! names the overlap: a named resource, a holder of the whole repository,
//! work outside Owlshift and a third holder of one path still exclude.

use std::cmp::Ordering;

use crate::resource::{Resource, contains, same_zone, segments};
use crate::schedule::natural_cmp;
use crate::vocab::Overlap;

/// When an Owlshift ticket was dispatched, which orders two tickets not yet
/// delivered: the one dispatched first goes on. Sorted in that order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Dispatched {
    /// Before its dispatch time was kept: a ticket ref from before P6, which
    /// came before every dispatch whose time is kept.
    Unrecorded,
    /// At this time, in milliseconds since the Unix epoch.
    At(i64),
    /// After every holder: the ticket a Build's admission weighs (OWL-448),
    /// which waits for anything held, whatever the holder's dispatch time,
    /// so that an admitted ticket never waits for one admitted after it.
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
    /// Work Owlshift does not run (OWL-410): another open pull request or a
    /// person's ticket. Never an overlap's sibling under `rebase`
    /// (OWL-449): the runner rebases only its own branches.
    pub outside: bool,
    /// What it holds, closed with the named resources its zones and files
    /// reach.
    pub held: Vec<Resource>,
}

/// Why a ticket waits: `holder` holds `resource`, which one of the ticket's
/// resources conflicts with. Under `rebase`, `sharing` is the second
/// Owlshift ticket holding the path `resource` names: the ticket would be
/// its third holder (OWL-449).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Collision<K> {
    pub holder: K,
    pub resource: Resource,
    pub sharing: Option<K>,
}

impl<K: Clone> Collision<K> {
    /// The tickets it names: the holder, then the one sharing the path.
    pub fn holders(&self) -> Vec<K> {
        std::iter::once(self.holder.clone())
            .chain(self.sharing.clone())
            .collect()
    }
}

/// An overlap a ticket tolerates under `rebase` (OWL-449): `ticket` holds
/// what it holds on `paths`, the inner path of each conflicting pair (a file
/// inside a shared zone, or the zone both hold), each once, sorted.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Shared<K> {
    pub ticket: K,
    pub paths: Vec<String>,
}

/// What [`weigh`] finds: the collision the ticket waits for, or, when it
/// goes on, the overlaps it tolerates, by identifier.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Weighed<K> {
    pub collision: Option<Collision<K>>,
    pub overlaps: Vec<Shared<K>>,
}

/// The most Owlshift tickets that hold one path under `rebase` (OWL-449):
/// each overlap costs one repair, never a chain of rebases.
pub const MAX_HOLDERS: usize = 2;

/// Whether `ticket` waits for one of `others`, and for whom: `None` when it
/// goes on. A delivered ticket never waits. Otherwise each other ticket that
/// is delivered, or was dispatched before it (then in identifier order,
/// [`natural_cmp`]), holds it back when one of its resources conflicts with
/// one of the ticket's ([`Resource::conflicts_with`]): the first such holder
/// in identifier order, with the first of its resources that conflicts.
/// Entries with the ticket's own identifier are left out. The rule of
/// `policy.overlap = "exclusive"` ([`weigh`]).
pub fn collision<K: Clone + Ord + AsRef<str>>(
    ticket: &InFlight<K>,
    others: &[InFlight<K>],
) -> Option<Collision<K>> {
    weigh(ticket, others, Overlap::Exclusive).collision
}

/// [`collision`] under the project's `overlap` (OWL-449; architecture
/// section 5, "Tolerated overlap"). Under [`Overlap::Exclusive`], exactly
/// [`collision`], no overlap named.
///
/// Under [`Overlap::Rebase`], a conflict between two zones (a file being
/// the zone of its own path) is tolerated when the holder is one of
/// Owlshift's tickets ([`InFlight::outside`] false) and neither zone is the
/// whole repository; any other conflict, a named resource's among them,
/// holds the ticket back as under [`collision`]. Then the bound: no path is
/// held by more than [`MAX_HOLDERS`] Owlshift tickets, a zone holding every
/// path it contains, so a ticket that would be the third holder of a path
/// waits, the two that go before it named (dispatch order as above). A
/// ticket that goes on is told its overlaps with every other Owlshift
/// ticket, before or after it, delivered or not: each with the inner path
/// of each tolerated pair.
pub fn weigh<K: Clone + Ord + AsRef<str>>(
    ticket: &InFlight<K>,
    others: &[InFlight<K>],
    overlap: Overlap,
) -> Weighed<K> {
    let mut others: Vec<&InFlight<K>> = others.iter().filter(|o| o.id != ticket.id).collect();
    others.sort_by(|a, b| natural_cmp(a.id.as_ref(), b.id.as_ref()).then_with(|| a.id.cmp(&b.id)));
    let before: Vec<&InFlight<K>> = if ticket.delivered {
        Vec::new()
    } else {
        others
            .iter()
            .copied()
            .filter(|other| other.delivered || first(other, ticket) == Ordering::Less)
            .collect()
    };
    let tolerated = |holder: &InFlight<K>, mine: &Resource, theirs: &Resource| {
        overlap == Overlap::Rebase
            && !holder.outside
            && zone_path(mine).is_some()
            && zone_path(theirs).is_some()
    };
    // Any conflict not tolerated, in the order `collision` always named.
    let found = before.iter().find_map(|holder| {
        holder
            .held
            .iter()
            .find(|theirs| {
                ticket
                    .held
                    .iter()
                    .any(|mine| mine.conflicts_with(theirs) && !tolerated(holder, mine, theirs))
            })
            .map(|resource| Collision {
                holder: holder.id.clone(),
                resource: resource.clone(),
                sharing: None,
            })
    });
    let found = found.or_else(|| {
        (overlap == Overlap::Rebase)
            .then(|| beyond_bound(ticket, &before))
            .flatten()
    });
    if found.is_some() || overlap == Overlap::Exclusive {
        return Weighed {
            collision: found,
            overlaps: Vec::new(),
        };
    }
    let overlaps = others
        .iter()
        .filter(|other| !other.outside)
        .filter_map(|other| {
            let mut paths: Vec<String> = Vec::new();
            for mine in ticket.held.iter().filter_map(zone_path) {
                for theirs in other.held.iter().filter_map(zone_path) {
                    let (m, t) = (segments(mine), segments(theirs));
                    let inner = if contains(&m, &t) {
                        theirs
                    } else if contains(&t, &m) {
                        mine
                    } else {
                        continue;
                    };
                    if !paths.iter().any(|kept| same_zone(kept, inner)) {
                        paths.push(inner.to_owned());
                    }
                }
            }
            // A path inside another shared one adds nothing to it.
            let kept = paths.clone();
            paths.retain(|path| {
                !kept.iter().any(|outer| {
                    !same_zone(outer, path) && contains(&segments(outer), &segments(path))
                })
            });
            paths.sort();
            (!paths.is_empty()).then(|| Shared {
                ticket: other.id.clone(),
                paths,
            })
        })
        .collect();
    Weighed {
        collision: None,
        overlaps,
    }
}

/// The path of a zone that is not the whole repository; `None` for the
/// whole repository and any other kind of resource.
fn zone_path(resource: &Resource) -> Option<&str> {
    match resource {
        Resource::Zone(zone) if !segments(zone).is_empty() => Some(zone),
        _ => None,
    }
}

/// The bound under `rebase` (OWL-449): the first path of `ticket` that
/// [`MAX_HOLDERS`] Owlshift tickets of `before` hold already, a zone
/// holding every path it contains. The paths weighed are the ticket's zones,
/// then the zones of the holders that one of the ticket's contains, since
/// the paths holders share most are the deepest.
fn beyond_bound<K: Clone + Ord + AsRef<str>>(
    ticket: &InFlight<K>,
    before: &[&InFlight<K>],
) -> Option<Collision<K>> {
    let owlshift: Vec<&InFlight<K>> = before.iter().copied().filter(|h| !h.outside).collect();
    let mine: Vec<&str> = ticket.held.iter().filter_map(zone_path).collect();
    let inner = owlshift
        .iter()
        .flat_map(|holder| holder.held.iter().filter_map(zone_path))
        .filter(|theirs| {
            mine.iter()
                .any(|zone| contains(&segments(zone), &segments(theirs)))
        });
    for point in mine.iter().copied().chain(inner) {
        let at = segments(point);
        let holding: Vec<&InFlight<K>> = owlshift
            .iter()
            .copied()
            .filter(|holder| {
                holder
                    .held
                    .iter()
                    .filter_map(zone_path)
                    .any(|zone| contains(&segments(zone), &at))
            })
            .collect();
        if holding.len() >= MAX_HOLDERS {
            return Some(Collision {
                holder: holding[0].id.clone(),
                resource: Resource::Zone(point.to_owned()),
                sharing: Some(holding[1].id.clone()),
            });
        }
    }
    None
}

/// Whether a rebase's conflict lies on an overlap (OWL-449, "A review after
/// a resolution"): one of the `conflicting` paths is, contains or lies in
/// one of the ticket's `overlaps` paths, compared as zones are, part by
/// part in any ASCII case; the whole repository is no overlap's path.
pub fn on_overlap(conflicting: &[String], overlaps: &[String]) -> bool {
    conflicting.iter().any(|path| {
        overlaps.iter().any(|shared| {
            !segments(path).is_empty()
                && !segments(shared).is_empty()
                && Resource::Zone(path.clone()).conflicts_with(&Resource::Zone(shared.clone()))
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
            outside: false,
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
                sharing: None,
            })
        );
        // At the same time, the lower identifier is first.
        let twin = flight("OWL-10", Dispatched::At(10), false, &[zone("app")]);
        assert!(collision(&early, std::slice::from_ref(&twin)).is_none());
        assert!(collision(&twin, std::slice::from_ref(&early)).is_some());
        // A ref from before P6 comes before any kept time; a Build's
        // admission after every holder, even one dispatched later (OWL-448).
        let old = flight("OWL-9", Dispatched::Unrecorded, false, &[zone("")]);
        assert!(collision(&old, std::slice::from_ref(&early)).is_none());
        assert!(collision(&early, std::slice::from_ref(&old)).is_some());
        let admitting = flight("OWL-3", Dispatched::Now, false, &[zone("app")]);
        assert!(collision(&admitting, std::slice::from_ref(&late)).is_some());
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
                sharing: None,
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
                sharing: None,
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
                sharing: None,
            })
        );
        let apart = flight("OWL-31", Dispatched::Now, false, &[zone("cli")]);
        assert_eq!(collision(&apart, &others), None);
    }

    fn shared(ticket: &'static str, paths: &[&str]) -> Shared<&'static str> {
        Shared {
            ticket,
            paths: paths.iter().map(|p| (*p).to_owned()).collect(),
        }
    }

    fn rebase(
        ticket: &InFlight<&'static str>,
        others: &[InFlight<&'static str>],
    ) -> Weighed<&'static str> {
        weigh(ticket, others, Overlap::Rebase)
    }

    /// OWL-449: under `rebase`, a file or a zone two Owlshift tickets hold
    /// is an overlap, the inner path of each pair named, with tickets
    /// before or after; exclusive weighs as `collision` and names none.
    #[test]
    fn under_rebase_a_shared_file_or_zone_is_an_overlap() {
        let a = flight("OWL-1", Dispatched::At(10), false, &[zone("app/a.rs")]);
        let b = flight("OWL-2", Dispatched::At(20), true, &[zone("App/b.rs")]);
        let later = flight(
            "OWL-9",
            Dispatched::At(90),
            false,
            &[zone("app/c.rs"), zone("lib")],
        );
        let ticket = flight(
            "OWL-5",
            Dispatched::Now,
            false,
            &[zone("app"), zone("docs/x.md")],
        );
        let others = [later.clone(), b.clone(), a.clone()];
        assert_eq!(
            rebase(&ticket, &others),
            Weighed {
                collision: None,
                overlaps: vec![
                    shared("OWL-1", &["app/a.rs"]),
                    shared("OWL-2", &["App/b.rs"]),
                    shared("OWL-9", &["app/c.rs"]),
                ],
            }
        );
        let exclusive = weigh(&ticket, &others, Overlap::Exclusive);
        assert_eq!(exclusive.collision, collision(&ticket, &others));
        assert!(exclusive.collision.is_some() && exclusive.overlaps.is_empty());
        // A file inside the ticket's zone, or the zone both hold, once.
        let twin = flight(
            "OWL-3",
            Dispatched::At(5),
            false,
            &[zone("app"), zone("app/a.rs")],
        );
        assert_eq!(
            rebase(&ticket, std::slice::from_ref(&twin)).overlaps,
            [shared("OWL-3", &["app"])]
        );
        // A delivered ticket never waits, and still names its overlaps.
        let delivered = flight("OWL-4", Dispatched::At(1), true, &[zone("app/a.rs")]);
        assert_eq!(
            rebase(&delivered, std::slice::from_ref(&a)).overlaps,
            [shared("OWL-1", &["app/a.rs"])]
        );
    }

    /// OWL-449: what still excludes under `rebase`: a named resource, the
    /// whole repository on either side, and work outside Owlshift.
    #[test]
    fn under_rebase_named_resources_whole_holders_and_outside_work_exclude() {
        let named = Resource::Named("migrations".to_owned());
        let ticket = flight(
            "OWL-5",
            Dispatched::Now,
            false,
            &[zone("db/2.sql"), named.clone()],
        );
        let migrating = flight(
            "OWL-1",
            Dispatched::At(1),
            false,
            &[zone("db/1.sql"), named.clone()],
        );
        assert_eq!(
            rebase(&ticket, std::slice::from_ref(&migrating)).collision,
            Some(Collision {
                holder: "OWL-1",
                resource: named,
                sharing: None,
            })
        );
        let whole = flight("OWL-2", Dispatched::Unrecorded, false, &[zone("")]);
        let file = flight("OWL-6", Dispatched::Now, false, &[zone("app/a.rs")]);
        assert!(
            rebase(&file, std::slice::from_ref(&whole))
                .collision
                .is_some()
        );
        let holding = flight("OWL-3", Dispatched::At(1), false, &[zone("app/a.rs")]);
        let all = flight("OWL-7", Dispatched::Now, false, &[zone("")]);
        assert!(
            rebase(&all, std::slice::from_ref(&holding))
                .collision
                .is_some()
        );
        let person = InFlight {
            outside: true,
            ..flight("#7", Dispatched::Unrecorded, true, &[zone("app/a.rs")])
        };
        let found = rebase(&file, std::slice::from_ref(&person));
        assert_eq!(found.collision.map(|c| c.holder), Some("#7"));
        assert!(found.overlaps.is_empty());
        // Nothing waits for a ticket that comes after it.
        let late = flight("OWL-8", Dispatched::At(1), false, &[zone("app/a.rs")]);
        let early = flight("OWL-4", Dispatched::At(0), false, &[zone("app")]);
        assert_eq!(rebase(&early, std::slice::from_ref(&late)).collision, None);
    }

    /// OWL-449: no path is held by more than two Owlshift tickets, a zone
    /// holding every path it contains; the two that go before the third
    /// are named, the deepest shared path checked.
    #[test]
    fn under_rebase_a_third_holder_of_a_path_waits() {
        let bound = |holder, path: &str, sharing| {
            Some(Collision {
                holder,
                resource: zone(path),
                sharing: Some(sharing),
            })
        };
        let one = flight("OWL-1", Dispatched::At(1), false, &[zone("app/one.rs")]);
        let two = flight("OWL-2", Dispatched::At(2), false, &[zone("app/one.rs")]);
        let third = flight("OWL-3", Dispatched::Now, false, &[zone("app/one.rs")]);
        let found = rebase(&third, &[two.clone(), one.clone()]).collision;
        assert_eq!(found, bound("OWL-1", "app/one.rs", "OWL-2"));
        assert_eq!(found.unwrap().holders(), ["OWL-1", "OWL-2"]);
        // Zones held by two contain the third's file.
        let wide = [
            flight("OWL-1", Dispatched::At(1), false, &[zone("app")]),
            flight("OWL-2", Dispatched::At(2), false, &[zone("app")]),
        ];
        let inside = flight("OWL-3", Dispatched::Now, false, &[zone("app/z.rs")]);
        assert_eq!(
            rebase(&inside, &wide).collision,
            bound("OWL-1", "app/z.rs", "OWL-2")
        );
        // A path inside the third's zone that two hold, nested.
        let nested = [
            flight("OWL-1", Dispatched::At(1), false, &[zone("app/x")]),
            flight("OWL-2", Dispatched::At(2), false, &[zone("app/x/y.rs")]),
        ];
        let outer = flight("OWL-3", Dispatched::Now, false, &[zone("app")]);
        assert_eq!(
            rebase(&outer, &nested).collision,
            bound("OWL-1", "app/x/y.rs", "OWL-2")
        );
        // Two files of one zone are no path held by two.
        let apart = [
            flight("OWL-1", Dispatched::At(1), false, &[zone("app/a.rs")]),
            flight("OWL-2", Dispatched::At(2), false, &[zone("app/b.rs")]),
        ];
        assert_eq!(rebase(&outer, &apart).collision, None);
        // The second of two dispatched before a third goes on: the third
        // does not hold it back.
        let second = flight("OWL-2", Dispatched::At(2), false, &[zone("app/one.rs")]);
        let after = flight("OWL-9", Dispatched::At(9), false, &[zone("app/one.rs")]);
        assert_eq!(
            rebase(&second, &[one.clone(), after.clone()]).collision,
            None
        );
        assert!(rebase(&after, &[one, second]).collision.is_some());
    }

    /// OWL-449: a rebase's conflict lies on an overlap when one of its paths
    /// is, holds or lies in a shared path, in any case.
    #[test]
    fn a_conflict_on_an_overlap_is_read_by_containment() {
        let strings =
            |items: &[&str]| -> Vec<String> { items.iter().map(|p| (*p).to_owned()).collect() };
        let overlaps = strings(&["app/one.rs", "web"]);
        assert!(on_overlap(&strings(&["App/One.rs"]), &overlaps));
        assert!(on_overlap(&strings(&["docs/a.md", "web/x.ts"]), &overlaps));
        assert!(!on_overlap(&strings(&["app/two.rs"]), &overlaps));
        assert!(!on_overlap(&strings(&["app/two.rs"]), &[]));
        assert!(!on_overlap(&strings(&[""]), &overlaps));
    }
}
