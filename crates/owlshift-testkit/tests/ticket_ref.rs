//! The ticket ref (OWL-122): what the runner keeps about a ticket between
//! `owlshift do` and `owlshift continue`, as a commit in the dedicated
//! checkout, read back whole or refused.

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use owlshift_adapters::forge::Repo as ForgeRepo;
use owlshift_contracts::event::EventKind;
use owlshift_contracts::ids::TicketId;
use owlshift_contracts::refs::{PersistedState, TicketQuestions};
use owlshift_runner::events::{EventLog, EventSink};
use owlshift_runner::executor::Git;
use owlshift_runner::forget;
use owlshift_runner::on_demand::Stop;
use owlshift_runner::project::ProjectDirs;
use owlshift_runner::ticket_ref::{self, Forgot, Previous, TicketRecord};
use owlshift_testkit::git::GitEnv;
use serde_json::json;

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
        r#"{{"format":2,"asks":[{{"kind":"questions","round":{round},"at":"2026-10-02T09:00:00Z",
        "comment":"c1","questions":[{{"id":"Q1","category":"scope","context":"c","text":"t"}}],
        "decider":{{"account":"maintainer","by":"assignee"}}}}]}}"#
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

/// A waiting record whose `questions.json` renders to `len` bytes, its
/// question's context padded.
fn record_of(len: usize) -> TicketRecord {
    let with = |context: usize| {
        let questions = questions(1).replace(
            r#""context":"c""#,
            &format!(r#""context":"{}""#, "c".repeat(context)),
        );
        TicketRecord {
            state: PersistedState::parse(WAITING).unwrap(),
            questions: TicketQuestions::parse(&questions).unwrap(),
        }
    };
    let base = with(1).questions.render().len();
    let record = with(1 + len - base);
    assert_eq!(record.questions.render().len(), len);
    record
}

#[test]
fn a_ticket_ref_file_past_the_read_cap_is_never_written() {
    let repo = Repo::new();
    let cap = usize::try_from(ticket_ref::MAX_FILE).unwrap();
    let over = record_of(cap + 1);

    // Refused before any git command, with no ref yet: none is made.
    let calls = Arc::new(AtomicUsize::new(0));
    let counted = {
        let (env, calls) = (repo.env.clone(), Arc::clone(&calls));
        Git::with_setup("git", move |command| {
            calls.fetch_add(1, Ordering::SeqCst);
            env.apply(command);
        })
    };
    let error = ticket_ref::write(&counted, &repo.root, &ticket(), &over, None).unwrap_err();
    assert!(
        error.contains(&format!(
            "would hold questions.json of {} bytes, past the {cap}",
            cap + 1
        )) && error.contains("nothing is written")
            && error.contains("`owlshift forget DEMO-1` makes room"),
        "{error}"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(repo.read().unwrap(), None);

    // A file of exactly the cap is written and read back.
    let at_cap = record_of(cap);
    let kept = ticket_ref::write(&repo.git, &repo.root, &ticket(), &at_cap, None).unwrap();
    assert_eq!(repo.read().unwrap().unwrap().record, at_cap);

    // One byte more over an existing ref: the ref keeps its previous state.
    let error = ticket_ref::write(&repo.git, &repo.root, &ticket(), &over, Some(&kept));
    assert!(error.is_err(), "{error:?}");
    let stored = repo.read().unwrap().unwrap();
    assert_eq!((stored.commit, stored.record), (kept, at_cap));
}

/// A ticket waiting for the answers of round 1, re-asked once, whose
/// `questions.json` keeps every part: asks with verdicts and a refused
/// check, what the last check read, a decision, Build's and the resolver's
/// refusals.
fn kept_everything() -> TicketRecord {
    TicketRecord {
        state: PersistedState::parse(
            r#"{"format":2,"stage":"build","waiting":"needs_input","round":1,"reasks":1,"failed_runs":1}"#,
        )
        .unwrap(),
        questions: TicketQuestions::parse(include_str!(
            "../../owlshift-contracts/tests/fixtures/ticket-questions.json"
        ))
        .unwrap(),
    }
}

/// What forgetting a record of `round` whose questions are `questions`
/// leaves (OWL-202).
fn forgotten(round: u32, questions: &TicketQuestions) -> TicketRecord {
    TicketRecord {
        state: PersistedState::parse(&format!(
            r#"{{"format":2,"stage":"ready","round":{round},"reasks":0,"failed_runs":0}}"#
        ))
        .unwrap(),
        questions: TicketQuestions {
            build_refusal: questions.build_refusal.clone(),
            resolver_refusal: questions.resolver_refusal.clone(),
            ..TicketQuestions::new()
        },
    }
}

/// OWL-202: `owlshift forget` writes a new record on top of the ticket ref,
/// the old one its parent: Ready, the round count kept, and only the
/// refusals the next runs are told; a ref holding just that is left alone.
#[test]
fn forget_keeps_the_round_count_and_the_refusals_and_drops_the_rest() {
    let repo = Repo::new();
    assert_eq!(
        ticket_ref::forget(&repo.git, &repo.root, &ticket()).unwrap(),
        None
    );
    let waiting = kept_everything();
    let old = ticket_ref::write(&repo.git, &repo.root, &ticket(), &waiting, None).unwrap();

    let Some(Forgot::Rewritten {
        previous,
        commit,
        from,
        kept,
    }) = ticket_ref::forget(&repo.git, &repo.root, &ticket()).unwrap()
    else {
        panic!("not rewritten");
    };
    assert_eq!(
        (previous.as_str(), from, &kept),
        (
            old.as_str(),
            Previous::Read(waiting.clone()),
            &forgotten(1, &waiting.questions)
        )
    );
    assert!(kept.questions.build_refusal.is_some() && kept.questions.resolver_refusal.is_some());
    let stored = repo.read().unwrap().expect("the ref");
    assert_eq!((&stored.commit, &stored.record), (&commit, &kept));
    // The previous record stays whole, as the new one's parent.
    assert_eq!(repo.git(&["rev-parse", &format!("{commit}^")]), old);
    let parent = repo.git(&["show", &format!("{old}:questions.json")]);
    assert_eq!(TicketQuestions::parse(&parent).unwrap(), waiting.questions);

    // Ready with its refusals and no ask: nothing more to forget.
    assert_eq!(
        ticket_ref::forget(&repo.git, &repo.root, &ticket()).unwrap(),
        Some(Forgot::Unchanged {
            commit: commit.clone(),
            kept
        })
    );
    assert_eq!(repo.read().unwrap().unwrap().commit, commit);
}

/// OWL-202: a ticket ref that cannot be read gets a record of what still
/// reads on its own, on top of it; a symbolic ref, or one that is no
/// commit, is deleted, and nothing else; a git command that fails changes
/// nothing.
#[test]
fn forget_salvages_a_broken_record_and_deletes_a_ref_that_is_no_commit() {
    let repo = Repo::new();
    let forget = || ticket_ref::forget(&repo.git, &repo.root, &ticket());
    let cap = usize::try_from(ticket_ref::MAX_FILE).unwrap();

    // A `questions.json` past the cap, planted by hand: the round of its
    // `state.json` is kept.
    let state = repo.blob(r#"{"format":2,"stage":"build","round":3,"reasks":0,"failed_runs":0}"#);
    let huge = repo.blob(&"x".repeat(cap + 1));
    repo.plant(&[
        ("100644", &state, "state.json"),
        ("100644", &huge, "questions.json"),
    ]);
    let planted = repo.git(&["rev-parse", REF]);
    let Some(Forgot::Rewritten {
        previous,
        from: Previous::Unreadable(why),
        kept,
        commit,
    }) = forget().unwrap()
    else {
        panic!("not salvaged");
    };
    assert!(
        why.contains(&format!("holds questions.json past {cap} bytes")),
        "{why}"
    );
    assert_eq!(
        (previous.as_str(), &kept),
        (planted.as_str(), &forgotten(3, &TicketQuestions::new()))
    );
    assert_eq!(repo.read().unwrap().unwrap().commit, commit);

    // A `state.json` that does not parse, beside a `questions.json` that
    // does: round 0, the refusals kept.
    let everything = kept_everything().questions;
    let questions = repo.blob(&everything.render());
    let broken = repo.blob("{}");
    repo.plant(&[
        ("100644", &broken, "state.json"),
        ("100644", &questions, "questions.json"),
    ]);
    let Some(Forgot::Rewritten { kept, .. }) = forget().unwrap() else {
        panic!("not salvaged");
    };
    assert_eq!(kept, forgotten(0, &everything));

    // A symbolic ref goes, and the ref it points to stays.
    let other = TicketId::new("DEMO-2").unwrap();
    let theirs = ticket_ref::write(&repo.git, &repo.root, &other, &record(WAITING, 1), None).unwrap();
    repo.git(&["symbolic-ref", REF, "refs/owlshift/tickets/DEMO-2"]);
    let error = repo.read().unwrap_err();
    assert!(error.contains("is a symbolic ref to refs/owlshift/tickets/DEMO-2"), "{error}");
    let Some(Forgot::Deleted { previous, why }) = forget().unwrap() else {
        panic!("not deleted");
    };
    assert_eq!(previous, theirs);
    assert!(why.contains("is a symbolic ref"), "{why}");
    assert_eq!(repo.read().unwrap(), None);
    let still = ticket_ref::read(&repo.git, &repo.root, &other).unwrap().unwrap();
    assert_eq!(still.commit, theirs);

    // A ref to a blob goes. Checked on 2026-10-10 with git 2.43.0: `git
    // update-ref --no-deref -d` from an old value the ref no longer holds is
    // refused and leaves the ref, so a ref moved since it was listed is
    // never deleted.
    repo.git(&["update-ref", REF, &state]);
    let stale = repo.env.run(&repo.root, &["update-ref", "--no-deref", "-d", REF, &huge]);
    assert!(stale.is_err(), "{stale:?}");
    assert_eq!(repo.git(&["rev-parse", REF]), state);
    let Some(Forgot::Deleted { previous, why }) = forget().unwrap() else {
        panic!("not deleted");
    };
    assert_eq!(previous, state);
    assert!(why.contains("points to a blob, not a commit"), "{why}");
    assert_eq!(repo.read().unwrap(), None);

    // A record git cannot show, its tree gone from the object store, is
    // no broken record: forget fails and the ref stays.
    let written = ticket_ref::write(&repo.git, &repo.root, &ticket(), &record(WAITING, 1), None).unwrap();
    let tree = repo.git(&["rev-parse", &format!("{written}^{{tree}}")]);
    fs::remove_file(repo.root.join(".git/objects").join(&tree[..2]).join(&tree[2..])).unwrap();
    assert!(forget().is_err());
    assert_eq!(repo.git(&["rev-parse", REF]), written);
}

/// OWL-202: `owlshift forget` holds the project's lock, as `do`, `continue`
/// and `watch` do, says what it dropped and kept, and records it.
#[test]
fn forget_holds_the_project_lock_and_says_what_it_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let env = GitEnv::create(dir.path().join("home")).unwrap();
    let data = dir.path().join("data");
    let dirs = ProjectDirs::new(&data, &ForgeRepo::parse("demo/project").unwrap());
    let git_env = env.clone();
    let git = Git::with_setup("git", move |command| git_env.apply(command));
    let mut out = Vec::new();
    let mut sink = EventSink::new("demo/project", EventLog::in_dir(&data), &mut out);

    // No dedicated checkout: nothing to forget, and no project directory.
    let stop = forget::forget(&git, &dirs, &ticket(), &mut sink).unwrap_err();
    assert_eq!(
        stop.to_string(),
        "Not run: nothing to forget: Owlshift keeps nothing on DEMO-1"
    );
    assert!(!dirs.root().exists());

    fs::create_dir_all(dirs.root()).unwrap();
    env.run(dirs.root(), &["init", "--quiet", "checkout"]).unwrap();
    let checkout = dirs.checkout();
    let waiting = kept_everything();
    let old = ticket_ref::write(&git, &checkout, &ticket(), &waiting, None).unwrap();
    let forgotten = forget::forget(&git, &dirs, &ticket(), &mut sink).unwrap();
    let Forgot::Rewritten { commit, .. } = &forgotten.forgot else {
        panic!("{forgotten:?}");
    };
    let said = forgotten.to_string();
    for part in [
        "Forgot what Owlshift kept on DEMO-1: dropped 2 asks of round 1 with their verdicts, 1 \
         decision of the resolver, what the last answer check read and its state, waiting for \
         the answers of round 1: those questions are abandoned",
        "; kept round 1 (the next round is 2), Build's held refusal and the resolver's refusal.",
        &format!("refs/owlshift/tickets/DEMO-1 now points to {commit}, whose parent {old}"),
        "its stage on the tracker moves at the next `owlshift do DEMO-1`",
    ] {
        assert!(said.contains(part), "{part}\n{said}");
    }
    let printed = String::from_utf8(out).unwrap();
    assert!(printed.contains(" DEMO-1 decision "), "{printed}");
    let events = fs::read_to_string(EventLog::in_dir(&data).path()).unwrap();
    let event = owlshift_contracts::event::Event::parse(events.lines().last().unwrap()).unwrap();
    assert_eq!(event.kind, EventKind::Decision);
    assert_eq!(
        serde_json::to_value(&event.data).unwrap(),
        json!({
            "forgotten": "rewritten", "previous": old, "commit": commit,
            "round": 1, "asks": 2, "decisions": 1,
        })
    );

    // Another command holds the project: refused, the ref as it was. A
    // child another test's thread forked may hold the lock file a moment
    // after the command above let it go (`ProjectDirs::lock`).
    let held = (0..100)
        .find_map(|_| {
            let lock = dirs.lock().unwrap();
            if lock.is_none() {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            lock
        })
        .expect("the project's lock");
    let mut out = Vec::new();
    let mut sink = EventSink::new("demo/project", EventLog::in_dir(&data), &mut out);
    let stop = forget::forget(&git, &dirs, &ticket(), &mut sink).unwrap_err();
    assert!(matches!(stop, Stop::Busy(_)), "{stop:?}");
    drop(held);
    assert_eq!(
        &ticket_ref::read(&git, &checkout, &ticket())
            .unwrap()
            .unwrap()
            .commit,
        commit
    );
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
