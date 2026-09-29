//! `owlshift do TICKET`: one ticket turned into a verified pull request, on
//! demand and in the foreground (roadmap P1, build plan OWL-20).
//!
//! [`OnDemand::run`] takes the project's lock, refuses a project a previous
//! run may have tampered with ([`crate::project`]), reads the ticket and
//! brings the dedicated checkout up to date. It checks the forge answers
//! before an hour of Build, then runs the build role through the executor,
//! following the core state machine: a failed run, a red project gate
//! included, gets one more run with the failure in its brief, and a second
//! one parks the ticket. A Build `done` whose gate passed is delivered by
//! the Writer: the gated commit pushed to the ticket's branch, the pull
//! request opened or found, its head checked to be that commit, its check
//! set read, and the delivery report posted on the ticket. Every step is an
//! event ([`crate::events`]).
//!
//! In this version the build stage is the whole pipeline: questions and a
//! blocked run are printed, not posted (P2), the tracker's visible stage is
//! not moved, and the project's rules are not injected into the brief.

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use jiff::Timestamp;
use serde_json::{Value, json};

use owlshift_adapters::forge::github::GitHubForge;
use owlshift_adapters::forge::push::Pushed;
use owlshift_adapters::forge::{Branch, CheckSet, CommitId, ErrorKind, PullRequest, Repo, Verdict};
use owlshift_adapters::harness::claude::Usage;
use owlshift_adapters::tracker::{Author as TrackerAuthor, Comment, Person, Ticket, Tracker};
use owlshift_contracts::brief::{
    Author, Brief, Checkpoint, GateFailure, PermissionLevel, Permissions, Relation, ThreadEntry,
    TicketBrief,
};
use owlshift_contracts::comment::MarkedComment;
use owlshift_contracts::config::{ProjectConfig, TrackerKind};
use owlshift_contracts::event::EventKind;
use owlshift_contracts::format::Format;
use owlshift_contracts::ids::{RelativePath, TicketId};
use owlshift_contracts::result::{self, Decision, Followup, Question, RunResult};
use owlshift_contracts::{Role, Stage, Variant};
use owlshift_core::pipeline::Pipeline;
use owlshift_core::state::{Event, ParkReason, Status, TicketState, Transition};

use crate::agent_env::AgentEnv;
use crate::events::{Data, EventSink, data};
use crate::executor::{
    DEFAULT_GATE_TIMEOUT, Executor, Git, Harness, Outcome, RESULT_PATH, RUN_DIR, RunReport, RunSpec,
};
use crate::project::{self, ProjectDirs};
use crate::writer::{DeliveryReport, Gate, Writer};

/// How long one Build run may take before its process tree is stopped.
pub const DEFAULT_RUN_TIMEOUT: Duration = Duration::from_secs(2 * 60 * 60);

/// The forge hosts the executor's credential check asks git and gh about.
pub const FORGE_HOSTS: &[&str] = &["github.com"];

/// How many times the pull request is read before its head must be the
/// pushed commit: GitHub updates a pull request's head shortly after a push.
const HEAD_READS: u32 = 5;

/// The executor of `owlshift do`: `git` for the runner's own commands,
/// `agent` for the environment of every agent, the credential check on
/// [`FORGE_HOSTS`], [`DEFAULT_RUN_TIMEOUT`] and the default gate deadline.
/// `agent` is where OWL-44's variables declared for the gate will come in.
pub fn executor(git: Git, agent: AgentEnv) -> Executor {
    Executor {
        git,
        agent,
        forge_hosts: FORGE_HOSTS.iter().map(ToString::to_string).collect(),
        timeout: DEFAULT_RUN_TIMEOUT,
        gate_timeout: DEFAULT_GATE_TIMEOUT,
    }
}

/// The ticket's branch: `owlshift/` and its id in lower case, which Linear
/// links to the ticket.
pub fn branch_for(ticket: &TicketId) -> String {
    format!("owlshift/{}", ticket.as_str().to_ascii_lowercase())
}

/// The GitHub repository of an `origin` URL. An http(s) URL that carries a
/// user or a password is refused: the dedicated checkout would copy the
/// credential, and the agent's credential check would refuse every run. The
/// URL itself is never repeated, since it may hold a token.
pub fn check_origin(url: &str) -> Result<Repo, String> {
    let authority = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .map(|rest| rest.split('/').next().unwrap_or_default());
    if authority.is_some_and(|authority| authority.contains('@')) {
        return Err(
            "the URL of the remote `origin` carries a credential: use SSH or a git \
                    credential helper instead, so no token is written in a URL"
                .to_owned(),
        );
    }
    Repo::from_github_remote(url).map_err(|_| {
        "the remote `origin` is not a github.com repository: `owlshift do` delivers to GitHub only"
            .to_owned()
    })
}

