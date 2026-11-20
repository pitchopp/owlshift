//! Scenario S14 (OWL-589, build plan "P10 · Teams", part 9): two runners
//! on one project, each with its own data directory (so its own dedicated
//! checkout, runner identifier and events), its operator, what it takes and
//! its notifier, on the bench's one bare remote and one Markdown tracker
//! folder. Each runner is a [`Runner`] seat, swapped into the bench for the
//! commands [`Bench::playing`] runs; the fake GitHub, the clock, the routing
//! record and the lock probe stay the bench's, shared by both seats on
//! purpose. Each line of S14 is a test, the last a handover that ends in a
//! delivery.

use owlshift_runner::notify;
use owlshift_runner::operator::Taking;

use super::claims_scenarios::STEP_WAIT;
use super::drain_scenarios::{done_for, made_now};
use super::review_scenarios::{RENAMED, chose, review};
use super::*;

/// The team's `[policy]` claims and its people, each with a tracker account
/// and a forge login that differ.
const TEAM: &str = "claims = \"remote\"\n\
                    [people.maintainer]\ntracker = \"maintainer\"\nforge = \"mo-dev\"\n\
                    [people.lea]\ntracker = \"lea\"\nforge = \"lea-martin\"\n";

/// One runner of the team: what differs between two machines.
struct Runner {
    data: PathBuf,
    operator: String,
    takes: Taking,
    notified: Notified,
    usage: PathBuf,
}

impl Bench {
    /// The team's project: `[people]` lists the maintainer and lea, either
    /// of whom admits with the label `agent`, and tickets are claimed on the
    /// remote, seeded so a runner reads both at its base.
    fn team() -> Self {
        Bench::build_admitting(
            "maintainer",
            "assignee = \"maintainer\"\nlabels = [\"agent\"]\n",
            TEAM,
            ", parked = \"Parked\"",
            "admit = { label = \"agent\" }\nadmitters = [\"maintainer\", \"lea\"]",
        )
    }

    /// `operator`'s runner, taking `takes`, with a data directory of its
    /// own.
    fn seat(&self, operator: &str, takes: Taking) -> Runner {
        let data = self.tmp.path().join(format!("{operator}-machine"));
        Runner {
            usage: data.join("usage"),
            data,
            operator: operator.to_owned(),
            takes,
            notified: Notified::default(),
        }
    }

    /// `act` on this bench as `seat`'s runner: its commands read and write
    /// that runner's data directory, as its operator. `act` must use the
    /// bench it is given.
    fn playing<R>(&mut self, seat: &mut Runner, act: impl FnOnce(&Self) -> R) -> R {
        self.swap_seat(seat);
        let acted = act(self);
        self.swap_seat(seat);
        acted
    }

    fn swap_seat(&mut self, seat: &mut Runner) {
        std::mem::swap(&mut self.data, &mut seat.data);
        std::mem::swap(self.operator.get_mut(), &mut seat.operator);
        std::mem::swap(self.takes.get_mut(), &mut seat.takes);
        std::mem::swap(&mut self.notified, &mut seat.notified);
        std::mem::swap(self.usage.get_mut(), &mut seat.usage);
    }

    /// The ticket `id`, labelled `agent` in the ready column, admitted by
    /// `admitter` (the Markdown tracker's gesture is its author's) and
    /// decided by `assignee`.
    fn team_ticket(&self, id: &str, admitter: &str, assignee: &str) {
        self.put_ticket(id, admitter, "Todo", &["agent"], "", "Add a greeting.");
        let file = self
            .remote
            .checkout
            .join("tickets")
            .join(id)
            .join("ticket.md");
        let text = fs::read_to_string(&file).unwrap();
        let text = text.replacen(
            "assignee = \"maintainer\"",
            &format!("assignee = \"{assignee}\""),
            1,
        );
        fs::write(file, text).unwrap();
    }
}

/// An intake run's `done`.
fn intake_done(bench: &Bench) -> Reply {
    bench.reply(None, Some(&intake_result("done", None, "")))
}

