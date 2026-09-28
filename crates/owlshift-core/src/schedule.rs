//! The scheduler skeleton: which ready tickets may start now.
//!
//! The ready set follows admission, blockers and caps. Resource collisions
//! (P6) and intake runs (P4) come later.

use std::collections::BTreeSet;

use crate::state::Status;
use crate::ticket::{Admission, Ticket};
use crate::vocab::Stage;

/// Limits on what may start.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Caps {
    /// Runs this machine runs at once.
    pub concurrent_runs: u32,
    /// Open pull requests awaiting review at which nothing new starts; `None`
    /// for no cap.
    pub open_prs: Option<u32>,
}

/// What is in flight now.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Load {
    /// Runs in flight on this machine.
    pub runs: u32,
    /// Open pull requests awaiting review.
    pub open_prs: u32,
}

/// Why a ready ticket does not start. The variants are in the order the
/// scheduler checks them; a ticket gets the first that applies.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Wait<K> {
    /// Another ticket with the same id comes first; one id starts once.
    Duplicate,
    NotAdmitted,
    Excluded,
    /// These blockers are not done yet, in the ticket's order.
    BlockedBy(Vec<K>),
    /// Open pull requests awaiting review reached their cap.
    PrCap,
    /// Every run slot is taken.
    RunCap,
}

/// The scheduler's answer: the tickets to dispatch now, and why each other
/// ready ticket waits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadySet<K> {
    /// In the order to dispatch them.
    pub dispatch: Vec<K>,
    /// Every other ticket at Ready, in the same order.
    pub waiting: Vec<(K, Wait<K>)>,
}

