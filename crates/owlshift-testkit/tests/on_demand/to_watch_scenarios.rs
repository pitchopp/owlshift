//! `owlshift continue --to-watch` (OWL-420; architecture section 5, "A
//! restart handed to watch"): a park restarted as `continue` restarts it,
//! then left to watch, the command running nothing.

use owlshift_runner::on_demand::Takes;
use owlshift_runner::watch::presence;

use super::*;

impl Bench {
    /// Runs `owlshift continue --to-watch DEMO-1`.
    fn to_watch(&self) -> (Result<Delivered, Stop>, String) {
        self.with_on_demand(Vec::new(), None, Vec::new(), |on_demand, sink| {
            on_demand.continue_to_watch(&ticket(), sink)
        })
    }

    /// The ticket ref rewritten at `status`, round 1.
    fn put(&self, status: Status) {
        let state = TicketState::restore(status, 1, 0, 0, 0, 0).unwrap();
        self.write_record(&TicketRecord {
            state: PersistedState::from(&state),
            ..self.record()
        });
    }
}

/// A ticket parked at Build by a blocked run, after an answered round.
fn parked_at_build() -> Bench {
    let bench = Bench::new(true);
    let (asked, _) = bench.run(vec![bench.reply(None, Some(ROUND_1))], None);
    assert!(matches!(asked, Err(Stop::NeedsInput { .. })), "{asked:?}");
    bench.answer("Q1: English.\nQ2: \"Hello, reader.\", no sign-off.\n");
    let answered = check(&[
        ("Q1", "answered", "English."),
        ("Q2", "answered", "\"Hello, reader.\", no sign-off."),
    ]);
    let blocked = r#"{"format":14,"status":"blocked","summary":"The gate needs network."}"#;
    let (parked, printed) = bench.continue_ticket(vec![
        bench.reply(None, Some(&answered)),
        bench.reply(None, Some(blocked)),
    ]);
    assert!(
        matches!(&parked, Err(Stop::Parked { .. })),
        "{parked:?}\n{printed}"
    );
    bench
}

/// The ticket's `decision` events since `from`.
fn decisions(bench: &Bench, from: usize) -> Vec<Value> {
    data_of(&bench.events()[from..], EventKind::Decision, "restarted")
}

/// The ticket's acceptance: a park at Build restarted with the flag runs
/// nothing, shows In Progress and records the restart for watch; watch's
/// next pass runs the Build and delivers it.
#[test]
fn a_park_at_build_is_left_to_watch_which_builds_it() {
    let bench = parked_at_build();
    let (briefs, events) = (bench.briefs().len(), bench.events().len());
    let (outcome, printed) = bench.to_watch();
    assert_eq!(
        outcome.as_ref().unwrap_err(),
        &Stop::ToWatch {
            ticket: ticket(),
            stage: Stage::Build,
            takes: Takes::Resume,
            restarted: true,
            watching: Some(false),
        },
        "{printed}"
    );
    assert!(
        outcome
            .unwrap_err()
            .to_string()
            .contains("nothing drives the ticket until one starts")
    );
    assert_eq!(bench.briefs().len(), briefs, "no run");
    assert_eq!(state_of(&bench).status(), Status::Active(Stage::Build));
    assert_eq!(bench.stage(), "In Progress");
    let restart = decisions(&bench, events);
    assert_eq!(restart.len(), 1, "{restart:?}");
    assert_eq!(restart[0]["to_watch"], json!(true));
    assert_eq!(restart[0]["takes"], json!("resume"));
    assert!(data_of(&bench.events()[events..], EventKind::RunStarted, "role").is_empty());

    let mut pass = 0;
    let (outcomes, lines, printed) =
        bench.watch(vec![bench.reply(Some("Hello"), Some(DONE))], &mut |_| {
            pass += 1;
            pass < 2
        });
    assert!(
        matches!(&outcomes[..], [Ok(_)]),
        "{outcomes:?}\n{lines}\n{printed}"
    );
    let roles: Vec<Role> = bench.briefs()[briefs..]
        .iter()
        .map(|brief| brief.role)
        .collect();
    assert_eq!(roles, [Role::Build, Role::Verify], "watch ran the Build");
    assert_eq!(bench.stage(), "In Review");
}

