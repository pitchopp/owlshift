//! The forge adapter's push against a local bare remote, with hermetic git:
//! the outcome is read from the exit status and the porcelain flag.

use std::fs;
use std::path::Path;
use std::process::Command;

use owlshift_adapters::forge::push::{
    PushError, Pushed, leased_push_command, push_command, read_push,
};
use owlshift_adapters::forge::{Branch, CommitId};
use owlshift_adapters::tracker::markdown::MarkdownTracker;
use owlshift_runner::executor::Git;
use owlshift_runner::neutral_commits::{NeutralHead, neutral_history};
use owlshift_runner::writer::{WriteError, Writer};
use owlshift_testkit::git::{GitEnv, Remote, seed};

/// `commit` as the Writer pushes it (OWL-558): its history's messages hold
/// no closing keyword, so it is its own neutral image.
fn neutral(git: &Git, checkout: &Path, commit: &CommitId) -> NeutralHead {
    let neutral = neutral_history(git, checkout, commit, &[]).unwrap();
    assert!(neutral.rewritten.is_empty(), "{:?}", neutral.rewritten);
    neutral.head
}

struct Bench {
    env: GitEnv,
    remote: Remote,
    _root: tempfile::TempDir,
}

fn bench() -> Bench {
    let root = tempfile::tempdir().unwrap();
    let env = GitEnv::create(root.path().join("home")).unwrap();
    let project = root.path().join("project");
    fs::create_dir(&project).unwrap();
    fs::write(project.join("README.md"), "seed\n").unwrap();
    let remote = seed(&env, root.path(), &project).unwrap();
    Bench {
        env,
        remote,
        _root: root,
    }
}

impl Bench {
    fn commit(&self, message: &str) -> CommitId {
        let dir = &self.remote.checkout;
        fs::write(dir.join("file.txt"), message).unwrap();
        self.env.run(dir, &["add", "--all"]).unwrap();
        self.env
            .run(dir, &["commit", "--quiet", "-m", message])
            .unwrap();
        self.head()
    }

    fn head(&self) -> CommitId {
        let out = self
            .env
            .run(&self.remote.checkout, &["rev-parse", "HEAD"])
            .unwrap();
        CommitId::new(String::from_utf8(out).unwrap().trim()).unwrap()
    }

    fn push(&self, remote: &str, commit: &CommitId, branch: &Branch) -> Result<Pushed, PushError> {
        let mut command: Command = push_command(
            Path::new("git"),
            &self.remote.checkout,
            remote,
            commit,
            branch,
        )
        .unwrap();
        self.env.apply(&mut command);
        read_push(branch, &command.output().unwrap())
    }

    fn push_leased(
        &self,
        commit: &CommitId,
        branch: &Branch,
        lease: &CommitId,
    ) -> Result<Pushed, PushError> {
        let mut command: Command = leased_push_command(
            Path::new("git"),
            &self.remote.checkout,
            "origin",
            commit,
            branch,
            lease,
        )
        .unwrap();
        self.env.apply(&mut command);
        read_push(branch, &command.output().unwrap())
    }

    /// A commit on `parent` writing `file`, or an empty one, from a
    /// detached head of the checkout.
    fn commit_on(&self, parent: &str, file: Option<(&str, &str)>, message: &str) -> CommitId {
        let dir = &self.remote.checkout;
        self.env
            .run(dir, &["checkout", "--quiet", "--detach", parent])
            .unwrap();
        match file {
            Some((name, text)) => {
                fs::write(dir.join(name), text).unwrap();
                self.env.run(dir, &["add", "--all"]).unwrap();
                self.env
                    .run(dir, &["commit", "--quiet", "-m", message])
                    .unwrap();
            }
            None => {
                self.env
                    .run(dir, &["commit", "--quiet", "--allow-empty", "-m", message])
                    .unwrap();
            }
        }
        self.head()
    }

    fn remote_branch(&self, branch: &Branch) -> String {
        let out = self
            .env
            .run(&self.remote.bare, &["rev-parse", branch.as_str()])
            .unwrap();
        String::from_utf8(out).unwrap().trim().to_owned()
    }
}

#[test]
fn a_branch_is_created_moved_forward_and_never_rewound() {
    let bench = bench();
    let branch = Branch::new("owl-17-push").unwrap();
    let first = bench.commit("one");
    let second = bench.commit("two");
    // The checkout is at `second`; the push carries `first` only.
    assert_eq!(bench.push("origin", &first, &branch), Ok(Pushed::Created));
    assert_eq!(bench.remote_branch(&branch), first.as_str());
    assert_eq!(bench.push("origin", &first, &branch), Ok(Pushed::UpToDate));
    assert_eq!(
        bench.push("origin", &second, &branch),
        Ok(Pushed::FastForward)
    );
    let rejected = bench.push("origin", &first, &branch).unwrap_err();
    assert!(
        matches!(rejected, PushError::Rejected { .. }),
        "{rejected:?}"
    );
    assert_eq!(bench.remote_branch(&branch), second.as_str());
}

