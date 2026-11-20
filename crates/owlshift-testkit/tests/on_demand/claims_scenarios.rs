//! OWL-586 on watch's own process (build plan "P10 · Teams", "Claims"): a
//! ticket claimed on the bench's bare remote by `owlshift do` under
//! `policy.claims = "remote"`; a holder whose claim another runner took
//! writes nothing more on it; under `local` nothing is pushed; the sweep
//! deletes the claim of a ticket the runner no longer follows, and a
//! renewal renews the others. The claims module's races and crash windows
//! are `tests/claims.rs`'s.

use owlshift_contracts::ready::Outcome;
use owlshift_contracts::refs::{Claim, ClaimHeld, claim_ref};
use owlshift_runner::claims::{ClaimsAt, RunnerId, Took};

use super::drain_scenarios::{collisions, intake_declaring, ready_now};
use super::*;

/// Another runner of the bench's remote (OWL-587): its own data directory,
/// dedicated checkout and identifier.
struct Colleague {
    dirs: ProjectDirs,
    runner: RunnerId,
}

/// The answers to [`ROUND_1`], counted at once.
const ANSWERS: &str = "Q1: English.\nQ2: \"Hello, reader.\", no sign-off.\ngo";

impl Bench {
    /// `claims = "remote"` merged to the default branch, where the runner
    /// reads it.
    fn claims_on_remote(&self) {
        self.policy("claims = \"remote\"");
        let checkout = &self.remote.checkout;
        self.env.run(checkout, &["add", "owlshift.toml"]).unwrap();
        self.env
            .run(
                checkout,
                &["commit", "--quiet", "-m", "Claim tickets on the remote"],
            )
            .unwrap();
        self.env
            .run(
                checkout,
                &["push", "--quiet", "origin", "HEAD:refs/heads/main"],
            )
            .unwrap();
    }

    /// The commit the bare remote holds as `ticket`'s claim.
    fn claim_commit(&self, ticket: &TicketId) -> Option<String> {
        self.env
            .run(
                &self.remote.bare,
                &["rev-parse", "--verify", "--quiet", &claim_ref(ticket)],
            )
            .ok()
            .map(|out| String::from_utf8(out).unwrap().trim().to_owned())
    }

    /// The claim the bare remote holds of `ticket`.
    fn remote_claim(&self, ticket: &TicketId) -> Claim {
        let file = format!("{}:claim.json", claim_ref(ticket));
        let text = self.env.run(&self.remote.bare, &["show", &file]).unwrap();
        Claim::parse(&String::from_utf8(text).unwrap()).unwrap()
    }

    /// Every ref under `refs/owlshift/` of the bare remote.
    fn owlshift_refs(&self) -> String {
        let out = self
            .env
            .run(&self.remote.bare, &["for-each-ref", "refs/owlshift/"])
            .unwrap();
        String::from_utf8(out).unwrap()
    }

    /// This bench's runner claiming `ticket` itself, through the claims
    /// module, as a command would before its first write.
    fn claim(&self, ticket: &TicketId) {
        let git = self.git();
        let dirs = self.dirs();
        let runner = claims::runner_id(&self.data).unwrap();
        let took = claims::take(
            ClaimsAt {
                git: &git,
                dirs: &dirs,
            },
            ticket,
            &runner,
            "maintainer",
            ClaimHeld::default(),
            Timestamp::now(),
            None,
        );
        assert!(matches!(took, Ok(Took::Taken { .. })), "{took:?}");
    }

    /// Another runner, with a data directory and a dedicated checkout of its
    /// own, takes the ticket once its claim is gone from the remote, as
    /// after a release.
    fn taken_by_another(&self) -> String {
        self.env
            .run(
                &self.remote.bare,
                &["update-ref", "-d", &claim_ref(&ticket())],
            )
            .unwrap();
        let data = self.tmp.path().join("other-runner");
        let dirs = ProjectDirs::new(&data, &Repo::parse(REPO).unwrap());
        let git = self.git();
        project::sync_checkout(&git, &dirs, &self.remote.bare.to_string_lossy()).unwrap();
        let runner = claims::runner_id(&data).unwrap();
        let took = claims::take(
            ClaimsAt {
                git: &git,
                dirs: &dirs,
            },
            &ticket(),
            &runner,
            "colleague",
            ClaimHeld::default(),
            Timestamp::now(),
            None,
        );
        assert!(matches!(took, Ok(Took::Taken { .. })), "{took:?}");
        runner.as_str().to_owned()
    }

