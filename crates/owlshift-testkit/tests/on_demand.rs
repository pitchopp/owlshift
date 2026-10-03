//! `owlshift do` (OWL-20) and `owlshift continue` (OWL-122) end to end: the
//! runner's `on_demand` runs on the Markdown tracker, the fake harness,
//! hermetic git with a local bare remote standing in for GitHub's git side,
//! and the real GitHub adapter over a fake transport standing in for its API.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::rc::Rc;
use std::sync::{Arc, Mutex, PoisonError};

/// Held by each `do` of this file: see `Bench::run`.
static RUNS: Mutex<()> = Mutex::new(());
use std::time::Duration;

use jiff::{SignedDuration, Timestamp};
use serde_json::{Value, json};
use tempfile::TempDir;

use owlshift_adapters::forge::Repo;
use owlshift_adapters::forge::github::GitHubForge;
use owlshift_adapters::graphql::{Response, Transport};
use owlshift_adapters::tracker::markdown::MarkdownTracker;
use owlshift_adapters::tracker::{self, Capability, Comment, Ticket, Tracker};
use owlshift_contracts::Role;
use owlshift_contracts::brief::{Brief, PermissionLevel, Relation, ThreadEntry};
use owlshift_contracts::config::ProjectConfig;
use owlshift_contracts::event::{Event, EventKind};
use owlshift_contracts::ids::{RelativePath, TicketId};
use owlshift_contracts::refs::{AskDecider, Waiting};
use owlshift_core::decider::DeciderRule;
use owlshift_core::state::{MAX_RESOLVED_PASSES, ParkReason};
use owlshift_runner::agent_env::AgentEnv;
use owlshift_runner::events::{EventLog, EventSink};
use owlshift_runner::executor::harness::ClaudeHarness;
use owlshift_runner::executor::{
    Git, Harness, HarnessEnd, HarnessError, HarnessRun, RUN_DIR, RunLog,
};
use owlshift_runner::on_demand::{self, Delivered, OnDemand, Stop};
use owlshift_runner::project::{self, ProjectDirs};
use owlshift_runner::ticket_ref::{self, TicketRecord};
use owlshift_testkit::gh;
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

/// What an agent does in its worktree beyond what a reply says, just before
/// its first run: the bench plays it in this process.
type Agent = Box<dyn FnOnce(&Path)>;

/// The build role leaving a plan and a ledger.
fn leave_plan(worktree: &Path) {
    let dir = worktree.join(RUN_DIR);
    fs::write(dir.join("plan.md"), "1. Add the greeting.\n").unwrap();
    fs::write(dir.join("ledger.json"), "{\"steps\":[]}\n").unwrap();
}

/// The fake harness, one reply per run in order, with what the agent does
/// first.
struct Replies {
    program: PathBuf,
    replies: RefCell<VecDeque<PathBuf>>,
    current: RefCell<Option<FakeHarness>>,
    first: RefCell<Option<Agent>>,
}

impl Harness for Replies {
    fn command(&self, run: &HarnessRun<'_>) -> Result<Command, HarnessError> {
        if let Some(agent) = self.first.borrow_mut().take() {
            agent(run.worktree);
        }
        let reply = self
            .replies
            .borrow_mut()
            .pop_front()
            .expect("a reply for every run");
        let harness = FakeHarness {
            program: self.program.clone(),
            reply,
            // The bench runs `do` bare: see `Bench::run`.
            readable: Vec::new(),
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

/// The Markdown tracker, posting the runner's comments at a virtual clock.
/// It records times to the second, and the answer check compares the
/// tracker's times only, a comment newer than an ask, so every comment of the bench, the decider's included, takes the next
/// minute of one clock.
struct Clocked<'a> {
    inner: MarkdownTracker,
    clock: &'a Cell<Timestamp>,
    /// A comment starting with this is refused, as a tracker that fails.
    refuse: Option<&'static str>,
    /// Every stage write is refused.
    refuse_stage: bool,
}

/// The clock's time, then a minute later.
fn tick(clock: &Cell<Timestamp>) -> Timestamp {
    let at = clock.get();
    clock.set(at.checked_add(SignedDuration::from_mins(1)).unwrap());
    at
}

impl Tracker for Clocked<'_> {
    fn capabilities(&self) -> &'static [Capability] {
        Tracker::capabilities(&self.inner)
    }

    fn ticket(&self, id: &TicketId) -> Result<Ticket, tracker::Error> {
        Tracker::ticket(&self.inner, id)
    }

    fn comments(&self, id: &TicketId) -> Result<Vec<Comment>, tracker::Error> {
        Tracker::comments(&self.inner, id)
    }

    fn post_comment(&self, id: &TicketId, body: &str) -> Result<Comment, tracker::Error> {
        let other = |e: &dyn std::fmt::Display| {
            tracker::Error::new(tracker::ErrorKind::Other, e.to_string())
        };
        if self.refuse.is_some_and(|start| body.starts_with(start)) {
            return Err(other(&"the tracker is down"));
        }
        self.inner
            .post_comment(id, MarkdownTracker::AGENT, tick(self.clock), body)
            .map_err(|e| other(&e))?;
        // The newest comment is the one just posted.
        Tracker::comments(&self.inner, id)?
            .pop()
            .ok_or_else(|| other(&"the comment just posted is gone"))
    }

    fn set_stage(&self, id: &TicketId, state: &str) -> Result<(), tracker::Error> {
        if self.refuse_stage {
            return Err(tracker::Error::new(
                tracker::ErrorKind::Other,
                "the tracker is down",
            ));
        }
        Tracker::set_stage(&self.inner, id, state)
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
    /// The time of the next comment on the ticket.
    clock: Cell<Timestamp>,
    /// The runner's comments starting with this are refused by the tracker.
    refuse: Cell<Option<&'static str>>,
    /// The tracker refuses every stage write.
    refuse_stage: Cell<bool>,
    /// How long after the time of the next comment `continue` reads this
    /// machine's clock: by default the quiet window, so the decider's
    /// latest comment, a minute older, counts.
    waited: Cell<SignedDuration>,
}

const DONE: &str = r#"{"format":5,"status":"done","summary":"Added GREETING.md; the gate passes.",
"decisions":[{"question":"Tone","decision":"Friendly","basis":"The ticket"}],
"pr":{"branch":"owlshift/demo-1","title":"Add a greeting","body":"Says hello. Gate: green."}}"#;

/// The project's rules, which every brief carries (OWL-61).
const AGENTS: &str = "Sign off every commit (`git commit -s`).\n";

impl Bench {
    fn new(assignee: bool) -> Self {
        Self::by(assignee, "maintainer")
    }

    /// A bench whose ticket was created by `author`.
    fn by(assignee: bool, author: &str) -> Self {
        let assignee = if assignee {
            "assignee = \"maintainer\"\n"
        } else {
            ""
        };
        Self::with(author, assignee, "")
    }

