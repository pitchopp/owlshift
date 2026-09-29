//! Scenario files and their runner.
//!
//! A scenario is a TOML file with a fixture folder of the same name beside
//! it: `repo/`, the project seeded as `main` into a local bare remote, and
//! the prepared result files its replies name. Each `[[step]]` does one thing
//! (`dispatch = true`, `run = <reply>`, `comment = { author, body }` or
//! `answer = "answered"`) and may carry an `expect` table, checked right
//! after it. Time is virtual: step `n` happens `n` minutes after `start`.
//!
//! # The stand-in driver
//!
//! Roles run through the real executor (`owlshift_runner::executor`), on the
//! fake harness: worktree, brief, agent environment, process tree, deadline
//! (`timeout_ms`, 60 s by default), result validation and isolation check.
//! No writer (OWL-18) exists yet, so the runner drives the rest itself, and
//! only as far as the scenarios need: it keeps the core state in memory from
//! Ready, builds the brief from the tracker, maps the run's outcome onto a
//! core event, posts the questions comment, sets the visible stage and
//! pushes the branch. It keeps the latest failure of the gate the executor
//! runs after a Build `done`, and hands it to the next Build brief. That
//! part is a stand-in: the writer replaces it, and
//! the scenario files stay. What it leaves out on purpose:
//!
//! - the brief's thread carries every tracker comment as a plain `comment`
//!   entry, the runner's own QUESTIONS comment included, where the executor
//!   will give each question round its own `questions` entry;
//! - no intake, admission, claim or ticket ref; the pipeline is the project's
//!   default variant;
//! - the answer check is not run: an `answer` step gives its verdict, and
//!   only `answered` until the answer-check role arrives in P2;
//! - a run's outcome maps onto the core event as `owlshift do` maps it
//!   (`owlshift_runner::on_demand::core_event`), and no PARKED, RE-ASK,
//!   RESUME or DELIVERY comment is written.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::time::Duration;

use jiff::{SignedDuration, Timestamp};
use serde::{Deserialize, Serialize};
use tempfile::TempDir;

use owlshift_adapters::tracker::markdown::MarkdownTracker;
use owlshift_contracts::brief::{
    Author, Brief, GateFailure, PermissionLevel, Permissions, Relation, ThreadEntry, TicketBrief,
};
use owlshift_contracts::comment::{Footer, Header, MarkerKind};
use owlshift_contracts::config::{ProjectConfig, States, TrackerKind};
use owlshift_contracts::format::Format;
use owlshift_contracts::ids::{RelativePath, TicketId};
use owlshift_contracts::result::RunResult;
use owlshift_contracts::{Role, Stage};
use owlshift_core::pipeline::Pipeline;
use owlshift_core::state::{Event, Status, TicketState, Transition};
use owlshift_runner::agent_env::AgentEnv;
use owlshift_runner::executor::{Executor, Failure, Git, Outcome, RESULT_PATH, RunReport, RunSpec};
use owlshift_runner::on_demand::core_event;

use crate::git::{GitEnv, Remote, seed};
use crate::harness::FakeHarness;
use crate::reply::{OWN_FAILURE, Reply};

/// The author name of the runner's own comments.
pub const OWLSHIFT_AUTHOR: &str = "owlshift";

/// The executor's deadline for a run, unless the scenario sets one.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

/// A scenario file.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scenario {
    pub description: String,
    pub ticket: TicketId,
    /// The virtual time the scenario starts at; step `n` is `n` minutes later.
    pub start: Timestamp,
    /// The executor's deadline for each run, and for the gate after a Build
    /// `done`, in milliseconds of real time; 60 000 by default.
    pub timeout_ms: Option<u64>,
    /// The project's gate for this scenario, in place of the fixture's
    /// `stack.gate`: a bench convenience, so short scenarios share a fixture.
    pub gate: Option<Vec<String>>,
    /// Whether the fake harness and the gate run inside the OS sandbox, as
    /// agent runs do (OWL-41). Off by default: the scenarios check the
    /// pipeline and the isolation check, a separate layer, and the sandbox
    /// would stop the writes the isolation scenarios make on purpose.
    #[serde(default)]
    pub confined: bool,
    #[serde(rename = "step")]
    pub steps: Vec<Step>,
}

