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

use jiff::tz::TimeZone;
use jiff::{SignedDuration, Timestamp};
use serde_json::{Value, json};
use tempfile::TempDir;

use owlshift_adapters::forge::github::GitHubForge;
use owlshift_adapters::forge::{Repo, Verdict};
use owlshift_adapters::graphql::{Response, RestAnswer, Transport};
use owlshift_adapters::harness::codex;
use owlshift_adapters::notifier::{Error as NotifierError, Notifier};
use owlshift_adapters::tracker::markdown::MarkdownTracker;
use owlshift_adapters::tracker::{self, Capability, Comment, Ticket, Tracker};
use owlshift_contracts::brief::{
    Brief, MAX_CHOICE_BYTES, MAX_RESULT_REFUSAL_BYTES, PermissionLevel, Relation, ThreadEntry,
};
use owlshift_contracts::config::{Admit, ProjectConfig};
use owlshift_contracts::drain::{
    DependencyOutcome, Gesture, GestureKind, ProposalOutcome, TicketIntake,
};
use owlshift_contracts::event::{Event, EventKind};
use owlshift_contracts::ids::{RelativePath, TicketId};
use owlshift_contracts::refs::{AskDecider, PersistedState, Waiting};
use owlshift_contracts::result::RefusedChoice;
use owlshift_contracts::{DelegationLevel, Role, Stage};
use owlshift_core::decider::DeciderRule;
use owlshift_core::routing::RunRouting;
use owlshift_core::state::{ParkReason, Status, TicketState};
use owlshift_core::vocab::{Effort, Tier};
use owlshift_runner::agent_env::AgentEnv;
use owlshift_runner::events::{Data, EventLog, EventSink};
use owlshift_runner::executor::harness::ClaudeHarness;
use owlshift_runner::executor::{
    Git, Harness, HarnessEnd, HarnessError, HarnessRun, RUN_DIR, RunLog,
};
use owlshift_runner::on_demand::{self, CodexReviewer, Delivered, Landed, OnDemand, Stop};
use owlshift_runner::project::{self, ProjectDirs};
use owlshift_runner::quarantine;
use owlshift_runner::routing::{ClaudeRouter, Router};
use owlshift_runner::ticket_ref::{self, TicketRecord};
use owlshift_runner::watch::{POLL_INTERVAL, Watch};
use owlshift_testkit::gh;
use owlshift_testkit::git::{GitEnv, Remote, seed};
use owlshift_testkit::harness::FakeHarness;
use owlshift_testkit::reply::Reply;

const TICKET: &str = "DEMO-1";
const BRANCH: &str = "owlshift/demo-1";
const REPO: &str = "demo/project";

/// GitHub's API as the adapter sees it: at most one open pull request,
/// whose head is its branch's tip on the bare remote, unless `stale_reads`
/// says to report an older head for that many reads. One pull request is
/// live at a time: every read answers about the latest one opened.
struct FakeGitHub {
    env: GitEnv,
    bare: PathBuf,
    open: Mutex<Option<u64>>,
    /// The branch of the latest pull request opened, DEMO-1's until one is:
    /// another ticket's delivers once the first is merged (OWL-254).
    head: Mutex<String>,
    created: Mutex<Vec<Value>>,
    stale_reads: Mutex<u32>,
    /// What watch's read of the pull request finds (OWL-209): its state,
    /// mergeability and its check's conclusion, and the base's.
    followed: Mutex<Followed>,
    /// Watch's reads of the pull request so far.
    follow_reads: Mutex<u32>,
    /// Reads of the base branch's checks so far.
    base_reads: Mutex<u32>,
    /// The status the red job's log answers (OWL-112), and how many times
    /// it was read.
    log: Mutex<(u16, u32)>,
    /// What happens at the next read of a head's checks, once (OWL-238).
    at_checks: Mutex<Option<AtChecks>>,
    /// The reviews submitted on the pull request, as GitHub's GraphQL nodes
    /// give them (OWL-307), and the reads of them so far.
    reviews: Mutex<Vec<Value>>,
    reviews_reads: Mutex<u32>,
    /// How many of the next `OpenPullRequests` reads are answered with
    /// HTTP 401 "Bad credentials", as a token GitHub refused for a moment
    /// (OWL-340), and how many such reads were made, refused or not.
    refuse_open: Mutex<u32>,
    open_reads: Mutex<u32>,
}

/// What happens at the next read of a head's checks.
enum AtChecks {
    /// Someone pushes this commit to the branch just before the read.
    Move(String),
    /// The read fails as a network that is down.
    Fail,
}

/// The red job's log, as GitHub Actions writes it: timestamps, an ANSI
/// escape, the error, then cleanup.
const JOB_LOG: &str = "2026-10-11T09:00:00.0000000Z ##[group]Run git grep -q Hello\n\
    2026-10-11T09:00:01.0000000Z \u{1b}[31merror\u{1b}[0m: GREETING.md says hello on macOS only\n\
    2026-10-11T09:00:01.5000000Z ##[error]Process completed with exit code 1.\n\
    2026-10-11T09:00:02.0000000Z Post job cleanup.\n";

/// The pull request as watch reads it.
#[derive(Clone)]
struct Followed {
    state: &'static str,
    mergeable: &'static str,
    conclusion: &'static str,
    base_conclusion: &'static str,
}

impl Default for Followed {
    fn default() -> Self {
        Self {
            state: "OPEN",
            mergeable: "MERGEABLE",
            conclusion: "SUCCESS",
            base_conclusion: "SUCCESS",
        }
    }
}

impl FakeGitHub {
    fn tip(&self) -> String {
        let head = self.head.lock().unwrap().clone();
        self.env
            .run(&self.bare, &["rev-parse", &format!("refs/heads/{head}")])
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
            let open = self.open.lock().unwrap().is_some()
                && request["variables"]["head"] == json!(*self.head.lock().unwrap());
            let nodes = if open {
                vec![self.pull_request()]
            } else {
                vec![]
            };
            json!({ "repository": { "pullRequests": { "totalCount": nodes.len(), "nodes": nodes } } })
        } else if query.starts_with("query RepositoryId") {
            json!({ "repository": { "id": "R_1" } })
        } else if query.starts_with("mutation OpenPullRequest") {
            let input = &request["variables"]["input"];
            self.created.lock().unwrap().push(input.clone());
            *self.head.lock().unwrap() = input["headRefName"].as_str().unwrap().to_owned();
            *self.open.lock().unwrap() = Some(1);
            json!({ "createPullRequest": { "pullRequest": self.pull_request() } })
        } else if query.starts_with("query Follow") {
            *self.follow_reads.lock().unwrap() += 1;
            let followed = self.followed.lock().unwrap().clone();
            let head = self.tip();
            json!({ "repository": { "pullRequest": {
                "state": followed.state, "headRefOid": head, "mergeable": followed.mergeable,
                "mergeStateStatus": "CLEAN",
                "commits": { "nodes": [{ "commit": { "oid": head, "statusCheckRollup": {
                    "contexts": {
                        "totalCount": 1,
                        "pageInfo": { "hasNextPage": false, "endCursor": null },
                        "nodes": [{
                            "__typename": "CheckRun", "databaseId": 2, "name": "ci",
                            "status": "COMPLETED", "conclusion": followed.conclusion,
                            "isRequired": true,
                            "detailsUrl": "https://github.com/demo/project/runs/2",
                            "checkSuite": { "app": { "slug": "github-actions" },
                                            "workflowRun": { "workflow": { "name": "CI" } } },
                        }],
                    },
                } } }] },
            } } })
        } else if query.starts_with("query Reviews") {
            *self.reviews_reads.lock().unwrap() += 1;
            let nodes = self.reviews.lock().unwrap().clone();
            json!({ "repository": { "pullRequest": { "reviews": { "nodes": nodes } } } })
        } else if query.starts_with("query BaseChecks") {
            *self.base_reads.lock().unwrap() += 1;
            let followed = self.followed.lock().unwrap().clone();
            let base = self
                .env
                .run(&self.bare, &["rev-parse", "refs/heads/main"])
                .map(|out| String::from_utf8(out).unwrap().trim().to_owned())
                .unwrap();
            let rollup = json!({ "contexts": {
                "totalCount": 1,
                "pageInfo": { "hasNextPage": false, "endCursor": null },
                "nodes": [{ "__typename": "CheckRun", "name": "ci", "status": "COMPLETED",
                            "conclusion": followed.base_conclusion }],
            } });
            json!({ "repository": {
                "head": { "oid": base, "statusCheckRollup": rollup },
                "atPush": { "oid": base, "statusCheckRollup": rollup },
            } })
        } else if query.starts_with("query Checks") {
            let moved = self.at_checks.lock().unwrap().take();
            if let Some(AtChecks::Move(commit)) = moved {
                let branch = format!("refs/heads/{BRANCH}");
                self.env
                    .run(&self.bare, &["update-ref", &branch, &commit])
                    .unwrap();
            }
            let head = self.tip();
            let conclusion = self.followed.lock().unwrap().conclusion;
            json!({ "repository": { "pullRequest": {
                "state": "OPEN", "headRefOid": head, "mergeable": "MERGEABLE",
                "mergeStateStatus": "CLEAN",
                "commits": { "nodes": [{ "commit": { "oid": head, "statusCheckRollup": {
                    "contexts": {
                        "totalCount": 1,
                        "pageInfo": { "hasNextPage": false, "endCursor": null },
                        "nodes": [{
                            "__typename": "CheckRun", "name": "ci", "status": "COMPLETED",
                            "conclusion": conclusion, "isRequired": true,
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
        let query = request["query"].as_str().unwrap_or_default();
        if query.starts_with("query OpenPullRequests") {
            *self.0.open_reads.lock().unwrap() += 1;
            let mut refuse = self.0.refuse_open.lock().unwrap();
            if *refuse > 0 {
                *refuse -= 1;
                return Ok(Response {
                    status: 401,
                    body: json!({ "message": "Bad credentials" }).to_string(),
                    ..Default::default()
                });
            }
        }
        if query.starts_with("query Checks") {
            let mut at_checks = self.0.at_checks.lock().unwrap();
            if matches!(*at_checks, Some(AtChecks::Fail)) {
                *at_checks = None;
                return Err("the network is down".to_owned());
            }
        }
        let data = self.0.answer(request);
        Ok(Response {
            status: 200,
            body: json!({ "data": data }).to_string(),
            ..Default::default()
        })
    }

    /// The red job's log, the one REST read of the bench.
    fn get(&self, path: &str) -> Result<RestAnswer, String> {
        assert_eq!(path, "repos/demo/project/actions/jobs/2/logs");
        let mut log = self.0.log.lock().unwrap();
        log.1 += 1;
        Ok(match log.0 {
            200 => RestAnswer::new(200, JOB_LOG),
            status => RestAnswer::new(status, r#"{"message":"Resource not accessible"}"#),
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

/// What happens elsewhere while a run of a command works: its number in
/// the command, from 1, and the act, such as a person commenting.
type During = (usize, Box<dyn FnOnce()>);

/// The fake harness, one reply per run in order, with what the agent does
/// first, and what happens while given runs work.
struct Replies {
    program: PathBuf,
    replies: RefCell<VecDeque<PathBuf>>,
    current: RefCell<Option<FakeHarness>>,
    first: RefCell<Option<Agent>>,
    during: RefCell<Vec<During>>,
    started: Cell<usize>,
    /// The routing of each routed run, in order.
    routed: Rc<RefCell<Vec<RunRouting>>>,
}

/// The routed harnesses are the bench's one harness, which records the
/// routing of each run it starts.
impl Router for Replies {
    fn harness(&self, routing: RunRouting) -> Box<dyn Harness + '_> {
        Box::new(Routed(self, routing))
    }
}

/// [`Replies`] given as a routed harness.
struct Routed<'a>(&'a Replies, RunRouting);

impl Harness for Routed<'_> {
    fn command(&self, run: &HarnessRun<'_>) -> Result<Command, HarnessError> {
        self.0.routed.borrow_mut().push(self.1);
        self.0.command(run)
    }

    fn drive(
        &self,
        run: &HarnessRun<'_>,
        child: &mut Child,
        log: &mut RunLog,
    ) -> io::Result<HarnessEnd> {
        self.0.drive(run, child, log)
    }
}

impl Harness for Replies {
    fn command(&self, run: &HarnessRun<'_>) -> Result<Command, HarnessError> {
        if let Some(agent) = self.first.borrow_mut().take() {
            agent(run.worktree);
        }
        // The brief is written: what happens now, the run never reads.
        self.started.set(self.started.get() + 1);
        let mut during = self.during.borrow_mut();
        if let Some(at) = during.iter().position(|(n, _)| *n == self.started.get()) {
            (during.remove(at).1)();
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

/// The verify seats' harness when a scenario plays none of their runs
/// (OWL-304): every run reviews clean, takes no reply of the bench's queue
/// and records no routing, so a scenario about something else keeps the
/// replies it had before Verify had a role. Its runs still show in the
/// event log and the briefs.
struct CleanReview(FakeHarness);

/// A Verify run's `done` with no finding.
const CLEAN_REVIEW: &str =
    r#"{"format":13,"status":"done","summary":"Nothing wrong found.","findings":[]}"#;

impl Router for CleanReview {
    fn harness(&self, _routing: RunRouting) -> Box<dyn Harness + '_> {
        Box::new(self.0.clone())
    }
}

/// The Markdown tracker, posting the runner's comments at a virtual clock.
/// Every comment of the bench, the decider's included, takes the next
/// minute of one clock, so its times read plainly and the quiet window is
/// counted in minutes; `answers_in_the_second_of_their_ask_count` posts
/// several in one second on purpose.
struct Clocked<'a> {
    inner: MarkdownTracker,
    clock: &'a Cell<Timestamp>,
    /// A comment starting with this is refused, as a tracker that fails.
    refuse: &'a Cell<Option<&'static str>>,
    /// The ticket ref's lock is taken once a comment starting with this has
    /// posted, so keeping what that comment asks fails (OWL-196).
    lock_after: &'a Cell<Option<&'static str>>,
    /// The ticket ref's lock file.
    ref_lock: PathBuf,
    /// Every stage write is refused while it is set, read at each write.
    refuse_stage: &'a Cell<bool>,
    /// Every mention is unreadable, as a Linear profile link that cannot be
    /// read.
    refuse_mention: bool,
    /// How many of the next comment reads fail, as a tracker that is down.
    fail_reads: &'a Cell<u32>,
    /// What happens once, right after the next stage read.
    after_stage_read: &'a RefCell<Option<Box<dyn FnOnce()>>>,
    /// How many stage reads were made (OWL-340).
    stage_reads: &'a Cell<u32>,
    /// The proposals inbox, when it is on (OWL-253).
    inbox: &'a RefCell<FakeInbox>,
    /// Folding comments, when it is on (OWL-353).
    fold: &'a RefCell<FakeFold>,
    /// Reactions on comments, when they are read (OWL-371).
    reactions: &'a RefCell<FakeReactions>,
}

/// Reactions on the tracker's comments, which the Markdown tracker's files
/// do not hold (OWL-371): read with the comments, each on the comment of
/// its id, and declared only when a test turns them on.
#[derive(Default)]
struct FakeReactions {
    on: bool,
    /// Each reaction with the id of the comment it is on.
    on_comments: Vec<(String, tracker::Reaction)>,
}

/// [`WITHOUT_INBOX`] with reactions read.
const REACTING: &[Capability] = &[
    Capability::ReadTicket,
    Capability::ListAdmitted,
    Capability::Comments,
    Capability::VisibleStage,
    Capability::BlockedBy,
    Capability::Documents,
    Capability::Reactions,
];

/// A tracker that resolves comments, which the Markdown tracker does not
/// (OWL-353): off unless a test turns it on, with the inbox off.
#[derive(Default)]
struct FakeFold {
    on: bool,
    /// The comments resolved, in order.
    resolved: Vec<String>,
    /// The comment whose resolve is refused.
    refuse: Option<String>,
}

/// [`WITHOUT_INBOX`] with comments folded.
const FOLDING: &[Capability] = &[
    Capability::ReadTicket,
    Capability::ListAdmitted,
    Capability::Comments,
    Capability::VisibleStage,
    Capability::BlockedBy,
    Capability::Documents,
    Capability::FoldComments,
];

/// A proposals inbox in place of the Markdown tracker's own (OWL-253,
/// OWL-267), so a test can read what was filed and make the filing fail:
/// off unless a test turns it on, the tracker then having no inbox at all.
#[derive(Default)]
struct FakeInbox {
    on: bool,
    /// The Markdown tracker's own inbox instead of this one (OWL-254).
    markdown: bool,
    /// The tickets filed or already there: id, title, description.
    tickets: Vec<(TicketId, String, String)>,
    /// Every filing is refused with this.
    refuse: Option<&'static str>,
}

/// The Markdown tracker's capabilities without its proposals inbox.
const WITHOUT_INBOX: &[Capability] = &[
    Capability::ReadTicket,
    Capability::ListAdmitted,
    Capability::Comments,
    Capability::VisibleStage,
    Capability::BlockedBy,
    Capability::Documents,
];

/// [`WITHOUT_INBOX`] is the Markdown tracker's list less the inbox alone,
/// and [`FOLDING`] that list with the folding the Markdown tracker lacks.
#[test]
fn the_bench_without_an_inbox_keeps_every_other_capability() {
    let mut expected = MarkdownTracker::CAPABILITIES.to_vec();
    expected.retain(|c| *c != Capability::ProposalsInbox);
    assert_eq!(WITHOUT_INBOX, expected);
    assert!(!expected.contains(&Capability::FoldComments));
    expected.push(Capability::FoldComments);
    assert_eq!(FOLDING, expected);
    assert!(!expected.contains(&Capability::Reactions));
    expected.pop();
    expected.push(Capability::Reactions);
    assert_eq!(REACTING, expected);
}

/// The clock's time, then a minute later.
fn tick(clock: &Cell<Timestamp>) -> Timestamp {
    let at = clock.get();
    clock.set(at.checked_add(SignedDuration::from_mins(1)).unwrap());
    at
}

impl Tracker for Clocked<'_> {
    fn capabilities(&self) -> &'static [Capability] {
        let inbox = self.inbox.borrow();
        if self.reactions.borrow().on {
            assert!(
                !inbox.on && !inbox.markdown && !self.fold.borrow().on,
                "no bench reads reactions with an inbox or folding"
            );
            REACTING
        } else if self.fold.borrow().on {
            assert!(!inbox.on && !inbox.markdown, "no bench folds with an inbox");
            FOLDING
        } else if inbox.on || inbox.markdown {
            Tracker::capabilities(&self.inner)
        } else {
            WITHOUT_INBOX
        }
    }

    fn resolve_comment(&self, comment: &str) -> Result<(), tracker::Error> {
        let mut fold = self.fold.borrow_mut();
        if fold.refuse.as_deref() == Some(comment) {
            return Err(tracker::Error::new(
                tracker::ErrorKind::Other,
                "the tracker refused the resolve",
            ));
        }
        fold.resolved.push(comment.to_owned());
        Ok(())
    }

    fn propose(&self, proposal: &tracker::Proposal<'_>) -> Result<tracker::Filed, tracker::Error> {
        if self.inbox.borrow().markdown {
            return Tracker::propose(&self.inner, proposal);
        }
        let mut inbox = self.inbox.borrow_mut();
        if let Some(why) = inbox.refuse {
            return Err(tracker::Error::new(tracker::ErrorKind::Other, why));
        }
        let id = TicketId::new(format!("DEMO-{}", 100 + inbox.tickets.len())).unwrap();
        inbox.tickets.push((
            id.clone(),
            proposal.title.to_owned(),
            proposal.description.to_owned(),
        ));
        Ok(tracker::Filed {
            id,
            state: "Triage".to_owned(),
            problems: Vec::new(),
        })
    }

    fn proposed(
        &self,
        team: Option<&str>,
        title: &str,
        marker: &str,
    ) -> Result<Vec<tracker::Found>, tracker::Error> {
        if self.inbox.borrow().markdown {
            return Tracker::proposed(&self.inner, team, title, marker);
        }
        Ok(self
            .inbox
            .borrow()
            .tickets
            .iter()
            .filter(|(_, t, d)| t.eq_ignore_ascii_case(title) || d.contains(marker))
            .map(|(id, _, description)| tracker::Found {
                id: id.clone(),
                description: description.clone(),
            })
            .collect())
    }

    fn ticket(&self, id: &TicketId) -> Result<Ticket, tracker::Error> {
        Tracker::ticket(&self.inner, id)
    }

    fn publish_document(
        &self,
        id: &TicketId,
        document: Option<&str>,
        title: &str,
        content: &str,
    ) -> Result<tracker::Document, tracker::Error> {
        Tracker::publish_document(&self.inner, id, document, title, content)
    }

    fn comments(&self, id: &TicketId) -> Result<Vec<Comment>, tracker::Error> {
        if self.fail_reads.get() > 0 {
            self.fail_reads.set(self.fail_reads.get() - 1);
            return Err(tracker::Error::new(
                tracker::ErrorKind::Other,
                "the tracker is down",
            ));
        }
        let mut comments = Tracker::comments(&self.inner, id)?;
        for (on, reaction) in &self.reactions.borrow().on_comments {
            if let Some(comment) = comments.iter_mut().find(|comment| comment.id == *on) {
                comment.reactions.push(reaction.clone());
            }
        }
        Ok(comments)
    }

    fn post_comment(&self, id: &TicketId, body: &str) -> Result<Comment, tracker::Error> {
        let other = |e: &dyn std::fmt::Display| {
            tracker::Error::new(tracker::ErrorKind::Other, e.to_string())
        };
        if self
            .refuse
            .get()
            .is_some_and(|start| body.starts_with(start))
        {
            return Err(other(&"the tracker is down"));
        }
        self.inner
            .post_comment(id, MarkdownTracker::AGENT, tick(self.clock), body)
            .map_err(|e| other(&e))?;
        if self
            .lock_after
            .get()
            .is_some_and(|start| body.starts_with(start))
        {
            self.lock_after.set(None);
            fs::create_dir_all(self.ref_lock.parent().unwrap()).unwrap();
            fs::write(&self.ref_lock, "").unwrap();
        }
        // The newest comment is the one just posted.
        Tracker::comments(&self.inner, id)?
            .pop()
            .ok_or_else(|| other(&"the comment just posted is gone"))
    }

    fn stage(&self, id: &TicketId) -> Result<String, tracker::Error> {
        self.stage_reads.set(self.stage_reads.get() + 1);
        let stage = Tracker::stage(&self.inner, id);
        if let Some(after) = self.after_stage_read.borrow_mut().take() {
            after();
        }
        stage
    }

    fn set_stage(&self, id: &TicketId, state: &str) -> Result<(), tracker::Error> {
        if self.refuse_stage.get() {
            return Err(tracker::Error::new(
                tracker::ErrorKind::Other,
                "the tracker is down",
            ));
        }
        Tracker::set_stage(&self.inner, id, state)
    }

    fn gesture(&self, id: &TicketId, admit: &Admit) -> Result<tracker::Gesture, tracker::Error> {
        Tracker::gesture(&self.inner, id, admit)
    }

    fn relations(&self, id: &TicketId) -> Result<tracker::Relations, tracker::Error> {
        Tracker::relations(&self.inner, id)
    }

    fn block(&self, blocker: &TicketId, blocked: &TicketId) -> Result<(), tracker::Error> {
        Tracker::block(&self.inner, blocker, blocked)
    }

    fn admitted(
        &self,
        column: &tracker::Column<'_>,
    ) -> Result<Vec<tracker::Listed>, tracker::Error> {
        Tracker::admitted(&self.inner, column)
    }

    fn state_type(&self, id: &TicketId) -> Result<Option<tracker::StateType>, tracker::Error> {
        Tracker::state_type(&self.inner, id)
    }

    /// A tracker that notifies on a mention, as Linear does (OWL-157).
    fn mention(&self, account: &str) -> Result<Option<String>, tracker::Error> {
        if self.refuse_mention {
            return Err(tracker::Error::new(
                tracker::ErrorKind::Other,
                "the profile link could not be read",
            ));
        }
        Ok(Some(format!("@{account}")))
    }
}

/// How many passes in a row a stage step may make due at once before a
/// bench says watch never settles (OWL-367).
const STEPS_CAP: u32 = 64;

/// The passes in a row a stage step made due at once (OWL-367), which the
/// benches that count idle passes run on without counting.
#[derive(Default)]
struct Steps(u32);

impl Steps {
    /// After a pass whose interval is `interval`: on at once after a step,
    /// else `sleep(interval)`, which counts an idle pass.
    fn idle(&mut self, interval: Duration, sleep: &mut dyn FnMut(Duration) -> bool) -> bool {
        if interval.is_zero() {
            self.0 += 1;
            assert!(
                self.0 < STEPS_CAP,
                "watch stepped {} passes in a row",
                self.0
            );
            return true;
        }
        self.0 = 0;
        sleep(interval)
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
    clock: Rc<Cell<Timestamp>>,
    /// The runner's comments starting with this are refused by the tracker.
    refuse: Cell<Option<&'static str>>,
    /// The ticket ref is locked once a runner comment starting with this has
    /// posted: see [`Clocked::lock_after`].
    lock_after: Cell<Option<&'static str>>,
    /// The tracker refuses every stage write.
    refuse_stage: Cell<bool>,
    /// The tracker cannot give any mention.
    refuse_mention: Cell<bool>,
    /// How many of the next comment reads the tracker refuses.
    fail_reads: Cell<u32>,
    /// What happens once, right after the tracker's next stage read.
    after_stage_read: RefCell<Option<Box<dyn FnOnce()>>>,
    /// How many stage reads the tracker answered (OWL-340).
    stage_reads: Cell<u32>,
    /// How long after the time of the next comment `continue` reads this
    /// machine's clock: by default the quiet window, so the decider's
    /// latest comment, a minute older, counts.
    waited: Cell<SignedDuration>,
    /// The tracker's proposals inbox (OWL-253).
    inbox: RefCell<FakeInbox>,
    /// The tracker's folding of comments (OWL-353).
    fold: RefCell<FakeFold>,
    /// The reactions on the tracker's comments (OWL-371).
    reactions: RefCell<FakeReactions>,
    /// The routing each routed harness was asked for, in order (OWL-305).
    routed: Rc<RefCell<Vec<RunRouting>>>,
    /// The verify seats play from the bench's reply queue, as every other
    /// run does; otherwise they review clean ([`CleanReview`], OWL-304).
    verify_plays: Cell<bool>,
    /// The opt-in Codex reviewer (OWL-348): `None` when the project does
    /// not ask for it; else what its readiness check answers, a seat that
    /// plays from the bench's reply queue when ready.
    codex: RefCell<Option<Result<(), String>>>,
    /// Each line the runner's notifier showed (OWL-368).
    notified: Notified,
}

/// A notifier that records each line instead of showing it.
#[derive(Default)]
struct Notified(RefCell<Vec<String>>);

impl Notifier for Notified {
    fn notify(&self, line: &str) -> Result<(), NotifierError> {
        self.0.borrow_mut().push(line.to_owned());
        Ok(())
    }
}

const DONE: &str = r#"{"format":13,"status":"done","summary":"Added GREETING.md; the gate passes.",
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
        Self::build(author, front, zones, "")
    }

    /// A bench with an assignee whose project names `Parked` as the state a
    /// parked ticket shows (OWL-148).
    fn parking() -> Self {
        Self::build(
            "maintainer",
            "assignee = \"maintainer\"\n",
            "",
            ", parked = \"Parked\"",
        )
    }

    /// A bench for intake (OWL-248): the project admits with the label
    /// `agent`, by `maintainer` alone, and names `Parked` as the parked
    /// state; the ticket's decider is its assignee.
    fn drained() -> Self {
        Self::build_admitting(
            "maintainer",
            "assignee = \"maintainer\"\n",
            "",
            ", parked = \"Parked\"",
            "admit = { label = \"agent\" }\nadmitters = [\"maintainer\"]",
        )
    }

    /// [`Self::with`], with `states` added to the project's tracker states.
    fn build(author: &str, front: &str, zones: &str, states: &str) -> Self {
        Self::build_admitting(author, front, zones, states, "admit = \"delegation\"")
    }

    /// [`Self::build`], with `admit` as the project's admission lines.
    fn build_admitting(author: &str, front: &str, zones: &str, states: &str, admit: &str) -> Self {
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
                "requires = \">=0.0\"\n[tracker]\nkind = \"markdown\"\n{admit}\n\
                 states = {{ ready = \"Todo\", working = \"In Progress\", needs_input = \"Needs Input\", review = \"In Review\"{states} }}\n\
                 [stack]\ngate = [\"git grep -q Hello -- GREETING.md\"]\n\
                 [pipeline]\ndefault = \"trivial\"\nplan_approval = \"never\"\nfindings = \"arbiter\"\n[models]\n[policy]\nalways_human = []\n\
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
            head: Mutex::new(BRANCH.to_owned()),
            created: Mutex::new(Vec::new()),
            stale_reads: Mutex::new(0),
            followed: Mutex::default(),
            follow_reads: Mutex::new(0),
            base_reads: Mutex::new(0),
            log: Mutex::new((200, 0)),
            at_checks: Mutex::new(None),
            reviews: Mutex::new(Vec::new()),
            reviews_reads: Mutex::new(0),
            refuse_open: Mutex::new(0),
            open_reads: Mutex::new(0),
        });
        Self {
            data: tmp.path().join("data"),
            tmp,
            env,
            remote,
            github,
            clock: Rc::new(Cell::new("2026-10-02T09:00:00Z".parse().unwrap())),
            refuse: Cell::new(None),
            lock_after: Cell::new(None),
            refuse_stage: Cell::new(false),
            refuse_mention: Cell::new(false),
            fail_reads: Cell::new(0),
            after_stage_read: RefCell::new(None),
            stage_reads: Cell::new(0),
            waited: Cell::new(SignedDuration::from_mins(10)),
            inbox: RefCell::default(),
            fold: RefCell::default(),
            reactions: RefCell::default(),
            routed: Rc::default(),
            verify_plays: Cell::new(false),
            codex: RefCell::new(None),
            notified: Notified::default(),
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
    /// Adds `lines` to the `[policy]` of the person's project file, which
    /// `owlshift do` reads the policy from (OWL-331).
    fn policy(&self, lines: &str) {
        let path = self.remote.checkout.join("owlshift.toml");
        let text = fs::read_to_string(&path).unwrap();
        fs::write(
            &path,
            text.replacen(
                "always_human = []",
                &format!("always_human = []\n{lines}"),
                1,
            ),
        )
        .unwrap();
    }

    fn run(&self, replies: Vec<Reply>, first: Option<Agent>) -> (Result<Delivered, Stop>, String) {
        self.invoke(false, replies, first, Vec::new())
    }

    /// Runs `owlshift continue DEMO-1` with one reply per run, the answer
    /// check's included.
    fn continue_ticket(&self, replies: Vec<Reply>) -> (Result<Delivered, Stop>, String) {
        self.invoke(true, replies, None, Vec::new())
    }

    /// Runs `owlshift continue DEMO-1` as `continue_ticket`, with `during`
    /// happening while the runs it names work.
    fn continue_during(
        &self,
        replies: Vec<Reply>,
        during: Vec<During>,
    ) -> (Result<Delivered, Stop>, String) {
        self.invoke(true, replies, None, during)
    }

    /// `author` commenting on the ticket while the `run`th run of a command
    /// works, at the next minute of the bench's clock.
    fn comment_during(&self, run: usize, author: &'static str, body: &'static str) -> During {
        let (checkout, clock) = (self.remote.checkout.clone(), self.clock.clone());
        (
            run,
            Box::new(move || {
                MarkdownTracker::new(&checkout)
                    .post_comment(&ticket(), author, tick(&clock), body)
                    .unwrap();
            }),
        )
    }

    /// The lock file a write of the ticket's ref takes.
    fn ref_lock(&self) -> PathBuf {
        self.dirs()
            .checkout()
            .join(".git/refs/owlshift/tickets/DEMO-1.lock")
    }

    /// Holds the ticket ref's lock file in Owlshift's checkout once the
    /// `run`th run of a command works, so that keeping a state after it
    /// fails, whatever comment was posted before (OWL-149). The lock is no
    /// ref, so the isolation check does not see it.
    fn lock_ticket_ref_during(&self, run: usize) -> During {
        let path = self.ref_lock();
        (
            run,
            Box::new(move || {
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(path, "").unwrap();
            }),
        )
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

    /// `author`, an account's name or, with `app`, an app user's, puts
    /// `emoji` on the comment `on`, at the next minute (OWL-371).
    fn react(&self, author: &str, app: bool, on: &str, emoji: &str) {
        let author = if app {
            tracker::Author::Other {
                name: author.to_owned(),
            }
        } else {
            tracker::Author::Account(tracker::Person {
                id: author.to_owned(),
                name: author.to_owned(),
            })
        };
        self.reactions.borrow_mut().on_comments.push((
            on.to_owned(),
            tracker::Reaction {
                emoji: emoji.to_owned(),
                author,
                created_at: tick(&self.clock),
            },
        ));
    }

    /// Takes back every reaction on the comment `on`.
    fn unreact(&self, on: &str) {
        self.reactions
            .borrow_mut()
            .on_comments
            .retain(|(comment, _)| comment != on);
    }

    /// `author` replies to the comment `parent`, at the next minute
    /// (OWL-146).
    fn reply_as(&self, author: &str, parent: &str, body: &str) {
        MarkdownTracker::new(&self.remote.checkout)
            .post_reply(&ticket(), author, tick(&self.clock), parent, body)
            .unwrap();
    }

    /// The decider comments on the ticket in the second of `at`, after the
    /// comments already there, leaving the clock as it is.
    fn answer_at(&self, at: Timestamp, body: &str) {
        MarkdownTracker::new(&self.remote.checkout)
            .post_comment(&ticket(), "maintainer", at, body)
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

    /// Gives DEMO-1, which has no labels yet, `labels`, as a person would.
    fn add_labels(&self, labels: &[&str]) {
        let file = self.ticket_file();
        let text = fs::read_to_string(&file).unwrap();
        let line = format!("labels = {labels:?}\n+++\n");
        fs::write(&file, text.replacen("\n+++\n", &format!("\n{line}"), 1)).unwrap();
    }

    /// The routing of each routed run so far, in order (OWL-305).
    fn routed(&self) -> Vec<RunRouting> {
        self.routed.borrow().clone()
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

    /// Writes `record` as the ticket ref, over the one it holds.
    fn write_record(&self, record: &TicketRecord) {
        let env = self.env.clone();
        let git = Git::with_setup("git", move |command| env.apply(command));
        let checkout = self.dirs().checkout();
        let previous = ticket_ref::read(&git, &checkout, &ticket())
            .unwrap()
            .map(|stored| stored.commit);
        ticket_ref::write(&git, &checkout, &ticket(), record, previous.as_deref()).unwrap();
    }

    /// Another merge into the remote's `main`, writing `files`, from a
    /// clone of its own; returns `main`'s new head.
    fn merge_into_main(&self, files: &[(&str, &str)]) -> String {
        merge_into(&self.env, self.tmp.path(), &self.remote.bare, files)
    }

    /// The ticket's worktree, where its runs work.
    fn worktree(&self) -> PathBuf {
        self.dirs().worktree(&ticket())
    }

    /// The remote's `main`.
    fn remote_main(&self) -> String {
        let out = self
            .env
            .run(&self.remote.bare, &["rev-parse", "refs/heads/main"])
            .unwrap();
        String::from_utf8(out).unwrap().trim().to_owned()
    }

    /// Whether the ticket has a ticket ref.
    fn has_ref(&self) -> bool {
        let env = self.env.clone();
        let git = Git::with_setup("git", move |command| env.apply(command));
        ticket_ref::read(&git, &self.dirs().checkout(), &ticket())
            .unwrap()
            .is_some()
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

    /// The brief of the latest run of `role`.
    fn last_of(&self, role: Role) -> Brief {
        self.briefs()
            .into_iter()
            .rev()
            .find(|brief| brief.role == role)
            .unwrap_or_else(|| panic!("no {role:?} run"))
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
        during: Vec<During>,
    ) -> (Result<Delivered, Stop>, String) {
        self.with_on_demand(replies, first, during, |on_demand, sink| {
            if continuing {
                on_demand.continue_ticket(&ticket(), sink)
            } else {
                on_demand.run(&ticket(), sink)
            }
        })
    }

    /// Runs `owlshift watch` (OWL-152) with one reply per run, `sleep`
    /// called after each idle pass; returns each continue's outcome, watch's
    /// own lines, and the events printed. The passes a stage step makes due
    /// at once (OWL-367) run on without calling `sleep`, so a test counts
    /// the passes after which watch waits: [`Self::watch_passes`] sees
    /// every pass.
    fn watch(
        &self,
        replies: Vec<Reply>,
        sleep: &mut dyn FnMut(Duration) -> bool,
    ) -> (Vec<Result<Delivered, Stop>>, String, String) {
        let mut steps = Steps::default();
        self.watch_passes(replies, &mut |interval| steps.idle(interval, &mut *sleep))
    }

    /// [`Self::watch`] with `sleep` called after every pass, a stage step's
    /// included, with the interval watch waits then (OWL-367).
    fn watch_passes(
        &self,
        replies: Vec<Reply>,
        sleep: &mut dyn FnMut(Duration) -> bool,
    ) -> (Vec<Result<Delivered, Stop>>, String, String) {
        let ((outcomes, lines), printed) =
            self.with_on_demand(replies, None, Vec::new(), |on_demand, sink| {
                let (mut outcomes, mut lines) = (Vec::new(), Vec::new());
                let watch = Watch {
                    on_demand,
                    interval: POLL_INTERVAL,
                };
                watch.run(
                    sink,
                    &mut lines,
                    &mut |_, outcome, _| outcomes.push(outcome.clone()),
                    sleep,
                );
                (outcomes, String::from_utf8(lines).unwrap())
            });
        (outcomes, lines, printed)
    }

    /// The commit the ticket ref points to.
    fn ref_commit(&self) -> String {
        let env = self.env.clone();
        let git = Git::with_setup("git", move |command| env.apply(command));
        ticket_ref::read(&git, &self.dirs().checkout(), &ticket())
            .unwrap()
            .expect("a ticket ref")
            .commit
    }

    /// `act` on the `OnDemand` of this bench, with one reply per run, the
    /// answer check's and the resolver's included, and `during` happening
    /// while the runs it names work; returns what `act` returned and what
    /// was printed.
    fn with_on_demand<R>(
        &self,
        replies: Vec<Reply>,
        first: Option<Agent>,
        during: Vec<During>,
        act: impl FnOnce(&OnDemand<'_>, &mut EventSink<'_>) -> R,
    ) -> (R, String) {
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
            during: RefCell::new(during),
            started: Cell::new(0),
            routed: self.routed.clone(),
        };
        let clean_result = self.tmp.path().join("clean-review.json");
        fs::write(&clean_result, CLEAN_REVIEW).unwrap();
        let clean_reply = self.tmp.path().join("clean-review.toml");
        let reply = Reply {
            result: Some(clean_result),
            ..Reply::default()
        };
        fs::write(&clean_reply, reply.render()).unwrap();
        let clean = CleanReview(FakeHarness {
            program: harness.program.clone(),
            reply: clean_reply,
            readable: Vec::new(),
        });
        let verify: &dyn Router = if self.verify_plays.get() {
            &harness
        } else {
            &clean
        };
        let codex_ready = || self.codex.borrow().clone().unwrap_or(Ok(()));
        let codex = self.codex.borrow().is_some().then_some(CodexReviewer {
            design_review: &harness,
            verify: &harness,
            ready: &codex_ready,
        });
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
            refuse: &self.refuse,
            lock_after: &self.lock_after,
            ref_lock: self.ref_lock(),
            refuse_stage: &self.refuse_stage,
            refuse_mention: self.refuse_mention.get(),
            fail_reads: &self.fail_reads,
            after_stage_read: &self.after_stage_read,
            stage_reads: &self.stage_reads,
            inbox: &self.inbox,
            fold: &self.fold,
            reactions: &self.reactions,
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
            intake: &harness,
            design: &harness,
            design_review: &harness,
            verify,
            codex,
            arbiter: &harness,
            answer_check: &harness,
            resolver: &harness,
            rebase: &harness,
            remote_url: &remote_url,
            config: &config,
            dirs: &dirs,
            head_wait: Duration::ZERO,
            clock: &now,
            notifier: Some(&self.notified),
        };
        let mut out = Vec::new();
        let mut sink = EventSink::new(REPO, EventLog::in_dir(&self.data), &mut out);
        let acted = act(&on_demand, &mut sink);
        let printed = String::from_utf8(out).unwrap();
        assert!(
            harness.replies.borrow().is_empty(),
            "a reply was left unused:\n{printed}"
        );
        assert!(
            harness.during.borrow().is_empty(),
            "a run expected to work did not:\n{printed}"
        );
        (acted, printed)
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

    /// The ticket's document titled `<title> · <ticket>` (OWL-350,
    /// OWL-351), as the Markdown tracker keeps it, its title its first line.
    fn document(&self, title: &str) -> String {
        let dir = self
            .remote
            .checkout
            .join("tickets")
            .join(TICKET)
            .join("documents");
        let heading = format!("# {title} · {TICKET}\n");
        fs::read_dir(&dir)
            .unwrap()
            .map(|entry| fs::read_to_string(entry.unwrap().path()).unwrap())
            .find(|text| text.starts_with(&heading))
            .unwrap_or_else(|| panic!("no document {heading}"))
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

/// OWL-340: the bench's GitHub answers the dispatch's read of the open pull
/// requests with the 401 the live case met, through the real adapter's error
/// mapping, a counted number of times.
#[test]
fn the_bench_forge_refuses_a_counted_number_of_reads_with_a_401() {
    let bench = Bench::new(true);
    *bench.github.refuse_open.lock().unwrap() = 1;
    // Refused before any run starts: no reply is played.
    let (outcome, printed) = bench.run(Vec::new(), None);
    let Err(Stop::Refused(message)) = outcome else {
        panic!("{outcome:?}\n{printed}");
    };
    for text in [
        "GitHub (demo/project)",
        "GitHub refused the token: HTTP 401 Bad credentials",
    ] {
        assert!(message.contains(text), "{text}\n{message}");
    }
    assert_eq!(*bench.github.open_reads.lock().unwrap(), 1);
    assert_eq!(*bench.github.refuse_open.lock().unwrap(), 0);

    let (outcome, printed) = bench.run(vec![bench.reply(Some("Hello"), Some(DONE))], None);
    outcome.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    assert!(*bench.github.open_reads.lock().unwrap() > 1);
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
        "**Decisions taken without a human:** none.",
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
            // The ticket's routing (OWL-305).
            Decision,
            TrackerWrite,
            RunStarted,
            RunEnded,
            // The Verify pass: its one critic, then the pass (OWL-304).
            RunStarted,
            RunEnded,
            Decision,
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

    // The ticket ref keeps the delivery, and the ticket at Watch: watch
    // follows its pull request (OWL-209).
    let kept = bench.record();
    let delivery = kept.delivery.clone().expect("the delivery");
    assert!(delivery.followed());
    assert_eq!(delivery.pull_request.as_ref().unwrap().number, 1);
    assert_eq!(delivery.pushed.as_deref(), Some(tip.as_str()));
    assert_eq!(delivery.base_at_push, Some(bench.remote_main()));
    assert_eq!(delivery.pushing, None);
    assert_eq!(kept.state.stage, owlshift_contracts::Stage::Watch);

    // `do` again is refused while watch follows it, nothing run.
    let (refused, printed) = bench.run(vec![], None);
    match refused {
        Err(Stop::Refused(why)) => assert!(why.contains("pull request #1"), "{why}"),
        other => panic!("{other:?}\n{printed}"),
    }

    // A delivery cut short after its push, before its pull request was
    // kept, is finished by `do`: the checkout is fetched, the push is up to
    // date, and the pull request and its report are found, not made twice,
    // though its checks now read red (OWL-231).
    let mut cut = kept.clone();
    cut.state = owlshift_contracts::refs::PersistedState::parse(
        r#"{"format":3,"stage":"verify","round":0,"reasks":0,"failed_runs":0,"fix_passes":0,"repairs":0}"#,
    )
    .unwrap();
    let mut pushing = owlshift_contracts::refs::TicketDelivery::new(BRANCH, "main");
    pushing.pushing = Some(owlshift_contracts::refs::Pushing {
        commit: tip.clone(),
        kind: owlshift_contracts::refs::PushKind::Delivery,
        base: bench.remote_main(),
        onto: None,
        report: None,
    });
    cut.delivery = Some(pushing);
    bench.write_record(&cut);
    bench.github.followed.lock().unwrap().conclusion = "FAILURE";
    let (again, printed) = bench.run(vec![bench.reply(None, Some(DONE))], None);
    let again = again.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    assert!(!again.opened);
    assert_eq!(again.verdict, Some(Verdict::Red));
    assert_eq!(bench.github.created.lock().unwrap().len(), 1);
    assert_eq!(bench.comments().len(), 1);
    assert!(printed.contains("pushed=up_to_date"), "{printed}");
    assert!(bench.record().delivery.unwrap().followed());
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
    // Two Build runs, then the Verify pass (OWL-304).
    assert_eq!(started.len(), 3);
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

/// OWL-180: a Build run whose `result.json` is refused, here for questions
/// not numbered from Q1 or for decisions of its own, counts as a failed run,
/// and the next Build run of the command is told why. Only that one: the run
/// after it, once the resolver settled its question, and the resolver itself
/// are not. A refusal of the build role's own decisions also holds that run
/// to asking (`decisions_refused`, OWL-186), and it asks: its question
/// reopens the refused choice, which since OWL-342 reaches the resolver,
/// here passed on, as any question does.
#[test]
fn a_refused_result_is_told_to_the_next_build_run_only() {
    let misnumbered = NAMING.replace(r#""id":"Q1""#, r#""id":"Q2""#);
    let decided = DONE.replace(
        r#""pr":"#,
        r#""decisions":[{"question":"Which file","decision":"GREETING.md","basis":"b"}],"pr":"#,
    );
    let bench = Bench::new(true);
    let (outcome, printed) = bench.run(
        vec![
            bench.reply(None, Some(&misnumbered)),
            bench.reply(None, Some(NAMING)),
            bench.reply(None, Some(DECIDED_Q1)),
            bench.reply(Some("Hello"), Some(DONE)),
        ],
        None,
    );
    outcome.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    let told = |bench: &Bench| -> Vec<(Role, bool, bool)> {
        bench
            .briefs()
            .iter()
            .map(|brief| {
                (
                    brief.role,
                    brief.result_refusal.is_some(),
                    brief.decisions_refused,
                )
            })
            .collect()
    };
    let flags = |bench: &Bench| -> Vec<Value> {
        data_of(&bench.events(), EventKind::RunStarted, "result_refusal")
            .iter()
            .map(|data| data["result_refusal"].clone())
            .collect()
    };
    assert_eq!(
        told(&bench),
        [
            (Role::Build, false, false),
            (Role::Build, true, false),
            (Role::Resolver, false, false),
            (Role::Build, false, false),
            (Role::Verify, false, false),
        ]
    );
    assert_eq!(flags(&bench), [false, true, false, false, false]);

    let bench = Bench::new(true);
    let (stopped, printed) = bench.run(
        vec![
            bench.reply(None, Some(&decided)),
            bench.reply(None, Some(&asking_refused(NAMING, 1))),
            bench.reply(None, Some(&passed_on(&["Q1"]))),
        ],
        None,
    );
    assert!(
        matches!(&stopped, Err(Stop::NeedsInput { posted: Ok(_), .. })),
        "{stopped:?}\n{printed}"
    );
    assert_eq!(
        told(&bench),
        [
            (Role::Build, false, false),
            (Role::Build, true, true),
            (Role::Resolver, false, false)
        ]
    );
    assert_eq!(flags(&bench), [false, true, false]);
}

/// OWL-203 and OWL-342: a held retry asks both refused choices, Q1 for the
/// first under a category the resolver may decide and Q2 for the second,
/// filed `scope`, and a third question of its own. Since OWL-342 a question
/// reopening a refused choice reaches the resolver as any does (OWL-222's
/// route is gone): Q1 and Q3 do, Q2's category keeping it the decider's;
/// the resolver decides Q3 in a DECISION comment that shows no choice and
/// passes Q1 on; Q1 and Q2 go to the decider, each showing its choice in the
/// round. The events record it.
#[test]
fn a_reopened_choice_reaches_the_resolver() {
    let bench = Bench::new(true);
    let decided = DONE.replace(
        r#""pr":"#,
        r#""decisions":[{"question":"Which file","decision":"GREETING.md","basis":"b"},
{"question":"Which words","decision":"Hello","basis":"b"}],"pr":"#,
    );
    let asks = format!(
        r#"{},
  {{"id":"Q3","category":"naming","context":"The greeting has no heading yet.",
   "text":"Which heading should the greeting use?","recommendation":{{"why":"The simplest reading of the ticket."}}}}],
"refused_choices_asked":[{{"choice":1,"question":"Q1"}},{{"choice":2,"question":"Q2"}}]}}"#,
        MIXED.strip_suffix("]}").unwrap()
    );
    let decided_q3 = r#"{"format":13,"status":"done","summary":"The ticket names the heading.",
"resolutions":[{"question":"Q1","outcome":"passed_on","reason":"The refused run took it; nothing settles it."},
{"question":"Q3","outcome":"decided","category":"naming","decision":"Hello.",
"basis":"The ticket's description.","basis_kind":"settled"}]}"#;
    let (stopped, printed) = bench.run(
        vec![
            bench.reply(None, Some(&decided)),
            bench.reply(None, Some(&asks)),
            bench.reply(None, Some(decided_q3)),
        ],
        None,
    );
    assert!(
        matches!(&stopped, Err(Stop::NeedsInput { posted: Ok(_), .. })),
        "{stopped:?}\n{printed}"
    );
    let resolve: Vec<String> = bench
        .briefs()
        .iter()
        .filter(|brief| brief.role == Role::Resolver)
        .flat_map(|brief| brief.resolve.iter().map(|q| q.id.as_str().to_owned()))
        .collect();
    assert_eq!(resolve, ["Q1", "Q3"]);

    let comments = bench.comments();
    let [decision, round] = &comments[comments.len() - 2..] else {
        unreachable!()
    };
    let refused = |question: &str, recorded: &str| {
        format!(
            "**Refused choice** (as the asking run lists it): {question}\n\
             Recorded by the refused run: {recorded}"
        )
    };
    assert!(
        decision.starts_with("[owlshift] DECISION")
            && decision.contains("Which heading should the greeting use?")
            && !decision.contains("Refused choice"),
        "{decision}"
    );
    assert!(
        round.starts_with("[owlshift] QUESTIONS · round 1")
            && round.contains(&format!(
                "### Q1 · Which file should hold the greeting?\n\
                 The repository has no greeting yet.\n\n\
                 **Recommendation:** The simplest reading of the ticket.\n\n{}",
                refused("Which file", "GREETING.md")
            ))
            && round.contains(&format!(
                "### Q2 · What should the greeting say?\n\
                 The ticket names neither the words nor the ending.\n\n\
                 **Recommendation:** The simplest reading of the ticket.\n\n{}",
                refused("Which words", "Hello")
            ))
            && !round.contains("### Q3"),
        "{round}"
    );

    let events = bench.events();
    let resolved = data_of(&events, EventKind::Gate, "resolver");
    assert_eq!(resolved.last().unwrap()["given"], json!(["Q1", "Q3"]));
    assert_eq!(resolved.last().unwrap()["asked_by"], json!("build"));
    let opened = data_of(&events, EventKind::Gate, "opened");
    assert_eq!(
        opened.last().unwrap()["reopened"],
        json!([
            { "choice": 1, "question": "Q1", "raised_as": "Q1" },
            { "choice": 2, "question": "Q2", "raised_as": "Q2" }
        ])
    );
    let written = data_of(&events, EventKind::TrackerWrite, "questions");
    assert_eq!(written.len(), 1);
    assert!(!written[0].as_object().unwrap().contains_key("reopened"));
}

/// OWL-192: a refusal of Build's decisions is kept in the ticket ref, made
/// for it, through a usage limit that ends the command and a park: the
/// Build runs of later commands, `continue` and `do` alike, are told why,
/// quoting the choices, and may not end `done`, until one asks and its
/// questions are kept. A told run's refusal is never freed, so the second
/// command parks.
#[test]
fn a_decisions_refusal_is_told_across_commands() {
    let bench = Bench::new(true);
    let decided = DONE.replace(
        r#""pr":"#,
        r#""decisions":[{"question":"Which file","decision":"GREETING.md","basis":"b"}],"pr":"#,
    );
    let mut limited = bench.reply(None, None);
    limited.usage_limit = Some("2026-10-03T18:00:00Z".parse().unwrap());
    let (stopped, printed) = bench.run(vec![bench.reply(None, Some(&decided)), limited], None);
    assert!(
        matches!(&stopped, Err(Stop::UsageLimit { .. })),
        "{stopped:?}\n{printed}"
    );
    let first = bench.record().questions.build_refusal.unwrap();
    assert!(
        first.reason.is_none()
            && first.lists.is_empty()
            && first.choices
                == [RefusedChoice {
                    question: "Which file".to_owned(),
                    recorded: "GREETING.md".to_owned(),
                }],
        "{first:?}"
    );

    let (parked, printed) = bench.continue_ticket(vec![bench.reply(None, Some(DONE))]);
    assert!(
        matches!(
            &parked,
            Err(Stop::Parked {
                reason: ParkReason::FailedRuns,
                ..
            })
        ),
        "{parked:?}\n{printed}"
    );
    let kept = bench.record().questions.build_refusal.unwrap();
    assert!(kept.holds() && kept.reason.is_some());
    assert_eq!(kept.choices, first.choices);
    assert!(
        bench
            .comments()
            .pop()
            .unwrap()
            .contains("run `owlshift continue DEMO-1`")
    );

    // A run that asks heeds the hold only once its round is kept: a post
    // that fails leaves the refusal kept.
    bench.refuse.set(Some("[owlshift] QUESTIONS"));
    let (unposted, printed) = bench.run(
        vec![bench.reply(None, Some(&asking_refused(ROUND_1, 1)))],
        None,
    );
    assert!(
        matches!(&unposted, Err(Stop::NeedsInput { posted: Err(_), .. })),
        "{unposted:?}\n{printed}"
    );
    assert_eq!(bench.record().questions.build_refusal, Some(kept));
    bench.refuse.set(None);

    // Its question reopens the refused choice: it goes to the decider
    // through the resolver since OWL-342, which passes it on.
    let (asked, printed) = bench.run(
        vec![
            bench.reply(None, Some(&asking_refused(NAMING, 1))),
            bench.reply(None, Some(&passed_on(&["Q1"]))),
        ],
        None,
    );
    assert!(
        matches!(&asked, Err(Stop::NeedsInput { posted: Ok(_), .. })),
        "{asked:?}\n{printed}"
    );
    let briefs = bench.briefs();
    let told: Vec<(Role, bool, bool)> = briefs
        .iter()
        .map(|brief| {
            (
                brief.role,
                brief.result_refusal.is_some(),
                brief.decisions_refused,
            )
        })
        .collect();
    assert_eq!(
        told,
        [
            (Role::Build, false, false),
            (Role::Build, true, true),
            (Role::Build, true, true),
            (Role::Build, true, true),
            (Role::Build, true, true),
            (Role::Resolver, false, false),
        ]
    );
    let after_park = briefs[4].result_refusal.as_deref().unwrap();
    assert!(
        after_park.contains("`refused_choices`")
            && after_park.contains("A later result was refused too: "),
        "{after_park}"
    );
    assert_eq!(briefs[4].refused_choices, first.choices);
    assert_eq!(bench.record().questions.build_refusal, None);
}

/// OWL-195, OWL-198: three refusals in a row of results that list choices,
/// across commands, the second listing more than the whole cap of
/// `result_refusal`: the first is freed and the told second parks; after
/// the `continue`, the told run lists choices again. The ticket ref keeps
/// every choice, typed, oldest first, and the next Build run is told each
/// of them in `refused_choices` and held to ask; its `done` is refused and
/// parks, every choice still kept.
#[test]
fn every_refused_choice_is_told_across_commands() {
    let bench = Bench::new(true);
    let listing = |choices: &[String]| {
        let decisions: Vec<String> = choices
            .iter()
            .map(|c| format!(r#"{{"question":"{c}","decision":"{c}: d","basis":"b"}}"#))
            .collect();
        DONE.replace(
            r#""pr":"#,
            &format!(r#""decisions":[{}],"pr":"#, decisions.join(",")),
        )
    };
    let lists = [
        vec!["Which file".to_owned()],
        (1..=5)
            .map(|n| format!("Which tone {n} {}", "x".repeat(600)))
            .collect(),
        vec!["Which case".to_owned()],
    ];
    let parked = |stop: &Result<_, Stop>| {
        matches!(
            stop,
            Err(Stop::Parked {
                reason: ParkReason::FailedRuns,
                ..
            })
        )
    };
    let (stopped, printed) = bench.run(
        vec![
            bench.reply(None, Some(&listing(&lists[0]))),
            bench.reply(None, Some(&listing(&lists[1]))),
        ],
        None,
    );
    assert!(parked(&stopped), "{stopped:?}\n{printed}");
    let (stopped, printed) = bench.continue_ticket(vec![
        bench.reply(None, Some(&listing(&lists[2]))),
        bench.reply(None, Some(DONE)),
    ]);
    assert!(parked(&stopped), "{stopped:?}\n{printed}");
    let kept = bench.record().questions.build_refusal.unwrap();
    assert!(kept.lists.is_empty() && kept.left_out == 0, "{kept:?}");
    assert!(kept.reason.as_deref().unwrap().contains("status is done"));
    let listed = lists.concat();
    assert_eq!(kept.choices.len(), listed.len(), "{kept:?}");
    for (choice, question) in kept.choices.iter().zip(&listed) {
        let recorded = format!("{question}: d");
        if question.len() <= MAX_CHOICE_BYTES {
            assert_eq!((&choice.question, &choice.recorded), (question, &recorded));
        } else {
            let start = &question[..MAX_CHOICE_BYTES - " [cut]".len()];
            assert!(
                choice.question == format!("{start} [cut]") && choice.recorded.starts_with(start),
                "{choice:?}"
            );
        }
    }
    let briefs = bench.briefs();
    let told = briefs.last().unwrap();
    assert!(told.decisions_refused);
    assert_eq!(told.refused_choices, kept.choices);
    let reason = told.result_refusal.as_deref().unwrap();
    assert!(reason.len() <= MAX_RESULT_REFUSAL_BYTES, "{}", reason.len());
    assert!(reason.contains("`refused_choices`"), "{reason}");
}

/// OWL-183: after a failed run, a refused result does not spend the last
/// attempt: the next Build run is told why. That run's own refusal is a
/// plain failed run, which parks the ticket, so refusals never loop.
#[test]
fn a_refused_result_never_spends_the_last_attempt() {
    let bench = Bench::new(true);
    let misnumbered = NAMING.replace(r#""id":"Q1""#, r#""id":"Q2""#);
    let (outcome, printed) = bench.run(
        vec![
            bench.reply(None, None),
            bench.reply(None, Some(&misnumbered)),
            bench.reply(None, Some(&misnumbered)),
        ],
        None,
    );
    let stop = outcome.map(|_| ()).unwrap_err();
    assert!(
        matches!(&stop, Stop::Parked { reason: ParkReason::FailedRuns, detail, .. } if detail.contains("out of order")),
        "{stop:?}\n{printed}"
    );
    let told: Vec<(Role, bool)> = bench
        .briefs()
        .iter()
        .map(|brief| (brief.role, brief.result_refusal.is_some()))
        .collect();
    assert_eq!(
        told,
        [
            (Role::Build, false),
            (Role::Build, false),
            (Role::Build, true)
        ]
    );
}

/// OWL-184: a Build result naming an artifact that is not there is refused
/// as a `result.json` is: after a failed run it does not spend the last
/// attempt, and the next Build run is told which artifact and why.
#[test]
fn a_refused_artifact_is_told_and_never_spends_the_last_attempt() {
    let bench = Bench::new(true);
    let missing_plan = DONE.replace(
        r#""pr":"#,
        r#""artifacts":{"plan":".owlshift/run/plan.md"},"pr":"#,
    );
    let (outcome, printed) = bench.run(
        vec![
            bench.reply(None, None),
            bench.reply(None, Some(&missing_plan)),
            bench.reply(Some("Hello"), Some(DONE)),
        ],
        None,
    );
    outcome.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    let told: Vec<Option<String>> = bench
        .briefs()
        .into_iter()
        .map(|brief| brief.result_refusal)
        .collect();
    assert_eq!(
        told,
        [
            None,
            None,
            Some("artifact `plan`: `.owlshift/run/plan.md` does not exist".to_owned()),
            // The Verify pass (OWL-304).
            None,
        ]
    );
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
    let brief = bench.briefs().remove(0);
    assert_eq!(brief.zones, ["docs", "web/cart"]);
    // OWL-134: the run log says the malformed label did not reach the agent,
    // once per run: the Build's, then the Verify pass's (OWL-304).
    let warnings: Vec<Event> = bench
        .events()
        .into_iter()
        .filter(|event| event.kind == EventKind::Warning)
        .collect();
    assert_eq!(warnings.len(), 2, "{warnings:?}");
    assert_eq!(warnings[1].data["role"], "verify");
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
/// PARKED comment, and the remote's branch moves only when the push itself
/// went through. Since OWL-397 the first run's end makes the ticket ref,
/// which keeps the park, so the PARKED comment says to run `continue`, no
/// longer `do`; a stop before any run, the rules too long, makes none.
#[test]
fn every_stop_before_delivery_leaves_the_ticket_untouched() {
    let blocked = r#"{"format":13,"status":"blocked","summary":"The gate needs network."}"#;
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
            // The Verify pass's `pass` decision, before the push (OWL-304).
            Some(EventKind::Decision),
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
            // Changed by OWL-397: the park is kept in the ref its run made.
            // A quarantine's names both, as it always did.
            assert!(
                parked.contains("`owlshift continue DEMO-1`")
                    && (name == "quarantine" || !parked.contains("`owlshift do DEMO-1`")),
                "{name}: {parked}"
            );
            assert!(!parked.contains("planted"), "{name}: {parked}");
        } else {
            assert!(comments.is_empty(), "{name}: {comments:?}");
        }
        // Every run's cost is kept, whatever the stop (OWL-397).
        let runs = bench.briefs().len();
        if runs > 0 {
            let cost = bench.record().cost.expect("cost.json");
            assert_eq!(cost.runs, u32::try_from(runs).unwrap(), "{name}");
        } else {
            assert!(!bench.has_ref(), "{name}");
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
    let blocked = r#"{"format":13,"status":"blocked","summary":"Waiting."}"#;
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
        bash: false,
    };
    let config_text = fs::read_to_string(bench.remote.checkout.join("owlshift.toml")).unwrap();
    let config = ProjectConfig::parse(&config_text).unwrap();
    let router = ClaudeRouter {
        harness: harness.clone(),
        models: config.models.clone(),
    };
    let tracker = MarkdownTracker::new(&bench.remote.checkout);
    let forge =
        GitHubForge::with_transport(Shared(bench.github.clone()), Repo::parse(REPO).unwrap());
    let dirs = bench.dirs();
    let remote_url = bench.remote.bare.to_string_lossy().into_owned();
    let on_demand = OnDemand {
        executor: &executor,
        tracker: &tracker,
        forge: &forge,
        build: &router,
        intake: &harness,
        design: &router,
        design_review: &router,
        verify: &router,
        codex: None,
        arbiter: &router,
        answer_check: &harness,
        resolver: &router,
        rebase: &router,
        remote_url: &remote_url,
        config: &config,
        dirs: &dirs,
        head_wait: Duration::ZERO,
        clock: &on_demand::system_clock,
        notifier: None,
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

/// OWL-207: a git older than the floor is refused by `do` and by `continue`
/// before the project's lock (so before its folder exists) and before the
/// tracker is read. The tracker here reads a folder that does not exist, so
/// a read would stop with another reason; the stand-in git answers
/// `--version` and leaves a witness for any other command.
#[cfg(unix)]
#[test]
fn a_git_older_than_the_floor_is_refused_before_the_lock_and_the_tracker() {
    use std::os::unix::fs::PermissionsExt;

    let bench = Bench::new(true);
    let witness = bench.tmp.path().join("other-git-command");
    let script = bench.tmp.path().join("old-git");
    fs::write(
        &script,
        format!(
            "#!/bin/sh\ncase \"$*\" in\n  *--version*) echo 'git version 2.38.5';;\n  *) : > '{}'; exit 1;;\nesac\n",
            witness.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();

    let executor = on_demand::executor(
        Git::new(&script),
        AgentEnv::new(bench.env.agent_parent()).unwrap(),
    );
    let harness = ClaudeHarness {
        program: PathBuf::from("claude"),
        prompt: "# Build".to_owned(),
        model: None,
        effort: None,
        max_budget_usd: None,
        login: None,
        bash: false,
    };
    let config_text = fs::read_to_string(bench.remote.checkout.join("owlshift.toml")).unwrap();
    let config = ProjectConfig::parse(&config_text).unwrap();
    let router = ClaudeRouter {
        harness: harness.clone(),
        models: config.models.clone(),
    };
    let tracker = MarkdownTracker::new(&bench.tmp.path().join("no-such-tracker"));
    let forge =
        GitHubForge::with_transport(Shared(bench.github.clone()), Repo::parse(REPO).unwrap());
    let dirs = bench.dirs();
    let remote_url = bench.remote.bare.to_string_lossy().into_owned();
    let on_demand = OnDemand {
        executor: &executor,
        tracker: &tracker,
        forge: &forge,
        build: &router,
        intake: &harness,
        design: &router,
        design_review: &router,
        verify: &router,
        codex: None,
        arbiter: &router,
        answer_check: &harness,
        resolver: &router,
        rebase: &router,
        remote_url: &remote_url,
        config: &config,
        dirs: &dirs,
        head_wait: Duration::ZERO,
        clock: &on_demand::system_clock,
        notifier: None,
    };
    for command in ["do", "continue"] {
        let mut out = Vec::new();
        let mut sink = EventSink::new(REPO, EventLog::in_dir(&bench.data), &mut out);
        let outcome = if command == "do" {
            on_demand.run(&ticket(), &mut sink)
        } else {
            on_demand.continue_ticket(&ticket(), &mut sink)
        };
        let Err(Stop::Refused(reason)) = &outcome else {
            panic!("{command}: {outcome:?}");
        };
        assert!(
            reason.contains("git 2.38.5 is older than 2.39.0"),
            "{command}: {reason}"
        );
        assert!(
            fs::symlink_metadata(dirs.root()).is_err(),
            "{command}: the project's folder was created"
        );
        assert!(!witness.exists(), "{command}: a git command ran");
        assert!(bench.events().is_empty());
    }
}

/// The Build run's first question round.
const ROUND_1: &str = r#"{"format":13,"status":"questions",
"summary":"The greeting's language and words are not given.",
"questions":[
  {"id":"Q1","category":"scope","context":"The ticket names no language.",
   "text":"Which language should the greeting use?","options":[{"label":"English"},{"label":"French"}],
   "recommendation":{"option":"A","why":"The repository is in English."}},
  {"id":"Q2","category":"scope","context":"The ticket names neither the words nor the ending.",
   "text":"What should the greeting say, and should it end with a sign-off?","recommendation":{"why":"The simplest reading of the ticket."}}]}"#;

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
    json!({ "format": 13, "status": "done", "summary": "Checked.", "verdicts": verdicts })
        .to_string()
}

/// The ids of a brief's latest ask.
fn latest_ask(brief: &Brief) -> Vec<String> {
    brief
        .latest_ask()
        .map(|(_, questions)| questions.iter().map(|q| q.id.to_string()).collect())
        .unwrap_or_default()
}

/// The ids of a brief's last `answered` round (OWL-359), whose questions the
/// decider answered: no ask is open after it.
fn last_answered(brief: &Brief) -> Vec<String> {
    brief
        .thread
        .iter()
        .rev()
        .find_map(|entry| match entry {
            ThreadEntry::Answered { questions, .. } => {
                Some(questions.iter().map(|q| q.id.to_string()).collect())
            }
            _ => None,
        })
        .unwrap_or_default()
}

/// A brief's thread, an entry per word: `questions`, `reask`, `answered`,
/// `decision`, or the comment author's relation.
fn shape(brief: &Brief) -> Vec<String> {
    brief
        .thread
        .iter()
        .map(|entry| match entry {
            ThreadEntry::Questions { .. } => "questions".to_owned(),
            ThreadEntry::Reask { .. } => "reask".to_owned(),
            ThreadEntry::Answered { .. } => "answered".to_owned(),
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
    // The questions mention their decider at the end of the header line
    // (OWL-157, OWL-324), as do the RE-ASK and the REPLY below.
    let mentioned = " · @maintainer\n";
    assert!(comments[0].starts_with("[owlshift] QUESTIONS · round 1 · @maintainer\n"));
    for text in [
        "### Q1 ·",
        "- **A.** ⭐ English\n- **B.** French",
        "**Recommendation: A.** The repository is in English.",
        "### Q2 ·",
        "Reply `Q1: A`, `ok` to take every recommendation, or in your own words.",
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
    // A person moved the ticket meanwhile: the re-ask puts it back. Another
    // person is its assignee now, until the REPLY below: the RE-ASK and the
    // REPLY still mention the decider the round was asked of.
    MarkdownTracker::new(&bench.remote.checkout)
        .set_stage(&ticket(), "Todo")
        .unwrap();
    let assign = |from: &str, to: &str| {
        let text = fs::read_to_string(bench.ticket_file()).unwrap();
        let changed = text.replace(
            &format!("assignee = \"{from}\""),
            &format!("assignee = \"{to}\""),
        );
        assert_ne!(changed, text);
        fs::write(bench.ticket_file(), changed).unwrap();
    };
    assign("maintainer", "deputy");
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
        reask.starts_with("[owlshift] RE-ASK · round 1 · @maintainer\n"),
        "{reask}"
    );
    assert_eq!(bench.stage(), "Needs Input");
    assert!(reask.contains(mentioned), "{reask}");
    assert!(reask.contains("(re-ask 1 of 3:"), "{reask}");
    assert!(
        reask.contains("**Still open** (partial): The words are given, but not the sign-off."),
        "{reask}"
    );
    assert!(!reask.contains("### Q1"), "{reask}");
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
    assert!(
        reply.starts_with("[owlshift] REPLY · @maintainer\n"),
        "{reply}"
    );
    for text in [
        "### Q2 · What should the greeting say, and should it end with a sign-off?",
        "**Reply:** A sign-off is a closing line. Should there be one?",
    ] {
        assert!(reply.contains(text), "{text}\n{reply}");
    }
    assert!(reply.contains("`owlshift continue DEMO-1`"), "{reply}");
    assert!(reply.contains(mentioned), "{reply}");
    assign("deputy", "maintainer");
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
    // led to. The Verify pass reviews the Build last (OWL-304).
    let mut briefs = bench.briefs();
    assert_eq!(briefs.pop().unwrap().role, Role::Verify);
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
    // The Build reads the answered round as its readings, Q1's from the
    // first check and Q2's from the last, its re-ask folded in (OWL-359).
    assert_eq!(build.role, Role::Build);
    assert_eq!(
        shape(&build),
        [
            "answered", "Decider", "Decider", "Owlshift", "Decider", "Owlshift"
        ]
    );
    assert!(latest_ask(&build).is_empty());
    let Some(ThreadEntry::Answered { questions, .. }) = build.thread.first() else {
        panic!("{:?}", build.thread)
    };
    let understood: Vec<&str> = questions.iter().map(|q| q.understood.as_str()).collect();
    assert_eq!(understood, ["English, the recommendation.", "No sign-off."]);
    let comments = bench.comments();
    assert_eq!(comments.len(), 8, "{comments:?}");
    let resume = &comments[6];
    assert!(
        resume.starts_with("[owlshift] RESUME · round 1\n"),
        "{resume}"
    );
    for text in [
        "**Understood:** English, the recommendation.",
        "**Understood:** No sign-off.",
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

    // Delivered: `continue` reads its pull request once, as watch would
    // (OWL-209), and runs nothing.
    let (done, _) = bench.continue_ticket(Vec::new());
    assert!(
        matches!(&done, Err(Stop::Watched { seen, .. }) if seen.verdict == Verdict::Green),
        "{done:?}"
    );
}

/// A round of two lettered questions, each recommending an option.
const LETTERED: &str = r#"{"format":13,"status":"questions",
"summary":"The greeting's language and ending are not given.",
"questions":[
  {"id":"Q1","category":"scope","context":"The ticket names no language.",
   "text":"Which language should the greeting use?",
   "options":[{"label":"English"},{"label":"French"}],
   "recommendation":{"option":"A","why":"The repository is in English."}},
  {"id":"Q2","category":"scope","context":"The ticket does not say how the greeting ends.",
   "text":"Should the greeting end with a sign-off?",
   "options":[{"label":"Yes","detail":"\"Best regards\" and a name"},{"label":"No"}],
   "recommendation":{"option":"B","why":"A greeting file needs none."}}]}"#;

/// The next round: one lettered question.
const LETTERED_NEXT: &str = r#"{"format":13,"status":"questions",
"summary":"Whether the README links to the greeting is not given.",
"questions":[
  {"id":"Q1","category":"scope","context":"The README lists the project's files.",
   "text":"Should the README link to the greeting?",
   "options":[{"label":"Yes"},{"label":"No"}],
   "recommendation":{"option":"B","why":"The README lists code only."}}]}"#;

/// OWL-324: the decider answers in two characters. `Q1: B` picks an option
/// of a two-question round, a bare `B` the option of the re-ask's one
/// question, and `ok` every recommendation of the next round; each reply
/// reaches the answer check's brief beside the lettered questions it
/// answers, and each round resumes. The fake harness stands in for the
/// check's classification, which the role's prompt states
/// (`roles/answer_check.md`).
#[test]
fn a_two_character_answer_reaches_the_check_and_resumes() {
    let bench = Bench::new(true);
    let (asked, printed) = bench.run(vec![bench.reply(None, Some(LETTERED))], None);
    assert!(
        matches!(&asked, Err(Stop::NeedsInput { posted: Ok(round), .. }) if round.get() == 1),
        "{asked:?}\n{printed}"
    );
    let round = bench.comments().pop().unwrap();
    for text in [
        "### Q1 · Which language should the greeting use?\nThe ticket names no language.\n\n\
         - **A.** ⭐ English\n- **B.** French\n\n**Recommendation: A.** The repository is in \
         English.",
        "- **A.** Yes — \"Best regards\" and a name\n- **B.** ⭐ No\n\n**Recommendation: B.** A \
         greeting file needs none.",
        "---\nReply `Q1: A`, `ok` to take every recommendation, or in your own words.",
    ] {
        assert!(round.contains(text), "{text}\n{round}");
    }
    assert!(!round.contains("(scope)"), "{round}");
    // The last decider comment a check's brief holds, and its latest ask.
    let checked = |bench: &Bench| {
        let brief = bench
            .briefs()
            .into_iter()
            .rfind(|b| b.role == Role::AnswerCheck)
            .unwrap();
        let reply = brief.thread.iter().rev().find_map(|entry| match entry {
            ThreadEntry::Comment { author, body, .. } if author.relation == Relation::Decider => {
                Some(body.clone())
            }
            _ => None,
        });
        let asked: Vec<(String, Option<String>)> = brief
            .latest_ask()
            .unwrap()
            .1
            .iter()
            .map(|q| {
                let letter = q.recommendation.as_ref().and_then(|r| r.option.clone());
                (q.id.to_string(), letter)
            })
            .collect();
        (reply.unwrap(), asked)
    };

    // `Q1: B` settles Q1 and leaves Q2: the RE-ASK asks Q2 alone.
    bench.answer("Q1: B");
    let partial = check(&[
        ("Q1", "answered", "B, French."),
        ("Q2", "unanswered", "Nothing on the sign-off."),
    ]);
    let (reasked, printed) = bench.continue_ticket(vec![bench.reply(None, Some(&partial))]);
    assert!(
        matches!(&reasked, Err(Stop::Reasked { .. })),
        "{reasked:?}\n{printed}"
    );
    assert_eq!(
        checked(&bench),
        (
            "Q1: B".to_owned(),
            vec![
                ("Q1".to_owned(), Some("A".to_owned())),
                ("Q2".to_owned(), Some("B".to_owned()))
            ]
        )
    );
    let reask = bench.comments().pop().unwrap();
    assert!(
        reask.contains("### Q2 · Should the greeting end with a sign-off?")
            && reask.contains("- **B.** ⭐ No")
            && !reask.contains("### Q1"),
        "{reask}"
    );

    // A bare `B` answers the re-ask's one question; Build resumes and asks
    // the next round.
    bench.answer("B");
    let answered = check(&[("Q2", "answered", "B, no sign-off.")]);
    let (next, printed) = bench.continue_ticket(vec![
        bench.reply(None, Some(&answered)),
        bench.reply(None, Some(LETTERED_NEXT)),
    ]);
    assert!(
        matches!(&next, Err(Stop::NeedsInput { posted: Ok(round), .. }) if round.get() == 2),
        "{next:?}\n{printed}"
    );
    assert_eq!(
        checked(&bench),
        (
            "B".to_owned(),
            vec![("Q2".to_owned(), Some("B".to_owned()))]
        )
    );
    let comments = bench.comments();
    let resume = &comments[comments.len() - 2];
    assert!(
        resume.starts_with("[owlshift] RESUME · round 1")
            && resume.contains("**Understood:** B, French.")
            && resume.contains("**Understood:** B, no sign-off."),
        "{resume}"
    );

    // `ok` takes every recommendation of the round, and the work delivers.
    bench.answer("ok");
    let taken = check(&[("Q1", "answered", "No link, the recommendation.")]);
    let (delivered, printed) = bench.continue_ticket(vec![
        bench.reply(None, Some(&taken)),
        bench.reply(Some("Hello"), Some(DONE)),
    ]);
    delivered.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    assert_eq!(
        checked(&bench),
        (
            "ok".to_owned(),
            vec![("Q1".to_owned(), Some("B".to_owned()))]
        )
    );
    let comments = bench.comments();
    let resume = &comments[comments.len() - 2];
    assert!(
        resume.starts_with("[owlshift] RESUME · round 2")
            && resume.contains("**Understood:** No link, the recommendation."),
        "{resume}"
    );
    assert!(comments.last().unwrap().starts_with("[owlshift] DELIVERY"));
}

/// What a round of one question says on a tracker that reads reactions
/// (OWL-371).
const THUMBS_UP_LINE: &str = "A thumbs-up on this comment counts as `ok`.";

/// What the runner understood of the decider's thumbs-up on
/// [`LETTERED_NEXT`] (OWL-371).
const THUMBS_UP_READ: &str = "The decider's thumbs-up on the question: its recommendation, B, No.";

/// OWL-371's acceptance: on a tracker that reads reactions, a round of one
/// question says a thumbs-up counts. An app user's thumbs-up, Owlshift's
/// own, answers nothing. The decider's settles through the quiet window,
/// whose stop offers no `go`; taken back before it counts, it answers
/// nothing. Put again, it counts: the round resumes on the recommendation
/// as `ok` would, with no answer check, and the work delivers. The rule's
/// other cases (another account, another emoji or comment, a round of two)
/// are `answer_check`'s unit tests.
#[test]
fn a_thumbs_up_of_the_decider_answers_a_question_asked_alone() {
    let bench = Bench::new(true);
    bench.reactions.borrow_mut().on = true;
    let (asked, printed) = bench.run(vec![bench.reply(None, Some(LETTERED_NEXT))], None);
    assert!(
        matches!(&asked, Err(Stop::NeedsInput { posted: Ok(round), .. }) if round.get() == 1),
        "{asked:?}\n{printed}"
    );
    let round = bench.comments().pop().unwrap();
    assert!(
        round.contains(&format!(
            "Reply `Q1: B`, `ok` to take every recommendation, or in your own words. \
             {THUMBS_UP_LINE}"
        )),
        "{round}"
    );
    let on = bench.record().questions.asks[0].comment.clone();

    bench.react("owlshift", true, &on, "+1");
    let (waiting, _) = bench.continue_ticket(Vec::new());
    assert!(matches!(&waiting, Err(Stop::Waiting { .. })), "{waiting:?}");

    let waited = bench.waited.get();
    bench.waited.set(SignedDuration::ZERO);
    let reacted_at = bench.clock.get();
    bench.react("maintainer", false, &on, "+1");
    let (settling, _) = bench.continue_ticket(Vec::new());
    match &settling {
        Err(
            stop @ Stop::Settling {
                thumbs_up: true,
                counts_at,
                ..
            },
        ) => {
            assert_eq!(
                *counts_at,
                reacted_at
                    .checked_add(SignedDuration::from_mins(10))
                    .unwrap()
            );
            let printed = stop.to_string();
            assert!(
                printed.contains("answered with a thumbs-up") && !printed.contains("`go`"),
                "{printed}"
            );
        }
        other => panic!("{other:?}"),
    }
    bench.unreact(&on);
    bench.waited.set(waited);
    let (withdrawn, _) = bench.continue_ticket(Vec::new());
    assert!(
        matches!(&withdrawn, Err(Stop::Waiting { .. })),
        "{withdrawn:?}"
    );

    bench.react("maintainer", false, &on, "+1");
    let (delivered, printed) = bench.continue_ticket(vec![bench.reply(Some("Hello"), Some(DONE))]);
    delivered.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    assert!(!bench.roles().contains(&Role::AnswerCheck), "no check ran");
    let comments = bench.comments();
    let resume = &comments[comments.len() - 2];
    assert!(
        resume.starts_with("[owlshift] RESUME · round 1")
            && resume.contains(&format!("**Understood:** {THUMBS_UP_READ}")),
        "{resume}"
    );
    assert!(comments.last().unwrap().starts_with("[owlshift] DELIVERY"));
    // The Build read the round as answered, as after a typed `ok`.
    let build = bench.last_of(Role::Build);
    assert!(
        build.thread.iter().any(|entry| matches!(
            entry,
            ThreadEntry::Answered { questions, .. }
                if questions.iter().any(|q| q.understood == THUMBS_UP_READ)
        )),
        "{:?}",
        build.thread
    );
    let gate: Vec<Event> = bench
        .events()
        .into_iter()
        .filter(|event| {
            event.kind == EventKind::Gate && event.data.get("answer") == Some(&json!("reaction"))
        })
        .collect();
    assert_eq!(gate.len(), 1, "{gate:?}");
    assert_eq!(gate[0].data["answer_check"], json!("answered"));
    assert_eq!(gate[0].run, None, "no run gave it");
}

/// OWL-371: words win. A thumbs-up older than the decider's typed answer is
/// read with it: the answer check runs on the words, and the round resumes
/// on what it understood of them.
#[test]
fn words_win_over_a_thumbs_up() {
    let bench = Bench::new(true);
    bench.reactions.borrow_mut().on = true;
    let (asked, printed) = bench.run(vec![bench.reply(None, Some(LETTERED_NEXT))], None);
    assert!(
        matches!(&asked, Err(Stop::NeedsInput { .. })),
        "{asked:?}\n{printed}"
    );
    let on = bench.record().questions.asks[0].comment.clone();
    bench.react("maintainer", false, &on, "+1");
    bench.answer("Q1: A");
    let taken = check(&[("Q1", "answered", "A, a link.")]);
    let (delivered, printed) = bench.continue_ticket(vec![
        bench.reply(None, Some(&taken)),
        bench.reply(Some("Hello"), Some(DONE)),
    ]);
    delivered.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    let check = bench.last_of(Role::AnswerCheck);
    assert!(
        check.thread.iter().any(|entry| matches!(
            entry,
            ThreadEntry::Comment { author, body, .. }
                if author.relation == Relation::Decider && body == "Q1: A"
        )),
        "{:?}",
        check.thread
    );
    let comments = bench.comments();
    let resume = &comments[comments.len() - 2];
    assert!(
        resume.contains("**Understood:** A, a link.") && !resume.contains(THUMBS_UP_READ),
        "{resume}"
    );
}

/// OWL-136: the comments of one second read in the order they were posted.
/// An answer posted in its ask's second reads after the ask and counts, and
/// so does the decider's next answer, posted in the second of the one the
/// last check read, with that check's REPLY between them.
#[test]
fn answers_in_the_second_of_their_ask_count() {
    let bench = Bench::new(true);
    let (asked, printed) = bench.run(vec![bench.reply(None, Some(ROUND_1))], None);
    assert!(
        matches!(&asked, Err(Stop::NeedsInput { posted: Ok(_), .. })),
        "{asked:?}\n{printed}"
    );
    let second = bench.record().questions.asks[0].at;
    bench.answer_at(second, "Q1: English.\nQ2: what is a sign-off? Go.\n");
    // The REPLY goes in that second too.
    bench.clock.set(second);
    let counter = check(&[
        ("Q1", "answered", "English."),
        ("Q2", "counter_question", "Asks what a sign-off is."),
    ]);
    let (asked_back, printed) = bench.continue_ticket(vec![bench.reply(None, Some(&counter))]);
    assert!(
        matches!(&asked_back, Err(Stop::CounterQuestion { .. })),
        "{asked_back:?}\n{printed}"
    );
    assert_eq!(
        shape(&bench.briefs().pop().unwrap()),
        ["questions", "Decider"]
    );

    bench.answer_at(second, "Q2: no sign-off. Go.\n");
    let answered = check(&[
        ("Q1", "answered", "English."),
        ("Q2", "answered", "No sign-off."),
    ]);
    let (delivered, printed) = bench.continue_ticket(vec![
        bench.reply(None, Some(&answered)),
        bench.reply(Some("Hello"), Some(DONE)),
    ]);
    delivered.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    let mut briefs = bench.briefs();
    // The Verify pass, then the Build (OWL-304).
    briefs.truncate(briefs.len() - 2);
    let last_check = briefs.pop().unwrap();
    assert_eq!(last_check.role, Role::AnswerCheck);
    assert_eq!(
        shape(&last_check),
        ["questions", "Decider", "Owlshift", "Decider"]
    );
}

/// OWL-202's acceptance: a ticket whose ref has no room for its next round
/// stops with the round posted but not kept, naming `owlshift forget`;
/// forgotten, its round count kept, the next `do` asks the round after it
/// and keeps it. Rounds of the size Owlshift asks take some 120 to 350 of
/// them to fill the 1 MiB a ref file may hold (build plan, OWL-202), so the
/// test pads round 1's question instead of asking that many.
#[test]
fn a_ticket_whose_ref_is_full_is_forgotten_and_asks_its_next_round() {
    use owlshift_contracts::Stage;
    use owlshift_contracts::refs::PersistedState;
    use owlshift_runner::forget;
    use owlshift_runner::ticket_ref::Forgot;

    let bench = Bench::new(true);
    let (asked, printed) = bench.run(vec![bench.reply(None, Some(ROUND_1))], None);
    assert!(
        matches!(&asked, Err(Stop::NeedsInput { posted: Ok(_), .. })),
        "{asked:?}\n{printed}"
    );
    // Round 1 was answered and Build resumed: the ticket is at Build, its
    // `questions.json` 300 bytes short of the cap.
    let env = bench.env.clone();
    let git = Git::with_setup("git", move |command| env.apply(command));
    let checkout = bench.dirs().checkout();
    let mut record = bench.record();
    record.state = PersistedState::parse(
        r#"{"format":2,"stage":"build","round":1,"reasks":0,"failed_runs":0}"#,
    )
    .unwrap();
    let short = usize::try_from(ticket_ref::MAX_FILE).unwrap() - 300;
    let pad = short - record.questions.render().len();
    record.questions.asks[0].questions[0]
        .context
        .push_str(&"x".repeat(pad));
    assert_eq!(record.questions.render().len(), short);
    let full = ticket_ref::write(
        &git,
        &checkout,
        &ticket(),
        &record,
        Some(&bench.ref_commit()),
    )
    .unwrap();

    let (stopped, printed) = bench.run(vec![bench.reply(None, Some(ROUND_1))], None);
    let Err(Stop::NotKept {
        landed: Landed::Questions,
        message,
    }) = &stopped
    else {
        panic!("{stopped:?}\n{printed}");
    };
    for part in [
        "the questions of round 2 are on the ticket",
        "past the 1048576 a ticket ref file may hold",
        "`owlshift forget DEMO-1` makes room",
        "once the ref can be written, run `owlshift do DEMO-1`",
    ] {
        assert!(message.contains(part), "{part}\n{message}");
    }
    // The round was not kept; since OWL-397 the Build run's cost was, right
    // after the run, on top of the full ref: the ref moved, by its spend
    // alone.
    let spent = bench.ref_commit();
    assert_ne!(spent, full);
    let kept = bench.record();
    assert_eq!(kept.cost.as_ref().map(|cost| cost.runs), Some(2));
    assert_eq!(
        TicketRecord { cost: None, ..kept },
        TicketRecord {
            cost: None,
            ..record
        }
    );

    let mut out = Vec::new();
    let mut sink = EventSink::new(REPO, EventLog::in_dir(&bench.data), &mut out);
    let forgotten = forget::forget(&git, &bench.dirs(), &ticket(), &mut sink).unwrap();
    assert!(
        matches!(&forgotten.forgot, Forgot::Rewritten { previous, .. } if *previous == spent),
        "{forgotten:?}"
    );
    let kept = bench.record();
    assert_eq!(
        (kept.state.stage, kept.state.waiting, kept.state.round),
        (Stage::Ready, None, 1)
    );
    assert!(kept.questions.asks.is_empty());

    let (asked, printed) = bench.run(vec![bench.reply(None, Some(ROUND_1))], None);
    assert!(
        matches!(&asked, Err(Stop::NeedsInput { posted: Ok(round), .. }) if round.get() == 2),
        "{asked:?}\n{printed}"
    );
    let record = bench.record();
    assert_eq!(
        (record.state.waiting, record.state.round),
        (Some(Waiting::NeedsInput), 2)
    );
    let rounds: Vec<u32> = record
        .questions
        .asks
        .iter()
        .map(|a| a.round.get())
        .collect();
    assert_eq!(rounds, [2]);
    // The round posted while the ref was full stays on the ticket, before
    // the one kept: a residual every round posted but not kept has.
    let posted: Vec<String> = bench
        .comments()
        .iter()
        .filter_map(|body| body.lines().next().map(str::to_owned))
        .collect();
    assert_eq!(
        posted,
        [
            "[owlshift] QUESTIONS · round 1 · @maintainer",
            "[owlshift] QUESTIONS · round 2 · @maintainer",
            "[owlshift] QUESTIONS · round 2 · @maintainer"
        ]
    );
}

/// OWL-152's acceptance: `owlshift watch` resumes a ticket once its
/// decider's answer counts, with no `continue` typed. Its passes wait for
/// the answer, live through a tracker that cannot list the comments, hold a
/// reply within the project's own quiet window, skip a project another
/// command holds and one a person must look at, then continue the ticket
/// to a delivery through `continue`'s path. The marker a cut-off run left,
/// which no command notified, is notified by the first pass that finds it,
/// and the ticket whose answer counts is told once that it waits, over two
/// passes, until `owlshift unquarantine` clears it (OWL-368).
#[test]
fn an_answer_on_the_ticket_resumes_it_under_watch() {
    let bench = Bench::new(true);
    let config_path = bench.remote.checkout.join("owlshift.toml");
    let config = fs::read_to_string(&config_path).unwrap();
    fs::write(
        &config_path,
        config.replace(
            "always_human = []",
            "always_human = []\nquiet_window_minutes = 20",
        ),
    )
    .unwrap();
    let (asked, printed) = bench.run(vec![bench.reply(None, Some(ROUND_1))], None);
    assert!(
        matches!(&asked, Err(Stop::NeedsInput { .. })),
        "{asked:?}\n{printed}"
    );
    let ran = || bench.briefs().len();
    let warnings = || {
        data_of(&bench.events(), EventKind::Warning, "what")
            .iter()
            .filter(|data| data["what"] == "watch_unreadable")
            .count()
    };

    let dirs = bench.dirs();
    let quarantine_told = || {
        let waits = data_of(&bench.events(), EventKind::Decision, "watch")
            .iter()
            .filter(|data| data["watch"] == "waits" && data["reason"] == "quarantine")
            .count();
        let comments = bench
            .comments()
            .iter()
            .filter(|c| c.starts_with("[owlshift] WAITING\n"))
            .count();
        (bench.notified.0.borrow().len(), waits, comments)
    };
    let mut lock = None;
    let mut pass = 0;
    let mut sleep = |interval: Duration| {
        assert_eq!(interval, POLL_INTERVAL);
        pass += 1;
        match pass {
            // No answer yet: nothing ran. The decider answers, a minute
            // before this machine's clock, and the tracker then fails to
            // list the comments once.
            1 => {
                assert_eq!(ran(), 1);
                bench.answer("Q1: English.\nQ2: \"Hello, reader.\"\n");
                bench.waited.set(SignedDuration::ZERO);
                bench.fail_reads.set(1);
            }
            // A warning, and the loop goes on. Eleven minutes after the
            // answer, within the project's 20-minute window.
            2 => {
                assert_eq!(warnings(), 1);
                bench.waited.set(SignedDuration::from_mins(10));
            }
            // Still settling: nothing ran. Past the window now, but another
            // command holds the project.
            3 => {
                assert_eq!(ran(), 1);
                bench.waited.set(SignedDuration::from_mins(20));
                lock = Some(dirs.lock().unwrap().expect("the project is free"));
            }
            // Skipped. Then a run's isolation check left the project
            // unverified.
            4 => {
                assert_eq!(ran(), 1);
                drop(lock.take());
                fs::write(dirs.unverified_file(), "A run was cut off.\n").unwrap();
            }
            // Nothing is continued until a person clears it; the operator
            // is notified once, the ticket told once.
            5 => {
                assert_eq!(ran(), 1);
                assert_eq!(quarantine_told(), (1, 1, 1));
            }
            // The second pass tells nothing more. A person clears it.
            6 => {
                assert_eq!(ran(), 1);
                assert_eq!(quarantine_told(), (1, 1, 1));
                let mut out = Vec::new();
                let mut sink = EventSink::new(REPO, EventLog::in_dir(&bench.data), &mut out);
                let mut ask = |_: &str| Ok(format!("{REPO}\n"));
                let cleared = quarantine::clear(&dirs, &mut ask, &mut sink).unwrap();
                assert!(matches!(cleared, quarantine::Cleared::Cleared { .. }));
            }
            // Continued: the answer check, then, at the next pass, at once
            // (OWL-367), Build and its Verify pass ran (OWL-304). The next
            // pass finds no question waiting.
            7 => assert_eq!(ran(), 4),
            _ => return false,
        }
        true
    };
    let (outcomes, lines, printed) = bench.watch(
        vec![
            bench.reply(
                None,
                Some(&check(&[
                    ("Q1", "answered", "English."),
                    ("Q2", "answered", "Hello, reader."),
                ])),
            ),
            bench.reply(Some("Hello"), Some(DONE)),
        ],
        &mut sleep,
    );
    assert_eq!(pass, 8, "{lines}\n{printed}");
    // The answer check is a stage step of its own (OWL-367).
    match &outcomes[..] {
        [
            Err(Stop::Stepped {
                stage: Stage::Build,
                ..
            }),
            Ok(delivered),
        ] => assert!(delivered.opened, "{delivered:?}"),
        other => panic!("{other:?}\n{lines}\n{printed}"),
    }
    assert_eq!(
        bench.notified.0.borrow()[..],
        [quarantine::found_line(REPO)],
        "{lines}"
    );
    // The answer check, then Build and its Verify pass (OWL-304), after
    // `do`'s Build; one warning only.
    let roles: Vec<Role> = bench.briefs().iter().map(|brief| brief.role).collect();
    assert_eq!(
        roles,
        [Role::Build, Role::AnswerCheck, Role::Build, Role::Verify]
    );
    assert_eq!(warnings(), 1);
    let comments = bench.comments();
    assert!(
        comments
            .iter()
            .any(|c| c.starts_with("[owlshift] RESUME · round 1\n")),
        "{comments:?}"
    );
    assert_eq!(bench.stage(), "In Review");
    // The wait for the quarantine; the continue of the answer, the resume
    // of its Build at the next pass (OWL-367), then, in that pass, watch's
    // first read of the pull request it now follows (OWL-209).
    let decided = data_of(&bench.events(), EventKind::Decision, "watch");
    assert_eq!(decided.len(), 4, "{decided:?}");
    assert_eq!(
        (&decided[0]["watch"], &decided[0]["reason"]),
        (&json!("waits"), &json!("quarantine"))
    );
    assert_eq!(decided[1]["watch"], "continue");
    assert_eq!(
        (&decided[2]["watch"], &decided[2]["stage"]),
        (&json!("continue"), &json!("build"))
    );
    assert_eq!(
        (&decided[3]["watch"], &decided[3]["verdict"]),
        (&json!("read"), &json!("green"))
    );
    assert!(
        lines.contains("watch follows the pull requests of DEMO-1"),
        "{lines}"
    );
    for line in [
        "watch: questions wait for an answer on DEMO-1",
        "watch: another command is working the project",
        "watch: a run's isolation check did not pass",
        "watch: no ticket's questions wait for an answer",
    ] {
        assert!(lines.contains(line), "{line}\n{lines}");
    }
    // Printed when it changed only: at the first pass, and once the project
    // was cleared.
    assert_eq!(
        lines.matches("questions wait for an answer on").count(),
        2,
        "{lines}"
    );
}

/// A round asked by `do` and answered at once, with `go`: the next
/// `continue` checks it (one run) and resumes Build (the second).
fn answered_round(bench: &Bench) -> Reply {
    let (asked, printed) = bench.run(vec![bench.reply(None, Some(ROUND_1))], None);
    assert!(
        matches!(&asked, Err(Stop::NeedsInput { posted: Ok(round), .. }) if round.get() == 1),
        "{asked:?}\n{printed}"
    );
    bench.answer("Q1: English.\nQ2: \"Hello, reader.\" Go.\n");
    bench.reply(
        None,
        Some(&check(&[
            ("Q1", "answered", "English."),
            ("Q2", "answered", "Hello, reader."),
        ])),
    )
}

/// The `outcome` of each `gate` event about late comments, oldest first.
fn late_outcomes(bench: &Bench) -> Vec<Value> {
    bench
        .events()
        .iter()
        .filter(|event| event.kind == EventKind::Gate && event.data.contains_key("late_comments"))
        .map(|event| event.data["outcome"].clone())
        .collect()
}

/// The decider's comments in a brief's thread, by body.
fn decider_says(brief: &Brief) -> Vec<String> {
    brief
        .thread
        .iter()
        .filter_map(|entry| match entry {
            ThreadEntry::Comment { author, body, .. } if author.relation == Relation::Decider => {
                Some(body.trim().to_owned())
            }
            _ => None,
        })
        .collect()
}

/// Scenario S2, step 5 (OWL-139): a decider comment posted while the
/// resumed Build works gets one more Build run, whose work the pull request
/// carries; someone else's comment during that run counts for nothing.
#[test]
fn a_late_comment_gets_one_more_build_run_and_reaches_the_pull_request() {
    let bench = Bench::new(true);
    let check = answered_round(&bench);
    let (delivered, printed) = bench.continue_during(
        vec![
            check,
            bench.reply(Some("Hello"), Some(DONE)),
            bench.reply(Some("Hello and Bonjour"), Some(DONE)),
        ],
        vec![
            bench.comment_during(2, "maintainer", "Also say Bonjour."),
            bench.comment_during(3, "visitor", "Bonjour is French, by the way."),
        ],
    );
    delivered.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));

    let mut briefs = bench.briefs();
    // Verify follows the re-read and the Build it added (OWL-304).
    assert_eq!(briefs.pop().unwrap().role, Role::Verify);
    let integrating = briefs.pop().unwrap();
    let first = briefs.pop().unwrap();
    assert_eq!((first.role, integrating.role), (Role::Build, Role::Build));
    assert!(!decider_says(&first).contains(&"Also say Bonjour.".to_owned()));
    assert_eq!(
        decider_says(&integrating).last().map(String::as_str),
        Some("Also say Bonjour.")
    );
    assert_eq!(late_outcomes(&bench), [json!("integrate")]);
    let pushed = bench
        .env
        .run(
            &bench.remote.bare,
            &["show", &format!("{BRANCH}:GREETING.md")],
        )
        .unwrap();
    assert_eq!(
        String::from_utf8(pushed).unwrap(),
        "Hello and Bonjour, reader.\n"
    );
    assert_eq!(bench.github.created.lock().unwrap().len(), 1);
    let report = bench.comments().pop().unwrap();
    assert!(report.starts_with("[owlshift] DELIVERY"), "{report}");
    assert!(report.contains("Late comment"), "{report}");
    assert_eq!(bench.stage(), "In Review");
}

/// A late comment the integrating run cannot follow without the decider:
/// its questions go through the resolver first (OWL-342), which passes it
/// on, then to the decider in a round that names the late comment, and
/// nothing is delivered.
#[test]
fn a_late_comment_the_build_cannot_follow_stops_in_needs_input() {
    let bench = Bench::new(true);
    let check = answered_round(&bench);
    let asks = r#"{"format":13,"status":"questions",
        "summary":"The greeting cannot say both Hello and Goodbye.",
        "questions":[{"id":"Q1","category":"wording","context":"Q2 was answered Hello; a later comment says Goodbye.",
          "text":"Hello or Goodbye?","options":[{"label":"Hello"},{"label":"Goodbye"}],"recommendation":{"option":"A","why":"The simplest reading of the ticket."}}]}"#;
    let (stopped, printed) = bench.continue_during(
        vec![
            check,
            bench.reply(Some("Hello"), Some(DONE)),
            bench.reply(None, Some(asks)),
            bench.reply(None, Some(&passed_on(&["Q1"]))),
        ],
        vec![bench.comment_during(2, "maintainer", "Actually, make it Goodbye.")],
    );
    assert!(
        matches!(&stopped, Err(Stop::NeedsInput { posted: Ok(round), .. }) if round.get() == 2),
        "{stopped:?}\n{printed}"
    );
    assert_eq!(bench.briefs().last().unwrap().role, Role::Resolver);
    let asked = bench.comments().pop().unwrap();
    assert!(
        asked.starts_with("[owlshift] QUESTIONS · round 2 · @maintainer\n"),
        "{asked}"
    );
    assert!(asked.contains("went to one more Build run"), "{asked}");
    assert!(asked.contains("Hello or Goodbye?"), "{asked}");
    let record = bench.record();
    assert_eq!(
        (record.state.waiting, record.state.round),
        (Some(Waiting::NeedsInput), 2)
    );
    assert_eq!(late_outcomes(&bench), [json!("integrate")]);
    assert_eq!(bench.remote_branch(), None);
    assert!(bench.github.created.lock().unwrap().is_empty());
    assert_eq!(bench.stage(), "Needs Input");
}

/// One Build run per delivery integrates late comments: another comment of
/// the decider during it opens a round of one always-human question naming
/// it, the runner's own and never the resolver's (OWL-342), and nothing is
/// delivered.
#[test]
fn a_comment_after_the_integrating_run_stops_in_needs_input() {
    let bench = Bench::new(true);
    let check = answered_round(&bench);
    let (stopped, printed) = bench.continue_during(
        vec![
            check,
            bench.reply(Some("Hello"), Some(DONE)),
            bench.reply(Some("Hello and Bonjour"), Some(DONE)),
        ],
        vec![
            bench.comment_during(2, "maintainer", "Also say Bonjour."),
            bench.comment_during(3, "maintainer", "And Hola."),
        ],
    );
    match &stopped {
        Err(Stop::NeedsInput {
            posted: Ok(round),
            questions,
            ..
        }) => {
            assert_eq!(round.get(), 2);
            assert_eq!(questions.len(), 1);
            assert_eq!(questions[0].category, "scope");
        }
        other => panic!("{other:?}\n{printed}"),
    }
    assert_eq!(late_outcomes(&bench), [json!("integrate"), json!("ask")]);
    assert!(!bench.briefs().iter().any(|b| b.role == Role::Resolver));
    let asked = bench.comments().pop().unwrap();
    assert!(
        asked.starts_with("[owlshift] QUESTIONS · round 2 · @maintainer\n"),
        "{asked}"
    );
    assert!(
        asked.contains("Should the work take these comments into account"),
        "{asked}"
    );
    let record = bench.record();
    assert_eq!(
        (record.state.waiting, record.state.round),
        (Some(Waiting::NeedsInput), 2)
    );
    assert_eq!(record.questions.asks.len(), 2);
    assert_eq!(bench.remote_branch(), None);
    assert!(bench.github.created.lock().unwrap().is_empty());
    assert_eq!(bench.stage(), "Needs Input");
}

/// A mention the tracker cannot give leaves the decider's QUESTIONS without
/// it, and the run goes on: the round is posted and kept, and a `warning`
/// right after its write, printed on the command's output, tells the
/// operator the tracker may not notify the decider (OWL-170).
#[test]
fn a_mention_the_tracker_cannot_give_is_reported() {
    let bench = Bench::new(true);
    bench.refuse_mention.set(true);
    let (asked, printed) = bench.run(vec![bench.reply(None, Some(ROUND_1))], None);
    assert!(
        matches!(&asked, Err(Stop::NeedsInput { posted: Ok(round), .. }) if round.get() == 1),
        "{asked:?}\n{printed}"
    );
    assert_eq!(bench.record().questions.asks.len(), 1);
    let comments = bench.comments();
    assert!(!comments[0].contains("Waiting for"), "{}", comments[0]);

    assert_mention_dropped_after_write(&bench, "QUESTIONS", &printed);
}

/// Checks that the single `mention_dropped` warning of a run came right
/// after the write of the `kind` comment, naming it, and was printed.
fn assert_mention_dropped_after_write(bench: &Bench, kind: &str, printed: &str) {
    let events = bench.events();
    let written = events
        .iter()
        .position(|e| {
            e.kind == EventKind::TrackerWrite && e.data.get("kind").is_some_and(|k| k == kind)
        })
        .unwrap_or_else(|| panic!("the {kind} comment was written: {events:?}"));
    let warning = &events[written + 1];
    assert_eq!(warning.kind, EventKind::Warning, "{events:?}");
    assert_eq!(warning.data["what"], "mention_dropped");
    assert_eq!(warning.data["kind"], kind);
    assert_eq!(warning.data["comment"], events[written].data["comment"]);
    assert_eq!(warning.data["account"], "maintainer");
    assert!(
        warning.data["reason"]
            .as_str()
            .unwrap()
            .contains("the profile link could not be read"),
        "{warning:?}"
    );
    let dropped = events
        .iter()
        .filter(|e| e.data.get("what").is_some_and(|w| w == "mention_dropped"))
        .count();
    assert_eq!(dropped, 1, "{events:?}");
    assert!(printed.contains("what=mention_dropped"), "{printed}");
}

/// The RE-ASK of an incomplete answer, posted while the tracker cannot give
/// the mention, goes without it, and the round is kept: one `warning` right
/// after the RE-ASK's write tells the operator (OWL-172).
#[test]
fn a_mention_the_tracker_cannot_give_is_reported_on_a_re_ask() {
    let bench = Bench::new(true);
    let (asked, printed) = bench.run(vec![bench.reply(None, Some(ROUND_1))], None);
    assert!(
        matches!(&asked, Err(Stop::NeedsInput { .. })),
        "{asked:?}\n{printed}"
    );
    bench.refuse_mention.set(true);
    bench.answer("Q1: English.\n");
    let partial = check(&[
        ("Q1", "answered", "English."),
        ("Q2", "partial", "The sign-off is not given."),
    ]);
    let (reasked, printed) = bench.continue_ticket(vec![bench.reply(None, Some(&partial))]);
    assert!(
        matches!(&reasked, Err(Stop::Reasked { .. })),
        "{reasked:?}\n{printed}"
    );
    let comments = bench.comments();
    let reask = comments.last().unwrap();
    assert!(reask.starts_with("[owlshift] RE-ASK"), "{reask}");
    assert!(!reask.contains("Waiting for"), "{reask}");
    assert_eq!(bench.record().state.reasks, 1);
    assert_mention_dropped_after_write(&bench, "RE-ASK", &printed);
}

/// The REPLY to a counter-question, posted while the tracker cannot give
/// the mention, goes without it, and the run waits as usual: one `warning`
/// right after the REPLY's write tells the operator (OWL-172).
#[test]
fn a_mention_the_tracker_cannot_give_is_reported_on_a_reply() {
    let bench = Bench::new(true);
    let (asked, printed) = bench.run(vec![bench.reply(None, Some(ROUND_1))], None);
    assert!(
        matches!(&asked, Err(Stop::NeedsInput { .. })),
        "{asked:?}\n{printed}"
    );
    bench.refuse_mention.set(true);
    bench.answer("Q1: English.\nQ2: what is a sign-off?\n");
    let counter = check(&[
        ("Q1", "answered", "English."),
        ("Q2", "counter_question", "Asks what a sign-off is."),
    ]);
    let (asked_back, printed) = bench.continue_ticket(vec![bench.reply(None, Some(&counter))]);
    assert!(
        matches!(&asked_back, Err(Stop::CounterQuestion { .. })),
        "{asked_back:?}\n{printed}"
    );
    let comments = bench.comments();
    let reply = comments.last().unwrap();
    assert!(reply.starts_with("[owlshift] REPLY"), "{reply}");
    assert!(!reply.contains("Waiting for"), "{reply}");
    assert_mention_dropped_after_write(&bench, "REPLY", &printed);
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

    let failed = r#"{"format":13,"status":"failed","summary":"The thread holds no ask."}"#;
    let (outcome, _) = bench.continue_ticket(vec![bench.reply(None, Some(failed))]);
    assert!(
        matches!(&outcome, Err(Stop::CheckFailed { .. })),
        "{outcome:?}"
    );
    assert_eq!(bench.record().state.failed_runs, 1);
    // What a watch pass reads now, before the `continue` below parks it.
    let watched_at = bench.ref_commit();

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
    // A watch that read the ticket before the park never restarts it
    // (OWL-152): the ticket ref is not the one it read, so nothing runs,
    // and nothing records that watch continued it.
    let (runs, events) = (bench.briefs().len(), bench.events().len());
    let (stale, _) = bench.with_on_demand(Vec::new(), None, Vec::new(), |on_demand, sink| {
        let decision = Data::from_iter([("watch".to_owned(), json!("continue"))]);
        on_demand.continue_seen(&ticket(), &watched_at, None, decision, sink)
    });
    assert!(stale.is_none(), "{stale:?}");
    assert_eq!(
        bench.record().state.waiting,
        Some(Waiting::ParkedAwaitingInput)
    );
    assert_eq!(bench.comments().len(), 3);
    assert_eq!((bench.briefs().len(), bench.events().len()), (runs, events));
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

/// OWL-184: an answer check whose result is refused leaves its reason in the
/// ticket ref, through a park, and the next check, in a later command, is
/// told why; a check that gives its verdicts clears it. OWL-190: after a
/// crashed check, the refused one, told of nothing, never spends the last
/// attempt, and its told retry, refused again, parks; `continue` restarts
/// it.
#[test]
fn a_refused_check_is_told_to_the_next_check_across_commands() {
    let bench = Bench::new(true);
    let (asked, _) = bench.run(vec![bench.reply(None, Some(ROUND_1))], None);
    assert!(matches!(asked, Err(Stop::NeedsInput { .. })), "{asked:?}");
    bench.answer("Q1: English.\nQ2: \"Hello, reader.\", no sign-off.\n");

    let (crashed, _) = bench.continue_ticket(vec![bench.reply(None, None)]);
    assert!(
        matches!(&crashed, Err(Stop::CheckFailed { .. })),
        "{crashed:?}"
    );
    let off_the_ask = |id: &'static str| {
        check(&[
            ("Q1", "answered", "English."),
            (id, "answered", "No sign-off."),
        ])
    };
    let kept_refusal = || {
        bench
            .record()
            .questions
            .latest()
            .unwrap()
            .result_refusal
            .clone()
    };
    let comments = bench.comments().len();
    let (freed, _) = bench.continue_ticket(vec![bench.reply(None, Some(&off_the_ask("Q3")))]);
    assert!(matches!(&freed, Err(Stop::CheckFailed { .. })), "{freed:?}");
    assert_eq!(bench.record().state.waiting, Some(Waiting::NeedsInput));
    assert_eq!(bench.record().state.failed_runs, 1);
    assert_eq!(bench.comments().len(), comments, "nothing posted");
    let freed_reason = kept_refusal();
    assert!(
        freed_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("the verdict for Q3 names a question")),
        "{freed_reason:?}"
    );

    let (parked, _) = bench.continue_ticket(vec![bench.reply(None, Some(&off_the_ask("Q4")))]);
    assert!(
        matches!(
            &parked,
            Err(Stop::Parked {
                reason: ParkReason::FailedRuns,
                ..
            })
        ),
        "{parked:?}"
    );
    let kept = kept_refusal();
    assert!(
        kept.as_deref()
            .is_some_and(|reason| reason.contains("the verdict for Q4 names a question")),
        "{kept:?}"
    );

    let answered = check(&[
        ("Q1", "answered", "English."),
        ("Q2", "answered", "\"Hello, reader.\", no sign-off."),
    ]);
    let (delivered, printed) = bench.continue_ticket(vec![
        bench.reply(None, Some(&answered)),
        bench.reply(Some("Hello"), Some(DONE)),
    ]);
    delivered.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    let told: Vec<(Role, Option<String>)> = bench
        .briefs()
        .into_iter()
        .map(|brief| (brief.role, brief.result_refusal))
        .collect();
    assert_eq!(
        told,
        [
            (Role::Build, None),
            (Role::AnswerCheck, None),
            (Role::AnswerCheck, None),
            (Role::AnswerCheck, freed_reason),
            (Role::AnswerCheck, kept),
            (Role::Build, None),
            (Role::Verify, None),
        ]
    );
    assert_eq!(
        bench.record().questions.latest().unwrap().result_refusal,
        None
    );
}

/// OWL-190 under watch: after a crashed check, a refused one keeps the
/// ticket waiting instead of parking it, and the next pass runs the told
/// retry, with no PARKED comment and no `continue` typed.
#[test]
fn watch_retries_a_refused_check_at_the_last_attempt() {
    let bench = Bench::new(true);
    let answered = answered_round(&bench);
    let mut pass = 0;
    let mut sleep = |_: Duration| {
        pass += 1;
        pass < 3
    };
    let off_the_ask = check(&[
        ("Q1", "answered", "English."),
        ("Q3", "answered", "Hello, reader."),
    ]);
    let (outcomes, lines, printed) = bench.watch(
        vec![
            bench.reply(None, None),
            bench.reply(None, Some(&off_the_ask)),
            answered,
            bench.reply(Some("Hello"), Some(DONE)),
        ],
        &mut sleep,
    );
    // The answer check that passes is a stage step of its own (OWL-367).
    match &outcomes[..] {
        [
            Err(Stop::CheckFailed { .. }),
            Err(Stop::CheckFailed { .. }),
            Err(Stop::Stepped {
                stage: Stage::Build,
                ..
            }),
            Ok(delivered),
        ] => assert!(delivered.opened, "{delivered:?}"),
        other => panic!("{other:?}\n{lines}\n{printed}"),
    }
    let comments = bench.comments();
    assert!(
        !comments.iter().any(|c| c.starts_with("[owlshift] PARKED")),
        "{comments:?}"
    );
    let told: Vec<(Role, bool)> = bench
        .briefs()
        .into_iter()
        .map(|brief| {
            let told = brief
                .result_refusal
                .is_some_and(|reason| reason.contains("the verdict for Q3 names a question"));
            (brief.role, told)
        })
        .collect();
    assert_eq!(
        told,
        [
            (Role::Build, false),
            (Role::AnswerCheck, false),
            (Role::AnswerCheck, false),
            (Role::AnswerCheck, true),
            (Role::Build, false),
            (Role::Verify, false),
        ]
    );
}

/// The re-ask limit through the shipped commands: the fourth incomplete
/// answer parks the ticket with a PARKED comment naming what is still open,
/// and a restart checks a new answer, whose RESUME restates each question
/// from the last check that judged it. The ticket shows the project's parked
/// state until the restart moves it back to needs input (OWL-148).
#[test]
fn the_reask_limit_parks_the_ticket_until_it_is_answered() {
    let bench = Bench::parking();
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
        "**Still open** (partial): The ending is still missing.",
        "answer the questions still open here, then run `owlshift continue DEMO-1`",
    ] {
        assert!(parked.contains(text), "{text}\n{parked}");
    }
    assert!(!parked.contains("### Q1"), "{parked}");
    assert_eq!(bench.stage(), "Parked");
    let moved = data_of(&bench.events(), EventKind::TrackerWrite, "stage");
    assert_eq!(moved.last().unwrap()["stage"], "parked");

    // Answered at last, but `continue` is typed within the quiet window: the
    // ticket is restarted, which is kept, and the answer waits unread. It no
    // longer shows parked.
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
    assert_eq!(bench.stage(), "Needs Input");

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
        "**Understood:** English, the recommendation.",
        "**Understood:** No sign-off.",
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

    // The stage is refused too: it is still tried, and the park stands.
    let bench = Bench::new(true);
    bench.refuse.set(Some("[owlshift] PARKED"));
    bench.refuse_stage.set(true);
    let blocked = r#"{"format":13,"status":"blocked","summary":"The gate needs network."}"#;
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
    assert_eq!(
        bench.stage_events(EventKind::Warning),
        ["In Progress", "Needs Input"]
    );
    let refused = data_of(&bench.events(), EventKind::Warning, "stage");
    assert_eq!(refused.last().unwrap()["stage"], "parked");
    assert_eq!(bench.stage(), "Todo");
}

/// A ticket parked at Build, its questions answered, shows needs input when
/// the project names no parked state; `continue` restarts it, which shows it
/// working again before Build runs to a delivery (OWL-148).
#[test]
fn a_restart_at_build_shows_the_ticket_working_again() {
    let bench = Bench::new(true);
    let (asked, _) = bench.run(vec![bench.reply(None, Some(ROUND_1))], None);
    assert!(matches!(asked, Err(Stop::NeedsInput { .. })), "{asked:?}");
    bench.answer("Q1: English.\nQ2: \"Hello, reader.\", no sign-off.\n");
    let answered = check(&[
        ("Q1", "answered", "English."),
        ("Q2", "answered", "\"Hello, reader.\", no sign-off."),
    ]);
    let blocked = r#"{"format":13,"status":"blocked","summary":"The gate needs network."}"#;
    let (parked, printed) = bench.continue_ticket(vec![
        bench.reply(None, Some(&answered)),
        bench.reply(None, Some(blocked)),
    ]);
    assert!(
        matches!(
            &parked,
            Err(Stop::Parked {
                reason: ParkReason::Blocked,
                ..
            })
        ),
        "{parked:?}\n{printed}"
    );
    assert_eq!(bench.record().state.waiting, Some(Waiting::Parked));
    assert_eq!(bench.stage(), "Needs Input");

    let (delivered, printed) = bench.continue_ticket(vec![bench.reply(Some("Hello"), Some(DONE))]);
    delivered.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    let moves: Vec<Value> = data_of(&bench.events(), EventKind::TrackerWrite, "stage")
        .iter()
        .map(|data| data["stage"].clone())
        .collect();
    assert_eq!(
        moves[moves.len() - 3..],
        [json!("parked"), json!("working"), json!("review")]
    );
    assert_eq!(bench.stage(), "In Review");
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
    let build = bench.last_of(Role::Build);
    assert_eq!(shape(&build), ["answered", "Other", "Decider", "Owlshift"]);
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
const MIXED: &str = r#"{"format":13,"status":"questions",
"summary":"The greeting's file and words are not given.",
"questions":[
  {"id":"Q1","category":"naming","context":"The repository has no greeting yet.",
   "text":"Which file should hold the greeting?","recommendation":{"why":"The simplest reading of the ticket."}},
  {"id":"Q2","category":"scope","context":"The ticket names neither the words nor the ending.",
   "text":"What should the greeting say?","recommendation":{"why":"The simplest reading of the ticket."}}]}"#;

/// A Build run's one discoverable question.
const NAMING: &str = r#"{"format":13,"status":"questions",
"summary":"The greeting's file is not named.",
"questions":[
  {"id":"Q1","category":"naming","context":"The repository has no greeting yet.",
   "text":"Which file should hold the greeting?","recommendation":{"why":"The simplest reading of the ticket."}}]}"#;

/// A result that asks, naming its Q1 as asking each of a hold's first
/// `choices` refused choices (OWL-193).
fn asking_refused(result: &str, choices: u32) -> String {
    let named: Vec<String> = (1..=choices)
        .map(|choice| format!(r#"{{"choice":{choice},"question":"Q1"}}"#))
        .collect();
    format!(
        r#"{},"refused_choices_asked":[{}]}}"#,
        result.strip_suffix('}').unwrap(),
        named.join(",")
    )
}

/// The resolver decides Q1.
const DECIDED_Q1: &str = r#"{"format":13,"status":"done","summary":"The ticket names the file.",
"resolutions":[{"question":"Q1","outcome":"decided","category":"naming","decision":"GREETING.md, at the root.",
"basis":"The ticket's description.","basis_kind":"settled"}]}"#;

/// The resolver passes on each of `ids` (OWL-342: every question reaches
/// it, and these stay the decider's).
fn passed_on(ids: &[&str]) -> String {
    let resolutions: Vec<Value> = ids
        .iter()
        .map(|id| {
            json!({ "question": id, "outcome": "passed_on",
                "reason": "Nothing here settles it, and the level does not let me weigh it." })
        })
        .collect();
    json!({ "format": 13, "status": "done", "summary": "Passed on.", "resolutions": resolutions })
        .to_string()
}

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
    // One line in the comment, the reasoning in the ticket's decisions
    // document (OWL-351).
    assert!(
        comments[0].contains(
            "- **D1 · Which file should hold the greeting?** — GREETING.md, at the root. The \
             ticket's description.\n"
        ) && comments[0].contains("Why: the decisions document, `tickets/DEMO-1/documents/1.md`."),
        "{}",
        comments[0]
    );
    let document = bench.document("Decisions");
    for text in [
        "#### D1 · Which file should hold the greeting?",
        "**Decision:** GREETING.md, at the root.",
        "**Settled by:** The ticket's description.",
    ] {
        assert!(document.contains(text), "{text}\n{document}");
    }
    assert!(bench.record().questions.decisions_document.is_some());
    let round_1 = &comments[1];
    assert!(
        round_1.starts_with("[owlshift] QUESTIONS · round 1 · @maintainer\n"),
        "{round_1}"
    );
    assert!(round_1.contains("### Q1 · What should the greeting say?"));
    assert!(round_1.contains("Owlshift decided one other question of this run itself"));
    assert!(
        !round_1.contains("naming") && !round_1.contains("### Q2"),
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
    let written = &data_of(&events, EventKind::TrackerWrite, "questions")[0];
    assert_eq!(written["kind"], "DECISION");
    // Without a hold, nothing is reopened (OWL-203).
    assert!(opened[0].get("reopened").is_none() && written.get("reopened").is_none());

    // Answered, the ticket resumes: the decision sits before the round in
    // every later brief, and the delivery report lists it.
    bench.answer("Q1: \"Hello, reader.\"\n");
    let answered = check(&[("Q1", "answered", "Hello, reader.")]);
    let (delivered, printed) = bench.continue_ticket(vec![
        bench.reply(None, Some(&answered)),
        bench.reply(Some("Hello"), Some(DONE)),
    ]);
    delivered.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    let build = bench.last_of(Role::Build);
    assert_eq!(
        shape(&build),
        ["decision", "answered", "Decider", "Owlshift"]
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

/// A Build run's two discoverable questions (OWL-351).
const TWO_NAMINGS: &str = r#"{"format":13,"status":"questions",
"summary":"The greeting's file and its link are not named.",
"questions":[
  {"id":"Q1","category":"naming","context":"The repository has no greeting yet.",
   "text":"Which file should hold the greeting?","recommendation":{"why":"The simplest reading of the ticket."}},
  {"id":"Q2","category":"naming","context":"The README lists the files at the root.",
   "text":"Should the README link to it?","recommendation":{"why":"The README's list."}}]}"#;

/// The resolver decides both of [`TWO_NAMINGS`].
const DECIDED_BOTH: &str = r#"{"format":13,"status":"done","summary":"The ticket and the README settle both.",
"resolutions":[{"question":"Q1","outcome":"decided","category":"naming","decision":"GREETING.md, at the root.",
"basis":"The ticket's description.","basis_kind":"settled"},
{"question":"Q2","outcome":"decided","category":"naming","decision":"Yes, in its list of files.",
"basis":"README.md lists every file at the root.","basis_kind":"fact"}]}"#;

/// OWL-351: a resolver run that decides two questions posts one DECISION
/// comment of two lines, D1 and D2, their reasoning in the ticket's
/// decisions document; both decisions are kept with that comment, in one
/// write. When the tracker refuses the comment, both go to the decider.
#[test]
fn a_resolver_runs_decisions_are_one_comment() {
    let bench = Bench::new(true);
    let mut limited = bench.reply(None, None);
    limited.usage_limit = Some("2026-10-03T18:00:00Z".parse().unwrap());
    let (outcome, printed) = bench.run(
        vec![
            bench.reply(None, Some(TWO_NAMINGS)),
            bench.reply(None, Some(DECIDED_BOTH)),
            limited,
        ],
        None,
    );
    assert!(
        matches!(&outcome, Err(Stop::UsageLimit { .. })),
        "{outcome:?}\n{printed}"
    );
    let comments = bench.comments();
    assert_eq!(comments.len(), 1, "one DECISION: {comments:?}");
    let lines: Vec<&str> = comments[0]
        .lines()
        .filter(|line| line.starts_with("- **D"))
        .collect();
    assert_eq!(
        lines,
        [
            "- **D1 · Which file should hold the greeting?** — GREETING.md, at the root. The \
             ticket's description.",
            "- **D2 · Should the README link to it?** — Yes, in its list of files. README.md \
             lists every file at the root.",
        ],
        "{}",
        comments[0]
    );
    let record = bench.record();
    let kept: Vec<(&str, u32)> = record
        .questions
        .decisions
        .iter()
        .map(|d| (d.comment.as_str(), d.place().get()))
        .collect();
    assert_eq!(kept.len(), 2);
    assert_eq!(kept[0].0, kept[1].0);
    assert_eq!((kept[0].1, kept[1].1), (1, 2));
    let document = bench.document("Decisions");
    assert!(
        document.contains("#### D1 · Which file should hold the greeting?")
            && document.contains("#### D2 · Should the README link to it?"),
        "{document}"
    );
    let written = data_of(&bench.events(), EventKind::TrackerWrite, "questions");
    assert_eq!(written.len(), 1);
    assert_eq!(written[0]["questions"], json!(["Q1", "Q2"]));

    let bench = Bench::new(true);
    bench.refuse.set(Some("[owlshift] DECISION"));
    let (outcome, _) = bench.run(
        vec![
            bench.reply(None, Some(TWO_NAMINGS)),
            bench.reply(None, Some(DECIDED_BOTH)),
        ],
        None,
    );
    let stop = outcome.expect_err("a round");
    assert_eq!(asked_texts(&stop).len(), 2);
    assert!(bench.record().questions.decisions.is_empty());
    let resolved = data_of(&bench.events(), EventKind::Gate, "resolver");
    assert_eq!(resolved[0]["unposted"], json!(["Q1", "Q2"]));
}

/// The resolver resolves Q2, which a brief giving Q1 alone did not give: its
/// result is refused.
const RESOLVED_Q2: &str = r#"{"format":13,"status":"done","summary":"The ticket names the file.",
"resolutions":[{"question":"Q2","outcome":"decided","category":"naming","decision":"GREETING.md, at the root.",
"basis":"The ticket's description.","basis_kind":"settled"}]}"#;

/// OWL-191: a refused resolver result is kept in the ticket ref before its
/// fallback round is posted, so when the tracker refuses that round, the
/// resolver run of a later `do` that replays the questions is told why;
/// it decides, which clears the kept reason. A reason the ticket ref
/// cannot keep stops the command before any round is posted.
#[test]
fn a_refused_resolver_result_is_told_to_the_replayed_round() {
    let bench = Bench::new(true);
    bench.refuse.set(Some("[owlshift] QUESTIONS"));
    let (unposted, printed) = bench.run(
        vec![
            bench.reply(None, Some(NAMING)),
            bench.reply(None, Some(RESOLVED_Q2)),
        ],
        None,
    );
    assert!(
        matches!(&unposted, Err(Stop::NeedsInput { posted: Err(_), .. })),
        "{unposted:?}\n{printed}"
    );
    bench.refuse.set(None);
    assert!(bench.comments().is_empty());
    let record = bench.record();
    assert_eq!((record.state.waiting, record.state.round), (None, 0));
    assert!(record.questions.asks.is_empty());
    let kept = record.questions.resolver_refusal.unwrap();
    assert!(kept.contains("Q2"), "{kept}");

    let (delivered, printed) = bench.run(
        vec![
            bench.reply(None, Some(NAMING)),
            bench.reply(None, Some(DECIDED_Q1)),
            bench.reply(Some("Hello"), Some(DONE)),
        ],
        None,
    );
    delivered.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    let told: Vec<(Role, Option<String>)> = bench
        .briefs()
        .into_iter()
        .map(|brief| (brief.role, brief.result_refusal))
        .collect();
    assert_eq!(
        told,
        [
            (Role::Build, None),
            (Role::Resolver, None),
            (Role::Build, None),
            (Role::Resolver, Some(kept)),
            (Role::Build, None),
            (Role::Verify, None),
        ]
    );
    assert_eq!(bench.record().questions.resolver_refusal, None);

    // The ticket's ref is locked while the resolver runs: keeping its
    // refusal fails, and the command stops with nothing posted.
    let bench = Bench::new(true);
    let (stopped, printed) = bench.invoke(
        false,
        vec![
            bench.reply(None, Some(NAMING)),
            bench.reply(None, Some(RESOLVED_Q2)),
        ],
        None,
        vec![bench.lock_ticket_ref_during(2)],
    );
    match &stopped {
        Err(Stop::Refused(message)) => assert!(
            message.contains("keeping the resolver's refusal in the ticket's ref"),
            "{message}"
        ),
        other => panic!("{other:?}\n{printed}"),
    }
    assert!(bench.comments().is_empty());
}

/// OWL-196: the fallback round of a refused resolver result is posted, then
/// the ticket ref refuses the write keeping it. The command stops naming the
/// comment, which stays on the ticket, and the refusal kept before the post
/// is still in the ref, so the resolver run of a later `do` that replays the
/// questions is told why.
#[test]
fn a_refused_resolver_result_is_told_when_its_round_was_posted_but_not_kept() {
    let bench = Bench::new(true);
    bench.lock_after.set(Some("[owlshift] QUESTIONS"));
    let (stopped, printed) = bench.run(
        vec![
            bench.reply(None, Some(NAMING)),
            bench.reply(None, Some(RESOLVED_Q2)),
        ],
        None,
    );
    match &stopped {
        Err(Stop::NotKept {
            message,
            landed: Landed::Questions,
        }) => assert!(
            message.contains("keeping them in the ticket's ref failed"),
            "{message}"
        ),
        other => panic!("{other:?}\n{printed}"),
    }
    fs::remove_file(bench.ref_lock()).unwrap();
    let comments = bench.comments();
    assert_eq!(comments.len(), 1);
    assert!(
        comments[0].starts_with("[owlshift] QUESTIONS"),
        "{comments:?}"
    );
    // The stage showed the wait before keeping was tried, as the stand-in
    // driver plays it.
    assert_eq!(bench.stage(), "Needs Input");
    let record = bench.record();
    assert_eq!((record.state.waiting, record.state.round), (None, 0));
    assert!(record.questions.asks.is_empty());
    let kept = record.questions.resolver_refusal.unwrap();
    assert!(kept.contains("Q2"), "{kept}");

    let (delivered, printed) = bench.run(
        vec![
            bench.reply(None, Some(NAMING)),
            bench.reply(None, Some(DECIDED_Q1)),
            bench.reply(Some("Hello"), Some(DONE)),
        ],
        None,
    );
    delivered.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    let told: Vec<(Role, Option<String>)> = bench
        .briefs()
        .into_iter()
        .map(|brief| (brief.role, brief.result_refusal))
        .collect();
    assert_eq!(
        told,
        [
            (Role::Build, None),
            (Role::Resolver, None),
            (Role::Build, None),
            (Role::Resolver, Some(kept)),
            (Role::Build, None),
            (Role::Verify, None),
        ]
    );
    assert_eq!(bench.record().questions.resolver_refusal, None);
}

/// OWL-151's acceptance: the project's added always-human categories reach
/// Build's brief and the resolver's, normalized, once each and without what
/// the floor already covers; a question Build files under one goes to the
/// decider and the resolver is never run.
#[test]
fn the_added_always_human_categories_reach_build_and_the_resolver() {
    let raised = r#"{"format":13,"status":"questions",
"summary":"The tone and the file are open.",
"questions":[
  {"id":"Q1","category":"naming","context":"The repository has no greeting yet.",
   "text":"Which file should hold the greeting?","recommendation":{"why":"The simplest reading of the ticket."}},
  {"id":"Q2","category":"Brand Voice","context":"The ticket does not say how formal it is.",
   "text":"How formal should the greeting be?","recommendation":{"why":"The simplest reading of the ticket."}}]}"#;
    let resolved = r#"{"format":13,"status":"done","summary":"The file is named.",
"resolutions":[
  {"question":"Q1","outcome":"decided","category":"naming",
   "decision":"GREETING.md, at the root.","basis":"The ticket's description.","basis_kind":"settled"}]}"#;
    let bench = Bench::new(true);
    let config_path = bench.remote.checkout.join("owlshift.toml");
    let original = fs::read_to_string(&config_path).unwrap();
    fs::write(
        &config_path,
        original.replace(
            "always_human = []",
            r#"always_human = ["Brand-Voice", "brand voice", "Data Loss", ""]"#,
        ),
    )
    .unwrap();
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
        ["How formal should the greeting be?"],
        "{printed}"
    );
    let briefs = bench.briefs();
    assert_eq!(briefs.len(), 2, "{printed}");
    assert_eq!(briefs[0].role, Role::Build);
    assert_eq!(briefs[0].always_human, ["brand_voice"]);
    assert_eq!(briefs[1].role, Role::Resolver);
    assert_eq!(briefs[1].always_human, ["brand_voice"]);
    let given: Vec<&str> = briefs[1].resolve.iter().map(|q| q.id.as_str()).collect();
    assert_eq!(
        given,
        ["Q1"],
        "the added category's question never reaches it"
    );
}

/// OWL-144's acceptance: Build files a data-loss question under another
/// category; the resolver, which never sees Build's categories, decides it
/// but labels it `data_loss`, and the runner refuses that decision: no
/// DECISION, nothing kept, and the question goes to the decider beside the
/// one passed on, while the other decision of the same result stands.
#[test]
fn a_decision_the_resolver_labels_always_human_goes_to_the_decider() {
    let raised = r#"{"format":13,"status":"questions",
"summary":"The greeting's file, the old greetings and the tone are open.",
"questions":[
  {"id":"Q1","category":"naming","context":"The repository has no greeting yet.",
   "text":"Which file should hold the greeting?","recommendation":{"why":"The simplest reading of the ticket."}},
  {"id":"Q2","category":"cleanup","context":"OLD_GREETINGS.md keeps every greeting sent.",
   "text":"May the old greetings file be deleted?","recommendation":{"why":"The simplest reading of the ticket."}},
  {"id":"Q3","category":"tone","context":"The ticket does not say how formal it is.",
   "text":"How formal should the greeting be?","recommendation":{"why":"The simplest reading of the ticket."}}]}"#;
    let resolved = r#"{"format":13,"status":"done","summary":"The file is named; the rest is not mine.",
"resolutions":[
  {"question":"Q1","outcome":"decided","category":"naming",
   "decision":"GREETING.md, at the root.","basis":"The ticket's description.","basis_kind":"settled"},
  {"question":"Q2","outcome":"decided","category":"data_loss",
   "decision":"Yes, delete it.","basis":"Nothing reads it.","basis_kind":"fact"},
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
        comments[1].contains("### Q1 · May the old greetings file be deleted?"),
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

/// The pass limit these benches set (`policy.resolver_passes`, OWL-328):
/// the arithmetic of each is written for 3, the limit before OWL-328.
const PASSES: u32 = 3;

/// A bench whose project sets the pass limit to [`PASSES`].
fn bench_at_passes() -> Bench {
    let bench = Bench::new(true);
    bench.policy(&format!("resolver_passes = {PASSES}"));
    bench
}

/// A Build that keeps asking what the resolver settles runs at most
/// the project's pass limit times in a row on decisions alone; the next
/// run's questions all go to the decider, and the round says why.
#[test]
fn the_resolver_settles_a_bounded_number_of_runs_in_a_row() {
    let bench = bench_at_passes();
    let mut replies = Vec::new();
    for _ in 0..PASSES {
        replies.push(bench.reply(None, Some(NAMING)));
        replies.push(bench.reply(None, Some(DECIDED_Q1)));
    }
    replies.push(bench.reply(None, Some(NAMING)));
    let (outcome, _) = bench.run(replies, None);
    let stop = outcome.expect_err("a round");
    assert_eq!(asked_texts(&stop), ["Which file should hold the greeting?"]);
    let comments = bench.comments();
    let rounds = comments.len() - usize::try_from(PASSES).unwrap();
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
    assert!(resolved.last().unwrap().get("reason").is_none());
}

/// A Build run's one `scope` question, which the ticket's own text answers
/// ("Add GREETING.md saying Hello.").
const SCOPE: &str = r#"{"format":13,"status":"questions",
"summary":"Whether the greeting is a file of its own is not said in so many words.",
"questions":[
  {"id":"Q1","category":"scope","context":"The repository has no greeting yet.",
   "text":"Does the ticket ask for a GREETING.md file of its own?","recommendation":{"why":"The ticket names the file."}}]}"#;

/// The resolver decides [`SCOPE`]'s question on a `ticket` basis, quoting
/// `quote`.
fn decided_scope(quote: &str) -> String {
    format!(
        r#"{{"format":13,"status":"done","summary":"The ticket names the file.",
"resolutions":[{{"question":"Q1","outcome":"decided","category":"scope",
"decision":"Yes: GREETING.md, at the root.","basis":"The ticket's description.",
"basis_kind":"ticket","quote":{quote:?}}}]}}"#
    )
}

/// OWL-328's bench: at `ticket`, a `scope` question the ticket's text
/// answers is decided on a quoted `ticket` basis, the DECISION and the
/// delivery report naming the level; at `discoverable`, the default, the
/// same ticket's question goes to the decider, no resolver running; at
/// `ticket` again, a quote the ticket does not hold is refused, its question
/// going to the decider.
#[test]
fn a_quoted_scope_question_is_decided_at_ticket_and_asked_at_discoverable() {
    let bench = Bench::new(true);
    bench.policy("resolver_delegation = \"ticket\"");
    let (delivered, printed) = bench.run(
        vec![
            bench.reply(None, Some(SCOPE)),
            bench.reply(
                None,
                Some(&decided_scope("Add  GREETING.md   saying Hello.")),
            ),
            bench.reply(Some("Hello"), Some(DONE)),
        ],
        None,
    );
    delivered.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    let briefs = bench.briefs();
    let roles: Vec<Role> = briefs.iter().map(|b| b.role).collect();
    // Verify, which reviews clean on this bench, follows.
    assert_eq!(
        roles[..3],
        [Role::Build, Role::Resolver, Role::Build],
        "{printed}"
    );
    assert_eq!(briefs[1].delegation, Some(DelegationLevel::Ticket));
    assert_eq!(briefs[0].delegation, None);
    let comments = bench.comments();
    assert!(
        comments[0].starts_with("[owlshift] DECISION\n")
            && comments[0].contains("delegation level `ticket`"),
        "{}",
        comments[0]
    );
    let report = comments.last().unwrap();
    assert!(
        report.starts_with("[owlshift] DELIVERY")
            && report.contains("allowed by this project's delegation level, `ticket`"),
        "{report}"
    );
    let resolved = data_of(&bench.events(), EventKind::Gate, "resolver");
    assert_eq!(resolved[0]["resolver"], "done");
    assert_eq!(resolved[0]["level"], "ticket");
    assert_eq!(resolved[0]["decided"], json!(["Q1"]));

    let bench = Bench::new(true);
    let (outcome, printed) = bench.run(vec![bench.reply(None, Some(SCOPE))], None);
    let stop = outcome.expect_err("a round");
    assert_eq!(
        asked_texts(&stop),
        ["Does the ticket ask for a GREETING.md file of its own?"],
        "{printed}"
    );
    let roles: Vec<Role> = bench.briefs().iter().map(|b| b.role).collect();
    assert_eq!(roles, [Role::Build], "no resolver runs: {printed}");
    assert!(data_of(&bench.events(), EventKind::Gate, "resolver").is_empty());

    let bench = Bench::new(true);
    bench.policy("resolver_delegation = \"ticket\"");
    let (outcome, printed) = bench.run(
        vec![
            bench.reply(None, Some(SCOPE)),
            bench.reply(None, Some(&decided_scope("Add GREETING.md and a README."))),
        ],
        None,
    );
    let stop = outcome.expect_err("a round");
    assert_eq!(asked_texts(&stop).len(), 1, "{printed}");
    assert!(
        !bench
            .comments()
            .iter()
            .any(|c| c.contains("[owlshift] DECISION"))
    );
    let resolved = data_of(&bench.events(), EventKind::Gate, "resolver");
    assert_eq!(resolved[0]["refused"], json!(["Q1"]));
    assert_eq!(resolved[0]["to_decider"], json!(["Q1"]));
}

/// OWL-328's ceiling, through the runner, on the resolver runs alone since
/// OWL-376: with `"unlimited"` passes and a $1 ceiling, Build runs costing
/// more than the ceiling, one of them reporting no cost, keep the chain
/// going while the resolver runs add up, and once those reach the ceiling
/// the next run's questions go to the decider, the round and the `gate`
/// event saying why; every `gate` event gives the resolver's spend so far,
/// and the asking runs', measured only.
/// A resolver run that reports no cost ends the chain at the next check.
#[test]
fn an_unlimited_chain_ends_at_its_ceiling_or_on_a_run_without_a_cost() {
    let costing = |bench: &Bench, result: &str, micros: Option<u64>| {
        let mut reply = bench.reply(None, Some(result));
        reply.cost_micros = micros;
        reply
    };
    let unlimited = "resolver_passes = \"unlimited\"\nresolver_budget_usd = 1.0";
    let build = Some(2_000_000);
    let resolver = Some(300_000);

    // Four resolver runs at 0.3 each: 1.2 once the fifth Build asks, past
    // the ceiling; the Build runs' $8 and the one without a cost do not
    // count.
    let bench = Bench::new(true);
    bench.policy(unlimited);
    let (outcome, printed) = bench.run(
        vec![
            costing(&bench, NAMING, build),
            costing(&bench, DECIDED_Q1, resolver),
            costing(&bench, NAMING, None),
            costing(&bench, DECIDED_Q1, resolver),
            costing(&bench, NAMING, build),
            costing(&bench, DECIDED_Q1, resolver),
            costing(&bench, NAMING, build),
            costing(&bench, DECIDED_Q1, resolver),
            costing(&bench, NAMING, build),
        ],
        None,
    );
    let stop = outcome.expect_err("a round");
    assert_eq!(asked_texts(&stop), ["Which file should hold the greeting?"]);
    let roles: Vec<Role> = bench.briefs().iter().map(|b| b.role).collect();
    let mut expected = [Role::Build, Role::Resolver].repeat(4);
    expected.push(Role::Build);
    assert_eq!(roles, expected, "{printed}");
    let round = bench.comments().pop().unwrap();
    assert!(
        round.contains("cost 1.20 USD, which reached this project's dollar ceiling of 1.00 USD"),
        "{round}"
    );
    let resolved = data_of(&bench.events(), EventKind::Gate, "resolver");
    let spent: Vec<&Value> = resolved.iter().map(|e| &e["resolver_spent_usd"]).collect();
    assert_eq!(
        spent,
        [
            &json!(0.3),
            &json!(0.6),
            &json!(0.9),
            &json!(1.2),
            &json!(1.2)
        ],
        "{printed}"
    );
    assert!(resolved.iter().all(|e| e["ceiling_usd"] == json!(1.0)));
    // The asking runs are measured: $2, then unreported once a Build
    // reported no cost.
    let asking: Vec<&Value> = resolved.iter().map(|e| &e["asking_spent_usd"]).collect();
    assert_eq!(
        asking,
        [
            &json!(2.0),
            &Value::Null,
            &Value::Null,
            &Value::Null,
            &Value::Null
        ],
        "{printed}"
    );
    let (last, settled) = resolved.split_last().unwrap();
    assert!(
        settled
            .iter()
            .all(|e| e["resolver"] == "done" && e.get("reason").is_none())
    );
    assert_eq!(last["resolver"], "pass_limit");
    assert_eq!(last["reason"], "budget");
    assert_eq!(last["level"], "discoverable");

    // A resolver run without a cost: the next check cannot read the
    // chain's, which ends it.
    let bench = Bench::new(true);
    bench.policy(unlimited);
    let (outcome, printed) = bench.run(
        vec![
            costing(&bench, NAMING, build),
            costing(&bench, DECIDED_Q1, None),
            costing(&bench, NAMING, build),
        ],
        None,
    );
    let stop = outcome.expect_err("a round");
    assert_eq!(asked_texts(&stop).len(), 1, "{printed}");
    let roles: Vec<Role> = bench.briefs().iter().map(|b| b.role).collect();
    assert_eq!(
        roles,
        [Role::Build, Role::Resolver, Role::Build],
        "{printed}"
    );
    let round = bench.comments().pop().unwrap();
    assert!(
        round.contains("A resolver run since this command started reported no cost"),
        "{round}"
    );
    let resolved = data_of(&bench.events(), EventKind::Gate, "resolver");
    assert!(
        resolved.iter().all(|e| e["resolver_spent_usd"].is_null()),
        "{printed}"
    );
    let asking: Vec<&Value> = resolved.iter().map(|e| &e["asking_spent_usd"]).collect();
    assert_eq!(asking, [&json!(2.0), &json!(4.0)], "{printed}");
    assert_eq!(resolved.last().unwrap()["reason"], "cost_unreported");
}

/// A reply giving `result` after committing `greeting`, when given, whose
/// run reports `micros` millionths of a dollar.
fn costed(bench: &Bench, greeting: Option<&str>, result: &str, micros: Option<u64>) -> Reply {
    let mut reply = bench.reply(greeting, Some(result));
    reply.cost_micros = micros;
    reply
}

/// OWL-378, the ticket's budget through the runner, `ticket_budget_usd =
/// 5`: two Build runs at $2 and a resolver run at $1 reach it, so the second
/// Build's question goes to the decider without a resolver run, the round
/// and the `gate` event saying why. The answer's check would be the next
/// run: the ticket parks, waiting for input, its spend kept in the ticket
/// ref. A person's `continue` restarts it, its budget counted afresh: the
/// check, the Build and Verify run, the whole spend kept.
#[test]
fn a_ticket_parks_once_its_runs_reach_its_budget_and_a_restart_counts_afresh() {
    let bench = Bench::parking();
    bench.policy("ticket_budget_usd = 5");
    let (outcome, printed) = bench.run(
        vec![
            costed(&bench, None, NAMING, Some(2_000_000)),
            costed(&bench, None, DECIDED_Q1, Some(1_000_000)),
            costed(&bench, None, NAMING, Some(2_000_000)),
        ],
        None,
    );
    assert!(
        matches!(&outcome, Err(Stop::NeedsInput { posted: Ok(_), .. })),
        "{outcome:?}\n{printed}"
    );
    let roles: Vec<Role> = bench.briefs().iter().map(|b| b.role).collect();
    assert_eq!(roles, [Role::Build, Role::Resolver, Role::Build]);
    let round = bench.comments().pop().unwrap();
    assert!(
        round.contains(
            "This ticket's runs cost 5.00 USD since it started or was last restarted, which \
             reached its budget of 5.00 USD (`policy.ticket_budget_usd`), so this run's \
             questions all come to you. Once you answer, the ticket stops for its budget until \
             a person types `owlshift continue DEMO-1`, which counts its runs afresh."
        ),
        "{round}"
    );
    let resolved = data_of(&bench.events(), EventKind::Gate, "resolver");
    let last = resolved.last().unwrap();
    assert_eq!(
        (&last["resolver"], &last["reason"], &last["run"]),
        (&json!("pass_limit"), &json!("ticket_budget"), &Value::Null)
    );
    assert_eq!(
        (
            &last["ticket_spent_usd"],
            &last["budget_spent_usd"],
            &last["ticket_budget_usd"]
        ),
        (&json!(5.0), &json!(5.0), &json!(5.0))
    );
    let cost = bench.record().cost.expect("cost.json");
    assert_eq!(
        (cost.runs, cost.spent_micros, cost.restarted_at_micros),
        (3, 5_000_000, 0)
    );

    bench.answer("Q1: GREETING.md, at the root. Go.\n");
    let (outcome, printed) = bench.continue_ticket(Vec::new());
    assert!(
        matches!(
            &outcome,
            Err(Stop::Parked {
                reason: ParkReason::TicketBudget,
                ..
            })
        ),
        "{outcome:?}\n{printed}"
    );
    assert_eq!(bench.briefs().len(), 3, "no run started");
    assert_eq!(
        state_of(&bench).status(),
        Status::Parked {
            at: Stage::Build,
            awaiting_input: true
        }
    );
    let parked = bench.comments().pop().unwrap();
    assert!(
        parked.contains(
            "Parked: the ticket's runs reached its budget. Its runs cost 5.00 USD since it \
             started or was last restarted, which reached its budget of 5.00 USD \
             (`policy.ticket_budget_usd`)."
        ) && parked.contains("its budget then counts its runs afresh"),
        "{parked}"
    );
    assert_eq!(bench.stage(), "Parked");

    let (delivered, printed) = bench.continue_ticket(vec![
        costed(
            &bench,
            None,
            &check(&[("Q1", "answered", "GREETING.md, at the root.")]),
            Some(500_000),
        ),
        costed(&bench, Some("Hello"), DONE, Some(2_000_000)),
    ]);
    delivered.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    let roles: Vec<Role> = bench.briefs()[3..].iter().map(|b| b.role).collect();
    assert_eq!(roles, [Role::AnswerCheck, Role::Build, Role::Verify]);
    // Verify's stand-in reports no cost: counted, adding nothing.
    let cost = bench.record().cost.expect("cost.json");
    assert_eq!(
        (
            cost.runs,
            cost.unreported_runs,
            cost.spent_micros,
            cost.restarted_at_micros
        ),
        (6, 1, 7_500_000, 5_000_000)
    );
}

/// OWL-378, the spend kept across commands and no run started past the
/// budget: under a $3 budget, a Build's `security` question goes to the
/// decider, no resolver involved, at $2.50; the next command reads that
/// spend back from the ticket ref, and the answer's check at $0.60 passes
/// the budget, so the Build it resumes does not start: the ticket parks at
/// Build.
#[test]
fn the_spend_is_kept_across_commands_and_no_run_starts_past_the_budget() {
    let bench = Bench::parking();
    bench.policy("ticket_budget_usd = 3");
    let security = NAMING.replace(r#""category":"naming""#, r#""category":"security""#);
    let (asked, printed) = bench.run(vec![costed(&bench, None, &security, Some(2_500_000))], None);
    assert!(
        matches!(&asked, Err(Stop::NeedsInput { posted: Ok(_), .. })),
        "{asked:?}\n{printed}"
    );
    assert!(!bench.comments().pop().unwrap().contains("budget"));
    bench.answer("Q1: GREETING.md. Go.\n");
    let (parked, printed) = bench.continue_ticket(vec![costed(
        &bench,
        None,
        &check(&[("Q1", "answered", "GREETING.md.")]),
        Some(600_000),
    )]);
    assert!(
        matches!(
            &parked,
            Err(Stop::Parked {
                reason: ParkReason::TicketBudget,
                ..
            })
        ),
        "{parked:?}\n{printed}"
    );
    let roles: Vec<Role> = bench.briefs().iter().map(|b| b.role).collect();
    assert_eq!(roles, [Role::Build, Role::AnswerCheck]);
    assert_eq!(
        state_of(&bench).status(),
        Status::Parked {
            at: Stage::Build,
            awaiting_input: false
        }
    );
    let cost = bench.record().cost.expect("cost.json");
    assert_eq!((cost.runs, cost.spent_micros), (2, 3_100_000));
    assert!(
        bench
            .comments()
            .pop()
            .unwrap()
            .contains("Its runs cost 3.10 USD since it started"),
        "{printed}"
    );
}

/// OWL-378 at `all`: the budget reached by a Build that asks, the runner
/// takes its recommendation, as at any bound of the chain (OWL-375), its
/// DECISION naming the budget, and the Build that would follow it does not
/// start: the ticket parks at Build.
#[test]
fn at_all_the_budget_takes_the_recommendation_then_parks_before_the_next_run() {
    let bench = Bench::parking();
    bench.policy("resolver_delegation = \"all\"\nticket_budget_usd = 5");
    let (parked, printed) = bench.run(vec![costed(&bench, None, NAMING, Some(6_000_000))], None);
    assert!(
        matches!(
            &parked,
            Err(Stop::Parked {
                reason: ParkReason::TicketBudget,
                ..
            })
        ),
        "{parked:?}\n{printed}"
    );
    assert_eq!(bench.briefs().len(), 1);
    let comments = bench.comments();
    let decision = comments
        .iter()
        .find(|c| c.starts_with("[owlshift] DECISION"))
        .expect("a DECISION comment");
    assert!(
        decision.contains(
            "This ticket's runs cost 6.00 USD since it started or was last restarted, which \
             reached its budget of 5.00 USD (`policy.ticket_budget_usd`). Owlshift took the \
             asking run's recommendation"
        ),
        "{decision}"
    );
    assert!(comments.last().unwrap().starts_with("[owlshift] PARKED"));
    assert_eq!(bench.record().questions.decisions.len(), 1);
}

/// A usage-limit reply that reports `micros` millionths of a dollar.
fn cut_off(bench: &Bench, micros: u64) -> Reply {
    let mut reply = bench.reply(None, None);
    reply.usage_limit = Some("2026-10-02T12:00:00Z".parse().unwrap());
    reply.cost_micros = Some(micros);
    reply
}

/// OWL-397's acceptance: a run cut by a usage limit is counted on the next
/// command. Under a $5 budget, a Build's `security` question goes to the
/// decider at $2; the answer's check, at $0.50, resumes Build, whose run a
/// usage limit cuts at $3. That command writes nothing after the run, yet
/// the ticket ref keeps its cost, its state as the RESUME left it: the next
/// `continue` reads $5.50 back and starts no run, the ticket parked for its
/// budget. Before OWL-397 the cut run's cost was lost and the Build ran.
#[test]
fn a_run_cut_by_a_usage_limit_is_counted_on_the_next_command() {
    let bench = Bench::parking();
    bench.policy("ticket_budget_usd = 5");
    let security = NAMING.replace(r#""category":"naming""#, r#""category":"security""#);
    let (asked, printed) = bench.run(vec![costed(&bench, None, &security, Some(2_000_000))], None);
    assert!(
        matches!(&asked, Err(Stop::NeedsInput { posted: Ok(_), .. })),
        "{asked:?}\n{printed}"
    );
    bench.answer("Q1: GREETING.md. Go.\n");
    let answered = check(&[("Q1", "answered", "GREETING.md.")]);
    let (stopped, printed) = bench.continue_ticket(vec![
        costed(&bench, None, &answered, Some(500_000)),
        cut_off(&bench, 3_000_000),
    ]);
    assert!(
        matches!(&stopped, Err(Stop::UsageLimit { .. })),
        "{stopped:?}\n{printed}"
    );
    let roles: Vec<Role> = bench.briefs().iter().map(|b| b.role).collect();
    assert_eq!(roles, [Role::Build, Role::AnswerCheck, Role::Build]);
    let kept = bench.record();
    let cost = kept.cost.clone().expect("cost.json");
    assert_eq!((cost.runs, cost.spent_micros), (3, 5_500_000));
    assert_eq!(
        state_of(&bench).status(),
        Status::Active(Stage::Build),
        "{kept:?}"
    );

    let (parked, printed) = bench.continue_ticket(Vec::new());
    assert!(
        matches!(
            &parked,
            Err(Stop::Parked {
                reason: ParkReason::TicketBudget,
                ..
            })
        ),
        "{parked:?}\n{printed}"
    );
    assert_eq!(bench.briefs().len(), 3, "no run started");
    assert!(
        bench
            .comments()
            .pop()
            .unwrap()
            .contains("Its runs cost 5.50 USD since it started"),
        "{printed}"
    );
}

/// OWL-397: a ticket without a ticket ref gets one at its first run's end,
/// holding what `owlshift forget` leaves (Ready, round 0, nothing asked) and
/// the run's cost, so the rest of the command keeps its state as any ticket
/// with a ref does. Each change this makes to a `do` typed by hand is
/// pinned here: its Build fails once at the gate, then a usage limit cuts
/// it, and the ref keeps Build with its failed run and both runs' cost,
/// where it kept nothing before; `continue` resumes that Build, where it
/// said that Owlshift keeps nothing; the run that fails again parks the
/// ticket, the ref keeping the park and the PARKED comment naming `owlshift
/// continue`, where it named `owlshift do`, which started over; the restart
/// delivers, every run counted.
#[test]
fn a_ticket_without_a_ref_gets_one_at_its_first_runs_end() {
    let bench = Bench::parking();
    let (stopped, printed) = bench.run(
        vec![
            costed(&bench, Some("Helo"), DONE, Some(1_000_000)),
            cut_off(&bench, 1_500_000),
        ],
        None,
    );
    assert!(
        matches!(&stopped, Err(Stop::UsageLimit { .. })),
        "{stopped:?}\n{printed}"
    );
    let state = state_of(&bench);
    assert_eq!(
        (state.status(), state.round(), state.failed_runs()),
        (Status::Active(Stage::Build), 0, 1)
    );
    let kept = bench.record();
    assert!(kept.questions.asks.is_empty());
    let cost = kept.cost.expect("cost.json");
    assert_eq!((cost.runs, cost.spent_micros), (2, 2_500_000));

    let (parked, printed) =
        bench.continue_ticket(vec![costed(&bench, Some("Hi"), DONE, Some(1_000_000))]);
    assert!(
        matches!(
            &parked,
            Err(Stop::Parked {
                reason: ParkReason::FailedRuns,
                ..
            })
        ),
        "{parked:?}\n{printed}"
    );
    assert_eq!(bench.briefs().len(), 3);
    assert_eq!(
        state_of(&bench).status(),
        Status::Parked {
            at: Stage::Build,
            awaiting_input: false
        }
    );
    let comment = bench.comments().pop().unwrap();
    assert!(
        comment.contains("`owlshift continue DEMO-1`") && !comment.contains("`owlshift do DEMO-1`"),
        "{comment}"
    );

    let (delivered, printed) = bench.continue_ticket(vec![bench.reply(Some("Hello"), Some(DONE))]);
    delivered.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    let roles: Vec<Role> = bench.briefs().iter().map(|b| b.role).collect();
    assert_eq!(
        roles,
        [
            Role::Build,
            Role::Build,
            Role::Build,
            Role::Build,
            Role::Verify
        ]
    );
    let cost = bench.record().cost.expect("cost.json");
    assert_eq!(
        (cost.runs, cost.unreported_runs, cost.spent_micros),
        (5, 2, 3_500_000)
    );
}

/// The role of each run of a bench and whether its brief was told of a
/// refused result, oldest first.
fn runs_of(bench: &Bench) -> Vec<(Role, bool)> {
    bench
        .briefs()
        .iter()
        .map(|brief| (brief.role, brief.result_refusal.is_some()))
        .collect()
}

/// OWL-188, the ceiling of OWL-183: a refused Build result is let off once
/// (the next run is told why), and the told run can ask a question the
/// resolver settles, after which a new refusal is let off again, so refusals
/// and resolver passes alternate. `settled` is never reset in a command, so
/// the alternation ends after [`PASSES`] settled passes, with a
/// round for the decider.
///
/// With N = [`PASSES`] = 3, the runs of the command are:
///
/// - N + 1 = 4 refused results, each let off: one before each of the N
///   settled passes and one before the run whose question hits the limit;
/// - the 4 told Build runs that ask (the 4th is past the limit, so its
///   question goes to the decider);
/// - N = 3 resolver runs.
///
/// 2 * (N + 1) + N = 11 runs, then one round. A fourth settled pass, or a
/// refusal let off after the limit, makes the command run on.
#[test]
fn refused_results_and_resolver_passes_alternate_up_to_the_ceiling() {
    assert_eq!(PASSES, 3, "the arithmetic above assumes 3");
    let bench = bench_at_passes();
    let misnumbered = NAMING.replace(r#""id":"Q1""#, r#""id":"Q2""#);
    let mut replies = Vec::new();
    let mut expected = Vec::new();
    for pass in 0..=PASSES {
        replies.push(bench.reply(None, Some(&misnumbered)));
        replies.push(bench.reply(None, Some(NAMING)));
        expected.extend([(Role::Build, false), (Role::Build, true)]);
        if pass < PASSES {
            replies.push(bench.reply(None, Some(DECIDED_Q1)));
            expected.push((Role::Resolver, false));
        }
    }
    let (outcome, printed) = bench.run(replies, None);
    let stop = outcome.expect_err("a round");
    assert_eq!(
        asked_texts(&stop),
        ["Which file should hold the greeting?"],
        "{printed}"
    );
    let runs = runs_of(&bench);
    assert_eq!(runs.len(), 11, "{runs:?}\n{printed}");
    assert_eq!(runs, expected, "{printed}");
    let resolved = data_of(&bench.events(), EventKind::Gate, "resolver");
    let verdicts: Vec<&str> = resolved
        .iter()
        .map(|gate| gate["resolver"].as_str().unwrap())
        .collect();
    assert_eq!(verdicts, ["done", "done", "done", "pass_limit"]);
}

/// OWL-188, with the late-comment bound added: a decider comment made while
/// the run that completes works gets one integration run (`integrating` is
/// never reset), whose own refusal is let off once too, and a further late
/// comment opens a round.
///
/// A `continue` after an answered round; the runs of the command, counted
/// as the bench's `during` hooks count them:
///
/// - run 1: the answer check;
/// - runs 2-10: N = 3 settled passes of (refused, asks, resolver);
/// - run 11: a refused result let off; run 12: its told retry completes,
///   while the decider comments late;
/// - run 13: the integration run, refused and let off; run 14: its told
///   retry completes, while the decider comments again, which opens a round
///   instead of delivering.
///
/// 1 + 9 + 2 + 2 = 14 runs, the last answered by a round about the comment.
#[test]
fn refusals_resolver_passes_and_a_late_comment_stop_at_the_ceiling() {
    assert_eq!(PASSES, 3, "the arithmetic above assumes 3");
    let bench = bench_at_passes();
    let check = answered_round(&bench);
    let misnumbered = NAMING.replace(r#""id":"Q1""#, r#""id":"Q2""#);
    let mut replies = vec![check];
    let mut expected = vec![(Role::AnswerCheck, false)];
    for _ in 0..PASSES {
        replies.push(bench.reply(None, Some(&misnumbered)));
        replies.push(bench.reply(None, Some(NAMING)));
        replies.push(bench.reply(None, Some(DECIDED_Q1)));
        expected.extend([
            (Role::Build, false),
            (Role::Build, true),
            (Role::Resolver, false),
        ]);
    }
    for greeting in ["Hello", "Hello and Bonjour"] {
        replies.push(bench.reply(None, Some(&misnumbered)));
        replies.push(bench.reply(Some(greeting), Some(DONE)));
        expected.extend([(Role::Build, false), (Role::Build, true)]);
    }
    let (stopped, printed) = bench.continue_during(
        replies,
        vec![
            bench.comment_during(12, "maintainer", "Also say Bonjour."),
            bench.comment_during(14, "maintainer", "And Hola."),
        ],
    );
    assert!(
        matches!(&stopped, Err(Stop::NeedsInput { posted: Ok(round), .. }) if round.get() == 2),
        "{stopped:?}\n{printed}"
    );
    // The first run of the bench is the `do` that asked the round.
    let runs = runs_of(&bench);
    assert_eq!(runs.len(), 15, "{runs:?}\n{printed}");
    assert_eq!(runs[1..], expected, "{printed}");
    assert_eq!(late_outcomes(&bench), [json!("integrate"), json!("ask")]);
    assert_eq!(bench.remote_branch(), None);
    assert!(bench.github.created.lock().unwrap().is_empty());
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

/// OWL-149: a comment that asks a person to act reached the ticket, but
/// keeping the state failed (the ticket's ref is locked):
/// the stop says which comment landed, so that the operator is notified.
#[test]
fn a_comment_on_the_ticket_whose_state_is_not_kept_is_a_stop_that_notifies() {
    fn not_kept(outcome: &Result<Delivered, Stop>, expected: Landed, comment: &str) {
        match outcome {
            Err(Stop::NotKept { landed, message }) if *landed == expected => {
                assert!(message.contains("but keeping"), "{message}");
                assert!(message.contains(comment), "{message}");
            }
            other => panic!("{other:?}"),
        }
    }
    let partial = check(&[
        ("Q1", "answered", "English."),
        ("Q2", "partial", "The ending is missing."),
    ]);

    let only_q2 = check(&[("Q2", "partial", "The ending is missing.")]);

    // The first round of questions.
    let bench = Bench::new(true);
    let (outcome, _) = bench.invoke(
        false,
        vec![bench.reply(None, Some(ROUND_1))],
        None,
        vec![bench.lock_ticket_ref_during(1)],
    );
    not_kept(&outcome, Landed::Questions, "the questions of round 1");
    assert!(bench.comments().last().unwrap().contains("Which language"));

    // A RESUME (OWL-169): it asks the decider nothing, but Build did not
    // resume, and the ticket ref still waits for the answers.
    let bench = Bench::new(true);
    let (asked, _) = bench.run(vec![bench.reply(None, Some(ROUND_1))], None);
    assert!(matches!(asked, Err(Stop::NeedsInput { .. })), "{asked:?}");
    bench.answer("Q1: English.\nQ2: \"Hello, reader.\", no sign-off.\n");
    let answered = check(&[
        ("Q1", "answered", "English."),
        ("Q2", "answered", "\"Hello, reader.\", no sign-off."),
    ]);
    let (outcome, _) = bench.continue_during(
        vec![bench.reply(None, Some(&answered))],
        vec![bench.lock_ticket_ref_during(1)],
    );
    not_kept(&outcome, Landed::Resume, "the resume is on the ticket");
    assert!(
        bench
            .comments()
            .last()
            .unwrap()
            .starts_with("[owlshift] RESUME · round 1\n")
    );
    assert_eq!(bench.briefs().len(), 2);
    assert_eq!(bench.record().state.waiting, Some(Waiting::NeedsInput));

    // A re-ask.
    let bench = Bench::new(true);
    let (asked, _) = bench.run(vec![bench.reply(None, Some(ROUND_1))], None);
    assert!(matches!(asked, Err(Stop::NeedsInput { .. })), "{asked:?}");
    bench.answer("Q2: words.\n");
    let (outcome, _) = bench.continue_during(
        vec![bench.reply(None, Some(&partial))],
        vec![bench.lock_ticket_ref_during(1)],
    );
    not_kept(&outcome, Landed::Reask, "the re-ask is on the ticket");
    assert!(bench.comments().last().unwrap().contains("re-ask 1"));

    // A reply to a counter-question.
    let bench = Bench::new(true);
    let (asked, _) = bench.run(vec![bench.reply(None, Some(ROUND_1))], None);
    assert!(matches!(asked, Err(Stop::NeedsInput { .. })), "{asked:?}");
    bench.answer("Q2: what is a sign-off?\n");
    let counter = check(&[
        ("Q1", "answered", "English."),
        ("Q2", "counter_question", "Asks what a sign-off is."),
    ]);
    let (outcome, _) = bench.continue_during(
        vec![bench.reply(None, Some(&counter))],
        vec![bench.lock_ticket_ref_during(1)],
    );
    not_kept(&outcome, Landed::Reply, "the reply is on the ticket");
    assert!(
        bench
            .comments()
            .last()
            .unwrap()
            .starts_with("[owlshift] REPLY")
    );

    // A park at the re-ask limit, its PARKED comment posted.
    let bench = Bench::parking();
    let (asked, _) = bench.run(vec![bench.reply(None, Some(ROUND_1))], None);
    assert!(matches!(asked, Err(Stop::NeedsInput { .. })), "{asked:?}");
    for n in 0..4 {
        bench.answer(&format!("Q2: answer {n}.\n"));
        let during = if n == 3 {
            vec![bench.lock_ticket_ref_during(1)]
        } else {
            Vec::new()
        };
        // Q1 is answered by the first check; the next ones judge Q2 alone.
        let result = if n == 0 { &partial } else { &only_q2 };
        let (outcome, _) = bench.continue_during(vec![bench.reply(None, Some(result))], during);
        if n == 3 {
            not_kept(
                &outcome,
                Landed::Parked,
                "the PARKED comment is on the ticket",
            );
        } else {
            assert!(matches!(&outcome, Err(Stop::Reasked { .. })), "{outcome:?}");
        }
    }
    assert!(
        bench
            .comments()
            .last()
            .unwrap()
            .starts_with("[owlshift] PARKED")
    );
}

/// OWL-159: a park whose state is not kept (the ticket's ref is locked) and
/// whose PARKED comment and stage the tracker refuses still notifies: the
/// run parked the ticket, which waits for a person whatever reached it.
#[test]
fn a_park_neither_kept_nor_posted_is_a_stop_that_notifies() {
    let partial = check(&[
        ("Q1", "answered", "English."),
        ("Q2", "partial", "The ending is missing."),
    ]);
    let only_q2 = check(&[("Q2", "partial", "The ending is missing.")]);
    let bench = Bench::parking();
    let (asked, _) = bench.run(vec![bench.reply(None, Some(ROUND_1))], None);
    assert!(matches!(asked, Err(Stop::NeedsInput { .. })), "{asked:?}");
    let mut outcome = None;
    for n in 0..4 {
        bench.answer(&format!("Q2: answer {n}.\n"));
        let mut during = Vec::new();
        if n == 3 {
            during.push(bench.lock_ticket_ref_during(1));
            bench.refuse.set(Some("[owlshift] PARKED"));
            bench.refuse_stage.set(true);
        }
        let result = if n == 0 { &partial } else { &only_q2 };
        let (stop, _) = bench.continue_during(vec![bench.reply(None, Some(result))], during);
        outcome = Some(stop);
    }
    let Some(Err(
        stop @ Stop::NotKept {
            landed: Landed::ParkedUnposted,
            ..
        },
    )) = &outcome
    else {
        panic!("{outcome:?}");
    };
    let message = stop.to_string();
    assert!(message.contains("but keeping"), "{message}");
    assert!(
        message.contains("the PARKED comment could not be posted"),
        "{message}"
    );
    // Nothing reached the ticket, and the ref still waits for answers.
    assert!(
        !bench
            .comments()
            .iter()
            .any(|comment| comment.starts_with("[owlshift] PARKED")),
        "{:?}",
        bench.comments()
    );
    let refused = data_of(&bench.events(), EventKind::Warning, "stage");
    assert_eq!(refused.last().unwrap()["stage"], "parked");
    assert_eq!(bench.record().state.waiting, Some(Waiting::NeedsInput));
    let line = owlshift_runner::notify::blocker_line(&ticket(), stop);
    assert!(
        line.as_deref()
            .is_some_and(|line| line.contains("its PARKED comment was not posted")),
        "{line:?}"
    );
}

// ---------------------------------------------------------------------------
// OWL-158: watch and a comment whose state is not kept.
// ---------------------------------------------------------------------------

/// OWL-169's acceptance: under `owlshift watch`, a RESUME that reached the
/// ticket while keeping its state failed (the ticket's ref is locked) is
/// posted once. The ref still waits for the answers, so without a hold every
/// pass past the 10-minute retry would run the answer check again and post
/// the RESUME again; with it, the ticket waits for a person. Here the
/// decider's next comment lifts it once the ref can be written: the RESUME
/// is posted a second time, an accepted residual, kept, and Build delivers.
#[test]
fn a_resume_whose_state_is_not_kept_is_posted_once_under_watch() {
    let bench = Bench::new(true);
    let (asked, printed) = bench.run(vec![bench.reply(None, Some(ROUND_1))], None);
    assert!(
        matches!(&asked, Err(Stop::NeedsInput { .. })),
        "{asked:?}\n{printed}"
    );
    let asked_at = bench.ref_commit();
    bench.answer("Q1: English.\nQ2: \"Hello, reader.\", no sign-off.\n");
    fs::write(bench.ref_lock(), "").unwrap();

    let ran = || bench.briefs().len();
    let resumes = || {
        bench
            .comments()
            .iter()
            .filter(|c| c.starts_with("[owlshift] RESUME"))
            .count()
    };
    let mut pass = 0;
    let mut sleep = |_: Duration| {
        pass += 1;
        match pass {
            // The answer check ran and posted the RESUME; its state was not
            // kept, and Build did not run.
            1 => {
                assert_eq!((ran(), resumes()), (2, 1));
                bench.waited.set(SignedDuration::from_mins(40));
            }
            // Past the 10-minute retry, then hours later: nothing ran and
            // nothing was posted again.
            2 => {
                assert_eq!((ran(), resumes()), (2, 1));
                bench.waited.set(SignedDuration::from_hours(3));
            }
            3 => {
                assert_eq!((ran(), resumes()), (2, 1));
                assert_eq!(bench.ref_commit(), asked_at);
                fs::remove_file(bench.ref_lock()).unwrap();
                bench.answer("Thanks.\n");
            }
            // The decider's comment lifted the hold: checked again, resumed,
            // kept, built, verified (OWL-304) and delivered.
            4 => {
                assert_eq!((ran(), resumes()), (5, 2));
                assert_ne!(bench.ref_commit(), asked_at);
                return false;
            }
            _ => unreachable!(),
        }
        true
    };
    let answered = || {
        bench.reply(
            None,
            Some(&check(&[
                ("Q1", "answered", "English."),
                ("Q2", "answered", "\"Hello, reader.\", no sign-off."),
            ])),
        )
    };
    let (outcomes, lines, printed) = bench.watch(
        vec![
            answered(),
            answered(),
            bench.reply(Some("Hello"), Some(DONE)),
        ],
        &mut sleep,
    );
    assert_eq!(pass, 4, "{lines}\n{printed}");
    // The second answer check is a stage step of its own (OWL-367).
    match &outcomes[..] {
        [
            Err(Stop::NotKept {
                landed: Landed::Resume,
                ..
            }),
            Err(Stop::Stepped {
                stage: Stage::Build,
                ..
            }),
            Ok(_),
        ] => {}
        other => panic!("{other:?}\n{lines}\n{printed}"),
    }
    // One line for the hold, and no 10-minute retry.
    assert_eq!(
        lines.matches("its state was not kept").count(),
        1,
        "{lines}"
    );
    assert!(!lines.contains("continues it again at"), "{lines}");
    // The operator's `continue` is named: the decider has nothing to answer.
    assert!(lines.contains("run `owlshift continue`"), "{lines}");
}

/// OWL-158's acceptance: under `owlshift watch`, a re-ask that reached the
/// ticket while keeping its state failed (the ticket's ref is locked) is not
/// posted again. Without a hold of its own, every pass past the 10-minute
/// retry would run the answer check again and post the re-ask again; with
/// it, the ticket waits for a person: here the decider, answering the
/// re-ask. Keeping fails a second time on that answer, which gets its own
/// re-ask, once, and once the ref can be written the third answer's re-ask
/// is kept: no ticket stays held.
#[test]
fn a_reask_whose_state_is_not_kept_is_posted_once_under_watch() {
    let bench = Bench::new(true);
    let (asked, printed) = bench.run(vec![bench.reply(None, Some(ROUND_1))], None);
    assert!(
        matches!(&asked, Err(Stop::NeedsInput { .. })),
        "{asked:?}\n{printed}"
    );
    let asked_at = bench.ref_commit();
    bench.answer("Q2: words.\n");
    let lock = bench
        .dirs()
        .checkout()
        .join(".git/refs/owlshift/tickets/DEMO-1.lock");
    fs::write(&lock, "").unwrap();

    let ran = || bench.briefs().len();
    let reasks = || {
        bench
            .comments()
            .iter()
            .filter(|c| c.starts_with("[owlshift] RE-ASK"))
            .count()
    };
    let mut pass = 0;
    let mut sleep = |_: Duration| {
        pass += 1;
        // This machine's clock only moves forward: `waited` is how far past
        // the next comment's time it reads.
        match pass {
            // The answer check ran and posted the re-ask; its state was not
            // kept.
            1 => {
                assert_eq!((ran(), reasks()), (2, 1));
                bench.waited.set(SignedDuration::from_mins(40));
            }
            // Past the 10-minute retry, then hours later: nothing ran and
            // nothing was posted again.
            2 => {
                assert_eq!((ran(), reasks()), (2, 1));
                bench.waited.set(SignedDuration::from_hours(3));
            }
            3 => {
                assert_eq!((ran(), reasks()), (2, 1));
                assert_eq!(bench.ref_commit(), asked_at);
                bench.answer("Q2: more words.\n");
            }
            // The decider's answer lifted the hold: checked, re-asked, and
            // keeping failed a second time.
            4 => {
                assert_eq!((ran(), reasks()), (3, 2));
                bench.waited.set(SignedDuration::from_hours(24));
            }
            5 => {
                assert_eq!((ran(), reasks()), (3, 2));
                fs::remove_file(&lock).unwrap();
                bench.answer("Q2: final words.\n");
            }
            // The ref can be written again: the re-ask is kept.
            6 => {
                assert_eq!((ran(), reasks()), (4, 3));
                assert_ne!(bench.ref_commit(), asked_at);
                return false;
            }
            _ => unreachable!(),
        }
        true
    };
    let partial = || {
        bench.reply(
            None,
            Some(&check(&[
                ("Q1", "answered", "English."),
                ("Q2", "partial", "The ending is missing."),
            ])),
        )
    };
    let (outcomes, lines, printed) = bench.watch(vec![partial(), partial(), partial()], &mut sleep);
    assert_eq!(pass, 6, "{lines}\n{printed}");
    match &outcomes[..] {
        [
            Err(Stop::NotKept {
                landed: Landed::Reask,
                ..
            }),
            Err(Stop::NotKept {
                landed: Landed::Reask,
                ..
            }),
            Err(Stop::Reasked { .. }),
        ] => {}
        other => panic!("{other:?}\n{lines}\n{printed}"),
    }
    // One line per hold, and no 10-minute retry.
    assert_eq!(
        lines.matches("its state was not kept").count(),
        2,
        "{lines}"
    );
    assert!(!lines.contains("continues it again at"), "{lines}");
}

/// OWL-209's acceptance: once `do` delivers, watch follows the pull request
/// without a command typed, records it going red, then conflicting, then
/// merged, each as one event, nothing for a pass that reads no change, and
/// reads it no more once merged.
#[test]
fn watch_follows_a_delivered_pull_request_until_it_is_merged() {
    let bench = Bench::new(true);
    let (outcome, printed) = bench.run(vec![bench.reply(Some("Hello"), Some(DONE))], None);
    outcome.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    let delivered_events = bench.events().len();

    // What the forge shows before each pass: green twice, red, red again,
    // conflicting, merged, then two passes with nothing to read.
    let github = bench.github.clone();
    let mut pass = 0;
    let (outcomes, lines, printed) = bench.watch(Vec::new(), &mut |_| {
        pass += 1;
        let mut followed = github.followed.lock().unwrap();
        match pass {
            // Red on the base too: nothing to fix (OWL-112 fixes a red of
            // the pull request's own).
            2 | 3 => {
                followed.conclusion = "FAILURE";
                followed.base_conclusion = "FAILURE";
            }
            4 => followed.mergeable = "CONFLICTING",
            5 => followed.state = "MERGED",
            _ => {}
        }
        pass < 7
    });
    // The conflict the forge reports goes to a rebase repair, which finds
    // the base already in the head here: no run (OWL-210).
    assert!(
        matches!(&outcomes[..], [Err(Stop::Refused(why))] if why.contains("nothing to rebase")),
        "{outcomes:?}"
    );

    let events = &bench.events()[delivered_events..];
    let decided = data_of(events, EventKind::Decision, "watch");
    let seen: Vec<(&Value, &Value, &Value, &Value)> = decided
        .iter()
        .map(|d| (&d["watch"], &d["verdict"], &d["mergeable"], &d["action"]))
        .collect();
    assert_eq!(
        seen,
        [
            (
                &json!("read"),
                &json!("green"),
                &json!("mergeable"),
                &json!("green")
            ),
            (
                &json!("verdict"),
                &json!("red"),
                &json!("mergeable"),
                &json!("wait_base_red")
            ),
            (
                &json!("mergeable"),
                &json!("red"),
                &json!("conflicting"),
                &json!("rebase")
            ),
            (
                &json!("ended"),
                &json!("not_open"),
                &json!("conflicting"),
                &json!("end")
            ),
        ],
        "{lines}\n{printed}"
    );
    assert_eq!(decided[3]["reason"], "merged");
    assert!(
        events.iter().all(|e| e.kind == EventKind::Decision),
        "no warning nor any other event: {events:?}"
    );
    // Seven passes: the sixth read it merged, the seventh read nothing; the
    // rebase repair read it once more under the lock.
    assert_eq!(*github.follow_reads.lock().unwrap(), 7);
    let delivery = bench.record().delivery.unwrap();
    assert!(!delivery.followed());
    assert_eq!(
        delivery.ended.unwrap().why,
        owlshift_contracts::refs::EndedWhy::Merged
    );
    assert!(lines.contains("DEMO-1: pull request #1"), "{lines}");
    assert!(
        lines.contains("it was merged: watch no longer follows it"),
        "{lines}"
    );
    assert!(
        lines.contains("watch follows the pull requests of DEMO-1"),
        "{lines}"
    );
    assert!(
        lines.ends_with("no ticket's questions wait for an answer\n"),
        "{lines}"
    );
}

/// A fix run's `done`: the greeting committed again.
const FIXED: &str = r#"{"format":13,"status":"done","summary":"Kept the greeting on every platform.",
"pr":{"branch":"owlshift/demo-1","title":"Add a greeting","body":"Fixed."}}"#;

/// A delivered ticket, its pull request followed.
fn delivered() -> Bench {
    deliver(Bench::new(true))
}

/// `bench`, its ticket delivered by `owlshift do`.
fn deliver(bench: Bench) -> Bench {
    let (outcome, printed) = bench.run(vec![bench.reply(Some("Hello"), Some(DONE))], None);
    outcome.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    bench
}

/// The ticket's state as its ref keeps it.
fn state_of(bench: &Bench) -> TicketState {
    TicketState::try_from(&bench.record().state).unwrap()
}

/// The ticket's stage line on the tracker, rewritten by a person.
fn move_ticket(bench: &Bench, stage: &str) {
    let file = bench.ticket_file();
    let text = fs::read_to_string(&file).unwrap();
    let line = text.lines().find(|l| l.starts_with("stage = ")).unwrap();
    fs::write(&file, text.replace(line, &format!("stage = \"{stage}\""))).unwrap();
}

/// Scenario S9's red-check part (OWL-112): watch reads the delivered pull
/// request red and runs a fix run briefed with the failed check and its
/// log's excerpt, whose commit passes the gate and is pushed to the same
/// branch with a lease, with a DELIVERY report naming the pass. A green
/// read clears the passes; two more fixes reach the bound, and the next red
/// parks the ticket with a PARKED comment and no fourth run.
#[test]
fn a_red_pull_request_gets_fix_runs_until_the_bound_parks_it() {
    let bench = delivered();
    let delivered_events = bench.events().len();
    let first_push = bench.remote_branch().unwrap();
    let github = bench.github.clone();
    let mut pass = 0;
    let replies = vec![
        bench.reply(Some("Hello again"), Some(FIXED)),
        bench.reply(Some("Hello, again and again"), Some(FIXED)),
        bench.reply(Some("Hello, at last"), Some(FIXED)),
    ];
    // What the forge shows at each pass: red, green, then red.
    github.followed.lock().unwrap().conclusion = "FAILURE";
    let (outcomes, lines, printed) = bench.watch(replies, &mut |_| {
        pass += 1;
        github.followed.lock().unwrap().conclusion = match pass {
            1 => "SUCCESS",
            _ => "FAILURE",
        };
        pass < 6
    });
    // Red: fix 1. Green: cleared. Red: fix 2, then fix 3. Red: parked.
    assert_eq!(outcomes.len(), 4, "{outcomes:?}\n{lines}\n{printed}");
    for outcome in &outcomes[..3] {
        let delivered = outcome
            .as_ref()
            .unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
        assert!(!delivered.opened);
    }
    assert!(
        matches!(
            &outcomes[3],
            Err(Stop::Parked {
                reason: ParkReason::FixPasses,
                ..
            })
        ),
        "{:?}",
        outcomes[3]
    );
    let state = state_of(&bench);
    assert!(matches!(state.status(), Status::Parked { .. }), "{state:?}");
    assert_eq!((state.fix_passes(), state.repairs()), (2, 3));
    // Every run is counted in the ticket's spend, Watch's fix runs too
    // (OWL-378): the delivery's Build and Verify, then the three fixes.
    assert_eq!(bench.record().cost.expect("cost.json").runs, 5);

    // Each fix run was the build role at Watch, told the failed check and
    // its excerpt: the job log's lines up to its first error, cleaned.
    // The delivery's Build and Verify pass (OWL-304), then the fix runs.
    let briefs = bench.briefs();
    assert_eq!(briefs.len(), 5);
    for brief in &briefs[2..] {
        assert_eq!(brief.role, Role::Build);
        let failure = brief.check_failure.as_ref().expect("a check failure");
        assert_eq!(failure.pull_request, 1);
        let check = &failure.checks[0];
        assert_eq!(
            (check.name.as_str(), check.conclusion.as_str()),
            ("ci", "FAILURE")
        );
        assert_eq!(
            check.excerpt.as_deref(),
            Some(
                "##[group]Run git grep -q Hello\nerror: GREETING.md says hello on macOS only\n\
                 ##[error]Process completed with exit code 1.\n"
            )
        );
    }
    assert!(briefs[0].check_failure.is_none());
    assert_eq!(github.log.lock().unwrap().1, 3);

    // Each fix landed on the same branch, a fast-forward leased on the push
    // before it; the ref follows the last one.
    let events = &bench.events()[delivered_events..];
    let pushes = data_of(events, EventKind::TrackerWrite, "lease");
    assert_eq!(pushes.len(), 3);
    assert_eq!(pushes[0]["lease"], json!(first_push));
    assert!(
        pushes.iter().all(|p| p["pushed"] == "fast_forward"),
        "{pushes:?}"
    );
    assert_eq!(pushes[1]["lease"], pushes[0]["commit"]);
    let last = bench.remote_branch().unwrap();
    assert_eq!(pushes[2]["commit"], json!(last));
    let delivery = bench.record().delivery.unwrap();
    assert_eq!(delivery.pushed.as_deref(), Some(last.as_str()));
    assert!(delivery.pushing.is_none() && delivery.followed());

    // A DELIVERY report per fix, naming its pass, then the PARKED comment.
    let comments = bench.comments();
    let reports: Vec<&String> = comments
        .iter()
        .filter(|c| c.starts_with("[owlshift] DELIVERY"))
        .collect();
    assert_eq!(reports.len(), 4);
    for (report, pass) in reports[1..].iter().zip([1, 1, 2]) {
        assert!(
            report.contains(&format!(
                "**Fix pass {pass} of 2 after delivery,** for the red checks `ci`."
            )),
            "{report}"
        );
    }
    let parked = comments.last().unwrap();
    assert!(parked.starts_with("[owlshift] PARKED"), "{parked}");
    assert!(parked.contains("after 2 fix passes"), "{parked}");
    let decided = data_of(events, EventKind::Decision, "watch");
    assert!(decided.iter().any(|d| d["action"] == "fix"), "{decided:?}");
    assert_eq!(bench.stage(), "Needs Input");
}

/// A ticket a person moved out of Owlshift's states gets no fix run while
/// it stays there, and one once it is back (architecture section 5, "A
/// ticket a person moved").
#[test]
fn a_moved_ticket_waits_and_gets_its_fix_run_once_back() {
    let bench = delivered();
    bench.github.followed.lock().unwrap().conclusion = "FAILURE";
    move_ticket(&bench, "Done");
    let (outcome, printed) = bench.continue_ticket(Vec::new());
    assert!(
        matches!(&outcome, Err(Stop::Refused(why)) if why.contains("\"Done\"")),
        "{outcome:?}\n{printed}"
    );
    // The delivery's Build and Verify pass alone (OWL-304).
    assert_eq!(bench.briefs().len(), 2);
    assert!(
        data_of(&bench.events(), EventKind::Decision, "watch")
            .iter()
            .any(|d| d["watch"] == "stage_moved")
    );

    move_ticket(&bench, "In Review");
    let (outcome, printed) =
        bench.continue_ticket(vec![bench.reply(Some("Hello again"), Some(FIXED))]);
    outcome.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    assert_eq!(bench.briefs().len(), 3);
    assert_eq!(state_of(&bench).fix_passes(), 1);
}

/// The visible stages the commands after event `from` moved the ticket to,
/// by their `[tracker].states` key.
fn stage_moves(bench: &Bench, from: usize) -> Vec<String> {
    data_of(&bench.events()[from..], EventKind::TrackerWrite, "action")
        .into_iter()
        .filter(|write| write["action"] == "stage")
        .map(|write| write["stage"].as_str().unwrap().to_owned())
        .collect()
}

/// While the `run`th run of a command works, the ticket's stage on the
/// tracker, kept in `seen`.
fn stage_during(bench: &Bench, run: usize, seen: &Rc<RefCell<Option<String>>>) -> During {
    let (checkout, seen) = (bench.remote.checkout.clone(), seen.clone());
    (
        run,
        Box::new(move || {
            let stage = MarkdownTracker::new(&checkout)
                .ticket(&ticket())
                .unwrap()
                .stage;
            *seen.borrow_mut() = Some(stage);
        }),
    )
}

/// OWL-259: a fix run shows its ticket as working while it works the pull
/// request, and as review again once its push has landed. It runs on the
/// ticket's routing, the labels read at its dispatch (OWL-305).
#[test]
fn a_fix_run_shows_working_until_its_push_lands() {
    let bench = delivered();
    assert_eq!(bench.stage(), "In Review");
    bench.add_labels(&["tier:deep", "effort:max"]);
    bench.github.followed.lock().unwrap().conclusion = "FAILURE";
    let from = bench.events().len();
    let seen = Rc::new(RefCell::new(None));
    let (fixed, printed) = bench.continue_during(
        vec![bench.reply(Some("Hello again"), Some(FIXED))],
        vec![stage_during(&bench, 1, &seen)],
    );
    fixed.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    assert_eq!(seen.borrow().as_deref(), Some("In Progress"));
    assert_eq!(stage_moves(&bench, from), ["working", "review"]);
    assert_eq!(bench.stage(), "In Review");
    assert_eq!(
        bench.routed(),
        [
            claude(Tier::Standard, None),
            claude(Tier::Deep, Some(Effort::Max))
        ],
        "the Build, then the fix run"
    );
}

/// A routing on Claude Code.
fn claude(tier: Tier, effort: Option<Effort>) -> RunRouting {
    RunRouting {
        harness: owlshift_contracts::Harness::Claude,
        tier,
        effort,
    }
}

/// OWL-259: a fix run a usage limit stops leaves the ticket at Watch, with
/// no run working: it shows review again.
#[test]
fn a_fix_run_a_usage_limit_stops_shows_review_again() {
    let bench = delivered();
    bench.github.followed.lock().unwrap().conclusion = "FAILURE";
    let from = bench.events().len();
    let mut limited = bench.reply(None, None);
    limited.usage_limit = Some("2026-10-03T18:00:00Z".parse().unwrap());
    let (stopped, printed) = bench.continue_ticket(vec![limited]);
    assert!(
        matches!(&stopped, Err(Stop::UsageLimit { .. })),
        "{stopped:?}\n{printed}"
    );
    assert_eq!(stage_moves(&bench, from), ["working", "review"]);
    assert_eq!(bench.stage(), "In Review");
}

/// OWL-259: a stage a person or an integration sets while a fix run works
/// (done, at a merge) is kept when the run ends: the move back to review is
/// made only from working.
#[test]
fn a_stage_set_during_a_fix_run_is_kept() {
    let bench = delivered();
    bench.github.followed.lock().unwrap().conclusion = "FAILURE";
    let from = bench.events().len();
    let file = bench.ticket_file();
    let (fixed, printed) = bench.continue_during(
        vec![bench.reply(Some("Hello again"), Some(FIXED))],
        vec![(
            1,
            Box::new(move || {
                let text = fs::read_to_string(&file).unwrap();
                let line = text.lines().find(|l| l.starts_with("stage = ")).unwrap();
                fs::write(&file, text.replace(line, "stage = \"Done\"")).unwrap();
            }),
        )],
    );
    fixed.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    assert_eq!(stage_moves(&bench, from), ["working"]);
    assert_eq!(bench.stage(), "Done");
}

/// OWL-259: a tracker that refuses the move to working is warned of, and
/// the fix run goes on and pushes.
#[test]
fn a_refused_move_to_working_is_a_warning_and_the_fix_runs() {
    let bench = delivered();
    bench.github.followed.lock().unwrap().conclusion = "FAILURE";
    bench.refuse_stage.set(true);
    let from = bench.events().len();
    let (fixed, printed) =
        bench.continue_ticket(vec![bench.reply(Some("Hello again"), Some(FIXED))]);
    fixed.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    let warned: Vec<Value> = data_of(&bench.events()[from..], EventKind::Warning, "what")
        .into_iter()
        .filter(|w| w["what"] == "stage_not_moved")
        .collect();
    assert_eq!(warned.len(), 1, "{warned:?}");
    assert_eq!(warned[0]["stage"], "working");
    assert_eq!(state_of(&bench).fix_passes(), 1);
    assert_eq!(bench.stage(), "In Review");
}

/// A delivered ticket whose first watch pass has run, so the owed report
/// is posted and the ticket ref stays as it is while its pull request does,
/// shown as working, as a stop that did not move it back leaves it (OWL-265).
fn left_working(bench: &Bench) {
    let (outcomes, lines, printed) = bench.watch(Vec::new(), &mut |_| false);
    assert!(outcomes.is_empty(), "{outcomes:?}\n{lines}\n{printed}");
    assert_eq!(bench.stage(), "In Review");
    move_ticket(bench, "In Progress");
}

/// OWL-265's acceptance: a ticket at Watch that a stop left at working, with
/// no run working it, shows review again at the next pass. A person who moves
/// it to working again, its ticket ref unchanged, is left alone.
#[test]
fn a_stage_a_stop_left_at_working_shows_review_again_once() {
    let bench = delivered();
    left_working(&bench);
    let (from, commit) = (bench.events().len(), bench.ref_commit());
    let mut pass = 0;
    let (outcomes, lines, printed) = bench.watch(Vec::new(), &mut |_| {
        pass += 1;
        if pass == 1 {
            // Moved at the first pass, then to working again by a person.
            assert_eq!(bench.stage(), "In Review");
            move_ticket(&bench, "In Progress");
        }
        pass < 2
    });
    assert!(outcomes.is_empty(), "{outcomes:?}\n{lines}\n{printed}");
    assert_eq!(stage_moves(&bench, from), ["review"]);
    assert_eq!(bench.stage(), "In Progress");
    assert_eq!(bench.ref_commit(), commit);
    assert!(
        lines.contains("DEMO-1 showed working with no run working it: shown as In Review again"),
        "{lines}"
    );
}

/// OWL-265: a park or a round at Watch whose own move the tracker refused,
/// left at working, shows the stage it waits in: parked, needs input.
#[test]
fn a_park_or_a_round_left_at_working_shows_its_own_stage() {
    let parked = deliver(Bench::parking());
    parked_at_watch(&parked);
    move_ticket(&parked, "In Progress");
    let (outcomes, lines, printed) = parked.watch(Vec::new(), &mut |_| false);
    assert!(outcomes.is_empty(), "{outcomes:?}\n{lines}\n{printed}");
    assert_eq!(parked.stage(), "Parked");

    let asked = delivered();
    asked_at_watch(&asked);
    move_ticket(&asked, "In Progress");
    let (outcomes, lines, printed) = asked.watch(Vec::new(), &mut |_| false);
    assert!(outcomes.is_empty(), "{outcomes:?}\n{lines}\n{printed}");
    assert_eq!(asked.stage(), "Needs Input");
}

/// OWL-265's guard: a stage read as working is moved only under the
/// project's lock, while the project is not quarantined, the ticket ref is
/// the one the pass read and a second read still shows working. Whatever
/// changes right after the first read, the pass moves nothing; the next pass
/// moves a stage still left at working once the lock is free.
#[test]
fn a_stage_left_at_working_is_moved_only_under_the_lock_and_unchanged() {
    // The lock taken, as by a command typed meanwhile.
    let bench = delivered();
    left_working(&bench);
    let held = Rc::new(RefCell::new(None));
    let (dirs, keep) = (bench.dirs(), held.clone());
    *bench.after_stage_read.borrow_mut() = Some(Box::new(move || {
        *keep.borrow_mut() = Some(dirs.lock().unwrap().expect("a free lock"));
    }));
    let from = bench.events().len();
    bench.watch(Vec::new(), &mut |_| false);
    assert!(held.borrow().is_some());
    assert_eq!(stage_moves(&bench, from), Vec::<String>::new());
    assert_eq!(bench.stage(), "In Progress");
    held.borrow_mut().take();
    bench.watch(Vec::new(), &mut |_| false);
    assert_eq!(stage_moves(&bench, from), ["review"]);

    // A run stopped meanwhile, leaving the project for a person to look at.
    let bench = delivered();
    left_working(&bench);
    let marker = bench.dirs().unverified_file();
    *bench.after_stage_read.borrow_mut() = Some(Box::new(move || {
        fs::write(&marker, "a run stopped\n").unwrap();
    }));
    let from = bench.events().len();
    bench.watch(Vec::new(), &mut |_| false);
    assert_eq!(stage_moves(&bench, from), Vec::<String>::new());
    assert_eq!(bench.stage(), "In Progress");

    // The ticket ref moved, as by a command typed meanwhile.
    let bench = delivered();
    left_working(&bench);
    let (env, checkout) = (bench.env.clone(), bench.dirs().checkout());
    let (record, commit) = (bench.record(), bench.ref_commit());
    *bench.after_stage_read.borrow_mut() = Some(Box::new(move || {
        let git = Git::with_setup("git", move |command| env.apply(command));
        let moved = ticket_ref::write(&git, &checkout, &ticket(), &record, Some(&commit)).unwrap();
        assert_ne!(moved, commit);
    }));
    let from = bench.events().len();
    bench.watch(Vec::new(), &mut |_| false);
    assert_eq!(stage_moves(&bench, from), Vec::<String>::new());
    assert_eq!(bench.stage(), "In Progress");

    // A repair ended meanwhile, moving the stage back itself.
    let bench = delivered();
    left_working(&bench);
    let file = bench.ticket_file();
    *bench.after_stage_read.borrow_mut() = Some(Box::new(move || {
        let text = fs::read_to_string(&file).unwrap();
        let line = text.lines().find(|l| l.starts_with("stage = ")).unwrap();
        fs::write(&file, text.replace(line, "stage = \"In Review\"")).unwrap();
    }));
    let from = bench.events().len();
    bench.watch(Vec::new(), &mut |_| false);
    assert_eq!(stage_moves(&bench, from), Vec::<String>::new());
    assert_eq!(bench.stage(), "In Review");
}

/// OWL-265: a tracker that refuses the move is warned of once, and the move
/// is not tried again at each pass.
#[test]
fn a_refused_move_from_a_stage_left_at_working_is_warned_of_once() {
    let bench = delivered();
    left_working(&bench);
    bench.refuse_stage.set(true);
    let from = bench.events().len();
    let mut pass = 0;
    bench.watch(Vec::new(), &mut |_| {
        pass += 1;
        pass < 3
    });
    let warned: Vec<Value> = data_of(&bench.events()[from..], EventKind::Warning, "what")
        .into_iter()
        .filter(|w| w["what"] == "stage_not_moved")
        .collect();
    assert_eq!(warned.len(), 1, "{warned:?}");
    assert_eq!(warned[0]["stage"], "review");
    assert_eq!(bench.stage(), "In Progress");
}

/// A repair run's question about something only a person holds (scenario
/// S9).
const ASKS_FOR_A_VARIABLE: &str = r#"{"format":13,"status":"questions",
"summary":"The red is a missing environment variable.",
"questions":[{"id":"Q1","category":"secrets","context":"CI lacks the GREETING_LOCALE variable the check reads.",
"text":"Can someone set GREETING_LOCALE in CI and re-run the check?","recommendation":{"why":"The simplest reading of the ticket."}}]}"#;

/// The replies of a repair run that asks for a CI variable, and of the
/// resolver it goes to first (OWL-342), which passes on a prerequisite only a
/// person can supply.
fn variable_asked(bench: &Bench) -> Vec<Reply> {
    vec![
        bench.reply(None, Some(ASKS_FOR_A_VARIABLE)),
        bench.reply(None, Some(&passed_on(&["Q1"]))),
    ]
}

/// A red pull request whose fix run asks, through `continue`: the round
/// goes through the resolver, which passes it on (OWL-342), then to the
/// decider, and returns to Watch with the fix passes and repairs kept
/// (OWL-211).
fn asked_at_watch(bench: &Bench) {
    bench.github.followed.lock().unwrap().conclusion = "FAILURE";
    let before = state_of(bench);
    let from = bench.events().len();
    let (asked, printed) = bench.continue_ticket(variable_asked(bench));
    assert!(
        matches!(&asked, Err(Stop::NeedsInput { posted: Ok(_), .. })),
        "{asked:?}\n{printed}"
    );
    let state = state_of(bench);
    assert_eq!(
        state.status(),
        Status::NeedsInput {
            return_to: Stage::Watch
        }
    );
    assert_eq!(
        (state.fix_passes(), state.repairs()),
        (before.fix_passes(), before.repairs())
    );
    let resolver = bench.last_of(Role::Resolver);
    assert_eq!(resolver.asked_by.as_ref().unwrap().role, Role::Build);
    // Working while the fix run worked, then waiting for the decider
    // (OWL-259).
    assert_eq!(stage_moves(bench, from), ["working", "needs_input"]);
    assert_eq!(bench.stage(), "Needs Input");
    let comments = bench.comments();
    assert!(!comments.iter().any(|c| c.starts_with("[owlshift] PARKED")));
    let asked = comments.last().unwrap();
    assert!(
        asked.starts_with("[owlshift] QUESTIONS · round 1"),
        "{asked}"
    );
    assert!(
        asked.contains(
            "The fix run of pull request #1 asks before it can go on. The red is a missing \
             environment variable."
        ),
        "{asked}"
    );
    assert!(asked.contains("set GREETING_LOCALE in CI"), "{asked}");
}

/// Scenario S9's way back to the decider (OWL-211): after one fix pass, a
/// red whose fix needs a missing environment variable asks the decider, the
/// fix passes kept. Once answered, the RESUME says watch reads the pull
/// request again, the ticket shows review, and the read, still red, runs a
/// fix run whose thread holds the round and the answer; its commit lands on
/// the same branch, and the pull request then reads green.
#[test]
fn a_fix_run_that_asks_goes_to_the_decider_and_the_answer_resumes_watch() {
    let bench = delivered();
    bench.github.followed.lock().unwrap().conclusion = "FAILURE";
    let (fixed, printed) =
        bench.continue_ticket(vec![bench.reply(Some("Hello again"), Some(FIXED))]);
    fixed.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    asked_at_watch(&bench);
    let asked_push = bench.remote_branch().unwrap();

    bench.answer("Q1: Set, and the check re-run. Go.\n");
    let before = bench.events().len();
    let (fixed, printed) = bench.continue_ticket(vec![
        bench.reply(
            None,
            Some(&check(&[("Q1", "answered", "The variable is set.")])),
        ),
        bench.reply(Some("Hello, at last"), Some(FIXED)),
    ]);
    let delivered = fixed.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    assert!(!delivered.opened);
    assert!(bench.comments().iter().any(|c| c.contains(
        "Every question of round 1 is answered. Watch reads the pull request again, and a \
         repair still due starts with this understanding:"
    )));
    assert_eq!(bench.stage(), "In Review");
    let fix = bench.briefs().pop().unwrap();
    assert!(fix.check_failure.is_some());
    assert_eq!(last_answered(&fix), ["Q1"]);
    assert!(
        decider_says(&fix)
            .iter()
            .any(|said| said.contains("Set, and the check re-run"))
    );
    let pushes = data_of(&bench.events()[before..], EventKind::TrackerWrite, "lease");
    assert_eq!(pushes.len(), 1);
    assert_eq!(pushes[0]["lease"], json!(asked_push));
    assert_eq!(pushes[0]["pushed"], "fast_forward");
    assert_eq!(
        bench.record().delivery.unwrap().pushed,
        bench.remote_branch()
    );
    assert_eq!(state_of(&bench).fix_passes(), 2);

    bench.github.followed.lock().unwrap().conclusion = "SUCCESS";
    let (look, printed) = bench.continue_ticket(Vec::new());
    assert!(
        matches!(look, Err(Stop::Watched { .. })),
        "{look:?}\n{printed}"
    );
    assert_eq!(state_of(&bench).fix_passes(), 0);
}

/// A fix run's question the resolver decides at Watch (OWL-342): no round
/// opens, the fix run runs again with the decision in its thread, an attempt
/// it does not count, and its push lands; the ticket stays at Watch.
#[test]
fn a_fix_runs_question_the_resolver_decides_runs_the_fix_again() {
    let bench = delivered();
    bench.github.followed.lock().unwrap().conclusion = "FAILURE";
    let decides = r#"{"format":13,"status":"done","summary":"The workflow sets it.",
"resolutions":[{"question":"Q1","outcome":"decided","category":"ci_config",
"decision":"The check's workflow sets GREETING_LOCALE: read it from there.",
"basis":"The workflow file sets GREETING_LOCALE.","basis_kind":"fact"}]}"#;
    let (fixed, printed) = bench.continue_ticket(vec![
        bench.reply(None, Some(ASKS_FOR_A_VARIABLE)),
        bench.reply(None, Some(decides)),
        bench.reply(Some("Hello again"), Some(FIXED)),
    ]);
    fixed.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    let briefs = bench.briefs();
    let roles: Vec<Role> = briefs[briefs.len() - 3..].iter().map(|b| b.role).collect();
    assert_eq!(roles, [Role::Build, Role::Resolver, Role::Build]);
    let again = briefs.last().unwrap();
    assert!(
        again
            .thread
            .iter()
            .any(|e| matches!(e, ThreadEntry::Decision { .. }))
    );
    let comments = bench.comments();
    assert!(
        comments
            .iter()
            .any(|c| c.starts_with("[owlshift] DECISION"))
    );
    assert!(
        !comments
            .iter()
            .any(|c| c.starts_with("[owlshift] QUESTIONS"))
    );
    let attempts: Vec<Value> = data_of(&bench.events(), EventKind::RunStarted, "attempt")
        .into_iter()
        .filter(|data| data["role"] == "build")
        .map(|data| data["attempt"].clone())
        .collect();
    assert_eq!(attempts[attempts.len() - 2..], [json!(1), json!(1)]);
    assert_eq!(state_of(&bench).status(), Status::Active(Stage::Watch));
    assert_eq!(state_of(&bench).fix_passes(), 1);
}

/// Under `owlshift watch`, a fix run's round that reached the ticket while
/// keeping it failed (the ticket's ref is locked) is posted once: the
/// repair's hold stays across passes for a followed ticket, so the fix does
/// not run and ask again at each pass (OWL-211). The operator's `continue`,
/// once the ref can be written, reads the pull request again and asks again.
#[test]
fn a_round_whose_state_is_not_kept_under_a_repair_is_posted_once() {
    let bench = delivered();
    bench.github.followed.lock().unwrap().conclusion = "FAILURE";
    fs::write(bench.ref_lock(), "").unwrap();
    let asked = || {
        bench
            .comments()
            .iter()
            .filter(|c| c.starts_with("[owlshift] QUESTIONS"))
            .count()
    };
    let mut pass = 0;
    let (outcomes, lines, printed) = bench.watch(variable_asked(&bench), &mut |_| {
        pass += 1;
        // The delivery's two runs (OWL-304), then the fix run that asked
        // and the resolver it went to first (OWL-342).
        assert_eq!((bench.briefs().len(), asked()), (4, 1));
        bench.waited.set(SignedDuration::from_hours(pass * 3));
        pass < 3
    });
    match &outcomes[..] {
        [
            Err(Stop::NotKept {
                landed: Landed::Questions,
                message,
            }),
        ] => assert!(
            message.contains("run `owlshift continue DEMO-1` to read the pull request again"),
            "{message}"
        ),
        other => panic!("{other:?}\n{lines}\n{printed}"),
    }
    fs::remove_file(bench.ref_lock()).unwrap();
    let (outcome, printed) = bench.continue_ticket(variable_asked(&bench));
    assert!(
        matches!(&outcome, Err(Stop::NeedsInput { posted: Ok(_), .. })),
        "{outcome:?}\n{printed}"
    );
    assert_eq!(asked(), 2);
}

/// A fix run that finds its premise false with no question asks the decider
/// nothing: it parks the ticket, so watch does not run it again at each
/// pass (OWL-211).
#[test]
fn a_false_premise_without_a_question_parks_the_fix() {
    let bench = delivered();
    bench.github.followed.lock().unwrap().conclusion = "FAILURE";
    let premise = r#"{"format":13,"status":"premise_false","summary":"The check tests a file this branch never had.","questions":[]}"#;
    let (outcome, printed) = bench.continue_ticket(vec![bench.reply(None, Some(premise))]);
    assert!(
        matches!(
            &outcome,
            Err(Stop::Parked {
                reason: ParkReason::Blocked,
                ..
            })
        ),
        "{outcome:?}\n{printed}"
    );
    let parked = bench.comments().pop().unwrap();
    assert!(parked.starts_with("[owlshift] PARKED"), "{parked}");
    assert!(
        parked.contains("found its premise false, with no question"),
        "{parked}"
    );
    assert!(matches!(state_of(&bench).status(), Status::Parked { .. }));
}

/// A pull request read green once the round is answered gets no fix run:
/// nothing resumes a remembered run (OWL-211).
#[test]
fn an_answer_after_the_pull_request_went_green_runs_no_fix() {
    let bench = delivered();
    asked_at_watch(&bench);
    bench.answer("Q1: Set, and the check re-run. Go.\n");
    bench.github.followed.lock().unwrap().conclusion = "SUCCESS";
    let (look, printed) = bench.continue_ticket(vec![bench.reply(
        None,
        Some(&check(&[("Q1", "answered", "The variable is set.")])),
    )]);
    assert!(
        matches!(look, Err(Stop::Watched { .. })),
        "{look:?}\n{printed}"
    );
    let roles: Vec<Role> = bench.briefs().iter().map(|b| b.role).collect();
    assert_eq!(
        roles,
        [
            Role::Build,
            Role::Verify,
            Role::Build,
            Role::Resolver,
            Role::AnswerCheck
        ]
    );
    assert_eq!(state_of(&bench).status(), Status::Active(Stage::Watch));
    assert_eq!(bench.stage(), "In Review");
}

/// OWL-242's acceptance: a round waiting at Watch ends with its pull
/// request. A red read leaves it waiting, with no base read and no event; the
/// merge, read while the ticket ref cannot be written, moves the stage to
/// review and leaves the round to the next pass, which closes it: the ticket
/// back at Watch, the following ended, one `ended` event naming the round.
/// The decider's answer, posted with the merge, starts nothing.
#[test]
fn a_round_waiting_at_watch_closes_when_its_pull_request_is_merged() {
    let bench = delivered();
    asked_at_watch(&bench);
    let waiting = state_of(&bench);
    let (briefs, comments) = (bench.briefs().len(), bench.comments().len() + 1);
    let before = bench.events().len();
    let github = bench.github.clone();
    let reads = || {
        (
            *github.follow_reads.lock().unwrap(),
            *github.base_reads.lock().unwrap(),
        )
    };
    let (follow_reads, base_reads) = reads();
    let mut pass = 0;
    let (outcomes, lines, printed) = bench.watch(Vec::new(), &mut |_| {
        pass += 1;
        match pass {
            // Red: still waiting, nothing recorded.
            1 => {
                assert_eq!(state_of(&bench), waiting);
                assert_eq!(bench.events().len(), before);
                // Past the quiet window: the answer counts.
                bench.answer("Q1: Set, and the check re-run. Go.\n");
                bench.waited.set(SignedDuration::from_mins(30));
                github.followed.lock().unwrap().state = "MERGED";
                fs::write(bench.ref_lock(), "").unwrap();
            }
            // The stage moved, the ref not written: still waiting.
            2 => {
                assert_eq!(state_of(&bench), waiting);
                assert_eq!(bench.stage(), "In Review");
                fs::remove_file(bench.ref_lock()).unwrap();
            }
            _ => {}
        }
        pass < 4
    });
    assert!(outcomes.is_empty(), "{outcomes:?}\n{lines}\n{printed}");
    // Three reads of the pull request, the fourth pass reading none, and
    // never the base's.
    assert_eq!(reads(), (follow_reads + 3, base_reads));
    let state = state_of(&bench);
    assert_eq!(state.status(), Status::Active(Stage::Watch));
    assert_eq!(
        (state.round(), state.fix_passes(), state.repairs()),
        (waiting.round(), waiting.fix_passes(), waiting.repairs())
    );
    let delivery = bench.record().delivery.unwrap();
    assert_eq!(
        delivery.ended.unwrap().why,
        owlshift_contracts::refs::EndedWhy::Merged
    );
    assert_eq!(bench.stage(), "In Review");
    // No answer check, no comment of Owlshift's.
    assert_eq!(bench.briefs().len(), briefs);
    assert_eq!(bench.comments().len(), comments);

    let events = &bench.events()[before..];
    let decided = data_of(events, EventKind::Decision, "watch");
    assert_eq!(decided.len(), 1, "{events:?}");
    assert_eq!(
        (
            &decided[0]["watch"],
            &decided[0]["reason"],
            &decided[0]["round"]
        ),
        (&json!("ended"), &json!("merged"), &json!(1))
    );
    let staged = data_of(events, EventKind::TrackerWrite, "stage");
    assert_eq!(staged.len(), 1);
    assert_eq!(staged[0]["stage"], "review");
    let warned = data_of(events, EventKind::Warning, "what");
    assert_eq!(warned.len(), 1, "{warned:?}");
    assert_eq!(warned[0]["what"], "watch_unreadable");
    assert!(
        warned[0]["reason"]
            .as_str()
            .unwrap()
            .starts_with("closing its round:"),
        "{warned:?}"
    );
    assert!(
        lines.contains(
            "it was merged: watch no longer follows it; its round 1 no longer waits for an answer"
        ),
        "{lines}"
    );
}

/// A round whose pull request was closed unmerged closes too, and a stage a
/// person set meanwhile is kept (OWL-242).
#[test]
fn a_round_closed_with_its_pull_request_keeps_a_stage_a_person_set() {
    let bench = delivered();
    asked_at_watch(&bench);
    move_ticket(&bench, "Done");
    bench.github.followed.lock().unwrap().state = "CLOSED";
    let before = bench.events().len();
    let (outcomes, lines, printed) = bench.watch(Vec::new(), &mut |_| false);
    assert!(outcomes.is_empty(), "{outcomes:?}\n{lines}\n{printed}");
    assert_eq!(state_of(&bench).status(), Status::Active(Stage::Watch));
    assert_eq!(
        bench.record().delivery.unwrap().ended.unwrap().why,
        owlshift_contracts::refs::EndedWhy::Closed
    );
    assert_eq!(bench.stage(), "Done");
    let events = &bench.events()[before..];
    assert!(data_of(events, EventKind::TrackerWrite, "stage").is_empty());
}

/// The ticket parked at Watch by a red no run fixes, through `continue`.
fn parked_at_watch(bench: &Bench) -> TicketState {
    bench.github.followed.lock().unwrap().conclusion = "CANCELLED";
    let (outcome, printed) = bench.continue_ticket(Vec::new());
    assert!(
        matches!(
            &outcome,
            Err(Stop::Parked {
                reason: ParkReason::Forge,
                ..
            })
        ),
        "{outcome:?}\n{printed}"
    );
    let parked = state_of(bench);
    assert_eq!(
        parked.status(),
        Status::Parked {
            at: Stage::Watch,
            awaiting_input: false
        }
    );
    parked
}

/// OWL-243's acceptance: a ticket parked at Watch leaves needs input once
/// its pull request is merged. Still open, the pull request changes
/// nothing; merged, it moves the stage to review and ends the park: the
/// ticket back at Watch with its fix passes and repairs, with no run, no
/// comment and no read of the base, and one `ended` event marking the park.
/// `continue` then has nothing to restart.
#[test]
fn a_ticket_parked_at_watch_leaves_needs_input_once_its_pull_request_is_merged() {
    let bench = delivered();
    let parked = parked_at_watch(&bench);
    assert_eq!(bench.stage(), "Needs Input");
    let (briefs, comments) = (bench.briefs().len(), bench.comments().len());
    let before = bench.events().len();
    let github = bench.github.clone();
    let reads = || {
        (
            *github.follow_reads.lock().unwrap(),
            *github.base_reads.lock().unwrap(),
        )
    };
    let (follow_reads, base_reads) = reads();
    let mut pass = 0;
    let (outcomes, lines, printed) = bench.watch(Vec::new(), &mut |_| {
        pass += 1;
        if pass == 1 {
            assert_eq!(state_of(&bench), parked);
            assert_eq!(bench.stage(), "Needs Input");
            assert_eq!(bench.events().len(), before);
            github.followed.lock().unwrap().state = "MERGED";
        }
        pass < 3
    });
    assert!(outcomes.is_empty(), "{outcomes:?}\n{lines}\n{printed}");
    // Read open, then merged; the third pass reads nothing, the park over.
    assert_eq!(reads(), (follow_reads + 2, base_reads));
    assert_eq!(bench.stage(), "In Review");
    let state = state_of(&bench);
    assert_eq!(state.status(), Status::Active(Stage::Watch));
    assert_eq!(
        (
            state.round(),
            state.failed_runs(),
            state.fix_passes(),
            state.repairs()
        ),
        (parked.round(), 0, parked.fix_passes(), parked.repairs())
    );
    assert_eq!(
        bench.record().delivery.unwrap().ended.unwrap().why,
        owlshift_contracts::refs::EndedWhy::Merged
    );
    assert_eq!(
        (bench.briefs().len(), bench.comments().len()),
        (briefs, comments)
    );
    let decided = data_of(&bench.events()[before..], EventKind::Decision, "watch");
    assert_eq!(decided.len(), 1, "{decided:?}");
    assert_eq!(
        (
            &decided[0]["watch"],
            &decided[0]["reason"],
            &decided[0]["parked"]
        ),
        (&json!("ended"), &json!("merged"), &json!(true))
    );
    assert!(decided[0].get("round").is_none(), "{decided:?}");
    assert!(
        lines.contains("it was merged: watch no longer follows it; it is no longer parked"),
        "{lines}"
    );

    let (outcome, printed) = bench.continue_ticket(Vec::new());
    assert!(
        matches!(&outcome, Err(Stop::Refused(why)) if why.starts_with("nothing to continue")),
        "{outcome:?}\n{printed}"
    );
}

/// On a project that maps a parked state, a park whose pull request was
/// closed unmerged leaves that state for review (OWL-243).
#[test]
fn a_park_whose_pull_request_is_closed_leaves_the_parked_state() {
    let bench = deliver(Bench::parking());
    parked_at_watch(&bench);
    assert_eq!(bench.stage(), "Parked");
    bench.github.followed.lock().unwrap().state = "CLOSED";
    let (outcomes, lines, printed) = bench.watch(Vec::new(), &mut |_| false);
    assert!(outcomes.is_empty(), "{outcomes:?}\n{lines}\n{printed}");
    assert_eq!(bench.stage(), "In Review");
    assert_eq!(state_of(&bench).status(), Status::Active(Stage::Watch));
}

/// A ticket left in a stage Owlshift does not work in records one
/// `stage_moved` event across the passes that find it there, and another
/// each time it is moved again (OWL-219, architecture section 5, "A ticket a
/// person moved"). The return to a working stage is the unit test's in
/// `follow`.
#[test]
fn a_moved_stage_is_recorded_once_across_watch_passes() {
    let bench = delivered();
    bench.github.followed.lock().unwrap().conclusion = "FAILURE";
    move_ticket(&bench, "Done");
    let before = bench.events().len();
    let mut pass = 0;
    let (outcomes, lines, printed) = bench.watch(Vec::new(), &mut |_| {
        pass += 1;
        // Each pass is past the hold a refused repair sets (ten minutes from
        // the bench's first ten), so each one attempts the repair again.
        bench.waited.set(SignedDuration::from_mins(10 + 11 * pass));
        match pass {
            3 => move_ticket(&bench, "Backlog"),
            5 => move_ticket(&bench, "Done"),
            _ => {}
        }
        pass < 7
    });
    assert_eq!(outcomes.len(), 7, "{outcomes:?}\n{lines}\n{printed}");
    assert!(
        outcomes
            .iter()
            .all(|o| matches!(o, Err(Stop::Refused(why)) if why.contains("not a state"))),
        "{outcomes:?}"
    );
    let moved: Vec<Value> = data_of(&bench.events()[before..], EventKind::Decision, "watch")
        .iter()
        .filter(|d| d["watch"] == "stage_moved")
        .map(|d| d["stage"].clone())
        .collect();
    assert_eq!(
        moved,
        [json!("Done"), json!("Backlog"), json!("Done")],
        "{lines}\n{printed}"
    );
}

/// A red no run fixes parks the ticket without a run; restarted, a red of
/// the code gets its fix run, and a push the forge refuses while the branch
/// is still at Owlshift's last push parks it again, git's answer quoted.
#[test]
fn a_red_no_run_fixes_and_a_refused_push_park_the_ticket() {
    let bench = delivered();
    bench.github.followed.lock().unwrap().conclusion = "CANCELLED";
    let (outcome, printed) = bench.continue_ticket(Vec::new());
    assert!(
        matches!(&outcome, Err(Stop::Parked { reason: ParkReason::Forge, detail, .. })
            if detail.contains("ci (CANCELLED)")),
        "{outcome:?}\n{printed}"
    );
    // The delivery's Build and Verify pass alone (OWL-304).
    assert_eq!(bench.briefs().len(), 2);

    // The remote cannot lock the branch's ref: it refuses the push.
    bench.github.followed.lock().unwrap().conclusion = "FAILURE";
    let lock = bench.remote.bare.join("refs/heads/owlshift/demo-1.lock");
    let pushed = bench.remote_branch().unwrap();
    let during = (
        1,
        Box::new(move || fs::write(lock, "").unwrap()) as Box<dyn FnOnce()>,
    );
    let (outcome, printed) = bench.continue_during(
        vec![bench.reply(Some("Hello again"), Some(FIXED))],
        vec![during],
    );
    assert!(
        matches!(&outcome, Err(Stop::Parked { reason: ParkReason::Forge, detail, .. })
            if detail.contains("the forge refused Owlshift's push")),
        "{outcome:?}\n{printed}"
    );
    assert_eq!(bench.remote_branch().unwrap(), pushed);
    let delivery = bench.record().delivery.unwrap();
    assert_eq!(
        (delivery.pushed.as_deref(), delivery.pushing),
        (Some(pushed.as_str()), None)
    );
    assert_eq!(state_of(&bench).fix_passes(), 0);
}

/// Someone pushing to the branch while the fix run works: the leased push
/// is refused, nothing is overwritten, and the next read ends the
/// following.
#[test]
fn a_persons_push_during_a_fix_run_ends_the_following() {
    let bench = delivered();
    bench.github.followed.lock().unwrap().conclusion = "FAILURE";
    let (env, bare) = (bench.env.clone(), bench.remote.bare.clone());
    let main = bench.remote_main();
    let during = (
        1,
        Box::new(move || {
            env.run(
                &bare,
                &["update-ref", &format!("refs/heads/{BRANCH}"), &main],
            )
            .unwrap();
        }) as Box<dyn FnOnce()>,
    );
    let (outcome, printed) = bench.continue_during(
        vec![bench.reply(Some("Hello again"), Some(FIXED))],
        vec![during],
    );
    assert!(
        matches!(&outcome, Err(Stop::Refused(why)) if why.contains("someone else pushed")),
        "{outcome:?}\n{printed}"
    );
    assert_eq!(bench.remote_branch().unwrap(), bench.remote_main());
    assert!(bench.record().delivery.unwrap().pushing.is_none());

    let (outcome, printed) = bench.continue_ticket(Vec::new());
    assert!(
        matches!(&outcome, Err(Stop::Watched { .. })),
        "{outcome:?}\n{printed}"
    );
    let ended = bench.record().delivery.unwrap().ended.unwrap();
    assert_eq!(ended.why, owlshift_contracts::refs::EndedWhy::HeadMoved);
}

/// The DELIVERY reports on the ticket.
fn reports(bench: &Bench) -> Vec<String> {
    bench
        .comments()
        .into_iter()
        .filter(|c| c.starts_with("[owlshift] DELIVERY"))
        .collect()
}

/// The fix pass line of a report naming the red check `ci`.
const FIX_PASS_1: &str = "**Fix pass 1 of 2 after delivery,** for the red checks `ci`.";

/// OWL-218's acceptance: the tracker refuses the DELIVERY report of a fix
/// run whose push landed; the ref keeps it owed, and watch's next pass
/// posts it, once. A crash after that post, before the ref kept it as
/// posted, is told by the report on the ticket: the pass after it posts
/// nothing.
#[test]
fn a_fix_report_the_tracker_refused_is_posted_at_the_next_pass_once() {
    let bench = delivered();
    let delivered_events = bench.events().len();
    let github = bench.github.clone();
    github.followed.lock().unwrap().conclusion = "FAILURE";
    bench.refuse.set(Some("[owlshift] DELIVERY"));
    let mut owing = None;
    let mut pass = 0;
    let (outcomes, lines, printed) = bench.watch(
        vec![bench.reply(Some("Hello again"), Some(FIXED))],
        &mut |_| {
            pass += 1;
            match pass {
                1 => {
                    let record = bench.record();
                    assert!(record.delivery.as_ref().unwrap().owed.is_some());
                    owing = Some(record);
                    bench.refuse.set(None);
                    github.followed.lock().unwrap().conclusion = "SUCCESS";
                }
                // The post landed, and the crash lost the ref's write.
                2 => {
                    assert!(bench.record().delivery.unwrap().owed.is_none());
                    let mut lost = bench.record();
                    lost.delivery = owing.take().unwrap().delivery;
                    bench.write_record(&lost);
                }
                _ => {}
            }
            pass < 3
        },
    );
    assert!(
        matches!(&outcomes[..], [Err(Stop::Delivery(why))] if why.contains("the next pass posts it")),
        "{outcomes:?}\n{lines}\n{printed}"
    );
    let reports = reports(&bench);
    assert_eq!(reports.len(), 2, "{reports:?}");
    assert!(reports[1].contains(FIX_PASS_1), "{}", reports[1]);
    assert!(
        reports[1].contains("Kept the greeting on every platform."),
        "{}",
        reports[1]
    );
    let delivery = bench.record().delivery.unwrap();
    assert!(delivery.owed.is_none() && delivery.followed());
    let events = &bench.events()[delivered_events..];
    let warned = data_of(events, EventKind::Warning, "what");
    let unposted: Vec<&Value> = warned
        .iter()
        .filter(|w| w["what"] == "fix_report_unposted")
        .collect();
    assert_eq!(unposted.len(), 1, "{warned:?}");
}

/// OWL-232's acceptance: the tracker keeps refusing the DELIVERY report
/// across several watch passes, and the log holds one warning, not one per
/// pass; the report is posted once the tracker accepts it.
#[test]
fn an_owed_fix_report_the_tracker_keeps_refusing_is_warned_of_once() {
    let bench = delivered();
    let delivered_events = bench.events().len();
    let github = bench.github.clone();
    github.followed.lock().unwrap().conclusion = "FAILURE";
    bench.refuse.set(Some("[owlshift] DELIVERY"));
    let mut pass = 0;
    let (_, lines, printed) = bench.watch(
        vec![bench.reply(Some("Hello again"), Some(FIXED))],
        &mut |_| {
            pass += 1;
            match pass {
                1 => github.followed.lock().unwrap().conclusion = "SUCCESS",
                4 => bench.refuse.set(None),
                _ => {}
            }
            pass < 6
        },
    );
    let reports = reports(&bench);
    assert_eq!(reports.len(), 2, "{reports:?}\n{lines}\n{printed}");
    assert!(reports[1].contains(FIX_PASS_1), "{}", reports[1]);
    assert!(bench.record().delivery.unwrap().owed.is_none());
    let events = &bench.events()[delivered_events..];
    let warned = data_of(events, EventKind::Warning, "what");
    let unposted: Vec<&Value> = warned
        .iter()
        .filter(|w| w["what"] == "fix_report_unposted")
        .collect();
    assert_eq!(unposted.len(), 1, "{warned:?}");
}

/// OWL-218: `owlshift continue` at Watch posts the report the ref owes,
/// once; one still owed when the pull request was merged is dropped with a
/// warning, not posted.
#[test]
fn continue_posts_an_owed_fix_report_and_an_end_drops_it() {
    let bench = delivered();
    bench.github.followed.lock().unwrap().conclusion = "FAILURE";
    bench.refuse.set(Some("[owlshift] DELIVERY"));
    let (refused, printed) =
        bench.continue_ticket(vec![bench.reply(Some("Hello again"), Some(FIXED))]);
    assert!(
        matches!(&refused, Err(Stop::Delivery(_))),
        "{refused:?}\n{printed}"
    );
    let owing = bench.record();
    assert!(owing.delivery.as_ref().unwrap().owed.is_some());
    assert_eq!(reports(&bench).len(), 1);

    bench.refuse.set(None);
    bench.github.followed.lock().unwrap().conclusion = "SUCCESS";
    let (outcome, printed) = bench.continue_ticket(Vec::new());
    assert!(
        matches!(&outcome, Err(Stop::Watched { .. })),
        "{outcome:?}\n{printed}"
    );
    assert!(bench.record().delivery.unwrap().owed.is_none());
    let reports_now = reports(&bench);
    assert_eq!(reports_now.len(), 2);
    assert!(reports_now[1].contains(FIX_PASS_1), "{}", reports_now[1]);

    // Owed again, as a refusal would leave it, then merged by a person.
    let mut again = bench.record();
    again.delivery = owing.delivery;
    bench.write_record(&again);
    bench.comment_as("someone", "Merged it.");
    let before = bench.comments().len();
    bench.github.followed.lock().unwrap().state = "MERGED";
    let (outcome, printed) = bench.continue_ticket(Vec::new());
    assert!(
        matches!(&outcome, Err(Stop::Watched { .. })),
        "{outcome:?}\n{printed}"
    );
    assert_eq!(bench.comments().len(), before);
    let delivery = bench.record().delivery.unwrap();
    assert!(delivery.owed.is_none() && delivery.ended.is_some());
    let warned = data_of(&bench.events(), EventKind::Warning, "what");
    assert!(
        warned.iter().any(|w| w["what"] == "fix_report_dropped"),
        "{warned:?}"
    );
}

/// A fix push a crash cut short: the branch holds the fix, the ref still
/// keeps it as under way, with what its run kept for its report, and `owed`
/// as the report left owed by an earlier push. Returns the fix's commit.
fn a_crashed_fix_push(bench: &Bench, owed: Option<owlshift_contracts::refs::OwedReport>) -> String {
    let (env, bare) = (bench.env.clone(), bench.remote.bare.clone());
    let branch = format!("refs/heads/{BRANCH}");
    let tip = bench.remote_branch().unwrap();
    let fix = env
        .run(
            &bare,
            &[
                "commit-tree",
                &format!("{tip}^{{tree}}"),
                "-p",
                &tip,
                "-m",
                "Fix",
            ],
        )
        .map(|out| String::from_utf8(out).unwrap().trim().to_owned())
        .unwrap();
    env.run(&bare, &["update-ref", &branch, &fix]).unwrap();
    let mut record = bench.record();
    let delivery = record.delivery.as_mut().unwrap();
    // A rebase's report is owed only with the base it rebased onto.
    if owed
        .as_ref()
        .is_some_and(|owed| owed.kind == owlshift_contracts::refs::PushKind::Rebase)
    {
        delivery.rebased_onto = Some(bench.remote_main());
    }
    delivery.owed = owed;
    delivery.pushing = Some(owlshift_contracts::refs::Pushing {
        commit: fix.clone(),
        kind: owlshift_contracts::refs::PushKind::Fix,
        base: bench.remote_main(),
        onto: None,
        report: Some(owlshift_contracts::refs::KeptReport {
            summary: "Kept the greeting on every platform.".to_owned(),
            gate: Some(vec!["git grep -q Hello".to_owned()]),
            checks: vec!["ci".to_owned()],
            questions: Vec::new(),
            notes: Vec::new(),
            followups: Vec::new(),
            reviews: Vec::new(),
        }),
    });
    bench.write_record(&record);
    fix
}

/// OWL-218: a fix push a crash cut short, adopted by watch's read, gets
/// its DELIVERY report at that pass, with what the run kept for it.
#[test]
fn an_adopted_fix_push_is_reported_with_what_its_run_kept() {
    let bench = delivered();
    let fix = a_crashed_fix_push(&bench, None);
    // The crash left the ticket at working; the adoption ends the run
    // (OWL-259).
    move_ticket(&bench, "In Progress");
    let (outcomes, lines, printed) = bench.watch(Vec::new(), &mut |_| false);
    assert!(outcomes.is_empty(), "{outcomes:?}\n{lines}\n{printed}");
    assert_eq!(bench.stage(), "In Review");
    let reports = reports(&bench);
    assert_eq!(reports.len(), 2, "{reports:?}");
    for text in [
        FIX_PASS_1,
        "Kept the greeting on every platform.",
        "`git grep -q Hello`",
    ] {
        assert!(reports[1].contains(text), "{text}: {}", reports[1]);
    }
    let delivery = bench.record().delivery.unwrap();
    assert_eq!(delivery.pushed.as_deref(), Some(fix.as_str()));
    assert!(delivery.owed.is_none() && delivery.pushing.is_none());
}

/// OWL-259: `continue` at Watch adopting a fix push a crash cut short
/// moves the ticket the crash left at working back to review.
#[test]
fn continue_adopting_a_crashed_push_shows_review_again() {
    let bench = delivered();
    a_crashed_fix_push(&bench, None);
    move_ticket(&bench, "In Progress");
    let (look, printed) = bench.continue_ticket(Vec::new());
    assert!(
        matches!(look, Err(Stop::Watched { .. })),
        "{look:?}\n{printed}"
    );
    assert_eq!(bench.stage(), "In Review");
}

/// The `*_report_replaced` warnings of the events after `from`.
fn replaced_warnings(bench: &Bench, from: usize) -> Vec<Value> {
    data_of(&bench.events()[from..], EventKind::Warning, "what")
        .into_iter()
        .filter(|w| {
            w["what"]
                .as_str()
                .is_some_and(|w| w.ends_with("_report_replaced"))
        })
        .collect()
}

/// OWL-239: a read that adopts a fix push replaces the report the ref owed
/// of an earlier push of `kind`, and tells it, once, with the replaced
/// report's commit and count.
fn adopting_a_push_replaces_an_owed_report(
    kind: owlshift_contracts::refs::PushKind,
    what: &str,
    count: u32,
) {
    let bench = delivered();
    let earlier = bench.record().delivery.unwrap().pushed.unwrap();
    let owed = owlshift_contracts::refs::OwedReport {
        kind,
        count,
        report: None,
        unchanged: false,
    };
    a_crashed_fix_push(&bench, Some(owed));
    let from = bench.events().len();
    let (outcomes, lines, printed) = bench.watch(Vec::new(), &mut |_| false);
    assert!(outcomes.is_empty(), "{outcomes:?}\n{lines}\n{printed}");
    let warned = replaced_warnings(&bench, from);
    assert_eq!(warned.len(), 1, "{warned:?}\n{lines}\n{printed}");
    assert_eq!(warned[0]["what"], format!("{what}_report_replaced"));
    assert_eq!(warned[0]["commit"], earlier.as_str());
    assert_eq!(warned[0]["count"], count);
}

#[test]
fn adopting_a_push_replaces_an_owed_delivery_report_with_a_warning() {
    adopting_a_push_replaces_an_owed_report(
        owlshift_contracts::refs::PushKind::Delivery,
        "delivery",
        1,
    );
}

#[test]
fn adopting_a_push_replaces_an_owed_fix_report_with_a_warning() {
    adopting_a_push_replaces_an_owed_report(owlshift_contracts::refs::PushKind::Fix, "fix", 2);
}

#[test]
fn adopting_a_push_replaces_an_owed_rebase_report_with_a_warning() {
    adopting_a_push_replaces_an_owed_report(
        owlshift_contracts::refs::PushKind::Rebase,
        "rebase",
        2,
    );
}

#[test]
fn adopting_a_push_replaces_an_owed_correction_report_with_a_warning() {
    adopting_a_push_replaces_an_owed_report(
        owlshift_contracts::refs::PushKind::Correction,
        "correction",
        2,
    );
}

/// OWL-239: `owlshift continue` at Watch tells the report its read's
/// adoption replaces, as watch does; and an adoption that replaces nothing
/// tells nothing.
#[test]
fn continue_tells_the_owed_report_an_adopted_push_replaces() {
    let bench = delivered();
    a_crashed_fix_push(&bench, None);
    let from = bench.events().len();
    let (_, printed) = bench.continue_ticket(Vec::new());
    assert!(replaced_warnings(&bench, from).is_empty(), "{printed}");

    let bench = delivered();
    let earlier = bench.record().delivery.unwrap().pushed.unwrap();
    let owed = owlshift_contracts::refs::OwedReport {
        kind: owlshift_contracts::refs::PushKind::Fix,
        count: 1,
        report: None,
        unchanged: false,
    };
    a_crashed_fix_push(&bench, Some(owed));
    let from = bench.events().len();
    let (_, printed) = bench.continue_ticket(Vec::new());
    let warned = replaced_warnings(&bench, from);
    assert_eq!(warned.len(), 1, "{warned:?}\n{printed}");
    assert_eq!(warned[0]["what"], "fix_report_replaced");
    assert_eq!(warned[0]["commit"], earlier.as_str());
}

/// A delivered ticket whose fix run's DELIVERY report the tracker refused:
/// the ref owes it, the tracker still refuses DELIVERY reports, and the
/// pull request reads green at the fix.
fn owing_a_fix_report() -> Bench {
    let bench = delivered();
    bench.github.followed.lock().unwrap().conclusion = "FAILURE";
    bench.refuse.set(Some("[owlshift] DELIVERY"));
    let (refused, printed) =
        bench.continue_ticket(vec![bench.reply(Some("Hello again"), Some(FIXED))]);
    assert!(
        matches!(&refused, Err(Stop::Delivery(_))),
        "{refused:?}\n{printed}"
    );
    assert!(bench.record().delivery.unwrap().owed.is_some());
    assert_eq!(reports(&bench).len(), 1);
    bench.github.followed.lock().unwrap().conclusion = "SUCCESS";
    bench
}

/// OWL-238's acceptance: the head moves between watch's read of the pull
/// request and its read of the checks for the owed report. The report is
/// not posted and stays owed; a head back at the push leaves the refusal
/// warned of once; the read that ends the following drops it, unposted.
#[test]
fn an_owed_report_waits_while_its_head_moved_until_the_following_ends() {
    let bench = owing_a_fix_report();
    let watched = bench.events().len();
    let github = bench.github.clone();
    let (env, bare) = (bench.env.clone(), bench.remote.bare.clone());
    let branch = format!("refs/heads/{BRANCH}");
    let pushed = bench.remote_branch().unwrap();
    let main = bench.remote_main();
    let briefs = bench.briefs().len();
    // The pass ends at the hold: the read's red starts no repair, whose
    // stop would be an outcome, and no fix run.
    let held = |bench: &Bench| {
        assert!(github.at_checks.lock().unwrap().is_none());
        assert_eq!(bench.briefs().len(), briefs);
        assert_eq!(bench.remote_branch().unwrap(), main);
        let delivery = bench.record().delivery.unwrap();
        assert!(delivery.owed.is_some() && delivery.ended.is_none());
        assert_eq!(reports(bench).len(), 1);
    };
    let mut pass = 0;
    let (outcomes, lines, printed) = bench.watch(Vec::new(), &mut |_| {
        pass += 1;
        match pass {
            // Refused, warned of at pass 1 only; the tracker now takes the
            // report, the pull request reads red, and someone pushes before
            // the report's checks are read.
            1 | 3 => {
                bench.refuse.set(None);
                github.followed.lock().unwrap().conclusion = "FAILURE";
                *github.at_checks.lock().unwrap() = Some(AtChecks::Move(main.clone()));
            }
            // Held at the moved head; then the head is back at the push,
            // green, and the tracker refuses again.
            2 => {
                held(&bench);
                env.run(&bare, &["update-ref", &branch, &pushed]).unwrap();
                github.followed.lock().unwrap().conclusion = "SUCCESS";
                bench.refuse.set(Some("[owlshift] DELIVERY"));
            }
            4 => held(&bench),
            _ => {}
        }
        pass < 5
    });
    assert!(outcomes.is_empty(), "{outcomes:?}\n{lines}\n{printed}");
    assert_eq!(reports(&bench).len(), 1);
    let delivery = bench.record().delivery.unwrap();
    assert!(delivery.owed.is_none());
    assert_eq!(
        delivery.ended.unwrap().why,
        owlshift_contracts::refs::EndedWhy::HeadMoved
    );
    let warned = data_of(&bench.events()[watched..], EventKind::Warning, "what");
    let named = |what: &str| warned.iter().filter(|w| w["what"] == what).count();
    assert_eq!(
        (named("fix_report_unposted"), named("fix_report_dropped")),
        (1, 1),
        "{warned:?}"
    );
}

/// OWL-238: the head moves between a fix push and the read of its checks
/// for the report: nothing is posted, and the ref keeps the push counted
/// and its report owed, for the next read of the pull request to settle.
#[test]
fn a_fix_report_waits_while_its_head_moved_after_the_push() {
    let bench = delivered();
    bench.github.followed.lock().unwrap().conclusion = "FAILURE";
    *bench.github.at_checks.lock().unwrap() = Some(AtChecks::Move(bench.remote_main()));
    let (outcome, printed) =
        bench.continue_ticket(vec![bench.reply(Some("Hello again"), Some(FIXED))]);
    assert!(
        matches!(&outcome, Err(Stop::Delivery(why))
            if why.contains("its head moved before its checks were read")),
        "{outcome:?}\n{printed}"
    );
    assert!(bench.github.at_checks.lock().unwrap().is_none());
    assert_eq!(reports(&bench).len(), 1);
    let delivery = bench.record().delivery.unwrap();
    let owed = delivery.owed.unwrap();
    assert_eq!(owed.kind, owlshift_contracts::refs::PushKind::Fix);
    assert!(delivery.pushing.is_none());
    assert_eq!(state_of(&bench).fix_passes(), 1);
}

/// OWL-238: a read of the checks that fails for another reason than a
/// moved head still posts the owed report, which says so.
#[test]
fn an_owed_report_whose_checks_cannot_be_read_is_posted() {
    let bench = owing_a_fix_report();
    bench.refuse.set(None);
    *bench.github.at_checks.lock().unwrap() = Some(AtChecks::Fail);
    let (outcomes, lines, printed) = bench.watch(Vec::new(), &mut |_| false);
    assert!(outcomes.is_empty(), "{outcomes:?}\n{lines}\n{printed}");
    let reports = reports(&bench);
    assert_eq!(reports.len(), 2, "{reports:?}");
    assert!(reports[1].contains(FIX_PASS_1), "{}", reports[1]);
    assert!(
        reports[1].contains("**Checks:** could not be read"),
        "{}",
        reports[1]
    );
    assert!(bench.record().delivery.unwrap().owed.is_none());
}

/// A Build `done` proposing a follow-up.
const DONE_WITH_FOLLOWUP: &str = r#"{"format":13,"status":"done","summary":"Added GREETING.md; the gate passes.",
"pr":{"branch":"owlshift/demo-1","title":"Add a greeting","body":"Says hello. Gate: green."},
"followups":[{"title":"Greet in French","why":"Readers asked.","source":"agent","done_when":"It says Bonjour."}]}"#;

/// OWL-236's acceptance: the tracker refuses the first DELIVERY report
/// after the ref keeps the ticket at Watch; the ref owes it, and watch's
/// next pass posts it, once, with what the delivering run said. A crash
/// after that post, before the ref kept it as posted, is told by the
/// report on the ticket: `continue` then posts nothing more.
#[test]
fn a_first_report_the_tracker_refused_is_posted_at_the_next_pass_once() {
    let bench = Bench::new(true);
    bench.refuse.set(Some("[owlshift] DELIVERY"));
    let (outcome, printed) = bench.run(
        vec![bench.reply(Some("Hello"), Some(DONE_WITH_FOLLOWUP))],
        None,
    );
    assert!(
        matches!(&outcome, Err(Stop::Delivery(why)) if why.contains("the next pass posts it")),
        "{outcome:?}\n{printed}"
    );
    assert!(reports(&bench).is_empty());
    let owing = bench.record();
    assert_eq!(owing.state.stage, owlshift_contracts::Stage::Watch);
    let owed = owing.delivery.as_ref().unwrap().owed.as_ref().unwrap();
    assert_eq!(owed.kind, owlshift_contracts::refs::PushKind::Delivery);

    bench.refuse.set(None);
    let (outcomes, lines, printed) = bench.watch(Vec::new(), &mut |_| false);
    assert!(outcomes.is_empty(), "{outcomes:?}\n{lines}\n{printed}");
    let posted = reports(&bench);
    assert_eq!(posted.len(), 1, "{posted:?}");
    for text in [
        "Added GREETING.md; the gate passes.",
        "**Project gate:** passed: `git grep -q Hello -- GREETING.md`.",
        "**Greet in French**",
    ] {
        assert!(posted[0].contains(text), "{text}: {}", posted[0]);
    }
    assert!(!posted[0].contains("**Fix pass "), "{}", posted[0]);
    assert!(bench.record().delivery.unwrap().owed.is_none());

    // The post landed, and a crash lost the ref's write.
    let mut lost = bench.record();
    lost.delivery = owing.delivery;
    bench.write_record(&lost);
    let (outcome, printed) = bench.continue_ticket(Vec::new());
    assert!(
        matches!(&outcome, Err(Stop::Watched { .. })),
        "{outcome:?}\n{printed}"
    );
    assert_eq!(reports(&bench), posted);
    let delivery = bench.record().delivery.unwrap();
    assert!(delivery.owed.is_none() && delivery.followed());
    let warned = data_of(&bench.events(), EventKind::Warning, "what");
    let unposted: Vec<&Value> = warned
        .iter()
        .filter(|w| w["what"] == "delivery_report_unposted")
        .collect();
    assert_eq!(unposted.len(), 1, "{warned:?}");
}

/// Another merge into `bare`'s `main`, writing `files`, an empty text
/// removing its file, from a new clone in `root`; returns `main`'s new head.
fn merge_into(env: &GitEnv, root: &Path, bare: &Path, files: &[(&str, &str)]) -> String {
    let n = fs::read_dir(root).unwrap().count();
    let clone = root.join(format!("merger-{n}"));
    env.run(
        root,
        &[
            "clone",
            "--quiet",
            bare.to_str().unwrap(),
            clone.to_str().unwrap(),
        ],
    )
    .unwrap();
    for (name, text) in files {
        if text.is_empty() {
            fs::remove_file(clone.join(name)).unwrap();
        } else {
            fs::write(clone.join(name), text).unwrap();
        }
    }
    env.run(&clone, &["add", "--all"]).unwrap();
    env.run(&clone, &["commit", "--quiet", "-m", "Another merge"])
        .unwrap();
    env.run(
        &clone,
        &["push", "--quiet", "origin", "HEAD:refs/heads/main"],
    )
    .unwrap();
    let head = env.run(bare, &["rev-parse", "refs/heads/main"]).unwrap();
    String::from_utf8(head).unwrap().trim().to_owned()
}

/// The project's rules as a delivery changes them, and as another merge
/// changes the same line meanwhile: a contents conflict on one path.
const AGENTS_DELIVERED: &str = "Sign off every commit (`git commit -s`), always.\n";
const AGENTS_MERGED: &str = "Sign off every commit (`git commit -s`) and keep it short.\n";
const AGENTS_RESOLVED: &str =
    "Sign off every commit (`git commit -s`), always, and keep it short.\n";

/// A rebase run's `done`.
const REBASED: &str = r#"{"format":13,"status":"done","summary":"Rebased onto the base; the conflict kept both sides."}"#;

/// A rebase run that could not resolve its conflict.
const NOT_REBASED: &str = r#"{"format":13,"status":"failed","summary":"Could not resolve AGENTS.md; the rebase was aborted."}"#;

/// A delivered ticket whose delivery also changed the project's rules, its
/// pull request followed.
fn delivered_with_rules() -> Bench {
    let bench = Bench::new(true);
    let mut reply = bench.reply(Some("Hello"), Some(DONE));
    reply.files.insert(
        RelativePath::new("AGENTS.md").unwrap(),
        AGENTS_DELIVERED.to_owned(),
    );
    let (outcome, printed) = bench.run(vec![reply], None);
    outcome.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    bench
}

/// What the agent of a rebase run does in its worktree, played by the
/// bench: `git rebase` onto `onto`, each conflicted file written as
/// `resolved`, then the rebase continued.
fn rebase_in(bench: &Bench, onto: String, resolved: Vec<(&'static str, &'static str)>) -> During {
    let (env, worktree) = (bench.env.clone(), bench.worktree());
    let act = move || {
        // The conflict stops the rebase: git exits 1.
        let _ = env.run(&worktree, &["rebase", "--quiet", &onto]);
        for (name, text) in &resolved {
            fs::write(worktree.join(name), text).unwrap();
            env.run(&worktree, &["add", "--", name]).unwrap();
        }
        env.run(
            &worktree,
            &["-c", "core.editor=true", "rebase", "--continue"],
        )
        .unwrap();
    };
    (1, Box::new(act))
}

fn is_ancestor(bench: &Bench, ancestor: &str, of: &str) -> bool {
    bench
        .env
        .run(
            &bench.remote.bare,
            &["merge-base", "--is-ancestor", ancestor, of],
        )
        .is_ok()
}

/// Scenario S9's conflict (OWL-210): another merge changed the line the
/// delivery changed, a trivial conflict. The rebase run is on the fast
/// tier, briefed with the base, the branch and the conflicting path; the
/// runner, never the agent, force-pushes the rebase leased on its last push,
/// keeps the base it was rebased onto and reports it. Read conflicting
/// again against that same base, the rebase counts as failed with no run,
/// and the second such read parks the ticket.
#[test]
fn a_trivial_conflict_is_rebased_on_the_fast_tier_and_pushed_by_the_runner() {
    let bench = delivered_with_rules();
    let delivered_events = bench.events().len();
    let first_push = bench.remote_branch().unwrap();
    // The merge also adds a gate command only the rebased code passes: the
    // rebase run takes the gate of the base it rebases onto (OWL-331). It
    // adds a setup too, which the runner's gate runs, never the rebase's
    // harness (OWL-330): it counts its runs in the run's own files.
    let config = fs::read_to_string(bench.remote.checkout.join("owlshift.toml")).unwrap();
    let onto_gate = [
        "git grep -q Hello -- GREETING.md",
        "git grep -q short -- AGENTS.md",
    ];
    let gated = config.replace(
        GATE_LINE,
        &format!(
            "gate = [\"{}\", \"{}\"]\nsetup = [\"echo ran >> .owlshift/run/setup-runs\"]",
            onto_gate[0], onto_gate[1]
        ),
    );
    let setup_runs = bench.worktree().join(".owlshift/run/setup-runs");
    let main = bench.merge_into_main(&[("AGENTS.md", AGENTS_MERGED), ("owlshift.toml", &gated)]);
    bench.github.followed.lock().unwrap().mergeable = "CONFLICTING";
    let (_, rebase) = rebase_in(&bench, main.clone(), vec![("AGENTS.md", AGENTS_RESOLVED)]);
    let before_harness = setup_runs.clone();
    let rebase_unset: During = (
        1,
        Box::new(move || {
            assert!(!before_harness.exists(), "the setup ran before the rebase");
            rebase();
        }),
    );
    let (outcome, printed) =
        bench.continue_during(vec![bench.reply(None, Some(REBASED))], vec![rebase_unset]);
    let delivered = outcome.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    assert!(!delivered.opened);
    // The gate after the rebase ran the new base's setup, once.
    assert_eq!(fs::read_to_string(&setup_runs).unwrap(), "ran\n");
    // Working while the rebase run worked, review once pushed (OWL-259).
    assert_eq!(stage_moves(&bench, delivered_events), ["working", "review"]);
    assert_eq!(bench.stage(), "In Review");

    // One run, of the rebase role on the fast tier, told what to rebase.
    let events = &bench.events()[delivered_events..];
    let runs = data_of(events, EventKind::Decision, "trivial");
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert_eq!(
        (
            &runs[0]["tier"],
            &runs[0]["trivial"],
            &runs[0]["conflicting"]
        ),
        (&json!("fast"), &json!(true), &json!(["AGENTS.md"]))
    );
    let briefs = bench.briefs();
    let brief = briefs.last().unwrap();
    assert_eq!(brief.role, Role::Rebase);
    assert_eq!(brief.gate, onto_gate);
    let configured = data_of(events, EventKind::Dispatch, "configured_at");
    assert_eq!(configured[0]["configured_at"], json!(main));
    let told = brief.rebase.as_ref().expect("what to rebase");
    assert_eq!(
        (
            told.head.as_str(),
            told.onto.as_str(),
            told.base_branch.as_str()
        ),
        (first_push.as_str(), main.as_str(), "main")
    );
    assert_eq!(
        (told.branch.as_str(), told.conflicting.as_slice()),
        (BRANCH, &["AGENTS.md".to_owned()][..])
    );
    assert!(!told.refresh);

    // The runner force-pushed the rebase, leased on its last push.
    let rebased = bench.remote_branch().unwrap();
    assert_ne!(rebased, first_push);
    assert!(is_ancestor(&bench, &main, &rebased));
    let pushes = data_of(events, EventKind::TrackerWrite, "lease");
    assert_eq!(pushes.len(), 1);
    assert_eq!(
        (
            &pushes[0]["pushed"],
            &pushes[0]["lease"],
            &pushes[0]["commit"]
        ),
        (&json!("forced"), &json!(first_push), &json!(rebased))
    );
    let delivery = bench.record().delivery.unwrap();
    assert_eq!(
        (delivery.pushed.as_deref(), delivery.rebased_onto.as_deref()),
        (Some(rebased.as_str()), Some(main.as_str()))
    );
    assert!(delivery.pushing.is_none() && delivery.followed());
    let state = state_of(&bench);
    assert_eq!((state.repairs(), state.failed_runs()), (1, 0));
    let report = bench.comments().pop().unwrap();
    assert!(report.starts_with("[owlshift] DELIVERY"), "{report}");
    assert!(
        report.contains("**Rebased onto `main` at ") && report.contains("repair 1 of 6"),
        "{report}"
    );

    // Still conflicting against the base it was rebased onto: a failed run
    // of that rebase, no second one; then a park.
    let (outcome, printed) = bench.continue_ticket(Vec::new());
    assert!(
        matches!(&outcome, Err(Stop::Refused(why)) if why.contains("still conflicts with")),
        "{outcome:?}\n{printed}"
    );
    assert_eq!(state_of(&bench).failed_runs(), 1);
    let (outcome, printed) = bench.continue_ticket(Vec::new());
    assert!(
        matches!(
            &outcome,
            Err(Stop::Parked {
                reason: ParkReason::FailedRuns,
                ..
            })
        ),
        "{outcome:?}\n{printed}"
    );
    assert_eq!(bench.briefs().len(), briefs.len());
    assert_eq!(bench.remote_branch().unwrap(), rebased);
}

/// A conflict that is not trivial, the delivery changing a file another
/// merge removed, goes to the Build routing at once (OWL-210). The base moving again during the
/// run changes nothing of what the rebase was onto: the ref keeps the base
/// the run used, and the base head read before the push beside it.
#[test]
fn a_non_trivial_conflict_goes_to_the_build_routing_at_once() {
    let bench = delivered_with_rules();
    let delivered_events = bench.events().len();
    let main = bench.merge_into_main(&[("AGENTS.md", "")]);
    bench.github.followed.lock().unwrap().mergeable = "CONFLICTING";
    let rebase = rebase_in(&bench, main.clone(), vec![("AGENTS.md", AGENTS_DELIVERED)]);
    let moved = Rc::new(RefCell::new(None));
    let (env, root, bare) = (
        bench.env.clone(),
        bench.tmp.path().to_owned(),
        bench.remote.bare.clone(),
    );
    let kept = moved.clone();
    let during = (
        1,
        Box::new(move || {
            (rebase.1)();
            *kept.borrow_mut() = Some(merge_into(&env, &root, &bare, &[("NOTES.md", "later\n")]));
        }) as Box<dyn FnOnce()>,
    );
    let (outcome, printed) =
        bench.continue_during(vec![bench.reply(None, Some(REBASED))], vec![during]);
    outcome.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));

    let events = &bench.events()[delivered_events..];
    let runs = data_of(events, EventKind::Decision, "trivial");
    assert_eq!(runs.len(), 1, "{runs:?}");
    assert_eq!(
        (&runs[0]["tier"], &runs[0]["trivial"]),
        (&json!("build"), &json!(false))
    );
    let rebased = bench.remote_branch().unwrap();
    assert!(is_ancestor(&bench, &main, &rebased));
    let later = moved.borrow().clone().unwrap();
    assert!(!is_ancestor(&bench, &later, &rebased));
    let delivery = bench.record().delivery.unwrap();
    assert_eq!(
        (
            delivery.rebased_onto.as_deref(),
            delivery.base_at_push.as_deref()
        ),
        (Some(main.as_str()), Some(later.as_str()))
    );
}

/// A trivial conflict whose fast run fails gets one more run, on the Build
/// routing; a second failure parks the ticket, the PARKED comment saying
/// what restarts it (OWL-210). Nothing was pushed, and the ticket's branch
/// is back at Owlshift's last push, so a fix run can start from it.
#[test]
fn a_rebase_that_fails_twice_parks_the_ticket() {
    let bench = delivered_with_rules();
    bench.add_labels(&["tier:deep", "effort:max"]);
    let delivered_events = bench.events().len();
    let first_push = bench.remote_branch().unwrap();
    bench.merge_into_main(&[("AGENTS.md", AGENTS_MERGED)]);
    bench.github.followed.lock().unwrap().mergeable = "CONFLICTING";
    let (outcome, printed) = bench.continue_ticket(vec![
        bench.reply(None, Some(NOT_REBASED)),
        bench.reply(None, Some(NOT_REBASED)),
    ]);
    assert!(
        matches!(
            &outcome,
            Err(Stop::Parked {
                reason: ParkReason::FailedRuns,
                ..
            })
        ),
        "{outcome:?}\n{printed}"
    );
    let events = &bench.events()[delivered_events..];
    let tiers: Vec<Value> = data_of(events, EventKind::Decision, "trivial")
        .iter()
        .map(|run| run["tier"].clone())
        .collect();
    assert_eq!(tiers, [json!("fast"), json!("build")]);
    // The fast tier, then the ticket's, neither with an effort (OWL-305).
    assert_eq!(
        bench.routed()[1..],
        [claude(Tier::Fast, None), claude(Tier::Deep, None)]
    );
    let parked = bench.comments().pop().unwrap();
    assert!(parked.starts_with("[owlshift] PARKED"), "{parked}");
    assert!(parked.contains("owlshift continue DEMO-1"), "{parked}");
    assert_eq!(bench.remote_branch().unwrap(), first_push);
    let local = bench
        .env
        .run(
            &bench.dirs().checkout(),
            &["rev-parse", &format!("refs/heads/{BRANCH}")],
        )
        .unwrap();
    assert_eq!(String::from_utf8(local).unwrap().trim(), first_push);
    assert_eq!(state_of(&bench).repairs(), 0);
}

/// A rebase run that asks goes through the resolver then to the decider as a
/// fix run's question does (OWL-342), the branch back at the last push
/// (OWL-211).
#[test]
fn a_rebase_run_that_asks_goes_to_the_decider() {
    let bench = delivered_with_rules();
    let first_push = bench.remote_branch().unwrap();
    bench.merge_into_main(&[("AGENTS.md", AGENTS_MERGED)]);
    bench.github.followed.lock().unwrap().mergeable = "CONFLICTING";
    let (outcome, printed) = bench.continue_ticket(variable_asked(&bench));
    assert!(
        matches!(&outcome, Err(Stop::NeedsInput { posted: Ok(_), .. })),
        "{outcome:?}\n{printed}"
    );
    let resolver = bench.last_of(Role::Resolver);
    assert_eq!(resolver.asked_by.as_ref().unwrap().role, Role::Rebase);
    assert_eq!(
        state_of(&bench).status(),
        Status::NeedsInput {
            return_to: Stage::Watch
        }
    );
    let asked = bench.comments().pop().unwrap();
    assert!(
        asked.contains("The rebase run of pull request #1 asks before it can go on."),
        "{asked}"
    );
    assert_eq!(bench.remote_branch().unwrap(), first_push);
}

/// A ticket whose Build's question the resolver decided, delivered in the
/// same command: the DECISION comment's id.
fn delivered_with_a_decision(bench: &Bench) -> String {
    let (outcome, printed) = bench.run(
        vec![
            bench.reply(None, Some(NAMING)),
            bench.reply(None, Some(DECIDED_Q1)),
            bench.reply(Some("Hello"), Some(DONE)),
        ],
        None,
    );
    outcome.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    let decisions = bench.record().questions.decisions;
    assert_eq!(decisions.len(), 1);
    assert!(decisions[0].reversed.is_none());
    decisions[0].comment.clone()
}

/// The `decision` events of correction runs, apart from a review pass's,
/// which also tell an `outcome` (OWL-304).
fn corrections(events: &[Event]) -> Vec<Value> {
    data_of(events, EventKind::Decision, "watch")
        .into_iter()
        .filter(|data| data["watch"] == "correction")
        .collect()
}

/// A correction run's `done`, the greeting moved as the reply asks.
const CORRECTED: &str = r#"{"format":13,"status":"done","summary":"Moved the greeting as the reply asks.",
"pr":{"branch":"owlshift/demo-1","title":"Add a greeting","body":"Corrected."}}"#;

/// Scenario S7's reversal (OWL-146): once the ticket is delivered, the
/// decider replies to the DECISION comment, and watch runs a correction run
/// briefed with the reply once it counts, whose commit is pushed with a lease
/// on the delivery; the decision keeps the reply as followed, and the next
/// pass runs nothing. A stranger's reply reverses nothing. Its DELIVERY
/// report, refused by the tracker, is owed and posted once at the next pass
/// (OWL-218).
#[test]
fn a_reply_to_a_decision_starts_a_correction_run_under_watch() {
    let bench = Bench::new(true);
    let decision = delivered_with_a_decision(&bench);
    let delivered_events = bench.events().len();
    let first_push = bench.remote_branch().unwrap();
    let ran = || bench.briefs().len();
    let mut pass = 0;
    let (outcomes, lines, printed) = bench.watch(
        vec![bench.reply(Some("Hello from HELLO.md"), Some(CORRECTED))],
        &mut |_| {
            pass += 1;
            match pass {
                // Nothing to correct. A stranger replies, then the decider,
                // within the quiet window.
                1 => {
                    assert_eq!(ran(), 4);
                    bench.reply_as("reviewer", &decision, "Keep GREETING.md.");
                    bench.reply_as("maintainer", &decision, "No: put it in HELLO.md.");
                    bench.waited.set(SignedDuration::ZERO);
                }
                // Settling: nothing ran. Past the window now, and the tracker
                // refuses the DELIVERY report.
                2 => {
                    assert_eq!(ran(), 4);
                    bench.waited.set(SignedDuration::from_mins(10));
                    bench.refuse.set(Some("[owlshift] DELIVERY"));
                }
                // The correction ran and was pushed; its report is owed.
                3 => {
                    assert_eq!(ran(), 5);
                    assert!(bench.record().delivery.unwrap().owed.is_some());
                    bench.refuse.set(None);
                }
                // Posted; the next pass runs nothing and posts nothing.
                4 => assert!(bench.record().delivery.unwrap().owed.is_none()),
                _ => {}
            }
            pass < 5
        },
    );
    assert_eq!(outcomes.len(), 1, "{outcomes:?}\n{lines}\n{printed}");
    assert!(
        matches!(&outcomes[0], Err(Stop::Delivery(why)) if why.contains("the next pass posts it")),
        "{outcomes:?}\n{printed}"
    );
    assert_eq!(ran(), 5);
    // Working while the correction run worked, review once pushed, its
    // report refused or not (OWL-259).
    assert_eq!(stage_moves(&bench, delivered_events), ["working", "review"]);
    assert_eq!(bench.stage(), "In Review");

    // The build role at Watch, given the decider's reply alone.
    let brief = bench.briefs().pop().unwrap();
    assert_eq!(brief.role, Role::Build);
    let correction = brief.correction.expect("a correction");
    assert_eq!(correction.pull_request, 1);
    assert_eq!(correction.head, first_push);
    let reversed = &correction.reversed[0];
    assert_eq!(
        reversed.question.text,
        "Which file should hold the greeting?"
    );
    assert_eq!(reversed.decision, "GREETING.md, at the root.");
    assert_eq!(reversed.reply, "No: put it in HELLO.md.");

    // Pushed as a fast-forward leased on the delivery, then reported.
    let events = &bench.events()[delivered_events..];
    let pushes = data_of(events, EventKind::TrackerWrite, "lease");
    assert_eq!(pushes.len(), 1);
    assert_eq!(pushes[0]["lease"], json!(first_push));
    assert_eq!(pushes[0]["pushed"], "fast_forward");
    let last = bench.remote_branch().unwrap();
    assert_eq!(pushes[0]["commit"], json!(last));
    let corrected: Vec<String> = bench
        .comments()
        .into_iter()
        .filter(|c| c.contains("**Corrected after delivery,**"))
        .collect();
    assert_eq!(corrected.len(), 1, "{corrected:?}");
    let report = &corrected[0];
    assert!(report.starts_with("[owlshift] DELIVERY"), "{report}");
    assert!(
        report.contains(
            "**Corrected after delivery,** to follow the decider's replies to the decisions on \
             **Which file should hold the greeting?**."
        ),
        "{report}"
    );
    assert!(!report.contains("Fix pass"), "{report}");
    // The decision the reply reverses is listed as reversed in this very
    // report, before the durable mark moves (OWL-233).
    assert!(
        report.contains("GREETING.md, at the root. (reversed by the decider's reply)"),
        "{report}"
    );

    // The decision keeps the reply as followed; the state counts nothing.
    let record = bench.record();
    let kept = record.questions.decisions[0].reversed.as_ref().unwrap();
    let replies = MarkdownTracker::new(&bench.remote.checkout)
        .comments(&ticket())
        .unwrap();
    let reply = replies
        .iter()
        .find(|c| c.body == "No: put it in HELLO.md.")
        .unwrap();
    assert_eq!(kept.comment, reply.id);
    assert_eq!(kept.through, reply.at);
    let state = state_of(&bench);
    assert_eq!(
        state.status(),
        Status::Active(owlshift_core::vocab::Stage::Watch)
    );
    assert_eq!((state.fix_passes(), state.repairs()), (0, 0));
    assert_eq!(
        record.delivery.unwrap().pushed.as_deref(),
        Some(last.as_str())
    );
    let decided = data_of(events, EventKind::Decision, "outcome");
    assert_eq!(decided.len(), 1, "{decided:?}");
    assert_eq!(decided[0]["watch"], "correction");
    assert_eq!(decided[0]["outcome"], "pushed");
    assert_eq!(decided[0]["questions"], json!(["Q1"]));
    assert_eq!(decided[0]["replies"], json!([reply.id]));
}

/// A reply the delivering Build read starts nothing once delivered: the
/// delivery keeps it as followed (OWL-146). A later reply, which the pull
/// request already follows, gets a correction run that changes nothing and
/// is reported so, through `owlshift continue` at Watch.
#[test]
fn a_reply_followed_before_delivery_starts_nothing_and_one_already_followed_is_reported() {
    let bench = Bench::new(true);
    let mut limited = bench.reply(None, None);
    limited.usage_limit = Some("2026-10-03T18:00:00Z".parse().unwrap());
    let (stopped, printed) = bench.run(
        vec![
            bench.reply(None, Some(NAMING)),
            bench.reply(None, Some(DECIDED_Q1)),
            limited,
        ],
        None,
    );
    assert!(
        matches!(&stopped, Err(Stop::UsageLimit { .. })),
        "{stopped:?}\n{printed}"
    );
    let decision = bench.record().questions.decisions[0].comment.clone();
    bench.reply_as("maintainer", &decision, "No: put it in HELLO.md.");
    let (delivered, printed) =
        bench.continue_ticket(vec![bench.reply(Some("Hello from HELLO.md"), Some(DONE))]);
    delivered.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    let build = bench.last_of(Role::Build);
    assert!(
        build
            .thread
            .iter()
            .any(|entry| matches!(entry, ThreadEntry::Comment { body, .. } if body == "No: put it in HELLO.md.")),
        "{:?}",
        build.thread
    );
    assert!(bench.record().questions.decisions[0].reversed.is_some());

    // Nothing to correct: a look.
    let (look, printed) = bench.continue_ticket(Vec::new());
    assert!(
        matches!(&look, Err(Stop::Watched { .. })),
        "{look:?}\n{printed}"
    );
    assert_eq!(bench.briefs().len(), 5);

    // A newer reply the pull request already follows. The tracker refuses
    // the report saying so: it is owed (OWL-236), and the next `continue`
    // posts it, once, even after a crash that lost the ref's write.
    bench.reply_as("maintainer", &decision, "To be clear: HELLO.md. go");
    let pushed = bench.remote_branch();
    bench.refuse.set(Some("[owlshift] DELIVERY"));
    let (unchanged, printed) = bench.continue_ticket(vec![bench.reply(
        None,
        Some(r#"{"format":13,"status":"done","summary":"HELLO.md already holds the greeting."}"#),
    )]);
    assert!(
        matches!(&unchanged, Err(Stop::Delivery(why)) if why.contains("the next pass posts it")),
        "{unchanged:?}\n{printed}"
    );
    let owing = bench.record();
    let owed = owing.delivery.as_ref().unwrap().owed.as_ref().unwrap();
    assert!(owed.unchanged && owed.kind == owlshift_contracts::refs::PushKind::Correction);
    bench.refuse.set(None);
    let reported = reports(&bench).len();
    for _ in 0..2 {
        let (look, printed) = bench.continue_ticket(Vec::new());
        assert!(
            matches!(&look, Err(Stop::Watched { .. })),
            "{look:?}\n{printed}"
        );
        assert_eq!(reports(&bench).len(), reported + 1);
        assert!(bench.record().delivery.unwrap().owed.is_none());
        // The post landed, and a crash lost the ref's write.
        let mut lost = bench.record();
        lost.delivery = owing.delivery.clone();
        bench.write_record(&lost);
    }
    let brief = bench.briefs().pop().unwrap();
    assert_eq!(
        brief.correction.unwrap().reversed[0].reply,
        "To be clear: HELLO.md. go"
    );
    assert_eq!(bench.remote_branch(), pushed, "nothing pushed");
    let report = bench.comments().pop().unwrap();
    assert!(
        report.contains("**No correction needed after delivery:**"),
        "{report}"
    );
    assert!(
        report.contains("HELLO.md already holds the greeting."),
        "{report}"
    );
    assert!(
        report.contains("GREETING.md, at the root. (reversed by the decider's reply)"),
        "{report}"
    );
    let kept = bench.record().questions.decisions[0]
        .reversed
        .clone()
        .unwrap();
    assert_ne!(kept.comment, "");
    let outcomes: Vec<Value> = corrections(&bench.events())
        .into_iter()
        .map(|d| d["outcome"].clone())
        .collect();
    assert_eq!(outcomes, [json!("unchanged")]);
    // Followed: a third look runs nothing.
    let (look, _) = bench.continue_ticket(Vec::new());
    assert!(matches!(look, Err(Stop::Watched { .. })), "{look:?}");
}

/// A correction run that fails gets one more run; one that asks goes to
/// the decider as a fix run's question does (OWL-211), the decision keeping
/// no reply as followed. Once answered, watch's read corrects again, the
/// round in its thread, and the reply is then followed (OWL-146).
#[test]
fn a_correction_that_fails_then_asks_goes_to_the_decider_then_corrects() {
    let bench = Bench::new(true);
    let decision = delivered_with_a_decision(&bench);
    bench.reply_as("maintainer", &decision, "Use the README instead. go");
    let asks = r#"{"format":13,"status":"questions","summary":"The README has no greeting section.",
"questions":[{"id":"Q1","category":"scope","context":"The README holds the install steps only.",
"text":"Where in the README should the greeting go?","recommendation":{"why":"The simplest reading of the ticket."}}]}"#;
    let (outcome, printed) =
        bench.continue_ticket(vec![bench.reply(None, None), bench.reply(None, Some(asks))]);
    assert!(
        matches!(&outcome, Err(Stop::NeedsInput { posted: Ok(_), .. })),
        "{outcome:?}\n{printed}"
    );
    let briefs = bench.briefs();
    assert_eq!(briefs.len(), 6);
    assert!(briefs[4..].iter().all(|b| b.correction.is_some()));
    let asked = bench.comments().pop().unwrap();
    assert!(asked.starts_with("[owlshift] QUESTIONS · round"), "{asked}");
    assert!(
        asked.contains("The correction run of pull request #1 asks before it can go on."),
        "{asked}"
    );
    assert_eq!(
        state_of(&bench).status(),
        Status::NeedsInput {
            return_to: Stage::Watch
        }
    );
    assert!(bench.record().questions.decisions[0].reversed.is_none());
    assert!(corrections(&bench.events()).is_empty());

    bench.answer("Q1: Under a new Greeting heading. Go.\n");
    let (outcome, printed) = bench.continue_ticket(vec![
        bench.reply(
            None,
            Some(&check(&[("Q1", "answered", "Under a Greeting heading.")])),
        ),
        bench.reply(Some("Hello from the README"), Some(CORRECTED)),
    ]);
    outcome.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    let correction = bench.briefs().pop().unwrap();
    assert!(correction.correction.is_some());
    assert!(
        decider_says(&correction)
            .iter()
            .any(|said| said.contains("Under a new Greeting heading"))
    );
    assert!(bench.record().questions.decisions[0].reversed.is_some());
    assert_eq!(bench.stage(), "In Review");
}

/// An intake result (OWL-248): `status`, with the record naming `DEMO-2`
/// when `dependency` is given, and `questions` when it asks.
fn intake_result(status: &str, dependency: Option<&str>, questions: &str) -> String {
    let dependencies: Vec<Value> = dependency
        .map(|id| json!({ "ticket": id, "quote": format!("Blocked by {id}.") }))
        .into_iter()
        .collect();
    let mut result = json!({
        "format": 13,
        "status": status,
        "summary": "A small greeting.",
        "intake": {
            "variant": "trivial",
            "routing": { "harness": "claude", "tier": "fast", "effort": "low" },
            "dependencies": dependencies,
        },
    });
    if !questions.is_empty() {
        result["questions"] = serde_json::from_str(questions).unwrap();
    }
    result.to_string()
}

/// A gesture of `account`, by the label the drained bench admits with.
fn gesture(account: &str) -> Gesture {
    Gesture {
        kind: GestureKind::Label,
        name: "agent".to_owned(),
        account: account.to_owned(),
        at: "2026-10-02T08:00:00Z".parse().unwrap(),
    }
}

impl Bench {
    /// Runs intake on DEMO-1 for the drain, under `gesture`, with one reply
    /// per run.
    fn intake(&self, replies: Vec<Reply>, gesture: Gesture) -> Result<TicketIntake, Stop> {
        self.with_on_demand(replies, None, Vec::new(), |on_demand, sink| {
            on_demand.intake(&ticket(), gesture, sink)
        })
        .0
    }
}

/// Intake for the drain (OWL-248): a gesture by an account `admitters` does
/// not list runs nothing; the intake role runs read-only and without
/// network; a dependency the ticket's text does not name is refused, a
/// failed run; the next run's record is kept in the ticket ref, its
/// dependency unknown since the tracker has no such ticket (OWL-249), and
/// shown in an INTAKE comment; the ticket, already in the ready column, is
/// not moved.
#[test]
fn intake_keeps_its_record_and_leaves_the_ticket_in_the_ready_column() {
    let bench = Bench::drained();
    let stop = bench
        .intake(Vec::new(), gesture("stranger"))
        .expect_err("not an admitter");
    assert!(
        matches!(&stop, Stop::Refused(why) if why.contains("does not list")),
        "{stop}"
    );

    bench.comment_as("maintainer", "Blocked by DEMO-2.");
    let guessed = bench.reply(None, Some(&intake_result("done", Some("DEMO-9"), "")));
    let done = bench.reply(None, Some(&intake_result("done", Some("DEMO-2"), "")));
    let record = bench
        .intake(vec![guessed, done], gesture("maintainer"))
        .unwrap();
    assert_eq!(record.dependencies[0].outcome, DependencyOutcome::Unknown);
    assert_eq!(record.gesture.account, "maintainer");
    let kept = bench.record();
    assert_eq!(kept.intake, Some(record));
    let state = TicketState::try_from(&kept.state).unwrap();
    assert_eq!(state.status(), Status::Active(Stage::Ready));

    let briefs = bench.briefs();
    assert_eq!(briefs.len(), 2);
    for brief in &briefs {
        assert_eq!(brief.role, Role::Intake);
        assert_eq!(brief.permissions.level, PermissionLevel::ReadOnly);
        assert!(!brief.permissions.network);
        assert!(!brief.rules.is_empty(), "the project's rules");
    }
    let refused: Vec<Event> = bench
        .events()
        .into_iter()
        .filter(|e| e.kind == EventKind::Warning && e.data["what"] == json!("intake_refused"))
        .collect();
    assert_eq!(refused.len(), 1);
    let comments = bench.comments();
    assert!(comments.last().unwrap().starts_with("[owlshift] INTAKE"));
    assert!(comments.last().unwrap().contains("no such ticket"));
    assert_eq!(bench.stage(), "Todo");
    assert!(
        !kinds(&bench.events()).contains(&EventKind::Dispatch),
        "intake dispatches nothing"
    );
}

/// An intake round (OWL-248): its questions go to the decider, the record
/// kept with the round; intake is refused while they wait; a park while
/// they wait restarts to waiting, shown as such (OWL-269); the answers'
/// RESUME returns to Intake, which runs again before the ticket goes back
/// to the ready column (OWL-269).
#[test]
fn an_intake_round_returns_the_ticket_to_the_ready_column() {
    let bench = Bench::drained();
    let asks = bench.reply(
        None,
        Some(&intake_result(
            "questions",
            None,
            r#"[{"id":"Q1","category":"scope","context":"The ticket names no language.",
                "text":"Which language should the greeting use?","recommendation":{"why":"The simplest reading of the ticket."}}]"#,
        )),
    );
    let stop = bench
        .intake(vec![asks], gesture("maintainer"))
        .expect_err("questions");
    assert!(
        matches!(&stop, Stop::NeedsInput { posted: Ok(round), .. } if round.get() == 1),
        "{stop}"
    );
    let kept = bench.record();
    assert!(kept.intake.is_some(), "the record is kept with the round");
    assert_eq!(
        TicketState::try_from(&kept.state).unwrap().status(),
        Status::NeedsInput {
            return_to: Stage::Intake
        }
    );
    assert_eq!(bench.stage(), "Needs Input");
    let stop = bench
        .intake(Vec::new(), gesture("maintainer"))
        .expect_err("waiting");
    assert!(matches!(stop, Stop::Refused(_)), "{stop}");

    // Parked while the round waits: `continue` restarts it to waiting, and
    // the ticket shows it waits, not that it is parked.
    let parked = TicketState::restore(
        Status::Parked {
            at: Stage::Intake,
            awaiting_input: true,
        },
        1,
        0,
        0,
        0,
        0,
    )
    .unwrap();
    bench.write_record(&TicketRecord {
        state: PersistedState::from(&parked),
        ..bench.record()
    });
    MarkdownTracker::new(&bench.remote.checkout)
        .set_stage(&ticket(), "Parked")
        .unwrap();
    let (outcome, _) = bench.continue_ticket(Vec::new());
    assert!(matches!(outcome, Err(Stop::Waiting { .. })), "{outcome:?}");
    assert_eq!(bench.stage(), "Needs Input");

    bench.answer("English.");
    let (outcome, _) = bench.continue_ticket(vec![
        bench.reply(None, Some(&check(&[("Q1", "answered", "English.")]))),
        bench.reply(None, Some(&intake_result("done", None, ""))),
    ]);
    let stop = outcome.expect_err("back to the ready column");
    assert!(matches!(stop, Stop::Ready { .. }), "{stop}");
    assert_eq!(bench.stage(), "Todo");
    let kept = bench.record();
    assert!(kept.intake.is_some());
    assert_eq!(
        TicketState::try_from(&kept.state).unwrap().status(),
        Status::Active(Stage::Ready)
    );
    assert!(
        bench
            .comments()
            .iter()
            .any(|c| c.starts_with("[owlshift] RESUME")
                && c.contains("Intake reads the ticket again"))
    );
    let roles: Vec<Role> = bench.briefs().iter().map(|b| b.role).collect();
    assert_eq!(roles, [Role::Intake, Role::AnswerCheck, Role::Intake]);
}

/// A false premise at intake (OWL-306, S8): its question goes through the
/// resolver first, told the premise is false (OWL-342), with the runner's
/// own closure as its last option; passed on, it is a round returning to
/// Intake; the QUESTIONS comment says the premise is false; no intake record
/// is kept; the answered round runs intake again.
#[test]
fn a_false_premise_at_intake_is_a_round_to_the_decider() {
    let bench = Bench::drained();
    let premise = r#"{"format":13,"status":"premise_false",
"summary":"The ticket extends a README section that does not exist.",
"questions":[{"id":"Q1","category":"naming","context":"The ticket extends README.md's Usage section; there is none.",
"text":"Re-scope to write the section, or close the ticket?",
"options":[{"label":"Re-scope: write the section."}],"recommendation":{"option":"A","why":"Re-scope"}}]}"#;
    let asks = bench.reply(None, Some(premise));
    let passes = bench.reply(None, Some(&passed_on(&["Q1"])));
    let stop = bench
        .intake(vec![asks, passes], gesture("maintainer"))
        .expect_err("the premise goes to the decider");
    let Stop::NeedsInput { questions, .. } = &stop else {
        panic!("{stop}");
    };
    let labels: Vec<&str> = questions[0]
        .options
        .iter()
        .map(|o| o.label.as_str())
        .collect();
    assert_eq!(
        labels,
        [
            "Re-scope: write the section.",
            "Close the ticket: its premise is false"
        ]
    );
    let told = bench.last_of(Role::Resolver);
    let asked = told.asked_by.as_ref().unwrap();
    assert!(asked.premise_false && asked.role == Role::Intake);
    assert_eq!(told.resolve[0].options.len(), 2);
    assert!(
        matches!(
            &stop,
            Stop::NeedsInput {
                status: owlshift_contracts::result::Status::PremiseFalse,
                posted: Ok(round),
                ..
            } if round.get() == 1
        ),
        "{stop}"
    );
    let roles: Vec<Role> = bench.briefs().iter().map(|b| b.role).collect();
    assert_eq!(roles, [Role::Intake, Role::Resolver]);
    let kept = bench.record();
    assert!(kept.intake.is_none(), "a premise round has no record");
    assert_eq!(
        TicketState::try_from(&kept.state).unwrap().status(),
        Status::NeedsInput {
            return_to: Stage::Intake
        }
    );
    assert_eq!(bench.stage(), "Needs Input");
    assert!(
        bench
            .comments()
            .iter()
            .any(|c| c.starts_with("[owlshift] QUESTIONS")
                && c.contains("The run found the ticket's premise false."))
    );

    bench.answer("Re-scope: write the section.");
    let (outcome, _) = bench.continue_ticket(vec![
        bench.reply(
            None,
            Some(&check(&[(
                "Q1",
                "answered",
                "Re-scope: write the section.",
            )])),
        ),
        bench.reply(None, Some(&intake_result("done", None, ""))),
    ]);
    let stop = outcome.expect_err("back to the ready column");
    assert!(matches!(stop, Stop::Ready { .. }), "{stop}");
    assert!(bench.record().intake.is_some());
}

/// A false premise at intake that asks nothing parks the ticket at Intake,
/// which `continue` restarts (OWL-306).
#[test]
fn a_false_premise_at_intake_without_a_question_parks_the_ticket() {
    let bench = Bench::drained();
    let premise = r#"{"format":13,"status":"premise_false","summary":"The section is not there."}"#;
    let stop = bench
        .intake(
            vec![bench.reply(None, Some(premise))],
            gesture("maintainer"),
        )
        .expect_err("parked");
    assert!(
        matches!(
            &stop,
            Stop::Parked {
                reason: ParkReason::Blocked,
                ..
            }
        ),
        "{stop}"
    );
    let kept = bench.record();
    assert!(kept.intake.is_none());
    assert!(matches!(
        TicketState::try_from(&kept.state).unwrap().status(),
        Status::Parked {
            at: Stage::Intake,
            ..
        }
    ));
    let parked = bench.comments().pop().unwrap();
    assert!(parked.starts_with("[owlshift] PARKED"), "{parked}");
    assert!(
        parked.contains("found its premise false, with no question"),
        "{parked}"
    );
    assert!(parked.contains("owlshift continue"), "{parked}");
}

/// A prerequisite the decider names only in an answer to an intake round
/// (OWL-269): the answers' RESUME returns to Intake, which runs again with
/// them; cut off there by a usage limit, the ticket stays at Intake, shown
/// waiting, out of the ready column, its record the round's; `continue`
/// runs intake again, which records the prerequisite, quoted from the
/// answer, writes its relation, and moves the ticket back to the column.
#[test]
fn a_dependency_named_only_in_an_answer_becomes_a_relation() {
    let bench = Bench::drained();
    bench.add_ticket("DEMO-2", "Todo", &[]);
    let asks = bench.reply(
        None,
        Some(&intake_result(
            "questions",
            None,
            r#"[{"id":"Q1","category":"scope",
                "context":"The ticket waits for the billing rework, named without its identifier.",
                "text":"Which ticket is the billing rework?","recommendation":{"why":"The simplest reading of the ticket."}}]"#,
        )),
    );
    bench
        .intake(vec![asks], gesture("maintainer"))
        .expect_err("questions");
    let answer = "It is DEMO-2: it must land first.";
    bench.answer(answer);

    let mut limited = bench.reply(None, None);
    limited.usage_limit = Some("2026-10-03T18:00:00Z".parse().unwrap());
    let (outcome, _) = bench.continue_ticket(vec![
        bench.reply(None, Some(&check(&[("Q1", "answered", "DEMO-2.")]))),
        limited,
    ]);
    assert!(
        matches!(outcome, Err(Stop::UsageLimit { .. })),
        "{outcome:?}"
    );
    let held = bench.record();
    assert_eq!(
        TicketState::try_from(&held.state).unwrap().status(),
        Status::Active(Stage::Intake)
    );
    assert_eq!(bench.stage(), "Needs Input");
    assert!(held.intake.unwrap().dependencies.is_empty());
    assert!(bench.relations_written().is_empty());

    let mut done: Value = serde_json::from_str(&intake_result("done", None, "")).unwrap();
    done["intake"]["dependencies"] = json!([{ "ticket": "DEMO-2", "quote": answer }]);
    let (outcome, _) = bench.continue_ticket(vec![bench.reply(None, Some(&done.to_string()))]);
    assert!(matches!(outcome, Err(Stop::Ready { .. })), "{outcome:?}");
    let record = bench.record().intake.expect("the record");
    assert_eq!(record.dependencies[0].ticket.as_str(), "DEMO-2");
    assert_eq!(record.dependencies[0].quote, answer);
    assert_eq!(record.dependencies[0].outcome, DependencyOutcome::Written);
    assert_eq!(bench.blocked_by(), ["DEMO-2"]);
    assert_eq!(bench.relations_written(), ["DEMO-2 -> DEMO-1"]);
    // Shown waiting from the round to the second intake's end.
    assert_eq!(
        bench.stage_events(EventKind::TrackerWrite),
        ["Needs Input", "Todo"]
    );
    let intakes: Vec<String> = bench
        .comments()
        .into_iter()
        .filter(|c| c.starts_with("[owlshift] INTAKE"))
        .collect();
    assert_eq!(intakes.len(), 2);
    assert!(intakes[1].contains("DEMO-2"), "{}", intakes[1]);
}

/// The second failed intake run parks the ticket at Intake, making its
/// ticket ref; `continue` restarts intake, under the gesture read again,
/// and the ticket goes back from parked to the ready column (OWL-248).
#[test]
fn a_parked_intake_is_restarted_by_continue_and_returns_to_ready() {
    let bench = Bench::drained();
    let failed = || {
        bench.reply(
            None,
            Some(r#"{"format":13,"status":"failed","summary":"Lost."}"#),
        )
    };
    let stop = bench
        .intake(vec![failed(), failed()], gesture("maintainer"))
        .expect_err("parked");
    assert!(matches!(stop, Stop::Parked { .. }), "{stop}");
    assert_eq!(bench.stage(), "Parked");
    assert!(
        bench
            .comments()
            .last()
            .unwrap()
            .contains("owlshift continue")
    );

    let done = bench.reply(None, Some(&intake_result("done", None, "")));
    let (outcome, _) = bench.continue_ticket(vec![done]);
    let stop = outcome.expect_err("back to the ready column");
    assert!(matches!(stop, Stop::Ready { .. }), "{stop}");
    // Kept parked while intake ran, then moved back.
    assert_eq!(
        bench.stage_events(EventKind::TrackerWrite),
        ["Parked", "Todo"]
    );
    let record = bench.record().intake.expect("the record");
    // The Markdown tracker keeps no history: the ticket's author made it.
    assert_eq!(record.gesture.account, "maintainer");
}

/// `continue` on a ticket left at Intake, by a usage limit or a crash, runs
/// intake again (OWL-248).
#[test]
fn continue_runs_intake_on_a_ticket_at_intake() {
    let bench = Bench::drained();
    // A park makes the checkout and the ref; the ticket is then left at
    // Intake, as a usage limit leaves it.
    let failed = || {
        bench.reply(
            None,
            Some(r#"{"format":13,"status":"failed","summary":"Lost."}"#),
        )
    };
    bench
        .intake(vec![failed(), failed()], gesture("maintainer"))
        .expect_err("parked");
    let at_intake = TicketState::restore(Status::Active(Stage::Intake), 0, 0, 0, 0, 0).unwrap();
    bench.write_record(&TicketRecord {
        state: PersistedState::from(&at_intake),
        ..bench.record()
    });
    let done = bench.reply(None, Some(&intake_result("done", None, "")));
    let (outcome, _) = bench.continue_ticket(vec![done]);
    assert!(matches!(outcome, Err(Stop::Ready { .. })), "{outcome:?}");
    assert!(bench.record().intake.is_some());
}

/// A Build run's `done` with two follow-ups (OWL-253).
const PROPOSING: &str = r#"{"format":13,"status":"done","summary":"Added GREETING.md; the gate passes.",
"pr":{"branch":"owlshift/demo-1","title":"Add a greeting","body":"Says hello. Gate: green."},
"followups":[
 {"title":"Translate the greeting","why":"Readers abroad.","evidence":["GREETING.md:1"],"source":"agent","done_when":"A French greeting exists."},
 {"title":"Lint the docs","why":"Typos slip in.","source":"reviewer","done_when":"A linter runs in CI."}]}"#;

/// The proposals the ticket ref keeps, with what became of each, in order.
fn proposal_outcomes(bench: &Bench) -> Vec<ProposalOutcome> {
    let env = bench.env.clone();
    let git = Git::with_setup("git", move |command| env.apply(command));
    ticket_ref::read(&git, &bench.dirs().checkout(), &ticket())
        .unwrap()
        .expect("a ticket ref")
        .proposals
        .expect("proposals kept")
        .proposals
        .into_iter()
        .map(|kept| kept.outcome)
        .collect()
}

/// The outcomes of the `decision` events about proposals, and the `what`
/// of their warnings, oldest first.
fn proposal_events(events: &[Event]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match event.kind {
            EventKind::Decision if event.data.get("drain") == Some(&json!("proposal")) => {
                Some(event.data["outcome"].as_str().unwrap().to_owned())
            }
            EventKind::Warning => event.data["what"]
                .as_str()
                .filter(|what| what.starts_with("proposal_"))
                .map(str::to_owned),
            _ => None,
        })
        .collect()
}

/// OWL-253: a delivery's follow-ups are filed in the tracker's proposals
/// inbox before its DELIVERY report, which names what became of each, and
/// kept as filed; one already proposed is a duplicate, never filed.
#[test]
fn follow_ups_are_filed_before_the_delivery_report_which_names_them() {
    let bench = Bench::new(true);
    {
        let mut inbox = bench.inbox.borrow_mut();
        inbox.on = true;
        // Someone filed the second one by hand already.
        inbox.tickets.push((
            TicketId::new("DEMO-99").unwrap(),
            "lint the docs".to_owned(),
            String::new(),
        ));
    }
    let (outcome, printed) = bench.run(vec![bench.reply(Some("Hello"), Some(PROPOSING))], None);
    outcome.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));

    let inbox = bench.inbox.borrow();
    assert_eq!(inbox.tickets.len(), 2);
    let (id, title, description) = &inbox.tickets[1];
    assert_eq!(
        (id.as_str(), title.as_str()),
        ("DEMO-101", "Translate the greeting")
    );
    assert!(
        description.ends_with(&format!(
            "\n[owlshift] PROPOSAL · DEMO-1 · fingerprint {}\n",
            owlshift_runner::proposals::fingerprint("Translate the greeting")
        )),
        "{description}"
    );
    let comments = bench.comments();
    assert_eq!(comments.len(), 1, "{comments:?}");
    for line in [
        "- Proposed follow-up **Translate the greeting**, filed as DEMO-101 for triage: \
         Readers abroad.",
        "- Proposed follow-up **Lint the docs**, not filed, already proposed as DEMO-99: \
         Typos slip in.",
    ] {
        assert!(comments[0].contains(line), "{line}\n---\n{}", comments[0]);
    }
    assert_eq!(
        proposal_outcomes(&bench),
        [
            ProposalOutcome::Filed {
                ticket: TicketId::new("DEMO-101").unwrap()
            },
            ProposalOutcome::Duplicate {
                ticket: TicketId::new("DEMO-99").unwrap()
            },
        ]
    );
    // Filed, then the report.
    let events = bench.events();
    assert_eq!(proposal_events(&events), ["duplicate", "filed"]);
    let at = |kind: &str| {
        events
            .iter()
            .position(|e| e.kind == EventKind::TrackerWrite && e.data["action"] == json!(kind))
            .unwrap_or_else(|| panic!("no {kind}"))
    };
    let delivery = events
        .iter()
        .position(|e| {
            e.kind == EventKind::TrackerWrite && e.data.get("kind") == Some(&json!("DELIVERY"))
        })
        .unwrap();
    assert!(at("propose") < delivery, "{events:?}");
}

/// OWL-253: a filing the tracker refuses stays owed, warned of once, and the
/// report says why; the next watch pass files it, with no second report.
#[test]
fn a_refused_filing_stays_owed_and_a_watch_pass_files_it_without_a_second_report() {
    let bench = Bench::new(true);
    {
        let mut inbox = bench.inbox.borrow_mut();
        inbox.on = true;
        inbox.refuse = Some("the plan holds 250 issues");
    }
    let (outcome, printed) = bench.run(vec![bench.reply(Some("Hello"), Some(PROPOSING))], None);
    outcome.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    let comments = bench.comments();
    assert_eq!(comments.len(), 1, "{comments:?}");
    let line = "- Proposed follow-up **Translate the greeting**, owed, not filed yet: tracker: \
                the plan holds 250 issues: Readers abroad.";
    assert!(comments[0].contains(line), "{}", comments[0]);
    let owed = ProposalOutcome::Owed {
        refusal: Some("tracker: the plan holds 250 issues".to_owned()),
    };
    assert_eq!(proposal_outcomes(&bench), [owed.clone(), owed]);
    assert_eq!(
        proposal_events(&bench.events()),
        ["proposal_unfiled", "proposal_unfiled"]
    );

    bench.inbox.borrow_mut().refuse = None;
    let before = bench.events().len();
    let mut passes = 0;
    let (outcomes, lines, printed) = bench.watch(Vec::new(), &mut |_| {
        passes += 1;
        passes < 2
    });
    assert!(outcomes.is_empty(), "{outcomes:?}\n{lines}\n{printed}");
    assert_eq!(bench.inbox.borrow().tickets.len(), 2);
    assert_eq!(bench.comments().len(), 1, "no second report");
    assert_eq!(
        proposal_outcomes(&bench),
        [
            ProposalOutcome::Filed {
                ticket: TicketId::new("DEMO-100").unwrap()
            },
            ProposalOutcome::Filed {
                ticket: TicketId::new("DEMO-101").unwrap()
            },
        ]
    );
    // Filed once, by the first pass.
    assert_eq!(
        proposal_events(&bench.events()[before..]),
        ["filed", "filed"]
    );
}

/// An intake result whose record names each of `ids`, each quoted as
/// "Blocked by <id>.", asking `questions` when they are not empty.
fn intake_naming(status: &str, ids: &[&str], questions: &str) -> String {
    let mut result: Value = serde_json::from_str(&intake_result(status, None, questions)).unwrap();
    result["intake"]["dependencies"] = ids
        .iter()
        .map(|id| json!({ "ticket": id, "quote": format!("Blocked by {id}.") }))
        .collect();
    result.to_string()
}

impl Bench {
    /// Adds the ticket `id` at `stage` to the tracker, `blocked_by` these.
    fn add_ticket(&self, id: &str, stage: &str, blocked_by: &[&str]) {
        let dir = self.remote.checkout.join("tickets").join(id);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("ticket.md"),
            format!(
                "+++\ntitle = \"{id}\"\nauthor = \"maintainer\"\nstage = \"{stage}\"\n\
                 blocked_by = {blocked_by:?}\n+++\n"
            ),
        )
        .unwrap();
    }

    /// What DEMO-1's front matter lists as blocking it.
    fn blocked_by(&self) -> Vec<String> {
        MarkdownTracker::new(&self.remote.checkout)
            .ticket(&ticket())
            .unwrap()
            .blocked_by
            .iter()
            .map(|id| id.as_str().to_owned())
            .collect()
    }

    /// The relations the runner wrote, as `from -> to`.
    fn relations_written(&self) -> Vec<String> {
        self.events()
            .iter()
            .filter(|e| e.kind == EventKind::TrackerWrite && e.data["action"] == json!("relation"))
            .map(|e| {
                format!(
                    "{} -> {}",
                    e.data["from"].as_str().unwrap(),
                    e.data["to"].as_str().unwrap()
                )
            })
            .collect()
    }
}

/// Dependencies to relations (OWL-249): each dependency of a kept verdict
/// gets its outcome, in the record and the INTAKE comment, and a `blocks`
/// relation is written, as DEMO-1's `blocked_by`, only for the open one
/// that neither blocks DEMO-1 already nor is blocked by it.
#[test]
fn intake_writes_a_relation_for_each_open_dependency_and_names_the_rest() {
    let bench = Bench::drained();
    bench.add_ticket("DEMO-2", "Todo", &[]);
    bench.add_ticket("DEMO-3", "Done", &[]);
    bench.add_ticket("DEMO-4", "Todo", &["DEMO-6"]);
    bench.add_ticket("DEMO-6", "Todo", &["DEMO-1"]);
    bench.add_ticket("DEMO-5", "Todo", &[]);
    MarkdownTracker::new(&bench.remote.checkout)
        .add_blocked_by(&ticket(), &TicketId::new("DEMO-5").unwrap())
        .unwrap();
    let named = [
        "DEMO-2", "DEMO-3", "DEMO-9", "DEMO-1", "DEMO-4", "DEMO-5", "DEMO-2",
    ];
    bench.comment_as("maintainer", &format!("Blocked by {}.", named.join(", ")));

    let done = bench.reply(None, Some(&intake_naming("done", &named, "")));
    let record = bench.intake(vec![done], gesture("maintainer")).unwrap();
    let outcomes: Vec<(&str, DependencyOutcome)> = record
        .dependencies
        .iter()
        .map(|d| (d.ticket.as_str(), d.outcome))
        .collect();
    assert_eq!(
        outcomes,
        [
            ("DEMO-2", DependencyOutcome::Written),
            ("DEMO-3", DependencyOutcome::Done),
            ("DEMO-9", DependencyOutcome::Unknown),
            ("DEMO-1", DependencyOutcome::Itself),
            ("DEMO-4", DependencyOutcome::Cycle),
            ("DEMO-5", DependencyOutcome::Existed),
        ]
    );
    assert_eq!(bench.record().intake, Some(record));
    assert_eq!(bench.blocked_by(), ["DEMO-5", "DEMO-2"]);
    assert_eq!(bench.relations_written(), ["DEMO-2 -> DEMO-1"]);
    let intake = bench.comments().pop().unwrap();
    for said in [
        "DEMO-2, named by `Blocked by DEMO-2.`: relation written",
        "DEMO-4, named by `Blocked by DEMO-4.`: already blocked by this ticket",
        "relation already there",
        "already done",
        "no such ticket",
        "this ticket itself",
    ] {
        assert!(intake.contains(said), "{said}:\n{intake}");
    }
}

/// A dependency the tracker cannot read (OWL-249) is a failed intake run,
/// with a `relations_not_written` warning, and nothing is kept; a verdict
/// kept with its round writes its relations before the round is asked.
#[test]
fn a_dependency_not_read_fails_the_run_and_a_round_writes_its_relations() {
    let bench = Bench::drained();
    bench.add_ticket("DEMO-2", "Todo", &[]);
    let broken = bench.remote.checkout.join("tickets").join("DEMO-7");
    fs::create_dir_all(&broken).unwrap();
    fs::write(broken.join("ticket.md"), "not a ticket\n").unwrap();
    bench.comment_as("maintainer", "Blocked by DEMO-7, then DEMO-2.");
    let question = r#"[{"id":"Q1","category":"scope","context":"The ticket names no language.",
        "text":"Which language should the greeting use?","recommendation":{"why":"The simplest reading of the ticket."}}]"#;

    let unread = bench.reply(
        None,
        Some(&intake_naming("questions", &["DEMO-7"], question)),
    );
    let asks = bench.reply(
        None,
        Some(&intake_naming("questions", &["DEMO-2"], question)),
    );
    let stop = bench
        .intake(vec![unread, asks], gesture("maintainer"))
        .expect_err("questions");
    assert!(matches!(&stop, Stop::NeedsInput { .. }), "{stop}");
    let warnings: Vec<Event> = bench
        .events()
        .into_iter()
        .filter(|e| {
            e.kind == EventKind::Warning && e.data["what"] == json!("relations_not_written")
        })
        .collect();
    assert_eq!(warnings.len(), 1);
    assert!(
        warnings[0].data["reason"]
            .as_str()
            .unwrap()
            .contains("DEMO-7"),
        "{:?}",
        warnings[0].data
    );
    assert_eq!(bench.briefs().len(), 2);
    let kept = bench.record().intake.unwrap();
    assert_eq!(kept.dependencies[0].outcome, DependencyOutcome::Written);
    assert_eq!(bench.blocked_by(), ["DEMO-2"]);
    assert_eq!(bench.stage(), "Needs Input");
}

impl Bench {
    /// A bench for the drain (OWL-250): the project admits with the label
    /// `agent`, by `maintainer` alone; DEMO-1, created by `author`, carries
    /// the label, sits in the ready column `Todo`, and its decider is its
    /// assignee.
    fn draining(author: &str) -> Self {
        Self::build_admitting(
            author,
            "assignee = \"maintainer\"\nlabels = [\"agent\"]\n",
            "",
            ", parked = \"Parked\"",
            "admit = { label = \"agent\" }\nadmitters = [\"maintainer\"]",
        )
    }

    /// Runs `owlshift watch --drain` for `passes` idle passes, with one reply
    /// per run, `between(n)` called after the `n`th but the last; returns
    /// each reported outcome, watch's own lines and the events printed. The
    /// passes a stage step makes due at once (OWL-367) run on uncounted:
    /// [`Self::drain_passes`] counts every pass.
    fn drain(
        &self,
        replies: Vec<Reply>,
        passes: usize,
        mut between: impl FnMut(usize),
    ) -> (Vec<Result<Delivered, Stop>>, String, String) {
        let (mut steps, mut done) = (Steps::default(), 0);
        let ((outcomes, lines), printed) =
            self.with_on_demand(replies, None, Vec::new(), |on_demand, sink| {
                let (mut outcomes, mut lines) = (Vec::new(), Vec::new());
                let watch = Watch {
                    on_demand,
                    interval: POLL_INTERVAL,
                };
                watch.run_drain(
                    sink,
                    &mut lines,
                    &mut |_, outcome, _| outcomes.push(outcome.clone()),
                    &mut |interval| {
                        steps.idle(interval, &mut |_| {
                            done += 1;
                            if done < passes {
                                between(done);
                            }
                            done < passes
                        })
                    },
                );
                (outcomes, String::from_utf8(lines).unwrap())
            });
        (outcomes, lines, printed)
    }

    /// Runs `owlshift watch --drain` for `passes` passes, every pass counted
    /// (OWL-367), `between(n, interval)` called after the `n`th but the last
    /// with the interval watch waits then; returns as [`Self::drain`].
    fn drain_passes(
        &self,
        replies: Vec<Reply>,
        passes: usize,
        mut between: impl FnMut(usize, Duration),
    ) -> (Vec<Result<Delivered, Stop>>, String, String) {
        let mut done = 0;
        let ((outcomes, lines), printed) =
            self.with_on_demand(replies, None, Vec::new(), |on_demand, sink| {
                let (mut outcomes, mut lines) = (Vec::new(), Vec::new());
                let watch = Watch {
                    on_demand,
                    interval: POLL_INTERVAL,
                };
                watch.run_drain(
                    sink,
                    &mut lines,
                    &mut |_, outcome, _| outcomes.push(outcome.clone()),
                    &mut |interval| {
                        done += 1;
                        if done < passes {
                            between(done, interval);
                        }
                        done < passes
                    },
                );
                (outcomes, String::from_utf8(lines).unwrap())
            });
        (outcomes, lines, printed)
    }

    /// Writes `ticket.md` of `id` in the tracker, at `stage`.
    fn other_ticket(&self, id: &str, stage: &str) {
        let dir = self.remote.checkout.join("tickets").join(id);
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("ticket.md"),
            format!("+++\ntitle = \"Other\"\nauthor = \"maintainer\"\nstage = \"{stage}\"\n+++\n"),
        )
        .unwrap();
    }

    /// Adds `line` to DEMO-1's description.
    fn describe(&self, line: &str) {
        let text = fs::read_to_string(self.ticket_file()).unwrap();
        fs::write(self.ticket_file(), format!("{text}{line}\n")).unwrap();
    }

    /// The drain's `decision` events, as `drain` and `reason`.
    fn drain_decisions(&self) -> Vec<(String, Value)> {
        self.events()
            .into_iter()
            .filter(|event| event.kind == EventKind::Decision)
            .filter_map(|event| {
                let drain = event.data.get("drain")?.as_str()?.to_owned();
                Some((
                    drain,
                    event.data.get("reason").cloned().unwrap_or(Value::Null),
                ))
            })
            .collect()
    }
}

/// `owlshift watch --drain` (OWL-250): a pass takes the admitted ticket in,
/// leaving it out of that pass's dispatch; while its blocker is open it
/// waits, told once; once the blocker is done, the next pass dispatches it
/// through `do`'s path, its `dispatch` event naming the drain. Plain
/// `watch` takes nothing in.
#[test]
fn the_drain_takes_a_ticket_in_then_builds_it_once_its_blocker_is_done() {
    let watching = Bench::draining("maintainer");
    let (outcomes, _, _) = watching.watch(Vec::new(), &mut |_| false);
    assert!(outcomes.is_empty());
    assert!(watching.drain_decisions().is_empty(), "watch picks nothing");

    let bench = Bench::draining("maintainer");
    bench.describe("Blocked by DEMO-2.");
    bench.other_ticket("DEMO-2", "Todo");
    let intake = bench.reply(None, Some(&intake_result("done", None, "")));
    let build = bench.reply(Some("Hello"), Some(DONE));
    let (outcomes, lines, _) = bench.drain(vec![intake, build], 4, |pass| {
        if pass == 3 {
            bench.other_ticket("DEMO-2", "Done");
        }
    });
    assert_eq!(outcomes.len(), 2, "{lines}");
    assert!(
        matches!(outcomes[0], Err(Stop::Ready { .. })),
        "{outcomes:?}"
    );
    assert!(outcomes[1].is_ok(), "{outcomes:?}");
    // Told once, though it waited two passes.
    assert_eq!(
        bench.drain_decisions(),
        [
            ("intake".to_owned(), Value::Null),
            ("waits".to_owned(), json!("blocked_by")),
        ]
    );
    assert!(lines.contains("DEMO-1 waits: blocked by DEMO-2"), "{lines}");
    let dispatches: Vec<Value> = bench
        .events()
        .into_iter()
        .filter(|e| e.kind == EventKind::Dispatch)
        .map(|e| e.data["command"].clone())
        .collect();
    assert_eq!(dispatches, [json!("drain")]);
    let roles: Vec<Role> = bench.briefs().iter().map(|b| b.role).collect();
    assert_eq!(roles, [Role::Intake, Role::Build, Role::Verify]);
    assert_eq!(bench.record().intake.unwrap().gesture.account, "maintainer");
}

/// `admit = "ready"` (OWL-322) admits as `{ state = "Todo" }` does: the
/// ticket in the ready column, its gesture by a listed admitter, is taken in
/// and built, with no label on it.
#[test]
fn the_ready_gesture_admits_a_ticket_in_the_ready_column() {
    let bench = Bench::build_admitting(
        "maintainer",
        "assignee = \"maintainer\"\n",
        "",
        ", parked = \"Parked\"",
        "admit = \"ready\"\nadmitters = [\"maintainer\"]",
    );
    let intake = bench.reply(None, Some(&intake_result("done", None, "")));
    let build = bench.reply(Some("Hello"), Some(DONE));
    let (outcomes, lines, printed) = bench.drain(vec![intake, build], 2, |_| {});
    assert!(
        matches!(outcomes[..], [Err(Stop::Ready { .. }), Ok(_)]),
        "{outcomes:?}\n{lines}\n{printed}"
    );
    assert_eq!(bench.record().intake.unwrap().gesture.account, "maintainer");
}

/// Per-ticket routing (OWL-305, architecture section 5): the Build follows
/// intake's verdict, an `effort:` label overriding its effort and a `tier:`
/// label naming no tier left out with a warning; the dispatch's `routing`
/// event says where each part came from. A verdict naming Codex is not
/// followed: the Build runs on Claude Code, on the verdict's tier and effort.
#[test]
fn the_build_follows_the_routing_verdict_under_the_tickets_labels() {
    let cases = [
        (
            json!({ "harness": "claude", "tier": "deep", "effort": "high" }),
            "\"effort:low\", \"tier:huge\"",
            claude(Tier::Deep, Some(Effort::Low)),
            ("followed", "label"),
        ),
        (
            json!({ "harness": "codex", "tier": "fast", "effort": "high" }),
            "",
            claude(Tier::Fast, Some(Effort::High)),
            ("not_followed", "verdict"),
        ),
    ];
    for (verdict, labels, routed, (followed, effort_from)) in cases {
        let bench = Bench::build_admitting(
            "maintainer",
            &format!("assignee = \"maintainer\"\nlabels = [\"agent\", {labels}]\n"),
            "",
            ", parked = \"Parked\"",
            "admit = { label = \"agent\" }\nadmitters = [\"maintainer\"]",
        );
        let mut result: Value = serde_json::from_str(&intake_result("done", None, "")).unwrap();
        result["intake"]["routing"] = verdict;
        let intake = bench.reply(None, Some(&result.to_string()));
        let build = bench.reply(Some("Hello"), Some(DONE));
        let (outcomes, lines, printed) = bench.drain(vec![intake, build], 2, |_| {});
        assert!(
            matches!(outcomes[..], [Err(Stop::Ready { .. }), Ok(_)]),
            "{outcomes:?}\n{lines}\n{printed}"
        );
        assert_eq!(bench.routed(), [routed], "the Build alone is routed");
        let events = bench.events();
        let routing = data_of(&events, EventKind::Decision, "routing");
        assert_eq!(routing.len(), 1, "{routing:?}");
        assert_eq!(
            (
                &routing[0]["routing"],
                &routing[0]["command"],
                &routing[0]["tier_from"],
                &routing[0]["effort_from"],
            ),
            (
                &json!(followed),
                &json!("drain"),
                &json!("verdict"),
                &json!(effort_from)
            )
        );
        let ignored: Vec<Value> = data_of(&events, EventKind::Warning, "label")
            .into_iter()
            .map(|warning| warning["label"].clone())
            .collect();
        let expected: &[&str] = if labels.is_empty() {
            &[]
        } else {
            &["tier:huge"]
        };
        assert_eq!(ignored, expected);
    }
}

/// A gesture by an account `admitters` does not list (OWL-250): one
/// `not_admitted` warning naming it, one line, nothing run; it is read again
/// after the hold, and not warned of again for the same account.
#[test]
fn a_gesture_not_admitted_is_warned_of_once_and_runs_nothing() {
    let bench = Bench::draining("stranger");
    let (outcomes, lines, _) = bench.drain(Vec::new(), 3, |pass| {
        if pass == 2 {
            let later = bench.clock.get().checked_add(SignedDuration::from_mins(11));
            bench.clock.set(later.unwrap());
        }
    });
    assert!(outcomes.is_empty(), "{outcomes:?}");
    let warned: Vec<Value> = bench
        .events()
        .into_iter()
        .filter(|e| e.kind == EventKind::Warning && e.data["what"] == json!("not_admitted"))
        .map(|e| e.data["account"].clone())
        .collect();
    assert_eq!(warned, [json!("stranger")]);
    assert_eq!(
        lines.matches("DEMO-1 is not taken in").count(),
        1,
        "{lines}"
    );
    assert_eq!(
        bench.drain_decisions(),
        [("waits".to_owned(), json!("not_admitted"))]
    );
    assert!(bench.briefs().is_empty());
}

/// A "Blocked by" identifier the tracker does not know holds the ticket,
/// named (OWL-250, OWL-264).
#[test]
fn an_unknown_blocker_holds_the_drained_ticket() {
    let bench = Bench::draining("maintainer");
    bench.describe("Blocked by OWL-1 (not UTF-8 related).");
    let intake = bench.reply(None, Some(&intake_result("done", None, "")));
    let (_, lines, _) = bench.drain(vec![intake], 2, |_| {});
    let waits: Vec<Value> = bench
        .events()
        .into_iter()
        .filter(|e| e.kind == EventKind::Decision && e.data.get("drain") == Some(&json!("waits")))
        .map(|e| e.data["tickets"].clone())
        .collect();
    assert_eq!(waits, [json!(["OWL-1", "UTF-8"])], "{lines}");
    assert!(lines.contains("which the tracker does not know"), "{lines}");
}

/// The `tickets` of the last `waits` event.
fn last_wait_tickets(bench: &Bench) -> Value {
    bench
        .events()
        .into_iter()
        .rfind(|e| e.kind == EventKind::Decision && e.data.get("drain") == Some(&json!("waits")))
        .and_then(|e| e.data.get("tickets").cloned())
        .unwrap_or(Value::Null)
}

/// A ticket ref another command left at Verify after the snapshot holds the
/// slot under the dispatch's lock (OWL-250): nothing is dispatched, and the
/// next pass says why the ticket waits, naming the holder (OWL-291).
#[test]
fn a_slot_taken_since_the_snapshot_holds_the_dispatch() {
    let bench = Bench::draining("maintainer");
    let intake = bench.reply(None, Some(&intake_result("done", None, "")));
    let (outcomes, lines, _) = bench.drain(vec![intake], 3, |pass| {
        if pass == 1 {
            let (env, checkout) = (bench.env.clone(), bench.dirs().checkout());
            let record = bench.record();
            *bench.after_stage_read.borrow_mut() = Some(Box::new(move || {
                let git = Git::with_setup("git", move |command| env.apply(command));
                let verifying =
                    TicketState::restore(Status::Active(Stage::Verify), 1, 0, 0, 0, 0).unwrap();
                let other = TicketRecord {
                    state: PersistedState::from(&verifying),
                    ..record
                };
                let demo_9 = TicketId::new("DEMO-9").unwrap();
                ticket_ref::write(&git, &checkout, &demo_9, &other, None).unwrap();
            }));
        }
    });
    assert_eq!(outcomes.len(), 1, "the intake alone: {outcomes:?}");
    assert!(!kinds(&bench.events()).contains(&EventKind::Dispatch));
    assert_eq!(
        bench.drain_decisions().last(),
        Some(&("waits".to_owned(), json!("slot"))),
        "{lines}"
    );
    assert_eq!(last_wait_tickets(&bench), json!(["DEMO-9"]), "{lines}");
    assert!(
        lines.contains("DEMO-1 waits: the project's slot is held by DEMO-9"),
        "{lines}"
    );
}

/// A ticket ref that cannot be read stops every dispatch (OWL-250), and
/// the waiting ticket names it as the slot's holder (OWL-291).
#[test]
fn an_unread_ticket_ref_holds_the_slot_and_is_named() {
    let bench = Bench::draining("maintainer");
    let intake = bench.reply(None, Some(&intake_result("done", None, "")));
    let (outcomes, lines, _) = bench.drain(vec![intake], 2, |pass| {
        if pass == 1 {
            // A ticket ref that points to a tree, not a commit.
            let checkout = bench.dirs().checkout();
            let tree = bench
                .env
                .run(&checkout, &["rev-parse", "HEAD^{tree}"])
                .unwrap();
            let tree = String::from_utf8(tree).unwrap();
            bench
                .env
                .run(
                    &checkout,
                    &["update-ref", "refs/owlshift/tickets/DEMO-9", tree.trim()],
                )
                .unwrap();
        }
    });
    assert_eq!(outcomes.len(), 1, "the intake alone: {outcomes:?}");
    assert!(!kinds(&bench.events()).contains(&EventKind::Dispatch));
    assert_eq!(last_wait_tickets(&bench), json!(["DEMO-9"]), "{lines}");
    assert!(
        lines.contains("nothing is dispatched while the ticket refs of DEMO-9 cannot be read"),
        "{lines}"
    );
}

/// The open pull request cap (OWL-251): with `max_open_prs = 1`, DEMO-1's
/// delivered pull request is the one open; DEMO-2 is still taken in (intake
/// goes on) but not built, the cap told once with the count, and no
/// `waits` event for it.
#[test]
fn the_open_pr_cap_holds_the_next_build_and_is_told_once() {
    let bench = Bench::build_admitting(
        "maintainer",
        "assignee = \"maintainer\"\nlabels = [\"agent\"]\npriority = \"high\"\n",
        "max_open_prs = 1\n",
        ", parked = \"Parked\"",
        "admit = { label = \"agent\" }\nadmitters = [\"maintainer\"]",
    );
    let dir = bench.remote.checkout.join("tickets").join("DEMO-2");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("ticket.md"),
        "+++\ntitle = \"Second\"\nauthor = \"maintainer\"\nstage = \"Todo\"\n\
         assignee = \"maintainer\"\nlabels = [\"agent\"]\n+++\n\nSomething else.\n",
    )
    .unwrap();
    let replies = vec![
        bench.reply(None, Some(&intake_result("done", None, ""))),
        bench.reply(None, Some(&intake_result("done", None, ""))),
        bench.reply(Some("Hello"), Some(DONE)),
    ];
    let (outcomes, lines, _) = bench.drain(replies, 5, |_| {});
    assert_eq!(outcomes.len(), 3, "{outcomes:?}\n{lines}");
    let roles: Vec<Role> = bench.briefs().iter().map(|b| b.role).collect();
    assert_eq!(
        roles,
        [Role::Intake, Role::Intake, Role::Build, Role::Verify],
        "{lines}"
    );
    assert_eq!(pr_caps(&bench), [json!(["DEMO-2", 1, 1])], "{lines}");
    assert_eq!(
        lines.matches("reached their cap (1 of 1)").count(),
        1,
        "{lines}"
    );
    assert!(!lines.contains("DEMO-2 waits: the open"), "{lines}");
}

/// The `pr_cap` decision events, each as its ticket, count and cap.
fn pr_caps(bench: &Bench) -> Vec<Value> {
    bench
        .events()
        .into_iter()
        .filter(|e| e.kind == EventKind::Decision && e.data.get("drain") == Some(&json!("pr_cap")))
        .map(|e| {
            json!([
                e.ticket.as_ref().map(|t| t.as_str().to_owned()),
                e.data["open_prs"],
                e.data["max_open_prs"]
            ])
        })
        .collect()
}

/// A pull request delivered by another command after the snapshot counts
/// under the dispatch's lock (OWL-313): with `max_open_prs = 1`, DEMO-9's
/// delivery written between the pass's snapshot and DEMO-1's dispatch holds
/// that dispatch, and the held dispatch tells the cap in its own pass. Once
/// DEMO-9's pull request ends, the next pass builds DEMO-1: the held
/// dispatch kept no hold.
#[test]
fn a_delivery_since_the_snapshot_holds_the_dispatch() {
    let bench = Bench::build_admitting(
        "maintainer",
        "assignee = \"maintainer\"\nlabels = [\"agent\"]\npriority = \"high\"\n",
        "max_open_prs = 1\n",
        ", parked = \"Parked\"",
        "admit = { label = \"agent\" }\nadmitters = [\"maintainer\"]",
    );
    bench.other_ticket("DEMO-9", "In Review");
    let demo_9 = TicketId::new("DEMO-9").unwrap();
    let git = {
        let env = bench.env.clone();
        Git::with_setup("git", move |command| env.apply(command))
    };
    let checkout = bench.dirs().checkout();
    let replies = vec![
        bench.reply(None, Some(&intake_result("done", None, ""))),
        bench.reply(Some("Hello"), Some(DONE)),
    ];
    let (outcomes, lines, _) = bench.drain(replies, 3, |pass| match pass {
        1 => {
            // A `do` on DEMO-9 delivered meanwhile: its ticket at Watch,
            // which holds no slot, its pull request followed.
            let (env, checkout) = (bench.env.clone(), checkout.clone());
            let record = bench.record();
            let base = bench.remote_main();
            *bench.after_stage_read.borrow_mut() = Some(Box::new(move || {
                let git = Git::with_setup("git", move |command| env.apply(command));
                let watching =
                    TicketState::restore(Status::Active(Stage::Watch), 1, 0, 0, 0, 0).unwrap();
                let mut delivery =
                    owlshift_contracts::refs::TicketDelivery::new("owlshift/demo-9", "main");
                delivery.pull_request = Some(owlshift_contracts::refs::DeliveredPullRequest {
                    number: 9,
                    url: "https://github.com/demo/project/pull/9".to_owned(),
                });
                delivery.pushed = Some(base.clone());
                delivery.base_at_push = Some(base);
                let other = TicketRecord {
                    state: PersistedState::from(&watching),
                    delivery: Some(delivery),
                    ..record
                };
                let demo_9 = TicketId::new("DEMO-9").unwrap();
                ticket_ref::write(&git, &checkout, &demo_9, &other, None).unwrap();
            }));
        }
        2 => {
            // The held dispatch told the cap in its own pass, and built
            // nothing.
            assert_eq!(pr_caps(&bench), [json!(["DEMO-1", 1, 1])]);
            let roles: Vec<Role> = bench.briefs().iter().map(|b| b.role).collect();
            assert_eq!(roles, [Role::Intake]);
            assert!(!kinds(&bench.events()).contains(&EventKind::Dispatch));
            // Then DEMO-9's pull request is closed.
            let stored = ticket_ref::read(&git, &checkout, &demo_9).unwrap().unwrap();
            let mut record = stored.record;
            record.delivery.as_mut().unwrap().ended = Some(owlshift_contracts::refs::Ended {
                why: owlshift_contracts::refs::EndedWhy::Closed,
                at: Timestamp::from_second(1_790_000_000).unwrap(),
                head: None,
            });
            ticket_ref::write(&git, &checkout, &demo_9, &record, Some(&stored.commit)).unwrap();
        }
        _ => {}
    });
    assert_eq!(outcomes.len(), 2, "{outcomes:?}\n{lines}");
    let roles: Vec<Role> = bench.briefs().iter().map(|b| b.role).collect();
    assert_eq!(roles, [Role::Intake, Role::Build, Role::Verify], "{lines}");
    assert_eq!(pr_caps(&bench), [json!(["DEMO-1", 1, 1])], "{lines}");
    assert_eq!(
        lines.matches("reached their cap (1 of 1)").count(),
        1,
        "{lines}"
    );
}

/// A first intake refused before any ticket ref is made is held as a
/// continue is (OWL-250): the next pass does not run it again.
#[test]
fn a_refused_first_intake_is_held() {
    let bench = Bench::draining("maintainer");
    bench.refuse.set(Some("[owlshift] INTAKE"));
    let intake = bench.reply(None, Some(&intake_result("done", None, "")));
    let (outcomes, _, _) = bench.drain(vec![intake], 2, |_| {});
    assert_eq!(outcomes.len(), 1, "{outcomes:?}");
    assert!(matches!(outcomes[0], Err(Stop::Refused(_))), "{outcomes:?}");
    assert_eq!(bench.briefs().len(), 1);
}

/// A ticket taken in whose admitter is then dropped from `admitters` at the
/// base is refused at its dispatch, warned of, and left out of the next
/// picks, so the next ready ticket is built meanwhile (OWL-250).
#[test]
fn a_dropped_admitter_does_not_hold_up_the_next_ticket() {
    let bench = Bench::build_admitting(
        "maintainer",
        "assignee = \"maintainer\"\nlabels = [\"agent\"]\n",
        "",
        ", parked = \"Parked\"",
        "admit = { label = \"agent\" }\nadmitters = [\"maintainer\", \"ann\"]",
    );
    // DEMO-2, more urgent, was admitted by ann.
    let dir = bench.remote.checkout.join("tickets").join("DEMO-2");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("ticket.md"),
        "+++\ntitle = \"Urgent\"\nauthor = \"ann\"\nstage = \"Todo\"\npriority = \"high\"\n\
         assignee = \"maintainer\"\nlabels = [\"agent\"]\n+++\n\nSomething urgent.\n",
    )
    .unwrap();
    let project_file = fs::read_to_string(bench.remote.checkout.join("owlshift.toml")).unwrap();
    let dropped = project_file.replace(
        "admitters = [\"maintainer\", \"ann\"]",
        "admitters = [\"maintainer\"]",
    );
    assert_ne!(dropped, project_file);
    let intake = || bench.reply(None, Some(&intake_result("done", None, "")));
    let replies = vec![intake(), intake(), bench.reply(Some("Hello"), Some(DONE))];
    let (outcomes, lines, _) = bench.drain(replies, 3, |pass| {
        if pass == 1 {
            bench.merge_into_main(&[("owlshift.toml", &dropped)]);
        }
    });
    assert_eq!(outcomes.len(), 3, "{lines}");
    assert!(outcomes[2].is_ok(), "DEMO-1 is built: {outcomes:?}");
    let warned: Vec<(String, Value)> = bench
        .events()
        .into_iter()
        .filter(|e| e.kind == EventKind::Warning && e.data["what"] == json!("not_admitted"))
        .map(|e| {
            (
                e.ticket.unwrap().as_str().to_owned(),
                e.data["account"].clone(),
            )
        })
        .collect();
    assert_eq!(warned, [("DEMO-2".to_owned(), json!("ann"))]);
    let dispatched: Vec<String> = bench
        .events()
        .into_iter()
        .filter(|e| e.kind == EventKind::Dispatch)
        .map(|e| e.ticket.unwrap().as_str().to_owned())
        .collect();
    assert_eq!(dispatched, ["DEMO-1"]);
}

/// OWL-273: an answered intake round's second intake, cut off by a usage
/// limit, leaves the ticket at Intake, shown waiting, out of the ready
/// column; plain `watch` holds it until the limit resets, then resumes it,
/// with no `continue` typed: intake runs again and the ticket goes back to
/// the ready column.
#[test]
fn watch_resumes_an_intake_cut_off_after_its_answered_round() {
    let bench = Bench::drained();
    let asks = bench.reply(
        None,
        Some(&intake_result(
            "questions",
            None,
            r#"[{"id":"Q1","category":"scope","context":"The ticket names no language.",
                "text":"Which language should the greeting use?","recommendation":{"why":"The simplest reading of the ticket."}}]"#,
        )),
    );
    bench
        .intake(vec![asks], gesture("maintainer"))
        .expect_err("questions");
    bench.answer("English.");
    let reset: Timestamp = "2026-10-03T18:00:00Z".parse().unwrap();
    let mut limited = bench.reply(None, None);
    limited.usage_limit = Some(reset);
    let replies = vec![
        bench.reply(None, Some(&check(&[("Q1", "answered", "English.")]))),
        limited,
        bench.reply(None, Some(&intake_result("done", None, ""))),
    ];
    let intakes = || {
        bench
            .briefs()
            .iter()
            .filter(|brief| brief.role == Role::Intake)
            .count()
    };
    let mut pass = 0;
    let (outcomes, lines, _) = bench.watch(replies, &mut |_| {
        pass += 1;
        match pass {
            // Cut off: at Intake, still shown waiting.
            1 => {
                let state = TicketState::try_from(&bench.record().state).unwrap();
                assert_eq!(state.status(), Status::Active(Stage::Intake));
                assert_eq!(bench.stage(), "Needs Input");
                assert_eq!(intakes(), 2);
            }
            // Paused until the reset: nothing ran. Past it now.
            2 => {
                assert_eq!(intakes(), 2);
                let past = reset.duration_since(bench.clock.get()) + SignedDuration::from_mins(1);
                bench.waited.set(past);
            }
            _ => return false,
        }
        true
    });
    assert_eq!(outcomes.len(), 3, "{outcomes:?}\n{lines}");
    // The answer check is a stage step of its own (OWL-367): watch resumes
    // the intake at the next pass, at once.
    assert!(
        matches!(
            outcomes[0],
            Err(Stop::Stepped {
                stage: Stage::Intake,
                ..
            })
        ),
        "{outcomes:?}"
    );
    assert!(
        matches!(outcomes[1], Err(Stop::UsageLimit { .. })),
        "{outcomes:?}"
    );
    assert!(
        matches!(outcomes[2], Err(Stop::Ready { .. })),
        "{outcomes:?}"
    );
    assert!(
        lines.contains("no ticket is continued before the usage limit resets"),
        "{lines}"
    );
    let roles: Vec<Role> = bench.briefs().iter().map(|b| b.role).collect();
    assert_eq!(
        roles,
        [Role::Intake, Role::AnswerCheck, Role::Intake, Role::Intake]
    );
    let state = TicketState::try_from(&bench.record().state).unwrap();
    assert_eq!(state.status(), Status::Active(Stage::Ready));
    assert_eq!(bench.stage(), "Todo");
    assert_eq!(
        bench.stage_events(EventKind::TrackerWrite),
        ["Needs Input", "Todo"]
    );
    let resumed: Vec<Value> = data_of(&bench.events(), EventKind::Decision, "watch")
        .into_iter()
        .filter(|data| data["stage"] == json!("intake"))
        .collect();
    // Once after the answer check's step, cut off, once after the reset.
    assert_eq!(resumed.len(), 2, "{resumed:?}");
    assert!(
        resumed
            .iter()
            .all(|data| data["watch"] == json!("continue")),
        "{resumed:?}"
    );
}

/// OWL-341: a Codex usage limit gives its reset time in the message only, as
/// wall-clock text. The reset time the Codex adapter reads from "try again at
/// 3:54 AM." stops the run, is the pause watch prints, and holds it: nothing
/// runs in the pass before the reset, and a pass past it resumes the ticket.
/// The fake harness stands for the Codex run, which the bench cannot confine:
/// its reply carries the time the real parser read.
#[test]
fn watch_holds_a_pause_until_the_reset_time_read_from_a_codex_message() {
    let bench = Bench::drained();
    let asks = bench.reply(
        None,
        Some(&intake_result(
            "questions",
            None,
            r#"[{"id":"Q1","category":"scope","context":"The ticket names no language.",
                "text":"Which language should the greeting use?","recommendation":{"why":"The simplest reading of the ticket."}}]"#,
        )),
    );
    bench
        .intake(vec![asks], gesture("maintainer"))
        .expect_err("questions");
    bench.answer("English.");
    // The message is read at the bench's clock, 09:00Z, in UTC: the next
    // 03:54 is the next day's.
    let parsed = codex::reset_time(
        "You've hit your usage limit. … try again at 3:54 AM.",
        bench.clock.get(),
        &TimeZone::UTC,
    );
    assert_eq!(parsed, Some("2026-10-03T03:54:00Z".parse().unwrap()));
    let reset = parsed.unwrap();
    let mut limited = bench.reply(None, None);
    limited.usage_limit = Some(reset);
    let replies = vec![
        bench.reply(None, Some(&check(&[("Q1", "answered", "English.")]))),
        limited,
        bench.reply(None, Some(&intake_result("done", None, ""))),
    ];
    let intakes = || {
        bench
            .briefs()
            .iter()
            .filter(|brief| brief.role == Role::Intake)
            .count()
    };
    let mut pass = 0;
    let (outcomes, lines, _) = bench.watch(replies, &mut |_| {
        pass += 1;
        match pass {
            // Cut off by the limit.
            1 => assert_eq!(intakes(), 2),
            // Paused until the reset: the pass started nothing. Past it now.
            2 => {
                assert_eq!(intakes(), 2);
                assert_eq!(bench.briefs().len(), 3);
                let past = reset.duration_since(bench.clock.get()) + SignedDuration::from_mins(1);
                bench.waited.set(past);
            }
            _ => return false,
        }
        true
    });
    assert_eq!(outcomes.len(), 3, "{outcomes:?}\n{lines}");
    // The answer check is a stage step of its own (OWL-367): watch resumes
    // the intake at the next pass, at once.
    assert!(
        matches!(
            outcomes[0],
            Err(Stop::Stepped {
                stage: Stage::Intake,
                ..
            })
        ),
        "{outcomes:?}"
    );
    assert!(
        matches!(
            outcomes[1],
            Err(Stop::UsageLimit {
                resets_at: Some(at)
            }) if at == reset
        ),
        "{outcomes:?}"
    );
    assert!(
        matches!(outcomes[2], Err(Stop::Ready { .. })),
        "{outcomes:?}"
    );
    assert!(
        lines.contains(&format!(
            "no ticket is continued before the usage limit resets at {reset}"
        )),
        "{lines}"
    );
    let roles: Vec<Role> = bench.briefs().iter().map(|b| b.role).collect();
    assert_eq!(
        roles,
        [Role::Intake, Role::AnswerCheck, Role::Intake, Role::Intake]
    );
}

/// OWL-273: watch leaves a ticket at Intake shown in another stage than
/// needs input, here back in the ready column, which is the drain's; one
/// shown waiting whose resume leaves its ticket ref as it was, its INTAKE
/// comment refused, is held for the ten minutes a continue is.
#[test]
fn a_resume_of_an_intake_is_held_and_one_shown_ready_is_left() {
    let bench = Bench::drained();
    let asks = bench.reply(
        None,
        Some(&intake_result(
            "questions",
            None,
            r#"[{"id":"Q1","category":"scope","context":"The ticket names no language.",
                "text":"Which language should the greeting use?","recommendation":{"why":"The simplest reading of the ticket."}}]"#,
        )),
    );
    bench
        .intake(vec![asks], gesture("maintainer"))
        .expect_err("questions");
    bench.answer("English.");
    let mut limited = bench.reply(None, None);
    limited.usage_limit = Some("2026-10-03T18:00:00Z".parse().unwrap());
    let (outcome, _) = bench.continue_ticket(vec![
        bench.reply(None, Some(&check(&[("Q1", "answered", "English.")]))),
        limited,
    ]);
    assert!(
        matches!(outcome, Err(Stop::UsageLimit { .. })),
        "{outcome:?}"
    );
    let ran = || bench.briefs().len();
    assert_eq!(ran(), 3);

    MarkdownTracker::new(&bench.remote.checkout)
        .set_stage(&ticket(), "Todo")
        .unwrap();
    let (outcomes, lines, _) = bench.watch(Vec::new(), &mut |_| false);
    assert!(outcomes.is_empty(), "{outcomes:?}\n{lines}");
    assert_eq!(ran(), 3);

    MarkdownTracker::new(&bench.remote.checkout)
        .set_stage(&ticket(), "Needs Input")
        .unwrap();
    bench.refuse.set(Some("[owlshift] INTAKE"));
    let done = || bench.reply(None, Some(&intake_result("done", None, "")));
    let mut pass = 0;
    let (outcomes, lines, _) = bench.watch(vec![done(), done()], &mut |_| {
        pass += 1;
        match pass {
            1 => assert_eq!(ran(), 4),
            // Held: nothing ran. Eleven minutes on, it is resumed again.
            2 => {
                assert_eq!(ran(), 4);
                bench
                    .waited
                    .set(bench.waited.get() + SignedDuration::from_mins(11));
            }
            _ => return false,
        }
        true
    });
    assert_eq!(ran(), 5, "{lines}");
    assert_eq!(outcomes.len(), 2, "{outcomes:?}");
    assert!(
        outcomes.iter().all(|o| matches!(o, Err(Stop::Refused(_)))),
        "{outcomes:?}"
    );
    assert!(
        lines.contains("DEMO-1 is at Intake as it was before that resume"),
        "{lines}"
    );
    assert_eq!(bench.stage(), "Needs Input");
}

/// OWL-275: under `watch --drain`, watch's resume of an answered intake
/// round is cut off by a usage limit; a person moves the ticket, left at
/// Intake with a record holding its text, back to the ready column. Nothing
/// runs before the reset; then the drain takes it in, intake running again,
/// and dispatches it to Build, with no `continue` typed.
#[test]
fn the_drain_takes_in_an_intake_moved_back_to_todo_after_its_round() {
    let bench = Bench::draining("maintainer");
    let asks = bench.reply(
        None,
        Some(&intake_result(
            "questions",
            None,
            r#"[{"id":"Q1","category":"scope","context":"The ticket names no language.",
                "text":"Which language should the greeting use?","recommendation":{"why":"The simplest reading of the ticket."}}]"#,
        )),
    );
    bench
        .intake(vec![asks], gesture("maintainer"))
        .expect_err("questions");
    bench.answer("English.");
    let reset: Timestamp = "2026-10-03T18:00:00Z".parse().unwrap();
    let mut limited = bench.reply(None, None);
    limited.usage_limit = Some(reset);
    let replies = vec![
        bench.reply(None, Some(&check(&[("Q1", "answered", "English.")]))),
        limited,
        bench.reply(None, Some(&intake_result("done", None, ""))),
        bench.reply(Some("Hello"), Some(DONE)),
    ];
    let ran = || bench.briefs().len();
    let (outcomes, lines, _) = bench.drain(replies, 4, |pass| match pass {
        // Cut off at Intake, its record holding its text; moved to Todo.
        1 => {
            let state = TicketState::try_from(&bench.record().state).unwrap();
            assert_eq!(state.status(), Status::Active(Stage::Intake));
            assert_eq!(state.round(), 1);
            assert_eq!(ran(), 3);
            MarkdownTracker::new(&bench.remote.checkout)
                .set_stage(&ticket(), "Todo")
                .unwrap();
        }
        // Paused until the reset: nothing ran. Past it now.
        2 => {
            assert_eq!(ran(), 3);
            let past = reset.duration_since(bench.clock.get()) + SignedDuration::from_mins(1);
            bench.waited.set(past);
        }
        _ => {}
    });
    assert_eq!(outcomes.len(), 4, "{outcomes:?}\n{lines}");
    // The answer check is a stage step of its own (OWL-367): watch resumes
    // the intake at the next pass, at once.
    assert!(
        matches!(
            outcomes[0],
            Err(Stop::Stepped {
                stage: Stage::Intake,
                ..
            })
        ),
        "{outcomes:?}"
    );
    assert!(
        matches!(outcomes[1], Err(Stop::UsageLimit { .. })),
        "{outcomes:?}"
    );
    assert!(
        matches!(outcomes[2], Err(Stop::Ready { .. })),
        "{outcomes:?}"
    );
    assert!(outcomes[3].is_ok(), "{outcomes:?}");
    let roles: Vec<Role> = bench.briefs().iter().map(|b| b.role).collect();
    assert_eq!(
        roles,
        [
            Role::Intake,
            Role::AnswerCheck,
            Role::Intake,
            Role::Intake,
            Role::Build,
            Role::Verify
        ]
    );
    // Watch continued the answered round once, then resumed its intake at
    // the next pass, the answer check being a step of its own (OWL-367);
    // the restart after the move to Todo is the drain's.
    let continued: Vec<Value> = data_of(&bench.events(), EventKind::Decision, "watch")
        .into_iter()
        .filter(|data| data["stage"] == json!("intake"))
        .collect();
    assert_eq!(continued.len(), 1, "{continued:?}");
    assert_eq!(
        bench
            .drain_decisions()
            .iter()
            .filter(|(drain, _)| drain == "intake")
            .count(),
        1
    );
    let dispatches: Vec<Value> = bench
        .events()
        .into_iter()
        .filter(|e| e.kind == EventKind::Dispatch)
        .map(|e| e.data["command"].clone())
        .collect();
    assert_eq!(dispatches, [json!("drain")]);
}

/// OWL-275: the drain's intake on a ticket left at Intake keeps its state,
/// so a failed run before the answered round still counts: the first failed
/// run after it parks the ticket at Intake, as it would under `continue`.
#[test]
fn the_drain_keeps_the_failed_runs_of_an_intake_left_at_intake() {
    let bench = Bench::draining("maintainer");
    let failed = || {
        bench.reply(
            None,
            Some(r#"{"format":13,"status":"failed","summary":"Lost."}"#),
        )
    };
    let asks = bench.reply(
        None,
        Some(&intake_result(
            "questions",
            None,
            r#"[{"id":"Q1","category":"scope","context":"The ticket names no language.",
                "text":"Which language should the greeting use?","recommendation":{"why":"The simplest reading of the ticket."}}]"#,
        )),
    );
    bench
        .intake(vec![failed(), asks], gesture("maintainer"))
        .expect_err("questions");
    bench.answer("English.");
    let mut limited = bench.reply(None, None);
    limited.usage_limit = Some("2026-10-03T18:00:00Z".parse().unwrap());
    let (outcome, _) = bench.continue_ticket(vec![
        bench.reply(None, Some(&check(&[("Q1", "answered", "English.")]))),
        limited,
    ]);
    assert!(
        matches!(outcome, Err(Stop::UsageLimit { .. })),
        "{outcome:?}"
    );
    let state = TicketState::try_from(&bench.record().state).unwrap();
    assert_eq!(state.status(), Status::Active(Stage::Intake));
    assert_eq!(state.failed_runs(), 1);
    assert_eq!(bench.briefs().len(), 4);

    MarkdownTracker::new(&bench.remote.checkout)
        .set_stage(&ticket(), "Todo")
        .unwrap();
    let (outcomes, lines, _) = bench.drain(vec![failed()], 1, |_| {});
    assert_eq!(outcomes.len(), 1, "{outcomes:?}\n{lines}");
    assert!(
        matches!(outcomes[0], Err(Stop::Parked { .. })),
        "{outcomes:?}"
    );
    assert_eq!(bench.briefs().len(), 5, "one run, then the park");
    let state = TicketState::try_from(&bench.record().state).unwrap();
    assert!(
        matches!(
            state.status(),
            Status::Parked {
                at: Stage::Intake,
                ..
            }
        ),
        "{:?}",
        state.status()
    );
    assert_eq!(bench.stage(), "Parked");
}

/// The drain's scenarios, S3, S4, S6, S12 and S13, played on this bench
/// (OWL-254, build plan part K).
#[path = "on_demand/drain_scenarios.rs"]
mod drain_scenarios;

#[path = "on_demand/design_scenarios.rs"]
mod design_scenarios;

/// A reversed approval or residue decision, before delivery (OWL-345).
#[path = "on_demand/reversal_scenarios.rs"]
mod reversal_scenarios;

#[path = "on_demand/verify_scenarios.rs"]
mod verify_scenarios;

/// OWL-312's acceptance: `[roles.build] rules` reaches Build's brief after
/// `stack.rules`, and not the resolver's; the `dispatch` event names every
/// source; a named file the base commit lacks refuses the run.
#[test]
fn a_roles_rules_reach_its_brief_and_not_the_resolvers() {
    let skill = "---\nname: frontend\n---\nUse tabs.\n";
    let bench = Bench::new(true);
    let checkout = &bench.remote.checkout;
    let config_path = checkout.join("owlshift.toml");
    let original = fs::read_to_string(&config_path).unwrap();
    let with = |named: &str| format!("{original}[roles.build]\nrules = [\"{named}\"]\n");

    // A file the base commit lacks refuses the run, naming it.
    fs::write(&config_path, with("docs/skills/frontend/SKILL.md")).unwrap();
    bench
        .env
        .run(checkout, &["commit", "--quiet", "-am", "Name a skill"])
        .unwrap();
    bench
        .env
        .run(checkout, &["push", "--quiet", "origin", "main"])
        .unwrap();
    let (outcome, printed) = bench.run(Vec::new(), None);
    let stop = outcome.expect_err("the skill is not in the base commit");
    assert!(
        matches!(&stop, Stop::Refused(reason)
            if reason.contains("docs/skills/frontend/SKILL.md on origin/main does not exist")
                && reason.contains("roles.build.rules")),
        "{stop:?}\n{printed}"
    );

    // Once the base holds it, Build gets it after AGENTS.md and the resolver
    // does not.
    let folder = checkout.join("docs/skills/frontend");
    fs::create_dir_all(&folder).unwrap();
    fs::write(folder.join("SKILL.md"), skill).unwrap();
    bench
        .env
        .run(checkout, &["add", "docs/skills/frontend/SKILL.md"])
        .unwrap();
    bench
        .env
        .run(checkout, &["commit", "--quiet", "-m", "Add the skill"])
        .unwrap();
    bench
        .env
        .run(checkout, &["push", "--quiet", "origin", "main"])
        .unwrap();
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
    let briefs = bench.briefs();
    let roles: Vec<Role> = briefs.iter().map(|b| b.role).collect();
    assert_eq!(roles, [Role::Build, Role::Resolver, Role::Build]);
    for build in [&briefs[0], &briefs[2]] {
        let sources: Vec<&str> = build.rules.iter().map(|r| r.source.as_str()).collect();
        assert_eq!(sources, ["AGENTS.md", "docs/skills/frontend/SKILL.md"]);
        assert_eq!(build.rules[1].text, skill);
    }
    let sources: Vec<&str> = briefs[1].rules.iter().map(|r| r.source.as_str()).collect();
    assert_eq!(sources, ["AGENTS.md"]);
    let dispatch = bench
        .events()
        .into_iter()
        .find(|e| e.kind == EventKind::Dispatch)
        .expect("a dispatch");
    assert_eq!(
        dispatch.data["rules"],
        serde_json::json!(["AGENTS.md", "docs/skills/frontend/SKILL.md"])
    );
}

/// OWL-312: a command reads only the `[roles]` tables of the roles it may
/// brief. A rebase run's brief takes `stack.rules` alone, so a table of the
/// build role naming a file the base lacks does not refuse it, where the
/// same table refuses the Build loop of `do`.
#[test]
fn a_bad_role_table_does_not_refuse_a_rebase() {
    let bench = delivered_with_rules();
    let checkout = &bench.remote.checkout;
    let config_path = checkout.join("owlshift.toml");
    let original = fs::read_to_string(&config_path).unwrap();
    fs::write(
        &config_path,
        format!("{original}[roles.build]\nrules = [\"docs/missing.md\"]\n"),
    )
    .unwrap();
    bench
        .env
        .run(
            checkout,
            &["commit", "--quiet", "-am", "Name a missing skill"],
        )
        .unwrap();
    bench
        .env
        .run(checkout, &["push", "--quiet", "origin", "main"])
        .unwrap();
    let main = bench.merge_into_main(&[("AGENTS.md", AGENTS_MERGED)]);
    bench.github.followed.lock().unwrap().mergeable = "CONFLICTING";
    let (outcome, printed) = bench.continue_during(
        vec![bench.reply(None, Some(REBASED))],
        vec![rebase_in(
            &bench,
            main,
            vec![("AGENTS.md", AGENTS_RESOLVED)],
        )],
    );
    outcome.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
}

/// OWL-344: at delegation level `all` the resolver run is routed to the deep
/// tier at the ticket's effort, none without an `effort:` label; at
/// `discoverable` to the standard tier without an effort. At Intake, which
/// has no verdict yet, the effort is the label's.
#[test]
fn the_resolver_runs_deep_at_the_tickets_effort_at_all() {
    for (level, labels, resolver) in [
        (
            "all",
            &["effort:high"][..],
            claude(Tier::Deep, Some(Effort::High)),
        ),
        (
            "discoverable",
            &["effort:high"][..],
            claude(Tier::Standard, None),
        ),
        ("all", &[][..], claude(Tier::Deep, None)),
    ] {
        let bench = Bench::new(true);
        bench.policy(&format!("resolver_delegation = \"{level}\""));
        bench.add_labels(labels);
        let (delivered, printed) = bench.run(
            vec![
                bench.reply(None, Some(TWO_NAMINGS)),
                bench.reply(None, Some(DECIDED_BOTH)),
                bench.reply(Some("Hello"), Some(DONE)),
            ],
            None,
        );
        delivered.unwrap_or_else(|stop| panic!("{level} {labels:?}: {stop}\n{printed}"));
        let build = claude(Tier::Standard, labels.first().map(|_| Effort::High));
        assert_eq!(
            bench.routed(),
            [build, resolver, build],
            "{level} {labels:?}: the Build, its resolver, the Build"
        );
    }

    let bench = Bench::drained();
    bench.policy("resolver_delegation = \"all\"");
    bench.add_labels(&["effort:high"]);
    let asks = bench.reply(
        None,
        Some(&intake_result(
            "questions",
            None,
            r#"[{"id":"Q1","category":"naming","context":"The repository has no greeting yet.",
                "text":"Which file should hold the greeting?","recommendation":{"why":"The simplest reading of the ticket."}}]"#,
        )),
    );
    let done = bench.reply(None, Some(&intake_result("done", None, "[]")));
    bench
        .intake(
            vec![asks, bench.reply(None, Some(DECIDED_Q1)), done],
            gesture("maintainer"),
        )
        .unwrap();
    assert_eq!(
        bench.routed(),
        [claude(Tier::Deep, Some(Effort::High))],
        "intake's resolver alone is routed"
    );
}

/// OWL-312: the resolver an intake pass runs for the questions intake raises
/// gets `[roles.resolver]` rules, read for that pass beside intake's own.
#[test]
fn an_intake_pass_gives_its_resolver_the_resolver_rules() {
    let bench = Bench::drained();
    let checkout = &bench.remote.checkout;
    let config_path = checkout.join("owlshift.toml");
    let original = fs::read_to_string(&config_path).unwrap();
    fs::write(
        &config_path,
        format!(
            "{original}[roles.resolver]\nrules = [\"docs/resolver.md\"]\n\
             [roles.intake]\nrules = [\"docs/intake.md\"]\n"
        ),
    )
    .unwrap();
    fs::create_dir_all(checkout.join("docs")).unwrap();
    fs::write(
        checkout.join("docs/resolver.md"),
        "Settle from the ticket.\n",
    )
    .unwrap();
    fs::write(checkout.join("docs/intake.md"), "Name dependencies.\n").unwrap();
    bench
        .env
        .run(checkout, &["add", "docs/resolver.md", "docs/intake.md"])
        .unwrap();
    bench
        .env
        .run(checkout, &["commit", "--quiet", "-am", "Add role rules"])
        .unwrap();
    bench
        .env
        .run(checkout, &["push", "--quiet", "origin", "main"])
        .unwrap();
    let asks = bench.reply(
        None,
        Some(&intake_result(
            "questions",
            None,
            r#"[{"id":"Q1","category":"naming","context":"The repository has no greeting yet.",
                "text":"Which file should hold the greeting?","recommendation":{"why":"The simplest reading of the ticket."}}]"#,
        )),
    );
    let done = bench.reply(None, Some(&intake_result("done", None, "[]")));
    let _ = bench.intake(
        vec![asks, bench.reply(None, Some(DECIDED_Q1)), done],
        gesture("maintainer"),
    );
    let briefs = bench.briefs();
    let roles: Vec<Role> = briefs.iter().map(|b| b.role).collect();
    assert_eq!(roles[..2], [Role::Intake, Role::Resolver], "{roles:?}");
    let sources = |brief: &Brief| -> Vec<String> {
        brief.rules.iter().map(|rule| rule.source.clone()).collect()
    };
    assert_eq!(sources(&briefs[0]), ["AGENTS.md", "docs/intake.md"]);
    assert_eq!(sources(&briefs[1]), ["AGENTS.md", "docs/resolver.md"]);
}

/// The bench's gate line in `owlshift.toml`.
const GATE_LINE: &str = "gate = [\"git grep -q Hello -- GREETING.md\"]";

/// OWL-331: the base moves, with a new gate, its script and new rules,
/// between two Builds of one ticket. The resumed Build keeps the gate and
/// rules of the commit its branch was made from, which the code it works on
/// was written for, so the ticket delivers where the new gate would stop it
/// `blocked` (exit 127, no script). Neither the person's own file, edited or
/// pulled, nor the branch's own edit of `owlshift.toml` configures a run.
#[test]
fn a_resumed_build_keeps_the_gate_and_rules_of_its_own_base() {
    let bench = Bench::new(true);
    let old_gate = vec!["git grep -q Hello -- GREETING.md".to_owned()];
    let config_path = bench.remote.checkout.join("owlshift.toml");
    let original = fs::read_to_string(&config_path).unwrap();
    let failing = original.replace(GATE_LINE, "gate = [\"false\"]");
    fs::write(&config_path, &failing).unwrap();
    let old_base = bench.remote_main();
    let mut asking = bench.reply(Some("Hello"), Some(ROUND_1));
    asking
        .files
        .insert(RelativePath::new("owlshift.toml").unwrap(), failing);
    let (asked, printed) = bench.run(vec![asking], None);
    assert!(
        matches!(&asked, Err(Stop::NeedsInput { .. })),
        "{asked:?}\n{printed}"
    );
    assert_eq!(bench.last_of(Role::Build).gate, old_gate);

    let new_config = original.replace(GATE_LINE, "gate = [\"sh gate.sh\"]");
    let new_base = bench.merge_into_main(&[
        ("owlshift.toml", &new_config),
        ("gate.sh", "exit 0\n"),
        ("AGENTS.md", "Write the test first.\n"),
    ]);
    fs::write(&config_path, &new_config).unwrap();
    bench.answer("ok");
    let answered = check(&[
        ("Q1", "answered", "English, the recommendation."),
        ("Q2", "answered", "Hello, reader."),
    ]);
    let (delivered, printed) = bench.continue_ticket(vec![
        bench.reply(None, Some(&answered)),
        bench.reply(Some("Hello again"), Some(DONE)),
    ]);
    delivered.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    let build = bench.last_of(Role::Build);
    assert_eq!(build.gate, old_gate);
    let rules: Vec<&str> = build.rules.iter().map(|rule| rule.text.as_str()).collect();
    assert_eq!(rules, [AGENTS]);
    let dispatched = data_of(&bench.events(), EventKind::Dispatch, "configured_at");
    let last = dispatched.last().unwrap();
    assert_ne!(old_base, new_base);
    assert_eq!(
        (&last["configured_at"], &last["project_file"]),
        (&json!(old_base), &json!("commit"))
    );
}

/// OWL-331: a base whose default branch holds no `owlshift.toml`, as in a
/// project adopting Owlshift, runs the gate of the person's own file.
#[test]
fn a_base_without_a_project_file_takes_the_persons_gate() {
    let bench = Bench::new(true);
    bench.merge_into_main(&[("owlshift.toml", "")]);
    let (delivered, printed) = bench.run(vec![bench.reply(Some("Hello"), Some(DONE))], None);
    delivered.unwrap_or_else(|stop| panic!("{stop}\n{printed}"));
    assert_eq!(
        bench.last_of(Role::Build).gate,
        ["git grep -q Hello -- GREETING.md"]
    );
    let dispatched = data_of(&bench.events(), EventKind::Dispatch, "project_file");
    assert_eq!(dispatched[0]["project_file"], json!("local"));
}

/// OWL-332: an empty gate at the base refuses `do` before any run, with no
/// dispatch recorded and no ticket ref kept, and says how to give one.
#[test]
fn an_empty_gate_refuses_do_before_any_run() {
    let bench = Bench::new(true);
    let config = fs::read_to_string(bench.remote.checkout.join("owlshift.toml")).unwrap();
    bench.merge_into_main(&[("owlshift.toml", &config.replace(GATE_LINE, "gate = []"))]);
    let (refused, printed) = bench.run(Vec::new(), None);
    match &refused {
        Err(Stop::Refused(why)) => assert!(
            why.contains("`stack.gate` is empty in owlshift.toml at origin/main")
                && why.ends_with("merge it to origin/main"),
            "{why}"
        ),
        other => panic!("{other:?}\n{printed}"),
    }
    assert!(bench.briefs().is_empty());
    assert!(data_of(&bench.events(), EventKind::Dispatch, "command").is_empty());
}

/// OWL-332: `continue` on a ticket whose gate is empty is refused before
/// its answer check runs or anything is kept: here the base has no
/// `owlshift.toml`, so the person's file, emptied after the round was
/// asked, gives the gate.
#[test]
fn an_empty_gate_refuses_continue_before_its_answer_check() {
    let bench = Bench::new(true);
    bench.merge_into_main(&[("owlshift.toml", "")]);
    let (asked, printed) = bench.run(vec![bench.reply(None, Some(ROUND_1))], None);
    assert!(
        matches!(&asked, Err(Stop::NeedsInput { .. })),
        "{asked:?}\n{printed}"
    );
    let config_path = bench.remote.checkout.join("owlshift.toml");
    let config = fs::read_to_string(&config_path).unwrap();
    fs::write(&config_path, config.replace(GATE_LINE, "gate = []")).unwrap();
    bench.answer("ok");
    let (runs, kept) = (bench.briefs().len(), bench.ref_commit());

    let (refused, printed) = bench.continue_ticket(Vec::new());
    match &refused {
        Err(Stop::Refused(why)) => assert!(
            why.contains("`stack.gate` is empty in your owlshift.toml, origin/main"),
            "{why}"
        ),
        other => panic!("{other:?}\n{printed}"),
    }
    assert_eq!(bench.briefs().len(), runs);
    assert_eq!(bench.ref_commit(), kept);
}

/// The review loop on a delivered pull request (OWL-307, build plan part J).
#[path = "on_demand/review_scenarios.rs"]
mod review_scenarios;

/// The quality pipeline's scenarios, S8, S9's findings and passes, S17's
/// routing, the variants, approval, `replan` and a review loop, played by
/// `watch --drain` on this bench (OWL-310, build plan "P5", part M).
#[path = "on_demand/pipeline_scenarios.rs"]
mod pipeline_scenarios;