/// Refuses a ticket of another team than the project's, before anything is
/// read: on Linear, a ticket id starts with its team's key (`OWL` in
/// `OWL-12`), and `owlshift do LOC-12` in an OWL project would otherwise
/// build another team's ticket and report there. The key is compared
/// without regard to case, as Linear reads identifiers.
pub fn check_team(config: &ProjectConfig, ticket: &TicketId) -> Result<(), String> {
    let (TrackerKind::Linear, Some(team)) = (config.tracker.kind, &config.tracker.team) else {
        return Ok(());
    };
    let key = ticket.as_str().rsplit_once('-').map(|(key, _)| key);
    if key.is_some_and(|key| key.eq_ignore_ascii_case(team)) {
        Ok(())
    } else {
        Err(format!(
            "{ticket} is not a ticket of team {team}, the project's `tracker.team`: \
             `owlshift do` runs this project's tickets only"
        ))
    }
}

/// The core event a run's outcome maps onto (the table of
/// [`owlshift_core::state::Event`]), and the run's result when it left a
/// valid one: a usage limit is an interruption; a failed run or a result
/// `failed` a failed run; a breach a quarantine; `questions` and
/// `premise_false` a question round; `blocked` a block.
pub fn core_event(outcome: &Outcome) -> (Event, Option<&RunResult>) {
    match outcome {
        Outcome::Finished { result, .. } => {
            let event = match result.status {
                result::Status::Done => Event::Completed,
                result::Status::Questions | result::Status::PremiseFalse => Event::Questions,
                result::Status::Blocked => Event::Blocked,
                result::Status::Failed => Event::RunFailed,
            };
            (event, Some(result))
        }
        Outcome::UsageLimit { .. } => (Event::Interrupted, None),
        Outcome::Failed(_) => (Event::RunFailed, None),
        Outcome::Quarantined(_) => (Event::Quarantined, None),
    }
}

/// One `owlshift do`: the adapters, the executor and the project.
pub struct OnDemand<'a> {
    pub executor: &'a Executor,
    pub tracker: &'a dyn Tracker,
    pub forge: &'a GitHubForge,
    pub harness: &'a dyn Harness,
    /// What the dedicated checkout clones and fetches: the `origin` of the
    /// person's checkout.
    pub remote_url: &'a str,
    pub config: &'a ProjectConfig,
    pub dirs: &'a ProjectDirs,
    /// How long to wait between two reads of the pull request's head.
    pub head_wait: Duration,
}

/// A ticket delivered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Delivered {
    pub pull_request: PullRequest,
    /// `false` when the pull request was already open.
    pub opened: bool,
    /// The verdict of its check set, when it could be read.
    pub verdict: Option<Verdict>,
    /// The delivery report's comment id.
    pub comment: String,
}

impl fmt::Display for Delivered {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let pr = &self.pull_request;
        let verdict = match self.verdict {
            Some(Verdict::Green) => "green",
            Some(Verdict::Pending) => "not finished yet",
            Some(Verdict::Red) => "red",
            Some(Verdict::NoChecks) => "none reported yet",
            Some(Verdict::NotOpen) => "the pull request is not open",
            None => "could not be read",
        };
        write!(
            f,
            "Delivered: pull request #{} {} ({}), checks {verdict}; the delivery report is on \
             the ticket. Owlshift never merges: review and merge it.",
            pr.number,
            pr.url,
            if self.opened { "opened" } else { "updated" },
        )
    }
}

/// Why `owlshift do` stopped short of a delivery.
#[derive(Clone, Debug, PartialEq)]
pub enum Stop {
    /// Another `owlshift do` holds the project.
    Busy(PathBuf),
    /// Nothing ran, or nothing more could: the reason and its fix.
    Refused(String),
    /// A previous run's isolation check did not pass, or never ran.
    Unverified { marker: PathBuf, text: String },
    /// The run asked questions, or found the ticket's premise false.
    NeedsInput {
        status: result::Status,
        summary: String,
        questions: Vec<Question>,
    },
    /// The core machine parked the ticket.
    Parked { reason: ParkReason, detail: String },
    /// The harness reached its usage limit.
    UsageLimit { resets_at: Option<Timestamp> },
    /// The run was done and its gate passed, but its delivery failed.
    Delivery(String),
}

