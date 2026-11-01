//! OWL-474 on watch's own process (architecture section 5, "The local web
//! UI", "Cancel"): a waiting ticket canceled is parked at the next pass, no
//! answer check run, and `continue --to-watch` puts it back to waiting;
//! `owlshift cancel`'s refusals; the cancels a pass drops; a typed
//! `continue` withdrawing a cancel; a park whose state is not kept holding
//! the ticket. The stop of a step under way is played with real processes
//! (`tests/child_steps.rs`, `tests/daemon.rs`).

use owlshift_contracts::service::{BY_CANCEL, Request};
use owlshift_runner::cancel::{self, Asked};
use owlshift_runner::on_demand::Takes;
use owlshift_runner::watch::presence;

use super::*;

/// What the cancels of this module say.
const WHY: &str = "it rewrites the wrong module";

/// The answers to [`ROUND_1`], counted at once.
const ANSWERS: &str = "Q1: English.\nQ2: \"Hello, reader.\", no sign-off.\ngo";

impl Bench {
    /// `owlshift cancel DEMO-1 --reason WHY`, as the command asks it.
    fn cancel(&self) -> Result<Asked, String> {
        let env = self.env.clone();
        let git = Git::with_setup("git", move |command| env.apply(command));
        let request = Request::new(BY_CANCEL, Timestamp::now(), WHY);
        cancel::ask(&git, &self.dirs(), &ticket(), &request)
    }

    /// The ticket ref rewritten at `status`, its counters kept.
    fn rewrite(&self, status: Status) {
        let state = state_of(self);
        let state = TicketState::restore(status, state.round(), 0, 0, 0, 0).unwrap();
        self.write_record(&TicketRecord {
            state: PersistedState::from(&state),
            ..self.record()
        });
    }
}

/// A ticket waiting for its decider's answers to round 1, at Build.
fn waiting() -> Bench {
    let bench = Bench::new(true);
    let (asked, printed) = bench.run(vec![bench.reply(None, Some(ROUND_1))], None);
    assert!(
        matches!(asked, Err(Stop::NeedsInput { .. })),
        "{asked:?}\n{printed}"
    );
    bench
}

/// The PARKED comments on the ticket.
fn parked_comments(bench: &Bench) -> Vec<String> {
    bench
        .comments()
        .into_iter()
        .filter(|comment| comment.starts_with("[owlshift] PARKED"))
        .collect()
}

/// The acceptance's second case: a waiting ticket canceled, its decider's
/// answer counted, is parked at the next pass, waiting kept, with no answer
/// check; the PARKED comment names `owlshift cancel` and why, the file is
/// gone, and `continue --to-watch` puts it back to waiting, running nothing.
#[test]
fn a_waiting_ticket_is_parked_at_the_next_pass_and_continue_puts_it_back() {
    let bench = waiting();
    let waits = Status::NeedsInput {
        return_to: Stage::Build,
    };
    assert_eq!(state_of(&bench).status(), waits);
    bench.answer(ANSWERS);
    let asked = bench.cancel().unwrap();
    assert!(
        !asked.unkept && !asked.quarantined && !asked.watching,
        "{asked:?}"
    );
    let briefs = bench.briefs().len();

    let (outcomes, lines, printed) = bench.watch(Vec::new(), &mut |_| false);
    assert!(outcomes.is_empty(), "{outcomes:?}\n{lines}\n{printed}");
    assert_eq!(bench.briefs().len(), briefs, "an answer check ran\n{lines}");
    assert_eq!(
        state_of(&bench).status(),
        Status::Parked {
            at: Stage::Build,
            awaiting_input: true
        },
        "{lines}\n{printed}"
    );
    // The stage moved to the one the project maps for parked.
    assert!(printed.contains("action=stage stage=parked"), "{printed}");
    assert!(!cancel::stands(&bench.dirs(), &ticket()));
    let parked = parked_comments(&bench);
    assert_eq!(parked.len(), 1, "{parked:?}");
    assert!(
        parked[0].contains("its work was stopped by a cancel. Canceled by `owlshift cancel` at "),
        "{}",
        parked[0]
    );
    assert!(parked[0].contains(WHY), "{}", parked[0]);
    assert!(
        parked[0].contains("run `owlshift continue DEMO-1`: it goes on from where it was stopped"),
        "{}",
        parked[0]
    );
    assert!(
        lines.contains("`owlshift continue DEMO-1` restarts it"),
        "{lines}"
    );
    let decided = data_of(&bench.events(), EventKind::Decision, "parked");
    assert_eq!(decided.last().unwrap()["parked"], json!("Stopped"));

    let (outcome, printed) = bench.with_on_demand(Vec::new(), None, Vec::new(), |od, sink| {
        od.continue_to_watch(&ticket(), sink)
    });
    assert!(
        matches!(
            &outcome,
            Err(Stop::ToWatch {
                takes: Takes::Answers,
                restarted: true,
                ..
            })
        ),
        "{outcome:?}\n{printed}"
    );
    assert_eq!(state_of(&bench).status(), waits);
}

