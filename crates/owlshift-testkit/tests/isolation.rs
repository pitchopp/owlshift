//! The executor's isolation check (`owlshift_runner::executor::isolation`)
//! on a real repository: a run's own work passes, and each way of leaving
//! the worktree is found. Each breach is checked between two snapshots
//! taken around it. A rebase run and runs in flight at the same time pass
//! when the check is told of them, and only then.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tempfile::TempDir;

use owlshift_runner::executor::Git;
use owlshift_runner::executor::isolation::{Concurrent, Snapshot, Violation};
use owlshift_testkit::git::{GitEnv, seed};

const BRANCH: &str = "owlshift/T-1";

struct Fixture {
    _tmp: TempDir,
    env: GitEnv,
    git: Git,
    main: PathBuf,
    worktree: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::Builder::new()
            .prefix("owlshift isolation ")
            .tempdir()
            .unwrap();
        let project = tmp.path().join("project");
        fs::create_dir_all(&project).unwrap();
        fs::write(project.join("README.md"), "hello\n").unwrap();
        fs::write(project.join("NOTES.md"), "notes\n").unwrap();
        fs::write(project.join(".gitignore"), ".env\ntarget/\n").unwrap();
        let env = GitEnv::create(tmp.path().join("home")).unwrap();
        let remote = seed(&env, tmp.path(), &project).unwrap();
        env.run(
            &remote.checkout,
            &[
                "worktree",
                "add",
                "--quiet",
                "-b",
                BRANCH,
                "../worktree",
                "origin/main",
            ],
        )
        .unwrap();
        let runner = env.clone();
        Self {
            git: Git::with_setup("git", move |command| runner.apply(command)),
            main: remote.checkout,
            worktree: tmp.path().join("worktree"),
            env,
            _tmp: tmp,
        }
    }

    /// The violations found around `breach`.
    fn around(&self, breach: impl FnOnce(&Self)) -> Vec<Violation> {
        let before = Snapshot::take(&self.git, &self.main, &self.worktree, BRANCH).unwrap();
        breach(self);
        before.check(&self.git, &self.main, &self.worktree, BRANCH)
    }

    /// The violations found around `breach`, for a rebase onto `onto`.
    fn around_rebase(&self, onto: &str, breach: impl FnOnce(&Self)) -> Vec<Violation> {
        let before =
            Snapshot::take_rebase(&self.git, &self.main, &self.worktree, BRANCH, onto).unwrap();
        breach(self);
        before.check(&self.git, &self.main, &self.worktree, BRANCH)
    }

    fn main_git(&self, args: &[&str]) {
        self.env.run(&self.main, args).unwrap();
    }

    fn worktree_git(&self, args: &[&str]) {
        self.env.run(&self.worktree, args).unwrap();
    }

    /// The commit `rev` names, seen from `dir`.
    fn commit(&self, dir: &Path, rev: &str) -> String {
        let id = self.env.run(dir, &["rev-parse", "--verify", rev]).unwrap();
        String::from_utf8(id).unwrap().trim().to_owned()
    }

    /// Commits `text` as `file` in `dir`.
    fn commit_file(&self, dir: &Path, file: &str, text: &str) {
        fs::write(dir.join(file), text).unwrap();
        self.env.run(dir, &["add", file]).unwrap();
        self.env
            .run(dir, &["commit", "--quiet", "-m", &format!("Write {file}")])
            .unwrap();
    }
}

fn files(paths: &[&str]) -> Violation {
    Violation::MainFiles(paths.iter().map(|path| (*path).to_owned()).collect())
}