impl fmt::Display for Stop {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Busy(root) => write!(
                f,
                "Another `owlshift do` is working this project ({}); run again once it ends.",
                root.display()
            ),
            Self::Refused(reason) => write!(f, "Not run: {reason}"),
            Self::Unverified { marker, text } => write!(
                f,
                "Refused: a previous run's isolation check did not pass, so the dedicated \
                 checkout may hold changes it made outside its worktree.\n{}\nInspect the \
                 project's folder, then delete its `checkout` and `worktrees` folders to start \
                 from a fresh clone, or delete {} to keep them.",
                text.trim_end(),
                marker.display()
            ),
            Self::NeedsInput {
                status,
                summary,
                questions,
            } => {
                if *status == result::Status::PremiseFalse {
                    writeln!(f, "Stopped: the run found the ticket's premise false.")?;
                } else {
                    writeln!(f, "Stopped: the run needs the decider's answers.")?;
                }
                writeln!(f, "{summary}")?;
                for question in questions {
                    writeln!(
                        f,
                        "\n{} ({}) {}\n{}",
                        question.id, question.category, question.text, question.context
                    )?;
                    if !question.options.is_empty() {
                        writeln!(f, "Options: {}", question.options.join(" / "))?;
                    }
                    if let Some(recommendation) = &question.recommendation {
                        writeln!(f, "Recommendation: {recommendation}")?;
                    }
                }
                write!(
                    f,
                    "\nNothing was posted on the ticket: answer there, then run `owlshift do` \
                     again; the run resumes from its plan."
                )
            }
            Self::Parked { reason, detail } => {
                write!(f, "Parked: {}: {detail}", describe(*reason))?;
                if *reason == ParkReason::IsolationBreach {
                    f.write_str(
                        "\nThe project is refused until a person looks: see the `unverified` \
                         file in its folder.",
                    )?;
                }
                Ok(())
            }
            Self::UsageLimit { resets_at } => match resets_at {
                Some(at) => write!(
                    f,
                    "Stopped: the harness reached its usage limit, which resets at {at}; run \
                     again then."
                ),
                None => f.write_str(
                    "Stopped: the harness reached its usage limit; run again once it resets.",
                ),
            },
            Self::Delivery(reason) => write!(
                f,
                "The run is done and its gate passed, but the delivery failed: {reason}. The \
                 worktree keeps the work; run again to retry the delivery."
            ),
        }
    }
}

fn describe(reason: ParkReason) -> &'static str {
    match reason {
        ParkReason::FailedRuns => "a second run failed",
        ParkReason::Reasks => "the answers stayed incomplete",
        ParkReason::Blocked => "the run is blocked",
        ParkReason::IsolationBreach => "the run broke isolation and is quarantined",
    }
}

fn refused(what: &str, error: impl fmt::Display) -> Stop {
    Stop::Refused(format!("{what}: {error}"))
}

/// What the runs of one `owlshift do` gathered for the delivery report.
#[derive(Default)]
struct Gathered {
    gate_failure: Option<GateFailure>,
    decisions: Vec<Decision>,
    followups: Vec<Followup>,
}

