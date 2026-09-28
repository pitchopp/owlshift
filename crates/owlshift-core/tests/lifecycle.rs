//! One ticket's life, driven only through the crate's public API, the way the
//! runner and the test harness use it.

use owlshift_core::floor::{Action, HumanApproval, check_action};
use owlshift_core::gate::{GatePolicy, Route};
use owlshift_core::resource::Resource;
use owlshift_core::schedule::{Caps, Load, Wait, ready_set};
use owlshift_core::state::{Event, Finish, ParkReason, Status, TicketState, Transition};
use owlshift_core::ticket::{Admission, Blocker, Ticket};
use owlshift_core::vocab::{PlanApproval, Priority, Stage, Variant};

/// Applies an event the ticket must accept, and returns its new state.
fn step(ticket: &mut Ticket<String>, event: Event) -> Transition {
    let transition = ticket
        .state
        .apply(ticket.pipeline(), event)
        .unwrap_or_else(|error| panic!("{error}"));
    match transition {
        Transition::To(state) | Transition::Parked { state, .. } => ticket.state = state,
        Transition::Finished(_) => {}
    }
    transition
}

#[test]
fn a_standard_ticket_from_admission_to_merge() {
    let mut ticket = Ticket {
        id: "OWL-42".to_owned(),
        priority: Priority::High,
        admission: Admission::Admitted,
        blocked_by: vec![Blocker {
            id: "OWL-41".to_owned(),
            done: false,
        }],
        variant: Variant::Standard,
        resources: vec![Resource::Zone("crates/owlshift-core".to_owned())],
        state: TicketState::admitted(),
    };
    let policy = GatePolicy::new(["billing"], PlanApproval::OnFork);

    // Intake asks a scope question: a human decides, and the ticket will
    // return to Ready.
    assert_eq!(policy.route("scope"), Route::Human);
    step(&mut ticket, Event::Questions);
    assert_eq!(
        ticket.state.status(),
        Status::NeedsInput {
            return_to: Stage::Ready
        }
    );
    step(&mut ticket, Event::Incomplete);
    step(&mut ticket, Event::Answered);
    assert_eq!(ticket.state.status(), Status::Active(Stage::Ready));

    // Blocked until OWL-41 is done, then dispatched.
    let caps = Caps {
        concurrent_runs: 1,
        open_prs: Some(3),
    };
    let ready = ready_set(std::slice::from_ref(&ticket), caps, Load::default());
    assert_eq!(
        ready.waiting,
        [(
            "OWL-42".to_owned(),
            Wait::BlockedBy(vec!["OWL-41".to_owned()])
        )]
    );
    ticket.blocked_by[0].done = true;
    let ready = ready_set(std::slice::from_ref(&ticket), caps, Load::default());
    assert_eq!(ready.dispatch, ["OWL-42".to_owned()]);
    step(&mut ticket, Event::Dispatched);
    assert_eq!(ticket.state.status(), Status::Active(Stage::Design));

    // Design, a revision asked by the review, then approval of a forked plan.
    step(&mut ticket, Event::Completed);
    step(&mut ticket, Event::LoopBack);
    step(&mut ticket, Event::Completed);
    assert!(policy.plan_approval_required(ticket.pipeline(), true));
    step(&mut ticket, Event::Questions);
    step(&mut ticket, Event::Answered);
    step(&mut ticket, Event::Completed);
    assert_eq!(ticket.state.status(), Status::Active(Stage::Build));
    assert_eq!(ticket.state.round(), 2);

    // A failed run and a usage-limit interruption, then Build passes.
    step(&mut ticket, Event::RunFailed);
    step(&mut ticket, Event::Interrupted);
    assert_eq!(ticket.state.failed_runs(), 1);
    step(&mut ticket, Event::Completed);
    step(&mut ticket, Event::Completed);

    // Deliver: the Writer may open the pull request, never merge it.
    assert_eq!(ticket.state.status(), Status::Active(Stage::Deliver));
    assert!(check_action(Action::OpenPullRequest, HumanApproval::Absent).is_ok());
    assert!(check_action(Action::Merge, HumanApproval::Recorded).is_err());
    step(&mut ticket, Event::Completed);

    // Watch: a red check sends it back to Build; a blocked fix run parks it;
    // a human restarts it; the fix goes through; a human merges.
    step(&mut ticket, Event::LoopBack);
    assert!(matches!(
        step(&mut ticket, Event::Blocked),
        Transition::Parked {
            reason: ParkReason::Blocked,
            ..
        }
    ));
    step(&mut ticket, Event::Restarted);
    for _ in 0..3 {
        step(&mut ticket, Event::Completed);
    }
    assert_eq!(ticket.state.status(), Status::Active(Stage::Watch));
    assert_eq!(
        step(&mut ticket, Event::Merged),
        Transition::Finished(Finish::Merged)
    );
}