/// One step: exactly one action, and what to expect after it.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    /// The scheduler dispatches the ready ticket.
    #[serde(default)]
    pub dispatch: bool,
    /// The current stage's role runs on the fake harness, with this reply.
    pub run: Option<Reply>,
    /// A person comments on the ticket.
    pub comment: Option<CommentStep>,
    /// The answer check's verdict on the open question round.
    pub answer: Option<Verdict>,
    #[serde(default)]
    pub expect: Expect,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommentStep {
    pub author: String,
    pub body: String,
}

/// The answer check's verdict; only `answered` until P2.
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Answered,
}

/// What must hold after a step. A key left out is not checked.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Expect {
    /// The core event the step produced, in snake case (`run_failed`), or
    /// `none`.
    pub event: Option<String>,
    pub stage: Option<Stage>,
    /// `none`, `needs_input` or `parked`.
    pub waiting: Option<String>,
    pub round: Option<u32>,
    pub failed_runs: Option<u32>,
    /// The tracker's visible stage.
    pub tracker_stage: Option<String>,
    /// How many comments the ticket has.
    pub comments: Option<usize>,
    pub last_comment: Option<LastComment>,
    /// Whether the ticket's branch is on the remote.
    pub branch_pushed: Option<bool>,
    /// Files on the remote's ticket branch, by path, with their content.
    #[serde(default)]
    pub branch_files: BTreeMap<String, String>,
    /// The authors' relations in the thread of the last brief, in order.
    pub brief_thread: Option<Vec<Relation>>,
    /// The gate failure of the step's run: `none`, or a text its command,
    /// reason or output contains.
    pub gate_failure: Option<String>,
    /// The gate failure the last brief carried: `none`, or a text its
    /// command, reason or output contains.
    pub brief_gate_failure: Option<String>,
    /// The commands of the gate the step's run passed.
    pub gate_passed: Option<Vec<String>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LastComment {
    pub author: Option<String>,
    pub first_line: Option<String>,
    /// Texts the body must contain.
    #[serde(default)]
    pub contains: Vec<String>,
}

/// Why a scenario failed: its name, the step, and what went wrong; for a run
/// step, the fake harness's exit status and output.
#[derive(Debug)]
pub struct ScenarioError {
    scenario: String,
    step: Option<(usize, &'static str)>,
    message: String,
    output: Option<String>,
}

impl fmt::Display for ScenarioError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.step {
            Some((number, kind)) => write!(
                f,
                "{}, step {number} ({kind}): {}",
                self.scenario, self.message
            )?,
            None => write!(f, "{}: {}", self.scenario, self.message)?,
        }
        if let Some(output) = &self.output {
            write!(f, "\n{output}")?;
        }
        Ok(())
    }
}

impl std::error::Error for ScenarioError {}

/// Plays a scenario file; its fixture folder is the file's path without the
/// `.toml` extension.
pub fn play(path: &Path, fake_harness: &Path) -> Result<(), ScenarioError> {
    let name = path
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let input = fs::read_to_string(path).map_err(|e| ScenarioError {
        scenario: name.clone(),
        step: None,
        message: format!("{}: {e}", path.display()),
        output: None,
    })?;
    play_str(&name, &input, &path.with_extension(""), fake_harness)
}