impl OnDemand<'_> {
    /// Runs `ticket` to a delivered pull request; see the module
    /// documentation.
    pub fn run(&self, ticket: &TicketId, sink: &mut EventSink<'_>) -> Result<Delivered, Stop> {
        check_team(self.config, ticket).map_err(Stop::Refused)?;
        // The lock before the marker: every run writes the marker when it
        // starts, so while another `do` works the project the marker only
        // says a run is in flight. Once the lock is ours, a marker means a
        // run that ended, or was cut off, before its check passed.
        let _lock = match self.dirs.lock() {
            Ok(Some(lock)) => lock,
            Ok(None) => return Err(Stop::Busy(self.dirs.root().to_owned())),
            Err(error) => return Err(refused("the project's lock", error)),
        };
        match self.dirs.unverified() {
            Ok(None) => {}
            Ok(Some(text)) => {
                return Err(Stop::Unverified {
                    marker: self.dirs.unverified_file(),
                    text,
                });
            }
            Err(error) => return Err(refused("the project's marker", error)),
        }
        let found = self
            .tracker
            .ticket(ticket)
            .map_err(|e| refused(&format!("reading {ticket}"), e))?;
        let decider = found.assignee.clone().ok_or_else(|| {
            Stop::Refused(format!(
                "{ticket} has no assignee: assign it to the person who decides its questions"
            ))
        })?;

        let git = &self.executor.git;
        let base =
            project::sync_checkout(git, self.dirs, self.remote_url).map_err(Stop::Refused)?;
        let checkout = self.dirs.checkout();
        let worktree = self.dirs.worktree(ticket);
        // A worktree kept from an earlier `do` is trusted only while its
        // `.git` still links it to the checkout.
        if fs::symlink_metadata(&worktree).is_ok()
            && let Err(reason) = project::check_worktree_link(&checkout, &worktree)
        {
            let text = format!("The worktree of {ticket} was changed between runs: {reason}\n");
            self.dirs
                .mark_unverified(&text)
                .map_err(|e| refused("the project's marker", e))?;
            return Err(Stop::Unverified {
                marker: self.dirs.unverified_file(),
                text,
            });
        }
        let branch = branch_for(ticket);
        let head = Branch::new(&branch).map_err(|e| refused("the ticket's branch", e))?;
        let base_branch = Branch::new(&base.branch).map_err(|e| refused("the base branch", e))?;
        // The forge is asked before an hour of Build: a refused token or an
        // unknown repository shows now.
        let open = self
            .forge
            .find_open_pull_request(&head, &base_branch)
            .map_err(|e| refused(&format!("GitHub ({})", self.forge.repo()), e))?;
        sink.emit(
            ticket,
            None,
            EventKind::Dispatch,
            data([
                ("title", json!(found.title)),
                ("branch", json!(branch)),
                ("base", json!(base.remote_ref)),
                ("worktree", path(&worktree)),
                ("checkout", path(&checkout)),
                ("pull_request", json!(open.map(|pr| pr.number))),
            ]),
        );

        // The build stage alone, in the core machine: its breaker gives a
        // failed run one more run before it parks the ticket.
        let pipeline = Pipeline::new(Variant::Trivial);
        let ready = TicketState::restore(Status::Active(Stage::Ready), 0, 0, 0)
            .map_err(|e| Stop::Refused(format!("the core state machine: {e:?}")))?;
        let mut state = match ready.apply(pipeline, Event::Dispatched) {
            Ok(Transition::To(state)) => state,
            other => return Err(Stop::Refused(format!("the core state machine: {other:?}"))),
        };
        let mut gathered = Gathered::default();
        let mut attempt = 0u32;
        let report = loop {
            attempt += 1;
            let (run, run_dir) = new_run_dir(&self.dirs.runs(ticket))
                .map_err(|e| refused("the run directory", e))?;
            let comments = self
                .tracker
                .comments(ticket)
                .map_err(|e| refused(&format!("reading the comments of {ticket}"), e))?;
            let brief = self.brief(&found, &decider, &comments, &worktree, &gathered);
            sink.emit(
                ticket,
                Some(&run),
                EventKind::RunStarted,
                data([
                    ("role", json!("build")),
                    ("attempt", json!(attempt)),
                    ("run_dir", path(&run_dir)),
                    ("resumes_plan", json!(brief.checkpoint.is_some())),
                    ("gate_failure", json!(brief.gate_failure.is_some())),
                ]),
            );
            let marker = format!(
                "Run {run} of {ticket} started at {}; its isolation check has not passed.\n",
                Timestamp::now()
            );
            self.dirs
                .mark_unverified(&marker)
                .map_err(|e| refused("the project's marker", e))?;
            let spec = RunSpec {
                main: &checkout,
                worktree: &worktree,
                branch: &branch,
                base: &base.remote_ref,
                run_dir: &run_dir,
                brief: &brief,
            };
            let report = match self.executor.run(&spec, self.harness) {
                Ok(report) => report,
                Err(error) => {
                    // Nothing was spawned: nothing to verify.
                    let _ = self.dirs.clear_unverified();
                    sink.emit(
                        ticket,
                        Some(&run),
                        EventKind::RunEnded,
                        data([
                            ("outcome", json!("refused")),
                            ("reason", json!(error.to_string())),
                        ]),
                    );
                    return Err(refused("the run could not start", error));
                }
            };
            // The executor's check does not see the worktree's own `.git`:
            // a link the run redirected is a breach too, found before the
            // runner trusts anything git says in the worktree.
            let link = project::check_worktree_link(&checkout, &worktree).err();
            let mut breaches: Vec<String> = match &report.outcome {
                Outcome::Quarantined(violations) => {
                    violations.iter().map(ToString::to_string).collect()
                }
                _ => Vec::new(),
            };
            breaches.extend(link.clone());
            let marked = if breaches.is_empty() {
                self.dirs.clear_unverified()
            } else {
                let lines: Vec<String> = breaches.iter().map(|b| format!("- {b}")).collect();
                self.dirs.mark_unverified(&format!(
                    "Run {run} of {ticket} broke isolation:\n{}\n",
                    lines.join("\n")
                ))
            };
            marked.map_err(|e| refused("the project's marker", e))?;
            let mut ended = run_ended(&report);
            if let Some(link) = &link {
                ended.insert("outcome".to_owned(), json!("quarantined"));
                ended.insert("reason".to_owned(), json!(breaches.join("; ")));
                ended.insert("link".to_owned(), json!(link));
            }
            sink.emit(ticket, Some(&run), EventKind::RunEnded, ended);
            if let Some(usage) = &report.usage {
                sink.emit(ticket, Some(&run), EventKind::Usage, usage_data(usage));
            }
            if let Some(gate) = &report.gate {
                gathered.gate_failure = gate.failure.clone();
            }
            let (event, result) = if breaches.is_empty() {
                core_event(&report.outcome)
            } else {
                (Event::Quarantined, None)
            };
            if let Some(result) = result {
                gather(&mut gathered.decisions, &result.decisions);
                gather(&mut gathered.followups, &result.followups);
            }
            match state.apply(pipeline, event) {
                Ok(Transition::To(next)) => state = next,
                Ok(Transition::Parked { reason, .. }) => {
                    let detail = if breaches.is_empty() {
                        park_detail(&report.outcome)
                    } else {
                        breaches.join("; ")
                    };
                    sink.emit(
                        ticket,
                        Some(&run),
                        EventKind::Decision,
                        data([
                            ("parked", json!(format!("{reason:?}"))),
                            ("detail", json!(detail)),
                        ]),
                    );
                    return Err(Stop::Parked { reason, detail });
                }
                other => return Err(Stop::Refused(format!("the core state machine: {other:?}"))),
            }
            match state.status() {
                Status::Active(Stage::Build) => {
                    if let Outcome::UsageLimit { resets_at } = report.outcome {
                        return Err(Stop::UsageLimit { resets_at });
                    }
                }
                Status::NeedsInput { .. } => {
                    let result = result.expect("a question round comes from a result");
                    sink.emit(
                        ticket,
                        Some(&run),
                        EventKind::Gate,
                        data([
                            ("opened", json!("questions")),
                            ("round", json!(state.round())),
                            ("status", status_name(result.status)),
                            ("questions", json!(result.questions.len())),
                        ]),
                    );
                    return Err(Stop::NeedsInput {
                        status: result.status,
                        summary: result.summary.clone(),
                        questions: result.questions.clone(),
                    });
                }
                // Build completed: in this version, delivery follows.
                _ => break report,
            }
        };
        self.deliver(ticket, &found, &head, &base_branch, report, gathered, sink)
    }

    /// The build brief of one run.
    fn brief(
        &self,
        ticket: &Ticket,
        decider: &Person,
        comments: &[Comment],
        worktree: &Path,
        gathered: &Gathered,
    ) -> Brief {
        let thread = comments
            .iter()
            .map(|comment| ThreadEntry::Comment {
                at: comment.created_at,
                author: comment_author(comment, decider),
                body: comment.body.clone(),
            })
            .collect();
        Brief {
            format: Format,
            role: Role::Build,
            project: self.forge.repo().to_string(),
            ticket: TicketBrief {
                id: ticket.id.clone(),
                title: ticket.title.clone(),
                url: None,
                labels: ticket.labels.clone(),
                // The adapters do not read who wrote a ticket yet: its text
                // is a request, not instructions.
                author: Author {
                    name: "unknown".to_owned(),
                    relation: Relation::Other,
                },
                description: ticket.description.clone(),
            },
            decider: decider.name.clone(),
            thread,
            checkpoint: checkpoint(worktree),
            zones: Vec::new(),
            resources: Vec::new(),
            rules: Vec::new(),
            permissions: Permissions {
                level: PermissionLevel::WriteWorktree,
                network: false,
                browser: false,
            },
            gate: self.config.stack.gate.clone(),
            gate_failure: gathered.gate_failure.clone(),
            result_path: RelativePath::new(RESULT_PATH).expect("RESULT_PATH is a relative path"),
        }
    }

    /// Delivers a Build `done` whose gate passed. The push leaves from the
    /// dedicated checkout, never from the worktree (`Writer::push_branch`).
    #[allow(clippy::too_many_arguments)]
    fn deliver(
        &self,
        ticket: &TicketId,
        found: &Ticket,
        head: &Branch,
        base: &Branch,
        report: RunReport,
        gathered: Gathered,
        sink: &mut EventSink<'_>,
    ) -> Result<Delivered, Stop> {
        let (Outcome::Finished { result, .. }, Some(gate)) = (&report.outcome, &report.gate) else {
            return Err(Stop::Delivery(
                "the run did not report a passing gate".to_owned(),
            ));
        };
        let commit = gate
            .commit
            .as_deref()
            .filter(|_| gate.passed())
            .and_then(|commit| CommitId::new(commit).ok())
            .ok_or_else(|| Stop::Delivery("the gate's commit is unknown".to_owned()))?;
        let writer = Writer::new(self.tracker);
        let delivery =
            |what: &str, error: &dyn fmt::Display| Stop::Delivery(format!("{what}: {error}"));

        let checkout = self.dirs.checkout();
        let pushed = writer
            .push_branch(&self.executor.git, &checkout, "origin", &commit, head)
            .map_err(|e| delivery("pushing the branch", &e))?;
        sink.emit(
            ticket,
            None,
            EventKind::TrackerWrite,
            data([
                ("target", json!("forge")),
                ("action", json!("push")),
                ("branch", json!(head.as_str())),
                ("commit", json!(commit.as_str())),
                ("pushed", json!(pushed_name(pushed))),
            ]),
        );

        let (title, body) = match &result.pr {
            Some(pr) => (pr.title.clone(), pr.body.clone()),
            None => (found.title.clone(), result.summary.clone()),
        };
        let opened = writer
            .open_pull_request(self.forge, head, base, &title, &body)
            .map_err(|e| delivery("opening the pull request", &e))?;
        sink.emit(
            ticket,
            None,
            EventKind::TrackerWrite,
            data([
                ("target", json!("forge")),
                ("action", json!("pull_request")),
                ("number", json!(opened.pull_request.number)),
                ("url", json!(opened.pull_request.url)),
                ("opened", json!(opened.opened)),
            ]),
        );
        let pull_request = self.head_at(opened.pull_request, head, base, &commit)?;
        let checks = self.forge.checks(pull_request.number, &commit);
        if let Err(error) = &checks
            && error.kind == ErrorKind::HeadMoved
        {
            return Err(delivery("reading the checks", error));
        }
        let verdict = checks.as_ref().ok().map(CheckSet::verdict);
        let delivery_report = DeliveryReport {
            ticket: ticket.clone(),
            summary: result.summary.clone(),
            pull_request: pull_request.clone(),
            checks,
            gate: Gate::Passed(gate.commands.clone()),
            decisions: gathered.decisions,
            followups: gathered.followups,
        };
        let comment = writer
            .post_delivery_report(&delivery_report)
            .map_err(|e| delivery("posting the delivery report", &e))?;
        sink.emit(
            ticket,
            None,
            EventKind::TrackerWrite,
            data([
                ("target", json!("tracker")),
                ("action", json!("comment")),
                ("kind", json!("DELIVERY")),
                ("comment", json!(comment.id)),
            ]),
        );
        Ok(Delivered {
            pull_request,
            opened: opened.opened,
            verdict,
            comment: comment.id,
        })
    }

    /// The pull request once its head is `commit`: GitHub moves it shortly
    /// after a push, so it is read again, [`HEAD_READS`] times at most.
    /// Still at another commit, someone else moved the branch after the
    /// push: the report would vouch for a commit the gate never ran on.
    fn head_at(
        &self,
        mut pull_request: PullRequest,
        head: &Branch,
        base: &Branch,
        commit: &CommitId,
    ) -> Result<PullRequest, Stop> {
        for _ in 1..HEAD_READS {
            if pull_request.head == *commit {
                return Ok(pull_request);
            }
            thread::sleep(self.head_wait);
            pull_request = self
                .forge
                .find_open_pull_request(head, base)
                .map_err(|e| Stop::Delivery(format!("reading the pull request: {e}")))?
                .ok_or_else(|| {
                    Stop::Delivery(format!(
                        "pull request #{} is no longer open",
                        pull_request.number
                    ))
                })?;
        }
        if pull_request.head == *commit {
            Ok(pull_request)
        } else {
            Err(Stop::Delivery(format!(
                "pull request #{} is at {}, not at the pushed commit {commit}: the branch moved \
                 after the push, so no report was posted",
                pull_request.number, pull_request.head
            )))
        }
    }
}