    fn git(&self) -> Git {
        let env = self.env.clone();
        Git::with_setup("git", move |command| env.apply(command))
    }

    /// Another runner on the bench's remote, its checkout cloned now.
    fn colleague(&self) -> Colleague {
        let data = self.tmp.path().join("colleague");
        let dirs = ProjectDirs::new(&data, &Repo::parse(REPO).unwrap());
        project::sync_checkout(&self.git(), &dirs, &self.remote.bare.to_string_lossy()).unwrap();
        let runner = claims::runner_id(&data).unwrap();
        Colleague { dirs, runner }
    }

    /// `colleague` claims `id` for lea, holding `files`.
    fn claimed_by(&self, colleague: &Colleague, id: &str, files: &[&str]) {
        let git = self.git();
        let held = ClaimHeld {
            files: files.iter().map(|f| (*f).to_owned()).collect(),
            ..ClaimHeld::default()
        };
        let took = claims::take(
            ClaimsAt {
                git: &git,
                dirs: &colleague.dirs,
            },
            &TicketId::new(id).unwrap(),
            &colleague.runner,
            "lea",
            held,
            Timestamp::now(),
            None,
        );
        assert!(matches!(took, Ok(Took::Taken { .. })), "{took:?}");
    }

    /// A claim of `id` on the bench's remote in a format later than this
    /// binary reads, as a newer Owlshift would push it.
    fn claimed_later(&self, id: &str) {
        let dir = self.tmp.path().join(format!("later-{id}"));
        fs::create_dir_all(&dir).unwrap();
        let text = r#"{"format":3,"runner":"0123456789abcdef","operator":"lea","claimed_at":"2026-11-20T09:00:00Z","expires_at":"2026-11-21T09:00:00Z","what_is_new":true}"#;
        fs::write(dir.join("claim.json"), text).unwrap();
        self.env.run(&dir, &["init", "--quiet"]).unwrap();
        self.env.run(&dir, &["add", "claim.json"]).unwrap();
        self.env
            .run(&dir, &["commit", "--quiet", "-m", "A later claim"])
            .unwrap();
        let to = format!("HEAD:{}", claim_ref(&TicketId::new(id).unwrap()));
        let bare = self.remote.bare.to_string_lossy().into_owned();
        self.env
            .run(&dir, &["push", "--quiet", &bare, &to])
            .unwrap();
    }

    /// Deletes `id`'s claim on the bench's remote, as its holder's sweep
    /// or a release does.
    fn unclaimed(&self, id: &str) {
        let name = claim_ref(&TicketId::new(id).unwrap());
        self.env
            .run(&self.remote.bare, &["update-ref", "-d", &name])
            .unwrap();
    }

    /// The claims the dedicated checkout fetched, by ref.
    fn fetched_claims(&self) -> String {
        let out = self
            .env
            .run(
                &self.dirs().checkout(),
                &[
                    "for-each-ref",
                    "--format=%(refname)",
                    claims::FETCHED_PREFIX,
                ],
            )
            .unwrap();
        String::from_utf8(out).unwrap()
    }

    /// The `claim_lost` warnings recorded.
    fn lost_warnings(&self) -> Vec<Value> {
        data_of(&self.events(), EventKind::Warning, "what")
            .into_iter()
            .filter(|data| data["what"] == json!("claim_lost"))
            .collect()
    }
}