/// Plays a scenario given as text, on a fixture folder.
pub fn play_str(
    name: &str,
    input: &str,
    fixture: &Path,
    fake_harness: &Path,
) -> Result<(), ScenarioError> {
    let setup = |message: String| ScenarioError {
        scenario: name.to_owned(),
        step: None,
        message,
        output: None,
    };
    let scenario: Scenario =
        toml::from_str(input).map_err(|e| setup(format!("invalid scenario: {e}")))?;
    let mut driver = Driver::new(name, &scenario, fixture, fake_harness).map_err(setup)?;
    for (index, step) in scenario.steps.iter().enumerate() {
        let number = index + 1;
        let fail = |kind, message, output| ScenarioError {
            scenario: name.to_owned(),
            step: Some((number, kind)),
            message,
            output,
        };
        let action = step.action().map_err(|m| fail("?", m, None))?;
        driver.now = scenario
            .start
            .checked_add(SignedDuration::from_mins(
                i64::try_from(number).unwrap_or(i64::MAX),
            ))
            .map_err(|e| fail(action.kind(), e.to_string(), None))?;
        let event = driver
            .step(&action)
            .map_err(|m| fail(action.kind(), m, driver.run_output()))?;
        driver
            .check(event, &step.expect)
            .map_err(|m| fail(action.kind(), m, driver.run_output()))?;
    }
    Ok(())
}

enum Action<'a> {
    Dispatch,
    Run(&'a Reply),
    Comment(&'a CommentStep),
    Answer(Verdict),
}

impl Action<'_> {
    fn kind(&self) -> &'static str {
        match self {
            Action::Dispatch => "dispatch",
            Action::Run(_) => "run",
            Action::Comment(_) => "comment",
            Action::Answer(_) => "answer",
        }
    }
}

impl Step {
    fn action(&self) -> Result<Action<'_>, String> {
        let mut actions = Vec::new();
        if self.dispatch {
            actions.push(Action::Dispatch);
        }
        actions.extend(self.run.as_ref().map(Action::Run));
        actions.extend(self.comment.as_ref().map(Action::Comment));
        actions.extend(self.answer.map(Action::Answer));
        match <[_; 1]>::try_from(actions) {
            Ok([action]) => Ok(action),
            Err(_) => Err("a step does exactly one of dispatch, run, comment or answer".to_owned()),
        }
    }
}

/// The stand-in driver: see the module documentation.
struct Driver {
    scenario: String,
    fixture: PathBuf,
    fake_harness: PathBuf,
    tmp: TempDir,
    git: GitEnv,
    remote: Remote,
    tracker: MarkdownTracker,
    states: States,
    pipeline: Pipeline,
    /// The project's gate (`stack.gate`), carried in every brief.
    gate: Vec<String>,
    /// The latest failure of the gate the executor ran, for the next Build
    /// brief; a passing gate clears it, other outcomes leave it.
    gate_failure: Option<GateFailure>,
    executor: Executor,
    id: TicketId,
    branch: String,
    state: TicketState,
    now: Timestamp,
    runs: u32,
    last_brief: Option<Brief>,
    last_run: Option<RunReport>,
}

