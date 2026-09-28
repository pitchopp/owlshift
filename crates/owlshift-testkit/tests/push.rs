//! The forge adapter's push against a local bare remote, with hermetic git:
//! the outcome is read from the exit status and the porcelain flag.

use std::fs;
use std::path::Path;
use std::process::Command;

use owlshift_adapters::forge::push::{PushError, Pushed, push_command, read_push};
use owlshift_adapters::forge::{Branch, CommitId};
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
    assert_eq!(bench.push("origin", &second, &branch), Ok(Pushed::FastForward));
    let rejected = bench.push("origin", &first, &branch).unwrap_err();
    assert!(matches!(rejected, PushError::Rejected { .. }), "{rejected:?}");
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
