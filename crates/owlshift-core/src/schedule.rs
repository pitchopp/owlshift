//! The scheduler: which admitted ticket of the ready column the drain
//! dispatches now, and why each other one waits (architecture, section 5,
//! "The drain"). Resource collisions come with parallelism (P6).

use std::cmp::Ordering;
use std::collections::BTreeSet;

use crate::state::{Status, TicketState};
use crate::ticket::{Admission, Blocker, BlockerState, Ticket};
use crate::vocab::Stage;

/// Limits on what may start.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Caps {
    /// Runs this machine runs at once, over every project (the operator's
    /// `concurrent_runs`).
    pub concurrent_runs: u32,
    /// Open pull requests awaiting review at which nothing new starts; `None`
    /// for no cap.
    pub open_prs: Option<u32>,
}

/// What is in flight now.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Load<K> {
    /// Runs in flight on this machine.
    pub runs: u32,
    /// Open pull requests awaiting review.
    pub open_prs: u32,
    /// The tickets whose ref holds the project's slot
    /// ([`holds_slot_or_unknown`]), in any order. [`ready_set`] adds those
    /// of the tickets it is given whose state holds it, once each.
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
    /// These tickets of the project hold the one slot, in identifier order,
    /// or this one takes it in this pass (OWL-291).
    Slot(Vec<K>),
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
            Wait::RunCap => "run_cap",
        }
    }

    /// The tickets the reason names: the blockers that hold the ticket, or
    /// the tickets that hold the slot; none for any other reason.
    pub fn tickets(&self) -> &[K] {
        match self {
            Wait::BlockerCanceled(ids)
            | Wait::BlockerUnknown(ids)
            | Wait::BlockedBy(ids)
            | Wait::Slot(ids) => ids,
            _ => &[],
        }
    }
}

/// The scheduler's answer: the ticket to dispatch now, and why each other
/// ticket waits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadySet<K> {
    /// At most one a pass. A ticket at Watch with its following ended is
    /// restarted from Ready, as `do` restarts it, before it is dispatched.
    pub dispatch: Option<K>,
    /// Every other ticket, in the order they would be dispatched.
    pub waiting: Vec<(K, Wait<K>)>,
}

/// Whether a ticket in this state holds the project's one slot: working a
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
/// every blocker but itself released it, the open pull requests are under their cap, the
/// slot is free and this machine has a run free.
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

    let pr_cap_reached = caps.open_prs.is_some_and(|cap| load.open_prs >= cap);
    // What holds back a ticket that passed every other check: the slot, then
    // this machine's runs. Once one ticket is dispatched, the slot is taken.
    // The holders the load names and those of the tickets given, which a
    // caller may name in both.
    let mut holders = load.slot_held_by;
    holders.extend(
        tickets
            .iter()
            .filter(|t| holds_slot(&t.state))
            .map(|t| t.id.clone()),
    );
    holders.sort_by(|a, b| natural_cmp(a.as_ref(), b.as_ref()).then_with(|| a.cmp(b)));
    holders.dedup();
    let mut capacity = if !holders.is_empty() {
        Some(Wait::Slot(holders))
    } else if load.runs >= caps.concurrent_runs {
        Some(Wait::RunCap)
    } else {
        None
    };
    let mut seen = BTreeSet::new();
    let mut ready = ReadySet {
        dispatch: None,
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
            } else if pr_cap_reached {
                Some(Wait::PrCap)
            } else {
                capacity.clone()
            }
        };
        match wait {
            Some(wait) => ready.waiting.push((ticket.id.clone(), wait)),
            None => {
                capacity = Some(Wait::Slot(vec![ticket.id.clone()]));
                ready.dispatch = Some(ticket.id.clone());
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
            resources: Vec::new(),
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
        assert_eq!(ready.dispatch, Some("OWL-3"));
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
            },
            Load {
                runs: 1,
                open_prs: 2,
                slot_held_by: vec!["Z-1"],
            },
        );
        assert_eq!(ready.dispatch, None);
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
        assert_eq!(ready.dispatch, Some("W-4"));
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
        assert_eq!(ready.dispatch, Some("B-1"));
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
        assert_eq!(ready.dispatch, None);
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
        assert_eq!(released.dispatch, Some("U-1"));
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
            assert_eq!(ready.dispatch, Some("S-1"), "{tracker:?}");
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
        assert_eq!(naming(vec![merged_self]).dispatch, Some("S-1"));
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
        assert_eq!(ready.dispatch, Some("A"));
        assert_eq!(ready.waiting, [("B", Wait::Slot(vec!["A"]))]);

        let held = Load {
            slot_held_by: vec!["X-9"],
            ..IDLE
        };
        let ready = ready_set(&tickets, ROOMY, held);
        assert_eq!(ready.dispatch, None);
        let by_x9 = || Wait::Slot(vec!["X-9"]);
        assert_eq!(ready.waiting, [("A", by_x9()), ("B", by_x9())]);

        // A ticket of the column at Build holds it, whatever the load says.
        let mut building = ticket("C", Priority::Urgent);
        building.state = restore(Status::Active(Stage::Build), 1);
        let ready = ready_set(&[building, tickets[1].clone()], ROOMY, IDLE);
        assert_eq!(ready.dispatch, None);
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
        assert_eq!(ready.dispatch, None);
        assert_eq!(ready.waiting[1], ("A", Wait::Slot(vec!["X-9", "X-10"])));
        assert_eq!(ready.waiting[1].1.tickets(), ["X-9", "X-10"]);
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
        };
        let busy = Load { runs: 2, ..IDLE };
        let ready = ready_set(&tickets, caps, busy);
        assert_eq!(ready.dispatch, None);
        assert_eq!(ready.waiting, [("A", Wait::RunCap), ("B", Wait::RunCap)]);
    }

    #[test]
    fn nothing_new_starts_at_the_open_pr_cap() {
        let tickets = [ticket("A", Priority::High)];
        let caps = Caps {
            concurrent_runs: 5,
            open_prs: Some(2),
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
        assert_eq!(ready_set(&tickets, caps, below).dispatch, Some("A"));
    }

    #[test]
    fn a_reason_names_its_key_and_only_blockers_and_the_slot_name_tickets() {
        assert_eq!(Wait::<&str>::NotReady.key(), "not_ready");
        assert!(Wait::<&str>::RunCap.tickets().is_empty());
        let slot = Wait::Slot(vec!["X-9"]);
        assert_eq!((slot.key(), slot.tickets()), ("slot", &["X-9"][..]));
        let held = Wait::BlockerUnknown(vec!["UTF-8"]);
        assert_eq!(
            (held.key(), held.tickets()),
            ("blocker_unknown", &["UTF-8"][..])
        );
        let open = Wait::BlockedBy(vec!["X-1", "X-2"]);
        assert_eq!(
            (open.key(), open.tickets()),
            ("blocked_by", &["X-1", "X-2"][..])
        );
    }

    #[test]
    fn an_id_is_dispatched_once() {
        let tickets = [ticket("A", Priority::Low), ticket("A", Priority::High)];
        let ready = ready_set(&tickets, ROOMY, IDLE);
        assert_eq!(ready.dispatch, Some("A"));
        assert_eq!(ready.waiting, [("A", Wait::Duplicate)]);
    }
}
