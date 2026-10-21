use std::process::Command;

use owlshift_adapters::forge::Repo;

use super::*;
use crate::ticket_ref::Unguarded;

/// A project's folder with a dedicated checkout of one commit, and git run
/// hermetically there.
struct Bench {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    dirs: ProjectDirs,
    git: Git,
}

impl Bench {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        // Resolved on Unix, where the temporary folder can be a link; as it
        // is on Windows, where a resolved path takes a prefix git refuses.
        let root = if cfg!(unix) {
            fs::canonicalize(tmp.path()).unwrap()
        } else {
            tmp.path().to_owned()
        };
        fs::write(
            root.join("gitconfig"),
            "[user]\n\tname = Bench\n\temail = bench@example.invalid\n\
             [init]\n\tdefaultBranch = main\n",
        )
        .unwrap();
        let dirs = ProjectDirs::new(&root.join("data"), &Repo::parse("acme/app").unwrap());
        let config = root.join("gitconfig");
        let git = Git::with_setup("git", move |command| {
            command
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", &config);
        });
        let bench = Self {
            _tmp: tmp,
            root,
            dirs,
            git,
        };
        fs::create_dir_all(bench.dirs.checkout()).unwrap();
        bench.run(&["init", "--quiet"]);
        bench.run(&["commit", "--quiet", "--allow-empty", "-m", "one"]);
        bench
    }

    fn run(&self, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(args)
            .current_dir(self.dirs.checkout())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", self.root.join("gitconfig"))
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }

    /// A new commit of the checkout, on top of `HEAD`.
    fn commit(&self, message: &str) -> String {
        let tree = self.run(&["rev-parse", "HEAD^{tree}"]);
        self.run(&["commit-tree", &tree, "-p", "HEAD", "-m", message])
    }

    fn run_file(&self, ticket: &TicketId) -> TicketRun {
        read_run(&self.dirs, ticket).unwrap().expect("a run file")
    }

    fn keep(&self, run: &TicketRun) {
        let mut run = run.clone();
        run.generation -= 1;
        save(&self.dirs, &mut run).unwrap();
    }

    fn open(&self, ticket: &TicketId) -> Result<TicketRun, RunFault> {
        let lock = self.dirs.ticket_lock(ticket).unwrap().unwrap();
        open(
            &self.git,
            &self.dirs,
            &self.dirs.checkout(),
            &lock,
            ticket,
            None,
        )
    }
}

fn ticket(id: &str) -> TicketId {
    TicketId::new(id).unwrap()
}

fn record() -> TicketRecord {
    ticket_ref::blank(None)
}

/// A write of the ticket's ref through its run file records the target
/// before and the result after; one cut off between the two is finished by
/// the next holder of the lock whichever value the ref shows, a deletion of
/// `forget` included, and a ref at neither value is a breach (OWL-400,
/// acceptance: "a write cut between ref and run file is finished").
#[test]
fn a_cut_write_is_finished_whichever_value_the_ref_shows() {
    let bench = Bench::new();
    let owl = ticket("OWL-1");
    let checkout = bench.dirs.checkout();
    let first = bench.open(&owl).unwrap();
    assert_eq!((first.generation, first.ticket_ref.as_deref()), (1, None));

    let lock = bench.dirs.ticket_lock(&owl).unwrap().unwrap();
    let written = write_ref(
        &bench.git,
        &bench.dirs,
        &checkout,
        &lock,
        &owl,
        &record(),
        ProposalsFile::Keep,
        None,
    )
    .unwrap();
    let run = bench.run_file(&owl);
    assert_eq!(run.ticket_ref.as_deref(), Some(written.as_str()));
    assert!(run.writing.is_empty());
    assert_eq!(run.generation, 3, "raised before and after the move");
    drop(lock);

    let name = ticket_ref(&owl);
    let next = bench.commit("next");
    let cut = |at: &str| {
        let mut run = bench.run_file(&owl);
        run.writing.ticket_ref = Some(RunTarget {
            to: Some(next.clone()),
        });
        bench.keep(&run);
        bench.run(&["update-ref", &name, at]);
    };
    // Cut before the move: the ref still at the recorded value.
    cut(&written);
    let settled = bench.open(&owl).unwrap();
    assert_eq!(settled.ticket_ref.as_deref(), Some(written.as_str()));
    assert!(settled.writing.is_empty());
    // Cut after the move: the ref at the target.
    cut(&next);
    let settled = bench.open(&owl).unwrap();
    assert_eq!(settled.ticket_ref.as_deref(), Some(next.as_str()));
    assert!(settled.writing.is_empty());

    // A deletion of `forget`'s cut after it, and before it.
    let mut run = bench.run_file(&owl);
    run.writing.ticket_ref = Some(RunTarget { to: None });
    bench.keep(&run);
    bench.run(&["update-ref", "-d", &name]);
    assert_eq!(bench.open(&owl).unwrap().ticket_ref, None);
    bench.run(&["update-ref", &name, &next]);
    let mut run = bench.run_file(&owl);
    run.ticket_ref = Some(next.clone());
    run.writing.ticket_ref = Some(RunTarget { to: None });
    bench.keep(&run);
    assert_eq!(
        bench.open(&owl).unwrap().ticket_ref.as_deref(),
        Some(next.as_str())
    );

    // At neither value: a breach, also for a write, which moves nothing.
    let other = bench.commit("other");
    bench.run(&["update-ref", &name, &other]);
    let Err(RunFault::Breach(why)) = bench.open(&owl) else {
        panic!("a moved ref is a breach");
    };
    assert!(why.contains("the ticket ref of OWL-1"), "{why}");
    let lock = bench.dirs.ticket_lock(&owl).unwrap().unwrap();
    let refused = write_ref(
        &bench.git,
        &bench.dirs,
        &checkout,
        &lock,
        &owl,
        &record(),
        ProposalsFile::Keep,
        Some(&other),
    );
    assert!(matches!(refused, Err(RunFault::Breach(_))), "{refused:?}");
    assert_eq!(bench.run(&["rev-parse", &name]), other);
    let forgot = forget_ref(&bench.git, &bench.dirs, &checkout, &lock, &owl);
    assert!(matches!(forgot, Err(RunFault::Breach(_))), "{forgot:?}");
}