#[test]
fn a_runs_own_work_passes_and_every_breach_is_found() {
    let f = Fixture::new();

    // The run's own work: a commit on its branch, its ignored run files, and
    // a fetch moving a remote-tracking ref.
    let own = f.around(|f| {
        fs::write(f.worktree.join("GREETING.md"), "hi\n").unwrap();
        f.worktree_git(&["add", "GREETING.md"]);
        f.worktree_git(&["commit", "--quiet", "-m", "Greet"]);
        fs::create_dir_all(f.worktree.join(".owlshift/run")).unwrap();
        fs::write(f.worktree.join(".owlshift/run/.gitignore"), "*\n").unwrap();
        fs::write(f.worktree.join(".owlshift/run/result.json"), "{}").unwrap();
        f.main_git(&["update-ref", "refs/remotes/origin/fetched", "HEAD"]);
    });
    assert_eq!(own, []);

    // The main checkout: a tracked edit, then a second edit that changes the
    // content alone, an untracked file, an ignored one, a rename.
    let edit =
        |text: &'static str| move |f: &Fixture| fs::write(f.main.join("README.md"), text).unwrap();
    assert_eq!(f.around(edit("edited\n")), [files(&["README.md"])]);
    assert_eq!(f.around(edit("edited again\n")), [files(&["README.md"])]);
    assert_eq!(
        f.around(|f| fs::write(f.main.join("stray.txt"), "x").unwrap()),
        [files(&["stray.txt"])]
    );
    assert_eq!(
        f.around(|f| fs::write(f.main.join(".env"), "TOKEN=x\n").unwrap()),
        [files(&[".env"])]
    );
    // A staged rename is one record, under its new name.
    assert_eq!(
        f.around(|f| f.main_git(&["mv", "NOTES.md", "NOTES 2.md"])),
        [files(&["NOTES 2.md"])]
    );

    // A commit there moves its HEAD and its branch.
    let commit = f.around(|f| f.main_git(&["commit", "--quiet", "-am", "Stray commit"]));
    assert!(commit.contains(&Violation::MainHead), "{commit:?}");
    assert!(
        commit.contains(&Violation::Refs(vec!["refs/heads/main".into()])),
        "{commit:?}"
    );

    // Refs the whole repository shares: a tag, the stash.
    assert_eq!(
        f.around(|f| f.main_git(&["tag", "v1"])),
        [Violation::Refs(vec!["refs/tags/v1".into()])]
    );
    assert_eq!(
        f.around(|f| {
            fs::write(f.worktree.join("GREETING.md"), "changed\n").unwrap();
            f.worktree_git(&["stash", "--quiet"]);
        }),
        [Violation::Refs(vec!["refs/stash".into()])]
    );

    // Shared git files: a hook, and the configuration, written from the
    // worktree.
    assert_eq!(
        f.around(|f| fs::write(f.main.join(".git/hooks/pre-commit"), "#!/bin/sh\n").unwrap()),
        [Violation::SharedGitFiles(vec!["hooks/pre-commit".into()])]
    );
    assert_eq!(
        f.around(|f| f.worktree_git(&["config", "core.hooksPath", "/elsewhere"])),
        [Violation::SharedGitFiles(vec!["config".into()])]
    );

    // The worktree: another branch, then history rewritten below the start.
    let switched = f.around(|f| f.worktree_git(&["switch", "--quiet", "-c", "other"]));
    assert!(
        switched.contains(&Violation::Branch {
            expected: format!("refs/heads/{BRANCH}"),
            found: Some("refs/heads/other".into()),
        }),
        "{switched:?}"
    );
    f.worktree_git(&["switch", "--quiet", BRANCH]);
    let rewound = f.around(|f| f.worktree_git(&["reset", "--quiet", "--hard", "HEAD~1"]));
    assert!(
        matches!(rewound.as_slice(), [Violation::History { .. }]),
        "{rewound:?}"
    );

    // A check that cannot run is a violation.
    let gone = f.around(|f| fs::remove_dir_all(&f.worktree).unwrap());
    assert!(
        matches!(gone.as_slice(), [Violation::CheckFailed(_)]),
        "{gone:?}"
    );
}

/// A main checkout whose status is larger than the probes' 64 KiB output cap
/// is read whole: a breach listed past that point is still found.
#[test]
fn a_breach_past_64_kib_of_status_is_found() {
    let f = Fixture::new();
    // 1,200 untracked files of 60-byte names: some 74 KiB of status before
    // the breach, which sorts after them.
    let filler = "x".repeat(54);
    for n in 0..1200 {
        fs::write(f.main.join(format!("a{n:04}-{filler}")), "").unwrap();
    }
    assert_eq!(
        f.around(|f| fs::write(f.main.join("z-breach.txt"), "x").unwrap()),
        [files(&["z-breach.txt"])]
    );
}

