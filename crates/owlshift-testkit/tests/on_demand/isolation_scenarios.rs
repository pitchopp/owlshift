//! OWL-400's acceptance through `owlshift do` and `continue` (architecture
//! section 8, "Isolation among steps"): a run cut off before its check
//! quarantines the project until a person clears it, for good; a ticket
//! changed outside its runs is caught at its next start; a write cut between
//! the ticket's ref and its run file is finished by the next command; and a
//! quarantine that stands stops a step at its next run and before its push.

use owlshift_contracts::step::{RunTarget, TicketRun};
use owlshift_runner::ticket_run;

use super::*;

const BLOCKED: &str = r#"{"format":14,"status":"blocked","summary":"Waiting."}"#;

impl Bench {
    /// The ticket's run file.
    fn run_file(&self) -> TicketRun {
        TicketRun::parse(&fs::read_to_string(self.dirs().run_file(&ticket())).unwrap()).unwrap()
    }

    /// The commit `rev` names in the dedicated checkout.
    fn rev(&self, rev: &str) -> String {
        let out = self.env.run(&self.dirs().checkout(), &["rev-parse", rev]).unwrap();
        String::from_utf8(out).unwrap().trim().to_owned()
    }
}

/// A run cut off before its check (a crash, a stop) leaves its obligation,
/// its work in its branch: the next command finds the ticket's lock free and
/// quarantines the project, naming the run, and runs nothing. Once a person
/// clears it, the ticket runs again, its run file taken from its branch as
/// the run left it, and nothing quarantines the project again (acceptance:
/// "a crash leaves the quarantine and `unquarantine` clears it for good").
#[test]
fn a_run_cut_off_quarantines_the_project_until_a_clear_for_good() {
    let bench = Bench::new(true);
    let (outcome, printed) = bench.run(vec![bench.reply(Some("Hi"), Some(BLOCKED))], None);
    assert!(matches!(outcome, Err(Stop::Parked { .. })), "{outcome:?}\n{printed}");
    let dirs = bench.dirs();
    assert!(!dirs.obligations_stand(), "a passed check removes its own");

    // The cut-off run: its obligation, and a commit it made.
    ticket_run::write_obligation(
        &dirs,
        Some(&ticket()),
        "20261021T000000Z",
        "Run 20261021T000000Z of DEMO-1",
    )
    .unwrap();
    bench
        .env
        .run(
            &dirs.worktree(&ticket()),
            &["commit", "--quiet", "--allow-empty", "-m", "Cut off"],
        )
        .unwrap();
    let runs = bench.briefs().len();
    let (refused, printed) = bench.run(Vec::new(), None);
    match &refused {
        Err(Stop::Unverified { marker, text }) => {
            assert_eq!(marker, &dirs.quarantine_file());
            assert!(
                text.contains("Run 20261021T000000Z of DEMO-1")
                    && text.contains("ended before its isolation check passed"),
                "{text}"
            );
        }
        other => panic!("{other:?}\n{printed}"),
    }
    assert_eq!(bench.briefs().len(), runs, "nothing ran");
    // Sticky: a second command is refused the same way.
    let (again, _) = bench.run(Vec::new(), None);
    assert!(matches!(again, Err(Stop::Unverified { .. })), "{again:?}");

    assert!(matches!(
        bench.unquarantine(),
        quarantine::Cleared::Cleared { .. }
    ));
    assert_eq!(dirs.quarantine().unwrap(), None);
    assert!(!dirs.obligations_stand());
    let (outcome, printed) = bench.run(vec![bench.reply(Some("Hello"), Some(DONE))], None);
    outcome.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    assert_eq!(dirs.quarantine().unwrap(), None, "cleared for good");
    assert!(!dirs.obligations_stand());
}

