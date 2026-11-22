//! OWL-586 on watch's own process (build plan "P10 · Teams", "Claims"): a
//! ticket claimed on the bench's bare remote by `owlshift do` under
//! `policy.claims = "remote"`; a holder whose claim another runner took
//! writes nothing more on it; under `local` nothing is pushed; the sweep
//! deletes the claim of a ticket the runner no longer follows, and a
//! renewal renews the others. The claims module's races and crash windows
//! are `tests/claims.rs`'s. OWL-588: `owlshift release` from another
//! machine and from the holder's.

use std::sync::mpsc;
use std::thread;

use owlshift_contracts::config::Person;
use owlshift_contracts::ready::Outcome;
use owlshift_contracts::refs::{Claim, ClaimHeld, claim_ref};
use owlshift_runner::cancel;
use owlshift_runner::claims::{ClaimsAt, RunnerId, Took};
use owlshift_runner::release;

use super::drain_scenarios::{collisions, intake_declaring, ready_now};
use super::*;

/// Another runner of the bench's remote (OWL-587): its own data directory,
/// dedicated checkout and identifier.
pub(super) struct Colleague {
    dirs: ProjectDirs,
    runner: RunnerId,
}

/// The answers to [`ROUND_1`], counted at once.
const ANSWERS: &str = "Q1: English.\nQ2: \"Hello, reader.\", no sign-off.\ngo";