#[test]
fn a_rebase_run_may_land_on_its_new_base_and_nowhere_else() {
    let f = Fixture::new();
    // Before any snapshot: two commits on the run's branch, another ticket's
    // branch stacked on the first of them, and a new upstream commit.
    f.commit_file(&f.worktree, "GREETING.md", "hi\n");
    f.worktree_git(&["branch", "owlshift/T-0", "HEAD"]);
    f.commit_file(&f.worktree, "GREETING.md", "hi again\n");
    let start = f.commit(&f.worktree, "HEAD");
    let seeded = f.commit(&f.main, "origin/main");
    f.commit_file(&f.main, "README.md", "upstream\n");
    f.main_git(&["update-ref", "refs/remotes/origin/main", "HEAD"]);
    let upstream = f.commit(&f.main, "origin/main");
    let restart = |f: &Fixture| {
        f.main_git(&["update-ref", "refs/remotes/origin/main", &upstream]);
        f.worktree_git(&["reset", "--quiet", "--hard", &start]);
    };

    // The rebase lands on the new base.
    let rebased = f.around_rebase("origin/main", |f| {
        f.worktree_git(&["rebase", "--quiet", "origin/main"]);
    });
    assert_eq!(rebased, []);
    restart(&f);

    // A rebase that meets a conflict and is aborted leaves the branch as it
    // was, which is no breach either.
    f.commit_file(&f.worktree, "README.md", "mine\n");
    let aborted = f.around_rebase("origin/main", |f| {
        let conflict = f
            .env
            .run(&f.worktree, &["rebase", "--quiet", "origin/main"]);
        assert!(conflict.is_err(), "the rebase met no conflict");
        f.worktree_git(&["rebase", "--abort"]);
    });
    assert_eq!(aborted, []);
    restart(&f);

    // The base is the commit it was before the run: moving the
    // remote-tracking ref that named it does not bless a history that
    // descends from neither the start nor the base.
    let rewound = f.around_rebase("origin/main", |f| {
        f.worktree_git(&["reset", "--quiet", "--hard", &seeded]);
        f.worktree_git(&["update-ref", "refs/remotes/origin/main", "HEAD"]);
    });
    assert_eq!(
        rewound,
        [Violation::History {
            start: start.clone(),
            end: seeded.clone(),
            onto: Some(upstream.clone()),
        }]
    );
    restart(&f);

    // Other branches stay out of reach of a rebase: `--update-refs` moves
    // the branch stacked on the run's.
    let stacked = f.around_rebase("origin/main", |f| {
        f.worktree_git(&["rebase", "--quiet", "--update-refs", "origin/main"]);
    });
    assert_eq!(
        stacked,
        [Violation::Refs(vec!["refs/heads/owlshift/T-0".into()])]
    );

    // A base that names no commit is refused before the run.
    assert!(Snapshot::take_rebase(&f.git, &f.main, &f.worktree, BRANCH, "no-such-base").is_err());
}