    /// A bench whose ticket was created by `author`, with `front` added to
    /// its front matter, and `zones` added to the project file the remote
    /// holds.
    fn with(author: &str, front: &str, zones: &str) -> Self {
        let tmp = tempfile::Builder::new()
            .prefix("owlshift do ")
            .tempdir()
            .unwrap();
        let project = tmp.path().join("project");
        let ticket = project.join("tickets").join(TICKET);
        fs::create_dir_all(&ticket).unwrap();
        fs::write(
            project.join("owlshift.toml"),
            format!(
                "requires = \">=0.0\"\n[tracker]\nkind = \"markdown\"\nadmit = \"delegation\"\n\
                 states = {{ ready = \"Todo\", working = \"In Progress\", needs_input = \"Needs Input\", review = \"In Review\" }}\n\
                 [stack]\ngate = [\"git grep -q Hello -- GREETING.md\"]\n\
                 [pipeline]\ndefault = \"trivial\"\nplan_approval = \"never\"\n[models]\n[policy]\nalways_human = []\n\
                 {zones}"
            ),
        )
        .unwrap();
        fs::write(project.join("AGENTS.md"), AGENTS).unwrap();
        fs::write(
            ticket.join("ticket.md"),
            format!(
                "+++\ntitle = \"Add a greeting\"\nauthor = \"{author}\"\nstage = \"Todo\"\n\
                 {front}+++\n\nAdd GREETING.md saying Hello.\n"
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
            clock: Cell::new("2026-10-02T09:00:00Z".parse().unwrap()),
            refuse: Cell::new(None),
            refuse_stage: Cell::new(false),
            waited: Cell::new(SignedDuration::from_mins(10)),
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
    fn run(&self, replies: Vec<Reply>, first: Option<Agent>) -> (Result<Delivered, Stop>, String) {
        self.invoke(false, replies, first)
    }

    /// Runs `owlshift continue DEMO-1` with one reply per run, the answer
    /// check's included.
    fn continue_ticket(&self, replies: Vec<Reply>) -> (Result<Delivered, Stop>, String) {
        self.invoke(true, replies, None)
    }

    /// The decider comments on the ticket, at the next minute.
    fn answer(&self, body: &str) {
        self.comment_as("maintainer", body);
    }

    /// `author` comments on the ticket, at the next minute.
    fn comment_as(&self, author: &str, body: &str) {
        MarkdownTracker::new(&self.remote.checkout)
            .post_comment(&ticket(), author, tick(&self.clock), body)
            .unwrap();
    }

    /// The ticket's file on the tracker, which a person edits.
    fn ticket_file(&self) -> PathBuf {
        self.remote
            .checkout
            .join("tickets")
            .join(TICKET)
            .join("ticket.md")
    }

    /// The ticket's visible stage on the tracker.
    fn stage(&self) -> String {
        MarkdownTracker::new(&self.remote.checkout)
            .ticket(&ticket())
            .unwrap()
            .stage
    }

    /// The `stage` of each event of `kind`, oldest first: the stage moves
    /// with `tracker_write`, the refused ones with `warning`.
    fn stage_events(&self, kind: EventKind) -> Vec<String> {
        self.events()
            .iter()
            .filter(|event| {
                event.kind == kind
                    && (event.data.get("action") == Some(&json!("stage"))
                        || event.data.get("what") == Some(&json!("stage_not_moved")))
            })
            .map(|event| event.data["state"].as_str().unwrap().to_owned())
            .collect()
    }

    /// What the ticket ref holds.
    fn record(&self) -> TicketRecord {
        let env = self.env.clone();
        let git = Git::with_setup("git", move |command| env.apply(command));
        ticket_ref::read(&git, &self.dirs().checkout(), &ticket())
            .unwrap()
            .expect("a ticket ref")
            .record
    }

    /// The brief of every run, oldest first.
    fn briefs(&self) -> Vec<Brief> {
        self.events()
            .iter()
            .filter(|event| event.kind == EventKind::RunStarted)
            .map(|event| {
                let dir = Path::new(event.data["run_dir"].as_str().unwrap());
                Brief::parse(&fs::read_to_string(dir.join("brief.json")).unwrap()).unwrap()
            })
            .collect()
    }

    fn invoke(
        &self,
        continuing: bool,
        replies: Vec<Reply>,
        first: Option<Agent>,
    ) -> (Result<Delivered, Stop>, String) {
        // One `do` at a time in this process. A run forks children (the
        // gate's `sh`, the agent's `git`: a command that sets PATH and names
        // a bare program is forked, not spawned), and a child forked by one
        // test's thread shares every file the process has open until it
        // execs, another test's project lock included: that test's next
        // `do` on the project would then find its own lock still held
        // (seen on macOS CI, where forking is slow). `owlshift do` itself
        // takes the lock once per process.
        let _one_at_a_time = RUNS.lock().unwrap_or_else(PoisonError::into_inner);
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
            first: RefCell::new(first),
        };
        // The executor's credential probes run the real gh on `github.com`.
        gh::warm_up(&self.env.agent_parent());
        let env = self.env.clone();
        let executor = on_demand::executor(
            Git::with_setup("git", move |command| env.apply(command)),
            // Bare, as the scenarios' isolation tests run: what `do` does
            // around the executor is under test here, and a breaking agent
            // must reach the checkout and its own `.git`, which the sandbox
            // closes. Confinement has its own tests (OWL-41), and native
            // Windows refuses confined runs.
            AgentEnv::new(self.env.agent_parent())
                .unwrap()
                .without_confinement(),
        );
        let config_text = fs::read_to_string(self.remote.checkout.join("owlshift.toml")).unwrap();
        let config = ProjectConfig::parse(&config_text).unwrap();
        let tracker = Clocked {
            inner: MarkdownTracker::new(&self.remote.checkout),
            clock: &self.clock,
            refuse: self.refuse.get(),
            refuse_stage: self.refuse_stage.get(),
        };
        let forge =
            GitHubForge::with_transport(Shared(self.github.clone()), Repo::parse(REPO).unwrap());
        let dirs = self.dirs();
        let remote_url = self.remote.bare.to_string_lossy().into_owned();
        let now = || self.clock.get().checked_add(self.waited.get()).unwrap();
        let on_demand = OnDemand {
            executor: &executor,
            tracker: &tracker,
            forge: &forge,
            // One harness plays every run, a reply each, whatever its role.
            build: &harness,
            answer_check: &harness,
            resolver: &harness,
            remote_url: &remote_url,
            config: &config,
            dirs: &dirs,
            head_wait: Duration::ZERO,
            clock: &now,
        };
        let mut out = Vec::new();
        let mut sink = EventSink::new(REPO, EventLog::in_dir(&self.data), &mut out);
        let outcome = if continuing {
            on_demand.continue_ticket(&ticket(), &mut sink)
        } else {
            on_demand.run(&ticket(), &mut sink)
        };
        assert!(
            harness.replies.borrow().is_empty(),
            "a reply was left unused: {outcome:?}"
        );
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

fn ticket() -> TicketId {
    TicketId::new(TICKET).unwrap()
}

fn kinds(events: &[Event]) -> Vec<EventKind> {
    events.iter().map(|event| event.kind).collect()
}

#[test]
fn a_ticket_becomes_a_pull_request_with_its_report() {
    let bench = Bench::new(true);
    let person = bench.person();
    let (outcome, printed) = bench.run(vec![bench.reply(Some("Hello"), Some(DONE))], None);
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
            TrackerWrite,
            RunStarted,
            RunEnded,
            TrackerWrite,
            TrackerWrite,
            TrackerWrite,
            TrackerWrite
        ]
    );
    // Working once dispatched, review once the pull request is open.
    assert_eq!(
        bench.stage_events(TrackerWrite),
        ["In Progress", "In Review"]
    );
    assert_eq!(bench.stage(), "In Review");
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
    let (again, printed) = bench.run(vec![bench.reply(None, Some(DONE))], None);
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
    let (outcome, printed) = bench.run(replies, Some(Box::new(leave_plan)));
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

    // The project's rules, from the base commit, reach every run's brief.
    assert_eq!(brief.rules.len(), 1, "{:?}", brief.rules);
    let rule = &brief.rules[0];
    assert_eq!(
        (rule.source.as_str(), rule.text.as_str()),
        ("AGENTS.md", AGENTS)
    );
    assert!(rule.applies_to.is_empty());
}

/// The brief of the first run, and its ticket author's relation: the brief
/// tells the agent whether the ticket text is the decider's instruction or
/// quoted data.
#[test]
fn the_brief_names_the_relation_of_the_ticket_author() {
    for (author, relation) in [
        ("maintainer", Relation::Decider),
        ("stranger", Relation::Other),
    ] {
        let bench = Bench::by(true, author);
        let replies = vec![bench.reply(Some("Hello"), Some(DONE))];
        let (outcome, printed) = bench.run(replies, None);
        outcome.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));

        let started = bench
            .events()
            .into_iter()
            .find(|event| event.kind == EventKind::RunStarted)
            .expect("a run started");
        let run_dir = started.data["run_dir"].as_str().unwrap();
        let brief =
            Brief::parse(&fs::read_to_string(Path::new(run_dir).join("brief.json")).unwrap())
                .unwrap();
        assert_eq!(brief.ticket.author.name, author);
        assert_eq!(brief.ticket.author.relation, relation, "{author}");
    }
}

/// OWL-129: the brief lists the zones the ticket's `zone:` labels declare,
/// for a ticket with an assignee too. Its decider never reads the labels, so
/// a malformed one is left out of the list instead of refusing the ticket.
#[test]
fn the_brief_lists_the_zones_the_ticket_declares() {
    let front = "assignee = \"maintainer\"\n\
                 labels = [\"Feature\", \"zone:docs\", \"Zone:../x\", \"zone:web/cart\"]\n";
    let bench = Bench::with("maintainer", front, "");
    let replies = vec![bench.reply(Some("Hello"), Some(DONE))];
    let (outcome, printed) = bench.run(replies, None);
    outcome.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    let brief = bench.briefs().pop().unwrap();
    assert_eq!(brief.zones, ["docs", "web/cart"]);
    // OWL-134: the run log says the malformed label did not reach the agent.
    let warnings: Vec<Event> = bench
        .events()
        .into_iter()
        .filter(|event| event.kind == EventKind::Warning)
        .collect();
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    let data = &warnings[0].data;
    assert_eq!(data["what"], "zone_label_left_out");
    assert_eq!(data["label"], "Zone:../x");
    assert!(data["reason"].as_str().is_some_and(|r| !r.is_empty()));
    let events = bench.events();
    let started = events
        .iter()
        .position(|event| event.kind == EventKind::RunStarted)
        .unwrap();
    assert_eq!(events[started + 1].kind, EventKind::Warning);
    assert_eq!(events[started + 1].run, events[started].run);
}

/// OWL-134: a malformed `zone:` label still refuses a ticket without an
/// assignee, before any run, so no warning is recorded.
#[test]
fn a_malformed_zone_label_still_refuses_a_ticket_without_an_assignee() {
    let bench = Bench::with("maintainer", "labels = [\"zone:../x\"]\n", "");
    let (outcome, _) = bench.run(Vec::new(), None);
    let stop = outcome.unwrap_err();
    assert!(
        matches!(&stop, Stop::Refused(why) if why.contains("declares a zone that")),
        "{stop}"
    );
    assert!(bench.events().is_empty());
}

/// How a case prepares its bench and gives its replies.
type Setup = Box<dyn Fn(&Bench) -> Vec<Reply>>;
/// Whether a case stopped as it should.
type Check = fn(&Stop) -> bool;

