//! The project's rules for the brief (OWL-61, OWL-67): the files
//! `stack.rules` names, by default `AGENTS.md`, read at the base commit, each
//! a regular file taken whole or the run refused.

use std::fs;
use std::path::{Path, PathBuf};

use owlshift_contracts::brief::Rule;
use owlshift_contracts::config::ProjectConfig;
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

    /// The rules at `origin/main`'s tip, for a project that names no rule
    /// file.
    fn rules(&self) -> Result<Vec<Rule>, String> {
        self.rules_named("")
    }

    /// The rules at `origin/main`'s tip, for an `owlshift.toml` whose
    /// `[stack]` also holds `stack` (`rules = [...]`, or nothing).
    fn rules_named(&self, stack: &str) -> Result<Vec<Rule>, String> {
        let tip = self.git(&["rev-parse", "refs/remotes/origin/main"]);
        self.rules_at(&tip, stack)
    }

    fn rules_at(&self, commit: &str, stack: &str) -> Result<Vec<Rule>, String> {
        let base = Base {
            remote_ref: "origin/main".to_owned(),
            branch: "main".to_owned(),
            commit: commit.to_owned(),
        };
        let config = ProjectConfig::parse(&format!(
            r#"
            requires = ">=0.1"
            [tracker]
            kind = "markdown"
            admit = {{ label = "owlshift" }}
            states = {{ ready = "Todo", working = "Doing", needs_input = "Asked", review = "Review" }}
            [stack]
            gate = ["make test"]
            {stack}
            [pipeline]
            default = "trivial"
            plan_approval = "never"
            [models]
            [policy]
            always_human = []
            "#
        ))
        .unwrap();
        project_rules(&self.git, self.root(), &base, &config.stack)
    }
}

fn rule(source: &str, text: &str) -> Rule {
    Rule {
        applies_to: Vec::new(),
        source: source.to_owned(),
        text: text.to_owned(),
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
    let error = repo.rules_at(&"0".repeat(40), "").unwrap_err();
    assert!(
        error.contains("reading AGENTS.md on origin/main"),
        "{error}"
    );
}

#[test]
fn a_project_names_its_rule_files() {
    let repo = Repo::new();
    let agents = repo.blob(b"Sign off every commit.\n");
    let testing = repo.blob(b"Run the whole suite.\n");
    let deep = repo.blob(b"Keep the core pure.\n");
    let blank = repo.blob(b"\n");
    let base = repo.base(&[
        ("100644", &agents, "AGENTS.md"),
        ("100644", &testing, ".claude/rules/testing.md"),
        ("100755", &deep, "docs/deep/conventions.md"),
        ("100644", &blank, "blank.md"),
    ]);

    // One rule per named file, in the order named; a blank file gives none.
    let named = r#"rules = [".claude/rules/testing.md", "blank.md", "docs/deep/conventions.md", "AGENTS.md"]"#;
    let expected = vec![
        rule(".claude/rules/testing.md", "Run the whole suite.\n"),
        rule("docs/deep/conventions.md", "Keep the core pure.\n"),
        rule("AGENTS.md", "Sign off every commit.\n"),
    ];
    assert_eq!(repo.rules_named(named).as_ref(), Ok(&expected));

    // A ticket branch that changes a named file does not count.
    // The first checkout keeps the index `base` left and writes no file.
    repo.git(&["checkout", "--quiet", "-b", "owlshift/demo-1", &base]);
    let conventions = repo.root().join("docs/deep/conventions.md");
    fs::create_dir_all(conventions.parent().unwrap()).unwrap();
    fs::write(&conventions, "Do I/O anywhere.\n").unwrap();
    repo.git(&["commit", "--quiet", "-am", "Change the rules"]);
    assert_eq!(repo.rules_named(named).as_ref(), Ok(&expected));

    // `[]` gives no rule, not the default.
    assert_eq!(repo.rules_named("rules = []"), Ok(Vec::new()));
    // A project that names none keeps the root AGENTS.md.
    assert_eq!(
        repo.rules(),
        Ok(vec![rule("AGENTS.md", "Sign off every commit.\n")])
    );
}

#[test]
fn a_named_rule_file_is_never_skipped() {
    let repo = Repo::new();
    let text = repo.blob(b"Rules.\n");
    let link = repo.blob(b"docs");
    let module = repo.base(&[("100644", &text, "README.md")]);
    repo.base(&[
        ("100644", &text, "docs/rules.md"),
        ("120000", &link, "linked"),
        ("160000", &module, "vendor"),
    ]);

    // Missing, under a symbolic link to a folder that holds it, or inside a
    // submodule: the base commit has no entry for it.
    for path in ["AGENTS.md", "linked/rules.md", "vendor/README.md"] {
        let error = repo
            .rules_named(&format!("rules = [\"docs/rules.md\", \"{path}\"]"))
            .unwrap_err();
        assert!(
            error.starts_with(&format!("{path} on origin/main does not exist")),
            "{path}: {error}"
        );
    }

    // A folder is not a file.
    let error = repo.rules_named(r#"rules = ["docs"]"#).unwrap_err();
    assert!(
        error.contains("docs on origin/main is not a regular file (mode 040000"),
        "{error}"
    );
}

#[test]
fn the_rules_take_64_kib_together() {
    let repo = Repo::new();
    let half = repo.blob(&vec![b'a'; 32 * 1024]);
    let full = repo.blob(&vec![b'a'; 64 * 1024]);
    let byte = repo.blob(b"b");
    repo.base(&[
        ("100644", &half, "one.md"),
        ("100644", &half, "two.md"),
        ("100644", &full, "full.md"),
        ("100644", &byte, "byte.md"),
    ]);

    let rules = repo.rules_named(r#"rules = ["one.md", "two.md"]"#).unwrap();
    assert_eq!(rules.iter().map(|r| r.text.len()).sum::<usize>(), 64 * 1024);

    let error = repo
        .rules_named(r#"rules = ["full.md", "byte.md"]"#)
        .unwrap_err();
    assert!(
        error.starts_with(
            "the rule files on origin/main hold 65537 bytes together (full.md 65536, byte.md 1)"
        ),
        "{error}"
    );
}
