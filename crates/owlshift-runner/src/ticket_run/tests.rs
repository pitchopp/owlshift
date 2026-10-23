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

/// A ticket's branch made a symbolic ref to another branch at the same
/// commit, through which the ticket's worktree would commit into that one,
/// is a breach at the next start (Codex P1 on #377); taking the run files
/// afresh does not accept it, and a ticket from before P6 gets no baseline
/// from it.
#[test]
fn a_branch_made_an_alias_at_the_same_commit_is_caught_at_the_next_start() {
    let bench = Bench::new();
    let owl = ticket("OWL-4");
    let branch = format!("refs/heads/{}", branch_for(&owl));
    bench.run(&["branch", &branch_for(&owl), "HEAD"]);
    let main = bench.run(&["symbolic-ref", "HEAD"]);
    let run = bench.open(&owl).unwrap();
    assert_eq!(run.tip, Some(bench.run(&["rev-parse", "HEAD"])));

    bench.run(&["symbolic-ref", &branch, &main]);
    let caught = |opened: Result<TicketRun, RunFault>| match opened {
        Err(RunFault::Breach(why)) => {
            assert!(
                why.contains(&format!("is a symbolic ref to {main}")),
                "{why}"
            );
        }
        other => panic!("{other:?}"),
    };
    caught(bench.open(&owl));
    rebaseline(&bench.git, &bench.dirs).unwrap();
    assert_eq!(bench.run_file(&owl).tip, None);
    caught(bench.open(&owl));
    fs::remove_file(bench.dirs.run_file(&owl)).unwrap();
    caught(bench.open(&owl));
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
    // A ticket of which nothing exists yet gets its empty baseline at once
    // (OWL-401): whatever a run in flight makes of it later is found at its
    // next start.
    let fresh = ticket("OWL-4");
    let run = bench.open(&fresh).unwrap();
    assert_eq!((run.ticket_ref, run.tip, run.entry), (None, None, None));
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

/// An obligation is never seen partly written (OWL-425): a step's child
/// writes its own without the project's lock, while a watch pass that holds
/// it lists the folder, and one read empty there quarantined the project.
/// The writer writes and removes one obligation after another; the reader
/// lists the folder meanwhile, and every record it finds reads. On Windows
/// it also meets obligations refused while they are deleted (OWL-426).
#[test]
fn an_obligation_is_never_seen_partly_written() {
    use std::sync::atomic::{AtomicBool, Ordering};

    const WRITES: usize = 300;
    let tmp = tempfile::tempdir().unwrap();
    let dirs = ProjectDirs::new(tmp.path(), &Repo::parse("acme/app").unwrap());
    let owl = ticket("OWL-1");
    let done = AtomicBool::new(false);
    let (mut seen, mut unread) = (0, Vec::new());
    std::thread::scope(|scope| {
        scope.spawn(|| {
            for n in 0..WRITES {
                let name = write_obligation(&dirs, Some(&owl), &format!("r{n}"), "a run").unwrap();
                remove_obligation(&dirs, &name).unwrap();
            }
            done.store(true, Ordering::Release);
        });
        while !done.load(Ordering::Acquire) {
            for found in obligations(&dirs).unwrap() {
                match found.record {
                    Ok(_) => seen += 1,
                    // What the file system says of it just after, so a
                    // failure on a system the bench cannot run tells why.
                    Err(why) => unread.push(format!(
                        "{}: {why}; gone now: {}",
                        found.name,
                        gone(&dirs.obligations_dir().join(&found.name)),
                    )),
                }
            }
        }
    });
    assert!(
        unread.is_empty(),
        "{} of {} read: {unread:?}",
        seen,
        seen + unread.len()
    );
    assert!(seen > 0, "the reader saw no obligation");
}

// ---------------------------------------------------------------------------
// OWL-426 probe, temporary: run on windows-latest by
// .github/workflows/owl-426-probe.yml, deleted before the pull request opens.
// Why does Windows refuse some reads of `unverified/` while the writer works?
// Four writers (publish by hard link, fb88d18's form, or by the no-replace
// move of `files::publish`; retire by deleting in the folder or by renaming
// out of it) against a reader that skips nothing, each refused read reported
// with the NTSTATUS of the refused call and what the file system says next.
// ---------------------------------------------------------------------------

#[cfg(windows)]
#[link(name = "ntdll", kind = "raw-dylib")]
unsafe extern "system" {
    safe fn RtlGetLastNtStatus() -> i32;
}

#[cfg(windows)]
#[link(name = "kernel32", kind = "raw-dylib")]
unsafe extern "system" {
    fn SetFileInformationByHandle(
        file: *mut std::ffi::c_void,
        class: i32,
        info: *const std::ffi::c_void,
        size: u32,
    ) -> i32;
}

const OWL426_DELETE_PENDING: u32 = 0xC000_0056;

/// The NTSTATUS the calling thread's last failed system call left; 0 off
/// Windows.
fn owl426_status() -> u32 {
    #[cfg(windows)]
    {
        RtlGetLastNtStatus().cast_unsigned()
    }
    #[cfg(not(windows))]
    {
        0
    }
}

/// The status read names the call that failed: a missing file, a folder
/// opened as a file, and a file held under a delete disposition each give
/// their own, and `files::delete_pending` sees the last one.
#[cfg(windows)]
fn owl426_calibrate(dir: &Path) {
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;

    let error = File::open(dir.join("missing")).unwrap_err();
    let status = owl426_status();
    println!("OWL426 calibration missing: {error}; status 0x{status:08X}");
    assert_eq!(
        (error.kind(), status),
        (io::ErrorKind::NotFound, 0xC000_0034)
    );

    let error = File::open(dir).unwrap_err();
    let status = owl426_status();
    println!("OWL426 calibration folder: {error}; status 0x{status:08X}");
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    assert!(![0, 0xC000_0034, OWL426_DELETE_PENDING].contains(&status));

    let path = dir.join("calibration");
    fs::write(&path, "x").unwrap();
    let held = OpenOptions::new()
        .access_mode(0x0001_0000) // DELETE
        .share_mode(7)
        .open(&path)
        .unwrap();
    let delete: u8 = 1;
    // SAFETY: `held` is open with DELETE access for the whole call, and the
    // buffer is a live BOOLEAN of the size passed (FileDispositionInfo = 4).
    let set = unsafe {
        SetFileInformationByHandle(held.as_raw_handle(), 4, (&raw const delete).cast(), 1)
    };
    assert_ne!(set, 0, "{}", io::Error::last_os_error());
    let error = File::open(&path).unwrap_err();
    let status = owl426_status();
    let listed = fs::read_dir(dir)
        .unwrap()
        .any(|entry| entry.unwrap().file_name() == "calibration");
    println!("OWL426 calibration delete pending: {error}; status 0x{status:08X}; listed: {listed}");
    assert_eq!(
        (error.kind(), status, listed),
        (io::ErrorKind::PermissionDenied, OWL426_DELETE_PENDING, true)
    );
    drop(held);
    assert!(gone(&path));
}

/// Sets `done` however the writer ends, so the reader never waits on a
/// writer that panicked.
struct Owl426Done<'a>(&'a std::sync::atomic::AtomicBool);

