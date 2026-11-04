//! A restart typed on the ticket (OWL-488; architecture section 5, "A
//! restart typed on the ticket"): the decider's reply `continue` on a
//! parked ticket, read by watch's pass, restarts it as `owlshift continue`
//! does; anyone else's, or one already settled, restarts nothing; a park at
//! Intake runs intake as a step of watch's; a quarantine answers the reply
//! once; a restart that could not be shown is tried again at the next pass.

use owlshift_contracts::step::{Settled, TicketRestart};
use owlshift_runner::on_demand::typed;

use super::*;

/// A ticket parked at Build by a blocked run, after an answered round.
fn parked_at_build() -> Bench {
    parked_at_build_on(Bench::new(true))
}

/// [`parked_at_build`] on `bench`.
fn parked_at_build_on(bench: Bench) -> Bench {
    let (asked, _) = bench.run(vec![bench.reply(None, Some(ROUND_1))], None);
    assert!(matches!(asked, Err(Stop::NeedsInput { .. })), "{asked:?}");
    bench.answer("Q1: English.\nQ2: \"Hello, reader.\", no sign-off.\n");
    let answered = check(&[
        ("Q1", "answered", "English."),
        ("Q2", "answered", "\"Hello, reader.\", no sign-off."),
    ]);
    let (parked, printed) = bench.continue_ticket(vec![
        bench.reply(None, Some(&answered)),
        bench.reply(None, Some(BLOCKED)),
    ]);
    assert!(
        matches!(&parked, Err(Stop::Parked { .. })),
        "{parked:?}\n{printed}"
    );
    bench
}

/// What a blocked Build run says.
const BLOCKED: &str = r#"{"format":14,"status":"blocked","summary":"The gate needs network."}"#;

/// The restarts recorded since `from`.
fn restarts(bench: &Bench, from: usize) -> Vec<Value> {
    data_of(&bench.events()[from..], EventKind::Decision, "restarted")
}

/// The last `continue` comment settled on the ticket.
fn settled(bench: &Bench) -> Option<TicketRestart> {
    typed::settled(&bench.dirs(), &ticket())
}

/// The REPLY comments on the ticket.
fn replies(bench: &Bench) -> Vec<String> {
    bench
        .comments()
        .into_iter()
        .filter(|body| body.starts_with("[owlshift] REPLY"))
        .collect()
}

/// One idle pass of watch, with `replies` for the runs its steps start.
fn one_pass(bench: &Bench, replies: Vec<Reply>) -> (Vec<Result<Delivered, Stop>>, String) {
    let (outcomes, lines, _) = bench.watch(replies, &mut |_| false);
    (outcomes, lines)
}

/// The ticket's acceptance: another person's `continue` restarts nothing,
/// and is not read again; the decider's restarts the park at the next pass
/// and watch drives the ticket on; parked again, that comment counts no
/// more, and a new one restarts it to a delivery.
#[test]
fn the_deciders_continue_restarts_a_park_and_watch_drives_it() {
    let bench = parked_at_build();
    assert!(
        bench
            .comments()
            .last()
            .unwrap()
            .contains("reply `continue`")
    );
    let (briefs, events) = (bench.briefs().len(), bench.events().len());

    bench.comment_as("visitor", "continue");
    one_pass(&bench, Vec::new());
    assert!(matches!(state_of(&bench).status(), Status::Parked { .. }));
    assert!(restarts(&bench, events).is_empty());
    assert_eq!(
        settled(&bench).map(|kept| kept.settled),
        Some(Settled::NotCounted)
    );
    // Settled: the next pass starts no step for it.
    let commit = bench.ref_commit();
    let from = bench.events().len();
    one_pass(&bench, Vec::new());
    assert_eq!(bench.ref_commit(), commit);
    assert!(
        data_of(&bench.events()[from..], EventKind::Warning, "what").is_empty(),
        "no step, no warning"
    );

    // The decider's reply: restarted, handed to watch, which runs the Build
    // in the same watch; blocked again, it parks again.
    bench.answer("  Continue\n");
    let (outcomes, lines) = one_pass(&bench, vec![bench.reply(None, Some(BLOCKED))]);
    let restart = restarts(&bench, events);
    assert_eq!(restart.len(), 1, "{restart:?}\n{lines}");
    assert_eq!(restart[0]["by"], json!("tracker"));
    assert_eq!(restart[0]["to_watch"], json!(true));
    assert_eq!(restart[0]["takes"], json!("resume"));
    let comment = settled(&bench).expect("settled");
    assert_eq!(comment.settled, Settled::Restarted);
    assert_eq!(restart[0]["comment"], json!(comment.comment));
    assert!(
        outcomes.iter().any(|outcome| matches!(
            outcome,
            Err(Stop::ToWatch {
                restarted: true,
                ..
            })
        )),
        "{outcomes:?}"
    );
    assert_eq!(bench.briefs()[briefs..].len(), 1, "one Build ran");
    assert!(matches!(state_of(&bench).status(), Status::Parked { .. }));

    // The comment that restarted the first park restarts nothing more.
    let from = bench.events().len();
    one_pass(&bench, Vec::new());
    assert!(restarts(&bench, from).is_empty());

    // A new `continue`, newer than the new PARKED comment: delivered.
    bench.answer("continue");
    let mut pass = 0;
    let (outcomes, lines, printed) =
        bench.watch(vec![bench.reply(Some("Hello"), Some(DONE))], &mut |_| {
            pass += 1;
            pass < 2
        });
    assert!(
        outcomes.iter().any(Result::is_ok),
        "{outcomes:?}\n{lines}\n{printed}"
    );
    assert_eq!(restarts(&bench, from).len(), 1);
    assert_eq!(bench.stage(), "In Review");
    assert!(replies(&bench).is_empty());
}

