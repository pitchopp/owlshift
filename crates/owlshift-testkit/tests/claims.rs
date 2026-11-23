//! Claims on the git remote (OWL-586; build plan "P10 · Teams", "Claims"),
//! the runner's `claims` module against a local bare remote with hermetic
//! git: two runners, each with its data directory and its dedicated
//! checkout, take, renew, push a held set and delete one ticket's claim.

use std::collections::BTreeMap;
use std::fs;
use std::sync::Barrier;
use std::time::SystemTime;

use jiff::Timestamp;

use owlshift_adapters::forge::Repo;
use owlshift_contracts::format::Format;
use owlshift_contracts::ids::TicketId;
use owlshift_contracts::refs::{Claim, ClaimHeld, RepairClaim, claim_ref, repair_claim_ref};
use owlshift_runner::claims::repair::{self, Pushed};
use owlshift_runner::claims::{self, Checked, ClaimsAt, Deleted, NotTaken, RunnerId, Took};
use owlshift_runner::executor::Git;
use owlshift_runner::project::{self, ProjectDirs};
use owlshift_runner::ticket_ref;
use owlshift_testkit::git::{GitEnv, Remote, seed};

const REPO: &str = "acme/web";

/// One runner: its data directory, its project folder with a dedicated
/// checkout cloned from the remote, and its identifier.
struct Runner {
    dirs: ProjectDirs,
    git: Git,
    id: RunnerId,
    operator: &'static str,
}