/// A comment's author as the brief shows it: the runner's own marked
/// comments are Owlshift's, whoever posted them (the key's account posts
/// them on Linear); the decider is the assignee's account; anyone else is
/// quoted as data. A marker only ever demotes: the decider's text that
/// looks like one reads as data.
fn comment_author(comment: &Comment, decider: &Person) -> Author {
    let (name, account) = match &comment.author {
        TrackerAuthor::Account(person) => (person.name.clone(), Some(person.id.as_str())),
        TrackerAuthor::Other { name } => (name.clone(), None),
    };
    let relation = if matches!(MarkedComment::parse(&comment.body), Ok(Some(_))) {
        Relation::Owlshift
    } else if account == Some(decider.id.as_str()) {
        Relation::Decider
    } else {
        Relation::Other
    };
    Author { name, relation }
}

/// The plan and ledger a previous run left in the worktree, so the next run
/// resumes from them: both must be plain files.
fn checkpoint(worktree: &Path) -> Option<Checkpoint> {
    let plan = format!("{RUN_DIR}/plan.md");
    let ledger = format!("{RUN_DIR}/ledger.json");
    let is_file = |relative: &str| {
        fs::symlink_metadata(worktree.join(relative)).is_ok_and(|meta| meta.file_type().is_file())
    };
    (is_file(&plan) && is_file(&ledger)).then(|| Checkpoint {
        plan: RelativePath::new(plan).ok(),
        ledger: RelativePath::new(ledger).ok(),
    })
}