/// A park at Intake, which `--to-watch` refuses, is restarted by the
/// decider's reply as plain `continue` restarts it: watch runs intake as
/// one of its steps, and the ticket goes back to the ready column.
#[test]
fn a_park_at_intake_runs_intake_as_a_step_of_watch() {
    let bench = Bench::drained();
    let premise = r#"{"format":14,"status":"premise_false","summary":"The section is not there."}"#;
    let stop = bench
        .intake(
            vec![bench.reply(None, Some(premise))],
            gesture("maintainer"),
        )
        .expect_err("parked");
    assert!(matches!(stop, Stop::Parked { .. }), "{stop}");
    let (briefs, events) = (bench.briefs().len(), bench.events().len());

    bench.answer("continue");
    let (outcomes, lines) = one_pass(
        &bench,
        vec![bench.reply(None, Some(&intake_result("done", None, "")))],
    );
    assert!(
        matches!(&outcomes[..], [Err(Stop::Ready { .. })]),
        "{outcomes:?}\n{lines}"
    );
    let restart = restarts(&bench, events);
    assert_eq!(restart.len(), 1, "{restart:?}");
    assert_eq!(restart[0]["by"], json!("tracker"));
    assert_eq!(restart[0].get("to_watch"), None, "run, not handed");
    let roles: Vec<Role> = bench.briefs()[briefs..]
        .iter()
        .map(|brief| brief.role)
        .collect();
    assert_eq!(roles, [Role::Intake]);
    assert!(bench.record().intake.is_some());
}

/// While the project is quarantined, nothing restarts: the decider's
/// `continue` gets one REPLY saying why, and that comment is answered once;
/// another person's gets none.
#[test]
fn a_quarantine_answers_the_reply_once() {
    let bench = parked_at_build();
    let marker = bench.dirs().quarantine_file();
    fs::write(&marker, "a run stopped\n").unwrap();
    bench.comment_as("visitor", "continue");
    one_pass(&bench, Vec::new());
    assert!(replies(&bench).is_empty());

    bench.answer("continue");
    one_pass(&bench, Vec::new());
    one_pass(&bench, Vec::new());
    let replied = replies(&bench);
    assert_eq!(replied.len(), 1, "{replied:?}");
    assert!(
        replied[0].contains("did not restart DEMO-1"),
        "{}",
        replied[0]
    );
    assert!(
        replied[0].contains("owlshift unquarantine"),
        "{}",
        replied[0]
    );
    assert_eq!(
        settled(&bench).map(|kept| kept.settled),
        Some(Settled::Replied)
    );
    assert!(matches!(state_of(&bench).status(), Status::Parked { .. }));
}

/// A refusal `continue` would print, here an empty gate, is answered by one
/// REPLY and keeps the park; a restart the tracker would not show, a write
/// that failed, posts nothing and is tried again at the next pass.
#[test]
fn a_refusal_is_replied_once_and_a_failed_write_is_tried_again() {
    let bench = parked_at_build();
    bench.refuse_stage.set(true);
    bench.answer("continue");
    let events = bench.events().len();
    one_pass(&bench, Vec::new());
    assert!(restarts(&bench, events).is_empty());
    assert!(replies(&bench).is_empty());
    assert!(settled(&bench).is_none());
    let warned = data_of(&bench.events()[events..], EventKind::Warning, "what");
    assert!(
        warned
            .iter()
            .any(|data| data["what"] == json!("restart_unread")),
        "{warned:?}"
    );
    bench.refuse_stage.set(false);
    one_pass(&bench, vec![bench.reply(None, Some(BLOCKED))]);
    assert_eq!(restarts(&bench, events).len(), 1);

    // Parked, then its gate emptied, the base keeping no project file:
    // refused, answered once, still parked.
    let bench = Bench::new(true);
    bench.merge_into_main(&[("owlshift.toml", "")]);
    let bench = parked_at_build_on(bench);
    let config_path = bench.remote.checkout.join("owlshift.toml");
    let config = fs::read_to_string(&config_path).unwrap();
    fs::write(&config_path, config.replace(GATE_LINE, "gate = []")).unwrap();
    bench.answer("continue");
    one_pass(&bench, Vec::new());
    one_pass(&bench, Vec::new());
    let replied = replies(&bench);
    assert_eq!(replied.len(), 1, "{replied:?}");
    assert!(replied[0].contains("declares no gate"), "{}", replied[0]);
    assert!(matches!(state_of(&bench).status(), Status::Parked { .. }));
}
