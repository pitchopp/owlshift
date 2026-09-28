//! `owlshift do` end to end (OWL-20): the runner's `on_demand` run on the
//! Markdown tracker, the fake harness, hermetic git with a local bare remote
//! standing in for GitHub's git side, and the real GitHub adapter over a fake
//! transport standing in for its API.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tempfile::TempDir;

use owlshift_adapters::forge::Repo;
use owlshift_adapters::forge::github::GitHubForge;
use owlshift_adapters::graphql::{Response, Transport};
use owlshift_adapters::tracker::markdown::MarkdownTracker;
use owlshift_contracts::brief::Brief;
use owlshift_contracts::config::ProjectConfig;
use owlshift_contracts::event::{Event, EventKind};
use owlshift_contracts::ids::{RelativePath, TicketId};
use owlshift_core::state::ParkReason;
use owlshift_runner::agent_env::AgentEnv;
use owlshift_runner::events::{EventLog, EventSink};
use owlshift_runner::executor::{
    Git, Harness, HarnessEnd, HarnessError, HarnessRun, RUN_DIR, RunLog,
};
use owlshift_runner::on_demand::{self, Delivered, OnDemand, Stop};
use owlshift_runner::project::ProjectDirs;
use owlshift_testkit::git::{GitEnv, Remote, seed};
use owlshift_testkit::harness::FakeHarness;
use owlshift_testkit::reply::Reply;

const TICKET: &str = "DEMO-1";
const BRANCH: &str = "owlshift/demo-1";
const REPO: &str = "demo/project";

/// GitHub's API as the adapter sees it: at most one open pull request,
/// whose head is the ticket branch's tip on the bare remote, unless
/// `stale_reads` says to report an older head for that many reads.
struct FakeGitHub {
    env: GitEnv,
    bare: PathBuf,
    open: Mutex<Option<u64>>,
    created: Mutex<Vec<Value>>,
    stale_reads: Mutex<u32>,
}

impl FakeGitHub {
    fn tip(&self) -> String {
        self.env
            .run(&self.bare, &["rev-parse", &format!("refs/heads/{BRANCH}")])
            .map(|out| String::from_utf8(out).unwrap().trim().to_owned())
            .unwrap_or_else(|_| "0".repeat(40))
    }

    /// The open pull request, its head as GitHub reports it: an older commit
    /// while `stale_reads` lasts.
    fn pull_request(&self) -> Value {
        let mut stale = self.stale_reads.lock().unwrap();
        let head = if *stale > 0 {
            *stale -= 1;
            "f".repeat(40)
        } else {
            self.tip()
        };
        json!({
            "number": 1, "url": "https://github.com/demo/project/pull/1", "state": "OPEN",
            "headRefOid": head, "headRepository": { "nameWithOwner": REPO },
        })
    }

    fn answer(&self, request: &Value) -> Value {
        let query = request["query"].as_str().unwrap();
        if query.starts_with("query OpenPullRequests") {
            let open = self.open.lock().unwrap().is_some();
            let nodes = if open {
                vec![self.pull_request()]
            } else {
                vec![]
            };
            json!({ "repository": { "pullRequests": { "totalCount": nodes.len(), "nodes": nodes } } })
        } else if query.starts_with("query RepositoryId") {
            json!({ "repository": { "id": "R_1" } })
        } else if query.starts_with("mutation OpenPullRequest") {
            self.created
                .lock()
                .unwrap()
                .push(request["variables"]["input"].clone());
            *self.open.lock().unwrap() = Some(1);
            json!({ "createPullRequest": { "pullRequest": self.pull_request() } })
        } else if query.starts_with("query Checks") {
            let head = self.tip();
            json!({ "repository": { "pullRequest": {
                "state": "OPEN", "headRefOid": head, "mergeable": "MERGEABLE",
                "mergeStateStatus": "CLEAN",
                "commits": { "nodes": [{ "commit": { "oid": head, "statusCheckRollup": {
                    "contexts": {
                        "totalCount": 1,
                        "pageInfo": { "hasNextPage": false, "endCursor": null },
                        "nodes": [{
                            "__typename": "CheckRun", "name": "ci", "status": "COMPLETED",
                            "conclusion": "SUCCESS", "isRequired": true,
                            "detailsUrl": "https://github.com/demo/project/runs/1",
                            "checkSuite": null,
                        }],
                    },
                } } }] },
            } } })
        } else {
            panic!("unexpected GitHub request: {query}")
        }
    }
}

struct Shared(Arc<FakeGitHub>);

