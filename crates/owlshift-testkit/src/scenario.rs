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
//! No executor (OWL-15) and no writer (OWL-18) exist yet, so the runner
//! drives the ticket itself, and only as far as the scenarios need: it keeps
//! the core state in memory from Ready, writes the brief, launches the fake
//! harness in the ticket's worktree, maps the run's outcome onto a core
//! event, posts the questions comment, sets the visible stage and pushes the
//! branch. It is a stand-in: those tickets replace it, and the scenario files
//! stay. What it leaves out on purpose:
//!
//! - the brief's thread carries every tracker comment as a plain `comment`
//!   entry, the runner's own QUESTIONS comment included, where the executor
//!   will give each question round its own `questions` entry;
//! - no intake, admission, claim or ticket ref; the pipeline is the project's
//!   default variant;
//! - the answer check is not run: an `answer` step gives its verdict, and
//!   only `answered` until the answer-check role arrives in P2;
//! - a `blocked` or `premise_false` result is refused, and no PARKED,
//!   RE-ASK, RESUME or DELIVERY comment is written.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use jiff::{SignedDuration, Timestamp};
use serde::{Deserialize, Serialize};
use tempfile::TempDir;

use owlshift_adapters::tracker::markdown::MarkdownTracker;
use owlshift_contracts::brief::{
    Author, Brief, PermissionLevel, Permissions, Relation, ThreadEntry, TicketBrief,
};
use owlshift_contracts::comment::{Footer, Header, MarkerKind};
use owlshift_contracts::config::{ProjectConfig, States, TrackerKind};
use owlshift_contracts::format::Format;
use owlshift_contracts::ids::TicketId;
use owlshift_contracts::result::{self, RunResult};
use owlshift_contracts::{Role, Stage};
use owlshift_core::pipeline::Pipeline;
use owlshift_core::state::{Event, Status, TicketState, Transition};

use crate::git::{GitEnv, Remote, seed};
use crate::reply::{OWN_FAILURE, Reply, usage_limit};

/// The author name of the runner's own comments.
pub const OWLSHIFT_AUTHOR: &str = "owlshift";

/// Where a role writes its result, relative to the worktree.
const RESULT_PATH: &str = ".owlshift/result.json";

/// A scenario file.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scenario {
    pub description: String,
    pub ticket: TicketId,
    /// The virtual time the scenario starts at; step `n` is `n` minutes later.
    pub start: Timestamp,
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
    id: TicketId,
    branch: String,
    state: TicketState,
    now: Timestamp,
    runs: u32,
    worktree: Option<PathBuf>,
    last_brief: Option<Brief>,
    last_output: Option<Output>,
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
        Ok(Self {
            scenario: name.to_owned(),
            fixture: fixture.to_owned(),
            fake_harness: fake_harness.to_owned(),
            tracker: MarkdownTracker::new(&remote.checkout),
            states: config.tracker.states,
            pipeline: Pipeline::new(config.pipeline.default),
            branch: format!("owlshift/{}", scenario.ticket),
            id: scenario.ticket.clone(),
            state,
            now: scenario.start,
            runs: 0,
            worktree: None,
            last_brief: None,
            last_output: None,
            tmp,
            git,
            remote,
        })
    }

    /// Does one action; returns the core event it produced, if any.
    fn step(&mut self, action: &Action<'_>) -> Result<Option<Event>, String> {
        self.last_output = None;
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

    /// Runs the current stage's role on the fake harness.
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
        let worktree = self.worktree()?;

        let brief = self.brief(role)?;
        let brief_path = dir.join("brief.json");
        write(&brief_path, &brief.render())?;
        let mut reply = reply.clone();
        reply.result = reply.result.map(|path| self.fixture.join(path));
        let reply_path = dir.join("reply.toml");
        write(&reply_path, &reply.render())?;
        let result_file = worktree.join(RESULT_PATH);
        match fs::remove_file(&result_file) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => {
                return Err(format!("{}: {e}", result_file.display()));
            }
            _ => {}
        }

        let mut command = Command::new(&self.fake_harness);
        command
            .arg("--brief")
            .arg(&brief_path)
            .arg("--reply")
            .arg(&reply_path)
            .current_dir(&worktree)
            .stdin(Stdio::null());
        self.git.at(self.now).apply(&mut command);
        let output = command
            .output()
            .map_err(|e| format!("{}: {e}", self.fake_harness.display()))?;
        self.last_brief = Some(brief);
        self.last_output = Some(output.clone());
        if output.status.code() == Some(OWN_FAILURE) {
            return Err("the fake harness could not do what the reply says".to_owned());
        }

        let (event, result) = outcome(&output, &result_file)?;
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

    /// The ticket's worktree, on its own branch from `origin/main`, created
    /// at its first run.
    fn worktree(&mut self) -> Result<PathBuf, String> {
        if let Some(worktree) = &self.worktree {
            return Ok(worktree.clone());
        }
        let relative = format!("../worktrees/{}", self.id);
        self.git
            .run(
                &self.remote.checkout,
                &[
                    "worktree",
                    "add",
                    "--quiet",
                    "-b",
                    &self.branch,
                    &relative,
                    "origin/main",
                ],
            )
            .map_err(|e| e.to_string())?;
        let worktree = self.tmp.path().join("worktrees").join(self.id.as_str());
        self.worktree = Some(worktree.clone());
        Ok(worktree)
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
            result_path: RESULT_PATH.to_owned(),
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
        Ok(())
    }

    /// The last run's exit status and output, for an error message.
    fn run_output(&self) -> Option<String> {
        self.last_output.as_ref().map(|output| {
            format!(
                "fake harness exit status: {}\n--- stdout ---\n{}\n--- stderr ---\n{}",
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
        })
    }
}

/// The core event a run's outcome maps onto, and its result when it left a
/// valid one. A usage limit is an interruption; a non-zero exit, a missing or
/// invalid `result.json`, or a `failed` status is a failed run: an exit
/// status alone is never proof of success.
fn outcome(output: &Output, result_file: &Path) -> Result<(Event, Option<RunResult>), String> {
    if usage_limit(&String::from_utf8_lossy(&output.stderr)).is_some() {
        return Ok((Event::Interrupted, None));
    }
    if !output.status.success() {
        return Ok((Event::RunFailed, None));
    }
    let Some(result) = fs::read_to_string(result_file)
        .ok()
        .and_then(|text| RunResult::parse(&text).ok())
    else {
        return Ok((Event::RunFailed, None));
    };
    let event = match result.status {
        result::Status::Done => Event::Completed,
        result::Status::Questions => Event::Questions,
        result::Status::Failed => Event::RunFailed,
        status @ (result::Status::Blocked | result::Status::PremiseFalse) => {
            return Err(format!(
                "a `{}` result is not supported by the stand-in driver",
                name(&status)
            ));
        }
    };
    Ok((event, Some(result)))
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
