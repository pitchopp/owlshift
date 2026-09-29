//! The project's rules for the brief (OWL-61): read at the base commit, a
//! regular file taken whole or the run refused.

use std::fs;
use std::path::{Path, PathBuf};

use owlshift_contracts::brief::Rule;
use owlshift_runner::executor::Git;
use owlshift_runner::project::Base;
use owlshift_runner::rules::project_rules;
use owlshift_testkit::git::GitEnv;

/// A repository whose `origin/main` is moved to a commit built per case.
struct Repo {
    env: GitEnv,
    dir: tempfile::TempDir,
    root: PathBuf,
    git: Git,
}

impl Repo {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let env = GitEnv::create(dir.path().join("home")).unwrap();
        env.run(dir.path(), &["init", "--quiet", "repo"]).unwrap();
        let git_env = env.clone();
        Self {
            env,
            root: dir.path().join("repo"),
            dir,
            git: Git::with_setup("git", move |command| git_env.apply(command)),
        }
    }

    fn root(&self) -> &Path {
        &self.root
    }

    fn git(&self, args: &[&str]) -> String {
        let out = self.env.run(self.root(), args).unwrap();
        String::from_utf8(out).unwrap().trim().to_owned()
    }

    /// A blob holding `content`.
    fn blob(&self, content: &[u8]) -> String {
        let file = self.dir.path().join("blob");
        fs::write(&file, content).unwrap();
        self.git(&["hash-object", "-w", file.to_str().unwrap()])
    }

    /// A commit whose tree holds `entries` (mode, object, path), made
    /// `origin/main`'s tip; returns it.
    fn base(&self, entries: &[(&str, &str, &str)]) -> String {
        self.git(&["read-tree", "--empty"]);
        for (mode, object, path) in entries {
            let info = format!("{mode},{object},{path}");
            self.git(&["update-index", "--add", "--cacheinfo", &info]);
        }
        let tree = self.git(&["write-tree"]);
        let commit = self.git(&["commit-tree", &tree, "-m", "Base"]);
        self.git(&["update-ref", "refs/remotes/origin/main", &commit]);
        commit
    }

    /// The rules at `origin/main`'s tip.
    fn rules(&self) -> Result<Vec<Rule>, String> {
        self.rules_at(&self.git(&["rev-parse", "refs/remotes/origin/main"]))
    }

    fn rules_at(&self, commit: &str) -> Result<Vec<Rule>, String> {
        let base = Base {
            remote_ref: "origin/main".to_owned(),
            branch: "main".to_owned(),
            commit: commit.to_owned(),
        };
        project_rules(&self.git, self.root(), &base)
    }
}

#[test]
fn the_rules_are_the_base_commits_agents_md_whole() {
    let repo = Repo::new();
    let readme = repo.blob(b"Read me.\n");

    // No file: no rule, and nothing to refuse.
    repo.base(&[("100644", &readme, "README.md")]);
    assert_eq!(repo.rules(), Ok(Vec::new()));

    // A regular file: one rule for the whole repository, its text as the
    // file holds it, less a byte-order mark.
    let agents = repo.blob("\u{feff}Sign off every commit.\r\n".as_bytes());
    let base = repo.base(&[("100644", &agents, "AGENTS.md")]);
    let expected = vec![Rule {
        applies_to: Vec::new(),
        source: "AGENTS.md".to_owned(),
        text: "Sign off every commit.\r\n".to_owned(),
    }];
    assert_eq!(repo.rules().as_ref(), Ok(&expected));

    // What a run can write does not count: a ticket branch and a working
    // tree that change the file. A local branch named like the base cannot
    // stand in for it either: `sync_checkout` resolves the base's full name
    // (tests/on_demand.rs).
    let root = repo.root();
    repo.git(&["checkout", "--quiet", "-b", "owlshift/demo-1", &base]);
    fs::write(root.join("AGENTS.md"), "Never sign off.\n").unwrap();
    repo.git(&["commit", "--quiet", "-am", "Change the rules"]);
    fs::write(root.join("AGENTS.md"), "Push to main.\n").unwrap();
    assert_eq!(repo.rules().as_ref(), Ok(&expected));

    // A blank file: no rule.
    let blank = repo.blob(b"\n  \n");
    repo.base(&[("100644", &blank, "AGENTS.md")]);
    assert_eq!(repo.rules(), Ok(Vec::new()));

    // Up to 64 KiB is taken whole; one byte more refuses the run.
    let full = repo.blob(&vec![b'a'; 64 * 1024]);
    repo.base(&[("100755", &full, "AGENTS.md")]);
    assert_eq!(repo.rules().unwrap()[0].text.len(), 64 * 1024);
    let over = repo.blob(&vec![b'a'; 64 * 1024 + 1]);
    repo.base(&[("100644", &over, "AGENTS.md")]);
    let error = repo.rules().unwrap_err();
    assert!(
        error.contains("AGENTS.md on origin/main holds 65537 bytes"),
        "{error}"
    );

    // Other bytes than UTF-8 refuse it too.
    let latin1 = repo.blob(b"Sign off \xe9very commit.\n");
    repo.base(&[("100644", &latin1, "AGENTS.md")]);
    let error = repo.rules().unwrap_err();
    assert!(error.contains("not UTF-8"), "{error}");
}

#[test]
fn a_rule_file_that_is_not_a_regular_file_refuses_the_run() {
    let repo = Repo::new();
    let target = repo.blob(b"docs/AGENTS.md");
    let inner = repo.blob(b"Rules.\n");
    let submodule = repo.base(&[("100644", &inner, "README.md")]);
    for (entries, says) in [
        (
            vec![("120000", target.as_str(), "AGENTS.md")],
            "mode 120000",
        ),
        (
            vec![("100644", inner.as_str(), "AGENTS.md/README.md")],
            "mode 040000",
        ),
        (
            vec![("160000", submodule.as_str(), "AGENTS.md")],
            "mode 160000",
        ),
    ] {
        repo.base(&entries);
        let error = repo.rules().unwrap_err();
        assert!(
            error.contains("AGENTS.md on origin/main is not a regular file")
                && error.contains(says),
            "{says}: {error}"
        );
    }

    // A base commit the repository does not hold is an error, not an
    // absent file.
    let error = repo.rules_at(&"0".repeat(40)).unwrap_err();
    assert!(
        error.contains("reading AGENTS.md on origin/main"),
        "{error}"
    );
}