#[test]
fn concurrent_runs_pass_and_every_other_ref_stays_guarded() {
    const SECOND: &str = "owlshift/T-2";
    const THIRD: &str = "owlshift/T-3";
    const FOURTH: &str = "owlshift/T-4";
    let f = Fixture::new();
    // Branches are created from a commit, as the executor does: from a
    // remote-tracking name, git would write their upstream into the shared
    // configuration.
    let base = f.commit(&f.main, "origin/main");
    let add = |branch: &str, path: &str| {
        f.main_git(&["worktree", "add", "--quiet", "-b", branch, path, &base]);
    };
    add(SECOND, "../worktree-2");
    add(FOURTH, "../worktree-4");
    f.main_git(&["branch", "owlshift/T-9", &base]);
    let second = f.worktree.with_file_name("worktree-2");
    let fourth = f.worktree.with_file_name("worktree-4");

    // The scheduler's view of the runs in flight, and what happens while a
    // check reads the refs: done once, just before git lists them.
    let registry = Arc::new(Mutex::new(Concurrent::default()));
    type Action = Box<dyn FnOnce() + Send>;
    let during_read: Arc<Mutex<Option<Action>>> = Arc::default();
    let git = {
        let (env, during_read) = (f.env.clone(), during_read.clone());
        Git::with_setup("git", move |command| {
            env.apply(command);
            if command.get_args().any(|arg| arg == "for-each-ref") {
                let action = during_read.lock().unwrap().take();
                if let Some(action) = action {
                    action();
                }
            }
        })
    };

    let first = Snapshot::take(&git, &f.main, &f.worktree, BRANCH).unwrap();
    let other = Snapshot::take(&git, &f.main, &second, SECOND).unwrap();
    f.commit_file(&f.worktree, "GREETING.md", "hi\n");
    f.commit_file(&second, "FAREWELL.md", "bye\n");

    // The second run ends first, while the first one still runs.
    let ended = other.check_among(&git, &f.main, &second, SECOND, &|| Concurrent {
        running: [BRANCH.to_owned()].into(),
        ..Concurrent::default()
    });
    assert_eq!(ended.violations, []);
    let tip = ended.tip.expect("the second worktree is on its branch");
    assert_eq!(tip, f.commit(&second, "HEAD"));
    registry
        .lock()
        .unwrap()
        .ended
        .insert(SECOND.to_owned(), tip);

    // Then the first run's check. While it reads the refs, a third run
    // starts (it registers, then creates its branch) and the fourth run
    // ends: its check reports a last commit that lands after the read, so
    // the refs read hold its branch below that tip.
    f.commit_file(&fourth, "NOTE.md", "last\n");
    let last = f.commit(&fourth, "HEAD");
    f.env
        .run(&fourth, &["reset", "--quiet", "--hard", "HEAD~1"])
        .unwrap();
    registry.lock().unwrap().running.insert(FOURTH.to_owned());
    *during_read.lock().unwrap() = Some(Box::new({
        let (env, main, registry) = (f.env.clone(), f.main.clone(), registry.clone());
        let (base, last) = (base.clone(), last.clone());
        move || {
            let mut registry = registry.lock().unwrap();
            registry.running.insert(THIRD.to_owned());
            let add = [
                "worktree",
                "add",
                "--quiet",
                "-b",
                THIRD,
                "../worktree-3",
                &base,
            ];
            env.run(&main, &add).unwrap();
            registry.running.remove(FOURTH);
            registry.ended.insert(FOURTH.to_owned(), last);
        }
    }));
    let scheduler = || registry.lock().unwrap().clone();
    let checked = first.check_among(&git, &f.main, &f.worktree, BRANCH, &scheduler);
    assert!(
        during_read.lock().unwrap().is_none(),
        "nothing happened while the check read the refs"
    );
    assert_eq!(checked.violations, []);
    f.env
        .run(&fourth, &["reset", "--quiet", "--hard", &last])
        .unwrap();

    // Told of no other run, as a run alone is, the same check finds the
    // other runs' branches.
    assert_eq!(
        first.check(&git, &f.main, &f.worktree, BRANCH),
        [Violation::Refs(vec![
            format!("refs/heads/{SECOND}"),
            format!("refs/heads/{THIRD}"),
            format!("refs/heads/{FOURTH}"),
        ])]
    );

    // Among concurrent runs, an ended run's branch stays where its own check
    // found it, and a parked ticket's branch and the tags stay out of reach.
    f.worktree_git(&["update-ref", &format!("refs/heads/{SECOND}"), &base]);
    f.worktree_git(&["branch", "--force", "owlshift/T-9", "HEAD"]);
    f.worktree_git(&["tag", "v1"]);
    assert_eq!(
        first
            .check_among(&git, &f.main, &f.worktree, BRANCH, &scheduler)
            .violations,
        [Violation::Refs(vec![
            format!("refs/heads/{SECOND}"),
            "refs/heads/owlshift/T-9".into(),
            "refs/tags/v1".into(),
        ])]
    );
}