impl Transport for Shared {
    fn send(&self, request: &Value) -> Result<Response, String> {
        let data = self.0.answer(request);
        Ok(Response {
            status: 200,
            body: json!({ "data": data }).to_string(),
        })
    }
}

/// The fake harness, one reply per run in order. With `leave_plan`, the
/// first run also leaves a plan and a ledger, as the build role does.
struct Replies {
    program: PathBuf,
    replies: RefCell<VecDeque<PathBuf>>,
    current: RefCell<Option<FakeHarness>>,
    leave_plan: Cell<bool>,
}

impl Harness for Replies {
    fn command(&self, run: &HarnessRun<'_>) -> Result<Command, HarnessError> {
        if self.leave_plan.replace(false) {
            let dir = run.worktree.join(RUN_DIR);
            fs::write(dir.join("plan.md"), "1. Add the greeting.\n")?;
            fs::write(dir.join("ledger.json"), "{\"steps\":[]}\n")?;
        }
        let reply = self
            .replies
            .borrow_mut()
            .pop_front()
            .expect("a reply for every run");
        let harness = FakeHarness {
            program: self.program.clone(),
            reply,
        };
        let command = harness.command(run);
        *self.current.borrow_mut() = Some(harness);
        command
    }

    fn drive(
        &self,
        run: &HarnessRun<'_>,
        child: &mut Child,
        log: &mut RunLog,
    ) -> io::Result<HarnessEnd> {
        let current = self.current.borrow();
        current
            .as_ref()
            .expect("a command first")
            .drive(run, child, log)
    }
}

/// A project seeded into a bare remote, with the person's checkout, and
/// Owlshift's data directory.
struct Bench {
    tmp: TempDir,
    env: GitEnv,
    remote: Remote,
    data: PathBuf,
    github: Arc<FakeGitHub>,
}

const DONE: &str = r#"{"format":1,"status":"done","summary":"Added GREETING.md; the gate passes.",
"decisions":[{"question":"Tone","decision":"Friendly","basis":"The ticket"}],
"pr":{"branch":"owlshift/demo-1","title":"Add a greeting","body":"Says hello. Gate: green."}}"#;

impl Bench {
    fn new(assignee: bool) -> Self {
        let tmp = tempfile::Builder::new()
            .prefix("owlshift do ")
            .tempdir()
            .unwrap();
        let project = tmp.path().join("project");
        let ticket = project.join("tickets").join(TICKET);
        fs::create_dir_all(&ticket).unwrap();
        fs::write(
            project.join("owlshift.toml"),
            "requires = \">=0.0\"\n[tracker]\nkind = \"markdown\"\nadmit = \"delegation\"\n\
             states = { ready = \"Todo\", working = \"In Progress\", needs_input = \"Needs Input\", review = \"In Review\" }\n\
             [stack]\ngate = [\"git grep -q Hello -- GREETING.md\"]\n\
             [pipeline]\ndefault = \"trivial\"\nplan_approval = \"never\"\n[models]\n[policy]\nalways_human = []\n",
        )
        .unwrap();
        let assignee = if assignee {
            "assignee = \"maintainer\"\n"
        } else {
            ""
        };
        fs::write(
            ticket.join("ticket.md"),
            format!(
                "+++\ntitle = \"Add a greeting\"\nauthor = \"maintainer\"\nstage = \"Todo\"\n\
                 {assignee}+++\n\nAdd GREETING.md saying Hello.\n"
            ),
        )
        .unwrap();
        let env = GitEnv::create(tmp.path().join("home")).unwrap();
        let remote = seed(&env, tmp.path(), &project).unwrap();
        let github = Arc::new(FakeGitHub {
            env: env.clone(),
            bare: remote.bare.clone(),
            open: Mutex::new(None),
            created: Mutex::new(Vec::new()),
            stale_reads: Mutex::new(0),
        });
        Self {
            data: tmp.path().join("data"),
            tmp,
            env,
            remote,
            github,
        }
    }

    /// A reply file: `greeting` committed as GREETING.md when given, then
    /// `result` as the run's result when given.
    fn reply(&self, greeting: Option<&str>, result: Option<&str>) -> Reply {
        let mut reply = Reply::default();
        if let Some(greeting) = greeting {
            reply.files.insert(
                RelativePath::new("GREETING.md").unwrap(),
                format!("{greeting}, reader.\n"),
            );
            reply.commit = Some("Add a greeting".to_owned());
        }
        if let Some(result) = result {
            let n = fs::read_dir(self.tmp.path()).unwrap().count();
            let path = self.tmp.path().join(format!("result-{n}.json"));
            fs::write(&path, result).unwrap();
            reply.result = Some(path);
        }
        reply
    }

