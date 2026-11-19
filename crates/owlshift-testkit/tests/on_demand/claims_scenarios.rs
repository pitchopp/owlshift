//! OWL-586 on watch's own process (build plan "P10 · Teams", "Claims"): a
//! ticket claimed on the bench's bare remote by `owlshift do` under
//! `policy.claims = "remote"`; a holder whose claim another runner took
//! writes nothing more on it; under `local` nothing is pushed; the sweep
//! deletes the claim of a ticket the runner no longer follows, and a
//! renewal renews the others. The claims module's races and crash windows
//! are `tests/claims.rs`'s.

use owlshift_contracts::refs::{Claim, ClaimHeld, claim_ref};
use owlshift_runner::claims::{ClaimsAt, Took};

use super::*;

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
