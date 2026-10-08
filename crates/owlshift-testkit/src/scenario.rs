//! Scenario files and their runner.
//!
//! A scenario is a TOML file with a fixture folder of the same name beside
//! it: `repo/`, the project seeded as `main` into a local bare remote, the
//! prepared result files its replies name, and an optional `personal.toml`,
//! the operator's personal file, whose `allow_gate_env` the project's
//! `stack.gate_env` is checked against as when the configuration is loaded. Each `[[step]]` does one thing
//! (`dispatch = true`, `run = <reply>`, `comment = { author, body }` or
//! `answer = <reply>` or `resolve = <reply>`) and may carry an `expect` table, checked right
//! after it. Time is virtual: step `n` happens `n` minutes after `start`.
//!
//! # The stand-in driver
//!
//! Roles run through the real executor (`owlshift_runner::executor`), on the
//! fake harness: worktree, brief, agent environment, process tree, deadline
//! (`timeout_ms`, 60 s by default), result validation and isolation check.
//! No writer (OWL-18) exists yet, so the runner drives the rest itself, and
//! only as far as the scenarios need: it keeps the core state in memory from
//! Ready, builds the brief from the tracker, its ticket and thread read as
//! `owlshift do` reads them (`owlshift_runner::on_demand::account_author`
//! and `thread`), maps the run's outcome onto a
//! core event, posts the questions comment, sets the visible stage after
//! the same core events as `owlshift do` and `continue`, through the same
//! Writer (`owlshift_runner::writer::VisibleStage::after`), and to parked on
//! a park, and pushes the branch. It keeps the latest failure of the gate the executor
//! runs after a Build `done`, and hands it to the next Build brief, as it
//! hands the next Build runs why a Build `result.json` was refused
//! (`owlshift_runner::on_demand::result_refusal`, OWL-180), a refusal that
//! never spends the last attempt when that run was not told of one
//! (OWL-183), and whether it was the build role's own decisions, which holds
//! them to asking (`decisions_refused`, OWL-186), kept as `owlshift do` and
//! `continue` keep them in the ticket ref until a Build result is accepted
//! (`owlshift_runner::on_demand::build_refusal_after`, OWL-192). A
//! `continue` step is a person's new command: the gate's failure, which a
//! command holds in memory, is gone, and a parked ticket restarts. That
//! part is a stand-in: the writer replaces it, and
//! the scenario files stay.
//!
//! A `run` step runs the current stage's role. When it asks questions, they
//! are routed as `owlshift do` routes them (`owlshift_runner::resolver`):
//! always-human ones, and every question of a false premise, go to the
//! decider; when any is left for the resolver, nothing is posted and the core
//! state does not move until a `resolve` step runs the resolver on them. It
//! posts each decision as a DECISION comment
//! (`owlshift_runner::writer::DecisionComment`), kept to take that comment's
//! place in every later brief's thread as a `decision` entry, and opens a
//! round of what is left, renumbered Q1..Qn; when nothing is left, the
//! ticket stays at its stage for the next `run`. A resolver without a
//! usable result sends every question to the decider, and one that breaks
//! isolation parks the ticket. An `answer` step runs the
//! answer check (OWL-116) while the ticket waits for input, once answers
//! arrived, as `owlshift continue` decides it but with no quiet window
//! (`owlshift_runner::answer_check::readiness`): a comment of the latest
//! ask's decider newer than that ask and than what the last check with
//! verdicts read. A failed or interrupted check is
//! retried on the same answers, and a check whose result was refused tells
//! the next one why, kept on the latest ask as `owlshift continue` keeps it
//! in the ticket ref (`owlshift_runner::answer_check::keep`, OWL-184). Its outcome maps onto one core event
//! (`owlshift_runner::answer_check::event`), a refused result the check was
//! not told of never spending the last attempt (OWL-190): an answer settles the round,
//! a RESUME comment restates what was understood of each of its questions
//! (`owlshift_runner::writer::ResumeComment`) and the ticket resumes; an
//! incomplete one posts a RE-ASK comment with only the open questions
//! (`owlshift_runner::writer::ReaskComment`); a counter-question posts a
//! REPLY comment with the check's reply to it
//! (`owlshift_runner::writer::ReplyComment`) and the ticket keeps waiting,
//! the REPLY staying an `owlshift` comment in every later brief's thread.
//! The driver keeps its asks and decisions as a ticket ref's
//! `questions.json` would (`owlshift_contracts::refs::TicketQuestions`), in
//! memory: each ask, a round's questions or a re-ask, with its comment, its
//! decider, the ticket's assignee (no zone owner is resolved), and the
//! verdicts of the latest check on it and why its result was refused, kept
//! as `continue` keeps them (`owlshift_runner::answer_check::keep`); each takes the place of its comment in
//! every brief's thread, as a `questions` or `reask` entry, as the
//! runner's own thread places it; and Build's kept refusal. Whenever the core parks the ticket, after a run or an
//! answer check, the driver posts a PARKED comment
//! (`owlshift_runner::writer::ParkedComment`). What it leaves out on purpose:
//!
//! - no intake, admission, claim or ticket ref; the pipeline is the project's
//!   default variant, and the asks live in memory, not in the ticket ref;
//! - a stage run's outcome maps onto the core event as `owlshift do` maps it
//!   (`owlshift_runner::on_demand::stage_event`), and no DELIVERY comment is
//!   written;
//! - no bound on the runs in a row whose questions the resolver all decided
//!   (`owlshift_core::state::MAX_RESOLVED_PASSES`), which `owlshift do` counts
//!   per command: a scenario's commands are only its `continue` steps, and
//!   it plays each run by hand.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::time::Duration;

