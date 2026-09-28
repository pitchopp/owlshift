//! The executor's isolation check (`owlshift_runner::executor::isolation`)
//! on a real repository: a run's own work passes, and each way of leaving
//! the worktree is found. One fixture; each breach is checked between two
//! snapshots taken around it.

use std::fs;
use std::path::PathBuf;

use tempfile::TempDir;

use owlshift_runner::executor::Git;
use owlshift_runner::executor::isolation::{Snapshot, Violation};
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

    fn main_git(&self, args: &[&str]) {
        self.env.run(&self.main, args).unwrap();
    }

    fn worktree_git(&self, args: &[&str]) {
        self.env.run(&self.worktree, args).unwrap();
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