/// Every park the flag takes or refuses, and a ticket not parked, on one
/// ticket ref rewritten: a stage watch resumes, shown working; a park
/// while a round waited, back to needs input; a park at Intake, refused
/// with nothing written; a ticket not parked, nothing written. A watch
/// holds the project throughout.
#[test]
fn the_flag_restarts_what_watch_takes_and_refuses_the_rest() {
    let bench = parked_at_build();
    let _watch = presence::hold(&bench.dirs()).unwrap().expect("held");
    let briefs = bench.briefs().len();
    let parked = |at, awaiting_input| Status::Parked { at, awaiting_input };
    for at in [Stage::Design, Stage::DesignReview, Stage::Build] {
        bench.put(parked(at, false));
        let (outcome, printed) = bench.to_watch();
        assert_eq!(
            outcome.unwrap_err(),
            Stop::ToWatch {
                ticket: ticket(),
                stage: at,
                takes: Takes::Resume,
                restarted: true,
                watching: Some(true),
            },
            "{printed}"
        );
        assert_eq!(state_of(&bench).status(), Status::Active(at));
        assert_eq!(bench.stage(), "In Progress");
    }

    bench.put(parked(Stage::Build, true));
    let (outcome, _) = bench.to_watch();
    assert!(
        matches!(
            outcome,
            Err(Stop::ToWatch {
                takes: Takes::Answers,
                restarted: true,
                ..
            })
        ),
        "{outcome:?}"
    );
    assert_eq!(
        state_of(&bench).status(),
        Status::NeedsInput {
            return_to: Stage::Build
        }
    );
    assert_eq!(bench.stage(), "Needs Input");

    // Not parked: a round waiting, its answers in, gets no answer check.
    let (commit, events) = (bench.ref_commit(), bench.events().len());
    let (outcome, _) = bench.to_watch();
    assert!(
        matches!(
            outcome,
            Err(Stop::ToWatch {
                takes: Takes::Answers,
                restarted: false,
                ..
            })
        ),
        "{outcome:?}"
    );
    assert_eq!(bench.ref_commit(), commit);
    assert!(decisions(&bench, events).is_empty());

    // Not parked at Build: resumed while it shows working, refused once a
    // person moved it.
    bench.put(Status::Active(Stage::Build));
    MarkdownTracker::new(&bench.remote.checkout)
        .set_stage(&ticket(), "In Progress")
        .unwrap();
    let (outcome, _) = bench.to_watch();
    assert!(
        matches!(
            outcome,
            Err(Stop::ToWatch {
                takes: Takes::Resume,
                restarted: false,
                ..
            })
        ),
        "{outcome:?}"
    );
    MarkdownTracker::new(&bench.remote.checkout)
        .set_stage(&ticket(), "Todo")
        .unwrap();
    let (outcome, _) = bench.to_watch();
    assert!(
        matches!(&outcome, Err(Stop::Refused(why)) if why.contains("shows Todo")
            && why.contains("`owlshift continue DEMO-1`")),
        "{outcome:?}"
    );

    // A park at Intake: watch does not take it.
    bench.put(parked(Stage::Intake, false));
    let (commit, events) = (bench.ref_commit(), bench.events().len());
    let (outcome, _) = bench.to_watch();
    assert!(
        matches!(&outcome, Err(Stop::Refused(why))
            if why.contains("does not take DEMO-1 at intake")
                && why.contains("`owlshift continue DEMO-1`")),
        "{outcome:?}"
    );
    assert_eq!(bench.ref_commit(), commit);
    assert!(
        bench.events()[events..]
            .iter()
            .all(|event| event.kind != EventKind::TrackerWrite)
    );
    assert_eq!(bench.briefs().len(), briefs, "nothing ran");
}

/// A stage the tracker refuses to move restarts nothing; a ref that cannot
/// be written moves the stage back to the parked one.
#[test]
fn a_restart_that_cannot_be_shown_or_kept_leaves_the_park() {
    let bench = parked_at_build();
    let parked = state_of(&bench);
    let events = bench.events().len();
    bench.refuse_stage.set(true);
    let (outcome, _) = bench.to_watch();
    assert!(
        matches!(&outcome, Err(Stop::Refused(why)) if why.contains("nothing is restarted")),
        "{outcome:?}"
    );
    assert_eq!(state_of(&bench), parked);
    assert!(decisions(&bench, events).is_empty());
    bench.refuse_stage.set(false);

    let shown = bench.stage();
    fs::write(bench.ref_lock(), "").unwrap();
    let (outcome, _) = bench.to_watch();
    fs::remove_file(bench.ref_lock()).unwrap();
    assert!(matches!(&outcome, Err(Stop::Refused(_))), "{outcome:?}");
    assert_eq!(state_of(&bench), parked);
    assert_eq!(bench.stage(), shown);
    assert_eq!(
        bench.stage_events(EventKind::TrackerWrite)[..].last_chunk::<2>(),
        Some(&["In Progress".to_owned(), shown])
    );
}

/// A park at Watch whose pull request is followed is restarted to Watch,
/// shown in review, for watch to follow.
#[test]
fn a_park_at_watch_is_left_to_watch_to_follow() {
    let bench = delivered();
    parked_at_watch(&bench);
    let briefs = bench.briefs().len();
    let (outcome, _) = bench.to_watch();
    assert!(
        matches!(
            outcome,
            Err(Stop::ToWatch {
                stage: Stage::Watch,
                takes: Takes::Follow,
                restarted: true,
                ..
            })
        ),
        "{outcome:?}"
    );
    assert_eq!(state_of(&bench).status(), Status::Active(Stage::Watch));
    assert_eq!(bench.stage(), "In Review");
    assert_eq!(bench.briefs().len(), briefs);
}