impl Driver {
    fn new(
        name: &str,
        scenario: &Scenario,
        fixture: &Path,
        fake_harness: &Path,
    ) -> Result<Self, String> {
        let tmp = tempfile::Builder::new()
            .prefix("owlshift scenario ")
            .tempdir()
            .map_err(|e| format!("temporary folder: {e}"))?;
        let git = GitEnv::create(tmp.path().join("home")).map_err(|e| e.to_string())?;
        let remote = seed(&git.at(scenario.start), tmp.path(), &fixture.join("repo"))
            .map_err(|e| e.to_string())?;
        let config_path = remote.checkout.join("owlshift.toml");
        let config = fs::read_to_string(&config_path)
            .map_err(|e| e.to_string())
            .and_then(|text| ProjectConfig::parse(&text).map_err(|e| e.to_string()))
            .map_err(|e| format!("{}: {e}", config_path.display()))?;
        if config.tracker.kind != TrackerKind::Markdown {
            return Err("a scenario's project uses the markdown tracker".to_owned());
        }
        // The on-demand run of P1: the ticket starts at Ready.
        let state = TicketState::restore(Status::Active(Stage::Ready), 0, 0, 0)
            .map_err(|e| e.to_string())?;
        let runner_git = git.clone();
        let timeout = scenario
            .timeout_ms
            .map_or(DEFAULT_TIMEOUT, Duration::from_millis);
        let agent = AgentEnv::new(git.agent_parent(), &[]).map_err(|e| e.to_string())?;
        let agent = if scenario.confined {
            agent
        } else {
            agent.without_confinement()
        };
        let executor = Executor {
            git: Git::with_setup("git", move |command| runner_git.apply(command)),
            agent,
            forge_hosts: Vec::new(),
            timeout,
            gate_timeout: timeout,
        };
        Ok(Self {
            executor,
            scenario: name.to_owned(),
            fixture: fixture.to_owned(),
            fake_harness: fake_harness.to_owned(),
            tracker: MarkdownTracker::new(&remote.checkout),
            states: config.tracker.states,
            pipeline: Pipeline::new(config.pipeline.default),
            gate: scenario.gate.clone().unwrap_or(config.stack.gate),
            gate_failure: None,
            branch: format!("owlshift/{}", scenario.ticket),
            id: scenario.ticket.clone(),
            state,
            now: scenario.start,
            runs: 0,
            last_brief: None,
            last_run: None,
            tmp,
            git,
            remote,
        })
    }

    /// Does one action; returns the core event it produced, if any.
    fn step(&mut self, action: &Action<'_>) -> Result<Option<Event>, String> {
        self.last_run = None;
        match action {
            Action::Dispatch => {
                self.apply(Event::Dispatched)?;
                self.set_stage(&self.states.working.clone())?;
                Ok(Some(Event::Dispatched))
            }
            Action::Run(reply) => self.run(reply).map(Some),
            Action::Comment(comment) => {
                self.tracker
                    .post_comment(&self.id, &comment.author, self.now, &comment.body)
                    .map_err(|e| e.to_string())?;
                Ok(None)
            }
            Action::Answer(Verdict::Answered) => {
                self.apply(Event::Answered)?;
                self.set_stage(&self.states.working.clone())?;
                Ok(Some(Event::Answered))
            }
        }
    }

    fn apply(&mut self, event: Event) -> Result<(), String> {
        self.state = match self
            .state
            .apply(self.pipeline, event)
            .map_err(|e| e.to_string())?
        {
            Transition::To(state) | Transition::Parked { state, .. } => state,
            Transition::Finished(finish) => {
                return Err(format!("the ticket left the machine ({finish:?})"));
            }
        };
        Ok(())
    }

    fn set_stage(&self, stage: &str) -> Result<(), String> {
        self.tracker
            .set_stage(&self.id, stage)
            .map_err(|e| e.to_string())
    }