/// Every way a run stops short of a delivery, questions aside (they are
/// posted: see the `continue` tests): nothing reaches the ticket but a park's
/// PARKED comment, which says to run `do` again since no question round
/// kept a ticket ref, and the remote's branch moves only when the push
/// itself went through.
#[test]
fn every_stop_before_delivery_leaves_the_ticket_untouched() {
    let blocked = r#"{"format":5,"status":"blocked","summary":"The gate needs network."}"#;
    let reset: jiff::Timestamp = "2026-09-29T15:00:00Z".parse().unwrap();

    let cases: Vec<(&str, Setup, Check, Option<EventKind>)> = vec![
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
            |s| matches!(s, Stop::Parked { reason: ParkReason::FailedRuns, detail, .. } if detail.contains("gate")),
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
        (
            "rules too long",
            Box::new(|b| {
                // A rule is never cut: the run is refused before it starts.
                let checkout = &b.remote.checkout;
                fs::write(checkout.join("AGENTS.md"), "a".repeat(64 * 1024 + 1)).unwrap();
                b.env
                    .run(checkout, &["commit", "--quiet", "-am", "Grow the rules"])
                    .unwrap();
                b.env
                    .run(checkout, &["push", "--quiet", "origin", "main"])
                    .unwrap();
                Vec::new()
            }),
            |s| matches!(s, Stop::Refused(reason) if reason.contains("AGENTS.md on origin/main holds")),
            None,
        ),
    ];
    for (name, replies, expected, last) in cases {
        let bench = Bench::new(true);
        let replies = replies(&bench);
        let pushed_before = bench.remote_branch();
        let (outcome, printed) = bench.run(replies, None);
        let stop = match outcome {
            Ok(delivered) => panic!("{name}: delivered {delivered:?}\n{printed}"),
            Err(stop) => stop,
        };
        assert!(expected(&stop), "{name}: {stop:?}\n{printed}");
        let comments = bench.comments();
        if let Stop::Parked { unposted, .. } = &stop {
            assert_eq!(unposted, &None, "{name}");
            assert_eq!(comments.len(), 1, "{name}: {comments:?}");
            let parked = &comments[0];
            assert!(
                parked.starts_with("[owlshift] PARKED\n"),
                "{name}: {parked}"
            );
            assert!(parked.contains("`owlshift do DEMO-1`"), "{name}: {parked}");
            assert!(!parked.contains("planted"), "{name}: {parked}");
        } else {
            assert!(comments.is_empty(), "{name}: {comments:?}");
        }
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

    // A ticket with neither an assignee nor a declared zone has no decider,
    // whatever the owners: refused before anything is cloned.
    let bench = Bench::new(false);
    let (outcome, _) = bench.run(Vec::new(), None);
    assert!(
        matches!(&outcome, Err(Stop::Refused(reason))
            if reason.contains("DEMO-1 has no decider: the ticket has no assignee and touches no zone with an owner")
                && reason.contains("label it `zone:<folder>`")),
        "{outcome:?}"
    );
    assert!(bench.events().is_empty());
    assert!(!bench.dirs().checkout().exists());
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
    let (outcome, _) = bench.run(vec![reply], None);
    assert!(matches!(outcome, Err(Stop::Parked { .. })), "{outcome:?}");
    let marker = bench.dirs().unverified().unwrap().expect("a marker");
    assert!(marker.contains("broke isolation"), "{marker}");

    let (again, _) = bench.run(Vec::new(), None);
    let Err(stop @ Stop::Unverified { .. }) = again else {
        panic!("{again:?}");
    };
    assert!(stop.to_string().contains("delete"), "{stop}");
}

/// Copies a folder, byte for byte.
fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).unwrap();
        }
    }
}

/// An agent that points its worktree's `.git` at a git directory of its own:
/// the checkout's, copied, with a `pre-push` hook that writes `fired` and
/// the configuration that makes git run it. Git in the worktree then works
/// as before, so the executor's isolation check sees nothing.
fn redirect_git_link(evil: PathBuf, fired: PathBuf) -> Agent {
    Box::new(move |worktree: &Path| {
        let link = fs::read_to_string(worktree.join(".git")).unwrap();
        let admin = PathBuf::from(link.trim().strip_prefix("gitdir:").unwrap().trim());
        let admin = if admin.is_relative() {
            worktree.join(admin)
        } else {
            admin
        };
        let common = admin.join(fs::read_to_string(admin.join("commondir")).unwrap().trim());
        let evil_common = evil.join("common");
        copy_dir(&common, &evil_common);
        let hooks = evil.join("hooks");
        fs::create_dir_all(&hooks).unwrap();
        let hook = hooks.join("pre-push");
        let fired = fired.to_string_lossy().replace('\\', "/");
        fs::write(&hook, format!("#!/bin/sh\necho ran > '{fired}'\n")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let hooks_path = hooks.to_string_lossy().replace('\\', "/");
        let mut config = fs::read_to_string(evil_common.join("config")).unwrap();
        config.push_str(&format!("[core]\n\thooksPath = {hooks_path}\n"));
        fs::write(evil_common.join("config"), config).unwrap();
        let evil_admin = evil.join("admin");
        copy_dir(&admin, &evil_admin);
        fs::write(
            evil_admin.join("commondir"),
            format!("{}\n", evil_common.display()),
        )
        .unwrap();
        fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", evil_admin.display()),
        )
        .unwrap();
    })
}

/// The runner never runs git with the person's credentials where the agent
/// could redirect it: a run whose worktree's `.git` leads elsewhere is a
/// breach, its hook never runs, and nothing is pushed.
#[test]
fn an_agent_that_redirects_its_git_link_gets_no_hook_and_no_push() {
    let bench = Bench::new(true);
    let evil = bench.tmp.path().join("evil");
    let fired = bench.tmp.path().join("hook-fired");
    let agent = redirect_git_link(evil, fired.clone());
    let (outcome, printed) = bench.run(vec![bench.reply(Some("Hello"), Some(DONE))], Some(agent));
    match &outcome {
        Err(Stop::Parked {
            reason: ParkReason::IsolationBreach,
            detail,
            ..
        }) => assert!(
            detail.contains("the worktree's .git link was changed"),
            "{detail}"
        ),
        other => panic!("{other:?}\n{printed}"),
    }
    assert!(!fired.exists(), "the planted hook ran");
    assert_eq!(bench.remote_branch(), None);
    assert!(bench.github.created.lock().unwrap().is_empty());
    // The PARKED comment alone, without what the breach changed.
    let comments = bench.comments();
    assert_eq!(comments.len(), 1, "{comments:?}");
    assert!(comments[0].starts_with("[owlshift] PARKED\n"));
    assert!(!comments[0].contains(".git"), "{}", comments[0]);
    let marker = bench.dirs().unverified().unwrap().expect("a marker");
    assert!(marker.contains(".git link"), "{marker}");
}

/// A project that changes its default branch gets its new base at the next
/// fetch.
#[test]
fn the_checkout_follows_the_forges_default_branch() {
    let bench = Bench::new(true);
    let env = bench.env.clone();
    let git = Git::with_setup("git", move |command| env.apply(command));
    let url = bench.remote.bare.to_string_lossy().into_owned();
    let base = project::sync_checkout(&git, &bench.dirs(), &url).unwrap();
    assert_eq!(base.remote_ref, "origin/main");
    let bare = &bench.remote.bare;
    let main = String::from_utf8(bench.env.run(bare, &["rev-parse", "main"]).unwrap()).unwrap();
    assert_eq!(base.commit, main.trim());

    bench.env.run(bare, &["branch", "trunk", "main"]).unwrap();
    bench
        .env
        .run(bare, &["symbolic-ref", "HEAD", "refs/heads/trunk"])
        .unwrap();
    let base = project::sync_checkout(&git, &bench.dirs(), &url).unwrap();
    assert_eq!(
        (base.remote_ref.as_str(), base.branch.as_str()),
        ("origin/trunk", "trunk")
    );

    // A default branch a run made symbolic survives the fetch and would lead
    // git to another branch of origin: it is refused (OWL-51).
    let checkout = bench.dirs().checkout();
    bench
        .env
        .run(
            &checkout,
            &[
                "symbolic-ref",
                "refs/remotes/origin/trunk",
                "refs/remotes/origin/main",
            ],
        )
        .unwrap();
    let error = project::sync_checkout(&git, &bench.dirs(), &url).unwrap_err();
    assert!(
        error.contains("refs/remotes/origin/trunk") && error.contains("is a symbolic ref"),
        "{error}"
    );
}

/// A run can move `origin/main` unseen, since the isolation check leaves
/// remote-tracking refs out, but it does not choose where the next branch
/// starts (OWL-51): the next `do` fetches, then pins the base to a commit by
/// its full name, which a local branch named `origin/main` cannot shadow.
#[test]
fn a_run_that_moves_origin_main_does_not_choose_the_next_base() {
    let bench = Bench::new(true);
    let evil = Rc::new(RefCell::new(String::new()));
    let (env, planted) = (bench.env.clone(), evil.clone());
    let agent: Agent = Box::new(move |worktree: &Path| {
        let git = |args: &[&str]| {
            let out = env.run(worktree, args).unwrap();
            String::from_utf8(out).unwrap().trim().to_owned()
        };
        let commit = git(&["commit-tree", "HEAD^{tree}", "-p", "HEAD", "-m", "Evil"]);
        git(&["update-ref", "refs/remotes/origin/main", &commit]);
        git(&["update-ref", "refs/remotes/origin/evil", &commit]);
        git(&[
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/evil",
        ]);
        *planted.borrow_mut() = commit;
    });
    let blocked = r#"{"format":5,"status":"blocked","summary":"Waiting."}"#;
    let (outcome, printed) = bench.run(vec![bench.reply(None, Some(blocked))], Some(agent));
    assert!(
        matches!(
            outcome,
            Err(Stop::Parked {
                reason: ParkReason::Blocked,
                ..
            })
        ),
        "{outcome:?}\n{printed}"
    );
    // Not a breach: the check leaves remote-tracking refs out.
    assert_eq!(bench.dirs().unverified().unwrap(), None);
    let evil = evil.borrow().clone();
    let checkout = bench.dirs().checkout();
    let at = |dir: &Path, name: &str| {
        let out = bench.env.run(dir, &["rev-parse", name]).unwrap();
        String::from_utf8(out).unwrap().trim().to_owned()
    };
    assert_eq!(at(&checkout, "refs/remotes/origin/main"), evil);

    // A later ticket starts a new branch: here the same one, removed. A
    // local branch named like the base points at the agent's commit too.
    let worktree = bench.dirs().worktree(&TicketId::new(TICKET).unwrap());
    let worktree = worktree.to_str().unwrap();
    for args in [
        vec!["worktree", "remove", "--force", worktree],
        vec!["branch", "-D", BRANCH],
        vec!["branch", "origin/main", &evil],
    ] {
        bench.env.run(&checkout, &args).unwrap();
    }
    let (outcome, printed) = bench.run(vec![bench.reply(Some("Hello"), Some(DONE))], None);
    outcome.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    let tip = bench.remote_branch().expect("the branch was pushed");
    let bare = &bench.remote.bare;
    assert_eq!(at(bare, &format!("{tip}^")), at(bare, "refs/heads/main"));
    assert_ne!(at(bare, &format!("{tip}^")), evil);
}

