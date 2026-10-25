//! The scheduler: which admitted tickets of the ready column the drain
//! dispatches now, and why each other one waits (architecture, section 5,
//! "The drain"). Since OWL-401 the project's one slot is a capacity,
//! `policy.max_in_flight` ([`Caps::max_in_flight`]), the dispatch a list,
//! and the turn of the stage steps goes to the first holders by dispatch
//! time ([`slot_turn`]); [`admission`] is the check a dispatch makes again
//! under the project's lock (architecture, section 5, "Parallel", "Three
//! counts", "Admission, one transaction"). Since OWL-448 a dispatch checks
//! the caps alone: a ticket takes its resources at its Build's admission,
//! where one that would touch what a ticket in flight holds waits
//! ([`admissions`], [`collision`](crate::collision)), and a ready ticket
//! never waits for a collision.

use std::cmp::Ordering;
use std::collections::BTreeSet;

use crate::collision::{Collision, Dispatched, InFlight, weigh};
use crate::resource::Resource;
use crate::state::{Status, TicketState};
use crate::ticket::{Admission, Blocker, BlockerState, Ticket};
use crate::vocab::{Overlap, Stage};

/// Limits on what may start.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Caps {
    /// Runs this machine runs at once, over every project (the operator's
    /// `concurrent_runs`).
    pub concurrent_runs: u32,
    /// Open pull requests awaiting review at which nothing new starts; `None`
    /// for no cap.
    pub open_prs: Option<u32>,
    /// The project's capacity, `policy.max_in_flight` (OWL-401): the tickets
    /// whose ref holds the slot ([`holds_slot`]) at which no ticket is
    /// dispatched from Ready, and the most whose stage steps take the turn
    /// ([`slot_turn`]).
    pub max_in_flight: u32,
}

/// What is in flight now.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Load<K> {
    /// Runs in flight on this machine.
    pub runs: u32,
    /// Open pull requests awaiting review.
    pub open_prs: u32,
    /// The tickets whose ref holds the project's slot
    /// ([`holds_slot_or_unknown`]), in any order: as many as
    /// [`Caps::max_in_flight`] stop every dispatch. [`ready_set`] adds those
    /// of the tickets it is given whose state holds it, once each. A ticket
    /// counts here from its dispatch, admitted or not (OWL-448).
    pub slot_held_by: Vec<K>,
}

impl<K> Default for Load<K> {
    fn default() -> Self {
        Load {
            runs: 0,
            open_prs: 0,
            slot_held_by: Vec::new(),
        }
    }
}

/// Why an admitted ticket of the ready column is not dispatched. The variants
/// are in the order the scheduler checks them, the design's order; a ticket
/// gets the first that applies.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Wait<K> {
    /// Another ticket with the same id comes first; one id starts once.
    Duplicate,
    NotAdmitted,
    Excluded,
    /// No decider (architecture, section 4).
    NoDecider,
    /// Its ticket ref holds no intake record.
    NoIntake,
    /// Its intake record is for another text than the ticket's current one.
    StaleIntake,
    /// Its ticket ref is neither at Ready nor at Watch with its following
    /// ended: it works, waits for an answer, is parked, or its pull request is
    /// still followed.
    NotReady,
    /// These blockers were canceled, in identifier order; the ticket waits
    /// until a person removes the relation or the line. Checked before the
    /// open blockers, since nothing but a person ends this wait.
    BlockerCanceled(Vec<K>),
    /// The tracker knows no ticket by these identifiers, in identifier order;
    /// the ticket waits until a person corrects the line that names them.
    /// Checked before the open blockers, for the same reason.
    BlockerUnknown(Vec<K>),
    /// These blockers are neither completed nor merged yet, in identifier
    /// order.
    BlockedBy(Vec<K>),
    /// Open pull requests awaiting review reached their cap.
    PrCap,
    /// These tickets of the project hold its capacity
    /// ([`Caps::max_in_flight`]), in identifier order, those dispatched in
    /// this pass included (OWL-291, OWL-401).
    Slot(Vec<K>),
    /// What the ticket holds or its Build would take conflicts with what a
    /// ticket in flight holds (OWL-403): the holder and its resource. Never a
    /// ready ticket's reason since OWL-448: its stage steps' and its Build's
    /// admission's ([`admissions`]).
    Collision(Collision<K>),
    /// Every run slot of this machine is taken.
    RunCap,
}

impl<K> Wait<K> {
    /// The reason's name in the drain's `decision` events (`drain` =
    /// `waits`), which `owlshift why` reads (architecture, section 5, "Why a
    /// ticket waits").
    pub const fn key(&self) -> &'static str {
        match self {
            Wait::Duplicate => "duplicate",
            Wait::NotAdmitted => "not_admitted",
            Wait::Excluded => "excluded",
            Wait::NoDecider => "no_decider",
            Wait::NoIntake => "no_intake",
            Wait::StaleIntake => "stale_intake",
            Wait::NotReady => "not_ready",
            Wait::BlockerCanceled(_) => "blocker_canceled",
            Wait::BlockerUnknown(_) => "blocker_unknown",
            Wait::BlockedBy(_) => "blocked_by",
            Wait::PrCap => "pr_cap",
            Wait::Slot(_) => "slot",
            Wait::Collision(_) => "collision",
            Wait::RunCap => "run_cap",
        }
    }

    /// The tickets the reason names: the blockers that hold the ticket, the
    /// tickets that hold the project's capacity, or the holder it collides
    /// with, and under `rebase` the one sharing the path it would be the
    /// third holder of (OWL-449); none for any other reason.
    pub fn tickets(&self) -> Vec<K>
    where
        K: Clone,
    {
        match self {
            Wait::BlockerCanceled(ids)
            | Wait::BlockerUnknown(ids)
            | Wait::BlockedBy(ids)
            | Wait::Slot(ids) => ids.clone(),
            Wait::Collision(collision) => collision.holders(),
            _ => Vec::new(),
        }
    }

    /// The resource a collision names; none for any other reason.
    pub fn resource(&self) -> Option<&Resource> {
        match self {
            Wait::Collision(collision) => Some(&collision.resource),
            _ => None,
        }
    }
}