/// The acceptance's second case: `do` claims the ticket and asks a round;
/// another runner deletes the claim and takes it; the decider answers. The
/// first runner's watch finds the claim lost at its renewal: one warning,
/// one notification, the record that stops a step under way, and nothing
/// more on the tracker, the forge or the branch: no answer check, no
/// comment, no stage move, no push. A person's `do` there is refused.
#[test]
fn a_holder_whose_claim_another_runner_took_writes_nothing_more() {
    let bench = Bench::new(true);
    bench.claims_on_remote();
    let (asked, printed) = bench.run(vec![bench.reply(None, Some(ROUND_1))], None);
    assert!(
        matches!(asked, Err(Stop::NeedsInput { .. })),
        "{asked:?}\n{printed}"
    );
    let taken = data_of(&bench.events(), EventKind::Decision, "claim");
    assert_eq!(taken.len(), 1, "{taken:?}");
    assert_eq!(taken[0]["claim"], json!("taken"));
    assert_eq!(taken[0]["operator"], json!("maintainer"));
    assert!(bench.claim_commit(&ticket()).is_some());

    let other = bench.taken_by_another();
    bench.answer(ANSWERS);
    let (comments, briefs, stage) = (bench.comments(), bench.briefs().len(), bench.stage());
    let branch = bench.remote_branch();
    let (outcomes, lines, printed) = bench.watch(Vec::new(), &mut |_| false);
    assert!(outcomes.is_empty(), "{outcomes:?}\n{lines}\n{printed}");
    assert_eq!(bench.briefs().len(), briefs, "an answer check ran\n{lines}");
    assert_eq!(bench.comments(), comments);
    assert_eq!(bench.stage(), stage);
    assert_eq!(bench.remote_branch(), branch);
    assert_eq!(bench.remote_claim(&ticket()).runner, other);
    assert!(claims::lost(&bench.dirs(), &ticket()).is_some());
    assert!(claims::read_record(&bench.dirs(), &ticket()).is_none());
    let warned = bench.lost_warnings();
    assert_eq!(warned.len(), 1, "{warned:?}");
    assert_eq!(warned[0]["operator"], json!("colleague"));
    assert!(
        lines.contains("DEMO-1: its claim on origin was lost"),
        "{lines}"
    );
    let told = || {
        bench
            .notified
            .0
            .borrow()
            .iter()
            .filter(|line| line.contains("claim"))
            .count()
    };
    assert_eq!(told(), 1, "{:?}", bench.notified.0.borrow());

    // Told once: a second watch tells nothing more, and runs nothing.
    let (outcomes, lines, _) = bench.watch(Vec::new(), &mut |_| false);
    assert!(outcomes.is_empty(), "{outcomes:?}\n{lines}");
    assert_eq!(bench.lost_warnings().len(), 1);
    assert_eq!(told(), 1);
    assert_eq!(bench.comments(), comments);

    let (refused, printed) = bench.run(Vec::new(), None);
    let Err(Stop::Refused(why)) = &refused else {
        panic!("{refused:?}\n{printed}");
    };
    assert!(why.contains("lost its claim on DEMO-1"), "{why}");
    assert!(why.contains("`owlshift forget DEMO-1`"), "{why}");
    assert_eq!(bench.comments(), comments);
}

/// The acceptance's last case: under `claims = "local"`, the default, a
/// round asked and a watch pass push nothing under `refs/owlshift/`, and
/// no runner identifier is made.
#[test]
fn under_local_claims_nothing_is_pushed() {
    let bench = Bench::new(true);
    let (asked, printed) = bench.run(vec![bench.reply(None, Some(ROUND_1))], None);
    assert!(
        matches!(asked, Err(Stop::NeedsInput { .. })),
        "{asked:?}\n{printed}"
    );
    let (outcomes, lines, _) = bench.watch(Vec::new(), &mut |_| false);
    assert!(outcomes.is_empty(), "{outcomes:?}\n{lines}");
    assert_eq!(bench.owlshift_refs(), "");
    assert!(claims::read_record(&bench.dirs(), &ticket()).is_none());
    assert!(!bench.data.join(claims::RUNNER_FILE).exists());
    assert!(data_of(&bench.events(), EventKind::Decision, "claim").is_empty());
}