    /// Runs the current stage's role through the executor, on the fake
    /// harness.
    fn run(&mut self, reply: &Reply) -> Result<Event, String> {
        let Status::Active(stage) = self.state.status() else {
            return Err(format!(
                "no stage role runs while the ticket is {:?}",
                self.state.status()
            ));
        };
        let role = stage
            .default_role()
            .ok_or_else(|| format!("no role runs at {}", name(&stage)))?;
        self.runs += 1;
        let dir = self.tmp.path().join("runs").join(self.runs.to_string());
        fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let worktree = self.worktree();

        let brief = self.brief(role)?;
        let mut reply = reply.clone();
        reply.result = reply.result.map(|path| self.fixture.join(path));
        reply.date = Some(self.now);
        // Outside the run directory, which a confined run cannot read.
        let replies = self.tmp.path().join("replies").join(self.runs.to_string());
        fs::create_dir_all(&replies).map_err(|e| format!("{}: {e}", replies.display()))?;
        let reply_path = replies.join("reply.toml");
        write(&reply_path, &reply.render())?;

        let spec = RunSpec {
            main: &self.remote.checkout,
            worktree: &worktree,
            branch: &self.branch,
            base: "origin/main",
            run_dir: &dir,
            brief: &brief,
        };
        let harness = FakeHarness {
            program: self.fake_harness.clone(),
            reply: reply_path,
            readable: self.git.agent_readable(),
        };
        let report = self
            .executor
            .run(&spec, &harness)
            .map_err(|e| e.to_string())?;
        self.last_brief = Some(brief);
        if let Some(gate) = &report.gate {
            self.gate_failure = gate.failure.clone();
        }
        let own_failure = report.exit_code == Some(OWN_FAILURE);
        let (event, result) = core_event(&report.outcome);
        let result = result.cloned();
        self.last_run = Some(report);
        if own_failure {
            return Err("the fake harness could not do what the reply says".to_owned());
        }

        self.apply(event)?;
        if let (Event::Questions, Some(result)) = (event, &result) {
            let body = self.questions_comment(result)?;
            self.tracker
                .post_comment(&self.id, OWLSHIFT_AUTHOR, self.now, &body)
                .map_err(|e| e.to_string())?;
            self.set_stage(&self.states.needs_input.clone())?;
        }
        if matches!(event, Event::Completed | Event::Questions) {
            self.push_if_ahead(&worktree)?;
        }
        Ok(event)
    }

    /// The ticket's worktree, beside the main checkout; the executor creates
    /// it at the first run, on the ticket's branch from `origin/main`.
    fn worktree(&self) -> PathBuf {
        self.tmp.path().join("worktrees").join(self.id.as_str())
    }

    fn brief(&self, role: Role) -> Result<Brief, String> {
        let ticket = self.tracker.ticket(&self.id).map_err(|e| e.to_string())?;
        let decider = ticket
            .assignee
            .clone()
            .ok_or("the ticket has no assignee to act as its decider")?;
        let author = |name: String| Author {
            relation: if name == OWLSHIFT_AUTHOR {
                Relation::Owlshift
            } else if name == decider {
                Relation::Decider
            } else {
                Relation::Other
            },
            name,
        };
        let thread = self
            .tracker
            .comments(&self.id)
            .map_err(|e| e.to_string())?
            .into_iter()
            .map(|comment| ThreadEntry::Comment {
                at: comment.at,
                author: author(comment.author),
                body: comment.body,
            })
            .collect();
        Ok(Brief {
            format: Format,
            role,
            project: self.scenario.clone(),
            ticket: TicketBrief {
                id: self.id.clone(),
                title: ticket.title,
                url: None,
                labels: ticket.labels,
                author: author(ticket.author),
                description: ticket.description,
            },
            decider: decider.clone(),
            thread,
            checkpoint: None,
            zones: Vec::new(),
            resources: Vec::new(),
            rules: Vec::new(),
            permissions: Permissions {
                level: if role == Role::Build {
                    PermissionLevel::WriteWorktree
                } else {
                    PermissionLevel::ReadOnly
                },
                network: false,
                browser: false,
            },
            gate: self.gate.clone(),
            gate_failure: if role == Role::Build {
                self.gate_failure.clone()
            } else {
                None
            },
            result_path: RelativePath::new(RESULT_PATH)
                .expect("RESULT_PATH is a valid relative path"),
        })
    }

    /// The QUESTIONS comment of the round just opened.
    fn questions_comment(&self, result: &RunResult) -> Result<String, String> {
        let round = NonZeroU32::new(self.state.round()).ok_or("no question round is open")?;
        let header = Header {
            kind: MarkerKind::Questions,
            round: Some(round),
        };
        let mut body = format!("{}\n\n{}\n", header.render(), result.summary);
        for question in &result.questions {
            body.push_str(&format!(
                "\n**{}** ({}) {}\n{}\n",
                question.id, question.category, question.text, question.context
            ));
            if !question.options.is_empty() {
                body.push_str(&format!("Options: {}\n", question.options.join(" / ")));
            }
            if let Some(recommendation) = &question.recommendation {
                body.push_str(&format!("Recommendation: {recommendation}\n"));
            }
        }
        let footer = Footer {
            format: Format,
            kind: MarkerKind::Questions,
            ticket: self.id.clone(),
            round: Some(round),
            run: None,
        };
        body.push_str(&format!("\n{}\n", footer.render()));
        Ok(body)
    }