/// The scheduler's answer: the tickets to dispatch now, and why each other
/// ticket waits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadySet<K> {
    /// The tickets dispatched in this pass, in the order they are dispatched
    /// (OWL-401). A ticket at Watch with its following ended is restarted
    /// from Ready, as `do` restarts it, before it is dispatched.
    pub dispatch: Vec<K>,
    /// Every other ticket, in the order they would be dispatched.
    pub waiting: Vec<(K, Wait<K>)>,
}

/// Whether a ticket in this state holds the project's slot, one of its
/// capacity ([`Caps::max_in_flight`], OWL-401): working a
/// stage after Ready and before Watch, whatever dispatched it. Waiting for an
/// answer, parked, at Intake, at Ready or at Watch, it does not. A ticket
/// left at Build by a usage limit, a failed delivery or a refused push is
/// still working there, so it keeps the slot until a person, or `owlshift
/// watch` once a question round was answered, continues it.
pub fn holds_slot(state: &TicketState) -> bool {
    matches!(state.status(), Status::Active(stage) if stage > Stage::Ready && stage < Stage::Watch)
}

/// Whether a ticket ref whose state reads as `state` holds the slot: one
/// whose state does not read (`None`) counts as holding it, since nothing
/// tells it does not.
pub fn holds_slot_or_unknown(state: Option<&TicketState>) -> bool {
    state.is_none_or(holds_slot)
}

/// Whose turn the project's slot is for `owlshift watch`'s stage steps
/// (OWL-367; architecture section 5, "One step a pass", "Three counts"): a
/// stage step runs one stage of one ticket, so more ticket refs may hold the
/// slot between two passes than the project's capacity, and only the first
/// of them go on.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Turn<K> {
    /// These tickets' steps run, at most [`Caps::max_in_flight`], in the
    /// order they were dispatched; every other ticket whose ref holds the
    /// slot waits for them. None: no ticket ref holds the slot.
    Holders(Vec<K>),
    /// No holder can be chosen: these ticket refs cannot be read, in
    /// identifier order, and any of them may hold the slot.
    Unknown(Vec<K>),
}

impl<K: PartialEq> Turn<K> {
    /// Whether `ticket` holds the turn.
    pub fn has(&self, ticket: &K) -> bool {
        matches!(self, Turn::Holders(holders) if holders.contains(ticket))
    }

    /// Whether a ticket whose ref comes to hold the slot can take the turn
    /// now: fewer than `cap` hold it.
    pub fn room(&self, cap: u32) -> bool {
        matches!(self, Turn::Holders(holders) if holders.len() < cap as usize)
    }
}

/// The turn among `holding`, the tickets whose ref holds the slot
/// ([`holds_slot`]) with when each was dispatched, and `unread`, the ticket
/// refs that cannot be read, for a project whose capacity is `cap`
/// (OWL-401; architecture section 5, "Three counts"): an unread ref, which
/// may hold the slot, leaves no holder to choose; otherwise the first `cap`
/// holders by dispatch time take it, one with no dispatch time first, then
/// in identifier order ([`natural_cmp`]). A ticket that waits for a
/// collision gives up its turn: the caller leaves it out of `holding`.
pub fn slot_turn<K: Clone + Ord + AsRef<str>>(
    holding: &[(K, Dispatched)],
    unread: &[K],
    cap: u32,
) -> Turn<K> {
    if !unread.is_empty() {
        let mut ids = unread.to_vec();
        ids.sort_by(|a, b| natural_cmp(a.as_ref(), b.as_ref()).then_with(|| a.cmp(b)));
        ids.dedup();
        return Turn::Unknown(ids);
    }
    let mut holders = holding.to_vec();
    holders.sort_by(|(a, at_a), (b, at_b)| {
        at_a.cmp(at_b)
            .then_with(|| natural_cmp(a.as_ref(), b.as_ref()))
            .then_with(|| a.cmp(b))
    });
    let mut turn: Vec<K> = Vec::new();
    for (id, _) in holders {
        if turn.len() >= cap as usize {
            break;
        }
        if !turn.contains(&id) {
            turn.push(id);
        }
    }
    Turn::Holders(turn)
}

/// Why `ticket`, a dispatch from Ready, would wait, against the project's
/// `caps` and what `load` reads in flight; `None` when it may go (OWL-401;
/// architecture section 5, "Admission, one transaction"). In the design's
/// order: the open pull requests at their cap, then the project's capacity
/// held by as many tickets as `caps.max_in_flight` (the ticket's own ref
/// left out). Never what is held: since OWL-448 that is its Build's
/// admission's to check ([`admissions`]). This machine's runs are the ready
/// set's alone ([`ready_set`]): a step that admits holds its run slot
/// already.
pub fn admission<K: Clone + Ord + AsRef<str>>(
    ticket: &K,
    caps: Caps,
    load: &Load<K>,
) -> Option<Wait<K>> {
    if caps.open_prs.is_some_and(|cap| load.open_prs >= cap) {
        return Some(Wait::PrCap);
    }
    let mut holders: Vec<K> = load
        .slot_held_by
        .iter()
        .filter(|id| *id != ticket)
        .cloned()
        .collect();
    holders.sort_by(|a, b| natural_cmp(a.as_ref(), b.as_ref()).then_with(|| a.cmp(b)));
    holders.dedup();
    if holders.len() >= caps.max_in_flight as usize {
        return Some(Wait::Slot(holders));
    }
    None
}