/// A move of the branch the runner makes outside a run is recorded with
/// its target, a cut one finished; a move by anyone else is a breach at the
/// next start, and so is a kept worktree on another entry than the one
/// recorded.
#[test]
fn a_branch_moved_outside_the_runner_is_found_at_the_next_start() {
    let bench = Bench::new();
    let owl = ticket("OWL-2");
    let checkout = bench.dirs.checkout();
    bench.open(&owl).unwrap();
    let branch = format!("refs/heads/{}", branch_for(&owl));
    let start = bench.run(&["rev-parse", "HEAD"]);
    let lock = bench.dirs.ticket_lock(&owl).unwrap().unwrap();
    let moved = move_branch(
        &bench.git,
        &bench.dirs,
        &checkout,
        &lock,
        &owl,
        Some(&start),
        || bench.run(&["update-ref", &branch, &start]),
    );
    assert!(moved.is_ok(), "{moved:?}");
    assert_eq!(bench.run_file(&owl).tip.as_deref(), Some(start.as_str()));
    // A move that stopped short leaves the branch where it was: kept.
    let next = bench.commit("next");
    move_branch(
        &bench.git,
        &bench.dirs,
        &checkout,
        &lock,
        &owl,
        Some(&next),
        || (),
    )
    .unwrap();
    assert_eq!(bench.run_file(&owl).tip.as_deref(), Some(start.as_str()));
    drop(lock);

    bench.run(&["update-ref", &branch, &next]);
    let Err(RunFault::Breach(why)) = bench.open(&owl) else {
        panic!("a branch moved by someone else is a breach");
    };
    assert!(why.contains("owlshift/owl-2"), "{why}");

    bench.run(&["update-ref", &branch, &start]);
    let mut run = bench.run_file(&owl);
    run.entry = Some("owl-2".to_owned());
    bench.keep(&run);
    let lock = bench.dirs.ticket_lock(&owl).unwrap().unwrap();
    let opened = open(
        &bench.git,
        &bench.dirs,
        &checkout,
        &lock,
        &owl,
        Some("owl-21"),
    );
    assert!(matches!(opened, Err(RunFault::Breach(_))), "{opened:?}");
}

/// A ticket from before P6 gets its run file from its refs as they are,
/// once no obligation stands; while one does, it waits.
#[test]
fn a_ticket_from_before_gets_its_baseline_once_no_run_is_in_flight() {
    let bench = Bench::new();
    let owl = ticket("OWL-3");
    let checkout = bench.dirs.checkout();
    let written = ticket_ref::write_proposals_guarded(
        &bench.git,
        &checkout,
        &owl,
        &record(),
        ProposalsFile::Keep,
        None,
        &mut Unguarded,
    )
    .unwrap();
    let name =
        write_obligation(&bench.dirs, Some(&ticket("OWL-9")), "r1", "run r1 of OWL-9").unwrap();
    assert_eq!(name, "owl-9-r1");
    assert_eq!(bench.open(&owl), Err(RunFault::Busy));
    assert!(read_run(&bench.dirs, &owl).unwrap().is_none());
    remove_obligation(&bench.dirs, &name).unwrap();
    let run = bench.open(&owl).unwrap();
    assert_eq!(run.ticket_ref.as_deref(), Some(written.as_str()));
    assert_eq!(run.tip, None, "no branch yet");
}