/// A confined `do` whose agent cannot run, because this machine cannot
/// confine agents or Claude Code has no token for agent runs (OWL-94), is
/// refused before anything is cloned, read or recorded. Which of the two
/// depends on the machine: native Windows and a Linux without bwrap fail
/// the first.
#[test]
fn a_do_whose_agent_cannot_run_is_refused_before_anything() {
    let bench = Bench::new(true);
    let env = bench.env.clone();
    // Confined, and with no token for agent runs.
    let executor = on_demand::executor(
        Git::with_setup("git", move |command| env.apply(command)),
        AgentEnv::new(bench.env.agent_parent()).unwrap(),
    );
    let harness = ClaudeHarness {
        program: PathBuf::from("claude"),
        prompt: "# Build".to_owned(),
        model: None,
        effort: None,
        max_budget_usd: None,
        login: None,
    };
    let config_text = fs::read_to_string(bench.remote.checkout.join("owlshift.toml")).unwrap();
    let config = ProjectConfig::parse(&config_text).unwrap();
    let tracker = MarkdownTracker::new(&bench.remote.checkout);
    let forge =
        GitHubForge::with_transport(Shared(bench.github.clone()), Repo::parse(REPO).unwrap());
    let dirs = bench.dirs();
    let remote_url = bench.remote.bare.to_string_lossy().into_owned();
    let on_demand = OnDemand {
        executor: &executor,
        tracker: &tracker,
        forge: &forge,
        build: &harness,
        answer_check: &harness,
        resolver: &harness,
        remote_url: &remote_url,
        config: &config,
        dirs: &dirs,
        head_wait: Duration::ZERO,
        clock: &on_demand::system_clock,
    };
    let mut out = Vec::new();
    let mut sink = EventSink::new(REPO, EventLog::in_dir(&bench.data), &mut out);
    let outcome = on_demand.run(&ticket(), &mut sink);
    let Err(Stop::Refused(reason)) = &outcome else {
        panic!("{outcome:?}");
    };
    if executor.agent.sandbox_ready().is_ok() {
        assert!(reason.contains("`claude setup-token`"), "{reason}");
    }
    assert!(!dirs.checkout().exists(), "nothing is cloned");
    assert!(bench.events().is_empty());
}

/// The Build run's first question round.
const ROUND_1: &str = r#"{"format":5,"status":"questions",
"summary":"The greeting's language and words are not given.",
"questions":[
  {"id":"Q1","category":"scope","context":"The ticket names no language.",
   "text":"Which language should the greeting use?","options":["English","French"],
   "recommendation":"English"},
  {"id":"Q2","category":"scope","context":"The ticket names neither the words nor the ending.",
   "text":"What should the greeting say, and should it end with a sign-off?"}]}"#;

/// An answer check's result: a verdict per question (id, class, reason),
/// with a reply on a counter-question.
fn check(verdicts: &[(&str, &str, &str)]) -> String {
    let verdicts: Vec<Value> = verdicts
        .iter()
        .map(|(question, class, reason)| {
            let mut verdict = json!({ "question": question, "class": class, "reason": reason });
            if *class == "counter_question" {
                verdict["reply"] = json!("A sign-off is a closing line. Should there be one?");
            }
            verdict
        })
        .collect();
    json!({ "format": 5, "status": "done", "summary": "Checked.", "verdicts": verdicts })
        .to_string()
}

/// The ids of a brief's latest ask.
fn latest_ask(brief: &Brief) -> Vec<String> {
    brief
        .latest_ask()
        .map(|(_, questions)| questions.iter().map(|q| q.id.to_string()).collect())
        .unwrap_or_default()
}

/// A brief's thread, an entry per word: `questions`, `reask`, `decision`,
/// or the comment author's relation.
fn shape(brief: &Brief) -> Vec<String> {
    brief
        .thread
        .iter()
        .map(|entry| match entry {
            ThreadEntry::Questions { .. } => "questions".to_owned(),
            ThreadEntry::Reask { .. } => "reask".to_owned(),
            ThreadEntry::Decision { .. } => "decision".to_owned(),
            ThreadEntry::Comment { author, .. } => format!("{:?}", author.relation),
        })
        .collect()
}