/// A ticket whose Build's admission is due (OWL-448; architecture section
/// 5, "Resources admitted at Build").
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Due<K> {
    /// The ticket, its dispatch time and what it would hold once admitted.
    pub ticket: InFlight<K>,
    /// What its admission adds to what it holds already, which is what is
    /// checked: everything it would hold at a first admission, the new
    /// plan's files after a return to Design.
    pub added: Vec<Resource>,
    /// Whether its step may start now: one held back for another reason
    /// still waits or goes as the rule says, but holds nothing for those
    /// after it, since it takes nothing yet.
    pub starts: bool,
}

/// The Builds' admissions of one pass (OWL-448; architecture section 5,
/// "Resources admitted at Build"): each of `due`, the one dispatched first
/// first, then in identifier order ([`natural_cmp`]), waits when what it
/// adds conflicts with anything `in_flight` holds, whatever the holder's
/// dispatch time ([`weigh`] under the project's `overlap`, the ticket
/// weighed [`Dispatched::Now`]), so an admitted ticket never waits for one
/// admitted after it; one that goes and [`Due::starts`] holds, for those
/// after it, what it would hold once admitted, so that under `rebase` two
/// Builds of one pass never make a third holder of a path (OWL-449). Each
/// with its collision, `None` when it is admitted, in that order.
pub fn admissions<K: Clone + Ord + AsRef<str>>(
    due: &[Due<K>],
    in_flight: &[InFlight<K>],
    overlap: Overlap,
) -> Vec<(K, Option<Collision<K>>)> {
    let mut order: Vec<&Due<K>> = due.iter().collect();
    order.sort_by(|a, b| {
        a.ticket
            .dispatched
            .cmp(&b.ticket.dispatched)
            .then_with(|| natural_cmp(a.ticket.id.as_ref(), b.ticket.id.as_ref()))
            .then_with(|| a.ticket.id.cmp(&b.ticket.id))
    });
    let mut holders = in_flight.to_vec();
    let mut admitted = Vec::new();
    for due in order {
        let weighed = InFlight {
            id: due.ticket.id.clone(),
            dispatched: Dispatched::Now,
            delivered: false,
            outside: false,
            held: due.added.clone(),
        };
        let found = weigh(&weighed, &holders, overlap).collision;
        if found.is_none() && due.starts {
            holders.retain(|holder| holder.id != due.ticket.id);
            holders.push(due.ticket.clone());
        }
        admitted.push((due.ticket.id.clone(), found));
    }
    admitted
}

/// Orders identifiers naturally: runs of digits compare as numbers, so
/// `OWL-9` comes before `OWL-10`; identifiers equal as numbers fall back to
/// their text.
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    let (mut x, mut y) = (a.as_bytes(), b.as_bytes());
    loop {
        match (x.first(), y.first()) {
            (None, None) => return a.cmp(b),
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(p), Some(q)) if p.is_ascii_digit() && q.is_ascii_digit() => {
                let (dx, rx) = split_digits(x);
                let (dy, ry) = split_digits(y);
                let (tx, ty) = (trim_zeros(dx), trim_zeros(dy));
                let order = tx.len().cmp(&ty.len()).then_with(|| tx.cmp(ty));
                if order != Ordering::Equal {
                    return order;
                }
                (x, y) = (rx, ry);
            }
            (Some(p), Some(q)) => {
                if p != q {
                    return p.cmp(q);
                }
                (x, y) = (&x[1..], &y[1..]);
            }
        }
    }
}

fn split_digits(bytes: &[u8]) -> (&[u8], &[u8]) {
    let n = bytes.iter().take_while(|b| b.is_ascii_digit()).count();
    bytes.split_at(n)
}

fn trim_zeros(digits: &[u8]) -> &[u8] {
    let zeros = digits.iter().take_while(|&&b| b == b'0').count();
    &digits[zeros..]
}

/// The blockers that still hold a ticket, each list in identifier order.
struct Holding<K> {
    canceled: Vec<K>,
    unknown: Vec<K>,
    open: Vec<K>,
}

/// The blockers that still hold the ticket `own`, merged by identifier: the
/// ticket's own identifier is ignored, since a ticket is never its own
/// prerequisite; one released copy releases the identifier; else one canceled
/// copy makes it canceled, and one copy the tracker knows makes it open; it is
/// unknown only when the tracker knows no copy.
fn holding<K: Clone + Ord + AsRef<str>>(own: &K, blockers: &[Blocker<K>]) -> Holding<K> {
    let mut ids: Vec<&K> = blockers
        .iter()
        .map(|b| &b.id)
        .filter(|id| *id != own)
        .collect();
    ids.sort_by(|a, b| natural_cmp(a.as_ref(), b.as_ref()).then_with(|| a.cmp(b)));
    ids.dedup();
    let mut held = Holding {
        canceled: Vec::new(),
        unknown: Vec::new(),
        open: Vec::new(),
    };
    for id in ids {
        let copies = || blockers.iter().filter(move |b| &b.id == id);
        if copies().any(Blocker::released) {
            continue;
        }
        if copies().any(|b| b.tracker == BlockerState::Canceled) {
            held.canceled.push(id.clone());
        } else if copies().all(|b| b.tracker == BlockerState::Unknown) {
            held.unknown.push(id.clone());
        } else {
            held.open.push(id.clone());
        }
    }
    held
}