/// A ticket whose branch was moved outside its runs, as a sibling's run
/// that got past its sandbox could move it, is caught at its next start:
/// the project is quarantined, naming the branch, and nothing runs
/// (acceptance: "a tampered ended ticket is caught at its next start").
#[test]
fn a_ticket_changed_outside_its_runs_is_caught_at_its_next_start() {
    let bench = Bench::new(true);
    let (outcome, printed) = bench.run(vec![bench.reply(Some("Hi"), Some(BLOCKED))], None);
    assert!(matches!(outcome, Err(Stop::Parked { .. })), "{outcome:?}\n{printed}");
    let checkout = bench.dirs().checkout();
    let base = bench.rev("refs/remotes/origin/main");
    bench
        .env
        .run(&checkout, &["update-ref", "refs/heads/owlshift/demo-1", &base])
        .unwrap();
    let runs = bench.briefs().len();
    let (refused, printed) = bench.run(Vec::new(), None);
    match &refused {
        Err(Stop::Unverified { text, .. }) => assert!(
            text.contains("the branch owlshift/demo-1 of DEMO-1 is at")
                && text.contains("changed outside its runs"),
            "{text}"
        ),
        other => panic!("{other:?}\n{printed}"),
    }
    assert_eq!(bench.briefs().len(), runs, "nothing ran");
    assert!(bench.dirs().quarantine().unwrap().is_some());
}

/// The runner's write of the ticket's ref cut off after the ref moved and
/// before its run file recorded it is finished by the next command, which
/// runs as usual (acceptance: "a write cut between ref and run file is
/// finished").
#[test]
fn a_write_cut_between_the_ref_and_the_run_file_is_finished_by_the_next_command() {
    let bench = Bench::new(true);
    let (outcome, printed) = bench.run(vec![bench.reply(Some("Hi"), Some(BLOCKED))], None);
    assert!(matches!(outcome, Err(Stop::Parked { .. })), "{outcome:?}\n{printed}");
    let env = bench.env.clone();
    let git = Git::with_setup("git", move |command| env.apply(command));
    let checkout = bench.dirs().checkout();
    let moved = ticket_ref::write(
        &git,
        &checkout,
        &ticket(),
        &bench.record(),
        Some(&bench.ref_commit()),
    )
    .unwrap();
    let mut run = bench.run_file();
    assert_ne!(run.ticket_ref.as_deref(), Some(moved.as_str()));
    run.writing.ticket_ref = Some(RunTarget {
        to: Some(moved.clone()),
    });
    fs::write(bench.dirs().run_file(&ticket()), run.render()).unwrap();

    let (outcome, printed) = bench.run(vec![bench.reply(Some("Hello"), Some(DONE))], None);
    outcome.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    let run = bench.run_file();
    assert!(run.writing.is_empty(), "{run:?}");
    assert_eq!(run.ticket_ref.as_deref(), Some(bench.ref_commit().as_str()));
    assert_eq!(bench.dirs().quarantine().unwrap(), None);
}

/// A quarantine that stands, since a sibling's breach, stops a step at its
/// next run's boundary and before its push: nothing more runs, nothing is
/// pushed, and the ticket is left where it is, not parked (decided on
/// 2026-10-21, OWL-400). The sibling's breach is played by writing the
/// quarantine during the step's first run.
#[test]
fn a_standing_quarantine_stops_a_step_at_its_next_run_and_before_its_push() {
    let quarantine = |bench: &Bench| -> Agent {
        let file = bench.dirs().quarantine_file();
        Box::new(move |_| {
            fs::write(&file, "Run r9 of DEMO-7 broke isolation:\n- x\n").unwrap();
        })
    };
    let stopped = |outcome: &Result<Delivered, Stop>, printed: &str| match outcome {
        Err(Stop::Unverified { text, .. }) => {
            assert!(text.contains("of DEMO-7 broke isolation"), "{text}");
        }
        other => panic!("{other:?}\n{printed}"),
    };

    // A failed run gets one more: that one never starts (one reply only).
    let bench = Bench::new(true);
    let failed = r#"{"format":14,"status":"failed","summary":"Lost."}"#;
    let (outcome, printed) =
        bench.run(vec![bench.reply(None, Some(failed))], Some(quarantine(&bench)));
    stopped(&outcome, &printed);
    assert_eq!(bench.briefs().len(), 1);
    assert_ne!(bench.record().state.waiting, Some(Waiting::Parked));

    // A done run: no push.
    let bench = Bench::new(true);
    let (outcome, printed) = bench.run(
        vec![bench.reply(Some("Hello"), Some(DONE))],
        Some(quarantine(&bench)),
    );
    stopped(&outcome, &printed);
    assert_eq!(bench.remote_branch(), None, "nothing pushed");
    assert!(
        bench.comments().iter().all(|c| !c.starts_with("[owlshift] DELIVERY")),
        "{:?}",
        bench.comments()
    );
}