/// S14, "two runners on two machines never claim the same ticket": both
/// runners take every admitted ticket. The maintainer's takes DEMO-1 in and
/// claims it, and its Build asks; put back in the ready column by a person,
/// DEMO-1 gets no step from lea's runner, its ready snapshot saying whose
/// runner holds it, and a `do` typed there stops on the claim.
/// The remote holds one claim, the maintainer's runner's.
#[test]
fn scenario_s14_two_runners_never_claim_the_same_ticket() {
    let mut bench = Bench::team();
    let mut maintainer = bench.seat("maintainer", Taking::All);
    let mut lea = bench.seat("lea", Taking::All);
    bench.playing(&mut maintainer, |b| {
        let replies = vec![intake_done(b), b.reply(None, Some(ROUND_1))];
        let (outcomes, lines, printed) = b.drain_passes(replies, 2, |_, _| {});
        assert_eq!(
            b.runs(),
            [
                (Role::Intake, TICKET.to_owned()),
                (Role::Build, TICKET.to_owned())
            ],
            "{outcomes:?}\n{lines}\n{printed}"
        );
    });
    let claimed = bench.remote_claim(&ticket());
    let held = bench.claim_commit(&ticket());
    assert_eq!(claimed.operator, "maintainer");
    assert_eq!(
        claimed.runner,
        claims::runner_id(&maintainer.data).unwrap().as_str()
    );

    // A person puts it back in the ready column, still labelled: lea's
    // runner sees it admitted.
    move_ticket(&bench, "Todo");
    bench.playing(&mut lea, |b| {
        let (outcomes, lines, printed) = b.drain_passes(Vec::new(), 2, |_, _| {});
        let report = format!("{outcomes:?}\n{lines}\n{printed}");
        assert!(b.briefs().is_empty(), "{report}");
        let (kind, text) = made_now(b, TICKET);
        assert_eq!(kind, "waits", "{report}");
        assert!(
            text.starts_with("claimed by maintainer's runner until"),
            "{text}\n{report}"
        );
        let (stopped, printed) = b.run(Vec::new(), None);
        assert!(
            matches!(&stopped, Err(Stop::Claimed { .. })),
            "{stopped:?}\n{printed}"
        );
        assert!(b.briefs().is_empty());
        assert!(claims::read_record(&b.dirs(), &ticket()).is_none());
    });
    assert_eq!(bench.claim_commit(&ticket()), held);
    assert_eq!(
        bench
            .owlshift_refs()
            .lines()
            .filter(|line| line.contains("refs/owlshift/claims/"))
            .count(),
        1
    );
}