/// Obligations left behind become the quarantine, naming each; one whose
/// ticket's lock is held is a run in flight and left alone; removing one
/// leaves the others. Run alone: a lock is probed free.
#[test]
fn obligations_left_behind_become_the_quarantine() {
    crate::alone::run_alone(
        "ticket_run::tests::helper_obligations_left_behind_become_the_quarantine",
    );
}

#[test]
#[ignore = "helper, run by the test above"]
fn helper_obligations_left_behind_become_the_quarantine() {
    if !crate::alone::running_alone() {
        return;
    }
    let bench = Bench::new();
    let dirs = &bench.dirs;
    let (own, running, ended) = (ticket("OWL-1"), ticket("OWL-2"), ticket("OWL-3"));
    let _held = dirs.ticket_lock(&running).unwrap().unwrap();
    let in_flight = write_obligation(dirs, Some(&running), "r2", "Run r2 of OWL-2").unwrap();
    assert_eq!(
        look(dirs, Some(&own)).unwrap(),
        Look::Clear { running: true }
    );

    let gone = write_obligation(dirs, Some(&ended), "r3", "Run r3 of OWL-3").unwrap();
    remove_obligation(dirs, &gone).unwrap();
    assert!(dirs.obligations_dir().join(&in_flight).exists());
    assert_eq!(
        look(dirs, Some(&own)).unwrap(),
        Look::Clear { running: true }
    );

    write_obligation(dirs, Some(&own), "r1", "Run r1 of OWL-1").unwrap();
    write_obligation(dirs, Some(&ended), "r4", "Run r4 of OWL-3").unwrap();
    write_obligation(dirs, None, "20261021T000000Z", "`owlshift gate` on abc").unwrap();
    fs::write(dirs.obligations_dir().join("owl-5-r5"), "not json").unwrap();
    let Look::Quarantined { path, text } = look(dirs, Some(&own)).unwrap() else {
        panic!("obligations left behind quarantine the project");
    };
    assert_eq!(path, dirs.quarantine_file());
    for named in [
        "Run r1 of OWL-1",
        "Run r4 of OWL-3",
        "`owlshift gate` on abc",
        "owl-5-r5",
    ] {
        assert!(text.contains(named), "{named}: {text}");
    }
    assert!(!text.contains("Run r2 of OWL-2"), "{text}");
    assert_eq!(
        look(dirs, Some(&own)).unwrap(),
        Look::Quarantined {
            path: dirs.quarantine_file(),
            text: text.clone()
        },
        "the quarantine stands, unchanged"
    );
    let shown = standing(dirs).unwrap();
    let running: Vec<&str> = shown
        .iter()
        .filter(|s| s.running)
        .map(|s| s.found.name.as_str())
        .collect();
    assert_eq!(running, [in_flight.as_str()]);
    assert_eq!(shown.len(), 5);
}

/// A run's check is told the other tickets from their run files and locks:
/// one whose lock is held is in flight, one whose generation moved since the
/// marks ended, one unchanged nothing; the run's own is left out, and a run
/// file that cannot be read is an error. Run alone: locks are probed free.
#[test]
fn siblings_are_read_from_their_run_files_and_locks() {
    crate::alone::run_alone(
        "ticket_run::tests::helper_siblings_are_read_from_their_run_files_and_locks",
    );
}