    fn dirs(&self) -> ProjectDirs {
        ProjectDirs::new(&self.data, &Repo::parse(REPO).unwrap())
    }

    /// Runs `owlshift do DEMO-1` with one reply per run; returns its outcome
    /// and what it printed.
    fn run(&self, replies: Vec<Reply>, leave_plan: bool) -> (Result<Delivered, Stop>, String) {
        let replies = replies
            .into_iter()
            .map(|reply| {
                let n = fs::read_dir(self.tmp.path()).unwrap().count();
                let path = self.tmp.path().join(format!("reply-{n}.toml"));
                fs::write(&path, reply.render()).unwrap();
                path
            })
            .collect();
        let harness = Replies {
            program: PathBuf::from(env!("CARGO_BIN_EXE_owlshift-fake-harness")),
            replies: RefCell::new(replies),
            current: RefCell::new(None),
            leave_plan: Cell::new(leave_plan),
        };
        let env = self.env.clone();
        let executor = on_demand::executor(
            Git::with_setup("git", move |command| env.apply(command)),
            AgentEnv::new(self.env.agent_parent(), &[]).unwrap(),
        );
        let config_text = fs::read_to_string(self.remote.checkout.join("owlshift.toml")).unwrap();
        let config = ProjectConfig::parse(&config_text).unwrap();
        let tracker = MarkdownTracker::new(&self.remote.checkout);
        let forge =
            GitHubForge::with_transport(Shared(self.github.clone()), Repo::parse(REPO).unwrap());
        let dirs = self.dirs();
        let remote_url = self.remote.bare.to_string_lossy().into_owned();
        let on_demand = OnDemand {
            executor: &executor,
            tracker: &tracker,
            forge: &forge,
            harness: &harness,
            remote_url: &remote_url,
            config: &config,
            dirs: &dirs,
            head_wait: Duration::ZERO,
        };
        let mut out = Vec::new();
        let mut sink = EventSink::new(REPO, EventLog::in_dir(&self.data), &mut out);
        let outcome = on_demand.run(&TicketId::new(TICKET).unwrap(), &mut sink);
        (outcome, String::from_utf8(out).unwrap())
    }

    fn events(&self) -> Vec<Event> {
        let text = fs::read_to_string(EventLog::in_dir(&self.data).path()).unwrap_or_default();
        text.lines()
            .map(|line| Event::parse(line).unwrap())
            .collect()
    }

    fn remote_branch(&self) -> Option<String> {
        self.env
            .run(
                &self.remote.bare,
                &["rev-parse", "--verify", "--quiet", BRANCH],
            )
            .ok()
            .map(|out| String::from_utf8(out).unwrap().trim().to_owned())
    }

    fn comments(&self) -> Vec<String> {
        let tracker = MarkdownTracker::new(&self.remote.checkout);
        tracker
            .comments(&TicketId::new(TICKET).unwrap())
            .unwrap()
            .into_iter()
            .map(|comment| comment.body)
            .collect()
    }

    /// The person's checkout: HEAD and its branch.
    fn person(&self) -> (Vec<u8>, Vec<u8>) {
        let checkout = &self.remote.checkout;
        (
            self.env.run(checkout, &["rev-parse", "HEAD"]).unwrap(),
            self.env.run(checkout, &["symbolic-ref", "HEAD"]).unwrap(),
        )
    }
}

fn kinds(events: &[Event]) -> Vec<EventKind> {
    events.iter().map(|event| event.kind).collect()
}

