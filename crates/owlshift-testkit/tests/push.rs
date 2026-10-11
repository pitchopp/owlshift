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
use owlshift_runner::writer::{WriteError, Writer};
use owlshift_testkit::git::{GitEnv, Remote, seed};

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
            &first,
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
            &fourth,
            &branch,
            &third
        ),
        Ok(Pushed::FastForward)
    );
}
