//! One ticket's life, driven only through the crate's public API, the way the
//! runner and the test harness use it.

use owlshift_core::floor::{Action, HumanApproval, check_action};
use owlshift_core::gate::{GatePolicy, PlanGate, Route};
use owlshift_core::resource::Resource;
use owlshift_core::review::{Standing, settle};
use owlshift_core::roster::roster;
use owlshift_core::schedule::{Caps, Load, Wait, ready_set};
use owlshift_core::state::{Event, Finish, ParkReason, Status, TicketState, Transition};
use owlshift_core::ticket::{Admission, Blocker, BlockerState, Ticket};
use owlshift_core::vocab::{PlanApproval, Priority, Severity, Stage, Variant};

/// Applies an event the ticket must accept, and returns its new state.
fn step(ticket: &mut Ticket<String>, event: Event) -> Transition {
    step_with(ticket, event, None)
}

/// As [`step`], with the plan gate of the ticket's current plan.
fn step_with(ticket: &mut Ticket<String>, event: Event, gate: Option<PlanGate>) -> Transition {
    let transition = match gate {
        None => ticket.state.apply(ticket.pipeline(), event),
        Some(gate) => ticket.state.apply_gated(ticket.pipeline(), event, gate),
    }
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
        created_ms: 1_760_000_000_000,
        admission: Admission::Admitted,
        has_decider: true,
        text_hash: "hash".to_owned(),
        intake_hash: Some("hash".to_owned()),
        blocked_by: vec![Blocker {
            id: "OWL-41".to_owned(),
            tracker: BlockerState::Open,
            merged: false,
        }],
        variant: Variant::Standard,
        resources: vec![Resource::Zone("crates/owlshift-core".to_owned())],
        state: TicketState::admitted(),
        delivery_followed: false,
    };
    let policy = GatePolicy::new(["billing"], PlanApproval::OnFork);

    // Intake asks a scope question: a human decides, and intake runs again
    // with the answers before the ticket is ready (OWL-269).
    assert_eq!(policy.route("scope"), Route::Human);
    step(&mut ticket, Event::Questions);
    assert_eq!(
        ticket.state.status(),
        Status::NeedsInput {
            return_to: Stage::Intake
        }
    );
    step(&mut ticket, Event::Incomplete);
    step(&mut ticket, Event::Answered);
    assert_eq!(ticket.state.status(), Status::Active(Stage::Intake));
    step(&mut ticket, Event::Completed);
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
    ticket.blocked_by[0].tracker = BlockerState::Completed;
    let ready = ready_set(std::slice::from_ref(&ticket), caps, Load::default());
    assert_eq!(ready.dispatch, Some("OWL-42".to_owned()));
    step(&mut ticket, Event::Dispatched);
    assert_eq!(ticket.state.status(), Status::Active(Stage::Design));

    // Design, then its review: the critic and conformance. The first pass's
    // confirmed finding sends the plan back; the second, last, pass leaves a
    // major for the decider, whose answer the design takes in before one
    // more pass, which only notes a minor.
    step(&mut ticket, Event::Completed);
    assert_eq!(
        roster(ticket.variant, Stage::DesignReview).len(),
        2,
        "two reviewers"
    );
    let pass = |ticket: &Ticket<String>, findings: &[Standing]| {
        settle(
            ticket.state.stage(),
            ticket.state.review_pass_due(),
            findings,
        )
        .expect("a review stage")
        .outcome
    };
    let major = Standing {
        severity: Severity::Major,
        on_criterion: true,
    };
    let minor = Standing {
        severity: Severity::Minor,
        on_criterion: false,
    };
    let event = pass(&ticket, &[minor]).event();
    step(&mut ticket, event);
    assert_eq!(ticket.state.status(), Status::Active(Stage::Design));
    step(&mut ticket, Event::Completed);
    let event = pass(&ticket, &[major]).event();
    step(&mut ticket, event);
    assert_eq!(
        ticket.state.status(),
        Status::NeedsInput {
            return_to: Stage::Design
        }
    );
    step(&mut ticket, Event::Answered);
    step(&mut ticket, Event::Completed);
    // The plan names a fork, so under `on-fork` the decider approves it
    // before Build (OWL-32): the passed pass opens the approval round, and
    // the design run that answers it `approved` takes the plan to Build.
    let plan = "the plan's hash";
    let unapproved = policy.plan_gate(ticket.pipeline(), true, plan, None);
    assert!(
        ticket
            .state
            .apply(ticket.pipeline(), Event::Completed)
            .is_err()
    );
    let event = pass(&ticket, &[minor]).design_review_event(unapproved);
    step(&mut ticket, event);
    assert_eq!(
        ticket.state.status(),
        Status::NeedsInput {
            return_to: Stage::Design
        }
    );
    step(&mut ticket, Event::Answered);
    let approved = policy.plan_gate(ticket.pipeline(), true, plan, Some(plan));
    step_with(&mut ticket, Event::PlanApproved, Some(approved));
    assert_eq!(ticket.state.status(), Status::Active(Stage::Build));
    assert_eq!(ticket.state.round(), 3);
    assert_eq!(ticket.state.review_passes(), 0);

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