/// `owlshift cancel`'s refusals and what it says, a typed `continue`
/// withdrawing a cancel, and the cancels one pass drops: a ticket parked
/// since, and a folder of a ticket Owlshift keeps nothing of.
#[test]
fn cancels_are_refused_withdrawn_and_dropped_as_nothing_runs() {
    let bench = Bench::new(true);
    let refused = bench.cancel().unwrap_err();
    assert!(
        refused.contains("Owlshift has nothing of DEMO-1 in flight"),
        "{refused}"
    );
    assert!(!cancel::stands(&bench.dirs(), &ticket()));

    let (asked, _) = bench.run(vec![bench.reply(None, Some(ROUND_1))], None);
    assert!(matches!(asked, Err(Stop::NeedsInput { .. })), "{asked:?}");
    let first = bench.cancel().unwrap();
    assert_eq!(first.replaced, None);
    assert!(!first.watching);
    {
        let _watch = presence::hold(&bench.dirs()).unwrap().expect("held");
        let again = bench.cancel().unwrap();
        assert!(again.watching, "{again:?}");
        assert!(matches!(&again.replaced, Some(Some(request)) if request.by == BY_CANCEL));
    }

    // A person's `continue` withdraws it.
    let events = bench.events().len();
    let (outcome, printed) = bench.continue_ticket(Vec::new());
    assert!(
        !cancel::stands(&bench.dirs(), &ticket()),
        "{outcome:?}\n{printed}"
    );
    let withdrawn = data_of(&bench.events()[events..], EventKind::Decision, "cancel");
    assert_eq!(withdrawn.len(), 1, "{withdrawn:?}");
    assert_eq!(withdrawn[0]["cancel"], json!("withdrawn"));
    assert_eq!(withdrawn[0]["why"], json!(WHY));

    // Parked since: refused, and a file left from before is dropped.
    bench.cancel().unwrap();
    let parked = Status::Parked {
        at: Stage::Build,
        awaiting_input: false,
    };
    bench.rewrite(parked);
    let refused = bench.cancel().unwrap_err();
    assert!(refused.contains("DEMO-1 is already parked"), "{refused}");
    let stray = bench.dirs().tickets().join("demo-7").join("cancel");
    fs::create_dir_all(stray.parent().unwrap()).unwrap();
    fs::write(&stray, "not a request").unwrap();
    let (outcomes, lines, _) = bench.watch(Vec::new(), &mut |_| false);
    assert!(outcomes.is_empty(), "{outcomes:?}\n{lines}");
    assert_eq!(state_of(&bench).status(), parked);
    assert!(!cancel::stands(&bench.dirs(), &ticket()), "{lines}");
    assert!(!stray.exists(), "{lines}");
    assert!(
        lines.contains("the cancel of DEMO-1 is dropped: DEMO-1 is already parked"),
        "{lines}"
    );
    assert!(
        lines.contains("the cancel of demo-7 is dropped: Owlshift has nothing of demo-7"),
        "{lines}"
    );
    let dropped = data_of(&bench.events(), EventKind::Decision, "cancel");
    assert_eq!(
        dropped
            .iter()
            .filter(|d| d["cancel"] == json!("dropped"))
            .count(),
        2,
        "{dropped:?}"
    );
    assert!(parked_comments(&bench).is_empty());
}

/// A cancel's park whose state cannot be kept, the ticket ref's lock held,
/// posts its PARKED comment once and holds the ticket until a person acts:
/// the next passes neither park it again nor check its counted answer.
#[test]
fn a_park_not_kept_holds_the_ticket_until_a_person_acts() {
    let bench = waiting();
    bench.answer(ANSWERS);
    bench.cancel().unwrap();
    let briefs = bench.briefs().len();
    let lock = bench.ref_lock();
    fs::create_dir_all(lock.parent().unwrap()).unwrap();
    fs::write(&lock, "").unwrap();

    let mut passes = 0;
    let (outcomes, lines, printed) = bench.watch_passes(Vec::new(), &mut |_| {
        passes += 1;
        passes < 3
    });
    fs::remove_file(&lock).unwrap();
    assert!(outcomes.is_empty(), "{outcomes:?}\n{lines}\n{printed}");
    assert_eq!(parked_comments(&bench).len(), 1, "{lines}\n{printed}");
    assert_eq!(bench.briefs().len(), briefs, "an answer check ran\n{lines}");
    assert!(!cancel::stands(&bench.dirs(), &ticket()), "{lines}");
    assert!(
        matches!(state_of(&bench).status(), Status::NeedsInput { .. }),
        "the park was kept\n{lines}"
    );
    assert!(
        lines.contains("but keeping the ticket's state in its ref failed"),
        "{lines}"
    );
}