/// A watch pass sweeps the claim of a ticket the runner holds nothing of,
/// a crash having come between its take and the ref's first write, and
/// renews the claim of the ticket it follows.
#[test]
fn a_pass_deletes_the_claim_of_a_ticket_without_a_ref_and_renews_the_others() {
    let bench = Bench::new(true);
    bench.claims_on_remote();
    let (asked, printed) = bench.run(vec![bench.reply(None, Some(ROUND_1))], None);
    assert!(
        matches!(asked, Err(Stop::NeedsInput { .. })),
        "{asked:?}\n{printed}"
    );
    let unwritten = TicketId::new("DEMO-2").unwrap();
    bench.claim(&unwritten);
    let before = bench.remote_claim(&ticket());

    let (outcomes, lines, _) = bench.watch(Vec::new(), &mut |_| false);
    assert!(outcomes.is_empty(), "{outcomes:?}\n{lines}");
    assert_eq!(bench.claim_commit(&unwritten), None);
    assert!(claims::read_record(&bench.dirs(), &unwritten).is_none());
    let deleted = data_of(&bench.events(), EventKind::Decision, "claim");
    assert!(
        deleted.iter().any(|data| data["claim"] == json!("deleted")),
        "{deleted:?}"
    );
    let after = bench.remote_claim(&ticket());
    assert_eq!(after.claimed_at, before.claimed_at);
    assert!(after.expires_at >= before.expires_at, "{after:?}");
    assert_eq!(
        claims::read_record(&bench.dirs(), &ticket())
            .unwrap()
            .commit,
        bench.claim_commit(&ticket())
    );
}

/// The `warning` events of `what`, by ticket.
fn warned(bench: &Bench, what: &str) -> Vec<(String, Value)> {
    bench
        .events()
        .into_iter()
        .filter(|e| e.kind == EventKind::Warning && e.data["what"] == json!(what))
        .map(|e| {
            let ticket = e.ticket.map_or_else(String::new, |t| t.as_str().to_owned());
            (ticket, Value::Object(e.data))
        })
        .collect()
}

/// The tickets a Build run started for.
fn built(bench: &Bench) -> Vec<String> {
    bench
        .events()
        .into_iter()
        .filter(|e| e.kind == EventKind::RunStarted && e.data["role"] == json!("build"))
        .map(|e| e.ticket.unwrap().as_str().to_owned())
        .collect()
}

/// OWL-587's acceptance, its first case: another runner claims DEMO-7,
/// holding `app/one.rs`. DEMO-1, its intake declaring `app`, is dispatched,
/// the project's capacity of one being this runner's alone, and its Build
/// waits for the claim, its wait naming DEMO-7 and lea, its operator: no
/// Build runs and nothing is admitted. Once the claim is deleted on the
/// remote, the next watch's first pass fetches the claims, and the Build is
/// admitted and runs.
#[test]
fn a_build_waits_for_a_file_another_runners_claim_holds_until_it_is_deleted() {
    let bench = Bench::draining("maintainer");
    bench.claims_on_remote();
    let colleague = bench.colleague();
    bench.claimed_by(&colleague, "DEMO-7", &["app/one.rs"]);
    let intake = bench.reply(None, Some(&intake_declaring(&["app"], &[])));
    let (_, lines, printed) = bench.drain_passes(vec![intake], 4, |_, _| {});
    let report = format!("{lines}\n{printed}");
    let events = bench.events();
    assert!(
        events
            .iter()
            .any(|e| e.kind == EventKind::Dispatch && e.data["command"] == json!("drain")),
        "{report}"
    );
    assert!(built(&bench).is_empty(), "{report}");
    let told = collisions(&events, "watch");
    assert!(
        told.contains(&(
            "DEMO-1".to_owned(),
            json!(["DEMO-7"]),
            json!({ "zone": "app/one.rs" })
        )),
        "{told:?}\n{report}"
    );
    let named = events
        .iter()
        .filter(|e| e.data.get("watch") == Some(&json!("waits")))
        .all(|e| e.data["operator"] == json!("lea") && e.data["runner"].is_string());
    assert!(named, "{report}");
    assert!(
        lines.contains(
            "DEMO-1's Build waits: DEMO-7, which lea's runner works, holds zone `app/one.rs`"
        ),
        "{report}"
    );
    assert!(!bench.record_of("DEMO-1").unwrap().held.unwrap().admitted());

    bench.unclaimed("DEMO-7");
    let (_, lines, printed) =
        bench.drain_passes(vec![bench.reply(None, Some(ROUND_1))], 3, |_, _| {});
    assert_eq!(built(&bench), ["DEMO-1"], "{lines}\n{printed}");
    assert!(bench.record_of("DEMO-1").unwrap().held.unwrap().admitted());
}