use jiff::{SignedDuration, Timestamp};
use serde::{Deserialize, Serialize};
use tempfile::TempDir;

use owlshift_adapters::tracker::markdown::{self, MarkdownTracker};
use owlshift_adapters::tracker::{Person, Tracker};
use owlshift_contracts::brief::{
    Brief, GateFailure, PermissionLevel, Permissions, Relation, ThreadEntry, TicketBrief,
};
use owlshift_contracts::config::{PersonalConfig, ProjectConfig, States, TrackerKind};
use owlshift_contracts::format::Format;
use owlshift_contracts::ids::{RelativePath, TicketId};
use owlshift_contracts::refs::{Ask, AskDecider, AskKind, KeptDecision, TicketQuestions};
use owlshift_contracts::result::{Question, RunResult, Status as ResultStatus, Verdict};
use owlshift_contracts::{Role, Stage};
use owlshift_core::decider::{DeciderRule, brief_zones};
use owlshift_core::gate::GatePolicy;
use owlshift_core::pipeline::Pipeline;
use owlshift_core::state::{Event, ParkReason, Status, TicketState, Transition};
use owlshift_runner::agent_env::AgentEnv;
use owlshift_runner::answer_check::{self, Readiness};
use owlshift_runner::executor::{Executor, Failure, Git, Outcome, RESULT_PATH, RunReport, RunSpec};
use owlshift_runner::on_demand::{
    account_author, build_refusal_after, stage_event, tell_build, thread,
};
use owlshift_runner::resolver::{self, Fallback, Resolved};
use owlshift_runner::writer::{
    DecisionComment, ParkedComment, QuestionsComment, ReaskComment, ReplyComment, Restart,
    ResumeComment, VisibleStage, Writer,
};

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
    /// The fixture folder to play on, a sibling of the scenario file, in place
    /// of the one named after the file: a scenario on another scenario's
    /// setup brings its own results, not a copy of the project.
    pub fixture: Option<String>,
    pub ticket: TicketId,
    /// The virtual time the scenario starts at; step `n` is `n` minutes later.
    pub start: Timestamp,
    /// The executor's deadline for each run, and for the gate after a Build
    /// `done`, in milliseconds of real time; 60 000 by default.
    pub timeout_ms: Option<u64>,
    /// The project's gate for this scenario, in place of the fixture's
    /// `stack.gate`: a bench convenience, so short scenarios share a fixture.
    pub gate: Option<Vec<String>>,
    /// Variables added to the runner's environment the agent environment is
    /// built from (the bench's, not the test process's), replacing one of
    /// the same name in any letter case: what a project's `stack.gate_env`
    /// can pick from.
    #[serde(default)]
    pub runner_env: BTreeMap<String, String>,
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
    /// The answer check runs on the fake harness, with this reply, on the
    /// open question round.
    pub answer: Option<Reply>,
    /// The resolver runs on the fake harness, with this reply, on the
    /// questions the last run left for it.
    pub resolve: Option<Reply>,
    /// A person types `owlshift continue`: a new command, so what the last
    /// one held in memory (the gate's failure) is gone, and a parked ticket
    /// is restarted. The next steps play what it runs.
    #[serde(default, rename = "continue")]
    pub continue_command: bool,
    #[serde(default)]
    pub expect: Expect,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommentStep {
    pub author: String,
    pub body: String,
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
    /// Re-asks of the current round.
    pub reasks: Option<u32>,
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
    /// Every entry of the last brief's thread, in order: `questions`,
    /// `reask` or `decision` for what the driver kept, a comment as its
    /// author's relation (`decider`, `owlshift`, `other`).
    pub brief_entries: Option<Vec<String>>,
    /// The question ids of the latest ask in the last brief's thread: the
    /// ones an answer check gives its verdicts on.
    pub brief_latest_ask: Option<Vec<String>>,
    /// The `decision` of each `decision` entry of the last brief's thread,
    /// in order.
    pub brief_decisions: Option<Vec<String>>,
    /// The ids of the questions the last brief gave the resolver.
    pub brief_resolve: Option<Vec<String>>,
    /// The always-human categories the last brief carried.
    pub brief_always_human: Option<Vec<String>>,
    /// The gate failure of the step's run: `none`, or a text its command,
    /// reason or output contains.
    pub gate_failure: Option<String>,
    /// The gate failure the last brief carried: `none`, or a text its
    /// command, reason or output contains.
    pub brief_gate_failure: Option<String>,
    /// The refusal of the previous result the last brief carried: `none`,
    /// or a text it contains.
    pub brief_result_refusal: Option<String>,
    /// Whether the last brief held its Build run to asking
    /// (`decisions_refused`).
    pub brief_decisions_refused: Option<bool>,
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
    /// Texts the body must not contain.
    #[serde(default)]
    pub lacks: Vec<String>,
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
/// `.toml` extension, or the sibling folder its `fixture` key names.
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
    // Only this key is read here; `play_str` parses the whole file.
    let named = toml::from_str::<FixtureKey>(&input)
        .ok()
        .and_then(|key| key.fixture);
    let folder = match named {
        Some(fixture) if !is_folder_name(&fixture) => {
            return Err(ScenarioError {
                scenario: name,
                step: None,
                message: format!("invalid scenario: fixture \"{fixture}\" is not a folder name"),
                output: None,
            });
        }
        Some(fixture) => path.with_file_name(fixture),
        None => path.with_extension(""),
    };
    play_str(&name, &input, &folder, fake_harness)
}