/// P2 through the shipped commands (OWL-122): `do` posts a round's
/// questions; `continue` waits for an answer, re-asks what an incomplete one
/// left open, replies to a counter-question and waits again, then checks a complete
/// answer and runs Build to a delivery.
#[test]
fn a_question_round_goes_through_continue_to_a_delivery() {
    let bench = Bench::new(true);
    let (outcome, printed) = bench.run(vec![bench.reply(None, Some(ROUND_1))], None);
    let stop = outcome.expect_err("questions stop the run");
    assert!(
        matches!(&stop, Stop::NeedsInput { posted: Ok(round), questions, .. }
            if round.get() == 1 && questions.len() == 2),
        "{stop:?}\n{printed}"
    );
    assert!(
        stop.to_string().contains("run `owlshift continue DEMO-1`"),
        "{stop}"
    );
    let comments = bench.comments();
    assert_eq!(comments.len(), 1, "{comments:?}");
    assert!(comments[0].starts_with("[owlshift] QUESTIONS · round 1\n"));
    for text in [
        "**Q1** (scope)",
        "**Q2** (scope)",
        "Recommendation: English",
    ] {
        assert!(comments[0].contains(text), "{text}\n{}", comments[0]);
    }
    let record = bench.record();
    assert_eq!(
        (record.state.waiting, record.state.round),
        (Some(Waiting::NeedsInput), 1)
    );
    assert_eq!(record.questions.asks.len(), 1);
    // The ticket shows it waits for its decider (OWL-137).
    assert_eq!(bench.stage(), "Needs Input");
    assert_eq!(
        bench.stage_events(EventKind::TrackerWrite),
        ["In Progress", "Needs Input"]
    );

    // While the questions wait, `do` is refused, and `continue` waits for an
    // answer without running anything.
    let (again, _) = bench.run(Vec::new(), None);
    assert!(
        matches!(&again, Err(Stop::Refused(why)) if why.contains("`owlshift continue DEMO-1`")),
        "{again:?}"
    );
    let (waiting, _) = bench.continue_ticket(Vec::new());
    assert!(
        matches!(&waiting, Err(Stop::Waiting { round: 1, .. })),
        "{waiting:?}"
    );

    // An answer within the quiet window: the decider may still be writing,
    // so nothing runs and nothing is kept until it has been left unedited
    // for 10 minutes, the default (OWL-127).
    let answered_at = bench.clock.get();
    bench.answer("Q1: English.\nQ2: \"Hello, reader.\"\n");
    bench.waited.set(SignedDuration::ZERO);
    let (before, runs) = (bench.record(), bench.briefs().len());
    let (settling, _) = bench.continue_ticket(Vec::new());
    let counts_at = answered_at
        .checked_add(SignedDuration::from_mins(10))
        .unwrap();
    match &settling {
        Err(stop @ Stop::Settling { counts_at: at, .. }) => {
            assert_eq!(*at, counts_at);
            let printed = stop.to_string();
            assert!(
                printed.contains(&format!("at {counts_at}, or at once if it ends with `go`"))
                    && printed.contains("Run `owlshift continue DEMO-1` then."),
                "{printed}"
            );
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(bench.briefs().len(), runs, "nothing ran");
    assert_eq!(bench.record(), before, "nothing was kept");
    assert_eq!(bench.comments().len(), 2);

    // A project's own window (OWL-145): 15 minutes in, a 30-minute window
    // still holds the answer, and says so.
    let config_path = bench.remote.checkout.join("owlshift.toml");
    let original = fs::read_to_string(&config_path).unwrap();
    fs::write(
        &config_path,
        original.replace(
            "always_human = []",
            "always_human = []\nquiet_window_minutes = 30",
        ),
    )
    .unwrap();
    bench.waited.set(SignedDuration::from_mins(15));
    let (longer, _) = bench.continue_ticket(Vec::new());
    match &longer {
        Err(stop @ Stop::Settling { counts_at: at, .. }) => {
            assert_eq!(
                *at,
                answered_at
                    .checked_add(SignedDuration::from_mins(30))
                    .unwrap()
            );
            assert!(stop.to_string().contains("for 30 minutes"), "{stop}");
        }
        other => panic!("{other:?}"),
    }
    fs::write(&config_path, original).unwrap();
    bench.waited.set(SignedDuration::ZERO);

    // At the time it counts, the same answer is checked.
    bench.waited.set(SignedDuration::from_mins(9));

    // An incomplete answer: the RE-ASK asks Q2 alone.
    let partial = check(&[
        ("Q1", "answered", "English, the recommendation."),
        (
            "Q2",
            "partial",
            "The words are given, but not the sign-off.",
        ),
    ]);
    // A person moved the ticket meanwhile: the re-ask puts it back.
    MarkdownTracker::new(&bench.remote.checkout)
        .set_stage(&ticket(), "Todo")
        .unwrap();
    let (reasked, printed) = bench.continue_ticket(vec![bench.reply(None, Some(&partial))]);
    match reasked {
        Err(Stop::Reasked {
            round, reask, open, ..
        }) => {
            assert_eq!((round.get(), reask), (1, 1));
            let ids: Vec<&str> = open.iter().map(|(q, _)| q.id.as_str()).collect();
            assert_eq!(ids, ["Q2"]);
        }
        other => panic!("{other:?}\n{printed}"),
    }
    let check_brief = bench.briefs().pop().unwrap();
    assert_eq!(check_brief.role, Role::AnswerCheck);
    assert_eq!(check_brief.permissions.level, PermissionLevel::ReadOnly);
    assert!(check_brief.rules.is_empty());
    assert_eq!(latest_ask(&check_brief), ["Q1", "Q2"]);
    assert_eq!(shape(&check_brief), ["questions", "Decider"]);
    let comments = bench.comments();
    assert_eq!(comments.len(), 3, "{comments:?}");
    let reask = &comments[2];
    assert!(
        reask.starts_with("[owlshift] RE-ASK · round 1\n"),
        "{reask}"
    );
    assert_eq!(bench.stage(), "Needs Input");
    assert!(reask.contains("(re-ask 1 of 3:"), "{reask}");
    assert!(
        reask.contains("Still open (partial): The words are given, but not the sign-off."),
        "{reask}"
    );
    assert!(!reask.contains("**Q1**"), "{reask}");
    let record = bench.record();
    assert_eq!((record.state.round, record.state.reasks), (1, 1));
    assert_eq!(record.questions.asks.len(), 2);

    // A counter-question: a REPLY carries the check's reply, nothing is
    // re-asked or counted, no Build runs, and the next `continue` waits for a
    // newer comment of the decider's. A REPLY the tracker refused keeps
    // nothing of the check, so the next `continue` checks the same answer.
    bench.answer("Q2: what is a sign-off?\n");
    let counter = check(&[("Q2", "counter_question", "Asks what a sign-off is.")]);
    bench.refuse.set(Some("[owlshift] REPLY"));
    let (refused, _) = bench.continue_ticket(vec![bench.reply(None, Some(&counter))]);
    assert!(
        matches!(&refused, Err(Stop::Refused(why)) if why.contains("posting the reply on the ticket")),
        "{refused:?}"
    );
    let record = bench.record();
    assert_eq!(record.questions.asks.len(), 2);
    assert!(record.questions.asks[1].verdicts.is_empty());
    assert_eq!(bench.comments().len(), 4);
    bench.refuse.set(None);
    let (asked_back, _) = bench.continue_ticket(vec![bench.reply(None, Some(&counter))]);
    let stop = asked_back.expect_err("a counter-question waits");
    assert!(
        matches!(&stop, Stop::CounterQuestion { asked, .. } if asked.len() == 1),
        "{stop:?}"
    );
    let printed = stop.to_string();
    assert!(
        printed.contains("the reply is on the ticket (comment")
            && printed.contains("Reply: A sign-off is a closing line. Should there be one?")
            && !printed.contains("does not reply yet"),
        "{printed}"
    );
    let check_brief = bench.briefs().pop().unwrap();
    assert_eq!(check_brief.role, Role::AnswerCheck, "no Build ran");
    assert_eq!(latest_ask(&check_brief), ["Q2"]);
    let comments = bench.comments();
    assert_eq!(comments.len(), 5, "{comments:?}");
    let reply = &comments[4];
    assert!(reply.starts_with("[owlshift] REPLY\n"), "{reply}");
    assert!(
        reply.contains(
            "**Q2** (scope) What should the greeting say, and should it end with a sign-off?\n\
             Reply: A sign-off is a closing line. Should there be one?"
        ),
        "{reply}"
    );
    assert!(reply.contains("`owlshift continue DEMO-1`"), "{reply}");
    let record = bench.record();
    assert_eq!(
        (record.state.waiting, record.state.reasks),
        (Some(Waiting::NeedsInput), 1)
    );
    assert_eq!(record.questions.asks.len(), 2);
    let (waiting, _) = bench.continue_ticket(Vec::new());
    assert!(matches!(&waiting, Err(Stop::Waiting { .. })), "{waiting:?}");

    // A complete answer: the RESUME restates the round, Q1 from the first
    // check and Q2 from the last, and the ticket resumes at Build, which
    // reads the asks and the answers, and delivers. The answer ends with
    // `go`, so it counts at once.
    bench.waited.set(SignedDuration::ZERO);
    bench.answer("Q2: no sign-off, just \"Hello, reader.\" Go.\n");
    let answered = check(&[("Q2", "answered", "No sign-off.")]);
    let (delivered, printed) = bench.continue_ticket(vec![
        bench.reply(None, Some(&answered)),
        bench.reply(Some("Hello"), Some(DONE)),
    ]);
    delivered.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    // The REPLY reads as Owlshift's in the thread, before the answer it
    // led to.
    let mut briefs = bench.briefs();
    let build = briefs.pop().unwrap();
    let last_check = briefs.pop().unwrap();
    assert_eq!(last_check.role, Role::AnswerCheck);
    assert_eq!(
        shape(&last_check),
        [
            "questions",
            "Decider",
            "reask",
            "Decider",
            "Owlshift",
            "Decider"
        ]
    );
    assert_eq!(build.role, Role::Build);
    assert_eq!(
        shape(&build),
        [
            "questions",
            "Decider",
            "reask",
            "Decider",
            "Owlshift",
            "Decider",
            "Owlshift"
        ]
    );
    assert_eq!(latest_ask(&build), ["Q2"]);
    let comments = bench.comments();
    assert_eq!(comments.len(), 8, "{comments:?}");
    let resume = &comments[6];
    assert!(
        resume.starts_with("[owlshift] RESUME · round 1\n"),
        "{resume}"
    );
    for text in [
        "Understood: English, the recommendation.",
        "Understood: No sign-off.",
    ] {
        assert!(resume.contains(text), "{text}\n{resume}");
    }
    assert!(comments[7].starts_with("[owlshift] DELIVERY"));
    let record = bench.record();
    assert_eq!((record.state.waiting, record.state.reasks), (None, 0));
    // Back to working once the answer resumed Build, then in review.
    assert_eq!(
        bench.stage_events(EventKind::TrackerWrite),
        [
            "In Progress",
            "Needs Input",
            "Needs Input",
            "In Progress",
            "In Review"
        ]
    );
    assert_eq!(bench.stage(), "In Review");
    let commands: Vec<Value> = bench
        .events()
        .iter()
        .filter(|event| event.kind == EventKind::Dispatch)
        .map(|event| event.data["command"].clone())
        .collect();
    assert_eq!(commands, [json!("do"), json!("continue")]);

    // Delivered: nothing is left to resume.
    let (done, _) = bench.continue_ticket(Vec::new());
    assert!(
        matches!(&done, Err(Stop::Refused(why)) if why.contains("nothing to continue")),
        "{done:?}"
    );
}

/// A tracker that refuses every stage write stops nothing: the round is
/// asked and kept, the re-ask too, and the answer resumes Build to a
/// delivery. Each refused move is a `warning` event (OWL-137).
#[test]
fn a_stage_the_tracker_refuses_stops_neither_a_round_nor_a_delivery() {
    let bench = Bench::new(true);
    bench.refuse_stage.set(true);
    let (asked, printed) = bench.run(vec![bench.reply(None, Some(ROUND_1))], None);
    assert!(
        matches!(&asked, Err(Stop::NeedsInput { posted: Ok(round), .. }) if round.get() == 1),
        "{asked:?}\n{printed}"
    );
    assert_eq!(bench.record().questions.asks.len(), 1);

    bench.answer("Q1: English.\n");
    let partial = check(&[
        ("Q1", "answered", "English."),
        ("Q2", "unanswered", "Not answered."),
    ]);
    let (reasked, _) = bench.continue_ticket(vec![bench.reply(None, Some(&partial))]);
    assert!(matches!(&reasked, Err(Stop::Reasked { .. })), "{reasked:?}");
    assert_eq!(bench.record().questions.asks.len(), 2);

    bench.answer("Q2: \"Hello, reader.\" Go.\n");
    let answered = check(&[("Q2", "answered", "Hello, reader.")]);
    let (delivered, printed) = bench.continue_ticket(vec![
        bench.reply(None, Some(&answered)),
        bench.reply(Some("Hello"), Some(DONE)),
    ]);
    delivered.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));

    assert_eq!(
        bench.stage_events(EventKind::Warning),
        [
            "In Progress",
            "Needs Input",
            "Needs Input",
            "In Progress",
            "In Review"
        ]
    );
    assert!(bench.stage_events(EventKind::TrackerWrite).is_empty());
    let warning = bench
        .events()
        .into_iter()
        .find(|event| event.kind == EventKind::Warning)
        .unwrap();
    assert_eq!(warning.data["stage"], "working");
    assert!(
        warning.data["reason"]
            .as_str()
            .unwrap()
            .contains("the tracker is down"),
        "{:?}",
        warning.data
    );
    assert_eq!(bench.stage(), "Todo");
}