#[test]
#[ignore = "helper, run by the test above"]
fn helper_siblings_are_read_from_their_run_files_and_locks() {
    if !crate::alone::running_alone() {
        return;
    }
    let bench = Bench::new();
    let dirs = &bench.dirs;
    let at = |id: &str, tip: Option<&str>, entry: &str| {
        let mut run = TicketRun::new(ticket(id));
        run.tip = tip.map(str::to_owned);
        run.ticket_ref = Some("b".repeat(40));
        run.entry = Some(entry.to_owned());
        save(dirs, &mut run).unwrap();
        run
    };
    let own = ticket("OWL-1");
    at("OWL-1", None, "owl-1");
    at("OWL-2", None, "owl-2");
    let mut ended = at("OWL-3", Some(&"a".repeat(40)), "owl-3");
    at("OWL-4", None, "owl-4");
    let siblings = Siblings::new(dirs, &own);
    let marks = siblings.marks().unwrap();
    assert_eq!(marks.0.len(), 3, "the run's own left out");

    let _held = dirs.ticket_lock(&ticket("OWL-2")).unwrap().unwrap();
    ended.writing.tip = Some(RunTarget {
        to: Some("c".repeat(40)),
    });
    save(dirs, &mut ended).unwrap();
    let known = siblings.concurrent(&marks).unwrap();
    assert_eq!(
        known.running,
        BTreeSet::from([
            "refs/heads/owlshift/owl-2".to_owned(),
            "refs/owlshift/tickets/OWL-2".to_owned()
        ])
    );
    assert_eq!(
        known.ended,
        BTreeMap::from([
            (
                "refs/heads/owlshift/owl-3".to_owned(),
                BTreeSet::from([Some("a".repeat(40)), Some("c".repeat(40))])
            ),
            (
                "refs/owlshift/tickets/OWL-3".to_owned(),
                BTreeSet::from([Some("b".repeat(40))])
            ),
        ])
    );
    assert_eq!(
        known.entries,
        BTreeSet::from(["owl-2".to_owned(), "owl-3".to_owned()])
    );

    fs::write(dirs.run_file(&ticket("OWL-4")), "{").unwrap();
    assert!(siblings.concurrent(&marks).is_err());
    assert!(siblings.marks().is_err());
}

/// `owlshift unquarantine`'s fresh start of the run files: each from its
/// refs as they are, an unreadable one replaced, one whose lock is held left
/// as it is; with no checkout, they go. Run alone: locks are taken again.
#[test]
fn run_files_are_taken_afresh_from_the_refs_as_they_are() {
    crate::alone::run_alone(
        "ticket_run::tests::helper_run_files_are_taken_afresh_from_the_refs_as_they_are",
    );
}

#[test]
#[ignore = "helper, run by the test above"]
fn helper_run_files_are_taken_afresh_from_the_refs_as_they_are() {
    if !crate::alone::running_alone() {
        return;
    }
    let bench = Bench::new();
    let dirs = &bench.dirs;
    let (moved, broken) = (ticket("OWL-6"), ticket("OWL-7"));
    bench.open(&moved).unwrap();
    bench.open(&broken).unwrap();
    let branch = format!("refs/heads/{}", branch_for(&moved));
    let head = bench.run(&["rev-parse", "HEAD"]);
    bench.run(&["update-ref", &branch, &head]);
    fs::write(dirs.run_file(&broken), "{").unwrap();
    let held = ticket("OWL-8");
    bench.open(&held).unwrap();
    let _lock = dirs.ticket_lock(&held).unwrap().unwrap();

    assert_eq!(rebaseline(&bench.git, dirs).unwrap(), ["OWL-8"]);
    assert_eq!(bench.run_file(&moved).tip.as_deref(), Some(head.as_str()));
    assert!(bench.open(&moved).is_ok());
    // Its folder named it: no ref says how its id is written, and its next
    // start takes the id as the command names it.
    assert_eq!(bench.run_file(&broken).ticket.as_str(), "owl-7");
    assert_eq!(bench.open(&broken).unwrap().ticket, broken);

    fs::remove_dir_all(dirs.checkout()).unwrap();
    assert_eq!(rebaseline(&bench.git, dirs).unwrap(), ["OWL-8"]);
    assert!(!dirs.run_file(&moved).exists());
    assert!(!dirs.run_file(&broken).exists());
    assert!(dirs.run_file(&held).exists());
}

/// Production code writes a ticket ref only through this module (OWL-400):
/// a call of `ticket_ref`'s writes anywhere else in the runner's sources is
/// a write that skips the run file.
#[test]
fn only_this_module_writes_a_ticket_ref() {
    fn sources(dir: &Path, found: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                sources(&path, found);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                found.push(path);
            }
        }
    }
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    sources(&src, &mut files);
    let allowed = [
        src.join("ticket_ref.rs"),
        src.join("ticket_run.rs"),
        src.join("ticket_run").join("tests.rs"),
    ];
    let mut offending = Vec::new();
    for file in files.iter().filter(|file| !allowed.contains(file)) {
        let text = fs::read_to_string(file).unwrap();
        for (n, line) in text.lines().enumerate() {
            let code = line.trim_start();
            if code.starts_with("//") {
                continue;
            }
            if ["ticket_ref::write", "ticket_ref::forget"]
                .iter()
                .any(|call| code.contains(call))
            {
                offending.push(format!("{}:{}", file.display(), n + 1));
            }
        }
    }
    assert!(offending.is_empty(), "{offending:?}");
    assert!(files.len() > 20, "the sources were not found");
}