impl Runner {
    fn at(&self) -> ClaimsAt<'_> {
        ClaimsAt {
            git: &self.git,
            dirs: &self.dirs,
        }
    }

    fn take(&self, ticket: &TicketId, branch: Option<&str>) -> Result<Took, NotTaken> {
        claims::take(
            self.at(),
            ticket,
            &self.id,
            self.operator,
            ClaimHeld::default(),
            Timestamp::now(),
            branch,
        )
    }

    /// Every ref of its dedicated checkout but the remote-tracking ones,
    /// which the isolation check leaves out.
    fn local_refs(&self) -> String {
        let out = self
            .git_run(&["for-each-ref", "--format=%(refname) %(objectname)"])
            .unwrap();
        out.lines()
            .filter(|line| !line.starts_with("refs/remotes/"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn git_run(&self, args: &[&str]) -> Result<String, String> {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(self.dirs.checkout())
            .output()
            .map_err(|e| e.to_string())?;
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }
}

struct Bench {
    env: GitEnv,
    remote: Remote,
    root: tempfile::TempDir,
}

impl Bench {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let env = GitEnv::create(root.path().join("home")).unwrap();
        let project = root.path().join("project");
        fs::create_dir(&project).unwrap();
        fs::write(project.join("README.md"), "seed\n").unwrap();
        let remote = seed(&env, root.path(), &project).unwrap();
        Self { env, remote, root }
    }

    fn runner(&self, name: &str, operator: &'static str) -> Runner {
        let data = self.root.path().join(name);
        let dirs = ProjectDirs::new(&data, &Repo::parse(REPO).unwrap());
        let env = self.env.clone();
        let git = Git::with_setup("git", move |command| env.apply(command));
        project::sync_checkout(&git, &dirs, &self.remote.bare.to_string_lossy()).unwrap();
        let id = claims::runner_id(&data).unwrap();
        Runner {
            dirs,
            git,
            id,
            operator,
        }
    }

    /// The commit the remote holds at `name`, if any.
    fn remote_ref(&self, name: &str) -> Option<String> {
        self.env
            .run(
                &self.remote.bare,
                &["rev-parse", "--verify", "--quiet", name],
            )
            .ok()
            .map(|out| String::from_utf8(out).unwrap().trim().to_owned())
    }

    /// The claim the remote holds of `ticket`.
    fn remote_claim(&self, ticket: &TicketId) -> Claim {
        let file = format!("{}:claim.json", claim_ref(ticket));
        let text = self.env.run(&self.remote.bare, &["show", &file]).unwrap();
        Claim::parse(&String::from_utf8(text).unwrap()).unwrap()
    }

    /// Deletes `ticket`'s claim on the remote, as a person's release or
    /// another runner's delete does.
    fn release(&self, ticket: &TicketId) {
        self.env
            .run(&self.remote.bare, &["update-ref", "-d", &claim_ref(ticket)])
            .unwrap();
    }
}

fn ticket(id: &str) -> TicketId {
    TicketId::new(id).unwrap()
}

/// `runner`'s own fetch of the claims, as watch makes it (OWL-587), then
/// its own claims as the listing reads them, by commit.
fn fetch_own(runner: &Runner) -> BTreeMap<TicketId, String> {
    let checkout = runner.dirs.checkout();
    claims::fetch(&runner.git, &checkout).unwrap();
    let listing = ticket_ref::listing_with(&runner.git, &checkout, true).unwrap();
    let (_, fetched) = ticket_ref::read_listing(&runner.git, &checkout, listing);
    fetched
        .into_iter()
        .filter(|claim| claim.is_of(&runner.id))
        .map(|claim| (claim.ticket, claim.commit))
        .collect()
}

/// OWL-587: a pass's check of this runner's claims against its own fetch.
/// Nothing moved: nothing lost. A claim taken once the fetch started, which
/// the fetch does not show, is kept with no read of the remote. A claim
/// another runner took over since is lost, its holder named, its record
/// removed; the other claim stays.
#[test]
fn a_fetch_finds_a_claim_lost_and_keeps_a_take_newer_than_it() {
    let bench = Bench::new();
    let ann = bench.runner("ann", "ann");
    let bob = bench.runner("bob", "bob");
    let (one, two) = (ticket("DEMO-1"), ticket("DEMO-2"));
    assert!(matches!(ann.take(&one, None), Ok(Took::Taken { .. })));
    std::thread::sleep(std::time::Duration::from_millis(20));
    let since = SystemTime::now();
    let fetched = fetch_own(&ann);
    assert_eq!(fetched.len(), 1);
    assert_eq!(
        claims::check_fetched(ann.at(), &fetched, since),
        Checked::default()
    );

    assert!(matches!(ann.take(&two, None), Ok(Took::Taken { .. })));
    assert_eq!(
        claims::check_fetched(ann.at(), &fetched, since),
        Checked::default()
    );

    bench.release(&one);
    assert!(matches!(bob.take(&one, None), Ok(Took::Taken { .. })));
    std::thread::sleep(std::time::Duration::from_millis(20));
    let since = SystemTime::now();
    let fetched = fetch_own(&ann);
    let checked = claims::check_fetched(ann.at(), &fetched, since);
    assert!(
        checked.adopted.is_empty() && checked.failed.is_none(),
        "{checked:?}"
    );
    let [(lost, Some(holder))] = checked.lost.as_slice() else {
        panic!("{checked:?}");
    };
    assert_eq!((lost, holder.operator.as_str()), (&one, "bob"));
    assert!(claims::read_record(&ann.dirs, &one).is_none());
    assert!(claims::read_record(&ann.dirs, &two).is_some());
}

/// The acceptance's first case: twenty rounds of two runners taking one
/// ticket at the same moment give one holder each, the other refused with
/// the holder named; the holder's delete frees the ticket for the next.
#[test]
fn twenty_rounds_of_two_runners_give_one_holder_each() {
    let bench = Bench::new();
    let (ada, bob) = (bench.runner("ada", "ada"), bench.runner("bob", "bob"));
    let one = ticket("OWL-1");
    for round in 0..20 {
        let start = Barrier::new(2);
        let (a, b) = std::thread::scope(|scope| {
            let (start, one) = (&start, &one);
            let (first, second) = (&ada, &bob);
            let a = scope.spawn(move || {
                start.wait();
                first.take(one, None)
            });
            let b = scope.spawn(move || {
                start.wait();
                second.take(one, None)
            });
            (a.join().unwrap(), b.join().unwrap())
        });
        let (winner, loser) = match (&a, &b) {
            (Ok(Took::Taken { .. }), Err(refused)) => ((&ada, &a), (&bob, refused)),
            (Err(refused), Ok(Took::Taken { .. })) => ((&bob, &b), (&ada, refused)),
            other => panic!("round {round}: {other:?}"),
        };
        let NotTaken::Claimed(Some(holder)) = loser.1 else {
            panic!("round {round}: {:?}", loser.1);
        };
        assert_eq!(holder.runner, winner.0.id.as_str(), "round {round}");
        assert_eq!(holder.operator, winner.0.operator, "round {round}");
        assert!(claims::read_record(&loser.0.dirs, &one).is_none());
        assert_eq!(claims::delete(winner.0.at(), &one), Ok(Deleted::Deleted));
        assert_eq!(bench.remote_ref(&claim_ref(&one)), None, "round {round}");
        assert!(winner.1.is_ok());
    }
}

/// Take, held set, renewal, another's refused take and delete move no ref
/// of either dedicated checkout but the remote-tracking ones; the claim on
/// the remote is a commit without a parent holding `claim.json` alone.
#[test]
fn no_local_ref_moves_for_a_claim() {
    let bench = Bench::new();
    let (ada, bob) = (bench.runner("ada", "ada"), bench.runner("bob", "bob"));
    let one = ticket("OWL-1");
    let (ada_refs, bob_refs) = (ada.local_refs(), bob.local_refs());

    assert!(matches!(ada.take(&one, None), Ok(Took::Taken { .. })));
    let commit = bench.remote_ref(&claim_ref(&one)).unwrap();
    let parents = bench
        .env
        .run(
            &bench.remote.bare,
            &["rev-list", "--parents", "-n", "1", &commit],
        )
        .unwrap();
    assert_eq!(String::from_utf8(parents).unwrap().trim(), commit);
    let tree = bench
        .env
        .run(&bench.remote.bare, &["ls-tree", "--name-only", &commit])
        .unwrap();
    assert_eq!(String::from_utf8(tree).unwrap().trim(), "claim.json");
    assert_eq!(ada.take(&one, None), Ok(Took::Own));

    let held = ClaimHeld {
        files: vec!["src/lib.rs".to_owned()],
        ..ClaimHeld::default()
    };
    assert_eq!(
        claims::update_held(ada.at(), &one, held.clone(), Timestamp::now()),
        Ok(true)
    );
    assert_eq!(bench.remote_claim(&one).held, held);
    assert_eq!(
        claims::update_held(ada.at(), &one, held, Timestamp::now()),
        Ok(false)
    );
    let renewal = claims::renew(ada.at(), &|_| ClaimHeld::default(), Timestamp::now());
    assert_eq!(renewal.renewed, std::slice::from_ref(&one), "{renewal:?}");
    assert!(bench.remote_claim(&one).held.is_empty());

    assert!(matches!(
        bob.take(&one, None),
        Err(NotTaken::Claimed(Some(_)))
    ));
    assert_eq!(claims::delete(ada.at(), &one), Ok(Deleted::Deleted));
    assert_eq!(ada.local_refs(), ada_refs);
    assert_eq!(bob.local_refs(), bob_refs);
}

/// One atomic renewal of three claims, one of them deleted and taken by
/// another runner meanwhile: that one is lost, holder named, and the two
/// others renewed; the next renewal renews the two alone.
#[test]
fn a_renewal_tells_the_lost_claim_from_the_others() {
    let bench = Bench::new();
    let (ada, bob) = (bench.runner("ada", "ada"), bench.runner("bob", "bob"));
    let tickets = [ticket("OWL-1"), ticket("OWL-2"), ticket("OWL-3")];
    for one in &tickets {
        assert!(matches!(ada.take(one, None), Ok(Took::Taken { .. })));
    }
    bench.release(&tickets[1]);
    assert!(matches!(
        bob.take(&tickets[1], None),
        Ok(Took::Taken { .. })
    ));

    let renewal = claims::renew(ada.at(), &|_| ClaimHeld::default(), Timestamp::now());
    assert_eq!(
        renewal.renewed,
        [tickets[0].clone(), tickets[2].clone()],
        "{renewal:?}"
    );
    let [(lost, Some(holder))] = renewal.lost.as_slice() else {
        panic!("{renewal:?}");
    };
    assert_eq!(*lost, tickets[1]);
    assert_eq!(holder.runner, bob.id.as_str());
    assert!(renewal.settled(), "{renewal:?}");
    assert!(claims::read_record(&ada.dirs, &tickets[1]).is_none());
    assert_eq!(bench.remote_claim(&tickets[1]).runner, bob.id.as_str());

    let again = claims::renew(ada.at(), &|_| ClaimHeld::default(), Timestamp::now());
    assert_eq!(again.renewed.len(), 2, "{again:?}");
    assert!(again.lost.is_empty(), "{again:?}");
}

/// A data directory whose runner identifier was made again keeps records
/// of claims the old identity took: none is the new runner's. Its take
/// finds the claim lost, naming the old runner, its renewal finds the
/// other lost, and no held set is pushed under the old identity.
#[test]
fn a_record_of_another_runner_identity_is_not_this_runners() {
    let bench = Bench::new();
    let ada = bench.runner("ada", "ada");
    let (one, two, three) = (ticket("OWL-1"), ticket("OWL-2"), ticket("OWL-3"));
    for each in [&one, &two, &three] {
        assert!(matches!(ada.take(each, None), Ok(Took::Taken { .. })));
    }
    let old = ada.id.clone();
    fs::remove_file(ada.dirs.data_dir().join(claims::RUNNER_FILE)).unwrap();
    let new = claims::runner_id(ada.dirs.data_dir()).unwrap();
    assert_ne!(new, old);

    let taken = claims::take(
        ada.at(),
        &one,
        &new,
        "ada",
        ClaimHeld::default(),
        Timestamp::now(),
        None,
    );
    let Err(NotTaken::Lost(Some(holder))) = taken else {
        panic!("{taken:?}");
    };
    assert_eq!(holder.runner, old.as_str());
    assert!(claims::read_record(&ada.dirs, &one).is_none());

    let held = ClaimHeld {
        files: vec!["src/lib.rs".to_owned()],
        ..ClaimHeld::default()
    };
    assert!(claims::update_held(ada.at(), &three, held, Timestamp::now()).is_err());
    assert!(bench.remote_claim(&three).held.is_empty());

    let renewal = claims::renew(ada.at(), &|_| ClaimHeld::default(), Timestamp::now());
    assert!(renewal.renewed.is_empty(), "{renewal:?}");
    let lost: Vec<&TicketId> = renewal.lost.iter().map(|(ticket, _)| ticket).collect();
    assert_eq!(lost, [&two, &three], "{renewal:?}");
    assert_eq!(bench.remote_claim(&two).runner, old.as_str());
}

/// What a take reads back after a crash or another machine's gesture: a
/// claim gone is lost; a claim of this runner's its record missed is
/// adopted; a take never confirmed whose claim is absent is taken afresh; a
/// delete of a claim already gone removes the record; and the ticket's
/// branch, on the remote though this runner never pushed it, refuses a
/// take.
#[test]
fn a_take_reads_back_what_a_crash_or_another_machine_left() {
    let bench = Bench::new();
    let ada = bench.runner("ada", "ada");
    let one = ticket("OWL-1");

    // Gone: lost, the record removed.
    assert!(matches!(ada.take(&one, None), Ok(Took::Taken { .. })));
    bench.release(&one);
    assert_eq!(ada.take(&one, None), Err(NotTaken::Lost(None)));
    assert!(claims::read_record(&ada.dirs, &one).is_none());

    // This runner's, at a commit its record does not name: adopted.
    assert!(matches!(ada.take(&one, None), Ok(Took::Taken { .. })));
    let mut stale = claims::read_record(&ada.dirs, &one).unwrap();
    let pushed = stale.commit.clone().unwrap();
    stale.commit = Some("0123456789abcdef0123456789abcdef01234567".to_owned());
    fs::write(claims::record_file(&ada.dirs, &one), stale.render()).unwrap();
    assert_eq!(ada.take(&one, None), Ok(Took::Adopted { commit: pushed }));

    // A take never confirmed, its claim absent: taken afresh.
    bench.release(&one);
    let mut pending = claims::read_record(&ada.dirs, &one).unwrap();
    pending.next = pending.commit.take();
    fs::write(claims::record_file(&ada.dirs, &one), pending.render()).unwrap();
    assert!(matches!(ada.take(&one, None), Ok(Took::Taken { .. })));

    // A delete of a claim already gone: no loss, the record removed.
    bench.release(&one);
    assert_eq!(claims::delete(ada.at(), &one), Ok(Deleted::NotOurs));
    assert!(claims::read_record(&ada.dirs, &one).is_none());

    // The ticket's branch on the remote, never pushed by this runner.
    let head = bench.remote_ref("HEAD").unwrap();
    bench
        .env
        .run(
            &bench.remote.bare,
            &["update-ref", "refs/heads/owlshift/owl-1", &head],
        )
        .unwrap();
    assert_eq!(
        ada.take(&one, Some("owlshift/owl-1")),
        Err(NotTaken::Branch {
            branch: "owlshift/owl-1".to_owned()
        })
    );
    assert_eq!(bench.remote_ref(&claim_ref(&one)), None);
}

/// OWL-642: a break's repair claim, as two runners push it. A take creates
/// it only while it is absent; a replacement and a naming land only on the
/// commit read; a give-back deletes it, leased on the take; a claim of a
/// later format reads as one to upgrade for. No local ref moves.
#[test]
fn a_repair_claim_is_created_once_and_moved_only_from_the_commit_read() {
    let bench = Bench::new();
    let ann = bench.runner("ann", "ann");
    let bob = bench.runner("bob", "bob");
    const KEY: &str = "setup-0123456789ab";
    let before = ann.local_refs();
    let filing = |runner: &Runner| RepairClaim {
        format: Format,
        runner: runner.id.as_str().to_owned(),
        operator: runner.operator.to_owned(),
        claimed_at: Timestamp::now(),
        ticket: None,
        repairs: 1,
    };
    assert_eq!(repair::read(ann.at(), KEY), Ok(None));
    let Ok(Pushed::Landed(taken)) = repair::push_claim(ann.at(), KEY, &filing(&ann), None) else {
        panic!("the first take lands");
    };
    assert_eq!(
        repair::push_claim(bob.at(), KEY, &filing(&bob), None),
        Ok(Pushed::Refused),
        "a second take is refused"
    );
    let read = repair::read(bob.at(), KEY).unwrap().unwrap();
    assert_eq!(read.commit, taken.as_str());
    assert_eq!(read.claim.unwrap().runner, ann.id.as_str());

    // Named, leased on the take; a replacement from the take is then stale.
    let named = RepairClaim {
        ticket: Some(ticket("OWL-9")),
        ..filing(&ann)
    };
    let Ok(Pushed::Landed(naming)) =
        repair::push_claim(ann.at(), KEY, &named, Some(taken.as_str()))
    else {
        panic!("the naming lands");
    };
    assert_eq!(
        repair::push_claim(bob.at(), KEY, &filing(&bob), Some(taken.as_str())),
        Ok(Pushed::Refused)
    );
    let read = repair::read(bob.at(), KEY).unwrap().unwrap();
    assert_eq!(read.claim.unwrap().ticket, Some(ticket("OWL-9")));

    // Replaced from the commit read, then given back as it was.
    let Ok(Pushed::Landed(replaced)) =
        repair::push_claim(bob.at(), KEY, &filing(&bob), Some(naming.as_str()))
    else {
        panic!("the replacement lands");
    };
    assert_eq!(
        repair::give_back(bob.at(), KEY, &replaced, Some(naming.as_str())),
        Ok(())
    );
    assert_eq!(
        bench.remote_ref(&repair_claim_ref(KEY)).as_deref(),
        Some(naming.as_str())
    );
    assert!(repair::give_back(ann.at(), KEY, &taken, None).is_err());
    assert_eq!(repair::give_back(ann.at(), KEY, &naming, None), Ok(()));
    assert_eq!(bench.remote_ref(&repair_claim_ref(KEY)), None);

    // Of a later format: read, and said to need an upgrade.
    let newer = filing(&ann)
        .render()
        .replacen("\"format\": 1", "\"format\": 2", 1);
    assert!(newer.contains("\"format\": 2"), "{newer}");
    let dir = bench.root.path().join("newer");
    fs::create_dir(&dir).unwrap();
    fs::write(dir.join("repair.json"), newer).unwrap();
    for args in [
        &["init", "--quiet"][..],
        &["add", "repair.json"],
        &["commit", "--quiet", "-m", "A newer claim"],
    ] {
        bench.env.run(&dir, args).unwrap();
    }
    let target = format!("HEAD:{}", repair_claim_ref(KEY));
    bench
        .env
        .run(
            &dir,
            &[
                "push",
                "--quiet",
                &bench.remote.bare.to_string_lossy(),
                &target,
            ],
        )
        .unwrap();
    let read = repair::read(ann.at(), KEY).unwrap().unwrap();
    assert!(read.claim.is_err_and(|unread| unread.newer));
    assert_eq!(ann.local_refs(), before, "no local ref moved");
}