    fn push_if_ahead(&self, worktree: &Path) -> Result<(), String> {
        let ahead = self
            .git
            .run(worktree, &["rev-list", "--count", "origin/main..HEAD"])
            .map_err(|e| e.to_string())?;
        if String::from_utf8_lossy(&ahead).trim() != "0" {
            self.git
                .at(self.now)
                .run(worktree, &["push", "--quiet", "origin", &self.branch])
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    fn check(&self, event: Option<Event>, expect: &Expect) -> Result<(), String> {
        if let Some(expected) = &expect.event {
            let found = event.map_or_else(|| "none".to_owned(), event_name);
            same("event", expected, found)?;
        }
        if let Some(expected) = &expect.stage {
            same("stage", name(expected), name(&self.state.stage()))?;
        }
        if let Some(expected) = &expect.waiting {
            let found = match self.state.status() {
                Status::Active(_) => "none",
                Status::NeedsInput { .. } => "needs_input",
                Status::Parked { .. } => "parked",
            };
            same("waiting", expected, found)?;
        }
        if let Some(expected) = expect.round {
            same("round", expected, self.state.round())?;
        }
        if let Some(expected) = expect.failed_runs {
            same("failed_runs", expected, self.state.failed_runs())?;
        }
        if let Some(expected) = &expect.tracker_stage {
            let ticket = self.tracker.ticket(&self.id).map_err(|e| e.to_string())?;
            same("tracker_stage", expected, ticket.stage)?;
        }
        if expect.comments.is_some() || expect.last_comment.is_some() {
            let comments = self.tracker.comments(&self.id).map_err(|e| e.to_string())?;
            if let Some(expected) = expect.comments {
                same("comments", expected, comments.len())?;
            }
            if let Some(last) = &expect.last_comment {
                let comment = comments
                    .last()
                    .ok_or("expected a last comment, found none")?;
                if let Some(expected) = &last.author {
                    same("last_comment.author", expected, &comment.author)?;
                }
                let first_line = comment.body.lines().next().unwrap_or_default();
                if let Some(expected) = &last.first_line {
                    same("last_comment.first_line", expected, first_line)?;
                }
                for text in &last.contains {
                    if !comment.body.contains(text.as_str()) {
                        return Err(format!(
                            "expected last_comment to contain {text:?}, found {:?}",
                            comment.body
                        ));
                    }
                }
            }
        }
        if let Some(expected) = expect.branch_pushed {
            let refname = format!("refs/heads/{}", self.branch);
            let found = self
                .git
                .run(
                    &self.remote.bare,
                    &["for-each-ref", "--format=%(refname)", &refname],
                )
                .map_err(|e| e.to_string())?;
            same("branch_pushed", expected, !found.is_empty())?;
        }
        for (path, expected) in &expect.branch_files {
            let found = self
                .git
                .run(
                    &self.remote.bare,
                    &["show", &format!("{}:{path}", self.branch)],
                )
                .map(|bytes| format!("{:?}", String::from_utf8_lossy(&bytes)))
                .unwrap_or_else(|e| format!("nothing ({e})"));
            same(
                &format!("branch file {path}"),
                format!("{expected:?}"),
                found,
            )?;
        }
        if let Some(expected) = &expect.brief_thread {
            let brief = self
                .last_brief
                .as_ref()
                .ok_or("expected a brief, found none")?;
            let found: Vec<Relation> = brief
                .thread
                .iter()
                .filter_map(|entry| match entry {
                    ThreadEntry::Comment { author, .. } => Some(author.relation),
                    ThreadEntry::Questions { .. } | ThreadEntry::Reask { .. } => None,
                })
                .collect();
            same("brief_thread", names(expected), names(&found))?;
        }
        if let Some(expected) = &expect.gate_failure {
            let found = self
                .last_run
                .as_ref()
                .and_then(|report| match &report.outcome {
                    Outcome::Failed(Failure::Gate(failure)) => Some(failure.as_ref()),
                    _ => None,
                });
            gate_failure_is("gate_failure", expected, found)?;
        }
        if let Some(expected) = &expect.brief_gate_failure {
            let brief = self
                .last_brief
                .as_ref()
                .ok_or("expected a brief, found none")?;
            gate_failure_is("brief_gate_failure", expected, brief.gate_failure.as_ref())?;
        }
        if let Some(expected) = &expect.gate_passed {
            let gate = self
                .last_run
                .as_ref()
                .and_then(|report| report.gate.as_ref())
                .ok_or("expected a gate run, found none")?;
            if let Some(failure) = &gate.failure {
                return Err(format!("expected the gate to pass, found {failure:?}"));
            }
            same(
                "gate_passed",
                format!("{expected:?}"),
                format!("{:?}", gate.commands),
            )?;
        }
        Ok(())
    }

    /// The last run's outcome, exit status and output, for an error message.
    fn run_output(&self) -> Option<String> {
        self.last_run.as_ref().map(|report| {
            let log = |path: &Path| {
                fs::read(path).map_or_else(
                    |e| format!("({}: {e})", path.display()),
                    |bytes| String::from_utf8_lossy(&bytes).into_owned(),
                )
            };
            format!(
                "outcome: {:?}\nfake harness exit code: {:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
                report.outcome,
                report.exit_code,
                log(&report.stdout_log),
                log(&report.stderr_log)
            )
        })
    }
}

/// Checks a gate failure against `none` or a text it contains.
fn gate_failure_is(field: &str, expected: &str, found: Option<&GateFailure>) -> Result<(), String> {
    match (expected, found) {
        ("none", None) => Ok(()),
        ("none", Some(failure)) => Err(format!("expected {field} none, found {failure:?}")),
        (_, None) => Err(format!(
            "expected {field} containing {expected:?}, found none"
        )),
        (_, Some(failure)) => {
            let text = format!(
                "{} {} {}",
                failure.command.as_deref().unwrap_or_default(),
                failure.reason,
                failure.output
            );
            if text.contains(expected) {
                Ok(())
            } else {
                Err(format!(
                    "expected {field} containing {expected:?}, found {failure:?}"
                ))
            }
        }
    }
}

fn same(field: &str, expected: impl fmt::Display, found: impl fmt::Display) -> Result<(), String> {
    let (expected, found) = (expected.to_string(), found.to_string());
    if expected == found {
        Ok(())
    } else {
        Err(format!("expected {field} {expected}, found {found}"))
    }
}

/// A value's serialized name, such as `design_review` for a stage.
fn name<T: Serialize>(value: &T) -> String {
    match toml::Value::try_from(value) {
        Ok(toml::Value::String(name)) => name,
        _ => "?".to_owned(),
    }
}

fn names<T: Serialize>(values: &[T]) -> String {
    let names: Vec<_> = values.iter().map(name).collect();
    format!("[{}]", names.join(", "))
}

/// An event's name in snake case: `RunFailed` is `run_failed`.
fn event_name(event: Event) -> String {
    let mut out = String::new();
    for (index, c) in format!("{event:?}").chars().enumerate() {
        if c.is_ascii_uppercase() {
            if index > 0 {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

fn write(path: &Path, content: &str) -> Result<(), String> {
    fs::write(path, content).map_err(|e| format!("{}: {e}", path.display()))
}