/// Whether a fixture name is a plain sibling folder name: one normal path
/// component, so not empty, `.`, `..`, a path or a drive prefix.
fn is_folder_name(name: &str) -> bool {
    let mut components = Path::new(name).components();
    matches!(
        (components.next(), components.next()),
        (Some(std::path::Component::Normal(_)), None)
    ) && !name.contains(['/', '\\', ':'])
}

/// Only the `fixture` key of a scenario file; a file that does not parse is
/// reported by `play_str`.
#[derive(Deserialize)]
struct FixtureKey {
    fixture: Option<String>,
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
    Answer(&'a Reply),
    Resolve(&'a Reply),
    Continue,
}

impl Action<'_> {
    fn kind(&self) -> &'static str {
        match self {
            Action::Dispatch => "dispatch",
            Action::Run(_) => "run",
            Action::Comment(_) => "comment",
            Action::Answer(_) => "answer",
            Action::Resolve(_) => "resolve",
            Action::Continue => "continue",
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
        actions.extend(self.answer.as_ref().map(Action::Answer));
        actions.extend(self.resolve.as_ref().map(Action::Resolve));
        if self.continue_command {
            actions.push(Action::Continue);
        }
        match <[_; 1]>::try_from(actions) {
            Ok([action]) => Ok(action),
            Err(_) => Err(
                "a step does exactly one of dispatch, run, comment, answer, resolve or continue"
                    .to_owned(),
            ),
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
    /// Whether the ticket would have a ticket ref: made once a round opens,
    /// a decision is kept or a Build result is refused, and kept after,
    /// whatever it holds. A `continue` needs one, and a PARKED comment says
    /// `continue` with one, `do` without.
    has_ref: bool,
    executor: Executor,
    id: TicketId,
    branch: String,
    state: TicketState,
    now: Timestamp,
    runs: u32,
    /// What a ticket ref's `questions.json` would keep, in memory: each ask
    /// the driver posted with its comment, its decider and the verdicts of
    /// the latest check on it, what the last check read, and each decision
    /// with its comment.
    questions: TicketQuestions,
    /// The project's gate policy, which routes a run's questions.
    policy: GatePolicy,
    /// The result of the last run, while its questions wait for a
    /// `resolve` step.
    pending: Option<RunResult>,
    /// The questions the resolver's brief gives it.
    resolving: Vec<Question>,
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
        let mut parent = git.agent_parent();
        for (name, value) in &scenario.runner_env {
            parent.retain(|(n, _)| !n.to_str().is_some_and(|n| n.eq_ignore_ascii_case(name)));
            parent.push((name.into(), value.into()));
        }
        let personal_path = fixture.join("personal.toml");
        let personal = match fs::read_to_string(&personal_path) {
            Ok(text) => Some(PersonalConfig::parse(&text).map_err(|e| e.to_string())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => Some(Err(error.to_string())),
        }
        .transpose()
        .map_err(|e| format!("{}: {e}", personal_path.display()))?;
        // The remote is a local bare repository, not a GitHub one: names
        // scoped to a repository could never apply, so they are refused
        // rather than ignored.
        if personal
            .as_ref()
            .is_some_and(|p| !p.repositories.is_empty())
        {
            return Err(format!(
                "{}: a scenario's repository is not on GitHub, so `repositories` cannot apply: \
                 use the machine-wide `allow_gate_env`",
                personal_path.display()
            ));
        }
        let allowed = personal
            .as_ref()
            .map(|personal| personal.allow_gate_env_names(None))
            .unwrap_or_default();
        config
            .check_gate_env(&allowed)
            .map_err(|e| format!("{}: {e}", config_path.display()))?;
        let agent = AgentEnv::for_project(parent, &config.stack.gate_env_names(), &allowed)
            .map_err(|e| e.to_string())?;
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
            policy: GatePolicy::new(&config.policy.always_human, config.pipeline.plan_approval),
            pending: None,
            resolving: Vec::new(),
            gate: scenario.gate.clone().unwrap_or(config.stack.gate),
            gate_failure: None,
            has_ref: false,
            branch: format!("owlshift/{}", scenario.ticket),
            id: scenario.ticket.clone(),
            state,
            now: scenario.start,
            runs: 0,
            questions: TicketQuestions::new(),
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
                self.show_after(Event::Dispatched)?;
                Ok(Some(Event::Dispatched))
            }
            Action::Run(reply) => self.run(reply),
            Action::Comment(comment) => {
                self.tracker
                    .post_comment(&self.id, &comment.author, self.now, &comment.body)
                    .map_err(|e| e.to_string())?;
                Ok(None)
            }
            Action::Answer(reply) => self.answer(reply).map(Some),
            Action::Resolve(reply) => self.resolve(reply),
            Action::Continue => self.continue_command(),
        }
    }

    /// A person's `owlshift continue`, as far as a scenario needs it: refused
    /// without a ticket ref, as the command is; what the last command held
    /// in memory is gone; a parked ticket is restarted and shows what it
    /// waits for again, as `continue` does (`owlshift_runner::on_demand`).
    /// Whatever the command then runs, the next steps play.
    fn continue_command(&mut self) -> Result<Option<Event>, String> {
        if !self.has_ref {
            return Err("nothing to continue: Owlshift keeps nothing on the ticket".to_owned());
        }
        if self.pending.is_some() {
            return Err("the last run's questions wait for a `resolve` step".to_owned());
        }
        self.gate_failure = None;
        if !matches!(self.state.status(), Status::Parked { .. }) {
            return Ok(None);
        }
        self.apply(Event::Restarted)?;
        if let Some(stage) = VisibleStage::of(self.state.status()) {
            Writer::new(&self.tracker)
                .set_stage(&self.id, stage, &self.states)
                .map_err(|e| e.to_string())?;
        }
        Ok(Some(Event::Restarted))
    }

    /// Applies `event` to the core state; returns why the ticket parked,
    /// when it did.
    fn apply(&mut self, event: Event) -> Result<Option<ParkReason>, String> {
        let (state, parked) = match self
            .state
            .apply(self.pipeline, event)
            .map_err(|e| e.to_string())?
        {
            Transition::To(state) => (state, None),
            Transition::Parked { state, reason } => (state, Some(reason)),
            Transition::Finished(finish) => {
                return Err(format!("the ticket left the machine ({finish:?})"));
            }
        };
        self.state = state;
        Ok(parked)
    }

    /// Posts the PARKED comment of a park, after `report`'s run, and moves
    /// the visible stage to parked, as `owlshift do` and `continue` do
    /// (`owlshift_runner::on_demand`, `park`). A ticket that asked questions,
    /// kept a decision or had a Build result refused would have a ticket
    /// ref, so `continue` restarts it; one with none runs again with `do`. A
    /// failed write fails the scenario.
    fn post_parked(
        &self,
        reason: ParkReason,
        report: &RunReport,
        open: Vec<(Question, Verdict)>,
    ) -> Result<(), String> {
        let detail = match &report.outcome {
            Outcome::Failed(failure) => failure.to_string(),
            Outcome::Finished { result, .. } => result.summary.clone(),
            Outcome::Quarantined(_) | Outcome::UsageLimit { .. } => String::new(),
        };
        let body = ParkedComment {
            ticket: self.id.clone(),
            reason,
            detail,
            round: NonZeroU32::new(self.state.round()).filter(|_| reason == ParkReason::Reasks),
            open,
            restart: if self.has_ref {
                Restart::Continue
            } else {
                Restart::Do
            },
        }
        .render();
        self.post(&body)?;
        Writer::new(&self.tracker)
            .set_stage(&self.id, VisibleStage::Parked, &self.states)
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    /// Moves the visible stage as `owlshift do` and `continue` do after
    /// `event`. A failed write fails the scenario, which expects the stage.
    fn show_after(&self, event: Event) -> Result<(), String> {
        let Some(stage) = VisibleStage::after(event) else {
            return Ok(());
        };
        Writer::new(&self.tracker)
            .set_stage(&self.id, stage, &self.states)
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    /// Runs the current stage's role through the executor, on the fake
    /// harness. Questions some of which the resolver settles first move
    /// nothing until a `resolve` step: no event.
    fn run(&mut self, reply: &Reply) -> Result<Option<Event>, String> {
        if self.pending.is_some() {
            return Err("the last run's questions wait for a `resolve` step".to_owned());
        }
        let Status::Active(stage) = self.state.status() else {
            return Err(format!(
                "no stage role runs while the ticket is {:?}",
                self.state.status()
            ));
        };
        let role = stage
            .default_role()
            .ok_or_else(|| format!("no role runs at {}", name(&stage)))?;
        let report = self.execute(role, reply)?;
        let brief = self.last_brief.as_ref().ok_or("no brief")?;
        let (event, result) = stage_event(&report.outcome, brief);
        let result = result.cloned();
        if let (Event::Questions, Some(result)) = (event, &result)
            && result.status != ResultStatus::PremiseFalse
            && !resolver::route(&self.policy, &result.questions)
                .to_resolver
                .is_empty()
        {
            self.last_run = Some(report);
            self.pending = Some(result.clone());
            self.push_if_ahead(&self.worktree())?;
            return Ok(None);
        }
        let parked = self.apply(event)?;
        if let Some(reason) = parked {
            self.post_parked(reason, &report, Vec::new())?;
        }
        self.last_run = Some(report);
        if let (Event::Questions, Some(result)) = (event, &result) {
            let questions = left(&result.questions, &[]);
            self.post_round(result, questions, 0, None)?;
        }
        if matches!(event, Event::Completed | Event::Questions) {
            self.push_if_ahead(&self.worktree())?;
        }
        Ok(Some(event))
    }

    /// Posts the QUESTIONS comment of a round just opened, and keeps the ask
    /// with its comment and its decider, the ticket's assignee: the bench
    /// resolves no zone owner.
    fn post_round(
        &mut self,
        result: &RunResult,
        questions: Vec<Question>,
        decided: usize,
        fallback: Option<Fallback>,
    ) -> Result<(), String> {
        let round = NonZeroU32::new(self.state.round()).ok_or("no question round is open")?;
        let body = QuestionsComment {
            ticket: self.id.clone(),
            round,
            summary: result.summary.clone(),
            questions: questions.clone(),
            premise_false: result.status == ResultStatus::PremiseFalse,
            decided,
            fallback,
        }
        .render();
        let account = self
            .tracker
            .ticket(&self.id)
            .map_err(|e| e.to_string())?
            .assignee
            .ok_or("the ticket has no assignee to act as its decider")?;
        let posted = self.post(&body)?;
        self.has_ref = true;
        self.questions.asks.push(Ask {
            kind: AskKind::Questions,
            round,
            at: posted.at,
            comment: posted.id,
            questions,
            decider: AskDecider {
                account,
                by: DeciderRule::Assignee,
            },
            verdicts: Vec::new(),
            result_refusal: None,
        });
        self.show_after(Event::Questions)
    }

    /// Runs the resolver through the executor, on the fake harness, on the
    /// questions the last run left for it, and acts on what it left: each
    /// decision posted as a DECISION comment, then a round of what is left,
    /// or nothing when every question was decided (no event).
    fn resolve(&mut self, reply: &Reply) -> Result<Option<Event>, String> {
        let result = self
            .pending
            .take()
            .ok_or("no run's questions wait for the resolver")?;
        let given = resolver::route(&self.policy, &result.questions).to_resolver;
        self.resolving.clone_from(&given);
        let report = self.execute(Role::Resolver, reply);
        self.resolving.clear();
        let report = report?;
        let (decided, fallback) = match resolver::outcome(&report.outcome) {
            Resolved::Quarantined => {
                if let Some(reason) = self.apply(Event::Quarantined)? {
                    self.post_parked(reason, &report, Vec::new())?;
                }
                self.last_run = Some(report);
                return Ok(Some(Event::Quarantined));
            }
            Resolved::Fallback(fallback) => (Vec::new(), Some(fallback)),
            Resolved::Done(resolutions) => {
                let mut decided = Vec::new();
                for (question, decision, basis) in
                    resolver::decisions(&self.policy, &given, resolutions)
                {
                    let body = DecisionComment {
                        ticket: self.id.clone(),
                        question: question.clone(),
                        decision: decision.to_owned(),
                        basis: basis.to_owned(),
                        run: None,
                    }
                    .render();
                    let posted = self.post(&body)?;
                    self.has_ref = true;
                    self.questions.decisions.push(KeptDecision {
                        at: posted.at,
                        comment: posted.id,
                        question: question.clone(),
                        decision: decision.to_owned(),
                        basis: basis.to_owned(),
                    });
                    decided.push(question.id.clone());
                }
                (decided, None)
            }
        };
        self.last_run = Some(report);
        let questions = left(&result.questions, &decided);
        if questions.is_empty() {
            return Ok(None);
        }
        self.apply(Event::Questions)?;
        self.post_round(&result, questions, decided.len(), fallback)?;
        Ok(Some(Event::Questions))
    }

    /// Runs the answer check through the executor, on the fake harness,
    /// once answers arrived, and acts on its one core event: the ticket
    /// resumes, or the open questions are asked again. A counter-question
    /// gets a REPLY with the check's reply and waits for the decider's next
    /// comment.
    fn answer(&mut self, reply: &Reply) -> Result<Event, String> {
        if !matches!(self.state.status(), Status::NeedsInput { .. }) {
            return Err(format!(
                "no answer check runs while the ticket is {:?}",
                self.state.status()
            ));
        }
        let read_through = self.require_new_answer()?;
        let report = self.execute(Role::AnswerCheck, reply)?;
        let brief = self.last_brief.as_ref().ok_or("no brief")?;
        // A refused result the check was not told of is freed from the last
        // attempt, as `owlshift continue` frees it (OWL-190).
        let (event, result) = answer_check::event(&report.outcome, brief);
        let result = result.cloned();
        // A check with verdicts keeps what it read and its verdicts, and a
        // refused one its reason for the next check, as `owlshift continue`
        // does.
        answer_check::keep(&mut self.questions, read_through, event, &report.outcome);

        let parked = self.apply(event)?;
        if let Some(reason) = parked {
            let open = match (reason, &result, &self.last_brief) {
                (ParkReason::Reasks, Some(result), Some(brief)) => {
                    answer_check::open_questions(brief, &result.verdicts)
                }
                _ => Vec::new(),
            };
            self.post_parked(reason, &report, open)?;
        }
        self.last_run = Some(report);
        match (event, self.state.status()) {
            (Event::Answered, _) => {
                let round =
                    NonZeroU32::new(self.state.round()).ok_or("no question round is open")?;
                let body = ResumeComment {
                    ticket: self.id.clone(),
                    round,
                    understood: answer_check::understood(self.questions.round_asks(round)),
                }
                .render();
                self.post(&body)?;
                self.show_after(Event::Answered)?;
            }
            // Still waiting: past the re-ask limit, the core parked it.
            (Event::Incomplete, Status::NeedsInput { .. }) => {
                let result = result.ok_or("an incomplete answer comes from a result")?;
                let brief = self.last_brief.as_ref().ok_or("no brief")?;
                let open = answer_check::open_questions(brief, &result.verdicts);
                let round =
                    NonZeroU32::new(self.state.round()).ok_or("no question round is open")?;
                let questions = open.iter().map(|(question, _)| question.clone()).collect();
                let body = ReaskComment {
                    ticket: self.id.clone(),
                    round,
                    reask: self.state.reasks(),
                    open,
                }
                .render();
                // A re-ask keeps its round's decider.
                let decider = self
                    .questions
                    .latest()
                    .ok_or("no ask to ask again")?
                    .decider
                    .clone();
                let posted = self.post(&body)?;
                self.show_after(Event::Incomplete)?;
                self.questions.asks.push(Ask {
                    kind: AskKind::Reask,
                    round,
                    at: posted.at,
                    comment: posted.id,
                    questions,
                    decider,
                    verdicts: Vec::new(),
                    result_refusal: None,
                });
            }
            // A REPLY answers each counter-question; the ticket keeps
            // waiting for the decider.
            (Event::CounterQuestion, _) => {
                let result = result.ok_or("a counter-question comes from a result")?;
                let brief = self.last_brief.as_ref().ok_or("no brief")?;
                let replies = answer_check::counter_replies(brief, &result.verdicts);
                if replies.is_empty() {
                    return Err("a counter-question without a reply to post".to_owned());
                }
                let body = ReplyComment {
                    ticket: self.id.clone(),
                    replies,
                }
                .render();
                // The next answer is a decider comment newer than the ones
                // this check read (kept above), never measured against the
                // REPLY, which is Owlshift's own.
                self.post(&body)?;
            }
            _ => {}
        }
        Ok(event)
    }

    /// The answer check runs once answers arrive, as `owlshift continue`
    /// decides it (`answer_check::readiness`): a comment of the latest ask's
    /// decider newer than that ask and than what the last check with
    /// verdicts read. There is no quiet window: a scenario plays each answer
    /// step by hand. A failed or interrupted check keeps nothing, so it is
    /// retried on the same answers. Returns what this check reads through,
    /// the newest last edit among the decider's comments.
    fn require_new_answer(&self) -> Result<Option<Timestamp>, String> {
        let waiting = "no question waits for an answer";
        // A Markdown account's identifier is its name.
        let account = &self.questions.latest().ok_or(waiting)?.decider.account;
        let decider = Person {
            id: account.clone(),
            name: account.clone(),
        };
        let comments = Tracker::comments(&self.tracker, &self.id).map_err(|e| e.to_string())?;
        match answer_check::readiness(
            &comments,
            &decider,
            &self.questions,
            self.now,
            Duration::ZERO,
        ) {
            None => Err(waiting.to_owned()),
            Some(Readiness::Waiting { since }) => Err(format!(
                "no comment from the decider since {since}: the answer check runs once answers arrive"
            )),
            Some(Readiness::Settling { counts_at }) => Err(format!(
                "the decider's reply counts at {counts_at}, with no quiet window"
            )),
            Some(Readiness::Counts) => Ok(answer_check::newest_decider_edit(&comments, &decider)),
        }
    }

    /// Posts a comment of the runner's own at the step's time; returns it as
    /// the tracker recorded it.
    fn post(&self, body: &str) -> Result<markdown::Comment, String> {
        self.tracker
            .post_comment(&self.id, OWLSHIFT_AUTHOR, self.now, body)
            .map_err(|e| e.to_string())
    }

    /// Runs `role` through the executor, on the fake harness, with `reply`;
    /// keeps the brief, and the failure of the gate the run ended with.
    fn execute(&mut self, role: Role, reply: &Reply) -> Result<RunReport, String> {
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
        // What the ticket ref keeps for the next Build run, whatever command
        // runs it, as `owlshift do` and `continue` keep it (OWL-192).
        if role == Role::Build && !matches!(report.outcome, Outcome::Quarantined(_)) {
            self.questions.build_refusal =
                build_refusal_after(self.questions.build_refusal.as_ref(), &report.outcome);
            self.has_ref |= self.questions.build_refusal.is_some();
        }
        if report.exit_code == Some(OWN_FAILURE) {
            self.last_run = Some(report);
            return Err("the fake harness could not do what the reply says".to_owned());
        }
        Ok(report)
    }

    /// The ticket's worktree, beside the main checkout; the executor creates
    /// it at the first run, on the ticket's branch from `origin/main`.
    fn worktree(&self) -> PathBuf {
        self.tmp.path().join("worktrees").join(self.id.as_str())
    }

    /// The brief of a run of `role`. The ticket and its thread read as
    /// `owlshift do` and `continue` read them (`owlshift_runner::on_demand`,
    /// `account_author` and `thread`): the ticket's assignee is the current
    /// decider, and the brief's decider is the latest ask's, the one in
    /// force at the end of the thread.
    fn brief(&self, role: Role) -> Result<Brief, String> {
        let ticket = Tracker::ticket(&self.tracker, &self.id).map_err(|e| e.to_string())?;
        let current = ticket
            .assignee
            .clone()
            .ok_or("the ticket has no assignee to act as its decider")?;
        let comments = Tracker::comments(&self.tracker, &self.id).map_err(|e| e.to_string())?;
        let decider = self
            .questions
            .latest()
            .map_or_else(|| current.name.clone(), |ask| ask.decider.account.clone());
        let mut brief = Brief {
            format: Format,
            role,
            project: self.scenario.clone(),
            ticket: TicketBrief {
                id: self.id.clone(),
                author: account_author(&ticket.author, &current),
                title: ticket.title,
                url: None,
                labels: ticket.labels.clone(),
                description: ticket.description,
            },
            decider,
            thread: thread(
                &comments,
                &self.questions.asks,
                &self.questions.decisions,
                &current,
            ),
            resolve: if role == Role::Resolver {
                resolver::unlabelled(&self.resolving)
            } else {
                Vec::new()
            },
            checkpoint: None,
            zones: brief_zones(ticket.labels.iter().map(String::as_str)).zones,
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
            always_human: if matches!(role, Role::Build | Role::Resolver) {
                self.policy.additions()
            } else {
                Vec::new()
            },
            gate_failure: if role == Role::Build {
                self.gate_failure.clone()
            } else {
                None
            },
            result_refusal: match role {
                Role::AnswerCheck => self
                    .questions
                    .latest()
                    .and_then(|ask| ask.result_refusal.clone()),
                _ => None,
            },
            decisions_refused: false,
            result_path: RelativePath::new(RESULT_PATH)
                .expect("RESULT_PATH is a valid relative path"),
        };
        if role == Role::Build {
            tell_build(&mut brief, &self.questions);
        }
        Ok(brief)
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
        if let Some(expected) = expect.reasks {
            same("reasks", expected, self.state.reasks())?;
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
                for text in &last.lacks {
                    if comment.body.contains(text.as_str()) {
                        return Err(format!(
                            "expected last_comment not to contain {text:?}, found {:?}",
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
                    ThreadEntry::Questions { .. }
                    | ThreadEntry::Reask { .. }
                    | ThreadEntry::Decision { .. } => None,
                })
                .collect();
            same("brief_thread", names(expected), names(&found))?;
        }
        if let Some(expected) = &expect.brief_entries {
            let brief = self
                .last_brief
                .as_ref()
                .ok_or("expected a brief, found none")?;
            let found: Vec<String> = brief
                .thread
                .iter()
                .map(|entry| match entry {
                    ThreadEntry::Comment { author, .. } => name(&author.relation),
                    ThreadEntry::Questions { .. } => "questions".to_owned(),
                    ThreadEntry::Reask { .. } => "reask".to_owned(),
                    ThreadEntry::Decision { .. } => "decision".to_owned(),
                })
                .collect();
            same(
                "brief_entries",
                format!("{expected:?}"),
                format!("{found:?}"),
            )?;
        }
        if let Some(expected) = &expect.brief_latest_ask {
            let brief = self
                .last_brief
                .as_ref()
                .ok_or("expected a brief, found none")?;
            let found: Vec<&str> = brief
                .latest_ask()
                .map(|(_, questions)| questions.iter().map(|q| q.id.as_str()).collect())
                .unwrap_or_default();
            same(
                "brief_latest_ask",
                format!("{expected:?}"),
                format!("{found:?}"),
            )?;
        }
        if let Some(expected) = &expect.brief_decisions {
            let brief = self
                .last_brief
                .as_ref()
                .ok_or("expected a brief, found none")?;
            let found: Vec<&str> = brief
                .thread
                .iter()
                .filter_map(|entry| match entry {
                    ThreadEntry::Decision { decision, .. } => Some(decision.as_str()),
                    _ => None,
                })
                .collect();
            same(
                "brief_decisions",
                format!("{expected:?}"),
                format!("{found:?}"),
            )?;
        }
        if let Some(expected) = &expect.brief_resolve {
            let brief = self
                .last_brief
                .as_ref()
                .ok_or("expected a brief, found none")?;
            let found: Vec<&str> = brief.resolve.iter().map(|q| q.id.as_str()).collect();
            same(
                "brief_resolve",
                format!("{expected:?}"),
                format!("{found:?}"),
            )?;
        }
        if let Some(expected) = &expect.brief_always_human {
            let brief = self
                .last_brief
                .as_ref()
                .ok_or("expected a brief, found none")?;
            same(
                "brief_always_human",
                format!("{expected:?}"),
                format!("{:?}", brief.always_human),
            )?;
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
        if let Some(expected) = &expect.brief_result_refusal {
            let brief = self
                .last_brief
                .as_ref()
                .ok_or("expected a brief, found none")?;
            none_or_containing(
                "brief_result_refusal",
                expected,
                brief.result_refusal.as_deref(),
            )?;
        }
        if let Some(expected) = expect.brief_decisions_refused {
            let brief = self
                .last_brief
                .as_ref()
                .ok_or("expected a brief, found none")?;
            same("brief_decisions_refused", expected, brief.decisions_refused)?;
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

/// The questions a round asks of a run's: all but those `decided`,
/// renumbered Q1..Qn.
fn left(raised: &[Question], decided: &[owlshift_contracts::ids::QuestionId]) -> Vec<Question> {
    resolver::left_for_decider(raised, decided)
        .into_iter()
        .map(|(_, question)| question)
        .collect()
}

/// Checks a gate failure against `none` or a text its command, reason or
/// output contains.
fn gate_failure_is(field: &str, expected: &str, found: Option<&GateFailure>) -> Result<(), String> {
    let text = found.map(|failure| {
        format!(
            "{} {} {}",
            failure.command.as_deref().unwrap_or_default(),
            failure.reason,
            failure.output
        )
    });
    none_or_containing(field, expected, text.as_deref())
}

/// Checks a text against `none` or a text it contains.
fn none_or_containing(field: &str, expected: &str, found: Option<&str>) -> Result<(), String> {
    match (expected, found) {
        ("none", None) => Ok(()),
        ("none", Some(text)) => Err(format!("expected {field} none, found {text:?}")),
        (_, None) => Err(format!(
            "expected {field} containing {expected:?}, found none"
        )),
        (_, Some(text)) if text.contains(expected) => Ok(()),
        (_, Some(text)) => Err(format!(
            "expected {field} containing {expected:?}, found {text:?}"
        )),
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