/// A new run directory under `runs`, named by the time in UTC, with `-2`,
/// `-3`… when that name is taken.
fn new_run_dir(runs: &Path) -> io::Result<(String, PathBuf)> {
    fs::create_dir_all(runs)?;
    let stamp = Timestamp::now().strftime("%Y%m%dT%H%M%SZ").to_string();
    for n in 1u32.. {
        let id = if n == 1 {
            stamp.clone()
        } else {
            format!("{stamp}-{n}")
        };
        let dir = runs.join(&id);
        match fs::create_dir(&dir) {
            Ok(()) => return Ok((id, dir)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    unreachable!("some run directory name is free")
}

/// Adds the items not already in `into`.
fn gather<T: Clone + PartialEq>(into: &mut Vec<T>, items: &[T]) {
    for item in items {
        if !into.contains(item) {
            into.push(item.clone());
        }
    }
}

/// Why a parked run failed, in one line.
fn park_detail(outcome: &Outcome) -> String {
    match outcome {
        Outcome::Quarantined(violations) => {
            let lines: Vec<String> = violations.iter().map(ToString::to_string).collect();
            lines.join("; ")
        }
        Outcome::Failed(failure) => failure.to_string(),
        Outcome::Finished { result, .. } => result.summary.clone(),
        Outcome::UsageLimit { .. } => "a usage limit".to_owned(),
    }
}

/// The `run_ended` event's details.
fn run_ended(report: &RunReport) -> Data {
    let (outcome, reason, status) = match &report.outcome {
        Outcome::Finished { result, .. } => ("finished", None, Some(status_name(result.status))),
        Outcome::UsageLimit { resets_at } => (
            "usage_limit",
            resets_at.map(|at| format!("resets at {at}")),
            None,
        ),
        Outcome::Failed(failure) => ("failed", Some(failure.to_string()), None),
        Outcome::Quarantined(violations) => {
            let lines: Vec<String> = violations.iter().map(ToString::to_string).collect();
            ("quarantined", Some(lines.join("; ")), None)
        }
    };
    let mut details = data([("outcome", json!(outcome))]);
    if let Some(status) = status {
        details.insert("status".to_owned(), status);
    }
    if let Outcome::Finished { result, .. } = &report.outcome {
        details.insert("summary".to_owned(), json!(result.summary));
    }
    if let Some(reason) = reason {
        details.insert("reason".to_owned(), json!(reason));
    }
    if let Some(gate) = &report.gate {
        let verdict = if gate.passed() { "passed" } else { "failed" };
        details.insert("gate".to_owned(), json!(verdict));
        details.insert("gate_commit".to_owned(), json!(gate.commit));
    }
    details.insert("exit_code".to_owned(), json!(report.exit_code));
    details.insert(
        "elapsed_ms".to_owned(),
        json!(u64::try_from(report.elapsed.as_millis()).unwrap_or(u64::MAX)),
    );
    if let Some(model) = &report.model {
        details.insert("model".to_owned(), json!(model));
    }
    if let Some(version) = &report.harness_version {
        details.insert("harness_version".to_owned(), json!(version));
    }
    if let Some(error) = &report.log_error {
        details.insert("log_error".to_owned(), json!(error));
    }
    details
}

/// The `usage` event's details: tokens, turns, duration and the CLI's cost
/// estimate, which measures consumption on a subscription, not a bill.
fn usage_data(usage: &Usage) -> Data {
    data([
        ("input_tokens", json!(usage.input_tokens)),
        ("output_tokens", json!(usage.output_tokens)),
        (
            "cache_read_input_tokens",
            json!(usage.cache_read_input_tokens),
        ),
        (
            "cache_creation_input_tokens",
            json!(usage.cache_creation_input_tokens),
        ),
        ("cost_usd", json!(usage.cost_usd)),
        ("num_turns", json!(usage.num_turns)),
        ("duration_ms", json!(usage.duration_ms)),
        ("models", json!(usage.models.keys().collect::<Vec<_>>())),
    ])
}

fn status_name(status: result::Status) -> Value {
    serde_json::to_value(status).unwrap_or(Value::Null)
}

fn pushed_name(pushed: Pushed) -> &'static str {
    match pushed {
        Pushed::Created => "created",
        Pushed::FastForward => "fast_forward",
        Pushed::UpToDate => "up_to_date",
    }
}

fn path(path: &Path) -> Value {
    json!(path.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_origin_must_be_github_without_a_credential() {
        let repo = check_origin("git@github.com:pitchopp/owlshift.git").unwrap();
        assert_eq!(repo.to_string(), "pitchopp/owlshift");
        assert!(check_origin("https://github.com/pitchopp/owlshift").is_ok());
        for (url, says) in [
            (
                "https://ghp_SECRET@github.com/o/r.git",
                "carries a credential",
            ),
            (
                "https://user:pass@github.com/o/r.git",
                "carries a credential",
            ),
            ("https://gitlab.com/o/r.git", "not a github.com repository"),
            ("/srv/git/r.git", "not a github.com repository"),
        ] {
            let error = check_origin(url).unwrap_err();
            assert!(error.contains(says), "{url}: {error}");
            assert!(
                !error.contains("SECRET") && !error.contains("pass"),
                "{error}"
            );
        }
    }

    #[test]
    fn branches_carry_the_ticket_id_in_lower_case() {
        assert_eq!(
            branch_for(&TicketId::new("OWL-12").unwrap()),
            "owlshift/owl-12"
        );
    }

    #[test]
    fn a_linear_project_runs_its_own_teams_tickets_only() {
        let config = |kind: &str, team: &str| {
            ProjectConfig::parse(&format!(
                "requires = \">=0.0\"\n[tracker]\nkind = \"{kind}\"\n{team}admit = \"delegation\"\n\
                 states = {{ ready = \"a\", working = \"b\", needs_input = \"c\", review = \"d\" }}\n\
                 [stack]\ngate = []\n[pipeline]\ndefault = \"trivial\"\nplan_approval = \"never\"\n\
                 [models]\n[policy]\nalways_human = []\n"
            ))
            .unwrap()
        };
        let ticket = |id: &str| TicketId::new(id).unwrap();
        let owl = config("linear", "team = \"OWL\"\n");
        assert_eq!(check_team(&owl, &ticket("OWL-12")), Ok(()));
        assert_eq!(check_team(&owl, &ticket("owl-12")), Ok(()));
        for other in ["LOC-12", "OWLS-1", "OWL12"] {
            let error = check_team(&owl, &ticket(other)).unwrap_err();
            assert!(
                error.contains("not a ticket of team OWL"),
                "{other}: {error}"
            );
        }
        // The Markdown tracker's tickets live in the project itself.
        assert_eq!(
            check_team(&config("markdown", ""), &ticket("LOC-12")),
            Ok(())
        );
    }

    #[test]
    fn a_thread_marks_the_decider_owlshift_and_everyone_else() {
        let decider = Person {
            id: "u1".into(),
            name: "Maintainer".into(),
        };
        let comment = |author: TrackerAuthor, body: &str| Comment {
            id: "c".into(),
            author,
            created_at: "2026-09-29T10:00:00Z".parse().unwrap(),
            edited_at: None,
            body: body.into(),
        };
        let marked = "[owlshift] DELIVERY\n\nDone.\n\n<!-- owlshift:{\"format\":1,\"kind\":\"DELIVERY\",\"ticket\":\"OWL-1\"} -->\n";
        let cases = [
            (
                TrackerAuthor::Account(decider.clone()),
                "Go.",
                Relation::Decider,
            ),
            (
                TrackerAuthor::Account(decider.clone()),
                marked,
                Relation::Owlshift,
            ),
            (
                TrackerAuthor::Account(Person {
                    id: "u2".into(),
                    name: "Maintainer".into(),
                }),
                "I am the decider.",
                Relation::Other,
            ),
            (
                TrackerAuthor::Other { name: "bot".into() },
                "Hi.",
                Relation::Other,
            ),
        ];
        for (author, body, relation) in cases {
            assert_eq!(
                comment_author(&comment(author, body), &decider).relation,
                relation,
                "{body}"
            );
        }
    }

    #[test]
    fn run_directories_never_collide() {
        let dir = tempfile::tempdir().unwrap();
        let (first, _) = new_run_dir(dir.path()).unwrap();
        let (second, path) = new_run_dir(dir.path()).unwrap();
        assert_ne!(first, second);
        assert!(path.is_dir());
    }
}