/// S14, "a runner never takes a ticket its operator did not delegate; a
/// team-server runner takes every admitted ticket": the maintainer admits
/// DEMO-1, lea DEMO-2, both runners under `takes = "mine"`. The
/// maintainer's runner takes in DEMO-1 alone, and its Build asks, DEMO-2
/// resting as lea's. Lea's runner takes in DEMO-2 alone; its Build waits
/// for DEMO-1, which the maintainer's runner works and whose claim holds the
/// repository. Under `takes = "all"`, lea's runner takes in DEMO-3, which
/// the maintainer admitted and no runner claims, and never DEMO-1.
#[test]
fn scenario_s14_each_runner_takes_its_operators_tickets_and_all_takes_any() {
    let mut bench = Bench::team();
    bench.team_ticket("DEMO-2", "lea", "lea");
    let mut maintainer = bench.seat("maintainer", Taking::Mine("maintainer".to_owned()));
    let mut lea = bench.seat("lea", Taking::Mine("lea".to_owned()));
    bench.playing(&mut maintainer, |b| {
        let replies = vec![intake_done(b), b.reply(None, Some(ROUND_1))];
        let (outcomes, lines, printed) = b.drain_passes(replies, 2, |_, _| {});
        let report = format!("{outcomes:?}\n{lines}\n{printed}");
        assert_eq!(
            b.runs(),
            [
                (Role::Intake, TICKET.to_owned()),
                (Role::Build, TICKET.to_owned())
            ],
            "{report}"
        );
        assert_eq!(
            made_now(b, "DEMO-2"),
            ("waits", "admitted by lea: their runner's".to_owned()),
            "{report}"
        );
    });
    assert!(bench.comments_on("DEMO-2").is_empty());
    bench.playing(&mut lea, |b| {
        let (outcomes, lines, printed) = b.drain_passes(vec![intake_done(b)], 2, |_, _| {});
        let report = format!("{outcomes:?}\n{lines}\n{printed}");
        assert_eq!(b.runs(), [(Role::Intake, "DEMO-2".to_owned())], "{report}");
        let waits = b.events().into_iter().any(|e| {
            e.ticket.as_ref().is_some_and(|t| t.as_str() == "DEMO-2")
                && e.data.get("watch") == Some(&json!("waits"))
                && e.data["reason"] == json!("collision")
                && e.data["tickets"] == json!([TICKET])
                && e.data["operator"] == json!("maintainer")
        });
        assert!(waits, "{report}");
    });
    assert_eq!(bench.remote_claim(&ticket()).operator, "maintainer");
    let demo2 = TicketId::new("DEMO-2").unwrap();
    assert_eq!(bench.remote_claim(&demo2).operator, "lea");

    bench.team_ticket("DEMO-3", "maintainer", "maintainer");
    lea.takes = Taking::All;
    bench.playing(&mut lea, |b| {
        let (outcomes, lines, printed) = b.drain_passes(vec![intake_done(b)], 2, |_, _| {});
        let runs = b.runs();
        assert_eq!(
            runs.last(),
            Some(&(Role::Intake, "DEMO-3".to_owned())),
            "{outcomes:?}\n{lines}\n{printed}"
        );
    });
    let demo3 = TicketId::new("DEMO-3").unwrap();
    // A claim names the runner's operator, whoever admitted the ticket.
    assert_eq!(bench.remote_claim(&demo3).operator, "lea");
    assert_eq!(
        bench.remote_claim(&demo3).runner,
        claims::runner_id(&lea.data).unwrap().as_str()
    );
}

/// The answers to [`ROUND_1`].
const ANSWERS: &str = "Q1: English.\nQ2: \"Hello, reader.\", no sign-off.\ngo";

/// The answers' comments on the thread of `brief`, as `(author,
/// relation)`.
fn thread_relations(brief: &Brief) -> Vec<(String, Relation)> {
    brief
        .thread
        .iter()
        .filter_map(|entry| match entry {
            ThreadEntry::Comment { author, body, .. } if body.contains("Hello, reader") => {
                Some((author.name.clone(), author.relation))
            }
            _ => None,
        })
        .collect()
}