#[test]
fn an_unreachable_remote_is_a_failure_without_a_verdict() {
    let bench = bench();
    let branch = Branch::new("owl-17-push").unwrap();
    let commit = bench.commit("one");
    let error = bench.push("../nowhere.git", &commit, &branch).unwrap_err();
    assert!(matches!(error, PushError::Failed { .. }), "{error:?}");
}

/// A leased push lands while the branch is still at the lease, and is
/// rejected once someone else moved it (OWL-112); the Writer refuses, before
/// git runs, a commit that does not descend from the lease, which the lease
/// alone would let replace the branch's history.
#[test]
fn a_leased_push_moves_the_branch_forward_from_its_lease_only() {
    let bench = bench();
    let branch = Branch::new("owl-112-push").unwrap();
    let first = bench.commit("one");
    let second = bench.commit("two");
    assert_eq!(bench.push("origin", &first, &branch), Ok(Pushed::Created));
    assert_eq!(
        bench.push_leased(&second, &branch, &first),
        Ok(Pushed::FastForward)
    );
    // Someone else pushed `third`: a push leased on `second` is refused.
    let third = bench.commit("three");
    assert_eq!(
        bench.push("origin", &third, &branch),
        Ok(Pushed::FastForward)
    );
    let fourth = bench.commit("four");
    let stale = bench.push_leased(&fourth, &branch, &second).unwrap_err();
    assert!(matches!(stale, PushError::Rejected { .. }), "{stale:?}");
    assert_eq!(bench.remote_branch(&branch), third.as_str());

    // A rewritten history leased on the branch's head: refused before git
    // pushes anything.
    let env = bench.env.clone();
    let git = Git::with_setup("git", move |command| env.apply(command));
    let tracker = MarkdownTracker::new(&bench.remote.checkout);
    let writer = Writer::new(&tracker);
    let refused = writer
        .push_leased(
            &git,
            &bench.remote.checkout,
            "origin",
            &neutral(&git, &bench.remote.checkout, &first),
            &branch,
            &third,
        )
        .unwrap_err();
    assert!(
        matches!(&refused, WriteError::Push(PushError::Failed { message })
            if message.contains("does not descend")),
        "{refused:?}"
    );
    assert_eq!(bench.remote_branch(&branch), third.as_str());
    assert_eq!(
        writer.push_leased(
            &git,
            &bench.remote.checkout,
            "origin",
            &neutral(&git, &bench.remote.checkout, &fourth),
            &branch,
            &third
        ),
        Ok(Pushed::FastForward)
    );
}

/// A rebase run's push (OWL-210): leased on Owlshift's last push, it
/// replaces the branch's history with a rebase of the delivery onto the new
/// base, read as forced. Before git runs, the Writer refuses a commit that
/// is the base, adds nothing to it, does not descend from it, or no longer
/// changes a file the delivery changed; the lease still refuses a branch a
/// person moved or deleted, which is never recreated.
#[test]
fn a_rebased_push_replaces_the_history_only_with_the_delivery_kept() {
    let bench = bench();
    let branch = Branch::new("owl-210-push").unwrap();
    let seed = bench.head();
    let delivered = bench.commit_on(seed.as_str(), Some(("file.txt", "delivered\n")), "deliver");
    assert_eq!(
        bench.push("origin", &delivered, &branch),
        Ok(Pushed::Created)
    );
    // Another merge moved the base.
    let base = bench.commit_on(seed.as_str(), Some(("main.txt", "merged\n")), "merge");
    let env = bench.env.clone();
    let git = Git::with_setup("git", move |command| env.apply(command));
    let tracker = MarkdownTracker::new(&bench.remote.checkout);
    let writer = Writer::new(&tracker);
    let push = |commit: &CommitId, lease: &CommitId| {
        writer.push_rebased(
            &git,
            &bench.remote.checkout,
            "origin",
            &neutral(&git, &bench.remote.checkout, commit),
            &branch,
            lease,
            &base,
            None,
        )
    };
    let lost = bench.commit_on(base.as_str(), Some(("other.txt", "x\n")), "lose it");
    let empty = bench.commit_on(base.as_str(), None, "nothing");
    let elsewhere = bench.commit_on(seed.as_str(), Some(("file.txt", "again\n")), "elsewhere");
    for (commit, why) in [
        (&base, "it is the base itself"),
        (&empty, "its tree is the base's"),
        (&elsewhere, "does not descend from the base"),
        (&lost, "no longer changes file.txt"),
    ] {
        let refused = push(commit, &delivered).unwrap_err();
        assert!(
            matches!(&refused, WriteError::Push(PushError::Failed { message })
                if message.contains(why)),
            "{why}: {refused:?}"
        );
        assert_eq!(bench.remote_branch(&branch), delivered.as_str());
    }

    let rebased = bench.commit_on(base.as_str(), Some(("file.txt", "delivered\n")), "deliver");
    assert_eq!(push(&rebased, &delivered), Ok(Pushed::Forced));
    assert_eq!(bench.remote_branch(&branch), rebased.as_str());

    // A person pushed meanwhile: the lease on the rebase refuses the next.
    let theirs = bench.commit_on(rebased.as_str(), Some(("theirs.txt", "t\n")), "theirs");
    assert_eq!(
        bench.push("origin", &theirs, &branch),
        Ok(Pushed::FastForward)
    );
    let again = bench.commit_on(base.as_str(), Some(("file.txt", "delivered!\n")), "again");
    let stale = push(&again, &rebased).unwrap_err();
    assert!(
        matches!(stale, WriteError::Push(PushError::Rejected { .. })),
        "{stale:?}"
    );
    assert_eq!(bench.remote_branch(&branch), theirs.as_str());

    // A person deleted the branch: it is not recreated.
    bench
        .env
        .run(
            &bench.remote.bare,
            &["update-ref", "-d", &format!("refs/heads/{branch}")],
        )
        .unwrap();
    let gone = push(&again, &rebased).unwrap_err();
    assert!(
        matches!(gone, WriteError::Push(PushError::Rejected { .. })),
        "{gone:?}"
    );
    assert!(
        bench
            .env
            .run(
                &bench.remote.bare,
                &["rev-parse", "--verify", "--quiet", branch.as_str()]
            )
            .is_err()
    );
}