/// A failed answer check is retried on the same answers; a quarantined one
/// parks the ticket, which `continue` restarts once the project is cleared,
/// checking the same answers again.
#[test]
fn a_failed_check_is_retried_and_a_parked_ticket_restarts_on_continue() {
    let bench = Bench::new(true);
    // Nothing asked yet: nothing to continue, and nothing cloned to find out.
    let (nothing, _) = bench.continue_ticket(Vec::new());
    assert!(
        matches!(&nothing, Err(Stop::Refused(why)) if why.contains("nothing to continue")),
        "{nothing:?}"
    );
    assert!(!bench.dirs().checkout().exists());

    let (asked, _) = bench.run(vec![bench.reply(None, Some(ROUND_1))], None);
    assert!(matches!(asked, Err(Stop::NeedsInput { .. })), "{asked:?}");
    bench.answer("Q1: English.\nQ2: \"Hello, reader.\", no sign-off.\n");

    let failed = r#"{"format":5,"status":"failed","summary":"The thread holds no ask."}"#;
    let (outcome, _) = bench.continue_ticket(vec![bench.reply(None, Some(failed))]);
    assert!(
        matches!(&outcome, Err(Stop::CheckFailed { .. })),
        "{outcome:?}"
    );
    assert_eq!(bench.record().state.failed_runs, 1);

    let answered = check(&[
        ("Q1", "answered", "English."),
        ("Q2", "answered", "\"Hello, reader.\", no sign-off."),
    ]);
    let mut breaking = bench.reply(None, Some(&answered));
    breaking
        .main_checkout
        .insert(RelativePath::new("planted.txt").unwrap(), "x".to_owned());
    let (outcome, _) = bench.continue_ticket(vec![breaking]);
    assert!(
        matches!(
            &outcome,
            Err(Stop::Parked {
                reason: ParkReason::IsolationBreach,
                ..
            })
        ),
        "{outcome:?}"
    );
    assert_eq!(
        bench.record().state.waiting,
        Some(Waiting::ParkedAwaitingInput)
    );
    // The PARKED comment, without what the breach changed: a ticket ref
    // keeps the round, so `continue` restarts it once the project is cleared.
    let comments = bench.comments();
    assert_eq!(comments.len(), 3, "{comments:?}");
    assert!(comments[2].starts_with("[owlshift] PARKED\n"));
    assert!(
        comments[2].contains("Then run `owlshift continue DEMO-1`"),
        "{}",
        comments[2]
    );
    assert!(!comments[2].contains("planted"), "{}", comments[2]);

    // A person looked and cleared the project; `continue` restarts the ticket
    // and checks the same answers, with no new comment.
    fs::remove_file(bench.dirs().unverified_file()).unwrap();
    let (delivered, printed) = bench.continue_ticket(vec![
        bench.reply(None, Some(&answered)),
        bench.reply(Some("Hello"), Some(DONE)),
    ]);
    delivered.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    assert!(
        bench
            .events()
            .iter()
            .any(|event| event.kind == EventKind::Decision
                && event.data.get("restarted") == Some(&json!(true))),
        "{printed}"
    );
    let comments = bench.comments();
    assert_eq!(comments.len(), 5, "{comments:?}");
    assert!(comments[3].starts_with("[owlshift] RESUME · round 1\n"));
}

/// The re-ask limit through the shipped commands: the fourth incomplete
/// answer parks the ticket with a PARKED comment naming what is still open,
/// and a restart checks a new answer, whose RESUME restates each question
/// from the last check that judged it.
#[test]
fn the_reask_limit_parks_the_ticket_until_it_is_answered() {
    let bench = Bench::new(true);
    let (asked, _) = bench.run(vec![bench.reply(None, Some(ROUND_1))], None);
    assert!(matches!(asked, Err(Stop::NeedsInput { .. })), "{asked:?}");
    let partial = |reason: &'static str| ("Q2", "partial", reason);
    let checks = [
        check(&[
            ("Q1", "answered", "English, the recommendation."),
            partial("The ending is missing."),
        ]),
        check(&[partial("Still no ending.")]),
        check(&[partial("Still no ending.")]),
        check(&[partial("The ending is still missing.")]),
    ];
    for (n, result) in checks.iter().enumerate() {
        bench.answer(&format!("Q2: answer {n}.\n"));
        let (outcome, printed) = bench.continue_ticket(vec![bench.reply(None, Some(result))]);
        if n < 3 {
            assert!(
                matches!(&outcome, Err(Stop::Reasked { reask, .. }) if *reask == n as u32 + 1),
                "{outcome:?}\n{printed}"
            );
        } else {
            assert!(
                matches!(
                    &outcome,
                    Err(Stop::Parked {
                        reason: ParkReason::Reasks,
                        unposted: None,
                        ..
                    })
                ),
                "{outcome:?}\n{printed}"
            );
        }
    }
    assert_eq!(
        bench.record().state.waiting,
        Some(Waiting::ParkedAwaitingInput)
    );
    let comments = bench.comments();
    let parked = comments.last().unwrap();
    assert!(parked.starts_with("[owlshift] PARKED\n"), "{parked}");
    for text in [
        "Still open in round 1:",
        "Still open (partial): The ending is still missing.",
        "answer the questions still open here, then run `owlshift continue DEMO-1`",
    ] {
        assert!(parked.contains(text), "{text}\n{parked}");
    }
    assert!(!parked.contains("**Q1**"), "{parked}");

    // Answered at last, but `continue` is typed within the quiet window: the
    // ticket is restarted, which is kept, and the answer waits unread.
    bench.answer("Q2: no sign-off.\n");
    bench.waited.set(SignedDuration::ZERO);
    let checked_through = bench.record().questions.checked_through;
    let (settling, _) = bench.continue_ticket(Vec::new());
    assert!(
        matches!(&settling, Err(Stop::Settling { .. })),
        "{settling:?}"
    );
    let record = bench.record();
    assert_eq!(record.state.waiting, Some(Waiting::NeedsInput));
    assert_eq!(record.questions.checked_through, checked_through);

    // Once it counts, `continue` checks it, and the RESUME takes Q1 from the
    // first check.
    bench.waited.set(SignedDuration::from_mins(10));
    let answered = check(&[("Q2", "answered", "No sign-off.")]);
    let (delivered, printed) = bench.continue_ticket(vec![
        bench.reply(None, Some(&answered)),
        bench.reply(Some("Hello"), Some(DONE)),
    ]);
    delivered.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    let comments = bench.comments();
    let resume = &comments[comments.len() - 2];
    assert!(
        resume.starts_with("[owlshift] RESUME · round 1\n"),
        "{resume}"
    );
    for text in [
        "Understood: English, the recommendation.",
        "Understood: No sign-off.",
    ] {
        assert!(resume.contains(text), "{text}\n{resume}");
    }
}

/// A tracker that refuses the RESUME keeps nothing of the check, so the next
/// `continue` checks the same answers again; one that refuses the PARKED
/// leaves the park as it is, and says so.
#[test]
fn a_resume_or_parked_comment_the_tracker_refuses() {
    let bench = Bench::new(true);
    let (asked, _) = bench.run(vec![bench.reply(None, Some(ROUND_1))], None);
    assert!(matches!(asked, Err(Stop::NeedsInput { .. })), "{asked:?}");
    bench.answer("Q1: English.\nQ2: \"Hello, reader.\", no sign-off.\n");
    let answered = check(&[
        ("Q1", "answered", "English."),
        ("Q2", "answered", "\"Hello, reader.\", no sign-off."),
    ]);
    bench.refuse.set(Some("[owlshift] RESUME"));
    let (refused, _) = bench.continue_ticket(vec![bench.reply(None, Some(&answered))]);
    assert!(
        matches!(&refused, Err(Stop::Refused(why)) if why.contains("posting the resume on the ticket")),
        "{refused:?}"
    );
    let record = bench.record();
    assert_eq!(record.state.waiting, Some(Waiting::NeedsInput));
    assert_eq!(record.questions.checked_through, None);
    assert!(record.questions.asks[0].verdicts.is_empty());

    bench.refuse.set(None);
    let (delivered, printed) = bench.continue_ticket(vec![
        bench.reply(None, Some(&answered)),
        bench.reply(Some("Hello"), Some(DONE)),
    ]);
    delivered.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    assert!(bench.comments()[2].starts_with("[owlshift] RESUME · round 1\n"));

    let bench = Bench::new(true);
    bench.refuse.set(Some("[owlshift] PARKED"));
    let blocked = r#"{"format":5,"status":"blocked","summary":"The gate needs network."}"#;
    let (outcome, _) = bench.run(vec![bench.reply(None, Some(blocked))], None);
    let Err(
        stop @ Stop::Parked {
            unposted: Some(_), ..
        },
    ) = &outcome
    else {
        panic!("{outcome:?}");
    };
    assert!(
        stop.to_string()
            .contains("The PARKED comment could not be posted on the ticket"),
        "{stop}"
    );
    assert!(bench.comments().is_empty());
}

/// The project file the remote holds for the zone-owner tests: `docs` is
/// owned by `owner`.
const DOCS_OWNED: &str = "[zones.docs]\nowner = \"owner\"\n";

/// A ticket without an assignee, labelled with the zone `docs`.
const ZONE_LABEL: &str = "labels = [\"zone:docs\"]\n";