/// S14, "only the decider's answers count; other comments are context, and
/// the answer check says so": the maintainer's runner works DEMO-1, which
/// the maintainer admitted and lea, its assignee, decides. Its round
/// notifies no one on the maintainer's machine. The maintainer's answer
/// starts no check, nor does lea's runner, which follows nothing of DEMO-1;
/// lea's answer starts the check on the maintainer's runner, whose brief
/// gives the maintainer's comment as another person's and lea's as the
/// decider's.
#[test]
fn scenario_s14_only_the_deciders_answers_count_whichever_runner_asked() {
    let mut bench = Bench::team();
    bench.team_ticket(TICKET, "maintainer", "lea");
    let mut maintainer = bench.seat("maintainer", Taking::Mine("maintainer".to_owned()));
    let mut lea = bench.seat("lea", Taking::Mine("lea".to_owned()));
    let told = |b: &Bench, outcome: &Result<Delivered, Stop>, operator: &str| {
        let tracker = MarkdownTracker::new(&b.remote.checkout);
        let mut out = Vec::new();
        let mut sink = EventSink::new(REPO, EventLog::in_dir(&b.data), &mut out);
        let before = b.notified.0.borrow().len();
        notify::notify_blocker(
            Some(&b.notified),
            &tracker,
            &ticket(),
            outcome,
            &|| Some(operator.to_owned()),
            &mut sink,
        );
        b.notified.0.borrow().len() - before
    };
    bench.playing(&mut maintainer, |b| {
        let (asked, printed) = b.run(vec![b.reply(None, Some(ROUND_1))], None);
        assert!(
            matches!(&asked, Err(Stop::NeedsInput { decider: Some(d), .. }) if d.as_str() == "lea"),
            "{asked:?}\n{printed}"
        );
        assert_eq!(told(b, &asked, "maintainer"), 0, "not the operator's");
        // Control: the same round, told to lea as the operator, is shown.
        assert_eq!(told(b, &asked, "lea"), 1);

        b.answer(ANSWERS);
        let briefs = b.briefs().len();
        let (waits, printed) = b.continue_ticket(Vec::new());
        assert!(waits.is_err(), "{waits:?}\n{printed}");
        assert_eq!(b.briefs().len(), briefs, "a check ran\n{printed}");
    });
    bench.comment_as("lea", ANSWERS);
    bench.playing(&mut lea, |b| {
        let (outcomes, lines, printed) = b.drain_passes(Vec::new(), 2, |_, _| {});
        assert!(b.briefs().is_empty(), "{outcomes:?}\n{lines}\n{printed}");
    });
    bench.playing(&mut maintainer, |b| {
        let reasked = check(&[
            ("Q1", "answered", "English."),
            ("Q2", "unanswered", "The sign-off is not settled."),
        ]);
        let (checked, printed) = b.continue_ticket(vec![b.reply(None, Some(&reasked))]);
        assert!(
            matches!(&checked, Err(Stop::Reasked { .. })),
            "{checked:?}\n{printed}"
        );
        let brief = b.last_of(Role::AnswerCheck);
        assert_eq!(
            thread_relations(&brief),
            [
                ("maintainer".to_owned(), Relation::Other),
                ("lea".to_owned(), Relation::Decider)
            ]
        );
    });
}

/// S14, "a PR review comment and a ticket comment from the same person count
/// as one decider, through the identity map; a colleague's review reaches
/// the decider as a question on the ticket and starts nothing until the
/// decider answers it": lea's runner delivers DEMO-1, which lea admitted
/// and decides. The maintainer reviews its pull request as `mo-dev`: lea's
/// runner asks lea, and runs nothing. Lea sets it aside in a ticket comment,
/// under her tracker account. Her own review, as `lea-martin`, is then the
/// decider's: the loop starts on it unasked.
#[test]
fn scenario_s14_a_colleagues_review_reaches_the_decider_who_counts_as_one_person() {
    let mut bench = Bench::team();
    bench.team_ticket(TICKET, "lea", "lea");
    let mut lea = bench.seat("lea", Taking::Mine("lea".to_owned()));
    let colleagues = review(
        8_000_000_101,
        "mo-dev",
        "Rename the file to HELLO.md.",
        &[("GREETING.md", 1, "Here.")],
    );
    bench.playing(&mut lea, |b| {
        let (delivered, printed) = b.run(vec![b.reply(Some("Hello"), Some(DONE))], None);
        delivered.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
        let briefs = b.briefs().len();
        b.reviews_on_github(vec![colleagues.clone()]);
        let (asked, printed) = b.continue_ticket(Vec::new());
        assert!(
            matches!(&asked, Err(Stop::NeedsInput { posted: Ok(_), .. })),
            "{asked:?}\n{printed}"
        );
        assert_eq!(b.briefs().len(), briefs, "no run");
        let asked = b.questions_posted();
        assert_eq!(asked.len(), 1);
        assert!(asked[0].contains("`mo-dev`"), "{}", asked[0]);

        b.comment_as("lea", "B. Set it aside.\n");
        let set_aside = chose(Some("B"), "B, set it aside.");
        let (looked, printed) = b.continue_ticket(vec![b.reply(None, Some(&set_aside))]);
        assert!(
            matches!(&looked, Err(Stop::Watched { .. })),
            "{looked:?}\n{printed}"
        );

        let own = review(
            8_000_000_102,
            "lea-martin",
            "Say hello in capitals.",
            &[("GREETING.md", 1, "Capitals.")],
        );
        b.reviews_on_github(vec![colleagues, own]);
        let (looped, printed) =
            b.continue_ticket(vec![b.reply(Some("Hello there"), Some(RENAMED))]);
        looped.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
        let given: Vec<(u64, Relation)> = b
            .last_of(Role::Build)
            .pull_request_reviews
            .iter()
            .map(|review| (review.id, review.author.relation))
            .collect();
        assert!(
            given.contains(&(8_000_000_102, Relation::Decider)),
            "{given:?}"
        );
        assert_eq!(b.questions_posted().len(), 1, "asked nothing more");
    });
}