/// A rebase cycle's push (OWL-514): the delivery check reads the commit the
/// rebase run made, and the pushed head need only descend from it. A later
/// commit of the cycle that deletes a delivered file on purpose is pushed
/// (OWL-460); a rebase commit that lost it is still refused with
/// `rebase_keeps_delivery`'s message, naming that commit, and a head off the
/// rebase commit is refused, both before git runs.
#[test]
fn a_rebase_cycles_push_reads_the_delivery_on_the_rebase_commit() {
    let bench = bench();
    let branch = Branch::new("owl-514-push").unwrap();
    let seed = bench.head();
    let delivered = bench.commit_on(seed.as_str(), Some(("file.txt", "delivered\n")), "deliver");
    assert_eq!(
        bench.push("origin", &delivered, &branch),
        Ok(Pushed::Created)
    );
    let base = bench.commit_on(seed.as_str(), Some(("main.txt", "merged\n")), "merge");
    let env = bench.env.clone();
    let git = Git::with_setup("git", move |command| env.apply(command));
    let tracker = MarkdownTracker::new(&bench.remote.checkout);
    let writer = Writer::new(&tracker);
    let push = |commit: &CommitId, rebased: &CommitId| {
        writer.push_rebased(
            &git,
            &bench.remote.checkout,
            "origin",
            &neutral(&git, &bench.remote.checkout, commit),
            &branch,
            &delivered,
            &base,
            Some(rebased),
        )
    };
    let refused = |result: Result<Pushed, WriteError>, why: &str| match result {
        Err(WriteError::Push(PushError::Failed { message })) => {
            assert!(message.contains(why), "{why}: {message}");
            assert_eq!(bench.remote_branch(&branch), delivered.as_str());
        }
        other => panic!("{why}: {other:?}"),
    };

    // The rebase run kept file.txt; the last fix of its cycle deletes it.
    let rebased = bench.commit_on(base.as_str(), Some(("file.txt", "delivered\n")), "rebase");
    let dir = &bench.remote.checkout;
    bench
        .env
        .run(dir, &["checkout", "--quiet", "--detach", rebased.as_str()])
        .unwrap();
    bench.env.run(dir, &["rm", "--quiet", "file.txt"]).unwrap();
    bench
        .env
        .run(dir, &["commit", "--quiet", "-m", "drop it"])
        .unwrap();
    let later = bench.head();

    // A rebase run whose own commit lost file.txt, with a later commit on it.
    let lost = bench.commit_on(base.as_str(), Some(("other.txt", "x\n")), "lose it");
    let after_lost = bench.commit_on(lost.as_str(), Some(("more.txt", "y\n")), "more");
    refused(
        push(&after_lost, &lost),
        &format!(
            "{lost} is not a rebase of {delivered} onto {base}: it no longer changes \
             file.txt, which the delivery changed"
        ),
    );
    // A head that does not descend from the rebase commit.
    let elsewhere = bench.commit_on(base.as_str(), Some(("file.txt", "again\n")), "elsewhere");
    refused(
        push(&elsewhere, &rebased),
        &format!("{elsewhere} does not descend from {rebased}"),
    );

    assert_eq!(push(&later, &rebased), Ok(Pushed::Forced));
    assert_eq!(bench.remote_branch(&branch), later.as_str());
}
