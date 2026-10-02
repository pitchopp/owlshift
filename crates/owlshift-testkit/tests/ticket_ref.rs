//! The ticket ref (OWL-122): what the runner keeps about a ticket between
//! `owlshift do` and `owlshift resume`, as a commit in the dedicated
//! checkout, read back whole or refused.

use std::fs;
use std::path::PathBuf;

use owlshift_contracts::ids::TicketId;
use owlshift_contracts::refs::{PersistedState, TicketQuestions};
use owlshift_runner::executor::Git;
use owlshift_runner::ticket_ref::{self, TicketRecord};
use owlshift_testkit::git::GitEnv;

const REF: &str = "refs/owlshift/tickets/DEMO-1";

struct Repo {
    _dir: tempfile::TempDir,
    env: GitEnv,
    root: PathBuf,
    git: Git,
}

impl Repo {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let env = GitEnv::create(dir.path().join("home")).unwrap();
        env.run(dir.path(), &["init", "--quiet", "repo"]).unwrap();
        let root = dir.path().join("repo");
        let git_env = env.clone();
        Self {
            _dir: dir,
            env,
            root,
            git: Git::with_setup("git", move |command| git_env.apply(command)),
        }
    }

    fn git(&self, args: &[&str]) -> String {
        let out = self.env.run(&self.root, args).unwrap();
        String::from_utf8(out).unwrap().trim().to_owned()
    }

    /// A blob holding `content`.
    fn blob(&self, content: &str) -> String {
        let file = self.root.join(".git").join("blob");
        fs::write(&file, content).unwrap();
        self.git(&["hash-object", "-w", file.to_str().unwrap()])
    }

    /// Points the ticket ref at a commit whose tree holds `entries` (mode,
    /// object, name).
    /// The tree is built in the index, so a symbolic link entry needs no
    /// link on disk.
    fn plant(&self, entries: &[(&str, &str, &str)]) {
        self.git(&["read-tree", "--empty"]);
        for (mode, oid, name) in entries {
            let info = format!("{mode},{oid},{name}");
            self.git(&["update-index", "--add", "--cacheinfo", &info]);
        }
        let tree = self.git(&["write-tree"]);
        let commit = self.git(&["commit-tree", "--no-gpg-sign", &tree, "-m", "planted"]);
        self.git(&["update-ref", REF, &commit]);
    }

    fn read(&self) -> Result<Option<ticket_ref::Stored>, String> {
        ticket_ref::read(&self.git, &self.root, &ticket())
    }
}

fn ticket() -> TicketId {
    TicketId::new("DEMO-1").unwrap()
}

/// A tree entry: mode, object, name.
type Entry<'a> = (&'a str, &'a str, &'a str);

const WAITING: &str =
    r#"{"format":2,"stage":"build","waiting":"needs_input","round":1,"reasks":0,"failed_runs":0}"#;

fn questions(round: u32) -> String {
    format!(
        r#"{{"format":1,"asks":[{{"kind":"questions","round":{round},"at":"2026-10-02T09:00:00Z",
        "comment":"c1","questions":[{{"id":"Q1","category":"scope","context":"c","text":"t"}}]}}]}}"#
    )
}

fn record(state: &str, round: u32) -> TicketRecord {
    TicketRecord {
        state: PersistedState::parse(state).unwrap(),
        questions: TicketQuestions::parse(&questions(round)).unwrap(),
    }
}

#[test]
fn a_ticket_ref_round_trips_keeps_other_files_and_moves_only_from_where_it_was_read() {
    let repo = Repo::new();
    assert_eq!(repo.read().unwrap(), None);

    let first = record(WAITING, 1);
    let one = ticket_ref::write(&repo.git, &repo.root, &ticket(), &first, None).unwrap();
    let stored = repo.read().unwrap().expect("the ref");
    assert_eq!(
        (&stored.record, stored.commit.as_str()),
        (&first, one.as_str())
    );

    // A later step's file in the same tree is kept by the next write, whose
    // parent is the commit read.
    let plan = repo.blob("1. Plan.\n");
    let entries = repo.git(&["ls-tree", &one]);
    let mut planted: Vec<(String, String, String)> = entries
        .lines()
        .map(|line| {
            let (meta, name) = line.split_once('\t').unwrap();
            let fields: Vec<&str> = meta.split(' ').collect();
            (fields[0].to_owned(), fields[2].to_owned(), name.to_owned())
        })
        .collect();
    planted.push(("100644".to_owned(), plan.clone(), "plan.md".to_owned()));
    let planted: Vec<(&str, &str, &str)> = planted
        .iter()
        .map(|(m, o, n)| (m.as_str(), o.as_str(), n.as_str()))
        .collect();
    repo.plant(&planted);
    let with_plan = repo.read().unwrap().expect("the ref").commit;

    let resumed = r#"{"format":2,"stage":"build","round":1,"reasks":0,"failed_runs":0}"#;
    let second = record(resumed, 1);
    let two =
        ticket_ref::write(&repo.git, &repo.root, &ticket(), &second, Some(&with_plan)).unwrap();
    assert_eq!(repo.read().unwrap().unwrap().record, second);
    assert_eq!(repo.git(&["rev-parse", &format!("{two}^")]), with_plan);
    assert_eq!(repo.git(&["rev-parse", &format!("{two}:plan.md")]), plan);

    // A ref moved since it was read is not overwritten.
    let stale = ticket_ref::write(&repo.git, &repo.root, &ticket(), &first, Some(&one));
    assert!(stale.is_err(), "{stale:?}");
    assert_eq!(repo.read().unwrap().unwrap().commit, two);
}

#[test]
fn a_broken_ticket_ref_is_refused() {
    let repo = Repo::new();
    let state = repo.blob(WAITING);
    let asked = repo.blob(&questions(1));
    let later = repo.blob(&questions(2));
    let cases: Vec<(Vec<Entry>, &str)> = vec![
        (
            vec![("100644", &state, "state.json")],
            "has no questions.json",
        ),
        (
            vec![
                ("120000", &state, "state.json"),
                ("100644", &asked, "questions.json"),
            ],
            "holds state.json as 120000 blob, not a regular file",
        ),
        (
            vec![
                ("100644", &asked, "state.json"),
                ("100644", &asked, "questions.json"),
            ],
            "holds an invalid state.json",
        ),
        (
            vec![
                ("100644", &state, "state.json"),
                ("100644", &later, "questions.json"),
            ],
            "waits for the answers of round 1, but its latest ask is of round 2",
        ),
    ];
    for (entries, needle) in cases {
        repo.plant(&entries);
        let error = repo.read().unwrap_err();
        assert!(error.contains(needle), "{needle}: {error}");
    }

    // A ref that names no commit.
    repo.git(&["update-ref", REF, &state]);
    let error = repo.read().unwrap_err();
    assert!(error.contains("points to a blob, not a commit"), "{error}");
}
