//! The executor's isolation check (`owlshift_runner::executor::isolation`)
//! on a real repository: a run's own work passes, and each way of leaving
//! the worktree is found. Each breach is checked between two snapshots
//! taken around it. A rebase run and runs in flight at the same time pass
//! when the check is told of them, and only then.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use tempfile::TempDir;

use owlshift_runner::agent_env::AgentEnv;
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

    /// Runs git in the worktree as the agent's own shell does: with the
    /// agent environment the executor gives it, and no other variable.
    fn agent_git(&self, args: &[&str]) {
        let agent = AgentEnv::new(self.env.agent_parent()).unwrap();
        let mut command = Command::new("git");
        command
            .args(args)
            .current_dir(&self.worktree)
            .stdin(Stdio::null());
        agent.apply(&mut command);
        let out = command.output().unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
    }

    /// The commit `rev` names, seen from `dir`.
    fn commit(&self, dir: &Path, rev: &str) -> String {
        let id = self.env.run(dir, &["rev-parse", "--verify", rev]).unwrap();
        String::from_utf8(id).unwrap().trim().to_owned()
    }

    /// Makes git read `file` of `dir` again, through its filters, whenever
    /// it checks the file for a change. Git skips that read when the stat
    /// data of an index entry shows the file changed or shows nothing
    /// racy, so a control that expects a filter to run cannot rely on a
    /// plain edit. An entry staged with `--cacheinfo` has no stat data:
    /// git can only compare the content, and reads it through the clean
    /// filter, whatever the timing.
    fn force_reread(&self, dir: &Path, file: &str) {
        let blob = self.commit(dir, &format!("HEAD:{file}"));
        let entry = format!("100644,{blob},{file}");
        self.env
            .run(dir, &["update-index", "--cacheinfo", &entry])
            .unwrap();
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

    // The run's own work: a commit on its branch, its ignored run files, a
    // fetch moving a remote-tracking ref, and git's own housekeeping as the
    // agent's git does it (OWL-197): a `gc` stands in for the maintenance a
    // commit starts, whose repack writes no `info/refs` there.
    let own = f.around(|f| {
        fs::write(f.worktree.join("GREETING.md"), "hi\n").unwrap();
        f.worktree_git(&["add", "GREETING.md"]);
        f.worktree_git(&["commit", "--quiet", "-m", "Greet"]);
        f.agent_git(&["gc", "--quiet"]);
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
    // All of `info/` stays guarded: an ignore rule, and the `info/refs` that
    // housekeeping writes when nothing turns it off, as a `gc` does with the
    // server information turned back on (the bench's git turns it off,
    // OWL-240).
    assert_eq!(
        f.around(|f| fs::write(f.main.join(".git/info/exclude"), "stray/\n").unwrap()),
        [Violation::SharedGitFiles(vec!["info/exclude".into()])]
    );
    assert_eq!(
        f.around(|f| f.worktree_git(&["-c", "repack.updateServerInfo=true", "gc", "--quiet"])),
        [Violation::SharedGitFiles(vec!["info/refs".into()])]
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
        matches!(
            rewound.as_slice(),
            [Violation::History {
                rewritten: true,
                ..
            }]
        ),
        "{rewound:?}"
    );

    // A worktree gone takes its `.git` link with it: a violation, found
    // before any git runs.
    let gone = f.around(|f| fs::remove_dir_all(&f.worktree).unwrap());
    assert!(
        matches!(gone.as_slice(), [Violation::WorktreeLink(_)]),
        "{gone:?}"
    );
}

/// What a run leaves behind its worktree's `.git`, or in the shared git
/// files, is found on the file system before the runner's git runs: no git
/// command runs after the breach, so neither a git directory of the run's
/// own, nor another repository the main checkout's `commondir` points at,
/// nor a clean filter it configured, which `git status` of the main
/// checkout would run on a changed file, ever runs.
#[test]
fn a_redirected_link_or_a_planted_filter_stops_the_check_before_any_git() {
    for case in [
        "a redirected link",
        "a redirected checkout",
        "a planted filter",
    ] {
        let f = Fixture::new();
        let ran: Arc<Mutex<Option<Vec<PathBuf>>>> = Arc::new(Mutex::new(None));
        let runner = f.env.clone();
        let seen = ran.clone();
        let git = Git::with_setup("git", move |command| {
            runner.apply(command);
            if let Some(dirs) = seen.lock().unwrap().as_mut() {
                dirs.push(command.get_current_dir().unwrap().to_owned());
            }
        });
        let before = Snapshot::take(&git, &f.main, &f.worktree, BRANCH).unwrap();
        let fired = f._tmp.path().join("filter-fired");
        match case {
            "a redirected link" => {
                let evil = f._tmp.path().join("evil");
                fs::create_dir_all(&evil).unwrap();
                fs::write(
                    f.worktree.join(".git"),
                    format!("gitdir: {}\n", evil.display()),
                )
                .unwrap();
            }
            "a redirected checkout" => {
                // Git in the main checkout would read that repository's
                // configuration, and whatever filter it names (OWL-50).
                let evil = f._tmp.path().join("evil");
                fs::create_dir_all(&evil).unwrap();
                fs::write(
                    f.main.join(".git/commondir"),
                    format!("{}\n", evil.display()),
                )
                .unwrap();
            }
            _ => {
                let fired = fired.to_string_lossy().replace('\\', "/");
                // Before the filter is configured: git would run it here.
                f.force_reread(&f.main, "README.md");
                f.main_git(&[
                    "config",
                    "filter.owl.clean",
                    &format!("sh -c 'echo ran > \"{fired}\"; cat'"),
                ]);
                fs::write(f.main.join(".git/info/attributes"), "* filter=owl\n").unwrap();
                // A tracked file whose status git must read again, through
                // the filter.
                fs::write(f.main.join("README.md"), "changed\n").unwrap();
            }
        }
        *ran.lock().unwrap() = Some(Vec::new());
        let violations = before.check(&git, &f.main, &f.worktree, BRANCH);
        let expected = match case {
            "a redirected link" => matches!(violations.as_slice(), [Violation::WorktreeLink(_)]),
            "a redirected checkout" => {
                violations == [Violation::SharedGitFiles(vec!["commondir".into()])]
            }
            _ => {
                violations
                    == [Violation::SharedGitFiles(vec![
                        "config".into(),
                        "info/attributes".into(),
                    ])]
            }
        };
        assert!(expected, "{case}: {violations:?}");
        assert_eq!(ran.lock().unwrap().as_deref(), Some(&[][..]), "{case}");
        assert!(!fired.exists(), "{case}: the filter ran");
        if case == "a planted filter" {
            // The control: `git status` does run it, whatever the stat data.
            f.main_git(&["status", "--porcelain"]);
            assert!(fired.exists(), "the planted filter never runs");
        }
    }
}

/// A repository a run nests in the main checkout, with a clean filter and a
/// file-system monitor in its own configuration, which no shared git file
/// names, and a gitlink staged for it: the check finds the gitlink without
/// starting git in the nested repository, so neither helper runs. Plain
/// `git status` would look into it and run the filter (OWL-50).
#[test]
fn a_planted_submodule_starts_no_git_in_it() {
    let f = Fixture::new();
    let fired = f._tmp.path().join("filter-fired");
    let monitored = f._tmp.path().join("monitor-fired");
    let sub = f.main.join("sub");
    let violations = f.around(|f| {
        f.main_git(&["init", "--quiet", "sub"]);
        f.commit_file(&sub, "file.txt", "one\n");
        // Before any helper is configured: git runs those too.
        f.force_reread(&sub, "file.txt");
        let sentinel = |path: &Path| path.to_string_lossy().replace('\\', "/");
        f.env
            .run(
                &sub,
                &[
                    "config",
                    "filter.owl.clean",
                    &format!("sh -c 'echo ran > \"{}\"; cat'", sentinel(&fired)),
                ],
            )
            .unwrap();
        f.env
            .run(
                &sub,
                &[
                    "config",
                    "core.fsmonitor",
                    &format!("sh -c 'echo ran > \"{}\"'", sentinel(&monitored)),
                ],
            )
            .unwrap();
        fs::write(sub.join(".gitattributes"), "* filter=owl\n").unwrap();
        // A tracked file whose status the nested git must read again,
        // through the filter.
        fs::write(sub.join("file.txt"), "two\n").unwrap();
        let gitlink = format!("160000,{},sub", f.commit(&sub, "HEAD"));
        f.main_git(&["update-index", "--add", "--cacheinfo", &gitlink]);
    });
    assert_eq!(violations, [files(&["sub"])]);
    assert!(!fired.exists(), "the nested repository's filter ran");
    assert!(!monitored.exists(), "the nested repository's monitor ran");
    // The control: plain `git status` looks into the nested repository and
    // runs its filter.
    f.main_git(&["status", "--porcelain"]);
    assert!(fired.exists(), "the nested repository's filter never runs");
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
    // A rebase that lands elsewhere shares history with its start by design:
    // its message names no rewrite.
    assert_eq!(
        rewound,
        [Violation::History {
            start: start.clone(),
            end: seeded.clone(),
            onto: Some(upstream.clone()),
            rewritten: false,
        }]
    );
    assert_eq!(
        rewound[0].to_string(),
        format!(
            "the branch is at {seeded}, which descends neither from the run's start {start} \
             nor from its new base {upstream}"
        )
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

/// OWL-369: a run that may only add to its branch and rewrote it is still a
/// breach, and its message says the run rewrote its own history when the
/// branch still shares a commit with the start; a history from elsewhere
/// keeps the plain message.
#[test]
fn a_rewritten_branch_is_a_breach_that_names_the_rewrite() {
    let f = Fixture::new();
    // Before the run: the earlier runs' commits on the branch.
    f.commit_file(&f.worktree, "GREETING.md", "hi\n");
    f.commit_file(&f.worktree, "GREETING.md", "hi again\n");
    let start = f.commit(&f.worktree, "HEAD");

    // OWL-160: the run re-applies the branch onto its own base, the same
    // trees under new commits (another committer, so the ids differ
    // whatever the clock), then adds a commit.
    let rebased = f.around(|f| {
        f.worktree_git(&[
            "-c",
            "user.name=Rebaser",
            "rebase",
            "--quiet",
            "--force-rebase",
            "origin/main",
        ]);
        f.commit_file(&f.worktree, "NEW.md", "new\n");
    });
    let end = f.commit(&f.worktree, "HEAD");
    assert_ne!(f.commit(&f.worktree, "HEAD~1"), start);
    assert_eq!(
        f.commit(&f.worktree, "HEAD~1^{tree}"),
        f.commit(&f.worktree, &format!("{start}^{{tree}}"))
    );
    assert_eq!(
        rebased,
        [Violation::History {
            start: start.clone(),
            end: end.clone(),
            onto: None,
            rewritten: true,
        }]
    );
    assert_eq!(
        rebased[0].to_string(),
        format!(
            "the branch is at {end}, which does not descend from the run's start {start}: \
             the run rewrote its branch's history (rebase, amend or reset)"
        )
    );
    f.worktree_git(&["reset", "--quiet", "--hard", &start]);

    // A commit sharing nothing with the start: the plain message.
    let unrelated = f.around(|f| {
        let tree = format!("{start}^{{tree}}");
        let orphan = f
            .env
            .run(&f.worktree, &["commit-tree", &tree, "-m", "Unrelated"])
            .unwrap();
        let orphan = String::from_utf8(orphan).unwrap();
        f.worktree_git(&["reset", "--quiet", "--hard", orphan.trim()]);
    });
    let orphan = f.commit(&f.worktree, "HEAD");
    assert_eq!(
        unrelated,
        [Violation::History {
            start: start.clone(),
            end: orphan.clone(),
            onto: None,
            rewritten: false,
        }]
    );
    assert_eq!(
        unrelated[0].to_string(),
        format!("the branch is at {orphan}, which does not descend from the run's start {start}")
    );
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
    // other runs' branches and their worktrees' entries.
    let alone = first.check(&git, &f.main, &f.worktree, BRANCH);
    assert!(
        alone.contains(&Violation::Refs(vec![
            format!("refs/heads/{SECOND}"),
            format!("refs/heads/{THIRD}"),
            format!("refs/heads/{FOURTH}"),
        ])),
        "{alone:?}"
    );
    let entries: Vec<&String> = alone
        .iter()
        .flat_map(|violation| match violation {
            Violation::WorktreeEntries(paths) => paths.iter().collect(),
            _ => Vec::new(),
        })
        .collect();
    for path in [
        "worktrees/worktree-2/index",
        "worktrees/worktree-3/HEAD",
        "worktrees/worktree-4/index",
    ] {
        assert!(
            entries.iter().any(|entry| *entry == path),
            "{path}: {alone:?}"
        );
    }
    assert_eq!(alone.len(), 2, "{alone:?}");

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

const OTHER: &str = "other";
const RUNNING: &str = "owlshift/running";

/// The fixture with a second worktree, `other`, made before any snapshot,
/// and the folder of its entry in the common git folder.
fn with_other() -> (Fixture, PathBuf) {
    let f = Fixture::new();
    let base = f.commit(&f.main, "origin/main");
    f.main_git(&["worktree", "add", "--quiet", "-b", OTHER, "../other", &base]);
    let entry = f.main.join(".git/worktrees").join(OTHER);
    (f, entry)
}

fn entries(paths: &[&str]) -> Violation {
    Violation::WorktreeEntries(paths.iter().map(|path| (*path).to_owned()).collect())
}

fn running(branches: &[&str]) -> Concurrent {
    Concurrent {
        running: branches.iter().map(|branch| (*branch).to_owned()).collect(),
        ..Concurrent::default()
    }
}

/// A run's own commit, `add` and rebase change its own entry and nothing of
/// another's; each way of reaching another worktree's entry is found
/// (OWL-339).
#[test]
fn another_worktrees_entry_is_guarded_and_the_runs_own_is_not() {
    let (f, other) = with_other();
    // A new upstream commit, so that the run's rebase has work to do.
    f.commit_file(&f.main, "README.md", "upstream\n");
    f.main_git(&["update-ref", "refs/remotes/origin/main", "HEAD"]);
    // The run's own work: a commit, an `add`, and a rebase.
    let own = f.around(|f| {
        fs::write(f.worktree.join("GREETING.md"), "hi\n").unwrap();
        f.worktree_git(&["add", "GREETING.md"]);
        f.worktree_git(&["commit", "--quiet", "-m", "Greet"]);
        f.worktree_git(&["rebase", "--quiet", "origin/main"]);
    });
    assert_eq!(own, []);

    // An edit of another's `index` and `HEAD`, a new file, a removed file.
    let append = |name: &'static str, text: &'static str| {
        let other = other.clone();
        move |_: &Fixture| {
            let path = other.join(name);
            let mut bytes = fs::read(&path).unwrap_or_default();
            bytes.extend_from_slice(text.as_bytes());
            fs::write(path, bytes).unwrap();
        }
    };
    assert_eq!(
        f.around(append("index", "x")),
        [entries(&["worktrees/other/index"])]
    );
    assert_eq!(
        f.around(append("HEAD", "x\n")),
        [entries(&["worktrees/other/HEAD"])]
    );
    assert_eq!(
        f.around(append("planted", "x")),
        [entries(&["worktrees/other/planted"])]
    );
    assert_eq!(
        f.around(|_| fs::remove_file(other.join("planted")).unwrap()),
        [entries(&["worktrees/other/planted"])]
    );
    // A new entry, with every path it holds.
    assert_eq!(
        f.around(|f| {
            let evil = f.main.join(".git/worktrees/evil");
            fs::create_dir_all(evil.join("logs")).unwrap();
            fs::write(evil.join("HEAD"), "ref: refs/heads/evil\n").unwrap();
        }),
        [entries(&[
            "worktrees/evil",
            "worktrees/evil/HEAD",
            "worktrees/evil/logs"
        ])]
    );
    // Together with a hook, only the shared files are reported: the check
    // ends before any git, and so before the entries are compared.
    assert_eq!(
        f.around(|f| {
            fs::write(other.join("index"), "tampered").unwrap();
            fs::write(f.main.join(".git/hooks/pre-commit"), "#!/bin/sh\n").unwrap();
        }),
        [Violation::SharedGitFiles(vec!["hooks/pre-commit".into()])]
    );
}

/// A change past the first mebibyte of a large file of another's entry is
/// found: the whole file counts (OWL-339).
#[test]
fn a_change_past_the_first_mebibyte_of_an_entry_file_is_found() {
    let (f, other) = with_other();
    let mut bytes = vec![b'a'; 1024 * 1024 + 100];
    fs::write(other.join("big"), &bytes).unwrap();
    *bytes.last_mut().unwrap() = b'b';
    assert_eq!(
        f.around(|_| fs::write(other.join("big"), &bytes).unwrap()),
        [entries(&["worktrees/other/big"])]
    );
}

/// Linux alone makes these names: a file whose name is not UTF-8, planted in
/// another entry or under `hooks/`, is a breach, listed with the name made
/// lossy.
#[cfg(target_os = "linux")]
#[test]
fn a_name_that_is_not_utf8_is_found_in_an_entry_and_in_the_hooks() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    let (f, other) = with_other();
    let odd = OsStr::from_bytes(b"x\xff");
    assert_eq!(
        f.around(|_| fs::write(other.join(odd), "x").unwrap()),
        [entries(&["worktrees/other/x\u{fffd}"])]
    );
    assert_eq!(
        f.around(|f| fs::write(f.main.join(".git/hooks").join(odd), "x").unwrap()),
        [Violation::SharedGitFiles(vec!["hooks/x\u{fffd}".into()])]
    );
}

/// A run in flight at the same time may change its own entry: told of it,
/// in either answer, the check leaves it out, and only it (OWL-339).
#[test]
fn a_concurrent_runs_entry_is_left_out_when_the_check_is_told_of_it() {
    let (f, other) = with_other();
    let check = |before: &Snapshot, first: Concurrent, second: Concurrent| {
        let calls = std::cell::Cell::new(0);
        before
            .check_among(&f.git, &f.main, &f.worktree, BRANCH, &|| {
                calls.set(calls.get() + 1);
                if calls.get() == 1 {
                    first.clone()
                } else {
                    second.clone()
                }
            })
            .violations
    };
    let before = Snapshot::take(&f.git, &f.main, &f.worktree, BRANCH).unwrap();
    // The other run commits in its worktree: its index, logs and `HEAD`.
    let other_worktree = f.worktree.with_file_name("other");
    f.commit_file(&other_worktree, "OTHER.md", "other\n");
    fs::write(other.join("planted"), "x").unwrap();
    // The branch `other` moved too, which is the same run's.
    let named = running(&[OTHER]);
    assert_eq!(check(&before, named.clone(), named.clone()), []);
    assert_eq!(check(&before, Concurrent::default(), named.clone()), []);
    assert_eq!(check(&before, named.clone(), Concurrent::default()), []);
    // Told of an unrelated run, or of none, the entry is a breach.
    for told in [running(&["owlshift/unrelated"]), Concurrent::default()] {
        let found = check(&before, told.clone(), told);
        assert!(
            found.iter().any(|violation| matches!(
                violation,
                Violation::WorktreeEntries(paths)
                    if paths.contains(&"worktrees/other/planted".to_owned())
            )),
            "{found:?}"
        );
    }
    // A write to the entry together with its owner's: the owner is the one
    // the snapshot saw, so rewriting `HEAD` to another branch does not move
    // the entry to a running one.
    fs::write(other.join("HEAD"), format!("ref: refs/heads/{RUNNING}\n")).unwrap();
    let forged = running(&[RUNNING]);
    let found = check(&before, forged.clone(), forged);
    assert!(
        found
            .iter()
            .any(|violation| matches!(violation, Violation::WorktreeEntries(_))),
        "{found:?}"
    );
}

/// An entry made after the snapshot is a run's when the check is told so,
/// and a breach when it is not; and a run that registers between the check's
/// two answers, with its entry made before the second one, is not flagged
/// even though the first answer was empty (OWL-339).
#[test]
fn an_entry_made_after_the_snapshot_is_judged_by_the_branch_it_holds() {
    let make = |f: &Fixture| {
        let base = f.commit(&f.main, "origin/main");
        f.main_git(&[
            "worktree",
            "add",
            "--quiet",
            "-b",
            RUNNING,
            "../running",
            &base,
        ]);
    };
    let (f, _) = with_other();
    let before = Snapshot::take(&f.git, &f.main, &f.worktree, BRANCH).unwrap();
    make(&f);
    let told = running(&[RUNNING]);
    let checked = before.check_among(&f.git, &f.main, &f.worktree, BRANCH, &|| told.clone());
    assert_eq!(checked.violations, []);
    let alone = before.check(&f.git, &f.main, &f.worktree, BRANCH);
    assert!(
        alone
            .iter()
            .any(|violation| matches!(violation, Violation::WorktreeEntries(_))),
        "{alone:?}"
    );

    // The registration race: the entry is made by the scheduler's first
    // answer, which is empty; the second answer knows the run.
    for (second, expected_clean) in [(running(&[RUNNING]), true), (Concurrent::default(), false)] {
        let (f, _) = with_other();
        let before = Snapshot::take(&f.git, &f.main, &f.worktree, BRANCH).unwrap();
        let calls = std::cell::Cell::new(0);
        let checked = before.check_among(&f.git, &f.main, &f.worktree, BRANCH, &|| {
            calls.set(calls.get() + 1);
            if calls.get() == 1 {
                make(&f);
                Concurrent::default()
            } else {
                second.clone()
            }
        });
        assert_eq!(checked.violations.is_empty(), expected_clean, "{checked:?}");
        if !expected_clean {
            assert!(
                checked
                    .violations
                    .iter()
                    .any(|violation| matches!(violation, Violation::WorktreeEntries(_))),
                "{checked:?}"
            );
        }
    }
}