#[test]
fn a_ticket_becomes_a_pull_request_with_its_report() {
    let bench = Bench::new(true);
    let person = bench.person();
    let (outcome, printed) = bench.run(vec![bench.reply(Some("Hello"), Some(DONE))], false);
    let delivered = outcome.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    assert!(delivered.opened);
    assert_eq!(delivered.pull_request.number, 1);

    // The gated commit is on the remote, and the pull request names it.
    let tip = bench.remote_branch().expect("the branch was pushed");
    assert_eq!(delivered.pull_request.head.as_str(), tip);
    let created = bench.github.created.lock().unwrap().clone();
    assert_eq!(created.len(), 1);
    assert_eq!(created[0]["headRefName"], BRANCH);
    assert_eq!(created[0]["baseRefName"], "main");
    assert_eq!(created[0]["title"], "Add a greeting");
    assert_eq!(created[0]["body"], "Says hello. Gate: green.");

    // The delivery report is on the ticket.
    let comments = bench.comments();
    assert_eq!(comments.len(), 1, "{comments:?}");
    for text in [
        "[owlshift] DELIVERY",
        "[#1](https://github.com/demo/project/pull/1)",
        "**Checks:** green",
        "**Project gate:** passed: `git grep -q Hello -- GREETING.md`.",
        "**Tone**: Friendly",
    ] {
        assert!(comments[0].contains(text), "{text}\n---\n{}", comments[0]);
    }

    // Every step is an event, recorded and printed alike.
    let events = bench.events();
    use EventKind::*;
    assert_eq!(
        kinds(&events),
        [
            Dispatch,
            RunStarted,
            RunEnded,
            TrackerWrite,
            TrackerWrite,
            TrackerWrite
        ]
    );
    for event in &events {
        assert!(
            printed.contains(&owlshift_runner::events::format_line(event)),
            "{printed}"
        );
    }

    // The work happened under the data directory, not in the person's
    // checkout, which kept its HEAD and branch.
    let dirs = bench.dirs();
    assert!(
        dirs.worktree(&TicketId::new(TICKET).unwrap())
            .join("GREETING.md")
            .is_file()
    );
    assert!(!bench.remote.checkout.join("GREETING.md").exists());
    assert_eq!(bench.person(), person);
    assert_eq!(dirs.unverified().unwrap(), None);

    // Again: the checkout is fetched, the push is up to date, and the pull
    // request and its report are found, not made twice.
    let (again, printed) = bench.run(vec![bench.reply(None, Some(DONE))], false);
    let again = again.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    assert!(!again.opened);
    assert_eq!(bench.github.created.lock().unwrap().len(), 1);
    assert_eq!(bench.comments().len(), 1);
    assert!(printed.contains("pushed=up_to_date"), "{printed}");
}

#[test]
fn a_red_gate_gets_one_fix_run_that_resumes_from_its_plan() {
    let bench = Bench::new(true);
    // The first report of the pull request's head lags behind the push.
    *bench.github.stale_reads.lock().unwrap() = 1;
    let replies = vec![
        bench.reply(Some("Helo"), Some(DONE)),
        bench.reply(Some("Hello"), Some(DONE)),
    ];
    let (outcome, printed) = bench.run(replies, true);
    outcome.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));

    let started: Vec<Event> = bench
        .events()
        .into_iter()
        .filter(|event| event.kind == EventKind::RunStarted)
        .collect();
    assert_eq!(started.len(), 2);
    let run_dir = started[1].data["run_dir"].as_str().unwrap();
    let brief =
        Brief::parse(&fs::read_to_string(Path::new(run_dir).join("brief.json")).unwrap()).unwrap();
    let failure = brief.gate_failure.expect("the fix run knows why");
    assert_eq!(
        failure.command.as_deref(),
        Some("git grep -q Hello -- GREETING.md")
    );
    let checkpoint = brief.checkpoint.expect("the fix run resumes from the plan");
    assert_eq!(checkpoint.plan.unwrap().as_str(), ".owlshift/run/plan.md");
}

/// How a case prepares its bench and gives its replies.
type Setup = Box<dyn Fn(&Bench) -> Vec<Reply>>;
/// Whether a case stopped as it should.
type Check = fn(&Stop) -> bool;