/// OWL-124: a ticket without an assignee that declares an owned zone is
/// decided by that zone's owner, from `do` to the delivery. The ask records
/// the owner; the ticket author's comment is no answer, even once the label
/// is gone, since waiting needs the recorded decider alone; the owner's
/// answer is checked; Build waits for the ticket to have a decider again,
/// then delivers.
#[test]
fn a_ticket_without_an_assignee_is_decided_by_the_owner_of_its_zone() {
    let bench = Bench::with("maintainer", ZONE_LABEL, DOCS_OWNED);
    let (outcome, printed) = bench.run(vec![bench.reply(None, Some(ROUND_1))], None);
    assert!(
        matches!(&outcome, Err(Stop::NeedsInput { posted: Ok(_), .. })),
        "{outcome:?}\n{printed}"
    );
    let build = bench.briefs().pop().unwrap();
    assert_eq!(build.decider, "owner");
    assert_eq!(build.zones, ["docs"]);
    assert_eq!(build.ticket.author.relation, Relation::Other);
    let asks = bench.record().questions.asks;
    assert_eq!(
        asks[0].decider,
        AskDecider {
            account: "owner".to_owned(),
            by: DeciderRule::ZoneOwner,
        }
    );

    // The label is removed: waiting needs no decider now, and the ticket
    // author's comment does not start the answer check.
    let labelled = fs::read_to_string(bench.ticket_file()).unwrap();
    fs::write(bench.ticket_file(), labelled.replace(ZONE_LABEL, "")).unwrap();
    bench.answer("Q1: English.\nQ2: \"Hello, reader.\", no sign-off.\n");
    let (waiting, _) = bench.continue_ticket(Vec::new());
    assert!(
        matches!(&waiting, Err(Stop::Waiting { decider, .. }) if decider == "owner"),
        "{waiting:?}"
    );
    let (refused, _) = bench.run(Vec::new(), None);
    assert!(
        matches!(&refused, Err(Stop::Refused(why)) if why.contains("has no decider")),
        "{refused:?}"
    );

    // The owner answers: the check reads their comment as the decider's and
    // the author's as another's. Build, which may ask a new round, waits for
    // a decider now.
    bench.comment_as(
        "owner",
        "Q1: English.\nQ2: \"Hello, reader.\", no sign-off.\n",
    );
    let answered = check(&[
        ("Q1", "answered", "English."),
        ("Q2", "answered", "\"Hello, reader.\", no sign-off."),
    ]);
    let (outcome, printed) = bench.continue_ticket(vec![bench.reply(None, Some(&answered))]);
    assert!(
        matches!(&outcome, Err(Stop::Refused(why))
            if why.contains("DEMO-1 has no decider: the ticket has no assignee")),
        "{outcome:?}\n{printed}"
    );
    let check_brief = bench.briefs().pop().unwrap();
    assert_eq!(check_brief.role, Role::AnswerCheck);
    assert_eq!(check_brief.decider, "owner");
    assert_eq!(shape(&check_brief), ["questions", "Other", "Decider"]);
    assert_eq!(bench.record().state.waiting, None);

    // Labelled again, the ticket resumes at Build, whose brief keeps the
    // owner's answers as instructions, and delivers.
    fs::write(bench.ticket_file(), labelled).unwrap();
    let (delivered, printed) = bench.continue_ticket(vec![bench.reply(Some("Hello"), Some(DONE))]);
    delivered.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    let build = bench.briefs().pop().unwrap();
    assert_eq!(build.role, Role::Build);
    assert_eq!(shape(&build), ["questions", "Other", "Decider", "Owlshift"]);
}

/// The owners are read from the project file the base commit holds: an
/// owner named only in the person's own, unpushed `owlshift.toml` decides
/// nothing.
#[test]
fn a_zone_owner_named_only_in_the_persons_checkout_decides_nothing() {
    let bench = Bench::with("maintainer", ZONE_LABEL, "");
    let file = bench.remote.checkout.join("owlshift.toml");
    let mut text = fs::read_to_string(&file).unwrap();
    text.push_str(DOCS_OWNED);
    fs::write(&file, text).unwrap();
    let (outcome, _) = bench.run(Vec::new(), None);
    assert!(
        matches!(&outcome, Err(Stop::Refused(why))
            if why.contains("DEMO-1 has no decider: the ticket has no assignee and touches no zone with an owner")),
        "{outcome:?}"
    );
    // Refused once the fetch gave the base, before anything ran.
    assert!(bench.dirs().checkout().exists());
    assert!(bench.events().is_empty());
}

/// A Build run's questions: Q1 discoverable (naming), Q2 always human
/// (scope).
const MIXED: &str = r#"{"format":5,"status":"questions",
"summary":"The greeting's file and words are not given.",
"questions":[
  {"id":"Q1","category":"naming","context":"The repository has no greeting yet.",
   "text":"Which file should hold the greeting?"},
  {"id":"Q2","category":"scope","context":"The ticket names neither the words nor the ending.",
   "text":"What should the greeting say?"}]}"#;

/// A Build run's one discoverable question.
const NAMING: &str = r#"{"format":5,"status":"questions",
"summary":"The greeting's file is not named.",
"questions":[
  {"id":"Q1","category":"naming","context":"The repository has no greeting yet.",
   "text":"Which file should hold the greeting?"}]}"#;

/// The resolver decides Q1.
const DECIDED_Q1: &str = r#"{"format":5,"status":"done","summary":"The ticket names the file.",
"resolutions":[{"question":"Q1","outcome":"decided","category":"naming","decision":"GREETING.md, at the root.",
"basis":"The ticket's description."}]}"#;

/// The events of a kind whose data holds `key`.
fn data_of(events: &[Event], kind: EventKind, key: &str) -> Vec<Value> {
    events
        .iter()
        .filter(|event| event.kind == kind && event.data.contains_key(key))
        .map(|event| Value::Object(event.data.clone()))
        .collect()
}

/// OWL-138's first acceptance: a discoverable question is decided and
/// logged in a DECISION comment, and the round holds the always-human one
/// alone, as Q1; the decision reaches the next Build as a `decision` entry
/// and the delivery report lists it.
#[test]
fn a_discoverable_question_is_decided_and_the_round_holds_the_rest() {
    let bench = Bench::new(true);
    let (outcome, printed) = bench.run(
        vec![
            bench.reply(None, Some(MIXED)),
            bench.reply(None, Some(DECIDED_Q1)),
        ],
        None,
    );
    let stop = outcome.expect_err("the always-human question stops the run");
    let Stop::NeedsInput {
        posted: Ok(round),
        questions,
        ..
    } = &stop
    else {
        panic!("{stop:?}\n{printed}");
    };
    assert_eq!(round.get(), 1);
    let asked: Vec<(&str, &str)> = questions
        .iter()
        .map(|q| (q.id.as_str(), q.text.as_str()))
        .collect();
    assert_eq!(asked, [("Q1", "What should the greeting say?")]);

    let comments = bench.comments();
    assert_eq!(comments.len(), 2, "{comments:?}");
    assert!(
        comments[0].starts_with("[owlshift] DECISION\n"),
        "{}",
        comments[0]
    );
    for text in [
        "**Question** (naming) Which file should hold the greeting?",
        "**Decision:** GREETING.md, at the root.",
        "**Settled by:** The ticket's description.",
    ] {
        assert!(comments[0].contains(text), "{text}\n{}", comments[0]);
    }
    let round_1 = &comments[1];
    assert!(
        round_1.starts_with("[owlshift] QUESTIONS · round 1\n"),
        "{round_1}"
    );
    assert!(round_1.contains("**Q1** (scope) What should the greeting say?"));
    assert!(round_1.contains("Owlshift decided one other question of this run itself"));
    assert!(
        !round_1.contains("naming") && !round_1.contains("**Q2**"),
        "{round_1}"
    );

    // The resolver read only the discoverable question, read-only, with the
    // project's rules.
    let briefs = bench.briefs();
    assert_eq!(briefs.len(), 2);
    let resolver = &briefs[1];
    assert_eq!(resolver.role, Role::Resolver);
    assert_eq!(resolver.permissions.level, PermissionLevel::ReadOnly);
    assert!(!resolver.permissions.network);
    assert_eq!(resolver.rules.len(), 1);
    let given: Vec<&str> = resolver.resolve.iter().map(|q| q.text.as_str()).collect();
    assert_eq!(given, ["Which file should hold the greeting?"]);

    let record = bench.record();
    assert_eq!(
        (record.state.waiting, record.state.round),
        (Some(Waiting::NeedsInput), 1)
    );
    assert_eq!(record.questions.decisions.len(), 1);
    assert_eq!(record.questions.asks[0].questions.len(), 1);
    let events = bench.events();
    let resolved = data_of(&events, EventKind::Gate, "resolver");
    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0]["resolver"], "done");
    assert_eq!(resolved[0]["decided"], json!(["Q1"]));
    assert_eq!(resolved[0]["to_decider"], json!(["Q2"]));
    let opened = data_of(&events, EventKind::Gate, "opened");
    assert_eq!(opened[0]["raised_as"], json!(["Q2"]));
    assert_eq!(
        data_of(&events, EventKind::TrackerWrite, "question")[0]["kind"],
        "DECISION"
    );

    // Answered, the ticket resumes: the decision sits before the round in
    // every later brief, and the delivery report lists it.
    bench.answer("Q1: \"Hello, reader.\"\n");
    let answered = check(&[("Q1", "answered", "Hello, reader.")]);
    let (delivered, printed) = bench.continue_ticket(vec![
        bench.reply(None, Some(&answered)),
        bench.reply(Some("Hello"), Some(DONE)),
    ]);
    delivered.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    let build = bench.briefs().pop().unwrap();
    assert_eq!(build.role, Role::Build);
    assert_eq!(
        shape(&build),
        ["decision", "questions", "Decider", "Owlshift"]
    );
    let report = bench.comments().pop().unwrap();
    assert!(report.starts_with("[owlshift] DELIVERY"), "{report}");
    assert!(
        report.contains(
            "**Which file should hold the greeting?**: GREETING.md, at the root. (basis: The \
             ticket's description.)"
        ),
        "{report}"
    );
}