impl Bench {
    /// `claims = "remote"` merged to the default branch, where the runner
    /// reads it.
    pub(super) fn claims_on_remote(&self) {
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
    pub(super) fn claim_commit(&self, ticket: &TicketId) -> Option<String> {
        self.env
            .run(
                &self.remote.bare,
                &["rev-parse", "--verify", "--quiet", &claim_ref(ticket)],
            )
            .ok()
            .map(|out| String::from_utf8(out).unwrap().trim().to_owned())
    }

    /// The claim the bare remote holds of `ticket`.
    pub(super) fn remote_claim(&self, ticket: &TicketId) -> Claim {
        let file = format!("{}:claim.json", claim_ref(ticket));
        let text = self.env.run(&self.remote.bare, &["show", &file]).unwrap();
        Claim::parse(&String::from_utf8(text).unwrap()).unwrap()
    }

    /// Every ref under `refs/owlshift/` of the bare remote.
    pub(super) fn owlshift_refs(&self) -> String {
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

    pub(super) fn git(&self) -> Git {
        let env = self.env.clone();
        Git::with_setup("git", move |command| env.apply(command))
    }

    /// Another runner on the bench's remote, its checkout cloned now.
    pub(super) fn colleague(&self) -> Colleague {
        let data = self.tmp.path().join("colleague");
        let dirs = ProjectDirs::new(&data, &Repo::parse(REPO).unwrap());
        project::sync_checkout(&self.git(), &dirs, &self.remote.bare.to_string_lossy()).unwrap();
        let runner = claims::runner_id(&data).unwrap();
        Colleague { dirs, runner }
    }

    /// `colleague` claims `id` for lea, holding `files`.
    pub(super) fn claimed_by(&self, colleague: &Colleague, id: &str, files: &[&str]) {
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
    pub(super) fn unclaimed(&self, id: &str) {
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

/// How long a release waits for a watched step it stopped: well past the
/// watcher's poll.
pub(super) const STEP_WAIT: Duration = Duration::from_secs(10);

impl Bench {
    /// The bench's project file with `[people]` listing the maintainer and
    /// lea, by their Markdown tracker names (OWL-588), unless the file has
    /// its own table (OWL-589).
    fn people_config(&self) -> ProjectConfig {
        let text = fs::read_to_string(self.remote.checkout.join("owlshift.toml")).unwrap();
        let mut config = ProjectConfig::parse(&text).unwrap();
        if config.people.is_some() {
            return config;
        }
        config.people = Some(
            ["maintainer", "lea"]
                .map(|name| {
                    let person = Person {
                        tracker: name.to_owned(),
                        forge: None,
                    };
                    (name.to_owned(), person)
                })
                .into(),
        );
        config
    }

    /// `owlshift release DEMO-1` from the data directory of `dirs`, as
    /// `account`, at the time `clock` reads, a step under way waited for
    /// `step_wait` at most, with `between` happening between the release's
    /// read and its delete.
    pub(super) fn release_from(
        &self,
        dirs: &ProjectDirs,
        account: &str,
        clock: &dyn Fn() -> Timestamp,
        step_wait: Duration,
        between: &mut dyn FnMut(),
    ) -> Result<release::Released, String> {
        let git = self.git();
        let config = self.people_config();
        let tracker = MarkdownTracker::new(&self.remote.checkout);
        let operator = release::Operator::of(&config, account);
        let remote_url = self.remote.bare.to_string_lossy().into_owned();
        let releasing = release::Release {
            git: &git,
            dirs,
            remote_url: &remote_url,
            config: &config,
            tracker: &tracker,
            operator: &operator,
            clock,
            step_wait,
            from_ui: false,
        };
        let mut out = Vec::new();
        let mut sink = EventSink::new(REPO.to_owned(), EventLog::in_dir(dirs.data_dir()), &mut out);
        let read = release::read(&releasing, &ticket())?;
        between();
        release::apply(&releasing, read, &mut sink)
    }

    /// `colleague` claims DEMO-1 for `operator`, its claim taken two days
    /// ago and expired a day ago.
    fn claimed_expired(&self, colleague: &Colleague, operator: &str) {
        let git = self.git();
        let took = claims::take(
            ClaimsAt {
                git: &git,
                dirs: &colleague.dirs,
            },
            &ticket(),
            &colleague.runner,
            operator,
            ClaimHeld::default(),
            Timestamp::now() - SignedDuration::from_hours(48),
            None,
        );
        assert!(matches!(took, Ok(Took::Taken { .. })), "{took:?}");
    }

    /// The RELEASED comments on the ticket.
    pub(super) fn released_comments(&self) -> Vec<String> {
        self.comments()
            .into_iter()
            .filter(|body| body.starts_with("[owlshift] RELEASED"))
            .collect()
    }
}

/// OWL-588's acceptance from another machine: another person's live claim
/// is refused, naming its holder, its runner and its expiry, and kept; once
/// expired, a person `[people]` lists releases it. The claim is gone, a
/// RELEASED comment says who released it and names the branch the earlier
/// runner left, the ticket's stage is left as it is, and nothing of it is
/// kept here. Then taken again here by `owlshift do`, it is refused while
/// that branch is on origin, and claimed once the branch is deleted.
#[test]
fn a_live_claim_of_another_person_is_refused_and_an_expired_one_released() {
    let bench = Bench::new(true);
    bench.claims_on_remote();
    let colleague = bench.colleague();
    bench.claimed_by(&colleague, TICKET, &[]);
    let stage = bench.stage();
    let live = bench.claim_commit(&ticket());
    let refused = bench
        .release_from(
            &bench.dirs(),
            "maintainer",
            &Timestamp::now,
            STEP_WAIT,
            &mut || {},
        )
        .unwrap_err();
    let holder = format!(
        "DEMO-1 is claimed by lea's runner {} until",
        colleague.runner.as_str()
    );
    assert!(refused.contains(&holder), "{refused}");
    assert!(refused.contains(" UTC, a live claim"), "{refused}");
    assert_eq!(bench.claim_commit(&ticket()), live);
    assert!(bench.released_comments().is_empty());

    // Taken again by the colleague's runner two days ago, left to expire.
    let git = bench.git();
    let colleague_at = ClaimsAt {
        git: &git,
        dirs: &colleague.dirs,
    };
    claims::forget(colleague_at, &ticket()).unwrap();
    bench.claimed_expired(&colleague, "lea");
    bench
        .env
        .run(
            &bench.remote.checkout,
            &[
                "push",
                "--quiet",
                "origin",
                &format!("HEAD:refs/heads/{BRANCH}"),
            ],
        )
        .unwrap();
    let released = bench
        .release_from(
            &bench.dirs(),
            "maintainer",
            &Timestamp::now,
            STEP_WAIT,
            &mut || {},
        )
        .unwrap();
    assert!(
        released.lines[0].starts_with("Released DEMO-1's claim on origin, lea's runner"),
        "{released:?}"
    );
    assert_eq!(released.warning, None);
    assert_eq!(bench.claim_commit(&ticket()), None);
    let posted = bench.released_comments();
    assert_eq!(posted.len(), 1, "{posted:?}");
    for says in [
        "**Released by** `maintainer` with `owlshift release`",
        &format!(
            "`lea`'s runner `{}`, which expired at",
            colleague.runner.as_str()
        ),
        "**The branch `owlshift/demo-1` is still on origin,**",
    ] {
        assert!(posted[0].contains(says), "{says}\n{}", posted[0]);
    }
    assert_eq!(bench.stage(), stage);
    assert!(claims::lost(&bench.dirs(), &ticket()).is_none());
    assert!(!bench.fetched_claims().contains(TICKET));
    let told = data_of(&bench.events(), EventKind::Decision, "claim");
    assert_eq!(told.len(), 1, "{told:?}");
    assert_eq!(told[0]["claim"], json!("released"));
    assert_eq!(told[0]["operator"], json!("lea"));
    assert_eq!(told[0]["expired"], json!(true));
    assert_eq!(told[0]["released_by"], json!("maintainer"));

    let (refused, printed) = bench.run(Vec::new(), None);
    let Err(Stop::Refused(why)) = &refused else {
        panic!("{refused:?}\n{printed}");
    };
    assert!(
        why.contains("the branch owlshift/demo-1 is on origin"),
        "{why}"
    );
    assert_eq!(bench.claim_commit(&ticket()), None);
    bench
        .env
        .run(
            &bench.remote.bare,
            &["update-ref", "-d", &format!("refs/heads/{BRANCH}")],
        )
        .unwrap();
    let (asked, printed) = bench.run(vec![bench.reply(None, Some(ROUND_1))], None);
    assert!(
        matches!(asked, Err(Stop::NeedsInput { .. })),
        "{asked:?}\n{printed}"
    );
    let runner = claims::runner_id(&bench.data).unwrap();
    assert_eq!(bench.remote_claim(&ticket()).runner, runner.as_str());
}

/// OWL-588's acceptance: the operator's own live claim is released from a
/// second data directory, as after one was lost; a holder that renewed
/// between the release's read and its delete keeps its claim, and nothing
/// is posted.
#[test]
fn the_operators_own_claim_is_released_from_a_second_data_directory_unless_renewed() {
    let bench = Bench::new(true);
    bench.claims_on_remote();
    let colleague = bench.colleague();
    let git = bench.git();
    let at = ClaimsAt {
        git: &git,
        dirs: &colleague.dirs,
    };
    let took = claims::take(
        at,
        &ticket(),
        &colleague.runner,
        "maintainer",
        ClaimHeld::default(),
        Timestamp::now(),
        None,
    );
    assert!(matches!(took, Ok(Took::Taken { .. })), "{took:?}");
    let mut renewed = None;
    let refused = bench
        .release_from(
            &bench.dirs(),
            "maintainer",
            &Timestamp::now,
            STEP_WAIT,
            &mut || {
                let renewal = claims::renew(
                    at,
                    &|_| ClaimHeld::default(),
                    Timestamp::now() + SignedDuration::from_hours(1),
                );
                assert_eq!(renewal.renewed, [ticket()], "{renewal:?}");
                renewed = bench.claim_commit(&ticket());
            },
        )
        .unwrap_err();
    assert!(
        refused.contains("moved on origin since it was read"),
        "{refused}"
    );
    assert!(renewed.is_some());
    assert_eq!(bench.claim_commit(&ticket()), renewed);
    assert!(bench.released_comments().is_empty());

    bench
        .release_from(
            &bench.dirs(),
            "maintainer",
            &Timestamp::now,
            STEP_WAIT,
            &mut || {},
        )
        .unwrap();
    assert_eq!(bench.claim_commit(&ticket()), None);
    assert_eq!(bench.released_comments().len(), 1);
}

/// OWL-588: on the holder's machine the ticket's step under way is stopped
/// first, its watcher finding the lost record, and waited for; then its
/// claim is deleted, its record with it, and the ticket is left out of
/// every count here. A step no watcher stops, as a run typed in a
/// terminal, is waited for in vain: the claim is kept and no longer
/// renewed.
#[test]
fn on_the_holders_machine_the_step_under_way_is_stopped_first() {
    let bench = Bench::new(true);
    bench.claims_on_remote();
    let dirs = bench.dirs();
    let remote = bench.remote.bare.to_string_lossy().into_owned();
    project::sync_checkout(&bench.git(), &dirs, &remote).unwrap();
    bench.claim(&ticket());
    let (ready, started) = mpsc::channel();
    let step_dirs = dirs.clone();
    let step = thread::spawn(move || {
        let lock = step_dirs.ticket_lock(&ticket()).unwrap().unwrap();
        let watcher = cancel::Watcher::start(&step_dirs, &ticket());
        ready.send(()).unwrap();
        while !watcher.fired() {
            thread::sleep(Duration::from_millis(10));
        }
        let cause = watcher.cause();
        drop(watcher);
        drop(lock);
        cause
    });
    started.recv().unwrap();
    let released = bench
        .release_from(&dirs, "maintainer", &Timestamp::now, STEP_WAIT, &mut || {})
        .unwrap();
    assert_eq!(step.join().unwrap(), Some(cancel::Cause::ClaimLost));
    assert!(
        released
            .lines
            .iter()
            .any(|line| line.starts_with("This runner's step of DEMO-1 was stopped")),
        "{released:?}"
    );
    assert_eq!(bench.claim_commit(&ticket()), None);
    assert!(claims::read_record(&dirs, &ticket()).is_none());
    assert!(claims::lost_tickets(&dirs).contains(&ticket()));

    // A step no watcher stops holds the ticket's lock past the wait.
    fs::remove_file(claims::lost_file(&dirs, &ticket())).unwrap();
    bench.claim(&ticket());
    let held = dirs.ticket_lock(&ticket()).unwrap().unwrap();
    let kept = bench.claim_commit(&ticket());
    let refused = bench
        .release_from(
            &dirs,
            "maintainer",
            &Timestamp::now,
            Duration::from_secs(1),
            &mut || {},
        )
        .unwrap_err();
    assert!(refused.contains("has not ended within 1 s:"), "{refused}");
    assert!(refused.contains("A run typed in a terminal"), "{refused}");
    assert_eq!(bench.claim_commit(&ticket()), kept);
    assert!(claims::lost(&dirs, &ticket()).is_some());
    let git = bench.git();
    let renewal = claims::renew(
        ClaimsAt {
            git: &git,
            dirs: &dirs,
        },
        &|_| ClaimHeld::default(),
        Timestamp::now() + SignedDuration::from_hours(1),
    );
    assert!(renewal.renewed.is_empty(), "{renewal:?}");
    assert_eq!(bench.claim_commit(&ticket()), kept);
    drop(held);
}