/// Computes the ready set.
///
/// Only tickets at Ready are candidates; the others appear in neither list.
/// Candidates are taken by priority, most urgent first; tickets of equal
/// priority keep their order in `tickets` (the tracker's order).
pub fn ready_set<K: Clone + Ord>(tickets: &[Ticket<K>], caps: Caps, load: Load) -> ReadySet<K> {
    let mut candidates: Vec<&Ticket<K>> = tickets
        .iter()
        .filter(|t| t.state.status() == Status::Active(Stage::Ready))
        .collect();
    candidates.sort_by_key(|t| t.priority);

    let pr_cap_reached = caps.open_prs.is_some_and(|cap| load.open_prs >= cap);
    let mut free_runs = caps.concurrent_runs.saturating_sub(load.runs);
    let mut seen = BTreeSet::new();
    let mut ready = ReadySet {
        dispatch: Vec::new(),
        waiting: Vec::new(),
    };

    for ticket in candidates {
        let open_blockers: Vec<K> = ticket
            .blocked_by
            .iter()
            .filter(|b| !b.done)
            .map(|b| b.id.clone())
            .collect();
        let wait = if !seen.insert(&ticket.id) {
            Some(Wait::Duplicate)
        } else if ticket.admission == Admission::NotAdmitted {
            Some(Wait::NotAdmitted)
        } else if ticket.admission == Admission::Excluded {
            Some(Wait::Excluded)
        } else if !open_blockers.is_empty() {
            Some(Wait::BlockedBy(open_blockers))
        } else if pr_cap_reached {
            Some(Wait::PrCap)
        } else if free_runs == 0 {
            Some(Wait::RunCap)
        } else {
            None
        };
        match wait {
            Some(wait) => ready.waiting.push((ticket.id.clone(), wait)),
            None => {
                free_runs -= 1;
                ready.dispatch.push(ticket.id.clone());
            }
        }
    }
    ready
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::TicketState;
    use crate::ticket::Blocker;
    use crate::vocab::{Priority, Variant};

    const ROOMY: Caps = Caps {
        concurrent_runs: 10,
        open_prs: None,
    };
    const IDLE: Load = Load {
        runs: 0,
        open_prs: 0,
    };

    fn ready_state() -> TicketState {
        TicketState::restore(Status::Active(Stage::Ready), 0, 0, 0).unwrap()
    }

    fn ticket(id: &'static str, priority: Priority) -> Ticket<&'static str> {
        Ticket {
            id,
            priority,
            admission: Admission::Admitted,
            blocked_by: Vec::new(),
            variant: Variant::Standard,
            resources: Vec::new(),
            state: ready_state(),
        }
    }

    #[test]
    fn only_tickets_at_ready_are_candidates() {
        let mut intake = ticket("A", Priority::Urgent);
        intake.state = TicketState::admitted();
        let mut building = ticket("B", Priority::Urgent);
        building.state = TicketState::restore(Status::Active(Stage::Build), 0, 0, 0).unwrap();
        let mut waiting = ticket("C", Priority::Urgent);
        waiting.state = TicketState::restore(
            Status::NeedsInput {
                return_to: Stage::Ready,
            },
            1,
            0,
            0,
        )
        .unwrap();
        let ready = ready_set(
            &[intake, building, waiting, ticket("D", Priority::Low)],
            ROOMY,
            IDLE,
        );
        assert_eq!(ready.dispatch, ["D"]);
        assert!(ready.waiting.is_empty());
    }

    #[test]
    fn priority_orders_dispatch_and_ties_keep_the_tracker_order() {
        let tickets = [
            ticket("low", Priority::Low),
            ticket("unset", Priority::Unset),
            ticket("high-1", Priority::High),
            ticket("urgent", Priority::Urgent),
            ticket("high-2", Priority::High),
        ];
        let ready = ready_set(&tickets, ROOMY, IDLE);
        assert_eq!(
            ready.dispatch,
            ["urgent", "high-1", "high-2", "low", "unset"]
        );
    }

    #[test]
    fn admission_and_blockers_hold_tickets_back() {
        let mut pending = ticket("pending", Priority::Urgent);
        pending.admission = Admission::NotAdmitted;
        let mut excluded = ticket("excluded", Priority::Urgent);
        excluded.admission = Admission::Excluded;
        // Exclusion wins over an open blocker: admission is checked first.
        excluded.blocked_by = vec![Blocker {
            id: "X",
            done: false,
        }];
        let mut blocked = ticket("blocked", Priority::Urgent);
        blocked.blocked_by = vec![
            Blocker {
                id: "X",
                done: false,
            },
            Blocker {
                id: "Y",
                done: true,
            },
            Blocker {
                id: "Z",
                done: false,
            },
        ];
        let mut released = ticket("released", Priority::Urgent);
        released.blocked_by = vec![Blocker {
            id: "Y",
            done: true,
        }];

        let ready = ready_set(&[pending, excluded, blocked, released], ROOMY, IDLE);
        assert_eq!(ready.dispatch, ["released"]);
        assert_eq!(
            ready.waiting,
            [
                ("pending", Wait::NotAdmitted),
                ("excluded", Wait::Excluded),
                ("blocked", Wait::BlockedBy(vec!["X", "Z"])),
            ]
        );
    }

    #[test]
    fn free_run_slots_limit_dispatch() {
        let tickets = [
            ticket("A", Priority::High),
            ticket("B", Priority::Medium),
            ticket("C", Priority::Low),
        ];
        let caps = Caps {
            concurrent_runs: 3,
            open_prs: None,
        };
        let ready = ready_set(
            &tickets,
            caps,
            Load {
                runs: 2,
                open_prs: 0,
            },
        );
        assert_eq!(ready.dispatch, ["A"]);
        assert_eq!(ready.waiting, [("B", Wait::RunCap), ("C", Wait::RunCap)]);

        // More runs in flight than the cap allows: nothing starts.
        let ready = ready_set(
            &tickets,
            caps,
            Load {
                runs: 5,
                open_prs: 0,
            },
        );
        assert!(ready.dispatch.is_empty());
    }

    #[test]
    fn nothing_new_starts_at_the_open_pr_cap() {
        let tickets = [ticket("A", Priority::High), ticket("B", Priority::Low)];
        let caps = Caps {
            concurrent_runs: 5,
            open_prs: Some(2),
        };
        let at_cap = ready_set(
            &tickets,
            caps,
            Load {
                runs: 0,
                open_prs: 2,
            },
        );
        assert!(at_cap.dispatch.is_empty());
        assert_eq!(at_cap.waiting, [("A", Wait::PrCap), ("B", Wait::PrCap)]);

        let below = ready_set(
            &tickets,
            caps,
            Load {
                runs: 0,
                open_prs: 1,
            },
        );
        assert_eq!(below.dispatch, ["A", "B"]);
    }

    #[test]
    fn an_id_is_dispatched_once() {
        let tickets = [
            ticket("A", Priority::Low),
            ticket("A", Priority::High),
            ticket("B", Priority::Medium),
        ];
        let ready = ready_set(&tickets, ROOMY, IDLE);
        assert_eq!(ready.dispatch, ["A", "B"]);
        assert_eq!(ready.waiting, [("A", Wait::Duplicate)]);
    }
}