/// OWL-138's second acceptance: a run whose questions are all decided goes
/// on without a Needs Input stop, Build running again in the same command.
/// Its decision is kept in a ticket ref made without any round, so when that
/// Build is cut off, `continue` runs Build again from it, the decision in the
/// brief and in the delivery report.
#[test]
fn questions_all_decided_go_on_without_a_stop() {
    let bench = Bench::new(true);
    let mut limited = bench.reply(None, None);
    limited.usage_limit = Some("2026-10-03T18:00:00Z".parse().unwrap());
    let (outcome, printed) = bench.run(
        vec![
            bench.reply(None, Some(NAMING)),
            bench.reply(None, Some(DECIDED_Q1)),
            limited,
        ],
        None,
    );
    assert!(
        matches!(&outcome, Err(Stop::UsageLimit { .. })),
        "{outcome:?}\n{printed}"
    );
    let roles: Vec<Role> = bench.briefs().iter().map(|b| b.role).collect();
    assert_eq!(roles, [Role::Build, Role::Resolver, Role::Build]);
    let comments = bench.comments();
    assert_eq!(comments.len(), 1, "no round: {comments:?}");
    assert!(comments[0].starts_with("[owlshift] DECISION\n"));
    let record = bench.record();
    assert_eq!((record.state.waiting, record.state.round), (None, 0));
    assert_eq!(record.questions.decisions.len(), 1);
    assert!(record.questions.asks.is_empty());
    assert!(data_of(&bench.events(), EventKind::Gate, "opened").is_empty());

    let (delivered, printed) = bench.continue_ticket(vec![bench.reply(Some("Hello"), Some(DONE))]);
    delivered.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    let build = bench.briefs().pop().unwrap();
    assert_eq!(shape(&build), ["decision"]);
    assert!(matches!(
        &build.thread[0],
        ThreadEntry::Decision { decision, .. } if decision == "GREETING.md, at the root."
    ));
    let report = bench.comments().pop().unwrap();
    assert!(
        report.contains("**Which file should hold the greeting?**"),
        "{report}"
    );
}

/// The questions a stop asked, by text.
fn asked_texts(stop: &Stop) -> Vec<String> {
    match stop {
        Stop::NeedsInput {
            posted: Ok(_),
            questions,
            ..
        } => questions.iter().map(|q| q.text.clone()).collect(),
        other => panic!("{other:?}"),
    }
}

/// A resolver at its usage limit leaves no decision: every question goes to
/// the decider in a normal round, which says why. A DECISION the tracker
/// refuses is never applied: its question goes to the decider too.
#[test]
fn without_a_logged_decision_every_question_goes_to_the_decider() {
    let bench = Bench::new(true);
    let mut limited = bench.reply(None, None);
    limited.usage_limit = Some("2026-10-03T18:00:00Z".parse().unwrap());
    let (outcome, _) = bench.run(vec![bench.reply(None, Some(MIXED)), limited], None);
    let stop = outcome.expect_err("a round");
    assert_eq!(asked_texts(&stop).len(), 2);
    let comments = bench.comments();
    assert_eq!(comments.len(), 1, "{comments:?}");
    assert!(
        comments[0].contains("Owlshift's resolver reached its usage limit"),
        "{}",
        comments[0]
    );
    let record = bench.record();
    assert_eq!((record.state.round, record.state.reasks), (1, 0));
    let resolved = data_of(&bench.events(), EventKind::Gate, "resolver");
    assert_eq!(resolved[0]["resolver"], "usage_limit");

    let bench = Bench::new(true);
    bench.refuse.set(Some("[owlshift] DECISION"));
    let (outcome, _) = bench.run(
        vec![
            bench.reply(None, Some(MIXED)),
            bench.reply(None, Some(DECIDED_Q1)),
        ],
        None,
    );
    let stop = outcome.expect_err("a round");
    assert_eq!(
        asked_texts(&stop),
        [
            "Which file should hold the greeting?",
            "What should the greeting say?"
        ]
    );
    let comments = bench.comments();
    assert_eq!(comments.len(), 1, "{comments:?}");
    assert!(!comments[0].contains("decided"), "{}", comments[0]);
    assert!(bench.record().questions.decisions.is_empty());
    let resolved = data_of(&bench.events(), EventKind::Gate, "resolver");
    assert_eq!(resolved[0]["unposted"], json!(["Q1"]));
}

/// OWL-144's acceptance: Build files a data-loss question under another
/// category; the resolver, which never sees Build's categories, decides it
/// but labels it `data_loss`, and the runner refuses that decision: no
/// DECISION, nothing kept, and the question goes to the decider beside the
/// one passed on, while the other decision of the same result stands.
#[test]
fn a_decision_the_resolver_labels_always_human_goes_to_the_decider() {
    let raised = r#"{"format":5,"status":"questions",
"summary":"The greeting's file, the old greetings and the tone are open.",
"questions":[
  {"id":"Q1","category":"naming","context":"The repository has no greeting yet.",
   "text":"Which file should hold the greeting?"},
  {"id":"Q2","category":"cleanup","context":"OLD_GREETINGS.md keeps every greeting sent.",
   "text":"May the old greetings file be deleted?"},
  {"id":"Q3","category":"tone","context":"The ticket does not say how formal it is.",
   "text":"How formal should the greeting be?"}]}"#;
    let resolved = r#"{"format":5,"status":"done","summary":"The file is named; the rest is not mine.",
"resolutions":[
  {"question":"Q1","outcome":"decided","category":"naming",
   "decision":"GREETING.md, at the root.","basis":"The ticket's description."},
  {"question":"Q2","outcome":"decided","category":"data_loss",
   "decision":"Yes, delete it.","basis":"Nothing reads it."},
  {"question":"Q3","outcome":"passed_on","reason":"A matter of tone."}]}"#;
    let bench = Bench::new(true);
    let (outcome, printed) = bench.run(
        vec![
            bench.reply(None, Some(raised)),
            bench.reply(None, Some(resolved)),
        ],
        None,
    );
    let stop = outcome.expect_err("a round");
    assert_eq!(
        asked_texts(&stop),
        [
            "May the old greetings file be deleted?",
            "How formal should the greeting be?"
        ],
        "{printed}"
    );
    let briefs = bench.briefs();
    let given: Vec<&str> = briefs[1].resolve.iter().map(|q| q.id.as_str()).collect();
    assert_eq!(given, ["Q1", "Q2", "Q3"]);

    let comments = bench.comments();
    assert_eq!(comments.len(), 2, "{comments:?}");
    assert!(
        comments[0].starts_with("[owlshift] DECISION\n") && comments[0].contains("GREETING.md"),
        "{}",
        comments[0]
    );
    assert!(
        comments[1].contains("**Q1** (cleanup) May the old greetings file be deleted?"),
        "{}",
        comments[1]
    );
    assert!(
        !comments.iter().any(|c| c.contains("Yes, delete it.")),
        "{comments:?}"
    );
    let record = bench.record();
    let kept: Vec<&str> = record
        .questions
        .decisions
        .iter()
        .map(|d| d.question.id.as_str())
        .collect();
    assert_eq!(kept, ["Q1"]);
    let events = bench.events();
    let resolved = data_of(&events, EventKind::Gate, "resolver");
    assert_eq!(resolved[0]["decided"], json!(["Q1"]));
    assert_eq!(resolved[0]["refused"], json!(["Q2"]));
    assert_eq!(resolved[0]["passed_on"], json!(["Q3"]));
    assert_eq!(resolved[0]["to_decider"], json!(["Q2", "Q3"]));
    let opened = data_of(&events, EventKind::Gate, "opened");
    assert_eq!(opened[0]["raised_as"], json!(["Q2", "Q3"]));
}

/// A Build that keeps asking what the resolver settles runs at most
/// `MAX_RESOLVED_PASSES` times in a row on decisions alone; the next run's
/// questions all go to the decider, and the round says why.
#[test]
fn the_resolver_settles_a_bounded_number_of_runs_in_a_row() {
    let bench = Bench::new(true);
    let mut replies = Vec::new();
    for _ in 0..MAX_RESOLVED_PASSES {
        replies.push(bench.reply(None, Some(NAMING)));
        replies.push(bench.reply(None, Some(DECIDED_Q1)));
    }
    replies.push(bench.reply(None, Some(NAMING)));
    let (outcome, _) = bench.run(replies, None);
    let stop = outcome.expect_err("a round");
    assert_eq!(asked_texts(&stop), ["Which file should hold the greeting?"]);
    let comments = bench.comments();
    let rounds = comments.len() - usize::try_from(MAX_RESOLVED_PASSES).unwrap();
    assert_eq!(rounds, 1, "{comments:?}");
    assert!(
        comments
            .last()
            .unwrap()
            .contains("already settled every question of 3 runs in a row"),
        "{comments:?}"
    );
    let resolved = data_of(&bench.events(), EventKind::Gate, "resolver");
    assert_eq!(resolved.last().unwrap()["resolver"], "pass_limit");
}

/// A resolver that breaks isolation is quarantined: the ticket parks, and
/// none of the run's questions is posted.
#[test]
fn a_resolver_that_breaks_isolation_parks_the_ticket() {
    let bench = Bench::new(true);
    let mut breaking = bench.reply(None, Some(DECIDED_Q1));
    breaking
        .main_checkout
        .insert(RelativePath::new("planted.txt").unwrap(), "x".to_owned());
    let (outcome, _) = bench.run(vec![bench.reply(None, Some(MIXED)), breaking], None);
    assert!(
        matches!(
            &outcome,
            Err(Stop::Parked {
                reason: ParkReason::IsolationBreach,
                ..
            })
        ),
        "{outcome:?}"
    );
    let comments = bench.comments();
    assert_eq!(comments.len(), 1, "{comments:?}");
    assert!(comments[0].starts_with("[owlshift] PARKED\n"));
}