impl Drop for Owl426Done<'_> {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::Release);
    }
}

#[derive(Default)]
struct Owl426Writer {
    link_retries: usize,
    partial_remove_failures: usize,
    publish_retries: usize,
    retire_retries: usize,
}

/// `op`, tried again up to 50 times 10 ms apart, each failure counted.
fn owl426_retry(count: &mut usize, mut op: impl FnMut() -> io::Result<()>) {
    for _ in 0..50 {
        if op().is_ok() {
            return;
        }
        *count += 1;
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    op().unwrap();
}

#[derive(Default)]
struct Owl426Reader {
    sightings: usize,
    vanished: usize,
    unparsed: usize,
    refused: usize,
    status_delete_pending: usize,
    status_other: usize,
    gone_first_ask: usize,
    gone_second_ask: usize,
    gone_later_ask: usize,
    never_gone: usize,
    max_asks: u32,
    max_us_to_gone: u128,
    us_le_25: usize,
    us_le_100: usize,
    us_le_1000: usize,
    us_gt_1000: usize,
    miss_listed: usize,
    miss_refused: usize,
    pending_at_miss: usize,
    over_cap: usize,
    during_partial_removal: usize,
    during_retire: usize,
    during_either: usize,
}

impl Owl426Reader {
    /// One listing of `folder`, each entry read with `read_capped`'s steps
    /// (ticket_run.rs), without `obligations()`'s skips.
    fn pass(&mut self, folder: &Path, phase: &std::sync::atomic::AtomicU64, variant: &str) {
        use std::sync::atomic::Ordering;
        for entry in fs::read_dir(folder).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let before = phase.load(Ordering::SeqCst);
            // The open `symlink_metadata` makes first on Windows (no access,
            // backup semantics, the link itself), whose status its fallback
            // to FindFirstFileExW would overwrite.
            #[cfg(windows)]
            let stat = {
                use std::os::windows::fs::OpenOptionsExt;
                OpenOptions::new()
                    .access_mode(0)
                    .custom_flags(0x0200_0000 | 0x0020_0000)
                    .open(&path)
                    .map(drop)
            };
            #[cfg(not(windows))]
            let stat = fs::symlink_metadata(&path).map(drop);
            let status = owl426_status();
            match stat {
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    self.vanished += 1;
                    continue;
                }
                Err(error) => {
                    self.refusal(
                        folder, &entry, "stat", &error, status, before, phase, variant,
                    );
                    continue;
                }
                Ok(_) => {}
            }
            let before = phase.load(Ordering::SeqCst);
            let opened = File::open(&path);
            let status = owl426_status();
            match opened {
                Ok(mut file) => {
                    let mut text = String::new();
                    if file.read_to_string(&mut text).is_ok() && RunObligation::parse(&text).is_ok()
                    {
                        self.sightings += 1;
                    } else {
                        self.unparsed += 1;
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => self.vanished += 1,
                Err(error) => {
                    self.refusal(
                        folder, &entry, "open", &error, status, before, phase, variant,
                    );
                }
            }
        }
    }

    /// A read refused: what the file system says right after, in the order
    /// a skip would ask it, and where the writer was.
    #[allow(clippy::too_many_arguments)]
    fn refusal(
        &mut self,
        _folder: &Path,
        entry: &fs::DirEntry,
        step: &str,
        error: &io::Error,
        status: u32,
        before: u64,
        phase: &std::sync::atomic::AtomicU64,
        variant: &str,
    ) {
        use std::sync::atomic::Ordering;
        use std::time::{Duration, Instant};
        let path = entry.path();
        // `gone` asked again and again from the refusal on: how many asks,
        // and how long, until the entry is gone; at the first ask that does
        // not find it gone, what the metadata said and whether the file
        // system called it being deleted.
        let started = Instant::now();
        let (mut asks, mut us_to_gone, mut first_miss) = (0_u32, None, None);
        while started.elapsed() < Duration::from_secs(1) {
            asks += 1;
            let meta = fs::symlink_metadata(&path);
            if meta
                .as_ref()
                .is_err_and(|error| error.kind() == io::ErrorKind::NotFound)
            {
                us_to_gone = Some(started.elapsed().as_micros());
                break;
            }
            if first_miss.is_none() {
                let said = match meta {
                    Ok(_) => "listed".to_owned(),
                    Err(error) => format!("error ({error})"),
                };
                // Round 3: `files::delete_pending` is removed (it never
                // answered yes in rounds 1 and 2).
                first_miss = Some((said, false));
            }
        }
        let after = phase.load(Ordering::SeqCst);
        let name = entry.file_name().to_string_lossy().into_owned();
        let n: u64 = name.rsplit_once("-r").unwrap().1.parse().unwrap();
        self.refused += 1;
        if status == OWL426_DELETE_PENDING {
            self.status_delete_pending += 1;
        } else {
            self.status_other += 1;
        }
        if us_to_gone.is_none() || asks as usize > GONE_ASKS {
            self.over_cap += 1;
        }
        match (asks, us_to_gone) {
            (_, None) => self.never_gone += 1,
            (1, Some(_)) => self.gone_first_ask += 1,
            (2, Some(_)) => self.gone_second_ask += 1,
            (_, Some(_)) => self.gone_later_ask += 1,
        }
        self.max_asks = self.max_asks.max(asks);
        if let Some(us) = us_to_gone {
            self.max_us_to_gone = self.max_us_to_gone.max(us);
            match us {
                0..=25 => self.us_le_25 += 1,
                26..=100 => self.us_le_100 += 1,
                101..=1000 => self.us_le_1000 += 1,
                _ => self.us_gt_1000 += 1,
            }
        }
        if let Some((said, pending)) = &first_miss {
            if said == "listed" {
                self.miss_listed += 1;
            } else {
                self.miss_refused += 1;
            }
            self.pending_at_miss += usize::from(*pending);
        }
        // Phases are `n << 8 | p`: 2 published, 3 partial removed, 4
        // retiring, 5 retired. Partial removed before the read began: the
        // retire. Retire not begun when the checks ended: the partial.
        let during = if before >= (n << 8 | 3) {
            self.during_retire += 1;
            "retire"
        } else if after < (n << 8 | 4) {
            self.during_partial_removal += 1;
            "partial removal"
        } else {
            self.during_either += 1;
            "either"
        };
        println!(
            "OWL426 refusal variant={variant} {name}: {error} ({step}); status 0x{status:08X}; \
             gone at ask {asks} after {us_to_gone:?} us; first miss (metadata, delete pending \
             now): {first_miss:?}; writer {}.{} -> {}.{} ({during})",
            before >> 8,
            before & 0xff,
            after >> 8,
            after & 0xff,
        );
    }
}

/// See the OWL-426 probe's header above.
#[test]
#[ignore = "OWL-426 probe, run by its workflow only"]
fn owl426_probe() {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    const WRITES: u64 = 300;
    let variant = std::env::var("OWL426_VARIANT").unwrap_or_else(|_| "link-delete".to_owned());
    // Round 3: `files::publish` is removed; "final" is the production writer,
    // `write_obligation` then `remove_obligation`.
    let (link, delete) = match variant.as_str() {
        "link-delete" | "final" => (true, true),
        "link-rename" => (true, false),
        other => panic!("unknown variant {other}"),
    };
    let production = variant == "final";
    let tmp = tempfile::tempdir().unwrap();
    #[cfg(windows)]
    owl426_calibrate(tmp.path());
    let dirs = ProjectDirs::new(&tmp.path().join("data"), &Repo::parse("acme/app").unwrap());
    let folder = dirs.obligations_dir();
    let retired = dirs.root().join("retired");
    fs::create_dir_all(&folder).unwrap();
    fs::create_dir_all(&retired).unwrap();
    let owl = ticket("OWL-1");
    let done = AtomicBool::new(false);
    let phase = AtomicU64::new(0);
    let mut reader = Owl426Reader::default();
    let writer = std::thread::scope(|scope| {
        let writer = scope.spawn(|| {
            let _done = Owl426Done(&done);
            let mut counts = Owl426Writer::default();
            for n in 0..WRITES {
                let set = |p: u64| phase.store(n << 8 | p, Ordering::SeqCst);
                if production {
                    let name =
                        write_obligation(&dirs, Some(&owl), &format!("r{n}"), "a run").unwrap();
                    set(3);
                    set(4);
                    remove_obligation(&dirs, &name).unwrap();
                    set(5);
                    continue;
                }
                let run = format!("r{n}");
                let name = obligation_name(Some(&owl), &run);
                let record = RunObligation {
                    format: Format,
                    ticket: Some(owl.clone()),
                    run,
                    started: Timestamp::now(),
                    what: "a run".to_owned(),
                };
                let partial = dirs.root().join(format!("unverified-{name}.partial"));
                let target = folder.join(&name);
                let mut file = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&partial)
                    .unwrap();
                file.write_all(record.render().as_bytes()).unwrap();
                file.sync_all().unwrap();
                set(1);
                if link {
                    // fb88d18's writer: linked with the handle open, then the
                    // partial removed, a failure there ignored.
                    owl426_retry(&mut counts.link_retries, || {
                        fs::hard_link(&partial, &target)
                    });
                    drop(file);
                    set(2);
                    if fs::remove_file(&partial).is_err() {
                        counts.partial_remove_failures += 1;
                    }
                }
                set(3);
                set(4);
                if delete {
                    owl426_retry(&mut counts.retire_retries, || fs::remove_file(&target));
                } else {
                    owl426_retry(&mut counts.retire_retries, || {
                        fs::rename(&target, retired.join(&name))
                    });
                }
                set(5);
            }
            counts
        });
        let deadline = Instant::now() + Duration::from_secs(120);
        while !done.load(Ordering::Acquire) && Instant::now() < deadline {
            reader.pass(&folder, &phase, &variant);
        }
        writer.join().unwrap()
    });
    let r = &reader;
    println!(
        "OWL426 summary variant={variant} sightings={} vanished={} unparsed={} refused={} \
         status_delete_pending={} status_other={} gone_first_ask={} gone_second_ask={} \
         gone_later_ask={} never_gone={} max_asks={} max_us_to_gone={} us_le_25={} \
         us_le_100={} us_le_1000={} us_gt_1000={} miss_listed={} miss_refused={} \
         pending_at_miss={} over_cap={} during_partial_removal={} during_retire={} during_either={} \
         link_retries={} partial_remove_failures={} publish_retries={} retire_retries={}",
        r.sightings,
        r.vanished,
        r.unparsed,
        r.refused,
        r.status_delete_pending,
        r.status_other,
        r.gone_first_ask,
        r.gone_second_ask,
        r.gone_later_ask,
        r.never_gone,
        r.max_asks,
        r.max_us_to_gone,
        r.us_le_25,
        r.us_le_100,
        r.us_le_1000,
        r.us_gt_1000,
        r.miss_listed,
        r.miss_refused,
        r.pending_at_miss,
        r.over_cap,
        r.during_partial_removal,
        r.during_retire,
        r.during_either,
        writer.link_retries,
        writer.partial_remove_failures,
        writer.publish_retries,
        writer.retire_retries,
    );
    assert!(reader.sightings > 0, "the reader saw no obligation");
}