/// S11 and S14, a handover: lea's runner takes DEMO-1 in, which lea
/// admitted, and its Build asks; then her runner stops. While its claim is
/// live, a release from the maintainer's machine is refused; a day and an
/// hour later it is released from there. The maintainer admits DEMO-1 again,
/// and his runner, which kept nothing of it, takes it in, builds it and
/// delivers it: its pull request opened, the remote's claim his runner's.
#[test]
fn scenario_s14_a_handover_ends_in_a_delivery_by_the_other_runner() {
    let mut bench = Bench::team();
    bench.team_ticket(TICKET, "lea", "lea");
    let mut maintainer = bench.seat("maintainer", Taking::Mine("maintainer".to_owned()));
    let mut lea = bench.seat("lea", Taking::Mine("lea".to_owned()));
    bench.playing(&mut lea, |b| {
        let replies = vec![intake_done(b), b.reply(None, Some(ROUND_1))];
        let (outcomes, lines, printed) = b.drain_passes(replies, 2, |_, _| {});
        assert_eq!(
            b.runs(),
            [
                (Role::Intake, TICKET.to_owned()),
                (Role::Build, TICKET.to_owned())
            ],
            "{outcomes:?}\n{lines}\n{printed}"
        );
    });
    assert_eq!(bench.remote_claim(&ticket()).operator, "lea");

    let release = |b: &Bench, later: SignedDuration| {
        let now = b.clock.get() + b.waited.get() + later;
        b.release_from(&b.dirs(), "maintainer", &move || now, STEP_WAIT, &mut || {})
    };
    bench.playing(&mut maintainer, |b| {
        let refused = release(b, SignedDuration::ZERO).unwrap_err();
        assert!(refused.contains("a live claim"), "{refused}");
        assert!(b.claim_commit(&ticket()).is_some());
        let released = release(b, SignedDuration::from_hours(25)).unwrap();
        assert!(
            released.lines[0].starts_with("Released DEMO-1's claim on origin, lea's runner"),
            "{released:?}"
        );
    });
    assert_eq!(bench.claim_commit(&ticket()), None);
    assert_eq!(bench.released_comments().len(), 1);

    bench.team_ticket(TICKET, "maintainer", "maintainer");
    bench.playing(&mut maintainer, |b| {
        assert!(
            b.record_of(TICKET).is_none(),
            "nothing of lea's runner here"
        );
        let replies = vec![
            intake_done(b),
            b.reply(Some("Hello"), Some(&done_for(TICKET))),
        ];
        let (outcomes, lines, printed) = b.drain_passes(replies, 4, |_, _| {});
        let report = format!("{outcomes:?}\n{lines}\n{printed}");
        assert_eq!(
            b.runs(),
            [
                (Role::Intake, TICKET.to_owned()),
                (Role::Build, TICKET.to_owned()),
                (Role::Verify, TICKET.to_owned()),
            ],
            "{report}"
        );
        assert_eq!(b.github.created.lock().unwrap().len(), 1, "{report}");
        assert!(b.remote_branch().is_some());
        assert!(b.record_of(TICKET).unwrap().delivery.is_some(), "{report}");
    });
    let claim = bench.remote_claim(&ticket());
    assert_eq!(claim.operator, "maintainer");
    assert_eq!(
        claim.runner,
        claims::runner_id(&maintainer.data).unwrap().as_str()
    );
}