/// Every way a run stops short of a delivery: nothing reaches the ticket,
/// and the remote's branch moves only when the push itself went through.
#[test]
fn every_stop_before_delivery_leaves_the_ticket_untouched() {
    let questions = r#"{"format":1,"status":"questions","summary":"One choice is yours.",
        "questions":[{"id":"Q1","category":"scope","context":"Two readings.","text":"Which one?"}]}"#;
    let blocked = r#"{"format":1,"status":"blocked","summary":"The gate needs network."}"#;
    let reset: jiff::Timestamp = "2026-09-29T15:00:00Z".parse().unwrap();

    let cases: Vec<(&str, Setup, Check, Option<EventKind>)> = vec![
        (
            "questions",
            Box::new(move |b| vec![b.reply(None, Some(questions))]),
            |s| matches!(s, Stop::NeedsInput { questions, .. } if questions.len() == 1),
            Some(EventKind::Gate),
        ),
        (
            "blocked",
            Box::new(move |b| vec![b.reply(None, Some(blocked))]),
            |s| {
                matches!(
                    s,
                    Stop::Parked {
                        reason: ParkReason::Blocked,
                        ..
                    }
                )
            },
            Some(EventKind::Decision),
        ),
        (
            "usage limit",
            Box::new(move |b| {
                let mut reply = b.reply(None, None);
                reply.usage_limit = Some(reset);
                vec![reply]
            }),
            |s| matches!(s, Stop::UsageLimit { resets_at: Some(_) }),
            Some(EventKind::RunEnded),
        ),
        (
            "quarantine",
            Box::new(|b| {
                let mut reply = b.reply(None, Some(DONE));
                reply
                    .main_checkout
                    .insert(RelativePath::new("planted.txt").unwrap(), "x".to_owned());
                vec![reply]
            }),
            |s| {
                matches!(
                    s,
                    Stop::Parked {
                        reason: ParkReason::IsolationBreach,
                        ..
                    }
                )
            },
            Some(EventKind::Decision),
        ),
        (
            "two red gates",
            Box::new(|b| {
                vec![
                    b.reply(Some("Helo"), Some(DONE)),
                    b.reply(Some("Hi"), Some(DONE)),
                ]
            }),
            |s| matches!(s, Stop::Parked { reason: ParkReason::FailedRuns, detail } if detail.contains("gate")),
            Some(EventKind::Decision),
        ),
        (
            "push refused",
            Box::new(|b| {
                // Someone else's commit is already on the ticket's branch.
                let checkout = &b.remote.checkout;
                b.env
                    .run(checkout, &["switch", "--quiet", "-c", BRANCH])
                    .unwrap();
                fs::write(checkout.join("OTHER.md"), "other\n").unwrap();
                b.env.run(checkout, &["add", "OTHER.md"]).unwrap();
                b.env
                    .run(checkout, &["commit", "--quiet", "-m", "Other"])
                    .unwrap();
                b.env
                    .run(checkout, &["push", "--quiet", "origin", BRANCH])
                    .unwrap();
                b.env.run(checkout, &["switch", "--quiet", "main"]).unwrap();
                vec![b.reply(Some("Hello"), Some(DONE))]
            }),
            |s| matches!(s, Stop::Delivery(reason) if reason.contains("rejected")),
            Some(EventKind::RunEnded),
        ),
        (
            "branch moved after the push",
            Box::new(|b| {
                *b.github.stale_reads.lock().unwrap() = 100;
                vec![b.reply(Some("Hello"), Some(DONE))]
            }),
            |s| matches!(s, Stop::Delivery(reason) if reason.contains("moved")),
            Some(EventKind::TrackerWrite),
        ),
    ];
    for (name, replies, expected, last) in cases {
        let bench = Bench::new(true);
        let replies = replies(&bench);
        let pushed_before = bench.remote_branch();
        let (outcome, printed) = bench.run(replies, false);
        let stop = match outcome {
            Ok(delivered) => panic!("{name}: delivered {delivered:?}\n{printed}"),
            Err(stop) => stop,
        };
        assert!(expected(&stop), "{name}: {stop:?}\n{printed}");
        assert!(
            bench.comments().is_empty(),
            "{name}: {:?}",
            bench.comments()
        );
        assert_eq!(
            kinds(&bench.events()).last().copied(),
            last,
            "{name}\n{printed}"
        );
        if name != "branch moved after the push" {
            assert_eq!(bench.remote_branch(), pushed_before, "{name}");
            assert!(bench.github.created.lock().unwrap().is_empty(), "{name}");
        }
    }

    // A ticket without a decider is refused before anything runs.
    let bench = Bench::new(false);
    let (outcome, _) = bench.run(Vec::new(), false);
    assert!(
        matches!(&outcome, Err(Stop::Refused(reason)) if reason.contains("no assignee")),
        "{outcome:?}"
    );
    assert!(bench.events().is_empty());
}

/// A run that broke isolation leaves the project refused, whatever the
/// ticket, until a person looks.
#[test]
fn a_quarantined_run_keeps_the_project_refused() {
    let bench = Bench::new(true);
    let mut reply = bench.reply(None, Some(DONE));
    reply
        .main_checkout
        .insert(RelativePath::new("planted.txt").unwrap(), "x".to_owned());
    let (outcome, _) = bench.run(vec![reply], false);
    assert!(matches!(outcome, Err(Stop::Parked { .. })), "{outcome:?}");
    let marker = bench.dirs().unverified().unwrap().expect("a marker");
    assert!(marker.contains("broke isolation"), "{marker}");

    let (again, _) = bench.run(Vec::new(), false);
    let Err(stop @ Stop::Unverified { .. }) = again else {
        panic!("{again:?}");
    };
    assert!(stop.to_string().contains("delete"), "{stop}");
}