/// OWL-587 under `claims = "local"`, the default: a claim another runner
/// pushed is never read, so `do`'s Build, which holds the whole repository
/// with no intake, is admitted and runs; and watch's passes fetch no claim
/// of their own.
#[test]
fn under_local_claims_another_runners_claim_is_never_read() {
    let bench = Bench::new(true);
    let colleague = bench.colleague();
    bench.claimed_by(&colleague, "DEMO-7", &["README.md"]);
    let (asked, printed) = bench.run(vec![bench.reply(None, Some(ROUND_1))], None);
    assert!(
        matches!(asked, Err(Stop::NeedsInput { .. })),
        "{asked:?}\n{printed}"
    );
    assert_eq!(built(&bench), ["DEMO-1"]);
    // Fetched by `do`'s fetch, as every fetch brings them, and unread.
    assert!(bench.fetched_claims().contains("DEMO-7"));
    bench.claimed_by(&colleague, "DEMO-8", &["README.md"]);
    let (outcomes, lines, _) = bench.watch(Vec::new(), &mut |_| false);
    assert!(outcomes.is_empty(), "{outcomes:?}\n{lines}");
    assert!(!bench.fetched_claims().contains("DEMO-8"), "{lines}");
}

/// OWL-587's acceptance, its second case: another runner claims DEMO-1,
/// admitted in the ready column, and DEMO-5, which a person moved to the
/// working state with no `zone:` label; a later Owlshift pushed a claim of
/// DEMO-8 this one cannot read. Over three passes DEMO-1 gets no step, and
/// waits as claimed by lea's runner in the ready snapshot alone; DEMO-5 is
/// warned of as no person's ticket, where DEMO-6, unclaimed, is; DEMO-8's
/// claim is warned of once, as a later format.
#[test]
fn a_claimed_ticket_is_neither_taken_nor_warned_of_as_a_persons() {
    let bench = Bench::draining("maintainer");
    bench.claims_on_remote();
    bench.put_ticket("DEMO-5", "maintainer", "In Progress", &[], "", "By hand.");
    bench.put_ticket("DEMO-6", "maintainer", "In Progress", &[], "", "By hand.");
    let colleague = bench.colleague();
    bench.claimed_by(&colleague, "DEMO-1", &[]);
    bench.claimed_by(&colleague, "DEMO-5", &[]);
    bench.claimed_later("DEMO-8");
    let (outcomes, lines, printed) = bench.drain_passes(Vec::new(), 3, |_, _| {});
    let report = format!("{outcomes:?}\n{lines}\n{printed}");
    assert!(
        !bench
            .events()
            .iter()
            .any(|e| e.kind == EventKind::RunStarted),
        "{report}"
    );
    let snapshot = ready_now(&bench).expect("a ready snapshot");
    let demo1 = snapshot
        .tickets
        .iter()
        .find(|ready| ready.ticket.as_str() == "DEMO-1")
        .unwrap();
    let Outcome::Waits { wait } = &demo1.drain else {
        panic!("{demo1:?}\n{report}");
    };
    assert_eq!(wait.reason, "claimed", "{report}");
    assert!(
        wait.text.starts_with("claimed by lea's runner until"),
        "{wait:?}"
    );
    let unknown: Vec<String> = warned(&bench, "zones_unknown")
        .into_iter()
        .map(|(ticket, _)| ticket)
        .collect();
    assert_eq!(unknown, ["DEMO-6"], "{report}");
    let unread = warned(&bench, "claim_unread");
    assert_eq!(unread.len(), 1, "{unread:?}\n{report}");
    assert_eq!(unread[0].0, "DEMO-8");
    assert_eq!(unread[0].1["newer"], json!(true));
}