/// Whether a ticket's ref lets the drain dispatch it: at Ready, or at Watch
/// with its following ended, which `do` would restart from Ready too.
fn dispatchable<K>(ticket: &Ticket<K>) -> bool {
    match ticket.state.status() {
        Status::Active(Stage::Ready) => true,
        Status::Active(Stage::Watch) => !ticket.delivery_followed,
        _ => false,
    }
}

/// Computes the ready set over the admitted tickets of the ready column.
///
/// Every ticket given is either dispatched or waits with one reason. They
/// are taken by priority, most urgent first and none last, then the first
/// created, then the lower identifier ([`natural_cmp`]); with equal ids, the
/// first in this order is the one kept. A ticket is dispatched when it is
/// admitted and not excluded, has a decider, its intake record holds its
/// current text, its ref is at Ready (or at Watch with its following ended),
/// every blocker but itself released it, [`admission`] lets it go against
/// `load` and the tickets dispatched before it in this pass (the open pull
/// requests under their cap, the project's capacity not held), and this
/// machine has a run free once those dispatched before it took theirs
/// (OWL-401). What is held never holds it back: a ticket takes its
/// resources at its Build's admission (OWL-448).
pub fn ready_set<K: Clone + Ord + AsRef<str>>(
    tickets: &[Ticket<K>],
    caps: Caps,
    load: Load<K>,
) -> ReadySet<K> {
    let mut candidates: Vec<&Ticket<K>> = tickets.iter().collect();
    candidates.sort_by(|a, b| {
        a.priority
            .cmp(&b.priority)
            .then(a.created_ms.cmp(&b.created_ms))
            .then_with(|| natural_cmp(a.id.as_ref(), b.id.as_ref()))
    });

    // What holds back a ticket that passed every other check, as the load
    // and the tickets dispatched before it in this pass hold it: the holders
    // the load names and those of the tickets given, which a caller may name
    // in both.
    let mut load = load;
    load.slot_held_by.extend(
        tickets
            .iter()
            .filter(|t| holds_slot(&t.state))
            .map(|t| t.id.clone()),
    );
    let mut seen = BTreeSet::new();
    let mut ready = ReadySet {
        dispatch: Vec::new(),
        waiting: Vec::new(),
    };
    for ticket in candidates {
        let wait = if !seen.insert(&ticket.id) {
            Some(Wait::Duplicate)
        } else if ticket.admission == Admission::NotAdmitted {
            Some(Wait::NotAdmitted)
        } else if ticket.admission == Admission::Excluded {
            Some(Wait::Excluded)
        } else if !ticket.has_decider {
            Some(Wait::NoDecider)
        } else if ticket.intake_hash.is_none() {
            Some(Wait::NoIntake)
        } else if ticket.intake_hash.as_deref() != Some(ticket.text_hash.as_str()) {
            Some(Wait::StaleIntake)
        } else if !dispatchable(ticket) {
            Some(Wait::NotReady)
        } else {
            let held = holding(&ticket.id, &ticket.blocked_by);
            if !held.canceled.is_empty() {
                Some(Wait::BlockerCanceled(held.canceled))
            } else if !held.unknown.is_empty() {
                Some(Wait::BlockerUnknown(held.unknown))
            } else if !held.open.is_empty() {
                Some(Wait::BlockedBy(held.open))
            } else if let Some(wait) = admission(&ticket.id, caps, &load) {
                Some(wait)
            } else if load.runs >= caps.concurrent_runs {
                Some(Wait::RunCap)
            } else {
                None
            }
        };
        match wait {
            Some(wait) => ready.waiting.push((ticket.id.clone(), wait)),
            None => {
                // Dispatched: for every ticket after it in this pass, it
                // holds the slot and one of this machine's runs.
                load.slot_held_by.push(ticket.id.clone());
                load.runs = load.runs.saturating_add(1);
                ready.dispatch.push(ticket.id.clone());
            }
        }
    }
    ready
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vocab::{Priority, Variant};

    const ROOMY: Caps = Caps {
        concurrent_runs: 10,
        open_prs: None,
        max_in_flight: 1,
    };
    const IDLE: Load<&'static str> = Load {
        runs: 0,
        open_prs: 0,
        slot_held_by: Vec::new(),
    };

    fn restore(status: Status, round: u32) -> TicketState {
        TicketState::restore(status, round, 0, 0, 0, 0).unwrap()
    }

    fn ticket(id: &'static str, priority: Priority) -> Ticket<&'static str> {
        Ticket {
            id,
            priority,
            created_ms: 0,
            admission: Admission::Admitted,
            has_decider: true,
            text_hash: "h1".to_owned(),
            intake_hash: Some("h1".to_owned()),
            blocked_by: Vec::new(),
            variant: Variant::Standard,
            state: restore(Status::Active(Stage::Ready), 0),
            delivery_followed: false,
        }
    }

    fn blocker(id: &'static str, tracker: BlockerState) -> Blocker<&'static str> {
        Blocker {
            id,
            tracker,
            merged: false,
        }
    }

    #[test]
    fn priority_then_creation_then_identifier_order_the_tickets() {
        let created = |id, priority, created_ms| Ticket {
            created_ms,
            ..ticket(id, priority)
        };
        let tickets = [
            created("OWL-1", Priority::Unset, 0),
            created("OWL-2", Priority::Low, 0),
            created("OWL-10", Priority::High, 5),
            created("OWL-11", Priority::High, 3),
            created("OWL-9", Priority::High, 5),
            created("OWL-3", Priority::Urgent, 9),
        ];
        let ready = ready_set(&tickets, ROOMY, IDLE);
        assert_eq!(ready.dispatch, ["OWL-3"]);
        let order: Vec<_> = ready.waiting.iter().map(|(id, _)| *id).collect();
        assert_eq!(order, ["OWL-11", "OWL-9", "OWL-10", "OWL-2", "OWL-1"]);
        assert!(
            ready
                .waiting
                .iter()
                .all(|(_, w)| *w == Wait::Slot(vec!["OWL-3"]))
        );
    }

    #[test]
    fn natural_order_compares_digit_runs_as_numbers() {
        assert_eq!(natural_cmp("OWL-9", "OWL-10"), Ordering::Less);
        assert_eq!(natural_cmp("OWL-10", "OWL-9"), Ordering::Greater);
        assert_eq!(natural_cmp("ABC-1", "OWL-1"), Ordering::Less);
        assert_eq!(natural_cmp("OWL-9", "OWL-09"), "OWL-9".cmp("OWL-09"));
        assert_eq!(
            natural_cmp(
                "OWL-99999999999999999999999",
                "OWL-100000000000000000000000"
            ),
            Ordering::Less
        );
    }

    #[test]
    fn each_ticket_gets_the_first_reason_in_the_designs_order() {
        let with = |id, f: &dyn Fn(&mut Ticket<&'static str>)| {
            let mut t = ticket(id, Priority::Medium);
            f(&mut t);
            t
        };
        let open = |t: &mut Ticket<_>| t.blocked_by = vec![blocker("X-1", BlockerState::Open)];
        let tickets = [
            with("A-1", &|t| {
                t.admission = Admission::NotAdmitted;
                t.has_decider = false;
            }),
            with("A-2", &|t| {
                t.admission = Admission::Excluded;
                open(t);
            }),
            with("A-3", &|t| {
                t.has_decider = false;
                t.intake_hash = None;
            }),
            with("A-4", &|t| {
                t.intake_hash = None;
                t.state = TicketState::admitted();
            }),
            with("A-5", &|t| {
                t.intake_hash = Some("h0".to_owned());
                t.state = restore(Status::Active(Stage::Build), 1);
            }),
            with("A-6", &|t| {
                t.state = restore(
                    Status::NeedsInput {
                        return_to: Stage::Ready,
                    },
                    1,
                );
                open(t);
            }),
            with("A-7", &|t| {
                t.blocked_by = vec![
                    blocker("X-2", BlockerState::Open),
                    blocker("X-10", BlockerState::Canceled),
                    blocker("X-1", BlockerState::Canceled),
                ];
            }),
            with("A-8", &|t| {
                t.blocked_by = vec![
                    blocker("X-1", BlockerState::Open),
                    blocker("UTF-8", BlockerState::Unknown),
                ];
            }),
            with("A-9", &open),
            with("A-10", &|_| {}),
        ];
        let ready = ready_set(
            &tickets,
            Caps {
                concurrent_runs: 1,
                open_prs: Some(2),
                max_in_flight: 1,
            },
            Load {
                runs: 1,
                open_prs: 2,
                slot_held_by: vec!["Z-1"],
            },
        );
        assert_eq!(ready.dispatch, Vec::<&str>::new());
        assert_eq!(
            ready.waiting,
            [
                ("A-1", Wait::NotAdmitted),
                ("A-2", Wait::Excluded),
                ("A-3", Wait::NoDecider),
                ("A-4", Wait::NoIntake),
                ("A-5", Wait::StaleIntake),
                ("A-6", Wait::NotReady),
                ("A-7", Wait::BlockerCanceled(vec!["X-1", "X-10"])),
                ("A-8", Wait::BlockerUnknown(vec!["UTF-8"])),
                ("A-9", Wait::BlockedBy(vec!["X-1"])),
                ("A-10", Wait::PrCap),
            ]
        );
    }

    #[test]
    fn a_ref_not_at_ready_waits_unless_its_following_ended() {
        let at = |id, status, round| Ticket {
            state: restore(status, round),
            ..ticket(id, Priority::Medium)
        };
        let parked = Status::Parked {
            at: Stage::Build,
            awaiting_input: false,
        };
        let mut followed = at("W-1", Status::Active(Stage::Watch), 1);
        followed.delivery_followed = true;
        let tickets = [
            followed,
            at("W-2", parked, 1),
            at("W-3", Status::Active(Stage::Intake), 0),
            at("W-4", Status::Active(Stage::Watch), 1),
        ];
        let ready = ready_set(&tickets, ROOMY, IDLE);
        assert_eq!(ready.dispatch, ["W-4"]);
        assert_eq!(
            ready.waiting,
            [
                ("W-1", Wait::NotReady),
                ("W-2", Wait::NotReady),
                ("W-3", Wait::NotReady),
            ]
        );
    }

    #[test]
    fn a_completed_or_merged_blocker_releases_a_canceled_one_does_not() {
        let mut released = ticket("B-1", Priority::High);
        released.blocked_by = vec![
            blocker("X-1", BlockerState::Completed),
            Blocker {
                merged: true,
                ..blocker("X-2", BlockerState::Canceled)
            },
            // The relation and the text name X-3; the tracker shows it done.
            blocker("X-3", BlockerState::Open),
            blocker("X-3", BlockerState::Completed),
        ];
        let mut canceled = ticket("B-2", Priority::Urgent);
        canceled.blocked_by = vec![blocker("X-4", BlockerState::Canceled)];
        let ready = ready_set(&[released, canceled], ROOMY, IDLE);
        assert_eq!(ready.dispatch, ["B-1"]);
        assert_eq!(ready.waiting, [("B-2", Wait::BlockerCanceled(vec!["X-4"]))]);
    }

    #[test]
    fn an_unknown_blocker_holds_the_ticket_and_names_it() {
        let held = |blocked_by| {
            let mut t = ticket("U-1", Priority::High);
            t.blocked_by = blocked_by;
            ready_set(&[t], ROOMY, IDLE)
        };
        // "Blocked by OWL-1 (not UTF-8 related)", OWL-1 done.
        let ready = held(vec![
            blocker("OWL-1", BlockerState::Completed),
            blocker("UTF-8", BlockerState::Unknown),
        ]);
        assert_eq!(ready.dispatch, Vec::<&str>::new());
        assert_eq!(
            ready.waiting,
            [("U-1", Wait::BlockerUnknown(vec!["UTF-8"]))]
        );
        // A copy the tracker knows decides: open, canceled or released.
        let open = held(vec![
            blocker("X-1", BlockerState::Unknown),
            blocker("X-1", BlockerState::Open),
        ]);
        assert_eq!(open.waiting, [("U-1", Wait::BlockedBy(vec!["X-1"]))]);
        let canceled = held(vec![
            blocker("X-1", BlockerState::Unknown),
            blocker("X-1", BlockerState::Canceled),
        ]);
        assert_eq!(
            canceled.waiting,
            [("U-1", Wait::BlockerCanceled(vec!["X-1"]))]
        );
        let released = held(vec![
            blocker("X-1", BlockerState::Unknown),
            blocker("X-1", BlockerState::Completed),
        ]);
        assert_eq!(released.dispatch, ["U-1"]);
    }

    #[test]
    fn a_ticket_naming_itself_is_not_held_by_it() {
        let naming = |blocked_by| {
            let mut t = ticket("S-1", Priority::High);
            t.blocked_by = blocked_by;
            ready_set(&[t], ROOMY, IDLE)
        };
        for tracker in [
            BlockerState::Open,
            BlockerState::Canceled,
            BlockerState::Unknown,
            BlockerState::Completed,
        ] {
            let ready = naming(vec![blocker("S-1", tracker)]);
            assert_eq!(ready.dispatch, ["S-1"], "{tracker:?}");
            let ready = naming(vec![
                blocker("S-1", tracker),
                blocker("X-1", BlockerState::Open),
            ]);
            assert_eq!(
                ready.waiting,
                [("S-1", Wait::BlockedBy(vec!["X-1"]))],
                "{tracker:?}"
            );
        }
        let merged_self = Blocker {
            merged: true,
            ..blocker("S-1", BlockerState::Canceled)
        };
        assert_eq!(naming(vec![merged_self]).dispatch, ["S-1"]);
    }

    #[test]
    fn the_slot_is_held_only_while_a_stage_after_ready_and_before_watch_works() {
        let holders = [
            Stage::Design,
            Stage::DesignReview,
            Stage::Build,
            Stage::Verify,
            Stage::Deliver,
        ];
        for stage in Stage::ALL {
            let round = u32::from(stage != Stage::Intake);
            assert_eq!(
                holds_slot(&restore(Status::Active(stage), round)),
                holders.contains(&stage),
                "{stage:?}"
            );
            let waiting = Status::NeedsInput { return_to: stage };
            if let Ok(state) = TicketState::restore(waiting, 1, 0, 0, 0, 0) {
                assert!(!holds_slot(&state), "{waiting:?}");
            }
            let parked = Status::Parked {
                at: stage,
                awaiting_input: false,
            };
            if let Ok(state) = TicketState::restore(parked, round, 0, 0, 0, 0) {
                assert!(!holds_slot(&state), "{parked:?}");
            }
        }
    }

    #[test]
    fn one_ticket_takes_a_free_slot_and_a_held_one_stops_all() {
        let tickets = [ticket("A", Priority::High), ticket("B", Priority::Low)];
        let ready = ready_set(&tickets, ROOMY, IDLE);
        assert_eq!(ready.dispatch, ["A"]);
        assert_eq!(ready.waiting, [("B", Wait::Slot(vec!["A"]))]);

        let held = Load {
            slot_held_by: vec!["X-9"],
            ..IDLE
        };
        let ready = ready_set(&tickets, ROOMY, held);
        assert_eq!(ready.dispatch, Vec::<&str>::new());
        let by_x9 = || Wait::Slot(vec!["X-9"]);
        assert_eq!(ready.waiting, [("A", by_x9()), ("B", by_x9())]);

        // A ticket of the column at Build holds it, whatever the load says.
        let mut building = ticket("C", Priority::Urgent);
        building.state = restore(Status::Active(Stage::Build), 1);
        let ready = ready_set(&[building, tickets[1].clone()], ROOMY, IDLE);
        assert_eq!(ready.dispatch, Vec::<&str>::new());
        assert_eq!(
            ready.waiting,
            [("C", Wait::NotReady), ("B", Wait::Slot(vec!["C"]))]
        );
    }

    /// OWL-291: the holders the load names and those of the column, once
    /// each, in identifier order.
    #[test]
    fn a_held_slot_names_its_holders_once_in_identifier_order() {
        let mut verifying = ticket("X-9", Priority::Urgent);
        verifying.state = restore(Status::Active(Stage::Verify), 1);
        let load = Load {
            slot_held_by: vec!["X-10", "X-9", "X-10"],
            ..IDLE
        };
        let ready = ready_set(&[verifying, ticket("A", Priority::Low)], ROOMY, load);
        assert_eq!(ready.dispatch, Vec::<&str>::new());
        assert_eq!(ready.waiting[1], ("A", Wait::Slot(vec!["X-9", "X-10"])));
        assert_eq!(ready.waiting[1].1.tickets(), ["X-9", "X-10"]);
    }

    /// OWL-401: the turn goes to the first `cap` holders by dispatch time,
    /// one with none first, then in identifier order; an unread ref blocks
    /// the choice.
    #[test]
    fn the_turn_goes_to_the_first_holders_by_dispatch_time() {
        let holding = [
            ("X-10", Dispatched::At(30)),
            ("X-9", Dispatched::At(20)),
            ("X-2", Dispatched::Unrecorded),
            ("X-1", Dispatched::At(20)),
        ];
        let turn = slot_turn(&holding, &[], 1);
        assert_eq!(turn, Turn::Holders(vec!["X-2"]));
        assert!(turn.has(&"X-2") && !turn.has(&"X-1") && !turn.room(1));
        let turn = slot_turn(&holding, &[], 3);
        assert_eq!(turn, Turn::Holders(vec!["X-2", "X-1", "X-9"]));
        assert!(!turn.has(&"X-10") && turn.room(4));
        assert_eq!(
            slot_turn(&holding, &["Z-2", "Z-10", "Z-2"], 3),
            Turn::Unknown(vec!["Z-2", "Z-10"])
        );
        assert!(!Turn::Unknown(vec!["Z-2"]).room(3));
        assert_eq!(slot_turn::<&str>(&[], &[], 2), Turn::Holders(Vec::new()));
    }

    /// OWL-401: a pass dispatches while the project's capacity, its open
    /// pull requests and this machine's runs allow, each ticket dispatched
    /// holding the capacity and a run against those after it. Since
    /// OWL-448 nothing it would touch holds it back.
    #[test]
    fn a_pass_dispatches_up_to_the_capacity_and_the_runs() {
        let tickets = [
            ticket("A-9", Priority::Urgent),
            ticket("A-1", Priority::High),
            ticket("A-2", Priority::Medium),
            ticket("A-3", Priority::Low),
        ];
        let caps = |max_in_flight, concurrent_runs| Caps {
            concurrent_runs,
            open_prs: None,
            max_in_flight,
        };
        let held = Load {
            slot_held_by: vec!["X-1"],
            ..IDLE
        };
        let ready = ready_set(&tickets, caps(3, 10), held.clone());
        assert_eq!(ready.dispatch, ["A-9", "A-1"]);
        let capacity = Wait::Slot(vec!["A-1", "A-9", "X-1"]);
        assert_eq!(
            ready.waiting,
            [("A-2", capacity.clone()), ("A-3", capacity)]
        );
        // This machine's runs, those of the pass included.
        let busy = Load { runs: 8, ..held };
        let ready = ready_set(&tickets, caps(10, 10), busy);
        assert_eq!(ready.dispatch, ["A-9", "A-1"]);
        assert_eq!(
            ready.waiting,
            [("A-2", Wait::RunCap), ("A-3", Wait::RunCap)]
        );
    }

    /// OWL-401, OWL-448: the check a dispatch makes again under the lock:
    /// the open pull requests, then the capacity without the ticket's own
    /// ref, a ticket dispatched and not admitted counting in it; never what
    /// is held, nor this machine's runs.
    #[test]
    fn the_admission_checks_the_cap_then_the_capacity() {
        let caps = Caps {
            concurrent_runs: 1,
            open_prs: Some(2),
            max_in_flight: 2,
        };
        let load = Load {
            runs: 1,
            open_prs: 1,
            slot_held_by: vec!["A-1", "B-1"],
        };
        assert_eq!(admission(&"A-1", caps, &load), None);
        let full = Load {
            slot_held_by: vec!["C-1", "B-1", "A-1"],
            ..load.clone()
        };
        assert_eq!(
            admission(&"A-1", caps, &full),
            Some(Wait::Slot(vec!["B-1", "C-1"]))
        );
        let capped = Load {
            open_prs: 2,
            ..full
        };
        assert_eq!(admission(&"A-1", caps, &capped), Some(Wait::PrCap));
    }

    #[test]
    fn a_state_that_does_not_read_holds_the_slot() {
        assert!(holds_slot_or_unknown(None));
        let verify = restore(Status::Active(Stage::Verify), 1);
        assert!(holds_slot_or_unknown(Some(&verify)));
        let ready = restore(Status::Active(Stage::Ready), 0);
        assert!(!holds_slot_or_unknown(Some(&ready)));
    }

    #[test]
    fn a_full_machine_holds_every_ticket_with_a_free_slot() {
        let tickets = [ticket("A", Priority::High), ticket("B", Priority::Low)];
        let caps = Caps {
            concurrent_runs: 2,
            open_prs: None,
            max_in_flight: 1,
        };
        let busy = Load { runs: 2, ..IDLE };
        let ready = ready_set(&tickets, caps, busy);
        assert_eq!(ready.dispatch, Vec::<&str>::new());
        assert_eq!(ready.waiting, [("A", Wait::RunCap), ("B", Wait::RunCap)]);
    }

    #[test]
    fn nothing_new_starts_at_the_open_pr_cap() {
        let tickets = [ticket("A", Priority::High)];
        let caps = Caps {
            concurrent_runs: 5,
            open_prs: Some(2),
            max_in_flight: 1,
        };
        let at_cap = Load {
            open_prs: 2,
            slot_held_by: vec!["X-1"],
            ..IDLE
        };
        assert_eq!(
            ready_set(&tickets, caps, at_cap).waiting,
            [("A", Wait::PrCap)]
        );
        let below = Load {
            open_prs: 1,
            ..IDLE
        };
        assert_eq!(ready_set(&tickets, caps, below).dispatch, ["A"]);
    }

    #[test]
    fn a_reason_names_its_key_and_only_blockers_and_the_slot_name_tickets() {
        assert_eq!(Wait::<&str>::NotReady.key(), "not_ready");
        assert!(Wait::<&str>::RunCap.tickets().is_empty());
        let slot = Wait::Slot(vec!["X-9"]);
        assert_eq!((slot.key(), slot.tickets()), ("slot", vec!["X-9"]));
        let held = Wait::BlockerUnknown(vec!["UTF-8"]);
        assert_eq!(
            (held.key(), held.tickets()),
            ("blocker_unknown", vec!["UTF-8"])
        );
        let open = Wait::BlockedBy(vec!["X-1", "X-2"]);
        assert_eq!(
            (open.key(), open.tickets()),
            ("blocked_by", vec!["X-1", "X-2"])
        );
    }

    /// OWL-448: the Builds' admissions of a pass. A Build waits for any
    /// holder of what it adds, whatever the holder's dispatch time, so an
    /// admitted ticket never waits for one admitted after it; among Builds
    /// due together the one dispatched first goes first, whatever the
    /// identifiers, and holds for the next one, unless its step cannot start
    /// now. What a ticket holds already is never checked again.
    #[test]
    fn builds_are_admitted_in_dispatch_order_against_any_holder() {
        let zone = |path: &str| Resource::Zone(path.to_owned());
        let flight = |id, at, held: &[Resource]| InFlight {
            id,
            dispatched: Dispatched::At(at),
            delivered: false,
            outside: false,
            held: held.to_vec(),
        };
        let due = |id, at, wanted: &[Resource], starts| Due {
            ticket: flight(id, at, wanted),
            added: wanted.to_vec(),
            starts,
        };
        // H-1 was dispatched after both Builds but admitted before them.
        let in_flight = [flight("H-1", 50, &[zone("lib/a.rs")])];
        let builds = [
            due("B-1", 20, &[zone("app/one.rs")], true),
            due("B-9", 10, &[zone("app/one.rs")], true),
            due("B-3", 30, &[zone("lib")], true),
            due("B-4", 40, &[zone("docs/x.md")], true),
        ];
        let collided = |holder, path: &str| {
            Some(Collision {
                holder,
                resource: zone(path),
                sharing: None,
            })
        };
        assert_eq!(
            admissions(&builds, &in_flight, Overlap::Exclusive),
            [
                ("B-9", None),
                ("B-1", collided("B-9", "app/one.rs")),
                ("B-3", collided("H-1", "lib/a.rs")),
                ("B-4", None),
            ]
        );
        // B-9 held back from starting holds nothing for B-1.
        let mut held_back = builds.clone();
        held_back[1].starts = false;
        assert_eq!(
            admissions(&held_back, &in_flight, Overlap::Exclusive)[1],
            ("B-1", None)
        );
        // A return to Design's admission checks only what it adds.
        let returning = Due {
            ticket: flight("B-5", 5, &[zone("lib"), zone("web/a.rs")]),
            added: vec![zone("web/a.rs")],
            starts: true,
        };
        assert_eq!(
            admissions(&[returning], &in_flight, Overlap::Exclusive),
            [("B-5", None)]
        );
        let collision = Wait::Collision(collided("H-1", "lib/a.rs").unwrap());
        assert_eq!(
            (collision.key(), collision.tickets(), collision.resource()),
            ("collision", vec!["H-1"], Some(&zone("lib/a.rs")))
        );
    }

    /// OWL-449: under `rebase`, two Builds of one pass that plan the same
    /// file are both admitted, and the third waits, naming both.
    #[test]
    fn under_rebase_two_builds_of_a_pass_share_a_file_and_a_third_waits() {
        let file = Resource::Zone("app/one.rs".to_owned());
        let due = |id, at| Due {
            ticket: InFlight {
                id,
                dispatched: Dispatched::At(at),
                delivered: false,
                outside: false,
                held: vec![file.clone()],
            },
            added: vec![file.clone()],
            starts: true,
        };
        let builds = [due("B-3", 30), due("B-1", 10), due("B-2", 20)];
        assert_eq!(
            admissions(&builds, &[], Overlap::Rebase),
            [
                ("B-1", None),
                ("B-2", None),
                (
                    "B-3",
                    Some(Collision {
                        holder: "B-1",
                        resource: file.clone(),
                        sharing: Some("B-2"),
                    })
                ),
            ]
        );
        let exclusive = admissions(&builds, &[], Overlap::Exclusive);
        assert!(exclusive[1].1.is_some() && exclusive[2].1.is_some());
    }

    #[test]
    fn an_id_is_dispatched_once() {
        let tickets = [ticket("A", Priority::Low), ticket("A", Priority::High)];
        let ready = ready_set(&tickets, ROOMY, IDLE);
        assert_eq!(ready.dispatch, ["A"]);
        assert_eq!(ready.waiting, [("A", Wait::Duplicate)]);
    }
}