/// OWL-587: a claim of this runner's that another runner took over between
/// two passes is found lost at the next pass that fetches the claims, a
/// minute and more later, without waiting for the hourly renewal: one
/// `claim_lost` warning, the record that stops a step under way.
#[test]
fn a_pass_finds_a_claim_lost_before_the_renewal() {
    let bench = Bench::new(true);
    bench.claims_on_remote();
    let (asked, printed) = bench.run(vec![bench.reply(None, Some(ROUND_1))], None);
    assert!(
        matches!(asked, Err(Stop::NeedsInput { .. })),
        "{asked:?}\n{printed}"
    );
    let mut pass = 0;
    let (outcomes, lines, _) = bench.watch_passes(Vec::new(), &mut |_| {
        pass += 1;
        if pass > 1 {
            return false;
        }
        assert!(bench.lost_warnings().is_empty());
        bench.taken_by_another();
        let later = bench.waited.get() + SignedDuration::from_mins(2);
        bench.waited.set(later);
        true
    });
    assert!(outcomes.is_empty(), "{outcomes:?}\n{lines}");
    let lost = bench.lost_warnings();
    assert_eq!(lost.len(), 1, "{lost:?}\n{lines}");
    assert_eq!(lost[0]["operator"], json!("colleague"));
    assert!(claims::lost(&bench.dirs(), &ticket()).is_some());
}

/// Whether `args` list refs or read objects (OWL-587).
fn lists_or_reads(args: &[String]) -> bool {
    reads_objects(args) || args.iter().any(|arg| arg == "for-each-ref")
}

/// OWL-587, after OWL-492: under remote claims, the admission's section
/// reads the claims of other runners with the ticket refs in a fixed number
/// of git commands. DEMO-1's ref, once asked, is set back at Ready and
/// DEMO-1 dispatched again, its admission reading the other refs and the
/// claims in the project's section: with 2 and with 8 claims of another
/// runner's fetched, the git commands that read objects under the lock are
/// the same.
#[test]
fn the_sections_read_the_claims_in_a_fixed_number_of_git_commands() {
    let bench = Bench::new(true);
    bench.claims_on_remote();
    let (asked, printed) = bench.run(vec![bench.reply(None, Some(ROUND_1))], None);
    assert!(
        matches!(asked, Err(Stop::NeedsInput { .. })),
        "{asked:?}\n{printed}"
    );
    let colleague = bench.colleague();
    let mut planted = 1;
    let mut held = |claims: usize| {
        for n in planted + 1..=claims + 1 {
            bench.claimed_by(&colleague, &format!("DEMO-{}", n + 10), &[]);
        }
        planted = claims + 1;
        bench.write_record(&ticket_ref::blank(None));
        *bench.lock_probe.lock().unwrap() = Some(LockProbe::of(lists_or_reads));
        let (asked, printed) = bench.run(vec![bench.reply(None, Some(ROUND_1))], None);
        assert!(
            matches!(asked, Err(Stop::NeedsInput { .. })),
            "{asked:?}\n{printed}"
        );
        let reads = bench.lock_probe.lock().unwrap().take().unwrap().reads;
        reads
            .into_iter()
            .filter(|(_, free)| !free)
            .map(|(args, _)| {
                let mut words = args.iter().map(String::as_str);
                let mut shape = Vec::new();
                while let Some(word) = words.next() {
                    if word == "-c" {
                        words.next();
                    } else if word.len() == 40 && word.bytes().all(|b| b.is_ascii_hexdigit()) {
                        shape.push("<id>");
                    } else {
                        shape.push(word);
                    }
                }
                shape.join(" ")
            })
            .collect::<Vec<_>>()
    };
    let (few, many) = (held(2), held(8));
    assert!(bench.fetched_claims().lines().count() >= 8);
    let listing = format!(
        "for-each-ref --format=%(objectname) %(symref) %(refname) refs/owlshift/tickets/ {}",
        claims::FETCHED_PREFIX
    );
    assert!(
        few.contains(&listing),
        "no listing of the claims under the lock: {few:?}"
    );
    assert_eq!(few, many);
}
